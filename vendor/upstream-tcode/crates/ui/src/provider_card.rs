//! One Settings → Providers list row.
//!
//! A compact, non-expanding row: driver glyph, name, `v<version>`,
//! an update icon when a newer CLI exists, the status summary line, a gear button
//! and the enable switch. The row body and the gear both open the per-profile
//! settings dialog, a transactional modal form.

use crate::icon::Icon;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use crate::{
    icon::IconName,
    overlay::OverlayExt as _,
    provider_dialog::ProviderDialog,
    provider_status::{EMAIL_SLOT, StatusDot, redact_email},
    sizing::Sizable as _,
    store::{TopicKind, WorkspaceStore, observe_store_topics},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        switch::Switch,
    },
};
use gpui::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui::{Styled as _, rgb};
use gpui_base::{h_flex, v_flex};

use agent::ProviderKind;
use tcode_core::settings::builtin_provider_color;

/// The card's dialog asked to manage this profile's plugins.
pub struct ManagePlugins(pub String);

pub struct ProviderCard {
    store: Entity<WorkspaceStore>,
    /// The protocol this card's profile drives (glyph, shared model catalog /
    /// status / version are all keyed on it).
    provider: ProviderKind,
    /// Which profile this row represents: a built-in native-provider id or a
    /// user profile slug. Status/config/secret lookups key on this.
    profile_id: String,
    /// Whether the account email in the summary line is revealed.
    email_revealed: bool,
    _subscription: Subscription,
}

impl gpui::EventEmitter<ManagePlugins> for ProviderCard {}

impl ProviderCard {
    pub fn new(
        store: Entity<WorkspaceStore>,
        provider: ProviderKind,
        profile_id: impl Into<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription =
            observe_store_topics(&store, &[TopicKind::Settings, TopicKind::Providers], cx);
        Self {
            store,
            provider,
            profile_id: profile_id.into(),
            email_revealed: false,
            _subscription: subscription,
        }
    }

    /// Open the per-profile settings modal (the transactional editor).
    fn open_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let provider = self.provider;
        let profile_id = self.profile_id.clone();
        let title = self
            .store
            .read(cx)
            .provider_profile_display_name(&profile_id);
        let card = cx.entity().downgrade();
        let manage_id = profile_id.clone();
        let manage_plugins: std::rc::Rc<dyn Fn(&mut gpui::App)> = std::rc::Rc::new(move |cx| {
            _ = card.update(cx, |_, cx| cx.emit(ManagePlugins(manage_id.clone())));
        });
        let dialog = cx.new(|cx| {
            ProviderDialog::new(
                store.clone(),
                provider,
                profile_id.clone(),
                manage_plugins,
                window,
                cx,
            )
        });
        window.open_dialog(cx, move |dlg, _, cx| {
            let content = dialog.clone();
            dlg.title(title.clone())
                .wide()
                .content(move |content_el, _window, _cx| content_el.child(content.clone()))
                .footer(crate::provider_dialog::render_footer(&dialog, cx))
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let provider = self.provider;
        // Update checks target the built-in installation; a custom profile can
        // launch another binary with a different package-manager environment.
        let store = self.store.read(cx);
        let name = store.provider_profile_display_name(&self.profile_id);
        let enabled = store.provider_profile_settings(&self.profile_id).enabled;
        let snapshot = store.provider_profile_snapshot(&self.profile_id);
        let summary = crate::provider_status::summarize(provider, snapshot.as_ref(), enabled);
        let provider_version = (self.profile_id
            == tcode_core::settings::Settings::builtin_profile_id(provider))
        .then(|| store.provider_version_status(provider))
        .flatten();
        let version = snapshot
            .as_ref()
            .and_then(|s| s.version.clone())
            .or_else(|| provider_version.as_ref().and_then(|v| v.installed.clone()));
        let update_available = provider_version
            .as_ref()
            .is_some_and(|v| v.update_available);
        let accent = store.provider_profile_accent(&self.profile_id);

        let provider_icon = provider_glyph(provider);
        let provider_icon = match accent {
            Some(accent) => provider_icon.text_color(rgb(accent)),
            None => provider_icon,
        };
        let glyph = div().flex_none().text_body(false, cx).child(provider_icon);

        let title = h_flex()
            .gap_2()
            .items_center()
            .child(div().text_title(cx).child(name.clone()))
            .when_some(version, |this, version| {
                this.child(
                    div()
                        .text_meta_mono(false, cx)
                        .child(format!("v{}", version.trim_start_matches('v'))),
                )
            });

        // The row body (glyph + text) is the primary "configure" affordance;
        // the update popover, gear and switch trail it as their own controls.
        let body = crate::material::accessible_clickable(
            h_flex(),
            gpui::SharedString::from(format!("configure-{}", self.profile_id)),
            gpui::Role::Button,
            crate::tr!("providers.configure", name = name.clone()),
            cx,
        )
        .flex_1()
        .min_w_0()
        .gap_3()
        .items_center()
        .cursor_pointer()
        .child(glyph)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(title)
                .child(self.render_summary_line(&summary, cx))
                .when(!provider.caps().mcp_servers, |this| {
                    this.child(
                        div()
                            .text_secondary(false, cx)
                            .child(crate::tr!("providers.mcp_unavailable")),
                    )
                }),
        )
        .tooltip({
            let name = name.clone();
            move |window, cx| {
                let label = crate::tr!("providers.configure", name = name.clone()).into_owned();
                crate::widgets::tooltip::Tooltip::new(label).build(window, cx)
            }
        })
        .on_click(cx.listener(|this, _, window, cx| this.open_dialog(window, cx)));

        crate::material::group_row(cx)
            .child(body)
            .when(update_available, |this| {
                this.child(self.render_update_popover(cx))
            })
            .child(
                Button::new("configure-profile")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Settings)
                    .tooltip(crate::tr!("providers.configure", name = name.clone()))
                    .on_click(cx.listener(|this, _, window, cx| this.open_dialog(window, cx))),
            )
            .child(
                Switch::new("enable-provider")
                    .checked(enabled)
                    .tooltip(crate::tr!("providers.enable", name = name))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let checked = *checked;
                        let profile_id = this.profile_id.clone();
                        this.store.update(cx, |store, _cx| {
                            store.update_profile_settings(
                                profile_id,
                                tcode_core::settings::ProfileSettingsPatch::SetEnabled {
                                    enabled: checked,
                                },
                            );
                            store.reload_provider();
                        });
                    })),
            )
            .into_any_element()
    }

    /// The status summary: headline (with a click-to-reveal email when the probe
    /// found one) followed by the probe's diagnostic detail.
    fn render_summary_line(
        &self,
        summary: &crate::provider_status::StatusSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let (status_bg, status_fg) = match summary.dot {
            StatusDot::Loading => (cx.theme().muted.opacity(0.), muted),
            StatusDot::Success => (
                cx.theme().success.opacity(0.12),
                cx.theme().success_foreground,
            ),
            StatusDot::Warning => (
                cx.theme().warning.opacity(0.12),
                cx.theme().warning_foreground,
            ),
            StatusDot::Error => (
                cx.theme().danger.opacity(0.12),
                cx.theme().danger_foreground,
            ),
            StatusDot::Amber => (cx.theme().muted, muted),
        };
        let (lead, rest) = match &summary.email {
            Some(_) => summary
                .headline
                .split_once(EMAIL_SLOT)
                .map(|(prefix, suffix)| (prefix.trim_end().to_string(), Some(suffix)))
                .unwrap_or((summary.headline.clone(), Some(""))),
            None => (summary.headline.clone(), None),
        };
        let mut line = crate::material::semantic_chip(lead, status_bg, status_fg, cx)
            .flex()
            .items_center()
            .gap_1();

        if let (Some(email), Some(suffix)) = (&summary.email, rest) {
            {
                let revealed = self.email_revealed;
                let shown = if revealed {
                    email.clone()
                } else {
                    redact_email(email)
                };
                line = line
                    .child(
                        Button::new("reveal-email")
                            .text()
                            .xsmall()
                            .label(shown)
                            .tooltip(if revealed {
                                crate::tr!("providers.hide_email")
                            } else {
                                crate::tr!("providers.reveal_email")
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.email_revealed = !this.email_revealed;
                                cx.notify();
                            })),
                    )
                    .child(div().child(suffix.trim_start().to_string()));
            }
        }
        if !summary.detail.is_empty() {
            line = line.child(div().child(format!("· {}", summary.detail)));
        }
        // Row wrapper so the chip left-aligns and hugs its content instead of
        // being stretched to the column width by the surrounding v_flex.
        h_flex().w_full().min_w_0().child(line).into_any_element()
    }

    /// The update-available icon + its popover.
    fn render_update_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let provider = self.provider;
        let version = self.store.read(cx).provider_version_status(provider);
        let updating = version.as_ref().is_some_and(|v| v.updating);
        let requires_terminal = version.as_ref().is_some_and(|v| v.update_requires_terminal);
        let command = version.as_ref().and_then(|v| v.update_command.clone());
        let summary = version
            .as_ref()
            .and_then(|v| v.update_command_summary.clone());
        let store = self.store.clone();

        crate::material::overlay_popover("update-popover", cx)
            .p_3()
            .trigger(
                Button::new("update-available")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/download.svg"))
                    .tooltip(crate::tr!("providers.update_aria")),
            )
            .content(move |_, _, cx| {
                let store = store.clone();
                let command = command.clone();
                // The Popover panel supplies the fill, border, shadow and p_3
                // padding; the pane itself stays transparent (single surface).
                let mut pane = v_flex()
                    .w(px(320.))
                    .gap_2()
                    .child(
                        div()
                            .text_title(cx)
                            .child(crate::tr!("providers.update_title")),
                    )
                    .child(div().text_secondary(false, cx).child(if command.is_none() {
                        crate::tr!("providers.update_unmanaged")
                    } else if requires_terminal {
                        crate::tr!("providers.update_terminal")
                    } else {
                        crate::tr!("providers.update_message")
                    }));
                if command.is_some() && !requires_terminal {
                    pane = pane.child(
                        Button::new("update-now")
                            .primary()
                            .small()
                            .loading(updating)
                            .label(if updating {
                                crate::tr!("providers.updating")
                            } else {
                                crate::tr!("providers.update_now")
                            })
                            .on_click({
                                let store = store.clone();
                                move |_, _, cx| {
                                    store.update(cx, |store, _cx| {
                                        store.update_providers(vec![provider]);
                                    });
                                }
                            }),
                    );
                }
                if let Some(command) = command {
                    let copy = command.clone();
                    pane = pane
                        .child(div().pt_1().text_caption(cx).child(if requires_terminal {
                            crate::tr!("providers.update_command")
                        } else {
                            crate::tr!("providers.update_manual")
                        }))
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .items_center()
                                .rounded(crate::material::radius_input(cx))
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().muted)
                                .px_2()
                                .py_1()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_code(cx)
                                        .child(crate::widgets::fade_text(
                                            summary.clone().unwrap_or_else(|| command.clone()),
                                        )),
                                )
                                .child(
                                    Button::new("copy-command")
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Copy)
                                        .tooltip(crate::tr!("providers.copy_command"))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ));
                                        }),
                                )
                                .context_menu({
                                    let command = command.clone();
                                    move |menu, _, _| {
                                        menu.menu(
                                            crate::tr!("providers.copy_command").into_owned(),
                                            Box::new(CopyText(command.clone())),
                                        )
                                    }
                                }),
                        );
                }
                pane
            })
            .into_any_element()
    }
}

impl Render for ProviderCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w_full().child(self.render_header(cx))
    }
}

/// The provider's glyph (the same asset the composer's picker rail uses).
/// Claude's is pre-tinted with its brand color; the others take the text color.
pub fn provider_glyph(provider: ProviderKind) -> Icon {
    let (path, tint) = match provider {
        ProviderKind::ClaudeCode => ("icons/claude.svg", builtin_provider_color(provider)),
        ProviderKind::Codex => ("icons/openai.svg", None),
        ProviderKind::Pi => ("icons/pi.svg", None),
        ProviderKind::OpenCode => ("icons/opencode.svg", None),
        ProviderKind::Cursor => ("icons/cursor.svg", None),
        ProviderKind::Grok => ("icons/grok.svg", None),
        ProviderKind::Acp => return Icon::empty(),
    };
    Icon::empty()
        .path(path)
        .when_some(tint, |icon, tint| icon.text_color(rgb(tint)))
}
