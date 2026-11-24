//! Hosting this machine: the Traverse endpoint and its settings. Settings →
//! Remote turns it on and names it; the invitation it mints and the devices
//! that paired are drawn by `access` there and on the Machines page.
//!
//! [`RemoteController`] is the process-wide handle the composition root installs.
//! It owns the local [`HostMux`] and endpoint independently of whichever host
//! the window is currently attached to, so **Connect** and **Back to local**
//! never stop it and never disturb another attached client.

use std::path::PathBuf;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, BorrowAppContext as _, Context, Entity, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Window, div, px,
};
use gpui_base::v_flex;
use tcode_client::HostLink;
#[cfg(test)]
use tcode_core::settings::TraverseSource;
use tcode_core::settings::{Settings, TraverseInstance, TraverseSetting};
use tcode_protocol::{Command, HostingAction, HostingState, SettingsPatch, TraverseSourceStatus};
use tcode_traverse::manifest::ManifestSource;
use tcode_traverse::{DeviceInfo, HostConfig, HostMux, Invitation, TraverseHost};

use super::access::AccessSection;
use super::spaces::{SpacesObserver, this_machine};
use crate::material::{group_row, group_row_stacked};
use crate::settings_rows::{FIELD_WIDTH, ToggleRow, row_labels, settings_group};
use crate::store::WorkspaceStore;
use crate::widgets::input::{Input, InputEvent, InputState};

pub struct RemoteController {
    mux: HostMux,
    host: Option<TraverseHost>,
    data_dir: PathBuf,
    local_settings_link: HostLink,
    local_settings: Settings,
    /// Why the last start did not bind, until hosting starts or is turned
    /// off; Settings → Remote shows it under the hosting switch.
    start_failure: Option<String>,
}

impl Global for RemoteController {}

impl RemoteController {
    pub fn new(
        mux: HostMux,
        data_dir: PathBuf,
        local_settings_link: HostLink,
        local_settings: Settings,
    ) -> Self {
        Self {
            mux,
            host: None,
            data_dir,
            local_settings_link,
            local_settings,
            start_failure: None,
        }
    }

    pub fn local_settings(&self) -> &Settings {
        &self.local_settings
    }

    /// The name this machine goes by: the one set in Settings → Remote, else
    /// the device's own.
    pub fn machine_name(&self) -> String {
        self.local_settings
            .remote_host_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(machine_name)
    }

    pub fn save_hosting_settings(
        &mut self,
        enabled: bool,
        traverse: TraverseSetting,
        name: Option<String>,
    ) {
        self.local_settings.remote_hosting_enabled = enabled;
        self.local_settings.traverse = traverse.clone();
        self.local_settings.remote_host_name = name.clone();
        for patch in [
            SettingsPatch::RemoteHostingEnabled(enabled),
            SettingsPatch::Traverse(traverse),
            SettingsPatch::RemoteHostName(name),
        ] {
            if let Err(error) = self
                .local_settings_link
                .dispatch(Command::PatchSettings { patch })
            {
                log::error!(
                    "could not persist local hosting settings: {}",
                    error.message
                );
            }
        }
    }

    pub fn is_hosting(&self) -> bool {
        self.host.is_some()
    }

    /// This machine's id while hosting.
    pub fn endpoint_id(&self) -> Option<String> {
        self.host.as_ref().map(TraverseHost::endpoint_id)
    }

    /// Bind the endpoint, publish to `traverse` and mint a first invitation.
    /// A failure is kept as [`Self::start_failure`] and returned, localized.
    pub fn start_hosting(
        &mut self,
        traverse: &TraverseSetting,
        host_name: String,
    ) -> Result<(), String> {
        if self.host.is_some() {
            return Ok(());
        }
        let started = traverse_sources(traverse).and_then(|traverse| {
            TraverseHost::start(
                self.mux.clone(),
                HostConfig {
                    host_name,
                    data_dir: self.data_dir.clone(),
                    traverse,
                    pairing_enabled: true,
                    // Fixed, so the invitation's port, firewall rules and LAN
                    // probes survive restarts.
                    bind_port: Some(tcode_traverse::lan::DEFAULT_PORT),
                },
            )
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::AddrInUse => crate::tr!("remote.host.port_in_use").into_owned(),
                _ => error.to_string(),
            })
        });
        let host = match started {
            Ok(host) => host,
            Err(reason) => {
                let failure = crate::tr!("remote.host.start_failed", reason = reason).into_owned();
                self.start_failure = Some(failure.clone());
                return Err(failure);
            }
        };
        self.start_failure = None;
        if host.pairing_enabled() {
            host.new_invitation();
        }
        self.host = Some(host);
        Ok(())
    }

    /// Why hosting did not start the last time it was asked to.
    pub fn start_failure(&self) -> Option<&str> {
        self.start_failure.as_deref()
    }

    /// Adopt a host started elsewhere: tests bind a random port, since the
    /// desktop's fixed one may be taken on the machine running them.
    #[cfg(test)]
    pub(crate) fn adopt_host(&mut self, host: TraverseHost) {
        self.host = Some(host);
    }

    pub fn stop_hosting(&mut self) {
        self.start_failure = None;
        if let Some(host) = self.host.take() {
            host.shutdown();
        }
    }

    pub fn new_invitation(&mut self) {
        if let Some(host) = self.host.as_ref() {
            host.new_invitation();
        }
    }

    /// Whether new devices can pair while hosting. The transport persists
    /// the choice, so it outlives a restart and applies to headless too.
    pub fn pairing_enabled(&self) -> bool {
        self.host
            .as_ref()
            .is_some_and(TraverseHost::pairing_enabled)
    }

    /// Turning pairing on mints an invitation at once; turning it off drops
    /// the active one, and paired devices keep working.
    pub fn set_pairing_enabled(&self, enabled: bool) {
        if let Some(host) = self.host.as_ref() {
            host.set_pairing_enabled(enabled);
            if enabled {
                host.new_invitation();
            }
        }
    }

    /// The active invitation with its remaining lifetime in seconds, or
    /// `None` once it has expired. It carries where this machine is
    /// reachable *now*: an invitation is minted the moment hosting starts,
    /// before the endpoint has found its home relay, so the QR is composed
    /// at paint time from [`TraverseHost::invitation`], as the hosting query
    /// answers a remote client.
    pub fn invitation(&self) -> Option<(Invitation, u64)> {
        let (invitation, remaining) = self.host.as_ref()?.invitation()?;
        (remaining.as_secs() > 0).then_some((invitation, remaining.as_secs()))
    }

    /// What the Hosts page can offer another device right now.
    pub fn invitation_offer(&self) -> InvitationOffer {
        if !self.is_hosting() {
            return InvitationOffer::NotHosting;
        }
        if !self.pairing_enabled() {
            return InvitationOffer::PairingOff;
        }
        match self.invitation() {
            Some((invitation, remaining)) => InvitationOffer::Live {
                invitation,
                remaining,
            },
            None => InvitationOffer::Expired,
        }
    }

    /// How each enabled Traverse source is doing while hosting.
    pub fn traverse_status(&self) -> Vec<TraverseSourceStatus> {
        self.host
            .as_ref()
            .map(TraverseHost::traverse_status)
            .unwrap_or_default()
    }

    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.host
            .as_ref()
            .map(TraverseHost::devices)
            .unwrap_or_default()
    }

    /// Run a hosting action against this machine, as a remote client's
    /// `Query::Hosting` would.
    pub fn hosting(&self, action: HostingAction) -> Result<HostingState, String> {
        self.host
            .as_ref()
            .ok_or_else(|| crate::tr!("spaces.not_hosting").into_owned())?
            .hosting(action)
            .map_err(|error| error.message)
    }

    /// Answers hosting queries off the UI thread while hosting.
    pub fn hosting_handle(&self) -> Option<tcode_traverse::host::HostingHandle> {
        self.host.as_ref().map(TraverseHost::hosting_handle)
    }

    /// Remove a device. When the allow list cannot be written the device
    /// stays paired and connected, and the error says so.
    pub fn revoke_device(&self, id: &str) -> Result<(), String> {
        match self.host.as_ref() {
            Some(host) => host.revoke(id).map_err(|error| error.to_string()),
            None => Ok(()),
        }
    }
}

/// Whether this machine has an invitation to show, and if not, why: the
/// Hosts page names the setting that would change that.
#[derive(Debug, Clone, PartialEq)]
pub enum InvitationOffer {
    NotHosting,
    PairingOff,
    Expired,
    Live {
        invitation: Invitation,
        /// Seconds left.
        remaining: u64,
    },
}

/// The transport's view of a Traverse setting: its enabled sources. A
/// self-hosted instance needs a usable base URL; the page validates it
/// before saving, so a failure here comes from a settings file edited by
/// hand.
fn traverse_sources(setting: &TraverseSetting) -> Result<Vec<ManifestSource>, String> {
    setting
        .enabled()
        .map(|instance| match instance {
            TraverseInstance::Official => Ok(ManifestSource::Official),
            TraverseInstance::Custom { url } => custom_traverse_url(url)
                .map(ManifestSource::Custom)
                .ok_or_else(|| crate::tr!("remote.traverse.invalid_url").into_owned()),
        })
        .collect()
}

/// A self-hosted Traverse base URL as typed: `http(s)` with a host.
pub(super) fn custom_traverse_url(value: &str) -> Option<url::Url> {
    let url = url::Url::parse(value.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then_some(url)
}

/// This machine's default advertised host name.
pub fn machine_name() -> String {
    tcode_traverse::native_host::default_device_name()
}

/// A row whose control is a text field beside the label. The label keeps
/// [`FIELD_LABEL_BASIS`] of the width, so where the field does not fit
/// beside it the field wraps under the label instead of squeezing it.
pub(super) fn field_row(compact: bool, cx: &App) -> gpui::Div {
    if compact {
        group_row_stacked(cx)
    } else {
        group_row(cx).flex_wrap()
    }
}

/// The width a [`field_row`]'s label keeps before its field wraps under it.
const FIELD_LABEL_BASIS: f32 = 200.;

/// [`row_labels`] for a [`field_row`].
pub(super) fn field_labels(
    title: SharedString,
    description: Option<SharedString>,
    compact: bool,
    cx: &App,
) -> gpui::Div {
    // Stacked, the basis would be a height.
    row_labels(title, description, None, compact, cx)
        .when(!compact, |labels| labels.flex_basis(px(FIELD_LABEL_BASIS)))
}

/// Start or stop hosting with `traverse` and `name`, saving them once that
/// worked. A failure saves nothing; the hosting switch shows it, and the
/// caller tells a user who is not looking at that switch.
fn switch_hosting(
    enabled: bool,
    traverse: TraverseSetting,
    name: Option<String>,
    cx: &mut App,
) -> Result<(), String> {
    let host_name = name.clone().unwrap_or_else(machine_name);
    cx.update_global::<RemoteController, _>(|controller, _| {
        if enabled {
            controller.start_hosting(&traverse, host_name)?;
        } else {
            controller.stop_hosting();
        }
        controller.save_hosting_settings(enabled, traverse, name);
        Ok(())
    })
}

/// Save a new name or Traverse setting; while hosting, re-bind the endpoint
/// with it so it takes effect at once.
pub(super) fn apply_hosting_settings(
    traverse: TraverseSetting,
    name: Option<String>,
    cx: &mut App,
) -> Result<(), String> {
    if cx
        .try_global::<RemoteController>()
        .is_some_and(RemoteController::is_hosting)
    {
        switch_hosting(false, traverse.clone(), name.clone(), cx)?;
        switch_hosting(true, traverse, name, cx)
    } else {
        cx.update_global::<RemoteController, _>(|controller, _| {
            controller.save_hosting_settings(false, traverse, name);
        });
        Ok(())
    }
}

/// The Machines page's New invitation.
pub(super) fn new_invitation() -> super::access::NewInvitation {
    std::rc::Rc::new(|window, cx| {
        cx.update_global::<RemoteController, _>(|controller, _| controller.new_invitation());
        window.refresh();
    })
}

/// Settings → Remote: whether this machine hosts, accepts new devices and
/// what it is called, then its invitation and the devices paired with it.
/// Their live state lives in the process-wide [`RemoteController`]; only the
/// name being typed belongs here.
pub struct HostingPanel {
    host_name_input: Entity<InputState>,
    access: Entity<AccessSection>,
    observer: SpacesObserver,
    _subscription: gpui::Subscription,
}

impl HostingPanel {
    /// `store` is the window's attachment: it names this machine's projects
    /// while the window shows this machine.
    pub fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let saved = cx
            .try_global::<RemoteController>()
            .and_then(|controller| controller.local_settings().remote_host_name.clone())
            .unwrap_or_default();
        let host_name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(machine_name())
                .default_value(saved)
        });
        // A typed name takes effect when the field is left or Enter is
        // pressed.
        let subscription = cx.subscribe_in(
            &host_name_input,
            window,
            |this, _, event: &InputEvent, _, cx| match event {
                InputEvent::Blur | InputEvent::PressEnter { .. } => this.apply_name(cx),
                InputEvent::Change | InputEvent::Focus => {}
            },
        );
        Self {
            host_name_input,
            access: cx.new(|_| AccessSection::new(store, this_machine, false)),
            observer: SpacesObserver::default(),
            _subscription: subscription,
        }
    }

    /// The name as typed; `None` when empty, for the device's own.
    fn typed_name(&self, cx: &App) -> Option<String> {
        let typed = self.host_name_input.read(cx).value().trim().to_owned();
        (!typed.is_empty()).then_some(typed)
    }

    fn set_hosting(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(traverse) = cx
            .try_global::<RemoteController>()
            .map(|controller| controller.local_settings().traverse.clone())
        else {
            return;
        };
        let name = self.typed_name(cx);
        // A failure shows under the hosting switch, on this page.
        let _ = switch_hosting(enabled, traverse, name, cx);
        cx.notify();
    }

    fn apply_name(&mut self, cx: &mut Context<Self>) {
        let Some(controller) = cx.try_global::<RemoteController>() else {
            return;
        };
        let name = self.typed_name(cx);
        if name == controller.local_settings().remote_host_name {
            return;
        }
        let traverse = controller.local_settings().traverse.clone();
        // A failure shows under the hosting switch, on this page.
        let _ = apply_hosting_settings(traverse, name, cx);
        cx.notify();
    }

    /// Turning pairing on mints an invitation at once; turning it off drops
    /// the active one, and paired devices keep working.
    fn set_pairing_enabled(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spaces) = super::spaces::local(cx) {
            let task = spaces.update(cx, |spaces, cx| {
                spaces.act_hosting(HostingAction::SetEnabled(enabled), cx)
            });
            super::report("remote.failed", task, window, cx);
        }
        cx.notify();
    }

    fn render_machine(&mut self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let (hosting, pairing, failure) = cx
            .try_global::<RemoteController>()
            .map(|controller| {
                (
                    controller.is_hosting(),
                    controller.pairing_enabled(),
                    controller.start_failure().map(SharedString::from),
                )
            })
            .unwrap_or_default();
        let mut rows = vec![
            ToggleRow::new(
                "remote-hosting",
                crate::tr!("remote.host.title"),
                hosting,
                compact,
                cx.listener(|this, checked: &bool, _, cx| {
                    this.set_hosting(*checked, cx);
                }),
            )
            .error(failure)
            .into_any_element(),
        ];
        if hosting {
            rows.push(
                ToggleRow::new(
                    "remote-pairing",
                    crate::tr!("remote.pairing.title"),
                    pairing,
                    compact,
                    cx.listener(|this, checked: &bool, window, cx| {
                        this.set_pairing_enabled(*checked, window, cx);
                    }),
                )
                .into_any_element(),
            );
        }
        rows.push(
            field_row(compact, cx)
                .child(field_labels(
                    crate::tr!("remote.host_name.title").into_owned().into(),
                    Some(
                        crate::tr!("remote.host_name.description")
                            .into_owned()
                            .into(),
                    ),
                    compact,
                    cx,
                ))
                .child(
                    div()
                        .when(compact, |field| field.w_full())
                        .when(!compact, |field| {
                            field.w(px(FIELD_WIDTH)).min_w_0().flex_shrink(1.)
                        })
                        .child(Input::new(&self.host_name_input)),
                )
                .into_any_element(),
        );
        settings_group(crate::tr!("remote.host.section"), rows, cx).into_any_element()
    }
}

impl Render for HostingPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        // Another client may turn pairing on or off; the machine's state
        // repaints the switch.
        let spaces = super::spaces::local(cx);
        self.observer.watch(spaces.as_ref(), cx);
        let hosting = spaces.is_some();
        v_flex()
            .w_full()
            .min_w_0()
            .gap(crate::material::section_gap(cx))
            .debug_selector(|| "hosting-settings".into())
            .child(self.render_machine(compact, cx))
            .when(hosting, |column| column.child(self.access.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// Settings → Remote, driven as a user does: the pairing switch exists
    /// only while hosting; with it on, the invitation the switch produces is
    /// on the same page, and turning it off reaches the transport, which
    /// drops the invitation. A typed name is saved when its field is left.
    #[gpui::test]
    fn the_remote_page_shows_the_invitation_while_accepting_devices(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let root = crate::store::tests::scratch_root("hosting-pairing");
        std::fs::create_dir_all(&root).unwrap();
        // Idle pipes: the host is never attached to here.
        let (to_host, _host_rx) = async_channel::unbounded::<String>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let (attachments, _attachment_rx) = async_channel::unbounded();
        let mux = HostMux::new(attachments, from_host.clone());
        cx.update(crate::theme::init);
        cx.update(|cx| {
            cx.set_global(RemoteController::new(
                mux.clone(),
                root.clone(),
                HostLink::new(to_host.clone(), from_host.clone()),
                Settings::default(),
            ))
        });
        let (panel, cx) = cx.add_window_view(|window, cx| {
            let store = cx.new(|cx| {
                WorkspaceStore::new_attached(
                    HostLink::new(to_host.clone(), from_host.clone()),
                    crate::store::WorkspaceAttachment::Local,
                    None,
                    None,
                    false,
                    cx,
                )
            });
            HostingPanel::new(store, window, cx)
        });
        cx.simulate_resize(gpui::size(px(900.), px(1200.)));
        let draw = |cx: &mut gpui::VisualTestContext| {
            for _ in 0..2 {
                cx.run_until_parked();
                cx.update(|window, cx| {
                    window.refresh();
                    _ = window.draw(cx);
                });
            }
        };
        draw(cx);
        assert!(
            cx.debug_bounds("remote-pairing").is_none(),
            "no pairing switch while not hosting"
        );
        assert!(cx.debug_bounds("remote-invitation").is_none());

        panel.update_in(cx, |panel, window, cx| {
            panel.host_name_input.update(cx, |input, cx| {
                input.set_value("Studio", window, cx);
                cx.emit(InputEvent::Blur);
            });
        });
        cx.read(|cx| {
            assert_eq!(
                cx.global::<RemoteController>()
                    .local_settings()
                    .remote_host_name
                    .as_deref(),
                Some("Studio")
            );
        });

        // A random port: the desktop's fixed one may be taken on this machine.
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
        draw(cx);
        assert!(cx.debug_bounds("remote-pairing").is_some());
        assert!(
            cx.debug_bounds("remote-invitation").is_some(),
            "hosting and accepting devices, the invitation is beside its switch"
        );

        let switch = cx.debug_bounds("remote-pairing").unwrap();
        cx.simulate_click(switch.center(), gpui::Modifiers::default());
        draw(cx);
        cx.read(|cx| {
            let controller = cx.global::<RemoteController>();
            assert!(!controller.pairing_enabled());
            assert_eq!(
                controller.invitation_offer(),
                InvitationOffer::PairingOff,
                "no invitation is offered while pairing is off"
            );
        });
        assert!(cx.debug_bounds("remote-invitation").is_none());

        let switch = cx.debug_bounds("remote-pairing").unwrap();
        cx.simulate_click(switch.center(), gpui::Modifiers::default());
        draw(cx);
        assert!(
            cx.debug_bounds("remote-invitation").is_some(),
            "turning pairing on mints an invitation at once"
        );

        cx.update(|_, cx| {
            cx.update_global::<RemoteController, _>(|controller, _| controller.stop_hosting());
        });
        draw(cx);
        assert!(cx.debug_bounds("remote-pairing").is_none());
        assert!(cx.debug_bounds("remote-invitation").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The transport publishes to the enabled sources, in order. A
    /// self-hosted instance is named by the base URL its manifest is served
    /// from; anything a browser would not fetch is refused.
    #[test]
    fn a_self_hosted_traverse_needs_a_fetchable_base_url() {
        let custom = |url: &str, enabled| TraverseSource {
            instance: TraverseInstance::Custom { url: url.into() },
            enabled,
        };
        let official = |enabled| TraverseSource {
            instance: TraverseInstance::Official,
            enabled,
        };
        assert_eq!(
            traverse_sources(&TraverseSetting::new(vec![
                official(true),
                custom(" https://traverse.example/ ", true),
                custom("https://disabled.example/", false),
            ])),
            Ok(vec![
                ManifestSource::Official,
                ManifestSource::Custom(url::Url::parse("https://traverse.example/").unwrap())
            ])
        );
        assert_eq!(
            traverse_sources(&TraverseSetting::new(vec![official(false)])),
            Ok(Vec::new())
        );
        for rejected in [
            "",
            "traverse.example",
            "ftp://traverse.example/",
            "https://",
        ] {
            assert!(
                traverse_sources(&TraverseSetting::new(vec![
                    official(true),
                    custom(rejected, true)
                ]))
                .is_err(),
                "{rejected:?}"
            );
        }
    }
}
