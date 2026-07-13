# Inspector Feedback — Iteration 4

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  the complete accumulated diff from
  `8b7472013665b168737dbb055d9f98f4f735b6d5` through Builder commit
  `bbdf46c99984b182780223b9413b429c4f9a1643`.
- Re-verified Claude Code 2.1.207 and Codex CLI 0.144.1 using the current
  release tags and installed `claude --version` / `codex --version`.
- Re-read the Codex 0.144.1 reasoning schema at source commit
  `44918ea10c0f99151c6710411b4322c2f5c96bea`; summary text is a string with
  no non-empty constraint, and reasoning id/encrypted content remain optional.
- Ran the updated public Axum fixture tests, the four `rs1` request carriers,
  aggregate-empty non-stream and stream fixtures, Codex optional item and
  id-less compaction continuation tests, translation probes, every required
  quality gate, and the installed loopback Codex canary. No paid/external
  provider or port 4141 was used.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, setup, endpoints, headers, and
  evidence matrix.** The guide records reproducible release/tag/source
  evidence for both clients, exact setup and endpoint/header behavior, the
  TypeScript reference, provider mappings, optional Codex items, and the
  intentional HTTP-only Responses transport.
- [x] **Criterion 2 — credential-free black-box public boundary harness.**
  The tests use the production Axum router, ephemeral loopback fixtures,
  captured request/response bodies, fake credentials, and no port 4141 or
  paid provider. Aggregate-empty reasoning cases now run through public
  provider Messages routes in both stream and non-stream modes.
- [ ] **Criterion 3 — complete Claude Code Messages compatibility.**
  The prior `rs1#...` request-carrier loss is fixed, and the new public
  aggregate-empty fixtures correctly distinguish absent carriers from
  opaque id/encrypted carriers. However, stream and non-stream reasoning
  content is still inconsistent:

  1. A non-stream output summary containing `"  analysis"` is trimmed to
     `"analysis"` by `effective_reasoning_text`.
  2. The streamed delta path buffers leading whitespace and emits
     `"  analysis"`; the existing unit test explicitly asserts this behavior.
  3. The non-stream path concatenates multiple summary blocks directly
     (`"one"` + `"two"` → `"onetwo"`), while the audited TypeScript reference
     uses `\u2063\n\n` between summary segments. The stream path also does not
     handle the corresponding summary-part separator event.

  Direct probes against this HEAD produced:
  `nonstream=... "thinking":"analysis"` and
  `stream=... ThinkingDelta { thinking: "  analysis" }`, and a two-summary
  probe produced `"thinking":"onetwo"`. These are public provider behavior
  issues because `respond_responses_provider_messages_json` calls this
  translator for non-stream Responses providers.
- [x] **Criterion 4 — Codex Responses and compaction compatibility.**
  Codex optional typed fields and raw-preserved variants remain aligned with
  the audited `ResponseItem` schema. Public tests still pass for optional
  reasoning/image/tool-search/function variants, legacy compaction aliases,
  id-less compact output, and continuation of that exact item on the next
  Responses request.
- [x] **Criterion 5 — native JSON/SSE contracts and terminal behavior.**
  Native Anthropic/OpenAI envelopes, stream ordering, IDs/call arguments,
  exactly-one terminal handling, malformed-frame failure, premature EOF, and
  incomplete Responses terminals remain covered and green. The new
  reasoning-content issue is a data-preservation inconsistency, not a
  cross-protocol error-envelope leak.
- [x] **Criterion 6 — authentication, routing, provider-only mode, and
  discovery.** Public tests and source still establish client-key validation,
  provider credential replacement, model mappings, `/v1/models`,
  explicit unsupported route/model errors, and provider-only startup without
  unrelated GitHub initialization.
- [ ] **Criterion 7 — regression tests for every compatibility claim.**
  Empty-array, empty/whitespace-only, opaque-carrier, and carrier-free cases
  now have public stream/non-stream fixtures. The requested leading
  whitespace-followed-by-text behavior is only covered in a stream unit
  test, and there is no public/non-stream regression for that shape or for
  multiple summary blocks and their separator. The direct probes demonstrate
  the missing coverage catches real observable differences.
- [ ] **Criterion 8 — documentation claims are fully evidenced.**
  The documentation now accurately describes the four request-carrier
  combinations and aggregate-empty carrier rules, but its broad
  non-stream/stream carrier-support claim does not disclose the whitespace
  and multi-summary divergence. It should not claim consistent reasoning
  carrier behavior until these cases are either aligned or explicitly
  documented as an intentional client-safe divergence with evidence.
- [ ] **Criterion 9 — surgical, lossless, hardened implementation.**
  Prior admission/auth/size/retry/error hardening and Codex raw-field
  preservation remain intact. Nevertheless, trimming leading reasoning
  whitespace in one transport while preserving it in another, and removing
  the separator between distinct summary blocks, loses or changes valid
  reasoning content and violates the lossless/consistent boundary goal.
- [x] **Criterion 10 — quality gates.** All required commands passed:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --verbose`
  - `cargo test --verbose`
  - `cargo deny check`

  Additional checks passed:
  `cargo test --test client_compatibility --verbose` (7 passed, 1 ignored),
  `cargo test --lib --verbose`, and
  `cargo test --test client_compatibility installed_codex_cli_smoke
  -- --ignored --nocapture` (1 passed). `cargo deny check` retained existing
  non-fatal unmatched-license/duplicate-dependency warnings but returned
  success.

## Issues Found

### Blocking: reasoning text differs between stream and non-stream paths

`effective_reasoning_text` trims aggregate summary text and joins multiple
summary blocks without a separator. Streamed reasoning deltas preserve leading
whitespace and do not add the audited summary separator. Therefore the same
Codex reasoning content can produce different Anthropic thinking text depending
on upstream framing or `stream` mode.

## What Must Be Fixed

1. Define one documented aggregate/summary policy for stream and non-stream
   reasoning, including leading whitespace and empty elements, then use the
   same policy in both translators.
2. Preserve the separator between multiple reasoning summary blocks (or
   document and prove a deliberate client-safe divergence), and handle the
   corresponding streamed summary-part boundary.
3. Add public provider-fixture regressions for leading whitespace followed by
   substantive text and multiple summary blocks in both modes, asserting exact
   thinking text and signatures. Re-run the full gates and canary.
