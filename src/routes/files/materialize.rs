#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap};
use std::io::Write;

use axum::http::{HeaderMap, StatusCode};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value};

use crate::libs::error::{AppError, HttpError};
use crate::libs::file_store::{
    global_file_store, request_file_owner, FileStore, StoredFileData, LOCAL_FILE_ID_PREFIX,
};
use crate::services::copilot::create_responses::{
    FunctionCallOutputContent, InputField, MessageContent, ResponseInputContent, ResponseInputItem,
    ResponsesPayload,
};

const MAX_MATERIALIZED_ENCODED_BYTES: u64 = crate::libs::http::MAX_REQUEST_BODY_BYTES as u64;

pub async fn materialize_anthropic_file_sources(payload: &mut Value) -> Result<(), AppError> {
    let store = global_file_store();
    let owner = request_file_owner();
    materialize_anthropic_file_sources_with_store(payload, &store, &owner).await
}

pub async fn materialize_responses_file_references(
    payload: &mut ResponsesPayload,
) -> Result<(), AppError> {
    let store = global_file_store();
    let owner = request_file_owner();
    materialize_responses_file_references_with_store(payload, &store, &owner).await
}

async fn materialize_anthropic_file_sources_with_store(
    payload: &mut Value,
    store: &FileStore,
    owner: &str,
) -> Result<(), AppError> {
    let base_bytes = serialized_size(payload)?;
    let mut ids = BTreeMap::new();
    collect_anthropic_local_ids(payload, &mut ids);
    let files = load_files(store, owner, ids, base_bytes).await?;
    rewrite_anthropic_file_sources(payload, &files)?;
    ensure_serialized_limit(payload)
}

async fn materialize_responses_file_references_with_store(
    payload: &mut ResponsesPayload,
    store: &FileStore,
    owner: &str,
) -> Result<(), AppError> {
    let base_bytes = serialized_size(payload)?;
    let mut ids = BTreeMap::new();
    for_each_response_block(payload, |block| match block {
        ResponseInputContent::Image(image) => {
            if let Some(id) = image.file_id.as_deref().filter(|id| is_local_reference(id)) {
                record_reference(&mut ids, id);
            }
        }
        ResponseInputContent::File(file) => {
            if let Some(id) = file.file_id.as_deref().filter(|id| is_local_reference(id)) {
                record_reference(&mut ids, id);
            }
        }
        _ => {}
    });
    let files = load_files(store, owner, ids, base_bytes).await?;
    for_each_response_block_mut(payload, |block| match block {
        ResponseInputContent::Image(image) => {
            let Some(id) = image.file_id.as_deref().filter(|id| is_local_reference(id)) else {
                return Ok(());
            };
            let file = files
                .get(id)
                .ok_or_else(|| AppError::BadRequest(format!("File '{id}' was not loaded")))?;
            require_image(file, id)?;
            image.image_url = Some(data_url(file));
            image.file_id = None;
            Ok(())
        }
        ResponseInputContent::File(input_file) => {
            let Some(id) = input_file
                .file_id
                .as_deref()
                .filter(|id| is_local_reference(id))
            else {
                return Ok(());
            };
            let file = files
                .get(id)
                .ok_or_else(|| AppError::BadRequest(format!("File '{id}' was not loaded")))?;
            input_file.file_data = Some(data_url(file));
            if input_file
                .filename
                .as_deref()
                .is_none_or(|filename| filename.trim().is_empty())
            {
                input_file.filename = Some(file.metadata.filename.clone());
            }
            input_file.file_id = None;
            Ok(())
        }
        _ => Ok(()),
    })?;
    ensure_serialized_limit(payload)
}

async fn load_files(
    store: &FileStore,
    owner: &str,
    ids: BTreeMap<String, usize>,
    base_bytes: u64,
) -> Result<HashMap<String, StoredFileData>, AppError> {
    let mut expanded_bytes = base_bytes;
    for (id, references) in &ids {
        let metadata = store.metadata(owner, id).await.map_err(AppError::from)?;
        let encoded_length = metadata
            .size_bytes
            .saturating_add(2)
            .saturating_div(3)
            .saturating_mul(4)
            .saturating_add(metadata.mime_type.len() as u64)
            .saturating_add(metadata.filename.len() as u64)
            .saturating_add(256);
        expanded_bytes =
            expanded_bytes.saturating_add(encoded_length.saturating_mul(*references as u64));
        if expanded_bytes > MAX_MATERIALIZED_ENCODED_BYTES {
            return Err(AppError::Http(HttpError::new(
                format!(
                    "Expanded file references exceed the {}-byte request limit",
                    MAX_MATERIALIZED_ENCODED_BYTES
                ),
                StatusCode::PAYLOAD_TOO_LARGE,
                HeaderMap::new(),
                String::new(),
            )));
        }
    }
    let mut files = HashMap::with_capacity(ids.len());
    for id in ids.into_keys() {
        let file = store.read(owner, &id).await.map_err(AppError::from)?;
        files.insert(id, file);
    }
    Ok(files)
}

fn serialized_size(value: &impl Serialize) -> Result<u64, AppError> {
    #[derive(Default)]
    struct Counter(u64);

    impl Write for Counter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buffer.len() as u64);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter::default();
    serde_json::to_writer(&mut counter, value).map_err(|error| {
        AppError::Other(anyhow::anyhow!("Could not size file request: {error}"))
    })?;
    Ok(counter.0)
}

fn ensure_serialized_limit(value: &impl Serialize) -> Result<(), AppError> {
    let bytes = serialized_size(value)?;
    if bytes <= MAX_MATERIALIZED_ENCODED_BYTES {
        return Ok(());
    }
    Err(AppError::Http(HttpError::new(
        format!(
            "Expanded file references exceed the {}-byte request limit",
            MAX_MATERIALIZED_ENCODED_BYTES
        ),
        StatusCode::PAYLOAD_TOO_LARGE,
        HeaderMap::new(),
        String::new(),
    )))
}

fn collect_anthropic_local_ids(payload: &Value, ids: &mut BTreeMap<String, usize>) {
    let Some(messages) = payload.get("messages").and_then(Value::as_array) else {
        return;
    };
    for message in messages {
        if let Some(blocks) = message.get("content").and_then(Value::as_array) {
            for block in blocks {
                collect_anthropic_block_ids(block, ids);
            }
        }
    }
}

fn collect_anthropic_block_ids(block: &Value, ids: &mut BTreeMap<String, usize>) {
    let Some(object) = block.as_object() else {
        return;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("image" | "document") => {
            if let Some(id) = object
                .get("source")
                .and_then(Value::as_object)
                .filter(|source| source.get("type").and_then(Value::as_str) == Some("file"))
                .and_then(|source| source.get("file_id"))
                .and_then(Value::as_str)
                .filter(|id| is_local_reference(id))
            {
                record_reference(ids, id);
            }
        }
        Some("tool_result") => {
            if let Some(blocks) = object.get("content").and_then(Value::as_array) {
                for nested in blocks {
                    collect_anthropic_block_ids(nested, ids);
                }
            }
        }
        _ => {}
    }
}

fn rewrite_anthropic_file_sources(
    payload: &mut Value,
    files: &HashMap<String, StoredFileData>,
) -> Result<(), AppError> {
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for message in messages {
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks {
                rewrite_anthropic_block(block, files)?;
            }
        }
    }
    Ok(())
}

fn rewrite_anthropic_block(
    block: &mut Value,
    files: &HashMap<String, StoredFileData>,
) -> Result<(), AppError> {
    let Some(object) = block.as_object_mut() else {
        return Ok(());
    };
    let block_type = object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string);
    if matches!(block_type.as_deref(), Some("image" | "document")) {
        let local_id = object
            .get("source")
            .and_then(Value::as_object)
            .filter(|source| source.get("type").and_then(Value::as_str) == Some("file"))
            .and_then(|source| source.get("file_id"))
            .and_then(Value::as_str)
            .filter(|id| is_local_reference(id))
            .map(str::to_string);
        if let Some(id) = local_id {
            let file = files
                .get(&id)
                .ok_or_else(|| AppError::BadRequest(format!("File '{id}' was not loaded")))?;
            let source = match block_type.as_deref() {
                Some("image") => {
                    require_image(file, &id)?;
                    json!({
                        "type": "base64",
                        "media_type": file.metadata.mime_type,
                        "data": STANDARD.encode(&file.bytes),
                    })
                }
                Some("document") => {
                    require_document(file, &id)?;
                    if object
                        .get("title")
                        .and_then(Value::as_str)
                        .is_none_or(|title| title.trim().is_empty())
                    {
                        object.insert(
                            "title".to_string(),
                            Value::String(file.metadata.filename.clone()),
                        );
                    }
                    if file.metadata.mime_type == "text/plain" {
                        let text = std::str::from_utf8(&file.bytes).map_err(|_| {
                            AppError::BadRequest(format!(
                                "File '{id}' is labeled text/plain but is not valid UTF-8"
                            ))
                        })?;
                        json!({
                            "type": "text",
                            "media_type": "text/plain",
                            "data": text,
                        })
                    } else {
                        json!({
                            "type": "base64",
                            "media_type": file.metadata.mime_type,
                            "data": STANDARD.encode(&file.bytes),
                        })
                    }
                }
                _ => return Ok(()),
            };
            object.insert("source".to_string(), source);
        }
    } else if block_type.as_deref() == Some("tool_result") {
        if let Some(blocks) = object.get_mut("content").and_then(Value::as_array_mut) {
            for nested in blocks {
                rewrite_anthropic_block(nested, files)?;
            }
        }
    }
    Ok(())
}

fn record_reference(ids: &mut BTreeMap<String, usize>, id: &str) {
    let count = ids.entry(id.to_string()).or_default();
    *count = count.saturating_add(1);
}

fn is_local_reference(id: &str) -> bool {
    id.starts_with(LOCAL_FILE_ID_PREFIX)
}

fn for_each_response_block(
    payload: &ResponsesPayload,
    mut visit: impl FnMut(&ResponseInputContent),
) {
    let Some(InputField::Items(items)) = payload.input.as_ref() else {
        return;
    };
    for item in items {
        match item {
            ResponseInputItem::Message(message) => {
                if let Some(MessageContent::Blocks(blocks)) = message.content.as_ref() {
                    for block in blocks {
                        visit(block);
                    }
                }
            }
            ResponseInputItem::FunctionCallOutput(output) => {
                if let FunctionCallOutputContent::Blocks(blocks) = &output.output {
                    for block in blocks {
                        visit(block);
                    }
                }
            }
            _ => {}
        }
    }
}

fn for_each_response_block_mut(
    payload: &mut ResponsesPayload,
    mut visit: impl FnMut(&mut ResponseInputContent) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let Some(InputField::Items(items)) = payload.input.as_mut() else {
        return Ok(());
    };
    for item in items {
        let blocks = match item {
            ResponseInputItem::Message(message) => match message.content.as_mut() {
                Some(MessageContent::Blocks(blocks)) => Some(blocks),
                _ => None,
            },
            ResponseInputItem::FunctionCallOutput(output) => match &mut output.output {
                FunctionCallOutputContent::Blocks(blocks) => Some(blocks),
                _ => None,
            },
            _ => None,
        };
        if let Some(blocks) = blocks {
            for block in blocks {
                visit(block)?;
            }
        }
    }
    Ok(())
}

fn data_url(file: &StoredFileData) -> String {
    format!(
        "data:{};base64,{}",
        file.metadata.mime_type,
        STANDARD.encode(&file.bytes)
    )
}

fn require_image(file: &StoredFileData, id: &str) -> Result<(), AppError> {
    if matches!(
        file.metadata.mime_type.as_str(),
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "File '{id}' has MIME type '{}' and cannot be used as an image",
            file.metadata.mime_type
        )))
    }
}

fn require_document(file: &StoredFileData, id: &str) -> Result<(), AppError> {
    if matches!(
        file.metadata.mime_type.as_str(),
        "application/pdf" | "text/plain"
    ) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "File '{id}' has MIME type '{}' and cannot be used as a document",
            file.metadata.mime_type
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use uuid::Uuid;

    fn test_store() -> (std::path::PathBuf, FileStore) {
        let root = std::env::temp_dir().join(format!("copilot-api-materialize-{}", Uuid::new_v4()));
        let store = FileStore::new(
            root.clone(),
            crate::libs::file_store::FileStoreLimits {
                max_file_bytes: 1024,
                max_owner_bytes: 4096,
                max_owner_files: 10,
                retention_seconds: None,
            },
        );
        (root, store)
    }

    #[tokio::test]
    async fn anthropic_local_text_file_source_becomes_text() {
        let (root, store) = test_store();
        let file = store
            .create(
                "alice",
                "notes.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from_static(b"hello"),
            )
            .await
            .unwrap();
        let mut payload = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "document",
                    "source": {"type": "file", "file_id": file.id}
                }]
            }]
        });
        materialize_anthropic_file_sources_with_store(&mut payload, &store, "alice")
            .await
            .unwrap();
        let source = &payload["messages"][0]["content"][0]["source"];
        assert_eq!(source["type"], "text");
        assert_eq!(source["media_type"], "text/plain");
        assert_eq!(source["data"], "hello");
        assert_eq!(payload["messages"][0]["content"][0]["title"], "notes.txt");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn anthropic_materialization_ignores_file_shapes_inside_tool_input() {
        let (root, store) = test_store();
        let file = store
            .create(
                "alice",
                "notes.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from_static(b"secret"),
            )
            .await
            .unwrap();
        let mut payload = json!({
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "call_1",
                    "name": "inspect",
                    "input": {
                        "type": "document",
                        "source": {"type": "file", "file_id": file.id}
                    }
                }]
            }]
        });
        let original = payload.clone();
        materialize_anthropic_file_sources_with_store(&mut payload, &store, "alice")
            .await
            .unwrap();
        assert_eq!(payload, original);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn aggregate_expansion_limit_is_checked_before_content_reads() {
        let (root, store) = test_store();
        let file = store
            .create(
                "alice",
                "notes.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from(vec![b'x'; 1_024]),
            )
            .await
            .unwrap();
        let error = load_files(&store, "alice", BTreeMap::from([(file.id, 100_000)]), 0)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::Http(HttpError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                ..
            })
        ));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn responses_local_file_id_becomes_inline_data() {
        let (root, store) = test_store();
        let file = store
            .create(
                "alice",
                "notes.txt".to_string(),
                "text/plain".to_string(),
                None,
                Bytes::from_static(b"hello"),
            )
            .await
            .unwrap();
        let mut payload: ResponsesPayload = serde_json::from_value(json!({
            "model": "gpt-test",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_file",
                    "file_id": file.id
                }]
            }]
        }))
        .unwrap();
        materialize_responses_file_references_with_store(&mut payload, &store, "alice")
            .await
            .unwrap();
        let value = serde_json::to_value(payload).unwrap();
        let input_file = &value["input"][0]["content"][0];
        assert!(input_file.get("file_id").is_none());
        assert_eq!(input_file["filename"], "notes.txt");
        assert_eq!(
            input_file["file_data"],
            format!("data:text/plain;base64,{}", STANDARD.encode(b"hello"))
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
