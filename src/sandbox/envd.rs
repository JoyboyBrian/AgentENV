use std::collections::HashMap;
use std::sync::LazyLock;

use anyhow::{anyhow, Result};
use tokio::time::{sleep, Duration};
use tracing::{debug, trace};

use envd::http_client::apis::{configuration::Configuration, default_api};
use envd::http_client::models::InitPostRequest;
use envd::process::ProcessClient;
use envd::reqwest::Client;

static ENVD_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);

pub(crate) struct EnvdInstance {
    config: Configuration,
    grpc_address: String,
}

impl EnvdInstance {
    pub(crate) fn new(base_path: String) -> Self {
        let grpc_address = base_path.clone();
        Self {
            // Use full construction here to ensure the shared `Client` is used and no new instances are created.
            config: Configuration {
                base_path,
                user_agent: None,
                client: ENVD_HTTP_CLIENT.clone(),
                basic_auth: None,
                oauth_access_token: None,
                bearer_access_token: None,
                api_key: None,
            },
            grpc_address,
        }
    }

    /// Create a new gRPC `ProcessClient` connected to the envd daemon.
    #[tracing::instrument(skip(self), fields(grpc_address = %self.grpc_address))]
    pub(crate) async fn process_client(&self) -> Result<ProcessClient> {
        trace!(grpc_address = %self.grpc_address, "connecting envd process client");
        let result = ProcessClient::connect(&self.grpc_address)
            .await
            .map_err(|e| anyhow!("failed to connect process client: {e}"));
        trace!("connected to envd process client");
        result
    }

    #[tracing::instrument(skip(self))]
    pub(crate) async fn wait_for_ready(
        &self,
        timeout: Duration,
        retry_interval: Duration,
    ) -> Result<()> {
        debug!(
            base_path = %self.config.base_path,
            timeout_ms = timeout.as_millis(),
            retry_interval_ms = retry_interval.as_millis(),
            "waiting for envd"
        );
        let start = std::time::Instant::now();

        loop {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Err(anyhow!("timed out waiting for envd"));
            }

            match default_api::health_get(&self.config).await {
                Ok(_) => {
                    debug!(base_path = %self.config.base_path, "envd started successfully");
                    return Ok(());
                }
                Err(_) => {
                    let remaining = timeout - elapsed;
                    sleep(std::cmp::min(retry_interval, remaining)).await;
                }
            }
        }
    }

    /// Uploads a local file into the guest at `guest_path` via envd's files
    /// API. `username` selects the guest account envd writes as; template
    /// builds pass "root" because plain OCI images have no other account.
    #[tracing::instrument(skip(self, local_path))]
    pub(crate) async fn upload_file(
        &self,
        local_path: &std::path::Path,
        guest_path: &str,
        username: &str,
    ) -> Result<()> {
        use envd::http_client::apis::files_api;

        match files_api::files_post(
            &self.config,
            Some(guest_path),
            Some(username),
            None,
            None,
            Some(local_path.to_path_buf()),
        )
        .await
        {
            Ok(_) => Ok(()),
            // envd currently returns a successful text/plain response for
            // this endpoint, while the generated client attempts to decode
            // every 2xx response as JSON. The upload has completed by the
            // time this response-body error is produced.
            Err(envd::http_client::apis::Error::Serde(error)) => {
                debug!(%error, "ignoring envd upload response-body decoding error");
                Ok(())
            }
            Err(error) => Err(anyhow!("upload file to sandbox via envd: {error}")),
        }
    }

    #[tracing::instrument(skip(self, env_vars))]
    pub(crate) async fn init(
        &self,
        env_vars: Option<HashMap<String, String>>,
        default_workdir: Option<String>,
        default_user: Option<String>,
    ) -> Result<()> {
        debug!(has_env_vars = env_vars.is_some(), "initializing envd");
        let now = chrono::Utc::now().fixed_offset();
        let init_post_request = InitPostRequest {
            env_vars,
            default_workdir,
            default_user,
            timestamp: Some(now),
            ..Default::default()
        };
        default_api::init_post(&self.config, Some(init_post_request)).await?;
        debug!("envd initialized");
        Ok(())
    }
}
