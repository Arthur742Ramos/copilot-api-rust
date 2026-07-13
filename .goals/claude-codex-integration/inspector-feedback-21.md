# Inspector Feedback — Iteration 21

## Verdict: FAIL

## Inspection basis

- Read the immutable goal, all prior feedback, current status, and the complete
  Builder commit `c74c393da8a406026f3954df45b007d339fed287`.
- Audited the new Chat SSE state machine, including identity assertions,
  top-level and message-start extras, usage merging, finish handling, tool
  fragments, argument accumulation, terminal cleanup, and provider/direct
  drivers.
- Compared the implementation with the official OpenAI Python
  `ChatCompletion` and `ChatCompletionChunk` models at `openai-python`
  `v1.109.0`. In particular, the official chunk contract makes `id`, `created`,
  `model`, and `object` required; makes tool-call `index` required but
  `type`/`id`/`function` and function `name`/`arguments` optional per fragment;
  makes `usage` optional as a whole; and defines enum-valued
  `finish_reason`/`service_tier`.
- Ran the public Axum suite: 66 passed and 1 ignored. Ran the ignored installed
  Codex `0.144.1` loopback canary explicitly: it passed.
- Ran all required gates: formatting, Clippy with warnings denied, verbose
  build, verbose tests, and `cargo deny check` passed. `cargo deny check`
  continues to report the repository's existing non-fatal unmatched-license and
  duplicate-dependency warnings.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The documentation now links the official ChatCompletionChunk
  contract and describes identity, usage, tool-fragment, extras, and terminal
  policy. The versions and provider/direct route matrix remain recorded.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The new public
  fixtures exercise strict provider Chat SSE, no-usage streams, streamed tool
  calls, a large malformed matrix, direct-Copilot strict/no-usage/identity
  cases, local Axum routing, safe headers, and one-terminal behavior. No paid
  provider or port 4141 is used.

- [ ] **Criterion 3 — complete Claude Code Messages compatibility.** The
  previous stream gaps are substantially fixed: every accepted chunk checks
  identity and object/created/model scalars, choices are constrained to one
  index-zero choice, service/fingerprint assertions are checked, tool indices
  are contiguous, arguments are accumulated and must end as an object, usage
  details use the strict non-stream parser, extras are retained, and malformed
  streams terminate once.

  Remaining contract gaps are visible in the implementation:

  1. The official stream model allows the first tool-call fragment's `type`,
     `id`, and `function` to be optional, and its function `name` and
     `arguments` to be optional. `validate_tool_deltas` requires all of those
     fields immediately on a first fragment and requires a `function` object
     plus an `arguments` string on every continuation. Valid official
     fragment orders are rejected before the state machine can accumulate them.
  2. Non-null `delta.refusal` is a valid OpenAI ChatCompletionChunk field but
     `validate_delta_extras` rejects it. A non-null `logprobs` object is valid
     on a chunk choice but `validate_choice_extras` rejects it, while the
     non-stream translator preserves `logprobs` as a choice extension. This is
     an unaligned stream/non-stream policy for known optional fields.
  3. The non-stream top-level and nested usage `service_tier` values are passed
     through `optional_chat_string`, which accepts arbitrary strings even
     though the official contract enumerates `auto`, `default`, `flex`, `scale`,
     and `priority`. (The stream top-level field has a stricter enum check, but
     its nested usage field still uses the permissive path.) A malformed known
     scalar can therefore reach a successful Anthropic stream/non-stream
     response.

- [ ] **Criterion 4 — Codex Responses and inbound translation.** Native Codex
  Responses, compaction, carriers, lifecycle, terminal, usage, and routing
  evidence remains green. The retained Chat transport is not fully a
  drop-in OpenAI Chat contract because valid optional tool-fragment shapes and
  refusal/logprobs stream fields are rejected, and nested service-tier values
  are under-validated.

- [ ] **Criterion 5 — native contracts, errors, and no silent loss.** The
  state machine now has one-error/one-terminal behavior, suppresses late
  success, preserves request IDs and safe top-level/usage/tool extras, and
  aligns whole-response absent/null usage with the documented zero-usage policy.
  However:

  - A valid stream fragment with an omitted optional `function`, `name`, or
    `arguments` is classified as malformed rather than being accumulated or
    explicitly documented as unsupported.
  - A valid refusal or non-null logprobs chunk is rejected while the buffered
    translator handles related fields differently.
  - A present top-level or nested usage `service_tier` with an invalid enum
    value is accepted by the non-stream path, and the nested value is accepted
    by the stream path.

  The error envelope for failures is native Anthropic, but these
  over-rejection and under-validation cases prevent exact native contract
  behavior.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models, and
  aliases.** Configured-provider and direct-Copilot Chat paths use the same
  updated stream driver. The public direct test confirms identity and optional
  usage behavior; the provider matrix confirms the broader failure behavior.

- [ ] **Criterion 7 — deterministic regression tests for every claim.** The
  public stream matrix is broad and covers many adversarial cases, but it does
  not cover official valid optional fragment forms:

  - first tool delta with omitted `type`, `function`, or `name`;
  - continuation with omitted `function` or omitted `arguments`;
  - valid refusal deltas;
  - valid non-null choice `logprobs`;
  - nested usage `service_tier` enum values and invalid enum values;
  - identity/service/fingerprint appearance after an initially absent optional
    field;
  - the same adversarial cases through the direct-Copilot path.

  The installed Codex canary does not exercise Chat SSE.

- [ ] **Criterion 8 — documentation claims are fully evidenced.** The docs
  state that Chat SSE uses the same contract and that every streamed function
  call follows the official fragment policy, but the implementation rejects
  optional official fragment shapes. They also say usage/detail parsing is
  shared and strict without documenting that nested usage service tiers are
  only checked as strings. The current claims are broader than the verified
  behavior.

- [ ] **Criterion 9 — surgical, lossless, hardened implementation.** The state
  machine is bounded and substantially more fail-closed, and it preserves
  representable extras with deterministic terminal cleanup. It still
  over-validates optional OpenAI tool fragments, rejects known optional stream
  fields that the non-stream path preserves, and accepts an invalid known
  nested service-tier scalar. Those are remaining contract and parity defects,
  not merely stylistic differences.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS.
  - `cargo clippy --all-targets -- -D warnings` — PASS.
  - `cargo build --verbose` — PASS.
  - `cargo test --verbose` — PASS.
  - `cargo deny check` — PASS, with the existing non-fatal warnings noted above.
  - `cargo test --test client_compatibility -- --nocapture` — PASS, 66 passed
    and 1 ignored.
  - `cargo test --test client_compatibility installed_codex_cli_smoke
    -- --ignored --nocapture` — PASS; the local Codex canary used only the
    loopback fixture.

## Issues Found

### Blocking: optional official tool fragments are over-rejected

The official `ChatCompletionChunk` contract defines a tool-call delta with a
required `index` but optional `id`, `type`, and `function`; the nested function's
`name` and `arguments` are also optional. The current `validate_tool_deltas`
requires `type == "function"`, non-empty `id`, an object-valued `function`, and
an immediate non-empty `name` on the first fragment. For continuation fragments
it requires an object-valued `function` and an `arguments` string.

This rejects valid fragment sequences in which identity/name or the function
object arrives later, even though the state structure is intended to accumulate
fragments. The public tests only exercise the strict “identity on first
fragment” form and malformed missing-identity cases; they do not establish the
official optional forms.

### Blocking: known optional fields and nested service tier are not parity-safe

`validate_delta_extras` rejects non-null refusal values and
`validate_choice_extras` rejects non-null logprobs objects. Both are fields in
the official ChatCompletionChunk contract; the non-stream translator accepts
and preserves related response fields instead. Conversely, the shared
`optional_chat_string` path accepts arbitrary top-level and nested usage
`service_tier` strings, despite the official enum. This combination is neither
a consistent preserve-or-reject policy nor exact stream/non-stream parity.

## What Must Be Fixed

1. Track optional first/continuation tool-call identity and function fragments
   according to the official schema, rejecting only missing identity/name or
   incomplete JSON at the terminal boundary; preserve the existing contiguous
   index and conflict checks.
2. Choose and apply one documented policy for refusal and logprobs in both JSON
   and SSE paths (preserve representable values or reject them consistently).
3. Validate top-level and nested usage service tiers against the official enum,
   including null/absent semantics, and add public provider/direct fixtures for
   valid and invalid values.
4. Add adversarial public SSE fixtures for all optional fragment orders and
   re-run the full accumulated compatibility, hardening, gate, and canary
   suite.
