//! Crash-test rendezvous at durable saga boundaries. The distribution build
//! has no environment lookup, filesystem access, or signal handling here.

macro_rules! points {
    ($($point:ident),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Point { $($point),+ }

        impl Point {
            #[cfg(feature = "fault-injection")]
            pub const ALL: &'static [Self] = &[$(Self::$point),+];

            pub const fn name(self) -> &'static str {
                match self { $(Self::$point => stringify!($point)),+ }
            }
        }
    };
}

points! {
    OperationRecorded,
    OperationDispatched,
    OperationCompleted,
    DiagnosticWritten,
    OperationFailureWritten,
    OperationFailed,
    RepositoryStaged,
    RepositoryPromoted,
    RepositoryRecorded,
    BaseStaged,
    BasePromoted,
    IncrementalBaseIndexStaged,
    IncrementalBaseIndexWritten,
    IncrementalBaseIndexRefreshed,
    IncrementalBaseTreeUpdated,
    IncrementalBaseTreeVerified,
    IncrementalBasePromoted,
    WorkspaceRecorded,
    WorkspaceBound,
    WorkspaceCloned,
    WorktreeRegistered,
    WorktreePointerMoved,
    WorktreeRepaired,
    WorktreeIndexWritten,
    DependenciesRecorded,
    DependencyStaged,
    DependencyFilled,
    DependencyFillValidated,
    DependencyReplayed,
    DependencyReplayValidated,
    DependencyScriptsExecuted,
    DependencyPromotionStaged,
    DependencyPayloadMoved,
    DependencyReceiptWritten,
    DependencyPromoted,
    DependencyCloneStaged,
    DependencyExistingStaged,
    DependencyMaterialized,
    DependencyBackupRemoved,
    DependencyReceiptRecorded,
    DependencyGcRenamed,
    DependencyGcPayloadRemoved,
    DependencyGcDeleted,
    DependencyInvalidated,
    PythonFillBootstrapCreated,
    PythonFillBaselineCaptured,
    PythonReplayBootstrapCreated,
    PythonReplayBaselineCaptured,
    PythonReplayEntryRelocated,
    PythonReplayRelocated,
    PythonWorkspaceEntrySpecialized,
    PythonWorkspaceSpecialized,
    PythonForkEntryRelocated,
    PythonForkRelocated,
    CargoCachedReplayValidated,
    GoProbeCloned,
    GoProbeValidated,
    GoGraphValidated,
    GoOnlineDownloaded,
    GoOnlineVerified,
    GoOfflineDownloaded,
    GoOfflineVerified,
    GoCachedReplayValidated,
    ScriptDecisionWritten,
    ScriptDecisionCompleted,
    SecretsCaptured,
    SessionActivated,
    CheckpointObjectsWritten,
    CheckpointAnchored,
    CheckpointRecorded,
    CheckpointHeadRecorded,
    FetchQuarantined,
    FetchPromoted,
    CloneStaged,
    ClonePromoted,
    ForkRecorded,
    ForkCloned,
    ForkRestored,
    ForkSecretsCaptured,
    ForkActivated,
    RestoreCleaned,
    RestoreWorkingWritten,
    RestoreHeadWritten,
    RestoreIndexWritten,
    SuccessorRecorded,
    SuccessorRestored,
    HandoffPrepared,
    HandoffAdopted,
    SyncRecorded,
    SyncIntegrated,
    SyncConflictRecorded,
    PublishPlanned,
    PublishAnchored,
    PublishPrepared,
    PublishLocalApplied,
    PublishLocalRecorded,
    PublishRemoteApplied,
    PublishRemoteRecorded,
    PublishCompleted,
    PublishAnchorDeleted,
    PublishAnchorCleaned,
    PublishResolutionRecorded,
    PublishResolutionIntegrated,
    PublishResolutionStateWritten,
    PublishResolutionIntentWritten,
    PublishResolutionReady,
    ReleaseLeaseReleased,
    ReleaseRecorded,
    SleepCheckpointed,
    SleepSecretsVaulted,
    SleepRegistrationRemoved,
    SleepRecorded,
    WakeMaterialized,
    WakeActivated,
    GcClaimed,
    GcTreeRemoved,
    GcRefsRemoved,
    GcSecretsRemoved,
    GcRecordDeleted,
    ReviewSnapshotStaged,
    ReviewSnapshotPromoted,
    ReviewCreated,
    ReviewDecisionWritten,
    ReviewDecisionCompleted,
    SecretsMergeApplied,
    SecretBaselineRemoved,
    SecretBaselineCaptured,
    SecretHandoffWritten,
    SecretHandoffCompleted,
    ReconcileLeasesExpired,
    ReconcilePublishesRecovered,
    ReconcileOperationsWritten,
    ReconcileOperationsRecovered,
    ReconcileWorkspaceClaimed,
    ReconcileWorktreeUnlocked,
    ReconcileRegistrationRemoved,
    ReconcileWorkspaceRefsRemoved,
    ReconcileWorkspaceTreeRemoved,
    ReconcileSecretsRemoved,
    ReconcileWorkspaceDeleted,
    ReconcileCheckpointRefsRemoved,
    ReconcileStagingRemoved,
    ReconcilePublishHandoffPrepared,
    ReconcileCompleted,
}

#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub fn hit(_point: Point) {}

#[cfg(feature = "fault-injection")]
pub fn hit(point: Point) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    static CONFIG: OnceLock<Option<(String, PathBuf)>> = OnceLock::new();
    static FIRED: AtomicBool = AtomicBool::new(false);
    let Some((selected, directory)) = CONFIG.get_or_init(|| {
        Some((
            std::env::var("SHADE_FAULT_POINT").ok()?,
            PathBuf::from(std::env::var_os("SHADE_FAULT_DIR")?),
        ))
    }) else {
        return;
    };
    if selected != point.name()
        || !directory.is_absolute()
        || !directory.join("arm").is_file()
        || FIRED.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("reached"))
        .expect("fault harness rendezvous must be writable");
    marker.write_all(point.name().as_bytes()).unwrap();
    marker.sync_all().unwrap();
    // Freeze every thread, including the maintenance loop. The parent harness
    // must observe the marker, SIGKILL this process, and restart the same pool.
    unsafe { libc::raise(libc::SIGSTOP) };
}
