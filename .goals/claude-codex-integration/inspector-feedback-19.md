# Inspector Feedback — Iteration 19

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, the accumulated Inspector feedback, current
  `status.json`, and the complete Builder commit
  `e97aa39210d88a31c08e0de261fc24f12a4cce9d`.
- Audited the new Chat request preserve-or-reject implementation, including
  top-level and message flattening, collision lists, structured system/content
  conversion, multimodal sources, tool and tool-choice conversion, metadata and
  thinking/output configuration policies, preprocessing, provider overrides,
  context-cache markers, and all Chat constructors.
- Verified installed versions remain Claude Code `2.1.207` and Codex CLI
  `0.144.1`.
- Ran the credential-free public Axum suite: 60 passed and 1 ignored. Ran the
  ignored installed Codex loopback canary explicitly: it passed.
- Ran all repository gates: formatting, Clippy with warnings denied, verbose
  build, verbose tests, and `cargo deny check` all passed. `cargo deny check`
  continues to print the repository's existing non-fatal unmatched-license and
  duplicate-dependency warnings.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The documentation now describes the Chat preserve-or-reject
  policy, route matrix, exact client versions, fixtures, and source-backed
  behavior. The new request-side claims are supported by the added captures and
  no-dispatch tests.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The new
  `claude_chat_extensions_preserve_scope_nulls_order_and_split_messages`,
  direct-Copilot Chat carrier test, and rejection test all cross
  `build_router()` with local deterministic fixtures, fake credentials, and no
  external or port-4141 traffic. The harness now exercises both provider and
  direct Chat routes.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The
  inbound Chat request side is substantially fixed:

  - top-level extensions are appended after canonical fields, retaining nested
    values, explicit nulls, and insertion order;
  - user and assistant extras survive normal messages and the tested
    tool-result/tool-use splits without moving onto generated tool messages;
  - structured system blocks stay structured when extensions/cache markers need
    them, while ordinary blocks retain the joined-string behavior;
  - text, image, document, tool-use/result, tool, choice, and generated
    reasoning fields either preserve representable extensions or return a
    path-specific 400;
  - metadata, thinking, output configuration, deferred/server controls, scalar
    fallbacks, and canonical collisions fail explicitly where Chat has no
    lossless target.

  Nevertheless, the Chat response translator still silently discards unknown
  upstream response fields and accepts malformed/wrong-shaped successful
  responses. `translate_to_anthropic` always constructs `AnthropicResponse`
  with an empty `extra`; it rebuilds text/tool blocks without their open
  fields; and it has no validation result for a missing or malformed `choices`.
  A valid JSON `{}` or a response with malformed tool-call arguments can become
  a successful Anthropic message instead of a protocol-native failure. This is
  still a supported Claude Messages transport and violates the goal's
  unknown-field and no-silent-fallback requirements.

- [ ] **Criterion 4 — Codex Responses and inbound translation.** The Codex
  Responses, compact, carrier, lifecycle, terminal, usage, and native
  noninterference evidence remains green. The required retained Chat surface is
  not fully compatible: malformed Chat JSON shapes and malformed function-call
  response scalars can still be turned into successful Anthropic output, and
  Chat response extensions are not preserved. This leaves the accumulated
  cross-transport compatibility goal incomplete even though the new inbound
  request policy passes.

- [ ] **Criterion 5 — native contracts, errors, and no silent loss.** Request
  failures now use native Anthropic 400s before provider dispatch for
  unrepresentable Chat extensions, and provider captures show the expected
  request shape. The response side remains a blocking violation:

  1. `translate_to_anthropic` uses `choices.unwrap_or(empty)` and returns a
     successful `AnthropicResponse` for a valid JSON object with no `choices`
     or with a wrong `choices` type.
  2. `get_anthropic_tool_use_blocks` defaults missing/non-string IDs, names, and
     arguments to empty strings and converts invalid JSON arguments to `{}`.
  3. `map_openai_chat_completion_usage` defaults malformed or wrong-typed usage
     counters to zero.
  4. The rebuilt response and content objects initialize their open `extra`
     maps empty, so upstream keys are silently lost.

  These paths can fabricate a successful Claude turn after malformed or
  incomplete Chat upstream data rather than emitting one sanitized,
  protocol-native error.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models, and
  aliases.** The inspected changes preserve provider-only routing, model
  endpoint selection, direct-Copilot Chat routing, API-key handling, and
  provider configuration. The public direct and configured-provider tests both
  reach the intended Chat fixture.

- [ ] **Criterion 7 — deterministic regression tests for every gap fixed and
  compatibility claim.** The new request tests are strong and cover nested
  nulls/order, normal and split user/assistant carriers, system/content/media,
  tools/choices, config rejection, and no-dispatch collisions. There are no
  equivalent public Chat response fixtures for:

  - missing/wrong-shaped `choices`;
  - missing or malformed `message`, tool-call IDs/names, or JSON arguments;
  - unknown top-level response, usage, message, content, and tool-call fields;
  - malformed usage values and response-side error/termination behavior.

  The current fixture always returns a well-formed minimal Chat completion, so
  the response defects above remain untested.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The new
  request-side documentation is accurate, including explicit rejection of
  unrepresentable request extensions and the tested Chat captures. It does not
  disclose that the supported Chat response translation drops open response
  fields or turns malformed Chat response shapes/arguments into successful
  Anthropic JSON. The broad compatibility and no-silent-loss claims therefore
  remain overbroad.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The
  request-side changes are surgical and collision-aware, and they correctly
  avoid duplicating wrapper extensions onto generated tool messages. However,
  `translate_to_anthropic` remains a non-failing, lossy constructor on the
  reverse Chat path. The `unwrap_or`/invalid-JSON defaults at the response
  boundary are precisely the kind of silent fallback the goal forbids.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS.
  - `cargo clippy --all-targets -- -D warnings` — PASS.
  - `cargo build --verbose` — PASS.
  - `cargo test --verbose` — PASS.
  - `cargo deny check` — PASS, with the existing non-fatal warnings noted above.
  - `cargo test --test client_compatibility -- --nocapture` — PASS, 60 passed
    and 1 ignored.
  - `cargo test --test client_compatibility installed_codex_cli_smoke
    -- --ignored --nocapture` — PASS; the installed Codex 0.144.1 probe used
    only the loopback fixture.

## Issues Found

### Blocking: reverse Chat responses still fabricate success and drop extras

`src/routes/messages/non_stream_translation.rs::translate_to_anthropic` has
the signature `fn translate_to_anthropic(response: &Value) ->
AnthropicResponse`, so it cannot report malformed upstream data. It treats a
missing/wrong `choices` value as an empty list and returns a successful empty
Anthropic message. It also rebuilds response text and tool-use blocks from
selected fields only, sets `AnthropicResponse.extra` and
`AnthropicUsage.extra` to empty maps, defaults missing tool-call identity and
invalid JSON arguments, and defaults malformed usage counters to zero.

The public fixture always returns:

```json
{
  "choices": [{
    "message": {"role": "assistant", "content": "chat fixture"},
    "finish_reason": "stop"
  }],
  "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
}
```

It therefore cannot establish the required malformed-response, exact
response-side extension, or native-error behavior. The new request-side
preserve-or-reject policy does not cover this reverse boundary.

## What Must Be Fixed

1. Make the non-streaming Chat response translator validate required response
   shape and handled scalar types, returning a sanitized Anthropic error before
   success when `choices`, message/tool-call identity, arguments, or usage are
   malformed.
2. Preserve representable unknown top-level, usage, message, content, and
   tool-call fields in the Anthropic response, or explicitly reject them when
   no safe Anthropic target exists; do not initialize the open extras as empty
   after discarding provider data.
3. Add deterministic public Chat fixtures for malformed JSON shapes, malformed
   tool calls/arguments/usage, response-side extras, and exact error/no-success
   semantics. Re-run the complete accumulated suite, gates, and canary.
