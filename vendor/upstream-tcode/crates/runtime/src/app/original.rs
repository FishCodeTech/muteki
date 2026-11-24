use std::fs::File;
use std::io::{self, Read as _, Seek as _};
use std::time::SystemTime;

use base64::engine::general_purpose::STANDARD;
use base64::read::DecoderReader;
use tcode_client::original::{OriginalSource, error};
use tcode_protocol::{ClientPayload, OriginalMetadata, OriginalTarget, Principal, ProtocolError};
use tcode_services::image_previews;

use super::{AppState, HostCx, HostTask, history::SessionLog};

const BLOCK: usize = 64 * 1024;
type Answer = async_channel::Sender<Result<OriginalSource, ProtocolError>>;

impl AppState {
    pub(crate) fn start_original(
        &mut self,
        principal: Principal,
        target: OriginalTarget,
        answer: Answer,
        cx: &mut HostCx,
    ) {
        self.observe_principal(&principal);
        if let Err(error) = self.authorize(&principal, &ClientPayload::Query(target.query()), cx) {
            let _ = answer.try_send(Err(error));
            return;
        }
        let item = match &target {
            OriginalTarget::Item {
                session_id,
                item_id,
                image_index,
            } => Some(self.recorded_image(session_id, item_id.clone(), *image_index, true, cx)),
            OriginalTarget::File { .. } => None,
        };
        let host = cx.clone();
        cx.spawn_background(async move {
            let result = async {
                if answer.is_closed() {
                    return Err(error("original read cancelled"));
                }
                let mut source = match &target {
                    OriginalTarget::File { path } => {
                        let requested_path = path.clone();
                        host.unblock(move || {
                            let path = requested_path.canonicalize().map_err(io_error)?;
                            let mut file = File::open(&path).map_err(io_error)?;
                            let metadata = file.metadata().map_err(io_error)?;
                            if !metadata.is_file() {
                                return Err(error("original is not a regular file"));
                            }
                            let identity = (metadata.len(), metadata.modified().map_err(io_error)?);
                            let dimensions = image_previews::dimensions(&mut file);
                            file.rewind().map_err(io_error)?;
                            Ok(Source::File {
                                file,
                                path,
                                requested_path,
                                identity,
                                dimensions,
                            })
                        })
                        .await?
                    }
                    OriginalTarget::Item { .. } => {
                        let mut image = item.unwrap().await?;
                        host.unblock(move || {
                            image.dimensions = image_previews::dimensions(DecoderReader::new(
                                image.data_base64.as_bytes(),
                                &STANDARD,
                            ));
                            Source::Item(image)
                        })
                        .await
                    }
                };
                let checked = match &source {
                    Source::File { path, .. } => OriginalTarget::File { path: path.clone() },
                    _ => target.clone(),
                };
                check(&host, principal.clone(), checked).await?;
                let (blocks, received) = async_channel::bounded(2);
                if answer
                    .try_send(Ok(OriginalSource {
                        metadata: source.metadata(),
                        blocks: received,
                    }))
                    .is_err()
                {
                    return Ok(());
                }
                let output = blocks.clone();
                let tail = host
                    .unblock(move || {
                        let read: Box<dyn io::Read + '_> = match &mut source {
                            Source::File { file, .. } => Box::new(file),
                            Source::Item(image) => Box::new(DecoderReader::new(
                                image.data_base64.as_bytes(),
                                &STANDARD,
                            )),
                        };
                        let tail = stream_blocks(read, &output)?;
                        if let Source::File {
                            file,
                            identity,
                            requested_path,
                            ..
                        } = &source
                            && (file_identity(file.metadata().map_err(io_error)?)? != *identity
                                || file_identity(
                                    std::fs::metadata(requested_path).map_err(io_error)?,
                                )? != *identity)
                        {
                            return Err(changed());
                        }
                        Ok((source, tail))
                    })
                    .await;
                let finish = async {
                    let (source, tail) = tail?;
                    check(&host, principal, target.clone()).await?;
                    if let (
                        Source::Item(sent),
                        OriginalTarget::Item {
                            session_id,
                            item_id,
                            image_index,
                        },
                    ) = (source, target)
                    {
                        let current = host
                            .enqueue_and_wait(move |state, cx| {
                                state.recorded_image(&session_id, item_id, image_index, false, cx)
                            })
                            .await
                            .map_err(|_| error("host stopped"))?
                            .await;
                        if current.map(|image| image.identity).ok() != Some(sent.identity) {
                            return Err(changed());
                        }
                    }
                    if !tail.is_empty() {
                        blocks.send(Ok(tail)).await.map_err(error)?;
                    }
                    blocks.send(Ok(Vec::new())).await.map_err(error)?;
                    Ok(())
                }
                .await;
                if let Err(error) = finish {
                    let _ = blocks.send(Err(error)).await;
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                let _ = answer.try_send(Err(error));
            }
        })
        .detach();
    }

    /// A recorded image as the session's history holds it now, with its bytes only when `bytes`
    /// is set.
    fn recorded_image(
        &mut self,
        session_id: &str,
        item_id: String,
        image_index: usize,
        bytes: bool,
        cx: &mut HostCx,
    ) -> HostTask<Result<ItemImage, ProtocolError>> {
        let address = item_id.clone();
        self.query_item(
            session_id,
            item_id,
            move |log: &SessionLog| {
                let (row, image) = log.original_image(&address, image_index)?;
                let len = tcode_services::image_bytes::decoded_len(&image.data_base64)
                    .map_err(io_error)?;
                Ok(ItemImage {
                    identity: (row, len),
                    mime: image.media_type.clone(),
                    data_base64: if bytes {
                        image.data_base64.clone()
                    } else {
                        String::new()
                    },
                    dimensions: None,
                })
            },
            cx,
        )
    }
}

async fn check(
    host: &HostCx,
    principal: Principal,
    target: OriginalTarget,
) -> Result<(), ProtocolError> {
    host.enqueue_and_wait(move |state, cx| {
        state.authorize(&principal, &ClientPayload::Query(target.query()), cx)
    })
    .await
    .map_err(|_| error("host stopped"))?
}

enum Source {
    File {
        file: File,
        path: std::path::PathBuf,
        requested_path: std::path::PathBuf,
        identity: (u64, SystemTime),
        dimensions: Option<(u32, u32)>,
    },
    Item(ItemImage),
}

struct ItemImage {
    /// The last row that recorded the item, and the image's decoded length.
    identity: (u64, u64),
    mime: String,
    data_base64: String,
    dimensions: Option<(u32, u32)>,
}

impl Source {
    fn metadata(&self) -> OriginalMetadata {
        match self {
            Self::File {
                path,
                identity,
                dimensions,
                ..
            } => OriginalMetadata {
                len: identity.0,
                mime: Some(tcode_core::attachments::mime_from_path(path)),
                dimensions: *dimensions,
            },
            Self::Item(image) => OriginalMetadata {
                len: image.identity.1,
                mime: Some(image.mime.clone()),
                dimensions: image.dimensions,
            },
        }
    }
}

fn stream_blocks(
    mut read: Box<dyn io::Read + '_>,
    output: &async_channel::Sender<Result<Vec<u8>, ProtocolError>>,
) -> Result<Vec<u8>, ProtocolError> {
    let mut held = Vec::new();
    loop {
        if output.is_closed() {
            return Err(error("original read cancelled"));
        }
        // A base64 decoder yields 768 bytes per read, and every block is a
        // hand-over to the viewer's thread, so blocks are filled before they go.
        let mut block = Vec::with_capacity(BLOCK);
        let count = read
            .by_ref()
            .take(BLOCK as u64)
            .read_to_end(&mut block)
            .map_err(io_error)?;
        if count == 0 {
            return Ok(held);
        }
        if !held.is_empty() {
            output.send_blocking(Ok(held)).map_err(error)?;
        }
        held = block;
    }
}

fn file_identity(metadata: std::fs::Metadata) -> Result<(u64, SystemTime), ProtocolError> {
    Ok((metadata.len(), metadata.modified().map_err(io_error)?))
}

fn changed() -> ProtocolError {
    ProtocolError {
        code: "changed".into(),
        message: "original changed during the read".into(),
    }
}

fn io_error(err: io::Error) -> ProtocolError {
    if err.kind() == io::ErrorKind::NotFound {
        ProtocolError {
            code: "not_found".into(),
            message: err.to_string(),
        }
    } else {
        error(err)
    }
}
