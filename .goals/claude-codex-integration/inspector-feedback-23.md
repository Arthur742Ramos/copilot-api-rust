# Inspector Feedback — Iteration 23

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior feedback, current status, and the complete
  Builder commit `8eed716b040aaf7ad2ed810ed3169c01480b8869`.
- Verified the public suite count remains 67 (66 passed and 1 ignored). The
  new refusal split, interleaved, mirror, repeated, empty, late, and malformed
  model branches are registered in the provider model list and executed by the
  existing public tests; the direct suite executes its split/interleaved/
  mirror/repeated/empty refusal paths as well.
- Audited refusal accumulation and bounds, ordinary-content mirroring,
  interleaving and deduplication, content-filter/finish/EOF/late-terminal
  handling, removal of the non-stream unsafe refusal carrier, and analogous
  incremental reasoning/content paths.
- Ran the public Axum suite: 66 passed and 1 ignored. Ran the installed Codex
  `0.144.1` loopback canary explicitly: it passed.
- Ran formatting, Clippy with warnings denied, verbose build, verbose tests,
  and `cargo deny check`; all passed. `cargo deny check` retains the existing
  non-fatal unmatched-license/duplicate-dependency warnings.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The documentation now covers refusal fragments, mirror/dedup
  policy, bounds, content-filter terminal semantics, and the provider/direct
  fixtures. The audited client versions and route matrix remain present.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The existing
  67-test public harness exercises the new cases through the production Axum
  router and deterministic loopback fixtures. It uses no paid provider, no
  external network, and no port 4141.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** Refusal
  fragments now accumulate in source order, ordinary content/refusal mirrors
  are prefix-checked, duplicate output is avoided for normal interleaving, and
  the raw refusal is no longer copied into the non-stream
  `chat_message_extensions` carrier. Provider/direct fixtures cover the
  requested ordinary cases.

  A remaining ordering bug exists when refusal/content interleaves with a
  currently open tool block. The translator appends every `delta.content`
  fragment to `state.chat_text_output` before `handle_content` decides whether
  it must defer that content behind the tool block. At finish, it calls
  `flush_reconciled_refusal` before `handle_finish` calls
  `flush_deferred_content`. If deferred content is `"foo"` and accumulated
  refusal is `"foobar"`, the refusal flush emits `"bar"` first because it
  assumes `"foo"` was already emitted; the deferred content is then emitted,
  producing `barfoo` instead of `foobar`. This is a client-visible
  content/refusal ordering error not covered by the new fixtures.

- [ ] **Criterion 4 — Codex Responses and inbound translation.** Native Codex
  Responses/compact and the accumulated inbound behavior remain green. The
  retained Chat transport still has a possible malformed ordering outcome in
  the translated streaming path when refusal/content fragments overlap with
  deferred tool content, so the complete cross-transport goal is not yet
  satisfied.

- [ ] **Criterion 5 — native contracts, errors, and no silent loss.** The
  refusal carrier removal, strict content-filter/finish checks, bounds, one
  terminal, late-event suppression, and native Anthropic error behavior are
  improved. The deferred-content ordering case can emit a successful stream
  with text in the wrong order rather than fail closed. This violates exact
  text preservation even though the terminal stop reason and event count remain
  valid.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models, and
  aliases.** Provider and direct-Copilot refusal cases route through the same
  updated state machine and execute successfully in the public harness.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  requested refusal split, interleaved, mirror, repeated, empty, and malformed
  cases exist and execute despite the unchanged test count. Missing is a
  provider/direct fixture that combines a live tool block, deferred ordinary
  content, and a longer refusal mirror, which is the ordering combination that
  exposes the remaining bug.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The docs
  claim accumulated ordinary content and refusal remain prefix-compatible and
  that refusal-only output is emitted once at finish. They do not describe the
  deferred-tool ordering interaction, and the current implementation can violate
  the claimed source order.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The
  refusal accumulator has bounded state and avoids the unsafe carrier, but it
  uses `chat_text_output` as both “seen” and “already emitted” content. Those
  are different states while tool content is deferred. Conflating them causes
  a silent ordering transformation at a supported boundary.

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

### Blocking: refusal reconciliation counts deferred content as emitted

In `translate_chunk_to_anthropic_events`, `emitted_text` is appended to
`state.chat_text_output` immediately after `handle_content`. But
`handle_content` intentionally stores text in `state.deferred_content` when a
tool block is open. At a `content_filter` finish, `flush_reconciled_refusal`
runs before `handle_finish`, while `handle_finish` is the code that later
flushes `deferred_content`.

A valid interleaved sequence can therefore be:

1. an open tool call;
2. a content/refusal mirror where content contributes `"foo"` and refusal
   contributes `"foobar"` while the tool block keeps content deferred;
3. a `content_filter` finish.

The refusal reconciliation sees `chat_text_output == "foo"` and emits only
`"bar"`, then the finish path emits the deferred `"foo"`. The client receives
the wrong text order despite a single valid terminal event.

## What Must Be Fixed

1. Track emitted text separately from deferred text, or flush deferred content
   before refusal suffix reconciliation while preserving tool/content block
   ordering.
2. Add provider and direct public fixtures combining an open tool call,
   deferred content, refusal fragments, and `content_filter`; assert exact
   Anthropic text order, stop reason, and one-terminal behavior.
3. Re-run the accumulated compatibility suite, all quality gates, and the
   installed Codex canary.
