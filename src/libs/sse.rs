//! Reusable Server-Sent-Events decoder over a reqwest byte stream.
//!
//! Mirrors the TS `fetch-event-stream` `events()` helper used pervasively by the
//! streaming provider paths. Callers JSON-parse `SseEvent::data` themselves.

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

/// A single parsed SSE record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE parser. Feed raw bytes via [`Decoder::push`]; it buffers
/// across chunk boundaries and returns the records that became complete.
///
/// Factored out so the parsing core is unit-testable without a
/// `reqwest::Response`.
#[derive(Debug, Default)]
pub struct Decoder {
    /// Rolling buffer of not-yet-dispatched bytes (UTF-8 text).
    buf: String,
}

impl Decoder {
    pub fn new() -> Self {
        Self { buf: String::new() }
    }

    /// Push a chunk of bytes and return any records completed by it.
    ///
    /// A record is completed by a blank line (`\n\n`, tolerating `\r\n\r\n`).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        // SSE is UTF-8; lossily decode so a stray invalid byte cannot wedge the
        // stream. Split points are ASCII newlines, so multi-byte chars are safe.
        self.buf.push_str(&String::from_utf8_lossy(bytes));

        let mut out = Vec::new();
        while let Some((record, rest)) = split_record(&self.buf) {
            if let Some(ev) = parse_record(&record) {
                out.push(ev);
            }
            self.buf = rest;
        }
        out
    }

    /// Flush any trailing record at stream end (no terminating blank line).
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.buf.is_empty() {
            return None;
        }
        let record = std::mem::take(&mut self.buf);
        parse_record(&record)
    }
}

/// Split off the first complete record (terminated by a blank line) from `buf`.
/// Returns `(record_without_terminator, remainder)` or `None` if incomplete.
fn split_record(buf: &str) -> Option<(String, String)> {
    // Find the earliest record boundary. Records end at a blank line, which the
    // SSE spec defines after normalizing line endings; tolerate both `\n\n` and
    // `\r\n\r\n` (and the mixed `\n\r\n`).
    let mut best: Option<(usize, usize)> = None; // (boundary_start, boundary_len)
    for (pat, len) in [("\r\n\r\n", 4usize), ("\n\n", 2), ("\r\r", 2)] {
        if let Some(idx) = buf.find(pat) {
            match best {
                Some((b, _)) if b <= idx => {}
                _ => best = Some((idx, len)),
            }
        }
    }
    let (idx, len) = best?;
    let record = buf[..idx].to_string();
    let rest = buf[idx + len..].to_string();
    Some((record, rest))
}

/// Parse a single record (block of lines) into an [`SseEvent`].
/// Returns `None` if the record carries no meaningful fields (e.g. only
/// comments), matching the spec's "no dispatch for empty event" behavior.
fn parse_record(record: &str) -> Option<SseEvent> {
    let mut id: Option<String> = None;
    let mut event: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();
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
                data_lines.push(value.to_string());
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

    Some(SseEvent {
        id,
        event,
        data: data_lines.join("\n"),
    })
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
