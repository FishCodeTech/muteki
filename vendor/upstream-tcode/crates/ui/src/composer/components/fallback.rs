use super::super::*;

use crate::store::{FallbackBlock, FallbackReview};
use agent::{ClassifierCategory, RewindMode};

/// Opus is the classifier's intended fallback target for legitimate work; the
/// flagged category picks which release takes the retry.
fn retry_model(category: Option<&ClassifierCategory>) -> &'static str {
    match category {
        Some(ClassifierCategory::Bio) => "claude-opus-5",
        _ => "claude-opus-4-8",
    }
}

impl Composer {
    /// Recovery for a classifier block or an unexpected model change.
    /// Every action here is a user click — nothing retries or reroutes by itself.
    pub(in super::super) fn render_fallback_panel(
        &self,
        block: &FallbackBlock,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let model_change = block.category.is_none() && block.fallback_model.is_some();
        let title = if model_change {
            crate::tr!("fallback.model_changed_title")
        } else {
            crate::tr!("fallback.title")
        };
        let reason = match block.category.as_ref() {
            Some(ClassifierCategory::Cyber) => crate::tr!("fallback.reason_cyber"),
            Some(ClassifierCategory::Bio) => crate::tr!("fallback.reason_bio"),
            None if model_change => crate::tr!(
                "fallback.reason_model_changed",
                model = block.model.clone().unwrap_or_default()
            ),
            _ => crate::tr!("fallback.reason_generic"),
        };
        let outcome = match block.fallback_model.as_ref() {
            Some(model) => crate::tr!("fallback.rerouted", model = model.clone()),
            None => crate::tr!(
                "fallback.blocked",
                model = block.model.clone().unwrap_or_default()
            ),
        };

        let model = if model_change {
            block.fallback_model.clone().unwrap_or_default()
        } else {
            retry_model(block.category.as_ref()).to_owned()
        };
        // The refused prompt is the last user message of this session; the
        // rewind path additionally needs its turn to have a provider checkpoint.
        let refused = self.workspace_store.read(cx).last_user_message();
        let can_rewind = refused.as_ref().is_some_and(|(turn, _)| {
            self.workspace_store.read(cx).absolute_turn(*turn) > 0
                && self
                    .workspace_store
                    .read(cx)
                    .chat_native_rewind_state(*turn)
                    .is_some_and(|(available, disabled)| available && !disabled)
        });

        let header = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .text_title(cx)
            .child(Icon::new(IconName::Info).text_color(muted))
            .child(div().flex_1().child(title))
            .child(
                Button::new("fallback-dismiss")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .aria_label(crate::tr!("fallback.dismiss"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store
                            .update(cx, |store, _cx| store.dismiss_fallback_block());
                        cx.notify();
                    })),
            );

        let retry_text = refused.as_ref().map(|(_, text)| text.clone());
        let rewind_turn = refused.as_ref().map(|(turn, _)| *turn);
        let actions = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(div().flex_1())
            .when(can_rewind, |this| {
                let turn = rewind_turn.unwrap_or_default();
                this.child(
                    Button::new("fallback-edit-retry")
                        .outline()
                        .chip()
                        .label(crate::tr!("fallback.edit_retry"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.rewind_turn(turn, RewindMode::Conversation);
                                store.dismiss_fallback_block();
                            });
                            cx.notify();
                        })),
                )
            })
            .when_some(retry_text, |this, text| {
                this.child(
                    Button::new("fallback-retry-opus")
                        .primary()
                        .chip()
                        .label(crate::tr!("fallback.retry_on", model = model.clone()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.workspace_store.update(cx, |store, _cx| {
                                store.set_active_model(
                                    ProviderKind::ClaudeCode,
                                    Some(model.clone()),
                                    None,
                                );
                                store.dismiss_fallback_block();
                            });
                            this.set_input_text(text.clone(), window, cx);
                        })),
                )
            });

        crate::material::card_raised(cx)
            .w_full()
            .child(header)
            .child(div().text_body(self.compact, cx).child(reason))
            .child(div().text_secondary(self.compact, cx).child(outcome))
            .when(!block.detail.trim().is_empty(), |this| {
                this.child(
                    div()
                        .text_secondary(self.compact, cx)
                        .child(block.detail.clone()),
                )
            })
            .child(actions)
            .into_any_element()
    }

    /// Seed the draft field once per suggestion: re-seeding only when the
    /// reviewer's text itself changes keeps the user's edits across re-renders.
    pub(in super::super) fn sync_fallback_review_draft(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = self
            .workspace_store
            .read(cx)
            .active_fallback_review()
            .map(|review| review.draft.clone());
        if self.fallback_review_seeded == draft {
            return;
        }
        self.fallback_review_seeded = draft.clone();
        let draft = draft.unwrap_or_default();
        self.fallback_review_input
            .update(cx, |state, cx| state.set_value(draft, window, cx));
    }

    /// The reviewer's second opinion on a classifier stop. It advises only: the
    /// clarification below is a draft the user edits and sends themselves.
    pub(in super::super) fn render_fallback_review_panel(
        &self,
        review: &FallbackReview,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let has_draft = !review.draft.trim().is_empty();
        let input = self.fallback_review_input.clone();
        let can_send = has_draft && !input.read(cx).value().trim().is_empty();

        let header = h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .text_title(cx)
            .child(Icon::new(IconName::Info).text_color(muted))
            .child(div().flex_1().child(crate::tr!("fallback.review_title")))
            .child(
                Button::new("fallback-review-dismiss")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .aria_label(crate::tr!("fallback.dismiss"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.workspace_store
                            .update(cx, |store, _cx| store.dismiss_fallback_review());
                        cx.notify();
                    })),
            );

        // Model-authored text, rendered as plain text.
        let assessment = div()
            .text_body(self.compact, cx)
            .text_color(muted)
            .child(review.assessment.clone());

        let draft_row = h_flex()
            .w_full()
            .items_end()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(crate::material::radius_input(cx))
            .border_1()
            .border_color(cx.theme().input)
            .bg(cx.theme().popover)
            .child(
                div().flex_1().min_w_0().child(
                    Textarea::new(&self.fallback_review_input)
                        .placeholder(crate::tr!("fallback.review_placeholder"))
                        .appearance(false),
                ),
            )
            .child(
                Button::new("fallback-review-send")
                    .primary()
                    .with_size(px(SEND_SIZE))
                    .icon(IconName::ArrowUp)
                    .aria_label(crate::tr!("fallback.review_send"))
                    .disabled(!can_send)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let text = input.read(cx).value().trim().to_string();
                        if text.is_empty() {
                            return;
                        }
                        this.workspace_store.update(cx, |store, _cx| {
                            store.send_turn(text, Vec::new());
                            store.dismiss_fallback_review();
                        });
                        input.update(cx, |state, cx| state.set_value(String::new(), window, cx));
                        this.fallback_review_seeded = None;
                        cx.notify();
                    })),
            );

        crate::material::card_raised(cx)
            .w_full()
            .child(header)
            .child(assessment)
            // No draft means the reviewer did not call it a false positive:
            // there is nothing to send, so only the assessment stands.
            .when(has_draft, |this| {
                this.child(
                    div()
                        .text_secondary(self.compact, cx)
                        .child(crate::tr!("fallback.review_hint")),
                )
                .child(draft_row)
            })
            .into_any_element()
    }
}
