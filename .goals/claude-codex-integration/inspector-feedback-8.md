# Inspector Feedback — Iteration 8

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  the complete accumulated diff from
  `8b7472013665b168737dbb055d9f98f4f735b6d5` through Builder commit
  `fe7fbbe618373ea465aaa49cb480ed45553c8975`.
- Re-read the Codex 0.144.1 `ResponseItem::FunctionCall` and terminal usage
  serde definitions at
  `44918ea10c0f99151c6710411b4322c2f5c96bea`.
- Independently probed valid model-less creation, statusless/matching/
  conflicting terminal IDs, absent/null/partial/wrong/negative/fractional/
  overflow usage, terminal variants, and malformed function-call scalar
  fields. Ran the expanded public suite, every required quality gate, and the
  installed loopback Codex canary. No paid/external provider or port 4141 was
  used.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, setup, endpoints, headers, and
  evidence matrix.** Documentation now includes current versions, setup,
  source-backed Codex terminal/usage contracts, framing/lifecycle policy,
  provider routing, and deterministic evidence references.
- [x] **Criterion 2 — credential-free black-box public boundary harness.**
  The real Axum router and ephemeral fixture cover both clients, optional
  items, compaction continuation, reasoning framing, lifecycle replays,
  statusless terminals, usage cases, and native terminal forwarding without
  external credentials or port 4141.
- [ ] **Criterion 3 — complete Claude Code Messages compatibility.**
  Created-model fallback, terminal identity/status, usage validation,
  reasoning framing, lifecycle replay, and truncation handling now work for
  the covered cases. A malformed but source-invalid function-call item still
  becomes successful Anthropic tool output:

  ```json
  {
    "type": "response.output_item.added",
    "output_index": 0,
    "item": { "type": "function_call" }
  }
  ```

  followed by an equally incomplete `response.output_item.done` produced a
  `content_block_start` with empty tool id/name, then `message_delta` with
  `tool_use` and `message_stop`. `extract_function_call_details` uses empty
  defaults for required `call_id`, `name`, and `arguments`, and the done path
  repeats that coercion. Codex's typed `FunctionCall` fields are required.
  Wrong-typed scalar values follow the same silent-default path.
- [x] **Criterion 4 — Codex Responses and compaction compatibility.**
  Native statusless terminal forwarding, optional `ResponseItem` variants,
  reasoning/content streams, function-call reconciliation for valid items,
  id-less compaction continuation, usage, and model/provider routing remain
  green. The discovered issue is malformed-event handling in the
  Responses-to-Anthropic bridge rather than a valid Codex request/response
  path.
- [ ] **Criterion 5 — native JSON/SSE contracts and terminal behavior.**
  Valid terminal and usage semantics are now strict, but malformed function
  item fields are converted into a success-shaped Anthropic stream instead of
  one protocol-native error. This violates the explicit malformed-frame and
  no-fabricated-success requirement.
- [x] **Criterion 6 — authentication, routing, provider-only mode, and
  discovery.** Client keys, provider credential replacement, mappings,
  `/v1/models`, unsupported route/model errors, and provider-only startup
  behavior remain covered and unchanged.
- [ ] **Criterion 7 — regression tests for every compatibility claim.**
  Terminal and usage tests are broad, but no public or unit regression sends a
  function-call item with missing/wrong `call_id`, `name`, or `arguments`
  through the translated Messages stream. The direct probe exposes the
  missing validation.
- [ ] **Criterion 8 — documentation claims are fully evidenced.**
  Documentation claims complete function-call and lifecycle support but does
  not state that malformed required function-call scalars are rejected; the
  actual bridge silently fabricates empty tool identity and success.
- [ ] **Criterion 9 — surgical, lossless, hardened implementation.**
  Previous hardening and bounded state remain intact, but defaulting
  malformed required function-call fields to empty values is a silent
  fallback and produces an invalid Anthropic tool block.
- [x] **Criterion 10 — quality gates.** All required commands passed:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --verbose`
  - `cargo test --verbose`
  - `cargo deny check`

  Additional checks passed:
  `cargo test --test client_compatibility --verbose` (18 passed, 1 ignored),
  `cargo test --lib --verbose`, and
  `cargo test --test client_compatibility installed_codex_cli_smoke
  -- --ignored --nocapture` (1 passed). `cargo deny check` retained existing
  non-fatal unmatched-license/duplicate-dependency warnings but returned
  success.

## Issues Found

### Blocking: required function-call scalars are silently coerced

Codex's `FunctionCall` response item requires `name`, `arguments`, and
`call_id`. The bridge's dynamic extraction uses `unwrap_or("")` and permits
missing/wrong-typed values, emitting an empty-id/empty-name Anthropic tool
block and successful terminal instead of a native error.

## What Must Be Fixed

1. Validate required function-call item fields and types on both
   `response.output_item.added` and `.done`; reject missing/wrong values with
   one terminal Anthropic error.
2. Audit tool-search and message item required scalars for the same
   `unwrap_or` coercion pattern.
3. Add public malformed scalar fixtures and assert no empty tool identity,
   no success terminal, exactly one final error, and no later frames. Re-run
   all prior gates and the installed canary.
