//! Reusable Server-Sent-Events decoder over a reqwest byte stream.
//!
//! Mirrors the TS `fetch-event-stream` `events()` helper used pervasively by the
//! streaming provider paths. Callers JSON-parse `SseEvent::data` themselves.

use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

/// Default interval between proxy-injected SSE heartbeat frames (seconds).
///
/// Long model "thinking" gaps can leave a streaming response idle-but-alive for
/// tens of seconds. Intermediaries (nginx, ALB, ...) with sub-120s idle timeouts
/// tear such a connection down even though the upstream is still working, so the
/// proxy injects a keep-alive frame whenever no real chunk has arrived for this
/// long. Kept well under the common ~60s defaults.
pub const DEFAULT_SSE_HEARTBEAT_SECS: u64 = 15;

/// SSE comment keep-alive used on OpenAI/Responses-style streams. A bare comment
/// line is ignored by every conformant SSE client, so it warms the connection
/// without perturbing the event stream.
pub const SSE_COMMENT_PING: &[u8] = b":\n\n";

/// Anthropic `ping` event keep-alive used on `/v1/messages` streams. Matches the
/// ping shape the Responses->Anthropic translation already forwards, so clients
/// see a single consistent ping frame regardless of source.
pub const ANTHROPIC_PING_FRAME: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";

/// Interval between proxy-injected SSE heartbeat frames, overridable via
/// `COPILOT_API_SSE_HEARTBEAT_SECS`. A value of `0` disables heartbeats entirely
/// (returns `None`); any positive value is the idle window after which a single
/// keep-alive frame is emitted while the upstream stays silent. Re-read per
/// stream so an operator override takes effect without a restart.
pub fn sse_heartbeat_interval() -> Option<Duration> {
    let secs = std::env::var("COPILOT_API_SSE_HEARTBEAT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SSE_HEARTBEAT_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// A single parsed SSE record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

/// Maximum bytes the decoder will buffer between record terminators. A
/// legitimate SSE record from an LLM upstream is tiny (a single JSON delta), so
/// this never trips on real traffic; it bounds memory against an upstream that
/// streams a large body without ever emitting a blank-line terminator (an OOM
/// vector otherwise, since the buffer would grow without limit).
pub const MAX_SSE_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// Incremental SSE parser. Feed raw bytes via [`Decoder::push`]; it buffers
/// across chunk boundaries and returns the records that became complete.
///
/// Factored out so the parsing core is unit-testable without a
/// `reqwest::Response`.
///
/// The buffer holds RAW bytes (not decoded text): a multi-byte UTF-8 sequence
/// can straddle a chunk boundary, and decoding each chunk independently would
/// turn each half into a `U+FFFD` replacement char, silently corrupting the
/// token. Instead, records are split on ASCII newline boundaries (which can
/// never fall inside a multi-byte code point) and only a *complete* record is
/// lossily decoded, by which point any multi-byte sequence is fully buffered.
#[derive(Debug, Default)]
pub struct Decoder {
    /// Rolling buffer of not-yet-dispatched raw bytes.
    buf: Vec<u8>,
    /// Set once the buffer exceeds [`MAX_SSE_RECORD_BYTES`] without a record
    /// terminator. [`events`] checks this and terminates the stream with an
    /// error, since [`push`] cannot itself return one.
    overflowed: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            overflowed: false,
        }
    }

    /// Whether the buffer overflowed the record-size cap. Once set, the decoder
    /// should be abandoned and the stream errored.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Push a chunk of bytes and return any records completed by it.
    ///
    /// A record is completed by a blank line (`\n\n`, tolerating `\r\n\r\n` and
    /// `\r\r`). Bytes are buffered raw and decoded only once a full record is
    /// split off, so a multi-byte char spanning a chunk boundary is never
    /// corrupted.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        // Once overflowed, the decoder is in a terminal error state: stop
        // buffering so a caller that hasn't yet checked `overflowed()` can't
        // drive further memory growth.
        if self.overflowed {
            return Vec::new();
        }

        self.buf.extend_from_slice(bytes);

        let mut out = Vec::new();
        // Cursor into `buf` of the next unscanned byte. Draining once after the
        // loop (rather than per-record) keeps this O(n) instead of O(n^2).
        let mut consumed = 0usize;
        while let Some((record_end, boundary_len)) = next_boundary(&self.buf[consumed..]) {
            let record_start = consumed;
            let record_bytes = &self.buf[record_start..record_start + record_end];
            // The full record is buffered, so lossy decode is safe here (any
            // partial multi-byte sequence would have been at the buffer tail,
            // past this boundary).
            let record = String::from_utf8_lossy(record_bytes);
            if let Some(ev) = parse_record(&record) {
                out.push(ev);
            }
            consumed = record_start + record_end + boundary_len;
        }
        if consumed > 0 {
            self.buf.drain(..consumed);
        }

        if self.buf.len() > MAX_SSE_RECORD_BYTES {
            self.overflowed = true;
            // Release the oversized allocation promptly; `events()` surfaces the
            // overflow via the latch, so the retained bytes serve no purpose.
            self.buf = Vec::new();
        }
        out
    }

    /// Flush any trailing record at stream end (no terminating blank line).
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.buf.is_empty() {
            return None;
        }
        let bytes = std::mem::take(&mut self.buf);
        let record = String::from_utf8_lossy(&bytes);
        parse_record(&record)
    }
}

/// Find the earliest blank-line record boundary in `buf`, returning
/// `(offset_of_boundary, boundary_len)` or `None` if no complete record yet.
/// All boundary byte sequences are ASCII, so they can never fall inside a
/// multi-byte UTF-8 code point.
fn next_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None; // (boundary_start, boundary_len)
    for (pat, len) in [(b"\r\n\r\n".as_slice(), 4usize), (b"\n\n", 2), (b"\r\r", 2)] {
        if let Some(idx) = find_subslice(buf, pat) {
            match best {
                Some((b, _)) if b <= idx => {}
                _ => best = Some((idx, len)),
            }
        }
    }
    best
}

/// First index of `needle` within `haystack`, or `None`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parse a single record (block of lines) into an [`SseEvent`].
/// Returns `None` if the record carries no meaningful fields (e.g. only
/// comments), matching the spec's "no dispatch for empty event" behavior.
fn parse_record(record: &str) -> Option<SseEvent> {
    let mut id: Option<String> = None;
    let mut event: Option<String> = None;
    // Build the data payload in place: avoids a per-event Vec allocation plus the
    // extra String copy that `join("\n")` performs (this runs once per SSE event,
    // i.e. per streamed token delta). Equivalent to joining the data lines with
    // '\n'.
    let mut data = String::new();
    let mut saw_data = false;

    for raw_line in record.split('\n') {
        // Normalize a trailing `\r` (handles `\r\n` line endings within a record).
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        if line.is_empty() {
            continue;
        }
        // Comment line.
        if line.starts_with(':') {
            continue;
        }

        let (field, value) = match line.split_once(':') {
            Some((f, v)) => {
                // Strip a single leading space after the colon per the spec.
                let v = v.strip_prefix(' ').unwrap_or(v);
                (f, v)
            }
            // A line with no colon is a field with an empty value.
            None => (line, ""),
        };

        match field {
            "data" => {
                if saw_data {
                    data.push('\n');
                }
                data.push_str(value);
                saw_data = true;
            }
            "event" => {
                event = Some(value.to_string());
            }
            // The spec ignores an id containing a NUL; otherwise set it.
            "id" if !value.contains('\u{0}') => {
                id = Some(value.to_string());
            }
            // "retry" and unknown fields are ignored.
            _ => {}
        }
    }

    // Per the SSE spec, a record only dispatches an event when it carried at
    // least one `data:` field; records with only `event:`/`id:` (e.g. bare
    // keep-alives) do not emit.
    if !saw_data {
        return None;
    }

    Some(SseEvent { id, event, data })
}

/// Decode a `reqwest::Response` byte stream into a stream of [`SseEvent`]s.
///
/// Mirrors the TS `events(response)` helper. Byte-stream errors are mapped to
/// `std::io::Error` via `std::io::Error::other`, matching the chat handler.
pub fn events(resp: reqwest::Response) -> impl Stream<Item = Result<SseEvent, std::io::Error>> {
    let mut byte_stream = resp.bytes_stream();
    async_stream::try_stream! {
        let mut decoder = Decoder::new();
        while let Some(chunk) = byte_stream.next().await {
            let bytes: Bytes = chunk.map_err(std::io::Error::other)?;
            for ev in decoder.push(&bytes) {
                yield ev;
            }
            // Bound memory: an upstream that never terminates a record would
            // otherwise grow the buffer without limit. Terminate the stream.
            if decoder.overflowed() {
                Err(std::io::Error::other(
                    "SSE record exceeded the maximum buffered size",
                ))?;
            }
        }
        if let Some(ev) = decoder.finish() {
            yield ev;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream::{self, StreamExt};

    #[test]
    fn heartbeat_ping_frames_are_well_formed_sse() {
        // The comment ping must be a bare SSE comment line; the Anthropic ping
        // must match the `{"type":"ping"}` frame the translation path forwards.
        assert_eq!(SSE_COMMENT_PING, b":\n\n");
        assert_eq!(
            std::str::from_utf8(ANTHROPIC_PING_FRAME).unwrap(),
            "event: ping\ndata: {\"type\":\"ping\"}\n\n"
        );
        let data = std::str::from_utf8(ANTHROPIC_PING_FRAME)
            .unwrap()
            .strip_prefix("event: ping\ndata: ")
            .and_then(|s| s.strip_suffix("\n\n"))
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(parsed["type"], "ping");
        assert_eq!(DEFAULT_SSE_HEARTBEAT_SECS, 15);
    }

    /// Drive the pure decoder over a sequence of byte chunks, including the
    /// final flush, returning all events in order.
    fn decode_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut decoder = Decoder::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(decoder.push(c));
        }
        if let Some(ev) = decoder.finish() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn parses_single_event() {
        let events = decode_all(&[b"data: hello\n\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                id: None,
                event: None,
                data: "hello".to_string(),
            }]
        );
    }

    #[test]
    fn multiple_data_lines_joined_with_newline() {
        let events = decode_all(&[b"event: message\ndata: line1\ndata: line2\n\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                id: None,
                event: Some("message".to_string()),
                data: "line1\nline2".to_string(),
            }]
        );
    }

    #[test]
    fn record_split_across_two_chunks() {
        // The blank-line boundary only arrives in the second chunk.
        let events = decode_all(&[b"data: par", b"tial chunk\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "partial chunk");
    }

    #[test]
    fn boundary_split_across_chunks() {
        // The `\n\n` terminator itself is split between chunks.
        let events = decode_all(&[b"data: x\n", b"\ndata: y\n\n"]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "x");
        assert_eq!(events[1].data, "y");
    }

    #[test]
    fn id_and_event_fields() {
        let events = decode_all(&[b"id: 42\nevent: ping\ndata: {\"a\":1}\n\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                id: Some("42".to_string()),
                event: Some("ping".to_string()),
                data: "{\"a\":1}".to_string(),
            }]
        );
    }

    #[test]
    fn tolerates_crlf_line_endings() {
        let events = decode_all(&[b"event: e\r\ndata: d\r\n\r\n"]);
        assert_eq!(
            events,
            vec![SseEvent {
                id: None,
                event: Some("e".to_string()),
                data: "d".to_string(),
            }]
        );
    }

    #[test]
    fn ignores_comment_lines() {
        let events = decode_all(&[b": this is a comment\ndata: real\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn comment_only_record_is_skipped() {
        let events = decode_all(&[b": just a comment\n\ndata: after\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "after");
    }

    #[test]
    fn strips_only_single_leading_space() {
        // Two leading spaces -> one is preserved.
        let events = decode_all(&[b"data:  two-spaces\n\n"]);
        assert_eq!(events[0].data, " two-spaces");
    }

    #[test]
    fn field_with_no_value() {
        let events = decode_all(&[b"data\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "");
    }

    #[test]
    fn trailing_record_flushed_without_blank_line() {
        let events = decode_all(&[b"data: tail"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "tail");
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let events = decode_all(&[b"data: a\n\ndata: b\n\ndata: c\n\n"]);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].data, "a");
        assert_eq!(events[1].data, "b");
        assert_eq!(events[2].data, "c");
    }

    #[test]
    fn multibyte_char_split_across_chunks_is_not_corrupted() {
        // A 4-byte emoji (🚀 = f0 9f 9a 80) split across two push() calls must
        // decode intact. The old String-per-chunk lossy decode turned each half
        // into U+FFFD; the byte-buffered decoder must not.
        let rocket = "🚀".as_bytes();
        assert_eq!(rocket.len(), 4);
        let mut decoder = Decoder::new();
        let mut out = Vec::new();
        // Split the emoji: first 2 bytes in chunk 1, last 2 + terminator in chunk 2.
        out.extend(decoder.push(b"data: "));
        out.extend(decoder.push(&rocket[..2]));
        out.extend(decoder.push(&rocket[2..]));
        out.extend(decoder.push(b"\n\n"));
        if let Some(ev) = decoder.finish() {
            out.push(ev);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "data: 🚀".trim_start_matches("data: ")); // "🚀"
        assert_eq!(out[0].data, "🚀");
    }

    #[test]
    fn multibyte_char_split_with_cjk() {
        // 3-byte CJK char 世 (e4 b8 96) straddling a boundary.
        let cjk = "世".as_bytes();
        assert_eq!(cjk.len(), 3);
        let mut decoder = Decoder::new();
        let mut out = Vec::new();
        out.extend(decoder.push(b"data: a"));
        out.extend(decoder.push(&cjk[..1]));
        out.extend(decoder.push(&cjk[1..]));
        out.extend(decoder.push(b"b\n\n"));
        if let Some(ev) = decoder.finish() {
            out.push(ev);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "a世b");
    }

    #[test]
    fn many_records_in_one_push_all_parsed() {
        // Linearity regression: a single push carrying many records must yield
        // them all (and, with the cursor-drain rewrite, do so in O(n)).
        let mut blob = Vec::new();
        for i in 0..1000 {
            blob.extend_from_slice(format!("data: {i}\n\n").as_bytes());
        }
        let mut decoder = Decoder::new();
        let out = decoder.push(&blob);
        assert_eq!(out.len(), 1000);
        assert_eq!(out[0].data, "0");
        assert_eq!(out[999].data, "999");
        assert!(decoder.finish().is_none());
    }

    #[test]
    fn overflow_latch_trips_without_terminator() {
        let mut decoder = Decoder::new();
        // Push more than the cap with no record terminator.
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..((MAX_SSE_RECORD_BYTES / chunk.len()) + 2) {
            decoder.push(&chunk);
        }
        assert!(decoder.overflowed(), "expected overflow latch to trip");
        // On overflow the oversized allocation is released...
        assert!(
            decoder.buf.is_empty(),
            "buffer should be cleared on overflow"
        );
        // ...and further pushes are no-ops that cannot regrow memory.
        let out = decoder.push(&chunk);
        assert!(out.is_empty());
        assert!(
            decoder.buf.is_empty(),
            "push after overflow must not buffer"
        );
    }

    #[test]
    fn no_overflow_for_normal_terminated_records() {
        let mut decoder = Decoder::new();
        for _ in 0..10_000 {
            decoder.push(b"data: small record\n\n");
        }
        assert!(!decoder.overflowed());
        assert!(decoder.buf.is_empty());
    }

    #[tokio::test]
    async fn try_stream_decodes_from_byte_stream() {
        // Exercise the same parsing core via an async stream built from an
        // iterator of Bytes, without needing a reqwest::Response.
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"data: hel")),
            Ok(Bytes::from_static(b"lo\n\ndata: world\n\n")),
        ];
        let mut byte_stream = stream::iter(chunks);

        let parsed = async_stream::try_stream! {
            let mut decoder = Decoder::new();
            while let Some(chunk) = byte_stream.next().await {
                let bytes: Bytes = chunk.map_err(std::io::Error::other)?;
                for ev in decoder.push(&bytes) {
                    yield ev;
                }
            }
            if let Some(ev) = decoder.finish() {
                yield ev;
            }
        };
        futures_util::pin_mut!(parsed);

        let mut got: Vec<SseEvent> = Vec::new();
        while let Some(item) = parsed.next().await {
            let item: Result<SseEvent, std::io::Error> = item;
            got.push(item.unwrap());
        }

        assert_eq!(got.len(), 2);
        assert_eq!(got[0].data, "hello");
        assert_eq!(got[1].data, "world");
    }
}
