use super::*;
use crate::{ObservationCleanup, runtime::Inner};
use futures_util::FutureExt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCleanup {
    pub run_id: String,
    pub executor: Option<String>,
    pub state: String,
    pub last_attempt_ms: u64,
    pub next_attempt_ms: u64,
}
struct CleanupJob {
    execution: String,
    run: String,
    executor: String,
    identity: String,
}
impl TraceStore {
    pub(super) fn migrate_cleanup(conn: &Connection) -> Result<()> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS trace_cleanup(execution_id TEXT NOT NULL,run_id TEXT NOT NULL,executor TEXT NOT NULL,identity TEXT NOT NULL,state TEXT NOT NULL,last_ms INTEGER NOT NULL,next_ms INTEGER NOT NULL,PRIMARY KEY(execution_id,run_id));
          INSERT OR IGNORE INTO trace_cleanup SELECT execution_id,run_id,'','','unbound',0,0 FROM trace_runs WHERE expires_ms IS NOT NULL;
          PRAGMA user_version=3;")?;
        Ok(())
    }
    pub(super) fn cleanup_view(
        &self,
        execution: &str,
        run: Option<&str>,
    ) -> Result<Vec<SourceCleanup>> {
        let conn = self.conn.lock().map_err(Error::io)?;
        let mut stmt=conn.prepare("SELECT run_id,executor,state,last_ms,next_ms FROM trace_cleanup WHERE execution_id=?1 AND (?2 IS NULL OR run_id=?2) ORDER BY run_id")?;
        Ok(stmt
            .query_map(params![execution, run], |r| {
                let executor: String = r.get(1)?;
                Ok(SourceCleanup {
                    run_id: r.get(0)?,
                    executor: if executor.is_empty() {
                        None
                    } else {
                        Some(executor)
                    },
                    state: r.get(2)?,
                    last_attempt_ms: r.get::<_, i64>(3)? as u64,
                    next_attempt_ms: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    fn cleanup_due(&self) -> Result<Option<CleanupJob>> {
        let conn = self.conn.lock().map_err(Error::io)?;
        Ok(conn.query_row("SELECT c.execution_id,c.run_id,c.executor,c.identity FROM trace_cleanup c JOIN trace_runs r USING(execution_id,run_id) WHERE r.status='expired' AND c.executor!='' AND c.state NOT IN ('purged','not_applicable') AND c.next_ms<=?1 ORDER BY c.next_ms,c.run_id LIMIT 1",[crate::runtime::now_ms() as i64],|r|Ok(CleanupJob{execution:r.get(0)?,run:r.get(1)?,executor:r.get(2)?,identity:r.get(3)?})).optional()?)
    }
    fn cleanup_result(&self, job: &CleanupJob, state: &str) -> Result<()> {
        let now = crate::runtime::now_ms() as i64;
        self.conn.lock().map_err(Error::io)?.execute("UPDATE trace_cleanup SET state=?1,last_ms=?2,next_ms=?3 WHERE execution_id=?4 AND run_id=?5",params![state,now,now.saturating_add(1000),job.execution,job.run])?;
        Ok(())
    }
}
impl RunTrace {
    pub fn reserve_executor(&self, name: &str, identity: &Value) -> bool {
        let reserved = (|| -> Result<()> {
            let conn = self.store.conn.lock().map_err(Error::io)?;
            self.store
                .admit(&conn, &self.execution, &self.run, self.observed_ms)?;
            if let Err(error) = self.store.global_capacity(&conn, 1, true) {
                if self.store.policy.is_some() {
                    conn.execute("INSERT OR IGNORE INTO trace_cleanup VALUES(?1,?2,'','','not_applicable',0,0)",params![self.execution,self.run])?;
                }
                return Err(error);
            }
            if self.store.policy.is_some() {
                let identity = crate::json::digest(&serde_json::to_vec(identity)?);
                conn.execute("INSERT INTO trace_cleanup VALUES(?1,?2,?3,?4,'scheduled',0,0) ON CONFLICT(execution_id,run_id) DO NOTHING",params![self.execution,self.run,name,identity])?;
            }
            Ok(())
        })();
        if let Err(e) = reserved {
            self.store.fault(
                &self.execution,
                &self.run,
                if e.code == "trace_capacity" {
                    "capacity"
                } else {
                    "admission_storage"
                },
            );
            false
        } else {
            true
        }
    }
}
pub(crate) async fn cleanup_loop(inner: Arc<Inner>) {
    let Some(trace) = inner.trace.as_ref() else {
        return;
    };
    loop {
        if inner.stop.is_cancelled() {
            return;
        }
        trace.background_maintenance();
        match trace.cleanup_due() {
            Ok(Some(job)) => {
                let operation = async {
                    let Ok(row) = inner.store.load(&job.execution) else {
                        return "binding_unavailable";
                    };
                    let Ok(record) = crate::runtime::decode(&row) else {
                        return "binding_unavailable";
                    };
                    // Expiry is not retirement: an admitted/starting invocation can
                    // still create a source after its observation deadline passes.
                    let active = record.active.as_ref().is_some_and(|r| r.id == job.run);
                    let settled = record.attempts.iter().any(|a| {
                        a.run_id == job.run
                            && !matches!(a.outcome, crate::ExecutionOutcome::StopUnknown { .. })
                    });
                    if active && !settled || record.stop_unknown && record.active.is_none() {
                        return "deferred";
                    }
                    let Some(executor) = inner.bindings.executors.get(&job.executor) else {
                        return "binding_unavailable";
                    };
                    let identity =
                        serde_json::to_vec(&executor.identity()).map(|b| crate::json::digest(&b));
                    if identity.ok().as_deref() != Some(job.identity.as_str()) {
                        return "binding_changed";
                    }
                    match executor.cleanup_observation(&job.run).await {
                        ObservationCleanup::Purged => "purged",
                        ObservationCleanup::NotApplicable => "not_applicable",
                        ObservationCleanup::Deferred => "deferred",
                        ObservationCleanup::Unsupported => "unsupported",
                        ObservationCleanup::Failed => "failed",
                    }
                };
                let state = tokio::select! {
                    _=inner.stop.cancelled()=>return,
                    result=tokio::time::timeout(std::time::Duration::from_secs(5),std::panic::AssertUnwindSafe(operation).catch_unwind())=>match result{Ok(Ok(state))=>state,_=>"failed"}
                };
                if trace.cleanup_result(&job, state).is_err() {
                    trace.degraded.store(true, Ordering::Release);
                }
            }
            Err(_) => {
                trace.degraded.store(true, Ordering::Release);
            }
            Ok(None) => {}
        }
        tokio::select! {_=inner.stop.cancelled()=>return,_=tokio::time::sleep(std::time::Duration::from_millis(500))=>{}}
    }
}
