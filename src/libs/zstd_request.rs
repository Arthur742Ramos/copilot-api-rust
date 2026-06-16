// Mirrors src/lib/zstd-request.ts: transparently decompresses zstd-encoded
// request bodies before they reach route handlers.

use std::io::Read;

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::header::{CONTENT_ENCODING, CONTENT_LENGTH};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

const ZSTD_CONTENT_ENCODING: &str = "zstd";

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

    let compressed = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return invalid_body(),
    };

    let decompressed = match tokio::task::spawn_blocking(move || decompress_zstd(&compressed)).await
    {
        Ok(Ok(bytes)) => bytes,
        _ => return invalid_body(),
    };

    parts.headers.remove(CONTENT_ENCODING);
    parts.headers.remove(CONTENT_LENGTH);

    let rebuilt = Request::from_parts(parts, Body::from(decompressed));
    next.run(rebuilt).await
}

fn decompress_zstd(input: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(input)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let mut output = Vec::new();
    decoder.read_to_end(&mut output)?;
    Ok(output)
}

fn invalid_body() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "message": "Failed to decompress zstd request body.",
                "type": "invalid_request_error",
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::decompress_zstd;

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
}
