//! Simplified port of `src/lib/logger.ts`.
//!
//! The TypeScript version maintains per-handler daily log files with buffered
//! `fs.WriteStream`s, retention cleanup, and a consola instance per handler.
//! That machinery is intentionally NOT ported. Here we route everything through
//! `tracing` and gate the verbose-only helpers on `state.verbose`, mirroring the
//! `debugLazy`/`debugJson`/`debugJsonTail` semantics.

use serde_json::Value;

use crate::libs::state::with_state;

/// Default tail length used by `debug_json_tail` (matches the TS default of 400).
const DEFAULT_TAIL_LENGTH: usize = 400;

fn verbose() -> bool {
    with_state(|s| s.verbose)
}

/// `debugLazy` — only invoke `f` (and log it) when verbose is enabled, so the
/// (potentially expensive) message is never built on the hot path.
pub fn debug_lazy<F>(tag: &str, f: F)
where
    F: FnOnce() -> String,
{
    if !verbose() {
        return;
    }
    tracing::debug!(target: "handler", "[{tag}] {}", f());
}

/// `debugJson` — log `"{label} {json}"` at debug level when verbose.
///
/// `tag` stands in for the consola instance/tag at the call site
/// (`debugJson(logger, "msg:", val)`); we collapse it to a simple string tag.
pub fn debug_json(tag: &str, label: &str, value: &Value) {
    debug_lazy(tag, || {
        format!(
            "{label} {}",
            serde_json::to_string(value).unwrap_or_default()
        )
    });
}

/// `debugJsonTail` — like `debug_json` but only the last `tail_length` chars of
/// the serialized value are logged.
pub fn debug_json_tail(tag: &str, label: &str, value: &Value, tail_length: usize) {
    debug_lazy(tag, || {
        let json = serde_json::to_string(value).unwrap_or_default();
        let tail: String = if json.len() <= tail_length {
            json
        } else {
            // Slice on char boundaries from the end, mirroring JS `.slice(-n)`
            // semantics closely enough for log output.
            json.chars()
                .rev()
                .take(tail_length)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        };
        format!("{label} {tail}")
    });
}

/// A lightweight stand-in for the consola handler instance returned by
/// `createHandlerLogger(name)`. Holds the handler name and prefixes log lines
/// with it. All `debug*` methods gate on `state.verbose`.
#[derive(Debug, Clone)]
pub struct HandlerLogger {
    name: String,
}

impl HandlerLogger {
    /// Log a debug message (verbose-gated), prefixed with the handler name.
    pub fn debug(&self, msg: &str) {
        if !verbose() {
            return;
        }
        tracing::debug!(target: "handler", "[{}] {msg}", self.name);
    }

    /// `debugJson(logger, label, value)` equivalent bound to this handler.
    pub fn debug_json(&self, label: &str, value: &Value) {
        debug_json(&self.name, label, value);
    }

    /// `debugJsonTail` equivalent bound to this handler, using the default tail.
    pub fn debug_json_tail(&self, label: &str, value: &Value) {
        debug_json_tail(&self.name, label, value, DEFAULT_TAIL_LENGTH);
    }

    /// Warnings are always emitted (not verbose-gated), prefixed with the name.
    pub fn warn(&self, msg: &str) {
        tracing::warn!(target: "handler", "[{}] {msg}", self.name);
    }
}

/// `createHandlerLogger(name)` — returns a named handle for ergonomic logging
/// from downstream handlers. The TS sanitization/file-routing is dropped; the
/// name is simply used as a log prefix.
pub fn create_handler_logger(name: &str) -> HandlerLogger {
    HandlerLogger {
        name: name.to_string(),
    }
}
