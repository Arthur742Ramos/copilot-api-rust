# Inspector Feedback — Iteration 11

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, current status, and
  Builder commit `9a23207e4fcf2629a01554a6527026fda009aee1`.
- Re-read the audited Codex 0.144.1 source at
  `44918ea10c0f99151c6710411b4322c2f5c96bea`, especially
  `codex-api/src/sse/responses.rs` and `protocol/src/models.rs`.
  `ResponseCompleted` contains required `id`, optional `usage`, and optional
  `end_turn`, but no `status`; response snapshots are carried as untyped
  `Value`s. Codex continuation/output variants such as `WebSearchCall` have
  optional IDs/statuses, and usage detail structs are optional.
- Verified installed versions:
  - `claude --version` → `2.1.207 (Claude Code)`
  - `codex --version` → `codex-cli 0.144.1`
- Inspected the new three-way snapshot, raw-variant classification, and native
  passthrough implementation and tests.
- Ran the complete public suite and all required gates. Added no repository
  product changes.
- Ran independent probes beyond checked-in tests through the public provider
  Messages route and the typed Responses boundary:
  - a `response.created`/`response.completed` web-search stream with no model in
    either snapshot returned HTTP 500 instead of a reconstructed Anthropic
    response;
  - a created usage object with valid cached/reasoning detail objects followed
    by a terminal usage object with the same required counters but omitted
    optional detail objects returned HTTP 500;
  - a created web-search item with an ID followed by an otherwise equivalent
    terminal item with the optional ID omitted returned HTTP 500;
  - conflicting `incomplete_details` objects on completed web-search snapshots
    were accepted and returned HTTP 200;
  - serializing a native non-stream `ResponsesResult` with explicit null
    response fields omitted those fields instead of preserving the wire shape.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, configuration, endpoints, headers,
  matrix, and evidence.** The compatibility documentation records the exact
  Claude/Codex versions, Codex source commit, setup, native routes, provider
  routes, headers, and feature matrix. It now documents the intended raw-output
  classification and three-snapshot web-search policy. It nevertheless claims
  that missing model values are filled in the web flow, which the public probe
  disproves; this documentation mismatch is listed under Criterion 8.

- [x] **Criterion 2 — credential-free public Axum harness.** The harness now
  has 34 tests (33 passed and one ignored), uses the production Axum router,
  ephemeral loopback fixtures, fake credentials, and no port 4141 or external
  provider. It exercises Claude Messages, native Responses, compaction,
  optional continuation items, carriers, scalar failures, raw variants,
  lifecycle, web search, routing, authentication, and the canary command.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** Most
  accumulated Claude behavior and the new raw-output failure policy pass.
  The dedicated web-search Messages path still rejects a source-valid
  model-less created/terminal stream because its merged `ResponsesResult`
  requires a non-empty `model` even though the audited Codex event contract
  permits `response.created` to contain only `id`. It also rejects valid
  optional usage details and optional web-search IDs when they disappear from
  a partial terminal. These are client-visible failures in a supported
  Messages workflow.

- [ ] **Criterion 4 — Codex Responses, continuation, optional items, and
  compaction.** Native `/v1/responses` JSON/SSE raw variants, statusless
  terminals, reasoning/carriers, usage validation, continuation fields,
  compaction, routing, and the installed canary remain green. However, the
  Responses-to-Anthropic web-search collector does not carry the already known
  requested model into the final result when both snapshots omit it, and it
  treats optional usage details as conflicting when the terminal merely omits
  them. This does not match the audited Codex serde contract.

- [ ] **Criterion 5 — native JSON/SSE contracts, ordering, and terminal
  behavior.** The regular Messages stream now fails unsupported raw output
  explicitly, nullable status is accepted, and repeated terminal behavior is
  covered. The web-search stream still produces an HTTP 500 for valid
  model-less/optional-detail combinations rather than an Anthropic success.
  The native non-stream Responses path also reserializes typed results and
  elides explicit null fields, so it is not a fully lossless native passthrough
  even though native SSE preserves raw frames.

- [x] **Criterion 6 — authentication, routing, provider-only mode, model
  discovery, and aliases.** Existing provider-only, gateway-key, provider
  credential, `/v1/models`, alias, unsupported-model, and unsupported-route
  tests pass. The canary uses an isolated `CODEX_HOME`, fake credentials, and a
  scratch loopback listener.

- [ ] **Criterion 7 — regression tests for every compatibility claim.** The
  Builder added broad raw-variant, nullable-status, output-snapshot, metadata,
  usage-conflict, terminal-order, and native passthrough tests. Missing
  coverage remains for:

  - both created and terminal model absent in the web collector;
  - created usage details present while terminal optional details are omitted;
  - created web-search ID present while terminal ID is absent, and the inverse;
  - conflicting `incomplete_details`/other nested snapshot fields;
  - exact native non-stream preservation of explicit null fields.

  The independent probes demonstrate that these are behavioral gaps, not just
  test omissions.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The docs
  state that the web collector fills absent/null model values and reconciles
  optional snapshot fields, but the model-less public stream fails before
  reconstruction and optional usage detail omission fails reconciliation. The
  docs also say repeated nested/metadata values are reconciled, while
  `incomplete_details` is not in the reconciliation field set and a probe with
  conflicting completed-snapshot details succeeds. The native matrix says raw
  output is preserved; native SSE is raw-preserving, but non-stream typed
  serialization drops explicit null fields without documenting that divergence.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** Prior
  bounds, unknown-key flattening, stream cleanup, protocol-native errors,
  admission, and remote-auth hardening remain intact. The remaining
  correctness/losslessness problems are:

  1. `build_web_search_responses_stream_result` eventually deserializes a
     merged map into `ResponsesResult`, whose `model: String` is required.
     The snapshot validators allow model absence, but the merge cannot finish
     without it and does not receive the requested model fallback.
  2. `reconcile_snapshot_usage` compares `ValidatedResponsesUsage` as a whole.
     `Some(cached/reasoning detail)` versus `None` is treated as a conflict,
     although Codex declares each detail object optional and a partial terminal
     may omit it.
  3. `canonical_web_output_item` compares optional item IDs as exact presence/
     absence, so a source-valid ID omission in a later snapshot fails instead
     of retaining the known stable ID or explicitly documenting the stricter
     policy.
  4. `incomplete_details` and other nested snapshot fields are not reconciled;
     conflicting values can be accepted and the created value silently wins.
  5. Native non-stream `ResponsesResult` serialization uses
     `skip_serializing_if = Value::is_null` for several response fields, which
     collapses explicit `null` into absence. The native SSE path does not have
     this loss.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS
  - `cargo clippy --all-targets -- -D warnings` — PASS
  - `cargo build --verbose` — PASS
  - `cargo test --verbose` — PASS
  - `cargo deny check` — PASS, with the repository's existing non-fatal
    unmatched-license/duplicate-dependency warnings
  - `cargo test --test client_compatibility --verbose` — PASS, 33 passed and
    1 ignored
  - `cargo test --test client_compatibility installed_codex_cli_smoke -- --ignored
    --nocapture` — PASS; loopback-only Codex 0.144.1 canary

## Issues Found

### Blocking: web-search result cannot complete when both snapshots omit model

The audited Codex `ev_response_created` fixture contains only a non-empty
response ID, and `ResponseCompleted` does not model `model`. The normal
Responses stream translator has a requested/resolved model fallback, but
`collect_web_search_responses_stream_result` and
`build_web_search_responses_stream_result` do not receive that fallback.

The new snapshot validator intentionally accepts absent/null model. However,
when both `response.created` and the partial terminal omit it, the merged map
is deserialized into `ResponsesResult`, whose `model: String` is required.
An independent public `/:provider/v1/messages` probe with valid web-search
output, valid usage, absent/null statuses, and no model in either snapshot
returned HTTP 500 with a sanitized Anthropic API error rather than the expected
server-tool/result/text response.

This is a direct failure of the documented Codex model-less behavior and a
client-critical web-search workflow.

### Blocking: optional usage detail omission is treated as a conflict

Codex's `ResponseCompletedUsage` makes
`input_tokens_details` and `output_tokens_details` optional. A full created
snapshot may contain those details while a partial terminal supplies only the
required input/output/total counters. The required counters can be identical
and valid, while the terminal's omitted optional details simply provide no
contradictory assertion.

The current `reconcile_snapshot_usage` compares the normalized optional detail
values directly. An independent public probe with valid created cached/reasoning
details and a terminal carrying identical required counters but no detail
objects returned HTTP 500. The collector should merge optional fields according
to presence/authority rather than treating omitted detail objects as a conflict,
and should add the reverse/null combinations to the public matrix.

### Blocking: optional web-search item IDs cannot disappear across snapshots

Codex models `WebSearchCall.id` as `Option<String>`. The current canonical
comparison includes the ID only when present, so a created item with
`id: "web-1"` and an otherwise identical terminal item with the optional ID
omitted compare unequal. The independent public probe returned HTTP 500.
The inverse (created omission followed by terminal ID) is rejected similarly.

If the intended policy is strict identity preservation, the implementation must
document and test that intentional rejection as a source-compatible divergence.
If the intended policy is the documented optional-null/absence normalization,
the known ID must be retained while the missing snapshot field is filled.
Either way, the current behavior is not an independently evidenced contract
for all source-valid optional shapes requested by this iteration.

### Additional reconciliation gap: nested terminal details can conflict silently

The three-way reconciler handles model, object, output, output text, usage, and
metadata, but not `incomplete_details` (or comparable nested response extras).
A public probe with differing `incomplete_details.reason` objects in otherwise
valid completed created/terminal snapshots returned HTTP 200 and a successful
Anthropic response. This may be intentionally client-ignored under Codex's raw
serde behavior, but the user-requested nested-field audit and current
documentation claim broader reconciliation. The field must either be included
in an explicit conflict policy or clearly classified as ignored.

### Native non-stream null-field preservation is not lossless

The native stream path forwards raw SSE values, but native non-streaming
Responses deserializes into `ResponsesResult` and serializes it again. A direct
probe with explicit null `object`, `created_at`, `output_text`, `usage`,
metadata, and other nullable fields serialized only:
`{"id":"r","model":"gpt","output":[],"status":"completed"}`.
Absent and explicit-null are equivalent to Codex's ignored extra-field serde
behavior, but they are not byte- or shape-preserving native passthrough. This
must be fixed or documented as an intentional native JSON divergence under the
goal's unknown-field/losslessness requirement.

## What Must Be Fixed

1. Pass the requested/resolved model into web-search stream reconciliation and
   use it when both source snapshots omit/null model.
2. Merge optional usage detail objects from created/terminal snapshots without
   treating omission as a conflict; reject only contradictory present values or
   malformed detail objects.
3. Define and test the source-compatible policy for optional web-search item
   IDs across created, lifecycle, and terminal snapshots, retaining a known ID
   when a later partial snapshot omits it if that is the chosen policy.
4. Reconcile or explicitly classify `incomplete_details` and other nested
   snapshot extras, and add adversarial public fixtures for matching,
   missing/null, and conflicting values.
5. Preserve explicit nulls in native non-stream Responses or document and test
   the intentional absent/null normalization.
6. Re-run all accumulated carrier, framing, lifecycle, scalar, optional-item,
   compaction, authentication/routing, native Responses, web-search,
   hardening, quality-gate, and canary checks.
