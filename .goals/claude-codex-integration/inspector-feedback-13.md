# Inspector Feedback — Iteration 13

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  Builder commit `2300418a5dde11b133391fe56dffa46f91be6e96`.
- Re-read the audited Codex 0.144.1 compact and Responses contracts. The
  compact mock in the audited Codex source returns an output-only object whose
  output contains retained message items and a compaction item; it does not
  require a complete Responses `id`, `model`, or `status`. Codex's compact
  usage remains optional and its counters/details are strict when present.
- Verified installed versions:
  - `claude --version` → `2.1.207 (Claude Code)`
  - `codex --version` → `codex-cli 0.144.1`
- Inspected direct Copilot and provider compact routes, separate buffered
  contracts, raw-byte/header forwarding, usage/metric recording, web
  authority validation, and the new public fixtures.
- Ran the current public compatibility suite, all required repository gates,
  and the ignored loopback Codex canary. No product code was changed by this
  inspection.
- Independently verified the direct compact route with an ephemeral loopback
  Copilot URL and a fake token/model catalog: the valid output-only, id-less
  compact body now returns exact bytes and safe headers. The remaining
  provider-path and annotation issues below are visible in the public route
  code and are not covered by equivalent adversarial tests.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, setup, endpoints, headers, matrix,
  and evidence.** The documentation records Claude Code 2.1.207, Codex CLI
  0.144.1, source links, direct/provider setup, endpoint/header behavior,
  snapshot authority, compact schemas, raw classifications, and failure
  semantics. The documentation now accurately describes the direct compact
  path, but it still presents provider compact validation/metrics as covered
  by the broad native matrix without equivalent negative provider fixtures.

- [x] **Criterion 2 — credential-free black-box public Axum harness.** The
  harness now has 40 tests (39 passed and one ignored), uses the production
  Axum router and ephemeral loopback upstream fixtures, and exercises direct
  Copilot regular/compact paths, provider aliases, Messages/web authority,
  native SSE, carriers, lifecycle, raw variants, authentication, and metrics.
  Normal tests are credential-free, external-network-free, and do not use port
  4141.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The
  previous web model fallback, usage-detail merge, optional IDs, nested
  object/null validation, raw variant classification, and end-turn semantics
  now pass. The web output assertion still mishandles optional annotation
  assertions: `parse_web_output_assertion` turns any present non-null
  `annotations` array into `Some(...)`, even when canonicalization removes
  every unknown annotation. A missing/`null` annotations field becomes
  `None`, so an otherwise equivalent unknown-only or empty annotation array
  can cause a false conflict and an HTTP error. This contradicts the declared
  “unknown annotation extensions are ignored”/“omission/null makes no
  assertion” policy.

- [ ] **Criterion 4 — Codex Responses, continuation, optional items,
  compaction, and provider/direct transport.** Direct Copilot regular and
  compact paths now use separate buffered contracts, preserve exact bytes and
  allowlisted headers, validate malformed bodies/usage/output, return 502 for
  malformed successful upstream bodies, and record direct compact usage.
  However, `handle_provider_compact` remains a weaker implementation: it
  buffers as raw `Value`, checks only that `output` is an array, and does not
  validate compact output item shapes or usage counters. It also does not
  create or record a provider compact usage event. Thus the same documented
  Codex compact contract differs between direct Copilot and provider routes.

- [ ] **Criterion 5 — native JSON/SSE contracts, exact ordering, errors, and
  termination.** Direct regular/compact JSON and native SSE remain green and
  direct malformed/oversized/status/error fixtures now produce the expected
  OpenAI semantics. Provider compact can still return HTTP 200 for a malformed
  known compact item or inconsistent usage because it only checks the output
  container. This violates the requirement that malformed upstream content
  not become a successful native response and leaves provider/direct failure
  semantics inconsistent.

- [x] **Criterion 6 — authentication, routing, provider-only mode, model
  discovery, and aliases.** Existing gateway/provider authentication,
  provider-only startup, model discovery, mappings, direct Copilot setup,
  provider aliases, unsupported model/route errors, and canary behavior remain
  green.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  new direct tests cover output-only compact success, exact null/unknown bytes,
  allowlisted headers, malformed/wrong/oversized compact bodies, upstream
  statuses, metrics, regular direct responses, and malformed regular bodies.
  The web authority suite covers most field combinations. Missing coverage
  remains for:

  - provider compact malformed output item and inconsistent usage responses;
  - provider compact token-usage recording and endpoint metrics;
  - empty versus absent and unknown-only versus absent annotation arrays;
  - provider/direct parity for all compact failure cases.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The direct
  compact documentation is now supported by the new fixtures. The broader
  native/compact claim still implies consistent validation and handling across
  provider branches, but provider compact does not apply the `ResponsesCompactResult`
  contract or record provider usage. The nested output authority table also
  claims unknown annotation extensions do not create false conflicts, which
  the current `Some(empty)` versus `None` representation can violate.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** Direct
  buffering, safe headers, 16 MiB bounds, sanitized 502 errors, raw-byte
  preservation, usage validation, and prior stream hardening remain intact.
  Remaining gaps:

  1. `handle_provider_compact` validates only `output.is_array()` and otherwise
     forwards malformed known items/usage as a successful response.
  2. The provider compact branch has no token-usage recorder, despite the
     direct compact branch adding a `responses_compact` endpoint.
  3. Web annotation assertion construction preserves `Some([])` for an empty
     or unknown-only present array, while omission/null is `None`; merge logic
     treats those as different assertions even though canonical citation
     semantics are identical.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS
  - `cargo clippy --all-targets -- -D warnings` — PASS
  - `cargo build --verbose` — PASS
  - `cargo test --verbose` — PASS
  - `cargo deny check` — PASS, with the repository's existing non-fatal
    unmatched-license/duplicate-dependency warnings
  - `cargo test --test client_compatibility --verbose` — PASS, 39 passed and
    1 ignored
  - `cargo test --test client_compatibility installed_codex_cli_smoke -- --ignored
    --nocapture` — PASS; Codex 0.144.1 loopback-only canary

## Issues Found

### Blocking: provider compact does not enforce the compact response contract

The direct Copilot route now parses `ResponsesCompactResult`, validates the
output item union and usage counters, preserves exact bytes, records usage, and
returns sanitized 502 responses for malformed successful upstream bodies.

The provider compact route in `src/routes/responses/compact.rs` instead reads
raw bytes, parses only a generic `Value`, checks only that `output` is an
array, and immediately returns the original body. A provider compact response
such as `{"output":[{"type":"compaction"}]}` or
`{"output":[],"usage":{"input_tokens":2,"output_tokens":1,"total_tokens":9}}`
would therefore be returned as HTTP 200 even though the direct Codex route
rejects the same known-shape violations. The route also never creates a token
usage recorder for provider compact responses.

This violates direct/provider parity, the compact contract, malformed-response
failure semantics, and the requested metrics audit. It is not covered by the
provider compact tests, which only assert a valid body.

### Blocking: optional annotation assertions can create false web conflicts

The declared authority table says annotation omission/null makes no assertion,
and unknown annotation extensions are ignored. `parse_web_output_assertion`
currently sets `annotations: Some(canonical_web_annotations(...))` whenever
the raw field is present and non-null. That includes `[]` and arrays
containing only unknown annotation types, which canonicalize to `Some([])`.
Another snapshot with the same text and omitted/null annotations produces
`None`; the merge code treats `Some([])` versus `None` as a conflict.

The result is an error for semantically equivalent web-search snapshots and
violates the documented unknown-field/optional-field behavior. Public tests
cover URL citations and null/omitted shapes but do not cover empty or
unknown-only annotation arrays.

### Additional coverage gap: provider compact metrics and malformed-body parity

The direct branch now exposes `responses_compact` token usage and status-class
metrics. The provider compact branch has no analogous usage event and only the
shared upstream forwarding path's incidental metrics. Provider compact
malformed JSON/oversized bodies are handled, but malformed known output/usage
values are not. The authority and native feature documentation should not
claim one consistent compact contract until those branches are aligned.

## What Must Be Fixed

1. Parse provider compact bodies with the same output-only
   `ResponsesCompactResult` validator, preserve exact bytes/allowlisted
   headers, validate usage, and emit a provider `responses_compact` usage
   record.
2. Add public provider compact fixtures for malformed output items,
   inconsistent/malformed usage, exact null/unknown preservation, and metrics.
3. Treat an empty or unknown-only canonical annotation array as no assertion
   (or explicitly define/test it as an assertion), so it cannot conflict with
   missing/null annotations under the stated authority policy.
4. Re-run all accumulated Claude/Codex carriers, framing, lifecycle, scalar,
   raw variant, web authority, compaction, routing/auth, native SSE/JSON,
   hardening, quality-gate, and canary checks.
