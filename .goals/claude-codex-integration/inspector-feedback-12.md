# Inspector Feedback — Iteration 12

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  Builder commit `66a5652c289d0eb3c8ecb7ee5eeb377c091815ed`.
- Re-read the audited Codex 0.144.1 source at
  `44918ea10c0f99151c6710411b4322c2f5c96bea`, including the
  `ResponseCompleted`/`ResponseCompletedUsage` serde definitions and
  `ResponseItem::WebSearchCall` optional fields. The client models terminal
  `id` as required, usage/details/end-turn as optional, does not model
  `status`, and treats response snapshots as untyped values around those
  fields.
- Verified installed versions:
  - `claude --version` → `2.1.207 (Claude Code)`
  - `codex --version` → `codex-cli 0.144.1`
- Inspected the Builder's declared snapshot authority table, nested item
  merge functions, native buffered-response changes, compact routing, and
  documentation.
- Ran the public compatibility suite, all repository gates, and the ignored
  loopback Codex canary. No repository product code was changed by this
  inspection.
- Ran independent loopback/public-boundary probes beyond checked-in tests:
  - created/terminal model fallback now succeeds;
  - created usage details present with terminal required counters and omitted
    optional details now succeeds;
  - optional web-search item ID omission now merges and succeeds;
  - conflicting nested `incomplete_details` now fails;
  - malformed string `incomplete_details` in both snapshots still succeeds and
    is silently discarded from the reconstructed Anthropic response;
  - a valid id-less compact response through the direct Copilot
    `/v1/responses/compact` path returns HTTP 500 `server_error`.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, setup, endpoints, headers, matrix,
  and evidence.** The documentation now records the exact client versions,
  Codex source, setup, endpoint/header matrix, field-authority table, raw
  classification, and native byte-forwarding policy. The documented claim that
  compact output is supported across the public route is not evidenced for the
  direct Copilot transport; this is recorded under Criterion 8.

- [x] **Criterion 2 — credential-free black-box public Axum harness.** The
  suite now contains 37 tests (36 passed and one ignored), uses the production
  Axum router and deterministic ephemeral loopback fixtures, and covers
  Claude Messages, Codex Responses, compaction, carriers, lifecycle,
  authority combinations, raw variants, native null shape, auth/routing,
  errors, and the opt-in installed Codex canary. Normal tests use no external
  network, paid provider, or port 4141.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The
  previous web model/usage/ID gaps are fixed, and raw unsupported variants now
  fail explicitly. A malformed `incomplete_details` value is still accepted
  by the web-search bridge even though the authority table classifies
  `incomplete_details` as an optional stable semantic assertion. The field is
  copied into the intermediate Responses result but never shape-validated,
  so a malformed upstream field can be silently ignored while Claude receives
  success.

- [ ] **Criterion 4 — Codex Responses, continuation, optional items,
  compaction, and native transport.** Continuation items, reasoning,
  compaction through provider routing, raw native output, terminal handling,
  and the installed canary remain green. However, direct Copilot compaction is
  still broken:

  `create_http_responses` always parses non-stream bodies with
  `parse_buffered_responses::<ResponsesResult>`. `ResponsesResult` requires
  `id`, `model`, and `status`, while the valid unary compaction shape contains
  only an output array (including id-less `compaction`) plus optional extensions.
  An independent direct loopback probe configured `copilot_api_url` and a fake
  Copilot token, then posted to `/v1/responses/compact`; it received HTTP 500
  instead of the exact compact bytes. Provider-alias compact forwarding passes,
  but it does not cover the documented Codex direct configuration.

- [ ] **Criterion 5 — native JSON/SSE contracts, exact ordering, errors, and
  termination.** Native non-stream Responses/provider branches now retain
  original bytes and tests cover null/unknown shape. The direct Copilot compact
  branch still turns a valid protocol response into an internal server error.
  In addition, malformed `incomplete_details` is allowed through the web
  collector and then omitted from the resulting Anthropic response rather than
  producing one explicit protocol-native failure. Native SSE and regular
  Messages lifecycle behavior otherwise remain green.

- [x] **Criterion 6 — authentication, routing, provider-only mode, model
  discovery, and aliases.** Existing gateway/provider authentication,
  provider-only startup, model discovery, mappings, provider routing, and
  unsupported route/model behavior remain green. The canary uses fake
  credentials, an isolated `CODEX_HOME`, and a scratch loopback listener.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  new suite covers model fallback, usage-detail merges in both directions,
  nullable fields, item ID/status/action merges, nested conflict, native raw
  bytes, and provider compaction. Missing or incomplete evidence remains for:

  - direct Copilot (non-provider-alias) `/v1/responses/compact` with the
    id-less output-only response shape;
  - malformed metadata and malformed `incomplete_details` shapes in the web
    authority matrix;
  - direct Copilot malformed compact JSON/oversized compact-body status and
    metrics behavior;
  - direct Copilot non-stream `/v1/responses` byte preservation rather than
    only the provider-alias raw fixture.

  The direct compact probe demonstrates a real failure, not just a missing
  assertion.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The
  authority table accurately documents model fallback, field-by-field usage
  merge, optional item fields, and raw ignored extras. The documentation also
  says native non-stream `/v1/responses` and `/v1/responses/compact` return
  original bytes across Copilot, Codex-provider, and generic-provider branches.
  The direct Copilot compact branch does not: it parses the body as a complete
  `ResponsesResult` before returning bytes and rejects an id-less compact
  response. The docs need either a direct compact fix and fixture or a narrower
  support statement.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The
  authority table and nested merge code are substantially clearer and prior
  lifecycle/bounds/auth hardening remains intact. Remaining gaps:

  1. The direct Copilot compact path uses the full Responses-result parser for
     an auxiliary response whose valid schema is output-only. This is a
     client-facing protocol mismatch and produces an internal 500.
  2. `metadata` and `incomplete_details` are classified as `OptionalStable`,
     but the snapshot validator only checks model/object/output/output_text and
     usage. A string `incomplete_details` is accepted, merged, and silently
     dropped by later translation. Metadata has the same absent shape gate.
  3. The direct compact parse/read failures use `HttpError::internal` in
     `create_http_responses`, yielding a generic 500 rather than the
     upstream/Bad-Gateway error semantics used by provider Responses paths.
     This should be explicitly justified or normalized for malformed/oversized
     direct upstream bodies.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS
  - `cargo clippy --all-targets -- -D warnings` — PASS
  - `cargo build --verbose` — PASS
  - `cargo test --verbose` — PASS
  - `cargo deny check` — PASS, with the repository's existing non-fatal
    unmatched-license/duplicate-dependency warnings
  - `cargo test --test client_compatibility --verbose` — PASS, 36 passed and
    1 ignored
  - `cargo test --test client_compatibility installed_codex_cli_smoke -- --ignored
    --nocapture` — PASS; Codex 0.144.1 loopback-only canary

## Issues Found

### Blocking: direct Copilot compaction rejects the valid id-less response shape

The public Codex configuration in the documentation points directly at the
proxy's `/v1` base URL. That means Codex remote compaction uses the direct
`/v1/responses/compact` route, not necessarily a `provider/model` alias.

The Builder introduced `ResponsesBufferedResult`, but
`create_http_responses` still parses every non-stream body through
`ResponsesResult`. That type requires `id`, `model`, and `status`; the compact
contract and the repository's own exact-shape fixture contain an output array
with an id-less `compaction` item and no complete response identity/status.

An independent direct loopback probe set `state.copilot_api_url` to the fixture,
installed a fake Copilot token/model catalog, and posted a valid compact request
to `/v1/responses/compact`. The route returned HTTP 500 with an OpenAI
`server_error` instead of the fixture's exact JSON bytes. The provider-alias
compact test passes because that branch already forwards raw bytes, so it does
not catch the documented direct-client path.

### Blocking: authority table permits malformed structured nested fields

`metadata` and `incomplete_details` are declared `OptionalStable` semantic
assertions, and the documentation says present conflicting values fail. The
implementation reconciles their raw `Value`s but does not validate that either
field is an object or null. A public loopback probe supplied the same malformed
string `incomplete_details` in created and terminal snapshots; the collector
returned HTTP 200 and a successful Anthropic response while silently omitting
the malformed field from the response. The equivalent malformed metadata shape
has the same unchecked path.

This is inconsistent with the authority declaration and with the goal's
malformed-content/no-silent-fallback requirement. Either these fields must be
strictly object/null validated or they must be reclassified and documented as
ignored raw extras.

### Error-semantics gap on direct malformed/oversized compact bodies

The direct Copilot `create_http_responses` path maps both bounded-read failures
and buffered JSON parse failures to `HttpError::internal`, which is a sanitized
HTTP 500. Provider Responses paths use explicit bad-gateway handling. Since the
goal requires native error/status behavior for malformed and oversized upstream
content, direct compact needs a deliberate, tested status/envelope policy rather
than the current incidental internal error.

## What Must Be Fixed

1. Parse/directly forward compact responses with their output-only schema, or
   introduce a separate buffered compact result type; preserve exact bytes and
   retain usage/size/error metrics.
2. Add direct Copilot `/v1/responses/compact` public fixtures and tests for
   valid id-less output, malformed JSON, oversized body, upstream status, and
   headers.
3. Validate `metadata` and `incomplete_details` as object/null when they are
   `OptionalStable`, or explicitly move them to `IgnoredRaw` and narrow the
   authority/docs/tests.
4. Normalize direct malformed/oversized upstream errors to the chosen native
   OpenAI/Bad-Gateway envelope and test it.
5. Re-run all accumulated carrier, framing, lifecycle, scalar, optional-item,
   raw-variant, web authority, compaction, auth/routing, native passthrough,
   quality-gate, and canary checks.
