use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shade_protocol::{
    CheckpointId, Diagnostic, EventEnvelope, HandoffId, LeaseId, ObjectId, OperationId, Outcome,
    PendingHandoff, RepositoryId, ReviewId, ScriptApproval, SessionId, ShadeError, WorkspaceId,
};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("state file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("state serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("state mutex is poisoned")]
    Poisoned,
    #[error("state schema {found} is unsupported; expected {expected}")]
    UnsupportedSchema { found: i64, expected: i64 },
    #[error("idempotency key was reused with a different intent")]
    IdempotencyConflict,
    #[error("lease or workspace fence no longer matches")]
    LeaseFenced,
    #[error("workspace is {state} and cannot take a new lease")]
    WorkspaceNotAttachable { state: String },
    #[error("handoff was not found")]
    HandoffNotFound,
    #[error("handoff belongs to another actor")]
    HandoffOwnerMismatch,
    #[error("handoff is not pending")]
    HandoffNotPending,
    #[error("handoff {handoff_id} is already pending for the session or predecessor")]
    HandoffAlreadyPending { handoff_id: HandoffId },
}

#[derive(Clone)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryRecord {
    pub id: RepositoryId,
    pub identity: String,
    pub bare_path: PathBuf,
    pub default_ref: Option<String>,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    pub repository_id: RepositoryId,
    pub session_id: Option<SessionId>,
    pub path: PathBuf,
    pub base_ref: String,
    pub base_oid: ObjectId,
    pub head_oid: ObjectId,
    pub state: String,
    pub predecessor_id: Option<WorkspaceId>,
    pub dependency_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    pub intent: Option<String>,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub id: LeaseId,
    pub session_id: SessionId,
    pub workspace_id: WorkspaceId,
    pub fence: i64,
    pub heartbeat_at_ms: i64,
    pub expires_at_ms: i64,
    pub released_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: CheckpointId,
    pub workspace_id: WorkspaceId,
    pub head_oid: ObjectId,
    pub index_oid: ObjectId,
    pub worktree_oid: ObjectId,
    pub reason: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: OperationId,
    pub state: String,
    pub intent_kind: String,
    pub outcome: Option<Outcome>,
    pub error: Option<ShadeError>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug)]
pub enum BeginOperation {
    New(OperationId),
    Existing(Box<OperationRecord>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub id: ReviewId,
    pub workspace_id: WorkspaceId,
    pub kind: String,
    pub payload: Value,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishResolutionRecord {
    pub workspace_id: WorkspaceId,
    pub parent_workspace_id: WorkspaceId,
    pub branch: String,
    pub message: String,
    pub push: bool,
    pub expected_remote_oid: ObjectId,
    pub expected_local_oid: Option<ObjectId>,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishIntentRecord {
    pub operation_id: OperationId,
    pub workspace_id: WorkspaceId,
    pub repository_id: RepositoryId,
    pub checkpoint_id: CheckpointId,
    pub source_kind: String,
    pub branch: String,
    pub message: String,
    pub push: bool,
    pub original_base_oid: ObjectId,
    pub expected_remote_oid: Option<ObjectId>,
    pub expected_local_oid: Option<ObjectId>,
    pub anchor_ref: String,
    pub commit_oid: Option<ObjectId>,
    pub tree_oid: Option<ObjectId>,
    pub state: String,
    pub anchor_cleaned: bool,
}

/// What one maintenance sweep moved to dormant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpirySweep {
    pub sessions: usize,
    pub workspaces: usize,
    pub handoffs_cancelled: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffRecord {
    pub id: HandoffId,
    pub operation_id: OperationId,
    pub actor_id: String,
    pub session_id: SessionId,
    pub predecessor_workspace_id: WorkspaceId,
    pub successor_workspace_id: WorkspaceId,
    pub state: String,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        // Restrict the file before SQLite creates WAL/SHM files, which inherit
        // its mode. Do not follow a replaced database file into another target.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.as_ref())?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 0 {
            create_schema(&transaction)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version != SCHEMA_VERSION {
            return Err(DbError::UnsupportedSchema {
                found: version,
                expected: SCHEMA_VERSION,
            });
        }
        // Before anything reads a state column. An embedded host that never
        // sends `Reconcile` -- or sends `GarbageCollect` first -- would
        // otherwise still see the rows an older binary wrote as `orphaned`,
        // which is the one state the collector treats as collectible.
        normalize_dormant_state_rows(&transaction)?;
        transaction.commit()?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, DbError> {
        self.connection.lock().map_err(|_| DbError::Poisoned)
    }

    pub(crate) fn record_diagnostic(&self, diagnostic: &Diagnostic) -> Result<(), DbError> {
        insert_diagnostic(&*self.connection()?, diagnostic)
    }

    pub fn diagnostic(&self, id: &str) -> Result<Option<Diagnostic>, DbError> {
        read_diagnostic(&*self.connection()?, id)
    }

    /// Explicit offline lookup. Never creates a database or runs reconciliation.
    pub fn read_diagnostic(path: &Path, id: &str) -> Result<Option<Diagnostic>, DbError> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::other("state database is not a regular file").into());
        }
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        read_diagnostic(&connection, id)
    }

    pub fn begin_operation(
        &self,
        actor_id: &str,
        idempotency_key: &str,
        request_hash: &str,
        intent_kind: &str,
    ) -> Result<BeginOperation, DbError> {
        let mut connection = self.connection()?;
        // Idempotency lookup and claim are one cross-process CAS. In
        // particular, only one retry may resume an interrupted adoption.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = transaction
            .query_row(
                "SELECT id, request_hash, state, intent_kind, outcome_json, error_json, \
                 created_at_ms, updated_at_ms FROM operations \
                 WHERE actor_id=?1 AND idempotency_key=?2",
                params![actor_id, idempotency_key],
                operation_from_row,
            )
            .optional()?;
        if let Some((record, hash)) = existing {
            if hash != request_hash {
                return Err(DbError::IdempotencyConflict);
            }
            let interrupted_adopt = record.state == "failed"
                && record.intent_kind == "successor_adopt"
                && record
                    .error
                    .as_ref()
                    .is_some_and(|error| error.code == "OPERATION_INTERRUPTED");
            if interrupted_adopt {
                let changed = transaction.execute(
                    "UPDATE operations SET state='running', error_json=NULL, updated_at_ms=?2 \
                     WHERE id=?1 AND state='failed' AND intent_kind='successor_adopt' \
                       AND json_extract(error_json, '$.code')='OPERATION_INTERRUPTED'",
                    params![record.id.0, now_ms()],
                )?;
                if changed == 1 {
                    append_event(
                        &transaction,
                        "operation.resumed",
                        &record.id.0,
                        &json!({"operation": record.id, "intent": record.intent_kind}),
                    )?;
                    transaction.commit()?;
                    return Ok(BeginOperation::New(record.id));
                }
            }
            return Ok(BeginOperation::Existing(Box::new(record)));
        }
        let id = OperationId(format!("op_{}", ulid::Ulid::new()));
        let now = now_ms();
        transaction.execute(
            "INSERT INTO operations \
             (id, actor_id, idempotency_key, request_hash, intent_kind, state, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?6)",
            params![id.0, actor_id, idempotency_key, request_hash, intent_kind, now],
        )?;
        append_event(
            &transaction,
            "operation.started",
            &id.0,
            &json!({"operation": id, "intent": intent_kind}),
        )?;
        transaction.commit()?;
        Ok(BeginOperation::New(id))
    }

    pub fn finish_operation(
        &self,
        operation_id: &OperationId,
        outcome: &Outcome,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        finish_operation_in_transaction(&transaction, operation_id, outcome)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn bind_operation_resource(
        &self,
        operation_id: &OperationId,
        resource_id: &str,
        phase: &str,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE operations SET resource_id=?2, phase=?3, updated_at_ms=?4 WHERE id=?1",
            params![operation_id.0, resource_id, phase, now_ms()],
        )?;
        append_event(
            &transaction,
            "operation.phase",
            &operation_id.0,
            &json!({"operation": operation_id, "resource": resource_id, "phase": phase}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn fail_operation(
        &self,
        operation_id: &OperationId,
        error: &ShadeError,
        diagnostic: Option<&Diagnostic>,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let error_json = serde_json::to_string(error)?;
        if let Some(diagnostic) = diagnostic {
            insert_diagnostic(&transaction, diagnostic)?;
            crate::faults::hit(crate::faults::Point::DiagnosticWritten);
        }
        let now = now_ms();
        let changed = transaction.execute(
            "UPDATE operations SET state='failed', error_json=?2, updated_at_ms=?3 \
             WHERE id=?1 AND state='running'",
            params![operation_id.0, error_json, now],
        )?;
        if changed == 1 {
            append_event(
                &transaction,
                "operation.failed",
                &operation_id.0,
                &json!({"operation": operation_id, "error": error}),
            )?;
        }
        crate::faults::hit(crate::faults::Point::OperationFailureWritten);
        transaction.commit()?;
        crate::faults::hit(crate::faults::Point::OperationFailed);
        Ok(())
    }

    pub fn operation(&self, id: &OperationId) -> Result<Option<OperationRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, request_hash, state, intent_kind, outcome_json, error_json, \
                 created_at_ms, updated_at_ms FROM operations WHERE id=?1",
                params![id.0],
                operation_from_row,
            )
            .optional()
            .map(|value| value.map(|(record, _)| record))
            .map_err(Into::into)
    }

    pub fn operation_by_key(
        &self,
        actor_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<OperationRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, request_hash, state, intent_kind, outcome_json, error_json, \
                 created_at_ms, updated_at_ms FROM operations \
                 WHERE actor_id=?1 AND idempotency_key=?2",
                params![actor_id, idempotency_key],
                operation_from_row,
            )
            .optional()
            .map(|value| value.map(|(record, _)| record))
            .map_err(Into::into)
    }

    pub fn operation_actor(&self, id: &OperationId) -> Result<Option<String>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT actor_id FROM operations WHERE id=?1",
                params![id.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn upsert_repository(
        &self,
        identity: &str,
        bare_path: &Path,
        default_ref: Option<&str>,
    ) -> Result<RepositoryRecord, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT id FROM repositories WHERE identity=?1",
                params![identity],
                |row| row.get(0),
            )
            .optional()?;
        let id = existing.unwrap_or_else(|| format!("repo_{}", ulid::Ulid::new()));
        let now = now_ms();
        transaction.execute(
            "INSERT INTO repositories \
             (id, identity, bare_path, default_ref, state, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, 'ready', ?5, ?5) \
             ON CONFLICT(identity) DO UPDATE SET bare_path=excluded.bare_path, \
             default_ref=excluded.default_ref, state='ready', updated_at_ms=excluded.updated_at_ms",
            params![id, identity, bare_path.to_string_lossy(), default_ref, now],
        )?;
        append_event(
            &transaction,
            "repository.ready",
            &id,
            &json!({"repository": id, "default_ref": default_ref}),
        )?;
        transaction.commit()?;
        Ok(RepositoryRecord {
            id: RepositoryId(id),
            identity: identity.to_owned(),
            bare_path: bare_path.to_path_buf(),
            default_ref: default_ref.map(str::to_owned),
            state: "ready".into(),
        })
    }

    pub fn repository_by_id(&self, id: &RepositoryId) -> Result<Option<RepositoryRecord>, DbError> {
        self.repository_query("id", &id.0)
    }

    pub fn repository_by_identity(
        &self,
        identity: &str,
    ) -> Result<Option<RepositoryRecord>, DbError> {
        self.repository_query("identity", identity)
    }

    pub fn repositories(&self) -> Result<Vec<RepositoryRecord>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, identity, bare_path, default_ref, state FROM repositories ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(RepositoryRecord {
                id: RepositoryId(row.get(0)?),
                identity: row.get(1)?,
                bare_path: PathBuf::from(row.get::<_, String>(2)?),
                default_ref: row.get(3)?,
                state: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn repository_query(
        &self,
        column: &str,
        value: &str,
    ) -> Result<Option<RepositoryRecord>, DbError> {
        let connection = self.connection()?;
        let sql = format!(
            "SELECT id, identity, bare_path, default_ref, state FROM repositories WHERE {column}=?1"
        );
        connection
            .query_row(&sql, params![value], |row| {
                Ok(RepositoryRecord {
                    id: RepositoryId(row.get(0)?),
                    identity: row.get(1)?,
                    bare_path: PathBuf::from(row.get::<_, String>(2)?),
                    default_ref: row.get(3)?,
                    state: row.get(4)?,
                })
            })
            .optional()
            .map_err(Into::into)
    }

    pub fn create_workspace(&self, record: &WorkspaceRecord) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let now = now_ms();
        transaction.execute(
            "INSERT INTO workspaces \
             (id, repository_id, session_id, path, base_ref, base_oid, head_oid, state, \
              predecessor_id, dependency_state, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![
                record.id.0,
                record.repository_id.0,
                record.session_id.as_ref().map(|value| &value.0),
                record.path.to_string_lossy(),
                record.base_ref,
                record.base_oid.0,
                record.head_oid.0,
                record.state,
                record.predecessor_id.as_ref().map(|value| &value.0),
                record.dependency_state,
                now,
            ],
        )?;
        append_event(
            &transaction,
            "workspace.created",
            &record.id.0,
            &json!({"workspace": record.id, "base_sha": record.base_oid}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn mark_workspace_ready(
        &self,
        id: &WorkspaceId,
        head_oid: &ObjectId,
        dependency_state: &str,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE workspaces SET state='ready', head_oid=?2, dependency_state=?3, \
             updated_at_ms=?4 WHERE id=?1",
            params![id.0, head_oid.0, dependency_state, now_ms()],
        )?;
        append_event(
            &transaction,
            "workspace.ready",
            &id.0,
            &json!({"workspace": id, "head_sha": head_oid}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn mark_workspace_state(&self, id: &WorkspaceId, state: &str) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE workspaces SET state=?2, updated_at_ms=?3 WHERE id=?1",
            params![id.0, state, now_ms()],
        )?;
        append_event(
            &transaction,
            &format!("workspace.{state}"),
            &id.0,
            &json!({"workspace": id}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Mark a workspace `failed`, recording where it came from and why.
    ///
    /// Reconciliation demotes a workspace whose tree or registration no longer
    /// checks out, and `failed` is collectible. Demoting a *dormant* workspace
    /// therefore turns work a caller was told is safe into a GC candidate, so
    /// the event has to say more than "failed": it names the state that was
    /// lost and the check that failed. Returns the previous state.
    pub fn fail_workspace(&self, id: &WorkspaceId, reason: &str) -> Result<String, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: String = transaction.query_row(
            "SELECT state FROM workspaces WHERE id=?1",
            params![id.0],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE workspaces SET state='failed', updated_at_ms=?2 WHERE id=?1",
            params![id.0, now_ms()],
        )?;
        append_event(
            &transaction,
            "workspace.failed",
            &id.0,
            &json!({"workspace": id, "from": previous, "reason": reason}),
        )?;
        transaction.commit()?;
        Ok(previous)
    }

    pub fn set_workspace_head(&self, id: &WorkspaceId, head_oid: &ObjectId) -> Result<(), DbError> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE workspaces SET head_oid=?2, updated_at_ms=?3 WHERE id=?1",
            params![id.0, head_oid.0, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_dependency_state(&self, id: &WorkspaceId, state: &str) -> Result<(), DbError> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE workspaces SET dependency_state=?2, updated_at_ms=?3 WHERE id=?1",
            params![id.0, state, now_ms()],
        )?;
        Ok(())
    }

    pub fn workspace(&self, id: &WorkspaceId) -> Result<Option<WorkspaceRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, repository_id, session_id, path, base_ref, base_oid, head_oid, \
                 state, predecessor_id, dependency_state FROM workspaces WHERE id=?1",
                params![id.0],
                workspace_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn workspaces(&self) -> Result<Vec<WorkspaceRecord>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, repository_id, session_id, path, base_ref, base_oid, head_oid, \
             state, predecessor_id, dependency_state FROM workspaces ORDER BY id",
        )?;
        let rows = statement.query_map([], workspace_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn workspace_for_cwd(&self, cwd: &Path) -> Result<Option<WorkspaceRecord>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, repository_id, session_id, path, base_ref, base_oid, head_oid, \
             state, predecessor_id, dependency_state FROM workspaces \
             WHERE state NOT IN ('deleted','deleting')",
        )?;
        let mut best: Option<WorkspaceRecord> = None;
        let rows = statement.query_map([], workspace_from_row)?;
        for row in rows {
            let candidate = row?;
            if cwd.starts_with(&candidate.path)
                && best.as_ref().is_none_or(|current| {
                    candidate.path.components().count() > current.path.components().count()
                })
            {
                best = Some(candidate);
            }
        }
        Ok(best)
    }

    pub fn create_session_and_lease(
        &self,
        session: &SessionRecord,
        ttl_secs: i64,
    ) -> Result<LeaseRecord, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT id FROM sessions WHERE id=?1",
                params![session.id.0],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some() {
            return Err(DbError::Sql(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                Some("session already exists".into()),
            )));
        }
        let now = now_ms();
        transaction.execute(
            "INSERT INTO sessions \
             (id, repository_id, workspace_id, intent, state, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                session.id.0,
                session.repository_id.0,
                session.workspace_id.0,
                session.intent,
                session.state,
                now,
            ],
        )?;
        transaction.execute(
            "UPDATE workspaces SET session_id=?2, state='ready', updated_at_ms=?3 WHERE id=?1",
            params![session.workspace_id.0, session.id.0, now],
        )?;
        let lease = LeaseRecord {
            id: LeaseId(format!("lease_{}", ulid::Ulid::new())),
            session_id: session.id.clone(),
            workspace_id: session.workspace_id.clone(),
            fence: 1,
            heartbeat_at_ms: now,
            expires_at_ms: now + ttl_secs * 1000,
            released_at_ms: None,
        };
        transaction.execute(
            "INSERT INTO leases \
             (id, session_id, workspace_id, fence, heartbeat_at_ms, expires_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                lease.id.0,
                lease.session_id.0,
                lease.workspace_id.0,
                lease.fence,
                lease.heartbeat_at_ms,
                lease.expires_at_ms,
            ],
        )?;
        append_event(
            &transaction,
            "workspace.ready",
            &session.workspace_id.0,
            &json!({"workspace": session.workspace_id}),
        )?;
        append_event(
            &transaction,
            "lease.acquired",
            &lease.id.0,
            &json!({"lease": lease.id, "workspace": lease.workspace_id}),
        )?;
        transaction.commit()?;
        Ok(lease)
    }

    /// Resume a dormant session on its existing workspace under a fresh lease.
    ///
    /// One `IMMEDIATE` transaction frees the expired lease row, mints the
    /// successor fence and returns both the session and the workspace to their
    /// live states, so a crash on either side leaves a consistent Dormant or
    /// Active session and no fault point is needed.
    pub fn reattach_session(
        &self,
        session_id: &SessionId,
        expected_workspace: &WorkspaceId,
        lease_id: &LeaseId,
        ttl_secs: i64,
    ) -> Result<LeaseRecord, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        if transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM handoffs WHERE session_id=?1 AND state='pending')",
            params![session_id.0],
            |row| row.get::<_, bool>(0),
        )? {
            return Err(DbError::LeaseFenced);
        }
        // Release the expired row first: `live_lease_per_session` is a partial
        // unique index over unreleased leases, so the successor cannot be
        // inserted while the dead one still occupies it.
        transaction.execute(
            "UPDATE leases SET released_at_ms=?2 WHERE session_id=?1 \
             AND released_at_ms IS NULL AND expires_at_ms<?2",
            params![session_id.0, now],
        )?;
        let live = transaction
            .query_row(
                "SELECT id, session_id, workspace_id, fence, heartbeat_at_ms, expires_at_ms, \
                 released_at_ms FROM leases WHERE session_id=?1 AND released_at_ms IS NULL \
                 ORDER BY fence DESC LIMIT 1",
                params![session_id.0],
                lease_from_row,
            )
            .optional()?;
        // Reattaching an Active session is idempotent: the caller receives the
        // lease it already holds rather than a second one. It receives it with
        // a full TTL, though. A reattach is a caller announcing that it is
        // here, and its own heartbeat is one interval away; handing back
        // whatever was left of a lease that had been running down since the
        // last beat is how an `attach` ends in dormancy a moment later.
        if let Some(lease) = live {
            if lease.workspace_id != *expected_workspace {
                return Err(DbError::LeaseFenced);
            }
            let expires_at_ms = now + ttl_secs * 1000;
            transaction.execute(
                "UPDATE leases SET heartbeat_at_ms=?2, expires_at_ms=?3 WHERE id=?1",
                params![lease.id.0, now, expires_at_ms],
            )?;
            append_event(
                &transaction,
                "lease.heartbeat",
                &lease.id.0,
                &json!({"lease": lease.id, "expires_at_ms": expires_at_ms}),
            )?;
            transaction.commit()?;
            return Ok(LeaseRecord {
                heartbeat_at_ms: now,
                expires_at_ms,
                ..lease
            });
        }
        let session = transaction.execute(
            "UPDATE sessions SET state='active', updated_at_ms=?3 \
             WHERE id=?1 AND workspace_id=?2 AND state IN ('active','dormant')",
            params![session_id.0, expected_workspace.0, now],
        )?;
        if session != 1 {
            return Err(DbError::LeaseFenced);
        }
        // A workspace that is neither `ready` nor `dormant` is not fenced --
        // nothing superseded this caller -- it is mid-resolution, mid-handoff
        // or failed. Saying `LEASE_FENCED` and pointing at "the current
        // successor workspace" sent the caller looking for a successor that
        // does not exist.
        let state: String = transaction.query_row(
            "SELECT state FROM workspaces WHERE id=?1",
            params![expected_workspace.0],
            |row| row.get(0),
        )?;
        if !matches!(state.as_str(), "ready" | "dormant") {
            return Err(DbError::WorkspaceNotAttachable { state });
        }
        let workspace = transaction.execute(
            "UPDATE workspaces SET state='ready', session_id=?2, updated_at_ms=?3 \
             WHERE id=?1 AND state IN ('ready','dormant')",
            params![expected_workspace.0, session_id.0, now],
        )?;
        if workspace != 1 {
            return Err(DbError::LeaseFenced);
        }
        let fence: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(fence), 0) + 1 FROM leases WHERE session_id=?1",
            params![session_id.0],
            |row| row.get(0),
        )?;
        let lease = LeaseRecord {
            id: lease_id.clone(),
            session_id: session_id.clone(),
            workspace_id: expected_workspace.clone(),
            fence,
            heartbeat_at_ms: now,
            expires_at_ms: now + ttl_secs * 1000,
            released_at_ms: None,
        };
        transaction.execute(
            "INSERT INTO leases \
             (id, session_id, workspace_id, fence, heartbeat_at_ms, expires_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                lease.id.0,
                lease.session_id.0,
                lease.workspace_id.0,
                lease.fence,
                lease.heartbeat_at_ms,
                lease.expires_at_ms,
            ],
        )?;
        append_event(
            &transaction,
            "session.reattached",
            &session_id.0,
            &json!({"session": session_id, "workspace": expected_workspace, "lease": lease.id}),
        )?;
        append_event(
            &transaction,
            "lease.acquired",
            &lease.id.0,
            &json!({"lease": lease.id, "workspace": lease.workspace_id}),
        )?;
        transaction.commit()?;
        Ok(lease)
    }

    pub fn session(&self, id: &SessionId) -> Result<Option<SessionRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, repository_id, workspace_id, intent, state FROM sessions WHERE id=?1",
                params![id.0],
                |row| {
                    Ok(SessionRecord {
                        id: SessionId(row.get(0)?),
                        repository_id: RepositoryId(row.get(1)?),
                        workspace_id: WorkspaceId(row.get(2)?),
                        intent: row.get(3)?,
                        state: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn active_lease_for_session(&self, id: &SessionId) -> Result<Option<LeaseRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, session_id, workspace_id, fence, heartbeat_at_ms, expires_at_ms, \
                 released_at_ms FROM leases WHERE session_id=?1 AND released_at_ms IS NULL \
                 ORDER BY fence DESC LIMIT 1",
                params![id.0],
                lease_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn heartbeat(
        &self,
        session_id: &SessionId,
        lease_id: &LeaseId,
        ttl_secs: i64,
    ) -> Result<Option<LeaseRecord>, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let now = now_ms();
        let changed = transaction.execute(
            "UPDATE leases SET heartbeat_at_ms=?3, expires_at_ms=?4 \
             WHERE id=?1 AND session_id=?2 AND released_at_ms IS NULL AND expires_at_ms>=?3",
            params![lease_id.0, session_id.0, now, now + ttl_secs * 1000],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let lease = transaction.query_row(
            "SELECT id, session_id, workspace_id, fence, heartbeat_at_ms, expires_at_ms, \
             released_at_ms FROM leases WHERE id=?1",
            params![lease_id.0],
            lease_from_row,
        )?;
        append_event(
            &transaction,
            "lease.heartbeat",
            &lease_id.0,
            &json!({"lease": lease_id, "expires_at_ms": now + ttl_secs * 1000}),
        )?;
        transaction.commit()?;
        Ok(Some(lease))
    }

    /// Adoption clears the current session association on a predecessor, but
    /// its lease history still identifies the session which owned that view.
    pub fn workspace_session_owner(
        &self,
        workspace: &WorkspaceId,
    ) -> Result<Option<SessionId>, DbError> {
        let connection = self.connection()?;
        let session: Option<String> = connection.query_row(
            "SELECT COALESCE(w.session_id, (SELECT l.session_id FROM leases l \
             WHERE l.workspace_id=w.id ORDER BY l.fence DESC LIMIT 1)) FROM workspaces w WHERE w.id=?1",
            params![workspace.0], |row| row.get(0),
        ).optional()?.flatten();
        Ok(session.map(SessionId))
    }

    pub fn release_session(
        &self,
        session_id: &SessionId,
        expected_workspace: &WorkspaceId,
    ) -> Result<(), DbError> {
        self.release_session_in_state(session_id, expected_workspace, "released")
    }

    /// Release a session from any of its non-terminal lifecycle states.
    ///
    /// An Active session must still surrender a live lease, which is what
    /// fences a caller whose lease already rotated to a successor. A Dormant or
    /// Suspended session has no live lease to surrender — the sweep released it
    /// — so the lease predicate is dropped for those states rather than turning
    /// an explicit release into `LEASE_FENCED`.
    ///
    /// The lease itself decides which of those two a session is, exactly as
    /// `Engine::workspace_lifecycle` does. Reading it off `sessions.state`
    /// instead made the window between a lease expiring and the next sweep --
    /// bounded by 30 s in the daemon and unbounded for an embedded host that
    /// never sweeps -- a window in which the session row still said `active`,
    /// no live lease existed, and an ordinary `release` came back
    /// `LEASE_FENCED`.
    fn release_session_in_state(
        &self,
        session_id: &SessionId,
        expected_workspace: &WorkspaceId,
        workspace_state: &str,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        transaction
            .query_row(
                "SELECT state FROM sessions WHERE id=?1 AND workspace_id=?2",
                params![session_id.0, expected_workspace.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(DbError::LeaseFenced)?;
        // A lease naming another workspace is the fence: the caller is holding
        // a handle its own session already rotated away from.
        let current_lease = transaction
            .query_row(
                "SELECT workspace_id, expires_at_ms FROM leases WHERE session_id=?1 \
                 AND released_at_ms IS NULL ORDER BY fence DESC LIMIT 1",
                params![session_id.0],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        if let Some((workspace, _)) = &current_lease
            && workspace != &expected_workspace.0
        {
            return Err(DbError::LeaseFenced);
        }
        let live_lease_required = current_lease.is_some_and(|(_, expires)| expires >= now);
        let released = transaction.execute(
            "UPDATE leases SET released_at_ms=?3 \
             WHERE session_id=?1 AND workspace_id=?2 AND released_at_ms IS NULL \
               AND (?4=0 OR expires_at_ms>=?3) AND NOT EXISTS (SELECT 1 FROM handoffs h \
                 WHERE h.session_id=?1 AND h.predecessor_workspace_id=?2 AND h.state='pending')",
            params![
                session_id.0,
                expected_workspace.0,
                now,
                i64::from(live_lease_required)
            ],
        )?;
        let session = transaction.execute(
            "UPDATE sessions SET state='released', updated_at_ms=?3 \
             WHERE id=?1 AND workspace_id=?2 AND state IN ('active','dormant','suspended') \
               AND NOT EXISTS (SELECT 1 FROM handoffs h WHERE h.session_id=?1 \
                 AND h.predecessor_workspace_id=?2 AND h.state='pending')",
            params![session_id.0, expected_workspace.0, now],
        )?;
        if (live_lease_required && released != 1) || session != 1 {
            return Err(DbError::LeaseFenced);
        }
        crate::faults::hit(crate::faults::Point::ReleaseLeaseReleased);
        // The released lease and collectible/retained workspace are one state
        // transition. A crash must not strand a ready workspace without a lease
        // or make a user's retained workspace eligible for GC.
        let workspace = transaction.execute(
            "UPDATE workspaces SET state=?2, updated_at_ms=?3 WHERE id=?1",
            params![expected_workspace.0, workspace_state, now],
        )?;
        if workspace != 1 {
            return Err(DbError::LeaseFenced);
        }
        append_event(
            &transaction,
            "session.released",
            &session_id.0,
            &json!({"session": session_id}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn prepare_successor_handoff(
        &self,
        operation_id: &OperationId,
        actor_id: &str,
        session_id: &SessionId,
        old_workspace: &WorkspaceId,
        new_workspace: &WorkspaceId,
    ) -> Result<PendingHandoff, DbError> {
        let mut connection = self.connection()?;
        // Serialize the validation and insertion across daemon processes. The
        // partial unique index below remains the final database-level guard.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let handoff = Self::prepare_successor_in_transaction(
            &transaction,
            operation_id,
            actor_id,
            session_id,
            old_workspace,
            new_workspace,
        )?;
        transaction.commit()?;
        Ok(handoff)
    }

    fn prepare_successor_in_transaction(
        transaction: &Transaction<'_>,
        operation_id: &OperationId,
        actor_id: &str,
        session_id: &SessionId,
        old_workspace: &WorkspaceId,
        new_workspace: &WorkspaceId,
    ) -> Result<PendingHandoff, DbError> {
        let now = now_ms();
        let operation_actor: Option<String> = transaction
            .query_row(
                "SELECT actor_id FROM operations WHERE id=?1 AND state='running'",
                params![operation_id.0],
                |row| row.get(0),
            )
            .optional()?;
        if operation_actor.as_deref() != Some(actor_id) {
            return Err(DbError::HandoffOwnerMismatch);
        }
        let current: Option<String> = transaction
            .query_row(
                "SELECT workspace_id FROM sessions WHERE id=?1 AND state='active'",
                params![session_id.0],
                |row| row.get(0),
            )
            .optional()?;
        if current.as_deref() != Some(old_workspace.0.as_str()) {
            return Err(DbError::LeaseFenced);
        }
        let live_lease: Option<String> = transaction
            .query_row(
                "SELECT id FROM leases WHERE session_id=?1 AND workspace_id=?2 \
                 AND released_at_ms IS NULL AND expires_at_ms>=?3 ORDER BY fence DESC LIMIT 1",
                params![session_id.0, old_workspace.0, now],
                |row| row.get(0),
            )
            .optional()?;
        if live_lease.is_none() {
            return Err(DbError::LeaseFenced);
        }
        let existing_handoff: Option<String> = transaction
            .query_row(
                "SELECT id FROM handoffs WHERE state='pending' \
                   AND (session_id=?1 OR predecessor_workspace_id=?2) LIMIT 1",
                params![session_id.0, old_workspace.0],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(handoff_id) = existing_handoff {
            return Err(DbError::HandoffAlreadyPending {
                handoff_id: HandoffId(handoff_id),
            });
        }
        let successor_ready: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM workspaces WHERE id=?1 AND predecessor_id=?2 \
                 AND session_id IS NULL AND state IN ('materializing','resolution')",
                params![new_workspace.0, old_workspace.0],
                |row| row.get(0),
            )
            .optional()?;
        if successor_ready.is_none() {
            return Err(DbError::LeaseFenced);
        }
        let handoff = PendingHandoff {
            handoff_id: HandoffId(format!("handoff_{}", ulid::Ulid::new())),
            session: session_id.clone(),
            predecessor: old_workspace.clone(),
            successor: new_workspace.clone(),
        };
        transaction.execute(
            "INSERT INTO handoffs \
             (id, operation_id, actor_id, session_id, predecessor_workspace_id, \
              successor_workspace_id, state, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
            params![
                handoff.handoff_id.0,
                operation_id.0,
                actor_id,
                session_id.0,
                old_workspace.0,
                new_workspace.0,
                now,
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE workspaces SET state='handoff_pending', updated_at_ms=?2 \
             WHERE id=?1 AND session_id IS NULL AND state IN ('materializing','resolution')",
            params![new_workspace.0, now],
        )?;
        if changed != 1 {
            return Err(DbError::LeaseFenced);
        }
        let outcome = Outcome::Completed(serde_json::to_value(&handoff)?);
        finish_operation_in_transaction(transaction, operation_id, &outcome)?;
        append_event(
            transaction,
            "successor.prepared",
            &new_workspace.0,
            &json!({
                "handoff": handoff.handoff_id,
                "session": session_id,
                "predecessor": old_workspace,
                "workspace": new_workspace,
            }),
        )?;
        Ok(handoff)
    }

    pub fn handoff(&self, id: &HandoffId) -> Result<Option<HandoffRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, operation_id, actor_id, session_id, predecessor_workspace_id, \
                 successor_workspace_id, state FROM handoffs WHERE id=?1",
                params![id.0],
                handoff_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn adopt_successor_handoff(
        &self,
        handoff_id: &HandoffId,
        actor_id: &str,
        operation_id: &OperationId,
        lease_id: &LeaseId,
        ttl_secs: i64,
        outcome: &Outcome,
    ) -> Result<LeaseRecord, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let now = now_ms();
        let handoff = transaction
            .query_row(
                "SELECT id, operation_id, actor_id, session_id, predecessor_workspace_id, \
                 successor_workspace_id, state FROM handoffs WHERE id=?1",
                params![handoff_id.0],
                handoff_from_row,
            )
            .optional()?
            .ok_or(DbError::HandoffNotFound)?;
        if handoff.actor_id != actor_id {
            return Err(DbError::HandoffOwnerMismatch);
        }
        if handoff.state != "pending" {
            return Err(DbError::HandoffNotPending);
        }
        let operation_actor: Option<String> = transaction
            .query_row(
                "SELECT actor_id FROM operations WHERE id=?1 AND state='running'",
                params![operation_id.0],
                |row| row.get(0),
            )
            .optional()?;
        if operation_actor.as_deref() != Some(actor_id) {
            return Err(DbError::HandoffOwnerMismatch);
        }
        let current: Option<String> = transaction
            .query_row(
                "SELECT workspace_id FROM sessions WHERE id=?1 AND state='active'",
                params![handoff.session_id.0],
                |row| row.get(0),
            )
            .optional()?;
        if current.as_deref() != Some(handoff.predecessor_workspace_id.0.as_str()) {
            return Err(DbError::LeaseFenced);
        }
        let live_lease: Option<String> = transaction
            .query_row(
                "SELECT id FROM leases WHERE session_id=?1 AND workspace_id=?2 \
                 AND released_at_ms IS NULL AND expires_at_ms>=?3 ORDER BY fence DESC LIMIT 1",
                params![
                    handoff.session_id.0,
                    handoff.predecessor_workspace_id.0,
                    now
                ],
                |row| row.get(0),
            )
            .optional()?;
        if live_lease.is_none() {
            return Err(DbError::LeaseFenced);
        }
        let previous_fence: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(fence), 0) FROM leases WHERE session_id=?1",
            params![handoff.session_id.0],
            |row| row.get(0),
        )?;
        let released = transaction.execute(
            "UPDATE leases SET released_at_ms=?3 WHERE session_id=?1 AND workspace_id=?2 \
             AND released_at_ms IS NULL AND expires_at_ms>=?3",
            params![
                handoff.session_id.0,
                handoff.predecessor_workspace_id.0,
                now
            ],
        )?;
        if released != 1 {
            return Err(DbError::LeaseFenced);
        }
        let old_changed = transaction.execute(
            "UPDATE workspaces SET session_id=NULL, state='released', updated_at_ms=?2 WHERE id=?1",
            params![handoff.predecessor_workspace_id.0, now],
        )?;
        let new_changed = transaction.execute(
            "UPDATE workspaces SET session_id=?2, state='ready', updated_at_ms=?3 \
             WHERE id=?1 AND predecessor_id=?4 AND session_id IS NULL \
               AND state='handoff_pending'",
            params![
                handoff.successor_workspace_id.0,
                handoff.session_id.0,
                now,
                handoff.predecessor_workspace_id.0
            ],
        )?;
        let session_changed = transaction.execute(
            "UPDATE sessions SET workspace_id=?2, state='active', updated_at_ms=?3 \
             WHERE id=?1 AND workspace_id=?4 AND state='active'",
            params![
                handoff.session_id.0,
                handoff.successor_workspace_id.0,
                now,
                handoff.predecessor_workspace_id.0
            ],
        )?;
        if old_changed != 1 || new_changed != 1 || session_changed != 1 {
            return Err(DbError::LeaseFenced);
        }
        let lease = LeaseRecord {
            id: lease_id.clone(),
            session_id: handoff.session_id.clone(),
            workspace_id: handoff.successor_workspace_id.clone(),
            fence: previous_fence + 1,
            heartbeat_at_ms: now,
            expires_at_ms: now + ttl_secs * 1000,
            released_at_ms: None,
        };
        transaction.execute(
            "INSERT INTO leases \
             (id, session_id, workspace_id, fence, heartbeat_at_ms, expires_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                lease.id.0,
                lease.session_id.0,
                lease.workspace_id.0,
                lease.fence,
                lease.heartbeat_at_ms,
                lease.expires_at_ms,
            ],
        )?;
        transaction.execute(
            "UPDATE handoffs SET state='adopted', adopted_at_ms=?2 WHERE id=?1 AND state='pending'",
            params![handoff_id.0, now],
        )?;
        finish_operation_in_transaction(&transaction, operation_id, outcome)?;
        append_event(
            &transaction,
            "successor.activated",
            &handoff.successor_workspace_id.0,
            &json!({
                "handoff": handoff_id,
                "session": handoff.session_id,
                "predecessor": handoff.predecessor_workspace_id,
                "workspace": handoff.successor_workspace_id,
                "lease": lease.id,
            }),
        )?;
        transaction.commit()?;
        Ok(lease)
    }

    /// Finish a suspension: `suspending` becomes `suspended` and the session
    /// follows it, in one transaction with the lease release.
    ///
    /// Idempotent, because the reconciliation pass that finishes an
    /// interrupted sleep calls exactly this.
    pub fn suspend_workspace(
        &self,
        workspace_id: &WorkspaceId,
        checkpoint_id: &CheckpointId,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let state: String = transaction
            .query_row(
                "SELECT state FROM workspaces WHERE id=?1",
                params![workspace_id.0],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(DbError::LeaseFenced)?;
        if state == "suspended" {
            return Ok(());
        }
        if state != "suspending" {
            return Err(DbError::LeaseFenced);
        }
        transaction.execute(
            "UPDATE leases SET released_at_ms=?2 WHERE workspace_id=?1 AND released_at_ms IS NULL",
            params![workspace_id.0, now],
        )?;
        let workspace = transaction.execute(
            "UPDATE workspaces SET state='suspended', updated_at_ms=?2 \
             WHERE id=?1 AND state='suspending'",
            params![workspace_id.0, now],
        )?;
        if workspace != 1 {
            return Err(DbError::LeaseFenced);
        }
        transaction.execute(
            "UPDATE sessions SET state='suspended', updated_at_ms=?2 \
             WHERE workspace_id=?1 AND state IN ('active','dormant')",
            params![workspace_id.0, now],
        )?;
        append_event(
            &transaction,
            "workspace.suspended",
            &workspace_id.0,
            &json!({"workspace": workspace_id, "checkpoint": checkpoint_id}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Undo a half-finished suspension, returning the workspace to Dormant.
    ///
    /// Used by reconciliation when a `suspending` workspace still has its tree
    /// but no sleep checkpoint to restore from. The session and the lease move
    /// with it so the result is a consistent Dormant workspace rather than an
    /// active session with no lease.
    pub fn restore_suspending_workspace(&self, workspace_id: &WorkspaceId) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let changed = transaction.execute(
            "UPDATE workspaces SET state='dormant', updated_at_ms=?2 \
             WHERE id=?1 AND state='suspending'",
            params![workspace_id.0, now],
        )?;
        if changed != 1 {
            return Ok(());
        }
        transaction.execute(
            "UPDATE leases SET released_at_ms=?2 WHERE workspace_id=?1 AND released_at_ms IS NULL",
            params![workspace_id.0, now],
        )?;
        transaction.execute(
            "UPDATE sessions SET state='dormant', updated_at_ms=?2 \
             WHERE workspace_id=?1 AND state='active'",
            params![workspace_id.0, now],
        )?;
        append_event(
            &transaction,
            "workspace.dormant",
            &workspace_id.0,
            &json!({"workspace": workspace_id}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Bind a woken session to the successor materialized from its suspension
    /// checkpoint.
    ///
    /// A sibling of [`Database::adopt_successor_handoff`], not a use of it:
    /// adoption requires a live lease on the predecessor, and a suspended
    /// workspace by definition has none. The preconditions become "session is
    /// suspended on the predecessor, predecessor is suspended, successor was
    /// materialized for it", and the whole swap — predecessor released,
    /// successor bound, session active, new lease at `fence + 1` — is one
    /// transaction, exactly like adoption.
    #[allow(clippy::too_many_arguments)]
    pub fn activate_woken_workspace(
        &self,
        session_id: &SessionId,
        predecessor: &WorkspaceId,
        successor: &WorkspaceId,
        operation_id: &OperationId,
        lease_id: &LeaseId,
        ttl_secs: i64,
        outcome: &Outcome,
    ) -> Result<LeaseRecord, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let current: Option<String> = transaction
            .query_row(
                "SELECT workspace_id FROM sessions WHERE id=?1 AND state='suspended'",
                params![session_id.0],
                |row| row.get(0),
            )
            .optional()?;
        if current.as_deref() != Some(predecessor.0.as_str()) {
            return Err(DbError::LeaseFenced);
        }
        let previous_fence: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(fence), 0) FROM leases WHERE session_id=?1",
            params![session_id.0],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE leases SET released_at_ms=?2 WHERE session_id=?1 AND released_at_ms IS NULL",
            params![session_id.0, now],
        )?;
        let old_changed = transaction.execute(
            "UPDATE workspaces SET session_id=NULL, state='released', updated_at_ms=?2 \
             WHERE id=?1 AND state='suspended'",
            params![predecessor.0, now],
        )?;
        let new_changed = transaction.execute(
            "UPDATE workspaces SET session_id=?2, state='ready', updated_at_ms=?3 \
             WHERE id=?1 AND predecessor_id=?4 AND session_id IS NULL \
               AND state IN ('materializing','handoff_pending')",
            params![successor.0, session_id.0, now, predecessor.0],
        )?;
        let session_changed = transaction.execute(
            "UPDATE sessions SET workspace_id=?2, state='active', updated_at_ms=?3 \
             WHERE id=?1 AND workspace_id=?4 AND state='suspended'",
            params![session_id.0, successor.0, now, predecessor.0],
        )?;
        if old_changed != 1 || new_changed != 1 || session_changed != 1 {
            return Err(DbError::LeaseFenced);
        }
        let lease = LeaseRecord {
            id: lease_id.clone(),
            session_id: session_id.clone(),
            workspace_id: successor.clone(),
            fence: previous_fence + 1,
            heartbeat_at_ms: now,
            expires_at_ms: now + ttl_secs * 1000,
            released_at_ms: None,
        };
        transaction.execute(
            "INSERT INTO leases \
             (id, session_id, workspace_id, fence, heartbeat_at_ms, expires_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                lease.id.0,
                lease.session_id.0,
                lease.workspace_id.0,
                lease.fence,
                lease.heartbeat_at_ms,
                lease.expires_at_ms,
            ],
        )?;
        finish_operation_in_transaction(&transaction, operation_id, outcome)?;
        append_event(
            &transaction,
            "session.woken",
            &session_id.0,
            &json!({
                "session": session_id,
                "predecessor": predecessor,
                "workspace": successor,
                "lease": lease.id,
            }),
        )?;
        transaction.commit()?;
        Ok(lease)
    }

    pub fn create_checkpoint(&self, record: &CheckpointRecord) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let now = now_ms();
        transaction.execute(
            "INSERT INTO checkpoints \
             (id, workspace_id, head_oid, index_oid, worktree_oid, reason, state, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                record.id.0,
                record.workspace_id.0,
                record.head_oid.0,
                record.index_oid.0,
                record.worktree_oid.0,
                record.reason,
                record.state,
                now,
            ],
        )?;
        append_event(
            &transaction,
            "checkpoint.created",
            &record.id.0,
            &json!({"checkpoint": record.id, "workspace": record.workspace_id}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn checkpoint(&self, id: &CheckpointId) -> Result<Option<CheckpointRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, workspace_id, head_oid, index_oid, worktree_oid, reason, state \
                 FROM checkpoints WHERE id=?1",
                params![id.0],
                |row| {
                    Ok(CheckpointRecord {
                        id: CheckpointId(row.get(0)?),
                        workspace_id: WorkspaceId(row.get(1)?),
                        head_oid: ObjectId(row.get(2)?),
                        index_oid: ObjectId(row.get(3)?),
                        worktree_oid: ObjectId(row.get(4)?),
                        reason: row.get(5)?,
                        state: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn has_ready_checkpoint_for_head(
        &self,
        workspace_id: &WorkspaceId,
        head_oid: &ObjectId,
    ) -> Result<bool, DbError> {
        let connection = self.connection()?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM checkpoints \
             WHERE workspace_id=?1 AND head_oid=?2 AND state='ready'",
            params![workspace_id.0, head_oid.0],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// The checkpoint a suspended workspace restores from.
    ///
    /// It is the newest ready `sleep` checkpoint, which is deterministic: a
    /// suspended workspace has no tree and therefore cannot produce another
    /// one. Keeping the rule here means `wake`, `doctor` and reconciliation
    /// all agree on which checkpoint a suspension owns.
    pub fn suspension_checkpoint(
        &self,
        workspace: &WorkspaceId,
    ) -> Result<Option<CheckpointRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, workspace_id, head_oid, index_oid, worktree_oid, reason, state \
                 FROM checkpoints WHERE workspace_id=?1 AND state='ready' AND reason='sleep' \
                 ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
                params![workspace.0],
                |row| {
                    Ok(CheckpointRecord {
                        id: CheckpointId(row.get(0)?),
                        workspace_id: WorkspaceId(row.get(1)?),
                        head_oid: ObjectId(row.get(2)?),
                        index_oid: ObjectId(row.get(3)?),
                        worktree_oid: ObjectId(row.get(4)?),
                        reason: row.get(5)?,
                        state: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn checkpoints_for_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<CheckpointRecord>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, workspace_id, head_oid, index_oid, worktree_oid, reason, state \
             FROM checkpoints WHERE workspace_id=?1 ORDER BY created_at_ms",
        )?;
        let rows = statement.query_map(params![workspace_id.0], |row| {
            Ok(CheckpointRecord {
                id: CheckpointId(row.get(0)?),
                workspace_id: WorkspaceId(row.get(1)?),
                head_oid: ObjectId(row.get(2)?),
                index_oid: ObjectId(row.get(3)?),
                worktree_oid: ObjectId(row.get(4)?),
                reason: row.get(5)?,
                state: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn create_review(&self, record: &ReviewRecord) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        if record.kind == "secret_cleanup" {
            transaction.execute("UPDATE reviews SET state='superseded' WHERE workspace_id=?1 AND kind='secret_cleanup' AND state='pending'", params![record.workspace_id.0])?;
        }
        transaction.execute(
            "INSERT INTO reviews (id, workspace_id, kind, payload_json, state, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.id.0,
                record.workspace_id.0,
                record.kind,
                serde_json::to_string(&record.payload)?,
                record.state,
                now_ms(),
            ],
        )?;
        append_event(
            &transaction,
            "review.required",
            &record.id.0,
            &json!({"review": record.id, "workspace": record.workspace_id, "kind": record.kind, "files": record.payload.get("files")}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn latest_secret_review(
        &self,
        workspace: &WorkspaceId,
    ) -> Result<Option<ReviewRecord>, DbError> {
        let id: Option<String> = {
            let connection = self.connection()?;
            connection.query_row(
                "SELECT id FROM reviews WHERE workspace_id=?1 AND kind='secret_cleanup' ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
                params![workspace.0], |row| row.get(0),
            ).optional()?
        };
        match id {
            Some(id) => self.review(&ReviewId(id)),
            None => Ok(None),
        }
    }

    pub fn review(&self, id: &ReviewId) -> Result<Option<ReviewRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, workspace_id, kind, payload_json, state FROM reviews WHERE id=?1",
                params![id.0],
                |row| {
                    let payload: String = row.get(3)?;
                    Ok(ReviewRecord {
                        id: ReviewId(row.get(0)?),
                        workspace_id: WorkspaceId(row.get(1)?),
                        kind: row.get(2)?,
                        payload: serde_json::from_str(&payload).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                payload.len(),
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                        state: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn resolve_review(&self, id: &ReviewId, state: &str) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE reviews SET state=?2, resolved_at_ms=?3 WHERE id=?1 AND state='pending'",
            params![id.0, state, now_ms()],
        )?;
        append_event(
            &transaction,
            "review.resolved",
            &id.0,
            &json!({"review": id, "resolution": state}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// A secret decision, lease disposition and idempotent result have one
    /// commit. Pending reviews remain actionable even after lease expiry.
    pub fn complete_secret_review(
        &self,
        review_id: &ReviewId,
        keep: bool,
        operation: &OperationId,
        outcome: &Outcome,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let workspace = review_workspace(&transaction, review_id)?;
        close_reviewed_workspace(
            &transaction,
            &workspace,
            if keep { "retained" } else { "released" },
        )?;
        finish_review_in_transaction(
            &transaction,
            review_id,
            if keep { "kept" } else { "discarded" },
        )?;
        crate::faults::hit(crate::faults::Point::ReviewDecisionWritten);
        finish_operation_in_transaction(&transaction, operation, outcome)?;
        transaction.commit()?;
        crate::faults::hit(crate::faults::Point::ReviewDecisionCompleted);
        Ok(())
    }

    /// Persist the merge decision and its handoff together. If the child is an
    /// adopted successor of the same session, hand off from that current child;
    /// otherwise close the fork's lease and hand off the parent's session.
    pub fn prepare_secret_successor_handoff(
        &self,
        operation: &OperationId,
        actor: &str,
        review_id: &ReviewId,
        successor: &WorkspaceId,
    ) -> Result<PendingHandoff, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let child = review_workspace(&transaction, review_id)?;
        let (parent, parent_session, child_session): (String, String, String) = transaction
            .query_row(
            "SELECT p.id, COALESCE(p.session_id, (SELECT l.session_id FROM leases l \
             WHERE l.workspace_id=p.id ORDER BY l.fence DESC LIMIT 1)), c.session_id FROM workspaces c \
             JOIN workspaces p ON p.id=c.predecessor_id \
             JOIN workspaces n ON n.id=?2 AND n.predecessor_id=p.id WHERE c.id=?1",
                params![child.0, successor.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let same_session = parent_session == child_session;
        let predecessor = if same_session {
            transaction.execute(
                "UPDATE workspaces SET predecessor_id=?2 WHERE id=?1",
                params![successor.0, child.0],
            )?;
            child.clone()
        } else {
            WorkspaceId(parent)
        };
        let handoff = Self::prepare_successor_in_transaction(
            &transaction,
            operation,
            actor,
            &SessionId(parent_session),
            &predecessor,
            successor,
        )?;
        if !same_session {
            close_reviewed_workspace(&transaction, &child, "released")?;
        }
        finish_review_in_transaction(&transaction, review_id, "merged")?;
        crate::faults::hit(crate::faults::Point::SecretHandoffWritten);
        transaction.commit()?;
        crate::faults::hit(crate::faults::Point::SecretHandoffCompleted);
        Ok(handoff)
    }

    /// Persist every input needed to replay a publish before Git is allowed to
    /// create or move its public target ref. This transaction also advances
    /// the generic operation journal and emits the corresponding outbox row.
    pub fn create_publish_intent(&self, record: &PublishIntentRecord) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation_running: Option<i64> = transaction
            .query_row(
                "SELECT 1 FROM operations WHERE id=?1 AND state='running'",
                params![record.operation_id.0],
                |row| row.get(0),
            )
            .optional()?;
        if operation_running.is_none() {
            return Err(DbError::LeaseFenced);
        }
        let now = now_ms();
        transaction.execute(
            "INSERT INTO publish_operations \
             (operation_id, workspace_id, repository_id, checkpoint_id, source_kind, branch, \
              message, push, original_base_oid, expected_remote_oid, expected_local_oid, \
              anchor_ref, commit_oid, tree_oid, state, anchor_cleaned, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?17)",
            params![
                record.operation_id.0,
                record.workspace_id.0,
                record.repository_id.0,
                record.checkpoint_id.0,
                record.source_kind,
                record.branch,
                record.message,
                i64::from(record.push),
                record.original_base_oid.0,
                record.expected_remote_oid.as_ref().map(|oid| &oid.0),
                record.expected_local_oid.as_ref().map(|oid| &oid.0),
                record.anchor_ref,
                record.commit_oid.as_ref().map(|oid| &oid.0),
                record.tree_oid.as_ref().map(|oid| &oid.0),
                record.state,
                i64::from(record.anchor_cleaned),
                now,
            ],
        )?;
        transaction.execute(
            "UPDATE operations SET resource_id=?2, phase='publish_planned', updated_at_ms=?3 \
             WHERE id=?1 AND state='running'",
            params![record.operation_id.0, record.workspace_id.0, now],
        )?;
        append_event(
            &transaction,
            "publish.planned",
            &record.operation_id.0,
            &json!({
                "operation": record.operation_id,
                "workspace": record.workspace_id,
                "branch": record.branch,
                "push": record.push,
            }),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn publish_intent(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<PublishIntentRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT operation_id, workspace_id, repository_id, checkpoint_id, source_kind, \
                 branch, message, push, original_base_oid, expected_remote_oid, \
                 expected_local_oid, anchor_ref, commit_oid, tree_oid, state, anchor_cleaned \
                 FROM publish_operations WHERE operation_id=?1",
                params![operation_id.0],
                publish_intent_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn publish_intents_for_recovery(
        &self,
        current_operation: &OperationId,
    ) -> Result<Vec<PublishIntentRecord>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT p.operation_id, p.workspace_id, p.repository_id, p.checkpoint_id, \
             p.source_kind, p.branch, p.message, p.push, p.original_base_oid, \
             p.expected_remote_oid, p.expected_local_oid, p.anchor_ref, p.commit_oid, \
             p.tree_oid, p.state, p.anchor_cleaned FROM publish_operations p \
             JOIN operations o ON o.id=p.operation_id \
             WHERE p.operation_id!=?1 AND (o.state='running' OR p.anchor_cleaned=0) \
             ORDER BY p.created_at_ms, p.operation_id",
        )?;
        let rows = statement.query_map(params![current_operation.0], publish_intent_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn mark_publish_prepared(
        &self,
        operation_id: &OperationId,
        commit: &ObjectId,
        tree: &ObjectId,
    ) -> Result<(), DbError> {
        self.advance_publish(
            operation_id,
            &["planned", "prepared"],
            "prepared",
            Some((commit, tree)),
            "publish.prepared",
        )
    }

    pub fn mark_publish_local_applied(&self, operation_id: &OperationId) -> Result<(), DbError> {
        self.advance_publish(
            operation_id,
            &["prepared", "local_applied"],
            "local_applied",
            None,
            "publish.local_applied",
        )
    }

    pub fn mark_publish_remote_applied(&self, operation_id: &OperationId) -> Result<(), DbError> {
        self.advance_publish(
            operation_id,
            &["local_applied", "remote_applied"],
            "remote_applied",
            None,
            "publish.remote_applied",
        )
    }

    fn advance_publish(
        &self,
        operation_id: &OperationId,
        allowed: &[&str],
        next_state: &str,
        prepared: Option<(&ObjectId, &ObjectId)>,
        event: &str,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let current: Option<String> = transaction
            .query_row(
                "SELECT state FROM publish_operations WHERE operation_id=?1",
                params![operation_id.0],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(DbError::LeaseFenced);
        };
        if current == next_state {
            transaction.commit()?;
            return Ok(());
        }
        if !allowed.contains(&current.as_str()) {
            return Err(DbError::LeaseFenced);
        }
        let now = now_ms();
        let changed = if let Some((commit, tree)) = prepared {
            transaction.execute(
                "UPDATE publish_operations SET state=?2, commit_oid=?3, tree_oid=?4, \
                 updated_at_ms=?5 WHERE operation_id=?1 AND state=?6",
                params![operation_id.0, next_state, commit.0, tree.0, now, current],
            )?
        } else {
            transaction.execute(
                "UPDATE publish_operations SET state=?2, updated_at_ms=?3 \
                 WHERE operation_id=?1 AND state=?4",
                params![operation_id.0, next_state, now, current],
            )?
        };
        if changed != 1 {
            return Err(DbError::LeaseFenced);
        }
        transaction.execute(
            "UPDATE operations SET phase=?2, updated_at_ms=?3 WHERE id=?1 AND state='running'",
            params![operation_id.0, next_state, now],
        )?;
        append_event(
            &transaction,
            event,
            &operation_id.0,
            &json!({"operation": operation_id, "phase": next_state}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Commit the publish content state and, for a direct publish, the
    /// operation outcome/outbox in the same SQLite transaction. Resolution
    /// completion deliberately keeps the operation running until its handoff
    /// is durably prepared.
    pub fn complete_publish_intent(
        &self,
        operation_id: &OperationId,
        outcome: Option<&Outcome>,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let source: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT source_kind, workspace_id, state FROM publish_operations \
                 WHERE operation_id=?1",
                params![operation_id.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((source_kind, workspace_id, state)) = source else {
            return Err(DbError::LeaseFenced);
        };
        if !matches!(
            state.as_str(),
            "local_applied" | "remote_applied" | "completed"
        ) {
            return Err(DbError::LeaseFenced);
        }
        let now = now_ms();
        if state != "completed" {
            transaction.execute(
                "UPDATE publish_operations SET state='completed', updated_at_ms=?2 \
                 WHERE operation_id=?1",
                params![operation_id.0, now],
            )?;
            if source_kind == "resolution_complete" {
                let changed = transaction.execute(
                    "UPDATE publish_resolutions SET state='completed', resolved_at_ms=?2 \
                     WHERE workspace_id=?1 AND state='pending'",
                    params![workspace_id, now],
                )?;
                if changed != 1 {
                    return Err(DbError::LeaseFenced);
                }
                append_event(
                    &transaction,
                    "publish.resolution_completed",
                    &workspace_id,
                    &json!({"workspace": workspace_id}),
                )?;
            }
            append_event(
                &transaction,
                "publish.completed",
                &operation_id.0,
                &json!({"operation": operation_id, "workspace": workspace_id}),
            )?;
        }
        if let Some(outcome) = outcome {
            finish_operation_in_transaction(&transaction, operation_id, outcome)?;
        } else {
            transaction.execute(
                "UPDATE operations SET phase='publish_completed', updated_at_ms=?2 \
                 WHERE id=?1 AND state='running'",
                params![operation_id.0, now],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn abort_publish_intent(
        &self,
        operation_id: &OperationId,
        state: &str,
    ) -> Result<(), DbError> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE publish_operations SET state=?2, updated_at_ms=?3 \
             WHERE operation_id=?1 AND state!='completed'",
            params![operation_id.0, state, now_ms()],
        )?;
        Ok(())
    }

    pub fn fail_publish_intent_and_operation(
        &self,
        operation_id: &OperationId,
        error: &ShadeError,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let now = now_ms();
        transaction.execute(
            "UPDATE publish_operations SET state='failed', updated_at_ms=?2 \
             WHERE operation_id=?1 AND state!='completed'",
            params![operation_id.0, now],
        )?;
        let changed = transaction.execute(
            "UPDATE operations SET state='failed', error_json=?2, updated_at_ms=?3 \
             WHERE id=?1 AND state='running'",
            params![operation_id.0, serde_json::to_string(error)?, now],
        )?;
        if changed == 1 {
            append_event(
                &transaction,
                "publish.recovery_failed",
                &operation_id.0,
                &json!({"operation": operation_id, "error": error}),
            )?;
            append_event(
                &transaction,
                "operation.failed",
                &operation_id.0,
                &json!({"operation": operation_id, "error": error}),
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn mark_publish_anchor_cleaned(&self, operation_id: &OperationId) -> Result<(), DbError> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE publish_operations SET anchor_cleaned=1, updated_at_ms=?2 \
             WHERE operation_id=?1",
            params![operation_id.0, now_ms()],
        )?;
        Ok(())
    }

    pub fn create_publish_resolution(
        &self,
        record: &PublishResolutionRecord,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        // A visible resolution must retain its original publish target and CAS
        // expectations. Keep state, intent and both events in one transaction.
        transaction.execute(
            "UPDATE workspaces SET state='resolution', dependency_state='blocked', \
             updated_at_ms=?2 WHERE id=?1",
            params![record.workspace_id.0, now_ms()],
        )?;
        append_event(
            &transaction,
            "workspace.resolution",
            &record.workspace_id.0,
            &json!({"workspace": record.workspace_id}),
        )?;
        crate::faults::hit(crate::faults::Point::PublishResolutionStateWritten);
        transaction.execute(
            "INSERT INTO publish_resolutions \
             (workspace_id, parent_workspace_id, branch, message, push, expected_remote_oid, \
              expected_local_oid, state, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.workspace_id.0,
                record.parent_workspace_id.0,
                record.branch,
                record.message,
                i64::from(record.push),
                record.expected_remote_oid.0,
                record.expected_local_oid.as_ref().map(|oid| &oid.0),
                record.state,
                now_ms(),
            ],
        )?;
        append_event(
            &transaction,
            "publish.resolution_required",
            &record.workspace_id.0,
            &json!({
                "workspace": record.workspace_id,
                "parent": record.parent_workspace_id,
                "branch": record.branch,
                "push": record.push,
            }),
        )?;
        crate::faults::hit(crate::faults::Point::PublishResolutionIntentWritten);
        transaction.commit()?;
        crate::faults::hit(crate::faults::Point::PublishResolutionReady);
        Ok(())
    }

    pub fn publish_resolution(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Option<PublishResolutionRecord>, DbError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT workspace_id, parent_workspace_id, branch, message, push, \
                 expected_remote_oid, expected_local_oid, state \
                 FROM publish_resolutions WHERE workspace_id=?1",
                params![workspace_id.0],
                |row| {
                    Ok(PublishResolutionRecord {
                        workspace_id: WorkspaceId(row.get(0)?),
                        parent_workspace_id: WorkspaceId(row.get(1)?),
                        branch: row.get(2)?,
                        message: row.get(3)?,
                        push: row.get::<_, i64>(4)? != 0,
                        expected_remote_oid: ObjectId(row.get(5)?),
                        expected_local_oid: row.get::<_, Option<String>>(6)?.map(ObjectId),
                        state: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn complete_publish_resolution(&self, workspace_id: &WorkspaceId) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE publish_resolutions SET state='completed', resolved_at_ms=?2 \
             WHERE workspace_id=?1 AND state='pending'",
            params![workspace_id.0, now_ms()],
        )?;
        if changed != 1 {
            return Err(DbError::LeaseFenced);
        }
        append_event(
            &transaction,
            "publish.resolution_completed",
            &workspace_id.0,
            &json!({"workspace": workspace_id}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_dependency_receipt(
        &self,
        workspace_id: &WorkspaceId,
        provider: &str,
        fingerprint: &str,
        layer_path: Option<&Path>,
        receipt: &Value,
    ) -> Result<(), DbError> {
        let connection = self.connection()?;
        let id = format!("dep_{}", ulid::Ulid::new());
        let now = now_ms();
        connection.execute(
            "INSERT INTO dependency_receipts \
             (id, workspace_id, provider, fingerprint, layer_path, state, receipt_json, \
              created_at_ms, validated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'ready', ?6, ?7, ?7) \
             ON CONFLICT(workspace_id, provider) DO UPDATE SET \
               fingerprint=excluded.fingerprint, layer_path=excluded.layer_path, \
               state='ready', receipt_json=excluded.receipt_json, validated_at_ms=excluded.validated_at_ms",
            params![
                id,
                workspace_id.0,
                provider,
                fingerprint,
                layer_path.map(|path| path.to_string_lossy()),
                serde_json::to_string(receipt)?,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn dependency_receipts(&self, workspace_id: &WorkspaceId) -> Result<Vec<Value>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT receipt_json FROM dependency_receipts WHERE workspace_id=?1 ORDER BY provider",
        )?;
        let rows = statement.query_map(params![workspace_id.0], |row| {
            let encoded: String = row.get(0)?;
            serde_json::from_str(&encoded).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    encoded.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn script_approvals(&self) -> Result<Vec<ScriptApproval>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT provider,package,version,integrity FROM script_approvals WHERE allowed=1 ORDER BY provider,package,version,integrity")?;
        statement
            .query_map([], |row| {
                Ok(ScriptApproval {
                    provider: row.get(0)?,
                    package: row.get(1)?,
                    version: row.get(2)?,
                    integrity: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn decide_script(
        &self,
        operation: &OperationId,
        actor: &str,
        approval: &ScriptApproval,
        allow: bool,
    ) -> Result<Outcome, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO script_approvals (provider,package,version,integrity,allowed,actor,updated_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7) \
             ON CONFLICT(provider,package,version,integrity) DO UPDATE SET allowed=excluded.allowed,actor=excluded.actor,updated_at_ms=excluded.updated_at_ms",
            params![approval.provider,approval.package,approval.version,approval.integrity,allow,actor,now_ms()],
        )?;
        crate::faults::hit(crate::faults::Point::ScriptDecisionWritten);
        append_event(
            &transaction,
            "dependency.script_decided",
            &operation.0,
            &json!({"approval":approval,"allowed":allow}),
        )?;
        let outcome = Outcome::Completed(json!({"allowed":allow,"refresh_required":true}));
        finish_operation_in_transaction(&transaction, operation, &outcome)?;
        transaction.commit()?;
        crate::faults::hit(crate::faults::Point::ScriptDecisionCompleted);
        Ok(outcome)
    }

    pub fn protected_dependency_fingerprints(&self) -> Result<Vec<String>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT d.fingerprint FROM dependency_receipts d \
             JOIN workspaces w ON w.id=d.workspace_id \
             WHERE w.state NOT IN ('deleted','deleting')",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Append one event for a decision that no state transition of its own
    /// records. Not part of the transaction that preceded it: a crash in
    /// between loses the note and keeps the release, which is the right way
    /// round for something whose only job is to be readable afterwards.
    pub fn record_event(
        &self,
        event: &str,
        resource: &str,
        payload: &Value,
    ) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        append_event(&transaction, event, resource, payload)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn events(&self, after_cursor: i64, limit: u32) -> Result<Vec<EventEnvelope>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT cursor, event, resource, payload_json, created_at_ms FROM events \
             WHERE cursor>?1 ORDER BY cursor ASC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after_cursor, limit.min(1000)], |row| {
            let payload: String = row.get(3)?;
            Ok(EventEnvelope {
                v: shade_protocol::PROTOCOL_VERSION,
                cursor: row.get(0)?,
                event: row.get(1)?,
                resource: row.get(2)?,
                payload: serde_json::from_str(&payload).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        payload.len(),
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                created_at_ms: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn gc_candidates(&self, grace_before_ms: i64) -> Result<Vec<WorkspaceRecord>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT w.id, w.repository_id, w.session_id, w.path, w.base_ref, w.base_oid, \
             w.head_oid, w.state, w.predecessor_id, w.dependency_state \
             FROM workspaces w \
             LEFT JOIN leases l ON l.workspace_id=w.id AND l.released_at_ms IS NULL \
             LEFT JOIN reviews r ON r.workspace_id=w.id AND r.state='pending' \
             WHERE w.state IN ('released','orphaned','failed') \
               AND w.updated_at_ms<?1 AND l.id IS NULL AND r.id IS NULL \
               AND NOT EXISTS (SELECT 1 FROM operations o \
                 WHERE o.state='running' AND o.resource_id=w.id) \
               AND NOT EXISTS (SELECT 1 FROM checkpoints c \
                 WHERE c.workspace_id=w.id AND c.state!='ready') \
               AND NOT EXISTS (SELECT 1 FROM publish_resolutions p \
                 WHERE p.state='pending' AND (p.workspace_id=w.id OR p.parent_workspace_id=w.id)) \
               AND NOT EXISTS (SELECT 1 FROM publish_operations p \
                 WHERE p.workspace_id=w.id AND p.state NOT IN ('completed','cancelled','failed')) \
               AND NOT EXISTS (SELECT 1 FROM handoffs h WHERE h.state='pending' \
                 AND (h.successor_workspace_id=w.id OR h.predecessor_workspace_id=w.id))",
        )?;
        let rows = statement.query_map(params![grace_before_ms], workspace_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Whether a workspace carries no unfinished work of its own.
    ///
    /// This is the `gc_candidates` predicate list minus the lease and state
    /// filters, so `sleep` refuses exactly what GC refuses: a pending review, a
    /// checkpoint that is not ready, an unfinished publish, a pending handoff
    /// or another running operation. `current_operation` is the sleep itself,
    /// which has already bound the workspace as its resource.
    pub fn workspace_is_quiescent(
        &self,
        id: &WorkspaceId,
        current_operation: &OperationId,
    ) -> Result<bool, DbError> {
        let connection = self.connection()?;
        let blocked: bool = connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM reviews r WHERE r.workspace_id=?1 \
                 AND r.state='pending') \
               OR EXISTS (SELECT 1 FROM operations o WHERE o.resource_id=?1 \
                 AND o.state='running' AND o.id!=?2) \
               OR EXISTS (SELECT 1 FROM checkpoints c WHERE c.workspace_id=?1 \
                 AND c.state!='ready') \
               OR EXISTS (SELECT 1 FROM publish_resolutions p WHERE p.state='pending' \
                 AND (p.workspace_id=?1 OR p.parent_workspace_id=?1)) \
               OR EXISTS (SELECT 1 FROM publish_operations p WHERE p.workspace_id=?1 \
                 AND p.state NOT IN ('completed','cancelled','failed')) \
               OR EXISTS (SELECT 1 FROM handoffs h WHERE h.state='pending' \
                 AND (h.successor_workspace_id=?1 OR h.predecessor_workspace_id=?1))",
            params![id.0, current_operation.0],
            |row| row.get(0),
        )?;
        Ok(!blocked)
    }

    /// Every workspace in `state` last touched before `updated_before_ms`.
    ///
    /// Drives the auto-sleep and suspension-retention sweeps, and the
    /// reconciliation pass over interrupted suspensions (which passes
    /// `i64::MAX` because it wants them all).
    pub fn workspaces_in_state_before(
        &self,
        state: &str,
        updated_before_ms: i64,
    ) -> Result<Vec<WorkspaceRecord>, DbError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, repository_id, session_id, path, base_ref, base_oid, head_oid, state, \
             predecessor_id, dependency_state FROM workspaces \
             WHERE state=?1 AND updated_at_ms<?2 ORDER BY updated_at_ms",
        )?;
        let rows = statement.query_map(params![state, updated_before_ms], workspace_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn claim_workspace_for_gc(
        &self,
        id: &WorkspaceId,
        grace_before_ms: i64,
        current_operation: &OperationId,
    ) -> Result<bool, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE workspaces SET state='deleting', updated_at_ms=?4 \
             WHERE id=?1 AND state IN ('released','orphaned','failed') AND updated_at_ms<?2 \
               AND NOT EXISTS (SELECT 1 FROM leases l WHERE l.workspace_id=?1 \
                 AND l.released_at_ms IS NULL) \
               AND NOT EXISTS (SELECT 1 FROM reviews r WHERE r.workspace_id=?1 \
                 AND r.state='pending') \
               AND NOT EXISTS (SELECT 1 FROM operations o WHERE o.resource_id=?1 \
                 AND o.state='running' AND o.id!=?3) \
               AND NOT EXISTS (SELECT 1 FROM checkpoints c WHERE c.workspace_id=?1 \
                 AND c.state!='ready') \
               AND NOT EXISTS (SELECT 1 FROM publish_resolutions p WHERE p.state='pending' \
                 AND (p.workspace_id=?1 OR p.parent_workspace_id=?1)) \
               AND NOT EXISTS (SELECT 1 FROM publish_operations p WHERE p.workspace_id=?1 \
                 AND p.state NOT IN ('completed','cancelled','failed')) \
               AND NOT EXISTS (SELECT 1 FROM handoffs h WHERE h.state='pending' \
                 AND (h.successor_workspace_id=?1 OR h.predecessor_workspace_id=?1))",
            params![id.0, grace_before_ms, current_operation.0, now_ms()],
        )?;
        if changed == 1 {
            append_event(
                &transaction,
                "workspace.deleting",
                &id.0,
                &json!({"workspace": id, "operation": current_operation}),
            )?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    /// Expire every lease whose deadline has passed and move its session and
    /// workspace to `dormant`.
    ///
    /// Dormant is not a collectible state: the tree, its checkpoints and its
    /// secrets all survive, and only an explicit `release` ever makes them
    /// eligible for GC. An expiry is a loss of exclusivity, never a loss of
    /// work.
    pub fn mark_expired_leases(&self, now: i64) -> Result<ExpirySweep, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mut statement = transaction.prepare(
            "SELECT h.id, h.successor_workspace_id FROM handoffs h \
             JOIN leases l ON l.session_id=h.session_id \
             WHERE h.state='pending' AND l.released_at_ms IS NULL AND l.expires_at_ms<?1",
        )?;
        let expired_handoffs = statement
            .query_map(params![now], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut statement = transaction.prepare(
            "SELECT id FROM sessions WHERE state='active' AND id IN \
             (SELECT session_id FROM leases WHERE released_at_ms IS NULL AND expires_at_ms<?1)",
        )?;
        let dormant_sessions = statement
            .query_map(params![now], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut statement = transaction.prepare(
            "SELECT id FROM workspaces WHERE state='ready' AND session_id IN \
             (SELECT session_id FROM leases WHERE released_at_ms IS NULL AND expires_at_ms<?1)",
        )?;
        let dormant_workspaces = statement
            .query_map(params![now], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let changed = transaction.execute(
            "UPDATE sessions SET state='dormant', updated_at_ms=?1 \
             WHERE state='active' AND id IN (SELECT session_id FROM leases \
               WHERE released_at_ms IS NULL AND expires_at_ms<?1)",
            params![now],
        )?;
        transaction.execute(
            "UPDATE workspaces SET state='dormant', updated_at_ms=?1 \
             WHERE state='ready' AND session_id IN (SELECT session_id FROM leases \
               WHERE released_at_ms IS NULL AND expires_at_ms<?1)",
            params![now],
        )?;
        transaction.execute(
            "UPDATE leases SET released_at_ms=?1 WHERE released_at_ms IS NULL AND expires_at_ms<?1",
            params![now],
        )?;
        for session in &dormant_sessions {
            append_event(
                &transaction,
                "session.dormant",
                session,
                &json!({"session": session}),
            )?;
        }
        for workspace in &dormant_workspaces {
            append_event(
                &transaction,
                "workspace.dormant",
                workspace,
                &json!({"workspace": workspace}),
            )?;
        }
        let handoffs_cancelled = expired_handoffs.len();
        for (handoff_id, successor_id) in expired_handoffs {
            transaction.execute(
                "UPDATE handoffs SET state='cancelled', cancelled_at_ms=?2 \
                 WHERE id=?1 AND state='pending'",
                params![handoff_id, now],
            )?;
            transaction.execute(
                "UPDATE workspaces SET state='failed', updated_at_ms=?2 \
                 WHERE id=?1 AND state='handoff_pending'",
                params![successor_id, now],
            )?;
            append_event(
                &transaction,
                "successor.cancelled",
                &successor_id,
                &json!({"handoff": handoff_id, "workspace": successor_id}),
            )?;
        }
        transaction.commit()?;
        Ok(ExpirySweep {
            sessions: changed,
            workspaces: dormant_workspaces.len(),
            handoffs_cancelled,
        })
    }

    /// Rewrite rows left by a binary that still wrote `orphaned`.
    ///
    /// The state columns carry no CHECK constraint, so renaming the value is a
    /// data change rather than DDL: `user_version` stays 1 and an older binary
    /// can still open the same database. It simply never collects a `dormant`
    /// row, which is the fail-safe direction.
    /// Also run by `Database::open`, so this is the idempotent second pass a
    /// `Reconcile` performs rather than the only chance the rows ever get.
    pub fn normalize_legacy_dormant_states(&self) -> Result<usize, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let changed = normalize_dormant_state_rows(&transaction)?;
        transaction.commit()?;
        Ok(changed)
    }

    pub fn recover_interrupted_operations(
        &self,
        current_operation: &OperationId,
    ) -> Result<usize, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let mut statement = transaction.prepare(
            "SELECT id, intent_kind FROM operations o WHERE state='running' AND id!=?1 \
             AND NOT EXISTS (SELECT 1 FROM publish_operations p WHERE p.operation_id=o.id \
               AND p.state NOT IN ('cancelled','failed'))",
        )?;
        let operation_ids = statement
            .query_map(params![current_operation.0], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let interrupted_error = ShadeError {
            code: "OPERATION_INTERRUPTED".into(),
            retry: "safe".into(),
            operation: None,
            next: Some("retry with a new idempotency key".into()),
            diagnostics_id: None,
        };
        let adopt_error = ShadeError {
            next: Some("retry with the same idempotency key".into()),
            ..interrupted_error.clone()
        };
        let interrupted_encoded = serde_json::to_string(&interrupted_error)?;
        let adopt_encoded = serde_json::to_string(&adopt_error)?;
        for (id, intent_kind) in &operation_ids {
            let (encoded, same_key) = if intent_kind == "successor_adopt" {
                (&adopt_encoded, true)
            } else {
                (&interrupted_encoded, false)
            };
            transaction.execute(
                "UPDATE operations SET state='failed', error_json=?2, updated_at_ms=?3 \
                 WHERE id=?1 AND state='running'",
                params![id, encoded, now_ms()],
            )?;
            append_event(
                &transaction,
                "operation.recovered",
                id,
                &json!({"operation": id, "state": "failed", "same_key_retry": same_key}),
            )?;
        }
        if !operation_ids.is_empty() {
            crate::faults::hit(crate::faults::Point::ReconcileOperationsWritten);
        }
        transaction.commit()?;
        if !operation_ids.is_empty() {
            crate::faults::hit(crate::faults::Point::ReconcileOperationsRecovered);
        }
        Ok(operation_ids.len())
    }

    /// Fence an unfinished workspace so startup recovery can finish deleting
    /// its filesystem and Git artifacts. Every protection predicate is
    /// evaluated in the same transaction as the state transition.
    pub fn claim_incomplete_workspace_for_recovery(
        &self,
        id: &WorkspaceId,
        current_operation: &OperationId,
    ) -> Result<bool, DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE workspaces SET state='recovering', updated_at_ms=?3 \
             WHERE id=?1 AND state IN ('materializing','deleting','recovering') \
               AND NOT EXISTS (SELECT 1 FROM sessions s WHERE s.workspace_id=?1 \
                 AND s.state IN ('active','dormant','suspended')) \
               AND NOT EXISTS (SELECT 1 FROM leases l WHERE l.workspace_id=?1 \
                 AND l.released_at_ms IS NULL) \
               AND NOT EXISTS (SELECT 1 FROM reviews r WHERE r.workspace_id=?1 \
                 AND r.state='pending') \
               AND NOT EXISTS (SELECT 1 FROM operations o WHERE o.resource_id=?1 \
                 AND o.state='running' AND o.id!=?2) \
               AND NOT EXISTS (SELECT 1 FROM publish_resolutions p WHERE p.state='pending' \
                 AND (p.workspace_id=?1 OR p.parent_workspace_id=?1)) \
               AND NOT EXISTS (SELECT 1 FROM publish_operations p WHERE p.workspace_id=?1 \
                 AND p.state NOT IN ('completed','cancelled','failed'))",
            params![id.0, current_operation.0, now_ms()],
        )?;
        if changed == 1 {
            append_event(
                &transaction,
                "workspace.recovering",
                &id.0,
                &json!({"workspace": id, "operation": current_operation}),
            )?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    pub fn delete_workspace_record(&self, id: &WorkspaceId) -> Result<(), DbError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM dependency_receipts WHERE workspace_id=?1",
            params![id.0],
        )?;
        transaction.execute(
            "DELETE FROM checkpoints WHERE workspace_id=?1",
            params![id.0],
        )?;
        transaction.execute("DELETE FROM reviews WHERE workspace_id=?1", params![id.0])?;
        transaction.execute(
            "DELETE FROM publish_resolutions WHERE workspace_id=?1 OR parent_workspace_id=?1",
            params![id.0],
        )?;
        // Pending handoffs are excluded by every GC/recovery claim. Once a
        // handoff is adopted or cancelled its row is only audit linkage and
        // must not keep either collected workspace alive through its FKs.
        transaction.execute(
            "DELETE FROM handoffs WHERE predecessor_workspace_id=?1 OR successor_workspace_id=?1",
            params![id.0],
        )?;
        transaction.execute(
            "DELETE FROM leases WHERE workspace_id=?1 OR session_id IN (\
               SELECT id FROM sessions WHERE workspace_id=?1 AND state!='active'\
             )",
            params![id.0],
        )?;
        // Clear every historical workspace association before deleting the
        // released session, regardless of which adopted workspace GC visits
        // first. Active sessions are excluded by the subquery predicate.
        transaction.execute(
            "UPDATE workspaces SET session_id=NULL WHERE id=?1 OR session_id IN (\
               SELECT id FROM sessions WHERE workspace_id=?1 AND state!='active'\
             )",
            params![id.0],
        )?;
        transaction.execute(
            "UPDATE workspaces SET predecessor_id=NULL WHERE predecessor_id=?1",
            params![id.0],
        )?;
        transaction.execute(
            "DELETE FROM sessions WHERE workspace_id=?1 AND state!='active'",
            params![id.0],
        )?;
        transaction.execute("DELETE FROM workspaces WHERE id=?1", params![id.0])?;
        append_event(
            &transaction,
            "workspace.deleted",
            &id.0,
            &json!({"workspace": id}),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn doctor(&self) -> Result<Value, DbError> {
        let connection = self.connection()?;
        let integrity: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        let active_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE state='active'",
            [],
            |row| row.get(0),
        )?;
        let live_workspaces: i64 = connection.query_row(
            "SELECT COUNT(*) FROM workspaces WHERE state NOT IN ('deleted','deleting')",
            [],
            |row| row.get(0),
        )?;
        let running_operations: i64 = connection.query_row(
            "SELECT COUNT(*) FROM operations WHERE state='running'",
            [],
            |row| row.get(0),
        )?;
        let dormant_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE state='dormant'",
            [],
            |row| row.get(0),
        )?;
        let suspended_sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE state='suspended'",
            [],
            |row| row.get(0),
        )?;
        let suspended_workspaces: i64 = connection.query_row(
            "SELECT COUNT(*) FROM workspaces WHERE state='suspended'",
            [],
            |row| row.get(0),
        )?;
        // A workspace stuck mid-sleep can be neither reattached nor woken until
        // something settles it. The reconciliation pass and every door into a
        // session do settle it, so a count that stays above zero means the
        // settlement itself keeps failing -- which is worth seeing.
        let suspending_workspaces: i64 = connection.query_row(
            "SELECT COUNT(*) FROM workspaces WHERE state='suspending'",
            [],
            |row| row.get(0),
        )?;
        // A suspended workspace with no sleep checkpoint has lost its content
        // pointer. Counting it makes a broken invariant visible instead of
        // silent.
        // A `failed` workspace is collectible, so a non-zero count is the one
        // number that says work reconciliation could not vouch for is queued
        // for deletion.
        let failed_workspaces: i64 = connection.query_row(
            "SELECT COUNT(*) FROM workspaces WHERE state='failed'",
            [],
            |row| row.get(0),
        )?;
        let suspended_without_checkpoint: i64 = connection.query_row(
            "SELECT COUNT(*) FROM workspaces w WHERE w.state='suspended' \
             AND NOT EXISTS (SELECT 1 FROM checkpoints c WHERE c.workspace_id=w.id \
               AND c.state='ready' AND c.reason='sleep')",
            [],
            |row| row.get(0),
        )?;
        Ok(json!({
            "state": integrity,
            "sessions": active_sessions,
            "workspaces": live_workspaces,
            "operations": running_operations,
            "sessions_dormant": dormant_sessions,
            "sessions_suspended": suspended_sessions,
            "workspaces_suspended": suspended_workspaces,
            "workspaces_suspending": suspending_workspaces,
            "workspaces_failed": failed_workspaces,
            "suspended_without_checkpoint": suspended_without_checkpoint,
        }))
    }
}

fn insert_diagnostic(connection: &Connection, diagnostic: &Diagnostic) -> Result<(), DbError> {
    connection.execute(
        "INSERT INTO diagnostics (id, record_json) VALUES (?1, ?2)",
        params![diagnostic.id, serde_json::to_string(diagnostic)?],
    )?;
    Ok(())
}

fn read_diagnostic(connection: &Connection, id: &str) -> Result<Option<Diagnostic>, DbError> {
    let record: Option<String> = connection
        .query_row(
            "SELECT record_json FROM diagnostics WHERE id=?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    record
        .map(|record| serde_json::from_str(&record))
        .transpose()
        .map_err(Into::into)
}

/// Rewrite rows left by a binary that still wrote `orphaned`.
///
/// Runs inside the caller's transaction so `Database::open` can do it before
/// the first read, and so a `Reconcile` can repeat it harmlessly.
fn normalize_dormant_state_rows(transaction: &Transaction<'_>) -> Result<usize, DbError> {
    let now = now_ms();
    let sessions = transaction.execute(
        "UPDATE sessions SET state='dormant', updated_at_ms=?1 WHERE state='orphaned'",
        params![now],
    )?;
    let workspaces = transaction.execute(
        "UPDATE workspaces SET state='dormant', updated_at_ms=?1 WHERE state='orphaned'",
        params![now],
    )?;
    Ok(sessions + workspaces)
}

fn create_schema(connection: &Transaction<'_>) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        r#"
        CREATE TABLE repositories (
          id TEXT PRIMARY KEY,
          identity TEXT NOT NULL UNIQUE,
          bare_path TEXT NOT NULL UNIQUE,
          default_ref TEXT,
          state TEXT NOT NULL,
          created_at_ms INTEGER NOT NULL,
          updated_at_ms INTEGER NOT NULL
        ) STRICT;
        CREATE TABLE sessions (
          id TEXT PRIMARY KEY,
          repository_id TEXT NOT NULL REFERENCES repositories(id),
          workspace_id TEXT NOT NULL,
          intent TEXT,
          state TEXT NOT NULL,
          created_at_ms INTEGER NOT NULL,
          updated_at_ms INTEGER NOT NULL
        ) STRICT;
        CREATE TABLE workspaces (
          id TEXT PRIMARY KEY,
          repository_id TEXT NOT NULL REFERENCES repositories(id),
          session_id TEXT REFERENCES sessions(id),
          path TEXT NOT NULL UNIQUE,
          base_ref TEXT NOT NULL,
          base_oid TEXT NOT NULL,
          head_oid TEXT NOT NULL,
          state TEXT NOT NULL,
          predecessor_id TEXT REFERENCES workspaces(id),
          dependency_state TEXT NOT NULL,
          created_at_ms INTEGER NOT NULL,
          updated_at_ms INTEGER NOT NULL
        ) STRICT;
        CREATE TABLE leases (
          id TEXT PRIMARY KEY,
          session_id TEXT NOT NULL REFERENCES sessions(id),
          workspace_id TEXT NOT NULL REFERENCES workspaces(id),
          fence INTEGER NOT NULL,
          heartbeat_at_ms INTEGER NOT NULL,
          expires_at_ms INTEGER NOT NULL,
          released_at_ms INTEGER,
          UNIQUE(session_id, fence)
        ) STRICT;
        CREATE UNIQUE INDEX live_lease_per_session ON leases(session_id)
          WHERE released_at_ms IS NULL;
        CREATE TABLE operations (
          id TEXT PRIMARY KEY,
          actor_id TEXT NOT NULL,
          idempotency_key TEXT NOT NULL,
          request_hash TEXT NOT NULL,
          intent_kind TEXT NOT NULL,
          state TEXT NOT NULL,
          resource_id TEXT,
          phase TEXT NOT NULL DEFAULT 'created',
          outcome_json TEXT,
          error_json TEXT,
          created_at_ms INTEGER NOT NULL,
          updated_at_ms INTEGER NOT NULL,
          UNIQUE(actor_id, idempotency_key)
        ) STRICT;
        CREATE TABLE diagnostics (
          id TEXT PRIMARY KEY,
          record_json TEXT NOT NULL
        ) STRICT;
        CREATE TABLE handoffs (
          id TEXT PRIMARY KEY,
          operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
          actor_id TEXT NOT NULL,
          session_id TEXT NOT NULL REFERENCES sessions(id),
          predecessor_workspace_id TEXT NOT NULL REFERENCES workspaces(id),
          successor_workspace_id TEXT NOT NULL UNIQUE REFERENCES workspaces(id),
          state TEXT NOT NULL CHECK(state IN ('pending', 'adopted', 'cancelled')),
          created_at_ms INTEGER NOT NULL,
          adopted_at_ms INTEGER,
          cancelled_at_ms INTEGER
        ) STRICT;
        CREATE UNIQUE INDEX one_pending_handoff_per_session ON handoffs(session_id)
          WHERE state='pending';
        CREATE UNIQUE INDEX one_pending_handoff_per_predecessor
          ON handoffs(predecessor_workspace_id) WHERE state='pending';
        CREATE TABLE checkpoints (
          id TEXT PRIMARY KEY,
          workspace_id TEXT NOT NULL REFERENCES workspaces(id),
          head_oid TEXT NOT NULL,
          index_oid TEXT NOT NULL,
          worktree_oid TEXT NOT NULL,
          reason TEXT NOT NULL,
          state TEXT NOT NULL,
          created_at_ms INTEGER NOT NULL
        ) STRICT;
        CREATE TABLE reviews (
          id TEXT PRIMARY KEY,
          workspace_id TEXT NOT NULL REFERENCES workspaces(id),
          kind TEXT NOT NULL,
          payload_json TEXT NOT NULL,
          state TEXT NOT NULL,
          created_at_ms INTEGER NOT NULL,
          resolved_at_ms INTEGER
        ) STRICT;
        CREATE TABLE publish_resolutions (
          workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id),
          parent_workspace_id TEXT NOT NULL REFERENCES workspaces(id),
          branch TEXT NOT NULL,
          message TEXT NOT NULL,
          push INTEGER NOT NULL CHECK(push IN (0, 1)),
          expected_remote_oid TEXT NOT NULL,
          expected_local_oid TEXT,
          state TEXT NOT NULL,
          created_at_ms INTEGER NOT NULL,
          resolved_at_ms INTEGER
        ) STRICT;
        CREATE TABLE publish_operations (
          operation_id TEXT PRIMARY KEY REFERENCES operations(id),
          workspace_id TEXT NOT NULL,
          repository_id TEXT NOT NULL,
          checkpoint_id TEXT NOT NULL,
          source_kind TEXT NOT NULL CHECK(source_kind IN ('workspace_publish','resolution_complete')),
          branch TEXT NOT NULL,
          message TEXT NOT NULL,
          push INTEGER NOT NULL CHECK(push IN (0, 1)),
          original_base_oid TEXT NOT NULL,
          expected_remote_oid TEXT,
          expected_local_oid TEXT,
          anchor_ref TEXT NOT NULL UNIQUE,
          commit_oid TEXT,
          tree_oid TEXT,
          state TEXT NOT NULL CHECK(state IN (
            'planned','prepared','local_applied','remote_applied','completed','cancelled','failed'
          )),
          anchor_cleaned INTEGER NOT NULL DEFAULT 0 CHECK(anchor_cleaned IN (0, 1)),
          created_at_ms INTEGER NOT NULL,
          updated_at_ms INTEGER NOT NULL
        ) STRICT;
        CREATE TABLE dependency_receipts (
          id TEXT PRIMARY KEY,
          workspace_id TEXT NOT NULL REFERENCES workspaces(id),
          provider TEXT NOT NULL,
          fingerprint TEXT NOT NULL,
          layer_path TEXT,
          state TEXT NOT NULL,
          receipt_json TEXT NOT NULL,
          created_at_ms INTEGER NOT NULL,
          validated_at_ms INTEGER NOT NULL,
          UNIQUE(workspace_id, provider)
        ) STRICT;
        CREATE TABLE script_approvals (
          provider TEXT NOT NULL,
          package TEXT NOT NULL,
          version TEXT NOT NULL,
          integrity TEXT NOT NULL,
          allowed INTEGER NOT NULL CHECK(allowed IN (0,1)),
          actor TEXT NOT NULL,
          updated_at_ms INTEGER NOT NULL,
          PRIMARY KEY(provider,package,version,integrity)
        ) STRICT;
        CREATE TABLE events (
          cursor INTEGER PRIMARY KEY AUTOINCREMENT,
          event TEXT NOT NULL,
          resource TEXT NOT NULL,
          payload_json TEXT NOT NULL,
          created_at_ms INTEGER NOT NULL
        ) STRICT;
        "#,
    )
}

fn review_workspace(
    transaction: &Transaction<'_>,
    review: &ReviewId,
) -> Result<WorkspaceId, DbError> {
    let workspace = transaction.query_row(
        "SELECT workspace_id FROM reviews WHERE id=?1 AND state='pending' AND kind='secret_cleanup'",
        params![review.0], |row| row.get::<_, String>(0),
    )?;
    Ok(WorkspaceId(workspace))
}

fn close_reviewed_workspace(
    transaction: &Transaction<'_>,
    workspace: &WorkspaceId,
    state: &str,
) -> Result<(), DbError> {
    if transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM handoffs WHERE state='pending' AND (predecessor_workspace_id=?1 OR successor_workspace_id=?1))",
        params![workspace.0], |row| row.get::<_, bool>(0),
    )? {
        return Err(DbError::LeaseFenced);
    }
    let now = now_ms();
    transaction.execute(
        "UPDATE leases SET released_at_ms=?2 WHERE workspace_id=?1 AND released_at_ms IS NULL",
        params![workspace.0, now],
    )?;
    let mut statement = transaction.prepare(
        "SELECT id FROM sessions WHERE workspace_id=?1 \
         AND state IN ('active','dormant','suspended','orphaned')",
    )?;
    let sessions = statement
        .query_map(params![workspace.0], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    transaction.execute(
        "UPDATE sessions SET state='released', updated_at_ms=?2 WHERE workspace_id=?1 \
         AND state IN ('active','dormant','suspended','orphaned')",
        params![workspace.0, now],
    )?;
    transaction.execute(
        "UPDATE workspaces SET state=?2, updated_at_ms=?3 WHERE id=?1",
        params![workspace.0, state, now],
    )?;
    for session in sessions {
        append_event(
            transaction,
            "session.released",
            &session,
            &json!({"session":session}),
        )?;
    }
    Ok(())
}

fn finish_review_in_transaction(
    transaction: &Transaction<'_>,
    review: &ReviewId,
    state: &str,
) -> Result<(), DbError> {
    let changed = transaction.execute(
        "UPDATE reviews SET state=?2, resolved_at_ms=?3 WHERE id=?1 AND state='pending'",
        params![review.0, state, now_ms()],
    )?;
    if changed != 1 {
        return Err(DbError::LeaseFenced);
    }
    append_event(
        transaction,
        "review.resolved",
        &review.0,
        &json!({"review":review,"resolution":state}),
    )
}

fn append_event(
    transaction: &Transaction<'_>,
    event: &str,
    resource: &str,
    payload: &Value,
) -> Result<(), DbError> {
    transaction.execute(
        "INSERT INTO events (event, resource, payload_json, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
        params![event, resource, serde_json::to_string(payload)?, now_ms()],
    )?;
    Ok(())
}

fn finish_operation_in_transaction(
    transaction: &Transaction<'_>,
    operation_id: &OperationId,
    outcome: &Outcome,
) -> Result<(), DbError> {
    let now = now_ms();
    let changed = transaction.execute(
        "UPDATE operations SET state='completed', outcome_json=?2, error_json=NULL, \
         updated_at_ms=?3 WHERE id=?1 AND state='running'",
        params![operation_id.0, serde_json::to_string(outcome)?, now],
    )?;
    if changed == 1 {
        append_event(
            transaction,
            "operation.completed",
            &operation_id.0,
            &json!({"operation": operation_id, "outcome": outcome}),
        )?;
    }
    Ok(())
}

fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        id: WorkspaceId(row.get(0)?),
        repository_id: RepositoryId(row.get(1)?),
        session_id: row.get::<_, Option<String>>(2)?.map(SessionId),
        path: PathBuf::from(row.get::<_, String>(3)?),
        base_ref: row.get(4)?,
        base_oid: ObjectId(row.get(5)?),
        head_oid: ObjectId(row.get(6)?),
        state: row.get(7)?,
        predecessor_id: row.get::<_, Option<String>>(8)?.map(WorkspaceId),
        dependency_state: row.get(9)?,
    })
}

fn handoff_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HandoffRecord> {
    Ok(HandoffRecord {
        id: HandoffId(row.get(0)?),
        operation_id: OperationId(row.get(1)?),
        actor_id: row.get(2)?,
        session_id: SessionId(row.get(3)?),
        predecessor_workspace_id: WorkspaceId(row.get(4)?),
        successor_workspace_id: WorkspaceId(row.get(5)?),
        state: row.get(6)?,
    })
}

fn lease_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LeaseRecord> {
    Ok(LeaseRecord {
        id: LeaseId(row.get(0)?),
        session_id: SessionId(row.get(1)?),
        workspace_id: WorkspaceId(row.get(2)?),
        fence: row.get(3)?,
        heartbeat_at_ms: row.get(4)?,
        expires_at_ms: row.get(5)?,
        released_at_ms: row.get(6)?,
    })
}

fn operation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(OperationRecord, String)> {
    let outcome_json: Option<String> = row.get(4)?;
    let error_json: Option<String> = row.get(5)?;
    let outcome = decode_optional_json(outcome_json)?;
    let error = decode_optional_json(error_json)?;
    Ok((
        OperationRecord {
            id: OperationId(row.get(0)?),
            state: row.get(2)?,
            intent_kind: row.get(3)?,
            outcome,
            error,
            created_at_ms: row.get(6)?,
            updated_at_ms: row.get(7)?,
        },
        row.get(1)?,
    ))
}

fn publish_intent_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PublishIntentRecord> {
    Ok(PublishIntentRecord {
        operation_id: OperationId(row.get(0)?),
        workspace_id: WorkspaceId(row.get(1)?),
        repository_id: RepositoryId(row.get(2)?),
        checkpoint_id: CheckpointId(row.get(3)?),
        source_kind: row.get(4)?,
        branch: row.get(5)?,
        message: row.get(6)?,
        push: row.get::<_, i64>(7)? != 0,
        original_base_oid: ObjectId(row.get(8)?),
        expected_remote_oid: row.get::<_, Option<String>>(9)?.map(ObjectId),
        expected_local_oid: row.get::<_, Option<String>>(10)?.map(ObjectId),
        anchor_ref: row.get(11)?,
        commit_oid: row.get::<_, Option<String>>(12)?.map(ObjectId),
        tree_oid: row.get::<_, Option<String>>(13)?.map(ObjectId),
        state: row.get(14)?,
        anchor_cleaned: row.get::<_, i64>(15)? != 0,
    })
}

fn decode_optional_json<T: for<'de> Deserialize<'de>>(
    value: Option<String>,
) -> rusqlite::Result<Option<T>> {
    value
        .map(|encoded| {
            serde_json::from_str(&encoded).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    encoded.len(),
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shade_protocol::{Actor, ActorKind};

    #[test]
    fn failed_operation_diagnostic_and_event_roll_back_together() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("state.sqlite")).unwrap();
        let BeginOperation::New(operation) = database
            .begin_operation("agent", "fail", "hash", "session_open")
            .unwrap()
        else {
            panic!("expected new operation")
        };
        let mut diagnostic = crate::diagnostics::new(
            shade_protocol::DiagnosticOrigin::Daemon,
            "INTERNAL",
            "fixture failed",
        );
        diagnostic.operation = Some(operation.clone());
        let error = ShadeError {
            code: "INTERNAL".into(),
            retry: "safe".into(),
            operation: Some(operation.clone()),
            next: None,
            diagnostics_id: Some(diagnostic.id.clone()),
        };
        database.connection().unwrap().execute_batch("CREATE TRIGGER reject_failure_event BEFORE INSERT ON events WHEN NEW.event='operation.failed' BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
        assert!(
            database
                .fail_operation(&operation, &error, Some(&diagnostic))
                .is_err()
        );
        assert_eq!(
            database.operation(&operation).unwrap().unwrap().state,
            "running"
        );
        assert!(database.diagnostic(&diagnostic.id).unwrap().is_none());
        assert_eq!(database.events(0, 100).unwrap().len(), 1);
        database
            .connection()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_failure_event")
            .unwrap();
        database
            .fail_operation(&operation, &error, Some(&diagnostic))
            .unwrap();
        assert_eq!(
            database.operation(&operation).unwrap().unwrap().state,
            "failed"
        );
        assert_eq!(
            database.diagnostic(&diagnostic.id).unwrap().unwrap(),
            diagnostic
        );
        assert_eq!(database.events(0, 100).unwrap().len(), 2);
    }

    #[test]
    fn script_decisions_commit_with_the_operation_and_outbox() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite");
        let database = Database::open(&path).unwrap();
        let approval = ScriptApproval {
            provider: "npm".into(),
            package: "addon".into(),
            version: "1.0.0".into(),
            integrity: "sha512-exact".into(),
        };
        let BeginOperation::New(operation) = database
            .begin_operation("agent", "approve", "hash", "dependency_script_decision")
            .unwrap()
        else {
            panic!("new operation required")
        };
        database.connection().unwrap().execute_batch("CREATE TRIGGER reject_script_completion BEFORE UPDATE OF state ON operations BEGIN SELECT RAISE(ABORT, 'fixture'); END;").unwrap();
        assert!(
            database
                .decide_script(&operation, "agent", &approval, true)
                .is_err()
        );
        assert!(database.script_approvals().unwrap().is_empty());
        assert_eq!(
            database.operation(&operation).unwrap().unwrap().state,
            "running"
        );
        assert!(
            !database
                .events(0, 100)
                .unwrap()
                .iter()
                .any(|event| event.event == "dependency.script_decided")
        );
        database
            .connection()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_script_completion;")
            .unwrap();
        database
            .decide_script(&operation, "agent", &approval, true)
            .unwrap();
        assert!(
            matches!(database.begin_operation("agent", "approve", "hash", "dependency_script_decision").unwrap(), BeginOperation::Existing(record) if record.state == "completed")
        );
        drop(database);
        let database = Database::open(path).unwrap();
        assert_eq!(database.script_approvals().unwrap(), vec![approval.clone()]);
        let BeginOperation::New(operation) = database
            .begin_operation(
                "agent",
                "revoke",
                "revoke-hash",
                "dependency_script_decision",
            )
            .unwrap()
        else {
            panic!("new operation required")
        };
        database
            .decide_script(&operation, "agent", &approval, false)
            .unwrap();
        assert!(database.script_approvals().unwrap().is_empty());
        assert_eq!(
            database
                .events(0, 100)
                .unwrap()
                .iter()
                .filter(|event| event.event == "dependency.script_decided")
                .count(),
            2
        );
        assert_eq!(
            database.operation(&operation).unwrap().unwrap().state,
            "completed"
        );
    }

    #[test]
    fn operation_is_idempotent_and_outbox_is_transactional() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("state.sqlite")).unwrap();
        let actor = Actor {
            kind: ActorKind::Agent,
            id: "agent-1".into(),
        };
        let first = database
            .begin_operation(&actor.id, "same", "hash", "session_open")
            .unwrap();
        let operation = match first {
            BeginOperation::New(operation) => operation,
            _ => panic!("expected new operation"),
        };
        database
            .finish_operation(&operation, &Outcome::Completed(json!({"ok": true})))
            .unwrap();
        let second = database
            .begin_operation(&actor.id, "same", "hash", "session_open")
            .unwrap();
        assert!(matches!(second, BeginOperation::Existing(record) if record.state == "completed"));
        assert_eq!(database.events(0, 100).unwrap().len(), 2);
        assert!(matches!(
            database.begin_operation(&actor.id, "same", "other", "session_open"),
            Err(DbError::IdempotencyConflict)
        ));
    }

    /// One repository, one workspace and one active session on a database at
    /// `path`, ready for a second connection to race against.
    fn session_fixture(path: &Path, workspace_path: &Path) -> (Database, SessionRecord) {
        let database = Database::open(path).unwrap();
        let repository = database
            .upsert_repository("local:race", &workspace_path.join("bare"), Some("main"))
            .unwrap();
        let workspace = WorkspaceRecord {
            id: WorkspaceId("ws_race".into()),
            repository_id: repository.id.clone(),
            session_id: None,
            path: workspace_path.join("workspace"),
            base_ref: "origin/main".into(),
            base_oid: ObjectId("opaque-object-id".into()),
            head_oid: ObjectId("opaque-object-id".into()),
            state: "ready".into(),
            predecessor_id: None,
            dependency_state: "ready".into(),
        };
        database.create_workspace(&workspace).unwrap();
        let session = SessionRecord {
            id: SessionId("session-race".into()),
            repository_id: repository.id,
            workspace_id: workspace.id,
            intent: None,
            state: "active".into(),
        };
        database.create_session_and_lease(&session, 120).unwrap();
        (database, session)
    }

    fn unreleased_leases(path: &Path, session: &SessionId) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM leases WHERE session_id=?1 AND released_at_ms IS NULL",
                params![session.0],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// `live_lease_per_session` is a partial unique index, so two racing
    /// reattaches cannot both insert. Whichever loses must observe the winner's
    /// lease rather than a violated index or a second live lease.
    #[test]
    fn two_concurrent_reattaches_leave_exactly_one_live_lease() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite");
        let (database, session) = session_fixture(&path, directory.path());
        database.mark_expired_leases(now_ms() + 200_000).unwrap();
        assert_eq!(unreleased_leases(&path, &session.id), 0);

        let outcomes = std::thread::scope(|scope| {
            let handles = ["lease-a", "lease-b"].map(|lease| {
                let path = path.clone();
                let session = session.clone();
                scope.spawn(move || {
                    Database::open(&path).unwrap().reattach_session(
                        &session.id,
                        &session.workspace_id,
                        &LeaseId(lease.into()),
                        120,
                    )
                })
            });
            handles.map(|handle| handle.join().unwrap())
        });

        assert!(
            outcomes.iter().any(Result::is_ok),
            "at least one reattach must win"
        );
        assert_eq!(unreleased_leases(&path, &session.id), 1);
        assert_eq!(
            database.session(&session.id).unwrap().unwrap().state,
            "active"
        );
        let live = database
            .active_lease_for_session(&session.id)
            .unwrap()
            .unwrap();
        for outcome in outcomes.iter().flatten() {
            assert_eq!(
                outcome.id, live.id,
                "a successful reattach returns the one live lease"
            );
        }
    }

    /// A reattach and a release naming the same session are both legitimate.
    /// Whichever commits second must see the first: a released session never
    /// keeps a live lease, and a reattached one is never half-released.
    #[test]
    fn a_reattach_racing_a_release_never_leaves_a_live_lease_on_a_released_session() {
        for lease in ["lease-race-1", "lease-race-2"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("state.sqlite");
            let (database, session) = session_fixture(&path, directory.path());
            database.mark_expired_leases(now_ms() + 200_000).unwrap();

            let (reattached, released) = std::thread::scope(|scope| {
                let reattach = {
                    let path = path.clone();
                    let session = session.clone();
                    scope.spawn(move || {
                        Database::open(&path).unwrap().reattach_session(
                            &session.id,
                            &session.workspace_id,
                            &LeaseId(lease.into()),
                            120,
                        )
                    })
                };
                let release = {
                    let path = path.clone();
                    let session = session.clone();
                    scope.spawn(move || {
                        Database::open(&path)
                            .unwrap()
                            .release_session(&session.id, &session.workspace_id)
                    })
                };
                (reattach.join().unwrap(), release.join().unwrap())
            });

            let state = database.session(&session.id).unwrap().unwrap().state;
            let live = unreleased_leases(&path, &session.id);
            if released.is_ok() {
                assert_eq!(state, "released");
                assert_eq!(live, 0, "a released session surrenders every lease");
            } else {
                assert!(reattached.is_ok(), "one of the two has to win");
                assert_eq!(state, "active");
                assert_eq!(live, 1);
            }
        }
    }

    #[test]
    fn lease_expiry_leaves_a_dormant_workspace_that_is_never_collectable() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("state.sqlite")).unwrap();
        let repository = database
            .upsert_repository("local:test", &directory.path().join("bare"), Some("main"))
            .unwrap();
        let workspace = WorkspaceRecord {
            id: WorkspaceId("ws_1".into()),
            repository_id: repository.id.clone(),
            session_id: None,
            path: directory.path().join("workspace"),
            base_ref: "origin/main".into(),
            base_oid: ObjectId("opaque-object-id".into()),
            head_oid: ObjectId("opaque-object-id".into()),
            state: "ready".into(),
            predecessor_id: None,
            dependency_state: "ready".into(),
        };
        database.create_workspace(&workspace).unwrap();
        let session = SessionRecord {
            id: SessionId("session-1".into()),
            repository_id: repository.id,
            workspace_id: workspace.id,
            intent: None,
            state: "active".into(),
        };
        database.create_session_and_lease(&session, 1).unwrap();
        let sweep = database.mark_expired_leases(now_ms() + 2_000).unwrap();
        assert_eq!(sweep.sessions, 1);
        assert_eq!(sweep.workspaces, 1);
        assert_eq!(
            database.session(&session.id).unwrap().unwrap().state,
            "dormant"
        );
        assert_eq!(
            database
                .workspace(&session.workspace_id)
                .unwrap()
                .unwrap()
                .state,
            "dormant"
        );
        // An expiry is a loss of exclusivity, not a loss of work: no elapsed
        // grace makes a dormant workspace collectable.
        assert!(database.gc_candidates(now_ms() + 3_000).unwrap().is_empty());
        database
            .release_session(&session.id, &session.workspace_id)
            .unwrap();
        assert_eq!(database.gc_candidates(now_ms() + 3_000).unwrap().len(), 1);
    }

    #[test]
    fn legacy_orphaned_rows_are_normalized_to_dormant_idempotently() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("state.sqlite")).unwrap();
        let repository = database
            .upsert_repository(
                "local:retired-states",
                &directory.path().join("bare"),
                Some("main"),
            )
            .unwrap();
        let workspace = WorkspaceRecord {
            id: WorkspaceId("ws_legacy".into()),
            repository_id: repository.id.clone(),
            session_id: None,
            path: directory.path().join("workspace"),
            base_ref: "origin/main".into(),
            base_oid: ObjectId("opaque-object-id".into()),
            head_oid: ObjectId("opaque-object-id".into()),
            state: "ready".into(),
            predecessor_id: None,
            dependency_state: "ready".into(),
        };
        database.create_workspace(&workspace).unwrap();
        let session = SessionRecord {
            id: SessionId("session-retired".into()),
            repository_id: repository.id,
            workspace_id: workspace.id.clone(),
            intent: None,
            state: "active".into(),
        };
        database.create_session_and_lease(&session, 1).unwrap();
        {
            let connection = database.connection().unwrap();
            connection
                .execute("UPDATE sessions SET state='orphaned'", [])
                .unwrap();
            connection
                .execute("UPDATE workspaces SET state='orphaned'", [])
                .unwrap();
        }
        assert_eq!(database.normalize_legacy_dormant_states().unwrap(), 2);
        assert_eq!(
            database.session(&session.id).unwrap().unwrap().state,
            "dormant"
        );
        assert_eq!(
            database.workspace(&workspace.id).unwrap().unwrap().state,
            "dormant"
        );
        assert_eq!(database.normalize_legacy_dormant_states().unwrap(), 0);
    }

    #[test]
    fn successor_handoff_preserves_parent_until_adoption_and_is_cas_fenced() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("state.sqlite")).unwrap();
        let repository = database
            .upsert_repository("local:fence", &directory.path().join("bare"), Some("main"))
            .unwrap();
        let record = |id: &str, predecessor: Option<&str>| WorkspaceRecord {
            id: WorkspaceId(id.into()),
            repository_id: repository.id.clone(),
            session_id: None,
            path: directory.path().join(id),
            base_ref: "origin/main".into(),
            base_oid: ObjectId("opaque-base".into()),
            head_oid: ObjectId("opaque-base".into()),
            state: "materializing".into(),
            predecessor_id: predecessor.map(|value| WorkspaceId(value.into())),
            dependency_state: "ready".into(),
        };
        let original = record("ws_original", None);
        let winner = record("ws_winner", Some("ws_original"));
        let stale = record("ws_stale", Some("ws_original"));
        database.create_workspace(&original).unwrap();
        database.create_workspace(&winner).unwrap();
        database.create_workspace(&stale).unwrap();
        let session = SessionRecord {
            id: SessionId("session-fence".into()),
            repository_id: repository.id,
            workspace_id: original.id.clone(),
            intent: None,
            state: "active".into(),
        };
        database.create_session_and_lease(&session, 120).unwrap();
        let prepare_operation = match database
            .begin_operation("host", "prepare", "prepare-hash", "workspace_sync")
            .unwrap()
        {
            BeginOperation::New(id) => id,
            BeginOperation::Existing(_) => unreachable!(),
        };
        let handoff = database
            .prepare_successor_handoff(
                &prepare_operation,
                "host",
                &session.id,
                &original.id,
                &winner.id,
            )
            .unwrap();
        assert_eq!(
            database.session(&session.id).unwrap().unwrap().workspace_id,
            original.id
        );
        assert_eq!(
            database
                .active_lease_for_session(&session.id)
                .unwrap()
                .unwrap()
                .workspace_id,
            original.id
        );
        assert!(matches!(
            database.release_session(&session.id, &original.id),
            Err(DbError::LeaseFenced)
        ));
        let adopt_operation = match database
            .begin_operation("host", "adopt", "adopt-hash", "successor_adopt")
            .unwrap()
        {
            BeginOperation::New(id) => id,
            BeginOperation::Existing(_) => unreachable!(),
        };
        let lease = database
            .adopt_successor_handoff(
                &handoff.handoff_id,
                "host",
                &adopt_operation,
                &LeaseId("lease-successor".into()),
                120,
                &Outcome::Completed(json!({"workspace": winner.id})),
            )
            .unwrap();
        assert_eq!(lease.workspace_id, winner.id);
        let stale_operation = match database
            .begin_operation("host", "stale", "stale-hash", "workspace_sync")
            .unwrap()
        {
            BeginOperation::New(id) => id,
            BeginOperation::Existing(_) => unreachable!(),
        };
        assert!(matches!(
            database.prepare_successor_handoff(
                &stale_operation,
                "host",
                &session.id,
                &original.id,
                &stale.id,
            ),
            Err(DbError::LeaseFenced)
        ));
        assert!(matches!(
            database.release_session(&session.id, &original.id),
            Err(DbError::LeaseFenced)
        ));
        assert_eq!(
            database.session(&session.id).unwrap().unwrap().workspace_id,
            winner.id
        );
        assert_eq!(
            database
                .active_lease_for_session(&session.id)
                .unwrap()
                .unwrap()
                .workspace_id,
            winner.id
        );
        database.release_session(&session.id, &winner.id).unwrap();
    }

    #[test]
    fn adopted_handoff_records_can_be_collected_in_either_workspace_order() {
        for successor_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let database_path = directory.path().join("state.sqlite");
            let database = Database::open(&database_path).unwrap();
            let repository = database
                .upsert_repository(
                    if successor_first {
                        "local:delete-successor-first"
                    } else {
                        "local:delete-predecessor-first"
                    },
                    &directory.path().join("bare"),
                    Some("main"),
                )
                .unwrap();
            let workspace = |id: &str, predecessor: Option<&str>| WorkspaceRecord {
                id: WorkspaceId(id.into()),
                repository_id: repository.id.clone(),
                session_id: None,
                path: directory.path().join(id),
                base_ref: "origin/main".into(),
                base_oid: ObjectId("opaque-base".into()),
                head_oid: ObjectId("opaque-base".into()),
                state: "materializing".into(),
                predecessor_id: predecessor.map(|value| WorkspaceId(value.into())),
                dependency_state: "ready".into(),
            };
            let predecessor = workspace("ws_delete_predecessor", None);
            let successor = workspace("ws_delete_successor", Some("ws_delete_predecessor"));
            database.create_workspace(&predecessor).unwrap();
            database.create_workspace(&successor).unwrap();
            let session = SessionRecord {
                id: SessionId("session-delete-adopted".into()),
                repository_id: repository.id,
                workspace_id: predecessor.id.clone(),
                intent: None,
                state: "active".into(),
            };
            database.create_session_and_lease(&session, 120).unwrap();
            let prepare = match database
                .begin_operation("host", "prepare-delete", "prepare-hash", "workspace_sync")
                .unwrap()
            {
                BeginOperation::New(id) => id,
                BeginOperation::Existing(_) => unreachable!(),
            };
            let handoff = database
                .prepare_successor_handoff(
                    &prepare,
                    "host",
                    &session.id,
                    &predecessor.id,
                    &successor.id,
                )
                .unwrap();
            let adopt = match database
                .begin_operation("host", "adopt-delete", "adopt-hash", "successor_adopt")
                .unwrap()
            {
                BeginOperation::New(id) => id,
                BeginOperation::Existing(_) => unreachable!(),
            };
            database
                .adopt_successor_handoff(
                    &handoff.handoff_id,
                    "host",
                    &adopt,
                    &LeaseId("lease-delete-successor".into()),
                    120,
                    &Outcome::Completed(json!({"workspace": successor.id})),
                )
                .unwrap();
            assert_eq!(
                database
                    .workspace(&predecessor.id)
                    .unwrap()
                    .unwrap()
                    .session_id,
                None
            );
            database
                .release_session(&session.id, &successor.id)
                .unwrap();
            database
                .mark_workspace_state(&successor.id, "released")
                .unwrap();

            let order = if successor_first {
                [&successor.id, &predecessor.id]
            } else {
                [&predecessor.id, &successor.id]
            };
            database
                .delete_workspace_record(order[0])
                .unwrap_or_else(|error| {
                    panic!("first adopted-workspace delete failed (successor_first={successor_first}): {error:?}")
                });
            assert!(database.handoff(&handoff.handoff_id).unwrap().is_none());
            database.delete_workspace_record(order[1]).unwrap();
            assert!(database.workspace(&predecessor.id).unwrap().is_none());
            assert!(database.workspace(&successor.id).unwrap().is_none());
            assert!(database.session(&session.id).unwrap().is_none());

            drop(database);
            let verification = Connection::open(database_path).unwrap();
            verification
                .pragma_update(None, "foreign_keys", "ON")
                .unwrap();
            let violation_count: i64 = verification
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(violation_count, 0);
        }
    }

    #[test]
    fn concurrent_successors_create_exactly_one_pending_handoff() {
        use std::sync::{Arc, Barrier};

        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("state.sqlite");
        let database = Database::open(&database_path).unwrap();
        let repository = database
            .upsert_repository(
                "local:handoff-race",
                &directory.path().join("bare"),
                Some("main"),
            )
            .unwrap();
        let workspace = |id: &str, predecessor: Option<&str>| WorkspaceRecord {
            id: WorkspaceId(id.into()),
            repository_id: repository.id.clone(),
            session_id: None,
            path: directory.path().join(id),
            base_ref: "origin/main".into(),
            base_oid: ObjectId("opaque-base".into()),
            head_oid: ObjectId("opaque-base".into()),
            state: "materializing".into(),
            predecessor_id: predecessor.map(|value| WorkspaceId(value.into())),
            dependency_state: "ready".into(),
        };
        let parent = workspace("ws_race_parent", None);
        let left = workspace("ws_race_left", Some("ws_race_parent"));
        let right = workspace("ws_race_right", Some("ws_race_parent"));
        for record in [&parent, &left, &right] {
            database.create_workspace(record).unwrap();
        }
        let session = SessionRecord {
            id: SessionId("session-handoff-race".into()),
            repository_id: repository.id,
            workspace_id: parent.id.clone(),
            intent: None,
            state: "active".into(),
        };
        database.create_session_and_lease(&session, 120).unwrap();

        let left_database = Database::open(&database_path).unwrap();
        let right_database = Database::open(&database_path).unwrap();
        let left_operation = match left_database
            .begin_operation("host", "race-left", "hash-left", "workspace_sync")
            .unwrap()
        {
            BeginOperation::New(id) => id,
            BeginOperation::Existing(_) => unreachable!(),
        };
        let right_operation = match right_database
            .begin_operation("host", "race-right", "hash-right", "workspace_sync")
            .unwrap()
        {
            BeginOperation::New(id) => id,
            BeginOperation::Existing(_) => unreachable!(),
        };
        let barrier = Arc::new(Barrier::new(3));
        let left_barrier = Arc::clone(&barrier);
        let left_session = session.id.clone();
        let left_parent = parent.id.clone();
        let left_workspace = left.id.clone();
        let left_thread = std::thread::spawn(move || {
            left_barrier.wait();
            left_database.prepare_successor_handoff(
                &left_operation,
                "host",
                &left_session,
                &left_parent,
                &left_workspace,
            )
        });
        let right_barrier = Arc::clone(&barrier);
        let right_session = session.id.clone();
        let right_parent = parent.id.clone();
        let right_workspace = right.id.clone();
        let right_thread = std::thread::spawn(move || {
            right_barrier.wait();
            right_database.prepare_successor_handoff(
                &right_operation,
                "host",
                &right_session,
                &right_parent,
                &right_workspace,
            )
        });
        barrier.wait();
        let results = [left_thread.join().unwrap(), right_thread.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let winner = results
            .iter()
            .find_map(|result| result.as_ref().ok())
            .unwrap();
        assert!(results.iter().any(|result| matches!(
            result,
            Err(DbError::HandoffAlreadyPending { handoff_id })
                if handoff_id == &winner.handoff_id
        )));
        let structured: crate::engine::EngineError = DbError::HandoffAlreadyPending {
            handoff_id: winner.handoff_id.clone(),
        }
        .into();
        assert_eq!(structured.code, "HANDOFF_PENDING");
        assert_eq!(structured.retry, "never");
        let expected_next = format!("adopt pending handoff {}", winner.handoff_id);
        assert_eq!(structured.next.as_deref(), Some(expected_next.as_str()));
        assert_eq!(
            [left.id, right.id]
                .into_iter()
                .filter(|id| database.workspace(id).unwrap().unwrap().state == "handoff_pending")
                .count(),
            1
        );
    }

    #[test]
    fn interrupted_successor_adopt_resumes_with_the_same_idempotency_key() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("state.sqlite");
        let database = Database::open(&database_path).unwrap();
        let repository = database
            .upsert_repository(
                "local:adopt-recovery",
                &directory.path().join("bare"),
                Some("main"),
            )
            .unwrap();
        let workspace = |id: &str, predecessor: Option<&str>| WorkspaceRecord {
            id: WorkspaceId(id.into()),
            repository_id: repository.id.clone(),
            session_id: None,
            path: directory.path().join(id),
            base_ref: "origin/main".into(),
            base_oid: ObjectId("opaque-base".into()),
            head_oid: ObjectId("opaque-base".into()),
            state: "materializing".into(),
            predecessor_id: predecessor.map(|value| WorkspaceId(value.into())),
            dependency_state: "ready".into(),
        };
        let parent = workspace("ws_recovery_parent", None);
        let successor = workspace("ws_recovery_successor", Some("ws_recovery_parent"));
        database.create_workspace(&parent).unwrap();
        database.create_workspace(&successor).unwrap();
        let session = SessionRecord {
            id: SessionId("session-adopt-recovery".into()),
            repository_id: repository.id,
            workspace_id: parent.id.clone(),
            intent: None,
            state: "active".into(),
        };
        database.create_session_and_lease(&session, 120).unwrap();
        let prepare_operation = match database
            .begin_operation("host", "prepare-recovery", "prepare-hash", "workspace_sync")
            .unwrap()
        {
            BeginOperation::New(id) => id,
            BeginOperation::Existing(_) => unreachable!(),
        };
        let handoff = database
            .prepare_successor_handoff(
                &prepare_operation,
                "host",
                &session.id,
                &parent.id,
                &successor.id,
            )
            .unwrap();

        let crashed_adopt = match database
            .begin_operation("host", "handoff:stable", "adopt-hash", "successor_adopt")
            .unwrap()
        {
            BeginOperation::New(id) => id,
            BeginOperation::Existing(_) => unreachable!(),
        };
        let ordinary_interrupted = match database
            .begin_operation("host", "ordinary", "ordinary-hash", "session_open")
            .unwrap()
        {
            BeginOperation::New(id) => id,
            BeginOperation::Existing(_) => unreachable!(),
        };
        drop(database);
        let database = Database::open(&database_path).unwrap();
        let recovery_operation = match database
            .begin_operation("daemon", "startup", "recovery-hash", "reconcile")
            .unwrap()
        {
            BeginOperation::New(id) => id,
            BeginOperation::Existing(_) => unreachable!(),
        };
        assert_eq!(
            database
                .recover_interrupted_operations(&recovery_operation)
                .unwrap(),
            2
        );
        assert_eq!(
            database.operation(&crashed_adopt).unwrap().unwrap().state,
            "failed"
        );
        assert_eq!(
            database
                .operation(&crashed_adopt)
                .unwrap()
                .unwrap()
                .error
                .unwrap()
                .next
                .as_deref(),
            Some("retry with the same idempotency key")
        );
        assert_eq!(
            database
                .operation(&ordinary_interrupted)
                .unwrap()
                .unwrap()
                .state,
            "failed"
        );
        assert_eq!(
            database
                .handoff(&handoff.handoff_id)
                .unwrap()
                .unwrap()
                .state,
            "pending"
        );
        assert_eq!(
            database.session(&session.id).unwrap().unwrap().workspace_id,
            parent.id
        );

        let retry_barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let retry_left_database = Database::open(&database_path).unwrap();
        let retry_left_barrier = std::sync::Arc::clone(&retry_barrier);
        let retry_left = std::thread::spawn(move || {
            retry_left_barrier.wait();
            retry_left_database.begin_operation(
                "host",
                "handoff:stable",
                "adopt-hash",
                "successor_adopt",
            )
        });
        let retry_right_database = Database::open(&database_path).unwrap();
        let retry_right_barrier = std::sync::Arc::clone(&retry_barrier);
        let retry_right = std::thread::spawn(move || {
            retry_right_barrier.wait();
            retry_right_database.begin_operation(
                "host",
                "handoff:stable",
                "adopt-hash",
                "successor_adopt",
            )
        });
        retry_barrier.wait();
        let retry_results = [retry_left.join().unwrap(), retry_right.join().unwrap()];
        assert_eq!(
            retry_results
                .iter()
                .filter(|result| matches!(
                    result,
                    Ok(BeginOperation::New(id)) if id == &crashed_adopt
                ))
                .count(),
            1
        );
        assert_eq!(
            retry_results
                .iter()
                .filter(|result| matches!(
                    result,
                    Ok(BeginOperation::Existing(record))
                        if record.id == crashed_adopt && record.state == "running"
                ))
                .count(),
            1
        );
        let resumed = crashed_adopt.clone();
        let outcome = Outcome::Completed(json!({"workspace": successor.id}));
        database
            .adopt_successor_handoff(
                &handoff.handoff_id,
                "host",
                &resumed,
                &LeaseId("lease-recovered-successor".into()),
                120,
                &outcome,
            )
            .unwrap();
        assert_eq!(
            database.session(&session.id).unwrap().unwrap().workspace_id,
            successor.id
        );
        assert!(matches!(
            database
                .begin_operation(
                    "host",
                    "handoff:stable",
                    "adopt-hash",
                    "successor_adopt"
                )
                .unwrap(),
            BeginOperation::Existing(record)
                if record.id == crashed_adopt && record.state == "completed"
        ));
    }

    #[test]
    fn expired_parent_cancels_pending_handoff_for_gc() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("state.sqlite")).unwrap();
        let repository = database
            .upsert_repository("local:expiry", &directory.path().join("bare"), Some("main"))
            .unwrap();
        let parent = WorkspaceRecord {
            id: WorkspaceId("ws_parent".into()),
            repository_id: repository.id.clone(),
            session_id: None,
            path: directory.path().join("ws_parent"),
            base_ref: "origin/main".into(),
            base_oid: ObjectId("opaque-base".into()),
            head_oid: ObjectId("opaque-base".into()),
            state: "materializing".into(),
            predecessor_id: None,
            dependency_state: "ready".into(),
        };
        let successor = WorkspaceRecord {
            id: WorkspaceId("ws_successor".into()),
            repository_id: repository.id.clone(),
            session_id: None,
            path: directory.path().join("ws_successor"),
            base_ref: "origin/main".into(),
            base_oid: ObjectId("opaque-base".into()),
            head_oid: ObjectId("opaque-base".into()),
            state: "materializing".into(),
            predecessor_id: Some(parent.id.clone()),
            dependency_state: "ready".into(),
        };
        database.create_workspace(&parent).unwrap();
        database.create_workspace(&successor).unwrap();
        let session = SessionRecord {
            id: SessionId("session-expiry".into()),
            repository_id: repository.id,
            workspace_id: parent.id.clone(),
            intent: None,
            state: "active".into(),
        };
        database.create_session_and_lease(&session, 120).unwrap();
        let operation = match database
            .begin_operation("host", "prepare-expiry", "hash", "workspace_sync")
            .unwrap()
        {
            BeginOperation::New(id) => id,
            BeginOperation::Existing(_) => unreachable!(),
        };
        let pending = database
            .prepare_successor_handoff(&operation, "host", &session.id, &parent.id, &successor.id)
            .unwrap();
        assert_eq!(
            database.mark_expired_leases(now_ms() + 121_000).unwrap(),
            crate::db::ExpirySweep {
                sessions: 1,
                workspaces: 1,
                handoffs_cancelled: 1,
            }
        );
        assert_eq!(
            database
                .handoff(&pending.handoff_id)
                .unwrap()
                .unwrap()
                .state,
            "cancelled"
        );
        assert_eq!(
            database.workspace(&successor.id).unwrap().unwrap().state,
            "failed"
        );
        assert_eq!(
            database.session(&session.id).unwrap().unwrap().state,
            "dormant"
        );
    }

    #[test]
    fn gc_candidates_and_claim_recheck_every_durable_preservation_gate() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(directory.path().join("state.sqlite")).unwrap();
        let repository = database
            .upsert_repository("local:gc", &directory.path().join("bare"), Some("main"))
            .unwrap();
        let workspace = |id: &str| WorkspaceRecord {
            id: WorkspaceId(id.into()),
            repository_id: repository.id.clone(),
            session_id: None,
            path: directory.path().join(id),
            base_ref: "origin/main".into(),
            base_oid: ObjectId("opaque-base".into()),
            head_oid: ObjectId("opaque-base".into()),
            state: "released".into(),
            predecessor_id: None,
            dependency_state: "ready".into(),
        };
        let eligible = workspace("ws_eligible");
        let leased = workspace("ws_leased");
        let active = workspace("ws_operation");
        let review = workspace("ws_review");
        let checkpoint = workspace("ws_checkpoint");
        let publish_parent = workspace("ws_publish_parent");
        let publish_resolution = workspace("ws_publish_resolution");
        for record in [
            &eligible,
            &leased,
            &active,
            &review,
            &checkpoint,
            &publish_parent,
            &publish_resolution,
        ] {
            database.create_workspace(record).unwrap();
        }

        database
            .create_session_and_lease(
                &SessionRecord {
                    id: SessionId("session-gc-live".into()),
                    repository_id: repository.id.clone(),
                    workspace_id: leased.id.clone(),
                    intent: None,
                    state: "active".into(),
                },
                120,
            )
            .unwrap();
        database
            .mark_workspace_state(&leased.id, "released")
            .unwrap();

        let running = match database
            .begin_operation("gc-test", "running", "hash", "workspace_checkpoint")
            .unwrap()
        {
            BeginOperation::New(operation) => operation,
            BeginOperation::Existing(_) => unreachable!(),
        };
        database
            .bind_operation_resource(&running, &active.id.0, "checkpoint")
            .unwrap();
        database
            .create_review(&ReviewRecord {
                id: ReviewId("review-gc".into()),
                workspace_id: review.id.clone(),
                kind: "secrets".into(),
                payload: json!({}),
                state: "pending".into(),
            })
            .unwrap();
        database
            .create_checkpoint(&CheckpointRecord {
                id: CheckpointId("checkpoint-gc".into()),
                workspace_id: checkpoint.id.clone(),
                head_oid: checkpoint.head_oid.clone(),
                index_oid: ObjectId("index".into()),
                worktree_oid: ObjectId("worktree".into()),
                reason: "failed".into(),
                state: "failed".into(),
            })
            .unwrap();
        database
            .create_publish_resolution(&PublishResolutionRecord {
                workspace_id: publish_resolution.id.clone(),
                parent_workspace_id: publish_parent.id.clone(),
                branch: "main".into(),
                message: "pending".into(),
                push: false,
                expected_remote_oid: ObjectId("remote".into()),
                expected_local_oid: None,
                state: "pending".into(),
            })
            .unwrap();

        let cutoff = now_ms() + 1_000;
        let candidates = database
            .gc_candidates(cutoff)
            .unwrap()
            .into_iter()
            .map(|record| record.id.0)
            .collect::<Vec<_>>();
        assert_eq!(candidates, vec![eligible.id.0.clone()]);

        // A protection appearing after the candidate scan must still win the
        // transactional claim immediately before deletion.
        database
            .create_review(&ReviewRecord {
                id: ReviewId("review-race".into()),
                workspace_id: eligible.id.clone(),
                kind: "secrets".into(),
                payload: json!({}),
                state: "pending".into(),
            })
            .unwrap();
        assert!(
            !database
                .claim_workspace_for_gc(&eligible.id, cutoff, &OperationId("operation-gc".into()),)
                .unwrap()
        );
    }
}
