use crate::{
    definition::validate_schema,
    json::digest,
    runtime::{AttemptRecord, CheckAudit, Inner, PhaseRecord, decode, enter, now_ms, requirements},
    store::{Commit, Row},
    *,
};
use async_trait::async_trait;
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

struct Bridge {
    inner: Arc<Inner>,
    execution_id: String,
    run_id: String,
    epoch: u64,
    entry: EntryContract,
    definition: Definition,
    context: ToolContext,
    candidates: Mutex<BTreeMap<String, Candidate>>,
    calls: Mutex<u64>,
    written_bytes: Mutex<u64>,
    storage_failure: Mutex<Option<Error>>,
    trace: Option<Arc<crate::trace::RunTrace>>,
}
impl Bridge {
    fn write(&self, candidate: Candidate) -> Result<()> {
        if self.context.is_cancelled() {
            return Err(Error::new(
                "stale_attempt",
                "/run",
                "execution was cancelled",
            ));
        }
        if candidate.slot.is_empty()
            || candidate.slot.len() > 160
            || !candidate
                .slot
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(Error::new("artifact_invalid", "/slot", "invalid slot name"));
        }
        if let Some(output) = self.entry.outputs.get(&candidate.slot) {
            self.definition
                .validate_value(&output.schema, &candidate.value()?)?;
        } else if !self
            .entry
            .attachments
            .as_ref()
            .is_some_and(|a| candidate.slot.starts_with(&a.slot_prefix))
        {
            return Err(Error::new(
                "artifact_invalid",
                "/slot",
                "output slot is not declared",
            ));
        }
        let mut all = self.candidates.lock().map_err(Error::io)?;
        let bytes = all
            .iter()
            .filter(|(k, _)| *k != &candidate.slot)
            .map(|(_, v)| v.bytes.len() as u64)
            .sum::<u64>()
            .saturating_add(candidate.bytes.len() as u64);
        if bytes > self.inner.options.max_candidate_bytes {
            return Err(Error::new(
                "limit_reached",
                "/artifacts",
                "candidate byte limit reached",
            ));
        }
        if let Some(policy) = &self.entry.attachments {
            let attachments: Vec<_> = all
                .values()
                .filter(|c| !self.entry.outputs.contains_key(&c.slot) && c.slot != candidate.slot)
                .collect();
            let is_attachment = !self.entry.outputs.contains_key(&candidate.slot);
            if attachments.len() as u64 + u64::from(is_attachment) > policy.max_files
                || attachments
                    .iter()
                    .map(|c| c.bytes.len() as u64)
                    .sum::<u64>()
                    + if is_attachment {
                        candidate.bytes.len() as u64
                    } else {
                        0
                    }
                    > policy.max_bytes
            {
                return Err(Error::new(
                    "limit_reached",
                    "/attachments",
                    "attachment limit reached",
                ));
            }
        }
        let mut written = self.written_bytes.lock().map_err(Error::io)?;
        if written.saturating_add(candidate.bytes.len() as u64)
            > self.inner.options.max_candidate_bytes
        {
            return Err(Error::new(
                "limit_reached",
                "/artifacts",
                "cumulative candidate write budget reached",
            ));
        }
        *written += candidate.bytes.len() as u64;
        let sha = self.inner.store.write_blob(&candidate.bytes)?;
        let mut saved = false;
        for _ in 0..4 {
            let row = self.inner.store.load(&self.execution_id)?;
            let mut record = decode(&row)?;
            if record.epoch != self.epoch || record.status != Status::Running {
                return Err(Error::new(
                    "stale_attempt",
                    "/run",
                    "execution lost authority while writing",
                ));
            }
            record
                .candidate_files
                .entry(self.run_id.clone())
                .or_default()
                .insert(candidate.slot.clone(), sha.clone());
            match self.inner.save(&row, &record, false, None, &[]) {
                Ok(_) => {
                    saved = true;
                    break;
                }
                Err(e) if e.code == "revision_conflict" => continue,
                Err(e) => return Err(e),
            }
        }
        if !saved {
            return Err(Error::new(
                "revision_conflict",
                "/artifact",
                "could not persist candidate reference",
            ));
        }
        all.insert(candidate.slot.clone(), candidate);
        Ok(())
    }
    fn active(&self) -> Result<()> {
        let row = self.inner.store.load(&self.execution_id)?;
        let r = decode(&row)?;
        if r.progress["phase"] != "executing"
            || r.status != Status::Running
            || r.epoch != self.epoch
            || r.active.as_ref().is_none_or(|a| a.id != self.run_id)
        {
            return Err(Error::new(
                "stale_attempt",
                "/run",
                "execution no longer has authority",
            ));
        }
        Ok(())
    }
    fn receipt(&self, id: &str, args: &Value, payload: &Value) -> Result<String> {
        let receipt_id = format!("{}:receipt-{}", self.execution_id, uuid::Uuid::new_v4());
        let receipt = ArtifactReceipt {
            protocol_version: 1,
            id: receipt_id.clone(),
            execution_id: self.execution_id.clone(),
            run_id: self.run_id.clone(),
            tool_id: id.into(),
            occurred_at: chrono::Utc::now().to_rfc3339(),
            request_sha256: digest(&serde_json::to_vec(args)?),
            result_sha256: digest(&serde_json::to_vec(payload)?),
            payload: payload.clone(),
        };
        for _ in 0..4 {
            let row = self.inner.store.load(&self.execution_id)?;
            let mut r = decode(&row)?;
            r.receipts.push(receipt.clone());
            match self.inner.save(&row, &r, false, None, &[]) {
                Ok(_) => return Ok(receipt_id),
                Err(e) if e.code == "revision_conflict" => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::new(
            "revision_conflict",
            "/receipt",
            "concurrent state changes prevented receipt persistence",
        ))
    }
}
#[async_trait]
impl ToolBridge for Bridge {
    fn specs(&self) -> Vec<ToolSpec> {
        self.entry.tools.iter().filter_map(|id|match id.as_str(){
        "artifact.read"=>Some(ToolSpec{id:id.clone(),description:"Read an authorized accepted artifact or a locked Skill resource by exact id; never an arbitrary path.".into(),input_schema:json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false})}),
        "artifact.write"=>Some(ToolSpec{id:id.clone(),description:"Write a JSON candidate to a declared output slot. This does not accept the result or advance the workflow.".into(),input_schema:json!({"type":"object","properties":{"slot":{"type":"string"},"value":{}},"required":["slot","value"],"additionalProperties":false})}),
        _=>self.inner.bindings.tools.get(id).map(|t|t.spec()),
    }).collect()
    }
    async fn call(&self, id: &str, args: Value) -> Result<Value> {
        self.call_traced(id, args, None).await
    }
    async fn call_with_metadata(
        &self,
        id: &str,
        args: Value,
        metadata: ToolCallMetadata,
    ) -> Result<Value> {
        self.call_traced(id, args, Some(metadata)).await
    }
}
impl Bridge {
    async fn call_traced(
        &self,
        id: &str,
        args: Value,
        metadata: Option<ToolCallMetadata>,
    ) -> Result<Value> {
        let span = crate::trace::RunTrace::span();
        if let Some(trace) = &self.trace {
            let parent = metadata
                .as_ref()
                .and_then(|m| m.traceparent.as_ref())
                .and_then(|p| {
                    let parts: Vec<_> = p.split('-').collect();
                    if parts.len() == 4
                        && parts[0] == "00"
                        && parts[1] == trace.trace_id()
                        && parts[2].len() == 16
                        && parts[2].bytes().all(|b| b.is_ascii_hexdigit())
                        && parts[2].bytes().any(|b| b != b'0')
                    {
                        Some(parts[2].to_owned())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| trace.phase_span());
            trace.emit("tool.started", &span, Some(parent), json!({"tool":if self.entry.tools.iter().any(|t|t==id){id}else{"<undeclared>"},"call_id":metadata.as_ref().map(|m|&m.call_id),"source_run_id":metadata.as_ref().map(|m|&m.source_run_id),"native_call_id":metadata.as_ref().and_then(|m|m.native_call_id.as_ref()),"input_content":null}));
            if self.active().is_ok() && self.entry.tools.iter().any(|t| t == id) {
                trace.queue_content(id, "input", &span, &args);
            }
        }
        let result = self.call_inner(id, args).await;
        if let Some(trace) = &self.trace {
            trace.emit("tool.finished", &span, None, json!({"outcome":if result.is_ok(){"succeeded"}else{"failed"},"code":result.as_ref().err().map(|e|&e.code),"output_content":null}));
            if let Ok(value) = &result {
                trace.queue_content(id, "output", &span, value);
            }
        }
        if let Err(error) = &result
            && matches!(error.code.as_str(), "storage_error" | "revision_conflict")
        {
            // Infrastructure failures cannot be erased or reclassified by an
            // executor's return value, even if it retries a later tool call.
            let mut failure = self.storage_failure.lock().map_err(Error::io)?;
            if failure.is_none() {
                *failure = Some(error.clone());
            }
        }
        result
    }
}
impl Bridge {
    async fn call_inner(&self, id: &str, args: Value) -> Result<Value> {
        self.active()?;
        if !self.entry.tools.iter().any(|t| t == id) {
            return Err(Error::new(
                "capability_denied",
                "/tool",
                "tool is not granted to current entry",
            ));
        }
        let spec = self
            .specs()
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::new("binding_missing", "/tool", "registered tool unavailable"))?;
        validate_schema(&spec.input_schema, &args, "/tool/input")?;
        {
            let mut calls = self.calls.lock().map_err(Error::io)?;
            if *calls >= self.inner.options.max_tool_calls_per_run {
                return Err(Error::new(
                    "limit_reached",
                    "/tools",
                    "tool call budget reached",
                ));
            }
            *calls += 1;
        }
        match id {
            "artifact.read" => {
                let requested = args["id"].as_str().unwrap_or("");
                if let Some(path) = requested.strip_prefix("skill:") {
                    for (skill, package) in &self.definition.skills {
                        if !self.entry.skills.contains(skill) {
                            continue;
                        }
                        if let Some(resource) = path.strip_prefix(&format!("{skill}/"))
                            && let Some(bytes) = package.files.get(resource)
                        {
                            return Ok(
                                json!({"id":requested,"sha256":digest(bytes),"text":std::str::from_utf8(bytes).map_err(|_|Error::new("capability_denied","/resource","this text profile cannot read binary methods"))?}),
                            );
                        }
                    }
                    return Err(Error::new(
                        "capability_denied",
                        "/resource",
                        "method resource is not in the locked current package",
                    ));
                }
                let a = self
                    .context
                    .accepted
                    .iter()
                    .find(|a| a.reference.id == requested)
                    .ok_or_else(|| {
                        Error::new(
                            "capability_denied",
                            "/artifact",
                            "artifact not granted to current entry",
                        )
                    })?;
                Ok(
                    json!({"artifact":a.reference,"text":std::str::from_utf8(&a.bytes).map_err(|_|Error::new("artifact_invalid","/artifact","artifact is not text"))?}),
                )
            }
            "artifact.write" => {
                let candidate =
                    Candidate::json(args["slot"].as_str().unwrap_or(""), &args["value"])?;
                let sha = candidate.sha256();
                self.write(candidate)?;
                Ok(json!({"written":true,"slot":args["slot"],"sha256":sha}))
            }
            _ => {
                let tool =
                    self.inner.bindings.tools.get(id).cloned().ok_or_else(|| {
                        Error::new("binding_missing", "/tool", "tool unavailable")
                    })?;
                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(45),
                    AssertUnwindSafe(tool.call(self.context.clone(), args.clone())).catch_unwind(),
                )
                .await;
                let result = match output {
                    Ok(Ok(Ok(v))) => v,
                    Ok(Ok(Err(e))) => return Err(e),
                    Ok(Err(_)) => {
                        return Err(Error::new(
                            "tool_unavailable",
                            "/tool",
                            "tool callback failed",
                        ));
                    }
                    Err(_) => {
                        return Err(Error::new(
                            "tool_unavailable",
                            "/tool",
                            "tool callback deadline reached",
                        ));
                    }
                };
                self.active()?;
                let attachment_info: Vec<_> = result
                    .files
                    .iter()
                    .map(|f| json!({"slot":f.slot,"sha256":f.sha256(),"bytes":f.bytes.len()}))
                    .collect();
                for candidate in result.files {
                    self.write(candidate)?;
                }
                let receipt = self.receipt(id, &args, &result.payload)?;
                Ok(
                    json!({"receipt_id":receipt,"result":result.payload,"attachments":attachment_info}),
                )
            }
        }
    }
}
pub(crate) async fn run(inner: Arc<Inner>, row: Row) {
    let trace = decode(&row).ok().and_then(|r| {
        let a = r.active.as_ref()?;
        let trace = inner.trace.as_ref()?.session(
            &row.id,
            &a.id,
            &r.current_state,
            *r.visits.get(&r.current_state)?,
        );
        let executor = inner.bindings.executors.get(&a.executor)?;
        if trace.reserve_executor(&a.executor, &executor.identity()) {
            Some(trace)
        } else {
            None
        }
    });
    if let Some(trace) = &trace {
        trace.emit("phase.started", &trace.phase_span(), None, json!({}));
    }
    let mut execution_stopped = true;
    if let Err(error) = run_checked(
        inner.clone(),
        row.clone(),
        &mut execution_stopped,
        trace.clone(),
    )
    .await
        && let Ok(latest) = inner.store.load(&row.id)
        && let Ok(mut record) = decode(&latest)
        && !record.status.terminal()
        && record.status != Status::Cancelling
    {
        record.status = Status::Blocked;
        record.error = Some(error);
        if execution_stopped {
            record.active = None;
            record.stop_unknown = false;
        } else {
            record.stop_unknown = true;
        }
        let _ = inner.save(&latest, &record, false, None, &[]);
    }
    // Execution already returned a confirmed outcome. A losing final CAS must not
    // discard that stop fact or force an in-process executor to rediscover it.
    if execution_stopped {
        loop {
            let Ok(latest) = inner.store.load(&row.id) else {
                break;
            };
            let Ok(mut record) = decode(&latest) else {
                break;
            };
            if record.status != Status::Cancelling {
                break;
            }
            record.status = Status::Cancelled;
            record.active = None;
            record.stop_unknown = false;
            record.error = None;
            record.business_outcome = None;
            record.progress = json!({"phase":"cancelled"});
            match inner.save(&latest, &record, false, None, &[]) {
                Ok(_) => break,
                Err(e) if e.code == "revision_conflict" => tokio::task::yield_now().await,
                Err(_) => break,
            }
        }
    }
    if let Some(trace) = &trace {
        let data=inner.store.load(&row.id).and_then(|r|decode(&r)).ok().map(|r|json!({"status":r.status,"stop_unknown":r.stop_unknown,"code":r.error.map(|e|e.code)})).unwrap_or_else(||json!({"status":"unknown"}));
        trace.emit("phase.finished", &trace.phase_span(), None, data);
    }
    if let Ok(mut handles) = inner.handles.lock() {
        handles.remove(&row.id);
    }
}
async fn run_checked(
    inner: Arc<Inner>,
    row: Row,
    execution_stopped: &mut bool,
    trace: Option<Arc<crate::trace::RunTrace>>,
) -> Result<()> {
    let mut record = decode(&row)?;
    let definition = inner.store.definition(&record.definition_digest)?;
    if record.binding_fingerprint != inner.binding_fingerprint {
        record.status = Status::Blocked;
        record.error = Some(Error::new(
            "binding_changed",
            "/bindings",
            "locked host implementation/configuration is unavailable; create a new execution",
        ));
        inner.save(&row, &record, false, None, &[])?;
        return Ok(());
    }
    let State::Work {
        entry,
        accepted,
        rejected,
        failed,
    } = definition.bundle.flow.states[&record.current_state].clone()
    else {
        return Err(Error::new(
            "definition_invalid",
            "/state",
            "queued state is not work",
        ));
    };
    let contract = definition.bundle.entries[&entry].clone();
    let executor = inner
        .bindings
        .executors
        .get(&contract.executor)
        .cloned()
        .ok_or_else(|| Error::new("binding_missing", "/executor", "executor unavailable"))?;
    let grant = inner
        .bindings
        .grants
        .get(&record.grant_ref)
        .ok_or_else(|| Error::new("capability_denied", "/grant", "grant no longer available"))?;
    if !contract.tools.iter().all(|t| grant.tools.contains(t)) {
        return Err(Error::new(
            "capability_denied",
            "/tools",
            "entry requires tools outside current grant",
        ));
    }
    if record.deadline_ms <= now_ms() || record.active_ms >= record.limits.max_active_ms {
        return Err(Error::new(
            "limit_reached",
            "/dispatch",
            "queued execution exhausted its budget before dispatch",
        ));
    }
    let references = requirements(&record, &definition, &contract.requires)?;
    let stage_input = json!({"flow_input":record.inputs,"input_artifact_ids":references.iter().map(|a|&a.id).collect::<Vec<_>>(),"signal_fact_ids":record.signals.iter().map(|s|&s.id).collect::<Vec<_>>()});
    definition.validate_value(&contract.input_schema, &stage_input)?;
    let mut artifacts = Vec::new();
    for reference in &references {
        artifacts.push(ArtifactData {
            bytes: inner.store.read_blob(&reference.sha256)?.into(),
            reference: reference.clone(),
        });
    }
    let mut active = record
        .active
        .clone()
        .ok_or_else(|| Error::new("stale_attempt", "/run", "queued run missing"))?;
    active.started = true;
    active.started_ms = now_ms();
    record.active = Some(active.clone());
    record.progress = json!({"phase":"executing"});
    let cancel = inner.stop.child_token();
    {
        // cancel() uses this same lock before deciding whether execution is absent.
        // Publish the started fact and its cancellation handle as one coordinated admission.
        let mut handles = inner.handles.lock().map_err(Error::io)?;
        if !matches!(
            inner.save(&row, &record, false, None, &[])?,
            Commit::Saved(_)
        ) {
            return Ok(());
        }
        handles.insert(row.id.clone(), cancel.clone());
    }
    let visit = record.visits[&record.current_state];
    let context = ToolContext {
        inputs: record.inputs.clone(),
        state_id: record.current_state.clone(),
        visit,
        accepted: artifacts.clone(),
        signal_facts: record.signals.clone(),
        cancellation: cancel.clone(),
    };
    let bridge = Arc::new(Bridge {
        inner: inner.clone(),
        execution_id: row.id.clone(),
        run_id: active.id.clone(),
        epoch: active.epoch,
        entry: contract.clone(),
        definition: definition.clone(),
        context,
        candidates: Mutex::new(BTreeMap::new()),
        calls: Mutex::new(0),
        written_bytes: Mutex::new(0),
        storage_failure: Mutex::new(None),
        trace: trace.clone(),
    });
    let time_left = record
        .limits
        .max_active_ms
        .saturating_sub(record.active_ms)
        .min(record.deadline_ms.saturating_sub(now_ms()))
        .min(240_000);
    let output_schemas = contract
        .outputs
        .iter()
        .map(|(slot, out)| (slot.clone(), definition.schemas[&out.schema].clone()))
        .collect();
    let invocation = Invocation {
        stage_input,
        output_schemas,
        execution_id: row.id.clone(),
        run_id: active.id.clone(),
        state_id: record.current_state.clone(),
        visit,
        entry: contract.clone(),
        skills: definition
            .skills
            .iter()
            .filter(|(id, _)| contract.skills.contains(id))
            .map(|(id, p)| (id.clone(), p.clone()))
            .collect(),
        inputs: record.inputs.clone(),
        artifacts: references,
        signal_facts: record.signals.clone(),
        work_dir: inner.store.run_dir(&active.id)?,
        timeout_ms: time_left,
    };
    if let Some(t) = &trace {
        t.emit("executor.capabilities",&t.phase_span(),None,json!({"internal_observation":if executor.internal_observation(){"available"}else{"unavailable"}}));
    }
    let mut internal_stop = false;
    *execution_stopped = false;
    let outcome = if time_left == 0 {
        ExecutionOutcome::Failed {
            code: "limit_reached".into(),
            message: "execution budget expired before adapter invocation".into(),
        }
    } else if cancel.is_cancelled() {
        ExecutionOutcome::Cancelled
    } else {
        match tokio::time::timeout(
            std::time::Duration::from_millis(time_left.max(1)),
            AssertUnwindSafe(async {
                if inner.trace.is_some() {
                    executor
                        .execute_observed(
                            invocation,
                            bridge.clone(),
                            cancel.clone(),
                            trace.clone().map(|t| t as Arc<dyn MilkieObserver>),
                        )
                        .await
                } else {
                    executor
                        .execute(invocation, bridge.clone(), cancel.clone())
                        .await
                }
            })
            .catch_unwind(),
        )
        .await
        {
            Ok(Ok(outcome)) => outcome,
            failed_execution => {
                internal_stop = true;
                let panicked = matches!(failed_execution, Ok(Err(_)));
                cancel.cancel();
                if executor.stop_or_confirm(&active.id).await == StopState::Confirmed {
                    ExecutionOutcome::Failed {
                        code: if panicked {
                            "executor_failed"
                        } else {
                            "limit_reached"
                        }
                        .into(),
                        message: if panicked {
                            "execution adapter panicked"
                        } else {
                            "execution deadline exceeded"
                        }
                        .into(),
                    }
                } else {
                    ExecutionOutcome::StopUnknown {
                        message: "execution failed without a confirmed stop".into(),
                    }
                }
            }
        }
    };
    if let Some(t) = &trace {
        t.emit("executor.finished",&t.phase_span(),None,json!({"outcome":match &outcome {ExecutionOutcome::Completed{..}=>"completed",ExecutionOutcome::Failed{..}=>"failed",ExecutionOutcome::Cancelled=>"cancelled",ExecutionOutcome::StopUnknown{..}=>"stop_unknown"},"code":match &outcome{ExecutionOutcome::Failed{code,..}=>Some(code),_=>None}}));
    }
    *execution_stopped = !matches!(outcome, ExecutionOutcome::StopUnknown { .. });
    let latest = inner.store.load(&row.id)?;
    let mut current = decode(&latest)?;
    current.attempts.push(AttemptRecord {
        run_id: active.id.clone(),
        state: record.current_state.clone(),
        visit,
        finished_ms: now_ms(),
        outcome: outcome.clone(),
    });
    current.active_ms = current
        .active_ms
        .saturating_add(now_ms().saturating_sub(active.started_ms));
    if matches!(outcome, ExecutionOutcome::StopUnknown { .. }) {
        if current.status != Status::Cancelling {
            current.status = Status::Blocked;
        }
        current.stop_unknown = true;
        current.error = Some(Error::new(
            "stop_unconfirmed",
            "/run",
            "adapter could not confirm execution stopped",
        ));
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    if current.status == Status::Cancelling
        || current.epoch != active.epoch
        || (cancel.is_cancelled() && !internal_stop)
    {
        current.status = Status::Cancelled;
        current.active = None;
        current.stop_unknown = false;
        current.progress = json!({"phase":"cancelled"});
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    if current.active.as_ref().is_none_or(|a| a.id != active.id) {
        return Err(Error::new("stale_attempt", "/run", "late result"));
    }
    if let Some(error) = bridge.storage_failure.lock().map_err(Error::io)?.clone() {
        current.status = Status::Blocked;
        current.active = None;
        current.stop_unknown = false;
        current.business_outcome = None;
        current.error = Some(error);
        current.progress = json!({"phase":"blocked"});
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    match outcome {
        ExecutionOutcome::Cancelled => {
            current.status = Status::Cancelled;
            current.active = None;
            inner.save(&latest, &current, false, None, &[])?;
            return Ok(());
        }
        ExecutionOutcome::Failed { code, message } => {
            enter(&mut current, &definition, &failed, true)?;
            current.error = Some(Error::new(&code, "/executor", message));
            inner.save(&latest, &current, true, None, &[])?;
            return Ok(());
        }
        _ => {}
    }
    let candidates = bridge.candidates.lock().map_err(Error::io)?.clone();
    for (slot, output) in &contract.outputs {
        if output.required && !candidates.contains_key(slot) {
            enter(&mut current, &definition, &rejected, true)?;
            current.error = Some(Error::new(
                "artifact_invalid",
                format!("/outputs/{slot}"),
                "required candidate missing",
            ));
            inner.save(&latest, &current, true, None, &[])?;
            return Ok(());
        }
    }
    let check_ref = format!("{}:check-{}", row.id, uuid::Uuid::new_v4());
    current.checks.push(CheckAudit {
        id: check_ref.clone(),
        run_id: active.id.clone(),
        gate: contract.gate.clone(),
        candidate_digests: candidates
            .iter()
            .map(|(slot, c)| (slot.clone(), c.sha256()))
            .collect(),
        result: None,
    });
    current.progress = json!({"phase":"checking"});
    let saved = inner.save(&latest, &current, false, None, &[])?;
    let Commit::Saved(check_row) = saved else {
        return Ok(());
    };

    let facts: Vec<_> = current
        .receipts
        .iter()
        .filter(|f| f.run_id == active.id)
        .cloned()
        .collect();
    let gate = self_gate(&inner, &contract.gate)?;
    let authorized_evidence: BTreeSet<_> =
        artifacts.iter().map(|a| a.reference.id.clone()).collect();
    let context = GateContext {
        inputs: current.inputs.clone(),
        state_id: current.current_state.clone(),
        visit,
        candidates: candidates.clone(),
        accepted: artifacts,
        tool_facts: facts.clone(),
        signal_facts: current.signals.clone(),
        check_evidence_ref: check_ref.clone(),
        cancellation: cancel.clone(),
    };
    let check_started = now_ms();
    let check_budget = current
        .limits
        .max_active_ms
        .saturating_sub(current.active_ms)
        .min(current.deadline_ms.saturating_sub(check_started))
        .min(120_000);
    if check_budget == 0 {
        current.status = Status::Blocked;
        current.active = None;
        current.error = Some(Error::new(
            "limit_reached",
            "/gate",
            "execution budget exhausted before domain check",
        ));
        current.progress = json!({"phase":"blocked"});
        inner.save(&check_row, &current, false, None, &[])?;
        return Ok(());
    }
    let check_span = crate::trace::RunTrace::span();
    if let Some(t) = &trace {
        t.emit(
            "check.started",
            &check_span,
            Some(t.phase_span()),
            json!({"gate":contract.gate,"check_ref":check_ref}),
        );
    }
    let checked = tokio::select! {
        _=cancel.cancelled()=>CheckResult::unavailable("check cancelled"),
        result=tokio::time::timeout(std::time::Duration::from_millis(check_budget),AssertUnwindSafe(gate.check(context)).catch_unwind())=>match result {Ok(Ok(v))=>v,_=>CheckResult::unavailable("gate failed or timed out")}
    };
    if let Some(t) = &trace {
        t.emit(
            "check.finished",
            &check_span,
            None,
            json!({"verdict":checked.verdict}),
        );
    }
    let latest = inner.store.load(&row.id)?;
    let mut current = decode(&latest)?;
    if current.status == Status::Cancelling || current.epoch != active.epoch {
        current.status = Status::Cancelled;
        current.active = None;
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    current.active_ms = current
        .active_ms
        .saturating_add(now_ms().saturating_sub(check_started));
    if now_ms() >= current.deadline_ms || current.active_ms >= current.limits.max_active_ms {
        current.status = Status::Blocked;
        current.active = None;
        current.error = Some(Error::new(
            "limit_reached",
            "/gate",
            "check exhausted execution budget",
        ));
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    if let Some(audit) = current.checks.iter_mut().find(|a| a.id == check_ref) {
        audit.result = Some(checked.clone());
    }
    let checked_json = serde_json::to_value(&checked)?;
    let schema: Value = serde_json::from_str(include_str!("../schemas/check-result.schema.json"))?;
    let valid = validate_schema(&schema, &checked_json, "/gate").is_ok()
        && checked.evidence_refs.iter().all(|e| {
            e == &check_ref || facts.iter().any(|f| &f.id == e) || authorized_evidence.contains(e)
        });
    if !valid || matches!(checked.verdict, Verdict::Error) {
        current.status = Status::Blocked;
        current.active = None;
        current.error = Some(Error::new(
            if valid {
                "check_unavailable"
            } else {
                "gate_protocol_invalid"
            },
            "/gate",
            "check did not produce a usable trusted result",
        ));
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    if matches!(checked.verdict, Verdict::Fail) {
        enter(&mut current, &definition, &rejected, true)?;
        current.error = Some(Error::new(
            "domain_check_failed",
            "/gate",
            checked
                .violations
                .first()
                .map(|v| v.message.clone())
                .unwrap_or_default(),
        ));
        inner.save(&latest, &current, true, None, &[])?;
        return Ok(());
    }
    let Some(target) = checked
        .outcome
        .as_ref()
        .and_then(|o| accepted.get(o))
        .cloned()
    else {
        current.status = Status::Blocked;
        current.active = None;
        current.error = Some(Error::new(
            "gate_protocol_invalid",
            "/gate/outcome",
            "outcome not declared",
        ));
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    };
    let mut accepted_refs = Vec::new();
    for candidate in candidates.values() {
        let sha = inner.store.write_blob(&candidate.bytes)?;
        accepted_refs.push(ArtifactRef {
            id: format!("{}:artifact-{}", row.id, uuid::Uuid::new_v4()),
            sha256: sha,
            slot: candidate.slot.clone(),
            state_id: current.current_state.clone(),
            visit,
        });
    }
    current.history.push(PhaseRecord {
        run_id: active.id.clone(),
        state: current.current_state.clone(),
        visit,
        artifacts: accepted_refs.clone(),
        check: checked,
    });
    enter(&mut current, &definition, &target, true)?;
    inner.save(&latest, &current, true, None, &accepted_refs)?;
    inner.notify.notify_one();
    let _ = check_row;
    Ok(())
}
fn self_gate(inner: &Inner, id: &str) -> Result<Arc<dyn DomainGate>> {
    inner
        .bindings
        .gates
        .get(id)
        .cloned()
        .ok_or_else(|| Error::new("binding_missing", "/gate", "gate unavailable"))
}
