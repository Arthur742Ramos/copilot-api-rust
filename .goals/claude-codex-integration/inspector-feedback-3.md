# Inspector Feedback — Iteration 3

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, both prior Inspector feedback files, current status,
  and the complete accumulated diff from
  `8b7472013665b168737dbb055d9f98f4f735b6d5` through Builder commit
  `5a00786a5c3f87ef1cd45d2365f149632549127f`.
- Re-verified the audited releases:
  - `gh api repos/anthropics/claude-code/releases/latest --jq .tag_name` →
    `v2.1.207`; installed `claude --version` →
    `2.1.207 (Claude Code)`.
  - `gh api repos/openai/codex/releases/latest --jq .tag_name` →
    `rust-v0.144.1`; installed `codex --version` → `codex-cli 0.144.1`.
- Re-read the complete Codex `ResponseItem` schema at
  `44918ea10c0f99151c6710411b4322c2f5c96bea`. The `Reasoning` response item
  permits optional `id` and `encrypted_content`, and its required summary
  text is a string with no non-empty constraint.
- Independently ran the four-carrier public Messages→Responses test, the
  Codex optional-item and id-less compaction continuation tests, the library
  carrier tests, all required quality gates, and the installed loopback
  Codex canary. No paid provider, external provider, or port 4141 was used.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, setup, endpoints, headers, and
  evidence matrix.** The documentation now uses reproducible release tags,
  installed CLI versions, Codex source, the TypeScript reference commit, and
  checked-in boundary-test references. It records the client setup, native
  endpoints, headers, provider mappings, and intentional HTTP-only transport.
- [x] **Criterion 2 — credential-free black-box public boundary harness.**
  The harness enters through the real Axum router, uses ephemeral loopback
  fixtures and fake credentials, captures forwarded JSON/SSE, avoids port
  4141, and covers both clients. Iteration 3 adds the four optional reasoning
  combinations through the public provider route.
- [ ] **Criterion 3 — complete Claude Code Messages compatibility.**
  **Blocking failure remains in a different carrier case.** The new
  `claude_optional_reasoning_carriers_cross_public_messages_boundary` test
  now proves all four `rs1#...`/legacy request carriers reach a typed
  Responses reasoning item with both optional fields preserved. Legacy
  `encrypted@id` behavior also remains covered.

  However, the non-stream Responses-to-Anthropic path still drops a valid
  opaque reasoning item when the response contains a non-empty summary array
  whose text is empty:

  ```json
  {
    "type": "reasoning",
    "id": "rs-id",
    "summary": [{"type": "summary_text", "text": ""}],
    "encrypted_content": "enc"
  }
  ```

  `extract_reasoning_text` returns an empty string after joining/trimming the
  summary, and `map_output_to_anthropic_content` only emits the thinking block
  when that string is non-empty. A direct probe against this HEAD returned
  `empty-summary-nonstream-content=[]`, losing the id/encrypted carrier.
  The public provider Messages route reaches this code through
  `respond_responses_provider_messages_json`; this is not an unreachable
  helper-only concern. The stream translator has separate placeholder logic,
  so non-stream and stream behavior are inconsistent for this valid shape.
- [x] **Criterion 4 — Codex Responses and compaction compatibility.**
  The iteration-2 optionality fixes remain intact. The public boundary loop
  covers typed optional `reasoning`, `compaction`, image, function/tool-search
  fields and raw-preserved local/custom/web/image/context variants. The public
  compaction test obtains an id-less compact item and submits that exact item
  on the next Responses request; upstream capture proves it is accepted and
  preserved.
- [x] **Criterion 5 — native JSON/SSE contracts and terminal behavior.**
  Existing tests still verify Anthropic versus OpenAI envelopes, deterministic
  event order, preserved call ids/arguments, one terminal event, malformed
  frame termination, premature EOF failure, and incomplete Responses
  terminals. Iteration 3 does not weaken these native boundary guards.
- [x] **Criterion 6 — authentication, routing, provider-only mode, and
  discovery.** Provider/model mappings, `/v1/models`, client authentication,
  provider credential replacement, explicit unsupported route/model errors,
  and provider-only startup behavior remain covered by source and public
  tests. The new carrier test reaches the configured provider rather than
  stopping at local translation.
- [ ] **Criterion 7 — regression tests for every compatibility claim.**
  The four request-carrier combinations now have a strong public regression,
  but there is no public or unit regression for the valid non-stream output
  item with an empty summary text and an opaque carrier. The direct probe
  demonstrates data loss that the current green suite does not catch.
- [ ] **Criterion 8 — documentation claims are fully evidenced.**
  The iteration-3 documentation accurately adds the four request-carrier
  combinations, but it still claims optional reasoning/compaction carriers
  round-trip with non-stream and stream support. The non-stream empty-summary
  reasoning carrier is not preserved, so that broad support claim remains
  overstated until the path is fixed and tested.
- [ ] **Criterion 9 — surgical, lossless, hardened implementation.**
  Admission, auth, response-size, retry, and internal-error hardening remains
  intact, and Codex typed/raw field preservation is surgical. Nonetheless, a
  valid known reasoning item is silently omitted on a public non-stream
  provider translation path, violating the lossless/no-silent-fallback
  requirement.
- [x] **Criterion 10 — quality gates.** All required commands passed:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --verbose`
  - `cargo test --verbose`
  - `cargo deny check`

  Additional checks passed:
  `cargo test --test client_compatibility --verbose` (5 passed, 1 ignored),
  `cargo test --lib --verbose`, and
  `cargo test --test client_compatibility installed_codex_cli_smoke
  -- --ignored --nocapture` (1 passed). `cargo deny check` retained existing
  non-fatal unmatched-license/duplicate-dependency warnings but returned
  success.

## Issues Found

### Blocking: non-stream reasoning carriers disappear for empty summary text

The audited Codex schema permits `summary_text.text` to be an empty string.
When `encrypted_content` or `id` is present, the Anthropic wire response must
still carry the opaque reasoning signature so Claude Code can continue the
conversation. The non-stream translator currently drops the entire item after
trimming its empty summary. The stream translator follows a different path,
making the public API inconsistent.

## What Must Be Fixed

1. Make non-stream reasoning extraction emit the established thinking
   placeholder whenever the summary text aggregates to empty but the reasoning
   item carries an id or encrypted content (and keep the signature).
2. Align stream placeholder detection with the same aggregate-empty rule so
   stream and non-stream Anthropic behavior agree.
3. Add a public provider-fixture regression for the empty-summary reasoning
   response and assert the returned Anthropic thinking block/signature, then
   update the matrix only after that regression passes.
