//! The parked tier: private build output kept on an external volume.
//!
//! `sleep` checkpoints a workspace and deletes its tree, and everything Git
//! ignores -- `target/`, `dist/`, `.next/` -- goes with it. Those bytes are
//! expensive to rebuild and worthless to keep on the boot disk, so this module
//! copies them to a park root on another volume and copies them back on
//! `wake`.
//!
//! Every copy here is a real byte copy. The park root is a different volume by
//! definition, which is the whole point of the tier and also why APFS cloning
//! cannot apply; there is deliberately no clone path to fall back to.
//!
//! Nothing private ever lands in the park. The set is filtered twice -- once
//! when it is inventoried and again for every file as it is copied -- against
//! `.git`, the dependency layer link targets the caller names, and every path
//! the secret policy claims.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::engine::EngineError;

/// Wire code for every failure of the parked tier. The park volume is
/// external and removable, so a failure here is a condition of the moment.
pub const PARK_FAILED: &str = "PARK_FAILED";
/// Bumped whenever the on-disk layout changes. A manifest from another
/// version is not read; the park it describes is simply not restored.
pub const MANIFEST_VERSION: u32 = 1;

const MANIFEST_NAME: &str = "manifest.json";
/// Parked bytes live under their own subdirectory so no parked path can ever
/// collide with the manifest that describes it.
const TREE_NAME: &str = "tree";
const STAGING_SUFFIX: &str = ".tmp";
const SUPERSEDED_SUFFIX: &str = ".stale";

/// One parked file, by its path relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkEntry {
    /// Relative, `/`-separated, and never escaping the workspace root.
    pub path: String,
    pub bytes: u64,
}

/// What a park directory claims to hold. Written last, so its presence is an
/// honest answer to "is there a complete park here".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkManifest {
    pub version: u32,
    pub workspace_id: String,
    pub checkpoint_id: String,
    pub head_oid: String,
    pub worktree_oid: String,
    pub created_at_ms: i64,
    pub bytes: u64,
    pub entries: Vec<ParkEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkOutcome {
    pub park_path: PathBuf,
    pub bytes: u64,
    pub entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    pub restored: bool,
    pub bytes: u64,
    /// Why nothing was restored: `absent` when there is no park to read,
    /// `manifest_mismatch` when the one on disk describes something else.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkSummary {
    pub workspace_id: String,
    pub checkpoint_id: String,
    pub bytes: u64,
    pub manifest_ok: bool,
}

/// `<park_root>/<workspace_id>/<checkpoint_id>`.
pub fn park_dir(park_root: &Path, workspace_id: &str, checkpoint_id: &str) -> PathBuf {
    park_root.join(workspace_id).join(checkpoint_id)
}

/// The link targets a dependency layer materializes into a workspace.
///
/// These are the paths `dependencies/` hands back as a receipt's materialized
/// paths, and wake rebuilds every one of them from the shared cache. Parking
/// them would copy a dependency forest across a slow volume to restore
/// something the layer already owns.
pub fn default_excluded_relative_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("node_modules"),
        PathBuf::from(".venv"),
        PathBuf::from(".shade"),
    ]
}

/// Every gitignored path in the workspace that may be parked, relative to the
/// workspace root and in Git's order.
///
/// Entries are what `git ls-files --directory` reports: a wholly ignored
/// directory collapses to one entry, so this list is short even when the
/// build output is not. Directories still need per-file filtering on the way
/// out, which [`park_tree`] applies.
pub fn collect_park_set(
    workspace_path: &Path,
    excluded: &[PathBuf],
) -> Result<Vec<PathBuf>, EngineError> {
    let filter = ParkFilter::new(workspace_path, excluded)?;
    collect(workspace_path, &filter)
}

/// Copy the workspace's private build output to the park volume.
///
/// The copy lands in a staging sibling and is published with one rename, so a
/// park directory is either absent or complete. An existing park for the same
/// checkpoint is replaced rather than merged: it describes an older tree.
pub fn park_tree(
    park_root: &Path,
    workspace_path: &Path,
    workspace_id: &str,
    checkpoint_id: &str,
    head_oid: &str,
    worktree_oid: &str,
    excluded: &[PathBuf],
) -> Result<ParkOutcome, EngineError> {
    ensure_segment("workspace id", workspace_id)?;
    ensure_segment("checkpoint id", checkpoint_id)?;
    let filter = ParkFilter::new(workspace_path, excluded)?;
    let set = collect(workspace_path, &filter)?;

    let destination = park_dir(park_root, workspace_id, checkpoint_id);
    let parent = destination
        .parent()
        .ok_or_else(|| EngineError::park("park destination has no parent"))?;
    create_dir_all(parent)?;
    let staging = sibling(&destination, STAGING_SUFFIX)?;
    remove_if_present(&staging)?;
    let staged_tree = staging.join(TREE_NAME);
    create_dir_all(&staged_tree)?;

    let mut entries = Vec::new();
    for relative in &set {
        copy_recursive(
            &workspace_path.join(relative),
            &staged_tree.join(relative),
            relative,
            &filter,
            &mut entries,
        )?;
    }
    let bytes = entries.iter().map(|entry| entry.bytes).sum();

    let manifest = ParkManifest {
        version: MANIFEST_VERSION,
        workspace_id: workspace_id.to_owned(),
        checkpoint_id: checkpoint_id.to_owned(),
        head_oid: head_oid.to_owned(),
        worktree_oid: worktree_oid.to_owned(),
        created_at_ms: crate::db::now_ms(),
        bytes,
        entries,
    };
    let serialized = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        EngineError::park(format!("cannot serialize the park manifest: {error}"))
    })?;
    write_file(&staging.join(MANIFEST_NAME), &serialized)?;

    publish(&staging, &destination)?;
    Ok(ParkOutcome {
        park_path: destination,
        bytes: manifest.bytes,
        entries: manifest.entries.len(),
    })
}

/// Copy a park back into a workspace, if one is there and it describes this
/// exact checkpoint.
///
/// A missing park and a stale one are both ordinary outcomes, not errors: the
/// volume may be unplugged, and a successor built from a different checkpoint
/// simply rebuilds its own output.
pub fn restore_park(
    park_root: &Path,
    workspace_path: &Path,
    workspace_id: &str,
    checkpoint_id: &str,
    expected_head_oid: &str,
    expected_worktree_oid: &str,
) -> Result<RestoreOutcome, EngineError> {
    ensure_segment("workspace id", workspace_id)?;
    ensure_segment("checkpoint id", checkpoint_id)?;
    let source = park_dir(park_root, workspace_id, checkpoint_id);
    let Some(manifest) = read_manifest(&source)? else {
        return Ok(declined("absent"));
    };
    if manifest.version != MANIFEST_VERSION
        || manifest.workspace_id != workspace_id
        || manifest.checkpoint_id != checkpoint_id
        || manifest.head_oid != expected_head_oid
        || manifest.worktree_oid != expected_worktree_oid
    {
        return Ok(declined("manifest_mismatch"));
    }

    let tracked = tracked_paths(workspace_path)?;
    let tree = source.join(TREE_NAME);
    let mut bytes = 0;
    for entry in &manifest.entries {
        let relative = relative_path(&entry.path)?;
        // The park set was gitignored when it was taken, so a tracked
        // collision means the base moved under it. The base wins: it is the
        // only copy Git can reproduce.
        if tracked.contains(&relative) || crate::secret_policy::private_env_path(&relative) {
            continue;
        }
        let from = tree.join(&relative);
        if !from.is_file() {
            continue;
        }
        let to = workspace_path.join(&relative);
        if let Some(parent) = to.parent() {
            create_dir_all(parent)?;
        }
        bytes += copy_file(&from, &to)?;
    }
    Ok(RestoreOutcome {
        restored: true,
        bytes,
        reason: None,
    })
}

/// Drop one park, along with any staging or superseded sibling it left
/// behind. A park that is already gone is not a failure.
pub fn remove_park(
    park_root: &Path,
    workspace_id: &str,
    checkpoint_id: &str,
) -> Result<(), EngineError> {
    ensure_segment("workspace id", workspace_id)?;
    ensure_segment("checkpoint id", checkpoint_id)?;
    let destination = park_dir(park_root, workspace_id, checkpoint_id);
    remove_if_present(&sibling(&destination, STAGING_SUFFIX)?)?;
    remove_if_present(&sibling(&destination, SUPERSEDED_SUFFIX)?)?;
    remove_if_present(&destination)?;
    if let Some(parent) = destination.parent() {
        // Best effort: the workspace directory is shared with other
        // checkpoints and only goes when the last one does.
        let _ = fs::remove_dir(parent);
    }
    Ok(())
}

/// Every park under a root, whether or not its manifest still reads.
///
/// A broken manifest is reported rather than raised: this is the inventory an
/// operator uses to find and remove exactly that.
pub fn list_parks(park_root: &Path) -> Result<Vec<ParkSummary>, EngineError> {
    let mut summaries = Vec::new();
    for workspace in read_dir(park_root)? {
        let Some(workspace_id) = directory_name(&workspace) else {
            continue;
        };
        for checkpoint in read_dir(&workspace)? {
            let Some(checkpoint_id) = directory_name(&checkpoint) else {
                continue;
            };
            if checkpoint_id.ends_with(STAGING_SUFFIX) || checkpoint_id.ends_with(SUPERSEDED_SUFFIX)
            {
                continue;
            }
            let manifest = read_manifest(&checkpoint).ok().flatten();
            summaries.push(ParkSummary {
                workspace_id: workspace_id.clone(),
                checkpoint_id,
                bytes: manifest.as_ref().map_or(0, |manifest| manifest.bytes),
                manifest_ok: manifest.is_some_and(|manifest| manifest.version == MANIFEST_VERSION),
            });
        }
    }
    summaries.sort_by(|left, right| {
        (&left.workspace_id, &left.checkpoint_id).cmp(&(&right.workspace_id, &right.checkpoint_id))
    });
    Ok(summaries)
}

/// What may not be parked, in the one predicate both the inventory and the
/// copy consult. Built once per park so the secret scan is paid once.
struct ParkFilter {
    excluded: Vec<PathBuf>,
    secrets: BTreeSet<PathBuf>,
}

impl ParkFilter {
    fn new(workspace_path: &Path, excluded: &[PathBuf]) -> Result<Self, EngineError> {
        let secrets = crate::secrets::discover_secret_paths(workspace_path)
            .map_err(|error| {
                EngineError::park(format!("cannot enumerate workspace secrets: {error}"))
            })?
            .into_iter()
            .map(PathBuf::from)
            .collect();
        Ok(Self {
            excluded: excluded.to_vec(),
            secrets,
        })
    }

    /// Is this workspace-relative path kept out of the park?
    fn skips(&self, relative: &Path) -> bool {
        if relative.components().any(
            |component| matches!(component, Component::Normal(name) if name == OsStr::new(".git")),
        ) {
            return true;
        }
        if crate::secret_policy::private_env_path(relative) {
            return true;
        }
        if self
            .secrets
            .iter()
            .any(|secret| relative.starts_with(secret))
        {
            return true;
        }
        self.excluded.iter().any(|entry| {
            let mut components = entry.components();
            match (components.next(), components.next()) {
                // A bare name excludes that directory wherever it is nested,
                // which is how one entry covers `packages/*/node_modules`.
                (Some(only), None) => relative.components().any(|component| component == only),
                (Some(_), Some(_)) => relative.starts_with(entry),
                _ => false,
            }
        })
    }
}

fn collect(workspace_path: &Path, filter: &ParkFilter) -> Result<Vec<PathBuf>, EngineError> {
    let stdout = git(
        workspace_path,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )?;
    let mut set = Vec::new();
    for record in stdout.split(|byte| *byte == 0) {
        // `--directory` reports a wholly ignored directory with a trailing
        // separator; the rest of the module works in plain relative paths.
        let record = record.strip_suffix(b"/").unwrap_or(record);
        if record.is_empty() {
            continue;
        }
        let relative = crate::git::bytes_to_path(record);
        if !is_safe_relative(&relative) || filter.skips(&relative) {
            continue;
        }
        set.push(relative);
    }
    Ok(set)
}

/// The paths Git tracks in a workspace, or nothing when there is no
/// repository there yet.
fn tracked_paths(workspace_path: &Path) -> Result<BTreeSet<PathBuf>, EngineError> {
    if git(workspace_path, &["rev-parse", "--git-dir"]).is_err() {
        return Ok(BTreeSet::new());
    }
    let stdout = git(workspace_path, &["ls-files", "-z"])?;
    Ok(stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(crate::git::bytes_to_path)
        .collect())
}

/// Run the system Git, scrubbed exactly the way [`crate::git::GitStore`]
/// scrubs it. The park runs on the blocking side of the engine, so it uses a
/// synchronous child rather than the async store.
fn git(workspace_path: &Path, args: &[&str]) -> Result<Vec<u8>, EngineError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(workspace_path).args(args);
    for variable in crate::git::INHERITED_GIT_ENVIRONMENT {
        command.env_remove(variable);
    }
    for (variable, value) in crate::git::PINNED_GIT_ENVIRONMENT {
        command.env(variable, value);
    }
    command.stdin(Stdio::null());
    let output = command
        .output()
        .map_err(|error| EngineError::park(format!("failed to start system Git: {error}")))?;
    if !output.status.success() {
        return Err(EngineError::park(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            workspace_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// Copy a file or a whole directory, recording every regular file it copies.
///
/// Symlinks and other non-regular entries are skipped: a park is restored into
/// a workspace built from a different base, where a link's target may not be
/// the same file or may not exist at all.
fn copy_recursive(
    source: &Path,
    destination: &Path,
    relative: &Path,
    filter: &ParkFilter,
    copied: &mut Vec<ParkEntry>,
) -> Result<(), EngineError> {
    if filter.skips(relative) {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        // The workspace is quiescent but not frozen; a build directory that
        // vanishes mid-copy is not worth failing a sleep over.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(EngineError::park(format!(
                "cannot stat {}: {error}",
                source.display()
            )));
        }
    };
    if metadata.is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        create_dir_all(destination)?;
        for child in read_dir(source)? {
            let Some(name) = child.file_name() else {
                continue;
            };
            copy_recursive(
                &child,
                &destination.join(name),
                &relative.join(name),
                filter,
                copied,
            )?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Ok(());
    }
    let Some(path) = unix_relative(relative) else {
        // A manifest is JSON, so a path that is not UTF-8 cannot be recorded
        // honestly. Recording it wrongly would restore it to the wrong name.
        tracing::warn!(path = %relative.display(), "skipping a park path that is not UTF-8");
        return Ok(());
    };
    let bytes = copy_file(source, destination)?;
    copied.push(ParkEntry { path, bytes });
    Ok(())
}

/// Copy one regular file, keeping its mode bits and modification time.
///
/// The mode carries the executable bit a rebuilt binary needs; the timestamp
/// is what every incremental build system compares against, so a park that
/// lost it would force the rebuild it exists to avoid.
fn copy_file(source: &Path, destination: &Path) -> Result<u64, EngineError> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| EngineError::park(format!("cannot stat {}: {error}", source.display())))?;
    let bytes = fs::copy(source, destination).map_err(|error| {
        EngineError::park(format!(
            "cannot copy {} to {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    let copied = fs::OpenOptions::new()
        .write(true)
        .open(destination)
        .map_err(|error| {
            EngineError::park(format!("cannot reopen {}: {error}", destination.display()))
        })?;
    copied
        .set_permissions(fs::Permissions::from_mode(
            metadata.permissions().mode() & 0o777,
        ))
        .map_err(|error| {
            EngineError::park(format!(
                "cannot set the mode of {}: {error}",
                destination.display()
            ))
        })?;
    if let Ok(modified) = metadata.modified() {
        // Best effort: a volume without timestamp support still holds bytes.
        let _ = copied.set_modified(modified);
    }
    Ok(bytes)
}

/// Swap a complete staging directory into place.
///
/// `rename` cannot replace a non-empty directory, so an existing park is moved
/// aside first and removed afterwards. The window in which neither name is the
/// final one is a single rename wide, and a crash inside it leaves a `.stale`
/// directory that the next `remove_park` collects.
fn publish(staging: &Path, destination: &Path) -> Result<(), EngineError> {
    let superseded = sibling(destination, SUPERSEDED_SUFFIX)?;
    remove_if_present(&superseded)?;
    let replaced = match fs::rename(destination, &superseded) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(EngineError::park(format!(
                "cannot move the existing park at {} aside: {error}",
                destination.display()
            )));
        }
    };
    fs::rename(staging, destination).map_err(|error| {
        EngineError::park(format!(
            "cannot publish the park at {}: {error}",
            destination.display()
        ))
    })?;
    if replaced {
        remove_if_present(&superseded)?;
    }
    Ok(())
}

fn read_manifest(park: &Path) -> Result<Option<ParkManifest>, EngineError> {
    let path = park.join(MANIFEST_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(EngineError::park(format!(
                "cannot read {}: {error}",
                path.display()
            )));
        }
    };
    // A manifest that does not parse describes nothing usable. Treated as a
    // mismatch by every caller rather than as a failure of the volume.
    Ok(serde_json::from_slice(&bytes).ok())
}

fn declined(reason: &str) -> RestoreOutcome {
    RestoreOutcome {
        restored: false,
        bytes: 0,
        reason: Some(reason.to_owned()),
    }
}

fn sibling(destination: &Path, suffix: &str) -> Result<PathBuf, EngineError> {
    let name = destination
        .file_name()
        .ok_or_else(|| EngineError::park("park destination has no name"))?;
    let mut name = name.to_os_string();
    name.push(suffix);
    Ok(destination.with_file_name(name))
}

fn ensure_segment(what: &str, value: &str) -> Result<(), EngineError> {
    let acceptable = !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\', '\0'])
        && !value.starts_with('.');
    if acceptable {
        return Ok(());
    }
    Err(EngineError::park(format!(
        "{what} is not usable as a park directory name"
    )))
}

fn is_safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn relative_path(recorded: &str) -> Result<PathBuf, EngineError> {
    let path = PathBuf::from(recorded);
    if !is_safe_relative(&path) {
        return Err(EngineError::park(
            "park manifest names a path outside the workspace",
        ));
    }
    Ok(path)
}

fn unix_relative(path: &Path) -> Option<String> {
    path.to_str().map(|path| path.replace('\\', "/"))
}

fn directory_name(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return None;
    }
    path.file_name()?.to_str().map(str::to_owned)
}

/// Immediate children of a directory, sorted, or nothing when the directory
/// is not there.
fn read_dir(path: &Path) -> Result<Vec<PathBuf>, EngineError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(EngineError::park(format!(
                "cannot read {}: {error}",
                path.display()
            )));
        }
    };
    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            EngineError::park(format!("cannot read {}: {error}", path.display()))
        })?;
        children.push(entry.path());
    }
    children.sort();
    Ok(children)
}

fn create_dir_all(path: &Path) -> Result<(), EngineError> {
    fs::create_dir_all(path)
        .map_err(|error| EngineError::park(format!("cannot create {}: {error}", path.display())))
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), EngineError> {
    fs::write(path, bytes)
        .map_err(|error| EngineError::park(format!("cannot write {}: {error}", path.display())))
}

fn remove_if_present(path: &Path) -> Result<(), EngineError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(EngineError::park(format!(
            "cannot remove {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests;
