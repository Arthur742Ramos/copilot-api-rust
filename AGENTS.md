# AGENTS.md

Guidance for AI coding agents working in this repository. This crate began as a
Rust port of [`caozhiyuan/copilot-api`](https://github.com/caozhiyuan/copilot-api)
(a TypeScript proxy), and the reference TS source is checked out at
`/tmp/copilot-api-orig` — a useful place to understand the *original intent* of a
behavior. **But parity is no longer a goal.** The port is established and working;
prefer the better engineering choice over matching the TS upstream. Diverge freely
when it improves correctness, client experience, performance, or clarity.

## Architecture

The code is organized into three layers:

- **`src/libs/`** — shared infrastructure: configuration (`config.rs`), global
  mutable state (`state.rs`), the shared HTTP client (`http.rs`), filesystem
  paths (`paths.rs`), API header construction (`api_config.rs`), error types
  (`error.rs`), tokens, rate limiting, and other cross-cutting utilities.
- **`src/routes/`** — Axum HTTP handlers for the public API surface
  (`messages`, `chat_completions`, `responses`, `embeddings`, `models`, etc.).
  These translate between client-facing API shapes and the Copilot backend.
- **`src/services/`** — upstream clients that talk to GitHub Copilot and other
  providers (e.g. `services/copilot/`).

`src/server.rs` assembles the router and middleware; `src/main.rs` is the CLI
entry point.

## Key conventions

- **Improve freely; parity is not a constraint.** The TS original is reference
  material for understanding intent, not a spec to match. When the TS behavior is
  suboptimal, improve it — add tests for the new behavior and update any test that
  locked the old one. (Historically this codebase carried a "parity first" rule;
  that has been removed.)
- **Serde:** types that round-trip JSON use `#[serde(flatten)]` to preserve
  unknown keys and the `preserve_order` feature so key order is stable. Don't
  drop fields you don't recognize.
- **Errors:** use `AppError` for client/internal failures and `HttpError`
  (`src/libs/error.rs`) to carry an upstream response's status/headers/body.
  Surface errors in the Anthropic JSON shape (`{error:{message,type}}`) with an
  appropriate `type`; avoid silent failures.
- **Preprocessing:** request bodies are often manipulated as `serde_json::Value`
  trees ("Value-walking") rather than fully-typed structs. This was inherited from
  the TS in-place-mutation approach; it's fine where it's simplest, but typed
  structs are welcome where they're clearer or avoid redundant clones.
- **Dead code:** the crate root has `#![allow(dead_code)]` because some ported
  subsystems are wired but not yet reachable. Don't delete an item unless `grep`
  proves it is unreferenced.

## Before you finish

Run the CI gates locally (see `CONTRIBUTING.md`): `cargo fmt`,
`cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`. Keep
changes surgical and never alter runtime behavior in a cleanup or docs change.

## Working with PRs and CI

- **CI trigger scope.** `.github/workflows/ci.yml` runs on `push` to `main` and
  on `pull_request` with the default activity types only
  (`opened`/`synchronize`/`reopened`). Retargeting a PR's base branch is an
  `edited` event and does **not** re-run CI — push a new commit to force a fresh
  run. Relevant when landing stacked PRs (merge the base, retarget the child to
  `main`, then push).
- **Requesting a Copilot review.** `gh pr edit --add-reviewer @copilot` does not
  resolve. Use the API:
  `gh api repos/<owner>/<repo>/pulls/<N>/requested_reviewers -X POST -f "reviewers[]=copilot-pull-request-reviewer[bot]"`.
  Copilot posts a `COMMENTED` review within a minute or two; read it with
  `gh api repos/<owner>/<repo>/pulls/<N>/comments`.

## Dogfooding

This proxy may be running locally (default port `4141`) as the backend for the
very Claude Code session editing it. Stopping that server cuts the model
connection mid-task, so don't restart `4141` casually. When rebuilding the
running server, validate the new binary on a scratch port first (e.g. `4142` via
`/readyz`) before swapping, and prefer an atomic detached swap with
auto-rollback. `/version` reports `git_sha` + `build_timestamp` so you can
confirm which binary is live. See the `hot-swap-server` skill for the full
runbook.
