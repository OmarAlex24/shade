#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(metadata) = std::fs::symlink_metadata(path)
            && metadata.file_type().is_socket()
        {
            return;
        }
        assert!(Instant::now() < deadline, "daemon socket did not appear");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn request(path: &Path, value: &Value) -> Value {
    let mut stream = UnixStream::connect(path).unwrap();
    serde_json::to_writer(&mut stream, value).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert!(line.ends_with('\n'));
    serde_json::from_str(&line).unwrap()
}

#[test]
fn daemon_socket_timeout_recovery_and_sigterm_are_durable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let socket = temp.path().join("shade.sock");
    let probe = temp.path().join("probe.sock");
    match std::os::unix::net::UnixListener::bind(&probe) {
        Ok(listener) => {
            drop(listener);
            std::fs::remove_file(&probe).unwrap();
        }
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => return,
        Err(error) => panic!("socket capability probe failed: {error}"),
    }
    let child = Command::new(env!("CARGO_BIN_EXE_shade"))
        .args(["--socket", socket.to_str().unwrap(), "daemon"])
        .env("SHADE_ROOT", &root)
        .env("SHADE_OPERATION_WAIT_MS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    wait_for_socket(&socket);
    let metadata = std::fs::symlink_metadata(&socket).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

    let execute = json!({
        "type": "execute",
        "v": 1,
        "request_id": "execute-1",
        "idempotency_key": "smoke-gc",
        "actor": {"kind": "cli", "id": "smoke"},
        "intent": {"kind": "garbage_collect"}
    });
    let accepted = request(&socket, &execute);
    assert_eq!(accepted["status"], "ok");
    assert_eq!(accepted["outcome"]["state"], "accepted");
    let operation_id = accepted["outcome"]["result"]["operation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let operation = request(
            &socket,
            &json!({
                "type": "query",
                "v": 1,
                "request_id": "operation-1",
                "query": {"kind": "operation", "operation_id": operation_id}
            }),
        );
        if operation["outcome"]["result"]["state"] == "completed" {
            break;
        }
        assert!(Instant::now() < deadline, "operation did not complete");
        std::thread::sleep(Duration::from_millis(10));
    }

    let replay = request(&socket, &execute);
    assert_eq!(replay["status"], "ok");
    assert_eq!(replay["outcome"]["state"], "accepted");
    assert_eq!(replay["outcome"]["result"]["operation_id"], operation_id);

    let invalid = {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.write_all(b"{not-json}\n").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    assert_eq!(invalid["status"], "error");
    assert_eq!(invalid["error"]["code"], "REQUEST_INVALID");

    unsafe {
        libc::kill(child.0.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.0.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "daemon ignored SIGTERM");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket.exists());
}

/// `shade sleep` removes the tree the caller may be standing in, and the shell
/// keeps the deleted directory as its cwd. Every later selector command then
/// fails in `getcwd`, before it reaches the daemon, and that used to arrive as
/// `CLI_FAILED` with `retry: safe`: "run `shade doctor`, then retry" -- for a
/// condition no retry has ever fixed and doctor cannot see. The answer has to
/// name itself and the two flags that get past it.
#[test]
fn a_command_run_from_a_deleted_directory_names_the_condition_and_the_way_out() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let gone = temp.path().join("gone");
    std::fs::create_dir(&gone).unwrap();
    let script = format!(
        "cd {gone} && rm -rf {gone} && exec \"$0\" --socket {socket} context",
        gone = gone.display(),
        socket = temp.path().join("s.sock").display(),
    );
    let output = Command::new("/bin/sh")
        .args(["-c", &script, env!("CARGO_BIN_EXE_shade")])
        .env("SHADE_ROOT", temp.path().join("state"))
        .env_remove("SHADE_WORKSPACE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["status"], "error", "{response}");
    assert_eq!(response["error"]["code"], "SELECTOR_CWD_UNAVAILABLE");
    assert_eq!(response["error"]["retry"], "never");
    assert_eq!(
        response["error"]["next"],
        "cd to an existing directory, or pass --workspace <id> or --cwd <path>"
    );
}
