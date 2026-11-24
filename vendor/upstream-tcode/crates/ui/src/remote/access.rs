//! Who can reach a machine: the invitation a new device pairs with and the
//! devices that have paired. Settings → Remote shows both for the machine it
//! manages, this one or the one a browser serves, through [`Spaces`]; the
//! Machines page shows this machine's invitation with [`invitation_card`].

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Task, Window, div, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_protocol::{DeviceAccess, HostingAction};

use super::qr::beside_qr;
use super::report;
use super::spaces::{Resolve, Spaces, SpacesObserver};
use crate::material::{empty_row, group_row};
use crate::overlay::{Notification, OverlayExt as _};
use crate::settings_rows::{ToggleRow, row_frame, row_labels, settings_group};
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

/// Mint a new invitation, replacing the current one.
pub(super) type NewInvitation = Rc<dyn Fn(&mut Window, &mut App)>;

fn new_invitation_button(on_new: NewInvitation) -> Button {
    Button::new("remote-new-invitation")
        .compact()
        .label(crate::tr!("remote.invite.new"))
        .on_click(move |_, window, cx| on_new(window, cx))
}

/// The invitation a machine shows other devices: the QR to scan, the link to
/// copy, how long it lasts and the way to a fresh one.
pub(super) fn invitation_card(
    link: &str,
    remaining: u64,
    group: Option<&str>,
    compact: bool,
    on_new: NewInvitation,
    cx: &App,
) -> AnyElement {
    let copied = link.to_owned();
    let text = v_flex()
        .flex_1()
        .min_w_0()
        .gap_3()
        .child(
            div()
                .text_body(compact, cx)
                .child(crate::tr!("hosts.invite.description")),
        )
        .children(group.map(|name| super::group::invitation_note(name, compact, cx)))
        .child(div().text_secondary(compact, cx).child(crate::tr!(
            "remote.invite.expires",
            time = crate::time::format_countdown(remaining)
        )))
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(
                    Button::new("remote-copy-invitation")
                        .ghost()
                        .outline()
                        .compact()
                        .label(crate::tr!("remote.invite.copy"))
                        .on_click(move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                            window.push_notification(
                                Notification::info(crate::tr!("remote.invite.copied").into_owned()),
                                cx,
                            );
                        }),
                )
                .child(new_invitation_button(on_new).ghost().outline()),
        );
    beside_qr(text, link, compact, cx)
        .p(px(crate::material::CARD_INSET))
        .debug_selector(|| "remote-invitation".into())
        .into_any_element()
}

/// The invitation ran out: say so, next to the button that mints another.
pub(super) fn expired_invitation_row(compact: bool, on_new: NewInvitation, cx: &App) -> AnyElement {
    group_row(cx)
        .debug_selector(|| "remote-invitation-expired".into())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_body(compact, cx)
                .child(crate::tr!("remote.invite.expired")),
        )
        .child(new_invitation_button(on_new).primary())
        .into_any_element()
}

/// Settings → Remote below the machine's own switches: its invitation while
/// it accepts new devices, and the devices paired with it. A client with no
/// hosting switch of its own (a browser) also gets the pairing switch here.
pub(crate) struct AccessSection {
    store: Entity<WorkspaceStore>,
    resolve: Resolve,
    pairing_switch: bool,
    observer: SpacesObserver,
    /// Repaints the invitation's countdown while there is one.
    ticker: Option<Task<()>>,
}

impl AccessSection {
    /// `resolve` names the machine, as for [`super::spaces::SpacesSection`].
    pub(crate) fn new(
        store: Entity<WorkspaceStore>,
        resolve: Resolve,
        pairing_switch: bool,
    ) -> Self {
        Self {
            store,
            resolve,
            pairing_switch,
            observer: SpacesObserver::default(),
            ticker: None,
        }
    }

    fn sync_ticker(&mut self, live: bool, cx: &mut Context<Self>) {
        match (live, self.ticker.is_some()) {
            (true, false) => {
                self.ticker = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(1))
                            .await;
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

    fn invitation(
        &mut self,
        spaces: &Entity<Spaces>,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let model = spaces.read(cx);
        let invitation = model.invitation();
        let group = model.group().map(|group| group.name.clone());
        let on_new: NewInvitation = Rc::new({
            let spaces = spaces.clone();
            move |window, cx| {
                let task = spaces.update(cx, |spaces, cx| {
                    spaces.act_hosting(HostingAction::NewInvitation, cx)
                });
                report("remote.failed", task, window, cx);
            }
        });
        self.sync_ticker(invitation.is_some(), cx);
        let row = match invitation {
            Some((link, remaining)) => {
                invitation_card(&link, remaining, group.as_deref(), compact, on_new, cx)
            }
            None => expired_invitation_row(compact, on_new, cx),
        };
        settings_group(crate::tr!("remote.invite.title"), vec![row], cx).into_any_element()
    }

    fn devices(
        &self,
        spaces: &Entity<Spaces>,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let model = spaces.read(cx);
        let space_name = |space_id: &str| {
            model
                .spaces()
                .iter()
                .find(|space| space.id == space_id)
                .map(|space| space.name.clone())
        };
        // In a group every full-access device is a member, so removing one
        // removes it from every machine of the group. A browser is listed
        // under a UUID and never is one.
        let group = model.group().map(|group| group.name.clone());
        let mut rows = Vec::new();
        for device in model.devices() {
            let space = match &device.access {
                DeviceAccess::Space { space_id } => space_name(space_id),
                DeviceAccess::Full => None,
            };
            let in_group = group.clone().filter(|_| {
                device.access == DeviceAccess::Full
                    && tcode_client::pairing::valid_host_id(&device.id)
            });
            let status_color = match &device.path {
                Some(_) => cx.theme().success,
                None => cx.theme().muted_foreground,
            };
            let spaces = spaces.clone();
            let id = device.id.clone();
            let name = device.name.clone();
            let revoke = move |window: &mut Window, cx: &mut App| {
                let task = spaces.update(cx, |spaces, cx| {
                    spaces.act_hosting(HostingAction::RevokeDevice(id.clone()), cx)
                });
                report("remote.devices.revoke_failed", task, window, cx);
            };
            rows.push(
                row_frame(compact, cx)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(row_labels(
                                super::device_label(&device.name, device.platform.as_deref()),
                                None,
                                None,
                                compact,
                                cx,
                            ))
                            .children(space.map(|name| super::spaces::member_badge(&name, cx))),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .when(compact, |controls| controls.w_full().justify_between())
                            .child(
                                div()
                                    .text_secondary(compact, cx)
                                    .text_color(status_color)
                                    .child(super::path_label(device.path.as_ref())),
                            )
                            .child(
                                Button::new(SharedString::from(format!("revoke-{}", device.id)))
                                    .ghost()
                                    .compact()
                                    .danger()
                                    .label(crate::tr!("remote.devices.revoke"))
                                    .on_click(move |_, window, cx| match in_group.clone() {
                                        Some(group) => super::group::confirm_group_removal(
                                            name.clone(),
                                            group,
                                            revoke.clone(),
                                            window,
                                            cx,
                                        ),
                                        None => super::confirm_destructive(
                                            crate::tr!(
                                                "remote.devices.revoke_title",
                                                name = name.clone()
                                            )
                                            .into_owned(),
                                            crate::tr!("remote.devices.revoke_description")
                                                .into_owned(),
                                            crate::tr!("remote.devices.revoke").into_owned(),
                                            revoke.clone(),
                                            window,
                                            cx,
                                        ),
                                    }),
                            ),
                    )
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(empty_row(crate::tr!("remote.devices.empty"), cx).into_any_element());
        }
        settings_group(crate::tr!("remote.devices.section"), rows, cx).into_any_element()
    }
}

impl gpui::Render for AccessSection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let spaces = (self.resolve)(&self.store, cx);
        self.observer.watch(spaces.as_ref(), cx);
        let Some(spaces) = spaces.filter(|spaces| spaces.read(cx).loaded()) else {
            self.sync_ticker(false, cx);
            return div().into_any_element();
        };
        let accepting = spaces.read(cx).accepting();
        let mut column = v_flex()
            .w_full()
            .gap(crate::material::section_gap(cx))
            .debug_selector(|| "remote-access".into());
        if self.pairing_switch {
            let switch = spaces.clone();
            column = column.child(settings_group(
                crate::tr!("remote.host.section"),
                vec![
                    ToggleRow::new(
                        "headless-pairing-enabled",
                        crate::tr!("remote.pairing.title"),
                        accepting,
                        compact,
                        move |enabled: &bool, window, cx| {
                            let task = switch.update(cx, |spaces, cx| {
                                spaces.act_hosting(HostingAction::SetEnabled(*enabled), cx)
                            });
                            report("remote.failed", task, window, cx);
                        },
                    )
                    .into_any_element(),
                ],
                cx,
            ));
        }
        if accepting {
            column = column.child(self.invitation(&spaces, compact, cx));
        } else {
            self.sync_ticker(false, cx);
        }
        column
            .child(self.devices(&spaces, compact, cx))
            .into_any_element()
    }
}
