use std::collections::HashSet;

use once_cell::sync::Lazy;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::libs::api_config::{
    copilot_base_url, copilot_headers, prepare_for_compact, prepare_interaction_headers,
    prepare_message_proxy_headers, set_header,
};
use crate::libs::copilot_rate_limit::log_copilot_rate_limits;
use crate::libs::error::{http_error_from_response, HttpError};
use crate::libs::http::{client, serialize_json_body};
use crate::libs::state;
use crate::libs::subagent::SubagentMarker;
use crate::libs::utils::parse_user_id_metadata;
use crate::routes::messages::anthropic_types::{
    AnthropicMessagesPayload, AnthropicResponse, AnthropicThinkingConfig,
};

const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const ADVANCED_TOOL_USE_BETA: &str = "advanced-tool-use-2025-11-20";
const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";
const TASK_BUDGETS_BETA: &str = "task-budgets-2026-03-13";
/// The 1M-context beta. Requested implicitly via the `[1m]` model-id suffix
/// (see `libs::models`); the handler folds it into the `anthropic-beta` header,
/// so it must survive the allowlist filter below.
pub const CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";

// Built once for the process lifetime rather than re-allocated on every request.
static ALLOWED_ANTHROPIC_BETAS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        INTERLEAVED_THINKING_BETA,
        CONTEXT_MANAGEMENT_BETA,
        ADVANCED_TOOL_USE_BETA,
        TASK_BUDGETS_BETA,
        CONTEXT_1M_BETA,
    ])
});

fn allowed_anthropic_betas() -> &'static HashSet<&'static str> {
    &ALLOWED_ANTHROPIC_BETAS
}

/// Options for `create_messages`, mirroring the TS options object.
pub struct CreateMessagesOptions<'a> {
    pub subagent_marker: Option<&'a SubagentMarker>,
    pub request_id: &'a str,
    pub session_id: Option<&'a str>,
    pub compact_type: Option<i32>,
    pub anthropic_version_header: Option<&'a str>,
}

/// The result of a `/v1/messages` call: either a fully-buffered JSON response
/// (non-streaming) or a streaming reqwest response whose SSE body is forwarded.
// The non-streaming variant carries a full buffered `AnthropicResponse`; the
// size gap to the streaming handle is inherent and short-lived (consumed
// immediately by the flow handler), so boxing would only add churn.
#[allow(clippy::large_enum_variant)]
pub enum CreateMessagesResult {
    NonStreaming(AnthropicResponse),
    Streaming(reqwest::Response),
}

/// Mirrors `buildAnthropicBetaHeader` in services/copilot/create-messages.ts.
fn build_anthropic_beta_header(
    anthropic_beta_header: Option<&str>,
    thinking: Option<&AnthropicThinkingConfig>,
    _model: &str,
) -> Option<String> {
    let is_adaptive_thinking = thinking.map(|t| t.kind == "adaptive").unwrap_or(false);

    if let Some(header) = anthropic_beta_header {
        let allowed = allowed_anthropic_betas();
        let filtered: Vec<&str> = header
            .split(',')
            .map(|item| item.trim())
            .filter(|item| !item.is_empty())
            .filter(|item| allowed.contains(*item))
            .collect();

        // in vscode copilot extension, advanced-tool-use is enabled by default
        // align header with vscode copilot extension
        if !filtered.is_empty() {
            return Some(filtered.join(","));
        }

        return None;
    }

    if thinking
        .and_then(|t| t.budget_tokens.as_ref().copied())
        .map(|b| b != 0)
        .unwrap_or(false)
        && !is_adaptive_thinking
    {
        return Some(INTERLEAVED_THINKING_BETA.to_string());
    }

    None
}

/// Whether a content block is, or contains, an image — used to enable vision.
fn block_has_image(block: &Value) -> bool {
    let block_type = block.get("type").and_then(|t| t.as_str());
    if block_type == Some("image") {
        return true;
    }
    if block_type == Some("tool_result") {
        if let Some(inner) = block.get("content").and_then(|c| c.as_array()) {
            return inner
                .iter()
                .any(|i| i.get("type").and_then(|t| t.as_str()) == Some("image"));
        }
    }
    false
}

/// Whether the last user message initiates a turn (any non-tool_result block, or
/// a plain string content). Mirrors the `isInitiateRequest` block in the TS.
fn is_initiate_request(payload: &AnthropicMessagesPayload) -> bool {
    let last = match payload.messages.last() {
        Some(m) => m,
        None => return false,
    };
    if last.role != "user" {
        return false;
    }
    match &last.content {
        Value::Array(blocks) => blocks
            .iter()
            .any(|block| block.get("type").and_then(|t| t.as_str()) != Some("tool_result")),
        // Non-array (string) content initiates a request.
        _ => true,
    }
}

/// Mirrors `createMessages` in services/copilot/create-messages.ts.
pub async fn create_messages(
    payload: &AnthropicMessagesPayload,
    anthropic_beta_header: Option<&str>,
    options: CreateMessagesOptions<'_>,
) -> Result<CreateMessagesResult, HttpError> {
    let st = state::snapshot();
    if st.copilot_token.as_deref().unwrap_or("").is_empty() {
        return Err(HttpError::internal("Copilot token not found"));
    }

    let enable_vision = payload
        .messages
        .iter()
        .any(|message| match &message.content {
            Value::Array(blocks) => blocks.iter().any(block_has_image),
            _ => false,
        });

    let is_initiate = is_initiate_request(payload);

    let user_id = payload.metadata.as_ref().and_then(|m| m.user_id.as_deref());
    let parsed = parse_user_id_metadata(user_id);
    let use_message_proxy = parsed.safety_identifier.is_some()
        && parsed.session_id.is_some()
        && payload.model != "claude-opus-4.8";

    // align with vscode copilot extension anthropic-beta
    let anthropic_beta = build_anthropic_beta_header(
        anthropic_beta_header,
        payload.thinking.as_ref(),
        &payload.model,
    );

    tracing::info!("<-- model: {}", payload.model);

    let base = copilot_base_url(&st);
    let body = serialize_json_body(payload).map_err(|e| HttpError::internal(format!("{e}")))?;
    let upstream_start = std::time::Instant::now();
    // Auth headers are rebuilt per attempt from the token the helper hands us, so
    // the single 401-triggered replay carries the inline-refreshed token and the
    // request provably uses the exact token the refresh decision is made against.
    let build = |token: &str| {
        let mut st = state::snapshot();
        st.copilot_token = Some(token.to_string());
        let mut headers: HeaderMap = copilot_headers(&st, Some(options.request_id), enable_vision);
        set_header(
            &mut headers,
            "x-initiator",
            if is_initiate { "user" } else { "agent" },
        );
        prepare_interaction_headers(
            options.session_id,
            options.subagent_marker.is_some(),
            &mut headers,
        );
        prepare_for_compact(&mut headers, options.compact_type);

        // claude-opus-4.8 is excluded: Copilot's upstream WAF returns a generic
        // "Access to this endpoint is forbidden" 403 whenever a request carries
        // the Claude-Code-style user-agent without a `copilot-integration-id`
        // header. The exact same header set is accepted on claude-opus-4.7, so
        // the gate is a model-id rollout gap on Copilot's side. Skipping the
        // rewrite for 4.8 keeps the default Copilot identity
        // (copilot-integration-id: vscode-chat + GitHubCopilotChat UA +
        // conversation-agent intent) in place; that path is 200. Remove this
        // skip once Copilot's upstream accepts the Claude-Code identity on 4.8.
        // Probed 2026-05-29.
        if use_message_proxy {
            prepare_message_proxy_headers(&mut headers);
        }

        if let Some(beta) = anthropic_beta.as_deref() {
            set_header(&mut headers, "anthropic-beta", beta);
        }
        if let Some(version) = options.anthropic_version_header {
            set_header(&mut headers, "anthropic-version", version);
        }

        client()
            .post(format!("{base}/v1/messages"))
            .headers(headers)
            .body(body.clone())
    };
    let response = crate::libs::token::send_copilot_with_401_retry(
        crate::libs::http::retry_endpoint::MESSAGES,
        build,
    )
    .await
    .map_err(|e| {
        crate::libs::metrics::record_upstream_request(
            "messages",
            crate::libs::metrics::UpstreamStatus::TransportError,
            upstream_start.elapsed().as_secs_f64(),
        );
        HttpError::internal(format!("Failed to create messages: {e}"))
    })?;
    crate::libs::metrics::record_upstream_request(
        "messages",
        crate::libs::metrics::UpstreamStatus::from_code(response.status().as_u16()),
        upstream_start.elapsed().as_secs_f64(),
    );

    log_copilot_rate_limits(response.headers());

    if !response.status().is_success() {
        tracing::error!("Failed to create messages");
        return Err(http_error_from_response("Failed to create messages", response).await);
    }

    if payload.stream.unwrap_or(false) {
        Ok(CreateMessagesResult::Streaming(response))
    } else {
        let json = crate::libs::http::read_json_capped::<AnthropicResponse>(response)
            .await
            .map_err(|e| HttpError::internal(format!("Failed to parse messages: {e}")))?;
        Ok(CreateMessagesResult::NonStreaming(json))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::messages::anthropic_types::AnthropicInputMessage;
    use serde_json::json;

    fn thinking(kind: &str, budget: Option<i64>) -> AnthropicThinkingConfig {
        AnthropicThinkingConfig {
            kind: kind.to_string(),
            budget_tokens: budget.into(),
            display: Default::default(),
            extra: Default::default(),
        }
    }

    #[test]
    fn beta_header_filters_to_allowed() {
        let out = build_anthropic_beta_header(
            Some("interleaved-thinking-2025-05-14, bogus-beta, advanced-tool-use-2025-11-20"),
            None,
            "claude-opus-4.8",
        );
        assert_eq!(
            out.as_deref(),
            Some("interleaved-thinking-2025-05-14,advanced-tool-use-2025-11-20")
        );
    }

    #[test]
    fn beta_header_empty_after_filter_is_none() {
        let out = build_anthropic_beta_header(Some("bogus, also-bogus"), None, "m");
        assert_eq!(out, None);
    }

    #[test]
    fn beta_header_keeps_context_1m_beta() {
        // The [1m] model variant folds context-1m-2025-08-07 into the header; it
        // must survive the allowlist filter.
        let out = build_anthropic_beta_header(Some(CONTEXT_1M_BETA), None, "claude-opus-4.8");
        assert_eq!(out.as_deref(), Some(CONTEXT_1M_BETA));
    }

    #[test]
    fn beta_header_keeps_task_budgets_beta() {
        let out = build_anthropic_beta_header(Some(TASK_BUDGETS_BETA), None, "claude-opus-4.8");
        assert_eq!(out.as_deref(), Some(TASK_BUDGETS_BETA));
    }

    #[test]
    fn beta_header_from_thinking_budget() {
        let t = thinking("enabled", Some(1024));
        let out = build_anthropic_beta_header(None, Some(&t), "m");
        assert_eq!(out.as_deref(), Some(INTERLEAVED_THINKING_BETA));
    }

    #[test]
    fn beta_header_adaptive_thinking_is_none() {
        let t = thinking("adaptive", Some(1024));
        let out = build_anthropic_beta_header(None, Some(&t), "m");
        assert_eq!(out, None);
    }

    #[test]
    fn beta_header_no_thinking_is_none() {
        let out = build_anthropic_beta_header(None, None, "m");
        assert_eq!(out, None);
    }

    #[test]
    fn beta_header_zero_budget_is_none() {
        let t = thinking("enabled", Some(0));
        let out = build_anthropic_beta_header(None, Some(&t), "m");
        assert_eq!(out, None);
    }

    fn payload_with(messages: Vec<AnthropicInputMessage>) -> AnthropicMessagesPayload {
        AnthropicMessagesPayload {
            model: "m".to_string(),
            messages,
            max_tokens: Some(100),
            ..Default::default()
        }
    }

    fn msg(role: &str, content: Value) -> AnthropicInputMessage {
        AnthropicInputMessage {
            role: role.to_string(),
            content,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn initiate_string_content_is_true() {
        let p = payload_with(vec![msg("user", json!("hello"))]);
        assert!(is_initiate_request(&p));
    }

    #[test]
    fn initiate_array_with_text_block_is_true() {
        let p = payload_with(vec![msg("user", json!([{ "type": "text", "text": "hi" }]))]);
        assert!(is_initiate_request(&p));
    }

    #[test]
    fn initiate_only_tool_result_is_false() {
        let p = payload_with(vec![msg(
            "user",
            json!([{ "type": "tool_result", "tool_use_id": "1", "content": "ok" }]),
        )]);
        assert!(!is_initiate_request(&p));
    }

    #[test]
    fn initiate_assistant_last_is_false() {
        let p = payload_with(vec![
            msg("user", json!("hi")),
            msg("assistant", json!("there")),
        ]);
        assert!(!is_initiate_request(&p));
    }

    #[test]
    fn initiate_empty_messages_is_false() {
        let p = payload_with(vec![]);
        assert!(!is_initiate_request(&p));
    }

    #[test]
    fn vision_detects_image_block() {
        let block = json!({ "type": "image", "source": {} });
        assert!(block_has_image(&block));
    }

    #[test]
    fn vision_detects_image_in_tool_result() {
        let block = json!({
            "type": "tool_result",
            "content": [{ "type": "image", "source": {} }]
        });
        assert!(block_has_image(&block));
    }

    #[test]
    fn vision_ignores_text_block() {
        let block = json!({ "type": "text", "text": "hi" });
        assert!(!block_has_image(&block));
    }
}
