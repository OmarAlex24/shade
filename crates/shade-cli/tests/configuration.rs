//! Default daemon providers with isolated ambient configuration and a real polyglot repo.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

#[path = "../../shade-engine/tests/support/registry.rs"]
mod registry;
#[path = "support/script_fixture.rs"]
mod script_fixture;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use shade_client::{ShadeClient, ShadeClientOptions};
use shade_protocol::{
    Actor, ActorKind, OpenSession, Query, RepositoryLocator, ResponseBody, SessionId,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(root: &Path, executable: &str, args: &[&str]) -> String {
    let result = Command::new(executable)
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("UV_PYTHON_DOWNLOADS", "never")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
#[ignore = "requires APFS and real npm/Node, uv/Python, Cargo/Rust and Go"]
async fn default_daemon_prepares_polyglot_dependencies_with_external_config_isolated() {
    let temporary = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temporary.path();
    let source = root.join("source");
    fs::create_dir_all(source.join("src")).unwrap();
    let registry = script_fixture::add_script_package(&source, root);
    fs::write(source.join("pyproject.toml"), "[project]\nname='shade-polyglot'\nversion='1.0.0'\nrequires-python='>=3.11'\ndependencies=[]\n").unwrap();
    command(
        &source,
        "uv",
        &["lock", "--no-config", "--offline", "--no-python-downloads"],
    );
    fs::write(
        source.join("Cargo.toml"),
        "[package]\nname='shade-polyglot'\nversion='1.0.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(source.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    fs::write(
        source.join("build.rs"),
        "fn main() { panic!(\"build scripts are forbidden\"); }\n",
    )
    .unwrap();
    command(&source, "cargo", &["generate-lockfile", "--offline"]);
    fs::write(
        source.join("go.mod"),
        "module example.invalid/shade-polyglot\n\ngo 1.27\n",
    )
    .unwrap();
    fs::write(source.join("go.sum"), "").unwrap();
    fs::write(
        source.join("main.go"),
        "package main\nfunc init() { panic(\"Go source execution forbidden\") }\nfunc main() {}\n",
    )
    .unwrap();
    command(&source, "git", &["init", "-b", "main"]);
    command(&source, "git", &["config", "user.name", "Shade acceptance"]);
    command(
        &source,
        "git",
        &["config", "user.email", "shade@example.invalid"],
    );
    command(&source, "git", &["add", "."]);
    command(&source, "git", &["commit", "-m", "polyglot fixture"]);
    let names = [
        "package-lock.json",
        "uv.lock",
        "Cargo.lock",
        "go.mod",
        "go.sum",
    ];
    let original: Vec<_> = names
        .iter()
        .map(|name| fs::read(source.join(name)).unwrap())
        .collect();

    let npm = fs::canonicalize(command(root, "/usr/bin/which", &["npm"])).unwrap();
    let uv = fs::canonicalize(command(root, "/usr/bin/which", &["uv"])).unwrap();
    let tools = root.join("tools");
    fs::create_dir(&tools).unwrap();
    let global = root.join("global.npmrc");
    fs::write(&global, "offline=true\n").unwrap();
    fs::write(
        tools.join("npm"),
        format!(
            "#!/usr/bin/env node\nprocess.env.NPM_CONFIG_GLOBALCONFIG ||= {};\nrequire({});\n",
            serde_json::to_string(&global).unwrap(),
            serde_json::to_string(&npm).unwrap()
        ),
    )
    .unwrap();
    let system = root.join("system");
    fs::create_dir_all(system.join("uv")).unwrap();
    fs::write(system.join("uv/uv.toml"), "[invalid system configuration\n").unwrap();
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    fs::write(
        tools.join("uv"),
        format!(
            "#!/bin/sh\nexport XDG_CONFIG_DIRS={}\nexec {} \"$@\"\n",
            quote(&system),
            quote(&uv)
        ),
    )
    .unwrap();
    for name in ["npm", "uv"] {
        fs::set_permissions(tools.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::create_dir(root.join(".cargo")).unwrap();
    fs::write(
        root.join(".cargo/config.toml"),
        "[invalid parent configuration\n",
    )
    .unwrap();
    let input_binary = std::env::var_os("SHADE_CONFIGURATION_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_shade").into());
    let binary = root.join("shade");
    fs::copy(input_binary, &binary).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o500)).unwrap();
    let digest = hex::encode(Sha256::digest(fs::read(&binary).unwrap()));
    let socket = root.join("s.sock");
    let state = root.join("state");
    let search_path = std::env::join_paths(
        std::iter::once(tools).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let log = fs::File::create(root.join("daemon.log")).unwrap();
    let mut daemon = Daemon(
        Command::new(&binary)
            .arg("--socket")
            .arg(&socket)
            .arg("daemon")
            .env("SHADE_ROOT", &state)
            .env("PATH", search_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let client = ShadeClient::with_options(
        &socket,
        Actor {
            kind: ActorKind::Cli,
            id: "cli".into(),
        },
        ShadeClientOptions {
            operation_timeout: Duration::from_secs(120),
            ..ShadeClientOptions::default()
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if client
            .query(Query::Doctor)
            .await
            .is_ok_and(|reply| matches!(reply.body, ResponseBody::Ok { .. }))
        {
            break;
        }
        assert!(daemon.0.try_wait().unwrap().is_none(), "daemon exited");
        assert!(Instant::now() < deadline, "daemon did not become ready");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let session = client
        .sessions()
        .open(OpenSession {
            session_id: SessionId("polyglot".into()),
            repository: RepositoryLocator::Local {
                path: source.to_string_lossy().into_owned(),
            },
            base: None,
            intent: None,
        })
        .await
        .unwrap();
    let opened = session.opened();
    let workspace = Path::new(&opened.cwd);
    assert!(registry.requests() > 0);
    assert!(
        workspace
            .join("node_modules/approved-alias/package.json")
            .is_file()
    );
    assert!(workspace.join(".venv/bin/python").is_file());
    assert!(
        !workspace
            .join("node_modules/approved-alias/built.txt")
            .exists()
    );
    assert!(!workspace.join("root-ran").exists());
    for (name, bytes) in names.iter().zip(&original) {
        assert_eq!(&fs::read(workspace.join(name)).unwrap(), bytes);
        assert_eq!(&fs::read(source.join(name)).unwrap(), bytes);
    }
    let context = session.context().await.unwrap();
    let mut providers = context.dependencies.providers.clone();
    providers.sort();
    assert_eq!(providers, ["cargo", "go", "npm", "uv"]);
    let encoded: Value = serde_json::to_value(&context).unwrap();
    assert_eq!(encoded["dependencies"]["state"], "ready");
    session.release().await.unwrap();
    eprintln!(
        "SHADE_CONFIGURATION_EVIDENCE {}",
        json!({"status":"passed", "binary_sha256":digest,
        "providers":["npm","uv","cargo","go"], "checks":["default_daemon_polyglot_open", "adverse_ambient_configuration", "all_dependencies_ready_before_cwd", "native_locks_unchanged", "root_and_dependency_scripts_blocked", "session_release"]})
    );
}
