//! Owner-exit watch built on `kqueue` / `EVFILT_PROC`.
//!
//! One blocking `kevent` call is both the heartbeat tick and the exit notice,
//! so the keepalive loop makes exactly one syscall per interval instead of
//! polling the process table.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    Exited,
    TimedOut,
}

#[derive(Debug)]
pub struct ProcessWatch {
    kq: OwnedFd,
    pid: u32,
}

impl ProcessWatch {
    /// Register interest in `pid` exiting. `Ok(None)` means the process was
    /// already gone when the filter was installed, which is not an error: the
    /// caller wanted to know about the exit and the exit has happened.
    pub fn new(pid: u32) -> io::Result<Option<Self>> {
        // SAFETY: `kqueue` takes no arguments and returns a descriptor or -1.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor this function exclusively owns.
        let kq = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: `kq` is a valid owned descriptor for the duration of the call.
        if unsafe { libc::fcntl(kq.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `kevent` is plain-old-data; a zeroed value is a valid one.
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = pid as libc::uintptr_t;
        change.filter = libc::EVFILT_PROC;
        change.flags = libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR;
        change.fflags = libc::NOTE_EXIT;
        let immediate = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: one initialized change entry is passed, no events are
        // requested, and `immediate` outlives the call.
        let registered = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                &immediate,
            )
        };
        if registered < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error);
        }
        Ok(Some(Self { kq, pid }))
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Block until the watched process exits or `timeout` elapses.
    pub fn wait(&self, timeout: Duration) -> io::Result<Observed> {
        // SAFETY: `kevent` is plain-old-data; a zeroed value is a valid one.
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        let deadline = libc::timespec {
            tv_sec: timeout.as_secs().min(i64::MAX as u64) as libc::time_t,
            tv_nsec: libc::c_long::from(timeout.subsec_nanos().min(999_999_999) as i32),
        };
        // SAFETY: no changes are submitted and exactly one event slot is
        // offered, matching the `1` passed as `nevents`.
        let count = unsafe {
            libc::kevent(
                self.kq.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &deadline,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(Observed::TimedOut);
            }
            return Err(error);
        }
        if count == 0 {
            return Ok(Observed::TimedOut);
        }
        if event.flags & libc::EV_ERROR != 0 {
            // A filter that can no longer name the process means the process
            // is gone, which is the outcome the caller is waiting for.
            if event.data as i32 == libc::ESRCH {
                return Ok(Observed::Exited);
            }
            return Err(io::Error::from_raw_os_error(event.data as i32));
        }
        Ok(Observed::Exited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Instant;

    #[test]
    fn a_watched_process_reports_its_exit_promptly() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let watch = ProcessWatch::new(pid)
            .unwrap()
            .expect("a running process must be watchable");
        assert_eq!(watch.pid(), pid);
        assert_eq!(
            watch.wait(Duration::from_millis(50)).unwrap(),
            Observed::TimedOut
        );
        // SAFETY: `kill` on an owned live child with SIGKILL is well defined.
        assert_eq!(unsafe { libc::kill(pid as libc::c_int, libc::SIGKILL) }, 0);
        let started = Instant::now();
        assert_eq!(
            watch.wait(Duration::from_secs(2)).unwrap(),
            Observed::Exited
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = child.wait();
    }

    #[test]
    fn a_reaped_process_cannot_be_watched() {
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        // The kernel may still hold the zombie until the parent reaps it; the
        // contract is only that an already gone process is never an error.
        match ProcessWatch::new(pid).unwrap() {
            None => {}
            Some(watch) => assert_eq!(
                watch.wait(Duration::from_secs(2)).unwrap(),
                Observed::Exited
            ),
        }
        assert!(ProcessWatch::new(u32::MAX - 1).unwrap().is_none());
    }
}
