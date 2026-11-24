//! System dictation and replacement of the session's whole transcript.

use super::super::*;
use client::{DictationEvent as SpeechEvent, SpeechState, SpeechStatus};
use gpui::SharedString;

mod client;

pub(in super::super) struct Voice {
    state: Entity<SpeechState>,
    locale: &'static str,
    insertion: Option<Insertion>,
    /// A send that arrived mid-dictation, with its steer flag. Platforms
    /// recognize behind the audio, so sending at once would cut off speech
    /// that has been spoken but not yet transcribed.
    pending_submit: Option<bool>,
}

struct Insertion {
    anchor: usize,
    last_len: usize,
    expected: String,
}

fn speech_locale() -> &'static str {
    if rust_i18n::locale().starts_with("zh") {
        "zh-CN"
    } else {
        "en-US"
    }
}

fn speech_state(cx: &mut Context<SpeechState>) -> SpeechState {
    client::configure(SpeechState::new(cx), speech_locale(), cx)
}

impl Voice {
    pub(in super::super) fn new(
        window: &mut Window,
        cx: &mut Context<Composer>,
        subscriptions: &mut Vec<Subscription>,
    ) -> Self {
        let state = cx.new(speech_state);
        subscriptions.push(
            cx.subscribe_in(&state, window, |this, _, event, window, cx| {
                this.on_dictation_event(event, window, cx);
            }),
        );
        subscriptions.push(cx.observe(&state, |_, _, cx| cx.notify()));
        subscriptions.push(cx.on_release(|this, cx| {
            this.voice.insertion = None;
            this.voice.state.update(cx, |state, cx| state.cancel(cx));
        }));
        Self {
            state,
            locale: speech_locale(),
            insertion: None,
            pending_submit: None,
        }
    }
}

impl Composer {
    pub(in super::super) fn render_mic_button(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let locale = speech_locale();
        if locale != self.voice.locale {
            self.abort_dictation(cx);
            self.voice.state.update(cx, |state, cx| {
                // Preserve session ids so a previous sink cannot address a new session.
                let previous = std::mem::replace(state, SpeechState::new(cx));
                *state = client::configure(previous, locale, cx);
            });
            self.voice.locale = locale;
        }
        let state = self.voice.state.read(cx);
        if !state.has_recognizer() {
            return None;
        }
        let status = state.status();
        let preparing = matches!(status, SpeechStatus::Connecting | SpeechStatus::Stopping);
        let available = state.is_available(cx);
        let tooltip: SharedString = if !available {
            crate::tr!("composer.voice_unavailable")
        } else if preparing {
            crate::tr!("composer.voice_preparing")
        } else if status.is_active() {
            crate::tr!("composer.voice_stop")
        } else {
            crate::tr!("composer.voice_start")
        }
        .into_owned()
        .into();
        let color = if status == SpeechStatus::Recording {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };
        Some(
            Button::new("voice-mic")
                .ghost()
                .chip()
                .disabled(!available)
                .tooltip(tooltip)
                .child(if preparing {
                    Spinner::new().color(color).into_any_element()
                } else {
                    let icon = Icon::empty().path("icons/mic.svg").text_color(color);
                    if status == SpeechStatus::Recording {
                        div()
                            .rounded_full()
                            .p(cx.theme().tokens.spacing.xxs)
                            .bg(cx.theme().danger.opacity(0.08 + state.level() * 0.42))
                            .child(icon)
                            .into_any_element()
                    } else {
                        icon.into_any_element()
                    }
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.voice.state.update(cx, |state, cx| state.toggle(cx));
                }))
                .into_any_element(),
        )
    }

    pub(in super::super) fn stop_dictation(&mut self, cx: &mut Context<Self>) -> bool {
        let active = self.voice.state.read(cx).status().is_active();
        if active {
            self.voice.state.update(cx, |state, cx| state.stop(cx));
        }
        active
    }

    /// Stops an active dictation and defers the send until its final
    /// transcript is in the draft.
    pub(in super::super) fn submit_after_dictation(
        &mut self,
        steer: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.voice.state.read(cx).status().is_active() {
            return false;
        }
        self.voice.pending_submit = Some(steer);
        self.voice.state.update(cx, |state, cx| state.stop(cx));
        true
    }

    pub(in super::super) fn abort_dictation(&mut self, cx: &mut Context<Self>) {
        self.voice.insertion = None;
        self.voice.pending_submit = None;
        self.voice.state.update(cx, |state, cx| state.cancel(cx));
    }

    pub(in super::super) fn stop_dictation_on_user_edit(&mut self, cx: &mut Context<Self>) {
        if self.voice.state.read(cx).status().is_active()
            && self
                .voice
                .insertion
                .as_ref()
                .is_none_or(|insertion| insertion.expected != self.input.read(cx).value().as_ref())
        {
            self.abort_dictation(cx);
        }
    }

    fn on_dictation_event(
        &mut self,
        event: &SpeechEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SpeechEvent::Started => {
                // A cancelled session can have a Started event already queued.
                if self.voice.state.read(cx).status().is_active() {
                    self.focus(window, cx);
                    let input = self.input.read(cx);
                    let (anchor, expected) = (input.cursor(), input.value().to_string());
                    self.voice.insertion = Some(Insertion {
                        anchor,
                        last_len: 0,
                        expected,
                    });
                }
            }
            SpeechEvent::Partial(text) | SpeechEvent::Final(text) => {
                self.insert_transcript(text.clone().into(), window, cx);
                if matches!(event, SpeechEvent::Final(_)) {
                    self.voice.insertion = None;
                    if let Some(steer) = self.voice.pending_submit.take() {
                        let input = self.input.clone();
                        self.submit(&input, steer, window, cx);
                    }
                }
            }
            SpeechEvent::Error(error) => {
                self.voice.insertion = None;
                self.voice.pending_submit = None;
                let notification = Notification::error(
                    crate::tr!("composer.voice_error", error = error).into_owned(),
                );
                #[cfg(target_os = "macos")]
                let notification = match error {
                    tcode_client::speech::SpeechError::Recognizer(error) if error.ends_with("(kLSRErrorDomain 201)") =>
                    {
                        Notification::error(crate::tr!("composer.voice_enable_dictation"))
                            .id1::<Voice>("dictation-disabled")
                            .autohide(false)
                            .action(|_, _, _| {
                                Button::new("open-dictation-settings")
                                    .outline()
                                    .label(crate::tr!("composer.voice_open_dictation_settings"))
                                    .on_click(|_, _, cx| {
                                        cx.open_url(
                                            "x-apple.systempreferences:com.apple.preference.keyboard?Dictation",
                                        );
                                    })
                            })
                    }
                    _ => notification,
                };
                window.push_notification(notification, cx);
            }
            SpeechEvent::Cancelled => {
                self.voice.insertion = None;
                self.voice.pending_submit = None;
            }
        }
        cx.notify();
    }

    fn insert_transcript(
        &mut self,
        text: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(insertion) = self.voice.insertion.as_mut() else {
            return;
        };
        if self.input.read(cx).value().as_ref() != insertion.expected {
            self.abort_dictation(cx);
            return;
        }
        let range = insertion.anchor..insertion.anchor + insertion.last_len;
        self.input.update(cx, |input, cx| {
            input.set_selected_range(range, cx);
            input.replace(text.clone(), window, cx);
        });
        insertion.last_len = text.len();
        // Change events run after the update; our writes must match before delivery.
        insertion.expected = self.input.read(cx).value().to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, size};
    use std::cell::RefCell;
    use tcode_client::speech::{SpeechEvent as PlatformEvent, SpeechRecognizer, SpeechSession};

    struct Recognizer(Rc<RefCell<Option<async_channel::Sender<PlatformEvent>>>>);
    struct Recognition;

    impl SpeechRecognizer for Recognizer {
        fn is_available(&self, _: &str) -> bool {
            true
        }
        fn start(
            &self,
            _: &str,
            events: async_channel::Sender<PlatformEvent>,
        ) -> Result<Box<dyn SpeechSession>, tcode_client::speech::SpeechError> {
            events.try_send(PlatformEvent::Ready).unwrap();
            *self.0.borrow_mut() = Some(events);
            Ok(Box::new(Recognition))
        }
    }

    impl SpeechSession for Recognition {
        fn finish(&mut self) {}
    }

    #[gpui::test]
    fn dictation_revises_the_draft_and_a_send_waits_for_its_final_transcript(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(crate::store::tests::scratch_root(
                "dictation-test",
            ))
            .unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let (session_id, timeline) = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("dictation".into(), std::env::temp_dir(), cx);
            let timeline = state.residents.live[&id].timeline.clone();
            (id, timeline)
        }))
        .unwrap();
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, _| store.select_session(session_id.clone()));
        driver.wait_until(cx, &store, "selected conversation", |cx| {
            store.read_with(cx, |store, _| {
                store.baseline_ready() && !store.chat_loading()
            })
        });
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        let sink = Rc::new(RefCell::new(None::<async_channel::Sender<PlatformEvent>>));
        let (composer, cx) = cx.add_window_view(|window, cx| {
            let composer = Composer::new(store.clone(), window, cx);
            composer.voice.state.update(cx, |state, cx| {
                *state = SpeechState::new(cx).recognizer(Rc::new(Recognizer(sink.clone())));
            });
            composer
        });
        cx.simulate_resize(size(px(800.), px(600.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        composer.update_in(cx, |composer, window, cx| {
            composer.set_draft("前后", window, cx);
            composer.input.update(cx, |input, cx| {
                input.set_selected_range(3..3, cx);
            });
        });
        composer.update_in(cx, |composer, _, cx| {
            composer
                .voice
                .state
                .update(cx, |state, cx| state.toggle(cx));
        });
        let first = sink.borrow().clone().unwrap();
        for (text, expected) in [
            ("hello brave world", "前hello brave world后"),
            ("hi", "前hi后"),
            ("", "前后"),
            ("你好", "前你好后"),
        ] {
            first
                .try_send(PlatformEvent::Hypothesis(text.into()))
                .unwrap();
            cx.run_until_parked();
            assert_eq!(
                composer.read_with(cx, |composer, cx| composer.draft(cx)),
                expected
            );
        }
        first.try_send(PlatformEvent::Finished).unwrap();
        cx.run_until_parked();
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "前你好后"
        );

        composer.update_in(cx, |composer, _, cx| {
            composer
                .voice
                .state
                .update(cx, |state, cx| state.toggle(cx));
        });
        let edited = sink.borrow().clone().unwrap();
        edited
            .try_send(PlatformEvent::Hypothesis(" text".into()))
            .unwrap();
        cx.run_until_parked();
        cx.simulate_input("!");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.voice.state.read(cx).status()),
            SpeechStatus::Idle,
        );
        let draft = composer.read_with(cx, |composer, cx| composer.draft(cx));
        assert_eq!(draft, "前你好 text!后");
        _ = edited.try_send(PlatformEvent::Hypothesis("late overwrite".into()));
        _ = edited.try_send(PlatformEvent::Finished);
        cx.run_until_parked();
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            draft
        );

        composer.update_in(cx, |composer, window, cx| {
            composer.set_draft("", window, cx);
        });
        composer.update_in(cx, |composer, _, cx| {
            composer
                .voice
                .state
                .update(cx, |state, cx| state.toggle(cx));
        });
        let submitted = sink.borrow().clone().unwrap();
        submitted
            .try_send(PlatformEvent::Hypothesis("/mod".into()))
            .unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.simulate_keystrokes("escape");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.voice.state.read(cx).status()),
            SpeechStatus::Stopping,
        );
        let picker = composer.read_with(cx, |composer, _| composer.model_picker_token);
        composer.update_in(cx, |composer, window, cx| {
            let input = composer.input.clone();
            composer.submit(&input, false, window, cx);
        });
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            "/mod"
        );
        submitted
            .try_send(PlatformEvent::Phrase("/model".into()))
            .unwrap();
        submitted.try_send(PlatformEvent::Finished).unwrap();
        cx.run_until_parked();
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            ""
        );
        assert_ne!(
            composer.read_with(cx, |composer, _| composer.model_picker_token),
            picker,
            "the send must carry the final transcript"
        );
        _ = submitted.try_send(PlatformEvent::Hypothesis("late text".into()));
        cx.run_until_parked();
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.draft(cx)),
            ""
        );

        cx.update(|window, cx| {
            crate::window_seam::override_mobile_for_test(cx, true);
            window.blur(cx);
        });
        composer.update_in(cx, |composer, _, cx| {
            composer
                .voice
                .state
                .update(cx, |state, cx| state.toggle(cx));
        });
        let touch = sink.borrow().clone().unwrap();
        touch
            .try_send(PlatformEvent::Hypothesis("no keyboard".into()))
            .unwrap();
        cx.run_until_parked();
        composer.update_in(cx, |composer, window, cx| {
            assert!(
                !composer.input.read(cx).focus_handle(cx).is_focused(window),
                "dictation on a touch layout must not raise the software keyboard"
            );
            assert_eq!(composer.draft(cx), "no keyboard");
        });
        driver.shutdown(cx, &store, &host);
    }
}
