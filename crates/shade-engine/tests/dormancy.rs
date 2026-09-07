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
    SessionStatus, ShadeError, WorkspaceSelector,
};
use std::fs;
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

async fn collect(engine: &Engine, key: &str) -> serde_json::Value {
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

#[tokio::test]
async fn reattach_of_an_active_session_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_at(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(&repository, SESSION, None)))
            .await,
    );

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
    assert_eq!(
        engine
            .database()
            .active_lease_for_session(&opened.session)
            .unwrap()
            .unwrap()
            .fence,
        1
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
