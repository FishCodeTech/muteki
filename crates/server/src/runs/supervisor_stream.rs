//! Bounded Worker output; transport failures have a separate, reserved delivery slot.
use super::container_exec::SupervisorFrame;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};

pub const MAX_FRAME_BYTES: usize = 512 * 1024;
pub const EARLY_FRAME_CAPACITY: usize = 32;
const FRAME_CAPACITY: usize = 1024;
const BUFFER_BYTES: usize = 8 * 1024 * 1024;

struct BufferedFrame {
    frame: SupervisorFrame,
    _bytes: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub struct Sender {
    frames: mpsc::Sender<BufferedFrame>,
    bytes: Arc<Semaphore>,
    failure: watch::Sender<Option<SupervisorFrame>>,
}

pub struct Stream {
    frames: mpsc::Receiver<BufferedFrame>,
    failure: watch::Receiver<Option<SupervisorFrame>>,
    failure_open: bool,
    done: bool,
}

pub fn channel() -> (Sender, Stream) {
    let (send, receive) = mpsc::channel(FRAME_CAPACITY);
    let (failure, failures) = watch::channel(None);
    (
        Sender {
            frames: send,
            bytes: Arc::new(Semaphore::new(BUFFER_BYTES)),
            failure,
        },
        Stream {
            frames: receive,
            failure: failures,
            failure_open: true,
            done: false,
        },
    )
}

impl Sender {
    pub fn is_closed(&self) -> bool {
        self.frames.is_closed() || self.failure.borrow().is_some()
    }

    fn validate(&self, frame: &SupervisorFrame) -> Result<u32, ()> {
        if self.is_closed() {
            return Err(());
        }
        let size = serde_json::to_vec(frame).map_or(usize::MAX, |bytes| bytes.len());
        if size > MAX_FRAME_BYTES {
            self.fail(&frame.worker_id, "Worker frame exceeds the 512 KiB limit");
            return Err(());
        }
        Ok((size + std::mem::size_of::<SupervisorFrame>()) as u32)
    }

    fn fail(&self, worker: &str, message: &str) {
        self.failure.send_if_modified(|failure| {
            if failure.is_some() {
                return false;
            }
            *failure = Some(SupervisorFrame {
                t: "transport_error".into(),
                worker_id: worker.into(),
                error: message.into(),
                ..Default::default()
            });
            true
        });
    }

    pub fn send(&self, frame: SupervisorFrame) -> Result<(), ()> {
        let size = self.validate(&frame)?;
        let Ok(bytes) = self.bytes.clone().try_acquire_many_owned(size) else {
            self.fail(
                &frame.worker_id,
                "Worker output exceeded its 8 MiB buffer; output is incomplete",
            );
            return Err(());
        };
        match self.frames.try_send(BufferedFrame {
            frame,
            _bytes: bytes,
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(frame)) => {
                self.fail(
                    &frame.frame.worker_id,
                    "Worker output consumer exceeded its bounded buffer; output is incomplete",
                );
                Err(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
        }
    }

    pub async fn send_async(&self, frame: SupervisorFrame) -> Result<(), ()> {
        let size = self.validate(&frame)?;
        let bytes = tokio::select! {
            permit = self.bytes.clone().acquire_many_owned(size) => permit.map_err(|_| ())?,
            _ = self.frames.closed() => return Err(()),
        };
        self.frames
            .send(BufferedFrame {
                frame,
                _bytes: bytes,
            })
            .await
            .map_err(|_| ())
    }
}

impl Stream {
    pub async fn recv(&mut self) -> Option<SupervisorFrame> {
        if self.done {
            return None;
        }
        loop {
            let failure = self.failure.borrow().clone();
            if let Some(failure) = failure {
                self.done = true;
                self.frames.close();
                return Some(failure);
            }
            tokio::select! {
                biased;
                changed = self.failure.changed(), if self.failure_open => {
                    if changed.is_err() { self.failure_open = false; }
                }
                frame = self.frames.recv() => return frame.map(|buffered| buffered.frame),
            }
        }
    }
}
