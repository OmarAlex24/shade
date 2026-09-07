use serde::Deserialize;
use serde::de::DeserializeOwned;
use shade_protocol::{
    Actor, ActorKind, CheckpointId, CompactContext, ConflictOutcome, ExecuteRequest, Intent,
    LeaseId, ObjectId, OpenSession, OpenedSession, OperationId, Outcome, PROTOCOL_VERSION,
    PendingHandoff, Query, QueryRequest, ResponseBody, ReviewAction, ReviewId, ReviewRequired,
    SessionId, SessionStatus, ShadeError, WireRequest, WireResponse, WorkspaceId,
    WorkspaceSelector,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixStream, unix::OwnedReadHalf};
use tokio::sync::watch;
use tokio::time::Instant;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_OPERATION_POLL_INTERVAL: Duration = Duration::from_millis(25);
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RESPONSE_FRAME_BYTES: usize = 4 * 1024 * 1024;
const OPERATION_LOOKUP_GRACE: Duration = Duration::from_secs(1);

pub type ClientResult<T> = Result<T, ClientError>;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("CLIENT_TIMEOUT")]
    Timeout {
        operation: Option<OperationId>,
        idempotency_key: Option<String>,
    },
    #[error("CLIENT_RESPONSE_TOO_LARGE")]
    ResponseTooLarge,
    #[error("CLIENT_RESPONSE_INCOMPLETE")]
    ResponseIncomplete,
    #[error("CLIENT_RESPONSE_EMPTY")]
    ResponseEmpty,
    #[error("CLIENT_PROTOCOL_VERSION: expected {expected}, received {received}")]
    ProtocolVersion { expected: u16, received: u16 },
    #[error("CLIENT_REQUEST_MISMATCH")]
    RequestMismatch,
    #[error("CLIENT_EVENT_CURSOR_INVALID")]
    InvalidCursor,
    #[error("CLIENT_OPERATION_RECORD_INVALID")]
    InvalidOperationRecord,
    #[error("CLIENT_UNEXPECTED_OUTCOME: {0}")]
    UnexpectedOutcome(&'static str),
    #[error("CLIENT_CONFIGURATION_INVALID: {0}")]
    InvalidConfiguration(&'static str),
    #[error("SHADE_DOMAIN_ERROR: {0:?}")]
    Domain(ShadeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl ClientError {
    pub fn operation(&self) -> Option<&OperationId> {
        match self {
            Self::Timeout { operation, .. } => operation.as_ref(),
            Self::Domain(error) => error.operation.as_ref(),
            _ => None,
        }
    }

    /// Durable retry handle retained even when the transport deadline expires
    /// before the daemon can return its operation id.
    pub fn idempotency_key(&self) -> Option<&str> {
        match self {
            Self::Timeout {
                idempotency_key, ..
            } => idempotency_key.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShadeClientOptions {
    pub request_timeout: Duration,
    pub operation_timeout: Duration,
    pub operation_poll_interval: Duration,
    pub heartbeat_interval: Duration,
    pub max_response_frame_bytes: usize,
    /// Recover a session whose lease expired instead of retiring the handle.
    ///
    /// A lease expiry means the workspace went dormant, not that the work is
    /// gone, so the default is to reattach once and carry on. Turn it off to
    /// have the handle retire on the first expiry, as it did before.
    pub reattach_on_expiry: bool,
}

impl Default for ShadeClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            operation_poll_interval: DEFAULT_OPERATION_POLL_INTERVAL,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            max_response_frame_bytes: DEFAULT_MAX_RESPONSE_FRAME_BYTES,
            reattach_on_expiry: true,
        }
    }
}

impl ShadeClientOptions {
    fn validate(&self) -> ClientResult<()> {
        if self.request_timeout.is_zero() {
            return Err(ClientError::InvalidConfiguration(
                "request_timeout must be positive",
            ));
        }
        if self.operation_timeout.is_zero() {
            return Err(ClientError::InvalidConfiguration(
                "operation_timeout must be positive",
            ));
        }
        if self.operation_poll_interval.is_zero() {
            return Err(ClientError::InvalidConfiguration(
                "operation_poll_interval must be positive",
            ));
        }
        if self.heartbeat_interval.is_zero() {
            return Err(ClientError::InvalidConfiguration(
                "heartbeat_interval must be positive",
            ));
        }
        if self.max_response_frame_bytes == 0 {
            return Err(ClientError::InvalidConfiguration(
                "max_response_frame_bytes must be positive",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ShadeClient {
    socket: PathBuf,
    actor: Actor,
    options: Arc<ShadeClientOptions>,
}

impl ShadeClient {
    pub fn new(socket: impl AsRef<Path>, actor: Actor) -> Self {
        Self::with_options(socket, actor, ShadeClientOptions::default())
            .expect("default Shade client options are valid")
    }

    pub fn with_options(
        socket: impl AsRef<Path>,
        actor: Actor,
        options: ShadeClientOptions,
    ) -> ClientResult<Self> {
        options.validate()?;
        Ok(Self {
            socket: socket.as_ref().to_path_buf(),
            actor,
            options: Arc::new(options),
        })
    }

    pub fn cli(socket: impl AsRef<Path>) -> Self {
        Self::new(
            socket,
            Actor {
                kind: ActorKind::Cli,
                // The daemon keys durable operations and successor ownership by
                // actor identity, so this must survive individual CLI processes.
                id: "cli".into(),
            },
        )
    }

    pub async fn execute(&self, intent: Intent) -> ClientResult<WireResponse> {
        self.execute_idempotent(intent, ulid::Ulid::new().to_string())
            .await
    }

    pub async fn execute_idempotent(
        &self,
        intent: Intent,
        idempotency_key: impl Into<String>,
    ) -> ClientResult<WireResponse> {
        let deadline = Instant::now() + self.options.request_timeout;
        let response = self
            .execute_idempotent_until(intent, idempotency_key.into(), deadline)
            .await?;
        self.adopt_direct_handoffs_until(response, deadline).await
    }

    /// Execute and follow a durable accepted operation to a domain outcome.
    /// The configured operation deadline covers both the initial request and polling.
    pub async fn execute_wait(&self, intent: Intent) -> ClientResult<WireResponse> {
        self.execute_wait_for(intent, self.options.operation_timeout)
            .await
    }

    pub async fn execute_wait_idempotent(
        &self,
        intent: Intent,
        idempotency_key: impl Into<String>,
    ) -> ClientResult<WireResponse> {
        self.execute_wait_idempotent_for(intent, idempotency_key, self.options.operation_timeout)
            .await
    }

    pub async fn execute_wait_for(
        &self,
        intent: Intent,
        timeout: Duration,
    ) -> ClientResult<WireResponse> {
        self.execute_wait_idempotent_for(intent, ulid::Ulid::new().to_string(), timeout)
            .await
    }

    pub async fn execute_wait_idempotent_for(
        &self,
        intent: Intent,
        idempotency_key: impl Into<String>,
        timeout: Duration,
    ) -> ClientResult<WireResponse> {
        if timeout.is_zero() {
            return Err(ClientError::InvalidConfiguration(
                "operation timeout must be positive",
            ));
        }
        let deadline = Instant::now() + timeout;
        let idempotency_key = idempotency_key.into();
        let response = self
            .execute_idempotent_until(intent, idempotency_key.clone(), deadline)
            .await?;
        self.settle_until(response, deadline, Some(idempotency_key))
            .await
    }

    pub async fn settle(&self, response: WireResponse) -> ClientResult<WireResponse> {
        self.settle_until(
            response,
            Instant::now() + self.options.operation_timeout,
            None,
        )
        .await
    }

    async fn settle_until(
        &self,
        mut response: WireResponse,
        deadline: Instant,
        mut idempotency_key: Option<String>,
    ) -> ClientResult<WireResponse> {
        loop {
            response = self
                .settle_operation_until(response, deadline, idempotency_key.as_deref())
                .await?;
            let Some(handoff) = pending_handoff(&response)? else {
                return Ok(response);
            };
            let handoff_key = format!("handoff:{}", handoff.handoff_id.0);
            response = self
                .execute_idempotent_until(
                    Intent::SuccessorAdopt {
                        handoff_id: handoff.handoff_id.clone(),
                    },
                    handoff_key.clone(),
                    deadline,
                )
                .await?;
            idempotency_key = Some(handoff_key);
        }
    }

    /// Keep the one-shot API non-blocking for accepted operations while ensuring
    /// the internal pending-handoff representation never escapes to CLI callers.
    async fn adopt_direct_handoffs_until(
        &self,
        mut response: WireResponse,
        deadline: Instant,
    ) -> ClientResult<WireResponse> {
        while let Some(handoff) = pending_handoff(&response)? {
            response = self
                .execute_idempotent_until(
                    Intent::SuccessorAdopt {
                        handoff_id: handoff.handoff_id.clone(),
                    },
                    format!("handoff:{}", handoff.handoff_id.0),
                    deadline,
                )
                .await?;
        }
        Ok(response)
    }

    async fn settle_operation_until(
        &self,
        mut response: WireResponse,
        deadline: Instant,
        idempotency_key: Option<&str>,
    ) -> ClientResult<WireResponse> {
        let operation = match &response.body {
            ResponseBody::Ok {
                outcome: Outcome::Accepted { operation_id },
            } => operation_id.clone(),
            _ => return Ok(response),
        };
        loop {
            if Instant::now() >= deadline {
                return Err(ClientError::Timeout {
                    operation: Some(operation),
                    idempotency_key: idempotency_key.map(str::to_owned),
                });
            }
            let query = match self
                .query_until(
                    Query::Operation {
                        operation_id: operation.clone(),
                    },
                    deadline,
                )
                .await
            {
                Err(ClientError::Timeout { .. }) => {
                    return Err(ClientError::Timeout {
                        operation: Some(operation),
                        idempotency_key: idempotency_key.map(str::to_owned),
                    });
                }
                result => result?,
            };
            let record = operation_record(query)?;
            match record.state.as_str() {
                "running" => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(ClientError::Timeout {
                            operation: Some(operation),
                            idempotency_key: idempotency_key.map(str::to_owned),
                        });
                    }
                    tokio::time::sleep(self.options.operation_poll_interval.min(remaining)).await;
                }
                "completed" => {
                    response.body = ResponseBody::Ok {
                        outcome: record.outcome.ok_or(ClientError::InvalidOperationRecord)?,
                    };
                    return Ok(response);
                }
                "failed" => {
                    response.body = ResponseBody::Error {
                        error: record.error.ok_or(ClientError::InvalidOperationRecord)?,
                    };
                    return Ok(response);
                }
                _ => return Err(ClientError::InvalidOperationRecord),
            }
        }
    }

    pub async fn query(&self, query: Query) -> ClientResult<WireResponse> {
        self.query_until(query, Instant::now() + self.options.request_timeout)
            .await
    }

    pub async fn diagnostics(
        &self,
        diagnostics_id: impl Into<String>,
    ) -> ClientResult<shade_protocol::Diagnostic> {
        completed_only(
            terminal(
                self.query(Query::Diagnostics {
                    diagnostics_id: diagnostics_id.into(),
                })
                .await?,
            )?,
            "diagnostics",
        )
    }

    async fn execute_idempotent_until(
        &self,
        intent: Intent,
        idempotency_key: String,
        deadline: Instant,
    ) -> ClientResult<WireResponse> {
        let request_id = ulid::Ulid::new().to_string();
        let result = self
            .request_until(
                WireRequest::Execute(ExecuteRequest {
                    v: PROTOCOL_VERSION,
                    request_id,
                    idempotency_key: idempotency_key.clone(),
                    actor: self.actor.clone(),
                    intent,
                }),
                deadline,
            )
            .await;
        match result {
            Err(ClientError::Timeout { .. }) => {
                Err(self.recover_mutation_timeout(idempotency_key).await)
            }
            other => other,
        }
    }

    async fn recover_mutation_timeout(&self, idempotency_key: String) -> ClientError {
        let lookup_deadline =
            Instant::now() + self.options.request_timeout.min(OPERATION_LOOKUP_GRACE);
        let operation = loop {
            let response = self
                .query_until(
                    Query::OperationByKey {
                        actor_kind: self.actor.kind,
                        actor_id: self.actor.id.clone(),
                        idempotency_key: idempotency_key.clone(),
                    },
                    lookup_deadline,
                )
                .await;
            match response {
                Ok(response) => match operation_record(response) {
                    Ok(record) => break Some(record.id),
                    Err(ClientError::Domain(error))
                        if error.code == "OPERATION_NOT_FOUND"
                            && Instant::now() < lookup_deadline => {}
                    Err(_) => break None,
                },
                Err(ClientError::Timeout { .. }) => break None,
                Err(_) => break None,
            }
            let remaining = lookup_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break None;
            }
            tokio::time::sleep(self.options.operation_poll_interval.min(remaining)).await;
        };
        ClientError::Timeout {
            operation,
            idempotency_key: Some(idempotency_key),
        }
    }

    async fn query_until(&self, query: Query, deadline: Instant) -> ClientResult<WireResponse> {
        let request_id = ulid::Ulid::new().to_string();
        self.request_until(
            WireRequest::Query(QueryRequest {
                v: PROTOCOL_VERSION,
                request_id,
                query,
            }),
            deadline,
        )
        .await
    }

    async fn request_until(
        &self,
        request: WireRequest,
        deadline: Instant,
    ) -> ClientResult<WireResponse> {
        let expected_request_id = wire_request_id(&request).to_owned();
        let future = async {
            let mut stream = UnixStream::connect(&self.socket).await?;
            let mut payload = serde_json::to_vec(&request)?;
            payload.push(b'\n');
            stream.write_all(&payload).await?;
            let mut reader = BufReader::new(stream);
            let frame = read_bounded_frame(&mut reader, self.options.max_response_frame_bytes)
                .await?
                .ok_or(ClientError::ResponseEmpty)?;
            decode_response(&frame, &expected_request_id)
        };
        tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| ClientError::Timeout {
                operation: None,
                idempotency_key: None,
            })?
    }

    pub fn sessions(&self) -> Sessions {
        Sessions {
            client: self.clone(),
        }
    }

    pub fn reviews(&self) -> Reviews {
        Reviews {
            client: self.clone(),
        }
    }

    pub async fn context(&self, selector: WorkspaceSelector) -> ClientResult<WireResponse> {
        self.query(Query::Context { selector }).await
    }

    pub async fn events(&self, after_cursor: i64) -> ClientResult<EventStream> {
        if after_cursor < 0 {
            return Err(ClientError::InvalidCursor);
        }
        let mut stream = tokio::time::timeout(
            self.options.request_timeout,
            UnixStream::connect(&self.socket),
        )
        .await
        .map_err(|_| ClientError::Timeout {
            operation: None,
            idempotency_key: None,
        })??;
        let request_id = ulid::Ulid::new().to_string();
        let request = WireRequest::Subscribe {
            v: PROTOCOL_VERSION,
            request_id: request_id.clone(),
            after_cursor,
        };
        let mut payload = serde_json::to_vec(&request)?;
        payload.push(b'\n');
        tokio::time::timeout(self.options.request_timeout, stream.write_all(&payload))
            .await
            .map_err(|_| ClientError::Timeout {
                operation: None,
                idempotency_key: None,
            })??;
        let (reader, _) = stream.into_split();
        Ok(EventStream {
            reader: BufReader::new(reader),
            max_frame_bytes: self.options.max_response_frame_bytes,
            request_id,
        })
    }

    async fn execute_typed<T: DeserializeOwned>(
        &self,
        intent: Intent,
    ) -> ClientResult<TerminalOutcome<T>> {
        terminal(self.execute_wait(intent).await?)
    }

    async fn heartbeat_lease(
        &self,
        session_id: SessionId,
        lease_id: LeaseId,
    ) -> ClientResult<HeartbeatResult> {
        let outcome = self
            .execute_typed(Intent::LeaseHeartbeat {
                session_id,
                lease_id,
            })
            .await?;
        completed_only(outcome, "lease heartbeat")
    }

    async fn reattach_session(&self, session_id: SessionId) -> ClientResult<OpenedSession> {
        let outcome = self
            .execute_typed::<OpenedSession>(Intent::SessionReattach { session_id })
            .await?;
        completed_only(outcome, "session reattach")
    }

    async fn wake_session(&self, session_id: SessionId) -> ClientResult<OpenedSession> {
        let outcome = self
            .execute_typed::<OpenedSession>(Intent::SessionWake { session_id })
            .await?;
        completed_only(outcome, "session wake")
    }
}

#[derive(Debug)]
pub enum TerminalOutcome<T> {
    Completed(T),
    ReviewRequired(ReviewRequired),
    Conflict(ConflictOutcome),
}

impl<T> TerminalOutcome<T> {
    fn map<U>(self, map: impl FnOnce(T) -> U) -> TerminalOutcome<U> {
        match self {
            Self::Completed(value) => TerminalOutcome::Completed(map(value)),
            Self::ReviewRequired(review) => TerminalOutcome::ReviewRequired(review),
            Self::Conflict(conflict) => TerminalOutcome::Conflict(conflict),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckpointResult {
    pub checkpoint_id: CheckpointId,
    pub head_sha: ObjectId,
    pub index_tree: ObjectId,
    pub working_tree: ObjectId,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublishResult {
    pub branch: String,
    pub commit: ObjectId,
    pub tree: ObjectId,
    pub previous_remote: Option<ObjectId>,
    pub pushed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseResult {
    pub session: SessionId,
    pub workspace: WorkspaceId,
    pub checkpoint_id: Option<CheckpointId>,
    pub released: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatResult {
    pub lease: LeaseId,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SleepResult {
    pub session: SessionId,
    pub workspace: WorkspaceId,
    pub checkpoint_id: CheckpointId,
    pub suspended: bool,
    pub reclaimed_bytes: u64,
}

#[derive(Clone)]
pub struct Sessions {
    client: ShadeClient,
}

impl Sessions {
    pub async fn open(&self, request: OpenSession) -> ClientResult<Session> {
        let outcome = self
            .client
            .execute_typed::<OpenedSession>(Intent::SessionOpen(request))
            .await?;
        let opened = completed_only(outcome, "session open")?;
        Ok(Session::new(self.client.clone(), opened))
    }

    /// Resume a dormant session on the workspace it already owns, with the
    /// same in-process heartbeat `open` starts.
    pub async fn reattach(&self, session_id: SessionId) -> ClientResult<Session> {
        let opened = self.client.reattach_session(session_id).await?;
        Ok(Session::new(self.client.clone(), opened))
    }

    /// Rebuild a suspended session's workspace and take a fresh lease. The
    /// returned handle has a new workspace id and a new cwd, like restore; the
    /// session id is the one that went to sleep. Safe to call on a session
    /// that never slept, which is simply resumed.
    pub async fn wake(&self, session_id: SessionId) -> ClientResult<Session> {
        let opened = self.client.wake_session(session_id).await?;
        Ok(Session::new(self.client.clone(), opened))
    }

    /// The lifecycle of a session, answerable without holding a lease on it.
    pub async fn status(&self, session_id: SessionId) -> ClientResult<SessionStatus> {
        let response = self.client.query(Query::Session { session_id }).await?;
        completed_only(terminal(response)?, "session status")
    }
}

struct SessionLifecycle {
    stop: watch::Sender<bool>,
    /// The lease this session is currently renewing. It is not
    /// `opened.lease`: a reattach rotates the lease under a handle the caller
    /// already holds, and that handle must keep working.
    lease: Mutex<LeaseId>,
}

impl SessionLifecycle {
    fn retire(&self) {
        let _ = self.stop.send(true);
    }

    fn is_active(&self) -> bool {
        !*self.stop.borrow()
    }

    fn lease(&self) -> LeaseId {
        self.lease.lock().expect("lease mutex poisoned").clone()
    }

    fn adopt(&self, lease: LeaseId) {
        *self.lease.lock().expect("lease mutex poisoned") = lease;
    }
}

/// A live session handle. Opening it starts a 30-second heartbeat by default.
/// Clones share one heartbeat; dropping the final handle stops it automatically.
#[derive(Clone)]
pub struct Session {
    client: ShadeClient,
    lifecycle: Arc<SessionLifecycle>,
    pub session_id: SessionId,
    pub selector: WorkspaceSelector,
    pub opened: OpenedSession,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Session")
            .field("session_id", &self.session_id)
            .field("workspace", &self.opened.workspace)
            .field("lease", &self.opened.lease)
            .field("active", &self.lifecycle.is_active())
            .finish_non_exhaustive()
    }
}

impl Session {
    fn new(client: ShadeClient, opened: OpenedSession) -> Self {
        let (stop, stop_receiver) = watch::channel(false);
        let session = Self {
            client: client.clone(),
            lifecycle: Arc::new(SessionLifecycle {
                stop,
                lease: Mutex::new(opened.lease.clone()),
            }),
            session_id: opened.session.clone(),
            selector: WorkspaceSelector {
                workspace_id: Some(opened.workspace.clone()),
                cwd: Some(opened.cwd.clone()),
            },
            opened,
        };
        session.spawn_heartbeat(client, stop_receiver);
        session
    }

    fn spawn_heartbeat(&self, client: ShadeClient, mut stop: watch::Receiver<bool>) {
        let session_id = self.session_id.clone();
        let mut lease_id = self.opened.lease.clone();
        let lifecycle = Arc::downgrade(&self.lifecycle);
        let heartbeat_interval = client.options.heartbeat_interval;
        let reattach_on_expiry = client.options.reattach_on_expiry;
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval_at(Instant::now() + heartbeat_interval, heartbeat_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            break;
                        }
                        continue;
                    }
                }
                match client
                    .heartbeat_lease(session_id.clone(), lease_id.clone())
                    .await
                {
                    Ok(_) => {}
                    // An expiry means the workspace went dormant, which is
                    // recoverable; every other terminal lease error means this
                    // handle is genuinely finished. One attempt, never a retry
                    // loop: if the reattach itself fails the session retires.
                    Err(ClientError::Domain(error))
                        if reattach_on_expiry && error.code == "LEASE_EXPIRED" =>
                    {
                        match client.reattach_session(session_id.clone()).await {
                            Ok(opened) => {
                                lease_id = opened.lease.clone();
                                if let Some(lifecycle) = lifecycle.upgrade() {
                                    lifecycle.adopt(opened.lease);
                                } else {
                                    break;
                                }
                            }
                            Err(_) => {
                                if let Some(lifecycle) = lifecycle.upgrade() {
                                    lifecycle.retire();
                                }
                                break;
                            }
                        }
                    }
                    Err(ClientError::Domain(error)) if terminal_lease_error(&error.code) => {
                        if let Some(lifecycle) = lifecycle.upgrade() {
                            lifecycle.retire();
                        }
                        break;
                    }
                    Err(_) => {}
                }
            }
        });
    }

    pub fn is_active(&self) -> bool {
        self.lifecycle.is_active()
    }

    /// The lease currently held. `opened.lease` records the lease this handle
    /// was created with; a reattach replaces it, and this follows.
    pub fn lease(&self) -> LeaseId {
        self.lifecycle.lease()
    }

    /// Stop automatic heartbeats for this handle and every clone.
    pub fn retire(&self) {
        self.lifecycle.retire();
    }

    pub async fn context(&self) -> ClientResult<CompactContext> {
        self.require_active()?;
        let response = self.client.context(self.selector.clone()).await?;
        completed_only(terminal(response)?, "context")
    }

    pub async fn checkpoint(
        &self,
        reason: impl Into<String>,
    ) -> ClientResult<TerminalOutcome<CheckpointResult>> {
        self.require_active()?;
        self.client
            .execute_typed(Intent::WorkspaceCheckpoint {
                selector: self.selector.clone(),
                reason: reason.into(),
            })
            .await
    }

    pub async fn fork(
        &self,
        child_session_id: SessionId,
        intent: Option<String>,
    ) -> ClientResult<TerminalOutcome<Session>> {
        self.require_active()?;
        let outcome = self
            .client
            .execute_typed::<OpenedSession>(Intent::WorkspaceFork {
                selector: self.selector.clone(),
                child_session_id,
                intent,
            })
            .await?;
        Ok(outcome.map(|opened| Session::new(self.client.clone(), opened)))
    }

    pub async fn sync(&self) -> ClientResult<TerminalOutcome<Session>> {
        self.successor(Intent::WorkspaceSync {
            selector: self.selector.clone(),
        })
        .await
    }

    pub async fn restore(
        &self,
        checkpoint_id: CheckpointId,
    ) -> ClientResult<TerminalOutcome<Session>> {
        self.successor(Intent::WorkspaceRestore {
            selector: self.selector.clone(),
            checkpoint_id,
        })
        .await
    }

    pub async fn refresh_dependencies(&self) -> ClientResult<TerminalOutcome<Session>> {
        self.successor(Intent::DependenciesRefresh {
            selector: self.selector.clone(),
        })
        .await
    }

    pub async fn dependency_scripts(
        &self,
    ) -> ClientResult<shade_protocol::DependencyScriptsResult> {
        self.require_active()?;
        completed_only(
            terminal(
                self.client
                    .query(Query::DependencyScripts {
                        selector: self.selector.clone(),
                    })
                    .await?,
            )?,
            "dependency scripts",
        )
    }

    /// Call only after the user explicitly approves this exact locked tuple.
    /// Refresh creates a successor using the new policy; this view is preserved.
    pub async fn approve_script(
        &self,
        approval: shade_protocol::ScriptApproval,
    ) -> ClientResult<TerminalOutcome<shade_protocol::ScriptDecisionResult>> {
        self.require_active()?;
        self.client
            .execute_typed(Intent::DependencyScriptDecision {
                selector: self.selector.clone(),
                approval,
                allow: true,
            })
            .await
    }

    pub async fn revoke_script(
        &self,
        approval: shade_protocol::ScriptApproval,
    ) -> ClientResult<TerminalOutcome<shade_protocol::ScriptDecisionResult>> {
        self.require_active()?;
        self.client
            .execute_typed(Intent::DependencyScriptDecision {
                selector: self.selector.clone(),
                approval,
                allow: false,
            })
            .await
    }

    pub async fn publish(
        &self,
        branch: impl Into<String>,
        message: impl Into<String>,
        push: bool,
    ) -> ClientResult<TerminalOutcome<PublishResult>> {
        self.require_active()?;
        self.client
            .execute_typed(Intent::WorkspacePublish {
                selector: self.selector.clone(),
                branch: branch.into(),
                message: message.into(),
                push,
            })
            .await
    }

    /// Finish a `conflict` outcome. Fix the conflict inside the resolution
    /// workspace named by `conflict` first: the daemon publishes from that
    /// workspace and hands this session a successor, exactly as `sync` does.
    pub async fn resolve(
        &self,
        conflict: &ConflictOutcome,
    ) -> ClientResult<TerminalOutcome<Session>> {
        self.successor(Intent::ResolutionComplete {
            selector: WorkspaceSelector {
                workspace_id: Some(conflict.workspace.clone()),
                cwd: None,
            },
        })
        .await
    }

    pub async fn release(&self) -> ClientResult<TerminalOutcome<ReleaseResult>> {
        self.require_active()?;
        let outcome = self
            .client
            .execute_typed(Intent::WorkspaceRelease {
                selector: self.selector.clone(),
            })
            .await?;
        if matches!(outcome, TerminalOutcome::Completed(_)) {
            self.retire();
        }
        Ok(outcome)
    }

    /// Free the workspace's disk and keep everything in it. The handle
    /// retires, exactly as it does on `release`: there is no lease left to
    /// heartbeat. `sessions.wake(session_id)` brings the work back.
    pub async fn sleep(&self) -> ClientResult<SleepResult> {
        self.require_active()?;
        let outcome = self
            .client
            .execute_typed(Intent::WorkspaceSleep {
                selector: self.selector.clone(),
            })
            .await?;
        let result = completed_only(outcome, "workspace sleep")?;
        self.retire();
        Ok(result)
    }

    pub async fn heartbeat(&self) -> ClientResult<HeartbeatResult> {
        self.require_active()?;
        self.client
            .heartbeat_lease(self.session_id.clone(), self.lease())
            .await
    }

    async fn successor(&self, intent: Intent) -> ClientResult<TerminalOutcome<Session>> {
        self.require_active()?;
        let outcome = self.client.execute_typed::<OpenedSession>(intent).await?;
        Ok(match outcome {
            TerminalOutcome::Completed(opened) => {
                let successor = Session::new(self.client.clone(), opened);
                self.retire();
                TerminalOutcome::Completed(successor)
            }
            TerminalOutcome::ReviewRequired(review) => TerminalOutcome::ReviewRequired(review),
            TerminalOutcome::Conflict(conflict) => TerminalOutcome::Conflict(conflict),
        })
    }

    fn require_active(&self) -> ClientResult<()> {
        if self.is_active() {
            Ok(())
        } else {
            Err(ClientError::UnexpectedOutcome("session handle is retired"))
        }
    }
}

#[derive(Debug)]
pub enum ReviewResolution {
    Successor(Box<Session>),
    Discarded {
        review: ReviewId,
        released: WorkspaceId,
    },
    Kept {
        review: ReviewId,
        workspace: WorkspaceId,
    },
}

#[derive(Clone)]
pub struct Reviews {
    client: ShadeClient,
}

impl Reviews {
    pub async fn resolve(
        &self,
        review_id: ReviewId,
        action: ReviewAction,
    ) -> ClientResult<TerminalOutcome<ReviewResolution>> {
        let response = self
            .client
            .execute_wait(Intent::ReviewResolve { review_id, action })
            .await?;
        review_terminal(response, &self.client)
    }
}

impl ShadeClient {
    pub async fn resolve_review(
        &self,
        review_id: ReviewId,
        action: ReviewAction,
    ) -> ClientResult<TerminalOutcome<ReviewResolution>> {
        self.reviews().resolve(review_id, action).await
    }
}

pub struct EventStream {
    reader: BufReader<OwnedReadHalf>,
    max_frame_bytes: usize,
    request_id: String,
}

impl EventStream {
    pub async fn next(&mut self) -> ClientResult<Option<shade_protocol::EventEnvelope>> {
        let Some(frame) = read_bounded_frame(&mut self.reader, self.max_frame_bytes).await? else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_slice(&frame)?;
        if value.get("status").and_then(serde_json::Value::as_str) == Some("error") {
            let response = decode_response(&frame, &self.request_id)?;
            return match response.body {
                ResponseBody::Error { error } => Err(ClientError::Domain(error)),
                ResponseBody::Ok { .. } => Err(ClientError::InvalidOperationRecord),
            };
        }
        let event: shade_protocol::EventEnvelope = serde_json::from_value(value)?;
        if event.v != PROTOCOL_VERSION {
            return Err(ClientError::ProtocolVersion {
                expected: PROTOCOL_VERSION,
                received: event.v,
            });
        }
        Ok(Some(event))
    }
}

#[derive(Deserialize)]
struct OperationRecordWire {
    id: OperationId,
    state: String,
    outcome: Option<Outcome>,
    error: Option<ShadeError>,
}

#[derive(Deserialize)]
#[serde(tag = "resolution", rename_all = "snake_case")]
enum ReviewResolutionWire {
    Discarded {
        review: ReviewId,
        released: WorkspaceId,
    },
    Kept {
        review: ReviewId,
        workspace: WorkspaceId,
    },
}

fn operation_record(response: WireResponse) -> ClientResult<OperationRecordWire> {
    match response.body {
        ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        } => Ok(serde_json::from_value(value)?),
        ResponseBody::Error { error } => Err(ClientError::Domain(error)),
        ResponseBody::Ok { .. } => Err(ClientError::InvalidOperationRecord),
    }
}

fn pending_handoff(response: &WireResponse) -> ClientResult<Option<PendingHandoff>> {
    let ResponseBody::Ok {
        outcome: Outcome::Completed(value),
    } = &response.body
    else {
        return Ok(None);
    };
    if value.get("handoff_id").is_none()
        || value.get("session").is_none()
        || value.get("predecessor").is_none()
        || value.get("successor").is_none()
    {
        return Ok(None);
    }
    Ok(Some(serde_json::from_value(value.clone())?))
}

fn terminal<T: DeserializeOwned>(response: WireResponse) -> ClientResult<TerminalOutcome<T>> {
    match response.body {
        ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        } => Ok(TerminalOutcome::Completed(serde_json::from_value(value)?)),
        ResponseBody::Ok {
            outcome: Outcome::ReviewRequired(review),
        } => Ok(TerminalOutcome::ReviewRequired(review)),
        ResponseBody::Ok {
            outcome: Outcome::Conflict(conflict),
        } => Ok(TerminalOutcome::Conflict(conflict)),
        ResponseBody::Ok {
            outcome: Outcome::Accepted { .. },
        } => Err(ClientError::UnexpectedOutcome(
            "accepted operation was not settled",
        )),
        ResponseBody::Error { error } => Err(ClientError::Domain(error)),
    }
}

fn review_terminal(
    response: WireResponse,
    client: &ShadeClient,
) -> ClientResult<TerminalOutcome<ReviewResolution>> {
    match response.body {
        ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        } => {
            if value.get("compact_context").is_some() {
                let opened: OpenedSession = serde_json::from_value(value)?;
                return Ok(TerminalOutcome::Completed(ReviewResolution::Successor(
                    Box::new(Session::new(client.clone(), opened)),
                )));
            }
            let resolution: ReviewResolutionWire = serde_json::from_value(value)?;
            Ok(TerminalOutcome::Completed(match resolution {
                ReviewResolutionWire::Discarded { review, released } => {
                    ReviewResolution::Discarded { review, released }
                }
                ReviewResolutionWire::Kept { review, workspace } => {
                    ReviewResolution::Kept { review, workspace }
                }
            }))
        }
        ResponseBody::Ok {
            outcome: Outcome::ReviewRequired(review),
        } => Ok(TerminalOutcome::ReviewRequired(review)),
        ResponseBody::Ok {
            outcome: Outcome::Conflict(conflict),
        } => Ok(TerminalOutcome::Conflict(conflict)),
        ResponseBody::Ok {
            outcome: Outcome::Accepted { .. },
        } => Err(ClientError::UnexpectedOutcome(
            "accepted review operation was not settled",
        )),
        ResponseBody::Error { error } => Err(ClientError::Domain(error)),
    }
}

fn completed_only<T>(outcome: TerminalOutcome<T>, operation: &'static str) -> ClientResult<T> {
    match outcome {
        TerminalOutcome::Completed(value) => Ok(value),
        TerminalOutcome::ReviewRequired(_) | TerminalOutcome::Conflict(_) => {
            Err(ClientError::UnexpectedOutcome(operation))
        }
    }
}

fn terminal_lease_error(code: &str) -> bool {
    matches!(
        code,
        "LEASE_EXPIRED" | "LEASE_FENCED" | "WORKSPACE_RELEASED" | "SESSION_ALREADY_RELEASED"
    )
}

fn wire_request_id(request: &WireRequest) -> &str {
    match request {
        WireRequest::Execute(request) => &request.request_id,
        WireRequest::Query(request) => &request.request_id,
        WireRequest::Subscribe { request_id, .. } => request_id,
    }
}

fn decode_response(frame: &[u8], expected_request_id: &str) -> ClientResult<WireResponse> {
    let response: WireResponse = serde_json::from_slice(frame)?;
    if response.v != PROTOCOL_VERSION {
        return Err(ClientError::ProtocolVersion {
            expected: PROTOCOL_VERSION,
            received: response.v,
        });
    }
    if response.request_id != expected_request_id {
        return Err(ClientError::RequestMismatch);
    }
    Ok(response)
}

async fn read_bounded_frame<R>(reader: &mut R, max_bytes: usize) -> ClientResult<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    loop {
        let (available_len, newline) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return if frame.is_empty() {
                    Ok(None)
                } else {
                    Err(ClientError::ResponseIncomplete)
                };
            }
            (
                available.len(),
                available.iter().position(|byte| *byte == b'\n'),
            )
        };
        let payload_len = newline.unwrap_or(available_len);
        if frame.len().saturating_add(payload_len) > max_bytes {
            return Err(ClientError::ResponseTooLarge);
        }
        {
            let available = reader.fill_buf().await?;
            frame.extend_from_slice(&available[..payload_len]);
        }
        reader.consume(newline.map_or(available_len, |index| index + 1));
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

pub use shade_protocol;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::UnixListener;

    fn response(request_id: &str, outcome: Outcome) -> WireResponse {
        WireResponse {
            v: PROTOCOL_VERSION,
            request_id: request_id.into(),
            body: ResponseBody::Ok { outcome },
        }
    }

    fn opened(lease: &str) -> OpenedSession {
        serde_json::from_value(json!({
            "session": "session-test",
            "workspace": "workspace-test",
            "lease": lease,
            "cwd": "/tmp/workspace-test",
            "env": {
                "SHADE_SESSION": "session-test",
                "SHADE_WORKSPACE": "workspace-test",
                "SHADE_LEASE": lease,
                "SHADE_SOCKET": "/tmp/shade.sock"
            },
            "compact_context": {
                "workspace": "workspace-test",
                "session": "session-test",
                "base_ref": "origin/main",
                "base_sha": "opaque-base",
                "head_sha": "opaque-head",
                "changes": {"staged": 0, "unstaged": 0, "untracked": 0},
                "lease": "live",
                "dependencies": {"state": "ready"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn response_validation_rejects_version_and_request_mismatch() {
        let mut wrong_version = response("expected", Outcome::Completed(json!({})));
        wrong_version.v = PROTOCOL_VERSION + 1;
        let encoded = serde_json::to_vec(&wrong_version).unwrap();
        assert!(matches!(
            decode_response(&encoded, "expected"),
            Err(ClientError::ProtocolVersion { .. })
        ));

        let encoded =
            serde_json::to_vec(&response("different", Outcome::Completed(json!({})))).unwrap();
        assert!(matches!(
            decode_response(&encoded, "expected"),
            Err(ClientError::RequestMismatch)
        ));
    }

    #[test]
    fn cli_actor_identity_is_stable_across_client_instances() {
        let first = ShadeClient::cli("/tmp/shade-one.sock");
        let second = ShadeClient::cli("/tmp/shade-two.sock");
        assert_eq!(first.actor.id, "cli");
        assert_eq!(first.actor.id, second.actor.id);
    }

    #[tokio::test]
    async fn response_frames_are_bounded_and_require_newline() {
        let oversized = b"12345\n";
        let mut reader = BufReader::new(&oversized[..]);
        assert!(matches!(
            read_bounded_frame(&mut reader, 4).await,
            Err(ClientError::ResponseTooLarge)
        ));

        let incomplete = b"1234";
        let mut reader = BufReader::new(&incomplete[..]);
        assert!(matches!(
            read_bounded_frame(&mut reader, 4).await,
            Err(ClientError::ResponseIncomplete)
        ));
    }

    #[tokio::test]
    async fn polling_deadline_retains_the_durable_operation_id() {
        let Some((temp, listener)) = test_listener() else {
            return;
        };
        let socket = temp.path().join("shade.sock");
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);
                    let mut frame = Vec::new();
                    if reader.read_until(b'\n', &mut frame).await.is_err() {
                        return;
                    }
                    let Some(newline) = frame.iter().position(|byte| *byte == b'\n') else {
                        return;
                    };
                    let request: WireRequest = serde_json::from_slice(&frame[..newline]).unwrap();
                    let request_id = wire_request_id(&request).to_owned();
                    let outcome = match request {
                        WireRequest::Execute(_) => Outcome::Accepted {
                            operation_id: OperationId("operation-durable".into()),
                        },
                        WireRequest::Query(_) => Outcome::Completed(json!({
                            "id": "operation-durable",
                            "state": "running",
                            "outcome": null,
                            "error": null
                        })),
                        WireRequest::Subscribe { .. } => return,
                    };
                    let mut encoded = serde_json::to_vec(&response(&request_id, outcome)).unwrap();
                    encoded.push(b'\n');
                    let _ = writer.write_all(&encoded).await;
                });
            }
        });
        let client = ShadeClient::with_options(
            &socket,
            Actor {
                kind: ActorKind::Agent,
                id: "deadline-test".into(),
            },
            ShadeClientOptions {
                request_timeout: Duration::from_millis(100),
                operation_timeout: Duration::from_millis(40),
                operation_poll_interval: Duration::from_millis(5),
                ..ShadeClientOptions::default()
            },
        )
        .unwrap();
        let error = client
            .execute_wait(Intent::GarbageCollect)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Timeout {
                operation: Some(OperationId(ref value)),
                idempotency_key: Some(ref key),
            } if value == "operation-durable" && !key.is_empty()
        ));
        server.abort();
    }

    #[tokio::test]
    async fn initial_response_timeout_recovers_operation_id_and_generated_retry_key() {
        let Some((temp, listener)) = test_listener() else {
            return;
        };
        let socket = temp.path().join("shade.sock");
        let lookup_attempts = Arc::new(AtomicUsize::new(0));
        let observed_lookups = lookup_attempts.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let observed_lookups = observed_lookups.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);
                    let mut frame = Vec::new();
                    if reader.read_until(b'\n', &mut frame).await.is_err() {
                        return;
                    }
                    let Some(newline) = frame.iter().position(|byte| *byte == b'\n') else {
                        return;
                    };
                    let request: WireRequest = serde_json::from_slice(&frame[..newline]).unwrap();
                    let request_id = wire_request_id(&request).to_owned();
                    match request {
                        WireRequest::Execute(_) => {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                        WireRequest::Query(QueryRequest {
                            query: Query::OperationByKey { .. },
                            ..
                        }) => {
                            let response = if observed_lookups.fetch_add(1, Ordering::SeqCst) == 0 {
                                WireResponse {
                                    v: PROTOCOL_VERSION,
                                    request_id,
                                    body: ResponseBody::Error {
                                        error: ShadeError {
                                            code: "OPERATION_NOT_FOUND".into(),
                                            retry: "never".into(),
                                            operation: None,
                                            next: None,
                                            diagnostics_id: None,
                                        },
                                    },
                                }
                            } else {
                                response(
                                    &request_id,
                                    Outcome::Completed(json!({
                                        "id": "operation-initial-timeout",
                                        "state": "running",
                                        "outcome": null,
                                        "error": null
                                    })),
                                )
                            };
                            let mut encoded = serde_json::to_vec(&response).unwrap();
                            encoded.push(b'\n');
                            let _ = writer.write_all(&encoded).await;
                        }
                        _ => {}
                    }
                });
            }
        });
        let client = ShadeClient::with_options(
            &socket,
            Actor {
                kind: ActorKind::Agent,
                id: "initial-timeout-test".into(),
            },
            ShadeClientOptions {
                request_timeout: Duration::from_millis(100),
                operation_timeout: Duration::from_millis(20),
                operation_poll_interval: Duration::from_millis(5),
                ..ShadeClientOptions::default()
            },
        )
        .unwrap();
        let error = client
            .execute_wait_idempotent_for(
                Intent::GarbageCollect,
                "durable:initial-timeout",
                Duration::from_millis(20),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            ClientError::Timeout {
                operation: Some(OperationId(value)),
                idempotency_key: Some(key),
            } if value == "operation-initial-timeout" && key == "durable:initial-timeout"
        ));
        assert_eq!(error.idempotency_key(), Some("durable:initial-timeout"));
        assert_eq!(lookup_attempts.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn pending_handoff_is_adopted_with_a_stable_key_for_waiting_and_raw_callers() {
        let Some((temp, listener)) = test_listener() else {
            return;
        };
        let socket = temp.path().join("shade.sock");
        let adoption_keys = Arc::new(Mutex::new(Vec::new()));
        let observed_keys = adoption_keys.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let observed_keys = observed_keys.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);
                    let mut frame = Vec::new();
                    if reader.read_until(b'\n', &mut frame).await.is_err() {
                        return;
                    }
                    let Some(newline) = frame.iter().position(|byte| *byte == b'\n') else {
                        return;
                    };
                    let request: WireRequest = serde_json::from_slice(&frame[..newline]).unwrap();
                    let request_id = wire_request_id(&request).to_owned();
                    let outcome = match request {
                        WireRequest::Execute(ExecuteRequest {
                            intent: Intent::WorkspaceSync { .. },
                            ..
                        }) => Outcome::Completed(
                            serde_json::to_value(PendingHandoff {
                                handoff_id: shade_protocol::HandoffId("handoff-test".into()),
                                session: SessionId("session-test".into()),
                                predecessor: WorkspaceId("workspace-old".into()),
                                successor: WorkspaceId("workspace-new".into()),
                            })
                            .unwrap(),
                        ),
                        WireRequest::Execute(ExecuteRequest {
                            idempotency_key,
                            intent: Intent::SuccessorAdopt { handoff_id },
                            ..
                        }) => {
                            assert_eq!(handoff_id.0, "handoff-test");
                            observed_keys.lock().unwrap().push(idempotency_key);
                            Outcome::Completed(serde_json::to_value(opened("lease-new")).unwrap())
                        }
                        _ => return,
                    };
                    let mut encoded = serde_json::to_vec(&response(&request_id, outcome)).unwrap();
                    encoded.push(b'\n');
                    let _ = writer.write_all(&encoded).await;
                });
            }
        });
        let client = ShadeClient::with_options(
            &socket,
            Actor {
                kind: ActorKind::Agent,
                id: "handoff-test".into(),
            },
            ShadeClientOptions {
                request_timeout: Duration::from_millis(100),
                operation_timeout: Duration::from_millis(100),
                heartbeat_interval: Duration::from_secs(1),
                ..ShadeClientOptions::default()
            },
        )
        .unwrap();
        let intent = || Intent::WorkspaceSync {
            selector: WorkspaceSelector {
                workspace_id: Some(WorkspaceId("workspace-old".into())),
                cwd: None,
            },
        };
        let response = client
            .execute_idempotent(intent(), "sync:raw")
            .await
            .unwrap();
        let adopted: OpenedSession =
            completed_only(terminal(response).unwrap(), "raw adoption").unwrap();
        assert_eq!(adopted.lease.0, "lease-new");

        let response = client.execute_wait(intent()).await.unwrap();
        let adopted: OpenedSession =
            completed_only(terminal(response).unwrap(), "adoption").unwrap();
        assert_eq!(adopted.lease.0, "lease-new");
        assert_eq!(
            adoption_keys.lock().unwrap().as_slice(),
            ["handoff:handoff-test", "handoff:handoff-test"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn session_heartbeat_is_automatic_and_raii_scoped() {
        let Some((temp, listener)) = test_listener() else {
            return;
        };
        let socket = temp.path().join("shade.sock");
        let heartbeats = Arc::new(AtomicUsize::new(0));
        let observed = heartbeats.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let observed = observed.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);
                    let mut frame = Vec::new();
                    if reader.read_until(b'\n', &mut frame).await.is_err() {
                        return;
                    }
                    let Some(newline) = frame.iter().position(|byte| *byte == b'\n') else {
                        return;
                    };
                    let request: WireRequest = serde_json::from_slice(&frame[..newline]).unwrap();
                    let request_id = wire_request_id(&request).to_owned();
                    if matches!(
                        request,
                        WireRequest::Execute(ExecuteRequest {
                            intent: Intent::LeaseHeartbeat { .. },
                            ..
                        })
                    ) {
                        observed.fetch_add(1, Ordering::SeqCst);
                    }
                    let outcome = Outcome::Completed(json!({
                        "lease": "lease-test",
                        "expires_at_ms": 1234
                    }));
                    let mut encoded = serde_json::to_vec(&response(&request_id, outcome)).unwrap();
                    encoded.push(b'\n');
                    let _ = writer.write_all(&encoded).await;
                });
            }
        });
        let client = ShadeClient::with_options(
            &socket,
            Actor {
                kind: ActorKind::Agent,
                id: "heartbeat-test".into(),
            },
            ShadeClientOptions {
                request_timeout: Duration::from_millis(100),
                operation_timeout: Duration::from_millis(100),
                heartbeat_interval: Duration::from_millis(10),
                ..ShadeClientOptions::default()
            },
        )
        .unwrap();
        let session = Session::new(client, opened("lease-test"));
        tokio::time::sleep(Duration::from_millis(45)).await;
        assert!(heartbeats.load(Ordering::SeqCst) >= 2);
        drop(session);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let after_drop = heartbeats.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(35)).await;
        assert_eq!(heartbeats.load(Ordering::SeqCst), after_drop);
        server.abort();
    }

    /// An expired lease is a dormancy, not an ending: the handle must survive
    /// it, adopt the new lease and keep heartbeating.
    #[tokio::test]
    async fn an_expired_lease_is_reattached_once_and_the_handle_keeps_its_new_lease() {
        let Some((temp, listener)) = test_listener() else {
            return;
        };
        let socket = temp.path().join("shade.sock");
        let leases = Arc::new(Mutex::new(Vec::<String>::new()));
        let reattachments = Arc::new(AtomicUsize::new(0));
        let observed = leases.clone();
        let counted = reattachments.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let observed = observed.clone();
                let counted = counted.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);
                    let mut frame = Vec::new();
                    if reader.read_until(b'\n', &mut frame).await.is_err() {
                        return;
                    }
                    let Some(newline) = frame.iter().position(|byte| *byte == b'\n') else {
                        return;
                    };
                    let request: WireRequest = serde_json::from_slice(&frame[..newline]).unwrap();
                    let request_id = wire_request_id(&request).to_owned();
                    let body = match request {
                        WireRequest::Execute(ExecuteRequest {
                            intent: Intent::LeaseHeartbeat { lease_id, .. },
                            ..
                        }) => {
                            observed.lock().unwrap().push(lease_id.0.clone());
                            if lease_id.0 == "lease-first" {
                                ResponseBody::Error {
                                    error: ShadeError {
                                        code: "LEASE_EXPIRED".into(),
                                        retry: "never".into(),
                                        operation: None,
                                        next: None,
                                        diagnostics_id: None,
                                    },
                                }
                            } else {
                                ResponseBody::Ok {
                                    outcome: Outcome::Completed(
                                        json!({"lease": lease_id, "expires_at_ms": 1}),
                                    ),
                                }
                            }
                        }
                        WireRequest::Execute(ExecuteRequest {
                            intent: Intent::SessionReattach { session_id },
                            ..
                        }) => {
                            assert_eq!(session_id.0, "session-test");
                            counted.fetch_add(1, Ordering::SeqCst);
                            ResponseBody::Ok {
                                outcome: Outcome::Completed(
                                    serde_json::to_value(opened("lease-second")).unwrap(),
                                ),
                            }
                        }
                        _ => return,
                    };
                    let mut encoded = serde_json::to_vec(&WireResponse {
                        v: PROTOCOL_VERSION,
                        request_id,
                        body,
                    })
                    .unwrap();
                    encoded.push(b'\n');
                    let _ = writer.write_all(&encoded).await;
                });
            }
        });
        let client = ShadeClient::with_options(
            &socket,
            Actor {
                kind: ActorKind::Agent,
                id: "reattach-test".into(),
            },
            ShadeClientOptions {
                request_timeout: Duration::from_millis(200),
                operation_timeout: Duration::from_millis(200),
                heartbeat_interval: Duration::from_millis(10),
                ..ShadeClientOptions::default()
            },
        )
        .unwrap();
        let session = Session::new(client, opened("lease-first"));
        for _ in 0..200 {
            if session.lease().0 == "lease-second" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(session.is_active(), "a dormancy must not retire the handle");
        assert_eq!(session.lease().0, "lease-second");
        assert_eq!(
            session.opened.lease.0, "lease-first",
            "`opened` still records the lease the handle was created with"
        );
        assert_eq!(reattachments.load(Ordering::SeqCst), 1);
        let renewed = leases
            .lock()
            .unwrap()
            .iter()
            .filter(|lease| *lease == "lease-second")
            .count();
        assert!(renewed >= 1, "the new lease must be the one being renewed");
        session.retire();
        server.abort();
    }

    #[tokio::test]
    async fn reattach_can_be_turned_off_and_the_handle_retires_on_expiry() {
        let Some((temp, listener)) = test_listener() else {
            return;
        };
        let socket = temp.path().join("shade.sock");
        let reattachments = Arc::new(AtomicUsize::new(0));
        let counted = reattachments.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let counted = counted.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);
                    let mut frame = Vec::new();
                    if reader.read_until(b'\n', &mut frame).await.is_err() {
                        return;
                    }
                    let Some(newline) = frame.iter().position(|byte| *byte == b'\n') else {
                        return;
                    };
                    let request: WireRequest = serde_json::from_slice(&frame[..newline]).unwrap();
                    let request_id = wire_request_id(&request).to_owned();
                    if let WireRequest::Execute(ExecuteRequest {
                        intent: Intent::SessionReattach { .. },
                        ..
                    }) = request
                    {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                    let mut encoded = serde_json::to_vec(&WireResponse {
                        v: PROTOCOL_VERSION,
                        request_id,
                        body: ResponseBody::Error {
                            error: ShadeError {
                                code: "LEASE_EXPIRED".into(),
                                retry: "never".into(),
                                operation: None,
                                next: None,
                                diagnostics_id: None,
                            },
                        },
                    })
                    .unwrap();
                    encoded.push(b'\n');
                    let _ = writer.write_all(&encoded).await;
                });
            }
        });
        let client = ShadeClient::with_options(
            &socket,
            Actor {
                kind: ActorKind::Agent,
                id: "no-reattach-test".into(),
            },
            ShadeClientOptions {
                request_timeout: Duration::from_millis(200),
                operation_timeout: Duration::from_millis(200),
                heartbeat_interval: Duration::from_millis(10),
                reattach_on_expiry: false,
                ..ShadeClientOptions::default()
            },
        )
        .unwrap();
        let session = Session::new(client, opened("lease-first"));
        for _ in 0..200 {
            if !session.is_active() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!session.is_active());
        assert_eq!(reattachments.load(Ordering::SeqCst), 0);
        server.abort();
    }

    fn test_listener() -> Option<(tempfile::TempDir, UnixListener)> {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("shade.sock");
        match std::os::unix::net::UnixListener::bind(&socket) {
            Ok(listener) => {
                listener.set_nonblocking(true).unwrap();
                Some((temp, UnixListener::from_std(listener).unwrap()))
            }
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => None,
            Err(error) => panic!("cannot bind test socket: {error}"),
        }
    }
}
