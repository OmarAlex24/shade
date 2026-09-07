use crate::config::EngineConfig;
use crate::db::{
    BeginOperation, CheckpointRecord, Database, DbError, LeaseRecord, OperationRecord,
    PublishIntentRecord, PublishResolutionRecord, RepositoryRecord, ReviewRecord, SessionRecord,
    WorkspaceRecord, now_ms,
};
use crate::dependencies::{
    DependencyContext as ReadinessContext, DependencyError, DependencyService,
};
use crate::filesystem::{ApfsFilesystem, WorkspaceFilesystem};
use crate::git::{
    BaseRevision, BaseSpec, Checkpoint, GitStore, ManagedRepository, Oid,
    PrepareSquashPublishRequest, PreparedPublish, RemoteIdentity,
};
use crate::secrets::{MergeChoice, SecretStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shade_protocol::{
    Actor, ActorKind, CheckpointId, CompactChanges, CompactContext, ConflictOutcome,
    DependencyContext as ProtocolDependencyContext, EventEnvelope, ExecuteRequest, HandoffId,
    Intent, LeaseId, ObjectId, OpenSession, OpenedSession, OperationId, Outcome, PROTOCOL_VERSION,
    Query, QueryRequest, RepositoryId, RepositoryLocator, ResponseBody, ReviewAction, ReviewId,
    ReviewRequired, SessionId, SessionStatus, ShadeError, SleepResult, WireResponse, WorkspaceId,
    WorkspaceSelector,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Instant;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Debug, Clone, thiserror::Error)]
#[error("{code}")]
pub struct EngineError {
    pub code: String,
    pub retry: String,
    pub operation: Option<OperationId>,
    pub next: Option<String>,
    pub diagnostics_id: Option<Box<str>>,
    diagnostic: Option<Box<shade_protocol::Diagnostic>>,
}

fn checkpoint_record(
    id: &CheckpointId,
    workspace_id: &WorkspaceId,
    reason: &str,
    checkpoint: &Checkpoint,
) -> CheckpointRecord {
    CheckpointRecord {
        id: id.clone(),
        workspace_id: workspace_id.clone(),
        head_oid: ObjectId(checkpoint.head.to_string()),
        index_oid: ObjectId(checkpoint.index_tree.to_string()),
        worktree_oid: ObjectId(checkpoint.working_tree.to_string()),
        reason: reason.to_owned(),
        state: "ready".into(),
    }
}

fn display_base_ref(base: &BaseRevision) -> String {
    match base.source_ref.as_deref() {
        Some(reference) if reference.starts_with("refs/heads/") => {
            format!("origin/{}", &reference["refs/heads/".len()..])
        }
        Some(reference) => reference.to_owned(),
        None => format!("oid:{}", base.commit),
    }
}

fn stored_base_request(base_ref: &str) -> Option<String> {
    base_ref
        .strip_prefix("oid:")
        .map(str::to_owned)
        .or_else(|| Some(base_ref.to_owned()))
}

fn simple_branch(value: &str) -> Option<String> {
    if value.is_empty()
        || value.starts_with('-')
        || value.starts_with("refs/")
        || value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(value.strip_prefix("origin/").unwrap_or(value).to_owned())
}

/// Does a `--base` argument name the base a workspace was already cut from?
///
/// `open` normalizes `main` to `origin/main` before storing it, so a literal
/// comparison would reject the most common reattach there is.
fn base_matches(requested: &str, stored: &str) -> bool {
    if requested == stored {
        return true;
    }
    if let Some(commit) = stored.strip_prefix("oid:") {
        return commit.eq_ignore_ascii_case(requested);
    }
    match (simple_branch(requested), simple_branch(stored)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn saturating_u32(value: usize) -> u32 {
    value.try_into().unwrap_or(u32::MAX)
}

fn remove_private_git_pointer(workspace: &Path) -> Result<(), EngineError> {
    let pointer = workspace.join(".git");
    let metadata = std::fs::symlink_metadata(&pointer).map_err(EngineError::internal)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(EngineError::domain("WORKTREE_METADATA_INVALID", "never"));
    }
    std::fs::remove_file(pointer).map_err(EngineError::internal)
}

fn secret_preview_changed(files: &[shade_protocol::SecretFilePreview]) -> bool {
    files.iter().any(|file| {
        file.file_result != "unchanged" || file.keys.iter().any(|key| key.result != "unchanged")
    })
}

fn operation_principal(actor: &Actor) -> String {
    let kind = match actor.kind {
        ActorKind::Host => "host",
        ActorKind::Agent => "agent",
        ActorKind::Cli => "cli",
        ActorKind::System => "system",
    };
    // The NUL separator is forbidden in caller supplied actor ids, so the
    // (kind,id) tuple has an injective durable representation and ordinary
    // clients cannot pre-seed the daemon's System namespace.
    format!("{kind}\0{}", actor.id)
}

fn publish_anchor(operation: &OperationId) -> String {
    format!("refs/shade/operations/{}/publish", operation.0)
}

fn publish_outcome(branch: &str, pushed: bool, prepared: &PreparedPublish) -> Outcome {
    Outcome::Completed(json!({
        "branch": branch,
        "commit": prepared.commit.to_string(),
        "tree": prepared.tree.to_string(),
        "previous_remote": prepared.previous_remote.as_ref().map(ToString::to_string),
        "pushed": pushed,
    }))
}

fn dependency_error(error: DependencyError) -> EngineError {
    match error {
        DependencyError::ToolUnavailable(_) => {
            EngineError::domain("DEPENDENCY_TOOLCHAIN_MISSING", "never")
                .next("install the exact required host tool and retry")
        }
        DependencyError::LockMissing(_) => EngineError::domain("DEPENDENCY_LOCK_REQUIRED", "never")
            .next("generate and commit the required lockfile"),
        DependencyError::LockStale(_) => EngineError::domain("DEPENDENCY_LOCK_STALE", "never")
            .next("refresh and commit the dependency lockfile"),
        DependencyError::UnsafeConfiguration { .. } => {
            EngineError::domain("DEPENDENCY_UNSAFE_CONFIG", "never")
                .next("remove executable or credential-bearing package-manager configuration")
        }
        DependencyError::InvalidConfiguration { .. }
        | DependencyError::InvalidLock { .. }
        | DependencyError::ToolVersionMismatch { .. } => {
            EngineError::domain("DEPENDENCY_INPUT_INVALID", "never")
                .next("make manifests, lockfiles and declared tool versions consistent")
        }
        DependencyError::CowUnavailable { .. } => EngineError::domain("COW_UNAVAILABLE", "never")
            .next("place the Shade root and dependency layers on one APFS volume"),
        DependencyError::Policy(_) => EngineError::domain("DEPENDENCY_POLICY_BLOCKED", "never")
            .next("remove the blocked source-build or executable configuration"),
        DependencyError::Validation { .. } => {
            EngineError::domain("DEPENDENCY_LAYER_INVALID", "safe")
                .next("retry to rebuild the immutable layer")
        }
        DependencyError::CommandFailed { .. }
        | DependencyError::Failed(_)
        | DependencyError::Io { .. } => EngineError::internal(error),
    }
}

/// A repository rejection names the first offending path. Bisecting a tree by
/// hand to find out which file was meant is not an answer, and the path is
/// the only part of the file that may ever be echoed. The bound keeps the
/// widest rejection inside the common 512-byte response budget.
const REJECTED_PATH_LIMIT_BYTES: usize = 120;

/// The rejections that carry a path, as `<code>: <path>` with nothing after
/// it, and how each one asks for that path to be dealt with.
const PATH_REJECTIONS: [(&str, &str, &str, &str); 5] = [
    (
        "TRACKED_SECRET_FILE",
        "untrack ",
        ", then retry",
        "untrack the private .env file, then retry",
    ),
    (
        "TRACKED_DEPENDENCY_OUTPUT",
        "untrack ",
        ", then retry",
        "untrack the dependency output directory, then retry",
    ),
    (
        "UNSUPPORTED_SUBMODULE",
        "remove the submodule at ",
        ", then retry",
        "remove or flatten submodules, then retry",
    ),
    (
        "UNSUPPORTED_GIT_LFS",
        "replace the Git LFS content at ",
        ", then retry",
        "replace Git LFS content before opening",
    ),
    (
        "UNSUPPORTED_GIT_FILTER",
        "remove the Git filter declared in ",
        ", then retry",
        "remove custom Git filters, then retry",
    ),
];

/// The message tail after `<code>: `, bounded and reduced to one line. Only a
/// path is ever repeated this way; no rejection carries content.
fn rejected_path(message: &str, code: &str) -> Option<String> {
    let tail = message.split_once(&format!("{code}: "))?.1;
    let tail = tail.lines().next().unwrap_or_default().trim();
    if tail.is_empty() {
        return None;
    }
    let mut path = tail.to_owned();
    if path.len() > REJECTED_PATH_LIMIT_BYTES {
        let mut boundary = REJECTED_PATH_LIMIT_BYTES;
        while !path.is_char_boundary(boundary) {
            boundary -= 1;
        }
        path.truncate(boundary);
        path.push_str("...");
    }
    Some(path)
}

fn subsystem_error(error: impl std::fmt::Display) -> EngineError {
    let message = error.to_string();
    for (code, before, after, fallback) in PATH_REJECTIONS {
        if !message.contains(code) {
            continue;
        }
        let next = match rejected_path(&message, code) {
            Some(path) => format!("{before}{path}{after}"),
            None => fallback.to_owned(),
        };
        return EngineError::domain(code, "never").next(next);
    }
    let known = [
        (
            "COW_UNAVAILABLE",
            "COW_UNAVAILABLE",
            "never",
            "place all Shade pools on one APFS volume",
        ),
        (
            "UNMERGED_WORKTREE",
            "UNMERGED_WORKTREE",
            "never",
            "resolve the index first",
        ),
        (
            "WORKSPACE_NOT_QUIESCENT",
            "WORKSPACE_NOT_QUIESCENT",
            "safe",
            "stop workspace writers and retry with a new idempotency key",
        ),
        (
            "REMOTE_MOVED",
            "PUBLISH_REMOTE_MOVED",
            "safe",
            "fetch/sync and retry publish",
        ),
        (
            "REMOTE_BRANCH_EXISTS",
            "PUBLISH_REMOTE_MOVED",
            "safe",
            "fetch/sync and retry publish",
        ),
        (
            "LOCAL_BRANCH_MOVED",
            "PUBLISH_LOCAL_MOVED",
            "safe",
            "inspect the local branch before retrying",
        ),
        (
            "MERGE_CONFLICT",
            "INTEGRATION_CONFLICT",
            "never",
            "resolve in the returned resolution workspace",
        ),
    ];
    for (needle, code, retry, next) in known {
        if message.contains(needle) {
            return EngineError::domain(code, retry).next(next);
        }
    }
    EngineError::internal(message)
}

impl EngineError {
    pub fn domain(code: impl Into<String>, retry: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            retry: retry.into(),
            operation: None,
            next: None,
            diagnostics_id: None,
            diagnostic: None,
        }
    }

    pub fn next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }

    pub fn operation(mut self, operation: OperationId) -> Self {
        self.operation = Some(operation);
        self
    }

    pub fn internal(error: impl std::fmt::Display) -> Self {
        let diagnostic =
            crate::diagnostics::new(shade_protocol::DiagnosticOrigin::Daemon, "INTERNAL", error);
        Self {
            code: "INTERNAL".into(),
            retry: "safe".into(),
            operation: None,
            next: Some("shade doctor".into()),
            diagnostics_id: None,
            diagnostic: Some(Box::new(diagnostic)),
        }
    }

    /// Sanitized detail for a startup failure that occurred before the engine
    /// could persist its own diagnostic. No uncommitted reference is exposed.
    pub fn diagnostic_message(&self) -> Option<&str> {
        self.diagnostic
            .as_ref()
            .map(|diagnostic| diagnostic.message.as_str())
    }

    pub fn as_wire(&self) -> ShadeError {
        ShadeError {
            code: self.code.clone(),
            retry: self.retry.clone(),
            operation: self.operation.clone(),
            next: self.next.clone(),
            diagnostics_id: self.diagnostics_id.as_deref().map(str::to_owned),
        }
    }
}

impl From<DbError> for EngineError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::IdempotencyConflict => Self::domain("IDEMPOTENCY_KEY_REUSED", "never")
                .next("retry with the original intent or a new idempotency key"),
            DbError::LeaseFenced => {
                Self::domain("LEASE_FENCED", "never").next("use the current successor workspace")
            }
            DbError::WorkspaceNotAttachable { state } => {
                Self::domain("WORKSPACE_NOT_ATTACHABLE", "never").next(format!(
                    "the workspace is {state}; finish or release it before attaching"
                ))
            }
            DbError::HandoffNotFound => Self::domain("HANDOFF_NOT_FOUND", "never"),
            DbError::HandoffOwnerMismatch => Self::domain("HANDOFF_FORBIDDEN", "never"),
            DbError::HandoffNotPending => Self::domain("HANDOFF_ALREADY_RESOLVED", "never"),
            DbError::HandoffAlreadyPending { handoff_id } => {
                Self::domain("HANDOFF_PENDING", "never")
                    .next(format!("adopt pending handoff {handoff_id}"))
            }
            other => Self::internal(other),
        }
    }
}

#[derive(Clone)]
pub struct Engine {
    config: Arc<EngineConfig>,
    database: Database,
    secrets: SecretStore,
    git: GitStore,
    filesystem: Arc<dyn WorkspaceFilesystem>,
    dependencies: Arc<DependencyService>,
    locks: Arc<LifecycleLocks>,
    #[cfg(any(test, feature = "test-support"))]
    failures: Arc<crate::faults::InjectedFailures>,
}

/// Sweep threshold for the keyed-lock map. Below it the common path stays
/// O(1); at or above it a lookup also drops entries nobody holds any more.
const KEYED_LOCK_SWEEP_THRESHOLD: usize = 256;

/// What a workspace is allowed to do right now, derived from the columns that
/// already exist. Mutations still demand `Lifecycle::Active`; the operations
/// that survive a lease expiry (`release`, `context`) branch on this instead.
#[derive(Debug)]
enum Lifecycle {
    Active { lease: LeaseRecord },
    Dormant,
    Suspended,
    Released,
}

/// Which way an interrupted suspension went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    /// Rolled forward: tree and registration gone, record `suspended`.
    Finished,
    /// Rolled back: no checkpoint to wake from, so the workspace is Dormant.
    Reverted,
}

impl Lifecycle {
    /// `active | dormant | suspended | released`, as reported on the wire.
    fn label(&self) -> &'static str {
        match self {
            Lifecycle::Active { .. } => "active",
            Lifecycle::Dormant => "dormant",
            Lifecycle::Suspended => "suspended",
            Lifecycle::Released => "released",
        }
    }
}

#[derive(Default)]
struct LifecycleLocks {
    /// Weak so a key touched once does not pin an `AsyncMutex` for the
    /// daemon's lifetime. Live callers keep their `Arc` through the
    /// `OwnedMutexGuard`, so mutual exclusion is unaffected.
    keyed: AsyncMutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    fetch_cache: AsyncMutex<HashMap<String, CachedBase>>,
    script_policy: tokio::sync::RwLock<()>,
    dependency_artifacts: tokio::sync::RwLock<()>,
}

impl LifecycleLocks {
    /// Serialize callers that name the same key. Concurrent callers resolve to
    /// the same `Arc`; the returned guard owns it, so the map entry stays
    /// upgradable for the whole critical section. Once every guard drops the
    /// `Weak` dangles and a later lookup replaces or sweeps it.
    async fn keyed_lock(&self, key: String) -> OwnedMutexGuard<()> {
        let lock = {
            let mut keyed = self.keyed.lock().await;
            // Opportunistic sweep: entries whose owners have all finished no
            // longer upgrade, so the map cannot grow once per key ever touched.
            if keyed.len() >= KEYED_LOCK_SWEEP_THRESHOLD {
                keyed.retain(|_, weak| weak.strong_count() > 0);
            }
            match keyed.get(&key).and_then(Weak::upgrade) {
                Some(existing) => existing,
                None => {
                    let created = Arc::new(AsyncMutex::new(()));
                    keyed.insert(key, Arc::downgrade(&created));
                    created
                }
            }
        };
        lock.lock_owned().await
    }

    /// Drop every keyed entry nobody holds any more. Production reaches this
    /// through the threshold in `keyed_lock`; tests call it directly.
    #[cfg(test)]
    async fn sweep_keyed(&self) -> usize {
        let mut keyed = self.keyed.lock().await;
        keyed.retain(|_, weak| weak.strong_count() > 0);
        keyed.len()
    }
}

#[derive(Clone)]
struct CachedBase {
    at: Instant,
    revision: BaseRevision,
}

struct DependencySnapshot {
    source_root: PathBuf,
    receipts: Vec<crate::dependencies::DependencyReceipt>,
}

struct RepositoryHandle {
    record: RepositoryRecord,
    managed: ManagedRepository,
    remote: RemoteIdentity,
}

#[derive(Debug, Default)]
struct ReconciliationStats {
    publishes_completed: u64,
    publishes_pending: u64,
    publishes_failed: u64,
    publish_anchors_removed: u64,
    incomplete_removed: u64,
    incomplete_failed: u64,
    invalid_workspaces: u64,
    /// How many of those were `dormant`: work a caller was told is safe, now
    /// collectible. Zero is the only value that needs no explanation.
    dormant_workspaces_failed: u64,
    worktree_metadata_removed: u64,
    worktree_metadata_conflicts: u64,
    checkpoint_refs_removed: u64,
    suspensions_finished: u64,
    suspensions_reverted: u64,
    suspensions_failed: u64,
}

struct PublishResolutionInput<'a> {
    branch: &'a str,
    message: &'a str,
    push: bool,
    operation: &'a OperationId,
    expected_local: Option<Oid>,
}

#[derive(Clone, Copy)]
enum SuccessorSource {
    ImmutableBase,
    ParentWorkspace,
}

/// Where a successor's private files come from.
///
/// Restore, sync and refresh read them out of the parent's working tree.
/// A wake has no parent tree left to read -- that is what being suspended
/// means -- so it reads the vault sleep wrote instead.
#[derive(Clone, Copy)]
enum SuccessorSecrets {
    ParentTree,
    SuspensionVault,
}

impl Engine {
    pub fn open(config: EngineConfig) -> Result<Self, EngineError> {
        let git = GitStore::system().with_content_filter(
            std::env::current_exe().map_err(EngineError::internal)?,
            config.runtime_dir(),
        );
        Self::with_components_and_git(
            config,
            Arc::new(ApfsFilesystem),
            Arc::new(DependencyService::production()),
            git,
        )
    }

    pub fn with_components(
        config: EngineConfig,
        filesystem: Arc<dyn WorkspaceFilesystem>,
        dependencies: Arc<DependencyService>,
    ) -> Result<Self, EngineError> {
        Self::with_components_and_git(config, filesystem, dependencies, GitStore::system())
    }

    /// Sole constructor body: every other constructor funnels through here so
    /// the platform gate cannot be bypassed. Tests must not run on a
    /// construction path production cannot reach.
    pub fn with_components_and_git(
        config: EngineConfig,
        filesystem: Arc<dyn WorkspaceFilesystem>,
        dependencies: Arc<DependencyService>,
        git: GitStore,
    ) -> Result<Self, EngineError> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            return Err(EngineError::domain("PLATFORM_UNSUPPORTED", "never")
                .next("use Apple Silicon macOS with an APFS Shade root"));
        }
        create_roots(&config).map_err(EngineError::internal)?;
        let database = Database::open(config.database_path())?;
        let secrets = SecretStore::new(config.secrets_dir()).map_err(EngineError::internal)?;
        Ok(Self {
            config: Arc::new(config),
            database,
            secrets,
            git,
            filesystem,
            dependencies,
            locks: Arc::new(LifecycleLocks::default()),
            #[cfg(any(test, feature = "test-support"))]
            failures: Arc::new(crate::faults::InjectedFailures::default()),
        })
    }

    /// Arm a boundary to fail on this engine. Test-only, like `CopyFilesystem`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn injected_failures(&self) -> &crate::faults::InjectedFailures {
        &self.failures
    }

    /// Turn an armed boundary into an ordinary operation failure. Compiled
    /// away entirely in the distribution build.
    #[cfg(any(test, feature = "test-support"))]
    fn injected_failure(&self, point: crate::faults::Point) -> Result<(), EngineError> {
        if self.failures.arrive(point) {
            return Err(EngineError::internal(std::io::Error::other(format!(
                "injected failure at {}",
                point.name()
            ))));
        }
        Ok(())
    }

    #[cfg(not(any(test, feature = "test-support")))]
    #[inline(always)]
    fn injected_failure(&self, _point: crate::faults::Point) -> Result<(), EngineError> {
        Ok(())
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn secrets(&self) -> &SecretStore {
        &self.secrets
    }

    /// Read the resumable transactional outbox after an exclusive cursor.
    /// This is the third public engine seam beside `execute` and `query`.
    pub fn events(&self, after_cursor: i64, limit: u32) -> Result<Vec<EventEnvelope>, EngineError> {
        self.database
            .events(after_cursor, limit)
            .map_err(Into::into)
    }

    pub async fn execute(&self, request: ExecuteRequest) -> WireResponse {
        let request_id = request.request_id.clone();
        let body = match self.execute_outcome(request).await {
            Ok(outcome) => ResponseBody::Ok { outcome },
            Err(error) => ResponseBody::Error {
                error: self.persist_error(error),
            },
        };
        WireResponse {
            v: PROTOCOL_VERSION,
            request_id,
            body,
        }
    }

    pub async fn query(&self, request: QueryRequest) -> WireResponse {
        let request_id = request.request_id.clone();
        let body = if request.v != PROTOCOL_VERSION {
            ResponseBody::Error {
                error: EngineError::domain("PROTOCOL_VERSION_UNSUPPORTED", "never")
                    .next(format!("use protocol version {PROTOCOL_VERSION}"))
                    .as_wire(),
            }
        } else {
            match self.query_outcome(request.query).await {
                Ok(outcome) => ResponseBody::Ok { outcome },
                Err(error) => ResponseBody::Error {
                    error: self.persist_error(error),
                },
            }
        };
        WireResponse {
            v: PROTOCOL_VERSION,
            request_id,
            body,
        }
    }

    fn persist_error(&self, mut error: EngineError) -> ShadeError {
        if let Some(mut diagnostic) = error.diagnostic.take() {
            diagnostic.operation = error.operation.clone();
            if self.database.record_diagnostic(&diagnostic).is_ok() {
                error.diagnostics_id = Some(diagnostic.id.into_boxed_str());
            }
        }
        // A failed diagnostic write must not leave a dangling public reference.
        error.as_wire()
    }

    async fn execute_outcome(&self, request: ExecuteRequest) -> Result<Outcome, EngineError> {
        if request.v != PROTOCOL_VERSION {
            return Err(EngineError::domain("PROTOCOL_VERSION_UNSUPPORTED", "never")
                .next(format!("use protocol version {PROTOCOL_VERSION}")));
        }
        if request.idempotency_key.is_empty() || request.idempotency_key.len() > 256 {
            return Err(EngineError::domain("IDEMPOTENCY_KEY_INVALID", "never")
                .next("provide a non-empty key of at most 256 bytes"));
        }
        if request.actor.id.is_empty()
            || request.actor.id.len() > 256
            || request.actor.id.contains('\0')
        {
            return Err(EngineError::domain("ACTOR_ID_INVALID", "never")
                .next("provide a non-empty actor id without NUL bytes"));
        }
        if matches!(
            &request.intent,
            Intent::MaintenanceSweep | Intent::Reconcile
        ) && request.actor.kind != ActorKind::System
        {
            return Err(EngineError::domain("INTENT_FORBIDDEN", "never"));
        }
        let encoded = serde_json::to_vec(&request.intent).map_err(EngineError::internal)?;
        let request_hash = hex::encode(Sha256::digest(encoded));
        let kind = intent_kind(&request.intent);
        let principal = operation_principal(&request.actor);
        let operation = match self.database.begin_operation(
            &principal,
            &request.idempotency_key,
            &request_hash,
            kind,
        )? {
            BeginOperation::New(operation) => operation,
            BeginOperation::Existing(record) => {
                let OperationRecord {
                    id, outcome, error, ..
                } = *record;
                if let Some(outcome) = outcome {
                    return Ok(outcome);
                }
                if let Some(error) = error {
                    return Err(EngineError {
                        code: error.code,
                        retry: error.retry,
                        operation: Some(id),
                        next: error.next,
                        diagnostics_id: error.diagnostics_id.map(String::into_boxed_str),
                        diagnostic: None,
                    });
                }
                return Ok(Outcome::Accepted { operation_id: id });
            }
        };
        crate::faults::hit(crate::faults::Point::OperationRecorded);
        let result = self
            .dispatch(request.intent, &operation, &request.actor)
            .await;
        match result {
            Ok(outcome) => {
                crate::faults::hit(crate::faults::Point::OperationDispatched);
                self.database
                    .finish_operation(&operation, &outcome)
                    .map_err(|error| EngineError::from(error).operation(operation.clone()))?;
                crate::faults::hit(crate::faults::Point::OperationCompleted);
                Ok(outcome)
            }
            Err(mut error) => {
                error.operation = Some(operation.clone());
                let diagnostic = error.diagnostic.take().map(|mut diagnostic| {
                    diagnostic.operation = Some(operation.clone());
                    diagnostic
                });
                let mut wire = error.as_wire();
                if let Some(diagnostic) = &diagnostic {
                    wire.diagnostics_id = Some(diagnostic.id.clone());
                }
                self.database
                    .fail_operation(&operation, &wire, diagnostic.as_deref())
                    .map_err(|error| EngineError::from(error).operation(operation.clone()))?;
                error.diagnostics_id = wire.diagnostics_id.map(String::into_boxed_str);
                Err(error)
            }
        }
    }

    async fn dispatch(
        &self,
        intent: Intent,
        operation: &OperationId,
        actor: &Actor,
    ) -> Result<Outcome, EngineError> {
        let actor_principal = operation_principal(actor);
        match intent {
            Intent::SessionOpen(request) => self.open_session(request, operation).await,
            Intent::SessionReattach { session_id } => {
                self.reattach_session(session_id, None, operation).await
            }
            Intent::RepositoryWarm { repository } => self.warm(repository, operation).await,
            Intent::LeaseHeartbeat {
                session_id,
                lease_id,
            } => {
                let lease = match self.database.heartbeat(
                    &session_id,
                    &lease_id,
                    self.config.lease_ttl_secs,
                )? {
                    Some(lease) => lease,
                    // Sleeping releases the lease, so a suspended session
                    // looks exactly like an expired one from here. Saying
                    // `LEASE_EXPIRED` would send the caller into a reattach
                    // that cannot succeed; name the suspension and the command
                    // that ends it instead.
                    None => {
                        let suspended =
                            self.database.session(&session_id)?.is_some_and(|session| {
                                matches!(session.state.as_str(), "suspended" | "suspending")
                            });
                        return Err(if suspended {
                            EngineError::domain("SESSION_SUSPENDED", "never")
                                .next(format!("shade wake --session {}", session_id.0))
                        } else {
                            EngineError::domain("LEASE_EXPIRED", "never").next("open a new session")
                        });
                    }
                };
                Ok(Outcome::Completed(json!({
                    "lease": lease.id,
                    "expires_at_ms": lease.expires_at_ms,
                })))
            }
            Intent::WorkspaceCheckpoint { selector, reason } => {
                let workspace = self.resolve_workspace(&selector)?;
                self.require_live_lease(&workspace)?;
                let checkpoint = self
                    .checkpoint_workspace(&workspace, &reason, operation)
                    .await?;
                Ok(Outcome::Completed(json!({
                    "checkpoint_id": checkpoint.id,
                    "head_sha": checkpoint.head_oid,
                    "index_tree": checkpoint.index_oid,
                    "working_tree": checkpoint.worktree_oid,
                })))
            }
            Intent::WorkspaceFork {
                selector,
                child_session_id,
                intent,
            } => {
                self.fork_workspace(selector, child_session_id, intent, operation)
                    .await
            }
            Intent::WorkspaceSync { selector } => {
                self.sync_workspace(selector, operation, &actor_principal)
                    .await
            }
            Intent::WorkspaceRestore {
                selector,
                checkpoint_id,
            } => {
                self.restore_workspace(selector, checkpoint_id, operation, &actor_principal)
                    .await
            }
            Intent::DependenciesRefresh { selector } => {
                self.refresh_dependencies(selector, operation, &actor_principal)
                    .await
            }
            Intent::DependencyScriptDecision {
                selector,
                approval,
                allow,
            } => {
                let workspace = self.resolve_workspace(&selector)?;
                let _lifecycle = self
                    .keyed_lock(format!("lifecycle:{}", workspace.id.0))
                    .await;
                self.require_live_lease(&workspace)?;
                let _policy = self.locks.script_policy.write().await;
                self.database.bind_operation_resource(
                    operation,
                    &workspace.id.0,
                    "script_decision",
                )?;
                let candidates = self.dependency_scripts(&workspace)?;
                if allow && !candidates.iter().any(|script| script.approval == approval) {
                    return Err(EngineError::domain("SCRIPT_APPROVAL_NOT_FOUND", "never")
                        .next("shade deps scripts"));
                }
                if !allow && !self.database.script_approvals()?.contains(&approval) {
                    return Err(EngineError::domain("SCRIPT_APPROVAL_NOT_FOUND", "never"));
                }
                self.database
                    .decide_script(operation, &actor_principal, &approval, allow)
                    .map_err(Into::into)
            }
            Intent::WorkspacePublish {
                selector,
                branch,
                message,
                push,
            } => {
                self.publish_workspace(selector, &branch, &message, push, operation)
                    .await
            }
            Intent::ResolutionComplete { selector } => {
                self.complete_resolution(selector, operation, &actor_principal)
                    .await
            }
            Intent::WorkspaceRelease { selector } => {
                self.release_workspace(selector, operation).await
            }
            Intent::WorkspaceSleep { selector } => {
                self.sleep_workspace(selector, operation, false).await
            }
            Intent::SessionWake { session_id } => self.wake_session(session_id, operation).await,
            Intent::ReviewResolve { review_id, action } => {
                self.resolve_review(review_id, action, operation, &actor_principal)
                    .await
            }
            Intent::SuccessorAdopt { handoff_id } => {
                self.adopt_successor(handoff_id, operation, &actor_principal)
                    .await
            }
            Intent::Reconcile => {
                // Rows written by a pre-dormancy binary still say `orphaned`,
                // which the collector treats as collectible. Normalizing before
                // anything else runs is what stops the first sweep after an
                // upgrade from deleting them.
                let normalized = self.database.normalize_legacy_dormant_states()?;
                let expired = self.database.mark_expired_leases(now_ms())?;
                crate::faults::hit(crate::faults::Point::ReconcileLeasesExpired);
                let publish_recovery = self.reconcile_publish_operations(operation).await?;
                crate::faults::hit(crate::faults::Point::ReconcilePublishesRecovered);
                let interrupted = self.database.recover_interrupted_operations(operation)?;
                let recovery = self.reconcile_workspace_resources(operation).await?;
                let staging = self.cleanup_staging()?;
                crate::faults::hit(crate::faults::Point::ReconcileCompleted);
                Ok(Outcome::Completed(json!({
                    "expired_sessions": expired.sessions,
                    "dormant_sessions": expired.sessions,
                    "dormant_workspaces": expired.workspaces,
                    "legacy_states_normalized": normalized,
                    "interrupted_operations": interrupted,
                    "publishes_completed": publish_recovery.publishes_completed,
                    "publishes_pending": publish_recovery.publishes_pending,
                    "publishes_failed": publish_recovery.publishes_failed,
                    "publish_anchors_removed": publish_recovery.publish_anchors_removed,
                    "incomplete_removed": recovery.incomplete_removed,
                    "incomplete_failed": recovery.incomplete_failed,
                    "invalid_workspaces": recovery.invalid_workspaces,
                    "dormant_workspaces_failed": recovery.dormant_workspaces_failed,
                    "worktree_metadata_removed": recovery.worktree_metadata_removed,
                    "worktree_metadata_conflicts": recovery.worktree_metadata_conflicts,
                    "checkpoint_refs_removed": recovery.checkpoint_refs_removed,
                    "suspensions_finished": recovery.suspensions_finished,
                    "suspensions_reverted": recovery.suspensions_reverted,
                    "suspensions_failed": recovery.suspensions_failed,
                    "staging_removed": staging,
                })))
            }
            Intent::MaintenanceSweep => {
                let sweep = self.database.mark_expired_leases(now_ms())?;
                // A suspension interrupted by a crash used to wait for the next
                // `Reconcile`, which is a daemon startup pass. The sweep runs
                // every 30 seconds, so the window in which a workspace is
                // neither reattachable nor wakeable closes on its own.
                let mut suspensions = ReconciliationStats::default();
                self.reconcile_suspending_workspaces(&mut suspensions)
                    .await?;
                // Both sweeps are no-ops unless an operator configured them.
                let auto_slept = self.auto_sleep_dormant().await?;
                let retention_released = self.expire_suspended_retention().await?;
                Ok(Outcome::Completed(json!({
                    // `expired_sessions` is load-bearing for existing hosts;
                    // the dormancy counters are additive.
                    "expired_sessions": sweep.sessions,
                    "dormant_sessions": sweep.sessions,
                    "dormant_workspaces": sweep.workspaces,
                    "suspensions_finished": suspensions.suspensions_finished,
                    "suspensions_reverted": suspensions.suspensions_reverted,
                    "suspensions_failed": suspensions.suspensions_failed,
                    "auto_slept": auto_slept,
                    "retention_released": retention_released,
                })))
            }
            Intent::GarbageCollect => self.garbage_collect(operation).await,
        }
    }

    async fn query_outcome(&self, query: Query) -> Result<Outcome, EngineError> {
        match query {
            Query::Diagnostics { diagnostics_id } => {
                if !crate::diagnostics::valid_id(&diagnostics_id) {
                    return Err(EngineError::domain("DIAGNOSTIC_ID_INVALID", "never"));
                }
                let diagnostic = self
                    .database
                    .diagnostic(&diagnostics_id)?
                    .ok_or_else(|| EngineError::domain("DIAGNOSTIC_NOT_FOUND", "never"))?;
                Ok(Outcome::Completed(
                    serde_json::to_value(diagnostic).map_err(EngineError::internal)?,
                ))
            }
            Query::DependencyScripts { selector } => {
                let workspace = self.resolve_workspace(&selector)?;
                let approved = self.database.script_approvals()?;
                let scripts = self.dependency_scripts(&workspace)?.into_iter().map(|script| {
                    json!({"allowed":approved.contains(&script.approval),"script":script})
                }).collect::<Vec<_>>();
                Ok(Outcome::Completed(json!({"scripts":scripts})))
            }
            Query::Operation { operation_id } => {
                let record = self
                    .database
                    .operation(&operation_id)?
                    .ok_or_else(|| EngineError::domain("OPERATION_NOT_FOUND", "never"))?;
                Ok(Outcome::Completed(
                    serde_json::to_value(record).map_err(EngineError::internal)?,
                ))
            }
            Query::OperationByKey {
                actor_kind,
                actor_id,
                idempotency_key,
            } => {
                let principal = operation_principal(&Actor {
                    kind: actor_kind,
                    id: actor_id,
                });
                let record = self
                    .database
                    .operation_by_key(&principal, &idempotency_key)?
                    .ok_or_else(|| EngineError::domain("OPERATION_NOT_FOUND", "never"))?;
                Ok(Outcome::Completed(
                    serde_json::to_value(record).map_err(EngineError::internal)?,
                ))
            }
            Query::Events {
                after_cursor,
                limit,
            } => Ok(Outcome::Completed(json!({
                "events": self.events(after_cursor, limit)?,
            }))),
            Query::Session { session_id } => {
                // Answerable without a live lease: this is what a caller that
                // just lost one uses to find out whether its work survived.
                let session = self
                    .database
                    .session(&session_id)?
                    .ok_or_else(|| EngineError::domain("SESSION_NOT_FOUND", "never"))?;
                let workspace = self
                    .database
                    .workspace(&session.workspace_id)?
                    .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
                let lease = self.database.active_lease_for_session(&session_id)?;
                let live = lease
                    .as_ref()
                    .is_some_and(|lease| lease.expires_at_ms >= now_ms());
                let lifecycle = match session.state.as_str() {
                    "active" if live => "active",
                    "active" | "dormant" | "orphaned" => "dormant",
                    "suspended" => "suspended",
                    _ => "released",
                };
                let materialized = workspace.path.join(".git").is_file();
                Ok(Outcome::Completed(
                    serde_json::to_value(SessionStatus {
                        session: session_id,
                        lifecycle: lifecycle.to_owned(),
                        workspace: workspace.id.clone(),
                        lease: lease.as_ref().map(|lease| lease.id.clone()),
                        lease_expires_at_ms: lease.as_ref().map(|lease| lease.expires_at_ms),
                        cwd: materialized.then(|| workspace.path.to_string_lossy().into_owned()),
                        materialized,
                    })
                    .map_err(EngineError::internal)?,
                ))
            }
            Query::Doctor => {
                let mut health = self.database.doctor()?;
                if let Value::Object(ref mut object) = health {
                    object.insert("protocol".into(), json!(PROTOCOL_VERSION));
                    object.insert("root".into(), json!(self.config.root));
                }
                Ok(Outcome::Completed(health))
            }
            Query::Context { selector } => {
                let workspace = self.resolve_workspace(&selector)?;
                Ok(Outcome::Completed(
                    serde_json::to_value(self.compact_context(&workspace).await?)
                        .map_err(EngineError::internal)?,
                ))
            }
        }
    }

    fn resolve_workspace(
        &self,
        selector: &WorkspaceSelector,
    ) -> Result<WorkspaceRecord, EngineError> {
        let workspace = if let Some(id) = &selector.workspace_id {
            self.database.workspace(id)?
        } else if let Some(cwd) = &selector.cwd {
            self.database.workspace_for_cwd(Path::new(cwd))?
        } else {
            None
        };
        workspace
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never").next("shade open"))
    }

    fn dependency_scripts(
        &self,
        workspace: &WorkspaceRecord,
    ) -> Result<Vec<shade_protocol::DependencyScript>, EngineError> {
        let mut scripts = Vec::new();
        for value in self.database.dependency_receipts(&workspace.id)? {
            let receipt: crate::dependencies::DependencyReceipt =
                serde_json::from_value(value).map_err(EngineError::internal)?;
            scripts.extend(receipt.scripts);
        }
        scripts.sort_by(|left, right| left.approval.cmp(&right.approval));
        scripts.dedup_by(|left, right| left.approval == right.approval);
        Ok(scripts)
    }

    fn repository_for(
        &self,
        workspace: &WorkspaceRecord,
    ) -> Result<(RepositoryRecord, ManagedRepository, RemoteIdentity), EngineError> {
        let record = self
            .database
            .repository_by_id(&workspace.repository_id)?
            .ok_or_else(|| EngineError::domain("REPOSITORY_NOT_FOUND", "never"))?;
        let remote = self
            .git
            .canonicalize_remote(&record.identity)
            .map_err(subsystem_error)?;
        let managed = ManagedRepository::new(record.bare_path.clone());
        Ok((record, managed, remote))
    }

    async fn keyed_lock(&self, key: String) -> OwnedMutexGuard<()> {
        self.locks.keyed_lock(key).await
    }

    async fn clone_tree_blocking(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<(), EngineError> {
        let filesystem = Arc::clone(&self.filesystem);
        let source = source.to_owned();
        let destination = destination.to_owned();
        let result =
            tokio::task::spawn_blocking(move || filesystem.clone_tree(&source, &destination))
                .await
                .map_err(|error| {
                    EngineError::internal(format!("filesystem clone task failed: {error}"))
                })?;
        result.map_err(subsystem_error)
    }

    async fn clone_base_blocking(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<(), EngineError> {
        let filesystem = Arc::clone(&self.filesystem);
        let source = source.to_owned();
        let destination = destination.to_owned();
        tokio::task::spawn_blocking(move || filesystem.clone_immutable_tree(&source, &destination))
            .await
            .map_err(|error| EngineError::internal(format!("base clone task failed: {error}")))?
            .map_err(subsystem_error)
    }

    async fn register_workspace_git_metadata(
        &self,
        repository_id: &RepositoryId,
        repository: &ManagedRepository,
        workspace: &Path,
        revision: &BaseRevision,
        reason: &str,
    ) -> Result<(), EngineError> {
        // `git worktree add` and `git worktree repair` both mutate the shared
        // administrative `worktrees/` directory. Git's per-file locks do not
        // make a concurrent repair safe, so serialize this short metadata-only
        // section per managed repository. Filesystem cloning remains parallel.
        let _guard = self
            .keyed_lock(format!("git-worktrees:{}", repository_id.0))
            .await;
        self.git
            .register_precloned_worktree(repository, workspace, revision, reason)
            .await
            .map(|_| ())
            .map_err(subsystem_error)
    }

    async fn remove_workspace_git_metadata(
        &self,
        repository_id: &RepositoryId,
        repository: &ManagedRepository,
        workspace: &Path,
    ) -> Result<(), EngineError> {
        let _guard = self
            .keyed_lock(format!("git-worktrees:{}", repository_id.0))
            .await;
        self.git
            .remove_worktree(repository, workspace, true)
            .await
            .map_err(subsystem_error)
    }

    async fn ensure_repository(
        &self,
        locator: &RepositoryLocator,
        requested_base: Option<&str>,
    ) -> Result<RepositoryHandle, EngineError> {
        if let RepositoryLocator::Registered { repository_id } = locator {
            let record = self
                .database
                .repository_by_id(repository_id)?
                .ok_or_else(|| EngineError::domain("REPOSITORY_NOT_FOUND", "never"))?;
            let managed = ManagedRepository::new(record.bare_path.clone());
            let remote = match self
                .git
                .managed_origin(&managed)
                .await
                .map_err(subsystem_error)?
            {
                Some(remote) => remote,
                None => self
                    .git
                    .canonicalize_remote(&record.identity)
                    .map_err(subsystem_error)?,
            };
            return Ok(RepositoryHandle {
                managed,
                record,
                remote,
            });
        }

        let (identity, remote, local_path) = match locator {
            RepositoryLocator::Local { path } => {
                let local = self
                    .git
                    .canonicalize_local(Path::new(path))
                    .await
                    .map_err(subsystem_error)?;
                (
                    local.canonical,
                    local.remote,
                    Some((local.worktree, local.origin_configured)),
                )
            }
            RepositoryLocator::Remote { url } => {
                let remote = self.git.canonicalize_remote(url).map_err(subsystem_error)?;
                (remote.canonical.clone(), remote, None)
            }
            RepositoryLocator::Registered { .. } => unreachable!(),
        };

        // Repository discovery participates in the same single-flight as its
        // first import. This keeps a record from becoming visible to another
        // opener in the small interval before the imported freshness proof is
        // installed below.
        let _guard = self.keyed_lock(format!("repository:{identity}")).await;
        if let Some(record) = self.database.repository_by_identity(&identity)? {
            return Ok(RepositoryHandle {
                managed: ManagedRepository::new(record.bare_path.clone()),
                record,
                remote,
            });
        }
        let digest = hex::encode(Sha256::digest(identity.as_bytes()));
        let destination = self.config.repositories_dir().join(&digest[..32]);
        if destination.is_dir() {
            let verified = self
                .git
                .run(
                    None,
                    [
                        format!("--git-dir={}", destination.display()),
                        "rev-parse".to_owned(),
                        "--is-bare-repository".to_owned(),
                    ],
                )
                .await
                .map_err(subsystem_error)?;
            if verified.stdout != "true" {
                return Err(EngineError::domain("REPOSITORY_STORE_INVALID", "never")
                    .next("run shade doctor"));
            }
            let record = self
                .database
                .upsert_repository(&identity, &destination, None)?;
            return Ok(RepositoryHandle {
                managed: ManagedRepository::new(destination),
                record,
                remote,
            });
        }
        let mut imported_freshness = None;
        let managed = if let Some((source, origin_configured)) = local_path {
            let branch = match requested_base.and_then(simple_branch) {
                Some(branch) => branch,
                None => {
                    self.git
                        .run(Some(&source), ["symbolic-ref", "--short", "HEAD"])
                        .await
                        .map_err(|_| {
                            EngineError::domain("LOCAL_IMPORT_BRANCH_REQUIRED", "never")
                                .next("pass --base <branch> or attach the source HEAD to a branch")
                        })?
                        .stdout
                }
            };
            let (managed, revision) = self
                .git
                .import_managed_bare(&source, &destination, &branch, &remote)
                .await
                .map_err(subsystem_error)?;
            // A local-only repository is its own authoritative source, so the
            // strict import proves freshness for concurrent opens. A checkout
            // with origin is merely a seed and may be stale; never cache that
            // revision as proof of upstream freshness.
            let specification_key = if origin_configured {
                None
            } else {
                match requested_base {
                    None => Some("remote-head".to_owned()),
                    Some(requested) => {
                        simple_branch(requested).map(|branch| format!("branch:{branch}"))
                    }
                }
            };
            imported_freshness = specification_key.map(|key| (key, revision));
            managed
        } else {
            self.git
                .create_managed_bare(&destination, Some(&remote))
                .await
                .map_err(subsystem_error)?
        };
        let record = self
            .database
            .upsert_repository(&identity, managed.git_dir(), None)?;
        crate::faults::hit(crate::faults::Point::RepositoryRecorded);
        if let Some((specification_key, revision)) = imported_freshness {
            // The imported tracking ref and objects are durable in the bare.
            // The monotonic timestamp only scopes reuse to opens that already
            // requested freshness before this strict import completed. A later
            // open (or a daemon restart) still performs a new strict fetch.
            self.locks.fetch_cache.lock().await.insert(
                format!("{}:{specification_key}", record.id.0),
                CachedBase {
                    at: Instant::now(),
                    revision,
                },
            );
        }
        Ok(RepositoryHandle {
            record,
            managed,
            remote,
        })
    }

    async fn fresh_base(
        &self,
        repository: &RepositoryHandle,
        requested: Option<&str>,
    ) -> Result<BaseRevision, EngineError> {
        self.fresh_base_requested_at(repository, requested, Instant::now())
            .await
    }

    async fn fresh_base_requested_at(
        &self,
        repository: &RepositoryHandle,
        requested: Option<&str>,
        requested_at: Instant,
    ) -> Result<BaseRevision, EngineError> {
        let specification = self.base_spec(&repository.managed, requested).await?;
        let specification_key = match &specification {
            BaseSpec::RemoteHead => "remote-head".to_owned(),
            BaseSpec::OriginBranch(branch) => format!("branch:{branch}"),
            BaseSpec::ExistingOid(oid) => format!("oid:{oid}"),
        };
        let key = format!("{}:{specification_key}", repository.record.id.0);
        let _guard = self.keyed_lock(format!("fetch:{key}")).await;
        if let Some(cached) = self.locks.fetch_cache.lock().await.get(&key).cloned()
            && cached.at >= requested_at
        {
            return Ok(cached.revision);
        }
        let revision = self
            .git
            .resolve_base(&repository.managed, Some(&repository.remote), specification)
            .await
            .map_err(subsystem_error)?;
        self.locks.fetch_cache.lock().await.insert(
            key,
            CachedBase {
                at: Instant::now(),
                revision: revision.clone(),
            },
        );
        Ok(revision)
    }

    async fn base_spec(
        &self,
        repository: &ManagedRepository,
        requested: Option<&str>,
    ) -> Result<BaseSpec, EngineError> {
        let Some(requested) = requested else {
            return Ok(BaseSpec::RemoteHead);
        };
        if requested.is_empty() || requested.starts_with('-') {
            return Err(EngineError::domain("BASE_INVALID", "never"));
        }
        if requested.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            let oid = Oid::new(requested).map_err(subsystem_error)?;
            if self
                .git
                .resolve_base(repository, None, BaseSpec::ExistingOid(oid.clone()))
                .await
                .is_ok_and(|revision| revision.commit.as_str().eq_ignore_ascii_case(requested))
            {
                return Ok(BaseSpec::ExistingOid(oid));
            }
        }
        let branch = requested.strip_prefix("origin/").unwrap_or(requested);
        if branch.starts_with("refs/") || branch.contains("..") {
            return Err(EngineError::domain("BASE_INVALID", "never")
                .next("use a simple branch name, origin/<branch>, or a complete local OID"));
        }
        Ok(BaseSpec::OriginBranch(branch.to_owned()))
    }

    async fn ensure_base(
        &self,
        repository: &RepositoryHandle,
        revision: &BaseRevision,
    ) -> Result<PathBuf, EngineError> {
        // OIDs are opaque but already validated as path-safe hexadecimal. By
        // retaining the complete OID as the directory key we can discover an
        // older immutable base after restart without assuming SHA-1 width or
        // maintaining a second source of truth.
        let base_pool = self.config.bases_dir().join(&repository.record.id.0);
        let destination = base_pool.join(revision.commit.as_str());
        let _guard = self
            .keyed_lock(format!("base:{}", repository.record.id.0))
            .await;
        match std::fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                return Ok(destination);
            }
            Ok(_) => {
                return Err(EngineError::internal(anyhow::anyhow!(
                    "immutable base destination is not a real directory"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(EngineError::internal(error)),
        }
        std::fs::create_dir_all(&base_pool).map_err(EngineError::internal)?;

        if let Some((previous, previous_root)) = self
            .incremental_base_candidate(repository, revision, &base_pool)
            .await?
        {
            self.prepare_incremental_base(
                repository,
                &previous,
                &previous_root,
                revision,
                &destination,
            )
            .await?;
        } else {
            self.git
                .prepare_base(&repository.managed, revision, &destination)
                .await
                .map_err(subsystem_error)?;
        }
        self.survey_base_secrets(repository, revision).await;
        Ok(destination)
    }

    /// Note what the newly admitted base tree already carries. A credential
    /// committed to a test fixture, a document or a CI file is the
    /// repository owner's decision and no longer a reason to refuse the
    /// repository, so the count and the paths are recorded once per base and
    /// reported by `doctor` instead. Never a fragment of the matching bytes,
    /// and never a reason to fail the open: an unreadable survey is a note
    /// Shade could not take, not a workspace it should refuse.
    async fn survey_base_secrets(&self, repository: &RepositoryHandle, revision: &BaseRevision) {
        let Ok(survey) = self
            .git
            .survey_tracked_secrets(&repository.managed, &revision.tree)
            .await
        else {
            return;
        };
        if survey.matched == 0 {
            return;
        }
        let _ = self.database.record_event(
            "repository.tracked_secret_matches",
            &repository.record.id.0,
            &json!({
                "repository": repository.record.id,
                "base": revision.commit.as_str(),
                "matched": survey.matched,
                "paths": survey
                    .paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            }),
        );
    }

    async fn incremental_base_candidate(
        &self,
        repository: &RepositoryHandle,
        requested: &BaseRevision,
        base_pool: &Path,
    ) -> Result<Option<(BaseRevision, PathBuf)>, EngineError> {
        let entries = match std::fs::read_dir(base_pool) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(EngineError::internal(error)),
        };
        let mut candidates = Vec::new();
        for entry in entries {
            let entry = entry.map_err(EngineError::internal)?;
            let file_type = entry.file_type().map_err(EngineError::internal)?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(oid) = Oid::new(name) else {
                continue;
            };
            if oid == requested.commit {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((modified, oid, entry.path()));
        }
        candidates.sort_by(|left, right| right.0.cmp(&left.0));

        for (_, oid, path) in candidates {
            let Ok(base) = self
                .git
                .resolve_base(&repository.managed, None, BaseSpec::ExistingOid(oid))
                .await
            else {
                continue;
            };
            if self
                .git
                .verify_materialized_tree(&repository.managed, &base.tree, &path)
                .await
                .is_ok()
            {
                return Ok(Some((base, path)));
            }
            tracing::warn!(
                repository = %repository.record.id,
                path = %path.display(),
                "ignoring corrupt immutable base while selecting an incremental source"
            );
        }
        Ok(None)
    }

    async fn prepare_incremental_base(
        &self,
        repository: &RepositoryHandle,
        previous: &BaseRevision,
        previous_root: &Path,
        next: &BaseRevision,
        destination: &Path,
    ) -> Result<(), EngineError> {
        let parent = destination.parent().ok_or_else(|| {
            EngineError::internal(anyhow::anyhow!("base destination needs a parent"))
        })?;
        let staging = parent.join(format!(
            ".shade-materialize-{}",
            ulid::Ulid::new().to_string().to_ascii_lowercase()
        ));
        self.clone_base_blocking(previous_root, &staging).await?;

        let updated = self
            .git
            .update_materialized_base(&repository.managed, previous, next, &staging)
            .await;
        if let Err(error) = updated {
            let _ = self.filesystem.remove_tree(&staging);
            return Err(subsystem_error(error));
        }
        if let Err(error) = self.filesystem.publish_tree(&staging, destination) {
            let _ = self.filesystem.remove_tree(&staging);
            return Err(EngineError::internal(error));
        }
        crate::faults::hit(crate::faults::Point::IncrementalBasePromoted);
        Ok(())
    }

    fn workspace_record(
        &self,
        repository_id: RepositoryId,
        base: &BaseRevision,
        predecessor: Option<WorkspaceId>,
    ) -> WorkspaceRecord {
        let id = WorkspaceId(format!("ws_{}", ulid::Ulid::new()));
        WorkspaceRecord {
            path: self.config.workspaces_dir().join(&id.0),
            id,
            repository_id,
            session_id: None,
            base_ref: display_base_ref(base),
            base_oid: ObjectId(base.commit.to_string()),
            head_oid: ObjectId(base.commit.to_string()),
            state: "materializing".into(),
            predecessor_id: predecessor,
            dependency_state: "preparing".into(),
        }
    }

    async fn prepare_dependencies(
        &self,
        workspace: &WorkspaceRecord,
    ) -> Result<Vec<crate::dependencies::DependencyReceipt>, EngineError> {
        self.prepare_dependencies_with_snapshot(workspace, None)
            .await
    }

    async fn prepare_dependencies_with_snapshot(
        &self,
        workspace: &WorkspaceRecord,
        inherited: Option<DependencySnapshot>,
    ) -> Result<Vec<crate::dependencies::DependencyReceipt>, EngineError> {
        // Parallel preparations retain their layers until every workspace
        // receipt is durable. GC cannot discover protection before this point.
        let _artifacts = self.locks.dependency_artifacts.read().await;
        let _policy = self.locks.script_policy.read().await;
        let script_approvals = self.database.script_approvals()?;
        let workspace_root = workspace.path.clone();
        let cache_root = self.config.root.clone();
        let runtime_root = self.config.runtime_dir();
        let filesystem = self.filesystem.clone();
        let dependencies = self.dependencies.clone();
        // Providers intentionally use synchronous host package managers and
        // APFS clone syscalls. Run their complete readiness transaction on the
        // blocking pool so an open cannot starve IPC, events or heartbeats.
        let receipts = tokio::task::spawn_blocking(move || {
            let context = ReadinessContext::with_filesystem(
                &workspace_root,
                &workspace_root,
                &cache_root,
                &runtime_root,
                filesystem.as_ref(),
            )
            .with_script_approvals(&script_approvals);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| DependencyError::Failed(error.to_string()))?;
            runtime.block_on(async {
                match inherited {
                    Some(snapshot) => {
                        dependencies
                            .inherit_workspace(&context, &snapshot.source_root, snapshot.receipts)
                            .await
                    }
                    None => dependencies.ensure_all(&context).await,
                }
            })
        })
        .await
        .map_err(EngineError::internal)?
        .map_err(dependency_error)?;
        for receipt in &receipts {
            self.database.record_dependency_receipt(
                &workspace.id,
                &receipt.provider,
                &receipt.fingerprint,
                None,
                &serde_json::to_value(receipt).map_err(EngineError::internal)?,
            )?;
            crate::faults::hit(crate::faults::Point::DependencyReceiptRecorded);
        }
        self.database.set_dependency_state(&workspace.id, "ready")?;
        crate::faults::hit(crate::faults::Point::DependenciesRecorded);
        Ok(receipts)
    }

    async fn compact_context(
        &self,
        workspace: &WorkspaceRecord,
    ) -> Result<CompactContext, EngineError> {
        let session = workspace
            .session_id
            .clone()
            .ok_or_else(|| EngineError::domain("WORKSPACE_SESSION_UNAVAILABLE", "safe"))?;
        let lease = self.database.active_lease_for_session(&session)?;
        // `lease` keeps its historical `live | expired | released` domain for
        // callers that already branch on it; `lifecycle` is the new, coarser
        // truth that says whether the work is still there.
        let lease_state = match lease {
            Some(lease) if lease.expires_at_ms >= now_ms() => "live",
            Some(_) => "expired",
            None => "released",
        };
        let lifecycle = self.workspace_lifecycle(workspace)?;
        // A suspended workspace has no tree to interrogate. Report the last
        // thing recorded about it instead of failing a read-only query.
        if matches!(lifecycle, Lifecycle::Suspended) {
            return Ok(CompactContext {
                workspace: workspace.id.clone(),
                session,
                base_ref: workspace.base_ref.clone(),
                base_sha: workspace.base_oid.clone(),
                head_sha: workspace.head_oid.clone(),
                remote_sha: None,
                changes: CompactChanges {
                    staged: 0,
                    unstaged: 0,
                    untracked: 0,
                },
                lease: lease_state.to_owned(),
                lifecycle: lifecycle.label().to_owned(),
                dependencies: self.dependency_context(workspace)?,
            });
        }
        self.compact_context_for(workspace, session, lease_state, lifecycle.label())
            .await
    }

    /// The dependency half of a compact context, which is answerable from
    /// receipts alone and therefore survives losing the tree.
    fn dependency_context(
        &self,
        workspace: &WorkspaceRecord,
    ) -> Result<ProtocolDependencyContext, EngineError> {
        let receipts = self.database.dependency_receipts(&workspace.id)?;
        let providers = receipts
            .iter()
            .filter_map(|value| {
                value
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        let mut blocked_builds = receipts
            .iter()
            .filter_map(|value| value.get("blocked_builds").and_then(Value::as_array))
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        blocked_builds.sort();
        blocked_builds.dedup();
        Ok(ProtocolDependencyContext {
            state: workspace.dependency_state.clone(),
            providers,
            blocked_builds,
        })
    }

    async fn compact_context_for(
        &self,
        workspace: &WorkspaceRecord,
        session: SessionId,
        lease_state: &str,
        lifecycle: &str,
    ) -> Result<CompactContext, EngineError> {
        // `context` is observational: one porcelain-v2 invocation returns
        // both the counters and detached HEAD. Content retention boundaries
        // (checkpoint/fork/sync/publish/release/GC) keep using the fully
        // policy-bearing GitStore methods.
        let (status, head) = self
            .git
            .context_state(&workspace.path)
            .await
            .map_err(subsystem_error)?;
        let dependencies = self.dependency_context(workspace)?;
        Ok(CompactContext {
            workspace: workspace.id.clone(),
            session,
            base_ref: workspace.base_ref.clone(),
            base_sha: workspace.base_oid.clone(),
            head_sha: ObjectId(head.to_string()),
            remote_sha: None,
            changes: CompactChanges {
                staged: saturating_u32(status.staged),
                unstaged: saturating_u32(status.unstaged),
                untracked: saturating_u32(status.untracked),
            },
            lease: lease_state.to_owned(),
            lifecycle: lifecycle.to_owned(),
            dependencies,
        })
    }

    async fn opened_session(
        &self,
        session_id: SessionId,
        workspace: &WorkspaceRecord,
        lease: crate::db::LeaseRecord,
    ) -> Result<OpenedSession, EngineError> {
        self.opened_session_with_lease(session_id, workspace, lease.id)
            .await
    }

    async fn opened_session_with_lease(
        &self,
        session_id: SessionId,
        workspace: &WorkspaceRecord,
        lease_id: LeaseId,
    ) -> Result<OpenedSession, EngineError> {
        let mut ready_workspace = workspace.clone();
        ready_workspace.session_id = Some(session_id.clone());
        ready_workspace.state = "ready".into();
        // The workspace is ready by construction -- every caller has just made
        // it so -- but its dependency state is a fact about the tree, not an
        // assumption. Forcing `ready` told a caller reattaching a workspace
        // whose scripts are blocked that its dependencies were installed.
        if let Some(current) = self.database.workspace(&workspace.id)? {
            ready_workspace.dependency_state = current.dependency_state;
        }
        let cwd = ready_workspace.path.to_string_lossy().into_owned();
        let mut env = BTreeMap::new();
        env.insert("SHADE_SESSION".into(), session_id.0.clone());
        env.insert("SHADE_WORKSPACE".into(), ready_workspace.id.0.clone());
        env.insert("SHADE_LEASE".into(), lease_id.0.clone());
        env.insert(
            "SHADE_SOCKET".into(),
            self.config.socket.to_string_lossy().into_owned(),
        );
        let compact_context = self
            .compact_context_for(&ready_workspace, session_id.clone(), "live", "active")
            .await?;
        Ok(OpenedSession {
            session: session_id,
            workspace: ready_workspace.id.clone(),
            lease: lease_id,
            cwd,
            env,
            compact_context,
            // The daemon has no keepalive of its own to report; the CLI fills
            // this in for the process it actually spawned.
            keepalive: None,
        })
    }

    async fn open_session(
        &self,
        request: OpenSession,
        operation: &OperationId,
    ) -> Result<Outcome, EngineError> {
        if self.database.session(&request.session_id)?.is_some() {
            // A suspension the daemon was killed in the middle of leaves the
            // session row saying `active` and the workspace saying
            // `suspending`, which routes an `open` into a reattach that finds
            // no tree. Settle it first, then read the session the settlement
            // left behind.
            self.settle_suspending_session(&request.session_id).await?;
        }
        if let Some(existing) = self.database.session(&request.session_id)? {
            // `open` is the single door: an id the host already used resumes
            // the work behind it rather than failing, so a host that lost its
            // lease never has to know that dormancy exists.
            return match existing.state.as_str() {
                "active" | "dormant" | "orphaned" => {
                    self.reattach_session(request.session_id, request.base.as_deref(), operation)
                        .await
                }
                // `open` is still the single door: a suspended session is
                // woken rather than refused, so a host that only ever calls
                // `open` never has to know that suspension exists either.
                "suspended" => self.wake_session(request.session_id, operation).await,
                _ => Err(EngineError::domain("SESSION_ALREADY_RELEASED", "never")
                    .next("open with a new stable session id")),
            };
        }

        let freshness_requested_at = Instant::now();
        let repository = self
            .ensure_repository(&request.repository, request.base.as_deref())
            .await?;
        let base = self
            .fresh_base_requested_at(&repository, request.base.as_deref(), freshness_requested_at)
            .await?;
        let base_root = self.ensure_base(&repository, &base).await?;
        let workspace = self.workspace_record(repository.record.id.clone(), &base, None);
        self.database.create_workspace(&workspace)?;
        crate::faults::hit(crate::faults::Point::WorkspaceRecorded);
        self.database
            .bind_operation_resource(operation, &workspace.id.0, "materialize")?;
        crate::faults::hit(crate::faults::Point::WorkspaceBound);
        let result = async {
            self.clone_base_blocking(&base_root, &workspace.path)
                .await?;
            crate::faults::hit(crate::faults::Point::WorkspaceCloned);
            self.register_workspace_git_metadata(
                &repository.record.id,
                &repository.managed,
                &workspace.path,
                &base,
                &format!("Shade session {}", request.session_id.0),
            )
            .await?;
            self.database
                .bind_operation_resource(operation, &workspace.id.0, "dependencies")?;
            self.prepare_dependencies(&workspace).await?;
            self.secrets
                .capture(&workspace.id, &workspace.path)
                .map_err(EngineError::internal)?;
            crate::faults::hit(crate::faults::Point::SecretsCaptured);
            let session = SessionRecord {
                id: request.session_id.clone(),
                repository_id: repository.record.id.clone(),
                workspace_id: workspace.id.clone(),
                intent: request.intent,
                state: "active".into(),
            };
            let lease = self
                .database
                .create_session_and_lease(&session, self.config.lease_ttl_secs)?;
            crate::faults::hit(crate::faults::Point::SessionActivated);
            self.opened_session(request.session_id, &workspace, lease)
                .await
        }
        .await;
        match result {
            Ok(opened) => Ok(Outcome::Completed(
                serde_json::to_value(opened).map_err(EngineError::internal)?,
            )),
            Err(error) => {
                let _ = self.database.mark_workspace_state(&workspace.id, "failed");
                Err(error)
            }
        }
    }

    /// Resume a session on the workspace it already owns.
    ///
    /// Reattach is deliberately not a materialization path: the tree, the
    /// dependency layer and the secret baseline all survived the lease expiry,
    /// so the whole state change is the one SQLite transaction in
    /// `Database::reattach_session` and a crash on either side of it leaves a
    /// consistent Dormant or Active session.
    async fn reattach_session(
        &self,
        session_id: SessionId,
        base: Option<&str>,
        operation: &OperationId,
    ) -> Result<Outcome, EngineError> {
        let session = self
            .database
            .session(&session_id)?
            .ok_or_else(|| EngineError::domain("SESSION_NOT_FOUND", "never"))?;
        let workspace = self
            .database
            .workspace(&session.workspace_id)?
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
        let _lifecycle = self
            .keyed_lock(format!("lifecycle:{}", workspace.id.0))
            .await;
        // Re-read under the lock: a concurrent release or reattach may have
        // moved the session between the dispatch above and this point.
        let workspace = self
            .database
            .workspace(&session.workspace_id)?
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
        self.settle_suspending_workspace(&workspace).await;
        let workspace = self
            .database
            .workspace(&session.workspace_id)?
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
        let session = self
            .database
            .session(&session_id)?
            .ok_or_else(|| EngineError::domain("SESSION_NOT_FOUND", "never"))?;
        // The workspace decides this, not the session row. A settlement that
        // could not finish leaves the pair disagreeing -- workspace
        // `suspending` or `suspended`, session still `active` -- and the
        // caller is owed the command that ends it rather than the
        // `WORKSPACE_NOT_MATERIALIZED` the missing tree would produce.
        if matches!(workspace.state.as_str(), "suspended" | "suspending") {
            return Err(
                EngineError::domain("SESSION_SUSPENDED", "never").next("shade wake --session <id>")
            );
        }
        match session.state.as_str() {
            "active" | "dormant" | "orphaned" => {}
            "suspended" => {
                return Err(EngineError::domain("SESSION_SUSPENDED", "never")
                    .next("shade wake --session <id>"));
            }
            _ => {
                return Err(EngineError::domain("SESSION_ALREADY_RELEASED", "never")
                    .next("open with a new stable session id"));
            }
        }
        // A resumed session keeps the base it was cut from. Silently ignoring
        // a different `--base` would hand the caller a workspace that is not
        // what they asked for.
        if let Some(requested) = base
            && !base_matches(requested, &workspace.base_ref)
        {
            return Err(
                EngineError::domain("SESSION_BASE_MISMATCH", "never").next(format!(
                    "shade fork --session <new-id> (this session is on {})",
                    workspace.base_ref
                )),
            );
        }
        if !workspace.path.join(".git").is_file() {
            return Err(EngineError::domain("WORKSPACE_NOT_MATERIALIZED", "never")
                .next("shade release and open a new session"));
        }
        self.database
            .bind_operation_resource(operation, &workspace.id.0, "reattach")?;
        let lease = self.database.reattach_session(
            &session_id,
            &workspace.id,
            &LeaseId(format!("lease_{}", ulid::Ulid::new())),
            self.config.lease_ttl_secs,
        )?;
        Ok(Outcome::Completed(
            serde_json::to_value(self.opened_session(session_id, &workspace, lease).await?)
                .map_err(EngineError::internal)?,
        ))
    }

    async fn warm(
        &self,
        locator: RepositoryLocator,
        operation: &OperationId,
    ) -> Result<Outcome, EngineError> {
        let freshness_requested_at = Instant::now();
        let repository = self.ensure_repository(&locator, None).await?;
        self.database
            .bind_operation_resource(operation, &repository.record.id.0, "fetch")?;
        let base = self
            .fresh_base_requested_at(&repository, None, freshness_requested_at)
            .await?;
        let root = self.ensure_base(&repository, &base).await?;
        Ok(Outcome::Completed(json!({
            "repository": repository.record.id,
            "base_sha": base.commit.to_string(),
            "base_ref": display_base_ref(&base),
            "base_ready": root.is_dir(),
        })))
    }

    async fn checkpoint_workspace(
        &self,
        workspace: &WorkspaceRecord,
        reason: &str,
        operation: &OperationId,
    ) -> Result<CheckpointRecord, EngineError> {
        let _guard = self
            .keyed_lock(format!("workspace:{}", workspace.id.0))
            .await;
        self.database
            .bind_operation_resource(operation, &workspace.id.0, "checkpoint")?;
        let (_, repository, _) = self.repository_for(workspace)?;
        let checkpoint_id = CheckpointId(format!("ckpt_{}", ulid::Ulid::new()));
        let checkpoint = self
            .git
            .checkpoint(
                &repository,
                &workspace.path,
                &workspace.id.0,
                &checkpoint_id.0,
            )
            .await
            .map_err(subsystem_error)?;
        let record = checkpoint_record(&checkpoint_id, &workspace.id, reason, &checkpoint);
        self.database.create_checkpoint(&record)?;
        crate::faults::hit(crate::faults::Point::CheckpointRecorded);
        self.database
            .set_workspace_head(&workspace.id, &record.head_oid)?;
        crate::faults::hit(crate::faults::Point::CheckpointHeadRecorded);
        Ok(record)
    }

    async fn load_git_checkpoint(
        &self,
        record: &CheckpointRecord,
    ) -> Result<(WorkspaceRecord, ManagedRepository, Checkpoint), EngineError> {
        let workspace = self
            .database
            .workspace(&record.workspace_id)?
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
        let (_, repository, _) = self.repository_for(&workspace)?;
        let checkpoint = self
            .git
            .load_checkpoint(&repository, &workspace.id.0, &record.id.0)
            .await
            .map_err(subsystem_error)?;
        Ok((workspace, repository, checkpoint))
    }

    async fn fork_workspace(
        &self,
        selector: WorkspaceSelector,
        child_session_id: SessionId,
        intent: Option<String>,
        operation: &OperationId,
    ) -> Result<Outcome, EngineError> {
        if self.database.session(&child_session_id)?.is_some() {
            return Err(EngineError::domain("SESSION_ALREADY_EXISTS", "never"));
        }
        let parent = self.resolve_workspace(&selector)?;
        let _lifecycle = self.keyed_lock(format!("lifecycle:{}", parent.id.0)).await;
        self.require_live_lease(&parent)?;
        let checkpoint_record = self
            .checkpoint_workspace(&parent, "fork", operation)
            .await?;
        let (_, repository, checkpoint) = self.load_git_checkpoint(&checkpoint_record).await?;
        let head_revision = self
            .git
            .resolve_base(
                &repository,
                None,
                BaseSpec::ExistingOid(checkpoint.head.clone()),
            )
            .await
            .map_err(subsystem_error)?;
        let mut child = self.workspace_record(
            parent.repository_id.clone(),
            &head_revision,
            Some(parent.id.clone()),
        );
        child.base_ref = parent.base_ref.clone();
        child.base_oid = parent.base_oid.clone();
        child.head_oid = ObjectId(checkpoint.head.to_string());
        self.database.create_workspace(&child)?;
        self.database
            .bind_operation_resource(operation, &child.id.0, "fork_clone")?;
        crate::faults::hit(crate::faults::Point::ForkRecorded);
        let result = async {
            self.clone_tree_blocking(&parent.path, &child.path).await?;
            crate::faults::hit(crate::faults::Point::ForkCloned);
            remove_private_git_pointer(&child.path)?;
            self.register_workspace_git_metadata(
                &parent.repository_id,
                &repository,
                &child.path,
                &head_revision,
                &format!("Shade fork {}", child_session_id.0),
            )
            .await?;
            self.git
                .restore_checkpoint(&child.path, &checkpoint, true)
                .await
                .map_err(subsystem_error)?;
            self.secrets
                .copy_workspace_secrets(&parent.path, &child.path)
                .map_err(EngineError::internal)?;
            crate::faults::hit(crate::faults::Point::ForkRestored);
            let inherited = self
                .database
                .dependency_receipts(&parent.id)?
                .into_iter()
                .map(serde_json::from_value)
                .collect::<Result<Vec<_>, _>>()
                .map_err(EngineError::internal)?;
            self.prepare_dependencies_with_snapshot(
                &child,
                Some(DependencySnapshot {
                    source_root: parent.path.clone(),
                    receipts: inherited,
                }),
            )
            .await?;
            self.secrets
                .capture(&child.id, &child.path)
                .map_err(EngineError::internal)?;
            crate::faults::hit(crate::faults::Point::ForkSecretsCaptured);
            let session = SessionRecord {
                id: child_session_id.clone(),
                repository_id: child.repository_id.clone(),
                workspace_id: child.id.clone(),
                intent,
                state: "active".into(),
            };
            let lease = self
                .database
                .create_session_and_lease(&session, self.config.lease_ttl_secs)?;
            crate::faults::hit(crate::faults::Point::ForkActivated);
            self.opened_session(child_session_id, &child, lease).await
        }
        .await;
        match result {
            Ok(opened) => Ok(Outcome::Completed(
                serde_json::to_value(opened).map_err(EngineError::internal)?,
            )),
            Err(error) => {
                let _ = self.database.mark_workspace_state(&child.id, "failed");
                Err(error)
            }
        }
    }

    /// Classify a workspace without demanding exclusivity. A lease that names
    /// another workspace is still a fence — the caller is holding a stale
    /// handle, which is a different failure from simply having gone idle.
    fn workspace_lifecycle(&self, workspace: &WorkspaceRecord) -> Result<Lifecycle, EngineError> {
        // Only the two states a caller reached on purpose are Released.
        // `failed` is something reconciliation decided about a workspace whose
        // owner may still be holding a live lease on it; calling that
        // "released" took `release` and `checkpoint` away from the one caller
        // able to salvage the work, and reported data the tree may still hold
        // as gone. It stays collectible either way.
        if matches!(workspace.state.as_str(), "released" | "retained") {
            return Ok(Lifecycle::Released);
        }
        // `suspending` is a suspension the daemon was killed in the middle of.
        // Its tree may already be gone, which is exactly what `is_dematerialized`
        // says about it, so it answers like the suspension it is about to be
        // rather than like a workspace with a tree to mutate.
        if matches!(workspace.state.as_str(), "suspended" | "suspending") {
            return Ok(Lifecycle::Suspended);
        }
        let session_id = workspace
            .session_id
            .as_ref()
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_LEASED", "never"))?;
        let Some(lease) = self.database.active_lease_for_session(session_id)? else {
            return Ok(Lifecycle::Dormant);
        };
        if lease.workspace_id != workspace.id {
            return Err(EngineError::domain("LEASE_FENCED", "never")
                .next("use the current successor workspace"));
        }
        if lease.expires_at_ms < now_ms() {
            // The sweep has not run yet; the session is already dormant in
            // every sense that matters to a caller.
            return Ok(Lifecycle::Dormant);
        }
        Ok(Lifecycle::Active { lease })
    }

    /// The gate every mutation keeps. Dormancy is recoverable, so it names the
    /// command that recovers it instead of reading as data loss.
    fn require_live_lease(&self, workspace: &WorkspaceRecord) -> Result<LeaseRecord, EngineError> {
        match self.workspace_lifecycle(workspace)? {
            Lifecycle::Active { lease } => Ok(lease),
            Lifecycle::Dormant => {
                let session = workspace
                    .session_id
                    .as_ref()
                    .map(|session| session.0.as_str())
                    .unwrap_or("<id>");
                Err(EngineError::domain("LEASE_EXPIRED", "never")
                    .next(format!("shade attach --session {session}")))
            }
            Lifecycle::Suspended => {
                Err(EngineError::domain("SESSION_SUSPENDED", "never")
                    .next("shade wake --session <id>"))
            }
            // A released workspace has no lease either; keep the code every
            // SDK already treats as terminal rather than minting a new one.
            Lifecycle::Released => {
                Err(EngineError::domain("LEASE_EXPIRED", "never").next("open a new session"))
            }
        }
    }

    async fn sync_workspace(
        &self,
        selector: WorkspaceSelector,
        operation: &OperationId,
        actor_id: &str,
    ) -> Result<Outcome, EngineError> {
        let parent = self.resolve_workspace(&selector)?;
        let _lifecycle = self.keyed_lock(format!("lifecycle:{}", parent.id.0)).await;
        self.require_live_lease(&parent)?;
        let checkpoint_record = self
            .checkpoint_workspace(&parent, "sync", operation)
            .await?;
        let (_, managed, checkpoint) = self.load_git_checkpoint(&checkpoint_record).await?;
        let (record, _, remote) = self.repository_for(&parent)?;
        let repository = RepositoryHandle {
            record,
            managed,
            remote,
        };
        let original_base = Oid::new(parent.base_oid.0.clone()).map_err(subsystem_error)?;
        let requested = stored_base_request(&parent.base_ref);
        let new_base = self.fresh_base(&repository, requested.as_deref()).await?;
        let base_root = self.ensure_base(&repository, &new_base).await?;
        let successor = self.workspace_record(
            parent.repository_id.clone(),
            &new_base,
            Some(parent.id.clone()),
        );
        self.database.create_workspace(&successor)?;
        self.database
            .bind_operation_resource(operation, &successor.id.0, "sync_materialize")?;
        crate::faults::hit(crate::faults::Point::SyncRecorded);
        let result = async {
            self.clone_base_blocking(&base_root, &successor.path)
                .await?;
            self.register_workspace_git_metadata(
                &parent.repository_id,
                &repository.managed,
                &successor.path,
                &new_base,
                "Shade sync successor",
            )
            .await?;
            self.secrets
                .copy_workspace_secrets(&parent.path, &successor.path)
                .map_err(EngineError::internal)?;
            let integration = self
                .git
                .integrate_checkpoint(
                    &repository.managed,
                    &successor.path,
                    &original_base,
                    &new_base,
                    &checkpoint,
                )
                .await
                .map_err(subsystem_error)?;
            crate::faults::hit(crate::faults::Point::SyncIntegrated);
            self.secrets
                .capture(&successor.id, &successor.path)
                .map_err(EngineError::internal)?;
            if !integration.clean {
                self.database
                    .set_dependency_state(&successor.id, "blocked")?;
                self.database
                    .mark_workspace_state(&successor.id, "resolution")?;
                crate::faults::hit(crate::faults::Point::SyncConflictRecorded);
                return Ok(Outcome::Conflict(ConflictOutcome {
                    operation_id: operation.clone(),
                    workspace: successor.id.clone(),
                    cwd: successor.path.to_string_lossy().into_owned(),
                    paths: integration
                        .paths
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect(),
                }));
            }
            self.prepare_dependencies(&successor).await?;
            self.prepare_handoff(&parent, &successor, operation, actor_id)
        }
        .await;
        match result {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                let _ = self.database.mark_workspace_state(&successor.id, "failed");
                Err(error)
            }
        }
    }

    async fn complete_resolution(
        &self,
        selector: WorkspaceSelector,
        operation: &OperationId,
        actor_id: &str,
    ) -> Result<Outcome, EngineError> {
        let successor = self.resolve_workspace(&selector)?;
        if successor.state != "resolution" {
            return Err(EngineError::domain("NOT_A_RESOLUTION_WORKSPACE", "never"));
        }
        let status = self
            .git
            .status(&successor.path)
            .await
            .map_err(subsystem_error)?;
        if !status.unmerged_paths.is_empty() {
            return Ok(Outcome::Conflict(ConflictOutcome {
                operation_id: operation.clone(),
                workspace: successor.id.clone(),
                cwd: successor.path.to_string_lossy().into_owned(),
                paths: status
                    .unmerged_paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
            }));
        }
        let parent_id = successor
            .predecessor_id
            .clone()
            .ok_or_else(|| EngineError::domain("RESOLUTION_PREDECESSOR_MISSING", "never"))?;
        let parent = self
            .database
            .workspace(&parent_id)?
            .ok_or_else(|| EngineError::domain("RESOLUTION_PREDECESSOR_MISSING", "never"))?;
        let _lifecycle = self.keyed_lock(format!("lifecycle:{}", parent.id.0)).await;
        self.require_live_lease(&parent)?;
        self.prepare_dependencies(&successor).await?;
        if let Some(publish) = self.database.publish_resolution(&successor.id)? {
            if publish.state != "pending" || publish.parent_workspace_id != parent.id {
                return Err(EngineError::domain("PUBLISH_RESOLUTION_INVALID", "never"));
            }
            let checkpoint = self
                .checkpoint_workspace(&successor, "publish-resolution", operation)
                .await?;
            let (_, repository, remote) = self.repository_for(&successor)?;
            let git_checkpoint = self
                .git
                .load_checkpoint(&repository, &successor.id.0, &checkpoint.id.0)
                .await
                .map_err(subsystem_error)?;
            let expected = Oid::new(publish.expected_remote_oid.0).map_err(subsystem_error)?;
            let expected_local = publish
                .expected_local_oid
                .map(|oid| Oid::new(oid.0))
                .transpose()
                .map_err(subsystem_error)?;
            let original_base = Oid::new(successor.base_oid.0.clone()).map_err(subsystem_error)?;
            let intent = PublishIntentRecord {
                operation_id: operation.clone(),
                workspace_id: successor.id.clone(),
                repository_id: successor.repository_id.clone(),
                checkpoint_id: checkpoint.id.clone(),
                source_kind: "resolution_complete".into(),
                branch: publish.branch.clone(),
                message: publish.message.clone(),
                push: publish.push,
                original_base_oid: ObjectId(original_base.to_string()),
                expected_remote_oid: Some(ObjectId(expected.to_string())),
                expected_local_oid: expected_local.as_ref().map(|oid| ObjectId(oid.to_string())),
                anchor_ref: publish_anchor(operation),
                commit_oid: None,
                tree_oid: None,
                state: "planned".into(),
                anchor_cleaned: false,
            };
            self.database.create_publish_intent(&intent)?;
            crate::faults::hit(crate::faults::Point::PublishPlanned);
            match self
                .drive_publish_intent(intent, &repository, &remote, &git_checkpoint)
                .await
            {
                Ok(prepared) => {
                    // The resolution row and publish saga complete together;
                    // the enclosing operation remains running until the
                    // successor handoff is transactionally prepared below.
                    self.database.complete_publish_intent(operation, None)?;
                    crate::faults::hit(crate::faults::Point::PublishCompleted);
                    if let Err(error) = self
                        .cleanup_publish_anchor(operation, &repository, &prepared)
                        .await
                    {
                        tracing::error!(
                            operation = %operation,
                            code = %error.code,
                            "resolved publish anchor cleanup deferred to recovery"
                        );
                    }
                }
                Err(error) => {
                    self.abort_publish(operation, &repository, Some(&remote), "failed")
                        .await;
                    return Err(error);
                }
            }
        }
        self.prepare_handoff(&parent, &successor, operation, actor_id)
    }

    async fn materialize_checkpoint_successor(
        &self,
        parent: &WorkspaceRecord,
        checkpoint_record: &CheckpointRecord,
        operation: &OperationId,
        source: SuccessorSource,
        secrets: SuccessorSecrets,
    ) -> Result<WorkspaceRecord, EngineError> {
        let (checkpoint_workspace, managed, checkpoint) =
            self.load_git_checkpoint(checkpoint_record).await?;
        if checkpoint_workspace.repository_id != parent.repository_id {
            return Err(EngineError::domain(
                "CHECKPOINT_REPOSITORY_MISMATCH",
                "never",
            ));
        }
        let head_revision = self
            .git
            .resolve_base(
                &managed,
                None,
                BaseSpec::ExistingOid(checkpoint.head.clone()),
            )
            .await
            .map_err(subsystem_error)?;
        let mut successor = self.workspace_record(
            parent.repository_id.clone(),
            &head_revision,
            Some(parent.id.clone()),
        );
        successor.base_ref = checkpoint_workspace.base_ref.clone();
        successor.base_oid = checkpoint_workspace.base_oid.clone();
        successor.head_oid = ObjectId(checkpoint.head.to_string());
        self.database.create_workspace(&successor)?;
        self.database.bind_operation_resource(
            operation,
            &successor.id.0,
            "successor_materialize",
        )?;
        crate::faults::hit(crate::faults::Point::SuccessorRecorded);
        self.injected_failure(crate::faults::Point::SuccessorRecorded)?;
        let result = async {
            match source {
                SuccessorSource::ParentWorkspace => {
                    self.clone_tree_blocking(&parent.path, &successor.path)
                        .await?;
                    remove_private_git_pointer(&successor.path)?;
                }
                SuccessorSource::ImmutableBase => {
                    let (record, _, remote) = self.repository_for(&checkpoint_workspace)?;
                    let base_commit = Oid::new(checkpoint_workspace.base_oid.0.clone())
                        .map_err(subsystem_error)?;
                    let base_revision = self
                        .git
                        .resolve_base(&managed, None, BaseSpec::ExistingOid(base_commit))
                        .await
                        .map_err(subsystem_error)?;
                    let handle = RepositoryHandle {
                        record,
                        managed: managed.clone(),
                        remote,
                    };
                    let base_root = self.ensure_base(&handle, &base_revision).await?;
                    self.clone_base_blocking(&base_root, &successor.path)
                        .await?;
                }
            }
            self.register_workspace_git_metadata(
                &parent.repository_id,
                &managed,
                &successor.path,
                &head_revision,
                "Shade checkpoint successor",
            )
            .await?;
            self.git
                .restore_checkpoint(&successor.path, &checkpoint, true)
                .await
                .map_err(subsystem_error)?;
            crate::faults::hit(crate::faults::Point::SuccessorRestored);
            match secrets {
                SuccessorSecrets::ParentTree => {
                    self.secrets
                        .copy_workspace_secrets(&parent.path, &successor.path)
                        .map_err(EngineError::internal)?;
                }
                SuccessorSecrets::SuspensionVault => {
                    self.secrets
                        .restore_suspension(&parent.id, &successor.path)
                        .map_err(EngineError::internal)?;
                }
            }
            self.prepare_dependencies(&successor).await?;
            self.secrets
                .capture(&successor.id, &successor.path)
                .map_err(EngineError::internal)?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let _ = self.database.mark_workspace_state(&successor.id, "failed");
            return Err(error);
        }
        Ok(successor)
    }

    async fn restore_workspace(
        &self,
        selector: WorkspaceSelector,
        checkpoint_id: CheckpointId,
        operation: &OperationId,
        actor_id: &str,
    ) -> Result<Outcome, EngineError> {
        let parent = self.resolve_workspace(&selector)?;
        let _lifecycle = self.keyed_lock(format!("lifecycle:{}", parent.id.0)).await;
        self.require_live_lease(&parent)?;
        let checkpoint = self
            .database
            .checkpoint(&checkpoint_id)?
            .ok_or_else(|| EngineError::domain("CHECKPOINT_NOT_FOUND", "never"))?;
        if checkpoint.state != "ready" {
            return Err(EngineError::domain("CHECKPOINT_NOT_READY", "safe"));
        }
        let successor = self
            .materialize_checkpoint_successor(
                &parent,
                &checkpoint,
                operation,
                SuccessorSource::ImmutableBase,
                SuccessorSecrets::ParentTree,
            )
            .await?;
        self.prepare_handoff(&parent, &successor, operation, actor_id)
    }

    /// Check a workspace in: keep every record, give the disk back.
    ///
    /// Sleep is the opposite of release, not a softer version of it. Release
    /// is the only door to deletion; sleep is the door to costing nothing
    /// while still existing. What it gives up is the gitignored output that is
    /// not a dependency layer -- `target/`, `dist/`, caches -- because the
    /// sleep checkpoint records what Git tracks, the vault records the private
    /// files, and nothing records the rest.
    /// Put one workspace to sleep.
    ///
    /// `require_dormant` separates the two callers. `shade sleep` is a person
    /// or an agent naming this workspace on purpose, and it may sleep a
    /// workspace it is actively holding. The idle sweep named nothing: it
    /// picked this workspace off a list of dormant records and may have been
    /// working through that list for the length of several other sleeps, so a
    /// caller that reattached in between must not have its tree deleted and its
    /// cwd unlinked underneath it. The re-read happens under the lifecycle
    /// lock, which is what makes the answer current rather than merely fresh.
    async fn sleep_workspace(
        &self,
        selector: WorkspaceSelector,
        operation: &OperationId,
        require_dormant: bool,
    ) -> Result<Outcome, EngineError> {
        let workspace = self.resolve_workspace(&selector)?;
        let _lifecycle = self
            .keyed_lock(format!("lifecycle:{}", workspace.id.0))
            .await;
        let workspace = self
            .database
            .workspace(&workspace.id)?
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
        // Sleep, like release, must keep working after a lease expires: an
        // idle workspace is exactly the one worth reclaiming disk from.
        let lifecycle = self.workspace_lifecycle(&workspace)?;
        match lifecycle {
            Lifecycle::Active { .. } | Lifecycle::Dormant => {}
            Lifecycle::Suspended => {
                return Err(EngineError::domain("SESSION_SUSPENDED", "never")
                    .next("shade wake --session <id>"));
            }
            Lifecycle::Released => {
                return Err(EngineError::domain("WORKSPACE_ALREADY_RELEASED", "never")
                    .next("open a new session"));
            }
        }
        if require_dormant && !matches!(lifecycle, Lifecycle::Dormant) {
            return Err(EngineError::domain("WORKSPACE_NOT_QUIESCENT", "safe")
                .next("shade sleep --workspace <id> to sleep it while it is held"));
        }
        let session_id = workspace
            .session_id
            .clone()
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_LEASED", "never"))?;
        self.database
            .bind_operation_resource(operation, &workspace.id.0, "sleep")?;
        // A workspace mid-handoff, mid-review or mid-publish has a second
        // owner that is still going to want the tree.
        if matches!(workspace.state.as_str(), "resolution" | "handoff_pending")
            || !self
                .database
                .workspace_is_quiescent(&workspace.id, operation)?
        {
            return Err(EngineError::domain("WORKSPACE_NOT_QUIESCENT", "safe")
                .next("finish or cancel the pending work, then shade sleep"));
        }
        if !workspace.path.join(".git").is_file() {
            return Err(EngineError::domain("WORKSPACE_NOT_MATERIALIZED", "never")
                .next("shade wake --session <id>"));
        }
        let checkpoint = self
            .checkpoint_workspace(&workspace, "sleep", operation)
            .await?;
        crate::faults::hit(crate::faults::Point::SleepCheckpointed);
        self.injected_failure(crate::faults::Point::SleepCheckpointed)?;
        // Measured while the tree is still there. `private_bytes` is the
        // honest number on APFS: blocks shared with the immutable base are not
        // reclaimed by removing this clone.
        let reclaimed_bytes = self
            .filesystem
            .usage(&workspace.path)
            .map(|usage| usage.private_bytes.unwrap_or(usage.referenced_bytes))
            .unwrap_or_default();
        // Deliberately not `secret_cleanup_review`: sleep preserves the
        // private files instead of deleting them, and there is nothing for a
        // human to decide when nothing leaves the machine.
        self.secrets
            .capture_suspension(&workspace.id, &workspace.path)
            .map_err(EngineError::internal)?;
        crate::faults::hit(crate::faults::Point::SleepSecretsVaulted);
        self.database
            .mark_workspace_state(&workspace.id, "suspending")?;
        let dematerialize = async {
            let (_, managed, _) = self.repository_for(&workspace)?;
            self.remove_workspace_git_metadata(&workspace.repository_id, &managed, &workspace.path)
                .await?;
            crate::faults::hit(crate::faults::Point::SleepRegistrationRemoved);
            self.injected_failure(crate::faults::Point::SleepRegistrationRemoved)?;
            if workspace.path.exists() {
                self.filesystem
                    .remove_tree(&workspace.path)
                    .map_err(subsystem_error)?;
            }
            self.database
                .suspend_workspace(&workspace.id, &checkpoint.id)?;
            Ok::<(), EngineError>(())
        }
        .await;
        if let Err(error) = dematerialize {
            if workspace.path.join(".git").is_file() {
                // The tree is still whole, so the honest place to leave the
                // workspace is where it was.
                let _ = self.database.restore_suspending_workspace(&workspace.id);
            } else {
                // The registration is gone, so Dormant is no longer reachable:
                // nothing can reattach to a tree with no `.git` pointer. The
                // durable checkpoint and vault are what the record has to agree
                // with, so the suspension rolls forward instead. Leaving this
                // to the reconciliation pass is what made the workspace
                // unwakeable until the next daemon start.
                let _ = self.finish_suspension(&workspace, &checkpoint.id).await;
            }
            return Err(error);
        }
        crate::faults::hit(crate::faults::Point::SleepRecorded);
        Ok(Outcome::Completed(
            serde_json::to_value(SleepResult {
                next: format!("cd to another directory, then: shade wake --session {session_id}"),
                session: session_id,
                workspace: workspace.id.clone(),
                checkpoint_id: checkpoint.id,
                suspended: true,
                reclaimed_bytes,
                cwd: workspace.path.to_string_lossy().into_owned(),
            })
            .map_err(EngineError::internal)?,
        ))
    }

    /// Rematerialize a suspended session.
    ///
    /// Wake produces a successor workspace, exactly like restore: a new
    /// workspace id and a new cwd under the same session id. That is not a
    /// compromise, it is what keeps every crash-recovery path already proven
    /// -- a half-built successor is an ordinary incomplete workspace, and the
    /// suspension it was built from is untouched until the final transaction.
    async fn wake_session(
        &self,
        session_id: SessionId,
        operation: &OperationId,
    ) -> Result<Outcome, EngineError> {
        let session = self
            .database
            .session(&session_id)?
            .ok_or_else(|| EngineError::domain("SESSION_NOT_FOUND", "never"))?;
        let workspace = self
            .database
            .workspace(&session.workspace_id)?
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
        let lifecycle_guard = self
            .keyed_lock(format!("lifecycle:{}", workspace.id.0))
            .await;
        // A suspension the daemon was killed in the middle of is finished here
        // rather than waited on: `Intent::Reconcile` is a daemon startup pass
        // an embedded host may never send, and a wake is exactly the moment to
        // pay for it. Propagated, not swallowed -- `wake` is the command the
        // caller was told to run, so its failure is the caller's answer.
        let workspace = self
            .database
            .workspace(&session.workspace_id)?
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
        if workspace.state == "suspending" {
            self.settle_suspension(&workspace).await?;
        }
        // Re-read under the lock: a concurrent sleep or release may have moved
        // the session since dispatch, and the settlement above moves both rows.
        let session = self
            .database
            .session(&session_id)?
            .ok_or_else(|| EngineError::domain("SESSION_NOT_FOUND", "never"))?;
        let workspace = self
            .database
            .workspace(&session.workspace_id)?
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
        let suspended = match session.state.as_str() {
            // `shade wake` is safe to call unconditionally: a session that
            // never slept is simply resumed where it already is.
            "active" | "dormant" | "orphaned" => false,
            "suspended" => true,
            _ => {
                return Err(EngineError::domain("SESSION_ALREADY_RELEASED", "never")
                    .next("open with a new stable session id"));
            }
        };
        if !suspended {
            // `reattach_session` takes the same lifecycle lock.
            drop(lifecycle_guard);
            return self.reattach_session(session_id, None, operation).await;
        }
        let checkpoint = self
            .database
            .suspension_checkpoint(&workspace.id)?
            .ok_or_else(|| {
                EngineError::domain("SUSPENSION_CHECKPOINT_MISSING", "safe")
                    .next("shade doctor, then release the workspace if it stays missing")
            })?;
        // Every sleep vaults the private files before it marks the workspace
        // `suspending`, and it writes a manifest even when there was nothing to
        // vault. A suspended workspace with no vault has therefore lost it to
        // something outside Shade, and waking would rebuild the tree from the
        // checkpoint with `.env.local` quietly gone -- an agent finds out when
        // the app cannot reach its database. Refuse instead, before a successor
        // exists to throw away.
        if !self.secrets.has_suspension_vault(&workspace.id) {
            return Err(EngineError::domain("SUSPENSION_VAULT_MISSING", "never").next(
                "restore the vault from backup, or shade release --session <id> to give the work up",
            ));
        }
        self.database
            .bind_operation_resource(operation, &workspace.id.0, "wake")?;
        let successor = self
            .materialize_checkpoint_successor(
                &workspace,
                &checkpoint,
                operation,
                SuccessorSource::ImmutableBase,
                SuccessorSecrets::SuspensionVault,
            )
            .await?;
        crate::faults::hit(crate::faults::Point::WakeMaterialized);
        let lease_id = LeaseId(format!("lease_{}", ulid::Ulid::new()));
        let opened = self
            .opened_session_with_lease(session_id.clone(), &successor, lease_id.clone())
            .await?;
        let outcome =
            Outcome::Completed(serde_json::to_value(opened).map_err(EngineError::internal)?);
        // Not `prepare_handoff`/`adopt_successor`: adoption requires a live
        // lease on the predecessor, and a suspended workspace has none.
        if let Err(error) = self.database.activate_woken_workspace(
            &session_id,
            &workspace.id,
            &successor.id,
            operation,
            &lease_id,
            self.config.lease_ttl_secs,
            &outcome,
        ) {
            // Only the successor is wasted; the suspension is still whole and
            // a second wake works.
            let _ = self.database.mark_workspace_state(&successor.id, "failed");
            return Err(error.into());
        }
        crate::faults::hit(crate::faults::Point::WakeActivated);
        Ok(outcome)
    }

    /// Sleep dormant workspaces idle for longer than the configured span.
    ///
    /// Off unless an operator sets `SHADE_AUTO_SLEEP_DAYS`. Candidates go
    /// through `sleep_workspace`, so every gate a manual sleep keeps applies
    /// here too, and one workspace refusing to sleep never stops the sweep.
    async fn auto_sleep_dormant(&self) -> Result<u64, EngineError> {
        let Some(after_secs) = self.config.auto_sleep_after_secs else {
            return Ok(0);
        };
        let idle_before = now_ms().saturating_sub(after_secs.saturating_mul(1000));
        let mut slept = 0_u64;
        for workspace in self
            .database
            .workspaces_in_state_before("dormant", idle_before)?
        {
            let selector = WorkspaceSelector {
                workspace_id: Some(workspace.id.clone()),
                cwd: None,
            };
            // One operation per candidate. `bind_operation_resource` sets a
            // column, so passing the sweep's own operation down meant the
            // sweep named whichever workspace it had touched last: the
            // quiescence guard that reads `operations.resource_id` protected
            // only that one, the journal recorded a single operation phasing
            // through unrelated workspaces, and a failure on the third
            // candidate would have been attributed to the whole sweep. Each
            // sleep is its own durable record now, and the sweep counts them.
            let key = format!("auto-sleep:{}:{}", workspace.id.0, ulid::Ulid::new());
            let request_hash = hex::encode(Sha256::digest(key.as_bytes()));
            let BeginOperation::New(candidate) = self.database.begin_operation(
                &operation_principal(&Actor {
                    kind: ActorKind::System,
                    id: "auto-sleep".into(),
                }),
                &key,
                &request_hash,
                "workspace_sleep",
            )?
            else {
                continue;
            };
            match self.sleep_workspace(selector, &candidate, true).await {
                Ok(outcome) => {
                    self.database.finish_operation(&candidate, &outcome)?;
                    slept += 1;
                }
                Err(error) => {
                    let _ = self
                        .database
                        .fail_operation(&candidate, &error.as_wire(), None);
                    tracing::warn!(
                        workspace = %workspace.id,
                        operation = %candidate,
                        code = %error.code,
                        "automatic sleep skipped this workspace"
                    );
                }
            }
        }
        Ok(slept)
    }

    /// Release suspended workspaces older than the configured retention.
    ///
    /// Releasing is not deleting. This only moves a workspace into the state
    /// the collector is allowed to look at; GC still applies every one of its
    /// own gates, and `orphan_grace_secs` still has to elapse afterwards.
    async fn expire_suspended_retention(&self) -> Result<u64, EngineError> {
        let Some(retention_secs) = self.config.suspended_retention_secs else {
            return Ok(0);
        };
        let suspended_before = now_ms().saturating_sub(retention_secs.saturating_mul(1000));
        let mut released = 0_u64;
        for workspace in self
            .database
            .workspaces_in_state_before("suspended", suspended_before)?
        {
            // The list is a snapshot, and a wake can land on any row in it
            // between the query and the release. Releasing that row anyway is
            // not a lost race, it is a woken session whose predecessor was
            // released underneath it: the caller sees `LEASE_FENCED` on a
            // successor it just built. Take the lock the wake takes and read
            // the record again inside it.
            let _lifecycle = self
                .keyed_lock(format!("lifecycle:{}", workspace.id.0))
                .await;
            let Some(workspace) = self.database.workspace(&workspace.id)? else {
                continue;
            };
            if workspace.state != "suspended" {
                continue;
            }
            let Some(session_id) = workspace.session_id.clone() else {
                continue;
            };
            match self.database.release_session(&session_id, &workspace.id) {
                Ok(()) => released += 1,
                Err(error) => tracing::warn!(
                    workspace = %workspace.id,
                    error = %error,
                    "suspended workspace could not be released by retention"
                ),
            }
        }
        Ok(released)
    }

    async fn refresh_dependencies(
        &self,
        selector: WorkspaceSelector,
        operation: &OperationId,
        actor_id: &str,
    ) -> Result<Outcome, EngineError> {
        let parent = self.resolve_workspace(&selector)?;
        let _lifecycle = self.keyed_lock(format!("lifecycle:{}", parent.id.0)).await;
        self.require_live_lease(&parent)?;
        let checkpoint = self
            .checkpoint_workspace(&parent, "dependencies-refresh", operation)
            .await?;
        let successor = self
            .materialize_checkpoint_successor(
                &parent,
                &checkpoint,
                operation,
                SuccessorSource::ParentWorkspace,
                SuccessorSecrets::ParentTree,
            )
            .await?;
        self.prepare_handoff(&parent, &successor, operation, actor_id)
    }

    fn prepare_handoff(
        &self,
        parent: &WorkspaceRecord,
        successor: &WorkspaceRecord,
        operation: &OperationId,
        actor_id: &str,
    ) -> Result<Outcome, EngineError> {
        let session_id = parent
            .session_id
            .clone()
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_LEASED", "never"))?;
        let handoff = self.database.prepare_successor_handoff(
            operation,
            actor_id,
            &session_id,
            &parent.id,
            &successor.id,
        )?;
        crate::faults::hit(crate::faults::Point::HandoffPrepared);
        Ok(Outcome::Completed(
            serde_json::to_value(handoff).map_err(EngineError::internal)?,
        ))
    }

    async fn adopt_successor(
        &self,
        handoff_id: HandoffId,
        operation: &OperationId,
        actor_id: &str,
    ) -> Result<Outcome, EngineError> {
        let handoff = self
            .database
            .handoff(&handoff_id)?
            .ok_or(DbError::HandoffNotFound)?;
        if handoff.actor_id != actor_id {
            return Err(DbError::HandoffOwnerMismatch.into());
        }
        let successor = self
            .database
            .workspace(&handoff.successor_workspace_id)?
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
        if handoff.state == "adopted" {
            let lease = self
                .database
                .active_lease_for_session(&handoff.session_id)?
                .filter(|lease| lease.workspace_id == successor.id)
                .ok_or(DbError::LeaseFenced)?;
            let opened = self
                .opened_session(handoff.session_id, &successor, lease)
                .await?;
            return Ok(Outcome::Completed(
                serde_json::to_value(opened).map_err(EngineError::internal)?,
            ));
        }
        if handoff.state != "pending" {
            return Err(DbError::HandoffNotPending.into());
        }
        let _lifecycle = self
            .keyed_lock(format!("lifecycle:{}", handoff.predecessor_workspace_id.0))
            .await;
        let lease_id = LeaseId(format!("lease_{}", ulid::Ulid::new()));
        let opened = self
            .opened_session_with_lease(handoff.session_id, &successor, lease_id.clone())
            .await?;
        let outcome =
            Outcome::Completed(serde_json::to_value(opened).map_err(EngineError::internal)?);
        self.database.adopt_successor_handoff(
            &handoff_id,
            actor_id,
            operation,
            &lease_id,
            self.config.lease_ttl_secs,
            &outcome,
        )?;
        crate::faults::hit(crate::faults::Point::HandoffAdopted);
        Ok(outcome)
    }

    async fn publish_workspace(
        &self,
        selector: WorkspaceSelector,
        branch: &str,
        message: &str,
        push: bool,
        operation: &OperationId,
    ) -> Result<Outcome, EngineError> {
        let parent = self.resolve_workspace(&selector)?;
        let _lifecycle = self.keyed_lock(format!("lifecycle:{}", parent.id.0)).await;
        self.require_live_lease(&parent)?;
        let checkpoint_record = self
            .checkpoint_workspace(&parent, "publish", operation)
            .await?;
        let (_, managed, checkpoint) = self.load_git_checkpoint(&checkpoint_record).await?;
        let (record, _, remote) = self.repository_for(&parent)?;
        let repository = RepositoryHandle {
            record,
            managed,
            remote,
        };
        let original_base = Oid::new(parent.base_oid.0.clone()).map_err(subsystem_error)?;
        let expected = self
            .git
            .remote_branch_oid(&repository.remote, branch)
            .await
            .map_err(subsystem_error)?;
        let expected_local = self
            .git
            .local_branch_oid(&repository.managed, branch)
            .await
            .map_err(subsystem_error)?;
        let intent = PublishIntentRecord {
            operation_id: operation.clone(),
            workspace_id: parent.id.clone(),
            repository_id: parent.repository_id.clone(),
            checkpoint_id: checkpoint_record.id.clone(),
            source_kind: "workspace_publish".into(),
            branch: branch.to_owned(),
            message: message.to_owned(),
            push,
            original_base_oid: ObjectId(original_base.to_string()),
            expected_remote_oid: expected.as_ref().map(|oid| ObjectId(oid.to_string())),
            expected_local_oid: expected_local.as_ref().map(|oid| ObjectId(oid.to_string())),
            anchor_ref: publish_anchor(operation),
            commit_oid: None,
            tree_oid: None,
            state: "planned".into(),
            anchor_cleaned: false,
        };
        self.database.create_publish_intent(&intent)?;
        crate::faults::hit(crate::faults::Point::PublishPlanned);
        match self
            .drive_publish_intent(intent, &repository.managed, &repository.remote, &checkpoint)
            .await
        {
            Ok(published) => {
                let outcome = publish_outcome(branch, push, &published);
                // Direct publish completion and operation/outbox completion are
                // one durable SQLite commit. The private anchor is released
                // only after that commit is acknowledged.
                self.database
                    .complete_publish_intent(operation, Some(&outcome))?;
                crate::faults::hit(crate::faults::Point::PublishCompleted);
                if let Err(error) = self
                    .cleanup_publish_anchor(operation, &repository.managed, &published)
                    .await
                {
                    tracing::error!(
                        operation = %operation,
                        code = %error.code,
                        "completed publish anchor cleanup deferred to recovery"
                    );
                }
                Ok(outcome)
            }
            Err(error) if error.code == "INTEGRATION_CONFLICT" => {
                self.abort_publish(operation, &repository.managed, None, "cancelled")
                    .await;
                let target = expected.ok_or_else(|| error.clone())?;
                let target_base = self
                    .git
                    .resolve_base(&repository.managed, None, BaseSpec::ExistingOid(target))
                    .await
                    .map_err(subsystem_error)?;
                self.publish_resolution(
                    &parent,
                    &repository,
                    &target_base,
                    &checkpoint,
                    PublishResolutionInput {
                        branch,
                        message,
                        push,
                        operation,
                        expected_local,
                    },
                )
                .await
            }
            Err(error) => {
                self.abort_publish(
                    operation,
                    &repository.managed,
                    Some(&repository.remote),
                    "failed",
                )
                .await;
                Err(error)
            }
        }
    }

    async fn drive_publish_intent(
        &self,
        mut intent: PublishIntentRecord,
        repository: &ManagedRepository,
        remote: &RemoteIdentity,
        checkpoint: &Checkpoint,
    ) -> Result<PreparedPublish, EngineError> {
        let original_base =
            Oid::new(intent.original_base_oid.0.clone()).map_err(subsystem_error)?;
        let expected_remote = intent
            .expected_remote_oid
            .as_ref()
            .map(|oid| Oid::new(oid.0.clone()))
            .transpose()
            .map_err(subsystem_error)?;
        let expected_local = intent
            .expected_local_oid
            .as_ref()
            .map(|oid| Oid::new(oid.0.clone()))
            .transpose()
            .map_err(subsystem_error)?;

        if intent.state == "planned" {
            let prepared = self
                .git
                .prepare_squash_publish(
                    repository,
                    PrepareSquashPublishRequest {
                        remote,
                        branch: &intent.branch,
                        original_base: &original_base,
                        expected_remote: expected_remote.as_ref(),
                        checkpoint,
                        message: &intent.message,
                        anchor_ref: &intent.anchor_ref,
                    },
                )
                .await
                .map_err(subsystem_error)?;
            self.database.mark_publish_prepared(
                &intent.operation_id,
                &ObjectId(prepared.commit.to_string()),
                &ObjectId(prepared.tree.to_string()),
            )?;
            crate::faults::hit(crate::faults::Point::PublishPrepared);
            intent.commit_oid = Some(ObjectId(prepared.commit.to_string()));
            intent.tree_oid = Some(ObjectId(prepared.tree.to_string()));
            intent.state = "prepared".into();
        }

        let commit = intent
            .commit_oid
            .as_ref()
            .ok_or_else(|| EngineError::domain("PUBLISH_JOURNAL_INVALID", "never"))?;
        let tree = intent
            .tree_oid
            .as_ref()
            .ok_or_else(|| EngineError::domain("PUBLISH_JOURNAL_INVALID", "never"))?;
        let prepared = PreparedPublish {
            previous_remote: expected_remote.clone(),
            commit: Oid::new(commit.0.clone()).map_err(subsystem_error)?,
            tree: Oid::new(tree.0.clone()).map_err(subsystem_error)?,
            target_ref: format!("refs/heads/{}", intent.branch),
            anchor_ref: intent.anchor_ref.clone(),
        };
        self.git
            .ensure_publish_anchor(repository, &prepared.anchor_ref, &prepared.commit)
            .await
            .map_err(subsystem_error)?;

        if intent.state == "prepared" {
            // Recovery must not apply a candidate prepared against a remote
            // tip which changed while the daemon was unavailable. The one
            // exception is the candidate itself, proving a remote CAS whose
            // acknowledgement was lost.
            let observed_remote = self
                .git
                .remote_branch_oid(remote, &intent.branch)
                .await
                .map_err(subsystem_error)?;
            let remote_already_applied =
                intent.push && observed_remote.as_ref() == Some(&prepared.commit);
            if !remote_already_applied && observed_remote.as_ref() != expected_remote.as_ref() {
                return Err(subsystem_error(format!(
                    "REMOTE_MOVED: expected {}, observed {}",
                    expected_remote.as_ref().map_or("absent", Oid::as_str),
                    observed_remote.as_ref().map_or("absent", Oid::as_str),
                )));
            }
            self.git
                .apply_prepared_publish_local(repository, &prepared, expected_local.as_ref())
                .await
                .map_err(subsystem_error)?;
            crate::faults::hit(crate::faults::Point::PublishLocalApplied);
            self.database
                .mark_publish_local_applied(&intent.operation_id)?;
            crate::faults::hit(crate::faults::Point::PublishLocalRecorded);
            intent.state = "local_applied".into();
            if remote_already_applied {
                self.database
                    .mark_publish_remote_applied(&intent.operation_id)?;
                intent.state = "remote_applied".into();
            }
        }

        if intent.state == "local_applied" && intent.push {
            self.git
                .apply_prepared_publish_remote(
                    repository,
                    remote,
                    &intent.branch,
                    &prepared,
                    expected_remote.as_ref(),
                )
                .await
                .map_err(subsystem_error)?;
            crate::faults::hit(crate::faults::Point::PublishRemoteApplied);
            self.database
                .mark_publish_remote_applied(&intent.operation_id)?;
            crate::faults::hit(crate::faults::Point::PublishRemoteRecorded);
            intent.state = "remote_applied".into();
        }

        let applied = intent.state == "remote_applied"
            || (intent.state == "local_applied" && !intent.push)
            || intent.state == "completed";
        if !applied {
            return Err(EngineError::domain("PUBLISH_JOURNAL_INVALID", "never"));
        }
        Ok(prepared)
    }

    async fn cleanup_publish_anchor(
        &self,
        operation: &OperationId,
        repository: &ManagedRepository,
        prepared: &PreparedPublish,
    ) -> Result<(), EngineError> {
        self.git
            .delete_publish_anchor(repository, &prepared.anchor_ref, &prepared.commit)
            .await
            .map_err(subsystem_error)?;
        crate::faults::hit(crate::faults::Point::PublishAnchorDeleted);
        self.database.mark_publish_anchor_cleaned(operation)?;
        crate::faults::hit(crate::faults::Point::PublishAnchorCleaned);
        Ok(())
    }

    async fn abort_publish(
        &self,
        operation: &OperationId,
        repository: &ManagedRepository,
        remote: Option<&RemoteIdentity>,
        state: &str,
    ) {
        let Ok(Some(intent)) = self.database.publish_intent(operation) else {
            return;
        };
        let _ = self.database.abort_publish_intent(operation, state);
        if let (Some(commit), Some(tree)) = (&intent.commit_oid, &intent.tree_oid)
            && let (Ok(commit), Ok(tree)) = (Oid::new(commit.0.clone()), Oid::new(tree.0.clone()))
        {
            let prepared = PreparedPublish {
                previous_remote: intent
                    .expected_remote_oid
                    .as_ref()
                    .and_then(|oid| Oid::new(oid.0.clone()).ok()),
                commit,
                tree,
                target_ref: format!("refs/heads/{}", intent.branch),
                anchor_ref: intent.anchor_ref.clone(),
            };
            let remote_has_commit = if intent.push {
                match remote {
                    Some(remote) => self
                        .git
                        .remote_branch_oid(remote, &intent.branch)
                        .await
                        .is_ok_and(|oid| oid.as_ref() == Some(&prepared.commit)),
                    None => false,
                }
            } else {
                false
            };
            if !remote_has_commit && intent.state != "remote_applied" {
                let expected_local = intent
                    .expected_local_oid
                    .as_ref()
                    .and_then(|oid| Oid::new(oid.0.clone()).ok());
                let _ = self
                    .git
                    .rollback_prepared_publish_local(repository, &prepared, expected_local.as_ref())
                    .await;
            }
        }
        let terminal = self
            .database
            .publish_intent(operation)
            .ok()
            .flatten()
            .unwrap_or(intent);
        let mut ignored = ReconciliationStats::default();
        self.cleanup_recovered_publish_anchor(&terminal, &mut ignored)
            .await;
    }

    async fn publish_resolution(
        &self,
        parent: &WorkspaceRecord,
        repository: &RepositoryHandle,
        target: &BaseRevision,
        checkpoint: &Checkpoint,
        input: PublishResolutionInput<'_>,
    ) -> Result<Outcome, EngineError> {
        let PublishResolutionInput {
            branch,
            message,
            push,
            operation,
            expected_local,
        } = input;
        let original_base = Oid::new(parent.base_oid.0.clone()).map_err(subsystem_error)?;
        let base_root = self.ensure_base(repository, target).await?;
        let resolution = self.workspace_record(
            parent.repository_id.clone(),
            target,
            Some(parent.id.clone()),
        );
        self.database.create_workspace(&resolution)?;
        self.database
            .bind_operation_resource(operation, &resolution.id.0, "publish_resolution")?;
        crate::faults::hit(crate::faults::Point::PublishResolutionRecorded);
        let result = async {
            self.clone_base_blocking(&base_root, &resolution.path)
                .await?;
            self.register_workspace_git_metadata(
                &parent.repository_id,
                &repository.managed,
                &resolution.path,
                target,
                "Shade publish resolution",
            )
            .await?;
            self.secrets
                .copy_workspace_secrets(&parent.path, &resolution.path)
                .map_err(EngineError::internal)?;
            let integration = self
                .git
                .integrate_checkpoint(
                    &repository.managed,
                    &resolution.path,
                    &original_base,
                    target,
                    checkpoint,
                )
                .await
                .map_err(subsystem_error)?;
            crate::faults::hit(crate::faults::Point::PublishResolutionIntegrated);
            self.secrets
                .capture(&resolution.id, &resolution.path)
                .map_err(EngineError::internal)?;
            self.database
                .create_publish_resolution(&PublishResolutionRecord {
                    workspace_id: resolution.id.clone(),
                    parent_workspace_id: parent.id.clone(),
                    branch: branch.to_owned(),
                    message: message.to_owned(),
                    push,
                    expected_remote_oid: ObjectId(target.commit.to_string()),
                    expected_local_oid: expected_local.map(|oid| ObjectId(oid.to_string())),
                    state: "pending".into(),
                })?;
            Ok::<_, EngineError>(integration)
        }
        .await;
        match result {
            Ok(integration) => Ok(Outcome::Conflict(ConflictOutcome {
                operation_id: operation.clone(),
                workspace: resolution.id.clone(),
                cwd: resolution.path.to_string_lossy().into_owned(),
                paths: integration
                    .paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
            })),
            Err(error) => {
                let _ = self.database.mark_workspace_state(&resolution.id, "failed");
                Err(error)
            }
        }
    }

    async fn release_workspace(
        &self,
        selector: WorkspaceSelector,
        operation: &OperationId,
    ) -> Result<Outcome, EngineError> {
        let workspace = self.resolve_workspace(&selector)?;
        let _lifecycle = self
            .keyed_lock(format!("lifecycle:{}", workspace.id.0))
            .await;
        // Release is the one command that must keep working after a lease
        // expires: it is the only door to deletion, so requiring an unexpired
        // lease would make idle work permanently unreleasable.
        let suspended = match self.workspace_lifecycle(&workspace)? {
            Lifecycle::Active { .. } | Lifecycle::Dormant => false,
            Lifecycle::Suspended => true,
            Lifecycle::Released => {
                return Err(EngineError::domain("WORKSPACE_ALREADY_RELEASED", "never")
                    .next("open a new session"));
            }
        };
        // A suspended workspace has no tree, so there is nothing to inspect,
        // checkpoint or scan for secrets: the sleep checkpoint and the
        // suspension vault already hold everything it had. Go straight to the
        // release transaction, which is what makes GC eligible to reclaim both.
        if suspended {
            let session_id = workspace
                .session_id
                .clone()
                .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_LEASED", "never"))?;
            self.database.release_session(&session_id, &workspace.id)?;
            crate::faults::hit(crate::faults::Point::ReleaseRecorded);
            return Ok(Outcome::Completed(json!({
                "session": session_id,
                "workspace": workspace.id,
                "checkpoint_id": self
                    .database
                    .suspension_checkpoint(&workspace.id)?
                    .map(|checkpoint| checkpoint.id),
                "released": true,
            })));
        }
        // A workspace reconciliation marked `failed` has no tree worth
        // interrogating, and `git status` against it would fail. Release is
        // the only door to deletion, so a missing tree must not close it:
        // there is nothing to checkpoint or scan, and everything already
        // captured stays captured.
        if !workspace.path.join(".git").is_file() {
            let session_id = workspace
                .session_id
                .clone()
                .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_LEASED", "never"))?;
            self.database.release_session(&session_id, &workspace.id)?;
            crate::faults::hit(crate::faults::Point::ReleaseRecorded);
            // The branch is reached by any workspace whose `.git` pointer is
            // gone, not only by one reconciliation marked `failed`, and what it
            // skips is worth a record: no status check, no release checkpoint,
            // and no secret review. A caller reading `checkpoint_id: null`
            // cannot tell a clean release apart from a tree that vanished, so
            // the event says which it was.
            let _ = self.database.record_event(
                "workspace.released_without_tree",
                &workspace.id.0,
                &json!({
                    "workspace": workspace.id,
                    "session": session_id,
                    "checkpoint_id": Value::Null,
                    "reason": "workspace_not_materialized",
                }),
            );
            return Ok(Outcome::Completed(json!({
                "session": session_id,
                "workspace": workspace.id,
                "checkpoint_id": Value::Null,
                "released": true,
            })));
        }
        let status = self
            .git
            .status(&workspace.path)
            .await
            .map_err(subsystem_error)?;
        if !status.unmerged_paths.is_empty() {
            return Err(EngineError::domain("UNMERGED_WORKTREE", "never")
                .next("resolve conflicts and run shade resolve before release"));
        }
        let head = self
            .git
            .run(
                Some(&workspace.path),
                ["rev-parse", "--verify", "HEAD^{commit}"],
            )
            .await
            .map_err(subsystem_error)?
            .stdout;
        let checkpoint = if !status.is_clean() || head != workspace.base_oid.0 {
            Some(
                self.checkpoint_workspace(&workspace, "release", operation)
                    .await?,
            )
        } else {
            None
        };
        if let Some(review) = self.secret_cleanup_review(&workspace, false)? {
            return Ok(Outcome::ReviewRequired(review));
        }
        let session_id = workspace
            .session_id
            .clone()
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_LEASED", "never"))?;
        self.database.release_session(&session_id, &workspace.id)?;
        crate::faults::hit(crate::faults::Point::ReleaseRecorded);
        Ok(Outcome::Completed(json!({
            "session": session_id,
            "workspace": workspace.id,
            "checkpoint_id": checkpoint.map(|value| value.id),
            "released": true,
        })))
    }

    fn secret_cleanup_review(
        &self,
        workspace: &WorkspaceRecord,
        respect_previous_decision: bool,
    ) -> Result<Option<ReviewRequired>, EngineError> {
        let parent = workspace
            .predecessor_id
            .as_ref()
            .map(|id| self.database.workspace(id))
            .transpose()?
            .flatten();
        let parent_path = parent.as_ref().map(|parent| parent.path.as_path());
        let mut stale = false;
        if let Some(review) = self.database.latest_secret_review(&workspace.id)? {
            let current = self
                .secrets
                .review_is_current(&workspace.id, &review.id, &workspace.path, parent_path)
                .map_err(EngineError::internal)?;
            stale = !current;
            if review.state == "pending" && current {
                return Ok(Some(ReviewRequired {
                    review_id: review.id,
                    kind: review.kind,
                    files: serde_json::from_value(review.payload["files"].clone())
                        .map_err(EngineError::internal)?,
                }));
            }
            if respect_previous_decision && current {
                return Ok(None);
            }
        }
        let preview_current = || {
            match parent_path {
                Some(path) => self.secrets.preview(&workspace.id, path, &workspace.path),
                None => self.secrets.preview_current(&workspace.id, &workspace.path),
            }
            .map_err(EngineError::internal)
        };
        let preview = preview_current()?;
        if stale || secret_preview_changed(&preview) {
            let review_id = ReviewId(format!("review_{}", ulid::Ulid::new()));
            self.secrets
                .capture_review(&workspace.id, &review_id, &workspace.path, parent_path)
                .map_err(EngineError::internal)?;
            let preview = preview_current()?;
            if !self
                .secrets
                .review_is_current(&workspace.id, &review_id, &workspace.path, parent_path)
                .map_err(EngineError::internal)?
            {
                return Err(EngineError::domain("SECRET_REVIEW_CHANGED", "safe")
                    .next("pause workspace writes and request a fresh review"));
            }
            let record = ReviewRecord {
                id: review_id.clone(),
                workspace_id: workspace.id.clone(),
                kind: "secret_cleanup".into(),
                payload: json!({
                    "workspace": workspace.id,
                    "predecessor": workspace.predecessor_id,
                    "files": preview,
                }),
                state: "pending".into(),
            };
            self.database.create_review(&record)?;
            crate::faults::hit(crate::faults::Point::ReviewCreated);
            let files = serde_json::from_value(record.payload["files"].clone())
                .map_err(EngineError::internal)?;
            return Ok(Some(ReviewRequired {
                review_id,
                kind: "secret_cleanup".into(),
                files,
            }));
        }
        Ok(None)
    }

    async fn resolve_review(
        &self,
        review_id: ReviewId,
        action: ReviewAction,
        operation: &OperationId,
        actor_id: &str,
    ) -> Result<Outcome, EngineError> {
        let review = self
            .database
            .review(&review_id)?
            .ok_or_else(|| EngineError::domain("REVIEW_NOT_FOUND", "never"))?;
        if review.state != "pending" {
            return Err(EngineError::domain("REVIEW_ALREADY_RESOLVED", "never"));
        }
        let child = self
            .database
            .workspace(&review.workspace_id)?
            .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_FOUND", "never"))?;
        let _child_lifecycle = self.keyed_lock(format!("lifecycle:{}", child.id.0)).await;
        if let Some(current) = self.secret_cleanup_review(&child, false)?
            && current.review_id != review_id
        {
            return Ok(Outcome::ReviewRequired(current));
        }
        match action {
            // Keep and discard both close the reviewed workspace's session --
            // `retained` and `released` differ only in whether the tree
            // survives for a human. The session is named on the outcome so a
            // caller that started something for the duration of that session,
            // the CLI keepalive above all, can end it here rather than leaving
            // a child heartbeating a lease nothing holds.
            ReviewAction::Discard => {
                let outcome = Outcome::Completed(json!({
                    "review": review_id,
                    "resolution": "discarded",
                    "released": child.id,
                    "session": child.session_id,
                }));
                self.database
                    .complete_secret_review(&review_id, false, operation, &outcome)?;
                Ok(outcome)
            }
            ReviewAction::Keep => {
                let outcome = Outcome::Completed(json!({
                    "review": review_id,
                    "resolution": "kept",
                    "workspace": child.id,
                    "session": child.session_id,
                }));
                self.database
                    .complete_secret_review(&review_id, true, operation, &outcome)?;
                Ok(outcome)
            }
            ReviewAction::MergeParent => {
                let child_session = child
                    .session_id
                    .clone()
                    .ok_or_else(|| EngineError::domain("WORKSPACE_NOT_LEASED", "never"))?;
                let parent_id = child.predecessor_id.clone().ok_or_else(|| {
                    EngineError::domain("SECRET_PARENT_UNAVAILABLE", "never")
                        .next("choose keep or discard")
                })?;
                let parent = self
                    .database
                    .workspace(&parent_id)?
                    .ok_or_else(|| EngineError::domain("SECRET_PARENT_UNAVAILABLE", "never"))?;
                let parent_session = self
                    .database
                    .workspace_session_owner(&parent.id)?
                    .ok_or_else(|| EngineError::domain("SECRET_PARENT_UNAVAILABLE", "never"))?;
                let same_session = parent_session == child_session;
                let _parent_lifecycle = if same_session {
                    None
                } else {
                    Some(self.keyed_lock(format!("lifecycle:{}", parent.id.0)).await)
                };
                if same_session {
                    self.require_live_lease(&child)?;
                } else {
                    self.require_live_lease(&parent)?;
                }
                if let Some(current) = self.secret_cleanup_review(&child, false)?
                    && current.review_id != review_id
                {
                    return Ok(Outcome::ReviewRequired(current));
                }
                // A reviewed automatic merge must be proven conflict-free before
                // allocating a successor. Returning the same pending review keeps
                // the user's keep/discard choices available and avoids leaking a
                // half-materialized workspace when a key has diverged on both sides.
                let preview = self
                    .secrets
                    .preview(&child.id, &parent.path, &child.path)
                    .map_err(EngineError::internal)?;
                if !self
                    .secrets
                    .can_merge(&child.id, &parent.path, &child.path)
                    .map_err(EngineError::internal)?
                {
                    return Ok(Outcome::ReviewRequired(ReviewRequired {
                        review_id,
                        kind: review.kind,
                        files: preview,
                    }));
                }
                let checkpoint = self
                    .checkpoint_workspace(&parent, "secret-merge", operation)
                    .await?;
                let successor = self
                    .materialize_checkpoint_successor(
                        &parent,
                        &checkpoint,
                        operation,
                        SuccessorSource::ParentWorkspace,
                        SuccessorSecrets::ParentTree,
                    )
                    .await?;
                self.secrets
                    .apply_reviewed(
                        &child.id,
                        &parent.path,
                        &child.path,
                        &successor.path,
                        MergeChoice::Merge,
                    )
                    .map_err(|error| match error {
                        crate::secrets::SecretError::Conflict => {
                            EngineError::domain("SECRET_CONFLICT", "never")
                                .next("choose keep or discard")
                        }
                        other => EngineError::internal(other),
                    })?;
                crate::faults::hit(crate::faults::Point::SecretsMergeApplied);
                self.secrets
                    .remove(&successor.id)
                    .map_err(EngineError::internal)?;
                crate::faults::hit(crate::faults::Point::SecretBaselineRemoved);
                self.secrets
                    .capture(&successor.id, &successor.path)
                    .map_err(EngineError::internal)?;
                crate::faults::hit(crate::faults::Point::SecretBaselineCaptured);
                let handoff = self.database.prepare_secret_successor_handoff(
                    operation,
                    actor_id,
                    &review_id,
                    &successor.id,
                )?;
                Ok(Outcome::Completed(
                    serde_json::to_value(handoff).map_err(EngineError::internal)?,
                ))
            }
        }
    }

    async fn garbage_collect(&self, operation: &OperationId) -> Result<Outcome, EngineError> {
        let grace_before = now_ms() - self.config.orphan_grace_secs * 1000;
        let candidates = self.database.gc_candidates(grace_before)?;
        let mut deleted = 0_u64;
        let mut skipped = 0_u64;
        for workspace in candidates {
            if !workspace.path.starts_with(self.config.workspaces_dir())
                || !workspace.id.0.starts_with("ws_")
            {
                skipped += 1;
                continue;
            }
            let (_, repository, _) = match self.repository_for(&workspace) {
                Ok(value) => value,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let registered_worktree = workspace.path.join(".git").is_file();
            if workspace.path.exists()
                && !matches!(self.secret_cleanup_review(&workspace, true), Ok(None))
            {
                skipped += 1;
                continue;
            }
            let mut claim_before = grace_before;
            if registered_worktree {
                let status = match self.git.status(&workspace.path).await {
                    Ok(status) => status,
                    Err(_) => {
                        skipped += 1;
                        continue;
                    }
                };
                if !status.unmerged_paths.is_empty() {
                    skipped += 1;
                    continue;
                }
                if !status.is_clean() {
                    if self
                        .checkpoint_workspace(&workspace, "gc", operation)
                        .await
                        .is_err()
                    {
                        skipped += 1;
                        continue;
                    }
                    // The candidate already passed the configured grace. A
                    // successful safety checkpoint updates its bookkeeping
                    // timestamp but does not make the released workspace live
                    // again. Allow that one known update while the claim still
                    // rechecks every durable preservation gate transactionally.
                    claim_before = now_ms().saturating_add(1);
                }
                let head = match self
                    .git
                    .run(
                        Some(&workspace.path),
                        ["rev-parse", "--verify", "HEAD^{commit}"],
                    )
                    .await
                {
                    Ok(output) => ObjectId(output.stdout),
                    Err(_) => {
                        skipped += 1;
                        continue;
                    }
                };
                if head != workspace.base_oid
                    && !self
                        .database
                        .has_ready_checkpoint_for_head(&workspace.id, &head)?
                {
                    skipped += 1;
                    continue;
                }
            }
            // Re-run every durable protection predicate transactionally and
            // claim the record before the first destructive filesystem step.
            if !self
                .database
                .claim_workspace_for_gc(&workspace.id, claim_before, operation)?
            {
                skipped += 1;
                continue;
            }
            crate::faults::hit(crate::faults::Point::GcClaimed);
            let removed = if registered_worktree {
                self.remove_workspace_git_metadata(
                    &workspace.repository_id,
                    &repository,
                    &workspace.path,
                )
                .await
            } else if workspace.path.exists() {
                self.filesystem
                    .remove_tree(&workspace.path)
                    .map_err(subsystem_error)
            } else {
                Ok(())
            };
            if removed.is_err() {
                let _ = self
                    .database
                    .mark_workspace_state(&workspace.id, &workspace.state);
                skipped += 1;
                continue;
            }
            crate::faults::hit(crate::faults::Point::GcTreeRemoved);
            let mut refs_clean = true;
            for checkpoint_record in self.database.checkpoints_for_workspace(&workspace.id)? {
                if checkpoint_record.state != "ready" {
                    refs_clean = false;
                    continue;
                }
                match self.load_git_checkpoint(&checkpoint_record).await {
                    Ok((_, _, checkpoint)) => {
                        if self
                            .git
                            .delete_checkpoint(&repository, &checkpoint)
                            .await
                            .is_err()
                        {
                            refs_clean = false;
                        }
                    }
                    Err(_) => refs_clean = false,
                }
            }
            if !refs_clean {
                let _ = self
                    .database
                    .mark_workspace_state(&workspace.id, &workspace.state);
                skipped += 1;
                continue;
            }
            crate::faults::hit(crate::faults::Point::GcRefsRemoved);
            self.secrets
                .remove(&workspace.id)
                .map_err(EngineError::internal)?;
            crate::faults::hit(crate::faults::Point::GcSecretsRemoved);
            self.database.delete_workspace_record(&workspace.id)?;
            crate::faults::hit(crate::faults::Point::GcRecordDeleted);
            deleted += 1;
        }
        let _dependency_gc = self.locks.dependency_artifacts.write().await;
        // Read protection only after all in-flight preparations have recorded
        // their receipts, never from a snapshot taken before awaiting the lock.
        let protected = self.database.protected_dependency_fingerprints()?;
        let dependencies = Arc::clone(&self.dependencies);
        let cache_root = self.config.root.clone();
        let dependency_gc = tokio::task::spawn_blocking(move || {
            dependencies.garbage_collect_default(&cache_root, &protected)
        })
        .await
        .map_err(|error| {
            EngineError::internal(format!("dependency GC task failed to join: {error}"))
        })?
        .map_err(dependency_error)?;
        Ok(Outcome::Completed(json!({
            "eligible": deleted + skipped,
            "deleted": deleted,
            "skipped": skipped,
            "layers_deleted": dependency_gc.removed.len(),
            "layers_reclaimed": dependency_gc.removed_bytes,
        })))
    }

    async fn reconcile_publish_operations(
        &self,
        operation: &OperationId,
    ) -> Result<ReconciliationStats, EngineError> {
        let mut stats = ReconciliationStats::default();
        for mut intent in self.database.publish_intents_for_recovery(operation)? {
            let operation_record = self.database.operation(&intent.operation_id)?;

            if matches!(intent.state.as_str(), "failed" | "cancelled") {
                self.cleanup_recovered_publish_anchor(&intent, &mut stats)
                    .await;
                if operation_record
                    .as_ref()
                    .is_some_and(|record| record.state == "running")
                {
                    let error = EngineError::domain("OPERATION_INTERRUPTED", "safe")
                        .next("retry publish with a new idempotency key")
                        .operation(intent.operation_id.clone());
                    self.database.fail_publish_intent_and_operation(
                        &intent.operation_id,
                        &error.as_wire(),
                    )?;
                    stats.publishes_failed += 1;
                }
                continue;
            }

            if intent.state != "completed" {
                let Some(checkpoint_record) = self.database.checkpoint(&intent.checkpoint_id)?
                else {
                    self.fail_recovered_publish(
                        &intent,
                        EngineError::domain("PUBLISH_CHECKPOINT_MISSING", "never"),
                        &mut stats,
                    )
                    .await?;
                    continue;
                };
                let loaded = self.load_git_checkpoint(&checkpoint_record).await;
                let (_, repository, checkpoint) = match loaded {
                    Ok(loaded) => loaded,
                    Err(error) if error.code == "INTERNAL" => {
                        tracing::error!(
                            operation = %intent.operation_id,
                            code = %error.code,
                            "publish recovery is waiting for its checkpoint repository"
                        );
                        stats.publishes_pending += 1;
                        continue;
                    }
                    Err(error) => {
                        self.fail_recovered_publish(&intent, error, &mut stats)
                            .await?;
                        continue;
                    }
                };
                if checkpoint_record.workspace_id != intent.workspace_id {
                    self.fail_recovered_publish(
                        &intent,
                        EngineError::domain("PUBLISH_JOURNAL_INVALID", "never"),
                        &mut stats,
                    )
                    .await?;
                    continue;
                }
                let Some(repository_record) =
                    self.database.repository_by_id(&intent.repository_id)?
                else {
                    self.fail_recovered_publish(
                        &intent,
                        EngineError::domain("REPOSITORY_NOT_FOUND", "never"),
                        &mut stats,
                    )
                    .await?;
                    continue;
                };
                if repository_record.bare_path != repository.git_dir() {
                    self.fail_recovered_publish(
                        &intent,
                        EngineError::domain("PUBLISH_JOURNAL_INVALID", "never"),
                        &mut stats,
                    )
                    .await?;
                    continue;
                }
                let remote = match self.git.canonicalize_remote(&repository_record.identity) {
                    Ok(remote) => remote,
                    Err(error) => {
                        self.fail_recovered_publish(&intent, subsystem_error(error), &mut stats)
                            .await?;
                        continue;
                    }
                };
                match self
                    .drive_publish_intent(intent.clone(), &repository, &remote, &checkpoint)
                    .await
                {
                    Ok(prepared) => {
                        if intent.source_kind == "workspace_publish" {
                            let outcome = publish_outcome(&intent.branch, intent.push, &prepared);
                            self.database
                                .complete_publish_intent(&intent.operation_id, Some(&outcome))?;
                        } else {
                            self.database
                                .complete_publish_intent(&intent.operation_id, None)?;
                        }
                        intent = self
                            .database
                            .publish_intent(&intent.operation_id)?
                            .ok_or_else(|| {
                                EngineError::domain("PUBLISH_JOURNAL_INVALID", "never")
                            })?;
                        self.cleanup_recovered_publish_anchor(&intent, &mut stats)
                            .await;
                    }
                    Err(error) if error.retry == "safe" && error.code == "INTERNAL" => {
                        tracing::error!(
                            operation = %intent.operation_id,
                            code = %error.code,
                            "publish recovery remains pending"
                        );
                        stats.publishes_pending += 1;
                        continue;
                    }
                    Err(error) => {
                        self.fail_recovered_publish(&intent, error, &mut stats)
                            .await?;
                        continue;
                    }
                }
            } else {
                self.cleanup_recovered_publish_anchor(&intent, &mut stats)
                    .await;
            }

            // A direct publish operation is completed in the same transaction
            // as its publish row. ResolutionComplete still owns one final
            // durable step: publishing the successor handoff.
            if intent.source_kind == "resolution_complete"
                && self
                    .database
                    .operation(&intent.operation_id)?
                    .is_some_and(|record| record.state == "running")
            {
                let Some(successor) = self.database.workspace(&intent.workspace_id)? else {
                    self.fail_recovered_publish(
                        &intent,
                        EngineError::domain("RESOLUTION_WORKSPACE_MISSING", "never"),
                        &mut stats,
                    )
                    .await?;
                    continue;
                };
                let Some(parent_id) = successor.predecessor_id.clone() else {
                    self.fail_recovered_publish(
                        &intent,
                        EngineError::domain("RESOLUTION_PREDECESSOR_MISSING", "never"),
                        &mut stats,
                    )
                    .await?;
                    continue;
                };
                let Some(parent) = self.database.workspace(&parent_id)? else {
                    self.fail_recovered_publish(
                        &intent,
                        EngineError::domain("RESOLUTION_PREDECESSOR_MISSING", "never"),
                        &mut stats,
                    )
                    .await?;
                    continue;
                };
                let actor = self
                    .database
                    .operation_actor(&intent.operation_id)?
                    .ok_or_else(|| EngineError::domain("PUBLISH_JOURNAL_INVALID", "never"))?;
                let _lifecycle = self.keyed_lock(format!("lifecycle:{}", parent.id.0)).await;
                match self.prepare_handoff(&parent, &successor, &intent.operation_id, &actor) {
                    Ok(_) => {
                        stats.publishes_completed += 1;
                        crate::faults::hit(crate::faults::Point::ReconcilePublishHandoffPrepared);
                    }
                    Err(error) if error.retry == "safe" => {
                        tracing::error!(
                            operation = %intent.operation_id,
                            code = %error.code,
                            "resolved publish handoff recovery remains pending"
                        );
                        stats.publishes_pending += 1;
                    }
                    Err(error) => {
                        self.fail_recovered_publish(&intent, error, &mut stats)
                            .await?;
                    }
                }
            } else {
                stats.publishes_completed += 1;
            }
        }
        Ok(stats)
    }

    async fn cleanup_recovered_publish_anchor(
        &self,
        intent: &PublishIntentRecord,
        stats: &mut ReconciliationStats,
    ) {
        if intent.anchor_cleaned {
            return;
        }
        let Some(repository) = self
            .database
            .repository_by_id(&intent.repository_id)
            .ok()
            .flatten()
        else {
            return;
        };
        let managed = ManagedRepository::new(repository.bare_path);
        let commit = match &intent.commit_oid {
            Some(commit) => Oid::new(commit.0.clone()).ok(),
            None => self
                .git
                .publish_anchor_oid(&managed, &intent.anchor_ref)
                .await
                .ok()
                .flatten(),
        };
        let Some(commit) = commit else {
            // Absence is already a clean state; recording it avoids retrying
            // terminal journal rows forever.
            if self
                .database
                .mark_publish_anchor_cleaned(&intent.operation_id)
                .is_ok()
            {
                stats.publish_anchors_removed += 1;
            }
            return;
        };
        match self
            .git
            .delete_publish_anchor(&managed, &intent.anchor_ref, &commit)
            .await
        {
            Ok(_) => {
                if self
                    .database
                    .mark_publish_anchor_cleaned(&intent.operation_id)
                    .is_ok()
                {
                    stats.publish_anchors_removed += 1;
                }
            }
            Err(error) => tracing::error!(
                operation = %intent.operation_id,
                error = %error,
                "publish anchor cleanup remains pending"
            ),
        }
    }

    async fn fail_recovered_publish(
        &self,
        intent: &PublishIntentRecord,
        mut error: EngineError,
        stats: &mut ReconciliationStats,
    ) -> Result<(), EngineError> {
        error.operation = Some(intent.operation_id.clone());
        // Compensate a local-only partial apply while its value is still ours.
        // A candidate observed on the remote is never rolled back locally.
        let repository = self.database.repository_by_id(&intent.repository_id)?;
        if let (Some(repository), Some(commit), Some(tree)) =
            (repository, &intent.commit_oid, &intent.tree_oid)
            && let (Ok(commit), Ok(tree)) = (Oid::new(commit.0.clone()), Oid::new(tree.0.clone()))
        {
            let managed = ManagedRepository::new(repository.bare_path.clone());
            let remote = self.git.canonicalize_remote(&repository.identity).ok();
            let prepared = PreparedPublish {
                previous_remote: intent
                    .expected_remote_oid
                    .as_ref()
                    .and_then(|oid| Oid::new(oid.0.clone()).ok()),
                commit,
                tree,
                target_ref: format!("refs/heads/{}", intent.branch),
                anchor_ref: intent.anchor_ref.clone(),
            };
            let remote_has_commit = if intent.push {
                match remote.as_ref() {
                    Some(remote) => self
                        .git
                        .remote_branch_oid(remote, &intent.branch)
                        .await
                        .is_ok_and(|oid| oid.as_ref() == Some(&prepared.commit)),
                    None => false,
                }
            } else {
                false
            };
            if !remote_has_commit && intent.state != "remote_applied" {
                let expected_local = intent
                    .expected_local_oid
                    .as_ref()
                    .and_then(|oid| Oid::new(oid.0.clone()).ok());
                let _ = self
                    .git
                    .rollback_prepared_publish_local(&managed, &prepared, expected_local.as_ref())
                    .await;
            }
        }
        self.database
            .fail_publish_intent_and_operation(&intent.operation_id, &self.persist_error(error))?;
        let terminal = self
            .database
            .publish_intent(&intent.operation_id)?
            .unwrap_or_else(|| intent.clone());
        self.cleanup_recovered_publish_anchor(&terminal, stats)
            .await;
        stats.publishes_failed += 1;
        Ok(())
    }

    /// Whether this workspace is entitled to have no tree on disk.
    ///
    /// Sleeping is the only thing that takes a tree away from a workspace that
    /// still has a record: `suspended` is the state itself, `suspending` is
    /// one mid-flight, and `released` is where a wake leaves the predecessor
    /// it rebuilt from until the collector takes it. Letting any of them reach
    /// the validity check in `reconcile_workspace_resources` would mark them
    /// `failed` on every daemon start and hand a whole suspension to the
    /// collector, which is the single worst thing that function could do.
    fn is_dematerialized(&self, workspace: &WorkspaceRecord) -> Result<bool, EngineError> {
        Ok(match workspace.state.as_str() {
            "suspended" | "suspending" => true,
            "released" => {
                std::fs::symlink_metadata(&workspace.path).is_err()
                    && self
                        .database
                        .suspension_checkpoint(&workspace.id)?
                        .is_some()
            }
            _ => false,
        })
    }

    /// Finish or undo suspensions that a crash interrupted.
    ///
    /// A `suspending` workspace is deliberately not an "incomplete" one:
    /// incomplete workspaces get deleted, and this one owns a durable
    /// checkpoint and a durable vault. If a ready sleep checkpoint exists the
    /// suspension is rolled forward -- registration and tree removed, record
    /// marked `suspended` -- and if it does not, the workspace goes back to
    /// Dormant with its tree untouched. Both branches are idempotent and
    /// neither ever deletes a record.
    async fn reconcile_suspending_workspaces(
        &self,
        stats: &mut ReconciliationStats,
    ) -> Result<(), EngineError> {
        for workspace in self
            .database
            .workspaces_in_state_before("suspending", i64::MAX)?
        {
            match self.settle_suspension(&workspace).await {
                Ok(Settled::Finished) => stats.suspensions_finished += 1,
                Ok(Settled::Reverted) => stats.suspensions_reverted += 1,
                Err(error) => {
                    tracing::error!(
                        workspace = %workspace.id,
                        code = %error.code,
                        "interrupted suspension could not be finished"
                    );
                    stats.suspensions_failed += 1;
                }
            }
        }
        Ok(())
    }

    /// Decide what one `suspending` workspace becomes, and make it so.
    ///
    /// The checkpoint is the deciding fact, not the tree: a workspace with a
    /// ready sleep checkpoint has everything a wake needs, so the suspension
    /// rolls forward; one without has nothing to wake from, so it goes back to
    /// Dormant. Shared by the startup reconciliation pass, the maintenance
    /// sweep and the three doors into a session, which is what keeps an
    /// embedded host that never reconciles from stranding a suspension.
    async fn settle_suspension(&self, workspace: &WorkspaceRecord) -> Result<Settled, EngineError> {
        let Some(checkpoint) = self.database.suspension_checkpoint(&workspace.id)? else {
            self.database.restore_suspending_workspace(&workspace.id)?;
            return Ok(Settled::Reverted);
        };
        self.finish_suspension(workspace, &checkpoint.id).await?;
        Ok(Settled::Finished)
    }

    /// Take the tree and the registration away and record the suspension.
    ///
    /// Idempotent at every step: the registration, the tree and the state
    /// change are each skipped when they are already done, so a crash anywhere
    /// inside leaves work for the next call rather than a broken record.
    async fn finish_suspension(
        &self,
        workspace: &WorkspaceRecord,
        checkpoint: &CheckpointId,
    ) -> Result<(), EngineError> {
        if workspace.path.join(".git").is_file() {
            let (_, managed, _) = self.repository_for(workspace)?;
            self.remove_workspace_git_metadata(&workspace.repository_id, &managed, &workspace.path)
                .await?;
        }
        if workspace.path.exists() {
            self.filesystem
                .remove_tree(&workspace.path)
                .map_err(subsystem_error)?;
        }
        self.database.suspend_workspace(&workspace.id, checkpoint)?;
        Ok(())
    }

    /// Settle an interrupted suspension before a caller looks at the tree.
    ///
    /// `reconcile_suspending_workspaces` runs from `Intent::Reconcile`, which
    /// the daemon sends at startup and an embedded host need never send at
    /// all. Until it runs, a `suspending` workspace answers `SESSION_SUSPENDED`
    /// to sleep and `WORKSPACE_NOT_MATERIALIZED` to everything else: it has no
    /// tree to reattach to and no `suspended` record to wake from, so every
    /// door into it is shut. Each door therefore settles it on the way in.
    ///
    /// Never fatal. The caller's own state checks run either way, and a
    /// settlement that fails is left for the next pass.
    async fn settle_suspending_workspace(&self, workspace: &WorkspaceRecord) {
        if workspace.state != "suspending" {
            return;
        }
        if let Err(error) = self.settle_suspension(workspace).await {
            tracing::warn!(
                workspace = %workspace.id,
                code = %error.code,
                "interrupted suspension could not be settled on demand"
            );
        }
    }

    /// The same pass, addressed by session, for the door that has not yet read
    /// a workspace record. Takes the lifecycle lock, so callers that already
    /// hold it must use [`Engine::settle_suspending_workspace`] instead.
    async fn settle_suspending_session(&self, session_id: &SessionId) -> Result<(), EngineError> {
        let Some(session) = self.database.session(session_id)? else {
            return Ok(());
        };
        let Some(workspace) = self.database.workspace(&session.workspace_id)? else {
            return Ok(());
        };
        if workspace.state != "suspending" {
            return Ok(());
        }
        let _lifecycle = self
            .keyed_lock(format!("lifecycle:{}", workspace.id.0))
            .await;
        // Re-read under the lock: the pass that settles this may already have
        // run between the read above and the lock.
        let Some(workspace) = self.database.workspace(&session.workspace_id)? else {
            return Ok(());
        };
        self.settle_suspending_workspace(&workspace).await;
        Ok(())
    }

    /// Demote one workspace reconciliation could not vouch for, and count it
    /// separately when it was dormant. A dormant workspace is work the caller
    /// was told survives a lost lease; turning it into a GC candidate is worth
    /// a warning and a number in the reconcile report, not a silent UPDATE.
    fn fail_reconciled_workspace(
        &self,
        workspace: &WorkspaceRecord,
        reason: &'static str,
        stats: &mut ReconciliationStats,
    ) -> Result<(), EngineError> {
        if workspace.state == "failed" {
            return Ok(());
        }
        let previous = self.database.fail_workspace(&workspace.id, reason)?;
        if previous == "dormant" {
            stats.dormant_workspaces_failed += 1;
            tracing::warn!(
                workspace = %workspace.id,
                reason,
                "a dormant workspace failed reconciliation and is now collectible"
            );
        }
        Ok(())
    }

    async fn reconcile_workspace_resources(
        &self,
        operation: &OperationId,
    ) -> Result<ReconciliationStats, EngineError> {
        let mut stats = ReconciliationStats::default();
        // Resolve interrupted suspensions before inventorying anything: after
        // this every workspace is either cleanly `suspended` or cleanly back
        // to `dormant`, and the loops below need no special case for the
        // in-between state.
        self.reconcile_suspending_workspaces(&mut stats).await?;
        let workspace_root =
            std::fs::canonicalize(self.config.workspaces_dir()).map_err(EngineError::internal)?;
        let repositories = self.database.repositories()?;
        let repository_map = repositories
            .iter()
            .map(|record| (record.id.0.clone(), record.clone()))
            .collect::<HashMap<_, _>>();
        let workspaces = self.database.workspaces()?;
        let mut recovery_claims = BTreeSet::new();
        for workspace in workspaces
            .iter()
            .filter(|workspace| is_incomplete_workspace_state(&workspace.state))
        {
            if self
                .database
                .claim_incomplete_workspace_for_recovery(&workspace.id, operation)?
            {
                recovery_claims.insert(workspace.id.0.clone());
                crate::faults::hit(crate::faults::Point::ReconcileWorkspaceClaimed);
            }
        }

        // Read registrations once per repository. A corrupt repository is
        // isolated: recovery leaves its paths untouched and reports through
        // tracing instead of guessing at ownership.
        let mut registrations = HashMap::<String, BTreeSet<PathBuf>>::new();
        for repository in &repositories {
            let managed = ManagedRepository::new(repository.bare_path.clone());
            self.git
                .configure_content_filter(&managed)
                .await
                .map_err(subsystem_error)?;
            match self.git.registered_worktrees(&managed).await {
                Ok(paths) => {
                    registrations.insert(repository.id.0.clone(), paths);
                }
                Err(error) => {
                    tracing::error!(
                        repository = %repository.id,
                        error = %error,
                        "cannot inventory worktrees during startup reconciliation"
                    );
                }
            }
        }

        let mut preserved_paths = BTreeSet::new();
        let mut expected_by_repository = HashMap::<String, BTreeSet<PathBuf>>::new();
        for workspace in &workspaces {
            if is_incomplete_workspace_state(&workspace.state) {
                if !recovery_claims.contains(&workspace.id.0)
                    && let Some(path) = checked_workspace_path(&workspace.path, &workspace_root)
                {
                    preserved_paths.insert(path);
                }
                continue;
            }
            if self.is_dematerialized(workspace)? {
                if let Some(path) = checked_workspace_path(&workspace.path, &workspace_root)
                    && std::fs::symlink_metadata(&workspace.path).is_ok()
                {
                    preserved_paths.insert(path);
                }
                continue;
            }
            let Some(path) = checked_workspace_path(&workspace.path, &workspace_root) else {
                stats.invalid_workspaces += 1;
                self.fail_reconciled_workspace(
                    workspace,
                    "path_outside_workspace_pool",
                    &mut stats,
                )?;
                continue;
            };
            let Some(registered) = registrations.get(&workspace.repository_id.0) else {
                preserved_paths.insert(path);
                continue;
            };
            let valid_directory = std::fs::symlink_metadata(&workspace.path)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
            let valid_pointer = std::fs::symlink_metadata(workspace.path.join(".git"))
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
            if valid_directory {
                preserved_paths.insert(path.clone());
            }
            if valid_directory && valid_pointer && registered.contains(&path) {
                expected_by_repository
                    .entry(workspace.repository_id.0.clone())
                    .or_default()
                    .insert(path);
            } else {
                stats.invalid_workspaces += 1;
                self.fail_reconciled_workspace(
                    workspace,
                    "missing_tree_or_worktree_registration",
                    &mut stats,
                )?;
            }
        }

        // Registrations beneath the daemon pool which are absent from the
        // durable control plane are safe to remove. Paths owned by any durable
        // workspace remain protected even if registered to the wrong repo.
        for repository in &repositories {
            let Some(registered) = registrations.get(&repository.id.0) else {
                continue;
            };
            let expected = expected_by_repository
                .get(&repository.id.0)
                .cloned()
                .unwrap_or_default();
            let managed = ManagedRepository::new(repository.bare_path.clone());
            for registered_path in registered {
                let Some(path) = checked_workspace_path(registered_path, &workspace_root)
                    .or_else(|| checked_registration_stage(registered_path, &workspace_root))
                else {
                    continue;
                };
                if expected.contains(&path) {
                    continue;
                }
                if preserved_paths.contains(&path) {
                    stats.worktree_metadata_conflicts += 1;
                    continue;
                }
                // After moving .git, the registration can still point at an
                // empty staging directory. Git rejects an existing directory
                // without .git as "not a working tree", but can safely remove
                // its registration once that empty directory is absent.
                if checked_registration_stage(registered_path, &workspace_root).is_some()
                    && std::fs::symlink_metadata(path.join(".git"))
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    match std::fs::remove_dir(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => {
                            stats.worktree_metadata_conflicts += 1;
                            continue;
                        }
                    }
                }
                match self
                    .git
                    .remove_orphan_worktree(&managed, registered_path, &workspace_root)
                    .await
                {
                    Ok(()) => {
                        stats.worktree_metadata_removed += 1;
                        crate::faults::hit(crate::faults::Point::ReconcileRegistrationRemoved);
                    }
                    Err(error) => tracing::error!(
                        repository = %repository.id,
                        path = %registered_path.display(),
                        error = %error,
                        "cannot remove orphan worktree registration"
                    ),
                }
            }
        }

        // Finish resources which were published only partially. Claiming is a
        // transactional fence; a live lease, pending review, publish intent,
        // or unrelated running operation makes the candidate ineligible.
        for workspace in workspaces
            .iter()
            .filter(|workspace| is_incomplete_workspace_state(&workspace.state))
        {
            if !recovery_claims.contains(&workspace.id.0) {
                stats.incomplete_failed += 1;
                continue;
            }
            let Some(repository) = repository_map.get(&workspace.repository_id.0) else {
                stats.incomplete_failed += 1;
                continue;
            };
            let managed = ManagedRepository::new(repository.bare_path.clone());
            let cleanup = async {
                let mut refs_removed = 0_u64;
                for checkpoint in self.database.checkpoints_for_workspace(&workspace.id)? {
                    if self
                        .git
                        .delete_private_checkpoint_refs(&managed, &workspace.id.0, &checkpoint.id.0)
                        .await
                        .map_err(subsystem_error)?
                    {
                        refs_removed += 1;
                        crate::faults::hit(crate::faults::Point::ReconcileWorkspaceRefsRemoved);
                    }
                }

                if std::fs::symlink_metadata(&workspace.path).is_ok() {
                    let path = checked_workspace_path(&workspace.path, &workspace_root)
                        .ok_or_else(|| EngineError::domain("WORKSPACE_PATH_INVALID", "never"))?;
                    let still_registered = self
                        .git
                        .registered_worktrees(&managed)
                        .await
                        .map_err(subsystem_error)?
                        .contains(&path);
                    if still_registered {
                        return Err(EngineError::domain("WORKTREE_RECOVERY_BLOCKED", "safe"));
                    }
                    match std::fs::symlink_metadata(workspace.path.join(".git")) {
                        Ok(metadata)
                            if metadata.is_file() && !metadata.file_type().is_symlink() =>
                        {
                            remove_private_git_pointer(&workspace.path)?;
                        }
                        Ok(_) => {
                            return Err(EngineError::domain("WORKTREE_METADATA_INVALID", "never"));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(EngineError::internal(error)),
                    }
                    self.filesystem
                        .remove_tree(&workspace.path)
                        .map_err(subsystem_error)?;
                    crate::faults::hit(crate::faults::Point::ReconcileWorkspaceTreeRemoved);
                }
                self.secrets
                    .remove(&workspace.id)
                    .map_err(EngineError::internal)?;
                crate::faults::hit(crate::faults::Point::ReconcileSecretsRemoved);
                self.database.delete_workspace_record(&workspace.id)?;
                crate::faults::hit(crate::faults::Point::ReconcileWorkspaceDeleted);
                Ok::<u64, EngineError>(refs_removed)
            }
            .await;
            match cleanup {
                Ok(refs_removed) => {
                    stats.checkpoint_refs_removed += refs_removed;
                    stats.incomplete_removed += 1;
                }
                Err(error) => {
                    tracing::error!(
                        workspace = %workspace.id,
                        code = %error.code,
                        "incomplete workspace could not be reconciled"
                    );
                    stats.incomplete_failed += 1;
                }
            }
        }

        // Finally, remove checkpoint groups which have no SQLite record. The
        // comparison is per repository and deletion CASes every present plane.
        let remaining = self.database.workspaces()?;
        let mut expected_checkpoints = HashMap::<String, BTreeSet<(String, String)>>::new();
        for workspace in &remaining {
            for checkpoint in self.database.checkpoints_for_workspace(&workspace.id)? {
                expected_checkpoints
                    .entry(workspace.repository_id.0.clone())
                    .or_default()
                    .insert((workspace.id.0.clone(), checkpoint.id.0));
            }
        }
        for repository in &repositories {
            let managed = ManagedRepository::new(repository.bare_path.clone());
            let expected = expected_checkpoints
                .get(&repository.id.0)
                .cloned()
                .unwrap_or_default();
            let keys = match self.git.private_checkpoint_keys(&managed).await {
                Ok(keys) => keys,
                Err(error) => {
                    tracing::error!(
                        repository = %repository.id,
                        error = %error,
                        "cannot inventory private checkpoint refs"
                    );
                    continue;
                }
            };
            for (workspace, checkpoint) in keys.difference(&expected) {
                match self
                    .git
                    .delete_private_checkpoint_refs(&managed, workspace, checkpoint)
                    .await
                {
                    Ok(true) => {
                        stats.checkpoint_refs_removed += 1;
                        crate::faults::hit(crate::faults::Point::ReconcileCheckpointRefsRemoved);
                    }
                    Ok(false) => {}
                    Err(error) => tracing::error!(
                        repository = %repository.id,
                        workspace,
                        checkpoint,
                        error = %error,
                        "cannot remove orphan checkpoint refs"
                    ),
                }
            }
        }

        Ok(stats)
    }

    fn cleanup_staging(&self) -> Result<u64, EngineError> {
        let mut removed = 0_u64;
        removed += cleanup_directory_entries(&self.config.repositories_dir(), |name| {
            name.starts_with(".shade-git-") || name.starts_with(".shade-git-import-")
        })?;
        removed += cleanup_directory_entries(&self.config.workspaces_dir(), |name| {
            [
                ".shade-apfs-clone-",
                ".shade-copy-fake-",
                ".shade-materialize-",
                ".shade-register-",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix))
        })?;

        // Base materialization stages beside the final tree, one directory
        // below `bases/<repository>/`, rather than directly below `bases/`.
        if let Ok(entries) = std::fs::read_dir(self.config.bases_dir()) {
            for entry in entries {
                let entry = entry.map_err(EngineError::internal)?;
                if entry.file_type().map_err(EngineError::internal)?.is_dir() {
                    removed += cleanup_directory_entries(&entry.path(), |name| {
                        name.starts_with(".shade-materialize-")
                            || name.starts_with(".shade-apfs-clone-")
                            || name.starts_with(".shade-copy-fake-")
                    })?;
                }
            }
        }

        removed += self
            .secrets
            .cleanup_review_staging()
            .map_err(EngineError::internal)?;
        removed += self
            .dependencies
            .cleanup_staging(&self.config.root)
            .map_err(dependency_error)?;
        removed += cleanup_directory_entries(&self.config.secrets_dir(), |name| {
            name.starts_with('.') && name.ends_with(".staging")
        })?;
        Ok(removed)
    }
}

fn is_incomplete_workspace_state(state: &str) -> bool {
    matches!(state, "materializing" | "deleting" | "recovering")
}

fn checked_workspace_path(path: &Path, workspace_root: &Path) -> Option<PathBuf> {
    if !path.is_absolute()
        || !path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"ws_"))
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return None;
    }
    let checked = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_owned(),
        Err(_) => return None,
    };
    (checked.starts_with(workspace_root) && checked != workspace_root).then_some(checked)
}

// Git registers the temporary root before its .git pointer is moved to the
// final workspace. A crash in that interval leaves a locked registration which
// `worktree prune` cannot remove. Only this exact daemon-owned staging shape is
// eligible; never broaden recovery to arbitrary paths below the workspace pool.
fn checked_registration_stage(path: &Path, workspace_root: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(workspace_root).ok()?;
    let mut components = relative.components();
    let std::path::Component::Normal(stage) = components.next()? else {
        return None;
    };
    if !stage.as_encoded_bytes().starts_with(b".shade-register-")
        || components.next()? != std::path::Component::Normal(std::ffi::OsStr::new("worktree"))
        || components.next().is_some()
    {
        return None;
    }
    let parent = path.parent()?;
    for candidate in [parent, path] {
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => return None,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(path.to_owned())
}

fn cleanup_directory_entries(
    root: &Path,
    matches: impl Fn(&str) -> bool,
) -> Result<u64, EngineError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(EngineError::internal(error)),
    };
    let mut removed = 0_u64;
    for entry in entries {
        let entry = entry.map_err(EngineError::internal)?;
        let file_type = entry.file_type().map_err(EngineError::internal)?;
        let name = entry.file_name();
        if file_type.is_dir() && matches(&name.to_string_lossy()) {
            std::fs::remove_dir_all(entry.path()).map_err(EngineError::internal)?;
            removed += 1;
            crate::faults::hit(crate::faults::Point::ReconcileStagingRemoved);
        }
    }
    Ok(removed)
}

/// Build the Shade root and its top-level directories, private from the
/// instant they exist.
///
/// `create_dir_all` creates with `0o777` minus the umask -- world-readable
/// under the usual `022` -- and the `chmod` that followed closed it a moment
/// later, which is a moment during which anything on the machine could walk
/// into a workspace tree. `DirBuilder::mode` closes it at creation instead,
/// and it applies to every component the recursive call has to invent, not
/// just the leaf. The `chmod` stays because `mode` is a creation mode the
/// caller's umask still subtracts from: under a restrictive umask it would
/// otherwise leave a directory its owner cannot enter, and it does nothing at
/// all for a directory that already existed.
fn create_roots(config: &EngineConfig) -> std::io::Result<()> {
    for directory in [
        config.root.clone(),
        config.repositories_dir(),
        config.bases_dir(),
        config.workspaces_dir(),
        config.dependencies_dir(),
        config.runtime_dir(),
        config.secrets_dir(),
    ] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn intent_kind(intent: &Intent) -> &'static str {
    match intent {
        Intent::SessionOpen(_) => "session_open",
        Intent::RepositoryWarm { .. } => "repository_warm",
        Intent::LeaseHeartbeat { .. } => "lease_heartbeat",
        Intent::WorkspaceCheckpoint { .. } => "workspace_checkpoint",
        Intent::WorkspaceFork { .. } => "workspace_fork",
        Intent::WorkspaceSync { .. } => "workspace_sync",
        Intent::WorkspaceRestore { .. } => "workspace_restore",
        Intent::DependenciesRefresh { .. } => "dependencies_refresh",
        Intent::DependencyScriptDecision { .. } => "dependency_script_decision",
        Intent::WorkspacePublish { .. } => "workspace_publish",
        Intent::ResolutionComplete { .. } => "resolution_complete",
        Intent::WorkspaceRelease { .. } => "workspace_release",
        Intent::ReviewResolve { .. } => "review_resolve",
        Intent::SuccessorAdopt { .. } => "successor_adopt",
        Intent::GarbageCollect => "garbage_collect",
        Intent::SessionReattach { .. } => "session_reattach",
        Intent::WorkspaceSleep { .. } => "workspace_sleep",
        Intent::SessionWake { .. } => "session_wake",
        Intent::MaintenanceSweep => "maintenance_sweep",
        Intent::Reconcile => "reconcile",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KEYED_LOCK_SWEEP_THRESHOLD, LifecycleLocks, REJECTED_PATH_LIMIT_BYTES, subsystem_error,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    #[test]
    fn a_repository_rejection_names_its_path_and_stays_bounded() {
        for (message, code, next) in [
            (
                "TRACKED_SECRET_FILE: apps/web/.env.production",
                "TRACKED_SECRET_FILE",
                "untrack apps/web/.env.production, then retry",
            ),
            (
                "TRACKED_DEPENDENCY_OUTPUT: apps/web/node_modules/left-pad/index.js",
                "TRACKED_DEPENDENCY_OUTPUT",
                "untrack apps/web/node_modules/left-pad/index.js, then retry",
            ),
            (
                "UNSUPPORTED_SUBMODULE: vendor/library",
                "UNSUPPORTED_SUBMODULE",
                "remove the submodule at vendor/library, then retry",
            ),
            (
                "UNSUPPORTED_GIT_LFS: assets/video.mp4",
                "UNSUPPORTED_GIT_LFS",
                "replace the Git LFS content at assets/video.mp4, then retry",
            ),
            (
                "UNSUPPORTED_GIT_FILTER: packages/api/.gitattributes",
                "UNSUPPORTED_GIT_FILTER",
                "remove the Git filter declared in packages/api/.gitattributes, then retry",
            ),
        ] {
            let error = subsystem_error(message);
            assert_eq!(error.code, code);
            assert_eq!(error.retry, "never");
            assert_eq!(error.next.as_deref(), Some(next));
        }

        // A path long enough to threaten the response budget is truncated,
        // and a rejection carrying no path still says something actionable.
        let long = format!("TRACKED_SECRET_FILE: {}/.env", "nested".repeat(64));
        let next = subsystem_error(long).next.unwrap();
        assert!(next.starts_with("untrack nestednested"));
        assert!(next.ends_with("..., then retry"));
        assert!(next.len() <= REJECTED_PATH_LIMIT_BYTES + 32);
        assert_eq!(
            subsystem_error("UNSUPPORTED_GIT_LFS").next.as_deref(),
            Some("replace Git LFS content before opening")
        );
    }

    #[tokio::test]
    async fn keyed_lock_serializes_concurrent_callers_on_the_same_key() {
        let locks = Arc::new(LifecycleLocks::default());
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (held, wait_for_held) = oneshot::channel();
        let (release, wait_for_release) = oneshot::channel();

        let first = tokio::spawn({
            let locks = Arc::clone(&locks);
            let inside = Arc::clone(&inside);
            let peak = Arc::clone(&peak);
            async move {
                let guard = locks.keyed_lock("k".to_owned()).await;
                let depth = inside.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(depth, Ordering::SeqCst);
                held.send(()).expect("signal that the lock is held");
                // Only released once the contender has demonstrably queued.
                wait_for_release.await.expect("release signal");
                inside.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }
        });

        wait_for_held.await.expect("first holder");
        let second = tokio::spawn({
            let locks = Arc::clone(&locks);
            let inside = Arc::clone(&inside);
            let peak = Arc::clone(&peak);
            async move {
                let guard = locks.keyed_lock("k".to_owned()).await;
                let depth = inside.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(depth, Ordering::SeqCst);
                inside.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }
        });

        // Give the contender every chance to break mutual exclusion.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            inside.load(Ordering::SeqCst),
            1,
            "second caller entered the critical section while the first held it"
        );
        assert_eq!(locks.keyed.lock().await.len(), 1);

        release.send(()).expect("release the first holder");
        first.await.expect("first task");
        second.await.expect("second task");
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "critical section overlapped"
        );
    }

    #[tokio::test]
    async fn dropped_guards_leave_a_sweepable_entry() {
        let locks = LifecycleLocks::default();

        let first = locks.keyed_lock("k".to_owned()).await;
        drop(first);
        let second = locks.keyed_lock("k".to_owned()).await;
        drop(second);

        assert_eq!(
            locks.keyed.lock().await.len(),
            1,
            "the dangling weak entry survives until a sweep"
        );
        assert_eq!(locks.sweep_keyed().await, 0, "sweep must drop dead entries");
        assert!(locks.keyed.lock().await.is_empty());
    }

    #[tokio::test]
    async fn crossing_the_threshold_sweeps_dead_entries() {
        let locks = LifecycleLocks::default();
        for index in 0..KEYED_LOCK_SWEEP_THRESHOLD {
            drop(locks.keyed_lock(format!("k{index}")).await);
        }
        assert_eq!(locks.keyed.lock().await.len(), KEYED_LOCK_SWEEP_THRESHOLD);

        // The next lookup crosses the threshold and reclaims every dead entry,
        // leaving only the key it was asked for.
        let live = locks.keyed_lock("fresh".to_owned()).await;
        assert_eq!(locks.keyed.lock().await.len(), 1);
        drop(live);
    }
}
