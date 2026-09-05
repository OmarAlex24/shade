use std::path::{Path, PathBuf};

pub const DEFAULT_LEASE_TTL_SECS: i64 = 120;
pub const DEFAULT_ORPHAN_GRACE_SECS: i64 = 600;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub root: PathBuf,
    pub socket: PathBuf,
    pub lease_ttl_secs: i64,
    pub orphan_grace_secs: i64,
    pub operation_wait_ms: u64,
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
        Ok(Self::at(root))
    }

    pub fn at(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        Self {
            socket: root.join("shade.sock"),
            root,
            lease_ttl_secs: DEFAULT_LEASE_TTL_SECS,
            orphan_grace_secs: DEFAULT_ORPHAN_GRACE_SECS,
            operation_wait_ms: 5_000,
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

    pub fn secrets_dir(&self) -> PathBuf {
        self.root.join("secrets")
    }

    pub fn database_path(&self) -> PathBuf {
        self.root.join("state.sqlite")
    }
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
}
