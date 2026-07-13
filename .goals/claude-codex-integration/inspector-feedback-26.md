# Inspector Feedback — Iteration 26

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback files (iterations
  1–25), current `status.json`, and Builder commit
  `38adc486458004c9fe2aa5fb7ba6ba5e6912c36f`.
- Rechecked the accumulated implementation from initial SHA
  `8b7472013665b168737dbb055d9f98f4f735b6d5`.
- Audited every Responses retained-state owner and every output-budget
  transition, including sequence snapshots, response IDs, block keys, pending
  items, item metadata/IDs, reasoning IDs/parts, output-text keys/text, and
  active/inactive function-call metadata/arguments.
- Ran the public provider/direct Responses budget fixture and the web-search
  overflow fixture through the production Axum router. The fixture server is
  ephemeral loopback Axum; `tests/common/mod.rs` uses
  `server::build_router().oneshot(...)`, so these are not helper-only probes.
- The installed `codex` binary is not present in this environment
  (`command -v codex` returned no path), so the ignored installed canary could
  not be executed. No external or paid call was attempted.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The compatibility guide records Claude Code 2.1.207, Codex CLI
  0.144.1, the Codex source commit, setup examples, `/v1/messages`,
  `/v1/responses`, `/v1/responses/compact`, headers, provider routing, and
  the feature/transport matrix. The prior audit evidence remains present.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The new
  `claude_responses_state_budgets_cross_provider_and_direct_boundaries` test
  sends realistic Messages requests through the production router to
  provider and direct loopback Responses fixtures. The web-search overflow
  test also traverses the public Messages route and checks usage and HTTP
  metrics. No 4141 listener or paid provider is used.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The
  accumulated Messages JSON/SSE, tools, reasoning, compaction, token-counting,
  headers, aliases, errors, cancellation/truncation, and Chat regressions
  remain green. However, the Responses-to-Messages stream used by the
  Responses-backed Claude path still has non-atomic cross-budget accounting:
  an output reservation can succeed even though the corresponding retained
  reservation then fails. That is a protocol hardening failure on an in-scope
  Messages transport.

- [ ] **Criterion 4 — Codex Responses and inbound translation.** Existing
  native Responses, continuation, compaction, reasoning, function/tool,
  parallel-call, usage, and raw-variant behavior remains green, and the new
  public budget fixtures do reach the production boundary. The retained/output
  ownership transition is nevertheless not atomic, so an adversarial valid
  near-limit event can fail after charging a payload that was never emitted.

- [ ] **Criterion 5 — native contracts, errors, and no silent loss.** The
  checked-in overflow fixtures produce one Anthropic SSE error and no
  `message_stop`; the web-search overflow fixture returns one sanitized native
  HTTP error, records no token-usage row, and does not increment HTTP 200
  success. The remaining Responses accounting defect is:

  1. `reserve_output_payload` mutates `TranslatedOutputBudget` before
     `replace_retained_state_bytes` or a block-key reservation. This occurs for
     function metadata/arguments (around lines 540–545, 1051–1056, and
     1092–1098), output text (2337–2350), and reasoning/compaction emission
     (2056–2060 and 2235–2240).
  2. If the independent retained cap is full, the output counter is charged
     and the event is not emitted. `close_all_open_blocks` clears the retained
     owners but does not roll back `output_budget`, leaving a stale successful
     output reservation in the terminal error state.

  I also observed that the public Responses overflow run writes usage rows for
  `gpt-responses-state-over`, `gpt-responses-function-state-over`, and
  `gpt-responses-mixed-budget-over` (`provider_messages`, source `provider`,
  input/output/total `1/1/2`) even though each public response contains one
  translation error and no success terminal. `api_flows.rs` assigns terminal
  usage before calling the Responses translator and records it unconditionally
  at stream teardown. The web-search path has the required ordering, but the
  regular Responses overflow accounting is not covered or reconciled with the
  documentation's guarded-validation claim.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models, and
  aliases.** The accumulated public tests still cover client-key validation,
  provider/direct selection, provider-only operation, model aliases and
  explicit route/model failures. The provider/direct Responses budget test
  reaches both configured paths.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  new public test provides strong exact, UTF-8, mixed, parallel-function, and
  +1 overflow boundary coverage and the unit tests cover checked arithmetic,
  replacement, underflow, release, and terminal cleanup. It does not assert
  the cross-counter rollback invariant after a retained-cap failure, and it
  does not assert usage suppression/semantics for regular Responses overflow.
  The installed canary is correctly opt-in but was unavailable locally.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The guide
  accurately documents the new owner model and the Messages web-search
  pre-usage validation, and its matrix names the public Responses budget test.
  It nevertheless claims exact independent ownership and guarded streaming
  usage behavior that the cross-counter ordering and observed regular
  Responses usage rows do not establish.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The
  checked arithmetic and fixed-size lifecycle digests are appropriately
  bounded, and terminal cleanup clears retained owners. The output budget is
  not transactionally coupled to retained-state reservation, so failed
  transitions can retain a charge for a non-emitted payload. This violates the
  requested no-stale-reservation invariant.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS.
  - `cargo clippy --all-targets -- -D warnings` — PASS.
  - `cargo build --verbose` — PASS.
  - `cargo test --verbose` — PASS; the public compatibility crate reported
    68 passed and 1 ignored, and the full unit/integration run completed
    without failures.
  - `cargo deny check` — PASS; advisories, bans, licenses, and sources are
    OK, with the existing non-fatal unmatched-license warnings.
  - Targeted Responses ownership unit tests — PASS (30 tests).
  - `claude_responses_state_budgets_cross_provider_and_direct_boundaries` —
    PASS (public Axum boundary; exact/UTF-8/mixed/parallel and overflow cases).
  - `claude_web_search_overflow_precedes_usage_recording` — PASS (public
    boundary; no usage row and no HTTP 200 success mutation).

## Issues Found

### Blocking: output and retained reservations are not atomic

The independent counters are updated in separate operations. For example,
`append_function_call_arguments` calls `reserve_output_payload` and then
`replace_retained_state_bytes`; `append_output_text` does the same. A stream
can fill the retained budget independently while leaving output capacity. The
first call succeeds, the second fails, `terminate_responses_stream_with_error`
clears retained owners, but `TranslatedOutputBudget.used_bytes` remains
incremented although no corresponding client event was produced. The same
ordering exists for function metadata and reasoning/compaction output.

The checked-in tests prove each counter independently and prove successful
release, but do not exercise this adversarial failure ordering. The Builder
must make the pair transactional (or preflight both counters before mutating
either) and assert the post-error state.

### Blocking: regular Responses overflow usage is not reconciled with failure

The public overflow cases only assert SSE error shape. Inspecting the
temporary SQLite stores created by the run showed usage rows for all three
provider overflow models. The stream handler updates `usage` from a terminal
event before the translator validates it, then records that usage even when
translation has already terminated with an error. Either suppress usage for
translation-budget failures or document and test the deliberate upstream-cost
policy consistently; the current implementation and documentation disagree.

## What Must Be Fixed

1. Make every Responses output/retained reservation pair atomic, including
   function metadata, active and inactive arguments, authoritative
   `arguments.done` replacement, text, reasoning/signature, compaction, and
   block-key creation. Failed transitions must leave both counters unchanged,
   and terminal cleanup must leave no unowned/stale reservation.
2. Add adversarial unit tests that fill retained state while leaving output
   capacity, trigger each cross-counter failure path, and assert one error,
   no success terminal, zero retained owners, and no charge for the
   un-emitted output.
3. Define and test regular Responses usage behavior on translation overflow;
   do not record usage before a validated terminal, or explicitly codify and
   evidence a separate upstream-consumption policy.
4. Rerun the complete accumulated public suite, all gates, and the opt-in
   Codex canary when the audited binary is available.
