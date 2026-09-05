#![cfg(unix)]

use super::common::artifact_dir;
use super::*;
use std::fs::{self, FileTimes, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use tempfile::TempDir;

use crate::filesystem::{CopyFilesystem, Usage, WorkspaceFilesystem};

static COPY_FILESYSTEM: CopyFilesystem = CopyFilesystem;

struct FailingCowFilesystem;

impl WorkspaceFilesystem for FailingCowFilesystem {
    fn clone_tree(&self, _source: &Path, _destination: &Path) -> anyhow::Result<()> {
        anyhow::bail!("forced clone failure")
    }

    fn publish_tree(&self, _staging: &Path, _destination: &Path) -> anyhow::Result<()> {
        anyhow::bail!("forced publish failure")
    }

    fn usage(&self, _root: &Path) -> anyhow::Result<Usage> {
        Ok(Usage::default())
    }

    fn remove_tree(&self, root: &Path) -> anyhow::Result<()> {
        if root.is_dir() {
            fs::remove_dir_all(root)?;
        }
        Ok(())
    }
}

struct Layout {
    _temp: TempDir,
    repository: PathBuf,
    workspace: PathBuf,
    cache: PathBuf,
    runtime: PathBuf,
}

impl Layout {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        let workspace = temp.path().join("workspace");
        let cache = temp.path().join("cache");
        let runtime = temp.path().join("runtime");
        for path in [&repository, &workspace, &cache, &runtime] {
            fs::create_dir_all(path).unwrap();
        }
        Self {
            _temp: temp,
            repository,
            workspace,
            cache,
            runtime,
        }
    }

    fn context(&self) -> DependencyContext<'_> {
        DependencyContext {
            repository_root: &self.repository,
            workspace_root: &self.workspace,
            cache_root: &self.cache,
            runtime_root: &self.runtime,
            filesystem: &COPY_FILESYSTEM,
            script_approvals: &[],
        }
    }

    fn write(&self, relative: &str, contents: impl AsRef<[u8]>) {
        let path = self.repository.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn write_workspace(&self, relative: &str, contents: impl AsRef<[u8]>) {
        let path = self.workspace.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn write_both(&self, relative: &str, contents: impl AsRef<[u8]>) {
        let contents = contents.as_ref();
        self.write(relative, contents);
        self.write_workspace(relative, contents);
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn fake_tool(directory: &Path, name: &str, version: &str, log: &Path, body: &str) -> PathBuf {
    fs::create_dir_all(directory).unwrap();
    let path = directory.join(name);
    let version_argument = if name == "go" { "version" } else { "--version" };
    let body = if name == "uv" {
        format!("if [ \"$1\" = venv ]; then mkdir -p .venv; exit 0; fi\n{body}")
    } else if name == "go" {
        // Native Go graph parsing has its own host acceptance. These controlled
        // cache/mutation fixtures have no local use/replace declarations.
        format!(
            "if [ \"$2\" = edit ] && [ \"$3\" = -json ]; then printf '{{}}'; exit 0; fi\n{body}"
        )
    } else {
        body.to_owned()
    };
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"{version_argument}\" ]; then\n  echo {version}\n  exit 0\nfi\nprintf '%s\\n' \"$PWD|$*|NPM_OFFLINE=${{NPM_CONFIG_OFFLINE:-}}|GOPROXY=${{GOPROXY:-}}\" >> {}\n{}\n",
        shell_quote(log),
        body
    );
    fs::write(&path, script).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn executable_script(path: &Path, source: impl AsRef<[u8]>) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, source).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
    path.to_path_buf()
}

fn dependency_staging_is_empty(layout: &Layout) -> bool {
    let staging = layout.cache.join("dependencies/staging");
    !staging.exists() || fs::read_dir(staging).unwrap().next().is_none()
}

fn synthetic_artifact(
    cache_root: &Path,
    provider: &str,
    fingerprint: &str,
    payload_bytes: usize,
    modified_seconds: u64,
    owned: bool,
) -> (PathBuf, u64) {
    let artifact = cache_root
        .join("dependencies/artifacts")
        .join(provider)
        .join(fingerprint);
    fs::create_dir_all(artifact.join("payload")).unwrap();
    fs::write(
        artifact.join("payload/layer.bin"),
        vec![b'x'; payload_bytes],
    )
    .unwrap();
    if owned {
        fs::write(
            artifact.join(".shade-dependency-artifact"),
            b"shade-dependency-artifact/v1\n",
        )
        .unwrap();
    }
    let receipt = serde_json::json!({
        "schema_version": 1,
        "provider": provider,
        "fingerprint": fingerprint,
        "state": "ready",
        "materialized_paths": [],
        "blocked_builds": [],
        "approvals": [],
        "layer_digests": {},
        "tools": [{
            "name": provider,
            "path_identity": provider,
            "version": "fixture 1.0.0",
            "digest": "0".repeat(64)
        }],
        "platform": "fixture-platform"
    });
    let receipt_path = artifact.join("receipt.json");
    fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    let receipt_file = OpenOptions::new().write(true).open(&receipt_path).unwrap();
    receipt_file
        .set_times(
            FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(modified_seconds)),
        )
        .unwrap();
    let bytes = walkdir::WalkDir::new(&artifact)
        .follow_links(false)
        .into_iter()
        .map(Result::unwrap)
        .map(|entry| fs::symlink_metadata(entry.path()).unwrap())
        .filter(|metadata| metadata.is_file() || metadata.file_type().is_symlink())
        .map(|metadata| metadata.len())
        .sum();
    (artifact, bytes)
}

fn npm_project(layout: &Layout) {
    layout.write(
        "package.json",
        br#"{"name":"fixture","private":true,"scripts":{"preinstall":"exit 99","postinstall":"exit 99"}}"#,
    );
    layout.write(
        "package-lock.json",
        br#"{
          "name":"fixture",
          "lockfileVersion":3,
          "packages":{
            "":{},
            "node_modules/left-pad":{
              "version":"1.3.0",
              "resolved":"https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
              "integrity":"sha512-approved"
            }
          }
        }"#,
    );
}

#[tokio::test]
async fn npm_uses_safe_fill_and_offline_replay_and_caches_only_the_forest() {
    let layout = Layout::new();
    npm_project(&layout);
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("npm.log");
    let lifecycle_sentinel = tools.path().join("lifecycle-script-ran");
    let npm_body = format!(
        "case \" $* \" in *\" --ignore-scripts \"*) ;; *) : > {} ;; esac\nmkdir -p node_modules/left-pad\nprintf 'module.exports = 1\\n' > node_modules/left-pad/index.js",
        shell_quote(&lifecycle_sentinel)
    );
    let npm = fake_tool(tools.path(), "npm", "11.6.0", &log, &npm_body);
    let provider = NpmProvider::with_executable(npm);

    let receipt = provider.ensure_ready(&layout.context()).await.unwrap();
    assert_eq!(receipt.state, "ready");
    assert_eq!(receipt.materialized_paths, ["node_modules"]);
    assert!(
        layout
            .workspace
            .join("node_modules/left-pad/index.js")
            .is_file()
    );
    let log = fs::read_to_string(&log).unwrap();
    let installs = log.lines().collect::<Vec<_>>();
    assert_eq!(installs.len(), 2, "one fill and one replay are required");
    assert!(installs[0].contains("ci --ignore-scripts --no-audit --no-fund"));
    assert!(!installs[0].contains("--offline"));
    assert!(installs[1].contains("--offline"));
    assert!(installs[1].contains("NPM_OFFLINE=true"));
    assert!(!log.contains(" run "));
    assert!(
        !lifecycle_sentinel.exists(),
        "root/dependency lifecycle scripts must stay disabled"
    );

    let artifact = artifact_dir(&layout.context(), "npm", &receipt.fingerprint);
    assert!(
        artifact
            .join("payload/node_modules/left-pad/index.js")
            .is_file()
    );
    assert!(!artifact.join("payload/package.json").exists());

    provider.ensure_ready(&layout.context()).await.unwrap();
    assert_eq!(
        fs::read_to_string(tools.path().join("npm.log"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[tokio::test]
async fn owned_artifact_without_receipt_is_removed_and_rebuilt() {
    let layout = Layout::new();
    npm_project(&layout);
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("npm.log");
    let npm = fake_tool(
        tools.path(),
        "npm",
        "11.6.0",
        &log,
        "mkdir -p node_modules/left-pad\nprintf rebuilt > node_modules/left-pad/index.js",
    );
    let provider = NpmProvider::with_executable(npm);

    let receipt = provider.ensure_ready(&layout.context()).await.unwrap();
    let artifact = artifact_dir(&layout.context(), "npm", &receipt.fingerprint);
    fs::remove_file(artifact.join("receipt.json")).unwrap();
    fs::write(artifact.join("stale-entry"), b"must be quarantined").unwrap();

    let rebuilt = provider.ensure_ready(&layout.context()).await.unwrap();
    assert_eq!(rebuilt.fingerprint, receipt.fingerprint);
    assert!(artifact.join("receipt.json").is_file());
    assert!(!artifact.join("stale-entry").exists());
    assert_eq!(
        fs::read_to_string(log).unwrap().lines().count(),
        4,
        "a missing receipt must force a fresh online fill and offline replay"
    );
}

#[tokio::test]
async fn bun_and_pnpm_use_frozen_scriptless_fill_and_offline_replay() {
    let tools = tempfile::tempdir().unwrap();

    let bun_layout = Layout::new();
    bun_layout.write("package.json", br#"{"name":"fixture","private":true}"#);
    bun_layout.write(
        "bun.lock",
        "{\n  \"lockfileVersion\": 1,\n  \"workspaces\": {},\n  \"packages\": {}\n}\n",
    );
    let bun_log = tools.path().join("bun.log");
    let bun = fake_tool(
        tools.path(),
        "bun",
        "1.3.0",
        &bun_log,
        "mkdir -p node_modules",
    );
    BunProvider::with_executable(bun)
        .ensure_ready(&bun_layout.context())
        .await
        .unwrap();
    let bun_log = fs::read_to_string(bun_log).unwrap();
    let bun_runs = bun_log.lines().collect::<Vec<_>>();
    assert_eq!(bun_runs.len(), 2);
    assert!(bun_runs[0].contains("install --frozen-lockfile --ignore-scripts"));
    assert!(!bun_runs[0].contains("--offline"));
    assert!(
        !bun_runs[1].contains("--offline"),
        "Bun uses OS network isolation; its CLI has no offline flag"
    );

    let pnpm_layout = Layout::new();
    pnpm_layout.write("package.json", br#"{"name":"fixture","private":true}"#);
    pnpm_layout.write(
        "pnpm-lock.yaml",
        "lockfileVersion: '9.0'\nimporters:\n  .: {}\npackages:\n",
    );
    let pnpm_log = tools.path().join("pnpm.log");
    let pnpm = fake_tool(
        tools.path(),
        "pnpm",
        "10.15.0",
        &pnpm_log,
        "mkdir -p node_modules",
    );
    PnpmProvider::with_executable(pnpm)
        .ensure_ready(&pnpm_layout.context())
        .await
        .unwrap();
    let pnpm_log = fs::read_to_string(pnpm_log).unwrap();
    let pnpm_runs = pnpm_log.lines().collect::<Vec<_>>();
    assert_eq!(pnpm_runs.len(), 2);
    assert!(pnpm_runs[0].contains("install --frozen-lockfile --ignore-scripts"));
    assert!(pnpm_runs[0].contains("--store-dir"));
    assert!(!pnpm_runs[0].contains("--offline"));
    assert!(pnpm_runs[1].contains("--offline"));
}

#[tokio::test]
async fn fingerprint_is_stable_across_absolute_roots_and_receipt_leaks_no_path() {
    let first = Layout::new();
    let second = Layout::new();
    npm_project(&first);
    npm_project(&second);
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("npm.log");
    let npm = fake_tool(
        tools.path(),
        "npm",
        "11.6.0",
        &log,
        "mkdir -p node_modules/left-pad\ntouch node_modules/left-pad/index.js",
    );
    let provider = NpmProvider::with_executable(npm);
    let first_receipt = provider.ensure_ready(&first.context()).await.unwrap();
    let second_receipt = provider.ensure_ready(&second.context()).await.unwrap();
    assert_eq!(first_receipt.fingerprint, second_receipt.fingerprint);
    let bytes = fs::read_to_string(
        artifact_dir(&first.context(), "npm", &first_receipt.fingerprint).join("receipt.json"),
    )
    .unwrap();
    assert!(!bytes.contains(&first.repository.to_string_lossy().to_string()));
    assert!(!bytes.contains(&first.workspace.to_string_lossy().to_string()));
    assert!(!bytes.contains(&first.cache.to_string_lossy().to_string()));
    assert!(!bytes.contains(&first.runtime.to_string_lossy().to_string()));
}

#[tokio::test]
async fn same_fingerprint_is_single_flight() {
    let layout = Layout::new();
    npm_project(&layout);
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("npm.log");
    let npm = fake_tool(
        tools.path(),
        "npm",
        "11.6.0",
        &log,
        "mkdir -p node_modules/left-pad\ntouch node_modules/left-pad/index.js",
    );
    let first = NpmProvider::with_executable(&npm);
    let second = first.clone();
    let first_context = layout.context();
    let second_context = layout.context();
    let (left, right) = tokio::join!(
        first.ensure_ready(&first_context),
        second.ensure_ready(&second_context)
    );
    assert_eq!(left.unwrap().fingerprint, right.unwrap().fingerprint);
    assert_eq!(fs::read_to_string(log).unwrap().lines().count(), 2);
}

#[tokio::test]
async fn workspace_fanout_propagates_cow_unavailable_without_copy_fallback() {
    let layout = Layout::new();
    npm_project(&layout);
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("npm.log");
    let npm = fake_tool(
        tools.path(),
        "npm",
        "11.6.0",
        &log,
        "mkdir -p node_modules/left-pad\ntouch node_modules/left-pad/index.js",
    );
    let provider = NpmProvider::with_executable(npm);
    provider.ensure_ready(&layout.context()).await.unwrap();

    let failing = FailingCowFilesystem;
    let context = DependencyContext::with_filesystem(
        &layout.repository,
        &layout.workspace,
        &layout.cache,
        &layout.runtime,
        &failing,
    );
    let error = provider.ensure_ready(&context).await.unwrap_err();
    assert!(matches!(error, DependencyError::CowUnavailable { .. }));
    assert!(error.to_string().starts_with("COW_UNAVAILABLE:"));
}

#[tokio::test]
async fn secret_config_never_changes_a_fingerprint_and_secret_locks_are_rejected() {
    let first = Layout::new();
    let second = Layout::new();
    npm_project(&first);
    npm_project(&second);
    first.write(
        ".npmrc",
        "registry=https://registry.npmjs.org/\n//registry.npmjs.org/:_authToken=first-secret\n",
    );
    second.write(
        ".npmrc",
        "//registry.npmjs.org/:_authToken=second-secret\nregistry=https://registry.npmjs.org/\n",
    );
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("npm.log");
    let npm = fake_tool(
        tools.path(),
        "npm",
        "11.6.0",
        &log,
        "mkdir -p node_modules/left-pad\ntouch node_modules/left-pad/index.js",
    );
    let provider = NpmProvider::with_executable(npm);
    let left = provider.ensure_ready(&first.context()).await.unwrap();
    let right = provider.ensure_ready(&second.context()).await.unwrap();
    assert_eq!(left.fingerprint, right.fingerprint);

    let unsafe_lock = Layout::new();
    npm_project(&unsafe_lock);
    unsafe_lock.write(
        "package-lock.json",
        br#"{"lockfileVersion":3,"packages":{"":{},"node_modules/bad":{"version":"1.0.0","resolved":"https://token:secret@example.test/bad.tgz","integrity":"sha512-x"}}}"#,
    );
    let error = provider
        .ensure_ready(&unsafe_lock.context())
        .await
        .unwrap_err();
    assert!(matches!(error, DependencyError::UnsafeConfiguration { .. }));
}

#[tokio::test]
async fn missing_lock_and_executable_pnpmfile_are_actionable_rejections() {
    let missing = Layout::new();
    missing.write("package.json", br#"{"name":"fixture"}"#);
    let error = NpmProvider::new()
        .ensure_ready(&missing.context())
        .await
        .unwrap_err();
    assert!(matches!(error, DependencyError::LockMissing(_)));

    let unsafe_project = Layout::new();
    unsafe_project.write("package.json", br#"{"name":"fixture"}"#);
    unsafe_project.write("pnpm-lock.yaml", "lockfileVersion: '9.0'\npackages:\n");
    unsafe_project.write(".pnpmfile.cjs", "module.exports = {}\n");
    let error = PnpmProvider::new()
        .ensure_ready(&unsafe_project.context())
        .await
        .unwrap_err();
    assert!(matches!(error, DependencyError::UnsafeConfiguration { .. }));
    assert!(error.to_string().contains("pnpmfile"));
}

#[tokio::test]
async fn uv_is_wheel_only_and_replays_offline_into_a_relocatable_venv() {
    let layout = Layout::new();
    let tools = tempfile::tempdir().unwrap();
    let build_sentinel = tools.path().join("pep517-backend-ran");
    layout.write(
        "pyproject.toml",
        "[build-system]\nrequires = []\nbuild-backend = \"malicious_backend\"\nbackend-path = [\".\"]\n\n[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );
    layout.write(
        "malicious_backend.py",
        format!(
            "from pathlib import Path\nPath({:?}).write_text('executed')\n",
            build_sentinel
        ),
    );
    layout.write(
        "uv.lock",
        "version = 1\nrevision = 1\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\nsource = { editable = \".\" }\n",
    );
    layout.write(".python-version", "3.13.1\n");
    let uv_log = tools.path().join("uv.log");
    let python_log = tools.path().join("python.log");
    let python = fake_tool(
        tools.path(),
        "python",
        "Python 3.13.1",
        &python_log,
        "exit 0",
    );
    let uv_body = format!(
        "case \" $* \" in *\" --no-build \"*) ;; *) : > {} ;; esac\ncase \" $* \" in *\" --no-install-project \"*) ;; *) : > {} ;; esac\nmkdir -p .venv/bin .venv/lib/python3.13/site-packages\nprintf \"home = {}\\ncommand = $PWD/.venv/bin/python\\n\" > .venv/pyvenv.cfg\nln -s {} .venv/bin/python",
        shell_quote(&build_sentinel),
        shell_quote(&build_sentinel),
        tools.path().display(),
        python.display()
    );
    let uv = fake_tool(tools.path(), "uv", "uv 0.12.6", &uv_log, &uv_body);
    let provider = UvProvider::with_tools(uv, &python);

    let receipt = provider.ensure_ready(&layout.context()).await.unwrap();
    assert_eq!(receipt.materialized_paths, [".venv"]);
    let log = fs::read_to_string(uv_log).unwrap();
    let runs = log
        .lines()
        .filter(|line| line.contains("|sync "))
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 2);
    for run in &runs {
        assert!(run.contains("sync --frozen --no-install-project --no-install-workspace"));
        assert!(run.contains("--no-editable --no-build --no-python-downloads"));
    }
    assert!(
        !build_sentinel.exists(),
        "PEP 517 backend code must not execute"
    );
    assert!(!runs[0].contains("--offline"));
    assert!(runs[1].contains("--offline"));
    assert_eq!(
        fs::read_link(layout.workspace.join(".venv/bin/python")).unwrap(),
        fs::canonicalize(python).unwrap()
    );
    let pyvenv = fs::read_to_string(layout.workspace.join(".venv/pyvenv.cfg")).unwrap();
    assert!(pyvenv.contains(&layout.workspace.join(".venv").to_string_lossy().to_string()));
    let cached = fs::read_to_string(
        artifact_dir(&layout.context(), "uv", &receipt.fingerprint)
            .join("payload/.venv/pyvenv.cfg"),
    )
    .unwrap();
    assert!(cached.contains("__SHADE_VENV__"));
    assert!(!cached.contains(&layout.cache.to_string_lossy().to_string()));
}

#[tokio::test]
async fn uv_rejects_sdist_and_local_sources_before_running_a_tool() {
    let layout = Layout::new();
    layout.write(
        "pyproject.toml",
        "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );
    layout.write(
        "uv.lock",
        "version = 1\n[[package]]\nname = \"bad\"\nversion = \"1.0.0\"\nsource = { registry = \"https://pypi.org/simple\" }\nsdist = { url = \"https://example/bad.tar.gz\", hash = \"sha256:bad\" }\n",
    );
    let error = UvProvider::new()
        .ensure_ready(&layout.context())
        .await
        .unwrap_err();
    assert!(matches!(error, DependencyError::InvalidLock { .. }));
    assert!(error.to_string().contains("sdist"));

    layout.write(
        "uv.lock",
        "version = 1\n[[package]]\nname = \"bad\"\nversion = \"1.0.0\"\nsource = { git = \"https://example/repo\" }\n",
    );
    let error = UvProvider::new()
        .ensure_ready(&layout.context())
        .await
        .unwrap_err();
    assert!(matches!(error, DependencyError::InvalidLock { .. }));
    assert!(error.to_string().contains("VCS"));
}

#[tokio::test]
async fn uv_rejects_python_startup_code_before_layer_promotion() {
    let layout = Layout::new();
    layout.write(
        "pyproject.toml",
        "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );
    layout.write(
        "uv.lock",
        "version = 1\nrevision = 1\n[[package]]\nname = \"fixture\"\nversion = \"0.1.0\"\nsource = { editable = \".\" }\n",
    );
    layout.write(".python-version", "3.13.1\n");
    let tools = tempfile::tempdir().unwrap();
    let python = fake_tool(
        tools.path(),
        "python",
        "Python 3.13.1",
        &tools.path().join("python.log"),
        "exit 0",
    );
    let uv = fake_tool(
        tools.path(),
        "uv",
        "uv 0.12.6",
        &tools.path().join("uv.log"),
        "mkdir -p .venv/lib/python3.13/site-packages; printf 'import malware\n' > .venv/lib/python3.13/site-packages/malicious.pth",
    );
    let error = UvProvider::with_tools(uv, python)
        .ensure_ready(&layout.context())
        .await
        .unwrap_err();
    assert!(matches!(error, DependencyError::Validation { .. }));
    assert!(error.to_string().contains("startup code"));
    assert!(!layout.workspace.join(".venv").exists());
}

#[tokio::test]
async fn cargo_only_fetches_then_replays_frozen_and_offline() {
    let layout = Layout::new();
    let tools = tempfile::tempdir().unwrap();
    let code_sentinel = tools.path().join("cargo-code-ran");
    layout.write(
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nbuild = \"build.rs\"\n\n[dependencies]\nfixture-macro = { path = \"fixture-macro\" }\n",
    );
    let sentinel_literal = format!("{:?}", code_sentinel.to_string_lossy());
    layout.write(
        "build.rs",
        format!(
            "fn main() {{ std::fs::write({sentinel_literal}, b\"build script executed\").unwrap(); }}\n"
        ),
    );
    layout.write(
        "fixture-macro/Cargo.toml",
        "[package]\nname = \"fixture-macro\"\nversion = \"0.1.0\"\n\n[lib]\nproc-macro = true\n",
    );
    layout.write(
        "fixture-macro/src/lib.rs",
        format!(
            "extern crate proc_macro;\n#[proc_macro]\npub fn sentinel(input: proc_macro::TokenStream) -> proc_macro::TokenStream {{ std::fs::write({sentinel_literal}, b\"proc macro executed\").unwrap(); input }}\n"
        ),
    );
    layout.write("Cargo.lock", "version = 4\n");
    let log = tools.path().join("cargo.log");
    let cargo_body = format!(
        "case \"$1\" in fetch) ;; *) : > {} ;; esac\nexit 0",
        shell_quote(&code_sentinel)
    );
    let cargo = fake_tool(tools.path(), "cargo", "cargo 1.93.0", &log, &cargo_body);
    let provider = CargoProvider::with_executable(cargo);
    let receipt = provider.ensure_ready(&layout.context()).await.unwrap();
    assert!(receipt.materialized_paths.is_empty());
    provider.ensure_ready(&layout.context()).await.unwrap();
    let log = fs::read_to_string(log).unwrap();
    let runs = log.lines().collect::<Vec<_>>();
    assert_eq!(runs.len(), 3);
    assert!(runs[0].contains("fetch --locked --manifest-path"));
    assert!(!runs[0].contains("--offline"));
    assert!(runs[1].contains("fetch --locked --frozen --offline --manifest-path"));
    assert!(runs[2].contains("fetch --locked --frozen --offline --manifest-path"));
    for forbidden in [" build", " check", " test"] {
        assert!(!log.contains(forbidden));
    }
    assert!(
        !code_sentinel.exists(),
        "build.rs and proc macros must not execute during readiness"
    );
}

#[tokio::test]
async fn cargo_refills_after_a_cached_offline_replay_failure() {
    let layout = Layout::new();
    layout.write(
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );
    layout.write("Cargo.lock", "version = 4\n");
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("cargo.log");
    let fail_next = tools.path().join("fail-next-offline");
    let consumed = tools.path().join("offline-failure-consumed");
    let body = format!(
        "if [ -f {} ] && [ ! -f {} ]; then\n  case \" $* \" in *\" --offline \"*) : > {}; exit 71 ;; esac\nfi\nexit 0",
        shell_quote(&fail_next),
        shell_quote(&consumed),
        shell_quote(&consumed)
    );
    let cargo = fake_tool(tools.path(), "cargo", "cargo 1.93.0", &log, &body);
    let provider = CargoProvider::with_executable(cargo);

    let first = provider.ensure_ready(&layout.context()).await.unwrap();
    fs::write(&fail_next, b"fail once").unwrap();
    let second = provider.ensure_ready(&layout.context()).await.unwrap();

    assert_eq!(first.fingerprint, second.fingerprint);
    assert!(consumed.is_file());
    assert!(
        artifact_dir(&layout.context(), "cargo", &first.fingerprint)
            .join("receipt.json")
            .is_file()
    );
    let log = fs::read_to_string(log).unwrap();
    let runs = log.lines().collect::<Vec<_>>();
    assert_eq!(runs.len(), 5);
    assert!(
        runs[2].contains("--offline"),
        "cached replay must fail first"
    );
    assert!(
        !runs[3].contains("--offline"),
        "a safe online refill must follow"
    );
    assert!(
        runs[4].contains("--offline"),
        "refill must be verified offline"
    );
}

#[tokio::test]
async fn cargo_fingerprints_the_repository_selected_cargo_and_rustc_without_paths() {
    let layout = Layout::new();
    layout.write(
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );
    layout.write("Cargo.lock", "version = 4\n");
    layout.write(
        "rust-toolchain.toml",
        "[toolchain]\nchannel = \"fake-a\"\nprofile = \"minimal\"\n",
    );

    let tools = tempfile::tempdir().unwrap();
    let cargo_a_log = tools.path().join("cargo-a.log");
    let cargo_b_log = tools.path().join("cargo-b.log");
    let rustc_log = tools.path().join("rustc.log");
    let cargo_a = fake_tool(
        tools.path(),
        "effective-cargo-a",
        "cargo 1.80.0",
        &cargo_a_log,
        "exit 0",
    );
    let rustc_a = fake_tool(
        tools.path(),
        "effective-rustc-a",
        "rustc 1.80.0",
        &rustc_log,
        "exit 0",
    );
    let cargo_b = fake_tool(
        tools.path(),
        "effective-cargo-b",
        "cargo 1.81.0",
        &cargo_b_log,
        "exit 0",
    );
    let rustc_b = fake_tool(
        tools.path(),
        "effective-rustc-b",
        "rustc 1.81.0",
        &rustc_log,
        "exit 0",
    );
    let rustup_log = tools.path().join("rustup.log");
    let rustup = tools.path().join("rustup");
    executable_script(
        &rustup,
        format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$PWD\" \"$*\" >> {log}\n[ \"$1\" = which ] || exit 64\nif grep -q fake-b \"$PWD/rust-toolchain.toml\" 2>/dev/null || grep -q fake-b \"$PWD/rust-toolchain\" 2>/dev/null; then\n  cargo={cargo_b}\n  rustc={rustc_b}\nelse\n  cargo={cargo_a}\n  rustc={rustc_a}\nfi\ncase \"$2\" in\n  cargo) printf '%s\\n' \"$cargo\" ;;\n  rustc) printf '%s\\n' \"$rustc\" ;;\n  *) exit 65 ;;\nesac\n",
            log = shell_quote(&rustup_log),
            cargo_a = shell_quote(&cargo_a),
            rustc_a = shell_quote(&rustc_a),
            cargo_b = shell_quote(&cargo_b),
            rustc_b = shell_quote(&rustc_b),
        ),
    );
    let cargo_proxy = tools.path().join("cargo");
    let rustc_proxy = tools.path().join("rustc");
    fs::hard_link(&rustup, &cargo_proxy).unwrap();
    symlink(&rustup, &rustc_proxy).unwrap();
    let provider = CargoProvider::with_tools(cargo_proxy, rustc_proxy);

    let first = provider.ensure_ready(&layout.context()).await.unwrap();

    // The selected Cargo binary is part of the fingerprint independently of
    // the repository inputs and rustc.
    fake_tool(
        tools.path(),
        "effective-cargo-a",
        "cargo 1.80.1",
        &cargo_a_log,
        "exit 0",
    );
    let cargo_changed = provider.ensure_ready(&layout.context()).await.unwrap();
    assert_ne!(first.fingerprint, cargo_changed.fingerprint);

    layout.write(
        "rust-toolchain.toml",
        "[toolchain]\nchannel = \"fake-b\"\nprofile = \"minimal\"\n",
    );
    let second = provider.ensure_ready(&layout.context()).await.unwrap();
    assert_ne!(first.fingerprint, second.fingerprint);

    // Repository toolchain inputs remain fingerprint inputs even if they
    // resolve to byte-identical effective tools.
    layout.write(
        "rust-toolchain.toml",
        "[toolchain]\nchannel = \"fake-b\"\nprofile = \"default\"\n",
    );
    let toolchain_input_changed = provider.ensure_ready(&layout.context()).await.unwrap();
    assert_ne!(second.fingerprint, toolchain_input_changed.fingerprint);

    // With the toolchain file unchanged, changing only effective rustc must
    // still invalidate readiness.
    fake_tool(
        tools.path(),
        "effective-rustc-b",
        "rustc 1.81.1",
        &rustc_log,
        "exit 0",
    );
    let third = provider.ensure_ready(&layout.context()).await.unwrap();
    assert_ne!(toolchain_input_changed.fingerprint, third.fingerprint);

    // Both rust-toolchain spellings are first-class inputs. The fake selector
    // deliberately resolves this plain form to the same effective binaries.
    fs::remove_file(layout.repository.join("rust-toolchain.toml")).unwrap();
    layout.write("rust-toolchain", "fake-b\n");
    let plain_toolchain = provider.ensure_ready(&layout.context()).await.unwrap();
    assert_ne!(third.fingerprint, plain_toolchain.fingerprint);

    let rustup_runs = fs::read_to_string(&rustup_log).unwrap();
    let canonical_repository = fs::canonicalize(&layout.repository).unwrap();
    assert_eq!(rustup_runs.lines().count(), 12);
    assert!(
        rustup_runs
            .lines()
            .all(|line| line.starts_with(&format!("{}|which ", canonical_repository.display())))
    );
    assert_eq!(fs::read_to_string(cargo_a_log).unwrap().lines().count(), 4);
    assert_eq!(fs::read_to_string(cargo_b_log).unwrap().lines().count(), 8);

    for receipt in [
        first,
        cargo_changed,
        second,
        toolchain_input_changed,
        third,
        plain_toolchain,
    ] {
        let serialized = fs::read_to_string(
            artifact_dir(&layout.context(), "cargo", &receipt.fingerprint).join("receipt.json"),
        )
        .unwrap();
        assert!(!serialized.contains(layout.repository.to_string_lossy().as_ref()));
        assert!(!serialized.contains(tools.path().to_string_lossy().as_ref()));
        assert!(!serialized.contains(canonical_repository.to_string_lossy().as_ref()));
        assert!(
            !serialized.contains(
                fs::canonicalize(tools.path())
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            )
        );
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        let recorded_tools = value["tools"].as_array().unwrap();
        assert_eq!(recorded_tools.len(), 2);
        assert_eq!(recorded_tools[0]["name"], "cargo");
        assert_eq!(recorded_tools[1]["name"], "rustc");
        for tool in recorded_tools {
            let name = tool["name"].as_str().unwrap();
            let version = tool["version"].as_str().unwrap();
            let digest = tool["digest"].as_str().unwrap();
            let path_identity = tool["path_identity"].as_str().unwrap();
            assert!(version.starts_with(name));
            assert_eq!(digest.len(), 64);
            assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_eq!(path_identity, format!("{name}@sha256:{digest}"));
        }
    }
}

#[tokio::test]
async fn cargo_rejects_executable_config() {
    let layout = Layout::new();
    layout.write("Cargo.toml", "[workspace]\nresolver = \"3\"\n");
    layout.write("Cargo.lock", "version = 4\n");
    layout.write(
        ".cargo/config.toml",
        "[build]\nrustc-wrapper = \"./wrapper.sh\"\n",
    );
    let error = CargoProvider::new()
        .ensure_ready(&layout.context())
        .await
        .unwrap_err();
    assert!(matches!(error, DependencyError::UnsafeConfiguration { .. }));
    assert!(error.to_string().contains("rustc-wrapper"));
}

#[tokio::test]
async fn go_downloads_and_verifies_then_replays_with_network_off() {
    let layout = Layout::new();
    let go_mod = "module example.test/fixture\n\ngo 1.25\n";
    let go_sum = "example.test/dependency v1.0.0 h1:approved\n";
    layout.write_both("go.mod", go_mod);
    layout.write_both("go.sum", go_sum);
    let tools = tempfile::tempdir().unwrap();
    let code_sentinel = tools.path().join("go-code-ran");
    layout.write_both(
        "main.go",
        format!(
            "package main\nimport \"os\"\nfunc init() {{ _ = os.WriteFile({:?}, []byte(\"executed\"), 0o600) }}\nfunc main() {{}}\n",
            code_sentinel.to_string_lossy()
        ),
    );
    let log = tools.path().join("go.log");
    let go_body = format!(
        "case \" $* \" in \" mod download all \"|\" mod verify \") ;; *) : > {} ;; esac\nexit 0",
        shell_quote(&code_sentinel)
    );
    let go = fake_tool(
        tools.path(),
        "go",
        "go version go1.25.0 test/arch",
        &log,
        &go_body,
    );
    let provider = GoProvider::with_executable(go);
    let receipt = provider.ensure_ready(&layout.context()).await.unwrap();
    assert!(receipt.materialized_paths.is_empty());
    provider.ensure_ready(&layout.context()).await.unwrap();
    let log = fs::read_to_string(log).unwrap();
    let runs = log
        .lines()
        .filter(|line| !line.contains(" edit -json "))
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 6);
    assert!(runs[0].contains("mod download all"));
    assert!(runs[1].contains("mod verify"));
    assert!(runs[2].contains("mod download all"));
    assert!(runs[2].contains("GOPROXY=off"));
    assert!(runs[3].contains("mod verify"));
    assert!(runs[3].contains("GOPROXY=off"));
    assert!(runs[4].contains("mod download all"));
    assert!(runs[4].contains("GOPROXY=off"));
    assert!(runs[5].contains("mod verify"));
    assert!(runs[5].contains("GOPROXY=off"));
    for forbidden in [" tidy", " build", " vendor", " sync", " generate"] {
        assert!(!log.contains(forbidden));
    }
    assert!(
        !code_sentinel.exists(),
        "Go package initialization must not execute during readiness"
    );
    let canonical_staging = fs::canonicalize(layout.cache.join("dependencies/staging")).unwrap();
    let canonical_repository = fs::canonicalize(&layout.repository).unwrap();
    let canonical_workspace = fs::canonicalize(&layout.workspace).unwrap();
    for run in runs {
        let cwd = run.split('|').next().unwrap();
        assert!(Path::new(cwd).starts_with(&canonical_staging));
        assert_ne!(cwd, canonical_repository.to_string_lossy().as_ref());
        assert_ne!(cwd, canonical_workspace.to_string_lossy().as_ref());
    }
    assert_eq!(
        fs::read_to_string(layout.repository.join("go.mod")).unwrap(),
        go_mod
    );
    assert_eq!(
        fs::read_to_string(layout.repository.join("go.sum")).unwrap(),
        go_sum
    );
    assert_eq!(
        fs::read_to_string(layout.workspace.join("go.mod")).unwrap(),
        go_mod
    );
    assert_eq!(
        fs::read_to_string(layout.workspace.join("go.sum")).unwrap(),
        go_sum
    );
    assert!(dependency_staging_is_empty(&layout));
}

#[tokio::test]
async fn go_refills_after_a_cached_offline_replay_failure() {
    let layout = Layout::new();
    layout.write_both("go.mod", "module example.test/fixture\n\ngo 1.25\n");
    layout.write_both("go.sum", "example.test/dependency v1.0.0 h1:approved\n");
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("go.log");
    let fail_next = tools.path().join("fail-next-offline");
    let consumed = tools.path().join("offline-failure-consumed");
    let body = format!(
        "if [ -f {} ] && [ ! -f {} ] && [ \"${{GOPROXY:-}}\" = off ]; then\n  : > {}; exit 72\nfi\nexit 0",
        shell_quote(&fail_next),
        shell_quote(&consumed),
        shell_quote(&consumed)
    );
    let go = fake_tool(
        tools.path(),
        "go",
        "go version go1.25.0 test/arch",
        &log,
        &body,
    );
    let provider = GoProvider::with_executable(go);

    let first = provider.ensure_ready(&layout.context()).await.unwrap();
    fs::write(&fail_next, b"fail once").unwrap();
    let second = provider.ensure_ready(&layout.context()).await.unwrap();

    assert_eq!(first.fingerprint, second.fingerprint);
    assert!(consumed.is_file());
    assert!(
        artifact_dir(&layout.context(), "go", &first.fingerprint)
            .join("receipt.json")
            .is_file()
    );
    let log = fs::read_to_string(log).unwrap();
    let runs = log
        .lines()
        .filter(|line| !line.contains(" edit -json "))
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 9);
    assert!(runs[4].contains("GOPROXY=off"));
    assert!(!runs[5].contains("GOPROXY=off"));
    assert!(!runs[6].contains("GOPROXY=off"));
    assert!(runs[7].contains("GOPROXY=off"));
    assert!(runs[8].contains("GOPROXY=off"));
    assert!(dependency_staging_is_empty(&layout));
}

#[tokio::test]
async fn go_rejects_probe_graph_mutation_preserves_originals_and_cleans_staging() {
    let layout = Layout::new();
    let go_mod = "module example.test/fixture\n\ngo 1.25\n";
    let go_sum = "example.test/dependency v1.0.0 h1:approved\n";
    layout.write_both("go.mod", go_mod);
    layout.write_both("go.sum", go_sum);
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("go.log");
    let go = fake_tool(
        tools.path(),
        "go",
        "go version go1.25.0 test/arch",
        &log,
        "printf '\\n// illicit probe mutation\\n' >> go.mod\nexit 73",
    );

    let error = GoProvider::with_executable(go)
        .ensure_ready(&layout.context())
        .await
        .unwrap_err();

    assert!(matches!(error, DependencyError::LockStale(_)));
    assert!(error.to_string().contains("Go online fill"));
    assert_eq!(
        fs::read_to_string(layout.repository.join("go.mod")).unwrap(),
        go_mod
    );
    assert_eq!(
        fs::read_to_string(layout.repository.join("go.sum")).unwrap(),
        go_sum
    );
    assert_eq!(
        fs::read_to_string(layout.workspace.join("go.mod")).unwrap(),
        go_mod
    );
    assert_eq!(
        fs::read_to_string(layout.workspace.join("go.sum")).unwrap(),
        go_sum
    );
    let run = fs::read_to_string(log).unwrap();
    assert!(run.contains("dependencies/staging/go-probe-"));
    assert!(!run.contains(&format!("{}|mod", layout.repository.display())));
    assert!(!run.contains(&format!("{}|mod", layout.workspace.display())));
    assert!(dependency_staging_is_empty(&layout));
}

#[tokio::test]
async fn go_requires_a_cow_probe_before_invoking_the_tool() {
    let layout = Layout::new();
    let go_mod = "module example.test/fixture\n\ngo 1.25\n";
    let go_sum = "example.test/dependency v1.0.0 h1:approved\n";
    layout.write_both("go.mod", go_mod);
    layout.write_both("go.sum", go_sum);
    let tools = tempfile::tempdir().unwrap();
    let log = tools.path().join("go.log");
    let invoked = tools.path().join("invoked");
    let go = fake_tool(
        tools.path(),
        "go",
        "go version go1.25.0 test/arch",
        &log,
        &format!(": > {}", shell_quote(&invoked)),
    );
    let failing = FailingCowFilesystem;
    let context = DependencyContext::with_filesystem(
        &layout.repository,
        &layout.workspace,
        &layout.cache,
        &layout.runtime,
        &failing,
    );

    let error = GoProvider::with_executable(go)
        .ensure_ready(&context)
        .await
        .unwrap_err();

    assert!(matches!(error, DependencyError::CowUnavailable { .. }));
    assert!(!invoked.exists());
    assert!(!log.exists());
    assert_eq!(
        fs::read_to_string(layout.repository.join("go.mod")).unwrap(),
        go_mod
    );
    assert_eq!(
        fs::read_to_string(layout.workspace.join("go.mod")).unwrap(),
        go_mod
    );
    assert!(dependency_staging_is_empty(&layout));
}

#[test]
fn default_provider_set_detects_multiple_ecosystems() {
    let layout = Layout::new();
    layout.write("package.json", br#"{"name":"fixture"}"#);
    layout.write(
        "package-lock.json",
        br#"{"lockfileVersion":3,"packages":{"":{}}}"#,
    );
    layout.write("pyproject.toml", "[project]\nname = \"fixture\"\n");
    layout.write("uv.lock", "version = 1\n");
    layout.write("Cargo.toml", "[workspace]\n");
    layout.write("Cargo.lock", "version = 4\n");
    layout.write("go.mod", "module example.test/fixture\n");
    layout.write("go.sum", "\n");
    let providers = default_providers();
    let detected = providers
        .iter()
        .filter(|provider| provider.applies(&layout.repository))
        .map(|provider| provider.name())
        .collect::<Vec<_>>();
    assert_eq!(detected, ["npm", "uv", "cargo", "go"]);
}

#[test]
fn nested_independent_dependency_roots_are_detected_without_splitting_workspace_members() {
    let layout = Layout::new();
    layout.write("pyproject.toml", "[project]\nname='root'\n");
    layout.write("uv.lock", "version = 1\n");
    layout.write(
        "packages/member/pyproject.toml",
        "[project]\nname='member'\n",
    );
    layout.write(
        "tools/independent/pyproject.toml",
        "[project]\nname='tool'\n",
    );
    layout.write("tools/independent/uv.lock", "version = 1\n");
    let roots = dependency_roots("uv", &layout.repository).unwrap();
    assert_eq!(
        roots,
        [
            layout.repository.clone(),
            layout.repository.join("tools/independent"),
        ]
    );

    layout.write("go.work", "go 1.25\nuse ./services/a\n");
    layout.write("services/a/go.mod", "module example.test/a\n");
    layout.write("standalone/go.mod", "module example.test/standalone\n");
    let roots = dependency_roots("go", &layout.repository).unwrap();
    assert!(roots.contains(&layout.repository));
    assert!(roots.contains(&layout.repository.join("standalone")));
    assert!(!roots.contains(&layout.repository.join("services/a")));
}

#[test]
fn dependency_gc_is_deterministic_lru_and_never_touches_native_or_unowned_data() {
    let layout = Layout::new();
    let oldest = "a".repeat(64);
    let protected = "b".repeat(64);
    let newest = "c".repeat(64);
    let native = "d".repeat(64);
    let unowned = "e".repeat(64);
    let (oldest_path, oldest_bytes) =
        synthetic_artifact(&layout.cache, "npm", &oldest, 128, 10, true);
    let (protected_path, protected_bytes) =
        synthetic_artifact(&layout.cache, "uv", &protected, 128, 20, true);
    let (newest_path, newest_bytes) =
        synthetic_artifact(&layout.cache, "pnpm", &newest, 128, 30, true);
    let (native_path, _) = synthetic_artifact(&layout.cache, "cargo", &native, 4_096, 1, true);
    let (unowned_path, _) = synthetic_artifact(&layout.cache, "npm", &unowned, 4_096, 0, false);
    let service = DependencyService::new(Vec::new());

    let report = service
        .garbage_collect(&layout.cache, [&protected], protected_bytes + newest_bytes)
        .unwrap();
    assert_eq!(
        report.before_bytes,
        oldest_bytes + protected_bytes + newest_bytes
    );
    assert_eq!(report.after_bytes, protected_bytes + newest_bytes);
    assert_eq!(report.protected_bytes, protected_bytes);
    assert_eq!(report.removed_bytes, oldest_bytes);
    assert_eq!(
        report.removed,
        [DependencyGcEntry {
            provider: "npm".to_owned(),
            fingerprint: oldest,
            bytes: oldest_bytes,
        }]
    );
    assert!(!oldest_path.exists());
    assert!(protected_path.exists());
    assert!(newest_path.exists());
    assert!(
        native_path.exists(),
        "Cargo receipts/native cache are outside layer GC"
    );
    assert!(
        unowned_path.exists(),
        "unowned cache entries must never be removed"
    );
    assert_eq!(report.skipped_unowned, 1);

    let report = service
        .garbage_collect(&layout.cache, [&protected], 0)
        .unwrap();
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.removed[0].fingerprint, newest);
    assert!(protected_path.exists());
    assert!(native_path.exists());
    assert!(unowned_path.exists());
}

#[test]
#[cfg(target_os = "macos")]
fn offline_replay_blocks_network_even_when_the_tool_ignores_offline_flags() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 1024];
        assert!(stream.read(&mut request).unwrap() > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
        listener
    });
    let temp = tempfile::tempdir().unwrap();
    let path = fake_tool(
        temp.path(),
        "network-probe",
        "1.0.0",
        &temp.path().join("probe.log"),
        &format!(
            "exec /usr/bin/curl --silent --show-error --noproxy '*' --connect-timeout 2 --max-time 3 http://{address}/"
        ),
    );
    let tool = super::common::identify_tool("network-probe", Some(&path)).unwrap();
    let online = super::common::run_tool(
        "probe",
        "online",
        &tool,
        temp.path(),
        ["run"],
        &[],
        super::common::ToolIsolation::network(false),
    )
    .unwrap();
    assert_eq!(online.stdout, b"ok");
    let listener = server.join().unwrap();
    let offline = super::common::run_tool(
        "probe",
        "offline",
        &tool,
        temp.path(),
        ["run"],
        &[],
        super::common::ToolIsolation::network(true),
    );
    assert!(matches!(
        offline,
        Err(DependencyError::CommandFailed { .. })
    ));
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "the sandboxed subprocess reached the registry"
    );
}
