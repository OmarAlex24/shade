use std::path::{Path, PathBuf};

pub const DEFAULT_LEASE_TTL_SECS: i64 = 120;
pub const DEFAULT_ORPHAN_GRACE_SECS: i64 = 600;
/// Auto-sleep is off unless an operator asks for it. Sleeping is cheap and
/// reversible, but it still changes a workspace id out from under a host, so
/// nothing does it on its own by default.
pub const DEFAULT_AUTO_SLEEP_AFTER_SECS: Option<i64> = None;
/// Suspension retention is off by default too: a suspension is the state that
/// exists to keep work indefinitely at almost no cost.
pub const DEFAULT_SUSPENDED_RETENTION_SECS: Option<i64> = None;
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
