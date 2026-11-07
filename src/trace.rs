//! Versioned observation projection. Never used to decide business state.
use crate::{Error, Invocation, MilkieObservationOptions, MilkieObserver, Result};
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
mod summary;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

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
}
pub(crate) struct TraceStore {
    conn: Mutex<Connection>,
    options: TraceOptions,
    faults: Mutex<BTreeSet<(String, String, String)>>,
    source_id: String,
    started: Instant,
}
impl TraceStore {
    pub fn open(root: &Path, options: TraceOptions) -> Result<Arc<Self>> {
        options.validate()?;
        let conn = Connection::open(root.join("trace.sqlite"))?;
        conn.busy_timeout(std::time::Duration::from_millis(5))?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 1 {
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
          PRAGMA user_version=1;")?;
        Self::migrate_summary(&conn)?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
            options,
            faults: Mutex::default(),
            source_id: format!("taskloop:{}", uuid::Uuid::new_v4()),
            started: Instant::now(),
        }))
    }
    fn fault(&self, execution: &str, run: &str, reason: &str) {
        self.faults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((execution.into(), run.into(), reason.into()));
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO trace_faults VALUES(?1,?2,?3)",
                params![execution, run, reason],
            );
        }
    }
    fn insert(&self, e: &TraceEvent) -> Result<()> {
        let body = serde_json::to_string(e)?;
        let conn = self.conn.lock().map_err(Error::io)?;
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
        if count as u64 >= self.options.max_events_per_run
            || (bytes as u64).saturating_add(body.len() as u64) > self.options.max_bytes_per_run
        {
            return Err(Error::new(
                "trace_capacity",
                "/trace",
                "trace capacity exhausted",
            ));
        }
        conn.execute("INSERT INTO trace_events(execution_id,run_id,id,source_id,source_sequence,bytes,body) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![e.execution_id,e.run_id,e.id,e.source_id,e.source_sequence as i64,body.len() as i64,body])?;
        Ok(())
    }
    fn record(&self, e: TraceEvent) {
        if let Err(error) = self.insert(&e) {
            self.fault(
                &e.execution_id,
                &e.run_id,
                match error.code.as_str() {
                    "trace_capacity" => "capacity",
                    "trace_conflict" => "source_conflict",
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
        })
    }
    pub fn read(&self, execution: &str, q: &TraceQuery) -> Result<TracePage> {
        validate_query(q)?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::new("trace_unavailable", "/trace", "trace store unavailable"))?;
        let events = Self::event_page(&conn, execution, q)?;
        let summary::Summary {
            sources,
            open_spans,
            mut issues,
        } = Self::read_summary(&conn, execution, q)?;
        let mut stmt=conn.prepare("SELECT run_id,reason FROM trace_faults WHERE execution_id=?1 AND (?2 IS NULL OR run_id=?2)")?;
        for row in stmt.query_map(params![execution, q.run_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (run, reason) = row?;
            issues.insert(format!("{reason}:{run}"));
        }
        let faults = self.faults.lock().unwrap_or_else(|e| e.into_inner());
        for (id, run, reason) in faults
            .range((execution.to_owned(), String::new(), String::new())..)
            .take_while(|(id, _, _)| id == execution)
        {
            if id == execution && q.run_id.as_ref().is_none_or(|q| q == run) {
                issues.insert(format!("{reason}:{run}"));
            }
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
    })
}
pub(crate) struct RunTrace {
    store: Arc<TraceStore>,
    execution: String,
    run: String,
    state: String,
    visit: u64,
    sequence: AtomicU64,
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
    #[test]
    fn page_reads_stay_within_a_fixed_sql_budget_after_large_history_and_restart() {
        let temp = tempfile::tempdir().unwrap();
        let opts = TraceOptions {
            max_events_per_run: 100000,
            max_bytes_per_run: 67108864,
            source_max_events: 100000,
            source_max_bytes: 67108864,
        };
        let store = TraceStore::open(temp.path(), opts.clone()).unwrap();
        {
            let mut conn = store.conn.lock().unwrap();
            let tx = conn.transaction().unwrap();
            {
                let mut insert=tx.prepare("INSERT INTO trace_events(execution_id,run_id,id,source_id,source_sequence,bytes,body) VALUES(?1,?2,?3,?4,?5,?6,?7)").unwrap();
                for sequence in 1..=10000 {
                    let mut e = event(1, "agent.activity");
                    e.id = format!("id-{sequence}");
                    e.source_sequence = sequence;
                    e.timestamp_ms = sequence;
                    let body = serde_json::to_string(&e).unwrap();
                    insert
                        .execute(params![
                            e.execution_id,
                            e.run_id,
                            e.id,
                            e.source_id,
                            e.source_sequence as i64,
                            body.len() as i64,
                            body
                        ])
                        .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        drop(store);
        let store = TraceStore::open(temp.path(), opts).unwrap();
        let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = ticks.clone();
        store
            .conn
            .lock()
            .unwrap()
            .progress_handler(
                1000,
                Some(move || count.fetch_add(1, Ordering::SeqCst) >= 50),
            )
            .unwrap();
        let result = store.read(
            "execution",
            &TraceQuery {
                after: 9999,
                limit: 1,
                run_id: Some("run".into()),
            },
        );
        store
            .conn
            .lock()
            .unwrap()
            .progress_handler(0, None::<fn() -> bool>)
            .unwrap();
        let page=result.expect("a one-event tail page scanned the entire history and exceeded 50000 SQLite VM instructions");
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.next, 10000);
        assert_eq!(page.sources[0].last_sequence, 10000);
        assert_eq!(page.sources[0].missing_sequences, 0);
        ticks.store(0, Ordering::SeqCst);
        let count = ticks.clone();
        store
            .conn
            .lock()
            .unwrap()
            .progress_handler(
                1000,
                Some(move || count.fetch_add(1, Ordering::SeqCst) >= 50),
            )
            .unwrap();
        let mut next = event(1, "agent.activity");
        next.id = "id-10001".into();
        next.source_sequence = 10001;
        let written = store.insert(&next);
        store
            .conn
            .lock()
            .unwrap()
            .progress_handler(0, None::<fn() -> bool>)
            .unwrap();
        written
            .expect("event identity lookups exceeded the fixed SQL budget after a large history");
    }
    #[test]
    fn summary_backfill_preserves_old_event_bytes_and_whole_run_retirement_is_isolated() {
        let (temp, s) = store();
        for (n, kind) in [
            (1, "execution.started"),
            (2, "agent.activity"),
            (3, "execution.finished"),
        ] {
            s.record(event(n, kind));
        }
        let before = s.read("execution", &q()).unwrap();
        assert!(before.complete);
        let options = s.options.clone();
        drop(s);
        let c = Connection::open(temp.path().join("trace.sqlite")).unwrap();
        let bodies: Vec<String> = c
            .prepare("SELECT body FROM trace_events ORDER BY cursor")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        c.execute_batch("DROP TRIGGER trace_summary_insert; DROP TRIGGER trace_summary_delete; DROP TABLE trace_summary_sources; DROP TABLE trace_summary_spans; DROP TABLE trace_summary_runs; DROP TABLE trace_summary_meta;").unwrap();
        drop(c);
        let s = TraceStore::open(temp.path(), options).unwrap();
        let after = s.read("execution", &q()).unwrap();
        assert!(after.complete);
        assert_eq!(after.next, before.next);
        assert_eq!(after.sources[0].last_activity_sequence, Some(2));
        let mut other = event(1, "execution.started");
        other.execution_id = "other".into();
        s.record(other);
        {
            let c = s.conn.lock().unwrap();
            let after: Vec<String> = c
                .prepare(
                    "SELECT body FROM trace_events WHERE execution_id='execution' ORDER BY cursor",
                )
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            assert_eq!(bodies, after);
            c.execute(
                "DELETE FROM trace_events WHERE execution_id='execution' AND run_id='run'",
                [],
            )
            .unwrap();
            for table in [
                "trace_summary_sources",
                "trace_summary_spans",
                "trace_summary_runs",
            ] {
                let count: i64 = c
                    .query_row(
                        &format!("SELECT count(*) FROM {table} WHERE execution_id='execution'"),
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(count, 0);
            }
            let version: u32 = c
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 1);
        }
        assert!(s.read("execution", &q()).unwrap().sources.is_empty());
        assert_eq!(s.read("other", &q()).unwrap().events.len(), 1);
    }
    #[test]
    fn activity_summary_preserves_the_full_wire_timestamp_range() {
        let (_temp, s) = store();
        let mut e = event(1, "agent.activity");
        e.timestamp_ms = u64::MAX;
        s.record(e);
        let p = s.read("execution", &q()).unwrap();
        assert_eq!(p.sources[0].last_activity_timestamp_ms, Some(u64::MAX));
    }
}
