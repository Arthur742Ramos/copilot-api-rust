# Inspector Feedback — Iteration 24

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior feedback, current status, and the complete
  Builder commit `c0ce368bff0f42440ff14c99b6bfcb2e5dc84736`.
- Verified the public suite remains 67 tests (66 passed and 1 ignored). The
  new deferred-tool refusal, multiple-tool, reasoning fallback, incomplete,
  EOF, late-event, and scheduler-bound fixtures are registered and execute in
  the provider matrix; the corresponding direct refusal/scheduler fixtures also
  execute.
- Audited the source-ordered scheduler's observed/emitted/deferred state,
  tool markers, parallel indices, content/refusal/reasoning ordering, terminal
  cleanup, late-event suppression, and bounds.
- Ran the public Axum suite: 66 passed and 1 ignored. Ran the installed Codex
  `0.144.1` loopback canary explicitly: it passed.
- Ran formatting, Clippy with warnings denied, verbose build, verbose tests,
  and `cargo deny check`; all passed. `cargo deny check` retains the existing
  non-fatal unmatched-license/duplicate-dependency warnings.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The documentation now describes the source-ordered scheduler,
  deferred tool/text/reasoning behavior, refusal suffix ordering, bounds, and
  direct/provider evidence. The audited client versions and route matrix remain
  recorded.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The unchanged
  67-test suite exercises the new provider/direct scheduler combinations through
  the public Axum router and deterministic local fixtures. No paid provider or
  port 4141 is used.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The
  scheduler now correctly keeps deferred ordinary content behind tool blocks,
  drains multiple tools in first-seen order, reconciles `foo`/`foobar` refusal
  mirrors, emits reasoning fallback in the queue, validates incomplete calls,
  and suppresses late success. The added schedule assertions cover the main
  requested cases.

  A remaining hardening gap is that direct reasoning output bypasses the
  scheduler's total emitted-text bound. `handle_thinking_text` emits
  `thinking_delta` directly, and immediate `emit_complete_reasoning_opaque`
  emits signature/reasoning events directly, without advancing
  `chat_text_emitted` or another total response counter. Only deferred
  reasoning signatures use `deferred_output_bytes`. A stream containing many
  otherwise valid reasoning deltas can therefore exceed the same 16 MiB
  output budget that ordinary/refusal/deferred text enforces.

- [ ] **Criterion 4 — Codex Responses and inbound translation.** Native Codex
  Responses/compact and previous inbound work remain green. The retained Chat
  streaming transport still has an inconsistent response-size bound: ordinary,
  refusal, and queued text are bounded, while directly emitted reasoning and
  opaque signatures are not. This leaves the accumulated hardening goal
  incomplete.

- [ ] **Criterion 5 — native contracts, errors, and no silent loss.** Tool,
  refusal, content, and reasoning block ordering now fails closed for the
  tested malformed/late/EOF cases and emits one terminal event. The direct
  reasoning paths can still produce a successful stream larger than the
  documented response budget instead of a terminal error. That is a remaining
  response-size hardening violation even though event ordering is correct.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models, and
  aliases.** Provider and direct-Copilot paths use the same scheduler and the
  executed fixtures confirm both route families.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  new deferred-order, multiple-tool, incomplete-call, late-event, terminal,
  and scheduler-bound cases exist and execute. There is no public provider or
  direct fixture that sends enough direct `reasoning_text` or
  `reasoning_opaque` output to exercise the total response bound; the existing
  bound tests cover only ordinary/deferred text and refusal state.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The docs
  state that the scheduler bounds “all emitted text” and that the existing
  upstream-response budget governs queued function arguments/output. Direct
  reasoning deltas and immediate opaque reasoning do not update the emitted
  text counter, and no public fixture establishes their bound behavior. The
  hardening claim is therefore broader than the implementation.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The
  scheduler is source-ordered and bounded for ordinary/refusal/deferred text,
  but the same output boundary has two unbounded direct reasoning paths. This
  creates an avoidable inconsistency in response-size enforcement.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS.
  - `cargo clippy --all-targets -- -D warnings` — PASS.
  - `cargo build --verbose` — PASS.
  - `cargo test --verbose` — PASS.
  - `cargo deny check` — PASS, with the existing non-fatal warnings noted above.
  - `cargo test --test client_compatibility -- --nocapture` — PASS, 66 passed
    and 1 ignored.
  - `cargo test --test client_compatibility installed_codex_cli_smoke
    -- --ignored --nocapture` — PASS; the installed Codex canary used only the
    loopback fixture.

## Issues Found

### Blocking: direct reasoning emission bypasses the scheduler size bound

`emit_text_fragment` enforces `MAX_UPSTREAM_RESPONSE_BYTES` through
`state.chat_text_emitted`, and `defer_text_fragment` enforces the same limit
through `deferred_output_bytes`. In contrast:

- `handle_thinking_text` writes `ThinkingDelta` events directly and does not
  account for their bytes;
- `emit_complete_reasoning_opaque` writes the placeholder and signature
  deltas directly and does not account for the signature;
- `schedule_reasoning_opaque` therefore has no cumulative bound when it can
  emit immediately rather than defer.

The source SSE record limit bounds each individual frame, not the aggregate
translated response. A sequence of valid reasoning frames can exceed the
aggregate limit while ordinary content would fail closed. This violates the
goal's preserved response-size hardening and the documentation's “all emitted
text” claim.

## What Must Be Fixed

1. Account direct thinking text and opaque signature/placeholder output in one
   shared emitted-output budget, or route all such output through a bounded
   emitter.
2. Add provider and direct public fixtures at exactly the limit and one byte
   over for repeated reasoning text and opaque signatures, asserting one
   Anthropic error and no success terminal on overflow.
3. Re-run the full accumulated compatibility suite, quality gates, and
   installed Codex canary.
