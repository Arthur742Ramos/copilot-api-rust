# Inspector Feedback — Iteration 1

## Verdict: FAIL

## Inspection basis

- Read the immutable goal and current process status, then reviewed the complete
  `8b7472013665b168737dbb055d9f98f4f735b6d5..d6910788dabafbc1b34293c689e4ccd22076503d`
  diff and the Builder commit.
- Independently verified the current release identities:
  - `gh api repos/anthropics/claude-code/releases/latest --jq .tag_name` →
    `v2.1.207`; installed `claude --version` → `2.1.207 (Claude Code)`.
  - `gh api repos/openai/codex/releases/latest --jq .tag_name` →
    `rust-v0.144.1`; installed `codex --version` → `codex-cli 0.144.1`.
- Inspected Codex source at
  `44918ea10c0f99151c6710411b4322c2f5c96bea`. In particular,
  `codex-rs/protocol/src/models.rs` defines `Reasoning.encrypted_content` and
  `Compaction.id` as optional, and `ContentItem::InputImage.detail` as
  optional. `codex-rs/codex-api/src/common.rs` defines the compaction request
  shape.
- Exercised the real public Axum router through the new
  `tests/client_compatibility.rs` harness and ran its ignored loopback-only
  installed-Codex canary. No external provider, paid quota, or port 4141 was
  used.

## Acceptance Criteria Check

- [x] **Criterion 1 — versions, configuration, endpoints, headers, matrix.**
  The guide records Claude Code 2.1.207, Codex CLI 0.144.1, setup examples,
  endpoint/header claims, source links, and a feature matrix. The release tags,
  installed versions, and the cited TypeScript commits are reproducible.
  However, the additional npm integrity claim for
  `@anthropic-ai/claude-code@2.1.207` is not reproducible in this environment:
  `npm view @anthropic-ai/claude-code@2.1.207` returned `E404`. This is a
  documentation-evidence issue tracked below, not the primary implementation
  blocker.
- [x] **Criterion 2 — credential-free public black-box harness.**
  `tests/client_compatibility.rs` uses `build_router()`/`oneshot`, ephemeral
  loopback upstream fixtures, captured requests, deterministic JSON/SSE, fake
  keys, serial process-global configuration, and no external provider. The
  normal test passes and the opt-in installed Codex canary also passes without
  port 4141.
- [x] **Criterion 3 — Claude Code Messages compatibility.**
  The accumulated Messages translators, native stream validation, error
  envelopes, token-counting path, beta/header handling, model mapping,
  unknown-field flattening, tool/thinking behavior, and prior regression suite
  remain present. The Claude-shaped public boundary test passes for streaming,
  non-streaming, structured system/content, multi-turn tool results,
  parallel tool blocks, prompt-cache markers, thinking, and provider auth.
- [ ] **Criterion 4 — Codex Responses and compaction compatibility.**
  **Blocking failure.** `handle_responses` deserializes the entire
  `ResponsesPayload` before provider dispatch (`src/routes/responses/handler.rs`
  lines 31–54). The current typed input union rejects valid Codex 0.144.1
  request items:
  - `ResponseInputReasoning.encrypted_content` is a required `String`
    (`src/services/copilot/create_responses.rs` lines 247–255), while Codex
    declares `encrypted_content: Option<String>`.
  - `ResponseInputCompaction.id` is a required `String` (lines 268–275), while
    Codex declares `id: Option<String>`. A compact response with no id cannot
    be continued through `/v1/responses`.
  - `ResponseInputImage.detail` is a required `String` (lines 297–303), while
    Codex declares `detail: Option<ImageDetail>`.

  A temporary probe compiled against this HEAD produced:
  `reasoning-no-encrypted: false`, `compaction-no-id: false`, and
  `image-no-detail: false` for those Codex-valid shapes. Because this
  deserialization occurs before the `provider/model` early dispatch, the
  failure applies even to configured OpenAI Responses providers. The new
  compaction fixture only returns a synthetic message and never exercises a
  real compaction item followed by a continuation request.
- [x] **Criterion 5 — native protocol/error/SSE boundary behavior.**
  The deterministic tests verify OpenAI-shaped JSON errors, Anthropic-shaped
  Messages errors, native Responses SSE ordering, preserved function-call
  arguments/ids, one terminal event, malformed-frame termination, premature
  EOF failure, and no fabricated completion. The stream guard is a useful
  shared native Responses boundary.
- [x] **Criterion 6 — authentication, routing, provider-only mode, discovery.**
  Provider aliases and mappings are resolved before Responses dispatch;
  `/v1/models` includes configured provider models and mappings; the fixture
  asserts client auth is replaced by provider auth; and the startup code skips
  GitHub/Copilot initialization in provider-only mode. Unknown models/routes
  return explicit native errors. This does not repair the invalid continuation
  item shapes listed under Criterion 4.
- [ ] **Criterion 7 — regression coverage for every compatibility claim.**
  The harness is substantial and all existing tests pass, but it does not
  cover the current Codex-valid optional reasoning, compaction, and image
  forms. Consequently it cannot catch the public 400 regression described
  above, and it claims compaction/continuation coverage without exercising the
  actual compaction item shape.
- [ ] **Criterion 8 — user-facing documentation is fully evidenced.**
  The setup/troubleshooting guide is useful, but it documents the unsupported
  continuation behavior as supported and includes the unreproducible npm
  package/integrity evidence. Remove or qualify the npm claim (or provide a
  reproducible artifact) and document the exact supported/unsupported
  Responses item forms only after the boundary tests agree with the audited
  Codex source.
- [x] **Criterion 9 — surgical, lossless, hardened implementation.**
  The implementation uses shared admission/auth/error/stream helpers, keeps
  unknown request fields through flattened maps, avoids forwarding client
  credentials to providers, and leaves the prior hardening intact. The
  discovered typed-union gap is a correctness omission, not evidence that the
  hardening was weakened.
- [x] **Criterion 10 — quality gates.**
  All required commands exited successfully:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo build --verbose`
  - `cargo test --verbose`
  - `cargo deny check`

  Additional checks also passed:
  `cargo test --test client_compatibility --verbose` (3 passed, 1 ignored) and
  `cargo test --test client_compatibility installed_codex_cli_smoke -- --ignored
  --nocapture` (1 passed). `cargo deny check` emitted existing non-fatal
  unmatched-license/duplicate-dependency warnings but returned success.

## Issues Found

### Blocking: valid Codex continuation items are rejected before dispatch

The audited Codex protocol intentionally makes reasoning encrypted content,
compaction ids, and image detail optional. The proxy makes all three required
in its `ResponsesPayload` input types. This breaks real multi-turn histories
and can break the next turn immediately after remote compaction. It is a
protocol compatibility failure, not merely missing test breadth.

### Documentation evidence is overstated

The documented `@anthropic-ai/claude-code@2.1.207` npm integrity evidence could
not be resolved by npm and should not be presented as a reproducible audit
artifact. The release tag and installed `claude --version` are valid evidence;
the unsupported package claim should be removed or replaced.

## What Must Be Fixed

1. Align the Responses input item types with the audited Codex schema:
   optionalize `Reasoning.encrypted_content`, `Compaction.id`, and
   `InputImage.detail` while preserving their fields and unknown keys. Audit
   the remaining optional Codex `ResponseItem` fields (including tool-search
   call ids) for the same required-vs-optional mismatch.
2. Add a credential-free public Axum regression that submits each valid shape
   through a configured `provider/model` Responses route, asserts successful
   upstream capture, and performs a real compaction-output-to-next-turn
   continuation using an item with no id.
3. Update the feature matrix and setup guide to match the source-backed tests,
   and remove or correct the unreproducible npm integrity statement.
