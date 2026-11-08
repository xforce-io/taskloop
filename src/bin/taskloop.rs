use serde::Serialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    process::ExitCode,
};
use taskloop::*;
#[path = "../cli/config.rs"]
mod config;

const HELP: &str = "用法：\n  taskloop goal run --config HOST.json --spec GOAL.json [--json]\n  taskloop goal resume GOAL_ID --config HOST.json --request-key KEY --revision REV [--json]\n  taskloop goal get|plan|budget GOAL_ID --store DIR [--json]\n  taskloop goal signal GOAL_ID --store DIR --request SIGNAL.json [--json]\n  taskloop goal cancel|stop GOAL_ID --store DIR --request-key KEY --revision REV [--json]\n  taskloop goal artifact ARTIFACT_ID --store DIR --output FILE [--overwrite] [--json]\n  taskloop goal trace GOAL_ID --store DIR --after CURSOR --limit N [--json]\n\nrun/resume在前台持有宿主所有权；其他查询只读，在线命令需要现有宿主。";
fn invalid(path: &str, message: &str) -> Error {
    Error::new("input_invalid", path, message)
}
struct Args {
    command: String,
    id: Option<String>,
    values: BTreeMap<String, String>,
    flags: BTreeSet<String>,
}
impl Args {
    fn parse(args: &[String]) -> Result<Self> {
        if args.first().map(String::as_str) != Some("goal") {
            return Err(invalid("/command", "expected taskloop goal"));
        }
        let command = args
            .get(1)
            .ok_or_else(|| invalid("/command", "Goal command missing"))?
            .clone();
        let allowed: &[&str] = match command.as_str() {
            "run" => &["config", "spec", "json"],
            "resume" => &["config", "request-key", "revision", "json"],
            "get" | "plan" | "budget" => &["store", "json"],
            "signal" => &["store", "request", "json"],
            "cancel" | "stop" => &["store", "request-key", "revision", "json"],
            "artifact" => &["store", "output", "overwrite", "json"],
            "trace" => &["store", "after", "limit", "json"],
            _ => return Err(invalid("/command", "unknown Goal command; use --help")),
        };
        let mut index = 2;
        let id = if command != "run" {
            let id = args
                .get(index)
                .filter(|s| !s.starts_with("--"))
                .ok_or_else(|| invalid("/id", "opaque Goal/Artifact ID missing"))?
                .clone();
            index += 1;
            Some(id)
        } else {
            None
        };
        let mut result = Self {
            command,
            id,
            values: BTreeMap::new(),
            flags: BTreeSet::new(),
        };
        while index < args.len() {
            let flag = args[index]
                .strip_prefix("--")
                .ok_or_else(|| invalid("/flags", "unexpected positional argument"))?;
            if !allowed.contains(&flag)
                || result.flags.contains(flag)
                || result.values.contains_key(flag)
            {
                return Err(invalid("/flags", "unknown or duplicate flag"));
            }
            if ["json", "overwrite"].contains(&flag) {
                result.flags.insert(flag.into());
                index += 1;
            } else {
                let value = args
                    .get(index + 1)
                    .filter(|s| !s.starts_with("--"))
                    .ok_or_else(|| invalid("/flags", "flag value missing"))?;
                result.values.insert(flag.into(), value.clone());
                index += 2;
            }
        }
        for flag in allowed
            .iter()
            .filter(|flag| !["json", "overwrite"].contains(flag))
        {
            result.value(flag)?;
        }
        Ok(result)
    }
    fn value(&self, key: &str) -> Result<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| invalid("/flags", &format!("--{key} is required")))
    }
    fn number(&self, key: &str) -> Result<u64> {
        self.value(key)?
            .parse()
            .map_err(|_| invalid("/flags", "expected an unsigned integer"))
    }
    fn id(&self) -> &str {
        self.id.as_deref().unwrap_or("")
    }
    fn json(&self) -> bool {
        self.flags.contains("json")
    }
}
fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > max {
        return Err(invalid("/file", "input file exceeds its supported bound"));
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(invalid("/file", "input file exceeds its supported bound"));
    }
    Ok(bytes)
}
fn print_json(value: &impl Serialize) -> Result<()> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, value)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}
fn event(kind: &str, id: Option<&str>, revision: Option<u64>, data: impl Serialize) -> Result<()> {
    print_json(
        &json!({"protocol_version":1,"type":kind,"goal_id":id,"revision":revision,"data":data}),
    )
}
fn human(snapshot: &GoalSnapshot) {
    eprintln!(
        "目标 {}：{:?} / {:?}，revision={}，结果={:?}",
        snapshot.goal_id, snapshot.status, snapshot.stage, snapshot.revision, snapshot.outcome
    );
    let completed = snapshot
        .nodes
        .values()
        .filter(|node| node.status == GoalNodeStatus::Succeeded)
        .count();
    eprintln!("节点完成：{}/{}", completed, snapshot.nodes.len());
    if let Some(reason) = &snapshot.progress.queue_reason {
        eprintln!(
            "排队：{}；待准入={}，目标占槽={}，宿主占槽={}，未知停止占槽={}",
            reason,
            snapshot.progress.pending_requests,
            snapshot.progress.goal_occupied_slots,
            snapshot.progress.host_occupied_slots,
            snapshot.progress.host_stop_unknown_slots
        );
    }
    for artifact in snapshot
        .artifacts
        .iter()
        .filter(|artifact| artifact.accepted)
    {
        eprintln!(
            "产物 {}：{}，sha256={}",
            artifact.artifact.slot, artifact.artifact.id, artifact.artifact.sha256
        );
    }
    for run in snapshot.runs.values() {
        if let Some(failure) = &run.plan_validation_failure {
            for issue in failure.issues.iter().take(10) {
                eprintln!(
                    "计划拒绝 {}：{:?} {}：{}",
                    run.run_id, issue.category, issue.path, issue.message
                );
            }
            if failure.issues.len() > 10 {
                eprintln!(
                    "其余 {} 项原因可用 goal get --json 查看。",
                    failure.issues.len() - 10
                );
            }
        }
    }
    for waiting in &snapshot.waiting {
        if !waiting.consumed {
            eprintln!(
                "等待补充：{}；wait_id={}，nonce={}，期限={}。使用 goal signal --request 提交。",
                waiting.reason, waiting.wait_id, waiting.nonce, waiting.expires_at_utc_ms
            );
        }
    }
    if let Some(error) = &snapshot.error {
        eprintln!("{}：{}", error.code, error.message);
    }
}
fn outcome_code(snapshot: &GoalSnapshot, signal: Option<u8>) -> u8 {
    match snapshot.status {
        GoalStatus::Blocked => 8,
        GoalStatus::Stopped => signal.unwrap_or(9),
        GoalStatus::Finished => match snapshot.outcome {
            Some(GoalOutcome::Achieved) => 0,
            Some(GoalOutcome::NotMet) => 4,
            Some(GoalOutcome::Cancelled) => 7,
            Some(GoalOutcome::Failed)
                if snapshot
                    .error
                    .as_ref()
                    .is_some_and(|e| e.code == "check_unavailable")
                    || snapshot
                        .terminal_intent
                        .as_ref()
                        .is_some_and(|i| i.reason == "check_unavailable") =>
            {
                5
            }
            _ => 3,
        },
        _ => 9,
    }
}
async fn foreground(args: &Args) -> Result<u8> {
    let host: config::HostConfig = serde_json::from_value(parse_unique(&read_bounded(
        Path::new(args.value("config")?),
        1_048_576,
    )?)?)?;
    let mut interrupts = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminations =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let (mut driver, store, caller) = host.open().await?;
    let observer = GoalClient::open(&store, caller)?;
    let mut replayed = false;
    let id = if args.command == "run" {
        let request: GoalRequest = serde_json::from_value(parse_unique(&read_bounded(
            Path::new(args.value("spec")?),
            4_194_304,
        )?)?)?;
        let receipt = driver.submit(request).await?;
        replayed = receipt.replayed;
        if args.json() {
            event(
                "goal.admitted",
                Some(&receipt.goal_id),
                Some(receipt.revision),
                &receipt,
            )?;
        }
        receipt.goal_id
    } else {
        let receipt = driver
            .resume(
                args.id(),
                args.value("request-key")?,
                args.number("revision")?,
            )
            .await?;
        if args.json() {
            event(
                "command.receipt",
                Some(args.id()),
                Some(receipt.applied_revision),
                &receipt,
            )?;
        }
        args.id().to_owned()
    };
    let mut last_revision = None;
    let mut last_progress = None;
    let mut exit_signal = None;
    let mut stop_deadline = None;
    let snapshot = loop {
        let snapshot = driver.get().await?;
        if last_revision != Some(snapshot.revision)
            || last_progress.as_ref() != Some(&snapshot.progress)
        {
            if args.json() {
                event(
                    "progress",
                    Some(&id),
                    Some(snapshot.revision),
                    json!({"status":snapshot.status,"stage":snapshot.stage,"outcome":snapshot.outcome,"waiting":snapshot.waiting,"progress":snapshot.progress,"nodes":snapshot.nodes.iter().map(|(id,node)| (id.clone(), node.status)).collect::<std::collections::BTreeMap<_,_>>(),"budget":{"runs_used":snapshot.budget.runs_used,"used":snapshot.budget.used}}),
                )?;
            } else {
                human(&snapshot);
            }
            last_revision = Some(snapshot.revision);
            last_progress = Some(snapshot.progress.clone());
        }
        if matches!(
            snapshot.status,
            GoalStatus::Finished | GoalStatus::Stopped | GoalStatus::Blocked
        ) || replayed
        {
            break snapshot;
        }
        if stop_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            break snapshot;
        }
        let signal = tokio::select! {
            _ = interrupts.recv() => Some(130), _ = terminations.recv() => Some(143),
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => None,
        };
        if let Some(signal) = signal {
            exit_signal.get_or_insert(signal);
            stop_deadline
                .get_or_insert(tokio::time::Instant::now() + std::time::Duration::from_secs(15));
            for attempt in 0..4 {
                match driver
                    .pause(&format!(
                        "os-signal-{signal}-{attempt}-{}",
                        uuid::Uuid::new_v4()
                    ))
                    .await
                {
                    Ok(receipt) => {
                        if args.json() {
                            event(
                                "command.receipt",
                                Some(&id),
                                Some(receipt.applied_revision),
                                receipt,
                            )?;
                        }
                        break;
                    }
                    Err(error) if error.code == "revision_conflict" => continue,
                    Err(error) if error.code == "not_resumable" => break,
                    Err(error) => return Err(error),
                }
            }
        }
    };
    let pending = !matches!(snapshot.status, GoalStatus::Finished | GoalStatus::Stopped);
    let remaining = if pending {
        stop_deadline
            .map(|d| d.saturating_duration_since(tokio::time::Instant::now()))
            .unwrap_or(std::time::Duration::from_secs(15))
    } else {
        std::time::Duration::from_secs(15)
    };
    let shutdown = tokio::time::timeout(remaining, driver.shutdown()).await;
    let mut code = if pending {
        8
    } else {
        outcome_code(&snapshot, exit_signal)
    };
    if replayed && !matches!(snapshot.status, GoalStatus::Finished | GoalStatus::Blocked) {
        code = 9;
    }
    if !matches!(shutdown, Ok(Ok(()))) {
        let error = match shutdown {
            Ok(Err(error)) => error,
            _ => Error::new(
                "stop_unconfirmed",
                "/shutdown",
                "foreground cleanup window expired; pending native state remains persisted",
            ),
        };
        if !error.code.starts_with("trace_cleanup") {
            code = 8;
        }
        if args.json() {
            event("error", Some(&id), Some(snapshot.revision), &error)?;
        } else {
            eprintln!("{error}");
        }
    }
    let final_snapshot = observer.get(&id).unwrap_or(snapshot);
    if args.json() {
        event(
            "goal.snapshot",
            Some(&id),
            Some(final_snapshot.revision),
            &final_snapshot,
        )?;
    } else {
        human(&final_snapshot);
    }
    Ok(code)
}
fn export_artifact(data: &ArtifactData, output: &Path, overwrite: bool) -> Result<&'static str> {
    if output.exists() {
        if let Ok(existing) = read_bounded(output, 67_108_864)
            && existing == *data.bytes
        {
            return Ok("already_exported");
        }
        if !overwrite {
            return Err(invalid(
                "/output",
                "output exists with different content; use --overwrite explicitly",
            ));
        }
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(".taskloop-export-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&data.bytes)?;
        file.sync_all()?;
        if overwrite {
            std::fs::rename(&temp, output)?;
        } else {
            std::fs::hard_link(&temp, output)?;
        }
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temp);
    result?;
    Ok("exported")
}
async fn run(args: Args) -> Result<u8> {
    if ["run", "resume"].contains(&args.command.as_str()) {
        return foreground(&args).await;
    }
    let client = GoalClient::open(
        Path::new(args.value("store")?),
        config::local_caller(vec![]),
    )?;
    let data = match args.command.as_str() {
        "get" => {
            let snapshot = client.get(args.id())?;
            if !args.json() {
                human(&snapshot);
                return Ok(0);
            }
            serde_json::to_value(snapshot)?
        }
        "plan" => serde_json::to_value(client.plan(args.id())?)?,
        "budget" => serde_json::to_value(client.budgets(args.id())?)?,
        "trace" => serde_json::to_value(
            client.trace(
                args.id(),
                TraceQuery {
                    run_id: None,
                    after: args.number("after")?,
                    limit: u16::try_from(args.number("limit")?)
                        .map_err(|_| invalid("/limit", "page limit must be in 1..=1000"))?,
                },
            )?,
        )?,
        "signal" => {
            let request = serde_json::from_value(parse_unique(&read_bounded(
                Path::new(args.value("request")?),
                4_194_304,
            )?)?)?;
            serde_json::to_value(client.signal(args.id(), request).await?)?
        }
        "stop" => serde_json::to_value(
            client
                .stop(
                    args.id(),
                    args.value("request-key")?,
                    args.number("revision")?,
                )
                .await?,
        )?,
        "cancel" => serde_json::to_value(
            client
                .cancel(
                    args.id(),
                    args.value("request-key")?,
                    args.number("revision")?,
                )
                .await?,
        )?,
        "artifact" => {
            let artifact = client.read_artifact(args.id())?;
            let output = Path::new(args.value("output")?);
            let status = export_artifact(&artifact, output, args.flags.contains("overwrite"))?;
            json!({"artifact":artifact.reference,"output":output,"status":status,"bytes":artifact.bytes.len()})
        }
        _ => return Err(invalid("/command", "unknown Goal command")),
    };
    if args.json() {
        print_json(&json!({"protocol_version":1,"ok":true,"data":data}))?;
    } else {
        println!("{}", serde_json::to_string_pretty(&data)?);
    }
    Ok(0)
}
#[tokio::main]
async fn main() -> ExitCode {
    let args = std::env::args_os()
        .skip(1)
        .map(|s| {
            s.into_string()
                .map_err(|_| invalid("/arguments", "command arguments must be UTF-8"))
        })
        .collect::<Result<Vec<_>>>();
    let args = match args {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let machine = args.iter().any(|arg| arg == "--json");
    let foreground = args
        .get(1)
        .is_some_and(|arg| arg == "run" || arg == "resume");
    let result = match Args::parse(&args) {
        Ok(args) => run(args).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            let code = if error.code == "host_not_running" {
                6
            } else if error.code == "clock_untrusted" || error.code == "stop_unconfirmed" {
                8
            } else {
                2
            };
            if machine {
                let _ = if foreground {
                    event("error", None, None, &error)
                } else {
                    print_json(&json!({"protocol_version":1,"ok":false,"error":error}))
                };
            } else {
                eprintln!("{error}");
            }
            ExitCode::from(code)
        }
    }
}
