//! Hand-rolled JSON-RPC 2.0 stdio server exposing a single `search` tool.
//!
//! Mirrors `src/mcp.ts` (the `@modelcontextprotocol/sdk` MCP server). The TS
//! version leans on the SDK's `StdioServerTransport`; here we implement a
//! minimal line-delimited JSON-RPC 2.0 loop by hand so we don't pull in an MCP
//! SDK crate.
//!
//! CRITICAL: in this mode stdout is the JSON-RPC transport. Nothing other than
//! framed JSON-RPC responses may be written there — tracing must go to stderr.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::libs::tool_search::create_mcp_tool_search_sentinel;

const SERVER_NAME: &str = "tool_search";
const SERVER_VERSION: &str = "1.0.0";
const PROTOCOL_VERSION: &str = "2024-11-05";

const SEARCH_TOOL_DESCRIPTION: &str =
    "Load deferred tools by exact name through the Copilot API tool_search bridge.";
const SEARCH_NAMES_DESCRIPTION: &str =
    "Comma-separated exact deferred tool names to load, for example \
     \"TaskList,TaskGet,mcp__fetch__fetch\".";

const GENERATE_IMAGE_DESCRIPTION: &str =
    "Generate an image from a text prompt using the Codex (Sign in with ChatGPT) \
     image_generation backend. Returns the generated image (which the model can \
     then see) and the path of the saved PNG/JPEG/WebP file on disk.";
const GENERATE_IMAGE_PROMPT_DESCRIPTION: &str = "Text description of the image to generate.";

/// Reads line-delimited JSON-RPC 2.0 requests from stdin and writes framed
/// responses to stdout, one JSON object per line, flushing after each.
pub async fn run_mcp_server() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                // JSON-RPC 2.0: reply to a parse error with code -32700 and a
                // null id so clients waiting on a response don't block.
                tracing::warn!("Malformed JSON-RPC line: {err}");
                let response = error(Value::Null, -32700, "Parse error".to_string());
                write_response(&mut stdout, &response).await?;
                continue;
            }
        };

        if let Some(response) = dispatch(&request).await {
            write_response(&mut stdout, &response).await?;
        }
    }

    Ok(())
}

/// Dispatch a decoded JSON-RPC message. `tools/call` is handled on an async path
/// (the image tool calls the upstream Codex backend); every other method is
/// synchronous. Returns `Some(response)` for requests and `None` for
/// notifications (messages without an `id`).
async fn dispatch(request: &Value) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str);
    // Notifications carry no `id` and must not be answered.
    let id = request.get("id").cloned()?;

    if method == Some("tools/call") {
        return Some(handle_tools_call(id, request).await);
    }
    handle_message(request)
}

/// Handles the synchronous JSON-RPC methods. `tools/call` is dispatched
/// separately (see [`dispatch`]) because it is async. Returns `Some(response)`
/// for requests and `None` for notifications (messages without an `id`).
fn handle_message(request: &Value) -> Option<Value> {
    let method = request.get("method").and_then(Value::as_str);

    // Notifications carry no `id` and must not be answered.
    let id = request.get("id").cloned()?;

    match method {
        Some("initialize") => Some(success(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": SERVER_VERSION,
                },
            }),
        )),
        Some("tools/list") => Some(success(id, tools_list_result())),
        Some(other) => Some(error(id, -32601, format!("Method not found: {other}"))),
        None => Some(error(
            id,
            -32600,
            "Invalid request: missing method".to_string(),
        )),
    }
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "search",
                "title": "Tool Search Bridge",
                "description": SEARCH_TOOL_DESCRIPTION,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "names": {
                            "type": "string",
                            "description": SEARCH_NAMES_DESCRIPTION,
                        },
                    },
                    "required": ["names"],
                },
                "_meta": {
                    "anthropic/alwaysLoad": true,
                },
            },
            {
                "name": "generate_image",
                "title": "Generate Image",
                "description": GENERATE_IMAGE_DESCRIPTION,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": GENERATE_IMAGE_PROMPT_DESCRIPTION,
                        },
                        "size": {
                            "type": "string",
                            "description": "auto | 1024x1024 | 1024x1536 | 1536x1024",
                        },
                        "quality": {
                            "type": "string",
                            "description": "auto | low | medium | high",
                        },
                        "output_format": {
                            "type": "string",
                            "description": "png | jpeg | webp",
                        },
                        "background": {
                            "type": "string",
                            "description": "auto | opaque | transparent",
                        },
                    },
                    "required": ["prompt"],
                },
            },
        ],
    })
}

async fn handle_tools_call(id: Value, request: &Value) -> Value {
    let params = request.get("params");
    let name = params.and_then(|p| p.get("name")).and_then(Value::as_str);

    match name {
        Some("search") => handle_search_call(id, params),
        Some("generate_image") => handle_generate_image_call(id, params).await,
        other => error(
            id,
            -32602,
            format!("Unknown tool: {}", other.unwrap_or("<missing>")),
        ),
    }
}

fn handle_search_call(id: Value, params: Option<&Value>) -> Value {
    // The SDK destructures `{ names }` from the call arguments; pass that value
    // straight through to the sentinel builder, which accepts any JSON shape.
    let names = params
        .and_then(|p| p.get("arguments"))
        .and_then(|args| args.get("names"))
        .cloned()
        .unwrap_or(Value::Null);

    let text = create_mcp_tool_search_sentinel(&names);

    success(
        id,
        json!({
            "content": [
                {
                    "type": "text",
                    "text": text,
                },
            ],
        }),
    )
}

/// Handle a `generate_image` tool call: generate the image(s) via the Codex
/// backend and return BOTH the saved file path (text, always reliable) and the
/// inline image content (so the model can see it when the host converts it to a
/// vision block). Errors map to JSON-RPC error envelopes.
async fn handle_generate_image_call(id: Value, params: Option<&Value>) -> Value {
    use crate::libs::provider_resolver::resolve_provider_config;
    use crate::services::codex::create_image::{create_codex_image, ImageGenerationRequest};

    let arguments = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or(Value::Null);

    let req = match ImageGenerationRequest::from_value(&arguments) {
        Ok(req) => req,
        Err(e) => return error(id, -32602, format!("Invalid image request: {e}")),
    };

    // Resolve Codex credentials (loads/refreshes the OAuth token from disk into
    // state). The MCP server has no inbound HTTP headers, so an empty HeaderMap
    // is passed — create_codex_image authenticates from global state, not
    // headers.
    let provider_config = match resolve_provider_config("codex").await {
        Some(cfg) => cfg,
        None => {
            return error(
                id,
                -32603,
                "Image generation requires Codex (Sign in with ChatGPT) credentials. \
                 Run `copilot-api auth --provider codex` first."
                    .to_string(),
            );
        }
    };

    let headers = axum::http::HeaderMap::new();
    let result = match create_codex_image(&req, &headers, &provider_config.base_url).await {
        Ok(result) => result,
        Err(e) => return error(id, -32603, format!("Image generation failed: {e}")),
    };

    let mime = mime_for_format(&req.output_format);
    let ext = ext_for_format(&req.output_format);

    let mut content: Vec<Value> = Vec::new();
    for (idx, b64) in result.images.iter().enumerate() {
        // Save to disk so the user reliably gets a file regardless of whether the
        // host renders/forwards the inline image.
        match save_image(b64, ext, idx) {
            Ok(path) => content.push(json!({
                "type": "text",
                "text": format!("Saved image to {path}"),
            })),
            Err(e) => content.push(json!({
                "type": "text",
                "text": format!("(could not save image {idx} to disk: {e})"),
            })),
        }
        // Inline image content (MCP ImageContent: flat data + mimeType). The host
        // converts this into a vision block so the model can see the result.
        content.push(json!({
            "type": "image",
            "data": b64,
            "mimeType": mime,
        }));
    }

    if content.is_empty() {
        return error(id, -32603, "Image generation produced no image".to_string());
    }

    success(id, json!({ "content": content }))
}

fn mime_for_format(output_format: &str) -> &'static str {
    match output_format {
        "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

fn ext_for_format(output_format: &str) -> &'static str {
    match output_format {
        "jpeg" => "jpg",
        "webp" => "webp",
        _ => "png",
    }
}

/// Decode a base64 image and write it under the app data dir, returning the
/// saved path. Filenames are unique per (pid, index, timestamp) so concurrent
/// generations don't collide.
fn save_image(b64: &str, ext: &str, idx: usize) -> Result<String, String> {
    use base64::Engine;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("base64 decode: {e}"))?;

    let dir = crate::libs::paths::PATHS.app_dir.join("images");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create dir: {e}"))?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("image-{stamp}-{}-{idx}.{ext}", std::process::id()));
    std::fs::write(&path, &bytes).map_err(|e| format!("write file: {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

fn success(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn error(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        },
    })
}

async fn write_response<W>(writer: &mut W, response: &Value) -> anyhow::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let mut line = serde_json::to_string(response)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_returns_server_info() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {},
        });
        let resp = handle_message(&req).expect("initialize should reply");
        assert_eq!(resp["id"], json!(1));
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["result"]["serverInfo"]["name"], "tool_search");
        assert_eq!(resp["result"]["serverInfo"]["version"], "1.0.0");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_get_no_reply() {
        let req = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        });
        assert!(handle_message(&req).is_none());
    }

    #[test]
    fn tools_list_exposes_search_tool() {
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let resp = handle_message(&req).expect("tools/list should reply");
        let tool = &resp["result"]["tools"][0];
        assert_eq!(tool["name"], "search");
        assert_eq!(tool["title"], "Tool Search Bridge");
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["required"][0], "names");
        assert_eq!(tool["_meta"]["anthropic/alwaysLoad"], true);
    }

    #[test]
    fn tools_list_exposes_generate_image_tool() {
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let resp = handle_message(&req).expect("tools/list should reply");
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        let img = tools
            .iter()
            .find(|t| t["name"] == "generate_image")
            .expect("generate_image tool present");
        assert_eq!(img["inputSchema"]["required"][0], "prompt");
        assert_eq!(img["inputSchema"]["properties"]["prompt"]["type"], "string");
    }

    #[tokio::test]
    async fn tools_call_search_returns_sentinel() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "search",
                "arguments": { "names": "foo,bar" },
            },
        });
        let resp = dispatch(&req).await.expect("tools/call should reply");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        assert_eq!(
            text,
            r#"{"type":"copilot_api_tool_search","names":["foo","bar"]}"#
        );
        assert_eq!(resp["result"]["content"][0]["type"], "text");
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let req = json!({ "jsonrpc": "2.0", "id": 4, "method": "does/not/exist" });
        let resp = handle_message(&req).expect("should reply with error");
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_errors() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "nope", "arguments": {} },
        });
        let resp = dispatch(&req).await.expect("should reply with error");
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn generate_image_missing_prompt_errors() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "generate_image", "arguments": {} },
        });
        let resp = dispatch(&req).await.expect("should reply with error");
        // Missing prompt is rejected before any network call.
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn mime_and_ext_mapping() {
        assert_eq!(mime_for_format("png"), "image/png");
        assert_eq!(mime_for_format("jpeg"), "image/jpeg");
        assert_eq!(mime_for_format("webp"), "image/webp");
        assert_eq!(mime_for_format("auto"), "image/png");
        assert_eq!(ext_for_format("jpeg"), "jpg");
        assert_eq!(ext_for_format("webp"), "webp");
        assert_eq!(ext_for_format("png"), "png");
    }
}
