//! Rebuildable metadata projection; event payloads remain the immutable source.
use super::*;

// The same statements are used for one-time backfill and atomic INSERT triggers.
const SOURCE: &str = "INSERT INTO trace_summary_sources(execution_id,run_id,source_id,last_sequence,observed,terminal,activity_sequence,activity_timestamp)
VALUES(json_extract(?1,'$.execution_id'),json_extract(?1,'$.run_id'),json_extract(?1,'$.source_id'),json_extract(?1,'$.source_sequence'),1,
 json_extract(?1,'$.kind') IN ('phase.finished','execution.finished'),
 CASE WHEN json_extract(?1,'$.kind') IN ('agent.activity','model.requested','model.responded') THEN json_extract(?1,'$.source_sequence') END,
 CASE WHEN json_extract(?1,'$.kind') IN ('agent.activity','model.requested','model.responded') THEN ?1 -> '$.timestamp_ms' END)
ON CONFLICT(execution_id,run_id,source_id) DO UPDATE SET
 last_sequence=max(last_sequence,excluded.last_sequence),observed=observed+1,terminal=max(terminal,excluded.terminal),
 activity_timestamp=CASE WHEN activity_sequence IS NULL OR excluded.activity_sequence>activity_sequence THEN excluded.activity_timestamp ELSE activity_timestamp END,
 activity_sequence=CASE WHEN activity_sequence IS NULL OR excluded.activity_sequence>activity_sequence THEN excluded.activity_sequence ELSE activity_sequence END;";
const SPAN: &str = "INSERT INTO trace_summary_spans(execution_id,run_id,span_id,kind,ended)
SELECT json_extract(?1,'$.execution_id'),json_extract(?1,'$.run_id'),json_extract(?1,'$.span_id'),
 CASE WHEN json_extract(?1,'$.kind') IN ('phase.started','execution.started','tool.started','tool.requested','check.started') THEN json_extract(?1,'$.kind') END,
 CASE WHEN json_extract(?1,'$.kind') IN ('phase.finished','execution.finished','tool.finished','check.finished') OR (json_extract(?1,'$.kind')='tool.delivery' AND json_extract(?1,'$.data.outcome')='delivered') THEN 1 ELSE 0 END
WHERE json_extract(?1,'$.kind') IN ('phase.started','execution.started','tool.started','tool.requested','check.started','phase.finished','execution.finished','tool.finished','check.finished') OR (json_extract(?1,'$.kind')='tool.delivery' AND json_extract(?1,'$.data.outcome')='delivered')
ON CONFLICT(execution_id,run_id,span_id) DO UPDATE SET kind=coalesce(excluded.kind,kind),ended=max(ended,excluded.ended);";
const RUN: &str = "INSERT INTO trace_summary_runs(execution_id,run_id,expected,unavailable,adapter_failed,status,finalized)
VALUES(json_extract(?1,'$.execution_id'),json_extract(?1,'$.run_id'),
 CASE WHEN json_extract(?1,'$.kind')='executor.capabilities' AND json_extract(?1,'$.data.internal_observation')='available' THEN 1 ELSE 0 END,
 CASE WHEN json_extract(?1,'$.kind')='executor.capabilities' AND json_extract(?1,'$.data.internal_observation')='unavailable' THEN 1 ELSE 0 END,
 CASE WHEN json_extract(?1,'$.kind')='source.status' AND json_extract(?1,'$.data.adapter_delivery_failed')=1 THEN 1 ELSE 0 END,
 CASE WHEN json_extract(?1,'$.kind')='source.status' AND json_extract(?1,'$.data.kind') IS NULL THEN json_extract(?1,'$.data') END,
 CASE WHEN json_extract(?1,'$.kind')='source.status' AND json_extract(?1,'$.data.kind')='adapter.summary' THEN 1 ELSE 0 END)
ON CONFLICT(execution_id,run_id) DO UPDATE SET expected=max(expected,excluded.expected),unavailable=max(unavailable,excluded.unavailable),adapter_failed=max(adapter_failed,excluded.adapter_failed),status=coalesce(excluded.status,status),finalized=max(finalized,excluded.finalized);";

pub(super) struct Summary {
    pub sources: Vec<TraceSource>,
    pub open_spans: Vec<OpenSpan>,
    pub issues: BTreeSet<String>,
}
impl TraceStore {
    pub(super) fn migrate_summary(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS trace_summary_meta(version INTEGER NOT NULL);",
        )?;
        let version: Option<u32> = conn
            .query_row("SELECT version FROM trace_summary_meta LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        if version.is_some_and(|v| v > 2) {
            return Err(Error::new(
                "storage_incompatible",
                "/trace",
                "unsupported trace summary version",
            ));
        }
        let present: i64=conn.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('trace_summary_sources','trace_summary_spans','trace_summary_runs','trace_summary_insert','trace_summary_delete')",[],|r|r.get(0))?;
        if version == Some(2) && present == 5 {
            return Ok(());
        }
        conn.execute_batch("BEGIN IMMEDIATE;
          DROP TRIGGER IF EXISTS trace_summary_insert; DROP TRIGGER IF EXISTS trace_summary_delete;
          DROP TABLE IF EXISTS trace_summary_sources; DROP TABLE IF EXISTS trace_summary_spans; DROP TABLE IF EXISTS trace_summary_runs;
          CREATE TABLE IF NOT EXISTS trace_summary_sources(execution_id TEXT NOT NULL,run_id TEXT NOT NULL,source_id TEXT NOT NULL,last_sequence INTEGER NOT NULL,observed INTEGER NOT NULL,terminal INTEGER NOT NULL,activity_sequence INTEGER,activity_timestamp TEXT,PRIMARY KEY(execution_id,run_id,source_id));
          CREATE TABLE IF NOT EXISTS trace_summary_spans(execution_id TEXT NOT NULL,run_id TEXT NOT NULL,span_id TEXT NOT NULL,kind TEXT,ended INTEGER NOT NULL,PRIMARY KEY(execution_id,run_id,span_id));
          CREATE INDEX IF NOT EXISTS trace_summary_open ON trace_summary_spans(execution_id,run_id) WHERE kind IS NOT NULL AND ended=0;
          CREATE TABLE IF NOT EXISTS trace_summary_runs(execution_id TEXT NOT NULL,run_id TEXT NOT NULL,expected INTEGER NOT NULL,unavailable INTEGER NOT NULL,adapter_failed INTEGER NOT NULL,status TEXT,finalized INTEGER NOT NULL,PRIMARY KEY(execution_id,run_id));
          CREATE INDEX IF NOT EXISTS trace_run_read ON trace_events(execution_id,run_id,cursor);
          DELETE FROM trace_summary_sources; DELETE FROM trace_summary_spans; DELETE FROM trace_summary_runs;")?;
        let rebuilt = (|| -> Result<()> {
            let mut stmt = conn.prepare("SELECT body FROM trace_events ORDER BY cursor")?;
            for body in stmt.query_map([], |r| r.get::<_, String>(0))? {
                let body = body?;
                for sql in [SOURCE, SPAN, RUN] {
                    conn.prepare_cached(sql)?.execute([&body])?;
                }
            }
            conn.execute_batch(&format!("CREATE TRIGGER trace_summary_insert AFTER INSERT ON trace_events BEGIN {} {} {} END;
              CREATE TRIGGER trace_summary_delete AFTER DELETE ON trace_events WHEN NOT EXISTS(SELECT 1 FROM trace_events WHERE execution_id=OLD.execution_id AND run_id=OLD.run_id) BEGIN
                DELETE FROM trace_summary_sources WHERE execution_id=OLD.execution_id AND run_id=OLD.run_id;
                DELETE FROM trace_summary_spans WHERE execution_id=OLD.execution_id AND run_id=OLD.run_id;
                DELETE FROM trace_summary_runs WHERE execution_id=OLD.execution_id AND run_id=OLD.run_id;
              END;
              DELETE FROM trace_summary_meta; INSERT INTO trace_summary_meta VALUES(2);",SOURCE.replace("?1","NEW.body"),SPAN.replace("?1","NEW.body"),RUN.replace("?1","NEW.body")))?;
            Ok(())
        })();
        match rebuilt {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
    pub(super) fn event_page(
        conn: &Connection,
        execution: &str,
        q: &TraceQuery,
    ) -> Result<Vec<TraceItem>> {
        let sql = if q.run_id.is_some() {
            "SELECT cursor,body FROM trace_events WHERE execution_id=?1 AND run_id=?2 AND cursor>?3 ORDER BY cursor LIMIT ?4"
        } else {
            "SELECT cursor,body FROM trace_events WHERE execution_id=?1 AND ?2 IS NULL AND cursor>?3 ORDER BY cursor LIMIT ?4"
        };
        let mut stmt = conn.prepare(sql)?;
        let mut events = vec![];
        for row in stmt.query_map(params![execution, q.run_id, q.after as i64, q.limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })? {
            let (cursor, body) = row?;
            let event = serde_json::from_str(&body).map_err(|_| {
                Error::new("trace_corrupt", "/trace", "invalid persisted trace record")
            })?;
            events.push(TraceItem {
                cursor: cursor as u64,
                event,
            });
        }
        Ok(events)
    }
    pub(super) fn read_summary(
        conn: &Connection,
        execution: &str,
        q: &TraceQuery,
    ) -> Result<Summary> {
        let mut issues = BTreeSet::new();
        let mut expected = BTreeSet::new();
        let mut statuses = BTreeMap::<String, Value>::new();
        let mut stmt=conn.prepare("SELECT run_id,expected,unavailable,adapter_failed,status,finalized FROM trace_summary_runs WHERE execution_id=?1 AND (?2 IS NULL OR run_id=?2)")?;
        for row in stmt.query_map(params![execution, q.run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, bool>(5)?,
            ))
        })? {
            let (run, wanted, unavailable, failed, status, finalized) = row?;
            if wanted && !finalized {
                issues.insert(format!("observation_pending:{run}"));
            }
            if wanted {
                expected.insert(run.clone());
            }
            if unavailable {
                issues.insert(format!("internal_unavailable:{run}"));
            }
            if failed {
                issues.insert(format!("adapter_delivery_failed:{run}"));
            }
            if let Some(status) = status {
                let v: Value = serde_json::from_str(&status).map_err(|_| {
                    Error::new("trace_corrupt", "/trace", "invalid persisted trace summary")
                })?;
                if v["state"] == "incomplete"
                    || v["fault"].as_str().is_some()
                    || v["dropped"].as_u64().is_some_and(|n| n > 0)
                {
                    issues.insert(format!("source_incomplete:{run}"));
                }
                statuses.insert(run, v);
            }
        }
        let mut sources = vec![];
        let mut stmt=conn.prepare("SELECT run_id,source_id,last_sequence,observed,terminal,activity_sequence,activity_timestamp FROM trace_summary_sources WHERE execution_id=?1 AND (?2 IS NULL OR run_id=?2) ORDER BY run_id,source_id")?;
        for row in stmt.query_map(params![execution, q.run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })? {
            let (run, source, last, observed, terminal, activity, time) = row?;
            let reported = if source.starts_with("milkie:") {
                statuses
                    .get(&run)
                    .and_then(|v| v["lastSequence"].as_u64())
                    .unwrap_or(0)
            } else {
                0
            };
            let last = (last as u64).max(reported);
            let missing = last.saturating_sub(observed as u64);
            if missing > 0 {
                issues.insert(format!("sequence_gap:{run}:{source}"));
            }
            sources.push(TraceSource {
                run_id: run,
                source_id: source,
                last_sequence: last,
                missing_sequences: missing,
                terminal,
                last_activity_sequence: activity.map(|n| n as u64),
                last_activity_timestamp_ms: time.map(|n| n.parse::<u64>()).transpose().map_err(
                    |_| {
                        Error::new(
                            "trace_corrupt",
                            "/trace",
                            "invalid activity timestamp summary",
                        )
                    },
                )?,
            });
        }
        for run in expected {
            if !sources
                .iter()
                .any(|s| s.run_id == run && s.source_id.starts_with("milkie:"))
            {
                issues.insert(format!("source_unavailable:{run}"));
            }
        }
        let mut stmt=conn.prepare("SELECT run_id,span_id,kind FROM trace_summary_spans WHERE execution_id=?1 AND (?2 IS NULL OR run_id=?2) AND kind IS NOT NULL AND ended=0 ORDER BY run_id,span_id")?;
        let open_spans = stmt
            .query_map(params![execution, q.run_id], |r| {
                Ok(OpenSpan {
                    run_id: r.get(0)?,
                    span_id: r.get(1)?,
                    kind: r.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Summary {
            sources,
            open_spans,
            issues,
        })
    }
}
