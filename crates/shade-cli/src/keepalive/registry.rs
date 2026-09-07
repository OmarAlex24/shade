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

/// The record filed for this session under this root, or nothing.
///
/// The filename is a digest of the session id and the directory comes from the
/// root, so a record that disagrees with either was not written for this
/// caller: it is a collision, a copied state directory or a hand-edited file,
/// and none of those may hand a PID to `kill`.
pub fn read(config: &EngineConfig, session: &str) -> Option<KeepaliveRecord> {
    let record = read_path(&path(config, session))?;
    (record.session == session && Path::new(&record.root) == config.root).then_some(record)
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

/// A record is live only when its PID still names the same process, started at
/// the same instant, and owned by the same user.
///
/// It used to demand more: that the process also be running this same
/// executable, `proc_pidpath` and `current_exe` both put through
/// `canonicalize` because one reports the resolved vnode and the other the
/// path the process was launched through. That comparison has no inconclusive
/// answer -- an unresolvable path on either side returned `false` -- and
/// `false` here means "stale", which is the dangerous direction. Upgrade the
/// binary in place (`brew upgrade`, `cargo install`, any replace-and-rename)
/// and the file the running child was launched from is gone: every live
/// keepalive reads as stale, `stop_registered` unlinks the pidfile without
/// ever signalling, and the child goes on holding the lease with nothing left
/// that knows how to stop it.
///
/// The identity triple is what actually defeats PID reuse, and it does so
/// without reading a single path. The euid keeps the signal inside this user's
/// own processes; the record's own `root` and `session` are checked where it
/// is read, so a record only ever answers for the session it was filed under.
pub fn is_live(record: &KeepaliveRecord) -> bool {
    let Some(identity) = super::owner::identity(record.keepalive_pid) else {
        return false;
    };
    identity.same_process(
        record.keepalive_pid,
        record.keepalive_start_tvsec,
        record.keepalive_start_tvusec,
    ) && identity.uid == super::owner::effective_uid()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A record as it would be filed under `config`: `read` checks the body
    /// against the session and root it was filed for, so a fixture that lies
    /// about either is not readable back.
    fn filed(config: &EngineConfig, session: &str, pid: u32) -> KeepaliveRecord {
        KeepaliveRecord {
            root: config.root.to_string_lossy().into_owned(),
            ..record(session, pid)
        }
    }

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
        let written = filed(&config, "chat/42", 4242);
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

    /// A keepalive whose binary was replaced or deleted under it is still a
    /// running process holding a lease, and the only thing that can stop it is
    /// a signal. Reading it as stale is how an in-place upgrade orphaned every
    /// keepalive it inherited.
    #[test]
    fn a_live_process_stays_live_when_its_executable_no_longer_matches_ours() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let identity = super::super::owner::identity(child.id()).unwrap();
        let mut foreign = record("foreign-binary", child.id());
        foreign.keepalive_start_tvsec = identity.start_tvsec;
        foreign.keepalive_start_tvusec = identity.start_tvusec;
        assert_ne!(
            std::env::current_exe().unwrap(),
            std::path::Path::new("/bin/sleep")
        );
        assert!(
            is_live(&foreign),
            "the recorded process is alive, whatever file it was launched from"
        );

        let _ = child.kill();
        let _ = child.wait();
        assert!(!is_live(&foreign), "and dead once the process is gone");
    }

    #[test]
    fn a_record_filed_under_another_session_or_root_is_not_this_ones() {
        let temporary = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temporary.path());
        let mut written = filed(&config, "mine", 4242);
        write(&config, &written).unwrap();
        assert!(read(&config, "mine").is_some());

        // The filename is a digest, so a record whose body names another
        // session got there by collision, copy or hand edit. None of those may
        // hand a PID to `kill`.
        written.session = "someone-elses".into();
        std::fs::write(path(&config, "mine"), serde_json::to_vec(&written).unwrap()).unwrap();
        assert!(read(&config, "mine").is_none());

        written.session = "mine".into();
        written.root = "/somewhere/else".into();
        std::fs::write(path(&config, "mine"), serde_json::to_vec(&written).unwrap()).unwrap();
        assert!(read(&config, "mine").is_none());
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
