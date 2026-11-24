//! What crosses a QUIC connection before and around the NDJSON protocol.
//!
//! Three ALPNs. `tcode/pair/1` carries one bi stream with a `pair` line and its
//! `paired`/`pair_rejected` answer. `tcode/group/1` connects the machines of a
//! group; see [`crate::group`]. `tcode/1` is accepted only from a paired
//! `EndpointId`; its first bi stream opens with `hello` and, after `hello_ok`,
//! carries `ClientMessage`/`HostMessage` lines. Every control line is JSON
//! terminated by `\n`, at most [`MAX_CONTROL_LINE`] bytes, and must arrive
//! within [`CONTROL_TIMEOUT`].
//!
//! The protocol lines after `hello_ok` travel as one raw deflate stream per
//! direction ([`LineWriter`], [`LineStream`]), sync-flushed after every line
//! so each line decodes as soon as it arrives. One dictionary spans the
//! connection: a line mostly repeats the keys and ids of earlier ones.
//!
//! After hello, a further bi stream on `tcode/1` opening with
//! `{"type":"connect","host":..,"port":..}` carries one Preview TCP tunnel:
//! the machine dials `host:port` the way any local program would (loopback
//! included), answers `connected` or `connect_failed` before any tunnel
//! byte, and from then on both directions are opaque bytes. Finishing the
//! device's send side shuts down the machine's write half to the service;
//! the service closing is a finished stream back; a reset closes both. A
//! `connect` before hello is refused, and one connection holds at most
//! [`MAX_TUNNELS`] tunnels at once.
use std::{io, time::Duration};

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};
use iroh::endpoint::{QuicTransportConfig, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, BufReader};

pub const ALPN_PAIR: &[u8] = b"tcode/pair/1";
pub const ALPN_MAIN: &[u8] = b"tcode/1";

/// Bound on any control line and on the first line of any stream.
pub const MAX_CONTROL_LINE: usize = 4096;
/// How long a peer has to send a stream's first line.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on one NDJSON line after hello, in either direction.
pub const MAX_LINE: usize = tcode_protocol::MAX_LINE_BYTES;
/// Bi streams one connection may hold open at once.
pub const MAX_STREAMS: u32 = 64;
/// Preview tunnels one connection may hold open at once, leaving streams for
/// the main stream and for answering refusals.
pub const MAX_TUNNELS: usize = MAX_STREAMS as usize - 8;
/// How long the machine dials a tunnel target before answering
/// `connect_failed`.
pub const TUNNEL_CONNECT: Duration = Duration::from_secs(10);
/// A tunnel with no bytes in either direction for this long is closed.
pub const TUNNEL_IDLE: Duration = Duration::from_secs(60);

pub const KEEP_ALIVE: Duration = Duration::from_secs(10);
pub const MAX_IDLE: Duration = Duration::from_secs(30);

/// Both endpoints share one QUIC profile: keep-alives every 10 s, a 30 s idle
/// limit, and no more streams than the host is willing to serve.
pub fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .keep_alive_interval(KEEP_ALIVE)
        .max_idle_timeout(Some(
            MAX_IDLE
                .try_into()
                .expect("30 s fits the QUIC idle timeout"),
        ))
        .max_concurrent_bidi_streams(MAX_STREAMS.into())
        .max_concurrent_uni_streams(0_u32.into())
        .build()
}

/// How a device introduces itself when pairing and connecting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceClaim {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

impl DeviceClaim {
    pub fn is_valid(&self) -> bool {
        let ok = |value: &str| value.len() <= 256 && !value.chars().any(char::is_control);
        !self.name.trim().is_empty() && ok(&self.name) && self.platform.as_deref().is_none_or(ok)
    }

    /// The claim as the host records it: trimmed, empty platform dropped.
    pub fn normalized(&self) -> (String, Option<String>) {
        (
            self.name.trim().to_owned(),
            self.platform
                .as_deref()
                .map(str::trim)
                .filter(|platform| !platform.is_empty())
                .map(str::to_owned),
        )
    }
}

/// First line a device sends on a stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientLine {
    /// Present an invitation's secret to join the allow list.
    Pair {
        secret: String,
        device: DeviceClaim,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<String>,
    },
    Hello {
        protocol_version: u32,
        device: DeviceClaim,
    },
    /// Open a Preview tunnel; see the module documentation.
    Connect { host: String, port: u16 },
    Original {
        target: tcode_protocol::OriginalTarget,
    },
}

/// Whether a `connect` line names something the machine will dial: a host
/// name or IP literal without brackets, and a real port.
pub fn valid_tunnel_target(host: &str, port: u16) -> bool {
    port != 0
        && !host.is_empty()
        && host.len() <= 253
        && !host
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '[' | ']' | '/'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairRejection {
    /// Wrong, expired or already used invitation.
    Invalid,
    /// The machine is not accepting new devices.
    Disabled,
    /// The machine could not record the pairing; try again.
    Busy,
    AlreadyMember,
    SpaceUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelloRejection {
    Protocol,
    Unpaired,
}

/// The machine's answer to a [`ClientLine`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostLine {
    Paired {
        host_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<crate::group::Roster>,
    },
    PairRejected {
        reason: PairRejection,
    },
    HelloOk {
        host_name: String,
        protocol_version: u32,
        /// The group's machines, for a full device of a group's machine.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<crate::group::Roster>,
    },
    HelloRejected {
        reason: HelloRejection,
    },
    /// The tunnel target accepted the connection; tunnel bytes follow.
    Connected,
    /// The tunnel target could not be reached; the stream ends here.
    ConnectFailed {
        reason: String,
    },
    /// A stream whose first line the machine does not serve.
    Refused {
        reason: String,
    },
}

pub type LineReader = BufReader<RecvStream>;

pub fn reader(recv: RecvStream) -> LineReader {
    BufReader::new(recv)
}

/// Read one `\n`-terminated line of at most `max` bytes. `None` at a clean
/// end of stream; a partial trailing line is an error.
pub async fn read_line(reader: &mut LineReader, max: usize) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let buffered = reader.fill_buf().await?;
        if buffered.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended inside a line",
                ))
            };
        }
        let (chunk, done) = match buffered.iter().position(|byte| *byte == b'\n') {
            Some(newline) => (newline + 1, true),
            None => (buffered.len(), false),
        };
        if line.len() + chunk > max + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line exceeds {max} bytes"),
            ));
        }
        line.extend_from_slice(&buffered[..chunk]);
        reader.consume(chunk);
        if done {
            break;
        }
    }
    let mut line = String::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "line is not UTF-8"))?;
    line.truncate(line.trim_end_matches(['\n', '\r']).len());
    Ok(Some(line))
}

/// Read a control line within [`CONTROL_TIMEOUT`] and decode it.
pub async fn read_control<T: serde::de::DeserializeOwned>(
    reader: &mut LineReader,
) -> io::Result<T> {
    read_control_within(reader, MAX_CONTROL_LINE).await
}

/// [`read_control`] for a line that may be longer: a reply carrying a
/// group's machines, or a `tcode/group/1` line.
pub async fn read_control_within<T: serde::de::DeserializeOwned>(
    reader: &mut LineReader,
    max: usize,
) -> io::Result<T> {
    let line = tokio::time::timeout(CONTROL_TIMEOUT, read_line(reader, max))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control line timed out"))??
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no control line"))?;
    serde_json::from_str(&line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub async fn write_line<T: Serialize>(send: &mut SendStream, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    send.write_all(&line).await?;
    Ok(())
}

/// Writes the protocol lines of a main stream, compressed.
pub struct LineWriter {
    send: SendStream,
    deflate: Compress,
    buffer: Vec<u8>,
}

impl LineWriter {
    pub fn new(send: SendStream) -> Self {
        Self {
            send,
            deflate: Compress::new(Compression::default(), false),
            buffer: Vec::new(),
        }
    }

    pub async fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.buffer.clear();
        compress_line(&mut self.deflate, line, &mut self.buffer)?;
        self.send.write_all(&self.buffer).await?;
        Ok(())
    }

    pub fn finish(&mut self) {
        let _ = self.send.finish();
    }
}

fn compress_line(deflate: &mut Compress, line: &str, output: &mut Vec<u8>) -> io::Result<()> {
    let line = line.trim_end_matches(['\n', '\r']);
    self::deflate(deflate, line.as_bytes(), output, FlushCompress::None)?;
    self::deflate(deflate, b"\n", output, FlushCompress::Sync)
}

fn deflate(
    deflate: &mut Compress,
    mut input: &[u8],
    output: &mut Vec<u8>,
    flush: FlushCompress,
) -> io::Result<()> {
    loop {
        output.reserve(input.len() + 64);
        let consumed = deflate.total_in();
        deflate
            .compress_vec(input, output, flush)
            .map_err(io::Error::other)?;
        input = &input[(deflate.total_in() - consumed) as usize..];
        // Spare room left after a call means the flush produced everything.
        if input.is_empty() && output.len() < output.capacity() {
            return Ok(());
        }
    }
}

/// Reads the protocol lines of a main stream, decompressed.
pub struct LineStream<R = LineReader> {
    reader: R,
    inflate: Decompress,
    pending: Vec<u8>,
    /// How much of `pending` holds no newline.
    scanned: usize,
}

impl<R: AsyncBufRead + Unpin> LineStream<R> {
    /// Continue `reader` after its control line; bytes it already buffered
    /// are the start of the compressed stream.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            inflate: Decompress::new(false),
            pending: Vec::new(),
            scanned: 0,
        }
    }

    /// Read one line of at most `max` bytes, as [`read_line`] does.
    pub async fn read_line(&mut self, max: usize) -> io::Result<Option<String>> {
        loop {
            if let Some(newline) = self.pending[self.scanned..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                let end = self.scanned + newline;
                let line = self.pending.drain(..=end).take(end).collect::<Vec<_>>();
                self.scanned = 0;
                if line.len() > max {
                    return Err(line_too_long(max));
                }
                let mut line = String::from_utf8(line)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "line is not UTF-8"))?;
                line.truncate(line.trim_end_matches('\r').len());
                return Ok(Some(line));
            }
            self.scanned = self.pending.len();
            if self.pending.len() > max {
                return Err(line_too_long(max));
            }
            let compressed = self.reader.fill_buf().await?;
            if compressed.is_empty() {
                return if self.pending.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "stream ended inside a line",
                    ))
                };
            }
            let start = self.inflate.total_in();
            loop {
                let input = &compressed[(self.inflate.total_in() - start) as usize..];
                self.pending.reserve(input.len() * 4 + 1024);
                self.inflate
                    .decompress_vec(input, &mut self.pending, FlushDecompress::None)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                // A full buffer can leave output inside the inflater even with
                // every input byte consumed; spare room means it holds none.
                if self.pending.len() < self.pending.capacity() {
                    break;
                }
            }
            let consumed = (self.inflate.total_in() - start) as usize;
            self.reader.consume(consumed);
        }
    }
}

fn line_too_long(max: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("line exceeds {max} bytes"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_lines_use_the_documented_shapes() {
        let pair = ClientLine::Pair {
            space: None,
            secret: "AAECAwQFBgcICQoLDA0ODw".into(),
            device: DeviceClaim {
                name: "Phone".into(),
                platform: Some("Android 15".into()),
            },
        };
        assert_eq!(
            serde_json::to_value(&pair).unwrap(),
            serde_json::json!({"type":"pair","secret":"AAECAwQFBgcICQoLDA0ODw","device":{"name":"Phone","platform":"Android 15"}})
        );
        assert_eq!(
            serde_json::to_value(HostLine::PairRejected {
                reason: PairRejection::Invalid
            })
            .unwrap(),
            serde_json::json!({"type":"pair_rejected","reason":"invalid"})
        );
        assert_eq!(
            serde_json::from_str::<ClientLine>(
                r#"{"type":"hello","protocol_version":5,"device":{"name":"Phone"}}"#
            )
            .unwrap(),
            ClientLine::Hello {
                protocol_version: 5,
                device: DeviceClaim {
                    name: "Phone".into(),
                    platform: None
                }
            }
        );
        assert_eq!(
            serde_json::to_value(HostLine::PairRejected {
                reason: PairRejection::Disabled
            })
            .unwrap(),
            serde_json::json!({"type":"pair_rejected","reason":"disabled"})
        );
        assert_eq!(
            serde_json::to_value(HostLine::HelloRejected {
                reason: HelloRejection::Unpaired
            })
            .unwrap(),
            serde_json::json!({"type":"hello_rejected","reason":"unpaired"})
        );
        assert_eq!(
            serde_json::to_value(HostLine::HelloOk {
                host_name: "Desk".into(),
                protocol_version: 5,
                group: None,
            })
            .unwrap(),
            serde_json::json!({"type":"hello_ok","host_name":"Desk","protocol_version":5})
        );
        assert_eq!(
            serde_json::from_str::<ClientLine>(
                r#"{"type":"connect","host":"localhost","port":5173}"#
            )
            .unwrap(),
            ClientLine::Connect {
                host: "localhost".into(),
                port: 5173
            }
        );
        assert_eq!(
            serde_json::to_value(HostLine::Connected).unwrap(),
            serde_json::json!({"type":"connected"})
        );
        assert_eq!(
            serde_json::to_value(HostLine::ConnectFailed {
                reason: "refused".into()
            })
            .unwrap(),
            serde_json::json!({"type":"connect_failed","reason":"refused"})
        );
        assert!(valid_tunnel_target("::1", 80));
        assert!(!valid_tunnel_target("[::1]", 80));
        assert!(!valid_tunnel_target("localhost", 0));
        assert!(!valid_tunnel_target("", 80));
        assert!(
            !DeviceClaim {
                name: " ".into(),
                platform: None
            }
            .is_valid()
        );
        assert!(
            !DeviceClaim {
                name: "x".repeat(257),
                platform: None
            }
            .is_valid()
        );
    }

    #[tokio::test]
    async fn a_highly_compressible_line_arrives_without_waiting_for_the_next() {
        let line = format!(
            r#"{{"checking":false,"d":"{}"}}"#,
            r#"{"k":1},"#.repeat(500)
        );
        let mut compressed = Vec::new();
        compress_line(
            &mut Compress::new(Compression::default(), false),
            &line,
            &mut compressed,
        )
        .unwrap();
        // The peer keeps the stream open: nothing follows until its next line.
        let (mut peer, local) = tokio::io::duplex(64 * 1024);
        tokio::io::AsyncWriteExt::write_all(&mut peer, &compressed)
            .await
            .unwrap();
        let mut stream = LineStream::new(BufReader::new(local));
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read_line(MAX_LINE)).await;
        assert_eq!(read.expect("line held back").unwrap(), Some(line));
    }
}
