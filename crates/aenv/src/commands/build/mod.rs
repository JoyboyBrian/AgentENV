//! Native client-orchestrated Dockerfile builds.
//!
//! `aenv build` parses and validates a single-stage Dockerfile up front,
//! boots an ordinary cold sandbox from the `FROM` image as the build VM,
//! executes the instructions sequentially through envd (streaming COPY/ADD
//! content into the guest without staging it on any AgentENV host), and
//! captures the result as a named snapshot carrying the final command
//! context and startup metadata. The build sandbox is deleted afterwards on
//! success and failure alike.

mod context;
#[cfg(test)]
mod e2e;
mod plan;

use crate::client::files::EnvdFilesClient;
use crate::client::snapshots::{CreateSnapshotRequest, SnapshotFinalContext};
use crate::client::Client;
use crate::grpc::{build_start_request, drain_output, StartOpts, Transport};
use crate::progress::TransferProgress;
use anyhow::{anyhow, bail, Context, Result};
use clap::Args as ClapArgs;
use context::{BuildContext, GuestPathKind, SelectedSource};
use envd::filesystem::FileType;
use envd::process::{process_event, StartResponse};
use futures::{Stream, StreamExt};
use plan::{BuildPlan, BuildStep, CopyStep};
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// TTL requested for the build sandbox; the keepalive refreshes it while the
/// build runs, and it doubles as the eviction fallback for abandoned builds.
const BUILD_SESSION_TTL_SECS: u32 = 300;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);
const ENVD_READY_TIMEOUT: Duration = Duration::from_secs(60);
const ENVD_READY_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const ENVD_READY_PROBE_INTERVAL: Duration = Duration::from_millis(200);
/// Extraction and guest-side interpretation run as root, so copied content
/// is root-owned like Docker's `--chown`-less COPY.
const GUEST_ROOT: &str = "root";
/// Run each Dockerfile command as a monitored background job in a dedicated
/// process group. When its shell exits, kill any descendants that it left
/// behind so build-time daemons are not frozen into the final snapshot.
///
/// A process that deliberately creates a new session/process group can
/// escape this boundary; supporting hostile daemonization would require a
/// guest cgroup contract that envd does not currently expose.
const RUN_PROCESS_GROUP_WRAPPER: &str = r#"
set -m
/bin/bash -lc "$1" &
__aenv_run_pgid=$!
set +m
wait "$__aenv_run_pgid"
__aenv_run_status=$?
kill -KILL -- "-$__aenv_run_pgid" 2>/dev/null || true
exit "$__aenv_run_status"
"#;

#[derive(ClapArgs)]
#[command(after_help = "Examples:
  aenv build . --name my-app
  aenv build ./service --name my-app -f ./service/Dockerfile.prod
  aenv build . --name my-app --image ghcr.io/myorg/base:latest --cpu 2 --memory 2048")]
pub struct Args {
    /// Build context directory
    #[arg(default_value = ".")]
    context: PathBuf,
    /// Path to the Dockerfile (defaults to <context>/Dockerfile)
    #[arg(short = 'f', long = "file")]
    dockerfile: Option<PathBuf>,
    /// Template name
    #[arg(long)]
    name: String,
    #[command(flatten)]
    resources: super::CpuMemoryArgs,
    /// Root filesystem size in MiB for the build sandbox (must be divisible by 1024)
    #[arg(long = "disk-size-mb", alias = "disk-mb", value_parser = super::start::parse_disk_size_mb)]
    disk_size_mb: Option<u32>,
    /// Override the Dockerfile FROM image used as the build base. Shortnames like `ubuntu:22.04` are supported.
    #[arg(long = "image", alias = "user-image")]
    user_image: Option<String>,
}

pub fn run(args: Args) -> Result<()> {
    let client = Client::from_env()?;
    run_with_client(client, args)
}

fn run_with_client(client: Client, args: Args) -> Result<()> {
    let prepared = prepare(&args)?;

    let total_steps = prepared.plan.steps.len() + 1;
    println!("[1/{total_steps}] FROM {}", prepared.plan.base_image);
    let sandbox_id = client
        .create_cold_sandbox(
            &prepared.plan.base_image,
            Some(BUILD_SESSION_TTL_SECS),
            args.resources.cpu_count,
            args.resources.memory_mb,
            args.disk_size_mb,
        )
        .context("creating build sandbox")?;
    println!("Created build sandbox {sandbox_id}");

    let capture_started = Arc::new(AtomicBool::new(false));
    let rt = super::tokio_rt()?;
    let build_result = rt.block_on(async {
        let build = execute_build(
            &client,
            &sandbox_id,
            &prepared,
            &args.name,
            capture_started.clone(),
        );
        tokio::pin!(build);
        tokio::select! {
            result = &mut build => result,
            _ = tokio::signal::ctrl_c() => {
                if capture_started.load(Ordering::Acquire) {
                    eprintln!(
                        "Snapshot capture is already in progress; waiting for it to finish \
                         before cleaning up the build sandbox"
                    );
                    build.await
                } else {
                    Err(anyhow!("build interrupted"))
                }
            },
        }
    });
    // Dropping the runtime stops the keepalive task before the sandbox goes
    // away. Cleanup always runs and must never mask the build outcome.
    drop(rt);
    if let Err(err) = client.delete_sandbox(&sandbox_id) {
        eprintln!(
            "warning: failed to delete build sandbox {sandbox_id}: {err:#}; \
             it will be evicted when its TTL expires"
        );
    }

    let snapshot = build_result?;
    println!(
        "Built template {} (snapshot {})",
        args.name, snapshot.snapshot_id
    );
    println!("Start with: aenv start {}", args.name);
    Ok(())
}

struct PreparedBuild {
    plan: BuildPlan,
    build_context: Option<BuildContext>,
    /// Pre-selected sources per COPY/ADD step, validated against the local
    /// context before any sandbox exists.
    copy_sources: Vec<Vec<SelectedSource>>,
}

/// Everything that can fail locally fails here, before a VM is created:
/// Dockerfile parsing/validation, context loading, source selection,
/// `.dockerignore` handling, and ADD archive rejection.
fn prepare(args: &Args) -> Result<PreparedBuild> {
    let dockerfile_path = args
        .dockerfile
        .clone()
        .unwrap_or_else(|| args.context.join("Dockerfile"));
    let dockerfile = std::fs::read_to_string(&dockerfile_path)
        .with_context(|| format!("reading {}", dockerfile_path.display()))?;
    let plan = plan::parse_build_plan(&dockerfile, args.user_image.clone())?;

    let needs_context = plan
        .steps
        .iter()
        .any(|step| matches!(step, BuildStep::Copy(_)));
    let build_context = if needs_context {
        Some(BuildContext::load(&args.context)?)
    } else {
        // Still fail fast on an obviously wrong context argument.
        if !args.context.is_dir() {
            bail!(
                "build context must be a directory: {} (pass the Dockerfile with -f and the \
                 context directory as the positional argument)",
                args.context.display()
            );
        }
        None
    };

    let mut copy_sources = Vec::new();
    for step in &plan.steps {
        let BuildStep::Copy(copy) = step else {
            copy_sources.push(Vec::new());
            continue;
        };
        let build_context = build_context
            .as_ref()
            .expect("copy steps imply a loaded build context");
        let sources = build_context
            .select_sources(&copy.sources)
            .with_context(|| format!("invalid {} instruction", copy.instruction))?;
        validate_copy_sources(build_context, copy, &sources)?;
        copy_sources.push(sources);
    }

    Ok(PreparedBuild {
        plan,
        build_context,
        copy_sources,
    })
}

/// Walks directory sources so symlinks, special files, and non-UTF-8 names
/// are rejected before the VM boots, and rejects ADD sources Docker would
/// auto-extract.
fn validate_copy_sources(
    build_context: &BuildContext,
    copy: &CopyStep,
    sources: &[SelectedSource],
) -> Result<()> {
    for source in sources {
        if source.is_dir {
            build_context
                .validate_directory_source(source)
                .with_context(|| format!("invalid {} instruction", copy.instruction))?;
        } else if copy.instruction == "ADD" && context::looks_like_add_archive(&source.path)? {
            bail!(
                "ADD source {} is an archive that Docker would auto-extract; automatic \
                 extraction is not supported. COPY it to keep the archive as a file, or \
                 unpack it with a RUN instruction after copying",
                source.path.display()
            );
        }
    }
    Ok(())
}

struct BuildOutcome {
    snapshot_id: String,
}

async fn execute_build(
    client: &Client,
    sandbox_id: &str,
    prepared: &PreparedBuild,
    template_name: &str,
    capture_started: Arc<AtomicBool>,
) -> Result<BuildOutcome> {
    let transport = client.transport(sandbox_id)?;
    let files = client.files(sandbox_id)?;

    wait_for_envd(client, sandbox_id).await?;
    let keepalive = spawn_keepalive(client.clone(), sandbox_id.to_string());

    let result = execute_steps(
        client,
        sandbox_id,
        &transport,
        &files,
        prepared,
        template_name,
        &capture_started,
    )
    .await;
    keepalive.abort();
    result
}

async fn execute_steps(
    client: &Client,
    sandbox_id: &str,
    transport: &Transport,
    files: &EnvdFilesClient,
    prepared: &PreparedBuild,
    template_name: &str,
    capture_started: &AtomicBool,
) -> Result<BuildOutcome> {
    let base = probe_base(transport, files).await?;
    let mut state = EffectiveState::from_base(&base);

    let has_copy_steps = prepared
        .plan
        .steps
        .iter()
        .any(|step| matches!(step, BuildStep::Copy(_)));
    if has_copy_steps {
        ensure_guest_tar(transport).await?;
    }

    let total_steps = prepared.plan.steps.len() + 1;
    for (index, step) in prepared.plan.steps.iter().enumerate() {
        println!("[{}/{total_steps}] {}", index + 2, step.display());
        match step {
            BuildStep::Run { command } => {
                run_build_command(transport, command, &state).await?;
            }
            BuildStep::Env { pairs } => {
                for (key, value) in pairs {
                    state.set_env(key, value);
                }
            }
            BuildStep::Workdir { path } => {
                let resolved = resolve_workdir(&state.workdir, path);
                ensure_guest_directory(transport, &resolved).await?;
                state.workdir = resolved;
            }
            BuildStep::User { user } => {
                state.user = Some(user.clone());
                state.execution_user = Some(user.clone());
            }
            BuildStep::Copy(copy) => {
                let build_context = prepared
                    .build_context
                    .as_ref()
                    .expect("copy steps imply a loaded build context");
                execute_copy(
                    transport,
                    files,
                    build_context,
                    copy,
                    &prepared.copy_sources[index],
                    &state,
                    index,
                )
                .await?;
            }
        }
    }

    let request = capture_request(template_name, &prepared.plan, &base, &state);
    let mut startup = start_snapshot_startup(
        transport,
        request.start_cmd.as_deref(),
        &request.final_context,
        state.execution_user.as_deref(),
    )
    .await?;
    if let Some(process) = startup.as_mut() {
        if process.ensure_running_or_success().await? {
            startup = None;
        }
    }

    println!("Capturing snapshot...");
    // Snapshot publication is a blocking, non-cancellable operation. Once it
    // starts, Ctrl-C must wait for the result rather than report an
    // interruption while the snapshot may still be committed in the
    // background.
    capture_started.store(true, Ordering::Release);
    let snapshot = {
        let client = client.clone();
        let sandbox_id = sandbox_id.to_string();
        tokio::task::spawn_blocking(move || {
            client.create_snapshot_with_metadata(&sandbox_id, &request)
        })
        .await
        .context("snapshot capture task failed")??
    };
    // Keep the envd process stream alive until capture has completed. The
    // captured VM memory then contains the already-started process; startup
    // metadata alone is not responsible for launching it on resume.
    drop(startup);
    Ok(BuildOutcome {
        snapshot_id: snapshot.snapshot_id,
    })
}

/// Builds the capture request from the base context observable inside the
/// guest, overlaid with the Dockerfile's final state.
fn capture_request(
    template_name: &str,
    plan: &BuildPlan,
    _base: &BaseProbe,
    state: &EffectiveState,
) -> OwnedCaptureRequest {
    let final_context = SnapshotFinalContext {
        env_vars: state.env.clone(),
        workdir: state.workdir.clone(),
        user: state.user.clone(),
        entrypoint: plan.entrypoint.clone(),
        cmd: plan.cmd.clone(),
        ..SnapshotFinalContext::default()
    };
    let start_cmd = effective_start_cmd(&final_context);

    OwnedCaptureRequest {
        name: template_name.to_string(),
        final_context,
        ready_cmd: start_cmd.as_ref().map(|_| String::new()),
        start_cmd,
    }
}

struct OwnedCaptureRequest {
    name: String,
    final_context: SnapshotFinalContext,
    start_cmd: Option<String>,
    ready_cmd: Option<String>,
}

impl Client {
    fn create_snapshot_with_metadata(
        &self,
        sandbox_id: &str,
        request: &OwnedCaptureRequest,
    ) -> Result<crate::client::snapshots::SnapshotInfo> {
        self.create_snapshot_with_request(
            sandbox_id,
            &CreateSnapshotRequest {
                name: Some(&request.name),
                final_context: Some(&request.final_context),
                start_cmd: request.start_cmd.as_deref(),
                ready_cmd: request.ready_cmd.as_deref(),
            },
        )
    }
}

/// Client-side effective build state used by subsequent instructions.
struct EffectiveState {
    env: HashMap<String, String>,
    workdir: String,
    user: Option<String>,
    /// Per-request envd user after a Dockerfile USER instruction. Before
    /// that instruction, omit Basic auth so envd uses the sandbox's boot
    /// default exactly, including base-image user/group forms that cannot be
    /// represented as a Basic-auth username.
    execution_user: Option<String>,
}

impl EffectiveState {
    fn from_base(base: &BaseProbe) -> Self {
        Self {
            env: base.env.clone(),
            workdir: base.workdir.clone(),
            user: Some(base.user.clone()),
            execution_user: None,
        }
    }

    fn set_env(&mut self, key: &str, value: &str) {
        self.env.insert(key.to_string(), value.to_string());
    }

    fn env_map(&self) -> HashMap<String, String> {
        self.env.clone()
    }
}

struct BaseProbe {
    env: HashMap<String, String>,
    workdir: String,
    user: String,
}

/// Reads the base context that is observable inside the running build VM.
async fn probe_base(transport: &Transport, files: &EnvdFilesClient) -> Result<BaseProbe> {
    let env = files
        .default_envs()
        .await
        .context("reading base image environment from envd")?;

    let workdir_output = run_guest_command(transport, "pwd", GuestExec::default())
        .await
        .map_err(|err| {
            err.context(
                "probing the base image working directory failed; aenv build requires the \
                 base image to provide /bin/bash",
            )
        })?;
    if workdir_output.exit_code != 0 {
        bail!(
            "probing the base image working directory exited with status {}{}",
            workdir_output.exit_code,
            workdir_output.detail()
        );
    }
    let workdir = workdir_output.stdout.trim().to_string();
    if !workdir.starts_with('/') {
        bail!("base image working directory probe returned {workdir:?}");
    }

    let user_output = run_guest_command(
        transport,
        r#"printf '%s:%s\n' "$(id -u)" "$(id -g)""#,
        GuestExec::default(),
    )
    .await
    .context("probing the base image default user")?;
    if user_output.exit_code != 0 {
        bail!(
            "probing the base image default user exited with status {}{}",
            user_output.exit_code,
            user_output.detail()
        );
    }
    let user = user_output.stdout.trim().to_string();
    let valid_user = user
        .split_once(':')
        .is_some_and(|(uid, gid)| uid.parse::<u32>().is_ok() && gid.parse::<u32>().is_ok());
    if !valid_user {
        bail!("base image default user probe returned {user:?}");
    }

    Ok(BaseProbe { env, workdir, user })
}

fn effective_start_cmd(context: &SnapshotFinalContext) -> Option<String> {
    let parts = match &context.entrypoint {
        Some(entrypoint) => entrypoint,
        None => context.cmd.as_ref()?,
    };
    if parts.len() == 3 && parts[0] == "/bin/sh" && parts[1] == "-c" {
        return Some(parts[2].clone());
    }
    (!parts.is_empty()).then(|| {
        parts
            .iter()
            .map(|part| shell_util::shell_quote(part))
            .collect::<Vec<_>>()
            .join(" ")
    })
}

type ProcessStream = Pin<Box<dyn Stream<Item = Result<StartResponse>> + Send>>;

struct StartedProcess {
    pid: u32,
    monitor: tokio::task::JoinHandle<Result<StartupExit>>,
}

impl StartedProcess {
    /// Mirror the existing template-builder behavior: a startup command may
    /// complete successfully, but an immediate failure must stop the build.
    async fn ensure_running_or_success(&mut self) -> Result<bool> {
        match tokio::time::timeout(Duration::from_millis(1), &mut self.monitor).await {
            Err(_) => Ok(false),
            Ok(joined) => {
                let exit = joined.context("template command monitor task failed")??;
                if exit.exit_code == 0 {
                    return Ok(true);
                }
                bail!(
                    "template command (pid {}) exited with status {} before snapshot capture{}",
                    self.pid,
                    exit.exit_code,
                    exit.detail()
                );
            }
        }
    }
}

impl Drop for StartedProcess {
    fn drop(&mut self) {
        self.monitor.abort();
    }
}

struct StartupExit {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    error: Option<String>,
}

impl StartupExit {
    fn detail(&self) -> String {
        if let Some(error) = self.error.as_deref().filter(|error| !error.is_empty()) {
            return format!(": {error}");
        }
        let stderr = String::from_utf8_lossy(&self.stderr);
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            return format!(": {stderr}");
        }
        let stdout = String::from_utf8_lossy(&self.stdout);
        let stdout = stdout.trim();
        if !stdout.is_empty() {
            return format!(": {stdout}");
        }
        String::new()
    }
}

/// Start the final template command before capture. Snapshot launch resumes
/// the process from VM memory; merely storing startup metadata does not start
/// it when a captured sandbox is resumed.
async fn start_snapshot_startup(
    transport: &Transport,
    start_cmd: Option<&str>,
    context: &SnapshotFinalContext,
    execution_user: Option<&str>,
) -> Result<Option<StartedProcess>> {
    let Some(start_cmd) = start_cmd else {
        return Ok(None);
    };

    println!("Starting template command...");
    let request = build_start_request(StartOpts {
        cmd: "/bin/bash",
        args: vec!["-lc".to_string(), start_cmd.to_string()],
        envs: context.env_vars.clone(),
        pty: None,
        stdin: false,
        cwd: Some(context.workdir.clone()),
    });
    let mut stream = transport
        .server_stream::<_, StartResponse>("Start", request, execution_user)
        .await
        .with_context(|| format!("starting template command {start_cmd:?}"))?;

    while let Some(message) = stream.next().await {
        let message = message.context("reading template command start event")?;
        let Some(event) = message.event.and_then(|wrapper| wrapper.event) else {
            continue;
        };
        match event {
            process_event::Event::Start(start) => {
                let monitor = tokio::spawn(drain_startup_stream(stream));
                return Ok(Some(StartedProcess {
                    pid: start.pid,
                    monitor,
                }));
            }
            process_event::Event::End(end) => {
                bail!(
                    "template command exited with status {} before it started{}",
                    end.exit_code,
                    end.error
                        .as_deref()
                        .map(|error| format!(": {error}"))
                        .unwrap_or_default()
                );
            }
            process_event::Event::Data(_) | process_event::Event::Keepalive(_) => {}
        }
    }
    bail!("template command stream ended before the process start event")
}

/// Continuously consume the startup stream through snapshot capture. This
/// prevents a chatty process from filling HTTP/2 flow-control buffers and
/// records terminal output if it exits before capture begins.
async fn drain_startup_stream(mut stream: ProcessStream) -> Result<StartupExit> {
    const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

    fn extend_bounded(target: &mut Vec<u8>, source: &[u8]) {
        let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(target.len());
        target.extend_from_slice(&source[..source.len().min(remaining)]);
    }

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(message) = stream.next().await {
        let message = message.context("reading template command output")?;
        let Some(event) = message.event.and_then(|wrapper| wrapper.event) else {
            continue;
        };
        match event {
            process_event::Event::Data(data) => match data.output {
                Some(process_event::data_event::Output::Stdout(bytes)) => {
                    extend_bounded(&mut stdout, &bytes);
                }
                Some(process_event::data_event::Output::Stderr(bytes)) => {
                    extend_bounded(&mut stderr, &bytes);
                }
                _ => {}
            },
            process_event::Event::End(end) => {
                return Ok(StartupExit {
                    exit_code: end.exit_code,
                    stdout,
                    stderr,
                    error: end.error,
                });
            }
            process_event::Event::Start(_) | process_event::Event::Keepalive(_) => {}
        }
    }
    bail!("template command stream ended without an exit event")
}

async fn ensure_guest_tar(transport: &Transport) -> Result<()> {
    let output = run_guest_command(transport, "command -v tar", GuestExec::default()).await?;
    if output.exit_code != 0 {
        bail!(
            "the base image does not provide tar, which COPY/ADD support requires; install \
             tar in the base image or remove the COPY/ADD instructions"
        );
    }
    Ok(())
}

async fn run_build_command(
    transport: &Transport,
    command: &str,
    state: &EffectiveState,
) -> Result<()> {
    // The inner login shell preserves the existing RUN semantics. The outer
    // shell gives it a dedicated process group and removes ordinary
    // background descendants before returning.
    let request = build_start_request(StartOpts {
        cmd: "/bin/bash",
        args: vec![
            "-c".to_string(),
            RUN_PROCESS_GROUP_WRAPPER.to_string(),
            "aenv-build-run".to_string(),
            command.to_string(),
        ],
        envs: state.env_map(),
        pty: None,
        stdin: false,
        cwd: Some(state.workdir.clone()),
    });
    let stream = transport
        .server_stream::<_, StartResponse>("Start", request, state.execution_user.as_deref())
        .await
        .context("starting RUN command")?;
    let exit_code = drain_output(stream).await?;
    if exit_code != 0 {
        bail!("RUN exited with status {exit_code}");
    }
    Ok(())
}

async fn ensure_guest_directory(transport: &Transport, path: &str) -> Result<()> {
    // Like Docker, WORKDIR directories (and COPY destinations) are created
    // as root regardless of the active USER.
    let command = format!("mkdir -p -- {}", shell_util::shell_quote(path));
    let output = run_guest_command(
        transport,
        &command,
        GuestExec {
            username: Some(GUEST_ROOT),
        },
    )
    .await?;
    if output.exit_code != 0 {
        bail!(
            "creating directory {path} exited with status {}{}",
            output.exit_code,
            output.detail()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute_copy(
    transport: &Transport,
    files: &EnvdFilesClient,
    build_context: &BuildContext,
    copy: &CopyStep,
    sources: &[SelectedSource],
    state: &EffectiveState,
    step_index: usize,
) -> Result<()> {
    let (dest, wants_directory) = context::resolve_guest_dest(&copy.dest, &state.workdir)
        .with_context(|| format!("invalid {} destination", copy.instruction))?;

    // Destinations without a trailing slash are resolved against the guest
    // before transfer: an existing directory means copy-into, anything else
    // is handled as a file target or rejected.
    let stat = files
        .stat(&dest, Some(GUEST_ROOT))
        .await
        .with_context(|| format!("resolving {} destination {dest}", copy.instruction))?;
    let kind = match stat {
        Some(entry) if entry.r#type == FileType::Directory as i32 => GuestPathKind::Directory,
        Some(_) => GuestPathKind::File,
        None => GuestPathKind::Missing,
    };
    let dest_plan =
        context::plan_destination(copy.instruction, sources, &dest, wants_directory, kind)?;
    let entries = context::transfer_entries(build_context, sources, &dest_plan)?;
    if entries.is_empty() {
        // Only empty source directories produce no entries; the destination
        // directory itself must still exist.
        ensure_guest_directory(transport, dest_plan.extraction_root()).await?;
        return Ok(());
    }

    let (archive, archive_size) =
        tokio::task::spawn_blocking(move || -> Result<(tempfile::NamedTempFile, u64)> {
            let file = tempfile::NamedTempFile::new().context("creating local staging file")?;
            let size = context::pack_transfer(
                &entries,
                file.reopen().context("opening local staging file")?,
            )?;
            Ok((file, size))
        })
        .await
        .context("context packaging task failed")??;

    let guest_archive = format!(
        "/var/tmp/.aenv-build-{}-{}.tar",
        std::process::id(),
        step_index
    );
    let progress = TransferProgress::new("Uploading context", archive_size)?;
    progress.set_message(format!("{} {}", copy.instruction, copy.dest));
    let upload = files
        .upload(archive.path(), &guest_archive, Some(GUEST_ROOT), &progress)
        .await;
    if upload.is_ok() {
        progress.finish();
    } else {
        progress.abandon();
    }
    upload.with_context(|| format!("uploading {} content", copy.instruction))?;

    // Interpretation and extraction happen inside the guest, as root; the
    // staged archive is removed in the same script so no temporary content
    // survives into the captured snapshot.
    let root = shell_util::shell_quote(dest_plan.extraction_root());
    let staged = shell_util::shell_quote(&guest_archive);
    let script =
        format!("set -e; mkdir -p -- {root}; tar -xpf {staged} -C {root}; rm -f -- {staged}");
    let output = run_guest_command(
        transport,
        &script,
        GuestExec {
            username: Some(GUEST_ROOT),
        },
    )
    .await?;
    if output.exit_code != 0 {
        bail!(
            "extracting {} content into {dest} exited with status {}{}",
            copy.instruction,
            output.exit_code,
            output.detail()
        );
    }
    Ok(())
}

#[derive(Default)]
struct GuestExec<'a> {
    username: Option<&'a str>,
}

struct GuestCommandOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl GuestCommandOutput {
    fn detail(&self) -> String {
        let mut out = String::new();
        let stderr = self.stderr.trim();
        let stdout = self.stdout.trim();
        if !stderr.is_empty() {
            out.push_str(&format!(": {stderr}"));
        } else if !stdout.is_empty() {
            out.push_str(&format!(": {stdout}"));
        }
        out
    }
}

/// Runs a helper command in the guest via `/bin/bash -c`, capturing output.
/// Build RUN steps use `run_build_command` instead, which streams output to
/// the terminal.
async fn run_guest_command(
    transport: &Transport,
    command: &str,
    exec: GuestExec<'_>,
) -> Result<GuestCommandOutput> {
    let request = build_start_request(StartOpts {
        cmd: "/bin/bash",
        args: vec!["-c".to_string(), command.to_string()],
        envs: HashMap::new(),
        pty: None,
        stdin: false,
        cwd: None,
    });
    let mut stream = transport
        .server_stream::<_, StartResponse>("Start", request, exec.username)
        .await?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(message) = stream.next().await {
        let message = message?;
        let Some(event) = message.event.and_then(|wrapper| wrapper.event) else {
            continue;
        };
        match event {
            process_event::Event::Data(data) => match data.output {
                Some(process_event::data_event::Output::Stdout(bytes)) => {
                    stdout.extend_from_slice(&bytes);
                }
                Some(process_event::data_event::Output::Stderr(bytes)) => {
                    stderr.extend_from_slice(&bytes);
                }
                _ => {}
            },
            process_event::Event::End(end) => {
                return Ok(GuestCommandOutput {
                    exit_code: end.exit_code,
                    stdout: String::from_utf8_lossy(&stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                });
            }
            _ => {}
        }
    }
    bail!("guest command stream ended without an exit event")
}

/// Resolves a WORKDIR value the way Docker records it: relative paths join
/// the current workdir and `.`/`..` resolve lexically.
fn resolve_workdir(current: &str, path: &str) -> String {
    let base = if current.is_empty() { "/" } else { current };
    let joined = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), path)
    };
    let mut segments: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

async fn wait_for_envd(client: &Client, sandbox_id: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + ENVD_READY_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if matches!(
            client
                .envd_ready_with_timeout(sandbox_id, ENVD_READY_PROBE_TIMEOUT)
                .await,
            Ok(true)
        ) {
            return Ok(());
        }
        tokio::time::sleep(ENVD_READY_PROBE_INTERVAL).await;
    }
    bail!(
        "build sandbox {sandbox_id} envd not healthy within {}s",
        ENVD_READY_TIMEOUT.as_secs()
    )
}

/// Periodically refreshes the build sandbox TTL for the lifetime of the
/// build, including while long RUN steps or uploads are in flight. Refresh
/// failures are logged and retried; if the sandbox is really gone, the next
/// build operation fails with the underlying error.
fn spawn_keepalive(client: Client, sandbox_id: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(KEEPALIVE_INTERVAL).await;
            let refresh_client = client.clone();
            let refresh_id = sandbox_id.clone();
            let refreshed = tokio::task::spawn_blocking(move || {
                refresh_client.refresh_sandbox(&refresh_id, Some(BUILD_SESSION_TTL_SECS))
            })
            .await;
            match refreshed {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    eprintln!("warning: build sandbox keepalive failed: {err:#}");
                }
                Err(_) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{capture_request, effective_start_cmd, resolve_workdir, BaseProbe, EffectiveState};
    use crate::client::snapshots::SnapshotFinalContext;
    use clap::Parser;
    use std::collections::HashMap;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: super::Args,
    }

    #[test]
    fn build_requires_name_flag() {
        let cli = TestCli::try_parse_from(["test", ".", "--name", "my-template"])
            .expect("--name should be accepted");
        assert_eq!(cli.args.name, "my-template");
        assert_eq!(cli.args.context, std::path::PathBuf::from("."));

        assert!(TestCli::try_parse_from(["test", "."]).is_err());
    }

    #[test]
    fn build_accepts_dockerfile_flag_and_defaults_context() {
        let cli =
            TestCli::try_parse_from(["test", "--name", "my-template", "-f", "./Dockerfile.prod"])
                .unwrap();
        assert_eq!(cli.args.context, std::path::PathBuf::from("."));
        assert_eq!(
            cli.args.dockerfile,
            Some(std::path::PathBuf::from("./Dockerfile.prod"))
        );
    }

    #[test]
    fn resolve_workdir_matches_docker_semantics() {
        assert_eq!(resolve_workdir("/", "/app"), "/app");
        assert_eq!(resolve_workdir("/app", "src"), "/app/src");
        assert_eq!(resolve_workdir("/app/src", ".."), "/app");
        assert_eq!(resolve_workdir("/app", "./x/./y"), "/app/x/y");
        assert_eq!(resolve_workdir("", "relative"), "/relative");
        assert_eq!(resolve_workdir("/a", "../../.."), "/");
    }

    #[test]
    fn capture_request_merges_probed_base_env_with_dockerfile_context() {
        let plan = super::plan::parse_build_plan(
            "FROM ubuntu:24.04\nENV APP=1 PATH=/custom\nENTRYPOINT [\"/bin/app\"]\n",
            None,
        )
        .unwrap();
        let base = BaseProbe {
            env: HashMap::from([
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("LANG".to_string(), "C.UTF-8".to_string()),
            ]),
            workdir: "/".to_string(),
            user: "1000:1000".to_string(),
        };
        let mut state = EffectiveState::from_base(&base);
        state.set_env("APP", "1");
        state.set_env("PATH", "/custom");
        state.workdir = "/app".to_string();

        let request = capture_request("my-template", &plan, &base, &state);

        assert_eq!(request.name, "my-template");
        // Base image env survives; Dockerfile ENV overrides on conflict.
        let env = &request.final_context.env_vars;
        assert_eq!(env.get("LANG").map(String::as_str), Some("C.UTF-8"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/custom"));
        assert_eq!(env.get("APP").map(String::as_str), Some("1"));
        assert_eq!(request.final_context.workdir, "/app");
        assert_eq!(request.final_context.user.as_deref(), Some("1000:1000"));
        assert_eq!(
            state.execution_user, None,
            "the base user/group must remain envd's boot default until USER overrides it"
        );
        assert_eq!(
            request.final_context.entrypoint,
            Some(vec!["/bin/app".to_string()])
        );
        assert_eq!(request.final_context.cmd, None);
        assert_eq!(request.start_cmd.as_deref(), Some("/bin/app"));
        assert_eq!(request.ready_cmd.as_deref(), Some(""));

        // A Dockerfile USER wins over the base user.
        state.user = Some("builder".to_string());
        let request = capture_request("my-template", &plan, &base, &state);
        assert_eq!(request.final_context.user.as_deref(), Some("builder"));
    }

    #[test]
    fn effective_start_cmd_keeps_entrypoint_over_cmd_precedence() {
        let context = SnapshotFinalContext {
            entrypoint: Some(vec!["/bin/sh".to_string(), "-c".to_string()]),
            cmd: Some(vec!["echo ignored".to_string()]),
            ..SnapshotFinalContext::default()
        };
        assert_eq!(effective_start_cmd(&context).as_deref(), Some("/bin/sh -c"));

        let context = SnapshotFinalContext {
            cmd: Some(vec!["python3".to_string(), "app.py".to_string()]),
            ..SnapshotFinalContext::default()
        };
        assert_eq!(
            effective_start_cmd(&context).as_deref(),
            Some("python3 app.py")
        );

        let context = SnapshotFinalContext {
            cmd: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf '%s\\n' 'shell form'".to_string(),
            ]),
            ..SnapshotFinalContext::default()
        };
        assert_eq!(
            effective_start_cmd(&context).as_deref(),
            Some("printf '%s\\n' 'shell form'")
        );
    }

    #[test]
    fn effective_state_env_updates_override_base_values() {
        let mut state = EffectiveState::from_base(&BaseProbe {
            env: HashMap::from([("A".to_string(), "base".to_string())]),
            workdir: "/".to_string(),
            user: "0:0".to_string(),
        });
        state.set_env("A", "1");
        state.set_env("B", "2");
        state.set_env("A", "3");

        assert_eq!(state.env.get("A").map(String::as_str), Some("3"));
        assert_eq!(state.env.get("B").map(String::as_str), Some("2"));
        assert_eq!(state.env_map().get("A").map(String::as_str), Some("3"));
    }
}
