# Inspector Feedback — Iteration 10

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior Inspector feedback, and the current status.
- Inspected the complete accumulated implementation from the initial SHA
  `8b747201665b168737dbb055d9f98f4f735b6d5` through Builder commit
  `bfb495d8418de266bcc13feecdee6c20fb387179`.
- Re-verified the installed client identities:
  - `claude --version` → `2.1.207 (Claude Code)`
  - `codex --version` → `codex-cli 0.144.1`
- Re-read the Codex 0.144.1 source at commit
  `44918ea10c0f99151c6710411b4322c2f5c96bea`, including
  `codex-api/src/sse/responses.rs` and `protocol/src/models.rs`.
- Ran the public compatibility suite, all required repository gates, and the
  ignored installed-Codex loopback canary. No product code was changed by this
  inspection.
- Ran an independent deterministic probe against the public translation
  modules. It showed that an explicit `status: null` on an otherwise valid
  terminal is rejected, while a valid unknown output variant is translated to
  an empty successful Anthropic turn.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** `docs/claude-code-codex-compatibility.md` records Claude Code
  2.1.207, Codex CLI 0.144.1, the Codex source commit, base-URL setup,
  `/v1/messages`, `/v1/responses`, `/v1/responses/compact`, headers, provider
  routing, and the feature/transport matrix. The source links and installed
  version commands are reproducible. The remaining documentation overclaims
  some output behavior; that is recorded under Criterion 8.

- [x] **Criterion 2 — credential-free black-box public Axum harness.**
  `tests/client_compatibility.rs` enters through `build_router()`, uses
  `tower::ServiceExt::oneshot`, and sends provider traffic to an ephemeral
  loopback Axum fixture. The fixture covers both Claude-shaped Messages and
  Codex-shaped Responses, compaction, carriers, lifecycle, malformed frames,
  usage, authentication, routing, model discovery, web search, and native
  Responses. Normal tests use fake keys, no external network, no paid provider,
  and no port 4141. The installed canary is ignored and opt-in.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.**
  The earlier Claude hardening and the new paired scalar tests pass, but the
  provider bridge still has client-visible loss/failure cases:

  1. A valid Codex terminal carrying `status: null` is rejected by the
     Anthropic translation. Codex's `ResponseCompleted` struct has no `status`
     field at all, and its `ResponsesStreamEvent` keeps `response` as an
     untyped `Value`; serde therefore ignores an extra/null status. The bridge
     instead calls `validate_terminal_status`, where any present value whose
     `as_str()` is not the expected status—including `null`—fails. The same
     over-validation exists for `response.created.status`.
  2. Valid output variants that are retained as `ResponseOutputItem::Other`
     are silently omitted by the Messages translator. The probe sent valid
     `web_search_call` and `image_generation_call` items and received
     `Ok(AnthropicResponse { content: [], stop_reason: Some("end_turn") })`.
     The stream path has the equivalent `Other(_) => {}` behavior. This is a
     fabricated successful empty turn rather than an explicit unsupported-output
     error or lossless handling.

- [ ] **Criterion 4 — Codex Responses, continuation, optional items, and
  compaction.** Native `/v1/responses` forwarding, statusless terminals,
  strict usage, optional continuation items, reasoning carriers, compaction
  continuation, and the installed Codex canary remain green. However, the
  translated Claude/provider path does not implement the Codex serde contract
  consistently for nullable/unknown optional terminal fields, and the
  Messages bridge drops raw output variants. The native path is not a
  substitute for the failing provider-to-Anthropic path.

- [ ] **Criterion 5 — native JSON/SSE contracts and exact terminal behavior.**
  The direct native Responses route continues to preserve OpenAI JSON/SSE and
  its lifecycle guard passes the existing statusless tests. The translated
  path is not fully contract-native:

  - `status: null` produces an Anthropic error even though Codex does not model
    that field and accepts the event.
  - Unknown output variants produce a successful `end_turn` with no content.
  - Web-search collection can accept contradictory full-created versus
    terminal values (see below), so it can emit a successful reconstructed
    Anthropic response after a conflict that should fail closed.

- [x] **Criterion 6 — authentication, routing, provider-only mode, and model
  discovery.** The public tests still cover gateway authentication, provider
  credentials, `provider/model` routing, aliases, `/v1/models`, explicit
  unsupported model/route errors, and provider-only startup. The installed
  Codex canary exercises a scratch loopback port and an isolated `CODEX_HOME`.
  No regression was found in this criterion.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  suite expanded to 31 tests (30 passed and one ignored), but it does not test
  the newly exposed cases:

  - explicit null terminal/created status, which is source-valid under the
    audited Codex serde contract;
  - a full `response.created` output and a partial terminal containing a
    different valid output;
  - valid but conflicting created/terminal usage counters;
  - unknown/raw output variants through both the JSON and SSE Messages
    translation paths.

  The existing web tests cover terminal-only output and malformed usage, but
  not valid-value conflicts between the two response snapshots.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The
  documentation claims that “unknown item variants remain raw/lifecycle-
  preserved” and that web search “partial terminals reconcile without output
  loss,” with paired conflict evidence. Raw variants are preserved by the
  native Responses serde/forwarding path but are dropped by the
  Responses-to-Anthropic provider bridge, as independently observed. The web
  tests do not establish conflict detection for a full-created output/usage
  snapshot versus a partial terminal; the implementation accepts those
  contradictions. These claims must either be fixed or narrowed after the
  implementation is corrected.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** Earlier
  bounds, strict scalar validation, lifecycle cleanup, request flattening, and
  native error hardening remain intact. The current implementation still
  violates the explicit “avoid silent fallbacks” and losslessness conventions:

  1. `ResponseOutputItem::Other(_)` is accepted by validation and then ignored
     in both `map_output_to_anthropic_content` and the streamed item renderer,
     allowing a valid unknown variant to become an empty `end_turn`.
  2. `build_web_search_responses_stream_result` validates only created/terminal
     identity fields (`id`, and supplied `model`/`object`). When no lifecycle
     items were collected, it chooses terminal `output` without comparing it
     with a non-empty created `output`, and it never compares valid created and
     terminal usage counters. This permits a contradictory but well-typed
     response to succeed.
  3. The terminal-status validator treats an explicitly null optional/unknown
     status as a conflict instead of applying the audited client serde
     semantics.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS
  - `cargo clippy --all-targets -- -D warnings` — PASS
  - `cargo build --verbose` — PASS
  - `cargo test --verbose` — PASS
  - `cargo deny check` — PASS, with the repository's existing non-fatal
    unmatched-license/duplicate-dependency warnings
  - `cargo test --test client_compatibility --verbose` — PASS, 30 passed and
    1 ignored
  - `cargo test --test client_compatibility installed_codex_cli_smoke -- --ignored
    --nocapture` — PASS; Codex 0.144.1 used a loopback-only scratch proxy,
    fake credentials, and isolated `CODEX_HOME`

## Issues Found

### Blocking: nullable/unknown terminal status is rejected despite Codex serde semantics

Codex 0.144.1's `ResponseCompleted` contains only `id`, optional `usage`, and
optional `end_turn`; it does not contain `status`. Its stream event keeps the
response as a `serde_json::Value`, so an extra `status: null` is ignored by the
client parser. The bridge's `validate_terminal_status` rejects any present
status whose value is not the expected string, including null. The independent
probe produced one Anthropic `error` after `response.created` for
`response.completed` with `status: null`, while the same event with
`status: "completed"` completed normally. `response.created.status: null` is
similarly rejected.

This is a client-visible incompatibility in the audited Responses-to-Messages
translation and is not covered by the current fixtures.

### Blocking: full-created web-search snapshots are not reconciled with partial terminals

`build_web_search_responses_stream_result` validates terminal identity and
terminal usage, then builds output as follows: if no lifecycle output items were
collected, terminal `output` wins when present, otherwise created `output` is
used. It does not compare a non-empty created `output` against a supplied
terminal `output` in that branch. It also does not compare valid created usage
against valid terminal usage. A full `response.created` containing output or
usage followed by a partial terminal with a different valid output or usage can
therefore produce a successful reconstructed Anthropic response instead of one
error. Existing “terminal output” and “usage conflict” fixtures do not exercise
these valid-value contradictions.

### Blocking: raw output variants silently fabricate an empty successful turn

`ResponseOutputItem::Other` is intentionally used to preserve source variants
that the proxy does not inspect, but the Anthropic result mapper handles it as
`Other(_) => {}`. The stream mapper has the same no-op branch. An independent
probe using valid `web_search_call` and `image_generation_call` output items
returned a successful Anthropic response with no content and `stop_reason:
end_turn`. This violates the goal's no-silent-fallback requirement and the
documentation's raw/lifecycle-preservation claim. The bridge must preserve an
appropriate representation or fail explicitly; it must not claim successful
completion after discarding the only output.

## What Must Be Fixed

1. Treat absent and explicit-null terminal/created status consistently with the
   audited Codex serde contract, while continuing to reject wrong non-null
   status values where the bridge intentionally validates them.
2. Reconcile full `response.created` and partial terminal snapshots for all
   authoritative fields used by web-search reconstruction, including valid
   output arrays and usage counters. Reject any conflict before JSON or
   synthetic SSE success, and add public fixtures for both JSON and SSE.
3. Do not silently discard `ResponseOutputItem::Other` in the Claude/provider
   bridge. Preserve a documented representation or emit one protocol-native
   unsupported-output error, with paired JSON/SSE regression tests and
   documentation narrowed if necessary.
4. Re-run the complete accumulated carrier, framing, lifecycle, terminal,
   optional-item, compaction, authentication/routing, native Responses,
   hardening, quality-gate, and loopback-canary checks after the fixes.
