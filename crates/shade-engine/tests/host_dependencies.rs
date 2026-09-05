//! Real host-tool acceptance with isolated registries and caches.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};
use shade_engine::dependencies::{
    BunProvider, CargoProvider, DependencyContext, DependencyProvider, GoProvider, NpmProvider,
    PnpmProvider, UvProvider,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[path = "support/registry.rs"]
mod registry;
use registry::{Registry, base64};

#[test]
fn registry_waits_for_complete_request_headers() {
    use std::io::{Read, Write};
    use std::time::Duration;
    let registry = Registry::new();
    registry.route("/package.tgz", b"fixture archive".to_vec());
    let mut stream =
        std::net::TcpStream::connect(registry.url.trim_start_matches("http://")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    stream
        .write_all(b"GET /package.tgz HTTP/1.1\r\nHost: 127.0.")
        .unwrap();
    let mut byte = [0; 1];
    let error = stream
        .read(&mut byte)
        .expect_err("registry responded before the complete request arrived");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    stream
        .write_all(b"0.1\r\nConnection: close\r\n\r\n")
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.ends_with("\r\n\r\nfixture archive"));
    assert_eq!(registry.requests(), 1);
}

fn host_tool_path(name: &str) -> std::path::PathBuf {
    let output = Command::new("/usr/bin/which").arg(name).output().unwrap();
    assert!(output.status.success(), "host tool missing: {name}");
    fs::canonicalize(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
}

#[tokio::test]
#[ignore = "requires actual Go and APFS; validates workspace and replacement paths"]
async fn real_go_rejects_external_workspace_and_replacement_paths() {
    for declaration in ["work-use", "mod-replace", "work-replace", "symlink-use"] {
        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        let root = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            outside.join("go.mod"),
            "module example.invalid/dep\n\ngo 1.27\n",
        )
        .unwrap();
        fs::write(outside.join("go.sum"), "").unwrap();
        let external = serde_json::to_string(&outside).unwrap();
        let mut manifest = "module example.invalid/app\n\ngo 1.27\n".to_owned();
        match declaration {
            "work-use" => fs::write(
                root.join("go.work"),
                format!("go 1.27\nuse (\n.\n{external}\n)\n"),
            )
            .unwrap(),
            "mod-replace" => manifest.push_str(&format!(
                "require example.invalid/dep v0.0.0\nreplace example.invalid/dep => {external}\n"
            )),
            "work-replace" => {
                manifest.push_str("require example.invalid/dep v0.0.0\n");
                fs::write(
                    root.join("go.work"),
                    format!("go 1.27\nuse .\nreplace example.invalid/dep => {external}\n"),
                )
                .unwrap();
            }
            "symlink-use" => {
                std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
                fs::write(root.join("go.work"), "go 1.27\nuse (\n.\n./linked\n)\n").unwrap();
            }
            _ => unreachable!(),
        }
        fs::write(root.join("go.mod"), &manifest).unwrap();
        fs::write(root.join("go.sum"), "").unwrap();
        let cache = temp.path().join("cache");
        let runtime = temp.path().join("runtime");
        let result = GoProvider::new()
            .ensure_ready(&DependencyContext::production(
                &root, &root, &cache, &runtime,
            ))
            .await;
        assert!(
            matches!(
                result,
                Err(shade_engine::dependencies::DependencyError::UnsafeConfiguration { .. })
            ),
            "accepted external Go declaration {declaration}: {result:?}"
        );
        assert_eq!(fs::read_to_string(root.join("go.mod")).unwrap(), manifest);
        assert_eq!(
            fs::read_to_string(outside.join("go.mod")).unwrap(),
            "module example.invalid/dep\n\ngo 1.27\n"
        );
        assert_eq!(
            fs::read_dir(cache.join("dependencies/staging"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[tokio::test]
#[ignore = "requires actual Go and APFS; validates quoted internal workspace paths"]
async fn real_go_preserves_internal_local_graphs_with_quoted_workspace_paths() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temp.path().join("workspace");
    let member = root.join("packages/dep with space");
    fs::create_dir_all(&member).unwrap();
    fs::write(root.join("go.mod"), "module example.invalid/app\n\ngo 1.27\nrequire example.invalid/dep v0.0.0\nreplace example.invalid/dep => \"./packages/dep with space\"\n").unwrap();
    fs::write(
        member.join("go.mod"),
        "module example.invalid/dep\n\ngo 1.27\n",
    )
    .unwrap();
    fs::write(root.join("go.sum"), "").unwrap();
    fs::write(member.join("go.sum"), "").unwrap();
    fs::write(
        root.join("go.work"),
        "go 1.27\nuse (\n . // root module\n \"./packages/dep with space\" // quoted member\n)\n",
    )
    .unwrap();
    let names = [
        "go.mod",
        "go.sum",
        "go.work",
        "packages/dep with space/go.mod",
        "packages/dep with space/go.sum",
    ];
    let before: Vec<_> = names
        .iter()
        .map(|name| fs::read(root.join(name)).unwrap())
        .collect();
    let cache = temp.path().join("cache");
    let runtime = temp.path().join("runtime");
    let context = DependencyContext::production(&root, &root, &cache, &runtime);
    let service =
        shade_engine::dependencies::DependencyService::new(vec![Box::new(GoProvider::new())]);
    let receipts = service.ensure_all(&context).await.unwrap();
    assert_eq!(
        receipts.len(),
        1,
        "workspace members were split into duplicate preparations"
    );
    let receipt = &receipts[0];
    for (name, bytes) in names.iter().zip(before) {
        assert_eq!(fs::read(root.join(name)).unwrap(), bytes);
    }
    emit_evidence(
        &context,
        receipt,
        &[
            "native_readonly_graph_parser",
            "quoted_workspace_paths",
            "internal_local_replacements",
            "byte_identical_metadata",
        ],
    );
}

#[tokio::test]
#[ignore = "requires actual Cargo/Rust and APFS; uses private executable parent configuration"]
async fn real_cargo_ignores_parent_and_native_cache_configuration() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='cargo-config-fixture'\nversion='1.0.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    host_command(&root, "cargo", &["generate-lockfile", "--offline"]);
    let rustc = host_command(&root, "rustup", &["which", "rustc"]);
    let marker = temp.path().join("unexpected-configured-program");
    let wrapper = temp.path().join("configured-rustc");
    let quote = |path: &str| format!("'{}'", path.replace('\'', "'\\''"));
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf ran >> {}\nexec {} \"$@\"\n",
            quote(marker.to_str().unwrap()),
            quote(&rustc)
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let config = temp.path().join(".cargo/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        format!(
            "[build]\nrustc={}\n",
            serde_json::to_string(&wrapper).unwrap()
        ),
    )
    .unwrap();
    let cache = temp.path().join("cache");
    let runtime = temp.path().join("runtime");
    let native_config = runtime.join("dependency-native/cargo/config.toml");
    fs::create_dir_all(native_config.parent().unwrap()).unwrap();
    fs::write(&native_config, "[invalid native configuration\n").unwrap();
    let context = DependencyContext::production(&root, &root, &cache, &runtime);
    let provider = CargoProvider::new();
    let receipt = provider.ensure_ready(&context).await.unwrap();
    assert!(
        !marker.exists(),
        "Cargo executed a program from unreviewed parent configuration"
    );
    assert_eq!(
        fs::read_to_string(&native_config).unwrap(),
        "[invalid native configuration\n"
    );
    assert_eq!(
        provider.ensure_ready(&context).await.unwrap().fingerprint,
        receipt.fingerprint
    );
    assert!(!marker.exists());
    emit_evidence(
        &context,
        &receipt,
        &[
            "parent_executable_config_ignored",
            "native_cache_config_ignored",
            "cached_replay_remains_isolated",
        ],
    );
}

#[tokio::test]
#[ignore = "requires actual npm/Node and APFS; injects private ambient configuration"]
async fn real_npm_ignores_global_configuration_outside_the_staged_project() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let global = temp.path().join("global.npmrc");
    fs::write(&global, "offline=true\n").unwrap();
    let npm = host_tool_path("npm");
    let wrapper = temp.path().join("npm");
    // Emulate a host prefix's default config without writing the user's npmrc.
    // An explicit globalconfig supplied by Shade takes precedence as in npm.
    fs::write(
        &wrapper,
        format!(
            "#!/usr/bin/env node\nprocess.env.NPM_CONFIG_GLOBALCONFIG ||= {};\nrequire({});\n",
            serde_json::to_string(&global).unwrap(),
            serde_json::to_string(&npm).unwrap()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    javascript_host_smoke("npm", &NpmProvider::with_executable(&wrapper)).await;
    assert_eq!(fs::read_to_string(global).unwrap(), "offline=true\n");
}

#[tokio::test]
#[ignore = "requires actual uv/Python and APFS; injects private system configuration"]
async fn real_uv_ignores_system_configuration_outside_the_staged_project() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("pyproject.toml"), "[project]\nname='isolated-uv-fixture'\nversion='1.0.0'\nrequires-python='>=3.11'\ndependencies=[]\n").unwrap();
    host_command(
        &root,
        "uv",
        &["lock", "--no-config", "--offline", "--no-python-downloads"],
    );
    let system = temp.path().join("system");
    fs::create_dir_all(system.join("uv")).unwrap();
    fs::write(system.join("uv/uv.toml"), "[invalid system configuration\n").unwrap();
    let uv = host_tool_path("uv");
    let wrapper = temp.path().join("uv-with-system-config");
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport XDG_CONFIG_DIRS={}\nexec {} \"$@\"\n",
            quote(&system),
            quote(&uv)
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let cache = temp.path().join("cache");
    let runtime = temp.path().join("runtime");
    let context = DependencyContext::production(&root, &root, &cache, &runtime);
    let receipt = UvProvider::with_tools(&wrapper, host_tool_path("python3"))
        .ensure_ready(&context)
        .await
        .unwrap();
    assert_eq!(receipt.state, "ready");
    assert_eq!(
        fs::read_to_string(system.join("uv/uv.toml")).unwrap(),
        "[invalid system configuration\n"
    );
    emit_evidence(
        &context,
        &receipt,
        &[
            "ambient_system_config_ignored",
            "exact_native_uv_through_owned_test_launcher",
        ],
    );
}

fn javascript_registry(temp: &Path, marker: &Path) -> Registry {
    let registry = Registry::new();
    let package = temp.join("registry-package/package");
    fs::create_dir_all(&package).unwrap();
    let script = "node lifecycle.cjs";
    fs::write(package.join("lifecycle.cjs"), format!(
        "const fs=require('node:fs'); try {{ fs.writeFileSync({},'ran'); }} catch(error) {{ if (!['EPERM','EACCES'].includes(error.code)) throw error; }}\nfs.writeFileSync(process.env.npm_lifecycle_event+'.txt','approved');\nif(process.env.npm_lifecycle_event==='postinstall') fs.writeFileSync('index.js','module.exports = 84;\\n');\n",
        serde_json::to_string(marker).unwrap(),
    )).unwrap();
    let manifest = json!({"name":"shade-registry-fixture", "version":"1.0.0", "main":"index.js", "scripts":{"preinstall":script,"install":script,"postinstall":script}});
    fs::write(
        package.join("package.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(package.join("index.js"), "module.exports = 42;\n").unwrap();
    let archive = temp.join("fixture.tgz");
    let status = Command::new("/usr/bin/tar")
        .env("COPYFILE_DISABLE", "1")
        .args(["-czf"])
        .arg(&archive)
        .arg("-C")
        .arg(package.parent().unwrap())
        .arg("package")
        .status()
        .unwrap();
    assert!(status.success());
    let bytes = fs::read(archive).unwrap();
    let integrity = format!("sha512-{}", base64(&Sha512::digest(&bytes)));
    let mut version = manifest;
    version["dist"] =
        json!({"tarball":format!("{}/fixture.tgz", registry.url), "integrity":integrity});
    registry.route("/shade-registry-fixture", serde_json::to_vec(&json!({"name":"shade-registry-fixture","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":version},"time":{"1.0.0":"2020-01-01T00:00:00.000Z"}})).unwrap());
    registry.route("/fixture.tgz", bytes);
    registry
}

fn python_registry(temp: &Path, marker: &Path, malicious_file: Option<&str>) -> Registry {
    let registry = Registry::new();
    let root = temp.join("wheel");
    let metadata = root.join("shade_registry_fixture-1.0.0.dist-info");
    fs::create_dir_all(&metadata).unwrap();
    fs::write(root.join("shade_registry_fixture.py"), "VALUE = 42\n").unwrap();
    fs::write(
        metadata.join("METADATA"),
        "Metadata-Version: 2.1\nName: shade-registry-fixture\nVersion: 1.0.0\n",
    )
    .unwrap();
    fs::write(
        metadata.join("WHEEL"),
        "Wheel-Version: 1.0\nGenerator: shade-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    )
    .unwrap();
    if let Some(file) = malicious_file {
        fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
        fs::write(
            root.join(file),
            format!(
                "import pathlib; pathlib.Path({:?}).touch()\n",
                marker.to_string_lossy()
            ),
        )
        .unwrap();
    }
    let mut record = walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            format!(
                "{},,\n",
                entry.path().strip_prefix(&root).unwrap().display()
            )
        })
        .collect::<String>();
    record.push_str("shade_registry_fixture-1.0.0.dist-info/RECORD,,\n");
    fs::write(metadata.join("RECORD"), record).unwrap();
    let filename = "shade_registry_fixture-1.0.0-py3-none-any.whl";
    let archive = temp.join(filename);
    assert!(
        Command::new("/usr/bin/zip")
            .args(["-q", "-r"])
            .arg(&archive)
            .arg(".")
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    let bytes = fs::read(archive).unwrap();
    let hash = hex::encode(Sha256::digest(&bytes));
    registry.route(
        "/simple/shade-registry-fixture/",
        format!(
            "<!DOCTYPE html><a href=\"{}/{filename}#sha256={hash}\">{filename}</a>",
            registry.url
        )
        .into_bytes(),
    );
    registry.route(&format!("/{filename}"), bytes);
    registry
}

fn emit_evidence(
    context: &DependencyContext<'_>,
    receipt: &shade_engine::dependencies::DependencyReceipt,
    checks: &[&str],
) {
    let stored: Value = serde_json::from_slice(
        &fs::read(
            context
                .cache_root
                .join("dependencies/artifacts")
                .join(&receipt.provider)
                .join(&receipt.fingerprint)
                .join("receipt.json"),
        )
        .unwrap(),
    )
    .unwrap();
    static EXECUTABLE_DIGEST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let executable_digest = EXECUTABLE_DIGEST.get_or_init(|| {
        hex::encode(Sha256::digest(
            fs::read(std::env::current_exe().unwrap()).unwrap(),
        ))
    });
    eprintln!(
        "SHADE_HOST_EVIDENCE {}",
        json!({"provider":receipt.provider,"fingerprint":receipt.fingerprint,"tools":stored["tools"],"scripts":receipt.scripts,"checks":checks,"test_executable_sha256":executable_digest})
    );
}

fn host_command(root: &Path, tool: &str, args: &[&str]) -> String {
    let home = root.parent().unwrap().join("host-home");
    fs::create_dir_all(&home).unwrap();
    let output = Command::new(tool)
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", home)
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("NPM_CONFIG_IGNORE_SCRIPTS", "true")
        .env("NPM_CONFIG_AUDIT", "false")
        .env("NPM_CONFIG_FUND", "false")
        .env("COREPACK_HOME", corepack_home())
        .env("COREPACK_ENABLE_NETWORK", "0")
        .env("COREPACK_DEFAULT_TO_LATEST", "0")
        .env(
            "RUSTUP_HOME",
            std::env::var_os("RUSTUP_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| dirs::home_dir().unwrap().join(".rustup")),
        )
        .env("RUSTUP_AUTO_INSTALL", "0")
        .env("UV_PYTHON_DOWNLOADS", "never")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{tool} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn corepack_home() -> std::path::PathBuf {
    std::env::var_os("COREPACK_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| dirs::home_dir().unwrap().join(".cache"))
                .join("node/corepack")
        })
}

async fn javascript_host_smoke(manager: &str, provider: &dyn DependencyProvider) {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(root.join("packages/member")).unwrap();
    if manager == "pnpm" && corepack_home().join("v1/pnpm").is_dir() {
        // When using a populated Corepack cache, pin an installed fixture
        // version without permitting a download. Direct installations obtain
        // their version below through the same network-disabled host command.
        let mut installed: Vec<_> = fs::read_dir(corepack_home().join("v1/pnpm"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.join("bin/pnpm.cjs").is_file())
            .collect();
        installed.sort();
        let package: Value = serde_json::from_slice(
            &fs::read(
                installed
                    .last()
                    .expect("install a host pnpm first")
                    .join("package.json"),
            )
            .unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("package.json"),
            serde_json::to_vec(
                &json!({"packageManager":format!("pnpm@{}",package["version"].as_str().unwrap())}),
            )
            .unwrap(),
        )
        .unwrap();
    }
    let version = host_command(&root, manager, &["--version"]);
    let marker = temp.path().join("root-script-ran");
    let dependency_marker = temp.path().join("dependency-script-ran");
    let registry = javascript_registry(temp.path(), &dependency_marker);
    fs::write(root.join(".npmrc"), format!("registry={}/\n", registry.url)).unwrap();
    let script = format!(
        "node -e 'require(\"fs\").writeFileSync({},\"ran\")'",
        serde_json::to_string(&marker).unwrap()
    );
    let mut manifest = json!({"name":"shade-host-fixture", "version":"1.0.0", "private":true,
        "packageManager":format!("{manager}@{version}"), "workspaces":["packages/*"], "scripts":{"postinstall":script}});
    if manager == "pnpm" {
        fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        )
        .unwrap();
    }
    if manager != "npm" {
        manifest["dependencies"] = json!({"shade-host-member":"workspace:*"});
    }
    manifest["dependencies"]["shade-registry-fixture"] = json!("1.0.0");
    fs::write(
        root.join("package.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("packages/member/package.json"),
        br#"{"name":"shade-host-member","version":"1.0.0","private":true}"#,
    )
    .unwrap();
    host_command(
        &root,
        manager,
        &["install", "--lockfile-only", "--ignore-scripts"],
    );
    assert!(!marker.exists());
    let cache = temp.path().join("cache");
    let runtime = temp.path().join("runtime");
    let context = DependencyContext::production(&root, &root, &cache, &runtime);
    let before_fill = registry.requests();
    let receipt = provider.ensure_ready(&context).await.unwrap();
    assert!(
        registry.requests() > before_fill,
        "cold fill never contacted the registry"
    );
    assert_eq!(
        fs::read_to_string(root.join("node_modules/shade-registry-fixture/index.js")).unwrap(),
        "module.exports = 42;\n"
    );
    assert!(
        !dependency_marker.exists(),
        "readiness ran a dependency lifecycle script"
    );
    assert_eq!(receipt.state, "ready");
    assert!(!marker.exists(), "readiness ran the root lifecycle script");
    assert!(
        root.join("node_modules/shade-host-member/package.json")
            .exists()
    );
    let stored: Value = serde_json::from_slice(
        &fs::read(
            cache
                .join("dependencies/artifacts")
                .join(manager)
                .join(&receipt.fingerprint)
                .join("receipt.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        manager == "bun"
            || stored["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "node" && tool["digest"].as_str().unwrap().len() == 64)
    );
    fs::remove_dir_all(root.join("node_modules")).unwrap();
    assert_eq!(
        provider.ensure_ready(&context).await.unwrap().fingerprint,
        receipt.fingerprint
    );
    assert!(
        root.join("node_modules/shade-host-member/package.json")
            .exists()
    );
    assert!(!marker.exists());
    let artifact = cache
        .join("dependencies/artifacts")
        .join(manager)
        .join(&receipt.fingerprint);
    let payload_file = walkdir::WalkDir::new(artifact.join("payload"))
        .into_iter()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name() == "index.js")
        .unwrap()
        .into_path();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&payload_file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&payload_file, "corrupt layer").unwrap();
    fs::remove_dir_all(root.join("node_modules")).unwrap();
    fs::remove_dir_all(runtime.join("dependency-native").join(manager)).unwrap();
    let before_rebuild = registry.requests();
    let rebuilt = provider.ensure_ready(&context).await.unwrap();
    assert_eq!(rebuilt.fingerprint, receipt.fingerprint);
    assert!(
        registry.requests() > before_rebuild,
        "corrupt layer was not rebuilt from a cold cache"
    );
    assert_eq!(
        fs::read_to_string(root.join("node_modules/shade-registry-fixture/index.js")).unwrap(),
        "module.exports = 42;\n"
    );
    assert!(!dependency_marker.exists());
    assert!(!marker.exists());

    assert_eq!(receipt.scripts.len(), 1);
    let script = &receipt.scripts[0];
    assert!(!script.executed);
    assert_eq!(script.events, ["preinstall", "install", "postinstall"]);
    let mut wrong = script.approval.clone();
    wrong.integrity.push('x');
    let wrong_grants = [wrong];
    assert_eq!(
        provider
            .ensure_ready(&context.with_script_approvals(&wrong_grants))
            .await
            .unwrap()
            .fingerprint,
        receipt.fingerprint
    );
    assert!(
        !root
            .join("node_modules/shade-registry-fixture/postinstall.txt")
            .exists()
    );
    let native_originals = walkdir::WalkDir::new(runtime.join("dependency-native").join(manager))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            fs::read(entry.path())
                .ok()
                .filter(|bytes| bytes == b"module.exports = 42;\n")
                .map(|_| entry.into_path())
        })
        .collect::<Vec<_>>();
    let grants = [script.approval.clone()];
    let approved_context = context.with_script_approvals(&grants);
    let approved = provider.ensure_ready(&approved_context).await.unwrap();
    assert_ne!(approved.fingerprint, receipt.fingerprint);
    assert!(approved.scripts[0].executed);
    assert_eq!(
        fs::read_to_string(root.join("node_modules/shade-registry-fixture/index.js")).unwrap(),
        "module.exports = 84;\n"
    );
    for event in ["preinstall", "install", "postinstall"] {
        assert_eq!(
            fs::read_to_string(
                root.join(format!("node_modules/shade-registry-fixture/{event}.txt"))
            )
            .unwrap(),
            "approved"
        );
    }
    for path in native_originals {
        assert_eq!(fs::read(path).unwrap(), b"module.exports = 42;\n");
    }
    assert_eq!(fs::read(&payload_file).unwrap(), b"module.exports = 42;\n");
    assert!(
        !dependency_marker.exists(),
        "approved hook escaped its package"
    );
    assert!(!marker.exists(), "approval ran the root lifecycle script");
    fs::remove_dir_all(root.join("node_modules")).unwrap();
    let requests_before_reuse = registry.requests();
    assert_eq!(
        provider
            .ensure_ready(&approved_context)
            .await
            .unwrap()
            .fingerprint,
        approved.fingerprint
    );
    assert_eq!(registry.requests(), requests_before_reuse);
    assert_eq!(
        fs::read_to_string(root.join("node_modules/shade-registry-fixture/index.js")).unwrap(),
        "module.exports = 84;\n"
    );
    let revoked = provider.ensure_ready(&context).await.unwrap();
    assert_eq!(revoked.fingerprint, receipt.fingerprint);
    assert!(!revoked.scripts[0].executed);
    assert!(
        !root
            .join("node_modules/shade-registry-fixture/postinstall.txt")
            .exists()
    );
    assert_eq!(
        fs::read_to_string(root.join("node_modules/shade-registry-fixture/index.js")).unwrap(),
        "module.exports = 42;\n"
    );

    manifest["packageManager"] = json!(format!("{manager}@999.0.0"));
    fs::write(
        root.join("package.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let error = provider.ensure_ready(&context).await.unwrap_err();
    assert!(matches!(
        error,
        shade_engine::dependencies::DependencyError::ToolUnavailable(_)
            | shade_engine::dependencies::DependencyError::ToolVersionMismatch { .. }
    ));
    assert!(
        !corepack_home()
            .join("v1")
            .join(manager)
            .join("999.0.0")
            .exists()
    );
    emit_evidence(
        &context,
        &receipt,
        &[
            "cold_registry_fill",
            "sandboxed_offline_replay",
            "workspace_links",
            "root_and_dependency_scripts_blocked",
            "corrupt_layer_rebuilt",
            "missing_manager_never_downloaded",
            "exact_script_approval_and_revocation",
            "approved_hooks_isolated_from_native_cache_and_other_layers",
            "approved_artifact_reused_without_network",
        ],
    );
    emit_evidence(
        &approved_context,
        &approved,
        &[
            "exact_script_approval",
            "isolated_hooks",
            "pinned_node_and_shell",
            "approved_artifact_reuse",
            "revocation_preserves_existing_artifact",
        ],
    );
}

#[tokio::test]
#[ignore = "requires the real host npm and Node on APFS"]
async fn real_npm_uses_exact_node_and_preserves_workspace_links() {
    javascript_host_smoke("npm", &NpmProvider::new()).await;
}

#[tokio::test]
#[ignore = "requires the real host pnpm and Node on APFS"]
async fn real_pnpm_uses_exact_node_and_preserves_workspace_links() {
    javascript_host_smoke("pnpm", &PnpmProvider::new()).await;
}

#[tokio::test]
#[ignore = "requires the real host Bun on APFS"]
async fn real_bun_preserves_workspace_links_without_root_scripts() {
    javascript_host_smoke("bun", &BunProvider::new()).await;
}

#[tokio::test]
#[ignore = "requires the real host uv and Python on APFS"]
async fn real_uv_prepares_a_relocatable_environment_without_building_the_project() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(&root).unwrap();
    let marker = temp.path().join("python-project-ran");
    fs::write(root.join("pyproject.toml"), "[project]\nname='shade-host-fixture'\nversion='1.0.0'\nrequires-python='>=3.11'\ndependencies=[]\n[build-system]\nrequires=[]\nbuild-backend='backend'\nbackend-path=['.']\n").unwrap();
    fs::write(root.join("backend.py"), format!("from pathlib import Path\nPath({:?}).touch()\nraise RuntimeError('project execution forbidden')\n", marker.to_string_lossy())).unwrap();
    host_command(&root, "uv", &["lock", "--offline", "--no-python-downloads"]);
    assert!(!marker.exists());
    let cache = temp.path().join("cache");
    let runtime = temp.path().join("runtime");
    let context = DependencyContext::production(&root, &root, &cache, &runtime);
    let provider = UvProvider::new();
    let receipt = provider.ensure_ready(&context).await.unwrap();
    assert_eq!(receipt.state, "ready");
    assert!(!marker.exists());
    let moved = temp.path().join("moved-workspace");
    fs::rename(&root, &moved).unwrap();
    let output = Command::new(moved.join(".venv/bin/python"))
        .args(["-I", "-c", "import sys; print(sys.prefix)"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        moved.join(".venv").to_str().unwrap()
    );
    assert!(!marker.exists());
    emit_evidence(
        &context,
        &receipt,
        &["project_pep517_backend_blocked", "venv_relocatable"],
    );
}

#[tokio::test]
#[ignore = "requires the real host Cargo and Rust on APFS"]
async fn real_cargo_fetches_without_running_build_scripts() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='shade-host-fixture'\nversion='1.0.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(
        root.join("build.rs"),
        "fn main() { panic!(\"build scripts forbidden\"); }\n",
    )
    .unwrap();
    host_command(&root, "cargo", &["generate-lockfile", "--offline"]);
    let lock = fs::read(root.join("Cargo.lock")).unwrap();
    let cache = temp.path().join("cache");
    let runtime = temp.path().join("runtime");
    let context = DependencyContext::production(&root, &root, &cache, &runtime);
    let provider = CargoProvider::new();
    let receipt = provider.ensure_ready(&context).await.unwrap();
    assert_eq!(receipt.state, "ready");
    assert_eq!(
        provider.ensure_ready(&context).await.unwrap().fingerprint,
        receipt.fingerprint
    );
    assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), lock);
    assert!(!root.join("target").exists());
}

#[tokio::test]
#[ignore = "requires the real host Go on APFS"]
async fn real_go_identifies_host_and_never_builds_project() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("go.mod"),
        "module example.invalid/shade-host\n\ngo 1.20\n",
    )
    .unwrap();
    fs::write(root.join("go.sum"), "").unwrap();
    fs::write(
        root.join("main.go"),
        "package main\nfunc init() { panic(\"project execution forbidden\") }\nfunc main() {}\n",
    )
    .unwrap();
    let cache = temp.path().join("cache");
    let runtime = temp.path().join("runtime");
    let context = DependencyContext::production(&root, &root, &cache, &runtime);
    let provider = GoProvider::new();
    let receipt = provider.ensure_ready(&context).await.unwrap();
    assert_eq!(receipt.state, "ready");
    assert_eq!(
        provider.ensure_ready(&context).await.unwrap().fingerprint,
        receipt.fingerprint
    );
    assert_eq!(
        fs::read_to_string(root.join("go.mod")).unwrap(),
        "module example.invalid/shade-host\n\ngo 1.20\n"
    );
    assert!(fs::read(root.join("go.sum")).unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires real uv, Python, APFS and a local wheel index"]
async fn real_uv_cold_wheel_fill_corruption_and_startup_defenses() {
    for malicious in [
        None,
        Some("malicious.pth"),
        Some("_virtualenv.py"),
        Some("shade_registry_fixture-1.0.0.data/scripts/python"),
    ] {
        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        let root = temp.path().join("workspace");
        fs::create_dir_all(&root).unwrap();
        let marker = temp.path().join("wheel-startup-ran");
        let registry = python_registry(temp.path(), &marker, malicious);
        fs::write(root.join("pyproject.toml"), format!("[project]\nname='shade-host-fixture'\nversion='1.0.0'\nrequires-python='>=3.11'\ndependencies=['shade-registry-fixture==1.0.0']\n[[tool.uv.index]]\nurl='{}/simple'\ndefault=true\n", registry.url)).unwrap();
        host_command(
            &root,
            "uv",
            &["lock", "--no-build", "--no-python-downloads"],
        );
        let before = registry.requests();
        let cache = temp.path().join("cache");
        let runtime = temp.path().join("runtime");
        let context = DependencyContext::production(&root, &root, &cache, &runtime);
        let result = UvProvider::new().ensure_ready(&context).await;
        assert!(
            !marker.exists(),
            "uv executed wheel startup code before validation"
        );
        if let Some(file) = malicious {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("startup code")
                    || error.contains("bootstrap file")
                    || (file.ends_with("/scripts/python")
                        && error.contains("reserved name `python`")),
                "{file}: {error}"
            );
            assert!(!root.join(".venv").exists());
            continue;
        }
        let receipt = result.unwrap();
        assert!(
            registry.requests() > before,
            "cold fill did not fetch the wheel"
        );
        let package = walkdir::WalkDir::new(root.join(".venv"))
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name() == "shade_registry_fixture.py")
            .unwrap()
            .into_path();
        assert_eq!(fs::read_to_string(&package).unwrap(), "VALUE = 42\n");
        let artifact = cache
            .join("dependencies/artifacts/uv")
            .join(&receipt.fingerprint);
        let cached = walkdir::WalkDir::new(artifact.join("payload"))
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name() == "shade_registry_fixture.py")
            .unwrap()
            .into_path();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cached, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&cached, "corrupt wheel layer").unwrap();
        fs::remove_dir_all(root.join(".venv")).unwrap();
        fs::remove_dir_all(runtime.join("dependency-native/uv")).unwrap();
        let before = registry.requests();
        assert_eq!(
            UvProvider::new()
                .ensure_ready(&context)
                .await
                .unwrap()
                .fingerprint,
            receipt.fingerprint
        );
        assert!(registry.requests() > before);
        assert_eq!(fs::read_to_string(package).unwrap(), "VALUE = 42\n");
        assert!(!marker.exists());
        emit_evidence(
            &context,
            &receipt,
            &[
                "cold_wheel_index_fill",
                "sandboxed_offline_replay",
                "corrupt_layer_rebuilt",
            ],
        );
    }
}

#[tokio::test]
#[ignore = "requires real Cargo/Rust, APFS and a local sparse registry"]
async fn real_cargo_cold_registry_fetch_never_executes_build_or_proc_macros() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temp.path().join("workspace");
    let package = temp.path().join("crate/shade-registry-fixture-1.0.0");
    for path in [root.join("src"), root.join(".cargo"), package.join("src")] {
        fs::create_dir_all(path).unwrap();
    }
    let marker = temp.path().join("rust-dependency-ran");
    fs::write(package.join("Cargo.toml"), "[package]\nname='shade-registry-fixture'\nversion='1.0.0'\nedition='2021'\n[lib]\nproc-macro=true\n").unwrap();
    fs::write(
        package.join("build.rs"),
        format!(
            "fn main() {{ std::fs::write({:?}, b\"build\").unwrap(); }}\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();
    fs::write(package.join("src/lib.rs"), format!("extern crate proc_macro;\n#[proc_macro]\npub fn fixture(_: proc_macro::TokenStream) -> proc_macro::TokenStream {{ std::fs::write({:?}, b\"macro\").unwrap(); proc_macro::TokenStream::new() }}\n", marker.to_string_lossy())).unwrap();
    let archive = temp.path().join("fixture.crate");
    assert!(
        Command::new("/usr/bin/tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(package.parent().unwrap())
            .arg(package.file_name().unwrap())
            .status()
            .unwrap()
            .success()
    );
    let bytes = fs::read(archive).unwrap();
    let registry = Registry::new();
    registry.route(
        "/config.json",
        serde_json::to_vec(&json!({"dl":format!("{}/api/v1/crates", registry.url)})).unwrap(),
    );
    registry.route("/sh/ad/shade-registry-fixture", format!("{}\n", json!({"name":"shade-registry-fixture","vers":"1.0.0","deps":[],"cksum":hex::encode(Sha256::digest(&bytes)),"features":{},"yanked":false})).into_bytes());
    registry.route(
        "/api/v1/crates/shade-registry-fixture/1.0.0/download",
        bytes,
    );
    fs::write(root.join("Cargo.toml"), "[package]\nname='shade-cargo-root'\nversion='1.0.0'\nedition='2021'\n[dependencies]\nshade-registry-fixture='=1.0.0'\n").unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "shade_registry_fixture::fixture!();\n",
    )
    .unwrap();
    fs::write(
        root.join(".cargo/config.toml"),
        format!(
            "[source.crates-io]\nreplace-with='fixture'\n[source.fixture]\nregistry='sparse+{}/'\n",
            registry.url
        ),
    )
    .unwrap();
    host_command(&root, "cargo", &["generate-lockfile"]);
    let lock = fs::read(root.join("Cargo.lock")).unwrap();
    let cache = temp.path().join("cache");
    let runtime = temp.path().join("runtime");
    let context = DependencyContext::production(&root, &root, &cache, &runtime);
    let before = registry.requests();
    let receipt = CargoProvider::new().ensure_ready(&context).await.unwrap();
    assert!(registry.requests() > before);
    assert!(!marker.exists());
    assert!(!root.join("target").exists());
    assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), lock);
    // A native cache miss must invalidate the receipt and fetch again.
    fs::remove_dir_all(runtime.join("dependency-native/cargo")).unwrap();
    let before = registry.requests();
    assert_eq!(
        CargoProvider::new()
            .ensure_ready(&context)
            .await
            .unwrap()
            .fingerprint,
        receipt.fingerprint
    );
    assert!(registry.requests() > before);
    assert!(!marker.exists());
    assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), lock);
    emit_evidence(
        &context,
        &receipt,
        &[
            "cold_sparse_registry_fill",
            "sandboxed_offline_replay",
            "lock_byte_identical",
            "missing_native_cache_refilled",
            "build_rs_and_proc_macro_blocked",
        ],
    );
    for config in [
        "build={rustc='malicious'}",
        "registry={global-credential-providers=['malicious']}",
        "credential-alias={fixture=['malicious']}",
    ] {
        fs::write(root.join(".cargo/config.toml"), config).unwrap();
        let error = CargoProvider::new()
            .ensure_ready(&context)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            shade_engine::dependencies::DependencyError::UnsafeConfiguration { .. }
        ));
        assert!(!marker.exists());
    }
}

#[tokio::test]
#[ignore = "requires real Go, APFS and proxy.golang.org for the pinned fixture module"]
async fn real_go_cold_module_cache_revalidates_without_running_source() {
    let temp = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir_all(&root).unwrap();
    let marker = temp.path().join("go-code-ran");
    fs::write(
        root.join("go.mod"),
        "module example.invalid/shade\n\ngo 1.20\n\nrequire golang.org/x/text v0.3.8\n",
    )
    .unwrap();
    fs::write(root.join("main.go"), format!("package main\nimport (\"os\"; _ \"golang.org/x/text/language\")\n//go:generate sh -c 'touch {}'\nfunc init() {{ os.WriteFile({:?}, []byte(\"ran\"), 0600) }}\nfunc main() {{}}\n", marker.display(), marker.to_string_lossy())).unwrap();
    host_command(&root, "go", &["mod", "download", "all"]);
    let manifest = fs::read(root.join("go.mod")).unwrap();
    let sums = fs::read(root.join("go.sum")).unwrap();
    assert!(!sums.is_empty());
    let cache = temp.path().join("cache");
    let runtime = temp.path().join("runtime");
    let context = DependencyContext::production(&root, &root, &cache, &runtime);
    let receipt = GoProvider::new().ensure_ready(&context).await.unwrap();
    assert!(!marker.exists());
    assert_eq!(fs::read(root.join("go.mod")).unwrap(), manifest);
    assert_eq!(fs::read(root.join("go.sum")).unwrap(), sums);
    assert!(
        runtime
            .join("dependency-native/go/mod/golang.org/x/text@v0.3.8")
            .is_dir()
    );
    assert_eq!(
        GoProvider::new()
            .ensure_ready(&context)
            .await
            .unwrap()
            .fingerprint,
        receipt.fingerprint
    );
    // Go intentionally makes extracted native modules read-only.
    use std::os::unix::fs::PermissionsExt;
    for entry in walkdir::WalkDir::new(runtime.join("dependency-native/go"))
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_dir() {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    fs::remove_dir_all(runtime.join("dependency-native/go")).unwrap();
    assert_eq!(
        GoProvider::new()
            .ensure_ready(&context)
            .await
            .unwrap()
            .fingerprint,
        receipt.fingerprint
    );
    assert!(!marker.exists());
    assert_eq!(fs::read(root.join("go.mod")).unwrap(), manifest);
    assert_eq!(fs::read(root.join("go.sum")).unwrap(), sums);
    emit_evidence(
        &context,
        &receipt,
        &[
            "cold_pinned_public_module_fill",
            "sandboxed_offline_replay",
            "manifests_and_sums_byte_identical",
            "missing_native_cache_refilled",
            "init_and_generate_blocked",
        ],
    );
}
