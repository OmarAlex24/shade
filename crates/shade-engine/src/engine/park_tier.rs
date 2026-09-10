//! Where the engine touches the parked tier.
//!
//! [`crate::park`] knows how to move bytes between a workspace and an external
//! volume and nothing else. This module is the other half: it decides *when*
//! that is worth doing, records what happened so the collector can reason
//! about it without walking a disk that may be unplugged, and turns every
//! failure into a sentence rather than an error.
//!
//! Nothing here may fail an operation. Sleep still sleeps and wake still
//! wakes when the volume is missing, full, or holding a park that describes
//! some other tree -- build output is, by definition, the one thing a build
//! can produce again.
//!
//! Every `park` call is a synchronous multi-gigabyte copy, so all of them run
//! on `spawn_blocking` and none of them run on the async runtime's threads.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;
use shade_protocol::{CheckpointId, WorkspaceId};

use super::{Engine, EngineError};
use crate::db::{CheckpointRecord, ParkRecord, WorkspaceRecord};
use crate::park;

/// What a sleep did with the workspace's private build output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParkReport {
    pub parked: bool,
    pub bytes: u64,
    pub path: Option<String>,
    pub reason: Option<String>,
}

impl ParkReport {
    fn declined(reason: &str) -> Self {
        Self {
            parked: false,
            bytes: 0,
            path: None,
            reason: Some(reason.to_owned()),
        }
    }
}

/// What a wake got back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RestoreReport {
    pub restored: bool,
    pub bytes: u64,
    pub reason: Option<String>,
    /// Whether a park record named this checkpoint at all. A wake of a
    /// workspace that never parked is not a wake that lost anything.
    pub found: bool,
}

impl RestoreReport {
    fn absent() -> Self {
        Self {
            restored: false,
            bytes: 0,
            reason: None,
            found: false,
        }
    }

    fn declined(reason: &str) -> Self {
        Self {
            restored: false,
            bytes: 0,
            reason: Some(reason.to_owned()),
            found: true,
        }
    }

    /// The one line a caller needs, and only in the case that earns it: a park
    /// was recorded for this checkpoint and its bytes did not come back, so
    /// the successor is a correct tree that will still take a full build.
    pub fn next(&self) -> Option<String> {
        if self.restored || !self.found {
            return None;
        }
        let reason = self.reason.as_deref().unwrap_or("unknown");
        Some(format!(
            "build output not restored ({reason}); it will regenerate on the next build"
        ))
    }
}

/// What one collector pass did to the parks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ParkSweep {
    /// Garbage parks removed from the volume and from the database.
    pub deleted: u64,
    /// Garbage parks left alone because the volume is not mounted.
    pub retained: u64,
    /// Directories on the volume that no record claims.
    pub orphans: u64,
    /// Records whose directory is gone from a mounted volume.
    pub dropped: u64,
}

/// Copy a workspace's private build output to the park volume, if that is
/// worth doing, and record it.
///
/// Never returns an error: a sleep that could not park is a sleep, and the
/// reason travels in the result instead.
pub(super) async fn park_on_sleep(
    engine: &Engine,
    workspace: &WorkspaceRecord,
    checkpoint: &CheckpointRecord,
    private_bytes: u64,
) -> ParkReport {
    let Some(root) = engine.config.park_root().map(Path::to_path_buf) else {
        return ParkReport::declined("unconfigured");
    };
    if !engine.config.park_root_mounted() {
        return ParkReport::declined("unmounted");
    }
    if private_bytes < engine.config.park_min_bytes() {
        return ParkReport::declined("below_min_bytes");
    }
    match park_and_record(engine, &root, workspace, checkpoint).await {
        Ok(report) => report,
        Err(error) => {
            tracing::warn!(
                workspace = %workspace.id,
                checkpoint = %checkpoint.id,
                code = %error.code,
                "sleep could not park the private build output"
            );
            ParkReport::declined("park_failed")
        }
    }
}

/// The copy out, with every borrowed argument marshalled into the owned values
/// a blocking task needs.
async fn copy_out(
    root: &Path,
    workspace: &WorkspaceRecord,
    checkpoint: &CheckpointRecord,
    excluded: Vec<PathBuf>,
) -> Result<park::ParkOutcome, EngineError> {
    let (root, tree) = (root.to_path_buf(), workspace.path.clone());
    let (workspace_id, checkpoint_id) = (workspace.id.0.clone(), checkpoint.id.0.clone());
    let (head, worktree) = (
        checkpoint.head_oid.0.clone(),
        checkpoint.worktree_oid.0.clone(),
    );
    blocking("park", move || {
        park::park_tree(
            &root,
            &tree,
            &workspace_id,
            &checkpoint_id,
            &head,
            &worktree,
            &excluded,
        )
    })
    .await
}

async fn park_and_record(
    engine: &Engine,
    root: &Path,
    workspace: &WorkspaceRecord,
    checkpoint: &CheckpointRecord,
) -> Result<ParkReport, EngineError> {
    let excluded = excluded_relative_paths(engine, &workspace.id)?;
    let outcome = copy_out(root, workspace, checkpoint, excluded).await?;
    let record = ParkRecord {
        workspace_id: workspace.id.clone(),
        checkpoint_id: checkpoint.id.clone(),
        park_path: outcome.park_path.clone(),
        bytes: outcome.bytes,
        created_at_ms: crate::db::now_ms(),
    };
    if let Err(error) = engine.database.record_park(&record) {
        // The bytes are on the volume with nothing pointing at them. Take them
        // back while the volume is still here rather than leave a sweep to
        // discover them as an orphan later.
        tracing::warn!(
            workspace = %workspace.id,
            checkpoint = %checkpoint.id,
            error = %error,
            "parked build output could not be recorded and was removed again"
        );
        let _ = remove(root, &workspace.id, &checkpoint.id).await;
        return Ok(ParkReport::declined("not_recorded"));
    }
    crate::faults::hit(crate::faults::Point::SleepParked);
    engine.injected_failure(crate::faults::Point::SleepParked)?;
    Ok(ParkReport {
        parked: true,
        bytes: outcome.bytes,
        path: Some(outcome.park_path.to_string_lossy().into_owned()),
        reason: None,
    })
}

/// Copy a park back into a freshly materialized successor, if one was recorded
/// for this suspension checkpoint.
///
/// Never returns an error, for the same reason [`park_on_sleep`] does not: a
/// wake with the volume unplugged is an ordinary wake.
pub(super) async fn restore_on_wake(
    engine: &Engine,
    parked_by: &WorkspaceId,
    checkpoint: &CheckpointRecord,
    successor_path: &Path,
) -> RestoreReport {
    match engine.database.park(parked_by, &checkpoint.id) {
        Ok(Some(_)) => {}
        Ok(None) => return RestoreReport::absent(),
        Err(error) => {
            tracing::warn!(
                workspace = %parked_by,
                error = %error,
                "could not read the park record; waking without it"
            );
            return RestoreReport::absent();
        }
    }
    let Some(root) = engine.config.park_root().map(Path::to_path_buf) else {
        return RestoreReport::declined("unconfigured");
    };
    if !engine.config.park_root_mounted() {
        return RestoreReport::declined("unmounted");
    }
    match restore_and_forget(engine, &root, parked_by, checkpoint, successor_path).await {
        Ok(report) => report,
        Err(error) => {
            tracing::warn!(
                workspace = %parked_by,
                checkpoint = %checkpoint.id,
                code = %error.code,
                "wake could not restore the parked build output"
            );
            RestoreReport::declined("park_failed")
        }
    }
}

/// The copy back, marshalled the same way as [`copy_out`].
async fn copy_in(
    root: &Path,
    parked_by: &WorkspaceId,
    checkpoint: &CheckpointRecord,
    successor_path: &Path,
) -> Result<park::RestoreOutcome, EngineError> {
    let (root, tree) = (root.to_path_buf(), successor_path.to_path_buf());
    let (workspace_id, checkpoint_id) = (parked_by.0.clone(), checkpoint.id.0.clone());
    let (head, worktree) = (
        checkpoint.head_oid.0.clone(),
        checkpoint.worktree_oid.0.clone(),
    );
    blocking("park restore", move || {
        park::restore_park(
            &root,
            &tree,
            &workspace_id,
            &checkpoint_id,
            &head,
            &worktree,
        )
    })
    .await
}

async fn restore_and_forget(
    engine: &Engine,
    root: &Path,
    parked_by: &WorkspaceId,
    checkpoint: &CheckpointRecord,
    successor_path: &Path,
) -> Result<RestoreReport, EngineError> {
    let outcome = copy_in(root, parked_by, checkpoint, successor_path).await?;
    if !outcome.restored {
        return Ok(RestoreReport::declined(
            outcome.reason.as_deref().unwrap_or("absent"),
        ));
    }
    crate::faults::hit(crate::faults::Point::WakeParkRestored);
    engine.injected_failure(crate::faults::Point::WakeParkRestored)?;
    // The park belongs to a checkpoint that stops being a suspension
    // checkpoint the moment this successor is activated, so it is spent.
    if !engine.config.park_keep_after_wake() {
        remove(root, parked_by, &checkpoint.id).await?;
        engine.database.delete_park(parked_by, &checkpoint.id)?;
    }
    Ok(RestoreReport {
        restored: true,
        bytes: outcome.bytes,
        reason: None,
        found: true,
    })
}

/// Reconcile the parks with the records that claim them.
///
/// Removal only ever happens on a mounted volume. An unplugged disk is not
/// evidence that a park is gone, and deleting the record for one would strand
/// the bytes with nothing left that knows their name.
pub(super) async fn sweep_parks(engine: &Engine) -> Result<ParkSweep, EngineError> {
    let Some(root) = engine.config.park_root().map(Path::to_path_buf) else {
        return Ok(ParkSweep::default());
    };
    let mounted = engine.config.park_root_mounted();
    let mut sweep = ParkSweep::default();
    let mut claimed = BTreeSet::new();
    for record in engine.database.parks()? {
        let key = (
            record.workspace_id.0.clone(),
            record.checkpoint_id.0.clone(),
        );
        match verdict(engine, &record, &root, mounted)? {
            Verdict::Keep => {
                claimed.insert(key);
            }
            Verdict::Retain => {
                claimed.insert(key);
                sweep.retained += 1;
            }
            Verdict::Drop => {
                engine
                    .database
                    .delete_park(&record.workspace_id, &record.checkpoint_id)?;
                sweep.dropped += 1;
            }
            Verdict::Collect => {
                if collect_one(engine, &root, &record).await? {
                    sweep.deleted += 1;
                } else {
                    claimed.insert(key);
                    sweep.retained += 1;
                }
            }
        }
    }
    if mounted {
        sweep.orphans = collect_unclaimed(&root, &claimed).await;
    }
    Ok(sweep)
}

/// Take one garbage park off a mounted volume, reporting whether the record
/// went with it. A removal that fails leaves both for the next pass rather
/// than forgetting bytes that are still there.
async fn collect_one(
    engine: &Engine,
    root: &Path,
    record: &ParkRecord,
) -> Result<bool, EngineError> {
    if let Err(error) = remove(root, &record.workspace_id, &record.checkpoint_id).await {
        tracing::warn!(
            workspace = %record.workspace_id,
            code = %error.code,
            "could not remove a garbage park; leaving the record"
        );
        return Ok(false);
    }
    engine
        .database
        .delete_park(&record.workspace_id, &record.checkpoint_id)?;
    Ok(true)
}

enum Verdict {
    /// A live park of a live suspension.
    Keep,
    /// Garbage, but the volume is not here to take it from.
    Retain,
    /// The record outlived its directory.
    Drop,
    /// Garbage, on a volume that is here.
    Collect,
}

fn verdict(
    engine: &Engine,
    record: &ParkRecord,
    root: &Path,
    mounted: bool,
) -> Result<Verdict, EngineError> {
    if is_garbage(engine, record)? {
        return Ok(if mounted {
            Verdict::Collect
        } else {
            Verdict::Retain
        });
    }
    let present = park::park_dir(root, &record.workspace_id.0, &record.checkpoint_id.0).is_dir();
    Ok(if mounted && !present {
        Verdict::Drop
    } else {
        Verdict::Keep
    })
}

/// A park is garbage once it stops describing a live suspension: its workspace
/// went or was released, or a later sleep gave that workspace a different
/// suspension checkpoint.
fn is_garbage(engine: &Engine, record: &ParkRecord) -> Result<bool, EngineError> {
    let Some(workspace) = engine.database.workspace(&record.workspace_id)? else {
        return Ok(true);
    };
    if matches!(
        workspace.state.as_str(),
        "released" | "deleted" | "deleting"
    ) {
        return Ok(true);
    }
    Ok(engine
        .database
        .suspension_checkpoint(&workspace.id)?
        .is_none_or(|checkpoint| checkpoint.id != record.checkpoint_id))
}

/// Remove every park directory on the volume that no record claims.
///
/// The volume can be unplugged between the check and the walk, so a failure
/// here reports nothing collected rather than failing the collector: this pass
/// is housekeeping on someone else's disk, and the rest of GC is not.
async fn collect_unclaimed(root: &Path, claimed: &BTreeSet<(String, String)>) -> u64 {
    let listed = {
        let root = root.to_path_buf();
        match blocking("park inventory", move || park::list_parks(&root)).await {
            Ok(listed) => listed,
            Err(error) => {
                tracing::warn!(code = %error.code, "could not inventory the park volume");
                return 0;
            }
        }
    };
    let mut removed = 0;
    for summary in listed {
        let key = (summary.workspace_id.clone(), summary.checkpoint_id.clone());
        if claimed.contains(&key) {
            continue;
        }
        let (root, workspace, checkpoint) = (
            root.to_path_buf(),
            summary.workspace_id,
            summary.checkpoint_id,
        );
        match blocking("park removal", move || {
            park::remove_park(&root, &workspace, &checkpoint)
        })
        .await
        {
            Ok(()) => removed += 1,
            Err(error) => tracing::warn!(
                code = %error.code,
                "could not remove an unclaimed park directory"
            ),
        }
    }
    removed
}

async fn remove(
    root: &Path,
    workspace: &WorkspaceId,
    checkpoint: &CheckpointId,
) -> Result<(), EngineError> {
    let (root, workspace, checkpoint) = (
        root.to_path_buf(),
        workspace.0.clone(),
        checkpoint.0.clone(),
    );
    blocking("park removal", move || {
        park::remove_park(&root, &workspace, &checkpoint)
    })
    .await
}

/// What must not be parked because something else already owns it.
///
/// The dependency layer refills every path in a receipt from the shared cache
/// on the way out of `wake`, so parking those would copy a dependency forest
/// across a slow volume to restore what is already there.
fn excluded_relative_paths(
    engine: &Engine,
    workspace: &WorkspaceId,
) -> Result<Vec<PathBuf>, EngineError> {
    let mut excluded = park::default_excluded_relative_paths();
    for receipt in engine.database.dependency_receipts(workspace)? {
        let Some(paths) = receipt.get("materialized_paths").and_then(Value::as_array) else {
            continue;
        };
        excluded.extend(paths.iter().filter_map(Value::as_str).map(PathBuf::from));
    }
    excluded.sort();
    excluded.dedup();
    Ok(excluded)
}

/// Run one synchronous park operation off the runtime's threads.
async fn blocking<T, F>(what: &'static str, task: F) -> Result<T, EngineError>
where
    F: FnOnce() -> Result<T, EngineError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|error| EngineError::park(format!("the {what} task failed to join: {error}")))?
}
