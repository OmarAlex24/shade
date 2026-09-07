use shade_engine::Engine;
use shade_engine::config::EngineConfig;
use shade_engine::dependencies::DependencyService;
use shade_engine::filesystem::CopyFilesystem;
use shade_protocol::{
    Actor, ActorKind, ExecuteRequest, Intent, OpenSession, OpenedSession, Outcome,
    PROTOCOL_VERSION, RepositoryLocator, ResponseBody, SessionId, WorkspaceSelector,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

struct PausedDependencyPromotion {
    promoted: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl shade_engine::dependencies::DependencyProvider for PausedDependencyPromotion {
    fn name(&self) -> &'static str {
        "fixture-promotion"
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
        let fingerprint = "a".repeat(64);
        let artifact = context
            .cache_root
            .join("dependencies/artifacts/npm")
            .join(&fingerprint);
        fs::create_dir_all(&artifact).unwrap();
        fs::write(
            artifact.join(".shade-dependency-artifact"),
            "shade-dependency-artifact/v1\n",
        )
        .unwrap();
        fs::write(artifact.join("receipt.json"), serde_json::to_vec(&serde_json::json!({"schema_version":1,"provider":"npm","fingerprint":fingerprint,"state":"ready","materialized_paths":[],"blocked_builds":[],"tools":[],"platform":"fixture"})).unwrap()).unwrap();
        // Logical pressure crosses the production LRU ceiling without allocating
        // twenty GiB of test data. No payload digest is bypassed by the provider.
        fs::File::create(artifact.join("pressure"))
            .unwrap()
            .set_len(shade_engine::dependencies::DEFAULT_DEPENDENCY_CACHE_BYTES + 1)
            .unwrap();
        self.promoted.notify_one();
        self.resume.notified().await;
        Ok(shade_engine::dependencies::DependencyReceipt {
            provider: "npm".into(),
            fingerprint,
            state: "ready".into(),
            materialized_paths: vec![],
            blocked_builds: vec![],
            scripts: vec![],
        })
    }
}

#[tokio::test]
async fn lru_cannot_delete_a_promoted_layer_before_its_workspace_receipt_is_recorded() {
    let temporary = tempfile::tempdir().unwrap();
    let repository = fixture(temporary.path());
    let promoted = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let engine = Engine::with_components(
        EngineConfig::at(temporary.path().join("state")),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(vec![Box::new(
            PausedDependencyPromotion {
                promoted: promoted.clone(),
                resume: resume.clone(),
            },
        )])),
    )
    .unwrap();
    let opener = engine.clone();
    let open = tokio::spawn(async move {
        opener
            .execute(execute(
                "open",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("promotion".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await
    });
    promoted.notified().await;
    let collector = engine.clone();
    let mut gc = tokio::spawn(async move {
        collector
            .execute(execute("gc-during-promotion", Intent::GarbageCollect))
            .await
    });
    // Keep the provider between promotion and receipt persistence while GC runs.
    // A correctly coordinated collector can wait; its eventual result, not its
    // duration, determines whether the live layer remained protected.
    let early = tokio::time::timeout(Duration::from_secs(1), &mut gc)
        .await
        .ok();
    resume.notify_one();
    let opened: OpenedSession = completed(open.await.unwrap());
    let result: serde_json::Value = completed(match early {
        Some(result) => result.unwrap(),
        None => gc.await.unwrap(),
    });
    assert_eq!(
        result["layers_deleted"], 0,
        "GC deleted a layer during an active preparation"
    );
    assert_eq!(
        engine
            .database()
            .dependency_receipts(&opened.workspace)
            .unwrap()
            .len(),
        1
    );
    assert!(
        temporary
            .path()
            .join("state/dependencies/artifacts/npm")
            .join("a".repeat(64))
            .join("receipt.json")
            .is_file(),
        "receipt points at a deleted artifact"
    );
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
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
            id: "gc-test-actor".into(),
        },
        intent,
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

async fn released_dirty_workspace(root: &Path) -> (Engine, OpenedSession, PathBuf, PathBuf) {
    let repository = fixture(root);
    let config = EngineConfig::at(root.join("state"))
        .with_harness_lifecycle_timing(120, 0)
        .unwrap();
    let engine = Engine::with_components(
        config,
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap();
    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "open",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("gc-test-session".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    let workspace_path = PathBuf::from(&opened.cwd);
    let workspace = engine
        .database()
        .workspace(&opened.workspace)
        .unwrap()
        .unwrap();
    let managed = engine
        .database()
        .repository_by_id(&workspace.repository_id)
        .unwrap()
        .unwrap()
        .bare_path;
    let _: serde_json::Value = completed(
        engine
            .execute(execute(
                "release",
                Intent::WorkspaceRelease {
                    selector: WorkspaceSelector {
                        workspace_id: Some(opened.workspace.clone()),
                        cwd: None,
                    },
                },
            ))
            .await,
    );
    fs::write(
        workspace_path.join("late-untracked.txt"),
        "must survive checkpoint\n",
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(2));
    (engine, opened, workspace_path, managed)
}

fn checkpoint_event_count(engine: &Engine, workspace: &str) -> usize {
    engine
        .events(0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.event == "checkpoint.created"
                && event.payload["workspace"].as_str() == Some(workspace)
        })
        .count()
}

#[tokio::test]
async fn cleanup_discovers_late_secrets_and_requires_an_actionable_review() {
    for (choice, retained) in [
        (shade_protocol::ReviewAction::Keep, true),
        (shade_protocol::ReviewAction::Discard, false),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (engine, opened, workspace_path, _) = released_dirty_workspace(directory.path()).await;
        fs::write(
            workspace_path.join(".env.local"),
            "TOKEN=private-fixture-value\n",
        )
        .unwrap();
        let result: serde_json::Value = completed(
            engine
                .execute(execute("gc-discover", Intent::GarbageCollect))
                .await,
        );
        assert_eq!(
            result["deleted"], 0,
            "unreviewed secrets must survive cleanup"
        );
        assert!(workspace_path.exists());
        let event = engine
            .events(0, 1000)
            .unwrap()
            .into_iter()
            .find(|event| event.event == "review.required")
            .unwrap();
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(
            encoded.contains("TOKEN"),
            "the event must contain the actionable key-only preview"
        );
        assert!(!encoded.contains("private-fixture-value"));
        let review_id = shade_protocol::ReviewId(event.payload["review"].as_str().unwrap().into());
        let _: serde_json::Value = completed(
            engine
                .execute(execute(
                    "review-choice",
                    Intent::ReviewResolve {
                        review_id,
                        action: choice,
                    },
                ))
                .await,
        );
        std::thread::sleep(Duration::from_millis(2));
        let _: serde_json::Value = completed(
            engine
                .execute(execute("gc-after-choice", Intent::GarbageCollect))
                .await,
        );
        assert_eq!(workspace_path.exists(), retained);
        if retained {
            assert_eq!(
                engine
                    .database()
                    .workspace(&opened.workspace)
                    .unwrap()
                    .unwrap()
                    .state,
                "retained"
            );
        }
    }
}

#[tokio::test]
async fn secret_decisions_remain_actionable_after_lease_expiry() {
    for action in [
        shade_protocol::ReviewAction::Keep,
        shade_protocol::ReviewAction::Discard,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (engine, _, _, _) = released_dirty_workspace(directory.path()).await;
        let opened: OpenedSession = completed(
            engine
                .execute(execute(
                    "open-expiring",
                    Intent::SessionOpen(OpenSession {
                        session_id: SessionId("expiring".into()),
                        repository: RepositoryLocator::Local {
                            path: directory
                                .path()
                                .join("source")
                                .to_string_lossy()
                                .into_owned(),
                        },
                        base: Some("main".into()),
                        intent: None,
                    }),
                ))
                .await,
        );
        fs::write(
            Path::new(&opened.cwd).join(".env.local"),
            "TOKEN=expired-fixture\n",
        )
        .unwrap();
        let response = engine
            .execute(execute(
                "review-expiring",
                Intent::WorkspaceRelease {
                    selector: WorkspaceSelector {
                        workspace_id: Some(opened.workspace.clone()),
                        cwd: None,
                    },
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
            .mark_expired_leases(shade_engine::db::now_ms() + 121_000)
            .unwrap();
        let expected = if matches!(action, shade_protocol::ReviewAction::Keep) {
            "retained"
        } else {
            "released"
        };
        let _: serde_json::Value = completed(
            engine
                .execute(execute(
                    "resolve-expired",
                    Intent::ReviewResolve { review_id, action },
                ))
                .await,
        );
        assert_eq!(
            engine
                .database()
                .workspace(&opened.workspace)
                .unwrap()
                .unwrap()
                .state,
            expected
        );
        assert!(
            engine
                .database()
                .active_lease_for_session(&opened.session)
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn dirty_eligible_workspace_is_checkpointed_once_and_deleted_in_the_same_gc() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened, workspace_path, _) = released_dirty_workspace(directory.path()).await;
    let checkpoints_before = checkpoint_event_count(&engine, &opened.workspace.0);

    let result: serde_json::Value =
        completed(engine.execute(execute("gc", Intent::GarbageCollect)).await);
    assert_eq!(result["eligible"].as_u64(), Some(1));
    assert_eq!(result["deleted"].as_u64(), Some(1));
    assert_eq!(result["skipped"].as_u64(), Some(0));
    assert!(
        engine
            .database()
            .workspace(&opened.workspace)
            .unwrap()
            .is_none()
    );
    assert!(!workspace_path.exists());
    assert_eq!(
        checkpoint_event_count(&engine, &opened.workspace.0),
        checkpoints_before + 1
    );
}

#[tokio::test]
async fn checkpoint_failure_keeps_the_dirty_workspace_and_its_content() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened, workspace_path, managed) =
        released_dirty_workspace(directory.path()).await;
    let ref_blocker = managed
        .join("refs/shade/workspaces")
        .join(&opened.workspace.0)
        .join("checkpoints");
    fs::create_dir_all(ref_blocker.parent().unwrap()).unwrap();
    fs::write(&ref_blocker, "checkpoint namespace intentionally blocked\n").unwrap();
    let checkpoints_before = checkpoint_event_count(&engine, &opened.workspace.0);

    let result: serde_json::Value =
        completed(engine.execute(execute("gc", Intent::GarbageCollect)).await);
    assert_eq!(result["eligible"].as_u64(), Some(1));
    assert_eq!(result["deleted"].as_u64(), Some(0));
    assert_eq!(result["skipped"].as_u64(), Some(1));
    assert_eq!(
        engine
            .database()
            .workspace(&opened.workspace)
            .unwrap()
            .unwrap()
            .state,
        "released"
    );
    assert_eq!(
        fs::read_to_string(workspace_path.join("late-untracked.txt")).unwrap(),
        "must survive checkpoint\n"
    );
    assert_eq!(
        checkpoint_event_count(&engine, &opened.workspace.0),
        checkpoints_before
    );
    assert!(!engine.events(0, 10_000).unwrap().into_iter().any(|event| {
        event.event == "workspace.deleted" && event.resource == opened.workspace.0
    }));
}

#[tokio::test]
async fn stale_secret_decisions_cannot_discard_later_edits() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened, path, _) = released_dirty_workspace(directory.path()).await;
    let secret = path.join(".env.local");
    fs::write(&secret, "TOKEN=first-private-value\n").unwrap();
    let _: serde_json::Value = completed(
        engine
            .execute(execute("discover-first", Intent::GarbageCollect))
            .await,
    );
    let first = engine
        .database()
        .latest_secret_review(&opened.workspace)
        .unwrap()
        .unwrap();
    fs::write(&secret, "TOKEN=second-private-value\n").unwrap();
    let response = engine
        .execute(execute(
            "resolve-stale",
            Intent::ReviewResolve {
                review_id: first.id.clone(),
                action: shade_protocol::ReviewAction::Discard,
            },
        ))
        .await;
    let second = match response.body {
        ResponseBody::Ok {
            outcome: Outcome::ReviewRequired(review),
        } => review.review_id,
        other => panic!("stale choice should return a new review: {other:?}"),
    };
    assert_ne!(first.id, second);
    assert_eq!(
        engine.database().review(&first.id).unwrap().unwrap().state,
        "superseded"
    );
    assert_eq!(
        fs::read_to_string(&secret).unwrap(),
        "TOKEN=second-private-value\n"
    );
    let _: serde_json::Value = completed(
        engine
            .execute(execute(
                "resolve-second",
                Intent::ReviewResolve {
                    review_id: second.clone(),
                    action: shade_protocol::ReviewAction::Discard,
                },
            ))
            .await,
    );
    // A decision is about those bytes, even when the key names do not change.
    fs::write(&secret, "TOKEN=third-private-value\n").unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    let gc: serde_json::Value = completed(
        engine
            .execute(execute("discover-after-decision", Intent::GarbageCollect))
            .await,
    );
    assert_eq!(gc["deleted"], 0);
    assert!(path.exists());
    let third = engine
        .database()
        .latest_secret_review(&opened.workspace)
        .unwrap()
        .unwrap();
    assert_ne!(second, third.id);
    assert_eq!(third.state, "pending");
    let events = serde_json::to_string(&engine.events(0, 1000).unwrap()).unwrap();
    for value in [
        "first-private-value",
        "second-private-value",
        "third-private-value",
    ] {
        assert!(!events.contains(value));
    }
    let _: serde_json::Value = completed(
        engine
            .execute(execute(
                "resolve-third",
                Intent::ReviewResolve {
                    review_id: third.id,
                    action: shade_protocol::ReviewAction::Discard,
                },
            ))
            .await,
    );
    tokio::time::sleep(Duration::from_millis(2)).await;
    let _: serde_json::Value = completed(
        engine
            .execute(execute("collect-reviewed", Intent::GarbageCollect))
            .await,
    );
    assert!(!path.exists());
    assert!(
        !directory
            .path()
            .join("state/secrets")
            .join(format!("reviews-{}", opened.workspace.0))
            .exists()
    );
}

/// A provider that refuses preparation the way a stale lockfile does.
///
/// `open` and successor materialization both prepare dependencies before
/// capturing the workspace's secret baseline, so a refusal here reproduces the
/// exact shape a real `DEPENDENCY_LOCK_STALE` leaves behind: a `failed`
/// workspace with a registered worktree, a clean tree at its base commit and
/// no baseline of its own.
struct RefusingDependencies {
    ready: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl shade_engine::dependencies::DependencyProvider for RefusingDependencies {
    fn name(&self) -> &'static str {
        "fixture-lock"
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
        if !self.ready.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(shade_engine::dependencies::DependencyError::LockStale(
                "fixture lockfile does not match its manifest".into(),
            ));
        }
        Ok(shade_engine::dependencies::DependencyReceipt {
            provider: "fixture-lock".into(),
            fingerprint: "b".repeat(64),
            state: "ready".into(),
            materialized_paths: vec![],
            blocked_builds: vec![],
            scripts: vec![],
        })
    }
}

/// A repository whose committed source matches the local secret detector.
///
/// This is the ordinary shape, not a contrived one: a client that names its
/// bearer token in a literal. `survey_tracked_secrets` records such a base and
/// admits it -- committed content is the repository owner's decision -- so
/// nothing downstream may treat it as private material the workspace produced.
fn fixture_with_committed_credential(root: &Path) -> PathBuf {
    let repository = fixture(root);
    fs::write(
        repository.join("client.rs"),
        "let token = \"7fbz3qkxWr92Ldv6\";\n",
    )
    .unwrap();
    git(&repository, &["add", "client.rs"]);
    git(&repository, &["commit", "-m", "committed credential"]);
    repository
}

fn engine_with(root: &Path, ready: &Arc<std::sync::atomic::AtomicBool>) -> Engine {
    Engine::with_components(
        EngineConfig::at(root.join("state"))
            .with_harness_lifecycle_timing(120, 0)
            .unwrap(),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(vec![Box::new(
            RefusingDependencies {
                ready: Arc::clone(ready),
            },
        )])),
    )
    .unwrap()
}

fn failed_workspaces(engine: &Engine) -> Vec<shade_engine::db::WorkspaceRecord> {
    engine
        .database()
        .workspaces_in_state_before("failed", i64::MAX)
        .unwrap()
}

fn open_intent(session: &str, repository: &Path) -> Intent {
    Intent::SessionOpen(OpenSession {
        session_id: SessionId(session.into()),
        repository: RepositoryLocator::Local {
            path: repository.to_string_lossy().into_owned(),
        },
        base: Some("main".into()),
        intent: None,
    })
}

/// A dependency failure leaves a workspace GC has to be able to reclaim.
///
/// `open` refuses a stale lockfile after the worktree is registered and before
/// the secret baseline is captured, so the tree it abandons is a clean
/// checkout of the base commit and nothing more. GC kept every one of them:
/// with no baseline to compare against, every committed file the detector
/// recognizes read as newly added private material, and the collector opened
/// a `secret_cleanup` review for a tree holding only committed source.
#[tokio::test]
async fn a_failed_dependency_open_is_reclaimed() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture_with_committed_credential(directory.path());
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let engine = engine_with(directory.path(), &ready);

    let response = engine
        .execute(execute(
            "open-stale",
            open_intent("stale-lock", &repository),
        ))
        .await;
    let ResponseBody::Error { error } = response.body else {
        panic!("a stale lockfile must refuse the open");
    };
    assert_eq!(error.code, "DEPENDENCY_LOCK_STALE");

    let [workspace] = failed_workspaces(&engine).try_into().unwrap();
    assert!(workspace.path.join(".git").is_file());
    assert!(workspace.path.join("client.rs").is_file());
    std::thread::sleep(Duration::from_millis(2));

    let result: serde_json::Value =
        completed(engine.execute(execute("gc", Intent::GarbageCollect)).await);
    assert_eq!(result["eligible"].as_u64(), Some(1));
    assert_eq!(result["deleted"].as_u64(), Some(1), "{result}");
    assert!(!workspace.path.exists(), "the tree survived collection");
    assert!(
        engine
            .database()
            .workspace(&workspace.id)
            .unwrap()
            .is_none(),
        "the record survived collection"
    );
    assert!(
        engine
            .database()
            .latest_secret_review(&workspace.id)
            .unwrap()
            .is_none(),
        "committed content asked a human for a decision"
    );
    let registrations = engine
        .database()
        .repository_by_id(&workspace.repository_id)
        .unwrap()
        .unwrap()
        .bare_path
        .join("worktrees");
    assert!(
        !registrations.join(workspace.id.0.as_str()).exists(),
        "the worktree registration survived collection"
    );
}

/// A wake that fails after restoring the vault leaves private files behind.
///
/// The successor holds a copy of what the predecessor vaulted, and the vault
/// is the original: `restore_suspension` reads it and the failed wake leaves
/// the suspension whole, which is why a second wake works. Collecting the
/// successor therefore loses nothing -- but GC kept it, for the same missing
/// baseline as above.
#[tokio::test]
async fn a_failed_wake_successor_is_reclaimed_while_its_vault_stays_whole() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture_with_committed_credential(directory.path());
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let engine = engine_with(directory.path(), &ready);

    let opened: OpenedSession = completed(
        engine
            .execute(execute("open-wake", open_intent("waking", &repository)))
            .await,
    );
    let workspace_path = PathBuf::from(&opened.cwd);
    fs::write(workspace_path.join(".env.local"), "TOKEN=vaulted-value\n").unwrap();
    let _: serde_json::Value = completed(
        engine
            .execute(execute(
                "sleep",
                Intent::WorkspaceSleep {
                    selector: WorkspaceSelector {
                        workspace_id: Some(opened.workspace.clone()),
                        cwd: None,
                    },
                },
            ))
            .await,
    );
    let vault = directory
        .path()
        .join("state/secrets")
        .join(&opened.workspace.0)
        .join("suspended");
    assert!(vault.join("manifest.json").is_file());

    ready.store(false, std::sync::atomic::Ordering::SeqCst);
    let response = engine
        .execute(execute(
            "wake",
            Intent::SessionWake {
                session_id: opened.session.clone(),
            },
        ))
        .await;
    let ResponseBody::Error { error } = response.body else {
        panic!("a stale lockfile must refuse the wake");
    };
    assert_eq!(error.code, "DEPENDENCY_LOCK_STALE");

    let [successor] = failed_workspaces(&engine).try_into().unwrap();
    assert_eq!(successor.predecessor_id.as_ref(), Some(&opened.workspace));
    assert_eq!(
        fs::read_to_string(successor.path.join(".env.local")).unwrap(),
        "TOKEN=vaulted-value\n",
        "the successor must be holding the restored vault"
    );
    std::thread::sleep(Duration::from_millis(2));

    let result: serde_json::Value =
        completed(engine.execute(execute("gc", Intent::GarbageCollect)).await);
    assert_eq!(result["deleted"].as_u64(), Some(1), "{result}");
    assert!(!successor.path.exists());
    assert!(
        engine
            .database()
            .workspace(&successor.id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fs::read_to_string(vault.join("files/.env.local")).unwrap(),
        "TOKEN=vaulted-value\n",
        "collecting the successor took the vault with it"
    );
    assert_eq!(
        engine
            .database()
            .workspace(&opened.workspace)
            .unwrap()
            .unwrap()
            .state,
        "suspended",
        "the suspension must survive its successor"
    );
}

/// A private file nothing else holds still stops the collector.
///
/// The reclaim above is licensed by preservation, not by the `failed` state:
/// bytes that are neither committed content nor a copy of something the
/// predecessor still holds are the workspace's only copy, and they get the
/// same actionable review any other undecided secret gets.
#[tokio::test]
async fn a_failed_workspace_keeps_a_private_file_nothing_else_holds() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture_with_committed_credential(directory.path());
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let engine = engine_with(directory.path(), &ready);
    let _ = engine
        .execute(execute(
            "open-stale",
            open_intent("stale-lock", &repository),
        ))
        .await;
    let [workspace] = failed_workspaces(&engine).try_into().unwrap();
    fs::write(workspace.path.join(".env.local"), "TOKEN=only-copy\n").unwrap();
    std::thread::sleep(Duration::from_millis(2));

    let result: serde_json::Value =
        completed(engine.execute(execute("gc", Intent::GarbageCollect)).await);
    assert_eq!(result["deleted"].as_u64(), Some(0), "{result}");
    assert_eq!(result["skipped"].as_u64(), Some(1));
    assert_eq!(
        fs::read_to_string(workspace.path.join(".env.local")).unwrap(),
        "TOKEN=only-copy\n"
    );
    let review = engine
        .database()
        .latest_secret_review(&workspace.id)
        .unwrap()
        .expect("an unpreserved private file must ask for a decision");
    assert_eq!(review.state, "pending");
    // The review takes the workspace out of GC candidacy until a person
    // answers it, which is disk nothing reclaims on its own. `doctor` names it.
    let health = engine.database().doctor().unwrap();
    assert_eq!(health["workspaces_failed"], 1);
    assert_eq!(health["workspaces_failed_awaiting_review"], 1);
    let again: serde_json::Value = completed(
        engine
            .execute(execute("gc-again", Intent::GarbageCollect))
            .await,
    );
    assert_eq!(again["eligible"].as_u64(), Some(0), "{again}");
}
