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
    /// Resume a dormant session on its existing workspace with a fresh lease.
    SessionReattach {
        session_id: SessionId,
    },
    /// Check the workspace in and free its tree, keeping every record.
    WorkspaceSleep {
        selector: WorkspaceSelector,
    },
    /// Rematerialize a suspended session as a successor workspace.
    SessionWake {
        session_id: SessionId,
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
    /// The lifecycle of one session, answerable without a live lease.
    Session {
        session_id: SessionId,
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

fn lifecycle_active() -> String {
    "active".into()
}

/// The common case is elided on the wire.
///
/// `context` is the response an agent reads most often and it lives inside a
/// 512-byte budget with two bytes to spare; spending twenty-one of them
/// repeating `active` on every healthy workspace would buy nothing. An absent
/// `lifecycle` deserializes back to `active`, so a v1 reader and a v1 writer
/// still agree, and the field appears exactly when it carries news.
fn lifecycle_is_active(value: &String) -> bool {
    value == "active"
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
    /// `live | expired | released`, kept because v1 callers read it.
    pub lease: String,
    /// `active | dormant | suspended | released`; absent means `active`.
    #[serde(
        default = "lifecycle_active",
        skip_serializing_if = "lifecycle_is_active"
    )]
    pub lifecycle: String,
    pub dependencies: DependencyContext,
}

/// The lifecycle of one session, independent of any live lease. Answering this
/// is how a keepalive decides between renewing, reattaching and exiting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session: SessionId,
    /// `active | dormant | suspended | released`.
    pub lifecycle: String,
    pub workspace: WorkspaceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_ms: Option<i64>,
    /// Absent when the workspace holds no materialized tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub materialized: bool,
}

/// The result of `workspace_sleep`. The workspace keeps its identity, its
/// checkpoints and its secrets; only the tree is gone, and `wake` rebuilds it
/// as a successor from `checkpoint_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SleepResult {
    pub session: SessionId,
    pub workspace: WorkspaceId,
    pub checkpoint_id: CheckpointId,
    pub suspended: bool,
    /// Disk reclaimed by removing the tree, measured before removal.
    pub reclaimed_bytes: u64,
    /// The directory that no longer exists.
    ///
    /// A caller that ran `shade sleep` from inside its own workspace is now
    /// sitting in a deleted directory, and every later selector command fails
    /// on `getcwd` before it reaches the daemon. Naming the path is what lets a
    /// shell wrapper or an agent notice and move out of it.
    pub cwd: String,
    /// What to do next, in one line: leave the deleted directory, then wake.
    pub next: String,
    /// Whether the private build output was copied to the park volume instead
    /// of being discarded with the tree.
    #[serde(default)]
    pub parked: bool,
    /// How many bytes the park holds. Zero unless `parked`.
    #[serde(default)]
    pub parked_bytes: u64,
    /// Where the park landed, for an operator who has to find it by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park_path: Option<String>,
    /// Why nothing was parked: `unconfigured`, `unmounted`, `below_min_bytes`,
    /// `park_failed` or `not_recorded`. Absent when the output was parked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park_reason: Option<String>,
}

/// Reported by the CLI only. The daemon and the SDKs never populate it: SDK
/// hosts heartbeat in-process and are opted out by construction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeepaliveStatus {
    /// `started | skipped | failed | stopped`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenedSession {
    pub session: SessionId,
    pub workspace: WorkspaceId,
    pub lease: LeaseId,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub compact_context: CompactContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive: Option<KeepaliveStatus>,
}

/// The result of `session_wake`: everything `open` returns, plus what the
/// parked tier could give back.
///
/// The session fields are flattened, so a v1 caller that reads a wake as an
/// [`OpenedSession`] still parses every field it knows and ignores the rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeResult {
    #[serde(flatten)]
    pub session: OpenedSession,
    /// Whether the parked build output was copied back into the successor.
    #[serde(default)]
    pub park_restored: bool,
    /// How many bytes came back. Zero unless `park_restored`.
    #[serde(default)]
    pub park_restored_bytes: u64,
    /// Why the park was not restored: `unconfigured`, `unmounted`, `absent`,
    /// `manifest_mismatch` or `park_failed`. Absent when there was no park to
    /// restore or when it was restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub park_reason: Option<String>,
    /// Said only when a park existed and did not come back, because that is
    /// the one case where the successor is not what the caller expected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn context(lifecycle: &str) -> CompactContext {
        CompactContext {
            workspace: WorkspaceId("ws_1".into()),
            session: SessionId("session-1".into()),
            base_ref: "origin/main".into(),
            base_sha: ObjectId("abc".into()),
            head_sha: ObjectId("def".into()),
            remote_sha: None,
            changes: CompactChanges {
                staged: 0,
                unstaged: 0,
                untracked: 0,
            },
            lease: "live".into(),
            lifecycle: lifecycle.into(),
            dependencies: DependencyContext {
                state: "ready".into(),
                providers: Vec::new(),
                blocked_builds: Vec::new(),
            },
        }
    }

    #[test]
    fn an_active_lifecycle_is_elided_and_restored_but_any_other_is_carried() {
        let active = serde_json::to_string(&context("active")).unwrap();
        assert!(
            !active.contains("lifecycle"),
            "the common case must not spend wire budget: {active}"
        );
        let decoded: CompactContext = serde_json::from_str(&active).unwrap();
        assert_eq!(decoded.lifecycle, "active");

        for lifecycle in ["dormant", "suspended", "released"] {
            let encoded = serde_json::to_string(&context(lifecycle)).unwrap();
            assert!(encoded.contains(&format!("\"lifecycle\":\"{lifecycle}\"")));
            let decoded: CompactContext = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded.lifecycle, lifecycle);
        }
    }

    #[test]
    fn a_v1_context_without_a_lifecycle_field_reads_as_active() {
        let elided = serde_json::json!({
            "workspace": "ws_1",
            "session": "session-1",
            "base_ref": "origin/main",
            "base_sha": "abc",
            "head_sha": "def",
            "changes": {"staged": 0, "unstaged": 0, "untracked": 0},
            "lease": "live",
            "dependencies": {"state": "ready"},
        });
        let decoded: CompactContext = serde_json::from_value(elided).unwrap();
        assert_eq!(decoded.lifecycle, "active");
    }

    fn opened() -> OpenedSession {
        OpenedSession {
            session: SessionId("session-1".into()),
            workspace: WorkspaceId("ws_1".into()),
            lease: LeaseId("lease_1".into()),
            cwd: "/tmp/ws".into(),
            env: BTreeMap::new(),
            compact_context: context("active"),
            keepalive: None,
        }
    }

    #[test]
    fn a_wake_result_reads_back_as_a_plain_opened_session() {
        let wake = WakeResult {
            session: opened(),
            park_restored: false,
            park_restored_bytes: 0,
            park_reason: Some("unmounted".into()),
            next: Some(
                "build output not restored (unmounted); it will regenerate on the next build"
                    .into(),
            ),
        };
        let encoded = serde_json::to_value(&wake).unwrap();
        // Flattened, so a v1 reader that only knows `OpenedSession` still sees
        // every field it was written against.
        let v1: OpenedSession = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(v1.workspace.0, "ws_1");
        assert_eq!(encoded["park_reason"], "unmounted");
        let decoded: WakeResult = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.session.lease.0, "lease_1");
        assert!(!decoded.park_restored);
        assert_eq!(decoded.park_restored_bytes, 0);
    }

    #[test]
    fn a_v1_sleep_result_without_park_fields_still_parses() {
        let v1 = serde_json::json!({
            "session": "session-1",
            "workspace": "ws_1",
            "checkpoint_id": "ckpt_1",
            "suspended": true,
            "reclaimed_bytes": 4096,
            "cwd": "/tmp/ws",
            "next": "cd elsewhere",
        });
        let decoded: SleepResult = serde_json::from_value(v1).unwrap();
        assert!(!decoded.parked);
        assert_eq!(decoded.parked_bytes, 0);
        assert_eq!(decoded.park_path, None);
        assert_eq!(decoded.park_reason, None);
    }

    #[test]
    fn a_v1_wake_result_without_park_fields_still_parses() {
        let mut v1 = serde_json::to_value(opened()).unwrap();
        v1.as_object_mut().unwrap().remove("keepalive");
        let decoded: WakeResult = serde_json::from_value(v1).unwrap();
        assert!(!decoded.park_restored);
        assert_eq!(decoded.next, None);
    }

    #[test]
    fn a_session_status_always_states_its_lifecycle() {
        let status = SessionStatus {
            session: SessionId("session-1".into()),
            lifecycle: "dormant".into(),
            workspace: WorkspaceId("ws_1".into()),
            lease: None,
            lease_expires_at_ms: None,
            cwd: Some("/tmp/ws".into()),
            materialized: true,
        };
        let encoded = serde_json::to_value(&status).unwrap();
        assert_eq!(encoded["lifecycle"], "dormant");
        assert_eq!(encoded.get("lease"), None);
        assert_eq!(encoded["materialized"], true);
    }
}
