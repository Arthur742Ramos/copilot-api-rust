# Claude Code and Codex CLI compatibility

This is the audited client contract for the proxy's two coding-agent clients.
It describes what is verified, how to configure each client, and where the
intentional limits are. It is not a promise to emulate every Anthropic or
OpenAI endpoint.

## Audit identity and reproducible evidence

Audited on **2026-07-12**:

| Client/reference | Exact identity | Evidence |
|---|---|---|
| Claude Code | **2.1.207**, release [`v2.1.207`](https://github.com/anthropics/claude-code/releases/tag/v2.1.207) | Official release tag; installed `claude --version` → `2.1.207 (Claude Code)` |
| OpenAI Codex CLI | **0.144.1**, release [`rust-v0.144.1`](https://github.com/openai/codex/releases/tag/rust-v0.144.1) | Source commit [`44918ea10c0f99151c6710411b4322c2f5c96bea`](https://github.com/openai/codex/tree/44918ea10c0f99151c6710411b4322c2f5c96bea); `codex --version` |
| TypeScript reference | `caozhiyuan/copilot-api` | Commit [`cd8207cb70ede07771bf37a04accfbf2af76d980`](https://github.com/caozhiyuan/copilot-api/tree/cd8207cb70ede07771bf37a04accfbf2af76d980) |
| Rust boundary harness | this repository | `cargo test --test client_compatibility` |

Useful audit commands:

```sh
gh api repos/anthropics/claude-code/releases/latest --jq .tag_name
gh api repos/openai/codex/releases/latest --jq .tag_name
claude --version
codex --version
cargo test --test client_compatibility
```

The Codex source is explicit that `wire_api = "chat"` has been removed and the
supported wire protocol is `responses`; see
[`model-provider-info/src/lib.rs`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/model-provider-info/src/lib.rs#L50-L82).
Its HTTP client posts to `responses`, its remote compactor posts to
`responses/compact`, and its canonical request fields are defined in
[`codex-api/src/common.rs`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/codex-api/src/common.rs#L216-L239).
Continuation item optionality is audited against
[`protocol/src/models.rs`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/protocol/src/models.rs#L932-L1163).

The normal test suite never starts the production port, contacts an external
provider, or consumes quota. `tests/client_compatibility.rs` enters through the
real public Axum router and uses an ephemeral loopback Axum upstream. It captures
the forwarded request and supplies deterministic Anthropic JSON/SSE, OpenAI
JSON/SSE, HTTP failures, malformed frames, and premature EOF.

## Claude Code 2.1.207 setup

Claude Code uses the Anthropic base URL **without** `/v1`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:4141",
    "ANTHROPIC_AUTH_TOKEN": "your-copilot-api-client-key",
    "ANTHROPIC_MODEL": "claude-sonnet-4-6",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-4-5"
  }
}
```

Put that in project `.claude/settings.json`, user settings, or export the same
variables before launching `claude`. If `auth.apiKeys` is empty, any non-empty
placeholder token works. If keys are configured, `ANTHROPIC_AUTH_TOKEN` must
match one of them. The gateway accepts the resulting bearer token as well as
`x-api-key`.

Claude Code exercises:

- `POST /v1/messages` with `anthropic-version`, `anthropic-beta`,
  `content-type`, client identity/subagent headers, and streaming or JSON bodies.
- `POST /v1/messages/count_tokens` for context budgeting.
- `GET /v1/models` for gateway-backed model discovery in current Claude Code.

Provider models use the same client setup with a `provider/model` ID, for
example `anthropic-prod/claude-sonnet-4-6`. The proxy removes the provider
prefix and replaces the client credential with the provider credential.

## Codex CLI 0.144.1 setup

Codex appends `responses` (and, when enabled by the client/provider,
`responses/compact`) to the configured base URL, so its base URL **must include**
`/v1`.

`~/.codex/config.toml`:

```toml
model = "gpt-5.4"
model_provider = "copilot_api"

[model_providers.copilot_api]
name = "copilot_api"
base_url = "http://127.0.0.1:4141/v1"
env_key = "COPILOT_API_KEY"
wire_api = "responses"
```

Then:

```sh
export COPILOT_API_KEY=your-copilot-api-client-key
codex
```

Use a placeholder value when gateway `auth.apiKeys` is empty. Do not put a
provider/OpenAI secret in this variable unless it is intentionally also your
gateway client key; configured upstream providers have their own `apiKey`, and
the gateway replaces inbound authorization before forwarding.

The public Responses WebSocket transport is not exposed. Leave
`supports_websockets` unset/false so Codex uses audited HTTP SSE.

Codex 0.144.1 sends:

- `POST /v1/responses`, normally with `stream: true`.
- `POST /v1/responses/compact` when Codex enables remote compaction for the
  selected provider.
- `Authorization: Bearer ...`, `Accept: text/event-stream`,
  `Content-Type: application/json`, `User-Agent`, `session-id`, `thread-id`,
  `x-client-request-id`, installation/turn metadata, and subagent headers.

The request body includes `model`, `instructions`, an input-item array, tools,
`tool_choice`, `parallel_tool_calls`, reasoning controls, `store`, `stream`,
`include`, `service_tier`, `prompt_cache_key`, text controls, and optional
client metadata. HTTP Codex requests carry full prior response items; the
WebSocket-only `previous_response_id` optimization is not required by the HTTP
configuration above. Unknown public Responses fields, including continuation
fields, are retained.

### Audited Codex continuation items

Codex sends the full prior `ResponseItem` history on the HTTP transport. The
proxy accepts every 0.144.1 variant and preserves variants it does not need to
inspect as raw JSON. The typed variants use the following required/optional
contract:

| Item/content | Required fields | Optional fields accepted |
|---|---|---|
| `message` | `role`, `content` | `id`, `phase`, internal metadata |
| `input_image` | `image_url` | `detail` |
| `reasoning` | `summary` | `id`, `content`, `encrypted_content`, internal metadata |
| `function_call` | `name`, `arguments`, `call_id` | `id`, `namespace`, internal metadata |
| `function_call_output` | `call_id`, `output` | `id`, internal metadata; image-output `detail` |
| `tool_search_call` | `execution`, `arguments` | `id`, `call_id`, `status`, internal metadata |
| `tool_search_output` | `status`, `execution`, `tools` | `id`, `call_id`, internal metadata |
| `compaction` | `encrypted_content` | `id`, internal metadata |
| raw-preserved variants | variant-specific Codex fields | optional IDs/metadata on `additional_tools`, `agent_message`, `local_shell_call`, custom tools, web/image calls, `context_compaction`, and triggers |

`codex_0_144_1_optional_continuation_items_cross_provider_boundary` submits
these shapes through public `/v1/responses` using a configured
`provider/model` and asserts the deterministic upstream capture.
Legacy `type: "compaction_summary"` history is accepted and canonicalized to
`type: "compaction"` before latest-compaction trimming and forwarding.
`claude_optional_reasoning_carriers_cross_public_messages_boundary` separately
starts with Anthropic assistant thinking history and proves that all four
`encrypted_content`/`id` presence combinations reach the typed Responses
reasoning item unchanged; complete legacy values retain
`encrypted_content@id`, while missing-field combinations use `rs1#...`.
Present-but-empty strings also use the versioned carrier and remain distinct
from absent fields.
For Responses output, the paired public non-stream/stream
`claude_aggregate_empty_reasoning_*` regressions cover absent summaries, empty
arrays, empty text, and whitespace-only text. They emit the standard thinking
placeholder plus the exact opaque signature only when an ID or encrypted
content field is present (including an explicitly empty string); carrier-free
aggregate-empty reasoning emits no fabricated
Anthropic thinking/signature data.

### Reasoning framing and stream lifecycle policy

One policy is shared by JSON and SSE translations:

1. Summary parts stay in ascending `summary_index`/array order, followed by
   reasoning-content parts in ascending `content_index`/array order.
2. Every part's text is preserved byte-for-byte, including leading/trailing
   whitespace and empty elements. `response.reasoning_text.delta` fragments are
   accumulated at their `content_index`; an explicit empty delta still records
   its part boundary.
3. Distinct summary/content parts are joined with exactly `U+2063` followed by
   one blank line (`"\u2063\n\n"`), including boundaries adjacent to empty
   parts.
4. Whitespace is inspected only to classify the whole reasoning item as
   aggregate-empty. An aggregate-empty item with an `id` or
   `encrypted_content` field emits `Thinking...` plus its exact carrier; a
   carrier-free aggregate-empty item emits no thinking block or signature.
5. Anthropic history splits the reserved separator back into the original
   Responses summary parts.

Codex 0.144.1
[`process_responses_event`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/codex-api/src/sse/responses.rs#L326-L465)
parses `response.reasoning_summary_part.added`,
`response.reasoning_summary_text.delta`, and
`response.reasoning_summary_text.done`,
`response.reasoning_text.delta`, `response.output_item.added`, and the
authoritative `response.output_item.done`. It has no paired
`response.reasoning_text.done` mapping in this version. The stream bridge
therefore buffers summary/content fragments by output and part index, assigns
rather than appends authoritative summary `text.done` values, and renders once
at `output_item.done` from its final non-empty arrays or the ordered buffers.
This prevents duplicate separators when deltas and complete values both carry
the same text while retaining content-only delta streams.

The terminal contract is also source-backed. Codex's
[`ResponseCompleted`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/codex-api/src/sse/responses.rs#L112-L120)
requires only `id`; `usage` and `end_turn` are optional, and it does not model
`status`. Its checked-in
[`ev_completed`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/core/tests/common/responses.rs#L643-L656)
fixture therefore contains `id` and usage but no status. The parser handles
`response.incomplete` directly from the event discriminator and optional
`incomplete_details.reason`, while `response.failed` becomes an error from its
nested error payload
([source](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/codex-api/src/sse/responses.rs#L386-L449)).
The canonical
[`ev_response_created`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/core/tests/common/responses.rs#L658-L666)
contains only a non-empty response id—no model. The bridge therefore uses the
already validated requested/resolved provider model for Anthropic
`message_start` when model is absent; a non-empty upstream model, when present,
is retained instead.

Codex's typed
[`ResponseCompletedUsage`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/codex-api/src/sse/responses.rs#L122-L157)
requires integer `input_tokens`, `output_tokens`, and `total_tokens` whenever
usage is present. Cached-input and reasoning-output detail objects are optional,
but their integer fields are required when those objects exist.

Every incrementally streamed item must first have an active
`response.output_item.added` at its non-negative output index. Optional item IDs
may first appear on a later event and are then reconciled to that index; a
non-empty ID cannot move between indices. Structurally identical replayed
`added` and `done` items are safely ignored. Conflicting replays, deltas after
summary/item completion, wrong item/call IDs or function names, missing
reasoning `delta`/`text` payloads, and missing/negative indices produce one
terminal Anthropic `error`. A complete item with no incremental
events may still arrive as a standalone `output_item.done`; complete message
text and function-call arguments are rendered from that item exactly once.
Streamed message text and function arguments are reconciled with their complete
`done` values: a verified missing suffix is emitted, while a conflicting value
terminates with an error rather than returning truncated text or JSON.

When provider events include OpenAI `sequence_number`, it must increase
monotonically; an immediately replayed identical frame is ignored, while reused
or decreasing numbers fail closed. Summary and reasoning-content part indices
must be contiguous from zero at item completion, so a sparse stream cannot
silently erase an empty boundary. A terminal response containing an output item
that was never completed through the item lifecycle is likewise rejected rather
than silently discarded. When a terminal `output` array is supplied, its length,
order, IDs, and complete item payloads must match the recorded
`output_item.done` items; explicit omission or conflicts fail closed.

The SSE event type—not `response.status`—is the terminal discriminator:

- `response.completed` requires the Codex-required response `id`. Optional
  status is accepted only when it is `completed`; usage is optional and maps to
  zero when absent.
- `response.incomplete` accepts absent status and maps
  `max_output_tokens` to Anthropic `max_tokens` and `content_filter` to
  `refusal`. Unknown or missing truncation reasons terminate with one
  Anthropic error rather than fabricating success. If status is present it must
  be `incomplete`.
- `response.failed` and top-level `error` terminate with one Anthropic error.
  Any repeated or later terminal event is ignored.

Created and terminal identities are non-empty and stable: completed and
incomplete ids must exactly match `response.created`; failed ids must match when
a created event preceded them. A canonical failed-only stream remains valid.
Usage omission or `null` is accepted. A present usage object must contain all
three required non-negative `i64` counters, `total_tokens` must equal input plus
output without overflow, and cached/reasoning details must be well typed,
non-negative, and no larger than their parent counters. Partial, wrong-typed,
negative, inconsistent, or overflowing usage produces one Anthropic error
instead of being coerced to zero. Optional `end_turn` is likewise accepted only
as a boolean or `null`.

Lifecycle state is bounded: at most 4,096 output items, 4,096 reasoning parts,
and 4,096 text parts are tracked. Stored item JSON, reasoning/text
reconciliation data, and queued function arguments share the existing 16 MiB
upstream-response budget. Exceeding a bound uses the same one-error terminal
cleanup instead of allowing an unfinished stream to grow memory without limit.

`response.completed` and `response.incomplete` cannot produce Anthropic success
while an added output item, function call, or reasoning buffer is unfinished.
The bridge closes open blocks, clears lifecycle state, emits exactly one native
Anthropic terminal error, and ignores all later provider events. Exact public
evidence is in
`claude_reasoning_content_deltas_cross_public_stream_losslessly`,
`claude_reasoning_lifecycle_replays_and_adjacent_variants_are_deterministic`,
`claude_standalone_done_items_render_complete_text_and_function_calls`,
`claude_completed_terminals_follow_codex_event_discriminator`,
`claude_incomplete_terminals_preserve_truncation_semantics_without_status`,
`claude_failed_and_error_terminals_suppress_all_later_terminals`,
`claude_model_less_created_uses_resolved_model_context`,
`claude_created_and_terminal_identity_fields_fail_closed`,
`claude_usage_contract_preserves_valid_details_and_omission`,
`claude_malformed_usage_never_coerces_to_success`,
and
`claude_incomplete_or_out_of_order_response_items_fail_once_without_success`,
in addition to the JSON/SSE framing regressions.

The `U+2063` boundary follows the
[audited TypeScript reference](https://github.com/caozhiyuan/copilot-api/blob/cd8207cb70ede07771bf37a04accfbf2af76d980/src/routes/messages/responses-translation.ts#L70-L75).
This proxy
intentionally does **not** apply that reference's final `.trim()`, because
preserving valid reasoning text and making JSON/SSE output identical is safer
for continuation.

To route Codex through a configured OpenAI Responses provider, set the model to
`provider/model`, or map a friendly model name in `config.json`:

```json
{
  "modelMappings": {
    "coding-default": "openai-prod/gpt-5.4"
  }
}
```

Mappings are resolved before provider parsing by Messages, Chat Completions,
Responses, compaction, and model discovery. Configured provider models are
advertised as `provider/model` records.

## Verified feature and transport matrix

Status means deterministic, credential-free evidence exists.

| Contract | Claude Code 2.1.207 | Codex CLI 0.144.1 | Evidence |
|---|---|---|---|
| Native public protocol | Anthropic Messages JSON/SSE | OpenAI Responses JSON/SSE | `client_compatibility` positive boundary tests |
| Streaming and non-streaming | Supported | Supported | fixture captures and native response assertions |
| Instructions/system variants | string and structured system blocks | `instructions` plus input messages | request captures |
| Text and structured input | Messages content blocks | string/array input; images with optional `detail` | typed audit and provider-boundary captures |
| Tool definitions/results | tool use/result, multi-turn | function/custom/tool-search calls and outputs, including optional IDs | optional-item boundary audit |
| Parallel/interleaved calls | serialized only where Anthropic requires it | native interleaved Responses events | stream ordering/ID assertions |
| Prompt caching | `cache_control` and beta headers | `prompt_cache_key`, cached usage | boundary capture and usage assertions |
| Thinking/reasoning | exact optional carriers; lossless summary/content whitespace and `U+2063\n\n` part boundaries; carrier-aware empty placeholders; fail-closed SSE lifecycle | reasoning items with every optional ID/encrypted-content combination and 0.144.1 summary/content events | public request-carrier, framing, content-delta, replay, and incomplete/out-of-order regressions |
| Usage | strict nonnegative input/output/cache mapping; malformed Responses usage errors once | OpenAI cached/reasoning token details | public valid/absent/partial/type/range/overflow usage fixtures |
| Model routing | aliases, `[1m]`, provider models; model-less created events use validated request context | aliases and `provider/model` | model helpers and public model-less-created boundary tests |
| Unknown fields | retained in known top-level/items | retained in typed items; uninspected variants raw-preserved | captured extension sentinels and complete `ResponseItem` audit |
| Cancellation | response-body drop releases admission/upstream resources | same | load-shedding and WebSocket cancellation tests |
| Truncation | status-optional incomplete terminals map known reasons to `max_tokens`/`refusal`; unknown reasons error once | `response.incomplete` remains terminal, never completed | public statusless terminal and failure regressions |
| Compaction | Messages carriers round-trip with or without an item `id` | unary compact output without `id`, then successful continuation | non-stream/stream carrier tests and compact-to-next-turn boundary regression |
| Chat Completions | translation fallback retained | not Codex 0.144.1's wire API | existing Chat Completions suite |
| Public Responses WebSocket | not applicable | **Unsupported**; use HTTP SSE | intentional scope limit |

The detailed Claude-specific matrix remains in
[`claude-code-api-compatibility.md`](./claude-code-api-compatibility.md).

## Failure contract

The public API chooses its error envelope by route, including failures raised by
authentication, body limits, admission, and JSON parsing:

| Failure | `/v1/messages` | `/v1/responses` and compact |
|---|---|---|
| malformed/invalid request | Anthropic `type:error` + `invalid_request_error` | OpenAI `{error:{message,type,param,code}}` |
| bad/missing client key | Anthropic `authentication_error` | OpenAI `authentication_error` / `invalid_api_key` |
| request too large | Anthropic `request_too_large` | OpenAI `request_too_large` code |
| rate limit/overload | Anthropic retryable error + `Retry-After` | OpenAI rate/server error + retry metadata |
| upstream 4xx/5xx | status preserved, sanitized native envelope | status preserved, sanitized native envelope |
| malformed stream frame | one terminal Anthropic `error`; no `message_stop` | one terminal OpenAI `error` or `response.failed`; no completion |
| premature EOF/transport reset | one retryable terminal error | one terminal OpenAI error; no completion |
| incomplete/conflicting output-item lifecycle | one terminal Anthropic `error`; open blocks close; later events ignored | native Responses stream is forwarded unchanged |
| empty/mismatched response id or malformed usage | one terminal Anthropic `error`; no success terminal | forwarded unchanged |
| statusless `response.completed` | one `end_turn`/`tool_use` terminal; usage optional | forwarded unchanged |
| `response.incomplete` | known truncation reason becomes one Anthropic terminal; unknown reason errors once | forwarded once as terminal incomplete |
| `response.failed` / `error` | one Anthropic error; later terminals ignored | forwarded in native Responses shape |

Correlation headers are allowlisted, and every router response also carries the
gateway `x-trace-id`. Arbitrary upstream `x-*` headers and internal diagnostics
are not exposed.

## Intentional differences from the TypeScript reference

The TypeScript reference remains useful behavior evidence, not the objective.
This implementation intentionally differs where client correctness is stronger:

1. Native Responses/Chat errors are OpenAI-shaped. They never acquire the
   Anthropic top-level discriminator used by Messages.
2. Responses SSE is lifecycle-validated. Malformed JSON, mismatched event names,
   output before `response.created`, `[DONE]` without a terminal event, and
   premature EOF cannot be mistaken for success.
3. A valid terminal Responses event is forwarded exactly once and polling stops;
   later upstream frames cannot append a second terminal event.
4. `/v1/responses/compact` is implemented for audited Codex remote compaction.
   The TypeScript reference at the audited commit does not expose this route.
5. Non-streaming upstream bodies and SSE records remain size-bounded.
6. Client authorization is never forwarded to a configured provider. Only safe
   Anthropic or Codex metadata is copied, then provider auth replaces it.

## Troubleshooting

- **401 from the gateway:** set the client token to an exact `auth.apiKeys`
  entry. This is not the upstream provider key.
- **Model/provider not found:** check `/v1/models`, the provider name, its
  `enabled` flag, and `modelMappings`. No silent fallback is performed.
- **Codex tries Chat Completions:** set `wire_api = "responses"`; `chat` is
  rejected by Codex 0.144.1 itself.
- **Codex receives 404 on compaction:** use this version's
  `/v1/responses/compact` route and keep `/v1` in `base_url`.
- **Stream ends with a native error:** inspect `x-trace-id`; malformed frames and
  premature EOF are intentionally not converted to successful completions.
- **413 request or 502 upstream response:** reduce the request/fixture response;
  the gateway enforces bounded admission and response buffering.
- **Local provider fixture is blocked:** production defaults reject private
  provider destinations. `COPILOT_API_ALLOW_PRIVATE_PROVIDERS=1` is only for
  deliberate local development/testing.

## Optional installed-client canary

Normal CI does not execute installed CLIs. A loopback-only Codex canary is
available when **exactly Codex 0.144.1** is installed:

```sh
cargo test --test client_compatibility installed_codex_cli_smoke \
  -- --ignored --nocapture
```

It uses an isolated `CODEX_HOME`, a fake key, an ephemeral public proxy listener,
and an ephemeral upstream fixture. It does not use port 4141 or an external
provider. Claude Code's long-lived process/environment behavior makes an equally
strong opt-in binary canary less deterministic; the credential-free Axum
boundary harness is the maintained Claude evidence.
