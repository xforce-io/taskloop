use super::{CommandReceipt, Store};
use crate::{
    orchestration::{
        ClockObservation, GoalClock, GoalCommitEffects, GoalRecord,
        control::{FinishFacts, ResultAuthority, ResumeFacts, SignalAuthority},
    },
    *,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone)]
pub(crate) struct GoalRow {
    pub id: String,
    pub owner: String,
    pub revision: u64,
    pub queue_order: u64,
    pub record: GoalRecord,
}
pub(crate) enum GoalCommit {
    Saved(Box<GoalRow>),
    Replayed(Value),
}
pub(crate) enum GoalWriteGuard<'a> {
    Advance,
    Result {
        run_id: &'a str,
        binding: &'a str,
    },
    NodeResult {
        node_id: &'a str,
        nonce: &'a str,
    },
    Signal {
        wait_id: &'a str,
        nonce: &'a str,
    },
    Resume {
        binding: &'a str,
    },
    Finish {
        proofs_valid: bool,
    },
    Settlement {
        run_id: &'a str,
        epoch: u64,
        binding: &'a str,
    },
    Cleanup,
    Recovery,
}
/// Call inside a read transaction so record, artifacts and physical slots share one snapshot.
pub(crate) fn goal_snapshot_from(conn: &Connection, id: &str, owner: &str) -> Result<GoalSnapshot> {
    let row = goal_by_id(conn, id)?;
    if row.owner != owner {
        return Err(not_found());
    }
    let mut snapshot = row.record.snapshot(id, row.revision);
    let mut statement =
        conn.prepare("SELECT metadata FROM goal_artifacts WHERE goal_id=?1 ORDER BY rowid")?;
    snapshot.artifacts = statement
        .query_map([id], |r| r.get::<_, String>(0))?
        .map(|r| serde_json::from_str(&r?).map_err(Error::io))
        .collect::<Result<_>>()?;
    let counts: (i64, i64, i64) = conn.query_row(
        "SELECT count(*), coalesce(sum(CASE WHEN owner_kind='goal' AND owner_id=?1 THEN 1 ELSE 0 END),0), coalesce(sum(CASE WHEN phase='stop_unknown' THEN 1 ELSE 0 END),0) FROM run_leases WHERE slot_held=1",
        [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    snapshot.progress.host_occupied_slots = u64::try_from(counts.0).map_err(Error::io)?;
    snapshot.progress.goal_occupied_slots = u64::try_from(counts.1).map_err(Error::io)?;
    snapshot.progress.host_stop_unknown_slots = u64::try_from(counts.2).map_err(Error::io)?;
    Ok(snapshot)
}
fn not_found() -> Error {
    Error::new("not_found", "/goal", "Goal not found")
}
fn goal_columns(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, i64, i64, String, String)> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}
fn decode_goal(
    (id, owner, revision, queue_order, body, budget): (String, String, i64, i64, String, String),
) -> Result<GoalRow> {
    let mut record: GoalRecord = serde_json::from_str(&body).map_err(Error::io)?;
    record.budget = serde_json::from_str(&budget).map_err(Error::io)?;
    Ok(GoalRow {
        id,
        owner,
        revision: u64::try_from(revision).map_err(Error::io)?,
        queue_order: u64::try_from(queue_order).map_err(Error::io)?,
        record,
    })
}
pub(crate) fn goal_by_id(conn: &Connection, id: &str) -> Result<GoalRow> {
    let row=conn.query_row("SELECT g.id,g.owner,g.revision,g.queue_order,g.body,b.body FROM goals g JOIN goal_budget b ON b.goal_id=g.id WHERE g.id=?1",[id],goal_columns).optional()?.ok_or_else(not_found)?;
    decode_goal(row)
}
fn update_highwater(conn: &Connection, observed: ClockObservation) -> Result<bool> {
    let high: i64 = conn.query_row(
        "SELECT value FROM meta WHERE key='last_observed_utc_ms'",
        [],
        |r| r.get(0),
    )?;
    if high < 0 {
        return Err(Error::io("negative clock highwater"));
    }
    if !observed.trusted || observed.utc_ms < high as u64 || observed.utc_ms > i64::MAX as u64 {
        return Ok(false);
    }
    conn.execute(
        "UPDATE meta SET value=?1 WHERE key='last_observed_utc_ms' AND value<?1",
        [observed.utc_ms as i64],
    )?;
    Ok(true)
}
impl Store {
    pub(crate) fn goal_clock_highwater(&self) -> Result<u64> {
        let value: i64 = self.connection()?.query_row(
            "SELECT value FROM meta WHERE key='last_observed_utc_ms'",
            [],
            |r| r.get(0),
        )?;
        u64::try_from(value).map_err(Error::io)
    }
    pub(crate) fn observe_goal_clock(&self, clock: &GoalClock) -> Result<ClockObservation> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut observation = clock.observe()?;
        observation.trusted = update_highwater(&tx, observation)?;
        tx.commit()?;
        Ok(observation)
    }
    pub(crate) fn goal_load(&self, id: &str) -> Result<GoalRow> {
        goal_by_id(&*self.connection()?, id)
    }
    pub(crate) fn goal_find(
        &self,
        owner: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<GoalRow>> {
        let c = self.connection()?;
        let prior: Option<(String, String)> = c
            .query_row(
                "SELECT id,fingerprint FROM goals WHERE owner=?1 AND request_key=?2",
                params![owner, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((id, fp)) = prior {
            if fp != fingerprint {
                return Err(Error::new(
                    "request_conflict",
                    "/request_key",
                    "same Goal request key has different content",
                ));
            }
            return Ok(Some(goal_by_id(&c, &id)?));
        }
        Ok(None)
    }
    pub(crate) fn goal_all_active(&self) -> Result<Vec<GoalRow>> {
        let c = self.connection()?;
        let mut st=c.prepare("SELECT g.id,g.owner,g.revision,g.queue_order,g.body,b.body FROM goals g JOIN goal_budget b ON b.goal_id=g.id WHERE json_extract(g.body,'$.status')!='finished' ORDER BY g.queue_order,g.id")?;
        let rows = st
            .query_map([], goal_columns)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(decode_goal).collect()
    }
    pub(crate) fn goal_create(
        &self,
        id: &str,
        owner: &str,
        record: &GoalRecord,
        capacity: usize,
        clock: &GoalClock,
    ) -> Result<(GoalRow, bool)> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let request = &record.prepared.request;
        let fingerprint = &record.prepared.request_fingerprint;
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT id,fingerprint FROM goals WHERE owner=?1 AND request_key=?2",
                params![owner, request.request_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((id, fp)) = prior {
            if &fp != fingerprint {
                return Err(Error::new(
                    "request_conflict",
                    "/request_key",
                    "same Goal request key has different content",
                ));
            }
            let row = goal_by_id(&tx, &id)?;
            tx.commit()?;
            return Ok((row, false));
        }
        if !update_highwater(&tx, clock.observe()?)? {
            return Err(Error::new(
                "clock_untrusted",
                "/clock",
                "host clock is earlier than the persisted highwater",
            ));
        }
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT id FROM goals WHERE id=?1 UNION ALL SELECT id FROM executions WHERE id=?1)",[id],|r|r.get(0))?;
        if exists {
            return Err(Error::new(
                "request_conflict",
                "/goal_id",
                "execution identity collision",
            ));
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM goals WHERE json_extract(body,'$.status')!='finished'",
            [],
            |r| r.get(0),
        )?;
        if count >= capacity as i64 {
            return Err(Error::new(
                "admission_full",
                "/goal",
                "Goal admission capacity reached",
            ));
        }
        if let Some(parent) = &request.spec.parent_goal_id {
            let parent_owner: Option<String> = tx
                .query_row("SELECT owner FROM goals WHERE id=?1", [parent], |r| {
                    r.get(0)
                })
                .optional()?;
            if parent_owner.as_deref() != Some(owner) {
                return Err(not_found());
            }
        }
        for import in &request.spec.imports {
            let source:Option<(String,String,Option<String>)>=tx.query_row("SELECT g.owner,a.sha256,a.schema_digest FROM goal_artifacts a JOIN goals g ON g.id=a.goal_id WHERE a.id=?1 AND a.accepted=1",[&import.artifact_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if !matches!(source,Some((ref source_owner,ref sha,ref schema)) if source_owner==owner&&sha==&import.sha256&&schema.as_deref()==Some(import.schema_digest.as_str()))
            {
                return Err(Error::new(
                    "capability_denied",
                    "/spec/imports",
                    "import is not an exact accepted artifact of the same owner",
                ));
            }
            self.read_blob(&import.sha256)?;
        }
        let queue = Self::next_queue(&tx)?;
        tx.execute(
            "INSERT INTO goals VALUES(?1,?2,?3,?4,0,?5,?6)",
            params![
                id,
                owner,
                request.request_key,
                fingerprint,
                queue as i64,
                serde_json::to_string(record)?
            ],
        )?;
        tx.execute(
            "INSERT INTO goal_budget VALUES(?1,?2)",
            params![id, serde_json::to_string(&record.budget)?],
        )?;
        let end = clock.observe()?;
        let trusted = update_highwater(&tx, end)?;
        if !record.permissions(end.utc_ms, trusted).advance_allowed() {
            return Err(Error::new(
                "input_invalid",
                "/spec/limits",
                "Goal admission deadline or clock became invalid before commit",
            ));
        }
        tx.commit()?;
        Ok((
            GoalRow {
                id: id.into(),
                owner: owner.into(),
                revision: 0,
                queue_order: queue,
                record: record.clone(),
            },
            true,
        ))
    }
    pub(crate) fn goal_artifact(&self, id: &str, owner: &str) -> Result<GoalArtifact> {
        let c = self.connection()?;
        let raw:Option<String>=c.query_row("SELECT a.metadata FROM goal_artifacts a JOIN goals g ON g.id=a.goal_id WHERE a.id=?1 AND g.owner=?2",params![id,owner],|r|r.get(0)).optional()?;
        serde_json::from_str(
            &raw.ok_or_else(|| {
                Error::new("not_found", "/artifact", "accepted artifact not found")
            })?,
        )
        .map_err(Error::io)
    }
    pub(crate) fn goal_command(
        &self,
        id: &str,
        owner: &str,
        kind: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<Option<Value>> {
        goal_command(&*self.connection()?, id, owner, kind, key, fingerprint)
    }
    pub(crate) fn goal_commit(
        &self,
        row: &GoalRow,
        record: &GoalRecord,
        guard: GoalWriteGuard<'_>,
        command: Option<CommandReceipt>,
        effects: &GoalCommitEffects,
        clock: &GoalClock,
    ) -> Result<GoalCommit> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(command) = &command
            && let Some(response) = goal_command(
                &tx,
                &row.id,
                &row.owner,
                &command.kind,
                &command.key,
                &command.fingerprint,
            )?
        {
            tx.commit()?;
            return Ok(GoalCommit::Replayed(response));
        }
        let revision: i64 = tx
            .query_row(
                "SELECT revision FROM goals WHERE id=?1 AND owner=?2",
                params![row.id, row.owner],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(not_found)?;
        if u64::try_from(revision).ok() != Some(row.revision) {
            return Err(Error::new(
                "revision_conflict",
                "/revision",
                "Goal changed; recompute from the current snapshot",
            ));
        }
        let observation = clock.observe()?;
        let trusted = update_highwater(&tx, observation)?;
        check_guard(&row.record, &guard, observation, trusted)?;
        validate_goal_change(&row.record, record, &guard, effects)?;
        let mut next = record.clone();
        let current_budget: String = tx.query_row(
            "SELECT body FROM goal_budget WHERE goal_id=?1",
            [&row.id],
            |r| r.get(0),
        )?;
        merge_admission_counters(
            &row.record.budget,
            &mut next.budget,
            &serde_json::from_str(&current_budget).map_err(Error::io)?,
        )?;
        next.budget.logical_accepted_bytes = effects.artifacts.iter().try_fold(
            row.record.budget.logical_accepted_bytes,
            |total, artifact| -> Result<u64> {
                total
                    .checked_add(self.read_blob(&artifact.artifact.sha256)?.len() as u64)
                    .ok_or_else(|| Error::io("logical accepted byte counter overflow"))
            },
        )?;
        if next.prepared.request_fingerprint != row.record.prepared.request_fingerprint
            || next.prepared.spec_digest != row.record.prepared.spec_digest
            || next.prepared.profile_digest != row.record.prepared.profile_digest
            || next.prepared.wall_deadline_utc_ms != row.record.prepared.wall_deadline_utc_ms
            || next.prepared.planning_deadline_utc_ms
                != row.record.prepared.planning_deadline_utc_ms
            || next.binding_fingerprint != row.record.binding_fingerprint
        {
            return Err(Error::io("immutable Goal admission facts changed"));
        }
        if let Some(locked) = &row.record.plan
            && serde_json::to_value(locked)? != serde_json::to_value(&next.plan)?
        {
            return Err(Error::io("locked Goal plan cannot change"));
        }
        let edges = state_edges(&row.record, &next);
        let cleanup = matches!(
            guard,
            GoalWriteGuard::Cleanup | GoalWriteGuard::Recovery | GoalWriteGuard::Settlement { .. }
        );
        let business = effects
            .business_edges
            .unwrap_or(if cleanup { 0 } else { edges });
        if business > edges {
            return Err(Error::io(
                "business transition count exceeds committed state edges",
            ));
        }
        next.budget
            .charge_transitions(business, false, &next.prepared.effective_limits)
            .map_err(|e| Error::new("budget_exhausted", e.path, e.message))?;
        next.budget
            .charge_transitions(edges - business, true, &next.prepared.effective_limits)?;
        sync_goal_leases(
            &tx,
            &row.id,
            &next,
            self.global_slots.load(std::sync::atomic::Ordering::Relaxed),
        )?;
        let next_revision = row
            .revision
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(|| Error::io("Goal revision overflow"))?;
        let changed = tx.execute(
            "UPDATE goals SET body=?1,revision=?2 WHERE id=?3 AND revision=?4",
            params![
                serde_json::to_string(&next)?,
                next_revision as i64,
                row.id,
                revision
            ],
        )?;
        if changed != 1 {
            return Err(Error::new(
                "revision_conflict",
                "/revision",
                "Goal CAS lost",
            ));
        }
        tx.execute(
            "UPDATE goal_budget SET body=?1 WHERE goal_id=?2",
            params![serde_json::to_string(&next.budget)?, row.id],
        )?;
        for artifact in &effects.artifacts {
            if artifact.goal_id != row.id {
                return Err(Error::io("cross-Goal artifact in commit"));
            }
            if !artifact.accepted {
                return Err(Error::io(
                    "business effects cannot accept a diagnostic candidate",
                ));
            }
            insert_goal_artifact(&tx, artifact)?;
        }
        if let Some(plan) = &effects.plan {
            tx.execute(
                "INSERT INTO goal_plans VALUES(?1,?2,?3,1,?4)",
                params![
                    row.id,
                    plan.plan_digest,
                    serde_json::to_vec(&plan.proposal)?,
                    serde_json::to_string(plan)?
                ],
            )?;
        }
        for (request, receipt) in &effects.checks {
            let id = &request.input.nonce;
            let check = next
                .checks
                .get(id)
                .ok_or_else(|| Error::io("check request missing from Goal record"))?;
            let metadata = serde_json::json!({"input_sha":check.input_sha,"input_digest":request.input_digest,"checker_ref":request.checker_ref,"producer_run_id":check.producer_run_id});
            tx.execute("INSERT INTO goal_checks VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(evaluation_id) DO UPDATE SET candidate_digest=excluded.candidate_digest,receipt=excluded.receipt,accepted_revision=excluded.accepted_revision WHERE goal_checks.goal_id=excluded.goal_id AND goal_checks.receipt IS NULL",params![id,row.id,metadata.to_string(),receipt.as_ref().map(|r|&r.candidate_digest),check.receipt_sha,receipt.as_ref().map(|r|r.accepted_revision as i64)])?;
        }
        if let Some(command) = command {
            tx.execute(
                "INSERT INTO goal_command_receipts VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    row.owner,
                    row.id,
                    command.kind,
                    command.key,
                    command.fingerprint,
                    serde_json::to_string(&command.response)?,
                    next_revision as i64
                ],
            )?;
        }
        // Recheck fixed deadlines after serialization and all writes, before the actual commit.
        let end = clock.observe()?;
        let trusted = update_highwater(&tx, end)?;
        check_guard(&row.record, &guard, end, trusted)?;
        if next.outcome == Some(GoalOutcome::Achieved) && row.record.outcome != next.outcome {
            let mut facts = next.permissions(end.utc_ms, trusted);
            facts.status = GoalStatus::Running;
            if !facts.finish_allowed(&FinishFacts {
                required_proofs_match_and_pass: effects.finish_proofs_valid,
                all_nodes_succeeded: next
                    .nodes
                    .values()
                    .all(|n| n.view.status == GoalNodeStatus::Succeeded),
                all_runs_settled: next
                    .runs
                    .values()
                    .all(|r| r.view.phase == GoalRunPhase::Settled),
                cleanup_complete: next
                    .requests
                    .values()
                    .all(|r| r.cancelled || r.run_id.is_some()),
            }) {
                return Err(Error::new(
                    "stale_attempt",
                    "/finish",
                    "Goal finish permission closed before commit",
                ));
            }
        }
        tx.commit()?;
        Ok(GoalCommit::Saved(Box::new(GoalRow {
            id: row.id.clone(),
            owner: row.owner.clone(),
            revision: next_revision,
            queue_order: row.queue_order,
            record: next,
        })))
    }
}
fn goal_command(
    conn: &Connection,
    id: &str,
    owner: &str,
    kind: &str,
    key: &str,
    fingerprint: &str,
) -> Result<Option<Value>> {
    let prior:Option<(String,String)>=conn.query_row("SELECT fingerprint,response FROM goal_command_receipts WHERE owner=?1 AND goal_id=?2 AND kind=?3 AND request_key=?4",params![owner,id,kind,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    match prior {
        None => Ok(None),
        Some((fp, response)) => {
            if fp != fingerprint {
                return Err(Error::new(
                    "request_conflict",
                    "/request_key",
                    "same command key has different content",
                ));
            }
            Ok(Some(serde_json::from_str(&response)?))
        }
    }
}
fn check_guard(
    before: &GoalRecord,
    guard: &GoalWriteGuard<'_>,
    observed: ClockObservation,
    trusted: bool,
) -> Result<()> {
    let facts = before.permissions(observed.utc_ms, trusted);
    let allowed = match guard {
        GoalWriteGuard::Advance => facts.advance_allowed(),
        GoalWriteGuard::Result { run_id, binding } => {
            facts.result_allowed(&before.result_authority(run_id, binding))
        }
        GoalWriteGuard::NodeResult { node_id, nonce } => {
            before.nodes.get(*node_id).is_some_and(|node| {
                facts.result_allowed(&ResultAuthority {
                    original_source_valid: true,
                    ownership_and_digests_valid: node.checking_nonce.as_deref() == Some(*nonce),
                    epoch: node.checking_epoch.unwrap_or(0),
                    predecessor_valid: node.view.status == GoalNodeStatus::Checking,
                })
            })
        }
        GoalWriteGuard::Signal { wait_id, nonce } => {
            before.waiting.get(*wait_id).is_some_and(|wait| {
                facts.signal_allowed(&SignalAuthority {
                    wait_identity_valid: wait.nonce == *nonce && !wait.consumed,
                    schema_and_caller_valid: true,
                    revision_valid: true,
                    expires_at_ms: wait.expires_at_utc_ms,
                })
            })
        }
        GoalWriteGuard::Resume { binding } => facts.resume_allowed(&ResumeFacts {
            unsettled_started_run: before
                .runs
                .values()
                .any(|r| r.view.started && r.view.phase != GoalRunPhase::Settled),
            bindings_valid: before.binding_fingerprint == *binding,
            all_wait_deadlines_valid: before
                .waiting
                .values()
                .filter(|w| !w.consumed)
                .all(|w| observed.utc_ms < w.expires_at_utc_ms),
            lost_acceptance: before.runs.values().any(|r| r.view.interrupted),
            uncommitted_checking: before
                .nodes
                .values()
                .any(|n| n.view.status == GoalNodeStatus::Checking),
        }),
        GoalWriteGuard::Finish { proofs_valid } => facts.finish_allowed(&FinishFacts {
            required_proofs_match_and_pass: *proofs_valid,
            all_nodes_succeeded: before
                .nodes
                .values()
                .all(|n| n.view.status == GoalNodeStatus::Succeeded),
            all_runs_settled: before
                .runs
                .values()
                .all(|r| r.view.phase == GoalRunPhase::Settled),
            cleanup_complete: before
                .requests
                .values()
                .all(|r| r.cancelled || r.run_id.is_some()),
        }),
        GoalWriteGuard::Settlement {
            run_id,
            epoch,
            binding,
        } => before.runs.get(*run_id).is_some_and(|run| {
            crate::orchestration::control::settlement_allowed(
                true,
                run.view.epoch == *epoch,
                run.executor_binding == *binding,
                true,
            )
        }),
        GoalWriteGuard::Cleanup | GoalWriteGuard::Recovery => true,
    };
    if allowed {
        Ok(())
    } else {
        Err(Error::new(
            "stale_attempt",
            "/goal",
            "transaction no longer has the required Goal permission",
        ))
    }
}
pub(crate) fn state_edges(before: &GoalRecord, after: &GoalRecord) -> u64 {
    let goal = u64::from(
        (before.status, before.stage, before.outcome) != (after.status, after.stage, after.outcome),
    );
    let nodes = after
        .nodes
        .iter()
        .filter(|(id, n)| {
            before
                .nodes
                .get(*id)
                .is_none_or(|old| old.view.status != n.view.status)
        })
        .count() as u64;
    let runs = after
        .runs
        .iter()
        .filter(|(id, r)| {
            before
                .runs
                .get(*id)
                .is_none_or(|old| old.view.phase != r.view.phase)
        })
        .count() as u64;
    goal + nodes + runs
}
fn sync_goal_leases(
    conn: &Connection,
    id: &str,
    record: &GoalRecord,
    global_slots: usize,
) -> Result<()> {
    let existing = {
        let mut st=conn.prepare("SELECT run_id,phase,slot_held,epoch,executor,binding,started,native_stop_confirmed FROM run_leases WHERE owner_kind='goal' AND owner_id=?1")?;
        st.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, bool>(6)?,
                    r.get::<_, bool>(7)?,
                ),
            ))
        })?
        .collect::<std::result::Result<BTreeMap<_, _>, _>>()?
    };
    if existing.keys().any(|id| !record.runs.contains_key(id)) {
        return Err(Error::io("persisted Goal Run cannot be deleted"));
    }
    let mut acquired = 0usize;
    for (run_id, run) in &record.runs {
        let held = if run.view.started {
            !run.view.native_stop_confirmed
        } else {
            run.view.phase == GoalRunPhase::Reserved
        };
        let phase = if run.view.stop_unknown {
            "stop_unknown"
        } else if held {
            if run.view.started {
                "started"
            } else {
                "reserved"
            }
        } else if run.view.phase == GoalRunPhase::Suspended {
            "suspended"
        } else {
            "settled"
        };
        if let Some((previous, was_held, epoch, executor, binding, was_started, was_confirmed)) =
            existing.get(run_id)
        {
            if u64::try_from(*epoch).ok() != Some(run.view.epoch)
                || executor != &run.executor
                || binding != &run.executor_binding
            {
                return Err(Error::io("original Goal Run identity changed"));
            }
            if *was_started && !run.view.started
                || *was_confirmed && !run.view.native_stop_confirmed
            {
                return Err(Error::io(
                    "Goal native start/stop facts cannot move backwards",
                ));
            }
            if (run.view.started
                && held
                && (!*was_held
                    || !matches!(previous.as_str(), "reserved" | "started" | "stop_unknown")))
                || (held && previous == "settled")
            {
                return Err(Error::io(
                    "Goal started without a held reservation or attempted to revive a settled lease",
                ));
            }
            if held && !*was_held {
                acquired += 1;
            }
        } else {
            if run.view.started || run.view.phase != GoalRunPhase::Reserved {
                return Err(Error::new(
                    "dispatch_not_reserved",
                    "/run",
                    "new Goal Run must begin reserved and unstarted",
                ));
            }
            if held {
                acquired += 1;
            }
        }
        let changed=conn.execute("INSERT INTO run_leases VALUES(?1,'goal',?2,?3,?4,?5,?6,?7,?8,?9,?10,?1) ON CONFLICT(run_id) DO UPDATE SET phase=excluded.phase,slot_held=excluded.slot_held,started=excluded.started,native_stop_confirmed=excluded.native_stop_confirmed,settlement_seq=excluded.settlement_seq WHERE run_leases.owner_kind='goal' AND run_leases.owner_id=excluded.owner_id AND run_leases.epoch=excluded.epoch AND run_leases.binding=excluded.binding",params![run_id,id,run.view.epoch as i64,run.executor,run.executor_binding,phase,held,run.view.started,run.view.native_stop_confirmed,run.settlement_sequence as i64])?;
        if changed != 1 {
            return Err(Error::io("RunId collision"));
        }
    }
    if acquired > 0 {
        let global: i64 = conn.query_row(
            "SELECT count(*) FROM run_leases WHERE slot_held=1",
            [],
            |r| r.get(0),
        )?;
        let goal:i64=conn.query_row("SELECT count(*) FROM run_leases WHERE slot_held=1 AND owner_kind='goal' AND owner_id=?1",[id],|r|r.get(0))?;
        if global > global_slots as i64
            || goal > record.prepared.effective_limits.max_concurrent_runs as i64
        {
            return Err(Error::new(
                "dispatch_capacity",
                "/run",
                "physical slot capacity currently unavailable",
            ));
        }
    }
    Ok(())
}

fn validate_goal_change(
    before: &GoalRecord,
    after: &GoalRecord,
    guard: &GoalWriteGuard<'_>,
    effects: &GoalCommitEffects,
) -> Result<()> {
    if serde_json::to_value(&before.prepared)? != serde_json::to_value(&after.prepared)?
        || before.raw_request_sha != after.raw_request_sha
    {
        return Err(Error::io("immutable Goal request or limits changed"));
    }
    if after.status == GoalStatus::Finished && after.outcome.is_none()
        || after.status != GoalStatus::Finished && after.outcome.is_some()
    {
        return Err(Error::io("Goal outcome and status disagree"));
    }
    let new_runs = after
        .runs
        .keys()
        .filter(|id| !before.runs.contains_key(*id))
        .count() as u64;
    if before.budget.runs_used.checked_add(new_runs) != Some(after.budget.runs_used) {
        return Err(Error::io(
            "Run materialization and max_runs charge disagree",
        ));
    }
    for (id, run) in &after.runs {
        let request = after
            .requests
            .get(&run.request_id)
            .ok_or_else(|| Error::io("Run has no persisted request"))?;
        if request.run_id.as_deref() != Some(id)
            || request.epoch != run.view.epoch
            || request.entry_ref != run.view.entry_ref
            || request.input_sha != run.input_sha
            || request.role != run.view.role
        {
            return Err(Error::io(
                "Run materialization differs from its immutable request",
            ));
        }
        let held = if run.view.started {
            !run.view.native_stop_confirmed
        } else {
            run.view.phase == GoalRunPhase::Reserved
        };
        if held && !after.budget.reserved.contains_key(id) {
            return Err(Error::io("held physical Run has no budget reservation"));
        }
    }
    if after.epoch < before.epoch {
        return Err(Error::io("Goal epoch moved backwards"));
    }
    if before.terminal_intent.is_some()
        && serde_json::to_value(&before.terminal_intent)?
            != serde_json::to_value(&after.terminal_intent)?
    {
        return Err(Error::io("the first terminal intent is immutable"));
    }
    if before.status == GoalStatus::Finished
        && (after.status != before.status || after.outcome != before.outcome)
    {
        return Err(Error::io("finished Goal business outcome cannot change"));
    }
    if after.outcome == Some(GoalOutcome::Achieved)
        && before.outcome != after.outcome
        && (!effects.finish_proofs_valid
            || matches!(guard, GoalWriteGuard::Cleanup | GoalWriteGuard::Recovery))
    {
        return Err(Error::io("achieved requires finish permission"));
    }
    if after.status == GoalStatus::Finished
        && after.runs.values().any(|r| {
            r.view.phase != GoalRunPhase::Settled || r.view.started && !r.view.native_stop_confirmed
        })
    {
        return Err(Error::io("Goal cannot finish with unsettled original Runs"));
    }
    if matches!(
        guard,
        GoalWriteGuard::Cleanup | GoalWriteGuard::Recovery | GoalWriteGuard::Settlement { .. }
    ) {
        if !effects.artifacts.is_empty() || !effects.checks.is_empty() || effects.plan.is_some() {
            return Err(Error::io(
                "cleanup/settlement cannot accept business outputs or receipts",
            ));
        }
        if after.nodes.iter().any(|(id, n)| {
            before.nodes.get(id).is_none_or(|old| {
                old.view.status != n.view.status
                    && matches!(
                        n.view.status,
                        GoalNodeStatus::Ready
                            | GoalNodeStatus::Running
                            | GoalNodeStatus::Succeeded
                            | GoalNodeStatus::Checking
                            | GoalNodeStatus::WaitingInput
                    )
            })
        }) {
            return Err(Error::io("cleanup cannot advance a node"));
        }
        if after.runs.iter().any(|(id, r)| {
            before.runs.get(id).is_none_or(|old| {
                !old.view.started && r.view.started
                    || old.view.phase == GoalRunPhase::Suspended
                        && r.view.phase == GoalRunPhase::Reserved
            })
        }) {
            return Err(Error::io("cleanup cannot materialize or start a Run"));
        }
        if before.stage != after.stage {
            return Err(Error::io("cleanup cannot advance Goal stage"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum GoalBudgetCharge {
    Tool,
    CandidateBytes(u64),
    RejectedCalls(u64),
    RejectedBytes(u64),
}
impl Store {
    /// High-frequency admission reads only authority plus the budget row, not a cloned GoalRecord.
    /// The per-Run remaining reservation and Goal total move in this one IMMEDIATE transaction.
    pub(crate) fn goal_budget_admit(
        &self,
        id: &str,
        run_id: &str,
        epoch: u64,
        binding: &str,
        charge: GoalBudgetCharge,
        clock: &GoalClock,
    ) -> Result<()> {
        if run_id.is_empty()
            || run_id.len() > 160
            || !run_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(Error::new(
                "stale_attempt",
                "/run",
                "invalid original Run identity",
            ));
        }
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let path = format!("$.runs.\"{run_id}\"");
        let (_revision,raw,budget):(i64,String,String)=tx.query_row("SELECT g.revision,json_object('status',json_extract(g.body,'$.status'),'stage',json_extract(g.body,'$.stage'),'terminal',json_extract(g.body,'$.terminal_intent') IS NOT NULL,'blocked',json_array_length(json_extract(g.body,'$.blockers'))>0,'pause',json_extract(g.body,'$.pause_at_utc_ms'),'epoch',json_extract(g.body,'$.epoch'),'wall',json_extract(g.body,'$.prepared.wall_deadline_utc_ms'),'planning',json_extract(g.body,'$.prepared.planning_deadline_utc_ms'),'run',json_extract(g.body,?2),'request',json_extract(g.body,'$.requests.'||json_quote(json_extract(g.body,?2||'.request_id'))),'node',json_extract(g.body,'$.nodes.'||json_quote(json_extract(g.body,?2||'.view.role.node_id'))),'check',json_extract(g.body,'$.checks.'||json_quote(json_extract(g.body,'$.requests.'||json_quote(json_extract(g.body,?2||'.request_id'))||'.check_request_id'))),'planning_attempt',json_extract(g.body,'$.planning_attempts'),'latest_proposal_digest',json_extract(g.body,'$.proposals[#-1].proposal_digest')),b.body FROM goals g JOIN goal_budget b ON b.goal_id=g.id WHERE g.id=?1",params![id,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.ok_or_else(not_found)?;
        let auth: Value = serde_json::from_str(&raw).map_err(Error::io)?;
        let original: crate::orchestration::GoalRunRecord =
            serde_json::from_value(auth["run"].clone())
                .map_err(|_| Error::new("stale_attempt", "/run", "original Run unavailable"))?;
        let request: crate::orchestration::GoalRunRequest =
            serde_json::from_value(auth["request"].clone()).map_err(|_| {
                Error::new(
                    "stale_attempt",
                    "/request",
                    "original Run request unavailable",
                )
            })?;
        let node = if auth["node"].is_null() {
            None
        } else {
            Some(
                serde_json::from_value::<crate::orchestration::GoalNodeRecord>(
                    auth["node"].clone(),
                )
                .map_err(Error::io)?,
            )
        };
        let issued = if auth["check"].is_null() {
            None
        } else {
            Some(
                serde_json::from_value::<crate::orchestration::GoalCheckRecord>(
                    auth["check"].clone(),
                )
                .map_err(Error::io)?,
            )
        };
        let check = |observed: ClockObservation, trusted: bool| -> Result<()> {
            let facts = crate::orchestration::control::PermissionFacts {
                status: serde_json::from_value(auth["status"].clone()).map_err(Error::io)?,
                stage: serde_json::from_value(auth["stage"].clone()).map_err(Error::io)?,
                terminal: auth["terminal"] == 1,
                blocked: auth["blocked"] == 1,
                clock_trusted: trusted,
                pause_at_ms: auth["pause"].as_u64(),
                epoch: auth["epoch"]
                    .as_u64()
                    .ok_or_else(|| Error::io("Goal epoch unavailable"))?,
                now_ms: observed.utc_ms,
                wall_deadline_ms: auth["wall"]
                    .as_u64()
                    .ok_or_else(|| Error::io("Goal wall deadline unavailable"))?,
                planning_deadline_ms: auth["planning"]
                    .as_u64()
                    .ok_or_else(|| Error::io("Goal planning deadline unavailable"))?,
            };
            if !facts.result_allowed(&ResultAuthority {
                original_source_valid: original.view.started
                    && !original.view.native_stop_confirmed
                    && !original.view.stop_unknown
                    && original.executor_binding == binding
                    && original.view.epoch == epoch,
                ownership_and_digests_valid: crate::orchestration::run_scope_valid(
                    &original,
                    &request,
                    node.as_ref(),
                    issued.as_ref(),
                    facts.stage,
                    auth["planning_attempt"].as_u64().unwrap_or(0),
                    auth["latest_proposal_digest"].as_str(),
                ),
                epoch,
                predecessor_valid: original.view.phase == GoalRunPhase::Started
                    && !original.business_disposed,
            }) {
                return Err(Error::new(
                    "stale_attempt",
                    "/run",
                    "tool/candidate authority has closed",
                ));
            }
            Ok(())
        };
        let observed = clock.observe()?;
        let trusted = update_highwater(&tx, observed)?;
        check(observed, trusted)?;
        let mut budget: GoalBudget = serde_json::from_str(&budget).map_err(Error::io)?;
        let result = match charge {
            GoalBudgetCharge::Tool => budget.admit_tool(run_id),
            GoalBudgetCharge::CandidateBytes(bytes) => budget.admit_bytes(run_id, bytes),
            GoalBudgetCharge::RejectedCalls(total) => budget.observe_rejected_calls(run_id, total),
            GoalBudgetCharge::RejectedBytes(bytes) => {
                budget.rejected_bytes = budget.rejected_bytes.saturating_add(bytes);
                Ok(())
            }
        };
        tx.execute(
            "UPDATE goal_budget SET body=?1 WHERE goal_id=?2",
            params![serde_json::to_string(&budget)?, id],
        )?;
        let observed = clock.observe()?;
        let trusted = update_highwater(&tx, observed)?;
        check(observed, trusted)?;
        tx.commit()?;
        result
    }
}

/// High-frequency admission changes only monotonic counters and the corresponding remaining
/// tool/byte reservations. Merge these deltas under the state transaction instead of invalidating
/// the public business revision for every call. Other budget facts remain protected by Goal CAS.
fn merge_admission_counters(
    before: &GoalBudget,
    after: &mut GoalBudget,
    current: &GoalBudget,
) -> Result<()> {
    fn delta(old: u64, current: u64) -> Result<u64> {
        current
            .checked_sub(old)
            .ok_or_else(|| Error::io("admission counter moved backwards"))
    }
    fn add(value: &mut u64, amount: u64) -> Result<()> {
        *value = value
            .checked_add(amount)
            .ok_or_else(|| Error::io("admission counter overflow"))?;
        Ok(())
    }
    add(
        &mut after.used.tool_calls,
        delta(before.used.tool_calls, current.used.tool_calls)?,
    )?;
    add(
        &mut after.used.candidate_bytes,
        delta(before.used.candidate_bytes, current.used.candidate_bytes)?,
    )?;
    add(
        &mut after.rejected_calls,
        delta(before.rejected_calls, current.rejected_calls)?,
    )?;
    add(
        &mut after.rejected_bytes,
        delta(before.rejected_bytes, current.rejected_bytes)?,
    )?;
    for (id, old) in &before.reserved {
        let Some(now) = current.reserved.get(id) else {
            return Err(Error::io(
                "reservation changed without a Goal state revision",
            ));
        };
        if let Some(next) = after.reserved.get_mut(id) {
            next.remaining.tool_calls = next
                .remaining
                .tool_calls
                .saturating_sub(delta(now.remaining.tool_calls, old.remaining.tool_calls)?);
            next.remaining.candidate_bytes = next.remaining.candidate_bytes.saturating_sub(delta(
                now.remaining.candidate_bytes,
                old.remaining.candidate_bytes,
            )?);
        }
    }
    for (id, old) in &before.meters {
        let Some(now) = current.meters.get(id) else {
            return Err(Error::io("original Run meter disappeared"));
        };
        let Some(next) = after.meters.get_mut(id) else {
            return Err(Error::io("Run meter cannot be deleted"));
        };
        add(&mut next.tool_calls, delta(old.tool_calls, now.tool_calls)?)?;
        add(
            &mut next.candidate_bytes,
            delta(old.candidate_bytes, now.candidate_bytes)?,
        )?;
        add(
            &mut next.calls_rejected,
            delta(old.calls_rejected, now.calls_rejected)?,
        )?;
    }
    Ok(())
}

fn insert_goal_artifact(conn: &Connection, artifact: &GoalArtifact) -> Result<()> {
    conn.execute(
        "INSERT INTO goal_artifacts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            artifact.artifact.id,
            artifact.goal_id,
            artifact.node_id,
            artifact.member_id,
            artifact.plan_digest,
            artifact.schema_digest,
            artifact.artifact.sha256,
            serde_json::to_string(artifact)?,
            artifact.accepted,
            artifact.producer_run_id
        ],
    )?;
    Ok(())
}
impl Store {
    pub(crate) fn goal_save_tool_receipt(
        &self,
        goal_id: &str,
        receipt: &ArtifactReceipt,
        epoch: u64,
        binding: &str,
    ) -> Result<String> {
        let bytes = serde_json::to_vec(receipt)?;
        let sha = self.write_blob(&bytes)?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let path = format!("$.runs.{}", serde_json::to_string(&receipt.run_id)?);
        let raw: Option<String> = tx
            .query_row(
                "SELECT json_extract(body,?2) FROM goals WHERE id=?1",
                params![goal_id, path],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let run: crate::orchestration::GoalRunRecord =
            serde_json::from_str(&raw.ok_or_else(not_found)?).map_err(Error::io)?;
        if receipt.execution_id != goal_id
            || !run.view.started
            || run.view.epoch != epoch
            || run.executor_binding != binding
        {
            return Err(Error::new(
                "stale_attempt",
                "/run",
                "untrusted tool receipt producer",
            ));
        }
        tx.execute(
            "INSERT INTO goal_tool_receipts VALUES(?1,?2,?3,?4)",
            params![receipt.id, goal_id, receipt.run_id, sha],
        )?;
        tx.commit()?;
        Ok(receipt.id.clone())
    }
    pub(crate) fn goal_tool_receipt_shas(&self, goal: &str, run: &str) -> Result<Vec<String>> {
        let c = self.connection()?;
        let mut statement = c.prepare(
            "SELECT sha256 FROM goal_tool_receipts WHERE goal_id=?1 AND run_id=?2 ORDER BY rowid",
        )?;
        Ok(statement
            .query_map(params![goal, run], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub(crate) fn goal_snapshot(&self, id: &str, owner: &str) -> Result<GoalSnapshot> {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let snapshot = goal_snapshot_from(&tx, id, owner)?;
        tx.commit()?;
        Ok(snapshot)
    }
    pub(crate) fn goal_run_candidates(
        &self,
        id: &str,
        run: &str,
    ) -> Result<BTreeMap<String, GoalArtifact>> {
        let c = self.connection()?;
        let mut st=c.prepare("SELECT metadata FROM goal_artifacts WHERE goal_id=?1 AND producer_run_id=?2 AND accepted=0 ORDER BY rowid")?;
        let records = st
            .query_map(params![id, run], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut latest = BTreeMap::new();
        for raw in records {
            let artifact: GoalArtifact = serde_json::from_str(&raw).map_err(Error::io)?;
            latest.insert(artifact.artifact.slot.clone(), artifact);
        }
        Ok(latest)
    }
    /// Diagnostic writes never accept output, modify the public revision, or grant import authority.
    pub(crate) fn goal_save_candidate(
        &self,
        id: &str,
        run_id: &str,
        epoch: u64,
        binding: &str,
        artifact: &GoalArtifact,
    ) -> Result<()> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if artifact.accepted
            || artifact.goal_id != id
            || artifact.producer_run_id.as_deref() != Some(run_id)
        {
            return Err(Error::io("candidate identity mismatch"));
        }
        let path = format!("$.runs.{}", serde_json::to_string(run_id)?);
        let raw: Option<String> = tx
            .query_row(
                "SELECT json_extract(body,?2) FROM goals WHERE id=?1",
                params![id, path],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let run: crate::orchestration::GoalRunRecord =
            serde_json::from_str(&raw.ok_or_else(not_found)?).map_err(Error::io)?;
        if run.view.epoch != epoch || run.executor_binding != binding || !run.view.started {
            return Err(Error::new(
                "stale_attempt",
                "/run",
                "original candidate producer is not trusted",
            ));
        }
        insert_goal_artifact(&tx, artifact)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::{GoalRunRecord, GoalRunRequest};
    use std::sync::Arc;
    fn setup(slots: usize) -> (tempfile::TempDir, Store, GoalClock, GoalRecord) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_mode(&dir.path().join("store"), true, slots).unwrap();
        let clock = GoalClock::new(store.goal_clock_highwater().unwrap());
        let record = GoalRecord::new(
            crate::orchestration::tests::prepared_fixture(),
            "0".repeat(64),
            "host-binding".into(),
        );
        (dir, store, clock, record)
    }
    fn materialize(record: &mut GoalRecord, id: &str) {
        record.planning_attempts = 1;
        let role = GoalRunRole::Planner { attempt: 1 };
        let request_id = format!("request-{id}");
        let input = "0".repeat(64);
        record.requests.insert(
            request_id.clone(),
            GoalRunRequest {
                request_id: request_id.clone(),
                role: role.clone(),
                entry_ref: "planner@1".into(),
                epoch: record.epoch,
                input_sha: input.clone(),
                input_digest: input.clone(),
                input_artifact_ids: vec![],
                check_request_id: None,
                run_id: Some(id.into()),
                cancelled: false,
            },
        );
        assert_eq!(
            record
                .budget
                .reserve(
                    id,
                    GoalBudgetAmount {
                        active_ms: 100,
                        tool_calls: 3,
                        candidate_bytes: 30
                    },
                    None,
                    true,
                    &record.prepared.effective_limits
                )
                .unwrap(),
            crate::orchestration::goal_budget::BudgetAdmission::Granted
        );
        record.runs.insert(
            id.into(),
            GoalRunRecord {
                view: GoalRunSnapshot {
                    run_id: id.into(),
                    role,
                    entry_ref: "planner@1".into(),
                    epoch: record.epoch,
                    phase: GoalRunPhase::Reserved,
                    started: false,
                    native_stop_confirmed: false,
                    stop_unknown: false,
                    interrupted: false,
                    budget: None,
                    error: None,
                },
                request_id,
                input_sha: input,
                executor: "ex".into(),
                executor_binding: "original-binding".into(),
                candidates: BTreeMap::new(),
                started_at_utc_ms: None,
                returned_at_utc_ms: None,
                usage_sha: None,
                business_disposed: false,
                settlement_sequence: 0,
                active_ms: 0,
                allocation: GoalBudgetAmount {
                    active_ms: 100,
                    tool_calls: 3,
                    candidate_bytes: 30,
                },
                run_allocation_ms: 90,
                check_allocation_ms: 10,
                tool_receipt_shas: vec![],
                execution_outcome: None,
            },
        );
    }
    fn save(
        store: &Store,
        row: &GoalRow,
        record: &GoalRecord,
        guard: GoalWriteGuard<'_>,
        clock: &GoalClock,
    ) -> GoalRow {
        match store
            .goal_commit(
                row,
                record,
                guard,
                None,
                &GoalCommitEffects::default(),
                clock,
            )
            .unwrap()
        {
            GoalCommit::Saved(row) => *row,
            _ => panic!("unexpected replay"),
        }
    }
    fn start(store: &Store, row: GoalRow, id: &str, clock: &GoalClock) -> GoalRow {
        let mut record = row.record.clone();
        materialize(&mut record, id);
        let row = save(store, &row, &record, GoalWriteGuard::Advance, clock);
        let mut record = row.record.clone();
        let run = record.runs.get_mut(id).unwrap();
        run.view.started = true;
        run.view.phase = GoalRunPhase::Started;
        save(store, &row, &record, GoalWriteGuard::Advance, clock)
    }
    #[test]
    fn goal_submission_replays_before_capacity_and_state_budget_commit_uses_cas() {
        let (_dir, store, clock, record) = setup(2);
        let (row, created) = store
            .goal_create("goal-one", "alice", &record, 1, &clock)
            .unwrap();
        assert!(created);
        let (replayed, created) = store
            .goal_create("goal-unused", "alice", &record, 1, &clock)
            .unwrap();
        assert!(!created);
        assert_eq!(replayed.id, row.id);
        let mut changed = row.record.clone();
        changed.pause_at_utc_ms = Some(clock.observe().unwrap().utc_ms);
        changed.status = GoalStatus::Stopping;
        let saved = save(&store, &row, &changed, GoalWriteGuard::Cleanup, &clock);
        assert_eq!(saved.record.budget.transitions_used, 1);
        assert_eq!(saved.record.budget.cleanup_transitions, 1);
        assert_eq!(
            store
                .goal_commit(
                    &row,
                    &changed,
                    GoalWriteGuard::Cleanup,
                    None,
                    &Default::default(),
                    &clock
                )
                .err()
                .unwrap()
                .code,
            "revision_conflict"
        );
        let loaded = store.goal_load(&row.id).unwrap();
        assert_eq!(loaded.record.budget.cleanup_transitions, 1);
    }
    #[test]
    fn command_replay_precedes_revision_and_reuses_the_fixed_response() {
        let (_dir, store, clock, record) = setup(2);
        let (row, _) = store
            .goal_create("goal-one", "alice", &record, 1, &clock)
            .unwrap();
        let response = serde_json::json!({"accepted":true,"applied_revision":1});
        let command = CommandReceipt {
            kind: "stop".into(),
            key: "stop-key".into(),
            fingerprint: "same".into(),
            response: response.clone(),
            not_after_ms: None,
        };
        let mut changed = row.record.clone();
        changed.status = GoalStatus::Stopping;
        changed.pause_at_utc_ms = Some(clock.observe().unwrap().utc_ms);
        store
            .goal_commit(
                &row,
                &changed,
                GoalWriteGuard::Cleanup,
                Some(command.clone()),
                &Default::default(),
                &clock,
            )
            .unwrap();
        let replay = store
            .goal_commit(
                &row,
                &changed,
                GoalWriteGuard::Advance,
                Some(command),
                &Default::default(),
                &clock,
            )
            .unwrap();
        assert!(matches!(replay,GoalCommit::Replayed(value) if value==response));
        assert_eq!(
            store
                .goal_command(&row.id, "alice", "stop", "stop-key", "different")
                .unwrap_err()
                .code,
            "request_conflict"
        );
    }
    #[test]
    fn failed_output_insertion_rolls_back_goal_budget_and_revision() {
        let (_dir, store, clock, record) = setup(2);
        let (row, _) = store
            .goal_create("goal-one", "alice", &record, 1, &clock)
            .unwrap();
        let mut changed = row.record.clone();
        changed.status = GoalStatus::Waiting;
        let artifact = GoalArtifact {
            protocol_version: 1,
            artifact: ArtifactRef {
                id: "artifact-one".into(),
                sha256: "0".repeat(64),
                slot: "report".into(),
                state_id: "_tl.node.review".into(),
                visit: 1,
            },
            goal_id: "other-goal".into(),
            node_id: Some("review".into()),
            member_id: None,
            plan_digest: None,
            schema_ref: Some("report".into()),
            schema_digest: Some("0".repeat(64)),
            accepted: true,
            producer_run_id: None,
            attachment_policy_digest: None,
            producer_kind: "public".into(),
        };
        let effects = GoalCommitEffects {
            artifacts: vec![artifact],
            ..Default::default()
        };
        assert!(
            store
                .goal_commit(
                    &row,
                    &changed,
                    GoalWriteGuard::Advance,
                    None,
                    &effects,
                    &clock
                )
                .is_err()
        );
        let loaded = store.goal_load(&row.id).unwrap();
        assert_eq!(loaded.revision, 0);
        assert_eq!(loaded.record.status, GoalStatus::Running);
        assert_eq!(loaded.record.budget.transitions_used, 1);
    }
    #[test]
    fn concurrent_tool_admission_charges_run_and_goal_together_without_state_edges() {
        let (_dir, store, clock, record) = setup(2);
        let (row, _) = store
            .goal_create("goal-one", "alice", &record, 1, &clock)
            .unwrap();
        let row = start(&store, row, "run-one", &clock);
        let transitions = row.record.budget.transitions_used;
        let store = Arc::new(store);
        let clock = Arc::new(clock);
        let jobs = (0..20)
            .map(|_| {
                let store = store.clone();
                let clock = clock.clone();
                std::thread::spawn(move || {
                    store
                        .goal_budget_admit(
                            "goal-one",
                            "run-one",
                            1,
                            "original-binding",
                            GoalBudgetCharge::Tool,
                            &clock,
                        )
                        .is_ok()
                })
            })
            .collect::<Vec<_>>();
        let accepted = jobs
            .into_iter()
            .map(|job| job.join().unwrap())
            .filter(|accepted| *accepted)
            .count();
        assert_eq!(accepted, 3);
        let current = store.goal_load("goal-one").unwrap();
        assert_eq!(current.revision, row.revision);
        assert_eq!(current.record.budget.used.tool_calls, 3);
        assert_eq!(
            current.record.budget.reserved["run-one"]
                .remaining
                .tool_calls,
            0
        );
        assert_eq!(current.record.budget.rejected_calls, 17);
        assert_eq!(current.record.budget.transitions_used, transitions);
    }
    #[test]
    fn cancellation_changes_business_epoch_but_original_run_can_still_settle() {
        let (_dir, store, clock, record) = setup(2);
        let (row, _) = store
            .goal_create("goal-one", "alice", &record, 1, &clock)
            .unwrap();
        let row = start(&store, row, "run-one", &clock);
        let mut cancelled = row.record.clone();
        // Admit after the snapshot: cancellation must merge rather than erase these facts.
        store
            .goal_budget_admit(
                "goal-one",
                "run-one",
                1,
                "original-binding",
                GoalBudgetCharge::Tool,
                &clock,
            )
            .unwrap();
        store
            .goal_budget_admit(
                "goal-one",
                "run-one",
                1,
                "original-binding",
                GoalBudgetCharge::CandidateBytes(7),
                &clock,
            )
            .unwrap();
        cancelled.epoch += 1;
        cancelled.status = GoalStatus::Stopping;
        cancelled.terminal_intent = Some(GoalTerminalIntent {
            revision: row.revision + 1,
            at_utc_ms: clock.observe().unwrap().utc_ms,
            outcome: GoalOutcome::Cancelled,
            reason: "user_cancelled".into(),
            subject: "goal-one".into(),
        });
        let row = save(&store, &row, &cancelled, GoalWriteGuard::Cleanup, &clock);
        assert_eq!(row.record.budget.used.tool_calls, 1);
        assert_eq!(row.record.budget.used.candidate_bytes, 7);
        assert_eq!(row.record.budget.meters["run-one"].tool_calls, 1);
        assert_eq!(
            row.record.budget.reserved["run-one"]
                .remaining
                .candidate_bytes,
            23
        );
        assert_eq!(
            store
                .goal_budget_admit(
                    "goal-one",
                    "run-one",
                    1,
                    "original-binding",
                    GoalBudgetCharge::Tool,
                    &clock
                )
                .unwrap_err()
                .code,
            "stale_attempt"
        );
        let mut settled = row.record.clone();
        settled
            .budget
            .settle("run-one", 1, 5000, true, &settled.prepared.effective_limits)
            .unwrap();
        let run = settled.runs.get_mut("run-one").unwrap();
        run.view.native_stop_confirmed = true;
        run.view.phase = GoalRunPhase::Settled;
        run.business_disposed = true;
        run.settlement_sequence = 1;
        let row = save(
            &store,
            &row,
            &settled,
            GoalWriteGuard::Settlement {
                run_id: "run-one",
                epoch: 1,
                binding: "original-binding",
            },
            &clock,
        );
        assert_eq!(row.record.budget.used.active_ms, 5000);
        assert!(row.record.budget.reserved.is_empty());
        let held: bool = store
            .connection()
            .unwrap()
            .query_row(
                "SELECT slot_held FROM run_leases WHERE run_id='run-one'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!held);
    }
    #[test]
    fn global_slots_are_shared_across_goal_records_and_do_not_trim_unknown_runs() {
        let (_dir, store, clock, record) = setup(1);
        let (row, _) = store
            .goal_create("goal-one", "alice", &record, 3, &clock)
            .unwrap();
        let row = start(&store, row, "run-one", &clock);
        let mut unknown = row.record.clone();
        unknown.runs.get_mut("run-one").unwrap().view.stop_unknown = true;
        unknown.status = GoalStatus::Blocked;
        unknown.blockers.insert(GoalBlocker {
            reason: "stop_unknown".into(),
            subject: "run-one".into(),
        });
        save(&store, &row, &unknown, GoalWriteGuard::Cleanup, &clock);
        let mut other = record;
        other.prepared.request.request_key = "other".into();
        other.prepared.request_fingerprint = "other".into();
        let (other_row, _) = store
            .goal_create("goal-two", "alice", &other, 3, &clock)
            .unwrap();
        materialize(&mut other, "run-two");
        assert_eq!(
            store
                .goal_commit(
                    &other_row,
                    &other,
                    GoalWriteGuard::Advance,
                    None,
                    &Default::default(),
                    &clock
                )
                .err()
                .unwrap()
                .code,
            "dispatch_capacity"
        );
        assert!(store.goal_load("goal-two").unwrap().record.runs.is_empty());
        let count: i64 = store
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM run_leases WHERE slot_held=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}
