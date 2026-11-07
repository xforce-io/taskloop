use super::*;
use crate::{TraceContent, TraceContentRef, TracePolicySnapshot};
use futures_util::FutureExt;

impl TraceStore {
    pub(super) fn migrate_policy(conn: &Connection, previous: u32) -> Result<()> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS trace_runs(execution_id TEXT NOT NULL,run_id TEXT NOT NULL,created_ms INTEGER NOT NULL,expires_ms INTEGER,policy TEXT NOT NULL,status TEXT NOT NULL,PRIMARY KEY(execution_id,run_id));
          CREATE INDEX IF NOT EXISTS trace_expiry ON trace_runs(expires_ms) WHERE status='active';
          CREATE TABLE IF NOT EXISTS trace_content(id TEXT PRIMARY KEY,execution_id TEXT NOT NULL,run_id TEXT NOT NULL,bytes INTEGER NOT NULL,reference TEXT NOT NULL,body TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS trace_content_run ON trace_content(execution_id,run_id);
          CREATE TABLE IF NOT EXISTS trace_health(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
          INSERT OR IGNORE INTO trace_health VALUES('boot_dirty',0),('dropped',0),('expired',0);
          CREATE TABLE IF NOT EXISTS trace_global(id INTEGER PRIMARY KEY CHECK(id=1),events INTEGER NOT NULL,bytes INTEGER NOT NULL,content_bytes INTEGER NOT NULL,runs INTEGER NOT NULL);
          INSERT OR REPLACE INTO trace_global SELECT 1,(SELECT count(*) FROM trace_events),(SELECT coalesce(sum(bytes),0) FROM trace_events),(SELECT coalesce(sum(bytes),0) FROM trace_content),(SELECT count(*) FROM trace_runs);
          CREATE TRIGGER IF NOT EXISTS trace_global_event_insert AFTER INSERT ON trace_events BEGIN UPDATE trace_global SET events=events+1,bytes=bytes+NEW.bytes WHERE id=1; END;
          CREATE TRIGGER IF NOT EXISTS trace_global_event_delete AFTER DELETE ON trace_events BEGIN UPDATE trace_global SET events=events-1,bytes=bytes-OLD.bytes WHERE id=1; END;
          CREATE TRIGGER IF NOT EXISTS trace_global_content_insert AFTER INSERT ON trace_content BEGIN UPDATE trace_global SET content_bytes=content_bytes+NEW.bytes WHERE id=1; END;
          CREATE TRIGGER IF NOT EXISTS trace_global_content_delete AFTER DELETE ON trace_content BEGIN UPDATE trace_global SET content_bytes=content_bytes-OLD.bytes WHERE id=1; END;
          CREATE TRIGGER IF NOT EXISTS trace_global_run_insert AFTER INSERT ON trace_runs BEGIN UPDATE trace_global SET runs=runs+1 WHERE id=1; END;
          CREATE TRIGGER IF NOT EXISTS trace_global_run_delete AFTER DELETE ON trace_runs BEGIN UPDATE trace_global SET runs=runs-1 WHERE id=1; END;")?;
        if previous < 2 {
            let legacy = serde_json::to_string(&TracePolicySnapshot::legacy())?;
            conn.execute("INSERT OR IGNORE INTO trace_runs SELECT DISTINCT execution_id,run_id,0,NULL,?1,'legacy' FROM trace_events",[legacy])?;
        }
        let dirty: i64 = conn.query_row(
            "SELECT value FROM trace_health WHERE key='boot_dirty'",
            [],
            |r| r.get(0),
        )?;
        conn.execute("UPDATE trace_health SET value=1 WHERE key='boot_dirty'", [])?;

        if dirty != 0 {
            conn.execute("INSERT OR IGNORE INTO trace_faults SELECT execution_id,run_id,'host_unflushed' FROM trace_runs WHERE status!='expired'",[])?;
        }
        Ok(())
    }
    pub(super) fn admit(
        &self,
        conn: &Connection,
        execution: &str,
        run: &str,
        observed_ms: u64,
    ) -> Result<()> {
        let now = crate::runtime::now_ms() as i64;
        let existing: Option<(String, Option<i64>)> = conn
            .query_row(
                "SELECT status,expires_ms FROM trace_runs WHERE execution_id=?1 AND run_id=?2",
                params![execution, run],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((status, expires)) = existing {
            if status == "expired" || expires.is_some_and(|n| n <= now) {
                return Err(Error::new(
                    "trace_expired",
                    "/trace",
                    "trace retention expired",
                ));
            }
            return Ok(());
        }
        if self
            .policy
            .as_ref()
            .is_some_and(|p| observed_ms.saturating_add(p.retention_ms) <= now as u64)
        {
            return Err(Error::new(
                "trace_expired",
                "/trace",
                "trace retention expired",
            ));
        }
        let snapshot = self
            .policy
            .as_ref()
            .map_or_else(TracePolicySnapshot::legacy, |p| p.snapshot());
        if let Some(policy) = &self.policy {
            let count: i64 =
                conn.query_row("SELECT runs FROM trace_global WHERE id=1", [], |r| r.get(0))?;
            if count >= policy.max_runs as i64 {
                // A lowered policy may require evicting multiple confirmed tombstones.
                // Batch the bounded cleanup instead of scanning once per record.
                let needed = count
                    .saturating_sub(policy.max_runs as i64)
                    .saturating_add(1);
                conn.execute("DELETE FROM trace_faults WHERE (execution_id,run_id) IN (SELECT execution_id,run_id FROM trace_runs WHERE status='expired' AND NOT EXISTS(SELECT 1 FROM trace_cleanup c WHERE c.execution_id=trace_runs.execution_id AND c.run_id=trace_runs.run_id AND c.state NOT IN ('purged','not_applicable')) LIMIT ?1)",[needed])?;
                conn.execute("DELETE FROM trace_totals WHERE (execution_id,run_id) IN (SELECT execution_id,run_id FROM trace_runs WHERE status='expired' AND NOT EXISTS(SELECT 1 FROM trace_cleanup c WHERE c.execution_id=trace_runs.execution_id AND c.run_id=trace_runs.run_id AND c.state NOT IN ('purged','not_applicable')) LIMIT ?1)",[needed])?;
                let removed=conn.execute("DELETE FROM trace_runs WHERE rowid IN (SELECT rowid FROM trace_runs WHERE status='expired' AND NOT EXISTS(SELECT 1 FROM trace_cleanup c WHERE c.execution_id=trace_runs.execution_id AND c.run_id=trace_runs.run_id AND c.state NOT IN ('purged','not_applicable')) LIMIT ?1)",[needed])?;
                conn.execute("DELETE FROM trace_cleanup WHERE NOT EXISTS(SELECT 1 FROM trace_runs r WHERE r.execution_id=trace_cleanup.execution_id AND r.run_id=trace_cleanup.run_id)",[])?;
                let remaining: i64 =
                    conn.query_row("SELECT runs FROM trace_global WHERE id=1", [], |r| r.get(0))?;
                if removed == 0 || remaining >= policy.max_runs as i64 {
                    return Err(Error::new(
                        "trace_capacity",
                        "/trace",
                        "trace Run capacity exhausted",
                    ));
                }
            }
        }
        let expires = self
            .policy
            .as_ref()
            .map(|p| (observed_ms as i64).saturating_add(p.retention_ms as i64));
        conn.execute(
            "INSERT INTO trace_runs VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                execution,
                run,
                now,
                expires,
                serde_json::to_string(&snapshot)?,
                if self.policy.is_some() {
                    "active"
                } else {
                    "legacy"
                }
            ],
        )?;
        Ok(())
    }
    pub(super) fn global_capacity(
        &self,
        conn: &Connection,
        bytes: usize,
        event: bool,
    ) -> Result<()> {
        if let Some(policy) = &self.policy {
            let (events, total): (i64, i64) = conn.query_row(
                "SELECT events,bytes+content_bytes FROM trace_global WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if event && events >= policy.max_total_events as i64
                || (total as u64).saturating_add(bytes as u64) > policy.max_total_bytes
            {
                return Err(Error::new(
                    "trace_capacity",
                    "/trace",
                    "global trace payload capacity exhausted",
                ));
            }
        }
        Ok(())
    }
    pub fn maintain(&self) -> Result<usize> {
        let mut conn = self.conn.lock().map_err(Error::io)?;
        let tx = conn.transaction()?;
        let runs = {
            let mut stmt=tx.prepare("SELECT execution_id,run_id FROM trace_runs WHERE status='active' AND expires_ms<=?1 LIMIT 64")?;
            stmt.query_map([crate::runtime::now_ms() as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (execution, run) in &runs {
            tx.execute(
                "DELETE FROM trace_events WHERE execution_id=?1 AND run_id=?2",
                params![execution, run],
            )?;
            tx.execute(
                "DELETE FROM trace_content WHERE execution_id=?1 AND run_id=?2",
                params![execution, run],
            )?;
            tx.execute(
                "DELETE FROM trace_faults WHERE execution_id=?1 AND run_id=?2",
                params![execution, run],
            )?;
            tx.execute("UPDATE trace_cleanup SET state='pending',next_ms=0 WHERE execution_id=?1 AND run_id=?2 AND state='scheduled'",params![execution,run])?;
            tx.execute(
                "UPDATE trace_runs SET status='expired' WHERE execution_id=?1 AND run_id=?2",
                params![execution, run],
            )?;
        }
        tx.execute(
            "UPDATE trace_health SET value=min(value+?1,9223372036854775807) WHERE key='expired'",
            [runs.len() as i64],
        )?;
        tx.commit()?;
        if !runs.is_empty() {
            let mut faults = self.faults.lock().unwrap_or_else(|e| e.into_inner());
            faults.retain(|(execution, run, _)| {
                !runs.iter().any(|(e, r)| e == execution && r == run)
            });
        }
        Ok(runs.len())
    }
    pub fn read_content(&self, execution: &str, id: &str) -> Result<TraceContent> {
        let _ = self.maintain();
        let conn = self.conn.lock().map_err(Error::io)?;
        let raw:Option<(String,String)>=conn.query_row("SELECT c.reference,c.body FROM trace_content c JOIN trace_runs r USING(execution_id,run_id) WHERE c.execution_id=?1 AND c.id=?2 AND r.status!='expired' AND (r.expires_ms IS NULL OR r.expires_ms>?3)",params![execution,id,crate::runtime::now_ms() as i64],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (reference, body) = raw
            .ok_or_else(|| Error::new("not_found", "/trace/content", "trace content not found"))?;
        let reference: TraceContentRef = serde_json::from_str(&reference)?;
        if crate::json::digest(body.as_bytes()) != reference.sha256
            || body.len() as u64 != reference.bytes
        {
            return Err(Error::new(
                "trace_corrupt",
                "/trace/content",
                "trace content digest mismatch",
            ));
        }
        Ok(TraceContent {
            reference,
            value: crate::parse_unique(body.as_bytes())?,
        })
    }
    pub fn policy_snapshot(&self) -> TracePolicySnapshot {
        self.policy
            .as_ref()
            .map_or_else(TracePolicySnapshot::legacy, |p| p.snapshot())
    }
    pub(super) fn query_policies(
        &self,
        execution: &str,
        run: Option<&str>,
    ) -> Result<(BTreeMap<String, TracePolicySnapshot>, Vec<String>)> {
        let conn = self.conn.lock().map_err(Error::io)?;
        let mut stmt=conn.prepare("SELECT run_id,policy,status,expires_ms FROM trace_runs WHERE execution_id=?1 AND (?2 IS NULL OR run_id=?2)")?;
        let mut policies = BTreeMap::new();
        let mut expired = Vec::new();
        for row in stmt.query_map(params![execution, run], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })? {
            let (run, policy, status, expires) = row?;
            policies.insert(run.clone(), serde_json::from_str(&policy)?);
            if status == "expired" || expires.is_some_and(|n| n <= crate::runtime::now_ms() as i64)
            {
                expired.push(run);
            }
        }
        Ok((policies, expired))
    }
    pub fn background_maintenance(&self) {
        if self.maintain().is_err() {
            self.degraded.store(true, Ordering::Release);
        }
    }
    pub fn finish_observation(&self) -> Result<()> {
        if !self.degraded.load(Ordering::Acquire) {
            self.conn
                .lock()
                .map_err(Error::io)?
                .execute("UPDATE trace_health SET value=0 WHERE key='boot_dirty'", [])?;
        }
        Ok(())
    }
}
impl RunTrace {
    pub async fn content(
        &self,
        tool: &str,
        direction: &str,
        span: &str,
        value: &Value,
    ) -> Option<TraceContentRef> {
        let policy = self.store.policy.as_ref()?;
        let selected = policy.select(tool, direction, value)?;
        let limit = policy.max_content_bytes;
        if serde_json::to_vec(&selected).ok()?.len() as u64 > limit {
            self.store
                .fault(&self.execution, &self.run, "content_capacity");
            return None;
        }
        let Ok(_permit) = self.store.content_slots.clone().try_acquire_owned() else {
            self.store
                .fault(&self.execution, &self.run, "content_queue");
            return None;
        };
        let filter = policy.filter.as_ref()?;
        let filtered = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            std::panic::AssertUnwindSafe(filter.redact(tool, direction, selected)).catch_unwind(),
        )
        .await;
        let Ok(Ok(Ok(filtered))) = filtered else {
            self.store
                .fault(&self.execution, &self.run, "content_filter");
            return None;
        };
        let body = serde_json::to_string(&crate::trace_policy::redact_sensitive(filtered)).ok()?;
        if body.len() as u64 > limit {
            self.store
                .fault(&self.execution, &self.run, "content_capacity");
            return None;
        }
        let reference = TraceContentRef {
            id: format!("trace-content-{}", uuid::Uuid::new_v4()),
            run_id: self.run.clone(),
            span_id: span.into(),
            tool: tool.into(),
            direction: direction.into(),
            sha256: crate::json::digest(body.as_bytes()),
            bytes: body.len() as u64,
        };
        let saved = (|| -> Result<()> {
            let conn = self.store.conn.lock().map_err(Error::io)?;
            self.store
                .admit(&conn, &self.execution, &self.run, self.observed_ms)?;
            let used:i64=conn.query_row("SELECT (SELECT coalesce(sum(bytes),0) FROM trace_content WHERE execution_id=?1 AND run_id=?2)+(SELECT coalesce(sum(bytes),0) FROM trace_events WHERE execution_id=?1 AND run_id=?2)",params![self.execution,self.run],|r|r.get(0))?;
            if (used as u64).saturating_add(body.len() as u64)
                > self.store.options.max_bytes_per_run
            {
                return Err(Error::new(
                    "trace_capacity",
                    "/trace/content",
                    "Run trace payload capacity exhausted",
                ));
            }
            self.store.global_capacity(&conn, body.len(), false)?;
            conn.execute(
                "INSERT INTO trace_content VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    reference.id,
                    self.execution,
                    self.run,
                    body.len() as i64,
                    serde_json::to_string(&reference)?,
                    body
                ],
            )?;
            Ok(())
        })();
        if let Err(error) = saved {
            self.store.fault(
                &self.execution,
                &self.run,
                match error.code.as_str() {
                    "trace_capacity" => "content_capacity",
                    "trace_expired" => "retention_expired",
                    _ => "content_storage",
                },
            );
            None
        } else {
            Some(reference)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options() -> TraceOptions {
        TraceOptions {
            max_events_per_run: 100,
            max_bytes_per_run: 65536,
            source_max_events: 100,
            source_max_bytes: 65536,
        }
    }
    fn policy() -> crate::TracePolicy {
        crate::TracePolicy {
            policy_id: "unit".into(),
            max_total_events: 100,
            max_total_bytes: 65536,
            max_runs: 1,
            retention_ms: 20,
            max_content_bytes: 0,
            content_tools: BTreeMap::new(),
            filter_version: None,
            filter: None,
        }
    }
    #[tokio::test]
    async fn expired_tombstone_eviction_never_revives_late_events() {
        let temp = tempfile::tempdir().unwrap();
        let s = TraceStore::open_with_policy(temp.path(), options(), Some(policy())).unwrap();
        let old = s.session("e", "old", "work", 1);
        old.emit("phase.started", &old.phase_span(), None, json!({}));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        s.maintain().unwrap();
        let new = s.session("e", "new", "work", 1);
        new.emit("phase.started", &new.phase_span(), None, json!({}));
        old.emit("phase.finished", &old.phase_span(), None, json!({}));
        let q = TraceQuery {
            run_id: Some("old".into()),
            limit: 100,
            ..Default::default()
        };
        let p = s.read("e", &q).unwrap();
        assert!(p.events.is_empty());
        assert!(!p.complete);
        assert_eq!(
            s.conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM trace_runs WHERE run_id='old'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
    #[tokio::test]
    async fn content_queue_is_bounded_and_overload_is_observable() {
        struct Hold {
            started: std::sync::atomic::AtomicUsize,
            release: tokio::sync::Notify,
        }
        #[async_trait]
        impl crate::TraceContentFilter for Hold {
            async fn redact(&self, _: &str, _: &str, value: Value) -> Result<Value> {
                self.started.fetch_add(1, Ordering::AcqRel);
                self.release.notified().await;
                Ok(value)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let filter = Arc::new(Hold {
            started: std::sync::atomic::AtomicUsize::new(0),
            release: tokio::sync::Notify::new(),
        });
        let mut p = policy();
        p.retention_ms = 60000;
        p.max_content_bytes = 4096;
        p.content_tools.insert(
            "tool".into(),
            crate::TraceToolContent {
                input: vec!["/value".into()],
                output: vec![],
            },
        );
        p.filter = Some(filter.clone());
        p.filter_version = Some("hold-v1".into());
        let s = TraceStore::open_with_policy(temp.path(), options(), Some(p)).unwrap();
        let t = s.session("e", "r", "work", 1);
        t.emit("phase.started", &t.phase_span(), None, json!({}));
        let mut jobs = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let t = t.clone();
            jobs.spawn(async move {
                t.content("tool", "input", "span", &json!({"value":"safe"}))
                    .await
            });
        }
        for _ in 0..100 {
            if filter.started.load(Ordering::Acquire) == 8 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(filter.started.load(Ordering::Acquire), 8);
        assert!(
            t.content("tool", "input", "span", &json!({"value":"overflow"}))
                .await
                .is_none()
        );
        filter.release.notify_waiters();
        while let Some(result) = jobs.join_next().await {
            assert!(result.unwrap().is_some());
        }
        let p = s
            .read(
                "e",
                &TraceQuery {
                    limit: 100,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(p.issues.iter().any(|e| e.starts_with("content_queue:")));
    }
    #[test]
    fn unflushed_boot_is_persisted_per_run_and_not_erased_by_clean_reopen() {
        let temp = tempfile::tempdir().unwrap();
        {
            let s = TraceStore::open(temp.path(), options()).unwrap();
            let t = s.session("e", "r", "work", 1);
            t.emit("phase.started", &t.phase_span(), None, json!({}));
            t.emit("phase.finished", &t.phase_span(), None, json!({}));
        }
        {
            let s = TraceStore::open(temp.path(), options()).unwrap();
            let p = s
                .read(
                    "e",
                    &TraceQuery {
                        limit: 100,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(p.issues.iter().any(|e| e.starts_with("host_unflushed:")));
            assert!(!p.complete);
            s.finish_observation().unwrap();
        }
        let s = TraceStore::open(temp.path(), options()).unwrap();
        let p = s
            .read(
                "e",
                &TraceQuery {
                    limit: 100,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(p.issues.iter().any(|e| e.starts_with("host_unflushed:")));
        assert!(!p.complete);
    }
    #[test]
    fn reduced_run_quota_evicts_enough_settled_records_or_refuses_admission() {
        for pending in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut old = policy();
            old.max_runs = 4;
            old.retention_ms = 20;
            {
                let s = TraceStore::open_with_policy(temp.path(), options(), Some(old)).unwrap();
                for i in 0..4 {
                    let t = s.session("e", &format!("r{i}"), "work", 1);
                    assert!(t.reserve_executor("test", &json!({"fixture":true})));
                    t.emit("phase.started", &t.phase_span(), None, json!({}));
                }
                s.finish_observation().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
            let mut new = policy();
            new.retention_ms = 60000;
            let s = TraceStore::open_with_policy(temp.path(), options(), Some(new)).unwrap();
            s.maintain().unwrap();
            s.conn
                .lock()
                .unwrap()
                .execute("UPDATE trace_cleanup SET state='not_applicable'", [])
                .unwrap();
            if pending {
                s.conn
                    .lock()
                    .unwrap()
                    .execute(
                        "UPDATE trace_cleanup SET state='deferred' WHERE run_id='r3'",
                        [],
                    )
                    .unwrap();
            }
            let t = s.session("e", "new", "work", 1);
            let admitted = t.reserve_executor("test", &json!({"fixture":true}));
            assert_eq!(admitted, !pending);
            let count: i64 = s
                .conn
                .lock()
                .unwrap()
                .query_row("SELECT runs FROM trace_global WHERE id=1", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1);
        }
    }
    #[test]
    fn v2_migration_does_not_invent_missing_executor_binding() {
        let temp = tempfile::tempdir().unwrap();
        let mut p = policy();
        p.retention_ms = 60000;
        {
            let s = TraceStore::open_with_policy(temp.path(), options(), Some(p.clone())).unwrap();
            let t = s.session("e", "r", "work", 1);
            t.emit("phase.started", &t.phase_span(), None, json!({}));
            s.finish_observation().unwrap();
        }
        let c = Connection::open(temp.path().join("trace.sqlite")).unwrap();
        c.execute_batch("DROP TABLE trace_cleanup; PRAGMA user_version=2;")
            .unwrap();
        drop(c);
        let s = TraceStore::open_with_policy(temp.path(), options(), Some(p)).unwrap();
        let p = s
            .read(
                "e",
                &TraceQuery {
                    limit: 100,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(p.source_cleanup.len(), 1);
        assert_eq!(p.source_cleanup[0].state, "unbound");
        assert!(p.source_cleanup[0].executor.is_none());
    }
    #[test]
    fn v1_migration_retains_events_as_legacy_without_fabricating_content_or_start() {
        let temp = tempfile::tempdir().unwrap();
        {
            let s = TraceStore::open(temp.path(), options()).unwrap();
            let t = s.session("e", "r", "work", 1);
            t.emit("phase.started", &t.phase_span(), None, json!({}));
            s.finish_observation().unwrap();
        }
        let c = Connection::open(temp.path().join("trace.sqlite")).unwrap();
        c.execute_batch("DROP TRIGGER trace_global_run_insert; DROP TRIGGER trace_global_run_delete; DROP TABLE trace_runs; PRAGMA user_version=1;").unwrap();
        drop(c);
        let s = TraceStore::open(temp.path(), options()).unwrap();
        let p = s
            .read(
                "e",
                &TraceQuery {
                    limit: 100,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(p.events.len(), 1);
        assert_eq!(p.run_policies["r"].policy_id, "legacy-metadata-manual");
        assert_eq!(
            s.conn
                .lock()
                .unwrap()
                .query_row(
                    "SELECT created_ms FROM trace_runs WHERE run_id='r'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}
