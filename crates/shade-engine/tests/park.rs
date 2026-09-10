//! The parked tier: what happens to private build output across a sleep.
//!
//! The invariant every test here defends is that the tier is free to fail.
//! Parking is an optimisation over throwing the bytes away, and restoring is
//! an optimisation over rebuilding them, so an unplugged volume, a stale park
//! or a missing one must leave `sleep` and `wake` doing exactly what they did
//! before the tier existed.

use shade_engine::Engine;
use shade_engine::config::EngineConfig;
use shade_engine::dependencies::DependencyService;
use shade_engine::filesystem::CopyFilesystem;
use shade_protocol::{
    Actor, ActorKind, ExecuteRequest, Intent, OpenSession, OpenedSession, Outcome,
    PROTOCOL_VERSION, Query, QueryRequest, RepositoryLocator, ResponseBody, SessionId, SleepResult,
    WakeResult, WorkspaceSelector,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SESSION: &str = "park-session";
/// Written into the workspace after it opens, under a committed `.gitignore`
/// entry, so it is exactly the kind of file a sleep would otherwise discard.
const BUILD_OUTPUT: &str = "build/out.bin";
const BUILD_CONTENT: &str = "expensively compiled\n";

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

fn fixture(root: &Path) -> PathBuf {
    let repository = root.join("source");
    fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init", "-b", "main"]);
    git(&repository, &["config", "user.name", "Shade Test"]);
    git(&repository, &["config", "user.email", "shade@test.invalid"]);
    fs::write(repository.join("tracked.txt"), "base\n").unwrap();
    fs::write(repository.join(".gitignore"), "build/\n").unwrap();
    git(&repository, &["add", "tracked.txt", ".gitignore"]);
    git(&repository, &["commit", "-m", "base"]);
    repository
}

fn execute(key: &str, intent: Intent) -> ExecuteRequest {
    ExecuteRequest {
        v: PROTOCOL_VERSION,
        request_id: format!("request-{key}"),
        idempotency_key: key.to_owned(),
        actor: Actor {
            kind: ActorKind::Host,
            id: "park-test-actor".into(),
        },
        intent,
    }
}

fn query(query: Query) -> QueryRequest {
    QueryRequest {
        v: PROTOCOL_VERSION,
        request_id: "request-query".into(),
        query,
    }
}

fn completed<T: serde::de::DeserializeOwned>(response: shade_protocol::WireResponse) -> T {
    match response.body {
        ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        } => serde_json::from_value(value).unwrap(),
        other => panic!("expected completed response, got {other:?}"),
    }
}

/// An engine whose parked tier is configured exactly as the test needs it.
/// `park_root` of `None` is the unconfigured default every existing test runs
/// with; `min_bytes` of zero parks anything at all.
fn engine_with(root: &Path, park_root: Option<&Path>, min_bytes: u64) -> Engine {
    let mut config = EngineConfig::at(root.join("state"))
        .with_harness_lifecycle_timing(1, 0)
        .unwrap();
    config.park_root = park_root.map(Path::to_path_buf);
    config.park_min_bytes = min_bytes;
    Engine::with_components(
        config,
        Arc::new(CopyFilesystem),
        Arc::new(DependencyService::new(Vec::new())),
    )
    .unwrap()
}

/// A mounted park volume: an ordinary directory the daemon can write to.
fn mounted(root: &Path) -> PathBuf {
    let park = root.join("park");
    fs::create_dir_all(&park).unwrap();
    park
}

fn open_request(repository: &Path, session: &str) -> Intent {
    Intent::SessionOpen(OpenSession {
        session_id: SessionId(session.to_owned()),
        repository: RepositoryLocator::Local {
            path: repository.to_string_lossy().into_owned(),
        },
        base: None,
        intent: None,
    })
}

fn selector(opened: &OpenedSession) -> WorkspaceSelector {
    WorkspaceSelector {
        workspace_id: Some(opened.workspace.clone()),
        cwd: None,
    }
}

async fn open(engine: &Engine, repository: &Path) -> OpenedSession {
    let opened: OpenedSession = completed(
        engine
            .execute(execute("open", open_request(repository, SESSION)))
            .await,
    );
    let build = PathBuf::from(&opened.cwd).join(BUILD_OUTPUT);
    fs::create_dir_all(build.parent().unwrap()).unwrap();
    fs::write(&build, BUILD_CONTENT).unwrap();
    opened
}

async fn sleep(engine: &Engine, key: &str, opened: &OpenedSession) -> SleepResult {
    completed(
        engine
            .execute(execute(
                key,
                Intent::WorkspaceSleep {
                    selector: selector(opened),
                },
            ))
            .await,
    )
}

async fn wake(engine: &Engine, key: &str) -> shade_protocol::WireResponse {
    engine
        .execute(execute(
            key,
            Intent::SessionWake {
                session_id: SessionId(SESSION.into()),
            },
        ))
        .await
}

async fn collect(engine: &Engine, key: &str) -> serde_json::Value {
    completed(engine.execute(execute(key, Intent::GarbageCollect)).await)
}

/// Open a session on a fresh repository, write build output into it, and sleep
/// it onto a mounted park volume.
async fn parked(root: &Path) -> (Engine, OpenedSession, SleepResult, PathBuf) {
    let park_root = mounted(root);
    let repository = fixture(root);
    let engine = engine_with(root, Some(&park_root), 0);
    let opened = open(&engine, &repository).await;
    let slept = sleep(&engine, "sleep", &opened).await;
    (engine, opened, slept, park_root)
}

fn park_dir(park_root: &Path, slept: &SleepResult) -> PathBuf {
    park_root
        .join(&slept.workspace.0)
        .join(&slept.checkpoint_id.0)
}

#[tokio::test]
async fn sleeping_onto_a_mounted_volume_parks_the_build_output_and_says_so() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, _opened, slept, park_root) = parked(directory.path()).await;

    assert!(slept.suspended);
    assert!(slept.parked, "a mounted volume parks: {slept:?}");
    assert_eq!(slept.park_reason, None);
    assert_eq!(
        slept.parked_bytes,
        BUILD_CONTENT.len() as u64,
        "the reported size is the bytes actually copied"
    );

    let park = park_dir(&park_root, &slept);
    assert_eq!(
        slept.park_path.as_deref(),
        Some(park.to_string_lossy().as_ref())
    );
    assert!(
        park.join("manifest.json").is_file(),
        "a published park is described by a manifest"
    );
    assert_eq!(
        fs::read_to_string(park.join("tree").join(BUILD_OUTPUT)).unwrap(),
        BUILD_CONTENT
    );

    let parked_event = engine
        .events(0, 1_000)
        .unwrap()
        .into_iter()
        .find(|event| event.event == "workspace.parked")
        .expect("parking announces itself");
    assert_eq!(parked_event.payload["checkpoint_id"], slept.checkpoint_id.0);
    assert_eq!(parked_event.payload["bytes"], BUILD_CONTENT.len());

    // And the daemon can answer for it without touching the volume.
    let records = engine.database().parks().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].checkpoint_id, slept.checkpoint_id);
}

#[tokio::test]
async fn sleeping_without_a_park_root_configured_is_the_sleep_it_always_was() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_with(directory.path(), None, 0);
    let opened = open(&engine, &repository).await;

    let slept = sleep(&engine, "sleep", &opened).await;
    assert!(slept.suspended);
    assert!(!slept.parked);
    assert_eq!(slept.parked_bytes, 0);
    assert_eq!(slept.park_path, None);
    assert_eq!(slept.park_reason.as_deref(), Some("unconfigured"));
    assert!(engine.database().parks().unwrap().is_empty());
}

#[tokio::test]
async fn build_output_too_small_to_be_worth_the_copy_is_discarded_as_before() {
    let directory = tempfile::tempdir().unwrap();
    let park_root = mounted(directory.path());
    let repository = fixture(directory.path());
    // Nothing on any disk clears this bar, so the gate is the only reason.
    let engine = engine_with(directory.path(), Some(&park_root), u64::MAX);
    let opened = open(&engine, &repository).await;

    let slept = sleep(&engine, "sleep", &opened).await;
    assert!(slept.suspended);
    assert!(!slept.parked);
    assert_eq!(slept.park_reason.as_deref(), Some("below_min_bytes"));
    assert!(engine.database().parks().unwrap().is_empty());
    assert!(!park_root.join(&slept.workspace.0).exists());
}

#[tokio::test]
async fn waking_puts_the_parked_build_output_back_and_spends_the_park() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, _opened, slept, park_root) = parked(directory.path()).await;

    let woken: WakeResult = completed(wake(&engine, "wake").await);
    assert!(woken.park_restored, "{woken:?}");
    assert_eq!(woken.park_restored_bytes, BUILD_CONTENT.len() as u64);
    assert_eq!(woken.park_reason, None);
    assert_eq!(woken.next, None, "nothing was lost, so nothing is advised");

    let successor = PathBuf::from(&woken.session.cwd);
    assert_eq!(
        fs::read_to_string(successor.join(BUILD_OUTPUT)).unwrap(),
        BUILD_CONTENT,
        "the gitignored output is back in the successor"
    );
    assert_eq!(
        fs::read_to_string(successor.join("tracked.txt")).unwrap(),
        "base\n"
    );

    // The park described a checkpoint that has stopped being a suspension.
    assert!(!park_dir(&park_root, &slept).exists());
    assert!(engine.database().parks().unwrap().is_empty());
}

#[tokio::test]
async fn a_park_can_be_kept_past_the_wake_that_spent_it() {
    let directory = tempfile::tempdir().unwrap();
    let park_root = mounted(directory.path());
    let repository = fixture(directory.path());
    let engine = {
        let mut config = EngineConfig::at(directory.path().join("state"))
            .with_harness_lifecycle_timing(1, 0)
            .unwrap();
        config.park_root = Some(park_root.clone());
        config.park_min_bytes = 0;
        config.park_keep_after_wake = true;
        Engine::with_components(
            config,
            Arc::new(CopyFilesystem),
            Arc::new(DependencyService::new(Vec::new())),
        )
        .unwrap()
    };
    let opened = open(&engine, &repository).await;
    let slept = sleep(&engine, "sleep", &opened).await;
    assert!(slept.parked);

    let woken: WakeResult = completed(wake(&engine, "wake").await);
    assert!(woken.park_restored);
    assert!(
        park_dir(&park_root, &slept).is_dir(),
        "the operator asked for the park to survive the wake"
    );
    assert_eq!(engine.database().parks().unwrap().len(), 1);

    // It is still garbage, though: the checkpoint it names stopped being a
    // suspension the moment the successor was bound.
    let collected = collect(&engine, "gc").await;
    assert_eq!(collected["parks_deleted"], 1);
    assert!(!park_dir(&park_root, &slept).exists());
}

#[tokio::test]
async fn waking_with_the_volume_unplugged_is_exactly_the_wake_it_always_was() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, _opened, _slept, park_root) = parked(directory.path()).await;

    // The disk goes home in someone's bag between the sleep and the wake.
    fs::remove_dir_all(&park_root).unwrap();

    let woken: WakeResult = completed(wake(&engine, "wake").await);
    assert!(!woken.park_restored);
    assert_eq!(woken.park_restored_bytes, 0);
    assert_eq!(woken.park_reason.as_deref(), Some("unmounted"));
    assert_eq!(
        woken.next.as_deref(),
        Some("build output not restored (unmounted); it will regenerate on the next build")
    );

    let successor = PathBuf::from(&woken.session.cwd);
    assert_eq!(
        fs::read_to_string(successor.join("tracked.txt")).unwrap(),
        "base\n",
        "the tracked tree is whole"
    );
    assert!(
        !successor.join(BUILD_OUTPUT).exists(),
        "the build output simply is not there, exactly as before the tier"
    );
    // The record survives an unplugged disk: those bytes still exist.
    assert_eq!(engine.database().parks().unwrap().len(), 1);
}

#[tokio::test]
async fn a_park_that_describes_another_tree_is_declined_rather_than_restored() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, _opened, slept, park_root) = parked(directory.path()).await;

    let manifest_path = park_dir(&park_root, &slept).join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["worktree_oid"] = serde_json::json!("0000000000000000000000000000000000000000");
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let woken: WakeResult = completed(wake(&engine, "wake").await);
    assert!(!woken.park_restored);
    assert_eq!(woken.park_reason.as_deref(), Some("manifest_mismatch"));
    assert!(
        woken
            .next
            .as_deref()
            .is_some_and(|next| next.contains("manifest_mismatch"))
    );

    let successor = PathBuf::from(&woken.session.cwd);
    assert!(!successor.join(BUILD_OUTPUT).exists());
    assert_eq!(
        fs::read_to_string(successor.join("tracked.txt")).unwrap(),
        "base\n"
    );
}

#[tokio::test]
async fn the_collector_takes_a_released_workspace_park_only_when_the_volume_is_there() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened, slept, park_root) = parked(directory.path()).await;
    let park = park_dir(&park_root, &slept);

    let released: serde_json::Value = completed(
        engine
            .execute(execute(
                "release",
                Intent::WorkspaceRelease {
                    selector: selector(&opened),
                },
            ))
            .await,
    );
    assert_eq!(released["released"], true);

    // Unplugged: the park is garbage, and nothing may act on that.
    let unplugged = directory.path().join("unplugged");
    fs::rename(&park_root, &unplugged).unwrap();
    let collected = collect(&engine, "gc-unmounted").await;
    assert_eq!(collected["parks_deleted"], 0);
    assert_eq!(collected["parks_retained"], 1);
    assert_eq!(
        engine.database().parks().unwrap().len(),
        1,
        "an absent disk is not evidence that the bytes are gone"
    );

    // Plugged back in: now the same verdict can be carried out.
    fs::rename(&unplugged, &park_root).unwrap();
    let collected = collect(&engine, "gc-mounted").await;
    assert_eq!(collected["parks_deleted"], 1);
    assert_eq!(collected["parks_retained"], 0);
    assert!(engine.database().parks().unwrap().is_empty());
    assert!(!park.exists());
}

#[tokio::test]
async fn the_collector_removes_a_park_directory_no_record_claims() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, _opened, _slept, park_root) = parked(directory.path()).await;

    // A park an older daemon published and then lost the record for.
    let orphan = park_root.join("ws_orphan").join("ckpt_orphan");
    fs::create_dir_all(orphan.join("tree")).unwrap();
    fs::write(orphan.join("manifest.json"), b"{}").unwrap();

    let collected = collect(&engine, "gc").await;
    assert_eq!(collected["parks_orphans_removed"], 1);
    assert!(!orphan.exists());
    assert_eq!(
        engine.database().parks().unwrap().len(),
        1,
        "the live suspension's own park is untouched"
    );
}

#[tokio::test]
async fn the_collector_drops_a_record_whose_directory_left_the_mounted_volume() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, _opened, slept, park_root) = parked(directory.path()).await;

    // Someone deleted it by hand while the volume was mounted.
    fs::remove_dir_all(park_dir(&park_root, &slept)).unwrap();

    let collected = collect(&engine, "gc").await;
    assert_eq!(collected["park_records_dropped"], 1);
    assert!(engine.database().parks().unwrap().is_empty());
}

#[tokio::test]
async fn doctor_reports_the_volume_the_parks_and_the_ones_pending_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let (engine, opened, _slept, park_root) = parked(directory.path()).await;

    let health: serde_json::Value = completed(engine.query(query(Query::Doctor)).await);
    assert_eq!(health["park_root"], park_root.to_string_lossy().as_ref());
    assert_eq!(health["park_mounted"], true);
    assert_eq!(health["parks"], 1);
    assert_eq!(health["park_bytes"], BUILD_CONTENT.len());
    assert_eq!(
        health["parks_orphaned"], 0,
        "a park of a live suspension is pending nothing"
    );

    // Releasing the workspace is what turns the park into garbage.
    let _: serde_json::Value = completed(
        engine
            .execute(execute(
                "release",
                Intent::WorkspaceRelease {
                    selector: selector(&opened),
                },
            ))
            .await,
    );
    let health: serde_json::Value = completed(engine.query(query(Query::Doctor)).await);
    assert_eq!(health["parks_orphaned"], 1);

    // Unplugging the volume changes the answer to `park_mounted` and nothing
    // else: the records still say what the daemon believes it owns.
    fs::remove_dir_all(&park_root).unwrap();
    let health: serde_json::Value = completed(engine.query(query(Query::Doctor)).await);
    assert_eq!(health["park_mounted"], false);
    assert_eq!(health["park_root"], park_root.to_string_lossy().as_ref());
    assert_eq!(health["parks"], 1);
}

#[tokio::test]
async fn an_unconfigured_daemon_says_the_tier_is_off_rather_than_broken() {
    let directory = tempfile::tempdir().unwrap();
    let repository = fixture(directory.path());
    let engine = engine_with(directory.path(), None, 0);
    let _opened = open(&engine, &repository).await;

    let health: serde_json::Value = completed(engine.query(query(Query::Doctor)).await);
    assert_eq!(health["park_root"], serde_json::Value::Null);
    assert_eq!(health["park_mounted"], false);
    assert_eq!(health["parks"], 0);
    assert_eq!(health["park_bytes"], 0);
    assert_eq!(health["parks_orphaned"], 0);
}
