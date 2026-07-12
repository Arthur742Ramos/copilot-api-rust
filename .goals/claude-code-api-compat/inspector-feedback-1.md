# Inspector Feedback — Iteration 1

## Verdict: FAIL

## Acceptance Criteria Check

- [ ] Criterion 1 — **FAILED**: The checked-in matrix is present and broad, but its
  Anthropic error-envelope row is marked `Supported` even though one of the
  documented implementation paths emits a non-SDK-recognizable envelope. The
  matrix therefore overstates the verified compatibility.
- [x] Criterion 2 — **Verified**: The request validation, structured/string
  content, system normalization, tools/tool results, metadata, and flattened
  unknown-field paths are implemented and covered by the reviewed unit/router
  tests.
- [x] Criterion 3 — **Verified**: The reviewed stream state machines cover
  deterministic lifecycle ordering, fragmented/interleaved tool calls, thinking
  and reasoning, usage, stop reasons, malformed chunks, transport failures,
  interrupted streams, upstream error events, and normal completion.
- [ ] Criterion 4 — **FAILED**: At least two Claude Code-facing non-streaming
  failure paths can omit the required top-level `"type": "error"` field; details
  are below.
- [x] Criterion 5 — **Verified**: Model discovery/aliases, `[1m]` handling,
  request headers/beta filtering, and client-error validation paths were
  reviewed with the associated tests; no additional blocking gap was found.
- [ ] Criterion 6 — **FAILED**: The existing upstream-envelope regression test
  does not assert the required top-level type, and there is no regression test
  for the unknown-provider response shape.
- [x] Criterion 7 — **Verified**: The required quality gates passed for the
  Builder's commit.

## Quality Gate

- `cargo fmt --all -- --check` — PASS
- `cargo clippy --all-targets -- -D warnings` — PASS
- `cargo build --verbose` — PASS
- `cargo test --verbose` — PASS
- `cargo deny check` — PASS (available and run)
- Targeted `cargo test -q upstream_json_error_envelope_is_forwarded_verbatim` —
  PASS, but this test is insufficient because it does not check
  `body["type"]`.

## Issues Found

### 1. Nested upstream error envelopes are forwarded without the Anthropic wrapper

`src/libs/error.rs:127-139` documents recognition of both
`{"error": {...}}` and the full Anthropic
`{"type":"error","error":{...}}`, but `parse_upstream_error_envelope` returns
any object containing an object-valued `error` unchanged. The branch at
`src/libs/error.rs:259-263` then serializes that value verbatim.

For an upstream response body such as:

```json
{"error":{"type":"invalid_request_error","message":"The requested model is not supported."}}
```

the proxy returns the same nested-only object. Anthropic SDK clients expect the
top-level `"type":"error"` discriminator, so this is not an SDK-recognizable
Anthropic error envelope. The test at
`src/libs/error.rs:474-493` passes while missing this defect because it checks
only the nested fields and string type.

This affects generic non-streaming upstream HTTP failures, including normal
400/401/403/404/5xx responses that happen to use the common nested error shape.

### 2. Unknown provider response has the same invalid wire shape

The provider-scoped route at `src/routes/provider/messages.rs:121-131`
constructs a 404 body containing only:

```json
{"error":{"message":"...","type":"invalid_request_error"}}
```

It bypasses `AppError` and therefore bypasses the wrapper produced by the
shared error renderer. This is another Claude Code-facing non-streaming failure
that violates criterion 4.

## What Must Be Fixed

1. Normalize nested-only upstream error objects into
   `{"type":"error","error":{...}}`, while preserving an already complete
   Anthropic envelope, the safe message, status, and existing header
   allowlisting. Do not regress the 429/529 reshaping or internal-error
   sanitization.
2. Return the same complete Anthropic envelope for an unknown provider (or
   route that failure through the shared `AppError` renderer).
3. Strengthen
   `upstream_json_error_envelope_is_forwarded_verbatim` with an assertion that
   `body["type"] == "error"` and add a credential-free router/unit regression
   for the unknown-provider 404 shape. Update the matrix evidence/status if the
   implementation intentionally diverges instead of fixing it.
4. Re-run all listed quality gates after the fix.

No credentials were used and port 4141 was not touched during this review.
