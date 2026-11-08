use super::{plan::bounded_json, registry::FrozenRegistry, schema::invalid, *};
use crate::{Caller, Error, HostBindings, Result, json::digest};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PreparedGoal {
    pub request: GoalRequest,
    pub request_fingerprint: String,
    pub spec_digest: String,
    pub profile_digest: String,
    pub effective_limits: GoalLimits,
    pub limit_sources: GoalLimitSources,
    pub admitted_at_utc_ms: u64,
    pub wall_deadline_utc_ms: u64,
    pub planning_deadline_utc_ms: u64,
    pub allowed_capabilities: Vec<String>,
    pub allowed_tools: Vec<String>,
}
impl FrozenRegistry {
    /// Preliminary validation only: Store must still authorize imports, parent and idempotency atomically.
    pub(crate) fn prepare_goal(
        &self,
        bytes: &[u8],
        caller: &Caller,
        bindings: &HostBindings,
        options: &OrchestrationOptions,
        now: u64,
    ) -> Result<PreparedGoal> {
        options.validate()?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(invalid("/", "request exceeds 4 MiB"));
        }
        let value = crate::parse_unique(bytes)?;
        self.schemas.contract("goal-request", &value)?;
        let request: GoalRequest = serde_json::from_value(value)?;
        let grant = options
            .grants
            .get(&request.grant_ref)
            .filter(|_| caller.grants.contains(&request.grant_ref))
            .ok_or_else(|| {
                Error::new(
                    "capability_denied",
                    "/grant_ref",
                    "GoalGrant not authorized",
                )
            })?;
        let tool_grant = bindings
            .grants
            .get(&request.grant_ref)
            .filter(|_| caller.grants.contains(&request.grant_ref))
            .ok_or_else(|| {
                Error::new(
                    "capability_denied",
                    "/grant_ref",
                    "host tool grant not authorized",
                )
            })?;
        if !request.spec.imports.is_empty()
            && (!grant.tools.iter().any(|t| t == "artifact.read")
                || !tool_grant.tools.iter().any(|t| t == "artifact.read"))
        {
            return Err(Error::new(
                "capability_denied",
                "/spec/imports",
                "artifact.read must be explicitly granted to import artifacts",
            ));
        }
        let profile = self
            .profiles
            .get(&request.spec.profile_ref)
            .ok_or_else(|| invalid("/spec/profile_ref", "unknown Profile"))?;
        let profile_digest = &self.profile_digests[&request.spec.profile_ref];
        if !grant.profile_digests.contains(profile_digest) {
            return Err(Error::new(
                "capability_denied",
                "/spec/profile_ref",
                "Profile digest not authorized",
            ));
        }
        self.schemas
            .validate(&profile.goal_input_schema, &request.spec.inputs)?;
        let spec_bytes = bounded_json(&request.spec, options.max_spec_bytes)?;
        let mut limits = options.effective_limits(&request.spec.limits, grant)?;
        limits.max_active_ms = limits.max_active_ms.min(tool_grant.limits.max_active_ms);
        limits.max_wall_ms = limits.max_wall_ms.min(tool_grant.limits.max_wall_ms);
        limits.max_runs = limits.max_runs.min(tool_grant.limits.max_runs);
        limits.max_transitions = limits
            .max_transitions
            .min(tool_grant.limits.max_transitions);
        super::plan::validate_limits(&limits)?;
        let wall = checked_deadline(now, limits.max_wall_ms)?;
        let planning = checked_deadline(
            now,
            request
                .spec
                .planning
                .max_planning_ms
                .min(limits.max_wall_ms),
        )?;
        if request.spec.planning.mode == PlanningMode::Workflow {
            if !request
                .spec
                .planning
                .template_ref
                .as_ref()
                .is_some_and(|id| profile.templates.contains(id))
            {
                return Err(invalid(
                    "/spec/planning/template_ref",
                    "workflow template outside Profile",
                ));
            }
        } else if request.spec.planning.template_ref.is_some() {
            return Err(invalid(
                "/spec/planning/template_ref",
                "autonomous mode has no template",
            ));
        }
        let tools = profile
            .allowed_tools
            .iter()
            .filter(|id| grant.tools.contains(id) && tool_grant.tools.contains(id))
            .cloned()
            .collect::<Vec<_>>();
        // Fixed-stage checks and the planner cannot be replaced by a less privileged plan.
        for entry_id in std::iter::once(&profile.planner_entry_ref).chain(
            profile
                .checks
                .iter()
                .map(|c| &self.checkers[&c.checker_ref].entry_ref),
        ) {
            if self.entries[entry_id]
                .contract
                .tools
                .iter()
                .any(|tool| !tools.contains(tool))
            {
                return Err(Error::new(
                    "capability_denied",
                    "/tools",
                    "planner/required checker tool outside grant intersection",
                ));
            }
        }
        let capabilities = profile
            .capabilities
            .iter()
            .filter(|id| grant.capabilities.contains(id))
            .cloned()
            .collect();
        let request_fingerprint = digest(&bounded_json(&request, 4 * 1024 * 1024)?);
        Ok(PreparedGoal {
            spec_digest: digest(&spec_bytes),
            profile_digest: profile_digest.clone(),
            effective_limits: limits,
            limit_sources: GoalLimitSources {
                requested: request.spec.limits.clone(),
                goal_grant: grant.limits.clone(),
                host: options.limits.clone(),
                tool_grant: tool_grant.limits.clone(),
            },
            admitted_at_utc_ms: now,
            wall_deadline_utc_ms: wall,
            planning_deadline_utc_ms: planning,
            allowed_capabilities: capabilities,
            allowed_tools: tools,
            request_fingerprint,
            request,
        })
    }
}
fn checked_deadline(now: u64, duration: u64) -> Result<u64> {
    if std::time::Instant::now()
        .checked_add(Duration::from_millis(duration))
        .is_none()
    {
        return Err(invalid(
            "/spec/limits",
            "deadline not representable by monotonic clock",
        ));
    }
    now.checked_add(duration)
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(|| invalid("/spec/limits", "deadline overflow"))
}
