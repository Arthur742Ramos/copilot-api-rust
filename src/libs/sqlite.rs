use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::OnceCell;
use rusqlite::Connection;

use crate::libs::paths::PATHS;

/// Mirrors the SQLite database bootstrap in src/lib/sqlite.ts +
/// src/lib/token-usage/store.ts. The TS version lazily opens a `SqliteDbStore`
/// and runs `initializeTokenUsageDb` on first use; we model the singleton as a
/// process-global `Mutex<Connection>` behind a `OnceCell`.
const DB_PATH_ENV: &str = "COPILOT_API_SQLITE_DB_PATH";
const DEFAULT_DB_FILENAME: &str = "copilot-api.sqlite";

static USAGE_DB: OnceCell<Mutex<Connection>> = OnceCell::new();

fn db_path() -> PathBuf {
    match std::env::var(DB_PATH_ENV) {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => PATHS.app_dir.join(DEFAULT_DB_FILENAME),
    }
}

fn open_usage_db() -> rusqlite::Result<Connection> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let conn = Connection::open(&path)?;
    // `pragma_update` rejects pragmas that return rows (journal_mode echoes the
    // new mode), so issue them via execute_batch which discards results.
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")?;
    crate::libs::token_usage::initialize_schema(&conn)?;
    Ok(conn)
}

/// Lazily-opened, process-global SQLite connection guarding the token-usage
/// store. Mirrors `tokenUsageDbStore.getDb()` in store.ts.
pub fn usage_db() -> &'static Mutex<Connection> {
    USAGE_DB.get_or_init(|| {
        let conn =
            open_usage_db().expect("failed to open token usage SQLite database");
        Mutex::new(conn)
    })
}
