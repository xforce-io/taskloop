use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use taskloop::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HostConfig {
    protocol_version: u32,
    factory: String,
    store_dir: PathBuf,
    resources_root: PathBuf,
    configuration_version: String,
    milkie: NativeConfig,
    entry_budgets: BTreeMap<String, RunBudgetPolicy>,
    goal_grants: BTreeMap<String, LocalGrant>,
    caller_grants: Vec<String>,
    limits: GoalLimits,
    max_admitted_goals: usize,
    max_tool_calls_per_run: u64,
    max_candidate_bytes: u64,
    trace: Option<TraceConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeConfig {
    program: PathBuf,
    args: Vec<String>,
    connection_file: PathBuf,
    data_dir: PathBuf,
    config_dir: PathBuf,
    session_dir: PathBuf,
    #[serde(default)]
    env: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalGrant {
    profile_refs: Vec<String>,
    capabilities: Vec<String>,
    tools: Vec<String>,
    limits: GoalLimits,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceConfig {
    options: TraceOptions,
    policy: MetadataPolicy,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataPolicy {
    policy_id: String,
    max_total_events: u64,
    max_total_bytes: u64,
    max_runs: u32,
    retention_ms: u64,
}

pub(super) fn local_caller(grants: Vec<String>) -> Caller {
    Caller {
        principal: format!("local-uid:{}", unsafe { libc::geteuid() }),
        grants,
    }
}
impl HostConfig {
    pub(super) async fn open(self) -> Result<(CliDriver, PathBuf, Caller)> {
        if self.protocol_version != 1
            || self.factory != "document-review@1"
            || !self.store_dir.is_absolute()
            || !self.resources_root.is_absolute()
            || self.max_tool_calls_per_run == 0
            || self.configuration_version.is_empty()
            || self.caller_grants.is_empty()
            || self
                .caller_grants
                .iter()
                .any(|id| !self.goal_grants.contains_key(id))
        {
            return Err(Error::new(
                "input_invalid",
                "/config",
                "require protocol 1, compiled document-review@1 factory, absolute paths and explicit authorized caller grants",
            ));
        }
        let native = self.milkie;
        let mut native_config = MilkieConfig::new(
            native.program,
            native.connection_file,
            native.data_dir,
            MilkieStorage {
                config_dir: native.config_dir,
                session_dir: native.session_dir,
            },
        )?
        .args(native.args);
        for (key, value) in native.env {
            native_config = native_config.env(key, value);
        }
        let mut bindings = HostBindings::default()
            .configuration_version(self.configuration_version)
            .executor("milkie", Arc::new(MilkieExecutor::new(native_config)?));
        for (id, grant) in &self.goal_grants {
            bindings = bindings.grant(
                id,
                Grant {
                    definitions: vec![],
                    tools: grant.tools.clone(),
                    limits: ResourceLimits {
                        max_wall_ms: grant.limits.max_wall_ms,
                        max_active_ms: grant.limits.max_active_ms,
                        max_runs: grant.limits.max_runs,
                        max_transitions: grant.limits.max_transitions,
                    },
                },
            );
        }
        let mut registry =
            document_review_registry(&self.resources_root, "milkie", &self.entry_budgets)?;
        let receipt = registry.validate_and_freeze(&bindings)?;
        let grants = self
            .goal_grants
            .into_iter()
            .map(|(id, grant)| {
                let digests = grant
                    .profile_refs
                    .into_iter()
                    .map(|profile| {
                        receipt.profiles.get(&profile).cloned().ok_or_else(|| {
                            Error::new(
                                "input_invalid",
                                "/goal_grants/profile_refs",
                                "profile is not in the compiled factory",
                            )
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok((
                    id,
                    GoalGrant {
                        profile_digests: digests,
                        capabilities: grant.capabilities,
                        tools: grant.tools,
                        limits: grant.limits,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let orchestration = OrchestrationOptions {
            grants,
            limits: self.limits,
            max_admitted_goals: self.max_admitted_goals,
            max_spec_bytes: 1_048_576,
            max_plan_bytes: 4_194_304,
            max_nodes: 256,
            max_edges: 4096,
            max_workers_per_node: 32,
            max_json_candidate_bytes: 4_194_304,
        };
        let mut options = RuntimeOptions::new(&self.store_dir);
        options.max_tool_calls_per_run = self.max_tool_calls_per_run;
        options.max_candidate_bytes = self.max_candidate_bytes;
        let trace = self.trace.map(|trace| TraceSetup {
            options: trace.options,
            policy: TracePolicy {
                policy_id: trace.policy.policy_id,
                max_total_events: trace.policy.max_total_events,
                max_total_bytes: trace.policy.max_total_bytes,
                max_runs: trace.policy.max_runs,
                retention_ms: trace.policy.retention_ms,
                max_content_bytes: 0,
                content_tools: BTreeMap::new(),
                filter_version: None,
                filter: None,
            },
        });
        let caller = local_caller(self.caller_grants);
        let driver = CliDriver::open(
            options,
            vec![],
            bindings,
            registry,
            orchestration,
            trace,
            caller.clone(),
        )
        .await?;
        Ok((driver, self.store_dir, caller))
    }
}
