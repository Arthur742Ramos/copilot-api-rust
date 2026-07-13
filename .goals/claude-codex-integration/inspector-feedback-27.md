# Inspector Feedback — Iteration 27

## Verdict: PASS

## Inspection basis

- Read the immutable goal, all prior Inspector feedback files (iterations
  1–26), current `status.json`, and Builder commit
  `3dfd87c1f50751801d9158770691891f362ea230`.
- Rechecked the accumulated implementation from initial SHA
  `8b7472013665b168737dbb055d9f98f4f735b6d5`.
- Audited the transactional Responses budget planner, every runtime
  reserve/replace/release caller, terminal/error/EOF cleanup, provider/direct
  usage finalization, web-search reconstruction, and the public fixture wiring.
- The installed Codex canary was attempted, but this environment has no
  `codex` executable. The failure is the documented missing-binary precondition,
  not a product or gate failure. No external or paid call was attempted.

## Acceptance Criteria Check

- [x] **Criterion 1 — audited versions, setup, endpoints, headers, matrix, and
  evidence.** The compatibility guide still records Claude Code 2.1.207,
  Codex CLI 0.144.1, the audited Codex source commit, configuration examples,
  exercised endpoints/headers, provider routing, and the feature matrix. The
  new observed-cost and transactional-budget claims now match the tested
  behavior.

- [x] **Criterion 2 — credential-free black-box Axum harness.** The provider
  and direct Responses budget fixtures and the provider/direct Messages
  web-search fixtures enter `copilot_api::server::build_router()` through
  `oneshot`, then reach an ephemeral loopback Axum upstream. Requests use fake
  credentials, no network or paid provider, and never use port 4141.

- [x] **Criterion 3 — complete Claude Code Messages compatibility.** The full
  accumulated Messages/Chat suite remains green, including JSON/SSE,
  structured content/system forms, tools and multi-turn results, parallel
  calls, prompt-cache markers, reasoning/carriers, token counting, aliases,
  beta/version headers, unknown fields, truncation/cancellation handling, and
  retryable/error paths. The Responses-backed Messages stream now uses the
  same transactional budget path.

- [x] **Criterion 4 — Codex Responses and inbound translation.** Native
  Responses JSON/SSE, instructions/input and continuation items, reasoning,
  function/custom/tool-search calls, parallel interleaving, usage, metadata,
  raw variants, compaction, and failure semantics remain green. The new exact,
  UTF-8, mixed, parallel, replacement, and +1 cases exercise the translated
  Responses path through both provider and direct public routes.

- [x] **Criterion 5 — native contracts, errors, and no silent loss.** The
  transaction planner preflights output capacity and all retained-owner
  mutations against snapshots, then commits both counters/maps only after all
  checked arithmetic, duplicate-owner, missing-owner, and capacity checks pass.
  Failed exact/+1, output-full/retained-free, retained-full/output-free,
  replacement, text, reasoning/signature, compaction, block-key, lifecycle,
  and function argument transitions emit one native error without a success
  terminal or fabricated content. Terminal/error/EOF paths clear retained
  owners and suppress trailing success frames.

  The selected usage policy is also consistent: valid upstream terminal usage
  is recorded once when upstream consumption was observed, even if translated
  Anthropic output later fails; the stream/request outcome is `error`, not
  `ok`, and no native client success terminal/body is emitted. Invalid or
  absent usage is not invented.

- [x] **Criterion 6 — authentication, routing, provider-only mode, models, and
  aliases.** The accumulated public tests still cover client authentication,
  safe header forwarding, provider/direct credential selection, provider-only
  operation without GitHub initialization, model discovery and aliases, and
  explicit unsupported model/route errors. New provider/direct accounting
  assertions verify endpoint/source/provider attribution.

- [x] **Criterion 7 — deterministic regression tests for every claim.** The
  Responses module now has 38 passing unit tests covering exact/+1,
  UTF-8, checked overflow/underflow, both budget-pressure directions,
  owner-map atomicity, growth/shrink replacement, active/inactive/done
  arguments, text/reasoning/signature/compaction/block keys, parallel calls,
  terminal cleanup, and one-error behavior. Public Axum tests cover both
  provider/direct Responses and provider/direct web-search paths, assert
  SQLite usage rows and per-model totals, and assert error/ok stream metrics.
  The opt-in installed-client canary remains reproducible and was correctly
  blocked here only because `codex` is not installed.

- [x] **Criterion 8 — documentation claims are fully evidenced.** The guide
  documents transactionally coupled ownership, observed-cost accounting on
  downstream translation/reconstruction failures, no success metric/terminal
  on those failures, provider/direct overflow evidence, and the 38 unit
  invariants. The canary remains clearly opt-in and loopback-only.

- [x] **Criterion 9 — surgical, lossless, hardened implementation.** Changes
  reuse the existing Responses translator, shared stream metrics, token usage,
  error, and routing helpers. Checked arithmetic remains in place; unknown
  fields/key ordering and prior hardening remain intact. No rollback gap or
  unpaired runtime output/retained path was found after auditing all callers.

- [x] **Criterion 10 — required quality gates.**

  - `cargo fmt --all -- --check` — PASS.
  - `cargo clippy --all-targets -- -D warnings` — PASS.
  - `cargo build --verbose` — PASS.
  - `cargo test --verbose` — PASS; the public compatibility crate reported
    69 passed and 1 ignored, with all other unit/integration/doc tests passing.
  - `cargo deny check` — PASS online; advisories, bans, licenses, and sources
    are OK with the existing non-fatal unmatched-license/duplicate warnings.
  - `CARGO_NET_OFFLINE=true cargo deny check` — PASS; this is a reproducible
    offline check, not an online advisory timeout.
  - Transactional Responses unit filter — PASS (38 tests).
  - Public Responses accounting fixture — PASS.
  - Public web-search observed-cost and validation-policy fixtures — PASS.

## Independent accounting evidence

- The public Responses test's temporary SQLite store contained exactly one
  `provider_messages`/`provider` row per provider exact/overflow model and one
  `responses`/`copilot` row per direct exact/overflow model, with
  `input/output/total = 1/1/2`; no duplicate finalization was observed.
- The web-search store contained exactly two direct `responses`/`copilot` rows
  and two provider `provider_messages`/`provider` rows for the two failed
  overflow requests per path, each with `1/1/2`. The public metric assertions
  confirmed the corresponding requests incremented `error`, did not increment
  HTTP 200 success, and did not increment stream `ok`.
- The public helper wiring was independently confirmed in
  `tests/common/mod.rs`: `send()` builds the production router and calls
  `router().oneshot(request)`, while `Fixture::start()` binds only an
  ephemeral loopback Axum listener.

## Issues Found

None. The iteration-26 atomicity and usage-policy findings are resolved and
independently verified.

## Canary note

`cargo test --test client_compatibility installed_codex_cli_smoke -- --ignored
--nocapture` was attempted. It stopped immediately with `No such file or
directory` because `codex` is not installed in this environment. The canary is
explicitly opt-in and does not affect the credential-free deterministic gates.
