//! Feedback ownership follows authenticated MCP sessions, not registration metadata.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(crate) struct FeedbackSession {
    state: Arc<FeedbackState>,
}

#[derive(Debug)]
struct FeedbackState {
    id: u64,
    generation: AtomicU64,
    closed: AtomicBool,
}

impl FeedbackSession {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(FeedbackState {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                generation: AtomicU64::new(0),
                closed: AtomicBool::new(false),
            }),
        })
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> FeedbackRun {
        FeedbackRun {
            ticket: FeedbackTicket {
                owner: self.state.id,
                action: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                generation: self.state.generation.load(Ordering::Acquire),
                session: Arc::downgrade(&self.state),
                cancellation,
            },
            completed: false,
        }
    }

    pub(crate) fn stop(&self) {
        self.state.closed.store(true, Ordering::Release);
        self.cancel();
    }

    pub(crate) fn cancel(&self) {
        self.state.generation.fetch_add(1, Ordering::AcqRel);
        crate::backend::clear_feedback(Some(self.state.id), None);
    }
}

impl Drop for FeedbackSession {
    fn drop(&mut self) {
        crate::backend::clear_feedback(Some(self.state.id), None);
    }
}

pub(crate) struct FeedbackRun {
    ticket: FeedbackTicket,
    completed: bool,
}

impl FeedbackRun {
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn ticket(&self) -> FeedbackTicket {
        self.ticket.clone()
    }
    pub(crate) fn is_current(&self) -> bool {
        self.ticket.is_current()
    }
    /// A finished run, whether or not its actions worked, keeps the cursor for
    /// its bounded idle tail. Cancellation and a dropped request clear their
    /// own current marker immediately.
    pub(crate) fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for FeedbackRun {
    fn drop(&mut self) {
        if !self.completed {
            crate::backend::clear_feedback(Some(self.ticket.owner), Some(self.ticket.action));
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FeedbackTicket {
    pub(crate) owner: u64,
    pub(crate) action: u64,
    generation: u64,
    session: Weak<FeedbackState>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
}

impl FeedbackTicket {
    pub(crate) fn is_current(&self) -> bool {
        !self
            .cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
            && self.session.upgrade().is_some_and(|session| {
                !session.closed.load(Ordering::Acquire)
                    && session.generation.load(Ordering::Acquire) == self.generation
            })
    }
}
