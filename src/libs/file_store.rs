//! Persistent local storage backing the compatibility Files API.
//!
//! Copilot does not expose an upstream Files API. Uploaded bytes therefore stay
//! under `COPILOT_API_HOME/files`; only an inline base64 representation is sent
//! upstream when a request references one of our opaque local IDs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use chrono::Utc;
use once_cell::sync::Lazy;
use rusqlite::{params, Connection, TransactionBehavior};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::libs::error::{AppError, HttpError};
use crate::libs::paths::{set_permissions_600, set_permissions_700, PATHS};

pub const LOCAL_FILE_ID_PREFIX: &str = "file_local_";
pub const DEFAULT_MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
pub const DEFAULT_MAX_OWNER_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_MAX_OWNER_FILES: u64 = 1_000;
pub const DEFAULT_RETENTION_DAYS: i64 = 30;
pub const MAX_FILENAME_BYTES: usize = 1_024;
pub const MAX_MIME_TYPE_BYTES: usize = 127;
pub const MAX_PURPOSE_BYTES: usize = 64;

const MAX_FILE_BYTES_ENV: &str = "COPILOT_API_FILE_MAX_BYTES";
const MAX_OWNER_BYTES_ENV: &str = "COPILOT_API_FILE_MAX_OWNER_BYTES";
const MAX_OWNER_FILES_ENV: &str = "COPILOT_API_FILE_MAX_OWNER_COUNT";
const RETENTION_DAYS_ENV: &str = "COPILOT_API_FILE_RETENTION_DAYS";
const STALE_LIFECYCLE_SECONDS: i64 = 5 * 60;

#[derive(Debug, Clone, Copy)]
pub struct FileStoreLimits {
    pub max_file_bytes: u64,
    pub max_owner_bytes: u64,
    pub max_owner_files: u64,
    pub retention_seconds: Option<i64>,
}

impl Default for FileStoreLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_owner_bytes: DEFAULT_MAX_OWNER_BYTES,
            max_owner_files: DEFAULT_MAX_OWNER_FILES,
            retention_seconds: Some(DEFAULT_RETENTION_DAYS * 24 * 60 * 60),
        }
    }
}

impl FileStoreLimits {
    fn from_env() -> Self {
        let defaults = Self::default();
        let router_ceiling =
            crate::libs::http::MAX_REQUEST_BODY_BYTES.saturating_sub(1024 * 1024) as u64;
        let max_file_bytes = positive_env_u64(MAX_FILE_BYTES_ENV)
            .unwrap_or(defaults.max_file_bytes)
            .min(router_ceiling);
        let max_owner_bytes = positive_env_u64(MAX_OWNER_BYTES_ENV)
            .unwrap_or(defaults.max_owner_bytes)
            .max(max_file_bytes);
        let max_owner_files = positive_env_u64(MAX_OWNER_FILES_ENV)
            .unwrap_or(defaults.max_owner_files)
            .min(DEFAULT_MAX_OWNER_FILES);
        let retention_days =
            nonnegative_env_i64(RETENTION_DAYS_ENV).unwrap_or(DEFAULT_RETENTION_DAYS);
        Self {
            max_file_bytes,
            max_owner_bytes,
            max_owner_files,
            retention_seconds: (retention_days > 0)
                .then_some(retention_days.saturating_mul(24 * 60 * 60)),
        }
    }
}

fn positive_env_u64(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    match raw.trim().parse::<u64>() {
        Ok(value) if value > 0 => Some(value),
        _ => {
            tracing::warn!("{name} must be a positive integer; using the default");
            None
        }
    }
}

fn nonnegative_env_i64(name: &str) -> Option<i64> {
    let raw = std::env::var(name).ok()?;
    match raw.trim().parse::<i64>() {
        Ok(value) if value >= 0 => Some(value),
        _ => {
            tracing::warn!("{name} must be a non-negative integer; using the default");
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFile {
    pub id: String,
    pub owner: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub purpose: Option<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct StoredFileData {
    pub metadata: StoredFile,
    pub bytes: Bytes,
}

#[derive(Debug)]
pub struct StoredFileStream {
    pub metadata: StoredFile,
    pub file: tokio::fs::File,
}

#[derive(Debug, Clone, Default)]
pub struct FileListOptions {
    pub limit: usize,
    pub after: Option<String>,
    pub before: Option<String>,
    pub purpose: Option<String>,
    pub descending: bool,
}

#[derive(Debug, Clone)]
pub struct StoredFilePage {
    pub data: Vec<StoredFile>,
    pub has_more: bool,
}

#[derive(Debug, Error)]
pub enum FileStoreError {
    #[error("File '{0}' was not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("File exceeds the {max_bytes}-byte upload limit")]
    TooLarge { max_bytes: u64 },
    #[error("File storage quota exceeded for this API key ({max_bytes} bytes)")]
    QuotaExceeded { max_bytes: u64 },
    #[error("File count quota exceeded for this API key ({max_files} files)")]
    FileCountQuotaExceeded { max_files: u64 },
    #[error("Stored file '{id}' is corrupt: {reason}")]
    Corrupt { id: String, reason: String },
    #[error("File storage I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("File metadata database failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("File storage task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl From<FileStoreError> for AppError {
    fn from(error: FileStoreError) -> Self {
        match error {
            FileStoreError::NotFound(id) => AppError::Http(HttpError::new(
                format!("File '{id}' was not found"),
                StatusCode::NOT_FOUND,
                HeaderMap::new(),
                String::new(),
            )),
            FileStoreError::Invalid(message) => AppError::BadRequest(message),
            FileStoreError::TooLarge { max_bytes } => AppError::Http(HttpError::new(
                format!("File exceeds the {max_bytes}-byte upload limit"),
                StatusCode::PAYLOAD_TOO_LARGE,
                HeaderMap::new(),
                String::new(),
            )),
            FileStoreError::QuotaExceeded { max_bytes } => AppError::Http(HttpError::new(
                format!("File storage quota exceeded for this API key ({max_bytes} bytes)"),
                StatusCode::PAYLOAD_TOO_LARGE,
                HeaderMap::new(),
                String::new(),
            )),
            FileStoreError::FileCountQuotaExceeded { max_files } => AppError::Http(HttpError::new(
                format!("File count quota exceeded for this API key ({max_files} files)"),
                StatusCode::PAYLOAD_TOO_LARGE,
                HeaderMap::new(),
                String::new(),
            )),
            internal => AppError::Other(anyhow::Error::new(internal)),
        }
    }
}

#[derive(Debug)]
pub struct FileStore {
    root: PathBuf,
    data_dir: PathBuf,
    database_path: PathBuf,
    limits: FileStoreLimits,
    reconciled: OnceCell<()>,
}

impl FileStore {
    pub fn new(root: PathBuf, limits: FileStoreLimits) -> Self {
        Self {
            data_dir: root.join("data"),
            database_path: root.join("metadata.sqlite3"),
            root,
            limits,
            reconciled: OnceCell::new(),
        }
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.limits.max_file_bytes
    }

    pub async fn create(
        &self,
        owner: &str,
        filename: String,
        mime_type: String,
        purpose: Option<String>,
        bytes: Bytes,
    ) -> Result<StoredFile, FileStoreError> {
        validate_owner(owner)?;
        if bytes.is_empty() {
            return Err(FileStoreError::Invalid(
                "The uploaded file must not be empty".to_string(),
            ));
        }
        if bytes.len() as u64 > self.limits.max_file_bytes {
            return Err(FileStoreError::TooLarge {
                max_bytes: self.limits.max_file_bytes,
            });
        }
        if filename.trim().is_empty() {
            return Err(FileStoreError::Invalid(
                "The uploaded file must have a filename".to_string(),
            ));
        }
        if filename.len() > MAX_FILENAME_BYTES {
            return Err(FileStoreError::Invalid(format!(
                "The uploaded filename must not exceed {MAX_FILENAME_BYTES} bytes"
            )));
        }
        if mime_type.trim().is_empty() {
            return Err(FileStoreError::Invalid(
                "The uploaded file must have a MIME type".to_string(),
            ));
        }
        if mime_type.len() > MAX_MIME_TYPE_BYTES {
            return Err(FileStoreError::Invalid(format!(
                "The uploaded MIME type must not exceed {MAX_MIME_TYPE_BYTES} bytes"
            )));
        }
        if purpose
            .as_deref()
            .is_some_and(|value| value.len() > MAX_PURPOSE_BYTES)
        {
            return Err(FileStoreError::Invalid(format!(
                "The upload purpose must not exceed {MAX_PURPOSE_BYTES} bytes"
            )));
        }

        self.ensure_layout().await?;
        self.prune_expired().await?;

        let id = format!("{LOCAL_FILE_ID_PREFIX}{}", Uuid::new_v4().simple());
        let created_at = Utc::now().timestamp();
        let expires_at = self
            .limits
            .retention_seconds
            .map(|seconds| created_at.saturating_add(seconds));
        let sha256 = hex::encode(Sha256::digest(&bytes));
        let metadata = StoredFile {
            id: id.clone(),
            owner: owner.to_string(),
            filename,
            mime_type,
            size_bytes: bytes.len() as u64,
            sha256,
            purpose,
            created_at,
            expires_at,
        };
        self.reserve_metadata(metadata.clone()).await?;

        let temp_path = self
            .data_dir
            .join(format!(".{id}.{}.tmp", Uuid::new_v4().simple()));
        let final_path = self.data_path(&id);
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let file = match options.open(&temp_path).await {
            Ok(file) => file,
            Err(error) => {
                if let Err(cleanup_error) = self.remove_metadata_record(&id).await {
                    tracing::warn!(
                        file_id = %id,
                        error = %cleanup_error,
                        "Could not roll back reserved file metadata; stale cleanup will retry"
                    );
                }
                return Err(FileStoreError::Io(error));
            }
        };
        drop(file);
        if let Err(error) = set_permissions_600(&temp_path).await {
            let _ = tokio::fs::remove_file(&temp_path).await;
            if let Err(cleanup_error) = self.remove_metadata_record(&id).await {
                tracing::warn!(
                    file_id = %id,
                    error = %cleanup_error,
                    "Could not roll back reserved file metadata; stale cleanup will retry"
                );
            }
            return Err(FileStoreError::Io(error));
        }
        let mut file = match tokio::fs::OpenOptions::new()
            .write(true)
            .open(&temp_path)
            .await
        {
            Ok(file) => file,
            Err(error) => {
                let _ = tokio::fs::remove_file(&temp_path).await;
                if let Err(cleanup_error) = self.remove_metadata_record(&id).await {
                    tracing::warn!(
                        file_id = %id,
                        error = %cleanup_error,
                        "Could not roll back reserved file metadata; stale cleanup will retry"
                    );
                }
                return Err(FileStoreError::Io(error));
            }
        };
        if let Err(error) = async {
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            tokio::fs::rename(&temp_path, &final_path).await?;
            sync_directory(&self.data_dir).await
        }
        .await
        {
            for path in [&temp_path, &final_path] {
                if let Err(cleanup_error) = tokio::fs::remove_file(path).await {
                    if cleanup_error.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(
                            file_id = %id,
                            path = %path.display(),
                            error = %cleanup_error,
                            "Could not remove failed file content; stale cleanup will retry"
                        );
                    }
                }
            }
            if let Err(cleanup_error) = self.remove_metadata_record(&id).await {
                tracing::warn!(
                    file_id = %id,
                    error = %cleanup_error,
                    "Could not roll back reserved file metadata; stale cleanup will retry"
                );
            }
            return Err(FileStoreError::Io(error));
        }

        if let Err(error) = self.mark_metadata_ready(&id).await {
            if let Err(cleanup_error) = tokio::fs::remove_file(&final_path).await {
                if cleanup_error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        file_id = %id,
                        error = %cleanup_error,
                        "Could not remove uncommitted file content; reconciliation will retry"
                    );
                }
            }
            if let Err(cleanup_error) = self.remove_metadata_record(&id).await {
                tracing::warn!(
                    file_id = %id,
                    error = %cleanup_error,
                    "Could not roll back reserved file metadata; stale cleanup will retry"
                );
            }
            return Err(error);
        }
        Ok(metadata)
    }

    pub async fn metadata(&self, owner: &str, id: &str) -> Result<StoredFile, FileStoreError> {
        validate_owner(owner)?;
        validate_local_id(id)?;
        self.ensure_layout().await?;
        let owner = owner.to_string();
        let id = id.to_string();
        let now = Utc::now().timestamp();
        self.with_database(move |connection| {
            connection
                .query_row(
                    "SELECT id, owner, filename, mime_type, size_bytes, sha256, purpose, \
                     created_at, expires_at
                     FROM local_files
                     WHERE owner = ?1 AND id = ?2
                       AND state = 'ready'
                       AND (expires_at IS NULL OR expires_at > ?3)",
                    params![owner, id, now],
                    stored_file_from_row,
                )
                .map_err(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => FileStoreError::NotFound(id),
                    other => FileStoreError::Database(other),
                })
        })
        .await
    }

    pub async fn read(&self, owner: &str, id: &str) -> Result<StoredFileData, FileStoreError> {
        let metadata = self.metadata(owner, id).await?;
        let bytes = match tokio::fs::read(self.data_path(&metadata.id)).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(FileStoreError::Corrupt {
                    id: metadata.id,
                    reason: "content is missing".to_string(),
                })
            }

            Err(error) => return Err(FileStoreError::Io(error)),
        };
        if bytes.len() as u64 != metadata.size_bytes {
            return Err(FileStoreError::Corrupt {
                id: metadata.id,
                reason: "content length does not match metadata".to_string(),
            });
        }
        let digest = hex::encode(Sha256::digest(&bytes));
        if digest != metadata.sha256 {
            return Err(FileStoreError::Corrupt {
                id: metadata.id,
                reason: "content digest does not match metadata".to_string(),
            });
        }
        Ok(StoredFileData {
            metadata,
            bytes: Bytes::from(bytes),
        })
    }

    pub async fn open_content(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<StoredFileStream, FileStoreError> {
        let metadata = self.metadata(owner, id).await?;
        let file = match tokio::fs::File::open(self.data_path(&metadata.id)).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(FileStoreError::Corrupt {
                    id: metadata.id,
                    reason: "content is missing".to_string(),
                })
            }
            Err(error) => return Err(FileStoreError::Io(error)),
        };
        let actual_size = file.metadata().await?.len();
        if actual_size != metadata.size_bytes {
            return Err(FileStoreError::Corrupt {
                id: metadata.id,
                reason: "content length does not match metadata".to_string(),
            });
        }
        Ok(StoredFileStream { metadata, file })
    }

    pub async fn list(
        &self,
        owner: &str,
        mut options: FileListOptions,
    ) -> Result<StoredFilePage, FileStoreError> {
        validate_owner(owner)?;
        if options.after.is_some() && options.before.is_some() {
            return Err(FileStoreError::Invalid(
                "after and before cursors cannot be used together".to_string(),
            ));
        }
        options.limit = options.limit.clamp(1, DEFAULT_MAX_OWNER_FILES as usize);
        self.ensure_layout().await?;
        self.prune_expired().await?;

        let owner = owner.to_string();
        let purpose = options.purpose.clone();
        let now = Utc::now().timestamp();
        let mut files = self
            .with_database(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT id, owner, filename, mime_type, size_bytes, sha256, purpose, \
                     created_at, expires_at
                     FROM local_files
                     WHERE owner = ?1 AND state = 'ready'
                       AND (expires_at IS NULL OR expires_at > ?2)
                     ORDER BY created_at DESC, id DESC
                     LIMIT 1001",
                )?;
                let rows = statement.query_map(params![owner, now], stored_file_from_row)?;
                let mut files = Vec::new();
                for row in rows {
                    let file = row?;
                    if purpose
                        .as_deref()
                        .is_none_or(|value| file.purpose.as_deref().unwrap_or("user_data") == value)
                    {
                        files.push(file);
                    }
                }
                Ok(files)
            })
            .await?;
        if !options.descending {
            files.reverse();
        }

        let cursor_position = |cursor: &str| files.iter().position(|file| file.id == cursor);
        let candidates = if let Some(after) = options.after.as_deref() {
            let index = cursor_position(after)
                .ok_or_else(|| FileStoreError::NotFound(after.to_string()))?;
            files[index + 1..].to_vec()
        } else if let Some(before) = options.before.as_deref() {
            let index = cursor_position(before)
                .ok_or_else(|| FileStoreError::NotFound(before.to_string()))?;
            let start = index.saturating_sub(options.limit + 1);
            files[start..index].to_vec()
        } else {
            files
        };

        let has_more = candidates.len() > options.limit;
        let data = if options.before.is_some() && has_more {
            candidates[candidates.len() - options.limit..].to_vec()
        } else {
            candidates.into_iter().take(options.limit).collect()
        };
        Ok(StoredFilePage { data, has_more })
    }

    pub async fn delete(&self, owner: &str, id: &str) -> Result<StoredFile, FileStoreError> {
        let metadata = self.metadata(owner, id).await?;
        self.mark_metadata_deleting(owner, id).await?;
        let final_path = self.data_path(id);
        let tombstone = self
            .data_dir
            .join(format!(".{id}.{}.deleting", Uuid::new_v4().simple()));
        match tokio::fs::rename(&final_path, &tombstone).await {
            Ok(()) => {
                sync_directory(&self.data_dir).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let _ = self.remove_metadata_record(id).await;
                return Err(FileStoreError::Corrupt {
                    id: id.to_string(),
                    reason: "content is missing".to_string(),
                });
            }
            Err(error) => {
                let _ = self.mark_metadata_ready(id).await;
                return Err(FileStoreError::Io(error));
            }
        }

        let delete_result = self.remove_metadata_record(id).await;
        if let Err(error) = delete_result {
            if let Err(restore_error) = tokio::fs::rename(&tombstone, &final_path).await {
                tracing::error!(
                    file_id = %metadata.id,
                    error = %restore_error,
                    "Failed to restore file content after metadata deletion failed"
                );
            } else {
                let _ = sync_directory(&self.data_dir).await;
                let _ = self.mark_metadata_ready(id).await;
            }
            return Err(error);
        }
        if let Err(error) = tokio::fs::remove_file(&tombstone).await {
            tracing::warn!(
                file_id = %metadata.id,
                error = %error,
                "Deleted file metadata but could not remove tombstoned content"
            );
        } else {
            sync_directory(&self.data_dir).await?;
        }
        Ok(metadata)
    }

    async fn ensure_layout(&self) -> Result<(), FileStoreError> {
        self.reconciled
            .get_or_try_init(|| async {
                tokio::fs::create_dir_all(&self.root).await?;
                set_permissions_700(&self.root).await?;
                tokio::fs::create_dir_all(&self.data_dir).await?;
                set_permissions_700(&self.data_dir).await?;
                self.reconcile().await?;
                Ok::<(), FileStoreError>(())
            })
            .await?;
        Ok(())
    }

    async fn reserve_metadata(&self, metadata: StoredFile) -> Result<(), FileStoreError> {
        let max_owner_bytes = self.limits.max_owner_bytes;
        let max_owner_files = self.limits.max_owner_files;
        self.with_database(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (used, count): (i64, i64) = transaction.query_row(
                "SELECT COALESCE(SUM(size_bytes), 0), COUNT(*)
                 FROM local_files WHERE owner = ?1",
                params![metadata.owner],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let size_bytes = i64::try_from(metadata.size_bytes).map_err(|_| {
                FileStoreError::Invalid("File size is too large to persist".to_string())
            })?;
            let max_owner_bytes_i64 = i64::try_from(max_owner_bytes).unwrap_or(i64::MAX);
            if used.saturating_add(size_bytes) > max_owner_bytes_i64 {
                return Err(FileStoreError::QuotaExceeded {
                    max_bytes: max_owner_bytes,
                });
            }
            let max_owner_files_i64 = i64::try_from(max_owner_files).unwrap_or(i64::MAX);
            if count.saturating_add(1) > max_owner_files_i64 {
                return Err(FileStoreError::FileCountQuotaExceeded {
                    max_files: max_owner_files,
                });
            }
            transaction.execute(
                "INSERT INTO local_files (
                    id, owner, filename, mime_type, size_bytes, sha256, purpose,
                    created_at, expires_at, state
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending')",
                params![
                    metadata.id,
                    metadata.owner,
                    metadata.filename,
                    metadata.mime_type,
                    size_bytes,
                    metadata.sha256,
                    metadata.purpose,
                    metadata.created_at,
                    metadata.expires_at,
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    async fn mark_metadata_ready(&self, id: &str) -> Result<(), FileStoreError> {
        let id = id.to_string();
        self.with_database(move |connection| {
            let affected = connection.execute(
                "UPDATE local_files SET state = 'ready' WHERE id = ?1",
                params![id],
            )?;
            if affected == 0 {
                return Err(FileStoreError::NotFound(id));
            }
            Ok(())
        })
        .await
    }

    async fn mark_metadata_deleting(&self, owner: &str, id: &str) -> Result<(), FileStoreError> {
        let owner = owner.to_string();
        let id = id.to_string();
        self.with_database(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let affected = transaction.execute(
                "UPDATE local_files SET state = 'deleting'
                 WHERE owner = ?1 AND id = ?2 AND state = 'ready'",
                params![owner, id],
            )?;
            if affected == 0 {
                return Err(FileStoreError::NotFound(id));
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    async fn remove_metadata_record(&self, id: &str) -> Result<(), FileStoreError> {
        let id = id.to_string();
        self.with_database(move |connection| {
            connection.execute("DELETE FROM local_files WHERE id = ?1", params![id])?;
            Ok(())
        })
        .await
    }

    async fn prune_expired(&self) -> Result<(), FileStoreError> {
        let now = Utc::now().timestamp();
        let stale_before = now.saturating_sub(STALE_LIFECYCLE_SECONDS);
        let expired = self
            .with_database(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let expired = {
                    let mut statement = transaction.prepare(
                        "SELECT id FROM local_files
                         WHERE expires_at <= ?1
                            OR (state != 'ready' AND created_at <= ?2)",
                    )?;
                    let rows = statement
                        .query_map(params![now, stale_before], |row| row.get::<_, String>(0))?;
                    let mut ids = Vec::new();
                    for row in rows {
                        ids.push(row?);
                    }
                    ids
                };
                transaction.execute(
                    "UPDATE local_files SET state = 'deleting'
                     WHERE expires_at <= ?1
                        OR (state != 'ready' AND created_at <= ?2)",
                    params![now, stale_before],
                )?;
                transaction.commit()?;
                Ok(expired)
            })
            .await?;
        for id in expired {
            match tokio::fs::remove_file(self.data_path(&id)).await {
                Ok(()) => {
                    self.remove_metadata_record(&id).await?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.remove_metadata_record(&id).await?;
                }
                Err(error) => {
                    tracing::warn!(
                        file_id = %id,
                        error = %error,
                        "Could not remove expired file content; cleanup will retry"
                    );
                }
            }
        }
        self.prune_stale_temp_files().await?;
        Ok(())
    }

    async fn prune_stale_temp_files(&self) -> Result<(), FileStoreError> {
        let mut changed = false;
        let mut entries = tokio::fs::read_dir(&self.data_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            if !name.to_string_lossy().ends_with(".tmp") {
                continue;
            }
            let stale = entry
                .metadata()
                .await?
                .modified()?
                .elapsed()
                .is_ok_and(|age| age.as_secs() >= STALE_LIFECYCLE_SECONDS as u64);
            if stale {
                tokio::fs::remove_file(entry.path()).await?;
                changed = true;
            }
        }
        if changed {
            sync_directory(&self.data_dir).await?;
        }
        Ok(())
    }

    async fn reconcile(&self) -> Result<(), FileStoreError> {
        let states = self
            .with_database(|connection| {
                let mut statement = connection.prepare("SELECT id, state FROM local_files")?;
                let rows = statement.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                let mut states = HashMap::new();
                for row in rows {
                    let (id, state) = row?;
                    states.insert(id, state);
                }
                Ok(states)
            })
            .await?;

        let mut changed_directory = false;
        let mut entries = tokio::fs::read_dir(&self.data_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".tmp") {
                tokio::fs::remove_file(entry.path()).await?;
                changed_directory = true;
                continue;
            }
            if let Some(id) = tombstone_file_id(&name) {
                let final_path = self.data_path(id);
                match states.get(id).map(String::as_str) {
                    Some("pending" | "ready")
                        if tokio::fs::metadata(&final_path).await.is_err() =>
                    {
                        tokio::fs::rename(entry.path(), &final_path).await?;
                        self.mark_metadata_ready(id).await?;
                    }
                    Some("deleting") => {
                        tokio::fs::remove_file(entry.path()).await?;
                        self.remove_metadata_record(id).await?;
                    }
                    _ => {
                        tokio::fs::remove_file(entry.path()).await?;
                    }
                }
                changed_directory = true;
                continue;
            }
            let Some(id) = name.strip_suffix(".bin") else {
                continue;
            };
            match states.get(id).map(String::as_str) {
                None => {
                    tokio::fs::remove_file(entry.path()).await?;
                    changed_directory = true;
                }
                Some("pending") => {
                    self.mark_metadata_ready(id).await?;
                }
                Some("deleting") => {
                    tokio::fs::remove_file(entry.path()).await?;
                    self.remove_metadata_record(id).await?;
                    changed_directory = true;
                }
                Some("ready") => {}
                Some(_) => {
                    return Err(FileStoreError::Corrupt {
                        id: id.to_string(),
                        reason: "metadata has an unknown lifecycle state".to_string(),
                    })
                }
            }
        }

        for (id, state) in &states {
            if matches!(state.as_str(), "pending" | "deleting")
                && tokio::fs::metadata(self.data_path(id)).await.is_err()
            {
                self.remove_metadata_record(id).await?;
            }
        }
        if changed_directory {
            sync_directory(&self.data_dir).await?;
        }
        Ok(())
    }

    async fn with_database<T, F>(&self, operation: F) -> Result<T, FileStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, FileStoreError> + Send + 'static,
    {
        let database_path = self.database_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_database(&database_path)?;
            operation(&mut connection)
        })
        .await?
    }

    fn data_path(&self, id: &str) -> PathBuf {
        self.data_dir.join(format!("{id}.bin"))
    }
}

fn open_database(path: &Path) -> Result<Connection, FileStoreError> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA busy_timeout = 5000;
         CREATE TABLE IF NOT EXISTS local_files (
             id TEXT PRIMARY KEY,
             owner TEXT NOT NULL,
             filename TEXT NOT NULL,
             mime_type TEXT NOT NULL,
             size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
             sha256 TEXT NOT NULL,
             purpose TEXT,
             created_at INTEGER NOT NULL,
             expires_at INTEGER,
             state TEXT NOT NULL DEFAULT 'ready'
                 CHECK (state IN ('pending', 'ready', 'deleting'))
         );
         CREATE INDEX IF NOT EXISTS idx_local_files_owner_created
             ON local_files(owner, created_at DESC, id DESC);
         CREATE INDEX IF NOT EXISTS idx_local_files_expiry
             ON local_files(expires_at);",
    )?;
    ensure_database_column(&connection, "state", "TEXT NOT NULL DEFAULT 'ready'")?;
    set_database_permissions(path);
    Ok(connection)
}

fn ensure_database_column(
    connection: &Connection,
    name: &str,
    definition: &str,
) -> Result<(), rusqlite::Error> {
    let mut statement = connection.prepare("PRAGMA table_info(local_files)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|column| column == name) {
        connection.execute_batch(&format!(
            "ALTER TABLE local_files ADD COLUMN {name} {definition};"
        ))?;
    }
    Ok(())
}

fn tombstone_file_id(name: &str) -> Option<&str> {
    let value = name.strip_prefix('.')?.strip_suffix(".deleting")?;
    let (id, nonce) = value.rsplit_once('.')?;
    (is_local_file_id(id)
        && nonce.len() == 32
        && nonce.bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(id)
}

#[cfg(unix)]
async fn sync_directory(path: &Path) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all())
        .await
        .map_err(std::io::Error::other)?
}

#[cfg(not(unix))]
async fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_database_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_database_permissions(_path: &Path) {}

fn stored_file_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredFile> {
    let size_bytes: i64 = row.get(4)?;
    let size_bytes = u64::try_from(size_bytes).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    Ok(StoredFile {
        id: row.get(0)?,
        owner: row.get(1)?,
        filename: row.get(2)?,
        mime_type: row.get(3)?,
        size_bytes,
        sha256: row.get(5)?,
        purpose: row.get(6)?,
        created_at: row.get(7)?,
        expires_at: row.get(8)?,
    })
}

fn validate_owner(owner: &str) -> Result<(), FileStoreError> {
    if owner.trim().is_empty() {
        Err(FileStoreError::Invalid(
            "File owner identity is missing".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn validate_local_id(id: &str) -> Result<(), FileStoreError> {
    if is_local_file_id(id) {
        Ok(())
    } else {
        Err(FileStoreError::NotFound(id.to_string()))
    }
}

pub fn is_local_file_id(id: &str) -> bool {
    let suffix = id.strip_prefix(LOCAL_FILE_ID_PREFIX);
    suffix.is_some_and(|value| {
        value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

pub fn request_file_owner() -> String {
    crate::libs::request_context::request_api_key_owner_id()
        .unwrap_or_else(|| "unauthenticated-local".to_string())
}

static GLOBAL_FILE_STORE: Lazy<Arc<FileStore>> = Lazy::new(|| {
    Arc::new(FileStore::new(
        PATHS.files_dir.clone(),
        FileStoreLimits::from_env(),
    ))
});

pub fn global_file_store() -> Arc<FileStore> {
    Arc::clone(&GLOBAL_FILE_STORE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store(limits: FileStoreLimits) -> (PathBuf, FileStore) {
        let root = std::env::temp_dir().join(format!("copilot-api-files-{}", Uuid::new_v4()));
        let store = FileStore::new(root.clone(), limits);
        (root, store)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn storage_directories_and_content_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (root, store) = test_store(FileStoreLimits::default());
        let created = store
            .create(
                "alice",
                "private.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from_static(b"private"),
            )
            .await
            .unwrap();
        for path in [&root, &root.join("data")] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(
            std::fs::metadata(store.data_path(&created.id))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn file_lifecycle_is_owner_scoped_and_integrity_checked() {
        let (root, store) = test_store(FileStoreLimits::default());
        let created = store
            .create(
                "alice",
                "notes.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from_static(b"hello"),
            )
            .await
            .unwrap();
        assert!(is_local_file_id(&created.id));
        assert!(matches!(
            store.metadata("bob", &created.id).await,
            Err(FileStoreError::NotFound(_))
        ));
        let loaded = store.read("alice", &created.id).await.unwrap();
        assert_eq!(&loaded.bytes[..], b"hello");

        let page = store
            .list(
                "alice",
                FileListOptions {
                    limit: 20,
                    descending: true,
                    ..FileListOptions::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.data.len(), 1);
        store.delete("alice", &created.id).await.unwrap();
        assert!(matches!(
            store.metadata("alice", &created.id).await,
            Err(FileStoreError::NotFound(_))
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn owner_quota_is_enforced_without_leaving_content() {
        let limits = FileStoreLimits {
            max_file_bytes: 8,
            max_owner_bytes: 5,
            max_owner_files: 10,
            retention_seconds: None,
        };
        let (root, store) = test_store(limits);
        store
            .create(
                "alice",
                "one.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from_static(b"1234"),
            )
            .await
            .unwrap();
        let error = store
            .create(
                "alice",
                "two.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from_static(b"56"),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, FileStoreError::QuotaExceeded { .. }));
        let entries = tokio::fs::read_dir(root.join("data")).await.unwrap();
        drop(entries);
        let page = store
            .list(
                "alice",
                FileListOptions {
                    limit: 20,
                    descending: true,
                    ..FileListOptions::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.data.len(), 1);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn owner_file_count_quota_is_enforced() {
        let limits = FileStoreLimits {
            max_file_bytes: 8,
            max_owner_bytes: 100,
            max_owner_files: 1,
            retention_seconds: None,
        };
        let (root, store) = test_store(limits);
        store
            .create(
                "alice",
                "one.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from_static(b"1"),
            )
            .await
            .unwrap();
        let error = store
            .create(
                "alice",
                "two.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from_static(b"2"),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            FileStoreError::FileCountQuotaExceeded { .. }
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn reconciliation_commits_pending_content_after_interrupted_create() {
        let limits = FileStoreLimits::default();
        let (root, store) = test_store(limits);
        store.ensure_layout().await.unwrap();
        let id = format!("{LOCAL_FILE_ID_PREFIX}{}", Uuid::new_v4().simple());
        let bytes = Bytes::from_static(b"recovered");
        let metadata = StoredFile {
            id: id.clone(),
            owner: "alice".to_string(),
            filename: "notes.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size_bytes: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&bytes)),
            purpose: None,
            created_at: Utc::now().timestamp(),
            expires_at: None,
        };
        store.reserve_metadata(metadata).await.unwrap();
        tokio::fs::write(store.data_path(&id), &bytes)
            .await
            .unwrap();
        drop(store);

        let recovered = FileStore::new(root.clone(), limits);
        let loaded = recovered.read("alice", &id).await.unwrap();
        assert_eq!(&loaded.bytes[..], b"recovered");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn reconciliation_finishes_interrupted_delete() {
        let limits = FileStoreLimits::default();
        let (root, store) = test_store(limits);
        let created = store
            .create(
                "alice",
                "notes.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from_static(b"delete me"),
            )
            .await
            .unwrap();
        store
            .mark_metadata_deleting("alice", &created.id)
            .await
            .unwrap();
        let tombstone = store.data_dir.join(format!(
            ".{}.{}.deleting",
            created.id,
            Uuid::new_v4().simple()
        ));
        tokio::fs::rename(store.data_path(&created.id), &tombstone)
            .await
            .unwrap();
        drop(store);

        let recovered = FileStore::new(root.clone(), limits);
        assert!(matches!(
            recovered.metadata("alice", &created.id).await,
            Err(FileStoreError::NotFound(_))
        ));
        assert!(tokio::fs::metadata(tombstone).await.is_err());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn stale_pending_reservation_is_reclaimed_without_restart() {
        let limits = FileStoreLimits::default();
        let (root, store) = test_store(limits);
        store.ensure_layout().await.unwrap();
        let id = format!("{LOCAL_FILE_ID_PREFIX}{}", Uuid::new_v4().simple());
        store
            .reserve_metadata(StoredFile {
                id: id.clone(),
                owner: "alice".to_string(),
                filename: "notes.txt".to_string(),
                mime_type: "text/plain".to_string(),
                size_bytes: 5,
                sha256: hex::encode(Sha256::digest(b"hello")),
                purpose: None,
                created_at: Utc::now()
                    .timestamp()
                    .saturating_sub(STALE_LIFECYCLE_SECONDS + 1),
                expires_at: None,
            })
            .await
            .unwrap();
        let page = store
            .list(
                "alice",
                FileListOptions {
                    limit: 20,
                    descending: true,
                    ..FileListOptions::default()
                },
            )
            .await
            .unwrap();
        assert!(page.data.is_empty());
        assert!(matches!(
            store.metadata("alice", &id).await,
            Err(FileStoreError::NotFound(_))
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
