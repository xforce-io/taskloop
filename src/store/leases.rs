use super::{Row, Store};
use crate::{Error, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

impl Store {
    /// The legacy lane remains one Run; Goal dispatch uses the other global slots.
    /// Reservation never changes the existing Flow RunId, epoch or already charged Run count.
    pub(crate) fn reserve_flow(&self, row: &Row) -> Result<bool> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<(i64, String)> = tx
            .query_row(
                "SELECT revision,body FROM executions WHERE id=?1",
                [&row.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((revision, raw)) = current else {
            return Ok(false);
        };
        if u64::try_from(revision).ok() != Some(row.revision) {
            return Ok(false);
        }
        let body: Value = serde_json::from_str(&raw)?;
        if body["status"] != "running"
            || body["active"]["started"] != false
            || body["deadline_ms"]
                .as_u64()
                .is_none_or(|deadline| crate::runtime::now_ms() >= deadline)
        {
            return Ok(false);
        }
        let Some(run) = body["active"]["id"].as_str() else {
            return Ok(false);
        };
        let phase:Option<String>=tx.query_row("SELECT phase FROM run_leases WHERE run_id=?1 AND owner_kind='flow' AND owner_id=?2",params![run,row.id],|r|r.get(0)).optional()?;
        if phase.as_deref() == Some("reserved") {
            return Ok(true);
        }
        if phase.as_deref() != Some("queued") {
            return Ok(false);
        }
        let occupied: i64 = tx.query_row(
            "SELECT count(*) FROM run_leases WHERE slot_held=1",
            [],
            |r| r.get(0),
        )?;
        let legacy: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM run_leases WHERE owner_kind='flow' AND slot_held=1)",
            [],
            |r| r.get(0),
        )?;
        if legacy || occupied >= self.global_slots.load(std::sync::atomic::Ordering::Relaxed) as i64
        {
            return Ok(false);
        }
        tx.execute(
            "UPDATE run_leases SET phase='reserved',slot_held=1 WHERE run_id=?1 AND phase='queued'",
            [run],
        )?;
        tx.commit()?;
        Ok(true)
    }
}
/// Mirror Flow business facts while the caller's IMMEDIATE CAS transaction is still open.
/// This adapter preserves Flow's existing outcome/stop semantics; it grants no Goal authority.
pub(super) fn sync_flow(conn: &Connection, id: &str, body: &Value) -> Result<()> {
    let active = body.get("active").filter(|v| !v.is_null());
    let active_id = active.and_then(|v| v["id"].as_str());
    // An adapter clears/replaces active only after its existing trusted stop/cleanup path.
    if body["stop_unknown"] != true {
        conn.execute("UPDATE run_leases SET phase='settled',slot_held=0,native_stop_confirmed=1 WHERE owner_kind='flow' AND owner_id=?1 AND run_id!=coalesce(?2,'') AND phase!='settled'",params![id,active_id])?;
    }
    let Some(active) = active else {
        return Ok(());
    };
    let run = active_id.ok_or_else(|| Error::io("Flow active RunId missing"))?;
    let executor = active["executor"]
        .as_str()
        .ok_or_else(|| Error::io("Flow executor missing"))?;
    let binding = body["binding_fingerprint"]
        .as_str()
        .ok_or_else(|| Error::io("Flow binding missing"))?;
    let epoch = active["epoch"]
        .as_u64()
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or_else(|| Error::io("Flow epoch invalid"))?;
    let started = active["started"] == true;
    let previous:Option<(String,bool)>=conn.query_row("SELECT phase,slot_held FROM run_leases WHERE run_id=?1 AND owner_kind='flow' AND owner_id=?2 AND epoch=?3 AND binding=?4",params![run,id,epoch as i64,binding],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if started
        && previous.as_ref().is_none_or(|(p, held)| {
            !held || !matches!(p.as_str(), "reserved" | "started" | "stop_unknown")
        })
    {
        return Err(Error::new(
            "dispatch_not_reserved",
            "/run",
            "Flow cannot start outside SharedDispatcher reservation",
        ));
    }
    let (phase, held, confirmed) = if body["stop_unknown"] == true {
        ("stop_unknown", true, false)
    } else if started {
        ("started", true, false)
    } else if body["status"] != "running" {
        ("settled", false, true)
    } else if previous.as_ref().is_some_and(|(p, _)| p == "reserved") {
        ("reserved", true, false)
    } else {
        ("queued", false, false)
    };
    let changed=conn.execute("INSERT INTO run_leases VALUES(?1,'flow',?2,?3,?4,?5,?6,?7,?8,?9,0,?1) ON CONFLICT(run_id) DO UPDATE SET phase=excluded.phase,slot_held=excluded.slot_held,started=excluded.started,native_stop_confirmed=excluded.native_stop_confirmed WHERE run_leases.owner_kind='flow' AND run_leases.owner_id=excluded.owner_id AND run_leases.epoch=excluded.epoch AND run_leases.binding=excluded.binding",params![run,id,epoch as i64,executor,binding,phase,held,started,confirmed])?;
    if changed != 1 {
        return Err(Error::io("RunId collision or lease identity mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn body(run: &str) -> Value {
        json!({"status":"running","deadline_ms":crate::runtime::now_ms()+60_000,"binding_fingerprint":"binding","stop_unknown":false,"active":{"id":run,"epoch":1,"executor":"test","started":false}})
    }
    #[test]
    fn flow_start_requires_reservation_and_never_recounts_or_changes_run_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_mode(&dir.path().join("store"), true, 3).unwrap();
        let original = body("run-a");
        let (row, _) = store
            .create("execution-a", "owner", "a", "fp", &original, 8)
            .unwrap();
        let mut started = original.clone();
        started["active"]["started"] = true.into();
        assert_eq!(
            store
                .commit(&row, &started, false, None, &[])
                .err()
                .unwrap()
                .code,
            "dispatch_not_reserved"
        );
        assert_eq!(store.load(&row.id).unwrap().revision, 0);
        assert!(store.reserve_flow(&row).unwrap());
        assert_eq!(store.load(&row.id).unwrap().body, original);
        assert!(store.commit(&row, &started, false, None, &[]).is_ok());
        let (second, _) = store
            .create("execution-b", "owner", "b", "fp", &body("run-b"), 8)
            .unwrap();
        assert!(!store.reserve_flow(&second).unwrap()); // legacy lane stays one, despite three global slots.
        let latest = store.load(&row.id).unwrap();
        let mut ended = latest.body.clone();
        ended["active"] = Value::Null;
        ended["status"] = "completed".into();
        store.commit(&latest, &ended, false, None, &[]).unwrap();
        assert!(store.reserve_flow(&second).unwrap());
    }
    #[test]
    fn unknown_goal_uses_global_capacity_without_owning_the_legacy_lane() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_mode(&dir.path().join("store"), true, 2).unwrap();
        store.connection().unwrap().execute("INSERT INTO run_leases VALUES('goal-run','goal','goal-a',1,'ex','binding','stop_unknown',1,1,0,0,'budget')",[]).unwrap();
        let (row, _) = store
            .create("execution-a", "owner", "a", "fp", &body("flow-run"), 8)
            .unwrap();
        assert!(store.reserve_flow(&row).unwrap());
        let occupied: i64 = store
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM run_leases WHERE slot_held=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(occupied, 2);
    }
}
