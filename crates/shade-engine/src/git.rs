//! Concrete, policy-enforcing orchestration around the system Git binary.
//!
//! Shade intentionally does not embed a Git implementation. This keeps the
//! object format opaque (SHA-1 and SHA-256 repositories both work), while the
//! explicit refspecs below make the boundary around `refs/shade/` auditable.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, anyhow, bail, ensure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use url::Url;

const PRIVATE_REFS: &str = "refs/shade";
const QUARANTINE_REF: &str = "refs/shade/quarantine/fetched-head";
const ORIGIN: &str = "origin";
const LFS_POINTER_HEADER: &[u8] = b"version https://git-lfs.github.com/spec/v1";
const CHECKPOINT_STABILITY_ATTEMPTS: usize = 4;

/// A Git object name. Deliberately opaque: callers must not assume a
/// forty-character SHA-1 object id.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(String);

impl Oid {
    pub fn new(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        ensure!(!value.is_empty(), "an object id cannot be empty");
        ensure!(
            value.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "object id is not hexadecimal"
        );
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Oid").field(&self.0).finish()
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::str::FromStr for Oid {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIdentity {
    /// Stable, credential-free identity used for deduplication.
    pub canonical: String,
    /// Credential-free transport URL passed to Git. Authentication is left
    /// to credential helpers and SSH agents.
    pub fetch_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIdentity {
    /// Local checkout used only as the seed for the first managed import.
    pub worktree: PathBuf,
    pub common_git_dir: PathBuf,
    /// Managed repository identity. This is the canonical origin when one is
    /// configured, otherwise the canonical file URL of `common_git_dir`.
    pub canonical: String,
    /// Credential-free transport used for strict freshness checks.
    pub remote: RemoteIdentity,
    /// Distinguishes an authoritative upstream from the local-only fallback.
    pub origin_configured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryIdentity {
    Remote(RemoteIdentity),
    Local(LocalIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRepository {
    git_dir: PathBuf,
}

impl ManagedRepository {
    pub fn new(git_dir: impl Into<PathBuf>) -> Self {
        Self {
            git_dir: git_dir.into(),
        }
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseSpec {
    RemoteHead,
    OriginBranch(String),
    ExistingOid(Oid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRevision {
    pub commit: Oid,
    pub tree: Oid,
    pub source_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeHandle {
    pub root: PathBuf,
    pub admin_dir: PathBuf,
    pub head: Oid,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeStatus {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicted: usize,
    pub unmerged_paths: Vec<PathBuf>,
}

impl WorktreeStatus {
    pub fn is_clean(&self) -> bool {
        self.staged == 0 && self.unstaged == 0 && self.untracked == 0 && self.conflicted == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub ref_prefix: String,
    pub head: Oid,
    pub index_tree: Oid,
    pub working_tree: Oid,
    pub index_commit: Oid,
    pub working_commit: Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckpointSnapshot {
    head: Oid,
    index_tree: Oid,
    working_tree: Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    pub previous_remote: Option<Oid>,
    pub commit: Oid,
    pub tree: Oid,
    pub target_ref: String,
}

/// Immutable, privately anchored publish candidate. Creating this value never
/// changes a user-visible local or remote branch. The anchor keeps the commit
/// reachable while SQLite durably records and advances the publish saga.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPublish {
    pub previous_remote: Option<Oid>,
    pub commit: Oid,
    pub tree: Oid,
    pub target_ref: String,
    pub anchor_ref: String,
}

#[derive(Debug, Clone, Copy)]
pub struct SquashPublishRequest<'a> {
    pub remote: &'a RemoteIdentity,
    pub branch: &'a str,
    pub original_base: &'a Oid,
    pub expected_remote: Option<&'a Oid>,
    pub expected_local: Option<&'a Oid>,
    pub checkpoint: &'a Checkpoint,
    pub message: &'a str,
    pub push: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct PrepareSquashPublishRequest<'a> {
    pub remote: &'a RemoteIdentity,
    pub branch: &'a str,
    pub original_base: &'a Oid,
    pub expected_remote: Option<&'a Oid>,
    pub checkpoint: &'a Checkpoint,
    pub message: &'a str,
    pub anchor_ref: &'a str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrationOutcome {
    pub clean: bool,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct GitStore {
    binary: PathBuf,
    counters: Option<Arc<GitCounters>>,
    content_filter: Option<(PathBuf, PathBuf)>,
}

#[derive(Debug, Default)]
pub struct GitCounters {
    source_fetches: AtomicUsize,
    base_materializations: AtomicUsize,
}

impl GitCounters {
    pub fn source_fetches(&self) -> usize {
        self.source_fetches.load(Ordering::SeqCst)
    }

    pub fn base_materializations(&self) -> usize {
        self.base_materializations.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
struct RawOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    success: bool,
    code: Option<i32>,
}

impl GitStore {
    pub fn system() -> Self {
        Self {
            binary: PathBuf::from("git"),
            counters: None,
            content_filter: None,
        }
    }

    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            counters: None,
            content_filter: None,
        }
    }

    pub fn with_content_filter(mut self, executable: PathBuf, spool_root: PathBuf) -> Self {
        self.content_filter = Some((executable, spool_root));
        self
    }

    pub fn with_counters(mut self, counters: Arc<GitCounters>) -> Self {
        self.counters = Some(counters);
        self
    }

    /// Escape hatch used by diagnostics. Lifecycle code should prefer the
    /// policy-bearing methods on `GitStore`.
    pub async fn run<I, S>(&self, cwd: Option<&Path>, args: I) -> anyhow::Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|argument| argument.as_ref().to_owned())
            .collect();
        let output = self.run_raw(cwd, &args, &[], None).await?;
        self.require_success(&output)?;
        Ok(GitOutput {
            stdout: String::from_utf8(output.stdout)?
                .trim_end_matches(['\r', '\n'])
                .to_owned(),
            stderr: String::from_utf8(output.stderr)?
                .trim_end_matches(['\r', '\n'])
                .to_owned(),
        })
    }

    async fn run_raw(
        &self,
        cwd: Option<&Path>,
        args: &[OsString],
        environment: &[(OsString, OsString)],
        input: Option<&[u8]>,
    ) -> anyhow::Result<RawOutput> {
        let mut command = self.command(cwd, args, environment);
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn().context("failed to start system Git")?;
        let stdin = child.stdin.take();
        // Commands such as cat-file --batch can fill stdout before consuming
        // all of stdin. Feed and drain concurrently so neither pipe can block
        // the other. Dropping stdin at the end also delivers EOF to Git.
        let feed = async move {
            if let Some(input) = input {
                stdin
                    .context("Git stdin was not piped")?
                    .write_all(input)
                    .await
                    .context("failed to write Git stdin")?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let (written, output) = tokio::join!(feed, child.wait_with_output());
        let output = output.context("failed to wait for system Git")?;
        if output.status.success() {
            written?;
        }
        Ok(RawOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            success: output.status.success(),
            code: output.status.code(),
        })
    }

    fn command(
        &self,
        cwd: Option<&Path>,
        args: &[OsString],
        environment: &[(OsString, OsString)],
    ) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(args);
        for variable in [
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_QUARANTINE_PATH",
            "GIT_WORK_TREE",
            "GIT_CONFIG_COUNT",
        ] {
            command.env_remove(variable);
        }
        command.env("GIT_TERMINAL_PROMPT", "0");
        command.env("GCM_INTERACTIVE", "Never");
        command.env("LC_ALL", "C");
        command.envs(environment.iter().cloned());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command.kill_on_drop(true);
        command
    }

    fn require_success(&self, output: &RawOutput) -> anyhow::Result<()> {
        if output.success {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git command failed (exit {}): {}",
            output
                .code
                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
            stderr.trim()
        )
    }

    fn repo_args(repository: &ManagedRepository, arguments: &[&str]) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("--git-dir"),
            repository.git_dir.as_os_str().to_owned(),
        ];
        args.extend(arguments.iter().map(OsString::from));
        args
    }

    pub fn canonicalize_remote(&self, locator: &str) -> anyhow::Result<RemoteIdentity> {
        canonicalize_remote(locator)
    }

    pub async fn canonicalize_local(&self, path: &Path) -> anyhow::Result<LocalIdentity> {
        let worktree = fs::canonicalize(path)
            .with_context(|| format!("cannot canonicalize {}", path.display()))?;
        let args = vec![
            OsString::from("rev-parse"),
            OsString::from("--path-format=absolute"),
            OsString::from("--git-common-dir"),
        ];
        let output = self.run_raw(Some(&worktree), &args, &[], None).await?;
        self.require_success(&output)?;
        let common = bytes_to_path(trim_ascii_newline(&output.stdout));
        let common = if common.is_absolute() {
            common
        } else {
            worktree.join(common)
        };
        let common_git_dir = fs::canonicalize(&common)
            .with_context(|| format!("cannot canonicalize Git dir {}", common.display()))?;
        let fallback_url = Url::from_file_path(&common_git_dir)
            .map_err(|_| anyhow!("cannot express local Git identity as a file URL"))?
            .to_string();
        let fallback = canonicalize_remote(&fallback_url)?;

        let origin_args = vec![
            OsString::from("config"),
            OsString::from("--get"),
            OsString::from("remote.origin.url"),
        ];
        let origin_output = self
            .run_raw(Some(&worktree), &origin_args, &[], None)
            .await?;
        let origin = if origin_output.success {
            let configured = std::str::from_utf8(trim_ascii_newline(&origin_output.stdout))?;
            Some(canonicalize_local_origin(configured, &worktree)?)
        } else if origin_output.code == Some(1) && origin_output.stdout.is_empty() {
            None
        } else {
            self.require_success(&origin_output)?;
            unreachable!("require_success returns for a successful Git command")
        };
        let origin_configured = origin.is_some();
        let remote = origin.unwrap_or(fallback);
        Ok(LocalIdentity {
            worktree,
            common_git_dir,
            canonical: remote.canonical.clone(),
            remote,
            origin_configured,
        })
    }

    /// Read the transport URL persisted in a daemon-owned bare repository.
    /// This preserves the exact fetch endpoint across registered opens even
    /// when the canonical identity intentionally omits a `.git` suffix.
    pub async fn managed_origin(
        &self,
        repository: &ManagedRepository,
    ) -> anyhow::Result<Option<RemoteIdentity>> {
        let args = Self::repo_args(repository, &["config", "--get", "remote.origin.url"]);
        let output = self.run_raw(None, &args, &[], None).await?;
        if output.success {
            let configured = std::str::from_utf8(trim_ascii_newline(&output.stdout))?;
            return canonicalize_remote(configured).map(Some);
        }
        if output.code == Some(1) && output.stdout.is_empty() {
            return Ok(None);
        }
        self.require_success(&output)?;
        unreachable!("require_success returns for a successful Git command")
    }

    pub async fn canonicalize_repository(
        &self,
        locator: &str,
    ) -> anyhow::Result<RepositoryIdentity> {
        let candidate = Path::new(locator);
        if candidate.exists() || candidate.is_absolute() || locator.starts_with('.') {
            Ok(RepositoryIdentity::Local(
                self.canonicalize_local(candidate).await?,
            ))
        } else {
            Ok(RepositoryIdentity::Remote(
                self.canonicalize_remote(locator)?,
            ))
        }
    }

    pub async fn create_managed_bare(
        &self,
        destination: &Path,
        origin: Option<&RemoteIdentity>,
    ) -> anyhow::Result<ManagedRepository> {
        ensure!(!destination.exists(), "managed repository already exists");
        let parent = destination
            .parent()
            .context("managed repository needs a parent directory")?;
        fs::create_dir_all(parent)?;
        let staging = tempfile::Builder::new()
            .prefix(".shade-git-")
            .tempdir_in(parent)?;
        let staged_repository = ManagedRepository::new(staging.path());
        self.initialize_bare(&staged_repository, origin, None)
            .await?;
        crate::faults::hit(crate::faults::Point::RepositoryStaged);
        fs::rename(staging.path(), destination).with_context(|| {
            format!(
                "cannot publish managed repository at {}",
                destination.display()
            )
        })?;
        crate::faults::hit(crate::faults::Point::RepositoryPromoted);
        let git_dir = fs::canonicalize(destination)?;
        self.assert_no_alternates(&git_dir)?;
        Ok(ManagedRepository::new(git_dir))
    }

    pub async fn create_managed_bare_with_object_format(
        &self,
        destination: &Path,
        origin: Option<&RemoteIdentity>,
        object_format: &str,
    ) -> anyhow::Result<ManagedRepository> {
        ensure!(
            matches!(object_format, "sha1" | "sha256"),
            "unsupported Git object format"
        );
        ensure!(!destination.exists(), "managed repository already exists");
        let parent = destination
            .parent()
            .context("managed repository needs a parent directory")?;
        fs::create_dir_all(parent)?;
        let staging = tempfile::Builder::new()
            .prefix(".shade-git-")
            .tempdir_in(parent)?;
        let staged_repository = ManagedRepository::new(staging.path());
        self.initialize_bare(&staged_repository, origin, Some(object_format))
            .await?;
        crate::faults::hit(crate::faults::Point::RepositoryStaged);
        fs::rename(staging.path(), destination)?;
        crate::faults::hit(crate::faults::Point::RepositoryPromoted);
        Ok(ManagedRepository::new(fs::canonicalize(destination)?))
    }

    pub async fn import_managed_bare(
        &self,
        source: &Path,
        destination: &Path,
        branch: &str,
        managed_origin: &RemoteIdentity,
    ) -> anyhow::Result<(ManagedRepository, BaseRevision)> {
        self.validate_branch(branch).await?;
        ensure!(!destination.exists(), "managed repository already exists");
        let local = self.canonicalize_local(source).await?;
        let source_url = Url::from_file_path(&local.common_git_dir)
            .map_err(|_| anyhow!("cannot express import source as a file URL"))?;
        let remote = canonicalize_remote(source_url.as_str())?;
        let parent = destination
            .parent()
            .context("managed repository needs a parent directory")?;
        fs::create_dir_all(parent)?;
        let staging = tempfile::Builder::new()
            .prefix(".shade-git-import-")
            .tempdir_in(parent)?;
        let staged_repository = ManagedRepository::new(staging.path());
        let format_args = vec![
            OsString::from("rev-parse"),
            OsString::from("--show-object-format"),
        ];
        let format_output = self.run_raw(Some(source), &format_args, &[], None).await?;
        self.require_success(&format_output)?;
        let object_format = std::str::from_utf8(trim_ascii_newline(&format_output.stdout))?;
        ensure!(
            matches!(object_format, "sha1" | "sha256"),
            "unsupported Git object format"
        );
        // Seed objects from the local checkout, but persist the selected
        // managed origin separately. When the checkout has an origin this is
        // the real upstream, not the potentially stale clone being imported.
        self.initialize_bare(
            &staged_repository,
            Some(managed_origin),
            Some(object_format),
        )
        .await?;
        let base = self.fetch_head(&staged_repository, &remote, branch).await?;
        self.assert_no_alternates(staging.path())?;
        crate::faults::hit(crate::faults::Point::RepositoryStaged);
        fs::rename(staging.path(), destination)?;
        crate::faults::hit(crate::faults::Point::RepositoryPromoted);
        let repository = ManagedRepository::new(fs::canonicalize(destination)?);
        Ok((repository, base))
    }

    pub async fn configure_content_filter(
        &self,
        repository: &ManagedRepository,
    ) -> anyhow::Result<()> {
        let mut attributes = Vec::new();
        if let Some((executable, spool_root)) = &self.content_filter {
            fs::create_dir_all(spool_root)?;
            fs::set_permissions(spool_root, fs::Permissions::from_mode(0o700))?;
            let quote = |path: &Path| -> anyhow::Result<String> {
                let path = path.to_str().context("filter path is not UTF-8")?;
                Ok(format!("'{}'", path.replace('\'', "'\"'\"'")))
            };
            let process = format!("{} __git-filter {}", quote(executable)?, quote(spool_root)?);
            for (key, value) in [
                ("filter.shade-content.process", process.as_str()),
                ("filter.shade-content.required", "true"),
            ] {
                let output = self
                    .run_raw(
                        None,
                        &Self::repo_args(repository, &["config", "--local", key, value]),
                        &[],
                        None,
                    )
                    .await?;
                self.require_success(&output)?;
            }
            attributes.extend_from_slice(b"* filter=shade-content\n");
        }
        attributes.extend_from_slice(b".env* filter=shade-secret\n**/.env* filter=shade-secret\n");
        let path = repository.git_dir.join("info/attributes");
        fs::write(&path, attributes)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    async fn initialize_bare(
        &self,
        repository: &ManagedRepository,
        origin: Option<&RemoteIdentity>,
        object_format: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut args = vec![OsString::from("init"), OsString::from("--bare")];
        if let Some(object_format) = object_format {
            args.push(format!("--object-format={object_format}").into());
        }
        args.push(repository.git_dir.as_os_str().to_owned());
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;

        for (key, value) in [
            ("core.logAllRefUpdates", "true"),
            ("transfer.fsckObjects", "true"),
            ("fetch.writeCommitGraph", "false"),
            ("filter.shade-secret.clean", "false"),
            ("filter.shade-secret.smudge", "cat"),
            ("filter.shade-secret.required", "true"),
        ] {
            let args = Self::repo_args(repository, &["config", "--local", key, value]);
            let output = self.run_raw(None, &args, &[], None).await?;
            self.require_success(&output)?;
        }
        self.configure_content_filter(repository).await?;
        if let Some(origin) = origin {
            let mut args = Self::repo_args(repository, &["config", "--local", "remote.origin.url"]);
            args.push(origin.fetch_url.clone().into());
            let output = self.run_raw(None, &args, &[], None).await?;
            self.require_success(&output)?;
            let args = Self::repo_args(
                repository,
                &["config", "--local", "remote.origin.tagOpt", "--no-tags"],
            );
            let output = self.run_raw(None, &args, &[], None).await?;
            self.require_success(&output)?;
        }
        self.assert_no_alternates(&repository.git_dir)
    }

    fn assert_no_alternates(&self, git_dir: &Path) -> anyhow::Result<()> {
        let alternates = git_dir.join("objects/info/alternates");
        ensure!(
            !alternates.exists(),
            "managed repositories may not use object alternates"
        );
        Ok(())
    }

    pub async fn fetch_head(
        &self,
        repository: &ManagedRepository,
        remote: &RemoteIdentity,
        branch: &str,
    ) -> anyhow::Result<BaseRevision> {
        self.validate_branch(branch).await?;
        let source_ref = format!("refs/heads/{branch}");
        let destination_ref = format!("refs/remotes/{ORIGIN}/{branch}");
        ensure!(!destination_ref.starts_with(PRIVATE_REFS));

        // A fetch may transfer objects before any checkout policy can inspect
        // them. Receive into a private, daemon-owned bare repository first so
        // a rejected history cannot contaminate the managed object database.
        let parent = repository
            .git_dir
            .parent()
            .context("managed repository needs a parent for fetch quarantine")?;
        let quarantine = tempfile::Builder::new()
            .prefix(".shade-git-quarantine-")
            .tempdir_in(parent)?;
        fs::set_permissions(quarantine.path(), fs::Permissions::from_mode(0o700))?;
        let quarantine_metadata = fs::metadata(quarantine.path())?;
        ensure!(
            quarantine_metadata.uid() == unsafe { libc::geteuid() },
            "fetch quarantine is not owned by the daemon user"
        );
        let quarantine_repository = ManagedRepository::new(quarantine.path());
        let object_format = self.object_format(repository).await?;
        self.initialize_bare(&quarantine_repository, None, Some(&object_format))
            .await?;
        if let Some(counters) = &self.counters {
            // Count only ingress from the source. The second explicit fetch is
            // a local promotion from the already validated quarantine.
            counters.source_fetches.fetch_add(1, Ordering::SeqCst);
        }
        self.fetch_explicit(
            &quarantine_repository,
            &remote.fetch_url,
            &source_ref,
            QUARANTINE_REF,
        )
        .await?;
        let quarantined = self
            .resolve_existing(
                &quarantine_repository,
                QUARANTINE_REF,
                Some(source_ref.clone()),
            )
            .await?;
        self.validate_reachable_checkout_policy(&quarantine_repository, &quarantined.commit)
            .await?;
        crate::faults::hit(crate::faults::Point::FetchQuarantined);

        let quarantine_url = Url::from_file_path(quarantine_repository.git_dir())
            .map_err(|_| anyhow!("cannot express fetch quarantine as a file URL"))?;
        self.fetch_explicit(
            repository,
            quarantine_url.as_str(),
            QUARANTINE_REF,
            &destination_ref,
        )
        .await?;
        crate::faults::hit(crate::faults::Point::FetchPromoted);
        self.resolve_existing(repository, &destination_ref, Some(source_ref))
            .await
    }

    async fn fetch_explicit(
        &self,
        repository: &ManagedRepository,
        fetch_url: &str,
        source_ref: &str,
        destination_ref: &str,
    ) -> anyhow::Result<()> {
        ensure!(
            source_ref.starts_with("refs/"),
            "fetch source ref is not qualified"
        );
        ensure!(
            destination_ref.starts_with("refs/"),
            "fetch destination ref is not qualified"
        );
        let refspec = format!("+{source_ref}:{destination_ref}");
        let mut args = Self::repo_args(
            repository,
            &[
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                "--no-write-fetch-head",
                "--no-auto-maintenance",
            ],
        );
        // Passing both the URL and the one fully-qualified refspec means
        // remote.*.fetch, mirror settings and private refs cannot participate.
        args.push(fetch_url.into());
        args.push(refspec.into());
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;
        self.assert_no_alternates(&repository.git_dir)?;
        Ok(())
    }

    async fn object_format(&self, repository: &ManagedRepository) -> anyhow::Result<String> {
        let args = Self::repo_args(repository, &["rev-parse", "--show-object-format"]);
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;
        let format = std::str::from_utf8(trim_ascii_newline(&output.stdout))?.to_owned();
        ensure!(
            matches!(format.as_str(), "sha1" | "sha256"),
            "unsupported Git object format"
        );
        Ok(format)
    }

    pub async fn resolve_base(
        &self,
        repository: &ManagedRepository,
        remote: Option<&RemoteIdentity>,
        specification: BaseSpec,
    ) -> anyhow::Result<BaseRevision> {
        match specification {
            BaseSpec::RemoteHead => {
                let remote = remote.context("remote HEAD requires a remote")?;
                let branch = self.remote_head_branch(remote).await?;
                self.fetch_head(repository, remote, &branch).await
            }
            BaseSpec::OriginBranch(branch) => {
                let remote = remote.context("origin/branch requires a remote")?;
                self.fetch_head(repository, remote, &branch).await
            }
            BaseSpec::ExistingOid(oid) => {
                self.resolve_existing(repository, oid.as_str(), None).await
            }
        }
    }

    async fn remote_head_branch(&self, remote: &RemoteIdentity) -> anyhow::Result<String> {
        let args = vec![
            OsString::from("ls-remote"),
            OsString::from("--symref"),
            OsString::from("--exit-code"),
            OsString::from(&remote.fetch_url),
            OsString::from("HEAD"),
        ];
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;
        let text = String::from_utf8(output.stdout)?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("ref: refs/heads/")
                && let Some((branch, "HEAD")) = rest.split_once('\t')
            {
                self.validate_branch(branch).await?;
                return Ok(branch.to_owned());
            }
        }
        bail!("remote HEAD is not a symbolic refs/heads/* reference")
    }

    async fn resolve_existing(
        &self,
        repository: &ManagedRepository,
        revision: &str,
        source_ref: Option<String>,
    ) -> anyhow::Result<BaseRevision> {
        let commit_expression = format!("{revision}^{{commit}}");
        let mut args = Self::repo_args(repository, &["rev-parse", "--verify", "--end-of-options"]);
        args.push(commit_expression.into());
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;
        let commit = parse_oid(&output.stdout)?;

        let tree_expression = format!("{}^{{tree}}", commit.as_str());
        let mut args = Self::repo_args(repository, &["rev-parse", "--verify", "--end-of-options"]);
        args.push(tree_expression.into());
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;
        let tree = parse_oid(&output.stdout)?;
        Ok(BaseRevision {
            commit,
            tree,
            source_ref,
        })
    }

    async fn validate_branch(&self, branch: &str) -> anyhow::Result<()> {
        ensure!(!branch.starts_with('-'), "branch cannot start with '-'");
        ensure!(
            !branch.starts_with(PRIVATE_REFS),
            "private ref is not a branch"
        );
        let args = vec![
            OsString::from("check-ref-format"),
            OsString::from("--branch"),
            OsString::from(branch),
        ];
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)
    }

    async fn validate_refname(&self, reference: &str) -> anyhow::Result<()> {
        ensure!(reference.starts_with("refs/"), "expected a full ref name");
        let args = vec![
            OsString::from("check-ref-format"),
            OsString::from(reference),
        ];
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)
    }
}

fn canonicalize_local_origin(locator: &str, worktree: &Path) -> anyhow::Result<RemoteIdentity> {
    let locator = locator.trim();
    ensure!(!locator.is_empty(), "remote.origin.url cannot be empty");
    let is_scp_style = !locator.contains("://")
        && locator
            .split_once(':')
            .is_some_and(|(authority, path)| !authority.contains('/') && !path.is_empty());
    let candidate = Path::new(locator);
    if !locator.contains("://") && !is_scp_style && !candidate.is_absolute() {
        let resolved = worktree.join(candidate);
        let resolved = resolved
            .to_str()
            .context("relative remote.origin.url is not valid UTF-8")?;
        return canonicalize_remote(resolved);
    }
    canonicalize_remote(locator)
}

fn canonicalize_remote(locator: &str) -> anyhow::Result<RemoteIdentity> {
    let locator = locator.trim();
    ensure!(!locator.is_empty(), "remote locator cannot be empty");

    if !locator.contains("://") {
        if let Some((authority, path)) = locator.split_once(':')
            && !authority.contains('/')
            && !path.is_empty()
        {
            let (user, host) = authority
                .rsplit_once('@')
                .map_or((None, authority), |(user, host)| (Some(user), host));
            ensure!(!host.is_empty(), "scp-style remote has no host");
            let host = host.to_ascii_lowercase();
            let transport_path = path.trim_matches('/');
            let identity_path = normalize_remote_path(path);
            let user = user.filter(|value| !value.is_empty());
            let canonical_authority = user.map(|user| format!("{user}@{host}")).unwrap_or(host);
            return Ok(RemoteIdentity {
                canonical: format!("ssh+scp://{canonical_authority}/{identity_path}"),
                // Keep scp syntax: unlike ssh://, its path is relative to
                // the remote user's home directory.
                fetch_url: format!("{authority}:{transport_path}"),
            });
        }
        let canonical_path = fs::canonicalize(locator)
            .with_context(|| format!("remote path does not exist: {locator}"))?;
        let url = Url::from_file_path(canonical_path)
            .map_err(|_| anyhow!("cannot express remote path as a file URL"))?;
        let fetch_url = url.to_string();
        let mut identity_url = url;
        let normalized_path = normalize_remote_path(identity_url.path());
        identity_url.set_path(&format!("/{normalized_path}"));
        return Ok(RemoteIdentity {
            canonical: identity_url.to_string().trim_end_matches('/').to_owned(),
            fetch_url,
        });
    }

    let mut url = Url::parse(locator).context("invalid remote URL")?;
    ensure!(
        matches!(url.scheme(), "https" | "http" | "ssh" | "git" | "file"),
        "unsupported Git transport scheme"
    );
    url.set_fragment(None);
    url.set_query(None);
    let _ = url.set_password(None);
    if matches!(url.scheme(), "http" | "https" | "git" | "file") {
        let _ = url.set_username("");
    }
    if let Some(host) = url.host_str().map(str::to_ascii_lowercase) {
        url.set_host(Some(&host))?;
    }
    let default_port = match url.scheme() {
        "http" => Some(80),
        "https" => Some(443),
        "ssh" => Some(22),
        "git" => Some(9418),
        _ => None,
    };
    if url.port() == default_port {
        let _ = url.set_port(None);
    }
    let fetch_url = url.to_string().trim_end_matches('/').to_owned();
    let mut identity_url = url;
    let normalized_path = normalize_remote_path(identity_url.path());
    identity_url.set_path(&format!("/{normalized_path}"));
    let canonical = identity_url.to_string().trim_end_matches('/').to_owned();
    Ok(RemoteIdentity {
        canonical,
        fetch_url,
    })
}

fn normalize_remote_path(path: &str) -> String {
    path.trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| path.trim_matches('/'))
        .to_owned()
}

fn trim_ascii_newline(mut bytes: &[u8]) -> &[u8] {
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn parse_oid(bytes: &[u8]) -> anyhow::Result<Oid> {
    Oid::new(String::from_utf8(trim_ascii_newline(bytes).to_vec())?)
}

fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[derive(Debug)]
struct TreeEntry {
    mode: Vec<u8>,
    object: Oid,
    path: PathBuf,
    size: Option<u64>,
}

fn validate_entry_path(entry: &TreeEntry) -> anyhow::Result<()> {
    let basename = entry.path.file_name().unwrap_or_default().as_bytes();
    ensure!(
        entry.mode.as_slice() != b"160000",
        "UNSUPPORTED_SUBMODULE: {} is a gitlink",
        entry.path.display()
    );
    ensure!(
        basename != b".gitmodules",
        "UNSUPPORTED_SUBMODULE: .gitmodules is present"
    );
    ensure!(
        !basename.starts_with(b".env"),
        "TRACKED_SECRET_FILE: {}",
        entry.path.display()
    );
    ensure!(
        !is_dependency_output_path(&entry.path),
        "TRACKED_DEPENDENCY_OUTPUT: {}",
        entry.path.display()
    );
    ensure!(basename != b".lfsconfig", "UNSUPPORTED_GIT_LFS");
    Ok(())
}

fn take_batch_blob<'a>(
    remaining: &mut &'a [u8],
    expected: &Oid,
    expected_size: Option<u64>,
) -> anyhow::Result<&'a [u8]> {
    let newline = remaining
        .iter()
        .position(|byte| *byte == b'\n')
        .context("cat-file batch omitted an object header")?;
    let header = &remaining[..newline];
    let fields = header.split(|byte| *byte == b' ').collect::<Vec<_>>();
    ensure!(
        fields.len() == 3 && fields[0] == expected.as_str().as_bytes() && fields[1] == b"blob",
        "cat-file batch returned a missing, mismatched or non-blob object"
    );
    let size = std::str::from_utf8(fields[2])?.parse::<usize>()?;
    ensure!(
        expected_size == Some(size as u64),
        "cat-file batch object size differs from tree inventory"
    );
    let payload = &remaining[newline + 1..];
    ensure!(
        payload.get(size) == Some(&b'\n'),
        "cat-file batch returned a truncated object"
    );
    let bytes = &payload[..size];
    *remaining = &payload[size + 1..];
    Ok(bytes)
}

#[derive(Debug)]
struct ConflictStage {
    mode: Vec<u8>,
    object: Oid,
    stage: u8,
    path: Vec<u8>,
}

#[derive(Debug)]
struct MergeTreeResult {
    tree: Oid,
    clean: bool,
    conflicts: Vec<ConflictStage>,
}

impl MergeTreeResult {
    fn conflict_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<_> = self
            .conflicts
            .iter()
            .map(|conflict| bytes_to_path(&conflict.path))
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }
}

impl GitStore {
    /// Validate every tree reachable from a fetched commit before any of its
    /// objects or refs are promoted out of quarantine. Checking only the tip
    /// would allow a deleted `.env*` blob to enter through repository history.
    async fn validate_reachable_checkout_policy(
        &self,
        repository: &ManagedRepository,
        commit: &Oid,
    ) -> anyhow::Result<()> {
        let mut args = Self::repo_args(
            repository,
            &["rev-list", "--format=%T", "--no-commit-header"],
        );
        args.push(commit.as_str().into());
        args.push("--".into());
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;

        let mut trees = BTreeSet::new();
        for line in output.stdout.split(|byte| *byte == b'\n') {
            let line = trim_ascii_newline(line);
            if line.is_empty() {
                continue;
            }
            trees.insert(parse_oid(line)?);
        }
        ensure!(!trees.is_empty(), "fetched history has no reachable trees");
        for tree in trees {
            self.validate_checkout_policy(repository, &tree).await?;
        }
        Ok(())
    }

    /// Reject repository features which would execute external programs or
    /// replace bytes while Shade materializes an index. Tracked dotenv files
    /// are rejected here as well: secrets must never reach a managed object
    /// database in the first place.
    pub async fn validate_checkout_policy(
        &self,
        repository: &ManagedRepository,
        tree: &Oid,
    ) -> anyhow::Result<()> {
        let entries = self.tree_entries(repository, tree).await?;
        ensure!(!entries.is_empty() || self.object_exists(repository, tree).await?);

        let mut attribute_input = Vec::new();
        let mut inspect_blobs = Vec::new();
        for entry in &entries {
            validate_entry_path(entry)?;
            attribute_input.extend_from_slice(entry.path.as_os_str().as_bytes());
            attribute_input.push(0);

            if entry.mode.as_slice() != b"160000" {
                inspect_blobs.push(entry);
            }
        }

        self.validate_blobs(repository, &inspect_blobs).await?;

        if !attribute_input.is_empty() {
            let source = format!("--source={}", tree.as_str());
            let mut args = Self::repo_args(repository, &["check-attr", "-z", "--stdin", "--all"]);
            args.push(source.into());
            let output = self
                .run_raw(None, &args, &[], Some(&attribute_input))
                .await?;
            self.require_success(&output)?;
            reject_filter_attributes(&output.stdout)?;
        }
        Ok(())
    }

    async fn validate_blobs(
        &self,
        repository: &ManagedRepository,
        entries: &[&TreeEntry],
    ) -> anyhow::Result<()> {
        // Batch small objects; stream large ones without retaining their
        // contents in memory. Every byte is checked before promotion/retention.
        let mut batch = Vec::new();
        let mut batch_bytes = 0;
        for &entry in entries {
            let size = entry.size.unwrap_or(0);
            if size > 1024 * 1024 {
                self.validate_large_blob(repository, entry).await?;
                continue;
            }
            if batch.len() == 1024 || batch_bytes + size > 8 * 1024 * 1024 {
                self.validate_blob_batch(repository, &batch).await?;
                batch.clear();
                batch_bytes = 0;
            }
            batch.push(entry);
            batch_bytes += size;
        }
        if !batch.is_empty() {
            self.validate_blob_batch(repository, &batch).await?;
        }

        Ok(())
    }

    async fn tree_entries(
        &self,
        repository: &ManagedRepository,
        tree: &Oid,
    ) -> anyhow::Result<Vec<TreeEntry>> {
        let mut args = Self::repo_args(repository, &["ls-tree", "-r", "-l", "-z"]);
        args.push(tree.as_str().into());
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .map(parse_tree_entry)
            .collect()
    }

    async fn validate_blob_batch(
        &self,
        repository: &ManagedRepository,
        entries: &[&TreeEntry],
    ) -> anyhow::Result<()> {
        let mut input = Vec::new();
        for entry in entries {
            input.extend_from_slice(entry.object.as_str().as_bytes());
            input.push(b'\n');
        }
        let args = Self::repo_args(repository, &["cat-file", "--batch", "--buffer"]);
        let output = self.run_raw(None, &args, &[], Some(&input)).await?;
        self.require_success(&output)?;
        let mut remaining = output.stdout.as_slice();
        for entry in entries {
            let bytes = take_batch_blob(&mut remaining, &entry.object, entry.size)?;
            ensure!(
                !crate::secret_policy::contains_secret(bytes),
                "TRACKED_SECRET_FILE: {}",
                entry.path.display()
            );
            if bytes.starts_with(LFS_POINTER_HEADER) {
                bail!("UNSUPPORTED_GIT_LFS: {}", entry.path.display());
            }
            if entry.path.file_name() == Some(OsStr::new(".gitattributes"))
                && attributes_declare_filter(bytes)
            {
                bail!(
                    "UNSUPPORTED_GIT_FILTER: {} declares a filter",
                    entry.path.display()
                );
            }
        }
        ensure!(
            remaining.is_empty(),
            "unexpected trailing cat-file batch output"
        );
        Ok(())
    }

    async fn validate_large_blob(
        &self,
        repository: &ManagedRepository,
        entry: &TreeEntry,
    ) -> anyhow::Result<()> {
        let args = Self::repo_args(repository, &["cat-file", "blob", entry.object.as_str()]);
        let mut child = self
            .command(None, &args, &[])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdout = child.stdout.take().context("Git stdout missing")?;
        let mut stderr = child.stderr.take().context("Git stderr missing")?;
        let scan = async {
            let mut scanner = crate::secret_policy::SecretScanner::default();
            let mut buffer = vec![0u8; 65536];
            let mut line = Vec::new();
            let attributes = entry.path.file_name() == Some(OsStr::new(".gitattributes"));
            let mut unsafe_attributes = false;
            let mut total = 0u64;
            loop {
                let size = stdout.read(&mut buffer).await?;
                if size == 0 {
                    break;
                }
                total += size as u64;
                scanner.feed(&buffer[..size]);
                if attributes && !unsafe_attributes {
                    for byte in &buffer[..size] {
                        line.push(*byte);
                        if *byte == b'\n' {
                            unsafe_attributes |= attributes_declare_filter(&line);
                            line.clear();
                        } else if line.len() > 65536 {
                            unsafe_attributes = true;
                            break;
                        }
                    }
                }
            }
            unsafe_attributes |= attributes && attributes_declare_filter(&line);
            Ok::<_, anyhow::Error>((scanner.detected(), unsafe_attributes, total))
        };
        let errors = async {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await?;
            Ok::<_, anyhow::Error>(bytes)
        };
        let ((secret, attributes, size), stderr, status) = tokio::try_join!(scan, errors, async {
            Ok::<_, anyhow::Error>(child.wait().await?)
        })?;
        self.require_success(&RawOutput {
            stdout: Vec::new(),
            stderr,
            success: status.success(),
            code: status.code(),
        })?;
        ensure!(
            entry.size.is_none_or(|expected| expected == size),
            "Git blob size changed"
        );
        ensure!(!secret, "TRACKED_SECRET_FILE: {}", entry.path.display());
        ensure!(
            !attributes,
            "UNSUPPORTED_GIT_FILTER: {}",
            entry.path.display()
        );
        Ok(())
    }

    async fn object_exists(
        &self,
        repository: &ManagedRepository,
        object: &Oid,
    ) -> anyhow::Result<bool> {
        let mut args = Self::repo_args(repository, &["cat-file", "-e"]);
        args.push(object.as_str().into());
        let output = self.run_raw(None, &args, &[], None).await?;
        Ok(output.success)
    }

    pub async fn prepare_base(
        &self,
        repository: &ManagedRepository,
        base: &BaseRevision,
        destination: &Path,
    ) -> anyhow::Result<()> {
        if let Some(counters) = &self.counters {
            counters
                .base_materializations
                .fetch_add(1, Ordering::SeqCst);
        }
        self.materialize_tree(repository, &base.tree, destination)
            .await
    }

    /// Update an exact materialized base from `previous` to `next` using
    /// Git's index/worktree transition machinery. The caller owns staging and
    /// atomic publication: this method only mutates `root`, which must be a
    /// private clone of the immutable previous base.
    pub async fn update_materialized_base(
        &self,
        repository: &ManagedRepository,
        previous: &BaseRevision,
        next: &BaseRevision,
        root: &Path,
    ) -> anyhow::Result<()> {
        if let Some(counters) = &self.counters {
            counters
                .base_materializations
                .fetch_add(1, Ordering::SeqCst);
        }
        ensure!(root.is_dir(), "incremental base root is not a directory");
        ensure!(
            matches!(
                fs::symlink_metadata(root.join(".git")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
            "incremental base root contains Git metadata"
        );
        self.validate_checkout_policy(repository, &previous.tree)
            .await?;
        self.validate_checkout_policy(repository, &next.tree)
            .await?;
        self.verify_materialized_tree(repository, &previous.tree, root)
            .await
            .context("incremental base source clone does not match its Git tree")?;

        let index_directory = tempfile::Builder::new()
            .prefix(".shade-git-index-")
            .tempdir_in(
                repository
                    .git_dir()
                    .parent()
                    .context("repository needs a parent")?,
            )?;
        let index_path = index_directory.path().join("index");
        let environment = vec![
            (
                OsString::from("GIT_INDEX_FILE"),
                index_path.as_os_str().to_owned(),
            ),
            (OsString::from("GIT_WORK_TREE"), root.as_os_str().to_owned()),
        ];

        let mut read_previous = Self::repo_args(repository, &["read-tree"]);
        crate::faults::hit(crate::faults::Point::IncrementalBaseIndexStaged);
        read_previous.push(previous.tree.as_str().into());
        let output = self
            .run_raw(None, &read_previous, &environment, None)
            .await?;
        self.require_success(&output)?;

        crate::faults::hit(crate::faults::Point::IncrementalBaseIndexWritten);

        // Populate the temporary index stat cache from the cloned A tree.
        // `read-tree --reset -u` can then apply A→B directly: Git performs
        // additions, removals, content replacements and mode/type changes.
        let refresh = Self::repo_args(repository, &["update-index", "--really-refresh"]);
        let output = self.run_raw(None, &refresh, &environment, None).await?;
        self.require_success(&output)?;
        crate::faults::hit(crate::faults::Point::IncrementalBaseIndexRefreshed);

        let mut transition = Self::repo_args(repository, &["read-tree", "--reset", "-u"]);
        transition.push(next.tree.as_str().into());
        let output = self.run_raw(None, &transition, &environment, None).await?;
        self.require_success(&output)?;
        crate::faults::hit(crate::faults::Point::IncrementalBaseTreeUpdated);

        self.verify_materialized_tree(repository, &next.tree, root)
            .await
            .context("incremental base result does not match its Git tree")?;
        crate::faults::hit(crate::faults::Point::IncrementalBaseTreeVerified);
        Ok(())
    }

    /// Verify both tracked content and absence of leftover paths without
    /// attaching worktree metadata to an immutable base.
    pub async fn verify_materialized_tree(
        &self,
        repository: &ManagedRepository,
        tree: &Oid,
        root: &Path,
    ) -> anyhow::Result<()> {
        ensure!(root.is_dir(), "materialized tree root is not a directory");
        ensure!(
            matches!(
                fs::symlink_metadata(root.join(".git")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
            "materialized tree root contains Git metadata"
        );
        let index_directory = tempfile::Builder::new()
            .prefix(".shade-git-index-")
            .tempdir_in(
                repository
                    .git_dir()
                    .parent()
                    .context("repository needs a parent")?,
            )?;
        let index_path = index_directory.path().join("index");
        let environment = vec![
            (
                OsString::from("GIT_INDEX_FILE"),
                index_path.as_os_str().to_owned(),
            ),
            (OsString::from("GIT_WORK_TREE"), root.as_os_str().to_owned()),
        ];
        let mut read_tree = Self::repo_args(repository, &["read-tree"]);
        read_tree.push(tree.as_str().into());
        let output = self.run_raw(None, &read_tree, &environment, None).await?;
        self.require_success(&output)?;

        let refresh = Self::repo_args(repository, &["update-index", "--really-refresh"]);
        let output = self.run_raw(None, &refresh, &environment, None).await?;
        self.require_success(&output)?;

        let differences = Self::repo_args(
            repository,
            &["diff-files", "--quiet", "--ignore-submodules=none", "--"],
        );
        let output = self.run_raw(None, &differences, &environment, None).await?;
        ensure!(
            output.success,
            "materialized tracked content differs from tree {}",
            tree
        );

        // Deliberately omit exclude-standard: ignored leftovers are still not
        // part of an immutable base and must fail verification.
        let untracked = Self::repo_args(repository, &["ls-files", "--others", "-z", "--"]);
        let output = self.run_raw(None, &untracked, &environment, None).await?;
        self.require_success(&output)?;
        ensure!(
            output.stdout.is_empty(),
            "materialized tree contains paths absent from tree {}",
            tree
        );
        Ok(())
    }

    /// Materialize a tree through a disposable Git index. The destination is
    /// published with one rename, so a crash cannot expose a partial base.
    pub async fn materialize_tree(
        &self,
        repository: &ManagedRepository,
        tree: &Oid,
        destination: &Path,
    ) -> anyhow::Result<()> {
        self.validate_checkout_policy(repository, tree).await?;
        ensure!(!destination.exists(), "materialization destination exists");
        let parent = destination
            .parent()
            .context("materialization destination needs a parent")?;
        fs::create_dir_all(parent)?;
        let index_directory = tempfile::Builder::new()
            .prefix(".shade-git-index-")
            .tempdir_in(
                repository
                    .git_dir()
                    .parent()
                    .context("repository needs a parent")?,
            )?;
        let index_path = index_directory.path().join("index");
        let environment = vec![(
            OsString::from("GIT_INDEX_FILE"),
            index_path.as_os_str().to_owned(),
        )];
        let mut args = Self::repo_args(repository, &["read-tree"]);
        args.push(tree.as_str().into());
        let output = self.run_raw(None, &args, &environment, None).await?;
        self.require_success(&output)?;

        let staging = tempfile::Builder::new()
            .prefix(".shade-materialize-")
            .tempdir_in(parent)?;
        let mut checkout_environment = environment.clone();
        checkout_environment.push((
            OsString::from("GIT_WORK_TREE"),
            staging.path().as_os_str().to_owned(),
        ));
        let mut prefix = staging.path().as_os_str().as_bytes().to_vec();
        prefix.push(b'/');
        let mut args = Self::repo_args(repository, &["checkout-index", "--all", "--force"]);
        args.push(OsString::from_vec(
            [b"--prefix=".as_slice(), prefix.as_slice()].concat(),
        ));
        let output = self
            .run_raw(None, &args, &checkout_environment, None)
            .await?;
        self.require_success(&output)?;
        crate::faults::hit(crate::faults::Point::BaseStaged);
        fs::rename(staging.path(), destination).with_context(|| {
            format!(
                "cannot publish materialized tree at {}",
                destination.display()
            )
        })?;
        crate::faults::hit(crate::faults::Point::BasePromoted);
        Ok(())
    }

    /// Register a root which has already been materialized (normally by an
    /// APFS clone). Git itself creates the linked-worktree administration;
    /// only the tiny `.git` pointer is moved onto the pre-cloned root.
    pub async fn register_precloned_worktree(
        &self,
        repository: &ManagedRepository,
        root: &Path,
        base: &BaseRevision,
        lock_reason: &str,
    ) -> anyhow::Result<WorktreeHandle> {
        ensure!(root.is_dir(), "pre-cloned worktree root is not a directory");
        ensure!(
            !root.join(".git").exists(),
            "worktree already has .git metadata"
        );
        ensure!(
            !lock_reason.contains(['\r', '\n']),
            "worktree lock reason contains a newline"
        );
        let root = fs::canonicalize(root)?;
        let parent = root.parent().context("worktree root needs a parent")?;
        let registration_parent = tempfile::Builder::new()
            .prefix(".shade-register-")
            .tempdir_in(parent)?;
        let registration_root = registration_parent.path().join("worktree");
        let mut args = Self::repo_args(
            repository,
            &[
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
                "--lock",
                "--reason",
                lock_reason,
            ],
        );
        args.push(registration_root.as_os_str().to_owned());
        args.push(base.commit.as_str().into());
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;

        crate::faults::hit(crate::faults::Point::WorktreeRegistered);
        let temporary_git_file = registration_root.join(".git");
        let installed_git_file = root.join(".git");
        if let Err(error) = fs::rename(&temporary_git_file, &installed_git_file) {
            self.rollback_registration(repository, &registration_root, &root)
                .await;
            return Err(error).context("cannot install linked-worktree .git file");
        }
        crate::faults::hit(crate::faults::Point::WorktreePointerMoved);
        let _ = fs::remove_dir(&registration_root);

        let mut repair_args = Self::repo_args(repository, &["worktree", "repair"]);
        repair_args.push(root.as_os_str().to_owned());
        let repaired = self.run_raw(None, &repair_args, &[], None).await?;
        if !repaired.success {
            self.rollback_registration(repository, &registration_root, &root)
                .await;
            self.require_success(&repaired)?;
        }
        crate::faults::hit(crate::faults::Point::WorktreeRepaired);

        let args = vec![OsString::from("read-tree"), base.tree.as_str().into()];
        let read_tree = self.run_raw(Some(&root), &args, &[], None).await?;
        if !read_tree.success {
            self.rollback_registration(repository, &registration_root, &root)
                .await;
            self.require_success(&read_tree)?;
        }
        crate::faults::hit(crate::faults::Point::WorktreeIndexWritten);

        let args = vec![
            OsString::from("rev-parse"),
            OsString::from("--path-format=absolute"),
            OsString::from("--git-dir"),
        ];
        let output = self.run_raw(Some(&root), &args, &[], None).await?;
        self.require_success(&output)?;
        let admin_dir = fs::canonicalize(bytes_to_path(trim_ascii_newline(&output.stdout)))?;
        Ok(WorktreeHandle {
            root,
            admin_dir,
            head: base.commit.clone(),
        })
    }

    async fn rollback_registration(
        &self,
        repository: &ManagedRepository,
        registration_root: &Path,
        installed_root: &Path,
    ) {
        let installed_git = installed_root.join(".git");
        if installed_git.is_file() {
            let _ = fs::create_dir_all(registration_root);
            let _ = fs::rename(&installed_git, registration_root.join(".git"));
            let mut repair = Self::repo_args(repository, &["worktree", "repair"]);
            repair.push(registration_root.as_os_str().to_owned());
            let _ = self.run_raw(None, &repair, &[], None).await;
        }
        let mut unlock = Self::repo_args(repository, &["worktree", "unlock"]);
        unlock.push(registration_root.as_os_str().to_owned());
        let _ = self.run_raw(None, &unlock, &[], None).await;
        let mut remove = Self::repo_args(repository, &["worktree", "remove", "--force"]);
        remove.push(registration_root.as_os_str().to_owned());
        let _ = self.run_raw(None, &remove, &[], None).await;
    }

    /// Read the state needed by the latency-sensitive, read-only context
    /// query. This deliberately does not run the checkout policy inventory:
    /// it neither hashes content nor creates objects or refs. Every operation
    /// that can retain or publish workspace content must use `status` (or a
    /// stronger policy-bearing method) instead.
    pub async fn context_state(&self, worktree: &Path) -> anyhow::Result<(WorktreeStatus, Oid)> {
        let args = vec![
            OsString::from("-c"),
            OsString::from("filter.shade-content.process="),
            OsString::from("-c"),
            OsString::from("filter.shade-content.required=false"),
            OsString::from("status"),
            OsString::from("--porcelain=v2"),
            OsString::from("--branch"),
            OsString::from("-z"),
            OsString::from("--untracked-files=all"),
            OsString::from("--ignore-submodules=none"),
            OsString::from("--no-renames"),
            OsString::from("--no-ahead-behind"),
        ];
        let output = self.run_raw(Some(worktree), &args, &[], None).await?;
        self.require_success(&output)?;
        let status = parse_status(&output.stdout)?;
        let head = parse_status_head(&output.stdout)?;
        Ok((status, head))
    }

    /// Policy-bearing status for lifecycle decisions. The inventory must run
    /// before `git status`: a deliberately bypassed required clean filter can
    /// otherwise execute first and obscure the actionable policy violation.
    pub async fn status(&self, worktree: &Path) -> anyhow::Result<WorktreeStatus> {
        // Inspect index path metadata before `git status`: a deliberately
        // bypassed required filter can otherwise make status invoke the clean
        // driver first and obscure the actionable policy violation.
        self.validate_worktree_policy(worktree).await?;
        let args = vec![
            OsString::from("-c"),
            OsString::from("filter.shade-content.process="),
            OsString::from("-c"),
            OsString::from("filter.shade-content.required=false"),
            OsString::from("status"),
            OsString::from("--porcelain=v2"),
            OsString::from("-z"),
            OsString::from("--untracked-files=all"),
            OsString::from("--ignore-submodules=none"),
        ];
        let output = self.run_raw(Some(worktree), &args, &[], None).await?;
        self.require_success(&output)?;
        parse_status(&output.stdout)
    }

    /// Capture the detached HEAD, the user's real index, and the working tree
    /// as three independently recoverable states. Two consecutive complete
    /// captures must agree before any retention ref is created. That
    /// optimistic fence makes a fork/checkpoint either a coherent snapshot or
    /// an actionable quiescence failure even though the agent owns ordinary
    /// filesystem writes. The synthetic commits are implementation anchors
    /// only and live exclusively below `refs/shade/`.
    pub async fn checkpoint(
        &self,
        repository: &ManagedRepository,
        worktree: &Path,
        workspace_key: &str,
        checkpoint_key: &str,
    ) -> anyhow::Result<Checkpoint> {
        validate_ref_segment(workspace_key)?;
        validate_ref_segment(checkpoint_key)?;

        let mut previous = self
            .capture_checkpoint_snapshot(repository, worktree)
            .await?;
        let mut stable = None;
        for _ in 1..CHECKPOINT_STABILITY_ATTEMPTS {
            let current = self
                .capture_checkpoint_snapshot(repository, worktree)
                .await?;
            if current == previous {
                stable = Some(current);
                break;
            }
            previous = current;
        }
        let snapshot = stable.ok_or_else(|| {
            anyhow!(
                "WORKSPACE_NOT_QUIESCENT: workspace changed during {} consecutive checkpoint captures",
                CHECKPOINT_STABILITY_ATTEMPTS
            )
        })?;

        let index_commit = self
            .commit_tree(
                repository,
                &snapshot.index_tree,
                &snapshot.head,
                &format!("Shade checkpoint {checkpoint_key}: index\n"),
            )
            .await?;
        let working_commit = self
            .commit_tree(
                repository,
                &snapshot.working_tree,
                &index_commit,
                &format!("Shade checkpoint {checkpoint_key}: working tree\n"),
            )
            .await?;
        let ref_prefix = checkpoint_ref_prefix(workspace_key, checkpoint_key);
        crate::faults::hit(crate::faults::Point::CheckpointObjectsWritten);
        let transaction = format!(
            "start\ncreate {ref_prefix}/head {}\ncreate {ref_prefix}/index {index_commit}\ncreate {ref_prefix}/worktree {working_commit}\nprepare\ncommit\n",
            snapshot.head
        );
        let args = Self::repo_args(repository, &["update-ref", "--stdin"]);
        let output = self
            .run_raw(None, &args, &[], Some(transaction.as_bytes()))
            .await?;
        self.require_success(&output)?;

        crate::faults::hit(crate::faults::Point::CheckpointAnchored);
        Ok(Checkpoint {
            ref_prefix,
            head: snapshot.head,
            index_tree: snapshot.index_tree,
            working_tree: snapshot.working_tree,
            index_commit,
            working_commit,
        })
    }

    async fn capture_checkpoint_snapshot(
        &self,
        repository: &ManagedRepository,
        worktree: &Path,
    ) -> anyhow::Result<CheckpointSnapshot> {
        let status = self.status(worktree).await?;
        ensure!(
            status.unmerged_paths.is_empty(),
            "UNMERGED_WORKTREE: cannot checkpoint conflicts"
        );

        let head = self.worktree_head(worktree).await?;
        let head_tree = self.worktree_revision_tree(worktree, &head).await?;
        self.validate_checkout_policy(repository, &head_tree)
            .await?;
        let index_tree = self.write_worktree_index(repository, worktree, &[]).await?;
        self.validate_checkout_policy(repository, &index_tree)
            .await?;

        let index_directory = tempfile::Builder::new()
            .prefix(".shade-git-index-")
            .tempdir_in(
                repository
                    .git_dir()
                    .parent()
                    .context("repository needs a parent")?,
            )?;
        let index_path = index_directory.path().join("index");
        let environment = vec![(
            OsString::from("GIT_INDEX_FILE"),
            index_path.as_os_str().to_owned(),
        )];
        let args = vec![OsString::from("read-tree"), index_tree.as_str().into()];
        let output = self
            .run_raw(Some(worktree), &args, &environment, None)
            .await?;
        self.require_success(&output)?;

        // Git rejects an ignored literal path even in an exclusion pathspec.
        // Recursive globs cover root and nested dependency directories without
        // making an ignored directory an explicit argument to `git add`.
        let mut args = vec![
            OsString::from("add"),
            OsString::from("-A"),
            OsString::from("--"),
            OsString::from("."),
            OsString::from(":(exclude,glob).env*"),
            OsString::from(":(exclude,glob)**/.env*"),
            OsString::from(":(exclude,glob)**/node_modules/**"),
            OsString::from(":(exclude,glob)**/.venv/**"),
        ];
        let secret_paths = crate::secrets::discover_secret_paths(worktree)?;
        let mut ignored_secrets = BTreeSet::new();
        if !secret_paths.is_empty() {
            let mut input = Vec::new();
            for path in &secret_paths {
                input.extend_from_slice(path.as_bytes());
                input.push(0);
            }
            let output = self
                .run_raw(
                    Some(worktree),
                    &["check-ignore".into(), "--stdin".into(), "-z".into()],
                    &environment,
                    Some(&input),
                )
                .await?;
            if output.code != Some(1) {
                self.require_success(&output)?;
            }
            ignored_secrets.extend(
                output
                    .stdout
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                    .map(<[u8]>::to_vec),
            );
        }
        // Implicit traversal already skips ignored secrets. Exclude every
        // remaining secret literally, preserving names containing glob syntax.
        // The required clean filter also guards changes while Git reads files.
        for path in secret_paths {
            if ignored_secrets.contains(path.as_bytes()) {
                continue;
            }
            args.push(format!(":(exclude,literal){path}").into());
        }
        let output = self
            .run_raw(Some(worktree), &args, &environment, None)
            .await?;
        self.require_success(&output)?;
        let working_tree = self
            .write_worktree_index(repository, worktree, &environment)
            .await?;
        self.validate_checkout_policy(repository, &working_tree)
            .await?;
        Ok(CheckpointSnapshot {
            head,
            index_tree,
            working_tree,
        })
    }

    pub async fn load_checkpoint(
        &self,
        repository: &ManagedRepository,
        workspace_key: &str,
        checkpoint_key: &str,
    ) -> anyhow::Result<Checkpoint> {
        validate_ref_segment(workspace_key)?;
        validate_ref_segment(checkpoint_key)?;
        let ref_prefix = checkpoint_ref_prefix(workspace_key, checkpoint_key);
        let head = self
            .resolve_ref_oid(repository, &format!("{ref_prefix}/head"))
            .await?;
        let index_commit = self
            .resolve_ref_oid(repository, &format!("{ref_prefix}/index"))
            .await?;
        let working_commit = self
            .resolve_ref_oid(repository, &format!("{ref_prefix}/worktree"))
            .await?;
        let index_tree = self
            .revision_tree(repository, index_commit.as_str())
            .await?;
        let working_tree = self
            .revision_tree(repository, working_commit.as_str())
            .await?;
        Ok(Checkpoint {
            ref_prefix,
            head,
            index_tree,
            working_tree,
            index_commit,
            working_commit,
        })
    }

    pub async fn delete_checkpoint(
        &self,
        repository: &ManagedRepository,
        checkpoint: &Checkpoint,
    ) -> anyhow::Result<()> {
        ensure!(
            checkpoint.ref_prefix.starts_with("refs/shade/workspaces/"),
            "ref is outside the checkpoint namespace"
        );
        let transaction = format!(
            "start\ndelete {}/head {}\ndelete {}/index {}\ndelete {}/worktree {}\nprepare\ncommit\n",
            checkpoint.ref_prefix,
            checkpoint.head,
            checkpoint.ref_prefix,
            checkpoint.index_commit,
            checkpoint.ref_prefix,
            checkpoint.working_commit,
        );
        let args = Self::repo_args(repository, &["update-ref", "--stdin"]);
        let output = self
            .run_raw(None, &args, &[], Some(transaction.as_bytes()))
            .await?;
        self.require_success(&output)
    }

    /// Return only well-formed checkpoint keys in Shade's private namespace.
    /// Other private refs are intentionally ignored by recovery.
    pub async fn private_checkpoint_keys(
        &self,
        repository: &ManagedRepository,
    ) -> anyhow::Result<BTreeSet<(String, String)>> {
        let entries = self.private_checkpoint_ref_entries(repository).await?;
        Ok(entries
            .into_iter()
            .map(|(workspace, checkpoint, _, _)| (workspace, checkpoint))
            .collect())
    }

    /// Delete every currently-present plane for one exact checkpoint using
    /// compare-and-swap ref updates. This also converges partial ref groups
    /// left by external corruption without touching adjacent checkpoints.
    pub async fn delete_private_checkpoint_refs(
        &self,
        repository: &ManagedRepository,
        workspace_key: &str,
        checkpoint_key: &str,
    ) -> anyhow::Result<bool> {
        validate_ref_segment(workspace_key)?;
        validate_ref_segment(checkpoint_key)?;
        let mut entries = self
            .private_checkpoint_ref_entries(repository)
            .await?
            .into_iter()
            .filter(|(workspace, checkpoint, _, _)| {
                workspace == workspace_key && checkpoint == checkpoint_key
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Ok(false);
        }
        entries.sort_by(|left, right| left.2.cmp(&right.2));
        let mut transaction = String::from("start\n");
        for (_, _, reference, oid) in entries {
            transaction.push_str(&format!("delete {reference} {oid}\n"));
        }
        transaction.push_str("prepare\ncommit\n");
        let args = Self::repo_args(repository, &["update-ref", "--stdin"]);
        let output = self
            .run_raw(None, &args, &[], Some(transaction.as_bytes()))
            .await?;
        self.require_success(&output)?;
        Ok(true)
    }

    async fn private_checkpoint_ref_entries(
        &self,
        repository: &ManagedRepository,
    ) -> anyhow::Result<Vec<(String, String, String, Oid)>> {
        let args = Self::repo_args(
            repository,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/shade/workspaces",
            ],
        );
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;
        let text = String::from_utf8(output.stdout)?;
        let mut entries = Vec::new();
        for line in text.lines() {
            let Some((reference, oid)) = line.split_once(' ') else {
                continue;
            };
            let segments = reference.split('/').collect::<Vec<_>>();
            if segments.len() != 7
                || segments[..3] != ["refs", "shade", "workspaces"]
                || segments[4] != "checkpoints"
                || !matches!(segments[6], "head" | "index" | "worktree")
                || validate_ref_segment(segments[3]).is_err()
                || validate_ref_segment(segments[5]).is_err()
            {
                continue;
            }
            entries.push((
                segments[3].to_owned(),
                segments[5].to_owned(),
                reference.to_owned(),
                Oid::new(oid)?,
            ));
        }
        Ok(entries)
    }

    /// Restore all three checkpoint planes. A non-forced restore is a
    /// compare-before-destroy operation and refuses any current changes.
    pub async fn restore_checkpoint(
        &self,
        worktree: &Path,
        checkpoint: &Checkpoint,
        force: bool,
    ) -> anyhow::Result<()> {
        if !force {
            ensure!(
                self.status(worktree).await?.is_clean(),
                "DIRTY_WORKTREE: restore requires force"
            );
        }

        // Remove ordinary untracked paths which are not in the checkpoint,
        // but preserve private secrets and workspace dependency forests, which
        // are intentionally absent from Git checkpoint trees.
        let mut clean_args = vec![
            OsString::from("clean"),
            OsString::from("-d"),
            OsString::from("-f"),
            OsString::from("-e"),
            OsString::from(".env*"),
            OsString::from("-e"),
            OsString::from("**/.env*"),
            OsString::from("-e"),
            OsString::from("node_modules/"),
            OsString::from("-e"),
            OsString::from(".venv/"),
        ];
        for path in crate::secrets::discover_secret_paths(worktree)? {
            let escaped = path
                .chars()
                .flat_map(|ch| {
                    if "\\*?[]!#".contains(ch) {
                        vec!['\\', ch]
                    } else {
                        vec![ch]
                    }
                })
                .collect::<String>();
            clean_args.extend([OsString::from("-e"), OsString::from(format!("/{escaped}"))]);
        }
        let output = self.run_raw(Some(worktree), &clean_args, &[], None).await?;
        self.require_success(&output)?;
        crate::faults::hit(crate::faults::Point::RestoreCleaned);

        let working_args = vec![
            OsString::from("read-tree"),
            OsString::from("--reset"),
            OsString::from("-u"),
            checkpoint.working_tree.as_str().into(),
        ];
        let output = self
            .run_raw(Some(worktree), &working_args, &[], None)
            .await?;
        self.require_success(&output)?;

        crate::faults::hit(crate::faults::Point::RestoreWorkingWritten);
        let head_args = vec![
            OsString::from("update-ref"),
            OsString::from("--no-deref"),
            OsString::from("HEAD"),
            checkpoint.head.as_str().into(),
        ];
        let output = self.run_raw(Some(worktree), &head_args, &[], None).await?;
        self.require_success(&output)?;
        crate::faults::hit(crate::faults::Point::RestoreHeadWritten);

        let index_args = vec![
            OsString::from("read-tree"),
            OsString::from("--reset"),
            checkpoint.index_tree.as_str().into(),
        ];
        let output = self.run_raw(Some(worktree), &index_args, &[], None).await?;
        self.require_success(&output)?;
        crate::faults::hit(crate::faults::Point::RestoreIndexWritten);
        Ok(())
    }

    pub async fn fork_checkpoint(
        &self,
        repository: &ManagedRepository,
        checkpoint: &Checkpoint,
        destination: &Path,
        lock_reason: &str,
    ) -> anyhow::Result<WorktreeHandle> {
        self.validate_checkout_policy(repository, &checkpoint.working_tree)
            .await?;
        self.materialize_tree(repository, &checkpoint.working_tree, destination)
            .await?;
        let head_tree = self
            .revision_tree(repository, checkpoint.head.as_str())
            .await?;
        let base = BaseRevision {
            commit: checkpoint.head.clone(),
            tree: head_tree,
            source_ref: None,
        };
        let handle = self
            .register_precloned_worktree(repository, destination, &base, lock_reason)
            .await?;
        let args = vec![
            OsString::from("read-tree"),
            OsString::from("--reset"),
            checkpoint.index_tree.as_str().into(),
        ];
        let output = self.run_raw(Some(destination), &args, &[], None).await?;
        self.require_success(&output)?;
        Ok(handle)
    }

    async fn worktree_head(&self, worktree: &Path) -> anyhow::Result<Oid> {
        let args = vec![
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("HEAD^{commit}"),
        ];
        let output = self.run_raw(Some(worktree), &args, &[], None).await?;
        self.require_success(&output)?;
        parse_oid(&output.stdout)
    }

    async fn worktree_revision_tree(&self, worktree: &Path, revision: &Oid) -> anyhow::Result<Oid> {
        let args = vec![
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from(format!("{}^{{tree}}", revision.as_str())),
        ];
        let output = self.run_raw(Some(worktree), &args, &[], None).await?;
        self.require_success(&output)?;
        parse_oid(&output.stdout)
    }

    async fn validate_index_contents(
        &self,
        repository: &ManagedRepository,
        worktree: &Path,
        environment: &[(OsString, OsString)],
    ) -> anyhow::Result<()> {
        let output = self
            .run_raw(
                Some(worktree),
                &["ls-files".into(), "--stage".into(), "-z".into()],
                environment,
                None,
            )
            .await?;
        self.require_success(&output)?;
        let mut entries = Vec::new();
        for record in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid Git index record")?;
            let fields = record[..tab]
                .split(|byte| *byte == b' ')
                .collect::<Vec<_>>();
            ensure!(
                fields.len() == 3 && fields[2] == b"0",
                "UNMERGED_WORKTREE: index contains conflicts"
            );
            ensure!(
                fields[0] != b"160000",
                "UNSUPPORTED_SUBMODULE: index contains a gitlink"
            );
            entries.push(TreeEntry {
                mode: fields[0].to_vec(),
                object: parse_oid(fields[1])?,
                path: bytes_to_path(&record[tab + 1..]),
                size: None,
            });
        }
        for entry in &entries {
            validate_entry_path(entry)?;
        }
        // Size metadata is read without write-tree: even a deliberately
        // contaminated index must not make Shade create its tree object.
        for batch in entries.chunks_mut(1024) {
            let input = batch
                .iter()
                .map(|entry| format!("{}\n", entry.object))
                .collect::<String>();
            let args = Self::repo_args(
                repository,
                &[
                    "cat-file",
                    "--batch-check=%(objectname) %(objecttype) %(objectsize)",
                ],
            );
            let output = self
                .run_raw(None, &args, &[], Some(input.as_bytes()))
                .await?;
            self.require_success(&output)?;
            let mut records = output
                .stdout
                .split(|byte| *byte == b'\n')
                .filter(|record| !record.is_empty());
            for entry in batch.iter_mut() {
                let fields = records
                    .next()
                    .context("missing index object metadata")?
                    .split(|byte| *byte == b' ')
                    .collect::<Vec<_>>();
                ensure!(
                    fields.len() == 3
                        && fields[1] == b"blob"
                        && parse_oid(fields[0])? == entry.object,
                    "invalid index object metadata"
                );
                entry.size = Some(std::str::from_utf8(fields[2])?.parse()?);
            }
            ensure!(records.next().is_none(), "unexpected index object metadata");
            self.validate_blobs(repository, &batch.iter().collect::<Vec<_>>())
                .await?;
        }
        Ok(())
    }

    async fn write_worktree_index(
        &self,
        repository: &ManagedRepository,
        worktree: &Path,
        environment: &[(OsString, OsString)],
    ) -> anyhow::Result<Oid> {
        // Freeze index metadata privately before validating or creating a
        // tree. A concurrent ordinary `git add` cannot swap new object IDs
        // between the policy check and write-tree.
        let path_args = [
            "rev-parse".into(),
            "--path-format=absolute".into(),
            "--git-path".into(),
            "index".into(),
        ];
        let output = self
            .run_raw(Some(worktree), &path_args, environment, None)
            .await?;
        self.require_success(&output)?;
        let source = bytes_to_path(trim_ascii_newline(&output.stdout));
        let directory = tempfile::Builder::new()
            .prefix(".shade-git-index-")
            .tempdir_in(
                repository
                    .git_dir()
                    .parent()
                    .context("repository needs a parent")?,
            )?;
        let snapshot = directory.path().join("index");
        match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)
        {
            Ok(mut file) => {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut file, &mut bytes)?;
                fs::write(&snapshot, bytes)?;
                fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o600))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut private_environment = environment
            .iter()
            .filter(|(key, _)| key != "GIT_INDEX_FILE")
            .cloned()
            .collect::<Vec<_>>();
        private_environment.push(("GIT_INDEX_FILE".into(), snapshot.into_os_string()));
        self.validate_index_contents(repository, worktree, &private_environment)
            .await?;
        let args = vec![OsString::from("write-tree")];
        let output = self
            .run_raw(Some(worktree), &args, &private_environment, None)
            .await?;
        self.require_success(&output)?;
        parse_oid(&output.stdout)
    }

    async fn validate_worktree_policy(&self, worktree: &Path) -> anyhow::Result<()> {
        let tracked_args = vec![OsString::from("ls-files"), OsString::from("-z")];
        let tracked_output = self
            .run_raw(Some(worktree), &tracked_args, &[], None)
            .await?;
        self.require_success(&tracked_output)?;
        let tracked: BTreeSet<Vec<u8>> = tracked_output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        for path in &tracked {
            let path = bytes_to_path(path);
            let basename = path.file_name().unwrap_or_default().as_bytes();
            ensure!(
                !basename.starts_with(b".env"),
                "TRACKED_SECRET_FILE: {}",
                path.display()
            );
            ensure!(
                !is_dependency_output_path(&path),
                "TRACKED_DEPENDENCY_OUTPUT: {}",
                path.display()
            );
        }

        let staged_args = vec![
            OsString::from("ls-files"),
            OsString::from("--stage"),
            OsString::from("-z"),
        ];
        let staged_output = self
            .run_raw(Some(worktree), &staged_args, &[], None)
            .await?;
        self.require_success(&staged_output)?;
        for record in staged_output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            ensure!(
                !record.starts_with(b"160000 "),
                "UNSUPPORTED_SUBMODULE: index contains a gitlink"
            );
        }

        let candidates_args = vec![
            OsString::from("ls-files"),
            OsString::from("--cached"),
            OsString::from("--others"),
            OsString::from("--exclude-standard"),
            OsString::from("-z"),
        ];
        let candidate_output = self
            .run_raw(Some(worktree), &candidates_args, &[], None)
            .await?;
        self.require_success(&candidate_output)?;
        let mut attribute_input = Vec::new();
        for raw_path in candidate_output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let relative = bytes_to_path(raw_path);
            let basename = relative.file_name().unwrap_or_default().as_bytes();
            if basename.starts_with(b".env") && !tracked.contains(raw_path) {
                continue;
            }
            if is_dependency_output_path(&relative) {
                continue;
            }
            ensure!(
                basename != b".gitmodules",
                "UNSUPPORTED_SUBMODULE: .gitmodules is present"
            );
            ensure!(basename != b".lfsconfig", "UNSUPPORTED_GIT_LFS");
            attribute_input.extend_from_slice(raw_path);
            attribute_input.push(0);

            let absolute = worktree.join(&relative);
            if basename == b".gitattributes" {
                match fs::read(&absolute) {
                    Ok(bytes) => ensure!(
                        !attributes_declare_filter(&bytes),
                        "UNSUPPORTED_GIT_FILTER: {} declares a filter",
                        relative.display()
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if fs::symlink_metadata(&absolute)
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= 1024)
            {
                let bytes = fs::read(&absolute)?;
                ensure!(
                    !bytes.starts_with(LFS_POINTER_HEADER),
                    "UNSUPPORTED_GIT_LFS: {}",
                    relative.display()
                );
            }
        }
        if !attribute_input.is_empty() {
            let args = vec![
                OsString::from("check-attr"),
                OsString::from("-z"),
                OsString::from("--stdin"),
                OsString::from("--all"),
            ];
            let output = self
                .run_raw(Some(worktree), &args, &[], Some(&attribute_input))
                .await?;
            self.require_success(&output)?;
            reject_filter_attributes(&output.stdout)?;
        }

        Ok(())
    }

    async fn commit_tree(
        &self,
        repository: &ManagedRepository,
        tree: &Oid,
        parent: &Oid,
        message: &str,
    ) -> anyhow::Result<Oid> {
        let mut args = Self::repo_args(repository, &["commit-tree"]);
        args.push(tree.as_str().into());
        args.push("-p".into());
        args.push(parent.as_str().into());
        args.push("-F".into());
        args.push("-".into());
        let environment = vec![
            (OsString::from("GIT_AUTHOR_NAME"), OsString::from("Shade")),
            (
                OsString::from("GIT_AUTHOR_EMAIL"),
                OsString::from("shade@localhost"),
            ),
            (
                OsString::from("GIT_COMMITTER_NAME"),
                OsString::from("Shade"),
            ),
            (
                OsString::from("GIT_COMMITTER_EMAIL"),
                OsString::from("shade@localhost"),
            ),
        ];
        let output = self
            .run_raw(None, &args, &environment, Some(message.as_bytes()))
            .await?;
        self.require_success(&output)?;
        parse_oid(&output.stdout)
    }

    async fn resolve_ref_oid(
        &self,
        repository: &ManagedRepository,
        reference: &str,
    ) -> anyhow::Result<Oid> {
        ensure!(reference.starts_with("refs/"), "expected a full ref name");
        let mut args = Self::repo_args(repository, &["rev-parse", "--verify", "--end-of-options"]);
        args.push(reference.into());
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;
        parse_oid(&output.stdout)
    }

    async fn revision_tree(
        &self,
        repository: &ManagedRepository,
        revision: &str,
    ) -> anyhow::Result<Oid> {
        let expression = format!("{revision}^{{tree}}");
        let mut args = Self::repo_args(repository, &["rev-parse", "--verify", "--end-of-options"]);
        args.push(expression.into());
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;
        parse_oid(&output.stdout)
    }

    /// Replay a checkpoint onto a successor worktree without mutating the
    /// original. The original-base→index and index→working deltas are merged
    /// as separate planes so committed workspace changes are not lost and a
    /// clean result retains staged versus unstaged state.
    pub async fn integrate_checkpoint(
        &self,
        repository: &ManagedRepository,
        successor_root: &Path,
        original_base: &Oid,
        new_base: &BaseRevision,
        checkpoint: &Checkpoint,
    ) -> anyhow::Result<IntegrationOutcome> {
        let original_base_tree = self
            .revision_tree(repository, original_base.as_str())
            .await?;
        self.validate_checkout_policy(repository, &original_base_tree)
            .await?;
        self.validate_checkout_policy(repository, &new_base.tree)
            .await?;
        self.validate_checkout_policy(repository, &checkpoint.index_tree)
            .await?;
        self.validate_checkout_policy(repository, &checkpoint.working_tree)
            .await?;
        let current = self.worktree_head(successor_root).await?;
        ensure!(
            current == new_base.commit,
            "successor HEAD does not match the requested new base"
        );
        let current_status = self.status(successor_root).await?;
        ensure!(
            current_status.staged == 0
                && current_status.unstaged == 0
                && current_status.conflicted == 0,
            "successor has tracked changes"
        );

        let staged_merge = self
            .merge_tree_detailed(
                repository,
                original_base,
                &new_base.commit,
                &checkpoint.index_commit,
            )
            .await?;
        if !staged_merge.clean {
            self.install_merge_result(
                successor_root,
                &staged_merge.tree,
                &staged_merge.tree,
                &staged_merge.conflicts,
                new_base.commit.as_str().len(),
            )
            .await?;
            return Ok(IntegrationOutcome {
                clean: false,
                paths: staged_merge.conflict_paths(),
            });
        }

        let staged_commit = self
            .commit_tree(
                repository,
                &staged_merge.tree,
                &new_base.commit,
                "Shade successor staged plane\n",
            )
            .await?;
        let working_merge = self
            .merge_tree_detailed(
                repository,
                &checkpoint.index_commit,
                &staged_commit,
                &checkpoint.working_commit,
            )
            .await?;
        if !working_merge.clean {
            self.install_merge_result(
                successor_root,
                &working_merge.tree,
                &staged_merge.tree,
                &working_merge.conflicts,
                new_base.commit.as_str().len(),
            )
            .await?;
            return Ok(IntegrationOutcome {
                clean: false,
                paths: working_merge.conflict_paths(),
            });
        }

        self.install_tree_and_index(successor_root, &working_merge.tree, &staged_merge.tree)
            .await?;
        Ok(IntegrationOutcome {
            clean: true,
            paths: Vec::new(),
        })
    }

    async fn install_tree_and_index(
        &self,
        worktree: &Path,
        working_tree: &Oid,
        index_tree: &Oid,
    ) -> anyhow::Result<()> {
        let args = vec![
            OsString::from("read-tree"),
            OsString::from("--reset"),
            OsString::from("-u"),
            working_tree.as_str().into(),
        ];
        let output = self.run_raw(Some(worktree), &args, &[], None).await?;
        self.require_success(&output)?;
        let args = vec![
            OsString::from("read-tree"),
            OsString::from("--reset"),
            index_tree.as_str().into(),
        ];
        let output = self.run_raw(Some(worktree), &args, &[], None).await?;
        self.require_success(&output)
    }

    async fn install_merge_result(
        &self,
        worktree: &Path,
        working_tree: &Oid,
        index_tree: &Oid,
        conflicts: &[ConflictStage],
        oid_width: usize,
    ) -> anyhow::Result<()> {
        self.install_tree_and_index(worktree, working_tree, index_tree)
            .await?;
        let paths: BTreeSet<&[u8]> = conflicts
            .iter()
            .map(|conflict| conflict.path.as_slice())
            .collect();
        let zero = "0".repeat(oid_width);
        let mut index_information = Vec::new();
        for path in paths {
            index_information.extend_from_slice(b"0 ");
            index_information.extend_from_slice(zero.as_bytes());
            index_information.push(b'\t');
            index_information.extend_from_slice(path);
            index_information.push(0);
            for conflict in conflicts.iter().filter(|entry| entry.path == path) {
                index_information.extend_from_slice(&conflict.mode);
                index_information.push(b' ');
                index_information.extend_from_slice(conflict.object.as_str().as_bytes());
                index_information.push(b' ');
                index_information.extend_from_slice(conflict.stage.to_string().as_bytes());
                index_information.push(b'\t');
                index_information.extend_from_slice(&conflict.path);
                index_information.push(0);
            }
        }
        let args = vec![
            OsString::from("update-index"),
            OsString::from("-z"),
            OsString::from("--index-info"),
        ];
        let output = self
            .run_raw(Some(worktree), &args, &[], Some(&index_information))
            .await?;
        self.require_success(&output)
    }

    /// Prepare and privately anchor the checkpoint's aggregate
    /// original-base→working-tree change. This phase never changes a public
    /// branch and is safe to repeat after a crash.
    pub async fn prepare_squash_publish(
        &self,
        repository: &ManagedRepository,
        request: PrepareSquashPublishRequest<'_>,
    ) -> anyhow::Result<PreparedPublish> {
        let PrepareSquashPublishRequest {
            remote,
            branch,
            original_base,
            expected_remote,
            checkpoint,
            message,
            anchor_ref,
        } = request;
        ensure!(
            !message.trim().is_empty(),
            "publish message cannot be empty"
        );
        self.validate_branch(branch).await?;
        ensure!(
            anchor_ref.starts_with("refs/shade/operations/") && anchor_ref.ends_with("/publish"),
            "publish anchor is outside the private operation namespace"
        );
        self.validate_refname(anchor_ref).await?;
        let target_ref = format!("refs/heads/{branch}");

        // A durable retry may observe the anchor created immediately before a
        // daemon crash. It is the sole prepared candidate for this operation,
        // so recover its immutable commit and tree without manufacturing a
        // second commit with different committer metadata.
        if let Some(commit) = self.try_resolve_ref_oid(repository, anchor_ref).await? {
            let tree = self.revision_tree(repository, commit.as_str()).await?;
            return Ok(PreparedPublish {
                previous_remote: expected_remote.cloned(),
                commit,
                tree,
                target_ref,
                anchor_ref: anchor_ref.to_owned(),
            });
        }

        let original_base_tree = self
            .revision_tree(repository, original_base.as_str())
            .await?;
        self.validate_checkout_policy(repository, &original_base_tree)
            .await?;
        self.validate_checkout_policy(repository, &checkpoint.working_tree)
            .await?;
        let (parent, tree) = if let Some(expected_remote) = expected_remote {
            let fetched = self.fetch_head(repository, remote, branch).await?;
            ensure!(
                fetched.commit == *expected_remote,
                "REMOTE_MOVED: expected {}, fetched {}",
                expected_remote,
                fetched.commit
            );
            let tree = if *original_base == *expected_remote {
                checkpoint.working_tree.clone()
            } else {
                self.merge_tree(
                    repository,
                    original_base,
                    expected_remote,
                    &checkpoint.working_commit,
                )
                .await?
            };
            (expected_remote.clone(), tree)
        } else {
            ensure!(
                self.remote_branch_oid(remote, branch).await?.is_none(),
                "REMOTE_BRANCH_EXISTS: refs/heads/{branch}"
            );
            (original_base.clone(), checkpoint.working_tree.clone())
        };
        let commit = self
            .commit_tree(repository, &tree, &parent, message)
            .await?;
        match self.cas_create_ref(repository, anchor_ref, &commit).await {
            Ok(()) => {}
            Err(error)
                if self
                    .try_resolve_ref_oid(repository, anchor_ref)
                    .await?
                    .as_ref()
                    == Some(&commit) => {}
            Err(error) => return Err(error),
        }
        crate::faults::hit(crate::faults::Point::PublishAnchored);
        Ok(PreparedPublish {
            previous_remote: expected_remote.cloned(),
            commit,
            tree,
            target_ref,
            anchor_ref: anchor_ref.to_owned(),
        })
    }

    /// Apply the user-visible local branch CAS. Re-observing the prepared
    /// commit is success, which closes the crash window after `update-ref`.
    pub async fn apply_prepared_publish_local(
        &self,
        repository: &ManagedRepository,
        prepared: &PreparedPublish,
        expected_local: Option<&Oid>,
    ) -> anyhow::Result<()> {
        let current = self
            .try_resolve_ref_oid(repository, &prepared.target_ref)
            .await?;
        if current.as_ref() == Some(&prepared.commit) {
            return Ok(());
        }
        match (expected_local, current.as_ref()) {
            (Some(expected), Some(observed)) if expected == observed => {
                self.cas_update_ref(repository, &prepared.target_ref, &prepared.commit, expected)
                    .await
            }
            (None, None) => {
                self.cas_create_ref(repository, &prepared.target_ref, &prepared.commit)
                    .await
            }
            _ => bail!("LOCAL_BRANCH_MOVED: {}", prepared.target_ref),
        }
    }

    /// Apply the explicit remote CAS. A remote already at the candidate is a
    /// successful replay of a push whose acknowledgement was lost.
    pub async fn apply_prepared_publish_remote(
        &self,
        repository: &ManagedRepository,
        remote: &RemoteIdentity,
        branch: &str,
        prepared: &PreparedPublish,
        expected_remote: Option<&Oid>,
    ) -> anyhow::Result<()> {
        self.validate_branch(branch).await?;
        let observed = self.remote_branch_oid(remote, branch).await?;
        if observed.as_ref() == Some(&prepared.commit) {
            return Ok(());
        }
        ensure!(
            observed.as_ref() == expected_remote,
            "REMOTE_MOVED: expected {}, observed {}",
            expected_remote.map_or("absent", Oid::as_str),
            observed.as_ref().map_or("absent", Oid::as_str),
        );

        let lease = format!(
            "--force-with-lease=refs/heads/{branch}:{}",
            expected_remote.map_or("", Oid::as_str)
        );
        let refspec = format!("{}:refs/heads/{branch}", prepared.commit.as_str());
        let mut args = Self::repo_args(
            repository,
            &[
                "push",
                "--porcelain",
                "--no-verify",
                "--no-follow-tags",
                "--no-recurse-submodules",
            ],
        );
        args.push(lease.into());
        args.push(remote.fetch_url.clone().into());
        // This is the only outgoing refspec. In particular, refs/shade/* are
        // neither enumerated nor reachable through the squash commit graph.
        args.push(refspec.into());
        let pushed = self.run_raw(None, &args, &[], None).await?;
        if pushed.success
            || self.remote_branch_oid(remote, branch).await?.as_ref() == Some(&prepared.commit)
        {
            Ok(())
        } else {
            self.require_success(&pushed)
        }
    }

    /// Best-effort compensation for a remote CAS rejection. The local ref is
    /// changed only while it still contains this operation's candidate.
    pub async fn rollback_prepared_publish_local(
        &self,
        repository: &ManagedRepository,
        prepared: &PreparedPublish,
        expected_local: Option<&Oid>,
    ) -> anyhow::Result<bool> {
        if self
            .try_resolve_ref_oid(repository, &prepared.target_ref)
            .await?
            .as_ref()
            != Some(&prepared.commit)
        {
            return Ok(false);
        }
        if let Some(previous) = expected_local {
            self.cas_update_ref(repository, &prepared.target_ref, previous, &prepared.commit)
                .await?;
        } else {
            self.cas_delete_ref(repository, &prepared.target_ref, &prepared.commit)
                .await?;
        }
        Ok(true)
    }

    pub async fn delete_publish_anchor(
        &self,
        repository: &ManagedRepository,
        anchor_ref: &str,
        expected: &Oid,
    ) -> anyhow::Result<bool> {
        ensure!(
            anchor_ref.starts_with("refs/shade/operations/") && anchor_ref.ends_with("/publish"),
            "publish anchor is outside the private operation namespace"
        );
        match self.try_resolve_ref_oid(repository, anchor_ref).await? {
            None => Ok(false),
            Some(observed) if observed == *expected => {
                self.cas_delete_ref(repository, anchor_ref, expected)
                    .await?;
                Ok(true)
            }
            Some(_) => bail!("PUBLISH_ANCHOR_MOVED: {anchor_ref}"),
        }
    }

    pub async fn ensure_publish_anchor(
        &self,
        repository: &ManagedRepository,
        anchor_ref: &str,
        commit: &Oid,
    ) -> anyhow::Result<()> {
        ensure!(
            anchor_ref.starts_with("refs/shade/operations/") && anchor_ref.ends_with("/publish"),
            "publish anchor is outside the private operation namespace"
        );
        self.validate_refname(anchor_ref).await?;
        match self.try_resolve_ref_oid(repository, anchor_ref).await? {
            Some(observed) if observed == *commit => Ok(()),
            Some(_) => bail!("PUBLISH_ANCHOR_MOVED: {anchor_ref}"),
            None => self.cas_create_ref(repository, anchor_ref, commit).await,
        }
    }

    pub async fn publish_anchor_oid(
        &self,
        repository: &ManagedRepository,
        anchor_ref: &str,
    ) -> anyhow::Result<Option<Oid>> {
        ensure!(
            anchor_ref.starts_with("refs/shade/operations/") && anchor_ref.ends_with("/publish"),
            "publish anchor is outside the private operation namespace"
        );
        self.try_resolve_ref_oid(repository, anchor_ref).await
    }

    /// Convenience facade for callers which do not need durable phase
    /// boundaries. The engine uses the phase methods directly.
    pub async fn squash_publish(
        &self,
        repository: &ManagedRepository,
        request: SquashPublishRequest<'_>,
    ) -> anyhow::Result<PublishOutcome> {
        let anchor_ref = format!("refs/shade/operations/{}/publish", ulid::Ulid::new());
        let prepared = self
            .prepare_squash_publish(
                repository,
                PrepareSquashPublishRequest {
                    remote: request.remote,
                    branch: request.branch,
                    original_base: request.original_base,
                    expected_remote: request.expected_remote,
                    checkpoint: request.checkpoint,
                    message: request.message,
                    anchor_ref: &anchor_ref,
                },
            )
            .await?;
        if let Err(error) = self
            .apply_prepared_publish_local(repository, &prepared, request.expected_local)
            .await
        {
            let _ = self
                .delete_publish_anchor(repository, &prepared.anchor_ref, &prepared.commit)
                .await;
            return Err(error);
        }
        if request.push
            && let Err(error) = self
                .apply_prepared_publish_remote(
                    repository,
                    request.remote,
                    request.branch,
                    &prepared,
                    request.expected_remote,
                )
                .await
        {
            // Preserve the previous all-or-nothing behavior of this
            // non-durable convenience path. The engine's durable saga keeps
            // the applied local phase and reconciles the remote independently.
            let _ = self
                .rollback_prepared_publish_local(repository, &prepared, request.expected_local)
                .await;
            let _ = self
                .delete_publish_anchor(repository, &prepared.anchor_ref, &prepared.commit)
                .await;
            return Err(error);
        }
        self.delete_publish_anchor(repository, &prepared.anchor_ref, &prepared.commit)
            .await?;
        Ok(PublishOutcome {
            previous_remote: prepared.previous_remote,
            commit: prepared.commit,
            tree: prepared.tree,
            target_ref: prepared.target_ref,
        })
    }

    pub async fn remote_branch_oid(
        &self,
        remote: &RemoteIdentity,
        branch: &str,
    ) -> anyhow::Result<Option<Oid>> {
        self.validate_branch(branch).await?;
        let reference = format!("refs/heads/{branch}");
        let args = vec![
            OsString::from("ls-remote"),
            OsString::from("--exit-code"),
            OsString::from("--heads"),
            remote.fetch_url.clone().into(),
            reference.clone().into(),
        ];
        let output = self.run_raw(None, &args, &[], None).await?;
        if !output.success {
            if output.code == Some(2) {
                return Ok(None);
            }
            self.require_success(&output)?;
        }
        let line = trim_ascii_newline(&output.stdout);
        let separator = line
            .iter()
            .position(|byte| *byte == b'\t')
            .context("malformed ls-remote branch response")?;
        let oid = &line[..separator];
        let returned_ref = &line[separator + 1..];
        ensure!(
            returned_ref == reference.as_bytes(),
            "remote returned another ref"
        );
        Ok(Some(Oid::new(String::from_utf8(oid.to_vec())?)?))
    }

    pub async fn local_branch_oid(
        &self,
        repository: &ManagedRepository,
        branch: &str,
    ) -> anyhow::Result<Option<Oid>> {
        self.validate_branch(branch).await?;
        self.try_resolve_ref_oid(repository, &format!("refs/heads/{branch}"))
            .await
    }

    async fn merge_tree(
        &self,
        repository: &ManagedRepository,
        merge_base: &Oid,
        target: &Oid,
        changes: &Oid,
    ) -> anyhow::Result<Oid> {
        let result = self
            .merge_tree_detailed(repository, merge_base, target, changes)
            .await?;
        ensure!(result.clean, "MERGE_CONFLICT");
        Ok(result.tree)
    }

    async fn merge_tree_detailed(
        &self,
        repository: &ManagedRepository,
        merge_base: &Oid,
        target: &Oid,
        changes: &Oid,
    ) -> anyhow::Result<MergeTreeResult> {
        let merge_base_argument = format!("--merge-base={merge_base}");
        let mut args = Self::repo_args(
            repository,
            &["merge-tree", "--write-tree", "-z", "--messages"],
        );
        args.push(merge_base_argument.into());
        args.push(target.as_str().into());
        args.push(changes.as_str().into());
        let output = self.run_raw(None, &args, &[], None).await?;
        if !output.success && output.code != Some(1) {
            self.require_success(&output)?;
        }
        let mut records = output.stdout.split(|byte| *byte == 0);
        let tree = parse_oid(records.next().context("merge-tree returned no tree")?)?;
        let mut conflicts = Vec::new();
        for record in records {
            if record.is_empty() {
                break;
            }
            let separator = record
                .iter()
                .position(|byte| *byte == b'\t')
                .context("malformed merge-tree conflict entry")?;
            let metadata: Vec<_> = record[..separator].split(|byte| *byte == b' ').collect();
            ensure!(
                metadata.len() == 3,
                "malformed merge-tree conflict metadata"
            );
            conflicts.push(ConflictStage {
                mode: metadata[0].to_vec(),
                object: Oid::new(String::from_utf8(metadata[1].to_vec())?)?,
                stage: std::str::from_utf8(metadata[2])?.parse()?,
                path: record[separator + 1..].to_vec(),
            });
        }
        Ok(MergeTreeResult {
            tree,
            clean: output.success,
            conflicts,
        })
    }

    pub async fn cas_update_ref(
        &self,
        repository: &ManagedRepository,
        reference: &str,
        new: &Oid,
        expected: &Oid,
    ) -> anyhow::Result<()> {
        ensure!(
            reference.starts_with("refs/"),
            "CAS requires a full ref name"
        );
        ensure!(
            !reference.starts_with("refs/tags/"),
            "Shade does not update tags"
        );
        let mut args = Self::repo_args(repository, &["update-ref"]);
        args.push(reference.into());
        args.push(new.as_str().into());
        args.push(expected.as_str().into());
        let output = self.run_raw(None, &args, &[], None).await?;
        if !output.success
            && self
                .try_resolve_ref_oid(repository, reference)
                .await?
                .as_ref()
                != Some(expected)
        {
            bail!("LOCAL_BRANCH_MOVED: {reference}");
        }
        self.require_success(&output)
    }

    async fn cas_create_ref(
        &self,
        repository: &ManagedRepository,
        reference: &str,
        new: &Oid,
    ) -> anyhow::Result<()> {
        let transaction = format!("start\ncreate {reference} {new}\nprepare\ncommit\n");
        let args = Self::repo_args(repository, &["update-ref", "--stdin"]);
        let output = self
            .run_raw(None, &args, &[], Some(transaction.as_bytes()))
            .await?;
        if !output.success
            && self
                .try_resolve_ref_oid(repository, reference)
                .await?
                .is_some()
        {
            bail!("LOCAL_BRANCH_MOVED: {reference}");
        }
        self.require_success(&output)
    }

    async fn cas_delete_ref(
        &self,
        repository: &ManagedRepository,
        reference: &str,
        expected: &Oid,
    ) -> anyhow::Result<()> {
        let mut args = Self::repo_args(repository, &["update-ref", "-d"]);
        args.push(reference.into());
        args.push(expected.as_str().into());
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)
    }

    async fn try_resolve_ref_oid(
        &self,
        repository: &ManagedRepository,
        reference: &str,
    ) -> anyhow::Result<Option<Oid>> {
        let mut args = Self::repo_args(
            repository,
            &["rev-parse", "--verify", "--quiet", "--end-of-options"],
        );
        args.push(reference.into());
        let output = self.run_raw(None, &args, &[], None).await?;
        if output.success {
            Ok(Some(parse_oid(&output.stdout)?))
        } else if output.code == Some(1) {
            Ok(None)
        } else {
            self.require_success(&output)?;
            unreachable!()
        }
    }

    pub async fn registered_worktrees(
        &self,
        repository: &ManagedRepository,
    ) -> anyhow::Result<BTreeSet<PathBuf>> {
        let args = Self::repo_args(repository, &["worktree", "list", "--porcelain", "-z"]);
        let output = self.run_raw(None, &args, &[], None).await?;
        self.require_success(&output)?;
        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter_map(|field| field.strip_prefix(b"worktree "))
            .map(|path| PathBuf::from(OsString::from_vec(path.to_vec())))
            .collect())
    }

    /// Remove a registration known to be outside the durable control plane.
    /// The exact target must still be registered and remain beneath the
    /// daemon-owned workspace root. Unlike normal removal this also handles a
    /// worktree directory that vanished before the daemon crashed.
    pub async fn remove_orphan_worktree(
        &self,
        repository: &ManagedRepository,
        root: &Path,
        workspace_root: &Path,
    ) -> anyhow::Result<()> {
        ensure!(root.is_absolute(), "orphan worktree path is not absolute");
        ensure!(
            root.components().all(|component| !matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )),
            "orphan worktree path contains traversal"
        );
        let trusted_root = fs::canonicalize(workspace_root)?;
        let checked_root = match fs::canonicalize(root) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => root.to_owned(),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            checked_root.starts_with(&trusted_root) && checked_root != trusted_root,
            "orphan worktree is outside the daemon workspace root"
        );
        ensure!(
            self.registered_worktrees(repository).await?.contains(root),
            "orphan worktree registration changed"
        );

        let mut unlock = Self::repo_args(repository, &["worktree", "unlock"]);
        unlock.push(root.as_os_str().to_owned());
        let unlocked = self.run_raw(None, &unlock, &[], None).await?;
        if !unlocked.success && !String::from_utf8_lossy(&unlocked.stderr).contains("not locked") {
            self.require_success(&unlocked)?;
        }
        crate::faults::hit(crate::faults::Point::ReconcileWorktreeUnlocked);

        let mut remove = Self::repo_args(repository, &["worktree", "remove", "--force"]);
        remove.push(root.as_os_str().to_owned());
        let output = self.run_raw(None, &remove, &[], None).await?;
        self.require_success(&output)?;
        ensure!(
            !self.registered_worktrees(repository).await?.contains(root),
            "orphan worktree registration survived removal"
        );
        Ok(())
    }

    /// Remove only a linked worktree proven to belong to this managed bare
    /// repository. `force` is the explicit authority to discard local work.
    pub async fn remove_worktree(
        &self,
        repository: &ManagedRepository,
        root: &Path,
        force: bool,
    ) -> anyhow::Result<()> {
        let root = fs::canonicalize(root)
            .with_context(|| format!("cannot canonicalize worktree {}", root.display()))?;
        ensure!(
            root.parent().is_some(),
            "refusing to remove a filesystem root"
        );
        let repository_root = fs::canonicalize(repository.git_dir())?;
        ensure!(
            root != repository_root,
            "refusing to remove the object store"
        );
        let dot_git_metadata = fs::symlink_metadata(root.join(".git"))?;
        ensure!(
            dot_git_metadata.is_file() && !dot_git_metadata.file_type().is_symlink(),
            "linked worktree .git must be a regular file"
        );

        let args = vec![
            OsString::from("rev-parse"),
            OsString::from("--path-format=absolute"),
            OsString::from("--git-common-dir"),
        ];
        let output = self.run_raw(Some(&root), &args, &[], None).await?;
        self.require_success(&output)?;
        let common = fs::canonicalize(bytes_to_path(trim_ascii_newline(&output.stdout)))?;
        ensure!(
            common == repository_root,
            "worktree belongs to another repository"
        );

        let args = vec![
            OsString::from("rev-parse"),
            OsString::from("--path-format=absolute"),
            OsString::from("--git-dir"),
        ];
        let output = self.run_raw(Some(&root), &args, &[], None).await?;
        self.require_success(&output)?;
        let admin = fs::canonicalize(bytes_to_path(trim_ascii_newline(&output.stdout)))?;
        ensure!(
            admin.starts_with(repository_root.join("worktrees")),
            "worktree administration is outside the managed repository"
        );
        ensure!(
            self.worktree_is_registered(repository, &root).await?,
            "worktree is not registered"
        );
        if !force {
            ensure!(
                self.status(&root).await?.is_clean(),
                "DIRTY_WORKTREE: removal requires force"
            );
        }

        let mut unlock = Self::repo_args(repository, &["worktree", "unlock"]);
        unlock.push(root.as_os_str().to_owned());
        let unlocked = self.run_raw(None, &unlock, &[], None).await?;
        // An already-unlocked but otherwise valid registration is harmless.
        if !unlocked.success && !String::from_utf8_lossy(&unlocked.stderr).contains("not locked") {
            self.require_success(&unlocked)?;
        }

        let mut remove = Self::repo_args(repository, &["worktree", "remove"]);
        if force {
            remove.push("--force".into());
        }
        remove.push(root.as_os_str().to_owned());
        let output = self.run_raw(None, &remove, &[], None).await?;
        self.require_success(&output)
    }

    async fn worktree_is_registered(
        &self,
        repository: &ManagedRepository,
        root: &Path,
    ) -> anyhow::Result<bool> {
        Ok(self.registered_worktrees(repository).await?.contains(root))
    }
}

fn checkpoint_ref_prefix(workspace_key: &str, checkpoint_key: &str) -> String {
    format!("refs/shade/workspaces/{workspace_key}/checkpoints/{checkpoint_key}")
}

fn validate_ref_segment(segment: &str) -> anyhow::Result<()> {
    ensure!(!segment.is_empty(), "private ref segment cannot be empty");
    ensure!(
        segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "invalid private ref segment"
    );
    ensure!(
        segment != "." && segment != "..",
        "invalid private ref segment"
    );
    Ok(())
}

fn is_dependency_output_path(path: &Path) -> bool {
    path.components().any(|component| {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        name == OsStr::new("node_modules") || name == OsStr::new(".venv")
    })
}

fn parse_tree_entry(record: &[u8]) -> anyhow::Result<TreeEntry> {
    let separator = record
        .iter()
        .position(|byte| *byte == b'\t')
        .context("malformed ls-tree record")?;
    let metadata = &record[..separator];
    let path = &record[separator + 1..];
    let fields: Vec<&[u8]> = metadata
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect();
    ensure!(fields.len() == 4, "malformed ls-tree metadata");
    let object = Oid::new(String::from_utf8(fields[2].to_vec())?)?;
    let size = if fields[3] == b"-" {
        None
    } else {
        Some(std::str::from_utf8(fields[3])?.parse()?)
    };
    Ok(TreeEntry {
        mode: fields[0].to_vec(),
        object,
        path: bytes_to_path(path),
        size,
    })
}

fn attributes_declare_filter(bytes: &[u8]) -> bool {
    bytes.split(|byte| *byte == b'\n').any(|line| {
        let line = line.split(|byte| *byte == b'#').next().unwrap_or_default();
        line.split(|byte| byte.is_ascii_whitespace())
            .skip(1)
            .any(|attribute| {
                attribute == b"filter"
                    || attribute == b"-filter"
                    || attribute == b"!filter"
                    || attribute.starts_with(b"filter=")
            })
    })
}

fn reject_filter_attributes(bytes: &[u8]) -> anyhow::Result<()> {
    let fields: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    for triple in fields.chunks(3) {
        if triple.len() == 3
            && triple[1] == b"filter"
            && triple[2] != b"unspecified"
            && triple[2] != b"unset"
            && triple[2] != b"shade-content"
        {
            bail!(
                "UNSUPPORTED_GIT_FILTER: {} uses filter {}",
                bytes_to_path(triple[0]).display(),
                String::from_utf8_lossy(triple[2])
            );
        }
    }
    Ok(())
}

fn parse_status(bytes: &[u8]) -> anyhow::Result<WorktreeStatus> {
    let records: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    let mut status = WorktreeStatus::default();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        if record.is_empty() {
            index += 1;
            continue;
        }
        match record[0] {
            b'1' => {
                let fields = split_fields(record, 9)?;
                count_xy(fields[1], &mut status);
                if fields[1].contains(&b'U') {
                    status.unmerged_paths.push(bytes_to_path(fields[8]));
                }
            }
            b'2' => {
                let fields = split_fields(record, 10)?;
                count_xy(fields[1], &mut status);
                if fields[1].contains(&b'U') {
                    status.unmerged_paths.push(bytes_to_path(fields[9]));
                }
                // Porcelain v2 emits the original rename path as the next
                // NUL-delimited field.
                index += 1;
            }
            b'u' => {
                let fields = split_fields(record, 11)?;
                status.conflicted += 1;
                status.unmerged_paths.push(bytes_to_path(fields[10]));
            }
            b'?' => status.untracked += 1,
            b'!' | b'#' => {}
            other => bail!("unknown porcelain-v2 status record {other}"),
        }
        index += 1;
    }
    status.unmerged_paths.sort();
    status.unmerged_paths.dedup();
    Ok(status)
}

fn parse_status_head(bytes: &[u8]) -> anyhow::Result<Oid> {
    const PREFIX: &[u8] = b"# branch.oid ";
    let value = bytes
        .split(|byte| *byte == 0)
        .find_map(|record| record.strip_prefix(PREFIX))
        .context("porcelain-v2 status omitted branch.oid")?;
    ensure!(value != b"(initial)", "workspace HEAD is unborn");
    Oid::new(String::from_utf8(value.to_vec())?)
}

fn split_fields(record: &[u8], fields: usize) -> anyhow::Result<Vec<&[u8]>> {
    let result: Vec<&[u8]> = record.splitn(fields, |byte| *byte == b' ').collect();
    ensure!(result.len() == fields, "malformed porcelain-v2 record");
    Ok(result)
}

fn count_xy(xy: &[u8], status: &mut WorktreeStatus) {
    if xy.first().is_some_and(|value| *value != b'.') {
        status.staged += 1;
    }
    if xy.get(1).is_some_and(|value| *value != b'.') {
        status.unstaged += 1;
    }
    if xy.contains(&b'U') {
        status.conflicted += 1;
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn batch_blobs_use_declared_lengths_and_opaque_object_ids() {
        for width in [40, 64] {
            let object = Oid::new("a".repeat(width)).unwrap();
            let mut output = format!("{object} blob 4\n").into_bytes();
            output.extend_from_slice(b"\0\n\xffx\n");
            output.extend_from_slice(format!("{object} blob 0\n\n").as_bytes());
            let mut remaining = output.as_slice();
            assert_eq!(
                take_batch_blob(&mut remaining, &object, Some(4)).unwrap(),
                b"\0\n\xffx"
            );
            assert!(
                take_batch_blob(&mut remaining, &object, Some(0))
                    .unwrap()
                    .is_empty()
            );
            assert!(remaining.is_empty());
        }
    }

    #[test]
    fn invalid_batch_objects_fail_closed_without_echoing_contents() {
        let object = Oid::new("abcd").unwrap();
        for output in [
            "abcd missing\n",
            "ffff blob 1\nx\n",
            "abcd tree 1\nx\n",
            "abcd blob 2\nx\n",
            "abcd blob 1\n",
            "abcd blob 1\nx!",
            "abcd blob 18446744073709551615\nx\n",
            "abcd blob 1",
        ] {
            assert!(take_batch_blob(&mut output.as_bytes(), &object, Some(1)).is_err());
        }
    }
}
