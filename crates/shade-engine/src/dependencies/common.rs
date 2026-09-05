use super::{
    DependencyContext, DependencyError, DependencyGcEntry, DependencyGcReport, DependencyReceipt,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, FileTimes, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::{Builder, TempDir};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use ulid::Ulid;
use walkdir::{DirEntry, WalkDir};

use crate::faults::{Point, hit};
use crate::filesystem::WorkspaceFilesystem;

pub(crate) const FINGERPRINT_VERSION: &str = "shade-dependencies/v1";
pub(crate) const RECEIPT_VERSION: u32 = 1;
const OWNERSHIP_MARKER_NAME: &str = ".shade-dependency-artifact";
const OWNERSHIP_MARKER: &[u8] = b"shade-dependency-artifact/v1\n";

#[derive(Debug, Clone)]
pub(crate) struct ToolIdentity {
    pub logical_name: String,
    pub path_identity: String,
    pub path: PathBuf,
    pub version: String,
    pub digest: String,
    /// The exact host Node used for a Node-based package manager. Invoke it
    /// explicitly so canonicalizing a manager symlink cannot lose its runtime.
    pub interpreter: Option<Box<ToolIdentity>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Approval {
    pub package: String,
    pub version: String,
    pub integrity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredReceipt {
    pub schema_version: u32,
    pub provider: String,
    pub fingerprint: String,
    pub state: String,
    pub materialized_paths: Vec<String>,
    pub blocked_builds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<shade_protocol::DependencyScript>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<Approval>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub layer_digests: BTreeMap<String, String>,
    pub tools: Vec<StoredTool>,
    pub platform: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredTool {
    pub name: String,
    pub path_identity: String,
    pub version: String,
    pub digest: String,
}

impl StoredReceipt {
    pub(crate) fn new(
        provider: &str,
        fingerprint: String,
        materialized_paths: Vec<String>,
        blocked_builds: Vec<String>,
        approvals: Vec<Approval>,
        tool: &ToolIdentity,
    ) -> Self {
        Self {
            schema_version: RECEIPT_VERSION,
            provider: provider.to_owned(),
            fingerprint,
            state: "ready".to_owned(),
            materialized_paths,
            blocked_builds,
            scripts: Vec::new(),
            approvals,
            layer_digests: BTreeMap::new(),
            tools: Vec::new(),
            platform: platform_identity(),
        }
        .with_tool(tool)
    }

    pub(crate) fn with_tool(mut self, tool: &ToolIdentity) -> Self {
        if self.tools.iter().any(|stored| {
            stored.name == tool.logical_name
                && stored.path_identity == tool.path_identity
                && stored.version == tool.version
                && stored.digest == tool.digest
        }) {
            return self;
        }
        self.tools.push(StoredTool {
            name: tool.logical_name.clone(),
            path_identity: tool.path_identity.clone(),
            version: tool.version.clone(),
            digest: tool.digest.clone(),
        });
        if let Some(interpreter) = &tool.interpreter {
            self = self.with_tool(interpreter);
        }
        self
    }

    pub(crate) fn public(&self) -> DependencyReceipt {
        DependencyReceipt {
            provider: self.provider.clone(),
            fingerprint: self.fingerprint.clone(),
            state: self.state.clone(),
            materialized_paths: self.materialized_paths.clone(),
            blocked_builds: self.blocked_builds.clone(),
            scripts: self.scripts.clone(),
        }
    }
}

pub(crate) struct FingerprintBuilder {
    hasher: Sha256,
}

impl FingerprintBuilder {
    pub(crate) fn new(provider: &str) -> Self {
        let mut this = Self {
            hasher: Sha256::new(),
        };
        this.field("schema", FINGERPRINT_VERSION.as_bytes());
        this.field("provider", provider.as_bytes());
        this.field("platform", platform_identity().as_bytes());
        this
    }

    pub(crate) fn tool(&mut self, tool: &ToolIdentity) {
        // Deliberately identify a tool by logical name, version and content.  The
        // resolved executable path is operational data and must never enter a
        // portable fingerprint.
        self.field("tool-name", tool.logical_name.as_bytes());
        self.field("tool-path-identity", tool.path_identity.as_bytes());
        self.field("tool-version", tool.version.as_bytes());
        self.field("tool-digest", tool.digest.as_bytes());
        if let Some(interpreter) = &tool.interpreter {
            self.tool(interpreter);
        }
    }

    pub(crate) fn field(&mut self, label: &str, value: &[u8]) {
        self.hasher.update((label.len() as u64).to_be_bytes());
        self.hasher.update(label.as_bytes());
        self.hasher.update((value.len() as u64).to_be_bytes());
        self.hasher.update(value);
    }

    pub(crate) fn file(&mut self, root: &Path, path: &Path) -> Result<(), DependencyError> {
        let relative = relative_portable(root, path)?;
        let bytes = fs::read(path).map_err(|error| DependencyError::Io {
            action: "read dependency input".to_owned(),
            path: relative.clone(),
            message: error.to_string(),
        })?;
        self.field(&format!("file:{relative}"), &bytes);
        Ok(())
    }

    pub(crate) fn semantic_json(
        &mut self,
        label: &str,
        bytes: &[u8],
    ) -> Result<(), DependencyError> {
        let mut value: Value = serde_json::from_slice(bytes).map_err(|error| {
            DependencyError::InvalidConfiguration {
                provider: "policy".to_owned(),
                path: label.to_owned(),
                reason: format!("invalid JSON: {error}"),
            }
        })?;
        redact_secret_fields(&mut value);
        let canonical = serde_json::to_vec(&value).map_err(|error| {
            DependencyError::Failed(format!("failed to canonicalize {label}: {error}"))
        })?;
        self.field(label, &canonical);
        Ok(())
    }

    pub(crate) fn finish(self) -> String {
        hex::encode(self.hasher.finalize())
    }
}

pub(crate) fn identify_tool(
    logical_name: &str,
    configured_path: Option<&Path>,
) -> Result<ToolIdentity, DependencyError> {
    identify_tool_at(logical_name, configured_path, Path::new("."), &[])
}

pub(crate) fn identify_tool_at(
    logical_name: &str,
    configured_path: Option<&Path>,
    cwd: &Path,
    env: &[(OsString, OsString)],
) -> Result<ToolIdentity, DependencyError> {
    let launcher = match configured_path {
        Some(path) if path.is_file() => path.to_path_buf(),
        Some(_) => {
            return Err(DependencyError::ToolUnavailable(format!(
                "{logical_name} (configured executable does not exist)"
            )));
        }
        None => super::command_path(logical_name)?,
    };
    let launcher = if launcher.is_absolute() {
        launcher
    } else {
        cwd.join(launcher)
    };
    let launcher = fs::canonicalize(&launcher).map_err(|error| {
        DependencyError::ToolUnavailable(format!(
            "{logical_name} (cannot canonicalize executable: {error})"
        ))
    })?;
    let path = resolve_rustup_proxy(logical_name, &launcher, cwd, env)?
        .or(resolve_corepack_proxy(logical_name, &launcher, cwd)?)
        .unwrap_or_else(|| launcher.clone());
    let bytes = fs::read(&path).map_err(|error| {
        DependencyError::ToolUnavailable(format!(
            "{logical_name} (cannot read executable: {error})"
        ))
    })?;
    let digest = hex::encode(Sha256::digest(&bytes));
    let interpreter = if matches!(logical_name, "npm" | "pnpm")
        && bytes
            .split(|byte| *byte == b'\n')
            .next()
            .is_some_and(|line| line == b"#!/usr/bin/env node" || line == b"#!/usr/bin/node")
    {
        Some(Box::new(identify_tool_at("node", None, cwd, env)?))
    } else {
        None
    };
    let mut version_command = match &interpreter {
        Some(node) => {
            let mut command = base_command(&node.path, cwd);
            command.arg(&path);
            command
        }
        None => base_command(&path, cwd),
    };
    version_command
        .arg(if logical_name == "go" {
            "version"
        } else {
            "--version"
        })
        .envs(env.iter().cloned());
    let output = version_command.output().map_err(|error| {
        DependencyError::ToolUnavailable(format!(
            "{logical_name} (cannot execute --version: {error})"
        ))
    })?;
    if !output.status.success() {
        return Err(DependencyError::ToolUnavailable(format!(
            "{logical_name} (--version exited with {})",
            output.status
        )));
    }
    let raw = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        String::from_utf8_lossy(&output.stdout)
    };
    let version = scrub_paths(
        raw.lines().next().unwrap_or_default().trim(),
        &[cwd, &launcher, &path],
    );
    if version.is_empty() {
        return Err(DependencyError::ToolUnavailable(format!(
            "{logical_name} (--version produced no version)"
        )));
    }
    Ok(ToolIdentity {
        logical_name: logical_name.to_owned(),
        // Operational paths can include usernames and checkout locations.
        // Content plus the logical executable name identifies the exact host
        // tool without putting an absolute path (or a hash of one) in portable
        // fingerprints and receipts.
        path_identity: format!("{logical_name}@sha256:{digest}"),
        path,
        version,
        digest,
        interpreter,
    })
}

fn resolve_rustup_proxy(
    logical_name: &str,
    launcher: &Path,
    cwd: &Path,
    env: &[(OsString, OsString)],
) -> Result<Option<PathBuf>, DependencyError> {
    if !matches!(logical_name, "cargo" | "rustc") {
        return Ok(None);
    }

    // rustup proxies are commonly symlinks, but may also be hardlinks.  In
    // either form hashing the proxy would fingerprint rustup itself instead of
    // the Cargo/rustc selected by the repository's rust-toolchain file.  Find
    // the sibling rustup launcher and only trust it when it is the same file.
    let rustup = if launcher.file_name() == Some(OsStr::new("rustup")) {
        launcher.to_path_buf()
    } else {
        let Some(parent) = launcher.parent() else {
            return Ok(None);
        };
        let candidate = parent.join("rustup");
        if !candidate.is_file() || !same_executable_file(launcher, &candidate)? {
            return Ok(None);
        }
        fs::canonicalize(&candidate).map_err(|error| {
            DependencyError::ToolUnavailable(format!(
                "{logical_name} (cannot canonicalize rustup proxy: {error})"
            ))
        })?
    };

    let mut command = base_command(&rustup, cwd);
    command.args(["which", logical_name]);
    if let Some(home) = dirs::home_dir() {
        command.env("HOME", &home);
    }
    command.envs(env.iter().cloned());
    let output = command.output().map_err(|error| {
        DependencyError::ToolUnavailable(format!(
            "{logical_name} (cannot resolve repository toolchain through rustup: {error})"
        ))
    })?;
    if !output.status.success() {
        let diagnostic = scrub_paths(
            &String::from_utf8_lossy(&output.stderr),
            &[cwd, launcher, &rustup],
        );
        return Err(DependencyError::ToolUnavailable(format!(
            "{logical_name} (rustup could not select the repository toolchain: {diagnostic})"
        )));
    }
    let selected = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !selected.is_absolute() {
        return Err(DependencyError::ToolUnavailable(format!(
            "{logical_name} (rustup returned a non-absolute tool path)"
        )));
    }
    let selected = fs::canonicalize(&selected).map_err(|error| {
        DependencyError::ToolUnavailable(format!(
            "{logical_name} (cannot canonicalize selected repository tool: {error})"
        ))
    })?;
    Ok(Some(selected))
}

fn resolve_corepack_proxy(
    logical_name: &str,
    launcher: &Path,
    cwd: &Path,
) -> Result<Option<PathBuf>, DependencyError> {
    if !matches!(logical_name, "npm" | "pnpm") {
        return Ok(None);
    }
    let corepack_manifest = launcher
        .parent()
        .and_then(Path::parent)
        .and_then(|root| fs::read(root.join("package.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    if corepack_manifest
        .as_ref()
        .and_then(|manifest| manifest["name"].as_str())
        != Some("corepack")
    {
        return Ok(None);
    }
    // Never execute the Corepack dispatcher: it may download a manager or
    // select a different version. Resolve only an already installed v1 cache
    // package and fingerprint/invoke its actual entry point plus host Node.
    let unavailable = || {
        DependencyError::ToolUnavailable(format!(
            "{logical_name} (pin an already installed Corepack version in packageManager; Shade never downloads package managers)"
        ))
    };
    let corepack_root = if let Some(root) = std::env::var_os("COREPACK_HOME") {
        PathBuf::from(root)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".cache")))
            .ok_or_else(unavailable)?
            .join("node/corepack")
    };
    let project = fs::read(cwd.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let declared = project
        .as_ref()
        .and_then(|value| value["packageManager"].as_str())
        .and_then(|value| value.strip_prefix(&format!("{logical_name}@")))
        .map(str::to_owned);
    let selected = declared
        .or_else(|| {
            fs::read(corepack_root.join("lastKnownGood.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|value| value[logical_name].as_str().map(str::to_owned))
        })
        .ok_or_else(unavailable)?;
    let version = selected.split('+').next().ok_or_else(unavailable)?;
    if version.is_empty()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err(unavailable());
    }
    let package = fs::canonicalize(corepack_root.join("v1").join(logical_name).join(version))
        .map_err(|_| unavailable())?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(package.join("package.json")).map_err(|_| unavailable())?)
            .map_err(|_| unavailable())?;
    if manifest["name"] != logical_name || manifest["version"] != version {
        return Err(unavailable());
    }
    let binary = manifest["bin"][logical_name]
        .as_str()
        .or_else(|| manifest["bin"].as_str())
        .ok_or_else(unavailable)?;
    let selected = fs::canonicalize(package.join(binary)).map_err(|_| unavailable())?;
    if !selected.starts_with(&package) || !selected.is_file() {
        return Err(unavailable());
    }
    Ok(Some(selected))
}

fn same_executable_file(left: &Path, right: &Path) -> Result<bool, DependencyError> {
    let left = fs::metadata(left).map_err(|error| {
        DependencyError::ToolUnavailable(format!(
            "cannot inspect configured Rust tool proxy: {error}"
        ))
    })?;
    let right = fs::metadata(right).map_err(|error| {
        DependencyError::ToolUnavailable(format!("cannot inspect rustup proxy: {error}"))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(not(unix))]
    {
        Ok(left.len() == right.len() && left.modified().ok() == right.modified().ok())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ToolIsolation<'a> {
    pub offline: bool,
    pub hidden_paths: &'a [PathBuf],
}

impl ToolIsolation<'_> {
    pub const fn network(offline: bool) -> Self {
        Self {
            offline,
            hidden_paths: &[],
        }
    }
}

pub(crate) fn run_tool<I, S>(
    provider: &str,
    phase: &str,
    tool: &ToolIdentity,
    cwd: &Path,
    args: I,
    env: &[(OsString, OsString)],
    isolation: ToolIsolation<'_>,
) -> Result<Output, DependencyError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if isolation
        .hidden_paths
        .iter()
        .any(|path| path.to_str().is_none())
    {
        return Err(DependencyError::Policy(
            "configuration isolation requires UTF-8 paths".into(),
        ));
    }
    if (isolation.offline || !isolation.hidden_paths.is_empty()) && !cfg!(target_os = "macos") {
        return Err(DependencyError::Failed(
            "dependency isolation requires the macOS process sandbox".to_owned(),
        ));
    }
    let mut command = match &tool.interpreter {
        Some(node) => {
            let mut command = isolated_command(&node.path, cwd, isolation);
            command.arg(&tool.path);
            command
        }
        None => isolated_command(&tool.path, cwd, isolation),
    };
    command.args(args);
    command.envs(env.iter().cloned());
    let output = command
        .output()
        .map_err(|error| DependencyError::CommandFailed {
            provider: provider.to_owned(),
            phase: phase.to_owned(),
            command: tool.logical_name.clone(),
            status: None,
            stderr: format!("could not start: {error}"),
        })?;
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        });
        return Err(DependencyError::CommandFailed {
            provider: provider.to_owned(),
            phase: phase.to_owned(),
            command: tool.logical_name.clone(),
            status: output.status.code(),
            stderr: scrub_paths(&diagnostic, &[cwd, &tool.path]),
        });
    }
    Ok(output)
}

fn base_command(executable: &Path, cwd: &Path) -> Command {
    isolated_command(executable, cwd, ToolIsolation::network(false))
}

fn isolated_command(executable: &Path, cwd: &Path, isolation: ToolIsolation<'_>) -> Command {
    // Enforce replay isolation for the entire process tree, including tools
    // without an offline flag (Bun). Failure to apply the sandbox fails closed.
    let mut command = if isolation.offline || !isolation.hidden_paths.is_empty() {
        let mut command = Command::new("/usr/bin/sandbox-exec");
        let mut profile = "(version 1) (allow default)".to_owned();
        if isolation.offline {
            profile.push_str(" (deny network*)");
        }
        for path in isolation.hidden_paths {
            let quoted = path
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            profile.push_str(&format!(" (deny file-read* (literal \"{quoted}\"))"));
        }
        command.args(["-p", &profile]);
        command.arg(executable);
        command
    } else {
        Command::new(executable)
    };
    let path = executable
        .parent()
        .map(|parent| {
            let mut entries = vec![parent.to_path_buf()];
            entries.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
            std::env::join_paths(entries).unwrap_or_else(|_| OsString::from("/usr/bin:/bin"))
        })
        .unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    command
        .current_dir(cwd)
        .env_clear()
        .env("PATH", path)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("CI", "1")
        .env("NO_COLOR", "1");
    command
}

pub(crate) fn platform_identity() -> String {
    format!(
        "{}-{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        abi_identity()
    )
}

fn abi_identity() -> &'static str {
    if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(target_env = "gnu") {
        "gnu"
    } else if cfg!(target_env = "msvc") {
        "msvc"
    } else if cfg!(target_vendor = "apple") {
        "darwin"
    } else {
        "unknown-abi"
    }
}

pub(crate) fn policy_fingerprint(
    root: &Path,
    fingerprint: &mut FingerprintBuilder,
) -> Result<(), DependencyError> {
    fingerprint.field(
        "built-in-policy",
        b"frozen=true;scripts=false;network=macos-sandbox-replay-v1;artifact=validated;root-builds=false;python-bootstrap=exact",
    );
    let path = root.join(".shade/dependencies.json");
    if path.is_file() {
        let bytes = fs::read(&path).map_err(|error| DependencyError::Io {
            action: "read dependency policy".to_owned(),
            path: ".shade/dependencies.json".to_owned(),
            message: error.to_string(),
        })?;
        fingerprint.semantic_json("policy:.shade/dependencies.json", &bytes)?;
    }
    Ok(())
}

pub(crate) fn collect_named_files(
    root: &Path,
    names: &[&str],
) -> Result<Vec<PathBuf>, DependencyError> {
    let mut files = Vec::new();
    if !root.is_dir() {
        return Ok(files);
    }
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(should_descend)
    {
        let entry = entry.map_err(|error| {
            DependencyError::Failed(format!("failed to inspect dependency graph: {error}"))
        })?;
        if entry.file_type().is_file()
            && names
                .iter()
                .any(|name| entry.file_name() == OsStr::new(name))
        {
            files.push(entry.into_path());
        }
    }
    files.sort_by_key(|path| relative_portable(root, path).unwrap_or_default());
    Ok(files)
}

fn should_descend(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if !entry.file_type().is_dir() {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | ".shade" | ".venv" | "node_modules" | "target" | "vendor" | "dist" | "build")
    )
}

pub(crate) fn semantic_line_config(bytes: &[u8]) -> Vec<u8> {
    let source = String::from_utf8_lossy(bytes);
    let mut section = String::new();
    let mut entries = Vec::new();
    for raw in source.lines() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.to_ascii_lowercase();
            entries.push(format!("section:{section}"));
            continue;
        }
        let normalized = line
            .split_once('=')
            .map(|(key, value)| format!("{}:{}={}", section, key.trim(), value.trim()))
            .unwrap_or_else(|| format!("{}:{}", section, line));
        entries.push(normalized);
    }
    entries.sort();
    entries.join("\n").into_bytes()
}

pub(crate) fn create_staging(
    context: &DependencyContext<'_>,
    provider: &str,
    phase: &str,
) -> Result<TempDir, DependencyError> {
    let root = context.cache_root.join("dependencies/staging");
    fs::create_dir_all(&root).map_err(|error| io_error("create staging root", &root, error))?;
    let staging = Builder::new()
        .prefix(&format!("{provider}-{phase}-"))
        .tempdir_in(&root)
        .map_err(|error| io_error("create dependency staging directory", &root, error))?;
    hit(Point::DependencyStaged);
    Ok(staging)
}

pub(crate) fn native_cache(
    context: &DependencyContext<'_>,
    provider: &str,
) -> Result<PathBuf, DependencyError> {
    let path = context
        .runtime_root
        .join("dependency-native")
        .join(provider);
    fs::create_dir_all(&path)
        .map_err(|error| io_error("create native dependency cache", &path, error))?;
    Ok(path)
}

/// Startup runs before requests are accepted. Incomplete promotions are siblings
/// of final artifacts, outside the preparation staging directory.
pub(crate) fn cleanup_staging(cache_root: &Path) -> Result<u64, DependencyError> {
    let dependencies = cache_root.join("dependencies");
    let mut removed = cleanup_entries(&dependencies.join("staging"), |_| true)?;
    for provider in ["npm", "pnpm", "bun", "uv", "cargo", "go"] {
        removed += cleanup_entries(&dependencies.join("artifacts").join(provider), |name| {
            name.starts_with("promote-") || name.starts_with(".shade-evict-")
        })?;
    }
    Ok(removed)
}

fn cleanup_entries(root: &Path, owned: impl Fn(&str) -> bool) -> Result<u64, DependencyError> {
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(io_error("inspect dependency staging", root, error)),
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err(DependencyError::Policy(
                "dependency staging root is not an owned directory".into(),
            ));
        }
        Ok(_) => {}
    }
    let mut removed = 0;
    for entry in
        fs::read_dir(root).map_err(|error| io_error("scan dependency staging", root, error))?
    {
        let entry =
            entry.map_err(|error| io_error("read dependency staging entry", root, error))?;
        if !owned(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| io_error("inspect dependency staging entry", &path, error))?;
        let removal = if kind.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removal.map_err(|error| io_error("remove interrupted dependency staging", &path, error))?;
        removed += 1;
    }
    Ok(removed)
}

pub(crate) fn copy_input(
    repository_root: &Path,
    source: &Path,
    staging_root: &Path,
) -> Result<PathBuf, DependencyError> {
    let relative = source.strip_prefix(repository_root).map_err(|_| {
        DependencyError::Policy("dependency input escaped the repository".to_owned())
    })?;
    ensure_safe_relative(relative)?;
    let destination = staging_root.join(relative);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| io_error("create staged input parent", parent, error))?;
    }
    fs::copy(source, &destination)
        .map_err(|error| io_error("copy dependency input", source, error))?;
    Ok(destination)
}

pub(crate) fn artifact_dir(
    context: &DependencyContext<'_>,
    provider: &str,
    fingerprint: &str,
) -> PathBuf {
    context
        .cache_root
        .join("dependencies/artifacts")
        .join(provider)
        .join(fingerprint)
}

fn validate_owned_artifact(provider: &str, artifact: &Path) -> Result<(), DependencyError> {
    let metadata = fs::symlink_metadata(artifact)
        .map_err(|error| io_error("inspect dependency artifact", artifact, error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(DependencyError::Validation {
            provider: provider.to_owned(),
            reason: "cached artifact is not a daemon-owned directory".to_owned(),
        });
    }
    let marker = artifact.join(OWNERSHIP_MARKER_NAME);
    let marker_is_regular = fs::symlink_metadata(&marker)
        .ok()
        .is_some_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
    if !marker_is_regular || fs::read(&marker).ok().as_deref() != Some(OWNERSHIP_MARKER) {
        return Err(DependencyError::Validation {
            provider: provider.to_owned(),
            reason: "cached artifact is not marked as daemon-owned".to_owned(),
        });
    }
    Ok(())
}

pub(crate) fn invalidate_receipt(
    context: &DependencyContext<'_>,
    provider: &str,
    fingerprint: &str,
) -> Result<bool, DependencyError> {
    let artifact = artifact_dir(context, provider, fingerprint);
    match fs::symlink_metadata(&artifact) {
        Ok(_) => validate_owned_artifact(provider, &artifact)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(io_error("inspect dependency artifact", &artifact, error)),
    }
    remove_owned_artifact(&artifact)?;
    hit(Point::DependencyInvalidated);
    Ok(true)
}

pub(crate) fn load_receipt(
    context: &DependencyContext<'_>,
    provider: &str,
    fingerprint: &str,
) -> Result<Option<StoredReceipt>, DependencyError> {
    let artifact = artifact_dir(context, provider, fingerprint);
    let receipt_path = artifact.join("receipt.json");
    match fs::symlink_metadata(&artifact) {
        Ok(_) => validate_owned_artifact(provider, &artifact)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("inspect dependency artifact", &artifact, error)),
    }
    match fs::symlink_metadata(&receipt_path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            invalidate_receipt(context, provider, fingerprint)?;
            tracing::warn!(
                provider,
                fingerprint,
                "rebuilding dependency artifact with invalid receipt entry"
            );
            return Ok(None);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            invalidate_receipt(context, provider, fingerprint)?;
            tracing::warn!(
                provider,
                fingerprint,
                "rebuilding dependency artifact without receipt"
            );
            return Ok(None);
        }
        Err(error) => return Err(io_error("inspect dependency receipt", &receipt_path, error)),
    }
    let validation = (|| -> Result<StoredReceipt, DependencyError> {
        let bytes = fs::read(&receipt_path)
            .map_err(|error| io_error("read dependency receipt", &receipt_path, error))?;
        let receipt: StoredReceipt =
            serde_json::from_slice(&bytes).map_err(|error| DependencyError::Validation {
                provider: provider.to_owned(),
                reason: format!("cached receipt is invalid JSON: {error}"),
            })?;
        if receipt.schema_version != RECEIPT_VERSION
            || receipt.provider != provider
            || receipt.fingerprint != fingerprint
            || receipt.state != "ready"
            || receipt.platform != platform_identity()
        {
            return Err(DependencyError::Validation {
                provider: provider.to_owned(),
                reason: "cached receipt metadata does not match the requested artifact".to_owned(),
            });
        }
        for path in &receipt.materialized_paths {
            ensure_safe_relative(Path::new(path))?;
            let layer = artifact.join("payload").join(path);
            if !layer.is_dir() {
                return Err(DependencyError::Validation {
                    provider: provider.to_owned(),
                    reason: format!("cached artifact is missing {path}"),
                });
            }
            let expected =
                receipt
                    .layer_digests
                    .get(path)
                    .ok_or_else(|| DependencyError::Validation {
                        provider: provider.to_owned(),
                        reason: format!("cached receipt has no digest for {path}"),
                    })?;
            let actual = tree_digest(&layer)?;
            if expected != &actual {
                return Err(DependencyError::Validation {
                    provider: provider.to_owned(),
                    reason: format!("cached dependency layer {path} failed integrity validation"),
                });
            }
        }
        Ok(receipt)
    })();
    match validation {
        Ok(receipt) => {
            touch_receipt(&receipt_path)?;
            Ok(Some(receipt))
        }
        Err(error) => {
            // The ownership marker is the authority to remove this exact
            // immutable artifact. Existing workspaces hold independent COW
            // clones, so quarantine-by-removal cannot mutate an active agent.
            invalidate_receipt(context, provider, fingerprint)?;
            tracing::warn!(provider, fingerprint, reason = %error, "rebuilding corrupt dependency artifact");
            Ok(None)
        }
    }
}

pub(crate) fn promote(
    context: &DependencyContext<'_>,
    receipt: &StoredReceipt,
    outputs: &[(PathBuf, String)],
) -> Result<PathBuf, DependencyError> {
    let provider_root = context
        .cache_root
        .join("dependencies/artifacts")
        .join(&receipt.provider);
    fs::create_dir_all(&provider_root)
        .map_err(|error| io_error("create artifact store", &provider_root, error))?;
    let temp = Builder::new()
        .prefix("promote-")
        .tempdir_in(&provider_root)
        .map_err(|error| io_error("create promotion directory", &provider_root, error))?;
    hit(Point::DependencyPromotionStaged);
    let payload = temp.path().join("payload");
    fs::create_dir_all(&payload)
        .map_err(|error| io_error("create promotion payload", &payload, error))?;
    for (source, relative) in outputs {
        ensure_safe_relative(Path::new(relative))?;
        let destination = payload.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| io_error("create promotion layer parent", parent, error))?;
        }
        // Replay staging and the artifact store must share the daemon-owned
        // APFS pool. Crossing a filesystem boundary is a configuration error;
        // Shade never hides it behind a byte-copy fallback.
        fs::rename(source, &destination).map_err(|error| DependencyError::CowUnavailable {
            path: relative.clone(),
            reason: format!("dependency staging is outside the shared APFS pool: {error}"),
        })?;
        hit(Point::DependencyPayloadMoved);
    }
    let mut promoted_receipt = receipt.clone();
    for (_, relative) in outputs {
        promoted_receipt
            .layer_digests
            .insert(relative.clone(), tree_digest(&payload.join(relative))?);
    }
    let receipt_bytes = serde_json::to_vec_pretty(&promoted_receipt).map_err(|error| {
        DependencyError::Failed(format!("failed to encode dependency receipt: {error}"))
    })?;
    fs::write(temp.path().join(OWNERSHIP_MARKER_NAME), OWNERSHIP_MARKER)
        .map_err(|error| io_error("write dependency ownership marker", temp.path(), error))?;
    fs::write(temp.path().join("receipt.json"), receipt_bytes)
        .map_err(|error| io_error("write dependency receipt", temp.path(), error))?;
    hit(Point::DependencyReceiptWritten);
    let final_path = provider_root.join(&receipt.fingerprint);
    let staged_path = temp.keep();
    match fs::rename(&staged_path, &final_path) {
        Ok(()) => {
            hit(Point::DependencyPromoted);
            Ok(final_path)
        }
        Err(error) if final_path.join("receipt.json").is_file() => {
            let _ = fs::remove_dir_all(&staged_path);
            Ok(final_path)
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&staged_path);
            Err(io_error(
                "atomically promote dependency artifact",
                &final_path,
                error,
            ))
        }
    }
}

pub(crate) fn materialize(
    context: &DependencyContext<'_>,
    receipt: &StoredReceipt,
) -> Result<(), DependencyError> {
    let artifact = artifact_dir(context, &receipt.provider, &receipt.fingerprint);
    for relative in &receipt.materialized_paths {
        ensure_safe_relative(Path::new(relative))?;
        let source = artifact.join("payload").join(relative);
        let destination = context.workspace_root.join(relative);
        atomic_clone_tree(context.filesystem, &source, &destination, relative)?;
    }
    Ok(())
}

pub(crate) fn garbage_collect(
    cache_root: &Path,
    protected_fingerprints: HashSet<String>,
    max_bytes: u64,
) -> Result<DependencyGcReport, DependencyError> {
    #[derive(Debug)]
    struct Candidate {
        provider: String,
        fingerprint: String,
        path: PathBuf,
        bytes: u64,
        last_used_ns: u128,
        protected: bool,
    }

    let mut report = DependencyGcReport::default();
    let artifacts_root = cache_root.join("dependencies/artifacts");
    if !artifacts_root.is_dir() {
        return Ok(report);
    }
    let mut candidates = Vec::new();
    for provider in ["bun", "pnpm", "npm", "uv"] {
        let provider_root = artifacts_root.join(provider);
        if !provider_root.is_dir() {
            continue;
        }
        let mut entries = fs::read_dir(&provider_root)
            .map_err(|error| io_error("scan dependency artifacts", &provider_root, error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io_error("scan dependency artifact entry", &provider_root, error))?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let file_type = entry
                .file_type()
                .map_err(|error| io_error("inspect dependency artifact", &entry.path(), error))?;
            let fingerprint = entry.file_name().to_string_lossy().into_owned();
            if !file_type.is_dir() || !valid_fingerprint_directory(&fingerprint) {
                report.skipped_unowned = report.skipped_unowned.saturating_add(1);
                continue;
            }
            let path = entry.path();
            let receipt_path = path.join("receipt.json");
            let owned = fs::read(path.join(OWNERSHIP_MARKER_NAME)).ok().as_deref()
                == Some(OWNERSHIP_MARKER);
            let receipt = fs::read(&receipt_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<StoredReceipt>(&bytes).ok());
            let valid_receipt = receipt.is_some_and(|receipt| {
                receipt.schema_version == RECEIPT_VERSION
                    && receipt.provider == provider
                    && receipt.fingerprint == fingerprint
                    && receipt.state == "ready"
            });
            if !owned || !valid_receipt {
                report.skipped_unowned = report.skipped_unowned.saturating_add(1);
                continue;
            }
            let bytes = directory_bytes(&path)?;
            let last_used_ns = fs::metadata(&receipt_path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let protected = protected_fingerprints.contains(&fingerprint);
            report.before_bytes = report.before_bytes.saturating_add(bytes);
            if protected {
                report.protected_bytes = report.protected_bytes.saturating_add(bytes);
            }
            candidates.push(Candidate {
                provider: provider.to_owned(),
                fingerprint,
                path,
                bytes,
                last_used_ns,
                protected,
            });
        }
    }
    report.after_bytes = report.before_bytes;
    candidates.sort_by(|left, right| {
        (
            left.protected,
            left.last_used_ns,
            &left.provider,
            &left.fingerprint,
        )
            .cmp(&(
                right.protected,
                right.last_used_ns,
                &right.provider,
                &right.fingerprint,
            ))
    });
    for candidate in candidates {
        if report.after_bytes <= max_bytes {
            break;
        }
        if candidate.protected {
            continue;
        }
        validate_owned_artifact(&candidate.provider, &candidate.path)?;
        remove_owned_artifact(&candidate.path)?;
        report.after_bytes = report.after_bytes.saturating_sub(candidate.bytes);
        report.removed_bytes = report.removed_bytes.saturating_add(candidate.bytes);
        report.removed.push(DependencyGcEntry {
            provider: candidate.provider,
            fingerprint: candidate.fingerprint,
            bytes: candidate.bytes,
        });
    }
    Ok(report)
}

fn remove_owned_artifact(artifact: &Path) -> Result<(), DependencyError> {
    let parent = artifact
        .parent()
        .ok_or_else(|| DependencyError::Policy("dependency artifact has no parent".into()))?;
    let tombstone = parent.join(format!(".shade-evict-{}", Ulid::new()));
    fs::rename(artifact, &tombstone)
        .map_err(|error| io_error("claim dependency artifact deletion", artifact, error))?;
    fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_error("persist dependency deletion claim", parent, error))?;
    hit(Point::DependencyGcRenamed);
    let payload = tombstone.join("payload");
    match fs::symlink_metadata(&payload) {
        Ok(metadata) => {
            let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(&payload)
            } else {
                fs::remove_file(&payload)
            };
            result
                .map_err(|error| io_error("remove retired dependency payload", &payload, error))?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(io_error(
                "inspect retired dependency payload",
                &payload,
                error,
            ));
        }
    }
    hit(Point::DependencyGcPayloadRemoved);
    fs::remove_dir_all(&tombstone)
        .map_err(|error| io_error("remove retired dependency artifact", &tombstone, error))?;
    fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_error("persist dependency deletion", parent, error))?;
    hit(Point::DependencyGcDeleted);
    Ok(())
}

fn valid_fingerprint_directory(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn directory_bytes(root: &Path) -> Result<u64, DependencyError> {
    let mut bytes = 0_u64;
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| {
            DependencyError::Failed(format!("failed to measure dependency artifact: {error}"))
        })?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| io_error("measure dependency artifact entry", entry.path(), error))?;
        if metadata.is_file() || metadata.file_type().is_symlink() {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok(bytes)
}

fn touch_receipt(receipt_path: &Path) -> Result<(), DependencyError> {
    let file = OpenOptions::new()
        .write(true)
        .open(receipt_path)
        .map_err(|error| io_error("open dependency receipt for LRU touch", receipt_path, error))?;
    file.set_times(FileTimes::new().set_modified(SystemTime::now()))
        .map_err(|error| io_error("touch dependency receipt for LRU", receipt_path, error))
}

fn atomic_clone_tree(
    filesystem: &dyn WorkspaceFilesystem,
    source: &Path,
    destination: &Path,
    relative: &str,
) -> Result<(), DependencyError> {
    let parent = destination.parent().ok_or_else(|| {
        DependencyError::Policy("materialized dependency path has no parent".to_owned())
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| io_error("create materialization parent", parent, error))?;
    let name = destination
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("dependencies");
    let stage = parent.join(format!(".{name}.shade-stage-{}", Ulid::new()));
    if let Err(error) = filesystem.clone_immutable_tree(source, &stage) {
        let _ = filesystem.remove_tree(&stage);
        return Err(DependencyError::CowUnavailable {
            path: relative.to_owned(),
            reason: error.to_string(),
        });
    }
    hit(Point::DependencyCloneStaged);
    let backup = parent.join(format!(".{name}.shade-backup-{}", Ulid::new()));
    let had_existing = destination.symlink_metadata().is_ok();
    if had_existing && let Err(error) = fs::rename(destination, &backup) {
        let _ = filesystem.remove_tree(&stage);
        return Err(io_error(
            "stage existing dependency tree",
            destination,
            error,
        ));
    }
    if had_existing {
        hit(Point::DependencyExistingStaged);
    }
    if let Err(error) = fs::rename(&stage, destination) {
        if had_existing {
            let _ = fs::rename(&backup, destination);
        }
        let _ = filesystem.remove_tree(&stage);
        return Err(io_error(
            "atomically materialize dependencies",
            destination,
            error,
        ));
    }
    hit(Point::DependencyMaterialized);
    if had_existing {
        let _ = filesystem.remove_tree(&backup);
        hit(Point::DependencyBackupRemoved);
    }
    Ok(())
}

pub(crate) async fn single_flight(key: String) -> OwnedMutexGuard<()> {
    static FLIGHTS: OnceLock<Mutex<HashMap<String, Weak<AsyncMutex<()>>>>> = OnceLock::new();
    let lock = {
        let mut flights = FLIGHTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        flights.retain(|_, flight| flight.strong_count() > 0);
        if let Some(flight) = flights.get(&key).and_then(Weak::upgrade) {
            flight
        } else {
            let flight = Arc::new(AsyncMutex::new(()));
            flights.insert(key, Arc::downgrade(&flight));
            flight
        }
    };
    lock.lock_owned().await
}

pub(crate) fn relative_portable(root: &Path, path: &Path) -> Result<String, DependencyError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        DependencyError::Policy(format!(
            "dependency input {} is outside the repository",
            path.display()
        ))
    })?;
    ensure_safe_relative(relative)?;
    Ok(relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
}

pub(crate) fn ensure_safe_relative(path: &Path) -> Result<(), DependencyError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(DependencyError::Policy(format!(
            "dependency artifact path is not a safe relative path: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn read_bytes(path: &Path, label: &str) -> Result<Vec<u8>, DependencyError> {
    fs::read(path).map_err(|error| DependencyError::Io {
        action: format!("read {label}"),
        path: path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(label)
            .to_owned(),
        message: error.to_string(),
    })
}

pub(crate) fn reject_embedded_secrets(
    provider: &str,
    path: &str,
    bytes: &[u8],
) -> Result<(), DependencyError> {
    let source = String::from_utf8_lossy(bytes);
    let lower = source.to_ascii_lowercase();
    let secret_assignment = [
        "_authtoken",
        "authorization:",
        "authorization =",
        "password =",
        "password=",
        "access_token=",
        "access-token=",
        "secret_key=",
        "secret-key=",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let credential_url = source
        .split(|character: char| {
            character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ')' | ']' | '}')
        })
        .filter_map(|token| token.split_once("://").map(|(_, remainder)| remainder))
        .filter_map(|remainder| remainder.split('/').next())
        .any(|authority| {
            authority
                .rsplit_once('@')
                .map(|(userinfo, _)| {
                    userinfo.contains(':') || userinfo.to_ascii_lowercase().contains("token")
                })
                .unwrap_or(false)
        });
    if secret_assignment || credential_url {
        return Err(DependencyError::UnsafeConfiguration {
            provider: provider.to_owned(),
            path: path.to_owned(),
            reason: "secret-bearing dependency inputs are forbidden; inject credentials at execution time, never in manifests or locks"
                .to_owned(),
        });
    }
    Ok(())
}

fn io_error(action: &str, path: &Path, error: io::Error) -> DependencyError {
    DependencyError::Io {
        action: action.to_owned(),
        path: path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("dependency path")
            .to_owned(),
        message: error.to_string(),
    }
}

fn tree_digest(root: &Path) -> Result<String, DependencyError> {
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| DependencyError::Validation {
            provider: "artifact".to_owned(),
            reason: format!("could not traverse cached layer: {error}"),
        })?;
    entries.sort_by_key(|entry| {
        entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .into_owned()
    });
    let mut hasher = Sha256::new();
    for entry in entries {
        let relative = relative_portable(root, entry.path())?;
        hasher.update((relative.len() as u64).to_be_bytes());
        hasher.update(relative.as_bytes());
        if entry.file_type().is_dir() {
            hasher.update(b"directory");
        } else if entry.file_type().is_symlink() {
            hasher.update(b"symlink");
            let target = fs::read_link(entry.path())
                .map_err(|error| io_error("read cached layer symlink", entry.path(), error))?;
            if target.is_absolute() {
                return Err(DependencyError::Validation {
                    provider: "artifact".to_owned(),
                    reason: format!("cached layer has an absolute symlink at {relative}"),
                });
            }
            let target = target.to_string_lossy();
            hasher.update((target.len() as u64).to_be_bytes());
            hasher.update(target.as_bytes());
        } else if entry.file_type().is_file() {
            hasher.update(b"file");
            let bytes = fs::read(entry.path())
                .map_err(|error| io_error("read cached layer file", entry.path(), error))?;
            hasher.update((bytes.len() as u64).to_be_bytes());
            hasher.update(bytes);
        } else {
            return Err(DependencyError::Validation {
                provider: "artifact".to_owned(),
                reason: format!("cached layer contains an unsupported object at {relative}"),
            });
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

fn redact_secret_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !is_secret_key(key));
            for value in map.values_mut() {
                redact_secret_fields(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_secret_fields),
        _ => {}
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["token", "secret", "password", "authorization", "_auth"]
        .iter()
        .any(|needle| key.contains(needle))
}

fn scrub_paths(value: &str, known_paths: &[&Path]) -> String {
    let mut scrubbed = value.to_owned();
    for path in known_paths {
        if path.is_absolute()
            && let Some(path) = path.to_str()
            && !path.is_empty()
        {
            scrubbed = scrubbed.replace(path, "<path>");
        }
    }
    scrubbed
        .split_whitespace()
        .take(64)
        .map(|part| {
            if part.starts_with('/')
                || (part.len() > 2 && part.as_bytes()[1] == b':' && part.contains('\\'))
            {
                "<path>".to_owned()
            } else if diagnostic_contains_secret(part) {
                "<redacted>".to_owned()
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn diagnostic_contains_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "token=",
        "token:",
        "password=",
        "password:",
        "_auth",
        "authorization",
        "secret=",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || value
            .split_once("://")
            .and_then(|(_, remainder)| remainder.split('/').next())
            .and_then(|authority| authority.rsplit_once('@'))
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
}
