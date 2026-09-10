use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_LEASE_TTL_SECS: i64 = 120;
pub const DEFAULT_ORPHAN_GRACE_SECS: i64 = 600;
/// Auto-sleep is off unless an operator asks for it. Sleeping is cheap and
/// reversible, but it still changes a workspace id out from under a host, so
/// nothing does it on its own by default.
pub const DEFAULT_AUTO_SLEEP_AFTER_SECS: Option<i64> = None;
/// The span `shade install` writes into the LaunchAgent when the installing
/// shell named none of its own.
///
/// The in-code default above stays `None`, because a library caller owns its
/// own lifecycle and a test that never asked for a sweep must not get one. An
/// installed daemon is the other case: it outlives every shell that talks to
/// it, nothing else will ever reclaim the trees its hosts abandoned, and three
/// days is longer than any agent turn and shorter than any disk fills.
pub const DEFAULT_INSTALLED_AUTO_SLEEP_DAYS: i64 = 3;
/// Suspension retention is off by default too: a suspension is the state that
/// exists to keep work indefinitely at almost no cost.
pub const DEFAULT_SUSPENDED_RETENTION_SECS: Option<i64> = None;
/// The smallest private build output worth moving to the park volume. Below
/// this the cross-volume byte copy costs more time than the boot disk gets
/// back, so `sleep` simply discards it as it always has.
pub const DEFAULT_PARK_MIN_BYTES: u64 = 256 * 1024 * 1024;
/// The longest span either sweep accepts, in days.
const MAX_LIFECYCLE_DAYS: i64 = 3650;
const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub root: PathBuf,
    pub socket: PathBuf,
    pub lease_ttl_secs: i64,
    /// How long a workspace waits after an explicit `release` before GC may
    /// delete it. It is not a dormancy timer: an expired lease never makes a
    /// workspace collectible.
    pub orphan_grace_secs: i64,
    pub operation_wait_ms: u64,
    /// Sleep a dormant workspace once it has been idle this long. `None`
    /// disables the sweep.
    pub auto_sleep_after_secs: Option<i64>,
    /// Release a suspended workspace once it has been suspended this long.
    /// `None` disables the sweep. Releasing is not deleting: GC still applies
    /// every one of its own gates afterwards.
    pub suspended_retention_secs: Option<i64>,
    /// External volume the parked tier copies private build output to. `None`
    /// -- the default -- keeps the current behaviour, where `sleep` discards
    /// everything Git ignores.
    pub park_root: Option<PathBuf>,
    /// Do not park build output smaller than this. See
    /// [`DEFAULT_PARK_MIN_BYTES`].
    pub park_min_bytes: u64,
    /// Keep a park after the wake that restored it. Off by default: the park
    /// belongs to a checkpoint that stops being a suspension checkpoint the
    /// moment the successor is activated, so keeping it is a debugging
    /// affordance rather than a tier the collector understands.
    pub park_keep_after_wake: bool,
}

impl EngineConfig {
    pub fn discover() -> anyhow::Result<Self> {
        let root = if let Some(value) = std::env::var_os("SHADE_ROOT") {
            PathBuf::from(value)
        } else {
            dirs::data_dir()
                .ok_or_else(|| anyhow::anyhow!("could not locate the user data directory"))?
                .join("Shade")
        };
        let mut config = Self::at(root);
        config.auto_sleep_after_secs = lifecycle_days("SHADE_AUTO_SLEEP_DAYS")?;
        config.suspended_retention_secs = lifecycle_days("SHADE_SUSPENDED_RETENTION_DAYS")?;
        config.park_root = park_root("SHADE_PARK_ROOT")?;
        config.park_min_bytes = park_min_bytes("SHADE_PARK_MIN_BYTES")?;
        config.park_keep_after_wake = flag("SHADE_PARK_KEEP_AFTER_WAKE");
        Ok(config)
    }

    pub fn at(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        Self {
            socket: root.join("shade.sock"),
            root,
            lease_ttl_secs: DEFAULT_LEASE_TTL_SECS,
            orphan_grace_secs: DEFAULT_ORPHAN_GRACE_SECS,
            operation_wait_ms: 5_000,
            auto_sleep_after_secs: DEFAULT_AUTO_SLEEP_AFTER_SECS,
            suspended_retention_secs: DEFAULT_SUSPENDED_RETENTION_SECS,
            park_root: None,
            park_min_bytes: DEFAULT_PARK_MIN_BYTES,
            park_keep_after_wake: false,
        }
    }

    /// Apply shorter lifecycle timings to an explicitly isolated harness daemon.
    ///
    /// The production discovery path never calls this. Keeping the override on
    /// the concrete config makes tests exercise the normal engine clock and GC
    /// predicates without editing SQLite or adding a second cleanup path.
    pub fn with_harness_lifecycle_timing(
        mut self,
        lease_ttl_secs: i64,
        orphan_grace_secs: i64,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=DEFAULT_LEASE_TTL_SECS).contains(&lease_ttl_secs),
            "harness lease TTL must be between 1 and {DEFAULT_LEASE_TTL_SECS} seconds"
        );
        anyhow::ensure!(
            (0..=DEFAULT_ORPHAN_GRACE_SECS).contains(&orphan_grace_secs),
            "harness orphan grace must be between 0 and {DEFAULT_ORPHAN_GRACE_SECS} seconds"
        );
        self.lease_ttl_secs = lease_ttl_secs;
        self.orphan_grace_secs = orphan_grace_secs;
        Ok(self)
    }

    /// Sleep dormant workspaces idle for longer than this. Validated on the
    /// way in so a misconfigured sweep cannot fire immediately or never.
    pub fn with_auto_sleep_after_secs(mut self, secs: i64) -> anyhow::Result<Self> {
        self.auto_sleep_after_secs = Some(checked_span(secs, "auto sleep")?);
        Ok(self)
    }

    /// Release suspended workspaces older than this.
    pub fn with_suspended_retention_secs(mut self, secs: i64) -> anyhow::Result<Self> {
        self.suspended_retention_secs = Some(checked_span(secs, "suspended retention")?);
        Ok(self)
    }

    pub fn repositories_dir(&self) -> PathBuf {
        self.root.join("repositories")
    }

    pub fn bases_dir(&self) -> PathBuf {
        self.root.join("bases")
    }

    pub fn workspaces_dir(&self) -> PathBuf {
        self.root.join("workspaces")
    }

    pub fn dependencies_dir(&self) -> PathBuf {
        self.root.join("dependencies")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.root.join("runtime")
    }

    /// Where the CLI keeps one private pidfile per keepalive. The daemon
    /// never reads this directory; it exists so a keepalive can be found and
    /// stopped by a later CLI invocation.
    pub fn keepalive_dir(&self) -> PathBuf {
        self.runtime_dir().join("keepalive")
    }

    pub fn secrets_dir(&self) -> PathBuf {
        self.root.join("secrets")
    }

    pub fn database_path(&self) -> PathBuf {
        self.root.join("state.sqlite")
    }

    /// Where the parked tier keeps private build output. `None` means the
    /// tier is off, which is what an unconfigured daemon reports.
    pub fn park_root(&self) -> Option<&Path> {
        self.park_root.as_deref()
    }

    pub fn park_min_bytes(&self) -> u64 {
        self.park_min_bytes
    }

    /// Should a restored park survive the wake that used it?
    pub fn park_keep_after_wake(&self) -> bool {
        self.park_keep_after_wake
    }

    /// Is the park volume there and writable right now?
    ///
    /// An external disk is unplugged far more often than it is misconfigured,
    /// so this is a question about the moment rather than about the config.
    /// It deliberately never creates the directory: a park root that has to be
    /// created is an unmounted volume's mountpoint, and filling that would put
    /// the parked bytes on the boot disk this tier exists to spare.
    pub fn park_root_mounted(&self) -> bool {
        self.park_root.as_deref().is_some_and(writable_directory)
    }
}

/// A boolean switch, on only for the exact string `1`.
///
/// Anything else -- unset, empty, `true`, `yes` -- leaves it off, because a
/// switch that guesses is worse than one that has to be spelled.
fn flag(variable: &str) -> bool {
    std::env::var_os(variable).is_some_and(|value| value == "1")
}

/// Whether the process may create entries inside an existing directory.
///
/// Asked of the kernel rather than derived from the mode bits, so an ACL, a
/// read-only mount or a foreign owner all answer honestly.
fn writable_directory(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a NUL-terminated C string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

fn checked_span(secs: i64, what: &str) -> anyhow::Result<i64> {
    anyhow::ensure!(
        (1..=MAX_LIFECYCLE_DAYS * SECONDS_PER_DAY).contains(&secs),
        "{what} must be between 1 second and {MAX_LIFECYCLE_DAYS} days"
    );
    Ok(secs)
}

/// Read a whole number of days from the environment. An unset variable leaves
/// the sweep off; a malformed one is an error rather than a silent default,
/// because both silent outcomes are wrong for a timer that moves work.
fn lifecycle_days(variable: &str) -> anyhow::Result<Option<i64>> {
    let raw = std::env::var_os(variable);
    let raw = raw
        .as_ref()
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("{variable} is not valid UTF-8"))
        })
        .transpose()?;
    parse_lifecycle_days(variable, raw)
}

fn parse_lifecycle_days(variable: &str, raw: Option<&str>) -> anyhow::Result<Option<i64>> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let days: i64 = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("{variable} must be a whole number of days"))?;
    anyhow::ensure!(
        (1..=MAX_LIFECYCLE_DAYS).contains(&days),
        "{variable} must be between 1 and {MAX_LIFECYCLE_DAYS} days"
    );
    Ok(Some(days * SECONDS_PER_DAY))
}

/// Read the park volume from the environment. Unlike the lifecycle spans this
/// is a path, so it is never required to be UTF-8; it is required to be
/// absolute, because the daemon's working directory is not the operator's.
fn park_root(variable: &str) -> anyhow::Result<Option<PathBuf>> {
    let raw = std::env::var_os(variable);
    parse_park_root(variable, raw.as_deref())
}

fn parse_park_root(variable: &str, raw: Option<&OsStr>) -> anyhow::Result<Option<PathBuf>> {
    let Some(raw) = raw.filter(|value| {
        !value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_whitespace())
    }) else {
        return Ok(None);
    };
    let path = PathBuf::from(raw);
    anyhow::ensure!(path.is_absolute(), "{variable} must be an absolute path");
    anyhow::ensure!(
        !path
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "{variable} must not contain `..`"
    );
    Ok(Some(path))
}

fn park_min_bytes(variable: &str) -> anyhow::Result<u64> {
    let raw = std::env::var_os(variable);
    let raw = raw
        .as_ref()
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("{variable} is not valid UTF-8"))
        })
        .transpose()?;
    parse_park_min_bytes(variable, raw)
}

fn parse_park_min_bytes(variable: &str, raw: Option<&str>) -> anyhow::Result<u64> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_PARK_MIN_BYTES);
    };
    raw.parse()
        .map_err(|_| anyhow::anyhow!("{variable} must be a whole number of bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_defaults_are_fixed_and_harness_timing_only_shortens_them() {
        let production = EngineConfig::at("/tmp/shade-config-test");
        assert_eq!(production.lease_ttl_secs, DEFAULT_LEASE_TTL_SECS);
        assert_eq!(production.orphan_grace_secs, DEFAULT_ORPHAN_GRACE_SECS);

        let harness = production
            .clone()
            .with_harness_lifecycle_timing(2, 0)
            .unwrap();
        assert_eq!(harness.lease_ttl_secs, 2);
        assert_eq!(harness.orphan_grace_secs, 0);
        assert_eq!(production.lease_ttl_secs, DEFAULT_LEASE_TTL_SECS);
        assert_eq!(production.orphan_grace_secs, DEFAULT_ORPHAN_GRACE_SECS);

        assert!(
            production
                .clone()
                .with_harness_lifecycle_timing(0, 0)
                .is_err()
        );
        assert!(
            production
                .with_harness_lifecycle_timing(
                    DEFAULT_LEASE_TTL_SECS,
                    DEFAULT_ORPHAN_GRACE_SECS + 1,
                )
                .is_err()
        );
    }

    #[test]
    fn sleep_and_retention_are_off_by_default_and_their_builders_validate() {
        let production = EngineConfig::at("/tmp/shade-config-sleep-test");
        assert_eq!(production.auto_sleep_after_secs, None);
        assert_eq!(production.suspended_retention_secs, None);

        let configured = production
            .clone()
            .with_auto_sleep_after_secs(7 * SECONDS_PER_DAY)
            .unwrap()
            .with_suspended_retention_secs(30 * SECONDS_PER_DAY)
            .unwrap();
        assert_eq!(configured.auto_sleep_after_secs, Some(604_800));
        assert_eq!(configured.suspended_retention_secs, Some(2_592_000));
        assert_eq!(production.auto_sleep_after_secs, None);

        assert!(production.clone().with_auto_sleep_after_secs(0).is_err());
        assert!(
            production
                .clone()
                .with_suspended_retention_secs(MAX_LIFECYCLE_DAYS * SECONDS_PER_DAY + 1)
                .is_err()
        );
    }

    #[test]
    fn park_is_off_by_default_and_its_root_must_be_absolute() {
        let variable = "SHADE_PARK_ROOT";
        assert_eq!(
            EngineConfig::at("/tmp/shade-config-park-test").park_root(),
            None
        );
        assert!(!EngineConfig::at("/tmp/shade-config-park-test").park_root_mounted());
        // A park is spent by the wake that restores it unless an operator says
        // otherwise, so the switch that keeps one is off by default.
        assert!(!EngineConfig::at("/tmp/shade-config-park-test").park_keep_after_wake());
        assert_eq!(parse_park_root(variable, None).unwrap(), None);
        assert_eq!(
            parse_park_root(variable, Some(OsStr::new("   "))).unwrap(),
            None
        );
        assert_eq!(
            parse_park_root(variable, Some(OsStr::new("/Volumes/dev-disk/shade-park"))).unwrap(),
            Some(PathBuf::from("/Volumes/dev-disk/shade-park"))
        );
        assert!(parse_park_root(variable, Some(OsStr::new("relative/park"))).is_err());
        assert!(parse_park_root(variable, Some(OsStr::new("/Volumes/../etc"))).is_err());
    }

    #[test]
    fn park_root_is_mounted_only_when_the_directory_is_there_and_writable() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = EngineConfig::at(temp.path().join("root"));
        config.park_root = Some(temp.path().join("absent"));
        assert!(!config.park_root_mounted());

        let park = temp.path().join("park");
        std::fs::create_dir_all(&park).unwrap();
        config.park_root = Some(park.clone());
        assert!(config.park_root_mounted());

        // A file where the volume should be is not a park root either.
        let file = temp.path().join("park-file");
        std::fs::write(&file, b"not a directory").unwrap();
        config.park_root = Some(file);
        assert!(!config.park_root_mounted());
    }

    #[test]
    fn park_min_bytes_defaults_and_rejects_anything_but_a_byte_count() {
        let variable = "SHADE_PARK_MIN_BYTES";
        assert_eq!(
            parse_park_min_bytes(variable, None).unwrap(),
            DEFAULT_PARK_MIN_BYTES
        );
        assert_eq!(
            parse_park_min_bytes(variable, Some("  ")).unwrap(),
            DEFAULT_PARK_MIN_BYTES
        );
        assert_eq!(parse_park_min_bytes(variable, Some("0")).unwrap(), 0);
        assert_eq!(
            parse_park_min_bytes(variable, Some(" 1048576 ")).unwrap(),
            1_048_576
        );
        for rejected in ["-1", "1.5", "256MiB", "lots"] {
            assert!(
                parse_park_min_bytes(variable, Some(rejected)).is_err(),
                "{rejected} should be rejected"
            );
        }
    }

    #[test]
    fn lifecycle_days_reads_whole_days_and_rejects_anything_else() {
        let variable = "SHADE_AUTO_SLEEP_DAYS";
        assert_eq!(
            parse_lifecycle_days(variable, Some("3")).unwrap(),
            Some(3 * SECONDS_PER_DAY)
        );
        assert_eq!(parse_lifecycle_days(variable, None).unwrap(), None);
        assert_eq!(parse_lifecycle_days(variable, Some("  ")).unwrap(), None);
        for rejected in ["0", "-1", "1.5", "many", "4000"] {
            assert!(
                parse_lifecycle_days(variable, Some(rejected)).is_err(),
                "{rejected} should be rejected"
            );
        }
    }
}
