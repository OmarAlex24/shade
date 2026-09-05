use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum, error::ErrorKind};
use serde_json::json;
use shade_client::{ClientError, ShadeClient};
use shade_engine::Engine;
use shade_engine::config::EngineConfig;
use shade_protocol::{
    CheckpointId, EventEnvelope, Intent, OpenSession, OperationId, Outcome, PROTOCOL_VERSION,
    Query, QueryRequest, RepositoryId, RepositoryLocator, ResponseBody, ReviewAction, ReviewId,
    SessionId, ShadeError, WireRequest, WireResponse, WorkspaceSelector,
};
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

const MAX_FRAME_BYTES: usize = 1_048_576;
const MAX_REQUEST_ID_BYTES: usize = 128;
const SOCKET_MODE: u32 = 0o600;

#[derive(Debug, Parser)]
#[command(name = "shade", version, disable_help_subcommand = true)]
struct Cli {
    #[arg(long, env = "SHADE_SOCKET", global = true)]
    socket: Option<PathBuf>,
    #[arg(long, global = true)]
    idempotency_key: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Open(OpenArgs),
    Context(SelectorArgs),
    Heartbeat {
        #[arg(long, env = "SHADE_SESSION")]
        session: String,
        #[arg(long, env = "SHADE_LEASE")]
        lease: String,
    },
    Checkpoint {
        #[command(flatten)]
        selector: SelectorArgs,
        #[arg(long, default_value = "agent")]
        reason: String,
    },
    Fork {
        #[command(flatten)]
        selector: SelectorArgs,
        #[arg(long)]
        session: String,
        #[arg(long)]
        intent: Option<String>,
    },
    Sync(SelectorArgs),
    Restore {
        #[command(flatten)]
        selector: SelectorArgs,
        checkpoint: String,
    },
    Deps {
        #[command(subcommand)]
        command: DepsCommand,
    },
    Publish {
        #[command(flatten)]
        selector: SelectorArgs,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        message: String,
        #[arg(long, default_value_t = false)]
        push: bool,
    },
    Resolve(SelectorArgs),
    Release(SelectorArgs),
    Events {
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = false)]
        follow: bool,
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    Warm {
        repository: String,
        #[arg(long)]
        repository_id: bool,
    },
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    Doctor {
        #[arg(long)]
        diagnostics: Option<String>,
    },
    Gc,
    Install(InstallArgs),
    #[command(hide = true)]
    Daemon(DaemonArgs),
}

#[derive(Debug, Args)]
struct InstallArgs {
    #[arg(long, hide = true, requires_all = ["harness_root", "harness_label"])]
    harness_install: bool,
    #[arg(long, hide = true, requires = "harness_install")]
    harness_root: Option<PathBuf>,
    #[arg(long, hide = true, requires = "harness_install")]
    harness_label: Option<String>,
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[arg(
        long,
        hide = true,
        requires_all = ["harness_lease_ttl_secs", "harness_orphan_grace_secs"]
    )]
    harness_lifecycle: bool,
    #[arg(long, hide = true, requires = "harness_lifecycle")]
    harness_lease_ttl_secs: Option<i64>,
    #[arg(long, hide = true, requires = "harness_lifecycle")]
    harness_orphan_grace_secs: Option<i64>,
}

#[derive(Debug, Subcommand)]
enum DepsCommand {
    Refresh(SelectorArgs),
    Scripts(SelectorArgs),
    ApproveScript(ScriptArgs),
    RevokeScript(ScriptArgs),
}

#[derive(Debug, Args)]
struct ScriptArgs {
    #[command(flatten)]
    selector: SelectorArgs,
    #[arg(long)]
    provider: String,
    #[arg(long)]
    package: String,
    #[arg(long)]
    version: String,
    #[arg(long)]
    integrity: String,
}

impl ScriptArgs {
    fn approval(&self) -> shade_protocol::ScriptApproval {
        shade_protocol::ScriptApproval {
            provider: self.provider.clone(),
            package: self.package.clone(),
            version: self.version.clone(),
            integrity: self.integrity.clone(),
        }
    }
}

#[derive(Debug, Subcommand)]
enum ReviewCommand {
    Resolve {
        review: String,
        #[arg(value_enum)]
        action: ReviewChoice,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ReviewChoice {
    Merge,
    Keep,
    Discard,
}

#[derive(Debug, Args)]
struct OpenArgs {
    repository: String,
    #[arg(long)]
    repository_id: bool,
    #[arg(long, env = "SHADE_SESSION")]
    session: Option<String>,
    #[arg(long)]
    base: Option<String>,
    #[arg(long)]
    intent: Option<String>,
}

#[derive(Debug, Args, Clone)]
struct SelectorArgs {
    #[arg(long, env = "SHADE_WORKSPACE")]
    workspace: Option<String>,
    #[arg(long)]
    cwd: Option<PathBuf>,
}

impl SelectorArgs {
    fn selector(&self) -> anyhow::Result<WorkspaceSelector> {
        let cwd = match &self.cwd {
            Some(path) => Some(std::fs::canonicalize(absolutize(path)?)?),
            None if self.workspace.is_none() => Some(std::env::current_dir()?),
            None => None,
        };
        Ok(WorkspaceSelector {
            workspace_id: self.workspace.clone().map(shade_protocol::WorkspaceId),
            cwd: cwd.map(|path| path_string(&path)).transpose()?,
        })
    }
}

fn main() {
    // This private subprocess protocol carries Git content, not CLI JSON.
    // Handle it before parsing or runtime setup, and never echo rejected input.
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments
        .get(1)
        .is_some_and(|argument| argument == "__git-filter")
    {
        if arguments.len() != 3
            || shade_engine::secret_policy::run_git_filter(
                Path::new(&arguments[2]),
                std::io::stdin().lock(),
                std::io::stdout().lock(),
            )
            .is_err()
        {
            std::process::exit(1);
        }
        return;
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp => {
                emit(&local_completed(help_contract()));
                return;
            }
            ErrorKind::DisplayVersion => {
                emit(&local_completed(
                    json!({"name":"shade","version":env!("CARGO_PKG_VERSION")}),
                ));
                return;
            }
            _ => {
                emit(&local_error(
                    "CLI_ARGUMENT_INVALID",
                    "never",
                    "inspect `shade --help` and correct the arguments",
                    None,
                ));
                std::process::exit(64);
            }
        },
    };
    // Short-lived CLI calls only drive their own socket and timers. Avoid
    // starting a worker per CPU on every invocation; the durable daemon
    // retains its multithreaded scheduler for concurrent sessions.
    let mut runtime = if matches!(&cli.command, Command::Daemon(_)) {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    let diagnostic_config = EngineConfig::discover().ok();
    let result = match runtime.enable_all().build() {
        Ok(runtime) => runtime.block_on(run(cli)),
        Err(error) => Err(error.into()),
    };
    if let Err(error) = result {
        if let Some(ClientError::Timeout {
            operation,
            idempotency_key,
        }) = error.downcast_ref::<ClientError>()
        {
            emit(&local_timeout(
                operation.clone(),
                idempotency_key.as_deref(),
            ));
            std::process::exit(75);
        }
        if let Some(ClientError::Domain(error)) = error.downcast_ref::<ClientError>() {
            emit(&WireResponse {
                v: PROTOCOL_VERSION,
                request_id: "cli".into(),
                body: ResponseBody::Error {
                    error: error.clone(),
                },
            });
            std::process::exit(1);
        }
        let diagnostics_id = diagnostic_config.and_then(|config| {
            let detail = error
                .chain()
                .find_map(|cause| {
                    cause
                        .downcast_ref::<shade_engine::EngineError>()
                        .and_then(shade_engine::EngineError::diagnostic_message)
                })
                .unwrap_or("");
            shade_engine::diagnostics::record_cli_error(&config, format!("{error:#}\n{detail}"))
                .ok()
                .map(|diagnostic| diagnostic.id)
        });
        emit(&local_error(
            "CLI_FAILED",
            "safe",
            "run `shade doctor`, then retry with the same idempotency key",
            diagnostics_id,
        ));
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    if let Command::Daemon(args) = &cli.command {
        return serve(cli.socket.clone(), args).await;
    }
    if let Command::Install(args) = &cli.command {
        let mut config = EngineConfig::discover()?;
        if let Some(socket) = cli.socket {
            config.socket = absolutize(&socket)?;
        }
        emit(&local_completed(install(&config, args).await?));
        return Ok(());
    }
    let config = EngineConfig::discover()?;
    let socket = cli.socket.unwrap_or_else(|| config.socket.clone());
    let client = ShadeClient::cli(socket);
    let idempotency_key = cli
        .idempotency_key
        .unwrap_or_else(|| ulid::Ulid::new().to_string());
    match cli.command {
        Command::Open(args) => {
            let session_id = args
                .session
                .unwrap_or_else(|| format!("session_{}", ulid::Ulid::new()));
            emit(
                &client
                    .execute_wait_idempotent(
                        Intent::SessionOpen(OpenSession {
                            session_id: SessionId(session_id),
                            repository: locator(&args.repository, args.repository_id)?,
                            base: args.base,
                            intent: args.intent,
                        }),
                        idempotency_key,
                    )
                    .await?,
            );
        }
        Command::Context(selector) => emit(&client.context(selector.selector()?).await?),
        Command::Heartbeat { session, lease } => emit(
            &client
                .execute_wait_idempotent(
                    Intent::LeaseHeartbeat {
                        session_id: SessionId(session),
                        lease_id: shade_protocol::LeaseId(lease),
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Checkpoint { selector, reason } => emit(
            &client
                .execute_wait_idempotent(
                    Intent::WorkspaceCheckpoint {
                        selector: selector.selector()?,
                        reason,
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Fork {
            selector,
            session,
            intent,
        } => emit(
            &client
                .execute_wait_idempotent(
                    Intent::WorkspaceFork {
                        selector: selector.selector()?,
                        child_session_id: SessionId(session),
                        intent,
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Sync(selector) => emit(
            &client
                .execute_wait_idempotent(
                    Intent::WorkspaceSync {
                        selector: selector.selector()?,
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Restore {
            selector,
            checkpoint,
        } => emit(
            &client
                .execute_wait_idempotent(
                    Intent::WorkspaceRestore {
                        selector: selector.selector()?,
                        checkpoint_id: CheckpointId(checkpoint),
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Deps {
            command: DepsCommand::Scripts(selector),
        } => emit(
            &client
                .query(Query::DependencyScripts {
                    selector: selector.selector()?,
                })
                .await?,
        ),
        Command::Deps {
            command: DepsCommand::ApproveScript(args),
        } => emit(
            &client
                .execute_wait_idempotent(
                    Intent::DependencyScriptDecision {
                        selector: args.selector.selector()?,
                        approval: args.approval(),
                        allow: true,
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Deps {
            command: DepsCommand::RevokeScript(args),
        } => emit(
            &client
                .execute_wait_idempotent(
                    Intent::DependencyScriptDecision {
                        selector: args.selector.selector()?,
                        approval: args.approval(),
                        allow: false,
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Deps {
            command: DepsCommand::Refresh(selector),
        } => emit(
            &client
                .execute_wait_idempotent(
                    Intent::DependenciesRefresh {
                        selector: selector.selector()?,
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Publish {
            selector,
            branch,
            message,
            push,
        } => emit(
            &client
                .execute_wait_idempotent(
                    Intent::WorkspacePublish {
                        selector: selector.selector()?,
                        branch,
                        message,
                        push,
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Resolve(selector) => emit(
            &client
                .execute_wait_idempotent(
                    Intent::ResolutionComplete {
                        selector: selector.selector()?,
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Release(selector) => emit(
            &client
                .execute_wait_idempotent(
                    Intent::WorkspaceRelease {
                        selector: selector.selector()?,
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Events {
            after,
            follow,
            limit,
        } => {
            if follow {
                let mut stream = client.events(after).await?;
                while let Some(event) = stream.next().await? {
                    emit(&event);
                }
            } else {
                emit_event_page(
                    client
                        .query(Query::Events {
                            after_cursor: after,
                            limit,
                        })
                        .await?,
                )?;
            }
        }
        Command::Warm {
            repository,
            repository_id,
        } => emit(
            &client
                .execute_wait_idempotent(
                    Intent::RepositoryWarm {
                        repository: locator(&repository, repository_id)?,
                    },
                    idempotency_key,
                )
                .await?,
        ),
        Command::Review {
            command: ReviewCommand::Resolve { review, action },
        } => {
            let action = match action {
                ReviewChoice::Merge => ReviewAction::MergeParent,
                ReviewChoice::Keep => ReviewAction::Keep,
                ReviewChoice::Discard => ReviewAction::Discard,
            };
            emit(
                &client
                    .execute_wait_idempotent(
                        Intent::ReviewResolve {
                            review_id: ReviewId(review),
                            action,
                        },
                        idempotency_key,
                    )
                    .await?,
            );
        }
        Command::Doctor { diagnostics: None } => emit(&client.query(Query::Doctor).await?),
        Command::Doctor {
            diagnostics: Some(id),
        } => emit(&read_diagnostic(&client, &config, id).await?),
        Command::Gc => emit(
            &client
                .execute_wait_idempotent(Intent::GarbageCollect, idempotency_key)
                .await?,
        ),
        Command::Install(_) | Command::Daemon(_) => unreachable!(),
    }
    Ok(())
}

async fn read_diagnostic(
    client: &ShadeClient,
    config: &EngineConfig,
    id: String,
) -> anyhow::Result<WireResponse> {
    if !shade_engine::diagnostics::valid_id(&id) {
        return Ok(local_error(
            "DIAGNOSTIC_ID_INVALID",
            "never",
            "use the diagnostics_id from a Shade error",
            None,
        ));
    }
    match client
        .query(Query::Diagnostics {
            diagnostics_id: id.clone(),
        })
        .await
    {
        Ok(response) => return Ok(response),
        Err(ClientError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(error.into()),
    }
    match shade_engine::db::Database::read_diagnostic(&config.database_path(), &id)? {
        Some(diagnostic) => Ok(local_completed(serde_json::to_value(diagnostic)?)),
        None => Ok(local_error(
            "DIAGNOSTIC_NOT_FOUND",
            "never",
            "use the Shade root that produced this error",
            None,
        )),
    }
}

async fn serve(socket_override: Option<PathBuf>, daemon_args: &DaemonArgs) -> anyhow::Result<()> {
    let mut config = EngineConfig::discover()?;
    if let Some(socket) = socket_override {
        config.socket = absolutize(&socket)?;
    }
    if daemon_args.harness_lifecycle {
        config = config.with_harness_lifecycle_timing(
            daemon_args
                .harness_lease_ttl_secs
                .context("explicit harness lifecycle requires a lease TTL")?,
            daemon_args
                .harness_orphan_grace_secs
                .context("explicit harness lifecycle requires an orphan grace")?,
        )?;
    }
    if let Some(wait_ms) = operation_wait_override()? {
        config.operation_wait_ms = wait_ms;
    }
    let engine = Engine::open(config.clone())?;
    remove_stale_socket(&config.socket).await?;
    let (listener, socket_guard) = bind_private_socket(&config.socket)?;
    let startup_nonce = ulid::Ulid::new().to_string();
    let _ = engine
        .execute(shade_protocol::ExecuteRequest {
            v: PROTOCOL_VERSION,
            request_id: startup_nonce.clone(),
            idempotency_key: format!("startup:{startup_nonce}"),
            actor: shade_protocol::Actor {
                kind: shade_protocol::ActorKind::System,
                id: "daemon".into(),
            },
            intent: Intent::Reconcile,
        })
        .await;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut maintenance = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(30),
    );
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let engine = engine.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(engine, stream).await {
                        tracing::debug!(%error, "client disconnected");
                    }
                });
            }
            _ = maintenance.tick() => {
                let engine = engine.clone();
                tokio::spawn(async move {
                    let nonce = ulid::Ulid::new();
                    let _ = engine.execute(shade_protocol::ExecuteRequest {
                        v: PROTOCOL_VERSION,
                        request_id: format!("maintenance_{nonce}"),
                        idempotency_key: format!("maintenance:{nonce}"),
                        actor: shade_protocol::Actor {
                            kind: shade_protocol::ActorKind::System,
                            id: "daemon".into(),
                        },
                        intent: Intent::MaintenanceSweep,
                    }).await;
                });
            }
            _ = interrupt.recv() => break,
            _ = terminate.recv() => break,
        }
    }
    drop(listener);
    drop(socket_guard);
    Ok(())
}

async fn handle_connection(engine: Engine, stream: UnixStream) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let frame = match read_frame(&mut reader).await {
        Ok(Some(frame)) => frame,
        Ok(None) => return Ok(()),
        Err(FrameError::TooLarge) => {
            write_json_line(
                &mut writer,
                &wire_error(
                    "invalid",
                    "REQUEST_TOO_LARGE",
                    "never",
                    "send one NDJSON request of at most 1048576 bytes",
                ),
            )
            .await?;
            return Ok(());
        }
        Err(FrameError::Incomplete) => {
            write_json_line(
                &mut writer,
                &wire_error(
                    "invalid",
                    "REQUEST_FRAME_INCOMPLETE",
                    "safe",
                    "terminate the request with a newline and retry",
                ),
            )
            .await?;
            return Ok(());
        }
        Err(FrameError::Io(error)) => return Err(error.into()),
    };
    let request_id = request_id_hint(&frame);
    let request = match serde_json::from_slice::<WireRequest>(&frame) {
        Ok(request) => request,
        Err(_) => {
            write_json_line(
                &mut writer,
                &wire_error(
                    &request_id,
                    "REQUEST_INVALID",
                    "never",
                    "send one protocol-v1 NDJSON request",
                ),
            )
            .await?;
            return Ok(());
        }
    };
    if !valid_request_id(wire_request_id(&request)) {
        write_json_line(
            &mut writer,
            &wire_error(
                "invalid",
                "REQUEST_ID_INVALID",
                "never",
                "use a non-empty request id of at most 128 bytes",
            ),
        )
        .await?;
        return Ok(());
    }
    match request {
        WireRequest::Execute(request) => {
            if daemon_only_request(&request) {
                write_json_line(
                    &mut writer,
                    &wire_error(
                        &request.request_id,
                        "INTENT_FORBIDDEN",
                        "never",
                        "internal maintenance is daemon-owned",
                    ),
                )
                .await?;
                return Ok(());
            }
            let request_id = request.request_id.clone();
            let actor = request.actor.clone();
            let idempotency_key = request.idempotency_key.clone();
            let wait_ms = engine.config().operation_wait_ms;
            let execution_engine = engine.clone();
            let mut task = tokio::spawn(async move { execution_engine.execute(request).await });
            if wait_ms == 0 {
                write_timeout_result(
                    &engine,
                    &mut task,
                    &mut writer,
                    request_id,
                    actor,
                    idempotency_key,
                )
                .await?;
                return Ok(());
            }
            tokio::select! {
                result = &mut task => {
                    match result {
                        Ok(response) => write_json_line(&mut writer, &response).await?,
                        Err(_) => {
                            write_json_line(
                                &mut writer,
                                &wire_error(
                                    &request_id,
                                    "OPERATION_TASK_FAILED",
                                    "safe",
                                    "query events, then retry with the same idempotency key",
                                ),
                            ).await?;
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {
                    write_timeout_result(
                        &engine,
                        &mut task,
                        &mut writer,
                        request_id,
                        actor,
                        idempotency_key,
                    ).await?;
                }
            }
        }
        WireRequest::Query(request) => {
            write_json_line(&mut writer, &engine.query(request).await).await?;
        }
        WireRequest::Subscribe {
            v,
            request_id,
            mut after_cursor,
        } => {
            if v != PROTOCOL_VERSION {
                write_json_line(
                    &mut writer,
                    &wire_error(
                        &request_id,
                        "PROTOCOL_VERSION_UNSUPPORTED",
                        "never",
                        "use protocol version 1",
                    ),
                )
                .await?;
                return Ok(());
            }
            loop {
                for event in engine.events(after_cursor, 256)? {
                    after_cursor = event.cursor;
                    write_json_line(&mut writer, &event).await?;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    Ok(())
}

fn daemon_only_request(request: &shade_protocol::ExecuteRequest) -> bool {
    request.actor.kind == shade_protocol::ActorKind::System
        || matches!(
            &request.intent,
            Intent::MaintenanceSweep | Intent::Reconcile
        )
}

async fn write_timeout_result(
    engine: &Engine,
    task: &mut tokio::task::JoinHandle<WireResponse>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    request_id: String,
    actor: shade_protocol::Actor,
    idempotency_key: String,
) -> anyhow::Result<()> {
    let mut task_finished = false;
    loop {
        let lookup = engine
            .query(QueryRequest {
                v: PROTOCOL_VERSION,
                request_id: format!("lookup_{}", ulid::Ulid::new()),
                query: Query::OperationByKey {
                    actor_kind: actor.kind,
                    actor_id: actor.id.clone(),
                    idempotency_key: idempotency_key.clone(),
                },
            })
            .await;
        if let Some(operation_id) = operation_id(&lookup) {
            let response = WireResponse {
                v: PROTOCOL_VERSION,
                request_id,
                body: ResponseBody::Ok {
                    outcome: Outcome::Accepted { operation_id },
                },
            };
            write_json_line(writer, &response).await?;
            // Dropping a JoinHandle detaches the durable operation. Reconciliation
            // recovers it if the daemon itself exits before completion.
            return Ok(());
        }
        if !operation_missing(&lookup) {
            let response = remap_request_id(lookup, request_id);
            write_json_line(writer, &response).await?;
            return Ok(());
        }
        if task_finished {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            continue;
        }
        tokio::select! {
            result = &mut *task => {
                let response = match result {
                    Ok(response) => response,
                    Err(_) => wire_error(
                        &request_id,
                        "OPERATION_TASK_FAILED",
                        "safe",
                        "query events, then retry with the same idempotency key",
                    ),
                };
                if matches!(
                    &response.body,
                    ResponseBody::Error { error } if error.operation.is_none()
                ) {
                    write_json_line(writer, &response).await?;
                    return Ok(());
                }
                // A completed engine response with an operation attached has
                // already committed its journal row. Resolve that row on the
                // next iteration so a timed-out caller always gets its id.
                task_finished = true;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
        }
    }
}

async fn write_json_line<T: serde::Serialize>(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    value: &T,
) -> anyhow::Result<()> {
    let mut encoded = serde_json::to_vec(value)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    Ok(())
}

#[derive(Debug)]
enum FrameError {
    TooLarge,
    Incomplete,
    Io(std::io::Error),
}

async fn read_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameError>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    loop {
        let (available_len, newline) = {
            let available = reader.fill_buf().await.map_err(FrameError::Io)?;
            if available.is_empty() {
                return if frame.is_empty() {
                    Ok(None)
                } else {
                    Err(FrameError::Incomplete)
                };
            }
            (
                available.len(),
                available.iter().position(|byte| *byte == b'\n'),
            )
        };
        let consumed = newline.map_or(available_len, |index| index + 1);
        if frame.len().saturating_add(consumed) > MAX_FRAME_BYTES {
            reader.consume(consumed);
            return Err(FrameError::TooLarge);
        }
        {
            let available = reader.fill_buf().await.map_err(FrameError::Io)?;
            frame.extend_from_slice(&available[..consumed]);
        }
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

fn request_id_hint(frame: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|value| value.get("request_id")?.as_str().map(str::to_owned))
        .filter(|request_id| valid_request_id(request_id))
        .unwrap_or_else(|| "invalid".into())
}

fn wire_request_id(request: &WireRequest) -> &str {
    match request {
        WireRequest::Execute(request) => &request.request_id,
        WireRequest::Query(request) => &request.request_id,
        WireRequest::Subscribe { request_id, .. } => request_id,
    }
}

fn valid_request_id(request_id: &str) -> bool {
    !request_id.is_empty()
        && request_id.len() <= MAX_REQUEST_ID_BYTES
        && !request_id.chars().any(char::is_control)
}

fn operation_id(response: &WireResponse) -> Option<OperationId> {
    let ResponseBody::Ok {
        outcome: Outcome::Completed(value),
    } = &response.body
    else {
        return None;
    };
    value
        .get("id")?
        .as_str()
        .map(|id| OperationId(id.to_owned()))
}

fn operation_missing(response: &WireResponse) -> bool {
    matches!(
        &response.body,
        ResponseBody::Error { error } if error.code == "OPERATION_NOT_FOUND"
    )
}

fn remap_request_id(mut response: WireResponse, request_id: String) -> WireResponse {
    response.request_id = request_id;
    response
}

fn wire_error(request_id: &str, code: &str, retry: &str, next: &str) -> WireResponse {
    WireResponse {
        v: PROTOCOL_VERSION,
        request_id: request_id.to_owned(),
        body: ResponseBody::Error {
            error: ShadeError {
                code: code.to_owned(),
                retry: retry.to_owned(),
                operation: None,
                next: Some(next.to_owned()),
                diagnostics_id: None,
            },
        },
    }
}

fn local_error(
    code: &str,
    retry: &str,
    next: &str,
    diagnostics_id: Option<String>,
) -> WireResponse {
    let mut response = wire_error("cli", code, retry, next);
    if let ResponseBody::Error { error } = &mut response.body {
        error.diagnostics_id = diagnostics_id;
    }
    response
}

fn local_timeout(operation: Option<OperationId>, idempotency_key: Option<&str>) -> WireResponse {
    let next = idempotency_key.map_or_else(
        || "query events, then retry the request".to_owned(),
        |key| format!("retry with --idempotency-key {key}"),
    );
    let mut response = local_error("CLIENT_TIMEOUT", "query_operation", &next, None);
    if let ResponseBody::Error { error } = &mut response.body {
        error.operation = operation;
    }
    response
}

fn local_completed(value: serde_json::Value) -> WireResponse {
    WireResponse {
        v: PROTOCOL_VERSION,
        request_id: "cli".into(),
        body: ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        },
    }
}

fn help_contract() -> serde_json::Value {
    json!({
        "name": "shade",
        "version": env!("CARGO_PKG_VERSION"),
        "commands": [
            "open", "context", "heartbeat", "checkpoint", "fork", "sync", "restore",
            "deps refresh", "publish", "resolve", "release", "events",
            "warm", "review resolve", "doctor", "doctor --diagnostics <id>", "gc", "install"
        ]
    })
}

fn operation_wait_override() -> anyhow::Result<Option<u64>> {
    let Some(value) = std::env::var_os("SHADE_OPERATION_WAIT_MS") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("invalid operation wait"))?
        .parse::<u64>()?;
    if value > 60_000 {
        anyhow::bail!("operation wait exceeds maximum");
    }
    Ok(Some(value))
}

#[derive(Debug)]
struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
    owner: u32,
}

struct UmaskGuard(libc::mode_t);

impl UmaskGuard {
    fn private_socket() -> Self {
        // SAFETY: this is a process-global setting, changed only for the
        // synchronous bind at daemon startup and restored by Drop.
        Self(unsafe { libc::umask(0o177) })
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the value returned by umask is always valid.
        unsafe {
            libc::umask(self.0);
        }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
            && metadata.uid() == self.owner
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn remove_stale_socket(path: &Path) -> anyhow::Result<()> {
    let initial = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !initial.file_type().is_socket() {
        anyhow::bail!("socket path is occupied");
    }
    if initial.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("socket has a different owner");
    }
    match UnixStream::connect(path).await {
        Ok(_) => anyhow::bail!("daemon is already running"),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let current = std::fs::symlink_metadata(path)?;
    if !current.file_type().is_socket()
        || current.dev() != initial.dev()
        || current.ino() != initial.ino()
        || current.uid() != initial.uid()
    {
        anyhow::bail!("socket changed during startup");
    }
    std::fs::remove_file(path)?;
    Ok(())
}

fn bind_private_socket(path: &Path) -> anyhow::Result<(UnixListener, SocketGuard)> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("socket parent unavailable"))?;
    std::fs::create_dir_all(parent)?;
    let umask = UmaskGuard::private_socket();
    let listener = UnixListener::bind(path).context("bind socket")?;
    drop(umask);
    let metadata = std::fs::symlink_metadata(path).context("inspect socket")?;
    let guard = SocketGuard {
        path: path.to_owned(),
        device: metadata.dev(),
        inode: metadata.ino(),
        owner: metadata.uid(),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("socket ownership validation failed");
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))
        .context("set socket permissions")?;
    let mode = std::fs::symlink_metadata(path)
        .context("verify socket permissions")?
        .mode()
        & 0o777;
    if mode != SOCKET_MODE {
        anyhow::bail!("socket permission validation failed");
    }
    Ok((listener, guard))
}

fn locator(value: &str, registered: bool) -> anyhow::Result<RepositoryLocator> {
    if registered {
        return Ok(RepositoryLocator::Registered {
            repository_id: RepositoryId(value.into()),
        });
    }
    let path = PathBuf::from(value);
    if path.exists() {
        Ok(RepositoryLocator::Local {
            path: path_string(&std::fs::canonicalize(absolutize(&path)?)?)?,
        })
    } else {
        Ok(RepositoryLocator::Remote { url: value.into() })
    }
}

fn absolutize(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normalized.parent().is_some() {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8"))
}

fn emit(value: &impl serde::Serialize) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = serde_json::to_writer(&mut lock, value);
    let _ = lock.write_all(b"\n");
}

fn event_page(response: &WireResponse) -> anyhow::Result<Option<Vec<EventEnvelope>>> {
    match &response.body {
        ResponseBody::Ok {
            outcome: Outcome::Completed(value),
        } => {
            let events = value
                .get("events")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("event page is missing events"))?;
            Ok(Some(serde_json::from_value(events)?))
        }
        ResponseBody::Error { .. } => Ok(None),
        ResponseBody::Ok { .. } => anyhow::bail!("event query returned an invalid outcome"),
    }
}

fn emit_event_page(response: WireResponse) -> anyhow::Result<()> {
    if let Some(events) = event_page(&response)? {
        for event in events {
            emit(&event);
        }
    } else {
        emit(&response);
    }
    Ok(())
}

async fn install(config: &EngineConfig, args: &InstallArgs) -> anyhow::Result<serde_json::Value> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        anyhow::bail!("Shade supports Apple Silicon macOS only");
    }
    let (install_root, agents, label) = install_locations(config, args)?;
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    let service = format!("{domain}/{label}");
    if args.harness_install
        && std::process::Command::new("/bin/launchctl")
            .args(["print", &service])
            .output()?
            .status
            .success()
    {
        anyhow::bail!("the isolated acceptance label already exists");
    }
    let bin = install_root.join("bin/shade");
    let bin_parent = bin
        .parent()
        .ok_or_else(|| anyhow::anyhow!("binary parent unavailable"))?;
    std::fs::create_dir_all(bin_parent)?;
    std::fs::set_permissions(&install_root, std::fs::Permissions::from_mode(0o700))?;
    std::fs::set_permissions(bin_parent, std::fs::Permissions::from_mode(0o700))?;
    atomic_copy(&std::env::current_exe()?, &bin, 0o755)?;
    std::fs::create_dir_all(&agents)?;
    let plist = agents.join(format!("{label}.plist"));
    let content = launch_agent_plist(&label, &bin, config)?;
    atomic_write(&plist, content.as_bytes(), 0o600)?;
    if !args.harness_install {
        let _ = std::process::Command::new("/bin/launchctl")
            .args(["bootout", &service])
            .output();
    }
    let output = std::process::Command::new("/bin/launchctl")
        .args(["bootstrap", &domain, &plist.to_string_lossy()])
        .output()?;
    if !output.status.success() {
        anyhow::bail!("launchagent bootstrap failed");
    }
    if let Err(error) = wait_for_installed_daemon(config).await {
        let _ = std::process::Command::new("/bin/launchctl")
            .args(["bootout", &service])
            .output();
        return Err(error);
    }
    Ok(json!({
        "installed": bin,
        "launch_agent": plist,
        "label": label,
        "root": config.root,
        "socket": config.socket,
    }))
}

fn install_locations(
    config: &EngineConfig,
    args: &InstallArgs,
) -> anyhow::Result<(PathBuf, PathBuf, String)> {
    if args.harness_install {
        let root = std::fs::canonicalize(
            args.harness_root
                .as_ref()
                .context("acceptance root is required")?,
        )?;
        let label = args
            .harness_label
            .as_ref()
            .context("acceptance label is required")?;
        let suffix = label
            .strip_prefix("com.shade.daemon.acceptance.")
            .context("acceptance requires a reserved service label")?;
        anyhow::ensure!(
            !suffix.is_empty()
                && suffix.len() <= 80
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.')),
            "invalid acceptance service label"
        );
        anyhow::ensure!(
            config.root.starts_with(&root) && config.socket.starts_with(&root),
            "acceptance state and socket must remain inside its isolated root"
        );
        return Ok((
            root.join("installed"),
            root.join("LaunchAgents"),
            label.clone(),
        ));
    }
    let home = dirs::home_dir().context("home directory unavailable")?;
    Ok((
        home.join("Library/Application Support/Shade"),
        home.join("Library/LaunchAgents"),
        "com.shade.daemon".into(),
    ))
}

async fn wait_for_installed_daemon(config: &EngineConfig) -> anyhow::Result<()> {
    use std::time::Duration;
    let client = ShadeClient::with_options(
        &config.socket,
        shade_protocol::Actor {
            kind: shade_protocol::ActorKind::Cli,
            id: "install".into(),
        },
        shade_client::ShadeClientOptions {
            request_timeout: Duration::from_millis(500),
            ..Default::default()
        },
    )?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = client.query(Query::Doctor).await
            && matches!(response.body, ResponseBody::Ok { outcome: Outcome::Completed(ref value) } if value["state"] == "ok")
        {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "installed LaunchAgent did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn launch_agent_plist(label: &str, binary: &Path, config: &EngineConfig) -> anyhow::Result<String> {
    let binary = path_string(binary)?;
    let root = path_string(&config.root)?;
    let socket = path_string(&config.socket)?;
    let search_path = installed_search_path()?;
    // SDK calls await this service over a Unix socket, which cannot raise an
    // Adaptive job's priority through an XPC transaction. Background throttling
    // otherwise turns even tiny Git imports into client timeouts.
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{}</string>
<key>ProgramArguments</key><array>
<string>{}</string><string>--socket</string><string>{}</string><string>daemon</string>
</array>
<key>EnvironmentVariables</key><dict>
<key>SHADE_ROOT</key><string>{}</string>
<key>PATH</key><string>{}</string>
</dict>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
<key>ProcessType</key><string>Interactive</string>
<key>ThrottleInterval</key><integer>5</integer>
<key>Umask</key><integer>63</integer>
</dict></plist>
"#,
        xml_escape(label),
        xml_escape(&binary),
        xml_escape(&socket),
        xml_escape(&root),
        xml_escape(&search_path),
    ))
}

fn installed_search_path() -> anyhow::Result<String> {
    let mut paths = Vec::new();
    for path in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).chain([
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ]) {
        if !path.is_absolute() {
            continue;
        }
        if let Ok(path) = std::fs::canonicalize(path)
            && path.is_dir()
            && !paths.contains(&path)
        {
            paths.push(path);
        }
    }
    std::env::join_paths(paths)?
        .into_string()
        .map_err(|_| anyhow::anyhow!("host tool search path is not UTF-8"))
}

fn atomic_copy(source: &Path, destination: &Path, mode: u32) -> anyhow::Result<()> {
    let staging = staging_path(destination);
    let result = (|| -> anyhow::Result<()> {
        let mut source = std::fs::File::open(source)?;
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        std::io::copy(&mut source, &mut target)?;
        target.set_permissions(std::fs::Permissions::from_mode(mode))?;
        target.sync_all()?;
        drop(target);
        std::fs::rename(&staging, destination)?;
        sync_parent(destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

fn atomic_write(destination: &Path, content: &[u8], mode: u32) -> anyhow::Result<()> {
    let staging = staging_path(destination);
    let result = (|| -> anyhow::Result<()> {
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        target.write_all(content)?;
        target.set_permissions(std::fs::Permissions::from_mode(mode))?;
        target.sync_all()?;
        drop(target);
        std::fs::rename(&staging, destination)?;
        sync_parent(destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

fn staging_path(destination: &Path) -> PathBuf {
    destination.with_extension(format!("shade-staging-{}", ulid::Ulid::new()))
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("parent unavailable"))?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use shade_protocol::{Actor, ActorKind, ExecuteRequest};

    #[test]
    fn daemon_internal_intents_are_never_accepted_from_ipc() {
        let request = |kind, intent| ExecuteRequest {
            v: PROTOCOL_VERSION,
            request_id: "request".into(),
            idempotency_key: "key".into(),
            actor: Actor {
                kind,
                id: "caller".into(),
            },
            intent,
        };
        assert!(daemon_only_request(&request(
            ActorKind::Cli,
            Intent::Reconcile
        )));
        assert!(daemon_only_request(&request(
            ActorKind::System,
            Intent::GarbageCollect
        )));
        assert!(!daemon_only_request(&request(
            ActorKind::Cli,
            Intent::GarbageCollect
        )));
    }

    #[test]
    fn parses_machine_contract_verbs() {
        let cases = [
            vec!["shade", "open", ".", "--session", "chat-1"],
            vec!["shade", "context", "--workspace", "ws-1"],
            vec![
                "shade",
                "heartbeat",
                "--session",
                "chat-1",
                "--lease",
                "lease-1",
            ],
            vec!["shade", "checkpoint", "--workspace", "ws-1"],
            vec![
                "shade",
                "fork",
                "--workspace",
                "ws-1",
                "--session",
                "chat-2",
            ],
            vec!["shade", "sync", "--workspace", "ws-1"],
            vec!["shade", "restore", "--workspace", "ws-1", "cp-1"],
            vec!["shade", "deps", "refresh", "--workspace", "ws-1"],
            vec![
                "shade",
                "publish",
                "--workspace",
                "ws-1",
                "--branch",
                "feature",
                "--message",
                "publish",
            ],
            vec!["shade", "resolve", "--workspace", "ws-1"],
            vec!["shade", "release", "--workspace", "ws-1"],
            vec!["shade", "events", "--after", "3"],
            vec!["shade", "warm", "origin"],
            vec!["shade", "review", "resolve", "review-1", "merge"],
            vec!["shade", "doctor"],
            vec!["shade", "gc"],
            vec!["shade", "install"],
        ];
        for arguments in cases {
            Cli::try_parse_from(arguments).unwrap();
        }
    }

    #[test]
    fn harness_lifecycle_timing_is_hidden_and_requires_an_explicit_complete_opt_in() {
        let production = Cli::try_parse_from(["shade", "daemon"]).unwrap();
        let Command::Daemon(production_args) = production.command else {
            panic!("expected daemon command");
        };
        assert!(!production_args.harness_lifecycle);
        assert_eq!(production_args.harness_lease_ttl_secs, None);
        assert_eq!(production_args.harness_orphan_grace_secs, None);

        let parsed = Cli::try_parse_from([
            "shade",
            "daemon",
            "--harness-lifecycle",
            "--harness-lease-ttl-secs",
            "120",
            "--harness-orphan-grace-secs",
            "0",
        ])
        .unwrap();
        let Command::Daemon(args) = parsed.command else {
            panic!("expected daemon command");
        };
        assert!(args.harness_lifecycle);
        assert_eq!(args.harness_lease_ttl_secs, Some(120));
        assert_eq!(args.harness_orphan_grace_secs, Some(0));

        assert!(
            Cli::try_parse_from(["shade", "daemon", "--harness-orphan-grace-secs", "0",]).is_err()
        );
        assert!(Cli::try_parse_from(["shade", "daemon", "--harness-lifecycle"]).is_err());

        let help = Cli::try_parse_from(["shade", "daemon", "--help"])
            .unwrap_err()
            .to_string();
        assert!(!help.contains("harness"));
    }

    #[test]
    fn help_and_version_are_captured_instead_of_printed() {
        assert_eq!(
            Cli::try_parse_from(["shade", "--help"]).unwrap_err().kind(),
            ErrorKind::DisplayHelp
        );
        assert_eq!(
            Cli::try_parse_from(["shade", "--version"])
                .unwrap_err()
                .kind(),
            ErrorKind::DisplayVersion
        );
    }

    #[test]
    fn selector_prefers_workspace_environment_shape() {
        let selector = SelectorArgs {
            workspace: Some("ws-1".into()),
            cwd: None,
        }
        .selector()
        .unwrap();
        assert_eq!(selector.workspace_id.unwrap().0, "ws-1");
        assert!(selector.cwd.is_none());
    }

    #[test]
    fn relative_paths_are_lexically_normalized() {
        let path = absolutize(Path::new("one/../two/./three")).unwrap();
        assert!(path.is_absolute());
        assert!(path.ends_with("two/three"));
        assert!(!path.to_string_lossy().contains(".."));
    }

    #[tokio::test]
    async fn frame_reader_requires_one_bounded_ndjson_frame() {
        let bytes = br#"{"type":"query"}
trailing"#;
        let mut reader = BufReader::new(&bytes[..]);
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap(),
            b"{\"type\":\"query\"}\n"
        );

        let incomplete = b"{}";
        let mut reader = BufReader::new(&incomplete[..]);
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(FrameError::Incomplete)
        ));

        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        let mut reader = BufReader::new(&oversized[..]);
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(FrameError::TooLarge)
        ));
    }

    #[test]
    fn invalid_request_ids_are_not_reflected() {
        let request = WireRequest::Execute(ExecuteRequest {
            v: PROTOCOL_VERSION,
            request_id: "x".repeat(MAX_REQUEST_ID_BYTES + 1),
            idempotency_key: "key".into(),
            actor: Actor {
                kind: ActorKind::Cli,
                id: "test".into(),
            },
            intent: Intent::GarbageCollect,
        });
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(request_id_hint(&encoded), "invalid");
        assert!(!valid_request_id(wire_request_id(&request)));
    }

    #[test]
    fn public_errors_are_structured_compact_and_redacted() {
        let response = local_error(
            "CLI_FAILED",
            "safe",
            "run `shade doctor`, then retry with the same idempotency key",
            Some("diag_test".into()),
        );
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(encoded.len() <= 512);
        let text = String::from_utf8(encoded).unwrap();
        assert!(text.contains("\"code\":\"CLI_FAILED\""));
        assert!(text.contains("\"retry\":\"safe\""));
        assert!(!text.contains("/Users/"));

        let timeout = local_timeout(
            Some(OperationId("operation-opaque".into())),
            Some("generated-retry-key"),
        );
        let encoded = serde_json::to_vec(&timeout).unwrap();
        assert!(encoded.len() <= 512);
        let ResponseBody::Error { error } = timeout.body else {
            panic!("expected structured timeout error");
        };
        assert_eq!(error.code, "CLIENT_TIMEOUT");
        assert_eq!(error.operation.unwrap().0, "operation-opaque");
        assert_eq!(
            error.next.as_deref(),
            Some("retry with --idempotency-key generated-retry-key")
        );
    }

    #[test]
    fn operation_lookup_is_projected_to_accepted() {
        let response = WireResponse {
            v: PROTOCOL_VERSION,
            request_id: "lookup".into(),
            body: ResponseBody::Ok {
                outcome: Outcome::Completed(json!({"id":"op-opaque","state":"running"})),
            },
        };
        assert_eq!(operation_id(&response).unwrap().0, "op-opaque");
        assert!(!operation_missing(&response));
    }

    #[test]
    fn finite_events_are_projected_as_jsonl_records() {
        let response = WireResponse {
            v: PROTOCOL_VERSION,
            request_id: "events".into(),
            body: ResponseBody::Ok {
                outcome: Outcome::Completed(json!({
                    "events": [{
                        "v": 1,
                        "cursor": 7,
                        "event": "workspace.ready",
                        "resource": "ws-1",
                        "payload": {},
                        "created_at_ms": 10
                    }]
                })),
            },
        };
        let events = event_page(&response).unwrap().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].cursor, 7);
        assert_eq!(events[0].event, "workspace.ready");
    }

    #[tokio::test]
    async fn socket_is_0600_and_guard_does_not_remove_replacements() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("shade.sock");
        let (listener, guard) = match bind_private_socket(&socket) {
            Ok(bound) => bound,
            Err(error)
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.raw_os_error() == Some(libc::EPERM))
                }) =>
            {
                return;
            }
            Err(error) => panic!("{error:#}"),
        };
        let mode = std::fs::symlink_metadata(&socket).unwrap().mode() & 0o777;
        assert_eq!(mode, SOCKET_MODE);
        drop(listener);
        std::fs::remove_file(&socket).unwrap();
        std::fs::write(&socket, "replacement").unwrap();
        drop(guard);
        assert_eq!(std::fs::read_to_string(&socket).unwrap(), "replacement");
        assert!(remove_stale_socket(&socket).await.is_err());
    }

    #[test]
    fn launch_agent_is_single_binary_and_escapes_paths() {
        let temp = tempfile::tempdir().unwrap();
        let config = EngineConfig::at(temp.path().join("data&root"));
        let plist =
            launch_agent_plist("com.shade.daemon", &temp.path().join("shade<bin>"), &config)
                .unwrap();
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("<string>--socket</string>"));
        assert!(plist.contains("<key>SHADE_ROOT</key>"));
        assert!(plist.contains("data&amp;root"));
        assert!(plist.contains("shade&lt;bin&gt;"));
        assert!(plist.contains("<key>Umask</key><integer>63</integer>"));
    }

    #[test]
    fn atomic_write_replaces_as_one_private_file() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("agent.plist");
        std::fs::write(&destination, "old").unwrap();
        atomic_write(&destination, b"new", 0o600).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
        assert_eq!(
            std::fs::metadata(&destination).unwrap().mode() & 0o777,
            0o600
        );
    }
}
