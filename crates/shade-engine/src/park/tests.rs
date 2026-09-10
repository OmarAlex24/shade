//! Unit tests for the parked tier. Each one builds a real Git repository
//! in a temporary directory: the park set is whatever the installed Git
//! calls ignored, so a fake would test the wrong predicate.

use super::*;

const HEAD: &str = "1111111111111111111111111111111111111111";
const WORKTREE: &str = "2222222222222222222222222222222222222222";
const WORKSPACE: &str = "ws_park_test";
const CHECKPOINT: &str = "ckpt_park_test";

fn git_ok(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git is available");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A workspace with ignored build output, an ignored dependency forest, an
/// ignored secret, and one tracked file.
fn workspace(root: &Path) -> PathBuf {
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    git_ok(&workspace, &["init", "-b", "main"]);
    git_ok(&workspace, &["config", "user.name", "Shade Test"]);
    git_ok(&workspace, &["config", "user.email", "shade@test.invalid"]);
    fs::write(
        workspace.join(".gitignore"),
        "target/\nnode_modules/\ndist/\n.env\n",
    )
    .unwrap();
    fs::write(workspace.join("main.rs"), "fn main() {}\n").unwrap();
    git_ok(&workspace, &["add", ".gitignore", "main.rs"]);
    git_ok(&workspace, &["commit", "-m", "base"]);

    fs::create_dir_all(workspace.join("target/debug")).unwrap();
    fs::write(workspace.join("target/debug/app"), b"binary-bytes").unwrap();
    fs::set_permissions(
        workspace.join("target/debug/app"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::write(workspace.join("target/CACHEDIR.TAG"), b"tag").unwrap();
    fs::create_dir_all(workspace.join("node_modules/left-pad")).unwrap();
    fs::write(workspace.join("node_modules/left-pad/index.js"), b"pad").unwrap();
    fs::create_dir_all(workspace.join("dist")).unwrap();
    fs::write(workspace.join("dist/bundle.js"), b"bundle").unwrap();
    fs::write(workspace.join("dist/.env"), b"NESTED=1\n").unwrap();
    fs::write(workspace.join(".env"), b"API_TOKEN=plain\n").unwrap();
    workspace
}

fn park(root: &Path, workspace: &Path) -> ParkOutcome {
    park_tree(
        root,
        workspace,
        WORKSPACE,
        CHECKPOINT,
        HEAD,
        WORKTREE,
        &default_excluded_relative_paths(),
    )
    .unwrap()
}

#[test]
fn the_park_set_is_ignored_build_output_without_layers_or_secrets() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let set = collect_park_set(&workspace, &default_excluded_relative_paths()).unwrap();
    assert!(set.contains(&PathBuf::from("target")), "{set:?}");
    assert!(set.contains(&PathBuf::from("dist")), "{set:?}");
    assert!(!set.contains(&PathBuf::from("node_modules")), "{set:?}");
    assert!(!set.contains(&PathBuf::from(".env")), "{set:?}");
    assert!(!set.contains(&PathBuf::from(".git")), "{set:?}");
}

#[test]
fn parking_records_a_manifest_and_restores_the_same_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let park_root = temp.path().join("park");
    let outcome = park(&park_root, &workspace);

    assert_eq!(
        outcome.park_path,
        park_dir(&park_root, WORKSPACE, CHECKPOINT)
    );
    let manifest = read_manifest(&outcome.park_path).unwrap().unwrap();
    assert_eq!(manifest.version, MANIFEST_VERSION);
    assert_eq!(manifest.workspace_id, WORKSPACE);
    assert_eq!(manifest.checkpoint_id, CHECKPOINT);
    assert_eq!(manifest.head_oid, HEAD);
    assert_eq!(manifest.worktree_oid, WORKTREE);
    assert!(manifest.created_at_ms > 0);
    assert_eq!(manifest.entries.len(), outcome.entries);
    assert_eq!(
        manifest.bytes,
        manifest
            .entries
            .iter()
            .map(|entry| entry.bytes)
            .sum::<u64>()
    );
    let recorded = manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    assert!(recorded.contains("target/debug/app"), "{recorded:?}");
    assert!(recorded.contains("dist/bundle.js"), "{recorded:?}");

    let restored_into = temp.path().join("successor");
    fs::create_dir_all(&restored_into).unwrap();
    let restore = restore_park(
        &park_root,
        &restored_into,
        WORKSPACE,
        CHECKPOINT,
        HEAD,
        WORKTREE,
    )
    .unwrap();
    assert!(restore.restored);
    assert_eq!(restore.reason, None);
    assert_eq!(restore.bytes, outcome.bytes);
    for entry in &manifest.entries {
        let original = workspace.join(&entry.path);
        let copy = restored_into.join(&entry.path);
        assert_eq!(fs::read(&original).unwrap(), fs::read(&copy).unwrap());
        assert_eq!(
            fs::metadata(&original).unwrap().permissions().mode() & 0o777,
            fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            "{}",
            entry.path
        );
    }
    assert!(restored_into.join("target/debug/app").is_file());
}

#[test]
fn secrets_never_reach_the_park_at_any_depth() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let park_root = temp.path().join("park");
    let outcome = park(&park_root, &workspace);
    let manifest = read_manifest(&outcome.park_path).unwrap().unwrap();
    for entry in &manifest.entries {
        assert!(!entry.path.contains(".env"), "{}", entry.path);
    }
    assert!(!outcome.park_path.join("tree/.env").exists());
    assert!(!outcome.park_path.join("tree/dist/.env").exists());
    assert!(outcome.park_path.join("tree/dist/bundle.js").is_file());
}

#[test]
fn a_manifest_for_another_checkpoint_restores_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let park_root = temp.path().join("park");
    park(&park_root, &workspace);

    let successor = temp.path().join("successor");
    fs::create_dir_all(&successor).unwrap();
    let mismatched = restore_park(
        &park_root,
        &successor,
        WORKSPACE,
        CHECKPOINT,
        HEAD,
        "3333333333333333333333333333333333333333",
    )
    .unwrap();
    assert!(!mismatched.restored);
    assert_eq!(mismatched.reason.as_deref(), Some("manifest_mismatch"));
    assert_eq!(mismatched.bytes, 0);
    assert!(!successor.join("target/debug/app").exists());
}

#[test]
fn an_absent_park_restores_nothing_and_is_not_a_failure() {
    let temp = tempfile::tempdir().unwrap();
    let successor = temp.path().join("successor");
    fs::create_dir_all(&successor).unwrap();
    let outcome = restore_park(
        &temp.path().join("park"),
        &successor,
        WORKSPACE,
        CHECKPOINT,
        HEAD,
        WORKTREE,
    )
    .unwrap();
    assert!(!outcome.restored);
    assert_eq!(outcome.reason.as_deref(), Some("absent"));
}

#[test]
fn restoring_never_overwrites_a_path_git_tracks() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let park_root = temp.path().join("park");
    park(&park_root, &workspace);

    // A successor whose base tracks what the park holds as build output.
    let successor = temp.path().join("successor");
    fs::create_dir_all(successor.join("target/debug")).unwrap();
    git_ok(&successor, &["init", "-b", "main"]);
    git_ok(&successor, &["config", "user.name", "Shade Test"]);
    git_ok(&successor, &["config", "user.email", "shade@test.invalid"]);
    fs::write(successor.join("target/debug/app"), b"tracked-now").unwrap();
    git_ok(&successor, &["add", "target/debug/app"]);
    git_ok(&successor, &["commit", "-m", "tracks the output"]);

    let outcome = restore_park(
        &park_root, &successor, WORKSPACE, CHECKPOINT, HEAD, WORKTREE,
    )
    .unwrap();
    assert!(outcome.restored);
    assert_eq!(
        fs::read(successor.join("target/debug/app")).unwrap(),
        b"tracked-now"
    );
    assert!(successor.join("dist/bundle.js").is_file());
}

#[test]
fn parking_twice_replaces_the_park_and_leaves_no_staging_behind() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let park_root = temp.path().join("park");
    park(&park_root, &workspace);

    fs::write(workspace.join("target/debug/app"), b"rebuilt").unwrap();
    fs::remove_file(workspace.join("target/CACHEDIR.TAG")).unwrap();
    let second = park(&park_root, &workspace);

    let manifest = read_manifest(&second.park_path).unwrap().unwrap();
    let recorded = manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    assert!(!recorded.contains("target/CACHEDIR.TAG"), "{recorded:?}");
    assert_eq!(
        fs::read(second.park_path.join("tree/target/debug/app")).unwrap(),
        b"rebuilt"
    );
    let siblings = read_dir(second.park_path.parent().unwrap()).unwrap();
    assert_eq!(siblings, vec![second.park_path.clone()], "{siblings:?}");
}

#[test]
fn parks_are_listed_and_removed_by_their_directory_names() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let park_root = temp.path().join("park");
    park(&park_root, &workspace);
    fs::create_dir_all(park_dir(&park_root, "ws_broken", "ckpt_broken")).unwrap();
    fs::write(
        park_dir(&park_root, "ws_broken", "ckpt_broken").join(MANIFEST_NAME),
        b"{ not json",
    )
    .unwrap();

    let parks = list_parks(&park_root).unwrap();
    assert_eq!(parks.len(), 2, "{parks:?}");
    assert_eq!(
        parks[0],
        ParkSummary {
            workspace_id: "ws_broken".into(),
            checkpoint_id: "ckpt_broken".into(),
            bytes: 0,
            manifest_ok: false,
        }
    );
    assert_eq!(parks[1].workspace_id, WORKSPACE);
    assert!(parks[1].manifest_ok);
    assert!(parks[1].bytes > 0);

    remove_park(&park_root, WORKSPACE, CHECKPOINT).unwrap();
    assert!(!park_dir(&park_root, WORKSPACE, CHECKPOINT).exists());
    // Removing a park that is already gone is not a failure.
    remove_park(&park_root, WORKSPACE, CHECKPOINT).unwrap();
    assert_eq!(list_parks(&park_root).unwrap().len(), 1);
    assert!(list_parks(&temp.path().join("absent")).unwrap().is_empty());
}

#[test]
fn park_identifiers_may_not_escape_the_park_root() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let park_root = temp.path().join("park");
    for bad in ["", ".", "..", "../escape", "with/separator", ".hidden"] {
        assert_eq!(
            park_tree(
                &park_root,
                &workspace,
                bad,
                CHECKPOINT,
                HEAD,
                WORKTREE,
                &default_excluded_relative_paths(),
            )
            .unwrap_err()
            .code,
            PARK_FAILED,
            "{bad} should be rejected"
        );
        assert!(remove_park(&park_root, WORKSPACE, bad).is_err());
    }
}

#[test]
fn symlinks_are_skipped_rather_than_followed() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    std::os::unix::fs::symlink(
        workspace.join("main.rs"),
        workspace.join("target/link-to-source"),
    )
    .unwrap();
    let park_root = temp.path().join("park");
    let outcome = park(&park_root, &workspace);
    let manifest = read_manifest(&outcome.park_path).unwrap().unwrap();
    assert!(
        manifest
            .entries
            .iter()
            .all(|entry| entry.path != "target/link-to-source"),
        "{:?}",
        manifest.entries
    );
    assert!(
        !outcome
            .park_path
            .join("tree/target/link-to-source")
            .exists()
    );
}
