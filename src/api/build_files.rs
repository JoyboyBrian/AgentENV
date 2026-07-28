//! Hand-written upload endpoint for template build-context archives.
//!
//! `GET /templates/{templateID}/files/{hash}` (generated API) hands the E2B
//! SDK a bearer URL pointing here; the SDK then `PUT`s a tar archive with no
//! authentication headers. The durable random token embedded in the URL is
//! therefore the credential, and this route stays outside the generated
//! router so the archive can stream to disk instead of buffering in memory.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use axum::{Json, Router};
use futures::StreamExt;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

use agentenv_http_server::models;

use super::ApiImpl;
use crate::cfg::ConfigManager;
use crate::snapshot::repository::build_files::is_valid_build_files_hash;

pub(crate) fn router<I>(api_impl: I) -> Router
where
    I: AsRef<ApiImpl> + Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/templates/{template_id}/files/{hash}/content",
            put(upload_build_archive::<I>),
        )
        .with_state(api_impl)
}

struct UploadQuery {
    expires: i64,
    token: String,
}

fn parse_upload_query(query: Option<&str>) -> Option<UploadQuery> {
    let query = query?;
    let mut expires: Option<i64> = None;
    let mut token: Option<String> = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "expires" => expires = value.parse().ok(),
            "token" => token = Some(value.into_owned()),
            _ => {}
        }
    }
    Some(UploadQuery {
        expires: expires?,
        token: token?,
    })
}

fn error_response(code: StatusCode, message: impl Into<String>) -> Response {
    (
        code,
        Json(models::Error::new(code.as_u16() as i32, message.into())),
    )
        .into_response()
}

async fn upload_build_archive<I>(
    State(api_impl): State<I>,
    Path((template_id, hash)): Path<(String, String)>,
    request: Request,
) -> Response
where
    I: AsRef<ApiImpl> + Clone + Send + Sync + 'static,
{
    let api: &ApiImpl = api_impl.as_ref();

    if !is_valid_build_files_hash(&hash) {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid build files hash '{hash}'"),
        );
    }
    let Some(store) = api.snapshot_manager().template_build_files() else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "the configured snapshot backend does not support build-context uploads",
        );
    };
    let Some(query) = parse_upload_query(request.uri().query()) else {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "upload URL is missing the expires/token query parameters",
        );
    };

    // Claiming consumes the grant, so an upload URL works exactly once.
    let now_unix = chrono::Utc::now().timestamp();
    let authorized = match store
        .claim_upload_grant(&query.token, &template_id, &hash, query.expires, now_unix)
        .await
    {
        Ok(authorized) => authorized,
        Err(error) => {
            warn!(error = %error, "failed to claim build-file upload grant");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to validate upload grant",
            );
        }
    };
    if !authorized {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "upload grant is invalid, expired, or already used; request a fresh upload link",
        );
    }

    let max_bytes = ConfigManager::global_config()
        .template_build
        .files_max_upload_mib
        .saturating_mul(1024 * 1024);

    let staged = match tempfile::NamedTempFile::new() {
        Ok(staged) => staged,
        Err(error) => {
            warn!(error = %error, "failed to create staging file for build archive");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to stage build archive",
            );
        }
    };
    let staged_path = staged.path().to_path_buf();

    let mut file = match tokio::fs::File::create(&staged_path).await {
        Ok(file) => file,
        Err(error) => {
            warn!(error = %error, "failed to open staging file for build archive");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to stage build archive",
            );
        }
    };

    let mut total: u64 = 0;
    let mut stream = request.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                debug!(error = %error, "build archive upload stream aborted");
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "failed to read the uploaded archive body",
                );
            }
        };
        total += chunk.len() as u64;
        if total > max_bytes {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("build archive exceeds the configured limit of {max_bytes} bytes"),
            );
        }
        if let Err(error) = file.write_all(&chunk).await {
            warn!(error = %error, "failed to write staged build archive");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to stage build archive",
            );
        }
    }
    if let Err(error) = file.flush().await {
        warn!(error = %error, "failed to flush staged build archive");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to stage build archive",
        );
    }
    drop(file);

    if let Err(error) = store.import(&hash, &staged_path).await {
        warn!(error = %error, hash, "failed to import build archive");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to store build archive",
        );
    }

    debug!(
        template_id,
        hash,
        bytes = total,
        "stored build-context archive"
    );
    StatusCode::OK.into_response()
}

#[cfg(test)]
mod tests {
    use super::parse_upload_query;

    #[test]
    fn upload_query_parses_bearer_token_and_expiry() {
        let query = parse_upload_query(Some("expires=1234&token=upload-token"))
            .expect("query should parse");
        assert_eq!(query.expires, 1234);
        assert_eq!(query.token, "upload-token");
    }

    #[test]
    fn upload_query_requires_both_fields() {
        assert!(parse_upload_query(Some("expires=1234")).is_none());
        assert!(parse_upload_query(Some("token=upload-token")).is_none());
        assert!(parse_upload_query(None).is_none());
    }
}
