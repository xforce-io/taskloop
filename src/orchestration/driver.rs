//! Goal ownership around the shared native driver; native return never certifies business success.
use super::{core::GoalCore, plan::bounded_json, record::*, *};
use crate::{
    budget::BudgetState,
    managed::{ManagedBridge, ManagedRunOwner, RunResources, execute_managed},
    store::goals::{GoalBudgetCharge, GoalCommit, GoalWriteGuard},
    *,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

struct GoalOwner {
    core: Arc<GoalCore>,
    context: GoalRunContext,
    run_id: String,
    entry: EntryContract,
    binding: String,
}
impl GoalOwner {
    fn charge(&self, charge: GoalBudgetCharge) -> Result<()> {
        self.core.inner.store.goal_budget_admit(
            &self.context.goal_id,
            &self.run_id,
            self.context.epoch,
            &self.binding,
            charge,
            &self.core.clock,
        )
    }
}
impl ManagedRunOwner for GoalOwner {
    fn active(&self) -> Result<()> {
        let row = self.core.inner.store.goal_load(&self.context.goal_id)?;
        let observed = self.core.clock.observe()?;
        let run = row
            .record
            .runs
            .get(&self.run_id)
            .ok_or_else(|| Error::io("original Goal Run missing"))?;
        if run.view.phase != GoalRunPhase::Started
            || run.view.native_stop_confirmed
            || !row
                .record
                .permissions(observed.utc_ms, observed.trusted)
                .result_allowed(&row.record.result_authority(&self.run_id, &self.binding))
        {
            return Err(Error::new(
                "stale_attempt",
                "/run",
                "Goal execution authority has closed",
            ));
        }
        Ok(())
    }
    fn admit_tool(&self, _: &str) -> Result<()> {
        self.charge(GoalBudgetCharge::Tool)
    }
    fn admit_candidate_bytes(&self, bytes: u64) -> Result<()> {
        self.charge(GoalBudgetCharge::CandidateBytes(bytes))
    }
    fn reject_candidate_bytes(&self, bytes: u64) -> Result<()> {
        self.charge(GoalBudgetCharge::RejectedBytes(bytes))
    }
    fn persist_budget(&self, snapshot: &RunBudgetSnapshot) -> Result<()> {
        // Tool/byte charges are already durable. Only merge the cumulative local rejection count.
        if snapshot.calls_rejected != 0 {
            self.charge(GoalBudgetCharge::RejectedCalls(snapshot.calls_rejected))?;
        }
        Ok(())
    }
    fn persist_candidate(&self, slot: &str, sha: &str) -> Result<()> {
        let (node_id, member_id) = match &self.context.role {
            GoalRunRole::NodeMember { node_id, member_id } => {
                (Some(node_id.clone()), Some(member_id.clone()))
            }
            _ => (None, None),
        };
        let schema_ref = self.entry.outputs.get(slot).map(|o| o.schema.clone());
        let schema_digest = schema_ref
            .as_ref()
            .map(|s| self.core.registry.schemas.sources[s].digest.clone());
        let attachment_policy_digest = if schema_ref.is_none() {
            Some(crate::json::digest(&bounded_json(
                &self.entry.attachments,
                1_048_576,
            )?))
        } else {
            None
        };
        let artifact = GoalArtifact {
            protocol_version: 1,
            artifact: ArtifactRef {
                id: format!("artifact-{}", uuid::Uuid::new_v4()),
                sha256: sha.into(),
                slot: slot.into(),
                state_id: self.context.state_id(),
                visit: self.context.visit(),
            },
            goal_id: self.context.goal_id.clone(),
            node_id,
            member_id,
            plan_digest: self.context.plan_digest.clone(),
            schema_ref,
            schema_digest,
            attachment_policy_digest,
            accepted: false,
            producer_run_id: Some(self.run_id.clone()),
            producer_kind: match self.context.role {
                GoalRunRole::Planner { .. } => "planner_candidate",
                GoalRunRole::NodeMember { .. } => "member_candidate",
                GoalRunRole::PlanCheck { .. } | GoalRunRole::FinalCheck { .. } => "check_candidate",
            }
            .into(),
        };
        self.core.inner.store.goal_save_candidate(
            &self.context.goal_id,
            &self.run_id,
            self.context.epoch,
            &self.binding,
            &artifact,
        )
    }
    fn persist_receipt(&self, receipt: ArtifactReceipt) -> Result<String> {
        self.core.inner.store.goal_save_tool_receipt(
            &self.context.goal_id,
            &receipt,
            self.context.epoch,
            &self.binding,
        )
    }
}

pub(crate) struct GoalExecution {
    core: Arc<GoalCore>,
    goal_id: String,
    run_id: String,
    epoch: u64,
    binding: String,
    executor: Arc<dyn Executor>,
    invocation: Invocation,
    bridge: Arc<ManagedBridge>,
    budget: Arc<BudgetState>,
    cancel: CancellationToken,
    preparation_ms: u64,
    trace: Option<Arc<crate::trace::RunTrace>>,
}
impl GoalCore {
    /// R02. All resources are prepared before the single durable native-start admission.
    /// The caller may spawn only the returned execution, never reconstruct an already-started Run.
    pub(crate) fn prepare_execution(
        self: &Arc<Self>,
        id: &str,
        run_id: &str,
    ) -> Result<Option<GoalExecution>> {
        let prepared_at = Instant::now();
        let _coordinator = self.coordinator.lock().map_err(Error::io)?;
        let row = self.inner.store.goal_load(id)?;
        let observed = self.inner.store.observe_goal_clock(&self.clock)?;
        if !row
            .record
            .permissions(observed.utc_ms, observed.trusted)
            .advance_allowed()
        {
            return Ok(None);
        }
        let run = row
            .record
            .runs
            .get(run_id)
            .ok_or_else(|| Error::io("reserved Goal Run missing"))?;
        if run.view.started || run.view.phase != GoalRunPhase::Reserved {
            return Ok(None);
        }
        let request = &row.record.requests[&run.request_id];
        let registered = &self.registry.entries[&run.view.entry_ref];
        let entry = registered.contract.clone();
        let executor = self.inner.bindings.executors[&run.executor].clone();
        let stage_input: Value =
            crate::parse_unique(&self.inner.store.read_blob(&request.input_sha)?)?;
        self.registry
            .schemas
            .validate(&entry.input_schema, &stage_input)?;
        let checker = request.check_request_id.is_some();
        let inputs = if checker {
            let input: CheckInput = serde_json::from_value(stage_input.clone())?;
            serde_json::to_value(input.goal_inputs)?
        } else {
            row.record.prepared.request.spec.inputs.clone()
        };
        let signals = if checker {
            vec![]
        } else {
            row.record.signals.clone()
        };
        let accepted = request
            .input_artifact_ids
            .iter()
            .map(|artifact_id| {
                let artifact = self.inner.store.goal_artifact(artifact_id, &row.owner)?;
                if !artifact.accepted {
                    return Err(Error::new(
                        "capability_denied",
                        "/artifacts",
                        "diagnostic candidate is not an authorized input",
                    ));
                }
                Ok(ArtifactData {
                    bytes: self
                        .inner
                        .store
                        .read_blob(&artifact.artifact.sha256)?
                        .into(),
                    reference: artifact.artifact,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let output_schemas = entry
            .outputs
            .iter()
            .map(|(slot, output)| Ok((slot.clone(), self.registry.schemas.bundle(&output.schema)?)))
            .collect::<Result<_>>()?;
        let work_dir = self.inner.store.run_dir(run_id)?;
        let preparation_ms = prepared_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mut policy = registered.budget.clone();
        policy.max_run_ms = Some(run.run_allocation_ms);
        policy.max_check_ms = Some(run.check_allocation_ms);
        policy.max_tool_calls = policy.max_tool_calls.min(run.allocation.tool_calls);
        policy.submission_reserve = policy.submission_reserve.min(policy.max_tool_calls);
        policy.max_candidate_bytes = policy
            .max_candidate_bytes
            .min(run.allocation.candidate_bytes);
        let now = self.clock.observe()?;
        let deadline = if matches!(
            run.view.role,
            GoalRunRole::Planner { .. } | GoalRunRole::PlanCheck { .. }
        ) {
            row.record
                .prepared
                .wall_deadline_utc_ms
                .min(row.record.prepared.planning_deadline_utc_ms)
        } else {
            row.record.prepared.wall_deadline_utc_ms
        };
        let context = GoalRunContext {
            goal_id: id.into(),
            plan_digest: row.record.plan.as_ref().map(|p| p.plan_digest.clone()),
            role: run.view.role.clone(),
            evaluation_id: request.check_request_id.clone(),
            epoch: run.view.epoch,
        };
        let state_id = context.state_id();
        let visit = context.visit();
        let budget = Arc::new(BudgetState::new(
            run_id,
            &state_id,
            &run.view.entry_ref,
            policy,
            true,
            run.run_allocation_ms.saturating_sub(preparation_ms),
            deadline.saturating_sub(now.utc_ms),
            preparation_ms,
            run.allocation
                .tool_calls
                .min(self.inner.options.max_tool_calls_per_run),
            run.allocation
                .candidate_bytes
                .min(self.inner.options.max_candidate_bytes),
            executor.budget_limits(),
        )?);
        let cancel = self.inner.stop.child_token();
        let trace = self.inner.trace.as_ref().and_then(|store| {
            let trace = store.goal_session(run_id, &context);
            trace
                .reserve_executor(&run.executor, &executor.identity())
                .then_some(trace)
        });
        let bridge = Arc::new(ManagedBridge {
            inner: self.inner.clone(),
            execution_id: id.into(),
            run_id: run_id.into(),
            owner: Arc::new(GoalOwner {
                core: self.clone(),
                context: context.clone(),
                run_id: run_id.into(),
                entry: entry.clone(),
                binding: run.executor_binding.clone(),
            }),
            entry: entry.clone(),
            resources: RunResources::Goal(self.registry.clone()),
            context: ToolContext {
                inputs: inputs.clone(),
                state_id: state_id.clone(),
                visit,
                accepted: accepted.clone(),
                signal_facts: signals.clone(),
                cancellation: cancel.clone(),
                goal: Some(context),
            },
            candidates: Mutex::new(BTreeMap::new()),
            budget: budget.clone(),
            storage_failure: Mutex::new(None),
            trace: trace.clone(),
            json_candidate_max: Some(self.options.max_json_candidate_bytes),
        });
        let invocation = Invocation {
            stage_input,
            output_schemas,
            execution_id: id.into(),
            run_id: run_id.into(),
            state_id,
            visit,
            entry: entry.clone(),
            skills: self
                .registry
                .skills
                .iter()
                .filter(|(id, _)| entry.skills.contains(id))
                .map(|(id, s)| (id.clone(), s.clone()))
                .collect(),
            inputs,
            artifacts: accepted.into_iter().map(|a| a.reference).collect(),
            signal_facts: signals,
            work_dir,
            timeout_ms: budget.snapshot().hint.remaining_ms,
            budget: Some(budget.snapshot()),
        };
        let mut next = row.record.clone();
        let started = next.runs.get_mut(run_id).expect("reserved Run");
        started.view.started = true;
        started.view.phase = GoalRunPhase::Started;
        started.started_at_utc_ms = Some(now.utc_ms);
        started.view.budget = Some(budget.snapshot());
        if let GoalRunRole::NodeMember { node_id, member_id } = &run.view.role {
            next.nodes
                .get_mut(node_id)
                .ok_or_else(|| Error::io("node missing"))?
                .view
                .status = GoalNodeStatus::Running;
            next.nodes
                .get_mut(node_id)
                .ok_or_else(|| Error::io("node missing"))?
                .view
                .members
                .get_mut(member_id)
                .ok_or_else(|| Error::io("member missing"))?
                .status = GoalMemberStatus::Running;
        }
        // Holding the same handle lock as cancellation prevents a committed start with no stop handle.
        let mut handles = self.inner.goal_run_handles.lock().map_err(Error::io)?;
        match self.commit_goal(
            &row,
            &next,
            GoalWriteGuard::Advance,
            None,
            &Default::default(),
            &self.clock,
        )? {
            GoalCommit::Saved(_) => {}
            GoalCommit::Replayed(_) => return Err(Error::io("native start cannot replay")),
        }
        handles.insert(run_id.into(), cancel.clone());
        if let Some(trace) = &trace {
            trace.emit(
                "phase.started",
                &trace.phase_span(),
                trace.goal_parent(),
                serde_json::json!({}),
            );
        }
        self.inner
            .live_budgets
            .lock()
            .map_err(Error::io)?
            .insert(run_id.into(), budget.clone());
        Ok(Some(GoalExecution {
            core: self.clone(),
            goal_id: id.into(),
            run_id: run_id.into(),
            epoch: run.view.epoch,
            binding: run.executor_binding.clone(),
            executor,
            invocation,
            bridge,
            budget,
            cancel,
            preparation_ms,
            trace,
        }))
    }
}
impl GoalExecution {
    pub(crate) async fn execute(self) -> Result<()> {
        let returned = execute_managed(
            self.executor,
            self.invocation,
            self.bridge.clone(),
            self.budget.clone(),
            self.cancel,
            self.trace.clone(),
            self.trace.is_some(),
        )
        .await;
        // R10/R11: persist physical facts and actual consumption even after cancel, expiry, or epoch loss.
        let _coordinator = self.core.coordinator.lock().map_err(Error::io)?;
        let row = self.core.inner.store.goal_load(&self.goal_id)?;
        let mut next = row.record.clone();
        let run = next
            .runs
            .get_mut(&self.run_id)
            .ok_or_else(|| Error::io("original Run missing at return"))?;
        run.view.native_stop_confirmed = returned.native_stop_confirmed;
        run.view.stop_unknown = !returned.native_stop_confirmed;
        run.view.phase = GoalRunPhase::ExecutionReturned;
        run.returned_at_utc_ms = Some(self.core.clock.observe()?.utc_ms);
        run.execution_outcome = Some(
            match self
                .bridge
                .storage_failure
                .lock()
                .map_err(Error::io)?
                .clone()
            {
                Some(error) => ExecutionOutcome::Failed {
                    code: error.code,
                    message: error.message,
                },
                None => returned.outcome,
            },
        );
        run.view.budget = Some(self.budget.snapshot());
        run.active_ms = self
            .preparation_ms
            .saturating_add(self.budget.snapshot().elapsed_ms);
        run.candidates = self
            .core
            .inner
            .store
            .goal_run_candidates(&self.goal_id, &self.run_id)?
            .into_iter()
            .map(|(slot, a)| (slot, a.artifact.sha256))
            .collect();
        run.tool_receipt_shas = self
            .core
            .inner
            .store
            .goal_tool_receipt_shas(&self.goal_id, &self.run_id)?;
        run.settlement_sequence += 1;
        next.budget.observe_active(
            &self.run_id,
            run.settlement_sequence,
            run.active_ms,
            run.view.native_stop_confirmed,
            false,
            &next.prepared.effective_limits,
        )?;
        self.core.commit_goal(
            &row,
            &next,
            GoalWriteGuard::Settlement {
                run_id: &self.run_id,
                epoch: self.epoch,
                binding: &self.binding,
            },
            None,
            &Default::default(),
            &self.core.clock,
        )?;
        if let Some(trace) = &self.trace {
            trace.emit(
                "phase.finished",
                &trace.phase_span(),
                None,
                serde_json::json!({"native_stop_confirmed":returned.native_stop_confirmed}),
            );
        }
        self.core
            .inner
            .goal_run_handles
            .lock()
            .map_err(Error::io)?
            .remove(&self.run_id);
        self.core
            .inner
            .live_budgets
            .lock()
            .map_err(Error::io)?
            .remove(&self.run_id);
        self.core.inner.notify.notify_one();
        Ok(())
    }
}
