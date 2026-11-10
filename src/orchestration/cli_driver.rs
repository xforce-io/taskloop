//! Foreground ownership shared by the bundled CLI and compiled application hosts.
use super::{control_socket::ControlEndpoint, *};
use crate::{runtime::DispatchScope, *};

pub struct CliDriver {
    runtime: Runtime,
    caller: Caller,
    endpoint: Option<ControlEndpoint>,
    selected: Option<String>,
}
impl CliDriver {
    pub async fn open(
        options: RuntimeOptions,
        definitions: Vec<Definition>,
        bindings: HostBindings,
        registry: OrchestrationRegistry,
        orchestration: OrchestrationOptions,
        trace: Option<TraceSetup>,
        caller: Caller,
    ) -> Result<Self> {
        super::core::nonempty(&caller.principal, "/caller")?;
        let runtime = Runtime::open_orchestrated_mode(
            options,
            definitions,
            bindings,
            registry,
            orchestration,
            trace,
            true,
        )
        .await?;
        let endpoint = ControlEndpoint::start(&runtime, caller.clone())?;
        Ok(Self {
            runtime,
            caller,
            endpoint: Some(endpoint),
            selected: None,
        })
    }
    fn check_selection(&self, id: &str) -> Result<()> {
        if self
            .selected
            .as_ref()
            .is_some_and(|selected| selected != id)
        {
            return Err(Error::new(
                "input_invalid",
                "/goal",
                "a foreground driver owns one selected Goal",
            ));
        }
        Ok(())
    }
    fn select(&mut self, id: &str) -> Result<()> {
        self.check_selection(id)?;
        self.runtime.goals()?.core.authorized(id, &self.caller)?;
        *self
            .runtime
            .inner
            .dispatch_scope
            .lock()
            .map_err(Error::io)? = DispatchScope::Goal(Some(id.into()));
        self.selected = Some(id.into());
        self.runtime.inner.notify.notify_one();
        Ok(())
    }
    pub async fn submit(&mut self, request: GoalRequest) -> Result<GoalReceipt> {
        if self.selected.is_some() {
            return Err(Error::new(
                "input_invalid",
                "/goal",
                "foreground Goal has already been selected",
            ));
        }
        let receipt = self.runtime.goals()?.submit(request, &self.caller).await?;
        self.select(&receipt.goal_id)?;
        Ok(receipt)
    }
    pub async fn resume(
        &mut self,
        id: &str,
        key: &str,
        revision: u64,
    ) -> Result<GoalCommandReceipt> {
        self.check_selection(id)?;
        let receipt = self
            .runtime
            .goals()?
            .resume(id, key, revision, &self.caller)
            .await?;
        self.select(id)?;
        Ok(receipt)
    }
    pub async fn get(&self) -> Result<GoalSnapshot> {
        let id = self.selected.as_ref().ok_or_else(|| {
            Error::new(
                "not_ready",
                "/goal",
                "foreground Goal has not been selected",
            )
        })?;
        self.runtime.goals()?.get(id, &self.caller).await
    }
    pub async fn pause(&self, key: &str) -> Result<GoalCommandReceipt> {
        let snapshot = self.get().await?;
        self.runtime
            .goals()?
            .stop(&snapshot.goal_id, key, snapshot.revision, &self.caller)
            .await
    }
    pub async fn wait(&self) -> Result<GoalSnapshot> {
        loop {
            let snapshot = self.get().await?;
            if matches!(
                snapshot.status,
                GoalStatus::Finished | GoalStatus::Stopped | GoalStatus::Blocked
            ) {
                return Ok(snapshot);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(endpoint) = self.endpoint.take() {
            endpoint.close().await;
        }
        self.runtime.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn review_regression_rejected_second_goal_resume_has_no_side_effects() {
        let directory = tempfile::Builder::new()
            .prefix("tl10-resume-")
            .tempdir_in("/tmp")
            .unwrap();
        let (mut registry, mut bindings) = super::super::tests::fixture();
        let prepared = super::super::tests::prepared_fixture();
        let limits = prepared.effective_limits.clone();
        bindings.grants.insert(
            "test".into(),
            Grant {
                definitions: vec![],
                tools: vec!["artifact.write".into()],
                limits: ResourceLimits {
                    max_wall_ms: limits.max_wall_ms,
                    max_active_ms: limits.max_active_ms,
                    max_runs: limits.max_runs,
                    max_transitions: limits.max_transitions,
                },
            },
        );
        registry.validate_and_freeze(&bindings).unwrap();
        let options = OrchestrationOptions {
            grants: std::collections::BTreeMap::from([(
                "test".into(),
                GoalGrant {
                    profile_digests: vec![registry.profile_digest("document-review@1").unwrap()],
                    capabilities: vec!["review@1".into()],
                    tools: vec!["artifact.write".into()],
                    limits: limits.clone(),
                },
            )]),
            limits,
            max_admitted_goals: 8,
            max_spec_bytes: 1048576,
            max_plan_bytes: 4194304,
            max_nodes: 256,
            max_edges: 4096,
            max_workers_per_node: 32,
            max_json_candidate_bytes: 4194304,
        };
        let caller = Caller {
            principal: "alice".into(),
            grants: vec!["test".into()],
        };
        let mut driver = CliDriver::open(
            RuntimeOptions::new(directory.path().join("store")),
            vec![],
            bindings,
            registry,
            options,
            None,
            caller.clone(),
        )
        .await
        .unwrap();
        let goals = driver.runtime.goals().unwrap();
        let first = goals
            .submit(prepared.request.clone(), &caller)
            .await
            .unwrap()
            .goal_id;
        let mut other_request = prepared.request;
        other_request.request_key = "other-goal".into();
        let second = goals.submit(other_request, &caller).await.unwrap().goal_id;
        let second_view = goals.get(&second, &caller).await.unwrap();
        goals
            .stop(&second, "pause-second", second_view.revision, &caller)
            .await
            .unwrap();
        let before = goals.get(&second, &caller).await.unwrap();
        driver.select(&first).unwrap();
        let error = driver
            .resume(&second, "must-not-resume", before.revision)
            .await
            .unwrap_err();
        assert_eq!(error.code, "input_invalid");
        let after = goals.get(&second, &caller).await.unwrap();
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(driver.selected.as_deref(), Some(first.as_str()));
        let first_view = goals.get(&first, &caller).await.unwrap();
        goals
            .cancel(&first, "close-first", first_view.revision, &caller)
            .await
            .unwrap();
        goals
            .cancel(&second, "close-second", after.revision, &caller)
            .await
            .unwrap();
        driver.shutdown().await.unwrap();
    }
}
