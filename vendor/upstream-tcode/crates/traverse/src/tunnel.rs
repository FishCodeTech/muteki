//! One Preview tunnel on a `tcode/1` connection: the machine side dials and
//! pumps, the device side opens and hands back a [`Tunnel`] whose halves are
//! polled from whichever executor the browser bridge runs on.
use std::{
    io,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll, ready},
    time::{Duration, Instant},
};

use iroh::endpoint::{Connection, RecvStream, SendStream};
use tcode_client::host::Tunnel;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::wire::{self, ClientLine, HostLine, LineReader};

/// Stream error code for a tunnel that ended because its TCP side failed or
/// idled, as opposed to a clean end of stream.
const RESET_BROKEN: u32 = 1;

/// Dial `host:port`, answer, then copy bytes until both directions have
/// ended, one side fails, or the tunnel idles for [`wire::TUNNEL_IDLE`].
pub(crate) async fn serve(
    mut send: SendStream,
    mut reader: LineReader,
    host: &str,
    port: u16,
) -> io::Result<()> {
    let dialed = tokio::time::timeout(
        wire::TUNNEL_CONNECT,
        tokio::net::TcpStream::connect((host, port)),
    )
    .await;
    let socket = match dialed {
        Ok(Ok(socket)) => socket,
        Ok(Err(error)) => return connect_failed(send, &dial_failure(&error)).await,
        Err(_) => return connect_failed(send, "timed out").await,
    };
    let _ = socket.set_nodelay(true);
    log::info!("preview tunnel {host}:{port}");
    wire::write_line(&mut send, &HostLine::Connected).await?;
    let (mut tcp_read, mut tcp_write) = socket.into_split();
    let activity = Mutex::new(Instant::now());
    let upload = async {
        pump(&mut reader, &mut tcp_write, &activity).await?;
        tcp_write.shutdown().await
    };
    let download = async {
        match pump(&mut tcp_read, &mut send, &activity).await {
            Ok(()) => send.finish().map_err(io::Error::other),
            Err(error) => {
                let _ = send.reset(RESET_BROKEN.into());
                Err(error)
            }
        }
    };
    let idle = async {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if activity.lock().unwrap().elapsed() >= wire::TUNNEL_IDLE {
                return;
            }
        }
    };
    tokio::select! {
        result = async { tokio::try_join!(upload, download) } => result.map(|_| ()),
        _ = idle => Err(io::Error::new(io::ErrorKind::TimedOut, "tunnel idle")),
    }
}

async fn connect_failed(mut send: SendStream, reason: &str) -> io::Result<()> {
    wire::write_line(
        &mut send,
        &HostLine::ConnectFailed {
            reason: reason.into(),
        },
    )
    .await?;
    send.finish().map_err(io::Error::other)
}

/// A short reason the device can show; the OS message may name interfaces
/// or addresses the device has no use for.
fn dial_failure(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => "connection refused".into(),
        io::ErrorKind::TimedOut => "timed out".into(),
        io::ErrorKind::NotFound | io::ErrorKind::InvalidInput => "unknown host".into(),
        io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable => "unreachable".into(),
        _ => {
            let message = error.to_string();
            message
                .split(" (os error")
                .next()
                .unwrap_or(&message)
                .chars()
                .take(80)
                .collect()
        }
    }
}

async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    activity: &Mutex<Instant>,
) -> io::Result<()> {
    let mut bytes = [0_u8; 16 * 1024];
    loop {
        let count = reader.read(&mut bytes).await?;
        if count == 0 {
            return Ok(());
        }
        *activity.lock().unwrap() = Instant::now();
        writer.write_all(&bytes[..count]).await?;
        *activity.lock().unwrap() = Instant::now();
    }
}

/// Open a tunnel to `host:port` on `connection`. Must run on the runtime.
pub(crate) async fn open(connection: &Connection, host: &str, port: u16) -> io::Result<Tunnel> {
    let (mut send, recv) = connection
        .open_bi()
        .await
        .map_err(|error| io::Error::new(io::ErrorKind::NotConnected, error))?;
    wire::write_line(
        &mut send,
        &ClientLine::Connect {
            host: host.to_owned(),
            port,
        },
    )
    .await?;
    let mut reader = wire::reader(recv);
    let reply = tokio::time::timeout(
        wire::TUNNEL_CONNECT + wire::CONTROL_TIMEOUT,
        wire::read_line(&mut reader, wire::MAX_CONTROL_LINE),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the machine did not answer"))??
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the machine closed the tunnel",
        )
    })?;
    let reply: HostLine = serde_json::from_str(&reply)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    match reply {
        HostLine::Connected => Ok(Tunnel {
            read: Box::new(TunnelRead {
                pending: reader.buffer().to_vec(),
                consumed: 0,
                stream: reader.into_inner(),
            }),
            write: Box::new(TunnelWrite(send)),
        }),
        HostLine::ConnectFailed { reason } => {
            Err(io::Error::new(io::ErrorKind::ConnectionRefused, reason))
        }
        HostLine::Refused { reason } => {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, reason))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected tunnel reply {other:?}"),
        )),
    }
}

/// The read half: bytes the line reader had already buffered, then the stream.
struct TunnelRead {
    pending: Vec<u8>,
    consumed: usize,
    stream: RecvStream,
}

impl futures_lite::AsyncRead for TunnelRead {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.consumed < this.pending.len() {
            let count = buf.len().min(this.pending.len() - this.consumed);
            buf[..count].copy_from_slice(&this.pending[this.consumed..this.consumed + count]);
            this.consumed += count;
            return Poll::Ready(Ok(count));
        }
        let mut read_buf = tokio::io::ReadBuf::new(buf);
        ready!(AsyncRead::poll_read(
            Pin::new(&mut this.stream),
            cx,
            &mut read_buf
        ))?;
        Poll::Ready(Ok(read_buf.filled().len()))
    }
}

/// The write half; closing it finishes the stream, which the machine turns
/// into a write shutdown towards the service.
struct TunnelWrite(SendStream);

impl futures_lite::AsyncWrite for TunnelWrite {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.get_mut().0), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.get_mut().0), cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.get_mut().0), cx)
    }
}
