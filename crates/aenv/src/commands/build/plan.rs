//! Dockerfile parsing and up-front validation for native builds.
//!
//! The whole supported subset is validated before any sandbox is created:
//! unsupported instructions and forms fail here with actionable errors, and
//! metadata-only instructions that AgentENV ignores emit explicit warnings.

use anyhow::{bail, Context, Result};
use parse_dockerfile::{Command, HereDoc, Instruction, RunInstruction, Source};
use shell_util::shell_quote;

/// A Dockerfile lowered to the instruction sequence the native build
/// executes, plus its final command-context overrides.
#[derive(Debug)]
pub(crate) struct BuildPlan {
    /// Base image the build sandbox boots from. The Dockerfile must contain a
    /// single non-scratch `FROM`; `--image` only replaces that image.
    pub base_image: String,
    pub steps: Vec<BuildStep>,
    /// `ENTRYPOINT` in OCI image config form (shell form becomes
    /// `["/bin/sh", "-c", ...]`).
    pub entrypoint: Option<Vec<String>>,
    /// `CMD` in OCI image config form.
    pub cmd: Option<Vec<String>>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum BuildStep {
    Run { command: String },
    Env { pairs: Vec<(String, String)> },
    Workdir { path: String },
    User { user: String },
    Copy(CopyStep),
}

#[derive(Debug, PartialEq)]
pub(crate) struct CopyStep {
    /// `COPY` or `ADD`; ADD sources are additionally rejected when they are
    /// recognized archives, which Docker would auto-extract.
    pub instruction: &'static str,
    pub sources: Vec<String>,
    pub dest: String,
}

impl BuildStep {
    /// One-line rendering used for progress output.
    pub(crate) fn display(&self) -> String {
        match self {
            BuildStep::Run { command } => {
                let mut line = command.replace('\n', " ");
                const MAX: usize = 200;
                if line.chars().count() > MAX {
                    line = line.chars().take(MAX).collect::<String>() + "...";
                }
                format!("RUN {line}")
            }
            BuildStep::Env { pairs } => {
                let rendered = pairs
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("ENV {rendered}")
            }
            BuildStep::Workdir { path } => format!("WORKDIR {path}"),
            BuildStep::User { user } => format!("USER {user}"),
            BuildStep::Copy(copy) => format!(
                "{} {} {}",
                copy.instruction,
                copy.sources.join(" "),
                copy.dest
            ),
        }
    }
}

pub(crate) fn parse_build_plan(
    dockerfile: &str,
    image_override: Option<String>,
) -> Result<BuildPlan> {
    let parsed = parse_dockerfile::parse(dockerfile).context("parsing Dockerfile")?;
    let escape = parsed
        .parser_directives
        .escape
        .as_ref()
        .map_or('\\', |directive| directive.value.value);

    let mut from_image: Option<String> = None;
    let mut steps = Vec::new();
    let mut entrypoint = None;
    let mut cmd = None;

    for instruction in &parsed.instructions {
        // Docker permits a global ARG before FROM. It is still unsupported in
        // v1, but let the ARG arm below return the instruction-specific error
        // instead of misclassifying it as an ordering problem.
        if from_image.is_none()
            && !matches!(instruction, Instruction::From(_) | Instruction::Arg(_))
        {
            bail!("Dockerfile instructions must follow the FROM instruction");
        }

        match instruction {
            Instruction::From(from) => {
                if from_image.is_some() {
                    bail!(
                        "multi-stage builds are not supported: only a single FROM instruction \
                         is allowed"
                    );
                }
                if let Some(option) = from.options.first() {
                    let name = option.name.value.as_ref();
                    bail!(
                        "FROM --{name} is not supported by aenv build v1; remove the option from \
                         the FROM instruction"
                    );
                }
                if let Some((_, alias)) = &from.as_ {
                    let alias = alias.value.as_ref();
                    bail!(
                        "FROM stage alias {alias:?} is not supported by aenv build v1; remove \
                         `AS {alias}` because only single-stage builds are supported"
                    );
                }
                let image = from.image.value.as_ref();
                if image.eq_ignore_ascii_case("scratch") {
                    bail!(
                        "FROM scratch is not supported: AgentENV build sandboxes need a \
                         bootable base image"
                    );
                }
                if image.contains('$') && image_override.is_none() {
                    bail!(
                        "FROM with a variable reference is not supported (ARG is not \
                         implemented); pass the base image explicitly with --image"
                    );
                }
                if image.is_empty() {
                    bail!("FROM instruction requires an image reference");
                }
                from_image = Some(image.to_string());
            }
            Instruction::Run(run) => {
                steps.push(BuildStep::Run {
                    command: dockerfile_run_command(run, escape)?,
                });
            }
            Instruction::Env(env) => {
                let pairs = parse_env_pairs(&env.arguments.value)?;
                for (_, value) in &pairs {
                    warn_unexpanded_variables("ENV", value);
                }
                steps.push(BuildStep::Env { pairs });
            }
            Instruction::Workdir(workdir) => {
                let path = workdir.arguments.value.trim().to_string();
                if path.is_empty() {
                    bail!("WORKDIR instruction requires a path");
                }
                warn_unexpanded_variables("WORKDIR", &path);
                steps.push(BuildStep::Workdir { path });
            }
            Instruction::User(user) => {
                let user = user.arguments.value.trim().to_string();
                validate_user(&user)?;
                warn_unexpanded_variables("USER", &user);
                steps.push(BuildStep::User { user });
            }
            Instruction::Copy(copy) => {
                validate_copy_options("COPY", &copy.options)?;
                steps.push(BuildStep::Copy(copy_step(
                    "COPY",
                    &copy.src,
                    copy.dest.value.as_ref(),
                )?));
            }
            Instruction::Add(add) => {
                validate_copy_options("ADD", &add.options)?;
                let step = copy_step("ADD", &add.src, add.dest.value.as_ref())?;
                for source in &step.sources {
                    if is_remote_source(source) {
                        bail!(
                            "ADD with a remote URL source is not supported: {source}. \
                             Download it in a RUN instruction instead (e.g. RUN curl -fLO ...)"
                        );
                    }
                }
                steps.push(BuildStep::Copy(step));
            }
            Instruction::Entrypoint(instruction) => {
                entrypoint = dockerfile_command_vector(&instruction.arguments);
            }
            Instruction::Cmd(instruction) => {
                cmd = dockerfile_command_vector(&instruction.arguments);
            }
            Instruction::Arg(_) => {
                bail!(
                    "ARG is not supported: aenv build does not implement Docker build \
                     arguments. Inline the value, or use ENV if it should persist into the \
                     template"
                );
            }
            Instruction::Shell(_) => {
                bail!("SHELL is not supported: RUN instructions always execute via /bin/bash -lc");
            }
            Instruction::Stopsignal(_) => {
                warn_ignored_instruction("STOPSIGNAL");
            }
            Instruction::Expose(_) => {
                warn_ignored_instruction("EXPOSE");
            }
            Instruction::Volume(_) => {
                warn_ignored_instruction("VOLUME");
            }
            Instruction::Label(_) => {
                warn_ignored_instruction("LABEL");
            }
            Instruction::Maintainer(_) => {
                warn_ignored_instruction("MAINTAINER");
            }
            Instruction::Healthcheck(_) => {
                bail!("Dockerfile instruction HEALTHCHECK is not supported")
            }
            Instruction::Onbuild(_) => {
                bail!("Dockerfile instruction ONBUILD is not supported")
            }
            _ => bail!("Dockerfile instruction is not supported"),
        }
    }

    let from_image =
        from_image.context("Dockerfile must contain exactly one actual FROM instruction")?;
    let base_image = image_override.unwrap_or(from_image);

    Ok(BuildPlan {
        base_image,
        steps,
        entrypoint,
        cmd,
    })
}

fn copy_step(instruction: &'static str, src: &[Source<'_>], dest: &str) -> Result<CopyStep> {
    let mut sources = Vec::with_capacity(src.len());
    for source in src {
        match source {
            Source::Path(path) => {
                let path = path.value.as_ref();
                if path.is_empty() {
                    bail!("{instruction} sources cannot be empty");
                }
                warn_unexpanded_variables(instruction, path);
                sources.push(path.to_string());
            }
            Source::HereDoc(_) => {
                bail!("{instruction} with heredoc sources is not supported");
            }
            _ => bail!("{instruction} source form is not supported"),
        }
    }
    if sources.is_empty() {
        bail!("{instruction} requires at least one source");
    }
    let dest = dest.to_string();
    if dest.is_empty() {
        bail!("{instruction} requires a destination");
    }
    warn_unexpanded_variables(instruction, &dest);
    Ok(CopyStep {
        instruction,
        sources,
        dest,
    })
}

fn validate_copy_options(instruction: &str, options: &[parse_dockerfile::Flag<'_>]) -> Result<()> {
    let Some(option) = options.first() else {
        return Ok(());
    };
    let name = option.name.value.as_ref();
    match name.to_ascii_lowercase().as_str() {
        "from" => {
            bail!("{instruction} --from is not supported: multi-stage builds are not available")
        }
        "chown" | "chmod" => bail!(
            "{instruction} --{name} is not supported yet: copied content is extracted as \
             root. Use a RUN chown/chmod instruction after the copy instead"
        ),
        _ => bail!("{instruction} --{name} is not supported"),
    }
}

fn is_remote_source(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ftp://")
        || lower.starts_with("git://")
        || lower.starts_with("git@")
        || lower.starts_with("ssh://")
}

fn validate_user(user: &str) -> Result<()> {
    if user.is_empty() {
        bail!("USER instruction requires a user name");
    }
    if user.contains(':') {
        bail!("USER with an explicit group ({user}) is not supported; specify only the user name");
    }
    if user.contains(char::is_whitespace) {
        bail!("USER value cannot contain whitespace: {user}");
    }
    Ok(())
}

/// aenv build performs no Dockerfile variable expansion; values containing
/// `$` are used literally. RUN commands are unaffected because the shell
/// expands environment references at execution time.
fn warn_unexpanded_variables(instruction: &str, value: &str) {
    if value.contains('$') {
        eprintln!(
            "warning: {instruction} value {value:?} contains '$' but aenv build does not \
             expand variables; the value is used literally"
        );
    }
}

/// Parses ENV arguments into key/value pairs. Supports the `key=value ...`
/// form with double/single quoting and backslash escapes, and the legacy
/// space-separated `ENV key value` form producing a single pair.
fn parse_env_pairs(raw: &str) -> Result<Vec<(String, String)>> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("ENV instruction requires arguments");
    }

    let first_token_has_equals = raw
        .split_whitespace()
        .next()
        .is_some_and(|token| token.contains('='));
    if !first_token_has_equals {
        let Some((key, value)) = raw.split_once(char::is_whitespace) else {
            bail!("ENV instruction requires key/value arguments");
        };
        return Ok(vec![(key.to_string(), value.trim_start().to_string())]);
    }

    let mut pairs = Vec::new();
    for word in split_env_words(raw)? {
        let Some((key, value)) = word.split_once('=') else {
            bail!("ENV syntax error: expected key=value pairs, got {word:?}");
        };
        if key.is_empty() {
            bail!("ENV instruction requires a non-empty key");
        }
        pairs.push((key.to_string(), value.to_string()));
    }
    Ok(pairs)
}

/// Splits ENV arguments into words, honoring double/single quotes and
/// backslash escapes the way Docker's parser does.
fn split_env_words(raw: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = raw.chars();

    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    word.push(ch);
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => match chars.next() {
                    Some(next @ ('"' | '\\')) => word.push(next),
                    Some(next) => {
                        word.push('\\');
                        word.push(next);
                    }
                    None => bail!("ENV value has a trailing backslash"),
                },
                _ => word.push(ch),
            },
            _ => match ch {
                '\'' | '"' => {
                    in_word = true;
                    quote = Some(ch);
                }
                '\\' => {
                    in_word = true;
                    match chars.next() {
                        Some(next) => word.push(next),
                        None => bail!("ENV value has a trailing backslash"),
                    }
                }
                ch if ch.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                _ => {
                    in_word = true;
                    word.push(ch);
                }
            },
        }
    }
    if quote.is_some() {
        bail!("ENV value has an unterminated quote");
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

fn dockerfile_run_command(run: &RunInstruction<'_>, escape: char) -> Result<String> {
    match (&run.arguments, run.here_docs.as_slice()) {
        (Command::Exec(_), []) => dockerfile_command(&run.arguments)
            .context("RUN instruction requires a non-empty command"),
        (Command::Shell(command), []) => Ok(normalize_run_continuations(command.value, escape)),
        (Command::Shell(command), [here_doc]) if command.value.trim().is_empty() => {
            Ok(here_doc.value.to_string())
        }
        (Command::Shell(command), [here_doc]) => {
            Ok(render_run_heredoc(command.value.trim(), here_doc))
        }
        (_, here_docs) if here_docs.len() > 1 => {
            bail!("multiple RUN heredocs are not supported")
        }
        _ => bail!("RUN heredocs are only supported for shell-form commands"),
    }
}

fn render_run_heredoc(command: &str, here_doc: &HereDoc<'_>) -> String {
    let delimiter = unique_heredoc_delimiter(&here_doc.value);
    let opening_delimiter = if here_doc.expand {
        delimiter.clone()
    } else {
        format!("'{delimiter}'")
    };
    let body_newline = if here_doc.value.is_empty() || here_doc.value.ends_with('\n') {
        ""
    } else {
        "\n"
    };

    format!(
        "<<{opening_delimiter} {command}\n{}{body_newline}{delimiter}",
        here_doc.value
    )
}

fn unique_heredoc_delimiter(body: &str) -> String {
    const BASE: &str = "AENV_HEREDOC";

    (0..)
        .map(|suffix| {
            if suffix == 0 {
                BASE.to_string()
            } else {
                format!("{BASE}_{suffix}")
            }
        })
        .find(|delimiter| body.lines().all(|line| line != delimiter))
        .expect("the unbounded delimiter sequence must contain an unused value")
}

fn normalize_run_continuations(command: &str, escape: char) -> String {
    if escape == '\\' {
        return command.to_string();
    }

    command
        .replace(&format!("{escape}\r\n"), "\\\r\n")
        .replace(&format!("{escape}\n"), "\\\n")
}

/// Renders a command as a single shell string for the startup command:
/// exec-form parts are shell-quoted and joined, shell form is used verbatim.
fn dockerfile_command(command: &Command<'_>) -> Option<String> {
    match command {
        Command::Exec(parts) => (!parts.value.is_empty()).then(|| {
            parts
                .value
                .iter()
                .map(|part| shell_quote(&part.value))
                .collect::<Vec<_>>()
                .join(" ")
        }),
        Command::Shell(command) => {
            let command = command.value.trim();
            (!command.is_empty()).then(|| command.to_string())
        }
        _ => None,
    }
}

/// Renders a command in OCI image config form: exec form maps to its parts,
/// shell form to `["/bin/sh", "-c", command]` the way Docker records it.
fn dockerfile_command_vector(command: &Command<'_>) -> Option<Vec<String>> {
    match command {
        Command::Exec(parts) => (!parts.value.is_empty()).then(|| {
            parts
                .value
                .iter()
                .map(|part| part.value.to_string())
                .collect()
        }),
        Command::Shell(command) => {
            let command = command.value.trim();
            (!command.is_empty())
                .then(|| vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()])
        }
        _ => None,
    }
}

fn warn_ignored_instruction(instruction: &str) {
    eprintln!("warning: {instruction} instruction is not supported and will be ignored");
}

#[cfg(test)]
mod tests {
    use super::{parse_build_plan, parse_env_pairs, BuildStep};

    fn plan(dockerfile: &str) -> super::BuildPlan {
        parse_build_plan(dockerfile, None).unwrap()
    }

    fn plan_err(dockerfile: &str) -> String {
        format!("{:#}", parse_build_plan(dockerfile, None).unwrap_err())
    }

    #[test]
    fn build_plan_converts_supported_instructions() {
        let plan = plan(
            r#"
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
WORKDIR /app
RUN apt-get update
USER alice
COPY src/ /app/src/
ADD notes.txt /app/
"#,
        );

        assert_eq!(plan.base_image, "ubuntu:24.04");
        assert_eq!(plan.steps.len(), 6);
        assert_eq!(
            plan.steps[0],
            BuildStep::Env {
                pairs: vec![("DEBIAN_FRONTEND".into(), "noninteractive".into())]
            }
        );
        assert_eq!(
            plan.steps[1],
            BuildStep::Workdir {
                path: "/app".into()
            }
        );
        assert_eq!(
            plan.steps[2],
            BuildStep::Run {
                command: "apt-get update".into()
            }
        );
        assert_eq!(
            plan.steps[3],
            BuildStep::User {
                user: "alice".into()
            }
        );
        let BuildStep::Copy(copy) = &plan.steps[4] else {
            panic!("expected copy step");
        };
        assert_eq!(copy.instruction, "COPY");
        assert_eq!(copy.sources, ["src/"]);
        assert_eq!(copy.dest, "/app/src/");
        let BuildStep::Copy(add) = &plan.steps[5] else {
            panic!("expected add step");
        };
        assert_eq!(add.instruction, "ADD");
    }

    #[test]
    fn build_plan_requires_from() {
        let err = plan_err("RUN echo hi");
        assert!(err.contains("FROM"), "{err}");
    }

    #[test]
    fn build_plan_rejects_instructions_before_from() {
        let cases = [
            ("ENV", "ENV A=1\nFROM ubuntu:24.04"),
            ("WORKDIR", "WORKDIR /app\nFROM ubuntu:24.04"),
            ("USER", "USER root\nFROM ubuntu:24.04"),
            ("COPY", "COPY src /app\nFROM ubuntu:24.04"),
            ("ADD", "ADD src /app\nFROM ubuntu:24.04"),
            (
                "ENTRYPOINT",
                "ENTRYPOINT [\"/bin/true\"]\nFROM ubuntu:24.04",
            ),
            ("CMD", "CMD [\"/bin/true\"]\nFROM ubuntu:24.04"),
            ("EXPOSE", "EXPOSE 8080\nFROM ubuntu:24.04"),
            ("VOLUME", "VOLUME /data\nFROM ubuntu:24.04"),
            ("LABEL", "LABEL maintainer=test\nFROM ubuntu:24.04"),
            ("MAINTAINER", "MAINTAINER AgentENV\nFROM ubuntu:24.04"),
            ("STOPSIGNAL", "STOPSIGNAL SIGTERM\nFROM ubuntu:24.04"),
        ];

        for (instruction, dockerfile) in cases {
            let err = plan_err(dockerfile);
            assert!(
                err.contains("FROM"),
                "{instruction} before FROM should be rejected: {err}"
            );
        }
    }

    #[test]
    fn build_plan_rejects_global_arg_with_the_arg_specific_error() {
        let err = plan_err("ARG BASE=ubuntu:24.04\nFROM $BASE");
        assert!(err.contains("ARG"), "{err}");
        assert!(err.contains("not supported"), "{err}");
    }

    #[test]
    fn build_plan_image_override_does_not_replace_missing_from() {
        let err = format!(
            "{:#}",
            parse_build_plan("# syntax=docker/dockerfile:1\n", Some("alpine:3".into()))
                .unwrap_err()
        );
        assert!(err.contains("FROM"), "{err}");
    }

    #[test]
    fn build_plan_image_override_replaces_from() {
        let plan =
            parse_build_plan("FROM ubuntu:24.04\nRUN true", Some("alpine:3".into())).unwrap();
        assert_eq!(plan.base_image, "alpine:3");
    }

    #[test]
    fn build_plan_image_override_allows_variable_from() {
        let plan = parse_build_plan("FROM $BASE\nRUN true", Some("alpine:3".into())).unwrap();
        assert_eq!(plan.base_image, "alpine:3");
    }

    #[test]
    fn build_plan_rejects_variable_from_without_override() {
        let err = plan_err("FROM $BASE\nRUN true");
        assert!(err.contains("--image"), "{err}");
    }

    #[test]
    fn build_plan_rejects_second_from() {
        let err = plan_err("FROM ubuntu:24.04\nFROM node:20");
        assert!(err.contains("multi-stage"), "{err}");
    }

    #[test]
    fn build_plan_rejects_from_options_and_aliases() {
        let cases = [
            (
                "platform option",
                "FROM --platform=linux/amd64 ubuntu:24.04",
                "--platform",
            ),
            ("stage alias", "FROM ubuntu:24.04 AS base", "stage alias"),
        ];

        for (case, dockerfile, expected) in cases {
            let err = plan_err(dockerfile);
            assert!(
                err.contains(expected),
                "{case} should be rejected actionably: {err}"
            );
        }
    }

    #[test]
    fn build_plan_rejects_from_scratch() {
        let err = plan_err("FROM scratch");
        assert!(err.contains("scratch"), "{err}");
    }

    #[test]
    fn build_plan_rejects_arg() {
        let err = plan_err("FROM ubuntu:24.04\nARG NODE_ENV=production");
        assert!(err.contains("ARG is not supported"), "{err}");
    }

    #[test]
    fn build_plan_rejects_shell() {
        let err = plan_err("FROM ubuntu:24.04\nSHELL [\"/bin/sh\", \"-c\"]");
        assert!(err.contains("SHELL is not supported"), "{err}");
    }

    #[test]
    fn build_plan_rejects_copy_from() {
        let err = plan_err("FROM ubuntu:24.04\nCOPY --from=builder /out /app");
        assert!(err.contains("--from"), "{err}");
    }

    #[test]
    fn build_plan_rejects_copy_chown_and_chmod() {
        let err = plan_err("FROM ubuntu:24.04\nCOPY --chown=app:app src /app");
        assert!(err.contains("--chown"), "{err}");
        let err = plan_err("FROM ubuntu:24.04\nCOPY --chmod=755 src /app");
        assert!(err.contains("--chmod"), "{err}");
    }

    #[test]
    fn build_plan_rejects_add_remote_urls() {
        for source in [
            "https://example.com/file.tar.gz",
            "http://example.com/file",
            "git@github.com:org/repo.git",
        ] {
            let err = plan_err(&format!("FROM ubuntu:24.04\nADD {source} /app/"));
            assert!(err.contains("remote URL"), "{source}: {err}");
        }
    }

    #[test]
    fn build_plan_rejects_user_with_group_or_empty() {
        let err = plan_err("FROM ubuntu:24.04\nUSER app:app");
        assert!(err.contains("group"), "{err}");
    }

    #[test]
    fn build_plan_rejects_healthcheck_and_onbuild() {
        assert!(plan_err("FROM ubuntu:24.04\nHEALTHCHECK CMD true").contains("HEALTHCHECK"));
        assert!(plan_err("FROM ubuntu:24.04\nONBUILD RUN true").contains("ONBUILD"));
    }

    #[test]
    fn build_plan_ignores_metadata_instructions() {
        let plan = plan(
            r#"
FROM ubuntu:24.04
EXPOSE 8080
VOLUME /data
LABEL maintainer=test
STOPSIGNAL SIGTERM
MAINTAINER AgentENV
RUN echo hi
"#,
        );
        assert_eq!(plan.steps.len(), 1);
        assert!(matches!(plan.steps[0], BuildStep::Run { .. }));
    }

    #[test]
    fn build_plan_parses_multiline_run() {
        let plan = plan(
            r#"FROM ubuntu:24.04
RUN apt-get update && \
    apt-get install -y curl
"#,
        );

        assert_eq!(
            plan.steps[0],
            BuildStep::Run {
                command: "apt-get update && \\\n    apt-get install -y curl".into()
            }
        );
    }

    #[test]
    fn build_plan_honors_escape_directive_for_multiline_run() {
        let plan = plan("# escape=`\nFROM ubuntu:24.04\nRUN echo first && `\n    echo second\n");

        assert_eq!(
            plan.steps[0],
            BuildStep::Run {
                command: "echo first && \\\n    echo second".into()
            }
        );
    }

    #[test]
    fn build_plan_parses_exec_form_run() {
        let plan = plan("FROM ubuntu:24.04\nRUN [\"echo\", \"hello world\"]\n");
        assert_eq!(
            plan.steps[0],
            BuildStep::Run {
                command: "echo 'hello world'".into()
            }
        );
    }

    #[test]
    fn build_plan_parses_bare_run_heredoc_as_script() {
        let plan = plan("FROM ubuntu:24.04\nRUN <<EOF\nset -eu\necho hello\nEOF\n");
        assert_eq!(
            plan.steps[0],
            BuildStep::Run {
                command: "set -eu\necho hello\n".into()
            }
        );
    }

    #[test]
    fn build_plan_preserves_run_heredoc_command() {
        let plan = plan("FROM ubuntu:24.04\nRUN <<EOF bash\nset -eu\necho hello\nEOF\n");
        assert_eq!(
            plan.steps[0],
            BuildStep::Run {
                command: "<<AENV_HEREDOC bash\nset -eu\necho hello\nAENV_HEREDOC".into()
            }
        );
    }

    #[test]
    fn build_plan_renders_safe_quoted_heredoc_delimiter() {
        let plan = plan("FROM ubuntu:24.04\nRUN <<'EOF' cat\nAENV_HEREDOC\nEOF\n");
        assert_eq!(
            plan.steps[0],
            BuildStep::Run {
                command: "<<'AENV_HEREDOC_1' cat\nAENV_HEREDOC\nAENV_HEREDOC_1".into()
            }
        );
    }

    #[test]
    fn build_plan_preserves_entrypoint_and_cmd_vectors() {
        let plan = plan(
            r#"
FROM ubuntu:24.04
CMD ["ignored"]
ENTRYPOINT ["/usr/bin/env", "bash"]
"#,
        );

        assert_eq!(
            plan.entrypoint,
            Some(vec!["/usr/bin/env".to_string(), "bash".to_string()])
        );
        assert_eq!(plan.cmd, Some(vec!["ignored".to_string()]));
    }

    #[test]
    fn build_plan_preserves_exec_form_args_with_spaces() {
        let plan = plan(
            r#"
FROM ubuntu:24.04
ENTRYPOINT ["sh", "-c", "echo hello world"]
"#,
        );

        assert_eq!(
            plan.entrypoint,
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo hello world".to_string()
            ])
        );
    }

    #[test]
    fn build_plan_exec_form_cmd_used_when_no_entrypoint() {
        let plan = plan("FROM ubuntu:24.04\nCMD [\"python3\", \"app.py\"]\n");
        assert_eq!(
            plan.cmd,
            Some(vec!["python3".to_string(), "app.py".to_string()])
        );
    }

    #[test]
    fn build_plan_shell_form_cmd_maps_to_sh_c_vector() {
        let plan = plan("FROM ubuntu:24.04\nCMD sleep infinity\n");
        assert_eq!(
            plan.cmd,
            Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "sleep infinity".to_string()
            ])
        );
    }

    #[test]
    fn build_plan_last_user_wins_in_sequence() {
        let plan = plan(
            r#"
FROM ubuntu:24.04
USER root
USER alice
"#,
        );

        let users: Vec<_> = plan
            .steps
            .iter()
            .filter_map(|step| match step {
                BuildStep::User { user } => Some(user.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, ["root", "alice"]);
    }

    #[test]
    fn env_pairs_multi_key_form() {
        assert_eq!(
            parse_env_pairs("A=1 B=2").unwrap(),
            vec![("A".into(), "1".into()), ("B".into(), "2".into())]
        );
    }

    #[test]
    fn env_pairs_quoted_values() {
        assert_eq!(
            parse_env_pairs(r#"MSG="hello world" NAME='John Doe'"#).unwrap(),
            vec![
                ("MSG".into(), "hello world".into()),
                ("NAME".into(), "John Doe".into())
            ]
        );
    }

    #[test]
    fn env_pairs_escaped_spaces() {
        assert_eq!(
            parse_env_pairs(r"GREETING=hello\ world").unwrap(),
            vec![("GREETING".into(), "hello world".into())]
        );
    }

    #[test]
    fn env_pairs_legacy_space_form() {
        assert_eq!(
            parse_env_pairs("MY_VAR my value with spaces").unwrap(),
            vec![("MY_VAR".into(), "my value with spaces".into())]
        );
    }

    #[test]
    fn env_pairs_empty_value() {
        assert_eq!(
            parse_env_pairs("EMPTY=").unwrap(),
            vec![("EMPTY".into(), String::new())]
        );
    }

    #[test]
    fn env_pairs_reject_word_without_equals() {
        let err = parse_env_pairs("A=1 B").unwrap_err().to_string();
        assert!(err.contains("key=value"), "{err}");
    }

    #[test]
    fn env_pairs_reject_unterminated_quote() {
        assert!(parse_env_pairs("A=\"unterminated").is_err());
    }

    #[test]
    fn build_plan_rejects_copy_heredoc_sources() {
        let err = plan_err("FROM ubuntu:24.04\nCOPY <<EOF /app/file\nhello\nEOF\n");
        assert!(err.contains("heredoc"), "{err}");
    }
}
