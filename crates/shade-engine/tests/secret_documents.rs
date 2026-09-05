use shade_engine::secrets::{MergeChoice, SecretError, SecretStore};
use shade_protocol::{ReviewId, WorkspaceId};
use std::fs;
use std::os::unix::fs::PermissionsExt;

const KEY: &str = "r3D8m2A9c6Z1q7B4v5L0n8H2";

#[test]
fn json_private_files_merge_keys_without_colliding_with_baseline_metadata() {
    let root = tempfile::tempdir().unwrap();
    let store = SecretStore::new(root.path().join("private")).unwrap();
    let parent = root.path().join("parent");
    let child = root.path().join("child");
    let successor = root.path().join("successor");
    for directory in [&parent, &child, &successor] {
        fs::create_dir(directory).unwrap();
    }
    let baseline = WorkspaceId("ws_json".into());
    let original = serde_json::json!({"auth":{"api_key": KEY},"nested":{"left":0,"right":0},"a/b":{"~key":true}});
    fs::write(parent.join("manifest.json"), original.to_string()).unwrap();
    let captured = store.capture(&baseline, &parent).unwrap();
    assert_eq!(captured.files, ["manifest.json"]);
    store
        .clone_baseline(&baseline, &WorkspaceId("ws_clone".into()))
        .unwrap();
    let private_file = root.path().join("private/ws_json/files/manifest.json");
    assert_eq!(
        fs::metadata(&private_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut left = original.clone();
    left["nested"]["left"] = 1.into();
    let mut right = original.clone();
    right["nested"]["right"] = 2.into();
    fs::write(parent.join("manifest.json"), left.to_string()).unwrap();
    fs::write(child.join("manifest.json"), right.to_string()).unwrap();
    let preview = store.preview(&baseline, &parent, &child).unwrap();
    let encoded = serde_json::to_string(&preview).unwrap();
    assert!(!encoded.contains(KEY));
    assert!(
        preview[0]
            .keys
            .iter()
            .any(|key| key.key == "/nested/left" && key.result == "parent")
    );
    assert!(
        preview[0]
            .keys
            .iter()
            .any(|key| key.key == "/nested/right" && key.result == "child")
    );
    assert!(preview[0].keys.iter().any(|key| key.key == "/a~1b/~0key"));
    store
        .apply_reviewed(&baseline, &parent, &child, &successor, MergeChoice::Merge)
        .unwrap();
    let merged: serde_json::Value =
        serde_json::from_slice(&fs::read(successor.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(merged["nested"], serde_json::json!({"left":1,"right":2}));
    assert_eq!(merged["auth"], original["auth"]);
    // Deleting an object while the other side edits it is a conflict.
    left.as_object_mut().unwrap().remove("nested");
    fs::write(parent.join("manifest.json"), left.to_string()).unwrap();
    assert!(matches!(
        store.apply_reviewed(&baseline, &parent, &child, &successor, MergeChoice::Merge),
        Err(SecretError::Conflict)
    ));
    let ambiguous =
        format!(r#"{{"auth":{{"api_key":"{KEY}"}},"nested":{{"left":1,"left":3,"right":0}}}}"#);
    fs::write(parent.join("manifest.json"), &ambiguous).unwrap();
    assert!(!store.can_merge(&baseline, &parent, &child).unwrap());
    assert!(matches!(
        store.apply_reviewed(&baseline, &parent, &child, &successor, MergeChoice::Merge),
        Err(SecretError::Conflict)
    ));
    assert_eq!(
        fs::read_to_string(parent.join("manifest.json")).unwrap(),
        ambiguous
    );
}

#[test]
fn toml_merge_preserves_types_and_binary_decisions_preserve_exact_bytes() {
    let root = tempfile::tempdir().unwrap();
    let store = SecretStore::new(root.path().join("private")).unwrap();
    let parent = root.path().join("parent");
    let child = root.path().join("child");
    let successor = root.path().join("successor");
    for directory in [&parent, &child, &successor] {
        fs::create_dir(directory).unwrap();
    }
    let baseline = WorkspaceId("ws_formats".into());
    let toml = format!(
        "api_key = '{KEY}'\ncreated = 2026-09-04T10:30:00Z\n[left]\ncount = 0\n[right]\ncount = 0\n"
    );
    let mut binary = vec![0xff, 0x00, 0x80];
    binary.extend_from_slice(["-----BEGIN ", "PRIVATE KEY-----"].concat().as_bytes());
    fs::write(parent.join("credentials.toml"), &toml).unwrap();
    fs::write(parent.join("private.bin"), &binary).unwrap();
    store.capture(&baseline, &parent).unwrap();
    store.copy_workspace_secrets(&parent, &child).unwrap();
    fs::write(
        parent.join("credentials.toml"),
        toml.replace("[left]\ncount = 0", "[left]\ncount = 1"),
    )
    .unwrap();
    fs::write(
        child.join("credentials.toml"),
        toml.replace("[right]\ncount = 0", "[right]\ncount = 2"),
    )
    .unwrap();
    binary.push(1);
    fs::write(child.join("private.bin"), &binary).unwrap();
    let preview = store.preview(&baseline, &parent, &child).unwrap();
    let binary_preview = preview
        .iter()
        .find(|file| file.path == "private.bin")
        .unwrap();
    assert!(binary_preview.keys.is_empty());
    assert_eq!(binary_preview.file_result, "child");
    store
        .apply_reviewed(&baseline, &parent, &child, &successor, MergeChoice::Merge)
        .unwrap();
    let merged: toml::Value =
        toml::from_str(&fs::read_to_string(successor.join("credentials.toml")).unwrap()).unwrap();
    assert!(merged["created"].is_datetime());
    assert_eq!(merged["left"]["count"].as_integer(), Some(1));
    assert_eq!(merged["right"]["count"].as_integer(), Some(2));
    assert_eq!(fs::read(successor.join("private.bin")).unwrap(), binary);
    let mut other = binary.clone();
    other.push(2);
    fs::write(parent.join("private.bin"), &other).unwrap();
    assert!(matches!(
        store.apply_reviewed(&baseline, &parent, &child, &successor, MergeChoice::Merge),
        Err(SecretError::Conflict)
    ));
    store
        .apply_reviewed(
            &baseline,
            &parent,
            &child,
            &successor,
            MergeChoice::KeepChild,
        )
        .unwrap();
    assert_eq!(fs::read(successor.join("private.bin")).unwrap(), binary);
    store
        .apply_reviewed(
            &baseline,
            &parent,
            &child,
            &successor,
            MergeChoice::DiscardChild,
        )
        .unwrap();
    assert_eq!(fs::read(successor.join("private.bin")).unwrap(), other);
    fs::write(parent.join("private.bin"), &binary[..binary.len() - 1]).unwrap();
    fs::write(child.join("private.bin"), []).unwrap();
    store
        .apply_reviewed(&baseline, &parent, &child, &successor, MergeChoice::Merge)
        .unwrap();
    assert_eq!(
        fs::read(successor.join("private.bin")).unwrap(),
        b"",
        "an empty file is different from a deletion"
    );
}

#[test]
fn review_tracks_known_private_files_after_credentials_are_removed() {
    let root = tempfile::tempdir().unwrap();
    let store = SecretStore::new(root.path().join("private")).unwrap();
    let child = root.path().join("child");
    fs::create_dir(&child).unwrap();
    fs::write(
        child.join("config.json"),
        format!(r#"{{"api_key":"{KEY}"}}"#),
    )
    .unwrap();
    let baseline = WorkspaceId("ws_fresh".into());
    store.capture(&baseline, &child).unwrap();
    fs::write(child.join("config.json"), "{}").unwrap();
    let review = ReviewId("rev_fresh".into());
    store
        .capture_review(&baseline, &review, &child, None)
        .unwrap();
    assert!(
        store
            .review_is_current(&baseline, &review, &child, None)
            .unwrap()
    );
    fs::write(child.join("config.json"), r#"{"enabled":true}"#).unwrap();
    assert!(
        !store
            .review_is_current(&baseline, &review, &child, None)
            .unwrap()
    );
}

#[test]
fn cloned_baseline_keeps_nested_dotenv_bytes_private() {
    let root = tempfile::tempdir().unwrap();
    let store = SecretStore::new(root.path().join("private")).unwrap();
    let parent = root.path().join("parent");
    fs::create_dir_all(parent.join("nested")).unwrap();
    fs::write(parent.join("nested/.env"), "TOKEN=original\n").unwrap();
    let parent_id = WorkspaceId("ws_parent".into());
    store.capture(&parent_id, &parent).unwrap();
    let child_id = WorkspaceId("ws_new".into());
    store.clone_baseline(&parent_id, &child_id).unwrap();
    let child = root.path().join("child");
    fs::create_dir(&child).unwrap();
    store.copy_workspace_secrets(&parent, &child).unwrap();
    assert_eq!(
        store.preview_current(&child_id, &child).unwrap()[0].file_result,
        "unchanged"
    );
    assert_eq!(
        fs::metadata(root.path().join("private/ws_new/files/nested/.env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
