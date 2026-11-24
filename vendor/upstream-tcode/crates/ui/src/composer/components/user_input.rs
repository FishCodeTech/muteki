use super::super::*;
use crate::scroll::ScrollableElement as _;
use tcode_core::session::PendingUserInput;

impl Composer {
    /// The active session's pending user-input request, if any.
    pub(in super::super) fn pending_user_input(&self, cx: &App) -> Option<PendingUserInput> {
        self.workspace_store
            .read(cx)
            .composer_state()
            .pending_user_input
    }

    /// Keep the per-request question state in sync: reset the index/selections
    /// when a new request arrives (or the pending one resolves).
    pub(in super::super) fn sync_user_input_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .workspace_store
            .read(cx)
            .composer_state()
            .pending_user_input;
        let current_id = current.as_ref().map(|pending| pending.request_id.clone());
        if current_id != self.ui_request_id {
            self.ui_request_id = current_id;
            self.ui_question_index = 0;
            self.ui_selections.clear();
            self.ui_dismissed_request_id = None;
            self.ui_expanded = current
                .as_ref()
                .is_some_and(|pending| pending.delivery.is_blocking());
            let prefill = current
                .as_ref()
                .and_then(|pending| pending.questions.first())
                .and_then(|question| question.prefill.as_deref())
                .unwrap_or_default();
            self.user_input_custom.update(cx, |state, cx| {
                state.set_value(prefill, window, cx);
            });
        }
    }

    /// One card for every question, whichever way it was delivered. A strip
    /// names the question and stays one line tall; the answer area below it
    /// collapses so the conversation behind it can be read while deciding,
    /// and scrolls inside a fraction of the window so the composer never
    /// leaves the screen however long the question runs. A blocking question
    /// arrives open, because the agent is waiting; a non-blocking one arrives
    /// closed and can be hidden, because the agent keeps working.
    pub(in super::super) fn render_user_input_panel(
        &self,
        pending: &PendingUserInput,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let request_id = pending.request_id.clone();
        let blocking = pending.delivery.is_blocking();
        // Hiding is local to this client; the question stays open on the host,
        // so a way back remains while the sidebar still asks for an answer.
        if !blocking && self.ui_dismissed_request_id.as_ref() == Some(&request_id) {
            return h_flex()
                .w_full()
                .child(
                    Button::new("ui-reveal")
                        .debug_selector(|| "ui-reveal".into())
                        .ghost()
                        .xsmall()
                        .touch(self.compact)
                        .icon(IconName::Info)
                        .label(crate::tr!("userinput.async_header"))
                        .tooltip(crate::tr!("userinput.answer"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ui_dismissed_request_id = None;
                            this.ui_expanded = true;
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        let questions = pending.questions.clone();
        let total = questions.len();
        let index = self.ui_question_index.min(total.saturating_sub(1));
        let Some(question) = questions.get(index).cloned() else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let primary = cx.theme().primary;
        let expanded = self.ui_expanded;
        let multi = question.multi_select;

        let toggle = Button::new("ui-toggle")
            .debug_selector(|| "ui-toggle".into())
            .ghost()
            .xsmall()
            .touch(self.compact)
            .icon(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronUp
            })
            .tooltip(if expanded {
                crate::tr!("userinput.collapse")
            } else {
                crate::tr!("userinput.answer")
            })
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.ui_expanded = !this.ui_expanded;
                cx.notify();
            }));
        let title = if blocking && !question.header.trim().is_empty() {
            question.header.clone()
        } else {
            crate::tr!("userinput.async_header").into_owned()
        };
        let mut strip = crate::material::accessible_clickable(
            h_flex(),
            "ui-strip",
            Role::Button,
            title.clone(),
            cx,
        )
        .w_full()
        .gap_2()
        .items_center()
        .cursor_pointer()
        .child(Icon::new(IconName::Info).text_color(primary))
        .child(
            div()
                .flex_none()
                .max_w_1_2()
                .overflow_hidden()
                .text_title(cx)
                .child(crate::widgets::fade_text(title)),
        )
        .when(!expanded, |strip| {
            strip.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_body(self.compact, cx)
                    .text_color(muted)
                    .child(crate::widgets::fade_text(question.question.clone())),
            )
        })
        .when(expanded, |strip| strip.child(div().flex_1()))
        .when(total > 1, |strip| {
            let questions_previous = questions.clone();
            let questions_next = questions.clone();
            strip
                .child(
                    Button::new("ui-prev")
                        .debug_selector(|| "ui-prev".into())
                        .ghost()
                        .xsmall()
                        .touch(self.compact)
                        .icon(IconName::ChevronLeft)
                        .disabled(index == 0)
                        .tooltip(crate::tr!("userinput.previous"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.ui_expanded = true;
                            this.ui_go(-1, &questions_previous, window, cx);
                        })),
                )
                .child(
                    div()
                        .flex_none()
                        .text_secondary(self.compact, cx)
                        .child(crate::tr!(
                            "userinput.question_count",
                            index = index + 1,
                            total = total
                        )),
                )
                .child(
                    Button::new("ui-next")
                        .debug_selector(|| "ui-next".into())
                        .ghost()
                        .xsmall()
                        .touch(self.compact)
                        .icon(IconName::ChevronRight)
                        .disabled(index + 1 >= total)
                        .tooltip(crate::tr!("userinput.next_question"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.ui_expanded = true;
                            this.ui_go(1, &questions_next, window, cx);
                        })),
                )
        })
        .child(toggle);
        if !blocking {
            let request_dismiss = request_id.clone();
            strip = strip.child(
                Button::new("ui-dismiss")
                    .debug_selector(|| "ui-dismiss".into())
                    .ghost()
                    .xsmall()
                    .touch(self.compact)
                    .icon(IconName::Close)
                    .tooltip(crate::tr!("userinput.dismiss"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.ui_dismissed_request_id = Some(request_dismiss.clone());
                        cx.notify();
                    })),
            );
        }
        let strip = strip.on_click(cx.listener(|this, _, _, cx| {
            this.ui_expanded = !this.ui_expanded;
            cx.notify();
        }));

        let card = crate::material::card_raised(cx).w_full().child(strip);
        if !expanded {
            return card.into_any_element();
        }

        let selected = self
            .ui_selections
            .get(&question.id)
            .cloned()
            .unwrap_or_default();
        let mut options = v_flex()
            .id("ui-options")
            .role(Role::ListBox)
            .aria_label(question.question.clone())
            .w_full()
            .gap_1();
        for (opt_index, option) in question.options.iter().enumerate() {
            let is_selected = selected.iter().any(|l| l == &option.label);
            let label = option.label.clone();
            let question_for_click = question.clone();
            let questions_for_click = questions.clone();
            let request_for_click = request_id.clone();
            let mark = div()
                .size(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(if multi {
                    px(5.)
                } else {
                    crate::material::radius_input(cx)
                })
                .border_1()
                .border_color(if is_selected { primary } else { muted })
                .when(is_selected, |mark| {
                    mark.bg(primary)
                        .child(div().size(px(6.)).rounded_full().bg(gpui::white()))
                });
            options = options.child(
                crate::material::menu_item_frame(self.compact, cx)
                    .id(("ui-opt", opt_index))
                    .role(Role::ListBoxOption)
                    .aria_label(option.label.clone())
                    .aria_selected(is_selected)
                    .flex_none()
                    .w_full()
                    .items_start()
                    .text_body(self.compact, cx)
                    .cursor_pointer()
                    .when(is_selected, |s| s.bg(cx.theme().list_active))
                    .hover(|s| s.bg(cx.theme().muted))
                    .child(
                        div()
                            .flex_none()
                            .pt(cx.theme().tokens.spacing.xxs)
                            .child(mark),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    // Digits select only while the agent is
                                    // blocked; the composer keeps them otherwise.
                                    .when(blocking, |row| {
                                        row.child(
                                            div()
                                                .flex_none()
                                                .text_color(muted)
                                                .child(format!("{}", opt_index + 1)),
                                        )
                                    })
                                    .child(div().child(option.label.clone())),
                            )
                            .when(!option.description.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_secondary(self.compact, cx)
                                        .child(option.description.clone()),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.ui_toggle_option(&question_for_click, label.clone(), cx);
                        // Single-select answers flow onward by themselves: next
                        // unanswered question, or submission when none remain.
                        if !multi {
                            this.ui_advance_or_submit(
                                &questions_for_click,
                                request_for_click.clone(),
                                window,
                                cx,
                            );
                        }
                    })),
            );
        }
        // The question and its options scroll together within a share of the
        // window, leaving the rest to the conversation and the composer.
        let body_cap = px((f32::from(window.viewport_size().height) * 0.4).clamp(120., 360.));
        let body = div()
            .id("user-input-body-scroll")
            .w_full()
            .max_h(body_cap)
            .overflow_y_scroll_area()
            .child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .text_body(self.compact, cx)
                            .child(question.question.clone()),
                    )
                    .when(!question.options.is_empty(), |this| this.child(options)),
            );

        let custom_input = self.user_input_custom.clone();
        let custom_has_text = !custom_input.read(cx).value().trim().is_empty();
        let custom_answer = h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .pl_2()
            .rounded(crate::material::radius_input(cx))
            .border_1()
            .border_color(cx.theme().input)
            .bg(cx.theme().background)
            .child(
                div().flex_1().min_w_0().child(
                    Textarea::new(&self.user_input_custom)
                        .placeholder(crate::tr!("userinput.custom_placeholder"))
                        .appearance(false),
                ),
            )
            .child(
                Button::new("ui-custom-submit")
                    .ghost()
                    .xsmall()
                    .touch(self.compact)
                    .icon(IconName::ArrowUp)
                    .disabled(!custom_has_text)
                    .tooltip(crate::tr!("userinput.submit_custom"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.submit_custom_user_input(&custom_input, window, cx);
                    })),
            );

        // Answers submit themselves: a single-select click that completes the
        // set submits it, so the only finishing affordance is Done on a
        // multi-select question, where clicks cannot signal "I'm finished".
        let all_answered = user_input_all_answered(&questions, &self.ui_selections);
        let questions_submit = questions.clone();
        let hint = if multi {
            Some(crate::tr!("userinput.multi_hint"))
        } else if !blocking {
            Some(crate::tr!("userinput.async_hint"))
        } else {
            None
        };
        let footer = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_secondary(self.compact, cx)
                    .children(hint),
            )
            .when(multi && all_answered, |this| {
                this.child(
                    Button::new("ui-done")
                        .primary()
                        .chip()
                        .touch(self.compact)
                        .label(crate::tr!("userinput.done"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.ui_submit(&questions_submit, request_id.clone(), window, cx);
                        })),
                )
            });

        card.child(body)
            .child(custom_answer)
            .child(footer)
            .into_any_element()
    }

    /// Number keys 1-9 pressed in the (empty) main composer input select the
    /// matching option of the pending blocking question. Returns true when
    /// consumed. Deliberately NOT wired to the panel itself: the only focusable
    /// child there is the custom-answer textarea, where digits must stay
    /// literal. While a non-blocking question is open the composer still
    /// writes ordinary messages, so digits stay literal there too.
    pub(in super::super) fn handle_user_input_digit(
        &mut self,
        ev: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if ev.keystroke.modifiers.modified() || !self.input.read(cx).value().is_empty() {
            return false;
        }
        let Some(PendingUserInput {
            request_id,
            questions,
            delivery,
        }) = self.pending_user_input(cx)
        else {
            return false;
        };
        if !delivery.is_blocking() {
            return false;
        }
        let index = self
            .ui_question_index
            .min(questions.len().saturating_sub(1));
        let Some(question) = questions.get(index).cloned() else {
            return false;
        };
        let Ok(n) = ev.keystroke.key.parse::<usize>() else {
            return false;
        };
        if n < 1 || n > question.options.len() {
            return false;
        }
        let label = question.options[n - 1].label.clone();
        self.ui_toggle_option(&question, label, cx);
        if !question.multi_select {
            self.ui_advance_or_submit(&questions, request_id, window, cx);
        }
        true
    }

    /// Toggle an option label for a question: single-select replaces, multi
    /// toggles membership.
    pub(in super::super) fn ui_toggle_option(
        &mut self,
        question: &UserInputQuestion,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let entry = self.ui_selections.entry(question.id.clone()).or_default();
        if question.multi_select {
            if let Some(pos) = entry.iter().position(|l| l == &label) {
                entry.remove(pos);
            } else {
                entry.push(label);
            }
        } else {
            *entry = vec![label];
        }
        cx.notify();
    }

    pub(in super::super) fn submit_custom_user_input(
        &mut self,
        input: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(PendingUserInput {
            request_id,
            questions,
            ..
        }) = self.pending_user_input(cx)
        else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let index = self
            .ui_question_index
            .min(questions.len().saturating_sub(1));
        if let Some(question) = questions.get(index) {
            let entry = self.ui_selections.entry(question.id.clone()).or_default();
            if question.multi_select {
                entry.push(text);
            } else {
                *entry = vec![text];
            }
        }
        input.update(cx, |state, cx| state.set_value("", window, cx));
        self.ui_advance_or_submit(&questions, request_id, window, cx);
        cx.notify();
    }

    pub(in super::super) fn ui_go(
        &mut self,
        delta: i32,
        questions: &[UserInputQuestion],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = self.ui_question_index as i32 + delta;
        if next >= 0 {
            self.ui_question_index = next as usize;
            self.seed_user_input_prefill(questions, window, cx);
            cx.notify();
        }
    }

    pub(in super::super) fn seed_user_input_prefill(
        &mut self,
        questions: &[UserInputQuestion],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefill = questions
            .get(self.ui_question_index)
            .and_then(|question| question.prefill.as_deref())
            .unwrap_or_default();
        self.user_input_custom
            .update(cx, |state, cx| state.set_value(prefill, window, cx));
    }

    /// Advance to the next unanswered question or submit a completed answer
    /// set. The brief pause lets the selection mark register before moving on.
    pub(in super::super) fn ui_advance_or_submit(
        &mut self,
        questions: &[UserInputQuestion],
        request_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let questions = questions.to_vec();
        let at = self.ui_question_index;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(200))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                // A newer request or manual navigation invalidates this hop.
                if this.ui_question_index != at
                    || this.ui_request_id.as_deref() != Some(&request_id)
                {
                    return;
                }
                if user_input_all_answered(&questions, &this.ui_selections) {
                    this.ui_submit(&questions, request_id, window, cx);
                } else if let Some(next) =
                    next_unanswered_question(&questions, &this.ui_selections, at)
                {
                    this.ui_question_index = next;
                    this.seed_user_input_prefill(&questions, window, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(in super::super) fn ui_submit(
        &mut self,
        questions: &[UserInputQuestion],
        request_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let custom = self.user_input_custom.read(cx).value().trim().to_string();
        let custom = if custom.is_empty() {
            None
        } else {
            Some(custom.as_str())
        };
        let answers = assemble_user_input_answers(
            questions,
            &self.ui_selections,
            self.ui_question_index,
            custom,
        );
        self.user_input_custom
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.ui_selections.clear();
        self.ui_question_index = 0;
        self.workspace_store.update(cx, |store, _cx| {
            store.respond_user_input(request_id, answers)
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, size};

    fn question(id: &str) -> UserInputQuestion {
        UserInputQuestion {
            id: id.into(),
            header: id.into(),
            question: format!("{id}?"),
            options: vec![agent::UserInputOption {
                label: "Yes".into(),
                description: String::new(),
            }],
            multi_select: false,
            prefill: None,
        }
    }

    /// A composer over a session whose agent asked `first` and `second` with
    /// `delivery`, plus a click driver returning the card state afterwards:
    /// (expanded, question index, toggle present, dismiss present).
    fn question_card(
        cx: &mut TestAppContext,
        delivery: agent::UserInputDelivery,
    ) -> impl FnMut(&'static str) -> ((bool, usize), bool, bool) {
        cx.update(crate::theme::init);
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(crate::store::tests::scratch_root(
                "question-test",
            ))
            .unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let (session_id, timeline) =
            smol::block_on(host.update_state_for_test(move |state, cx| {
                let id = state.start_draft("question".into(), std::env::temp_dir(), cx);
                for event in [
                    agent::AgentEvent::TurnStarted {
                        turn_id: "turn".into(),
                    },
                    agent::AgentEvent::UserInputRequested {
                        request_id: "ask".into(),
                        questions: vec![question("first"), question("second")],
                        delivery,
                    },
                ] {
                    state.provider_event_for_test(&id, event, cx);
                }
                (id.clone(), state.residents.live[&id].timeline.clone())
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
        let (composer, cx) =
            cx.add_window_view(|window, cx| Composer::new(store.clone(), window, cx));
        cx.simulate_resize(size(px(800.), px(600.)));
        move |selector: &'static str| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            if !selector.is_empty() {
                let bounds = cx.debug_bounds(selector).expect(selector);
                cx.simulate_click(bounds.center(), Default::default());
                cx.update(|window, cx| window.draw(cx).clear(cx));
            }
            (
                composer.read_with(cx, |composer, _| {
                    (composer.ui_expanded, composer.ui_question_index)
                }),
                cx.debug_bounds("ui-toggle").is_some(),
                cx.debug_bounds("ui-dismiss").is_some(),
            )
        }
    }

    /// The strip's own buttons sit inside a clickable strip; a click on one
    /// must act once, not also reach the strip underneath. A non-blocking
    /// question arrives closed and can be hidden, with a way back.
    #[gpui::test]
    fn async_question_strip_controls(cx: &mut TestAppContext) {
        let mut click = question_card(cx, agent::UserInputDelivery::Async);
        assert_eq!(click(""), ((false, 0), true, true));
        assert_eq!(click("ui-toggle"), ((true, 0), true, true));
        assert_eq!(click("ui-next"), ((true, 1), true, true));
        assert_eq!(click("ui-prev"), ((true, 0), true, true));
        assert_eq!(click("ui-toggle"), ((false, 0), true, true));
        // Hidden, the still-open question keeps an entry that reopens it.
        assert_eq!(click("ui-dismiss"), ((false, 0), false, false));
        assert_eq!(click("ui-reveal"), ((true, 0), true, true));
    }

    /// A blocking question arrives open, folds away so the conversation can
    /// be read while deciding, and cannot be hidden while the agent waits.
    #[gpui::test]
    fn blocking_question_collapses_but_stays(cx: &mut TestAppContext) {
        let mut click = question_card(cx, agent::UserInputDelivery::Blocking);
        assert_eq!(click(""), ((true, 0), true, false));
        assert_eq!(click("ui-toggle"), ((false, 0), true, false));
        assert_eq!(click("ui-next"), ((true, 1), true, false));
    }
}
