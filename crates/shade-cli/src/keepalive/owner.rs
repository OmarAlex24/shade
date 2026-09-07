//! Owner process identity and ancestor detection.
//!
//! The keepalive exists to hold a lease for exactly as long as the agent
//! process that asked for the workspace is alive. Identity is the triple
//! `(pid, start_tvsec, start_tvusec)`, so a recycled PID can never be mistaken
//! for the original owner.

/// Never walk further than this many ancestors. A pathological process tree
/// must not turn owner detection into an unbounded loop.
pub const MAX_ANCESTOR_HOPS: usize = 32;

/// Process names treated as agent owners when no explicit set is configured.
pub const DEFAULT_OWNER_NAMES: [&str; 6] = [
    "claude",
    "codex",
    "cursor",
    "cursor-agent",
    "kimi",
    "zumith",
];

pub const OWNER_NAMES_ENV: &str = "SHADE_OWNER_PROCESS_NAMES";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub start_tvsec: u64,
    pub start_tvusec: u64,
    pub name: String,
}

impl ProcessIdentity {
    pub fn same_process(&self, pid: u32, start_tvsec: u64, start_tvusec: u64) -> bool {
        self.pid == pid && self.start_tvsec == start_tvsec && self.start_tvusec == start_tvusec
    }
}

/// Read one process's durable identity, or `None` when it does not exist.
pub fn identity(pid: u32) -> Option<ProcessIdentity> {
    if pid == 0 {
        return None;
    }
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `proc_pidinfo` writes at most `size` bytes into a buffer that is
    // exactly `size` bytes of correctly aligned, zeroed storage.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the kernel reported a complete `proc_bsdinfo` write above.
    let info = unsafe { info.assume_init() };
    Some(ProcessIdentity {
        pid: info.pbi_pid,
        ppid: info.pbi_ppid,
        uid: info.pbi_uid,
        start_tvsec: info.pbi_start_tvsec,
        start_tvusec: info.pbi_start_tvusec,
        // `pbi_name` holds 32 bytes, so long agent names such as
        // `cursor-agent` survive; `pbi_comm` truncates at 16 and is the
        // fallback only when the kernel left the long name empty.
        name: fixed_name(&info.pbi_name).or_else(|| fixed_name(&info.pbi_comm))?,
    })
}

/// The effective uid of this process; the boundary for ownership detection.
pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` reads process-local state and cannot fail.
    unsafe { libc::geteuid() }
}

/// The nearest ancestor of this process whose name matches `names`, walking
/// from the immediate parent upwards. Returns `None` rather than guessing.
pub fn detect_owner(names: &[String]) -> Option<ProcessIdentity> {
    // SAFETY: `getppid` reads process-local state and cannot fail.
    let parent = unsafe { libc::getppid() } as u32;
    nearest_named_ancestor(parent, identity, names, effective_uid())
}

/// Walk a process chain from `start` upwards and return the first identity
/// whose name matches, comparing case-insensitively.
///
/// The walk stops at pid 0/1, at the hop limit, or at the first process owned
/// by another user: crossing a privilege boundary means the chain no longer
/// describes the caller's own agent.
pub fn nearest_named_ancestor(
    start: u32,
    lookup: impl Fn(u32) -> Option<ProcessIdentity>,
    names: &[String],
    euid: u32,
) -> Option<ProcessIdentity> {
    let mut current = start;
    for _ in 0..MAX_ANCESTOR_HOPS {
        if current == 0 || current == 1 {
            return None;
        }
        let identity = lookup(current)?;
        if identity.uid != euid {
            return None;
        }
        if names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&identity.name))
        {
            return Some(identity);
        }
        if identity.ppid == current {
            return None;
        }
        current = identity.ppid;
    }
    None
}

/// The configured owner name set: explicit flags first, then the environment,
/// then the built-in defaults. Each source replaces the previous one whole.
pub fn configured_names(flags: &[String]) -> Vec<String> {
    names_from(
        flags,
        std::env::var_os(OWNER_NAMES_ENV).map(|value| value.to_string_lossy().into_owned()),
    )
}

pub fn names_from(flags: &[String], environment: Option<String>) -> Vec<String> {
    if !flags.is_empty() {
        return flags.to_vec();
    }
    if let Some(value) = environment {
        let names = value
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !names.is_empty() {
            return names;
        }
    }
    DEFAULT_OWNER_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect()
}

fn fixed_name(raw: &[libc::c_char]) -> Option<String> {
    let bytes = raw
        .iter()
        .map(|value| *value as u8)
        .take_while(|byte| *byte != 0)
        .collect::<Vec<_>>();
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic(pid: u32, ppid: u32, name: &str, uid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            ppid,
            uid,
            start_tvsec: u64::from(pid),
            start_tvusec: 0,
            name: name.into(),
        }
    }

    fn chain(entries: Vec<ProcessIdentity>) -> impl Fn(u32) -> Option<ProcessIdentity> {
        move |pid| entries.iter().find(|entry| entry.pid == pid).cloned()
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn the_nearest_matching_ancestor_wins() {
        let lookup = chain(vec![
            synthetic(100, 90, "zsh", 501),
            synthetic(90, 80, "claude", 501),
            synthetic(80, 1, "codex", 501),
        ]);
        let found = nearest_named_ancestor(100, lookup, &names(&["claude", "codex"]), 501).unwrap();
        assert_eq!(found.pid, 90);
    }

    #[test]
    fn matching_is_case_insensitive_and_keeps_long_names_intact() {
        let lookup = chain(vec![
            synthetic(100, 90, "zsh", 501),
            synthetic(90, 1, "Cursor-Agent", 501),
        ]);
        let found = nearest_named_ancestor(100, lookup, &names(&["cursor-agent"]), 501).unwrap();
        assert_eq!(found.name, "Cursor-Agent");
        assert_eq!(found.pid, 90);
    }

    #[test]
    fn the_walk_stops_at_a_uid_boundary() {
        let lookup = chain(vec![
            synthetic(100, 90, "zsh", 501),
            synthetic(90, 80, "login", 0),
            synthetic(80, 1, "claude", 501),
        ]);
        assert!(nearest_named_ancestor(100, lookup, &names(&["claude"]), 501).is_none());
    }

    #[test]
    fn the_walk_stops_at_init_and_at_the_hop_limit() {
        let init = chain(vec![synthetic(100, 1, "zsh", 501)]);
        assert!(nearest_named_ancestor(100, init, &names(&["claude"]), 501).is_none());

        let limit = MAX_ANCESTOR_HOPS as u32;
        let mut deep = Vec::new();
        for step in 0..=limit {
            deep.push(synthetic(100 + step, 100 + step + 1, "zsh", 501));
        }
        deep.push(synthetic(100 + limit + 1, 1, "claude", 501));
        assert!(nearest_named_ancestor(100, chain(deep), &names(&["claude"]), 501).is_none());
    }

    #[test]
    fn an_unreadable_ancestor_ends_the_walk_without_guessing() {
        let lookup = chain(vec![synthetic(100, 90, "zsh", 501)]);
        assert!(nearest_named_ancestor(100, lookup, &names(&["claude"]), 501).is_none());
    }

    #[test]
    fn a_real_process_identity_is_readable_and_self_consistent() {
        let me = identity(std::process::id()).expect("current process identity");
        assert_eq!(me.pid, std::process::id());
        assert_eq!(me.uid, effective_uid());
        assert!(!me.name.is_empty());
        // SAFETY: `getppid` reads process-local state and cannot fail.
        assert_eq!(me.ppid, unsafe { libc::getppid() } as u32);

        let found = nearest_named_ancestor(
            std::process::id(),
            identity,
            std::slice::from_ref(&me.name),
            effective_uid(),
        )
        .expect("the running test binary is its own nearest named ancestor");
        assert!(found.same_process(me.pid, me.start_tvsec, me.start_tvusec));
    }

    #[test]
    fn an_absent_process_has_no_identity() {
        assert!(identity(0).is_none());
        assert!(identity(u32::MAX - 1).is_none());
    }

    /// The keepalive attaches to an agent, not to whatever shell happens to be
    /// up the chain. A plain `bash`/`make`/CI ancestry must decline under the
    /// shipped defaults rather than adopt one of them.
    #[test]
    fn a_plain_shell_ancestry_is_not_an_owner_under_the_default_names() {
        let defaults = names_from(&[], None);
        assert_eq!(defaults, names(&DEFAULT_OWNER_NAMES));
        for shell in ["sh", "bash", "zsh", "make", "runner", "login"] {
            let lookup = chain(vec![
                synthetic(100, 90, shell, 501),
                synthetic(90, 80, "make", 501),
                synthetic(80, 1, "sshd-session", 501),
            ]);
            assert!(
                nearest_named_ancestor(100, lookup, &defaults, 501).is_none(),
                "{shell} must not be adopted as an agent owner"
            );
        }
        // The same chain with a recognised agent above it is adopted.
        let lookup = chain(vec![
            synthetic(100, 90, "zsh", 501),
            synthetic(90, 80, "make", 501),
            synthetic(80, 1, "codex", 501),
        ]);
        assert_eq!(
            nearest_named_ancestor(100, lookup, &defaults, 501)
                .unwrap()
                .name,
            "codex"
        );
    }

    #[test]
    fn owner_names_prefer_flags_then_environment_then_the_defaults() {
        let flags = names(&["my-agent"]);
        assert_eq!(names_from(&flags, Some("ignored".into())), flags);
        assert_eq!(
            names_from(&[], Some(" alpha , beta ,, ".into())),
            names(&["alpha", "beta"])
        );
        let defaults = names_from(&[], Some("   ".into()));
        assert_eq!(defaults, names(&DEFAULT_OWNER_NAMES));
        assert_eq!(names_from(&[], None), names(&DEFAULT_OWNER_NAMES));
    }
}
