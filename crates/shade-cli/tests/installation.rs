//! Operational installation in the caller's GUI launchd domain, using a fresh
//! reserved label and a disposable directory. No production service is replaced.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

#[path = "../../shade-engine/tests/support/registry.rs"]
mod registry;
#[path = "support/retained_fixture.rs"]
mod retained_fixture;
#[path = "support/script_fixture.rs"]
mod script_fixture;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shade_client::ShadeClient;
use shade_protocol::{Intent, OpenSession, Query, RepositoryLocator, ResponseBody, SessionId};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

struct Service(String);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = launchctl(&["bootout", &self.0]);
    }
}

fn launchctl(args: &[&str]) -> Output {
    Command::new("/bin/launchctl").args(args).output().unwrap()
}

fn service_pid(service: &str) -> Option<u32> {
    let output = launchctl(&["print", service]);
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("pid = ")
                .and_then(|value| value.parse().ok())
        })
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "requires a GUI launchd domain, APFS and real host npm/Node"]
async fn isolated_launchagent_installs_opens_and_restarts_the_same_runtime() {
    let mut temporary = retained_fixture::RetainedFixture::new("shade-install-");
    let root = fs::canonicalize(temporary.path()).unwrap();
    if std::env::var_os("SHADE_INSTALL_KEEP_ROOT").is_some() {
        eprintln!("SHADE_INSTALL_ROOT {}", root.display());
        temporary.preserve();
    }
    let state = root.join("state");
    let socket = state.join("shade.sock");
    let binary = std::env::var_os("SHADE_INSTALL_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_shade")));
    let binary_digest = hex::encode(Sha256::digest(fs::read(&binary).unwrap()));
    let label = format!("com.shade.daemon.acceptance.{}", ulid::Ulid::new());
    let service = Service(format!("gui/{}/{label}", unsafe { libc::geteuid() }));
    assert!(service_pid(&service.0).is_none());
    let install = || {
        Command::new(&binary)
            .args([
                "--socket",
                socket.to_str().unwrap(),
                "install",
                "--harness-install",
                "--harness-root",
                root.to_str().unwrap(),
                "--harness-label",
                &label,
            ])
            .env("SHADE_ROOT", &state)
            .output()
            .unwrap()
    };
    let output = install();
    assert!(
        output.status.success(),
        "install failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["status"], "ok");
    let result = &response["outcome"]["result"];
    assert_eq!(result["label"], label);
    let installed = PathBuf::from(result["installed"].as_str().unwrap());
    let plist = PathBuf::from(result["launch_agent"].as_str().unwrap());
    assert!(installed.starts_with(&root) && plist.starts_with(&root));
    assert_eq!(
        hex::encode(Sha256::digest(fs::read(&installed).unwrap())),
        binary_digest
    );
    assert_eq!(
        fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(&plist).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&plist)
            .output()
            .unwrap()
            .status
            .success()
    );
    let socket_metadata = fs::symlink_metadata(&socket).unwrap();
    assert!(socket_metadata.file_type().is_socket());
    assert_eq!(socket_metadata.permissions().mode() & 0o777, 0o600);
    let original_pid = service_pid(&service.0).expect("installed service is not running");

    let repository = root.join("source");
    fs::create_dir(&repository).unwrap();
    git(&repository, &["init", "-b", "main"]);
    git(
        &repository,
        &["config", "user.name", "Shade installation test"],
    );
    git(
        &repository,
        &["config", "user.email", "shade@example.invalid"],
    );
    let registry = script_fixture::add_script_package(&repository, &root);
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-m", "fixture"]);
    let client = ShadeClient::cli(&socket);
    let open_started = Instant::now();
    let session = client
        .sessions()
        .open(OpenSession {
            session_id: SessionId("installed-session".into()),
            repository: RepositoryLocator::Local {
                path: repository.to_string_lossy().into_owned(),
            },
            base: Some("main".into()),
            intent: None,
        })
        .await
        .unwrap();
    let open_ms = open_started.elapsed().as_millis();
    assert!(
        registry.requests() > 0,
        "launchd could not use the real host npm"
    );
    assert!(
        !Path::new(&session.opened().cwd)
            .join("node_modules/approved-alias/built.txt")
            .exists()
    );
    let failed = client
        .execute_wait_idempotent(
            Intent::SessionOpen(OpenSession {
                session_id: SessionId("installed-diagnostic".into()),
                repository: RepositoryLocator::Local {
                    path: root.join("missing").to_string_lossy().into_owned(),
                },
                base: None,
                intent: None,
            }),
            "installed-diagnostic-failure",
        )
        .await
        .unwrap();
    let ResponseBody::Error { error } = failed.body else {
        panic!("expected diagnostic failure")
    };
    let diagnostic_id = error.diagnostics_id.unwrap();
    let diagnostic = client.diagnostics(&diagnostic_id).await.unwrap();
    assert_eq!(diagnostic.operation, error.operation);
    assert!(!diagnostic.message.is_empty());
    let sqlite_metadata = fs::metadata(state.join("state.sqlite")).unwrap();
    assert_eq!(sqlite_metadata.permissions().mode() & 0o777, 0o600);
    let sqlite_inode = sqlite_metadata.ino();
    let restart_started = Instant::now();
    assert!(launchctl(&["kill", "SIGKILL", &service.0]).status.success());
    let deadline = Instant::now() + Duration::from_secs(30);
    let restarted_pid = loop {
        if let Some(pid) = service_pid(&service.0)
            && pid != original_pid
            && session.context().await.is_ok()
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "KeepAlive did not recover the installed daemon"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let restart_ms = restart_started.elapsed().as_millis();
    assert_eq!(
        client.diagnostics(&diagnostic_id).await.unwrap(),
        diagnostic
    );
    assert_eq!(
        fs::metadata(state.join("state.sqlite")).unwrap().ino(),
        sqlite_inode
    );
    assert_eq!(
        session.context().await.unwrap().workspace,
        session.opened().workspace
    );
    let collision = install();
    assert!(
        !collision.status.success(),
        "acceptance install replaced an existing service"
    );
    assert_eq!(service_pid(&service.0), Some(restarted_pid));
    session.release().await.unwrap();
    let doctor = client.query(Query::Doctor).await.unwrap();
    assert!(matches!(
        doctor.body,
        shade_protocol::ResponseBody::Ok { .. }
    ));
    assert!(launchctl(&["bootout", &service.0]).status.success());
    // bootout acknowledges removal before launchd necessarily reaps the job.
    let unload_deadline = Instant::now() + Duration::from_secs(10);
    while launchctl(&["print", &service.0]).status.success() {
        assert!(
            Instant::now() < unload_deadline,
            "acceptance service was not unloaded"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let offline = Command::new(&installed)
        .arg("--socket")
        .arg(&socket)
        .args(["doctor", "--diagnostics", &diagnostic_id])
        .env("SHADE_ROOT", &state)
        .output()
        .unwrap();
    assert!(offline.status.success());
    let offline: Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert_eq!(
        offline["outcome"]["result"],
        serde_json::to_value(&diagnostic).unwrap()
    );
    eprintln!(
        "SHADE_INSTALL_EVIDENCE {}",
        json!({"binary_sha256":binary_digest,"label":label,"initial_pid":original_pid,"restarted_pid":restarted_pid,"open_ms":open_ms,"restart_ms":restart_ms,"checks":["single_binary_install","private_plist_and_socket","launchagent_ready_before_success","real_host_npm_from_launchd","unapproved_scripts_blocked","keepalive_sigkill_restart","same_database_and_workspace","private_durable_diagnostic","diagnostic_survives_restart","offline_diagnostic_after_unload","reserved_label_collision_rejected","service_unloaded"],"status":"passed"})
    );
}
