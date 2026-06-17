//! Token-usage SQLite bootstrap and a small fixed-size connection pool.
//!
//! Mirrors the SQLite bootstrap in src/lib/sqlite.ts + store.ts. The TS version
//! lazily opens a single connection; here we open a small pool of connections so
//! concurrent usage reads/writes don't all serialize on one `Mutex<Connection>`.
//! WAL mode (set on each connection) lets multiple readers run concurrently, so
//! the pool actually buys parallelism. A failed on-disk open degrades to a
//! single in-memory connection (which cannot be pooled across separate handles,
//! since each unshared `:memory:` DB is distinct).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use once_cell::sync::OnceCell;
use rusqlite::Connection;

use crate::libs::paths::PATHS;

const DB_PATH_ENV: &str = "COPILOT_API_SQLITE_DB_PATH";
const DEFAULT_DB_FILENAME: &str = "copilot-api.sqlite";

/// Number of pooled connections for the on-disk database.
const POOL_SIZE: usize = 4;

/// The token-usage connection pool: a set of connections selected round-robin.
struct UsagePool {
    connections: Vec<Mutex<Connection>>,
    next: AtomicUsize,
}

impl UsagePool {
    /// Pick the next connection round-robin and run `f` with it locked.
    fn with_conn<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Connection) -> R,
    {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.connections.len();
        let guard = self.connections[idx]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&guard)
    }
}

static USAGE_POOL: OnceCell<UsagePool> = OnceCell::new();

fn db_path() -> PathBuf {
    match std::env::var(DB_PATH_ENV) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => PATHS.app_dir.join(DEFAULT_DB_FILENAME),
    }
}

fn open_usage_connection(path: &PathBuf) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    // `pragma_update` rejects pragmas that return rows (journal_mode echoes the
    // new mode), so issue them via execute_batch which discards results.
    // synchronous=NORMAL (the recommended setting under WAL) avoids an fsync on
    // every INSERT on the per-request usage-record write path; it stays durable
    // against app crashes and only risks the last txn(s) on OS/power loss, which
    // is acceptable for telemetry-class usage data.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;",
    )?;
    crate::libs::token_usage::initialize_schema(&conn)?;
    Ok(conn)
}

fn build_pool() -> UsagePool {
    let path = db_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    match open_usage_connection(&path) {
        Ok(first) => {
            let mut connections = Vec::with_capacity(POOL_SIZE);
            connections.push(Mutex::new(first));
            // The schema is already initialized; the remaining connections share
            // the same on-disk DB and all see the same data.
            for _ in 1..POOL_SIZE {
                match open_usage_connection(&path) {
                    Ok(conn) => connections.push(Mutex::new(conn)),
                    Err(error) => {
                        tracing::warn!(
                            "Failed to open additional token-usage connection ({error}); \
                             continuing with {} connection(s)",
                            connections.len()
                        );
                        break;
                    }
                }
            }
            UsagePool {
                connections,
                next: AtomicUsize::new(0),
            }
        }
        Err(error) => {
            tracing::warn!(
                "Failed to open token usage SQLite database ({error}); \
                 falling back to a single in-memory database"
            );
            let conn = Connection::open_in_memory()
                .expect("failed to open in-memory token usage database");
            let _ = conn.execute_batch("PRAGMA busy_timeout = 5000;");
            let _ = crate::libs::token_usage::initialize_schema(&conn);
            UsagePool {
                connections: vec![Mutex::new(conn)],
                next: AtomicUsize::new(0),
            }
        }
    }
}

/// Run `f` with a pooled token-usage connection. Concurrent calls fan out across
/// the pool's connections (round robin), so reads and writes don't all serialize
/// on a single connection lock.
pub fn with_usage_conn<F, R>(f: F) -> R
where
    F: FnOnce(&Connection) -> R,
{
    USAGE_POOL.get_or_init(build_pool).with_conn(f)
}
