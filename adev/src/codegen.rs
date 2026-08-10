use anyhow::Result;
use clap::{Args, Subcommand};

use crate::config;
use crate::util;

#[derive(Args)]
pub struct CodegenArgs {
    #[command(subcommand)]
    pub target: Option<CodegenTarget>,

    /// Only ensure dependencies are installed, don't run codegen
    #[arg(long)]
    pub ensure_deps_only: bool,
}

#[derive(Subcommand)]
pub enum CodegenTarget {
    /// Regenerate Firecracker API client
    Firecracker,
    /// Regenerate envd HTTP client
    Envd,
    /// Regenerate AENV HTTP server stubs
    Server,
    /// Regenerate custom extension HTTP client
    CustomExtension,
}

/// npm wrapper version for @openapitools/openapi-generator-cli.
/// The actual generator jar version is controlled by openapitools.json in the project root.
const OPENAPI_GENERATOR_CLI_VERSION: &str = "2.32.0";

pub fn run(args: CodegenArgs) -> Result<()> {
    let project_root = config::project_root()?;

    if args.ensure_deps_only {
        let cfg = config::load_config_from_root(&project_root)?;
        // Also ensure protoc
        crate::ensure_tool::ensure_protoc(&cfg.protoc.version, &cfg.protoc.url)?;
        util::info("All codegen dependencies are ready.");
        return Ok(());
    }

    match args.target {
        Some(CodegenTarget::Firecracker) => run_firecracker(&project_root),
        Some(CodegenTarget::Envd) => run_envd(&project_root),
        Some(CodegenTarget::Server) => run_server(&project_root),
        Some(CodegenTarget::CustomExtension) => run_custom_extension(&project_root),
        None => {
            run_firecracker(&project_root)?;
            run_envd(&project_root)?;
            run_server(&project_root)?;
            run_custom_extension(&project_root)?;
            Ok(())
        }
    }
}

/// Regenerate the custom extension HTTP client into
/// `src/custom_extension_api/generated`.
fn run_custom_extension(project_root: &std::path::Path) -> Result<()> {
    let ext_dir = project_root.join("src/custom_extension_api/generated");
    let spec = project_root.join("src/custom_extension_api/openapi.yml");

    util::info("Regenerating custom extension HTTP client...");
    run_openapi_generator(
        project_root,
        &[
            "generate",
            "-i",
            &spec.to_string_lossy(),
            "-g",
            "rust",
            "-o",
            &ext_dir.to_string_lossy(),
            "--additional-properties=packageName=custom_extension_client,hideGenerationTimestamp=true",
            "--skip-validate-spec",
        ],
    )?;

    prepend_allow_attrs(&ext_dir.join("src/lib.rs"))?;
    util::cmd("cargo", &["fmt", "-p", "custom_extension_client"])?;
    util::info("custom extension client generated.");
    Ok(())
}

/// Run openapi-generator-cli via npx.
/// The generator jar version is read from openapitools.json in the project root.
fn run_openapi_generator(project_root: &std::path::Path, args: &[&str]) -> Result<()> {
    let package = format!(
        "@openapitools/openapi-generator-cli@{}",
        OPENAPI_GENERATOR_CLI_VERSION
    );
    let mut cmd_args: Vec<&str> = vec!["--yes", &package, "--"];
    cmd_args.extend_from_slice(args);
    util::cmd_in_dir("npx", &cmd_args, project_root)
}

fn run_firecracker(project_root: &std::path::Path) -> Result<()> {
    let fc_dir = project_root.join("thirdparty/firecracker-client");
    let spec = fc_dir.join("firecracker.yaml");

    util::info("Regenerating Firecracker API client...");
    run_openapi_generator(
        project_root,
        &[
            "generate",
            "-i",
            &spec.to_string_lossy(),
            "-g",
            "rust",
            "-o",
            &fc_dir.to_string_lossy(),
            "--global-property",
            "models,supportingFiles",
            "--additional-properties=packageName=firecracker_client,hideGenerationTimestamp=true",
        ],
    )?;

    prepend_allow_attrs(&fc_dir.join("src/lib.rs"))?;
    util::cmd("cargo", &["fmt", "-p", "firecracker_client"])?;
    util::info("Firecracker client generated.");
    Ok(())
}

fn run_envd(project_root: &std::path::Path) -> Result<()> {
    let envd_dir = project_root.join("thirdparty/envd/http-client");
    let spec = envd_dir.join("envd.yaml");

    util::info("Regenerating envd HTTP client...");
    run_openapi_generator(
        project_root,
        &[
            "generate",
            "-i",
            &spec.to_string_lossy(),
            "-g",
            "rust",
            "-o",
            &envd_dir.to_string_lossy(),
            "--additional-properties=packageName=http_client,hideGenerationTimestamp=true",
            "--skip-validate-spec",
        ],
    )?;

    prepend_allow_attrs(&envd_dir.join("src/lib.rs"))?;
    util::cmd("cargo", &["fmt", "-p", "http_client"])?;
    util::info("envd HTTP client generated.");
    Ok(())
}

fn run_server(project_root: &std::path::Path) -> Result<()> {
    let server_dir = project_root.join("src/api/generated");
    let spec = project_root.join("src/api/openapi.yml");

    util::info("Regenerating AENV HTTP server...");
    run_openapi_generator(
        project_root,
        &[
            "generate",
            "-g",
            "rust-axum",
            "-i",
            &spec.to_string_lossy(),
            "-o",
            &server_dir.to_string_lossy(),
            "--additional-properties=packageName=agentenv_http_server,hideGenerationTimestamp=true",
        ],
    )?;

    // Port of fix_rust_axum_duplicate_auth_trait.py
    let mod_rs = server_dir.join("src/apis/mod.rs");
    fix_duplicate_auth_trait(&mod_rs)?;
    patch_server_models(&server_dir.join("src/models.rs"))?;
    patch_capture_request_handler(&server_dir.join("src/server/mod.rs"))?;

    util::cmd("cargo", &["fmt", "-p", "agentenv_http_server"])?;
    util::info("AENV server generated.");
    Ok(())
}

/// Prepend #![allow(clippy::all)] and #![allow(warnings)] to a file if not already present.
fn prepend_allow_attrs(path: &std::path::Path) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    if content.starts_with("#![allow(clippy::all)]") {
        return Ok(());
    }
    let new_content = format!("#![allow(clippy::all)]\n#![allow(warnings)]\n{content}");
    std::fs::write(path, new_content)?;
    Ok(())
}

/// Port of scripts/fix_rust_axum_duplicate_auth_trait.py
/// Removes duplicate ApiKeyAuthHeader trait blocks from generated code.
fn fix_duplicate_auth_trait(path: &std::path::Path) -> Result<()> {
    use std::sync::LazyLock;

    if !path.exists() {
        anyhow::bail!("file not found: {}", path.display());
    }

    let content = std::fs::read_to_string(path)?;

    static PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"(?s)/// API Key Authentication - Header\.\r?\n",
            r"\s*#\[async_trait::async_trait\]\r?\n",
            r"\s*pub trait ApiKeyAuthHeader \{\r?\n",
            r"\s+type Claims;\r?\n\r?\n",
            r"\s*/// Extracting Claims from Header\. Return None if the Claims are invalid\.\r?\n",
            r"\s+async fn extract_claims_from_header\(&self, headers: &axum::http::header::HeaderMap, key: &str\) -> Option<Self::Claims>;\r?\n",
            r"\s*\}\r?\n\r?\n"
        ))
        .unwrap()
    });

    let matches: Vec<_> = PATTERN.find_iter(&content).collect();

    if matches.is_empty() {
        anyhow::bail!(
            "ApiKeyAuthHeader trait block not found in {}",
            path.display()
        );
    }

    if matches.len() == 1 {
        util::info(&format!("No duplicate trait blocks in {}", path.display()));
        return Ok(());
    }

    // Keep the first match, remove subsequent duplicates
    let first_end = matches[0].end();
    let before = &content[..first_end];
    let after = PATTERN.replace_all(&content[first_end..], "");
    let deduped = format!("{before}{after}");

    std::fs::write(path, deduped)?;
    util::info(&format!(
        "Removed {} duplicate ApiKeyAuthHeader trait block(s) in {}",
        matches.len() - 1,
        path.display()
    ));
    Ok(())
}

fn patch_server_models(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("file not found: {}", path.display());
    }

    let content = std::fs::read_to_string(path)?;
    let patched = patch_server_models_content(content)?;
    std::fs::write(path, patched)?;
    util::info(&format!(
        "Applied capture API serde and validation fixes in {}",
        path.display()
    ));
    Ok(())
}

fn patch_server_models_content(mut content: String) -> Result<String> {
    for model in ["SandboxSnapshotRequest", "SandboxSnapshotContext"] {
        content = add_serde_deny_unknown_fields(content, model)?;
    }

    content = remove_all_xss_validators_from_struct(content, "SandboxSnapshotContext")?;

    for field in ["start_cmd", "ready_cmd"] {
        content = remove_field_xss_validator(content, "SandboxSnapshotRequest", field)?;
    }

    content.push_str(
        r##"

#[cfg(test)]
mod capture_api_model_tests {
    use super::SandboxSnapshotRequest;
    use validator::Validate;

    #[test]
    fn rejects_unknown_capture_request_fields_at_both_levels() {
        let top_level = serde_json::from_value::<SandboxSnapshotRequest>(serde_json::json!({
            "unexpected": true
        }));
        assert!(top_level.is_err());

        let nested = serde_json::from_value::<SandboxSnapshotRequest>(serde_json::json!({
            "finalContext": {
                "workingDirectory": "/app"
            }
        }));
        assert!(nested.is_err());
    }

    #[test]
    fn accepts_opaque_command_context_strings_but_still_validates_name() {
        let request = serde_json::from_value::<SandboxSnapshotRequest>(serde_json::json!({
            "name": "safe-name",
            "finalContext": {
                "envVars": {
                    "RUST_EXPR": "Vec::<u8>::new()"
                },
                "workdir": "/work/<tenant>/app",
                "entrypoint": ["/bin/sh", "-lc", "cargo run --bin app::<prod>"],
                "cmd": ["printf '<ready>\\n'"],
                "volumes": ["/data/<tenant>"],
                "labels": {
                    "org.example.expression": "Vec::<u8>::new()"
                }
            },
            "startCmd": "cargo run --bin app::<prod>",
            "readyCmd": "test \"$(cat /tmp/status)\" = '<ready>'"
        }))
        .expect("deserialize capture request");
        request
            .validate()
            .expect("opaque command/context strings must not use HTML validation");

        let invalid_name =
            serde_json::from_value::<SandboxSnapshotRequest>(serde_json::json!({
                "name": "<script>alert(1)</script>"
            }))
            .expect("deserialize capture request");
        assert!(invalid_name.validate().is_err());
    }
}
"##,
    );

    Ok(content)
}

fn add_serde_deny_unknown_fields(mut content: String, model: &str) -> Result<String> {
    let marker = format!("pub struct {model} {{");
    let offset = unique_match_offset(&content, &marker)?;
    content.insert_str(offset, "#[serde(deny_unknown_fields)]\n");
    Ok(content)
}

fn remove_all_xss_validators_from_struct(content: String, model: &str) -> Result<String> {
    let (start, end) = struct_range(&content, model)?;
    let body = &content[start..end];
    let mut removed = 0usize;
    let patched = body
        .split_inclusive('\n')
        .filter(|line| {
            let is_xss_validator = line.contains("#[validate(custom(function = \"check_xss_");
            removed += usize::from(is_xss_validator);
            !is_xss_validator
        })
        .collect::<String>();

    if removed == 0 {
        anyhow::bail!("no XSS validators found in generated model {model}");
    }

    Ok(format!(
        "{}{}{}",
        &content[..start],
        patched,
        &content[end..]
    ))
}

fn remove_field_xss_validator(content: String, model: &str, field: &str) -> Result<String> {
    let (start, end) = struct_range(&content, model)?;
    let body = &content[start..end];
    let field_marker = format!("pub {field}:");
    let field_offset = unique_match_offset(body, &field_marker)?;
    let block_start = body[..field_offset]
        .rfind("\n\n")
        .map_or(0, |offset| offset + 2);
    let field_end = body[field_offset..]
        .find('\n')
        .map_or(body.len(), |offset| field_offset + offset + 1);
    let field_block = &body[block_start..field_end];
    let mut removed = 0usize;
    let patched_block = field_block
        .split_inclusive('\n')
        .filter(|line| {
            let is_xss_validator = line.contains("#[validate(custom(function = \"check_xss_");
            removed += usize::from(is_xss_validator);
            !is_xss_validator
        })
        .collect::<String>();

    if removed != 1 {
        anyhow::bail!(
            "expected one XSS validator for generated field {model}.{field}, found {removed}"
        );
    }

    let patched_body = format!(
        "{}{}{}",
        &body[..block_start],
        patched_block,
        &body[field_end..]
    );
    Ok(format!(
        "{}{}{}",
        &content[..start],
        patched_body,
        &content[end..]
    ))
}

fn struct_range(content: &str, model: &str) -> Result<(usize, usize)> {
    let marker = format!("pub struct {model} {{");
    let start = unique_match_offset(content, &marker)?;
    let open_brace = start + marker.len() - 1;
    let mut depth = 0usize;

    for (relative, byte) in content.as_bytes()[open_brace..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok((start, open_brace + relative + 1));
                }
            }
            _ => {}
        }
    }

    anyhow::bail!("unterminated generated model struct {model}")
}

fn unique_match_offset(content: &str, marker: &str) -> Result<usize> {
    let mut matches = content.match_indices(marker);
    let Some((offset, _)) = matches.next() else {
        anyhow::bail!("generated code marker not found: {marker}");
    };
    if matches.next().is_some() {
        anyhow::bail!("generated code marker is not unique: {marker}");
    }
    Ok(offset)
}

fn patch_capture_request_handler(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("file not found: {}", path.display());
    }

    let content = std::fs::read_to_string(path)?;
    let patched = patch_capture_request_handler_content(content)?;
    std::fs::write(path, patched)?;
    util::info(&format!(
        "Preserved capture request JSON rejection statuses in {}",
        path.display()
    ));
    Ok(())
}

fn patch_capture_request_handler_content(mut content: String) -> Result<String> {
    let body_parameter = "Json(body): Json<models::SandboxSnapshotRequest>,";
    let body_parameter_offset = unique_match_offset(&content, body_parameter)?;
    content.replace_range(
        body_parameter_offset..body_parameter_offset + body_parameter.len(),
        concat!(
            "    body: std::result::Result<\n",
            "        Json<models::SandboxSnapshotRequest>,\n",
            "        axum::extract::rejection::JsonRejection,\n",
            "    >,"
        ),
    );

    let handler_doc =
        "/// SandboxesSandboxIdSnapshotsPost - POST /sandboxes/{sandboxID}/snapshots\n";
    let handler_doc_offset = unique_match_offset(&content, handler_doc)?;
    content.insert_str(
        handler_doc_offset,
        concat!(
            "fn capture_snapshot_request_body(\n",
            "    body: std::result::Result<\n",
            "        Json<models::SandboxSnapshotRequest>,\n",
            "        axum::extract::rejection::JsonRejection,\n",
            "    >,\n",
            ") -> std::result::Result<models::SandboxSnapshotRequest, (StatusCode, String)> {\n",
            "    match body {\n",
            "        Ok(Json(body)) => Ok(body),\n",
            "        Err(rejection) => Err((rejection.status(), rejection.to_string())),\n",
            "    }\n",
            "}\n\n"
        ),
    );

    let handler_marker = "async fn sandboxes_sandbox_id_snapshots_post<I, A, E, C>(";
    let handler_start = unique_match_offset(&content, handler_marker)?;
    let relative_auth_comment = content[handler_start..]
        .find("// Authentication")
        .ok_or_else(|| anyhow::anyhow!("capture handler authentication marker not found"))?;
    let auth_comment = handler_start + relative_auth_comment;
    let insertion = content[..auth_comment]
        .rfind('\n')
        .map_or(handler_start, |offset| offset + 1);
    content.insert_str(
        insertion,
        concat!(
            "    let body = match capture_snapshot_request_body(body) {\n",
            "        Ok(body) => body,\n",
            "        Err((status, message)) => {\n",
            "            return Response::builder()\n",
            "                .status(status)\n",
            "                .body(Body::from(message))\n",
            "                .map_err(|_| status);\n",
            "        }\n",
            "    };\n\n"
        ),
    );

    content.push_str(
        r##"

#[cfg(test)]
mod capture_api_handler_tests {
    use super::capture_snapshot_request_body;
    use axum::body::Body;
    use axum::extract::{FromRequest, Json};
    use axum::http::{Request, StatusCode};

    #[test]
    fn unknown_capture_context_field_returns_bad_request() {
        let runtime = tokio::runtime::Runtime::new().expect("create Tokio runtime");
        runtime.block_on(async {
            let request = Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"finalContext":{"workingDirectory":"/app"}}"#,
                ))
                .expect("build request");
            let extracted =
                Json::<crate::models::SandboxSnapshotRequest>::from_request(request, &()).await;

            let rejection = match extracted {
                Ok(_) => panic!("unknown nested field must be rejected"),
                Err(rejection) => rejection,
            };
            let response = match capture_snapshot_request_body(Err(rejection)) {
                Err((status, _)) => status,
                Ok(_) => panic!("JSON rejection must not reach the API implementation"),
            };

            assert_eq!(response, StatusCode::BAD_REQUEST);
        });
    }
}
"##,
    );

    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::{patch_capture_request_handler_content, patch_server_models_content};

    #[test]
    fn server_model_patch_is_strict_and_only_removes_opaque_xss_validation() {
        let input = r#"
pub struct SandboxSnapshotContext {
    #[validate(custom(function = "check_xss_string"))]
    pub workdir: Option<String>,
}

pub struct SandboxSnapshotRequest {
    #[validate(custom(function = "check_xss_string"))]
    pub name: Option<String>,

    #[validate(nested)]
    pub final_context: Option<models::SandboxSnapshotContext>,

    #[validate(custom(function = "check_xss_string"))]
    pub start_cmd: Option<String>,

    #[validate(custom(function = "check_xss_string"))]
    pub ready_cmd: Option<String>,
}
"#;

        let output =
            patch_server_models_content(input.to_string()).expect("patch generated models");

        assert_eq!(output.matches("#[serde(deny_unknown_fields)]").count(), 2);
        assert!(
            output.contains("#[validate(custom(function = \"check_xss_string\"))]\n    pub name:")
        );
        assert!(output.contains("#[validate(nested)]\n    pub final_context:"));
        assert!(!output
            .contains("#[validate(custom(function = \"check_xss_string\"))]\n    pub start_cmd:"));
        assert!(!output
            .contains("#[validate(custom(function = \"check_xss_string\"))]\n    pub ready_cmd:"));
    }

    #[test]
    fn capture_handler_patch_preserves_json_rejection_status() {
        let input = r#"
/// SandboxesSandboxIdSnapshotsPost - POST /sandboxes/{sandboxID}/snapshots
async fn sandboxes_sandbox_id_snapshots_post<I, A, E, C>(
    Json(body): Json<models::SandboxSnapshotRequest>,
) -> Result<Response, StatusCode>
where
    I: Send,
{
    // Authentication
    consume(body);
}
"#;

        let output = patch_capture_request_handler_content(input.to_string())
            .expect("patch generated capture handler");

        assert!(output.contains("axum::extract::rejection::JsonRejection"));
        assert!(output.contains("let body = match capture_snapshot_request_body(body)"));
        assert!(output.contains("rejection.status()"));
        assert!(output.contains(".status(status)"));
        assert!(
            output
                .find("let body = match capture_snapshot_request_body(body)")
                .unwrap()
                < output.find("// Authentication").unwrap()
        );
    }
}
