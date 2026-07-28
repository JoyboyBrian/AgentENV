use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tokio::task;
use tracing::{debug, warn};

use crate::snapshot::repository::build_files::{
    generate_upload_token, is_valid_build_files_hash, is_valid_upload_token,
    TemplateBuildFileStore, TemplateBuildUploadGrant,
};
use crate::snapshot::repository::{RepositoryError, RepositoryResult};

/// How long imported build-context archives and upload grants are retained.
/// Archives are cache entries keyed by content hash; the SDK re-uploads any
/// archive that has been pruned, so expiry only costs one extra upload.
/// Grants expire after `template_build.files_url_ttl_secs` anyway, so this
/// only bounds how long the spent grant files linger on disk.
const BUILD_FILE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const GRANTS_DIR_NAME: &str = "upload-grants";

/// Build-context archive store rooted on the shared POSIX repository.
///
/// Layout: `{repository_root}/template-build-files/{hash}.tar` plus durable
/// upload grants under `upload-grants/`. Both live on the shared filesystem,
/// so every node observes the same archives and verifies the same upload URLs.
pub(crate) struct PosixFsTemplateBuildFileStore {
    root: PathBuf,
}

impl PosixFsTemplateBuildFileStore {
    pub(crate) fn new(repository_root: &Path) -> Arc<Self> {
        Arc::new(Self {
            root: repository_root.join("template-build-files"),
        })
    }

    fn archive_path(&self, hash: &str) -> RepositoryResult<PathBuf> {
        if !is_valid_build_files_hash(hash) {
            return Err(RepositoryError::InvalidRequest {
                reason: format!("invalid build files hash '{hash}'"),
            });
        }
        Ok(self.root.join(format!("{hash}.tar")))
    }

    fn ensure_root(root: &Path) -> RepositoryResult<()> {
        fs::create_dir_all(root).map_err(|error| {
            RepositoryError::backend(
                format!("create template build files dir '{}'", root.display()),
                error,
            )
        })
    }

    /// Removes archives whose modification time is older than the retention
    /// window. Runs opportunistically on import; failures only log.
    fn prune_expired(root: &Path) {
        let cutoff = SystemTime::now() - BUILD_FILE_RETENTION;
        Self::prune_dir_older_than(root, "tar", cutoff);
    }

    /// Removes upload grants older than the retention window. Runs
    /// opportunistically whenever a new grant is written, so the grants
    /// directory stays bounded by upload-link traffic; failures only log.
    fn prune_expired_grants(root: &Path) {
        let cutoff = SystemTime::now() - BUILD_FILE_RETENTION;
        Self::prune_dir_older_than(&Self::grants_dir(root), "json", cutoff);
    }

    fn prune_dir_older_than(dir: &Path, extension: &str, cutoff: SystemTime) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != extension) {
                continue;
            }
            let expired = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map(|modified| modified < cutoff)
                .unwrap_or(false);
            if expired {
                if let Err(error) = fs::remove_file(&path) {
                    warn!(
                        path = %path.display(),
                        error = %error,
                        "failed to prune expired template build file"
                    );
                } else {
                    debug!(path = %path.display(), "pruned expired template build file");
                }
            }
        }
    }

    fn grants_dir(root: &Path) -> PathBuf {
        root.join(GRANTS_DIR_NAME)
    }

    fn grant_path(root: &Path, token: &str) -> Option<PathBuf> {
        is_valid_upload_token(token).then(|| Self::grants_dir(root).join(format!("{token}.json")))
    }

    fn write_grant(
        root: &Path,
        template_id: &str,
        hash: &str,
        expires_unix: i64,
    ) -> RepositoryResult<String> {
        let grants_dir = Self::grants_dir(root);
        fs::create_dir_all(&grants_dir).map_err(|error| {
            RepositoryError::backend(
                format!("create upload grants dir '{}'", grants_dir.display()),
                error,
            )
        })?;
        Self::prune_expired_grants(root);
        let bytes = serde_json::to_vec(&TemplateBuildUploadGrant::new(
            template_id,
            hash,
            expires_unix,
        ))
        .map_err(|error| RepositoryError::backend("serialize upload grant", error))?;

        for _ in 0..3 {
            let token = generate_upload_token();
            let path = Self::grant_path(root, &token).expect("generated token is valid");
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(&bytes)
                        .and_then(|()| file.sync_all())
                        .map_err(|error| {
                            let _ = fs::remove_file(&path);
                            RepositoryError::backend("write upload grant", error)
                        })?;
                    return Ok(token);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(RepositoryError::backend("create upload grant", error)),
            }
        }
        Err(RepositoryError::Backend {
            message: "failed to allocate a unique upload grant token".to_string(),
            source: None,
        })
    }
}

#[async_trait]
impl TemplateBuildFileStore for PosixFsTemplateBuildFileStore {
    async fn exists(&self, hash: &str) -> RepositoryResult<bool> {
        let path = self.archive_path(hash)?;
        task::spawn_blocking(move || path.exists())
            .await
            .map_err(|error| RepositoryError::backend("join build file exists task", error))
    }

    async fn import(&self, hash: &str, staged: &Path) -> RepositoryResult<()> {
        let final_path = self.archive_path(hash)?;
        let root = self.root.clone();
        let staged = staged.to_path_buf();
        task::spawn_blocking(move || -> RepositoryResult<()> {
            Self::ensure_root(&root)?;
            Self::prune_expired(&root);
            // Copy into the store filesystem first (the staged file usually
            // lives on node-local tmp), then rename within the store directory
            // so readers only ever observe complete archives.
            let store_staged = root.join(format!(".import-{}.tmp", uuid::Uuid::new_v4()));
            fs::copy(&staged, &store_staged).map_err(|error| {
                let _ = fs::remove_file(&store_staged);
                RepositoryError::backend("copy build archive into store", error)
            })?;
            fs::rename(&store_staged, &final_path).map_err(|error| {
                let _ = fs::remove_file(&store_staged);
                RepositoryError::backend("publish build archive", error)
            })
        })
        .await
        .map_err(|error| RepositoryError::backend("join build file import task", error))?
    }

    async fn materialize(
        &self,
        hash: &str,
        _scratch_dir: &Path,
    ) -> RepositoryResult<Option<PathBuf>> {
        let path = self.archive_path(hash)?;
        task::spawn_blocking(move || path.exists().then_some(path))
            .await
            .map_err(|error| RepositoryError::backend("join build file materialize task", error))
    }

    async fn create_upload_grant(
        &self,
        template_id: &str,
        hash: &str,
        expires_unix: i64,
    ) -> RepositoryResult<String> {
        let root = self.root.clone();
        let template_id = template_id.to_string();
        let hash = hash.to_string();
        task::spawn_blocking(move || Self::write_grant(&root, &template_id, &hash, expires_unix))
            .await
            .map_err(|error| RepositoryError::backend("join create upload grant task", error))?
    }

    async fn validate_upload_grant(
        &self,
        token: &str,
        template_id: &str,
        hash: &str,
        expires_unix: i64,
        now_unix: i64,
    ) -> RepositoryResult<bool> {
        let Some(path) = Self::grant_path(&self.root, token) else {
            return Ok(false);
        };
        let template_id = template_id.to_string();
        let hash = hash.to_string();
        task::spawn_blocking(move || {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => {
                    return Err(RepositoryError::backend("read upload grant", error));
                }
            };
            let grant: TemplateBuildUploadGrant = serde_json::from_slice(&bytes)
                .map_err(|error| RepositoryError::backend("parse upload grant", error))?;
            Ok(grant.authorizes(&template_id, &hash, expires_unix, now_unix))
        })
        .await
        .map_err(|error| RepositoryError::backend("join validate upload grant task", error))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const HASH: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn staged_file(dir: &Path, contents: &[u8]) -> PathBuf {
        let path = dir.join("staged.tar");
        fs::write(&path, contents).expect("write staged file");
        path
    }

    #[tokio::test]
    async fn import_then_exists_and_materialize() {
        let tempdir = TempDir::new().expect("tempdir");
        let store = PosixFsTemplateBuildFileStore::new(tempdir.path());

        assert!(!store.exists(HASH).await.expect("exists should work"));
        assert_eq!(
            store
                .materialize(HASH, tempdir.path())
                .await
                .expect("materialize should work"),
            None
        );

        let staged = staged_file(tempdir.path(), b"tar-bytes");
        store
            .import(HASH, &staged)
            .await
            .expect("import should work");

        assert!(store.exists(HASH).await.expect("exists should work"));
        let materialized = store
            .materialize(HASH, tempdir.path())
            .await
            .expect("materialize should work")
            .expect("archive should exist");
        assert_eq!(
            fs::read(materialized).expect("read materialized"),
            b"tar-bytes"
        );
    }

    #[tokio::test]
    async fn import_rejects_invalid_hash() {
        let tempdir = TempDir::new().expect("tempdir");
        let store = PosixFsTemplateBuildFileStore::new(tempdir.path());
        let staged = staged_file(tempdir.path(), b"tar-bytes");

        let err = store
            .import("../escape", &staged)
            .await
            .expect_err("invalid hash should fail");
        assert!(matches!(err, RepositoryError::InvalidRequest { .. }));
    }

    #[tokio::test]
    async fn writing_a_grant_prunes_expired_grant_files() {
        let tempdir = TempDir::new().expect("tempdir");
        let store = PosixFsTemplateBuildFileStore::new(tempdir.path());

        let fresh_token = store
            .create_upload_grant("template", HASH, i64::MAX)
            .await
            .expect("fresh grant should be created");

        // Plant a grant file that predates the retention window.
        let grants_dir = tempdir
            .path()
            .join("template-build-files")
            .join("upload-grants");
        let stale_path = grants_dir.join(format!("{}.json", generate_upload_token()));
        fs::write(&stale_path, b"{}").expect("write stale grant");
        let stale_mtime = SystemTime::now() - BUILD_FILE_RETENTION - Duration::from_secs(60);
        let stale_file = fs::File::options()
            .write(true)
            .open(&stale_path)
            .expect("open stale grant");
        stale_file
            .set_times(fs::FileTimes::new().set_modified(stale_mtime))
            .expect("set stale mtime");
        drop(stale_file);

        store
            .create_upload_grant("template", HASH, i64::MAX)
            .await
            .expect("new grant should be created");

        assert!(!stale_path.exists(), "expired grant file should be pruned");
        assert!(
            store
                .validate_upload_grant(&fresh_token, "template", HASH, i64::MAX, 0)
                .await
                .expect("validation should work"),
            "unexpired grants must survive pruning"
        );
    }

    #[tokio::test]
    async fn upload_grant_is_shared_across_instances() {
        let tempdir = TempDir::new().expect("tempdir");
        let first = PosixFsTemplateBuildFileStore::new(tempdir.path());
        let token = first
            .create_upload_grant("template", HASH, 1000)
            .await
            .expect("grant should be created");
        let second = PosixFsTemplateBuildFileStore::new(tempdir.path());
        assert!(second
            .validate_upload_grant(&token, "template", HASH, 1000, 999)
            .await
            .expect("grant should validate"));
        assert!(!second
            .validate_upload_grant(&token, "other", HASH, 1000, 999)
            .await
            .expect("mismatched grant should be rejected"));
        assert!(!second
            .validate_upload_grant(&token, "template", HASH, 1000, 1001)
            .await
            .expect("expired grant should be rejected"));
    }
}
