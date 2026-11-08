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
    fn select(&mut self, id: &str) -> Result<()> {
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
