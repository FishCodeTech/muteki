//! One process-wide tokio runtime shared by host and client endpoints, so
//! the rest of the application never needs tokio.
use std::sync::OnceLock;

pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tcode-traverse")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Run `future` to completion from a non-tokio thread.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    runtime().block_on(future)
}
