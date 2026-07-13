//! Mirrors `src/routes/messages/handler.ts` (`handleCompletion`).
//!
//! Ports the full `/v1/messages` request lifecycle: model mapping, web-search
//! short-circuit, provider-alias delegation, Anthropic preprocessing, the
//! warmup small-model swap, request/session id derivation, and the three-way
//! dispatch between the Messages, Responses, and Chat-Completions flows.

use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::Value;

use crate::libs::compact::COMPACT_REQUEST;
use crate::libs::config::{
    get_message_api_web_search_model, get_small_model, is_messages_api_enabled,
    is_responses_api_web_search_enabled, provider_uses_responses_api, resolve_mapped_model,
};
use crate::libs::error::AppError;
use crate::libs::models::{find_endpoint_model, is_context_1m_model};
use crate::libs::provider_model::parse_provider_model_alias;
use crate::libs::state;
use crate::libs::subagent::parse_subagent_marker_from_first_user;
use crate::libs::utils::{generate_request_id_from_payload, get_root_session_id, get_uuid};
use crate::routes::messages::anthropic_types::AnthropicMessagesPayload;
use crate::routes::messages::api_flows::{
    handle_with_chat_completions, handle_with_messages_api, handle_with_responses_api, FlowOptions,
};
use crate::routes::messages::preprocess::{
    apply_last_message_cache_control, get_compact_type, get_last_message_content_cache_control,
    merge_tool_result_for_claude, normalize_system_messages, prepare_messages_api_payload,
    sanitize_ide_tools, strip_tool_reference_turn_boundary,
};
use crate::routes::messages::request_validation::validate_messages_request_shape;
use crate::routes::messages::responses_translation::validate_responses_request_controls;
use crate::routes::messages::web_search::fulfill::{
    resolve_web_search_route, ResolveWebSearchRouteOptions, WebSearchRoute,
};
use crate::routes::responses::utils::get_responses_transport_for_model;
use crate::services::copilot::create_messages::CONTEXT_1M_BETA;
use crate::services::copilot::get_models::Model;

const MESSAGES_ENDPOINT: &str = "/v1/messages";
const CLAUDE_CODE_AGENT_ID_HEADER: &str = "x-claude-code-agent-id";

/// Reads the `model` field from the (loosely-typed) payload, defaulting to "".
fn model_of(payload: &Value) -> String {
    payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Writes the `model` field on the payload, ensuring it stays an object.
fn set_model(payload: &mut Value, model: &str) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".to_string(), Value::String(model.to_string()));
    }
}

fn is_claude_code_subagent_warmup(
    payload: &Value,
    headers: &HeaderMap,
    has_subagent_marker: bool,
) -> bool {
    let no_tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| tools.is_empty())
        .unwrap_or(true);
    headers.contains_key("anthropic-beta")
        && no_tools
        && get_compact_type(payload) == 0
        && (has_subagent_marker || headers.contains_key(CLAUDE_CODE_AGENT_ID_HEADER))
}

/// Validate controls only after resolving the request's actual transport.
/// Native Anthropic and Chat Completions retain their own control support.
#[allow(clippy::result_large_err)]
fn validate_selected_responses_controls(
    payload: &Value,
    headers: &HeaderMap,
) -> Result<(), AppError> {
    let has_nonempty_stop = payload
        .get("stop_sequences")
        .and_then(Value::as_array)
        .is_some_and(|sequences| !sequences.is_empty());
    let has_non_null = |field: &str| payload.get(field).is_some_and(|value| !value.is_null());
    if !has_nonempty_stop
        && ![
            "top_k",
            "cache_control",
            "service_tier",
            "temperature",
            "top_p",
        ]
        .into_iter()
        .any(has_non_null)
    {
        return Ok(());
    }
    let typed = deserialize_payload(payload)?;

    if has_web_search_server_tool_value(payload) {
        match resolve_web_search_route(
            &typed,
            ResolveWebSearchRouteOptions {
                web_search_model: get_message_api_web_search_model(),
                responses_web_search_enabled: is_responses_api_web_search_enabled(),
            },
        ) {
            WebSearchRoute::Responses { .. } => {
                return validate_responses_request_controls(&typed, false);
            }
            WebSearchRoute::Provider { alias } if provider_uses_responses_api(&alias.provider) => {
                return validate_responses_request_controls(&typed, alias.provider == "codex");
            }
            WebSearchRoute::Provider { .. } => return Ok(()),
            WebSearchRoute::Strip => {}
        }
    }

    if let Some(alias) = parse_provider_model_alias(&model_of(payload)) {
        if provider_uses_responses_api(&alias.provider) {
            return validate_responses_request_controls(&typed, alias.provider == "codex");
        }
        return Ok(());
    }

    let compact_type = get_compact_type(payload);
    let warmup = is_claude_code_subagent_warmup(
        payload,
        headers,
        parse_subagent_marker_from_first_user(payload).is_some(),
    );
    let effective_model = if warmup {
        get_small_model()
    } else {
        model_of(payload)
    };
    let selected_model = find_endpoint_model(&effective_model);
    if !should_use_messages_api(selected_model.as_ref())
        && should_use_responses_api(selected_model.as_ref(), compact_type)
    {
        return validate_responses_request_controls(&typed, false);
    }
    Ok(())
}

/// Mirrors `handleCompletion`. `body` is the raw Anthropic request JSON; it is
/// kept as a `serde_json::Value` so the in-place preprocess passes (which take
/// `&mut Value`) can mutate it, and deserialized into a typed
/// [`AnthropicMessagesPayload`] only when handed to the flow handlers.
pub async fn handle_completion(body: Value, headers: HeaderMap) -> Result<Response, AppError> {
    let mut payload = body;

    // Reject a missing/empty `model` up front: a legitimate client always sends
    // a concrete model id. Without this, an empty string flows through model
    // resolution and silently succeeds against a default — a 200 for what is
    // really an invalid request.
    if model_of(&payload).trim().is_empty() {
        return Err(AppError::BadRequest(
            "model: field required and must be a non-empty string".to_string(),
        ));
    }
    validate_generation_request(&payload)?;
    validate_messages_request_shape(&payload)?;
    normalize_system_messages(&mut payload);
    validate_generation_request(&payload)?;

    // 1. Resolve configured model mappings.
    let requested_model = model_of(&payload);
    let mapped_model = resolve_mapped_model(&requested_model);
    set_model(&mut payload, &mapped_model);
    if mapped_model != requested_model {
        tracing::debug!("Resolved model mapping: {requested_model} -> {mapped_model}");
    }
    validate_selected_responses_controls(&payload, &headers)?;

    // Shared admission must precede every early dispatch below. In particular,
    // fulfilled web-search requests and provider aliases return directly and
    // would otherwise bypass rate limits and daily/per-key token budgets.
    crate::libs::admission::check_shared_admission()
        .await
        .map_err(AppError::Http)?;

    // 2. Web-search server-tool short-circuit. Mirrors `tryHandleWebSearch`.
    //
    // Runs BEFORE the provider-alias step because a web-search request may
    // itself route to a configured provider (via the injected callback). A
    // cheap `Value`-level check gates the typed round-trip so the common case
    // (no web_search server tool) avoids an extra deserialize/serialize.
    if has_web_search_server_tool_value(&payload) {
        let mut typed = deserialize_payload(&payload)?;
        let forward_headers = headers.clone();
        let web_search_result =
            crate::routes::messages::web_search::fulfill::try_handle_web_search(
                &mut typed,
                &headers,
                |fwd_payload, provider| async move {
                    crate::routes::provider::messages::handle_provider_messages_for_provider(
                        fwd_payload,
                        provider,
                        forward_headers,
                    )
                    .await
                },
            )
            .await;
        if let Some(result) = web_search_result {
            return result;
        }
        // The tool was stripped (not fulfilled); fold the mutations back into
        // the working `Value` payload.
        payload =
            serde_json::to_value(&typed).map_err(|e| AppError::Other(anyhow::anyhow!("{e}")))?;
    }

    // 3. `<provider>/model` alias -> delegate to the provider proxy.
    if let Some(alias) = parse_provider_model_alias(&model_of(&payload)) {
        set_model(&mut payload, &alias.model);
        let typed = deserialize_payload(&payload)?;
        return crate::routes::provider::messages::handle_provider_messages_for_provider(
            typed,
            alias.provider,
            headers,
        )
        .await;
    }

    crate::libs::premium_interactions::check_premium_interactions()?;

    sanitize_ide_tools(&mut payload);

    let subagent_marker = parse_subagent_marker_from_first_user(&payload);
    if subagent_marker.is_some() {
        tracing::debug!("Detected Subagent marker");
    }

    let mut session_id = get_root_session_id(&payload, &headers);

    // claude code / opencode compact / auto-continue detection.
    let compact_type = get_compact_type(&payload);

    // Claude Code subagent warmups can consume a premium request. Restrict the
    // small-model swap to identified subagents: ordinary no-tool user requests
    // carry the same beta header and must retain their selected model.
    let anthropic_beta = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let anthropic_version = headers
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if is_claude_code_subagent_warmup(&payload, &headers, subagent_marker.is_some()) {
        set_model(&mut payload, &get_small_model());
    }

    if compact_type != 0 {
        tracing::debug!("Compact request type: {compact_type}");
    }

    // 5. Tool-result merging + cache-control re-application (skipped under
    //    token-based billing). `state.token_based_billing` is `Option<bool>`;
    //    the TS `!state.tokenBasedBilling` runs the block unless it is `true`.
    let token_based_billing = state::with_state(|s| s.token_based_billing).unwrap_or(false);
    if !token_based_billing {
        let last_cache_control = payload
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|m| m.last())
            .cloned()
            .as_ref()
            .and_then(get_last_message_content_cache_control);

        strip_tool_reference_turn_boundary(&mut payload);

        // Merge tool_result + text blocks to avoid consuming premium requests
        // (skill invocations, edit hooks, plan/todo reminders). Compact requests
        // still run this, except the final compact message itself.
        merge_tool_result_for_claude(&mut payload, compact_type == COMPACT_REQUEST);

        apply_last_message_cache_control(&mut payload, last_cache_control.as_ref());
    }

    // 6. Request id + session id. Derive the id from a borrowed view of the
    // messages array rather than cloning it: the previous `.cloned()` allocated
    // a full copy of the (potentially multi-MB) conversation that then lived as
    // a function-scope local across the entire upstream await. The scoped block
    // ends the immutable borrow of `payload` before later `&mut` uses.
    let request_id = {
        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        generate_request_id_from_payload(messages, session_id.as_deref())
    };
    tracing::debug!("Generated request ID: {request_id}");

    if session_id.is_none() {
        session_id = Some(get_uuid(&request_id));
    }
    tracing::debug!("Extracted session ID: {session_id:?}");

    if state::with_state(|s| s.manual_approve) {
        crate::libs::approval::await_approval().await?;
    }

    // 7. Resolve the concrete endpoint model and pin the payload model to it.
    //
    // A `[1m]` suffix on the requested model selects the 1M-context variant: the
    // base model id resolves the endpoint, and the `context-1m-2025-08-07` beta
    // must be folded into the `anthropic-beta` header so the upstream actually
    // enables the larger window. Detect it before `find_endpoint_model` strips
    // the suffix while pinning the payload to the concrete model id.
    let anthropic_beta = if is_context_1m_model(&model_of(&payload)) {
        Some(merge_context_1m_beta(anthropic_beta.as_deref()))
    } else {
        anthropic_beta
    };

    let selected_model = find_endpoint_model(&model_of(&payload));
    if let Some(model) = selected_model.as_ref() {
        set_model(&mut payload, &model.id);
    }

    // Apply native-Messages-only preprocessing while the payload is still a
    // Value. Doing it here avoids a typed -> Value -> typed round trip over the
    // complete conversation inside `handle_with_messages_api`.
    let use_messages_api = should_use_messages_api(selected_model.as_ref());
    if use_messages_api {
        prepare_messages_api_payload(&mut payload, selected_model.as_ref());
    }

    // 8. Dispatch. Deserialize the fully-preprocessed payload for the flows.
    // `payload` is not read after this point, so consume it (`from_value` takes
    // the `Value` by value) rather than cloning a potentially multi-MB body.
    let typed = deserialize_payload_owned(payload)?;
    let options = FlowOptions {
        subagent_marker,
        selected_model: selected_model.clone(),
        anthropic_beta_header: anthropic_beta,
        anthropic_version_header: anthropic_version,
        request_id,
        session_id,
        compact_type: Some(compact_type),
    };

    if use_messages_api {
        return handle_with_messages_api(&typed, options).await;
    }

    if should_use_responses_api(selected_model.as_ref(), compact_type) {
        return handle_with_responses_api(&typed, options).await;
    }

    handle_with_chat_completions(&typed, options).await
}

/// Validate fields whose optional Rust representation is shared with
/// `/count_tokens` but which are required for generation. Rejecting here keeps
/// invalid requests from consuming admission/premium budgets or silently
/// acquiring transport-specific defaults.
#[allow(clippy::result_large_err)]
fn validate_generation_request(payload: &Value) -> Result<(), AppError> {
    match payload.get("messages") {
        Some(Value::Array(messages)) if !messages.is_empty() => {}
        Some(Value::Array(_)) => {
            return Err(AppError::BadRequest(
                "messages: must contain at least one message".to_string(),
            ));
        }
        _ => {
            return Err(AppError::BadRequest(
                "messages: field required and must be an array".to_string(),
            ));
        }
    }

    match payload.get("max_tokens").and_then(Value::as_u64) {
        Some(value) if value > 0 => Ok(()),
        _ => Err(AppError::BadRequest(
            "max_tokens: field required and must be a positive integer".to_string(),
        )),
    }
}

#[allow(clippy::result_large_err)]
fn deserialize_payload(payload: &Value) -> Result<AnthropicMessagesPayload, AppError> {
    serde_json::from_value(payload.clone())
        .map_err(|e| AppError::BadRequest(format!("Invalid request payload: {e}")))
}

/// Like [`deserialize_payload`] but consumes the `Value`, avoiding the deep
/// clone. Use at the final dispatch where the working `Value` is no longer read.
#[allow(clippy::result_large_err)]
fn deserialize_payload_owned(payload: Value) -> Result<AnthropicMessagesPayload, AppError> {
    serde_json::from_value(payload)
        .map_err(|e| AppError::BadRequest(format!("Invalid request payload: {e}")))
}

/// Folds the `context-1m-2025-08-07` beta into an existing comma-separated
/// `anthropic-beta` header value (or produces a header carrying just that beta).
/// Idempotent: a header already listing the beta is returned unchanged.
fn merge_context_1m_beta(existing: Option<&str>) -> String {
    let header = existing.unwrap_or("").trim();
    if header.is_empty() {
        return CONTEXT_1M_BETA.to_string();
    }
    let already_present = header
        .split(',')
        .map(str::trim)
        .any(|item| item == CONTEXT_1M_BETA);
    if already_present {
        header.to_string()
    } else {
        format!("{header},{CONTEXT_1M_BETA}")
    }
}

/// Cheap `Value`-level probe for an Anthropic `web_search` server tool: a
/// `tools[]` entry whose `type` starts with `web_search` and that has no
/// `input_schema`. Mirrors `is_web_search_server_tool` without deserializing.
fn has_web_search_server_tool_value(payload: &Value) -> bool {
    payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools.iter().any(|tool| {
                let is_web_search = tool
                    .get("type")
                    .and_then(Value::as_str)
                    .map(|t| t.starts_with("web_search"))
                    .unwrap_or(false);
                let no_input_schema = tool.get("input_schema").map(Value::is_null).unwrap_or(true);
                is_web_search && no_input_schema
            })
        })
        .unwrap_or(false)
}

/// Mirrors `shouldUseResponsesApi`.
fn should_use_responses_api(selected_model: Option<&Model>, compact_type: i32) -> bool {
    get_responses_transport_for_model(selected_model, Some(compact_type)).is_some()
}

/// Mirrors `shouldUseMessagesApi`.
fn should_use_messages_api(selected_model: Option<&Model>) -> bool {
    if !is_messages_api_enabled() {
        return false;
    }
    selected_model
        .and_then(|m| m.supported_endpoints.as_ref())
        .map(|endpoints| endpoints.iter().any(|e| e == MESSAGES_ENDPOINT))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_1m_beta_added_when_no_existing_header() {
        assert_eq!(merge_context_1m_beta(None), CONTEXT_1M_BETA);
        assert_eq!(merge_context_1m_beta(Some("")), CONTEXT_1M_BETA);
        assert_eq!(merge_context_1m_beta(Some("   ")), CONTEXT_1M_BETA);
    }

    #[test]
    fn context_1m_beta_appended_to_existing_header() {
        assert_eq!(
            merge_context_1m_beta(Some("interleaved-thinking-2025-05-14")),
            format!("interleaved-thinking-2025-05-14,{CONTEXT_1M_BETA}")
        );
    }

    #[test]
    fn context_1m_beta_is_idempotent() {
        let already = format!("foo,{CONTEXT_1M_BETA}");
        assert_eq!(merge_context_1m_beta(Some(&already)), already);
    }

    #[test]
    fn generation_validation_requires_messages_and_positive_max_tokens() {
        assert!(validate_generation_request(&serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1
        }))
        .is_ok());

        for invalid in [
            serde_json::json!({"messages": [], "max_tokens": 1}),
            serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 0
            }),
            serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1.5
            }),
        ] {
            assert!(matches!(
                validate_generation_request(&invalid),
                Err(AppError::BadRequest(_))
            ));
        }
    }

    #[test]
    fn no_tool_user_request_is_not_misclassified_as_subagent_warmup() {
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}],
            "tools": []
        });
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-beta", "claude-code-20250219".parse().unwrap());

        assert!(!is_claude_code_subagent_warmup(&payload, &headers, false));

        headers.insert(
            CLAUDE_CODE_AGENT_ID_HEADER,
            "agent-fixture".parse().unwrap(),
        );
        assert!(is_claude_code_subagent_warmup(&payload, &headers, false));
        headers.remove(CLAUDE_CODE_AGENT_ID_HEADER);
        assert!(is_claude_code_subagent_warmup(&payload, &headers, true));
    }
}
