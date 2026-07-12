// Mirrors src/lib/zstd-request.ts: transparently decompresses zstd-encoded
// request bodies before they reach route handlers.

use std::io::Read;

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::header::{CONTENT_ENCODING, CONTENT_LENGTH};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

const ZSTD_CONTENT_ENCODING: &str = "zstd";

/// Maximum size of the *compressed* request body we will read (16 MiB).
const MAX_COMPRESSED_BYTES: usize = 16 * 1024 * 1024;

/// Maximum size of the *decompressed* output. Bounds a zstd decompression bomb:
/// a small frame can otherwise expand to many GiB and OOM-kill the worker. Tied
/// to the per-request body limit so the middleware buffer can never exceed the
/// same advertised bound the rest of the stack enforces (a decompressed body over
/// this is rejected by the handler's extractor anyway).
const MAX_DECOMPRESSED_BYTES: usize = crate::libs::http::MAX_REQUEST_BODY_BYTES;

/// Failure modes of [`decompress_zstd`].
#[derive(Debug)]
enum DecompressError {
    /// The frame was malformed or could not be decoded.
    Invalid,
    /// The decompressed output exceeded [`MAX_DECOMPRESSED_BYTES`].
    TooLarge,
}

pub async fn zstd_decompression_middleware(req: Request, next: Next) -> Response {
    let is_zstd = req
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(ZSTD_CONTENT_ENCODING));

    if !is_zstd {
        return next.run(req).await;
    }

    let (mut parts, body) = req.into_parts();

    let compressed = match to_bytes(body, MAX_COMPRESSED_BYTES).await {
        Ok(bytes) => bytes,
        // `to_bytes` errors when the body exceeds the cap (or on a read error).
        Err(_) => return payload_too_large(),
    };

    let decompressed = match tokio::task::spawn_blocking(move || decompress_zstd(&compressed)).await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(DecompressError::TooLarge)) => return payload_too_large(),
        _ => return invalid_body(),
    };

    parts.headers.remove(CONTENT_ENCODING);
    parts.headers.remove(CONTENT_LENGTH);

    let rebuilt = Request::from_parts(parts, Body::from(decompressed));
    next.run(rebuilt).await
}

fn decompress_zstd(input: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut decoder =
        ruzstd::decoding::StreamingDecoder::new(input).map_err(|_| DecompressError::Invalid)?;

    // Read in bounded chunks so a decompression bomb can't expand past the
    // ceiling. `read_to_end` would happily grow the Vec to many GiB.
    let mut output = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = decoder
            .read(&mut chunk)
            .map_err(|_| DecompressError::Invalid)?;
        if n == 0 {
            break;
        }
        if output.len() + n > MAX_DECOMPRESSED_BYTES {
            return Err(DecompressError::TooLarge);
        }
        output.extend_from_slice(&chunk[..n]);
    }
    Ok(output)
}

fn invalid_body() -> Response {
    crate::libs::error::anthropic_error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request_error",
        "Failed to decompress zstd request body.",
    )
}

fn payload_too_large() -> Response {
    crate::libs::error::anthropic_error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "request_too_large",
        "Request body is too large.",
    )
}

#[cfg(test)]
mod tests {
    use super::{decompress_zstd, invalid_body, payload_too_large};
    use axum::http::StatusCode;
    use http_body_util::BodyExt;

    // zstd frame for the bytes "hello zstd world" (produced by the zstd CLI).
    const HELLO_FRAME: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x58, 0x81, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20,
        0x7a, 0x73, 0x74, 0x64, 0x20, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0x7f, 0x81, 0x68, 0x60,
    ];

    #[test]
    fn decompresses_a_valid_frame() {
        let out = decompress_zstd(HELLO_FRAME).expect("valid frame decompresses");
        assert_eq!(out, b"hello zstd world");
    }

    #[test]
    fn rejects_non_zstd_input() {
        assert!(decompress_zstd(b"not a zstd frame at all").is_err());
    }

    async fn assert_complete_error(
        response: axum::response::Response,
        status: StatusCode,
        error_type: &str,
        message: &str,
    ) {
        assert_eq!(response.status(), status);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect error body")
            .to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("error response is JSON");
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], error_type);
        assert_eq!(body["error"]["message"], message);
    }

    #[tokio::test]
    async fn zstd_failures_use_complete_anthropic_envelopes() {
        assert_complete_error(
            invalid_body(),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Failed to decompress zstd request body.",
        )
        .await;
        assert_complete_error(
            payload_too_large(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "Request body is too large.",
        )
        .await;
    }
}
