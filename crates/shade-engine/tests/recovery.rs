use shade_engine::Engine;
use shade_engine::config::EngineConfig;
use shade_engine::db::{BeginOperation, CheckpointRecord, PublishIntentRecord, WorkspaceRecord};
use shade_engine::dependencies::DependencyService;
use shade_engine::filesystem::CopyFilesystem;
use shade_engine::git::{
    BaseRevision, GitStore, ManagedRepository, Oid, PrepareSquashPublishRequest,
};
use shade_protocol::{
    Actor, ActorKind, CheckpointId, ExecuteRequest, Intent, ObjectId, OpenSession, OpenedSession,
    OperationId, Outcome, PROTOCOL_VERSION, Query, QueryRequest, RepositoryLocator, ResponseBody,
    SessionId, WorkspaceId, WorkspaceSelector,
};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn git(cwd: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn fixture(root: &Path) -> PathBuf {
    let repository = root.join("source");
    fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init", "-b", "main"]);
    git(&repository, &["config", "user.name", "Shade Recovery"]);
    git(
        &repository,
        &["config", "user.email", "recovery@shade.invalid"],
    );
    fs::write(repository.join("tracked.txt"), "base\n").unwrap();
    git(&repository, &["add", "tracked.txt"]);
    git(&repository, &["commit", "-m", "base"]);
    repository
}

fn test_engine(root: &Path) -> Engine {
    Engine::with_components(
        EngineConfig::at(root.join("state")),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap()
}

fn execute(key: &str, intent: Intent) -> ExecuteRequest {
    ExecuteRequest {
        v: PROTOCOL_VERSION,
        request_id: format!("request-{key}"),
        idempotency_key: key.to_owned(),
        actor: Actor {
            kind: ActorKind::System,
            id: "recovery-test".into(),
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

fn new_workspace_id() -> WorkspaceId {
    WorkspaceId(format!("ws_{}", ulid::Ulid::new()))
}

fn begin_interrupted_operation(
    engine: &Engine,
    workspace: &WorkspaceId,
    suffix: &str,
) -> OperationId {
    let operation = match engine
        .database()
        .begin_operation("crashed-daemon", suffix, suffix, "session_open")
        .unwrap()
    {
        BeginOperation::New(operation) => operation,
        BeginOperation::Existing(_) => unreachable!(),
    };
    engine
        .database()
        .bind_operation_resource(&operation, &workspace.0, suffix)
        .unwrap();
    operation
}

#[tokio::test]
async fn startup_reconcile_converges_every_durable_phase_without_touching_valid_state() {
    let directory = tempfile::tempdir().unwrap();
    let source = fixture(directory.path());
    let engine = test_engine(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "open-valid",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("valid-session".into()),
                    repository: RepositoryLocator::Local {
                        path: source.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    let checkpoint: serde_json::Value = completed(
        engine
            .execute(execute(
                "checkpoint-valid",
                Intent::WorkspaceCheckpoint {
                    selector: WorkspaceSelector {
                        workspace_id: Some(opened.workspace.clone()),
                        cwd: None,
                    },
                    reason: "preserve-through-recovery".into(),
                },
            ))
            .await,
    );
    let valid_checkpoint = CheckpointId(checkpoint["checkpoint_id"].as_str().unwrap().to_owned());
    let valid_workspace = engine
        .database()
        .workspace(&opened.workspace)
        .unwrap()
        .unwrap();
    let repository = engine
        .database()
        .repository_by_id(&valid_workspace.repository_id)
        .unwrap()
        .unwrap();
    let managed = ManagedRepository::new(repository.bare_path.clone());
    let store = GitStore::system();
    let commit = Oid::new(valid_workspace.head_oid.0.clone()).unwrap();
    let tree = Oid::new(
        store
            .run(
                None,
                [
                    format!("--git-dir={}", repository.bare_path.display()),
                    "rev-parse".to_owned(),
                    format!("{}^{{tree}}", commit),
                ],
            )
            .await
            .unwrap()
            .stdout,
    )
    .unwrap();
    let base = BaseRevision {
        commit: commit.clone(),
        tree,
        source_ref: None,
    };

    let mut crash_workspaces = Vec::new();
    let mut interrupted_operations = Vec::new();
    for (phase, state) in [
        ("sqlite", "materializing"),
        ("filesystem", "materializing"),
        ("worktree", "deleting"),
        ("checkpoint", "recovering"),
    ] {
        let id = new_workspace_id();
        let path = engine.config().workspaces_dir().join(&id.0);
        let record = WorkspaceRecord {
            id: id.clone(),
            repository_id: repository.id.clone(),
            session_id: None,
            path: path.clone(),
            base_ref: "origin/main".into(),
            base_oid: ObjectId(commit.to_string()),
            head_oid: ObjectId(commit.to_string()),
            state: state.into(),
            predecessor_id: None,
            dependency_state: "preparing".into(),
        };
        engine.database().create_workspace(&record).unwrap();
        interrupted_operations.push(begin_interrupted_operation(&engine, &id, phase));

        if phase != "sqlite" {
            store
                .materialize_tree(&managed, &base.tree, &path)
                .await
                .unwrap();
        }
        if matches!(phase, "worktree" | "checkpoint") {
            store
                .register_precloned_worktree(&managed, &path, &base, phase)
                .await
                .unwrap();
        }
        if phase == "checkpoint" {
            let checkpoint_id = CheckpointId(format!("ckpt_{}", ulid::Ulid::new()));
            let captured = store
                .checkpoint(&managed, &path, &id.0, &checkpoint_id.0)
                .await
                .unwrap();
            engine
                .database()
                .create_checkpoint(&CheckpointRecord {
                    id: checkpoint_id,
                    workspace_id: id.clone(),
                    head_oid: ObjectId(captured.head.to_string()),
                    index_oid: ObjectId(captured.index_tree.to_string()),
                    worktree_oid: ObjectId(captured.working_tree.to_string()),
                    reason: "crash-after-checkpoint".into(),
                    state: "ready".into(),
                })
                .unwrap();
        }
        crash_workspaces.push((id, path));
    }

    // Registration exists but its directory vanished before SQLite learned it.
    let missing_registration = engine.config().workspaces_dir().join(new_workspace_id().0);
    store
        .materialize_tree(&managed, &base.tree, &missing_registration)
        .await
        .unwrap();
    store
        .register_precloned_worktree(
            &managed,
            &missing_registration,
            &base,
            "crash-before-sqlite",
        )
        .await
        .unwrap();
    fs::remove_dir_all(&missing_registration).unwrap();

    // One orphan checkpoint group shares a valid workspace namespace. The
    // durable checkpoint next to it must survive.
    let orphan_checkpoint = format!("ckpt_{}", ulid::Ulid::new());
    let orphan_ref = format!(
        "refs/shade/workspaces/{}/checkpoints/{orphan_checkpoint}/head",
        opened.workspace.0
    );
    store
        .run(
            None,
            [
                format!("--git-dir={}", repository.bare_path.display()),
                "update-ref".to_owned(),
                orphan_ref.clone(),
                commit.to_string(),
            ],
        )
        .await
        .unwrap();
    store
        .run(
            None,
            [
                format!("--git-dir={}", repository.bare_path.display()),
                "update-ref".to_owned(),
                "refs/shade/unrelated".to_owned(),
                commit.to_string(),
            ],
        )
        .await
        .unwrap();

    let repository_stage = engine.config().repositories_dir().join(".shade-git-crash");
    let base_stage = engine
        .config()
        .bases_dir()
        .join(&repository.id.0)
        .join(".shade-materialize-crash");
    let workspace_stage = engine
        .config()
        .workspaces_dir()
        .join(".shade-copy-fake-crash");
    let dependency_stage = engine.config().dependencies_dir().join("staging/crash");
    let secret_stage = engine.config().secrets_dir().join(".ws_crash.staging");
    for stage in [
        &repository_stage,
        &base_stage,
        &workspace_stage,
        &dependency_stage,
        &secret_stage,
    ] {
        fs::create_dir_all(stage).unwrap();
    }
    let external_stage = directory.path().join("must-not-delete");
    fs::create_dir(&external_stage).unwrap();
    fs::write(external_stage.join("sentinel"), "preserved\n").unwrap();
    let staging_symlink = engine
        .config()
        .workspaces_dir()
        .join(".shade-copy-fake-symlink");
    symlink(&external_stage, &staging_symlink).unwrap();

    let recovery: serde_json::Value = completed(
        engine
            .execute(execute("startup-reconcile", Intent::Reconcile))
            .await,
    );
    assert_eq!(recovery["interrupted_operations"], 4);
    assert_eq!(recovery["incomplete_removed"], 4);
    assert_eq!(recovery["incomplete_failed"], 0);
    assert_eq!(recovery["worktree_metadata_removed"], 3);
    assert_eq!(recovery["checkpoint_refs_removed"], 2);
    assert_eq!(recovery["staging_removed"], 5);

    for (id, path) in &crash_workspaces {
        assert!(engine.database().workspace(id).unwrap().is_none());
        assert!(!path.exists());
    }
    for operation in interrupted_operations {
        assert_eq!(
            engine
                .database()
                .operation(&operation)
                .unwrap()
                .unwrap()
                .state,
            "failed"
        );
    }
    assert!(!missing_registration.exists());
    for stage in [
        repository_stage,
        base_stage,
        workspace_stage,
        dependency_stage,
        secret_stage,
    ] {
        assert!(!stage.exists());
    }
    assert!(staging_symlink.is_symlink());
    assert_eq!(
        fs::read_to_string(external_stage.join("sentinel")).unwrap(),
        "preserved\n"
    );

    let registered = store.registered_worktrees(&managed).await.unwrap();
    let valid_cwd = fs::canonicalize(&opened.cwd).unwrap();
    assert!(registered.contains(&valid_cwd));
    assert!(!registered.contains(&missing_registration));
    let canonical_workspace_root = fs::canonicalize(engine.config().workspaces_dir()).unwrap();
    assert_eq!(
        registered
            .iter()
            .filter(|path| path.starts_with(&canonical_workspace_root))
            .collect::<Vec<_>>(),
        vec![&valid_cwd]
    );
    let private_keys = store.private_checkpoint_keys(&managed).await.unwrap();
    assert!(private_keys.contains(&(opened.workspace.0.clone(), valid_checkpoint.0.clone())));
    assert!(!private_keys.contains(&(opened.workspace.0.clone(), orphan_checkpoint)));
    assert_eq!(private_keys.len(), 1);
    store
        .run(
            None,
            [
                format!("--git-dir={}", repository.bare_path.display()),
                "rev-parse".to_owned(),
                "--verify".to_owned(),
                "refs/shade/unrelated".to_owned(),
            ],
        )
        .await
        .unwrap();

    let context = engine
        .query(QueryRequest {
            v: PROTOCOL_VERSION,
            request_id: "valid-context".into(),
            query: Query::Context {
                selector: WorkspaceSelector {
                    workspace_id: Some(opened.workspace),
                    cwd: None,
                },
            },
        })
        .await;
    assert!(matches!(
        context.body,
        ResponseBody::Ok {
            outcome: Outcome::Completed(_)
        }
    ));

    let second: serde_json::Value = completed(
        engine
            .execute(execute("startup-reconcile-again", Intent::Reconcile))
            .await,
    );
    assert_eq!(second["interrupted_operations"], 0);
    assert_eq!(second["incomplete_removed"], 0);
    assert_eq!(second["worktree_metadata_removed"], 0);
    assert_eq!(second["checkpoint_refs_removed"], 0);
    assert_eq!(second["staging_removed"], 0);
}

#[tokio::test]
async fn startup_reconcile_never_removes_an_incomplete_workspace_with_a_live_lease() {
    let directory = tempfile::tempdir().unwrap();
    let source = fixture(directory.path());
    let engine = test_engine(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "open-live",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("live-session".into()),
                    repository: RepositoryLocator::Local {
                        path: source.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    engine
        .database()
        .mark_workspace_state(&opened.workspace, "materializing")
        .unwrap();

    let recovery: serde_json::Value = completed(
        engine
            .execute(execute("reconcile-live", Intent::Reconcile))
            .await,
    );
    assert_eq!(recovery["incomplete_removed"], 0);
    assert_eq!(recovery["incomplete_failed"], 1);
    assert_eq!(recovery["worktree_metadata_removed"], 0);
    assert!(Path::new(&opened.cwd).join(".git").is_file());
    assert!(
        engine
            .database()
            .workspace(&opened.workspace)
            .unwrap()
            .is_some()
    );
    assert!(
        engine
            .database()
            .active_lease_for_session(&opened.session)
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn startup_replays_publish_crashes_before_and_after_each_local_and_remote_cas() {
    let directory = tempfile::tempdir().unwrap();
    let source = fixture(directory.path());
    let engine = test_engine(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "publish-open",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("publish-recovery-session".into()),
                    repository: RepositoryLocator::Local {
                        path: source.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    fs::write(
        Path::new(&opened.cwd).join("durable.txt"),
        "durable publish\n",
    )
    .unwrap();
    let checkpoint_value: serde_json::Value = completed(
        engine
            .execute(execute(
                "publish-checkpoint",
                Intent::WorkspaceCheckpoint {
                    selector: WorkspaceSelector {
                        workspace_id: Some(opened.workspace.clone()),
                        cwd: None,
                    },
                    reason: "publish recovery fixture".into(),
                },
            ))
            .await,
    );
    let checkpoint_id = CheckpointId(
        checkpoint_value["checkpoint_id"]
            .as_str()
            .unwrap()
            .to_owned(),
    );
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
    let managed = ManagedRepository::new(repository.bare_path.clone());
    let store = GitStore::system();
    let remote = store.canonicalize_remote(&repository.identity).unwrap();
    let checkpoint = store
        .load_checkpoint(&managed, &workspace.id.0, &checkpoint_id.0)
        .await
        .unwrap();
    let original_base = Oid::new(workspace.base_oid.0.clone()).unwrap();

    for (phase, push) in [
        ("planned", false),
        ("anchored", false),
        ("local_cas", false),
        ("remote_cas", true),
    ] {
        let operation = match engine
            .database()
            .begin_operation(
                "system\0crashed-publisher",
                phase,
                phase,
                "workspace_publish",
            )
            .unwrap()
        {
            BeginOperation::New(operation) => operation,
            BeginOperation::Existing(_) => unreachable!(),
        };
        let branch = format!("recover-{phase}");
        let anchor = format!("refs/shade/operations/{}/publish", operation.0);
        engine
            .database()
            .create_publish_intent(&PublishIntentRecord {
                operation_id: operation.clone(),
                workspace_id: workspace.id.clone(),
                repository_id: repository.id.clone(),
                checkpoint_id: checkpoint_id.clone(),
                source_kind: "workspace_publish".into(),
                branch: branch.clone(),
                message: format!("recover {phase}"),
                push,
                original_base_oid: ObjectId(original_base.to_string()),
                expected_remote_oid: None,
                expected_local_oid: None,
                anchor_ref: anchor.clone(),
                commit_oid: None,
                tree_oid: None,
                state: "planned".into(),
                anchor_cleaned: false,
            })
            .unwrap();

        let prepared = if phase == "planned" {
            None
        } else {
            Some(
                store
                    .prepare_squash_publish(
                        &managed,
                        PrepareSquashPublishRequest {
                            remote: &remote,
                            branch: &branch,
                            original_base: &original_base,
                            expected_remote: None,
                            checkpoint: &checkpoint,
                            message: &format!("recover {phase}"),
                            anchor_ref: &anchor,
                        },
                    )
                    .await
                    .unwrap(),
            )
        };
        if matches!(phase, "local_cas" | "remote_cas") {
            let prepared = prepared.as_ref().unwrap();
            engine
                .database()
                .mark_publish_prepared(
                    &operation,
                    &ObjectId(prepared.commit.to_string()),
                    &ObjectId(prepared.tree.to_string()),
                )
                .unwrap();
            store
                .apply_prepared_publish_local(&managed, prepared, None)
                .await
                .unwrap();
        }
        if phase == "remote_cas" {
            let prepared = prepared.as_ref().unwrap();
            engine
                .database()
                .mark_publish_local_applied(&operation)
                .unwrap();
            store
                .apply_prepared_publish_remote(&managed, &remote, &branch, prepared, None)
                .await
                .unwrap();
        }

        let recovery: serde_json::Value = completed(
            engine
                .execute(execute(&format!("reconcile-{phase}"), Intent::Reconcile))
                .await,
        );
        assert_eq!(recovery["publishes_failed"], 0);
        let recovered = engine.database().operation(&operation).unwrap().unwrap();
        assert_eq!(recovered.state, "completed", "phase {phase}");
        let outcome = recovered.outcome.unwrap();
        let commit = match outcome {
            Outcome::Completed(value) => value["commit"].as_str().unwrap().to_owned(),
            other => panic!("unexpected recovered outcome for {phase}: {other:?}"),
        };
        assert_eq!(
            store
                .local_branch_oid(&managed, &branch)
                .await
                .unwrap()
                .unwrap()
                .as_str(),
            commit
        );
        assert_eq!(
            store.publish_anchor_oid(&managed, &anchor).await.unwrap(),
            None,
            "phase {phase} must release its private anchor only after completion"
        );
        if push {
            assert_eq!(
                store
                    .remote_branch_oid(&remote, &branch)
                    .await
                    .unwrap()
                    .unwrap()
                    .as_str(),
                commit
            );
        }
        let completion_events = engine
            .database()
            .events(0, 1_000)
            .unwrap()
            .into_iter()
            .filter(|event| event.event == "operation.completed" && event.resource == operation.0)
            .count();
        assert_eq!(completion_events, 1, "phase {phase}");
    }

    let second: serde_json::Value = completed(
        engine
            .execute(execute("reconcile-publish-again", Intent::Reconcile))
            .await,
    );
    assert_eq!(second["publishes_failed"], 0);
    assert_eq!(second["publishes_pending"], 0);
}
