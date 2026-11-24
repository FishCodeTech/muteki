//! A clipboard of the embedder's own: when the core has one
//! ([`crate::Core::set_clipboard`]), the app's clipboard calls (`nsh/base`'s
//! `Clipboard`, `setString`, `getString`, `hasString`) are served from it
//! rather than the system's, so that a test's clipboard is its own and the
//! machine's stays as it was (`nsh test --target`). Text inputs' own copy
//! and paste stay the system's.

use std::sync::Mutex;

/// Text kept for the app's clipboard calls.
pub trait ClipboardBackend: Send + Sync {
    /// The text it holds.
    fn text(&self) -> Option<String>;
    /// Holds `text` from now on (`None`: nothing, as after an image was
    /// copied, which the backend does not keep).
    fn set_text(&self, text: Option<String>);
}

/// A clipboard in memory.
#[derive(Debug, Default)]
pub struct MemoryClipboard {
    text: Mutex<Option<String>>,
}

impl MemoryClipboard {
    /// A clipboard holding `text` at first.
    pub fn new(text: Option<String>) -> MemoryClipboard {
        MemoryClipboard {
            text: Mutex::new(text),
        }
    }
}

impl ClipboardBackend for MemoryClipboard {
    fn text(&self) -> Option<String> {
        self.text.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_text(&self, text: Option<String>) {
        *self.text.lock().unwrap_or_else(|e| e.into_inner()) = text;
    }
}
