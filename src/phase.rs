use crate::{
    definition::validate_schema,
    runtime::{AttemptRecord, CheckAudit, Inner, PhaseRecord, decode, enter, now_ms, requirements},
    store::{Commit, Row},
    *,
};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

struct FlowRunOwner {
    inner: Arc<Inner>,
    execution_id: String,
    run_id: String,
    epoch: u64,
    budget: Arc<crate::budget::BudgetState>,
}
impl crate::managed::ManagedRunOwner for FlowRunOwner {
    fn active(&self) -> Result<()> {
        let row = self.inner.store.load(&self.execution_id)?;
        let r = decode(&row)?;
        if now_ms() >= r.deadline_ms {
            self.budget.mark(crate::BudgetLimit::TaskWallTime);
            return Err(Error::new(
                "limit_reached",
                "/budget/task_wall",
                "task wall deadline reached",
            ));
        }
        if r.progress["phase"] != "executing"
            || r.status != Status::Running
            || r.epoch != self.epoch
            || r.active.as_ref().is_none_or(|a| a.id != self.run_id)
        {
            return Err(Error::new(
                "stale_attempt",
                "/run",
                "execution no longer has authority",
            ));
        }
        Ok(())
    }
    fn persist_budget(&self, snapshot: &crate::RunBudgetSnapshot) -> Result<()> {
        for _ in 0..4 {
            let row = self.inner.store.load(&self.execution_id)?;
            let mut r = decode(&row)?;
            if r.epoch != self.epoch
                || r.progress["phase"] != "executing"
                || r.status != Status::Running
                || r.active.as_ref().is_none_or(|a| a.id != self.run_id)
            {
                return Err(Error::new(
                    "stale_attempt",
                    "/budget",
                    "budget scope no longer active",
                ));
            }
            r.budgets.insert(self.run_id.clone(), snapshot.clone());
            match self.inner.save(&row, &r, false, None, &[]) {
                Ok(_) => return Ok(()),
                Err(e) if e.code == "revision_conflict" => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::new(
            "revision_conflict",
            "/budget",
            "could not persist budget facts",
        ))
    }
    fn persist_candidate(&self, slot: &str, sha: &str) -> Result<()> {
        let mut saved = false;
        for _ in 0..4 {
            let row = self.inner.store.load(&self.execution_id)?;
            let mut record = decode(&row)?;
            if record.epoch != self.epoch || record.status != Status::Running {
                return Err(Error::new(
                    "stale_attempt",
                    "/run",
                    "execution lost authority while writing",
                ));
            }
            record
                .candidate_files
                .entry(self.run_id.clone())
                .or_default()
                .insert(slot.to_owned(), sha.to_owned());
            match self.inner.save(&row, &record, false, None, &[]) {
                Ok(_) => {
                    saved = true;
                    break;
                }
                Err(e) if e.code == "revision_conflict" => continue,
                Err(e) => return Err(e),
            }
        }
        if !saved {
            return Err(Error::new(
                "revision_conflict",
                "/artifact",
                "could not persist candidate reference",
            ));
        }
        Ok(())
    }
    fn persist_receipt(&self, receipt: ArtifactReceipt) -> Result<String> {
        let receipt_id = receipt.id.clone();
        for _ in 0..4 {
            let row = self.inner.store.load(&self.execution_id)?;
            let mut r = decode(&row)?;
            r.receipts.push(receipt.clone());
            match self.inner.save(&row, &r, false, None, &[]) {
                Ok(_) => return Ok(receipt_id),
                Err(e) if e.code == "revision_conflict" => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::new(
            "revision_conflict",
            "/receipt",
            "concurrent state changes prevented receipt persistence",
        ))
    }
}
pub(crate) async fn run(inner: Arc<Inner>, row: Row) {
    let trace = decode(&row).ok().and_then(|r| {
        let a = r.active.as_ref()?;
        let trace = inner.trace.as_ref()?.session(
            &row.id,
            &a.id,
            &r.current_state,
            *r.visits.get(&r.current_state)?,
        );
        let executor = inner.bindings.executors.get(&a.executor)?;
        if trace.reserve_executor(&a.executor, &executor.identity()) {
            Some(trace)
        } else {
            None
        }
    });
    if let Some(trace) = &trace {
        trace.emit("phase.started", &trace.phase_span(), None, json!({}));
    }
    let mut execution_stopped = true;
    if let Err(error) = run_checked(
        inner.clone(),
        row.clone(),
        &mut execution_stopped,
        trace.clone(),
    )
    .await
        && let Ok(latest) = inner.store.load(&row.id)
        && let Ok(mut record) = decode(&latest)
        && !record.status.terminal()
        && record.status != Status::Cancelling
    {
        record.status = Status::Blocked;
        record.error = Some(error);
        if execution_stopped {
            record.active = None;
            record.stop_unknown = false;
        } else {
            record.stop_unknown = true;
        }
        let _ = inner.save(&latest, &record, false, None, &[]);
    }
    // Execution already returned a confirmed outcome. A losing final CAS must not
    // discard that stop fact or force an in-process executor to rediscover it.
    if execution_stopped {
        loop {
            let Ok(latest) = inner.store.load(&row.id) else {
                break;
            };
            let Ok(mut record) = decode(&latest) else {
                break;
            };
            if record.status != Status::Cancelling {
                break;
            }
            record.status = Status::Cancelled;
            record.active = None;
            record.stop_unknown = false;
            record.error = None;
            record.business_outcome = None;
            record.progress = json!({"phase":"cancelled"});
            match inner.save(&latest, &record, false, None, &[]) {
                Ok(_) => break,
                Err(e) if e.code == "revision_conflict" => tokio::task::yield_now().await,
                Err(_) => break,
            }
        }
    }
    if let Some(trace) = &trace {
        let data=inner.store.load(&row.id).and_then(|r|decode(&r)).ok().map(|r|json!({"status":r.status,"stop_unknown":r.stop_unknown,"code":r.error.map(|e|e.code)})).unwrap_or_else(||json!({"status":"unknown"}));
        trace.emit("phase.finished", &trace.phase_span(), None, data);
    }
    if let Ok(r) = decode(&row)
        && let Some(active) = r.active
        && let Ok(mut live) = inner.live_budgets.lock()
    {
        live.remove(&active.id);
    }
    if let Ok(mut handles) = inner.handles.lock() {
        handles.remove(&row.id);
    }
}
async fn run_checked(
    inner: Arc<Inner>,
    row: Row,
    execution_stopped: &mut bool,
    trace: Option<Arc<crate::trace::RunTrace>>,
) -> Result<()> {
    let mut record = decode(&row)?;
    let definition = inner.store.definition(&record.definition_digest)?;
    if record.binding_fingerprint != inner.binding_fingerprint {
        record.status = Status::Blocked;
        record.error = Some(Error::new(
            "binding_changed",
            "/bindings",
            "locked host implementation/configuration is unavailable; create a new execution",
        ));
        inner.save(&row, &record, false, None, &[])?;
        return Ok(());
    }
    let State::Work {
        entry,
        accepted,
        rejected,
        failed,
    } = definition.bundle.flow.states[&record.current_state].clone()
    else {
        return Err(Error::new(
            "definition_invalid",
            "/state",
            "queued state is not work",
        ));
    };
    let contract = definition.bundle.entries[&entry].clone();
    let executor = inner
        .bindings
        .executors
        .get(&contract.executor)
        .cloned()
        .ok_or_else(|| Error::new("binding_missing", "/executor", "executor unavailable"))?;
    let grant = inner
        .bindings
        .grants
        .get(&record.grant_ref)
        .ok_or_else(|| Error::new("capability_denied", "/grant", "grant no longer available"))?;
    if !contract.tools.iter().all(|t| grant.tools.contains(t)) {
        return Err(Error::new(
            "capability_denied",
            "/tools",
            "entry requires tools outside current grant",
        ));
    }
    if record.deadline_ms <= now_ms() || record.active_ms >= record.limits.max_active_ms {
        return Err(Error::new(
            "limit_reached",
            "/dispatch",
            "queued execution exhausted its budget before dispatch",
        ));
    }
    let references = requirements(&record, &definition, &contract.requires)?;
    let stage_input = json!({"flow_input":record.inputs,"input_artifact_ids":references.iter().map(|a|&a.id).collect::<Vec<_>>(),"signal_fact_ids":record.signals.iter().map(|s|&s.id).collect::<Vec<_>>()});
    definition.validate_value(&contract.input_schema, &stage_input)?;
    let mut artifacts = Vec::new();
    for reference in &references {
        artifacts.push(ArtifactData {
            bytes: inner.store.read_blob(&reference.sha256)?.into(),
            reference: reference.clone(),
        });
    }
    let mut active = record
        .active
        .clone()
        .ok_or_else(|| Error::new("stale_attempt", "/run", "queued run missing"))?;
    active.started = true;
    active.started_ms = now_ms();
    record.active = Some(active.clone());
    record.progress = json!({"phase":"executing"});
    let cancel = inner.stop.child_token();
    {
        // cancel() uses this same lock before deciding whether execution is absent.
        // Publish the started fact and its cancellation handle as one coordinated admission.
        let mut handles = inner.handles.lock().map_err(Error::io)?;
        if !matches!(
            inner.save(&row, &record, false, None, &[])?,
            Commit::Saved(_)
        ) {
            return Ok(());
        }
        handles.insert(row.id.clone(), cancel.clone());
    }
    let visit = record.visits[&record.current_state];
    let context = ToolContext {
        inputs: record.inputs.clone(),
        state_id: record.current_state.clone(),
        visit,
        accepted: artifacts.clone(),
        signal_facts: record.signals.clone(),
        cancellation: cancel.clone(),
        goal: None,
    };
    let explicit = inner.options.entry_budgets.contains_key(&entry);
    let policy = inner
        .options
        .entry_budgets
        .get(&entry)
        .cloned()
        .unwrap_or_else(|| {
            crate::RunBudgetPolicy::legacy(
                inner.options.max_tool_calls_per_run,
                inner.options.max_candidate_bytes,
            )
        });
    let preparation_ms = now_ms().saturating_sub(active.started_ms);
    let budget = Arc::new(crate::budget::BudgetState::new(
        &active.id,
        &record.current_state,
        &entry,
        policy,
        explicit,
        record
            .limits
            .max_active_ms
            .saturating_sub(record.active_ms)
            .saturating_sub(preparation_ms),
        record.deadline_ms.saturating_sub(now_ms()),
        preparation_ms,
        inner.options.max_tool_calls_per_run,
        inner.options.max_candidate_bytes,
        executor.budget_limits(),
    )?);
    let latest_budget_row = inner.store.load(&row.id)?;
    let mut budget_record = decode(&latest_budget_row)?;
    budget_record
        .budgets
        .insert(active.id.clone(), budget.snapshot());
    inner.save(&latest_budget_row, &budget_record, false, None, &[])?;
    inner
        .live_budgets
        .lock()
        .map_err(Error::io)?
        .insert(active.id.clone(), budget.clone());
    let bridge = Arc::new(crate::managed::ManagedBridge {
        inner: inner.clone(),
        execution_id: row.id.clone(),
        run_id: active.id.clone(),
        owner: Arc::new(FlowRunOwner {
            inner: inner.clone(),
            execution_id: row.id.clone(),
            run_id: active.id.clone(),
            epoch: active.epoch,
            budget: budget.clone(),
        }),
        entry: contract.clone(),
        resources: crate::managed::RunResources::Flow(Box::new(definition.clone())),
        context,
        candidates: Mutex::new(BTreeMap::new()),
        budget: budget.clone(),
        storage_failure: Mutex::new(None),
        trace: trace.clone(),
        json_candidate_max: None,
    });
    let time_left = budget.snapshot().hint.remaining_ms;
    let output_schemas = contract
        .outputs
        .iter()
        .map(|(slot, out)| (slot.clone(), definition.schemas[&out.schema].clone()))
        .collect();
    let invocation = Invocation {
        stage_input,
        output_schemas,
        execution_id: row.id.clone(),
        run_id: active.id.clone(),
        state_id: record.current_state.clone(),
        visit,
        entry: contract.clone(),
        skills: definition
            .skills
            .iter()
            .filter(|(id, _)| contract.skills.contains(id))
            .map(|(id, p)| (id.clone(), p.clone()))
            .collect(),
        inputs: record.inputs.clone(),
        artifacts: references,
        signal_facts: record.signals.clone(),
        work_dir: inner.store.run_dir(&active.id)?,
        timeout_ms: time_left,
        budget: Some(budget.snapshot()),
    };
    *execution_stopped = false;
    let execution = crate::managed::execute_managed(
        executor.clone(),
        invocation,
        bridge.clone(),
        budget.clone(),
        cancel.clone(),
        trace.clone(),
        inner.trace.is_some(),
    )
    .await;
    let outcome = execution.outcome;
    let internal_stop = execution.internal_stop;
    *execution_stopped = execution.native_stop_confirmed;
    let latest = inner.store.load(&row.id)?;
    let mut current = decode(&latest)?;
    current.budgets.insert(active.id.clone(), budget.snapshot());
    current.attempts.push(AttemptRecord {
        run_id: active.id.clone(),
        state: record.current_state.clone(),
        visit,
        finished_ms: now_ms(),
        outcome: outcome.clone(),
    });
    current.active_ms = current
        .active_ms
        .saturating_add(preparation_ms.saturating_add(budget.snapshot().elapsed_ms));
    if matches!(outcome, ExecutionOutcome::StopUnknown { .. }) {
        if current.status != Status::Cancelling {
            current.status = Status::Blocked;
        }
        current.stop_unknown = true;
        current.error = Some(Error::new(
            "stop_unconfirmed",
            "/run",
            "adapter could not confirm execution stopped",
        ));
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    if current.status == Status::Cancelling
        || current.epoch != active.epoch
        || (cancel.is_cancelled() && !internal_stop)
    {
        current.status = Status::Cancelled;
        current.active = None;
        current.stop_unknown = false;
        current.progress = json!({"phase":"cancelled"});
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    if current.active.as_ref().is_none_or(|a| a.id != active.id) {
        return Err(Error::new("stale_attempt", "/run", "late result"));
    }
    if let Some(error) = bridge.storage_failure.lock().map_err(Error::io)?.clone() {
        current.status = Status::Blocked;
        current.active = None;
        current.stop_unknown = false;
        current.business_outcome = None;
        current.error = Some(error);
        current.progress = json!({"phase":"blocked"});
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    match outcome {
        ExecutionOutcome::Cancelled => {
            current.status = Status::Cancelled;
            current.active = None;
            inner.save(&latest, &current, false, None, &[])?;
            return Ok(());
        }
        ExecutionOutcome::Failed { code, message } => {
            enter(&mut current, &definition, &failed, true)?;
            current.error = Some(Error::new(&code, "/executor", message));
            inner.save(&latest, &current, true, None, &[])?;
            return Ok(());
        }
        _ => {}
    }
    let candidates = bridge.candidates.lock().map_err(Error::io)?.clone();
    for (slot, output) in &contract.outputs {
        if output.required && !candidates.contains_key(slot) {
            enter(&mut current, &definition, &rejected, true)?;
            current.error = Some(Error::new(
                "artifact_invalid",
                format!("/outputs/{slot}"),
                "required candidate missing",
            ));
            inner.save(&latest, &current, true, None, &[])?;
            return Ok(());
        }
    }
    let check_ref = format!("{}:check-{}", row.id, uuid::Uuid::new_v4());
    current.checks.push(CheckAudit {
        id: check_ref.clone(),
        run_id: active.id.clone(),
        gate: contract.gate.clone(),
        candidate_digests: candidates
            .iter()
            .map(|(slot, c)| (slot.clone(), c.sha256()))
            .collect(),
        result: None,
    });
    current.progress = json!({"phase":"checking"});
    let saved = inner.save(&latest, &current, false, None, &[])?;
    let Commit::Saved(check_row) = saved else {
        return Ok(());
    };

    let facts: Vec<_> = current
        .receipts
        .iter()
        .filter(|f| f.run_id == active.id)
        .cloned()
        .collect();
    let gate = self_gate(&inner, &contract.gate)?;
    let authorized_evidence: BTreeSet<_> =
        artifacts.iter().map(|a| a.reference.id.clone()).collect();
    let context = GateContext {
        inputs: current.inputs.clone(),
        state_id: current.current_state.clone(),
        visit,
        candidates: candidates.clone(),
        accepted: artifacts,
        tool_facts: facts.clone(),
        signal_facts: current.signals.clone(),
        check_evidence_ref: check_ref.clone(),
        cancellation: cancel.clone(),
    };
    let check_clock = tokio::time::Instant::now();
    let check_budget = budget.check_start(
        current
            .limits
            .max_active_ms
            .saturating_sub(current.active_ms),
        current.deadline_ms.saturating_sub(now_ms()),
    );
    current.budgets.insert(active.id.clone(), budget.snapshot());
    let Commit::Saved(check_row) = inner.save(&check_row, &current, false, None, &[])? else {
        return Ok(());
    };
    if check_budget == 0 {
        current.status = Status::Blocked;
        current.active = None;
        current.error = Some(Error::new(
            "limit_reached",
            "/gate",
            "execution budget exhausted before domain check",
        ));
        current.progress = json!({"phase":"blocked"});
        inner.save(&check_row, &current, false, None, &[])?;
        return Ok(());
    }
    let check_span = crate::trace::RunTrace::span();
    if let Some(t) = &trace {
        t.emit(
            "check.started",
            &check_span,
            Some(t.phase_span()),
            json!({"gate":contract.gate,"check_ref":check_ref}),
        );
    }
    let mut checked = tokio::select! {
        _=cancel.cancelled()=>CheckResult::unavailable("check cancelled"),
        result=tokio::time::timeout(std::time::Duration::from_millis(check_budget),AssertUnwindSafe(gate.check(context)).catch_unwind())=>match result {
            Ok(Ok(v))=>v,
            Ok(Err(_))=>CheckResult::unavailable("gate callback panicked"),
            Err(_)=>{budget.mark(crate::BudgetLimit::CheckTime);CheckResult::unavailable("gate timed out")}
        }
    };
    if check_clock.elapsed() >= std::time::Duration::from_millis(check_budget) {
        budget.mark(crate::BudgetLimit::CheckTime);
        checked = CheckResult::unavailable("gate returned after the check deadline");
    }
    if let Some(t) = &trace {
        t.emit(
            "check.finished",
            &check_span,
            None,
            json!({"verdict":checked.verdict}),
        );
    }
    let latest = inner.store.load(&row.id)?;
    let mut current = decode(&latest)?;
    budget.check_end(check_clock.elapsed().as_millis().min(u64::MAX as u128) as u64);
    current.budgets.insert(active.id.clone(), budget.snapshot());
    current.active_ms = current
        .active_ms
        .saturating_add(check_clock.elapsed().as_millis().min(u64::MAX as u128) as u64);
    if current.status == Status::Cancelling || current.epoch != active.epoch {
        current.status = Status::Cancelled;
        current.active = None;
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    if now_ms() >= current.deadline_ms || current.active_ms >= current.limits.max_active_ms {
        current.status = Status::Blocked;
        current.active = None;
        current.error = Some(Error::new(
            "limit_reached",
            "/gate",
            "check exhausted execution budget",
        ));
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    if let Some(audit) = current.checks.iter_mut().find(|a| a.id == check_ref) {
        audit.result = Some(checked.clone());
    }
    let checked_json = serde_json::to_value(&checked)?;
    let schema: Value = serde_json::from_str(include_str!("../schemas/check-result.schema.json"))?;
    let valid = validate_schema(&schema, &checked_json, "/gate").is_ok()
        && checked.evidence_refs.iter().all(|e| {
            e == &check_ref || facts.iter().any(|f| &f.id == e) || authorized_evidence.contains(e)
        });
    if !valid || matches!(checked.verdict, Verdict::Error) {
        current.status = Status::Blocked;
        current.active = None;
        current.error = Some(Error::new(
            if valid {
                "check_unavailable"
            } else {
                "gate_protocol_invalid"
            },
            "/gate",
            "check did not produce a usable trusted result",
        ));
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    }
    if matches!(checked.verdict, Verdict::Fail) {
        enter(&mut current, &definition, &rejected, true)?;
        current.error = Some(Error::new(
            "domain_check_failed",
            "/gate",
            checked
                .violations
                .first()
                .map(|v| v.message.clone())
                .unwrap_or_default(),
        ));
        inner.save(&latest, &current, true, None, &[])?;
        return Ok(());
    }
    let Some(target) = checked
        .outcome
        .as_ref()
        .and_then(|o| accepted.get(o))
        .cloned()
    else {
        current.status = Status::Blocked;
        current.active = None;
        current.error = Some(Error::new(
            "gate_protocol_invalid",
            "/gate/outcome",
            "outcome not declared",
        ));
        inner.save(&latest, &current, false, None, &[])?;
        return Ok(());
    };
    let mut accepted_refs = Vec::new();
    for candidate in candidates.values() {
        let sha = inner.store.write_blob(&candidate.bytes)?;
        accepted_refs.push(ArtifactRef {
            id: format!("{}:artifact-{}", row.id, uuid::Uuid::new_v4()),
            sha256: sha,
            slot: candidate.slot.clone(),
            state_id: current.current_state.clone(),
            visit,
        });
    }
    current.history.push(PhaseRecord {
        run_id: active.id.clone(),
        state: current.current_state.clone(),
        visit,
        artifacts: accepted_refs.clone(),
        check: checked,
    });
    enter(&mut current, &definition, &target, true)?;
    inner.save(&latest, &current, true, None, &accepted_refs)?;
    inner.notify.notify_one();
    let _ = check_row;
    Ok(())
}
fn self_gate(inner: &Inner, id: &str) -> Result<Arc<dyn DomainGate>> {
    inner
        .bindings
        .gates
        .get(id)
        .cloned()
        .ok_or_else(|| Error::new("binding_missing", "/gate", "gate unavailable"))
}
