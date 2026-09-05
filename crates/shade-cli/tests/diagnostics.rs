#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(root: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_shade"))
        .arg("--socket")
        .arg(root.join("s.sock"))
        .args(args)
        .env("SHADE_ROOT", root.join("state"))
        .output()
        .unwrap();
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)))
}

fn daemon(root: &Path) -> Daemon {
    let socket = root.join("s.sock");
    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_shade"))
            .arg("--socket")
            .arg(&socket)
            .arg("daemon")
            .env("SHADE_ROOT", root.join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if std::os::unix::net::UnixStream::connect(&socket).is_ok()
            && cli(root, &["doctor"])["status"] == "ok"
        {
            return daemon;
        }
        assert!(daemon.0.try_wait().unwrap().is_none(), "daemon exited");
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn daemon_error_details_survive_restart_and_remain_readable_without_a_daemon() {
    let temporary = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temporary.path();
    let running = daemon(root);
    let missing = root.join("missing-repository");
    let args = [
        "--idempotency-key",
        "missing-repo",
        "open",
        missing.to_str().unwrap(),
        "--session",
        "diagnostic-session",
    ];
    let failed = cli(root, &args);
    assert_eq!(failed["status"], "error", "{failed}");
    let id = failed["error"]["diagnostics_id"]
        .as_str()
        .expect("missing diagnostic id");
    let diagnostic = cli(root, &["doctor", "--diagnostics", id]);
    assert_eq!(
        diagnostic["status"], "ok",
        "diagnostics cannot be retrieved: {diagnostic}"
    );
    let record = diagnostic["outcome"]["result"].clone();
    assert_eq!(record["id"], id);
    assert!(
        record["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
    assert_eq!(record["operation"], failed["error"]["operation"]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let typed = runtime
        .block_on(shade_client::ShadeClient::cli(root.join("s.sock")).diagnostics(id))
        .unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), record);
    assert_eq!(
        fs::metadata(root.join("state/state.sqlite"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(running);
    assert_eq!(
        cli(root, &["doctor", "--diagnostics", id])["outcome"]["result"],
        record
    );
    let _restarted = daemon(root);
    assert_eq!(cli(root, &args)["error"]["diagnostics_id"], id);
    assert_eq!(
        cli(root, &["doctor", "--diagnostics", id])["outcome"]["result"],
        record
    );
}

#[test]
fn a_local_cli_failure_has_a_private_diagnostic_without_starting_the_daemon() {
    let temporary = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temporary.path();
    let failed = cli(root, &["doctor"]);
    assert_eq!(failed["status"], "error", "{failed}");
    let id = failed["error"]["diagnostics_id"]
        .as_str()
        .expect("missing local diagnostic id");
    let diagnostic = cli(root, &["doctor", "--diagnostics", id]);
    assert_eq!(diagnostic["status"], "ok", "{diagnostic}");
    assert_eq!(diagnostic["outcome"]["result"]["origin"], "cli");
    assert!(!root.join("s.sock").exists());
}

#[test]
fn diagnostic_lookup_does_not_create_state_or_accept_paths_as_ids() {
    let temporary = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temporary.path();
    assert_eq!(
        cli(root, &["doctor", "--diagnostics", "../../private"])["error"]["code"],
        "DIAGNOSTIC_ID_INVALID"
    );
    let id = format!("diag_{}", ulid::Ulid::new());
    assert_eq!(
        cli(root, &["doctor", "--diagnostics", &id])["error"]["code"],
        "DIAGNOSTIC_NOT_FOUND"
    );
    assert!(!root.join("state").exists());
}
