//! Settings → Traverse: the relay and lookup services this machine publishes
//! to, each with how it is doing while hosting.

use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task,
    Window, div, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_core::settings::{TraverseInstance, TraverseSetting, TraverseSource};
use tcode_protocol::{
    TraverseLookupState, TraverseManifestState, TraverseRelayStatus, TraverseSourceStatus,
};

use super::hosting::{
    RemoteController, apply_hosting_settings, custom_traverse_url, field_labels, field_row,
};
use crate::icon::{Icon, IconName};
use crate::material::{group_caption, grouped};
use crate::overlay::{Notification, OverlayExt as _};
use crate::settings_rows::{FIELD_WIDTH, ToggleRow, footnote};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::switch::Switch;

/// How often the sources' status is read again while hosting.
const STATUS_REFRESH: Duration = Duration::from_secs(2);

/// One Traverse source as the page edits it.
struct TraverseRow {
    enabled: bool,
    /// A self-hosted instance's base URL as typed; `None` is the official
    /// service.
    url: Option<(Entity<InputState>, gpui::Subscription)>,
    /// Whether every relay and lookup of its status is shown.
    expanded: bool,
}

pub struct TraversePanel {
    /// The sources in order, the official service first.
    rows: Vec<TraverseRow>,
    /// The URL of a self-hosted instance about to be added.
    add_input: Entity<InputState>,
    /// Repaints the status while hosting.
    ticker: Option<Task<()>>,
    _subscription: gpui::Subscription,
}

impl TraversePanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let traverse = cx
            .try_global::<RemoteController>()
            .map(|controller| controller.local_settings().traverse.clone())
            .unwrap_or_default();
        let rows = traverse
            .sources
            .iter()
            .map(|source| TraverseRow {
                enabled: source.enabled,
                expanded: false,
                url: match &source.instance {
                    TraverseInstance::Official => None,
                    TraverseInstance::Custom { url } => Some(url_input(url, window, cx)),
                },
            })
            .collect();
        let add_input = cx.new(|cx| InputState::new(window, cx));
        // Enter adds the typed instance; the field repaints as the URL turns
        // valid.
        let subscription = cx.subscribe_in(
            &add_input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.add(window, cx),
                InputEvent::Change => cx.notify(),
                InputEvent::Blur | InputEvent::Focus => {}
            },
        );
        Self {
            rows,
            add_input,
            ticker: None,
            _subscription: subscription,
        }
    }

    fn sync_ticker(&mut self, hosting: bool, cx: &mut Context<Self>) {
        match (hosting, self.ticker.is_some()) {
            (true, false) => {
                self.ticker = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(STATUS_REFRESH).await;
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            return;
                        }
                    }
                }));
            }
            (false, true) => self.ticker = None,
            _ => {}
        }
    }

    /// The self-hosted URLs as typed, one per row (`None` for the official
    /// service), each a usable base URL or why it is not one.
    fn typed_urls(&self, cx: &App) -> Vec<Option<Result<url::Url, SharedString>>> {
        let mut seen = Vec::new();
        self.rows
            .iter()
            .map(|row| {
                let (input, _) = row.url.as_ref()?;
                Some(
                    match custom_traverse_url(&input.read(cx).value()) {
                        None => Err(crate::tr!("remote.traverse.invalid_url")),
                        Some(url) if seen.contains(&url) => {
                            Err(crate::tr!("remote.traverse.duplicate"))
                        }
                        Some(url) => {
                            seen.push(url.clone());
                            Ok(url)
                        }
                    }
                    .map_err(|error| error.into_owned().into()),
                )
            })
            .collect()
    }

    /// The setting as edited, or `None` while a self-hosted URL is not a
    /// usable one.
    fn typed(&self, cx: &App) -> Option<TraverseSetting> {
        let sources = self
            .rows
            .iter()
            .zip(self.typed_urls(cx))
            .map(|(row, url)| {
                Some(TraverseSource {
                    instance: match url {
                        None => TraverseInstance::Official,
                        Some(url) => TraverseInstance::Custom {
                            url: url.ok()?.to_string(),
                        },
                    },
                    enabled: row.enabled,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(TraverseSetting::new(sources))
    }

    /// The instance typed into the add field, or why it cannot be added;
    /// `None` while the field is empty.
    fn typed_addition(&self, cx: &App) -> Option<Result<url::Url, SharedString>> {
        let value = self.add_input.read(cx).value();
        if value.trim().is_empty() {
            return None;
        }
        Some(match custom_traverse_url(&value) {
            None => Err(crate::tr!("remote.traverse.invalid_url")
                .into_owned()
                .into()),
            Some(url)
                if self
                    .typed_urls(cx)
                    .into_iter()
                    .flatten()
                    .any(|typed| typed.as_ref() == Ok(&url)) =>
            {
                Err(crate::tr!("remote.traverse.duplicate").into_owned().into())
            }
            Some(url) => Ok(url),
        })
    }

    /// Save the edits, re-binding the endpoint while hosting so they take
    /// effect at once. Nothing happens while they match what is saved or are
    /// incomplete, so leaving a field untouched never restarts the host.
    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(controller) = cx.try_global::<RemoteController>() else {
            return;
        };
        let name = controller.local_settings().remote_host_name.clone();
        match self.typed(cx) {
            Some(traverse) if traverse != controller.local_settings().traverse => {
                // The hosting switch that shows a failure is on another page.
                if let Err(error) = apply_hosting_settings(traverse, name, cx) {
                    window.push_notification(Notification::error(error), cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// A source's switch applies at once.
    fn set_enabled(
        &mut self,
        index: usize,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.rows.get_mut(index) {
            row.enabled = enabled;
        }
        self.apply(window, cx);
    }

    /// Remove a self-hosted instance; the official service is only ever
    /// switched off.
    fn remove(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.get(index).is_some_and(|row| row.url.is_some()) {
            self.rows.remove(index);
        }
        self.apply(window, cx);
    }

    /// Add the typed instance, enabled, and apply it at once.
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Ok(url)) = self.typed_addition(cx) else {
            return;
        };
        let input = url_input(url.as_str(), window, cx);
        self.rows.push(TraverseRow {
            enabled: true,
            expanded: false,
            url: Some(input),
        });
        self.add_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.apply(window, cx);
    }

    fn toggle_detail(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(index) {
            row.expanded = !row.expanded;
        }
        cx.notify();
    }

    fn render_rows(&mut self, compact: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let field = |compact: bool| {
            div()
                .when(compact, |field| field.flex_1().min_w_0())
                .when(!compact, |field| {
                    field.w(px(FIELD_WIDTH)).min_w_0().flex_shrink(1.)
                })
        };
        let urls = self.typed_urls(cx);
        let (hosting, statuses) = cx
            .try_global::<RemoteController>()
            .map(|controller| (controller.is_hosting(), controller.traverse_status()))
            .unwrap_or_default();
        self.sync_ticker(hosting, cx);
        let mut rows = Vec::new();
        for (index, (source, url)) in self.rows.iter().zip(urls).enumerate() {
            // The status names a source as invitations do.
            let key = match &url {
                None => Some(tcode_client::pairing::TRAVERSE_OFFICIAL.to_owned()),
                Some(Ok(url)) => Some(url.to_string()),
                Some(Err(_)) => None,
            };
            let status = key
                .filter(|_| hosting)
                .and_then(|key| statuses.iter().find(|status| status.source == key));
            let chip = source
                .enabled
                .then(|| status_chip(index, status, source.expanded, cx));
            let detail = status.filter(|_| source.enabled).map(|status| {
                div()
                    .w_full()
                    .px(px(crate::material::CARD_INSET))
                    .pb(cx.theme().tokens.spacing.sm)
                    .child(
                        gpui_base::Collapsible::new()
                            .w_full()
                            .open(source.expanded)
                            .content(status_detail(status, cx)),
                    )
            });
            let switch_id = SharedString::from(format!("remote-traverse-{index}"));
            let set_enabled = cx.listener(move |this, checked: &bool, window, cx| {
                this.set_enabled(index, *checked, window, cx);
            });
            let element = match (&source.url, url) {
                (Some((input, _)), Some(url)) => {
                    let description = match url {
                        Ok(_) => crate::tr!("remote.traverse.url_description")
                            .into_owned()
                            .into(),
                        Err(error) => error,
                    };
                    let remove = Button::new(SharedString::from(format!(
                        "remote-traverse-remove-{index}"
                    )))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .aria_label(crate::tr!("remote.traverse.remove"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.remove(index, window, cx);
                    }));
                    field_row(compact, cx)
                        .child(field_labels(
                            crate::tr!("remote.traverse.custom").into_owned().into(),
                            Some(description),
                            compact,
                            cx,
                        ))
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap_3()
                                .items_center()
                                .when(compact, |controls| controls.w_full())
                                .child(
                                    field(compact).child(Input::new(input).placeholder(
                                        crate::tr!("remote.traverse.url_placeholder"),
                                    )),
                                )
                                .child(remove)
                                .children(chip)
                                // Beside a text field the row cannot be one
                                // control, so the switch is its own.
                                .child(
                                    Switch::new(switch_id)
                                        .checked(source.enabled)
                                        .on_click(set_enabled),
                                ),
                        )
                        .into_any_element()
                }
                _ => ToggleRow::new(
                    switch_id,
                    crate::tr!("remote.traverse.official"),
                    source.enabled,
                    compact,
                    set_enabled,
                )
                .accessory(chip.map(|chip| {
                    // The accessory takes the room the labels leave, so the
                    // chip sits by the switch at the row's end.
                    h_flex()
                        .flex_1()
                        .justify_end()
                        .child(chip)
                        .into_any_element()
                }))
                .into_any_element(),
            };
            rows.push(
                v_flex()
                    .w_full()
                    .child(element)
                    .children(detail)
                    .into_any_element(),
            );
        }
        let addition = self.typed_addition(cx);
        let add_description = match &addition {
            Some(Err(error)) => error.clone(),
            _ => crate::tr!("remote.traverse.url_description")
                .into_owned()
                .into(),
        };
        rows.push(
            field_row(compact, cx)
                .child(field_labels(
                    crate::tr!("remote.traverse.add_title").into_owned().into(),
                    Some(add_description),
                    compact,
                    cx,
                ))
                .child(
                    h_flex()
                        .min_w_0()
                        .gap_3()
                        .items_center()
                        .when(compact, |controls| controls.w_full())
                        .child(
                            field(compact).child(
                                Input::new(&self.add_input)
                                    .placeholder(crate::tr!("remote.traverse.url_placeholder")),
                            ),
                        )
                        .child(
                            Button::new("remote-traverse-add")
                                .ghost()
                                .outline()
                                .compact()
                                .icon(IconName::Plus)
                                .label(crate::tr!("remote.traverse.add"))
                                .disabled(!matches!(addition, Some(Ok(_))))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add(window, cx);
                                })),
                        ),
                )
                .into_any_element(),
        );
        rows
    }
}

/// The field holding a self-hosted instance's URL, which takes effect when it
/// is left or Enter is pressed.
fn url_input(
    url: &str,
    window: &mut Window,
    cx: &mut Context<TraversePanel>,
) -> (Entity<InputState>, gpui::Subscription) {
    let input = cx.new(|cx| InputState::new(window, cx).default_value(url.to_owned()));
    let subscription = cx.subscribe_in(
        &input,
        window,
        |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::Blur | InputEvent::PressEnter { .. } => this.apply(window, cx),
            InputEvent::Change => cx.notify(),
            InputEvent::Focus => {}
        },
    );
    (input, subscription)
}

fn manifest_word(state: TraverseManifestState) -> SharedString {
    match state {
        TraverseManifestState::Live => crate::tr!("remote.traverse.status.live"),
        TraverseManifestState::Cached => crate::tr!("remote.traverse.status.cached"),
        TraverseManifestState::Bundled => crate::tr!("remote.traverse.status.bundled"),
        TraverseManifestState::Failed => crate::tr!("remote.traverse.status.failed"),
    }
    .into_owned()
    .into()
}

/// A source's state at a glance: a fetched manifest is current, a cached or
/// bundled one may be stale, a source without one offers nothing, and one
/// not checked yet (or while not hosting) has no state.
fn chip_style(state: Option<TraverseManifestState>, cx: &App) -> (SharedString, gpui::Hsla) {
    let (label, tone) = match state {
        Some(TraverseManifestState::Live) => {
            (crate::tr!("remote.traverse.chip.live"), cx.theme().success)
        }
        Some(TraverseManifestState::Cached) => (
            crate::tr!("remote.traverse.chip.cached"),
            cx.theme().warning,
        ),
        Some(TraverseManifestState::Bundled) => (
            crate::tr!("remote.traverse.chip.bundled"),
            cx.theme().warning,
        ),
        Some(TraverseManifestState::Failed) => {
            (crate::tr!("remote.traverse.chip.failed"), cx.theme().danger)
        }
        None => (
            crate::tr!("remote.traverse.chip.unchecked"),
            cx.theme().muted_foreground,
        ),
    };
    (label.into_owned().into(), tone)
}

/// The status of an enabled source's row: its state chip, and with a status
/// to show, the disclosure that opens every relay and lookup under the row.
fn status_chip(
    index: usize,
    status: Option<&TraverseSourceStatus>,
    expanded: bool,
    cx: &mut Context<TraversePanel>,
) -> AnyElement {
    let (label, tone) = chip_style(status.map(|status| status.manifest.state), cx);
    let chip =
        crate::material::semantic_chip(label.clone(), tone.opacity(0.15), tone, cx).flex_none();
    if status.is_none() {
        return chip.into_any_element();
    }
    crate::material::accessible_clickable(
        h_flex(),
        SharedString::from(format!("remote-traverse-status-{index}")),
        gpui::Role::Button,
        label,
        cx,
    )
    .aria_expanded(expanded)
    .debug_selector(move || format!("remote-traverse-status-{index}"))
    .flex_none()
    .gap_1()
    .items_center()
    .cursor_pointer()
    .text_color(cx.theme().muted_foreground)
    .child(chip)
    .child(Icon::new(if expanded {
        IconName::ChevronUp
    } else {
        IconName::ChevronDown
    }))
    .on_click(cx.listener(move |this, _, _, cx| {
        // The official source's row is its switch; expanding the status
        // must not also flip it.
        cx.stop_propagation();
        this.toggle_detail(index, cx);
    }))
    .into_any_element()
}

/// The region the manifest gives a relay, else what tells its URL apart.
fn relay_name(relay: &TraverseRelayStatus) -> String {
    relay
        .region
        .clone()
        .or_else(|| super::relay_region(&relay.url))
        .unwrap_or_else(|| relay.url.clone())
}

/// Every relay with its latency, and every lookup with its last check.
fn status_detail(status: &TraverseSourceStatus, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let danger = cx.theme().danger;
    let heading = |text: SharedString| {
        div()
            .pt(cx.theme().tokens.spacing.xs)
            .text_caption(cx)
            .child(text)
    };
    let line = |name: String, value: AnyElement| {
        h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .justify_between()
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(name)),
            )
            .child(value)
    };
    let error = |text: &str| div().w_full().text_color(danger).child(text.to_owned());
    let manifest = &status.manifest;
    let mut manifest_value = vec![manifest_word(manifest.state).to_string()];
    if let Some(fetched) = manifest.fetched_unix {
        manifest_value.push(
            crate::tr!(
                "remote.traverse.status.fetched",
                time = crate::time::humanize_ago(crate::time::now_secs().saturating_sub(fetched))
            )
            .into_owned(),
        );
    }
    let mut column = crate::material::card(cx)
        .w_full()
        .text_token(&cx.theme().tokens.typography.xs)
        .child(line(
            crate::tr!("remote.traverse.status.manifest").into_owned(),
            div()
                .flex_none()
                .text_color(muted)
                .child(manifest_value.join(" · "))
                .into_any_element(),
        ))
        .children(manifest.error.as_deref().map(error));
    if !status.relays.is_empty() {
        column = column.child(heading(
            crate::tr!("remote.traverse.status.relays_title")
                .into_owned()
                .into(),
        ));
    }
    for relay in &status.relays {
        let latency = match relay.latency_ms {
            Some(ms) => crate::tr!("remote.traverse.status.latency", ms = ms),
            None => crate::tr!("remote.traverse.status.unmeasured"),
        };
        column = column
            .child(line(
                relay_name(relay),
                h_flex()
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .when(relay.home, |value| {
                        value.child(crate::material::semantic_chip(
                            crate::tr!("remote.traverse.status.home").into_owned(),
                            cx.theme().success.opacity(0.15),
                            cx.theme().success,
                            cx,
                        ))
                    })
                    .child(div().text_color(muted).child(latency.into_owned()))
                    .into_any_element(),
            ))
            .children(relay.error.as_deref().map(error));
    }
    if !status.lookups.is_empty() {
        column = column.child(heading(
            crate::tr!("remote.traverse.status.lookups_title")
                .into_owned()
                .into(),
        ));
    }
    for lookup in &status.lookups {
        let name = url::Url::parse(&lookup.url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| lookup.url.clone());
        let (value, color) = match lookup.state {
            TraverseLookupState::Ok => (crate::tr!("remote.traverse.status.ok"), muted),
            TraverseLookupState::Pending => (crate::tr!("remote.traverse.status.pending"), muted),
            TraverseLookupState::Failed => (crate::tr!("remote.traverse.status.failed"), danger),
        };
        column = column
            .child(line(
                name,
                div()
                    .flex_none()
                    .text_color(color)
                    .child(value.into_owned())
                    .into_any_element(),
            ))
            .children(lookup.error.as_deref().map(error));
    }
    column.into_any_element()
}

impl Render for TraversePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let rows = self.render_rows(compact, cx);
        v_flex()
            .w_full()
            .min_w_0()
            .debug_selector(|| "traverse-settings".into())
            .child(group_caption(crate::tr!("remote.traverse.section"), cx))
            .child(grouped(rows, cx))
            .child(footnote(
                crate::tr!("remote.traverse.description"),
                compact,
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use tcode_client::HostLink;
    use tcode_core::settings::Settings;
    use tcode_traverse::HostMux;

    /// A source's switch, addition or removal takes effect when it is made:
    /// it is saved at once, and a self-hosted instance joins only once its
    /// URL is usable and not already listed.
    #[gpui::test]
    fn traverse_edits_apply_themselves(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("traverse-edits");
        std::fs::create_dir_all(&root).unwrap();
        // Idle pipes: the host is never attached to here.
        let (to_host, _host_rx) = async_channel::unbounded::<String>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let (attachments, _attachment_rx) = async_channel::unbounded();
        cx.update(crate::theme::init);
        cx.update(|cx| {
            cx.set_global(RemoteController::new(
                HostMux::new(attachments, from_host.clone()),
                root.clone(),
                HostLink::new(to_host, from_host),
                Settings::default(),
            ))
        });
        let (panel, cx) = cx.add_window_view(TraversePanel::new);
        let saved = |cx: &mut gpui::VisualTestContext| {
            cx.read(|cx| {
                serde_json::to_value(&cx.global::<RemoteController>().local_settings().traverse)
                    .unwrap()
            })
        };
        panel.update_in(cx, |panel, window, cx| {
            panel.set_enabled(0, false, window, cx)
        });
        assert_eq!(
            saved(cx),
            serde_json::json!({"sources": [{"kind": "official", "enabled": false}]})
        );
        for typed in [
            "traverse.example",
            "https://traverse.example",
            "https://traverse.example/",
        ] {
            panel.update_in(cx, |panel, window, cx| {
                panel
                    .add_input
                    .update(cx, |input, cx| input.set_value(typed, window, cx));
                panel.add(window, cx);
            });
        }
        assert_eq!(
            saved(cx),
            serde_json::json!({"sources": [
                {"kind": "official", "enabled": false},
                {"kind": "custom", "url": "https://traverse.example/", "enabled": true},
            ]})
        );
        panel.update_in(cx, |panel, window, cx| {
            panel.set_enabled(0, true, window, cx);
            panel.remove(1, window, cx);
        });
        assert_eq!(
            saved(cx),
            serde_json::json!({"sources": [{"kind": "official", "enabled": true}]})
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
