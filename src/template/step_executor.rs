use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use shell_util::shell_quote;
use tracing::debug;

use super::build_spec::{TemplateBuildStep, TemplateBuildStepKind};
use super::copy_plan::plan_copy_archive;
use super::errors::{command_output_suffix, TemplateBuildFailure};
use crate::sandbox::{ProcessOpts, SandboxExecutor};
use crate::snapshot::CommandContext;

/// Validates a Docker-style `--chown` value (`user`, `uid`, `user:group`).
///
/// The value ends up in a shell command inside the build sandbox (quoted), so
/// this stays conservative rather than mirroring every libc name rule.
fn is_valid_chown_spec(spec: &str) -> bool {
    fn valid_part(part: &str) -> bool {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    }
    let mut parts = spec.split(':');
    let (user, group, extra) = (parts.next(), parts.next(), parts.next());
    if extra.is_some() {
        return false;
    }
    match (user, group) {
        (Some(user), None) => valid_part(user),
        (Some(user), Some(group)) => valid_part(user) && valid_part(group),
        _ => false,
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TemplateStepExecutor;

impl TemplateStepExecutor {
    pub(crate) fn new() -> Self {
        Self
    }

    #[tracing::instrument(
        skip(self, sandbox, steps, initial_context, build_archives),
        fields(step_count = steps.len())
    )]
    pub(crate) async fn execute(
        &self,
        sandbox: &impl SandboxExecutor,
        steps: &[TemplateBuildStep],
        initial_context: CommandContext,
        build_archives: &HashMap<String, PathBuf>,
    ) -> Result<CommandContext> {
        let mut context = initial_context;

        debug!("executing template build steps");
        for step in steps {
            match &step.kind {
                TemplateBuildStepKind::Env { key, value } => {
                    context = context.with_env_var(key.clone(), value.clone());
                }
                TemplateBuildStepKind::Workdir { path } => {
                    context = context.with_workdir(path.to_string_lossy());
                }
                TemplateBuildStepKind::User { value } => {
                    context = context.with_user(Some(value.clone()));
                }
                TemplateBuildStepKind::ExposedPort { port } => {
                    let mut ports = context.exposed_ports.clone();
                    if !ports.contains(port) {
                        ports.push(port.clone());
                    }
                    context = context.with_exposed_ports(ports);
                }
                TemplateBuildStepKind::Volume { path } => {
                    let mut volumes = context.volumes.clone();
                    if !volumes.contains(path) {
                        volumes.push(path.clone());
                    }
                    context = context.with_volumes(volumes);
                }
                TemplateBuildStepKind::Label { key, value } => {
                    let mut labels = context.labels.clone();
                    labels.insert(key.clone(), value.clone());
                    context = context.with_labels(labels);
                }
                TemplateBuildStepKind::Run { cmd } => {
                    self.run_step(sandbox, &context.workdir, &context.env_vars, cmd)
                        .await?;
                }
                TemplateBuildStepKind::Copy {
                    src,
                    dest,
                    files_hash,
                    user,
                    mode,
                } => {
                    self.copy_step(
                        sandbox,
                        &context,
                        build_archives,
                        src,
                        dest,
                        files_hash,
                        user.as_deref(),
                        *mode,
                    )
                    .await?;
                }
            }
        }
        debug!("template build steps completed");

        Ok(context)
    }

    /// Applies one COPY step: rewrites the uploaded context archive to final
    /// absolute guest paths on the host, streams it into the sandbox via
    /// envd, and extracts it at `/` inside the guest.
    #[allow(clippy::too_many_arguments)]
    async fn copy_step(
        &self,
        sandbox: &impl SandboxExecutor,
        context: &CommandContext,
        build_archives: &HashMap<String, PathBuf>,
        src: &str,
        dest: &str,
        files_hash: &str,
        user: Option<&str>,
        mode: Option<u32>,
    ) -> Result<()> {
        let step_label = format!("COPY {src} {dest}");
        let with_step = |message: String| TemplateBuildFailure::with_step(message, &step_label);

        let archive = build_archives.get(files_hash).ok_or_else(|| {
            with_step(format!(
                "build step failed: build context archive '{files_hash}' has not been uploaded"
            ))
        })?;

        if let Some(user) = user {
            if !is_valid_chown_spec(user) {
                return Err(
                    with_step(format!("build step failed: invalid COPY user '{user}'")).into(),
                );
            }
        }

        let rewritten = tempfile::Builder::new()
            .prefix("agentenv-copy-")
            .suffix(".tar")
            .tempfile()
            .context("create rewritten copy archive")?;
        let plan = plan_copy_archive(archive, src, dest, &context.workdir, mode, rewritten.path())
            .map_err(|error| with_step(format!("build step failed: {error:#}")))?;
        debug!(
            files_hash,
            entries = plan.entry_count,
            roots = ?plan.created_roots,
            "prepared copy archive"
        );

        let guest_archive = format!("/tmp/.agentenv-copy-{}.tar", uuid::Uuid::new_v4());
        sandbox
            .upload_file(rewritten.path(), &guest_archive, "root")
            .await
            .with_context(|| with_step("build step failed: upload build context".to_string()))?;

        let mut script = format!(
            "tar -xpf {archive} -C /\nrc=$?\nrm -f {archive}\nif [ $rc -ne 0 ]; then exit $rc; fi\n",
            archive = shell_quote(&guest_archive),
        );
        if let Some(user) = user {
            let roots = plan
                .created_roots
                .iter()
                .map(|root| shell_quote(root))
                .collect::<Vec<_>>()
                .join(" ");
            if !roots.is_empty() {
                script.push_str(&format!("chown -R {} {roots}\n", shell_quote(user)));
            }
        }

        let output = sandbox
            .run_command_with_opts("/bin/bash", &["-lc", &script], &ProcessOpts::default())
            .await
            .with_context(|| with_step("build step failed".to_string()))?;
        if output.exit_code != 0 {
            let message = format!(
                "build step failed: extracting the build context exited with status {}{}",
                output.exit_code,
                command_output_suffix(&output.stdout, &output.stderr)
            );
            return Err(with_step(message).into());
        }
        Ok(())
    }

    async fn run_step(
        &self,
        sandbox: &impl SandboxExecutor,
        workdir: &str,
        env: &HashMap<String, String>,
        cmd: &str,
    ) -> Result<()> {
        let opts = ProcessOpts {
            envs: env.clone(),
            cwd: Some(workdir.to_string()),
            ..ProcessOpts::default()
        };

        let output = sandbox
            .run_command_with_opts("/bin/bash", &["-lc", cmd], &opts)
            .await
            .with_context(|| {
                TemplateBuildFailure::with_step("build step failed", format!("RUN {cmd}"))
            })?;
        if output.exit_code != 0 {
            let message = format!(
                "build step failed: command exited with status {}{}",
                output.exit_code,
                command_output_suffix(&output.stdout, &output.stderr)
            );
            return Err(TemplateBuildFailure::with_step(message, format!("RUN {cmd}")).into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anyhow::{anyhow, Result};
    use async_trait::async_trait;

    use super::TemplateStepExecutor;
    use crate::sandbox::{Executor, ProcessHandle, ProcessOpts, ProcessOutput, SandboxExecutor};
    use crate::snapshot::CommandContext;
    use crate::template::build_spec::TemplateBuildStep;

    struct NoopSandbox;

    #[async_trait(?Send)]
    impl SandboxExecutor for NoopSandbox {
        fn executor(&self) -> Result<Executor<'_>> {
            Err(anyhow!("not used"))
        }
        async fn run_command_with_opts(
            &self,
            _cmd: &str,
            _args: &[&str],
            _opts: &ProcessOpts,
        ) -> Result<ProcessOutput> {
            Err(anyhow!("not used"))
        }
        async fn start_process(
            &self,
            _cmd: &str,
            _args: &[&str],
            _opts: &ProcessOpts,
        ) -> Result<ProcessHandle> {
            Err(anyhow!("not used"))
        }
    }

    async fn run(steps: Vec<TemplateBuildStep>) -> CommandContext {
        TemplateStepExecutor::new()
            .execute(
                &NoopSandbox,
                &steps,
                CommandContext::default(),
                &HashMap::new(),
            )
            .await
            .expect("steps should execute without error")
    }

    #[tokio::test]
    async fn user_step_sets_context_user() {
        let ctx = run(vec![TemplateBuildStep::user("zzz")]).await;
        assert_eq!(ctx.user.as_deref(), Some("zzz"));
    }

    #[tokio::test]
    async fn user_step_overrides_base_image_user() {
        let initial = CommandContext::default().with_user(Some("root".to_string()));
        let ctx = TemplateStepExecutor::new()
            .execute(
                &NoopSandbox,
                &[TemplateBuildStep::user("zzz")],
                initial,
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(ctx.user.as_deref(), Some("zzz"));
    }

    #[tokio::test]
    async fn exposed_port_deduplicates() {
        let ctx = run(vec![
            TemplateBuildStep::exposed_port("8080"),
            TemplateBuildStep::exposed_port("8080"),
            TemplateBuildStep::exposed_port("443"),
        ])
        .await;
        assert_eq!(ctx.exposed_ports, vec!["8080", "443"]);
    }

    #[tokio::test]
    async fn exposed_port_from_base_image_is_not_duplicated() {
        let initial = CommandContext::default().with_exposed_ports(vec!["8080".to_string()]);
        let ctx = TemplateStepExecutor::new()
            .execute(
                &NoopSandbox,
                &[TemplateBuildStep::exposed_port("8080")],
                initial,
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(ctx.exposed_ports, vec!["8080"]);
    }

    #[tokio::test]
    async fn copy_step_fails_without_uploaded_archive() {
        let err = TemplateStepExecutor::new()
            .execute(
                &NoopSandbox,
                &[TemplateBuildStep::copy(
                    "hello.txt",
                    "/hello.txt",
                    "aabbccddeeff0011",
                    None,
                    None,
                )],
                CommandContext::default(),
                &HashMap::new(),
            )
            .await
            .expect_err("missing archive should fail the step");
        assert!(err.to_string().contains("has not been uploaded"));
    }

    #[test]
    fn chown_spec_validation() {
        assert!(super::is_valid_chown_spec("user"));
        assert!(super::is_valid_chown_spec("user:group"));
        assert!(super::is_valid_chown_spec("1000:1000"));
        assert!(super::is_valid_chown_spec("www-data"));
        assert!(!super::is_valid_chown_spec(""));
        assert!(!super::is_valid_chown_spec("user:"));
        assert!(!super::is_valid_chown_spec("user:group:extra"));
        assert!(!super::is_valid_chown_spec("user name"));
        assert!(!super::is_valid_chown_spec("user;rm -rf /"));
    }

    #[tokio::test]
    async fn volume_deduplicates() {
        let ctx = run(vec![
            TemplateBuildStep::volume("/data"),
            TemplateBuildStep::volume("/data"),
            TemplateBuildStep::volume("/logs"),
        ])
        .await;
        assert_eq!(ctx.volumes, vec!["/data", "/logs"]);
    }

    #[tokio::test]
    async fn env_step_sets_env_var() {
        let ctx = run(vec![TemplateBuildStep::env("FOO", "bar")]).await;
        assert_eq!(ctx.env_vars.get("FOO").map(String::as_str), Some("bar"));
    }

    #[tokio::test]
    async fn workdir_step_updates_workdir() {
        let ctx = run(vec![TemplateBuildStep::workdir("/workspace")]).await;
        assert_eq!(ctx.workdir, "/workspace");
    }
}
