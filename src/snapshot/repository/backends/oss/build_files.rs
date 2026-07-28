use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use super::client::OssClient;
use crate::snapshot::repository::build_files::{
    generate_upload_token, is_valid_build_files_hash, is_valid_upload_token,
    TemplateBuildFileStore, TemplateBuildUploadGrant,
};
use crate::snapshot::repository::{RepositoryError, RepositoryResult};

const BUILD_FILES_PREFIX: &str = "template-build-files";

/// Build-context archive store backed by the OSS repository bucket.
///
/// Layout: `template-build-files/{hash}.tar` plus durable bearer grants under
/// `template-build-files/upload-grants/`. Retention is delegated to bucket
/// lifecycle rules; archives are cache entries the SDK re-uploads when absent.
pub(crate) struct OssTemplateBuildFileStore {
    client: Arc<OssClient>,
}

impl OssTemplateBuildFileStore {
    pub(crate) fn new(client: Arc<OssClient>) -> Arc<Self> {
        Arc::new(Self { client })
    }

    fn archive_key(hash: &str) -> RepositoryResult<String> {
        if !is_valid_build_files_hash(hash) {
            return Err(RepositoryError::InvalidRequest {
                reason: format!("invalid build files hash '{hash}'"),
            });
        }
        Ok(format!("{BUILD_FILES_PREFIX}/{hash}.tar"))
    }

    fn grant_key(token: &str) -> Option<String> {
        is_valid_upload_token(token)
            .then(|| format!("{BUILD_FILES_PREFIX}/upload-grants/{token}.json"))
    }
}

#[async_trait]
impl TemplateBuildFileStore for OssTemplateBuildFileStore {
    async fn exists(&self, hash: &str) -> RepositoryResult<bool> {
        let key = Self::archive_key(hash)?;
        self.client
            .exists(&key)
            .await
            .map_err(|error| RepositoryError::backend("check build archive", error))
    }

    async fn import(&self, hash: &str, staged: &Path) -> RepositoryResult<()> {
        let key = Self::archive_key(hash)?;
        self.client
            .put_file(&key, staged)
            .await
            .map_err(|error| RepositoryError::backend("upload build archive", error))
    }

    async fn materialize(
        &self,
        hash: &str,
        scratch_dir: &Path,
    ) -> RepositoryResult<Option<PathBuf>> {
        let key = Self::archive_key(hash)?;
        if !self
            .client
            .exists(&key)
            .await
            .map_err(|error| RepositoryError::backend("check build archive", error))?
        {
            return Ok(None);
        }
        let dest = scratch_dir.join(format!("{hash}.tar"));
        self.client
            .get_to_file(&key, &dest)
            .await
            .map_err(|error| RepositoryError::backend("download build archive", error))?;
        Ok(Some(dest))
    }

    async fn create_upload_grant(
        &self,
        template_id: &str,
        hash: &str,
        expires_unix: i64,
    ) -> RepositoryResult<String> {
        let token = generate_upload_token();
        let key = Self::grant_key(&token).expect("generated token is valid");
        let grant = serde_json::to_vec(&TemplateBuildUploadGrant::new(
            template_id,
            hash,
            expires_unix,
        ))
        .map_err(|error| RepositoryError::backend("serialize upload grant", error))?;
        self.client
            .put_bytes(&key, grant)
            .await
            .map_err(|error| RepositoryError::backend("write upload grant", error))?;
        Ok(token)
    }

    async fn validate_upload_grant(
        &self,
        token: &str,
        template_id: &str,
        hash: &str,
        expires_unix: i64,
        now_unix: i64,
    ) -> RepositoryResult<bool> {
        let Some(key) = Self::grant_key(token) else {
            return Ok(false);
        };
        let bytes = match self.client.get_bytes(&key).await {
            Ok(bytes) => bytes,
            Err(error) if OssClient::is_not_found_error(&error) => return Ok(false),
            Err(error) => return Err(RepositoryError::backend("read upload grant", error)),
        };
        let grant: TemplateBuildUploadGrant = serde_json::from_slice(&bytes)
            .map_err(|error| RepositoryError::backend("parse upload grant", error))?;
        Ok(grant.authorizes(template_id, hash, expires_unix, now_unix))
    }
}
