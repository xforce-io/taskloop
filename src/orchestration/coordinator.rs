//! Typed coordination of persisted results. Native completion and business acceptance are separate.
use super::{
    check::IssuedCheck,
    core::{GoalCore, set_terminal},
    plan::bounded_json,
    record::*,
    *,
};
use crate::{
    store::goals::{GoalCommit, GoalRow, GoalWriteGuard},
    *,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use tokio::time::Instant;

impl GoalCore {
    /// R03 persists checking before validating; a crash at this boundary cannot replay acceptance.
    pub(crate) fn consume_return(&self, id: &str, run_id: &str) -> Result<bool> {
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        let row = self.inner.store.goal_load(id)?;
        let run = row
            .record
            .runs
            .get(run_id)
            .ok_or_else(|| Error::io("returned Run missing"))?;
        if run.view.phase != GoalRunPhase::ExecutionReturned || run.business_disposed {
            return Ok(false);
        }
        let observed = self.inner.store.observe_goal_clock(&self.clock)?;
        if !run.view.native_stop_confirmed {
            if row.record.blockers.contains(&GoalBlocker {
                reason: "stop_unknown".into(),
                subject: run_id.into(),
            }) {
                return Ok(false);
            }
            let mut next = row.record.clone();
            next.blockers.insert(GoalBlocker {
                reason: "stop_unknown".into(),
                subject: run_id.into(),
            });
            next.status = GoalStatus::Blocked;
            self.commit_goal(
                &row,
                &next,
                GoalWriteGuard::Settlement {
                    run_id,
                    epoch: run.view.epoch,
                    binding: &run.executor_binding,
                },
                None,
                &Default::default(),
                &self.clock,
            )?;
            return Ok(true);
        }
        if !row
            .record
            .permissions(observed.utc_ms, observed.trusted)
            .result_allowed(&row.record.result_authority(run_id, &run.executor_binding))
        {
            self.dispose_stale(&row, run_id, 0, observed.utc_ms)?;
            return Ok(true);
        }
        let mut next = row.record.clone();
        next.runs.get_mut(run_id).expect("returned Run").view.phase = GoalRunPhase::Checking;
        let row = match self.commit_goal(
            &row,
            &next,
            GoalWriteGuard::Result {
                run_id,
                binding: &run.executor_binding,
            },
            None,
            &Default::default(),
            &self.clock,
        )? {
            GoalCommit::Saved(row) => row,
            GoalCommit::Replayed(_) => return Err(Error::io("internal result replay")),
        };
        let checking_at = Instant::now();
        let run = &row.record.runs[run_id];
        let validation = self.validate_member_outputs(run);
        let mut next = row.record.clone();
        let mut effects = GoalCommitEffects::default();
        next.runs
            .get_mut(run_id)
            .expect("original Run")
            .business_disposed = true;
        let application = match validation {
            Err(error) => self.apply_execution_failure(
                &mut next,
                run_id,
                error,
                row.revision + 1,
                observed.utc_ms,
            ),
            Ok(outputs) => match &run.view.role {
                GoalRunRole::Planner { attempt } => self.accept_planner(
                    &row,
                    &mut next,
                    &outputs,
                    *attempt,
                    &mut effects,
                    observed.utc_ms,
                ),
                GoalRunRole::PlanCheck { .. } | GoalRunRole::FinalCheck { .. } => self
                    .accept_check(
                        &row,
                        &mut next,
                        run_id,
                        &outputs,
                        &mut effects,
                        observed.utc_ms,
                    ),
                GoalRunRole::NodeMember { node_id, member_id } => {
                    let member = next
                        .nodes
                        .get_mut(node_id)
                        .and_then(|n| n.view.members.get_mut(member_id))
                        .ok_or_else(|| Error::io("original member missing"))?;
                    member.status = GoalMemberStatus::Succeeded;
                    let candidates = self.inner.store.goal_run_candidates(id, run_id)?;
                    member.outputs = candidates
                        .into_iter()
                        .filter(|(slot, _)| outputs.contains_key(slot))
                        .map(|(slot, a)| (slot, a.artifact))
                        .collect();
                    Ok(())
                }
            },
        };
        let elapsed = checking_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
        if let Err(error) = application {
            next = row.record.clone();
            effects = GoalCommitEffects::default();
            self.apply_execution_failure(
                &mut next,
                run_id,
                error,
                row.revision + 1,
                observed.utc_ms,
            )?;
        }
        if elapsed > run.check_allocation_ms {
            next = row.record.clone();
            effects = GoalCommitEffects::default();
            self.apply_execution_failure(
                &mut next,
                run_id,
                Error::new(
                    "check_timeout",
                    "/check",
                    "result validation exceeded its reserved check time",
                ),
                row.revision + 1,
                observed.utc_ms,
            )?;
        }
        settle_run(&mut next, run_id, elapsed)?;
        if let GoalRunRole::NodeMember { node_id, .. } = &run.view.role
            && next.terminal_intent.is_none()
            && let Err(error) = self.begin_node_check(&mut next, node_id, observed.utc_ms)
        {
            next.nodes
                .get_mut(node_id)
                .expect("original node")
                .view
                .status = GoalNodeStatus::Failed;
            next.nodes
                .get_mut(node_id)
                .expect("original node")
                .view
                .error = Some(error.clone());
            set_terminal(
                &mut next,
                GoalOutcome::Failed,
                &error.code,
                node_id,
                row.revision + 1,
                observed.utc_ms,
            );
            next.error = Some(error);
        }
        self.cleanup_terminal(&mut next)?;
        self.close_stopping(&mut next, observed.utc_ms)?;
        let saved = self.commit_goal(
            &row,
            &next,
            GoalWriteGuard::Result {
                run_id,
                binding: &run.executor_binding,
            },
            None,
            &effects,
            &self.clock,
        );
        match saved {
            Ok(_) => {}
            Err(e) if e.code == "stale_attempt" => {
                let latest = self.inner.store.goal_load(id)?;
                self.dispose_stale(&latest, run_id, elapsed, self.clock.observe()?.utc_ms)?;
            }
            Err(e) => return Err(e),
        }
        self.cancel_goal_handles(&next);
        self.inner.notify.notify_one();
        Ok(true)
    }
    fn validate_member_outputs(&self, run: &GoalRunRecord) -> Result<BTreeMap<String, Value>> {
        match &run.execution_outcome {
            Some(ExecutionOutcome::Completed { .. }) => {}
            Some(ExecutionOutcome::Failed { code, message }) => {
                return Err(Error::new(
                    code,
                    "/executor",
                    message.chars().take(4000).collect::<String>(),
                ));
            }
            Some(ExecutionOutcome::Cancelled) => {
                return Err(Error::new(
                    "interrupted",
                    "/executor",
                    "native invocation was cancelled before business acceptance",
                ));
            }
            _ => {
                return Err(Error::new(
                    "executor_failed",
                    "/executor",
                    "native completion fact is unavailable",
                ));
            }
        }
        let entry = &self.registry.entries[&run.view.entry_ref].contract;
        let mut outputs = BTreeMap::new();
        for (slot, contract) in &entry.outputs {
            let Some(sha) = run.candidates.get(slot) else {
                if contract.required {
                    return Err(Error::new(
                        "artifact_invalid",
                        format!("/outputs/{slot}"),
                        "required output was not submitted",
                    ));
                }
                continue;
            };
            let bytes = self.inner.store.read_blob(sha)?;
            if bytes.len() > self.options.max_json_candidate_bytes.min(4_194_304) {
                return Err(Error::new(
                    "artifact_invalid",
                    "/outputs",
                    "JSON candidate exceeds host limit",
                ));
            }
            let value = crate::parse_unique(&bytes)?;
            self.registry.schemas.validate(&contract.schema, &value)?;
            outputs.insert(slot.clone(), value);
        }
        Ok(outputs)
    }
    fn apply_execution_failure(
        &self,
        next: &mut GoalRecord,
        run_id: &str,
        error: Error,
        revision: u64,
        now: u64,
    ) -> Result<()> {
        let role = next.runs[run_id].view.role.clone();
        next.runs
            .get_mut(run_id)
            .expect("original Run")
            .business_disposed = true;
        next.runs.get_mut(run_id).expect("original Run").view.error = Some(error.clone());
        if matches!(role, GoalRunRole::Planner { .. }) {
            let sha = next.runs[run_id]
                .candidates
                .get("proposal")
                .cloned()
                .unwrap_or_else(|| crate::json::digest(error.message.as_bytes()));
            next.proposals.push(ProposalRecord {
                attempt: next.planning_attempts,
                candidate_sha: sha,
                validated_proposal_sha: None,
                proposal_digest: None,
                failure: Some(plan_failure(error)),
                check_ids: vec![],
            });
            self.retry_planner(next, revision, now)?;
        } else {
            if let GoalRunRole::NodeMember { node_id, member_id } = role {
                let node = next
                    .nodes
                    .get_mut(&node_id)
                    .ok_or_else(|| Error::io("failed node missing"))?;
                node.view.status = GoalNodeStatus::Failed;
                node.view.error = Some(error.clone());
                node.view
                    .members
                    .get_mut(&member_id)
                    .ok_or_else(|| Error::io("failed member missing"))?
                    .status = GoalMemberStatus::Failed;
            }
            let code = if matches!(
                next.runs[run_id].view.role,
                GoalRunRole::PlanCheck { .. } | GoalRunRole::FinalCheck { .. }
            ) {
                "check_unavailable"
            } else {
                &error.code
            };
            set_terminal(next, GoalOutcome::Failed, code, run_id, revision, now);
            next.error = Some(error);
        }
        Ok(())
    }
    fn retry_planner(&self, next: &mut GoalRecord, revision: u64, now: u64) -> Result<()> {
        for request in next
            .requests
            .values_mut()
            .filter(|r| matches!(r.role, GoalRunRole::PlanCheck { .. }))
        {
            request.cancelled = true;
            if let Some(id) = &request.run_id
                && let Some(run) = next.runs.get_mut(id)
                && !run.view.started
                && run.view.phase != GoalRunPhase::Settled
            {
                run.view.native_stop_confirmed = true;
                run.view.phase = GoalRunPhase::Settled;
                run.business_disposed = true;
                run.settlement_sequence += 1;
                next.budget.observe_active(
                    id,
                    run.settlement_sequence,
                    run.active_ms,
                    true,
                    true,
                    &next.prepared.effective_limits,
                )?;
            }
        }
        next.stage = GoalStage::Planning;
        if next.planning_attempts > next.prepared.request.spec.planning.max_revisions {
            set_terminal(
                next,
                GoalOutcome::Failed,
                "planning_exhausted",
                "planner",
                revision,
                now,
            );
            next.error = Some(Error::new(
                "planning_exhausted",
                "/planning",
                "bounded planner attempts exhausted",
            ));
        } else if next.permissions(now, true).advance_allowed() {
            self.prepare_planner_request(next)?;
        }
        Ok(())
    }
    fn accept_planner(
        &self,
        row: &GoalRow,
        next: &mut GoalRecord,
        outputs: &BTreeMap<String, Value>,
        attempt: u64,
        effects: &mut GoalCommitEffects,
        now: u64,
    ) -> Result<()> {
        let value = outputs
            .get("proposal")
            .ok_or_else(|| Error::io("planner proposal slot missing"))?;
        let candidate_sha = crate::json::digest(&bounded_json(value, self.options.max_plan_bytes)?);
        let result: PlannerResult = serde_json::from_value(value.clone())?;
        match result {
            PlannerResult::NeedsInput {
                input_schema_ref,
                reason,
                ..
            } => {
                let profile = &self.registry.profiles[&next.prepared.request.spec.profile_ref];
                if !profile.planning_input_schemas.contains(&input_schema_ref) {
                    return Err(Error::new(
                        "plan_invalid",
                        "/input_schema_ref",
                        "planner selected an unregistered planning input schema",
                    ));
                }
                let wait_id = format!("wait-{}", uuid::Uuid::new_v4());
                next.waiting.insert(
                    wait_id.clone(),
                    GoalWaiting {
                        wait_id,
                        nonce: format!("nonce-{}", uuid::Uuid::new_v4()),
                        node_id: None,
                        payload_schema_digest: self.registry.schemas.sources[&input_schema_ref]
                            .digest
                            .clone(),
                        schema_ref: input_schema_ref,
                        expires_at_utc_ms: next.prepared.planning_deadline_utc_ms,
                        consumed: false,
                        reason,
                    },
                );
                if next.pause_at_utc_ms.is_none() {
                    next.status = GoalStatus::Waiting;
                }
            }
            PlannerResult::Proposal { proposal, .. } => {
                let imports = next
                    .prepared
                    .request
                    .spec
                    .imports
                    .iter()
                    .map(|i| {
                        let schema = self
                            .registry
                            .schemas
                            .sources
                            .iter()
                            .find(|(_, s)| s.digest == i.schema_digest)
                            .map(|(id, _)| id.clone())
                            .ok_or_else(|| Error::io("admitted import schema missing"))?;
                        Ok((i.artifact_id.clone(), schema))
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?;
                let context = PlanValidationContext {
                    spec_digest: next.prepared.spec_digest.clone(),
                    profile_ref: next.prepared.request.spec.profile_ref.clone(),
                    planning: next.prepared.request.spec.planning.clone(),
                    limits: next.prepared.effective_limits.clone(),
                    allowed_capabilities: next.prepared.allowed_capabilities.clone(),
                    allowed_tools: next.prepared.allowed_tools.clone(),
                    imports,
                    runs_used: next.budget.runs_used,
                };
                match self.registry.validate_plan(
                    &bounded_json(&proposal, self.options.max_plan_bytes)?,
                    &context,
                    &self.options,
                ) {
                    Err(failure) => {
                        next.proposals.push(ProposalRecord {
                            attempt,
                            candidate_sha,
                            validated_proposal_sha: None,
                            proposal_digest: None,
                            failure: Some(failure),
                            check_ids: vec![],
                        });
                        self.retry_planner(next, row.revision + 1, now)?;
                    }
                    Ok(valid) => {
                        let raw = bounded_json(valid.proposal(), self.options.max_plan_bytes)?;
                        // Proposal bodies remain content-addressed; the locked plan is their only public version.
                        let proposal_sha = self.inner.store.write_blob(&raw)?;
                        next.proposals.push(ProposalRecord {
                            attempt,
                            candidate_sha,
                            validated_proposal_sha: Some(proposal_sha),
                            proposal_digest: Some(valid.digest().into()),
                            failure: None,
                            check_ids: vec![],
                        });
                        next.stage = GoalStage::PlanChecking;
                        self.advance_plan(row, next, effects, now)?;
                    }
                }
            }
        }
        Ok(())
    }
    pub(crate) fn advance_plan(
        &self,
        row: &GoalRow,
        next: &mut GoalRecord,
        effects: &mut GoalCommitEffects,
        now: u64,
    ) -> Result<()> {
        if !next.permissions(now, true).advance_allowed() {
            return Ok(());
        }
        let profile = &self.registry.profiles[&next.prepared.request.spec.profile_ref];
        let latest = next
            .proposals
            .last()
            .ok_or_else(|| Error::io("validated proposal missing"))?
            .clone();
        let proposal: PlanProposal = serde_json::from_slice(
            &self.inner.store.read_blob(
                latest
                    .validated_proposal_sha
                    .as_ref()
                    .ok_or_else(|| Error::io("validated proposal bytes missing"))?,
            )?,
        )?;
        let requirements = profile
            .checks
            .iter()
            .filter(|c| c.phase == CheckPhase::Plan)
            .cloned()
            .collect::<Vec<_>>();
        if latest.check_ids.is_empty() && !requirements.is_empty() {
            for requirement in requirements {
                let issued = self.registry.issue_check(
                    &row.id,
                    &next.prepared.request.spec.goal,
                    &next.prepared.spec_digest,
                    &next.prepared.request.spec.inputs,
                    &requirement.id,
                    CheckInputPhase::Plan,
                    &requirement.checker_ref,
                    next.epoch,
                    row.revision + 1,
                    CheckSubject::Proposal {
                        proposal_digest: latest.proposal_digest.clone().expect("validated digest"),
                        proposal: proposal.clone(),
                    },
                    BTreeSet::new(),
                )?;
                let evaluation_id = self.enqueue_check(
                    next,
                    issued.clone(),
                    GoalRunRole::PlanCheck {
                        check_id: requirement.id,
                        proposal_digest: latest.proposal_digest.clone().expect("validated digest"),
                    },
                    vec![],
                )?;
                next.proposals
                    .last_mut()
                    .expect("proposal")
                    .check_ids
                    .push(evaluation_id);
                effects.checks.push((issued, None));
            }
            return Ok(());
        }
        if latest
            .check_ids
            .iter()
            .any(|id| next.checks[id].view.verdict != Some(EvaluationVerdict::Pass))
        {
            return Ok(());
        }
        let locked = LockedPlan {
            protocol_version: 1,
            goal_id: row.id.clone(),
            plan_digest: latest.proposal_digest.expect("validated digest"),
            proposal,
            accepted_checks: latest.check_ids,
            locked_at_utc_ms: now,
        };
        for (id, node) in &locked.proposal.nodes {
            let contract = &self.registry.capabilities[&node.capability_ref];
            next.nodes.insert(
                id.clone(),
                GoalNodeRecord {
                    view: GoalNodeSnapshot {
                        capability_ref: node.capability_ref.clone(),
                        status: GoalNodeStatus::Pending,
                        input_digest: None,
                        members: contract
                            .members
                            .keys()
                            .map(|id| {
                                (
                                    id.clone(),
                                    GoalMemberSnapshot {
                                        status: GoalMemberStatus::Pending,
                                        run_id: None,
                                        outputs: BTreeMap::new(),
                                        error: None,
                                    },
                                )
                            })
                            .collect(),
                        outputs: BTreeMap::new(),
                        error: None,
                    },
                    input_sha: None,
                    input_artifact_ids: vec![],
                    check_request_id: None,
                    checking_started_at_ms: None,
                    checking_nonce: None,
                    checking_epoch: None,
                },
            );
        }
        next.plan = Some(locked.clone());
        next.stage = GoalStage::Executing;
        effects.plan = Some(locked);
        Ok(())
    }
    pub(crate) fn enqueue_check(
        &self,
        next: &mut GoalRecord,
        issued: IssuedCheck,
        role: GoalRunRole,
        artifacts: Vec<String>,
    ) -> Result<String> {
        let bytes = bounded_json(&issued.input, self.options.max_plan_bytes)?;
        let sha = self.inner.store.write_blob(&bytes)?;
        let evaluation_id = issued.input.nonce.clone();
        next.checks.insert(
            evaluation_id.clone(),
            GoalCheckRecord {
                view: GoalEvaluationSummary {
                    evaluation_id: Some(evaluation_id.clone()),
                    check_id: issued.input.check_id.clone(),
                    phase: issued.input.phase,
                    status: "issued".into(),
                    verdict: None,
                    receipt_id: None,
                    error: None,
                },
                input_sha: sha.clone(),
                input_digest: issued.input_digest.clone(),
                producer_run_id: None,
                candidate_sha: None,
                receipt_sha: None,
            },
        );
        next.unissued_checks.remove(&issued.input.check_id);
        let request_id = format!("request-{}", uuid::Uuid::new_v4());
        next.requests.insert(
            request_id.clone(),
            GoalRunRequest {
                request_id,
                role,
                entry_ref: self.registry.checkers[&issued.checker_ref]
                    .entry_ref
                    .clone(),
                epoch: next.epoch,
                input_sha: sha.clone(),
                input_digest: sha,
                input_artifact_ids: artifacts,
                check_request_id: Some(evaluation_id.clone()),
                run_id: None,
                cancelled: false,
            },
        );
        Ok(evaluation_id)
    }
    fn accept_check(
        &self,
        row: &GoalRow,
        next: &mut GoalRecord,
        run_id: &str,
        outputs: &BTreeMap<String, Value>,
        effects: &mut GoalCommitEffects,
        now: u64,
    ) -> Result<()> {
        let run = &next.runs[run_id];
        let request = &next.requests[&run.request_id];
        let evaluation_id = request
            .check_request_id
            .as_ref()
            .ok_or_else(|| Error::io("check request missing"))?
            .clone();
        let check = &next.checks[&evaluation_id];
        let input: CheckInput =
            serde_json::from_slice(&self.inner.store.read_blob(&check.input_sha)?)?;
        let checker_ref = match &run.view.role {
            GoalRunRole::PlanCheck { check_id, .. } | GoalRunRole::FinalCheck { check_id } => {
                self.registry.profiles[&next.prepared.request.spec.profile_ref]
                    .checks
                    .iter()
                    .find(|c| c.id == *check_id)
                    .ok_or_else(|| Error::io("check requirement missing"))?
                    .checker_ref
                    .clone()
            }
            _ => return Err(Error::io("phase check has a node role")),
        };
        let issued = IssuedCheck {
            input,
            input_digest: check.input_digest.clone(),
            checker_ref: checker_ref.clone(),
            producer_run_id: Some(run_id.into()),
        };
        let slot = &self.registry.checkers[&checker_ref].result_slot;
        let value = outputs.get(slot).ok_or_else(|| {
            Error::new("check_unavailable", "/evaluation", "checker result missing")
        })?;
        let existing = self.existing_check_evidence(next, &issued)?;
        let checked = self.registry.check_evaluation(
            &issued,
            &bounded_json(value, self.options.max_json_candidate_bytes)?,
            self.options.max_json_candidate_bytes,
            &existing,
        )?;
        let receipt = issued.receipt(checked, row.revision + 1);
        let sha = self
            .inner
            .store
            .write_blob(&bounded_json(&receipt, 4_194_304)?)?;
        let check = next.checks.get_mut(&evaluation_id).expect("issued check");
        check.candidate_sha = Some(receipt.candidate_digest.clone());
        check.receipt_sha = Some(sha);
        check.view.status = "accepted".into();
        check.view.verdict = Some(receipt.verdict.clone());
        check.view.receipt_id = Some(evaluation_id);
        effects.checks.push((issued, Some(receipt.clone())));
        match receipt.verdict {
            EvaluationVerdict::Error => {
                set_terminal(
                    next,
                    GoalOutcome::Failed,
                    "check_unavailable",
                    run_id,
                    row.revision + 1,
                    now,
                );
                next.error = Some(Error::new(
                    "check_unavailable",
                    "/evaluation",
                    "checker reported error",
                ));
            }
            EvaluationVerdict::Fail if receipt.phase == CheckInputPhase::Plan => {
                self.retry_planner(next, row.revision + 1, now)?
            }
            EvaluationVerdict::Fail => set_terminal(
                next,
                GoalOutcome::NotMet,
                "quality_not_met",
                run_id,
                row.revision + 1,
                now,
            ),
            EvaluationVerdict::Pass if receipt.phase == CheckInputPhase::Plan => {
                self.advance_plan(row, next, effects, now)?
            }
            EvaluationVerdict::Pass => {}
        }
        Ok(())
    }
    pub(crate) fn existing_check_evidence(
        &self,
        next: &GoalRecord,
        issued: &IssuedCheck,
    ) -> Result<BTreeSet<String>> {
        let mut evidence = BTreeSet::new();
        for field in issued.input.goal_inputs.keys() {
            evidence.insert(format!("goal-input:{field}"));
        }
        if let CheckSubject::Artifacts { targets, .. } = &issued.input.subject {
            for target in targets.values() {
                let artifact = if let Some(artifact) = next.artifacts.get(&target.artifact_id) {
                    artifact.clone()
                } else {
                    if !next.prepared.request.spec.imports.iter().any(|import| {
                        import.artifact_id == target.artifact_id
                            && import.sha256 == target.sha256
                            && import.schema_digest == target.schema_digest
                    }) {
                        return Err(Error::new(
                            "check_unavailable",
                            "/evidence",
                            "check target is neither a public output nor an admitted import",
                        ));
                    }
                    let owner = self.inner.store.goal_load(&issued.input.goal_id)?.owner;
                    self.inner
                        .store
                        .goal_artifact(&target.artifact_id, &owner)?
                };
                if !artifact.accepted
                    || artifact.artifact.sha256 != target.sha256
                    || artifact.schema_digest.as_ref() != Some(&target.schema_digest)
                {
                    return Err(Error::new(
                        "check_unavailable",
                        "/evidence",
                        "check target identity changed",
                    ));
                }
                self.inner.store.read_blob(&target.sha256)?;
                evidence.insert(target.artifact_id.clone());
            }
        }
        Ok(evidence)
    }
    fn dispose_stale(&self, row: &GoalRow, run_id: &str, elapsed: u64, now: u64) -> Result<()> {
        let original = &row.record.runs[run_id];
        let mut next = row.record.clone();
        let superseded = matches!(&original.view.role, GoalRunRole::PlanCheck { proposal_digest, .. }
            if next.stage == GoalStage::Planning || next.proposals.last().and_then(|p| p.proposal_digest.as_ref()) != Some(proposal_digest));
        next.runs
            .get_mut(run_id)
            .expect("original Run")
            .view
            .interrupted = !superseded;
        if next.terminal_intent.is_none() && !superseded {
            next.blockers.insert(GoalBlocker {
                reason: "interrupted".into(),
                subject: run_id.into(),
            });
            next.status = GoalStatus::Blocked;
        }
        settle_run(&mut next, run_id, elapsed)?;
        self.cleanup_terminal(&mut next)?;
        self.close_stopping(&mut next, now)?;
        self.commit_goal(
            row,
            &next,
            GoalWriteGuard::Settlement {
                run_id,
                epoch: original.view.epoch,
                binding: &original.executor_binding,
            },
            None,
            &Default::default(),
            &self.clock,
        )?;
        Ok(())
    }
}
pub(crate) fn settle_run(record: &mut GoalRecord, run_id: &str, check_ms: u64) -> Result<()> {
    let run = record
        .runs
        .get_mut(run_id)
        .ok_or_else(|| Error::io("original Run missing"))?;
    if !run.view.native_stop_confirmed {
        return Err(Error::io("cannot settle live native invocation"));
    }
    run.active_ms = run.active_ms.saturating_add(check_ms);
    run.settlement_sequence += 1;
    run.business_disposed = true;
    run.view.phase = GoalRunPhase::Settled;
    if let Some(budget) = &mut run.view.budget {
        budget.check_elapsed_ms = Some(check_ms);
    }
    record.budget.observe_active(
        run_id,
        run.settlement_sequence,
        run.active_ms,
        true,
        true,
        &record.prepared.effective_limits,
    )?;
    Ok(())
}
fn plan_failure(error: Error) -> PlanValidationFailure {
    PlanValidationFailure {
        code: "plan_invalid".into(),
        issues: vec![PlanIssue {
            category: PlanIssueCategory::Format,
            path: error.path,
            message: error.message.chars().take(4000).collect(),
            expected: None,
            actual: None,
        }],
        truncated: false,
    }
}
