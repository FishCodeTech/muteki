// Copyright 2024 - 2026 Longbridge <https://longbridge.com>
// Licensed under Apache-2.0; see LICENSE-APACHE.
// Modified for Tcode client sessions and native capture.

//! On-device Apple dictation. The permission flow is adapted from
//! gpui-component 0.7.1 (Apache-2.0); capture and recognition run in
//! `apple/MacDictation.swift`, since SpeechAnalyzer is Swift-only.

use super::{SpeechError, SpeechEvent, SpeechRecognizer, SpeechSession, level::LevelMeter};
use async_channel::{Receiver, Sender, unbounded};
use block2::RcBlock;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_foundation::{NSBundle, NSString};
use objc2_speech::{SFSpeechRecognizer, SFSpeechRecognizerAuthorizationStatus};
use std::{
    cell::RefCell,
    ffi::c_void,
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};

type Callback = extern "C" fn(*const c_void, u32, *const c_void, usize, f64);
type Release = extern "C" fn(*const c_void);

unsafe extern "C" {
    fn tcode_macos_speech_available(locale: *const u8, length: usize) -> u8;
    fn tcode_macos_speech_start(
        id: u64,
        locale: *const u8,
        length: usize,
        context: *const c_void,
        callback: Callback,
        release: Release,
    );
    fn tcode_macos_speech_stop(id: u64, finish: u8);
}

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

pub struct SystemRecognizer {
    supported: bool,
}

impl SystemRecognizer {
    pub fn new(locale: &str) -> Self {
        // Asking without usage descriptions terminates the process.
        let described = [
            "NSSpeechRecognitionUsageDescription",
            "NSMicrophoneUsageDescription",
        ]
        .iter()
        .all(|key| {
            NSBundle::mainBundle()
                .objectForInfoDictionaryKey(&NSString::from_str(key))
                .is_some()
        });
        // SAFETY: Swift copies the locale before returning.
        let supported = described
            && unsafe { tcode_macos_speech_available(locale.as_ptr(), locale.len()) } != 0;
        Self { supported }
    }
}

impl SpeechRecognizer for SystemRecognizer {
    fn is_available(&self, _: &str) -> bool {
        self.supported && !speech_denied()
    }

    fn start(
        &self,
        locale: &str,
        sink: Sender<SpeechEvent>,
    ) -> Result<Box<dyn SpeechSession>, SpeechError> {
        if !self.supported {
            return Err(SpeechError::Unsupported);
        }
        let (events, receiver) = unbounded();
        let recognition = Rc::new(RefCell::new(Recognition {
            id: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            locale: locale.to_string(),
            sink,
            events: events.clone(),
            started: false,
            finishing: false,
            done: false,
        }));
        // SAFETY: reading authorization never prompts.
        let status = unsafe { SFSpeechRecognizer::authorizationStatus() };
        if status == SFSpeechRecognizerAuthorizationStatus::NotDetermined {
            let handler = RcBlock::new(move |status| {
                _ = events.try_send(Event::Authorized(
                    status == SFSpeechRecognizerAuthorizationStatus::Authorized,
                ));
            });
            // SAFETY: usage descriptions were checked; callback only sends a message.
            unsafe { SFSpeechRecognizer::requestAuthorization(&handler) };
        } else {
            _ = events.try_send(Event::Authorized(
                status == SFSpeechRecognizerAuthorizationStatus::Authorized,
            ));
        }
        let task = Box::pin(run(recognition.clone(), receiver));
        Ok(Box::new(Session { recognition, task }))
    }
}

fn speech_denied() -> bool {
    // SAFETY: reading authorization never prompts.
    let status = unsafe { SFSpeechRecognizer::authorizationStatus() };
    status == SFSpeechRecognizerAuthorizationStatus::Denied
        || status == SFSpeechRecognizerAuthorizationStatus::Restricted
}

enum Event {
    Authorized(bool),
    Microphone(bool),
}

/// Ask for access, then hand the session to Swift, which reports to the sink.
async fn run(recognition: Rc<RefCell<Recognition>>, events: Receiver<Event>) {
    while let Ok(event) = events.recv().await {
        let mut recognition = recognition.borrow_mut();
        match event {
            Event::Authorized(true) => recognition.authorize_microphone(),
            Event::Authorized(false) | Event::Microphone(false) => {
                recognition.emit(SpeechEvent::PermissionDenied);
                recognition.done = true;
            }
            Event::Microphone(true) => recognition.begin(),
        }
        if recognition.done || recognition.started {
            break;
        }
    }
}

struct Session {
    recognition: Rc<RefCell<Recognition>>,
    task: Pin<Box<dyn Future<Output = ()>>>,
}

impl SpeechSession for Session {
    fn finish(&mut self) {
        self.recognition.borrow_mut().finish();
    }
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.task.as_mut().poll(cx)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let recognition = self.recognition.borrow();
        if recognition.started {
            // SAFETY: Swift ignores sessions that already ended.
            unsafe { tcode_macos_speech_stop(recognition.id, 0) };
        }
    }
}

struct Recognition {
    id: u64,
    locale: String,
    sink: Sender<SpeechEvent>,
    events: Sender<Event>,
    started: bool,
    finishing: bool,
    done: bool,
}

impl Recognition {
    fn emit(&self, event: SpeechEvent) {
        _ = self.sink.try_send(event);
    }

    fn authorize_microphone(&self) {
        // SAFETY: reading the audio media type constant.
        let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
            self.emit(SpeechEvent::Unsupported);
            return;
        };
        // SAFETY: the audio media type is valid for capture authorization.
        let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) };
        let events = self.events.clone();
        if status == AVAuthorizationStatus::NotDetermined {
            let handler = RcBlock::new(move |granted: objc2::runtime::Bool| {
                _ = events.try_send(Event::Microphone(granted.as_bool()));
            });
            // SAFETY: usage descriptions were checked; callback only sends a message.
            unsafe {
                AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &handler)
            };
        } else {
            _ = events.try_send(Event::Microphone(
                status == AVAuthorizationStatus::Authorized,
            ));
        }
    }

    fn begin(&mut self) {
        if self.finishing {
            self.emit(SpeechEvent::Finished);
            self.done = true;
            return;
        }
        let context = Arc::into_raw(Arc::new(Shared {
            sink: self.sink.clone(),
            meter: Mutex::new(None),
        }));
        // SAFETY: Swift copies the locale, reports to `context` from any thread
        // and hands it to `release` exactly once.
        unsafe {
            tcode_macos_speech_start(
                self.id,
                self.locale.as_ptr(),
                self.locale.len(),
                context.cast(),
                on_event,
                release,
            )
        };
        self.started = true;
    }

    fn finish(&mut self) {
        if self.finishing {
            return;
        }
        self.finishing = true;
        if self.started {
            // SAFETY: Swift flushes the last result, then reports Finished.
            unsafe { tcode_macos_speech_stop(self.id, 1) };
        }
    }
}

/// What Swift's callbacks reach, on whichever thread they run.
struct Shared {
    sink: Sender<SpeechEvent>,
    meter: Mutex<Option<LevelMeter>>,
}

const SAMPLES: u32 = 7;

extern "C" fn on_event(
    context: *const c_void,
    kind: u32,
    data: *const c_void,
    length: usize,
    sample_rate: f64,
) {
    // SAFETY: `context` stays alive until Swift calls `release`.
    let shared = unsafe { &*context.cast::<Shared>() };
    if kind == SAMPLES {
        if data.is_null() {
            return;
        }
        // SAFETY: Swift passes the tap buffer's first channel for this call.
        let samples = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), length) };
        let mut meter = shared.meter.lock().unwrap();
        let meter = meter.get_or_insert_with(|| LevelMeter::new(sample_rate));
        for &sample in samples {
            if let Some(level) = meter.push(sample) {
                _ = shared.sink.try_send(SpeechEvent::Level(level));
            }
        }
        return;
    }
    let text = if length == 0 || data.is_null() {
        String::new()
    } else {
        // SAFETY: Swift keeps its UTF-8 buffer alive for this call.
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) };
        String::from_utf8_lossy(bytes).into_owned()
    };
    let event = match kind {
        0 => SpeechEvent::Ready,
        1 => SpeechEvent::Hypothesis(text),
        2 => SpeechEvent::Phrase(text),
        3 => SpeechEvent::Finished,
        5 => SpeechEvent::Error(SpeechError::NoInputDevice),
        6 => SpeechEvent::Unsupported,
        _ => SpeechEvent::Error(text.into()),
    };
    _ = shared.sink.try_send(event);
}

extern "C" fn release(context: *const c_void) {
    // SAFETY: the reference `begin` leaked, handed back once by Swift.
    drop(unsafe { Arc::from_raw(context.cast::<Shared>()) });
}
