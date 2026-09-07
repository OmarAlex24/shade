//! Dormancy: what happens to a workspace when its lease expires.
//!
//! The invariant every test here defends is that losing a lease loses
//! exclusivity, never work. A dormant workspace keeps its tree, stays out of
//! the collector's candidate set, and comes back with `open` or `attach`.

use shade_engine::Engine;
use shade_engine::config::EngineConfig;
use shade_engine::dependencies::DependencyService;
use shade_engine::filesystem::CopyFilesystem;
use shade_protocol::{
    Actor, ActorKind, CompactContext, ExecuteRequest, Intent, OpenSession, OpenedSession, Outcome,
    PROTOCOL_VERSION, Query, QueryRequest, RepositoryLocator, ResponseBody, SessionId,
    SessionStatus, ShadeError, SleepResult, WorkspaceSelector,
};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SESSION: &str = "dormancy-session";

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(root: &Path) -> PathBuf {
    let repository = root.join("source");
    fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init", "-b", "main"]);
    git(&repository, &["config", "user.name", "Shade Test"]);
    git(&repository, &["config", "user.email", "shade@test.invalid"]);
    fs::write(repository.join("tracked.txt"), "base\n").unwrap();
    git(&repository, &["add", "tracked.txt"]);
    git(&repository, &["commit", "-m", "base"]);
    repository
}

fn execute(key: &str, intent: Intent) -> ExecuteRequest {
    ExecuteRequest {
        v: PROTOCOL_VERSION,
        request_id: format!("request-{key}"),
        idempotency_key: key.to_owned(),
        actor: Actor {
            kind: ActorKind::Host,
            id: "dormancy-test-actor".into(),
        },
        intent,
    }
}

fn system(key: &str, intent: Intent) -> ExecuteRequest {
    ExecuteRequest {
        actor: Actor {
            kind: ActorKind::System,
            id: "dormancy-test-daemon".into(),
        },
        ..execute(key, intent)
    }
}

fn query(query: Query) -> QueryRequest {
    QueryRequest {
        v: PROTOCOL_VERSION,
        request_id: "request-query".into(),
        query,
    }
}

fn completed<T: serde::de::DeserializeOwned>(response: shade_protocol::WireResponse) -> T {
    match response.body {
        ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        } => serde_json::from_value(value).unwrap(),
        other => panic!("expected completed response, got {other:?}"),
    }
}

fn failed(response: shade_protocol::WireResponse) -> ShadeError {
    match response.body {
        ResponseBody::Error { error } => error,
        other => panic!("expected an error response, got {other:?}"),
    }
}

fn engine_at(root: &Path) -> Engine {
    Engine::with_components(
        // Grace zero and a one-second TTL make every collectability assertion
        // in this file about the lifecycle state alone, never about waiting.
        EngineConfig::at(root.join("state"))
            .with_harness_lifecycle_timing(1, 0)
            .unwrap(),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap()
}

fn open_request(repository: &Path, session: &str, base: Option<&str>) -> Intent {
    Intent::SessionOpen(OpenSession {
        session_id: SessionId(session.to_owned()),
        repository: RepositoryLocator::Local {
            path: repository.to_string_lossy().into_owned(),
        },
        base: base.map(str::to_owned),
        intent: None,
    })
}

fn selector(opened: &OpenedSession) -> WorkspaceSelector {
    WorkspaceSelector {
        workspace_id: Some(opened.workspace.clone()),
        cwd: None,
    }
}

/// Open one session, then let its lease expire the way the daemon's sweep
/// would. Returns the engine and the session as it was when it was Active.
async fn dormant_session(root: &Path) -> (Engine, OpenedSession) {
    let repository = fixture(root);
    let engine = engine_at(root);
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );
    let sweep = engine
        .database()
        .mark_expired_leases(shade_engine::db::now_ms() + 10_000)
        .unwrap();
    assert_eq!(sweep.sessions, 1);
    assert_eq!(sweep.workspaces, 1);
    (engine, opened)
}

fn session_state(engine: &Engine, session: &SessionId) -> String {
    engine.database().session(session).unwrap().unwrap().state
}

fn workspace_state(engine: &Engine, opened: &OpenedSession) -> String {
    engine
        .database()
        .workspace(&opened.workspace)
        .unwrap()
        .unwrap()
        .state
}

/// Run the collector.
///
/// The zero-second harness grace is still a strict millisecond cutoff, so
/// advance past the timestamp of whatever the test just did before asking the
/// collector to look at it.
async fn collect(engine: &Engine, key: &str) -> serde_json::Value {
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    completed(engine.execute(execute(key, Intent::GarbageCollect)).await)
}

#[tokio::test]
async fn expired_lease_becomes_dormant_and_is_never_a_gc_candidate() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened) = dormant_session(directory.path()).await;

    assert_eq!(session_state(&engine, &opened.session), "dormant");
    assert_eq!(workspace_state(&engine, &opened), "dormant");
    let tree = PathBuf::from(&opened.cwd);
    assert!(tree.join("tracked.txt").is_file());

    let collected = collect(&engine, "gc-dormant").await;
    assert_eq!(collected["eligible"], 0);
    assert_eq!(collected["deleted"], 0);
    assert!(
        tree.join("tracked.txt").is_file(),
        "a dormant workspace keeps its content"
    );
    assert_eq!(workspace_state(&engine, &opened), "dormant");
}

#[tokio::test]
async fn reattach_renews_the_lease_and_returns_the_same_cwd_and_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened) = dormant_session(directory.path()).await;

    let resumed: OpenedSession = completed(
        engine
            .execute(execute(
                "attach",
                Intent::SessionReattach {
                    session_id: opened.session.clone(),
                },
            ))
            .await,
    );
    assert_eq!(resumed.workspace, opened.workspace);
    assert_eq!(resumed.cwd, opened.cwd);
    assert_ne!(resumed.lease, opened.lease);
    assert_eq!(resumed.compact_context.lifecycle, "active");
    assert_eq!(session_state(&engine, &opened.session), "active");
    assert_eq!(workspace_state(&engine, &opened), "ready");

    let lease = engine
        .database()
        .active_lease_for_session(&opened.session)
        .unwrap()
        .unwrap();
    assert_eq!(lease.id, resumed.lease);
    assert_eq!(lease.fence, 2, "a fresh lease is the successor fence");

    // The point of reattaching is being allowed to mutate again.
    let checkpoint: serde_json::Value = completed(
        engine
            .execute(execute(
                "checkpoint-after-attach",
                Intent::WorkspaceCheckpoint {
                    selector: selector(&opened),
                    reason: "manual".into(),
                },
            ))
            .await,
    );
    assert!(checkpoint["checkpoint_id"].is_string());
}

#[tokio::test]
async fn open_with_an_existing_dormant_session_id_reattaches_instead_of_erroring() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened) = dormant_session(directory.path()).await;
    let repository = directory.path().join("source");

    let resumed: OpenedSession = completed(
        engine
            .execute(execute("reopen", open_request(&repository, SESSION, None)))
            .await,
    );
    assert_eq!(resumed.workspace, opened.workspace);
    assert_eq!(resumed.cwd, opened.cwd);
    assert_eq!(session_state(&engine, &opened.session), "active");

    // A different base is a different workspace, so it must be refused rather
    // than silently answered with the old one.
    let mismatch = failed(
        engine
            .execute(execute(
                "reopen-other-base",
                open_request(&repository, SESSION, Some("other-branch")),
            ))
            .await,
    );
    assert_eq!(mismatch.code, "SESSION_BASE_MISMATCH");
    assert!(mismatch.next.unwrap().contains("shade fork"));

    // The stored base is `origin/main`; asking for `main` is the same base.
    let same_base: OpenedSession = completed(
        engine
            .execute(execute(
                "reopen-same-base",
                open_request(&repository, SESSION, Some("main")),
            ))
            .await,
    );
    assert_eq!(same_base.workspace, opened.workspace);
}

/// Move a live lease's deadline without releasing it, the way a lease that has
/// been running down since its last heartbeat looks.
fn set_lease_deadline(root: &Path, session: &SessionId, expires_at_ms: i64) {
    let connection =
        rusqlite::Connection::open(EngineConfig::at(root.join("state")).database_path()).unwrap();
    let changed = connection
        .execute(
            "UPDATE leases SET expires_at_ms=?2 WHERE session_id=?1 AND released_at_ms IS NULL",
            rusqlite::params![session.0, expires_at_ms],
        )
        .unwrap();
    assert_eq!(changed, 1);
}

#[tokio::test]
async fn reattach_of_an_active_session_is_idempotent_and_renews_its_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let ttl_secs = 120;
    let engine = engine_with_lease_ttl(directory.path(), ttl_secs);
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );
    // Live, but with a fraction of a second left on it: the state an `attach`
    // arrives in after the previous holder stopped heartbeating.
    let expiring = shade_engine::db::now_ms() + 200;
    set_lease_deadline(directory.path(), &opened.session, expiring);

    let resumed: OpenedSession = completed(
        engine
            .execute(execute(
                "attach-active",
                Intent::SessionReattach {
                    session_id: opened.session.clone(),
                },
            ))
            .await,
    );
    assert_eq!(
        resumed.lease, opened.lease,
        "no second live lease is minted"
    );
    assert_eq!(resumed.workspace, opened.workspace);
    let lease = engine
        .database()
        .active_lease_for_session(&opened.session)
        .unwrap()
        .unwrap();
    assert_eq!(lease.fence, 1, "the same lease at the same fence");
    assert!(
        lease.expires_at_ms >= shade_engine::db::now_ms() + ttl_secs * 1000 - 5_000,
        "an idempotent reattach still hands back a full TTL, not {} ms of one",
        lease.expires_at_ms - expiring
    );
}

#[tokio::test]
async fn reattach_after_release_returns_session_already_released() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened) = dormant_session(directory.path()).await;
    let _: serde_json::Value = completed(
        engine
            .execute(execute(
                "release",
                Intent::WorkspaceRelease {
                    selector: selector(&opened),
                },
            ))
            .await,
    );

    let error = failed(
        engine
            .execute(execute(
                "attach-released",
                Intent::SessionReattach {
                    session_id: opened.session.clone(),
                },
            ))
            .await,
    );
    assert_eq!(error.code, "SESSION_ALREADY_RELEASED");

    let reopened = failed(
        engine
            .execute(execute(
                "reopen-released",
                open_request(&directory.path().join("source"), SESSION, None),
            ))
            .await,
    );
    assert_eq!(reopened.code, "SESSION_ALREADY_RELEASED");
}

#[tokio::test]
async fn dormant_workspace_rejects_mutations_until_reattach() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened) = dormant_session(directory.path()).await;

    for (key, intent) in [
        (
            "checkpoint",
            Intent::WorkspaceCheckpoint {
                selector: selector(&opened),
                reason: "manual".into(),
            },
        ),
        (
            "fork",
            Intent::WorkspaceFork {
                selector: selector(&opened),
                child_session_id: SessionId("dormancy-child".into()),
                intent: None,
            },
        ),
        (
            "sync",
            Intent::WorkspaceSync {
                selector: selector(&opened),
            },
        ),
    ] {
        let error = failed(engine.execute(execute(key, intent)).await);
        assert!(
            error.code == "LEASE_EXPIRED" || error.code == "LEASE_FENCED",
            "{key} must be fenced while dormant, got {}",
            error.code
        );
        if error.code == "LEASE_EXPIRED" {
            assert_eq!(
                error.next.as_deref(),
                Some(format!("shade attach --session {SESSION}").as_str()),
                "the error must name the command that recovers the session"
            );
        }
    }

    let _: OpenedSession = completed(
        engine
            .execute(execute(
                "attach",
                Intent::SessionReattach {
                    session_id: opened.session.clone(),
                },
            ))
            .await,
    );
    let checkpoint: serde_json::Value = completed(
        engine
            .execute(execute(
                "checkpoint-after",
                Intent::WorkspaceCheckpoint {
                    selector: selector(&opened),
                    reason: "manual".into(),
                },
            ))
            .await,
    );
    assert!(checkpoint["checkpoint_id"].is_string());
}

#[tokio::test]
async fn release_works_on_a_dormant_workspace_and_only_then_is_it_gc_eligible() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened) = dormant_session(directory.path()).await;
    let tree = PathBuf::from(&opened.cwd);

    let released: serde_json::Value = completed(
        engine
            .execute(execute(
                "release-dormant",
                Intent::WorkspaceRelease {
                    selector: selector(&opened),
                },
            ))
            .await,
    );
    assert_eq!(released["released"], true);
    assert_eq!(session_state(&engine, &opened.session), "released");
    assert_eq!(workspace_state(&engine, &opened), "released");

    let collected = collect(&engine, "gc-released").await;
    assert_eq!(collected["deleted"], 1);
    assert!(
        !tree.exists(),
        "an explicitly released workspace is deleted"
    );

    // Releasing twice is a caller mistake, not a second deletion.
    let again = failed(
        engine
            .execute(execute(
                "release-twice",
                Intent::WorkspaceRelease {
                    selector: selector(&opened),
                },
            ))
            .await,
    );
    assert_eq!(again.code, "WORKSPACE_NOT_FOUND");
}

/// Expire the lease the way real time does, without running the sweep that
/// rewrites the session and workspace rows.
///
/// This is the window every embedded host lives in: `MaintenanceSweep` is at
/// most 30 s away in the daemon and never arrives at all for a host that only
/// issues explicit intents, so `release` has to behave here exactly as it does
/// afterwards.
fn expire_lease_without_sweeping(root: &Path, session: &SessionId) {
    let connection =
        rusqlite::Connection::open(EngineConfig::at(root.join("state")).database_path()).unwrap();
    let changed = connection
        .execute(
            "UPDATE leases SET expires_at_ms=?2 WHERE session_id=?1 AND released_at_ms IS NULL",
            rusqlite::params![session.0, shade_engine::db::now_ms() - 1],
        )
        .unwrap();
    assert_eq!(changed, 1, "exactly one live lease to expire");
}

#[tokio::test]
async fn an_unswept_expired_lease_behaves_exactly_like_a_swept_one() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_at(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );
    expire_lease_without_sweeping(directory.path(), &opened.session);

    // The row still says `active`; only the deadline has passed.
    assert_eq!(session_state(&engine, &opened.session), "active");
    assert_eq!(workspace_state(&engine, &opened), "ready");

    let context: CompactContext = completed(
        engine
            .query(query(Query::Context {
                selector: selector(&opened),
            }))
            .await,
    );
    assert_eq!(context.lifecycle, "dormant");

    // A mutation is refused the same way it is after the sweep: recoverable,
    // and naming the command that recovers it.
    let refused = failed(
        engine
            .execute(execute(
                "checkpoint-unswept",
                Intent::WorkspaceCheckpoint {
                    selector: selector(&opened),
                    reason: "unswept".into(),
                },
            ))
            .await,
    );
    assert_eq!(refused.code, "LEASE_EXPIRED");
    assert!(
        refused.next.unwrap().contains("shade attach"),
        "dormancy names the command that recovers it"
    );

    // And release still works. Deriving the lease requirement from the session
    // row instead of the lease made this return LEASE_FENCED.
    let released: serde_json::Value = completed(
        engine
            .execute(execute(
                "release-unswept",
                Intent::WorkspaceRelease {
                    selector: selector(&opened),
                },
            ))
            .await,
    );
    assert_eq!(released["released"], true);
    assert_eq!(session_state(&engine, &opened.session), "released");
    assert_eq!(workspace_state(&engine, &opened), "released");
    assert!(
        engine
            .database()
            .active_lease_for_session(&opened.session)
            .unwrap()
            .is_none_or(|lease| lease.released_at_ms.is_some()),
        "the release surrenders the expired lease row too"
    );

    let collected = collect(&engine, "gc-unswept-release").await;
    assert_eq!(collected["deleted"], 1);
}

#[tokio::test]
async fn an_unswept_dormant_session_can_still_be_reattached() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_at(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );
    expire_lease_without_sweeping(directory.path(), &opened.session);

    let resumed: OpenedSession = completed(
        engine
            .execute(execute(
                "attach-unswept",
                Intent::SessionReattach {
                    session_id: opened.session.clone(),
                },
            ))
            .await,
    );
    assert_ne!(resumed.lease, opened.lease, "a fresh lease at a new fence");
    assert_eq!(resumed.workspace, opened.workspace);
    assert_eq!(session_state(&engine, &opened.session), "active");
}

/// A long enough lease that a multi-step test still holds a live one, without
/// leaving the collectability assertions to a timer.
fn engine_with_lease_ttl(root: &Path, ttl_secs: i64) -> Engine {
    Engine::with_components(
        EngineConfig::at(root.join("state"))
            .with_harness_lifecycle_timing(ttl_secs, 0)
            .unwrap(),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap()
}

#[tokio::test]
async fn a_failed_workspace_with_a_live_lease_can_still_checkpoint_and_release() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_with_lease_ttl(directory.path(), 120);
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );
    fs::write(Path::new(&opened.cwd).join("tracked.txt"), "edited\n").unwrap();

    // Reconciliation's verdict, planted directly: the record says `failed`
    // while the tree is intact and the lease is still live.
    engine
        .database()
        .mark_workspace_state(&opened.workspace, "failed")
        .unwrap();

    let checkpoint: serde_json::Value = completed(
        engine
            .execute(execute(
                "checkpoint-failed",
                Intent::WorkspaceCheckpoint {
                    selector: selector(&opened),
                    reason: "salvage".into(),
                },
            ))
            .await,
    );
    assert!(
        checkpoint["checkpoint_id"].is_string(),
        "a live lease still owns its workspace: {checkpoint}"
    );

    let released: serde_json::Value = completed(
        engine
            .execute(execute(
                "release-failed",
                Intent::WorkspaceRelease {
                    selector: selector(&opened),
                },
            ))
            .await,
    );
    assert_eq!(released["released"], true);
    assert!(
        released["checkpoint_id"].is_string(),
        "the release checkpoint still captures the edit: {released}"
    );
    assert_eq!(workspace_state(&engine, &opened), "released");
    assert_eq!(session_state(&engine, &opened.session), "released");
}

#[tokio::test]
async fn a_failed_workspace_that_lost_its_tree_is_still_releasable() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_with_lease_ttl(directory.path(), 120);
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );
    // The reason reconciliation marks a workspace `failed` in the first place.
    fs::remove_dir_all(&opened.cwd).unwrap();
    engine
        .database()
        .mark_workspace_state(&opened.workspace, "failed")
        .unwrap();

    let released: serde_json::Value = completed(
        engine
            .execute(execute(
                "release-lost-tree",
                Intent::WorkspaceRelease {
                    selector: selector(&opened),
                },
            ))
            .await,
    );
    assert_eq!(released["released"], true);
    assert!(
        released["checkpoint_id"].is_null(),
        "there is no tree left to checkpoint: {released}"
    );
    assert_eq!(workspace_state(&engine, &opened), "released");

    let collected = collect(&engine, "gc-failed-release").await;
    assert_eq!(collected["deleted"], 1);
}

#[tokio::test]
async fn context_reports_lifecycle_dormant_and_active() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened) = dormant_session(directory.path()).await;

    let dormant: CompactContext = completed(
        engine
            .query(query(Query::Context {
                selector: selector(&opened),
            }))
            .await,
    );
    assert_eq!(dormant.lifecycle, "dormant");
    assert_eq!(dormant.lease, "released");

    let status: SessionStatus = completed(
        engine
            .query(query(Query::Session {
                session_id: opened.session.clone(),
            }))
            .await,
    );
    assert_eq!(status.lifecycle, "dormant");
    assert_eq!(status.workspace, opened.workspace);
    assert!(status.materialized);
    assert_eq!(status.cwd.as_deref(), Some(opened.cwd.as_str()));
    assert!(status.lease.is_none());

    let _: OpenedSession = completed(
        engine
            .execute(execute(
                "attach",
                Intent::SessionReattach {
                    session_id: opened.session.clone(),
                },
            ))
            .await,
    );
    let active: CompactContext = completed(
        engine
            .query(query(Query::Context {
                selector: selector(&opened),
            }))
            .await,
    );
    assert_eq!(active.lifecycle, "active");
    assert_eq!(active.lease, "live");

    let status: SessionStatus = completed(
        engine
            .query(query(Query::Session {
                session_id: opened.session.clone(),
            }))
            .await,
    );
    assert_eq!(status.lifecycle, "active");
    assert!(status.lease.is_some());
    assert!(status.lease_expires_at_ms.is_some());
}

#[tokio::test]
async fn startup_normalizes_legacy_orphaned_rows_to_dormant() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened) = dormant_session(directory.path()).await;
    let tree = PathBuf::from(&opened.cwd);

    // Exactly what a pre-dormancy binary would have left behind.
    {
        let connection = rusqlite::Connection::open(
            EngineConfig::at(directory.path().join("state")).database_path(),
        )
        .unwrap();
        connection
            .execute("UPDATE sessions SET state='orphaned'", [])
            .unwrap();
        connection
            .execute("UPDATE workspaces SET state='orphaned'", [])
            .unwrap();
    }

    let reconciled: serde_json::Value =
        completed(engine.execute(system("reconcile", Intent::Reconcile)).await);
    assert_eq!(reconciled["legacy_states_normalized"], 2);
    assert_eq!(session_state(&engine, &opened.session), "dormant");
    assert_eq!(workspace_state(&engine, &opened), "dormant");

    let collected = collect(&engine, "gc-after-normalize").await;
    assert_eq!(collected["deleted"], 0);
    assert!(tree.join("tracked.txt").is_file());
}

#[tokio::test]
async fn secret_decisions_remain_actionable_while_dormant() {
    for action in [
        shade_protocol::ReviewAction::Keep,
        shade_protocol::ReviewAction::Discard,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let repository = fixture(directory.path());
        let engine = engine_at(directory.path());
        let opened: OpenedSession = completed(
            engine
                .execute(execute("open", open_request(&repository, SESSION, None)))
                .await,
        );
        fs::write(
            Path::new(&opened.cwd).join(".env.local"),
            "TOKEN=dormant-fixture\n",
        )
        .unwrap();
        let response = engine
            .execute(execute(
                "release-for-review",
                Intent::WorkspaceRelease {
                    selector: selector(&opened),
                },
            ))
            .await;
        let review_id = match response.body {
            ResponseBody::Ok {
                outcome: Outcome::ReviewRequired(review),
            } => review.review_id,
            other => panic!("expected a secret review: {other:?}"),
        };

        engine
            .database()
            .mark_expired_leases(shade_engine::db::now_ms() + 10_000)
            .unwrap();
        assert_eq!(session_state(&engine, &opened.session), "dormant");

        let expected = if matches!(action, shade_protocol::ReviewAction::Keep) {
            "retained"
        } else {
            "released"
        };
        let _: serde_json::Value = completed(
            engine
                .execute(execute(
                    "resolve-dormant",
                    Intent::ReviewResolve { review_id, action },
                ))
                .await,
        );
        assert_eq!(workspace_state(&engine, &opened), expected);
    }
}

// --- Phase 2: sleep and wake ------------------------------------------------

/// A provider that materializes a fixture dependency layer, so a test can tell
/// whether wake rebuilt one.
struct FixtureDependencies;

#[async_trait::async_trait]
impl shade_engine::dependencies::DependencyProvider for FixtureDependencies {
    fn name(&self) -> &'static str {
        "fixture-js"
    }
    fn applies(&self, _root: &Path) -> bool {
        true
    }
    async fn ensure_ready(
        &self,
        context: &shade_engine::dependencies::DependencyContext<'_>,
    ) -> Result<
        shade_engine::dependencies::DependencyReceipt,
        shade_engine::dependencies::DependencyError,
    > {
        let path = context
            .workspace_root
            .join("node_modules/shade-fixture/index.js");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "module.exports = 1;\n").unwrap();
        Ok(shade_engine::dependencies::DependencyReceipt {
            provider: self.name().into(),
            fingerprint: "fixture-layer".into(),
            state: "ready".into(),
            materialized_paths: vec!["node_modules".into()],
            blocked_builds: vec![],
            scripts: vec![],
        })
    }
}

struct FailingDependencies;

#[async_trait::async_trait]
impl shade_engine::dependencies::DependencyProvider for FailingDependencies {
    fn name(&self) -> &'static str {
        "failure-fixture"
    }
    fn applies(&self, _root: &Path) -> bool {
        true
    }
    async fn ensure_ready(
        &self,
        _context: &shade_engine::dependencies::DependencyContext<'_>,
    ) -> Result<
        shade_engine::dependencies::DependencyReceipt,
        shade_engine::dependencies::DependencyError,
    > {
        Err(shade_engine::dependencies::DependencyError::Failed(
            "dependency provider is unavailable".into(),
        ))
    }
}

fn engine_with(
    root: &Path,
    providers: Vec<Box<dyn shade_engine::dependencies::DependencyProvider>>,
) -> Engine {
    Engine::with_components(
        EngineConfig::at(root.join("state"))
            .with_harness_lifecycle_timing(1, 0)
            .unwrap(),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(providers)),
    )
    .unwrap()
}

async fn sleep_workspace(engine: &Engine, key: &str, opened: &OpenedSession) -> SleepResult {
    completed(
        engine
            .execute(execute(
                key,
                Intent::WorkspaceSleep {
                    selector: selector(opened),
                },
            ))
            .await,
    )
}

async fn wake(engine: &Engine, key: &str, session: &SessionId) -> shade_protocol::WireResponse {
    engine
        .execute(execute(
            key,
            Intent::SessionWake {
                session_id: session.clone(),
            },
        ))
        .await
}

fn registered_worktrees(engine: &Engine, opened: &OpenedSession) -> String {
    let workspace = engine
        .database()
        .workspace(&opened.workspace)
        .unwrap()
        .unwrap();
    let repository = engine
        .database()
        .repository_by_id(&workspace.repository_id)
        .unwrap()
        .unwrap();
    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&repository.bare_path)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Open one session and put its workspace to sleep. Returns the engine, the
/// session as it was when Active, and the sleep result.
async fn suspended_session(
    root: &Path,
    providers: Vec<Box<dyn shade_engine::dependencies::DependencyProvider>>,
) -> (Engine, OpenedSession, SleepResult) {
    let repository = fixture(root);
    let engine = engine_with(root, providers);
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );
    let slept = sleep_workspace(&engine, "sleep", &opened).await;
    (engine, opened, slept)
}

fn backdate(root: &Path, workspace: &shade_protocol::WorkspaceId, millis: i64) {
    let connection =
        rusqlite::Connection::open(EngineConfig::at(root.join("state")).database_path()).unwrap();
    connection
        .execute(
            "UPDATE workspaces SET updated_at_ms=updated_at_ms-?2 WHERE id=?1",
            rusqlite::params![workspace.0, millis],
        )
        .unwrap();
}

#[tokio::test]
async fn sleep_checkpoints_dematerializes_and_preserves_every_record() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_at(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );
    let tree = PathBuf::from(&opened.cwd);
    fs::write(tree.join("tracked.txt"), "edited before sleeping\n").unwrap();

    let slept = sleep_workspace(&engine, "sleep", &opened).await;
    assert!(slept.suspended);
    assert_eq!(slept.session, opened.session);
    assert_eq!(slept.workspace, opened.workspace);
    assert!(
        slept.reclaimed_bytes > 0,
        "sleeping a materialized tree reclaims disk"
    );

    assert!(!tree.exists(), "the tree is what sleep gives up");
    let worktrees = registered_worktrees(&engine, &opened);
    assert!(
        !worktrees.contains(&opened.cwd),
        "the worktree registration goes with the tree: {worktrees}"
    );

    assert_eq!(workspace_state(&engine, &opened), "suspended");
    assert_eq!(session_state(&engine, &opened.session), "suspended");
    assert!(
        engine
            .database()
            .active_lease_for_session(&opened.session)
            .unwrap()
            .is_none(),
        "a suspended session holds no lease"
    );

    // Everything that is not the tree survives.
    let checkpoints = engine
        .database()
        .checkpoints_for_workspace(&opened.workspace)
        .unwrap();
    assert!(
        checkpoints
            .iter()
            .any(|checkpoint| checkpoint.id == slept.checkpoint_id
                && checkpoint.reason == "sleep"
                && checkpoint.state == "ready")
    );
    let suspension = engine
        .database()
        .suspension_checkpoint(&opened.workspace)
        .unwrap()
        .unwrap();
    assert_eq!(suspension.id, slept.checkpoint_id);
    assert!(
        directory
            .path()
            .join("state/secrets")
            .join(&opened.workspace.0)
            .is_dir(),
        "the secret baseline outlives the tree"
    );
}

#[tokio::test]
async fn sleep_preserves_untracked_secret_files_without_requiring_a_review() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_at(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );
    fs::write(
        Path::new(&opened.cwd).join(".env.local"),
        "TOKEN=suspension-fixture\n",
    )
    .unwrap();

    // Sleep preserves private files instead of deleting them, so unlike
    // release it never has a decision to ask a human for.
    let response = engine
        .execute(execute(
            "sleep-with-secret",
            Intent::WorkspaceSleep {
                selector: selector(&opened),
            },
        ))
        .await;
    assert!(
        matches!(
            response.body,
            ResponseBody::Ok {
                outcome: Outcome::Completed(_)
            }
        ),
        "sleep must not require a secret review: {response:?}"
    );

    let vault = directory
        .path()
        .join("state/secrets")
        .join(&opened.workspace.0)
        .join("suspended");
    let file = vault.join("files/.env.local");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "TOKEN=suspension-fixture\n"
    );
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&vault).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[tokio::test]
async fn wake_restores_content_from_the_sleep_checkpoint_and_the_shared_dependency_layer() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_with(directory.path(), vec![Box::new(FixtureDependencies)]);
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );
    let tree = PathBuf::from(&opened.cwd);
    assert!(tree.join("node_modules/shade-fixture/index.js").is_file());

    fs::remove_file(tree.join("tracked.txt")).unwrap();
    fs::write(tree.join("added.txt"), "written by the agent\n").unwrap();
    fs::write(tree.join("script.sh"), "#!/bin/sh\necho shade\n").unwrap();
    fs::set_permissions(tree.join("script.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("added.txt", tree.join("link.txt")).unwrap();
    fs::write(tree.join(".env.local"), "TOKEN=woken\n").unwrap();

    let slept = sleep_workspace(&engine, "sleep", &opened).await;
    let woken: OpenedSession = completed(wake(&engine, "wake", &opened.session).await);

    assert_eq!(woken.session, opened.session, "the session id is stable");
    assert_ne!(
        woken.workspace, opened.workspace,
        "wake produces a successor, like restore"
    );
    assert_ne!(woken.cwd, opened.cwd);
    assert_eq!(woken.env["SHADE_SESSION"], opened.session.0);
    assert_eq!(woken.env["SHADE_WORKSPACE"], woken.workspace.0);
    assert_eq!(woken.compact_context.lifecycle, "active");

    let successor = PathBuf::from(&woken.cwd);
    assert!(
        !successor.join("tracked.txt").exists(),
        "a file the agent deleted stays deleted"
    );
    assert_eq!(
        fs::read_to_string(successor.join("added.txt")).unwrap(),
        "written by the agent\n"
    );
    assert_eq!(
        fs::metadata(successor.join("script.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111,
        "an executable bit survives the round trip"
    );
    assert_eq!(
        fs::read_link(successor.join("link.txt")).unwrap(),
        Path::new("added.txt")
    );
    assert_eq!(
        fs::read_to_string(successor.join(".env.local")).unwrap(),
        "TOKEN=woken\n",
        "the suspension vault is restored into the successor"
    );
    assert!(
        successor
            .join("node_modules/shade-fixture/index.js")
            .is_file(),
        "the dependency layer is rebuilt from its recorded fingerprint"
    );

    // The predecessor is released in the same transaction that binds the
    // successor, so exactly one workspace owns the session.
    assert_eq!(workspace_state(&engine, &opened), "released");
    assert_eq!(session_state(&engine, &opened.session), "active");
    let successor_record = engine
        .database()
        .workspace(&woken.workspace)
        .unwrap()
        .unwrap();
    assert_eq!(successor_record.state, "ready");
    assert_eq!(successor_record.session_id, Some(opened.session.clone()));
    assert_eq!(successor_record.predecessor_id, Some(opened.workspace));
    assert!(!slept.checkpoint_id.0.is_empty());
}

#[tokio::test]
async fn wake_failure_leaves_the_workspace_suspended() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened, _) = suspended_session(directory.path(), Vec::new()).await;
    drop(engine);

    let broken = engine_with(directory.path(), vec![Box::new(FailingDependencies)]);
    let error = failed(wake(&broken, "wake-broken", &opened.session).await);
    assert_ne!(error.code, "");
    assert_eq!(
        workspace_state(&broken, &opened),
        "suspended",
        "a failed wake never touches the suspension it was building from"
    );
    assert_eq!(session_state(&broken, &opened.session), "suspended");
    drop(broken);

    let engine = engine_with(directory.path(), Vec::new());
    let woken: OpenedSession = completed(wake(&engine, "wake-retry", &opened.session).await);
    assert_eq!(woken.session, opened.session);
    assert!(PathBuf::from(&woken.cwd).join("tracked.txt").is_file());
    assert_eq!(workspace_state(&engine, &opened), "released");
}

#[tokio::test]
async fn suspended_workspace_survives_startup_reconciliation() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened, _) = suspended_session(directory.path(), Vec::new()).await;
    drop(engine);

    // This is the regression test for the reconciliation skip: without it the
    // missing directory reads as corruption and every suspended workspace is
    // marked `failed` and handed to the collector on the next daemon start.
    let restarted = engine_with(directory.path(), Vec::new());
    let reconciled: serde_json::Value = completed(
        restarted
            .execute(system("reconcile", Intent::Reconcile))
            .await,
    );
    assert_eq!(reconciled["invalid_workspaces"], 0);
    assert_eq!(workspace_state(&restarted, &opened), "suspended");
    assert_eq!(session_state(&restarted, &opened.session), "suspended");

    let collected = collect(&restarted, "gc-after-restart").await;
    assert_eq!(collected["deleted"], 0);
    let woken: OpenedSession =
        completed(wake(&restarted, "wake-after-restart", &opened.session).await);
    assert!(PathBuf::from(&woken.cwd).join("tracked.txt").is_file());
    drop(restarted);

    // The predecessor a wake released kept the suspension's missing tree, so
    // the same skip has to cover it: otherwise the first restart after every
    // wake reports corruption and the successor's own history is collected.
    let after_wake = engine_with(directory.path(), Vec::new());
    let reconciled: serde_json::Value = completed(
        after_wake
            .execute(system("reconcile-woken", Intent::Reconcile))
            .await,
    );
    assert_eq!(reconciled["invalid_workspaces"], 0);
    assert_eq!(workspace_state(&after_wake, &opened), "released");
    assert_eq!(session_state(&after_wake, &opened.session), "active");
    assert!(PathBuf::from(&woken.cwd).join("tracked.txt").is_file());
}

#[tokio::test]
async fn suspended_workspace_is_not_a_gc_candidate_and_released_one_is() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened, _) = suspended_session(directory.path(), Vec::new()).await;

    let collected = collect(&engine, "gc-suspended").await;
    assert_eq!(collected["eligible"], 0);
    assert_eq!(collected["deleted"], 0);
    assert_eq!(workspace_state(&engine, &opened), "suspended");

    // Release is still the only door to deletion, and it works without a tree.
    let released: serde_json::Value = completed(
        engine
            .execute(execute(
                "release-suspended",
                Intent::WorkspaceRelease {
                    selector: selector(&opened),
                },
            ))
            .await,
    );
    assert_eq!(released["released"], true);
    assert_eq!(workspace_state(&engine, &opened), "released");

    let collected = collect(&engine, "gc-released").await;
    assert_eq!(collected["deleted"], 1);
    assert!(
        engine
            .database()
            .workspace(&opened.workspace)
            .unwrap()
            .is_none()
    );
    assert!(
        !directory
            .path()
            .join("state/secrets")
            .join(&opened.workspace.0)
            .exists(),
        "collecting the record reclaims the suspension vault with it"
    );
}

#[tokio::test]
async fn auto_sleep_and_suspended_retention_are_off_by_default_and_fire_when_configured() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened) = dormant_session(directory.path()).await;

    let swept: serde_json::Value = completed(
        engine
            .execute(system("sweep-default", Intent::MaintenanceSweep))
            .await,
    );
    assert_eq!(swept["auto_slept"], 0);
    assert_eq!(swept["retention_released"], 0);
    assert_eq!(workspace_state(&engine, &opened), "dormant");
    drop(engine);

    let sleeper = Engine::with_components(
        EngineConfig::at(directory.path().join("state"))
            .with_harness_lifecycle_timing(1, 0)
            .unwrap()
            .with_auto_sleep_after_secs(3600)
            .unwrap(),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap();
    backdate(directory.path(), &opened.workspace, 7_200_000);
    let swept: serde_json::Value = completed(
        sleeper
            .execute(system("sweep-auto-sleep", Intent::MaintenanceSweep))
            .await,
    );
    assert_eq!(swept["auto_slept"], 1);
    assert_eq!(workspace_state(&sleeper, &opened), "suspended");
    assert!(!PathBuf::from(&opened.cwd).exists());
    drop(sleeper);

    let retirer = Engine::with_components(
        EngineConfig::at(directory.path().join("state"))
            .with_harness_lifecycle_timing(1, 0)
            .unwrap()
            .with_suspended_retention_secs(3600)
            .unwrap(),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap();
    backdate(directory.path(), &opened.workspace, 7_200_000);
    let swept: serde_json::Value = completed(
        retirer
            .execute(system("sweep-retention", Intent::MaintenanceSweep))
            .await,
    );
    assert_eq!(swept["retention_released"], 1);
    assert_eq!(
        workspace_state(&retirer, &opened),
        "released",
        "retention releases; only GC ever deletes"
    );
}
