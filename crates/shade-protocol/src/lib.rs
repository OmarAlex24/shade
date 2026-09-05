use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: u16 = 1;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(RepositoryId);
id_type!(SessionId);
id_type!(WorkspaceId);
id_type!(LeaseId);
id_type!(CheckpointId);
id_type!(OperationId);
id_type!(ReviewId);
id_type!(DependencyLayerId);
id_type!(ObjectId);
id_type!(HandoffId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Host,
    Agent,
    Cli,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepositoryLocator {
    Local { path: String },
    Remote { url: String },
    Registered { repository_id: RepositoryId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenSession {
    pub session_id: SessionId,
    pub repository: RepositoryLocator,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub intent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSelector {
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScriptApproval {
    pub provider: String,
    pub package: String,
    pub version: String,
    pub integrity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyScript {
    pub approval: ScriptApproval,
    pub events: Vec<String>,
    pub executed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyScriptStatus {
    pub script: DependencyScript,
    pub allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyScriptsResult {
    pub scripts: Vec<DependencyScriptStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptDecisionResult {
    pub allowed: bool,
    pub refresh_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Intent {
    SessionOpen(OpenSession),
    RepositoryWarm {
        repository: RepositoryLocator,
    },
    LeaseHeartbeat {
        session_id: SessionId,
        lease_id: LeaseId,
    },
    WorkspaceCheckpoint {
        selector: WorkspaceSelector,
        reason: String,
    },
    WorkspaceFork {
        selector: WorkspaceSelector,
        child_session_id: SessionId,
        #[serde(default)]
        intent: Option<String>,
    },
    WorkspaceSync {
        selector: WorkspaceSelector,
    },
    WorkspaceRestore {
        selector: WorkspaceSelector,
        checkpoint_id: CheckpointId,
    },
    DependenciesRefresh {
        selector: WorkspaceSelector,
    },
    DependencyScriptDecision {
        selector: WorkspaceSelector,
        approval: ScriptApproval,
        allow: bool,
    },
    WorkspacePublish {
        selector: WorkspaceSelector,
        branch: String,
        message: String,
        #[serde(default)]
        push: bool,
    },
    ResolutionComplete {
        selector: WorkspaceSelector,
    },
    WorkspaceRelease {
        selector: WorkspaceSelector,
    },
    ReviewResolve {
        review_id: ReviewId,
        action: ReviewAction,
    },
    SuccessorAdopt {
        handoff_id: HandoffId,
    },
    GarbageCollect,
    MaintenanceSweep,
    Reconcile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAction {
    MergeParent,
    Keep,
    Discard,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Query {
    DependencyScripts {
        selector: WorkspaceSelector,
    },
    Context {
        selector: WorkspaceSelector,
    },
    Operation {
        operation_id: OperationId,
    },
    OperationByKey {
        actor_kind: ActorKind,
        actor_id: String,
        idempotency_key: String,
    },
    Events {
        after_cursor: i64,
        limit: u32,
    },
    Doctor,
    Diagnostics {
        diagnostics_id: String,
    },
}

/// Retrieved explicitly; detailed errors never inflate ordinary responses or events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub id: String,
    pub origin: DiagnosticOrigin,
    pub operation: Option<OperationId>,
    pub code: String,
    pub message: String,
    pub redacted: bool,
    pub truncated: bool,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticOrigin {
    Daemon,
    Cli,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub v: u16,
    pub request_id: String,
    pub idempotency_key: String,
    pub actor: Actor,
    pub intent: Intent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub v: u16,
    pub request_id: String,
    pub query: Query,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireRequest {
    Execute(ExecuteRequest),
    Query(QueryRequest),
    Subscribe {
        v: u16,
        request_id: String,
        after_cursor: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactChanges {
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyContext {
    pub state: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_builds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactContext {
    pub workspace: WorkspaceId,
    pub session: SessionId,
    pub base_ref: String,
    pub base_sha: ObjectId,
    pub head_sha: ObjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_sha: Option<ObjectId>,
    pub changes: CompactChanges,
    pub lease: String,
    pub dependencies: DependencyContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenedSession {
    pub session: SessionId,
    pub workspace: WorkspaceId,
    pub lease: LeaseId,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub compact_context: CompactContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingHandoff {
    pub handoff_id: HandoffId,
    pub session: SessionId,
    pub predecessor: WorkspaceId,
    pub successor: WorkspaceId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretKeyPreview {
    pub key: String,
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretFilePreview {
    pub path: String,
    pub file_result: String,
    #[serde(default)]
    pub keys: Vec<SecretKeyPreview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRequired {
    pub review_id: ReviewId,
    pub kind: String,
    pub files: Vec<SecretFilePreview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictOutcome {
    pub operation_id: OperationId,
    pub workspace: WorkspaceId,
    pub cwd: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", content = "result", rename_all = "snake_case")]
pub enum Outcome {
    Completed(Value),
    Accepted { operation_id: OperationId },
    ReviewRequired(ReviewRequired),
    Conflict(ConflictOutcome),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadeError {
    pub code: String,
    pub retry: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireResponse {
    pub v: u16,
    pub request_id: String,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseBody {
    Ok { outcome: Outcome },
    Error { error: ShadeError },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub v: u16,
    pub cursor: i64,
    pub event: String,
    pub resource: String,
    pub payload: Value,
    pub created_at_ms: i64,
}
