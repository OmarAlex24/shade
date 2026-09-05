use super::common::{
    FingerprintBuilder, StoredReceipt, ToolIdentity, ToolIsolation, collect_named_files,
    create_staging, identify_tool_at, invalidate_receipt, load_receipt, native_cache,
    policy_fingerprint, promote, read_bytes, reject_embedded_secrets, relative_portable, run_tool,
    single_flight,
};
use super::{DependencyContext, DependencyError, DependencyProvider, DependencyReceipt};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct CargoProvider {
    cargo_executable: Option<PathBuf>,
    rustc_executable: Option<PathBuf>,
}

impl CargoProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_executable(executable: impl Into<PathBuf>) -> Self {
        Self {
            cargo_executable: Some(executable.into()),
            rustc_executable: None,
        }
    }

    pub fn with_tools(
        cargo_executable: impl Into<PathBuf>,
        rustc_executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            cargo_executable: Some(cargo_executable.into()),
            rustc_executable: Some(rustc_executable.into()),
        }
    }
}

#[async_trait]
impl DependencyProvider for CargoProvider {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn applies(&self, repository_root: &Path) -> bool {
        repository_root.join("Cargo.toml").is_file()
    }

    async fn ensure_ready(
        &self,
        context: &DependencyContext<'_>,
    ) -> Result<DependencyReceipt, DependencyError> {
        ensure_cargo(
            context,
            self.cargo_executable.as_deref(),
            self.rustc_executable.as_deref(),
        )
        .await
    }
}

#[derive(Debug, Clone, Default)]
pub struct GoProvider {
    executable: Option<PathBuf>,
}

impl GoProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_executable(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: Some(executable.into()),
        }
    }
}

#[async_trait]
impl DependencyProvider for GoProvider {
    fn name(&self) -> &'static str {
        "go"
    }

    fn applies(&self, repository_root: &Path) -> bool {
        repository_root.join("go.mod").is_file() || repository_root.join("go.work").is_file()
    }

    async fn ensure_ready(
        &self,
        context: &DependencyContext<'_>,
    ) -> Result<DependencyReceipt, DependencyError> {
        ensure_go(context, self.executable.as_deref()).await
    }
}

struct CargoPlan {
    inputs: Vec<PathBuf>,
    canonical_configs: Vec<(String, Vec<u8>)>,
}

impl CargoPlan {
    fn inspect(root: &Path) -> Result<Self, DependencyError> {
        for path in [root.join(".cargo"), root.join(".cargo/config.toml")] {
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
                return Err(DependencyError::UnsafeConfiguration {
                    provider: "cargo".into(),
                    path: relative_portable(root, &path)?,
                    reason: "Cargo configuration must be an inspected file inside the repository"
                        .into(),
                });
            }
        }
        let lock = root.join("Cargo.lock");
        if !lock.is_file() {
            return Err(DependencyError::LockMissing(
                "Cargo requires a committed Cargo.lock; run `cargo generate-lockfile` and commit it"
                    .to_owned(),
            ));
        }
        if root.join(".cargo/config").is_file() {
            return Err(DependencyError::InvalidConfiguration {
                provider: "cargo".to_owned(),
                path: ".cargo/config".to_owned(),
                reason: "executable Cargo configuration is not accepted; use reviewed config.toml"
                    .to_owned(),
            });
        }
        let mut inputs = collect_named_files(root, &["Cargo.toml"])?;
        inputs.push(lock);
        for name in ["rust-toolchain", "rust-toolchain.toml"] {
            let path = root.join(name);
            if path.is_file() {
                inputs.push(path);
            }
        }
        let mut canonical_configs = Vec::new();
        for path in collect_named_files(root, &["config.toml"])? {
            if path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == OsStr::new(".cargo"))
            {
                let bytes = read_bytes(&path, "Cargo config.toml")?;
                let canonical = validate_cargo_config(root, &path, &bytes)?;
                canonical_configs.push((relative_portable(root, &path)?, canonical));
                inputs.push(path);
            }
        }
        inputs.sort_by_key(|path| relative_portable(root, path).unwrap_or_default());
        inputs.dedup();
        for input in &inputs {
            let relative = relative_portable(root, input)?;
            reject_embedded_secrets("cargo", &relative, &read_bytes(input, &relative)?)?;
        }
        canonical_configs.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(Self {
            inputs,
            canonical_configs,
        })
    }

    fn fingerprint(
        &self,
        root: &Path,
        cargo: &ToolIdentity,
        rustc: &ToolIdentity,
    ) -> Result<String, DependencyError> {
        let mut fingerprint = FingerprintBuilder::new("cargo");
        fingerprint.tool(cargo);
        fingerprint.tool(rustc);
        let graph = self
            .inputs
            .iter()
            .map(|path| relative_portable(root, path))
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        fingerprint.field("graph", graph.as_bytes());
        for input in &self.inputs {
            if !self.canonical_configs.iter().any(|(relative, _)| {
                relative == &relative_portable(root, input).unwrap_or_default()
            }) {
                fingerprint.file(root, input)?;
            }
        }
        for (path, config) in &self.canonical_configs {
            fingerprint.field(&format!("config:{path}"), config);
        }
        fingerprint.field(
            "policy:cargo",
            b"fetch-only;locked;frozen-offline-replay;no-build;no-check;no-test;hidden-parent-native-config;pinned-rustc-v1",
        );
        policy_fingerprint(root, &mut fingerprint)?;
        Ok(fingerprint.finish())
    }
}

async fn ensure_cargo(
    context: &DependencyContext<'_>,
    cargo_executable: Option<&Path>,
    rustc_executable: Option<&Path>,
) -> Result<DependencyReceipt, DependencyError> {
    let plan = CargoPlan::inspect(context.repository_root)?;
    let cargo = identify_tool_at(
        "cargo",
        cargo_executable,
        context.repository_root,
        &rustup_environment(),
    )?;
    let rustc = identify_tool_at(
        "rustc",
        rustc_executable,
        context.repository_root,
        &rustup_environment(),
    )?;
    let fingerprint = plan.fingerprint(context.repository_root, &cargo, &rustc)?;
    let _flight = single_flight(format!("cargo:{fingerprint}")).await;
    if let Some(receipt) = load_receipt(context, "cargo", &fingerprint)? {
        let before = snapshot_files(context.repository_root, &plan.inputs)?;
        let cache = native_cache(context, "cargo")?;
        let replay = run_cargo(context.repository_root, &cargo, &rustc, &cache, true);
        assert_snapshot_unchanged(
            context.repository_root,
            &before,
            &plan.inputs,
            "cargo cached frozen/offline replay",
        )?;
        match replay {
            Ok(()) => {
                crate::faults::hit(crate::faults::Point::CargoCachedReplayValidated);
                return Ok(receipt.public());
            }
            Err(error) => {
                tracing::warn!(fingerprint, reason = %error, "refilling Cargo cache after offline replay failure");
                invalidate_receipt(context, "cargo", &fingerprint)?;
            }
        }
    }
    let before = snapshot_files(context.repository_root, &plan.inputs)?;
    let cache = native_cache(context, "cargo")?;
    run_cargo(context.repository_root, &cargo, &rustc, &cache, false)?;
    crate::faults::hit(crate::faults::Point::DependencyFilled);
    assert_snapshot_unchanged(
        context.repository_root,
        &before,
        &plan.inputs,
        "cargo fetch",
    )?;
    crate::faults::hit(crate::faults::Point::DependencyFillValidated);
    run_cargo(context.repository_root, &cargo, &rustc, &cache, true)?;
    crate::faults::hit(crate::faults::Point::DependencyReplayed);
    assert_snapshot_unchanged(
        context.repository_root,
        &before,
        &plan.inputs,
        "cargo frozen/offline replay",
    )?;
    crate::faults::hit(crate::faults::Point::DependencyReplayValidated);
    let receipt = StoredReceipt::new(
        "cargo",
        fingerprint,
        Vec::new(),
        vec![
            "cargo build".to_owned(),
            "cargo check".to_owned(),
            "cargo test".to_owned(),
            "Cargo build scripts and proc-macro execution".to_owned(),
        ],
        Vec::new(),
        &cargo,
    )
    .with_tool(&rustc);
    promote(context, &receipt, &[])?;
    Ok(receipt.public())
}

fn rustup_environment() -> Vec<(OsString, OsString)> {
    if let Some(rustup_home) = std::env::var_os("RUSTUP_HOME") {
        vec![(OsString::from("RUSTUP_HOME"), rustup_home)]
    } else if let Some(home) = dirs::home_dir() {
        vec![(
            OsString::from("RUSTUP_HOME"),
            home.join(".rustup").into_os_string(),
        )]
    } else {
        Vec::new()
    }
}

fn run_cargo(
    repository_root: &Path,
    tool: &ToolIdentity,
    rustc: &ToolIdentity,
    cache: &Path,
    offline: bool,
) -> Result<(), DependencyError> {
    let mut args = vec![OsString::from("fetch"), OsString::from("--locked")];
    if offline {
        args.extend([OsString::from("--frozen"), OsString::from("--offline")]);
    }
    args.push("--manifest-path".into());
    args.push(repository_root.join("Cargo.toml").into_os_string());
    let home = cache.join("home");
    fs::create_dir_all(&home)
        .map_err(|error| DependencyError::Failed(format!("create isolated Cargo home: {error}")))?;
    let mut env = vec![
        (OsString::from("CARGO_HOME"), cache.as_os_str().to_owned()),
        (OsString::from("HOME"), home.into_os_string()),
        (OsString::from("RUSTC"), rustc.path.as_os_str().to_owned()),
        (OsString::from("RUSTUP_AUTO_INSTALL"), OsString::from("0")),
    ];
    if let Some(rustup_home) = std::env::var_os("RUSTUP_HOME") {
        env.push((OsString::from("RUSTUP_HOME"), rustup_home));
    } else if let Some(home) = dirs::home_dir() {
        env.push((
            OsString::from("RUSTUP_HOME"),
            home.join(".rustup").into_os_string(),
        ));
    }
    let hidden_paths = cargo_external_configs(repository_root, cache)?;
    run_tool(
        "cargo",
        if offline {
            "frozen offline replay"
        } else {
            "locked fetch"
        },
        tool,
        repository_root,
        args,
        &env,
        ToolIsolation {
            offline,
            hidden_paths: &hidden_paths,
        },
    )?;
    Ok(())
}

fn cargo_external_configs(
    repository_root: &Path,
    cache: &Path,
) -> Result<Vec<PathBuf>, DependencyError> {
    let root = fs::canonicalize(repository_root).map_err(|error| {
        DependencyError::Failed(format!("resolve Cargo repository root: {error}"))
    })?;
    let cache = fs::canonicalize(cache)
        .map_err(|error| DependencyError::Failed(format!("resolve Cargo cache root: {error}")))?;
    let mut hidden = Vec::new();
    for directory in root
        .ancestors()
        .skip(1)
        .map(|parent| parent.join(".cargo"))
        .chain([cache])
    {
        for name in ["config", "config.toml"] {
            let path = directory.join(name);
            if let Ok(target) = fs::canonicalize(&path) {
                if target == root.join(".cargo/config.toml") {
                    return Err(DependencyError::UnsafeConfiguration {
                        provider: "cargo".into(),
                        path: ".cargo/config.toml".into(),
                        reason: "external Cargo configuration aliases the repository configuration"
                            .into(),
                    });
                }
                hidden.push(target);
            }
            hidden.push(path);
        }
    }
    hidden.sort();
    hidden.dedup();
    Ok(hidden)
}

fn validate_cargo_config(
    root: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<Vec<u8>, DependencyError> {
    let value: toml::Table =
        toml::from_slice(bytes).map_err(|_| DependencyError::InvalidConfiguration {
            provider: "cargo".to_owned(),
            path: relative_portable(root, path).unwrap_or_default(),
            reason: "invalid TOML document".to_owned(),
        })?;
    fn unsafe_key(value: &toml::Value) -> Option<&str> {
        match value {
            toml::Value::Table(table) => table.iter().find_map(|(key, value)| {
                if matches!(
                    key.as_str(),
                    "rustc"
                        | "rustc-wrapper"
                        | "rustc-workspace-wrapper"
                        | "runner"
                        | "credential-process"
                        | "credential-provider"
                        | "credential-alias"
                        | "global-credential-providers"
                        | "include"
                        | "git-fetch-with-cli"
                        | "proxy"
                        | "token"
                        | "env"
                ) {
                    Some(key.as_str())
                } else {
                    unsafe_key(value)
                }
            }),
            toml::Value::Array(values) => values.iter().find_map(unsafe_key),
            _ => None,
        }
    }
    let value = toml::Value::Table(value);
    if let Some(key) = unsafe_key(&value) {
        return Err(DependencyError::UnsafeConfiguration {
            provider: "cargo".to_owned(),
            path: relative_portable(root, path)?,
            reason: format!("{key} is executable or secret-bearing Cargo configuration"),
        });
    }
    serde_json::to_vec(&value).map_err(|error| {
        DependencyError::Failed(format!("could not normalize Cargo config: {error}"))
    })
}

struct GoPlan {
    inputs: Vec<PathBuf>,
    module_roots: Vec<PathBuf>,
    go_work: Option<PathBuf>,
}

impl GoPlan {
    fn inspect(root: &Path) -> Result<Self, DependencyError> {
        let go_work = root.join("go.work").is_file().then(|| root.join("go.work"));
        let module_files = collect_named_files(root, &["go.mod"])?;
        if module_files.is_empty() {
            return Err(DependencyError::LockMissing(
                "Go readiness requires at least one go.mod".to_owned(),
            ));
        }
        if go_work.is_none() && !root.join("go.mod").is_file() {
            return Err(DependencyError::InvalidConfiguration {
                provider: "go".to_owned(),
                path: "go.mod".to_owned(),
                reason: "nested modules require a root go.work".to_owned(),
            });
        }
        let mut inputs = Vec::new();
        let mut module_roots = Vec::new();
        for module in module_files {
            let module_root = module.parent().unwrap_or(root).to_path_buf();
            let sum = module_root.join("go.sum");
            if !sum.is_file() {
                return Err(DependencyError::LockMissing(format!(
                    "{} has no go.sum; run `go mod download` and commit it",
                    relative_portable(root, &module)?
                )));
            }
            inputs.push(module);
            inputs.push(sum);
            module_roots.push(module_root);
        }
        if let Some(work) = &go_work {
            inputs.push(work.clone());
            let work_sum = root.join("go.work.sum");
            if work_sum.is_file() {
                inputs.push(work_sum);
            }
        }
        inputs.sort_by_key(|path| relative_portable(root, path).unwrap_or_default());
        inputs.dedup();
        validate_go_input_locations(root, &inputs)?;
        for input in &inputs {
            let relative = relative_portable(root, input)?;
            reject_embedded_secrets("go", &relative, &read_bytes(input, &relative)?)?;
        }
        module_roots.sort();
        module_roots.dedup();
        Ok(Self {
            inputs,
            module_roots,
            go_work,
        })
    }

    fn fingerprint(&self, root: &Path, tool: &ToolIdentity) -> Result<String, DependencyError> {
        let mut fingerprint = FingerprintBuilder::new("go");
        fingerprint.tool(tool);
        let graph = self
            .inputs
            .iter()
            .map(|path| relative_portable(root, path))
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        fingerprint.field("graph", graph.as_bytes());
        for input in &self.inputs {
            fingerprint.file(root, input)?;
        }
        fingerprint.field(
            "policy:go",
            b"mod-download-all;mod-verify;offline-replay;no-tidy;no-build;no-vendor;no-sync;no-generate;native-readonly-local-graph-v1",
        );
        policy_fingerprint(root, &mut fingerprint)?;
        Ok(fingerprint.finish())
    }

    fn remap(&self, source_root: &Path, destination_root: &Path) -> Result<Self, DependencyError> {
        Ok(Self {
            inputs: self
                .inputs
                .iter()
                .map(|path| remap_go_path(source_root, destination_root, path))
                .collect::<Result<Vec<_>, _>>()?,
            module_roots: self
                .module_roots
                .iter()
                .map(|path| remap_go_path(source_root, destination_root, path))
                .collect::<Result<Vec<_>, _>>()?,
            go_work: self
                .go_work
                .as_ref()
                .map(|path| remap_go_path(source_root, destination_root, path))
                .transpose()?,
        })
    }
}

struct GoProbe {
    _staging: tempfile::TempDir,
    root: PathBuf,
    plan: GoPlan,
    baseline: Snapshot,
    originals: Vec<(PathBuf, Vec<PathBuf>, Snapshot)>,
}

impl GoProbe {
    fn create(
        context: &DependencyContext<'_>,
        repository_plan: &GoPlan,
    ) -> Result<Self, DependencyError> {
        let repository_before = snapshot_files(context.repository_root, &repository_plan.inputs)?;
        assert_go_graph_unchanged(
            context.repository_root,
            &repository_before,
            &repository_plan.inputs,
            "Go repository baseline",
        )?;

        let workspace_plan =
            repository_plan.remap(context.repository_root, context.workspace_root)?;
        assert_go_graph_unchanged(
            context.workspace_root,
            &repository_before,
            &workspace_plan.inputs,
            "Go workspace baseline",
        )?;
        let workspace_before = snapshot_files(context.workspace_root, &workspace_plan.inputs)?;

        let staging = create_staging(context, "go", "probe")?;
        let root = staging.path().join("workspace");
        context
            .filesystem
            .clone_tree(context.workspace_root, &root)
            .map_err(|error| DependencyError::CowUnavailable {
                path: "go-probe".to_owned(),
                reason: error.to_string(),
            })?;
        crate::faults::hit(crate::faults::Point::GoProbeCloned);
        let plan = workspace_plan.remap(context.workspace_root, &root)?;
        validate_go_input_locations(&root, &plan.inputs)?;
        assert_go_graph_unchanged(
            &root,
            &workspace_before,
            &plan.inputs,
            "Go COW probe baseline",
        )?;
        let baseline = snapshot_files(&root, &plan.inputs)?;
        crate::faults::hit(crate::faults::Point::GoProbeValidated);

        let mut originals = vec![(
            context.repository_root.to_path_buf(),
            repository_plan.inputs.clone(),
            repository_before,
        )];
        if context.workspace_root != context.repository_root {
            originals.push((
                context.workspace_root.to_path_buf(),
                workspace_plan.inputs,
                workspace_before,
            ));
        }
        Ok(Self {
            _staging: staging,
            root,
            plan,
            baseline,
            originals,
        })
    }

    fn verify(&self, phase: &str) -> Result<(), DependencyError> {
        assert_go_graph_unchanged(&self.root, &self.baseline, &self.plan.inputs, phase)?;
        for (root, inputs, baseline) in &self.originals {
            assert_go_graph_unchanged(root, baseline, inputs, phase)?;
        }
        Ok(())
    }
}

fn remap_go_path(
    source_root: &Path,
    destination_root: &Path,
    path: &Path,
) -> Result<PathBuf, DependencyError> {
    if path == source_root {
        return Ok(destination_root.to_path_buf());
    }
    // `relative_portable` performs the path traversal validation while the
    // original OsStr components preserve non-UTF-8 workspace names.
    relative_portable(source_root, path)?;
    let relative = path.strip_prefix(source_root).map_err(|_| {
        DependencyError::Policy("Go dependency input escaped its workspace".to_owned())
    })?;
    Ok(destination_root.join(relative))
}

async fn ensure_go(
    context: &DependencyContext<'_>,
    executable: Option<&Path>,
) -> Result<DependencyReceipt, DependencyError> {
    let plan = GoPlan::inspect(context.repository_root)?;
    let mut probe = GoProbe::create(context, &plan)?;
    let go_environment = [
        (OsString::from("GOTOOLCHAIN"), OsString::from("local")),
        (OsString::from("GOENV"), OsString::from("off")),
        (OsString::from("GOWORK"), OsString::from("off")),
    ];
    let tool_result = identify_tool_at("go", executable, &probe.root, &go_environment);
    probe.verify("Go tool identification")?;
    let tool = tool_result?;
    let fingerprint = plan.fingerprint(context.repository_root, &tool)?;
    let _flight = single_flight(format!("go:{fingerprint}")).await;
    if let Some(receipt) = load_receipt(context, "go", &fingerprint)? {
        let cache = native_cache(context, "go")?;
        match run_go_phase(&probe, &tool, &cache, true, "Go cached offline replay") {
            Ok(()) => {
                crate::faults::hit(crate::faults::Point::GoCachedReplayValidated);
                return Ok(receipt.public());
            }
            Err(error @ DependencyError::CommandFailed { .. }) => {
                tracing::warn!(fingerprint, reason = %error, "refilling Go cache after offline replay failure");
                invalidate_receipt(context, "go", &fingerprint)?;
                probe = GoProbe::create(context, &plan)?;
            }
            Err(error) => return Err(error),
        }
    }
    let cache = native_cache(context, "go")?;
    run_go_phase(&probe, &tool, &cache, false, "Go online fill")?;
    run_go_phase(&probe, &tool, &cache, true, "Go offline replay")?;
    let receipt = StoredReceipt::new(
        "go",
        fingerprint,
        Vec::new(),
        vec![
            "go tidy".to_owned(),
            "go build/test/generate".to_owned(),
            "go mod vendor".to_owned(),
            "go work sync".to_owned(),
        ],
        Vec::new(),
        &tool,
    );
    promote(context, &receipt, &[])?;
    Ok(receipt.public())
}

fn run_go_phase(
    probe: &GoProbe,
    tool: &ToolIdentity,
    cache: &Path,
    offline: bool,
    phase: &str,
) -> Result<(), DependencyError> {
    validate_go_local_graph(probe, tool)?;
    crate::faults::hit(crate::faults::Point::GoGraphValidated);
    let command_result = run_go(&probe.plan, tool, cache, offline);
    if command_result.is_ok() {
        crate::faults::hit(if offline {
            crate::faults::Point::DependencyReplayed
        } else {
            crate::faults::Point::DependencyFilled
        });
    }
    // Always check the immutable dependency graph, including when the tool
    // failed. A graph mutation is the higher-priority safety failure.
    probe.verify(phase)?;
    command_result?;
    crate::faults::hit(if offline {
        crate::faults::Point::DependencyReplayValidated
    } else {
        crate::faults::Point::DependencyFillValidated
    });
    Ok(())
}

fn go_graph_error(root: &Path, file: &Path, reason: &str) -> DependencyError {
    DependencyError::UnsafeConfiguration {
        provider: "go".into(),
        path: relative_portable(root, file).unwrap_or_else(|_| "go.mod/go.work".into()),
        reason: reason.into(),
    }
}

fn validate_go_input_locations(root: &Path, inputs: &[PathBuf]) -> Result<(), DependencyError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| DependencyError::Failed(format!("resolve Go root: {error}")))?;
    for file in inputs {
        let resolved = fs::canonicalize(file).map_err(|_| {
            go_graph_error(root, file, "Go metadata must exist inside the repository")
        })?;
        if !resolved.starts_with(&canonical_root) {
            return Err(go_graph_error(
                root,
                file,
                "Go metadata follows a link outside the repository",
            ));
        }
    }
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoGraphFile {
    #[serde(rename = "Use", default)]
    use_: Option<Vec<GoGraphUse>>,
    #[serde(default)]
    replace: Option<Vec<GoGraphReplace>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoGraphUse {
    disk_path: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoGraphReplace {
    new: GoGraphTarget,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoGraphTarget {
    path: String,
    #[serde(default)]
    version: String,
}

fn validate_go_local_graph(probe: &GoProbe, tool: &ToolIdentity) -> Result<(), DependencyError> {
    let env = [
        (OsString::from("GOENV"), OsString::from("off")),
        (OsString::from("GOTOOLCHAIN"), OsString::from("local")),
        (OsString::from("GOWORK"), OsString::from("off")),
    ];
    let root = fs::canonicalize(&probe.root)
        .map_err(|error| DependencyError::Failed(format!("resolve Go probe: {error}")))?;
    let modules = probe
        .plan
        .module_roots
        .iter()
        .map(fs::canonicalize)
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|error| DependencyError::Failed(format!("resolve Go modules: {error}")))?;
    for file in probe.plan.inputs.iter().filter(|file| {
        matches!(
            file.file_name().and_then(OsStr::to_str),
            Some("go.mod" | "go.work")
        )
    }) {
        let kind = if file.file_name() == Some(OsStr::new("go.work")) {
            "work"
        } else {
            "mod"
        };
        let output = run_tool(
            "go",
            "read-only module graph",
            tool,
            &probe.root,
            [
                OsString::from(kind),
                "edit".into(),
                "-json".into(),
                file.as_os_str().to_owned(),
            ],
            &env,
            ToolIsolation::network(true),
        );
        // -json is read-only. Verify that promise even if the native command fails.
        probe.verify("Go read-only graph inspection")?;
        let output = output?;
        let graph: GoGraphFile = serde_json::from_slice(&output.stdout).map_err(|_| {
            go_graph_error(
                &probe.root,
                file,
                "Go did not return a valid structured module graph",
            )
        })?;
        for path in graph
            .use_
            .unwrap_or_default()
            .into_iter()
            .map(|entry| entry.disk_path)
            .chain(
                graph
                    .replace
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|entry| entry.new.version.is_empty())
                    .map(|entry| entry.new.path),
            )
        {
            let error = || {
                go_graph_error(
                    &probe.root,
                    file,
                    "local use/replace paths must resolve to a module with committed go.mod/go.sum inside the COW probe",
                )
            };
            let relative = Path::new(&path);
            if relative.is_absolute() || path.is_empty() {
                return Err(error());
            }
            let owner = file.parent().ok_or_else(error)?;
            let mut target = root.join(owner.strip_prefix(&probe.root).map_err(|_| error())?);
            for component in relative.components() {
                match component {
                    std::path::Component::CurDir => {}
                    std::path::Component::Normal(name) => target.push(name),
                    std::path::Component::ParentDir if target != root => {
                        target.pop();
                    }
                    _ => return Err(error()),
                }
            }
            let canonical = fs::canonicalize(target).map_err(|_| error())?;
            if !canonical.starts_with(&root) || !modules.contains(&canonical) {
                return Err(error());
            }
        }
    }
    Ok(())
}

fn run_go(
    plan: &GoPlan,
    tool: &ToolIdentity,
    cache: &Path,
    offline: bool,
) -> Result<(), DependencyError> {
    let mod_cache = cache.join("mod");
    let build_cache = cache.join("build");
    let gopath = cache.join("gopath");
    fs::create_dir_all(&mod_cache).map_err(|error| {
        DependencyError::Failed(format!("failed to create Go module cache: {error}"))
    })?;
    fs::create_dir_all(&build_cache).map_err(|error| {
        DependencyError::Failed(format!("failed to create Go build cache: {error}"))
    })?;
    fs::create_dir_all(&gopath)
        .map_err(|error| DependencyError::Failed(format!("failed to create Go path: {error}")))?;
    let home = cache.join("home");
    fs::create_dir_all(&home).map_err(|error| {
        DependencyError::Failed(format!("failed to create isolated Go home: {error}"))
    })?;
    let mut env = vec![
        (OsString::from("HOME"), home.into_os_string()),
        (OsString::from("GOENV"), OsString::from("off")),
        (OsString::from("GOTOOLCHAIN"), OsString::from("local")),
        (OsString::from("GOMODCACHE"), mod_cache.into_os_string()),
        (OsString::from("GOCACHE"), build_cache.into_os_string()),
        (OsString::from("GOPATH"), gopath.into_os_string()),
    ];
    if let Some(work) = &plan.go_work {
        env.push((OsString::from("GOWORK"), work.as_os_str().to_owned()));
    } else {
        env.push((OsString::from("GOWORK"), OsString::from("off")));
    }
    if offline {
        env.push((OsString::from("GOPROXY"), OsString::from("off")));
        env.push((OsString::from("GOSUMDB"), OsString::from("off")));
    }
    for module_root in &plan.module_roots {
        run_tool(
            "go",
            if offline {
                "offline module replay"
            } else {
                "module download"
            },
            tool,
            module_root,
            ["mod", "download", "all"],
            &env,
            ToolIsolation::network(offline),
        )?;
        crate::faults::hit(if offline {
            crate::faults::Point::GoOfflineDownloaded
        } else {
            crate::faults::Point::GoOnlineDownloaded
        });
        run_tool(
            "go",
            if offline {
                "offline module verify"
            } else {
                "module verify"
            },
            tool,
            module_root,
            ["mod", "verify"],
            &env,
            ToolIsolation::network(offline),
        )?;
        crate::faults::hit(if offline {
            crate::faults::Point::GoOfflineVerified
        } else {
            crate::faults::Point::GoOnlineVerified
        });
    }
    Ok(())
}

type Snapshot = BTreeMap<String, String>;

fn snapshot_files(root: &Path, files: &[PathBuf]) -> Result<Snapshot, DependencyError> {
    let mut snapshot = BTreeMap::new();
    for path in files {
        let relative = relative_portable(root, path)?;
        let bytes = fs::read(path).map_err(|error| DependencyError::Io {
            action: "snapshot dependency graph".to_owned(),
            path: relative.clone(),
            message: error.to_string(),
        })?;
        snapshot.insert(relative, hex::encode(Sha256::digest(bytes)));
    }
    Ok(snapshot)
}

fn assert_snapshot_unchanged(
    root: &Path,
    before: &Snapshot,
    files: &[PathBuf],
    phase: &str,
) -> Result<(), DependencyError> {
    let after = snapshot_files(root, files)?;
    if before != &after {
        let changed = changed_paths(before, &after).join(", ");
        return Err(DependencyError::LockStale(format!(
            "{phase} changed dependency inputs: {changed}"
        )));
    }
    Ok(())
}

fn assert_go_graph_unchanged(
    root: &Path,
    before: &Snapshot,
    files: &[PathBuf],
    phase: &str,
) -> Result<(), DependencyError> {
    assert_snapshot_unchanged(root, before, files, phase)?;
    let current = collect_named_files(root, &["go.mod", "go.sum", "go.work", "go.work.sum"])?;
    let expected = files
        .iter()
        .map(|path| relative_portable(root, path))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let current = current
        .iter()
        .map(|path| relative_portable(root, path))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if current != expected {
        return Err(DependencyError::LockStale(format!(
            "{phase} added or removed go.mod/go.sum/go.work inputs"
        )));
    }
    Ok(())
}

fn changed_paths(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect()
}
