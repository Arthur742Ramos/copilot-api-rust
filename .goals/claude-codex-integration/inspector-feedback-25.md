# Inspector Feedback — Iteration 25

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior feedback, current status, and the complete
  Builder commit `bc4dff295af891c4cbd1c32e6fac9ec80366ee2d`.
- Audited the shared `TranslatedOutputBudget`, Chat reservation ownership,
  Responses text/reasoning/tool/signature paths, deferred argument buffering,
  web-search reconstruction validation, overflow arithmetic, and terminal/error
  cleanup.
- Verified the public suite remains 67 tests (66 passed and 1 ignored). The new
  exact/+1/mixed-UTF-8 Chat reasoning/opaque/mixed fixtures are registered and
  execute for both provider and direct paths. There are no equivalent public
  Responses budget fixtures in `tests/client_compatibility.rs`; the Responses
  budget coverage remains unit-level.
- Ran the public Axum suite: 66 passed and 1 ignored. Ran the installed Codex
  `0.144.1` loopback canary explicitly: it passed.
- Ran formatting, Clippy with warnings denied, verbose build, verbose tests,
  and `cargo deny check`; all passed. `cargo deny check` retains the existing
  non-fatal unmatched-license/duplicate-dependency warnings.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The documentation records one shared 16 MiB translated-output
  budget, counted dynamic families, reservation behavior, Chat fixtures, and
  the client/route matrix.

- [ ] **Criterion 2 — credential-free black-box Axum harness.** The Chat
  budget cases cross the public Axum boundary for provider and direct paths.
  The claimed shared Chat/Responses/web-search budget is not equivalently
  exercised at the public boundary: no Responses provider/direct budget model
  or web-search overflow fixture is registered in the integration harness.
  Existing Responses tests remain functional but do not establish the new
  aggregate-budget contract.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** Chat
  exact, overflow, UTF-8, mixed tool/reasoning/signature, partial-error, and
  late-suppression cases pass. The Chat budget ownership is mostly coherent:
  deferred text and tool metadata/arguments reserve once and are emitted with
  an already-budgeted path; direct reasoning and opaque output now use the
  same budget.

  The implementation still has a separate Responses buffering-accounting
  defect. `append_function_call_arguments` always calls
  `reserve_buffered_translation_bytes(arguments.len())`, then calls it a
  second time when the call is inactive. It also reserves the full
  `arguments.done` value after reconciliation, even when it has already been
  retained as accumulated/buffered arguments. This can reject otherwise valid
  Responses argument streams based on stale or double-counted state bytes.

- [ ] **Criterion 4 — Codex Responses and inbound translation.** Native
  Responses behavior and existing Codex fixtures remain green, but the new
  shared output-budget contract is not proven at the public Responses
  boundary, and valid buffered function-call argument streams can consume
  multiple state-buffer reservations for one logical payload. This leaves the
  Codex streaming hardening claim incomplete.

- [ ] **Criterion 5 — native contracts, errors, and no silent loss.** Chat
  overflow errors close blocks, emit one error, and suppress late success.
  Responses has two remaining accounting problems:

  1. `append_function_call_arguments` reserves state-buffer bytes once for all
     arguments and a second time for inactive calls, while
     `handle_function_call_arguments_done` reserves the complete authoritative
     string again after reconciliation. The `TranslatedOutputBudget` charge is
     once, but the separate state cap is not ownership-balanced and can turn a
     valid stream into a premature protocol error.
  2. Web-search reconstruction records upstream usage before calling
     `validate_reconstructed_payload_budget`. An oversized reconstructed output
     therefore fails after usage has already been recorded, contrary to the
     documented “size failures rejected before usage is recorded” hardening
     order.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models, and
  aliases.** Existing provider/direct routing, credentials, model selection,
  and canary behavior are unchanged and the executed fixtures reach their
  intended paths.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** Chat
  exact/+1/multibyte/mixed-output budget fixtures execute in the unchanged
  public suite. Missing are public provider/direct fixtures for:

  - Responses function-call arguments at exact and near-limit sizes, including
    inactive buffered calls and `arguments.done`;
  - Responses reasoning/signature/text aggregate budgets;
  - direct/provider web-search reconstructed output overflow;
  - usage recording suppression when web-search budget validation fails.

  The only Responses budget test added by the Builder is an internal unit
  test for one UTF-8 text delta, not a public end-to-end contract test.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The docs
  claim one shared aggregate budget across Chat and Responses and assert
  malformed/oversized output is rejected before usage is recorded. The public
  evidence covers only Chat, and web-search currently records usage before its
  final budget check. The documentation therefore overstates the verified
  budget/metrics behavior.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The shared
  byte arithmetic is checked and Chat reservation paths are largely correct,
  but Responses has stale/double state-buffer reservation and web-search has
  post-record budget validation. These are concrete ownership/order defects in
  the new hardening layer.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS.
  - `cargo clippy --all-targets -- -D warnings` — PASS.
  - `cargo build --verbose` — PASS.
  - `cargo test --verbose` — PASS.
  - `cargo deny check` — PASS, with the existing non-fatal warnings noted above.
  - `cargo test --test client_compatibility -- --nocapture` — PASS, 66 passed
    and 1 ignored.
  - `cargo test --test client_compatibility installed_codex_cli_smoke
    -- --ignored --nocapture` — PASS; the local canary used only the loopback
    fixture.

## Issues Found

### Blocking: Responses function-call buffering is over-reserved

In `append_function_call_arguments`, the code reserves `output_budget` once,
but reserves `buffered_translation_bytes` unconditionally and again for an
inactive call. `handle_function_call_arguments_done` then reserves the full
authoritative argument string after `reconcile_function_call_arguments` has
already retained the streamed/suffix data. Because state-buffer reservations
are monotonic and are not released when buffered arguments are drained, a
valid large parallel-call stream can hit the 16 MiB state cap well before its
actual retained/output bytes require it.

### Blocking: web-search usage is recorded before output-budget validation

`try_handle_web_search` calls `recorder.record(...)` before
`validate_reconstructed_payload_budget(&response)`. If the reconstructed
content, citations, extensions, or stop sequence exceeds the new aggregate
budget, the request returns an error but usage has already been recorded. This
violates the documented hardening order and can affect admission/usage
accounting on a failed output.

## What Must Be Fixed

1. Separate retained-state reservations from output reservations, reserve each
   logical copy exactly once, and release/replace reservations when buffered
   arguments are drained or authoritative snapshots replace deltas.
2. Move web-search usage recording after reconstructed output budget validation,
   and add a public overflow fixture proving no usage is recorded on failure.
3. Add public Responses exact/+1/multibyte/mixed tool/reasoning/signature
   fixtures and rerun the full accumulated suite, gates, and canary.
