use super::{registry::Registry, script_fixture};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Pnpm,
    Bun,
    Uv,
    Cargo,
    Go,
}

impl Manager {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pnpm => "pnpm",
            Self::Bun => "bun",
            Self::Uv => "uv",
            Self::Cargo => "cargo",
            Self::Go => "go",
        }
    }
}

fn corepack_home() -> PathBuf {
    std::env::var_os("COREPACK_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| dirs::home_dir().unwrap().join(".cache"))
                .join("node/corepack")
        })
}

pub fn command(root: &Path, tool: &str, args: &[&str]) -> String {
    let home = root.parent().unwrap().join("fixture-tool-home");
    fs::create_dir_all(&home).unwrap();
    let output = Command::new(tool)
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", &home)
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
                .map(PathBuf::from)
                .unwrap_or_else(|| dirs::home_dir().unwrap().join(".rustup")),
        )
        .env("RUSTUP_AUTO_INSTALL", "0")
        .env("CARGO_HOME", home.join("cargo"))
        .env("UV_PYTHON_DOWNLOADS", "never")
        .env("UV_NO_SYSTEM_CONFIG", "1")
        .env("GOENV", "off")
        .env("GOTOOLCHAIN", "local")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture {tool} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

pub fn prepare(root: &Path, temp: &Path, manager: Manager) -> Option<Registry> {
    fs::write(root.join(".gitignore"), "node_modules/\n.venv/\ntarget/\n").unwrap();
    match manager {
        Manager::Bun | Manager::Pnpm => Some(javascript(root, temp, manager)),
        Manager::Uv => Some(python(root, temp)),
        Manager::Cargo => Some(cargo(root, temp)),
        Manager::Go => {
            fs::write(
                root.join("go.mod"),
                "module example.invalid/shade\n\ngo 1.20\n\nrequire golang.org/x/text v0.3.8\n",
            )
            .unwrap();
            fs::write(root.join("main.go"), "package main\nimport (\"os\"; _ \"golang.org/x/text/language\")\n//go:generate sh -c 'touch root-ran'\nfunc init() { os.WriteFile(\"root-ran\", []byte(\"ran\"), 0600) }\nfunc main() {}\n").unwrap();
            command(root, "go", &["mod", "download", "all"]);
            assert!(!fs::read(root.join("go.sum")).unwrap().is_empty());
            None
        }
    }
}

fn javascript(root: &Path, temp: &Path, manager: Manager) -> Registry {
    let registry = script_fixture::add_script_package(root, temp);
    let lock: Value =
        serde_json::from_slice(&fs::read(root.join("package-lock.json")).unwrap()).unwrap();
    let dist = &lock["packages"]["node_modules/approved-alias"];
    let metadata = serde_json::to_vec(&json!({"name":"@shade/test-build","dist-tags":{"latest":"1.0.0"},"time":{"1.0.0":"2020-01-01T00:00:00.000Z"},"versions":{"1.0.0":{"name":"@shade/test-build","version":"1.0.0","scripts":{"postinstall":"node build.cjs"},"dist":{"tarball":dist["resolved"],"integrity":dist["integrity"]}}}})).unwrap();
    for path in [
        "/@shade%2ftest-build",
        "/@shade%2Ftest-build",
        "/@shade/test-build",
    ] {
        registry.route(path, metadata.clone());
    }
    fs::remove_file(root.join("package-lock.json")).unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(root.join("package.json")).unwrap()).unwrap();
    if manager == Manager::Pnpm && corepack_home().join("v1/pnpm").is_dir() {
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
                    .expect("installed pnpm required")
                    .join("package.json"),
            )
            .unwrap(),
        )
        .unwrap();
        manifest["packageManager"] =
            json!(format!("pnpm@{}", package["version"].as_str().unwrap()));
    } else {
        manifest["packageManager"] = json!(format!(
            "{}@{}",
            manager.name(),
            command(root, manager.name(), &["--version"])
        ));
    }
    manifest["workspaces"] = json!(["packages/*"]);
    manifest["dependencies"]["shade-member"] = json!("workspace:*");
    fs::create_dir_all(root.join("packages/member")).unwrap();
    fs::write(
        root.join("packages/member/package.json"),
        br#"{"name":"shade-member","version":"1.0.0","private":true}"#,
    )
    .unwrap();
    if manager == Manager::Pnpm {
        fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        )
        .unwrap();
    }
    fs::write(
        root.join("package.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    command(
        root,
        manager.name(),
        &["install", "--lockfile-only", "--ignore-scripts"],
    );
    remove_tree(&root.join("node_modules"));
    registry
}

fn python(root: &Path, temp: &Path) -> Registry {
    let registry = Registry::new();
    let wheel = temp.join("wheel");
    let metadata = wheel.join("shade_registry_fixture-1.0.0.dist-info");
    fs::create_dir_all(&metadata).unwrap();
    fs::write(
        wheel.join("shade_registry_fixture.py"),
        "VALUE = 42\ndef main():\n    print(VALUE)\n",
    )
    .unwrap();
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
    fs::write(
        metadata.join("entry_points.txt"),
        "[console_scripts]\nshade-fixture = shade_registry_fixture:main\n",
    )
    .unwrap();
    fs::write(metadata.join("RECORD"), "shade_registry_fixture.py,,\nshade_registry_fixture-1.0.0.dist-info/METADATA,,\nshade_registry_fixture-1.0.0.dist-info/WHEEL,,\nshade_registry_fixture-1.0.0.dist-info/entry_points.txt,,\nshade_registry_fixture-1.0.0.dist-info/RECORD,,\n").unwrap();
    let filename = "shade_registry_fixture-1.0.0-py3-none-any.whl";
    let archive = temp.join(filename);
    assert!(
        Command::new("/usr/bin/zip")
            .args(["-q", "-r"])
            .arg(&archive)
            .arg(".")
            .current_dir(&wheel)
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
    fs::write(root.join("pyproject.toml"), format!("[project]\nname='shade-python-root'\nversion='1.0.0'\nrequires-python='>=3.11'\ndependencies=['shade-registry-fixture==1.0.0']\n[build-system]\nrequires=[]\nbuild-backend='trap_build'\nbackend-path=['.']\n[[tool.uv.index]]\nurl='{}/simple'\ndefault=true\n", registry.url)).unwrap();
    fs::write(root.join("trap_build.py"), "from pathlib import Path\nPath('root-ran').write_text('build')\nraise RuntimeError('project builds forbidden')\n").unwrap();
    command(root, "uv", &["lock", "--no-python-downloads"]);
    registry
}

fn cargo(root: &Path, temp: &Path) -> Registry {
    let registry = Registry::new();
    let package = temp.join("crate/shade-registry-fixture-1.0.0");
    for path in [root.join("src"), root.join(".cargo"), package.join("src")] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(package.join("Cargo.toml"), "[package]\nname='shade-registry-fixture'\nversion='1.0.0'\nedition='2021'\n[lib]\nproc-macro=true\n").unwrap();
    fs::write(package.join("build.rs"), "fn main() { std::fs::write(\"root-ran\", b\"build\").unwrap(); panic!(\"build forbidden\"); }\n").unwrap();
    fs::write(package.join("src/lib.rs"), "extern crate proc_macro;\n#[proc_macro]\npub fn fixture(_: proc_macro::TokenStream) -> proc_macro::TokenStream { std::fs::write(\"root-ran\", b\"macro\").unwrap(); panic!(\"proc macro forbidden\") }\n").unwrap();
    let archive = temp.join("fixture.crate");
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
    registry.route(
        "/config.json",
        serde_json::to_vec(&json!({"dl":format!("{}/api/v1/crates",registry.url)})).unwrap(),
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
    fs::write(root.join("build.rs"), "fn main() { std::fs::write(\"root-ran\", b\"build\").unwrap(); panic!(\"build forbidden\"); }\n").unwrap();
    fs::write(
        root.join(".cargo/config.toml"),
        format!(
            "[source.crates-io]\nreplace-with='fixture'\n[source.fixture]\nregistry='sparse+{}/'\n",
            registry.url
        ),
    )
    .unwrap();
    command(root, "cargo", &["generate-lockfile"]);
    registry
}

pub fn files(root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            result.extend(files(&entry.path()));
        } else if entry.file_type().unwrap().is_file() {
            result.push(entry.path());
        }
    }
    result
}

pub fn remove_tree(root: &Path) {
    if !root.is_dir() {
        return;
    }
    fn writable(root: &Path) {
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        for entry in fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                writable(&entry.path());
            }
        }
    }
    writable(root);
    fs::remove_dir_all(root).unwrap();
}
