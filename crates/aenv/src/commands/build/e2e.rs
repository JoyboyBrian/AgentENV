//! End-to-end tests for the native build workflow against a live AgentENV
//! server (Linux/KVM). These are `#[ignore]`d by default and gated on
//! environment variables:
//!
//! ```bash
//! AENV_E2E_URL=http://<server> AENV_E2E_API_KEY=<key> \
//!     cargo test -p aenv -- --ignored e2e_native_build
//! ```
//!
//! The default base image (`ubuntu:24.04`) can be overridden with
//! `AENV_E2E_BASE_IMAGE` for air-gapped registries.

use super::{run_with_client, Args};
use crate::client::Client;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn e2e_client() -> Result<Client> {
    let url = std::env::var("AENV_E2E_URL")
        .context("set AENV_E2E_URL to run the native build end-to-end test")?;
    let api_key = std::env::var("AENV_E2E_API_KEY")
        .context("set AENV_E2E_API_KEY to run the native build end-to-end test")?;
    Client::new(&url, &api_key)
}

fn base_image() -> String {
    std::env::var("AENV_E2E_BASE_IMAGE").unwrap_or_else(|_| "ubuntu:24.04".to_string())
}

fn unique_name(prefix: &str) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis();
    format!("{prefix}-{stamp}-{}", std::process::id())
}

fn build_args(context: &Path, name: &str) -> Args {
    Args {
        context: context.to_path_buf(),
        dockerfile: None,
        name: name.to_string(),
        resources: crate::commands::CpuMemoryArgs::default(),
        disk_size_mb: None,
        user_image: None,
    }
}

fn write_context(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir.join("data/nested"))?;
    fs::write(dir.join("data/file.txt"), "copied-file\n")?;
    fs::write(dir.join("data/nested/deep.txt"), "deep\n")?;
    fs::write(dir.join("data/secret.env"), "SECRET=1\n")?;
    fs::write(dir.join("config.json"), "{\"ok\":true}\n")?;
    fs::write(dir.join("single.txt"), "single\n")?;
    fs::write(dir.join("extra.txt"), "extra\n")?;
    fs::write(dir.join(".dockerignore"), "**/*.env\n")?;
    fs::write(
        dir.join("Dockerfile"),
        r#"FROM ubuntu:24.04
ENV BUILD_MARKER=native-build
RUN ["bash", "-c", "echo exec-form-ok > /exec-form.txt"]
RUN <<EOF
set -eu
echo heredoc-ok > /heredoc.txt
EOF
RUN useradd -m builder
WORKDIR /workspace/app
COPY data ./data
COPY config.json /etc/aenv-build/config.json
ADD extra.txt added/
COPY single.txt /workspace
USER builder
RUN id -un > /tmp/who.txt && pwd > /tmp/where.txt && printf '%s' "$BUILD_MARKER" > /tmp/marker.txt
ENTRYPOINT ["/bin/sh", "-c", "sleep infinity"]
"#,
    )?;
    Ok(())
}

fn exec_capture(client: &Client, sandbox_id: &str, command: &str) -> Result<String> {
    let transport = client.transport(sandbox_id)?;
    let rt = crate::commands::tokio_rt()?;
    let output = rt.block_on(super::run_guest_command(
        &transport,
        command,
        super::GuestExec::default(),
    ))?;
    if output.exit_code != 0 {
        bail!(
            "command {command:?} exited with status {}{}",
            output.exit_code,
            output.detail()
        );
    }
    Ok(output.stdout)
}

fn listed_sandbox_ids(client: &Client) -> Result<HashSet<String>> {
    Ok(client
        .list_sandboxes()?
        .into_iter()
        .map(|sandbox| sandbox.sandbox_id)
        .collect())
}

/// Full acceptance flow: build a single-stage Dockerfile with RUN (shell,
/// exec-form, heredoc), ENV, WORKDIR, USER, COPY, and local ADD; then launch
/// the produced template and assert the copied content and the final
/// environment, workdir, user, and startup metadata survived capture.
#[test]
#[ignore = "requires a live AgentENV server; set AENV_E2E_URL and AENV_E2E_API_KEY"]
fn e2e_native_build_end_to_end() -> Result<()> {
    let client = e2e_client()?;
    let context = tempfile::tempdir()?;
    write_context(context.path())?;
    let name = unique_name("aenv-e2e-build");

    let image = base_image();
    let mut args = build_args(context.path(), &name);
    args.user_image = (image != "ubuntu:24.04").then(|| image.clone());
    run_with_client(client.clone(), args)?;

    // The build sandbox must be gone after a successful build; the template
    // is launched from the published snapshot alias.
    let template_id = client.resolve_alias(&name)?;
    let sandbox_id = client.create_sandbox(&template_id, Some(120))?;
    let checks = (|| -> Result<()> {
        // Base image environment survives alongside the Dockerfile ENV.
        let env = exec_capture(&client, &sandbox_id, "printf '%s' \"$BUILD_MARKER\"")?;
        assert_eq!(env, "native-build");
        let path = exec_capture(&client, &sandbox_id, "printf '%s' \"$PATH\"")?;
        assert!(!path.is_empty(), "base image PATH must survive capture");

        // Final workdir and user are the envd defaults after relaunch.
        let workdir = exec_capture(&client, &sandbox_id, "pwd")?;
        assert_eq!(workdir.trim(), "/workspace/app");
        let user = exec_capture(&client, &sandbox_id, "id -un")?;
        assert_eq!(user.trim(), "builder");

        // RUN executed as the effective user and in the effective workdir.
        assert_eq!(
            exec_capture(&client, &sandbox_id, "cat /tmp/who.txt")?.trim(),
            "builder"
        );
        assert_eq!(
            exec_capture(&client, &sandbox_id, "cat /tmp/where.txt")?.trim(),
            "/workspace/app"
        );
        assert_eq!(
            exec_capture(&client, &sandbox_id, "cat /tmp/marker.txt")?.trim(),
            "native-build"
        );

        // Exec-form and heredoc RUN lowering from #143 kept working.
        assert_eq!(
            exec_capture(&client, &sandbox_id, "cat /exec-form.txt")?.trim(),
            "exec-form-ok"
        );
        assert_eq!(
            exec_capture(&client, &sandbox_id, "cat /heredoc.txt")?.trim(),
            "heredoc-ok"
        );

        // COPY directory contents, file-target with created parents, ADD
        // into a trailing-slash directory, and the stat-resolved single-file
        // copy onto the existing /workspace directory.
        assert_eq!(
            exec_capture(&client, &sandbox_id, "cat /workspace/app/data/file.txt")?.trim(),
            "copied-file"
        );
        assert_eq!(
            exec_capture(
                &client,
                &sandbox_id,
                "cat /workspace/app/data/nested/deep.txt"
            )?
            .trim(),
            "deep"
        );
        assert_eq!(
            exec_capture(&client, &sandbox_id, "cat /etc/aenv-build/config.json")?.trim(),
            "{\"ok\":true}"
        );
        assert_eq!(
            exec_capture(&client, &sandbox_id, "cat /workspace/app/added/extra.txt")?.trim(),
            "extra"
        );
        assert_eq!(
            exec_capture(&client, &sandbox_id, "cat /workspace/single.txt")?.trim(),
            "single"
        );

        // Copied content is root-owned (no --chown support yet).
        assert_eq!(
            exec_capture(
                &client,
                &sandbox_id,
                "stat -c %U:%G /workspace/app/data/file.txt"
            )?
            .trim(),
            "root:root"
        );

        // .dockerignore excluded the secret file.
        let ignored = exec_capture(
            &client,
            &sandbox_id,
            "test -e /workspace/app/data/secret.env && echo present || echo absent",
        )?;
        assert_eq!(ignored.trim(), "absent");

        // Startup metadata: the entrypoint-derived start command is running.
        let sleeping = exec_capture(
            &client,
            &sandbox_id,
            "pgrep -f 'sleep infinity' >/dev/null && echo running || echo missing",
        )?;
        assert_eq!(sleeping.trim(), "running");
        Ok(())
    })();

    let _ = client.delete_sandbox(&sandbox_id);
    let _ = client.delete_template(&template_id);
    checks
}

/// A failing RUN step must fail the build and remove the build sandbox.
#[test]
#[ignore = "requires a live AgentENV server; set AENV_E2E_URL and AENV_E2E_API_KEY"]
fn e2e_native_build_step_failure_cleans_up_sandbox() -> Result<()> {
    let client = e2e_client()?;
    let context = tempfile::tempdir()?;
    fs::write(
        context.path().join("Dockerfile"),
        "FROM ubuntu:24.04\nRUN exit 7\n",
    )?;
    let before = listed_sandbox_ids(&client)?;

    let image = base_image();
    let mut args = build_args(context.path(), &unique_name("aenv-e2e-buildfail"));
    args.user_image = (image != "ubuntu:24.04").then(|| image.clone());
    let result = run_with_client(client.clone(), args);
    let err = format!("{:#}", result.expect_err("build must fail on RUN exit 7"));
    assert!(err.contains("status 7"), "unexpected error: {err}");

    let after = listed_sandbox_ids(&client)?;
    let leaked: Vec<_> = after.difference(&before).collect();
    assert!(
        leaked.is_empty(),
        "build sandbox leaked after failed build: {leaked:?}"
    );
    Ok(())
}

/// Multi-stage Dockerfiles and extended ADD/COPY forms fail before any
/// sandbox is created, even with valid credentials.
#[test]
#[ignore = "requires a live AgentENV server; set AENV_E2E_URL and AENV_E2E_API_KEY"]
fn e2e_native_build_rejections_do_not_create_sandboxes() -> Result<()> {
    let client = e2e_client()?;
    let before = listed_sandbox_ids(&client)?;

    for dockerfile in [
        "FROM ubuntu:24.04 AS base\nFROM ubuntu:24.04\n",
        "FROM ubuntu:24.04\nCOPY --from=base /x /y\n",
        "FROM ubuntu:24.04\nADD https://example.com/f.tar /x/\n",
        "FROM ubuntu:24.04\nARG X=1\n",
    ] {
        let context = tempfile::tempdir()?;
        fs::write(context.path().join("Dockerfile"), dockerfile)?;
        let args = build_args(context.path(), &unique_name("aenv-e2e-reject"));
        assert!(
            run_with_client(client.clone(), args).is_err(),
            "expected rejection for {dockerfile:?}"
        );
    }

    let after = listed_sandbox_ids(&client)?;
    let leaked: Vec<_> = after.difference(&before).collect();
    assert!(
        leaked.is_empty(),
        "rejected builds must not create sandboxes"
    );
    Ok(())
}
