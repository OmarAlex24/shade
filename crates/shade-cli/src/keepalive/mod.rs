//! The detached lease keepalive.
//!
//! A CLI-driven agent has no event loop of its own: it runs one command and
//! exits. Without a keepalive its lease expires two minutes later and the
//! workspace goes dormant even though the agent is still working. `open`,
//! `attach` and `wake` therefore spawn one detached child per session that
//! heartbeats until the *owning agent process* exits.
//!
//! The child never writes to stdout: the CLI contract is exactly one minified
//! JSON value per invocation, and the child outlives its invocation.

pub mod owner;
pub mod registry;
pub mod watch;

use serde_json::{Value, json};
use shade_client::{ClientError, ShadeClient};
use shade_engine::config::EngineConfig;
use shade_protocol::{
    Intent, KeepaliveStatus, LeaseId, OpenedSession, Outcome, Query, ResponseBody, SessionId,
    SessionStatus, ShadeError, WireResponse,
};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEFAULT_INTERVAL_SECS: u64 = 30;
pub const MIN_INTERVAL_SECS: u64 = 1;
pub const MAX_INTERVAL_SECS: u64 = 120;
pub const DISABLE_ENV: &str = "SHADE_NO_KEEPALIVE";

/// Consecutive transport failures tolerated before the keepalive gives up. At
/// the default tick that is ten minutes of an unreachable daemon.
const MAX_TRANSPORT_FAILURES: u32 = 20;
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How long the child waits for the parent to publish its pidfile.
const PIDFILE_WAIT: Duration = Duration::from_secs(2);
const STOP_POLL: Duration = Duration::from_millis(20);
const MAX_LOG_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct KeepaliveOptions {
    pub disabled: bool,
    pub owner_pid: Option<u32>,
    pub owner_names: Vec<String>,
    pub interval_secs: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RunArgs {
    pub session: String,
    pub lease: String,
    pub owner_pid: u32,
    pub owner_start_tvsec: u64,
    pub owner_start_tvusec: u64,
    pub interval_secs: u64,
}

/// Start (or restart) the keepalive for a freshly leased session.
///
/// This can never fail an `open`: every problem is reported inside the
/// returned status and the session keeps working exactly as it does today,
/// because a session without a keepalive merely goes dormant.
pub fn start(
    config: &EngineConfig,
    socket: &Path,
    session: &str,
    lease: &str,
    options: &KeepaliveOptions,
) -> KeepaliveStatus {
    if options.disabled || disabled_by_environment() {
        return skipped("disabled", None);
    }
    let euid = owner::effective_uid();
    let owner = match options.owner_pid {
        Some(pid) => match owner::identity(pid) {
            Some(identity) if identity.uid == euid => identity,
            Some(_) => return skipped("owner_not_permitted", Some(pid)),
            None => return skipped("owner_not_found", Some(pid)),
        },
        None => match owner::detect_owner(&owner::configured_names(&options.owner_names)) {
            Some(identity) => identity,
            // Never guess an owner. A session with no keepalive is safe.
            None => return skipped("owner_not_detected", None),
        },
    };
    // Exactly one keepalive may observe a session; replace any predecessor
    // before spawning so two children never heartbeat the same lease.
    let _ = stop_registered(config, session);
    match spawn(
        config,
        socket,
        session,
        lease,
        &owner,
        options.interval_secs,
    ) {
        Ok(status) => status,
        Err(reason) => KeepaliveStatus {
            state: "failed".into(),
            pid: None,
            owner_pid: Some(owner.pid),
            owner: Some(owner.name),
            reason: Some(reason.into()),
        },
    }
}

pub fn stop(config: &EngineConfig, session: &str) -> Value {
    match stop_registered(config, session) {
        Some(pid) => json!({"session": session, "stopped": true, "pid": pid}),
        None => json!({"session": session, "stopped": false, "reason": "not_running"}),
    }
}

pub fn status(config: &EngineConfig, session: &str) -> Value {
    match registry::read(config, session) {
        Some(record) if registry::is_live(&record) => json!({
            "session": session,
            "running": true,
            "pid": record.keepalive_pid,
            "owner_pid": record.owner_pid,
            "owner": record.owner_name,
            "lease": record.lease,
        }),
        Some(_) => json!({"session": session, "running": false, "reason": "stale"}),
        None => json!({"session": session, "running": false, "reason": "not_running"}),
    }
}

/// Attach a keepalive to a completed `open`/`attach` response and report the
/// outcome inside that same response.
pub fn annotate(
    config: &EngineConfig,
    socket: &Path,
    options: &KeepaliveOptions,
    mut response: WireResponse,
) -> WireResponse {
    let ResponseBody::Ok {
        outcome: Outcome::Completed(ref mut value),
    } = response.body
    else {
        return response;
    };
    let Ok(opened) = serde_json::from_value::<OpenedSession>(value.clone()) else {
        return response;
    };
    let status = start(config, socket, &opened.session.0, &opened.lease.0, options);
    if let Value::Object(object) = value
        && let Ok(encoded) = serde_json::to_value(status)
    {
        object.insert("keepalive".into(), encoded);
    }
    response
}

/// Stop the keepalive belonging to a completed `release` outcome. A
/// `review_required` release has not released anything, so it keeps its lease.
pub fn stop_for_release(config: &EngineConfig, response: &WireResponse) {
    stop_for_completed(config, response, "released");
}

/// Stop the keepalive belonging to a completed `sleep`. The child would exit
/// by itself the next time it asked after the session, but leaving it running
/// would make `sleep` reclaim only the disk and not the process.
pub fn stop_for_sleep(config: &EngineConfig, response: &WireResponse) {
    stop_for_completed(config, response, "suspended");
}

/// Stop the keepalive belonging to a resolved secret review. Keep and discard
/// both close the reviewed workspace's session -- `retained` keeps the tree for
/// a human, `released` does not, and neither leaves a lease to renew -- so both
/// end the child. A merge does not: it hands off to a successor the session
/// keeps living in.
pub fn stop_for_review(config: &EngineConfig, response: &WireResponse) {
    let ResponseBody::Ok {
        outcome: Outcome::Completed(ref value),
    } = response.body
    else {
        return;
    };
    if !matches!(
        value.get("resolution").and_then(Value::as_str),
        Some("kept" | "discarded")
    ) {
        return;
    }
    if let Some(session) = value.get("session").and_then(Value::as_str) {
        let _ = stop_registered(config, session);
    }
}

fn stop_for_completed(config: &EngineConfig, response: &WireResponse, flag: &str) {
    let ResponseBody::Ok {
        outcome: Outcome::Completed(ref value),
    } = response.body
    else {
        return;
    };
    if value.get(flag).and_then(Value::as_bool) != Some(true) {
        return;
    }
    if let Some(session) = value.get("session").and_then(Value::as_str) {
        let _ = stop_registered(config, session);
    }
}

/// The keepalive child's own loop. One blocking `kevent` per tick is both the
/// owner-exit watch and the heartbeat timer.
pub async fn run(config: &EngineConfig, client: &ShadeClient, args: RunArgs) -> anyhow::Result<()> {
    let mut log = Log::open(config, &args.session);
    let Some(current) = owner::identity(args.owner_pid) else {
        log.line("owner_gone_before_start");
        let _ = registry::remove(config, &args.session);
        return Ok(());
    };
    if !current.same_process(
        args.owner_pid,
        args.owner_start_tvsec,
        args.owner_start_tvusec,
    ) {
        // The PID was recycled between spawn and exec. Another process owns
        // this pidfile; leave it alone.
        log.line("owner_identity_mismatch");
        return Ok(());
    }
    // The parent writes the pidfile immediately after `spawn`, but "immediately"
    // is still after this child may have reached `exec`. Wait a bounded moment
    // rather than treating the race as a missing registration.
    let Some(mut record) = await_pidfile(config, &args.session).await else {
        log.line("pidfile_missing");
        return Ok(());
    };
    // The lease came in on the command line from the same parent that wrote the
    // pidfile. Preferring it means a leftover record can never hand this child
    // a lease that belongs to an earlier session.
    record.lease = args.lease.clone();
    let mine = owner::identity(std::process::id());
    if !mine.is_some_and(|mine| {
        mine.same_process(
            record.keepalive_pid,
            record.keepalive_start_tvsec,
            record.keepalive_start_tvusec,
        )
    }) {
        log.line("pidfile_belongs_to_another_keepalive");
        return Ok(());
    }
    let Some(process) = watch::ProcessWatch::new(args.owner_pid)? else {
        log.line("owner_already_exited");
        let _ = registry::remove(config, &args.session);
        return Ok(());
    };
    let process = Arc::new(process);
    let interval = Duration::from_secs(clamp_interval(args.interval_secs));
    let mut failures = 0_u32;
    let mut backoff = Duration::from_millis(250);
    log.line(&format!(
        "started owner={} interval={}s",
        process.pid(),
        interval.as_secs()
    ));
    // The first beat goes out before the first wait. Everything this child was
    // handed -- the lease, the owner identity, the daemon socket -- is proven
    // by using it now rather than a whole interval from now, and a lease
    // handed over with less than an interval left is renewed before it can
    // lapse into dormancy.
    let mut immediate = true;
    loop {
        if immediate {
            immediate = false;
        } else {
            let watcher = Arc::clone(&process);
            let observed = tokio::task::spawn_blocking(move || watcher.wait(interval)).await??;
            if observed == watch::Observed::Exited {
                log.line("owner_exited");
                break;
            }
            if !owner::identity(args.owner_pid).is_some_and(|current| {
                current.same_process(
                    args.owner_pid,
                    args.owner_start_tvsec,
                    args.owner_start_tvusec,
                )
            }) {
                log.line("owner_replaced");
                break;
            }
        }
        match heartbeat(client, &record.session, &record.lease).await {
            Beat::Renewed | Beat::Domain => {
                failures = 0;
                backoff = Duration::from_millis(250);
            }
            Beat::Terminal(code) => {
                log.line(&format!("session_not_resumable {code}"));
                break;
            }
            Beat::Transport => {
                failures += 1;
                if failures >= MAX_TRANSPORT_FAILURES {
                    log.line("transport_unavailable");
                    break;
                }
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(interval);
            }
            Beat::LeaseGone => {
                let Some(session) = session_status(client, &record.session).await else {
                    failures += 1;
                    if failures >= MAX_TRANSPORT_FAILURES {
                        log.line("transport_unavailable");
                        break;
                    }
                    continue;
                };
                failures = 0;
                match session.lifecycle.as_str() {
                    "active" => match session.lease {
                        Some(lease) if lease.0 != record.lease => {
                            record.lease = lease.0;
                            let _ = registry::write(config, &record);
                            log.line("adopted_rotated_lease");
                        }
                        Some(_) => log.line("lease_still_live"),
                        None => {
                            log.line("lease_unavailable");
                            break;
                        }
                    },
                    // The owner is demonstrably still alive, so renewing is
                    // exactly right: this is what makes a laptop waking from
                    // sleep recover its workspace without the agent noticing.
                    "dormant" => match reattach(client, &record.session).await {
                        Some(lease) => {
                            record.lease = lease.0;
                            let _ = registry::write(config, &record);
                            log.line("reattached");
                        }
                        None => {
                            log.line("reattach_failed");
                            break;
                        }
                    },
                    // Materializing a tree is only ever explicit; never wake.
                    _ => {
                        log.line("session_not_resumable");
                        break;
                    }
                }
            }
        }
    }
    let _ = registry::remove(config, &args.session);
    log.line("stopped");
    Ok(())
}

/// Poll for this keepalive's own pidfile for a bounded moment.
///
/// Async because it runs on the child's runtime, before the heartbeat loop
/// starts: a `std::thread::sleep` here parks the whole reactor, and the child
/// has a signal handler and a socket client on it.
async fn await_pidfile(config: &EngineConfig, session: &str) -> Option<registry::KeepaliveRecord> {
    let deadline = Instant::now() + PIDFILE_WAIT;
    loop {
        if let Some(record) = registry::read(config, session) {
            return Some(record);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(STOP_POLL).await;
    }
}

pub fn clamp_interval(seconds: u64) -> u64 {
    seconds.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS)
}

fn disabled_by_environment() -> bool {
    std::env::var_os(DISABLE_ENV).is_some_and(|value| {
        let value = value.to_string_lossy().to_ascii_lowercase();
        !matches!(value.as_str(), "" | "0" | "false" | "no")
    })
}

fn skipped(reason: &str, owner_pid: Option<u32>) -> KeepaliveStatus {
    KeepaliveStatus {
        state: "skipped".into(),
        pid: None,
        owner_pid,
        owner: None,
        reason: Some(reason.into()),
    }
}

fn spawn(
    config: &EngineConfig,
    socket: &Path,
    session: &str,
    lease: &str,
    owner: &owner::ProcessIdentity,
    interval_secs: Option<u64>,
) -> Result<KeepaliveStatus, &'static str> {
    let executable = std::env::current_exe().map_err(|_| "executable_unavailable")?;
    registry::ensure_directory(config).map_err(|_| "registry_unavailable")?;
    let interval = clamp_interval(interval_secs.unwrap_or(DEFAULT_INTERVAL_SECS));
    let mut command = Command::new(executable);
    command
        .arg("--socket")
        .arg(socket)
        .args([
            "keepalive",
            "run",
            "--session",
            session,
            "--lease",
            lease,
            "--owner-pid",
            &owner.pid.to_string(),
            "--owner-start-tvsec",
            &owner.start_tvsec.to_string(),
            "--owner-start-tvusec",
            &owner.start_tvusec.to_string(),
            "--interval-secs",
            &interval.to_string(),
        ])
        // Detaching stdio keeps the caller's pipe read on `shade open` from
        // ever blocking on a child that outlives the command.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("SHADE_ROOT", &config.root)
        // A keepalive must never freeze under the crash harness, and must not
        // inherit the caller's session identity.
        .env_remove("SHADE_FAULT_POINT")
        .env_remove("SHADE_FAULT_DIR")
        .env_remove("SHADE_SESSION")
        .env_remove("SHADE_LEASE")
        .env_remove("SHADE_WORKSPACE")
        .env_remove(DISABLE_ENV);
    // SAFETY: `setsid` only rearranges the child's own session and process
    // group between fork and exec; it allocates nothing and is async-signal
    // safe. It is what lets the keepalive survive Ctrl-C on the agent and a
    // `kill -- -<pgid>` of the calling shell.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|_| "spawn_failed")?;
    let pid = child.id();
    // The parent writes the pidfile immediately, so a `release` arriving
    // milliseconds later always finds the child it has to stop.
    let identity = owner::identity(pid).ok_or("keepalive_identity_unavailable")?;
    let record = registry::KeepaliveRecord {
        v: registry::RECORD_VERSION,
        session: session.to_owned(),
        lease: lease.to_owned(),
        socket: socket.to_string_lossy().into_owned(),
        root: config.root.to_string_lossy().into_owned(),
        keepalive_pid: pid,
        keepalive_start_tvsec: identity.start_tvsec,
        keepalive_start_tvusec: identity.start_tvusec,
        owner_pid: owner.pid,
        owner_start_tvsec: owner.start_tvsec,
        owner_start_tvusec: owner.start_tvusec,
        owner_name: owner.name.clone(),
        created_at_ms: shade_engine::db::now_ms(),
    };
    registry::write(config, &record).map_err(|_| "registry_write_failed")?;
    Ok(KeepaliveStatus {
        state: "started".into(),
        pid: Some(pid),
        owner_pid: Some(owner.pid),
        owner: Some(owner.name.clone()),
        reason: None,
    })
}

/// A missing or stale pidfile is a success: the keepalive exits on its own
/// when the session reports `released` or `suspended`.
fn stop_registered(config: &EngineConfig, session: &str) -> Option<u32> {
    let record = registry::read(config, session)?;
    if !registry::is_live(&record) {
        let _ = registry::remove(config, session);
        return None;
    }
    let pid = record.keepalive_pid as libc::c_int;
    // SAFETY: `is_live` proved this pid is a live process running this same
    // binary with the recorded start time, so the signal cannot reach an
    // unrelated process.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline && registry::is_live(&record) {
        std::thread::sleep(STOP_POLL);
    }
    if registry::is_live(&record) {
        // SAFETY: as above; the identity check still holds.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline && registry::is_live(&record) {
            std::thread::sleep(STOP_POLL);
        }
    }
    let _ = registry::remove(config, session);
    Some(record.keepalive_pid)
}

enum Beat {
    Renewed,
    LeaseGone,
    /// A domain error the daemon says will never succeed, carrying its code.
    Terminal(String),
    Domain,
    Transport,
}

async fn heartbeat(client: &ShadeClient, session: &str, lease: &str) -> Beat {
    let intent = Intent::LeaseHeartbeat {
        session_id: SessionId(session.to_owned()),
        lease_id: LeaseId(lease.to_owned()),
    };
    let key = format!("keepalive:{}", ulid::Ulid::new());
    match client.execute_wait_idempotent(intent, key).await {
        Ok(response) => match response.body {
            ResponseBody::Ok { .. } => Beat::Renewed,
            ResponseBody::Error { error } => classify(&error),
        },
        Err(ClientError::Domain(error)) => classify(&error),
        Err(_) => Beat::Transport,
    }
}

/// Decide what one refused beat means for this child.
///
/// The two lease codes are the only ones worth another question: an expiry is
/// a dormancy the child reattaches from, and a fence may be a lease that
/// rotated under it. Every other `retry: never` answer names something no
/// amount of beating changes -- a suspended session, a released one, a
/// workspace that is gone -- and beating on was how a suspended session kept a
/// child alive until its agent exited. Retryable codes stay `Domain`: the
/// daemon is saying "not now", not "never".
fn classify(error: &ShadeError) -> Beat {
    match error.code.as_str() {
        "LEASE_EXPIRED" | "LEASE_FENCED" => Beat::LeaseGone,
        _ if error.retry == "never" => Beat::Terminal(error.code.clone()),
        _ => Beat::Domain,
    }
}

async fn session_status(client: &ShadeClient, session: &str) -> Option<SessionStatus> {
    let response = client
        .query(Query::Session {
            session_id: SessionId(session.to_owned()),
        })
        .await
        .ok()?;
    match response.body {
        ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        } => serde_json::from_value(value).ok(),
        _ => None,
    }
}

async fn reattach(client: &ShadeClient, session: &str) -> Option<LeaseId> {
    let intent = Intent::SessionReattach {
        session_id: SessionId(session.to_owned()),
    };
    let key = format!("keepalive-reattach:{}", ulid::Ulid::new());
    let response = client.execute_wait_idempotent(intent, key).await.ok()?;
    match response.body {
        ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        } => serde_json::from_value::<OpenedSession>(value)
            .ok()
            .map(|opened| opened.lease),
        _ => None,
    }
}

/// One append-only line per state change, never one per heartbeat.
struct Log {
    file: Option<std::fs::File>,
}

impl Log {
    fn open(config: &EngineConfig, session: &str) -> Self {
        if registry::ensure_directory(config).is_err() {
            return Self { file: None };
        }
        let path = registry::log_path(config, session);
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES) {
            let _ = std::fs::remove_file(&path);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .ok();
        Self { file }
    }

    fn line(&mut self, message: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let _ = writeln!(
            file,
            "{} {} {}",
            shade_engine::db::now_ms(),
            std::process::id(),
            message
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shade_protocol::PROTOCOL_VERSION;

    fn completed(value: Value) -> WireResponse {
        WireResponse {
            v: PROTOCOL_VERSION,
            request_id: "cli".into(),
            body: ResponseBody::Ok {
                outcome: Outcome::Completed(value),
            },
        }
    }

    #[test]
    fn the_interval_is_bounded_on_both_sides() {
        assert_eq!(clamp_interval(0), MIN_INTERVAL_SECS);
        assert_eq!(clamp_interval(30), 30);
        assert_eq!(clamp_interval(10_000), MAX_INTERVAL_SECS);
    }

    #[test]
    fn an_explicit_opt_out_reports_a_skipped_keepalive_and_never_spawns() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());
        let options = KeepaliveOptions {
            disabled: true,
            ..KeepaliveOptions::default()
        };
        let status = start(
            &config,
            &temporary.path().join("s.sock"),
            "session-1",
            "lease-1",
            &options,
        );
        assert_eq!(status.state, "skipped");
        assert_eq!(status.reason.as_deref(), Some("disabled"));
        assert!(status.pid.is_none());
        assert!(registry::read(&config, "session-1").is_none());
    }

    #[test]
    fn an_absent_owner_pid_is_reported_without_failing_the_command() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());
        let options = KeepaliveOptions {
            owner_pid: Some(u32::MAX - 1),
            ..KeepaliveOptions::default()
        };
        let status = start(
            &config,
            &temporary.path().join("s.sock"),
            "session-2",
            "lease-2",
            &options,
        );
        assert_eq!(status.state, "skipped");
        assert_eq!(status.reason.as_deref(), Some("owner_not_found"));
    }

    #[test]
    fn stopping_an_unknown_session_is_a_success() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());
        let stopped = stop(&config, "never-started");
        assert_eq!(stopped["stopped"], false);
        assert_eq!(stopped["reason"], "not_running");
        assert_eq!(status(&config, "never-started")["running"], false);
    }

    #[test]
    fn a_non_completed_response_is_returned_untouched() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());
        let response = WireResponse {
            v: PROTOCOL_VERSION,
            request_id: "cli".into(),
            body: ResponseBody::Error {
                error: shade_protocol::ShadeError {
                    code: "LEASE_EXPIRED".into(),
                    retry: "never".into(),
                    operation: None,
                    next: None,
                    diagnostics_id: None,
                },
            },
        };
        let annotated = annotate(
            &config,
            &temporary.path().join("s.sock"),
            &KeepaliveOptions::default(),
            response,
        );
        assert!(matches!(annotated.body, ResponseBody::Error { .. }));
    }

    #[test]
    fn a_disabled_keepalive_still_annotates_the_opened_session() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());
        let opened = json!({
            "session": "s1",
            "workspace": "ws_1",
            "lease": "lease_1",
            "cwd": "/tmp/ws_1",
            "env": {},
            "compact_context": {
                "workspace": "ws_1",
                "session": "s1",
                "base_ref": "origin/main",
                "base_sha": "a",
                "head_sha": "a",
                "changes": {"staged": 0, "unstaged": 0, "untracked": 0},
                "lease": "live",
                "lifecycle": "active",
                "dependencies": {"state": "ready"}
            }
        });
        let annotated = annotate(
            &config,
            &temporary.path().join("s.sock"),
            &KeepaliveOptions {
                disabled: true,
                ..KeepaliveOptions::default()
            },
            completed(opened),
        );
        let ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        } = annotated.body
        else {
            panic!("expected a completed outcome");
        };
        assert_eq!(value["keepalive"]["state"], "skipped");
    }

    #[test]
    fn a_release_review_keeps_its_keepalive() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());
        // No pidfile exists, so this only asserts the guards do not panic and
        // that a non-released result is ignored.
        stop_for_release(&config, &completed(json!({"session": "s1"})));
        stop_for_release(
            &config,
            &completed(json!({"session": "s1", "released": true})),
        );
    }

    /// A stale pidfile is the observable half of a stop: `stop_registered`
    /// removes it without signalling anything, so its absence proves the stop
    /// ran and its presence proves it did not.
    fn stale_record(config: &EngineConfig, session: &str) {
        registry::write(
            config,
            &registry::KeepaliveRecord {
                v: registry::RECORD_VERSION,
                session: session.into(),
                lease: "lease_1".into(),
                socket: "/tmp/shade.sock".into(),
                root: "/tmp/shade-root".into(),
                keepalive_pid: u32::MAX - 1,
                keepalive_start_tvsec: 1,
                keepalive_start_tvusec: 2,
                owner_pid: u32::MAX - 2,
                owner_start_tvsec: 3,
                owner_start_tvusec: 4,
                owner_name: "claude".into(),
                created_at_ms: 1_700_000_000_000,
            },
        )
        .unwrap();
    }

    /// Keep and discard both release the reviewed workspace's session, so both
    /// have to end its keepalive. Only `release` was wired, which left a child
    /// heartbeating a lease nothing held until its owner exited.
    #[test]
    fn keep_and_discard_stop_the_keepalive_and_merge_does_not() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());

        for resolution in ["kept", "discarded"] {
            stale_record(&config, "s1");
            stop_for_review(
                &config,
                &completed(json!({
                    "review": "rev_1",
                    "resolution": resolution,
                    "session": "s1",
                })),
            );
            assert!(
                registry::read(&config, "s1").is_none(),
                "{resolution} closes the session, so it closes the keepalive"
            );
        }

        // A merge hands off to a successor the session keeps living in, so its
        // keepalive has to survive. It arrives as an `accepted` handoff, not a
        // completed resolution, but guard the shape either way.
        stale_record(&config, "s1");
        stop_for_review(
            &config,
            &completed(json!({
                "handoff_id": "ho_1",
                "session": "s1",
                "predecessor": "ws_1",
                "successor": "ws_2",
            })),
        );
        assert!(registry::read(&config, "s1").is_some());

        // And a resolution the engine could not attribute to a session leaves
        // the registry alone rather than guessing.
        stop_for_review(
            &config,
            &completed(json!({
                "review": "rev_1",
                "resolution": "kept",
                "session": Value::Null,
            })),
        );
        assert!(registry::read(&config, "s1").is_some());
    }

    fn refused(code: &str, retry: &str) -> ShadeError {
        ShadeError {
            code: code.into(),
            retry: retry.into(),
            operation: None,
            next: None,
            diagnostics_id: None,
        }
    }

    #[test]
    fn heartbeat_classification_separates_lease_loss_from_other_domain_errors() {
        assert!(matches!(
            classify(&refused("LEASE_EXPIRED", "never")),
            Beat::LeaseGone
        ));
        assert!(matches!(
            classify(&refused("LEASE_FENCED", "never")),
            Beat::LeaseGone
        ));
        // Retryable: the daemon is saying "not now", and the next tick asks
        // again rather than abandoning a lease that is still this child's.
        assert!(matches!(
            classify(&refused("WORKSPACE_NOT_QUIESCENT", "safe")),
            Beat::Domain
        ));
    }

    /// Every terminal code that is not one of the two lease codes ends the
    /// child. `SESSION_SUSPENDED` is the one that made this a bug: a slept
    /// session answers it forever, and treating it as an ordinary domain
    /// error left a keepalive beating against a workspace with no tree until
    /// its agent exited.
    #[test]
    fn a_terminal_domain_error_ends_the_keepalive() {
        for code in [
            "SESSION_SUSPENDED",
            "SESSION_ALREADY_RELEASED",
            "SESSION_NOT_FOUND",
            "WORKSPACE_ALREADY_RELEASED",
            "WORKSPACE_NOT_MATERIALIZED",
            "WORKSPACE_NOT_ATTACHABLE",
            "WORKSPACE_NOT_FOUND",
        ] {
            match classify(&refused(code, "never")) {
                Beat::Terminal(reported) => assert_eq!(reported, code),
                _ => panic!("{code} must end the keepalive"),
            }
        }
    }
}
