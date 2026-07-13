# Inspector Feedback — Iteration 20

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior feedback, current status, and the complete
  Builder commit `44edb1e253ea0e03e6689e223dd6afcc1b643982`.
- Audited the new fallible non-stream Chat translator, response/usage
  extension handling, provider/direct error forwarding, and the existing Chat
  SSE state machine for parity.
- Compared the implementation with the official OpenAI Python ChatCompletion
  and ChatCompletionChunk contracts at `openai-python` `v1.109.0`. Those
  contracts make `usage` optional, require chunk top-level identity fields and
  tool-call `index`, allow multiple choices when `n > 1`, and enumerate
  `finish_reason` and `service_tier` values.
- Ran the public Axum compatibility suite: 63 passed and 1 ignored. Ran the
  ignored installed Codex `0.144.1` loopback canary explicitly: it passed.
- Ran `cargo fmt --all -- --check`, Clippy with warnings denied, verbose build,
  verbose tests, and `cargo deny check`; all passed. `cargo deny check` retains
  the repository's existing non-fatal unmatched-license/duplicate-dependency
  warnings.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The documentation now records the audited Claude Code `2.1.207`
  and Codex CLI `0.144.1` versions, the Chat route matrix, source-backed
  non-streaming policy, provider/direct fixtures, and safe error/header
  behavior. The new non-stream claims are backed by the added fixtures.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The public
  suite exercises provider and direct Chat requests through `build_router()`
  and local loopback fixtures, including malformed JSON, wrong shapes,
  usage/details, response extras, upstream errors, and safe headers. Normal
  tests use no credentials or paid provider and do not use port 4141.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The
  non-streaming Chat response boundary is substantially improved: required
  identity/shape fields, one-choice policy, content, function arguments,
  usage totals/details, response extras, collisions, and sanitized 502
  failures are now handled explicitly. The stream path is not equivalent:

  1. `validate_chat_chunk` validates only `choices`, `delta`, usage shape, and
     finish-reason JSON type. It does not validate the required stream `id`,
     `object: "chat.completion.chunk"`, nonnegative `created`, or non-empty
     `model`; `handle_message_start` silently defaults missing/wrong values to
     empty strings.
  2. It does not validate choice indices or reject multiple choices, while the
     non-stream bridge requires exactly one choice at index zero.
  3. It accepts unknown finish-reason strings and `function_call` in the stream;
     `handle_finish` maps them to no Anthropic stop reason and can still emit a
     successful `message_stop`. It also does not enforce the non-stream
     conflict rule between `tool_calls` and the finish reason.
  4. Stream tool-call deltas allow a missing index, missing identity, and
     conflicting later identity. The handler defaults a missing index to zero,
     ignores later identity conflicts, and has no terminal assertion that every
     tool call has a valid id/name.

  These are supported Claude Messages streaming paths and can produce an
  SDK-recognizable success for malformed or contradictory OpenAI Chat chunks.

- [ ] **Criterion 4 — Codex Responses and inbound translation.** Native Codex
  Responses/compact behavior and all prior inbound request work remain green.
  The retained Chat Completions transport is still not contract-complete:
  non-stream and stream have materially different validation/termination
  behavior, and the stream boundary can fabricate success after malformed
  identity, finish, tool-call, or usage data.

- [ ] **Criterion 5 — native contracts, errors, and no silent loss.** The
  non-stream provider/direct routes now return sanitized Anthropic 502 errors
  and preserve safe request-id/retry headers for malformed successful upstream
  bodies. Remaining failures:

  - OpenAI's non-stream `ChatCompletion.usage` is optional, but this bridge
    requires a non-null usage object and returns 502 for a valid absent/null
    usage response. The stream path accepts optional/null usage and can finish
    successfully without a final usage record, so the two routes disagree.
  - A stream with `choices: []` and an empty or partial usage object passes
    `validate_usage`; `get_anthropic_usage_from_openai_chunk` then defaults
    missing counters to zero and completes a success terminal.
  - Stream usage validation does not enforce required final counters, total
    consistency, or the same details bounds/overflow rules as the non-stream
    path.
  - Stream top-level identity and finish/tool-call fields can default or be
    ignored rather than causing one terminal Anthropic error.

  The non-stream error envelope is native and safe, but the stream still
  violates the no-success-after-malformed-input requirement.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models, and
  aliases.** The inspected changes preserve the configured-provider and
  direct-Copilot Chat routes, provider credentials, model selection, and safe
  upstream header forwarding. Both public paths were exercised by the updated
  tests.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  non-stream public fixtures are broad and cover response extras, collisions,
  malformed fields, usage details, direct failures, and headers. There is no
  equivalent public Chat SSE fixture matrix for:

  - missing/wrong `id`, `object`, `created`, or `model`;
  - wrong/multiple choice indices;
  - unknown/function-call finish reasons and tool/finish conflicts;
  - missing/conflicting tool-call indices and identities;
  - empty/partial/inconsistent terminal usage;
  - stream/non-stream equivalence for optional usage and response extras.

  The installed Codex canary does not exercise the Chat transport.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The
  documentation accurately describes the new fail-closed *non-streaming*
  boundary, but the Chat matrix still presents streaming support without
  documenting the weaker top-level/choice/tool/usage validation or the
  non-stream optional-usage divergence. Its broad malformed-stream and strict
  usage claims are not established by the current implementation/tests.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The
  non-stream implementation is appropriately fallible and preserves
  representable top-level, choice, message, content, usage, and
  function-scope extras with collision checks. The existing stream state
  machine still contains silent defaults (`unwrap_or_default`, missing index
  default to zero, ignored identity conflicts, and zero-filled usage) at the
  public protocol boundary. This violates the goal's explicit “avoid silent
  fallbacks” and exact native-contract requirements.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS.
  - `cargo clippy --all-targets -- -D warnings` — PASS.
  - `cargo build --verbose` — PASS.
  - `cargo test --verbose` — PASS.
  - `cargo deny check` — PASS, with the existing non-fatal warnings noted above.
  - `cargo test --test client_compatibility -- --nocapture` — PASS, 63 passed
    and 1 ignored.
  - `cargo test --test client_compatibility installed_codex_cli_smoke
    -- --ignored --nocapture` — PASS; the installed Codex canary used only the
    loopback fixture.

## Issues Found

### Blocking: Chat SSE validation is not parity with the hardened JSON path

The new strict translator is used only for buffered non-streaming Chat
responses. The streaming path still has the following concrete behaviors:

- `validate_chat_chunk` does not validate the OpenAI-required chunk identity
  (`id`, `object`, `created`, `model`) or choice `index`; `handle_message_start`
  emits empty identity fields when they are absent or wrongly typed.
- Multiple choices are iterated but only the first is used, unlike the explicit
  one-choice non-stream policy.
- A finish reason outside the supported mapped set is only checked as a string.
  `handle_finish` queues a pending delta with `stop_reason: None`, and EOF or a
  usage chunk can complete it as a success.
- Tool-call deltas may never establish an id/name, may omit `index` (which is
  silently converted to zero), or may later contradict the first id/name.
  These conditions do not necessarily trigger a terminal error.
- `validate_usage` treats all counters as optional and checks neither required
  terminal counters nor `total_tokens == prompt_tokens + completion_tokens`.
  An empty usage-only chunk can therefore finish a successful Anthropic stream
  with zero usage.

The official `ChatCompletionChunk` contract requires `id`, `created`, `model`,
`object: "chat.completion.chunk"`, and each tool-call `index`; its usage is
optional only as a whole and, when present in the final include-usage chunk,
contains the completion usage statistics. The implementation does not make
these semantics explicit or consistent with the JSON translator.

### Additional contract gap: valid optional non-stream usage is over-rejected

The official `ChatCompletion` model declares `usage: Optional[CompletionUsage]`.
The bridge currently requires the field and rejects absent/null usage with a
502. If the bridge intentionally requires usage to construct Anthropic usage,
that divergence must be justified and mirrored in streaming; otherwise absent
and null usage need an explicit, documented representation rather than an
incorrect malformed-response classification.

## What Must Be Fixed

1. Add a first-chunk Chat SSE validator for required top-level identity,
   `chat.completion.chunk` object, created/model scalars, choice count/index,
   supported finish reasons, and service-tier enum/null semantics.
2. Track tool-call indices and identities fail-closed; reject missing or
   conflicting identity and incomplete tool calls before a success terminal.
3. Make stream usage validation use the same nonnegative, overflow, detail-bound,
   and total-consistency rules as non-stream, with an explicit policy for the
   OpenAI-optional absent/null usage case.
4. Align non-stream and stream behavior for optional usage, finish reasons,
   multiple choices, response extras, and exactly-one terminal/error semantics.
5. Add deterministic public SSE fixtures for each adversarial case and rerun
   all accumulated compatibility, hardening, gate, and canary checks.
