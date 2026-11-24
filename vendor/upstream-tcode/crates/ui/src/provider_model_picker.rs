//! Reusable native-provider model picker used by settings surfaces.
//!
//! The profile tabs (one per provider profile, built-in or user-created),
//! resolved model catalog, offline/custom-model handling, and selection event
//! live here so Orchestrate and other settings do not grow subtly different
//! pickers.

use std::rc::Rc;

use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::tabs::{TabBar, TabItem};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_base::{PopoverState, h_flex, v_flex};

use agent::ProviderKind;
use tcode_core::settings::Settings;

use crate::provider_card::provider_glyph;
use crate::settings::provider_label;
use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelOption {
    pub provider: ProviderKind,
    pub id: String,
    pub name: String,
    pub profile_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ModelSelected(pub ModelOption);

#[derive(Clone)]
enum TriggerKind {
    /// Translation key, resolved at render so a language switch reaches it.
    Add(&'static str),
    Selection,
}

pub(crate) struct ProviderModelPicker {
    store: Entity<WorkspaceStore>,
    popover_id: &'static str,
    trigger_id: &'static str,
    trigger_kind: TriggerKind,
    /// The active tab: a profile id (built-in or user-created).
    selected_profile: String,
    selected: Option<(ProviderKind, String, Option<String>)>,
    excluded: Vec<(ProviderKind, String)>,
    _store_subscription: Subscription,
}

impl EventEmitter<ModelSelected> for ProviderModelPicker {}

impl ProviderModelPicker {
    pub fn add(
        store: Entity<WorkspaceStore>,
        popover_id: &'static str,
        trigger_id: &'static str,
        label_key: &'static str,
        cx: &mut Context<Self>,
    ) -> Self {
        let store_subscription =
            observe_store_topics(&store, &[TopicKind::Settings, TopicKind::Providers], cx);
        Self {
            store,
            popover_id,
            trigger_id,
            trigger_kind: TriggerKind::Add(label_key),
            selected_profile: Settings::builtin_profile_id(ProviderKind::Codex).to_string(),
            selected: None,
            excluded: Vec::new(),
            _store_subscription: store_subscription,
        }
    }

    pub fn selection(
        store: Entity<WorkspaceStore>,
        popover_id: &'static str,
        trigger_id: &'static str,
        provider: ProviderKind,
        model: impl Into<String>,
        profile_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let store_subscription =
            observe_store_topics(&store, &[TopicKind::Settings, TopicKind::Providers], cx);
        let model = model.into();
        Self {
            store,
            popover_id,
            trigger_id,
            trigger_kind: TriggerKind::Selection,
            selected_profile: selection_profile_id(provider, profile_id.as_deref()),
            selected: Some((provider, model, profile_id)),
            excluded: Vec::new(),
            _store_subscription: store_subscription,
        }
    }

    pub fn set_selected(
        &mut self,
        provider: ProviderKind,
        model: impl Into<String>,
        profile_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let selected = (provider, model.into(), profile_id);
        if self.selected.as_ref() != Some(&selected) {
            self.selected_profile = selection_profile_id(provider, selected.2.as_deref());
            self.selected = Some(selected);
            cx.notify();
        }
    }

    pub fn set_excluded(&mut self, excluded: Vec<(ProviderKind, String)>, cx: &mut Context<Self>) {
        if self.excluded != excluded {
            self.excluded = excluded;
            cx.notify();
        }
    }

    /// The models a profile tab offers. Only enabled profiles are offered for
    /// new selections; a disabled profile stays configurable in Settings but
    /// never reaches the picker.
    fn profile_options(&self, profile_id: &str, cx: &App) -> Vec<ModelOption> {
        let store = self.store.read(cx);
        let Some(profile) = store
            .enabled_profiles()
            .into_iter()
            .find(|profile| profile.id == profile_id)
        else {
            return Vec::new();
        };
        let option_profile =
            (!Settings::is_builtin_profile_id(&profile.id)).then_some(profile.id.clone());
        store
            .picker_models_for_profile(&profile.id)
            .into_iter()
            .map(|model| ModelOption {
                provider: profile.kind,
                id: model.id,
                name: model.name,
                profile_id: option_profile.clone(),
            })
            .collect()
    }

    pub(crate) fn display_name(
        &self,
        provider: ProviderKind,
        model: &str,
        profile_id: Option<&str>,
        cx: &App,
    ) -> String {
        self.profile_options(&selection_profile_id(provider, profile_id), cx)
            .into_iter()
            .find(|option| {
                option.provider == provider
                    && option.id == model
                    && option.profile_id.as_deref() == profile_id
            })
            .map(|option| option.name)
            .unwrap_or_else(|| model.to_string())
    }

    fn trigger(&self, cx: &Context<Self>) -> Button {
        match &self.trigger_kind {
            TriggerKind::Add(label_key) => Button::new(self.trigger_id)
                .outline()
                .small()
                .icon(IconName::Plus)
                .label(crate::tr!(*label_key)),
            TriggerKind::Selection => {
                let (provider, model, profile_id) = self
                    .selected
                    .as_ref()
                    .map(|(provider, model, profile_id)| {
                        (*provider, model.as_str(), profile_id.as_deref())
                    })
                    .unwrap_or_else(|| {
                        let kind = self
                            .store
                            .read(cx)
                            .provider_profile_kind(&self.selected_profile);
                        (kind, "", None)
                    });
                let display = self.display_name(provider, model, profile_id, cx);
                let glyph = tinted_glyph(&self.store, provider, profile_id, cx);
                Button::new(self.trigger_id)
                    .ghost()
                    .outline()
                    .compact()
                    .child(
                        h_flex()
                            .w(px(230.))
                            .items_center()
                            .gap_2()
                            .text_body(false, cx)
                            .child(glyph)
                            .child(div().flex_1().min_w_0().child(display))
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
            }
        }
    }
}

impl Render for ProviderModelPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let picker = cx.entity();
        crate::material::overlay_popover(self.popover_id, cx)
            .trigger(self.trigger(cx))
            .content(move |_, window, cx| {
                let (profiles, selected_profile, selected, excluded) = {
                    let picker = picker.read(cx);
                    (
                        picker.store.read(cx).enabled_profiles(),
                        picker.selected_profile.clone(),
                        picker.selected.clone(),
                        picker.excluded.clone(),
                    )
                };
                let store = picker.read(cx).store.clone();
                // A deleted profile falls back to the first tab (built-ins
                // always exist, so the list is never empty).
                let current_profile = if profiles.iter().any(|p| p.id == selected_profile) {
                    selected_profile
                } else {
                    profiles.first().map(|p| p.id.clone()).unwrap_or_default()
                };
                let available: Rc<[ModelOption]> = picker
                    .read(cx)
                    .profile_options(&current_profile, cx)
                    .into_iter()
                    .filter(|option| {
                        !excluded.iter().any(|(provider, model)| {
                            *provider == option.provider && model == &option.id
                        })
                    })
                    .collect();

                // The catalog is a viewport of its own; cap it against the
                // window so a short one scrolls instead of overflowing it.
                let catalog = crate::sizing::fit_viewport(300., window.viewport_size().height);
                let rows = if available.is_empty() {
                    div()
                        .w_full()
                        .h(catalog)
                        .p_1()
                        .child(crate::material::empty_row(
                            crate::tr!("model_picker.no_models"),
                            cx,
                        ))
                        .into_any_element()
                } else {
                    let picker = picker.clone();
                    let store = store.clone();
                    let popover = cx.entity();
                    crate::scroll::VirtualList::uniform(
                        "settings-model-options",
                        available.len(),
                        move |range, _, cx| {
                            range
                                .map(|index| {
                                    let option = &available[index];
                                    let is_selected = selected.as_ref().is_some_and(
                                        |(provider, model, profile_id)| {
                                            *provider == option.provider
                                                && model == &option.id
                                                && profile_id == &option.profile_id
                                        },
                                    );
                                    div().pb_0p5().child(option_row(
                                        index,
                                        option,
                                        is_selected,
                                        &picker,
                                        &store,
                                        &popover,
                                        cx,
                                    ))
                                })
                                .collect()
                        },
                    )
                    .scrollbar()
                    .w_full()
                    .h(catalog)
                    .pt_1()
                    .px_1()
                    // The last row carries its gap.
                    .pb(px(2.))
                    .into_any_element()
                };

                // Keep profile tabs in the same order as the settings cards.
                let mut tabs = TabBar::new("provider-model-picker-tabs")
                    .segmented()
                    .w_full();
                for (tab_index, profile) in profiles.iter().enumerate() {
                    let is_selected = profile.id == current_profile;
                    let label = if Settings::is_builtin_profile_id(&profile.id) {
                        provider_label(profile.kind).to_string()
                    } else {
                        profile
                            .settings
                            .display_name
                            .as_deref()
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .unwrap_or(&profile.id)
                            .to_string()
                    };
                    let kind = profile.kind;
                    let profile_id = profile.id.clone();
                    let picker = picker.clone();
                    let popover = cx.entity();
                    tabs = tabs.item(
                        TabItem::new(("settings-provider-tab", tab_index), label)
                            .icon(tinted_glyph(&store, kind, Some(&profile.id), cx))
                            .selected(is_selected)
                            .on_click(move |_, _, cx| {
                                picker.update(cx, |picker, cx| {
                                    picker.selected_profile = profile_id.clone();
                                    cx.notify();
                                });
                                popover.update(cx, |_, cx| cx.notify());
                            }),
                    );
                }

                v_flex()
                    .w(crate::sizing::fit_viewport(
                        390.,
                        window.viewport_size().width,
                    ))
                    .child(tabs)
                    .child(crate::material::faded_hairline(cx))
                    .child(rows)
            })
    }
}

fn option_row(
    index: usize,
    option: &ModelOption,
    is_selected: bool,
    picker: &Entity<ProviderModelPicker>,
    store: &Entity<WorkspaceStore>,
    popover: &Entity<PopoverState>,
    cx: &App,
) -> AnyElement {
    let picker = picker.clone();
    let popover = popover.clone();
    let selected = option.clone();
    crate::material::accessible_clickable(
        crate::material::menu_item_frame(false, cx),
        ("settings-model-option", index),
        gpui::Role::MenuItem,
        option.name.clone(),
        cx,
    )
    .aria_selected(is_selected)
    .flex_none()
    .w_full()
    .items_center()
    .text_body(false, cx)
    .cursor_pointer()
    .hover(|style| style.bg(cx.theme().muted))
    .child(tinted_glyph(
        store,
        option.provider,
        option.profile_id.as_deref(),
        cx,
    ))
    .child(
        v_flex()
            .flex_1()
            .min_w_0()
            .child(
                div()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(option.name.clone())),
            )
            .child(
                div()
                    .overflow_hidden()
                    .text_meta_mono(false, cx)
                    .child(crate::widgets::fade_text(option.id.clone())),
            ),
    )
    .when(is_selected, |row| row.child(Icon::new(IconName::Check)))
    .on_click(move |_, window, cx| {
        picker.update(cx, |picker, cx| {
            picker.selected_profile = option_profile_id(&selected);
            if matches!(picker.trigger_kind, TriggerKind::Selection) {
                picker.selected = Some((
                    selected.provider,
                    selected.id.clone(),
                    selected.profile_id.clone(),
                ));
            }
            cx.emit(ModelSelected(selected.clone()));
        });
        popover.update(cx, |state, cx| state.dismiss(window, cx));
    })
    .into_any_element()
}

/// The provider glyph tinted with the profile's own accent (falling back to the
/// kind's built-in card accent), matching the composer's picker rail.
fn tinted_glyph(
    store: &Entity<WorkspaceStore>,
    provider: ProviderKind,
    profile_id: Option<&str>,
    cx: &App,
) -> Icon {
    let glyph = provider_glyph(provider);
    let profile_id = profile_id
        .map(str::to_string)
        .unwrap_or_else(|| Settings::builtin_profile_id(provider).to_string());
    let accent = store.read(cx).provider_profile_accent(&profile_id);
    match accent {
        Some(accent) => glyph.text_color(rgb(accent)),
        None => glyph,
    }
}

/// The profile a selection belongs to: its explicit profile id, or the kind's
/// built-in profile.
fn selection_profile_id(provider: ProviderKind, profile_id: Option<&str>) -> String {
    profile_id
        .map(str::to_string)
        .unwrap_or_else(|| Settings::builtin_profile_id(provider).to_string())
}

/// The profile tab an option files under (see [`selection_profile_id`]).
fn option_profile_id(option: &ModelOption) -> String {
    selection_profile_id(option.provider, option.profile_id.as_deref())
}
