use shade_engine::Engine;
use shade_engine::config::EngineConfig;
use shade_engine::dependencies::DependencyService;
use shade_engine::filesystem::{CopyFilesystem, CowUnavailable, Usage, WorkspaceFilesystem};
use shade_engine::git::{GitCounters, GitStore};
use shade_protocol::{
    Actor, ActorKind, CheckpointId, ExecuteRequest, Intent, OpenSession, OpenedSession, Outcome,
    PROTOCOL_VERSION, PendingHandoff, Query, QueryRequest, RepositoryLocator, ResponseBody,
    ReviewAction, SessionId, WorkspaceSelector,
};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Barrier, Notify};

struct ObservedDependencies(Arc<Mutex<Vec<Option<String>>>>);

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
            "native command failed\nAuthorization: Bearer private-diagnostic-fixture".into(),
        ))
    }
}

#[tokio::test]
async fn operation_diagnostics_are_durable_redacted_and_excluded_from_events() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let config = EngineConfig::at(directory.path().join("state"));
    let engine = Engine::with_components(
        config.clone(),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(vec![Box::new(FailingDependencies)])),
    )
    .unwrap();
    let request = execute(
        "diagnostics",
        "failed-open",
        Intent::SessionOpen(OpenSession {
            session_id: SessionId("diagnostics-session".into()),
            repository: RepositoryLocator::Local {
                path: repository.to_string_lossy().into_owned(),
            },
            base: None,
            intent: None,
        }),
    );
    let failed = engine.execute(request.clone()).await;
    assert!(serde_json::to_vec(&failed).unwrap().len() <= 512);
    let ResponseBody::Error { error } = failed.body else {
        panic!("expected failed preparation")
    };
    let id = error.diagnostics_id.as_ref().unwrap();
    let record: shade_protocol::Diagnostic = completed(
        engine
            .query(QueryRequest {
                v: PROTOCOL_VERSION,
                request_id: "diagnostics".into(),
                query: Query::Diagnostics {
                    diagnostics_id: id.clone(),
                },
            })
            .await,
    );
    assert!(record.redacted);
    assert_eq!(record.operation, error.operation);
    assert_eq!(record.code, error.code);
    assert_eq!(
        record.message,
        "<REDACTED: diagnostic contains credentials>"
    );
    let operation = engine
        .database()
        .operation(error.operation.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(operation.error.unwrap().diagnostics_id.as_ref(), Some(id));
    let events = serde_json::to_string(&engine.events(0, 1000).unwrap()).unwrap();
    assert!(!events.contains("private-diagnostic-fixture"));
    assert!(!events.contains(&record.message));
    for name in ["state.sqlite", "state.sqlite-wal", "state.sqlite-shm"] {
        let path = config.root.join(name);
        if path.is_file() {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(
                !String::from_utf8_lossy(&fs::read(path).unwrap())
                    .contains("private-diagnostic-fixture")
            );
        }
    }
    drop(engine);
    let restarted = test_engine(directory.path());
    let ResponseBody::Error { error: replayed } = restarted.execute(request).await.body else {
        panic!("lost failed operation")
    };
    assert_eq!(replayed.diagnostics_id.as_ref(), Some(id));
    assert_eq!(
        restarted.database().diagnostic(id).unwrap().unwrap(),
        record
    );
}

struct LocatedPython;

#[tokio::test]
async fn failed_diagnostic_storage_keeps_the_operation_handle_without_a_dangling_reference() {
    let directory = tempfile::tempdir().unwrap();
    let engine = test_engine(directory.path());
    let connection = rusqlite::Connection::open(engine.config().database_path()).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_diagnostic BEFORE INSERT ON diagnostics BEGIN SELECT RAISE(ABORT, 'diagnostic storage unavailable'); END;").unwrap();
    let failed = engine
        .execute(execute(
            "diagnostics",
            "unwritable",
            Intent::SessionOpen(OpenSession {
                session_id: SessionId("unwritable-session".into()),
                repository: RepositoryLocator::Local {
                    path: directory
                        .path()
                        .join("missing")
                        .to_string_lossy()
                        .into_owned(),
                },
                base: None,
                intent: None,
            }),
        ))
        .await;
    let ResponseBody::Error { error } = failed.body else {
        panic!("expected storage failure")
    };
    assert_eq!(error.code, "INTERNAL");
    assert!(error.diagnostics_id.is_none());
    let operation = engine
        .database()
        .operation(error.operation.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(operation.state, "running");
    assert!(operation.error.is_none());
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM diagnostics", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[async_trait::async_trait]
impl shade_engine::dependencies::DependencyProvider for LocatedPython {
    fn name(&self) -> &'static str {
        "uv"
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
        let venv = context.workspace_root.join(".venv");
        fs::create_dir_all(venv.join("lib")).unwrap();
        fs::create_dir_all(venv.join("bin")).unwrap();
        fs::write(venv.join("lib/addon.py"), "cached dependency\n").unwrap();
        fs::write(
            venv.join("bin/activate"),
            format!("VIRTUAL_ENV='{}'\n", venv.display()),
        )
        .unwrap();
        Ok(shade_engine::dependencies::DependencyReceipt {
            provider: "uv".into(),
            fingerprint: "fixture-python".into(),
            state: "ready".into(),
            materialized_paths: vec![".venv".into()],
            blocked_builds: vec![],
            scripts: vec![],
        })
    }
}

struct ObservedNativeCache(&'static str, Arc<AtomicUsize>);

#[async_trait::async_trait]
impl shade_engine::dependencies::DependencyProvider for ObservedNativeCache {
    fn name(&self) -> &'static str {
        self.0
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
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(shade_engine::dependencies::DependencyReceipt {
            provider: self.name().into(),
            fingerprint: "external-cache".into(),
            state: "ready".into(),
            materialized_paths: vec![],
            blocked_builds: vec![],
            scripts: vec![],
        })
    }
}

#[async_trait::async_trait]
impl shade_engine::dependencies::DependencyProvider for ObservedDependencies {
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
            .join("packages/member/node_modules/addon/index.js");
        self.0.lock().unwrap().push(fs::read_to_string(&path).ok());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "cached dependency\n").unwrap();
        Ok(shade_engine::dependencies::DependencyReceipt {
            provider: self.name().into(),
            fingerprint: "fixture-layer".into(),
            state: "ready".into(),
            materialized_paths: vec!["packages/member/node_modules".into()],
            blocked_builds: vec![],
            scripts: vec![],
        })
    }
}

#[tokio::test]
async fn fork_preserves_agent_dependency_bytes_and_inherits_their_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    fs::write(
        repository.join("pyproject.toml"),
        "[project]\nname='fork-fixture'\nversion='1.0.0'\n",
    )
    .unwrap();
    fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname='fork-fixture'\nversion='1.0.0'\n",
    )
    .unwrap();
    fs::write(
        repository.join("go.mod"),
        "module example.invalid/fork-fixture\n\ngo 1.20\n",
    )
    .unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-m", "native cache roots"]);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let cargo = Arc::new(AtomicUsize::new(0));
    let go = Arc::new(AtomicUsize::new(0));
    let engine = Engine::with_components(
        EngineConfig::at(directory.path().join("state")),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(vec![
            Box::new(ObservedDependencies(observed.clone())),
            Box::new(LocatedPython),
            Box::new(ObservedNativeCache("cargo", cargo.clone())),
            Box::new(ObservedNativeCache("go", go.clone())),
        ])),
    )
    .unwrap();
    let parent: OpenedSession = completed(
        engine
            .execute(execute(
                "parent",
                "open",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("parent".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    let paths = [
        "packages/member/node_modules/addon/index.js",
        ".venv/lib/addon.py",
    ];
    for relative in paths {
        fs::write(Path::new(&parent.cwd).join(relative), "agent dependency\n").unwrap();
    }
    let child: OpenedSession = completed(
        engine
            .execute(execute(
                "parent",
                "fork",
                Intent::WorkspaceFork {
                    selector: WorkspaceSelector {
                        workspace_id: Some(parent.workspace.clone()),
                        cwd: None,
                    },
                    child_session_id: SessionId("child".into()),
                    intent: None,
                },
            ))
            .await,
    );
    assert_eq!(
        fs::read_to_string(Path::new(&child.cwd).join(".venv/bin/activate")).unwrap(),
        format!("VIRTUAL_ENV='{}/.venv'\n", child.cwd)
    );
    assert_eq!(
        fs::read_to_string(Path::new(&parent.cwd).join(".venv/bin/activate")).unwrap(),
        format!("VIRTUAL_ENV='{}/.venv'\n", parent.cwd)
    );
    for relative in paths {
        assert_eq!(
            fs::read_to_string(Path::new(&child.cwd).join(relative)).unwrap(),
            "agent dependency\n",
            "observed before readiness: {:?}",
            observed.lock().unwrap()
        );
        fs::write(Path::new(&child.cwd).join(relative), "child-only\n").unwrap();
        assert_eq!(
            fs::read_to_string(Path::new(&parent.cwd).join(relative)).unwrap(),
            "agent dependency\n"
        );
    }
    assert_eq!(
        observed.lock().unwrap().len(),
        1,
        "fork must not replace the cloned forest from a cache"
    );
    assert_eq!(
        cargo.load(Ordering::SeqCst),
        2,
        "external Cargo cache must be revalidated"
    );
    assert_eq!(
        go.load(Ordering::SeqCst),
        2,
        "external Go cache must be revalidated"
    );
    assert_eq!(
        engine
            .database()
            .dependency_receipts(&parent.workspace)
            .unwrap(),
        engine
            .database()
            .dependency_receipts(&child.workspace)
            .unwrap()
    );
}

#[derive(Debug)]
struct RejectIncrementalClone;

impl WorkspaceFilesystem for RejectIncrementalClone {
    fn clone_tree(&self, source: &Path, destination: &Path) -> anyhow::Result<()> {
        if destination
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".shade-materialize-"))
        {
            return Err(CowUnavailable {
                source_path: source.to_owned(),
                destination_path: destination.to_owned(),
                reason: "injected clone failure".into(),
            }
            .into());
        }
        CopyFilesystem.clone_tree(source, destination)
    }

    fn publish_tree(&self, staging: &Path, destination: &Path) -> anyhow::Result<()> {
        CopyFilesystem.publish_tree(staging, destination)
    }

    fn usage(&self, root: &Path) -> anyhow::Result<Usage> {
        CopyFilesystem.usage(root)
    }

    fn remove_tree(&self, root: &Path) -> anyhow::Result<()> {
        CopyFilesystem.remove_tree(root)
    }
}

#[tokio::test]
async fn conflicting_secret_merge_reuses_review_without_allocating_a_successor() {
    for (filename, base_value, parent_value, child_value) in [
        (
            ".env.local",
            "TOKEN=base\n".as_bytes().to_vec(),
            b"TOKEN=parent\n".to_vec(),
            b"TOKEN=child\n".to_vec(),
        ),
        (
            "credentials.json",
            br#"{"api_key":"b7Y2n9W4q1R8d3M6"}"#.to_vec(),
            br#"{"api_key":"d9L2p7W4m1B8a3H6"}"#.to_vec(),
            br#"{"api_key":"h4C8v1R6n2D7q9S3"}"#.to_vec(),
        ),
        (
            "private.bin",
            b"\xff-----BEGIN PRIVATE KEY-----\x01".to_vec(),
            b"\xff-----BEGIN PRIVATE KEY-----\x02".to_vec(),
            b"\xff-----BEGIN PRIVATE KEY-----\x03".to_vec(),
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let repository = fixture(directory.path());
        let engine = test_engine(directory.path());
        let parent: OpenedSession = completed(
            engine
                .execute(execute(
                    "secret-parent",
                    "secret-open",
                    Intent::SessionOpen(OpenSession {
                        session_id: SessionId("secret-parent".into()),
                        repository: RepositoryLocator::Local {
                            path: repository.to_string_lossy().into_owned(),
                        },
                        base: Some("main".into()),
                        intent: None,
                    }),
                ))
                .await,
        );
        fs::write(Path::new(&parent.cwd).join(filename), base_value).unwrap();
        let child: OpenedSession = completed(
            engine
                .execute(execute(
                    "secret-parent",
                    "secret-fork",
                    Intent::WorkspaceFork {
                        selector: WorkspaceSelector {
                            workspace_id: Some(parent.workspace.clone()),
                            cwd: None,
                        },
                        child_session_id: SessionId("secret-child".into()),
                        intent: None,
                    },
                ))
                .await,
        );
        fs::write(Path::new(&parent.cwd).join(filename), parent_value).unwrap();
        fs::write(Path::new(&child.cwd).join(filename), child_value).unwrap();

        let review = engine
            .execute(execute(
                "secret-child",
                "secret-release",
                Intent::WorkspaceRelease {
                    selector: WorkspaceSelector {
                        workspace_id: Some(child.workspace.clone()),
                        cwd: None,
                    },
                },
            ))
            .await;
        let review_id = match review.body {
            ResponseBody::Ok {
                outcome: Outcome::ReviewRequired(required),
            } => {
                assert!(
                    required
                        .files
                        .iter()
                        .any(|file| file.path == filename && file.file_result == "conflict")
                );
                required.review_id
            }
            other => panic!("expected a secret review, got {other:?}"),
        };
        let workspace_count = engine.database().workspaces().unwrap().len();
        let retry = engine
            .execute(execute(
                "secret-child",
                "secret-merge-conflict",
                Intent::ReviewResolve {
                    review_id: review_id.clone(),
                    action: ReviewAction::MergeParent,
                },
            ))
            .await;
        match retry.body {
            ResponseBody::Ok {
                outcome: Outcome::ReviewRequired(required),
            } => assert_eq!(required.review_id, review_id),
            other => panic!("expected the pending review to remain actionable, got {other:?}"),
        }
        let workspaces = engine.database().workspaces().unwrap();
        assert_eq!(workspaces.len(), workspace_count);
        assert!(
            workspaces
                .iter()
                .all(|workspace| workspace.state != "materializing"),
            "a conflict must not leak a half-materialized successor"
        );
    }
}

#[tokio::test]
async fn fork_and_restore_preserve_detected_secrets_in_tracked_files_outside_git() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = test_engine(directory.path());
    let parent: OpenedSession = completed(
        engine
            .execute(execute(
                "content-parent",
                "open-content",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("content-parent".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    let bytes = br#"{"api_key":"b7Y2n9W4q1R8d3M6","enabled":true}"#;
    let root = Path::new(&parent.cwd);
    fs::write(root.join("tracked.txt"), bytes).unwrap();
    fs::write(root.join("credentials.json"), bytes).unwrap();
    let blob = git(root, &["hash-object", "--no-filters", "tracked.txt"]);
    let child: OpenedSession = completed(
        engine
            .execute(execute(
                "content-parent",
                "fork-content",
                Intent::WorkspaceFork {
                    selector: WorkspaceSelector {
                        workspace_id: Some(parent.workspace.clone()),
                        cwd: None,
                    },
                    child_session_id: SessionId("content-child".into()),
                    intent: None,
                },
            ))
            .await,
    );
    for path in ["tracked.txt", "credentials.json"] {
        assert_eq!(fs::read(Path::new(&child.cwd).join(path)).unwrap(), bytes);
        assert_eq!(
            fs::metadata(Path::new(&child.cwd).join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(
        git(Path::new(&child.cwd), &["show", ":tracked.txt"]),
        "base"
    );
    assert!(
        !Command::new("git")
            .current_dir(root)
            .args(["cat-file", "-e", &blob])
            .output()
            .unwrap()
            .status
            .success()
    );
    let checkpoint: serde_json::Value = completed(
        engine
            .execute(execute(
                "content-child",
                "checkpoint-content",
                Intent::WorkspaceCheckpoint {
                    selector: WorkspaceSelector {
                        workspace_id: Some(child.workspace.clone()),
                        cwd: None,
                    },
                    reason: "private content".into(),
                },
            ))
            .await,
    );
    let response = engine
        .execute(execute(
            "content-child",
            "restore-content",
            Intent::WorkspaceRestore {
                selector: WorkspaceSelector {
                    workspace_id: Some(child.workspace.clone()),
                    cwd: None,
                },
                checkpoint_id: CheckpointId(checkpoint["checkpoint_id"].as_str().unwrap().into()),
            },
        ))
        .await;
    let successor = adopt_successor(&engine, "content-child", "adopt-content", response).await;
    for path in ["tracked.txt", "credentials.json"] {
        assert_eq!(
            fs::read(Path::new(&successor.cwd).join(path)).unwrap(),
            bytes
        );
        assert_eq!(fs::read(Path::new(&child.cwd).join(path)).unwrap(), bytes);
    }
    assert_eq!(
        git(Path::new(&successor.cwd), &["show", ":tracked.txt"]),
        "base"
    );
    assert!(
        !Command::new("git")
            .current_dir(root)
            .args(["cat-file", "-e", &blob])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        fs::read_to_string(repository.join("tracked.txt")).unwrap(),
        "base\n"
    );
}

#[derive(Debug, Default)]
struct CountIncrementalClones {
    count: AtomicUsize,
}

impl WorkspaceFilesystem for CountIncrementalClones {
    fn clone_tree(&self, source: &Path, destination: &Path) -> anyhow::Result<()> {
        if destination
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(".shade-materialize-"))
        {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
        CopyFilesystem.clone_tree(source, destination)
    }

    fn publish_tree(&self, staging: &Path, destination: &Path) -> anyhow::Result<()> {
        CopyFilesystem.publish_tree(staging, destination)
    }

    fn usage(&self, root: &Path) -> anyhow::Result<Usage> {
        CopyFilesystem.usage(root)
    }

    fn remove_tree(&self, root: &Path) -> anyhow::Result<()> {
        CopyFilesystem.remove_tree(root)
    }
}

#[derive(Default)]
struct CloneBlockState {
    block_next: bool,
    entered_at: Option<Instant>,
    released: bool,
}

#[derive(Default)]
struct BlockingCloneFilesystem {
    state: Mutex<CloneBlockState>,
    wake: Condvar,
    entered: Notify,
}

impl BlockingCloneFilesystem {
    fn arm_next(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.block_next = true;
        state.entered_at = None;
        state.released = false;
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.released = true;
        self.wake.notify_all();
    }

    fn entered_delay(&self) -> Option<Duration> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entered_at
            .map(|entered| entered.elapsed())
    }

    fn is_blocked(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.entered_at.is_some() && !state.released
    }

    fn spawn_watchdog(filesystem: Arc<Self>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut state = filesystem
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while state.entered_at.is_none() && !state.released {
                state = filesystem
                    .wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if state.released {
                return;
            }
            let (mut state, _) = filesystem
                .wake
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.released)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !state.released {
                state.released = true;
                filesystem.wake.notify_all();
            }
        })
    }
}

impl WorkspaceFilesystem for BlockingCloneFilesystem {
    fn clone_tree(&self, source: &Path, destination: &Path) -> anyhow::Result<()> {
        let should_block = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.block_next {
                state.block_next = false;
                state.entered_at = Some(Instant::now());
                self.entered.notify_one();
                self.wake.notify_all();
                true
            } else {
                false
            }
        };
        if should_block {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while !state.released {
                state = self
                    .wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        }
        CopyFilesystem.clone_tree(source, destination)
    }

    fn publish_tree(&self, staging: &Path, destination: &Path) -> anyhow::Result<()> {
        CopyFilesystem.publish_tree(staging, destination)
    }

    fn usage(&self, root: &Path) -> anyhow::Result<Usage> {
        CopyFilesystem.usage(root)
    }

    fn remove_tree(&self, root: &Path) -> anyhow::Result<()> {
        CopyFilesystem.remove_tree(root)
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn fixture(root: &Path) -> PathBuf {
    let repository = root.join("source");
    fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init", "-b", "main"]);
    git(&repository, &["config", "user.name", "Shade Test"]);
    git(&repository, &["config", "user.email", "shade@test.invalid"]);
    fs::write(repository.join("tracked.txt"), "base\n").unwrap();
    fs::write(repository.join("delete.txt"), "delete me\n").unwrap();
    git(&repository, &["add", "tracked.txt", "delete.txt"]);
    git(&repository, &["commit", "-m", "base"]);
    repository
}

fn origin_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let origin = root.join("origin.git");
    git(
        root,
        &["init", "--bare", "-b", "main", origin.to_str().unwrap()],
    );
    let writer = root.join("writer");
    fs::create_dir_all(&writer).unwrap();
    git(&writer, &["init", "-b", "main"]);
    git(&writer, &["config", "user.name", "Shade Test"]);
    git(&writer, &["config", "user.email", "shade@test.invalid"]);
    fs::write(writer.join("tracked.txt"), "origin-old\n").unwrap();
    git(&writer, &["add", "tracked.txt"]);
    git(&writer, &["commit", "-m", "origin old"]);
    git(
        &writer,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&writer, &["push", "-u", "origin", "main"]);
    (origin, writer)
}

fn clone_origin(root: &Path, origin: &Path, name: &str) -> PathBuf {
    let destination = root.join(name);
    git(
        root,
        &[
            "clone",
            "--branch",
            "main",
            origin.to_str().unwrap(),
            destination.to_str().unwrap(),
        ],
    );
    destination
}

fn advance_origin(writer: &Path, contents: &str) -> String {
    fs::write(writer.join("tracked.txt"), contents).unwrap();
    git(writer, &["add", "tracked.txt"]);
    git(writer, &["commit", "-m", "advance origin"]);
    git(writer, &["push", "origin", "main"]);
    git(writer, &["rev-parse", "HEAD"])
}

fn test_engine(root: &Path) -> Engine {
    Engine::with_components(
        EngineConfig::at(root.join("state")),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap()
}

fn execute(session: &str, key: &str, intent: Intent) -> ExecuteRequest {
    ExecuteRequest {
        v: PROTOCOL_VERSION,
        request_id: format!("request-{key}"),
        idempotency_key: key.to_owned(),
        actor: Actor {
            kind: ActorKind::Zenith,
            id: format!("actor-{session}"),
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

async fn adopt_successor(
    engine: &Engine,
    session: &str,
    key: &str,
    response: shade_protocol::WireResponse,
) -> OpenedSession {
    let pending: PendingHandoff = completed(response);
    completed(
        engine
            .execute(execute(
                session,
                key,
                Intent::SuccessorAdopt {
                    handoff_id: pending.handoff_id,
                },
            ))
            .await,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_sessions_share_one_base_and_remain_isolated() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("shade_engine=trace")
        .with_test_writer()
        .try_init();
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let counters = Arc::new(GitCounters::default());
    let engine = Engine::with_components_and_git(
        EngineConfig::at(directory.path().join("state")),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
        GitStore::system().with_counters(counters.clone()),
    )
    .unwrap();
    let start = Arc::new(Barrier::new(21));
    let mut tasks = Vec::new();
    for index in 0..20 {
        let engine = engine.clone();
        let repository = repository.clone();
        let start = start.clone();
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            let session = format!("chat-{index:02}");
            let response = engine
                .execute(execute(
                    &session,
                    &format!("open-{index}"),
                    Intent::SessionOpen(OpenSession {
                        session_id: SessionId(session.clone()),
                        repository: RepositoryLocator::Local {
                            path: repository.to_string_lossy().into_owned(),
                        },
                        base: Some("main".into()),
                        intent: Some("test".into()),
                    }),
                ))
                .await;
            completed::<OpenedSession>(response)
        }));
    }
    start.wait().await;
    let mut opened = Vec::new();
    for task in tasks {
        opened.push(task.await.unwrap());
    }
    let bases = fs::read_dir(engine.config().bases_dir())
        .unwrap()
        .flat_map(|entry| fs::read_dir(entry.unwrap().path()).unwrap())
        .count();
    assert_eq!(bases, 1, "concurrent opens must share one immutable base");
    assert_eq!(
        counters.source_fetches(),
        1,
        "the strict local import must be the only source fetch"
    );
    assert_eq!(
        counters.base_materializations(),
        1,
        "the imported revision must materialize exactly one immutable base"
    );

    fs::write(
        Path::new(&opened[0].cwd).join("tracked.txt"),
        "workspace zero\n",
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(Path::new(&opened[1].cwd).join("tracked.txt")).unwrap(),
        "base\n"
    );
    let base_file = fs::read_dir(engine.config().bases_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let base_file = fs::read_dir(base_file)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("tracked.txt");
    assert_eq!(fs::read_to_string(base_file).unwrap(), "base\n");

    let query = engine
        .query(QueryRequest {
            v: PROTOCOL_VERSION,
            request_id: "context".into(),
            query: Query::Context {
                selector: WorkspaceSelector {
                    workspace_id: Some(opened[0].workspace.clone()),
                    cwd: None,
                },
            },
        })
        .await;
    assert!(serde_json::to_vec(&query).unwrap().len() <= 512);
    let events = engine.events(0, 1_000).unwrap();
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].cursor < pair[1].cursor)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_clone_uses_origin_identity_and_opens_the_fresh_remote_head() {
    let directory = tempfile::tempdir().unwrap();
    let (origin, writer) = origin_fixture(directory.path());
    let stale_clone = clone_origin(directory.path(), &origin, "stale-clone");
    let fresh_commit = advance_origin(&writer, "origin-fresh\n");
    assert_ne!(git(&stale_clone, &["rev-parse", "HEAD"]), fresh_commit);

    let engine = test_engine(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "fresh-local",
                "open-fresh-local",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("fresh-local".into()),
                    repository: RepositoryLocator::Local {
                        path: stale_clone.to_string_lossy().into_owned(),
                    },
                    base: None,
                    intent: Some("freshness".into()),
                }),
            ))
            .await,
    );

    assert_eq!(
        fs::read_to_string(Path::new(&opened.cwd).join("tracked.txt")).unwrap(),
        "origin-fresh\n"
    );
    assert_eq!(opened.compact_context.base_sha.0, fresh_commit);

    let records = engine.database().repositories().unwrap();
    assert_eq!(records.len(), 1);
    let expected = GitStore::system()
        .canonicalize_remote(origin.to_str().unwrap())
        .unwrap();
    assert_eq!(records[0].identity, expected.canonical);
    let managed = shade_engine::git::ManagedRepository::new(records[0].bare_path.clone());
    let configured = GitStore::system()
        .managed_origin(&managed)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(configured, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_clones_of_one_origin_share_import_fetch_and_base() {
    let directory = tempfile::tempdir().unwrap();
    let (origin, writer) = origin_fixture(directory.path());
    let clone_a = clone_origin(directory.path(), &origin, "clone-a");
    let clone_b = clone_origin(directory.path(), &origin, "clone-b");
    let fresh_commit = advance_origin(&writer, "shared-fresh\n");

    let counters = Arc::new(GitCounters::default());
    let engine = Engine::with_components_and_git(
        EngineConfig::at(directory.path().join("state")),
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
        GitStore::system().with_counters(counters.clone()),
    )
    .unwrap();
    let start = Arc::new(Barrier::new(3));
    let mut tasks = Vec::new();
    for (index, repository) in [clone_a, clone_b].into_iter().enumerate() {
        let engine = engine.clone();
        let start = start.clone();
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            let session = format!("origin-chat-{index}");
            completed::<OpenedSession>(
                engine
                    .execute(execute(
                        &session,
                        &format!("origin-open-{index}"),
                        Intent::SessionOpen(OpenSession {
                            session_id: SessionId(session.clone()),
                            repository: RepositoryLocator::Local {
                                path: repository.to_string_lossy().into_owned(),
                            },
                            base: None,
                            intent: Some("origin-deduplication".into()),
                        }),
                    ))
                    .await,
            )
        }));
    }
    start.wait().await;
    let opened_a = tasks.remove(0).await.unwrap();
    let opened_b = tasks.remove(0).await.unwrap();

    for opened in [&opened_a, &opened_b] {
        assert_eq!(opened.compact_context.base_sha.0, fresh_commit);
        assert_eq!(
            fs::read_to_string(Path::new(&opened.cwd).join("tracked.txt")).unwrap(),
            "shared-fresh\n"
        );
    }
    assert_eq!(engine.database().repositories().unwrap().len(), 1);
    assert_eq!(
        counters.source_fetches(),
        2,
        "one local seed import plus one strict origin fetch must serve both opens"
    );
    assert_eq!(counters.base_materializations(), 1);
    let bases = fs::read_dir(engine.config().bases_dir())
        .unwrap()
        .flat_map(|entry| fs::read_dir(entry.unwrap().path()).unwrap())
        .count();
    assert_eq!(bases, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_clone_keeps_queries_and_heartbeats_responsive() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let filesystem = Arc::new(BlockingCloneFilesystem::default());
    let engine = Engine::with_components(
        EngineConfig::at(directory.path().join("state")),
        filesystem.clone(),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap();
    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "responsive-a",
                "responsive-open-a",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("responsive-a".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );

    filesystem.arm_next();
    let watchdog = BlockingCloneFilesystem::spawn_watchdog(filesystem.clone());
    let open_engine = engine.clone();
    let open_repository = repository.clone();
    let second_open = tokio::spawn(async move {
        open_engine
            .execute(execute(
                "responsive-b",
                "responsive-open-b",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("responsive-b".into()),
                    repository: RepositoryLocator::Local {
                        path: open_repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await
    });

    let entered = tokio::time::timeout(Duration::from_secs(5), filesystem.entered.notified()).await;
    if entered.is_err() {
        filesystem.release();
        watchdog.join().unwrap();
        panic!("the second open never reached the injected clone boundary");
    }
    let entered_delay = filesystem.entered_delay().unwrap();

    let responsiveness = tokio::time::timeout(Duration::from_millis(500), async {
        tokio::join!(
            engine.query(QueryRequest {
                v: PROTOCOL_VERSION,
                request_id: "doctor-during-clone".into(),
                query: Query::Doctor,
            }),
            engine.execute(execute(
                "responsive-a",
                "heartbeat-during-clone",
                Intent::LeaseHeartbeat {
                    session_id: opened.session.clone(),
                    lease_id: opened.lease.clone(),
                },
            )),
        )
    })
    .await;
    let remained_blocked = filesystem.is_blocked();
    filesystem.release();
    watchdog.join().unwrap();
    let second_response = tokio::time::timeout(Duration::from_secs(10), second_open)
        .await
        .expect("second open did not finish after releasing the clone")
        .unwrap();
    let _: OpenedSession = completed(second_response);

    assert!(
        entered_delay < Duration::from_millis(500),
        "the runtime could not observe clone entry for {entered_delay:?}"
    );
    let (doctor, heartbeat) = responsiveness.expect("query or heartbeat was blocked by cloning");
    assert!(matches!(doctor.body, ResponseBody::Ok { .. }));
    let _: serde_json::Value = completed(heartbeat);
    assert!(
        remained_blocked,
        "the clone must still be fenced while query and heartbeat complete"
    );
}

#[tokio::test]
async fn engine_publishes_incremental_base_without_mutating_previous_base() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let commit_a = git(&repository, &["rev-parse", "HEAD"]);
    let engine = test_engine(directory.path());
    let opened_a: OpenedSession = completed(
        engine
            .execute(execute(
                "base-a-chat",
                "open-base-a",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("base-a-chat".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    assert_eq!(
        fs::read_to_string(Path::new(&opened_a.cwd).join("tracked.txt")).unwrap(),
        "base\n"
    );

    fs::write(repository.join("tracked.txt"), "base-b\n").unwrap();
    fs::remove_file(repository.join("delete.txt")).unwrap();
    fs::write(repository.join("added.txt"), "added\n").unwrap();
    fs::write(repository.join("executable"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        repository.join("executable"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("tracked.txt", repository.join("tracked-link")).unwrap();
    git(&repository, &["add", "-A"]);
    git(&repository, &["commit", "-m", "base b"]);
    let commit_b = git(&repository, &["rev-parse", "HEAD"]);

    let opened_b: OpenedSession = completed(
        engine
            .execute(execute(
                "base-b-chat",
                "open-base-b",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("base-b-chat".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    assert_eq!(
        fs::read_to_string(Path::new(&opened_b.cwd).join("tracked.txt")).unwrap(),
        "base-b\n"
    );
    assert!(!Path::new(&opened_b.cwd).join("delete.txt").exists());
    assert_eq!(
        fs::read_link(Path::new(&opened_b.cwd).join("tracked-link")).unwrap(),
        Path::new("tracked.txt")
    );

    let repository_pool = fs::read_dir(engine.config().bases_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let base_a = repository_pool.join(&commit_a);
    let base_b = repository_pool.join(&commit_b);
    assert!(base_a.is_dir());
    assert!(base_b.is_dir());
    assert_eq!(
        fs::read_to_string(base_a.join("tracked.txt")).unwrap(),
        "base\n"
    );
    assert!(base_a.join("delete.txt").is_file());
    assert!(!base_a.join("added.txt").exists());
    assert_eq!(
        fs::read_to_string(base_b.join("tracked.txt")).unwrap(),
        "base-b\n"
    );
    assert!(!base_b.join("delete.txt").exists());
    assert_ne!(
        fs::metadata(base_b.join("executable"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert!(fs::read_dir(&repository_pool).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".shade-materialize-")
    }));
}

#[tokio::test]
async fn incremental_clone_failure_is_cow_unavailable_without_copy_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let commit_a = git(&repository, &["rev-parse", "HEAD"]);
    let engine = Engine::with_components(
        EngineConfig::at(directory.path().join("state")),
        Arc::new(RejectIncrementalClone),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap();
    let _: OpenedSession = completed(
        engine
            .execute(execute(
                "cow-a-chat",
                "open-cow-a",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("cow-a-chat".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );

    fs::write(repository.join("tracked.txt"), "next\n").unwrap();
    git(&repository, &["add", "tracked.txt"]);
    git(&repository, &["commit", "-m", "next"]);
    let commit_b = git(&repository, &["rev-parse", "HEAD"]);
    let response = engine
        .execute(execute(
            "cow-b-chat",
            "open-cow-b",
            Intent::SessionOpen(OpenSession {
                session_id: SessionId("cow-b-chat".into()),
                repository: RepositoryLocator::Local {
                    path: repository.to_string_lossy().into_owned(),
                },
                base: Some("main".into()),
                intent: None,
            }),
        ))
        .await;
    assert!(matches!(
        response.body,
        ResponseBody::Error { error } if error.code == "COW_UNAVAILABLE"
    ));

    let repository_pool = fs::read_dir(engine.config().bases_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(repository_pool.join(commit_a).is_dir());
    assert!(
        !repository_pool.join(commit_b).exists(),
        "Shade must not fall back to a non-COW base copy"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_revision_opens_single_flight_the_incremental_base() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let filesystem = Arc::new(CountIncrementalClones::default());
    let engine = Engine::with_components(
        EngineConfig::at(directory.path().join("state")),
        filesystem.clone(),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap();
    let _: OpenedSession = completed(
        engine
            .execute(execute(
                "single-flight-a",
                "single-flight-open-a",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("single-flight-a".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    fs::write(repository.join("tracked.txt"), "revision-b\n").unwrap();
    git(&repository, &["add", "tracked.txt"]);
    git(&repository, &["commit", "-m", "revision b"]);

    let mut tasks = Vec::new();
    for index in 0..20 {
        let engine = engine.clone();
        let repository = repository.clone();
        tasks.push(tokio::spawn(async move {
            let session = format!("incremental-{index:02}");
            completed::<OpenedSession>(
                engine
                    .execute(execute(
                        &session,
                        &format!("incremental-open-{index:02}"),
                        Intent::SessionOpen(OpenSession {
                            session_id: SessionId(session.clone()),
                            repository: RepositoryLocator::Local {
                                path: repository.to_string_lossy().into_owned(),
                            },
                            base: Some("main".into()),
                            intent: None,
                        }),
                    ))
                    .await,
            )
        }));
    }
    for task in tasks {
        let opened = task.await.unwrap();
        assert_eq!(
            fs::read_to_string(Path::new(&opened.cwd).join("tracked.txt")).unwrap(),
            "revision-b\n"
        );
    }
    assert_eq!(
        filesystem.count.load(Ordering::SeqCst),
        1,
        "one revision must produce exactly one incremental base clone"
    );
}

#[tokio::test]
async fn restore_reproduces_all_git_planes_in_a_successor() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = test_engine(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "restore-chat",
                "open",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("restore-chat".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    let original = PathBuf::from(&opened.cwd);
    fs::write(original.join("staged.txt"), "staged\n").unwrap();
    git(&original, &["add", "staged.txt"]);
    fs::write(original.join("staged.txt"), "staged then changed\n").unwrap();
    fs::remove_file(original.join("delete.txt")).unwrap();
    fs::write(original.join("untracked.txt"), "untracked\n").unwrap();
    fs::write(original.join("executable.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(original.join("executable.sh"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(original.join("executable.sh"), permissions).unwrap();
    symlink("tracked.txt", original.join("link")).unwrap();
    fs::write(original.join(".env.local"), "TOKEN=never-in-git\n").unwrap();
    let before = git(
        &original,
        &["status", "--porcelain=v2", "--untracked-files=all"],
    );

    let checkpoint_response = engine
        .execute(execute(
            "restore-chat",
            "checkpoint",
            Intent::WorkspaceCheckpoint {
                selector: WorkspaceSelector {
                    workspace_id: Some(opened.workspace.clone()),
                    cwd: None,
                },
                reason: "acceptance".into(),
            },
        ))
        .await;
    let checkpoint_value: serde_json::Value = completed(checkpoint_response);
    let checkpoint = CheckpointId(
        checkpoint_value["checkpoint_id"]
            .as_str()
            .unwrap()
            .to_owned(),
    );
    let restore_response = engine
        .execute(execute(
            "restore-chat",
            "restore",
            Intent::WorkspaceRestore {
                selector: WorkspaceSelector {
                    workspace_id: Some(opened.workspace.clone()),
                    cwd: None,
                },
                checkpoint_id: checkpoint,
            },
        ))
        .await;
    let pending: PendingHandoff = completed(restore_response.clone());
    assert_eq!(pending.predecessor, opened.workspace);
    assert_eq!(
        engine
            .database()
            .active_lease_for_session(&opened.session)
            .unwrap()
            .unwrap()
            .workspace_id,
        opened.workspace,
        "predecessor lease must remain live until Zenith adopts"
    );
    let pending_workspace = engine
        .database()
        .workspace(&pending.successor)
        .unwrap()
        .unwrap();
    assert_eq!(pending_workspace.state, "handoff_pending");
    assert!(pending_workspace.session_id.is_none());
    let journaled = engine
        .database()
        .operation_by_key("zenith\0actor-restore-chat", "restore")
        .unwrap()
        .unwrap();
    assert_eq!(journaled.state, "completed");
    assert!(matches!(journaled.outcome, Some(Outcome::Completed(_))));
    let restored =
        adopt_successor(&engine, "restore-chat", "restore-adopt", restore_response).await;
    let successor = PathBuf::from(restored.cwd);
    assert_ne!(successor, original);
    assert!(
        original.exists(),
        "restore must not mutate/delete predecessor"
    );
    assert_eq!(
        git(
            &successor,
            &["status", "--porcelain=v2", "--untracked-files=all"]
        ),
        before
    );
    assert_eq!(
        fs::read_link(successor.join("link")).unwrap(),
        PathBuf::from("tracked.txt")
    );
    assert_eq!(
        fs::metadata(successor.join("executable.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
    assert_eq!(
        fs::read_to_string(successor.join(".env.local")).unwrap(),
        "TOKEN=never-in-git\n"
    );
    assert_eq!(
        fs::read_to_string(original.join("staged.txt")).unwrap(),
        "staged then changed\n"
    );
}

#[tokio::test]
async fn resolving_publish_conflict_finishes_the_original_cas_intent() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = test_engine(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "publish-chat",
                "open-publish",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("publish-chat".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    fs::write(Path::new(&opened.cwd).join("tracked.txt"), "agent\n").unwrap();
    fs::write(repository.join("tracked.txt"), "remote\n").unwrap();
    git(&repository, &["add", "tracked.txt"]);
    git(&repository, &["commit", "-m", "remote advance"]);

    let conflict = engine
        .execute(execute(
            "publish-chat",
            "publish-conflict",
            Intent::WorkspacePublish {
                selector: WorkspaceSelector {
                    workspace_id: Some(opened.workspace.clone()),
                    cwd: None,
                },
                branch: "main".into(),
                message: "Shade resolved publish".into(),
                push: false,
            },
        ))
        .await;
    let (resolution_id, resolution_cwd) = match conflict.body {
        ResponseBody::Ok {
            outcome: Outcome::Conflict(conflict),
        } => (conflict.workspace, PathBuf::from(conflict.cwd)),
        other => panic!("expected publish conflict, got {other:?}"),
    };
    assert!(Path::new(&opened.cwd).exists());
    fs::write(resolution_cwd.join("tracked.txt"), "resolved\n").unwrap();
    git(&resolution_cwd, &["add", "tracked.txt"]);

    let resolution = engine
        .database()
        .workspace(&resolution_id)
        .unwrap()
        .unwrap();
    let managed = engine
        .database()
        .repository_by_id(&resolution.repository_id)
        .unwrap()
        .unwrap();
    let remote_tip = git(&repository, &["rev-parse", "HEAD"]);
    let git_dir = format!("--git-dir={}", managed.bare_path.display());
    git(
        directory.path(),
        &[&git_dir, "update-ref", "refs/heads/main", &remote_tip],
    );
    let fenced = engine
        .execute(execute(
            "publish-chat",
            "resolve-publish-stale-local",
            Intent::ResolutionComplete {
                selector: WorkspaceSelector {
                    workspace_id: Some(resolution_id.clone()),
                    cwd: None,
                },
            },
        ))
        .await;
    assert!(matches!(
        fenced.body,
        ResponseBody::Error { error } if error.code == "PUBLISH_LOCAL_MOVED"
    ));
    assert_eq!(
        git(
            directory.path(),
            &[&git_dir, "rev-parse", "refs/heads/main"]
        ),
        remote_tip
    );
    git(
        directory.path(),
        &[&git_dir, "update-ref", "-d", "refs/heads/main", &remote_tip],
    );

    let successor = adopt_successor(
        &engine,
        "publish-chat",
        "resolve-publish-adopt",
        engine
            .execute(execute(
                "publish-chat",
                "resolve-publish",
                Intent::ResolutionComplete {
                    selector: WorkspaceSelector {
                        workspace_id: Some(resolution_id.clone()),
                        cwd: None,
                    },
                },
            ))
            .await,
    )
    .await;
    assert_eq!(successor.workspace, resolution_id);
    assert_ne!(successor.workspace, opened.workspace);
    assert_eq!(
        fs::read_to_string(repository.join("tracked.txt")).unwrap(),
        "remote\n"
    );

    assert_eq!(
        git(
            directory.path(),
            &[
                &format!("--git-dir={}", managed.bare_path.display()),
                "show",
                "refs/heads/main:tracked.txt",
            ],
        ),
        "resolved"
    );
    assert_eq!(
        engine
            .database()
            .publish_resolution(&successor.workspace)
            .unwrap()
            .unwrap()
            .state,
        "completed"
    );
}

#[tokio::test]
async fn internal_maintenance_is_system_only_and_checkpoint_requires_live_lease() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = test_engine(directory.path());
    let forbidden = engine
        .execute(execute("guard-chat", "reconcile", Intent::Reconcile))
        .await;
    assert!(matches!(
        forbidden.body,
        ResponseBody::Error { error } if error.code == "INTENT_FORBIDDEN"
    ));
    assert!(
        engine
            .database()
            .operation_by_key("zenith\0actor-guard-chat", "reconcile")
            .unwrap()
            .is_none(),
        "forbidden internal intents must not enter the durable journal"
    );

    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "guard-chat",
                "open",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("guard-chat".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    let selector = WorkspaceSelector {
        workspace_id: Some(opened.workspace.clone()),
        cwd: None,
    };
    let _: serde_json::Value = completed(
        engine
            .execute(execute(
                "guard-chat",
                "release",
                Intent::WorkspaceRelease {
                    selector: selector.clone(),
                },
            ))
            .await,
    );
    let checkpoint = engine
        .execute(execute(
            "guard-chat",
            "checkpoint-after-release",
            Intent::WorkspaceCheckpoint {
                selector,
                reason: "must-fail".into(),
            },
        ))
        .await;
    assert!(matches!(
        checkpoint.body,
        ResponseBody::Error { error }
            if error.code == "LEASE_EXPIRED" || error.code == "LEASE_FENCED"
    ));
}

#[tokio::test]
async fn read_only_context_is_fast_path_but_every_retention_intent_rejects_indexed_secrets() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = test_engine(directory.path());
    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "secret-boundary",
                "open",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId("secret-boundary".into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: Some("main".into()),
                    intent: None,
                }),
            ))
            .await,
    );
    let selector = WorkspaceSelector {
        workspace_id: Some(opened.workspace.clone()),
        cwd: None,
    };
    let workspace = Path::new(&opened.cwd);
    let context: serde_json::Value = completed(
        engine
            .query(QueryRequest {
                v: PROTOCOL_VERSION,
                request_id: "request-context-after-bypass".into(),
                query: Query::Context {
                    selector: selector.clone(),
                },
            })
            .await,
    );
    assert_eq!(context["changes"]["staged"], 0);

    fs::write(workspace.join(".env.local"), "TOKEN=must-not-be-retained\n").unwrap();
    git(
        workspace,
        &[
            "-c",
            "filter.shade-secret.clean=cat",
            "add",
            "-f",
            "--",
            ".env.local",
        ],
    );

    let attempts = [
        (
            "checkpoint-secret",
            Intent::WorkspaceCheckpoint {
                selector: selector.clone(),
                reason: "policy-boundary".into(),
            },
        ),
        (
            "fork-secret",
            Intent::WorkspaceFork {
                selector: selector.clone(),
                child_session_id: SessionId("secret-boundary-child".into()),
                intent: None,
            },
        ),
        (
            "sync-secret",
            Intent::WorkspaceSync {
                selector: selector.clone(),
            },
        ),
        (
            "publish-secret",
            Intent::WorkspacePublish {
                selector: selector.clone(),
                branch: "secret-must-not-publish".into(),
                message: "must fail before publish".into(),
                push: false,
            },
        ),
        (
            "release-secret",
            Intent::WorkspaceRelease {
                selector: selector.clone(),
            },
        ),
    ];
    for (key, intent) in attempts {
        let response = engine
            .execute(execute("secret-boundary", key, intent))
            .await;
        assert!(
            matches!(
                response.body,
                ResponseBody::Error { ref error } if error.code == "TRACKED_SECRET_FILE"
            ),
            "{key} crossed the secret retention boundary: {:?}",
            response.body
        );
    }

    let workspace_record = engine
        .database()
        .workspace(&opened.workspace)
        .unwrap()
        .unwrap();
    assert!(
        engine
            .database()
            .checkpoints_for_workspace(&opened.workspace)
            .unwrap()
            .is_empty()
    );
    assert_eq!(engine.database().workspaces().unwrap().len(), 1);
    assert!(
        engine
            .database()
            .session(&SessionId("secret-boundary-child".into()))
            .unwrap()
            .is_none()
    );
    let managed = engine
        .database()
        .repository_by_id(&workspace_record.repository_id)
        .unwrap()
        .unwrap()
        .bare_path;
    assert_eq!(
        git(
            directory.path(),
            &[
                &format!("--git-dir={}", managed.display()),
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/shade/workspaces/{}/checkpoints/", opened.workspace.0),
            ],
        ),
        "",
        "failed retention intents must not anchor a secret-bearing index"
    );
    assert_eq!(
        git(
            directory.path(),
            &[
                &format!("--git-dir={}", managed.display()),
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/secret-must-not-publish",
            ],
        ),
        "",
        "a rejected publish must not create or move its target branch"
    );
}
