use crate::{
    json::digest,
    store::{CommandReceipt, Commit, MAX_BLOB_BYTES, Row, Store},
    *,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{sync::Notify, task::JoinHandle};
use tokio_util::sync::CancellationToken;

pub struct RuntimeOptions {
    pub store_dir: PathBuf,
    pub max_admitted_instances: usize,
    pub max_tool_calls_per_run: u64,
    /// Total candidate bytes per Run; must be in 1..=64 MiB.
    pub max_candidate_bytes: u64,
    pub entry_budgets: BTreeMap<String, crate::RunBudgetPolicy>,
}
impl RuntimeOptions {
    pub fn new(store: impl Into<PathBuf>) -> Self {
        Self {
            store_dir: store.into(),
            max_admitted_instances: 8,
            max_tool_calls_per_run: 40,
            max_candidate_bytes: MAX_BLOB_BYTES,
            entry_budgets: BTreeMap::new(),
        }
    }
}
pub struct Runtime {
    pub(crate) inner: Arc<Inner>,
    task: Mutex<Option<JoinHandle<()>>>,
    trace_task: Mutex<Option<JoinHandle<()>>>,
    shutdown_lock: tokio::sync::Mutex<()>,
    goals: Option<Arc<crate::orchestration::GoalCore>>,
}
pub struct TraceSetup {
    pub options: crate::TraceOptions,
    pub policy: crate::TracePolicy,
}
pub(crate) struct Inner {
    pub store: Store,
    pub trace: Option<Arc<crate::trace::TraceStore>>,
    pub bindings: HostBindings,
    pub options: RuntimeOptions,
    pub binding_fingerprint: String,
    pub stop: CancellationToken,
    pub notify: Notify,
    pub handles: Mutex<BTreeMap<String, CancellationToken>>,
    pub goal_run_handles: Mutex<BTreeMap<String, CancellationToken>>,
    pub live_budgets: Mutex<BTreeMap<String, Arc<crate::budget::BudgetState>>>,
    pub dispatch_scope: Mutex<DispatchScope>,
}
pub(crate) enum DispatchScope {
    All,
    Goal(Option<String>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ActiveRun {
    pub id: String,
    pub executor: String,
    pub epoch: u64,
    pub started: bool,
    pub started_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PhaseRecord {
    pub run_id: String,
    pub state: String,
    pub visit: u64,
    pub artifacts: Vec<ArtifactRef>,
    pub check: CheckResult,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CheckAudit {
    pub id: String,
    pub run_id: String,
    pub gate: String,
    pub candidate_digests: BTreeMap<String, String>,
    pub result: Option<CheckResult>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AttemptRecord {
    pub run_id: String,
    pub state: String,
    pub visit: u64,
    pub finished_ms: u64,
    pub outcome: ExecutionOutcome,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub definition_digest: String,
    pub binding_fingerprint: String,
    pub grant_ref: String,
    pub inputs: Value,
    pub limits: ResourceLimits,
    #[serde(default)]
    pub limit_sources: Option<crate::TaskLimitSources>,
    #[serde(default)]
    pub budgets: BTreeMap<String, crate::RunBudgetSnapshot>,
    pub status: Status,
    pub current_state: String,
    pub visits: BTreeMap<String, u64>,
    pub epoch: u64,
    pub transitions: u64,
    pub runs: u64,
    pub created_ms: u64,
    pub deadline_ms: u64,
    pub active_ms: u64,
    pub wait_deadline_ms: Option<u64>,
    pub stop_unknown: bool,
    pub active: Option<ActiveRun>,
    pub history: Vec<PhaseRecord>,
    #[serde(default)]
    pub candidate_files: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub checks: Vec<CheckAudit>,
    #[serde(default)]
    pub attempts: Vec<AttemptRecord>,
    pub receipts: Vec<ArtifactReceipt>,
    pub signals: Vec<SignalFact>,
    pub business_outcome: Option<String>,
    pub error: Option<Error>,
    pub progress: Value,
}
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub(crate) fn decode(row: &Row) -> Result<Record> {
    serde_json::from_value(row.body.clone()).map_err(Error::io)
}
pub fn validate(definition: &Definition, bindings: &HostBindings) -> Result<ValidationReport> {
    definition.verify()?;
    for (id, e) in &definition.bundle.entries {
        let executor = bindings.executors.get(&e.executor).ok_or_else(|| {
            Error::new(
                "binding_missing",
                format!("/entries/{id}/executor"),
                "executor unavailable",
            )
        })?;
        if !executor.capabilities().managed() {
            return Err(Error::new(
                "capability_denied",
                format!("/entries/{id}/executor"),
                "executor lacks bounded/isolated execution, collection or cancellation",
            ));
        }
        if !bindings.gates.contains_key(&e.gate) {
            return Err(Error::new(
                "binding_missing",
                format!("/entries/{id}/gate"),
                "gate unavailable",
            ));
        }
        for t in &e.tools {
            if !matches!(t.as_str(), "artifact.read" | "artifact.write")
                && !bindings.tools.contains_key(t)
            {
                return Err(Error::new(
                    "binding_missing",
                    format!("/entries/{id}/tools"),
                    format!("tool {t} unavailable"),
                ));
            }
        }
    }
    for id in ["artifact.read", "artifact.write"] {
        if bindings.tools.contains_key(id) {
            return Err(Error::new(
                "capability_denied",
                "/tools",
                "built-in artifact tools cannot be overridden",
            ));
        }
    }
    for (id, tool) in &bindings.tools {
        let spec = tool.spec();
        if spec.id != *id || crate::json::has_ref(&spec.input_schema) {
            return Err(Error::new(
                "definition_invalid",
                format!("/tools/{id}"),
                "tool id must match binding and schema must be self-contained",
            ));
        }
        jsonschema::validator_for(&spec.input_schema).map_err(|_| {
            Error::new(
                "definition_invalid",
                format!("/tools/{id}"),
                "invalid tool input schema",
            )
        })?;
    }
    Ok(definition.report())
}
impl Runtime {
    pub async fn open(
        options: RuntimeOptions,
        definitions: Vec<Definition>,
        bindings: HostBindings,
    ) -> Result<Self> {
        Self::open_impl(options, definitions, bindings, None, None, None, false).await
    }
    pub async fn open_with_trace(
        options: RuntimeOptions,
        definitions: Vec<Definition>,
        bindings: HostBindings,
        trace: crate::TraceOptions,
    ) -> Result<Self> {
        Self::open_impl(
            options,
            definitions,
            bindings,
            Some(trace),
            None,
            None,
            false,
        )
        .await
    }
    pub async fn open_with_trace_policy(
        options: RuntimeOptions,
        definitions: Vec<Definition>,
        bindings: HostBindings,
        trace: crate::TraceOptions,
        policy: crate::TracePolicy,
    ) -> Result<Self> {
        Self::open_impl(
            options,
            definitions,
            bindings,
            Some(trace),
            Some(policy),
            None,
            false,
        )
        .await
    }
    pub async fn open_orchestrated(
        options: RuntimeOptions,
        definitions: Vec<Definition>,
        bindings: HostBindings,
        registry: crate::OrchestrationRegistry,
        orchestration: crate::OrchestrationOptions,
        trace: Option<TraceSetup>,
    ) -> Result<Self> {
        Self::open_orchestrated_mode(
            options,
            definitions,
            bindings,
            registry,
            orchestration,
            trace,
            false,
        )
        .await
    }
    pub(crate) async fn open_orchestrated_mode(
        options: RuntimeOptions,
        definitions: Vec<Definition>,
        bindings: HostBindings,
        registry: crate::OrchestrationRegistry,
        orchestration: crate::OrchestrationOptions,
        trace: Option<TraceSetup>,
        selected_only: bool,
    ) -> Result<Self> {
        orchestration.validate()?;
        let frozen = registry.frozen()?;
        if frozen.bindings_digest != crate::orchestration::bindings_digest(&bindings)? {
            return Err(Error::new(
                "binding_mismatch",
                "/registry",
                "registry was frozen with different HostBindings",
            ));
        }
        let (trace_options, trace_policy) = match trace {
            Some(t) => (Some(t.options), Some(t.policy)),
            None => (None, None),
        };
        Self::open_impl(
            options,
            definitions,
            bindings,
            trace_options,
            trace_policy,
            Some((frozen, orchestration)),
            selected_only,
        )
        .await
    }
    pub fn goals(&self) -> Result<crate::Goals> {
        self.goals
            .as_ref()
            .map(|core| crate::Goals { core: core.clone() })
            .ok_or_else(|| {
                Error::new(
                    "unsupported_store_mode",
                    "/goals",
                    "open_orchestrated is required",
                )
            })
    }
    async fn open_impl(
        options: RuntimeOptions,
        definitions: Vec<Definition>,
        bindings: HostBindings,
        trace_options: Option<crate::TraceOptions>,
        trace_policy: Option<crate::TracePolicy>,
        orchestration: Option<(
            Arc<crate::orchestration::FrozenRegistry>,
            crate::OrchestrationOptions,
        )>,
        selected_only: bool,
    ) -> Result<Self> {
        if options.max_admitted_instances == 0 || options.max_admitted_instances > 1024 {
            return Err(Error::new(
                "input_invalid",
                "/max_admitted_instances",
                "expected 1..=1024",
            ));
        }
        if options.max_candidate_bytes == 0 || options.max_candidate_bytes > MAX_BLOB_BYTES {
            return Err(Error::new(
                "input_invalid",
                "/max_candidate_bytes",
                "expected 1..=67108864 bytes",
            ));
        }
        for (entry, policy) in &options.entry_budgets {
            nonempty(entry, "/entry_budgets")?;
            policy.validate(options.max_tool_calls_per_run, options.max_candidate_bytes)?;
        }
        for d in &definitions {
            validate(d, &bindings)?;
        }
        let fingerprint = digest(&serde_json::to_vec(
            &json!({"host_binary":digest(&std::fs::read(std::env::current_exe()?)?),"configuration_version":bindings.configuration_version,"entry_budgets":options.entry_budgets,"host_tool_limit":options.max_tool_calls_per_run,"host_candidate_limit":options.max_candidate_bytes,"tools":bindings.tools.iter().map(|(id,t)|(id,t.spec())).collect::<BTreeMap<_,_>>(),"executors":bindings.executors.iter().map(|(id,e)|(id,e.identity())).collect::<BTreeMap<_,_>>(),"executor_budget_limits":bindings.executors.iter().map(|(id,e)|(id,e.budget_limits())).collect::<BTreeMap<_,_>>(),"gates":bindings.gates.keys().collect::<Vec<_>>(),"grants":bindings.grants}),
        )?);
        let slots = orchestration
            .as_ref()
            .map(|(_, o)| o.limits.max_concurrent_runs)
            .unwrap_or(1);
        let store = if orchestration.is_some() {
            Store::open_mode(
                &options.store_dir,
                true,
                usize::try_from(slots).map_err(Error::io)?,
            )?
        } else {
            Store::open(&options.store_dir)?
        };
        for d in &definitions {
            store.save_definition(d)?;
        }
        let trace = trace_options
            .map(|t| match trace_policy {
                Some(p) => {
                    crate::trace::TraceStore::open_with_policy(&options.store_dir, t, Some(p))
                }
                None => crate::trace::TraceStore::open(&options.store_dir, t),
            })
            .transpose()?;
        let inner = Arc::new(Inner {
            store,
            trace,
            bindings,
            options,
            binding_fingerprint: fingerprint,
            stop: CancellationToken::new(),
            notify: Notify::new(),
            handles: Mutex::new(BTreeMap::new()),
            goal_run_handles: Mutex::new(BTreeMap::new()),
            live_budgets: Mutex::new(BTreeMap::new()),
            dispatch_scope: Mutex::new(if selected_only {
                DispatchScope::Goal(None)
            } else {
                DispatchScope::All
            }),
        });
        for row in inner.store.all()? {
            let mut r = decode(&row)?;
            if !r.status.terminal() && r.active.as_ref().is_some_and(|a| a.started) {
                if r.status != Status::Cancelling {
                    r.status = Status::Blocked;
                }
                r.stop_unknown = true;
                r.epoch += 1;
                r.error = Some(Error::new(
                    "interrupted",
                    "/execution",
                    "host restarted during an uncommitted execution; it will not be replayed",
                ));
                inner.save(&row, &r, false, None, &[])?;
            }
        }
        let goals = orchestration.map(|(registry, options)| {
            let binding = digest(&serde_json::to_vec(&json!({"host":inner.binding_fingerprint,"registry":registry.digest,"orchestration":options}))?);
            crate::orchestration::GoalCore::new(inner.clone(), registry, options, binding)
        }).transpose()?;
        if let Some(core) = &goals {
            core.recover()?;
        }
        let work = inner.clone();
        let coordination = goals.clone();
        let task = tokio::spawn(async move {
            shared_dispatcher(work, coordination).await;
        });
        let trace_task = if inner.trace.is_some() {
            let gc = inner.clone();
            Some(tokio::spawn(
                async move { crate::trace::cleanup_loop(gc).await },
            ))
        } else {
            None
        };
        Ok(Self {
            inner,
            task: Mutex::new(Some(task)),
            trace_task: Mutex::new(trace_task),
            shutdown_lock: tokio::sync::Mutex::new(()),
            goals,
        })
    }
    pub async fn start(&self, request: StartRequest, caller: &Caller) -> Result<String> {
        if self.inner.stop.is_cancelled() {
            return Err(Error::new(
                "runtime_closed",
                "/runtime",
                "runtime is closing; reopen before starting work",
            ));
        }
        nonempty(&request.request_key, "/request_key")?;
        let grant = self
            .inner
            .bindings
            .grants
            .get(&request.grant_ref)
            .filter(|_| caller.grants.contains(&request.grant_ref))
            .ok_or_else(|| Error::new("capability_denied", "/grant_ref", "grant not authorized"))?;
        if !grant.definitions.contains(&request.definition_digest) {
            return Err(Error::new(
                "capability_denied",
                "/definition_digest",
                "definition is not authorized",
            ));
        }
        let d = self.inner.store.definition(&request.definition_digest)?;
        validate(&d, &self.inner.bindings)?;
        d.validate_value(&d.bundle.flow.input_schema, &request.inputs)?;
        if serde_json::to_vec(&request.inputs)?.len() > 64 * 1024 {
            return Err(Error::new(
                "input_invalid",
                "/inputs",
                "initial inputs exceed 64 KiB",
            ));
        }
        if !d
            .bundle
            .entries
            .values()
            .all(|e| e.tools.iter().all(|t| grant.tools.contains(t)))
        {
            return Err(Error::new(
                "capability_denied",
                "/tools",
                "definition requires a tool outside this grant",
            ));
        }
        let fp = digest(&serde_json::to_vec(&request)?);
        let limits = ResourceLimits {
            max_active_ms: request.limits.max_active_ms.min(grant.limits.max_active_ms),
            max_wall_ms: request.limits.max_wall_ms.min(grant.limits.max_wall_ms),
            max_transitions: request
                .limits
                .max_transitions
                .min(grant.limits.max_transitions)
                .min(d.bundle.flow.limits.max_transitions),
            max_runs: request
                .limits
                .max_runs
                .min(grant.limits.max_runs)
                .min(d.bundle.flow.limits.max_runs),
        };
        if [
            limits.max_active_ms,
            limits.max_wall_ms,
            limits.max_transitions,
            limits.max_runs,
        ]
        .contains(&0)
        {
            return Err(Error::new(
                "input_invalid",
                "/limits",
                "limits must be positive",
            ));
        }
        if [limits.max_active_ms, limits.max_wall_ms].iter().any(|n| {
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_millis(*n))
                .is_none()
        }) {
            return Err(Error::new(
                "input_invalid",
                "/limits",
                "task deadline cannot be represented",
            ));
        }
        let time = now_ms();
        let mut r = Record {
            definition_digest: d.digest.clone(),
            binding_fingerprint: self.inner.binding_fingerprint.clone(),
            grant_ref: request.grant_ref,
            inputs: request.inputs,
            limits: limits.clone(),
            limit_sources: Some(crate::TaskLimitSources {
                requested: request.limits.clone(),
                grant: grant.limits.clone(),
                definition_max_runs: d.bundle.flow.limits.max_runs,
                definition_max_transitions: d.bundle.flow.limits.max_transitions,
            }),
            budgets: BTreeMap::new(),
            status: Status::Created,
            current_state: d.bundle.flow.initial.clone(),
            visits: BTreeMap::new(),
            epoch: 0,
            transitions: 0,
            runs: 0,
            created_ms: time,
            deadline_ms: time.saturating_add(limits.max_wall_ms),
            active_ms: 0,
            wait_deadline_ms: None,
            stop_unknown: false,
            active: None,
            history: vec![],
            candidate_files: BTreeMap::new(),
            checks: vec![],
            attempts: vec![],
            receipts: vec![],
            signals: vec![],
            business_outcome: None,
            error: None,
            progress: json!({"phase":"queued"}),
        };
        enter(&mut r, &d, &d.bundle.flow.initial, false)?;
        let id = format!("execution-{}", uuid::Uuid::new_v4());
        let (row, _) = self.inner.store.create(
            &id,
            &caller.principal,
            &request.request_key,
            &fp,
            &serde_json::to_value(r)?,
            self.inner.options.max_admitted_instances,
        )?;
        self.inner.notify.notify_one();
        Ok(row.id)
    }
    pub async fn get(&self, id: &str, caller: &Caller) -> Result<ExecutionSnapshot> {
        self.inner.authorized(id, caller)?;
        self.inner.expire(id)?;
        let row = self.inner.authorized(id, caller)?;
        self.inner.snapshot(&row)
    }
    /// Execution-policy facts, independently authorized from observation collection.
    pub fn budgets(&self, id: &str, caller: &Caller) -> Result<crate::BudgetView> {
        let row = self.inner.authorized(id, caller)?;
        let record = decode(&row)?;
        let live = self.inner.live_budgets.lock().map_err(Error::io)?;
        let runs = record
            .budgets
            .into_iter()
            .map(|(id, mut snapshot)| {
                if let Some(state) = live.get(&id) {
                    return state.snapshot();
                }
                if snapshot.execution_outcome.is_none() {
                    snapshot.hint.mode = "unknown".into();
                }
                snapshot
            })
            .collect();
        Ok(crate::BudgetView {
            task_limits: record.limits,
            task_limit_sources: record.limit_sources,
            runs,
        })
    }
    /// Authorized, versioned observation page. Does not drive workflow state.
    pub fn trace(
        &self,
        id: &str,
        query: crate::TraceQuery,
        caller: &Caller,
    ) -> Result<crate::TracePage> {
        self.inner.authorized(id, caller)?;
        match &self.inner.trace {
            Some(store) => store.read(id, &query),
            None => crate::trace::disabled(&query),
        }
    }
    /// Bounded JSON export; continue with the returned page cursor.
    pub fn export_trace(
        &self,
        id: &str,
        query: crate::TraceQuery,
        caller: &Caller,
    ) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&self.trace(id, query, caller)?)?)
    }
    pub fn read_trace_content(
        &self,
        id: &str,
        content_id: &str,
        caller: &Caller,
    ) -> Result<crate::TraceContent> {
        self.inner.authorized(id, caller)?;
        self.inner
            .trace
            .as_ref()
            .ok_or_else(|| Error::new("not_found", "/trace/content", "trace content not found"))?
            .read_content(id, content_id)
    }
    /// Host maintenance; no other principal's records or identifiers are returned.
    pub fn maintain_trace(&self) -> Result<usize> {
        self.inner.trace.as_ref().map_or(Ok(0), |s| s.maintain())
    }
    pub async fn signal(
        &self,
        id: &str,
        request: SignalRequest,
        caller: &Caller,
    ) -> Result<SignalReceipt> {
        if self.inner.stop.is_cancelled() {
            return Err(Error::new(
                "runtime_closed",
                "/runtime",
                "runtime is closing; reopen before submitting events",
            ));
        }
        nonempty(&request.request_key, "/request_key")?;
        let row = self.inner.authorized(id, caller)?;
        let fp = digest(&serde_json::to_vec(&request)?);
        if let Some(v) = self
            .inner
            .store
            .command(id, "signal", &request.request_key, &fp)?
        {
            return Ok(serde_json::from_value(v)?);
        }
        let mut r = decode(&row)?;
        if r.status == Status::Waiting && r.wait_deadline_ms.is_some_and(|x| now_ms() >= x) {
            self.inner.expire(id)?;
            return Err(Error::new(
                "signal_expired",
                "/signal",
                "waiting deadline has passed",
            ));
        }
        if row.revision != request.expected_revision {
            return Err(Error::new(
                "revision_conflict",
                "/revision",
                "refresh current snapshot",
            ));
        }
        let d = self.inner.store.definition(&r.definition_digest)?;
        let State::Wait { signals, .. } = &d.bundle.flow.states[&r.current_state] else {
            return Err(Error::new(
                "signal_not_allowed",
                "/signal",
                "not in a waiting state",
            ));
        };
        if r.status != Status::Waiting {
            return Err(Error::new(
                "signal_not_allowed",
                "/signal",
                "instance is not waiting",
            ));
        }
        let s = signals.get(&request.event_name).ok_or_else(|| {
            Error::new(
                "signal_not_allowed",
                "/event_name",
                "event is not declared here",
            )
        })?;
        d.validate_value(&s.schema, &request.payload)?;
        r.signals.push(SignalFact {
            id: format!("{id}:signal-{}", uuid::Uuid::new_v4()),
            event_name: request.event_name,
            payload: request.payload,
            received_at: chrono::Utc::now().to_rfc3339(),
        });
        let waiting_deadline = r.wait_deadline_ms;
        enter(&mut r, &d, &s.target, true)?;
        let receipt = SignalReceipt {
            request_key: request.request_key.clone(),
            revision: row.revision + 1,
            accepted: true,
        };
        let cmd = CommandReceipt {
            kind: "signal".into(),
            key: request.request_key,
            fingerprint: fp,
            response: serde_json::to_value(&receipt)?,
            not_after_ms: waiting_deadline,
        };
        let result = self.inner.save(&row, &r, true, Some(cmd), &[])?;
        self.inner.notify.notify_one();
        match result {
            Commit::Saved(_) => Ok(receipt),
            Commit::Replayed(v) => Ok(serde_json::from_value(v)?),
        }
    }
    pub async fn cancel(&self, id: &str, key: &str, caller: &Caller) -> Result<ExecutionSnapshot> {
        nonempty(key, "/request_key")?;
        let fp = digest(format!("cancel:{id}").as_bytes());
        let mut committed = false;
        for _ in 0..16 {
            let row = self.inner.authorized(id, caller)?;
            let mut r = decode(&row)?;
            if r.status.terminal() {
                return self.inner.snapshot(&row);
            }
            if self.inner.store.command(id, "cancel", key, &fp)?.is_some() {
                committed = true;
                break;
            }
            r.epoch += 1;
            r.error = None;
            if r.active.as_ref().is_some_and(|a| a.started) || r.stop_unknown {
                r.status = Status::Cancelling;
            } else {
                r.status = Status::Cancelled;
                r.active = None;
            }
            let cmd = CommandReceipt {
                kind: "cancel".into(),
                key: key.into(),
                fingerprint: fp.clone(),
                response: json!({"requested":true}),
                not_after_ms: None,
            };
            match self.inner.save(&row, &r, false, Some(cmd), &[]) {
                Ok(_) => {
                    committed = true;
                    break;
                }
                Err(e) if e.code == "revision_conflict" => continue,
                Err(e) => return Err(e),
            }
        }
        if !committed {
            return Err(Error::new(
                "revision_conflict",
                "/cancel",
                "concurrent state changes prevented cancellation; retry with the same key",
            ));
        }
        if let Some(token) = self.inner.handles.lock().map_err(Error::io)?.get(id) {
            token.cancel();
        }
        let row = self.inner.authorized(id, caller)?;
        let mut r = decode(&row)?;
        let has_handle = self
            .inner
            .handles
            .lock()
            .map_err(Error::io)?
            .contains_key(id);
        if r.status == Status::Cancelling
            && !has_handle
            && self.inner.confirm_stop(&r).await == StopState::Confirmed
        {
            r.status = Status::Cancelled;
            r.active = None;
            r.stop_unknown = false;
            r.error = None;
            r.business_outcome = None;
            r.progress = json!({"phase":"cancelled"});
            // The cancel command is already durable. A concurrent stop commit
            // can win this CAS; return its fresh state instead of leaking an
            // internal revision conflict to an API without expected_revision.
            match self.inner.save(&row, &r, false, None, &[]) {
                Ok(_) => {}
                Err(e) if e.code == "revision_conflict" => {}
                Err(e) => return Err(e),
            }
        }
        self.inner.notify.notify_one();
        self.inner.snapshot(&self.inner.authorized(id, caller)?)
    }
    pub async fn read_artifact(&self, id: &str, caller: &Caller) -> Result<ArtifactData> {
        let (execution, reference) = self.inner.store.artifact(id)?;
        self.inner.authorized(&execution, caller)?;
        Ok(ArtifactData {
            bytes: self.inner.store.read_blob(&reference.sha256)?.into(),
            reference,
        })
    }
    pub async fn shutdown(&self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        let _shutdown = tokio::time::timeout_at(deadline, self.shutdown_lock.lock())
            .await
            .map_err(|_| {
                Error::new(
                    "stop_unconfirmed",
                    "/shutdown",
                    "another shutdown has not completed",
                )
            })?;
        if let Some(core) = &self.goals {
            core.pause_for_shutdown()?;
        }
        self.inner.stop.cancel();
        for token in self.inner.handles.lock().map_err(Error::io)?.values() {
            token.cancel();
        }
        for token in self
            .inner
            .goal_run_handles
            .lock()
            .map_err(Error::io)?
            .values()
        {
            token.cancel();
        }
        self.inner.notify.notify_waiters();
        let trace_task = self.trace_task.lock().map_err(Error::io)?.take();
        if let Some(trace_task) = trace_task {
            trace_task.abort();
            let _ = trace_task.await;
        }
        let task = self.task.lock().map_err(Error::io)?.take();
        if let Some(mut task) = task {
            match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(joined) => joined.map_err(Error::io)?,
                Err(_) => {
                    *self.task.lock().map_err(Error::io)? = Some(task);
                    return Err(Error::new(
                        "stop_unconfirmed",
                        "/shutdown",
                        "executor did not confirm termination",
                    ));
                }
            }
        }
        for row in self.inner.store.all()? {
            let mut record = decode(&row)?;
            if record.stop_unknown || record.active.as_ref().is_some_and(|a| a.started) {
                if !matches!(
                    tokio::time::timeout_at(deadline, self.inner.confirm_stop(&record)).await,
                    Ok(StopState::Confirmed)
                ) {
                    return Err(Error::new(
                        "stop_unconfirmed",
                        "/shutdown",
                        "an execution has not confirmed stopping",
                    ));
                }
                record.stop_unknown = false;
                record.active = None;
                if record.status == Status::Cancelling {
                    record.status = Status::Cancelled;
                    record.error = None;
                } else if record.status == Status::Running {
                    record.status = Status::Blocked;
                    record.error = Some(Error::new(
                        "interrupted",
                        "/shutdown",
                        "host closed during execution; it will not be replayed",
                    ));
                }
                self.inner.save(&row, &record, false, None, &[])?;
            }
        }
        if self.goals.is_some()
            && self.inner.store.goal_all_active()?.iter().any(|row| {
                row.record
                    .runs
                    .values()
                    .any(|run| run.view.started && !run.view.native_stop_confirmed)
            })
        {
            return Err(Error::new(
                "stop_unconfirmed",
                "/shutdown",
                "a Goal native invocation has not confirmed stopping",
            ));
        }
        for executor in self.inner.bindings.executors.values() {
            tokio::time::timeout_at(deadline, executor.finish_observations())
                .await
                .map_err(|_| {
                    Error::new(
                        "trace_cleanup_unconfirmed",
                        "/shutdown/observation",
                        "native stops confirmed but observation collectors have not finished",
                    )
                })??;
        }
        if let Some(trace) = &self.inner.trace {
            tokio::time::timeout_at(deadline, trace.finish_content())
                .await
                .map_err(|_| {
                    Error::new(
                        "trace_cleanup_unconfirmed",
                        "/shutdown/content",
                        "content collectors have not finished",
                    )
                })??;
            let _ = trace.finish_observation();
        }
        Ok(())
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if let Ok(mut task) = self.trace_task.lock()
            && let Some(task) = task.take()
        {
            task.abort();
        }
        self.inner.stop.cancel();
        if let Ok(tokens) = self.inner.handles.lock() {
            for t in tokens.values() {
                t.cancel();
            }
        }
        if let Ok(tokens) = self.inner.goal_run_handles.lock() {
            for token in tokens.values() {
                token.cancel();
            }
        }
        self.inner.notify.notify_waiters();
    }
}
impl Inner {
    async fn confirm_stop(&self, record: &Record) -> StopState {
        // A new implementation bound under the same logical ID cannot attest
        // to a process started by the previous locked host configuration.
        if record.binding_fingerprint != self.binding_fingerprint {
            return StopState::Unknown;
        }
        let Some(active) = &record.active else {
            return StopState::Unknown;
        };
        let Some(executor) = self.bindings.executors.get(&active.executor) else {
            return StopState::Unknown;
        };
        crate::managed::confirm_stop(executor, &active.id).await
    }
    pub fn authorized(&self, id: &str, caller: &Caller) -> Result<Row> {
        let row = self.store.load(id)?;
        if row.owner != caller.principal {
            return Err(Error::new("not_found", "/execution", "execution not found"));
        }
        Ok(row)
    }
    pub fn save(
        &self,
        row: &Row,
        r: &Record,
        enqueue: bool,
        cmd: Option<CommandReceipt>,
        artifacts: &[ArtifactRef],
    ) -> Result<Commit> {
        self.store
            .commit(row, &serde_json::to_value(r)?, enqueue, cmd, artifacts)
    }
    pub fn snapshot(&self, row: &Row) -> Result<ExecutionSnapshot> {
        let r = decode(row)?;
        let d = self.store.definition(&r.definition_digest)?;
        let waiting_signals = if r.status == Status::Waiting {
            match &d.bundle.flow.states[&r.current_state] {
                State::Wait { signals, .. } => signals
                    .iter()
                    .map(|(name, s)| WaitingSignal {
                        name: name.clone(),
                        payload_schema_digest: digest(
                            &serde_json::to_vec(&d.schemas[&s.schema]).unwrap_or_default(),
                        ),
                    })
                    .collect(),
                _ => vec![],
            }
        } else {
            vec![]
        };
        Ok(ExecutionSnapshot {
            protocol_version: 1,
            execution_id: row.id.clone(),
            definition_digest: r.definition_digest,
            inputs: r.inputs,
            status: r.status,
            current_state: r.current_state.clone(),
            visit: *r.visits.get(&r.current_state).unwrap_or(&1),
            revision: row.revision,
            business_outcome: r.business_outcome,
            progress: r.progress,
            accepted_artifacts: r.history.into_iter().flat_map(|h| h.artifacts).collect(),
            waiting_signals,
            error: r.error,
        })
    }
    pub fn expire(&self, id: &str) -> Result<()> {
        let row = self.store.load(id)?;
        let mut r = decode(&row)?;
        if r.status == Status::Running
            && r.active.as_ref().is_some_and(|a| !a.started)
            && (now_ms() >= r.deadline_ms || r.active_ms >= r.limits.max_active_ms)
        {
            r.status = Status::Blocked;
            r.active = None;
            r.error = Some(Error::new(
                "limit_reached",
                "/dispatch",
                "queued execution exhausted its budget before dispatch",
            ));
            match self.save(&row, &r, false, None, &[]) {
                Ok(_) => {}
                Err(e) if e.code == "revision_conflict" => {}
                Err(e) => return Err(e),
            }
            return Ok(());
        }
        if r.status == Status::Waiting && r.wait_deadline_ms.is_some_and(|x| now_ms() >= x) {
            let d = self.store.definition(&r.definition_digest)?;
            if let State::Wait { timeout_target, .. } =
                d.bundle.flow.states[&r.current_state].clone()
            {
                enter(&mut r, &d, &timeout_target, true)?;
                match self.save(&row, &r, true, None, &[]) {
                    Ok(_) => self.notify.notify_one(),
                    Err(e) if e.code == "revision_conflict" => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }
}
fn nonempty(s: &str, path: &str) -> Result<()> {
    if s.trim().is_empty() || s.len() > 256 {
        return Err(Error::new(
            "input_invalid",
            path,
            "expected a nonempty key of at most 256 bytes",
        ));
    }
    Ok(())
}
pub(crate) fn requirements(
    r: &Record,
    d: &Definition,
    requires: &[Requirement],
) -> Result<Vec<ArtifactRef>> {
    let mut found = Vec::new();
    for dep in requires {
        let candidates: Vec<_> = r.history.iter().filter(|h| h.state == dep.state).collect();
        let selected: Vec<_> = if dep.selection == "latest" {
            candidates
                .into_iter()
                .max_by_key(|h| h.visit)
                .into_iter()
                .collect()
        } else {
            candidates
        };
        let items: Vec<_> = selected
            .iter()
            .flat_map(|h| h.artifacts.iter())
            .filter(|a| a.slot == dep.slot)
            .cloned()
            .collect();
        if items.is_empty() && !dep.optional {
            return Err(Error::new(
                "input_missing",
                format!("{}/{}", dep.state, dep.slot),
                "required accepted artifact missing",
            ));
        }
        found.extend(items);
        if dep.include_attachments {
            let Some(State::Work { entry, .. }) = d.bundle.flow.states.get(&dep.state) else {
                return Err(Error::new(
                    "definition_invalid",
                    "/requires",
                    "attachment producer missing",
                ));
            };
            let producer = &d.bundle.entries[entry];
            let policy = producer.attachments.as_ref().ok_or_else(|| {
                Error::new(
                    "definition_invalid",
                    "/requires",
                    "producer has no attachments",
                )
            })?;
            for history in selected {
                // An output dependency grants only attachments from its selected producer visit.
                if history.artifacts.iter().any(|a| a.slot == dep.slot) {
                    found.extend(
                        history
                            .artifacts
                            .iter()
                            .filter(|a| {
                                a.slot.starts_with(&policy.slot_prefix)
                                    && !producer.outputs.contains_key(&a.slot)
                            })
                            .cloned(),
                    );
                }
            }
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found.dedup_by(|a, b| a.id == b.id);
    Ok(found)
}
pub(crate) fn enter(r: &mut Record, d: &Definition, target: &str, transition: bool) -> Result<()> {
    let state = d
        .bundle
        .flow
        .states
        .get(target)
        .ok_or_else(|| Error::new("definition_invalid", "/target", "unknown state"))?
        .clone();
    let visit = r.visits.get(target).copied().unwrap_or(0) + 1;
    if visit > d.bundle.flow.limits.max_visits_per_state
        || (transition && r.transitions >= r.limits.max_transitions)
    {
        r.status = Status::Blocked;
        r.active = None;
        r.error = Some(Error::new(
            "limit_reached",
            "/flow",
            "visit or transition limit reached",
        ));
        return Ok(());
    }
    if let State::Terminal { requires, .. } = &state
        && let Err(e) = requirements(r, d, requires)
    {
        r.status = Status::Blocked;
        r.active = None;
        r.error = Some(e);
        return Ok(());
    }
    r.current_state = target.into();
    r.visits.insert(target.into(), visit);
    if transition {
        r.transitions += 1;
    }
    r.wait_deadline_ms = None;
    r.active = None;
    r.error = None;
    r.progress = json!({"phase":"queued"});
    match state {
        State::Terminal {
            status, outcome, ..
        } => {
            r.status = if status == "completed" {
                Status::Completed
            } else {
                Status::Failed
            };
            r.business_outcome = Some(outcome);
            r.progress = json!({"phase":"finished"});
        }
        State::Wait {
            timeout_seconds, ..
        } => {
            r.status = Status::Waiting;
            r.wait_deadline_ms = Some(
                now_ms()
                    .saturating_add(timeout_seconds.saturating_mul(1000))
                    .min(r.deadline_ms),
            );
            r.progress = json!({"phase":"waiting"});
        }
        State::Work { entry, .. } => {
            if r.runs >= r.limits.max_runs
                || r.active_ms >= r.limits.max_active_ms
                || now_ms() >= r.deadline_ms
            {
                r.status = Status::Blocked;
                r.error = Some(Error::new(
                    "limit_reached",
                    "/execution",
                    "execution budget reached",
                ));
                return Ok(());
            }
            let e = &d.bundle.entries[&entry];
            if let Err(error) = requirements(r, d, &e.requires) {
                r.status = Status::Blocked;
                r.error = Some(error);
                return Ok(());
            }
            r.runs += 1;
            r.epoch += 1;
            r.status = Status::Running;
            r.active = Some(ActiveRun {
                id: format!("run-{}", uuid::Uuid::new_v4()),
                executor: e.executor.clone(),
                epoch: r.epoch,
                started: false,
                started_ms: 0,
            });
        }
    }
    Ok(())
}
async fn recover_flow(inner: Arc<Inner>, row: Row) -> Result<()> {
    let mut record = decode(&row)?;
    let started = record.active.as_ref().is_some_and(|a| a.started);
    if started {
        if !record.stop_unknown {
            record.epoch += 1;
        }
        record.stop_unknown = true;
        if record.status == Status::Running {
            record.status = Status::Blocked;
            record.business_outcome = None;
            record.error.get_or_insert_with(|| {
                Error::new(
                    "interrupted",
                    "/execution",
                    "prior run has no committed final state; it will not be replayed",
                )
            });
        }
    }
    let confirmed = if record.active.is_some() {
        if !record.stop_unknown && !started {
            true
        } else {
            inner.confirm_stop(&record).await == StopState::Confirmed
        }
    } else {
        record.status == Status::Cancelling && !record.stop_unknown
    };
    if confirmed {
        record.stop_unknown = false;
        record.active = None;
        if record.status == Status::Cancelling {
            record.status = Status::Cancelled;
            record.error = None;
            record.business_outcome = None;
            record.progress = json!({"phase":"cancelled"});
        }
    }
    inner.save(&row, &record, false, None, &[])?;
    Ok(())
}

/// The only caller that starts Flow phases or Goal native invocations.
async fn shared_dispatcher(inner: Arc<Inner>, goals: Option<Arc<crate::orchestration::GoalCore>>) {
    use futures_util::FutureExt;
    use std::{collections::BTreeSet, panic::AssertUnwindSafe};
    let mut work = tokio::task::JoinSet::<(String, Result<()>)>::new();
    let mut live = BTreeSet::<String>::new();
    let mut cursor = None::<(u64, String)>;
    loop {
        while let Some(joined) = work.try_join_next() {
            if let Ok((key, _result)) = joined {
                live.remove(&key);
            }
        }
        if inner.stop.is_cancelled() {
            break;
        }
        if let Some(trace) = &inner.trace {
            trace.background_maintenance();
        }
        let mut owners = Vec::<(u64, String, bool)>::new();
        if let Ok(rows) = inner.store.all() {
            for row in rows {
                let Ok(record) = decode(&row) else {
                    continue;
                };
                if record.status == Status::Waiting
                    || record.status == Status::Running
                        && record.active.as_ref().is_some_and(|a| !a.started)
                {
                    let _ = inner.expire(&row.id);
                }
                let active_key = record.active.as_ref().map(|a| format!("flow:{}", a.id));
                if active_key.as_ref().is_some_and(|key| live.contains(key)) {
                    continue;
                }
                if record.stop_unknown
                    || record.status == Status::Cancelling
                    || record.active.as_ref().is_some_and(|a| a.started)
                {
                    let key = format!("flow-recovery:{}", row.id);
                    if live.insert(key.clone()) {
                        let host = inner.clone();
                        work.spawn(async move {
                            let result = AssertUnwindSafe(recover_flow(host, row))
                                .catch_unwind()
                                .await
                                .unwrap_or_else(|_| Err(Error::io("Flow stop probe panicked")));
                            (key, result)
                        });
                    }
                } else if record.status == Status::Running
                    && record.active.as_ref().is_some_and(|a| !a.started)
                {
                    owners.push((row.queue_order, row.id, false));
                }
            }
        }
        if let Some(core) = &goals {
            if let Ok(rows) = inner.store.goal_all_active() {
                for row in rows {
                    let _ = core.maintain(&row.id);
                    for (run_id, run) in &row.record.runs {
                        if live.contains(&format!("goal:{run_id}")) {
                            continue;
                        }
                        if run.view.phase == crate::GoalRunPhase::ExecutionReturned {
                            let _ = core.consume_return(&row.id, run_id);
                        }
                        if run.view.started && !run.view.native_stop_confirmed {
                            let key = format!("goal-recovery:{run_id}");
                            if live.insert(key.clone()) {
                                let core = core.clone();
                                let id = row.id.clone();
                                let run_id = run_id.clone();
                                work.spawn(async move {
                                    let result = AssertUnwindSafe(core.probe_stop(id, run_id))
                                        .catch_unwind()
                                        .await
                                        .unwrap_or_else(|_| {
                                            Err(Error::io("Goal stop probe panicked"))
                                        });
                                    (key, result)
                                });
                            }
                        }
                    }
                    if let Ok(current) = inner.store.goal_load(&row.id) {
                        for (node, state) in &current.record.nodes {
                            if state.view.status == crate::GoalNodeStatus::Checking {
                                let _ = core.finish_node_check(&row.id, node);
                            }
                        }
                    }
                    let _ = core.advance(&row.id);
                    owners.push((row.queue_order, row.id, true));
                }
            }
        }
        owners.sort();
        let split = cursor
            .as_ref()
            .map(|last| owners.partition_point(|(queue, id, _)| (*queue, id.clone()) <= *last))
            .unwrap_or(0);
        owners.rotate_left(split);
        let mut dispatched = false;
        for (queue, id, goal) in owners {
            let allowed = inner
                .dispatch_scope
                .lock()
                .map(|scope| match &*scope {
                    DispatchScope::All => true,
                    DispatchScope::Goal(selected) => {
                        goal && selected.as_deref() == Some(id.as_str())
                    }
                })
                .unwrap_or(false);
            if !allowed {
                continue;
            }
            if goal {
                let Some(core) = &goals else {
                    continue;
                };
                let Ok(Some(run_id)) = core.materialize(&id) else {
                    continue;
                };
                let preparation_at = tokio::time::Instant::now();
                match core.prepare_execution(&id, &run_id) {
                    Ok(Some(execution)) => {
                        let key = format!("goal:{run_id}");
                        live.insert(key.clone());
                        work.spawn(async move {
                            let result = AssertUnwindSafe(execution.execute())
                                .catch_unwind()
                                .await
                                .unwrap_or_else(|_| Err(Error::io("Goal driver panicked")));
                            (key, result)
                        });
                        cursor = Some((queue, id));
                        dispatched = true;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = core.fail_preparation(
                            &id,
                            &run_id,
                            error,
                            preparation_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        );
                    }
                }
            } else {
                let Ok(row) = inner.store.load(&id) else {
                    continue;
                };
                if inner.store.reserve_flow(&row).unwrap_or(false) {
                    let Ok(record) = decode(&row) else {
                        continue;
                    };
                    let Some(active) = &record.active else {
                        continue;
                    };
                    let key = format!("flow:{}", active.id);
                    live.insert(key.clone());
                    let host = inner.clone();
                    work.spawn(async move {
                        let result = AssertUnwindSafe(crate::phase::run(host, row))
                            .catch_unwind()
                            .await
                            .map_err(|_| Error::io("Flow driver panicked"));
                        (key, result)
                    });
                    cursor = Some((queue, id));
                    dispatched = true;
                }
            }
        }
        if dispatched {
            tokio::task::yield_now().await;
            continue;
        }
        tokio::select! {
            _ = inner.stop.cancelled() => break,
            _ = inner.notify.notified() => {},
            Some(joined) = work.join_next(), if !work.is_empty() => { if let Ok((key, _)) = joined { live.remove(&key); } },
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
        }
    }
    // shutdown keeps owning the dispatcher until all native futures return or report unknown.
    while work.join_next().await.is_some() {}
    if let Some(core) = goals {
        if let Ok(rows) = inner.store.goal_all_active() {
            for row in rows {
                for (run, state) in &row.record.runs {
                    if state.view.phase == crate::GoalRunPhase::ExecutionReturned {
                        let _ = core.consume_return(&row.id, run);
                    }
                }
                let _ = core.maintain(&row.id);
            }
        }
    }
}
