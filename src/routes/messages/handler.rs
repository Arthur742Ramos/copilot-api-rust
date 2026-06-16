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
use crate::libs::config::{get_small_model, is_messages_api_enabled, resolve_mapped_model};
use crate::libs::error::AppError;
use crate::libs::models::find_endpoint_model;
use crate::libs::provider_model::parse_provider_model_alias;
use crate::libs::rate_limit::check_rate_limit;
use crate::libs::state;
use crate::libs::subagent::parse_subagent_marker_from_first_user;
use crate::libs::utils::{generate_request_id_from_payload, get_root_session_id, get_uuid};
use crate::routes::messages::anthropic_types::AnthropicMessagesPayload;
use crate::routes::messages::api_flows::{
    handle_with_chat_completions, handle_with_messages_api, handle_with_responses_api, FlowOptions,
};
use crate::routes::messages::preprocess::{
    apply_last_message_cache_control, get_compact_type, get_last_message_content_cache_control,
    merge_tool_result_for_claude, normalize_system_messages, sanitize_ide_tools,
    strip_tool_reference_turn_boundary,
};
use crate::routes::responses::utils::get_responses_transport_for_model;
use crate::services::copilot::get_models::Model;

const MESSAGES_ENDPOINT: &str = "/v1/messages";

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

/// Mirrors `handleCompletion`. `body` is the raw Anthropic request JSON; it is
/// kept as a `serde_json::Value` so the in-place preprocess passes (which take
/// `&mut Value`) can mutate it, and deserialized into a typed
/// [`AnthropicMessagesPayload`] only when handed to the flow handlers.
pub async fn handle_completion(body: Value, headers: HeaderMap) -> Result<Response, AppError> {
    let mut payload = body;

    // 1. Resolve configured model mappings.
    let requested_model = model_of(&payload);
    let mapped_model = resolve_mapped_model(&requested_model);
    set_model(&mut payload, &mapped_model);
    if mapped_model != requested_model {
        tracing::debug!("Resolved model mapping: {requested_model} -> {mapped_model}");
    }

    // 2. Web-search server-tool short-circuit. Mirrors `tryHandleWebSearch`.
    //
    // TODO web_search: wire once `web_search::fulfill::try_handle_web_search`
    // stabilizes its signature (it is being built this stage and currently
    // exposes the building blocks but not the entry point, whose final shape
    // takes a generic provider-forward callback). Expected behaviour: if it
    // returns `Some(response)`, return it here; the call must run BEFORE the
    // provider-alias step below since web-search may itself route to a provider.

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

    // 4. Anthropic preprocessing.
    normalize_system_messages(&mut payload);

    check_rate_limit().await?;

    sanitize_ide_tools(&mut payload);

    let subagent_marker = parse_subagent_marker_from_first_user(&payload);
    if subagent_marker.is_some() {
        tracing::debug!("Detected Subagent marker");
    }

    let mut session_id = get_root_session_id(&payload, &headers);

    // claude code / opencode compact / auto-continue detection.
    let compact_type = get_compact_type(&payload);

    // claude code 2.0.28+ warmup requests consume a premium request; force the
    // small model when no tools are used and this is not a compact request.
    let anthropic_beta = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let no_tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|t| t.is_empty())
        .unwrap_or(true);
    if anthropic_beta.is_some() && no_tools && compact_type == 0 {
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

    // 6. Request id + session id.
    let messages: Vec<Value> = payload
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let request_id = generate_request_id_from_payload(&messages, session_id.as_deref());
    tracing::debug!("Generated request ID: {request_id}");

    if session_id.is_none() {
        session_id = Some(get_uuid(&request_id));
    }
    tracing::debug!("Extracted session ID: {session_id:?}");

    if state::with_state(|s| s.manual_approve) {
        crate::libs::approval::await_approval().await?;
    }

    // 7. Resolve the concrete endpoint model and pin the payload model to it.
    let selected_model = find_endpoint_model(&model_of(&payload));
    if let Some(model) = selected_model.as_ref() {
        set_model(&mut payload, &model.id);
    }

    // 8. Dispatch. Deserialize the fully-preprocessed payload for the flows.
    let mut typed = deserialize_payload(&payload)?;
    let options = FlowOptions {
        subagent_marker,
        selected_model: selected_model.clone(),
        anthropic_beta_header: anthropic_beta,
        request_id,
        session_id,
        compact_type: Some(compact_type),
    };

    if should_use_messages_api(selected_model.as_ref()) {
        return handle_with_messages_api(&mut typed, options).await;
    }

    if should_use_responses_api(selected_model.as_ref(), compact_type) {
        return handle_with_responses_api(&typed, options).await;
    }

    handle_with_chat_completions(&typed, options).await
}

#[allow(clippy::result_large_err)]
fn deserialize_payload(payload: &Value) -> Result<AnthropicMessagesPayload, AppError> {
    serde_json::from_value(payload.clone())
        .map_err(|e| AppError::Other(anyhow::anyhow!("Invalid request payload: {e}")))
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
