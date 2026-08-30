//! Just enough HTTP/1.1 to issue one authenticated GET.
//!
//! Shared by the management-API client and the `top` viewer. Both need a single
//! plaintext GET against a private endpoint, which does not justify pulling in a
//! full client stack and its TLS tree — the metrics server in this crate is
//! hand-rolled for the same reason.
//!
//! Plaintext only. Put a local reverse proxy in front of anything that requires
//! TLS and point the relevant URL at it.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Guard against an endpoint that streams without end.
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("{0}")]
    Transport(String),
    #[error("HTTP {0}")]
    Status(u16),
    #[error("malformed response: {0}")]
    Body(String),
}

/// One plaintext GET. `authorization` is the raw header value (e.g. `Basic ...`).
pub async fn get(
    host: &str,
    port: u16,
    path: &str,
    authorization: Option<&str>,
    timeout: Duration,
) -> Result<String, HttpError> {
    let fut = async {
        let mut stream = TcpStream::connect((host, port))
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        let auth = match authorization {
            Some(value) => format!("Authorization: {value}\r\n"),
            None => String::new(),
        };
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}\
             Accept: */*\r\nUser-Agent: effiqueue\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?;

        let mut raw = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| HttpError::Transport(e.to_string()))?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
            if raw.len() > MAX_RESPONSE {
                return Err(HttpError::Body("response too large".into()));
            }
        }
        parse_response(&raw)
    };

    match tokio::time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => Err(HttpError::Transport("request timed out".into())),
    }
}

/// Split a response into status + body, de-chunking when necessary.
pub fn parse_response(raw: &[u8]) -> Result<String, HttpError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| HttpError::Body("no header terminator".into()))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];

    let mut lines = head.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Body("empty response".into()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| HttpError::Body(format!("bad status line '{status_line}'")))?;
    if !(200..300).contains(&status) {
        return Err(HttpError::Status(status));
    }

    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    String::from_utf8(body).map_err(|e| HttpError::Body(e.to_string()))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::with_capacity(body.len());
    loop {
        let eol = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| HttpError::Body("truncated chunk header".into()))?;
        let header = String::from_utf8_lossy(&body[..eol]);
        // A chunk header may carry extensions after a ';'.
        let size_hex = header.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| HttpError::Body(format!("bad chunk size '{size_hex}'")))?;
        body = &body[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size {
            return Err(HttpError::Body("truncated chunk body".into()));
        }
        out.extend_from_slice(&body[..size]);
        // Skip the chunk's trailing CRLF.
        body = body.get(size + 2..).unwrap_or(&[]);
    }
}

/// Standard base64 with padding. Small enough not to warrant a dependency.
pub fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"guest:guest"), "Z3Vlc3Q6Z3Vlc3Q=");
    }

    #[test]
    fn parses_a_content_length_response() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 9\r\n\r\n{\"a\": 1}\n";
        assert_eq!(parse_response(raw).unwrap(), "{\"a\": 1}\n");
    }

    #[test]
    fn parses_a_chunked_response() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n3\r\n 1}\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), "{\"a\": 1}");
    }

    #[test]
    fn surfaces_http_errors() {
        assert!(matches!(
            parse_response(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"),
            Err(HttpError::Status(404))
        ));
        assert!(matches!(
            parse_response(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"),
            Err(HttpError::Status(401))
        ));
    }
}
