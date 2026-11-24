use std::{future::Future, pin::Pin, sync::Arc};

use tcode_protocol::{OriginalMetadata, OriginalTarget, Principal, ProtocolError, QueryResponse};

#[cfg(not(target_family = "wasm"))]
pub type OriginalFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ProtocolError>> + Send + 'a>>;
#[cfg(target_family = "wasm")]
pub type OriginalFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ProtocolError>> + 'a>>;

#[cfg(not(target_family = "wasm"))]
pub trait OriginalThread: Send {}
#[cfg(not(target_family = "wasm"))]
impl<T: Send> OriginalThread for T {}
#[cfg(target_family = "wasm")]
pub trait OriginalThread {}
#[cfg(target_family = "wasm")]
impl<T> OriginalThread for T {}

/// Reads at most 64 KiB per call. `None` certifies successful completion.
pub trait OriginalReader: OriginalThread {
    fn read(&mut self) -> OriginalFuture<'_, Option<Vec<u8>>>;
    fn cancel(&mut self);
}

pub struct OpenedOriginal {
    pub metadata: OriginalMetadata,
    pub reader: Box<dyn OriginalReader>,
}

pub trait OriginalOpener: Send + Sync {
    fn open(&self, target: OriginalTarget) -> OriginalFuture<'static, OpenedOriginal>;
}

/// Process-local ingress; the adapter supplies the authenticated principal.
pub trait OriginalBackend: Send + Sync {
    fn open(
        &self,
        principal: Principal,
        target: OriginalTarget,
    ) -> OriginalFuture<'static, OriginalSource>;
}

/// Bounded runtime output. An empty block certifies final identity and
/// authorization checks; closing without it is failure. Dropping the receiver
/// cancels a worker blocked on delivery.
pub struct OriginalSource {
    pub metadata: OriginalMetadata,
    pub blocks: async_channel::Receiver<Result<Vec<u8>, ProtocolError>>,
}

impl Drop for OriginalSource {
    fn drop(&mut self) {
        self.blocks.close();
    }
}

/// An in-process original uses the host's bounded binary ingress instead of a JSON reply.
pub struct BackendOriginal {
    pub backend: Arc<dyn OriginalBackend>,
    pub principal: Principal,
}

impl OriginalOpener for BackendOriginal {
    fn open(&self, target: OriginalTarget) -> OriginalFuture<'static, OpenedOriginal> {
        let source = self.backend.open(self.principal.clone(), target);
        Box::pin(async move {
            let source = source.await?;
            Ok(OpenedOriginal {
                metadata: source.metadata.clone(),
                reader: Box::new(BackendReader {
                    remaining: source.metadata.len,
                    source,
                }),
            })
        })
    }
}

struct BackendReader {
    source: OriginalSource,
    remaining: u64,
}

impl OriginalReader for BackendReader {
    fn read(&mut self) -> OriginalFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            let block = self.source.blocks.recv().await.map_err(error)??;
            if block.is_empty() {
                return if self.remaining == 0 {
                    Ok(None)
                } else {
                    Err(error("original ended short"))
                };
            }
            self.remaining = self
                .remaining
                .checked_sub(block.len() as u64)
                .ok_or_else(|| error("original exceeded its length"))?;
            Ok(Some(block))
        })
    }
    fn cancel(&mut self) {
        self.source.blocks.close();
    }
}

/// An explicit read, opened only when metadata or bytes are requested.
pub struct OriginalTransfer {
    opener: Arc<dyn OriginalOpener>,
    target: OriginalTarget,
    opened: Option<OpenedOriginal>,
    cancelled: bool,
    received: u64,
    finished: bool,
}

impl OriginalTransfer {
    pub(crate) fn new(opener: Arc<dyn OriginalOpener>, target: OriginalTarget) -> Self {
        Self {
            opener,
            target,
            opened: None,
            cancelled: false,
            received: 0,
            finished: false,
        }
    }

    pub async fn metadata(&mut self) -> Result<&OriginalMetadata, ProtocolError> {
        if self.cancelled {
            return Err(error("original read cancelled"));
        }
        if self.opened.is_none() {
            self.opened = Some(self.opener.open(self.target.clone()).await?);
        }
        Ok(&self.opened.as_ref().unwrap().metadata)
    }

    pub async fn read(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        if self.finished && !self.cancelled {
            return Ok(None);
        }
        self.metadata().await?;
        let block = self.opened.as_mut().unwrap().reader.read().await?;
        if let Some(block) = &block {
            self.received += block.len() as u64;
        } else {
            self.finished = true;
        }
        Ok(block)
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
        if let Some(opened) = &mut self.opened {
            opened.reader.cancel();
        }
        self.opened = None;
    }
}

impl Drop for OriginalTransfer {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn error(message: impl ToString) -> ProtocolError {
    ProtocolError {
        code: "io".into(),
        message: message.to_string(),
    }
}

pub(crate) struct LocalOriginal(pub crate::HostLink);
impl OriginalOpener for LocalOriginal {
    fn open(&self, target: OriginalTarget) -> OriginalFuture<'static, OpenedOriginal> {
        let host = self.0.clone();
        Box::pin(async move {
            let QueryResponse::FileBytes(bytes) = host.query(target.query()).await? else {
                return Err(error("unexpected original response"));
            };
            Ok(OpenedOriginal {
                metadata: OriginalMetadata {
                    len: bytes.len() as u64,
                    mime: None,
                    dimensions: None,
                },
                reader: Box::new(MemoryOriginal(std::io::Cursor::new(bytes))),
            })
        })
    }
}

struct MemoryOriginal(std::io::Cursor<Vec<u8>>);
impl OriginalReader for MemoryOriginal {
    fn read(&mut self) -> OriginalFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            use std::io::Read as _;
            let mut block = vec![0; 64 * 1024];
            let count = self.0.read(&mut block).map_err(error)?;
            block.truncate(count);
            Ok((count > 0).then_some(block))
        })
    }
    fn cancel(&mut self) {
        self.0 = std::io::Cursor::new(Vec::new());
    }
}

pub(crate) struct UnsupportedOriginal;
impl OriginalOpener for UnsupportedOriginal {
    fn open(&self, _: OriginalTarget) -> OriginalFuture<'static, OpenedOriginal> {
        Box::pin(async { Err(error("remote transport cannot read originals")) })
    }
}
