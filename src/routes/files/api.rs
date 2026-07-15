#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::multipart::{Field, MultipartRejection};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, SecondsFormat, Utc};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::libs::error::{AppError, HttpError};
use crate::libs::file_store::{
    global_file_store, request_file_owner, FileListOptions, FileStore, StoredFile,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilesProtocol {
    Anthropic,
    OpenAi,
}

impl FilesProtocol {
    fn from_headers(headers: &HeaderMap) -> Self {
        if crate::libs::error::is_anthropic_files_request(headers) {
            Self::Anthropic
        } else {
            Self::OpenAi
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct FilesListQuery {
    limit: Option<usize>,
    after_id: Option<String>,
    before_id: Option<String>,
    after: Option<String>,
    purpose: Option<String>,
    order: Option<String>,
}

struct UploadPart {
    filename: String,
    mime_type: String,
    bytes: Bytes,
}

static FILE_UPLOAD_SLOTS: Lazy<Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static FILE_DOWNLOAD_SLOTS: Lazy<Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
const FILE_UPLOAD_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const FILE_UPLOAD_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

pub fn router() -> Router {
    router_with_store(global_file_store())
}

fn router_with_store(store: Arc<FileStore>) -> Router {
    Router::new()
        .route("/files", get(list_files).post(upload_file))
        .route("/v1/files", get(list_files).post(upload_file))
        .route("/files/:id", get(get_file).delete(delete_file))
        .route("/v1/files/:id", get(get_file).delete(delete_file))
        .route("/files/:id/content", get(get_file_content))
        .route("/v1/files/:id/content", get(get_file_content))
        .with_state(store)
}

async fn upload_file(
    State(store): State<Arc<FileStore>>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let protocol = FilesProtocol::from_headers(&headers);
    let owner = request_file_owner();
    let _upload_permit = match owner_semaphore(&FILE_UPLOAD_SLOTS, &owner, 4).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return capacity_error(protocol, "Too many concurrent file uploads"),
    };
    let upload_deadline = tokio::time::Instant::now() + FILE_UPLOAD_TOTAL_TIMEOUT;
    let mut multipart = match multipart {
        Ok(multipart) => multipart,
        Err(error) => {
            return render_error(
                protocol,
                multipart_error(error.status(), format!("Invalid multipart upload: {error}")),
            )
        }
    };

    let mut upload: Option<UploadPart> = None;
    let mut purpose: Option<String> = None;
    loop {
        let field = match tokio::time::timeout(
            upload_wait_timeout(upload_deadline),
            multipart.next_field(),
        )
        .await
        {
            Ok(Ok(Some(field))) => field,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                return render_error(
                    protocol,
                    multipart_error(error.status(), format!("Invalid multipart upload: {error}")),
                )
            }
            Err(_) => {
                return render_error(
                    protocol,
                    multipart_error(
                        StatusCode::REQUEST_TIMEOUT,
                        "Timed out waiting for multipart upload data".to_string(),
                    ),
                )
            }
        };
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                if upload.is_some() {
                    return render_error(
                        protocol,
                        AppError::BadRequest(
                            "Exactly one multipart field named 'file' is required".to_string(),
                        ),
                    );
                }
                let filename = sanitize_filename(field.file_name());
                let mime_type = normalize_mime_type(field.content_type(), &filename);
                let bytes = match read_field_limited(field, store.max_file_bytes(), upload_deadline)
                    .await
                {
                    Ok(bytes) => bytes,
                    Err((status, message)) => {
                        return render_error(protocol, multipart_error(status, message))
                    }
                };
                upload = Some(UploadPart {
                    filename,
                    mime_type,
                    bytes,
                });
            }
            "purpose" => {
                let bytes = match read_field_limited(field, 1_024, upload_deadline).await {
                    Ok(bytes) => bytes,
                    Err((status, message)) => {
                        return render_error(protocol, multipart_error(status, message))
                    }
                };
                let value = match std::str::from_utf8(&bytes) {
                    Ok(value) => value.trim().to_string(),
                    Err(_) => {
                        return render_error(
                            protocol,
                            AppError::BadRequest("purpose must be valid UTF-8".to_string()),
                        )
                    }
                };
                if value.is_empty() || value.len() > 64 {
                    return render_error(
                        protocol,
                        AppError::BadRequest(
                            "purpose must be a non-empty string of at most 64 bytes".to_string(),
                        ),
                    );
                }
                purpose = Some(value);
            }
            "" => {
                return render_error(
                    protocol,
                    AppError::BadRequest("Every multipart field must have a name".to_string()),
                )
            }
            other => {
                return render_error(
                    protocol,
                    AppError::BadRequest(format!("Unsupported multipart field '{other}'")),
                )
            }
        }
    }

    if protocol == FilesProtocol::OpenAi && purpose.is_none() {
        return render_error(
            protocol,
            AppError::BadRequest("purpose: field required".to_string()),
        );
    }
    let Some(upload) = upload else {
        return render_error(
            protocol,
            AppError::BadRequest(
                "Exactly one multipart field named 'file' is required".to_string(),
            ),
        );
    };
    match store
        .create(
            &owner,
            upload.filename,
            upload.mime_type,
            purpose,
            upload.bytes,
        )
        .await
    {
        Ok(file) => Json(render_file(&file, protocol)).into_response(),
        Err(error) => render_error(protocol, error.into()),
    }
}

async fn read_field_limited(
    mut field: Field<'_>,
    max_bytes: u64,
    deadline: tokio::time::Instant,
) -> Result<Bytes, (StatusCode, String)> {
    let mut bytes = BytesMut::new();
    loop {
        match tokio::time::timeout(upload_wait_timeout(deadline), field.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                if (bytes.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!("Multipart field exceeds the {max_bytes}-byte limit"),
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(Ok(None)) => return Ok(bytes.freeze()),
            Ok(Err(error)) => {
                return Err((
                    error.status(),
                    format!("Could not read multipart field: {error}"),
                ))
            }
            Err(_) => {
                return Err((
                    StatusCode::REQUEST_TIMEOUT,
                    "Timed out waiting for multipart upload data".to_string(),
                ))
            }
        }
    }
}

fn upload_wait_timeout(deadline: tokio::time::Instant) -> std::time::Duration {
    deadline
        .checked_duration_since(tokio::time::Instant::now())
        .unwrap_or_default()
        .min(FILE_UPLOAD_IDLE_TIMEOUT)
}

fn multipart_error(status: StatusCode, message: String) -> AppError {
    if status == StatusCode::BAD_REQUEST {
        AppError::BadRequest(message)
    } else {
        AppError::Http(HttpError::new(
            message,
            status,
            HeaderMap::new(),
            String::new(),
        ))
    }
}

async fn list_files(
    State(store): State<Arc<FileStore>>,
    headers: HeaderMap,
    query: Result<Query<FilesListQuery>, QueryRejection>,
) -> Response {
    let protocol = FilesProtocol::from_headers(&headers);
    let Query(query) = match query {
        Ok(query) => query,
        Err(error) => {
            return render_error(
                protocol,
                AppError::BadRequest(format!("Invalid files query: {error}")),
            )
        }
    };
    let after = match merge_cursor(query.after_id, query.after, "after_id", "after") {
        Ok(cursor) => cursor,
        Err(error) => return render_error(protocol, error),
    };
    let (default_limit, maximum_limit) = match protocol {
        FilesProtocol::Anthropic => (20, 100),
        FilesProtocol::OpenAi => (1_000, 1_000),
    };
    let limit = query.limit.unwrap_or(default_limit);
    if !(1..=maximum_limit).contains(&limit) {
        return render_error(
            protocol,
            AppError::BadRequest(format!("limit must be between 1 and {maximum_limit}")),
        );
    }
    if query
        .purpose
        .as_deref()
        .is_some_and(|purpose| purpose.trim().is_empty())
    {
        return render_error(
            protocol,
            AppError::BadRequest("purpose must not be empty".to_string()),
        );
    }
    let descending = match query.order.as_deref().unwrap_or("desc") {
        "desc" => true,
        "asc" => false,
        _ => {
            return render_error(
                protocol,
                AppError::BadRequest("order must be 'asc' or 'desc'".to_string()),
            )
        }
    };
    let owner = request_file_owner();
    match store
        .list(
            &owner,
            FileListOptions {
                limit,
                after,
                before: query.before_id,
                purpose: query.purpose,
                descending,
            },
        )
        .await
    {
        Ok(page) => {
            let first_id = page.data.first().map(|file| file.id.clone());
            let last_id = page.data.last().map(|file| file.id.clone());
            let data: Vec<Value> = page
                .data
                .iter()
                .map(|file| render_file(file, protocol))
                .collect();
            let value = match protocol {
                FilesProtocol::Anthropic => json!({
                    "data": data,
                    "has_more": page.has_more,
                    "first_id": first_id,
                    "last_id": last_id,
                }),
                FilesProtocol::OpenAi => json!({
                    "object": "list",
                    "data": data,
                    "has_more": page.has_more,
                    "first_id": first_id,
                    "last_id": last_id,
                }),
            };
            Json(value).into_response()
        }
        Err(error) => render_error(protocol, error.into()),
    }
}

async fn get_file(
    State(store): State<Arc<FileStore>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let protocol = FilesProtocol::from_headers(&headers);
    let owner = request_file_owner();
    match store.metadata(&owner, &id).await {
        Ok(file) => Json(render_file(&file, protocol)).into_response(),
        Err(error) => render_error(protocol, error.into()),
    }
}

async fn get_file_content(
    State(store): State<Arc<FileStore>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let protocol = FilesProtocol::from_headers(&headers);
    let owner = request_file_owner();
    let download_permit = match owner_semaphore(&FILE_DOWNLOAD_SLOTS, &owner, 8).try_acquire_owned()
    {
        Ok(permit) => permit,
        Err(_) => return capacity_error(protocol, "Too many concurrent file downloads"),
    };
    match store.open_content(&owner, &id).await {
        Ok(file) => {
            let content_type = HeaderValue::from_str(&file.metadata.mime_type)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
            let size_bytes = file.metadata.size_bytes;
            let expected_digest = file.metadata.sha256;
            let mut content = file.file;
            let stream = async_stream::stream! {
                let _download_permit = download_permit;
                let mut digest = Sha256::new();
                let mut streamed_bytes = 0_u64;
                let mut buffer = vec![0_u8; 64 * 1024];
                loop {
                    let count = match content.read(&mut buffer).await {
                        Ok(count) => count,
                        Err(error) => {
                            yield Err::<Bytes, std::io::Error>(error);
                            return;
                        }
                    };
                    if count == 0 {
                        break;
                    }
                    digest.update(&buffer[..count]);
                    streamed_bytes = streamed_bytes.saturating_add(count as u64);
                    yield Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(&buffer[..count]));
                }
                if streamed_bytes != size_bytes || hex::encode(digest.finalize()) != expected_digest {
                    yield Err::<Bytes, std::io::Error>(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "stored file integrity check failed",
                    ));
                }
            };
            let mut response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, size_bytes.to_string())
                .body(Body::from_stream(stream))
                .expect("static file response");
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-store"),
            );
            response
        }
        Err(error) => render_error(protocol, error.into()),
    }
}

fn owner_semaphore(
    semaphores: &Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    owner: &str,
    permits: usize,
) -> Arc<tokio::sync::Semaphore> {
    let mut semaphores = semaphores
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(
        semaphores
            .entry(owner.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(permits))),
    )
}

fn capacity_error(protocol: FilesProtocol, message: &'static str) -> Response {
    let mut error_headers = HeaderMap::new();
    error_headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    render_error(
        protocol,
        AppError::Http(HttpError::new(
            message,
            StatusCode::SERVICE_UNAVAILABLE,
            error_headers,
            String::new(),
        )),
    )
}

async fn delete_file(
    State(store): State<Arc<FileStore>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let protocol = FilesProtocol::from_headers(&headers);
    let owner = request_file_owner();
    match store.delete(&owner, &id).await {
        Ok(file) => {
            let value = match protocol {
                FilesProtocol::Anthropic => json!({
                    "id": file.id,
                    "type": "file_deleted",
                }),
                FilesProtocol::OpenAi => json!({
                    "id": file.id,
                    "object": "file",
                    "deleted": true,
                }),
            };
            Json(value).into_response()
        }
        Err(error) => render_error(protocol, error.into()),
    }
}

fn merge_cursor(
    primary: Option<String>,
    alias: Option<String>,
    primary_name: &str,
    alias_name: &str,
) -> Result<Option<String>, AppError> {
    match (primary, alias) {
        (Some(_), Some(_)) => Err(AppError::BadRequest(format!(
            "{primary_name} and {alias_name} cannot be used together"
        ))),
        (primary, alias) => Ok(primary.or(alias)),
    }
}

fn render_file(file: &StoredFile, protocol: FilesProtocol) -> Value {
    match protocol {
        FilesProtocol::Anthropic => json!({
            "id": file.id,
            "type": "file",
            "filename": file.filename,
            "mime_type": file.mime_type,
            "size_bytes": file.size_bytes,
            "created_at": timestamp_rfc3339(file.created_at),
            "downloadable": true,
        }),
        FilesProtocol::OpenAi => {
            let mut value = json!({
                "id": file.id,
                "object": "file",
                "bytes": file.size_bytes,
                "created_at": file.created_at,
                "filename": file.filename,
                "purpose": file.purpose.as_deref().unwrap_or("user_data"),
                "status": "processed",
                "status_details": Value::Null,
            });
            if let Some(expires_at) = file.expires_at {
                value["expires_at"] = json!(expires_at);
            }
            value
        }
    }
}

fn timestamp_rfc3339(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn sanitize_filename(filename: Option<&str>) -> String {
    let leaf = filename
        .unwrap_or("upload.bin")
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("upload.bin")
        .trim();
    let mut clean: String = leaf
        .chars()
        .filter(|character| !character.is_control())
        .take(255)
        .collect();
    if clean.is_empty() || clean == "." || clean == ".." {
        clean = "upload.bin".to_string();
    }
    clean
}

fn normalize_mime_type(content_type: Option<&str>, filename: &str) -> String {
    let explicit = content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    if explicit
        .as_deref()
        .is_some_and(|value| value != "application/octet-stream")
    {
        return explicit.unwrap_or_default();
    }
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("pdf") => "application/pdf",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some(
            "txt" | "md" | "markdown" | "csv" | "json" | "jsonl" | "xml" | "html" | "htm" | "js"
            | "ts" | "py" | "rs" | "go" | "java" | "c" | "h" | "cpp" | "hpp",
        ) => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn render_error(protocol: FilesProtocol, error: AppError) -> Response {
    match protocol {
        FilesProtocol::Anthropic => error.into_response(),
        FilesProtocol::OpenAi => error.into_openai_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use uuid::Uuid;

    fn test_store() -> (std::path::PathBuf, Arc<FileStore>) {
        let root = std::env::temp_dir().join(format!("copilot-api-files-api-{}", Uuid::new_v4()));
        let store = Arc::new(FileStore::new(
            root.clone(),
            crate::libs::file_store::FileStoreLimits {
                max_file_bytes: 1024,
                max_owner_bytes: 4096,
                max_owner_files: 10,
                retention_seconds: None,
            },
        ));
        (root, store)
    }

    async fn json_response(response: Response) -> Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect response")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("JSON response")
    }

    #[tokio::test]
    async fn anthropic_upload_list_content_and_delete_round_trip() {
        let (root, store) = test_store();
        let app = router_with_store(store);
        let boundary = "files-boundary";
        let upload_body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\n\
             Content-Type: text/plain\r\n\r\n\
             hello files\r\n\
             --{boundary}--\r\n"
        );
        let upload = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/files")
                    .header("anthropic-version", "2023-06-01")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(upload_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(upload.status(), StatusCode::OK);
        let uploaded = json_response(upload).await;
        assert_eq!(uploaded["type"], "file");
        assert_eq!(uploaded["filename"], "notes.txt");
        let id = uploaded["id"].as_str().unwrap();

        let listed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/files")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_response(listed).await["data"][0]["id"], id);

        let content = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/files/{id}/content"))
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let content = content.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&content[..], b"hello files");

        let deleted = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/files/{id}"))
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_response(deleted).await["type"], "file_deleted");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn openai_upload_requires_purpose_and_uses_openai_metadata() {
        let (root, store) = test_store();
        let app = router_with_store(store);
        let boundary = "openai-files-boundary";
        let upload_body = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"purpose\"\r\n\r\n\
             user_data\r\n\
             --{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\n\
             Content-Type: text/plain\r\n\r\n\
             hello\r\n\
             --{boundary}--\r\n"
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/files")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(upload_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_response(response).await;
        assert_eq!(body["object"], "file");
        assert_eq!(body["purpose"], "user_data");
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
