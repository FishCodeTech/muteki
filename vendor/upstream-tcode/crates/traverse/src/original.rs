use std::{io, time::Duration};

use iroh::endpoint::{Connection, SendStream};
use tcode_client::original::{OpenedOriginal, OriginalFuture, OriginalReader, error};
use tcode_protocol::{
    OriginalMetadata, OriginalReply, OriginalRequest, OriginalTarget, Principal, ProtocolError,
};
use tokio::io::AsyncReadExt as _;

use crate::{
    HostMux,
    wire::{self, LineReader},
};

pub(crate) const IDLE: Duration = Duration::from_secs(60);
const CANCEL: u32 = 0x100;
const BROKEN: u32 = 0x104;

pub(crate) async fn serve(
    mux: &HostMux,
    device: String,
    principal: Principal,
    target: OriginalTarget,
    policy: tcode_client::attachment::AttachmentPolicy,
    mut send: SendStream,
) -> io::Result<()> {
    let _ = send.set_priority(-10);
    let stopped = send.stopped();
    let result = tokio::select! {
        stopped = stopped => match stopped {
            Ok(None) => Ok(()),
            _ => Err(io::Error::other("original cancelled")),
        },
        result = async {
            let admitted = match timed(mux.open_original(device, principal, target, policy)).await? {
                Ok(source) => source,
                Err(error) => {
                    wire::write_line(&mut send, &OriginalReply::OriginalFailed { error }).await?;
                    send.finish()?;
                    return Ok(());
                }
            };
            let source = &admitted.source;
            timed(wire::write_line(&mut send, &OriginalReply::OriginalReady { len: source.metadata.len, mime: source.metadata.mime.clone(), dimensions: source.metadata.dimensions })).await??;
            write_progress(&mut send, &source.metadata.len.to_be_bytes()).await?;
            let mut sent = 0_u64;
            loop {
                let block = timed(source.blocks.recv()).await?.map_err(io::Error::other)?.map_err(|err| io::Error::other(err.message))?;
                if block.is_empty() { break; }
                sent += block.len() as u64;
                if sent > source.metadata.len { return Err(io::Error::other("original exceeded its length")); }
                write_progress(&mut send, &block).await?;
            }
            if sent != source.metadata.len { return Err(io::Error::other("original ended short")); }
            timed(wire::write_line(&mut send, &OriginalReply::OriginalComplete)).await??;
            send.finish()?;
            // Keep admission until QUIC acknowledges completion or the peer cancels.
            if timed(send.stopped()).await?.map_err(io::Error::other)?.is_some() {
                return Err(io::Error::other("original cancelled"));
            }
            Ok(())
        } => result,
    };
    if result.is_err() {
        let _ = send.reset(BROKEN.into());
    }
    result
}

async fn timed<T>(future: impl std::future::Future<Output = T>) -> io::Result<T> {
    tokio::time::timeout(IDLE, future).await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "original made no progress for 60 seconds",
        )
    })
}

pub(crate) async fn write_progress(send: &mut SendStream, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let count = timed(send.write(bytes)).await?.map_err(io::Error::other)?;
        if count == 0 {
            return Err(io::Error::other("original write stopped"));
        }
        bytes = &bytes[count..];
    }
    Ok(())
}

type ReadResult = Result<Option<Vec<u8>>, ProtocolError>;
struct NativeReader {
    reads: async_channel::Sender<async_channel::Sender<ReadResult>>,
}
impl OriginalReader for NativeReader {
    fn read(&mut self) -> OriginalFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            let (answer, result) = async_channel::bounded(1);
            self.reads.send(answer).await.map_err(error)?;
            result.recv().await.map_err(error)?
        })
    }
    fn cancel(&mut self) {
        self.reads.close();
    }
}
impl Drop for NativeReader {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(crate) async fn open(
    connection: &Connection,
    target: OriginalTarget,
) -> Result<OpenedOriginal, ProtocolError> {
    let (mut send, recv) = timed(connection.open_bi())
        .await
        .map_err(error)?
        .map_err(error)?;
    timed(wire::write_line(
        &mut send,
        &OriginalRequest::Original { target },
    ))
    .await
    .map_err(error)?
    .map_err(error)?;
    let mut reader = wire::reader(recv);
    let line = timed(wire::read_line(&mut reader, wire::MAX_CONTROL_LINE))
        .await
        .map_err(error)?
        .map_err(error)?
        .ok_or_else(|| error("original ended before metadata"))?;
    let metadata = match serde_json::from_str(&line).map_err(error)? {
        OriginalReply::OriginalReady {
            len,
            mime,
            dimensions,
        } => OriginalMetadata {
            len,
            mime,
            dimensions,
        },
        OriginalReply::OriginalFailed { error } => return Err(error),
        _ => return Err(error("unexpected original reply")),
    };
    let prefix = timed(reader.read_u64())
        .await
        .map_err(error)?
        .map_err(error)?;
    if prefix != metadata.len {
        return Err(error("original length prefix disagrees with metadata"));
    }
    let (reads, requested) = async_channel::bounded::<async_channel::Sender<ReadResult>>(1);
    let cancelled = reads.clone();
    crate::runtime().spawn(async move {
        let mut remaining = prefix;
        tokio::select! {
            _ = cancelled.closed() => {}
            _ = async {
                while let Ok(answer) = requested.recv().await {
                    let result = read_block(&mut reader, &mut remaining).await.map_err(error);
                    let finished = !matches!(result, Ok(Some(_)));
                    let _ = answer.try_send(result);
                    if finished { break; }
                }
            } => {}
        }
        let _ = reader.get_mut().stop(CANCEL.into());
        let _ = send.reset(CANCEL.into());
    });
    Ok(OpenedOriginal {
        metadata,
        reader: Box::new(NativeReader { reads }),
    })
}

async fn read_block(reader: &mut LineReader, remaining: &mut u64) -> io::Result<Option<Vec<u8>>> {
    if *remaining > 0 {
        let mut block = vec![0; (*remaining).min(64 * 1024) as usize];
        let count = timed(reader.read(&mut block)).await??;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "original ended inside its body",
            ));
        }
        block.truncate(count);
        *remaining -= count as u64;
        return Ok(Some(block));
    }
    let line = timed(wire::read_line(reader, wire::MAX_CONTROL_LINE))
        .await??
        .ok_or_else(|| io::Error::other("missing original completion"))?;
    if !matches!(
        serde_json::from_str::<OriginalReply>(&line)?,
        OriginalReply::OriginalComplete
    ) {
        return Err(io::Error::other("invalid original completion"));
    }
    if timed(reader.read(&mut [0])).await?? != 0 {
        return Err(io::Error::other("bytes after original completion"));
    }
    Ok(None)
}
