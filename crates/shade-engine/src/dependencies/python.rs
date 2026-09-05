use super::common::{
    Approval, FingerprintBuilder, StoredReceipt, ToolIdentity, ToolIsolation, collect_named_files,
    copy_input, create_staging, identify_tool, load_receipt, materialize, native_cache,
    policy_fingerprint, promote, read_bytes, reject_embedded_secrets, relative_portable, run_tool,
    single_flight,
};
use super::{DependencyContext, DependencyError, DependencyProvider, DependencyReceipt};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const VENV_PLACEHOLDER: &str = "__SHADE_VENV__";
const PYTHON_PLACEHOLDER: &str = "__SHADE_PYTHON__";
const PYTHON_HOME_PLACEHOLDER: &str = "__SHADE_PYTHON_HOME__";
const PYTHON_LINK_PLACEHOLDER: &str = "__shade_interpreter__";

#[derive(Debug, Clone, Default)]
pub struct UvProvider {
    executable: Option<PathBuf>,
    interpreter: Option<PathBuf>,
}

impl UvProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_tools(executable: impl Into<PathBuf>, interpreter: impl Into<PathBuf>) -> Self {
        Self {
            executable: Some(executable.into()),
            interpreter: Some(interpreter.into()),
        }
    }
}

#[async_trait]
impl DependencyProvider for UvProvider {
    fn name(&self) -> &'static str {
        "uv"
    }

    fn applies(&self, repository_root: &Path) -> bool {
        repository_root.join("pyproject.toml").is_file()
    }

    async fn ensure_ready(
        &self,
        context: &DependencyContext<'_>,
    ) -> Result<DependencyReceipt, DependencyError> {
        ensure_uv(
            context,
            self.executable.as_deref(),
            self.interpreter.as_deref(),
        )
        .await
    }
}

struct PythonPlan {
    inputs: Vec<PathBuf>,
    approvals: Vec<Approval>,
    expected_python: Option<String>,
}

impl PythonPlan {
    fn inspect(root: &Path) -> Result<Self, DependencyError> {
        let pyproject = root.join("pyproject.toml");
        let lock = root.join("uv.lock");
        if !lock.is_file() {
            return Err(DependencyError::LockMissing(
                "uv requires a committed uv.lock next to pyproject.toml; run `uv lock` and commit it"
                    .to_owned(),
            ));
        }
        let forbidden_names = [
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "requirements-dev.txt",
            "Pipfile",
            "Pipfile.lock",
            "poetry.lock",
            "pdm.lock",
            "uv.toml",
            ".uv.toml",
        ];
        if let Some(forbidden) = collect_named_files(root, &forbidden_names)?.first() {
            return Err(DependencyError::InvalidConfiguration {
                provider: "uv".to_owned(),
                path: relative_portable(root, forbidden)?,
                reason: "Shade V1 accepts only pyproject.toml + uv.lock; place reviewed uv settings in pyproject.toml"
                    .to_owned(),
            });
        }

        let pyproject_bytes = read_bytes(&pyproject, "pyproject.toml")?;
        let project = validate_pyproject(&pyproject_bytes)?;
        let project_name = project
            .get("project")
            .and_then(|value| value.get("name"))
            .and_then(toml::Value::as_str);
        let lock_bytes = read_bytes(&lock, "uv.lock")?;
        let approvals = parse_uv_lock(&lock_bytes, project_name)?;
        let expected_python = if root.join(".python-version").is_file() {
            let value = String::from_utf8_lossy(&read_bytes(
                &root.join(".python-version"),
                ".python-version",
            )?)
            .trim()
            .to_owned();
            if value.is_empty()
                || value.contains(char::is_whitespace)
                || value
                    .chars()
                    .any(|character| matches!(character, '^' | '~' | '*' | '>' | '<'))
            {
                return Err(DependencyError::InvalidConfiguration {
                    provider: "uv".to_owned(),
                    path: ".python-version".to_owned(),
                    reason: "an exact Python version is required".to_owned(),
                });
            }
            Some(value)
        } else {
            None
        };

        let mut inputs = collect_named_files(root, &["pyproject.toml"])?;
        inputs.push(lock);
        if root.join(".python-version").is_file() {
            inputs.push(root.join(".python-version"));
        }
        inputs.sort_by_key(|path| relative_portable(root, path).unwrap_or_default());
        inputs.dedup();
        for input in &inputs {
            let relative = relative_portable(root, input)?;
            reject_embedded_secrets("uv", &relative, &read_bytes(input, &relative)?)?;
        }
        Ok(Self {
            inputs,
            approvals,
            expected_python,
        })
    }

    fn fingerprint(
        &self,
        root: &Path,
        uv: &ToolIdentity,
        python: &ToolIdentity,
    ) -> Result<String, DependencyError> {
        let mut fingerprint = FingerprintBuilder::new("uv");
        fingerprint.tool(uv);
        fingerprint.field("interpreter-name", python.logical_name.as_bytes());
        fingerprint.field("interpreter-path-identity", python.path_identity.as_bytes());
        fingerprint.field("interpreter-version", python.version.as_bytes());
        fingerprint.field("interpreter-digest", python.digest.as_bytes());
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
            "policy:wheel-approvals",
            &serde_json::to_vec(&self.approvals).map_err(|error| {
                DependencyError::Failed(format!("failed to encode wheel approvals: {error}"))
            })?,
        );
        fingerprint.field(
            "policy:python",
            b"wheel-only;no-project;no-editable;no-vcs;no-path;no-sdist;offline-replay;no-system-config;no-keyring-v1",
        );
        policy_fingerprint(root, &mut fingerprint)?;
        Ok(fingerprint.finish())
    }

    fn stage_inputs(&self, repository_root: &Path, stage: &Path) -> Result<(), DependencyError> {
        for input in &self.inputs {
            copy_input(repository_root, input, stage)?;
        }
        Ok(())
    }
}

async fn ensure_uv(
    context: &DependencyContext<'_>,
    uv_path: Option<&Path>,
    interpreter_path: Option<&Path>,
) -> Result<DependencyReceipt, DependencyError> {
    let plan = PythonPlan::inspect(context.repository_root)?;
    let uv = identify_tool("uv", uv_path)?;
    require_uv_config_isolation(&uv.version)?;
    let interpreter_name = if interpreter_path.is_some() {
        "python"
    } else if super::command_path("python3").is_ok() {
        "python3"
    } else {
        "python"
    };
    let python = identify_tool(interpreter_name, interpreter_path)?;
    if let Some(expected) = &plan.expected_python
        && !python
            .version
            .split_whitespace()
            .any(|part| part == expected)
    {
        return Err(DependencyError::ToolVersionMismatch {
            tool: "python".to_owned(),
            expected: expected.clone(),
            actual: python.version.clone(),
        });
    }
    let fingerprint = plan.fingerprint(context.repository_root, &uv, &python)?;
    let _flight = single_flight(format!("uv:{fingerprint}")).await;
    if let Some(receipt) = load_receipt(context, "uv", &fingerprint)? {
        materialize(context, &receipt)?;
        specialize_venv(&context.workspace_root.join(".venv"), &python.path)?;
        return Ok(receipt.public());
    }

    let native = native_cache(context, "uv")?;
    let fill = create_staging(context, "uv", "fill")?;
    plan.stage_inputs(context.repository_root, fill.path())?;
    let baseline = run_uv(&uv, &python, fill.path(), &native, false)?;
    crate::faults::hit(crate::faults::Point::DependencyFilled);
    validate_venv(fill.path(), "online fill", &baseline)?;
    crate::faults::hit(crate::faults::Point::DependencyFillValidated);

    let replay = create_staging(context, "uv", "replay")?;
    plan.stage_inputs(context.repository_root, replay.path())?;
    let baseline = run_uv(&uv, &python, replay.path(), &native, true)?;
    crate::faults::hit(crate::faults::Point::DependencyReplayed);
    validate_venv(replay.path(), "offline replay", &baseline)?;
    make_venv_relocatable(&replay.path().join(".venv"), &python.path)?;
    crate::faults::hit(crate::faults::Point::DependencyReplayValidated);
    let outputs = vec![(replay.path().join(".venv"), ".venv".to_owned())];
    let receipt = StoredReceipt::new(
        "uv",
        fingerprint,
        vec![".venv".to_owned()],
        vec![
            "project installation".to_owned(),
            "sdist and source builds".to_owned(),
            "editable, VCS, path and local dependencies".to_owned(),
        ],
        plan.approvals.clone(),
        &uv,
    )
    .with_tool(&python);
    promote(context, &receipt, &outputs)?;
    materialize(context, &receipt)?;
    specialize_venv(&context.workspace_root.join(".venv"), &python.path)?;
    Ok(receipt.public())
}

#[derive(Clone)]
enum BootstrapEntry {
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn require_uv_config_isolation(version: &str) -> Result<(), DependencyError> {
    // UV_NO_SYSTEM_CONFIG was introduced in 0.11.16. Older tools silently
    // ignore that environment variable, so require its supported stable API.
    let numeric = version
        .strip_prefix("uv ")
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| {
            let mut parts = value.split('.');
            let tuple = (
                parts.next()?.parse::<u64>().ok()?,
                parts.next()?.parse::<u64>().ok()?,
                parts.next()?.parse::<u64>().ok()?,
            );
            parts.next().is_none().then_some(tuple)
        });
    if numeric.is_some_and(|version| version >= (0, 11, 16)) {
        return Ok(());
    }
    Err(DependencyError::ToolVersionMismatch {
        tool: "uv".into(),
        expected: ">=0.11.16 stable (system configuration isolation)".into(),
        actual: version.into(),
    })
}

fn run_uv(
    uv: &ToolIdentity,
    python: &ToolIdentity,
    stage: &Path,
    native_cache: &Path,
    offline: bool,
) -> Result<BTreeMap<PathBuf, BootstrapEntry>, DependencyError> {
    let home = stage.join(".home");
    let temp = stage.join(".tmp");
    fs::create_dir_all(&home).map_err(|error| {
        DependencyError::Failed(format!("failed to create isolated uv home: {error}"))
    })?;
    fs::create_dir_all(&temp).map_err(|error| {
        DependencyError::Failed(format!(
            "failed to create isolated uv temp directory: {error}"
        ))
    })?;
    let mut args = [
        "sync",
        "--frozen",
        "--no-install-project",
        "--no-install-workspace",
        "--no-editable",
        "--no-build",
        "--no-python-downloads",
        "--python",
    ]
    .into_iter()
    .map(OsString::from)
    .collect::<Vec<_>>();
    args.push(python.path.as_os_str().to_owned());
    args.push("--cache-dir".into());
    args.push(native_cache.as_os_str().to_owned());
    if offline {
        args.push("--offline".into());
    }
    let env = vec![
        (OsString::from("HOME"), home.into_os_string()),
        (OsString::from("TMPDIR"), temp.into_os_string()),
        (
            OsString::from("UV_PROJECT_ENVIRONMENT"),
            OsString::from(".venv"),
        ),
        (
            OsString::from("UV_PYTHON"),
            python.path.as_os_str().to_owned(),
        ),
        (
            OsString::from("UV_PYTHON_DOWNLOADS"),
            OsString::from("never"),
        ),
        (OsString::from("UV_NO_BUILD"), OsString::from("1")),
        (OsString::from("UV_NO_BUILD_ISOLATION"), OsString::from("1")),
        (OsString::from("UV_NO_SYSTEM_CONFIG"), OsString::from("1")),
        (
            OsString::from("UV_KEYRING_PROVIDER"),
            OsString::from("disabled"),
        ),
    ];
    // Establish a clean, exact-tool baseline before any wheel is installed.
    // This command cannot discover project config, resolve packages or fetch Python.
    run_tool(
        "uv",
        "empty environment",
        uv,
        stage,
        [
            OsString::from("venv"),
            OsString::from("--no-config"),
            OsString::from("--offline"),
            OsString::from("--no-python-downloads"),
            OsString::from("--python"),
            python.path.as_os_str().to_owned(),
            OsString::from(".venv"),
        ],
        &env,
        ToolIsolation::network(true),
    )?;
    crate::faults::hit(if offline {
        crate::faults::Point::PythonReplayBootstrapCreated
    } else {
        crate::faults::Point::PythonFillBootstrapCreated
    });
    let mut baseline = BTreeMap::new();
    for entry in WalkDir::new(stage.join(".venv")).follow_links(false) {
        let entry = entry.map_err(|error| {
            DependencyError::Failed(format!("could not inspect empty venv: {error}"))
        })?;
        if entry.file_type().is_file() {
            let relative = entry
                .path()
                .strip_prefix(stage)
                .expect("venv is inside stage")
                .to_path_buf();
            let bytes = fs::read(entry.path()).map_err(|error| {
                DependencyError::Failed(format!("could not read empty venv: {error}"))
            })?;
            baseline.insert(relative, BootstrapEntry::File(bytes));
        } else if entry.file_type().is_symlink() {
            let relative = entry
                .path()
                .strip_prefix(stage)
                .expect("venv is inside stage")
                .to_path_buf();
            let target = fs::read_link(entry.path()).map_err(|error| {
                DependencyError::Failed(format!("could not read bootstrap symlink: {error}"))
            })?;
            baseline.insert(relative, BootstrapEntry::Symlink(target));
        }
    }
    crate::faults::hit(if offline {
        crate::faults::Point::PythonReplayBaselineCaptured
    } else {
        crate::faults::Point::PythonFillBaselineCaptured
    });
    run_tool(
        "uv",
        if offline {
            "offline replay"
        } else {
            "online wheel fill"
        },
        uv,
        stage,
        args,
        &env,
        ToolIsolation::network(offline),
    )?;
    Ok(baseline)
}

fn validate_pyproject(bytes: &[u8]) -> Result<toml::Table, DependencyError> {
    let project: toml::Table =
        toml::from_slice(bytes).map_err(|_| DependencyError::InvalidConfiguration {
            provider: "uv".to_owned(),
            path: "pyproject.toml".to_owned(),
            reason: "invalid TOML document".to_owned(),
        })?;
    fn unsafe_source(value: &toml::Value) -> bool {
        match value {
            toml::Value::Table(table) => table.iter().any(|(key, value)| {
                matches!(
                    key.as_str(),
                    "git" | "vcs" | "path" | "directory" | "editable" | "workspace"
                ) || (key == "keyring-provider" && value.as_str() != Some("disabled"))
                    || (key == "url" && !value.as_str().is_some_and(remote_url))
                    || unsafe_source(value)
            }),
            toml::Value::Array(values) => values.iter().any(unsafe_source),
            _ => false,
        }
    }
    if project
        .get("tool")
        .and_then(|tool| tool.get("uv"))
        .is_some_and(unsafe_source)
    {
        return Err(DependencyError::UnsafeConfiguration {
            provider: "uv".to_owned(),
            path: "pyproject.toml".to_owned(),
            reason: "tool.uv contains a VCS, editable, workspace, path, local source or executable credential provider".to_owned(),
        });
    }
    Ok(project)
}

fn remote_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn parse_uv_lock(
    lock: &[u8],
    project_name: Option<&str>,
) -> Result<Vec<Approval>, DependencyError> {
    let invalid = |reason: String| DependencyError::InvalidLock {
        provider: "uv".to_owned(),
        path: "uv.lock".to_owned(),
        reason,
    };
    let lock: toml::Table =
        toml::from_slice(lock).map_err(|_| invalid("invalid TOML document".to_owned()))?;
    if lock
        .get("version")
        .and_then(toml::Value::as_integer)
        .is_none()
    {
        return Err(invalid("uv lock version is missing".to_owned()));
    }
    let packages = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| invalid("package entries are missing".to_owned()))?;
    let mut approvals = Vec::new();
    for package in packages {
        let name = package
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| invalid("package entry has no name".to_owned()))?;
        let source = package
            .get("source")
            .and_then(toml::Value::as_table)
            .ok_or_else(|| invalid(format!("{name} has no source")))?;
        let is_root = project_name == Some(name)
            && source.len() == 1
            && ["editable", "virtual"]
                .iter()
                .any(|key| source.get(*key).and_then(toml::Value::as_str) == Some("."));
        if !is_root
            && (source.len() != 1
                || !source.iter().all(|(key, value)| {
                    matches!(key.as_str(), "registry" | "url")
                        && value.as_str().is_some_and(remote_url)
                }))
        {
            return Err(invalid(format!(
                "{name} uses a VCS, editable, path or local source"
            )));
        }
        if package.get("sdist").is_some() {
            return Err(invalid(format!(
                "{name} includes an sdist; wheel-only locks are required"
            )));
        }
        if is_root {
            continue;
        }
        let version = package
            .get("version")
            .and_then(toml::Value::as_str)
            .filter(|version| !version.is_empty())
            .ok_or_else(|| invalid(format!("{name} has no exact version")))?;
        let wheels = package
            .get("wheels")
            .and_then(toml::Value::as_array)
            .filter(|wheels| !wheels.is_empty())
            .ok_or_else(|| invalid(format!("{name} {version} has no wheel artifacts")))?;
        let mut hashes = Vec::new();
        for wheel in wheels {
            let hash = wheel
                .get("hash")
                .and_then(toml::Value::as_str)
                .unwrap_or_default();
            let digest = hash.strip_prefix("sha256:").unwrap_or_default();
            if !wheel
                .get("url")
                .and_then(toml::Value::as_str)
                .is_some_and(remote_url)
                || digest.len() != 64
                || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(invalid(format!(
                    "{name} {version} has a local wheel or lacks valid SHA-256 wheel hashes"
                )));
            }
            hashes.push(hash.to_owned());
        }
        hashes.sort();
        hashes.dedup();
        approvals.push(Approval {
            package: name.to_owned(),
            version: version.to_owned(),
            integrity: hashes.join("+"),
        });
    }
    approvals.sort_by(|left, right| {
        (&left.package, &left.version).cmp(&(&right.package, &right.version))
    });
    Ok(approvals)
}

fn validate_venv(
    stage: &Path,
    phase: &str,
    baseline: &BTreeMap<PathBuf, BootstrapEntry>,
) -> Result<(), DependencyError> {
    for (relative, expected) in baseline {
        let path = stage.join(relative);
        let matches = match expected {
            BootstrapEntry::File(bytes) => {
                fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file())
                    && fs::read(&path).as_deref().ok() == Some(bytes.as_slice())
            }
            BootstrapEntry::Symlink(target) => fs::read_link(&path).as_ref().ok() == Some(target),
        };
        if !matches {
            return Err(DependencyError::Validation {
                provider: "uv".to_owned(),
                reason: format!(
                    "installed environment modified bootstrap file: {}",
                    relative.display()
                ),
            });
        }
    }
    let venv = stage.join(".venv");
    if !venv.is_dir() {
        return Err(DependencyError::Validation {
            provider: "uv".to_owned(),
            reason: format!("{phase} did not produce .venv"),
        });
    }
    for entry in WalkDir::new(&venv).follow_links(false) {
        let entry = entry.map_err(|error| DependencyError::Validation {
            provider: "uv".to_owned(),
            reason: format!("could not inspect {phase} .venv: {error}"),
        })?;
        let name = entry.file_name().to_string_lossy();
        let startup_bytecode = matches!(
            entry.path().extension().and_then(OsStr::to_str),
            Some("pyc" | "pyo")
        ) && ["sitecustomize", "usercustomize", "_virtualenv"]
            .iter()
            .any(|prefix| name.starts_with(prefix));
        if (entry.file_type().is_file() || entry.file_type().is_symlink())
            && (startup_bytecode
                || entry.path().extension() == Some(OsStr::new("pth"))
                || matches!(
                    entry.file_name().to_str(),
                    Some("sitecustomize.py" | "usercustomize.py")
                ))
        {
            if baseline.contains_key(
                entry
                    .path()
                    .strip_prefix(stage)
                    .expect("venv is inside stage"),
            ) {
                continue;
            }
            return Err(DependencyError::Validation {
                provider: "uv".to_owned(),
                reason: format!(
                    "installed environment contains Python startup code: {}",
                    entry
                        .path()
                        .strip_prefix(&venv)
                        .unwrap_or(entry.path())
                        .display()
                ),
            });
        }
        if entry.file_type().is_file() && entry.file_name() == OsStr::new("direct_url.json") {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(entry.path()).map_err(|error| {
                    DependencyError::Validation {
                        provider: "uv".to_owned(),
                        reason: format!("could not inspect direct_url.json: {error}"),
                    }
                })?)
                .map_err(|error| DependencyError::Validation {
                    provider: "uv".to_owned(),
                    reason: format!("invalid direct_url.json: {error}"),
                })?;
            let url = value
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if url.starts_with("file:")
                || value.get("vcs_info").is_some()
                || value
                    .get("dir_info")
                    .and_then(|value| value.get("editable"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            {
                return Err(DependencyError::Validation {
                    provider: "uv".to_owned(),
                    reason: "installed environment contains a local, editable or VCS distribution"
                        .to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn make_venv_relocatable(venv: &Path, interpreter: &Path) -> Result<(), DependencyError> {
    let venv_text = venv.to_string_lossy();
    let interpreter_text = interpreter.to_string_lossy();
    let interpreter_home = interpreter
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .to_string_lossy();
    for entry in WalkDir::new(venv).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|error| DependencyError::Validation {
            provider: "uv".to_owned(),
            reason: format!("could not normalize .venv: {error}"),
        })?;
        if entry.file_type().is_symlink() {
            let target =
                fs::read_link(entry.path()).map_err(|error| DependencyError::Validation {
                    provider: "uv".to_owned(),
                    reason: format!("could not read .venv symlink: {error}"),
                })?;
            if target.is_absolute() {
                let name = entry.file_name().to_string_lossy();
                if target == interpreter || name.starts_with("python") {
                    fs::remove_file(entry.path()).map_err(|error| {
                        DependencyError::Failed(format!(
                            "failed to normalize Python symlink: {error}"
                        ))
                    })?;
                    create_symlink(Path::new(PYTHON_LINK_PLACEHOLDER), entry.path())?;
                    crate::faults::hit(crate::faults::Point::PythonReplayEntryRelocated);
                } else {
                    return Err(DependencyError::Validation {
                        provider: "uv".to_owned(),
                        reason: format!(
                            "absolute symlink {} makes .venv non-relocatable",
                            entry
                                .path()
                                .strip_prefix(venv)
                                .unwrap_or(entry.path())
                                .display()
                        ),
                    });
                }
            }
        } else if entry.file_type().is_file() {
            let bytes = fs::read(entry.path()).map_err(|error| {
                DependencyError::Failed(format!(
                    "failed to read .venv file during relocation: {error}"
                ))
            })?;
            if let Ok(text) = std::str::from_utf8(&bytes) {
                let mut replaced = text
                    .replace(venv_text.as_ref(), VENV_PLACEHOLDER)
                    .replace(interpreter_text.as_ref(), PYTHON_PLACEHOLDER);
                if interpreter_home.len() > 1 {
                    replaced = replaced.replace(interpreter_home.as_ref(), PYTHON_HOME_PLACEHOLDER);
                }
                if replaced.as_bytes() != bytes {
                    fs::write(entry.path(), replaced).map_err(|error| {
                        DependencyError::Failed(format!("failed to normalize .venv file: {error}"))
                    })?;
                    crate::faults::hit(crate::faults::Point::PythonReplayEntryRelocated);
                }
            }
        }
    }
    crate::faults::hit(crate::faults::Point::PythonReplayRelocated);
    Ok(())
}

/// Retarget location-dependent activation scripts without rebuilding or replacing
/// agent-edited packages. The source environment is never opened for writing.
pub(super) fn relocate_forked_venv(venv: &Path, source: &Path) -> Result<(), DependencyError> {
    let metadata = fs::symlink_metadata(venv).map_err(|error| {
        DependencyError::Failed(format!("cannot inspect forked .venv: {error}"))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(DependencyError::Policy(
            "forked .venv must be a workspace-owned directory".into(),
        ));
    }
    let source_text = source.to_string_lossy();
    let destination_text = venv.to_string_lossy();
    for entry in WalkDir::new(venv).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|error| {
            DependencyError::Failed(format!("cannot inspect forked .venv entry: {error}"))
        })?;
        if entry.file_type().is_symlink() {
            let target = fs::read_link(entry.path()).map_err(|error| {
                DependencyError::Failed(format!("cannot read forked Python link: {error}"))
            })?;
            if let Ok(relative) = target.strip_prefix(source) {
                fs::remove_file(entry.path()).map_err(|error| {
                    DependencyError::Failed(format!("cannot relocate forked Python link: {error}"))
                })?;
                create_symlink(&venv.join(relative), entry.path())?;
                crate::faults::hit(crate::faults::Point::PythonForkEntryRelocated);
            }
        } else if entry.file_type().is_file() {
            let bytes = fs::read(entry.path()).map_err(|error| {
                DependencyError::Failed(format!("cannot read forked Python file: {error}"))
            })?;
            if let Ok(text) = std::str::from_utf8(&bytes) {
                let relocated = text.replace(source_text.as_ref(), destination_text.as_ref());
                if relocated.as_bytes() != bytes {
                    fs::write(entry.path(), relocated).map_err(|error| {
                        DependencyError::Failed(format!(
                            "cannot relocate forked Python file: {error}"
                        ))
                    })?;
                    crate::faults::hit(crate::faults::Point::PythonForkEntryRelocated);
                }
            }
        }
    }
    crate::faults::hit(crate::faults::Point::PythonForkRelocated);
    Ok(())
}

fn specialize_venv(venv: &Path, interpreter: &Path) -> Result<(), DependencyError> {
    if !venv.is_dir() {
        return Err(DependencyError::Validation {
            provider: "uv".to_owned(),
            reason: "materialized .venv is missing".to_owned(),
        });
    }
    let venv_text = venv.to_string_lossy();
    let interpreter_text = interpreter.to_string_lossy();
    let interpreter_home = interpreter
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .to_string_lossy();
    for entry in WalkDir::new(venv).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|error| {
            DependencyError::Failed(format!("failed to specialize .venv: {error}"))
        })?;
        if entry.file_type().is_symlink()
            && fs::read_link(entry.path()).ok().as_deref()
                == Some(Path::new(PYTHON_LINK_PLACEHOLDER))
        {
            fs::remove_file(entry.path()).map_err(|error| {
                DependencyError::Failed(format!("failed to replace Python symlink: {error}"))
            })?;
            create_symlink(interpreter, entry.path())?;
            crate::faults::hit(crate::faults::Point::PythonWorkspaceEntrySpecialized);
        } else if entry.file_type().is_file() {
            let bytes = fs::read(entry.path()).map_err(|error| {
                DependencyError::Failed(format!("failed to read materialized .venv file: {error}"))
            })?;
            if let Ok(text) = std::str::from_utf8(&bytes) {
                let replaced = text
                    .replace(VENV_PLACEHOLDER, venv_text.as_ref())
                    .replace(PYTHON_PLACEHOLDER, interpreter_text.as_ref())
                    .replace(PYTHON_HOME_PLACEHOLDER, interpreter_home.as_ref());
                if replaced.as_bytes() != bytes {
                    fs::write(entry.path(), replaced).map_err(|error| {
                        DependencyError::Failed(format!("failed to specialize .venv file: {error}"))
                    })?;
                }
            }
        }
    }
    crate::faults::hit(crate::faults::Point::PythonWorkspaceSpecialized);
    Ok(())
}

fn create_symlink(source: &Path, destination: &Path) -> Result<(), DependencyError> {
    std::os::unix::fs::symlink(source, destination).map_err(|error| {
        DependencyError::Failed(format!("failed to create Python symlink: {error}"))
    })
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn uv_requires_system_config_isolation_and_disables_executable_keyrings() {
        for version in ["uv 0.11.15", "uv 0.8.0", "uv 0.11.16-preview", "unknown"] {
            assert!(matches!(
                require_uv_config_isolation(version),
                Err(DependencyError::ToolVersionMismatch { .. })
            ));
        }
        for version in ["uv 0.11.16", "uv 0.12.6 (Homebrew)", "uv 1.0.0"] {
            assert!(require_uv_config_isolation(version).is_ok());
        }
        assert!(validate_pyproject(b"[tool.uv]\nkeyring-provider='subprocess'\n").is_err());
        assert!(validate_pyproject(b"[tool.uv]\nkeyring-provider='disabled'\n").is_ok());
    }

    #[test]
    fn quoted_and_compact_toml_cannot_hide_local_sources() {
        let project = validate_pyproject(b"project={name='fixture',version='1'}").unwrap();
        assert_eq!(project["project"]["name"].as_str(), Some("fixture"));
        for source in [
            "{path='../outside'}",
            "{editable=true}",
            "{url='file:///tmp/wheel'}",
            "[{git='https://example/repo'}]",
        ] {
            let project = format!("[tool.uv.sources]\n'unsafe'={source}\n");
            assert!(validate_pyproject(project.as_bytes()).is_err(), "{source}");
        }
        let root = b"version=1\n[[package]]\nname='fixture'\nversion='1'\nsource={editable='.'}\n";
        assert!(parse_uv_lock(root, Some("fixture")).unwrap().is_empty());
        assert!(parse_uv_lock(root, Some("other")).is_err());
    }

    #[test]
    fn every_wheel_requires_a_remote_url_and_complete_hash() {
        let valid = format!(
            "version=1\n[[package]]\nname='dep'\nversion='1'\nsource={{registry='https://pypi.org/simple'}}\nwheels=[{{url='https://example/dep.whl',hash='sha256:{}'}}]\n",
            "a".repeat(64)
        );
        assert_eq!(parse_uv_lock(valid.as_bytes(), None).unwrap().len(), 1);
        for invalid in [
            valid.replace("https://example/dep.whl", "file:///tmp/dep.whl"),
            valid.replace(&"a".repeat(64), "bad"),
            valid.replace("wheels=[", "wheels=[{url='https://example/unhashed.whl'},"),
        ] {
            assert!(parse_uv_lock(invalid.as_bytes(), None).is_err());
        }
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;

    #[test]
    fn wheel_cannot_replace_the_bootstrap_interpreter_link() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join(".venv/bin");
        fs::create_dir_all(&bin).unwrap();
        let python = bin.join("python");
        let target = PathBuf::from("/usr/bin/python3");
        std::os::unix::fs::symlink(&target, &python).unwrap();
        let baseline = BTreeMap::from([(
            PathBuf::from(".venv/bin/python"),
            BootstrapEntry::Symlink(target),
        )]);
        validate_venv(temp.path(), "empty environment", &baseline).unwrap();
        fs::remove_file(&python).unwrap();
        fs::write(&python, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(
            validate_venv(temp.path(), "wheel installation", &baseline)
                .unwrap_err()
                .to_string()
                .contains("bootstrap file")
        );
    }
}
