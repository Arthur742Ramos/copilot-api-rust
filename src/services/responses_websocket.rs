//! Pooled WebSocket SSE-over-WS engine.
//!
//! Port of `src/services/responses-websocket.ts`. The Copilot `/responses`
//! transport can be flipped from HTTP SSE to a WebSocket carrying the same SSE
//! framing. This module owns a process-global pool of open sockets keyed by an
//! opaque `pool_key` so that consecutive requests to the same upstream reuse a
//! warm connection, closing it after a short idle window.
//!
//! Design notes vs. the TS original:
//! - The TS `Map`s become `std::sync::Mutex<HashMap<..>>` (not `tokio::Mutex`)
//!   so the release path can run synchronously inside `Drop`. We never hold one
//!   of these guards across an `.await`; the live socket itself lives behind a
//!   `tokio::sync::Mutex` and that is the only lock held across awaits.
//! - The TS stores a *pending* `websocketPromise` in the pool so concurrent
//!   first-requests can share one in-flight connect. We instead connect eagerly
//!   before publishing the entry. This is sound because, while any request for a
//!   key is in flight, `active_request_count > 0`, so concurrent requests open
//!   their own *non-pooled* sockets anyway (mirroring `getPooledWebSocketRequestTarget`).
//!   The only behavioural difference is two truly-simultaneous cold starts each
//!   create a socket instead of sharing one; the survivor is pooled for reuse.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, Stream, StreamExt};
use once_cell::sync::Lazy;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// The chunk type yielded by the engine. We reuse the SSE event shape so the
/// WebSocket transport is a drop-in for the HTTP SSE path.
pub type SseChunk = crate::libs::sse::SseEvent;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const DEFAULT_WEBSOCKET_IDLE_TIMEOUT_MS: u64 = 60_000;

/// Per-operation deadline for the pooled WebSocket transport: it bounds the
/// initial request-frame send and each subsequent awaited frame independently
/// (the timer resets on every frame received, including control frames). Mirrors
/// the 120s reqwest `read_timeout` on the shared HTTP path — it caps the gap
/// between successive operations, not the total stream duration. Without it a
/// silent upstream would hang the request forever: the pooled idle-close only
/// fires once `request_count` hits 0, which never happens while a request is
/// in-flight.
const DEFAULT_WEBSOCKET_READ_TIMEOUT_MS: u64 = 120_000;

/// A single pooled request description. Mirrors `PooledWebSocketRequest<TPayload>`.
#[derive(Debug, Clone)]
pub struct PooledWebSocketRequest {
    pub headers: Vec<(String, String)>,
    pub payload: serde_json::Value,
    pub pool_key: String,
    pub url: String,
}

/// Behavioural knobs for [`create_pooled_web_socket_stream`]. Mirrors
/// `PooledWebSocketStreamOptions<TChunk>`.
///
/// `create_chunk` / `is_terminal_chunk` are plain function pointers because the
/// real callers pass free functions; this keeps the option struct `Clone` and
/// avoids boxing.
#[derive(Clone)]
pub struct PooledWebSocketStreamOptions {
    /// Build a chunk from one normalized WebSocket message payload.
    pub create_chunk: fn(String) -> SseChunk,
    /// Idle window before a pooled socket is closed. Defaults to 60s when `None`.
    pub idle_timeout_ms: Option<u64>,
    /// Per-operation deadline (in ms) bounding the initial request-frame send
    /// and each subsequently awaited frame independently; resets on every frame
    /// received. Defaults to 120s when `None`, matching the HTTP path's
    /// read timeout. It caps the gap between operations, not total duration.
    pub read_timeout_ms: Option<u64>,
    /// Returns true once a chunk signals the end of the response.
    pub is_terminal_chunk: fn(&SseChunk) -> bool,
    /// Error text if the socket fails to open.
    pub open_error_message: String,
    /// Error text if the socket errors mid-stream.
    pub stream_error_message: String,
    /// Error text if the socket closes before a terminal chunk arrived.
    pub terminal_chunk_missing_message: String,
    /// Error text if a pooled socket became unavailable before use.
    pub unavailable_error_message: Option<String>,
}

impl std::fmt::Debug for PooledWebSocketStreamOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledWebSocketStreamOptions")
            .field("idle_timeout_ms", &self.idle_timeout_ms)
            .field("open_error_message", &self.open_error_message)
            .field("stream_error_message", &self.stream_error_message)
            .field(
                "terminal_chunk_missing_message",
                &self.terminal_chunk_missing_message,
            )
            .field("unavailable_error_message", &self.unavailable_error_message)
            .finish()
    }
}

/// `https:` -> `wss:`, `http:` -> `ws:`; any other scheme is returned unchanged.
/// Mirrors `createWebSocketUrl`.
pub fn create_web_socket_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https:") {
        format!("wss:{rest}")
    } else if let Some(rest) = url.strip_prefix("http:") {
        format!("ws:{rest}")
    } else {
        url.to_string()
    }
}

// ---------------------------------------------------------------------------
// Global pool state
// ---------------------------------------------------------------------------

/// One live (or just-opened) socket, shared behind an async mutex so that the
/// serialized pooled request can read/write it across awaits.
struct Conn {
    ws: tokio::sync::Mutex<WsStream>,
}

struct PoolEntry {
    /// Identity tag so a releaser can verify the map still holds *its* entry
    /// (the TS code compares object identity: `pool.get(key) === entry`).
    id: u64,
    closed: bool,
    /// Dropping this sender cancels the pending idle-close task.
    idle_cancel: Option<tokio::sync::oneshot::Sender<()>>,
    request_count: i64,
    conn: Arc<Conn>,
}

static WEBSOCKET_POOL: Lazy<std::sync::Mutex<HashMap<String, PoolEntry>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

static WEBSOCKET_ACTIVE_REQUESTS: Lazy<std::sync::Mutex<HashMap<String, i64>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

static NEXT_ENTRY_ID: AtomicU64 = AtomicU64::new(1);

fn next_entry_id() -> u64 {
    NEXT_ENTRY_ID.fetch_add(1, Ordering::Relaxed)
}

fn active_request_count(pool_key: &str) -> i64 {
    *WEBSOCKET_ACTIVE_REQUESTS
        .lock()
        .unwrap()
        .get(pool_key)
        .unwrap_or(&0)
}

fn increment_active_request_count(pool_key: &str) {
    let mut active = WEBSOCKET_ACTIVE_REQUESTS.lock().unwrap();
    *active.entry(pool_key.to_string()).or_insert(0) += 1;
}

fn decrement_active_request_count(pool_key: &str) {
    let mut active = WEBSOCKET_ACTIVE_REQUESTS.lock().unwrap();
    let next = active.get(pool_key).copied().unwrap_or(0) - 1;
    if next <= 0 {
        active.remove(pool_key);
    } else {
        active.insert(pool_key.to_string(), next);
    }
}

/// Remove the pool entry identified by `(pool_key, id)` if it is still the
/// mapped one, marking it closed and cancelling any idle timer. Mirrors
/// `removePooledWebSocketEntry` (identity guarded). The socket itself closes
/// when the last `Arc<Conn>` is dropped.
fn remove_pooled_entry(pool_key: &str, id: u64) {
    let mut pool = WEBSOCKET_POOL.lock().unwrap();
    if let Some(entry) = pool.get(pool_key) {
        if entry.id == id {
            if let Some(mut entry) = pool.remove(pool_key) {
                entry.closed = true;
                // Dropping the cancel sender aborts the idle-close task.
                entry.idle_cancel = None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Acquire / release
// ---------------------------------------------------------------------------

/// What an acquire resolved to: a shared connection plus the bookkeeping needed
/// to release it on drop.
struct RequestHandle {
    conn: Arc<Conn>,
    pool_key: String,
    /// Entry identity in the pool (0 for non-pooled sockets).
    id: u64,
    pooled: bool,
    idle_timeout_ms: u64,
    released: bool,
}

impl Drop for RequestHandle {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;

        decrement_active_request_count(&self.pool_key);

        if !self.pooled {
            // Non-pooled socket: nothing mapped; closes when the Arc drops.
            return;
        }

        let mut pool = WEBSOCKET_POOL.lock().unwrap();
        let still_mapped = match pool.get_mut(&self.pool_key) {
            Some(entry) if entry.id == self.id => entry,
            _ => return, // entry was replaced/removed already
        };

        still_mapped.request_count -= 1;
        if still_mapped.closed || still_mapped.request_count > 0 {
            return;
        }

        // Schedule an idle close. Mirrors schedulePooledWebSocketIdleClose.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        still_mapped.idle_cancel = Some(tx);
        let pool_key = self.pool_key.clone();
        let id = self.id;
        let idle_ms = self.idle_timeout_ms;

        // Spawn requires a running runtime; in our server the stream is always
        // dropped within the tokio runtime. If somehow not, close immediately.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(idle_ms)) => {
                            remove_pooled_entry(&pool_key, id);
                        }
                        // Cancelled (sender dropped on reuse or explicit remove).
                        _ = rx => {}
                    }
                });
            }
            Err(_) => {
                drop(pool);
                remove_pooled_entry(&self.pool_key, self.id);
            }
        }
    }
}

/// Decide whether to reuse a pooled socket, create a new pooled one, or open a
/// fresh non-pooled socket; then connect if needed and register the request.
/// Combines `getPooledWebSocketRequestTarget` + `acquirePooledWebSocketEntry`.
async fn acquire(
    request: &PooledWebSocketRequest,
    options: &PooledWebSocketStreamOptions,
) -> Result<RequestHandle, std::io::Error> {
    let idle_timeout_ms = options
        .idle_timeout_ms
        .unwrap_or(DEFAULT_WEBSOCKET_IDLE_TIMEOUT_MS);

    enum Decision {
        Reuse(Arc<Conn>, u64),
        NewPooled,
        NewUnpooled,
    }

    let decision = {
        // Both maps under their own locks; held only for this sync section.
        if active_request_count(&request.pool_key) > 0 {
            // Increment before the connect await so concurrent acquirers in-flight
            // reliably observe active_request_count > 0 and avoid racing on the
            // pooled path. Decremented below if the connect fails.
            increment_active_request_count(&request.pool_key);
            Decision::NewUnpooled
        } else {
            let mut pool = WEBSOCKET_POOL.lock().unwrap();
            match pool.get_mut(&request.pool_key) {
                Some(entry) if !entry.closed => {
                    // Reuse: cancel idle timer and increment counts inline.
                    entry.idle_cancel = None;
                    entry.request_count += 1;
                    let conn = entry.conn.clone();
                    let id = entry.id;
                    drop(pool);
                    increment_active_request_count(&request.pool_key);
                    Decision::Reuse(conn, id)
                }
                _ => {
                    // Increment before awaiting the connect so a concurrent
                    // acquire sees active_request_count > 0 and takes the
                    // unpooled path instead of starting a redundant pooled
                    // connection. Decremented below on connect failure.
                    drop(pool);
                    increment_active_request_count(&request.pool_key);
                    Decision::NewPooled
                }
            }
        }
    };

    match decision {
        Decision::Reuse(conn, id) => Ok(RequestHandle {
            conn,
            pool_key: request.pool_key.clone(),
            id,
            pooled: true,
            idle_timeout_ms,
            released: false,
        }),
        Decision::NewPooled => {
            // active count already incremented before this await.
            let ws =
                match open_web_socket(&request.url, &request.headers, &options.open_error_message)
                    .await
                {
                    Ok(ws) => ws,
                    Err(e) => {
                        decrement_active_request_count(&request.pool_key);
                        return Err(e);
                    }
                };
            let conn = Arc::new(Conn {
                ws: tokio::sync::Mutex::new(ws),
            });
            let id = next_entry_id();
            {
                let mut pool = WEBSOCKET_POOL.lock().unwrap();
                pool.insert(
                    request.pool_key.clone(),
                    PoolEntry {
                        id,
                        closed: false,
                        idle_cancel: None,
                        request_count: 1,
                        conn: conn.clone(),
                    },
                );
            }
            Ok(RequestHandle {
                conn,
                pool_key: request.pool_key.clone(),
                id,
                pooled: true,
                idle_timeout_ms,
                released: false,
            })
        }
        Decision::NewUnpooled => {
            // active count already incremented before this await.
            let ws =
                match open_web_socket(&request.url, &request.headers, &options.open_error_message)
                    .await
                {
                    Ok(ws) => ws,
                    Err(e) => {
                        decrement_active_request_count(&request.pool_key);
                        return Err(e);
                    }
                };
            let conn = Arc::new(Conn {
                ws: tokio::sync::Mutex::new(ws),
            });
            Ok(RequestHandle {
                conn,
                pool_key: request.pool_key.clone(),
                id: 0,
                pooled: false,
                idle_timeout_ms,
                released: false,
            })
        }
    }
}

/// Open one socket with the supplied headers. Mirrors `openWebSocket`.
async fn open_web_socket(
    url: &str,
    headers: &[(String, String)],
    open_error_message: &str,
) -> Result<WsStream, std::io::Error> {
    // Build a properly-formed handshake request (Host / Sec-WebSocket-Key / ...)
    // then layer the caller's custom headers on top.
    let mut req = url
        .into_client_request()
        .map_err(|e| std::io::Error::other(format!("{open_error_message}: {e}")))?;
    {
        let map = req.headers_mut();
        for (key, value) in headers {
            if let (Ok(name), Ok(val)) = (
                HeaderName::from_bytes(key.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                map.insert(name, val);
            }
        }
    }

    let (ws, _resp) = connect_async(req)
        .await
        .map_err(|e| std::io::Error::other(format!("{open_error_message}: {e}")))?;
    Ok(ws)
}

/// Normalize a WebSocket frame into the SSE text payload, or `None` for control
/// frames that carry no application data. Mirrors `normalizeWebSocketMessageData`.
fn normalize_message(message: Message) -> Option<String> {
    match message {
        Message::Text(text) => Some(text),
        Message::Binary(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        // Ping/Pong/Frame carry no application payload; skip them.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run a pooled WebSocket request and stream its SSE chunks. Mirrors
/// `createPooledWebSocketStream` / `runPooledWebSocketRequest`.
///
/// The returned stream yields `Ok(chunk)` per message until (and including) the
/// terminal chunk, then ends. If the socket closes before a terminal chunk, or
/// errors mid-stream, it yields a single `Err` and the pooled entry is dropped.
pub fn create_pooled_web_socket_stream(
    request: PooledWebSocketRequest,
    options: PooledWebSocketStreamOptions,
) -> impl Stream<Item = Result<SseChunk, std::io::Error>> {
    async_stream::stream! {
        // Acquire (connect/reuse + bookkeeping). On failure, surface one error.
        let handle = match acquire(&request, &options).await {
            Ok(handle) => handle,
            Err(err) => {
                yield Err(err);
                return;
            }
        };
        // `handle` is held for the whole stream; its Drop runs the release path.
        let pool_key = handle.pool_key.clone();
        let id = handle.id;
        let pooled = handle.pooled;

        let payload = match serde_json::to_string(&request.payload) {
            Ok(text) => text,
            Err(err) => {
                if pooled {
                    remove_pooled_entry(&pool_key, id);
                }
                yield Err(std::io::Error::other(format!(
                    "{}: {err}",
                    options.stream_error_message
                )));
                return;
            }
        };

        // The live socket is serialized behind this async mutex for the request.
        let mut ws = handle.conn.ws.lock().await;

        let read_timeout = std::time::Duration::from_millis(
            options
                .read_timeout_ms
                .unwrap_or(DEFAULT_WEBSOCKET_READ_TIMEOUT_MS),
        );

        match tokio::time::timeout(read_timeout, ws.send(Message::Text(payload))).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                if pooled {
                    remove_pooled_entry(&pool_key, id);
                }
                yield Err(std::io::Error::other(format!(
                    "{}: {err}",
                    options.stream_error_message
                )));
                return;
            }
            Err(_elapsed) => {
                if pooled {
                    remove_pooled_entry(&pool_key, id);
                }
                yield Err(std::io::Error::other(format!(
                    "{}: timed out sending request frame after {}ms",
                    options.stream_error_message,
                    read_timeout.as_millis()
                )));
                return;
            }
        }

        loop {
            let next = match tokio::time::timeout(read_timeout, ws.next()).await {
                Ok(next) => next,
                Err(_elapsed) => {
                    // No frame (data or control) within the deadline: treat the
                    // socket as wedged, drop it from the pool, and surface a
                    // terminal error so the client stops waiting.
                    if pooled {
                        remove_pooled_entry(&pool_key, id);
                    }
                    yield Err(std::io::Error::other(format!(
                        "{}: no data within {}ms",
                        options.stream_error_message,
                        read_timeout.as_millis()
                    )));
                    return;
                }
            };
            match next {
                Some(Ok(message)) => {
                    let Some(data) = normalize_message(message) else {
                        // Control frame (ping/pong) -> keep reading.
                        continue;
                    };
                    let chunk = (options.create_chunk)(data);
                    let terminal = (options.is_terminal_chunk)(&chunk);
                    yield Ok(chunk);
                    if terminal {
                        // Normal completion: keep the pooled socket for reuse;
                        // the handle's Drop schedules the idle close.
                        return;
                    }
                }
                Some(Err(err)) => {
                    if pooled {
                        remove_pooled_entry(&pool_key, id);
                    }
                    yield Err(std::io::Error::other(format!(
                        "{}: {err}",
                        options.stream_error_message
                    )));
                    return;
                }
                None => {
                    // Socket closed before a terminal chunk arrived.
                    if pooled {
                        remove_pooled_entry(&pool_key, id);
                    }
                    yield Err(std::io::Error::other(
                        options.terminal_chunk_missing_message.clone(),
                    ));
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_web_socket_url_upgrades_https() {
        assert_eq!(
            create_web_socket_url("https://example.com/v1/responses"),
            "wss://example.com/v1/responses"
        );
    }

    #[test]
    fn create_web_socket_url_upgrades_http() {
        assert_eq!(
            create_web_socket_url("http://localhost:1234/x"),
            "ws://localhost:1234/x"
        );
    }

    #[test]
    fn create_web_socket_url_passes_through_other_schemes() {
        assert_eq!(create_web_socket_url("wss://a/b"), "wss://a/b");
        assert_eq!(create_web_socket_url("ws://a/b"), "ws://a/b");
    }

    #[test]
    fn active_request_count_increments_and_decrements() {
        let key = "test-key-counts-unique";
        assert_eq!(active_request_count(key), 0);
        increment_active_request_count(key);
        increment_active_request_count(key);
        assert_eq!(active_request_count(key), 2);
        decrement_active_request_count(key);
        assert_eq!(active_request_count(key), 1);
        decrement_active_request_count(key);
        assert_eq!(active_request_count(key), 0);
        // Hitting zero removes the map entry entirely.
        assert!(!WEBSOCKET_ACTIVE_REQUESTS.lock().unwrap().contains_key(key));
    }

    #[test]
    fn remove_pooled_entry_on_empty_pool_is_noop() {
        // Removing a key that was never inserted must not panic.
        remove_pooled_entry("test-key-never-inserted-unique", 12345);
        assert!(!WEBSOCKET_POOL
            .lock()
            .unwrap()
            .contains_key("test-key-never-inserted-unique"));
    }
}
