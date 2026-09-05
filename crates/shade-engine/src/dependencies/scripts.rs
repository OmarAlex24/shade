//! Explicit lifecycle execution over fresh, isolated package contents.
//! Repository approval metadata is never consulted here.
use super::DependencyError;
use super::common::{
    Approval, FingerprintBuilder, ToolIdentity, ToolIsolation, identify_tool_at, run_tool,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use shade_protocol::{DependencyScript, ScriptApproval};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(super) struct ScriptTools {
    pub node: ToolIdentity,
    pub shell: ToolIdentity,
    pub libraries_digest: String,
    readable_libraries: BTreeSet<PathBuf>,
}

impl ScriptTools {
    pub fn identify(root: &Path) -> Result<Self, DependencyError> {
        let node = identify_tool_at("node", None, root, &[])?;
        // Ask the exact installed runtime for its loaded images. No repository
        // code, ambient NODE_OPTIONS/DYLD variables or network access is used.
        let output = run_tool(
            "scripts",
            "identify runtime libraries",
            &node,
            root,
            [
                "--input-type=commonjs",
                "-e",
                "process.stdout.write(JSON.stringify(process.report.getReport().sharedObjects))",
            ],
            &[("OPENSSL_CONF".into(), "/dev/null".into())],
            ToolIsolation::network(true),
        )?;
        let images: Vec<PathBuf> = serde_json::from_slice(&output.stdout)
            .map_err(|_| invalid("scripts", "Node did not report runtime libraries"))?;
        let mut readable_libraries = BTreeSet::new();
        for image in images {
            if !image.is_absolute() {
                return Err(invalid(
                    "scripts",
                    "Node reported a relative runtime library",
                ));
            }
            // These roots are already readable in the system sandbox profile.
            // macOS shared-cache images need not exist as standalone files.
            if ["/System", "/usr", "/bin", "/sbin", "/Library/Developer"]
                .iter()
                .any(|root| image.starts_with(root))
            {
                continue;
            }
            let image = fs::canonicalize(image)
                .map_err(|_| invalid("scripts", "cannot resolve Node runtime library"))?;
            if image != node.path {
                readable_libraries.insert(image);
            }
        }
        let mut identities = Vec::new();
        for image in &readable_libraries {
            let name = image
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or_else(|| invalid("scripts", "invalid Node runtime library name"))?;
            let mut file = fs::File::open(image)
                .map_err(|_| invalid("scripts", "cannot read Node runtime library"))?;
            let mut hash = Sha256::new();
            let mut buffer = [0; 65536];
            loop {
                let count = file
                    .read(&mut buffer)
                    .map_err(|_| invalid("scripts", "cannot hash Node runtime library"))?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            identities.push((name.to_owned(), hex::encode(hash.finalize())));
        }
        // Portable inputs contain names and contents, never installation paths.
        identities.sort();
        let mut libraries = FingerprintBuilder::new("node-runtime-libraries");
        for (name, digest) in identities {
            libraries.field(&name, digest.as_bytes());
        }
        Ok(Self {
            node,
            shell: identify_tool_at("sh", Some(Path::new("/bin/sh")), root, &[])?,
            libraries_digest: libraries.finish(),
            readable_libraries,
        })
    }
}

pub(super) struct InstalledScript {
    pub root: PathBuf,
    pub report: DependencyScript,
    commands: Vec<(String, String)>,
    dependencies: BTreeSet<String>,
}

fn invalid(provider: &str, reason: &str) -> DependencyError {
    DependencyError::Validation {
        provider: provider.into(),
        reason: reason.into(),
    }
}

fn approval(provider: &str, identity: &Approval) -> ScriptApproval {
    ScriptApproval {
        provider: provider.into(),
        package: identity.package.clone(),
        version: identity.version.clone(),
        integrity: identity.integrity.clone(),
    }
}

/// Bind installation paths to the lock, rather than trusting names supplied
/// by a tarball's package.json. Workspace packages are deliberately absent.
pub(super) fn inventory(
    provider: &str,
    stage: &Path,
    lock: &[u8],
    identities: &[Approval],
) -> Result<Vec<InstalledScript>, DependencyError> {
    let mut bindings = BTreeMap::new();
    match provider {
        "npm" => {
            let lock: Value =
                serde_json::from_slice(lock).map_err(|_| invalid(provider, "invalid npm lock"))?;
            for (path, package) in lock["packages"]
                .as_object()
                .ok_or_else(|| invalid(provider, "missing npm package map"))?
            {
                if path.is_empty() || package["link"] == true || !path.contains("node_modules/") {
                    continue;
                }
                let name = package["name"]
                    .as_str()
                    .or_else(|| path.rsplit_once("node_modules/").map(|(_, name)| name))
                    .ok_or_else(|| invalid(provider, "missing package name"))?;
                let identity = identities
                    .iter()
                    .find(|identity| {
                        identity.package == name
                            && package["version"] == identity.version
                            && package["integrity"] == identity.integrity
                    })
                    .ok_or_else(|| invalid(provider, "package path has no locked identity"))?;
                bindings.insert(path.clone(), approval(provider, identity));
            }
        }
        "bun" => {
            let lock: Value = json5::from_str(
                std::str::from_utf8(lock).map_err(|_| invalid(provider, "invalid Bun lock"))?,
            )
            .map_err(|_| invalid(provider, "invalid Bun lock"))?;
            let workspaces = lock["workspaces"]
                .as_object()
                .into_iter()
                .flat_map(|map| map.iter())
                .filter(|(path, _)| !path.is_empty())
                .filter_map(|(path, value)| {
                    value["name"]
                        .as_str()
                        .map(|name| (name.to_owned(), path.to_owned()))
                })
                .collect::<BTreeMap<_, _>>();
            for (key, value) in lock["packages"]
                .as_object()
                .ok_or_else(|| invalid(provider, "missing Bun package map"))?
            {
                let Some((name, version)) =
                    value[0].as_str().and_then(|value| value.rsplit_once('@'))
                else {
                    continue;
                };
                if version.starts_with("workspace:") {
                    continue;
                }
                let identity = identities
                    .iter()
                    .find(|identity| {
                        identity.package == name
                            && identity.version == version
                            && value[3] == identity.integrity
                    })
                    .ok_or_else(|| invalid(provider, "package path has no locked identity"))?;
                bindings.insert(
                    bun_install_path(key, &workspaces)
                        .ok_or_else(|| invalid(provider, "invalid Bun package path"))?,
                    approval(provider, identity),
                );
            }
        }
        "pnpm" => {
            bindings = pnpm_bindings(stage, lock, identities)?;
        }
        _ => return Err(invalid(provider, "unsupported script provider")),
    }
    let mut scripts = Vec::new();
    for (relative, approval) in bindings {
        if Path::new(&relative).is_absolute()
            || Path::new(&relative)
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(invalid(provider, "unsafe installed package path"));
        }
        let root = stage.join(&relative);
        let metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue, // OS-specific optional dependency.
            Err(_) => return Err(invalid(provider, "cannot inspect installed package")),
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid(
                provider,
                "registry package directory is not regular",
            ));
        }
        let canonical =
            fs::canonicalize(&root).map_err(|_| invalid(provider, "invalid package root"))?;
        if !canonical
            .starts_with(fs::canonicalize(stage).map_err(|_| invalid(provider, "invalid stage"))?)
        {
            return Err(invalid(provider, "package escapes its stage"));
        }
        let manifest = root.join("package.json");
        if !manifest.exists() {
            continue;
        }
        if fs::symlink_metadata(&manifest).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(invalid(provider, "package manifest is a symlink"));
        }
        let value: Value = serde_json::from_slice(
            &fs::read(&manifest).map_err(|_| invalid(provider, "missing package manifest"))?,
        )
        .map_err(|_| invalid(provider, "invalid package manifest"))?;
        if value["name"] != approval.package || value["version"] != approval.version {
            return Err(invalid(
                provider,
                "installed package identity differs from its locked path",
            ));
        }
        let mut commands = Vec::new();
        for event in ["preinstall", "install", "postinstall"] {
            if let Some(script) = value["scripts"][event]
                .as_str()
                .filter(|script| !script.trim().is_empty())
            {
                commands.push((event.to_owned(), script.to_owned()));
            } else if event == "install"
                && value["scripts"]["preinstall"].as_str().is_none()
                && value["scripts"]["install"].as_str().is_none()
                && root.join("binding.gyp").is_file()
            {
                // This uses only a locked dependency's node-gyp binary. The
                // offline sandbox never installs a tool or downloads headers.
                commands.push((event.into(), "node-gyp rebuild --ensure".into()));
            }
        }
        let dependencies = ["dependencies", "optionalDependencies", "peerDependencies"]
            .into_iter()
            .filter_map(|field| value[field].as_object())
            .flat_map(|map| map.keys().cloned())
            .collect();
        let events = commands.iter().map(|(event, _)| event.clone()).collect();
        scripts.push(InstalledScript {
            root: canonical,
            report: DependencyScript {
                approval,
                events,
                executed: false,
            },
            commands,
            dependencies,
        });
    }
    dependency_order(provider, stage, scripts)
}

fn dependency_order(
    provider: &str,
    stage: &Path,
    mut scripts: Vec<InstalledScript>,
) -> Result<Vec<InstalledScript>, DependencyError> {
    let stage = fs::canonicalize(stage).map_err(|_| invalid(provider, "invalid stage"))?;
    scripts.sort_by(|left, right| left.root.cmp(&right.root));
    let positions = scripts
        .iter()
        .enumerate()
        .map(|(index, script)| (script.root.clone(), index))
        .collect::<BTreeMap<_, _>>();
    let mut edges = vec![BTreeSet::new(); scripts.len()];
    for (index, script) in scripts.iter().enumerate() {
        for dependency in &script.dependencies {
            if dependency.is_empty()
                || Path::new(dependency).is_absolute()
                || Path::new(dependency)
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                return Err(invalid(provider, "invalid dependency name"));
            }
            for ancestor in script
                .root
                .ancestors()
                .take_while(|path| path.starts_with(&stage))
            {
                if ancestor.file_name() == Some(OsStr::new("node_modules")) {
                    continue;
                }
                if let Ok(resolved) =
                    fs::canonicalize(ancestor.join("node_modules").join(dependency))
                {
                    if let Some(target) = positions.get(&resolved) {
                        edges[index].insert(*target);
                    }
                    break;
                }
            }
        }
    }
    // An iterative DFS also handles large graphs and dependency cycles. A
    // cycle has no total dependency order; its traversal is deterministic.
    let mut visited = vec![false; scripts.len()];
    let mut order = Vec::new();
    for index in 0..scripts.len() {
        let mut stack = vec![(index, false)];
        while let Some((index, complete)) = stack.pop() {
            if complete {
                order.push(index);
                continue;
            }
            if std::mem::replace(&mut visited[index], true) {
                continue;
            }
            stack.push((index, true));
            stack.extend(edges[index].iter().rev().map(|target| (*target, false)));
        }
    }
    let mut scripts = scripts.into_iter().map(Some).collect::<Vec<_>>();
    Ok(order
        .into_iter()
        .filter_map(|index| scripts[index].take())
        .filter(|script| !script.commands.is_empty())
        .collect())
}

fn bun_install_path(key: &str, workspaces: &BTreeMap<String, String>) -> Option<String> {
    let mut parts = key.split('/');
    let mut packages = Vec::new();
    while let Some(first) = parts.next() {
        if first.is_empty() || matches!(first, "." | "..") {
            return None;
        }
        packages.push(if first.starts_with('@') {
            format!("{first}/{}", parts.next()?)
        } else {
            first.into()
        });
    }
    let mut path = String::new();
    for (index, package) in packages.iter().enumerate() {
        if index == 0
            && let Some(workspace) = workspaces.get(package)
        {
            path = workspace.clone();
            continue;
        }
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str("node_modules/");
        path.push_str(package);
    }
    Some(path)
}

fn pnpm_bindings(
    stage: &Path,
    lock: &[u8],
    identities: &[Approval],
) -> Result<BTreeMap<String, ScriptApproval>, DependencyError> {
    let fail = || {
        invalid(
            "pnpm",
            "installed dependency does not match the locked graph",
        )
    };
    let lock: Value = serde_yaml_ng::from_slice(lock).map_err(|_| fail())?;
    let stage = fs::canonicalize(stage).map_err(|_| fail())?;
    let mut pending = std::collections::VecDeque::new();
    for (relative, importer) in lock["importers"].as_object().ok_or_else(fail)? {
        if relative != "."
            && (Path::new(relative).is_absolute()
                || Path::new(relative)
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_))))
        {
            return Err(fail());
        }
        pending.push_back((stage.join(relative), importer.clone()));
    }
    let by_key = identities
        .iter()
        .map(|identity| {
            (
                format!("{}@{}", identity.package, identity.version),
                identity,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut bindings = BTreeMap::new();
    while let Some((parent, dependencies)) = pending.pop_front() {
        for field in ["dependencies", "devDependencies", "optionalDependencies"] {
            for (alias, value) in dependencies[field]
                .as_object()
                .into_iter()
                .flat_map(|map| map.iter())
            {
                if !super::javascript::valid_registry_name(alias) {
                    return Err(fail());
                }
                let reference = value
                    .as_str()
                    .or_else(|| value["version"].as_str())
                    .ok_or_else(fail)?;
                let resolved = parent
                    .ancestors()
                    .take_while(|path| path.starts_with(&stage))
                    .filter(|path| path.file_name() != Some(OsStr::new("node_modules")))
                    .find_map(|path| fs::canonicalize(path.join("node_modules").join(alias)).ok());
                let Some(root) = resolved else { continue }; // OS-specific optional dependency.
                let relative = root
                    .strip_prefix(&stage)
                    .map_err(|_| fail())?
                    .to_str()
                    .ok_or_else(fail)?
                    .to_owned();
                if reference.starts_with("link:") {
                    continue;
                } // Workspace hooks are excluded.
                let base = reference.split('(').next().ok_or_else(fail)?;
                let key = if by_key.contains_key(base) {
                    reference.to_owned()
                } else {
                    format!("{alias}@{reference}")
                };
                let identity = by_key
                    .get(key.split('(').next().ok_or_else(fail)?)
                    .ok_or_else(fail)?;
                let grant = approval("pnpm", identity);
                if let Some(previous) = bindings.get(&relative) {
                    if previous != &grant {
                        return Err(fail());
                    }
                    continue;
                }
                bindings.insert(relative, grant);
                let snapshot = lock["snapshots"].get(&key).ok_or_else(fail)?;
                pending.push_back((root, snapshot.clone()));
            }
        }
    }
    Ok(bindings)
}

pub(super) fn execute(
    stage: &Path,
    scripts: &mut [InstalledScript],
    grants: &[ScriptApproval],
    tools: &ScriptTools,
) -> Result<(), DependencyError> {
    let stage = fs::canonicalize(stage).map_err(|_| invalid("scripts", "invalid stage"))?;
    let home = stage.join(".script-home");
    let temp = stage.join(".script-tmp");
    let bin = stage.join(".script-bin");
    for path in [&home, &temp, &bin] {
        fs::create_dir_all(path).map_err(|_| invalid("scripts", "cannot create script runtime"))?;
    }
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"));
    fs::write(
        bin.join("node"),
        format!("#!/bin/sh\nexec {} \"$@\"\n", quote(&tools.node.path)),
    )
    .map_err(|_| invalid("scripts", "cannot pin script Node"))?;
    fs::set_permissions(bin.join("node"), fs::Permissions::from_mode(0o755))
        .map_err(|_| invalid("scripts", "cannot pin script Node"))?;
    for script in scripts {
        if !grants.contains(&script.report.approval) {
            continue;
        }
        let root = fs::canonicalize(&script.root)
            .map_err(|_| invalid("scripts", "invalid script root"))?;
        detach_hardlinks(&root)?;
        let profile = sandbox_profile(&stage, &root, &home, &temp, tools)?;
        let mut paths = vec![bin.clone()];
        let mut current = Some(root.as_path());
        while let Some(path) = current.filter(|path| path.starts_with(&stage)) {
            paths.push(path.join("node_modules/.bin"));
            current = path.parent();
        }
        paths.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        let path = std::env::join_paths(paths)
            .map_err(|_| invalid("scripts", "invalid script search path"))?;
        for (event, text) in &script.commands {
            let mut child = Command::new("/usr/bin/sandbox-exec")
                .args([
                    OsStr::new("-p"),
                    OsStr::new(&profile),
                    tools.shell.path.as_os_str(),
                    OsStr::new("-c"),
                    OsStr::new(text),
                ])
                .current_dir(&root)
                .env_clear()
                .env("PATH", &path)
                .env("HOME", &home)
                .env("TMPDIR", &temp)
                .env("CI", "1")
                .env("LANG", "C")
                .env("LC_ALL", "C")
                .env("NO_COLOR", "1")
                .env("OPENSSL_CONF", "/dev/null")
                .env("npm_lifecycle_event", event)
                .env("npm_lifecycle_script", text)
                .env("npm_package_name", &script.report.approval.package)
                .env("npm_package_version", &script.report.approval.version)
                .env("npm_package_json", root.join("package.json"))
                .env("npm_node_execpath", &tools.node.path)
                .env("npm_config_ignore_scripts", "true")
                .env("npm_config_offline", "true")
                .env("INIT_CWD", &stage)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .map_err(|_| invalid("scripts", "cannot start approved lifecycle"))?;
            let deadline = Instant::now() + Duration::from_secs(300);
            let status = loop {
                if let Some(status) = child
                    .try_wait()
                    .map_err(|_| invalid("scripts", "cannot wait for approved lifecycle"))?
                {
                    break Some(status);
                }
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            // Descendants may not outlive the approved hook or write after
            // validation. The process group belongs only to this child.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            if !status.is_some_and(|status| status.success()) {
                return Err(DependencyError::CommandFailed {
                    provider: script.report.approval.provider.clone(),
                    phase: format!("approved {event}"),
                    command: script.report.approval.package.clone(),
                    status: status.and_then(|status| status.code()),
                    stderr: "approved lifecycle failed or timed out; output is private".into(),
                });
            }
        }
        script.report.executed = true;
    }
    Ok(())
}

/// A manager may hardlink equal files across package directories. A hook's
/// permitted write must never alter a different package through that inode.
#[cfg(target_os = "macos")]
fn detach_hardlinks(package: &Path) -> Result<(), DependencyError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let entries = walkdir::WalkDir::new(package)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != OsStr::new("node_modules"))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("scripts", "cannot inspect script files"))?;
    for entry in entries
        .into_iter()
        .filter(|entry| entry.file_type().is_file())
    {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(entry.path())
            .map_err(|_| invalid("scripts", "cannot pin script file"))?;
        if file
            .metadata()
            .map_err(|_| invalid("scripts", "cannot inspect script links"))?
            .nlink()
            < 2
        {
            continue;
        }
        let temp = tempfile::Builder::new()
            .prefix(".script-cow-")
            .tempdir_in(package)
            .map_err(|_| invalid("scripts", "cannot stage independent script file"))?;
        let target = temp.path().join("file");
        let name = std::ffi::CString::new(target.as_os_str().as_bytes())
            .map_err(|_| invalid("scripts", "invalid script file path"))?;
        if unsafe { libc::fclonefileat(file.as_raw_fd(), libc::AT_FDCWD, name.as_ptr(), 0) } != 0 {
            return Err(DependencyError::CowUnavailable {
                path: "script file".into(),
                reason: std::io::Error::last_os_error().to_string(),
            });
        }
        fs::rename(target, entry.path())
            .map_err(|_| invalid("scripts", "cannot detach script file"))?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn detach_hardlinks(_package: &Path) -> Result<(), DependencyError> {
    Err(DependencyError::CowUnavailable {
        path: "script package".into(),
        reason: "script builds require APFS on macOS".into(),
    })
}

fn sandbox_profile(
    stage: &Path,
    package: &Path,
    home: &Path,
    temp: &Path,
    tools: &ScriptTools,
) -> Result<String, DependencyError> {
    let quoted = |path: &Path| {
        serde_json::to_string(&path.to_string_lossy())
            .map_err(|_| invalid("scripts", "invalid sandbox path"))
    };
    let mut reads = BTreeSet::from([
        stage.to_path_buf(),
        PathBuf::from("/System"),
        PathBuf::from("/usr"),
        PathBuf::from("/bin"),
        PathBuf::from("/sbin"),
        PathBuf::from("/Library/Developer"),
    ]);
    reads.insert(
        tools
            .node
            .path
            .parent()
            .ok_or_else(|| invalid("scripts", "invalid Node path"))?
            .to_path_buf(),
    );
    let mut profile="(version 1) (allow default) (deny network*) (deny mach-lookup) (deny file-read-data) (deny file-write*)".to_owned();
    for path in reads {
        profile.push_str(&format!(
            " (allow file-read-data (subpath {}))",
            quoted(&path)?
        ));
    }
    for path in &tools.readable_libraries {
        profile.push_str(&format!(
            " (allow file-read-data (literal {}))",
            quoted(path)?
        ));
    }
    for path in [package, home, temp] {
        profile.push_str(&format!(" (allow file-write* (subpath {}))", quoted(path)?));
    }
    profile.push_str(&format!(
        " (deny file-write* (subpath {}))",
        quoted(&package.join("node_modules"))?
    ));
    // dyld opens the root directory during process initialization. Reading
    // this directory does not grant access to any file below it.
    profile.push_str(" (allow file-read-data (literal \"/\") (literal \"/dev/null\") (literal \"/dev/random\") (literal \"/dev/urandom\")) (allow file-write* (literal \"/dev/null\"))");
    Ok(profile)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn pnpm_uses_locked_aliases_and_peer_graphs_with_opaque_store_paths() {
        let stage = tempfile::tempdir().unwrap();
        let root = stage
            .path()
            .join("node_modules/.pnpm/shortened_opaque_hash/node_modules/@scope/addon");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(
            ".pnpm/shortened_opaque_hash/node_modules/@scope/addon",
            stage.path().join("node_modules/alias"),
        )
        .unwrap();
        fs::write(
            root.join("package.json"),
            br#"{"name":"@scope/addon","version":"1.0.0","scripts":{"install":"node build.js"}}"#,
        )
        .unwrap();
        let lock = br#"lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      alias: {specifier: 'npm:@scope/addon@1.0.0', version: '@scope/addon@1.0.0(peer@2.0.0)'}
snapshots:
  '@scope/addon@1.0.0(peer@2.0.0)': {}
"#;
        let identity = Approval {
            package: "@scope/addon".into(),
            version: "1.0.0".into(),
            integrity: "sha512-exact".into(),
        };
        let scripts = inventory("pnpm", stage.path(), lock, &[identity]).unwrap();
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].root, fs::canonicalize(root).unwrap());
        assert_eq!(scripts[0].report.approval.package, "@scope/addon");
    }

    #[test]
    fn hoisted_dependencies_run_first_even_through_packages_without_hooks() {
        let stage = tempfile::tempdir().unwrap();
        let mut packages = serde_json::Map::new();
        let mut identities = Vec::new();
        for (name, dependencies, hooks) in [
            ("a-app", serde_json::json!({"middle":"1.0.0"}), true),
            ("middle", serde_json::json!({"z-addon":"1.0.0"}), false),
            ("z-addon", serde_json::json!({}), true),
        ] {
            let root = stage.path().join("node_modules").join(name);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("package.json"), serde_json::to_vec(&serde_json::json!({
                "name":name,"version":"1.0.0","dependencies":dependencies,
                "scripts":if hooks { serde_json::json!({"postinstall":"node build.cjs"}) } else { serde_json::json!({}) },
            })).unwrap()).unwrap();
            let integrity = format!("sha512-{name}");
            packages.insert(
                format!("node_modules/{name}"),
                serde_json::json!({"version":"1.0.0","integrity":integrity}),
            );
            identities.push(Approval {
                package: name.into(),
                version: "1.0.0".into(),
                integrity,
            });
        }
        let lock = serde_json::to_vec(&serde_json::json!({"packages":packages})).unwrap();
        let scripts = inventory("npm", stage.path(), &lock, &identities).unwrap();
        assert_eq!(
            scripts
                .iter()
                .map(|script| script.report.approval.package.as_str())
                .collect::<Vec<_>>(),
            ["z-addon", "a-app"]
        );
        fs::write(stage.path().join("node_modules/z-addon/binding.gyp"), "{}").unwrap();
        let scripts = inventory("npm", stage.path(), &lock, &identities).unwrap();
        assert_eq!(scripts[0].report.events, ["install", "postinstall"]);
    }

    #[test]
    fn only_an_exact_grant_runs_and_the_hook_cannot_escape_its_package() {
        let temporary = tempfile::tempdir().unwrap();
        let stage = temporary.path().join("stage");
        let package = stage.join("node_modules/addon");
        fs::create_dir_all(&package).unwrap();
        fs::create_dir_all(stage.join("node_modules/neighbor")).unwrap();
        let secret = temporary.path().join("private.txt");
        fs::write(&secret, "outside-private-data").unwrap();
        let library = temporary.path().join("runtime.dylib");
        fs::write(&library, "runtime library fixture").unwrap();
        let escaped = temporary.path().join("escape.txt");
        let neighbor = stage.join("node_modules/neighbor/escape.txt");
        fs::write(&neighbor, "untouched").unwrap();
        fs::hard_link(&neighbor, package.join("shared.txt")).unwrap();
        let nested = package.join("node_modules/nested/untouched.txt");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, "nested-original").unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let code = format!(
            r#"
const fs=require('node:fs'),net=require('node:net'); let denied=0;
if (fs.readFileSync({library},'utf8') !== 'runtime library fixture') process.exit(11);
for (const action of [()=>fs.readFileSync({secret}),()=>fs.writeFileSync({escaped},'bad'),()=>fs.writeFileSync({neighbor},'bad'),()=>{{fs.linkSync({neighbor},'escape-link');fs.writeFileSync('escape-link','bad')}},()=>fs.writeFileSync('node_modules/nested/untouched.txt','bad')]) {{ try {{ action(); }} catch {{ denied++; }} }}
fs.writeFileSync('shared.txt','independent');
const socket=net.connect({{host:'127.0.0.1',port:{port}}});
socket.on('connect',()=>process.exit(8));
socket.on('error',error=>{{ if (!['EPERM','EACCES'].includes(error.code)) process.exit(9); fs.writeFileSync('built.txt',String(denied)); }});
setTimeout(()=>process.exit(10),1500).unref();
"#,
            secret = serde_json::to_string(&secret).unwrap(),
            library = serde_json::to_string(&library).unwrap(),
            escaped = serde_json::to_string(&escaped).unwrap(),
            neighbor = serde_json::to_string(&neighbor).unwrap(),
            port = listener.local_addr().unwrap().port()
        );
        fs::write(package.join("postinstall.cjs"), code).unwrap();
        fs::write(package.join("package.json"),r#"{"name":"addon","version":"1.0.0","scripts":{"postinstall":"node postinstall.cjs"}}"#).unwrap();
        let identity = Approval {
            package: "addon".into(),
            version: "1.0.0".into(),
            integrity: "sha512-exact-test".into(),
        };
        let lock=serde_json::to_vec(&serde_json::json!({"packages":{"node_modules/addon":{"version":identity.version,"integrity":identity.integrity}}})).unwrap();
        let mut scripts = inventory("npm", &stage, &lock, std::slice::from_ref(&identity)).unwrap();
        let mut tools = ScriptTools::identify(&stage).unwrap();
        // Grant one runtime file beside the private file above. Allowing its
        // parent directory would expose the secret and fail the denial count.
        tools
            .readable_libraries
            .insert(fs::canonicalize(library).unwrap());
        let mut wrong = scripts[0].report.approval.clone();
        wrong.integrity = "sha512-other".into();
        execute(&stage, &mut scripts, &[wrong], &tools).unwrap();
        assert!(!package.join("built.txt").exists());
        let grant = scripts[0].report.approval.clone();
        execute(&stage, &mut scripts, &[grant], &tools).unwrap();
        assert_eq!(fs::read_to_string(package.join("built.txt")).unwrap(), "5");
        assert_eq!(
            fs::read(package.join("shared.txt")).unwrap(),
            b"independent"
        );
        assert_eq!(fs::read(nested).unwrap(), b"nested-original");
        assert!(scripts[0].report.executed);
        assert!(!escaped.exists());
        assert_eq!(fs::read(neighbor).unwrap(), b"untouched");
        assert_eq!(fs::read_to_string(secret).unwrap(), "outside-private-data");
    }

    #[test]
    fn a_tarball_cannot_claim_another_locked_packages_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let package = temporary.path().join("node_modules/attacker");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"approved","version":"1.0.0","scripts":{"install":"exit 0"}}"#,
        )
        .unwrap();
        let identities = vec![
            Approval {
                package: "attacker".into(),
                version: "1.0.0".into(),
                integrity: "sha512-attacker".into(),
            },
            Approval {
                package: "approved".into(),
                version: "1.0.0".into(),
                integrity: "sha512-approved".into(),
            },
        ];
        let lock=br#"{"packages":{"node_modules/attacker":{"version":"1.0.0","integrity":"sha512-attacker"},"node_modules/approved":{"version":"1.0.0","integrity":"sha512-approved"}}}"#;
        assert!(inventory("npm", temporary.path(), lock, &identities).is_err());
    }
}
