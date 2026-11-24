use super::super::*;
use crate::overlay::DialogActions;
use crate::store::ComposerState;

use agent::context_window::{self, OPTION_ID};

/// The context-window choice for the active model: its presets, each applied
/// as it is chosen, and a custom size.
pub(in super::super) struct ContextWindowDialog {
    store: Entity<WorkspaceStore>,
    custom: Entity<InputState>,
    invalid: bool,
    _custom_events: Subscription,
}

impl ContextWindowDialog {
    fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let max = context_window::format(largest_window(&store, cx));
        let custom = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::tr!(
                "composer.context_window_custom_placeholder",
                max = max
            ))
        });
        let custom_events =
            cx.subscribe_in(&custom, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter { .. } => this.apply_custom(window, cx),
                InputEvent::Change => {
                    this.invalid = false;
                    cx.notify();
                }
                _ => {}
            });
        Self {
            store,
            custom,
            invalid: false,
            _custom_events: custom_events,
        }
    }

    fn apply_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.custom.read(cx).value().to_string();
        let max = largest_window(&self.store, cx);
        match context_window::parse_tokens(&serde_json::Value::String(value))
            .filter(|tokens| *tokens <= max)
        {
            Some(tokens) => {
                self.store.update(cx, |store, _cx| {
                    store.set_active_option(OPTION_ID.to_string(), Some(serde_json::json!(tokens)));
                });
                window.close_dialog(cx);
            }
            None => {
                self.invalid = true;
                cx.notify();
            }
        }
    }
}

impl Render for ContextWindowDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let composer = self.store.read(cx).composer_state();
        let Some((options, default_value)) = context_window_descriptor(&composer) else {
            return div().into_any_element();
        };
        let selected = composer
            .active_option_selections
            .iter()
            .find(|selection| selection.id == OPTION_ID)
            .and_then(|selection| context_window::parse_tokens(&selection.value))
            .or_else(|| {
                default_value
                    .as_ref()
                    .and_then(|value| context_window::parse_tokens(&serde_json::json!(value)))
            });
        let tokens_of = |value: &str| context_window::parse_tokens(&serde_json::json!(value));
        let custom_selected = selected.is_some_and(|selected| {
            !options
                .iter()
                .any(|option| tokens_of(&option.value) == Some(selected))
        });
        let compact = crate::window_seam::window_is_compact(window, cx);
        let primary = cx.theme().primary;
        let default_suffix = crate::tr!("composer.option_default").into_owned();

        let row = |id: gpui::SharedString, label: String, checked: bool, cx: &App| {
            crate::material::menu_item_frame(compact, cx)
                .id(id)
                .w_full()
                .items_center()
                .cursor_pointer()
                .text_body(compact, cx)
                .hover(|style| style.bg(cx.theme().muted))
                .child(div().flex_1().min_w_0().child(label))
                .when(checked, |row| {
                    row.child(Icon::new(IconName::Check).text_color(primary))
                })
        };

        let mut list = v_flex().w_full().gap_0p5();
        for (index, option) in options.iter().enumerate() {
            let mut label = option.label.clone();
            if default_value.as_deref() == Some(option.value.as_str()) {
                label.push_str(&default_suffix);
            }
            let checked = selected.is_some() && tokens_of(&option.value) == selected;
            let store = self.store.clone();
            let value = option.value.clone();
            list = list.child(
                row(format!("context-window-{index}").into(), label, checked, cx).on_click(
                    move |_, window, cx| {
                        let value = value.clone();
                        store.update(cx, |store, _cx| {
                            store.set_active_option(
                                OPTION_ID.to_string(),
                                Some(serde_json::Value::String(value)),
                            );
                        });
                        window.close_dialog(cx);
                    },
                ),
            );
        }
        let mut custom_label = crate::tr!("composer.context_window_custom").into_owned();
        if let Some(selected) = selected.filter(|_| custom_selected) {
            custom_label.push_str(&format!(" ({})", context_window::format(selected)));
        }
        let custom = self.custom.clone();
        list.child(
            row(
                "context-window-custom".into(),
                custom_label,
                custom_selected,
                cx,
            )
            .on_click(move |_, window, cx| {
                custom.update(cx, |state, cx| state.focus(window, cx));
            }),
        )
        .child(
            v_flex()
                .px_2()
                .pt_1()
                .gap_1()
                .child(Input::new(&self.custom))
                .when(self.invalid, |this| {
                    this.child(
                        div()
                            .text_secondary(compact, cx)
                            .text_color(cx.theme().danger)
                            .child(crate::tr!(
                                "composer.context_window_invalid",
                                max = context_window::format(largest_window(&self.store, cx))
                            )),
                    )
                }),
        )
        .into_any_element()
    }
}

/// The active model's `contextWindow` presets and default, when it has one.
pub(in super::super) fn context_window_descriptor(
    composer: &ComposerState,
) -> Option<(Vec<agent::SelectOption>, Option<String>)> {
    match context_window::descriptor(&composer.active_option_descriptors)? {
        OptionDescriptor::Select {
            options,
            default_value,
            ..
        } => Some((options.clone(), default_value.clone())),
        OptionDescriptor::Boolean { .. } => None,
    }
}

/// The largest preset of the active model: a custom size cannot exceed the
/// window the model can launch with. A model whose presets are unknown is
/// capped at the largest window any provider offers today.
fn largest_window(store: &Entity<WorkspaceStore>, cx: &App) -> u64 {
    context_window::largest(&store.read(cx).composer_state().active_option_descriptors)
        .unwrap_or(1_000_000)
}

/// Open the context-window dialog for the active model.
pub(in super::super) fn open(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut App) {
    let dialog = cx.new(|cx| ContextWindowDialog::new(store, window, cx));
    window.open_dialog(cx, move |builder, _, _| {
        let content = dialog.clone();
        let apply = dialog.clone();
        builder
            .title(crate::tr!("composer.context_window_title").into_owned())
            .content(move |content_el, _, _| content_el.child(content.clone()))
            .footer(
                DialogActions::new()
                    .child(
                        Button::new("context-window-cancel")
                            .label(crate::tr!("composer.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("context-window-apply")
                            .primary()
                            .label(crate::tr!("composer.context_window_apply"))
                            .on_click(move |_, window, cx| {
                                apply.update(cx, |dialog, cx| dialog.apply_custom(window, cx));
                            }),
                    ),
            )
    });
}
