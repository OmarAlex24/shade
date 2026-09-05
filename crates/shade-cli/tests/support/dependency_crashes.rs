use super::dependency_fixture::{self as data, Manager};
use super::{
    Fixture, Point, assert_dependency_artifacts_clean, count, execute, git, query, strings, try_rpc,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Open,
    Refresh,
    Build,
    Gc,
    CacheHit,
    Refill,
    Corrupt,
    Fork,
}

#[derive(Debug, Clone, Copy)]
pub struct Case {
    pub manager: Manager,
    pub action: Action,
}

pub fn specific_case(point: Point) -> Case {
    use Action::*;
    let (manager, action) = match point {
        Point::PythonForkEntryRelocated | Point::PythonForkRelocated => (Manager::Uv, Fork),
        Point::PythonFillBootstrapCreated
        | Point::PythonFillBaselineCaptured
        | Point::PythonReplayBootstrapCreated
        | Point::PythonReplayBaselineCaptured
        | Point::PythonReplayEntryRelocated
        | Point::PythonReplayRelocated
        | Point::PythonWorkspaceEntrySpecialized
        | Point::PythonWorkspaceSpecialized => (Manager::Uv, Open),
        Point::CargoCachedReplayValidated => (Manager::Cargo, CacheHit),
        Point::GoCachedReplayValidated => (Manager::Go, CacheHit),
        Point::DependencyInvalidated => (Manager::Cargo, Refill),
        Point::GoProbeCloned
        | Point::GoProbeValidated
        | Point::GoGraphValidated
        | Point::GoOnlineDownloaded
        | Point::GoOnlineVerified
        | Point::GoOfflineDownloaded
        | Point::GoOfflineVerified => (Manager::Go, Open),
        _ => unreachable!("provider-specific point needs a concrete case"),
    };
    Case { manager, action }
}

pub fn cases() -> Vec<(Point, Case)> {
    let mut result = Vec::new();
    for manager in [Manager::Pnpm, Manager::Bun, Manager::Uv] {
        for point in [
            Point::DependencyStaged,
            Point::DependencyFilled,
            Point::DependencyFillValidated,
            Point::DependencyReplayed,
            Point::DependencyReplayValidated,
            Point::DependencyPromotionStaged,
            Point::DependencyPayloadMoved,
            Point::DependencyReceiptWritten,
            Point::DependencyPromoted,
            Point::DependencyCloneStaged,
            Point::DependencyMaterialized,
            Point::DependencyReceiptRecorded,
            Point::DependenciesRecorded,
        ] {
            result.push((
                point,
                Case {
                    manager,
                    action: Action::Open,
                },
            ));
        }
        for point in [
            Point::DependencyExistingStaged,
            Point::DependencyBackupRemoved,
        ] {
            result.push((
                point,
                Case {
                    manager,
                    action: Action::Refresh,
                },
            ));
        }
        for point in [
            Point::DependencyGcRenamed,
            Point::DependencyGcPayloadRemoved,
            Point::DependencyGcDeleted,
        ] {
            result.push((
                point,
                Case {
                    manager,
                    action: Action::Gc,
                },
            ));
        }
        result.push((
            Point::DependencyInvalidated,
            Case {
                manager,
                action: Action::Corrupt,
            },
        ));
        if manager != Manager::Uv {
            result.push((
                Point::DependencyScriptsExecuted,
                Case {
                    manager,
                    action: Action::Build,
                },
            ));
        }
    }
    for manager in [Manager::Cargo, Manager::Go] {
        for point in [
            Point::DependencyFilled,
            Point::DependencyFillValidated,
            Point::DependencyReplayed,
            Point::DependencyReplayValidated,
            Point::DependencyPromotionStaged,
            Point::DependencyReceiptWritten,
            Point::DependencyPromoted,
            Point::DependencyReceiptRecorded,
            Point::DependenciesRecorded,
        ] {
            result.push((
                point,
                Case {
                    manager,
                    action: Action::Open,
                },
            ));
        }
    }
    result.push((
        Point::DependencyStaged,
        Case {
            manager: Manager::Go,
            action: Action::Open,
        },
    ));
    result.push((
        Point::DependencyInvalidated,
        Case {
            manager: Manager::Go,
            action: Action::Refill,
        },
    ));
    result
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, String> {
    data::files(root)
        .into_iter()
        .map(|path| {
            let digest = hex::encode(Sha256::digest(fs::read(&path).unwrap()));
            (path.strip_prefix(root).unwrap().to_owned(), digest)
        })
        .collect()
}

fn artifact(fixture: &Fixture, manager: Manager, workspace: &Value) -> PathBuf {
    let db = fixture.database();
    let fingerprint: String = db
        .query_row(
            "SELECT fingerprint FROM dependency_receipts WHERE workspace_id=?1 AND provider=?2",
            rusqlite::params![workspace.as_str().unwrap(), manager.name()],
            |row| row.get(0),
        )
        .unwrap();
    fixture
        .root
        .join("dependencies/artifacts")
        .join(manager.name())
        .join(fingerprint)
}

fn assert_ready(fixture: &Fixture, workspace: &Path, case: Case, forked: bool) {
    assert!(!workspace.join("root-ran").exists());
    assert!(!workspace.join("target").exists());
    match case.manager {
        Manager::Pnpm | Manager::Bun => {
            let package = workspace.join("node_modules/approved-alias");
            assert!(package.join("package.json").is_file());
            assert_eq!(
                package.join("built.txt").is_file(),
                case.action == Action::Build
            );
            if case.action == Action::Build {
                assert_eq!(
                    fs::read_to_string(package.join("built.txt")).unwrap(),
                    "1.0.0"
                );
            }
            assert_eq!(
                fs::canonicalize(workspace.join("node_modules/shade-member")).unwrap(),
                workspace.join("packages/member")
            );
        }
        Manager::Uv => {
            let entry = workspace.join(".venv/bin/shade-fixture");
            assert_eq!(
                data::command(&fixture.source, entry.to_str().unwrap(), &[]),
                if forked { "84" } else { "42" }
            );
        }
        Manager::Cargo => {
            assert!(
                data::files(&fixture.root.join("runtime/dependency-native/cargo"))
                    .iter()
                    .any(|path| path.ends_with("shade-registry-fixture-1.0.0/Cargo.toml"))
            );
        }
        Manager::Go => {
            assert!(
                fixture
                    .root
                    .join("runtime/dependency-native/go/mod/golang.org/x/text@v0.3.8")
                    .is_dir()
            );
        }
    }
    assert!(
        !data::files(&fixture.root)
            .iter()
            .any(|path| path.file_name().is_some_and(|name| name == "root-ran")),
        "a root/dependency build or source hook ran"
    );
}

pub fn run(binary: &Path, point: Point, case: Case) -> Value {
    let started = Instant::now();
    let fixture = Fixture::new(binary);
    let registry = data::prepare(&fixture.source, fixture._temp.path(), case.manager);
    git(&fixture.source, &["add", "."]);
    git(&fixture.source, &["commit", "-m", "provider crash fixture"]);
    let inputs: BTreeMap<_, _> = git(&fixture.source, &["ls-files", "-z"])
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(|name| {
            (
                name.to_owned(),
                fs::read(fixture.source.join(name)).unwrap(),
            )
        })
        .collect();
    let requests_before = registry.as_ref().map(|registry| registry.requests());
    let mut daemon = fixture.start(point);
    let mut original = None;
    let mut original_bytes = None;
    let mut intent = fixture.open();
    if case.action != Action::Open {
        let opened = fixture.execute(fixture.open(), "setup-provider");
        let cwd = PathBuf::from(opened["cwd"].as_str().unwrap());
        let selector = json!({"workspace_id":opened["workspace"]});
        if case.action != Action::Gc {
            original_bytes = Some(snapshot(&cwd));
            original = Some(cwd.clone());
        }
        if case.action == Action::Fork {
            let module = data::files(&cwd.join(".venv"))
                .into_iter()
                .find(|path| {
                    path.file_name()
                        .is_some_and(|name| name == "shade_registry_fixture.py")
                })
                .unwrap();
            let content = fs::read_to_string(&module)
                .unwrap()
                .replace("VALUE = 42", "VALUE = 84");
            fs::write(module, content).unwrap();
            intent = json!({"kind":"workspace_fork","selector":selector,"child_session_id":"provider-child"});
        } else if case.action == Action::Gc {
            let path = artifact(&fixture, case.manager, &opened["workspace"]);
            fixture.execute(
                json!({"kind":"workspace_release","selector":selector}),
                "setup-release",
            );
            fs::File::create(path.join("pressure"))
                .unwrap()
                .set_len(shade_engine::dependencies::DEFAULT_DEPENDENCY_CACHE_BYTES + 1)
                .unwrap();
            std::thread::sleep(Duration::from_millis(2));
            intent = json!({"kind":"garbage_collect"});
        } else if matches!(case.action, Action::Refresh | Action::Build) {
            if case.action == Action::Build {
                let report = try_rpc(
                    &fixture.socket,
                    &query(json!({"kind":"dependency_scripts","selector":selector})),
                )
                .unwrap();
                let approval = &report["outcome"]["result"]["scripts"][0]["script"]["approval"];
                assert!(approval.is_object());
                fixture.execute(json!({"kind":"dependency_script_decision","selector":selector,"approval":approval,"allow":true}), "setup-approval");
            }
            intent = json!({"kind":"dependencies_refresh","selector":selector});
        } else {
            intent["session_id"] = json!("provider-second");
            if case.action == Action::Refill {
                data::remove_tree(
                    &fixture
                        .root
                        .join("runtime/dependency-native")
                        .join(case.manager.name()),
                );
            } else if case.action == Action::Corrupt {
                let payload =
                    artifact(&fixture, case.manager, &opened["workspace"]).join("payload");
                let file = data::files(&payload).into_iter().next().unwrap();
                fs::OpenOptions::new()
                    .append(true)
                    .open(file)
                    .unwrap()
                    .write_all(b"\ncorrupt\n")
                    .unwrap();
            }
        }
        if case.action == Action::Fork {
            original_bytes = Some(snapshot(&cwd));
        }
    }
    fixture.arm();
    let mut stream = UnixStream::connect(&fixture.socket).unwrap();
    serde_json::to_writer(&mut stream, &execute(intent.clone(), "interrupted")).unwrap();
    stream.write_all(b"\n").unwrap();
    fixture.interrupt_at(&mut daemon, point, 120);
    let inode = fs::metadata(fixture.root.join("state.sqlite"))
        .unwrap()
        .ino();
    drop(stream);
    fixture.disarm();
    let _restarted = fixture.start(point);
    assert_eq!(
        fs::metadata(fixture.root.join("state.sqlite"))
            .unwrap()
            .ino(),
        inode
    );
    fixture.assert_consistent();
    assert_dependency_artifacts_clean(&fixture.root);
    if let (Some(cwd), Some(before)) = (&original, &original_bytes) {
        assert_eq!(
            &snapshot(cwd),
            before,
            "predecessor changed across interrupted dependency work"
        );
    }
    let recovered = if case.action == Action::Fork {
        fixture.execute(intent, "retry-provider-fork")
    } else {
        let mut open = fixture.open();
        open["session_id"] = json!("provider-recovered");
        fixture.execute(open, "reopen-provider")
    };
    let cwd = Path::new(recovered["cwd"].as_str().unwrap());
    if case.action == Action::Fork {
        let parent = original.as_ref().unwrap().join(".venv");
        let held = parent.with_file_name("held-venv");
        fs::rename(&parent, &held).unwrap();
        assert_ready(&fixture, cwd, case, true);
        fs::rename(held, parent).unwrap();
    } else {
        assert_ready(&fixture, cwd, case, false);
    }
    for (name, bytes) in inputs {
        assert_eq!(
            fs::read(fixture.source.join(&name)).unwrap(),
            bytes,
            "source metadata changed: {name}"
        );
        assert_eq!(
            fs::read(cwd.join(&name)).unwrap(),
            bytes,
            "workspace metadata changed: {name}"
        );
    }
    if let Some(registry) = &registry {
        assert!(
            registry.requests() > requests_before.unwrap(),
            "daemon never used the real registry"
        );
    }
    let receipt: Value = serde_json::from_slice(
        &fs::read(artifact(&fixture, case.manager, &recovered["workspace"]).join("receipt.json"))
            .unwrap(),
    )
    .unwrap();
    for id in strings(
        &fixture.database(),
        "SELECT id FROM handoffs WHERE state='pending'",
    ) {
        fixture.execute(
            json!({"kind":"successor_adopt","handoff_id":id}),
            &format!("adopt-{id}"),
        );
    }
    for id in strings(
        &fixture.database(),
        "SELECT workspace_id FROM sessions WHERE state='active'",
    ) {
        fixture.execute(
            json!({"kind":"workspace_release","selector":{"workspace_id":id}}),
            &format!("release-{id}"),
        );
    }
    std::thread::sleep(Duration::from_millis(2));
    fixture.execute(json!({"kind":"garbage_collect"}), "cleanup-provider");
    fixture.assert_consistent();
    assert_dependency_artifacts_clean(&fixture.root);
    assert_eq!(
        count(&fixture.database(), "SELECT count(*) FROM workspaces"),
        0
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
            "SELECT count(*) FROM dependency_receipts"
        ),
        0
    );
    let result = json!({"point":point.name(), "scenario":format!("Provider({case:?})"), "provider":case.manager.name(), "action":format!("{:?}",case.action), "signal":"SIGKILL", "sigkill_count":1, "restart":"same_pool_and_sqlite_inode", "consistent":true, "cleanup_workspaces":0, "retained_by_user_choice":0, "tools":receipt["tools"], "fingerprint":receipt["fingerprint"], "elapsed_ms":started.elapsed().as_millis()});
    // Native Go modules have read-only directories; make only this owned
    // fixture cache removable before TempDir performs final cleanup.
    if case.manager == Manager::Go {
        data::remove_tree(&fixture.root.join("runtime/dependency-native/go"));
        data::remove_tree(&fixture._temp.path().join("fixture-tool-home"));
    }
    result
}
