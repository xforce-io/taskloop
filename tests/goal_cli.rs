//! Real CLI processes and a deterministic Milkie protocol peer, not a real model/Harness proof.
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    host: PathBuf,
    goal: PathBuf,
}
impl Fixture {
    fn new(mode: &str, hold: Option<&str>) -> Self {
        let root = tempfile::Builder::new()
            .prefix("tl10-")
            .tempdir_in("/tmp")
            .unwrap();
        for folder in ["config", "sessions", "milkie"] {
            fs::create_dir(root.path().join(folder)).unwrap();
        }
        fs::write(root.path().join("connection.json"), serde_json::to_vec(&json!({"contractVersion":1,"fields":{"transport":"agent-cli","runtime":"codex","model":"fixture"}})).unwrap()).unwrap();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut goal: Value = serde_json::from_str(include_str!(
            "../docs/design/10-goal-dag-orchestration/contract/goal-request.example.json"
        ))
        .unwrap();
        goal["spec"]["inputs"]["document_text"] = json!("Fixture original document.");
        let limits = goal["spec"]["limits"].clone();
        let policy = json!({"policy_id":"cli-fixture","max_run_ms":10000,"max_check_ms":1000,"max_tool_calls":8,"submission_reserve":2,"finalize_before_ms":500,"max_model_iterations":null,"max_candidate_bytes":262144});
        let mut env = json!({"MILKIE_FIXTURE_MODE":mode,"MILKIE_FIXTURE_STARTED_FILE":root.path().join("started.log")});
        if let Some(stage) = hold {
            env["MILKIE_FIXTURE_GOAL_HOLD_STAGE"] = json!(stage);
        }
        let config = json!({"protocol_version":1,"factory":"document-review@1","store_dir":root.path().join("store"),"resources_root":repo.join("docs/design/10-goal-dag-orchestration/contract/document-review-package"),"configuration_version":"cli-fixture-v1",
            "milkie":{"program":"/usr/bin/env","args":["python3",repo.join("tests/fixtures/milkie_sidecar.py").to_str().unwrap()],"connection_file":root.path().join("connection.json"),"data_dir":root.path().join("milkie"),"config_dir":root.path().join("config"),"session_dir":root.path().join("sessions"),"env":env},
            "entry_budgets":{"planner@1":policy,"review@1":policy,"quality@1":policy},"goal_grants":{"local-review":{"profile_refs":["document-review@1"],"capabilities":["review@1"],"tools":["artifact.write"],"limits":limits}},"caller_grants":["local-review"],"limits":limits,"max_admitted_goals":8,"max_tool_calls_per_run":16,"max_candidate_bytes":1048576,
            "trace":{"options":{"max_events_per_run":1000,"max_bytes_per_run":1048576,"source_max_events":1000,"source_max_bytes":1048576},"policy":{"policy_id":"cli-fixture","max_total_events":10000,"max_total_bytes":10485760,"max_runs":100,"retention_ms":86400000}}});
        let host = root.path().join("host.json");
        let goal_path = root.path().join("goal.json");
        fs::write(&host, serde_json::to_vec(&config).unwrap()).unwrap();
        fs::write(&goal_path, serde_json::to_vec(&goal).unwrap()).unwrap();
        Self {
            root,
            host,
            goal: goal_path,
        }
    }
    fn command(&self) -> Command {
        Command::new(env!("CARGO_BIN_EXE_taskloop"))
    }
    fn run(&self) -> Output {
        self.command()
            .args(["goal", "run", "--config"])
            .arg(&self.host)
            .arg("--spec")
            .arg(&self.goal)
            .arg("--json")
            .output()
            .unwrap()
    }
    fn start(&self) -> OwnedChild {
        let mut child = self
            .command()
            .args(["goal", "run", "--config"])
            .arg(&self.host)
            .arg("--spec")
            .arg(&self.goal)
            .arg("--json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let admitted: Value =
            serde_json::from_str(&line).expect("foreground admission must be public NDJSON");
        assert_eq!(admitted["type"], "goal.admitted");
        OwnedChild {
            child: Some(child),
            reader: Some(reader),
            prefix: line.into_bytes(),
            goal_id: admitted["goal_id"].as_str().unwrap().into(),
        }
    }
    fn start_resume(&self, id: &str, revision: u64) -> OwnedChild {
        let mut child = self
            .command()
            .args(["goal", "resume", id, "--config"])
            .arg(&self.host)
            .args([
                "--request-key",
                "resume-wait",
                "--revision",
                &revision.to_string(),
                "--json",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let event: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["type"], "command.receipt");
        assert_eq!(event["goal_id"], id);
        OwnedChild {
            child: Some(child),
            reader: Some(reader),
            prefix: line.into_bytes(),
            goal_id: id.into(),
        }
    }
    fn wait_status(&self, id: &str, status: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let snapshot = self.query("get", id);
            if snapshot["status"] == status {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "expected {status}, actual {snapshot}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn query(&self, command: &str, id: &str) -> Value {
        let out = self
            .command()
            .args(["goal", command, id, "--store"])
            .arg(self.root.path().join("store"))
            .arg("--json")
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["data"].clone()
    }
    fn started(&self) -> usize {
        fs::read_to_string(self.root.path().join("started.log"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn wait_started(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.started() < count {
            assert!(
                Instant::now() < deadline,
                "native fixture did not start phase {count}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
struct OwnedChild {
    child: Option<Child>,
    reader: Option<BufReader<ChildStdout>>,
    prefix: Vec<u8>,
    goal_id: String,
}
impl OwnedChild {
    fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }
    fn finish(mut self) -> Output {
        let mut reader = self.reader.take().unwrap();
        let mut bytes = std::mem::take(&mut self.prefix);
        let output = thread::spawn(move || {
            reader.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let mut result = self.child.take().unwrap().wait_with_output().unwrap();
        result.stdout = output.join().unwrap();
        result
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().unwrap().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}
fn records(out: &Output, code: i32) -> Vec<Value> {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn snapshot(out: &Output, code: i32) -> Value {
    let events = records(out, code);
    assert_eq!(events.last().unwrap()["type"], "goal.snapshot");
    events.last().unwrap()["data"].clone()
}
#[test]
fn cli_runs_queries_exports_and_replays_without_new_native_invocations() {
    let f = Fixture::new("goal_success", None);
    let result = snapshot(&f.run(), 0);
    let id = result["goal_id"].as_str().unwrap();
    assert_eq!(result["outcome"], "achieved");
    assert_eq!(result["budget"]["runs_used"], 3);
    assert_eq!(f.started(), 3);
    assert_eq!(f.query("get", id)["revision"], result["revision"]);
    assert_eq!(f.query("plan", id)["plan_digest"], result["plan_digest"]);
    assert_eq!(f.query("budget", id)["budget"]["used"]["tool_calls"], 3);
    let trace = f
        .command()
        .args(["goal", "trace", id, "--store"])
        .arg(f.root.path().join("store"))
        .args(["--after", "0", "--limit", "1000", "--json"])
        .output()
        .unwrap();
    assert_eq!(trace.status.code(), Some(0));
    let trace: Value = serde_json::from_slice(&trace.stdout).unwrap();
    assert!(
        trace["data"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"]["kind"] == "goal.finished")
    );
    let artifact = result["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["accepted"] == true)
        .unwrap();
    let artifact_id = artifact["artifact"]["id"].as_str().unwrap();
    let output = f.root.path().join("report.json");
    let export = |overwrite: bool| {
        let mut cmd = f.command();
        cmd.args(["goal", "artifact", artifact_id, "--store"])
            .arg(f.root.path().join("store"))
            .arg("--output")
            .arg(&output)
            .arg("--json");
        if overwrite {
            cmd.arg("--overwrite");
        }
        cmd.output().unwrap()
    };
    assert_eq!(export(false).status.code(), Some(0));
    let bytes = fs::read(&output).unwrap();
    assert!(result["budget"]["logical_accepted_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&export(false).stdout).unwrap()["data"]["status"],
        "already_exported"
    );
    fs::write(&output, b"keep me").unwrap();
    assert_eq!(export(false).status.code(), Some(2));
    assert_eq!(fs::read(&output).unwrap(), b"keep me");
    assert_eq!(export(true).status.code(), Some(0));
    assert_eq!(fs::read(&output).unwrap(), bytes);
    let replay = snapshot(&f.run(), 0);
    assert_eq!(replay["revision"], result["revision"]);
    assert_eq!(f.started(), 3);
    assert!(!f.root.path().join("store/owner.sock").exists());
    let human = f
        .command()
        .args(["goal", "run", "--config"])
        .arg(&f.host)
        .arg("--spec")
        .arg(&f.goal)
        .output()
        .unwrap();
    assert_eq!(human.status.code(), Some(0));
    let human = String::from_utf8_lossy(&human.stderr);
    assert!(human.contains(id));
    assert!(human.contains("Achieved"));
    assert!(human.contains("节点完成：1/1"));
    assert!(human.contains(artifact_id));
    assert!(human.contains(artifact["artifact"]["sha256"].as_str().unwrap()));
    assert_eq!(f.started(), 3);
}
#[test]
fn cli_preserves_reports_and_distinguishes_not_met_checker_error_and_planning_exhaustion() {
    for (mode, code, outcome, runs) in [
        ("goal_not_met", 4, "not_met", 3),
        ("goal_check_error", 5, "failed", 3),
        ("goal_bad_plan", 3, "failed", 3),
    ] {
        let f = Fixture::new(mode, None);
        let result = snapshot(&f.run(), code);
        assert_eq!(result["outcome"], outcome);
        assert_eq!(result["budget"]["runs_used"], runs);
        assert_eq!(f.started(), runs as usize);
        if mode == "goal_bad_plan" {
            let runs = result["runs"].as_object().unwrap();
            let failures: Vec<_> = runs
                .values()
                .filter_map(|run| run["plan_validation_failure"].as_object())
                .collect();
            assert_eq!(failures.len(), 3);
            let path = failures[0]["issues"][0]["path"].as_str().unwrap();
            assert!(path.starts_with('/'));
            let human = f
                .command()
                .args([
                    "goal",
                    "get",
                    result["goal_id"].as_str().unwrap(),
                    "--store",
                ])
                .arg(f.root.path().join("store"))
                .output()
                .unwrap();
            assert_eq!(human.status.code(), Some(0));
            let human = String::from_utf8_lossy(&human.stderr);
            assert!(human.contains("计划拒绝"));
            assert!(human.contains(path));
            assert_eq!(f.started(), runs.len());
        }
        if mode != "goal_bad_plan" {
            assert_eq!(
                result["artifacts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|a| a["accepted"] == true)
                    .count(),
                1
            );
        }
    }
}
#[test]
fn cli_online_cancel_returns_an_intent_then_closes_without_erasing_the_accepted_report() {
    let f = Fixture::new("goal_success", Some("final"));
    let child = f.start();
    f.wait_started(3);
    let id = child.goal_id.clone();
    let state = f.query("get", &id);
    let cancelled = f
        .command()
        .args(["goal", "cancel", &id, "--store"])
        .arg(f.root.path().join("store"))
        .args([
            "--request-key",
            "cancel-foreground",
            "--revision",
            &state["revision"].as_u64().unwrap().to_string(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(cancelled.status.code(), Some(0));
    let response: Value = serde_json::from_slice(&cancelled.stdout).unwrap();
    assert!(response["data"]["receipt"]["accepted"].as_bool().unwrap());
    let result = snapshot(&child.finish(), 7);
    assert_eq!(result["outcome"], "cancelled");
    assert_eq!(
        result["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["accepted"] == true)
            .count(),
        1
    );
    assert!(!f.root.path().join("store/owner.sock").exists());
    let offline = f
        .command()
        .args(["goal", "stop", &id, "--store"])
        .arg(f.root.path().join("store"))
        .args([
            "--request-key",
            "offline",
            "--revision",
            &result["revision"].as_u64().unwrap().to_string(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(offline.status.code(), Some(6));
}
#[test]
fn clean_os_signals_pause_then_resume_without_reexecuting_the_worker() {
    for (signal, code) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let f = Fixture::new("goal_success", Some("work"));
        let child = f.start();
        f.wait_started(2);
        assert_eq!(unsafe { libc::kill(child.pid() as i32, signal) }, 0);
        let stopped = snapshot(&child.finish(), code);
        assert_eq!(stopped["status"], "stopped");
        assert_eq!(stopped["budget"]["runs_used"], 2);
        let id = stopped["goal_id"].as_str().unwrap();
        let resumed = f
            .command()
            .args(["goal", "resume", id, "--config"])
            .arg(&f.host)
            .args([
                "--request-key",
                "resume-once",
                "--revision",
                &stopped["revision"].as_u64().unwrap().to_string(),
                "--json",
            ])
            .output()
            .unwrap();
        let result = snapshot(&resumed, 0);
        assert_eq!(result["outcome"], "achieved");
        assert_eq!(result["budget"]["runs_used"], 3);
        assert_eq!(f.started(), 3);
        let queried = f
            .command()
            .args(["goal", "trace", id, "--store"])
            .arg(f.root.path().join("store"))
            .args(["--after", "0", "--limit", "1000", "--json"])
            .output()
            .unwrap();
        assert_eq!(queried.status.code(), Some(0));
        let trace: Value = serde_json::from_slice(&queried.stdout).unwrap();
        assert!(
            trace["data"]["sources"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|source| source["run_id"] == id)
                .all(|source| source["terminal"] == true)
        );
    }
}

#[test]
fn killed_hosts_reconcile_started_runs_without_replaying_any_business_stage() {
    for (phase, count) in [("planner", 1), ("work", 2), ("final", 3)] {
        let f = Fixture::new("goal_success", Some(phase));
        let child = f.start();
        let id = child.goal_id.clone();
        f.wait_started(count);
        assert_eq!(unsafe { libc::kill(child.pid() as i32, libc::SIGKILL) }, 0);
        assert!(!child.finish().status.success());
        let before = f.query("get", &id);
        assert_ne!(before["outcome"], "achieved");
        let recovered = snapshot(&f.run(), 8);
        assert_eq!(recovered["status"], "blocked", "{phase}: {recovered}");
        assert!(
            recovered["blockers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["reason"] == "interrupted")
        );
        assert_eq!(
            f.started(),
            count,
            "recovery must never execute a started Run again"
        );
        if phase == "final" {
            let report = recovered["nodes"]["review"]["outputs"]["report"]["id"]
                .as_str()
                .unwrap();
            assert!(
                !report.is_empty(),
                "already accepted report must survive the crash"
            );
        }
        let resumed = f
            .command()
            .args(["goal", "resume", &id, "--config"])
            .arg(&f.host)
            .args([
                "--request-key",
                "unsafe-replay",
                "--revision",
                &recovered["revision"].as_u64().unwrap().to_string(),
                "--json",
            ])
            .output()
            .unwrap();
        assert_eq!(resumed.status.code(), Some(2));
        assert_eq!(f.started(), count);
    }
}

#[test]
fn cli_waits_for_signal_and_preserves_wait_identity_across_foreground_restart() {
    let f = Fixture::new("goal_need_input", Some("work"));
    let child = f.start();
    let id = child.goal_id.clone();
    let waiting = f.wait_status(&id, "waiting");
    assert_eq!(f.started(), 1);
    assert_eq!(waiting["progress"]["goal_occupied_slots"], 0);
    let wait = waiting["waiting"][0].clone();
    let paused = f
        .command()
        .args(["goal", "stop", &id, "--store"])
        .arg(f.root.path().join("store"))
        .args([
            "--request-key",
            "pause-wait",
            "--revision",
            &waiting["revision"].as_u64().unwrap().to_string(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(paused.status.code(), Some(0));
    let stopped = snapshot(&child.finish(), 9);
    assert_eq!(stopped["status"], "stopped");
    assert_eq!(stopped["waiting"][0], wait);
    assert!(!f.root.path().join("store/owner.sock").exists());
    let stopped = f.query("get", &id);
    let resumed = f.start_resume(&id, stopped["revision"].as_u64().unwrap());
    let waiting = f.wait_status(&id, "waiting");
    assert_eq!(waiting["waiting"][0], wait);
    let input = f.root.path().join("signal.json");
    fs::write(&input, serde_json::to_vec(&json!({"protocol_version":1,"wait_id":wait["wait_id"],"nonce":wait["nonce"],"payload":"侧重风险与恢复边界","request_key":"answer-once","expected_revision":waiting["revision"]})).unwrap()).unwrap();
    let signal = || {
        f.command()
            .args(["goal", "signal", &id, "--store"])
            .arg(f.root.path().join("store"))
            .arg("--request")
            .arg(&input)
            .arg("--json")
            .output()
            .unwrap()
    };
    let first = signal();
    assert_eq!(
        first.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let second = signal();
    assert_eq!(second.status.code(), Some(0));
    let a: Value = serde_json::from_slice(&first.stdout).unwrap();
    let b: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(a["data"]["receipt"], b["data"]["receipt"]);
    let result = snapshot(&resumed.finish(), 0);
    assert_eq!(result["outcome"], "achieved");
    assert!(result["waiting"].as_array().unwrap().is_empty());
    assert_eq!(
        f.started(),
        4,
        "two planners, one worker, one final check; no duplicate signal run"
    );
}
