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
`claude_aggregate_empty_reasoning_*` regressions cover required empty summary
arrays, empty text, and whitespace-only text. They emit the standard thinking
placeholder plus the exact opaque signature only when an ID or encrypted
content field is present (including an explicitly empty string); carrier-free
aggregate-empty reasoning emits no fabricated
Anthropic thinking/signature data.

### Anthropic request validation policy

Known Messages request collections and objects are validated before admission or
provider dispatch. Unknown keys on open objects remain intact; malformed known
fields never become omitted/defaulted values through `as_*`, `filter_map`, or
empty-string fallbacks. A future content-block variant can still pass through a
native Messages transport, but a Responses translation that cannot represent it
fails explicitly instead of dropping it.

The web-search policy follows Anthropic's
[`web_search_20250305` definition](https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool#tool-definition):

- `allowed_domains` and `blocked_domains` may be absent, `null`, or arrays of
  non-blank strings. Empty arrays mean no restriction and are omitted from the
  Responses tool. Non-empty values retain exact order and duplicates. Both lists
  cannot be non-empty. The source contract publishes no narrower count/string
  limit, so the normal 32 MiB request-body bound remains the size ceiling.
- `user_location` may be absent/null or an object. Its required `type` is
  `approximate`; at least one non-blank `city`, `region`, two-letter `country`,
  or `timezone` is required. Known fields are string/null only; unknown object
  keys are preserved.
- `max_uses` is positive when present (the bridge performs one search).
  `allowed_callers` and `response_inclusion` cannot be represented by this
  Responses bridge and therefore fail explicitly rather than being discarded.

Custom/deferred tools require a non-empty unique name and an object or boolean
`input_schema`. Schema validation is structural rather than a full metaschema
evaluation: every schema node is object/boolean, and known recursive containers
are traversed (`properties`, `patternProperties`, `$defs`/`definitions`,
`items`/`prefixItems`, additional/unevaluated properties, combinators,
conditionals, dependencies, `contains`, `propertyNames`, and required-name
arrays). Unknown keywords and values remain byte-semantically unchanged.
Validation is bounded to depth 64, 4,096 schema nodes, and 4,096 entries per
known collection. An object schema with `type: "object"` and no `properties`
retains the established empty-properties normalization.
The known `type` keyword accepts only `null`, `boolean`, `object`, `array`,
`number`, `string`, or `integer`; arrays must be non-empty, unique subsets of
those values. `required`, `dependentRequired`, and legacy name-array
dependencies contain unique non-blank property names. Empty name arrays are
valid no-op constraints: the JSON Schema
[`stringArray` meta-schema](https://github.com/json-schema-org/json-schema-spec/blob/fb2a20df8ea471e8754e32205ea372939b5527ca/specs/meta/meta.schema.json)
has `default: []`, `uniqueItems: true`, and no `minItems`. `enum`, `const`, and
unknown keywords are not semantically interpreted and remain unchanged.

An explicit `tool_choice` of type `tool` must name exactly one declared
non-deferred function. Server tools and deferred functions cannot be selected as
ordinary functions. The sole exception is the declared tool-search bridge: it
maps deliberately to Responses `auto` only when the resolved model supports tool
search and the catalog contains a real `defer_loading: true` tool. `auto`, `any`,
and `none` accept no `name`; duplicate catalog names fail before choice
resolution.

Open-object extensions use a per-target policy rather than blanket dropping:

- Benign unknown keys on function tools, deferred namespaces, the tool-search
  bridge, and web-search tools are appended to the corresponding open Responses
  object in source order. `strict` maps to the canonical function field.
- Object-form function choices preserve unknown keys after canonical `type` and
  `name`. Scalar `auto`/`required`/`none` choices and the bridge-to-`auto`
  exception cannot represent extensions and reject them explicitly.
- Extensions that collide with a canonical target key (`parameters`, namespace
  `tools`, bridge `execution`, web `filters`, and analogous content/item keys)
  fail with a path-specific Anthropic 400 before provider dispatch.
- On Responses, metadata extras serialize directly. Representable text, media,
  tool-use/result, thinking, source, thinking-config, and output-config extras
  are retained on their open target objects; unrepresentable structured-system
  extras fail explicitly. Large base64 data is not duplicated.

Top-level `AnthropicMessagesPayload.extra` fields are appended to the open
Responses request after canonical fields, and `AnthropicInputMessage.extra`
fields are copied onto every produced Responses message item—even when a
tool-use/result splits one Anthropic message into multiple message items.
An extension on a tool-call/result-only message is rejected because moving it
onto the non-message item would change its scope.
Canonical request/message collisions (`input`, `phase`, `status`, and similar)
and the unsafe `stop` bypass fail explicitly before provider dispatch.

The supported Messages-to-Chat bridge follows the same preserve-or-reject rule:

- Top-level request extensions append after canonical Chat controls without
  overriding `model`, `messages`, token/stream controls, tools, choice, cache,
  or reasoning fields. Nested values, explicit nulls, and source key order stay
  intact.
- A user/assistant wrapper extension is attached exactly once to its
  corresponding Chat message. In a mixed tool-result/user turn it belongs only
  to the ordinary user message, never the generated tool message. A
  tool-result-only wrapper fails unless exactly one rich-content fallback
  creates an unambiguous moved user carrier.
- Extension-bearing structured system text stays structured as Chat text
  content instead of being flattened. Ordinary structured system text without
  extensions retains the established joined-string behavior. Text/image/file
  parts, tool calls/results, custom tools, and object-form choices retain
  representable extensions on their corresponding open objects.
- Scalar tool choices, deferred/server-tool controls, scalar tool-content
  fallbacks, non-PDF document fallbacks, thinking-block extras, and nested
  metadata/thinking/output-config extras have no lossless Chat target and
  return a path-specific Anthropic 400. Canonical request/message/content/tool
  collisions fail before upstream dispatch. Provider-added cache markers never
  overwrite an existing client cache-control object.

The reverse non-streaming Chat boundary is fail-closed. A successful response
must be a `chat.completion` object with non-empty `id`/`model`, one choice at
index zero, an assistant message, a supported finish reason, representable
content, valid function tool calls with object-valued JSON arguments, and a
strict usage object whose non-negative counters and totals agree. Extra choices,
legacy function calls, unsupported structured parts, conflicting reasoning,
malformed details, negative/overflowing counters, and empty fabricated output
become one sanitized Anthropic `502 api_error`. A top-level Chat `error` in an
HTTP-200 body follows the same path. Real upstream status codes remain intact;
allowlisted request IDs and retry/rate-limit headers survive, while unsafe
headers and malformed bodies do not.

Representable reverse fields remain lossless: top-level extras flatten into
`AnthropicResponse.extra`; choice and assistant-message extras use ordered
`chat_choice_extensions` / `chat_message_extensions` objects; structured text
extras remain on text blocks; usage and prompt-detail extras remain in
`AnthropicUsage.extra`; and tool-call/function extras remain on the generated
`tool_use` block (function scope uses `chat_function_extensions`). Canonical
target collisions fail rather than overriding response authority. Explicit
nulls, nested values, and source order are retained.

`stop_sequences` has no field in either the audited Codex 0.144.1
[`ResponsesApiRequest`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/codex-api/src/common.rs)
or the current generated OpenAI
[`ResponseCreateParamsBase`](https://github.com/openai/openai-python/blob/f16fbbd2bd25dc1ff150b5f78dbd15ff6bab6d91/src/openai/types/responses/response_create_params.py).
Non-empty sequences therefore return Anthropic HTTP 400 before a selected
Responses provider or web-search bridge consumes admission. Omitted, `null`,
and empty arrays are no-op. Native Anthropic forwarding preserves the original
array, and the Chat Completions translator maps it exactly to `stop`, retaining
order and duplicates.

The same route-specific policy covers other top-level controls. Generic and
direct-Copilot Responses preserve explicit `temperature`/`top_p`
(`temperature` defaults to the established `1` only when omitted/null), while
the audited Codex request has neither field and rejects explicit values before
admission. Non-null `top_k`, top-level `cache_control`, and Anthropic
`service_tier` fail before any Responses provider dispatch because they cannot
be carried unchanged by every supported Responses transport. Native Anthropic
forwarding retains those controls. Null/omitted values remain no-op. Anthropic
`max_tokens` remains required at the public Messages boundary, but Codex has no
output-token-limit member; its value is therefore validation/translation input,
not a Codex wire constraint.

System blocks, metadata, thinking/output configuration, stop sequences, cache
controls, text/image/document sources, tool-use inputs, and tool-result content
all validate their known container and scalar types before translation. Image
and document sources support only the bridge's `base64` and `url` forms;
`file` and future source types fail before admission. Unknown fields inside a
supported source object are retained in
`anthropic_source_extensions` without duplicating large base64 data.

Anthropic
[`tool_reference`](https://github.com/anthropics/anthropic-sdk-python/blob/d2f6543ee7995adcae74666a5d37b3d9743debfe/src/anthropic/types/tool_reference_block_param.py)
blocks are valid only inside a `tool_result`; `type` and a non-blank
`tool_name` are required, and the name must identify a supplied tool with
`defer_loading: true`. Omitted tool-result content and an empty block list mean
that no tools were loaded. Explicit references and validated internal sentinel
name arrays preserve order and duplicates; unknown block/schema keys remain
allowed. Exact public evidence is in
`claude_web_search_request_policy_rejects_malformed_before_dispatch`,
`claude_web_search_request_policy_preserves_valid_empty_duplicate_and_unknown_values`,
`claude_known_request_collections_fail_closed_before_provider_dispatch`,
`claude_deferred_tool_references_reject_malformed_collections_before_dispatch`,
`claude_deferred_tool_empty_duplicate_and_unknown_extensions_are_explicit`,
`claude_tool_choice_must_resolve_to_one_compatible_declared_tool`,
`claude_recursive_schema_shape_and_bounds_fail_before_dispatch`,
`claude_complex_boolean_schemas_choices_and_sources_preserve_supported_shape`,
`claude_open_object_extension_collisions_fail_before_provider_dispatch`,
`claude_payload_and_message_extensions_survive_split_responses_translation`,
`claude_payload_and_message_extension_collisions_fail_without_dispatch`,
`claude_chat_extensions_preserve_scope_nulls_order_and_split_messages`,
`claude_direct_chat_preprocessing_keeps_split_message_extension_carrier`,
`claude_chat_extensions_reject_collisions_and_unrepresentable_scopes`,
`claude_chat_response_extensions_and_usage_survive_provider_boundary`,
`claude_chat_malformed_provider_responses_fail_as_sanitized_bad_gateway`,
`claude_chat_upstream_status_and_direct_failures_preserve_safe_semantics`,
`claude_stop_sequences_reject_responses_but_preserve_native_anthropic_support`,
`claude_responses_controls_preserve_supported_and_reject_unrepresentable_values`,
and `claude_unsupported_source_types_fail_before_admission_or_dispatch`.

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
`message_start` when model is absent or explicitly `null`; a non-empty upstream
model, when present, is retained instead. For created/completed/incomplete/
failed snapshots, absent and explicit-null unmodeled fields follow the same
Codex serde semantics. Status is checked only when it is a non-null string:
matching lifecycle values are accepted, while wrong strings or non-string,
non-null values fail. Fields the audited client and bridge do not consume remain
raw and are not type-gated.

Codex's typed
[`ResponseCompletedUsage`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/codex-api/src/sse/responses.rs#L122-L157)
requires integer `input_tokens`, `output_tokens`, and `total_tokens` whenever
usage is present. Cached-input and reasoning-output detail objects are optional,
but their integer fields are required when those objects exist.

Handled JSON and stream items share one typed validator against Codex's tagged
[`ResponseItem`](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/protocol/src/models.rs#L932-L1163)
contract. A malformed complete JSON result fails before translation; a malformed
SSE item closes any open block and emits exactly one Anthropic error:

- `function_call` requires non-empty string `call_id` and `name`, plus string
  `arguments`. Empty arguments are valid only on the initial added item;
  authoritative done values must contain a JSON object. Argument deltas, the
  optional item/call IDs, `arguments.done`, and final item values must reconcile.
  Duplicate done events, late deltas, invalid JSON, or identity/content conflicts
  fail once.
- `custom_tool_call` requires `call_id`, `name`, and string `input`, and maps to
  the same `{"input": ...}` Anthropic tool input in JSON and SSE.
- `tool_search_call` requires non-empty string `execution` and a present JSON
  `arguments` value; `id`, `call_id`, and status remain optional string/null
  fields, with a non-empty value required when `call_id` is present. When
  `call_id` is absent/null on the added item, SSE emission waits for
  `output_item.done`: a late call ID wins in both transports, otherwise the item
  ID is used, and finally `tool_call_<output_index>` is generated when both are
  absent. A present call ID cannot change or disappear. `tool_search_output`
  requires status, execution, and a tools array.
- Streamed messages require assistant role and a content array containing
  audited text variants with correctly typed required fields. Unsupported
  assistant output variants fail explicitly rather than disappearing. Optional
  annotations must be arrays of objects. Streamed content indices and
  authoritative completed text must reconcile.
- Reasoning requires a summary array with typed `summary_text` blocks. Optional
  reasoning content and encrypted carriers remain optional, but malformed
  present values fail; summary/content deltas must be prefixes of their
  authoritative values.
- Compaction requires non-empty string `encrypted_content`; its ID remains
  optional. Optional item IDs, statuses, namespaces, and internal passthrough
  metadata are accepted only with their source-defined string/null or object
  shapes.

Complete result identity, model, terminal status, item IDs, tool-call IDs, item
statuses, and usage counters are also validated. Duplicate identities or an
incomplete item inside a completed response cannot become an empty `end_turn`.
Client-ignored complete-response extras remain raw rather than being coerced or
type-gated.

Codex raw output variants have an explicit transport classification:

- Native `/v1/responses` JSON/SSE preserves `additional_tools`,
  `agent_message`, `local_shell_call`, function/custom-tool outputs,
  `web_search_call`, `image_generation_call`, `context_compaction`,
  `compaction_trigger`, and future unknown variants byte-semantically unchanged.
- Generic Responses-to-Anthropic Messages translation cannot preserve their
  author/recipient, executed-tool, image, compaction-control, or future semantics,
  so JSON and SSE fail once with an unsupported-output error rather than
  returning an empty `end_turn`.
- The dedicated Messages web-search flow is the sole exception:
  one completed (or status-omitted/null) `web_search_call` with exactly one
  non-empty search query maps losslessly to `server_tool_use`, retaining its
  source ID, followed by the reconstructed `web_search_tool_result` and cited
  text. Other raw variants, incomplete calls, multiple/empty queries, multiple
  calls, or unrepresentable web actions fail before JSON or synthetic-SSE
  success.

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
If `response.created` carries output, it must match the lifecycle's normalized
added or done snapshot; non-empty created-only output cannot be silently turned
into `end_turn`. Optional null item fields normalize to omission, while tool
arguments and other semantic payload data remain exact.

When provider events include OpenAI `sequence_number`, it must increase
monotonically; an immediately replayed identical frame is ignored, while reused
or decreasing numbers fail closed. Summary and reasoning-content part indices
must be contiguous from zero at item completion, so a sparse stream cannot
silently erase an empty boundary. Completed output-item indices must likewise be
contiguous from zero. A terminal response containing an output item that was
never completed through the item lifecycle is rejected rather than silently
discarded. When a terminal `output` array is supplied, its length, order, IDs,
and complete item payloads must match the recorded `output_item.done` items;
explicit omission or conflicts fail closed.

The SSE event type—not `response.status`—is the terminal discriminator:

- `response.completed` requires the Codex-required response `id`. Optional
  status may be absent, `null`, or `completed`; usage is optional and maps to
  zero when absent/null. `end_turn: false` maps to Anthropic `pause_turn`
  ([Codex requests follow-up inference](https://github.com/openai/codex/blob/44918ea10c0f99151c6710411b4322c2f5c96bea/codex-rs/core/src/session/turn.rs#L2288-L2303)),
  while true/absent values retain normal `end_turn` or `tool_use` behavior.
- `response.incomplete` accepts absent/null status and maps
  `max_output_tokens` to Anthropic `max_tokens` and `content_filter` to
  `refusal`. Unknown or missing truncation reasons terminate with one
  Anthropic error rather than fabricating success. A non-null status must be
  `incomplete`.
- `response.failed` accepts absent/null or matching status and, like top-level
  `error`, terminates with one Anthropic error.
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

The web-search collector reconciles three snapshots before replying: the full
`response.created`, optional item lifecycle, and partial terminal. Missing/null
model, object, output, output text, usage, and metadata are filled without
overwriting existing values. Repeated model/object/output-text/metadata must
match; required usage counters agree while optional cached/reasoning assertions
merge field-by-field. Non-empty created output must semantically match final
output, or match the lifecycle's added snapshot while the terminal matches its
authoritative done snapshot. Terminal and lifecycle output must agree under the
same canonical form. Created-only,
terminal-only, matching duplicate, and lifecycle-derived output are supported;
well-typed conflicts fail. Failed/incomplete terminals, malformed JSON, and any
event after a terminal fail before JSON or synthetic Anthropic SSE success.
Output comparison canonicalizes only reconstruction semantics: optional null and
absence are equivalent, ignored item/annotation extensions do not create false
conflicts, citation URL/title and ordering remain authoritative, and cached
plus reasoning usage details are compared. A representable web-search call keeps
its original Responses item ID as the Anthropic server-tool ID.

### Web-search field authority

The collector uses the following explicit authority model; later partial
snapshots never overwrite an asserted earlier value silently:

| Response field | Authority / merge rule |
|---|---|
| `id` | Required in created and terminal; present values must match. |
| `model` | Non-null source values must match. The validated requested/resolved model is used only when every source snapshot omits/nulls it. |
| `object` | Optional assertion; absent/null makes no assertion, conflicting present values fail. |
| `status` | Phase discriminator: created allows absent/null/`in_progress`; terminal allows absent/null or its event-matching value. Final status comes from the terminal event type. |
| `output` | Structured merge across created, lifecycle-added, lifecycle-done, and terminal snapshots using the nested item rules below. Created `[]` is provisional; terminal `[]` is authoritative. |
| `output_text` | Optional assertion; one present value is retained, matching values merge, conflicts fail. |
| `usage` | A present object requires valid input/output/total counters. Required counters must match across snapshots. Optional cached/reasoning details merge independently when present; omission/null is no assertion, and contradictory present values fail. |
| `metadata`, `incomplete_details` | Optional structured assertions. Present values must be objects; missing/null values are filled from another snapshot, matching objects merge, and deep conflicting present objects fail. Scalars/arrays fail before success. |
| `end_turn` | Optional boolean assertion. Missing/null is no assertion; conflicts fail; `false` maps to `pause_turn`. |
| `error` | Used only to report failed terminals; never merged into successful reconstruction. |
| `created_at`, `instructions`, `parallel_tool_calls`, `temperature`, `tool_choice`, `tools`, `top_p`, unknown fields | Client/bridge-ignored raw extras. They are retained from created or filled when missing, but are not conflict/type gates. |

Public fixtures cover created-only, terminal-only, matching, null/absent,
conflicting, scalar, and array cases for both structured
`metadata` and `incomplete_details`. Ignored raw fields are deliberately tested
with conflicting values to prove they are not accidental semantic gates.

Nested output authority is likewise field-specific:

| Item/field | Authority / merge rule |
|---|---|
| Every item `type` | Required and stable by output index. |
| Message / web-search `id` | Optional stable assertion. A known ID survives omission/null in any other partial snapshot; conflicting present IDs fail. |
| Item `status` | Progressive assertion. `in_progress` may advance to `completed`/`incomplete`; a done snapshot may omit status; contradictory present terminal states fail. |
| Message `role` | Required and stable. |
| Message `content` | Added snapshots may be empty/partial; done/terminal text must converge. Content type/text conflicts fail. |
| Text `annotations` | Canonical optional assertion. Missing/null/empty/unknown-only arrays make no assertion. Mixed arrays compare only known `url_citation` URL/title semantics in source order; absent/null title deterministically defaults to the URL. Duplicate URLs and unknown extensions are ignored. Matching known citations merge; conflicting known citations fail. A non-array field, non-object entry, wrong-typed annotation discriminator, or malformed known URL/title fails. |
| Web-search `action` | Optional in partial item snapshots. Exactly one non-empty final search query is required; matching `query`/single `queries` forms merge, conflicts fail. |
| Message `phase`, internal passthrough metadata, unknown item extensions | Ignored by web reconstruction and excluded consistently from semantic equality. |

The annotation matrix covers both created/terminal directions, lifecycle
added/done snapshots, empty/null/absent/unknown-only arrays, mixed known and
unknown values, deterministic title/default/deduplication behavior, conflicts,
and malformed field/entry/known-citation shapes through both Anthropic JSON and
synthetic SSE. The remaining optional collections were audited: response output
and progressive message content deliberately give empty arrays phase-specific
authority. A present web-search action must still resolve to exactly one query
(`queries: []` may use a non-empty `query` fallback); it is not an optional
canonical collection. Annotations are the only canonical optional collection
where empty is equivalent to no assertion.

Native non-stream `/v1/responses` and `/v1/responses/compact` do not serialize
their validation model back to the client. Bodies remain size-bounded and valid
JSON is returned in its original bytes, preserving explicit nulls, unknown
fields, whitespace, and key order. Direct regular Responses validates the full
result contract. Direct Copilot, Codex-provider, and generic-provider compact
routing all use the same shared reader and output-only contract. Native SSE
remains raw event forwarding under the existing lifecycle guard.

Buffered validation contracts:

| Endpoint | Buffered validation |
|---|---|
| Direct `/v1/responses` | Full Responses result: required identity/model/status/output, strict known output item fields, and internally consistent nonnegative usage. |
| `/v1/responses/compact` (direct and provider) | Shared compact output-only result: required output array, source-valid ResponseItem shapes (including id-less `compaction`), optional strict usage, and arbitrary extensions; no fabricated response ID/model/status requirement. |

Malformed JSON, wrong known compact output/item shapes, malformed, inconsistent,
negative, or overflowing usage, and size-limit failures from an otherwise
successful direct/provider compact response become sanitized OpenAI
`502 Bad Gateway` errors. Real upstream 4xx/5xx statuses, OpenAI error bodies,
`Retry-After`, and allowlisted request IDs are retained. Successful direct
regular and all compact bodies preserve original bytes and only
allowlisted `x-request-id`, `openai-request-id`, and `x-codex-turn-state`
headers; arbitrary upstream headers are not exposed.
Public fixtures exercise the actual 16 MiB response bound. Direct and provider
latency use `copilot_upstream_request_seconds` and
`provider_upstream_request_seconds`, respectively, with only bounded
`endpoint=responses_compact` and coarse status labels; configured provider
aliases are never metric labels. Successful compact usage is recorded under the
`responses_compact` endpoint with `copilot` or `provider` source attribution;
malformed/oversized bodies are rejected before usage is recorded.

`response.completed` and `response.incomplete` cannot produce Anthropic success
while an added output item, function call, or reasoning buffer is unfinished.
The bridge closes open blocks, clears lifecycle state, emits exactly one native
Anthropic terminal error, and ignores all later provider events. Exact public
evidence is in
`claude_reasoning_content_deltas_cross_public_stream_losslessly`,
`claude_reasoning_lifecycle_replays_and_adjacent_variants_are_deterministic`,
`claude_standalone_done_items_render_complete_text_and_function_calls`,
`claude_completed_terminals_follow_codex_event_discriminator`,
`claude_completed_end_turn_false_maps_to_pause_turn`,
`claude_incomplete_terminals_preserve_truncation_semantics_without_status`,
`claude_failed_and_error_terminals_suppress_all_later_terminals`,
`claude_model_less_created_uses_resolved_model_context`,
`claude_created_output_requires_matching_rendered_lifecycle`,
`claude_created_and_terminal_identity_fields_fail_closed`,
`claude_usage_contract_preserves_valid_details_and_omission`,
`claude_malformed_usage_never_coerces_to_success`,
`claude_handled_scalar_families_accept_source_valid_shapes`,
`claude_malformed_handled_scalars_fail_once_without_empty_blocks`,
`claude_json_and_sse_outputs_match_for_valid_families`,
`claude_json_and_sse_reject_equivalent_malformed_outputs`,
`claude_tool_search_identity_merges_omission_and_rejects_conflicts`,
`claude_web_search_partial_terminals_preserve_output_in_json_and_sse`,
`claude_web_search_lifecycle_optionals_merge_in_both_directions`,
`claude_web_search_end_turn_assertions_merge_in_both_directions`,
`claude_web_search_terminal_conflicts_fail_before_json_or_sse_success`,
`claude_raw_output_variants_fail_explicitly_in_json_and_sse`,
`native_responses_preserves_all_raw_output_variants`,
`native_nonstream_responses_preserves_exact_null_and_unknown_shape`,
`direct_copilot_compact_preserves_output_only_contract_and_continuation`,
`direct_copilot_compact_failures_use_native_bad_gateway_semantics`,
`direct_copilot_regular_responses_preserve_bytes_headers_and_errors`,
`provider_compact_uses_shared_output_contract_and_records_usage`,
`provider_compact_failures_match_direct_native_semantics`,
`claude_web_search_annotations_canonicalize_across_all_snapshots`,
`claude_web_search_malformed_or_conflicting_annotations_fail_once`,
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
| Tool definitions/results | tool use/result, multi-turn; explicit choices bind to one compatible catalog entry; recursive object/boolean schemas are shape/bound validated; deferred references preserve explicit order/duplicates; malformed definitions, identities, arguments, and result collections fail before dispatch | function/custom/tool-search calls and outputs, including optional IDs | catalog choice, recursive schema, request collection, deferred reference, optional-item, and paired JSON/SSE boundary audits |
| Parallel/interleaved calls | serialized only where Anthropic requires it | native interleaved Responses events | stream ordering/ID assertions |
| Prompt caching | `cache_control` and beta headers on native transports; top-level cache control is rejected for translated Responses/Chat transports | `prompt_cache_key`, cached usage | boundary capture and usage assertions |
| Thinking/reasoning | exact optional carriers; lossless summary/content whitespace and `U+2063\n\n` part boundaries; carrier-aware empty placeholders; fail-closed SSE lifecycle | reasoning items with every optional ID/encrypted-content combination and 0.144.1 summary/content events | public request-carrier, framing, content-delta, replay, and incomplete/out-of-order regressions |
| Usage | strict nonnegative input/output/cache mapping; malformed Responses usage errors once | OpenAI cached/reasoning token details | public valid/absent/partial/type/range/overflow usage fixtures |
| Model routing | aliases, `[1m]`, provider models; model-less created events use validated request context | aliases and `provider/model` | model helpers and public model-less-created boundary tests |
| Unknown fields/output variants | representable open-object extensions retain value/order; canonical collisions and scalar-target extensions fail explicitly; unsupported raw outputs fail explicitly | typed extensions and all raw variants preserved natively | captured tool/choice/content/config sentinels, collision fixtures, paired raw-variant failures, and native passthrough audit |
| Cancellation | response-body drop releases admission/upstream resources | same | load-shedding and WebSocket cancellation tests |
| Truncation | status-optional incomplete terminals map known reasons to `max_tokens`/`refusal`; unknown reasons error once | `response.incomplete` remains terminal, never completed | public statusless terminal and failure regressions |
| Compaction | Messages carriers round-trip with or without an item `id` | unary compact output without `id`, then successful continuation | non-stream/stream carrier tests and compact-to-next-turn boundary regression |
| Web search | validated domain/location policy, native server-tool/result blocks in JSON and synthetic SSE; partial terminals reconcile without output loss | native Responses web-search output | request-policy, paired partial/terminal-only output, and conflict fixtures |
| Chat Completions | lossless representable request/response extensions; explicit request 400 or malformed-upstream 502; strict one-choice/tool/usage validation | not Codex 0.144.1's wire API | public provider/direct captures, split-carrier, malformed-body, status/header, and collision fixtures |
| Public Responses WebSocket | not applicable | **Unsupported**; use HTTP SSE | intentional scope limit |

The detailed Claude-specific matrix remains in
[`claude-code-api-compatibility.md`](./claude-code-api-compatibility.md).

## Failure contract

The public API chooses its error envelope by route, including failures raised by
authentication, body limits, admission, and JSON parsing:

| Failure | `/v1/messages` | `/v1/responses` and compact |
|---|---|---|
| malformed/invalid request | Anthropic `type:error` + `invalid_request_error` | OpenAI `{error:{message,type,param,code}}` |
| malformed known request collection/object | Anthropic HTTP 400 before admission/provider dispatch; no partial policy/tool forwarding | route-specific OpenAI validation error |
| bad/missing client key | Anthropic `authentication_error` | OpenAI `authentication_error` / `invalid_api_key` |
| request too large | Anthropic `request_too_large` | OpenAI `request_too_large` code |
| rate limit/overload | Anthropic retryable error + `Retry-After` | OpenAI rate/server error + retry metadata |
| upstream 4xx/5xx | status preserved, sanitized native envelope | status preserved, sanitized native envelope |
| malformed/oversized successful upstream JSON | sanitized Anthropic `502 api_error`; no fabricated success | sanitized OpenAI `502 server_error`; no upstream body leakage |
| malformed stream frame | one terminal Anthropic `error`; no `message_stop` | one terminal OpenAI `error` or `response.failed`; no completion |
| premature EOF/transport reset | one retryable terminal error | one terminal OpenAI error; no completion |
| incomplete/conflicting output-item lifecycle | one terminal Anthropic `error`; open blocks close; later events ignored | native Responses stream is forwarded unchanged |
| malformed handled item/event scalar | JSON fails before `end_turn`; SSE emits one terminal Anthropic `error`; no empty block or success terminal | forwarded unchanged |
| raw/unrepresentable output variant | one explicit unsupported-output error in JSON/SSE; dedicated single-query web search is mapped | forwarded unchanged in native JSON/SSE |
| conflicting/malformed web-search terminal | Anthropic HTTP error before JSON or synthetic-SSE success | forwarded unchanged outside the Messages bridge |
| empty/mismatched response id or malformed usage | one terminal Anthropic `error`; no success terminal | forwarded unchanged |
| absent/null-status `response.completed` | one `end_turn`/`tool_use` terminal; usage optional | forwarded unchanged |
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
