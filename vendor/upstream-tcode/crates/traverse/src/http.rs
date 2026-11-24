//! The HTTP/1.1 request head, as the browser listener and the Preview
//! proxy read it. Bounded on every dimension; nothing past the head (or the
//! declared body, where one is read) is consumed from the stream.
use std::collections::HashMap;
use std::io;

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
};

pub(crate) const MAX_HEAD_BYTES: usize = 16 * 1024;
pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024;

pub(crate) struct Request {
    pub method: String,
    pub path: String,
    #[cfg_attr(not(feature = "browser"), allow(dead_code))]
    pub target: String,
    pub headers: HashMap<String, String>,
    /// Empty for proxy requests, whose bodies stay in the stream; only the
    /// browser listener reads one.
    #[cfg_attr(not(feature = "browser"), allow(dead_code))]
    pub body: Vec<u8>,
}

pub(crate) async fn read_request<S>(stream: &mut S) -> io::Result<Request>
where
    S: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let available = stream.fill_buf().await?;
        if available.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed during HTTP head",
            ));
        }
        let scanned = bytes.len();
        let take = available.len().min(MAX_HEAD_BYTES - scanned);
        bytes.extend_from_slice(&available[..take]);
        // The terminator may straddle two reads.
        let from = scanned.saturating_sub(3);
        if let Some(position) = bytes[from..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            let head_end = from + position + 4;
            stream.consume(head_end - scanned);
            bytes.truncate(head_end);
            break;
        }
        stream.consume(take);
        if bytes.len() >= MAX_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP head exceeds 16 KiB",
            ));
        }
    }
    let head = std::str::from_utf8(&bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "HTTP head is not UTF-8"))?;
    let mut lines = head[..head.len() - 4].split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default();
    let target = request_parts.next().unwrap_or_default();
    let version = request_parts.next().unwrap_or_default();
    if method.is_empty()
        || target.is_empty()
        || version != "HTTP/1.1"
        || request_parts.next().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed HTTP request line",
        ));
    }
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP header"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
            || headers
                .insert(name.to_ascii_lowercase(), value.trim().to_owned())
                .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid or duplicate HTTP header",
            ));
        }
    }
    let proxy = method == "CONNECT" || target.starts_with("http://");
    let content_length = match headers.get("content-length").filter(|_| !proxy) {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad Content-Length"))?,
        None => 0,
    };
    if content_length > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HTTP body exceeds 64 KiB",
        ));
    }
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body).await.map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed during HTTP body",
            )
        } else {
            error
        }
    })?;
    let path = if proxy {
        target
    } else {
        target.split('?').next().unwrap_or(target)
    }
    .to_owned();
    Ok(Request {
        method: method.to_owned(),
        path,
        target: target.to_owned(),
        headers,
        body,
    })
}

pub(crate) async fn response<S>(
    stream: &mut S,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    response_with_body_mode(stream, status, content_type, body, false).await
}

/// HEAD has the same representation headers as GET, without a body.
pub(crate) async fn response_with_body_mode<S>(
    stream: &mut S,
    status: &str,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    if !head_only {
        stream.write_all(body).await?;
    }
    stream.flush().await
}

#[cfg(feature = "browser")]
pub(crate) fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The head is taken from the stream and nothing after it, however the
    /// bytes arrive.
    #[tokio::test]
    async fn head_ends_at_the_first_blank_line_and_leaves_the_rest_in_the_stream() {
        for chunk in [1, 3, 4, 7, 1024] {
            let raw =
                b"CONNECT localhost:1 HTTP/1.1\r\nHost: localhost:1\r\n\r\n\x16\x03tls".to_vec();
            let (mut client, server) = tokio::io::duplex(64);
            let writer = tokio::spawn(async move {
                for piece in raw.chunks(chunk) {
                    client.write_all(piece).await.unwrap();
                }
            });
            let mut server = tokio::io::BufReader::new(server);
            let request = read_request(&mut server).await.unwrap();
            assert_eq!(request.method, "CONNECT");
            assert_eq!(request.path, "localhost:1");
            assert_eq!(request.headers["host"], "localhost:1");
            writer.await.unwrap();
            let mut rest = [0; 5];
            server.read_exact(&mut rest).await.unwrap();
            assert_eq!(&rest, b"\x16\x03tls", "chunk size {chunk}");
        }
    }

    #[tokio::test]
    async fn oversized_or_malformed_heads_are_refused() {
        for raw in [
            format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_HEAD_BYTES)),
            "GET / HTTP/1.0\r\n\r\n".into(),
            "GET / HTTP/1.1\r\nBad header\r\n\r\n".into(),
            "GET / HTTP/1.1\r\nA: 1\r\na: 2\r\n\r\n".into(),
            format!(
                "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY_BYTES + 1
            ),
        ] {
            let mut stream = tokio::io::BufReader::new(std::io::Cursor::new(raw.into_bytes()));
            assert!(read_request(&mut stream).await.is_err());
        }
    }
}
