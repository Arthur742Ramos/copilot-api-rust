# AGENTS.md

Guidance for AI coding agents working in this repository. This crate is a
**faithful Rust port** of [`caozhiyuan/copilot-api`](https://github.com/caozhiyuan/copilot-api)
(a TypeScript proxy). The reference TS source is checked out at
`/tmp/copilot-api-orig` — consult it whenever behavior is ambiguous and mirror
it rather than inventing new logic.

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

- **Parity first.** Match the TS original's wire format and edge-case handling.
  Annotate non-obvious code with the corresponding TS path (e.g.
  `src/lib/api-config.ts`).
- **Serde:** types that round-trip JSON use `#[serde(flatten)]` to preserve
  unknown keys and the `preserve_order` feature so key order is stable. Don't
  drop fields you don't recognize.
- **Errors:** use `AppError` for internal failures and `HttpError`
  (`src/libs/error.rs`) to forward an upstream response's status/headers/body
  unchanged. Avoid silent failures.
- **Preprocessing:** request bodies are often manipulated as `serde_json::Value`
  trees ("Value-walking") rather than fully-typed structs, mirroring the TS
  approach of mutating the request object in place.
- **Dead code:** the crate root has `#![allow(dead_code)]` because some ported
  subsystems are wired but not yet reachable. Don't delete an item unless `grep`
  proves it is unreferenced.

## Before you finish

Run the CI gates locally (see `CONTRIBUTING.md`): `cargo fmt`,
`cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`. Keep
changes surgical and never alter runtime behavior in a cleanup or docs change.
