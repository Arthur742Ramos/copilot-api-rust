# Inspector Feedback — Iteration 2

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, iteration-1 feedback, current status, and the
  complete accumulated diff from
  `8b7472013665b168737dbb055d9f98f4f735b6d5` through Builder commit
  `550f20258ca38823ec9d1b2805f176f3cd45acb9`.
- Re-verified the audited releases:
  - `gh api repos/anthropics/claude-code/releases/latest --jq .tag_name` →
    `v2.1.207`; installed `claude --version` →
    `2.1.207 (Claude Code)`.
  - `gh api repos/openai/codex/releases/latest --jq .tag_name` →
    `rust-v0.144.1`; installed `codex --version` → `codex-cli 0.144.1`.
- Re-read the Codex 0.144.1 source at
  `44918ea10c0f99151c6710411b4322c2f5c96bea`, including the complete
  `ResponseItem` union and its optional fields in
  `codex-rs/protocol/src/models.rs`.
- Independently exercised the updated public Axum compatibility harness,
  the new id-less compaction-to-next-turn flow, the complete optional-item
  provider boundary loop, the library carrier tests, and the installed
  loopback Codex canary.
- No product code was changed by this inspection; no external provider or
  paid quota was used and port 4141 was not accessed.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited identities, setup, endpoints, headers, and
  evidence matrix.** The guide now records only reproducible release-tag,
  installed-version, source-commit, and test-command evidence. It documents
  Claude Code 2.1.207 and Codex CLI 0.144.1, the `/v1/messages`,
  `/v1/messages/count_tokens`, `/v1/responses`, and
  `/v1/responses/compact` setup, client headers, provider aliases, and the
  intentional HTTP-only Responses transport. The Codex source links and
  TypeScript reference commit are valid.
- [x] **Criterion 2 — credential-free black-box public boundary harness.**
  `tests/client_compatibility.rs` still drives the production Axum router via
  `oneshot`, captures requests at an ephemeral loopback upstream, uses fake
  credentials, and avoids external networking and port 4141. It now includes
  a public id-less compaction continuation and a public loop over the audited
  optional item cases.
- [ ] **Criterion 3 — complete Claude Code Messages compatibility.**
  **Blocking failure.** Streaming Responses-to-Anthropic translation emits
  the new versioned optional-reasoning carrier (`rs1#...`) in
  `responses_stream_translation.rs` (the new test covers this), but
  `translate_assistant_message` in `responses_translation.rs` only creates a
  `ResponseInputItem::Reasoning` when `signature.contains('@')`. A valid
  versioned carrier such as
  `rs1#{"encrypted_content":null,"id":null}` has no `@` and is silently
  dropped from the next Anthropic-to-Responses request.

  A direct probe compiled against this HEAD called
  `encode_reasoning_signature(None, None)` and then
  `translate_anthropic_messages_to_responses_payload`; it reported
  `input_len=0` and `input=Some(Items([]))`. Thus the encoder/parser unit
  tests pass while the actual Claude Messages carrier workflow loses the
  reasoning item. This violates the required thinking/reasoning,
  multi-turn, and carrier behavior even though the prior ordinary
  `encrypted_content@id` path remains intact.
- [x] **Criterion 4 — Codex Responses and compaction compatibility.**
  The previous typed optionality defect is fixed. The input types now align
  with the audited Codex schema for typed variants:
  `reasoning.encrypted_content`, `compaction.id`, image `detail`, and
  tool-search ids are optional; unknown variants such as local shell,
  custom-tool, web/image, context-compaction, agent-message, and
  additional-tools items remain raw-preserved. Legacy
  `compaction_summary` is accepted and canonicalized.

  The public test
  `codex_0_144_1_optional_continuation_items_cross_provider_boundary` submits
  all those cases through `/v1/responses` and verifies the captured upstream
  body. The main Codex test obtains an id-less `compaction` item from
  `/v1/responses/compact`, submits that exact item on the next
  `/v1/responses` request, and verifies the captured continuation. These
  tests passed.
- [x] **Criterion 5 — native JSON/SSE contracts and terminal behavior.**
  The existing native Responses guard and Messages translators still assert
  native envelopes, deterministic stream ordering, preserved function-call
  ids/arguments, exactly one terminal event, malformed-frame failure, and
  premature-EOF failure. The iteration-2 changes did not weaken those paths;
  the updated compatibility and full suites pass.
- [x] **Criterion 6 — authentication, routing, provider-only mode, and model
  discovery.** The public tests continue to verify provider/model mapping,
  `/v1/models` records, client-key validation, provider credential
  replacement, explicit unknown model/route failures, and provider-only
  dispatch without unrelated GitHub initialization. The new optional-item
  cases reach the configured provider and are captured there.
- [ ] **Criterion 7 — regression tests for every compatibility claim.**
  Codex optionality and compaction continuation now have meaningful public
  regressions, but the new Claude optional-carrier tests stop at
  encoder/parser or stream-event units. There is no end-to-end
  Anthropic-message request regression proving that an `rs1#...` signature
  becomes a reasoning input item. The direct probe demonstrates the missing
  regression catches a real data-loss bug.
- [ ] **Criterion 8 — documentation claims are fully evidenced.**
  The npm-integrity evidence problem from iteration 1 is corrected, and the
  Codex optional-item table is substantially better. However, the guide
  still claims that optional reasoning carriers and Claude compaction
  carriers round-trip, while the versioned optional-reasoning carrier is
  dropped on the request-translation path. Those claims must be narrowed or
  fixed before documentation is accurate.
- [ ] **Criterion 9 — surgical, lossless, hardened implementation.**
  The Codex changes are surgical and preserve unknown/raw variants, and all
  prior admission, auth, size, retry, and internal-error hardening remains
  present. Nevertheless, the new optional-carrier path silently drops a
  valid known reasoning item, which violates the goal's lossless/no-silent-
  fallback requirement.
- [x] **Criterion 10 — quality gates.** All required gates exited successfully:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --verbose`
  - `cargo test --verbose`
  - `cargo deny check`

  Additional checks passed:
  `cargo test --test client_compatibility --verbose` (4 passed, 1 ignored),
  `cargo test --lib --verbose`, and
  `cargo test --test client_compatibility installed_codex_cli_smoke
  -- --ignored --nocapture` (1 passed). `cargo deny check` retained its
  existing non-fatal unmatched-license/duplicate-dependency warnings but
  returned success.

## Issues Found

### Blocking: versioned optional-reasoning carriers are dropped

`encode_reasoning_signature` intentionally emits `rs1#<json>` whenever either
Codex reasoning field is missing. The response stream translator emits that
carrier, but the request translator recognizes only signatures containing the
legacy `@` separator. This causes a valid Claude thinking block to disappear
from the next Responses request. The encoder/parser tests do not exercise the
translation decision that drops it.

## What Must Be Fixed

1. Make `translate_assistant_message` recognize and route the
   `OPTIONAL_REASONING_SIGNATURE_PREFIX` carrier through
   `create_reasoning_content`, while retaining legacy signature behavior.
2. Add a public or translator-level regression that starts with an Anthropic
   assistant thinking block using each optional-field carrier combination,
   translates it, and asserts the resulting Responses input contains the
   typed reasoning item with the same optional values.
3. Update the matrix/documentation only after that end-to-end carrier path is
   proven. Re-run all quality gates and the installed Codex canary.
