# Inspector Feedback — Iteration 22

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior feedback, current status, and the complete
  Builder commit `10f5dfbeac1847ffb231bf15e571717761b0fd70`.
- Verified that the public test count is unchanged at 67 total (66 passed and
  1 ignored), then confirmed that the new requested cases are model branches
  inside the existing public tests and actually execute. The provider suite
  runs the optional tool, refusal, service-tier, logprobs, and malformed
  matrices; the direct suite runs the optional tool, refusal, late-tier, strict,
  no-usage, and identity paths.
- Audited the official optional tool-delta permutations, late identity and
  extra merging, bounds, final object validation, refusal reconciliation,
  logprobs rejection, top/nested service-tier parsing, and provider/direct
  drivers.
- Ran the public Axum suite: 66 passed and 1 ignored. Ran the installed Codex
  `0.144.1` loopback canary explicitly: it passed.
- Ran formatting, Clippy with warnings denied, verbose build, verbose tests,
  and `cargo deny check`; all passed. `cargo deny check` continues to show the
  repository's existing non-fatal unmatched-license/duplicate-dependency
  warnings.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The documented Chat stream contract now covers optional tool
  fragments, refusal, logprobs, service-tier enums, extras, and terminal
  policy. The source and route matrix remain recorded.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The existing
  public tests execute the new provider/direct cases despite the unchanged test
  count. They cross the production Axum router and local fixtures without paid
  credentials or port 4141.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The
  optional tool delta work is now substantially correct: first and
  continuation fields may be absent, late id/name authority is merged,
  conflicts and late extras fail, bounds are enforced, and terminal calls
  require complete object-valued JSON. Top-level and nested service tiers now
  share enum/null/conflict handling, and non-null logprobs are rejected in both
  buffered and SSE paths.

  A remaining refusal defect breaks exact JSON/SSE semantics. The non-stream
  translator emits refusal text as an Anthropic text block **and** retains the
  original refusal under `chat_message_extensions`. The SSE translator emits
  the text and `stop_reason: "refusal"` but has no equivalent message-extension
  carrier. More importantly, `reconcile_refusal_content` treats the first
  non-empty refusal delta as the complete authoritative string and rejects a
  later different fragment. A streamed refusal such as `"blo"` followed by
  `"cked"` is therefore classified as malformed instead of accumulating the
  delta. The public “split” case splits ordinary content and repeats the full
  refusal string; it does not test refusal fragments.

- [ ] **Criterion 4 — Codex Responses and inbound translation.** All native
  Codex Responses/compact and prior inbound work remains green. The retained
  Chat transport still has a client-visible stream/non-stream refusal
  divergence and does not support the official incremental refusal-delta
  behavior, so the accumulated Claude compatibility goal remains incomplete.

- [ ] **Criterion 5 — native contracts, errors, and no silent loss.** The
  one-error/one-terminal behavior, strict tool completion, shared service-tier
  parser, consistent logprobs rejection, safe extras, and direct/provider
  headers are now strong. The refusal path still:

  - rejects valid-looking incremental refusal fragments when adjacent deltas
    carry different pieces;
  - preserves `refusal` in a non-stream extension object but drops that
    client-visible extension in SSE while returning otherwise similar text and
    stop-reason content.

  That is not an equivalent native Anthropic JSON/SSE policy, and the
  rejection happens after a partial stream may already have emitted a text
  block.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models, and
  aliases.** Configured-provider and direct-Copilot Chat paths use the updated
  state machine. The executed direct/provider cases confirm route selection
  and safe behavior.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  requested optional tool, service-tier, refusal, logprobs, and malformed cases
  do exist and execute inside the unchanged 67-test suite. Missing regressions
  remain for:

  - refusal text split across distinct refusal deltas (`"blo"`/`"cked"`);
  - refusal deltas split and interleaved with equivalent content fragments;
  - JSON/SSE assertions that compare the presence and shape of the preserved
    `refusal` extension, not only text and stop reason;
  - the same refusal-fragment adversaries through the direct-Copilot route.

  The current installed Codex canary does not exercise Chat SSE.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The docs
  claim refusal text is handled losslessly and that the stream uses the same
  contract, but the implementation rejects differing refusal fragments and
  omits the non-stream `chat_message_extensions.refusal` carrier from SSE.
  The existing split fixture does not substantiate the stronger claim.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The
  optional tool and service-tier implementation is bounded and collision-aware,
  and the logprobs reject policy is now consistent. The refusal accumulator
  still assumes each non-empty refusal delta is a complete value, and the two
  output protocols expose different preserved fields. This is a remaining
  client-contract and unknown-field preservation gap, not a style issue.

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

### Blocking: refusal deltas are treated as complete values instead of fragments

`reconcile_refusal_content` compares every later non-empty refusal value against
`state.chat_refusal_text` and errors when they differ. OpenAI's
`ChoiceDelta.refusal` is a field on an incremental ChatCompletionChunk delta;
the bridge already treats ordinary content as fragments and accumulates tool
arguments. A valid refusal stream can provide multiple refusal pieces. The
current implementation rejects that stream after potentially emitting the
earlier text.

The non-stream path also retains the source refusal under
`chat_message_extensions.refusal`, while the SSE path has no corresponding
extension field. The text and terminal reason match, but the claimed
loss-preserving JSON/SSE behavior does not.

## What Must Be Fixed

1. Accumulate refusal fragments in source order, reconcile them with content
   fragments without duplicate emission, and validate the final
   `content_filter`/refusal relationship.
2. Choose one client-visible policy for the source refusal field in JSON and
   SSE: preserve it in an equivalent stream carrier or explicitly document and
   test its intentional omission.
3. Add provider and direct public fixtures for split/interleaved refusal
   fragments, then rerun the full accumulated suite, quality gates, and
   canary.
