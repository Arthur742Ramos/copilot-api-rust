# Inspector Feedback — Iteration 18

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, the current status, and
  the complete Builder commit `2edf1e8be642de172f8e216b1e0f32d1451c40a5`.
- Re-checked the implementation from the initial SHA
  `8b7472013665b168737dbb055d9f98f4f735b6d5`, including the new
  Messages-to-Responses extension/collision policy and route-aware control
  validation.
- Verified the installed clients: `claude --version` reports `2.1.207
  (Claude Code)` and `codex --version` reports `codex-cli 0.144.1`.
- Independently checked the cited Codex `ResponsesApiRequest` source. It has no
  `stop_sequences` member, so rejecting a non-empty Anthropic stop list on a
  Responses bridge is preferable to silently dropping it. The cited JSON
  Schema `stringArray` meta-schema has `default: []` and `uniqueItems: true`
  but no `minItems`; accepting empty `required`, `dependentRequired`, and
  legacy dependency arrays is therefore correct.
- Ran the public compatibility suite through the Axum router: 57 passed, 1
  ignored (the loopback-only Codex canary). The ignored canary was also run
  explicitly and passed.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The documentation records Claude Code 2.1.207, Codex CLI
  0.144.1, configuration examples, native Messages/Responses/compact routes,
  headers, source links, and the feature matrix. The new stop-sequence source
  citation and JSON Schema source link are useful and accurate.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The
  `client_compatibility` suite uses `build_router()`, Axum `oneshot`, local
  deterministic fixtures, fake credentials, and no port 4141 or paid provider.
  It exercises both Claude-shaped Messages and Codex-shaped Responses traffic.
  The harness is real public-boundary coverage, although it still lacks the
  Chat Completions open-extension case described below.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The
  Responses bridge now preserves top-level and user/assistant message extras,
  including split message items, and rejects an extension on a tool-only or
  tool-result-only message rather than moving it to the wrong object. Native
  Anthropic forwarding and Responses stop-sequence rejection are also
  improved. However, the Chat Completions bridge still silently discards the
  same valid open fields:

  1. `translate_to_openai_with_options` constructs `ChatCompletionsPayload.extra`
     from a fixed list of `stop`, `temperature`, `top_p`, `user`, tools,
     choice, and thinking budget; it never merges
     `AnthropicMessagesPayload.extra`.
  2. `translate_anthropic_messages_to_openai` passes only `message.content` to
     `handle_user_message`/`handle_assistant_message`, and those constructors
     create `Message { extra: Map::new() }`. Thus
     `AnthropicInputMessage.extra` is lost for both user and assistant roles,
     including messages split around tool results.
  3. `handle_system_prompt` reduces structured system blocks to text and
     discards their unknown fields. The common request validator does not
     reject those fields for the Chat route.

  This is a supported Claude Messages-to-Chat transport, not a dead helper or
  an out-of-scope transport. A valid Claude request can therefore receive a
  successful response while silently changing the request object.

- [ ] **Criterion 4 — Codex Responses and inbound translation.** Codex native
  Responses/compact behavior, optional items, continuation, carriers, terminal
  validation, and the new Responses inbound extension policy remain covered and
  green. The inbound translation is still not consistently lossless: the
  Chat Completions path drops top-level and message open-object fields while
  the Responses path preserves them. That is a client-visible gap in the
  shared Messages inbound boundary and is not fixed by the current commit.

- [ ] **Criterion 5 — native contracts, errors, and no silent loss.** The new
  Responses policy correctly returns a native Anthropic 400 for non-empty
  `stop_sequences` before provider dispatch; null and empty lists are no-ops,
  Chat maps the list to `stop` without changing order/duplicates, and native
  Anthropic forwarding remains intact. The failure is that an unknown
  top-level or message field sent through the Chat route is silently omitted
  and the request still succeeds. This violates the criterion's explicit
  prohibition on silent loss even though the Responses route is now fail-closed
  for its own canonical collisions.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models, and
  aliases.** Existing provider-only initialization, client-key checks, safe
  header forwarding, model discovery, provider aliases, Responses/Chat/native
  routing, and admission ordering remain intact in the inspected diff and
  public tests. The new `provider_uses_responses_api` helper also keeps the
  Codex alias on the Responses control policy without requiring provider
  credential materialization first.

- [ ] **Criterion 7 — deterministic regression tests for every gap fixed and
  every compatibility claim.** The new public tests cover Responses
  top-level/message extensions, split items, collision rejection, empty schema
  arrays, and route-specific stop sequences. They do not cover the analogous
  valid top-level/message extensions through the Chat Completions provider,
  the split tool-result Chat sequence, or structured-system extensions on that
  route. Those omissions allow the concrete loss described under Criterion 3 to
  remain undetected.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The
  documentation accurately documents the Responses extension policy and the
  route-specific `stop_sequences` behavior, and correctly cites the
  `stringArray` source. But the broad compatibility/unknown-field claims and
  the “open-object extensions” description do not disclose that the supported
  Chat translation path still drops payload and message extras. The docs and
  matrix consequently overstate lossless Claude Messages compatibility.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The
  Responses changes are surgical, collision-aware, preserve nulls and map
  insertion order, and do not weaken admission or size/error hardening. The
  implementation is not lossless across all supported transports because the
  Chat translator still initializes every output message's `extra` to an empty
  map and builds the payload extra map from only selected known controls.
  Separately, the Responses translator intentionally rejects unrepresentable
  stop lists rather than silently dropping them, which is the correct policy.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS.
  - `cargo clippy --all-targets -- -D warnings` — PASS.
  - `cargo build --verbose` — PASS.
  - `cargo test --verbose` — PASS.
  - `cargo deny check` — PASS, with the repository's existing non-fatal
    unmatched-license/duplicate-dependency warnings.
  - `cargo test --test client_compatibility -- --nocapture` — PASS, 57 passed
    and 1 ignored.
  - `cargo test --test client_compatibility installed_codex_cli_smoke
    -- --ignored --nocapture` — PASS; the local Codex 0.144.1 canary used a
    loopback fixture and did not consume paid quota.

## Issues Found

### Blocking: Chat Completions silently loses open request fields

`src/routes/messages/non_stream_translation.rs` preserves only a selected set
of Anthropic controls in `ChatCompletionsPayload.extra`. It does not copy the
flattened `AnthropicMessagesPayload.extra`. Its message translation similarly
operates on content only and emits `Message` values with empty `extra` maps.
The structured-system helper extracts text with no extension transfer. The
public request validator allows ordinary unknown fields, so a request such as
the following is accepted and dispatched successfully on a Chat model:

```json
{
  "model": "chat-fixture/gpt-chat-fixture",
  "max_tokens": 128,
  "future_request_extension": {"keep": true, "null": null},
  "messages": [{
    "role": "user",
    "future_message_extension": {"keep": true, "null": null},
    "content": "hello"
  }]
}
```

The captured `/v1/chat/completions` request has neither extension. The same
loss applies to assistant messages and message splits around tool results.
This is a reproducible public-boundary compatibility defect and a violation of
the goal's unknown-field preservation and no-silent-fallback requirements.

## What Must Be Fixed

1. Either preserve `AnthropicMessagesPayload.extra` and representable
   `AnthropicInputMessage.extra` in the Chat Completions payload with
   canonical collision checks, including all split message items, or reject
   unrepresentable extensions before upstream dispatch with a documented native
   Anthropic error.
2. Apply the same explicit policy to structured system-block extensions; do not
   silently discard them.
3. Add credential-free public Chat-route fixtures for user/assistant/message
   splits, top-level nested/null/order preservation, collisions, and system
   extensions. Re-run the full accumulated contract, hardening, gate, and
   canary suite.
