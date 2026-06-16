# Contributing

Thanks for your interest in improving `copilot-api`. This crate is a Rust port
of [`caozhiyuan/copilot-api`](https://github.com/caozhiyuan/copilot-api); please
preserve behavioral parity with the TypeScript original wherever the two
overlap (wire formats, header construction, request/response shaping, edge-case
handling). When in doubt, match what the TS code does.

## Building and testing

```sh
cargo build              # build the library + binary
cargo test               # run the full test suite
cargo test --lib         # run library unit tests only
```

The crate exposes both a `[lib]` (`src/lib.rs`) and a binary, so most logic is
testable without spawning the server.

## CI gates

Every pull request must pass the same checks CI runs (see
`.github/workflows/ci.yml`). Reproduce them locally before pushing:

```sh
cargo fmt --all -- --check                  # formatting
cargo clippy --all-targets -- -D warnings   # lint (warnings are errors)
cargo build --verbose
cargo test --verbose
```

Notes:

- CI runs the build/test/clippy matrix on **Linux, macOS, and Windows**. Avoid
  platform-specific assumptions (paths, line endings, env vars).
- `RUSTFLAGS: -D warnings` is set in CI, so any compiler warning fails the
  build. `cargo fmt --check` is enforced on Linux only (to avoid CRLF noise).
- `cargo-deny` (license/advisory) runs and is blocking; `cargo-audit` runs but
  is non-blocking.

Run `cargo fmt` before committing so the formatting check passes.

## Branches and pull requests

- Branch off `main` with a descriptive name (e.g. `feat/...`, `fix/...`,
  `docs/...`).
- Keep changes focused and surgical; prefer small PRs that do one thing.
- Pull requests target `main` and must be green on CI before merge.
- Write clear commit messages explaining the *why*, not just the *what*.

## Style

- Keep parity-sensitive code annotated with references to the corresponding TS
  source (e.g. `src/lib/api-config.ts`) when it helps future maintainers.
- Don't introduce behavior changes in cleanup/docs PRs.
