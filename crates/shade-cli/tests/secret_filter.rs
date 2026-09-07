//! Exercise the installed Git process filter through the actual CLI binary.
use shade_engine::git::{BaseSpec, GitStore};
use shade_engine::secrets::SecretStore;
use shade_protocol::WorkspaceId;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn git(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap()
}

fn ok(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "Git command failed: {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[tokio::test]
async fn real_filter_withholds_secrets_and_preserves_safe_binary_content() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    fs::create_dir(&source).unwrap();
    ok(&source, &["init", "-b", "main"]);
    ok(&source, &["config", "user.name", "Shade Test"]);
    ok(&source, &["config", "user.email", "shade@example.invalid"]);
    fs::write(source.join("tracked.json"), "{}\n").unwrap();
    ok(&source, &["add", "."]);
    ok(&source, &["commit", "-m", "base"]);
    // Spaces and apostrophes exercise the command Git runs through its shell.
    let spool = temporary.path().join("private spool's directory");
    let store =
        GitStore::system().with_content_filter(env!("CARGO_BIN_EXE_shade").into(), spool.clone());
    let remote = store.canonicalize_remote(source.to_str().unwrap()).unwrap();
    let managed = store
        .create_managed_bare(&temporary.path().join("managed.git"), Some(&remote))
        .await
        .unwrap();
    let base = store
        .resolve_base(
            &managed,
            Some(&remote),
            BaseSpec::OriginBranch("main".into()),
        )
        .await
        .unwrap();
    let workspace = temporary.path().join("workspace");
    store
        .prepare_base(&managed, &base, &workspace)
        .await
        .unwrap();
    store
        .register_precloned_worktree(&managed, &workspace, &base, "filter-test")
        .await
        .unwrap();

    let safe_binary = (0u8..=255)
        .cycle()
        .take(2 * 1024 * 1024)
        .collect::<Vec<_>>();
    fs::write(workspace.join("safe.bin"), &safe_binary).unwrap();
    fs::write(workspace.join("safe.txt"), "unchanged by the filter\n").unwrap();
    // Two requests share one persistent filter process; the large file spills.
    ok(&workspace, &["add", "--", "safe.bin", "safe.txt"]);
    assert_eq!(ok(&workspace, &["show", ":safe.bin"]), safe_binary);
    assert_eq!(
        ok(&workspace, &["show", ":safe.txt"]),
        b"unchanged by the filter\n"
    );
    assert_eq!(
        fs::read_dir(&spool).unwrap().count(),
        0,
        "anonymous spool must leave no files"
    );

    let credential = r#"{"api_key":"r3D8m2A9c6Z1q7B4v5L0n8H2"}"#;
    let mut large_secret = vec![0xff; 2 * 1024 * 1024 - 11];
    large_secret.extend_from_slice(["-----BEGIN ", "PRIVATE KEY-----"].concat().as_bytes());
    for (path, bytes) in [
        ("tracked.json", credential.as_bytes()),
        ("credentials.json", credential.as_bytes()),
        ("large-private.bin", large_secret.as_slice()),
        (".env.local", b"TOKEN=short".as_slice()),
    ] {
        fs::write(workspace.join(path), bytes).unwrap();
        let oid = String::from_utf8(ok(&workspace, &["hash-object", "--no-filters", "--", path]))
            .unwrap();
        let oid = oid.trim();
        assert!(!git(&workspace, &["cat-file", "-e", oid]).status.success());
        let rejected = git(&workspace, &["add", "-f", "--", path]);
        assert!(!rejected.status.success(), "filter must reject {path}");
        assert!(
            !git(&workspace, &["cat-file", "-e", oid]).status.success(),
            "rejected content entered the object database"
        );
        assert!(!String::from_utf8_lossy(&rejected.stderr).contains(credential));
    }
    assert_eq!(fs::read_dir(&spool).unwrap().count(), 0);

    // Template names are exempt from the refusal by name, and only from that:
    // a placeholder file stages like any other content, while one carrying a
    // real credential is still withheld by the same scanner.
    fs::write(
        workspace.join(".env.example"),
        "API_URL=http://localhost:3000\nAPI_KEY=your-api-key-here\n",
    )
    .unwrap();
    ok(&workspace, &["add", "-f", "--", ".env.example"]);
    fs::write(workspace.join(".env.sample"), credential.as_bytes()).unwrap();
    let rejected = git(&workspace, &["add", "-f", "--", ".env.sample"]);
    assert!(
        !rejected.status.success(),
        "a template holding a real credential must still be withheld"
    );
    assert!(!String::from_utf8_lossy(&rejected.stderr).contains(credential));
    fs::remove_file(workspace.join(".env.sample")).unwrap();
    assert_eq!(fs::read_dir(&spool).unwrap().count(), 0);

    store.status(&workspace).await.unwrap();
    store.context_state(&workspace).await.unwrap();
    let checkpoint = store
        .checkpoint(&managed, &workspace, "ws_filter", "cp_filter")
        .await
        .unwrap();
    let child = temporary.path().join("child");
    store.prepare_base(&managed, &base, &child).await.unwrap();
    store
        .register_precloned_worktree(&managed, &child, &base, "filter-child")
        .await
        .unwrap();
    store
        .restore_checkpoint(&child, &checkpoint, true)
        .await
        .unwrap();
    assert_eq!(fs::read(child.join("tracked.json")).unwrap(), b"{}\n");
    assert!(!child.join("credentials.json").exists());
    let private = SecretStore::new(temporary.path().join("private")).unwrap();
    private
        .capture(&WorkspaceId("ws_filter".into()), &workspace)
        .unwrap();
    private.copy_workspace_secrets(&workspace, &child).unwrap();
    assert_eq!(
        fs::read(child.join("tracked.json")).unwrap(),
        credential.as_bytes()
    );
    assert_eq!(
        fs::read(child.join("large-private.bin")).unwrap(),
        large_secret
    );
    // A same-UID bypass can still put a credential in the index: the boundary
    // no longer re-reads that content. A tracked path the owner staged is the
    // owner's decision, and the clean filter above is where Shade refuses to
    // write a new secret. What the boundary still refuses is the private name.
    ok(
        &workspace,
        &[
            "-c",
            "filter.shade-content.required=false",
            "-c",
            "filter.shade-content.process=",
            "add",
            "tracked.json",
        ],
    );
    let bypassed = store
        .checkpoint(&managed, &workspace, "ws_filter", "cp_bypass")
        .await
        .expect("committed content is not a retention rejection");
    assert_eq!(
        ok(
            &workspace,
            &["show", &format!("{}:tracked.json", bypassed.working_tree),],
        ),
        credential.as_bytes(),
    );

    fs::write(workspace.join(".env.local"), "TOKEN=must-not-be-retained\n").unwrap();
    ok(
        &workspace,
        &[
            "-c",
            "filter.shade-secret.clean=cat",
            "add",
            "-f",
            "--",
            ".env.local",
        ],
    );
    let error = store
        .checkpoint(&managed, &workspace, "ws_filter", "cp_dotenv")
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "TRACKED_SECRET_FILE: .env.local");
    assert!(!error.contains("must-not-be-retained"));
    assert!(
        !git(
            &workspace,
            &[
                "rev-parse",
                "--verify",
                "refs/shade/workspaces/ws_filter/checkpoints/cp_dotenv/head"
            ]
        )
        .status
        .success()
    );
}
