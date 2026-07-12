# Inspector Feedback — Iteration 2

## Verdict: FAIL

## Acceptance Criteria Check

- [ ] Criterion 1 — **FAILED**: The checked-in matrix is broad and has code/test
  references, but it overstates provider-scoped compatibility. The row
  “Provider-scoped Messages and count-token routes” is marked **Supported**, and
  the error row claims that 400/401/403/404/413/429/529/5xx client-facing
  failures all produce complete Anthropic envelopes. The provider count-token
  route still violates both claims.
- [x] Criterion 2 — **Verified**: The reviewed diff and regression suite cover
  common streaming and non-streaming requests, string and structured content,
  system normalization, tools and tool results, metadata, model selection, and
  flattened unknown fields. The generation validation and provider admission
  paths are also exercised.
- [x] Criterion 3 — **Verified**: The stream state machines and flow drivers
  cover deterministic message lifecycle ordering, thinking/reasoning,
  fragmented and interleaved tool calls, usage, stop reasons, upstream error
  events, malformed chunks, transport failures, truncated/empty streams, and
  normal completion. The reviewed tests assert terminal events and block
  ordering rather than silently accepting EOF.
- [ ] Criterion 4 — **FAILED**: A Claude Code-facing count-token failure can
  still be non-SDK-recognizable. Unknown providers return a nested-only
  `{"error": {...}}` object without the required top-level `"type": "error"`.
  Malformed JSON on the direct provider count-token route is rejected by
  Axum's `Json<Value>` extractor before the handler and produces plain text
  instead of an Anthropic JSON error envelope.
- [x] Criterion 5 — **Verified**: Model discovery and aliases, `[1m]` handling,
  beta/header filtering, provider resolution, and client-side validation were
  reviewed with their associated tests. No additional blocking gap was found.
- [ ] Criterion 6 — **FAILED**: The suite contains the corrected provider
  Messages 404 regression and public Messages malformed-body coverage, but no
  regression test covers the direct provider count-token unknown-provider
  envelope or its malformed JSON extractor rejection. The matrix therefore
  lacks credential-free evidence for a critical supported provider count-token
  failure path.
- [x] Criterion 7 — **Verified**: Existing behavior in the audited surface was
  reviewed, and every required quality gate completed successfully.

## Quality Gate

- `cargo fmt --all -- --check` — **PASS**
- `cargo clippy --all-targets -- -D warnings` — **PASS**
- `cargo build --verbose` — **PASS**
- `cargo test --verbose` — **PASS**
- `cargo deny check` — **PASS** (available; only existing non-blocking
  license warnings were reported)

## Runtime Evidence

I independently exercised an isolated credential-free server instance on port
4142 with provider-only test configuration; no real credentials were used and
port 4141 was not touched. A valid provider count-token request returned the
expected `input_tokens` JSON. An unknown provider returned HTTP 404 with the
nested-only error shape, and malformed JSON on the direct provider count-token
route returned Axum plain text rather than JSON. The public
`/v1/messages/count_tokens` provider/model alias delegates to this same handler
(`src/routes/messages/count_tokens_handler.rs:116-120`), so this is a reachable
Claude Code-facing path rather than dead code.

## Issues Found

### 1. Unknown provider bypasses the shared Anthropic error renderer

`src/routes/provider/count_tokens.rs:35-46` directly constructs:

```json
{
  "error": {
    "message": "Provider '...' not found or disabled",
    "type": "invalid_request_error"
  }
}
```

It omits the SDK-required top-level `"type": "error"`. The shared
`AppError` renderer in `src/libs/error.rs` now correctly normalizes nested-only
upstream envelopes, but this route bypasses it. The same response is reachable
through a public provider/model count-token alias.

### 2. Malformed provider count-token JSON bypasses application error handling

`src/routes/provider/count_tokens.rs:99-102` accepts `Json<serde_json::Value>`.
Malformed JSON is therefore rejected by Axum before
`post_provider_count_tokens` can call `AppError::into_response`; the resulting
plain-text extractor rejection is not an Anthropic `type: error` envelope.
The handler's manual `serde_json::from_value` branch only handles valid JSON
with an invalid Anthropic payload and does not cover malformed request bytes.

### 3. Required regression evidence is missing

`tests/provider_routing.rs` asserts the fixed Messages 404 shape, but it does
not exercise `/:provider/v1/messages/count_tokens`. `tests/router_smoke.rs`
asserts malformed JSON for the public Messages route, not the provider
count-token route. Consequently, the checked-in matrix's provider count-token
support claim is not backed by tests for the failure behavior that currently
breaks it.

## What Must Be Fixed

1. Make unknown-provider count-token responses use the shared error path or
   explicitly emit the complete Anthropic envelope with top-level
   `"type": "error"`, while preserving HTTP 404 and the safe nested diagnostic.
2. Replace the direct provider count-token `Json<Value>` rejection path with
   raw-body/manual parsing or an equivalent rejection mapper so malformed JSON,
   body-limit failures, and invalid payloads all return SDK-recognizable
   Anthropic envelopes with the appropriate status.
3. Add deterministic credential-free tests for direct provider count-token
   unknown-provider and malformed-body responses, plus the public
   provider/model alias path. Assert status, top-level discriminator, nested
   error type, and safe message.
4. Re-run the quality gates and correct the compatibility matrix's status and
   evidence if the behavior is intentionally left divergent; the current
   **Supported** claims cannot remain unchanged.
