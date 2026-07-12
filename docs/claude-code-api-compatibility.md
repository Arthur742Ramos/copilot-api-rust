# Claude Code / Anthropic API compatibility

This document records the compatibility contract for clients that speak the
Anthropic Messages API, especially Claude Code. It is an implementation matrix,
not a claim that every Anthropic product endpoint is emulated.

## Reference audited

- Reference implementation:
  [`caozhiyuan/copilot-api`](https://github.com/caozhiyuan/copilot-api) at
  commit
  [`65ab96bd806f47c35443aa58b65134d45a345570`](https://github.com/caozhiyuan/copilot-api/tree/65ab96bd806f47c35443aa58b65134d45a345570)
  (`dev`).
- Retrieved/audited: **2026-07-12T04:22:34Z**.
- Compared surfaces: `src/routes/messages/` (handler, token counting,
  preprocessing, request, non-streaming and both streaming translators),
  `src/routes/provider/messages/`, `src/routes/models.ts`, `src/lib/error.ts`, and
  `src/services/copilot/create-messages.ts`.
- The Rust comparison was performed against the repository state immediately
  before this compatibility pass (`2c8a37436462c808830c62c11ea367cd9f53817b`).

Status terms:

- **Supported** — implemented with deterministic credential-free coverage.
- **Fixed here** — a concrete compatibility or lifecycle gap found during this
  audit and corrected in this pass.
- **Intentional divergence** — differs from the TypeScript reference for a
  documented safety, correctness, or upstream-compatibility reason.
- **Out of scope** — not promised by this proxy.

## Endpoint and request matrix

| Surface / behavior | Status | Rust implementation and regression evidence | Notes |
|---|---|---|---|
| `POST /v1/messages` (streaming and JSON) | Supported | `src/routes/messages/handler.rs`; `src/routes/messages/api_flows.rs`; `tests/router_smoke.rs` | Dispatches to native Messages, Responses, or Chat Completions based on resolved model capabilities. |
| `POST /v1/messages/count_tokens` | Supported | `src/routes/messages/count_tokens_handler.rs`; tokenizer tests; `tests/provider_routing.rs::provider_model_alias_count_tokens_returns_complete_anthropic_404` | Uses Anthropic's counter when configured; otherwise uses the documented local estimate, including provider/model aliases. |
| `GET /v1/models` and `GET /v1/models/:id` | Supported | `src/routes/models.rs` tests `shape_model_overlays_client_fields` and `shape_model_advertises_1m_variant` | Returns Anthropic-shaped model records and `[1m]` aliases. |
| Provider-scoped Messages route | Supported | `src/routes/provider/messages.rs`; `tests/provider_routing.rs::unknown_provider_returns_complete_anthropic_404` | Anthropic, OpenAI-compatible, Responses, and configured model aliases are covered. |
| Provider-scoped and provider/model-alias count-token routes | **Fixed here** | `src/routes/provider/count_tokens.rs`; tests `unknown_provider_count_tokens_returns_complete_anthropic_404`, `provider_model_alias_count_tokens_returns_complete_anthropic_404`, `direct_and_alias_count_tokens_malformed_json_returns_anthropic_400`, `direct_and_alias_count_tokens_invalid_payloads_return_anthropic_400`, and `direct_and_alias_count_tokens_body_limits_return_anthropic_413` | Direct and alias dispatch now share complete Anthropic envelopes for provider resolution, raw JSON parsing, typed-payload validation, and request-size rejection. |
| Required `model`, non-empty `messages`, positive integer `max_tokens` | **Fixed here** | `validate_generation_request`; test `generation_validation_requires_messages_and_positive_max_tokens`; router test `generation_requires_positive_max_tokens_before_upstream_dispatch` | Generation now fails as a stable 400 before admission/accounting instead of silently acquiring a transport-specific default. `count_tokens` intentionally does not require `max_tokens`. |
| String and structured system prompts | Supported | `src/routes/messages/preprocess.rs`; request translation tests | Normalizes multiple system representations while preserving cache-control data. |
| User/assistant text, image, document/PDF, tool-use, and tool-result blocks | Supported | `src/routes/messages/non_stream_translation.rs` and `responses_translation.rs`; tests including `tool_result_message_comes_before_user_message` and `tool_result_image_moves_to_user_message_when_unsupported` | Unsupported provider media capabilities return a client error or use the provider's documented fallback rather than panicking. |
| Tool definitions and `auto` / `any` / named `tool` / `none` choice | Supported | `translate_anthropic_tools_to_openai`; test `tool_choice_maps_variants` | Object schemas missing `properties` are normalized for OpenAI-compatible providers. |
| Prompt caching and `cache_control` | Supported | `src/routes/messages/preprocess.rs`; `api_flows.rs` cache marker tests | Cache markers and tool-result merge behavior match Claude Code request patterns. |
| Extended/adaptive thinking and reasoning signatures | Supported | `non_stream_translation.rs`; `responses_translation.rs`; streaming translator tests; `create_messages.rs` beta tests | Includes compaction-carrier and reasoning signature round trips. |
| Metadata / `user_id` and safe unknown JSON fields | Supported | `anthropic_types.rs` tests `messages_payload_round_trips_byte_stable` and `response_round_trips_unknown_usage_and_top_level_fields` | Unknown fields survive native typed round trips via flattened maps; translated transports preserve fields they can represent safely. |
| Web-search server tool bridge | Supported | `src/routes/messages/web_search/`; provider web-search tests | Fulfilled requests preserve Anthropic content and usage shapes. |
| Model mapping, endpoint normalization, warmup/small-model selection, and `[1m]` beta injection | Supported | `handler.rs`; model/config tests; `create_messages.rs::beta_header_keeps_context_1m_beta` | The alias is resolved before transport selection and the 1M beta is injected idempotently. |

## Response and streaming matrix

| Surface / behavior | Status | Rust implementation and regression evidence | Notes |
|---|---|---|---|
| Non-streaming text, thinking, tool-use, usage, and stop reasons | Supported | `non_stream_translation.rs`; `responses_translation.rs` tests | Produces Anthropic `message` responses for both translated transports. |
| Anthropic SSE lifecycle (`message_start`, sequential content blocks, `message_delta`, `message_stop`) | Supported | `stream_translation.rs`; `responses_stream_translation.rs` | Content-block indices are deterministic and open/stop ordering is tested. |
| Fragmented Chat Completions tool arguments | Supported | Test `tool_call_input_json_accumulation` | Preserves `input_json_delta` fragment order. |
| Parallel/interleaved Chat Completions tool calls | **Fixed here** | Test `parallel_fragmented_tool_calls_are_serialized_into_valid_blocks` | OpenAI indices may interleave; Anthropic blocks are now serialized in first-seen order so a stopped block is never reopened. |
| Parallel/interleaved Responses function calls | **Fixed here** | Test `parallel_function_calls_keep_anthropic_blocks_sequential` | Pending calls buffer fragments until the active Anthropic block closes. |
| Thinking/reasoning deltas and signatures | Supported | Tests `thinking_text_opens_and_closes_thinking_block`, `reasoning_only_turn_closes_thinking_block_on_finish`, and Responses reasoning tests | A non-empty compatibility placeholder is emitted when only an opaque signature exists. |
| Usage deltas, cached tokens, and partial-stream accounting | Supported | Usage mapping tests in both translators; `token_usage` tests | Successful terminal usage and sniffed partial usage are recorded without negative uncached-token counts. |
| `content_filter` stop semantics | **Fixed here** | `utils.rs`; test `content_filter_incomplete_maps_to_refusal` | Maps to Anthropic `refusal`, not a misleading successful `end_turn`. |
| Empty, truncated, or `[DONE]`-before-finish translated streams | **Fixed here** | Tests `unterminated_stream_closes_block_then_errors` and `unterminated_empty_stream_emits_terminal_error` | Emits one terminal `api_error`; never fabricates success for partial output. |
| Malformed translated SSE JSON | **Fixed here** | Test `malformed_event_closes_open_block_and_errors_once`; flow drivers in `api_flows.rs` and provider routes | Closes any active block, emits one terminal error, and stops reading. |
| Top-level translated Chat Completions upstream error objects | **Fixed here** | `stream_translation.rs`; tests `top_level_upstream_error_closes_open_block_before_safe_error`, `top_level_upstream_error_closes_thinking_block_in_protocol_order`, `top_level_upstream_error_discards_pending_success_and_terminates_once`, `malformed_or_unsafe_top_level_error_uses_opaque_fallback`, and `empty_choices_completes_pending_with_usage`; public and provider flow drivers | Detects a non-null top-level `error` before the legitimate empty-choice usage path. Active blocks close first, deferred success is discarded, only safe bounded type/message fields are retained, and later chunks/EOF cannot emit success or another terminal event. |
| Out-of-order or duplicate Responses lifecycle events | **Fixed here** | Tests `completion_before_created_terminates_with_error` and `duplicate_created_terminates_with_error_without_second_start` | Completion cannot precede `message_start`, and duplicate starts terminate with an error instead of emitting an invalid Anthropic lifecycle. |
| Native Anthropic stream truncation/malformed JSON | **Fixed here** | Native stream drivers in `api_flows.rs` and `provider/messages.rs` | A native stream must end in upstream `message_stop` or `error`; silent EOF is converted to an error, not synthetic `message_stop`. |
| Timeout/reset/body-read stream failures | **Fixed here** | `is_transient_transport_error`; test `transient_transport_cause_maps_to_overloaded_error` | Retryable transport failures consistently become Anthropic `overloaded_error` across all transports. |
| Keepalive pings | Supported | SSE flow drivers and `src/libs/sse.rs` tests | Pings do not count as first-token content for latency metrics. |

## Headers, betas, and errors

| Surface / behavior | Status | Rust implementation and regression evidence | Notes |
|---|---|---|---|
| Anthropic beta allowlist | Supported | `create_messages.rs` tests `beta_header_filters_to_allowed`, `beta_header_keeps_context_1m_beta`, and thinking-beta tests | Unknown client beta values are not blindly forwarded upstream. |
| Claude Code initiator and editor-version headers | Supported | `create_messages.rs`; subagent/initiator tests | Distinguishes user, agent, and tool-result turns. |
| Claude Opus 4.8 identity-header exception | Intentional divergence | `create_messages.rs` model-specific header logic | The upstream Copilot WAF rollout does not yet accept Claude Code identity headers for this model; omitting them avoids a known rejection. Remove only after upstream parity is confirmed. |
| Anthropic error envelopes and status-derived types | **Fixed here** | `src/libs/error.rs` tests `upstream_status_maps_to_anthropic_error_type`, `upstream_json_error_envelope_is_forwarded_verbatim`, and `complete_upstream_error_envelope_is_preserved`; provider count-token regressions in `tests/provider_routing.rs`; `router_smoke.rs::oversize_body_returns_json_shaped_413`; `zstd_request::tests::zstd_failures_use_complete_anthropic_envelopes` | Audited 400/401/403/404/413/429/529/5xx paths map to complete Anthropic error envelopes with their original status and safe diagnostic. Nested-only upstream errors gain the SDK-required top-level discriminator without changing their nested error, status, or allowed headers; local body limits use `request_too_large`. |
| Nested message extraction for 429/529 | **Fixed here** | Test `rate_limit_429_renders_anthropic_shape` | The client receives the human message, not JSON encoded inside a string. |
| Retry/rate-limit/correlation response headers | **Fixed here / intentional hardening** | Tests `error_headers_use_explicit_allowlist` and `rate_limit_does_not_forward_arbitrary_x_headers` | Correlation IDs are forwarded on every failure; retry metadata on 429/529. Arbitrary `x-*` headers are deliberately not reflected. |
| Internal error sanitization | **Fixed here** | Tests `app_error_other_renders_500_api_error` and `synthetic_internal_http_error_hides_diagnostic` | Detailed transport/parse diagnostics stay in logs; clients get an opaque trace reference. |

## Intentional divergences and limits

1. **Stricter generation validation.** The TypeScript reference can let a
   missing `max_tokens` flow into different upstream defaults. This proxy
   returns `invalid_request_error` consistently. This does not affect
   `/count_tokens`.
2. **Header allowlisting.** The TypeScript path forwards broad `x-*` metadata
   for some retryable errors. This proxy forwards only documented retry,
   rate-limit, and correlation fields to avoid leaking internal routing data.
3. **Interrupted streams are errors.** A clean socket EOF is not proof that a
   model completed. The proxy requires an upstream finish/completion event and
   does not synthesize `end_turn` or `message_stop`.
4. **Parallel tool calls are serialized at the Anthropic wire boundary.**
   Argument bytes and call order are preserved, but buffered calls may appear
   slightly later than they did on the OpenAI wire. This is required to maintain
   Anthropic's sequential content-block lifecycle.
5. **Token counts can be estimates.** Without an Anthropic credential, the
   count-token route uses the configured tokenizer and Claude multiplier. It is
   suitable for context budgeting but is not represented as byte-for-byte
   billing authority.

## Explicitly out of scope

- Anthropic endpoints other than Messages, Messages token counting, and model
  discovery.
- Live credential/provider availability, quota policy, model quality, or exact
  upstream rollout timing; the regression suite is credential-free.
- Blind passthrough of unknown beta headers, private/internal response headers,
  or capabilities that cannot be represented safely on the selected upstream
  transport.
- Guaranteeing future Claude Code or Anthropic protocol additions that postdate
  the audited reference commit. New betas/events require a new audit and matrix
  update.

## Maintaining this contract

When updating the TypeScript reference or adding a transport:

1. Record its exact commit and UTC retrieval time above.
2. Add or update a matrix row, including an explicit divergence rationale.
3. Add a credential-free regression for request shape, event ordering, terminal
   behavior, and error classification.
4. Run the repository quality gates documented in `AGENTS.md`.
