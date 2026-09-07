//! Development-only Shade V1 hardware release gate.

use anyhow::{Context, ensure};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use shade_client::{ClientError, ShadeClient, ShadeClientOptions, TerminalOutcome};
use shade_engine::filesystem::{
    APFS_IMMUTABLE_CLONE_STRATEGY, ApfsFilesystem, Usage, WorkspaceFilesystem,
};
use shade_protocol::{
    Actor, ActorKind, CompactChanges, CompactContext, DependencyContext, ObjectId, OpenSession,
    OperationId, Outcome, RepositoryLocator, ResponseBody, SessionId, ShadeError, WireResponse,
    WorkspaceId, WorkspaceSelector,
};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;

const RELEASE_ENTRIES: usize = 25_000;
const RELEASE_WORKSPACES: usize = 20;
const RELEASE_LATENCY_SAMPLES: usize = 100;
const RELEASE_PAYLOAD_BYTES: usize = 1_024;
const SMOKE_ENTRIES: usize = 64;
const SMOKE_WORKSPACES: usize = 3;
const SMOKE_LATENCY_SAMPLES: usize = 5;
const SMOKE_PAYLOAD_BYTES: usize = 256;

const MATERIALIZATION_P50_LIMIT_US: u128 = 300_000;
const MATERIALIZATION_P95_LIMIT_US: u128 = 1_000_000;
const CONTEXT_P95_LIMIT_US: u128 = 200_000;
const CLI_IPC_P95_LIMIT_US: u128 = 10_000;
const SPACE_RATIO_MINIMUM: u64 = 5;
const COMMON_RESPONSE_LIMIT_BYTES: usize = 512;
const GATE_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const GATE_OPERATION_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(60);
const POOL_CLEANUP_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(25),
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(1),
];

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GateMode {
    Smoke,
    Release,
}

#[derive(Debug, Parser)]
#[command(
    name = "shade-release-gate",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Args {
    #[arg(long, value_enum, default_value = "smoke")]
    mode: GateMode,
    /// APFS directory in which the isolated benchmark pool is created.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Number of regular files in the immutable source tree.
    #[arg(long)]
    entries: Option<usize>,
    /// Number of unchanged workspaces. Release evidence requires at least 20.
    #[arg(long)]
    workspaces: Option<usize>,
    /// Samples for the context and CLI/IPC latency distributions.
    #[arg(long)]
    latency_samples: Option<usize>,
    /// Deterministic, incompressible bytes written to every source file.
    #[arg(long)]
    payload_bytes: Option<usize>,
    /// Release-built Shade binary used for the real daemon/CLI measurements.
    #[arg(long)]
    shade_bin: Option<PathBuf>,
    /// Atomically write the same minified JSON emitted on stdout.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
struct BenchmarkConfig {
    entries: usize,
    workspaces: usize,
    latency_samples: usize,
    payload_bytes: usize,
}

impl BenchmarkConfig {
    fn from_args(args: &Args) -> anyhow::Result<Self> {
        let defaults = match args.mode {
            GateMode::Release => (
                RELEASE_ENTRIES,
                RELEASE_WORKSPACES,
                RELEASE_LATENCY_SAMPLES,
                RELEASE_PAYLOAD_BYTES,
            ),
            GateMode::Smoke => (
                SMOKE_ENTRIES,
                SMOKE_WORKSPACES,
                SMOKE_LATENCY_SAMPLES,
                SMOKE_PAYLOAD_BYTES,
            ),
        };
        let config = Self {
            entries: args.entries.unwrap_or(defaults.0),
            workspaces: args.workspaces.unwrap_or(defaults.1),
            latency_samples: args.latency_samples.unwrap_or(defaults.2),
            payload_bytes: args.payload_bytes.unwrap_or(defaults.3),
        };
        ensure!(
            (1..=RELEASE_ENTRIES).contains(&config.entries),
            "entry count must be between 1 and {RELEASE_ENTRIES}"
        );
        ensure!(
            (1..=100).contains(&config.workspaces),
            "workspace count must be between 1 and 100"
        );
        ensure!(
            (1..=10_000).contains(&config.latency_samples),
            "latency sample count must be between 1 and 10000"
        );
        ensure!(
            (1..=1_048_576).contains(&config.payload_bytes),
            "payload size must be between 1 byte and 1 MiB"
        );
        if args.mode == GateMode::Release {
            ensure!(
                config.entries == RELEASE_ENTRIES
                    && config.workspaces == RELEASE_WORKSPACES
                    && config.latency_samples == RELEASE_LATENCY_SAMPLES
                    && config.payload_bytes == RELEASE_PAYLOAD_BYTES,
                "release evidence requires exactly {RELEASE_ENTRIES} entries, \
                 {RELEASE_WORKSPACES} workspaces, {RELEASE_LATENCY_SAMPLES} latency samples \
                 and {RELEASE_PAYLOAD_BYTES} payload bytes"
            );
        }
        Ok(config)
    }
}

#[derive(Debug, Serialize)]
struct EnvironmentEvidence {
    target_os: &'static str,
    target_arch: &'static str,
    filesystem: String,
    macos_version: Option<String>,
    kernel: Option<String>,
    hardware_model: Option<String>,
    chip: Option<String>,
    git: Option<String>,
    rustc: Option<String>,
    shade: String,
    shade_binary_sha256: Option<String>,
    gate: String,
    gate_binary_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct UsageEvidence {
    logical_bytes: u64,
    referenced_bytes: u64,
    private_reclaimable_bytes: Option<u64>,
}

impl From<Usage> for UsageEvidence {
    fn from(value: Usage) -> Self {
        Self {
            logical_bytes: value.logical_bytes,
            referenced_bytes: value.referenced_bytes,
            private_reclaimable_bytes: value.private_bytes,
        }
    }
}

#[derive(Debug, Serialize)]
struct LatencyEvidence {
    samples: usize,
    samples_ms: Vec<f64>,
    p50_ms: f64,
    p95_ms: f64,
    p50_limit_ms: Option<f64>,
    p95_limit_ms: f64,
    within_threshold: bool,
}

impl LatencyEvidence {
    fn new(samples_us: Vec<u128>, p50_limit_us: Option<u128>, p95_limit_us: u128) -> Self {
        let p50 = percentile(&samples_us, 50);
        let p95 = percentile(&samples_us, 95);
        let within_threshold = p50_limit_us.is_none_or(|limit| p50 < limit) && p95 < p95_limit_us;
        Self {
            samples: samples_us.len(),
            samples_ms: samples_us.into_iter().map(millis).collect(),
            p50_ms: millis(p50),
            p95_ms: millis(p95),
            p50_limit_ms: p50_limit_us.map(millis),
            p95_limit_ms: millis(p95_limit_us),
            within_threshold,
        }
    }
}

#[derive(Debug, Serialize)]
struct SpaceEvidence {
    method: &'static str,
    source: UsageEvidence,
    cow_workspaces: UsageEvidence,
    full_copy_workspaces: UsageEvidence,
    minimum_ratio: u64,
    observed_private_ratio: Option<f64>,
    within_threshold: bool,
}

#[derive(Debug, Serialize)]
struct MaterializationEvidence {
    method: &'static str,
    state: &'static str,
    entry_count: usize,
    workspace_count: usize,
    latency: LatencyEvidence,
}

#[derive(Debug, Serialize)]
struct ContextEvidence {
    method: &'static str,
    tracked_entry_count: usize,
    latency: LatencyEvidence,
}

#[derive(Debug, Serialize)]
struct CliIpcEvidence {
    method: &'static str,
    latency: LatencyEvidence,
}

#[derive(Debug, Serialize)]
struct ResponseSizeSample {
    name: &'static str,
    source: &'static str,
    bytes: usize,
}

#[derive(Debug, Serialize)]
struct ResponseBudgetEvidence {
    method: &'static str,
    limit_bytes: usize,
    max_bytes: usize,
    samples: Vec<ResponseSizeSample>,
    within_threshold: bool,
}

#[derive(Debug, Serialize)]
struct GateFailure {
    code: &'static str,
    metric: &'static str,
    observed: String,
    required: &'static str,
}

#[derive(Debug, Serialize)]
struct GateReport {
    schema_version: u8,
    status: &'static str,
    evidence: &'static str,
    mode: GateMode,
    thresholds_enforced: bool,
    measured_at_unix_ms: u128,
    environment: EnvironmentEvidence,
    configuration: BenchmarkConfig,
    materialization: MaterializationEvidence,
    space: SpaceEvidence,
    context: ContextEvidence,
    cli_ipc: CliIpcEvidence,
    common_response_budget: ResponseBudgetEvidence,
    failures: Vec<GateFailure>,
}

#[derive(Debug, Serialize)]
struct ErrorReport<'a> {
    schema_version: u8,
    status: &'static str,
    evidence: &'static str,
    mode: Option<GateMode>,
    error: ErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: &'static str,
    message: &'a str,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(_) => {
            emit_json(&ErrorReport {
                schema_version: 1,
                status: "failed",
                evidence: "shade_v1_release_gate",
                mode: None,
                error: ErrorBody {
                    code: "INVALID_ARGUMENT",
                    message: "inspect docs/RELEASE.md and correct the benchmark arguments",
                },
            });
            std::process::exit(64);
        }
    };
    let output = args.output.clone();
    let mode = args.mode;
    match run_gate(&args).await {
        Ok(report) => {
            if let Err(error) = emit_and_store(&report, output.as_deref()) {
                emit_json(&ErrorReport {
                    schema_version: 1,
                    status: "failed",
                    evidence: "shade_v1_release_gate",
                    mode: Some(mode),
                    error: ErrorBody {
                        code: "ARTIFACT_WRITE_FAILED",
                        message: &error.to_string(),
                    },
                });
                std::process::exit(1);
            }
            if let Some(code) = threshold_failure_exit_code(mode, report.status) {
                std::process::exit(code);
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            let report = ErrorReport {
                schema_version: 1,
                status: "failed",
                evidence: "shade_v1_release_gate",
                mode: Some(mode),
                error: ErrorBody {
                    code: "BENCHMARK_FAILED",
                    message: &message,
                },
            };
            if emit_and_store(&report, output.as_deref()).is_err() {
                emit_json(&report);
            }
            std::process::exit(1);
        }
    }
}

async fn run_gate(args: &Args) -> anyhow::Result<GateReport> {
    let config = BenchmarkConfig::from_args(args)?;
    ensure!(
        cfg!(all(target_os = "macos", target_arch = "aarch64")),
        "release evidence requires a native Apple Silicon macOS binary"
    );
    let pool_parent = match &args.root {
        Some(root) => {
            fs::create_dir_all(root)
                .with_context(|| format!("cannot create benchmark root {}", root.display()))?;
            fs::canonicalize(root)?
        }
        None => fs::canonicalize(std::env::current_dir()?)?,
    };
    let filesystem = filesystem_name(&pool_parent)?;
    ensure!(filesystem == "apfs", "benchmark root must be on APFS");
    let shade_bin = resolve_shade_binary(args.shade_bin.as_deref())?;
    let environment = environment_evidence(filesystem, &shade_bin);

    let pool = tempfile::Builder::new()
        .prefix(".shade-release-gate-")
        .tempdir_in(&pool_parent)?;
    let pool_path = pool.path().to_path_buf();
    let benchmark =
        run_benchmark_in_pool(args.mode, config, environment, &pool_path, &shade_bin).await;
    let cleanup = cleanup_pool(pool);
    finish_gate_run(benchmark, cleanup, &pool_path)
}

async fn run_benchmark_in_pool(
    mode: GateMode,
    config: BenchmarkConfig,
    environment: EnvironmentEvidence,
    pool: &Path,
    shade_bin: &Path,
) -> anyhow::Result<GateReport> {
    let source = pool.join("source");
    create_source(&source, config.entries, config.payload_bytes)?;

    let apfs = ApfsFilesystem;
    let warmup = pool.join("cow-warmup");
    apfs.clone_immutable_tree(&source, &warmup)?;
    apfs.remove_tree(&warmup)?;

    let mut cow_roots = Vec::with_capacity(config.workspaces);
    let mut materialization_samples = Vec::with_capacity(config.workspaces);
    for index in 0..config.workspaces {
        let destination = pool.join(format!("cow-{index:03}"));
        let started = Instant::now();
        apfs.clone_immutable_tree(&source, &destination)?;
        materialization_samples.push(started.elapsed().as_micros());
        cow_roots.push(destination);
    }

    let mut copy_roots = Vec::with_capacity(config.workspaces);
    for index in 0..config.workspaces {
        let destination = pool.join(format!("copy-{index:03}"));
        full_copy_tree(&source, &destination)?;
        copy_roots.push(destination);
    }

    let source_usage = apfs.usage(&source)?;
    let cow_usage = aggregate_usage(&apfs, &cow_roots)?;
    let copy_usage = aggregate_usage(&apfs, &copy_roots)?;
    let space = space_evidence(source_usage, cow_usage, copy_usage);
    remove_measured_roots(&apfs, &cow_roots, &copy_roots)
        .context("cannot remove measured clone/copy roots before daemon latency phase")?;
    let materialization = MaterializationEvidence {
        method: APFS_IMMUTABLE_CLONE_STRATEGY,
        state: "warm_after_one_unmeasured_materialization",
        entry_count: config.entries,
        workspace_count: config.workspaces,
        latency: LatencyEvidence::new(
            materialization_samples,
            Some(MATERIALIZATION_P50_LIMIT_US),
            MATERIALIZATION_P95_LIMIT_US,
        ),
    };

    let (context, cli_ipc, observed_response_sizes) = latency_evidence(
        pool,
        &source,
        config.entries,
        config.latency_samples,
        shade_bin,
        &apfs,
    )
    .await?;
    let common_response_budget = common_response_budget(observed_response_sizes)?;

    let thresholds_enforced = mode == GateMode::Release;
    let failures = threshold_failures(
        &materialization,
        &space,
        &context,
        &cli_ipc,
        &common_response_budget,
    );
    let report = GateReport {
        schema_version: 1,
        status: gate_status(&failures),
        evidence: "shade_v1_release_gate",
        mode,
        thresholds_enforced,
        measured_at_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        environment,
        configuration: config,
        materialization,
        space,
        context,
        cli_ipc,
        common_response_budget,
        failures,
    };
    Ok(report)
}

fn remove_measured_roots(
    filesystem: &ApfsFilesystem,
    cow_roots: &[PathBuf],
    copy_roots: &[PathBuf],
) -> anyhow::Result<()> {
    for root in cow_roots.iter().chain(copy_roots) {
        filesystem
            .remove_tree(root)
            .with_context(|| format!("cannot remove measured root {}", root.display()))?;
    }
    Ok(())
}

fn cleanup_pool(pool: tempfile::TempDir) -> anyhow::Result<()> {
    let path = pool.path().to_path_buf();
    let first_error = match pool.close() {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    let mut last_error = None;
    for delay in POOL_CLEANUP_RETRY_DELAYS {
        std::thread::sleep(delay);
        match fs::remove_dir_all(&path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(anyhow::anyhow!(
        "cannot remove isolated benchmark pool {} after retries; first_error={first_error}; \
         last_error={}",
        path.display(),
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown".into())
    ))
}

fn finish_gate_run<T>(
    benchmark: anyhow::Result<T>,
    cleanup: anyhow::Result<()>,
    pool_path: &Path,
) -> anyhow::Result<T> {
    match (benchmark, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error.context(format!(
            "benchmark failed; isolated pool {} was removed",
            pool_path.display()
        ))),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error.context(format!(
            "benchmark completed but isolated pool {} leaked",
            pool_path.display()
        ))),
        (Err(error), Err(cleanup_error)) => Err(anyhow::anyhow!(
            "benchmark failed and isolated pool cleanup also failed; pool={}; \
             benchmark={error:#}; cleanup={cleanup_error:#}",
            pool_path.display()
        )),
    }
}

fn create_source(root: &Path, entries: usize, payload_bytes: usize) -> anyhow::Result<()> {
    fs::create_dir(root)?;
    for index in 0..entries {
        let path = root.join(format!("entry-{index:05}.bin"));
        let payload = deterministic_payload(index as u64, payload_bytes);
        let mut file = File::create(path)?;
        file.write_all(&payload)?;
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

fn deterministic_payload(index: u64, size: usize) -> Vec<u8> {
    let mut state = index ^ 0x9e37_79b9_7f4a_7c15;
    let mut payload = Vec::with_capacity(size);
    for _ in 0..size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        payload.push(state as u8);
    }
    payload
}

fn full_copy_tree(source: &Path, destination: &Path) -> anyhow::Result<()> {
    fs::create_dir(destination)?;
    let mut entries = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut buffer = vec![0_u8; 64 * 1024];
    for entry in entries {
        let metadata = entry.file_type()?;
        ensure!(
            metadata.is_file(),
            "full-copy fixture accepts only regular files"
        );
        let mut input = File::open(entry.path())?;
        let mut output = File::create(destination.join(entry.file_name()))?;
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
        }
    }
    File::open(destination)?.sync_all()?;
    Ok(())
}

fn aggregate_usage(filesystem: &ApfsFilesystem, roots: &[PathBuf]) -> anyhow::Result<Usage> {
    let mut aggregate = Usage {
        private_bytes: Some(0),
        ..Usage::default()
    };
    for root in roots {
        let usage = filesystem.usage(root)?;
        aggregate.logical_bytes = aggregate.logical_bytes.saturating_add(usage.logical_bytes);
        aggregate.referenced_bytes = aggregate
            .referenced_bytes
            .saturating_add(usage.referenced_bytes);
        aggregate.private_bytes = match (aggregate.private_bytes, usage.private_bytes) {
            (Some(total), Some(value)) => Some(total.saturating_add(value)),
            _ => None,
        };
    }
    Ok(aggregate)
}

fn space_evidence(source: Usage, cow: Usage, copied: Usage) -> SpaceEvidence {
    let (ratio, within_threshold) = private_ratio(cow.private_bytes, copied.private_bytes);
    SpaceEvidence {
        method: "aggregate_ATTR_CMNEXT_PRIVATESIZE_vs_userspace_read_write",
        source: source.into(),
        cow_workspaces: cow.into(),
        full_copy_workspaces: copied.into(),
        minimum_ratio: SPACE_RATIO_MINIMUM,
        observed_private_ratio: ratio,
        within_threshold,
    }
}

fn private_ratio(cow: Option<u64>, copied: Option<u64>) -> (Option<f64>, bool) {
    match (cow, copied) {
        (Some(0), Some(copy_bytes)) => (None, copy_bytes > 0),
        (Some(cow_bytes), Some(copy_bytes)) => (
            Some(copy_bytes as f64 / cow_bytes as f64),
            copy_bytes >= cow_bytes.saturating_mul(SPACE_RATIO_MINIMUM),
        ),
        _ => (None, false),
    }
}

async fn latency_evidence(
    pool: &Path,
    source: &Path,
    entry_count: usize,
    samples: usize,
    shade_bin: &Path,
    apfs: &ApfsFilesystem,
) -> anyhow::Result<(ContextEvidence, CliIpcEvidence, Vec<ResponseSizeSample>)> {
    let repository = pool.join("context-repository");
    apfs.clone_immutable_tree(source, &repository)?;
    git(&repository, &["init", "--initial-branch=main"])?;
    git(&repository, &["config", "user.name", "Shade Release Gate"])?;
    git(
        &repository,
        &["config", "user.email", "release-gate@shade.invalid"],
    )?;
    git(&repository, &["add", "--all"])?;
    git(&repository, &["commit", "-m", "benchmark fixture"])?;

    let daemon_root = pool.join("daemon");
    fs::create_dir(&daemon_root)?;
    let socket = daemon_root.join("shade.sock");
    let mut daemon = DaemonGuard::start(shade_bin, &daemon_root, &socket).await?;
    let client = release_gate_client(&socket)?;
    let session = client
        .sessions()
        .open(OpenSession {
            session_id: SessionId("release-gate-context".into()),
            repository: RepositoryLocator::Local {
                path: repository.to_string_lossy().into_owned(),
            },
            base: Some("main".into()),
            intent: Some("release gate latency measurement".into()),
        })
        .await
        .map_err(|error| gate_client_error("real daemon session open", error))?;

    let mut context_samples = Vec::with_capacity(samples);
    let mut max_context_response_bytes = 0;
    for _ in 0..samples {
        let started = Instant::now();
        let response = client
            .context(WorkspaceSelector {
                workspace_id: Some(session.opened.workspace.clone()),
                cwd: None,
            })
            .await
            .map_err(|error| gate_client_error("context latency sample", error))?;
        ensure_response_ok(&response.body, "context")?;
        max_context_response_bytes =
            max_context_response_bytes.max(serde_json::to_vec(&response)?.len());
        context_samples.push(started.elapsed().as_micros());
    }

    let mut cli_samples = Vec::with_capacity(samples);
    let mut max_doctor_response_bytes = 0;
    for _ in 0..samples {
        let started = Instant::now();
        let output = Command::new(shade_bin)
            .arg("--socket")
            .arg(&socket)
            .arg("doctor")
            .env("SHADE_ROOT", &daemon_root)
            .output()?;
        let elapsed = started.elapsed().as_micros();
        ensure!(
            output.status.success(),
            "shade doctor failed during CLI/IPC measurement"
        );
        let response: shade_protocol::WireResponse = serde_json::from_slice(&output.stdout)?;
        ensure_response_ok(&response.body, "doctor")?;
        max_doctor_response_bytes =
            max_doctor_response_bytes.max(serde_json::to_vec(&response)?.len());
        cli_samples.push(elapsed);
    }

    let release = session
        .release()
        .await
        .map_err(|error| gate_client_error("workspace release and final checkpoint", error))?;
    ensure!(
        matches!(release, TerminalOutcome::Completed(_)),
        "release returned a non-completed domain outcome"
    );
    daemon.stop();
    Ok((
        ContextEvidence {
            method: "real_daemon_unix_socket_context_query",
            tracked_entry_count: entry_count,
            latency: LatencyEvidence::new(context_samples, None, CONTEXT_P95_LIMIT_US),
        },
        CliIpcEvidence {
            method: "release_binary_process_spawn_doctor_unix_socket_round_trip",
            latency: LatencyEvidence::new(cli_samples, None, CLI_IPC_P95_LIMIT_US),
        },
        vec![
            ResponseSizeSample {
                name: "observed_context",
                source: "real_daemon",
                bytes: max_context_response_bytes,
            },
            ResponseSizeSample {
                name: "observed_doctor",
                source: "real_cli",
                bytes: max_doctor_response_bytes,
            },
        ],
    ))
}

fn release_gate_client(socket: &Path) -> anyhow::Result<ShadeClient> {
    ShadeClient::with_options(
        socket,
        Actor {
            kind: ActorKind::Cli,
            id: "release-gate".into(),
        },
        release_gate_client_options(),
    )
    .map_err(Into::into)
}

fn release_gate_client_options() -> ShadeClientOptions {
    ShadeClientOptions {
        request_timeout: GATE_REQUEST_TIMEOUT,
        operation_timeout: GATE_OPERATION_TIMEOUT,
        ..ShadeClientOptions::default()
    }
}

fn gate_client_error(phase: &'static str, error: ClientError) -> anyhow::Error {
    let operation = error
        .operation()
        .map(ToString::to_string)
        .unwrap_or_else(|| "none".into());
    anyhow::Error::new(error).context(format!("{phase} failed; operation_id={operation}"))
}

fn common_response_budget(
    observed: Vec<ResponseSizeSample>,
) -> anyhow::Result<ResponseBudgetEvidence> {
    let mut samples = common_response_fixtures()?
        .into_iter()
        .map(|(name, response)| {
            Ok(ResponseSizeSample {
                name,
                source: "deterministic_fixture",
                bytes: serde_json::to_vec(&response)?.len(),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    samples.extend(observed);
    let max_bytes = samples.iter().map(|sample| sample.bytes).max().unwrap_or(0);
    Ok(ResponseBudgetEvidence {
        method: "serde_json_minified_payload_bytes_without_jsonl_delimiter",
        limit_bytes: COMMON_RESPONSE_LIMIT_BYTES,
        max_bytes,
        samples,
        within_threshold: max_bytes <= COMMON_RESPONSE_LIMIT_BYTES,
    })
}

fn common_response_fixtures() -> anyhow::Result<Vec<(&'static str, WireResponse)>> {
    let request_id = "01J7W3N7Y9AZ8T6G5F4E3D2C1B";
    let sha256_a = ObjectId("0123456789abcdef".repeat(4));
    let sha256_b = ObjectId("fedcba9876543210".repeat(4));
    let completed = |result| WireResponse {
        v: shade_protocol::PROTOCOL_VERSION,
        request_id: request_id.into(),
        body: ResponseBody::Ok {
            outcome: Outcome::Completed(result),
        },
    };

    let context = CompactContext {
        workspace: WorkspaceId("ws_01J7W3N7Y9AZ8T6G5F4E3D2C1B".into()),
        session: SessionId("host_01J7W3N7Y9AZ8T6G5F4E3D".into()),
        base_ref: "origin/feature/shade-agent".into(),
        base_sha: sha256_a.clone(),
        head_sha: sha256_b.clone(),
        remote_sha: None,
        changes: CompactChanges {
            staged: 2,
            unstaged: 1,
            untracked: 3,
        },
        lifecycle: "active".into(),
        lease: "live".into(),
        dependencies: DependencyContext {
            state: "ready".into(),
            providers: vec!["pnpm".into(), "uv".into()],
            blocked_builds: Vec::new(),
        },
    };

    Ok(vec![
        (
            "context_sha256_polyglot_deps",
            completed(serde_json::to_value(context)?),
        ),
        (
            "checkpoint_sha256",
            completed(json!({
                "checkpoint_id": "ckpt_01J7W3N7Y9AZ8T6G5F4E3D2C1B",
                "head_sha": sha256_a,
                "index_tree": sha256_b,
                "working_tree": "89abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567",
            })),
        ),
        (
            "heartbeat",
            completed(json!({
                "lease": "lease_01J7W3N7Y9AZ8T6G5F4E3D2C1B",
                "expires_at_ms": 1_808_000_000_000_i64,
            })),
        ),
        (
            "publish_sha256",
            completed(json!({
                "branch": "shade/agent-runtime",
                "commit": "3456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef012",
                "tree": "456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123",
                "previous_remote": "56789abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234",
                "pushed": true,
            })),
        ),
        (
            "release",
            completed(json!({
                "session": "host_01J7W3N7Y9AZ8T6G5F4E3D",
                "workspace": "ws_01J7W3N7Y9AZ8T6G5F4E3D2C1B",
                "checkpoint_id": "ckpt_01J7W3N7Y9AZ8T6G5F4E3D2C1B",
                "released": true,
            })),
        ),
        (
            "sleep",
            completed(json!({
                "session": "host_01J7W3N7Y9AZ8T6G5F4E3D",
                "workspace": "ws_01J7W3N7Y9AZ8T6G5F4E3D2C1B",
                "checkpoint_id": "ckpt_01J7W3N7Y9AZ8T6G5F4E3D2C1B",
                "suspended": true,
                "reclaimed_bytes": 4_294_967_296_u64,
            })),
        ),
        (
            "accepted",
            WireResponse {
                v: shade_protocol::PROTOCOL_VERSION,
                request_id: request_id.into(),
                body: ResponseBody::Ok {
                    outcome: Outcome::Accepted {
                        operation_id: OperationId("op_01J7W3N7Y9AZ8T6G5F4E3D2C1B".into()),
                    },
                },
            },
        ),
        (
            "structured_error",
            WireResponse {
                v: shade_protocol::PROTOCOL_VERSION,
                request_id: request_id.into(),
                body: ResponseBody::Error {
                    error: ShadeError {
                        code: "DEPENDENCY_SOURCE_BUILD_REQUIRED".into(),
                        retry: "after_user_action".into(),
                        operation: Some(OperationId("op_01J7W3N7Y9AZ8T6G5F4E3D2C1B".into())),
                        next: Some("replace the dependency with a hashed wheel".into()),
                        diagnostics_id: Some("diag_01J7W3N7Y9AZ8T6G5F4E3D2C1B".into()),
                    },
                },
            },
        ),
    ])
}

fn ensure_response_ok(body: &ResponseBody, operation: &str) -> anyhow::Result<()> {
    ensure!(
        matches!(body, ResponseBody::Ok { .. }),
        "{operation} returned a domain error"
    );
    Ok(())
}

struct DaemonGuard {
    child: Child,
}

impl DaemonGuard {
    async fn start(binary: &Path, root: &Path, socket: &Path) -> anyhow::Result<Self> {
        let child = Command::new(binary)
            .arg("--socket")
            .arg(socket)
            .arg("daemon")
            .env("SHADE_ROOT", root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start {} daemon", binary.display()))?;
        let mut guard = Self { child };
        let deadline = Instant::now() + DAEMON_READY_TIMEOUT;
        loop {
            if let Some(status) = guard.child.try_wait()? {
                anyhow::bail!("shade daemon exited before readiness: {status}");
            }
            if fs::symlink_metadata(socket).is_ok_and(|metadata| metadata.file_type().is_socket())
                && std::os::unix::net::UnixStream::connect(socket).is_ok()
            {
                return Ok(guard);
            }
            ensure!(
                Instant::now() < deadline,
                "shade daemon did not become ready within {} seconds",
                DAEMON_READY_TIMEOUT.as_secs()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn stop(&mut self) {
        terminate(&mut self.child);
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        terminate(&mut self.child);
    }
}

fn terminate(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    for _ in 0..100 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn threshold_failures(
    materialization: &MaterializationEvidence,
    space: &SpaceEvidence,
    context: &ContextEvidence,
    cli_ipc: &CliIpcEvidence,
    common_response_budget: &ResponseBudgetEvidence,
) -> Vec<GateFailure> {
    let mut failures = Vec::new();
    if materialization.latency.p50_ms >= millis(MATERIALIZATION_P50_LIMIT_US) {
        failures.push(GateFailure {
            code: "MATERIALIZATION_P50_EXCEEDED",
            metric: "materialization.p50_ms",
            observed: materialization.latency.p50_ms.to_string(),
            required: "<300",
        });
    }
    if materialization.latency.p95_ms >= millis(MATERIALIZATION_P95_LIMIT_US) {
        failures.push(GateFailure {
            code: "MATERIALIZATION_P95_EXCEEDED",
            metric: "materialization.p95_ms",
            observed: materialization.latency.p95_ms.to_string(),
            required: "<1000",
        });
    }
    if context.latency.p95_ms >= millis(CONTEXT_P95_LIMIT_US) {
        failures.push(GateFailure {
            code: "CONTEXT_P95_EXCEEDED",
            metric: "context.p95_ms",
            observed: context.latency.p95_ms.to_string(),
            required: "<200",
        });
    }
    if cli_ipc.latency.p95_ms >= millis(CLI_IPC_P95_LIMIT_US) {
        failures.push(GateFailure {
            code: "CLI_IPC_P95_EXCEEDED",
            metric: "cli_ipc.p95_ms",
            observed: cli_ipc.latency.p95_ms.to_string(),
            required: "<10",
        });
    }
    if !space.within_threshold {
        failures.push(GateFailure {
            code: "SPACE_RATIO_NOT_MET",
            metric: "space.observed_private_ratio",
            observed: space.observed_private_ratio.map_or_else(
                || "unbounded_or_unavailable".into(),
                |value| value.to_string(),
            ),
            required: ">=5 using private/reclaimable bytes",
        });
    }
    if !common_response_budget.within_threshold {
        failures.push(GateFailure {
            code: "COMMON_RESPONSE_BUDGET_EXCEEDED",
            metric: "common_response_budget.max_bytes",
            observed: common_response_budget.max_bytes.to_string(),
            required: "<=512 minified JSON bytes",
        });
    }
    failures
}

fn gate_status(failures: &[GateFailure]) -> &'static str {
    if failures.is_empty() {
        "passed"
    } else {
        "failed"
    }
}

fn threshold_failure_exit_code(mode: GateMode, status: &str) -> Option<i32> {
    (mode == GateMode::Release && status == "failed").then_some(2)
}

fn percentile(samples: &[u128], percentile: usize) -> u128 {
    assert!(!samples.is_empty());
    let mut ordered = samples.to_vec();
    ordered.sort_unstable();
    let rank = (percentile * ordered.len()).div_ceil(100);
    ordered[rank.saturating_sub(1)]
}

fn millis(microseconds: u128) -> f64 {
    (microseconds as f64 / 1_000.0 * 1_000.0).round() / 1_000.0
}

fn resolve_shade_binary(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    let candidate = match explicit {
        Some(path) => path.to_path_buf(),
        None => {
            let executable = std::env::current_exe()?;
            let parent = executable
                .parent()
                .context("release-gate executable has no parent directory")?;
            if parent.file_name().is_some_and(|name| name == "examples") {
                parent
                    .parent()
                    .context("release-gate examples directory has no release parent")?
                    .join("shade")
            } else {
                parent.join("shade")
            }
        }
    };
    let candidate = fs::canonicalize(&candidate).with_context(|| {
        format!(
            "cannot locate release-built Shade binary at {}; run cargo build --workspace --release",
            candidate.display()
        )
    })?;
    ensure!(candidate.is_file(), "Shade binary path is not a file");
    Ok(candidate)
}

fn environment_evidence(filesystem: String, shade_bin: &Path) -> EnvironmentEvidence {
    EnvironmentEvidence {
        target_os: std::env::consts::OS,
        target_arch: std::env::consts::ARCH,
        filesystem,
        macos_version: command_text("sw_vers", &["-productVersion"]),
        kernel: command_text("uname", &["-a"]),
        hardware_model: command_text("sysctl", &["-n", "hw.model"]),
        chip: command_text("sysctl", &["-n", "machdep.cpu.brand_string"]),
        git: command_text("git", &["--version"]),
        rustc: command_text("rustc", &["--version"]),
        shade: shade_version(shade_bin).unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
        shade_binary_sha256: sha256_file(shade_bin).ok(),
        gate: env!("CARGO_PKG_VERSION").into(),
        gate_binary_sha256: std::env::current_exe()
            .ok()
            .and_then(|binary| sha256_file(&binary).ok()),
    }
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn shade_version(binary: &Path) -> Option<String> {
    let output = command_text(binary, &["--version"])?;
    let value: serde_json::Value = serde_json::from_str(&output).ok()?;
    value
        .pointer("/outcome/result/version")?
        .as_str()
        .map(str::to_owned)
}

fn command_text(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git(repository: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repository)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    ensure!(
        output.status.success(),
        "git command failed while preparing latency fixture"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn filesystem_name(path: &Path) -> anyhow::Result<String> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut information = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    let result = unsafe { libc::statfs(path.as_ptr(), information.as_mut_ptr()) };
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    let information = unsafe { information.assume_init() };
    let filesystem = unsafe { std::ffi::CStr::from_ptr(information.f_fstypename.as_ptr()) };
    Ok(filesystem.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "macos"))]
fn filesystem_name(_path: &Path) -> anyhow::Result<String> {
    anyhow::bail!("APFS filesystem discovery requires macOS")
}

fn emit_and_store(value: &impl Serialize, output: Option<&Path>) -> anyhow::Result<()> {
    let mut json = serde_json::to_string(value)?;
    json.push('\n');
    if let Some(output) = output {
        let parent = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let name = output
            .file_name()
            .context("artifact output path needs a file name")?
            .to_string_lossy();
        let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));
        fs::write(&temporary, json.as_bytes())?;
        fs::rename(&temporary, output)?;
        File::open(parent)?.sync_all()?;
    }
    print!("{json}");
    Ok(())
}

fn emit_json(value: &impl Serialize) {
    if let Ok(json) = serde_json::to_string(value) {
        println!("{json}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(mode: GateMode) -> Args {
        Args {
            mode,
            root: None,
            entries: None,
            workspaces: None,
            latency_samples: None,
            payload_bytes: None,
            shade_bin: None,
            output: None,
        }
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let samples: Vec<u128> = (1..=20).collect();
        assert_eq!(percentile(&samples, 50), 10);
        assert_eq!(percentile(&samples, 95), 19);
    }

    #[test]
    fn private_space_ratio_handles_zero_cow_growth_without_infinity() {
        assert_eq!(private_ratio(Some(0), Some(4096)), (None, true));
        assert_eq!(private_ratio(Some(0), Some(0)), (None, false));
    }

    #[test]
    fn private_space_ratio_enforces_five_times() {
        assert_eq!(private_ratio(Some(100), Some(500)), (Some(5.0), true));
        assert_eq!(private_ratio(Some(100), Some(499)), (Some(4.99), false));
        assert_eq!(private_ratio(None, Some(500)), (None, false));
    }

    #[test]
    fn payload_is_deterministic_and_varies_by_entry() {
        assert_eq!(deterministic_payload(7, 32), deterministic_payload(7, 32));
        assert_ne!(deterministic_payload(7, 32), deterministic_payload(8, 32));
    }

    #[test]
    fn release_gate_client_budget_covers_heavy_final_checkpoint() {
        let options = release_gate_client_options();
        assert_eq!(options.request_timeout, Duration::from_secs(120));
        assert_eq!(options.operation_timeout, Duration::from_secs(15 * 60));
        assert!(options.operation_timeout > options.request_timeout);
    }

    #[test]
    fn benchmark_error_still_removes_the_isolated_pool() {
        let parent = tempfile::tempdir().unwrap();
        let pool = tempfile::Builder::new()
            .prefix(".shade-release-gate-test-")
            .tempdir_in(parent.path())
            .unwrap();
        let pool_path = pool.path().to_path_buf();
        fs::write(pool_path.join("partial-artifact"), b"partial").unwrap();

        let result = finish_gate_run::<()>(
            Err(anyhow::anyhow!("synthetic benchmark failure")),
            cleanup_pool(pool),
            &pool_path,
        );

        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("synthetic benchmark failure"));
        assert!(message.contains("was removed"));
        assert!(!pool_path.exists());
    }

    #[test]
    fn release_profile_is_exact_and_rejects_every_custom_dimension() {
        let config = BenchmarkConfig::from_args(&args(GateMode::Release)).unwrap();
        assert_eq!(config.entries, RELEASE_ENTRIES);
        assert_eq!(config.workspaces, RELEASE_WORKSPACES);
        assert_eq!(config.latency_samples, RELEASE_LATENCY_SAMPLES);
        assert_eq!(config.payload_bytes, RELEASE_PAYLOAD_BYTES);

        let mut explicit = args(GateMode::Release);
        explicit.entries = Some(RELEASE_ENTRIES);
        explicit.workspaces = Some(RELEASE_WORKSPACES);
        explicit.latency_samples = Some(RELEASE_LATENCY_SAMPLES);
        explicit.payload_bytes = Some(RELEASE_PAYLOAD_BYTES);
        assert!(BenchmarkConfig::from_args(&explicit).is_ok());

        let mut wrong_entries = args(GateMode::Release);
        wrong_entries.entries = Some(RELEASE_ENTRIES - 1);
        assert!(BenchmarkConfig::from_args(&wrong_entries).is_err());

        let mut wrong_workspaces = args(GateMode::Release);
        wrong_workspaces.workspaces = Some(RELEASE_WORKSPACES + 1);
        assert!(BenchmarkConfig::from_args(&wrong_workspaces).is_err());

        let mut wrong_samples = args(GateMode::Release);
        wrong_samples.latency_samples = Some(RELEASE_LATENCY_SAMPLES + 1);
        assert!(BenchmarkConfig::from_args(&wrong_samples).is_err());

        let mut wrong_payload = args(GateMode::Release);
        wrong_payload.payload_bytes = Some(RELEASE_PAYLOAD_BYTES + 1);
        assert!(BenchmarkConfig::from_args(&wrong_payload).is_err());
    }

    #[test]
    fn failed_smoke_reports_failed_without_release_exit_code() {
        let failures = vec![GateFailure {
            code: "MATERIALIZATION_P95_EXCEEDED",
            metric: "materialization.p95_ms",
            observed: "1000".into(),
            required: "<1000",
        }];
        let status = gate_status(&failures);
        assert_eq!(status, "failed");
        assert_eq!(threshold_failure_exit_code(GateMode::Smoke, status), None);
        assert_eq!(
            threshold_failure_exit_code(GateMode::Release, status),
            Some(2)
        );
    }

    #[test]
    fn common_response_fixtures_fit_the_budget() {
        let evidence = common_response_budget(Vec::new()).unwrap();
        assert!(
            evidence.within_threshold,
            "largest fixture is {} bytes against a {}-byte budget: {:?}",
            evidence.max_bytes, COMMON_RESPONSE_LIMIT_BYTES, evidence.samples
        );
        assert!(evidence.max_bytes <= COMMON_RESPONSE_LIMIT_BYTES);
        assert_eq!(evidence.samples.len(), 8);
    }

    #[test]
    fn context_fixture_covers_sha256_oids_and_dependencies() {
        let fixtures = common_response_fixtures().unwrap();
        let (_, context) = fixtures
            .iter()
            .find(|(name, _)| *name == "context_sha256_polyglot_deps")
            .unwrap();
        let context = serde_json::to_value(context).unwrap();
        assert_eq!(
            context["outcome"]["result"]["base_sha"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(
            context["outcome"]["result"]["head_sha"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(
            context["outcome"]["result"]["dependencies"]["providers"],
            json!(["pnpm", "uv"])
        );
    }

    #[test]
    fn sha256_file_records_content_digest() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), b"abc").unwrap();
        assert_eq!(
            sha256_file(file.path()).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn production_engine_can_open_an_isolated_control_plane() {
        let root = tempfile::tempdir().unwrap();
        let result =
            shade_engine::Engine::open(shade_engine::config::EngineConfig::at(root.path()));
        if let Err(error) = result {
            panic!("production engine failed to open: {error:?}");
        }
    }
}
