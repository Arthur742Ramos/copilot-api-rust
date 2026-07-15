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
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
/// `create_chunk` is a plain function pointer because callers pass free
/// functions; this keeps the option struct `Clone` and avoids boxing. Terminal
/// authority comes from the shared Responses lifecycle guard, not a raw type
/// predicate.
#[derive(Clone)]
pub struct PooledWebSocketStreamOptions {
    /// Build a chunk from one normalized WebSocket message payload.
    pub create_chunk: fn(String) -> SseChunk,
    /// Idle window before a pooled socket is closed. Defaults to 60s when `None`.
    pub idle_timeout_ms: Option<u64>,
    /// Deadline for opening the TCP/TLS/WebSocket connection.
    pub connect_timeout: Duration,
    /// Per-operation deadline bounding the initial request-frame send and each
    /// subsequently awaited frame independently; resets on every frame. `None`
    /// disables it, matching `COPILOT_API_UPSTREAM_READ_TIMEOUT_SECS=0` on the
    /// HTTP path.
    pub read_timeout: Option<Duration>,
    /// Deadline for the protocol-level ping/pong health probe performed before
    /// every application request frame. A probe failure is definitively
    /// pre-request and is therefore safe for the caller to fall back to HTTP.
    pub preflight_timeout: Duration,
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
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .field("preflight_timeout", &self.preflight_timeout)
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

async fn watch_idle_connection(
    pool_key: String,
    id: u64,
    conn: Arc<Conn>,
    idle_timeout: Duration,
    mut cancel: tokio::sync::oneshot::Receiver<()>,
) {
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);

    loop {
        let mut websocket = conn.ws.lock().await;
        tokio::select! {
            _ = &mut cancel => return,
            _ = &mut idle => {
                drop(websocket);
                remove_pooled_entry(&pool_key, id);
                return;
            }
            next = websocket.next() => {
                match next {
                    Some(Ok(Message::Ping(_))) => {
                        // Tungstenite queues the protocol-required pong while
                        // reading the ping. Flush it before continuing to watch.
                        if websocket.flush().await.is_err() {
                            drop(websocket);
                            remove_pooled_entry(&pool_key, id);
                            return;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    // Any application frame after an accepted terminal is stale
                    // protocol data. Close/error/EOF likewise makes the entry
                    // unusable. In every case evict before the next acquire.
                    Some(Ok(_)) | Some(Err(_)) | None => {
                        drop(websocket);
                        remove_pooled_entry(&pool_key, id);
                        return;
                    }
                }
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
    reused: bool,
    idle_timeout_ms: u64,
    released: bool,
    /// Set only after the shared lifecycle guard accepts a terminal application
    /// frame. A cancelled or protocol-invalid stream may leave response frames
    /// queued, so its connection must never return to the pool.
    reusable: bool,
}

impl RequestHandle {
    fn mark_reusable(&mut self) {
        self.reusable = true;
    }
}

impl Drop for RequestHandle {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;

        decrement_active_request_count(&self.pool_key);

        if self.pooled && !self.reusable {
            remove_pooled_entry(&self.pool_key, self.id);
            return;
        }

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
        let conn = still_mapped.conn.clone();

        // Spawn requires a running runtime; in our server the stream is always
        // dropped within the tokio runtime. If somehow not, close immediately.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(watch_idle_connection(
                    pool_key,
                    id,
                    conn,
                    Duration::from_millis(idle_ms),
                    rx,
                ));
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
            reused: true,
            idle_timeout_ms,
            released: false,
            reusable: false,
        }),
        Decision::NewPooled => {
            // active count already incremented before this await.
            let ws = match open_web_socket(
                &request.url,
                &request.headers,
                &options.open_error_message,
                options.connect_timeout,
            )
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
                reused: false,
                idle_timeout_ms,
                released: false,
                reusable: false,
            })
        }
        Decision::NewUnpooled => {
            // active count already incremented before this await.
            let ws = match open_web_socket(
                &request.url,
                &request.headers,
                &options.open_error_message,
                options.connect_timeout,
            )
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
                reused: false,
                idle_timeout_ms,
                released: false,
                reusable: false,
            })
        }
    }
}

/// Open one socket with the supplied headers. Mirrors `openWebSocket`.
async fn open_web_socket(
    url: &str,
    headers: &[(String, String)],
    open_error_message: &str,
    connect_timeout: Duration,
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

    let (ws, _resp) = tokio::time::timeout(connect_timeout, connect_async(req))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "{open_error_message}: connection timed out after {}ms",
                    connect_timeout.as_millis()
                ),
            )
        })?
        .map_err(|e| std::io::Error::other(format!("{open_error_message}: {e}")))?;
    Ok(ws)
}

async fn await_optional_timeout<F, T>(timeout: Option<Duration>, future: F) -> Result<T, ()>
where
    F: Future<Output = T>,
{
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future).await.map_err(|_| ()),
        None => Ok(future.await),
    }
}

async fn preflight_web_socket(
    websocket: &mut WsStream,
    timeout: Duration,
    unavailable_message: &str,
) -> Result<(), std::io::Error> {
    let nonce = next_entry_id().to_be_bytes().to_vec();
    let probe = async {
        websocket
            .send(Message::Ping(nonce.clone()))
            .await
            .map_err(|error| {
                std::io::Error::other(format!("{unavailable_message}: ping failed: {error}"))
            })?;
        loop {
            match websocket.next().await {
                Some(Ok(Message::Pong(payload))) if payload == nonce => return Ok(()),
                Some(Ok(Message::Ping(_))) => {
                    websocket.flush().await.map_err(|error| {
                        std::io::Error::other(format!(
                            "{unavailable_message}: pong flush failed: {error}"
                        ))
                    })?;
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                    return Err(std::io::Error::other(format!(
                        "{unavailable_message}: stale application frame before request"
                    )));
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Err(std::io::Error::other(format!(
                        "{unavailable_message}: connection closed before request"
                    )));
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Err(std::io::Error::other(format!(
                        "{unavailable_message}: health probe failed: {error}"
                    )));
                }
            }
        }
    };

    tokio::time::timeout(timeout, probe).await.map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "{unavailable_message}: health probe timed out after {}ms",
                timeout.as_millis()
            ),
        )
    })?
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
pub async fn create_pooled_web_socket_stream(
    request: PooledWebSocketRequest,
    options: PooledWebSocketStreamOptions,
) -> Result<impl Stream<Item = Result<SseChunk, std::io::Error>>, std::io::Error> {
    let payload = serde_json::to_string(&request.payload).map_err(|error| {
        std::io::Error::other(format!("{}: {error}", options.stream_error_message))
    })?;
    // Probe and send while still inside this async constructor. Every returned
    // Err is therefore known to precede the application request frame and is
    // safe for the caller's HTTP fallback. A request-frame send failure is
    // ambiguous (the peer may have received bytes), so it becomes the first
    // error item of an Ok stream and can never trigger a replay.
    let mut handle = acquire(&request, &options).await?;
    let (pool_key, id, pooled, initial_error) = loop {
        let pool_key = handle.pool_key.clone();
        let id = handle.id;
        let pooled = handle.pooled;
        let conn = handle.conn.clone();
        let mut websocket = conn.ws.lock().await;
        let unavailable_message = options
            .unavailable_error_message
            .as_deref()
            .unwrap_or("Websocket connection became unavailable before the request started");
        if let Err(error) = preflight_web_socket(
            &mut websocket,
            options.preflight_timeout,
            unavailable_message,
        )
        .await
        {
            if pooled {
                remove_pooled_entry(&pool_key, id);
            }
            let reopen = handle.reused;
            drop(websocket);
            drop(handle);
            if reopen {
                metrics::counter!("responses_websocket_pool_reopen_total").increment(1);
                handle = acquire(&request, &options).await?;
                continue;
            }
            return Err(error);
        }

        let initial_error = match await_optional_timeout(
            options.read_timeout,
            websocket.send(Message::Text(payload.clone())),
        )
        .await
        {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(std::io::Error::other(format!(
                "{}: request frame send failed: {error}",
                options.stream_error_message
            ))),
            Err(()) => Some(std::io::Error::other(format!(
                "{}: timed out sending request frame after {}ms",
                options.stream_error_message,
                options
                    .read_timeout
                    .map(|value| value.as_millis())
                    .unwrap_or_default()
            ))),
        };
        break (pool_key, id, pooled, initial_error);
    };

    if initial_error.is_some() && pooled {
        remove_pooled_entry(&pool_key, id);
    }

    Ok(async_stream::stream! {
        // `handle` is held for the whole stream; its Drop runs the release path.
        let mut handle = handle;
        if let Some(error) = initial_error {
            // The request frame may have reached the peer. Surface the error but
            // deliberately do not return it from the constructor (which would
            // make the caller replay over HTTP).
            yield Err(error);
            return;
        }

        let conn = handle.conn.clone();
        let mut websocket = conn.ws.lock().await;
        let read_timeout = options.read_timeout;
        let mut lifecycle = crate::routes::responses::stream_guard::ResponsesStreamGuard::new();
        let mut ids = crate::routes::responses::stream_id_sync::StreamIdTracker::new();

        loop {
            let next = match await_optional_timeout(read_timeout, websocket.next()).await {
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
                        read_timeout.map(|value| value.as_millis()).unwrap_or_default()
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
                    let terminal = match lifecycle.process(&chunk, &mut ids) {
                        Ok(Some(processed)) => processed.terminal.is_some(),
                        Ok(None) => continue,
                        Err(reason) => {
                            if pooled {
                                remove_pooled_entry(&pool_key, id);
                            }
                            yield Err(std::io::Error::other(format!(
                                "{}: invalid Responses lifecycle ({reason})",
                                options.stream_error_message
                            )));
                            return;
                        }
                    };
                    if terminal {
                        // Only the authoritative lifecycle guard can approve
                        // reuse. Mark before yielding so an immediate downstream
                        // drop after a valid terminal still schedules idle watch.
                        handle.mark_reusable();
                    }
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_chunk(data: String) -> SseChunk {
        let event = serde_json::from_str::<serde_json::Value>(&data)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            });
        SseChunk {
            id: None,
            event,
            data,
        }
    }

    fn test_terminal(chunk: &SseChunk) -> bool {
        chunk.event.as_deref() == Some("response.completed")
    }

    async fn receive_request(
        websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> String {
        loop {
            match websocket.next().await.expect("websocket frame").unwrap() {
                Message::Ping(payload) => websocket
                    .send(Message::Pong(payload))
                    .await
                    .expect("send preflight pong"),
                Message::Text(payload) => return payload,
                other => panic!("unexpected pre-request frame: {other:?}"),
            }
        }
    }

    async fn send_created(
        websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        response_id: &str,
    ) {
        websocket
            .send(Message::Text(
                serde_json::json!({
                    "type":"response.created",
                    "sequence_number":0,
                    "response":{"id":response_id}
                })
                .to_string(),
            ))
            .await
            .expect("send response.created");
    }

    async fn send_completed(
        websocket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        response_id: &str,
    ) {
        websocket
            .send(Message::Text(
                serde_json::json!({
                    "type":"response.completed",
                    "sequence_number":2,
                    "response":{"id":response_id}
                })
                .to_string(),
            ))
            .await
            .expect("send response.completed");
    }

    async fn wait_until_pool_entry_is_removed(pool_key: &str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !WEBSOCKET_POOL.lock().unwrap().contains_key(pool_key) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("pool entry should be removed");
    }

    fn test_options() -> PooledWebSocketStreamOptions {
        PooledWebSocketStreamOptions {
            create_chunk: test_chunk,
            idle_timeout_ms: Some(5),
            connect_timeout: Duration::from_secs(2),
            read_timeout: Some(Duration::from_secs(2)),
            preflight_timeout: Duration::from_millis(250),
            open_error_message: "open failed".to_string(),
            stream_error_message: "stream failed".to_string(),
            terminal_chunk_missing_message: "terminal missing".to_string(),
            unavailable_error_message: None,
        }
    }

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

    #[tokio::test]
    async fn handshake_failure_is_returned_before_a_stream_is_created() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind handshake test listener");
        let addr = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept connection");
            drop(socket); // EOF before the WebSocket handshake completes
        });

        let request = PooledWebSocketRequest {
            headers: Vec::new(),
            payload: serde_json::json!({"type": "response.create"}),
            pool_key: format!("failed-handshake-{}", next_entry_id()),
            url: format!("ws://{addr}"),
        };
        assert!(
            create_pooled_web_socket_stream(request, test_options())
                .await
                .is_err(),
            "connect failure must be observable while HTTP fallback is still safe"
        );
        server.await.expect("handshake server task");
    }

    #[tokio::test]
    async fn stale_preflight_frame_returns_before_application_request_send() {
        use tokio_tungstenite::accept_async;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind preflight listener");
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(tcp).await.unwrap();
            let first = websocket.next().await.unwrap().unwrap();
            assert!(matches!(first, Message::Ping(_)));
            websocket
                .send(Message::Text(
                    r#"{"type":"response.completed","response":{"id":"stale"}}"#.to_string(),
                ))
                .await
                .unwrap();
            // The peer must close without ever sending response.create.
            if let Ok(Some(Ok(Message::Text(payload)))) =
                tokio::time::timeout(Duration::from_millis(100), websocket.next()).await
            {
                panic!("application request was sent after failed preflight: {payload}");
            }
        });
        let request = PooledWebSocketRequest {
            headers: Vec::new(),
            payload: serde_json::json!({"type":"response.create"}),
            pool_key: format!("preflight-failure-{}", next_entry_id()),
            url: format!("ws://{address}"),
        };
        let options = test_options();
        let error = match create_pooled_web_socket_stream(request, options).await {
            Ok(_) => panic!("failed preflight should be constructor error"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("stale application frame"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn client_cancellation_evicts_socket_instead_of_reusing_queued_frames() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio_tungstenite::accept_async;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket test listener");
        let addr = listener.local_addr().expect("listener address");
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_server = accepted.clone();

        let server = tokio::spawn(async move {
            let mut handlers = Vec::new();
            for connection_index in 0..2 {
                let (tcp, _) = listener.accept().await.expect("accept websocket");
                accepted_server.fetch_add(1, Ordering::SeqCst);
                handlers.push(tokio::spawn(async move {
                    let mut ws = accept_async(tcp).await.expect("websocket handshake");
                    let _request = receive_request(&mut ws).await;
                    let response_id = format!("resp-cancel-{connection_index}");
                    send_created(&mut ws, &response_id).await;
                    ws.send(Message::Text(
                        r#"{"type":"response.output_text.delta","delta":"old"}"#.to_string(),
                    ))
                    .await
                    .expect("send delta");

                    if connection_index == 0 {
                        // A cancelled client must close this connection. If the
                        // pool incorrectly reuses it, this receives the second
                        // request frame and that request consumes stale output.
                        let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
                    } else {
                        send_completed(&mut ws, &response_id).await;
                    }
                }));
            }
            for handler in handlers {
                handler.await.expect("websocket handler task");
            }
        });

        let pool_key = format!("cancel-test-{}", next_entry_id());
        let request = || PooledWebSocketRequest {
            headers: Vec::new(),
            payload: serde_json::json!({"type": "response.create"}),
            pool_key: pool_key.clone(),
            url: format!("ws://{addr}"),
        };

        let mut first = Box::pin(
            create_pooled_web_socket_stream(request(), test_options())
                .await
                .expect("first websocket stream"),
        );
        let created = first
            .next()
            .await
            .expect("first chunk")
            .expect("valid first chunk");
        assert_eq!(created.event.as_deref(), Some("response.created"));
        let first_chunk = first.next().await.unwrap().unwrap();
        assert!(!test_terminal(&first_chunk));
        drop(first); // cancellation before response.completed

        let mut second = Box::pin(
            create_pooled_web_socket_stream(request(), test_options())
                .await
                .expect("second websocket stream"),
        );
        let created = second
            .next()
            .await
            .expect("second created")
            .expect("valid second created");
        assert_eq!(created.event.as_deref(), Some("response.created"));
        let _delta = second.next().await.unwrap().unwrap();
        let terminal = second
            .next()
            .await
            .expect("second terminal")
            .expect("valid terminal");
        assert!(test_terminal(&terminal));
        drop(second);

        server.await.expect("websocket server task");
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !WEBSOCKET_POOL.lock().unwrap().contains_key(&pool_key) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("idle-close task should evict the completed socket");
    }

    #[tokio::test]
    async fn clean_terminal_stream_reuses_one_connection() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio_tungstenite::accept_async;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind reuse listener");
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_server = accepted.clone();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            accepted_server.fetch_add(1, Ordering::SeqCst);
            let mut websocket = accept_async(tcp).await.unwrap();
            for request_number in 1..=2 {
                let _request = receive_request(&mut websocket).await;
                let response_id = format!("resp-reuse-{request_number}");
                send_created(&mut websocket, &response_id).await;
                websocket
                    .send(Message::Text(format!(
                        r#"{{"type":"response.output_text.delta","delta":"{request_number}"}}"#
                    )))
                    .await
                    .unwrap();
                send_completed(&mut websocket, &response_id).await;
            }
        });

        let pool_key = format!("reuse-test-{}", next_entry_id());
        let request = || PooledWebSocketRequest {
            headers: Vec::new(),
            payload: serde_json::json!({"type":"response.create"}),
            pool_key: pool_key.clone(),
            url: format!("ws://{address}"),
        };
        let mut options = test_options();
        options.idle_timeout_ms = Some(1_000);

        for _ in 0..2 {
            let mut stream = Box::pin(
                create_pooled_web_socket_stream(request(), options.clone())
                    .await
                    .unwrap(),
            );
            let created = stream.next().await.unwrap().expect("created");
            assert_eq!(created.event.as_deref(), Some("response.created"));
            assert!(!test_terminal(
                &stream.next().await.unwrap().expect("delta")
            ));
            assert!(test_terminal(
                &stream.next().await.unwrap().expect("terminal")
            ));
            assert!(stream.next().await.is_none());
        }

        server.await.unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        let entry_id = WEBSOCKET_POOL
            .lock()
            .unwrap()
            .get(&pool_key)
            .map(|entry| entry.id);
        if let Some(entry_id) = entry_id {
            remove_pooled_entry(&pool_key, entry_id);
        }
    }

    #[tokio::test]
    async fn remote_close_after_terminal_is_evicted_before_next_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio_tungstenite::accept_async;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind remote-close listener");
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_server = accepted.clone();
        let server = tokio::spawn(async move {
            for connection_index in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                accepted_server.fetch_add(1, Ordering::SeqCst);
                let mut websocket = accept_async(tcp).await.unwrap();
                let _request = receive_request(&mut websocket).await;
                let response_id = format!("resp-remote-close-{connection_index}");
                send_created(&mut websocket, &response_id).await;
                send_completed(&mut websocket, &response_id).await;
                if connection_index == 0 {
                    websocket.close(None).await.unwrap();
                }
            }
        });

        let pool_key = format!("remote-close-test-{}", next_entry_id());
        let request = || PooledWebSocketRequest {
            headers: Vec::new(),
            payload: serde_json::json!({"type":"response.create"}),
            pool_key: pool_key.clone(),
            url: format!("ws://{address}"),
        };
        let mut options = test_options();
        options.idle_timeout_ms = Some(1_000);

        let mut first = Box::pin(
            create_pooled_web_socket_stream(request(), options.clone())
                .await
                .unwrap(),
        );
        assert_eq!(
            first.next().await.unwrap().unwrap().event.as_deref(),
            Some("response.created")
        );
        assert!(test_terminal(&first.next().await.unwrap().unwrap()));
        assert!(first.next().await.is_none());
        drop(first);
        wait_until_pool_entry_is_removed(&pool_key).await;

        let mut second = Box::pin(
            create_pooled_web_socket_stream(request(), options)
                .await
                .unwrap(),
        );
        assert_eq!(
            second.next().await.unwrap().unwrap().event.as_deref(),
            Some("response.created")
        );
        assert!(test_terminal(&second.next().await.unwrap().unwrap()));
        assert!(second.next().await.is_none());
        server.await.unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn dead_reused_socket_is_preflighted_and_reopened_before_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio_tungstenite::accept_async;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind reopen listener");
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_server = accepted.clone();
        let close_first = Arc::new(tokio::sync::Notify::new());
        let close_first_server = close_first.clone();
        let server = tokio::spawn(async move {
            for connection_index in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                accepted_server.fetch_add(1, Ordering::SeqCst);
                let mut websocket = accept_async(tcp).await.unwrap();
                let _request = receive_request(&mut websocket).await;
                let response_id = format!("resp-reopen-{connection_index}");
                send_created(&mut websocket, &response_id).await;
                send_completed(&mut websocket, &response_id).await;
                if connection_index == 0 {
                    close_first_server.notified().await;
                    websocket.close(None).await.unwrap();
                }
            }
        });

        let pool_key = format!("preflight-reopen-test-{}", next_entry_id());
        let request = || PooledWebSocketRequest {
            headers: Vec::new(),
            payload: serde_json::json!({"type":"response.create"}),
            pool_key: pool_key.clone(),
            url: format!("ws://{address}"),
        };
        let mut options = test_options();
        options.idle_timeout_ms = Some(1_000);

        let mut first = Box::pin(
            create_pooled_web_socket_stream(request(), options.clone())
                .await
                .unwrap(),
        );
        assert_eq!(
            first.next().await.unwrap().unwrap().event.as_deref(),
            Some("response.created")
        );
        assert!(test_terminal(&first.next().await.unwrap().unwrap()));
        assert!(first.next().await.is_none());
        drop(first);

        // Cancel the background watcher to deterministically model a remote
        // close racing with reuse. The stale entry remains mapped, so the second
        // request must detect it in preflight and reopen before response.create.
        {
            let mut pool = WEBSOCKET_POOL.lock().unwrap();
            pool.get_mut(&pool_key)
                .expect("completed pooled entry")
                .idle_cancel = None;
        }
        tokio::task::yield_now().await;
        close_first.notify_one();
        tokio::time::sleep(Duration::from_millis(10)).await;

        let mut second = Box::pin(
            create_pooled_web_socket_stream(request(), options)
                .await
                .expect("dead pooled socket should reopen"),
        );
        assert_eq!(
            second.next().await.unwrap().unwrap().event.as_deref(),
            Some("response.created")
        );
        assert!(test_terminal(&second.next().await.unwrap().unwrap()));
        assert!(second.next().await.is_none());
        server.await.unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn malformed_terminal_evicts_socket_instead_of_marking_it_reusable() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio_tungstenite::accept_async;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind malformed-terminal listener");
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_server = accepted.clone();
        let server = tokio::spawn(async move {
            for connection_index in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                accepted_server.fetch_add(1, Ordering::SeqCst);
                let mut websocket = accept_async(tcp).await.unwrap();
                let _request = receive_request(&mut websocket).await;
                if connection_index == 0 {
                    websocket
                        .send(Message::Text(
                            r#"{"type":"response.completed","response":{"id":"malformed"}}"#
                                .to_string(),
                        ))
                        .await
                        .unwrap();
                } else {
                    send_created(&mut websocket, "resp-valid-after-malformed").await;
                    send_completed(&mut websocket, "resp-valid-after-malformed").await;
                }
            }
        });

        let pool_key = format!("malformed-terminal-test-{}", next_entry_id());
        let request = || PooledWebSocketRequest {
            headers: Vec::new(),
            payload: serde_json::json!({"type":"response.create"}),
            pool_key: pool_key.clone(),
            url: format!("ws://{address}"),
        };
        let mut options = test_options();
        options.idle_timeout_ms = Some(1_000);

        let mut malformed = Box::pin(
            create_pooled_web_socket_stream(request(), options.clone())
                .await
                .unwrap(),
        );
        let error = malformed
            .next()
            .await
            .unwrap()
            .expect_err("terminal without response.created must fail");
        assert!(error.to_string().contains("invalid Responses lifecycle"));
        drop(malformed);
        wait_until_pool_entry_is_removed(&pool_key).await;

        let mut valid = Box::pin(
            create_pooled_web_socket_stream(request(), options)
                .await
                .unwrap(),
        );
        assert_eq!(
            valid.next().await.unwrap().unwrap().event.as_deref(),
            Some("response.created")
        );
        assert!(test_terminal(&valid.next().await.unwrap().unwrap()));
        assert!(valid.next().await.is_none());
        server.await.unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ping_is_a_heartbeat_but_silence_is_bounded() {
        use tokio_tungstenite::accept_async;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind heartbeat listener");
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(tcp).await.unwrap();
            let _request = receive_request(&mut websocket).await;
            send_created(&mut websocket, "resp-heartbeat").await;
            websocket.send(Message::Ping(vec![1, 2, 3])).await.unwrap();
            send_completed(&mut websocket, "resp-heartbeat").await;
        });
        let request = PooledWebSocketRequest {
            headers: Vec::new(),
            payload: serde_json::json!({"type":"response.create"}),
            pool_key: format!("heartbeat-test-{}", next_entry_id()),
            url: format!("ws://{address}"),
        };
        let mut stream = Box::pin(
            create_pooled_web_socket_stream(request, test_options())
                .await
                .unwrap(),
        );
        let created = stream.next().await.unwrap().unwrap();
        assert_eq!(created.event.as_deref(), Some("response.created"));
        let terminal = stream.next().await.unwrap().unwrap();
        assert!(
            test_terminal(&terminal),
            "control frames must not become events"
        );
        assert!(stream.next().await.is_none());
        server.await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind silence listener");
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(tcp).await.unwrap();
            let _request = receive_request(&mut websocket).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let request = PooledWebSocketRequest {
            headers: Vec::new(),
            payload: serde_json::json!({"type":"response.create"}),
            pool_key: format!("silence-test-{}", next_entry_id()),
            url: format!("ws://{address}"),
        };
        let mut options = test_options();
        options.read_timeout = Some(Duration::from_millis(20));
        let mut stream = Box::pin(
            create_pooled_web_socket_stream(request, options)
                .await
                .unwrap(),
        );
        let error = stream
            .next()
            .await
            .expect("silence error")
            .expect_err("silence must fail");
        assert!(error.to_string().contains("no data within"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn close_without_terminal_is_an_explicit_failure() {
        use tokio_tungstenite::accept_async;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind truncated listener");
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(tcp).await.unwrap();
            let _request = receive_request(&mut websocket).await;
            send_created(&mut websocket, "resp-truncated").await;
            websocket
                .send(Message::Text(
                    r#"{"type":"response.output_text.delta","delta":"partial"}"#.to_string(),
                ))
                .await
                .unwrap();
            websocket.close(None).await.unwrap();
        });
        let request = PooledWebSocketRequest {
            headers: Vec::new(),
            payload: serde_json::json!({"type":"response.create"}),
            pool_key: format!("truncated-test-{}", next_entry_id()),
            url: format!("ws://{address}"),
        };
        let mut stream = Box::pin(
            create_pooled_web_socket_stream(request, test_options())
                .await
                .unwrap(),
        );
        assert_eq!(
            stream.next().await.unwrap().unwrap().event.as_deref(),
            Some("response.created")
        );
        assert!(stream.next().await.unwrap().is_ok());
        let error = stream
            .next()
            .await
            .expect("terminal-missing error")
            .expect_err("truncated stream must fail");
        assert_eq!(error.to_string(), "terminal missing");
        server.await.unwrap();
    }
}
