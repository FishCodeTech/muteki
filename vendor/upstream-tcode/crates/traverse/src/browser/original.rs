use std::{io, sync::Arc};

use tcode_protocol::{OriginalTarget, Principal, ProtocolError};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};

use super::{Request, Shared};
use crate::{http::response, original::IDLE};

pub(super) async fn serve<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request: Request,
    shared: &Arc<Shared>,
) -> io::Result<()> {
    let token = request
        .headers
        .get("authorization")
        .and_then(|header| header.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned();
    let device = shared.auth.lock().unwrap().device_id_for_token(&token);
    let Some(device) = device else {
        return response(stream, "401 Unauthorized", "text/plain", b"invalid token").await;
    };
    if let Some(origin) = request.headers.get("origin") {
        let same = url::Url::parse(origin).ok().is_some_and(|url| {
            url.scheme() == "http"
                && url.authority()
                    == request
                        .headers
                        .get("host")
                        .map(String::as_str)
                        .unwrap_or_default()
        });
        if !same {
            return response(stream, "403 Forbidden", "text/plain", b"foreign origin").await;
        }
    }
    let target = request
        .target
        .split_once('?')
        .and_then(|(_, query)| {
            url::form_urlencoded::parse(query.as_bytes()).find(|(key, _)| key == "target")
        })
        .and_then(|(_, value)| serde_json::from_str::<OriginalTarget>(&value).ok());
    let Some(target) = target else {
        return response(stream, "400 Bad Request", "text/plain", b"invalid target").await;
    };
    let transfer = tokio::time::timeout(
        IDLE,
        shared.mux.open_original(
            format!("browser:{device}"),
            Principal::Full,
            target,
            shared.policy,
        ),
    )
    .await;
    let transfer = match transfer {
        Ok(Ok(transfer)) => transfer,
        Ok(Err(error)) => return failure(stream, error).await,
        Err(error) => return failure(stream, tcode_client::original::error(error)).await,
    };
    let source = &transfer.source;
    let len = source.metadata.len;
    let (start, end, partial) = match request.headers.get("range") {
        None => (0, len, false),
        Some(range) => match parse_range(range, len) {
            Some((start, end)) => (start, end, true),
            None => {
                stream.write_all(format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await?;
                return stream.flush().await;
            }
        },
    };
    let mime = source
        .metadata
        .mime
        .as_deref()
        .filter(|mime| !mime.contains(['\r', '\n']))
        .unwrap_or("application/octet-stream");
    let mut head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nCache-Control: no-store\r\nConnection: close\r\n",
        if partial {
            "206 Partial Content"
        } else {
            "200 OK"
        },
        end - start
    );
    if partial {
        head.push_str(&format!(
            "Content-Range: bytes {start}-{}/{len}\r\n",
            end - 1
        ));
    }
    if let Some((width, height)) = source.metadata.dimensions {
        head.push_str(&format!("X-Image-Dimensions: {width}x{height}\r\n"));
    }
    head.push_str("\r\n");
    if len == 0 {
        let complete = tokio::time::timeout(IDLE, source.blocks.recv()).await;
        match complete {
            Ok(Ok(Ok(block))) if block.is_empty() => {}
            Ok(Ok(Err(error))) => return failure(stream, error).await,
            _ => {
                return failure(
                    stream,
                    tcode_client::original::error("invalid empty original"),
                )
                .await;
            }
        }
    }
    let body = async {
        write_progress(stream, head.as_bytes()).await?;
        let mut position = 0_u64;
        let mut held = Vec::new();
        while position < len {
            let block = tokio::time::timeout(IDLE, source.blocks.recv())
                .await
                .map_err(io::Error::other)?
                .map_err(io::Error::other)?
                .map_err(|error| io::Error::other(error.message))?;
            if block.is_empty() {
                return Err(io::Error::other("original ended short"));
            }
            let next = position + block.len() as u64;
            if next > len {
                return Err(io::Error::other("original exceeded length"));
            }
            let from = start.max(position);
            let to = end.min(next);
            if from < to {
                if !held.is_empty() {
                    write_progress(stream, &held).await?;
                }
                held = block[(from - position) as usize..(to - position) as usize].to_vec();
            }
            position = next;
        }
        if len > 0 {
            let completion = tokio::time::timeout(IDLE, source.blocks.recv())
                .await
                .map_err(io::Error::other)?
                .map_err(io::Error::other)?
                .map_err(|error| io::Error::other(error.message))?;
            if !completion.is_empty() {
                return Err(io::Error::other("missing original completion"));
            }
        }
        if shared.auth.lock().unwrap().device_id_for_token(&token) != Some(device) {
            return Err(io::Error::other("token revoked"));
        }
        write_progress(stream, &held).await?;
        stream.flush().await
    };
    let revoked = async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if shared.auth.lock().unwrap().device_id_for_token(&token) != Some(device) {
                break;
            }
        }
    };
    tokio::select! {
        result = body => result,
        _ = revoked => Err(io::Error::other("token revoked")),
    }
}

fn parse_range(value: &str, len: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    if start >= len {
        return None;
    }
    let end = if end.is_empty() {
        len
    } else {
        end.parse::<u64>().ok()?.saturating_add(1).min(len)
    };
    (end > start).then_some((start, end))
}

async fn write_progress<S: AsyncWrite + Unpin>(stream: &mut S, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let count = tokio::time::timeout(IDLE, stream.write(bytes))
            .await
            .map_err(io::Error::other)??;
        if count == 0 {
            return Err(io::Error::other("original write stopped"));
        }
        bytes = &bytes[count..];
    }
    Ok(())
}

async fn failure<S: AsyncWrite + Unpin>(stream: &mut S, error: ProtocolError) -> io::Result<()> {
    let status = match error.code.as_str() {
        "out_of_scope" => "403 Forbidden",
        "not_found" => "404 Not Found",
        "changed" => "409 Conflict",
        "busy" => "429 Too Many Requests",
        _ => "500 Internal Server Error",
    };
    response(stream, status, "text/plain", error.message.as_bytes()).await
}
