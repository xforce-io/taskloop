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
    pub live_budgets: Mutex<BTreeMap<String, Arc<crate::budget::BudgetState>>>,
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
        Self::open_impl(options, definitions, bindings, None, None).await
    }
    pub async fn open_with_trace(
        options: RuntimeOptions,
        definitions: Vec<Definition>,
        bindings: HostBindings,
        trace: crate::TraceOptions,
    ) -> Result<Self> {
        Self::open_impl(options, definitions, bindings, Some(trace), None).await
    }
    pub async fn open_with_trace_policy(
        options: RuntimeOptions,
        definitions: Vec<Definition>,
        bindings: HostBindings,
        trace: crate::TraceOptions,
        policy: crate::TracePolicy,
    ) -> Result<Self> {
        Self::open_impl(options, definitions, bindings, Some(trace), Some(policy)).await
    }
    async fn open_impl(
        options: RuntimeOptions,
        definitions: Vec<Definition>,
        bindings: HostBindings,
        trace_options: Option<crate::TraceOptions>,
        trace_policy: Option<crate::TracePolicy>,
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
        let store = Store::open(&options.store_dir)?;
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
            live_budgets: Mutex::new(BTreeMap::new()),
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
        let work = inner.clone();
        let task = tokio::spawn(async move {
            worker_loop(work).await;
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
        self.inner.stop.cancel();
        for token in self.inner.handles.lock().map_err(Error::io)?.values() {
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
        executor.stop_or_confirm(&active.id).await
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
async fn worker_loop(inner: Arc<Inner>) {
    while !inner.stop.is_cancelled() {
        if let Some(trace) = &inner.trace {
            trace.background_maintenance();
        }
        let rows = match inner.store.all() {
            Ok(rows) => rows,
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };
        let mut blocked_capacity = false;
        let mut next = None;
        for row in rows {
            let Ok(mut r) = decode(&row) else {
                continue;
            };
            if r.status == Status::Waiting
                || (r.status == Status::Running && r.active.as_ref().is_some_and(|a| !a.started))
            {
                let _ = inner.expire(&row.id);
            }
            let started = r.active.as_ref().is_some_and(|a| a.started);
            if r.stop_unknown || r.status == Status::Cancelling || started {
                // This loop is serial with phase execution: any started Run
                // seen here has returned without a durable final state. Failed
                // stop-state writes must not make it disappear from capacity.
                let recovery_changed = started && (!r.stop_unknown || r.status == Status::Running);
                if recovery_changed {
                    if !r.stop_unknown {
                        r.epoch += 1;
                    }
                    r.stop_unknown = true;
                    if r.status == Status::Running {
                        r.status = Status::Blocked;
                        r.business_outcome = None;
                        r.error.get_or_insert_with(|| {
                            Error::new(
                                "interrupted",
                                "/execution",
                                "prior run has no committed final state; it will not be replayed",
                            )
                        });
                        r.progress = json!({"phase":"blocked"});
                    }
                }
                let confirmed = if let Some(active) = &r.active {
                    if !r.stop_unknown && !active.started {
                        true
                    } else {
                        inner.confirm_stop(&r).await == StopState::Confirmed
                    }
                } else {
                    r.status == Status::Cancelling && !r.stop_unknown
                };
                if confirmed {
                    r.stop_unknown = false;
                    r.active = None;
                    if r.status == Status::Cancelling {
                        r.status = Status::Cancelled;
                        r.error = None;
                        r.business_outcome = None;
                        r.progress = json!({"phase":"cancelled"});
                    }
                    if inner.save(&row, &r, false, None, &[]).is_err() {
                        blocked_capacity = true;
                    }
                } else {
                    blocked_capacity = true;
                    if recovery_changed {
                        // Keep capacity blocked even if storage is still faulty.
                        let _ = inner.save(&row, &r, false, None, &[]);
                    }
                }
            }
            if next.is_none()
                && r.status == Status::Running
                && r.active.as_ref().is_some_and(|a| !a.started)
            {
                next = Some(row);
            }
        }
        if !blocked_capacity
            && let Some(row) = next
            && inner.store.reserve_flow(&row).unwrap_or(false)
        {
            crate::phase::run(inner.clone(), row).await;
            continue;
        }
        tokio::select! {_ = inner.stop.cancelled()=>break,_ = inner.notify.notified()=>{},_ = tokio::time::sleep(std::time::Duration::from_millis(500))=>{}}
    }
}
