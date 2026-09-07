//! Public CLI and Rust facade against the real daemon, npm, Git and APFS.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

#[path = "../../shade-engine/tests/support/registry.rs"]
mod registry;

#[path = "support/retained_fixture.rs"]
mod retained_fixture;
#[path = "support/script_fixture.rs"]
mod script_fixture;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shade_client::{ShadeClient, TerminalOutcome};
use shade_protocol::{OpenSession, RepositoryLocator, SessionId};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cli(socket: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_shade"))
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "CLI {args:?}: {error}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(output.status.success(), "CLI {args:?}: {value}");
    assert_eq!(value["status"], "ok", "{value}");
    value["outcome"]["result"].clone()
}

fn start(root: &Path, socket: &Path) -> Daemon {
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_shade"))
            .args(["--socket", socket.to_str().unwrap(), "daemon"])
            .env("SHADE_ROOT", root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(daemon.0.try_wait().unwrap().is_none(), "daemon exited");
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    daemon
}

#[tokio::test]
#[ignore = "requires real host npm/Node and an unsandboxed APFS test process"]
async fn public_script_approval_is_durable_and_refresh_creates_an_independent_successor() {
    let binary_digest = hex::encode(Sha256::digest(
        fs::read(env!("CARGO_BIN_EXE_shade")).unwrap(),
    ));
    let temporary = retained_fixture::RetainedFixture::new("shade-script-acceptance-");
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "Shade test"]);
    git(&source, &["config", "user.email", "shade@example.invalid"]);
    let registry = script_fixture::add_script_package(&source, temporary.path());
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "fixture"]);
    let state = temporary.path().join("state");
    let socket = temporary.path().join("s.sock");
    let daemon = start(&state, &socket);
    let client = ShadeClient::cli(&socket);
    let session = client
        .sessions()
        .open(OpenSession {
            session_id: SessionId("script-session".into()),
            repository: RepositoryLocator::Local {
                path: source.to_string_lossy().into_owned(),
            },
            base: Some("main".into()),
            intent: None,
        })
        .await
        .unwrap();
    assert!(registry.requests() > 0);
    let original = PathBuf::from(&session.opened().cwd);
    let reports = session.dependency_scripts().await.unwrap();
    assert_eq!(reports.scripts.len(), 1);
    let approval = reports.scripts[0].script.approval.clone();
    assert_eq!(approval.package, "@shade/test-build");
    assert!(!reports.scripts[0].allowed);
    assert!(!reports.scripts[0].script.executed);
    let mut wrong = approval.clone();
    wrong.integrity.push('x');
    assert!(session.approve_script(wrong).await.is_err());
    let workspace = session.opened().workspace.0;
    let workspace = workspace.as_str();
    let args = [
        "--idempotency-key",
        "approve-once",
        "deps",
        "approve-script",
        "--workspace",
        workspace,
        "--provider",
        &approval.provider,
        "--package",
        &approval.package,
        "--version",
        &approval.version,
        "--integrity",
        &approval.integrity,
    ];
    let approved = cli(&socket, &args);
    assert_eq!(approved, json!({"allowed":true,"refresh_required":true}));
    assert_eq!(cli(&socket, &args), approved);
    assert!(
        !original
            .join("node_modules/approved-alias/built.txt")
            .exists()
    );
    drop(daemon);
    let _daemon = start(&state, &socket);
    assert!(session.dependency_scripts().await.unwrap().scripts[0].allowed);
    let TerminalOutcome::Completed(successor) = session.refresh_dependencies().await.unwrap()
    else {
        panic!("expected successor")
    };
    let built = PathBuf::from(&successor.opened().cwd);
    assert_ne!(built, original);
    assert!(
        !original
            .join("node_modules/approved-alias/built.txt")
            .exists()
    );
    assert_eq!(
        fs::read(built.join("node_modules/approved-alias/built.txt")).unwrap(),
        b"1.0.0"
    );
    assert!(
        successor.dependency_scripts().await.unwrap().scripts[0]
            .script
            .executed
    );
    assert!(!built.join("root-ran").exists());
    let TerminalOutcome::Completed(decision) = successor.revoke_script(approval).await.unwrap()
    else {
        panic!("expected revocation")
    };
    assert!(!decision.allowed);
    let TerminalOutcome::Completed(revoked) = successor.refresh_dependencies().await.unwrap()
    else {
        panic!("expected revoked successor")
    };
    assert!(
        !Path::new(&revoked.opened().cwd)
            .join("node_modules/approved-alias/built.txt")
            .exists()
    );
    assert_eq!(
        fs::read(built.join("node_modules/approved-alias/built.txt")).unwrap(),
        b"1.0.0"
    );
    let report = cli(
        &socket,
        &[
            "deps",
            "scripts",
            "--workspace",
            &revoked.opened().workspace.0,
        ],
    );
    assert_eq!(report["scripts"][0]["allowed"], false);
    let connection = rusqlite::Connection::open(state.join("state.sqlite")).unwrap();
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM events WHERE event='dependency.script_decided'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "idempotent retry duplicated a script decision");
    revoked.release().await.unwrap();
    assert_eq!(
        hex::encode(Sha256::digest(
            fs::read(env!("CARGO_BIN_EXE_shade")).unwrap()
        )),
        binary_digest,
        "the tested binary changed during acceptance"
    );
    eprintln!(
        "SHADE_SCRIPT_CLI_EVIDENCE {}",
        json!({"binary_sha256":binary_digest,"checks":["real_npm_and_daemon","canonical_alias_identity","exact_tuple_rejection","cli_idempotency","same_database_restart","rust_session_facade","approved_and_revoked_successors","predecessors_preserved","root_scripts_blocked"]})
    );
}
