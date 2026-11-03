"""设计验证用的离散状态模型；不是 TaskLoop 运行时或真实 Harness。

运行：python verify_contract.py
依赖：requirements.txt。测试只证明本文件列出的契约模型性质。
"""
from __future__ import annotations

import copy
import concurrent.futures
import hashlib
import importlib.metadata
import json
import sqlite3
import sys
import tempfile
import unittest
import uuid
import threading
from collections import deque
from pathlib import Path

from jsonschema import Draft202012Validator

ROOT = Path(__file__).resolve().parent


class ContractError(Exception):
    pass


def require(condition, code):
    if not condition:
        raise ContractError(code)


def canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()


def digest(value):
    return hashlib.sha256(value).hexdigest()


def parse_json(raw):
    def unique(pairs):
        out = {}
        for key, value in pairs:
            require(key not in out, "duplicate_json_key")
            out[key] = value
        return out
    return json.loads(raw, object_pairs_hook=unique)


def schema(name):
    require(Path(name).name == name, "schema_path_escape")
    data = parse_json((ROOT / name).read_text())
    # 设计版本仅使用完整本地 Schema，禁止验证时向网络解析引用。
    require('"$ref"' not in json.dumps(data), "schema_ref_unsupported_in_profile")
    Draft202012Validator.check_schema(data)
    return data


def validate(name, value):
    errors = list(Draft202012Validator(schema(name)).iter_errors(value))
    require(not errors, "schema_invalid:" + name)


REGISTRY = parse_json((ROOT / "registry.example.json").read_text())


def compile_bundle(bundle):
    validate("bundle.schema.json", bundle)
    flow, entries = bundle["flow"], bundle["entries"]
    states = flow["states"]
    require(flow["initial"] in states, "initial_missing")
    edges = {}
    schema_names = {"bundle.schema.json", "submission.schema.json", "check-result.schema.json", "tool-receipt.schema.json", flow["input_schema"]}
    for eid, entry in entries.items():
        for ref in entry["skills"]:
            require(ref in REGISTRY["skills"], "skill_unregistered")
            binding = REGISTRY["skills"][ref]
            root = (ROOT / binding["root"]).resolve()
            require(root.is_relative_to(ROOT) and (root / "SKILL.md").is_file(), "skill_root_invalid")
            require(not any(p.is_symlink() for p in root.rglob("*")), "skill_symlink")
            raw = b"".join(p.relative_to(root).as_posix().encode() + b"\0" + p.read_bytes() + b"\0" for p in sorted(root.rglob("*")) if p.is_file())
            require(digest(raw) == binding["tree_digest"], "skill_digest_mismatch")
        require(entry["executor"] in REGISTRY["executors"], "executor_unregistered")
        require(entry["gate"] in REGISTRY["gates"], "gate_unregistered")
        require(set(entry["tools"]) <= set(REGISTRY["tools"]), "tool_unregistered")
        binding = REGISTRY["executors"][entry["executor"]]
        require(set(["bounded_entry", "collect_artifacts", "cancel", "isolated_workspace", "isolated_context"]) <= set(binding.get("capabilities", [])), "adapter_incapable")
        schema_names.add(entry["input_schema"])
        schema_names.update(o["schema"] for o in entry["outputs"].values())
        for dep in entry.get("requires", []):
            producer = states.get(dep["state"])
            require(producer is not None and producer["kind"] == "work", "input_producer_missing")
            require(producer["entry"] in entries, "entry_missing")
            require(dep["slot"] in entries[producer["entry"]]["outputs"], "input_slot_missing")
    for sid, state in states.items():
        if state["kind"] == "work":
            require(state["entry"] in entries, "entry_missing")
            require(set(state["accepted"]) == set(entries[state["entry"]]["outcomes"]), "outcome_coverage")
            edges[sid] = set(state["accepted"].values()) | {state["rejected"], state["failed"]}
        elif state["kind"] == "wait":
            require(all(not name.startswith(("work.", "runtime.")) for name in state["signals"]), "reserved_event_name")
            schema_names.update(s["schema"] for s in state["signals"].values())
            edges[sid] = {s["target"] for s in state["signals"].values()} | {state["timeout_target"]}
        else:
            edges[sid] = set()
            for dep in state["requires"]:
                producer = states.get(dep["state"])
                require(producer is not None and producer["kind"] == "work", "terminal_producer_missing")
                require(producer["entry"] in entries and dep["slot"] in entries[producer["entry"]]["outputs"], "terminal_output_missing")
        require(edges[sid] <= set(states), "target_missing")
    seen, todo = set(), [flow["initial"]]
    while todo:
        n = todo.pop()
        if n not in seen:
            seen.add(n)
            todo.extend(edges[n])
    require(seen == set(states), "unreachable_state")
    finals = {n for n, s in states.items() if s["kind"] == "terminal"}
    can_finish = set(finals)
    while True:
        expanded = can_finish | {n for n, targets in edges.items() if targets & can_finish}
        if expanded == can_finish:
            break
        can_finish = expanded
    require(can_finish == set(states), "no_terminal_path")
    locked_schemas = {n: digest(canonical(schema(n))) for n in sorted(schema_names)}
    return digest(canonical({"bundle": bundle, "schemas": locked_schemas, "registry": REGISTRY}))


class Model:
    """单写者、串行模型。origin/actor 是测试的可信通道标签，不是实际鉴权实现。"""

    def __init__(self, bundle_name="research.bundle.json", routes=None, inputs=None, grants=None):
        self.bundle = parse_json((ROOT / bundle_name).read_text())
        self.lock = compile_bundle(self.bundle)
        self.execution_id = "execution-" + uuid.uuid4().hex
        self.inputs = copy.deepcopy(inputs) if inputs is not None else parse_json((ROOT / "research-input.example.json").read_text())
        validate(self.bundle["flow"]["input_schema"], self.inputs)
        effective = set(REGISTRY["tools"]) if grants is None else set(grants)
        require(all(set(e["tools"]) <= effective for e in self.bundle["entries"].values()), "required_capability_denied")
        self.state = self.bundle["flow"]["initial"]
        self.status, self.revision, self.epoch, self.run_count = "created", 0, 0, 0
        self.transitions = 0
        self.visits, self.events, self.history, self.artifacts = {}, [], [], {}
        self.receipts, self.submissions, self.signals, self.signal_facts, self.check_evidence = {}, {}, {}, [], {}
        self.owned_runs, self.output_roots = set(), {}
        self.routes = copy.deepcopy(routes or {})  # 注入的领域检查结果，来自受信 fixture，不来自 Agent 字段。
        self.active, self.pending = None, None
        self._enter(self.state)

    def definition(self):
        return self.bundle["flow"]["states"][self.state]

    def _enter(self, target):
        limits = self.bundle["flow"]["limits"]
        if self.visits.get(target, 0) >= limits["max_visits_per_state"]:
            self.status, self.active = "failed", None
            self.events.append({"type": "runtime.limit_reached", "state": self.state})
            self.revision += 1
            return
        self.state = target
        self.visits[target] = self.visits.get(target, 0) + 1
        s = self.definition()
        self.revision += 1
        if s["kind"] == "terminal":
            self.status, self.active = s["status"], None
        elif s["kind"] == "wait":
            self.status, self.active = "waiting", None
        else:
            self.status = "running"
            self._new_run()

    def _new_run(self):
        entry = self.bundle["entries"][self.definition()["entry"]]
        input_ids = []
        for dep in entry.get("requires", []):
            found = [h["artifacts"][dep["slot"]]["id"] for h in self.history if h["state"] == dep["state"] and dep["slot"] in h["artifacts"]]
            if not found:
                self.status, self.active = "blocked", None
                self.events.append({"type": "runtime.input_missing", "dependency": dep})
                return
            input_ids.extend(found if dep["selection"] == "all" else found[-1:])
        try:
            validate(entry["input_schema"], {"flow_input": self.inputs, "input_artifact_ids": input_ids, "signal_fact_ids": [f["id"] for f in self.signal_facts]})
        except ContractError:
            self.status, self.active = "blocked", None
            self.events.append({"type": "runtime.input_invalid", "state": self.state})
            return
        if self.run_count >= self.bundle["flow"]["limits"]["max_runs"]:
            self.status, self.active = "failed", None
            self.events.append({"type": "runtime.limit_reached", "state": self.state})
            return
        self.run_count += 1
        self.epoch += 1
        self.active = {"protocol_version": 1, "execution_id": self.execution_id, "task_id": f"{self.execution_id}:task-{self.state}-{self.visits[self.state]}", "run_id": f"{self.execution_id}:run-{self.run_count}", "state_id": self.state, "visit": self.visits[self.state], "contract_digest": self.lock, "run_epoch": self.epoch}
        self.owned_runs.add(self.active["run_id"])

    def _move(self, target, reason):
        limits = self.bundle["flow"]["limits"]
        target_is_work = self.bundle["flow"]["states"][target]["kind"] == "work"
        if self.transitions >= limits["max_transitions"] or self.visits.get(target, 0) >= limits["max_visits_per_state"] or (target_is_work and self.run_count >= limits["max_runs"]):
            self.status, self.active = "failed", None
            self.revision += 1
            self.events.append({"type": "runtime.limit_reached", "state": self.state, "requested_target": target})
            return
        terminal = self.bundle["flow"]["states"][target]
        if terminal["kind"] == "terminal" and any(not any(h["state"] == dep["state"] and dep["slot"] in h["artifacts"] for h in self.history) for dep in terminal["requires"]):
            self.status, self.active = "blocked", None
            self.revision += 1
            self.events.append({"type": "runtime.terminal_output_missing", "state": self.state, "requested_target": target})
            return
        self.transitions += 1
        self.events.append({"type": reason, "from": self.state, "to": target})
        self._enter(target)

    def last(self, slot):
        return next(a for item in reversed(self.history) for name, a in item["artifacts"].items() if name == slot)

    def accepted_text(self):
        out = {}
        for h in self.history:
            if "materials" not in h["artifacts"]:
                continue
            for item in h["artifacts"]["materials"]["json"]["items"]:
                if item["fetch_status"] == "captured":
                    artifact = h["artifacts"][item["snapshot_slot"]]
                    out[artifact["id"]] = artifact
        return out

    def record_tool_receipt(self, rid, fact, origin="trusted_tool"):
        require(origin == "trusted_tool", "untrusted_receipt")
        require(rid not in self.receipts, "receipt_immutable")
        validate("receipt.schema.json", fact)
        require(fact["run_id"] in self.owned_runs, "receipt_wrong_execution")
        self.receipts[rid] = copy.deepcopy(fact)

    def bind_output_root(self, run_id, directory, origin="host"):
        require(origin == "host" and run_id in self.owned_runs, "output_binding_forbidden")
        path = str(directory.resolve())
        require(run_id not in self.output_roots or self.output_roots[run_id] == path, "output_root_rebind")
        self.output_roots[run_id] = path

    def read_artifact(self, artifact_id):
        require(artifact_id in self.artifacts, "artifact_not_in_execution")
        return self.artifacts[artifact_id]["bytes"]

    def _collect(self, submission, directory, entry):
        root = directory.resolve()
        collected = {}
        count, total = 0, 0
        for item in submission["artifacts"]:
            slot = item["slot"]
            require(slot not in collected, "duplicate_slot")
            rel = Path(item["path"])
            require(not rel.is_absolute() and ".." not in rel.parts, "path_escape")
            candidate = root / rel
            require(not any((root / Path(*rel.parts[:i])).is_symlink() for i in range(1, len(rel.parts) + 1)), "symlink_forbidden")
            require(candidate.resolve().is_relative_to(root), "path_escape")
            require(candidate.is_file(), "artifact_missing")
            raw = candidate.read_bytes()  # 只读一次，下面的校验和存档使用同一份字节。
            if slot in entry["outputs"]:
                try:
                    value = parse_json(raw)
                except (UnicodeDecodeError, ValueError) as exc:
                    raise ContractError("artifact_not_json") from exc
                validate(entry["outputs"][slot]["schema"], value)
            else:
                cfg = entry.get("attachments")
                require(cfg and slot.startswith(cfg["slot_prefix"]) and item["path"].startswith(cfg["path_prefix"]), "undeclared_slot")
                count, total = count + 1, total + len(raw)
                require(count <= cfg["max_files"] and total <= cfg["max_bytes"], "attachment_limit")
                value = None
            collected[slot] = {"bytes": raw, "json": value, "sha256": digest(raw), "slot": slot}
        require(set(entry["outputs"]) <= set(collected), "required_output_missing")
        return collected

    def _gate(self, entry, data):
        ref = f"{self.execution_id}:check-call-{len(self.check_evidence) + 1}"
        self.check_evidence[ref] = {"run_id": self.active["run_id"], "contract_digest": self.lock, "checker": entry["gate"], "candidate_sha256": {k: v["sha256"] for k, v in data.items()}}
        view = GateView(self)
        view.check_ref = ref
        try:
            return HOST_GATES[entry["gate"]](view, copy.deepcopy(entry), copy.deepcopy(data))
        except ContractError:
            raise  # 示例检查器的可判定领域拒绝；基础设施异常走 error。
        except Exception:
            self.check_evidence[ref]["error_code"] = "check_unavailable"
            return {"verdict": "error", "outcome": None, "evidence_refs": [ref],
                    "violations": [{"code": "check_unavailable", "subject": entry["gate"],
                                    "message": "checker execution failed"}]}

    def _research_fixture_gate(self, entry, data):
        # 来源和引用做真实交叉校验，其余领域推理为明确标注的 fixture gate。
        slot = next(iter(entry["outputs"]))
        value = data[slot]["json"]
        if slot == "plan":
            require(value["question"] == self.inputs["question"], "question_mismatch")
            outcome = "ready"
        elif slot == "materials":
            expected = {i["source_id"]: i for i in self.last("candidates")["json"]["items"]}
            items = value["items"]
            require(len(items) == len({i["source_id"] for i in items}), "duplicate_source")
            require(set(expected) == {i["source_id"] for i in items}, "source_coverage")
            usable = 0
            for item in items:
                require(item["requested_url"] == expected[item["source_id"]]["url"], "url_mismatch")
                receipt = self.receipts.get(item["receipt_id"])
                require(receipt is not None, "receipt_missing")
                require(receipt["run_id"] == self.active["run_id"] and receipt["source_id"] == item["source_id"], "receipt_scope")
                for field in ["requested_url", "final_url", "http_status", "reason"]:
                    require(receipt[field] == item[field], "receipt_fact_mismatch")
                require(receipt["fetch_status"] == item["fetch_status"], "receipt_fact_mismatch")
                if item["fetch_status"] == "captured":
                    require(item["raw_slot"] in data, "raw_snapshot_missing")
                    require(item["snapshot_slot"] in data, "snapshot_missing")
                    blob = data[item["snapshot_slot"]]
                    require(data[item["raw_slot"]]["sha256"] == receipt["body_sha256"] and blob["sha256"] == receipt["normalized_sha256"], "provenance_digest_mismatch")
                    require(receipt["usable"], "body_not_usable")
                    require(bool(receipt["transform_version"]), "transform_version_missing")
                    try:
                        blob["bytes"].decode("utf-8")
                    except UnicodeDecodeError as exc:
                        raise ContractError("normalized_text_invalid") from exc
                    usable += 1
                else:
                    require(item["snapshot_slot"] is None and item["raw_slot"] is None, "false_capture")
            outcome = "ready" if usable else "need_input"
        elif slot == "report":
            if value["answer_status"] == "answered":
                require(bool(value["citations"]), "citation_missing")
                for citation in value["citations"]:
                    artifact = self.accepted_text().get(citation["artifact_id"])
                    require(artifact is not None, "citation_not_accepted")
                    require(artifact["sha256"] == citation["sha256"], "citation_version_mismatch")
                    paragraphs = artifact["bytes"].decode().split("\n\n")
                    i = citation["paragraph"]
                    require(i < len(paragraphs) and citation["quote"] in paragraphs[i], "citation_location_mismatch")
                outcome = "ready"
            else:
                outcome = "need_input"
        else:
            outcome = self.routes.get((self.state, self.visits[self.state]))
            if outcome is None:
                outcome = "need_input" if slot == "candidates" and not value["items"] else "ready"
        evidence = [self.check_ref]
        if slot == "materials":
            evidence.extend(i["receipt_id"] for i in value["items"])
        elif slot == "report":
            evidence.extend(i["artifact_id"] for i in value["citations"])
        return {"verdict": "pass", "outcome": outcome, "evidence_refs": evidence, "violations": []}

    def submit(self, submission, directory, origin="adapter", force_check_error=False):
        require(origin == "adapter", "untrusted_event_source")
        validate("submission.schema.json", submission)
        require(submission["execution_id"] == self.execution_id, "wrong_execution")
        key, fingerprint = submission["run_id"], digest(canonical(submission))
        if key in self.submissions:
            prior = self.submissions[key]
            require(prior["fingerprint"] == fingerprint, "submission_conflict")
            return copy.deepcopy(prior)
        require(self.status == "running" and self.active, "not_running")
        require(compile_bundle(self.bundle) == self.lock, "definition_changed")
        require(all(submission[k] == v for k, v in self.active.items()), "stale_or_wrong_attempt")
        require(self.output_roots.get(submission["run_id"]) == str(directory.resolve()), "output_root_mismatch")
        entry = self.bundle["entries"][self.definition()["entry"]]
        try:
            data = self._collect(submission, directory, entry)
            if force_check_error:
                self.pending = (copy.deepcopy(submission), copy.deepcopy(data))
                self.status = "blocked"
                self.revision += 1
                return {"accepted": False, "code": "check_unavailable"}
            return self._accept_checked(submission, data, entry)
        except ContractError as exc:
            rejected = {"accepted": False, "code": str(exc), "fingerprint": fingerprint}
            self.submissions[key] = rejected
            self._move(self.definition()["rejected"], "work.rejected")
            return rejected
        except OSError:
            self.status = "blocked"
            self.revision += 1
            return {"accepted": False, "code": "storage_error"}

    def _accept_checked(self, submission, data, entry):
        checked = copy.deepcopy(self._gate(entry, data))
        try:
            validate("check-result.schema.json", checked)
            require(checked["verdict"] != "pass" or checked["outcome"] in entry["outcomes"], "gate_result_invalid")
            require(all(ref in self.check_evidence or ref in self.receipts or ref in self.artifacts for ref in checked["evidence_refs"]), "gate_evidence_unresolvable")
        except ContractError:
            self.pending = (copy.deepcopy(submission), copy.deepcopy(data))
            self.status = "blocked"
            self.revision += 1
            return {"accepted": False, "code": "gate_protocol_invalid"}
        if checked["verdict"] == "error":
            self.pending = (copy.deepcopy(submission), copy.deepcopy(data))
            self.status = "blocked"
            self.revision += 1
            return {"accepted": False, "code": "check_unavailable"}
        require(checked["verdict"] == "pass", "domain_check_failed")
        stored = {}
        for slot, item in data.items():
            aid = f"{self.execution_id}:artifact-{len(self.artifacts) + 1}"
            saved = dict(item, id=aid, producer_run_id=submission["run_id"])
            self.artifacts[aid] = saved
            stored[slot] = saved
        self.history.append({"state": self.state, "visit": self.visits[self.state], "run_id": submission["run_id"], "artifacts": stored, "check": checked})
        receipt = {"accepted": True, "fingerprint": digest(canonical(submission)), "run_id": submission["run_id"], "outcome": checked["outcome"]}
        self.submissions[submission["run_id"]] = receipt
        self.pending = None
        self._move(self.definition()["accepted"][checked["outcome"]], "work.accepted")
        return receipt

    def recheck(self, actor="operator", expected_revision=None):
        # 后续扩展约束见证；不属于一期公开 API。
        require(actor == "operator", "signal_forbidden")
        require(expected_revision == self.revision, "revision_conflict")
        require(self.status == "blocked" and self.pending, "not_check_blocked")
        submission, data = self.pending
        entry = self.bundle["entries"][self.definition()["entry"]]
        try:
            return self._accept_checked(submission, data, entry)
        except ContractError as exc:
            self.pending = None
            rejected = {"accepted": False, "code": str(exc), "fingerprint": digest(canonical(submission))}
            self.submissions[submission["run_id"]] = rejected
            self._move(self.definition()["rejected"], "work.rejected")
            return rejected

    def signal(self, name, payload, request_key, expected_revision, actor="user"):
        require(actor in ["user", "operator"], "signal_forbidden")
        fingerprint = digest(canonical([name, payload, actor]))
        if request_key in self.signals:
            require(self.signals[request_key]["fingerprint"] == fingerprint, "signal_conflict")
            return self.signals[request_key]
        require(expected_revision == self.revision, "revision_conflict")
        require(self.status == "waiting", "not_waiting")
        action = self.definition()["signals"].get(name)
        require(action is not None, "signal_not_allowed")
        validate(action["schema"], payload)
        fact = {"id": f"{self.execution_id}:signal-{len(self.signal_facts) + 1}", "name": name, "payload": copy.deepcopy(payload), "actor": actor}
        self.signal_facts.append(fact)
        self.signals[request_key] = {"fingerprint": fingerprint, "fact_id": fact["id"]}
        self._move(action["target"], "signal.accepted")
        return self.signals[request_key]

    def wait_timeout(self, elapsed_seconds):
        require(self.status == "waiting", "not_waiting")
        require(elapsed_seconds >= self.definition()["timeout_seconds"], "timeout_not_due")
        self._move(self.definition()["timeout_target"], "wait.timeout")

    def cancel(self):
        if self.status in ["completed", "failed", "cancelled"]:
            return self.status
        self.status, self.active, self.pending = "cancelling", None, None
        self.epoch += 1
        self.revision += 1
        return self.status

    def confirm_stopped(self, trusted=True):
        require(trusted and self.status == "cancelling", "stop_not_confirmed")
        self.status = "cancelled"
        self.revision += 1

    def crash_snapshot(self):
        return copy.deepcopy(self.__dict__)

    @classmethod
    def restore(cls, state, active_was_started=True):
        instance = cls.__new__(cls)
        instance.__dict__ = copy.deepcopy(state)
        require(compile_bundle(instance.bundle) == instance.lock, "definition_changed")
        if instance.status == "running" and active_was_started:
            instance.status = "blocked"
            instance.active = None
            instance.epoch += 1
            instance.revision += 1
        return instance


class GateView:
    """领域检查器只有隔离的只读事实视图，没有状态控制句柄；不是 Python 安全沙箱。"""

    def __init__(self, model):
        for name in ["state", "visits", "active", "inputs", "routes", "history", "artifacts", "receipts"]:
            setattr(self, name, copy.deepcopy(getattr(model, name)))

    def last(self, slot):
        return Model.last(self, slot)

    def accepted_text(self):
        return Model.accepted_text(self)


def review_fixture_gate(model, entry, data):
    return {"verdict": "pass", "outcome": model.routes.get((model.state, model.visits[model.state]), "accepted"), "evidence_refs": [model.check_ref], "violations": []}


# 领域实现由宿主注册；通用 _move / _enter / signal 不包含任何业务状态名。
HOST_GATES = {"inquiry." + n + ".gate": lambda view, entry, data: Model._research_fixture_gate(view, entry, data) for n in ["scope", "discover", "capture", "catalogue", "analyze", "report"]}
HOST_GATES["review.inspect.gate"] = review_fixture_gate


def fixture_submission(model, directory, modify=None):
    """受信工具和模型候选分开构造；example.org 不是联网探测结果。"""
    state = model.state
    active = copy.deepcopy(model.active)
    model.bind_output_root(active["run_id"], directory)
    entry = model.bundle["entries"][model.definition()["entry"]]
    slot = next(iter(entry["outputs"]))
    if slot == "plan":
        payload = {"question": model.inputs["question"], "subquestions": ["能力", "限制"], "search_queries": ["X Y 官方资料"], "stop_reason": "query_budget"}
    elif slot == "candidates":
        payload = {"items": [{"source_id": "s1", "url": "https://example.org/doc", "discovery_receipt_id": "search-receipt-fixture", "subquestion": "限制"}], "stop_reason": "candidate_budget"}
    elif slot == "materials":
        body = "Y 能力存在。\n\nY 仅适用于条件 Z。".encode()
        (directory / "captures").mkdir()
        (directory / "captures/s1.txt").write_bytes(body)
        rid = f"fetch-{active['run_id']}"
        model.record_tool_receipt(rid, {"run_id": active["run_id"], "source_id": "s1", "requested_url": "https://example.org/doc", "final_url": "https://example.org/doc", "captured_at": "2026-11-01T00:00:00Z", "http_status": 200, "fetch_status": "captured", "reason": "retrieved", "body_sha256": digest(body), "normalized_sha256": digest(body), "transform_version": "identity@1", "usable": True})
        payload = {"items": [{"source_id": "s1", "requested_url": "https://example.org/doc", "final_url": "https://example.org/doc", "fetch_status": "captured", "http_status": 200, "reason": "retrieved", "receipt_id": rid, "snapshot_slot": "capture.s1", "raw_slot": "capture.s1"}], "stop_reason": "input_covered"}
    elif slot == "catalogue":
        payload = {"groups": [{"topic": "限制", "source_ids": ["s1"]}], "gaps": [], "decision": "ready"}
    elif slot == "analysis":
        payload = {"judgments": [{"text": "Y 有条件限制", "support": ["s1"], "counter": [], "limitations": ["仅验证样例"]}], "gaps": [], "decision": "ready"}
    elif slot == "review":
        payload = {"issues": ["fixture issue"], "needs_revision": model.routes.get((model.state, model.visits[model.state]), "accepted") == "revise"}
    else:
        artifacts = model.accepted_text()
        citations = []
        if artifacts:
            aid, a = next(iter(artifacts.items()))
            citations = [{"artifact_id": aid, "sha256": a["sha256"], "paragraph": 1, "quote": "Y 仅适用于条件 Z。"}]
        payload = {"answer_status": "answered" if citations else "insufficient_evidence", "conclusion": "Y 的适用存在条件", "citations": citations, "limitations": ["fixture，仅证明契约模型"]}
    if modify:
        modify(payload)
    (directory / f"{slot}.json").write_text(json.dumps(payload, ensure_ascii=False))
    submission = dict(active, artifacts=[{"slot": slot, "path": f"{slot}.json"}])
    if slot == "materials":
        submission["artifacts"].append({"slot": "capture.s1", "path": "captures/s1.txt"})
    return submission


def advance(model, modify=None, **kwargs):
    with tempfile.TemporaryDirectory() as path:
        directory = Path(path)
        submission = fixture_submission(model, directory, modify)
        return model.submit(submission, directory, **kwargs)


def through(model, target):
    for _ in range(20):
        if model.state == target or model.status != "running":
            return
        require(advance(model)["accepted"], "fixture_failed")
    raise ContractError("fixture_loop")


class InstancePool:
    """单 Worker 的有界、轮转设计模型；不是实际进程调度器。"""

    def __init__(self, capacity=8):
        self.capacity, self.instances, self.starts, self.queue = capacity, {}, {}, deque()

    def start(self, request_key, inputs=None, routes=None, principal="local-user"):
        key = ("workspace-fixture", principal, "start", request_key)
        data = inputs if inputs is not None else parse_json((ROOT / "research-input.example.json").read_text())
        route_descriptor = sorted([list(k) + [v] for k, v in (routes or {}).items()])
        fingerprint = digest(canonical([data, route_descriptor]))
        if key in self.starts:
            prior = self.starts[key]
            require(prior["fingerprint"] == fingerprint, "request_conflict")
            return prior["execution_id"]
        active = sum(m.status not in ["completed", "failed", "cancelled"] for m in self.instances.values())
        require(active < self.capacity, "admission_full")
        model = Model(inputs=data, routes=routes)
        self.instances[model.execution_id] = model
        self.starts[key] = {"fingerprint": fingerprint, "execution_id": model.execution_id}
        self.queue.append(model.execution_id)
        return model.execution_id

    def step(self):
        while self.queue:
            eid = self.queue.popleft()
            model = self.instances[eid]
            if model.status != "running":
                continue
            advance(model)
            if model.status == "running":
                self.queue.append(eid)
            return eid
        return None

    def signal(self, eid, name, payload, request_key, revision):
        result = self.instances[eid].signal(name, payload, request_key, revision)
        if self.instances[eid].status == "running" and eid not in self.queue:
            self.queue.append(eid)
        return result

    def cancel(self, eid):
        model = self.instances[eid]
        model.cancel()
        if model.status == "cancelling":
            model.confirm_stopped()  # 仅模拟已收到真实停止确认，不证明真实 OS 停止。
        self.queue = deque(i for i in self.queue if i != eid)

    def snapshot(self):
        return {"capacity": self.capacity, "instances": {eid: m.crash_snapshot() for eid, m in self.instances.items()}, "starts": copy.deepcopy(self.starts), "queue": list(self.queue)}

    @classmethod
    def restore(cls, snapshot):
        pool = cls(snapshot["capacity"])
        pool.starts = copy.deepcopy(snapshot["starts"])
        pending = set(snapshot["queue"])
        pool.instances = {eid: Model.restore(s, active_was_started=eid not in pending) for eid, s in snapshot["instances"].items()}
        pool.queue = deque(eid for eid in snapshot["queue"] if pool.instances[eid].status == "running")
        return pool


class ContractTests(unittest.TestCase):
    def test_relabelled_other_question_content_fails_domain_gate(self):
        a=Model(inputs={"question":"question A","seed_urls":[],"scope":"fixture"})
        b=Model(inputs={"question":"question B","seed_urls":[],"scope":"fixture"})
        with tempfile.TemporaryDirectory() as pa,tempfile.TemporaryDirectory() as pb:
            da,db=Path(pa),Path(pb);fixture_submission(a,da);submission=fixture_submission(b,db)
            (db/"plan.json").write_bytes((da/"plan.json").read_bytes())
            self.assertEqual(b.submit(submission,db)["code"],"question_mismatch")
        self.assertEqual((a.state,b.state),("scope","failed"))

    def test_shared_request_objects_are_frozen_per_instance(self):
        inputs={"question":"original","seed_urls":[],"scope":"fixture"};routes={("catalogue",1):"need_more"}
        a,b=Model(inputs=inputs,routes=routes),Model(inputs=inputs,routes=routes)
        inputs["question"]="changed";routes[("catalogue",1)]="need_input"
        self.assertEqual((a.inputs["question"],b.inputs["question"]),("original","original"))
        a.routes[("catalogue",1)]="ready"
        self.assertEqual(b.routes[("catalogue",1)],"need_more")

    def test_adapter_without_instance_isolation_is_rejected(self):
        saved = copy.deepcopy(REGISTRY["executors"]["research.agent"])
        REGISTRY["executors"]["research.agent"]["capabilities"].remove("isolated_context")
        try:
            with self.assertRaisesRegex(ContractError, "adapter_incapable"): compile_bundle(parse_json((ROOT / "research.bundle.json").read_text()))
        finally:
            REGISTRY["executors"]["research.agent"] = saved

    def test_same_skill_instances_have_distinct_allocation_ids(self):
        a, b = Model(), Model()
        self.assertEqual(a.lock, b.lock)
        for field in ["execution_id", "task_id", "run_id"]:
            self.assertNotEqual(a.active[field], b.active[field])

    def test_cross_instance_submission_is_rejected_without_mutation(self):
        a, b = Model(), Model(); before = b.crash_snapshot()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); submission = fixture_submission(a, d)
            with self.assertRaisesRegex(ContractError, "wrong_execution"): b.submit(submission, d)
        self.assertEqual(b.__dict__, before)

    def test_relabelled_cross_instance_run_is_rejected(self):
        a, b = Model(), Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); submission = fixture_submission(a, d); submission["execution_id"] = b.execution_id
            with self.assertRaisesRegex(ContractError, "stale_or_wrong_attempt"): b.submit(submission, d)

    def test_cross_instance_output_directory_is_rejected(self):
        a, b = Model(), Model()
        with tempfile.TemporaryDirectory() as pa, tempfile.TemporaryDirectory() as pb:
            da, db = Path(pa), Path(pb); foreign = fixture_submission(a, da); fixture_submission(b, db)
            forged = dict(b.active, artifacts=foreign["artifacts"])
            with self.assertRaisesRegex(ContractError, "output_root_mismatch"): b.submit(forged, da)

    def test_cross_instance_tool_receipt_is_rejected(self):
        a, b = Model(), Model(); through(a, "capture"); through(b, "capture")
        with tempfile.TemporaryDirectory() as p:
            fixture_submission(a, Path(p)); rid, fact = next(iter(a.receipts.items()))
            with self.assertRaisesRegex(ContractError, "receipt_wrong_execution"): b.record_tool_receipt(rid, fact)
        self.assertFalse(b.receipts)

    def test_parallel_same_skill_models_keep_artifact_ownership(self):
        def run(question):
            m = Model(inputs={"question": question, "seed_urls": [], "scope": "fixture"})
            through(m, "done")
            return m
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            a, b = list(pool.map(run, ["question A", "question B"]))
        self.assertEqual((a.status, b.status), ("completed", "completed"))
        self.assertEqual(a.last("plan")["json"]["question"], "question A")
        self.assertEqual(b.last("plan")["json"]["question"], "question B")
        self.assertTrue(set(a.artifacts).isdisjoint(b.artifacts))
        self.assertTrue(set(a.check_evidence).isdisjoint(b.check_evidence))
        with self.assertRaisesRegex(ContractError, "artifact_not_in_execution"): b.read_artifact(next(iter(a.artifacts)))

    def test_same_skill_pool_round_robin_and_independent_states(self):
        p = InstancePool(); a = p.start("a"); b = p.start("b")
        self.assertEqual(p.step(), a)
        self.assertEqual((p.instances[a].state, p.instances[b].state), ("discover", "scope"))
        self.assertEqual(p.step(), b)
        self.assertEqual((p.instances[a].state, p.instances[b].state), ("discover", "discover"))
        self.assertEqual(p.step(), a)
        self.assertEqual(p.step(), b)

    def test_waiting_instance_does_not_block_other_instance(self):
        p = InstancePool(); a = p.start("a", routes={("discover", 1): "need_input"}); b = p.start("b")
        for _ in range(20):
            if p.step() is None: break
        self.assertEqual(p.instances[a].status, "waiting")
        self.assertEqual(p.instances[b].status, "completed")

    def test_cancel_one_instance_does_not_cancel_another(self):
        p = InstancePool(); a = p.start("a"); b = p.start("b"); p.cancel(a)
        for _ in range(10):
            if p.step() is None: break
        self.assertEqual(p.instances[a].status, "cancelled")
        self.assertEqual(p.instances[b].status, "completed")

    def test_same_signal_key_is_scoped_to_each_instance(self):
        p = InstancePool(); route={("discover", 1): "need_input"}; a=p.start("a",routes=route);b=p.start("b",routes=route)
        while p.step() is not None: pass
        data={"urls":["https://example.org/more"],"reason":"more"}
        p.signal(a,"sources.added",data,"same-key",p.instances[a].revision)
        self.assertEqual(p.instances[b].status,"waiting")
        p.signal(b,"sources.added",data,"same-key",p.instances[b].revision)
        self.assertNotEqual(p.instances[a].signal_facts[0]["id"],p.instances[b].signal_facts[0]["id"])

    def test_same_skill_start_idempotency_is_not_skill_deduplication(self):
        p=InstancePool(capacity=2);a=p.start("one");self.assertEqual(p.start("one"),a);b=p.start("two")
        self.assertNotEqual(a,b)
        self.assertEqual(p.start("one"),a)  # admission 满时仍先返回已接受的幂等回执。
        with self.assertRaisesRegex(ContractError,"admission_full"):p.start("three")
        with self.assertRaisesRegex(ContractError,"request_conflict"):p.start("one",inputs={"question":"different","seed_urls":[],"scope":"fixture"})
        self.assertEqual(len(p.instances),2)

    def test_pool_restore_keeps_multiple_ids_and_pending_work(self):
        p=InstancePool();a=p.start("one");b=p.start("two");p.step()
        restored=InstancePool.restore(p.snapshot())
        self.assertEqual(restored.start("one"),a);self.assertEqual(restored.start("two"),b)
        self.assertEqual(restored.instances[a].state,"discover");self.assertEqual(restored.instances[b].state,"scope")
        for _ in range(20):
            if restored.step() is None:break
        self.assertEqual((restored.instances[a].status,restored.instances[b].status),("completed","completed"))

    def test_one_instance_loop_budget_does_not_consume_anothers(self):
        p=InstancePool();a=p.start("a",routes={("catalogue",i):"need_more" for i in range(1,5)});b=p.start("b")
        for _ in range(50):
            if p.step() is None:break
        self.assertEqual((p.instances[a].status,p.instances[b].status),("failed","completed"))
        self.assertEqual(p.instances[b].visits["discover"],1)

    def test_sqlite_cas_is_scoped_by_execution_and_survives_reopen(self):
        with tempfile.TemporaryDirectory() as path:
            db=Path(path)/"state.db"
            c=sqlite3.connect(db);c.execute("PRAGMA journal_mode=WAL");c.execute("CREATE TABLE flow(execution_id TEXT PRIMARY KEY, skill TEXT NOT NULL, state TEXT NOT NULL, revision INTEGER NOT NULL)")
            c.executemany("INSERT INTO flow VALUES(?,?,?,?)",[("A","same-skill","scope",0),("B","same-skill","scope",0)]);c.commit();c.close()
            barrier=threading.Barrier(2)
            def compete(target):
                conn=sqlite3.connect(db,timeout=10)
                revision=conn.execute("SELECT revision FROM flow WHERE execution_id='A'").fetchone()[0]
                barrier.wait(timeout=10)
                count=conn.execute("UPDATE flow SET state=?, revision=revision+1 WHERE execution_id=? AND revision=?",(target,"A",revision)).rowcount
                conn.commit();conn.close();return count
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:results=list(pool.map(compete,["discover","cancelled"]))
            self.assertEqual(sorted(results),[0,1])
            c=sqlite3.connect(db);self.assertEqual(c.execute("SELECT state,revision FROM flow WHERE execution_id='B'").fetchone(),("scope",0))
            self.assertEqual(c.execute("UPDATE flow SET state='discover',revision=revision+1 WHERE execution_id='B' AND revision=0").rowcount,1);c.commit();c.close()
            c=sqlite3.connect(db);self.assertEqual(c.execute("SELECT revision FROM flow ORDER BY execution_id").fetchall(),[(1,),(1,)]);c.close()

    def test_terminal_cannot_bypass_required_accepted_report(self):
        m = Model(); m._move("done", "fixture_attempted_jump")
        self.assertEqual((m.state, m.status), ("scope", "blocked"))
        self.assertEqual(m.events[-1]["type"], "runtime.terminal_output_missing")
        self.assertFalse(any(e.get("to") == "done" for e in m.events))

    def test_gate_fabricated_evidence_reference_blocks(self):
        m = Model(); m._gate = lambda entry, data: {"verdict": "pass", "outcome": "ready", "evidence_refs": ["invented-proof"], "violations": []}
        self.assertEqual(advance(m)["code"], "gate_protocol_invalid")
        self.assertEqual((m.state, m.status, len(m.artifacts)), ("scope", "blocked", 0))

    def test_recheck_failure_takes_rejected_edge(self):
        m = Model(); advance(m, force_check_error=True)
        m._gate = lambda entry, data: {"verdict": "fail", "outcome": None, "evidence_refs": [], "violations": [{"code": "invalid", "subject": "plan", "message": "recheck failed"}]}
        self.assertFalse(m.recheck(expected_revision=m.revision)["accepted"])
        self.assertEqual((m.state, m.status), ("failed", "failed"))

    def test_checker_unexpected_exception_blocks_without_accepting(self):
        saved = HOST_GATES["inquiry.scope.gate"]
        def broken(view, entry, data):
            raise RuntimeError("private provider details")
        HOST_GATES["inquiry.scope.gate"] = broken
        try:
            m = Model(); result = advance(m)
            self.assertEqual((m.state, m.status, len(m.artifacts)), ("scope", "blocked", 0))
            self.assertEqual(next(iter(m.check_evidence.values()))["error_code"], "check_unavailable")
            self.assertNotIn("private provider", json.dumps(m.check_evidence))
            self.assertFalse(any(e.get("type") == "work.accepted" for e in m.events))
            m.cancel(); m.confirm_stopped(); self.assertEqual(m.status, "cancelled")
        finally:
            HOST_GATES["inquiry.scope.gate"] = saved

    def test_candidate_storage_error_blocks_without_rejected_transition(self):
        m = Model()
        def unreadable(*args):
            raise OSError("storage read unavailable")
        m._collect = unreadable
        with tempfile.TemporaryDirectory() as p:
            directory = Path(p); submission = fixture_submission(m, directory)
            result = m.submit(submission, directory)
        self.assertEqual(result["code"], "storage_error")
        self.assertEqual((m.state, m.status, len(m.artifacts)), ("scope", "blocked", 0))

    def test_gate_has_no_state_control_handle(self):
        saved = HOST_GATES["inquiry.scope.gate"]
        def checker(view, entry, data):
            self.assertFalse(hasattr(view, "_move"))
            self.assertFalse(hasattr(view, "cancel"))
            view.state = "done"
            return {"verdict": "pass", "outcome": "ready", "evidence_refs": [view.check_ref], "violations": []}
        HOST_GATES["inquiry.scope.gate"] = checker
        try:
            m = Model(); advance(m); self.assertEqual(m.state, "discover")
        finally:
            HOST_GATES["inquiry.scope.gate"] = saved

    def test_gate_cannot_mutate_candidate_by_shared_reference(self):
        saved = HOST_GATES["inquiry.scope.gate"]
        def checker(view, entry, data):
            data["plan"]["json"]["question"] = "modified in gate"
            return {"verdict": "pass", "outcome": "ready", "evidence_refs": [view.check_ref], "violations": []}
        HOST_GATES["inquiry.scope.gate"] = checker
        try:
            m = Model(); advance(m); self.assertEqual(m.last("plan")["json"]["question"], m.inputs["question"])
        finally:
            HOST_GATES["inquiry.scope.gate"] = saved

    def test_public_start_request_schema(self):
        value = {"protocol_version": 1, "request_key": "start-1", "definition_digest": "a" * 64, "inputs": {"question": "Q"}, "grant_ref": "grant-1", "limits": {"max_active_ms": 600000, "max_wall_ms": 90000000, "max_transitions": 30, "max_runs": 24}}
        validate("start-request.schema.json", value)
        value["limits"]["max_runs"] = 0
        with self.assertRaises(ContractError): validate("start-request.schema.json", value)

    def test_public_signal_cannot_claim_actor_or_target(self):
        value = {"protocol_version": 1, "execution_id": "e", "event_name": "sources.added", "payload": {"urls": ["https://example.org/a"], "reason": "more"}, "request_key": "k", "expected_revision": 1}
        validate("signal-request.schema.json", value)
        value["actor"] = "operator"
        with self.assertRaises(ContractError): validate("signal-request.schema.json", value)
        del value["actor"]; value["target_state"] = "done"
        with self.assertRaises(ContractError): validate("signal-request.schema.json", value)

    def test_public_snapshot_allows_business_state_ids(self):
        value = {"protocol_version": 1, "execution_id": "e", "definition_digest": "a" * 64, "status": "waiting", "current_state": "another_business.custom_state", "visit": 1, "revision": 4, "business_outcome": None, "inputs": {"topic": "generic"}, "progress": {}, "accepted_artifacts": [], "waiting_signals": [], "error": None}
        validate("snapshot.schema.json", value)
        validate("cancel-request.schema.json", {"protocol_version": 1, "execution_id": "e", "request_key": "cancel-1"})

    def test_public_snapshot_can_render_without_run_lookup(self):
        value = {"protocol_version": 1, "execution_id": "e", "definition_digest": "a" * 64, "status": "running", "current_state": "inspect", "visit": 2, "revision": 4, "business_outcome": None, "inputs": {"document": "D"}, "progress": {}, "accepted_artifacts": [{"id": "artifact-1", "slot": "result", "sha256": "b" * 64, "state_id": "inspect", "visit": 1}], "waiting_signals": [], "error": None}
        validate("snapshot.schema.json", value)
        del value["accepted_artifacts"][0]["state_id"]
        with self.assertRaises(ContractError): validate("snapshot.schema.json", value)

    def test_public_snapshot_requires_original_business_inputs(self):
        value = {"protocol_version": 1, "execution_id": "e", "definition_digest": "a" * 64, "status": "waiting", "current_state": "wait", "visit": 1, "revision": 2, "business_outcome": None, "progress": {}, "accepted_artifacts": [], "waiting_signals": [], "error": None}
        with self.assertRaises(ContractError): validate("snapshot.schema.json", value)

    def test_generic_tool_receipt_has_no_http_requirement(self):
        receipt = {"protocol_version": 1, "id": "receipt-1", "execution_id": "e", "run_id": "r", "tool_id": "code.inspect", "occurred_at": "2026-11-02T00:00:00Z", "request_sha256": "a" * 64, "result_sha256": "b" * 64, "payload": {"files": ["src/lib.rs"], "parsed": True}}
        validate("tool-receipt.schema.json", receipt)
        receipt["http_status"] = 200
        with self.assertRaises(ContractError): validate("tool-receipt.schema.json", receipt)

    def test_missing_runtime_input_blocks_before_run(self):
        m = Model(); m._enter("report")
        self.assertEqual((m.state, m.status, m.active), ("report", "blocked", None))
        self.assertEqual(m.events[-1]["type"], "runtime.input_missing")

    def test_valid_but_stale_epoch_rejected(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); m.active["run_epoch"] = 2
            with self.assertRaisesRegex(ContractError, "stale_or_wrong_attempt"): m.submit(s, d)

    def test_wrong_state_cannot_reuse_current_run(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); s["state_id"] = "report"
            with self.assertRaisesRegex(ContractError, "stale_or_wrong_attempt"): m.submit(s, d)

    def test_no_terminal_path_rejected(self):
        b = parse_json((ROOT / "research.bundle.json").read_text())
        b["entries"] = {"inquiry.scope": b["entries"]["inquiry.scope"]}
        b["flow"]["states"] = {"scope": {"kind": "work", "entry": "inquiry.scope", "accepted": {"ready": "scope"}, "rejected": "scope", "failed": "scope"}}
        with self.assertRaisesRegex(ContractError, "no_terminal_path"): compile_bundle(b)

    def test_signal_same_key_different_payload_conflicts(self):
        m = Model(routes={("discover", 1): "need_input"}); through(m, "await_sources")
        m.signal("sources.added", {"urls": ["https://example.org/a"], "reason": "a"}, "k", m.revision)
        with self.assertRaisesRegex(ContractError, "signal_conflict"): m.signal("sources.added", {"urls": ["https://example.org/b"], "reason": "b"}, "k", m.revision)

    def test_signal_out_of_order_rejected(self):
        m = Model()
        with self.assertRaisesRegex(ContractError, "not_waiting"): m.signal("sources.added", {"urls": ["https://example.org/a"], "reason": "a"}, "k", m.revision)

    def test_skill_cannot_grant_itself_network(self):
        with self.assertRaisesRegex(ContractError, "required_capability_denied"): Model(grants=["artifact.read", "artifact.write"])

    def test_unknown_tool_rejected(self):
        b = parse_json((ROOT / "research.bundle.json").read_text()); b["entries"]["inquiry.scope"]["tools"] = ["unknown.shell"]
        with self.assertRaisesRegex(ContractError, "tool_unregistered"): compile_bundle(b)

    def test_duplicate_json_key_rejected(self):
        with self.assertRaisesRegex(ContractError, "duplicate_json_key"): parse_json('{"state":"a","state":"b"}')

    def test_unknown_skill_rejected(self):
        b = parse_json((ROOT / "research.bundle.json").read_text()); b["entries"]["inquiry.scope"]["skills"] = ["missing-skill@1"]
        with self.assertRaisesRegex(ContractError, "skill_unregistered"): compile_bundle(b)

    def test_missing_input_producer_rejected(self):
        b = parse_json((ROOT / "research.bundle.json").read_text()); b["entries"]["inquiry.capture"]["requires"][0]["state"] = "missing"
        with self.assertRaisesRegex(ContractError, "input_producer_missing"): compile_bundle(b)

    def test_gate_missing_evidence_blocks(self):
        m = Model(); m._gate = lambda entry, data: {"verdict": "pass", "outcome": "ready", "evidence_refs": [], "violations": []}
        self.assertEqual(advance(m)["code"], "gate_protocol_invalid")
        self.assertEqual((m.state, m.status), ("scope", "blocked"))

    def test_gate_unknown_outcome_blocks(self):
        m = Model(); m._gate = lambda entry, data: {"verdict": "pass", "outcome": "jump_to_done", "evidence_refs": ["fixture"], "violations": []}
        self.assertEqual(advance(m)["code"], "gate_protocol_invalid")
        self.assertEqual(m.state, "scope")

    def test_gate_error_does_not_advance(self):
        m = Model(); m._gate = lambda entry, data: {"verdict": "error", "outcome": None, "evidence_refs": [], "violations": [{"code": "unavailable", "subject": "gate", "message": "offline"}]}
        self.assertEqual(advance(m)["code"], "check_unavailable")
        self.assertFalse(m.artifacts)

    def test_gate_failure_cannot_have_outcome(self):
        with self.assertRaises(ContractError): validate("check-result.schema.json", {"verdict": "fail", "outcome": "ready", "evidence_refs": [], "violations": [{"code": "bad", "subject": "x", "message": "invalid"}]})

    def test_agent_outcome_suggestion_is_not_gate_result(self):
        m = Model(routes={("catalogue", 1): "need_more"}); through(m, "catalogue")
        advance(m, lambda p: p.update(decision="ready"))
        self.assertEqual(m.state, "discover")

    def test_research_full_path(self):
        m = Model(); through(m, "done")
        self.assertEqual((m.state, m.status), ("done", "completed"))
        self.assertEqual([h["state"] for h in m.history], ["scope", "discover", "capture", "catalogue", "analyze", "report"])
        self.assertEqual(len(m.events), 6)

    def test_second_business_same_engine(self):
        m = Model("document-review.bundle.json", {("inspect", 1): "revise"}, inputs={"document_id": "document-1", "criteria": ["required_sections"]}, grants=["artifact.read", "artifact.write"])
        advance(m); self.assertEqual(m.state, "revise")
        m.signal("revision.submitted", {"document_id": "document-1", "revision": 2, "reason": "revised"}, "k", m.revision)
        # 文档审阅 fixture 用无事实结论，只检验自定义状态和事件编排。
        advance(m); self.assertEqual((m.state, m.status), ("approved", "completed"))

    def test_missing_state_rejected_at_load(self):
        b = json.loads((ROOT / "research.bundle.json").read_text()); b["flow"]["states"]["scope"]["accepted"]["ready"] = "missing"
        with self.assertRaisesRegex(ContractError, "target_missing"): compile_bundle(b)

    def test_unknown_executor_rejected_at_load(self):
        b = json.loads((ROOT / "research.bundle.json").read_text()); b["entries"]["inquiry.scope"]["executor"] = "unknown.cli"
        with self.assertRaisesRegex(ContractError, "executor_unregistered"): compile_bundle(b)

    def test_unknown_gate_rejected_at_load(self):
        b = json.loads((ROOT / "research.bundle.json").read_text()); b["entries"]["inquiry.scope"]["gate"] = "unknown.check"
        with self.assertRaisesRegex(ContractError, "gate_unregistered"): compile_bundle(b)

    def test_missing_outcome_rejected_at_load(self):
        b = json.loads((ROOT / "research.bundle.json").read_text()); del b["flow"]["states"]["capture"]["accepted"]["need_input"]
        with self.assertRaisesRegex(ContractError, "outcome_coverage"): compile_bundle(b)

    def test_terminal_with_transition_rejected(self):
        b = json.loads((ROOT / "research.bundle.json").read_text()); b["flow"]["states"]["done"]["accepted"] = {"ready": "scope"}
        with self.assertRaises(ContractError): compile_bundle(b)

    def test_unreachable_state_rejected(self):
        b = json.loads((ROOT / "research.bundle.json").read_text()); b["flow"]["states"]["orphan"] = {"kind": "terminal", "status": "completed", "outcome": "unused", "requires": []}
        with self.assertRaisesRegex(ContractError, "unreachable_state"): compile_bundle(b)

    def test_unknown_protocol_rejected(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); s["protocol_version"] = 2
            with self.assertRaises(ContractError): m.submit(s, d)

    def test_agent_cannot_submit_runtime_event(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d)
            with self.assertRaisesRegex(ContractError, "untrusted_event_source"): m.submit(s, d, origin="agent")
        self.assertEqual(m.state, "scope")

    def test_agent_cannot_choose_next_state(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); s["next_state"] = "done"
            with self.assertRaises(ContractError): m.submit(s, d)
        self.assertEqual(m.state, "scope")

    def test_wrong_run_rejected(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); s["run_id"] = "other-run"
            with self.assertRaisesRegex(ContractError, "stale_or_wrong_attempt"): m.submit(s, d)

    def test_wrong_contract_rejected(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); s["contract_digest"] = "0" * 64
            with self.assertRaisesRegex(ContractError, "stale_or_wrong_attempt"): m.submit(s, d)

    def test_missing_file_fails_not_advance(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); (d / "plan.json").unlink()
            result = m.submit(s, d)
        self.assertFalse(result["accepted"]); self.assertEqual(m.state, "failed")

    def test_bad_schema_fails(self):
        m = Model(); result = advance(m, lambda p: p.pop("question"))
        self.assertFalse(result["accepted"]); self.assertFalse(m.artifacts)

    def test_path_escape_fails(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); s["artifacts"][0]["path"] = "../outside.json"
            self.assertEqual(m.submit(s, d)["code"], "path_escape")

    def test_symlink_fails(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); (d / "link.json").symlink_to(d / "plan.json"); s["artifacts"][0]["path"] = "link.json"
            self.assertEqual(m.submit(s, d)["code"], "symlink_forbidden")

    def test_duplicate_slot_fails(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); s["artifacts"].append(s["artifacts"][0].copy())
            self.assertEqual(m.submit(s, d)["code"], "duplicate_slot")

    def test_source_receipt_cannot_be_forged_by_agent(self):
        m = Model()
        with self.assertRaisesRegex(ContractError, "untrusted_receipt"): m.record_tool_receipt("x", {}, origin="agent")

    def test_fake_capture_receipt_fails(self):
        m = Model(); through(m, "capture")
        result = advance(m, lambda p: p["items"][0].update(receipt_id="fake"))
        self.assertEqual(result["code"], "receipt_missing")

    def test_capture_content_changed_fails(self):
        m = Model(); through(m, "capture")
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); (d / "captures/s1.txt").write_text("模型伪造的正文")
            self.assertEqual(m.submit(s, d)["code"], "provenance_digest_mismatch")

    def test_failed_url_can_be_valid_stage_result(self):
        m = Model(); through(m, "capture")
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); material = json.loads((d / "materials.json").read_text()); item = material["items"][0]
            receipt = copy.deepcopy(m.receipts[item["receipt_id"]])
            # 模拟可信获取工具本身报告不可访问，非模型覆写回执。
            receipt.update(fetch_status="unavailable", usable=False, body_sha256=None, normalized_sha256=None, transform_version=None, http_status=404, reason="not_found")
            rid = "unavailable-receipt"
            m.record_tool_receipt(rid, receipt)
            item.update(fetch_status="unavailable", snapshot_slot=None, raw_slot=None, http_status=404, reason="not_found", receipt_id=rid)
            (d / "materials.json").write_text(json.dumps(material)); s["artifacts"] = s["artifacts"][:1]
            self.assertTrue(m.submit(s, d)["accepted"])
        self.assertEqual((m.state, m.status), ("await_sources", "waiting"))

    def test_citation_to_unknown_artifact_fails(self):
        m = Model(); through(m, "report")
        result = advance(m, lambda p: p["citations"][0].update(artifact_id="made-up"))
        self.assertEqual(result["code"], "citation_not_accepted")

    def test_wrong_quote_position_fails(self):
        m = Model(); through(m, "report")
        result = advance(m, lambda p: p["citations"][0].update(paragraph=0))
        self.assertEqual(result["code"], "citation_location_mismatch")

    def test_duplicate_submission_does_not_transition_twice(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); first = m.submit(s, d); revision = m.revision
            self.assertEqual(m.submit(s, d), first); self.assertEqual(m.revision, revision)

    def test_changed_duplicate_submission_conflicts(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); m.submit(s, d); s["artifacts"][0]["path"] = "other.json"
            with self.assertRaisesRegex(ContractError, "submission_conflict"): m.submit(s, d)

    def test_cancel_before_result_blocks_late_completion(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); m.cancel()
            self.assertEqual(m.status, "cancelling")
            with self.assertRaisesRegex(ContractError, "not_running"): m.submit(s, d)
            m.confirm_stopped(); self.assertEqual(m.status, "cancelled")

    def test_completion_before_cancel_keeps_terminal(self):
        m = Model(); through(m, "done"); self.assertEqual(m.cancel(), "completed")

    def test_check_error_blocks_and_operator_rechecks(self):
        m = Model(); advance(m, force_check_error=True)
        self.assertEqual((m.state, m.status, len(m.artifacts)), ("scope", "blocked", 0))
        with self.assertRaisesRegex(ContractError, "signal_forbidden"): m.recheck(actor="agent", expected_revision=m.revision)
        m.recheck(expected_revision=m.revision); self.assertEqual(m.state, "discover")

    def test_wait_signal_is_checked_and_idempotent(self):
        m = Model(routes={("discover", 1): "need_input"}); through(m, "await_sources")
        revision = m.revision; payload = {"urls": ["https://example.org/new"], "reason": "补充"}
        result = m.signal("sources.added", payload, "key", revision)
        current = m.revision
        self.assertEqual(m.signal("sources.added", payload, "key", revision), result)
        self.assertEqual(m.revision, current)

    def test_unauthorized_signal_fails(self):
        m = Model(routes={("discover", 1): "need_input"}); through(m, "await_sources")
        with self.assertRaisesRegex(ContractError, "signal_forbidden"): m.signal("sources.added", {"urls": ["x"], "reason": "x"}, "k", m.revision, actor="agent")

    def test_signal_revision_conflict(self):
        m = Model(routes={("discover", 1): "need_input"}); through(m, "await_sources")
        with self.assertRaisesRegex(ContractError, "revision_conflict"): m.signal("sources.added", {"urls": ["x"], "reason": "x"}, "k", m.revision - 1)

    def test_signal_payload_invalid(self):
        m = Model(routes={("discover", 1): "need_input"}); through(m, "await_sources")
        with self.assertRaises(ContractError): m.signal("sources.added", {"urls": []}, "k", m.revision)
        self.assertEqual(m.state, "await_sources")

    def test_wait_timeout_requires_due_time(self):
        m = Model(routes={("discover", 1): "need_input"}); through(m, "await_sources")
        with self.assertRaisesRegex(ContractError, "timeout_not_due"): m.wait_timeout(1)
        m.wait_timeout(86400); self.assertEqual((m.state, m.status), ("partial", "completed"))

    def test_bounded_research_loop(self):
        routes={("catalogue", i): "need_more" for i in range(1, 5)}
        m = Model(routes=routes)
        for _ in range(30):
            if m.status != "running": break
            advance(m)
        self.assertEqual(m.status, "failed")
        self.assertEqual(m.visits["discover"], 3)
        self.assertEqual(m.events[-1]["type"], "runtime.limit_reached")
        actual = [e for e in m.events if "to" in e]
        self.assertEqual(actual[-1]["to"], m.state)
        self.assertEqual(m.events[-1]["requested_target"], "discover")

    def test_restore_waiting_keeps_history_and_accepts_signal(self):
        m = Model(routes={("catalogue", 1): "need_input"}); through(m, "await_sources")
        restored = Model.restore(m.crash_snapshot())
        self.assertEqual(len(restored.artifacts), len(m.artifacts))
        restored.signal("sources.added", {"urls": ["https://example.org/new"], "reason": "补充"}, "k", restored.revision)
        self.assertEqual(restored.state, "discover")

    def test_restore_active_blocks_old_attempt(self):
        m = Model()
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); restored = Model.restore(m.crash_snapshot())
            with self.assertRaisesRegex(ContractError, "not_running"): restored.submit(s, d)
        self.assertEqual(restored.status, "blocked")

    def test_definition_change_not_applied_to_active_instance(self):
        m = Model(); m.bundle["flow"]["version"] = "0.2.0"
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d)
            with self.assertRaisesRegex(ContractError, "definition_changed"): m.submit(s, d)

    def test_accepted_content_is_immutable_copy(self):
        m = Model(); through(m, "capture")
        with tempfile.TemporaryDirectory() as p:
            d = Path(p); s = fixture_submission(m, d); m.submit(s, d)
            saved = next(iter(m.accepted_text().values()))["bytes"]
            (d / "captures/s1.txt").write_text("后续修改")
            self.assertEqual(next(iter(m.accepted_text().values()))["bytes"], saved)


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(ContractTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    files = sorted(p for p in ROOT.glob("*.json") if p.name not in ["verification-result.json", "trace.examples.json"]) + [Path(__file__), ROOT / "requirements.txt"] + sorted((ROOT / "skills").rglob("SKILL.md"))
    summary = {"scope": "TaskLoop 通用契约、多实例模型、公开快照及工具回执 Schema 与最小 SQLite CAS 设计见证；研究/审阅为 fixture，不证明 Rust/Harness、完整事务、隔离或业务语义质量", "tests": result.testsRun, "failures": len(result.failures), "errors": len(result.errors), "passed": result.wasSuccessful(), "python": sys.version.split()[0], "jsonschema": importlib.metadata.version("jsonschema"), "source_sha256": {p.relative_to(ROOT).as_posix(): digest(p.read_bytes()) for p in files}, "bundle_locks": {n: compile_bundle(parse_json((ROOT / n).read_text())) for n in ["research.bundle.json", "document-review.bundle.json"]}, "cases": sorted(n for n in dir(ContractTests) if n.startswith("test_"))}
    (ROOT / "verification-result.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n")
    raise SystemExit(not result.wasSuccessful())
