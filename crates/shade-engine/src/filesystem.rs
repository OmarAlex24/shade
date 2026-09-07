//! Workspace tree materialization.
//!
//! Production uses forced APFS clones, staged beside the destination and
//! atomically renamed into place. Immutable daemon-owned trees of at most
//! 25,000 entries without hardlinks may use one directory clone. Other trees
//! use the controlled traversal, which
//! inventories each directory with `getattrlistbulk`, then clones entries
//! directly into the unpublished staging tree with `clonefileat` relative to
//! pinned directory descriptors in a bounded worker set. `CopyFilesystem` is a
//! deliberately boring deterministic fake for
//! tests; it is never selected as a production fallback and only exists under
//! `cfg(test)` or the `test-support` feature, so it is absent from the
//! distribution binary entirely.

use std::collections::BTreeMap;
#[cfg(target_os = "macos")]
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File};
use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
// Only `set_portable_mode`, itself test-only, needs `Permissions::from_mode`.
#[cfg(any(test, feature = "test-support"))]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use anyhow::{Context, ensure};
use thiserror::Error;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub logical_bytes: u64,
    pub referenced_bytes: u64,
    pub private_bytes: Option<u64>,
}

pub trait WorkspaceFilesystem: Send + Sync {
    /// Publish a complete, independent tree at `destination`.
    fn clone_tree(&self, source: &Path, destination: &Path) -> anyhow::Result<()>;
    /// Clone a verified, daemon-owned base or layer that the caller holds
    /// immutable for the entire call. Never use this for an agent workspace.
    fn clone_immutable_tree(&self, source: &Path, destination: &Path) -> anyhow::Result<()> {
        self.clone_tree(source, destination)
    }
    /// Atomically promote an already verified sibling staging directory. The
    /// destination must not be replaced if it appears concurrently.
    fn publish_tree(&self, staging: &Path, destination: &Path) -> anyhow::Result<()>;
    fn usage(&self, root: &Path) -> anyhow::Result<Usage>;
    fn remove_tree(&self, root: &Path) -> anyhow::Result<()>;
}

#[derive(Debug, Error)]
#[error("COW_UNAVAILABLE: cannot clone {source_path} to {destination_path}: {reason}")]
pub struct CowUnavailable {
    pub source_path: PathBuf,
    pub destination_path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct ApfsFilesystem;

/// Stable evidence label for controlled materialization of mutable sources.
pub const APFS_CLONE_STRATEGY: &str =
    "getattrlistbulk_direct_clonefileat_per_entry_staged_exclusive_publish";
pub const APFS_IMMUTABLE_CLONE_STRATEGY: &str =
    "readdir_bounded_immutable_fclonefileat_staged_exclusive_publish";

/// Full-copy fake used only by tests. Production code must construct
/// `ApfsFilesystem` directly. Compiled only under `cfg(test)` or the
/// `test-support` feature, so a downstream embedder cannot reach it through
/// `Engine::with_components` and silently trade APFS clones for byte copies.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
pub struct CopyFilesystem;

impl WorkspaceFilesystem for ApfsFilesystem {
    fn clone_tree(&self, source: &Path, destination: &Path) -> anyhow::Result<()> {
        clone_tree_apfs(source, destination, false)
    }

    fn clone_immutable_tree(&self, source: &Path, destination: &Path) -> anyhow::Result<()> {
        clone_tree_apfs(source, destination, true)
    }

    fn publish_tree(&self, staging: &Path, destination: &Path) -> anyhow::Result<()> {
        publish_tree_checked(staging, destination)
    }

    fn usage(&self, root: &Path) -> anyhow::Result<Usage> {
        apfs_usage(root)
    }

    fn remove_tree(&self, root: &Path) -> anyhow::Result<()> {
        remove_tree_checked(root)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl WorkspaceFilesystem for CopyFilesystem {
    fn clone_tree(&self, source: &Path, destination: &Path) -> anyhow::Result<()> {
        ensure_source_and_destination(source, destination)?;
        let parent = destination
            .parent()
            .context("clone destination needs a parent directory")?;
        fs::create_dir_all(parent)?;
        let staging = tempfile::Builder::new()
            .prefix(".shade-copy-fake-")
            .tempdir_in(parent)?;
        let mut hardlinks = BTreeMap::new();
        copy_entries(source, staging.path(), &mut hardlinks)?;
        set_portable_mode(staging.path(), &fs::symlink_metadata(source)?)?;
        publish_tree_checked(staging.path(), destination)
    }

    fn publish_tree(&self, staging: &Path, destination: &Path) -> anyhow::Result<()> {
        publish_tree_checked(staging, destination)
    }

    fn usage(&self, root: &Path) -> anyhow::Result<Usage> {
        fake_usage(root)
    }

    fn remove_tree(&self, root: &Path) -> anyhow::Result<()> {
        remove_tree_checked(root)
    }
}

#[cfg(target_os = "macos")]
fn clone_tree_apfs(source: &Path, destination: &Path, immutable: bool) -> anyhow::Result<()> {
    ensure_source_and_destination(source, destination)?;
    let parent = destination
        .parent()
        .context("clone destination needs a parent directory")?;
    fs::create_dir_all(parent)?;
    require_same_apfs(source, parent, destination)?;

    let staging = tempfile::Builder::new()
        .prefix(".shade-apfs-clone-")
        .tempdir_in(parent)?;
    let source = fs::canonicalize(source)?;
    let staging_root = fs::canonicalize(staging.path())?;
    let source_fd = open_root_directory(&source)?;
    let destination_fd = open_root_directory(&staging_root)?;
    let source_stat = directory_stat(source_fd.as_raw_fd())?;
    let mut immutable_plan = ImmutableClonePlan::default();
    let use_directory_clone =
        immutable && immutable_plan.inventory(source_fd.as_raw_fd(), source_stat.st_dev)?;
    // Directory cloning loses hardlink topology. A controlled clone is also
    // mandatory above the kernel-blocking budget or for mutable sources.
    if use_directory_clone {
        drop(destination_fd);
        fs::remove_dir(staging.path())?;
        let parent_fd = open_root_directory(&fs::canonicalize(parent)?)?;
        let name = path_to_cstring(Path::new(
            staging.path().file_name().context("staging name")?,
        ))?;
        const CLONE_NOFOLLOW: u32 = 0x0001;
        const CLONE_NOFOLLOW_ANY: u32 = 0x0008;
        const CLONE_RESOLVE_BENEATH: u32 = 0x0010;
        let result = unsafe {
            libc::fclonefileat(
                source_fd.as_raw_fd(),
                parent_fd.as_raw_fd(),
                name.as_ptr(),
                CLONE_NOFOLLOW | CLONE_NOFOLLOW_ANY | CLONE_RESOLVE_BENEATH,
            )
        };
        if result == -1 {
            return Err(CowUnavailable {
                source_path: source,
                destination_path: destination.to_owned(),
                reason: io::Error::last_os_error().to_string(),
            }
            .into());
        }
        let cloned_root = open_root_directory(staging.path())?;
        for (directory, mode) in immutable_plan.directories.iter().rev() {
            let descriptor = open_relative_directory(cloned_root.as_raw_fd(), directory)?;
            set_directory_mode(descriptor.as_raw_fd(), *mode)?;
        }
        set_directory_mode(
            cloned_root.as_raw_fd(),
            u32::from(source_stat.st_mode & 0o777),
        )?;
        return publish_tree_checked(staging.path(), destination);
    }
    let mut plan = ClonePlan::default();
    let mut hardlinks = BTreeMap::new();
    let mut bulk_buffer = vec![0_u64; BULK_BUFFER_BYTES / std::mem::size_of::<u64>()];
    plan_clone_entries(
        source_fd.as_raw_fd(),
        Path::new(""),
        source_stat.st_dev,
        &mut bulk_buffer,
        &mut plan,
        &mut hardlinks,
    )?;
    for (directory, _) in &plan.directories {
        create_directory_at(destination_fd.as_raw_fd(), directory)?;
    }
    execute_clone_operations(
        source_fd.as_raw_fd(),
        destination_fd.as_raw_fd(),
        &source,
        &staging_root,
        &plan.operations,
    )?;
    for hardlink in &plan.hardlinks {
        create_hardlink(destination_fd.as_raw_fd(), &source, &staging_root, hardlink)?;
    }
    for (directory, mode) in plan.directories.iter().rev() {
        let descriptor = open_relative_directory(destination_fd.as_raw_fd(), directory)?;
        set_directory_mode(descriptor.as_raw_fd(), *mode)?;
    }
    set_directory_mode(
        destination_fd.as_raw_fd(),
        u32::from(source_stat.st_mode & 0o777),
    )?;
    publish_tree_checked(staging.path(), destination)
}

#[cfg(target_os = "macos")]
fn rename_exclusive(source: &Path, destination: &Path) -> io::Result<()> {
    let source = path_to_cstring(source)?;
    let destination = path_to_cstring(destination)?;
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_exclusive(source: &Path, destination: &Path) -> io::Result<()> {
    let source = path_to_cstring(source)?;
    let destination = path_to_cstring(destination)?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_exclusive(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exclusive directory promotion is unsupported on this platform",
    ))
}

fn publish_tree_checked(staging: &Path, destination: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(staging)
        .with_context(|| format!("cannot inspect staging tree {}", staging.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "staging tree must be a real directory"
    );
    let parent = destination
        .parent()
        .context("published tree destination needs a parent")?;
    ensure!(
        staging.parent() == Some(parent),
        "staging tree must be a sibling of its destination"
    );
    File::open(staging)?.sync_all()?;
    crate::faults::hit(crate::faults::Point::CloneStaged);
    rename_exclusive(staging, destination).with_context(|| {
        format!(
            "cannot exclusively publish tree at {}",
            destination.display()
        )
    })?;
    File::open(parent)?.sync_all()?;
    crate::faults::hit(crate::faults::Point::ClonePromoted);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn clone_tree_apfs(source: &Path, destination: &Path, _immutable: bool) -> anyhow::Result<()> {
    Err(CowUnavailable {
        source_path: source.to_owned(),
        destination_path: destination.to_owned(),
        reason: "APFS clonefile is available only on macOS".to_owned(),
    }
    .into())
}

fn ensure_source_and_destination(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let source_metadata = fs::symlink_metadata(source)
        .with_context(|| format!("cannot inspect source {}", source.display()))?;
    ensure!(
        source_metadata.is_dir() && !source_metadata.file_type().is_symlink(),
        "workspace source must be a real directory"
    );
    ensure!(
        !destination.exists(),
        "workspace destination already exists"
    );
    ensure!(
        fs::symlink_metadata(destination).is_err(),
        "workspace destination is a dangling symlink"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn require_same_apfs(
    source: &Path,
    destination_parent: &Path,
    destination: &Path,
) -> anyhow::Result<()> {
    let source_fs = statfs(source)?;
    let destination_fs = statfs(destination_parent)?;
    let source_type = filesystem_type(&source_fs);
    let destination_type = filesystem_type(&destination_fs);
    if source_type != b"apfs" || destination_type != b"apfs" {
        return Err(CowUnavailable {
            source_path: source.to_owned(),
            destination_path: destination.to_owned(),
            reason: format!(
                "source filesystem is {}, destination filesystem is {}",
                String::from_utf8_lossy(source_type),
                String::from_utf8_lossy(destination_type)
            ),
        }
        .into());
    }
    let same_volume = unsafe {
        libc::memcmp(
            std::ptr::addr_of!(source_fs.f_fsid).cast(),
            std::ptr::addr_of!(destination_fs.f_fsid).cast(),
            std::mem::size_of::<libc::fsid_t>(),
        ) == 0
    };
    if !same_volume {
        return Err(CowUnavailable {
            source_path: source.to_owned(),
            destination_path: destination.to_owned(),
            reason: "source and destination are on different APFS volumes".to_owned(),
        }
        .into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn statfs(path: &Path) -> io::Result<libc::statfs> {
    let path = path_to_cstring(path)?;
    let mut information = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    let result = unsafe { libc::statfs(path.as_ptr(), information.as_mut_ptr()) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { information.assume_init() })
}

#[cfg(target_os = "macos")]
fn filesystem_type(information: &libc::statfs) -> &[u8] {
    unsafe { CStr::from_ptr(information.f_fstypename.as_ptr()) }.to_bytes()
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct ClonePlan {
    operations: Vec<CloneOperation>,
    hardlinks: Vec<HardlinkOperation>,
    directories: Vec<(CString, u32)>,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct CloneOperation {
    relative: CString,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct HardlinkOperation {
    source: CString,
    destination: CString,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct HardlinkSource {
    relative: CString,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct BulkEntry {
    name: CString,
    kind: EntryKind,
    mode: u32,
    device: libc::dev_t,
    file_id: u64,
    link_count: u32,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
}

#[cfg(target_os = "macos")]
const BULK_BUFFER_BYTES: usize = 1024 * 1024;

/// The immutable fast path needs entry kinds, duplicate inode detection and
/// directory modes, but no per-file attributes or clone operations. APFS
/// directory entries provide the first two without fetching every inode's
/// attributes. Unknown types and repeated file inodes use the full inventory.
#[cfg(target_os = "macos")]
#[derive(Default)]
struct ImmutableClonePlan {
    directories: Vec<(CString, u32)>,
    files: HashSet<u64>,
    entries: usize,
}

#[cfg(target_os = "macos")]
impl ImmutableClonePlan {
    fn inventory(&mut self, root: RawFd, device: libc::dev_t) -> anyhow::Result<bool> {
        let mut pending = vec![(PathBuf::new(), None)];
        while let Some((relative, expected_inode)) = pending.pop() {
            let name = if relative.as_os_str().is_empty() {
                c".".to_owned()
            } else {
                path_to_cstring(&relative)?
            };
            // A new open description leaves the root's directory offset
            // untouched if this attempt falls back to getattrlistbulk.
            let descriptor = open_relative_directory(root, &name)?;
            let metadata = directory_stat(descriptor.as_raw_fd())?;
            ensure!(
                metadata.st_dev == device,
                "workspace tree crosses a filesystem boundary"
            );
            if let Some(inode) = expected_inode {
                ensure!(
                    metadata.st_ino == inode,
                    "workspace directory changed during materialization"
                );
                self.directories
                    .push((name, u32::from(metadata.st_mode & 0o777)));
            }
            let mut directory = DirectoryStream::open(descriptor)?;
            while let Some(entry) = directory.next()? {
                let name = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) };
                if matches!(name.to_bytes(), b"." | b"..") {
                    continue;
                }
                self.entries += 1;
                if self.entries > 25_000 || entry.d_ino == 0 {
                    return Ok(false);
                }
                match entry.d_type {
                    libc::DT_REG => {
                        if !self.files.insert(entry.d_ino) {
                            return Ok(false);
                        }
                    }
                    libc::DT_LNK => {}
                    libc::DT_DIR => {
                        pending.push((relative_entry_path(&relative, name), Some(entry.d_ino)));
                    }
                    // The controlled traversal validates unknown types and
                    // rejects unsupported objects before any clone starts.
                    _ => return Ok(false),
                }
            }
        }
        Ok(true)
    }
}

#[cfg(target_os = "macos")]
struct DirectoryStream(std::ptr::NonNull<libc::DIR>);

#[cfg(target_os = "macos")]
impl DirectoryStream {
    fn open(descriptor: OwnedFd) -> io::Result<Self> {
        let pointer = unsafe { libc::fdopendir(descriptor.as_raw_fd()) };
        let pointer = std::ptr::NonNull::new(pointer).ok_or_else(io::Error::last_os_error)?;
        // fdopendir owns the descriptor only after a successful call.
        std::mem::forget(descriptor);
        Ok(Self(pointer))
    }

    fn next(&mut self) -> io::Result<Option<&libc::dirent>> {
        unsafe {
            *libc::__error() = 0;
            let entry = libc::readdir(self.0.as_ptr());
            if entry.is_null() {
                let error = *libc::__error();
                return if error == 0 {
                    Ok(None)
                } else {
                    Err(io::Error::from_raw_os_error(error))
                };
            }
            Ok(Some(&*entry))
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0.as_ptr()) };
    }
}

#[cfg(target_os = "macos")]
fn plan_clone_entries(
    source_root: RawFd,
    relative_directory: &Path,
    root_device: libc::dev_t,
    bulk_buffer: &mut [u64],
    plan: &mut ClonePlan,
    hardlinks: &mut BTreeMap<(libc::dev_t, u64), HardlinkSource>,
) -> anyhow::Result<()> {
    // Reopen each directory from the pinned root and close it before
    // descending. Descriptor use is therefore constant rather than
    // proportional to directory count or depth.
    let directory = if relative_directory.as_os_str().is_empty() {
        duplicate_descriptor(source_root)?
    } else {
        let relative = path_to_cstring(relative_directory)?;
        open_relative_directory(source_root, &relative)?
    };
    let mut entries = bulk_directory_entries(directory.as_raw_fd(), bulk_buffer)?;
    drop(directory);
    entries.sort_unstable_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
    for entry in entries {
        let relative = relative_entry_path(relative_directory, &entry.name);
        ensure!(
            entry.device == root_device,
            "workspace tree crosses a filesystem boundary at {}",
            relative.display()
        );
        match entry.kind {
            EntryKind::Directory => {
                let relative_c = path_to_cstring(&relative)?;
                let source = open_relative_directory(source_root, &relative_c)?;
                let source_stat = directory_stat(source.as_raw_fd())?;
                ensure!(
                    source_stat.st_dev == root_device,
                    "workspace tree crosses a filesystem boundary at {}",
                    relative.display()
                );
                ensure!(
                    (source_stat.st_mode & libc::S_IFMT) == libc::S_IFDIR,
                    "workspace directory changed during materialization at {}",
                    relative.display()
                );
                ensure!(
                    source_stat.st_ino == entry.file_id,
                    "workspace directory changed during materialization at {}",
                    relative.display()
                );
                drop(source);
                plan.directories.push((relative_c, entry.mode));
                plan_clone_entries(
                    source_root,
                    &relative,
                    root_device,
                    bulk_buffer,
                    plan,
                    hardlinks,
                )?;
            }
            EntryKind::File if entry.link_count > 1 => {
                let key = (entry.device, entry.file_id);
                let relative_c = path_to_cstring(&relative)?;
                if let Some(existing) = hardlinks.get(&key) {
                    plan.hardlinks.push(HardlinkOperation {
                        source: existing.relative.clone(),
                        destination: relative_c,
                    });
                } else {
                    plan.operations.push(CloneOperation {
                        relative: relative_c.clone(),
                    });
                    hardlinks.insert(
                        key,
                        HardlinkSource {
                            relative: relative_c,
                        },
                    );
                }
            }
            EntryKind::File | EntryKind::Symlink => {
                plan.operations.push(CloneOperation {
                    relative: path_to_cstring(&relative)?,
                });
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn execute_clone_operations(
    source_descriptor: RawFd,
    destination_descriptor: RawFd,
    source_root: &Path,
    destination_root: &Path,
    operations: &[CloneOperation],
) -> anyhow::Result<()> {
    if operations.is_empty() {
        return Ok(());
    }
    let active = ActiveClone::enter();
    let worker_count = clone_parallelism()
        .div_ceil(active.count)
        .max(1)
        .min(operations.len());
    let failed = AtomicBool::new(false);
    let failure = Mutex::new(None);
    std::thread::scope(|scope| {
        for worker in 0..worker_count {
            let failed = &failed;
            let failure = &failure;
            scope.spawn(move || {
                let _permit = clone_limiter().acquire();
                for index in (worker..operations.len()).step_by(worker_count) {
                    if failed.load(Ordering::Acquire) {
                        return;
                    }
                    let operation = &operations[index];
                    if let Err(error) = clone_one(
                        source_descriptor,
                        destination_descriptor,
                        source_root,
                        destination_root,
                        operation,
                    ) {
                        failed.store(true, Ordering::Release);
                        let mut slot = failure.lock().unwrap_or_else(|poison| poison.into_inner());
                        if slot.is_none() {
                            *slot = Some(error);
                        }
                        break;
                    }
                }
            });
        }
    });
    drop(active);
    match take_clone_failure(&failure) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(target_os = "macos")]
fn clone_one(
    source_descriptor: RawFd,
    destination_descriptor: RawFd,
    source_root: &Path,
    destination_root: &Path,
    operation: &CloneOperation,
) -> anyhow::Result<()> {
    // Directory descriptors pin the validated traversal. BENEATH and
    // NOFOLLOW_ANY prevent resolution outside those directories or through a
    // concurrently substituted symlink; NOFOLLOW clones a final symlink as
    // the symlink itself.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    const CLONE_NOFOLLOW_ANY: u32 = 0x0008;
    const CLONE_RESOLVE_BENEATH: u32 = 0x0010;
    let flags = CLONE_NOFOLLOW | CLONE_NOFOLLOW_ANY | CLONE_RESOLVE_BENEATH;
    let result = unsafe {
        libc::clonefileat(
            source_descriptor,
            operation.relative.as_ptr(),
            destination_descriptor,
            operation.relative.as_ptr(),
            flags,
        )
    };
    if result == -1 {
        let relative = OsStr::from_bytes(operation.relative.to_bytes());
        return Err(CowUnavailable {
            source_path: source_root.join(relative),
            destination_path: destination_root.join(relative),
            reason: io::Error::last_os_error().to_string(),
        }
        .into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn take_clone_failure(failure: &Mutex<Option<anyhow::Error>>) -> Option<anyhow::Error> {
    failure
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take()
}

#[cfg(target_os = "macos")]
fn bulk_directory_entries(directory: RawFd, buffer: &mut [u64]) -> anyhow::Result<Vec<BulkEntry>> {
    const ATTR_CMN_ERROR: libc::attrgroup_t = 0x2000_0000;
    let requested_common = libc::ATTR_CMN_RETURNED_ATTRS
        | ATTR_CMN_ERROR
        | libc::ATTR_CMN_NAME
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_OBJTYPE
        | libc::ATTR_CMN_ACCESSMASK
        | libc::ATTR_CMN_FILEID;
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: requested_common,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_LINKCOUNT,
        forkattr: 0,
    };
    let mut entries = Vec::new();
    loop {
        let count = unsafe {
            libc::getattrlistbulk(
                directory,
                std::ptr::addr_of_mut!(attributes).cast(),
                buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(buffer),
                u64::from(libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_NOFOLLOW),
            )
        };
        if count == -1 {
            return Err(io::Error::last_os_error())
                .context("cannot inventory workspace directory with getattrlistbulk");
        }
        if count == 0 {
            break;
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), std::mem::size_of_val(buffer))
        };
        parse_bulk_entries(bytes, count as usize, requested_common, &mut entries)?;
    }
    Ok(entries)
}

#[cfg(target_os = "macos")]
fn parse_bulk_entries(
    buffer: &[u8],
    count: usize,
    required_common: libc::attrgroup_t,
    entries: &mut Vec<BulkEntry>,
) -> anyhow::Result<()> {
    const ATTR_CMN_ERROR: libc::attrgroup_t = 0x2000_0000;
    let mut record_offset = 0_usize;
    for _ in 0..count {
        let length = read_u32(buffer, record_offset)? as usize;
        ensure!(
            length >= std::mem::size_of::<u32>() + std::mem::size_of::<libc::attribute_set_t>(),
            "getattrlistbulk returned a short record"
        );
        let record_end = record_offset
            .checked_add(length)
            .context("getattrlistbulk record length overflow")?;
        ensure!(
            record_end <= buffer.len(),
            "getattrlistbulk returned a truncated record"
        );
        let record = &buffer[record_offset..record_end];
        let mut offset = std::mem::size_of::<u32>();
        let returned: libc::attribute_set_t = read_value(record, offset)?;
        offset += std::mem::size_of::<libc::attribute_set_t>();

        // FSOPT_PACK_INVAL_ATTRS gives every requested fixed-width field a
        // stable slot. The returned bitmap still tells us whether its value is
        // valid for this entry.
        let entry_error = read_u32(record, offset)?;
        offset += std::mem::size_of::<u32>();
        if returned.commonattr & ATTR_CMN_ERROR != 0 && entry_error != 0 {
            return Err(io::Error::from_raw_os_error(entry_error as i32))
                .context("getattrlistbulk could not inspect a workspace entry");
        }

        let name_reference_offset = offset;
        let name_reference: libc::attrreference_t = read_value(record, offset)?;
        offset += std::mem::size_of::<libc::attrreference_t>();
        let device: libc::dev_t = read_value(record, offset)?;
        offset += std::mem::size_of::<libc::dev_t>();
        let object_type = read_u32(record, offset)?;
        offset += std::mem::size_of::<u32>();
        let mode = read_u32(record, offset)? & 0o777;
        offset += std::mem::size_of::<u32>();
        let file_id: u64 = read_value(record, offset)?;
        offset += std::mem::size_of::<u64>();
        let link_count = read_u32(record, offset)?;

        let required_for_every_entry = required_common & !ATTR_CMN_ERROR;
        ensure!(
            returned.commonattr & required_for_every_entry == required_for_every_entry,
            "APFS did not return required workspace entry attributes"
        );
        let name_start = (name_reference_offset as i64)
            .checked_add(i64::from(name_reference.attr_dataoffset))
            .context("getattrlistbulk name offset overflow")?;
        ensure!(
            name_start >= 0,
            "getattrlistbulk returned an invalid name offset"
        );
        let name_start = name_start as usize;
        let name_end = name_start
            .checked_add(name_reference.attr_length as usize)
            .context("getattrlistbulk name length overflow")?;
        ensure!(
            name_reference.attr_length > 1 && name_end <= record.len(),
            "getattrlistbulk returned an invalid entry name"
        );
        let name = &record[name_start..name_end];
        ensure!(
            name.last() == Some(&0),
            "getattrlistbulk entry name is not NUL terminated"
        );
        let name = CString::new(&name[..name.len() - 1])
            .context("getattrlistbulk entry name contains an embedded NUL")?;
        validate_entry_name(&name)?;

        let kind = match object_type {
            1 => EntryKind::File,
            2 => EntryKind::Directory,
            5 => EntryKind::Symlink,
            _ => anyhow::bail!(
                "unsupported filesystem object in workspace: {}",
                OsStr::from_bytes(name.to_bytes()).to_string_lossy()
            ),
        };
        if matches!(kind, EntryKind::File | EntryKind::Symlink) {
            ensure!(
                returned.fileattr & libc::ATTR_FILE_LINKCOUNT != 0,
                "APFS did not return a workspace entry link count"
            );
            ensure!(link_count > 0, "workspace entry has an invalid link count");
        }
        entries.push(BulkEntry {
            name,
            kind,
            mode,
            device,
            file_id,
            link_count,
        });
        record_offset = record_end;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn validate_entry_name(name: &CStr) -> anyhow::Result<()> {
    let bytes = name.to_bytes();
    ensure!(
        !bytes.is_empty() && bytes != b"." && bytes != b".." && !bytes.contains(&b'/'),
        "workspace traversal returned an unsafe entry name"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_root_directory(path: &Path) -> io::Result<OwnedFd> {
    let path = path_to_cstring(path)?;
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    owned_descriptor(descriptor)
}

#[cfg(target_os = "macos")]
fn open_relative_directory(root: RawFd, relative: &CStr) -> io::Result<OwnedFd> {
    const O_RESOLVE_BENEATH: libc::c_int = 0x0000_1000;
    let descriptor = unsafe {
        libc::openat(
            root,
            relative.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW_ANY
                | O_RESOLVE_BENEATH,
        )
    };
    owned_descriptor(descriptor)
}

#[cfg(target_os = "macos")]
fn duplicate_descriptor(descriptor: RawFd) -> io::Result<OwnedFd> {
    owned_descriptor(unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) })
}

#[cfg(target_os = "macos")]
fn owned_descriptor(descriptor: RawFd) -> io::Result<OwnedFd> {
    if descriptor == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

#[cfg(target_os = "macos")]
fn directory_stat(directory: RawFd) -> io::Result<libc::stat> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(directory, metadata.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { metadata.assume_init() })
}

#[cfg(target_os = "macos")]
fn create_directory_at(parent: RawFd, name: &CStr) -> io::Result<()> {
    if unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_directory_mode(directory: RawFd, mode: u32) -> io::Result<()> {
    if unsafe { libc::fchmod(directory, mode as libc::mode_t) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn create_hardlink(
    destination_descriptor: RawFd,
    source_root: &Path,
    destination_root: &Path,
    operation: &HardlinkOperation,
) -> anyhow::Result<()> {
    const AT_RESOLVE_BENEATH: libc::c_int = 0x2000;
    let result = unsafe {
        libc::linkat(
            destination_descriptor,
            operation.source.as_ptr(),
            destination_descriptor,
            operation.destination.as_ptr(),
            AT_RESOLVE_BENEATH,
        )
    };
    if result == -1 {
        let source = OsStr::from_bytes(operation.source.to_bytes());
        let destination = OsStr::from_bytes(operation.destination.to_bytes());
        return Err(io::Error::last_os_error()).with_context(|| {
            format!(
                "cannot recreate workspace hardlink {} -> {} (source {})",
                destination_root.join(destination).display(),
                destination_root.join(source).display(),
                source_root.join(source).display()
            )
        });
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn relative_entry_path(directory: &Path, name: &CStr) -> PathBuf {
    directory.join(OsStr::from_bytes(name.to_bytes()))
}

#[cfg(target_os = "macos")]
fn clone_parallelism() -> usize {
    // APFS serializes enough directory metadata work that wider fan-out
    // regresses a flat tree sharply. Four independent clone syscalls is the
    // measured sweet spot on the supported Apple Silicon target; the global
    // limiter keeps concurrent materializations inside the same bound.
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(4)
}

#[cfg(target_os = "macos")]
static ACTIVE_CLONES: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "macos")]
struct ActiveClone {
    count: usize,
}

#[cfg(target_os = "macos")]
impl ActiveClone {
    fn enter() -> Self {
        Self {
            count: ACTIVE_CLONES.fetch_add(1, Ordering::AcqRel) + 1,
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for ActiveClone {
    fn drop(&mut self) {
        ACTIVE_CLONES.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(target_os = "macos")]
struct CloneLimiter {
    available: Mutex<usize>,
    wake: Condvar,
}

#[cfg(target_os = "macos")]
impl CloneLimiter {
    fn acquire(&'static self) -> ClonePermit {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while *available == 0 {
            available = self
                .wake
                .wait(available)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        *available -= 1;
        ClonePermit { limiter: self }
    }
}

#[cfg(target_os = "macos")]
struct ClonePermit {
    limiter: &'static CloneLimiter,
}

#[cfg(target_os = "macos")]
impl Drop for ClonePermit {
    fn drop(&mut self) {
        let mut available = self
            .limiter
            .available
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *available += 1;
        self.limiter.wake.notify_one();
    }
}

#[cfg(target_os = "macos")]
fn clone_limiter() -> &'static CloneLimiter {
    static LIMITER: OnceLock<CloneLimiter> = OnceLock::new();
    LIMITER.get_or_init(|| CloneLimiter {
        available: Mutex::new(clone_parallelism()),
        wake: Condvar::new(),
    })
}

#[cfg(any(test, feature = "test-support"))]
fn copy_entries(
    source: &Path,
    destination: &Path,
    hardlinks: &mut BTreeMap<(u64, u64), PathBuf>,
) -> anyhow::Result<()> {
    for entry in sorted_entries(source)? {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&source_path)?, &destination_path)?;
        } else if metadata.is_dir() {
            fs::create_dir(&destination_path)?;
            copy_entries(&source_path, &destination_path, hardlinks)?;
            set_portable_mode(&destination_path, &metadata)?;
        } else if metadata.is_file() {
            let key = (metadata.dev(), metadata.ino());
            if metadata.nlink() > 1
                && let Some(existing) = hardlinks.get(&key)
            {
                fs::hard_link(existing, &destination_path)?;
                continue;
            }
            fs::copy(&source_path, &destination_path)?;
            set_portable_mode(&destination_path, &metadata)?;
            if metadata.nlink() > 1 {
                hardlinks.insert(key, destination_path);
            }
        } else {
            anyhow::bail!(
                "unsupported filesystem object in fake workspace: {}",
                source_path.display()
            );
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn sorted_entries(directory: &Path) -> anyhow::Result<Vec<fs::DirEntry>> {
    let mut entries: Vec<_> = fs::read_dir(directory)?.collect::<Result<_, _>>()?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });
    Ok(entries)
}

#[cfg(any(test, feature = "test-support"))]
fn set_portable_mode(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    // Git worktrees need the executable bits; ownership/set-id bits are not
    // propagated into an agent workspace.
    fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode() & 0o777))
}

fn remove_tree_checked(root: &Path) -> anyhow::Result<()> {
    ensure!(
        root.is_absolute(),
        "workspace tree removal requires an absolute path"
    );
    ensure!(
        root.components()
            .all(|component| !matches!(component, Component::CurDir | Component::ParentDir)),
        "workspace tree removal rejects relative traversal"
    );
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        root.file_name().is_some(),
        "refusing to remove a filesystem root"
    );
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "workspace tree removal requires a real directory"
    );
    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn fake_usage(root: &Path) -> anyhow::Result<Usage> {
    let mut logical = 0_u64;
    let mut seen = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if (metadata.is_file() && seen.insert((metadata.dev(), metadata.ino()), ()).is_none())
            || metadata.file_type().is_symlink()
        {
            logical = logical.saturating_add(metadata.len());
        }
    }
    Ok(Usage {
        logical_bytes: logical,
        referenced_bytes: logical,
        private_bytes: Some(logical),
    })
}

#[cfg(target_os = "macos")]
fn apfs_usage(root: &Path) -> anyhow::Result<Usage> {
    let mut usage = Usage {
        private_bytes: Some(0),
        ..Usage::default()
    };
    let mut seen = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !(metadata.is_file() || metadata.file_type().is_symlink()) {
            continue;
        }
        if metadata.is_file() && seen.insert((metadata.dev(), metadata.ino()), ()).is_some() {
            continue;
        }
        let sizes = file_sizes(entry.path())?;
        usage.logical_bytes = usage.logical_bytes.saturating_add(sizes.logical);
        usage.referenced_bytes = usage.referenced_bytes.saturating_add(sizes.referenced);
        usage.private_bytes = match (usage.private_bytes, sizes.private) {
            (Some(total), Some(value)) => Some(total.saturating_add(value)),
            _ => None,
        };
    }
    Ok(usage)
}

#[cfg(not(target_os = "macos"))]
fn apfs_usage(_root: &Path) -> anyhow::Result<Usage> {
    anyhow::bail!("COW_UNAVAILABLE: APFS usage accounting requires macOS")
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct FileSizes {
    logical: u64,
    referenced: u64,
    private: Option<u64>,
}

#[cfg(target_os = "macos")]
fn file_sizes(path: &Path) -> anyhow::Result<FileSizes> {
    match query_file_sizes(path, true) {
        Ok(sizes) => Ok(sizes),
        Err(error)
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::EINVAL || code == libc::ENOTSUP) =>
        {
            query_file_sizes(path, false).map_err(Into::into)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "macos")]
fn query_file_sizes(path: &Path, request_private: bool) -> io::Result<FileSizes> {
    let path = path_to_cstring(path)?;
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_TOTALSIZE | libc::ATTR_FILE_ALLOCSIZE,
        forkattr: if request_private {
            libc::ATTR_CMNEXT_PRIVATESIZE
        } else {
            0
        },
    };
    let mut buffer = [0_u8; 64];
    let mut options = libc::FSOPT_NOFOLLOW | libc::FSOPT_REPORT_FULLSIZE;
    if request_private {
        options |= libc::FSOPT_ATTR_CMN_EXTENDED;
    }
    let result = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            std::ptr::addr_of_mut!(attributes).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            options,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    let returned_length = read_u32(&buffer, 0)? as usize;
    if returned_length > buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "getattrlist response was truncated",
        ));
    }
    let returned: libc::attribute_set_t = read_value(&buffer, 4)?;
    let mut offset = 4 + std::mem::size_of::<libc::attribute_set_t>();
    let logical = if returned.fileattr & libc::ATTR_FILE_TOTALSIZE != 0 {
        let value: i64 = read_value(&buffer, offset)?;
        offset += std::mem::size_of::<i64>();
        value.max(0) as u64
    } else {
        0
    };
    let referenced = if returned.fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
        let value: i64 = read_value(&buffer, offset)?;
        offset += std::mem::size_of::<i64>();
        value.max(0) as u64
    } else {
        0
    };
    let private = if request_private && returned.forkattr & libc::ATTR_CMNEXT_PRIVATESIZE != 0 {
        let value: i64 = read_value(&buffer, offset)?;
        Some(value.max(0) as u64)
    } else {
        None
    };
    Ok(FileSizes {
        logical,
        referenced,
        private,
    })
}

#[cfg(target_os = "macos")]
fn read_u32(buffer: &[u8], offset: usize) -> io::Result<u32> {
    read_value(buffer, offset)
}

#[cfg(target_os = "macos")]
fn read_value<T: Copy>(buffer: &[u8], offset: usize) -> io::Result<T> {
    if offset + std::mem::size_of::<T>() > buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short getattrlist response",
        ));
    }
    Ok(unsafe { std::ptr::read_unaligned(buffer.as_ptr().add(offset).cast::<T>()) })
}

#[cfg(target_os = "macos")]
fn path_to_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "filesystem path contains a NUL byte",
        )
    })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn immutable_inventory_caps_all_entries_and_preserves_the_fallback_offset() {
        let temporary = tempfile::tempdir().unwrap();
        for index in 0..25_000 {
            File::create(temporary.path().join(format!("file-{index:05}"))).unwrap();
        }
        let root = open_root_directory(temporary.path()).unwrap();
        let device = directory_stat(root.as_raw_fd()).unwrap().st_dev;
        let mut at_limit = ImmutableClonePlan::default();
        assert!(at_limit.inventory(root.as_raw_fd(), device).unwrap());

        // Directories also consume the kernel traversal budget. Falling back
        // must inventory the entire root, including entries already scanned.
        fs::create_dir(temporary.path().join("last-directory")).unwrap();
        let mut over_limit = ImmutableClonePlan::default();
        assert!(!over_limit.inventory(root.as_raw_fd(), device).unwrap());
        let mut plan = ClonePlan::default();
        let mut links = BTreeMap::new();
        let mut buffer = vec![0_u64; BULK_BUFFER_BYTES / std::mem::size_of::<u64>()];
        plan_clone_entries(
            root.as_raw_fd(),
            Path::new(""),
            device,
            &mut buffer,
            &mut plan,
            &mut links,
        )
        .unwrap();
        assert_eq!(plan.operations.len(), 25_000);
        assert_eq!(plan.directories.len(), 1);
    }
}
