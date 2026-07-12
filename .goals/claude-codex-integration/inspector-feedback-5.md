# Inspector Feedback — Iteration 5

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  the complete accumulated diff from
  `8b7472013665b168737dbb055d9f98f4f735b6d5` through Builder commit
  `33fd7f293f7e4c681465dceaf2e6fbad936b97a0`.
- Re-verified Claude Code 2.1.207 and Codex CLI 0.144.1, and re-read Codex's
  HTTP SSE parser at source commit
  `44918ea10c0f99151c6710411b4322c2f5c96bea`. That parser recognizes
  `response.reasoning_text.delta`, summary part added/delta/done events, and
  the authoritative `response.output_item.done`.
- Independently ran the public framing fixtures, the four `rs1` carrier cases,
  Codex optional item and id-less compaction continuation cases, direct event
  probes for missing/duplicate lifecycle events and reasoning-content deltas,
  every required quality gate, and the installed loopback Codex canary. No
  paid/external provider or port 4141 was used.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, setup, endpoints, headers, and
  evidence matrix.** The guide records reproducible client versions, setup,
  native endpoints and headers, Codex source evidence, the TypeScript
  reference, provider routing, and the shared summary framing policy.
- [x] **Criterion 2 — credential-free black-box public boundary harness.**
  Public Axum tests use ephemeral loopback fixtures, captured JSON/SSE, fake
  credentials, and no external provider or production port. The new public
  framing tests now prove exact non-stream/stream equivalence for leading and
  trailing whitespace, empty/whitespace parts, multiple parts, U+2063
  separators, opaque carriers, absent carriers, and exact signatures.
- [ ] **Criterion 3 — complete Claude Code Messages compatibility.**
  **Blocking failures remain in translated Responses streams:**

  1. Codex 0.144.1's recognized `response.reasoning_text.delta` event is not
     matched by `translate_responses_stream_event`; it falls through to
     `Vec::new()`. A direct probe after `response.created` returned
     `reasoning_content_delta=[]`, so valid reasoning content disappears from
     Claude Messages streaming.
  2. Summary parts are buffered until `response.output_item.done`. If
     `response.created`, `response.reasoning_summary_part.added`, and a
     summary delta are followed by `response.completed` without
     `output_item.done`, `handle_response_completed` emits a successful
     `message_delta`/`message_stop` and leaves the buffered reasoning
     unrendered. This is a successful truncation/data-loss path.
  3. A duplicate reasoning `response.output_item.done` is not suppressed or
     rejected. A direct probe produced a second `Thinking...` delta and a
     second identical signature delta for the same output index.

  The requested framing policy itself is now consistent for the covered
  summary-array/delta forms, and the earlier `rs1#...` request-carrier loss is
  fixed. The missing event and lifecycle validation still prevent complete
  audited-client compatibility.
- [x] **Criterion 4 — Codex Responses and compaction compatibility.**
  Optional typed fields, raw-preserved variants, legacy compaction aliases,
  id-less compact output, and exact next-turn continuation remain green.
  Native `/v1/responses` continues to forward uninspected native events
  without changing their OpenAI shape; the new failures are in the
  Responses-to-Anthropic bridge.
- [ ] **Criterion 5 — native JSON/SSE contracts and terminal behavior.**
  Native Responses JSON/SSE guards still protect the direct Codex surface,
  but the translated public Messages SSE path silently drops a recognized
  reasoning event, fabricates a successful terminal after a missing
  `output_item.done`, and duplicates a completed reasoning block. That
  violates the requirement to preserve events and never fabricate success
  after malformed/truncated input.
- [x] **Criterion 6 — authentication, routing, provider-only mode, and
  discovery.** Client authentication, provider credential replacement,
  model mappings, `/v1/models`, explicit unsupported route/model errors, and
  provider-only startup remain covered by source and public tests.
- [ ] **Criterion 7 — regression tests for every compatibility claim.**
  The green suite covers aggregate framing and duplicate `part.added`
  separator de-duplication, but has no regression for
  `response.reasoning_text.delta`, missing `output_item.done` at terminal,
  duplicate reasoning `output_item.done`, or malformed/out-of-order summary
  events. The direct probes demonstrate observable failures.
- [ ] **Criterion 8 — documentation claims are fully evidenced.**
  The summary policy and public framing evidence are documented, but the
  documentation presents the Codex reasoning event sequence as a complete
  bridge without documenting the dropped `reasoning_text.delta` event or
  missing/duplicate output-item behavior. The support claim remains broader
  than the implementation evidence.
- [ ] **Criterion 9 — surgical, lossless, hardened implementation.**
  Prior hardening and Codex request-field preservation remain intact, but
  silently ignoring a current Codex reasoning event and silently converting
  an incomplete item sequence into successful Anthropic completion violate
  the lossless/no-silent-fallback and stream-safety requirements.
- [x] **Criterion 10 — quality gates.** All required commands passed:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --verbose`
  - `cargo test --verbose`
  - `cargo deny check`

  Additional checks passed:
  `cargo test --test client_compatibility --verbose` (9 passed, 1 ignored),
  `cargo test --lib --verbose`, and
  `cargo test --test client_compatibility installed_codex_cli_smoke
  -- --ignored --nocapture` (1 passed). `cargo deny check` retained existing
  non-fatal unmatched-license/duplicate-dependency warnings but returned
  success.

## Issues Found

### Blocking: recognized Codex reasoning content events are dropped

Codex's audited HTTP SSE parser recognizes `response.reasoning_text.delta`,
but the bridge dispatcher has no corresponding case. The event is silently
discarded rather than emitted as Anthropic thinking content or rejected as an
unsupported/malformed event.

### Blocking: missing and duplicate reasoning item lifecycle is not validated

Buffered summary parts are not flushed or failed when `response.completed`
arrives without `response.output_item.done`, so the bridge emits success while
losing reasoning. Repeating `response.output_item.done` for the same reasoning
output index emits duplicate Anthropic content/signatures.

## What Must Be Fixed

1. Implement the audited `response.reasoning_text.delta` mapping, or reject it
   with a native terminal error rather than silently dropping it.
2. Track reasoning item lifecycle and make terminal completion fail (or
   deterministically flush a complete item) when required item completion is
   missing; never emit successful Anthropic completion with pending reasoning.
3. Suppress or reject duplicate `response.output_item.done` for reasoning and
   add malformed/out-of-order event validation.
4. Add public provider-fixture regressions for all three cases, then rerun
   every gate and the installed canary.
