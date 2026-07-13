# Inspector Feedback — Iteration 9

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  the complete accumulated diff from
  `8b7472013665b168737dbb055d9f98f4f735b6d5` through Builder commit
  `16ca220ff5705cc79c6d2a56909551eccd71ba8f`.
- Re-read the Codex 0.144.1 `ResponseItem` serde contracts and HTTP SSE event
  parser at `44918ea10c0f99151c6710411b4322c2f5c96bea`.
- Independently probed malformed known event scalars, optional tool-search
  call ids, non-stream malformed function outputs, valid argument
  reconciliation, lifecycle IDs/indices, and native Responses forwarding.
  Ran the expanded public suite, all required quality gates, and the installed
  loopback Codex canary. No paid/external provider or port 4141 was used.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, setup, endpoints, headers, and
  evidence matrix.** Documentation records the current client versions,
  source-backed item/event contracts, setup, native routes, framing/lifecycle
  policy, provider routing, and deterministic test evidence.
- [x] **Criterion 2 — credential-free black-box public boundary harness.**
  Public Axum fixtures exercise both clients, optional items, compaction,
  reasoning framing, lifecycle and terminal failures, malformed scalar
  families, statusless terminals, usage, and native forwarding without
  external credentials or port 4141.
- [ ] **Criterion 3 — complete Claude Code Messages compatibility.**
  Stream-side known scalar validation now rejects the prior malformed
  function/message/tool-search/reasoning/compaction cases. Cross-transport
  gaps remain:

  1. Codex permits `tool_search_call.call_id` to be absent/null. The stream
     bridge synthesizes a stable `tool_call_0` id and emits a tool block, but
     `create_tool_search_use_content_block` returns `None` in the non-stream
     bridge when the same valid item has no call id. JSON and SSE therefore
     disagree and the non-stream Claude request silently loses a valid tool
     call.
  2. A non-stream `ResponsesResult` with a malformed/missing
     `function_call.call_id`, `name`, or `arguments` is accepted by the
     `null_to_default` response deserializer, then the translator drops the
     empty tool block and returns `stop_reason: "end_turn"`. A direct probe
     returned `nonstream-malformed-function content=[] stop=end_turn`.
     Stream translation rejects the analogous item, so the public APIs have
     inconsistent failure semantics.
- [x] **Criterion 4 — Codex Responses and compaction compatibility.**
  Native Responses forwarding, optional input items, reasoning/content stream
  handling, valid function argument reconciliation, tool-search optional
  fields, id-less compaction continuation, usage, IDs, and model/provider
  routing remain green. The remaining defects are in the Claude bridge's
  non-stream translation and do not alter the native OpenAI Responses wire
  path for valid Codex traffic.
- [ ] **Criterion 5 — native JSON/SSE contracts and terminal behavior.**
  The stream path now fails malformed known scalars, but the non-stream path
  silently drops malformed function-call output and silently drops a valid
  optional-id tool-search call. This violates the requirement for consistent
  native failure semantics and no fabricated successful completion.
- [x] **Criterion 6 — authentication, routing, provider-only mode, and
  discovery.** Client keys, provider credentials, mappings, `/v1/models`,
  explicit unsupported route/model errors, and provider-only startup remain
  covered and unchanged.
- [ ] **Criterion 7 — regression tests for every compatibility claim.**
  The new public scalar suite is extensive for streamed output events, but
  there is no paired non-stream fixture for absent tool-search call ids or
  malformed function-call output fields. The direct probes expose the
  missing cross-transport regressions.
- [ ] **Criterion 8 — documentation claims are fully evidenced.**
  The documentation claims complete function/custom/tool-search and
  streaming/non-streaming support, but does not disclose that JSON translation
  drops a valid optional-id tool-search call and turns malformed function
  output into `end_turn`. These support claims remain broader than the
  evidence.
- [ ] **Criterion 9 — surgical, lossless, hardened implementation.**
  Earlier hardening, bounded state, strict stream validation, and request
  unknown-field preservation remain intact. The non-stream `null_to_default`
  path still silently converts invalid response fields into dropped content,
  and the two transports disagree on valid optional tool-search calls.
- [x] **Criterion 10 — quality gates.** All required commands passed:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --verbose`
  - `cargo test --verbose`
  - `cargo deny check`

  Additional checks passed:
  `cargo test --test client_compatibility --verbose` (25 passed, 1 ignored),
  `cargo test --lib --verbose`, and
  `cargo test --test client_compatibility installed_codex_cli_smoke
  -- --ignored --nocapture` (1 passed). `cargo deny check` retained existing
  non-fatal unmatched-license/duplicate-dependency warnings but returned
  success.

## Issues Found

### Blocking: non-stream Responses translation silently loses tool output

The stream bridge validates required function-call scalars and supports a
missing tool-search call id with a deterministic generated id. The non-stream
bridge neither validates malformed typed output fields nor applies the same
tool-search fallback. This makes the same Codex response succeed differently
depending only on `stream`.

## What Must Be Fixed

1. Apply equivalent output-item validation/reconciliation to non-stream
   `ResponsesResult` translation; malformed required function-call fields must
   yield one native error, never an empty `end_turn`.
2. Make non-stream tool-search calls with absent/null optional `call_id`
   follow the stream policy (or explicitly reject them), with a regression
   asserting identical tool-use semantics.
3. Add public JSON fixtures for malformed function/message/tool-search output
   fields and valid optional tool-search IDs, then rerun all prior gates,
   lifecycle/framing tests, and the installed canary.
