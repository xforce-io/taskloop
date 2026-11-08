//! Persisted trace projection only. Opening this reader never migrates or maintains Trace storage.
use super::*;

pub(crate) fn read(root: &Path, execution: &str, query: &TraceQuery) -> Result<TracePage> {
    validate_query(query)?;
    let db = root.join("trace.sqlite");
    if !db.exists() {
        return disabled(query);
    }
    let mut conn = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let tx = conn.transaction()?;
    let version: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let summary: Option<u32> = tx
        .query_row("SELECT version FROM trace_summary_meta LIMIT 1", [], |r| {
            r.get(0)
        })
        .optional()?;
    if version != 3 || !matches!(summary, Some(2 | 3)) {
        return Err(Error::new(
            "storage_incompatible",
            "/trace",
            "read-only trace projection requires a supported migrated schema",
        ));
    }
    let events = TraceStore::event_page(&tx, execution, query)?;
    let summary::Summary {
        sources,
        open_spans,
        mut issues,
    } = TraceStore::read_summary(&tx, execution, query)?;
    let (run_policies, expired_runs) =
        TraceStore::query_policies_from(&tx, execution, query.run_id.as_deref())?;
    let source_cleanup = TraceStore::cleanup_view_from(&tx, execution, query.run_id.as_deref())?;
    for run in &expired_runs {
        issues.insert(format!("retention_expired:{run}"));
    }
    let collector_degraded: bool = tx.query_row(
        "SELECT value>0 FROM trace_health WHERE key='dropped'",
        [],
        |r| r.get(0),
    )?;
    let unflushed: bool = tx.query_row(
        "SELECT value>0 FROM trace_health WHERE key='boot_dirty'",
        [],
        |r| r.get(0),
    )?;
    if unflushed {
        issues.insert("collector_not_flushed".into());
    }
    let mut statement = tx.prepare("SELECT run_id,reason FROM trace_faults WHERE execution_id=?1 AND (?2 IS NULL OR run_id=?2)")?;
    for result in statement.query_map(params![execution, query.run_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (run, reason) = result?;
        issues.insert(format!("{reason}:{run}"));
    }
    if sources.is_empty() {
        issues.insert("not_recorded_or_expired".into());
    }
    let complete = !collector_degraded
        && !sources.is_empty()
        && sources.iter().all(|s| s.terminal)
        && open_spans.is_empty()
        && issues.is_empty();
    let next = events.last().map_or(query.after, |e| e.cursor);
    drop(statement);
    tx.commit()?;
    Ok(TracePage {
        schema_version: 1,
        semantic_version: 1,
        enabled: true,
        events,
        next,
        ordering: "reception_cursor; source_sequence_within_source_only".into(),
        complete,
        issues: issues.into_iter().collect(),
        sources,
        open_spans,
        policy: None,
        run_policies,
        expired_runs,
        collector_degraded,
        source_cleanup,
    })
}
