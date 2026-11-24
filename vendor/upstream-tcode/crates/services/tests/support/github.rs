use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use tcode_services::{github::Credentials, settings::SettingsStore};

#[path = "http.rs"]
mod http;
pub(crate) use http::*;

pub(crate) struct Store {
    pub(crate) store: SettingsStore,
    pub(crate) root: std::path::PathBuf,
}
impl Store {
    pub(crate) fn new() -> Self {
        let root = std::env::temp_dir().join(format!("tcode-github-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        Self {
            store: SettingsStore::new(root.clone()),
            root,
        }
    }
    pub(crate) fn credentials(&self, environment: &[(&str, &str)]) -> Arc<Credentials> {
        Credentials::new(
            self.store.clone(),
            environment
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        )
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

impl Fixture {
    pub(crate) fn serve(self, mut reply: impl FnMut(Exchange) + Send + 'static) -> Server {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match self.incoming.recv_timeout(Duration::from_millis(50)) {
                    Ok(exchange) => reply(exchange),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Server {
            stop,
            thread: Some(thread),
        }
    }
}
pub(crate) struct Server {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
