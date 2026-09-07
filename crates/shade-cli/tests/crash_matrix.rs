#![cfg(all(
    feature = "fault-injection",
    target_os = "macos",
    target_arch = "aarch64"
))]

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shade_engine::faults::Point;
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[path = "support/dependency_crashes.rs"]
mod dependency_crashes;
#[path = "support/dependency_fixture.rs"]
mod dependency_fixture;
#[path = "../../shade-engine/tests/support/registry.rs"]
mod registry;
#[path = "support/script_fixture.rs"]
mod script_fixture;

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    source: PathBuf,
    socket: PathBuf,
    faults: PathBuf,
    binary: PathBuf,
    lease_ttl_secs: u64,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self._temp.disable_cleanup(true);
            eprintln!("SHADE_CRASH_FAILED_ROOT {}", self._temp.path().display());
        }
    }
}

impl Fixture {
    fn new(binary: &Path) -> Self {
        // Short path leaves room for Darwin's 104-byte Unix socket limit.
        let temp = tempfile::Builder::new()
            .prefix("shade-crash-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.name", "Shade crash test"]);
        git(&source, &["config", "user.email", "crash@example.invalid"]);
        std::fs::write(source.join("tracked.txt"), "base\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "fixture"]);
        let faults = temp.path().join("faults");
        std::fs::create_dir(&faults).unwrap();
        Self {
            root: temp.path().join("state"),
            socket: temp.path().join("s.sock"),
            source,
            faults,
            binary: binary.to_path_buf(),
            lease_ttl_secs: 120,
            _temp: temp,
        }
    }

    fn spawn(&self, point: Point) -> Daemon {
        let lease_ttl = self.lease_ttl_secs.to_string();
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self._temp.path().join("daemon.log"))
            .unwrap();
        Daemon(
            Command::new(&self.binary)
                .args([
                    "--socket",
                    self.socket.to_str().unwrap(),
                    "daemon",
                    "--harness-lifecycle",
                    "--harness-lease-ttl-secs",
                    lease_ttl.as_str(),
                    "--harness-orphan-grace-secs",
                    "0",
                ])
                .env("SHADE_ROOT", &self.root)
                .env("SHADE_OPERATION_WAIT_MS", "0")
                .env("SHADE_FAULT_POINT", point.name())
                .env("SHADE_FAULT_DIR", &self.faults)
                // Git's disposable indexes must also be inside the isolated case.
                .env("TMPDIR", self._temp.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .unwrap(),
        )
    }

    fn start(&self, point: Point) -> Daemon {
        let mut daemon = self.spawn(point);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(value) = try_rpc(&self.socket, &query(json!({"kind":"doctor"})))
                && value["status"] == "ok"
            {
                return daemon;
            }
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(self._temp.path().join("daemon.log")).unwrap()
            );
            assert!(
                Instant::now() < deadline,
                "daemon never reconciled at {}",
                point.name()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn arm(&self) {
        std::fs::write(self.faults.join("arm"), "").unwrap();
    }

    fn disarm(&self) {
        std::fs::remove_file(self.faults.join("arm")).unwrap();
        std::fs::remove_file(self.faults.join("reached")).unwrap();
    }

    fn interrupt_at(&self, daemon: &mut Daemon, point: Point, timeout_secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        while std::fs::read_to_string(self.faults.join("reached"))
            .ok()
            .as_deref()
            != Some(point.name())
        {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "daemon exited before {}",
                point.name()
            );
            assert!(
                Instant::now() < deadline,
                "fault point never reached: {}; journal: {:?}",
                point.name(),
                strings(
                    &self.database(),
                    "SELECT state || ':' || COALESCE(error_json, outcome_json, phase) FROM operations WHERE state='running' OR idempotency_key='interrupted'"
                )
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        daemon.0.kill().unwrap();
        assert_eq!(daemon.0.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    fn open(&self) -> Value {
        json!({"kind":"session_open", "session_id":"crash-session",
            "repository":{"kind":"local","path":self.source}, "base":"main"})
    }

    fn execute(&self, intent: Value, key: &str) -> Value {
        let response = try_rpc(&self.socket, &execute(intent, key)).unwrap();
        assert_eq!(response["status"], "ok", "{response}");
        if response["outcome"]["state"] != "accepted" {
            return response["outcome"]["result"].clone();
        }
        let id = &response["outcome"]["result"]["operation_id"];
        let deadline = Instant::now()
            + Duration::from_secs(
                if [
                    "package.json",
                    "pyproject.toml",
                    "Cargo.toml",
                    "go.mod",
                    "go.work",
                ]
                .iter()
                .any(|name| self.source.join(name).is_file())
                {
                    120
                } else {
                    20
                },
            );
        loop {
            let response = try_rpc(
                &self.socket,
                &query(json!({"kind":"operation","operation_id":id})),
            )
            .unwrap();
            let operation = &response["outcome"]["result"];
            match operation["state"].as_str() {
                Some("completed") => return operation["outcome"]["result"].clone(),
                Some("failed") => panic!(
                    "operation failed: {operation}; daemon diagnostics: {}",
                    operation["error"]["diagnostics_id"]
                        .as_str()
                        .and_then(|id| shade_engine::db::Database::read_diagnostic(
                            &self.root.join("state.sqlite"),
                            id
                        )
                        .ok()
                        .flatten())
                        .map(|diagnostic| diagnostic.message)
                        .unwrap_or_else(|| "no persisted detail".into())
                ),
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "operation timed out: {operation}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn database(&self) -> Connection {
        let db = Connection::open_with_flags(
            self.root.join("state.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.busy_timeout(Duration::from_secs(5)).unwrap();
        db
    }

    fn assert_consistent(&self) {
        let db = self.database();
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM operations o WHERE json_extract(o.error_json, '$.diagnostics_id') IS NOT NULL AND NOT EXISTS (SELECT 1 FROM diagnostics d WHERE d.id=json_extract(o.error_json, '$.diagnostics_id'))"
            ),
            0,
            "operation references a missing diagnostic"
        );
        assert_eq!(
            db.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert!(
            !db.prepare("PRAGMA foreign_key_check")
                .unwrap()
                .exists([])
                .unwrap()
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM workspaces WHERE state NOT IN ('ready','released','dormant','suspended','handoff_pending','resolution','retained')"
            ),
            0
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM workspaces w WHERE w.state='suspended' AND NOT EXISTS (SELECT 1 FROM checkpoints c WHERE c.workspace_id=w.id AND c.reason='sleep' AND c.state='ready')"
            ),
            0,
            "a suspended workspace lost the checkpoint it wakes from"
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM workspaces w JOIN sessions s ON s.id=w.session_id AND s.workspace_id=w.id WHERE w.state='ready' AND s.state!='active'"
            ),
            0,
            "released session left an uncollectable ready workspace"
        );
        // Sleeping is the one thing that takes the tree away from a workspace
        // that still has a record: the suspension itself, and the predecessor
        // a wake released out of one.
        let dematerialized = strings(
            &db,
            "SELECT w.path FROM workspaces w WHERE w.state IN ('suspended','released') AND EXISTS (SELECT 1 FROM checkpoints c WHERE c.workspace_id=w.id AND c.reason='sleep' AND c.state='ready')",
        );
        for path in &dematerialized {
            assert!(
                !Path::new(path).exists(),
                "a dematerialized workspace kept its tree: {path}"
            );
        }
        let paths: BTreeSet<_> = strings(&db, "SELECT path FROM workspaces")
            .difference(&dematerialized)
            .cloned()
            .collect();
        let disk_paths: BTreeSet<_> = std::fs::read_dir(self.root.join("workspaces"))
            .unwrap()
            .map(|entry| entry.unwrap().path().to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, disk_paths, "orphan or missing workspace directory");
        for bare in strings(&db, "SELECT bare_path FROM repositories") {
            let registered: BTreeSet<_> =
                git(Path::new(&bare), &["worktree", "list", "--porcelain"])
                    .lines()
                    .filter_map(|line| line.strip_prefix("worktree "))
                    .filter(|path| *path != bare)
                    .map(str::to_owned)
                    .collect();
            assert_eq!(registered, paths, "orphan Git registration");
            // A base imported from the local repository is anchored under
            // `refs/shade/bases/` for as long as the repository is managed,
            // exactly like the `refs/remotes/origin/*` an origin base leaves
            // behind. It is durable state rather than per-checkpoint state, so
            // the count that catches a leaked quarantine, operation or
            // checkpoint ref excludes it.
            let refs = git(
                Path::new(&bare),
                &["for-each-ref", "--format=%(refname)", "refs/shade/"],
            );
            assert_eq!(
                refs.lines()
                    .filter(|reference| !reference.starts_with("refs/shade/bases/"))
                    .count() as i64,
                count(&db, "SELECT count(*)*3 FROM checkpoints"),
                "orphan checkpoint refs"
            );
        }
        for path in &paths {
            assert_eq!(
                git(Path::new(path), &["rev-parse", "--is-inside-work-tree"]),
                "true"
            );
        }
        for pool in [
            "repositories",
            "bases",
            "workspaces",
            "secrets",
            "dependencies/staging",
        ] {
            assert_no_staging(&self.root.join(pool));
        }
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM (SELECT resource FROM events WHERE event='operation.completed' GROUP BY resource HAVING count(*)>1)"
            ),
            0
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM operations WHERE state='running' AND intent_kind!='reconcile'"
            ),
            0
        );
        assert_eq!(git(&self.source, &["status", "--porcelain"]), "");
    }
}

fn prepare_incremental_fixture(source: &Path) {
    for (name, bytes) in [
        ("removed.txt", b"remove me\n".as_slice()),
        ("mode.txt", b"#!/bin/sh\nexit 0\n".as_slice()),
        ("shape", b"file becomes a directory\n".as_slice()),
        ("content.bin", b"\0old\xff".as_slice()),
    ] {
        std::fs::write(source.join(name), bytes).unwrap();
    }
    std::fs::set_permissions(
        source.join("mode.txt"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::os::unix::fs::symlink("tracked.txt", source.join("link")).unwrap();
    git(source, &["add", "."]);
    git(source, &["commit", "-m", "incremental base inputs"]);
}

fn update_incremental_fixture(source: &Path) {
    std::fs::remove_file(source.join("removed.txt")).unwrap();
    std::fs::remove_file(source.join("shape")).unwrap();
    std::fs::create_dir(source.join("shape")).unwrap();
    std::fs::write(source.join("shape/child.txt"), "directory child\n").unwrap();
    std::fs::write(source.join("content.bin"), b"\0new\xfe").unwrap();
    std::fs::set_permissions(
        source.join("mode.txt"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::remove_file(source.join("link")).unwrap();
    std::os::unix::fs::symlink("mode.txt", source.join("link")).unwrap();
}

fn assert_incremental_bases(fixture: &Fixture, next_published: bool) {
    let repository = strings(&fixture.database(), "SELECT id FROM repositories")
        .into_iter()
        .next()
        .unwrap();
    let pool = fixture.root.join("bases").join(repository);
    let previous = pool.join(git(&fixture.source, &["rev-parse", "HEAD^"]));
    let next = pool.join(git(&fixture.source, &["rev-parse", "HEAD"]));
    assert_eq!(
        std::fs::read_dir(&pool).unwrap().count(),
        1 + usize::from(next_published)
    );
    assert_eq!(next.exists(), next_published);
    assert_eq!(
        std::fs::read(previous.join("tracked.txt")).unwrap(),
        b"base\n"
    );
    assert_eq!(
        std::fs::read(previous.join("removed.txt")).unwrap(),
        b"remove me\n"
    );
    assert_eq!(
        std::fs::read(previous.join("shape")).unwrap(),
        b"file becomes a directory\n"
    );
    assert_eq!(
        std::fs::read(previous.join("content.bin")).unwrap(),
        b"\0old\xff"
    );
    assert_eq!(
        std::fs::read_link(previous.join("link")).unwrap(),
        Path::new("tracked.txt")
    );
    assert_eq!(
        std::fs::metadata(previous.join("mode.txt")).unwrap().mode() & 0o111,
        0
    );
    assert!(!previous.join("upstream.txt").exists());
    assert!(!previous.join(".git").exists());
    if next_published {
        assert_eq!(std::fs::read(next.join("tracked.txt")).unwrap(), b"base\n");
        assert!(!next.join("removed.txt").exists());
        assert_eq!(
            std::fs::read(next.join("shape/child.txt")).unwrap(),
            b"directory child\n"
        );
        assert_eq!(
            std::fs::read(next.join("content.bin")).unwrap(),
            b"\0new\xfe"
        );
        assert_eq!(
            std::fs::read_link(next.join("link")).unwrap(),
            Path::new("mode.txt")
        );
        assert_eq!(
            std::fs::metadata(next.join("mode.txt")).unwrap().mode() & 0o111,
            0o111
        );
        assert_eq!(
            std::fs::read(next.join("upstream.txt")).unwrap(),
            b"upstream\n"
        );
        assert!(!next.join(".git").exists());
    }
}

fn count(db: &Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn strings(db: &Connection, sql: &str) -> BTreeSet<String> {
    db.prepare(sql)
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn assert_no_staging(root: &Path) {
    if !root.exists() {
        return;
    }
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        assert!(
            !name.starts_with(".shade-") && !name.ends_with(".staging"),
            "orphan staging: {}",
            entry.path().display()
        );
        if entry.file_type().unwrap().is_dir() {
            assert_no_staging(&entry.path());
        }
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

fn query(query: Value) -> Value {
    json!({"type":"query", "v":1,"request_id":"crash-query","query":query})
}

fn execute(intent: Value, key: &str) -> Value {
    json!({"type":"execute", "v":1,"request_id":key,"idempotency_key":key,
        "actor":{"kind":"agent","id":"crash-matrix"},"intent":intent})
}

fn try_rpc(socket: &Path, value: &Value) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    serde_json::to_writer(&mut stream, value)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

#[derive(Debug, Clone, Copy)]
enum Scenario {
    Open,
    OpenFailure,
    Checkpoint,
    Fork,
    Refresh,
    Adopt,
    Sync,
    IncrementalBase,
    SyncConflict,
    Publish,
    PublishConflict,
    PublishResolved,
    Reconcile,
    Provider(dependency_crashes::Case),
    Release,
    Sleep,
    Wake,
    Gc,
    Restore,
    SecretReview,
    SecretDiscard,
    SecretKeep,
    SecretMerge,
    SecretMergeAdopted,
    GcSecrets,
    ScriptApprove,
    ScriptRevoke,
    DependencyOpen,
    DependencyRefresh,
    DependencyBuild,
    DependencyGc,
}

impl Scenario {
    fn in_group(self, group: &str) -> bool {
        match group {
            "dependencies" => matches!(
                self,
                Self::DependencyOpen
                    | Self::DependencyRefresh
                    | Self::DependencyBuild
                    | Self::DependencyGc
                    | Self::Provider(_)
            ),
            "dependency-pnpm" | "dependency-bun" | "dependency-uv" | "dependency-cargo"
            | "dependency-go" => {
                matches!(self, Self::Provider(case) if group.strip_prefix("dependency-") == Some(case.manager.name()))
            }
            "lifecycle" => matches!(self, Self::Sleep | Self::Wake),
            "resolved-publication" => matches!(self, Self::PublishResolved),
            "reconciliation" => matches!(self, Self::Reconcile),
            _ => false,
        }
    }
}

fn scenario(point: Point) -> Scenario {
    // Exhaustive: adding a fault point requires adding a real crash case.
    match point {
        Point::DiagnosticWritten | Point::OperationFailureWritten | Point::OperationFailed => {
            Scenario::OpenFailure
        }
        Point::OperationRecorded
        | Point::OperationDispatched
        | Point::OperationCompleted
        | Point::RepositoryStaged
        | Point::RepositoryPromoted
        | Point::RepositoryRecorded
        | Point::BaseStaged
        | Point::BasePromoted
        | Point::WorkspaceRecorded
        | Point::WorkspaceBound
        | Point::WorkspaceCloned
        | Point::WorktreeRegistered
        | Point::WorktreePointerMoved
        | Point::WorktreeRepaired
        | Point::WorktreeIndexWritten
        | Point::DependenciesRecorded
        | Point::SecretsCaptured
        | Point::SessionActivated
        | Point::FetchQuarantined
        | Point::FetchPromoted
        | Point::CloneStaged
        | Point::ClonePromoted => Scenario::Open,
        Point::CheckpointObjectsWritten
        | Point::CheckpointAnchored
        | Point::CheckpointRecorded
        | Point::CheckpointHeadRecorded => Scenario::Checkpoint,
        Point::ForkRecorded
        | Point::ForkCloned
        | Point::ForkRestored
        | Point::ForkSecretsCaptured
        | Point::ForkActivated
        | Point::RestoreCleaned
        | Point::RestoreWorkingWritten
        | Point::RestoreHeadWritten
        | Point::RestoreIndexWritten => Scenario::Fork,
        Point::SuccessorRecorded | Point::SuccessorRestored | Point::HandoffPrepared => {
            Scenario::Refresh
        }
        Point::HandoffAdopted => Scenario::Adopt,
        Point::SyncRecorded | Point::SyncIntegrated => Scenario::Sync,
        Point::IncrementalBaseIndexStaged
        | Point::IncrementalBaseIndexWritten
        | Point::IncrementalBaseIndexRefreshed
        | Point::IncrementalBaseTreeUpdated
        | Point::IncrementalBaseTreeVerified
        | Point::IncrementalBasePromoted => Scenario::IncrementalBase,
        Point::SyncConflictRecorded => Scenario::SyncConflict,
        Point::PublishPlanned
        | Point::PublishAnchored
        | Point::PublishPrepared
        | Point::PublishLocalApplied
        | Point::PublishLocalRecorded
        | Point::PublishRemoteApplied
        | Point::PublishRemoteRecorded
        | Point::PublishCompleted
        | Point::PublishAnchorDeleted
        | Point::PublishAnchorCleaned => Scenario::Publish,
        Point::PublishResolutionRecorded
        | Point::PublishResolutionIntegrated
        | Point::PublishResolutionStateWritten
        | Point::PublishResolutionIntentWritten
        | Point::PublishResolutionReady => Scenario::PublishConflict,
        Point::ReleaseLeaseReleased | Point::ReleaseRecorded => Scenario::Release,
        Point::SleepCheckpointed
        | Point::SleepSecretsVaulted
        | Point::SleepRegistrationRemoved
        | Point::SleepRecorded => Scenario::Sleep,
        Point::WakeMaterialized | Point::WakeActivated => Scenario::Wake,
        Point::ScriptDecisionWritten | Point::ScriptDecisionCompleted => Scenario::ScriptApprove,
        Point::DependencyStaged
        | Point::DependencyFilled
        | Point::DependencyFillValidated
        | Point::DependencyReplayed
        | Point::DependencyReplayValidated
        | Point::DependencyPromotionStaged
        | Point::DependencyPayloadMoved
        | Point::DependencyReceiptWritten
        | Point::DependencyPromoted
        | Point::DependencyCloneStaged
        | Point::DependencyMaterialized
        | Point::DependencyReceiptRecorded => Scenario::DependencyOpen,
        Point::DependencyExistingStaged | Point::DependencyBackupRemoved => {
            Scenario::DependencyRefresh
        }
        Point::DependencyScriptsExecuted => Scenario::DependencyBuild,
        Point::DependencyGcRenamed
        | Point::DependencyGcPayloadRemoved
        | Point::DependencyGcDeleted => Scenario::DependencyGc,
        Point::GcClaimed
        | Point::GcTreeRemoved
        | Point::GcRefsRemoved
        | Point::GcSecretsRemoved
        | Point::GcRecordDeleted => Scenario::Gc,
        Point::ReviewSnapshotStaged | Point::ReviewSnapshotPromoted | Point::ReviewCreated => {
            Scenario::SecretReview
        }
        Point::ReviewDecisionWritten | Point::ReviewDecisionCompleted => Scenario::SecretDiscard,
        Point::SecretsMergeApplied
        | Point::SecretBaselineRemoved
        | Point::SecretBaselineCaptured
        | Point::SecretHandoffWritten
        | Point::SecretHandoffCompleted => Scenario::SecretMerge,
        Point::ReconcileLeasesExpired
        | Point::ReconcilePublishesRecovered
        | Point::ReconcileOperationsWritten
        | Point::ReconcileOperationsRecovered
        | Point::ReconcileWorkspaceClaimed
        | Point::ReconcileWorktreeUnlocked
        | Point::ReconcileRegistrationRemoved
        | Point::ReconcileWorkspaceRefsRemoved
        | Point::ReconcileWorkspaceTreeRemoved
        | Point::ReconcileSecretsRemoved
        | Point::ReconcileWorkspaceDeleted
        | Point::ReconcileCheckpointRefsRemoved
        | Point::ReconcileStagingRemoved
        | Point::ReconcilePublishHandoffPrepared
        | Point::ReconcileCompleted => Scenario::Reconcile,
        point @ (Point::DependencyInvalidated
        | Point::PythonFillBootstrapCreated
        | Point::PythonFillBaselineCaptured
        | Point::PythonReplayBootstrapCreated
        | Point::PythonReplayBaselineCaptured
        | Point::PythonReplayEntryRelocated
        | Point::PythonReplayRelocated
        | Point::PythonWorkspaceEntrySpecialized
        | Point::PythonWorkspaceSpecialized
        | Point::PythonForkEntryRelocated
        | Point::PythonForkRelocated
        | Point::CargoCachedReplayValidated
        | Point::GoProbeCloned
        | Point::GoProbeValidated
        | Point::GoGraphValidated
        | Point::GoOnlineDownloaded
        | Point::GoOnlineVerified
        | Point::GoOfflineDownloaded
        | Point::GoOfflineVerified
        | Point::GoCachedReplayValidated) => {
            Scenario::Provider(dependency_crashes::specific_case(point))
        }
    }
}

fn reconciliation_seed(point: Point) -> (Point, Scenario) {
    match point {
        Point::ReconcileLeasesExpired => (Point::SessionActivated, Scenario::Open),
        Point::ReconcilePublishesRecovered => {
            (Point::PublishLocalApplied, Scenario::PublishResolved)
        }
        Point::ReconcilePublishHandoffPrepared => {
            (Point::PublishCompleted, Scenario::PublishResolved)
        }
        Point::ReconcileWorkspaceTreeRemoved => (Point::ForkCloned, Scenario::Fork),
        Point::ReconcileWorkspaceRefsRemoved => (Point::GcClaimed, Scenario::Gc),
        Point::ReconcileCheckpointRefsRemoved => (Point::CheckpointAnchored, Scenario::Checkpoint),
        Point::ReconcileStagingRemoved => (Point::CloneStaged, Scenario::Fork),
        Point::ReconcileOperationsWritten
        | Point::ReconcileOperationsRecovered
        | Point::ReconcileWorkspaceClaimed
        | Point::ReconcileWorktreeUnlocked
        | Point::ReconcileRegistrationRemoved
        | Point::ReconcileSecretsRemoved
        | Point::ReconcileWorkspaceDeleted
        | Point::ReconcileCompleted => (Point::ForkSecretsCaptured, Scenario::Fork),
        _ => unreachable!("reconciliation point must have a real interruption seed"),
    }
}

fn assert_reconciliation_cut(fixture: &Fixture, point: Point) {
    let db = fixture.database();
    match point {
        Point::ReconcileLeasesExpired => {
            assert_eq!(
                count(&db, "SELECT count(*) FROM sessions WHERE state='dormant'"),
                1
            );
            assert_eq!(
                count(&db, "SELECT count(*) FROM workspaces WHERE state='dormant'"),
                1
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM leases WHERE released_at_ms IS NULL"
                ),
                0
            );
        }
        Point::ReconcileOperationsWritten | Point::ReconcileOperationsRecovered => {
            let committed = point == Point::ReconcileOperationsRecovered;
            assert_eq!(
                strings(
                    &db,
                    "SELECT state FROM operations WHERE idempotency_key='interrupted'"
                ),
                BTreeSet::from([if committed { "failed" } else { "running" }.to_owned()])
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM events e JOIN operations o ON o.id=e.resource WHERE o.idempotency_key='interrupted' AND e.event='operation.recovered'"
                ),
                i64::from(committed)
            );
        }
        Point::ReconcileWorkspaceClaimed => {
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM workspaces WHERE state='recovering'"
                ),
                1
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM leases WHERE released_at_ms IS NULL"
                ),
                1
            );
        }
        Point::ReconcileWorkspaceDeleted => {
            assert_eq!(count(&db, "SELECT count(*) FROM workspaces"), 1);
            assert_eq!(
                count(&db, "SELECT count(*) FROM workspaces WHERE state='ready'"),
                1
            );
        }
        Point::ReconcilePublishesRecovered | Point::ReconcilePublishHandoffPrepared => {
            assert_eq!(
                count(&db, "SELECT count(*) FROM handoffs WHERE state='pending'"),
                1
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM operations WHERE idempotency_key='interrupted' AND state='completed'"
                ),
                1
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM publish_operations WHERE source_kind='resolution_complete' AND state='completed'"
                ),
                1
            );
        }
        _ => {}
    }
}

#[test]
fn real_sigkill_at_every_registered_boundary_recovers_consistently() {
    let selected_point = std::env::var("SHADE_CRASH_POINT").ok();
    if let Some(selected) = &selected_point {
        assert!(
            Point::ALL.iter().any(|point| point.name() == selected),
            "unknown crash point: {selected}"
        );
    }
    let selected_group = std::env::var("SHADE_CRASH_GROUP").ok();
    assert!(
        selected_group.as_deref().is_none_or(|group| matches!(
            group,
            "dependencies"
                | "lifecycle"
                | "resolved-publication"
                | "reconciliation"
                | "dependency-pnpm"
                | "dependency-bun"
                | "dependency-uv"
                | "dependency-cargo"
                | "dependency-go"
        )),
        "unknown crash group"
    );
    // Freeze the instrumented image for the whole matrix. Another Cargo build
    // must not replace the executable between cases or alter its final digest.
    let binary_directory = tempfile::Builder::new()
        .prefix("shade-crash-binary-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let binary = binary_directory.path().join("shade");
    std::fs::copy(env!("CARGO_BIN_EXE_shade"), &binary).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o500)).unwrap();
    let mut results = Vec::new();
    let mut cases: Vec<_> = Point::ALL
        .iter()
        .map(|&point| (point, scenario(point)))
        .collect();
    cases.extend(
        [Point::ScriptDecisionWritten, Point::ScriptDecisionCompleted]
            .map(|point| (point, Scenario::ScriptRevoke)),
    );
    cases.extend(
        [Point::ReviewDecisionWritten, Point::ReviewDecisionCompleted]
            .map(|point| (point, Scenario::SecretKeep)),
    );
    cases.extend(
        [
            Point::SecretsMergeApplied,
            Point::SecretBaselineRemoved,
            Point::SecretBaselineCaptured,
            Point::SecretHandoffWritten,
            Point::SecretHandoffCompleted,
        ]
        .map(|point| (point, Scenario::SecretMergeAdopted)),
    );
    cases.extend(
        [
            Point::RestoreCleaned,
            Point::RestoreWorkingWritten,
            Point::RestoreHeadWritten,
            Point::RestoreIndexWritten,
        ]
        .map(|point| (point, Scenario::Restore)),
    );
    cases.extend(
        [
            Point::ReviewSnapshotStaged,
            Point::ReviewSnapshotPromoted,
            Point::ReviewCreated,
        ]
        .map(|point| (point, Scenario::GcSecrets)),
    );
    cases.extend([Point::CloneStaged, Point::ClonePromoted].map(|point| (point, Scenario::Sync)));
    cases.extend(
        [
            Point::DependenciesRecorded,
            Point::CheckpointObjectsWritten,
            Point::CheckpointAnchored,
            Point::CheckpointRecorded,
            Point::CheckpointHeadRecorded,
            Point::PublishPlanned,
            Point::PublishAnchored,
            Point::PublishPrepared,
            Point::PublishLocalApplied,
            Point::PublishLocalRecorded,
            Point::PublishRemoteApplied,
            Point::PublishRemoteRecorded,
            Point::PublishCompleted,
            Point::PublishAnchorDeleted,
            Point::PublishAnchorCleaned,
            Point::HandoffPrepared,
        ]
        .map(|point| (point, Scenario::PublishResolved)),
    );
    cases.extend(
        dependency_crashes::cases()
            .into_iter()
            .map(|(point, case)| (point, Scenario::Provider(case))),
    );
    for (point, case) in cases {
        if selected_point
            .as_ref()
            .is_some_and(|selected| selected != point.name())
            || selected_group
                .as_deref()
                .is_some_and(|group| !case.in_group(group))
        {
            continue;
        }
        eprintln!("crash matrix: {} ({case:?})", point.name());
        if let Scenario::Provider(case) = case {
            results.push(dependency_crashes::run(&binary, point, case));
            continue;
        }
        let reported_case = case;
        let during_reconciliation = matches!(case, Scenario::Reconcile);
        let (seed, case) = if during_reconciliation {
            reconciliation_seed(point)
        } else {
            (point, case)
        };
        let started = Instant::now();
        let mut fixture = Fixture::new(&binary);
        if point == Point::ReconcileLeasesExpired {
            fixture.lease_ttl_secs = 1;
        }
        if matches!(case, Scenario::IncrementalBase) {
            prepare_incremental_fixture(&fixture.source);
        }
        let registry = matches!(
            case,
            Scenario::ScriptApprove
                | Scenario::ScriptRevoke
                | Scenario::DependencyOpen
                | Scenario::DependencyRefresh
                | Scenario::DependencyBuild
                | Scenario::DependencyGc
        )
        .then(|| {
            let registry =
                script_fixture::add_script_package(&fixture.source, fixture._temp.path());
            git(&fixture.source, &["add", "."]);
            git(&fixture.source, &["commit", "-m", "script fixture"]);
            registry
        });
        let mut daemon = fixture.start(seed);
        let mut original = None;
        let mut secret_originals = Vec::new();
        let content_base =
            serde_json::to_vec(&json!({"api_key":"b7Y2n9W4q1R8d3M6","side":"base"})).unwrap();
        let content_child =
            serde_json::to_vec(&json!({"api_key":"b7Y2n9W4q1R8d3M6","side":"child"})).unwrap();
        let intent = match case {
            Scenario::Open | Scenario::DependencyOpen => fixture.open(),
            Scenario::OpenFailure => json!({"kind":"session_open", "session_id":"failed-session",
                "repository":{"kind":"local","path":fixture.source.join("missing")}}),
            _ => {
                let opened = fixture.execute(fixture.open(), "setup-open");
                let cwd = PathBuf::from(opened["cwd"].as_str().unwrap());
                std::fs::write(cwd.join("tracked.txt"), "staged\n").unwrap();
                git(&cwd, &["add", "tracked.txt"]);
                std::fs::write(cwd.join("tracked.txt"), "working\n").unwrap();
                std::fs::write(cwd.join("untracked.txt"), "untracked\n").unwrap();
                if !matches!(
                    case,
                    Scenario::Gc | Scenario::DependencyGc | Scenario::Sleep | Scenario::Wake
                ) {
                    original = Some((cwd, opened["workspace"].clone()));
                }
                let selector = json!({"workspace_id":opened["workspace"]});
                match case {
                    Scenario::Open
                    | Scenario::DependencyOpen
                    | Scenario::OpenFailure
                    | Scenario::Reconcile
                    | Scenario::Provider(_) => {
                        unreachable!()
                    }
                    Scenario::Checkpoint => {
                        json!({"kind":"workspace_checkpoint","selector":selector,"reason":"crash"})
                    }
                    Scenario::Fork => {
                        json!({"kind":"workspace_fork","selector":selector,"child_session_id":"child-session"})
                    }
                    Scenario::Refresh | Scenario::DependencyRefresh => {
                        json!({"kind":"dependencies_refresh","selector":selector})
                    }
                    Scenario::DependencyBuild => {
                        let report = try_rpc(
                            &fixture.socket,
                            &query(json!({"kind":"dependency_scripts","selector":selector})),
                        )
                        .unwrap();
                        let approval =
                            &report["outcome"]["result"]["scripts"][0]["script"]["approval"];
                        assert!(approval.is_object(), "{report}");
                        fixture.execute(json!({"kind":"dependency_script_decision","selector":selector,"approval":approval,"allow":true}), "setup-approval");
                        json!({"kind":"dependencies_refresh","selector":selector})
                    }
                    Scenario::Restore => {
                        let checkpoint = fixture.execute(json!({"kind":"workspace_checkpoint","selector":selector,"reason":"restore-fixture"}), "setup-checkpoint");
                        json!({"kind":"workspace_restore","selector":selector,"checkpoint_id":checkpoint["checkpoint_id"]})
                    }
                    Scenario::Adopt => {
                        let handoff = fixture.execute(
                            json!({"kind":"dependencies_refresh","selector":selector}),
                            "setup-handoff",
                        );
                        json!({"kind":"successor_adopt","handoff_id":handoff["handoff_id"]})
                    }
                    Scenario::Sync | Scenario::SyncConflict | Scenario::IncrementalBase => {
                        if matches!(case, Scenario::IncrementalBase) {
                            update_incremental_fixture(&fixture.source);
                        }
                        if matches!(case, Scenario::SyncConflict) {
                            std::fs::write(fixture.source.join("tracked.txt"), "upstream\n")
                                .unwrap();
                        } else {
                            std::fs::write(fixture.source.join("upstream.txt"), "upstream\n")
                                .unwrap();
                        }
                        git(&fixture.source, &["add", "."]);
                        git(&fixture.source, &["commit", "-m", "upstream"]);
                        json!({"kind":"workspace_sync","selector":selector})
                    }
                    Scenario::Publish => {
                        json!({"kind":"workspace_publish","selector":selector,"branch":"published","message":"squash crash fixture","push":true})
                    }
                    Scenario::PublishConflict | Scenario::PublishResolved => {
                        git(&fixture.source, &["checkout", "-b", "published"]);
                        std::fs::write(fixture.source.join("tracked.txt"), "remote conflict\n")
                            .unwrap();
                        git(&fixture.source, &["add", "tracked.txt"]);
                        git(&fixture.source, &["commit", "-m", "publish conflict"]);
                        git(&fixture.source, &["checkout", "main"]);
                        let publish = json!({"kind":"workspace_publish","selector":selector,"branch":"published","message":"resolved crash fixture","push":true});
                        if matches!(case, Scenario::PublishResolved) {
                            let conflict = fixture.execute(publish, "setup-publish-conflict");
                            let resolution = Path::new(conflict["cwd"].as_str().unwrap());
                            std::fs::write(resolution.join("tracked.txt"), "resolved\n").unwrap();
                            git(resolution, &["add", "tracked.txt"]);
                            json!({"kind":"resolution_complete","selector":{"workspace_id":conflict["workspace"]}})
                        } else {
                            publish
                        }
                    }
                    Scenario::Release => json!({"kind":"workspace_release","selector":selector}),
                    Scenario::Sleep => {
                        json!({"kind":"workspace_sleep","selector":selector})
                    }
                    Scenario::Wake => {
                        fixture.execute(
                            json!({"kind":"workspace_sleep","selector":selector}),
                            "setup-sleep",
                        );
                        json!({"kind":"session_wake","session_id":"crash-session"})
                    }
                    Scenario::ScriptApprove | Scenario::ScriptRevoke => {
                        let report = try_rpc(
                            &fixture.socket,
                            &query(json!({"kind":"dependency_scripts","selector":selector})),
                        )
                        .unwrap();
                        let approval =
                            &report["outcome"]["result"]["scripts"][0]["script"]["approval"];
                        assert!(approval.is_object(), "{report}");
                        assert!(registry.as_ref().unwrap().requests() > 0);
                        if matches!(case, Scenario::ScriptRevoke) {
                            fixture.execute(json!({"kind":"dependency_script_decision","selector":selector,"approval":approval,"allow":true}), "setup-approval");
                        }
                        json!({"kind":"dependency_script_decision","selector":selector,"approval":approval,"allow":matches!(case, Scenario::ScriptApprove)})
                    }
                    Scenario::GcSecrets => {
                        fixture.execute(
                            json!({"kind":"workspace_release","selector":selector}),
                            "setup-release",
                        );
                        let secret =
                            PathBuf::from(opened["cwd"].as_str().unwrap()).join(".env.local");
                        std::fs::write(&secret, "TOKEN=late\n").unwrap();
                        secret_originals.push((secret, b"TOKEN=late\n".to_vec()));
                        let secret =
                            PathBuf::from(opened["cwd"].as_str().unwrap()).join("private.json");
                        std::fs::write(&secret, &content_child).unwrap();
                        secret_originals.push((secret, content_child.clone()));
                        json!({"kind":"garbage_collect"})
                    }
                    Scenario::SecretReview
                    | Scenario::SecretDiscard
                    | Scenario::SecretKeep
                    | Scenario::SecretMerge
                    | Scenario::SecretMergeAdopted => {
                        let parent_env =
                            PathBuf::from(opened["cwd"].as_str().unwrap()).join(".env.local");
                        std::fs::write(&parent_env, "TOKEN=base\n").unwrap();
                        let parent_content =
                            PathBuf::from(opened["cwd"].as_str().unwrap()).join("private.json");
                        std::fs::write(&parent_content, &content_base).unwrap();
                        let child = if matches!(case, Scenario::SecretMergeAdopted) {
                            let handoff = fixture.execute(
                                json!({"kind":"dependencies_refresh","selector":selector}),
                                "setup-secret-successor",
                            );
                            fixture.execute(json!({"kind":"successor_adopt","handoff_id":handoff["handoff_id"]}), "setup-secret-adopt")
                        } else {
                            fixture.execute(json!({"kind":"workspace_fork","selector":selector,"child_session_id":"secret-child"}), "setup-secret-fork")
                        };
                        let child_env =
                            PathBuf::from(child["cwd"].as_str().unwrap()).join(".env.local");
                        std::fs::write(&child_env, "TOKEN=child\n").unwrap();
                        let child_content =
                            PathBuf::from(child["cwd"].as_str().unwrap()).join("private.json");
                        std::fs::write(&child_content, &content_child).unwrap();
                        secret_originals.push((parent_env, b"TOKEN=base\n".to_vec()));
                        secret_originals.push((child_env, b"TOKEN=child\n".to_vec()));
                        secret_originals.push((parent_content, content_base.clone()));
                        secret_originals.push((child_content, content_child.clone()));
                        let release = json!({"kind":"workspace_release","selector":{"workspace_id":child["workspace"]}});
                        if matches!(case, Scenario::SecretReview) {
                            release
                        } else {
                            let review = fixture.execute(release, "setup-secret-review");
                            let action = match case {
                                Scenario::SecretKeep => "keep",
                                Scenario::SecretDiscard => "discard",
                                _ => "merge_parent",
                            };
                            json!({"kind":"review_resolve","review_id":review["review_id"],"action":action})
                        }
                    }
                    Scenario::Gc => {
                        fixture.execute(
                            json!({"kind":"workspace_release","selector":selector}),
                            "setup-release",
                        );
                        json!({"kind":"garbage_collect"})
                    }
                    Scenario::DependencyGc => {
                        fixture.execute(
                            json!({"kind":"workspace_release","selector":selector}),
                            "setup-release",
                        );
                        let artifacts = fixture.root.join("dependencies/artifacts/npm");
                        let artifact = std::fs::read_dir(artifacts)
                            .unwrap()
                            .next()
                            .unwrap()
                            .unwrap()
                            .path();
                        // Exercise the production byte ceiling with logical sparse
                        // pressure, keeping physical fixture allocation bounded.
                        std::fs::File::create(artifact.join("pressure"))
                            .unwrap()
                            .set_len(shade_engine::dependencies::DEFAULT_DEPENDENCY_CACHE_BYTES + 1)
                            .unwrap();
                        std::thread::sleep(Duration::from_millis(2));
                        json!({"kind":"garbage_collect"})
                    }
                }
            }
        };
        fixture.arm();
        let mut stream = UnixStream::connect(&fixture.socket).unwrap();
        serde_json::to_writer(&mut stream, &execute(intent.clone(), "interrupted")).unwrap();
        stream.write_all(b"\n").unwrap();
        // Cold package preparation is deliberately outside the latency gate.
        // Wait for the named phase under host load before sending SIGKILL.
        fixture.interrupt_at(&mut daemon, seed, if registry.is_some() { 120 } else { 15 });
        let database_inode = std::fs::metadata(fixture.root.join("state.sqlite"))
            .unwrap()
            .ino();
        drop(stream);
        fixture.disarm();
        if during_reconciliation {
            if point == Point::ReconcileLeasesExpired {
                // Use the supported isolated-harness TTL and actual elapsed
                // time; never fabricate expired rows by modifying SQLite.
                std::thread::sleep(Duration::from_millis(1100));
            }
            fixture.arm();
            let mut recovering = fixture.spawn(point);
            fixture.interrupt_at(&mut recovering, point, 20);
            assert_eq!(
                std::fs::metadata(fixture.root.join("state.sqlite"))
                    .unwrap()
                    .ino(),
                database_inode
            );
            assert_reconciliation_cut(&fixture, point);
            fixture.disarm();
        }
        fixture.lease_ttl_secs = 120;
        let _restarted = fixture.start(point);
        assert_eq!(
            std::fs::metadata(fixture.root.join("state.sqlite"))
                .unwrap()
                .ino(),
            database_inode
        );
        fixture.assert_consistent();
        if point == Point::ReconcileLeasesExpired {
            // An expired lease is not garbage any more. A collector round must
            // leave the dormant workspace exactly where reconciliation left it;
            // only the explicit release below can still reach it.
            std::thread::sleep(Duration::from_millis(2));
            fixture.execute(json!({"kind":"garbage_collect"}), "sweep-dormant-session");
            assert_eq!(
                count(
                    &fixture.database(),
                    "SELECT count(*) FROM workspaces WHERE state='dormant'"
                ),
                1,
                "the collector took a dormant workspace it must not touch"
            );
            assert_eq!(
                count(&fixture.database(), "SELECT count(*) FROM workspaces"),
                1
            );
        }
        assert_eq!(
            count(
                &fixture.database(),
                "SELECT count(*) FROM (SELECT resource FROM events WHERE event='operation.recovered' GROUP BY resource HAVING count(*)>1)"
            ),
            0,
            "restarting recovery duplicated an interruption event"
        );
        if matches!(case, Scenario::IncrementalBase) {
            assert_incremental_bases(&fixture, point == Point::IncrementalBasePromoted);
            // A new operation retries the failed sync; the original key remains
            // durably interrupted. The old workspace retains all user edits.
            fixture.execute(intent.clone(), "retry-incremental");
            assert_incremental_bases(&fixture, true);
        }
        if matches!(case, Scenario::PublishConflict) {
            assert_eq!(
                count(
                    &fixture.database(),
                    "SELECT count(*) FROM workspaces w LEFT JOIN publish_resolutions p ON p.workspace_id=w.id WHERE w.state='resolution' AND p.workspace_id IS NULL"
                ),
                0,
                "publish resolution became visible without its durable publication intent"
            );
            if count(
                &fixture.database(),
                "SELECT count(*) FROM publish_resolutions WHERE state='pending'",
            ) == 0
            {
                fixture.execute(intent.clone(), "retry-publish-conflict");
            }
            assert_eq!(
                count(
                    &fixture.database(),
                    "SELECT count(*) FROM publish_resolutions WHERE state='pending'"
                ),
                1
            );
            assert_eq!(
                git(
                    &fixture.source,
                    &["show", "refs/heads/published:tracked.txt"]
                ),
                "remote conflict"
            );
            let bare = strings(&fixture.database(), "SELECT bare_path FROM repositories")
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(
                git(
                    Path::new(&bare),
                    &[
                        "for-each-ref",
                        "--format=%(refname)",
                        "refs/heads/published"
                    ]
                ),
                "",
                "conflicted publish moved the local branch"
            );
        }
        if matches!(case, Scenario::OpenFailure) {
            let record = try_rpc(&fixture.socket, &query(json!({"kind":"operation_by_key", "actor_kind":"agent", "actor_id":"crash-matrix", "idempotency_key":"interrupted"}))).unwrap();
            let record = &record["outcome"]["result"];
            assert_eq!(record["state"], "failed", "{record}");
            let committed = point == Point::OperationFailed;
            assert_eq!(
                record["error"]["code"],
                if committed {
                    "INTERNAL"
                } else {
                    "OPERATION_INTERRUPTED"
                }
            );
            assert_eq!(
                count(&fixture.database(), "SELECT count(*) FROM diagnostics"),
                i64::from(committed)
            );
            if committed {
                let id = record["error"]["diagnostics_id"].as_str().unwrap();
                let diagnostic = try_rpc(
                    &fixture.socket,
                    &query(json!({"kind":"diagnostics", "diagnostics_id":id})),
                )
                .unwrap();
                assert_eq!(diagnostic["outcome"]["result"]["operation"], record["id"]);
                assert!(
                    !diagnostic["outcome"]["result"]["message"]
                        .as_str()
                        .unwrap()
                        .is_empty()
                );
            }
            let _ = try_rpc(&fixture.socket, &execute(intent.clone(), "interrupted")).unwrap();
            let replay = try_rpc(
                &fixture.socket,
                &query(json!({"kind":"operation", "operation_id":record["id"]})),
            )
            .unwrap();
            assert_eq!(replay["outcome"]["result"]["error"], record["error"]);
        }
        if count(
            &fixture.database(),
            "SELECT count(*) FROM operations WHERE idempotency_key='interrupted' AND state='completed'",
        ) == 1
        {
            fixture.execute(intent.clone(), "interrupted");
        }
        if matches!(case, Scenario::Sleep) {
            let db = fixture.database();
            let path = strings(&db, "SELECT path FROM workspaces")
                .into_iter()
                .next()
                .unwrap();
            if matches!(point, Point::SleepCheckpointed | Point::SleepSecretsVaulted) {
                // The tree is still registered and still owned, so the honest
                // place to leave the workspace is where it was.
                assert_eq!(
                    strings(&db, "SELECT state FROM workspaces"),
                    BTreeSet::from(["ready".to_owned()])
                );
                assert_eq!(
                    std::fs::read(Path::new(&path).join("untracked.txt")).unwrap(),
                    b"untracked\n"
                );
                fixture.execute(intent.clone(), "retry-sleep");
            } else {
                // The registration or the tree is already gone; the
                // `suspending` pass finishes the suspension at startup.
                assert_eq!(
                    strings(&db, "SELECT state FROM workspaces"),
                    BTreeSet::from(["suspended".to_owned()]),
                    "reconciliation left an interrupted suspension unfinished"
                );
            }
            let db = fixture.database();
            assert_eq!(
                strings(&db, "SELECT state FROM workspaces"),
                BTreeSet::from(["suspended".to_owned()])
            );
            assert_eq!(
                strings(&db, "SELECT state FROM sessions"),
                BTreeSet::from(["suspended".to_owned()])
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM leases WHERE released_at_ms IS NULL"
                ),
                0,
                "a suspended session kept a live lease"
            );
            assert!(
                !Path::new(&path).exists(),
                "a suspended workspace kept its tree"
            );
            // The whole point of the state: everything that was on disk comes
            // back, including the file Git never tracked.
            let woken = fixture.execute(
                json!({"kind":"session_wake","session_id":"crash-session"}),
                "wake-after-sleep",
            );
            let cwd = PathBuf::from(woken["cwd"].as_str().unwrap());
            assert_ne!(cwd, PathBuf::from(&path), "wake produces a successor");
            assert_eq!(
                std::fs::read(cwd.join("untracked.txt")).unwrap(),
                b"untracked\n"
            );
            assert_eq!(
                std::fs::read(cwd.join("tracked.txt")).unwrap(),
                b"working\n"
            );
        }
        if matches!(case, Scenario::Wake) {
            let db = fixture.database();
            if point == Point::WakeMaterialized {
                // The suspension the successor was built from is untouched,
                // and the half-built successor is an ordinary incomplete
                // workspace that reconciliation already removed.
                assert_eq!(
                    strings(&db, "SELECT state FROM workspaces"),
                    BTreeSet::from(["suspended".to_owned()]),
                    "an interrupted wake left an orphan successor"
                );
                assert_eq!(
                    strings(&db, "SELECT state FROM sessions"),
                    BTreeSet::from(["suspended".to_owned()])
                );
                assert_eq!(
                    count(
                        &db,
                        "SELECT count(*) FROM operations WHERE idempotency_key='interrupted' AND state='completed'"
                    ),
                    0
                );
                fixture.execute(intent.clone(), "retry-wake");
            } else {
                // The successor was bound in the same transaction that wrote
                // the journal, so the replay above returned the recorded
                // outcome instead of waking a second time.
                assert_eq!(
                    count(
                        &db,
                        "SELECT count(*) FROM operations WHERE idempotency_key='interrupted' AND state='completed'"
                    ),
                    1
                );
            }
            let db = fixture.database();
            assert_eq!(
                strings(&db, "SELECT state FROM workspaces"),
                BTreeSet::from(["ready".to_owned(), "released".to_owned()]),
                "wake releases the predecessor as it binds the successor"
            );
            assert_eq!(
                strings(&db, "SELECT state FROM sessions"),
                BTreeSet::from(["active".to_owned()])
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM leases WHERE released_at_ms IS NULL"
                ),
                1
            );
            let cwd = strings(&db, "SELECT path FROM workspaces WHERE state='ready'")
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(
                std::fs::read(Path::new(&cwd).join("untracked.txt")).unwrap(),
                b"untracked\n"
            );
            assert_eq!(
                std::fs::read(Path::new(&cwd).join("tracked.txt")).unwrap(),
                b"working\n"
            );
        }
        if matches!(case, Scenario::ScriptApprove | Scenario::ScriptRevoke) {
            let committed = point == Point::ScriptDecisionCompleted;
            let allowed_before = matches!(case, Scenario::ScriptRevoke);
            let allowed = if committed {
                !allowed_before
            } else {
                allowed_before
            };
            assert_eq!(
                count(
                    &fixture.database(),
                    "SELECT count(*) FROM script_approvals WHERE allowed=1"
                ),
                i64::from(allowed)
            );
            assert_eq!(
                count(
                    &fixture.database(),
                    "SELECT count(*) FROM events WHERE event='dependency.script_decided'"
                ),
                i64::from(allowed_before) + i64::from(committed)
            );
            assert_eq!(
                count(
                    &fixture.database(),
                    "SELECT count(*) FROM operations WHERE idempotency_key='interrupted' AND state='completed'"
                ),
                i64::from(committed)
            );
            if !committed {
                fixture.execute(intent.clone(), "retry-script-decision");
            }
            assert_eq!(
                count(
                    &fixture.database(),
                    "SELECT count(*) FROM script_approvals WHERE allowed=1"
                ),
                i64::from(!allowed_before)
            );
        }
        for (path, bytes) in &secret_originals {
            assert_eq!(
                &std::fs::read(path).unwrap(),
                bytes,
                "secret predecessor changed"
            );
        }
        if let Some((cwd, _)) = &original {
            assert_eq!(
                std::fs::read(cwd.join("tracked.txt")).unwrap(),
                b"working\n"
            );
            assert_eq!(
                std::fs::read(cwd.join("untracked.txt")).unwrap(),
                b"untracked\n"
            );
            assert_eq!(git(cwd, &["show", ":tracked.txt"]), "staged");
        }
        if matches!(case, Scenario::Open | Scenario::DependencyOpen) {
            let opened = fixture.execute(fixture.open(), "reopen");
            if matches!(case, Scenario::DependencyOpen) {
                let package =
                    Path::new(opened["cwd"].as_str().unwrap()).join("node_modules/approved-alias");
                assert!(package.join("package.json").is_file());
                assert!(
                    !package.join("built.txt").exists(),
                    "unapproved script executed after recovery"
                );
                assert!(registry.as_ref().unwrap().requests() > 0);
            }
            assert_eq!(
                count(&fixture.database(), "SELECT count(*) FROM workspaces"),
                1
            );
        }
        if matches!(case, Scenario::Publish) {
            let db = fixture.database();
            let commit: String = db
                .query_row(
                    "SELECT commit_oid FROM publish_operations WHERE state='completed'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let bare = strings(&db, "SELECT bare_path FROM repositories")
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(
                git(Path::new(&bare), &["rev-parse", "refs/heads/published"]),
                commit
            );
            assert_eq!(
                git(&fixture.source, &["rev-parse", "refs/heads/published"]),
                commit
            );
            assert_eq!(
                git(
                    &fixture.source,
                    &["for-each-ref", "--format=%(refname)", "refs/shade/"]
                ),
                ""
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM publish_operations WHERE anchor_cleaned=0"
                ),
                0
            );
        }
        // Retry the chosen action if its transaction was interrupted.
        for id in strings(
            &fixture.database(),
            "SELECT id FROM reviews WHERE state='pending'",
        ) {
            let action = match case {
                Scenario::SecretKeep => "keep",
                Scenario::SecretMerge | Scenario::SecretMergeAdopted => "merge_parent",
                _ => "discard",
            };
            fixture.execute(
                json!({"kind":"review_resolve","review_id":id,"action":action}),
                "retry-secret-choice",
            );
        }
        if matches!(case, Scenario::SecretMerge | Scenario::SecretMergeAdopted) {
            let paths = strings(
                &fixture.database(),
                "SELECT w.path FROM workspaces w JOIN handoffs h ON h.successor_workspace_id=w.id WHERE h.state='pending'",
            );
            assert_eq!(paths.len(), 1);
            assert_eq!(
                std::fs::read(Path::new(paths.first().unwrap()).join(".env.local")).unwrap(),
                b"TOKEN=child\n"
            );
            assert_eq!(
                std::fs::read(Path::new(paths.first().unwrap()).join("private.json")).unwrap(),
                content_child
            );
        }
        // Finish preserved conflict/handoff resources through the public seam.
        for path in strings(
            &fixture.database(),
            "SELECT path FROM workspaces WHERE state='resolution'",
        ) {
            std::fs::write(Path::new(&path).join("tracked.txt"), "resolved\n").unwrap();
            git(Path::new(&path), &["add", "tracked.txt"]);
            fixture.execute(
                json!({"kind":"resolution_complete","selector":{"cwd":path}}),
                "resolve-conflict",
            );
        }
        for id in strings(
            &fixture.database(),
            "SELECT id FROM handoffs WHERE state='pending'",
        ) {
            fixture.execute(
                json!({"kind":"successor_adopt","handoff_id":id}),
                "adopt-pending",
            );
        }
        if matches!(case, Scenario::PublishConflict | Scenario::PublishResolved) {
            let db = fixture.database();
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM publish_resolutions WHERE state='completed'"
                ),
                1
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM publish_operations WHERE source_kind='resolution_complete' AND state='completed'"
                ),
                1
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT count(*) FROM publish_operations WHERE anchor_cleaned=0"
                ),
                0
            );
            let bare = strings(&db, "SELECT bare_path FROM repositories")
                .into_iter()
                .next()
                .unwrap();
            let commit = git(Path::new(&bare), &["rev-parse", "refs/heads/published"]);
            assert_eq!(
                commit,
                git(&fixture.source, &["rev-parse", "refs/heads/published"])
            );
            assert_eq!(
                git(
                    &fixture.source,
                    &["show", "refs/heads/published:tracked.txt"]
                ),
                "resolved"
            );
            assert_eq!(
                git(
                    &fixture.source,
                    &["rev-list", "--parents", "-n", "1", &commit]
                )
                .split_whitespace()
                .count(),
                2,
                "resolved publish must squash to one parent"
            );
        }
        fixture.assert_consistent();
        assert_dependency_artifacts_clean(&fixture.root);
        for id in strings(
            &fixture.database(),
            // A dormant session still owns a tree, so the cleanup has to
            // release it as deliberately as an active one.
            "SELECT workspace_id FROM sessions WHERE state IN ('active','dormant')",
        ) {
            let result = fixture.execute(
                json!({"kind":"workspace_release","selector":{"workspace_id":id}}),
                &format!("release-{id}"),
            );
            if let Some(review) = result.get("review_id") {
                fixture.execute(
                    json!({"kind":"review_resolve","review_id":review,"action":"discard"}),
                    &format!("cleanup-review-{id}"),
                );
            }
        }
        for attempt in 0..4 {
            // The zero-second harness grace still uses a strict millisecond
            // cutoff. Advance past release/review's timestamp before GC.
            std::thread::sleep(Duration::from_millis(2));
            fixture.execute(json!({"kind":"garbage_collect"}), &format!("gc-{attempt}"));
            let reviews = strings(
                &fixture.database(),
                "SELECT id FROM reviews WHERE state='pending'",
            );
            if reviews.is_empty() {
                break;
            }
            for id in reviews {
                fixture.execute(
                    json!({"kind":"review_resolve","review_id":id,"action":"discard"}),
                    &format!("gc-review-{id}"),
                );
            }
        }
        fixture.assert_consistent();
        let retained = count(
            &fixture.database(),
            "SELECT count(*) FROM workspaces WHERE state='retained'",
        );
        assert_eq!(retained, i64::from(matches!(case, Scenario::SecretKeep)));
        assert_eq!(
            count(&fixture.database(), "SELECT count(*) FROM workspaces"),
            retained,
            "GC left workspace states {:?}",
            strings(
                &fixture.database(),
                "SELECT state || ':' || updated_at_ms FROM workspaces"
            )
        );
        assert_eq!(
            count(
                &fixture.database(),
                "SELECT count(*) FROM leases WHERE released_at_ms IS NULL"
            ),
            0
        );
        assert_eq!(
            count(
                &fixture.database(),
                "SELECT count(*) FROM reviews WHERE state='pending'"
            ),
            0
        );
        assert_eq!(
            count(
                &fixture.database(),
                "SELECT count(*) FROM handoffs WHERE state='pending'"
            ),
            0
        );
        results.push(json!({"point":point.name(), "scenario":format!("{reported_case:?}"), "seed_point":seed.name(), "seed_scenario":format!("{case:?}"), "signal":"SIGKILL", "sigkill_count":if during_reconciliation {2} else {1}, "restart":"same_pool_and_sqlite_inode", "consistent":true, "cleanup_workspaces":retained, "retained_by_user_choice":retained, "elapsed_ms":started.elapsed().as_millis()}));
    }
    assert!(
        !results.is_empty(),
        "crash selection did not execute a case"
    );
    if let Some(path) = std::env::var_os("SHADE_CRASH_REPORT") {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&json!({
                "schema_version":1, "status":"passed", "scope":"lifecycle_and_dependency_artifacts",
                "os":std::env::consts::OS, "architecture":std::env::consts::ARCH,
                "binary_sha256":hex::encode(Sha256::digest(std::fs::read(&binary).unwrap())),
                "fault_injection_feature":true, "content_secret_fixtures":true, "filtered":selected_point, "filtered_group":selected_group,
                "catalog_points":Point::ALL.len(), "case_count":results.len(), "cases":results
                ,"cold_preparation_rendezvous_timeout_secs":120
            }))
            .unwrap(),
        )
        .unwrap();
    }
}

fn assert_dependency_artifacts_clean(root: &Path) {
    let staging = root.join("dependencies/staging");
    if staging.exists() {
        assert_eq!(
            std::fs::read_dir(staging).unwrap().count(),
            0,
            "orphan dependency staging"
        );
    }
    let artifacts = root.join("dependencies/artifacts");
    if !artifacts.exists() {
        return;
    }
    for provider in std::fs::read_dir(artifacts).unwrap() {
        for artifact in std::fs::read_dir(provider.unwrap().path()).unwrap() {
            let path = artifact.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(
                name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "orphan dependency promotion: {}",
                path.display()
            );
            let receipt: Value =
                serde_json::from_slice(&std::fs::read(path.join("receipt.json")).unwrap()).unwrap();
            assert_eq!(receipt["fingerprint"], name);
            assert_eq!(receipt["state"], "ready");
            for relative in receipt["materialized_paths"].as_array().unwrap() {
                assert!(
                    path.join("payload")
                        .join(relative.as_str().unwrap())
                        .is_dir(),
                    "completed receipt lost its payload"
                );
            }
        }
    }
}
