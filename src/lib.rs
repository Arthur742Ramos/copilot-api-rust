// Library root for the copilot-api crate. Declaring the modules here (rather
// than only in main.rs) exposes them to integration tests under `tests/`, which
// link against this `copilot_api` library. The binary (src/main.rs) `use`s these
// same modules, so there is a single compiled copy of the process-global state
// (STATE, CACHED_CONFIG, the SQLite connection, ...).
//
// Deferred subsystems (provider routing, Codex OAuth, MCP, usage store) are
// translated and wired but not yet reachable from the core proxy spine, so
// their items read as dead code until those features are turned on.
#![allow(dead_code)]

pub mod libs;
pub mod routes;
pub mod server;
pub mod services;

pub mod debug;
pub mod mcp;
