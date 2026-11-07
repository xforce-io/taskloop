//! Versioned observation projection. Never used to decide business state.
use crate::{Error, Invocation, MilkieObservationOptions, MilkieObserver, Result};
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

mod cleanup;
mod governance;
pub use cleanup::SourceCleanup;
pub(crate) use cleanup::cleanup_loop;

#[derive(Debug, Clone)]
pub struct TraceOptions {
    pub max_events_per_run: u64,
    pub max_bytes_per_run: u64,
    pub source_max_events: u64,
    pub source_max_bytes: u64,
}
impl TraceOptions {
    fn validate(&self) -> Result<()> {
        if !(2..=100_000).contains(&self.max_events_per_run)
            || !(4096..=67_108_864).contains(&self.max_bytes_per_run)
            || !(2..=100_000).contains(&self.source_max_events)
            || !(4096..=67_108_864).contains(&self.source_max_bytes)
        {
            return Err(Error::new(
                "input_invalid",
                "/trace",
                "explicit trace capacities out of range",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Default)]
pub struct TraceQuery {
    pub run_id: Option<String>,
    pub after: u64,
    /// Required page size, 1..=1000 (Default requires caller to set it).
    pub limit: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceEvent {
    pub schema_version: u32,
    pub semantic_version: u32,
    pub id: String,
    pub execution_id: String,
    pub run_id: String,
    pub state_id: String,
    pub visit: u64,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub source_id: String,
    pub source_sequence: u64,
    pub timestamp_ms: u64,
    #[serde(default)]
    pub run_observed_at_ms: u64,
    pub monotonic_ns: String,
    pub kind: String,
    pub data: Value,
    pub milkie_run_id: Option<String>,
    pub milkie_context_id: Option<String>,
    pub native: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceItem {
    pub cursor: u64,
    pub event: TraceEvent,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSource {
    pub run_id: String,
    pub source_id: String,
    pub last_sequence: u64,
    pub missing_sequences: u64,
    pub terminal: bool,
    /// Latest observable model/agent activity in this source's sequence, not a liveness claim.
    pub last_activity_sequence: Option<u64>,
    pub last_activity_timestamp_ms: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenSpan {
    pub run_id: String,
    pub span_id: String,
    pub kind: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracePage {
    pub schema_version: u32,
    pub semantic_version: u32,
    pub enabled: bool,
    pub events: Vec<TraceItem>,
    pub next: u64,
    /// Reception order is a cursor, not a cross-process clock or causal order.
    pub ordering: String,
    pub complete: bool,
    pub issues: Vec<String>,
    pub sources: Vec<TraceSource>,
    pub open_spans: Vec<OpenSpan>,
    #[serde(default)]
    pub policy: Option<crate::TracePolicySnapshot>,
    #[serde(default)]
    pub run_policies: BTreeMap<String, crate::TracePolicySnapshot>,
    #[serde(default)]
    pub expired_runs: Vec<String>,
    #[serde(default)]
    pub collector_degraded: bool,
    #[serde(default)]
    pub source_cleanup: Vec<SourceCleanup>,
}
pub(crate) struct TraceStore {
    conn: Mutex<Connection>,
    options: TraceOptions,
    faults: Mutex<BTreeSet<(String, String, String)>>,
    source_id: String,
    started: Instant,
    policy: Option<Arc<crate::TracePolicy>>,
    content_slots: Arc<tokio::sync::Semaphore>,
    degraded: AtomicBool,
}
impl TraceStore {
    pub fn open(root: &Path, options: TraceOptions) -> Result<Arc<Self>> {
        Self::open_with_policy(root, options, None)
    }
    pub fn open_with_policy(
        root: &Path,
        options: TraceOptions,
        policy: Option<crate::TracePolicy>,
    ) -> Result<Arc<Self>> {
        options.validate()?;
        if let Some(policy) = &policy {
            policy.validate()?;
        }
        let conn = Connection::open(root.join("trace.sqlite"))?;
        conn.busy_timeout(std::time::Duration::from_millis(5))?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 3 {
            return Err(Error::new(
                "storage_incompatible",
                "/trace",
                "unsupported trace schema",
            ));
        }
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS trace_events(cursor INTEGER PRIMARY KEY AUTOINCREMENT,execution_id TEXT NOT NULL,run_id TEXT NOT NULL,id TEXT NOT NULL,source_id TEXT NOT NULL,source_sequence INTEGER NOT NULL,bytes INTEGER NOT NULL,body TEXT NOT NULL,UNIQUE(execution_id,run_id,id),UNIQUE(execution_id,run_id,source_id,source_sequence));
          CREATE INDEX IF NOT EXISTS trace_read ON trace_events(execution_id,cursor);
          CREATE TABLE IF NOT EXISTS trace_faults(execution_id TEXT NOT NULL,run_id TEXT NOT NULL,reason TEXT NOT NULL,PRIMARY KEY(execution_id,run_id,reason));
          CREATE TABLE IF NOT EXISTS trace_totals(execution_id TEXT NOT NULL,run_id TEXT NOT NULL,events INTEGER NOT NULL,bytes INTEGER NOT NULL,PRIMARY KEY(execution_id,run_id));
          INSERT OR REPLACE INTO trace_totals SELECT execution_id,run_id,count(*),sum(bytes) FROM trace_events GROUP BY execution_id,run_id;
          CREATE TRIGGER IF NOT EXISTS trace_account_insert AFTER INSERT ON trace_events BEGIN
            INSERT INTO trace_totals VALUES(NEW.execution_id,NEW.run_id,1,NEW.bytes) ON CONFLICT(execution_id,run_id) DO UPDATE SET events=events+1,bytes=bytes+NEW.bytes;
          END;
          CREATE TRIGGER IF NOT EXISTS trace_account_delete AFTER DELETE ON trace_events BEGIN
            UPDATE trace_totals SET events=events-1,bytes=bytes-OLD.bytes WHERE execution_id=OLD.execution_id AND run_id=OLD.run_id;
          END;
          ")?;
        Self::migrate_policy(&conn, version)?;
        Self::migrate_cleanup(&conn)?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
            options,
            faults: Mutex::default(),
            source_id: format!("taskloop:{}", uuid::Uuid::new_v4()),
            started: Instant::now(),
            policy: policy.map(Arc::new),
            content_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            degraded: AtomicBool::new(false),
        }))
    }
    fn fault(&self, execution: &str, run: &str, reason: &str) {
        if reason != "retention_expired" {
            self.degraded.store(true, Ordering::Release);
        }
        let mut faults = self.faults.lock().unwrap_or_else(|e| e.into_inner());
        if faults.len() < 1024
            || faults
                .iter()
                .any(|(e, r, s)| e == execution && r == run && s == reason)
        {
            faults.insert((execution.into(), run.into(), reason.into()));
        }
        drop(faults);
        if let Ok(conn) = self.conn.lock() {
            let _=conn.execute("UPDATE trace_health SET value=min(value+1,9223372036854775807) WHERE key='dropped'",[]);
            let _=conn.execute("INSERT OR IGNORE INTO trace_faults SELECT ?1,?2,?3 WHERE EXISTS(SELECT 1 FROM trace_runs WHERE execution_id=?1 AND run_id=?2)",params![execution,run,reason]);
        }
    }
    fn insert(&self, e: &TraceEvent) -> Result<()> {
        let body = serde_json::to_string(e)?;
        let conn = self.conn.lock().map_err(Error::io)?;
        self.admit(&conn, &e.execution_id, &e.run_id, e.run_observed_at_ms)?;
        let old:Option<String>=conn.query_row("SELECT body FROM trace_events WHERE execution_id=?1 AND run_id=?2 AND (id=?3 OR (source_id=?4 AND source_sequence=?5)) LIMIT 1",params![e.execution_id,e.run_id,e.id,e.source_id,e.source_sequence as i64],|r|r.get(0)).optional()?;
        if let Some(old) = old {
            if old == body {
                return Ok(());
            }
            return Err(Error::new(
                "trace_conflict",
                "/trace",
                "conflicting source identity",
            ));
        }
        let (count, bytes): (i64, i64) = conn
            .query_row(
                "SELECT events,bytes FROM trace_totals WHERE execution_id=?1 AND run_id=?2",
                params![e.execution_id, e.run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0));
        let content_bytes: i64 = conn.query_row(
            "SELECT coalesce(sum(bytes),0) FROM trace_content WHERE execution_id=?1 AND run_id=?2",
            params![e.execution_id, e.run_id],
            |r| r.get(0),
        )?;
        if count as u64 >= self.options.max_events_per_run
            || (bytes as u64)
                .saturating_add(content_bytes as u64)
                .saturating_add(body.len() as u64)
                > self.options.max_bytes_per_run
        {
            return Err(Error::new(
                "trace_capacity",
                "/trace",
                "trace capacity exhausted",
            ));
        }
        self.global_capacity(&conn, body.len(), true)?;
        conn.execute("INSERT INTO trace_events(execution_id,run_id,id,source_id,source_sequence,bytes,body) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![e.execution_id,e.run_id,e.id,e.source_id,e.source_sequence as i64,body.len() as i64,body])?;
        Ok(())
    }
    fn record(&self, e: TraceEvent) {
        if self.maintain().is_err() {
            self.fault(&e.execution_id, &e.run_id, "retention_storage");
        }
        if let Err(error) = self.insert(&e) {
            self.fault(
                &e.execution_id,
                &e.run_id,
                match error.code.as_str() {
                    "trace_capacity" => "capacity",
                    "trace_conflict" => "source_conflict",
                    "trace_expired" => "retention_expired",
                    _ => "storage",
                },
            );
        }
    }
    pub fn session(
        self: &Arc<Self>,
        execution: &str,
        run: &str,
        state: &str,
        visit: u64,
    ) -> Arc<RunTrace> {
        Arc::new(RunTrace {
            store: self.clone(),
            execution: execution.into(),
            run: run.into(),
            state: state.into(),
            visit,
            sequence: AtomicU64::new(0),
            observed_ms: crate::runtime::now_ms(),
        })
    }
    pub fn read(&self, execution: &str, q: &TraceQuery) -> Result<TracePage> {
        validate_query(q)?;
        let _ = self.maintain();
        let (run_policies, expired_runs) = self.query_policies(execution, q.run_id.as_deref())?;
        let source_cleanup = self.cleanup_view(execution, q.run_id.as_deref())?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::new("trace_unavailable", "/trace", "trace store unavailable"))?;
        // The per-run admission bound makes each source scan bounded; no original Harness files are read.
        let mut stmt=conn.prepare("SELECT cursor,body FROM trace_events WHERE execution_id=?1 AND (?2 IS NULL OR run_id=?2) AND (execution_id,run_id) IN (SELECT execution_id,run_id FROM trace_runs WHERE status!='expired' AND (expires_ms IS NULL OR expires_ms>?3)) ORDER BY cursor")?;
        let rows = stmt.query_map(
            params![execution, q.run_id, crate::runtime::now_ms() as i64],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?)),
        )?;
        let mut events = Vec::new();
        let mut sources: BTreeMap<(String, String), (BTreeSet<u64>, bool)> = BTreeMap::new();
        let mut spans: BTreeMap<(String, String), (Option<String>, bool)> = BTreeMap::new();
        let mut issues = BTreeSet::new();
        let dropped: i64 = conn.query_row(
            "SELECT value FROM trace_health WHERE key='dropped'",
            [],
            |r| r.get(0),
        )?;
        let collector_degraded = self.degraded.load(Ordering::Acquire) || dropped > 0;

        for run in &expired_runs {
            issues.insert(format!("retention_expired:{run}"));
        }
        let mut latest_status = BTreeMap::new();
        let mut activities: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
        let mut expected_runs = BTreeSet::new();
        for row in rows {
            let (cursor, body) = row?;
            let e: TraceEvent = serde_json::from_str(&body).map_err(|_| {
                Error::new("trace_corrupt", "/trace", "invalid persisted trace record")
            })?;
            let source = sources
                .entry((e.run_id.clone(), e.source_id.clone()))
                .or_default();
            source.0.insert(e.source_sequence);
            if matches!(
                e.kind.as_str(),
                "agent.activity" | "model.requested" | "model.responded"
            ) {
                let key = (e.run_id.clone(), e.source_id.clone());
                if activities.get(&key).is_none_or(|a| a.0 < e.source_sequence) {
                    activities.insert(key, (e.source_sequence, e.timestamp_ms));
                }
            }
            source.1 |= matches!(e.kind.as_str(), "phase.finished" | "execution.finished");
            let span = spans
                .entry((e.run_id.clone(), e.span_id.clone()))
                .or_default();
            if matches!(
                e.kind.as_str(),
                "phase.started"
                    | "execution.started"
                    | "tool.started"
                    | "tool.requested"
                    | "check.started"
            ) {
                span.0 = Some(e.kind.clone());
            }
            if matches!(
                e.kind.as_str(),
                "phase.finished" | "execution.finished" | "tool.finished" | "check.finished"
            ) || (e.kind == "tool.delivery" && e.data["outcome"] == "delivered")
            {
                span.1 = true;
            }
            if e.kind == "source.status" && e.data["kind"].is_null() {
                latest_status.insert(e.run_id.clone(), e.data.clone());
            }
            if e.kind == "source.status" && e.data["adapter_delivery_failed"] == true {
                issues.insert(format!("adapter_delivery_failed:{}", e.run_id));
            }
            if e.kind == "executor.capabilities" && e.data["internal_observation"] == "available" {
                expected_runs.insert(e.run_id.clone());
            }
            if e.kind == "executor.capabilities" && e.data["internal_observation"] == "unavailable"
            {
                issues.insert(format!("internal_unavailable:{}", e.run_id));
            }
            if cursor > q.after && events.len() < q.limit as usize {
                events.push(TraceItem { cursor, event: e });
            }
        }
        let mut stmt=conn.prepare("SELECT run_id,reason FROM trace_faults WHERE execution_id=?1 AND (?2 IS NULL OR run_id=?2)")?;
        for row in stmt.query_map(params![execution, q.run_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (run, reason) = row?;
            issues.insert(format!("{reason}:{run}"));
        }
        for (id, run, reason) in self.faults.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if id == execution && q.run_id.as_ref().is_none_or(|q| q == run) {
                issues.insert(format!("{reason}:{run}"));
            }
        }
        let sources: Vec<_> = sources
            .into_iter()
            .map(|((run_id, source_id), (seq, terminal))| {
                let reported = if source_id.starts_with("milkie:") {
                    latest_status
                        .get(&run_id)
                        .and_then(|s: &Value| s["lastSequence"].as_u64())
                        .unwrap_or(0)
                } else {
                    0
                };
                let last_sequence = seq.last().copied().unwrap_or(0).max(reported);
                let missing_sequences = last_sequence.saturating_sub(seq.len() as u64);
                if missing_sequences > 0 {
                    issues.insert(format!("sequence_gap:{run_id}:{source_id}"));
                }
                let activity = activities.get(&(run_id.clone(), source_id.clone()));
                TraceSource {
                    last_activity_sequence: activity.map(|a| a.0),
                    last_activity_timestamp_ms: activity.map(|a| a.1),
                    run_id,
                    source_id,
                    last_sequence,
                    missing_sequences,
                    terminal,
                }
            })
            .collect();
        for run in expected_runs {
            if !sources
                .iter()
                .any(|s: &TraceSource| s.run_id == run && s.source_id.starts_with("milkie:"))
            {
                issues.insert(format!("source_unavailable:{run}"));
            }
        }
        for (run, status) in latest_status {
            if status["state"] == "incomplete"
                || status["fault"].as_str().is_some()
                || status["dropped"].as_u64().is_some_and(|n| n > 0)
            {
                issues.insert(format!("source_incomplete:{run}"));
            }
        }
        let open_spans: Vec<_> = spans
            .into_iter()
            .filter_map(|((run_id, span_id), (kind, ended))| {
                if ended {
                    None
                } else {
                    kind.map(|kind| OpenSpan {
                        run_id,
                        span_id,
                        kind,
                    })
                }
            })
            .collect();
        if sources.is_empty() {
            issues.insert("not_recorded_or_expired".into());
        }
        let complete = !sources.is_empty()
            && sources.iter().all(|s| s.terminal)
            && open_spans.is_empty()
            && issues.is_empty();
        Ok(TracePage {
            schema_version: 1,
            semantic_version: 1,
            enabled: true,
            next: events.last().map_or(q.after, |e| e.cursor),
            events,
            ordering: "reception_cursor; source_sequence_within_source_only".into(),
            complete,
            issues: issues.into_iter().collect(),
            sources,
            open_spans,
            policy: Some(self.policy_snapshot()),
            run_policies,
            expired_runs,
            collector_degraded,
            source_cleanup,
        })
    }
}
fn validate_query(q: &TraceQuery) -> Result<()> {
    if !(1..=1000).contains(&q.limit)
        || q.after > i64::MAX as u64
        || q.run_id
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 256)
    {
        return Err(Error::new(
            "input_invalid",
            "/trace/query",
            "invalid cursor, run or page size",
        ));
    }
    Ok(())
}
pub(crate) fn disabled(q: &TraceQuery) -> Result<TracePage> {
    validate_query(q)?;
    Ok(TracePage {
        schema_version: 1,
        semantic_version: 1,
        enabled: false,
        events: vec![],
        next: q.after,
        ordering: "reception_cursor; source_sequence_within_source_only".into(),
        complete: false,
        issues: vec!["disabled".into()],
        sources: vec![],
        open_spans: vec![],
        policy: None,
        run_policies: BTreeMap::new(),
        expired_runs: vec![],
        collector_degraded: false,
        source_cleanup: vec![],
    })
}
pub(crate) struct RunTrace {
    store: Arc<TraceStore>,
    execution: String,
    run: String,
    state: String,
    visit: u64,
    sequence: AtomicU64,
    observed_ms: u64,
}
impl RunTrace {
    pub fn trace_id(&self) -> String {
        crate::json::digest(self.run.as_bytes())[..32].to_owned()
    }
    pub fn phase_span(&self) -> String {
        crate::json::digest(format!("phase:{}", self.run).as_bytes())[..16].to_owned()
    }
    pub fn span() -> String {
        uuid::Uuid::new_v4().simple().to_string()[..16].to_owned()
    }
    pub fn emit(&self, kind: &str, span: &str, parent: Option<String>, data: Value) {
        self.store.record(TraceEvent {
            schema_version: 1,
            semantic_version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            execution_id: self.execution.clone(),
            run_id: self.run.clone(),
            state_id: self.state.clone(),
            visit: self.visit,
            trace_id: self.trace_id(),
            span_id: span.into(),
            parent_span_id: parent,
            source_id: self.store.source_id.clone(),
            source_sequence: self.sequence.fetch_add(1, Ordering::AcqRel) + 1,
            timestamp_ms: crate::runtime::now_ms(),
            run_observed_at_ms: self.observed_ms,
            monotonic_ns: self.store.started.elapsed().as_nanos().to_string(),
            kind: kind.into(),
            data,
            milkie_run_id: None,
            milkie_context_id: None,
            native: Value::Null,
        });
    }
    fn reject(&self) {
        self.store
            .fault(&self.execution, &self.run, "invalid_source");
    }
}
fn text(v: &Value, key: &str, max: usize) -> Option<String> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= max)
        .map(str::to_owned)
}
fn hex(v: &Value, key: &str, len: usize) -> Option<String> {
    text(v, key, len).filter(|s| {
        s.len() == len
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && s.bytes().any(|b| b != b'0')
    })
}
// Explicit allow-list prevents a future Harness field from silently collecting content.
fn source_data(kind: &str, data: &Value) -> Option<Value> {
    let fields: &[&str] = match kind {
        "execution.started" => &["runtime", "capabilities"],
        "execution.finished" => &["status", "stopped", "code"],
        "execution.interrupted" => &["reason"],
        "agent.activity" => &["category"],
        "model.requested" => &["scope"],
        "model.responded" => &["scope", "outcome"],
        "usage" => &[
            "scope",
            "aggregation",
            "inputTokens",
            "outputTokens",
            "totalTokens",
            "cachedInputTokens",
            "reasoningOutputTokens",
        ],
        "tool.requested" => &["callId", "name", "input"],
        "tool.rejected" => &["callId", "code"],
        "tool.host.started" => &["callId"],
        "tool.host.responded" => &["callId", "outcome", "output"],
        "tool.delivery" => &["callId", "outcome"],
        "tool.interrupted" => &["callId", "reason"],
        _ => return None,
    };
    let mut out = serde_json::Map::new();
    for key in fields {
        if let Some(v) = data.get(key) {
            let safe = match *key {
                "input" | "output" => {
                    let hash = hex(v, "sha256", 64)?;
                    let bytes = v["bytes"].as_u64()?;
                    json!({"sha256":hash,"bytes":bytes})
                }
                "capabilities" => {
                    let mut c = serde_json::Map::new();
                    for k in [
                        "version",
                        "lifecycle",
                        "activity",
                        "usage",
                        "modelRequests",
                        "hostTools",
                        "hiddenReasoning",
                        "purge",
                    ] {
                        if let Some(v) = v.get(k) {
                            if !(v.is_boolean()
                                || v.is_u64()
                                || v.as_str().is_some_and(|s| s.len() <= 64))
                            {
                                return None;
                            }
                            c.insert(k.into(), v.clone());
                        }
                    }
                    Value::Object(c)
                }
                _ => {
                    if !(v.is_boolean() || v.is_u64() || v.as_str().is_some_and(|s| s.len() <= 128))
                    {
                        return None;
                    }
                    v.clone()
                }
            };
            out.insert((*key).into(), safe);
        }
    }
    Some(Value::Object(out))
}
#[async_trait]
impl MilkieObserver for RunTrace {
    fn options(&self, _: &Invocation) -> MilkieObservationOptions {
        MilkieObservationOptions {
            traceparent: format!("00-{}-{}-01", self.trace_id(), self.phase_span()),
            max_events: self.store.options.source_max_events,
            max_bytes: self.store.options.source_max_bytes,
        }
    }
    async fn event(&self, inv: &Invocation, e: Value) -> Result<()> {
        let parsed = (|| {
            if inv.run_id != self.run
                || inv.execution_id != self.execution
                || e["version"] != 1
                || e["traceId"] != self.trace_id()
            {
                return None;
            }
            let kind = text(&e, "kind", 64)?;
            let source = text(&e, "sourceId", 128)?;
            let milkie_run = text(&e, "runId", 128)?;
            if source != milkie_run {
                return None;
            }
            let sequence = e["sequence"]
                .as_u64()
                .filter(|n| *n > 0 && *n <= i64::MAX as u64)?;
            let mono =
                text(&e, "monotonicNs", 40).filter(|s| s.bytes().all(|b| b.is_ascii_digit()))?;
            let mut native = serde_json::Map::new();
            for key in ["sessionId", "turnId", "callId", "responseId"] {
                if let Some(v) = e.get("native").and_then(|v| v.get(key)) {
                    if !v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 128) {
                        return None;
                    }
                    native.insert(key.into(), v.clone());
                }
            }
            Some(TraceEvent {
                schema_version: 1,
                semantic_version: 1,
                id: text(&e, "id", 128)?,
                execution_id: self.execution.clone(),
                run_id: self.run.clone(),
                state_id: self.state.clone(),
                visit: self.visit,
                trace_id: self.trace_id(),
                span_id: hex(&e, "spanId", 16)?,
                parent_span_id: Some(hex(&e, "parentSpanId", 16)?),
                source_id: format!("milkie:{source}"),
                source_sequence: sequence,
                timestamp_ms: e["timestamp"].as_u64()?,
                run_observed_at_ms: self.observed_ms,
                monotonic_ns: mono,
                kind: kind.clone(),
                data: source_data(&kind, &e["data"])?,
                milkie_run_id: Some(milkie_run),
                milkie_context_id: Some(text(&e, "contextId", 128)?),
                native: Value::Object(native),
            })
        })();
        if let Some(e) = parsed {
            self.store.record(e)
        } else {
            self.reject()
        };
        Ok(())
    }
    async fn status(&self, inv: &Invocation, s: Value) -> Result<()> {
        if inv.run_id != self.run || inv.execution_id != self.execution {
            self.reject();
            return Ok(());
        }
        // Adapter status contains no arbitrary error/body data.
        let failed = s["adapter_delivery_failed"] == true;
        let data = if s["kind"] == "adapter.summary" {
            json!({"kind":"adapter.summary","adapter_delivery_failed":failed})
        } else if s["kind"] == "adapter.termination" {
            json!({"kind":"adapter.termination","agentStopConfirmed":s["agentStopConfirmed"].as_bool(),"adapterStopConfirmed":s["adapterStopConfirmed"].as_bool(),"pendingToolCalls":s["pendingToolCalls"].as_u64(),"adapter_delivery_failed":failed})
        } else {
            json!({"state":text(&s,"state",32),"complete":s["complete"].as_bool(),"lastSequence":s["lastSequence"].as_u64(),"dropped":s["dropped"].as_u64(),"fault":text(&s,"fault",32),"adapter_delivery_failed":failed})
        };
        self.emit("source.status", &self.phase_span(), None, data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> (tempfile::TempDir, Arc<TraceStore>) {
        let temp = tempfile::tempdir().unwrap();
        let store = TraceStore::open(
            temp.path(),
            TraceOptions {
                max_events_per_run: 100,
                max_bytes_per_run: 65536,
                source_max_events: 100,
                source_max_bytes: 65536,
            },
        )
        .unwrap();
        (temp, store)
    }
    fn q() -> TraceQuery {
        TraceQuery {
            limit: 100,
            ..Default::default()
        }
    }
    fn event(sequence: u64, kind: &str) -> TraceEvent {
        TraceEvent {
            schema_version: 1,
            semantic_version: 1,
            id: format!("id-{sequence}"),
            execution_id: "execution".into(),
            run_id: "run".into(),
            state_id: "work".into(),
            visit: 1,
            trace_id: "a".repeat(32),
            span_id: "b".repeat(16),
            parent_span_id: Some("c".repeat(16)),
            source_id: "milkie:source".into(),
            source_sequence: sequence,
            timestamp_ms: 1000 - sequence,
            run_observed_at_ms: 1000,
            monotonic_ns: sequence.to_string(),
            kind: kind.into(),
            data: json!({}),
            milkie_run_id: Some("source".into()),
            milkie_context_id: Some("context".into()),
            native: Value::Null,
        }
    }
    #[test]
    fn source_gap_is_visible_then_resolves_and_duplicates_are_idempotent() {
        let (_temp, s) = store();
        s.record(event(1, "execution.started"));
        s.record(event(3, "execution.finished"));
        let p = s.read("execution", &q()).unwrap();
        assert!(!p.complete);
        assert_eq!(p.sources[0].missing_sequences, 1);
        s.record(event(1, "execution.started"));
        s.record(event(2, "agent.activity"));
        let p = s.read("execution", &q()).unwrap();
        assert!(p.complete);
        assert_eq!(p.events.len(), 3);
        assert_eq!(p.sources[0].last_activity_sequence, Some(2));
        assert_eq!(p.sources[0].last_activity_timestamp_ms, Some(998));
        assert_eq!(
            p.events
                .iter()
                .map(|e| e.event.source_sequence)
                .collect::<Vec<_>>(),
            vec![1, 3, 2]
        );
        let mut collision = event(2, "agent.activity");
        collision.data = json!({"category":"turn"});
        s.record(collision);
        let p = s.read("execution", &q()).unwrap();
        assert!(!p.complete);
        assert!(p.issues.iter().any(|s| s.starts_with("source_conflict:")));
        assert_eq!(p.events.len(), 3);
    }
    #[test]
    fn persisted_conflict_and_storage_failure_survive_reopen() {
        let (temp, s) = store();
        s.record(event(1, "execution.started"));
        let mut collision = event(1, "agent.activity");
        collision.id = "different-id".into();
        s.record(collision);
        s.conn.lock().unwrap().execute_batch("CREATE TRIGGER fail_trace BEFORE INSERT ON trace_events BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
        s.record(event(2, "execution.finished"));
        let p = s.read("execution", &q()).unwrap();
        assert!(p.issues.iter().any(|e| e.starts_with("storage:")));
        assert!(!p.complete);
        let opts = s.options.clone();
        drop(s);
        let s = TraceStore::open(temp.path(), opts).unwrap();
        let p = s.read("execution", &q()).unwrap();
        assert!(p.issues.iter().any(|e| e.starts_with("source_conflict:")));
        assert!(p.issues.iter().any(|e| e.starts_with("storage:")));
    }
    #[test]
    fn content_allowlist_and_unknown_semantics() {
        let clean=source_data("tool.requested",&json!({"callId":"c","name":"tool","input":{"bytes":1,"sha256":"a".repeat(64),"raw":"SECRET"},"prompt":"SECRET"})).unwrap();
        assert!(!clean.to_string().contains("SECRET"));
        assert!(source_data("future.event", &json!({})).is_none());
        assert!(source_data("usage", &json!({"totalTokens":-1})).is_none());
    }
    #[test]
    fn reported_tail_gap_and_missing_source_are_not_complete() {
        let (_temp, s) = store();
        s.record(event(1, "execution.started"));
        s.record(event(2, "execution.finished"));
        let local = s.session("execution", "run", "work", 1);
        local.emit(
            "source.status",
            &local.phase_span(),
            None,
            json!({"state":"finished","complete":true,"lastSequence":4,"dropped":0}),
        );
        local.emit("phase.finished", &local.phase_span(), None, json!({}));
        let p = s.read("execution", &q()).unwrap();
        assert!(!p.complete);
        assert_eq!(
            p.sources
                .iter()
                .find(|s| s.source_id == "milkie:source")
                .unwrap()
                .missing_sequences,
            2
        );
        let other = s.session("empty", "run", "work", 1);
        other.emit(
            "executor.capabilities",
            &other.phase_span(),
            None,
            json!({"internal_observation":"available"}),
        );
        other.emit("phase.finished", &other.phase_span(), None, json!({}));
        let p = s.read("empty", &q()).unwrap();
        assert!(!p.complete);
        assert!(
            p.issues
                .iter()
                .any(|i| i.starts_with("source_unavailable:"))
        );
    }
}
