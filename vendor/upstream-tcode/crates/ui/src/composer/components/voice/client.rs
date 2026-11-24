use gpui::{App, Context, EventEmitter, Task};
use std::{cell::RefCell, rc::Rc, time::Duration};
use tcode_client::speech::{Dictation, SpeechEvent, SpeechRecognizer, SpeechSession};
pub(super) use tcode_client::speech::{DictationEvent, SpeechStatus};

struct Session {
    platform: Rc<RefCell<Box<dyn SpeechSession>>>,
    id: u64,
    _events: Task<()>,
    _timeout: Option<Task<()>>,
}

pub(super) struct SpeechState {
    recognizer: Option<Rc<dyn SpeechRecognizer>>,
    locale: &'static str,
    dictation: Dictation,
    session: Option<Session>,
}

impl EventEmitter<DictationEvent> for SpeechState {}

impl SpeechState {
    pub(super) fn new(_: &mut Context<Self>) -> Self {
        Self {
            recognizer: None,
            locale: "en-US",
            dictation: Dictation::default(),
            session: None,
        }
    }

    pub(super) fn recognizer(mut self, recognizer: Rc<dyn SpeechRecognizer>) -> Self {
        self.recognizer = Some(recognizer);
        self
    }

    pub(super) fn has_recognizer(&self) -> bool {
        self.recognizer.is_some()
    }
    pub(super) fn is_available(&self, _: &App) -> bool {
        self.recognizer
            .as_ref()
            .is_some_and(|recognizer| recognizer.is_available(self.locale))
    }
    pub(super) fn status(&self) -> SpeechStatus {
        self.dictation.status()
    }
    pub(super) fn level(&self) -> f32 {
        self.dictation.level()
    }

    pub(super) fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.status().is_active() {
            self.stop(cx);
        } else {
            self.start(cx);
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(recognizer) = self.recognizer.clone() else {
            return;
        };
        let id = self.dictation.start();
        let (sender, receiver) = async_channel::unbounded();
        let platform = match recognizer.start(self.locale, sender) {
            Ok(platform) => Rc::new(RefCell::new(platform)),
            Err(error) => {
                self.on_event(id, SpeechEvent::Error(error), cx);
                return;
            }
        };
        let events = cx.spawn({
            let platform = platform.clone();
            async move |this, cx| {
                futures_lite::future::or(
                    async {
                        while let Ok(event) = receiver.recv().await {
                            let active = this.update(cx, |this, cx| {
                                this.on_event(id, event, cx);
                                this.dictation.is_session(id)
                            });
                            if !matches!(active, Ok(true)) {
                                break;
                            }
                        }
                    },
                    async {
                        std::future::poll_fn(|cx| platform.borrow_mut().poll(cx)).await;
                        std::future::pending().await
                    },
                )
                .await;
            }
        });
        self.session = Some(Session {
            platform,
            id,
            _events: events,
            _timeout: None,
        });
        cx.emit(DictationEvent::Started);
        cx.notify();
    }

    pub(super) fn stop(&mut self, cx: &mut Context<Self>) {
        if !self.dictation.stop() {
            return;
        }
        if let Some(session) = &mut self.session {
            session.platform.borrow_mut().finish();
            let id = session.id;
            let timeout = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                _ = this.update(cx, |this, cx| this.on_event(id, SpeechEvent::Finished, cx));
            });
            self.session.as_mut().unwrap()._timeout = Some(timeout);
        }
        cx.notify();
    }

    pub(super) fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(event) = self.dictation.cancel() {
            self.session = None;
            cx.emit(event);
            cx.notify();
        }
    }

    fn on_event(&mut self, id: u64, event: SpeechEvent, cx: &mut Context<Self>) {
        if let Some(event) = self.dictation.apply(id, event) {
            cx.emit(event);
        }
        if !self.dictation.status().is_active() {
            self.session = None;
        }
        cx.notify();
    }
}

pub(super) fn configure(mut state: SpeechState, locale: &'static str, cx: &App) -> SpeechState {
    state.locale = locale;
    let recognizer = cx
        .try_global::<crate::remote::ClientAttachment>()
        .and_then(|attachment| attachment.host().speech_recognizer());
    if let Some(recognizer) = recognizer {
        return state.recognizer(recognizer);
    }
    state.recognizer = None;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if state.recognizer.is_none() {
        state = state.recognizer(Rc::new(tcode_client::speech::SystemRecognizer::new(locale)));
    }
    state
}
