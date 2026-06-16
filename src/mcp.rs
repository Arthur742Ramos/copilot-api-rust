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

        if let Some(response) = handle_message(&request) {
            write_response(&mut stdout, &response).await?;
        }
    }

    Ok(())
}

/// Handles a single decoded JSON-RPC message. Returns `Some(response)` for
/// requests and `None` for notifications (messages without an `id`).
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
        Some("tools/call") => Some(handle_tools_call(id, request)),
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
        ],
    })
}

fn handle_tools_call(id: Value, request: &Value) -> Value {
    let params = request.get("params");
    let name = params.and_then(|p| p.get("name")).and_then(Value::as_str);

    if name != Some("search") {
        return error(
            id,
            -32602,
            format!("Unknown tool: {}", name.unwrap_or("<missing>")),
        );
    }

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
    fn tools_call_search_returns_sentinel() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "search",
                "arguments": { "names": "foo,bar" },
            },
        });
        let resp = handle_message(&req).expect("tools/call should reply");
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

    #[test]
    fn tools_call_unknown_tool_errors() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "nope", "arguments": {} },
        });
        let resp = handle_message(&req).expect("should reply with error");
        assert_eq!(resp["error"]["code"], -32602);
    }
}
