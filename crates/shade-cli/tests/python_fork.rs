//! Real uv/Python readiness followed by a faithful APFS fork through the Rust SDK.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use serde_json::json;
use sha2::{Digest, Sha256};
use shade_client::{ShadeClient, TerminalOutcome};
use shade_protocol::{OpenSession, RepositoryLocator, SessionId};
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

fn command(root: &Path, executable: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> String {
    let result = Command::new(executable)
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("UV_PYTHON_DOWNLOADS", "never")
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
#[ignore = "requires APFS, real uv/Python and private daemon sockets"]
async fn real_python_fork_runs_independently_with_agent_edited_packages_and_entrypoints() {
    let temporary = tempfile::tempdir_in("/private/tmp").unwrap();
    let root = temporary.path();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    let marker = root.join("project-built");
    fs::write(source.join("pyproject.toml"), "[project]\nname='shade-fork-fixture'\nversion='1.0.0'\nrequires-python='>=3.11'\ndependencies=[]\n[build-system]\nrequires=[]\nbuild-backend='backend'\nbackend-path=['.']\n").unwrap();
    fs::write(source.join("backend.py"), format!("from pathlib import Path\nPath({:?}).touch()\nraise RuntimeError('project build forbidden')\n", marker.to_str().unwrap())).unwrap();
    command(
        &source,
        "uv",
        &["lock", "--offline", "--no-python-downloads"],
    );
    command(&source, "git", &["init", "-b", "main"]);
    command(&source, "git", &["config", "user.name", "Shade test"]);
    command(
        &source,
        "git",
        &["config", "user.email", "shade@example.invalid"],
    );
    command(&source, "git", &["add", "."]);
    command(&source, "git", &["commit", "-m", "Python fork fixture"]);
    let input_binary = std::env::var_os("SHADE_PYTHON_FORK_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_shade").into());
    let binary = root.join("shade");
    fs::copy(input_binary, &binary).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o500)).unwrap();
    let digest = hex::encode(Sha256::digest(fs::read(&binary).unwrap()));
    let socket = root.join("s.sock");
    let mut daemon = Daemon(
        Command::new(&binary)
            .args(["--socket", socket.to_str().unwrap(), "daemon"])
            .env("SHADE_ROOT", root.join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(daemon.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "daemon did not start");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = ShadeClient::cli(&socket);
    let parent = client
        .sessions()
        .open(OpenSession {
            session_id: SessionId("python-parent".into()),
            repository: RepositoryLocator::Local {
                path: source.to_string_lossy().into_owned(),
            },
            base: Some("main".into()),
            intent: None,
        })
        .await
        .unwrap();
    let parent_opened = parent.opened();
    let parent_root = Path::new(&parent_opened.cwd);
    let parent_venv = parent_root.join(".venv");
    let library = PathBuf::from(command(
        parent_root,
        parent_venv.join("bin/python"),
        &[
            "-I",
            "-c",
            "import sysconfig; print(sysconfig.get_paths()['purelib'])",
        ],
    ));
    let module = library.join("shade_fork_fixture.py");
    fs::write(&module, "VALUE = 84\n").unwrap();
    let entrypoint = parent_venv.join("bin/shade-fork-fixture");
    let script = format!(
        "#!{}/bin/python\nimport sys, shade_fork_fixture\nprint(sys.prefix)\nprint(shade_fork_fixture.VALUE)\n",
        parent_venv.display()
    );
    fs::write(&entrypoint, &script).unwrap();
    fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o755)).unwrap();
    let child = match parent
        .fork(SessionId("python-child".into()), None)
        .await
        .unwrap()
    {
        TerminalOutcome::Completed(child) => child,
        other => panic!("{other:?}"),
    };
    let child_opened = child.opened();
    let child_root = Path::new(&child_opened.cwd);
    let child_venv = child_root.join(".venv");
    assert_eq!(
        fs::read_to_string(child_root.join(module.strip_prefix(parent_root).unwrap())).unwrap(),
        "VALUE = 84\n"
    );
    assert_eq!(fs::read_to_string(&entrypoint).unwrap(), script);
    let hidden = parent_root.join(".venv-unavailable");
    fs::rename(&parent_venv, &hidden).unwrap();
    let output = command(child_root, child_venv.join("bin/shade-fork-fixture"), &[]);
    fs::rename(hidden, &parent_venv).unwrap();
    assert_eq!(output, format!("{}\n84", child_venv.display()));
    assert_eq!(
        command(
            child_root,
            "/bin/sh",
            &["-c", ". .venv/bin/activate; printf '%s' \"$VIRTUAL_ENV\""]
        ),
        child_venv.to_str().unwrap()
    );
    assert!(!marker.exists(), "project backend executed");
    child.release().await.unwrap();
    parent.release().await.unwrap();
    eprintln!(
        "SHADE_PYTHON_FORK_EVIDENCE {}",
        json!({"binary_sha256":digest,"status":"passed","checks":["real_uv_python","project_build_blocked","edited_packages_preserved","entrypoint_relocated","activation_relocated","child_runs_without_parent_venv","parent_bytes_preserved"]})
    );
}
