//! OpenAI Responses compaction endpoint used by Codex CLI 0.144.1.

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

use crate::libs::compact::COMPACT_REQUEST;
use crate::libs::config::resolve_mapped_model;
use crate::libs::error::{openai_error_response, AppError, HttpError};
use crate::libs::provider_model::parse_provider_model_alias;
use crate::libs::provider_resolver::resolve_provider_config;
use crate::libs::state;
use crate::libs::token_usage::{
    create_copilot_token_usage_recorder, create_request_scoped_provider_token_usage_recorder,
    normalize_responses_usage, TokenUsageRecorder,
};
use crate::routes::parse_json_body;
use crate::routes::responses::handler::{
    get_codex_responses_subagent_marker, get_incoming_responses_session_id,
    remove_unsupported_tools, responses_request_id, validate_responses_payload,
};
use crate::routes::responses::utils::{
    get_responses_request_options, get_responses_transport_for_model,
};
use crate::services::codex::create_responses::forward_codex_compact;
use crate::services::copilot::create_responses::{
    create_responses, read_buffered_responses_response, CreateResponsesReturn,
    ResponsesBufferedBody, ResponsesBufferedContract, ResponsesCompactBufferedResult,
    ResponsesPayload, ResponsesRequestOptions,
};
use crate::services::providers::provider_proxy::forward_provider_responses_compact;

pub async fn post_responses_compact(headers: HeaderMap, body: Bytes) -> Response {
    let value = match parse_json_body(&body) {
        Ok(value) => value,
        Err(error) => return error.into_openai_response(),
    };
    match handle_responses_compact(value, headers).await {
        Ok(response) => response,
        Err(error) => error.into_openai_response(),
    }
}

async fn handle_responses_compact(body: Value, headers: HeaderMap) -> Result<Response, AppError> {
    let mut payload: ResponsesPayload = serde_json::from_value(body)
        .map_err(|error| AppError::BadRequest(format!("Invalid request payload: {error}")))?;
    validate_responses_payload(&payload)?;
    if payload.stream == Some(true) {
        return Err(AppError::BadRequest(
            "stream: /responses/compact is a non-streaming endpoint".to_string(),
        ));
    }
    payload.stream = None;

    payload.model = resolve_mapped_model(&payload.model);

    crate::libs::admission::check_shared_admission()
        .await
        .map_err(AppError::Http)?;

    if let Some(alias) = parse_provider_model_alias(&payload.model) {
        payload.model = alias.model;
        return handle_provider_compact(payload, alias.provider, headers).await;
    }

    crate::libs::premium_interactions::check_premium_interactions()?;
    remove_unsupported_tools(&mut payload);

    let selected_model = state::with_state(|state| {
        state.models.as_ref().and_then(|models| {
            models
                .data
                .iter()
                .find(|model| model.id == payload.model)
                .cloned()
        })
    });
    let Some(transport) =
        get_responses_transport_for_model(selected_model.as_ref(), Some(COMPACT_REQUEST))
    else {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("model_not_supported"),
            "This model does not support Responses compaction.",
        ));
    };

    let marker = get_codex_responses_subagent_marker(&headers);
    let incoming_session_id = get_incoming_responses_session_id(&headers);
    let session_id = incoming_session_id
        .as_deref()
        .map(crate::libs::utils::get_uuid);
    let request_id = responses_request_id(&payload, session_id.as_deref());
    let fallback_session_id =
        session_id.unwrap_or_else(|| crate::libs::utils::get_uuid(&request_id));
    let (vision, inferred_initiator) = get_responses_request_options(&payload);
    let response_model = payload.model.clone();
    let recorder = create_copilot_token_usage_recorder(
        "responses_compact",
        response_model.clone(),
        Some(fallback_session_id.clone()),
    );

    let result = create_responses(
        payload,
        ResponsesRequestOptions {
            vision,
            initiator: if marker.is_some() {
                "agent"
            } else {
                inferred_initiator
            },
            subagent_marker: marker.as_ref(),
            request_id: &request_id,
            session_id: Some(&fallback_session_id),
            compact_type: Some(COMPACT_REQUEST),
            transport,
            buffered_contract: ResponsesBufferedContract::Compact,
        },
    )
    .await?;

    match result {
        CreateResponsesReturn::CompactResult(result) => Ok(finish_compact_response(
            result,
            recorder,
            &response_model,
            StatusCode::OK,
        )),
        CreateResponsesReturn::Result(_) => Err(HttpError::internal(
            "Responses compact unexpectedly returned a full response",
        )
        .into()),
        CreateResponsesReturn::Stream(_) => {
            Err(HttpError::internal("Responses compact unexpectedly returned a stream").into())
        }
    }
}

fn finish_compact_response(
    result: Box<ResponsesCompactBufferedResult>,
    recorder: TokenUsageRecorder,
    response_model: &str,
    status: StatusCode,
) -> Response {
    if let Some(ctx) = crate::libs::request_context::request_context_store() {
        ctx.set_flow_transport_model_non_streaming(
            response_model,
            "responses_compact",
            crate::libs::stream_metrics::transport::NATIVE,
        );
    }
    let usage = result
        .parsed
        .usage
        .as_ref()
        .and_then(|usage| serde_json::to_value(usage).ok());
    recorder.record(normalize_responses_usage(usage.as_ref()));
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(result.raw))
        .expect("static compact Responses response");
    for (name, value) in &result.headers {
        response.headers_mut().insert(name, value.clone());
    }
    response
}

async fn handle_provider_compact(
    payload: ResponsesPayload,
    provider: String,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let Some(config) = resolve_provider_config(&provider).await else {
        return Ok(openai_error_response(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            Some("provider_not_found"),
            format!("Provider '{provider}' not found or disabled."),
        ));
    };
    if config.provider_type != "openai-responses" {
        return Ok(openai_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("unsupported_endpoint"),
            format!("Provider '{provider}' does not support /v1/responses/compact."),
        ));
    }

    let response_model = payload.model.clone();
    let recorder = create_request_scoped_provider_token_usage_recorder(
        "responses_compact",
        response_model.clone(),
        provider.clone(),
    );
    let upstream_start = std::time::Instant::now();
    let upstream_result = if config.name == "codex" {
        forward_codex_compact(&payload, &headers, &config.base_url).await
    } else {
        forward_provider_responses_compact(&config, &payload, &headers).await
    };
    let upstream = match upstream_result {
        Ok(upstream) => {
            crate::libs::metrics::record_provider_upstream_request(
                ResponsesBufferedContract::Compact.metrics_endpoint(),
                crate::libs::metrics::UpstreamStatus::from_code(upstream.status().as_u16()),
                upstream_start.elapsed().as_secs_f64(),
            );
            upstream
        }
        Err(error) => {
            crate::libs::metrics::record_provider_upstream_request(
                ResponsesBufferedContract::Compact.metrics_endpoint(),
                crate::libs::metrics::UpstreamStatus::TransportError,
                upstream_start.elapsed().as_secs_f64(),
            );
            return Err(error.into());
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let error_message = format!("Failed to compact {provider} responses");
    let result = match read_buffered_responses_response(
        upstream,
        ResponsesBufferedContract::Compact,
        &error_message,
    )
    .await?
    {
        ResponsesBufferedBody::Compact(result) => result,
        ResponsesBufferedBody::Regular(_) => {
            return Err(HttpError::internal(
                "Provider compact unexpectedly returned a full response",
            )
            .into())
        }
    };
    Ok(finish_compact_response(
        result,
        recorder,
        &response_model,
        status,
    ))
}
