#![cfg(all(target_os = "macos", target_arch = "aarch64"))]
//! A repository whose committed content matches the secret detector must keep
//! its whole lifecycle.
//!
//! These tests run the engine with the real installed clean filter -- the CLI
//! binary under test -- because that is the only configuration in which Git
//! re-cleans tracked paths at all. The ordinary engine suites construct a
//! `GitStore` without the filter and cannot see this class of failure.

use shade_engine::Engine;
use shade_engine::config::EngineConfig;
use shade_engine::dependencies::DependencyService;
use shade_engine::filesystem::ApfsFilesystem;
use shade_engine::git::GitStore;
use shade_protocol::{
    Actor, ActorKind, ExecuteRequest, Intent, OpenSession, OpenedSession, Outcome,
    PROTOCOL_VERSION, RepositoryLocator, ResponseBody, SessionId, SleepResult, WorkspaceSelector,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SESSION: &str = "committed-content-session";

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A private-key header inside a string literal, which is what a redaction
/// test, a documentation sample or a fixture actually looks like.
fn committed_fixture() -> String {
    format!(
        "const SAMPLE: &str = \"{}{}\";\n",
        "-----BEGIN ", "PRIVATE KEY-----"
    )
}

fn fixture(root: &Path) -> PathBuf {
    let repository = root.join("source");
    fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init", "-b", "main"]);
    git(&repository, &["config", "user.name", "Shade Test"]);
    git(&repository, &["config", "user.email", "shade@test.invalid"]);
    fs::write(repository.join("tracked.txt"), "base\n").unwrap();
    fs::write(repository.join("redaction_test.rs"), committed_fixture()).unwrap();
    git(&repository, &["add", "-A"]);
    git(&repository, &["commit", "-m", "base"]);
    repository
}

fn engine_at(root: &Path) -> Engine {
    let config = EngineConfig::at(root.join("state"))
        .with_harness_lifecycle_timing(120, 0)
        .unwrap();
    let git = GitStore::system()
        .with_content_filter(env!("CARGO_BIN_EXE_shade").into(), config.runtime_dir());
    Engine::with_components_and_git(
        config,
        Arc::new(ApfsFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
        git,
    )
    .unwrap()
}

fn execute(key: &str, intent: Intent) -> ExecuteRequest {
    ExecuteRequest {
        v: PROTOCOL_VERSION,
        request_id: format!("request-{key}"),
        idempotency_key: key.to_owned(),
        actor: Actor {
            kind: ActorKind::Host,
            id: "secret-lifecycle-actor".into(),
        },
        intent,
    }
}

fn completed<T: serde::de::DeserializeOwned>(response: shade_protocol::WireResponse) -> T {
    match response.body {
        ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        } => serde_json::from_value(value).unwrap(),
        other => panic!("expected a completed response, got {other:?}"),
    }
}

fn selector(opened: &OpenedSession) -> WorkspaceSelector {
    WorkspaceSelector {
        workspace_id: Some(opened.workspace.clone()),
        cwd: None,
    }
}

/// Open, sleep, wake, checkpoint and release a repository that carries a
/// detectable credential in committed source.
///
/// Waking restores the checkpoint's working tree and then reads its index out
/// of the checkpoint's index tree. Git re-hashes the files it has just written
/// while it does that, which sends every tracked path back through the clean
/// filter. Judging those bytes as new content refused the wake outright, and
/// the work behind the suspension was reachable only by giving it up.
#[tokio::test]
async fn a_repository_with_a_committed_credential_sleeps_wakes_and_checkpoints() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_at(directory.path());

    let opened: OpenedSession = completed(
        engine
            .execute(execute(
                "open",
                Intent::SessionOpen(OpenSession {
                    session_id: SessionId(SESSION.into()),
                    repository: RepositoryLocator::Local {
                        path: repository.to_string_lossy().into_owned(),
                    },
                    base: None,
                    intent: None,
                }),
            ))
            .await,
    );
    let tree = engine
        .database()
        .workspace(&opened.workspace)
        .unwrap()
        .unwrap()
        .path;
    assert_eq!(
        fs::read_to_string(tree.join("redaction_test.rs")).unwrap(),
        committed_fixture(),
        "the workspace carries the committed bytes"
    );
    fs::write(tree.join("tracked.txt"), "edited before sleeping\n").unwrap();

    let slept: SleepResult = completed(
        engine
            .execute(execute(
                "sleep",
                Intent::WorkspaceSleep {
                    selector: selector(&opened),
                },
            ))
            .await,
    );
    assert!(slept.suspended);

    let woken: OpenedSession = completed(
        engine
            .execute(execute(
                "wake",
                Intent::SessionWake {
                    session_id: SessionId(SESSION.into()),
                },
            ))
            .await,
    );
    let successor = engine
        .database()
        .workspace(&woken.workspace)
        .unwrap()
        .unwrap()
        .path;
    assert_eq!(
        fs::read_to_string(successor.join("redaction_test.rs")).unwrap(),
        committed_fixture(),
        "the committed bytes survive the wake unchanged"
    );
    assert_eq!(
        fs::read_to_string(successor.join("tracked.txt")).unwrap(),
        "edited before sleeping\n",
        "the work the sleep captured comes back with it"
    );

    // Checkpointing the woken successor stages through a private index read
    // out of a tree, so every tracked path is cleaned again.
    let checkpoint = engine
        .execute(execute(
            "checkpoint",
            Intent::WorkspaceCheckpoint {
                selector: selector(&woken),
                reason: "manual".into(),
            },
        ))
        .await;
    assert!(
        matches!(checkpoint.body, ResponseBody::Ok { .. }),
        "checkpointing a woken successor must not be refused: {:?}",
        checkpoint.body
    );

    let release = engine
        .execute(execute(
            "release",
            Intent::WorkspaceRelease {
                selector: selector(&woken),
            },
        ))
        .await;
    assert!(
        matches!(release.body, ResponseBody::Ok { .. }),
        "release must not be refused: {:?}",
        release.body
    );
}
