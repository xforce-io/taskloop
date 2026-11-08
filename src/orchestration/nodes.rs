//! Frozen DAG inputs and fixed projections. No application-specific transition logic lives here.
use super::{
    check::IssuedCheck,
    core::{GoalCore, set_terminal},
    goal_budget::BudgetAdmission,
    plan::bounded_json,
    record::*,
    *,
};
use crate::{
    store::goals::{GoalCommit, GoalRow, GoalWriteGuard},
    *,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use tokio::time::Instant;

impl GoalCore {
    /// G05/G06/G07/G08: one durable coordination transaction, independent of native slot capacity.
    pub(crate) fn advance(&self, id: &str) -> Result<bool> {
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        let row = self.inner.store.goal_load(id)?;
        let observed = self.inner.store.observe_goal_clock(&self.clock)?;
        if !row
            .record
            .permissions(observed.utc_ms, observed.trusted)
            .advance_allowed()
        {
            return Ok(false);
        }
        let mut next = row.record.clone();
        let mut effects = GoalCommitEffects::default();
        match next.stage {
            GoalStage::Planning => {
                if !next.waiting.values().any(|w| !w.consumed) {
                    self.prepare_planner_request(&mut next)?;
                }
            }
            GoalStage::PlanChecking => {
                self.advance_plan(&row, &mut next, &mut effects, observed.utc_ms)?
            }
            GoalStage::Executing => {
                let plan = next
                    .plan
                    .as_ref()
                    .ok_or_else(|| Error::io("executing Goal has no locked plan"))?
                    .proposal
                    .clone();
                for (node_id, node) in &plan.nodes {
                    if next.nodes[node_id].view.status == GoalNodeStatus::Pending
                        && node
                            .depends_on
                            .iter()
                            .all(|dep| next.nodes[dep].view.status == GoalNodeStatus::Succeeded)
                    {
                        if let Err(error) = self.ready_node(
                            &row,
                            &mut next,
                            node_id,
                            node,
                            &mut effects,
                            observed.utc_ms,
                        ) {
                            let state = next.nodes.get_mut(node_id).expect("locked node");
                            state.view.status = GoalNodeStatus::Failed;
                            state.view.error = Some(error.clone());
                            set_terminal(
                                &mut next,
                                GoalOutcome::Failed,
                                &error.code,
                                node_id,
                                row.revision + 1,
                                observed.utc_ms,
                            );
                            next.error = Some(error);
                            break;
                        }
                    }
                    if next.nodes[node_id].view.status == GoalNodeStatus::Running
                        && next.nodes[node_id]
                            .view
                            .members
                            .values()
                            .all(|m| m.status == GoalMemberStatus::Succeeded)
                    {
                        self.begin_node_check(&mut next, node_id, observed.utc_ms)?;
                    }
                }
                if next.terminal_intent.is_none()
                    && next
                        .nodes
                        .values()
                        .all(|n| n.view.status == GoalNodeStatus::Succeeded)
                    && next
                        .runs
                        .values()
                        .all(|r| r.view.phase == GoalRunPhase::Settled)
                {
                    self.issue_final_checks(&row, &mut next, &mut effects)?;
                } else if next.terminal_intent.is_none() {
                    let runnable = next.nodes.values().any(|n| {
                        matches!(
                            n.view.status,
                            GoalNodeStatus::Ready
                                | GoalNodeStatus::Running
                                | GoalNodeStatus::Checking
                        )
                    });
                    let waiting = next.waiting.values().any(|w| !w.consumed);
                    next.status = if !runnable && waiting {
                        GoalStatus::Waiting
                    } else {
                        GoalStatus::Running
                    };
                }
            }
            GoalStage::GoalChecking => {
                let valid = self.required_proofs_pass(&next)?;
                if next
                    .permissions(observed.utc_ms, observed.trusted)
                    .finish_allowed(&control::FinishFacts {
                        required_proofs_match_and_pass: valid,
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
                    })
                {
                    next.status = GoalStatus::Finished;
                    next.outcome = Some(GoalOutcome::Achieved);
                    effects.finish_proofs_valid = true;
                }
            }
        }
        self.cleanup_terminal(&mut next)?;
        if serde_json::to_value(&row.record)? == serde_json::to_value(&next)?
            && serde_json::to_value(&row.record.budget)? == serde_json::to_value(&next.budget)?
        {
            return Ok(false);
        }
        let guard = if effects.finish_proofs_valid {
            GoalWriteGuard::Finish { proofs_valid: true }
        } else {
            GoalWriteGuard::Advance
        };
        self.inner
            .store
            .goal_commit(&row, &next, guard, None, &effects, &self.clock)?;
        self.cancel_goal_handles(&next);
        Ok(true)
    }
    fn ready_node(
        &self,
        row: &GoalRow,
        next: &mut GoalRecord,
        node_id: &str,
        node: &PlanNode,
        effects: &mut GoalCommitEffects,
        now: u64,
    ) -> Result<()> {
        let contract = &self.registry.capabilities[&node.capability_ref];
        let spec = serde_json::to_value(&next.prepared.request.spec)?;
        let mut ports = Map::new();
        let mut source_artifacts = BTreeMap::new();
        for (port, input) in &contract.inputs {
            let Some(source) = node.inputs.get(port) else {
                if let Some(default) = &input.default {
                    ports.insert(port.clone(), default.clone());
                } else if input.required {
                    return Err(Error::new(
                        "input_invalid",
                        format!("/nodes/{node_id}/inputs/{port}"),
                        "required input missing",
                    ));
                }
                continue;
            };
            let value = match source {
                PlanInput::Literal { value } => value.clone(),
                PlanInput::Goal { pointer } => select(&spec, pointer)?,
                PlanInput::Node {
                    node_id: producer,
                    port: output,
                    pointer,
                } => {
                    let reference = next.nodes[producer]
                        .view
                        .outputs
                        .get(output)
                        .ok_or_else(|| Error::io("accepted dependency output missing"))?;
                    let artifact = next
                        .artifacts
                        .get(&reference.id)
                        .ok_or_else(|| Error::io("public artifact metadata missing"))?
                        .clone();
                    let raw = crate::parse_unique(&self.inner.store.read_blob(&reference.sha256)?)?;
                    source_artifacts.insert(port.clone(), artifact);
                    select(&raw, pointer)?
                }
                PlanInput::Import {
                    artifact_id,
                    pointer,
                } => {
                    let artifact = self.inner.store.goal_artifact(artifact_id, &row.owner)?;
                    let raw = crate::parse_unique(
                        &self.inner.store.read_blob(&artifact.artifact.sha256)?,
                    )?;
                    source_artifacts.insert(port.clone(), artifact);
                    select(&raw, pointer)?
                }
            };
            self.registry.schemas.validate(&input.schema_ref, &value)?;
            ports.insert(port.clone(), value);
        }
        let sha = self
            .inner
            .store
            .write_blob(&bounded_json(&ports, self.options.max_plan_bytes)?)?;
        let artifacts = source_artifacts
            .values()
            .map(|a| a.artifact.id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        {
            let state = next.nodes.get_mut(node_id).expect("locked node");
            state.input_sha = Some(sha.clone());
            state.input_artifact_ids = artifacts.clone();
            state.view.input_digest = Some(sha);
            state.view.status = GoalNodeStatus::Ready;
        }
        if contract.kind == NodeKind::Input {
            let schema_ref = contract
                .signal_schema_ref
                .as_ref()
                .ok_or_else(|| Error::io("input schema missing"))?;
            let wait_id = format!("wait-{}", uuid::Uuid::new_v4());
            next.waiting.insert(
                wait_id.clone(),
                GoalWaiting {
                    wait_id,
                    nonce: format!("nonce-{}", uuid::Uuid::new_v4()),
                    node_id: Some(node_id.into()),
                    schema_ref: schema_ref.clone(),
                    payload_schema_digest: self.registry.schemas.sources[schema_ref].digest.clone(),
                    expires_at_utc_ms: now
                        .saturating_add(contract.max_wait_ms.unwrap_or(0))
                        .min(next.prepared.wall_deadline_utc_ms),
                    consumed: false,
                    reason: "The locked plan requires additional input".into(),
                },
            );
            next.nodes.get_mut(node_id).expect("input node").view.status =
                GoalNodeStatus::WaitingInput;
            return Ok(());
        }
        if let Some(evaluation) = &contract.evaluation {
            let (check_id, phase) = if evaluation.purpose == EvaluationPurpose::Checkpoint {
                let plan = &next.plan.as_ref().expect("locked plan").proposal;
                let (id, _) = plan
                    .checks
                    .iter()
                    .find(|(_, c)| c.evaluation_node_id.as_deref() == Some(node_id))
                    .ok_or_else(|| Error::io("checkpoint mapping missing"))?;
                (id.clone(), CheckInputPhase::NodeCheckpoint)
            } else {
                (
                    format!("eval.{}", &crate::json::digest(node_id.as_bytes())[..32]),
                    CheckInputPhase::NodeEvaluation,
                )
            };
            let mut targets = BTreeMap::new();
            for (alias, port) in &evaluation.target_ports {
                let artifact = source_artifacts
                    .get(port)
                    .ok_or_else(|| Error::io("evaluation target artifact missing"))?;
                targets.insert(alias.clone(), self.check_target(artifact)?);
            }
            let issued = self.issue_artifact_check(
                row,
                next,
                &check_id,
                phase,
                &evaluation.checker_ref,
                targets,
            )?;
            let (member_id, _) = contract
                .members
                .first_key_value()
                .ok_or_else(|| Error::io("evaluation member missing"))?;
            let evaluation_id = self.enqueue_check(
                next,
                issued.clone(),
                GoalRunRole::NodeMember {
                    node_id: node_id.into(),
                    member_id: member_id.clone(),
                },
                artifacts,
            )?;
            next.nodes
                .get_mut(node_id)
                .expect("evaluation node")
                .check_request_id = Some(evaluation_id);
            effects.checks.push((issued, None));
        } else {
            for (member_id, member) in &contract.members {
                let NodeMember::NodePorts {
                    entry_ref,
                    input_bindings,
                } = member
                else {
                    return Err(Error::io("ordinary member requires node_ports"));
                };
                let mut input = Map::new();
                for (field, binding) in input_bindings {
                    if let Some(port) = ports.get(&binding.port) {
                        input.insert(field.clone(), select(port, &binding.pointer)?);
                    }
                }
                self.registry.schemas.validate(
                    &self.registry.entries[entry_ref].contract.input_schema,
                    &Value::Object(input.clone()),
                )?;
                let sha = self
                    .inner
                    .store
                    .write_blob(&bounded_json(&input, self.options.max_plan_bytes)?)?;
                let request_id = format!("request-{}", uuid::Uuid::new_v4());
                next.requests.insert(
                    request_id.clone(),
                    GoalRunRequest {
                        request_id,
                        role: GoalRunRole::NodeMember {
                            node_id: node_id.into(),
                            member_id: member_id.clone(),
                        },
                        entry_ref: entry_ref.clone(),
                        epoch: next.epoch,
                        input_sha: sha.clone(),
                        input_digest: sha,
                        input_artifact_ids: artifacts.clone(),
                        check_request_id: None,
                        run_id: None,
                        cancelled: false,
                    },
                );
            }
        }
        Ok(())
    }
    pub(crate) fn begin_node_check(
        &self,
        next: &mut GoalRecord,
        node_id: &str,
        now: u64,
    ) -> Result<bool> {
        let node = &next.nodes[node_id];
        if node.view.status == GoalNodeStatus::Checking {
            return Ok(false);
        }
        if !node
            .view
            .members
            .values()
            .all(|m| m.status == GoalMemberStatus::Succeeded)
        {
            return Ok(false);
        }
        let contract = &self.registry.capabilities[&node.view.capability_ref];
        if contract.evaluation.is_none() && !next.permissions(now, true).advance_allowed() {
            return Ok(false);
        }
        let allocation = contract.max_rule_check_ms.min(
            next.budget
                .permanent_available(&next.prepared.effective_limits)
                .active_ms,
        );
        if allocation == 0 {
            return Err(Error::new(
                "budget_exhausted",
                "/check",
                "no time remains for fixed node projection",
            ));
        }
        let nonce = format!("node-check-{}", uuid::Uuid::new_v4());
        match next.budget.reserve(
            &nonce,
            GoalBudgetAmount {
                active_ms: allocation,
                tool_calls: 0,
                candidate_bytes: 0,
            },
            None,
            false,
            &next.prepared.effective_limits,
        )? {
            BudgetAdmission::Pending => return Ok(false),
            BudgetAdmission::Exhausted => {
                return Err(Error::new(
                    "budget_exhausted",
                    "/check",
                    "node check cannot be reserved",
                ));
            }
            BudgetAdmission::Granted => {}
        }
        let state = next.nodes.get_mut(node_id).expect("locked node");
        state.view.status = GoalNodeStatus::Checking;
        state.checking_started_at_ms = Some(now);
        state.checking_nonce = Some(nonce);
        state.checking_epoch = Some(next.epoch);
        Ok(true)
    }
    /// N05: fixed registered projection followed by the one Schema/evaluation acceptance path.
    pub(crate) fn finish_node_check(&self, id: &str, node_id: &str) -> Result<bool> {
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        let row = self.inner.store.goal_load(id)?;
        let node = row
            .record
            .nodes
            .get(node_id)
            .ok_or_else(|| Error::io("node missing"))?;
        if node.view.status != GoalNodeStatus::Checking {
            return Ok(false);
        }
        let nonce = node
            .checking_nonce
            .as_deref()
            .ok_or_else(|| Error::io("node check nonce missing"))?;
        let started = Instant::now();
        let observed = self.inner.store.observe_goal_clock(&self.clock)?;
        let mut next = row.record.clone();
        let mut effects = GoalCommitEffects::default();
        let contract = &self.registry.capabilities[&node.view.capability_ref];
        let allocation = row.record.budget.reserved[nonce].allocation.active_ms;
        let projected = (|| -> Result<BTreeMap<String, Value>> {
            let join = contract
                .join
                .as_ref()
                .ok_or_else(|| Error::io("work node join missing"))?;
            let mut projected = BTreeMap::new();
            for (port, projection) in &join.outputs {
                let value = match projection {
                    OutputProjection::Member { member_id, slot } => {
                        self.member_value(node, member_id, slot)?
                    }
                    OutputProjection::Members { slots } => Value::Object(
                        slots
                            .iter()
                            .map(|(member, slot)| {
                                Ok((member.clone(), self.member_value(node, member, slot)?))
                            })
                            .collect::<Result<Map<_, _>>>()?,
                    ),
                };
                self.registry
                    .schemas
                    .validate(&contract.outputs[port].schema_ref, &value)?;
                projected.insert(port.clone(), value);
            }
            Ok(projected)
        })();
        let applied = projected.and_then(|outputs| {
            let mut evaluation_error = false;
            if let Some(evaluation) = &contract.evaluation {
                let evaluation_id = node
                    .check_request_id
                    .as_ref()
                    .ok_or_else(|| Error::io("node check request missing"))?;
                let check = &next.checks[evaluation_id];
                let issued = IssuedCheck {
                    input: serde_json::from_slice(&self.inner.store.read_blob(&check.input_sha)?)?,
                    input_digest: check.input_digest.clone(),
                    checker_ref: evaluation.checker_ref.clone(),
                    producer_run_id: check.producer_run_id.clone(),
                };
                let checked = self.registry.check_evaluation(
                    &issued,
                    &bounded_json(
                        &outputs["evaluation"],
                        self.options.max_json_candidate_bytes,
                    )?,
                    self.options.max_json_candidate_bytes,
                    &self.existing_check_evidence(&next, &issued)?,
                )?;
                let receipt = issued.receipt(checked, row.revision + 1);
                let sha = self
                    .inner
                    .store
                    .write_blob(&bounded_json(&receipt, 4_194_304)?)?;
                let check = next.checks.get_mut(evaluation_id).expect("issued check");
                check.receipt_sha = Some(sha);
                check.candidate_sha = Some(receipt.candidate_digest.clone());
                check.view.status = "accepted".into();
                check.view.verdict = Some(receipt.verdict.clone());
                check.view.receipt_id = Some(evaluation_id.clone());
                effects.checks.push((issued, Some(receipt.clone())));
                if receipt.verdict == EvaluationVerdict::Error {
                    evaluation_error = true;
                    set_terminal(
                        &mut next,
                        GoalOutcome::Failed,
                        "check_unavailable",
                        node_id,
                        row.revision + 1,
                        observed.utc_ms,
                    );
                } else if receipt.verdict == EvaluationVerdict::Fail
                    && evaluation.purpose == EvaluationPurpose::Checkpoint
                {
                    set_terminal(
                        &mut next,
                        GoalOutcome::NotMet,
                        "quality_not_met",
                        node_id,
                        row.revision + 1,
                        observed.utc_ms,
                    );
                }
            }
            if evaluation_error {
                next.nodes
                    .get_mut(node_id)
                    .expect("checking node")
                    .view
                    .status = GoalNodeStatus::Failed;
            } else {
                for (port, value) in outputs {
                    let artifact = self.public_artifact(
                        &row.id,
                        &next,
                        node_id,
                        &port,
                        &contract.outputs[&port].schema_ref,
                        &value,
                        "node_projection",
                    )?;
                    next.nodes
                        .get_mut(node_id)
                        .expect("checking node")
                        .view
                        .outputs
                        .insert(port, artifact.artifact.clone());
                    next.artifacts
                        .insert(artifact.artifact.id.clone(), artifact.clone());
                    effects.artifacts.push(artifact);
                }
                next.nodes
                    .get_mut(node_id)
                    .expect("checking node")
                    .view
                    .status = GoalNodeStatus::Succeeded;
            }
            Ok(())
        });
        let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let error = if elapsed > allocation {
            Some(Error::new(
                "check_timeout",
                "/node/check",
                "fixed node check exceeded its reserved time",
            ))
        } else {
            applied.err()
        };
        if let Some(error) = error {
            next = row.record.clone();
            effects = GoalCommitEffects::default();
            let state = next.nodes.get_mut(node_id).expect("checking node");
            state.view.status = GoalNodeStatus::Failed;
            state.view.error = Some(error.clone());
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
        next.budget.observe_active(
            nonce,
            1,
            elapsed,
            true,
            true,
            &next.prepared.effective_limits,
        )?;
        self.cleanup_terminal(&mut next)?;
        self.close_stopping(&mut next, observed.utc_ms)?;
        match self.inner.store.goal_commit(
            &row,
            &next,
            GoalWriteGuard::NodeResult { node_id, nonce },
            None,
            &effects,
            &self.clock,
        ) {
            Ok(GoalCommit::Saved(_)) => {}
            Ok(GoalCommit::Replayed(_)) => return Err(Error::io("internal node check replay")),
            Err(e) if e.code == "stale_attempt" => {
                let latest = self.inner.store.goal_load(id)?;
                let mut cleanup = latest.record.clone();
                cleanup.budget.observe_active(
                    nonce,
                    1,
                    elapsed,
                    true,
                    true,
                    &cleanup.prepared.effective_limits,
                )?;
                cleanup
                    .nodes
                    .get_mut(node_id)
                    .expect("checking node")
                    .view
                    .status = if cleanup.terminal_intent.is_some() {
                    GoalNodeStatus::Cancelled
                } else {
                    GoalNodeStatus::Interrupted
                };
                if cleanup.terminal_intent.is_none() {
                    cleanup.status = GoalStatus::Blocked;
                    cleanup.blockers.insert(GoalBlocker {
                        reason: "interrupted".into(),
                        subject: node_id.into(),
                    });
                }
                self.cleanup_terminal(&mut cleanup)?;
                self.inner.store.goal_commit(
                    &latest,
                    &cleanup,
                    GoalWriteGuard::Cleanup,
                    None,
                    &Default::default(),
                    &self.clock,
                )?;
            }
            Err(e) => return Err(e),
        }
        self.cancel_goal_handles(&next);
        self.inner.notify.notify_one();
        Ok(true)
    }
    fn member_value(&self, node: &GoalNodeRecord, member: &str, slot: &str) -> Result<Value> {
        let artifact = node
            .view
            .members
            .get(member)
            .and_then(|m| m.outputs.get(slot))
            .ok_or_else(|| {
                Error::new(
                    "artifact_invalid",
                    "/join",
                    "required successful member output missing",
                )
            })?;
        crate::parse_unique(&self.inner.store.read_blob(&artifact.sha256)?)
    }
    pub(crate) fn public_artifact(
        &self,
        goal: &str,
        next: &GoalRecord,
        node: &str,
        port: &str,
        schema: &str,
        value: &Value,
        producer: &str,
    ) -> Result<GoalArtifact> {
        self.registry.schemas.validate(schema, value)?;
        let sha = self
            .inner
            .store
            .write_blob(&bounded_json(value, self.options.max_json_candidate_bytes)?)?;
        Ok(GoalArtifact {
            protocol_version: 1,
            artifact: ArtifactRef {
                id: format!("artifact-{}", uuid::Uuid::new_v4()),
                sha256: sha,
                slot: port.into(),
                state_id: format!("_tl.node.{node}"),
                visit: 1,
            },
            goal_id: goal.into(),
            node_id: Some(node.into()),
            member_id: None,
            plan_digest: next.plan.as_ref().map(|p| p.plan_digest.clone()),
            schema_ref: Some(schema.into()),
            schema_digest: Some(self.registry.schemas.sources[schema].digest.clone()),
            attachment_policy_digest: None,
            accepted: true,
            producer_run_id: None,
            producer_kind: producer.into(),
        })
    }
    fn check_target(&self, artifact: &GoalArtifact) -> Result<CheckTarget> {
        Ok(CheckTarget {
            artifact_id: artifact.artifact.id.clone(),
            sha256: artifact.artifact.sha256.clone(),
            schema_digest: artifact
                .schema_digest
                .clone()
                .ok_or_else(|| Error::io("public JSON target schema missing"))?,
            value: crate::parse_unique(&self.inner.store.read_blob(&artifact.artifact.sha256)?)?,
        })
    }
    fn issue_artifact_check(
        &self,
        row: &GoalRow,
        next: &GoalRecord,
        id: &str,
        phase: CheckInputPhase,
        checker: &str,
        targets: BTreeMap<String, CheckTarget>,
    ) -> Result<IssuedCheck> {
        let mut evidence = targets
            .values()
            .map(|t| t.artifact_id.clone())
            .collect::<BTreeSet<_>>();
        for field in &self.registry.checkers[checker].goal_input_fields {
            if next.prepared.request.spec.inputs.get(field).is_some() {
                evidence.insert(format!("goal-input:{field}"));
            }
        }
        self.registry.issue_check(
            &row.id,
            &next.prepared.request.spec.goal,
            &next.prepared.spec_digest,
            &next.prepared.request.spec.inputs,
            id,
            phase,
            checker,
            next.epoch,
            row.revision + 1,
            CheckSubject::Artifacts {
                plan_digest: next.plan.as_ref().expect("locked plan").plan_digest.clone(),
                targets,
            },
            evidence,
        )
    }
    fn issue_final_checks(
        &self,
        row: &GoalRow,
        next: &mut GoalRecord,
        effects: &mut GoalCommitEffects,
    ) -> Result<()> {
        let profile = &self.registry.profiles[&next.prepared.request.spec.profile_ref];
        let plan = next.plan.as_ref().expect("locked plan");
        let requirements = profile
            .checks
            .iter()
            .filter(|c| c.phase == CheckPhase::GoalFinal)
            .cloned()
            .collect::<Vec<_>>();
        let mut requests = Vec::new();
        for requirement in requirements {
            let mut targets = BTreeMap::new();
            for alias in &requirement.target_outputs {
                let output = &plan.proposal.outputs[alias];
                let reference = &next.nodes[&output.node_id].view.outputs[&output.port];
                targets.insert(
                    alias.clone(),
                    self.check_target(&next.artifacts[&reference.id])?,
                );
            }
            let issued = self.issue_artifact_check(
                row,
                next,
                &requirement.id,
                CheckInputPhase::GoalFinal,
                &requirement.checker_ref,
                targets,
            )?;
            let artifacts = match &issued.input.subject {
                CheckSubject::Artifacts { targets, .. } => {
                    targets.values().map(|t| t.artifact_id.clone()).collect()
                }
                _ => unreachable!(),
            };
            requests.push((issued, requirement.id, artifacts));
        }
        for (issued, check_id, artifacts) in requests {
            self.enqueue_check(
                next,
                issued.clone(),
                GoalRunRole::FinalCheck { check_id },
                artifacts,
            )?;
            effects.checks.push((issued, None));
        }
        next.stage = GoalStage::GoalChecking;
        next.status = GoalStatus::Running;
        Ok(())
    }
    fn required_proofs_pass(&self, next: &GoalRecord) -> Result<bool> {
        let Some(plan) = &next.plan else {
            return Ok(false);
        };
        let profile = &self.registry.profiles[&next.prepared.request.spec.profile_ref];
        for requirement in profile.checks.iter().filter(|r| r.required) {
            let found = next.checks.values().filter(|c| {
                c.view.check_id == requirement.id
                    && c.view.status == "accepted"
                    && c.view.verdict == Some(EvaluationVerdict::Pass)
            });
            let mut matched = false;
            for check in found {
                let input: CheckInput =
                    serde_json::from_slice(&self.inner.store.read_blob(&check.input_sha)?)?;
                if input.rules_digest != requirement.rules_digest
                    || input.phase != (&requirement.phase).into()
                {
                    continue;
                }
                match &input.subject {
                    CheckSubject::Proposal {
                        proposal_digest, ..
                    } if requirement.phase == CheckPhase::Plan => {
                        matched = proposal_digest == &plan.plan_digest
                    }
                    CheckSubject::Artifacts {
                        plan_digest,
                        targets,
                    } if plan_digest == &plan.plan_digest => {
                        matched = targets.len() == requirement.target_outputs.len()
                            && requirement.target_outputs.iter().all(|alias| {
                                let output = &plan.proposal.outputs[alias];
                                let reference =
                                    next.nodes[&output.node_id].view.outputs.get(&output.port);
                                targets.get(alias).zip(reference).is_some_and(
                                    |(target, reference)| {
                                        target.artifact_id == reference.id
                                            && target.sha256 == reference.sha256
                                    },
                                )
                            });
                    }
                    _ => {}
                }
                if matched {
                    break;
                }
            }
            if !matched {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
fn select(value: &Value, pointer: &str) -> Result<Value> {
    value.pointer(pointer).cloned().ok_or_else(|| {
        Error::new(
            "input_invalid",
            "/pointer",
            "locked selector is absent from the actual input",
        )
    })
}
