use crate::{Error, Result};
use rusqlite::{Connection, params};
use serde_json::Value;

const BASE:&str="
CREATE TABLE executions(id TEXT PRIMARY KEY,owner TEXT NOT NULL,request_key TEXT NOT NULL,fingerprint TEXT NOT NULL,revision INTEGER NOT NULL,queue_order INTEGER NOT NULL,body TEXT NOT NULL,UNIQUE(owner,request_key));
CREATE INDEX active_queue ON executions(queue_order,id) WHERE json_extract(body,'$.status') NOT IN ('completed','failed','cancelled');
CREATE TABLE commands(execution_id TEXT NOT NULL,kind TEXT NOT NULL,request_key TEXT NOT NULL,fingerprint TEXT NOT NULL,response TEXT NOT NULL,PRIMARY KEY(execution_id,kind,request_key));
CREATE TABLE artifacts(id TEXT PRIMARY KEY,execution_id TEXT NOT NULL,sha256 TEXT NOT NULL,metadata TEXT NOT NULL);
CREATE TABLE definitions(digest TEXT PRIMARY KEY,body TEXT NOT NULL);
CREATE TABLE meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
INSERT INTO meta(key,value) VALUES('queue',0);";
const GOAL:&str="
CREATE TABLE run_leases(run_id TEXT PRIMARY KEY,owner_kind TEXT NOT NULL CHECK(owner_kind IN ('flow','goal')),owner_id TEXT NOT NULL,epoch INTEGER NOT NULL CHECK(epoch>=0),executor TEXT NOT NULL,binding TEXT NOT NULL,phase TEXT NOT NULL CHECK(phase IN ('queued','reserved','suspended','started','stop_unknown','settled')),slot_held INTEGER NOT NULL CHECK(slot_held IN (0,1)),started INTEGER NOT NULL CHECK(started IN (0,1)),native_stop_confirmed INTEGER NOT NULL CHECK(native_stop_confirmed IN (0,1)),settlement_seq INTEGER NOT NULL CHECK(settlement_seq>=0),budget_ref TEXT NOT NULL);
CREATE INDEX held_run_leases ON run_leases(owner_kind,owner_id) WHERE slot_held=1;
CREATE TABLE goals(id TEXT PRIMARY KEY,owner TEXT NOT NULL,request_key TEXT NOT NULL,fingerprint TEXT NOT NULL,revision INTEGER NOT NULL,queue_order INTEGER NOT NULL,body TEXT NOT NULL,UNIQUE(owner,request_key));
CREATE INDEX active_goals ON goals(queue_order,id) WHERE json_extract(body,'$.status')!='finished';
CREATE TABLE goal_budget(goal_id TEXT PRIMARY KEY,body TEXT NOT NULL);
CREATE TABLE goal_plans(goal_id TEXT NOT NULL,digest TEXT NOT NULL,bytes BLOB NOT NULL,accepted INTEGER NOT NULL CHECK(accepted IN (0,1)),metadata TEXT NOT NULL,PRIMARY KEY(goal_id,digest));
CREATE UNIQUE INDEX one_locked_goal_plan ON goal_plans(goal_id) WHERE accepted=1;
CREATE TABLE goal_command_receipts(owner TEXT NOT NULL,goal_id TEXT NOT NULL,kind TEXT NOT NULL,request_key TEXT NOT NULL,fingerprint TEXT NOT NULL,response TEXT NOT NULL,applied_revision INTEGER NOT NULL,PRIMARY KEY(owner,goal_id,kind,request_key));
CREATE TABLE goal_artifacts(id TEXT PRIMARY KEY,goal_id TEXT NOT NULL,node_id TEXT,member_id TEXT,plan_digest TEXT,schema_digest TEXT NOT NULL,sha256 TEXT NOT NULL,metadata TEXT NOT NULL);
CREATE TABLE goal_checks(evaluation_id TEXT PRIMARY KEY,goal_id TEXT NOT NULL,request TEXT NOT NULL,candidate_digest TEXT,receipt TEXT,accepted_revision INTEGER);
CREATE INDEX checks_by_goal ON goal_checks(goal_id);
INSERT OR IGNORE INTO meta(key,value) VALUES('last_observed_utc_ms',0);";
const BASE_COLUMNS: &[(&str, &[&str])] = &[
    (
        "executions",
        &[
            "id",
            "owner",
            "request_key",
            "fingerprint",
            "revision",
            "queue_order",
            "body",
        ],
    ),
    (
        "commands",
        &[
            "execution_id",
            "kind",
            "request_key",
            "fingerprint",
            "response",
        ],
    ),
    ("artifacts", &["id", "execution_id", "sha256", "metadata"]),
    ("definitions", &["digest", "body"]),
    ("meta", &["key", "value"]),
];
const GOAL_COLUMNS: &[(&str, &[&str])] = &[
    (
        "run_leases",
        &[
            "run_id",
            "owner_kind",
            "owner_id",
            "epoch",
            "executor",
            "binding",
            "phase",
            "slot_held",
            "started",
            "native_stop_confirmed",
            "settlement_seq",
            "budget_ref",
        ],
    ),
    (
        "goals",
        &[
            "id",
            "owner",
            "request_key",
            "fingerprint",
            "revision",
            "queue_order",
            "body",
        ],
    ),
    ("goal_budget", &["goal_id", "body"]),
    (
        "goal_plans",
        &["goal_id", "digest", "bytes", "accepted", "metadata"],
    ),
    (
        "goal_command_receipts",
        &[
            "owner",
            "goal_id",
            "kind",
            "request_key",
            "fingerprint",
            "response",
            "applied_revision",
        ],
    ),
    (
        "goal_artifacts",
        &[
            "id",
            "goal_id",
            "node_id",
            "member_id",
            "plan_digest",
            "schema_digest",
            "sha256",
            "metadata",
        ],
    ),
    (
        "goal_checks",
        &[
            "evaluation_id",
            "goal_id",
            "request",
            "candidate_digest",
            "receipt",
            "accepted_revision",
        ],
    ),
];
fn incompatible(message: impl Into<String>) -> Error {
    Error::new("storage_incompatible", "/store", message)
}
fn check_columns(conn: &Connection, tables: &[(&str, &[&str])]) -> Result<()> {
    for (table, expected) in tables {
        let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let actual = st
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if actual
            .iter()
            .map(String::as_str)
            .ne(expected.iter().copied())
        {
            return Err(incompatible(format!("schema/version mismatch at {table}")));
        }
    }
    Ok(())
}
pub(super) fn initialize(conn: &mut Connection, orchestrated: bool) -> Result<()> {
    let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > 2 {
        return Err(incompatible(
            "newer schema version; refusing to guess migration",
        ));
    }
    if version > 0 {
        check_columns(conn, BASE_COLUMNS)?;
    }
    if version == 2 {
        check_columns(conn, GOAL_COLUMNS)?;
        if !orchestrated
            && conn.query_row("SELECT EXISTS(SELECT 1 FROM goals)", [], |r| {
                r.get::<_, bool>(0)
            })?
        {
            return Err(Error::new(
                "unsupported_store_mode",
                "/store",
                "this store contains Goals; open_orchestrated is required",
            ));
        }
    }
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
    if version == 2 {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if version == 0 {
        let existing: i64 = tx.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if existing != 0 {
            return Err(incompatible("unversioned store is not empty"));
        }
        tx.execute_batch(BASE)?;
    }
    tx.execute_batch(GOAL)?;
    let records = {
        let mut st = tx.prepare(
            "SELECT id,body FROM executions WHERE json_extract(body,'$.active') IS NOT NULL",
        )?;
        st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (id, raw) in records {
        let body: Value = serde_json::from_str(&raw)
            .map_err(|_| incompatible("malformed legacy execution body"))?;
        let active = &body["active"];
        let run = active["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| incompatible("legacy active RunId missing"))?;
        let executor = active["executor"]
            .as_str()
            .ok_or_else(|| incompatible("legacy executor missing"))?;
        let epoch = active["epoch"]
            .as_u64()
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or_else(|| incompatible("legacy epoch invalid"))?;
        let started = active["started"]
            .as_bool()
            .ok_or_else(|| incompatible("legacy started fact missing"))?;
        let binding = body["binding_fingerprint"]
            .as_str()
            .ok_or_else(|| incompatible("legacy binding missing"))?;
        let unknown = started || body["stop_unknown"] == true;
        tx.execute(
            "INSERT INTO run_leases VALUES(?1,'flow',?2,?3,?4,?5,?6,?7,?8,0,0,?1)",
            params![
                run,
                id,
                epoch as i64,
                executor,
                binding,
                if unknown { "stop_unknown" } else { "queued" },
                unknown,
                started
            ],
        )?;
    }
    tx.pragma_update(None, "user_version", 2)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn v1() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(BASE).unwrap();
        c.pragma_update(None, "user_version", 1).unwrap();
        c
    }
    fn insert(c: &Connection, id: &str, started: bool) {
        let body = json!({"status":"running","runs":7,"epoch":4,"binding_fingerprint":"original-binding","active":{"id":format!("run-{id}"),"executor":"original-executor","epoch":3,"started":started,"started_ms":5},"stop_unknown":false});
        c.execute(
            "INSERT INTO executions VALUES(?1,'owner',?1,'fp',4,1,?2)",
            params![id, body.to_string()],
        )
        .unwrap();
    }
    #[test]
    fn migration_preserves_queued_identity_and_all_unknown_occupancy_atomically() {
        let mut c = v1();
        insert(&c, "queued", false);
        insert(&c, "started-a", true);
        insert(&c, "started-b", true);
        let original: String = c
            .query_row("SELECT body FROM executions WHERE id='queued'", [], |r| {
                r.get(0)
            })
            .unwrap();
        initialize(&mut c, true).unwrap();
        assert_eq!(
            c.query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0))
                .unwrap(),
            2
        );
        let queued: (String, i64, String, bool) = c
            .query_row(
                "SELECT run_id,epoch,phase,slot_held FROM run_leases WHERE owner_id='queued'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(queued, ("run-queued".into(), 3, "queued".into(), false));
        assert_eq!(
            c.query_row::<i64, _, _>(
                "SELECT count(*) FROM run_leases WHERE phase='stop_unknown' AND slot_held=1",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            2
        );
        assert_eq!(
            c.query_row::<String, _, _>("SELECT body FROM executions WHERE id='queued'", [], |r| r
                .get(0))
                .unwrap(),
            original
        );
        initialize(&mut c, true).unwrap();
        assert_eq!(
            c.query_row::<i64, _, _>("SELECT count(*) FROM run_leases", [], |r| r.get(0))
                .unwrap(),
            3
        );
    }
    #[test]
    fn migration_failure_rolls_back_tables_import_and_version() {
        let mut c = v1();
        insert(&c, "valid", false);
        c.execute(
            "INSERT INTO executions VALUES('broken','owner','broken','fp',0,2,?1)",
            [json!({"status":"running","active":{"id":"broken"}}).to_string()],
        )
        .unwrap();
        assert!(initialize(&mut c, true).is_err());
        assert_eq!(
            c.query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0))
                .unwrap(),
            1
        );
        assert_eq!(
            c.query_row::<i64, _, _>(
                "SELECT count(*) FROM sqlite_master WHERE name='run_leases'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
    }
    #[test]
    fn legacy_mode_rejects_any_goal_store_and_version_mismatch() {
        let mut c = Connection::open_in_memory().unwrap();
        initialize(&mut c, true).unwrap();
        c.execute(
            "INSERT INTO goals VALUES('goal-1','owner','key','fp',0,1,'{\"status\":\"finished\"}')",
            [],
        )
        .unwrap();
        assert_eq!(
            initialize(&mut c, false).unwrap_err().code,
            "unsupported_store_mode"
        );
        initialize(&mut c, true).unwrap();
        c.execute("ALTER TABLE goal_checks ADD COLUMN incompatible TEXT", [])
            .unwrap();
        assert_eq!(
            initialize(&mut c, true).unwrap_err().code,
            "storage_incompatible"
        );
    }
}
