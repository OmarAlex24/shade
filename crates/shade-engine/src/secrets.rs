use serde::{Deserialize, Serialize};
use shade_protocol::{ReviewId, SecretFilePreview, SecretKeyPreview, WorkspaceId};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret storage failed")]
    Io(#[from] std::io::Error),
    #[error("secret path is unsafe: {0}")]
    UnsafePath(String),
    #[error("secret merge still contains conflicts")]
    Conflict,
}

#[derive(Debug, Clone)]
pub struct SecretStore {
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretManifest {
    pub workspace_id: WorkspaceId,
    pub files: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct BaselineManifest {
    files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeChoice {
    Merge,
    KeepChild,
    DiscardChild,
}

impl SecretStore {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, SecretError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        Ok(Self { root })
    }

    /// Captures dotenv and detected-secret files outside Git. The returned manifest contains paths only.
    pub fn capture(
        &self,
        workspace_id: &WorkspaceId,
        workspace: &Path,
    ) -> Result<SecretManifest, SecretError> {
        let destination = self.workspace_root(workspace_id);
        if destination.exists() {
            return self.manifest(workspace_id);
        }
        let staging = self.root.join(format!(".{}.staging", workspace_id.0));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir(&staging)?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
        let mut files = Vec::new();
        for relative in discover_secret_paths(workspace)? {
            let source = secure_workspace_path(workspace, &relative, false)?;
            let target = staging.join("files").join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
            write_private(&target, &fs::read(source)?)?;
            files.push(relative);
        }
        files.sort();
        write_private(
            &staging.join("manifest.json"),
            &serde_json::to_vec(&BaselineManifest {
                files: files.clone(),
            })
            .map_err(std::io::Error::other)?,
        )?;
        fs::rename(staging, destination)?;
        Ok(SecretManifest {
            workspace_id: workspace_id.clone(),
            files,
        })
    }

    pub fn clone_baseline(
        &self,
        parent: &WorkspaceId,
        child: &WorkspaceId,
    ) -> Result<SecretManifest, SecretError> {
        let parent_manifest = self.manifest(parent)?;
        let child_root = self.workspace_root(child);
        let staging = self.root.join(format!(".{}.staging", child.0));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir(&staging)?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
        for path in &parent_manifest.files {
            let source = self.baseline_path(parent, path);
            let destination = staging.join("files").join(path);
            if let Some(directory) = destination.parent() {
                fs::create_dir_all(directory)?;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
            write_private(&destination, &fs::read(source)?)?;
        }
        write_private(
            &staging.join("manifest.json"),
            &serde_json::to_vec(&BaselineManifest {
                files: parent_manifest.files.clone(),
            })
            .map_err(std::io::Error::other)?,
        )?;
        fs::rename(staging, child_root)?;
        Ok(SecretManifest {
            workspace_id: child.clone(),
            files: parent_manifest.files,
        })
    }

    /// Copies current private files, including content-detected secrets, with
    /// private permissions.
    pub fn copy_workspace_secrets(
        &self,
        source_workspace: &Path,
        destination_workspace: &Path,
    ) -> Result<Vec<String>, SecretError> {
        let paths = discover_secret_paths(source_workspace)?;
        for relative in &paths {
            let source = secure_workspace_path(source_workspace, relative, false)?;
            let destination = secure_workspace_path(destination_workspace, relative, true)?;
            write_private(&destination, &fs::read(source)?)?;
        }
        Ok(paths)
    }

    /// Vaults the private files of a workspace that is about to lose its tree.
    ///
    /// Unlike [`SecretStore::capture`], which keeps the first baseline it ever
    /// saw, this replaces any previous vault: the agent may have edited
    /// `.env.local` long after `open`, and sleep must preserve what is on disk
    /// right now. The review baseline is deliberately untouched, so
    /// `secret_cleanup_review` semantics are unchanged.
    pub fn capture_suspension(
        &self,
        workspace_id: &WorkspaceId,
        workspace: &Path,
    ) -> Result<SecretManifest, SecretError> {
        let workspace_root = self.workspace_root(workspace_id);
        fs::create_dir_all(&workspace_root)?;
        fs::set_permissions(&workspace_root, fs::Permissions::from_mode(0o700))?;

        let staging = self
            .root
            .join(format!(".{}.suspend.staging", workspace_id.0));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir(&staging)?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;

        let mut files = Vec::new();
        for relative in discover_secret_paths(workspace)? {
            let source = secure_workspace_path(workspace, &relative, false)?;
            let target = staging.join("files").join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
            write_private(&target, &fs::read(source)?)?;
            files.push(relative);
        }
        files.sort();
        write_private(
            &staging.join("manifest.json"),
            &serde_json::to_vec(&BaselineManifest {
                files: files.clone(),
            })
            .map_err(std::io::Error::other)?,
        )?;

        let destination = self.suspension_root(workspace_id);
        if destination.exists() {
            fs::remove_dir_all(&destination)?;
        }
        fs::rename(staging, destination)?;
        Ok(SecretManifest {
            workspace_id: workspace_id.clone(),
            files,
        })
    }

    /// Whether a suspension vault is there to be restored.
    ///
    /// The manifest is the last thing [`SecretStore::capture_suspension`]
    /// writes into the staging directory, and the whole directory arrives by
    /// one rename, so a readable manifest is what distinguishes a finished
    /// vault from a directory some other step happened to create.
    pub fn has_suspension_vault(&self, workspace_id: &WorkspaceId) -> bool {
        self.suspension_root(workspace_id)
            .join("manifest.json")
            .is_file()
    }

    /// Writes a suspension vault back into a freshly materialized tree.
    ///
    /// The suspended workspace has no tree left, so the wake path cannot use
    /// [`SecretStore::copy_workspace_secrets`], which reads from the source
    /// working copy.
    ///
    /// A missing vault is not an empty one: every sleep writes a manifest,
    /// even for a workspace with no private files at all, so its absence means
    /// something removed it. The caller checks
    /// [`SecretStore::has_suspension_vault`] first and refuses the wake rather
    /// than materializing a tree that silently lost its `.env.local`.
    pub fn restore_suspension(
        &self,
        source_workspace: &WorkspaceId,
        destination: &Path,
    ) -> Result<Vec<String>, SecretError> {
        let vault = self.suspension_root(source_workspace);
        let encoded = fs::read(vault.join("manifest.json"))?;
        let manifest: BaselineManifest =
            serde_json::from_slice(&encoded).map_err(std::io::Error::other)?;
        for relative in &manifest.files {
            let source = vault.join("files").join(relative);
            let target = secure_workspace_path(destination, relative, true)?;
            write_private(&target, &fs::read(source)?)?;
        }
        Ok(manifest.files)
    }

    pub fn preview_current(
        &self,
        baseline_id: &WorkspaceId,
        workspace: &Path,
    ) -> Result<Vec<SecretFilePreview>, SecretError> {
        let baseline = self.manifest_or_empty(baseline_id)?;
        let mut paths: BTreeSet<String> = baseline.files.into_iter().collect();
        paths.extend(discover_secret_paths(workspace)?);
        let mut preview = Vec::new();
        for path in paths {
            let base_bytes = read_optional_bytes(&self.baseline_path(baseline_id, &path))?;
            let current_bytes = read_workspace_bytes(workspace, &path)?;
            let base = read_pairs_optional(&self.baseline_path(baseline_id, &path))?;
            let current = read_workspace_pairs(workspace, &path)?;
            let mut keys = BTreeSet::new();
            keys.extend(base.keys().cloned());
            keys.extend(current.keys().cloned());
            let keys = keys
                .into_iter()
                .map(|key| {
                    let result = match (base.get(&key), current.get(&key)) {
                        (left, right) if left == right => "unchanged",
                        (_, None) => "removed",
                        _ => "child",
                    };
                    SecretKeyPreview {
                        key,
                        result: result.to_owned(),
                    }
                })
                .collect();
            let file_result = if base_bytes == current_bytes {
                "unchanged"
            } else if current_bytes.is_none() {
                "removed"
            } else {
                "child"
            };
            preview.push(SecretFilePreview {
                path,
                file_result: file_result.into(),
                keys,
            });
        }
        Ok(preview)
    }

    /// Compares parent and child against the private baseline without exposing values.
    pub fn preview(
        &self,
        baseline_id: &WorkspaceId,
        parent_workspace: &Path,
        child_workspace: &Path,
    ) -> Result<Vec<SecretFilePreview>, SecretError> {
        let baseline = self.manifest_or_empty(baseline_id)?;
        let mut paths: BTreeSet<String> = baseline.files.into_iter().collect();
        paths.extend(discover_secret_paths(parent_workspace)?);
        paths.extend(discover_secret_paths(child_workspace)?);
        let mut preview = Vec::new();
        for path in paths {
            let base_bytes = read_optional_bytes(&self.baseline_path(baseline_id, &path))?;
            let parent_bytes = read_workspace_bytes(parent_workspace, &path)?;
            let child_bytes = read_workspace_bytes(child_workspace, &path)?;
            let base = read_pairs_optional(&self.baseline_path(baseline_id, &path))?;
            let parent = read_workspace_pairs(parent_workspace, &path)?;
            let child = read_workspace_pairs(child_workspace, &path)?;
            let mut keys = BTreeSet::new();
            keys.extend(base.keys().cloned());
            keys.extend(parent.keys().cloned());
            keys.extend(child.keys().cloned());
            let keys = keys
                .into_iter()
                .map(|key| {
                    let result = merge_result(base.get(&key), parent.get(&key), child.get(&key));
                    SecretKeyPreview {
                        key,
                        result: result.to_owned(),
                    }
                })
                .collect();
            let file_result = merge_label(
                base_bytes.as_ref(),
                parent_bytes.as_ref(),
                child_bytes.as_ref(),
            );
            preview.push(SecretFilePreview {
                path,
                file_result: file_result.into(),
                keys,
            });
        }
        Ok(preview)
    }

    /// Prove all document merges before allocating a successor. Binary and
    /// structural conflicts have no conflicting key in the public preview.
    pub fn can_merge(
        &self,
        baseline_id: &WorkspaceId,
        parent: &Path,
        child: &Path,
    ) -> Result<bool, SecretError> {
        for file in self.preview(baseline_id, parent, child)? {
            match self.merge_file(baseline_id, &file.path, parent, child) {
                Ok(_) => {}
                Err(SecretError::Conflict) => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    fn merge_file(
        &self,
        baseline_id: &WorkspaceId,
        path: &str,
        parent: &Path,
        child: &Path,
    ) -> Result<Option<Vec<u8>>, SecretError> {
        let base_path = self.baseline_path(baseline_id, path);
        let pairs = merge_pairs(
            &read_pairs_optional(&base_path)?,
            &read_workspace_pairs(parent, path)?,
            &read_workspace_pairs(child, path)?,
        )?;
        merge_documents(
            Path::new(path),
            read_optional_bytes(&base_path)?.as_deref(),
            read_workspace_bytes(parent, path)?.as_deref(),
            read_workspace_bytes(child, path)?.as_deref(),
            &pairs,
        )
    }

    /// Applies a reviewed decision to a new successor directory. Parent is never mutated.
    pub fn apply_reviewed(
        &self,
        baseline_id: &WorkspaceId,
        parent_workspace: &Path,
        child_workspace: &Path,
        successor_workspace: &Path,
        choice: MergeChoice,
    ) -> Result<(), SecretError> {
        let preview = self.preview(baseline_id, parent_workspace, child_workspace)?;
        if choice == MergeChoice::Merge
            && preview
                .iter()
                .any(|file| file.keys.iter().any(|key| key.result == "conflict"))
        {
            return Err(SecretError::Conflict);
        }
        for file in preview {
            if choice != MergeChoice::Merge {
                let source_root = match choice {
                    MergeChoice::KeepChild => child_workspace,
                    MergeChoice::DiscardChild => parent_workspace,
                    MergeChoice::Merge => unreachable!(),
                };
                let selected = read_workspace_bytes(source_root, &file.path)?;
                let destination =
                    secure_workspace_path(successor_workspace, &file.path, selected.is_some())?;
                if let Some(bytes) = selected {
                    write_private(&destination, &bytes)?;
                } else if destination.exists() {
                    fs::remove_file(destination)?;
                }
                continue;
            }
            let merged_bytes =
                self.merge_file(baseline_id, &file.path, parent_workspace, child_workspace)?;
            if merged_bytes.is_none() {
                let destination = secure_workspace_path(successor_workspace, &file.path, false)?;
                if destination.exists() {
                    fs::remove_file(destination)?;
                }
                continue;
            }
            let destination = secure_workspace_path(successor_workspace, &file.path, true)?;
            write_private(&destination, merged_bytes.as_deref().unwrap_or_default())?;
        }
        Ok(())
    }

    /// An immutable private copy fences a decision to the bytes actually reviewed.
    /// No fingerprint, values or fragments enter the public review payload.
    pub fn capture_review(
        &self,
        workspace_id: &WorkspaceId,
        review_id: &ReviewId,
        child: &Path,
        parent: Option<&Path>,
    ) -> Result<(), SecretError> {
        let directory = self.review_root(workspace_id);
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let known = self.manifest_or_empty(workspace_id)?.files;
        let snapshot = review_snapshot(child, parent, &known)?;
        let bytes = serde_json::to_vec(&snapshot).map_err(std::io::Error::other)?;
        let mut staged = tempfile::Builder::new()
            .prefix(".review-")
            .suffix(".staging")
            .tempfile_in(&directory)?;
        staged
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        staged.write_all(&bytes)?;
        staged.as_file().sync_all()?;
        crate::faults::hit(crate::faults::Point::ReviewSnapshotStaged);
        staged
            .persist_noclobber(directory.join(format!("{}.json", review_id.0)))
            .map_err(|error| error.error)?;
        fs::File::open(&directory)?.sync_all()?;
        crate::faults::hit(crate::faults::Point::ReviewSnapshotPromoted);
        Ok(())
    }

    pub fn review_is_current(
        &self,
        workspace_id: &WorkspaceId,
        review_id: &ReviewId,
        child: &Path,
        parent: Option<&Path>,
    ) -> Result<bool, SecretError> {
        let path = self
            .review_root(workspace_id)
            .join(format!("{}.json", review_id.0));
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let snapshot: ReviewSnapshot =
            serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
        let known = self.manifest_or_empty(workspace_id)?.files;
        Ok(snapshot == review_snapshot(child, parent, &known)?)
    }

    fn review_root(&self, workspace_id: &WorkspaceId) -> PathBuf {
        self.root.join(format!("reviews-{}", workspace_id.0))
    }

    pub fn cleanup_review_staging(&self) -> Result<u64, SecretError> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir()
                || !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("reviews-ws_")
            {
                continue;
            }
            for snapshot in fs::read_dir(entry.path())? {
                let snapshot = snapshot?;
                let name = snapshot.file_name();
                let name = name.to_string_lossy();
                if snapshot.file_type()?.is_file()
                    && name.starts_with(".review-")
                    && name.ends_with(".staging")
                {
                    fs::remove_file(snapshot.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    pub fn remove(&self, workspace_id: &WorkspaceId) -> Result<(), SecretError> {
        let path = self.workspace_root(workspace_id);
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
        let reviews = self.review_root(workspace_id);
        if reviews.exists() {
            fs::remove_dir_all(reviews)?;
        }
        Ok(())
    }

    fn manifest_or_empty(&self, workspace_id: &WorkspaceId) -> Result<SecretManifest, SecretError> {
        if !self.workspace_root(workspace_id).exists() {
            // An interrupted open may not have captured its first baseline.
            // Treat every current secret as newly added, never as disposable.
            return Ok(SecretManifest {
                workspace_id: workspace_id.clone(),
                files: Vec::new(),
            });
        }
        self.manifest(workspace_id)
    }

    fn manifest(&self, workspace_id: &WorkspaceId) -> Result<SecretManifest, SecretError> {
        let files = self.load_manifest(workspace_id)?.files;
        Ok(SecretManifest {
            workspace_id: workspace_id.clone(),
            files,
        })
    }

    fn load_manifest(&self, workspace_id: &WorkspaceId) -> Result<BaselineManifest, SecretError> {
        let encoded = fs::read(self.workspace_root(workspace_id).join("manifest.json"))?;
        serde_json::from_slice(&encoded).map_err(|error| std::io::Error::other(error).into())
    }

    fn baseline_path(&self, workspace_id: &WorkspaceId, path: &str) -> PathBuf {
        self.workspace_root(workspace_id).join("files").join(path)
    }

    fn workspace_root(&self, workspace_id: &WorkspaceId) -> PathBuf {
        self.root.join(&workspace_id.0)
    }

    fn suspension_root(&self, workspace_id: &WorkspaceId) -> PathBuf {
        self.workspace_root(workspace_id).join("suspended")
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct ReviewSnapshot {
    child: BTreeMap<String, Vec<u8>>,
    parent: Option<BTreeMap<String, Vec<u8>>>,
}

fn review_snapshot(
    child: &Path,
    parent: Option<&Path>,
    known: &[String],
) -> Result<ReviewSnapshot, SecretError> {
    fn files(root: &Path, known: &[String]) -> Result<BTreeMap<String, Vec<u8>>, SecretError> {
        let mut paths = discover_secret_paths(root)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        paths.extend(known.iter().cloned());
        let mut snapshot = BTreeMap::new();
        for relative in paths {
            if let Some(bytes) = read_workspace_bytes(root, &relative)? {
                snapshot.insert(relative, bytes);
            }
        }
        Ok(snapshot)
    }
    Ok(ReviewSnapshot {
        child: files(child, known)?,
        parent: parent.map(|root| files(root, known)).transpose()?,
    })
}

pub(crate) fn discover_secret_paths(root: &Path) -> Result<Vec<String>, SecretError> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | "node_modules" | ".venv" | "target" | "vendor" | ".shade")
                )
        })
    {
        let entry =
            entry.map_err(|error| SecretError::Io(std::io::Error::other(error.to_string())))?;
        if entry.path() != root
            && (is_secret_path(entry.path())
                || (entry.file_type().is_file()
                    && crate::secret_policy::file_contains_secret(entry.path())?))
        {
            if entry.file_type().is_symlink() || !entry.file_type().is_file() {
                return Err(SecretError::UnsafePath(relative(root, entry.path())?));
            }
            paths.push(relative(root, entry.path())?);
        }
    }
    Ok(paths)
}

fn is_secret_path(path: &Path) -> bool {
    crate::secret_policy::dotenv_path(path)
}

fn relative(root: &Path, path: &Path) -> Result<String, SecretError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| SecretError::UnsafePath(path.display().to_string()))?;
    if relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) {
        return Err(SecretError::UnsafePath(relative.display().to_string()));
    }
    relative
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| SecretError::UnsafePath("secret path is not UTF-8".into()))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_workspace_pairs(
    root: &Path,
    relative: &str,
) -> Result<BTreeMap<String, String>, SecretError> {
    let path = secure_workspace_path(root, relative, false)?;
    read_pairs_optional(&path)
}

fn read_workspace_bytes(root: &Path, relative: &str) -> Result<Option<Vec<u8>>, SecretError> {
    let path = secure_workspace_path(root, relative, false)?;
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn secure_workspace_path(
    root: &Path,
    relative: &str,
    create_parents: bool,
) -> Result<PathBuf, SecretError> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(SecretError::UnsafePath(relative.into()));
    }
    let root_metadata = fs::symlink_metadata(root)?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(SecretError::UnsafePath(root.display().to_string()));
    }
    let components = relative_path.components().collect::<Vec<_>>();
    let mut current = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            return Err(SecretError::UnsafePath(relative.into()));
        };
        current.push(name);
        let final_component = index + 1 == components.len();
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SecretError::UnsafePath(relative.into()));
            }
            Ok(metadata) if !final_component && !metadata.is_dir() => {
                return Err(SecretError::UnsafePath(relative.into()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !final_component => {
                if !create_parents {
                    return Ok(root.join(relative_path));
                }
                fs::create_dir(&current)?;
                fs::set_permissions(&current, fs::Permissions::from_mode(0o700))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

fn read_pairs_optional(path: &Path) -> Result<BTreeMap<String, String>, SecretError> {
    let Some(bytes) = read_optional_bytes(path)? else {
        return Ok(BTreeMap::new());
    };
    let Ok(contents) = std::str::from_utf8(&bytes) else {
        return Ok(BTreeMap::new());
    };
    if is_secret_path(path) {
        return Ok(parse_pairs(contents));
    }
    let structured = if path
        .extension()
        .is_some_and(|extension| extension == "toml")
    {
        toml::from_str::<toml::Value>(contents)
            .ok()
            .and_then(|value| serde_json::to_value(value).ok())
    } else {
        parse_private_json(contents.as_bytes()).ok()
    };
    if let Some(value) = structured {
        let mut pairs = BTreeMap::new();
        flatten_keys("", &value, &mut pairs);
        Ok(pairs)
    } else {
        Ok(crate::secret_policy::credential_pairs(&bytes))
    }
}

fn flatten_keys(path: &str, value: &serde_json::Value, pairs: &mut BTreeMap<String, String>) {
    if let Some(object) = value.as_object().filter(|object| !object.is_empty()) {
        for (key, value) in object {
            let escaped = key.replace('~', "~0").replace('/', "~1");
            flatten_keys(&format!("{path}/{escaped}"), value, pairs);
        }
    } else {
        pairs.insert(path.to_owned(), value.to_string());
    }
}

// Duplicate object keys are ambiguous. Refuse structural merging instead of
// silently discarding earlier values through serde_json's last-key-wins map.
fn parse_private_json(bytes: &[u8]) -> Result<serde_json::Value, serde_json::Error> {
    struct Unique(serde_json::Value);
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Unique;
                fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                    formatter.write_str("unambiguous JSON")
                }
                fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Unique, E> {
                    serde_json::Number::from_f64(value)
                        .map(|number| Unique(number.into()))
                        .ok_or_else(|| E::custom("invalid JSON number"))
                }
                fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Unique, E> {
                    Ok(Unique(value.into()))
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
                    Ok(Unique(serde_json::Value::Null))
                }
                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut sequence: A,
                ) -> Result<Unique, A::Error> {
                    let mut values = Vec::new();
                    while let Some(Unique(value)) = sequence.next_element()? {
                        values.push(value);
                    }
                    Ok(Unique(values.into()))
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut object: A,
                ) -> Result<Unique, A::Error> {
                    let mut values = serde_json::Map::new();
                    while let Some((key, Unique(value))) = object.next_entry::<String, Unique>()? {
                        if values.insert(key, value).is_some() {
                            return Err(serde::de::Error::custom("duplicate JSON key"));
                        }
                    }
                    Ok(Unique(values.into()))
                }
            }
            deserializer.deserialize_any(Visitor)
        }
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let Unique(value) = Unique::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

fn read_optional_bytes(path: &Path) -> Result<Option<Vec<u8>>, SecretError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn parse_pairs(contents: &str) -> BTreeMap<String, String> {
    contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
            let (key, value) = trimmed.split_once('=')?;
            let key = key.trim();
            if key.is_empty()
                || !key
                    .chars()
                    .all(|character| character == '_' || character.is_ascii_alphanumeric())
            {
                return None;
            }
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn merge_result(
    base: Option<&String>,
    parent: Option<&String>,
    child: Option<&String>,
) -> &'static str {
    merge_label(base, parent, child)
}

fn merge_label<T: PartialEq>(
    base: Option<&T>,
    parent: Option<&T>,
    child: Option<&T>,
) -> &'static str {
    if parent == child {
        return if parent.is_none() {
            "removed"
        } else {
            "unchanged"
        };
    }
    if child == base {
        return if parent.is_none() {
            "removed"
        } else {
            "parent"
        };
    }
    if parent == base {
        return if child.is_none() { "removed" } else { "child" };
    }
    "conflict"
}

fn merge_pairs(
    base: &BTreeMap<String, String>,
    parent: &BTreeMap<String, String>,
    child: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, SecretError> {
    let mut keys = BTreeSet::new();
    keys.extend(base.keys().cloned());
    keys.extend(parent.keys().cloned());
    keys.extend(child.keys().cloned());
    let mut merged = BTreeMap::new();
    for key in keys {
        let base_value = base.get(&key);
        let parent_value = parent.get(&key);
        let child_value = child.get(&key);
        let selected = if parent_value == child_value || child_value == base_value {
            parent_value
        } else if parent_value == base_value {
            child_value
        } else {
            return Err(SecretError::Conflict);
        };
        if let Some(value) = selected {
            merged.insert(key, value.clone());
        }
    }
    Ok(merged)
}

#[derive(Default)]
struct DotenvDocument {
    lines: Vec<String>,
    assignments: BTreeMap<String, String>,
    values: BTreeMap<String, String>,
    other_lines: Vec<String>,
    duplicate_key: bool,
}

impl DotenvDocument {
    fn parse(bytes: Option<&[u8]>) -> Result<Self, SecretError> {
        let Some(bytes) = bytes else {
            return Ok(Self::default());
        };
        let text = std::str::from_utf8(bytes).map_err(|_| SecretError::Conflict)?;
        let mut document = Self::default();
        for raw in text.split_inclusive('\n') {
            let line = raw.trim_end_matches('\n').trim_end_matches('\r');
            if let Some((key, value)) = parse_assignment(line) {
                if document.values.insert(key.clone(), value).is_some() {
                    document.duplicate_key = true;
                }
                document.assignments.insert(key, raw.to_owned());
            } else {
                document.other_lines.push(raw.to_owned());
            }
            document.lines.push(raw.to_owned());
        }
        if !text.is_empty() && !text.ends_with('\n') && document.lines.is_empty() {
            document.lines.push(text.to_owned());
        }
        Ok(document)
    }
}

fn parse_assignment(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let (key, value) = trimmed.split_once('=')?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return None;
    }
    Some((key.to_owned(), value.to_owned()))
}

fn merge_documents(
    path: &Path,
    base: Option<&[u8]>,
    parent: Option<&[u8]>,
    child: Option<&[u8]>,
    merged: &BTreeMap<String, String>,
) -> Result<Option<Vec<u8>>, SecretError> {
    if parent == child {
        return Ok(parent.map(<[u8]>::to_vec));
    }
    if child == base {
        return Ok(parent.map(<[u8]>::to_vec));
    }
    if parent == base {
        return Ok(child.map(<[u8]>::to_vec));
    }
    if !is_secret_path(path) {
        return merge_structured(path, base, parent, child);
    }
    let base_document = DotenvDocument::parse(base)?;
    let parent_document = DotenvDocument::parse(parent)?;
    let child_document = DotenvDocument::parse(child)?;
    if base_document.duplicate_key || parent_document.duplicate_key || child_document.duplicate_key
    {
        return Err(SecretError::Conflict);
    }
    let template = if parent_document.other_lines == child_document.other_lines
        || child_document.other_lines == base_document.other_lines
    {
        &parent_document
    } else if parent_document.other_lines == base_document.other_lines {
        &child_document
    } else {
        return Err(SecretError::Conflict);
    };
    let mut rendered = String::new();
    let mut emitted = BTreeSet::new();
    for raw in &template.lines {
        let line = raw.trim_end_matches('\n').trim_end_matches('\r');
        let Some((key, _)) = parse_assignment(line) else {
            rendered.push_str(raw);
            continue;
        };
        let Some(value) = merged.get(&key) else {
            continue;
        };
        let selected = [template, &parent_document, &child_document, &base_document]
            .into_iter()
            .find(|document| document.values.get(&key) == Some(value))
            .and_then(|document| document.assignments.get(&key))
            .cloned()
            .unwrap_or_else(|| format!("{key}={value}\n"));
        rendered.push_str(&selected);
        emitted.insert(key);
    }
    for (key, value) in merged {
        if emitted.contains(key) {
            continue;
        }
        if !rendered.is_empty() && !rendered.ends_with('\n') {
            rendered.push('\n');
        }
        let selected = [&parent_document, &child_document, &base_document]
            .into_iter()
            .find(|document| document.values.get(key) == Some(value))
            .and_then(|document| document.assignments.get(key))
            .cloned()
            .unwrap_or_else(|| format!("{key}={value}\n"));
        rendered.push_str(&selected);
    }
    Ok((!rendered.is_empty()).then(|| rendered.into_bytes()))
}

// Arrays and scalar values are atomic. Object/table keys merge recursively;
// delete-versus-edit and type changes are conflicts, never lossy coercions.
trait PrivateNode: Clone + PartialEq {
    fn entries(&self) -> Option<BTreeMap<String, Self>>;
    fn object(entries: BTreeMap<String, Self>) -> Self;
}

impl PrivateNode for serde_json::Value {
    fn entries(&self) -> Option<BTreeMap<String, Self>> {
        self.as_object().map(|values| {
            values
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
    }
    fn object(entries: BTreeMap<String, Self>) -> Self {
        Self::Object(entries.into_iter().collect())
    }
}

impl PrivateNode for toml::Value {
    fn entries(&self) -> Option<BTreeMap<String, Self>> {
        self.as_table().map(|values| {
            values
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
    }
    fn object(entries: BTreeMap<String, Self>) -> Self {
        Self::Table(entries.into_iter().collect())
    }
}

fn merge_nodes<T: PrivateNode>(
    base: Option<&T>,
    parent: Option<&T>,
    child: Option<&T>,
) -> Result<Option<T>, SecretError> {
    if parent == child || child == base {
        return Ok(parent.cloned());
    }
    if parent == base {
        return Ok(child.cloned());
    }
    let base = match base {
        Some(value) => value.entries().ok_or(SecretError::Conflict)?,
        None => BTreeMap::new(),
    };
    let parent = parent
        .and_then(PrivateNode::entries)
        .ok_or(SecretError::Conflict)?;
    let child = child
        .and_then(PrivateNode::entries)
        .ok_or(SecretError::Conflict)?;
    let keys = base
        .keys()
        .chain(parent.keys())
        .chain(child.keys())
        .collect::<BTreeSet<_>>();
    let mut merged = BTreeMap::new();
    for key in keys {
        if let Some(value) = merge_nodes(base.get(key), parent.get(key), child.get(key))? {
            merged.insert(key.clone(), value);
        }
    }
    Ok(Some(T::object(merged)))
}

fn merge_structured(
    path: &Path,
    base: Option<&[u8]>,
    parent: Option<&[u8]>,
    child: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, SecretError> {
    if path
        .extension()
        .is_some_and(|extension| extension == "toml")
    {
        let parse = |bytes: Option<&[u8]>| {
            bytes
                .map(|bytes| {
                    let text = std::str::from_utf8(bytes).map_err(|_| SecretError::Conflict)?;
                    toml::from_str::<toml::Value>(text).map_err(|_| SecretError::Conflict)
                })
                .transpose()
        };
        let (base, parent, child) = (parse(base)?, parse(parent)?, parse(child)?);
        merge_nodes(base.as_ref(), parent.as_ref(), child.as_ref())?
            .map(|value| {
                toml::to_string_pretty(&value)
                    .map(String::into_bytes)
                    .map_err(|_| SecretError::Conflict)
            })
            .transpose()
    } else {
        let parse = |bytes: Option<&[u8]>| {
            bytes
                .map(|bytes| parse_private_json(bytes).map_err(|_| SecretError::Conflict))
                .transpose()
        };
        let (base, parent, child) = (parse(base)?, parse(parent)?, parse(child)?);
        merge_nodes(base.as_ref(), parent.as_ref(), child.as_ref())?
            .map(|value| {
                let mut bytes =
                    serde_json::to_vec_pretty(&value).map_err(|_| SecretError::Conflict)?;
                bytes.push(b'\n');
                Ok(bytes)
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_key_names_without_values_and_merges_to_successor() {
        let directory = tempfile::tempdir().unwrap();
        let store = SecretStore::new(directory.path().join("private")).unwrap();
        let parent = directory.path().join("parent");
        let child = directory.path().join("child");
        let successor = directory.path().join("successor");
        fs::create_dir_all(&parent).unwrap();
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&successor).unwrap();
        fs::write(
            parent.join(".env"),
            "# preserved comment\nexport TOKEN =secret-base-value\nnot-an-assignment\nPARENT=old\n",
        )
        .unwrap();
        let workspace_id = WorkspaceId("ws_secret".into());
        store.capture(&workspace_id, &parent).unwrap();
        fs::write(
            parent.join(".env"),
            "# preserved comment\nexport TOKEN =secret-base-value\nnot-an-assignment\nPARENT=new\n",
        )
        .unwrap();
        fs::write(
            child.join(".env"),
            "# preserved comment\nexport TOKEN =secret-child-value\nnot-an-assignment\nPARENT=old\n",
        )
        .unwrap();
        let preview = store.preview(&workspace_id, &parent, &child).unwrap();
        let encoded = serde_json::to_string(&preview).unwrap();
        assert!(!encoded.contains("secret-base-value"));
        assert!(!encoded.contains("secret-child-value"));
        assert!(encoded.contains("TOKEN"));
        store
            .apply_reviewed(
                &workspace_id,
                &parent,
                &child,
                &successor,
                MergeChoice::Merge,
            )
            .unwrap();
        let merged = fs::read_to_string(successor.join(".env")).unwrap();
        assert!(merged.contains("PARENT=new"));
        assert!(merged.contains("# preserved comment"));
        assert!(merged.contains("not-an-assignment"));
        assert!(merged.contains("export TOKEN =secret-child-value"));
        let mode = fs::metadata(directory.path().join("private/ws_secret/files/.env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn conflicting_values_require_review_resolution() {
        let directory = tempfile::tempdir().unwrap();
        let store = SecretStore::new(directory.path().join("private")).unwrap();
        let parent = directory.path().join("parent");
        let child = directory.path().join("child");
        let successor = directory.path().join("successor");
        fs::create_dir_all(&parent).unwrap();
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&successor).unwrap();
        fs::write(parent.join(".env.local"), "TOKEN=base\n").unwrap();
        let workspace_id = WorkspaceId("ws_secret".into());
        store.capture(&workspace_id, &parent).unwrap();
        fs::write(parent.join(".env.local"), "TOKEN=parent\n").unwrap();
        fs::write(child.join(".env.local"), "TOKEN=child\n").unwrap();
        assert!(matches!(
            store.apply_reviewed(
                &workspace_id,
                &parent,
                &child,
                &successor,
                MergeChoice::Merge
            ),
            Err(SecretError::Conflict)
        ));
    }

    #[test]
    fn captures_every_dotenv_prefix_without_exposing_values() {
        let directory = tempfile::tempdir().unwrap();
        let store = SecretStore::new(directory.path().join("private")).unwrap();
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join(".envrc"), "TOKEN=never-print-this\n").unwrap();
        fs::write(workspace.join(".environment"), "NAME=also-private\n").unwrap();
        let workspace_id = WorkspaceId("ws_prefixes".into());
        let manifest = store.capture(&workspace_id, &workspace).unwrap();
        assert_eq!(manifest.files, [".environment", ".envrc"]);
        fs::write(
            workspace.join(".envrc"),
            "# comment-only change\nTOKEN=never-print-this\n",
        )
        .unwrap();
        let preview = store.preview_current(&workspace_id, &workspace).unwrap();
        let encoded = serde_json::to_string(&preview).unwrap();
        assert!(encoded.contains("TOKEN"));
        assert!(encoded.contains("NAME"));
        assert!(!encoded.contains("never-print-this"));
        assert!(!encoded.contains("also-private"));
        let envrc = preview.iter().find(|file| file.path == ".envrc").unwrap();
        assert_eq!(envrc.file_result, "child");
        assert_eq!(envrc.keys[0].result, "unchanged");
    }

    #[test]
    fn rejects_symlink_ancestors_when_applying_reviewed_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let store = SecretStore::new(directory.path().join("private")).unwrap();
        let parent = directory.path().join("parent");
        let child = directory.path().join("child");
        let successor = directory.path().join("successor");
        let outside = directory.path().join("outside");
        for root in [&parent, &child, &successor, &outside] {
            fs::create_dir_all(root).unwrap();
        }
        fs::create_dir_all(parent.join("nested")).unwrap();
        fs::create_dir_all(child.join("nested")).unwrap();
        fs::write(parent.join("nested/.env"), "TOKEN=base\n").unwrap();
        let workspace_id = WorkspaceId("ws_symlink".into());
        store.capture(&workspace_id, &parent).unwrap();
        fs::write(child.join("nested/.env"), "TOKEN=child\n").unwrap();
        std::os::unix::fs::symlink(&outside, successor.join("nested")).unwrap();
        assert!(matches!(
            store.apply_reviewed(
                &workspace_id,
                &parent,
                &child,
                &successor,
                MergeChoice::KeepChild,
            ),
            Err(SecretError::UnsafePath(_))
        ));
        assert!(!outside.join(".env").exists());
    }
}
