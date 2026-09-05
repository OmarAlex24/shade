use super::common::{
    Approval, FingerprintBuilder, StoredReceipt, ToolIdentity, ToolIsolation, collect_named_files,
    copy_input, create_staging, identify_tool_at, load_receipt, materialize, native_cache,
    policy_fingerprint, promote, read_bytes, reject_embedded_secrets, relative_portable, run_tool,
    semantic_line_config, single_flight,
};
use super::{DependencyContext, DependencyError, DependencyProvider, DependencyReceipt};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Manager {
    Bun,
    Pnpm,
    Npm,
}

impl Manager {
    fn name(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Pnpm => "pnpm",
            Self::Npm => "npm",
        }
    }

    fn lock_names(self) -> &'static [&'static str] {
        match self {
            Self::Bun => &["bun.lock", "bun.lockb"],
            Self::Pnpm => &["pnpm-lock.yaml"],
            Self::Npm => &["package-lock.json", "npm-shrinkwrap.json"],
        }
    }
}

macro_rules! javascript_provider {
    ($type:ident, $manager:expr) => {
        #[derive(Debug, Clone, Default)]
        pub struct $type {
            executable: Option<PathBuf>,
        }

        impl $type {
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
        impl DependencyProvider for $type {
            fn name(&self) -> &'static str {
                $manager.name()
            }

            fn applies(&self, repository_root: &Path) -> bool {
                manager_applies(repository_root, $manager)
            }

            async fn ensure_ready(
                &self,
                context: &DependencyContext<'_>,
            ) -> Result<DependencyReceipt, DependencyError> {
                ensure_javascript(context, $manager, self.executable.as_deref()).await
            }
        }
    };
}

javascript_provider!(BunProvider, Manager::Bun);
javascript_provider!(PnpmProvider, Manager::Pnpm);
javascript_provider!(NpmProvider, Manager::Npm);

struct JavascriptPlan {
    manager: Manager,
    inputs: Vec<PathBuf>,
    safe_npmrc: Option<Vec<u8>>,
    safe_bunfig: Option<Vec<u8>>,
    approvals: Vec<Approval>,
    lock_bytes: Vec<u8>,
    declared_version: Option<String>,
}

impl JavascriptPlan {
    fn inspect(root: &Path, manager: Manager) -> Result<Self, DependencyError> {
        let manifest = root.join("package.json");
        if !manifest.is_file() {
            return Err(DependencyError::LockMissing(format!(
                "{} requires package.json at the dependency root",
                manager.name()
            )));
        }
        reject_executable_configuration(root, manager)?;
        let locks = all_root_locks(root);
        if locks.len() > 1 {
            return Err(DependencyError::InvalidConfiguration {
                provider: manager.name().to_owned(),
                path: "package.json".to_owned(),
                reason: format!(
                    "multiple JavaScript lockfiles are present ({}); keep exactly one",
                    locks
                        .iter()
                        .filter_map(|path| path.file_name().and_then(OsStr::to_str))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        let lock = manager
            .lock_names()
            .iter()
            .map(|name| root.join(name))
            .find(|path| path.is_file())
            .ok_or_else(|| {
                if manager == Manager::Npm && all_root_locks(root).is_empty() {
                    DependencyError::LockMissing(
                        "package.json has no supported JavaScript lock; commit exactly one of bun.lock, pnpm-lock.yaml, package-lock.json, or npm-shrinkwrap.json and pin packageManager"
                            .to_owned(),
                    )
                } else {
                    DependencyError::LockMissing(format!(
                        "{} requires one of: {} (generate and commit it before opening the workspace)",
                        manager.name(),
                        manager.lock_names().join(", ")
                    ))
                }
            })?;
        if lock.file_name() == Some(OsStr::new("bun.lockb")) {
            return Err(DependencyError::InvalidLock {
                provider: "bun".to_owned(),
                path: "bun.lockb".to_owned(),
                reason: "binary Bun locks cannot prove package/version/integrity approvals; regenerate bun.lock with a current Bun".to_owned(),
            });
        }

        let manifest_bytes = read_bytes(&manifest, "package.json")?;
        let manifest_json: Value = serde_json::from_slice(&manifest_bytes).map_err(|error| {
            DependencyError::InvalidConfiguration {
                provider: manager.name().to_owned(),
                path: "package.json".to_owned(),
                reason: format!("invalid JSON: {error}"),
            }
        })?;
        let (declared_manager, declared_version) = declared_package_manager(&manifest_json)?;
        if let Some(declared_manager) = declared_manager
            && declared_manager != manager.name()
        {
            return Err(DependencyError::InvalidConfiguration {
                provider: manager.name().to_owned(),
                path: "package.json".to_owned(),
                reason: format!(
                    "packageManager selects {declared_manager}, but {} was selected by the lockfile",
                    manager.name()
                ),
            });
        }

        let lock_bytes = read_bytes(&lock, "JavaScript lockfile")?;
        validate_lock_layout(manager, &lock_bytes)?;
        let approvals = parse_approvals(manager, &lock_bytes)?;
        let safe_npmrc = root
            .join(".npmrc")
            .is_file()
            .then(|| sanitize_line_config(manager.name(), ".npmrc", &root.join(".npmrc")))
            .transpose()?;
        let safe_bunfig = root
            .join("bunfig.toml")
            .is_file()
            .then(|| sanitize_bunfig(&root.join("bunfig.toml")))
            .transpose()?;

        let mut inputs = collect_named_files(root, &["package.json"])?;
        inputs.push(lock.clone());
        for name in ["pnpm-workspace.yaml", "pnpm-workspace.yml"] {
            let path = root.join(name);
            if path.is_file() {
                let bytes = read_bytes(&path, name)?;
                let value: Value = serde_yaml_ng::from_slice(&bytes).map_err(|_| {
                    DependencyError::InvalidConfiguration {
                        provider: manager.name().to_owned(),
                        path: name.to_owned(),
                        reason: "invalid workspace YAML".to_owned(),
                    }
                })?;
                reject_automatic_tools_or_hooks(manager.name(), name, &value)?;
                inputs.push(path);
            }
        }
        for input in &inputs {
            if input.file_name() == Some(OsStr::new("package.json")) {
                let relative = relative_portable(root, input)?;
                let value: Value =
                    serde_json::from_slice(&read_bytes(input, &relative)?).map_err(|_| {
                        DependencyError::InvalidConfiguration {
                            provider: manager.name().to_owned(),
                            path: relative.clone(),
                            reason: "invalid package JSON".to_owned(),
                        }
                    })?;
                reject_automatic_tools_or_hooks(manager.name(), &relative, &value)?;
                if !safe_dependency_names(&value) {
                    return Err(DependencyError::InvalidConfiguration {
                        provider: manager.name().into(),
                        path: relative,
                        reason: "unsafe dependency name".into(),
                    });
                }
            }
        }
        inputs.sort_by_key(|path| relative_portable(root, path).unwrap_or_default());
        inputs.dedup();
        for input in &inputs {
            let relative = relative_portable(root, input)?;
            reject_embedded_secrets(manager.name(), &relative, &read_bytes(input, &relative)?)?;
        }
        Ok(Self {
            manager,
            inputs,
            safe_npmrc,
            safe_bunfig,
            approvals,
            lock_bytes,
            declared_version,
        })
    }

    fn fingerprint(
        &self,
        root: &Path,
        tool: &ToolIdentity,
        grants: &[shade_protocol::ScriptApproval],
        scripts: Option<&super::scripts::ScriptTools>,
    ) -> Result<String, DependencyError> {
        let mut fingerprint = FingerprintBuilder::new(self.manager.name());
        fingerprint.tool(tool);
        fingerprint.field(
            "policy:script-runtime",
            b"explicit-v1;isolated-package-writes;detached-file-links;no-nested-writes;dependency-order;no-network;fixed-node;hoisted-bun;isolated-pnpm",
        );
        fingerprint.field(
            "policy:script-grants",
            &serde_json::to_vec(grants)
                .map_err(|_| DependencyError::Failed("invalid script grants".into()))?,
        );
        if let Some(scripts) = scripts {
            fingerprint.tool(&scripts.node);
            fingerprint.tool(&scripts.shell);
            fingerprint.field(
                "script-runtime-libraries",
                scripts.libraries_digest.as_bytes(),
            );
        }
        let graph = self
            .inputs
            .iter()
            .map(|path| relative_portable(root, path))
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        fingerprint.field("graph", graph.as_bytes());
        fingerprint.field(
            "policy:configuration",
            b"private-home;explicit-empty-user-global-config;no-load-hooks;owned-output-paths-v1",
        );
        for path in &self.inputs {
            fingerprint.file(root, path)?;
        }
        if let Some(config) = &self.safe_npmrc {
            fingerprint.field("config:.npmrc", &semantic_line_config(config));
        }
        if let Some(config) = &self.safe_bunfig {
            let value: toml::Table = toml::from_slice(config).map_err(|_| {
                DependencyError::Failed("invalid sanitized Bun configuration".to_owned())
            })?;
            fingerprint.field(
                "config:bunfig.toml",
                &serde_json::to_vec(&value).map_err(|_| {
                    DependencyError::Failed("could not normalize Bun configuration".to_owned())
                })?,
            );
        }
        fingerprint.field(
            "policy:approvals",
            &serde_json::to_vec(&self.approvals).map_err(|error| {
                DependencyError::Failed(format!("failed to encode JavaScript approvals: {error}"))
            })?,
        );
        policy_fingerprint(root, &mut fingerprint)?;
        Ok(fingerprint.finish())
    }

    fn stage_inputs(&self, repository_root: &Path, stage: &Path) -> Result<(), DependencyError> {
        for input in &self.inputs {
            copy_input(repository_root, input, stage)?;
        }
        if let Some(config) = &self.safe_npmrc {
            fs::write(stage.join(".npmrc"), config).map_err(|error| DependencyError::Io {
                action: "write sanitized npm configuration".to_owned(),
                path: ".npmrc".to_owned(),
                message: error.to_string(),
            })?;
        }
        if let Some(config) = &self.safe_bunfig {
            fs::write(stage.join("bunfig.toml"), config).map_err(|error| DependencyError::Io {
                action: "write sanitized Bun configuration".to_owned(),
                path: "bunfig.toml".to_owned(),
                message: error.to_string(),
            })?;
        }
        Ok(())
    }
}

async fn ensure_javascript(
    context: &DependencyContext<'_>,
    manager: Manager,
    executable: Option<&Path>,
) -> Result<DependencyReceipt, DependencyError> {
    let plan = JavascriptPlan::inspect(context.repository_root, manager)?;
    let identification = create_staging(context, manager.name(), "identify")?;
    plan.stage_inputs(context.repository_root, identification.path())?;
    let environment = isolated_javascript_environment(identification.path())?;
    let tool = identify_tool_at(
        manager.name(),
        executable,
        identification.path(),
        &environment,
    )?;
    if let Some(expected) = &plan.declared_version
        && !tool.version.split_whitespace().any(|part| part == expected)
    {
        return Err(DependencyError::ToolVersionMismatch {
            tool: manager.name().to_owned(),
            expected: expected.clone(),
            actual: tool.version.clone(),
        });
    }
    let mut grants = context
        .script_approvals
        .iter()
        .filter(|grant| {
            grant.provider == manager.name()
                && plan.approvals.iter().any(|identity| {
                    identity.package == grant.package
                        && identity.version == grant.version
                        && identity.integrity == grant.integrity
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    grants.sort();
    grants.dedup();
    let script_tools = (!grants.is_empty())
        .then(|| super::scripts::ScriptTools::identify(context.repository_root))
        .transpose()?;
    let fingerprint = plan.fingerprint(
        context.repository_root,
        &tool,
        &grants,
        script_tools.as_ref(),
    )?;
    let _flight = single_flight(format!("{}:{fingerprint}", manager.name())).await;
    if let Some(receipt) = load_receipt(context, manager.name(), &fingerprint)? {
        materialize(context, &receipt)?;
        return Ok(receipt.public());
    }

    let native = native_cache(context, manager.name())?;
    let fill = create_staging(context, manager.name(), "fill")?;
    plan.stage_inputs(context.repository_root, fill.path())?;
    run_install(&plan, &tool, fill.path(), &native, false)?;
    crate::faults::hit(crate::faults::Point::DependencyFilled);
    validate_install(&plan, fill.path(), "online fill")?;
    crate::faults::hit(crate::faults::Point::DependencyFillValidated);

    let replay = create_staging(context, manager.name(), "replay")?;
    plan.stage_inputs(context.repository_root, replay.path())?;
    run_install(&plan, &tool, replay.path(), &native, true)?;
    crate::faults::hit(crate::faults::Point::DependencyReplayed);
    let replay_outputs = validate_install(&plan, replay.path(), "offline replay")?;
    crate::faults::hit(crate::faults::Point::DependencyReplayValidated);
    let build = if script_tools.is_some() {
        let build = create_staging(context, manager.name(), "scripts")?;
        plan.stage_inputs(context.repository_root, build.path())?;
        // Manager stores may use hardlinks. Scripts receive independent COW
        // files, never writable links into a native cache or another layer.
        for (source, relative) in &replay_outputs {
            let target = build.path().join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|_| DependencyError::Failed("cannot create script stage".into()))?;
            }
            context
                .filesystem
                .clone_tree(source, &target)
                .map_err(|error| DependencyError::CowUnavailable {
                    path: relative.clone(),
                    reason: error.to_string(),
                })?;
        }
        Some(build)
    } else {
        None
    };
    let stage = build.as_ref().map_or(replay.path(), |stage| stage.path());
    let mut scripts =
        super::scripts::inventory(manager.name(), stage, &plan.lock_bytes, &plan.approvals)?;
    if let Some(tools) = &script_tools {
        super::scripts::execute(stage, &mut scripts, &grants, tools)?;
        crate::faults::hit(crate::faults::Point::DependencyScriptsExecuted);
    }
    let outputs = validate_install(&plan, stage, "approved scripts")?;
    let materialized_paths = outputs
        .iter()
        .map(|(_, relative)| relative.clone())
        .collect::<Vec<_>>();
    let mut receipt = StoredReceipt::new(
        manager.name(),
        fingerprint,
        materialized_paths,
        std::iter::once("root lifecycle scripts".to_owned())
            .chain(
                scripts
                    .iter()
                    .any(|script| !script.report.executed)
                    .then(|| "dependency lifecycle scripts".to_owned()),
            )
            .collect(),
        plan.approvals.clone(),
        &tool,
    );
    receipt.scripts = scripts.into_iter().map(|script| script.report).collect();
    if let Some(tools) = &script_tools {
        receipt = receipt.with_tool(&tools.node).with_tool(&tools.shell);
    }
    promote(context, &receipt, &outputs)?;
    materialize(context, &receipt)?;
    Ok(receipt.public())
}

fn isolated_javascript_environment(
    stage: &Path,
) -> Result<Vec<(OsString, OsString)>, DependencyError> {
    let home = stage.join(".home");
    for directory in [
        &home,
        &home.join(".config"),
        &home.join("system-config"),
        &stage.join(".tmp"),
    ] {
        fs::create_dir_all(directory).map_err(|error| {
            DependencyError::Failed(format!("create isolated JavaScript environment: {error}"))
        })?;
    }
    let user_config = home.join("user.npmrc");
    let global_config = home.join("global.npmrc");
    for path in [&user_config, &global_config] {
        fs::write(path, b"").map_err(|error| {
            DependencyError::Failed(format!("create empty npm configuration: {error}"))
        })?;
    }
    Ok(vec![
        ("HOME".into(), home.clone().into_os_string()),
        (
            "XDG_CONFIG_HOME".into(),
            home.join(".config").into_os_string(),
        ),
        (
            "XDG_CONFIG_DIRS".into(),
            home.join("system-config").into_os_string(),
        ),
        ("TMPDIR".into(), stage.join(".tmp").into_os_string()),
        ("NPM_CONFIG_USERCONFIG".into(), user_config.into_os_string()),
        (
            "NPM_CONFIG_GLOBALCONFIG".into(),
            global_config.into_os_string(),
        ),
        ("NPM_CONFIG_IGNORE_SCRIPTS".into(), "true".into()),
        ("NPM_CONFIG_IGNORE_PNPMFILE".into(), "true".into()),
        ("NPM_CONFIG_AUDIT".into(), "false".into()),
        ("NPM_CONFIG_FUND".into(), "false".into()),
        (
            "NPM_CONFIG_MANAGE_PACKAGE_MANAGER_VERSIONS".into(),
            "false".into(),
        ),
        ("NPM_CONFIG_PM_ON_FAIL".into(), "error".into()),
        ("NPM_CONFIG_RUNTIME_ON_FAIL".into(), "error".into()),
    ])
}

fn run_install(
    plan: &JavascriptPlan,
    tool: &ToolIdentity,
    stage: &Path,
    native_cache: &Path,
    offline: bool,
) -> Result<(), DependencyError> {
    let mut env = isolated_javascript_environment(stage)?;
    if !stage.join(".npmrc").is_file() {
        fs::write(stage.join(".npmrc"), b"").map_err(|error| {
            DependencyError::Failed(format!("failed to create isolated npm config: {error}"))
        })?;
    }
    let mut args: Vec<OsString> = match plan.manager {
        Manager::Bun => ["install", "--frozen-lockfile", "--ignore-scripts"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        Manager::Pnpm => ["install", "--frozen-lockfile", "--ignore-scripts"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        Manager::Npm => ["ci", "--ignore-scripts", "--no-audit", "--no-fund"]
            .into_iter()
            .map(OsString::from)
            .collect(),
    };
    match plan.manager {
        Manager::Bun => {
            args.push("--linker=hoisted".into());
        }
        Manager::Pnpm => {
            args.push("--node-linker=isolated".into());
            args.push("--virtual-store-dir".into());
            args.push(stage.join("node_modules/.pnpm").into_os_string());
            if offline {
                args.push("--offline".into());
            }
            args.push("--store-dir".into());
            args.push(native_cache.as_os_str().to_owned());
            // pnpm's policy-verification and metadata cache is separate from
            // its content store. Preserve both across isolated fill/replay homes.
            args.push("--cache-dir".into());
            args.push(native_cache.join("metadata").into_os_string());
        }
        Manager::Npm => {
            if offline {
                args.push("--offline".into());
            }
            args.push("--cache".into());
            args.push(native_cache.as_os_str().to_owned());
        }
    }
    if plan.manager == Manager::Bun {
        env.push((
            OsString::from("BUN_INSTALL_CACHE_DIR"),
            native_cache.as_os_str().to_owned(),
        ));
    }
    if offline {
        env.push((OsString::from("NPM_CONFIG_OFFLINE"), OsString::from("true")));
        env.push((
            OsString::from("NPM_CONFIG_FETCH_RETRIES"),
            OsString::from("0"),
        ));
    }
    run_tool(
        plan.manager.name(),
        if offline {
            "offline replay"
        } else {
            "online fill"
        },
        tool,
        stage,
        args,
        &env,
        ToolIsolation::network(offline),
    )?;
    Ok(())
}

fn validate_install(
    plan: &JavascriptPlan,
    stage: &Path,
    phase: &str,
) -> Result<Vec<(PathBuf, String)>, DependencyError> {
    let mut roots = Vec::new();
    for entry in WalkDir::new(stage).follow_links(false) {
        let entry = entry.map_err(|error| DependencyError::Validation {
            provider: plan.manager.name().to_owned(),
            reason: format!("could not inspect {phase}: {error}"),
        })?;
        if entry.file_type().is_symlink() {
            let target =
                fs::read_link(entry.path()).map_err(|error| DependencyError::Validation {
                    provider: plan.manager.name().to_owned(),
                    reason: format!("could not read installed symlink: {error}"),
                })?;
            if target.is_absolute() {
                return Err(DependencyError::Validation {
                    provider: plan.manager.name().to_owned(),
                    reason: format!(
                        "{phase} produced an absolute symlink at {}",
                        relative_portable(stage, entry.path())?
                    ),
                });
            }
        }
        if entry.file_type().is_dir() && entry.file_name() == OsStr::new("node_modules") {
            let relative = relative_portable(stage, entry.path())?;
            if !roots
                .iter()
                .any(|(_, parent): &(PathBuf, String)| relative.starts_with(&format!("{parent}/")))
            {
                roots.push((entry.path().to_path_buf(), relative));
            }
        }
    }
    if !plan.approvals.is_empty() && roots.is_empty() {
        return Err(DependencyError::Validation {
            provider: plan.manager.name().to_owned(),
            reason: format!("{phase} did not produce a node_modules forest"),
        });
    }
    roots.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(roots)
}

fn manager_applies(root: &Path, manager: Manager) -> bool {
    let manifest = root.join("package.json");
    if !manifest.is_file() {
        return false;
    }
    if manager
        .lock_names()
        .iter()
        .any(|name| root.join(name).is_file())
    {
        return true;
    }
    let declared = fs::read(&manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| {
            value
                .get("packageManager")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    if let Some(declared) = declared {
        return declared.split('@').next() == Some(manager.name());
    }
    manager == Manager::Npm && all_root_locks(root).is_empty()
}

fn all_root_locks(root: &Path) -> Vec<PathBuf> {
    [
        "bun.lock",
        "bun.lockb",
        "pnpm-lock.yaml",
        "package-lock.json",
        "npm-shrinkwrap.json",
    ]
    .into_iter()
    .map(|name| root.join(name))
    .filter(|path| path.is_file())
    .collect()
}

fn declared_package_manager(
    manifest: &Value,
) -> Result<(Option<String>, Option<String>), DependencyError> {
    let Some(value) = manifest.get("packageManager") else {
        return Ok((None, None));
    };
    let value = value
        .as_str()
        .ok_or_else(|| DependencyError::InvalidConfiguration {
            provider: "javascript".to_owned(),
            path: "package.json".to_owned(),
            reason: "packageManager must be a string such as npm@11.0.0".to_owned(),
        })?;
    let (manager, version) =
        value
            .rsplit_once('@')
            .ok_or_else(|| DependencyError::InvalidConfiguration {
                provider: "javascript".to_owned(),
                path: "package.json".to_owned(),
                reason: "packageManager must pin an exact version".to_owned(),
            })?;
    if manager.is_empty() || version.is_empty() || version.contains(['^', '~', '*', '>', '<', ' '])
    {
        return Err(DependencyError::InvalidConfiguration {
            provider: "javascript".to_owned(),
            path: "package.json".to_owned(),
            reason: "packageManager must pin an exact tool version".to_owned(),
        });
    }
    Ok((Some(manager.to_owned()), Some(version.to_owned())))
}

fn reject_executable_configuration(root: &Path, manager: Manager) -> Result<(), DependencyError> {
    let files = collect_named_files(
        root,
        &[
            ".pnpmfile.cjs",
            ".pnpmfile.js",
            "pnpmfile.cjs",
            "pnpmfile.js",
        ],
    )?;
    if let Some(path) = files.first() {
        return Err(DependencyError::UnsafeConfiguration {
            provider: manager.name().to_owned(),
            path: relative_portable(root, path)?,
            reason: "pnpmfile is executable dependency policy and is never run by Shade V1"
                .to_owned(),
        });
    }
    Ok(())
}

fn sanitize_line_config(
    provider: &str,
    name: &str,
    path: &Path,
) -> Result<Vec<u8>, DependencyError> {
    let bytes = read_bytes(path, name)?;
    let source = String::from_utf8_lossy(&bytes);
    let mut safe = Vec::new();
    for raw in source.lines() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let key = line
            .split_once('=')
            .map(|(key, _)| key)
            .unwrap_or(line)
            .trim()
            .to_ascii_lowercase();
        if [
            "token",
            "password",
            "_auth",
            "authorization",
            "certfile",
            "keyfile",
        ]
        .iter()
        .any(|needle| key.contains(needle))
        {
            continue;
        }
        if [
            "script-shell",
            "shell-emulator",
            "node-options",
            "preload",
            "pnpmfile",
            "config-dependencies",
            "configdependencies",
            "use-node-version",
            "usenodeversion",
            "onload-script",
        ]
        .iter()
        .any(|needle| key.contains(needle))
            || matches!(
                key.as_str(),
                "globalconfig"
                    | "userconfig"
                    | "prefix"
                    | "modules-dir"
                    | "lockfile-dir"
                    | "logs-dir"
                    | "store-dir"
                    | "virtual-store-dir"
            )
        {
            return Err(DependencyError::UnsafeConfiguration {
                provider: provider.to_owned(),
                path: name.to_owned(),
                reason: format!(
                    "{key} can load external configuration, execute code or redirect dependency outputs"
                ),
            });
        }
        safe.push(line.to_owned());
    }
    // Preserve valid config syntax for the isolated staging environment.  The
    // fingerprint call canonicalizes these already-sanitized lines separately.
    let safe = format!("{}\n", safe.join("\n")).into_bytes();
    reject_embedded_secrets(provider, name, &safe)?;
    Ok(safe)
}

fn reject_automatic_tools_or_hooks(
    provider: &str,
    path: &str,
    value: &Value,
) -> Result<(), DependencyError> {
    fn unsafe_setting(value: &Value) -> bool {
        match value {
            Value::Object(object) => object.iter().any(|(key, value)| {
                let normalized = key.replace(['-', '_'], "").to_ascii_lowercase();
                matches!(
                    normalized.as_str(),
                    "confighooks"
                        | "configdependencies"
                        | "pnpmfile"
                        | "globalpnpmfile"
                        | "usenodeversion"
                ) || (key == "devEngines"
                    && (value.get("runtime").is_some() || value.get("packageManager").is_some()))
                    || unsafe_setting(value)
            }),
            Value::Array(values) => values.iter().any(unsafe_setting),
            Value::String(value) => value.starts_with("runtime:"),
            _ => false,
        }
    }
    if unsafe_setting(value) {
        return Err(DependencyError::UnsafeConfiguration { provider: provider.to_owned(), path: path.to_owned(),
            reason: "executable dependency hooks or automatic runtime/package-manager provisioning require removing that configuration; Shade uses installed host tools".to_owned() });
    }
    Ok(())
}

fn sanitize_bunfig(path: &Path) -> Result<Vec<u8>, DependencyError> {
    let invalid = || DependencyError::InvalidConfiguration {
        provider: "bun".to_owned(),
        path: "bunfig.toml".to_owned(),
        reason: "invalid Bun TOML configuration".to_owned(),
    };
    let mut value: toml::Value =
        toml::from_slice(&read_bytes(path, "bunfig.toml")?).map_err(|_| invalid())?;
    fn sanitize(value: &mut toml::Value) -> Result<(), DependencyError> {
        match value {
            toml::Value::Table(table) => {
                table.retain(|key, _| {
                    ![
                        "token",
                        "password",
                        "_auth",
                        "authorization",
                        "certfile",
                        "keyfile",
                    ]
                    .iter()
                    .any(|part| key.to_ascii_lowercase().contains(part))
                });
                for (key, value) in table {
                    let normalized = key.replace(['-', '_'], "").to_ascii_lowercase();
                    if matches!(
                        normalized.as_str(),
                        "preload" | "scriptshell" | "shellemulator" | "nodeoptions"
                    ) {
                        return Err(DependencyError::UnsafeConfiguration {
                            provider: "bun".to_owned(),
                            path: "bunfig.toml".to_owned(),
                            reason:
                                "Bun configuration can execute code during dependency preparation"
                                    .to_owned(),
                        });
                    }
                    sanitize(value)?;
                }
            }
            toml::Value::Array(values) => {
                for value in values {
                    sanitize(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    sanitize(&mut value)?;
    let bytes = toml::to_string(&value).map_err(|_| invalid())?.into_bytes();
    reject_embedded_secrets("bun", "bunfig.toml", &bytes)?;
    Ok(bytes)
}

fn parse_approvals(manager: Manager, lock: &[u8]) -> Result<Vec<Approval>, DependencyError> {
    let approvals = match manager {
        Manager::Npm => parse_npm_approvals(lock),
        Manager::Pnpm => parse_pnpm_approvals(lock),
        Manager::Bun => parse_bun_approvals(lock),
    }?;
    if approvals
        .iter()
        .any(|approval| !valid_registry_name(&approval.package))
    {
        return Err(DependencyError::InvalidLock {
            provider: manager.name().into(),
            path: "JavaScript lockfile".into(),
            reason: "unsafe registry package name".into(),
        });
    }
    Ok(approvals)
}

fn safe_dependency_names(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().all(|(key, value)| {
            (![
                "dependencies",
                "devDependencies",
                "optionalDependencies",
                "peerDependencies",
            ]
            .contains(&key.as_str())
                || value
                    .as_object()
                    .is_none_or(|map| map.keys().all(|name| valid_registry_name(name))))
                && safe_dependency_names(value)
        }),
        Value::Array(values) => values.iter().all(safe_dependency_names),
        _ => true,
    }
}

fn validate_lock_layout(manager: Manager, bytes: &[u8]) -> Result<(), DependencyError> {
    let invalid = || DependencyError::InvalidLock {
        provider: manager.name().into(),
        path: "JavaScript lockfile".into(),
        reason: "unsafe or invalid dependency graph paths".into(),
    };
    let value: Value = match manager {
        Manager::Npm => serde_json::from_slice(bytes).map_err(|_| invalid())?,
        Manager::Bun => json5::from_str(std::str::from_utf8(bytes).map_err(|_| invalid())?)
            .map_err(|_| invalid())?,
        Manager::Pnpm => serde_yaml_ng::from_slice(bytes).map_err(|_| invalid())?,
    };
    if !safe_dependency_names(&value) {
        return Err(invalid());
    }
    let safe_path = |value: &str| {
        !Path::new(value).is_absolute()
            && Path::new(value)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
    };
    let paths = match manager {
        Manager::Npm => "packages",
        Manager::Bun => "workspaces",
        Manager::Pnpm => "importers",
    };
    if value[paths].as_object().is_some_and(|map| {
        map.keys()
            .any(|path| !path.is_empty() && path != "." && !safe_path(path))
    }) {
        return Err(invalid());
    }
    if manager == Manager::Bun
        && value["packages"]
            .as_object()
            .is_some_and(|map| map.keys().any(|path| !safe_path(path)))
    {
        return Err(invalid());
    }
    if manager == Manager::Pnpm
        && value["snapshots"].as_object().is_some_and(|map| {
            map.keys().any(|key| {
                key.split('(')
                    .next()
                    .and_then(|base| base.rsplit_once('@'))
                    .is_none_or(|(package, version)| {
                        !valid_registry_name(package) || version.contains(['/', '\\', ':'])
                    })
            })
        })
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn valid_registry_name(name: &str) -> bool {
    let parts = name.split('/').collect::<Vec<_>>();
    (if name.starts_with('@') {
        parts.len() == 2 && parts[0].len() > 1
    } else {
        parts.len() == 1
    }) && parts.iter().all(|part| {
        !part.is_empty()
            && !matches!(*part, "." | "..")
            && !part.contains(['\\', ':'])
            && !part.bytes().any(|byte| byte <= 32 || byte == 127)
    })
}

fn parse_npm_approvals(lock: &[u8]) -> Result<Vec<Approval>, DependencyError> {
    let value: Value =
        serde_json::from_slice(lock).map_err(|error| DependencyError::InvalidLock {
            provider: "npm".to_owned(),
            path: "package-lock.json".to_owned(),
            reason: format!("invalid JSON: {error}"),
        })?;
    let packages = value
        .get("packages")
        .and_then(Value::as_object)
        .ok_or_else(|| DependencyError::InvalidLock {
            provider: "npm".to_owned(),
            path: "package-lock.json".to_owned(),
            reason: "lockfileVersion 2 or newer with a packages map is required".to_owned(),
        })?;
    let mut approvals = Vec::new();
    for (path, package) in packages {
        if path.is_empty()
            || !path.contains("node_modules/")
            || package.get("link") == Some(&Value::Bool(true))
        {
            continue;
        }
        if let Some(resolved) = package.get("resolved").and_then(Value::as_str)
            && (resolved.starts_with("file:")
                || resolved.starts_with("git+")
                || resolved.starts_with("github:"))
        {
            return Err(DependencyError::InvalidLock {
                provider: "npm".to_owned(),
                path: "package-lock.json".to_owned(),
                reason: format!("{path} uses a local or VCS source"),
            });
        }
        let package_name = package
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                path.rsplit_once("node_modules/")
                    .map(|(_, name)| name)
                    .unwrap_or(path)
            });
        let version = package.get("version").and_then(Value::as_str);
        let integrity = package.get("integrity").and_then(Value::as_str);
        let (Some(version), Some(integrity)) = (version, integrity) else {
            return Err(DependencyError::InvalidLock {
                provider: "npm".to_owned(),
                path: "package-lock.json".to_owned(),
                reason: format!("{package_name} lacks an exact version or integrity hash"),
            });
        };
        if !valid_sri(integrity) {
            return Err(DependencyError::InvalidLock {
                provider: "npm".to_owned(),
                path: "package-lock.json".to_owned(),
                reason: format!("{package_name} has a malformed integrity approval"),
            });
        }
        approvals.push(Approval {
            package: package_name.to_owned(),
            version: version.to_owned(),
            integrity: integrity.to_owned(),
        });
    }
    approvals.sort_by(|left, right| {
        (&left.package, &left.version, &left.integrity).cmp(&(
            &right.package,
            &right.version,
            &right.integrity,
        ))
    });
    approvals.dedup();
    Ok(approvals)
}

fn parse_pnpm_approvals(lock: &[u8]) -> Result<Vec<Approval>, DependencyError> {
    let invalid = |reason: String| DependencyError::InvalidLock {
        provider: "pnpm".to_owned(),
        path: "pnpm-lock.yaml".to_owned(),
        reason,
    };
    let lock: Value =
        serde_yaml_ng::from_slice(lock).map_err(|_| invalid("invalid YAML document".to_owned()))?;
    if !lock
        .get("lockfileVersion")
        .is_some_and(|value| value.is_string() || value.is_number())
    {
        return Err(invalid("lockfileVersion is missing".to_owned()));
    }
    let mut approvals = Vec::new();
    let Some(packages) = lock.get("packages").filter(|value| !value.is_null()) else {
        return Ok(approvals);
    };
    let packages = packages
        .as_object()
        .ok_or_else(|| invalid("packages must be a mapping".to_owned()))?;
    for (key, entry) in packages {
        let normalized = key.trim_start_matches('/');
        let (package, version) = normalized
            .rsplit_once('@')
            .filter(|(package, version)| {
                !package.is_empty() && !version.is_empty() && !version.contains(':')
            })
            .ok_or_else(|| invalid(format!("package entry {key} has no exact registry version")))?;
        if !valid_registry_name(package) || version.contains(['/', '\\']) {
            return Err(invalid("unsafe registry package identity".into()));
        }
        let resolution = entry
            .get("resolution")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid(format!("package entry {key} has no registry resolution")))?;
        if resolution.contains_key("directory")
            || resolution.contains_key("repo")
            || resolution.get("type").is_some()
            || resolution.get("tarball").is_some_and(|value| {
                !value.as_str().is_some_and(|value| {
                    url::Url::parse(value).is_ok_and(|url| {
                        matches!(url.scheme(), "http" | "https")
                            && url.username().is_empty()
                            && url.password().is_none()
                    })
                })
            })
        {
            return Err(invalid(format!(
                "package entry {key} uses a local or VCS source"
            )));
        }
        let integrity = resolution
            .get("integrity")
            .and_then(Value::as_str)
            .filter(|value| valid_sri(value))
            .ok_or_else(|| invalid(format!("package entry {key} lacks a valid integrity hash")))?;
        approvals.push(Approval {
            package: package.to_owned(),
            version: version.to_owned(),
            integrity: integrity.to_owned(),
        });
    }
    approvals.sort_by(|left, right| {
        (&left.package, &left.version).cmp(&(&right.package, &right.version))
    });
    Ok(approvals)
}

fn parse_bun_approvals(lock: &[u8]) -> Result<Vec<Approval>, DependencyError> {
    let invalid = |reason: String| DependencyError::InvalidLock {
        provider: "bun".to_owned(),
        path: "bun.lock".to_owned(),
        reason,
    };
    let source = std::str::from_utf8(lock)
        .map_err(|error| invalid(format!("lock is not UTF-8: {error}")))?;
    let lock: serde_json::Value =
        json5::from_str(source).map_err(|_| invalid("invalid JSONC document".to_owned()))?;
    if lock
        .get("lockfileVersion")
        .and_then(serde_json::Value::as_u64)
        .is_none()
    {
        return Err(invalid("lockfileVersion is missing".to_owned()));
    }
    let packages = lock
        .get("packages")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| invalid("package entries are missing".to_owned()))?;
    let mut approvals = Vec::new();
    let mut seen = BTreeSet::new();
    for (key, entry) in packages {
        let values = entry
            .as_array()
            .ok_or_else(|| invalid(format!("invalid package entry {key}")))?;
        let identity = values
            .first()
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid(format!("package entry {key} has no identity")))?;
        let (package, version) = identity
            .trim_start_matches("npm:")
            .rsplit_once('@')
            .filter(|(name, version)| !name.is_empty() && !version.is_empty())
            .ok_or_else(|| invalid(format!("package entry {key} has no exact version")))?;
        if let Some(path) = version.strip_prefix("workspace:") {
            let safe = !path.is_empty()
                && Path::new(path)
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_)));
            let declared_name = lock
                .get("workspaces")
                .and_then(|workspaces| workspaces.get(path))
                .and_then(|workspace| workspace.get("name"))
                .and_then(serde_json::Value::as_str);
            if !safe || declared_name != Some(package) {
                return Err(invalid(format!(
                    "{key} references an undeclared or unsafe workspace"
                )));
            }
            continue;
        }
        let integrity = values
            .get(3)
            .and_then(serde_json::Value::as_str)
            .filter(|integrity| valid_sri(integrity))
            .ok_or_else(|| {
                invalid(format!(
                    "package entry {key} lacks a valid integrity approval"
                ))
            })?;
        if version.contains(':') || version.contains('/') {
            return Err(invalid(format!(
                "package entry {key} does not use a registry version"
            )));
        }
        if seen.insert((package.to_owned(), version.to_owned(), integrity.to_owned())) {
            approvals.push(Approval {
                package: package.to_owned(),
                version: version.to_owned(),
                integrity: integrity.to_owned(),
            });
        }
    }
    approvals.sort_by(|left, right| {
        (&left.package, &left.version).cmp(&(&right.package, &right.version))
    });
    Ok(approvals)
}

fn valid_sri(integrity: &str) -> bool {
    let mut values = integrity.split_whitespace().peekable();
    values.peek().is_some()
        && values.all(|value| {
            ["sha256-", "sha384-", "sha512-"]
                .iter()
                .any(|prefix| value.starts_with(prefix) && value.len() > prefix.len())
        })
}

#[cfg(test)]
mod bun_lock_tests {
    use super::*;

    #[test]
    fn jsonc_workspaces_need_no_registry_integrity() {
        let lock = br#"{ // Bun's text lock
          "lockfileVersion":1, "workspaces":{"packages/member":{"name":"@test/member"}},
          "packages":{"@test/member":["@test/member@workspace:packages/member"]},
        }"#;
        assert!(parse_bun_approvals(lock).unwrap().is_empty());
        let invalid = String::from_utf8_lossy(lock)
            .replace("workspace:packages/member", "workspace:../outside");
        assert!(parse_bun_approvals(invalid.as_bytes()).is_err());
    }

    #[test]
    fn one_valid_package_cannot_hide_an_unhashed_package() {
        let lock = br#"{"lockfileVersion":1,"packages":{
          "good":["good@1.0.0","",{},"sha512-YWJj"],
          "bad":["bad@1.0.0","",{}]
        }}"#;
        assert!(parse_bun_approvals(lock).is_err());
    }
}

#[cfg(test)]
mod pnpm_lock_tests {
    use super::*;

    #[test]
    fn traversal_in_locked_package_names_and_importers_is_rejected_before_install() {
        let lock = b"lockfileVersion: '9.0'\npackages: { '../../../outside@1.0.0': { resolution: {integrity: sha512-YWJj} } }\n";
        assert!(parse_pnpm_approvals(lock).is_err());
        for lock in [
            "importers: { '../outside': {} }",
            "importers: { '.': { dependencies: { '../outside': { version: '1.0.0' } } } }",
            "snapshots: { '../outside@1.0.0': {} }",
        ] {
            assert!(validate_lock_layout(Manager::Pnpm, lock.as_bytes()).is_err());
        }
    }

    #[test]
    fn resolution_mapping_preserves_integrity_beside_tarball() {
        let lock = b"lockfileVersion: '9.0'\npackages: { 'fixture@1.0.0': { resolution: {integrity: sha512-YWJj, tarball: 'http://127.0.0.1:9000/fixture.tgz'} } }\n";
        let approvals = parse_pnpm_approvals(lock).unwrap();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].integrity, "sha512-YWJj");
        let local = String::from_utf8_lossy(lock).replace(
            "http://127.0.0.1:9000/fixture.tgz",
            "file:///tmp/fixture.tgz",
        );
        assert!(parse_pnpm_approvals(local.as_bytes()).is_err());
    }
}

#[cfg(test)]
mod configuration_tests {
    use super::*;

    #[test]
    fn npm_configuration_cannot_load_external_config_or_redirect_outputs() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".npmrc");
        for key in [
            "onload-script",
            "globalconfig",
            "userconfig",
            "prefix",
            "modules-dir",
            "lockfile-dir",
            "logs-dir",
            "store-dir",
            "virtual-store-dir",
        ] {
            fs::write(&config, format!("{key}=/outside/owned/staging\n")).unwrap();
            assert!(
                matches!(
                    sanitize_line_config("npm", ".npmrc", &config),
                    Err(DependencyError::UnsafeConfiguration { .. })
                ),
                "accepted {key}"
            );
        }
        fs::write(
            &config,
            "save-prefix=~\n@prefix:registry=https://registry.example.invalid\n",
        )
        .unwrap();
        assert!(sanitize_line_config("npm", ".npmrc", &config).is_ok());
    }

    #[test]
    fn structured_configuration_cannot_provision_tools_or_load_hooks() {
        for value in [
            serde_json::json!({"devEngines":{"runtime":{"name":"node","version":"22","onFail":"download"}}}),
            serde_json::json!({"devEngines":{"packageManager":{"name":"pnpm","version":"12"}}}),
            serde_json::json!({"dependencies":{"node":"runtime:22"}}),
            serde_json::json!({"pnpm":{"configDependencies":{"unreviewed-hooks":"1.0.0"}}}),
            serde_json::json!({"pnpmfile":"./hooks.cjs"}),
            serde_json::json!({"useNodeVersion":"22.0.0"}),
        ] {
            assert!(reject_automatic_tools_or_hooks("pnpm", "fixture", &value).is_err());
        }
        assert!(
            reject_automatic_tools_or_hooks(
                "pnpm",
                "fixture",
                &serde_json::json!({"packageManager":"pnpm@11.2.2","engines":{"node":">=22"}})
            )
            .is_ok()
        );
    }

    #[test]
    fn bun_inline_tables_cannot_hide_preload_and_credentials_are_omitted() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("bunfig.toml");
        for source in ["preload=['./hook.ts']", "run={preload=['./hook.ts']}"] {
            fs::write(&config, source).unwrap();
            assert!(sanitize_bunfig(&config).is_err());
        }
        fs::write(
            &config,
            "[install.registry]\nurl='https://example.test'\ntoken='fixture-private-token'\n",
        )
        .unwrap();
        let sanitized = String::from_utf8(sanitize_bunfig(&config).unwrap()).unwrap();
        assert!(!sanitized.contains("fixture-private-token"));
        assert!(sanitized.contains("https://example.test"));
    }
}
