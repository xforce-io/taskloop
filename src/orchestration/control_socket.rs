//! Local online commands. OS credentials choose the trusted Caller; frames cannot name one.
use super::{client::GoalClient, plan::bounded_json, *};
use crate::*;
use serde::{Deserialize, Serialize};
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::PathBuf,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

const MAX_FRAME: usize = 4_194_304;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalCommandResult {
    pub protocol_version: u32,
    pub receipt: GoalCommandReceipt,
    pub revision: u64,
    pub status: GoalStatus,
    pub outcome: Option<GoalOutcome>,
    pub pending_runs: Vec<String>,
    pub blockers: std::collections::BTreeSet<GoalBlocker>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Signal,
    Cancel,
    Stop,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    protocol_version: u32,
    kind: Kind,
    goal_id: String,
    request_key: String,
    expected_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<SignalPayload>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalPayload {
    wait_id: String,
    nonce: String,
    payload: serde_json::Value,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Response {
    Accepted { result: GoalCommandResult },
    Rejected { error: Error },
}

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(Error::new(
            "input_invalid",
            "/frame",
            "command frame must be in 1..=4 MiB",
        ));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}
async fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(Error::new(
            "input_invalid",
            "/frame",
            "command response exceeds 4 MiB",
        ));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}
async fn handle(goals: &Goals, caller: &Caller, request: Request) -> Result<GoalCommandResult> {
    if request.protocol_version != 1 {
        return Err(Error::new(
            "input_invalid",
            "/protocol_version",
            "expected protocol version 1",
        ));
    }
    let receipt = match request.kind {
        Kind::Stop | Kind::Cancel if request.payload.is_some() => {
            return Err(Error::new(
                "input_invalid",
                "/payload",
                "stop/cancel do not accept payload",
            ));
        }
        Kind::Stop => {
            goals
                .stop(
                    &request.goal_id,
                    &request.request_key,
                    request.expected_revision,
                    caller,
                )
                .await?
        }
        Kind::Cancel => {
            goals
                .cancel(
                    &request.goal_id,
                    &request.request_key,
                    request.expected_revision,
                    caller,
                )
                .await?
        }
        Kind::Signal => {
            let payload = request
                .payload
                .ok_or_else(|| Error::new("input_invalid", "/payload", "signal payload missing"))?;
            goals
                .signal(
                    &request.goal_id,
                    GoalSignalRequest {
                        protocol_version: 1,
                        wait_id: payload.wait_id,
                        nonce: payload.nonce,
                        payload: payload.payload,
                        request_key: request.request_key,
                        expected_revision: request.expected_revision,
                    },
                    caller,
                )
                .await?
        }
    };
    let snapshot = goals.get(&request.goal_id, caller).await?;
    Ok(GoalCommandResult {
        protocol_version: 1,
        receipt,
        revision: snapshot.revision,
        status: snapshot.status,
        outcome: snapshot.outcome,
        pending_runs: snapshot.stop.pending_runs,
        blockers: snapshot.blockers,
    })
}

pub(crate) struct ControlEndpoint {
    path: PathBuf,
    inode: (u64, u64),
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}
impl ControlEndpoint {
    pub(crate) fn start(runtime: &Runtime, caller: Caller) -> Result<Self> {
        let root = runtime.inner.options.store_dir.canonicalize()?;
        let uid = unsafe { libc::geteuid() };
        if std::fs::metadata(&root)?.uid() != uid {
            return Err(Error::new(
                "capability_denied",
                "/store",
                "local control requires the current OS user's Store",
            ));
        }
        let path = root.join("owner.sock");
        if let Ok(existing) = std::fs::symlink_metadata(&path) {
            if !existing.file_type().is_socket() {
                return Err(Error::new(
                    "capability_denied",
                    "/socket",
                    "refusing to replace a non-socket owner.sock",
                ));
            }
            // Runtime has already acquired owner.lock; no other valid host can own this endpoint.
            std::fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let metadata = std::fs::metadata(&path)?;
        let inode = (metadata.dev(), metadata.ino());
        let goals = runtime.goals()?;
        let cancel = CancellationToken::new();
        let stopped = cancel.clone();
        let task = tokio::spawn(async move {
            let mut jobs = JoinSet::new();
            loop {
                tokio::select! {
                    _ = stopped.cancelled() => break,
                    _ = jobs.join_next(), if !jobs.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { break; };
                        if jobs.len() >= 32 || stream.peer_cred().map(|c| c.uid() != uid).unwrap_or(true) { continue; }
                        let goals = goals.clone(); let caller = caller.clone();
                        jobs.spawn(async move {
                            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                                let result = async {
                                    let bytes = read_frame(&mut stream).await?;
                                    let request: Request = serde_json::from_value(crate::parse_unique(&bytes)?)?;
                                    handle(&goals, &caller, request).await
                                }.await;
                                let response = match result { Ok(result) => Response::Accepted { result }, Err(error) => Response::Rejected { error } };
                                if let Ok(bytes) = bounded_json(&response, MAX_FRAME) { let _ = write_frame(&mut stream, &bytes).await; }
                            }).await;
                        });
                    },
                }
            }
            jobs.shutdown().await;
        });
        Ok(Self {
            path,
            inode,
            cancel,
            task: Some(task),
        })
    }
    fn remove(&self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| (m.dev(), m.ino()) == self.inode && m.file_type().is_socket())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
    pub(crate) async fn close(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.remove();
    }
}
impl Drop for ControlEndpoint {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.remove();
    }
}
impl GoalClient {
    async fn command(&self, request: Request) -> Result<GoalCommandResult> {
        let bytes = bounded_json(&request, MAX_FRAME)?;
        let operation = async {
            let mut stream = UnixStream::connect(self.root.join("owner.sock"))
                .await
                .map_err(|_| {
                    Error::new(
                        "host_not_running",
                        "/socket",
                        "no online TaskLoop host; command was not queued",
                    )
                })?;
            let uid = unsafe { libc::geteuid() };
            if stream.peer_cred().map(|c| c.uid() != uid).unwrap_or(true) {
                return Err(Error::new(
                    "capability_denied",
                    "/socket",
                    "local host OS identity differs",
                ));
            }
            write_frame(&mut stream, &bytes).await?;
            let reply = read_frame(&mut stream).await?;
            match serde_json::from_value::<Response>(crate::parse_unique(&reply)?)? {
                Response::Accepted { result } => {
                    let kind = match request.kind {
                        Kind::Signal => "signal",
                        Kind::Cancel => "cancel",
                        Kind::Stop => "stop",
                    };
                    if result.protocol_version != 1
                        || result.receipt.protocol_version != 1
                        || result.receipt.goal_id != request.goal_id
                        || result.receipt.request_key != request.request_key
                        || result.receipt.kind != kind
                        || !result.receipt.accepted
                    {
                        return Err(Error::new(
                            "input_invalid",
                            "/response",
                            "command response correlation differs",
                        ));
                    }
                    Ok(result)
                }
                Response::Rejected { error } => Err(error),
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(6), operation)
            .await
            .map_err(|_| {
                Error::new(
                    "host_not_running",
                    "/socket",
                    "online command timed out; retry the same key to obtain its persisted receipt",
                )
            })?
    }
    pub async fn stop(
        &self,
        goal: &str,
        request_key: &str,
        revision: u64,
    ) -> Result<GoalCommandResult> {
        self.command(Request {
            protocol_version: 1,
            kind: Kind::Stop,
            goal_id: goal.into(),
            request_key: request_key.into(),
            expected_revision: revision,
            payload: None,
        })
        .await
    }
    pub async fn cancel(
        &self,
        goal: &str,
        request_key: &str,
        revision: u64,
    ) -> Result<GoalCommandResult> {
        self.command(Request {
            protocol_version: 1,
            kind: Kind::Cancel,
            goal_id: goal.into(),
            request_key: request_key.into(),
            expected_revision: revision,
            payload: None,
        })
        .await
    }
    pub async fn signal(&self, goal: &str, signal: GoalSignalRequest) -> Result<GoalCommandResult> {
        self.command(Request {
            protocol_version: signal.protocol_version,
            kind: Kind::Signal,
            goal_id: goal.into(),
            request_key: signal.request_key,
            expected_revision: signal.expected_revision,
            payload: Some(SignalPayload {
                wait_id: signal.wait_id,
                nonce: signal.nonce,
                payload: signal.payload,
            }),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn length_prefix_rejects_oversized_frames_before_allocating_a_body() {
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        sender.write_u32(MAX_FRAME as u32 + 1).await.unwrap();
        assert_eq!(
            read_frame(&mut receiver).await.unwrap_err().code,
            "input_invalid"
        );
    }
    #[test]
    fn command_frames_cannot_supply_a_caller_or_unknown_action() {
        let frame = serde_json::json!({"protocol_version":1,"kind":"stop","goal_id":"goal-1","request_key":"key","expected_revision":1,"principal":"alice"});
        assert!(serde_json::from_value::<Request>(frame).is_err());
        let frame = serde_json::json!({"protocol_version":1,"kind":"resume","goal_id":"goal-1","request_key":"key","expected_revision":1});
        assert!(serde_json::from_value::<Request>(frame).is_err());
    }
}
