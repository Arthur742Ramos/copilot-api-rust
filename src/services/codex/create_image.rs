//! Codex-backed image generation.
//!
//! GitHub Copilot has no image-generation endpoint, but the Codex Responses
//! backend (the ChatGPT "Sign in with ChatGPT" OAuth transport this crate
//! already speaks) exposes a native `image_generation` tool. This module builds
//! a Responses payload that invokes that tool, forwards it over the existing
//! [`forward_codex_responses`] transport, buffers the streamed result, and
//! extracts the base64 PNG(s).
//!
//! The request MUST stream (`stream: true`) — the Codex backend only returns the
//! image over SSE — but the result is delivered to the client as a single
//! non-streaming OpenAI `images.generate` response, so we buffer the whole
//! stream before responding.

use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::libs::config::{get_image_chat_model, get_image_model};
use crate::libs::error::{AppError, HttpError};
use crate::libs::sse::Decoder;
use crate::services::codex::create_responses::forward_codex_responses;
use crate::services::copilot::create_responses::{
    InputField, MessageContent, ResponseInputItem, ResponseInputMessage, ResponsesPayload,
};

/// A parsed `/v1/images/generations` request. Only `prompt` is required; every
/// other field defaults server-side so a minimal `images.generate(model, prompt)`
/// SDK call works unchanged.
#[derive(Debug, Clone)]
pub struct ImageGenerationRequest {
    pub prompt: String,
    pub n: i64,
    /// `auto` | `1024x1024` | `1024x1536` | `1536x1024` (passed to the tool).
    pub size: String,
    /// `auto` | `low` | `medium` | `high`.
    pub quality: String,
    /// `png` | `jpeg` | `webp`.
    pub output_format: String,
    /// `auto` | `opaque` | `transparent`.
    pub background: String,
}

impl ImageGenerationRequest {
    /// Parse the OpenAI-shaped request body. Returns a 400-style error only when
    /// `prompt` is missing/empty; all other fields fall back to defaults.
    #[allow(clippy::result_large_err)]
    pub fn from_value(body: &Value) -> Result<Self, AppError> {
        let prompt = body
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AppError::BadRequest(
                    "prompt: field required and must be a non-empty string".to_string(),
                )
            })?
            .to_string();

        let str_or = |key: &str, default: &str| {
            body.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(default)
                .to_string()
        };
        // `n` clamped to [1, 10] so a bad value can't fan out unboundedly.
        let n = body
            .get("n")
            .and_then(Value::as_i64)
            .unwrap_or(1)
            .clamp(1, 10);

        Ok(Self {
            prompt,
            n,
            size: str_or("size", "auto"),
            quality: str_or("quality", "auto"),
            output_format: str_or("output_format", "png"),
            background: str_or("background", "auto"),
        })
    }
}

/// Build the Responses payload that invokes the `image_generation` tool for ONE
/// image. The top-level model is a chat model (`gpt-5.x`); the image model is
/// requested inside the tool. `tool_choice` is forced so the backend always
/// produces an image rather than answering with text.
///
/// The Codex `image_generation` tool produces exactly one image per call and
/// rejects a tool-level `n` with a 400, so `n > 1` is handled by the caller
/// looping this single-image call, not by a batch parameter here.
fn build_image_payload(req: &ImageGenerationRequest) -> ResponsesPayload {
    let tool = json!({
        "type": "image_generation",
        "model": get_image_model(),
        "output_format": req.output_format,
        "size": req.size,
        "quality": req.quality,
        "background": req.background,
    });

    let input = InputField::Items(vec![ResponseInputItem::Message(ResponseInputMessage {
        item_type: Some("message".to_string()),
        role: "user".to_string(),
        content: Some(MessageContent::Text(req.prompt.clone())),
        status: None,
        phase: None,
        extra: Default::default(),
    })]);

    ResponsesPayload {
        model: get_image_chat_model(),
        instructions: Some(String::new()),
        input: Some(input),
        tools: Some(vec![tool]),
        tool_choice: Some(json!({ "type": "image_generation" })),
        parallel_tool_calls: Some(false),
        stream: Some(true),
        store: Some(false),
        ..default_responses_payload()
    }
}

/// A `ResponsesPayload` with every optional field cleared, so `build_image_payload`
/// only sets what it needs. (`ResponsesPayload` has no `Default`, so spell it out.)
fn default_responses_payload() -> ResponsesPayload {
    ResponsesPayload {
        model: String::new(),
        instructions: None,
        input: None,
        tools: None,
        tool_choice: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        metadata: None,
        stream: None,
        safety_identifier: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        parallel_tool_calls: None,
        store: None,
        reasoning: None,
        context_management: None,
        include: None,
        service_tier: None,
        extra: serde_json::Map::new(),
    }
}

/// The outcome of an image generation: the base64 image(s) in order, plus the
/// upstream `usage` object from `response.completed` (when present) so the
/// caller can both record token usage and surface it in the OpenAI response.
#[derive(Debug, Clone)]
pub struct ImageResult {
    pub images: Vec<String>,
    pub usage: Option<Value>,
}

/// Generate `req.n` images, returning the base64-encoded bytes in order plus
/// any upstream usage. `request_headers` is forwarded to the Codex transport (it
/// strips and re-auths them); pass the inbound request's headers. `base_url` is
/// the resolved Codex provider base URL (empty string for the default ChatGPT
/// backend), threaded so an operator-configured custom Codex `baseUrl` (and its
/// SSRF validation) is honored exactly as on the other Codex routes.
///
/// The Codex `image_generation` tool produces one image per call (it rejects a
/// tool-level `n` with a 400), so `n > 1` is fulfilled by calling it `n` times
/// sequentially and concatenating the results. Usage from each call is summed.
pub async fn create_codex_image(
    req: &ImageGenerationRequest,
    request_headers: &axum::http::HeaderMap,
    base_url: &str,
) -> Result<ImageResult, AppError> {
    let mut images: Vec<String> = Vec::new();
    let mut usage: Option<Value> = None;

    for _ in 0..req.n.max(1) {
        let one = generate_one_image(req, request_headers, base_url).await?;
        images.extend(one.images);
        usage = sum_usage(usage, one.usage);
    }

    Ok(ImageResult { images, usage })
}

/// Sum two upstream usage objects so looping per-image image generation reports
/// the aggregate spend. Returns the non-None side when only one is present.
///
/// Sums the top-level token fields plus the nested
/// `input_tokens_details.cached_tokens` that `normalize_responses_usage` reads,
/// so cached-token accounting isn't lost when `n > 1`.
fn sum_usage(acc: Option<Value>, next: Option<Value>) -> Option<Value> {
    match (acc, next) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => {
            let get = |v: &Value, k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
            let nested = |v: &Value, parent: &str, k: &str| {
                v.get(parent)
                    .and_then(|p| p.get(k))
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
            };
            let cached = nested(&a, "input_tokens_details", "cached_tokens")
                + nested(&b, "input_tokens_details", "cached_tokens");
            let mut out = json!({
                "input_tokens": get(&a, "input_tokens") + get(&b, "input_tokens"),
                "output_tokens": get(&a, "output_tokens") + get(&b, "output_tokens"),
                "total_tokens": get(&a, "total_tokens") + get(&b, "total_tokens"),
            });
            // Only emit input_tokens_details when at least one side reported it,
            // so we don't fabricate a zero where the upstream sent nothing.
            if a.get("input_tokens_details").is_some() || b.get("input_tokens_details").is_some() {
                if let Some(obj) = out.as_object_mut() {
                    obj.insert(
                        "input_tokens_details".to_string(),
                        json!({ "cached_tokens": cached }),
                    );
                }
            }
            Some(out)
        }
    }
}

/// Generate exactly one image via a single Codex `image_generation` tool call.
async fn generate_one_image(
    req: &ImageGenerationRequest,
    request_headers: &axum::http::HeaderMap,
    base_url: &str,
) -> Result<ImageResult, AppError> {
    let payload = build_image_payload(req);
    let response = forward_codex_responses(payload, request_headers, base_url).await?;

    if !response.status().is_success() {
        return Err(AppError::Http(
            crate::libs::error::http_error_from_response("Codex image generation failed", response)
                .await,
        ));
    }

    // Buffer the whole SSE stream (the image only exists at `response.completed`),
    // bounding memory against a misbehaving upstream.
    let mut buf: Vec<u8> = Vec::new();
    let mut byte_stream = response.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|e| {
            AppError::Http(HttpError::internal(format!(
                "Error reading codex image stream: {e}"
            )))
        })?;
        if buf.len() + chunk.len() > crate::libs::http::MAX_UPSTREAM_RESPONSE_BYTES {
            return Err(AppError::Http(HttpError::internal(
                "Codex image response exceeded the upstream size cap",
            )));
        }
        buf.extend_from_slice(&chunk);
    }

    parse_image_results(&buf).map_err(|e| AppError::Http(HttpError::internal(e)))
}

/// Extract base64 image(s) and the upstream usage from the buffered Codex
/// Responses SSE stream.
///
/// Image tiers, in priority order:
/// 1. `response.completed` -> `response.output[]` -> every `image_generation_call`
///    item's `.result` (the authoritative final set, preserving order for n>1).
/// 2. fallback: each `response.output_item.done` whose `item.type` is
///    `image_generation_call` -> `item.result`.
/// 3. last resort: the highest-index `partial_image_b64` (a progressive preview).
///
/// Usage is taken from the `response.completed` event's `/response/usage`.
///
/// An error event (`type == "error"` or `response.failed`) short-circuits with
/// its message so refusals/quota failures surface their real reason.
fn parse_image_results(sse_bytes: &[u8]) -> Result<ImageResult, String> {
    let mut decoder = Decoder::new();
    let mut events = decoder.push(sse_bytes);
    if let Some(ev) = decoder.finish() {
        events.push(ev);
    }

    let mut completed: Vec<String> = Vec::new();
    let mut done_items: Vec<String> = Vec::new();
    let mut best_partial: Option<(i64, String)> = None;
    let mut usage: Option<Value> = None;

    for ev in &events {
        if ev.data == "[DONE]" {
            continue;
        }
        let v: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let event_type = v.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            // Surface an upstream error/refusal with its real message.
            "error" => {
                let msg = v
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| v.pointer("/error/message").and_then(Value::as_str))
                    .unwrap_or("Codex image generation returned an error");
                return Err(msg.to_string());
            }
            "response.failed" => {
                let msg = v
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex image generation failed");
                return Err(msg.to_string());
            }
            // Tier 1: authoritative final output set (also carries usage).
            "response.completed" => {
                if let Some(out) = v.pointer("/response/output").and_then(Value::as_array) {
                    for item in out {
                        if item.get("type").and_then(Value::as_str) == Some("image_generation_call")
                        {
                            if let Some(b64) = item.get("result").and_then(Value::as_str) {
                                completed.push(b64.to_string());
                            }
                        }
                    }
                }
                if let Some(u) = v.pointer("/response/usage") {
                    if u.is_object() {
                        usage = Some(u.clone());
                    }
                }
            }
            // Tier 2: per-item completion.
            "response.output_item.done" => {
                if let Some(item) = v.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("image_generation_call") {
                        if let Some(b64) = item.get("result").and_then(Value::as_str) {
                            done_items.push(b64.to_string());
                        }
                    }
                }
            }
            // Tier 3: progressive preview (lowest priority).
            "response.image_generation_call.partial_image" => {
                if let Some(b64) = v.get("partial_image_b64").and_then(Value::as_str) {
                    let idx = v
                        .get("partial_image_index")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    if best_partial
                        .as_ref()
                        .map(|(i, _)| idx >= *i)
                        .unwrap_or(true)
                    {
                        best_partial = Some((idx, b64.to_string()));
                    }
                }
            }
            _ => {}
        }
    }

    let images = if !completed.is_empty() {
        completed
    } else if !done_items.is_empty() {
        done_items
    } else if let Some((_, b64)) = best_partial {
        vec![b64]
    } else {
        return Err("Codex image generation produced no image".to_string());
    };

    Ok(ImageResult { images, usage })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_from_completed() {
        let sse = "\
event: response.image_generation_call.partial_image\n\
data: {\"type\":\"response.image_generation_call.partial_image\",\"partial_image_b64\":\"PARTIAL\",\"partial_image_index\":0}\n\
\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"image_generation_call\",\"result\":\"FINAL_B64\"}],\"usage\":{\"input_tokens\":11,\"output_tokens\":1290,\"total_tokens\":1301}}}\n\
\n\
data: [DONE]\n\
\n";
        let out = parse_image_results(sse.as_bytes()).expect("should extract");
        assert_eq!(out.images, vec!["FINAL_B64"]);
        // Usage from response.completed is captured.
        let usage = out.usage.expect("usage present");
        assert_eq!(
            usage.pointer("/total_tokens").and_then(Value::as_i64),
            Some(1301)
        );
    }

    #[test]
    fn parse_collects_multiple_images_in_order() {
        let sse = "\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"image_generation_call\",\"result\":\"A\"},{\"type\":\"message\"},{\"type\":\"image_generation_call\",\"result\":\"B\"}]}}\n\
\n";
        let out = parse_image_results(sse.as_bytes()).expect("should extract");
        assert_eq!(out.images, vec!["A", "B"]);
        assert!(out.usage.is_none());
    }

    #[test]
    fn parse_falls_back_to_output_item_done() {
        let sse = "\
event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"image_generation_call\",\"result\":\"FROM_ITEM\"}}\n\
\n";
        let out = parse_image_results(sse.as_bytes()).expect("should extract");
        assert_eq!(out.images, vec!["FROM_ITEM"]);
    }

    #[test]
    fn parse_surfaces_error_message() {
        let sse = "\
event: error\n\
data: {\"type\":\"error\",\"message\":\"content policy violation\"}\n\
\n";
        let err = parse_image_results(sse.as_bytes()).expect_err("should error");
        assert!(err.contains("content policy"), "got: {err}");
    }

    #[test]
    fn parse_errors_when_no_image() {
        let sse = "\
event: response.created\n\
data: {\"type\":\"response.created\"}\n\
\n";
        assert!(parse_image_results(sse.as_bytes()).is_err());
    }

    #[test]
    fn request_requires_prompt() {
        assert!(ImageGenerationRequest::from_value(&json!({})).is_err());
        assert!(ImageGenerationRequest::from_value(&json!({ "prompt": "" })).is_err());
        let req = ImageGenerationRequest::from_value(&json!({ "prompt": "a cat" })).unwrap();
        assert_eq!(req.prompt, "a cat");
        assert_eq!(req.n, 1);
        assert_eq!(req.output_format, "png");
    }

    #[test]
    fn request_clamps_n() {
        let req = ImageGenerationRequest::from_value(&json!({ "prompt": "x", "n": 99 })).unwrap();
        assert_eq!(req.n, 10);
        let req = ImageGenerationRequest::from_value(&json!({ "prompt": "x", "n": 0 })).unwrap();
        assert_eq!(req.n, 1);
    }

    #[test]
    fn build_image_payload_enforces_backend_contract() {
        let req = ImageGenerationRequest::from_value(&json!({ "prompt": "a cat" })).unwrap();
        let payload = build_image_payload(&req);
        // The image_generation tool is forced so the backend always emits an
        // image rather than answering with text, and stream/store/parallel are
        // pinned to the values the Codex backend expects for this flow.
        assert_eq!(
            payload.tool_choice,
            Some(json!({ "type": "image_generation" }))
        );
        assert_eq!(payload.parallel_tool_calls, Some(false));
        assert_eq!(payload.store, Some(false));
        assert_eq!(payload.stream, Some(true));
        let tools = payload.tools.expect("tools present");
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].get("type").and_then(Value::as_str),
            Some("image_generation")
        );
        // Default model slugs flow through.
        assert_eq!(payload.model, "gpt-5.5");
        assert_eq!(
            tools[0].get("model").and_then(Value::as_str),
            Some("gpt-image-2")
        );
        // n==1 omits the "n" key from the tool.
        assert!(tools[0].get("n").is_none());
    }

    #[test]
    fn build_image_payload_never_sets_tool_n() {
        // The Codex image_generation tool 400s on a tool-level `n`; n>1 is
        // handled by looping the call, so the payload must never carry `n`.
        let req = ImageGenerationRequest::from_value(&json!({ "prompt": "x", "n": 3 })).unwrap();
        let payload = build_image_payload(&req);
        let tools = payload.tools.unwrap();
        assert!(tools[0].get("n").is_none());
    }

    #[test]
    fn sum_usage_adds_token_fields() {
        let a = json!({
            "input_tokens": 10, "output_tokens": 1290, "total_tokens": 1300,
            "input_tokens_details": { "cached_tokens": 4 }
        });
        let b = json!({
            "input_tokens": 5, "output_tokens": 1290, "total_tokens": 1295,
            "input_tokens_details": { "cached_tokens": 1 }
        });
        let summed = sum_usage(Some(a), Some(b)).unwrap();
        assert_eq!(summed["input_tokens"], 15);
        assert_eq!(summed["output_tokens"], 2580);
        assert_eq!(summed["total_tokens"], 2595);
        // Nested cached-token detail is summed and preserved (not dropped).
        assert_eq!(summed["input_tokens_details"]["cached_tokens"], 5);
        // One-sided cases return the present side.
        assert!(sum_usage(None, None).is_none());
        assert_eq!(
            sum_usage(None, Some(json!({"total_tokens": 7}))).unwrap()["total_tokens"],
            7
        );
        // When neither side reports details, none is fabricated.
        let no_details = sum_usage(
            Some(json!({"input_tokens": 1})),
            Some(json!({"input_tokens": 2})),
        )
        .unwrap();
        assert!(no_details.get("input_tokens_details").is_none());
    }
}
