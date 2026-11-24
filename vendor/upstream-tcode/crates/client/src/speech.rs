//! Client-local speech capture. Transcripts never pass through the remote host.

#[derive(Debug, Clone, PartialEq)]
pub enum SpeechEvent {
    Ready,
    Level(f32),
    Hypothesis(String),
    Phrase(String),
    Finished,
    PermissionDenied,
    Unsupported,
    Error(SpeechError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechError {
    PermissionDenied,
    NoInputDevice,
    Unsupported,
    Input(String),
    Recognizer(String),
}

impl std::fmt::Display for SpeechError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionDenied => f.write_str("microphone access was denied"),
            Self::NoInputDevice => f.write_str("no audio input device is available"),
            Self::Unsupported => f.write_str("speech input is not supported"),
            Self::Input(error) => write!(f, "audio input failed: {error}"),
            Self::Recognizer(error) => write!(f, "speech recognition failed: {error}"),
        }
    }
}
impl std::error::Error for SpeechError {}
impl From<String> for SpeechError {
    fn from(error: String) -> Self {
        Self::Recognizer(error)
    }
}
impl From<&str> for SpeechError {
    fn from(error: &str) -> Self {
        Self::Recognizer(error.into())
    }
}

/// The platform owns microphone capture, permissions and recognition resources.
/// Dropping a session cancels it; finish flushes its last result before Finished.
pub trait SpeechSession {
    fn finish(&mut self);

    /// Drive platform callbacks on the client thread. Callback-only platforms
    /// can leave this pending; dropping the session cancels this work too.
    fn poll(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        std::task::Poll::Pending
    }
}

pub trait SpeechRecognizer {
    fn is_available(&self, locale: &str) -> bool;
    fn start(
        &self,
        locale: &str,
        events: async_channel::Sender<SpeechEvent>,
    ) -> Result<Box<dyn SpeechSession>, SpeechError>;
}

#[cfg(all(feature = "voice", any(target_os = "macos", target_os = "windows")))]
mod level;
#[cfg(all(feature = "voice", target_os = "macos"))]
mod macos;
#[cfg(all(feature = "voice", target_os = "windows"))]
mod windows;
#[cfg(all(feature = "voice", target_os = "macos"))]
pub use macos::SystemRecognizer;
#[cfg(all(feature = "voice", target_os = "windows"))]
pub use windows::SystemRecognizer;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SpeechStatus {
    #[default]
    Idle,
    Connecting,
    Recording,
    Stopping,
}

impl SpeechStatus {
    pub fn is_active(self) -> bool {
        self != Self::Idle
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DictationEvent {
    Started,
    Partial(String),
    Final(String),
    Cancelled,
    Error(SpeechError),
}

/// One client-local transcript. Each session has a distinct identity so queued
/// platform callbacks cannot write into a replacement session.
#[derive(Default)]
pub struct Dictation {
    next_session: u64,
    session: Option<u64>,
    status: SpeechStatus,
    committed: String,
    hypothesis: String,
    level: f32,
}

impl Dictation {
    pub fn start(&mut self) -> u64 {
        self.next_session += 1;
        self.session = Some(self.next_session);
        self.status = SpeechStatus::Connecting;
        self.committed.clear();
        self.hypothesis.clear();
        self.level = 0.;
        self.next_session
    }

    pub fn status(&self) -> SpeechStatus {
        self.status
    }
    pub fn level(&self) -> f32 {
        self.level
    }
    pub fn is_session(&self, id: u64) -> bool {
        self.session == Some(id)
    }
    pub fn transcript(&self) -> String {
        format!("{}{}", self.committed, self.hypothesis)
    }

    pub fn stop(&mut self) -> bool {
        if matches!(
            self.status,
            SpeechStatus::Connecting | SpeechStatus::Recording
        ) {
            self.status = SpeechStatus::Stopping;
            true
        } else {
            false
        }
    }

    pub fn cancel(&mut self) -> Option<DictationEvent> {
        self.session?;
        self.committed.clear();
        self.hypothesis.clear();
        self.end();
        Some(DictationEvent::Cancelled)
    }

    pub fn apply(&mut self, id: u64, event: SpeechEvent) -> Option<DictationEvent> {
        if !self.is_session(id) {
            return None;
        }
        let event = match event {
            SpeechEvent::Ready => {
                if self.status == SpeechStatus::Connecting {
                    self.status = SpeechStatus::Recording;
                }
                return None;
            }
            SpeechEvent::Level(level) => {
                self.level = level;
                return None;
            }
            SpeechEvent::Hypothesis(text) => {
                self.hypothesis = text;
                DictationEvent::Partial(self.transcript())
            }
            SpeechEvent::Phrase(text) => {
                self.committed.push_str(&text);
                self.hypothesis.clear();
                DictationEvent::Partial(self.transcript())
            }
            SpeechEvent::Finished => {
                self.end();
                DictationEvent::Final(self.transcript())
            }
            SpeechEvent::PermissionDenied => {
                self.end();
                DictationEvent::Error(SpeechError::PermissionDenied)
            }
            SpeechEvent::Unsupported => {
                self.end();
                DictationEvent::Error(SpeechError::Unsupported)
            }
            SpeechEvent::Error(error) => {
                self.end();
                DictationEvent::Error(error)
            }
        };
        Some(event)
    }

    fn end(&mut self) {
        self.session = None;
        self.status = SpeechStatus::Idle;
        self.level = 0.;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_revisions_phrases_and_terminal_events_are_session_local() {
        let mut state = Dictation::default();
        let id = state.start();
        state.apply(id, SpeechEvent::Ready);
        assert_eq!(state.status(), SpeechStatus::Recording);
        for (event, text) in [
            (SpeechEvent::Hypothesis("hello word".into()), "hello word"),
            (SpeechEvent::Hypothesis("hello world".into()), "hello world"),
            (SpeechEvent::Phrase("Hello world.".into()), "Hello world."),
            (SpeechEvent::Hypothesis(" How".into()), "Hello world. How"),
            (
                SpeechEvent::Phrase(" How are you?".into()),
                "Hello world. How are you?",
            ),
        ] {
            assert_eq!(
                state.apply(id, event),
                Some(DictationEvent::Partial(text.into()))
            );
        }
        assert!(state.stop());
        state.apply(id, SpeechEvent::Ready);
        assert_eq!(state.status(), SpeechStatus::Stopping);
        assert_eq!(
            state.apply(id, SpeechEvent::Finished),
            Some(DictationEvent::Final("Hello world. How are you?".into()))
        );
        assert_eq!(state.status(), SpeechStatus::Idle);
        assert_eq!(
            state.apply(id, SpeechEvent::Hypothesis("late".into())),
            None
        );

        let cancelled = state.start();
        state.apply(cancelled, SpeechEvent::Hypothesis("discard".into()));
        assert_eq!(state.cancel(), Some(DictationEvent::Cancelled));
        assert_eq!(state.transcript(), "");
        for terminal in [
            SpeechEvent::PermissionDenied,
            SpeechEvent::Unsupported,
            SpeechEvent::Error("offline".into()),
        ] {
            let current = state.start();
            assert_eq!(
                state.apply(cancelled, SpeechEvent::Phrase("late".into())),
                None
            );
            assert!(matches!(
                state.apply(current, terminal),
                Some(DictationEvent::Error(_))
            ));
            assert_eq!(state.status(), SpeechStatus::Idle);
            assert_eq!(state.apply(current, SpeechEvent::Finished), None);
        }
    }
}
