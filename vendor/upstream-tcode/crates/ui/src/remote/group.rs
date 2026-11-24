//! Settings → Machine group: machines that admit the devices any
//! member admits, so a device paired with one reaches all of them. The
//! state and its changes go through [`Spaces`], the machine's hosting-state
//! owner.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
};
use gpui_base::v_flex;
use tcode_protocol::{DeviceAccess, GroupAction, GroupInfo, GroupMachineInfo, HostingState};

use super::spaces::{Resolve, Spaces, SpacesObserver};
use super::{Submit, report, submit_actions};
use crate::icon::{Icon, IconName};
use crate::material::{empty_row, group_row, notice, section_gap};
use crate::overlay::{Notification, OverlayExt as _};
use crate::settings_rows::{footnote, row_frame, settings_group};
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputState};

pub(crate) struct GroupSection {
    store: Entity<WorkspaceStore>,
    resolve: Resolve,
    observer: SpacesObserver,
}

impl GroupSection {
    /// `resolve` names the machine, as for [`super::spaces::SpacesSection`].
    pub(crate) fn new(store: Entity<WorkspaceStore>, resolve: Resolve) -> Self {
        Self {
            store,
            resolve,
            observer: SpacesObserver::default(),
        }
    }

    fn spaces(&self, cx: &mut App) -> Option<Entity<Spaces>> {
        (self.resolve)(&self.store, cx)
    }

    fn act(&mut self, action: GroupAction, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spaces) = self.spaces(cx) {
            let task = spaces.update(cx, |spaces, cx| spaces.act_group(action, cx));
            report("group.failed", task, window, cx);
        }
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spaces) = self.spaces(cx) {
            open_create(spaces, window, cx);
        }
    }

    fn join(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spaces) = self.spaces(cx) {
            open_join(spaces, window, cx);
        }
    }

    fn remove_machine(
        &mut self,
        machine: &GroupMachineInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(spaces) = self.spaces(cx) else {
            return;
        };
        let id = machine.id.clone();
        super::confirm_destructive(
            crate::tr!("group.machines.remove_title", name = machine.name.clone()).into_owned(),
            crate::tr!("group.machines.remove_description").into_owned(),
            crate::tr!("group.machines.remove").into_owned(),
            move |window, cx| {
                let task = spaces.update(cx, |spaces, cx| {
                    spaces.act_group(GroupAction::RemoveMachine(id.clone()), cx)
                });
                report("group.failed", task, window, cx);
            },
            window,
            cx,
        );
    }

    fn leave(&mut self, group: &GroupInfo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spaces) = self.spaces(cx) else {
            return;
        };
        super::confirm_destructive(
            crate::tr!("group.leave_title", name = group.name.clone()).into_owned(),
            crate::tr!("group.leave_description").into_owned(),
            crate::tr!("group.leave").into_owned(),
            move |window, cx| {
                let task = spaces.update(cx, |spaces, cx| spaces.act_group(GroupAction::Leave, cx));
                report("group.failed", task, window, cx);
            },
            window,
            cx,
        );
    }

    /// Not in a group: what one risks, the two ways in, and what one is.
    fn intro(&self, compact: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let risk = notice(cx.theme().warning, cx)
            .text_body(compact, cx)
            .debug_selector(|| "group-risk".into())
            .child(Icon::new(IconName::TriangleAlert).text_color(cx.theme().warning_foreground))
            .child(div().flex_1().min_w_0().child(crate::tr!("group.risk")))
            .into_any_element();
        let actions = group_row(cx)
            .flex_wrap()
            .gap_2()
            .child(
                Button::new("group-create")
                    .debug_selector(|| "group-create".into())
                    .ghost()
                    .outline()
                    .compact()
                    .icon(IconName::Plus)
                    .label(crate::tr!("group.create.action"))
                    .on_click(cx.listener(|section, _, window, cx| section.create(window, cx))),
            )
            .child(
                Button::new("group-join")
                    .debug_selector(|| "group-join".into())
                    .ghost()
                    .outline()
                    .compact()
                    .label(crate::tr!("group.join.action"))
                    .on_click(cx.listener(|section, _, window, cx| section.join(window, cx))),
            )
            .into_any_element();
        vec![
            risk,
            settings_group(crate::tr!("group.section"), vec![actions], cx)
                .child(super::footnote_with_docs(
                    crate::tr!("group.intro"),
                    "machine-groups",
                    compact,
                    cx,
                ))
                .into_any_element(),
        ]
    }

    fn machines_group(
        &self,
        group: &GroupInfo,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows = Vec::new();
        for machine in &group.machines {
            let remove = machine.clone();
            rows.push(
                group_row(cx)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_body(compact, cx)
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(machine.name.clone())),
                    )
                    .when(machine.this_machine, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_secondary(compact, cx)
                                .child(crate::tr!("group.machines.this")),
                        )
                    })
                    .when(!machine.this_machine, |row| {
                        row.child(
                            Button::new(SharedString::from(format!(
                                "group-machine-remove-{}",
                                machine.id
                            )))
                            .ghost()
                            .compact()
                            .danger()
                            .label(crate::tr!("group.machines.remove"))
                            .on_click(cx.listener(
                                move |section, _, window, cx| {
                                    section.remove_machine(&remove, window, cx);
                                },
                            )),
                        )
                    })
                    .into_any_element(),
            );
        }
        settings_group(crate::tr!("group.machines.section"), rows, cx).into_any_element()
    }

    fn declined_group(
        &self,
        group: &GroupInfo,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if group.declined.is_empty() {
            return None;
        }
        let mut rows = Vec::new();
        for device in &group.declined {
            let id = device.id.clone();
            rows.push(
                row_frame(compact, cx)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_body(compact, cx)
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(super::device_label(
                                &device.name,
                                device.platform.as_deref(),
                            ))),
                    )
                    .child(
                        Button::new(SharedString::from(format!("group-admit-{id}")))
                            .ghost()
                            .outline()
                            .compact()
                            .label(crate::tr!("group.declined.admit"))
                            .on_click(cx.listener(move |section, _, window, cx| {
                                section.act(GroupAction::Admit(id.clone()), window, cx);
                            })),
                    )
                    .into_any_element(),
            );
        }
        Some(
            settings_group(crate::tr!("group.declined.section"), rows, cx)
                .child(footnote(
                    crate::tr!("group.declined.description"),
                    compact,
                    cx,
                ))
                .into_any_element(),
        )
    }

    /// In a group: its name and size, its machines, the devices waiting for
    /// this machine, and the way out.
    fn detail(&self, group: &GroupInfo, compact: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let count = if group.machines.len() == 1 {
            crate::tr!("group.summary.machine_one")
        } else {
            crate::tr!("group.summary.machines", count = group.machines.len())
        };
        let name = row_frame(compact, cx)
            .debug_selector(|| "group-detail".into())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_body(compact, cx)
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(group.name.clone())),
            )
            .child(div().flex_none().text_secondary(compact, cx).child(count))
            .into_any_element();
        let leave = group.clone();
        let leave_row = crate::material::accessible_clickable(
            group_row(cx),
            "group-leave",
            gpui::Role::Button,
            crate::tr!("group.leave"),
            cx,
        )
        .debug_selector(|| "group-leave".into())
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().list_hover))
        .text_body(compact, cx)
        .text_color(cx.theme().danger_foreground)
        .child(crate::tr!("group.leave"))
        .on_click(cx.listener(move |section, _, window, cx| {
            section.leave(&leave, window, cx);
        }))
        .into_any_element();
        let mut groups = vec![
            settings_group(crate::tr!("group.section"), vec![name], cx)
                .child(super::footnote_with_docs(
                    crate::tr!("group.detail_note"),
                    "machine-groups",
                    compact,
                    cx,
                ))
                .into_any_element(),
            self.machines_group(group, compact, cx),
        ];
        groups.extend(self.declined_group(group, compact, cx));
        groups.push(
            crate::material::group(cx)
                .child(leave_row)
                .into_any_element(),
        );
        groups
    }
}

impl gpui::Render for GroupSection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let spaces = self.spaces(cx);
        self.observer.watch(spaces.as_ref(), cx);
        let body = match spaces.map(|spaces| spaces.read(cx).group().cloned()) {
            None => vec![
                settings_group(
                    crate::tr!("group.section"),
                    vec![empty_row(crate::tr!("remote.hosting_off"), cx).into_any_element()],
                    cx,
                )
                .into_any_element(),
            ],
            Some(Some(group)) => self.detail(&group, compact, cx),
            Some(None) => self.intro(compact, cx),
        };
        v_flex()
            .w_full()
            .gap(section_gap(cx))
            .debug_selector(|| "group-section".into())
            .children(body)
    }
}

/// What creating or joining brings: every full-access device paired with
/// this machine. Browsers logged in to a headless machine are listed among
/// its devices under a UUID; they sign in with the machine's password and
/// never belong to a group.
fn brought(spaces: &Entity<Spaces>, cx: &App) -> Vec<String> {
    spaces
        .read(cx)
        .devices()
        .iter()
        .filter(|device| {
            device.access == DeviceAccess::Full && tcode_client::pairing::valid_host_id(&device.id)
        })
        .map(|device| super::device_label(&device.name, device.platform.as_deref()))
        .collect()
}

/// The warning every create and join carries: the devices this machine
/// brings, which every member admits once confirmed.
fn brought_warning(devices: &[String], compact: bool, cx: &App) -> AnyElement {
    notice(cx.theme().warning, cx)
        .flex_col()
        .items_start()
        .text_body(compact, cx)
        .debug_selector(|| "group-brought".into())
        .child(
            div()
                .text_caption(cx)
                .text_color(cx.theme().warning_foreground)
                .child(if devices.is_empty() {
                    crate::tr!("group.brought.none")
                } else {
                    crate::tr!("group.brought.title")
                }),
        )
        .children(
            devices
                .iter()
                .map(|label| div().child(format!("• {label}"))),
        )
        .into_any_element()
}

fn open_create(spaces: Entity<Spaces>, window: &mut Window, cx: &mut App) {
    let name = cx
        .new(|cx| InputState::new(window, cx).placeholder(crate::tr!("group.create.placeholder")));
    let brought = brought(&spaces, cx);
    let submit: Submit = Rc::new({
        let name = name.clone();
        move |window, cx| {
            let typed = name.read(cx).value().trim().to_owned();
            if typed.is_empty() {
                return false;
            }
            let task = spaces.update(cx, |spaces, cx| {
                spaces.act_group(GroupAction::Create { name: typed }, cx)
            });
            report("group.failed", task, window, cx);
            true
        }
    });
    let field = name.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let field = field.clone();
        let brought = brought.clone();
        submit_actions(
            dialog
                .title(crate::tr!("group.create.title").into_owned())
                .content(move |content, window, cx| {
                    let compact = crate::window_seam::window_is_compact(window, cx);
                    content.child(
                        v_flex()
                            .gap_3()
                            .debug_selector(|| "group-create-dialog".into())
                            .child(Input::new(&field))
                            .child(brought_warning(&brought, compact, cx)),
                    )
                }),
            "group-create",
            crate::tr!("group.create.confirm").into_owned().into(),
            submit.clone(),
        )
    });
    name.update(cx, |name, cx| name.focus(window, cx));
}

fn open_join(spaces: Entity<Spaces>, window: &mut Window, cx: &mut App) {
    let link =
        cx.new(|cx| InputState::new(window, cx).placeholder(crate::tr!("group.join.placeholder")));
    let address = cx.new(|cx| {
        InputState::new(window, cx).placeholder(crate::tr!("group.join.address_placeholder"))
    });
    let brought = brought(&spaces, cx);
    let submit: Submit = Rc::new({
        let link = link.clone();
        let address = address.clone();
        move |window, cx| {
            let typed = link.read(cx).value().trim().to_owned();
            if typed.is_empty() {
                return false;
            }
            if !tcode_client::pairing::parse_pair_url(&typed)
                .is_some_and(|invite| invite.group.is_some())
            {
                window.push_notification(
                    Notification::error(crate::tr!("group.join.not_a_group_link").into_owned()),
                    cx,
                );
                return false;
            }
            let address = address.read(cx).value().trim().to_owned();
            let action = GroupAction::Join {
                link: typed,
                address: (!address.is_empty()).then_some(address),
            };
            let task = spaces.update(cx, |spaces, cx| spaces.act_group(action, cx));
            window
                .spawn(cx, async move |cx| {
                    let result: Result<HostingState, String> = task.await;
                    let _ = cx.update(|window, cx| {
                        let notification = match result {
                            Ok(state) => Notification::info(
                                crate::tr!(
                                    "group.join.joined",
                                    name = state.group.map(|group| group.name).unwrap_or_default()
                                )
                                .into_owned(),
                            ),
                            Err(error) => Notification::error(
                                crate::tr!("group.failed", error = error).into_owned(),
                            ),
                        };
                        window.push_notification(notification, cx);
                    });
                })
                .detach();
            true
        }
    });
    let (link_field, address_field) = (link.clone(), address.clone());
    window.open_dialog(cx, move |dialog, _, _| {
        let link_field = link_field.clone();
        let address_field = address_field.clone();
        let brought = brought.clone();
        submit_actions(
            dialog
                .title(crate::tr!("group.join.title").into_owned())
                .content(move |content, window, cx| {
                    let compact = crate::window_seam::window_is_compact(window, cx);
                    content.child(
                        v_flex()
                            .gap_3()
                            .debug_selector(|| "group-join-dialog".into())
                            .child(
                                div()
                                    .text_secondary(compact, cx)
                                    .child(crate::tr!("group.join.description")),
                            )
                            .child(Input::new(&link_field))
                            .child(
                                div()
                                    .text_caption(cx)
                                    .child(crate::tr!("group.join.address")),
                            )
                            .child(Input::new(&address_field))
                            .child(brought_warning(&brought, compact, cx)),
                    )
                }),
            "group-join",
            crate::tr!("group.join.confirm").into_owned().into(),
            submit.clone(),
        )
    });
    link.update(cx, |link, cx| link.focus(window, cx));
}

/// Removing a group member removes it from every machine of the group, so
/// the device lists ask first.
pub(crate) fn confirm_group_removal(
    device_name: String,
    group_name: String,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    super::confirm_destructive(
        crate::tr!("group.remove_device.title", name = device_name).into_owned(),
        crate::tr!("group.remove_device.description", group = group_name).into_owned(),
        crate::tr!("group.remove_device.action").into_owned(),
        on_ok,
        window,
        cx,
    );
}

/// Under a machine's invitation while the machine is in a group: pairing
/// with it is joining the group, for a device and for another machine.
pub(crate) fn invitation_note(group: &str, compact: bool, cx: &App) -> AnyElement {
    div()
        .text_secondary(compact, cx)
        .debug_selector(|| "group-invitation-note".into())
        .child(crate::tr!("group.invitation_note", name = group.to_owned()).into_owned())
        .into_any_element()
}
