//! Shared tool/candidate pipeline. Owners supply business authority and persistence only.
use crate::{definition::validate_schema, json::digest, runtime::Inner, *};
use async_trait::async_trait;
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

pub(crate) trait ManagedRunOwner: Send + Sync {
    fn active(&self) -> Result<()>;
    fn persist_candidate(&self, slot: &str, sha: &str) -> Result<()>;
    fn persist_budget(&self, snapshot: &RunBudgetSnapshot) -> Result<()>;
    fn persist_receipt(&self, receipt: ArtifactReceipt) -> Result<String>;
    fn admit_tool(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    fn admit_candidate_bytes(&self, _bytes: u64) -> Result<()> {
        Ok(())
    }
    fn reject_candidate_bytes(&self, _bytes: u64) -> Result<()> {
        Ok(())
    }
    fn allow_cancelled_submission(&self) -> bool {
        false
    }
}
pub(crate) enum RunResources {
    Flow(Box<Definition>),
    Goal(Arc<crate::orchestration::FrozenRegistry>),
}
impl RunResources {
    fn validate_value(&self, schema: &str, value: &Value) -> Result<()> {
        match self {
            Self::Flow(d) => d.validate_value(schema, value),
            Self::Goal(r) => r.schemas.validate(schema, value),
        }
    }
    fn skills(&self) -> &BTreeMap<String, SkillPackage> {
        match self {
            Self::Flow(d) => &d.skills,
            Self::Goal(r) => &r.skills,
        }
    }
}
pub(crate) struct ManagedBridge {
    pub inner: Arc<Inner>,
    pub execution_id: String,
    pub run_id: String,
    pub owner: Arc<dyn ManagedRunOwner>,
    pub entry: EntryContract,
    pub resources: RunResources,
    pub context: ToolContext,
    pub candidates: Mutex<BTreeMap<String, Candidate>>,
    pub budget: Arc<crate::budget::BudgetState>,
    pub storage_failure: Mutex<Option<Error>>,
    pub trace: Option<Arc<crate::trace::RunTrace>>,
    pub json_candidate_max: Option<usize>,
}
impl ManagedBridge {
    fn write(&self, candidate: Candidate) -> Result<()> {
        if self.context.is_cancelled() && !self.owner.allow_cancelled_submission() {
            return Err(Error::new(
                "stale_attempt",
                "/run",
                "execution was cancelled",
            ));
        }
        if candidate.slot.is_empty()
            || candidate.slot.len() > 160
            || !candidate
                .slot
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(Error::new("artifact_invalid", "/slot", "invalid slot name"));
        }
        if let Some(output) = self.entry.outputs.get(&candidate.slot) {
            if self
                .json_candidate_limit()
                .is_some_and(|max| candidate.bytes.len() > max)
            {
                self.budget.mark(BudgetLimit::CandidateBytes);
                self.owner
                    .reject_candidate_bytes(candidate.bytes.len() as u64)?;
                return Err(Error::new(
                    "limit_reached",
                    "/candidate",
                    "JSON candidate exceeds the admitted limit",
                ));
            }
            self.resources
                .validate_value(&output.schema, &candidate.value()?)?;
        } else if !self
            .entry
            .attachments
            .as_ref()
            .is_some_and(|a| candidate.slot.starts_with(&a.slot_prefix))
        {
            return Err(Error::new(
                "artifact_invalid",
                "/slot",
                "output slot is not declared",
            ));
        }
        let mut all = self.candidates.lock().map_err(Error::io)?;
        let bytes = all
            .iter()
            .filter(|(k, _)| *k != &candidate.slot)
            .map(|(_, v)| v.bytes.len() as u64)
            .sum::<u64>()
            .saturating_add(candidate.bytes.len() as u64);
        if let Some(policy) = &self.entry.attachments {
            let attachments: Vec<_> = all
                .values()
                .filter(|c| !self.entry.outputs.contains_key(&c.slot) && c.slot != candidate.slot)
                .collect();
            let is_attachment = !self.entry.outputs.contains_key(&candidate.slot);
            if attachments.len() as u64 + u64::from(is_attachment) > policy.max_files
                || attachments
                    .iter()
                    .map(|c| c.bytes.len() as u64)
                    .sum::<u64>()
                    + if is_attachment {
                        candidate.bytes.len() as u64
                    } else {
                        0
                    }
                    > policy.max_bytes
            {
                return Err(Error::new(
                    "limit_reached",
                    "/attachments",
                    "attachment limit reached",
                ));
            }
        }
        self.budget
            .admit_bytes_with(candidate.bytes.len() as u64, bytes, || {
                self.owner
                    .admit_candidate_bytes(candidate.bytes.len() as u64)
            })?;
        let sha = self.inner.store.write_blob(&candidate.bytes)?;
        self.owner.persist_candidate(&candidate.slot, &sha)?;
        self.budget.guard_time()?;
        all.insert(candidate.slot.clone(), candidate);
        Ok(())
    }
    fn active(&self) -> Result<()> {
        self.budget.guard_time()?;
        self.owner.active()
    }
    fn persist_budget(&self) -> Result<()> {
        self.owner.persist_budget(&self.budget.snapshot())
    }
    fn receipt(&self, id: &str, args: &Value, payload: &Value) -> Result<String> {
        let receipt_id = format!("{}:receipt-{}", self.execution_id, uuid::Uuid::new_v4());
        let receipt = ArtifactReceipt {
            protocol_version: 1,
            id: receipt_id.clone(),
            execution_id: self.execution_id.clone(),
            run_id: self.run_id.clone(),
            tool_id: id.into(),
            occurred_at: chrono::Utc::now().to_rfc3339(),
            request_sha256: digest(&serde_json::to_vec(args)?),
            result_sha256: digest(&serde_json::to_vec(payload)?),
            payload: payload.clone(),
        };
        self.owner.persist_receipt(receipt)
    }
}
#[async_trait]
impl ToolBridge for ManagedBridge {
    fn json_candidate_limit(&self) -> Option<usize> {
        self.context.goal.as_ref().map(|_| {
            let snapshot = self.budget.snapshot();
            snapshot
                .effective_candidate_bytes
                .saturating_sub(snapshot.candidate_bytes_used)
                .min(self.json_candidate_max.unwrap_or(4_194_304) as u64)
                .min(4_194_304) as usize
        })
    }
    async fn reject_json_candidate(&self, bytes: u64, metadata: ToolCallMetadata) -> Result<Value> {
        self.active()?;
        if !self.entry.tools.iter().any(|t| t == "artifact.write") {
            return Err(Error::new(
                "capability_denied",
                "/tool",
                "artifact.write is not granted",
            ));
        }
        let admitted = self
            .budget
            .admit_tool_with("artifact.write", || self.owner.admit_tool("artifact.write"));
        self.persist_budget()?;
        admitted?;
        self.budget.mark(BudgetLimit::CandidateBytes);
        self.owner.reject_candidate_bytes(bytes)?;
        if let Some(trace) = &self.trace {
            let span = crate::trace::RunTrace::span();
            trace.emit(
                "tool.started",
                &span,
                Some(trace.phase_span()),
                json!({"tool":"artifact.write","call_id":metadata.call_id,"input_content":null}),
            );
            trace.emit("tool.finished", &span, None, json!({"tool":"artifact.write","ok":false,"code":"limit_reached","candidate_bytes":bytes}));
        }
        Err(Error::new(
            "limit_reached",
            "/candidate",
            "external JSON candidate exceeds the admitted limit",
        ))
    }
    fn goal_context(&self) -> Option<GoalRunContext> {
        self.context.goal.clone()
    }
    fn budget_hint(&self) -> Option<crate::BudgetHint> {
        self.budget.hint()
    }
    fn specs(&self) -> Vec<ToolSpec> {
        let specs:Vec<ToolSpec>=self.entry.tools.iter().filter_map(|id|match id.as_str(){
        "artifact.read"=>Some(ToolSpec{id:id.clone(),description:"Read an authorized accepted artifact or a locked Skill resource by exact id; never an arbitrary path.".into(),input_schema:json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false})}),
        "artifact.write"=>Some(ToolSpec{id:id.clone(),description:"Write a JSON candidate to a declared output slot. This does not accept the result or advance the workflow.".into(),input_schema:json!({"type":"object","properties":{"slot":{"type":"string"},"value":{}},"required":["slot","value"],"additionalProperties":false})}),
        _=>self.inner.bindings.tools.get(id).map(|t|t.spec()),
    }).collect();
        if let Some(hint) = self.budget.hint() {
            specs.into_iter().map(|mut spec|{spec.description.push_str(&format!(" Host execution budget: {} admitted calls; last {} reserved for artifact.write. Read _taskloop_budget. Stop exploration in finalize mode; submit existing evidence and honest limitations. Hard deadlines, schemas and permissions still apply.",hint.limit,hint.submission_reserve));spec}).collect()
        } else {
            specs
        }
    }
    async fn call(&self, id: &str, args: Value) -> Result<Value> {
        self.call_traced(id, args, None).await
    }
    async fn call_with_metadata(
        &self,
        id: &str,
        args: Value,
        metadata: ToolCallMetadata,
    ) -> Result<Value> {
        self.call_traced(id, args, Some(metadata)).await
    }
}
impl ManagedBridge {
    async fn call_traced(
        &self,
        id: &str,
        args: Value,
        metadata: Option<ToolCallMetadata>,
    ) -> Result<Value> {
        let span = crate::trace::RunTrace::span();
        if let Some(trace) = &self.trace {
            let parent = metadata
                .as_ref()
                .and_then(|m| m.traceparent.as_ref())
                .and_then(|p| {
                    let parts: Vec<_> = p.split('-').collect();
                    if parts.len() == 4
                        && parts[0] == "00"
                        && parts[1] == trace.trace_id()
                        && parts[2].len() == 16
                        && parts[2].bytes().all(|b| b.is_ascii_hexdigit())
                        && parts[2].bytes().any(|b| b != b'0')
                    {
                        Some(parts[2].to_owned())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| trace.phase_span());
            trace.emit("tool.started", &span, Some(parent), json!({"tool":if self.entry.tools.iter().any(|t|t==id){id}else{"<undeclared>"},"call_id":metadata.as_ref().map(|m|&m.call_id),"source_run_id":metadata.as_ref().map(|m|&m.source_run_id),"native_call_id":metadata.as_ref().and_then(|m|m.native_call_id.as_ref()),"input_content":null}));
            if self.active().is_ok() && self.entry.tools.iter().any(|t| t == id) {
                trace.queue_content(id, "input", &span, &args);
            }
        }
        let mut result = self.call_inner(id, args).await;
        if let Err(error) = self.persist_budget() {
            if ["storage_error", "revision_conflict"].contains(&error.code.as_str()) {
                let mut failure = self.storage_failure.lock().map_err(Error::io)?;
                if failure.is_none() {
                    *failure = Some(error.clone());
                }
            }
            if result.is_ok() {
                result = Err(error);
            }
        }
        if let Some(hint) = self.budget.hint() {
            match &mut result {
                Ok(value) => {
                    if let Some(object) = value.as_object_mut() {
                        object.insert("_taskloop_budget".into(), serde_json::to_value(&hint)?);
                    }
                }
                Err(error) => {
                    error.message.push_str(&format!(
                        "; execution budget: {}",
                        serde_json::to_string(&hint)?
                    ));
                }
            }
        }
        if let Some(trace) = &self.trace {
            trace.emit("tool.finished", &span, None, json!({"outcome":if result.is_ok(){"succeeded"}else{"failed"},"code":result.as_ref().err().map(|e|&e.code),"output_content":null}));
            if let Ok(value) = &result {
                trace.queue_content(id, "output", &span, value);
            }
        }
        if let Err(error) = &result
            && matches!(error.code.as_str(), "storage_error" | "revision_conflict")
        {
            // Infrastructure failures cannot be erased or reclassified by an
            // executor's return value, even if it retries a later tool call.
            let mut failure = self.storage_failure.lock().map_err(Error::io)?;
            if failure.is_none() {
                *failure = Some(error.clone());
            }
        }
        result
    }
}
impl ManagedBridge {
    async fn call_inner(&self, id: &str, args: Value) -> Result<Value> {
        self.active()?;
        if !self.entry.tools.iter().any(|t| t == id) {
            return Err(Error::new(
                "capability_denied",
                "/tool",
                "tool is not granted to current entry",
            ));
        }
        let spec = self
            .specs()
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::new("binding_missing", "/tool", "registered tool unavailable"))?;
        if self.budget.explicit() {
            let admitted = self
                .budget
                .admit_tool_with(id, || self.owner.admit_tool(id));
            self.persist_budget()?;
            admitted?;
        }
        validate_schema(&spec.input_schema, &args, "/tool/input")?;
        if !self.budget.explicit() {
            let admitted = self
                .budget
                .admit_tool_with(id, || self.owner.admit_tool(id));
            self.persist_budget()?;
            admitted?;
        }
        match id {
            "artifact.read" => {
                let requested = args["id"].as_str().unwrap_or("");
                if let Some(path) = requested.strip_prefix("skill:") {
                    for (skill, package) in self.resources.skills() {
                        if !self.entry.skills.contains(skill) {
                            continue;
                        }
                        if let Some(resource) = path.strip_prefix(&format!("{skill}/"))
                            && let Some(bytes) = package.files.get(resource)
                        {
                            return Ok(
                                json!({"id":requested,"sha256":digest(bytes),"text":std::str::from_utf8(bytes).map_err(|_|Error::new("capability_denied","/resource","this text profile cannot read binary methods"))?}),
                            );
                        }
                    }
                    return Err(Error::new(
                        "capability_denied",
                        "/resource",
                        "method resource is not in the locked current package",
                    ));
                }
                let a = self
                    .context
                    .accepted
                    .iter()
                    .find(|a| a.reference.id == requested)
                    .ok_or_else(|| {
                        Error::new(
                            "capability_denied",
                            "/artifact",
                            "artifact not granted to current entry",
                        )
                    })?;
                Ok(
                    json!({"artifact":a.reference,"text":std::str::from_utf8(&a.bytes).map_err(|_|Error::new("artifact_invalid","/artifact","artifact is not text"))?}),
                )
            }
            "artifact.write" => {
                let candidate =
                    Candidate::json(args["slot"].as_str().unwrap_or(""), &args["value"])?;
                let sha = candidate.sha256();
                self.write(candidate)?;
                Ok(json!({"written":true,"slot":args["slot"],"sha256":sha}))
            }
            _ => {
                let tool =
                    self.inner.bindings.tools.get(id).cloned().ok_or_else(|| {
                        Error::new("binding_missing", "/tool", "tool unavailable")
                    })?;
                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(45),
                    AssertUnwindSafe(tool.call(self.context.clone(), args.clone())).catch_unwind(),
                )
                .await;
                let result = match output {
                    Ok(Ok(Ok(v))) => v,
                    Ok(Ok(Err(e))) => return Err(e),
                    Ok(Err(_)) => {
                        return Err(Error::new(
                            "tool_unavailable",
                            "/tool",
                            "tool callback failed",
                        ));
                    }
                    Err(_) => {
                        return Err(Error::new(
                            "tool_unavailable",
                            "/tool",
                            "tool callback deadline reached",
                        ));
                    }
                };
                self.active()?;
                let attachment_info: Vec<_> = result
                    .files
                    .iter()
                    .map(|f| json!({"slot":f.slot,"sha256":f.sha256(),"bytes":f.bytes.len()}))
                    .collect();
                for candidate in result.files {
                    self.write(candidate)?;
                }
                let receipt = self.receipt(id, &args, &result.payload)?;
                Ok(
                    json!({"receipt_id":receipt,"result":result.payload,"attachments":attachment_info}),
                )
            }
        }
    }
}

pub(crate) struct ManagedExecution {
    pub outcome: ExecutionOutcome,
    pub internal_stop: bool,
    pub native_stop_confirmed: bool,
}
/// One native invocation driver for Flow and Goal. Completion and physical stopping remain explicit.
pub(crate) async fn execute_managed(
    executor: Arc<dyn Executor>,
    invocation: Invocation,
    bridge: Arc<dyn ToolBridge>,
    budget: Arc<crate::budget::BudgetState>,
    cancel: tokio_util::sync::CancellationToken,
    trace: Option<Arc<crate::trace::RunTrace>>,
    observe: bool,
) -> ManagedExecution {
    let run_id = invocation.run_id.clone();
    let time_left = invocation.timeout_ms;
    if let Some(t) = &trace {
        t.emit("executor.capabilities",&t.phase_span(),None,json!({"internal_observation":if executor.internal_observation(){"available"}else{"unavailable"}}));
    }
    let mut internal_stop = false;
    let mut outcome = if time_left == 0 {
        ExecutionOutcome::Failed {
            code: "limit_reached".into(),
            message: "execution budget expired before adapter invocation".into(),
        }
    } else if cancel.is_cancelled() {
        ExecutionOutcome::Cancelled
    } else if !budget.model_constraint_supported() {
        ExecutionOutcome::Failed {
            code: "unsupported_constraint".into(),
            message: "executor cannot enforce requested model iteration limit".into(),
        }
    } else {
        let native = tokio::time::timeout_at(
            budget.deadline,
            AssertUnwindSafe(async {
                if observe {
                    executor
                        .execute_observed(
                            invocation,
                            bridge.clone(),
                            cancel.clone(),
                            trace.clone().map(|t| t as Arc<dyn MilkieObserver>),
                        )
                        .await
                } else {
                    executor
                        .execute(invocation, bridge.clone(), cancel.clone())
                        .await
                }
            })
            .catch_unwind(),
        );
        let returned = tokio::select! {
            biased;
            result = native => Some(result),
            _ = cancel.cancelled() => None,
        };
        match returned {
            Some(Ok(Ok(outcome))) => outcome,
            None => {
                internal_stop = true;
                if confirm_stop(&executor, &run_id).await == StopState::Confirmed {
                    ExecutionOutcome::Cancelled
                } else {
                    ExecutionOutcome::StopUnknown {
                        message: "cancellation did not establish a native stop".into(),
                    }
                }
            }
            Some(failed_execution) => {
                internal_stop = true;
                if budget.guard_time().is_err() {
                    budget.mark(crate::BudgetLimit::RunTime);
                }
                let panicked = matches!(failed_execution, Ok(Err(_)));
                cancel.cancel();
                if confirm_stop(&executor, &run_id).await == StopState::Confirmed {
                    ExecutionOutcome::Failed {
                        code: if panicked {
                            "executor_failed"
                        } else {
                            "limit_reached"
                        }
                        .into(),
                        message: if panicked {
                            "execution adapter panicked"
                        } else {
                            "execution deadline exceeded"
                        }
                        .into(),
                    }
                } else {
                    ExecutionOutcome::StopUnknown {
                        message: "execution failed without a confirmed stop".into(),
                    }
                }
            }
        }
    };
    if budget.guard_time().is_err() && matches!(outcome, ExecutionOutcome::Completed { .. }) {
        internal_stop = true;
        cancel.cancel();
        outcome = ExecutionOutcome::Failed {
            code: "limit_reached".into(),
            message: "executor returned after the execution deadline".into(),
        };
    }
    if let Some(t) = &trace {
        t.emit("executor.finished",&t.phase_span(),None,json!({"outcome":match &outcome {ExecutionOutcome::Completed{..}=>"completed",ExecutionOutcome::Failed{..}=>"failed",ExecutionOutcome::Cancelled=>"cancelled",ExecutionOutcome::StopUnknown{..}=>"stop_unknown"},"code":match &outcome{ExecutionOutcome::Failed{code,..}=>Some(code),_=>None}}));
    }
    budget.finish(&outcome);
    let native_stop_confirmed = !matches!(outcome, ExecutionOutcome::StopUnknown { .. });
    ManagedExecution {
        outcome,
        internal_stop,
        native_stop_confirmed,
    }
}
pub(crate) async fn confirm_stop(executor: &Arc<dyn Executor>, run_id: &str) -> StopState {
    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        AssertUnwindSafe(executor.stop_or_confirm(run_id)).catch_unwind(),
    )
    .await
    {
        Ok(Ok(state)) => state,
        _ => StopState::Unknown,
    }
}
