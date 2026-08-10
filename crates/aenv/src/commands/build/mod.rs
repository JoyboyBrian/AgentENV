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
use futures::StreamExt;
use plan::{BuildPlan, BuildStep, CopyStep};
use std::collections::HashMap;
use std::path::PathBuf;
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

    let rt = super::tokio_rt()?;
    let build_result = rt.block_on(async {
        tokio::select! {
            result = execute_build(&client, &sandbox_id, &prepared, &args.name) => result,
            _ = tokio::signal::ctrl_c() => Err(anyhow!("build interrupted")),
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
) -> Result<BuildOutcome> {
    let base = probe_base(transport, files).await?;
    let has_copy_steps = prepared
        .plan
        .steps
        .iter()
        .any(|step| matches!(step, BuildStep::Copy(_)));
    if has_copy_steps {
        ensure_guest_tar(transport).await?;
    }

    let mut state = EffectiveState {
        env: Vec::new(),
        workdir: base.workdir.clone(),
        user: None,
    };

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

    println!("Capturing snapshot...");
    let request = capture_request(template_name, &prepared.plan, &base, &state);
    let snapshot = {
        let client = client.clone();
        let sandbox_id = sandbox_id.to_string();
        tokio::task::spawn_blocking(move || {
            client.create_snapshot_with_metadata(&sandbox_id, &request)
        })
        .await
        .context("snapshot capture task failed")??
    };
    Ok(BuildOutcome {
        snapshot_id: snapshot.snapshot_id,
    })
}

/// Builds the capture request: the final context is the probed base context
/// overlaid with the Dockerfile's ENV/WORKDIR/USER results, and the startup
/// command keeps the existing ENTRYPOINT/CMD derivation.
fn capture_request(
    template_name: &str,
    plan: &BuildPlan,
    base: &BaseProbe,
    state: &EffectiveState,
) -> OwnedCaptureRequest {
    let mut env_vars = base.env.clone();
    for (key, value) in &state.env {
        env_vars.insert(key.clone(), value.clone());
    }
    let user = state
        .user
        .clone()
        .or_else(|| (!base.user.is_empty()).then(|| base.user.clone()));
    OwnedCaptureRequest {
        name: template_name.to_string(),
        final_context: SnapshotFinalContext {
            env_vars,
            workdir: state.workdir.clone(),
            user,
            entrypoint: plan.entrypoint.clone(),
            cmd: plan.cmd.clone(),
        },
        start_cmd: plan.start_cmd.clone(),
    }
}

struct OwnedCaptureRequest {
    name: String,
    final_context: SnapshotFinalContext,
    start_cmd: Option<String>,
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
            },
        )
    }
}

/// Client-side effective build state: accumulated ENV pairs (in first-set
/// order), the resolved working directory, and the active USER.
struct EffectiveState {
    env: Vec<(String, String)>,
    workdir: String,
    user: Option<String>,
}

impl EffectiveState {
    fn set_env(&mut self, key: &str, value: &str) {
        if let Some(existing) = self.env.iter_mut().find(|(name, _)| name == key) {
            existing.1 = value.to_string();
        } else {
            self.env.push((key.to_string(), value.to_string()));
        }
    }

    fn env_map(&self) -> HashMap<String, String> {
        self.env.iter().cloned().collect()
    }
}

struct BaseProbe {
    env: HashMap<String, String>,
    workdir: String,
    user: String,
}

/// Reads the base image context from the running build VM: envd's default
/// environment (`GET /envs`, the values injected from the image config) plus
/// the effective default working directory and user.
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

    let user_output = run_guest_command(transport, "id -un", GuestExec::default())
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

    Ok(BaseProbe { env, workdir, user })
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
    // Login shell, explicit env and cwd — matching the server-side template
    // builder. The active USER is carried per-request as the envd execution
    // user, exactly like the unary path.
    let request = build_start_request(StartOpts {
        cmd: "/bin/bash",
        args: vec!["-lc".to_string(), command.to_string()],
        envs: state.env_map(),
        pty: None,
        stdin: false,
        cwd: Some(state.workdir.clone()),
    });
    let stream = transport
        .server_stream::<_, StartResponse>("Start", request, state.user.as_deref())
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
    use super::{resolve_workdir, EffectiveState};
    use clap::Parser;

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
    fn capture_request_merges_base_env_and_dockerfile_env() {
        let plan = super::plan::parse_build_plan(
            "FROM ubuntu:24.04\nENV APP=1 PATH=/custom\nENTRYPOINT [\"/bin/app\"]\n",
            None,
        )
        .unwrap();
        let base = super::BaseProbe {
            env: std::collections::HashMap::from([
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("LANG".to_string(), "C.UTF-8".to_string()),
            ]),
            workdir: "/".to_string(),
            user: "root".to_string(),
        };
        let mut state = EffectiveState {
            env: Vec::new(),
            workdir: "/app".to_string(),
            user: None,
        };
        state.set_env("APP", "1");
        state.set_env("PATH", "/custom");

        let request = super::capture_request("my-template", &plan, &base, &state);

        assert_eq!(request.name, "my-template");
        // Base image env survives; Dockerfile ENV overrides on conflict.
        let env = &request.final_context.env_vars;
        assert_eq!(env.get("LANG").map(String::as_str), Some("C.UTF-8"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/custom"));
        assert_eq!(env.get("APP").map(String::as_str), Some("1"));
        assert_eq!(request.final_context.workdir, "/app");
        // No Dockerfile USER: the probed base user is captured explicitly.
        assert_eq!(request.final_context.user.as_deref(), Some("root"));
        assert_eq!(
            request.final_context.entrypoint,
            Some(vec!["/bin/app".to_string()])
        );
        assert_eq!(request.start_cmd.as_deref(), Some("/bin/app"));

        // A Dockerfile USER wins over the probed base user.
        state.user = Some("builder".to_string());
        let request = super::capture_request("my-template", &plan, &base, &state);
        assert_eq!(request.final_context.user.as_deref(), Some("builder"));
    }

    #[test]
    fn effective_state_env_updates_preserve_first_set_order() {
        let mut state = EffectiveState {
            env: Vec::new(),
            workdir: "/".into(),
            user: None,
        };
        state.set_env("A", "1");
        state.set_env("B", "2");
        state.set_env("A", "3");

        assert_eq!(
            state.env,
            vec![("A".into(), "3".into()), ("B".into(), "2".into())]
        );
        assert_eq!(state.env_map().get("A").map(String::as_str), Some("3"));
    }
}
