use std::{
    cell::RefCell,
    collections::HashMap,
    slice,
    sync::atomic::{AtomicU64, Ordering},
};
use tcode_client::speech::{SpeechError, SpeechEvent, SpeechRecognizer, SpeechSession};

pub(crate) struct IosRecognizer;
struct Session(u64);

thread_local! {
    static EVENTS: RefCell<HashMap<u64, async_channel::Sender<SpeechEvent>>> = RefCell::new(HashMap::new());
}
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

unsafe extern "C" {
    fn tcode_ios_host_speech_available(locale: *const u8, length: usize) -> u8;
    fn tcode_ios_host_speech_start(id: u64, locale: *const u8, length: usize);
    fn tcode_ios_host_speech_stop(id: u64, finish: u8);
}

impl SpeechRecognizer for IosRecognizer {
    fn is_available(&self, locale: &str) -> bool {
        // SAFETY: Swift copies the locale synchronously on the UIKit main thread.
        unsafe { tcode_ios_host_speech_available(locale.as_ptr(), locale.len()) != 0 }
    }

    fn start(
        &self,
        locale: &str,
        events: async_channel::Sender<SpeechEvent>,
    ) -> Result<Box<dyn SpeechSession>, SpeechError> {
        let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        EVENTS.with(|sessions| sessions.borrow_mut().insert(id, events));
        // SAFETY: Swift copies the locale and retains the session until stop.
        unsafe { tcode_ios_host_speech_start(id, locale.as_ptr(), locale.len()) };
        Ok(Box::new(Session(id)))
    }
}

impl SpeechSession for Session {
    fn finish(&mut self) {
        // SAFETY: Swift ignores unknown/already-finished sessions.
        unsafe { tcode_ios_host_speech_stop(self.0, 1) };
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        EVENTS.with(|sessions| sessions.borrow_mut().remove(&self.0));
        // SAFETY: Swift cancellation is idempotent and releases capture.
        unsafe { tcode_ios_host_speech_stop(self.0, 0) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_speech_event(id: u64, kind: u32, bytes: *const u8, length: usize) {
    let text = if length == 0 {
        String::new()
    } else {
        if bytes.is_null() {
            return;
        }
        // SAFETY: Swift keeps its UTF-8 buffer alive for this synchronous call.
        String::from_utf8_lossy(unsafe { slice::from_raw_parts(bytes, length) }).into_owned()
    };
    let event = match kind {
        0 => SpeechEvent::Ready,
        1 => SpeechEvent::Hypothesis(text),
        2 => SpeechEvent::Phrase(text),
        3 => SpeechEvent::Finished,
        5 => SpeechEvent::PermissionDenied,
        6 => SpeechEvent::Unsupported,
        _ => SpeechEvent::Error(text.into()),
    };
    EVENTS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        if let Some(sender) = sessions.get(&id) {
            let _ = sender.try_send(event);
        }
        if kind >= 3 {
            sessions.remove(&id);
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn tcode_ios_speech_availability_changed() {
    crate::entry::refresh_windows();
}
