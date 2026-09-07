use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use shade_engine::filesystem::{ApfsFilesystem, CopyFilesystem, WorkspaceFilesystem};
use shade_engine::git::{
    BaseSpec, GitStore, Oid, PrepareSquashPublishRequest, SquashPublishRequest,
};

fn git(cwd: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

fn bare_git_succeeds(git_dir: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run bare git")
        .status
        .success()
}

fn init_repository(path: &Path, object_format: Option<&str>) {
    fs::create_dir(path).unwrap();
    let mut arguments = vec!["init", "-b", "main"];
    let format;
    if let Some(object_format) = object_format {
        format = format!("--object-format={object_format}");
        arguments.push(&format);
    }
    git(path, &arguments);
    git(path, &["config", "user.name", "Shade Test"]);
    git(
        path,
        &["config", "user.email", "shade-test@example.invalid"],
    );
}

fn commit_fixture(path: &Path) {
    fs::write(path.join("staged.txt"), "base\n").unwrap();
    fs::write(path.join("deleted.txt"), "delete me\n").unwrap();
    fs::write(path.join("unchanged.txt"), "unchanged\n").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "base"]);
}

#[tokio::test]
async fn checkout_policy_batches_small_blobs_and_handles_pipe_backpressure() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, Some("sha256"));
    for index in 0..2048 {
        let mut bytes = vec![b'x'; 1024];
        bytes[..8].copy_from_slice(&(index as u64).to_le_bytes());
        fs::write(source.join(format!("file-{index:04}")), bytes).unwrap();
    }
    // A large attributes file must still be inspected; NULs and newlines in
    // ordinary blobs must not be mistaken for batch protocol delimiters.
    fs::write(source.join(".gitattributes"), "# safe\n".repeat(1024)).unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "batch policy fixture"]);
    let tree = Oid::new(git(&source, &["rev-parse", "HEAD^{tree}"])).unwrap();
    let managed = shade_engine::git::ManagedRepository::new(source.join(".git"));
    let wrapper = temporary.path().join("counting-git");
    fs::write(
        &wrapper,
        "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = cat-file ]; then\n    printf 'batch\\n' >> \"$0.calls\"\n  fi\ndone\nexec git \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let store = GitStore::with_binary(&wrapper);
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        store.validate_checkout_policy(&managed, &tree),
    )
    .await
    .expect("policy scan must drain Git output while writing batch input")
    .unwrap();
    let calls = fs::read_to_string(temporary.path().join("counting-git.calls")).unwrap();
    assert!(
        calls.lines().count() <= 3,
        "one cat-file process per batch, not per blob: {}",
        calls.lines().count()
    );

    // Verify that policy rejection also reaches objects beyond the first batch.
    fs::write(
        source.join("zz-pointer"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:012345\nsize 1\n",
    )
    .unwrap();
    git(&source, &["add", "."]);
    let tree = Oid::new(git(&source, &["write-tree"])).unwrap();
    let error = store
        .validate_checkout_policy(&managed, &tree)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("UNSUPPORTED_GIT_LFS"));
    fs::remove_file(source.join("zz-pointer")).unwrap();
    fs::write(
        source.join(".gitattributes"),
        format!("{}\n*.txt filter=unsafe\n", "# safe\n".repeat(1024)),
    )
    .unwrap();
    git(&source, &["add", "--all"]);
    let tree = Oid::new(git(&source, &["write-tree"])).unwrap();
    let error = store
        .validate_checkout_policy(&managed, &tree)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("UNSUPPORTED_GIT_FILTER"));
}

#[tokio::test]
async fn immutable_base_is_created_then_incrementally_updated_with_git() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    fs::write(source.join("modified.txt"), "version-a\n").unwrap();
    fs::write(source.join("deleted.txt"), "only-a\n").unwrap();
    fs::write(source.join("mode-change"), "#!/bin/sh\nexit 0\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "base a"]);
    let commit_a = Oid::new(git(&source, &["rev-parse", "HEAD"])).unwrap();

    fs::write(source.join("modified.txt"), "version-b\n").unwrap();
    fs::remove_file(source.join("deleted.txt")).unwrap();
    fs::write(source.join("added.txt"), "only-b\n").unwrap();
    symlink("modified.txt", source.join("added-link")).unwrap();
    fs::set_permissions(
        source.join("mode-change"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "-m", "base b"]);
    let commit_b = Oid::new(git(&source, &["rev-parse", "HEAD"])).unwrap();

    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed = store
        .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
        .await
        .unwrap();
    store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let base_b = store
        .resolve_base(&managed, None, BaseSpec::ExistingOid(commit_b))
        .await
        .unwrap();
    let base_a = store
        .resolve_base(&managed, None, BaseSpec::ExistingOid(commit_a))
        .await
        .unwrap();

    let root_a = temporary.path().join("base-a");
    store
        .prepare_base(&managed, &base_a, &root_a)
        .await
        .unwrap();
    store
        .verify_materialized_tree(&managed, &base_a.tree, &root_a)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(root_a.join("modified.txt")).unwrap(),
        "version-a\n"
    );
    assert!(root_a.join("deleted.txt").is_file());
    assert_eq!(
        fs::symlink_metadata(root_a.join("mode-change"))
            .unwrap()
            .mode()
            & 0o111,
        0
    );

    // Production takes this clone through clonefile on APFS. All tree
    // changes below are then applied by Git's temporary-index transition.
    let root_b_staging = temporary.path().join(".shade-materialize-b");
    ApfsFilesystem.clone_tree(&root_a, &root_b_staging).unwrap();
    store
        .update_materialized_base(&managed, &base_a, &base_b, &root_b_staging)
        .await
        .unwrap();
    let root_b = temporary.path().join("base-b");
    fs::rename(&root_b_staging, &root_b).unwrap();

    store
        .verify_materialized_tree(&managed, &base_b.tree, &root_b)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(root_b.join("modified.txt")).unwrap(),
        "version-b\n"
    );
    assert_eq!(
        fs::read_to_string(root_b.join("added.txt")).unwrap(),
        "only-b\n"
    );
    assert!(!root_b.join("deleted.txt").exists());
    assert_eq!(
        fs::read_link(root_b.join("added-link")).unwrap(),
        Path::new("modified.txt")
    );
    assert_ne!(
        fs::symlink_metadata(root_b.join("mode-change"))
            .unwrap()
            .mode()
            & 0o111,
        0
    );

    // Updating B must never write through its COW relationship into A.
    store
        .verify_materialized_tree(&managed, &base_a.tree, &root_a)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(root_a.join("modified.txt")).unwrap(),
        "version-a\n"
    );
    assert!(root_a.join("deleted.txt").is_file());
    assert!(!root_a.join("added.txt").exists());
    assert!(!root_a.join("added-link").exists());
    assert_eq!(
        fs::symlink_metadata(root_a.join("mode-change"))
            .unwrap()
            .mode()
            & 0o111,
        0
    );
}

fn commit_tree_barrier_wrapper(path: &Path, barrier: &Path) {
    fs::create_dir(barrier).unwrap();
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    assert!(real_git.status.success());
    let real_git = String::from_utf8(real_git.stdout)
        .unwrap()
        .trim()
        .to_owned();
    let script = format!(
        r#"#!/bin/sh
case " $* " in
  *" commit-tree "*)
    if mkdir "{barrier}/first" 2>/dev/null; then
      : > "{barrier}/ready-first"
    else
      : > "{barrier}/ready-second"
    fi
    attempts=0
    while [ ! -e "{barrier}/ready-first" ] || [ ! -e "{barrier}/ready-second" ]; do
      attempts=$((attempts + 1))
      [ "$attempts" -lt 1000 ] || exit 97
      sleep 0.01
    done
    ;;
esac
exec "{real_git}" "$@"
"#,
        barrier = barrier.display()
    );
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn checkpoint_writer_wrapper(path: &Path, workspace: &Path, state: &Path) {
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    assert!(real_git.status.success());
    let real_git = String::from_utf8(real_git.stdout)
        .unwrap()
        .trim()
        .to_owned();
    let script = format!(
        r#"#!/bin/sh
"{real_git}" "$@"
status=$?
case " $* " in
  *" write-tree "*)
    if [ "$status" -eq 0 ] && [ -n "${{GIT_INDEX_FILE:-}}" ]; then
      count=0
      [ ! -f "{state}" ] || count=$(cat "{state}")
      count=$((count + 1))
      printf '%s\n' "$count" > "{state}"
      printf 'writer-%s\n' "$count" > "{workspace}/racing.txt"
    fi
    ;;
esac
exit "$status"
"#,
        workspace = workspace.display(),
        state = state.display(),
    );
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn copy_fake_and_apfs_clone_are_independent_and_preserve_shape() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested/data"), "original").unwrap();
    fs::write(source.join("executable"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(source.join("executable"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::hard_link(source.join("nested/data"), source.join("hardlink")).unwrap();
    symlink("nested/data", source.join("symlink")).unwrap();

    for (name, filesystem, immutable) in [
        ("copy", &CopyFilesystem as &dyn WorkspaceFilesystem, false),
        ("apfs", &ApfsFilesystem as &dyn WorkspaceFilesystem, false),
        (
            "immutable-hardlinks",
            &ApfsFilesystem as &dyn WorkspaceFilesystem,
            true,
        ),
    ] {
        let destination = temporary.path().join(name);
        if immutable {
            filesystem
                .clone_immutable_tree(&source, &destination)
                .unwrap();
        } else {
            filesystem.clone_tree(&source, &destination).unwrap();
        }
        fs::write(destination.join("nested/data"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(source.join("nested/data")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::symlink_metadata(destination.join("nested/data"))
                .unwrap()
                .ino(),
            fs::symlink_metadata(destination.join("hardlink"))
                .unwrap()
                .ino()
        );
        assert_eq!(
            fs::read_link(destination.join("symlink")).unwrap(),
            Path::new("nested/data")
        );
        assert_ne!(
            fs::symlink_metadata(destination.join("executable"))
                .unwrap()
                .mode()
                & 0o111,
            0
        );
        let usage = filesystem.usage(&destination).unwrap();
        assert!(usage.logical_bytes > 0);
        assert!(usage.private_bytes.is_some());
    }
}

#[test]
fn immutable_apfs_clone_preserves_shape_isolation_and_atomic_failure() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("nested/empty")).unwrap();
    fs::write(source.join("nested/executable"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        source.join("nested/executable"),
        fs::Permissions::from_mode(0o751),
    )
    .unwrap();
    fs::set_permissions(
        source.join("nested/empty"),
        fs::Permissions::from_mode(0o3750),
    )
    .unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o1750)).unwrap();
    let outside = temporary.path().join("outside");
    fs::write(&outside, "untouched").unwrap();
    symlink("../outside", source.join("external-link")).unwrap();
    symlink("missing", source.join("dangling-link")).unwrap();
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    ApfsFilesystem
        .clone_immutable_tree(&source, &first)
        .unwrap();
    ApfsFilesystem
        .clone_immutable_tree(&source, &second)
        .unwrap();
    assert_eq!(
        fs::read_link(first.join("external-link")).unwrap(),
        Path::new("../outside")
    );
    assert_eq!(
        fs::read_link(first.join("dangling-link")).unwrap(),
        Path::new("missing")
    );
    assert_eq!(
        fs::metadata(first.join("nested/executable"))
            .unwrap()
            .mode()
            & 0o777,
        0o751
    );
    assert_eq!(
        fs::metadata(first.join("nested/empty")).unwrap().mode() & 0o7777,
        0o750
    );
    assert_eq!(fs::metadata(&first).unwrap().mode() & 0o7777, 0o750);
    fs::write(first.join("nested/executable"), "changed").unwrap();
    assert_eq!(
        fs::read(source.join("nested/executable")).unwrap(),
        b"#!/bin/sh\n"
    );
    assert_eq!(
        fs::read(second.join("nested/executable")).unwrap(),
        b"#!/bin/sh\n"
    );
    assert!(
        ApfsFilesystem
            .clone_immutable_tree(&source, &first)
            .is_err()
    );
    assert_eq!(
        fs::read(first.join("nested/executable")).unwrap(),
        b"changed"
    );

    // Unsupported entries must be rejected during inventory, before a
    // recursive kernel clone can reach them or expose a partial destination.
    let pipe = std::ffi::CString::new(source.join("pipe").as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
    let rejected = temporary.path().join("rejected");
    assert!(
        ApfsFilesystem
            .clone_immutable_tree(&source, &rejected)
            .is_err()
    );
    assert!(!rejected.exists());
    assert_eq!(fs::read(outside).unwrap(), b"untouched");
    assert!(fs::read_dir(temporary.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".shade-")
    }));
}

#[test]
#[ignore = "manual APFS latency evidence probe"]
fn apfs_clone_tree_flat_latency_probe() {
    let entry_count = std::env::var("SHADE_APFS_BENCH_ENTRIES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(25_000_usize);
    let sample_count = std::env::var("SHADE_APFS_BENCH_SAMPLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20_usize);
    assert!(entry_count > 0 && sample_count > 0);

    let temporary = tempfile::Builder::new()
        .prefix("shade-apfs-latency-")
        .tempdir_in("/tmp")
        .unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    for index in 0..entry_count {
        // Match the release gate's incompressible deterministic payload.
        let mut state = index as u64 ^ 0x9e37_79b9_7f4a_7c15;
        let payload: Vec<u8> = (0..1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        fs::write(source.join(format!("entry-{index:05}.bin")), &payload).unwrap();
    }
    fs::File::open(&source).unwrap().sync_all().unwrap();
    let warmup = temporary.path().join("warmup");
    ApfsFilesystem
        .clone_immutable_tree(&source, &warmup)
        .unwrap();
    ApfsFilesystem.remove_tree(&warmup).unwrap();

    let mut samples = Vec::with_capacity(sample_count);
    let mut destinations = Vec::with_capacity(sample_count);
    for index in 0..sample_count {
        let destination = temporary.path().join(format!("sample-{index:03}"));
        let started = Instant::now();
        ApfsFilesystem
            .clone_immutable_tree(&source, &destination)
            .unwrap();
        samples.push(started.elapsed().as_micros());
        destinations.push(destination);
    }
    // Keep all clones alive together, as the release gate and concurrent
    // workspaces do. Removing each sample hides sustained APFS metadata work.
    for destination in destinations {
        ApfsFilesystem.remove_tree(&destination).unwrap();
    }
    samples.sort_unstable();
    let nearest_rank = |percentile: usize| {
        let rank = (percentile * samples.len()).div_ceil(100);
        samples[rank.saturating_sub(1).min(samples.len() - 1)]
    };
    eprintln!(
        "apfs_clone_immutable_tree entries={entry_count} samples_us={samples:?} p50_us={} p95_us={}",
        nearest_rank(50),
        nearest_rank(95)
    );
    assert!(
        nearest_rank(50) < 300_000,
        "APFS materialization p50 must be below 300 ms"
    );
    assert!(
        nearest_rank(95) < 1_000_000,
        "APFS materialization p95 must be below 1 s"
    );
}

#[test]
fn tree_publication_is_exclusive_and_never_replaces_a_winner() {
    let temporary = tempfile::tempdir().unwrap();
    let staging = temporary.path().join(".shade-materialize-racer");
    let destination = temporary.path().join("base");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("value"), "loser\n").unwrap();
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("value"), "winner\n").unwrap();

    assert!(CopyFilesystem.publish_tree(&staging, &destination).is_err());
    assert_eq!(
        fs::read_to_string(destination.join("value")).unwrap(),
        "winner\n"
    );
    assert_eq!(
        fs::read_to_string(staging.join("value")).unwrap(),
        "loser\n"
    );
}

#[tokio::test]
async fn checkpoint_preserves_index_worktree_deletions_and_untracked_without_dotenv() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    commit_fixture(&source);

    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed_path = temporary.path().join("managed.git");
    let managed = store
        .create_managed_bare(&managed_path, Some(&remote))
        .await
        .unwrap();
    let base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let template = temporary.path().join("template");
    store
        .prepare_base(&managed, &base, &template)
        .await
        .unwrap();
    let workspace = temporary.path().join("workspace");
    CopyFilesystem.clone_tree(&template, &workspace).unwrap();
    store
        .register_precloned_worktree(&managed, &workspace, &base, "test")
        .await
        .unwrap();

    fs::write(workspace.join("staged.txt"), "staged\n").unwrap();
    git(&workspace, &["add", "staged.txt"]);
    fs::write(workspace.join("staged.txt"), "working\n").unwrap();
    fs::remove_file(workspace.join("deleted.txt")).unwrap();
    fs::write(workspace.join("untracked.txt"), "untracked\n").unwrap();
    fs::write(workspace.join("new-executable"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(
        workspace.join("new-executable"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("untracked.txt", workspace.join("new-link")).unwrap();
    fs::write(workspace.join(".env.local"), "TOKEN=do-not-hash\n").unwrap();
    fs::create_dir_all(workspace.join("node_modules/pkg")).unwrap();
    fs::write(workspace.join("node_modules/pkg/index.js"), "dependency\n").unwrap();
    fs::create_dir_all(workspace.join(".venv/lib/python/site-packages")).unwrap();
    fs::write(
        workspace.join(".venv/lib/python/site-packages/dependency.py"),
        "dependency\n",
    )
    .unwrap();

    let before = store.status(&workspace).await.unwrap();
    assert_eq!(before.staged, 1);
    assert!(before.unstaged >= 2);
    assert!(before.untracked >= 4);
    let checkpoint = store
        .checkpoint(&managed, &workspace, "workspace_1", "checkpoint_1")
        .await
        .unwrap();

    let working_names = git(
        temporary.path(),
        &[
            &format!("--git-dir={}", managed.git_dir().display()),
            "ls-tree",
            "-r",
            "--name-only",
            checkpoint.working_tree.as_str(),
        ],
    );
    assert!(working_names.lines().any(|path| path == "untracked.txt"));
    assert!(!working_names.lines().any(|path| path.starts_with(".env")));
    assert!(
        !working_names
            .lines()
            .any(|path| path.contains("node_modules"))
    );
    assert!(!working_names.lines().any(|path| path.contains(".venv")));
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed.git_dir().display()),
                "show",
                &format!("{}:staged.txt", checkpoint.index_tree),
            ],
        ),
        "staged"
    );

    fs::write(workspace.join("staged.txt"), "later\n").unwrap();
    fs::remove_file(workspace.join("untracked.txt")).unwrap();
    fs::write(workspace.join(".env.local"), "TOKEN=still-local\n").unwrap();
    store
        .restore_checkpoint(&workspace, &checkpoint, true)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("staged.txt")).unwrap(),
        "working\n"
    );
    assert_eq!(git(&workspace, &["show", ":staged.txt"]), "staged");
    assert!(!workspace.join("deleted.txt").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("untracked.txt")).unwrap(),
        "untracked\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.join(".env.local")).unwrap(),
        "TOKEN=still-local\n"
    );
    assert!(
        fs::symlink_metadata(workspace.join("new-link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_ne!(
        fs::symlink_metadata(workspace.join("new-executable"))
            .unwrap()
            .mode()
            & 0o111,
        0
    );

    let restored = store.status(&workspace).await.unwrap();
    assert_eq!(restored.staged, 1);
    assert!(restored.unstaged >= 2);
    assert!(restored.untracked >= 4);
    assert!(restored.unmerged_paths.is_empty());
    store
        .remove_worktree(&managed, &workspace, true)
        .await
        .unwrap();
}

#[tokio::test]
async fn checkpoint_excludes_ignored_dependency_directories() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    fs::write(
        source.join(".gitignore"),
        ".venv/\nnode_modules/\ntarget/\n.env*\nprivate.json\n",
    )
    .unwrap();
    commit_fixture(&source);
    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed = store
        .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
        .await
        .unwrap();
    let base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let workspace = temporary.path().join("workspace");
    store
        .prepare_base(&managed, &base, &workspace)
        .await
        .unwrap();
    store
        .register_precloned_worktree(&managed, &workspace, &base, "ignored dependencies")
        .await
        .unwrap();
    for path in [
        ".venv/lib/dependency.py",
        "node_modules/pkg/index.js",
        "nested/.venv/lib/dependency.py",
        "nested/node_modules/pkg/index.js",
        "target/generated",
    ] {
        fs::create_dir_all(workspace.join(path).parent().unwrap()).unwrap();
        fs::write(workspace.join(path), "dependency\n").unwrap();
    }
    fs::write(workspace.join(".env.local"), "TOKEN=do-not-hash\n").unwrap();
    // Secret content outside dotenv paths is kept private whether ignored or
    // visible to Git. Glob metacharacters in names must stay literal.
    let private =
        "{\"api_key\":\"sk-proj-shade-regression-abcdefghijklmnopqrstuvwxyz0123456789\"}\n";
    for path in ["private.json", "visible[private].json"] {
        fs::write(workspace.join(path), private).unwrap();
    }
    fs::write(workspace.join("visiblep.json"), "public\n").unwrap();
    fs::write(workspace.join("staged.txt"), "staged\n").unwrap();
    git(&workspace, &["add", "staged.txt"]);
    fs::write(workspace.join("staged.txt"), "working\n").unwrap();
    fs::remove_file(workspace.join("deleted.txt")).unwrap();
    fs::write(workspace.join("untracked.txt"), "keep me\n").unwrap();
    let checkpoint = store
        .checkpoint(
            &managed,
            &workspace,
            "workspace_ignored",
            "checkpoint_ignored",
        )
        .await
        .unwrap();
    let names = git(
        temporary.path(),
        &[
            &format!("--git-dir={}", managed.git_dir().display()),
            "ls-tree",
            "-r",
            "--name-only",
            checkpoint.working_tree.as_str(),
        ],
    );
    assert!(names.lines().any(|path| path == "untracked.txt"));
    assert!(names.lines().any(|path| path == "visiblep.json"));
    assert!(!names.lines().any(|path| path.contains(".venv")
        || path.contains("node_modules")
        || path.starts_with("target/")
        || path.contains("private")
        || path.starts_with(".env")
        || path == "deleted.txt"));
    assert_eq!(git(&workspace, &["show", ":staged.txt"]), "staged");
    assert_eq!(
        fs::read_to_string(workspace.join("staged.txt")).unwrap(),
        "working\n"
    );
    for path in ["private.json", "visible[private].json"] {
        assert_eq!(fs::read_to_string(workspace.join(path)).unwrap(), private);
    }
    for path in [
        "private.json",
        "visible[private].json",
        ".env.local",
        ".venv/lib/dependency.py",
    ] {
        let oid = git(&workspace, &["hash-object", "--no-filters", path]);
        assert!(
            !bare_git_succeeds(managed.git_dir(), &["cat-file", "-e", &oid]),
            "excluded content must never enter the object database: {path}"
        );
    }
    assert_eq!(
        fs::read_to_string(workspace.join(".venv/lib/dependency.py")).unwrap(),
        "dependency\n"
    );
}

/// A path an exclusion pathspec matches counts as explicitly named, and Git
/// refuses to add an explicitly named ignored path. The blanket
/// `:(exclude,glob).env*` therefore failed the whole checkpoint in the most
/// ordinary repository there is: one whose `.gitignore` lists `.env` while an
/// untracked `.env` sits in the tree. Whether the name is gitignored decides
/// nothing about the exclusion -- both trees below keep their private files
/// and their dependency forest out of the checkpoint, and both succeed.
#[tokio::test]
async fn checkpoint_keeps_private_and_dependency_paths_out_whether_or_not_gitignored() {
    for ignored in [true, false] {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        init_repository(&source, None);
        fs::write(
            source.join(".gitignore"),
            if ignored {
                ".env\nnode_modules/\n.venv/\n"
            } else {
                "unrelated-output/\n"
            },
        )
        .unwrap();
        fs::write(source.join(".env.example"), "API_URL=http://localhost\n").unwrap();
        commit_fixture(&source);
        let store = GitStore::system();
        let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
        let managed = store
            .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
            .await
            .unwrap();
        let base = store
            .resolve_base(
                &managed,
                Some(&remote),
                BaseSpec::OriginBranch("main".to_owned()),
            )
            .await
            .unwrap();
        let workspace = temporary.path().join("workspace");
        store
            .prepare_base(&managed, &base, &workspace)
            .await
            .unwrap();
        store
            .register_precloned_worktree(&managed, &workspace, &base, "dotenv exclusion")
            .await
            .unwrap();

        fs::write(workspace.join(".env"), "FOO=bar\n").unwrap();
        fs::write(workspace.join(".env.local"), "FOO=baz\n").unwrap();
        for path in ["node_modules/pkg/index.js", ".venv/lib/dependency.py"] {
            fs::create_dir_all(workspace.join(path).parent().unwrap()).unwrap();
            fs::write(workspace.join(path), "dependency\n").unwrap();
        }
        // A tracked template is ordinary content and its edit is checkpointed.
        fs::write(workspace.join(".env.example"), "API_URL=http://127.0.0.1\n").unwrap();
        fs::write(workspace.join("untracked.txt"), "keep me\n").unwrap();

        let checkpoint = store
            .checkpoint(
                &managed,
                &workspace,
                "workspace_dotenv",
                "checkpoint_dotenv",
            )
            .await
            .unwrap_or_else(|error| panic!("ignored={ignored}: {error}"));
        let names = git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed.git_dir().display()),
                "ls-tree",
                "-r",
                "--name-only",
                checkpoint.working_tree.as_str(),
            ],
        );
        let staged = names.lines().collect::<Vec<_>>();
        assert!(staged.contains(&"untracked.txt"), "ignored={ignored}");
        assert!(staged.contains(&".env.example"), "ignored={ignored}");
        assert!(
            !staged
                .iter()
                .any(|path| *path == ".env" || *path == ".env.local"),
            "ignored={ignored}: a private dotenv was staged: {staged:?}"
        );
        assert!(
            !staged
                .iter()
                .any(|path| path.contains("node_modules") || path.contains(".venv")),
            "ignored={ignored}: dependency output was staged: {staged:?}"
        );
        assert_eq!(
            git(
                temporary.path(),
                &[
                    &format!("--git-dir={}", managed.git_dir().display()),
                    "show",
                    &format!("{}:.env.example", checkpoint.working_tree),
                ],
            ),
            "API_URL=http://127.0.0.1"
        );
        for path in [".env", ".env.local", "node_modules/pkg/index.js"] {
            let oid = git(&workspace, &["hash-object", "--no-filters", path]);
            assert!(
                !bare_git_succeeds(managed.git_dir(), &["cat-file", "-e", &oid]),
                "ignored={ignored}: excluded content entered the object database: {path}"
            );
        }
    }
}

#[tokio::test]
async fn checkpoint_fails_without_refs_when_the_workspace_never_quiesces() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    fs::write(source.join("racing.txt"), "base\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "base"]);

    let system = GitStore::system();
    let remote = system
        .canonicalize_remote(source.to_str().unwrap())
        .unwrap();
    let managed = system
        .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
        .await
        .unwrap();
    let base = system
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let workspace = temporary.path().join("workspace");
    system
        .prepare_base(&managed, &base, &workspace)
        .await
        .unwrap();
    system
        .register_precloned_worktree(&managed, &workspace, &base, "checkpoint fence")
        .await
        .unwrap();

    let wrapper = temporary.path().join("mutating-git");
    checkpoint_writer_wrapper(
        &wrapper,
        &workspace,
        &temporary.path().join("mutation-count"),
    );
    let error = GitStore::with_binary(wrapper)
        .checkpoint(
            &managed,
            &workspace,
            "workspace_fenced",
            "checkpoint_fenced",
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("WORKSPACE_NOT_QUIESCENT"), "{error}");
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed.git_dir().display()),
                "for-each-ref",
                "--format=%(refname)",
                "refs/shade/workspaces/workspace_fenced",
            ],
        ),
        "",
        "an unstable capture must not publish retention refs"
    );
}

#[tokio::test]
async fn linked_workspace_guardrail_blocks_porcelain_and_checkpoint_rejects_a_bypass() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    commit_fixture(&source);

    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed_path = temporary.path().join("managed.git");
    let managed = store
        .create_managed_bare(&managed_path, Some(&remote))
        .await
        .unwrap();
    let base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let workspace = temporary.path().join("workspace");
    store
        .prepare_base(&managed, &base, &workspace)
        .await
        .unwrap();
    store
        .register_precloned_worktree(&managed, &workspace, &base, "secret-filter-test")
        .await
        .unwrap();

    fs::write(workspace.join(".env"), "TOKEN=top-level-secret\n").unwrap();
    fs::create_dir_all(workspace.join("nested")).unwrap();
    fs::write(workspace.join("nested/.env.local"), "TOKEN=nested-secret\n").unwrap();
    let top_level_oid = git(&workspace, &["hash-object", "--no-filters", "--", ".env"]);
    let nested_oid = git(
        &workspace,
        &["hash-object", "--no-filters", "--", "nested/.env.local"],
    );
    for oid in [&top_level_oid, &nested_oid] {
        assert!(
            !bare_git_succeeds(&managed_path, &["cat-file", "-e", oid]),
            "hash-object without -w must not populate the managed ODB"
        );
    }

    for (path, oid) in [(".env", &top_level_oid), ("nested/.env.local", &nested_oid)] {
        let add = Command::new("git")
            .args(["add", "-f", "--", path])
            .current_dir(&workspace)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run force add");
        assert!(
            !add.status.success(),
            "required shade-secret clean filter must reject forced add of {path}"
        );
        assert!(
            !String::from_utf8_lossy(&add.stderr).contains("top-level-secret")
                && !String::from_utf8_lossy(&add.stderr).contains("nested-secret"),
            "Git diagnostics must not echo secret values"
        );
        assert!(
            !bare_git_succeeds(&managed_path, &["cat-file", "-e", oid]),
            "failed clean filtering of {path} must occur before Git writes its blob"
        );
    }
    assert_eq!(
        git(&workspace, &["ls-files", "--", ".env", "nested/.env.local"]),
        "",
        "rejected dotenv paths must not enter the index"
    );
    fs::write(workspace.join("normal.txt"), "normal change\n").unwrap();
    git(&workspace, &["config", "user.name", "Shade Test"]);
    git(
        &workspace,
        &["config", "user.email", "shade-test@example.invalid"],
    );
    git(&workspace, &["add", "normal.txt"]);
    git(&workspace, &["commit", "-m", "normal change"]);
    let normal_commit = git(&workspace, &["rev-parse", "HEAD"]);
    assert_eq!(
        git(&workspace, &["show", "HEAD:normal.txt"]),
        "normal change"
    );
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed_path.display()),
                "show",
                &format!("{normal_commit}:normal.txt"),
            ],
        ),
        "normal change"
    );
    let (observed, observed_head) = store.context_state(&workspace).await.unwrap();
    assert_eq!(observed.staged, 0);
    assert_eq!(observed.unstaged, 0);
    assert!(observed.untracked >= 2);
    assert_eq!(observed_head.as_str(), normal_commit);

    // Same-UID arbitrary Git plumbing is explicitly outside V1's isolation
    // boundary: command-scope config can replace the clean driver. Shade must
    // still fail closed before it creates any private retention ref.
    let bypass = Command::new("git")
        .args([
            "-c",
            "filter.shade-secret.clean=cat",
            "add",
            "-f",
            "--",
            ".env",
        ])
        .current_dir(&workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run explicit same-UID filter bypass");
    assert!(bypass.status.success());
    assert!(bare_git_succeeds(
        &managed_path,
        &["cat-file", "-e", &top_level_oid]
    ));

    let error = store
        .checkpoint(&managed, &workspace, "ws_boundary", "ckpt_boundary")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("TRACKED_SECRET_FILE"),
        "unexpected checkpoint rejection: {error}"
    );
    assert!(!error.contains("top-level-secret"));
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed_path.display()),
                "for-each-ref",
                "--format=%(refname)",
                "refs/shade/",
            ],
        ),
        "",
        "checkpoint rejection must happen before anchoring a private ref"
    );
}

#[tokio::test]
async fn sha256_import_and_opaque_oid_resolution_do_not_assume_sha1_width() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("sha256-source");
    init_repository(&source, Some("sha256"));
    commit_fixture(&source);
    let store = GitStore::system();
    let source_identity = store.canonicalize_local(&source).await.unwrap();
    let managed_path = temporary.path().join("managed-sha256.git");
    let (managed, base) = store
        .import_managed_bare(
            &source,
            &managed_path,
            "main",
            "refs/heads/main",
            &source_identity.remote,
        )
        .await
        .unwrap();
    assert_eq!(base.commit.as_str().len(), 64);
    let opaque = Oid::new(base.commit.as_str()).unwrap();
    let resolved = store
        .resolve_base(&managed, None, BaseSpec::ExistingOid(opaque))
        .await
        .unwrap();
    assert_eq!(resolved.commit, base.commit);
    assert!(Oid::new("abc123").is_ok());
}

/// `.env.example` and its siblings are committed placeholders, tracked on
/// purpose in most repositories. They have to survive open and checkpoint as
/// ordinary content while `.env`, `.env.local` and `.env.production` keep
/// their blanket refusal, including the required clean filter that stops a
/// forced `git add` before Git writes the blob.
#[tokio::test]
async fn env_templates_are_ordinary_content_while_real_dotenv_files_stay_out() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    fs::create_dir_all(source.join("apps/web")).unwrap();
    fs::write(
        source.join("apps/web/.env.example"),
        "API_URL=http://localhost:3000\nAPI_KEY=your-api-key-here\n",
    )
    .unwrap();
    fs::write(source.join(".env.sample"), "PORT=3000\n").unwrap();
    commit_fixture(&source);

    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed_path = temporary.path().join("managed.git");
    let managed = store
        .create_managed_bare(&managed_path, Some(&remote))
        .await
        .unwrap();
    let base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let workspace = temporary.path().join("workspace");
    store
        .prepare_base(&managed, &base, &workspace)
        .await
        .unwrap();
    store
        .register_precloned_worktree(&managed, &workspace, &base, "env-template-test")
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("apps/web/.env.example")).unwrap(),
        "API_URL=http://localhost:3000\nAPI_KEY=your-api-key-here\n",
        "a tracked template must check out like any other file"
    );

    // An edit to the template, a brand new untracked one, and two private
    // files that must not follow them into the checkpoint.
    fs::write(
        workspace.join("apps/web/.env.example"),
        "API_URL=http://localhost:4000\nAPI_KEY=your-api-key-here\n",
    )
    .unwrap();
    fs::write(workspace.join(".env.dist"), "PORT=8080\n").unwrap();
    fs::write(workspace.join("apps/web/.env.local"), "TOKEN=do-not-hash\n").unwrap();
    fs::write(workspace.join(".env.production"), "TOKEN=also-private\n").unwrap();

    let checkpoint = store
        .checkpoint(&managed, &workspace, "workspace_1", "checkpoint_1")
        .await
        .unwrap();
    let working_names = git(
        temporary.path(),
        &[
            &format!("--git-dir={}", managed.git_dir().display()),
            "ls-tree",
            "-r",
            "--name-only",
            checkpoint.working_tree.as_str(),
        ],
    );
    let names: Vec<&str> = working_names.lines().collect();
    for template in ["apps/web/.env.example", ".env.sample", ".env.dist"] {
        assert!(
            names.contains(&template),
            "{template} must be checkpointed as ordinary content: {names:?}"
        );
    }
    for private in ["apps/web/.env.local", ".env.production"] {
        assert!(
            !names.contains(&private),
            "{private} must never enter a checkpoint tree: {names:?}"
        );
    }
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed.git_dir().display()),
                "show",
                &format!("{}:apps/web/.env.example", checkpoint.working_tree),
            ],
        ),
        "API_URL=http://localhost:4000\nAPI_KEY=your-api-key-here",
        "an edit to a template belongs in the checkpoint"
    );

    // The required clean filter now distinguishes the two by name as well.
    for (path, staged) in [
        ("apps/web/.env.example", true),
        (".env.dist", true),
        ("apps/web/.env.local", false),
        (".env.production", false),
    ] {
        let add = Command::new("git")
            .args(["add", "-f", "--", path])
            .current_dir(&workspace)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run force add");
        assert_eq!(
            add.status.success(),
            staged,
            "unexpected `git add -f {path}`: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        assert!(!String::from_utf8_lossy(&add.stderr).contains("do-not-hash"));
    }
    assert_eq!(
        git(&workspace, &["ls-files", "--", ".env.production"]),
        "",
        "a private file rejected by the clean filter must not enter the index"
    );

    // And the open-time refusal is unchanged for a repository that really does
    // track a private file.
    let hostile = temporary.path().join("hostile");
    init_repository(&hostile, None);
    fs::write(hostile.join(".env.production"), "TOKEN=tracked-secret\n").unwrap();
    fs::write(hostile.join(".env.example"), "TOKEN=your-token-here\n").unwrap();
    git(&hostile, &["add", "."]);
    git(&hostile, &["commit", "-m", "tracked secret"]);
    let identity = store.canonicalize_local(&hostile).await.unwrap();
    let error = store
        .import_managed_bare(
            &hostile,
            &temporary.path().join("hostile.git"),
            "main",
            "refs/heads/main",
            &identity.remote,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("TRACKED_SECRET_FILE") && error.contains(".env.production"),
        "unexpected rejection: {error}"
    );
    assert!(!error.contains("tracked-secret"));
}

#[tokio::test]
async fn local_import_rejects_tracked_dotenv_before_publishing_the_bare() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("malicious-local");
    init_repository(&source, None);
    fs::write(source.join("safe.txt"), "safe\n").unwrap();
    fs::write(source.join(".env.production"), "TOKEN=local-secret\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "tracked secret"]);
    let secret_blob = git(&source, &["rev-parse", "HEAD:.env.production"]);

    let store = GitStore::system();
    let source_identity = store.canonicalize_local(&source).await.unwrap();
    let managed_path = temporary.path().join("managed-local.git");
    let error = store
        .import_managed_bare(
            &source,
            &managed_path,
            "main",
            "refs/heads/main",
            &source_identity.remote,
        )
        .await
        .unwrap_err();

    let error = error.to_string();
    assert!(error.contains("TRACKED_SECRET_FILE"));
    assert!(error.contains(".env.production"));
    assert!(!error.contains("local-secret"));
    assert!(
        !managed_path.exists(),
        "a rejected local import must not publish its bare repository"
    );
    assert!(
        fs::read_dir(temporary.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".shade-git-")),
        "temporary import and quarantine repositories must be removed"
    );
    assert!(!secret_blob.is_empty());
}

/// Committed content is the repository owner's decision. A source file that
/// carries a private-key header inside a redaction test, or a documented
/// `password = "..."`, is ordinary tracked content: the repository opens, and
/// what the base tree carries is surveyed by path so `doctor` can report it.
#[tokio::test]
async fn tracked_content_signatures_are_surveyed_rather_than_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("content-signature-source");
    init_repository(&source, None);
    fs::create_dir_all(source.join("apps/service/src")).unwrap();
    fs::write(
        source.join("apps/service/src/main.rs"),
        format!(
            "#[test]\nfn redacts() {{\n    let sample = \"{}{}\";\n    let password = \"hunter2hunter2\";\n    assert!(redact(sample, password));\n}}\n",
            "-----BEGIN ", "PRIVATE KEY-----"
        ),
    )
    .unwrap();
    fs::write(
        source.join(".env.example"),
        "DATABASE_URL=postgres://user:hunter2hunter2@localhost:5432/app\n",
    )
    .unwrap();
    fs::create_dir_all(source.join("ops")).unwrap();
    fs::write(
        source.join("ops/ci.yaml"),
        "env:\n  password = \"p9F2vQ7xR4tL0nB6\"\n",
    )
    .unwrap();
    // A blob past the streaming bound is surveyed by the same scanner.
    let mut oversized = vec![0xff; 2 * 1024 * 1024 - 7];
    oversized.extend_from_slice(["-----BEGIN ", "PRIVATE KEY-----"].concat().as_bytes());
    fs::write(source.join("fixture.bin"), &oversized).unwrap();
    fs::write(source.join("safe.txt"), "ordinary\n").unwrap();
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "-m", "committed fixtures"]);

    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed_path = temporary.path().join("managed.git");
    let managed = store
        .create_managed_bare(&managed_path, Some(&remote))
        .await
        .unwrap();
    let base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .expect("committed content must not refuse a repository");
    let workspace = temporary.path().join("workspace");
    store
        .prepare_base(&managed, &base, &workspace)
        .await
        .unwrap();
    assert!(workspace.join("apps/service/src/main.rs").is_file());
    assert_eq!(fs::read(workspace.join("fixture.bin")).unwrap(), oversized);

    let survey = store
        .survey_tracked_secrets(&managed, &base.tree)
        .await
        .unwrap();
    let paths: Vec<String> = survey
        .paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    assert_eq!(survey.matched, 4, "surveyed paths: {paths:?}");
    for expected in [
        "apps/service/src/main.rs",
        ".env.example",
        "ops/ci.yaml",
        "fixture.bin",
    ] {
        assert!(
            paths.iter().any(|path| path == expected),
            "{expected} must be surveyed: {paths:?}"
        );
    }
    // A tip that carries none of them surveys clean.
    git(
        &source,
        &[
            "rm",
            "-q",
            "--",
            "apps/service/src/main.rs",
            ".env.example",
            "ops/ci.yaml",
            "fixture.bin",
        ],
    );
    git(&source, &["commit", "-m", "clean tip"]);
    let clean_base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let clean = store
        .survey_tracked_secrets(&managed, &clean_base.tree)
        .await
        .unwrap();
    assert_eq!(clean.matched, 0);
    assert!(clean.paths.is_empty());
}

/// A repository whose history once tracked `.env.production` opens: the tip
/// is what Shade checks out, and an ancestor the owner already moved past is
/// no longer a reason to refuse the whole repository. A tracked private file
/// in the base tree still is, and the rejection names it.
#[tokio::test]
async fn dotenv_in_history_opens_while_the_base_tree_still_decides() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("dotenv-history");
    init_repository(&source, None);
    fs::write(source.join(".env.production"), "TOKEN=remote-secret\n").unwrap();
    git(&source, &["add", ".env.production"]);
    git(&source, &["commit", "-m", "tracked secret"]);
    let secret_blob = git(&source, &["rev-parse", "HEAD:.env.production"]);
    fs::remove_file(source.join(".env.production")).unwrap();
    fs::write(source.join("safe.txt"), "tip tree is clean\n").unwrap();
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "-m", "untrack the secret"]);

    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed_path = temporary.path().join("managed-history.git");
    let managed = store
        .create_managed_bare(&managed_path, Some(&remote))
        .await
        .unwrap();
    let base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .expect("an ancestor's dotenv must not refuse the repository");
    let workspace = temporary.path().join("workspace");
    store
        .prepare_base(&managed, &base, &workspace)
        .await
        .unwrap();
    assert!(!workspace.join(".env.production").exists());
    assert!(bare_git_succeeds(
        &managed_path,
        &["cat-file", "-e", &secret_blob]
    ));
    assert!(bare_git_succeeds(
        &managed_path,
        &["rev-parse", "--verify", "refs/remotes/origin/main"]
    ));
    assert!(
        !store
            .survey_tracked_secrets(&managed, &base.tree)
            .await
            .unwrap()
            .paths
            .iter()
            .any(|path| path.ends_with(".env.production")),
        "the survey reads the base tree, not history"
    );

    // Bringing it back to the tip refuses the repository again, by name.
    fs::write(source.join(".env.production"), "TOKEN=back-again\n").unwrap();
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "-m", "track it again"]);
    let error = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "TRACKED_SECRET_FILE: .env.production");
    assert!(!error.contains("back-again"));
}

/// Ancestor trees are still inspected for the features Shade cannot
/// materialize at all, and those rejections name their path too.
#[tokio::test]
async fn history_still_refuses_gitlinks_lfs_and_declared_filters() {
    let store = GitStore::system();
    for (name, path, contents, code) in [
        (
            "lfs-history",
            "assets/video.mp4",
            "version https://git-lfs.github.com/spec/v1\noid sha256:012345\nsize 1\n",
            "UNSUPPORTED_GIT_LFS",
        ),
        (
            "filter-history",
            "packages/api/.gitattributes",
            "*.bin filter=custom\n",
            "UNSUPPORTED_GIT_FILTER",
        ),
        (
            "lfsconfig-history",
            ".lfsconfig",
            "[lfs]\n",
            "UNSUPPORTED_GIT_LFS",
        ),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join(name);
        init_repository(&source, None);
        let file = source.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, contents).unwrap();
        git(&source, &["add", "-A"]);
        git(&source, &["commit", "-m", "unsupported ancestor"]);
        fs::remove_file(&file).unwrap();
        fs::write(source.join("safe.txt"), "clean tip\n").unwrap();
        git(&source, &["add", "-A"]);
        git(&source, &["commit", "-m", "clean tip"]);

        let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
        let managed = store
            .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
            .await
            .unwrap();
        let error = store
            .resolve_base(
                &managed,
                Some(&remote),
                BaseSpec::OriginBranch("main".to_owned()),
            )
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, format!("{code}: {path}"), "{name}");
    }
}

#[tokio::test]
async fn successor_conflict_leaves_an_unmerged_index() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    commit_fixture(&source);
    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed = store
        .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
        .await
        .unwrap();
    let base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let original = temporary.path().join("original");
    store
        .prepare_base(&managed, &base, &original)
        .await
        .unwrap();
    store
        .register_precloned_worktree(&managed, &original, &base, "original")
        .await
        .unwrap();
    fs::write(original.join("staged.txt"), "agent side\n").unwrap();
    let checkpoint = store
        .checkpoint(&managed, &original, "workspace_2", "checkpoint_1")
        .await
        .unwrap();

    fs::write(source.join("staged.txt"), "remote side\n").unwrap();
    git(&source, &["add", "staged.txt"]);
    git(&source, &["commit", "-m", "remote change"]);
    let new_base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let successor = temporary.path().join("successor");
    store
        .prepare_base(&managed, &new_base, &successor)
        .await
        .unwrap();
    store
        .register_precloned_worktree(&managed, &successor, &new_base, "successor")
        .await
        .unwrap();
    let outcome = store
        .integrate_checkpoint(&managed, &successor, &base.commit, &new_base, &checkpoint)
        .await
        .unwrap();
    assert!(!outcome.clean);
    assert_eq!(outcome.paths, vec![Path::new("staged.txt").to_path_buf()]);
    let status = store.status(&successor).await.unwrap();
    assert_eq!(status.unmerged_paths, outcome.paths);
    assert!(
        fs::read_to_string(successor.join("staged.txt"))
            .unwrap()
            .contains("<<<<<<<")
    );
}

#[tokio::test]
async fn squash_publish_uses_local_cas_and_an_absent_branch_lease() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    commit_fixture(&source);
    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed = store
        .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
        .await
        .unwrap();
    let base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let workspace = temporary.path().join("workspace");
    store
        .prepare_base(&managed, &base, &workspace)
        .await
        .unwrap();
    store
        .register_precloned_worktree(&managed, &workspace, &base, "publish")
        .await
        .unwrap();
    fs::write(workspace.join("feature.txt"), "feature\n").unwrap();
    let checkpoint = store
        .checkpoint(&managed, &workspace, "workspace_3", "checkpoint_1")
        .await
        .unwrap();

    let local = store
        .squash_publish(
            &managed,
            SquashPublishRequest {
                remote: &remote,
                branch: "main",
                original_base: &base.commit,
                expected_remote: Some(&base.commit),
                expected_local: None,
                checkpoint: &checkpoint,
                message: "local squash\n",
                push: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed.git_dir().display()),
                "rev-parse",
                "refs/heads/main",
            ],
        ),
        local.commit.as_str()
    );
    assert_eq!(git(&source, &["rev-parse", "main"]), base.commit.as_str());

    let published = store
        .squash_publish(
            &managed,
            SquashPublishRequest {
                remote: &remote,
                branch: "feature",
                original_base: &base.commit,
                expected_remote: None,
                expected_local: None,
                checkpoint: &checkpoint,
                message: "new branch\n",
                push: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        git(&source, &["rev-parse", "refs/heads/feature"]),
        published.commit.as_str()
    );
    assert_eq!(
        store.remote_branch_oid(&remote, "feature").await.unwrap(),
        Some(published.commit)
    );
    assert!(
        git(
            &source,
            &["for-each-ref", "--format=%(refname)", "refs/shade"]
        )
        .is_empty()
    );
}

#[tokio::test]
async fn committed_workspace_delta_survives_successor_integration_and_repeated_publish() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    commit_fixture(&source);
    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed = store
        .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
        .await
        .unwrap();
    let original_base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();

    let original = temporary.path().join("original-committed");
    store
        .prepare_base(&managed, &original_base, &original)
        .await
        .unwrap();
    store
        .register_precloned_worktree(&managed, &original, &original_base, "committed")
        .await
        .unwrap();
    fs::write(original.join("local-commit.txt"), "local commit\n").unwrap();
    git(&original, &["add", "local-commit.txt"]);
    git(
        &original,
        &[
            "-c",
            "user.name=Shade Test",
            "-c",
            "user.email=shade-test@example.invalid",
            "commit",
            "-m",
            "detached local commit",
        ],
    );
    assert!(store.status(&original).await.unwrap().is_clean());
    let checkpoint = store
        .checkpoint(
            &managed,
            &original,
            "workspace_committed",
            "checkpoint_committed",
        )
        .await
        .unwrap();
    assert_ne!(checkpoint.head, original_base.commit);

    fs::write(source.join("remote-advance.txt"), "remote advance\n").unwrap();
    git(&source, &["add", "remote-advance.txt"]);
    git(&source, &["commit", "-m", "remote advance"]);
    let new_base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    assert_ne!(new_base.commit, original_base.commit);

    let successor = temporary.path().join("successor-committed");
    store
        .prepare_base(&managed, &new_base, &successor)
        .await
        .unwrap();
    store
        .register_precloned_worktree(&managed, &successor, &new_base, "successor")
        .await
        .unwrap();
    let integration = store
        .integrate_checkpoint(
            &managed,
            &successor,
            &original_base.commit,
            &new_base,
            &checkpoint,
        )
        .await
        .unwrap();
    assert!(integration.clean);
    assert_eq!(
        fs::read_to_string(successor.join("local-commit.txt")).unwrap(),
        "local commit\n"
    );
    assert_eq!(
        fs::read_to_string(successor.join("remote-advance.txt")).unwrap(),
        "remote advance\n"
    );
    let status = store.status(&successor).await.unwrap();
    assert_eq!((status.staged, status.unstaged), (1, 0));

    let first = store
        .squash_publish(
            &managed,
            SquashPublishRequest {
                remote: &remote,
                branch: "main",
                original_base: &original_base.commit,
                expected_remote: Some(&new_base.commit),
                expected_local: None,
                checkpoint: &checkpoint,
                message: "preserve detached commit\n",
                push: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed.git_dir().display()),
                "show",
                &format!("{}:local-commit.txt", first.commit),
            ],
        ),
        "local commit"
    );
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed.git_dir().display()),
                "show",
                &format!("{}:remote-advance.txt", first.commit),
            ],
        ),
        "remote advance"
    );
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed.git_dir().display()),
                "rev-parse",
                &format!("{}^", first.commit),
            ],
        ),
        new_base.commit.as_str()
    );

    let second = store
        .squash_publish(
            &managed,
            SquashPublishRequest {
                remote: &remote,
                branch: "main",
                original_base: &original_base.commit,
                expected_remote: Some(&new_base.commit),
                expected_local: Some(&first.commit),
                checkpoint: &checkpoint,
                message: "repeat local publish\n",
                push: false,
            },
        )
        .await
        .unwrap();
    assert_ne!(second.commit, first.commit);
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed.git_dir().display()),
                "rev-parse",
                "refs/heads/main",
            ],
        ),
        second.commit.as_str()
    );
}

#[tokio::test]
async fn concurrent_local_publishers_cannot_overwrite_the_same_observed_tip() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    commit_fixture(&source);
    let system_store = GitStore::system();
    let remote = system_store
        .canonicalize_remote(source.to_str().unwrap())
        .unwrap();
    let managed = system_store
        .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
        .await
        .unwrap();
    let base = system_store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let workspace = temporary.path().join("workspace-race");
    system_store
        .prepare_base(&managed, &base, &workspace)
        .await
        .unwrap();
    system_store
        .register_precloned_worktree(&managed, &workspace, &base, "race")
        .await
        .unwrap();
    fs::write(workspace.join("race.txt"), "candidate\n").unwrap();
    let checkpoint = system_store
        .checkpoint(&managed, &workspace, "workspace_race", "checkpoint_race")
        .await
        .unwrap();

    let wrapper = temporary.path().join("barrier-git");
    commit_tree_barrier_wrapper(&wrapper, &temporary.path().join("commit-barrier"));
    let racing_store = GitStore::with_binary(wrapper);
    let first = racing_store.squash_publish(
        &managed,
        SquashPublishRequest {
            remote: &remote,
            branch: "race",
            original_base: &base.commit,
            expected_remote: None,
            expected_local: None,
            checkpoint: &checkpoint,
            message: "racer one\n",
            push: false,
        },
    );
    let second = racing_store.squash_publish(
        &managed,
        SquashPublishRequest {
            remote: &remote,
            branch: "race",
            original_base: &base.commit,
            expected_remote: None,
            expected_local: None,
            checkpoint: &checkpoint,
            message: "racer two\n",
            push: false,
        },
    );
    let (first, second) = tokio::join!(first, second);
    assert_ne!(first.is_ok(), second.is_ok(), "exactly one CAS must win");
    let winner = first.ok().or_else(|| second.ok()).unwrap();
    assert_eq!(
        git(
            temporary.path(),
            &[
                &format!("--git-dir={}", managed.git_dir().display()),
                "rev-parse",
                "refs/heads/race",
            ],
        ),
        winner.commit.as_str()
    );
    assert_eq!(
        system_store
            .remote_branch_oid(&remote, "race")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn prepared_publish_is_private_and_every_cas_phase_is_idempotent() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    init_repository(&source, None);
    commit_fixture(&source);
    let store = GitStore::system();
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed = store
        .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
        .await
        .unwrap();
    let base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".to_owned()),
        )
        .await
        .unwrap();
    let workspace = temporary.path().join("workspace-phased-publish");
    store
        .prepare_base(&managed, &base, &workspace)
        .await
        .unwrap();
    store
        .register_precloned_worktree(&managed, &workspace, &base, "phased")
        .await
        .unwrap();
    fs::write(workspace.join("phased.txt"), "candidate\n").unwrap();
    let checkpoint = store
        .checkpoint(
            &managed,
            &workspace,
            "workspace_phased",
            "checkpoint_phased",
        )
        .await
        .unwrap();
    let anchor = "refs/shade/operations/op_phased/publish";
    let request = PrepareSquashPublishRequest {
        remote: &remote,
        branch: "phased",
        original_base: &base.commit,
        expected_remote: None,
        checkpoint: &checkpoint,
        message: "phased publish\n",
        anchor_ref: anchor,
    };
    let prepared = store
        .prepare_squash_publish(&managed, request)
        .await
        .unwrap();
    let replayed = store
        .prepare_squash_publish(&managed, request)
        .await
        .unwrap();
    assert_eq!(prepared, replayed);
    assert_eq!(
        store.publish_anchor_oid(&managed, anchor).await.unwrap(),
        Some(prepared.commit.clone())
    );
    assert_eq!(
        store.local_branch_oid(&managed, "phased").await.unwrap(),
        None
    );
    assert_eq!(
        store.remote_branch_oid(&remote, "phased").await.unwrap(),
        None
    );

    store
        .apply_prepared_publish_local(&managed, &prepared, None)
        .await
        .unwrap();
    store
        .apply_prepared_publish_local(&managed, &prepared, None)
        .await
        .unwrap();
    store
        .apply_prepared_publish_remote(&managed, &remote, "phased", &prepared, None)
        .await
        .unwrap();
    store
        .apply_prepared_publish_remote(&managed, &remote, "phased", &prepared, None)
        .await
        .unwrap();
    assert_eq!(
        store.remote_branch_oid(&remote, "phased").await.unwrap(),
        Some(prepared.commit.clone())
    );
    assert!(
        store
            .delete_publish_anchor(&managed, anchor, &prepared.commit)
            .await
            .unwrap()
    );
    assert!(
        !store
            .delete_publish_anchor(&managed, anchor, &prepared.commit)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn an_scp_style_origin_survives_being_read_back_from_its_recorded_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("scp-origin");
    init_repository(&source, None);
    commit_fixture(&source);
    git(
        &source,
        &["remote", "add", "origin", "git@GitHub.com:Owner/Repo.git"],
    );
    let store = GitStore::system();
    let identity = store.canonicalize_local(&source).await.unwrap();
    assert_eq!(
        identity.remote.canonical,
        "ssh+scp://git@github.com/Owner/Repo"
    );
    assert_eq!(identity.remote.fetch_url, "git@GitHub.com:Owner/Repo.git");

    // Every operation after the open re-canonicalizes the identity the
    // database recorded, so an identity this cannot read back is a repository
    // no checkpoint, sleep, fork or publish can reach.
    let recorded = store
        .canonicalize_remote(&identity.remote.canonical)
        .unwrap();
    assert_eq!(recorded.canonical, identity.remote.canonical);
    assert_eq!(recorded.fetch_url, "git@github.com:Owner/Repo");
    assert_eq!(
        store
            .canonicalize_remote(&recorded.canonical)
            .unwrap()
            .canonical,
        recorded.canonical
    );

    // The scheme names an endpoint, not a path: it never becomes one.
    for hostile in [
        "ssh+scp://git@github.com",
        "ssh+scp://git@github.com/",
        "ssh+scp:///Owner/Repo",
    ] {
        assert!(
            store.canonicalize_remote(hostile).is_err(),
            "accepted {hostile}"
        );
    }
}
