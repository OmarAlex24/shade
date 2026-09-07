use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::filesystem::{ApfsFilesystem, WorkspaceFilesystem};

mod common;
mod javascript;
mod native;
mod python;
mod scripts;

pub use javascript::{BunProvider, NpmProvider, PnpmProvider};
pub use native::{CargoProvider, GoProvider};
pub use python::UvProvider;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyReceipt {
    pub provider: String,
    pub fingerprint: String,
    pub state: String,
    pub materialized_paths: Vec<String>,
    pub blocked_builds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<shade_protocol::DependencyScript>,
}

pub const DEFAULT_DEPENDENCY_CACHE_BYTES: u64 = 20 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyGcEntry {
    pub provider: String,
    pub fingerprint: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyGcReport {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub protected_bytes: u64,
    pub removed_bytes: u64,
    pub removed: Vec<DependencyGcEntry>,
    pub skipped_unowned: u64,
}

#[derive(Clone, Copy)]
pub struct DependencyContext<'a> {
    pub repository_root: &'a Path,
    pub workspace_root: &'a Path,
    pub cache_root: &'a Path,
    pub runtime_root: &'a Path,
    /// Filesystem capability used for workspace fan-out. Production passes an
    /// `ApfsFilesystem`; tests may deliberately pass `CopyFilesystem`, which is
    /// compiled only under `cfg(test)` or the `test-support` feature.
    pub filesystem: &'a dyn WorkspaceFilesystem,
    /// Decisions supplied by the control plane, never repository configuration.
    pub script_approvals: &'a [shade_protocol::ScriptApproval],
}

impl fmt::Debug for DependencyContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DependencyContext")
            .field("repository_root", &self.repository_root)
            .field("workspace_root", &self.workspace_root)
            .field("cache_root", &self.cache_root)
            .field("runtime_root", &self.runtime_root)
            .field("filesystem", &"WorkspaceFilesystem")
            .finish()
    }
}

static PRODUCTION_FILESYSTEM: ApfsFilesystem = ApfsFilesystem;

impl<'a> DependencyContext<'a> {
    /// Construct the production readiness boundary. There is intentionally no
    /// full-copy fallback: unsupported volumes surface `COW_UNAVAILABLE`.
    pub fn production(
        repository_root: &'a Path,
        workspace_root: &'a Path,
        cache_root: &'a Path,
        runtime_root: &'a Path,
    ) -> Self {
        Self {
            repository_root,
            workspace_root,
            cache_root,
            runtime_root,
            filesystem: &PRODUCTION_FILESYSTEM,
            script_approvals: &[],
        }
    }

    pub fn with_filesystem(
        repository_root: &'a Path,
        workspace_root: &'a Path,
        cache_root: &'a Path,
        runtime_root: &'a Path,
        filesystem: &'a dyn WorkspaceFilesystem,
    ) -> Self {
        Self {
            repository_root,
            workspace_root,
            cache_root,
            runtime_root,
            filesystem,
            script_approvals: &[],
        }
    }

    pub fn with_script_approvals(
        mut self,
        approvals: &'a [shade_protocol::ScriptApproval],
    ) -> Self {
        self.script_approvals = approvals;
        self
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum DependencyError {
    #[error("dependency provider is unavailable: {0}")]
    ToolUnavailable(String),
    #[error("dependency lock is missing: {0}")]
    LockMissing(String),
    #[error("dependency lock is stale: {0}")]
    LockStale(String),
    #[error("dependency policy blocks this project: {0}")]
    Policy(String),
    #[error("dependency preparation failed: {0}")]
    Failed(String),
    #[error("{provider} configuration {path} is invalid: {reason}")]
    InvalidConfiguration {
        provider: String,
        path: String,
        reason: String,
    },
    #[error("{provider} configuration {path} is unsafe: {reason}")]
    UnsafeConfiguration {
        provider: String,
        path: String,
        reason: String,
    },
    #[error("{provider} lock {path} is invalid: {reason}")]
    InvalidLock {
        provider: String,
        path: String,
        reason: String,
    },
    #[error("{tool} version mismatch: expected {expected}, found {actual}")]
    ToolVersionMismatch {
        tool: String,
        expected: String,
        actual: String,
    },
    #[error("{provider} dependency validation failed: {reason}")]
    Validation { provider: String, reason: String },
    #[error("{provider} {phase} command {command} failed with status {status:?}: {stderr}")]
    CommandFailed {
        provider: String,
        phase: String,
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    #[error("could not {action} ({path}): {message}")]
    Io {
        action: String,
        path: String,
        message: String,
    },
    #[error("COW_UNAVAILABLE: could not clone dependency layer {path}: {reason}")]
    CowUnavailable { path: String, reason: String },
}

#[async_trait]
pub trait DependencyProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn applies(&self, repository_root: &Path) -> bool;
    async fn ensure_ready(
        &self,
        context: &DependencyContext<'_>,
    ) -> Result<DependencyReceipt, DependencyError>;
}

pub struct DependencyService {
    providers: Vec<Box<dyn DependencyProvider>>,
}

impl DependencyService {
    pub fn new(providers: Vec<Box<dyn DependencyProvider>>) -> Self {
        Self { providers }
    }

    pub fn production() -> Self {
        Self::new(default_providers())
    }

    pub async fn ensure_all(
        &self,
        context: &DependencyContext<'_>,
    ) -> Result<Vec<DependencyReceipt>, DependencyError> {
        self.ensure_providers(
            context,
            self.providers
                .iter()
                .map(|provider| provider.as_ref())
                .collect(),
        )
        .await
    }

    /// Forks already own a faithful clone of workspace dependency bytes. Only
    /// external native caches need revalidation; immutable layers must never
    /// overwrite the agent's cloned edits.
    pub async fn inherit_workspace(
        &self,
        context: &DependencyContext<'_>,
        source_root: &Path,
        mut inherited: Vec<DependencyReceipt>,
    ) -> Result<Vec<DependencyReceipt>, DependencyError> {
        for receipt in &inherited {
            let (provider, relative) = receipt
                .provider
                .split_once('@')
                .unwrap_or((&receipt.provider, ""));
            if provider == "uv" {
                let relative = Path::new(relative);
                if !relative.as_os_str().is_empty() {
                    common::ensure_safe_relative(relative)?;
                }
                python::relocate_forked_venv(
                    &context.workspace_root.join(relative).join(".venv"),
                    &source_root.join(relative).join(".venv"),
                )?;
            }
        }
        let native = |name: &str| matches!(name.split('@').next(), Some("cargo" | "go"));
        let refreshed = self
            .ensure_providers(
                context,
                self.providers
                    .iter()
                    .filter(|provider| native(provider.name()))
                    .map(|provider| provider.as_ref())
                    .collect(),
            )
            .await?;
        inherited.retain(|receipt| !native(&receipt.provider));
        inherited.extend(refreshed);
        inherited.sort_by(|left, right| left.provider.cmp(&right.provider));
        Ok(inherited)
    }

    async fn ensure_providers(
        &self,
        context: &DependencyContext<'_>,
        providers: Vec<&dyn DependencyProvider>,
    ) -> Result<Vec<DependencyReceipt>, DependencyError> {
        let mut receipts = Vec::new();
        for provider in providers {
            for root in dependency_roots(provider.name(), context.repository_root)? {
                if !provider.applies(&root) {
                    continue;
                }
                let relative = root.strip_prefix(context.repository_root).map_err(|_| {
                    DependencyError::Failed("dependency root escaped the repository".into())
                })?;
                let workspace_root = context.workspace_root.join(relative);
                let scoped = DependencyContext::with_filesystem(
                    &root,
                    &workspace_root,
                    context.cache_root,
                    context.runtime_root,
                    context.filesystem,
                )
                .with_script_approvals(context.script_approvals);
                let mut receipt = provider.ensure_ready(&scoped).await?;
                if !relative.as_os_str().is_empty() {
                    receipt.provider = format!(
                        "{}@{}",
                        provider.name(),
                        relative.to_string_lossy().replace('\\', "/")
                    );
                }
                receipts.push(receipt);
            }
        }
        receipts.sort_by(|left, right| left.provider.cmp(&right.provider));
        Ok(receipts)
    }

    /// Apply an LRU byte ceiling to daemon-owned JavaScript/Python layers.
    /// Cargo and Go native caches and their receipts are outside this GC domain.
    pub fn garbage_collect<I, S>(
        &self,
        cache_root: &Path,
        protected_fingerprints: I,
        max_bytes: u64,
    ) -> Result<DependencyGcReport, DependencyError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        common::garbage_collect(
            cache_root,
            protected_fingerprints
                .into_iter()
                .map(|fingerprint| fingerprint.as_ref().to_owned())
                .collect(),
            max_bytes,
        )
    }

    pub fn cleanup_staging(&self, cache_root: &Path) -> Result<u64, DependencyError> {
        common::cleanup_staging(cache_root)
    }

    pub fn garbage_collect_default<I, S>(
        &self,
        cache_root: &Path,
        protected_fingerprints: I,
    ) -> Result<DependencyGcReport, DependencyError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.garbage_collect(
            cache_root,
            protected_fingerprints,
            DEFAULT_DEPENDENCY_CACHE_BYTES,
        )
    }
}

fn dependency_roots(
    provider: &str,
    repository_root: &Path,
) -> Result<Vec<PathBuf>, DependencyError> {
    let manifests: &[&str] = match provider {
        "bun" | "pnpm" | "npm" => &["package.json"],
        "uv" => &["pyproject.toml"],
        "cargo" => &["Cargo.toml"],
        "go" => &["go.mod", "go.work"],
        _ => return Ok(vec![repository_root.to_path_buf()]),
    };
    let mut directories = BTreeSet::new();
    for entry in WalkDir::new(repository_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_dir()
                || !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "node_modules" | ".venv" | "target" | ".shade")
                )
        })
    {
        let entry = entry.map_err(|error| DependencyError::Io {
            action: "scan dependency roots".into(),
            path: repository_root.to_string_lossy().into_owned(),
            message: error.to_string(),
        })?;
        if entry.file_type().is_file()
            && manifests
                .iter()
                .any(|name| entry.file_name() == OsStr::new(name))
            && let Some(parent) = entry.path().parent()
        {
            directories.insert(parent.to_path_buf());
        }
    }
    let mut directories = directories.into_iter().collect::<Vec<_>>();
    directories.sort_by_key(|path| path.components().count());
    let selected = match provider {
        "bun" | "pnpm" | "npm" => select_workspace_roots(
            repository_root,
            directories,
            &[
                "bun.lock",
                "bun.lockb",
                "pnpm-lock.yaml",
                "package-lock.json",
                "npm-shrinkwrap.json",
            ],
        ),
        "uv" => select_workspace_roots(repository_root, directories, &["uv.lock"]),
        "cargo" => select_workspace_roots(repository_root, directories, &["Cargo.lock"]),
        "go" => {
            let work_roots = directories
                .iter()
                .filter(|directory| directory.join("go.work").is_file())
                .cloned()
                .collect::<Vec<_>>();
            let work_members = work_roots
                .iter()
                .flat_map(|root| go_work_members(root))
                .collect::<BTreeSet<_>>();
            directories
                .into_iter()
                .filter(|directory| {
                    directory.join("go.work").is_file()
                        || (directory.join("go.mod").is_file() && !work_members.contains(directory))
                })
                .collect()
        }
        _ => directories,
    };
    Ok(selected)
}

fn go_work_members(work_root: &Path) -> Vec<PathBuf> {
    let Ok(source) = std::fs::read_to_string(work_root.join("go.work")) else {
        return Vec::new();
    };
    let mut members = Vec::new();
    let mut in_use_block = false;
    for raw in source.lines() {
        let line = raw.split("//").next().unwrap_or_default().trim();
        if line == "use (" {
            in_use_block = true;
            continue;
        }
        if in_use_block && line == ")" {
            in_use_block = false;
            continue;
        }
        let candidate = if in_use_block {
            line
        } else if let Some(value) = line.strip_prefix("use ") {
            value.trim()
        } else {
            continue;
        };
        let candidate = candidate.trim_matches('"');
        let path = Path::new(candidate);
        if !candidate.is_empty()
            && !path.is_absolute()
            && !path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            members.push(work_root.join(path));
        }
    }
    members
}

fn select_workspace_roots(
    repository_root: &Path,
    directories: Vec<PathBuf>,
    locks: &[&str],
) -> Vec<PathBuf> {
    let mut selected: Vec<PathBuf> = Vec::new();
    for directory in directories {
        let owns_lock = locks.iter().any(|name| directory.join(name).is_file());
        let covered = selected
            .iter()
            .any(|root| directory != *root && directory.starts_with(root));
        if directory == repository_root || owns_lock || !covered {
            selected.push(directory);
        }
    }
    selected
}

pub fn default_providers() -> Vec<Box<dyn DependencyProvider>> {
    vec![
        Box::new(BunProvider::new()),
        Box::new(PnpmProvider::new()),
        Box::new(NpmProvider::new()),
        Box::new(UvProvider::new()),
        Box::new(CargoProvider::new()),
        Box::new(GoProvider::new()),
    ]
}

pub(crate) fn command_path(name: &str) -> Result<PathBuf, DependencyError> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for root in std::env::split_paths(&path) {
        let candidate = root.join(name);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(DependencyError::ToolUnavailable(name.to_string()))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests;
