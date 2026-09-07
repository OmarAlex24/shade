#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
//! The detached keepalive, against a real daemon and a real owner process.
//!
//! Everything here is about process lifetime, so nothing is faked: a real
//! child holds the lease, a real owner is killed, and the assertions are made
//! on the pidfile and the daemon's own event log.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A process this test started and must not leave behind, addressed by pid
/// because the keepalive is deliberately not our child.
struct PidGuard(u32);

impl Drop for PidGuard {
    fn drop(&mut self) {
        // SAFETY: SIGKILL to a pid this test spawned; worst case it is gone.
        unsafe { libc::kill(self.0 as libc::c_int, libc::SIGKILL) };
    }
}

struct Fixture {
    root: PathBuf,
    socket: PathBuf,
    source: PathBuf,
    _temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        // Short path leaves room for Darwin's 104-byte Unix socket limit.
        let temp = tempfile::Builder::new()
            .prefix("shade-ka-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.name", "Shade keepalive test"]);
        git(&source, &["config", "user.email", "keepalive@test.invalid"]);
        std::fs::write(source.join("tracked.txt"), "base\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "fixture"]);
        Self {
            root: temp.path().join("state"),
            socket: temp.path().join("s.sock"),
            source,
            _temp: temp,
        }
    }

    fn daemon(&self, lease_ttl_secs: i64) -> ChildGuard {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self._temp.path().join("daemon.log"))
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_shade"))
            .args([
                "--socket",
                self.socket.to_str().unwrap(),
                "daemon",
                "--harness-lifecycle",
                "--harness-lease-ttl-secs",
                &lease_ttl_secs.to_string(),
                "--harness-orphan-grace-secs",
                "0",
            ])
            .env("SHADE_ROOT", &self.root)
            .env("SHADE_OPERATION_WAIT_MS", "0")
            .env("TMPDIR", self._temp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let guard = ChildGuard(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(value) = try_rpc(
                &self.socket,
                &json!({"type":"query","v":1,"request_id":"ready","query":{"kind":"doctor"}}),
            ) && value["status"] == "ok"
            {
                return guard;
            }
            assert!(Instant::now() < deadline, "daemon never became ready");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Run one `shade` command and return the single JSON value it printed.
    fn cli(&self, arguments: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_shade"))
            .arg("--socket")
            .arg(&self.socket)
            .args(arguments)
            .env("SHADE_ROOT", &self.root)
            .env("SHADE_OPERATION_WAIT_MS", "0")
            .env("TMPDIR", self._temp.path())
            // The environment must not decide what the test is exercising.
            .env_remove("SHADE_NO_KEEPALIVE")
            .env_remove("SHADE_SESSION")
            .env_remove("SHADE_LEASE")
            .env_remove("SHADE_WORKSPACE")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            stdout.lines().count(),
            1,
            "a command prints exactly one JSON value, got: {stdout}"
        );
        serde_json::from_str(stdout.trim()).unwrap()
    }

    fn open(&self, session: &str, extra: &[&str]) -> Value {
        let mut arguments = vec![
            "open",
            self.source.to_str().unwrap(),
            "--session",
            session,
            "--interval-secs",
            "1",
        ];
        arguments.extend_from_slice(extra);
        self.cli(&arguments)
    }

    fn pidfile(&self, session: &str) -> PathBuf {
        self.root
            .join("runtime/keepalive")
            .join(format!("{}.json", sha256_hex(session)))
    }

    /// The keepalive's own one-line-per-state-change log. It is the only
    /// account of what a detached child did, so failures quote it.
    fn keepalive_log(&self, session: &str) -> String {
        let path = self
            .root
            .join("runtime/keepalive")
            .join(format!("{}.log", sha256_hex(session)));
        std::fs::read_to_string(path).unwrap_or_else(|error| format!("<no log: {error}>"))
    }

    fn heartbeats(&self, lease: &str) -> usize {
        let events = try_rpc(
            &self.socket,
            &json!({"type":"query","v":1,"request_id":"events",
                "query":{"kind":"events","after_cursor":0,"limit":10000}}),
        )
        .unwrap();
        events["outcome"]["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event"] == "lease.heartbeat" && event["resource"] == lease)
            .count()
    }

    fn session_state(&self, session: &str) -> String {
        let response = try_rpc(
            &self.socket,
            &json!({"type":"query","v":1,"request_id":"session",
                "query":{"kind":"session","session_id":session}}),
        )
        .unwrap();
        response["outcome"]["result"]["lifecycle"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn git(cwd: &Path, args: &[&str]) {
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
}

fn try_rpc(socket: &Path, value: &Value) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    serde_json::to_writer(&mut stream, value)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// A stand-in for the agent process whose lifetime owns the lease.
fn fake_owner() -> PidGuard {
    let child = Command::new("/bin/sh")
        .args(["-c", "exec sleep 300"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Leaked deliberately: the pid outlives the `Child`, and `PidGuard` is what
    // reaps it. Keeping the handle would make the test the process's parent and
    // the keepalive's grandparent, which is the relationship under test.
    let pid = child.id();
    std::mem::forget(child);
    PidGuard(pid)
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs the permission and existence check only.
    unsafe { libc::kill(pid as libc::c_int, 0) == 0 }
}

fn session_id(pid: u32) -> i32 {
    // SAFETY: `getsid` reads kernel process state and returns -1 on failure.
    unsafe { libc::getsid(pid as libc::pid_t) }
}

fn parent_of(pid: u32) -> u32 {
    let output = Command::new("/bin/ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap_or(0)
}

fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_until(what: &str, condition: impl FnMut() -> bool) {
    assert!(wait_for(condition), "timed out waiting for {what}");
}

fn keepalive(opened: &Value) -> &Value {
    &opened["outcome"]["result"]["keepalive"]
}

#[test]
fn keepalive_stops_when_the_owner_exits() {
    let fixture = Fixture::new();
    let _daemon = fixture.daemon(120);
    let owner = fake_owner();

    let opened = fixture.open("ka-owner", &["--owner-pid", &owner.0.to_string()]);
    assert_eq!(opened["status"], "ok", "{opened}");
    let status = keepalive(&opened);
    assert_eq!(status["state"], "started", "{opened}");
    assert_eq!(status["owner_pid"], owner.0);
    let pid = status["pid"].as_u64().unwrap() as u32;
    let lease = opened["outcome"]["result"]["lease"].as_str().unwrap();

    let pidfile = fixture.pidfile("ka-owner");
    assert!(pidfile.is_file(), "{} is missing", pidfile.display());
    assert_eq!(
        std::fs::metadata(&pidfile).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let record: Value = serde_json::from_slice(&std::fs::read(&pidfile).unwrap()).unwrap();
    assert_eq!(record["keepalive_pid"], pid);
    assert_eq!(record["owner_pid"], owner.0);
    assert_eq!(record["lease"], lease);

    // Detached: its own session, and reparented away from the exited CLI.
    assert_ne!(
        session_id(pid),
        session_id(std::process::id()),
        "the keepalive must survive a kill of the caller's process group"
    );
    wait_until("the keepalive to be reparented", || parent_of(pid) == 1);

    // It renews the lease with nobody else heartbeating.
    wait_until("two heartbeats from the keepalive", || {
        fixture.heartbeats(lease) >= 2
    });

    // SAFETY: `owner.0` is the pid this test spawned.
    unsafe { libc::kill(owner.0 as libc::c_int, libc::SIGKILL) };
    wait_until("the keepalive to exit with its owner", || !alive(pid));
    wait_until("the pidfile to be removed", || !pidfile.exists());
}

#[test]
fn release_stops_the_keepalive() {
    let fixture = Fixture::new();
    let _daemon = fixture.daemon(120);
    let owner = fake_owner();

    let opened = fixture.open("ka-release", &["--owner-pid", &owner.0.to_string()]);
    let pid = keepalive(&opened)["pid"].as_u64().unwrap() as u32;
    let workspace = opened["outcome"]["result"]["workspace"].as_str().unwrap();

    let released = fixture.cli(&["release", "--workspace", workspace]);
    assert_eq!(
        released["outcome"]["result"]["released"], true,
        "{released}"
    );
    wait_until("the keepalive to stop on release", || !alive(pid));
    assert!(!fixture.pidfile("ka-release").exists());

    // The owner is still running; nothing but the release stopped the child.
    assert!(alive(owner.0));
}

#[test]
fn sleep_stops_the_keepalive_and_wake_starts_a_new_one() {
    let fixture = Fixture::new();
    let _daemon = fixture.daemon(120);
    let owner = fake_owner();

    let opened = fixture.open("ka-sleep", &["--owner-pid", &owner.0.to_string()]);
    let first = keepalive(&opened)["pid"].as_u64().unwrap() as u32;
    let workspace = opened["outcome"]["result"]["workspace"]
        .as_str()
        .unwrap()
        .to_owned();

    let slept = fixture.cli(&["sleep", "--workspace", &workspace]);
    assert_eq!(slept["outcome"]["result"]["suspended"], true, "{slept}");
    // Sleep reclaims the process as well as the disk: a child heartbeating a
    // suspended session would be renewing a lease nobody holds.
    wait_until("the keepalive to stop on sleep", || !alive(first));
    assert!(!fixture.pidfile("ka-sleep").exists());
    assert!(alive(owner.0), "sleep must not touch the owning agent");
    assert_eq!(fixture.session_state("ka-sleep"), "suspended");

    let woken = fixture.cli(&[
        "wake",
        "--session",
        "ka-sleep",
        "--owner-pid",
        &owner.0.to_string(),
        "--interval-secs",
        "1",
    ]);
    let result = &woken["outcome"]["result"];
    assert_eq!(result["session"], "ka-sleep", "{woken}");
    assert_ne!(result["workspace"], workspace.as_str());
    let second = keepalive(&woken)["pid"].as_u64().unwrap() as u32;
    assert_ne!(second, first);
    let guard = PidGuard(second);
    assert_eq!(
        std::fs::metadata(fixture.pidfile("ka-sleep"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // The new child renews the new lease, not the one sleep gave up.
    let lease = result["lease"].as_str().unwrap().to_owned();
    let beating = wait_for(|| fixture.heartbeats(&lease) >= 2);
    assert!(
        beating,
        "wake must leave a keepalive that heartbeats; keepalive log:\n{}",
        fixture.keepalive_log("ka-sleep")
    );
    assert_eq!(fixture.session_state("ka-sleep"), "active");
    drop(guard);
}

#[test]
fn stale_pidfile_is_replaced_on_reopen() {
    let fixture = Fixture::new();
    let _daemon = fixture.daemon(120);
    let owner = fake_owner();

    let opened = fixture.open("ka-stale", &["--owner-pid", &owner.0.to_string()]);
    let first = keepalive(&opened)["pid"].as_u64().unwrap() as u32;
    let pidfile = fixture.pidfile("ka-stale");
    let stale = std::fs::read(&pidfile).unwrap();

    // SAFETY: `first` is the keepalive this test just started.
    unsafe { libc::kill(first as libc::c_int, libc::SIGKILL) };
    wait_until("the first keepalive to die", || !alive(first));
    // A killed keepalive cannot clean up after itself, which is exactly the
    // pidfile a reopen has to survive.
    std::fs::write(&pidfile, &stale).unwrap();

    let reopened = fixture.open("ka-stale", &["--owner-pid", &owner.0.to_string()]);
    let second = keepalive(&reopened)["pid"].as_u64().unwrap() as u32;
    assert_ne!(second, first);
    let record: Value = serde_json::from_slice(&std::fs::read(&pidfile).unwrap()).unwrap();
    assert_eq!(record["keepalive_pid"], second);
    wait_until("the replacement keepalive to heartbeat", || {
        fixture.heartbeats(reopened["outcome"]["result"]["lease"].as_str().unwrap()) >= 1
    });
    let _ = PidGuard(second);
}

#[test]
fn no_keepalive_flag_and_undetectable_owner_are_reported_and_harmless() {
    let fixture = Fixture::new();
    let _daemon = fixture.daemon(120);

    let opted_out = fixture.open("ka-off", &["--no-keepalive"]);
    assert_eq!(opted_out["status"], "ok", "{opted_out}");
    assert!(opted_out["outcome"]["result"]["cwd"].is_string());
    assert_eq!(keepalive(&opted_out)["state"], "skipped");
    assert_eq!(keepalive(&opted_out)["reason"], "disabled");
    assert!(!fixture.pidfile("ka-off").exists());

    // No ancestor of the test binary is a recognised agent, so detection must
    // decline rather than adopt whatever happens to be up the chain.
    let undetected = fixture.open("ka-nobody", &["--owner-name", "no-such-agent-process"]);
    assert_eq!(undetected["status"], "ok", "{undetected}");
    assert_eq!(keepalive(&undetected)["state"], "skipped");
    assert_eq!(keepalive(&undetected)["reason"], "owner_not_detected");
    assert!(!fixture.pidfile("ka-nobody").exists());

    // An owner pid that names nothing is reported, not fatal.
    let absent = fixture.open("ka-absent", &["--owner-pid", "4294967294"]);
    assert_eq!(absent["status"], "ok", "{absent}");
    assert_eq!(keepalive(&absent)["state"], "skipped");
    assert_eq!(keepalive(&absent)["reason"], "owner_not_found");
}

#[test]
fn dormant_session_is_reattached_by_the_running_keepalive() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon(2);
    let owner = fake_owner();

    let opened = fixture.open("ka-dormant", &["--owner-pid", &owner.0.to_string()]);
    let pid = keepalive(&opened)["pid"].as_u64().unwrap() as u32;
    let first_lease = opened["outcome"]["result"]["lease"]
        .as_str()
        .unwrap()
        .to_owned();

    // Stopping the daemon is what makes the lease genuinely expire: nothing can
    // renew it, and the sweep that marks it dormant runs on the next start.
    drop(daemon);
    std::thread::sleep(Duration::from_secs(4));
    let _daemon = fixture.daemon(2);

    // The owner never died, so the keepalive must bring the session back.
    wait_until("the session to return to active", || {
        fixture.session_state("ka-dormant") == "active"
    });
    assert!(alive(pid), "the keepalive outlives one dormancy");

    // The daemon commits the reattach before the keepalive rewrites its
    // pidfile, so the record is checked by polling rather than once.
    let followed = wait_for(|| {
        std::fs::read(fixture.pidfile("ka-dormant"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|record| record["lease"].as_str().map(str::to_owned))
            .is_some_and(|lease| lease != first_lease)
    });
    assert!(
        followed,
        "the pidfile must follow the lease it is renewing; keepalive log:\n{}",
        fixture.keepalive_log("ka-dormant")
    );

    let status = fixture.cli(&["status", "--session", "ka-dormant"]);
    assert_eq!(status["outcome"]["result"]["lifecycle"], "active");
    assert_eq!(status["outcome"]["result"]["keepalive"]["running"], true);
    assert_eq!(status["outcome"]["result"]["keepalive"]["pid"], pid);

    let _ = PidGuard(pid);
}
