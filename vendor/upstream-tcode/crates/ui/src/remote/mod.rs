//! Hosts: which host this window talks to, and which devices may talk to it.
//!
//! This is a product surface, not a settings page. It is where connections
//! are made, in both directions: the invitation this machine shows other
//! devices, the saved machines this window can open, pairing by a scanned or
//! pasted invitation and authentication repair. It is reached from the
//! sidebar's feature area at every width. It needs `tcode_client` and the
//! attachment owner's switch action, and nothing else: it compiles on every
//! client, including `--no-default-features`.
//!
//! **Hosting** — whether the endpoint runs, its name, Traverse, whether new
//! devices are accepted, and the devices that have paired — is a genuine
//! setting of *this machine* and lives in `hosting`, behind `remote-hosting`,
//! inside Settings → Remote. The browser uses `hosted` to control its headless
//! listener over the authenticated pipe.

use std::rc::Rc;

use gpui::{
    Action, AnyElement, App, Context, Entity, Global, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{InteractiveElementExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_client::host::ClientHost;
use tcode_client::pairing::PairedHost;

use crate::icon::{Icon, IconName};
use crate::material::{
    CARD_INSET, COMPACT_PAGE_INSET, group_row, group_row_stacked, panel_row, section_gap,
};
use crate::overlay::{Notification, OverlayExt as _};
use crate::pairing::PairForm;
use crate::settings_rows::{CONTENT_MAX_WIDTH, settings_group};
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent};
use crate::widgets::menu::DropdownMenu as _;
use crate::window_state::{Destination, WindowState};

#[cfg(feature = "remote-hosting")]
mod hosting;
#[cfg(feature = "remote-hosting")]
mod traverse;

#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
mod access;
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
mod group;
mod qr;
pub(crate) mod spaces;

#[cfg(target_family = "wasm")]
pub(crate) use access::AccessSection;
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
pub(crate) use group::GroupSection;
#[cfg(feature = "remote-hosting")]
pub use hosting::{HostingPanel, RemoteController, machine_name};
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
pub(crate) use spaces::SpacesSection;
#[cfg(feature = "remote-hosting")]
pub use traverse::TraversePanel;

/// Where this window's workspace comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentTarget {
    Local,
    Remote(Box<PairedHost>),
}

pub type SwitchAttachment = Rc<dyn Fn(AttachmentTarget, &mut Window, &mut App)>;

/// Leave the host this row names, keeping the saved record.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_hosts, no_json)]
struct DisconnectHost;

/// Forget the saved record. A live attachment to it keeps running: the record
/// is a credential, not the connection.
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_hosts, no_json)]
struct ForgetHost(String);

/// The client's own identity and the action that re-points this window at a
/// different host.
///
/// It is deliberately separate from hosting: a client that can never host still
/// needs saved hosts, a device name and a way to switch attachment.
pub struct ClientAttachment {
    host: Rc<dyn ClientHost>,
    local: bool,
    switch: SwitchAttachment,
}

impl Global for ClientAttachment {}

impl ClientAttachment {
    /// `local` is whether bootstrap gave this window a host inside its own
    /// process. A phone or browser has none, so "back to local" is not a thing
    /// it can be offered.
    pub fn new(
        host: Rc<dyn ClientHost>,
        local: bool,
        switch: impl Fn(AttachmentTarget, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            host,
            local,
            switch: Rc::new(switch),
        }
    }

    /// Whether [`AttachmentTarget::Local`] is reachable from this window.
    pub fn can_attach_local(&self) -> bool {
        self.local
    }

    pub fn host(&self) -> Rc<dyn ClientHost> {
        self.host.clone()
    }

    pub fn switcher(&self) -> SwitchAttachment {
        self.switch.clone()
    }

    pub fn hosts(&self) -> Vec<PairedHost> {
        self.host.load_hosts()
    }

    pub fn save_host(&self, host: PairedHost) {
        self.host.remember_host(host);
    }

    pub fn remove_host(&self, host_id: &str) {
        self.host.remove_host(host_id);
    }
}

/// The name of the machine this process hosts, where it hosts one.
pub(crate) fn local_machine_name(cx: &App) -> Option<String> {
    #[cfg(feature = "remote-hosting")]
    {
        cx.try_global::<RemoteController>()
            .map(RemoteController::machine_name)
    }
    #[cfg(not(feature = "remote-hosting"))]
    {
        let _ = cx;
        None
    }
}

/// Open a *client-local* path in the user's editor through whatever integration
/// this client was given. `None` when it has none.
pub(crate) fn open_in_editor(path: &std::path::Path, cx: &App) -> Option<Result<(), String>> {
    cx.try_global::<ClientAttachment>()?
        .host
        .open_in_editor(path)
}

/// Tell the user why a change did not happen; a change that did shows itself.
/// `failed` is the message key, given the error as `error`.
fn report<T: 'static>(
    failed: &'static str,
    task: gpui::Task<Result<T, String>>,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            if let Err(error) = task.await {
                let _ = cx.update(|window, cx| {
                    window.push_notification(
                        Notification::error(crate::tr!(failed, error = error).into_owned()),
                        cx,
                    );
                });
            }
        })
        .detach();
}

/// A page's [`footnote`](crate::settings_rows::footnote) and the link to
/// the `anchor` section of the remote guide, which says the rest.
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
fn footnote_with_docs(
    text: impl Into<SharedString>,
    anchor: &str,
    compact: bool,
    cx: &App,
) -> gpui::Div {
    crate::settings_rows::footnote(text, compact, cx)
        .flex()
        .flex_col()
        .items_start()
        .gap_1()
        .child(crate::settings_rows::external_link(
            format!("remote-docs-{anchor}"),
            crate::tr!("remote.learn_more").into_owned().into(),
            format!("https://github.com/Tryanks/tcode/blob/main/docs/remote.md#{anchor}"),
        ))
}

/// Ask before a change that cannot be undone from here. `title` names what
/// goes, `description` says in one sentence what follows, and `action` labels
/// the danger button that runs `on_ok`.
fn confirm_destructive(
    title: String,
    description: String,
    action: String,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let on_ok = Rc::new(on_ok);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_ok = on_ok.clone();
        alert
            .title(title.clone())
            .description(description.clone())
            .button_props(
                crate::overlay::DialogButtons::default()
                    .ok_variant(crate::widgets::ButtonVariant::Danger)
                    .ok_text(action.clone())
                    .cancel_text(crate::tr!("spaces.cancel"))
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                on_ok(window, cx);
                true
            })
    });
}

/// A dialog's confirmation; `false` keeps the dialog open.
type Submit = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

/// A form dialog's buttons: Cancel, and `confirm`, which runs `submit` as
/// Enter does and closes the dialog when it succeeds. `id` prefixes the
/// buttons' ids.
fn submit_actions(
    dialog: crate::overlay::Dialog,
    id: &'static str,
    confirm: SharedString,
    submit: Submit,
) -> crate::overlay::Dialog {
    let ok = submit.clone();
    dialog.on_ok(move |_, window, cx| ok(window, cx)).footer(
        crate::overlay::DialogActions::new()
            .child(
                Button::new(SharedString::from(format!("{id}-cancel")))
                    .label(crate::tr!("spaces.cancel"))
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new(SharedString::from(format!("{id}-ok")))
                    .debug_selector(move || format!("{id}-ok"))
                    .primary()
                    .label(confirm)
                    .on_click(move |_, window, cx| {
                        if submit(window, cx) {
                            window.close_dialog(cx);
                        }
                    }),
            ),
    )
}

type Row = gpui::Stateful<gpui::Div>;

/// A [`panel_row`] inside one of the page's groups. Compact, a panel row is a
/// page-inset list row; inside a group it takes the group's inset, so its text
/// lines up with the caption and the group's other rows.
fn group_entry(
    id: impl Into<gpui::ElementId>,
    label: SharedString,
    compact: bool,
    cx: &App,
) -> Row {
    panel_row(id, label, compact, cx).px(px(CARD_INSET))
}

pub struct RemotePanel {
    /// The window's current attachment, when it has one. Hosts is also the
    /// root of a window that has none.
    store: Option<Entity<WorkspaceStore>>,
    /// Why the host in this process did not start the last time this window
    /// tried to attach to it.
    local_failure: Option<String>,
    /// Navigation: "Paste an invitation link" pushes [`Destination::Pair`],
    /// which Back pops back to whatever asked for it.
    window_state: Entity<WindowState>,
    form: PairForm,
    /// Whether the window was on the paste page when it last changed pages.
    pairing: bool,
    /// [`ClientHost::fixed_machine`]: a browser lists its one machine and
    /// no way to add or re-pair one.
    fixed_machine: bool,
    hosts_scroll: ScrollHandle,
    pair_scroll: ScrollHandle,
    /// Repaints the invitation's countdown while this machine offers one.
    #[cfg(feature = "remote-hosting")]
    invitation_ticker: Option<gpui::Task<()>>,
    /// The space whose QR is open under this machine's invitation.
    #[cfg(feature = "remote-hosting")]
    space_qr: Option<String>,
    #[cfg(feature = "remote-hosting")]
    spaces_observer: spaces::SpacesObserver,
    _subscriptions: Vec<Subscription>,
}

impl RemotePanel {
    pub fn new(
        store: Option<Entity<WorkspaceStore>>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fixed_machine = cx
            .try_global::<ClientAttachment>()
            .is_some_and(|attachment| attachment.host.fixed_machine());
        let form = PairForm::new(window, cx);
        // A link that parses is the whole request, so it is sent as soon as
        // it lands in the field; Enter and the button only cover a retry.
        let subscriptions = vec![
            cx.observe_in(
                &window_state,
                window,
                |this: &mut Self, state, window, cx| {
                    let pairing = state.read(cx).destination() == Destination::Pair;
                    if std::mem::replace(&mut this.pairing, pairing) && !pairing {
                        this.form.leave(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &form.invitation,
                window,
                |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        this.form.error = None;
                        if this.form.should_submit(cx) {
                            this.submit(window, cx);
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter {
                        shift: false,
                        secondary: false,
                    } => this.submit(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &form.address,
                window,
                |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter {
                        shift: false,
                        secondary: false,
                    } => this.submit(window, cx),
                    _ => {}
                },
            ),
        ];
        Self {
            store,
            local_failure: None,
            window_state,
            form,
            pairing: false,
            fixed_machine,
            hosts_scroll: ScrollHandle::new(),
            pair_scroll: ScrollHandle::new(),
            #[cfg(feature = "remote-hosting")]
            invitation_ticker: None,
            #[cfg(feature = "remote-hosting")]
            space_qr: None,
            #[cfg(feature = "remote-hosting")]
            spaces_observer: spaces::SpacesObserver::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Follow the window onto another attachment, or off every attachment.
    pub fn set_store(&mut self, store: Option<Entity<WorkspaceStore>>, cx: &mut Context<Self>) {
        self.store = store;
        cx.notify();
    }

    pub(crate) fn set_local_failure(&mut self, reason: Option<String>, cx: &mut Context<Self>) {
        self.local_failure = reason;
        cx.notify();
    }

    pub(crate) fn set_pairing_error(&mut self, error: Option<String>) {
        self.form.error = error;
    }

    fn client(&self, cx: &App) -> Option<Rc<dyn ClientHost>> {
        cx.try_global::<ClientAttachment>()
            .map(ClientAttachment::host)
    }

    /// Read an invite off the camera and pair with it at once. The scanned
    /// link goes through the same parser a pasted one does; the page the
    /// user is on stays, and shows the attempt and its outcome.
    fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.client(cx) else {
            return;
        };
        self.form.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let scanned = host.scan_qr().await;
            let _ = this.update_in(cx, |panel, window, cx| {
                match scanned {
                    Ok(value) => {
                        if panel.form.fill_invite(&value, window, cx) {
                            panel.submit(window, cx);
                        } else {
                            panel.form.error =
                                Some(crate::tr!("hosts.pair.bad_invite").into_owned());
                        }
                    }
                    Err(error) => panel.form.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Pair with the invitation in the field and, once the machine has
    /// admitted this device, connect to it. One step: there is nothing to
    /// confirm that the link did not already say.
    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some((request, address, generation)) = self.form.begin_pair(cx) else {
            return;
        };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = client.pair(request, address).await;
            let _ = this.update_in(cx, |panel, window, cx| {
                panel.finish_pair(generation, result, window, cx);
            });
        })
        .detach();
    }

    fn finish_pair(
        &mut self,
        generation: u64,
        result: Result<PairedHost, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(host) = self.form.finish_pair(generation, result) {
            if let Some(message) = crate::pairing::joined_message(&host) {
                window.push_notification(Notification::success(message), cx);
            }
            let attachment = cx.global::<ClientAttachment>();
            attachment.save_host(host.clone());
            let switch = attachment.switcher();
            switch(AttachmentTarget::Remote(Box::new(host)), window, cx);
        }
        cx.notify();
    }

    /// Open the paste form with an empty field. An invitation is single use,
    /// so whatever the field held last time is spent.
    fn open_pair(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form.restart();
        self.form.clear(window, cx);
        self.window_state
            .update(cx, |state, cx| state.go(Destination::Pair, cx));
        cx.notify();
    }

    fn attached_host_id(&self, cx: &App) -> Option<String> {
        self.store
            .as_ref()?
            .read(cx)
            .remote_host_id()
            .map(str::to_owned)
    }

    fn attached_locally(&self, cx: &App) -> bool {
        self.store
            .as_ref()
            .is_some_and(|store| store.read(cx).remote_host_id().is_none())
    }

    /// The dot that says how this window's link to the attached machine is
    /// doing. Only a remote link has a state to show.
    fn status_glyph(&self, cx: &App) -> AnyElement {
        let color = self
            .store
            .as_ref()
            .map(|store| {
                cx.theme()
                    .connection_color(&store.read(cx).connection_state())
            })
            .unwrap_or(cx.theme().success);
        crate::material::status_dot(color).into_any_element()
    }

    /// The mark on the row this window is on when there is no link to
    /// report on: this machine itself.
    fn current_glyph(cx: &App) -> AnyElement {
        Icon::new(IconName::Check)
            .flex_none()
            .text_color(cx.theme().muted_foreground)
            .into_any_element()
    }

    /// This machine: one target among the saved machines, offered only where
    /// bootstrap actually gave this window a local host to attach to. Being
    /// on it is not a connection, so the row carries a check, not a dot. A
    /// host that did not start says why, and offers starting it again or
    /// quitting.
    fn local_row(&self, compact: bool, cx: &mut Context<Self>) -> Option<Row> {
        cx.try_global::<ClientAttachment>()
            .is_some_and(ClientAttachment::can_attach_local)
            .then(|| {
                let current = self.attached_locally(cx);
                let failure = self.local_failure.clone();
                let failed = failure.is_some();
                group_entry(
                    "hosts-local",
                    crate::tr!("hosts.this_computer").into_owned().into(),
                    compact,
                    cx,
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(cx.theme().tokens.spacing.xxs)
                        .child(
                            div().text_title(cx).overflow_hidden().child(
                                crate::widgets::fade_text(crate::tr!("hosts.this_computer")),
                            ),
                        )
                        .when_some(failure, |column, reason| {
                            column.child(
                                div()
                                    .debug_selector(|| "hosts-local-failure".into())
                                    .text_secondary(compact, cx)
                                    .text_color(cx.theme().danger_foreground)
                                    .child(
                                        crate::tr!("hosts.local.unavailable", reason = reason)
                                            .into_owned(),
                                    ),
                            )
                        }),
                )
                .when(current, |row| row.child(Self::current_glyph(cx)))
                .when(failed, |row| {
                    row.child(
                        h_flex()
                            .flex_none()
                            .gap_2()
                            .child(
                                Button::new("hosts-local-retry")
                                    .debug_selector(|| "hosts-local-retry".into())
                                    .primary()
                                    .compact()
                                    .label(crate::tr!("hosts.local.retry"))
                                    .on_click(|_, window, cx| {
                                        let switch = cx.global::<ClientAttachment>().switcher();
                                        switch(AttachmentTarget::Local, window, cx);
                                    }),
                            )
                            .child(
                                Button::new("hosts-local-quit")
                                    .compact()
                                    .label(crate::tr!("quit.confirm"))
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(crate::shell::Quit), cx);
                                    }),
                            ),
                    )
                })
                .when(!failed, |row| {
                    row.on_click(|_, window, cx| {
                        let switch = cx.global::<ClientAttachment>().switcher();
                        switch(AttachmentTarget::Local, window, cx);
                    })
                })
            })
    }

    /// Saved machines offer pairing again when authorization is rejected.
    fn host_row(
        &self,
        host: &PairedHost,
        current: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let reason = if current {
            self.store
                .as_ref()
                .and_then(|store| match store.read(cx).connection_state() {
                    tcode_client::ConnectionState::Offline { reason } => Some(reason),
                    tcode_client::ConnectionState::Reconnecting { reason, .. } => reason,
                    _ => None,
                })
        } else {
            None
        };
        let needs_pairing = reason == Some(tcode_client::ConnectionFailure::AuthenticationRejected)
            && !self.fixed_machine;
        let name = SharedString::from(host.name.clone());
        let connect_host = host.clone();
        let row = group_entry(
            SharedString::from(format!("host-{}", host.host_id)),
            name.clone(),
            compact,
            cx,
        )
        .debug_selector(|| format!("host-{}", host.host_id))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(cx.theme().tokens.spacing.xxs)
                .child(
                    div()
                        .text_title(cx)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(name.clone())),
                )
                .when_some(host.space_name.as_deref(), |column, space| {
                    column.child(div().text_secondary(compact, cx).overflow_hidden().child(
                        crate::widgets::fade_text(
                            crate::tr!("member.space", space = space).into_owned(),
                        ),
                    ))
                })
                .when(
                    host.space_name.is_none() && host.group_id.is_some(),
                    |column| {
                        column.child(
                            div()
                                .text_secondary(compact, cx)
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(crate::tr!("hosts.in_group"))),
                        )
                    },
                )
                .when_some(reason, |column, reason| {
                    column.child(
                        div()
                            .text_secondary(compact, cx)
                            .text_color(cx.theme().danger_foreground)
                            .child(failure_description(&reason)),
                    )
                }),
        )
        .when(current, |row| row.child(self.status_glyph(cx)))
        .when(needs_pairing, |row| {
            row.child(
                Button::new(SharedString::from(format!("repair-{}", host.host_id)))
                    .primary()
                    .compact()
                    .label(crate::tr!("hosts.pair_again"))
                    // A new pairing needs a new invitation from the machine;
                    // nothing from the stale record carries over.
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.open_pair(window, cx);
                    })),
            )
        })
        .when(!needs_pairing, |row| {
            row.on_click(move |_, window, cx| {
                let switch = cx.global::<ClientAttachment>().switcher();
                switch(
                    AttachmentTarget::Remote(Box::new(connect_host.clone())),
                    window,
                    cx,
                );
            })
        })
        // The row must keep its hover fill while the child menu is hovered.
        .child(self.host_menu(host, current));
        row.into_any_element()
    }

    fn host_menu(&self, host: &PairedHost, current: bool) -> AnyElement {
        let host_id = host.host_id.clone();
        let label = crate::tr!("hosts.actions", name = host.name.clone()).into_owned();
        div()
            .flex_none()
            .size(px(crate::material::TOUCH_TARGET))
            .flex()
            .items_center()
            .justify_center()
            // The row must not arm a connection press before the menu opens.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new(SharedString::from(format!("host-menu-{host_id}")))
                    .ghost()
                    .icon(IconName::Ellipsis)
                    .aria_label(label)
                    .dropdown_menu(move |menu, _window, _cx| {
                        let menu = menu.when(current, |menu| {
                            menu.menu(
                                crate::tr!("hosts.disconnect").into_owned(),
                                Box::new(DisconnectHost),
                            )
                        });
                        menu.menu(
                            crate::tr!("hosts.forget").into_owned(),
                            Box::new(ForgetHost(host_id.clone())),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The whole Hosts surface, one captioned group each: this machine and
    /// the invitation it offers other devices, its shared spaces, the saved
    /// machines and, where the client can pair, the ways to add another.
    pub(crate) fn render_hosts(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let hosts = self
            .client(cx)
            .map(|client| client.load_hosts())
            .unwrap_or_default();
        let current_id = self.attached_host_id(cx);
        let mut column = v_flex().gap(section_gap(cx));

        let mut this_machine: Vec<AnyElement> = Vec::new();
        if let Some(local) = self.local_row(compact, cx) {
            this_machine.push(local.into_any_element());
        }
        // Only a host running in this process can offer an invitation.
        #[cfg(feature = "remote-hosting")]
        if cx.has_global::<RemoteController>() {
            this_machine.push(self.invitation_row(compact, cx));
        }
        if !this_machine.is_empty() {
            column = column.child(settings_group(
                crate::tr!("hosts.this_machine_section"),
                this_machine,
                cx,
            ));
        }
        #[cfg(feature = "remote-hosting")]
        if cx.has_global::<RemoteController>() {
            column = column.children(self.space_links(compact, cx));
        }

        let saved = if hosts.is_empty() {
            vec![
                crate::material::empty_row(crate::tr!("hosts.empty"), cx)
                    .text_secondary(compact, cx)
                    .into_any_element(),
            ]
        } else {
            hosts
                .iter()
                .map(|host| {
                    let current = current_id.as_deref() == Some(host.host_id.as_str());
                    self.host_row(host, current, compact, cx)
                })
                .collect()
        };
        column = column.child(settings_group(crate::tr!("hosts.saved"), saved, cx));
        if !self.fixed_machine {
            column = column.child(self.add_machine(compact, cx));
        }
        self.page(column, &self.hosts_scroll, cx)
    }

    /// What this machine offers other devices: the live invitation while
    /// hosting accepts new devices, otherwise the way to the setting that
    /// turns it on. Hosting itself — the endpoint, its name, Traverse — is
    /// configured in Settings → Remote; this is where the connection is made.
    #[cfg(feature = "remote-hosting")]
    fn invitation_row(&mut self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        use hosting::{InvitationOffer, RemoteController};
        let offer = cx
            .try_global::<RemoteController>()
            .map(RemoteController::invitation_offer)
            .unwrap_or(InvitationOffer::NotHosting);
        self.sync_invitation_ticker(matches!(offer, InvitationOffer::Live { .. }), cx);
        match offer {
            InvitationOffer::Live {
                invitation,
                remaining,
            } => {
                let group = spaces::local(cx)
                    .and_then(|spaces| spaces.read(cx).group().map(|group| group.name.clone()));
                access::invitation_card(
                    &invitation.url(),
                    remaining,
                    group.as_deref(),
                    compact,
                    hosting::new_invitation(),
                    cx,
                )
            }
            InvitationOffer::Expired => {
                access::expired_invitation_row(compact, hosting::new_invitation(), cx)
            }
            InvitationOffer::NotHosting | InvitationOffer::PairingOff => {
                let note = if offer == InvitationOffer::PairingOff {
                    crate::tr!("hosts.invite.pairing_off")
                } else {
                    crate::tr!("hosts.invite.disabled")
                };
                crate::settings_rows::row_frame(compact, cx)
                    .child(crate::settings_rows::row_labels(
                        note, None, None, compact, cx,
                    ))
                    .child(
                        Button::new("hosts-remote-settings")
                            .debug_selector(|| "hosts-remote-settings".into())
                            .ghost()
                            .outline()
                            .compact()
                            .label(crate::tr!("hosts.invite.open_settings"))
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.window_state.update(cx, |state, cx| {
                                    state.pending_settings_section = Some("remote".into());
                                    state.open_settings(cx);
                                });
                            })),
                    )
                    .into_any_element()
            }
        }
    }

    /// This machine's shared spaces, each with its link and QR on demand.
    #[cfg(feature = "remote-hosting")]
    fn space_links(&mut self, compact: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let spaces = spaces::local(cx);
        self.spaces_observer.watch(spaces.as_ref(), cx);
        spaces.and_then(|spaces| {
            spaces::machine_links(
                &spaces,
                self.space_qr.as_deref(),
                |panel: &mut Self, id, cx| {
                    panel.space_qr = (panel.space_qr.as_deref() != Some(id.as_str())).then_some(id);
                    cx.notify();
                },
                compact,
                cx,
            )
        })
    }

    /// Run the one-second repaint exactly while an invitation counts down.
    /// Its last tick paints the expiry; a fresh invitation is painted by the
    /// visit that shows it.
    #[cfg(feature = "remote-hosting")]
    fn sync_invitation_ticker(&mut self, live: bool, cx: &mut Context<Self>) {
        match (live, self.invitation_ticker.is_some()) {
            (true, false) => {
                self.invitation_ticker = Some(cx.spawn(async move |this, cx| {
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
            (false, true) => self.invitation_ticker = None,
            _ => {}
        }
    }

    /// The ways in: the camera where there is one, and the invitation field
    /// everywhere. Scanning pairs on the spot; pasting opens a page for the
    /// field. A first pairing is always by invitation, never by finding a
    /// machine on the network. The attempt in flight and its failure are
    /// shown here, under the rows that started them.
    fn add_machine(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let scannable = self.client(cx).is_some_and(|client| client.supports_qr());
        let entry = |id: &'static str, title: SharedString, cx: &mut Context<Self>| {
            group_entry(id, title.clone(), compact, cx)
                .debug_selector(move || id.into())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_body(compact, cx)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(title)),
                )
                .child(
                    Icon::new(IconName::ChevronRight)
                        .flex_none()
                        .text_color(cx.theme().muted_foreground),
                )
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        if scannable {
            rows.push(
                entry(
                    "hosts-scan",
                    crate::tr!("hosts.pair.scan").into_owned().into(),
                    cx,
                )
                .on_click(cx.listener(|panel, _, _, cx| panel.scan(cx)))
                .into_any_element(),
            );
        }
        rows.push(
            entry(
                "hosts-pair",
                crate::tr!("hosts.pair.paste").into_owned().into(),
                cx,
            )
            .on_click(cx.listener(|panel, _, window, cx| panel.open_pair(window, cx)))
            .into_any_element(),
        );
        rows.extend(
            self.attempt_status(compact, cx)
                .map(|status| group_row(cx).child(status).into_any_element()),
        );
        rows.extend(
            self.address_prompt(true, compact, cx)
                .map(|prompt| group_row(cx).child(prompt).into_any_element()),
        );
        settings_group(crate::tr!("hosts.pair.title"), rows, cx)
            .debug_selector(|| "hosts-add-machine".into())
            .into_any_element()
    }

    /// The pairing attempt in flight, or the failure of the last one; one
    /// line at both the Hosts page and the paste page.
    fn attempt_status(&self, compact: bool, cx: &App) -> Option<AnyElement> {
        if self.form.busy {
            return Some(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .text_secondary(compact, cx)
                    .debug_selector(|| "hosts-pairing".into())
                    .child(crate::widgets::spinner::Spinner::new())
                    .child(div().min_w_0().child(crate::tr!("hosts.pair.connecting")))
                    .into_any_element(),
            );
        }
        let error = self.form.error.clone().or_else(|| {
            self.form
                .invalid(cx)
                .then(|| crate::tr!("hosts.pair.bad_invite").into_owned())
        })?;
        Some(
            div()
                .w_full()
                .text_secondary(compact, cx)
                .min_w_0()
                .text_color(cx.theme().danger_foreground)
                .debug_selector(|| "hosts-pair-error".into())
                .child(error)
                .into_any_element(),
        )
    }

    /// The machine's IP address, asked for once an attempt found no path to
    /// it; the prompt stands in for that failure. `retry` adds the button
    /// that sends it, where no Connect button is beside the field.
    fn address_prompt(
        &self,
        retry: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.form.needs_address(cx) {
            return None;
        }
        let invalid = self.form.typed_address(cx).is_err();
        let error = if invalid {
            Some(crate::tr!("hosts.pair.bad_address").into_owned())
        } else {
            self.form.address_error(cx).map(str::to_owned)
        };
        let busy = self.form.busy;
        Some(
            v_flex()
                .w_full()
                .gap_1p5()
                .debug_selector(|| "hosts-pair-address".into())
                .child(
                    div()
                        .text_body(compact, cx)
                        .child(crate::tr!("hosts.pair.address")),
                )
                .child(
                    div()
                        .text_secondary(compact, cx)
                        .min_w_0()
                        .child(crate::tr!("hosts.pair.address_description")),
                )
                .child(
                    Input::new(&self.form.address)
                        .placeholder(crate::tr!("hosts.pair.address_placeholder")),
                )
                .children(error.map(|error| {
                    div()
                        .text_secondary(compact, cx)
                        .min_w_0()
                        .text_color(cx.theme().danger_foreground)
                        .debug_selector(|| "hosts-pair-address-error".into())
                        .child(error)
                }))
                .when(retry, |column| {
                    column.child(
                        Button::new("hosts-pair-retry")
                            .primary()
                            .when(compact, |button| button.w_full())
                            .loading(busy)
                            .disabled(busy || invalid)
                            .label(crate::tr!("hosts.pair.retry"))
                            .on_click(cx.listener(|panel, _, window, cx| panel.submit(window, cx))),
                    )
                })
                .into_any_element(),
        )
    }

    /// The paste page. A link that parses is submitted as it lands; Connect
    /// is never the only way, only the retry. Wide, the form is one card with
    /// Connect beside its field. Compact, Connect is pinned to the foot of
    /// the page, above the software keyboard, which the window seam already
    /// accounts for.
    pub(crate) fn render_pair(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let busy = self.form.busy;
        let ready = !busy && self.form.request(cx).is_some() && self.form.typed_address(cx).is_ok();
        let scannable = self.client(cx).is_some_and(|client| client.supports_qr());
        let action = Button::new("hosts-pair-submit")
            .primary()
            .loading(busy)
            .disabled(!ready)
            .label(crate::tr!("hosts.pair.action"))
            .on_click(cx.listener(|panel, _, window, cx| panel.submit(window, cx)));
        let scan = scannable.then(|| {
            Button::new("hosts-scan")
                .ghost()
                .outline()
                .disabled(busy)
                .label(crate::tr!("hosts.pair.scan"))
                .on_click(cx.listener(|panel, _, _, cx| panel.scan(cx)))
        });
        let field = Input::new(&self.form.invitation)
            .placeholder(crate::tr!("hosts.pair.invitation_placeholder"));
        if !compact {
            let form = group_row_stacked(cx)
                .child(crate::settings_rows::row_labels(
                    crate::tr!("hosts.pair.invitation"),
                    Some(crate::tr!("hosts.pair.description").into_owned().into()),
                    None,
                    false,
                    cx,
                ))
                .child(
                    h_flex()
                        .w_full()
                        .gap(cx.theme().tokens.spacing.sm)
                        .items_center()
                        .child(div().flex_1().min_w_0().child(field))
                        .children(scan)
                        .child(action),
                )
                .children(self.attempt_status(compact, cx))
                .children(self.address_prompt(false, compact, cx));
            let page = v_flex().child(
                crate::material::group(cx)
                    .debug_selector(|| "hosts-pair-form".into())
                    .child(form),
            );
            return self.page(page, &self.pair_scroll, cx);
        }
        let body = v_flex()
            .gap(section_gap(cx))
            .child(
                div()
                    .text_secondary(compact, cx)
                    .min_w_0()
                    .child(crate::tr!("hosts.pair.description")),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        div()
                            .text_caption(cx)
                            .child(crate::tr!("hosts.pair.invitation")),
                    )
                    .child(field),
            )
            .children(self.attempt_status(compact, cx))
            .children(self.address_prompt(false, compact, cx))
            .children(scan.map(|scan| scan.w_full()));
        self.page_with_footer(body, action.w_full().into_any_element(), cx)
    }

    /// The scrolling page body: the settings content column, so the page's
    /// groups line up with Settings' at every width.
    fn page(&self, body: gpui::Div, scroll: &ScrollHandle, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        crate::scroll::page_viewport(
            "hosts-scroll-bounce",
            scroll,
            div()
                .id("hosts-scroll")
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(scroll)
                .on_action(cx.listener(Self::on_disconnect))
                .on_action(cx.listener(Self::on_forget))
                .child(crate::settings_rows::content_column(body, compact, cx)),
        )
        .into_any_element()
    }

    fn page_with_footer(
        &self,
        body: gpui::Div,
        action: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .child(self.page(body, &self.pair_scroll, cx))
            .child(
                h_flex().flex_none().w_full().justify_center().child(
                    div()
                        .w_full()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .px(px(COMPACT_PAGE_INSET))
                        .pb(px(COMPACT_PAGE_INSET))
                        .pt(cx.theme().tokens.spacing.sm)
                        .child(action),
                ),
            )
            .into_any_element()
    }

    fn on_disconnect(&mut self, _: &DisconnectHost, window: &mut Window, cx: &mut Context<Self>) {
        // Leaving a host is an explicit act; the saved record stays. A client
        // with a host of its own falls back to it rather than to nothing, and
        // to nothing when that host does not start.
        crate::shell::detach_current(cx);
        if cx
            .try_global::<ClientAttachment>()
            .is_some_and(ClientAttachment::can_attach_local)
        {
            let switch = cx.global::<ClientAttachment>().switcher();
            switch(AttachmentTarget::Local, window, cx);
        }
        cx.notify();
    }

    fn on_forget(&mut self, action: &ForgetHost, window: &mut Window, cx: &mut Context<Self>) {
        let id = action.0.clone();
        let Some(name) = self
            .client(cx)
            .and_then(|client| {
                client
                    .load_hosts()
                    .into_iter()
                    .find(|host| host.host_id == id)
            })
            .map(|host| host.name)
        else {
            return;
        };
        let panel = cx.entity().downgrade();
        confirm_destructive(
            crate::tr!("hosts.forget_title", name = name).into_owned(),
            crate::tr!("hosts.forget_description").into_owned(),
            crate::tr!("hosts.forget").into_owned(),
            move |_, cx| {
                cx.global::<ClientAttachment>().remove_host(&id);
                let _ = panel.update(cx, |_, cx| cx.notify());
            },
            window,
            cx,
        );
    }
}

/// A connected device is listed by name, then its operating system when the
/// device reported one — the same ` · ` separator used elsewhere.
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
pub(crate) fn device_label(name: &str, platform: Option<&str>) -> String {
    match platform {
        Some(platform) => format!("{name} · {platform}"),
        None => name.to_owned(),
    }
}

/// A path's name, in a device's status column and under a machine's name:
/// on the LAN, punched across the internet, or through a relay named by
/// its region; `None` is a device that is not connected.
pub(crate) fn path_label(path: Option<&tcode_protocol::PathInfo>) -> String {
    use tcode_protocol::PathKind;
    match path.map(tcode_protocol::PathInfo::kind) {
        None => crate::tr!("remote.path.offline"),
        Some(PathKind::Lan) => crate::tr!("remote.path.lan"),
        Some(PathKind::Tunnel) => crate::tr!("remote.path.tunnel"),
        Some(PathKind::Relay { url }) => match url.and_then(relay_region) {
            Some(region) => crate::tr!("remote.path.relay_region", region = region),
            None => crate::tr!("remote.path.relay"),
        },
    }
    .into_owned()
}

/// What tells one relay from another: the region label of an n0 relay
/// (`aps1-1.relay.n0.iroh.link` is `aps1-1`), the whole host of any other.
fn relay_region(relay: &str) -> Option<String> {
    let host = url::Url::parse(relay).ok()?.host_str()?.to_owned();
    Some(match host.strip_suffix(".relay.n0.iroh.link") {
        Some(region) => region.to_owned(),
        None => host,
    })
}

/// How this window's link to its machine is doing, one line under the
/// machine's name: the path while the link is up, and once it is lost the
/// failure, which says what to do about it. A transport that cannot tell
/// how it is carried (a browser) says only that it is connected. Loading
/// the baseline over an up link is not a link state: the content area's
/// skeletons show it, the line says the link is connected.
pub(crate) fn connection_label(state: &tcode_client::ConnectionState) -> String {
    use tcode_client::ConnectionState;
    match state {
        ConnectionState::Connected { path } | ConnectionState::Syncing { path } => match path {
            Some(path) if path.probing_direct => {
                crate::tr!("remote.path.probing_direct").into_owned()
            }
            Some(path) => format!(
                "{} · {}",
                path_label(Some(path)),
                crate::tr!("remote.state.connected")
            ),
            None => crate::tr!("remote.state.connected").into_owned(),
        },
        ConnectionState::Reconnecting { .. } => {
            crate::tr!("remote.state.reconnecting").into_owned()
        }
        ConnectionState::Offline { reason } => format!(
            "{} · {}",
            crate::tr!("remote.path.offline"),
            failure_label(reason)
        ),
    }
}

/// The same recovery wording is used in the shell and the machine row.
pub(crate) fn failure_label(reason: &tcode_client::ConnectionFailure) -> String {
    use tcode_client::ConnectionFailure::*;
    match reason {
        Unreachable(_) => crate::tr!("remote.failure.unreachable"),
        Timeout => crate::tr!("remote.failure.timeout"),
        AuthenticationRejected => crate::tr!("remote.failure.authentication_rejected"),
        ProtocolMismatch => crate::tr!("remote.failure.protocol_mismatch"),
        HostClosed => crate::tr!("remote.failure.host_closed"),
    }
    .into_owned()
}

/// The machine row has room for the transport's cause; the shell's status
/// line does not.
fn failure_description(reason: &tcode_client::ConnectionFailure) -> String {
    match reason {
        tcode_client::ConnectionFailure::Unreachable(Some(cause)) => {
            crate::tr!("remote.failure.unreachable_cause", reason = cause).into_owned()
        }
        reason => failure_label(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Render, TestAppContext};

    /// The line names the path while the link is up and the failure once
    /// it is lost; a relay is told apart by region, a self-hosted one by
    /// host; a transport that cannot tell how it is carried names no path.
    #[test]
    fn the_connection_label_names_the_path_or_the_failure() {
        use tcode_client::{ConnectionFailure, ConnectionState};
        use tcode_protocol::PathInfo;
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let lan = PathInfo {
            direct: true,
            relay: None,
            lan: true,
            probing_direct: false,
        };
        let tunnel = PathInfo {
            lan: false,
            ..lan.clone()
        };
        let relayed = PathInfo {
            direct: false,
            relay: Some("https://aps1-1.relay.n0.iroh.link/".into()),
            lan: false,
            probing_direct: false,
        };
        let self_hosted = PathInfo {
            relay: Some("https://traverse.example:8443/".into()),
            ..relayed.clone()
        };
        let probing = PathInfo {
            probing_direct: true,
            ..relayed.clone()
        };
        let connected = |path: &PathInfo| {
            connection_label(&ConnectionState::Connected {
                path: Some(path.clone()),
            })
        };
        assert_eq!(connected(&lan), "LAN · Connected");
        assert_eq!(connected(&tunnel), "Tunnel · Connected");
        assert_eq!(connected(&relayed), "Relay (aps1-1) · Connected");
        assert_eq!(
            connected(&self_hosted),
            "Relay (traverse.example) · Connected"
        );
        assert_eq!(connected(&probing), "Relay · trying direct");
        assert_eq!(path_label(None), "Offline");
        assert_eq!(path_label(Some(&probing)), "Relay (aps1-1)");
        assert_eq!(
            connection_label(&ConnectionState::Connected { path: None }),
            "Connected"
        );
        assert_eq!(
            connection_label(&ConnectionState::Syncing {
                path: Some(lan.clone())
            }),
            "LAN · Connected",
            "loading the baseline is not a link state"
        );
        assert_eq!(
            connection_label(&ConnectionState::Syncing { path: None }),
            "Connected"
        );
        assert_eq!(
            connection_label(&ConnectionState::Reconnecting {
                attempt: 3,
                reason: Some(ConnectionFailure::Timeout)
            }),
            "Reconnecting"
        );
        assert_eq!(
            connection_label(&ConnectionState::Offline {
                reason: ConnectionFailure::AuthenticationRejected
            }),
            "Offline · Access rejected · Pair again"
        );
        let unreachable = ConnectionFailure::Unreachable(Some(
            "No addressing information available: Address lookup failed".into(),
        ));
        assert_eq!(
            connection_label(&ConnectionState::Offline {
                reason: unreachable.clone()
            }),
            "Offline · Host unreachable",
            "the status line has no room for the cause"
        );
        assert_eq!(
            failure_description(&unreachable),
            "Host unreachable: No addressing information available: Address lookup failed",
            "the machine row says why"
        );
        assert_eq!(
            failure_description(&ConnectionFailure::Unreachable(None)),
            "Host unreachable"
        );
    }

    #[cfg(feature = "remote-hosting")]
    struct PairingProbe(Entity<RemotePanel>);

    #[cfg(feature = "remote-hosting")]
    impl Render for PairingProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// The browser is signed in with the machine that served it: its Hosts
    /// page lists that machine and nothing that would add or re-pair one.
    #[gpui::test]
    fn a_fixed_machine_client_has_no_way_to_add_one(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        struct Browser;
        impl ClientHost for Browser {
            fn device_name(&self) -> String {
                "Safari".into()
            }
            fn device_id(&self) -> String {
                "browser".into()
            }
            fn device_platform(&self) -> Option<String> {
                None
            }
            fn load_hosts(&self) -> Vec<PairedHost> {
                vec![PairedHost {
                    host_id: "served-by".into(),
                    name: "Build server".into(),
                    traverse: Vec::new(),
                    relay: None,
                    addrs: Vec::new(),
                    last_connected_unix: None,
                    group_id: None,
                    space_id: None,
                    space_name: None,
                }]
            }
            fn load_preferences(&self) -> tcode_client::host::ClientPreferences {
                Default::default()
            }
            fn save_preferences(&self, _: &tcode_client::host::ClientPreferences) {}
            fn save_hosts(&self, _: &[PairedHost]) {}
            fn last_host_id(&self) -> Option<String> {
                Some("served-by".into())
            }
            fn set_last_host_id(&self, _: Option<&str>) {}
            fn fixed_machine(&self) -> bool {
                true
            }
            fn connect(&self, _: &PairedHost) -> tcode_client::host::Transport {
                unreachable!("the page only lists the machine")
            }
        }
        struct HostsProbe(Entity<RemotePanel>);
        impl Render for HostsProbe {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                v_flex().size_full().child(
                    self.0
                        .update(cx, |panel, cx| panel.render_hosts(window, cx)),
                )
            }
        }
        cx.update(crate::theme::init);
        cx.update(|cx| cx.set_global(ClientAttachment::new(Rc::new(Browser), false, |_, _, _| {})));
        let window = cx.open_window(gpui::size(px(393.), px(852.)), |window, cx| {
            let state = cx.new(|_| WindowState::new(false));
            HostsProbe(cx.new(|cx| RemotePanel::new(None, state, window, cx)))
        });
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        cx.run_until_parked();
        cx.update(|window, cx| {
            _ = window.draw(cx);
        });
        assert!(
            cx.debug_bounds("host-served-by").is_some(),
            "the serving machine is listed"
        );
        assert!(
            cx.debug_bounds("hosts-add-machine").is_none(),
            "a browser has no way to add a machine"
        );
    }

    /// A scanned link that opens no path to the machine: the Hosts page asks
    /// for the machine's address under the failure, and the retry sends the
    /// same invitation with it.
    #[gpui::test]
    fn a_link_that_found_no_path_asks_for_the_address_and_retries_with_it(cx: &mut TestAppContext) {
        use std::cell::RefCell;
        use std::net::IpAddr;
        use tcode_client::pairing::PairInvite;
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        #[derive(Default)]
        struct Phone {
            attempts: RefCell<Vec<(PairInvite, Option<IpAddr>)>>,
            hosts: RefCell<Vec<PairedHost>>,
        }
        impl ClientHost for Phone {
            fn device_name(&self) -> String {
                "phone".into()
            }
            fn device_id(&self) -> String {
                "phone".into()
            }
            fn device_platform(&self) -> Option<String> {
                None
            }
            fn load_hosts(&self) -> Vec<PairedHost> {
                self.hosts.borrow().clone()
            }
            fn load_preferences(&self) -> tcode_client::host::ClientPreferences {
                Default::default()
            }
            fn save_preferences(&self, _: &tcode_client::host::ClientPreferences) {}
            fn save_hosts(&self, hosts: &[PairedHost]) {
                *self.hosts.borrow_mut() = hosts.to_vec();
            }
            fn last_host_id(&self) -> Option<String> {
                None
            }
            fn set_last_host_id(&self, _: Option<&str>) {}
            fn supports_qr(&self) -> bool {
                true
            }
            fn pair(
                &self,
                invite: PairInvite,
                address: Option<IpAddr>,
            ) -> tcode_client::host::HostFuture<'_, Result<PairedHost, String>> {
                self.attempts.borrow_mut().push((invite.clone(), address));
                Box::pin(async move {
                    let reached: Option<IpAddr> = "192.168.1.20".parse().ok();
                    if address == reached {
                        Ok(invite.paired("Studio".into()))
                    } else {
                        Err(
                            "could not connect to the machine: No addressing information \
                             available"
                                .into(),
                        )
                    }
                })
            }
            fn connect(&self, _: &PairedHost) -> tcode_client::host::Transport {
                unreachable!("the switch is the attachment owner's")
            }
        }
        struct HostsProbe(Entity<RemotePanel>);
        impl Render for HostsProbe {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                v_flex().size_full().child(
                    self.0
                        .update(cx, |panel, cx| panel.render_hosts(window, cx)),
                )
            }
        }
        let phone = Rc::new(Phone::default());
        cx.update(crate::theme::init);
        cx.update(|cx| cx.set_global(ClientAttachment::new(phone.clone(), false, |_, _, _| {})));
        let window = cx.open_window(gpui::size(px(393.), px(852.)), |window, cx| {
            let state = cx.new(|_| WindowState::new(true));
            HostsProbe(cx.new(|cx| RemotePanel::new(None, state, window, cx)))
        });
        let panel = window.update(cx, |probe, _, _| probe.0.clone()).unwrap();
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.refresh();
                _ = window.draw(cx);
            });
        };
        let invite = PairInvite {
            host_id: "ab".repeat(32),
            secret: "AAECAwQFBgcICQoLDA0ODw".into(),
            space: None,
            group: None,
            traverse: Vec::new(),
            relay: None,
            port: 47420,
        };
        let link = tcode_client::pairing::pair_url(&invite);
        // What a scan does with the link it read.
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                assert!(panel.form.fill_invite(&link, window, cx));
                panel.submit(window, cx);
            });
        });
        draw(cx);
        assert_eq!(*phone.attempts.borrow(), [(invite.clone(), None)]);
        assert!(
            cx.debug_bounds("hosts-pair-address").is_some(),
            "no path: the page asks for the address"
        );
        assert!(
            cx.debug_bounds("hosts-pair-error").is_none()
                && cx.debug_bounds("hosts-pair-address-error").is_none(),
            "the prompt stands in for the transport's cause"
        );
        let retry = |address: &'static str, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel
                        .form
                        .address
                        .update(cx, |state, cx| state.set_value(address, window, cx));
                    panel.submit(window, cx);
                });
            });
            draw(cx);
        };
        retry("192.168.1.99", cx);
        assert!(
            cx.debug_bounds("hosts-pair-address-error").is_some(),
            "a typed address that found no path says so in the prompt"
        );
        assert!(cx.debug_bounds("hosts-pair-error").is_none());
        retry("192.168.1.20", cx);
        assert_eq!(
            phone.attempts.borrow()[2],
            (invite.clone(), Some("192.168.1.20".parse().unwrap())),
            "the same invitation, with the typed address"
        );
        assert_eq!(phone.load_hosts(), [invite.paired("Studio".into())]);
        assert!(cx.debug_bounds("hosts-pair-address").is_none());
    }

    /// The Hosts page is where this machine offers a connection: with hosting
    /// off it points at the setting; hosting and accepting devices, it shows
    /// the invitation itself, and the expiry when that runs out.
    #[cfg(feature = "remote-hosting")]
    #[gpui::test]
    fn the_hosts_page_offers_this_machines_invitation(cx: &mut TestAppContext) {
        use gpui::BorrowAppContext as _;
        use tcode_client::HostLink;
        use tcode_traverse::{HostConfig, HostMux, TraverseHost};
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("hosts-invitation");
        std::fs::create_dir_all(&root).unwrap();
        let (to_host, _host_rx) = async_channel::unbounded::<String>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let (attachments, _attachment_rx) = async_channel::unbounded();
        let mux = HostMux::new(attachments, from_host.clone());
        cx.update(crate::theme::init);
        cx.update(|cx| {
            cx.set_global(RemoteController::new(
                mux.clone(),
                root.clone(),
                HostLink::new(to_host, from_host),
                tcode_core::settings::Settings::default(),
            ));
            let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "desk"));
            cx.set_global(ClientAttachment::new(client, true, |_, _, _| {}));
        });
        struct HostsProbe(Entity<RemotePanel>);
        impl Render for HostsProbe {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                v_flex().size_full().child(
                    self.0
                        .update(cx, |panel, cx| panel.render_hosts(window, cx)),
                )
            }
        }
        let state = cx.new(|_| WindowState::new(false));
        let window = cx.open_window(gpui::size(px(1024.), px(768.)), |window, cx| {
            HostsProbe(cx.new(|cx| RemotePanel::new(None, state.clone(), window, cx)))
        });
        let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.refresh();
                _ = window.draw(cx);
            });
        };
        for width in [1024., 393.] {
            cx.simulate_resize(gpui::size(px(width), px(852.)));
            state.update(cx, |state, cx| {
                state.compact = width < 900.;
                cx.notify();
            });
            draw(cx);
            assert!(
                cx.debug_bounds("hosts-remote-settings").is_some(),
                "with hosting off, the page leads to the setting"
            );
            assert!(cx.debug_bounds("remote-invitation").is_none());
            assert!(cx.debug_bounds("hosting-settings").is_none());
        }

        let host = TraverseHost::start(
            mux,
            HostConfig {
                host_name: "Studio".into(),
                data_dir: root.clone(),
                traverse: Vec::new(),
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .unwrap();
        host.new_invitation();
        cx.update(|_, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| controller.adopt_host(host));
        });
        for width in [1024., 393.] {
            cx.simulate_resize(gpui::size(px(width), px(852.)));
            state.update(cx, |state, cx| {
                state.compact = width < 900.;
                cx.notify();
            });
            draw(cx);
            assert!(
                cx.debug_bounds("remote-invitation").is_some(),
                "hosting and accepting devices, the invitation is on the page"
            );
            assert!(cx.debug_bounds("hosts-remote-settings").is_none());
            assert!(
                cx.debug_bounds("hosting-settings").is_none(),
                "the hosting controls stay in Settings"
            );
        }

        cx.update(|_, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| {
                controller.set_pairing_enabled(false)
            });
        });
        for width in [1024., 393.] {
            cx.simulate_resize(gpui::size(px(width), px(852.)));
            state.update(cx, |state, cx| {
                state.compact = width < 900.;
                cx.notify();
            });
            draw(cx);
            assert!(cx.debug_bounds("remote-invitation").is_none());
            assert!(
                cx.debug_bounds("hosts-remote-settings").is_some(),
                "with pairing off, the page leads to the setting"
            );
            assert!(cx.debug_bounds("hosting-settings").is_none());
        }

        cx.update(|_, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| controller.stop_hosting());
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "remote-hosting")]
    #[gpui::test]
    fn superseded_pairing_does_not_overwrite_the_saved_machine(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("stale-pairing");
        let client = Rc::new(tcode_traverse::NativeClientHost::new(root.clone(), "phone"));
        cx.update(|cx| cx.set_global(ClientAttachment::new(client.clone(), false, |_, _, _| {})));
        let (probe, cx) = cx.add_window_view(|window, cx| {
            let state = cx.new(|_| WindowState::new(false));
            PairingProbe(cx.new(|cx| RemotePanel::new(None, state, window, cx)))
        });
        let host = |name: &str| PairedHost {
            host_id: "09e5c592f3234ef329634a0baa6bfe06193be6b64801121d84242af1d27dbecf".into(),
            name: name.into(),
            traverse: Vec::new(),
            relay: None,
            addrs: vec!["192.168.1.10:47420".into()],
            last_connected_unix: None,
            group_id: None,
            space_id: None,
            space_name: None,
        };
        probe.update_in(cx, |probe, window, cx| {
            probe.0.update(cx, |panel, cx| {
                let old = panel.form.restart();
                let current = panel.form.restart();
                panel.finish_pair(old, Err("invalid or expired invitation".into()), window, cx);
                assert!(panel.form.error.is_none());
                assert!(client.load_hosts().is_empty());
                panel.finish_pair(
                    current,
                    Err("invalid or expired invitation".into()),
                    window,
                    cx,
                );
                assert_eq!(
                    panel.form.error.as_deref(),
                    Some(crate::tr!("hosts.pair.rejected").as_ref())
                );
                assert!(client.load_hosts().is_empty());
                let current = panel.form.restart();
                panel.finish_pair(current, Ok(host("current pairing")), window, cx);
                panel.finish_pair(old, Ok(host("superseded pairing")), window, cx);
            });
        });
        assert_eq!(client.load_hosts(), vec![host("current pairing")]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
