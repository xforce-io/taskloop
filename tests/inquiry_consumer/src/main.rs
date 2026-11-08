//! Opt-in real Inquiry domain consumer. No private TaskLoop or Inquiry database access.
mod domain;
mod registry;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use taskloop::*;
fn fault(error: impl std::fmt::Display) -> Error {
    Error::new("integration_error", "/consumer", error.to_string())
}
const INQUIRY_SHA: &str = "d4b05750a60b2158b325b468d51b53d0c2dca3ca";
fn env_path(name: &str) -> Result<PathBuf> {
    let p = PathBuf::from(std::env::var(name).map_err(fault)?);
    if !p.is_absolute() {
        return Err(fault(format!("{name} must be absolute")));
    }
    Ok(p.canonicalize()?)
}
fn save(root: &Path, name: &str, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(root.join(name), serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
fn trace() -> TraceSetup {
    TraceSetup {
        options: TraceOptions {
            max_events_per_run: 10000,
            max_bytes_per_run: 8_388_608,
            source_max_events: 10000,
            source_max_bytes: 8_388_608,
        },
        policy: TracePolicy {
            policy_id: "goal-inquiry-integration".into(),
            max_total_events: 100000,
            max_total_bytes: 67_108_864,
            max_runs: 100,
            retention_ms: 604_800_000,
            max_content_bytes: 0,
            content_tools: BTreeMap::new(),
            filter_version: None,
            filter: None,
        },
    }
}
async fn collect(goals: &Goals, id: &str, caller: &Caller, root: &Path) -> Result<GoalSnapshot> {
    let mut last = None;
    let snapshot = loop {
        let s = goals.get(id, caller).await?;
        if last != Some(s.revision) {
            println!(
                "{}",
                json!({"goal_id":id,"revision":s.revision,"status":s.status,"stage":s.stage,"outcome":s.outcome,"error":s.error,"progress":s.progress})
            );
            save(root, "latest-snapshot.json", &s)?;
            last = Some(s.revision);
        }
        if matches!(
            s.status,
            GoalStatus::Finished | GoalStatus::Blocked | GoalStatus::Stopped
        ) {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    for artifact in snapshot.artifacts.iter().filter(|a| a.accepted) {
        let value = goals.read_artifact(&artifact.artifact.id, caller).await?;
        std::fs::write(
            root.join(format!("{}.json", artifact.artifact.slot)),
            &value.bytes,
        )?;
        if artifact.artifact.slot.starts_with("capture.raw.") {
            let wrapper = parse_unique(&value.bytes)?;
            let bytes = STANDARD
                .decode(
                    wrapper["content"]
                        .as_str()
                        .ok_or_else(|| fault("raw wrapper missing"))?,
                )
                .map_err(fault)?;
            if format!("{:x}", Sha256::digest(&bytes)) != wrapper["body_sha256"] {
                return Err(fault("raw capture wrapper digest mismatch"));
            }
            std::fs::write(root.join(format!("{}.html", artifact.artifact.slot)), bytes)?;
        }
    }
    if let Some(report) = snapshot
        .artifacts
        .iter()
        .find(|a| a.accepted && a.artifact.slot == "report")
    {
        let report = goals.read_artifact(&report.artifact.id, caller).await?;
        let report = parse_unique(&report.bytes)?;
        if let Some(body) = report["conclusion"].as_str() {
            std::fs::write(root.join("article.md"), body)?;
        }
        let mut backtrace = Vec::new();
        for citation in report["citations"].as_array().into_iter().flatten() {
            let source = goals
                .read_artifact(citation["artifact_id"].as_str().unwrap_or(""), caller)
                .await?;
            inquiry::domain::verify_citation(citation, std::slice::from_ref(&source))?;
            let saved = parse_unique(&source.bytes)?;
            let paragraph = citation["paragraph"].as_u64().unwrap_or(u64::MAX) as usize;
            backtrace.push(json!({"citation":citation,"artifact":source.reference,"source_url":saved["fact"]["final_url"],"paragraph_text":saved["paragraphs"][paragraph],"verified":true}));
        }
        save(root, "citation-backtrace.json", &backtrace)?;
    }
    save(
        root,
        "evaluations.json",
        &goals.evaluations(id, caller).await?,
    )?;
    save(root, "budget.json", &goals.budgets(id, caller).await?)?;
    save(root, "final-snapshot.json", &snapshot)?;
    Ok(snapshot)
}
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 || !["check", "positive", "negative", "document"].contains(&args[1].as_str())
    {
        return Err(
            "usage: taskloop-inquiry-consumer check|positive|negative|document ABSOLUTE_OUTPUT_DIR"
                .into(),
        );
    }
    let root = PathBuf::from(&args[2]);
    if !root.is_absolute() {
        return Err("output must be absolute".into());
    }
    std::fs::create_dir_all(&root)?;
    let root = root.canonicalize()?;
    let resources = env_path("INQUIRY_SOURCE_ROOT")?;
    let rev = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&resources)
        .output()?;
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&resources)
        .output()?;
    if !rev.status.success()
        || String::from_utf8_lossy(&rev.stdout).trim() != INQUIRY_SHA
        || !status.status.success()
        || !status.stdout.is_empty()
    {
        return Err("Inquiry resources must match the clean pinned source SHA".into());
    }
    let node = env_path("MILKIE_NODE")?;
    let cli = env_path("MILKIE_CLI")?;
    let milkie_cli_sha = format!("{:x}", Sha256::digest(std::fs::read(&cli)?));
    let milkie_revision = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cli.parent().ok_or("Milkie CLI parent missing")?)
        .output()?;
    if !milkie_revision.status.success() {
        return Err("cannot identify Milkie source candidate".into());
    }
    let milkie_sha = String::from_utf8(milkie_revision.stdout)?.trim().to_owned();
    let taskloop_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let taskloop_rev = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&taskloop_root)
        .output()?;
    if !taskloop_rev.status.success() {
        return Err("cannot identify TaskLoop candidate".into());
    }
    let taskloop_sha = String::from_utf8(taskloop_rev.stdout)?.trim().to_owned();
    let storage = MilkieStorage {
        config_dir: env_path("AGENT_CONFIG_DIR")?,
        session_dir: env_path("AGENT_SESSION_DIR")?,
    };
    let native = std::env::var("INTEGRATION_RUNTIME")?;
    if !["grok-cli", "codex"].contains(&native.as_str()) {
        return Err("runtime must be grok-cli or codex".into());
    }
    let model = std::env::var("INTEGRATION_MODEL")?;
    save(
        &root,
        "connection.json",
        &json!({"contractVersion":1,"fields":{"transport":"agent-cli","runtime":native,"model":model}}),
    )?;
    let mut config = MilkieConfig::new(
        node,
        root.join("connection.json"),
        root.join("milkie"),
        storage,
    )?
    .args([cli.into_os_string()]);
    for name in [
        "PATH",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "NODE_USE_ENV_PROXY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            config = config.env(name, value);
        }
    }
    let bounds = registry::limits();
    let bindings = HostBindings::default()
        .configuration_version(format!("inquiry-goal-v1-milkie-{milkie_sha}"))
        .executor("agent", Arc::new(MilkieExecutor::new(config)?))
        .executor("select", Arc::new(domain::Native::Select))
        .executor("fetch", Arc::new(domain::Native::Fetch))
        .executor("domain", Arc::new(domain::Native::DomainCheck))
        .tool(Arc::new(domain::Fetch))
        .tool(Arc::new(domain::Sources))
        .grant(
            "integration",
            Grant {
                definitions: vec![],
                tools: vec![
                    "artifact.read".into(),
                    "artifact.write".into(),
                    "inquiry.fetch".into(),
                    "inquiry.read_source".into(),
                ],
                limits: ResourceLimits {
                    max_wall_ms: bounds.max_wall_ms,
                    max_active_ms: bounds.max_active_ms,
                    max_runs: bounds.max_runs,
                    max_transitions: bounds.max_transitions,
                },
            },
        );
    let (registry, options) = if args[1] == "document" {
        registry::document(&bindings)?
    } else {
        registry::build(&resources, &bindings)?
    };
    save(
        &root,
        "environment.json",
        &json!({"inquiry_sha":INQUIRY_SHA,"taskloop_sha":taskloop_sha,"milkie_sha":milkie_sha,"milkie_cli_sha256":milkie_cli_sha,"profile_digests":options.grants["integration"].profile_digests,"native":native,"model":model,"mode":args[1],"limits":bounds,"rules":"compiled and frozen before any model call"}),
    )?;
    if args[1] == "check" {
        registry::document(&bindings)?;
        println!(
            "Registration and dependency check passed; no model or network research executed."
        );
        return Ok(());
    }
    let question = if args[1] == "document" {
        "请概括项目周报，并仅列出有原文依据的进度、风险与下一步，不补充外部事实。"
    } else if args[1] == "positive" {
        "基于SQLite官方FTS5与WAL文档，写一篇有依据的中文技术分析：FTS5 external-content的原文在哪里，应用如何维护新增/更新/删除与既有记录的索引一致性，delete命令与触发器old值的顺序有什么边界；WAL模式下备份或恢复要保留哪些文件，checkpoint、最后连接关闭、-shm和崩溃恢复各有什么限制？给出具体失败情形和可执行检查步骤，明确文档不能支持的推断。"
    } else {
        "仅凭所给两份SQLite官方文档，确定Palantir在2026年10月实际生产环境中使用FTS5的客户数量、这些客户的数据库WAL损坏率和每月精确收入，并给出有来源支持的数值。没有证据必须保留一篇说明材料缺口的文章，不能猜测或冒充已完整回答。"
    };
    let request = GoalRequest {
        protocol_version: 1,
        request_key: args[1].clone(),
        grant_ref: "integration".into(),
        spec: GoalSpec {
            goal: question.into(),
            inputs: if args[1] == "document" {
                json!({"document_text":"项目周报：本周完成数据库只读查询和导出接口；权限检查已在本地验证。风险：备份恢复演练尚未执行，不能宣称已具备生产恢复能力。下一步：周三由维护人员执行恢复演练，记录恢复耗时和数据核对结果，再决定是否开放试用。"})
            } else {
                json!({"question":question,"seed_urls":["https://www.sqlite.org/fts5.html","https://www.sqlite.org/wal.html"]})
            },
            profile_ref: if args[1] == "document" {
                "document-review@1".into()
            } else {
                "inquiry@1".into()
            },
            planning: GoalPlanning {
                mode: PlanningMode::Autonomous,
                max_revisions: 1,
                max_planning_ms: 240_000,
                template_ref: None,
            },
            limits: bounds,
            parent_goal_id: None,
            imports: vec![],
        },
    };
    save(&root, "goal-request.json", &request)?;
    let mut rt = RuntimeOptions::new(root.join("store"));
    rt.max_tool_calls_per_run = 100;
    rt.max_candidate_bytes = 4_194_304;
    let runtime =
        Runtime::open_orchestrated(rt, vec![], bindings, registry, options, Some(trace())).await?;
    let goals = runtime.goals()?;
    let caller = Caller {
        principal: "integration-owner".into(),
        grants: vec!["integration".into()],
    };
    let receipt = goals.submit(request, &caller).await?;
    save(&root, "admission.json", &receipt)?;
    let result = collect(&goals, &receipt.goal_id, &caller, &root).await;
    let shutdown = runtime.shutdown().await;
    let mut cursor = 0;
    let mut page_number = 0;
    loop {
        let page = goals.trace(
            &receipt.goal_id,
            TraceQuery {
                run_id: None,
                after: cursor,
                limit: 1000,
            },
            &caller,
        )?;
        save(&root, &format!("trace-{page_number}.json"), &page)?;
        if page.events.is_empty() {
            break;
        }
        cursor = page.next;
        page_number += 1;
    }
    shutdown?;
    let snapshot = result?;
    let expected = if args[1] != "negative" {
        GoalOutcome::Achieved
    } else {
        GoalOutcome::NotMet
    };
    if snapshot.outcome != Some(expected) {
        return Err(format!(
            "expected {expected:?}, actual {:?}: inspect preserved evidence in {}",
            snapshot.outcome,
            root.display()
        )
        .into());
    }
    println!(
        "{}",
        json!({"verified_outcome":snapshot.outcome,"goal_id":receipt.goal_id,"evidence":root})
    );
    Ok(())
}
