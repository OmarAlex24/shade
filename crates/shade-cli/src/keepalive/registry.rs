//! Client-side keepalive registry.
//!
//! The daemon deliberately holds no keepalive state: a keepalive is a
//! client-side liveness observer, so a daemon restart needs no keepalive
//! recovery logic at all. One private pidfile per session records who is
//! observing what, keyed by the SHA-256 of the session id because host-provided
//! session ids may contain path separators.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shade_engine::config::EngineConfig;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const RECORD_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeepaliveRecord {
    pub v: u32,
    pub session: String,
    pub lease: String,
    pub socket: String,
    pub root: String,
    pub keepalive_pid: u32,
    pub keepalive_start_tvsec: u64,
    pub keepalive_start_tvusec: u64,
    pub owner_pid: u32,
    pub owner_start_tvsec: u64,
    pub owner_start_tvusec: u64,
    pub owner_name: String,
    pub created_at_ms: i64,
}

pub fn directory(config: &EngineConfig) -> PathBuf {
    config.keepalive_dir()
}

/// Session ids are opaque host strings and may contain `/`; the digest keeps
/// every record inside the keepalive directory.
pub fn digest(session: &str) -> String {
    hex::encode(Sha256::digest(session.as_bytes()))
}

pub fn path(config: &EngineConfig, session: &str) -> PathBuf {
    directory(config).join(format!("{}.json", digest(session)))
}

pub fn log_path(config: &EngineConfig, session: &str) -> PathBuf {
    directory(config).join(format!("{}.log", digest(session)))
}

pub fn ensure_directory(config: &EngineConfig) -> io::Result<PathBuf> {
    let directory = directory(config);
    // `DirBuilder::mode` is a creation mode, so the caller's umask still
    // subtracts from it: under a restrictive umask this makes a directory the
    // owner cannot enter, and every later write into it fails with EACCES.
    // Set the mode we actually require afterwards, the way the socket and the
    // state file do, on the runtime root as well as on the leaf.
    for path in [config.runtime_dir(), directory.clone()] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

/// Write the record through a private staging file and one atomic rename, so a
/// reader never observes a half-written pidfile.
pub fn write(config: &EngineConfig, record: &KeepaliveRecord) -> io::Result<()> {
    let directory = ensure_directory(config)?;
    let final_path = path(config, &record.session);
    let staging = directory.join(format!(
        ".{}.{}.staging",
        digest(&record.session),
        std::process::id()
    ));
    let encoded = serde_json::to_vec(record).map_err(io::Error::other)?;
    let result = (|| -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&staging)?;
        std::io::Write::write_all(&mut file, &encoded)?;
        file.sync_all()?;
        std::fs::rename(&staging, &final_path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

pub fn read(config: &EngineConfig, session: &str) -> Option<KeepaliveRecord> {
    read_path(&path(config, session))
}

pub fn read_path(path: &Path) -> Option<KeepaliveRecord> {
    let encoded = std::fs::read(path).ok()?;
    let record: KeepaliveRecord = serde_json::from_slice(&encoded).ok()?;
    (record.v == RECORD_VERSION).then_some(record)
}

pub fn remove(config: &EngineConfig, session: &str) -> io::Result<()> {
    match std::fs::remove_file(path(config, session)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// A record is live only when its PID still names the same process *and* that
/// process is still this binary. Anything else is stale and safe to replace.
pub fn is_live(record: &KeepaliveRecord) -> bool {
    let Some(identity) = super::owner::identity(record.keepalive_pid) else {
        return false;
    };
    if !identity.same_process(
        record.keepalive_pid,
        record.keepalive_start_tvsec,
        record.keepalive_start_tvusec,
    ) {
        return false;
    }
    match (
        super::owner::executable_path(record.keepalive_pid).and_then(resolved),
        std::env::current_exe().ok().and_then(resolved),
    ) {
        (Some(running), Some(current)) => running == current,
        _ => false,
    }
}

/// Both sides of the identity check, resolved to the same real file.
///
/// `proc_pidpath` reports the resolved vnode while `current_exe` reports the
/// path the process was launched through. Invoked through a symlink -- a
/// Homebrew shim, `~/.local/bin/shade`, anything on PATH -- the two strings
/// differ, and comparing them raw made every keepalive look like it belonged
/// to a different program: `stop_registered` deleted pidfiles without ever
/// signalling, so each `open` left another orphaned child behind, and
/// `shade status` called a running keepalive `stale`.
fn resolved(path: PathBuf) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn record(session: &str, pid: u32) -> KeepaliveRecord {
        KeepaliveRecord {
            v: RECORD_VERSION,
            session: session.into(),
            lease: "lease_1".into(),
            socket: "/tmp/shade.sock".into(),
            root: "/tmp/shade-root".into(),
            keepalive_pid: pid,
            keepalive_start_tvsec: 1,
            keepalive_start_tvusec: 2,
            owner_pid: pid + 1,
            owner_start_tvsec: 3,
            owner_start_tvusec: 4,
            owner_name: "claude".into(),
            created_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn a_record_round_trips_through_a_private_pidfile() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());
        let written = record("chat/42", 4242);
        write(&config, &written).unwrap();
        assert_eq!(read(&config, "chat/42").as_ref(), Some(&written));

        let file = path(&config, "chat/42");
        assert_eq!(
            file.file_name().unwrap().to_string_lossy(),
            format!("{}.json", digest("chat/42"))
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(directory(&config))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(
            std::fs::read_dir(directory(&config))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".staging")),
            "staging files must not survive a successful write"
        );

        remove(&config, "chat/42").unwrap();
        assert!(read(&config, "chat/42").is_none());
        remove(&config, "chat/42").unwrap();
    }

    #[test]
    fn the_digest_keeps_every_record_inside_the_keepalive_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());
        for session in ["../escape", "a/b/c", "plain"] {
            assert_eq!(
                path(&config, session).parent(),
                Some(directory(&config).as_path())
            );
            assert_eq!(digest(session).len(), 64);
        }
    }

    #[test]
    fn a_record_is_stale_when_the_pid_is_absent_or_started_at_another_time() {
        assert!(!is_live(&record("absent", u32::MAX - 1)));

        let mut mine = record("mine", std::process::id());
        let identity = super::super::owner::identity(std::process::id()).unwrap();
        mine.keepalive_start_tvsec = identity.start_tvsec;
        mine.keepalive_start_tvusec = identity.start_tvusec;
        assert!(is_live(&mine), "the running test binary is its own process");

        mine.keepalive_start_tvsec += 1;
        assert!(!is_live(&mine), "a start-time mismatch defeats PID reuse");
    }

    #[test]
    fn an_unreadable_or_foreign_record_is_ignored() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());
        ensure_directory(&config).unwrap();
        std::fs::write(path(&config, "broken"), b"{not json}").unwrap();
        assert!(read(&config, "broken").is_none());

        let mut future = record("future", 1);
        future.v = RECORD_VERSION + 1;
        let encoded = serde_json::to_vec(&future).unwrap();
        std::fs::write(path(&config, "future"), encoded).unwrap();
        assert!(read(&config, "future").is_none());
    }
}
