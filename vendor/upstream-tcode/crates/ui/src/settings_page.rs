//! Full-page settings route with section navigation and editable settings.

mod control;
pub(crate) mod thread_behavior;

use std::rc::Rc;

use crate::overlay::{DialogButtons, OverlayExt as _};
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::input::{Input, InputEvent, InputState};
use crate::widgets::tabs::{TabBar, TabItem};
use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{InteractiveElementExt as _, v_flex};

use crate::acp_panel::{AcpAgentCard, AcpPanel};
use crate::orchestrate_settings::OrchestrateSettingsPanel;
use crate::plugins_settings::PluginsSettingsPanel;
use crate::provider_card::{ManagePlugins, ProviderCard};
use crate::provider_model_picker::ProviderModelPicker;
use crate::settings::{ImageMode, LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE, ThemeMode};
use crate::store::WorkspaceStore;
use crate::theme::{self, ActiveTheme as _, ThemeMode as UiThemeMode};
use crate::time::{humanize_ago, now_secs};
use crate::typography::TextRole as _;
use crate::window_caption;
use crate::window_drag_area;
use crate::window_state::WindowState;
use tcode_core::project::SessionMeta;
use tcode_core::settings::{FallbackReviewSettings, TitleGenerationSettings};

use crate::settings_rows::{FIELD_WIDTH, SelectRowOption, ToggleRow};

const NAV_WIDTH: f32 = 255.;
/// Archived rows rendered per page. Each row carries two buttons with click
/// listeners, so the whole list is unaffordable once thousands are archived.
const ARCHIVED_PER_PAGE: usize = 50;

/// Clamp `page` so its first row still exists and return `(page, start, end)`
/// for the rows it covers. `total == 0` yields the empty range on page 0.
fn page_bounds(total: usize, page: usize, per_page: usize) -> (usize, usize, usize) {
    let last_page = total.saturating_sub(1) / per_page.max(1);
    let page = page.min(last_page);
    let start = page * per_page;
    (page, start, start.saturating_add(per_page).min(total))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    Threads,
    ProjectThreadRules,
    Providers,
    SourceControl,
    Usage,
    Browser,
    ComputerUse,
    Orchestrate,
    Control,
    Plugins,
    #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
    Remote,
    #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
    Spaces,
    #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
    Group,
    #[cfg(feature = "remote-hosting")]
    Traverse,
    Archived,
    Maintenance,
}

/// Navigation order, shared by the desktop rail and the compact section list.
/// Device-local settings come first; replicated settings for the attached
/// machine follow. Remote, Shared spaces, Machine group and Traverse configure
/// the local desktop listener, or the serving headless listener in a browser;
/// Traverse, the least used, comes last. Choosing a machine lives in
/// `crate::remote`.
#[cfg(feature = "remote-hosting")]
const SECTIONS: [Section; 16] = [
    Section::General,
    Section::Remote,
    Section::Spaces,
    Section::Group,
    Section::Traverse,
    Section::Threads,
    Section::Control,
    Section::Providers,
    Section::SourceControl,
    Section::Usage,
    Section::Orchestrate,
    Section::Plugins,
    Section::Archived,
    Section::ComputerUse,
    Section::Browser,
    Section::Maintenance,
];
#[cfg(all(target_family = "wasm", not(feature = "remote-hosting")))]
const SECTIONS: [Section; 15] = [
    Section::General,
    Section::Remote,
    Section::Spaces,
    Section::Group,
    Section::Threads,
    Section::Control,
    Section::Providers,
    Section::SourceControl,
    Section::Usage,
    Section::Orchestrate,
    Section::Plugins,
    Section::Archived,
    Section::ComputerUse,
    Section::Browser,
    Section::Maintenance,
];
#[cfg(not(any(feature = "remote-hosting", target_family = "wasm")))]
const SECTIONS: [Section; 12] = [
    Section::General,
    Section::Threads,
    Section::Control,
    Section::Providers,
    Section::SourceControl,
    Section::Usage,
    Section::Orchestrate,
    Section::Plugins,
    Section::Archived,
    Section::ComputerUse,
    Section::Browser,
    Section::Maintenance,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionGroup {
    Device,
    Machine,
}

/// The group of the remote sections, which configure the listener of the
/// machine this client runs on: a desktop's own, under This device. A browser
/// has none, so they configure the machine serving it.
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
fn remote_group() -> SectionGroup {
    if cfg!(target_family = "wasm") {
        SectionGroup::Machine
    } else {
        SectionGroup::Device
    }
}

/// The remote sections' views: the machine's hosting switches, invitation
/// and devices; its shared spaces; its group; and on a desktop, its Traverse
/// sources.
#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
struct RemotePanels {
    remote: gpui::AnyView,
    spaces: Entity<crate::remote::SpacesSection>,
    group: Entity<crate::remote::GroupSection>,
    #[cfg(feature = "remote-hosting")]
    traverse: Entity<crate::remote::TraversePanel>,
}

#[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
impl RemotePanels {
    /// A desktop manages its own listener; a browser, the machine serving it,
    /// with that machine's pairing switch on the Remote page.
    fn new(store: &Entity<WorkspaceStore>, window: &mut Window, cx: &mut App) -> Self {
        #[cfg(feature = "remote-hosting")]
        let (remote, resolve): (gpui::AnyView, crate::remote::spaces::Resolve) = (
            cx.new(|cx| crate::remote::HostingPanel::new(store.clone(), window, cx))
                .into(),
            crate::remote::spaces::this_machine,
        );
        #[cfg(not(feature = "remote-hosting"))]
        let (remote, resolve): (gpui::AnyView, crate::remote::spaces::Resolve) = {
            let _ = window;
            let resolve: crate::remote::spaces::Resolve = crate::remote::spaces::for_store;
            (
                cx.new(|_| crate::remote::AccessSection::new(store.clone(), resolve, true))
                    .into(),
                resolve,
            )
        };
        Self {
            remote,
            spaces: cx.new(|_| crate::remote::SpacesSection::new(store.clone(), resolve)),
            group: cx.new(|_| crate::remote::GroupSection::new(store.clone(), resolve)),
            #[cfg(feature = "remote-hosting")]
            traverse: cx.new(|cx| crate::remote::TraversePanel::new(window, cx)),
        }
    }
}

/// Capabilities relevant to Settings applicability. The OS-specific questions
/// are answered by the capability owners; Settings only consumes their result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SettingsCapabilities {
    preview_backend: bool,
    hosting: bool,
    local_permissions: bool,
    remote_attachment: bool,
}

impl SettingsCapabilities {
    #[cfg_attr(not(feature = "remote-hosting"), allow(unused_variables))]
    fn current(store: &WorkspaceStore, cx: &App) -> Self {
        // A desktop whose own host did not start has no listener to configure.
        #[cfg(feature = "remote-hosting")]
        let hosting = cx.has_global::<crate::remote::RemoteController>();
        #[cfg(not(feature = "remote-hosting"))]
        let hosting = cfg!(target_family = "wasm");
        Self {
            preview_backend: crate::preview_panel::preview_backend(),
            hosting,
            local_permissions: cfg!(all(feature = "local-permissions", target_os = "macos")),
            remote_attachment: store.is_remote(),
        }
    }

    fn can_manage_local_permissions(self) -> bool {
        self.local_permissions && !self.remote_attachment
    }

    /// Whether the advanced subgroup starts collapsed. Computer Use and Browser
    /// configure a *screen* — the one the agent drives. A client attached to
    /// another machine, with no computer use of its own, has no screen in the
    /// picture, so those sections fold out of the way instead of leading the
    /// machine's list. The local desktop drives its own screen: expanded.
    fn folds_advanced(self) -> bool {
        self.remote_attachment && !self.local_permissions
    }
}

impl Section {
    fn group(self) -> SectionGroup {
        match self {
            Self::General => SectionGroup::Device,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Remote | Self::Spaces | Self::Group => remote_group(),
            #[cfg(feature = "remote-hosting")]
            Self::Traverse => remote_group(),
            Self::Threads
            | Self::ProjectThreadRules
            | Self::SourceControl
            | Self::Providers
            | Self::Usage
            | Self::Browser
            | Self::ComputerUse
            | Self::Orchestrate
            | Self::Control
            | Self::Plugins
            | Self::Archived
            | Self::Maintenance => SectionGroup::Machine,
        }
    }

    /// Sections that configure the screen and browser the agent drives, and
    /// the machine's upkeep, which share the machine group's collapsible
    /// **Advanced** subgroup.
    fn advanced(self) -> bool {
        matches!(self, Self::ComputerUse | Self::Browser | Self::Maintenance)
    }

    /// A section is navigable when at least one row can do real work for this
    /// client and attachment. Replicated host settings remain applicable over
    /// remote links; the Browser section has only embedded-preview rows.
    fn applies(&self, cx: &SettingsCapabilities) -> bool {
        match self {
            Self::Browser => cx.preview_backend,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Remote | Self::Spaces | Self::Group => cx.hosting,
            #[cfg(feature = "remote-hosting")]
            Self::Traverse => cx.hosting,
            Self::Threads
            | Self::ProjectThreadRules
            | Self::General
            | Self::SourceControl
            | Self::Providers
            | Self::Usage
            | Self::ComputerUse
            | Self::Orchestrate
            | Self::Control
            | Self::Plugins
            | Self::Archived
            | Self::Maintenance => true,
        }
    }

    /// Whether this client may open the section: it [`applies`](Self::applies),
    /// and a shared-space member (`full_scope == false`) manages only this
    /// device's settings and the space's archived threads.
    fn available(self, cx: &SettingsCapabilities, full_scope: bool) -> bool {
        self.applies(cx) && (full_scope || matches!(self, Self::General | Self::Archived))
    }

    fn id(self) -> &'static str {
        match self {
            Self::General => "settings-nav-general",
            Self::Threads => "settings-nav-threads",
            Self::ProjectThreadRules => "settings-nav-project-rules",
            Self::SourceControl => "settings-nav-source-control",
            Self::Providers => "settings-nav-providers",
            Self::Usage => "settings-nav-usage",
            Self::Browser => "settings-nav-browser",
            Self::ComputerUse => "settings-nav-computer-use",
            Self::Orchestrate => "settings-nav-orchestrate",
            Self::Control => "settings-nav-control",
            Self::Plugins => "settings-nav-plugins",
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Remote => "settings-nav-remote",
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Spaces => "settings-nav-spaces",
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Group => "settings-nav-group",
            #[cfg(feature = "remote-hosting")]
            Self::Traverse => "settings-nav-traverse",
            Self::Archived => "settings-nav-archived",
            Self::Maintenance => "settings-nav-maintenance",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::General => IconName::Settings,
            Self::Threads | Self::ProjectThreadRules => IconName::MessageSquare,
            Self::SourceControl => IconName::Github,
            Self::Providers => IconName::Bot,
            Self::Usage => IconName::ChartPie,
            Self::Browser => IconName::Globe,
            Self::ComputerUse => IconName::LayoutDashboard,
            Self::Orchestrate => IconName::Map,
            Self::Control => IconName::Layers,
            Self::Plugins => IconName::Puzzle,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Remote => IconName::Network,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Spaces => IconName::Users,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Group => IconName::HardDrive,
            #[cfg(feature = "remote-hosting")]
            Self::Traverse => IconName::Link,
            Self::Archived => IconName::Inbox,
            Self::Maintenance => IconName::Settings2,
        }
    }

    fn label(self) -> SharedString {
        match self {
            Self::General => crate::tr!("settings.general"),
            Self::Threads | Self::ProjectThreadRules => crate::tr!("settings.threads"),
            Self::SourceControl => crate::tr!("settings.source_control"),
            Self::Providers => crate::tr!("settings.providers"),
            Self::Usage => crate::tr!("settings.usage"),
            Self::Browser => crate::tr!("settings.browser"),
            Self::ComputerUse => crate::tr!("settings.computer_use"),
            Self::Orchestrate => crate::tr!("settings.orchestrate"),
            Self::Control => crate::tr!("control.title"),
            Self::Plugins => crate::tr!("settings.plugins"),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Remote => crate::tr!("settings.remote"),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Spaces => crate::tr!("settings.spaces"),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Self::Group => crate::tr!("settings.group"),
            #[cfg(feature = "remote-hosting")]
            Self::Traverse => crate::tr!("settings.traverse"),
            Self::Archived => crate::tr!("settings.archived"),
            Self::Maintenance => crate::tr!("settings.maintenance"),
        }
        .into_owned()
        .into()
    }
}

/// An input seeded from replicated settings.
///
/// `pushed` is the last value this page wrote into the field, so an echoed
/// change event can be told apart from a real edit; `dirty` then freezes the
/// field against later snapshots so a host update cannot overwrite what the
/// user is in the middle of typing.
/// The remote attachment ceiling as the settings field shows it, in MiB.
fn remote_attachment_limit_value(store: &WorkspaceStore) -> String {
    (store.client_remote_attachment_limit_bytes() / (1024 * 1024)).to_string()
}

struct SettingsInput {
    state: Entity<InputState>,
    pushed: String,
    dirty: bool,
}

impl SettingsInput {
    fn new(state: Entity<InputState>) -> Self {
        Self {
            state,
            pushed: String::new(),
            dirty: false,
        }
    }

    /// `true` when the change came from the user and should be committed.
    fn is_user_edit(&mut self, value: &str) -> bool {
        if value == self.pushed {
            return false;
        }
        self.dirty = true;
        true
    }

    fn push(&mut self, value: String, window: &mut Window, cx: &mut App) {
        self.pushed = value.clone();
        self.state
            .update(cx, |input, cx| input.set_value(value, window, cx));
    }
}

/// Apply a settings theme mode to the live window (shared with the palette's
/// "Toggle theme" action).
pub(crate) fn apply_theme(mode: ThemeMode, window: &mut Window, cx: &mut App) {
    match mode {
        ThemeMode::Light => theme::change_mode(UiThemeMode::Light, Some(window), cx),
        ThemeMode::Dark => theme::change_mode(UiThemeMode::Dark, Some(window), cx),
        ThemeMode::System => theme::sync_system_appearance(Some(window), cx),
    }
}

pub struct SettingsPage {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    /// One card per native profile, keyed by profile id (built-in + user).
    provider_cards: Vec<(String, Entity<ProviderCard>)>,
    provider_card_subscriptions: Vec<Subscription>,
    /// Long-lived state for the modal ACP marketplace and custom form.
    acp_panel: Entity<AcpPanel>,
    /// Editable main-model identities and child-model routing matrix.
    orchestrate_panel: Entity<OrchestrateSettingsPanel>,
    /// Each enabled profile's native plugin catalog.
    plugins_panel: Entity<PluginsSettingsPanel>,
    source_control_panel: Entity<crate::source_control_settings::SourceControlPanel>,
    /// The remote sections. Absent where the client cannot listen at all.
    #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
    remote: RemotePanels,
    /// Shared provider/model picker configured for background thread titles.
    title_model_picker: Entity<ProviderModelPicker>,
    /// Shared provider/model picker configured for fallback reviews.
    fallback_review_model_picker: Entity<ProviderModelPicker>,
    /// Editable name this client presents to machines it connects to.
    device_name_input: SettingsInput,
    /// Stable entities keep expanded state and lazily-created inputs across rerenders.
    acp_cards: Vec<(String, Entity<AcpAgentCard>)>,
    section: Section,
    /// Whether this page holds the store's archived threads.
    archived_held: bool,
    capabilities: SettingsCapabilities,
    /// Whether the machine group's **Advanced** subgroup is open, once the user
    /// has said. `None` keeps the capability-derived default.
    advanced_expanded: Option<bool>,
    /// Latches the one usage refresh fired when Usage becomes the active
    /// section; cleared as soon as the page shows anything else.
    usage_refresh_sent: bool,
    /// Editable "Home URL" for the Browser page; committed on change.
    home_url_input: SettingsInput,
    auto_settle_input: SettingsInput,
    project_rules_editor: Option<Entity<thread_behavior::ProjectRulesEditor>>,
    project_rules_subscription: Option<Subscription>,
    control: control::ControlSection,
    remote_attachment_limit_input: SettingsInput,
    /// Zero-based page shown by the Archived Threads list.
    archived_page: usize,
    /// Whether the editable fields have been seeded from the host's settings.
    /// A portable client renders before its first snapshot arrives, and the
    /// local defaults it starts with must never be shown as the host's answer.
    hydrated: bool,
    /// The native permission group. `Some` only where this build can read TCC
    /// *and* the workspace is this machine's.
    #[cfg(all(feature = "local-permissions", target_os = "macos"))]
    local_permissions: Option<Entity<crate::local_permissions::LocalPermissions>>,
    host_permissions: Option<Entity<crate::host_permissions::HostPermissions>>,
    /// The compact section list and the section content page each keep their
    /// reading position across rerenders.
    section_scroll: ScrollHandle,
    content_scroll: ScrollHandle,
    project_rules_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    fn section_applies(&self, section: &Section, cx: &App) -> bool {
        section.available(&self.capabilities, self.store.read(cx).scope().is_full())
    }
    fn take_requested_section(
        window_state: &Entity<WindowState>,
        cx: &mut Context<Self>,
    ) -> Option<Section> {
        window_state
            .update(cx, |state, _| state.pending_settings_section.take())
            .map(|section| match section.as_str() {
                "providers" => Section::Providers,
                "usage" => Section::Usage,
                "source_control" => Section::SourceControl,
                "browser" => Section::Browser,
                "computer_use" => Section::ComputerUse,
                "orchestrate" => Section::Orchestrate,
                "control" => Section::Control,
                "plugins" => Section::Plugins,
                #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
                "remote" => Section::Remote,
                #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
                "spaces" => Section::Spaces,
                #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
                "group" => Section::Group,
                #[cfg(feature = "remote-hosting")]
                "traverse" => Section::Traverse,
                "archived" => Section::Archived,
                "thread_behavior" | "threads" => Section::Threads,
                "maintenance" => Section::Maintenance,
                _ => Section::General,
            })
    }

    fn return_to_settings_root(window_state: &Entity<WindowState>, cx: &mut Context<Self>) {
        window_state.update(cx, |state, cx| {
            if state.compact
                && state.destination() == crate::window_state::Destination::SettingsSection
            {
                state.back(cx);
            }
        });
    }

    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let capabilities = SettingsCapabilities::current(store.read(cx), cx);
        let title_generation = store.read(cx).settings().title_generation;
        let title_model_picker = cx.new(|cx| {
            ProviderModelPicker::selection(
                store.clone(),
                "title-model-popover",
                "title-model-dropdown",
                title_generation.provider,
                title_generation.model,
                title_generation.profile_id,
                cx,
            )
        });
        let fallback_review = store.read(cx).settings().fallback_review;
        let fallback_review_model_picker = cx.new(|cx| {
            ProviderModelPicker::selection(
                store.clone(),
                "fallback-review-model-popover",
                "fallback-review-model-dropdown",
                fallback_review.provider,
                fallback_review.model,
                fallback_review.profile_id,
                cx,
            )
        });
        let subscriptions = vec![
            cx.observe(&store, |this, _, cx| {
                let selection = this.store.read(cx).settings().title_generation;
                this.title_model_picker.update(cx, |picker, cx| {
                    picker.set_selected(
                        selection.provider,
                        selection.model,
                        selection.profile_id,
                        cx,
                    );
                });
                cx.notify();
            }),
            cx.observe(&store, |this, _, cx| {
                let selection = this.store.read(cx).settings().fallback_review;
                this.fallback_review_model_picker.update(cx, |picker, cx| {
                    picker.set_selected(
                        selection.provider,
                        selection.model,
                        selection.profile_id,
                        cx,
                    );
                });
                cx.notify();
            }),
            cx.observe(&window_state, |this, _, cx| {
                let window_state = this.window_state.clone();
                if let Some(section) = Self::take_requested_section(&window_state, cx) {
                    this.select_section(section, cx);
                }
                cx.notify();
            }),
            cx.observe_in(&store, window, |this, _, window, cx| {
                this.hydrate_inputs(window, cx);
            }),
            cx.subscribe(&title_model_picker, |this, _, event, cx| {
                let selected = event.0.clone();
                this.dispatch_settings(
                    move |store| {
                        store.set_title_generation_provider(selected.provider);
                        store.set_title_generation_model(selected.id);
                        store.set_title_generation_profile_id(selected.profile_id);
                    },
                    cx,
                );
            }),
            cx.subscribe(&fallback_review_model_picker, |this, _, event, cx| {
                let selected = event.0.clone();
                this.dispatch_settings(
                    move |store| {
                        store.set_fallback_review_provider(selected.provider);
                        store.set_fallback_review_model(selected.id);
                        store.set_fallback_review_profile_id(selected.profile_id);
                    },
                    cx,
                );
            }),
        ];

        // Consume relaunch and in-app navigation requests through the same
        // channel used by in-app Settings links.
        let requested_section = Self::take_requested_section(&window_state, cx);
        let full_scope = store.read(cx).scope().is_full();
        let section = requested_section
            .filter(|section| section.available(&capabilities, full_scope))
            .unwrap_or(Section::General);
        if requested_section.is_some_and(|section| !section.available(&capabilities, full_scope)) {
            Self::return_to_settings_root(&window_state, cx);
        }
        let acp_panel = cx.new(|cx| AcpPanel::new(store.clone(), window, cx));
        let orchestrate_panel =
            cx.new(|cx| OrchestrateSettingsPanel::new(store.clone(), window, cx));
        let content_scroll = ScrollHandle::new();
        let plugins_panel = cx.new(|cx| {
            PluginsSettingsPanel::new(
                store.clone(),
                window_state.clone(),
                content_scroll.clone(),
                window,
                cx,
            )
        });
        #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
        let remote = RemotePanels::new(&store, window, cx);
        // Editable fields start empty and are seeded by `hydrate_inputs` once
        // the host's settings actually arrive, so a portable client never shows
        // its local defaults as if they were the host's configuration.
        let home_url_input = cx.new(|cx| InputState::new(window, cx));
        let device_name = store.read(cx).client_device_name();
        let device_name_input =
            cx.new(|cx| InputState::new(window, cx).default_value(device_name.clone()));
        let remote_attachment_limit = remote_attachment_limit_value(store.read(cx));
        let remote_attachment_limit_input = cx.new(|cx| {
            InputState::new(window, cx)
                .step(1.)
                .min(1.)
                .max(u32::MAX as f64)
                .default_value(remote_attachment_limit.clone())
        });
        #[cfg(all(feature = "local-permissions", target_os = "macos"))]
        let local_permissions = capabilities.can_manage_local_permissions().then(|| {
            let store = store.clone();
            cx.new(|cx| crate::local_permissions::LocalPermissions::new(store, window, cx))
        });
        let auto_settle_input = cx.new(|cx| InputState::new(window, cx).step(1.).min(1.).max(90.));
        let source_control_panel = cx.new(|cx| {
            crate::source_control_settings::SourceControlPanel::new(
                store.clone(),
                window_state.clone(),
                cx,
            )
        });
        let mut page = Self {
            store,
            window_state,
            provider_cards: Vec::new(),
            provider_card_subscriptions: Vec::new(),
            acp_panel,
            orchestrate_panel,
            plugins_panel,
            source_control_panel,
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            remote,
            title_model_picker,
            fallback_review_model_picker,
            device_name_input: SettingsInput {
                state: device_name_input.clone(),
                pushed: device_name,
                dirty: false,
            },
            acp_cards: Vec::new(),
            section,
            archived_held: false,
            capabilities,
            advanced_expanded: None,
            usage_refresh_sent: false,
            home_url_input: SettingsInput::new(home_url_input.clone()),
            auto_settle_input: SettingsInput::new(auto_settle_input.clone()),
            project_rules_editor: None,
            project_rules_subscription: None,
            control: control::ControlSection::default(),
            remote_attachment_limit_input: SettingsInput {
                state: remote_attachment_limit_input.clone(),
                pushed: remote_attachment_limit,
                dirty: false,
            },
            archived_page: 0,
            hydrated: false,
            #[cfg(all(feature = "local-permissions", target_os = "macos"))]
            local_permissions,
            host_permissions: None,
            section_scroll: ScrollHandle::new(),
            content_scroll,
            project_rules_scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        page._subscriptions
            .push(cx.subscribe(&home_url_input, |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    if this.home_url_input.is_user_edit(&value) {
                        this.commit_home_url(cx);
                    }
                }
            }));
        page._subscriptions
            .push(cx.subscribe(&device_name_input, |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    if this.device_name_input.is_user_edit(&value) {
                        this.commit_device_name(cx);
                    }
                }
            }));
        page._subscriptions.push(cx.subscribe(
            &remote_attachment_limit_input,
            |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    if this.remote_attachment_limit_input.is_user_edit(&value) {
                        this.commit_remote_attachment_limit(cx);
                    }
                }
            },
        ));

        page._subscriptions.push(cx.subscribe_in(
            &auto_settle_input,
            window,
            |this, input, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.auto_settle_input
                        .is_user_edit(input.read(cx).value().as_ref());
                } else if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    this.commit_auto_settle_days(window, cx);
                }
            },
        ));
        page.build_provider_cards(cx);
        page.sync_acp_cards(window, cx);
        page.hydrate_inputs(window, cx);
        page._subscriptions.push(cx.on_release(|this, cx| {
            if this.archived_held {
                this.store
                    .update(cx, |store, _| store.release_archived_sessions());
            }
        }));
        page.sync_archived(cx);
        page
    }

    /// Seed the editable fields from the host's settings, exactly once, the
    /// first time a real snapshot exists.
    ///
    /// A desktop client already has one at construction; a portable client gets
    /// it milliseconds later. Untouched fields adopt it; a field the user has
    /// already edited keeps that edit, and no later snapshot rewrites either.
    fn hydrate_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.hydrated || !self.store.read(cx).settings_hydrated() {
            return;
        }
        self.hydrated = true;
        let settings = self.store.read(cx).settings();
        if !self.auto_settle_input.dirty {
            self.auto_settle_input.push(
                settings.auto_settle_after_days.unwrap_or(3.).to_string(),
                window,
                cx,
            );
        }
        if !self.home_url_input.dirty {
            let value = settings.browser.home_url.clone().unwrap_or_default();
            self.home_url_input.push(value, window, cx);
        }
    }

    fn commit_home_url(&self, cx: &mut Context<Self>) {
        let value = self
            .home_url_input
            .state
            .read(cx)
            .value()
            .trim()
            .to_string();
        let home_url = (!value.is_empty()).then_some(value);
        self.dispatch_settings(move |store| store.set_browser_home_url(home_url), cx);
    }

    fn commit_device_name(&self, cx: &mut Context<Self>) {
        let value = self
            .device_name_input
            .state
            .read(cx)
            .value()
            .trim()
            .to_string();
        let name = (!value.is_empty()).then_some(value);
        self.dispatch_settings(move |store| store.set_client_device_name(name), cx);
    }

    fn commit_remote_attachment_limit(&self, cx: &mut Context<Self>) {
        let Some(limit) = self
            .remote_attachment_limit_input
            .state
            .read(cx)
            .value()
            .trim()
            .parse::<u32>()
            .ok()
        else {
            return;
        };
        self.dispatch_settings(
            move |store| store.set_client_remote_attachment_limit_mib(Some(limit)),
            cx,
        );
    }

    fn build_provider_cards(&mut self, cx: &mut Context<Self>) {
        let profiles = self.store.read(cx).all_provider_profiles();
        self.provider_cards = profiles
            .into_iter()
            .map(|profile| {
                let store = self.store.clone();
                let kind = profile.kind;
                let id = profile.id.clone();
                let card = cx.new(|cx| ProviderCard::new(store, kind, id, cx));
                (profile.id, card)
            })
            .collect();
        self.provider_card_subscriptions = self
            .provider_cards
            .iter()
            .map(|(_, card)| {
                cx.subscribe(card, |this, _, ManagePlugins(profile_id), cx| {
                    this.select_section(Section::Plugins, cx);
                    let profile_id = profile_id.clone();
                    this.plugins_panel
                        .update(cx, |panel, cx| panel.focus_profile(profile_id, cx));
                })
            })
            .collect();
    }

    /// Reconcile card entities by installed id, preserving editors for agents
    /// that were not installed or removed since the previous render.
    fn sync_acp_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let installed: Vec<_> = self.store.read(cx).settings_installed_acp_agents();
        let mut old = std::mem::take(&mut self.acp_cards);
        self.acp_cards = installed
            .into_iter()
            .map(|agent| {
                let card = old
                    .iter()
                    .position(|(id, _)| id == &agent.id)
                    .map(|index| old.swap_remove(index).1)
                    .unwrap_or_else(|| {
                        let store = self.store.clone();
                        cx.new(|cx| AcpAgentCard::new(store, &agent, window, cx))
                    });
                (agent.id, card)
            })
            .collect();
    }

    fn open_acp_dialog(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.acp_panel
            .update(cx, |panel, cx| panel.prepare_to_open(cx));
        let panel = self.acp_panel.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let panel = panel.clone();
            dialog
                .wide()
                .title(crate::tr!("providers.acp.add_agent").into_owned())
                .content(move |content, _, _| {
                    // The catalog is a GPUI `list`, which needs a definite height, and the
                    // dialog's flex height is not readable from its builder.
                    content.h(px(crate::overlay::DIALOG_LIST_HEIGHT)).child(
                        div()
                            .debug_selector(|| "add-agent-body".into())
                            .size_full()
                            .child(panel.clone()),
                    )
                })
        });
    }

    fn dispatch_settings(&self, intent: impl FnOnce(&mut WorkspaceStore), cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            intent(store);
            store.local_settings_changed(cx);
        });
    }

    /// The index leaves archived threads out; the Archived section holds
    /// them only while it is open.
    fn sync_archived(&mut self, cx: &mut Context<Self>) {
        let open = self.section == Section::Archived;
        if open == self.archived_held {
            return;
        }
        self.archived_held = open;
        self.store.update(cx, |store, cx| {
            if open {
                store.hold_archived_sessions(cx);
            } else {
                store.release_archived_sessions();
            }
        });
    }

    fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.host_permissions = None;
        if !self.section_applies(&section, cx) {
            self.section = Section::General;
            self.sync_archived(cx);
            Self::return_to_settings_root(&self.window_state, cx);
            cx.notify();
            return;
        }
        self.section = section;
        self.sync_archived(cx);
        // Arriving from a deep link or the palette must not leave the selected
        // section hidden inside a folded subgroup.
        if section.advanced() {
            self.advanced_expanded = Some(true);
        }
        // Which section is open is this page's business; *that* a detail is
        // open is navigation, and belongs to the window's one history.
        self.window_state.update(cx, |state, cx| {
            if state.compact {
                state.go(crate::window_state::Destination::SettingsSection, cx);
            }
        });
        cx.notify();
    }

    /// The open section's name, which is also this page's compact nav-bar
    /// title and the label its child's Back control carries.
    pub(crate) fn section_title(&self, cx: &App) -> SharedString {
        if self.section == Section::ProjectThreadRules
            && self.window_state.read(cx).destination()
                != crate::window_state::Destination::SettingsThreadRules
        {
            return Section::Threads.label();
        }
        // The phone page names just the project: the nav bar has no room for
        // the dialog's sentence-long title.
        if self.section == Section::ProjectThreadRules
            && let Some(editor) = &self.project_rules_editor
        {
            return editor.read(cx).project.name.clone().into();
        }
        self.section.label()
    }

    fn nav_item(&self, section: Section, cx: &mut Context<Self>) -> TabItem {
        TabItem::new(section.id(), section.label())
            .icon(section.icon())
            .selected(self.section == section)
            .on_click(cx.listener(move |this, _, _, cx| this.select_section(section, cx)))
    }

    fn group_caption(&self, group: SectionGroup, cx: &Context<Self>) -> AnyElement {
        let (label, selector) = match group {
            SectionGroup::Device => (
                crate::tr!("settings.this_device").into_owned(),
                "settings-device-caption",
            ),
            SectionGroup::Machine => {
                let store = self.store.read(cx);
                let name = if store.is_remote() {
                    store.remote_host_name().map(str::to_owned)
                } else {
                    crate::remote::local_machine_name(cx)
                }
                .filter(|name| !name.trim().is_empty());
                // A member manages none of the machine's settings; what this
                // group holds for it is the space's own threads.
                let label = store.space_label().unwrap_or_else(|| {
                    name.map_or_else(
                        || crate::tr!("settings.machine_settings_fallback").into_owned(),
                        |name| crate::tr!("settings.machine_settings", name = name).into_owned(),
                    )
                });
                (label, "settings-machine-caption")
            }
        };
        crate::material::group_caption(label, cx)
            .debug_selector(move || selector.into())
            .into_any_element()
    }

    fn advanced_expanded(&self) -> bool {
        self.advanced_expanded
            .unwrap_or(!self.capabilities.folds_advanced())
    }

    /// The **Advanced** subgroup's disclosure, shared by the rail and the
    /// compact section list: the same caption style as a group's, plus the
    /// chevron that opens it.
    fn advanced_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let expanded = self.advanced_expanded();
        let spacing = &cx.theme().tokens.spacing;
        div()
            .pl(px(crate::material::CARD_INSET))
            .pt(spacing.sm)
            .pb(spacing.sm)
            .child(
                Button::new("settings-advanced-toggle")
                    .text()
                    .xsmall()
                    .text_caption(cx)
                    .icon(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .label(crate::tr!("settings.advanced"))
                    .aria_expanded(expanded)
                    .debug_selector(|| "settings-advanced-toggle".into())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.advanced_expanded = Some(!this.advanced_expanded());
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// The wide rail's way out of Settings. Compact has no such row: there, the
    /// shell's nav bar carries Back like it does on every other page.
    fn back_row(&self, cx: &mut Context<Self>) -> AnyElement {
        crate::material::bar(
            "settings-back-bar",
            crate::material::Bar::Toolbar,
            false,
            cx,
        )
        .child(
            crate::material::selectable_row(
                crate::material::accessible_clickable(
                    gpui_base::h_flex(),
                    "settings-back",
                    Role::Button,
                    crate::tr!("settings.back"),
                    cx,
                ),
                false,
                false,
                cx,
            )
            .flex_1()
            .text_body(false, cx)
            .text_color(cx.theme().sidebar_foreground)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.window_state
                    .update(cx, |state, cx| state.close_settings(cx));
            }))
            .child(Icon::new(IconName::ArrowLeft).text_color(cx.theme().muted_foreground))
            .child(crate::tr!("settings.back")),
        )
        .into_any_element()
    }

    fn render_nav(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let spacing = cx.theme().tokens.spacing;
        let mut tabs = v_flex().flex_1().min_h_0().px(spacing.sm).gap(spacing.xxs);
        for (index, group) in [SectionGroup::Device, SectionGroup::Machine]
            .into_iter()
            .enumerate()
        {
            let sections = SECTIONS
                .into_iter()
                .filter(|section| section.group() == group && self.section_applies(section, cx))
                .collect::<Vec<_>>();
            if sections.is_empty() {
                continue;
            }
            tabs = tabs.child(
                div()
                    .when(index > 0, |caption| caption.pt(spacing.md))
                    .child(self.group_caption(group, cx)),
            );
            let plain = sections
                .iter()
                .copied()
                .filter(|section| !section.advanced())
                .map(|section| self.nav_item(section, cx))
                .collect::<Vec<_>>();
            tabs = tabs.child(
                TabBar::new(if group == SectionGroup::Device {
                    "settings-nav-tabs"
                } else {
                    "settings-machine-tabs"
                })
                .rail()
                .aria_label(crate::tr!("settings.title"))
                .items(plain),
            );
            if sections.iter().any(|section| section.advanced()) {
                tabs = tabs.child(self.advanced_toggle(cx));
                if self.advanced_expanded() {
                    tabs = tabs.child(
                        TabBar::new("settings-advanced-tabs")
                            .rail()
                            .aria_label(crate::tr!("settings.advanced"))
                            .items(
                                sections
                                    .into_iter()
                                    .filter(|section| section.advanced())
                                    .map(|section| self.nav_item(section, cx))
                                    .collect::<Vec<_>>(),
                            ),
                    );
                }
            }
        }
        v_flex()
            .flex_none()
            .w(px(NAV_WIDTH))
            .h_full()
            .bg(cx.theme().sidebar)
            // Settings replaces the whole window, so this rail is the window's
            // left column: it carries the wordmark and clears the platform's
            // own window controls, exactly as the workspace sidebar does.
            .child(
                window_drag_area(
                    "settings-nav-drag",
                    crate::material::bar(
                        "settings-nav-strip",
                        crate::material::Bar::Strip,
                        false,
                        cx,
                    )
                    .pl(window_caption::traffic_light_inset(window).max(spacing.sm)),
                    window,
                    cx,
                )
                .child(crate::material::brand_wordmark(cx)),
            )
            .child(tabs)
            .child(div().flex_none().child(self.back_row(cx)))
            .into_any_element()
    }

    /// Compact clients have no room for a 255px rail beside the content, so the
    /// same sections become a full-width list that pushes to a detail page.
    /// The page draws no header and no back row: in compact every page wears
    /// the shell's one nav bar (`crate::shell`).
    pub(crate) fn render_compact_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let advanced_expanded = self.advanced_expanded();
        let mut column = v_flex()
            .w_full()
            .p(px(crate::material::COMPACT_PAGE_INSET))
            .gap(crate::material::section_gap(cx));
        for group in [SectionGroup::Device, SectionGroup::Machine] {
            let rows = |advanced: bool, cx: &mut Context<Self>| {
                SECTIONS
                    .into_iter()
                    .filter(|section| {
                        section.group() == group
                            && section.advanced() == advanced
                            && self.section_applies(section, cx)
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|section| self.compact_section_row(section, cx))
                    .collect::<Vec<_>>()
            };
            let plain = rows(false, cx);
            let advanced = rows(true, cx);
            if plain.is_empty() && advanced.is_empty() {
                continue;
            }
            let mut card = v_flex().w_full().child(self.group_caption(group, cx));
            if !plain.is_empty() {
                card = card.child(crate::material::grouped(plain, cx));
            }
            if !advanced.is_empty() {
                card = card
                    .child(self.advanced_toggle(cx))
                    .when(advanced_expanded, |card| {
                        card.child(crate::material::grouped(advanced, cx))
                    });
            }
            column = column.child(card);
        }
        crate::scroll::page_viewport(
            "settings-section-bounce",
            &self.section_scroll,
            div()
                .id("settings-section-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(&self.section_scroll)
                .child(column),
        )
        .into_any_element()
    }

    /// One tappable section row of the compact list.
    fn compact_section_row(&self, section: Section, cx: &mut Context<Self>) -> AnyElement {
        let label = section.label();
        crate::material::list_row(section.id(), label.clone(), cx)
            .debug_selector(move || section.id().into())
            .text_body(true, cx)
            .child(Icon::new(section.icon()).text_color(cx.theme().muted_foreground))
            .child(div().flex_1().child(label))
            .child(Icon::new(IconName::ChevronRight).text_color(cx.theme().muted_foreground))
            .on_click(cx.listener(move |this, _, _, cx| this.select_section(section, cx)))
            .into_any_element()
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // Windows: the settings route replaces the workspace, so this header is
        // the window's top-right corner and hosts the caption buttons. The
        // centered column must keep its position (it aligns with the content
        // below), so the cluster is placed out of flow at the strip's right edge
        // and the column reserves matching trailing room for it — its actions
        // therefore end left of the buttons rather than under them.
        let (right_panel_open, right_tab) = self.store.read(cx).window_caption_state();
        let hosts_caption = window_caption::hosts_caption_for_state(
            window_caption::CaptionSurface::Settings,
            self.window_state.read(cx).route(),
            right_panel_open,
            right_tab,
        );
        // Align the header title and actions with the centered content column.
        window_drag_area(
            "settings-header-drag",
            crate::settings_rows::content_header(
                crate::material::bar("settings-header", crate::material::Bar::Strip, false, cx),
                cx,
            )
            .when(hosts_caption, |strip| strip.relative()),
            window,
            cx,
        )
        .child(
            crate::settings_rows::content_header_column()
                .when(hosts_caption, |column| {
                    column.pr(px(window_caption::CAPTION_CLUSTER_WIDTH))
                })
                // The strip carries no controls, so the whole title doubles as
                // the window's native drag handle.
                .child(window_caption::drag_region(crate::material::bar_title(
                    crate::tr!("settings.title"),
                    cx,
                ))),
        )
        // Painted last so the cluster stays on top of the strip.
        .children(hosts_caption.then(|| {
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .child(window_caption::caption_controls(window, cx))
        }))
        .into_any_element()
    }

    fn confirm_restore(&self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let page = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let page = page.clone();
            alert
                .title(crate::tr!("settings.restore_title"))
                .description(crate::tr!("settings.restore_description"))
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("settings.restore"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    store.update(cx, |store, _cx| {
                        store.reset_settings();
                        store.reset_client_preferences();
                    });
                    // Provider profiles and installed agents survive the reset,
                    // so their cards need no rebuild — only the panels and
                    // inputs holding a copy of a reset preference do.
                    page.update(cx, |page, cx| {
                        let store = page.store.clone();
                        page.orchestrate_panel =
                            cx.new(|cx| OrchestrateSettingsPanel::new(store, window, cx));
                        // The Home URL input now holds a stale override.
                        let home_url = page
                            .store
                            .read(cx)
                            .settings()
                            .browser
                            .home_url
                            .clone()
                            .unwrap_or_default();
                        page.home_url_input.push(home_url, window, cx);
                        let device_name = page.store.read(cx).client_device_name();
                        page.device_name_input.push(device_name, window, cx);
                        let limit = remote_attachment_limit_value(page.store.read(cx));
                        page.remote_attachment_limit_input.push(limit, window, cx);
                    });
                    apply_theme(ThemeMode::System, window, cx);
                    true
                })
        });
    }

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.section_applies(&self.section, cx) {
            self.section = Section::General;
        }
        // Usage is fetched on demand: kick one refresh off on the transition
        // into the section (any entry path — nav click, route key, restore),
        // not on every frame it renders.
        if self.section == Section::Usage {
            if !self.usage_refresh_sent {
                self.usage_refresh_sent = true;
                self.store
                    .update(cx, |store, _cx| store.refresh_provider_usage());
            }
        } else {
            self.usage_refresh_sent = false;
        }
        if self.section != Section::ComputerUse {
            self.host_permissions = None;
        }
        if self.section != Section::Control {
            self.control.hidden();
        }
        if self.section != Section::Plugins {
            self.plugins_panel.update(cx, |panel, _| panel.hide());
        }
        if self.section == Section::SourceControl {
            self.source_control_panel
                .update(cx, |panel, cx| panel.show(cx));
        } else {
            self.source_control_panel
                .update(cx, |panel, _| panel.hide());
        }
        let column = match self.section {
            Section::General => self.render_general(cx),
            Section::Threads => self.render_threads(cx),
            Section::ProjectThreadRules => v_flex().children(self.project_rules_editor.clone()),
            Section::SourceControl => self.render_source_control(cx),
            Section::Providers => self.render_providers(window, cx),
            Section::Usage => self.render_usage(cx),
            Section::Browser => self.render_browser(cx),
            Section::ComputerUse => self.render_computer_use(cx),
            Section::Orchestrate => v_flex().child(self.orchestrate_panel.clone()),
            Section::Control => self.render_control(cx),
            Section::Plugins => v_flex().child(self.plugins_panel.clone()),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Section::Remote => v_flex().child(self.remote.remote.clone()),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Section::Spaces => v_flex().child(self.remote.spaces.clone()),
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            Section::Group => v_flex().child(self.remote.group.clone()),
            #[cfg(feature = "remote-hosting")]
            Section::Traverse => v_flex().child(self.remote.traverse.clone()),
            Section::Archived => self.render_archived(cx),
            Section::Maintenance => self.render_maintenance(cx),
        };
        let scroll = if self.section == Section::ProjectThreadRules {
            &self.project_rules_scroll
        } else {
            &self.content_scroll
        };
        self.render_content_column(column, scroll, cx)
    }

    fn render_content_column(
        &self,
        column: gpui::Div,
        scroll: &ScrollHandle,
        cx: &App,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        crate::scroll::page_viewport(
            "settings-scroll-bounce",
            scroll,
            div()
                .id("settings-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(scroll)
                .child(crate::settings_rows::content_column(column, compact, cx)),
        )
        .into_any_element()
    }

    fn render_general(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let store = self.store.read(cx);
        let settings = store.settings();
        let language_overridden = store.client_language_override().is_some();
        let theme_overridden = store.client_theme_override().is_some();
        let device_name_overridden = store.client_device_name_override().is_some();
        let attachment_limit_overridden = store.client_remote_attachment_limit_override().is_some();
        let provider_marks_reset = crate::settings_rows::reset_action(
            "reset-sidebar-provider-marks",
            settings.sidebar_provider_marks,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_sidebar_provider_marks(false), cx)
            }),
        );
        let appearance = vec![
            self.language_row(settings.language.as_deref(), language_overridden, cx),
            self.theme_row(settings.theme_mode, theme_overridden, cx),
            self.toggle_row(
                "sidebar-provider-marks",
                crate::tr!("settings.sidebar_provider_marks.title"),
                crate::tr!("settings.sidebar_provider_marks.description"),
                settings.sidebar_provider_marks,
                provider_marks_reset,
                cx,
                WorkspaceStore::set_sidebar_provider_marks,
            ),
        ];
        let device = vec![
            self.device_name_row(device_name_overridden, cx),
            self.remote_attachment_limit_row(attachment_limit_overridden, cx),
        ];
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.appearance_section"),
                appearance,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.device_section"),
                device,
                cx,
            ))
    }

    fn render_maintenance(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let throttle_disabled = self
            .store
            .read(cx)
            .settings()
            .inactive_frame_throttle_disabled;
        let frame_throttle_reset = crate::settings_rows::reset_action(
            "reset-inactive-frame-throttle",
            throttle_disabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_inactive_frame_throttle_disabled(false),
                    cx,
                )
            }),
        );
        let performance = vec![self.toggle_row(
            "inactive-frame-throttle",
            crate::tr!("settings.inactive_frame_throttle.title"),
            crate::tr!("settings.inactive_frame_throttle.description"),
            // Stored inverted: checked = enabled.
            !throttle_disabled,
            frame_throttle_reset,
            cx,
            |store, checked| store.set_inactive_frame_throttle_disabled(!checked),
        )];
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.performance_section"),
                performance,
                cx,
            ))
            .child(crate::settings_rows::settings_group(
                crate::tr!("settings.reset_section"),
                vec![self.restore_all_row(cx)],
                cx,
            ))
    }

    /// The whole-settings escape hatch: the per-item buttons cover single
    /// settings, this one covers the rest and keeps its confirm dialog.
    fn restore_all_row(&self, cx: &mut Context<Self>) -> AnyElement {
        crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
            .child(crate::settings_rows::row_labels(
                crate::tr!("settings.reset_all"),
                Some(crate::tr!("settings.restore_description").into()),
                None,
                self.window_state.read(cx).compact,
                cx,
            ))
            .child(
                Button::new("restore-defaults")
                    .danger()
                    .outline()
                    .small()
                    .label(crate::tr!("settings.restore"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.confirm_restore(window, cx);
                    })),
            )
            .into_any_element()
    }

    fn title_generation_row(&self, cx: &mut Context<Self>) -> AnyElement {
        // Provider, model and profile are one setting to the user, so they
        // reset together; the picker follows through the store observer.
        let overridden =
            self.store.read(cx).settings().title_generation != TitleGenerationSettings::default();
        let reset = crate::settings_rows::reset_action(
            "reset-title-generation",
            overridden,
            cx.listener(|this, _, _, cx| {
                let default = TitleGenerationSettings::default();
                this.dispatch_settings(
                    move |store| {
                        store.set_title_generation_provider(default.provider);
                        store.set_title_generation_model(default.model);
                        store.set_title_generation_profile_id(default.profile_id);
                    },
                    cx,
                );
            }),
        );
        crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
            .child(crate::settings_rows::row_labels(
                crate::tr!("settings.title_generation.title"),
                Some(crate::tr!("settings.title_generation.description").into()),
                reset,
                self.window_state.read(cx).compact,
                cx,
            ))
            .child(self.title_model_picker.clone())
            .into_any_element()
    }

    fn fallback_review_model_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let overridden =
            self.store.read(cx).settings().fallback_review != FallbackReviewSettings::default();
        let reset = crate::settings_rows::reset_action(
            "reset-fallback-review-model",
            overridden,
            cx.listener(|this, _, _, cx| {
                let default = FallbackReviewSettings::default();
                this.dispatch_settings(
                    move |store| {
                        store.set_fallback_review_provider(default.provider);
                        store.set_fallback_review_model(default.model);
                        store.set_fallback_review_profile_id(default.profile_id);
                    },
                    cx,
                );
            }),
        );
        crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
            .child(crate::settings_rows::row_labels(
                crate::tr!("settings.fallback_review_model.title"),
                Some(crate::tr!("settings.fallback_review_model.description").into()),
                reset,
                self.window_state.read(cx).compact,
                cx,
            ))
            .child(self.fallback_review_model_picker.clone())
            .into_any_element()
    }

    fn device_name_row(&self, overridden: bool, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let reset = crate::settings_rows::reset_action(
            "reset-device-name",
            overridden,
            cx.listener(|this, _, window, cx| {
                this.dispatch_settings(|store| store.set_client_device_name(None), cx);
                let name = this.store.read(cx).client_device_name();
                this.device_name_input.push(name, window, cx);
            }),
        );
        crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
            .debug_selector(|| "settings-device-name-row".into())
            .child(crate::settings_rows::row_labels(
                crate::tr!("settings.device_name.title"),
                Some(crate::tr!("settings.device_name.description").into()),
                reset,
                self.window_state.read(cx).compact,
                cx,
            ))
            .child(
                div()
                    .when(compact, |field| field.w_full())
                    .when(!compact, |field| field.w(px(FIELD_WIDTH)))
                    .child(
                        Input::new(&self.device_name_input.state)
                            .placeholder(crate::tr!("settings.device_name.placeholder")),
                    ),
            )
            .into_any_element()
    }

    fn remote_attachment_limit_row(&self, overridden: bool, cx: &mut Context<Self>) -> AnyElement {
        let reset = crate::settings_rows::reset_action(
            "reset-remote-attachment-limit",
            overridden,
            cx.listener(|this, _, window, cx| {
                this.dispatch_settings(
                    |store| store.set_client_remote_attachment_limit_mib(None),
                    cx,
                );
                let limit = remote_attachment_limit_value(this.store.read(cx));
                this.remote_attachment_limit_input.push(limit, window, cx);
            }),
        );
        crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
            .debug_selector(|| "settings-remote-attachment-limit-row".into())
            .child(crate::settings_rows::row_labels(
                crate::tr!("settings.remote_attachment_limit.title"),
                Some(crate::tr!("settings.remote_attachment_limit.description").into()),
                reset,
                self.window_state.read(cx).compact,
                cx,
            ))
            .child(
                crate::widgets::input::NumberInput::new(&self.remote_attachment_limit_input.state)
                    .accessibility_label(crate::tr!("settings.remote_attachment_limit.title")),
            )
            .into_any_element()
    }

    fn render_tcode_update_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.store.read(cx).tcode_update_status();
        let version = status.latest.unwrap_or_default();
        let release_url = status.release_url.unwrap_or_default();

        crate::material::overlay_popover("tcode-update-popover", cx)
            .p_3()
            .trigger(
                Button::new("tcode-update-available")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/download.svg"))
                    .label(crate::tr!(
                        "providers.tcode_update_available",
                        version = version
                    ))
                    .tooltip(crate::tr!("providers.tcode_update_aria")),
            )
            .content(move |_, _, cx| {
                let release_url = release_url.clone();
                v_flex()
                    // A reading measure for the message; gpui-base has no popover width.
                    .w(px(320.))
                    .gap_2()
                    .child(
                        div()
                            .text_title(cx)
                            .child(crate::tr!("providers.tcode_update_title")),
                    )
                    .child(
                        div()
                            .text_secondary(false, cx)
                            .child(crate::tr!("providers.tcode_update_message")),
                    )
                    .child(
                        Button::new("view-tcode-release")
                            .primary()
                            .small()
                            .label(crate::tr!("providers.view_release"))
                            .on_click(move |_, _, cx| cx.open_url(&release_url)),
                    )
            })
            .into_any_element()
    }

    /// Settings → Providers: native providers and installed ACP agents share one
    /// bordered list. The marketplace lives behind the Add agent dialog.
    fn render_providers(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        self.sync_acp_cards(window, cx);
        // Reconcile the profile cards with the current profile set: creating or
        // deleting a profile changes it, and the card list must follow (else a
        // deleted profile's card lingers and its Delete looks like a no-op).
        let current_ids: Vec<String> = self
            .store
            .read(cx)
            .all_provider_profiles()
            .into_iter()
            .map(|profile| profile.id)
            .collect();
        let card_ids: Vec<String> = self
            .provider_cards
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        if current_ids != card_ids {
            self.build_provider_cards(cx);
        }
        let checked_at = self.store.read(cx).providers_checked_at();
        let checking = self.store.read(cx).providers_checking();
        let run = self.store.read(cx).provider_update_run();
        let automatic = self.store.read(cx).automatic_provider_updates();
        let compact = self.window_state.read(cx).compact;

        let mut header = gpui_base::h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .pl(px(crate::material::CARD_INSET))
                    .text_caption(cx)
                    .child(crate::tr!("settings.providers_section")),
            );
        if let Some(checked_at) = checked_at {
            let ago = humanize_ago(now_secs().saturating_sub(checked_at));
            header = header.child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("providers.checked", when = ago).into_owned()),
            );
        }
        if self.store.read(cx).tcode_update_status().update_available {
            header = header.child(self.render_tcode_update_popover(cx));
        }
        header = header.child(
            Button::new("add-acp-agent")
                .outline()
                .xsmall()
                .icon(IconName::Plus)
                .label(crate::tr!("providers.acp.add_agent").into_owned())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_acp_dialog(window, cx);
                })),
        );
        if !automatic.is_empty() {
            header = header.child(
                Button::new("update-all-providers")
                    .outline()
                    .xsmall()
                    .loading(run.is_some())
                    .label(crate::tr!("providers.update_all"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.store
                            .update(cx, |store, _cx| store.update_providers(automatic.clone()));
                    })),
            );
        }
        header = header.child(
            Button::new("refresh-providers")
                .ghost()
                .xsmall()
                .loading(checking)
                .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                .tooltip(crate::tr!("providers.refresh"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store.update(cx, |store, _cx| {
                        store.refresh_provider_status();
                        store.check_provider_versions();
                    });
                })),
        );

        let update_checks_disabled = self
            .store
            .read(cx)
            .settings()
            .provider_update_checks_disabled;
        let provider_updates_reset = crate::settings_rows::reset_action(
            "reset-provider-update-checks",
            update_checks_disabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_provider_update_checks_disabled(false), cx)
            }),
        );
        let updates = vec![self.toggle_row(
            "provider-update-checks",
            crate::tr!("settings.provider_updates.title"),
            crate::tr!("settings.provider_updates.description"),
            // Stored inverted: checked = enabled.
            !update_checks_disabled,
            provider_updates_reset,
            cx,
            |store, checked| store.set_provider_update_checks_disabled(!checked),
        )];

        let provider_rows: Vec<AnyElement> = self
            .provider_cards
            .iter()
            .map(|(_, card)| card.clone().into_any_element())
            .collect();

        let mut section = v_flex().w_full().gap_3().child(header);
        if let Some(run) = run {
            section = section.child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .px(px(crate::material::CARD_INSET))
                    .child(
                        div().text_secondary(compact, cx).child(
                            crate::tr!(
                                "providers.update_progress",
                                provider = run
                                    .current
                                    .map(|provider| provider.display_name())
                                    .unwrap_or_default(),
                                completed = run.completed,
                                total = run.total
                            )
                            .into_owned(),
                        ),
                    )
                    .child(
                        crate::widgets::progress::Progress::new("provider-update-progress")
                            .value(100. * run.completed as f32 / run.total.max(1) as f32),
                    ),
            );
        }
        section = section.child(crate::material::grouped(provider_rows, cx));
        for (_, card) in &self.acp_cards {
            section = section.child(card.clone());
        }
        // The update toggle follows the cards it governs; above them its caption
        // would stack straight under the page's own header row.
        section.child(div().pt(crate::material::section_gap(cx)).child(
            crate::settings_rows::settings_group(
                crate::tr!("settings.updates_section"),
                updates,
                cx,
            ),
        ))
    }

    /// Show the rate-limit windows each enabled Codex / Claude Code profile
    /// reports; missing windows are not synthesized.
    fn render_usage(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let store = self.store.read(cx);
        let profiles: Vec<_> = store
            .enabled_profiles()
            .into_iter()
            .filter(|profile| profile.supports_account_usage())
            .collect();
        let rows: Vec<(String, String, agent::ProviderKind, _, bool)> = profiles
            .iter()
            .map(|profile| {
                (
                    profile.id.clone(),
                    store.provider_profile_display_name(&profile.id),
                    profile.kind,
                    store.provider_usage(&profile.id),
                    store.usage_checking(&profile.id),
                )
            })
            .collect();
        let updated_at = rows
            .iter()
            .filter_map(|(_, _, _, usage, _)| usage.as_ref().map(|usage| usage.fetched_at))
            .max();
        let checking = rows.iter().any(|(_, _, _, _, checking)| *checking);
        let compact = self.window_state.read(cx).compact;

        let mut header = gpui_base::h_flex().w_full().items_center().gap_2().child(
            div()
                .flex_1()
                .pl(px(crate::material::CARD_INSET))
                .text_caption(cx)
                .child(crate::tr!("usage.section")),
        );
        if let Some(updated_at) = updated_at {
            let ago = humanize_ago(now_secs().saturating_sub(updated_at));
            header = header.child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("usage.updated", when = ago).into_owned()),
            );
        }
        header = header.child(
            Button::new("refresh-usage")
                .ghost()
                .xsmall()
                .loading(checking)
                .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                .tooltip(crate::tr!("usage.refresh"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.store
                        .update(cx, |store, _cx| store.refresh_provider_usage());
                })),
        );

        let mut section = v_flex().w_full().gap_3().child(header);
        if rows.is_empty() {
            section = section.child(crate::material::grouped(
                vec![self.usage_note_row(crate::tr!("usage.no_profiles").into_owned(), None, cx)],
                cx,
            ));
        }
        for (profile_id, name, kind, usage, checking) in rows {
            let mut card_rows = vec![self.usage_card_header(
                &name,
                kind,
                usage.as_ref().and_then(|u| u.plan.clone()),
                cx,
            )];
            match &usage {
                Some(usage) if usage.error.is_some() || !usage.windows.is_empty() => {
                    if let Some(error) = usage.error.clone() {
                        card_rows.push(self.usage_note_row(
                            crate::tr!("usage.unavailable").into_owned(),
                            Some(error),
                            cx,
                        ));
                    }
                    let now = now_secs();
                    card_rows.extend(
                        usage
                            .windows
                            .iter()
                            .map(|window| self.usage_window_row(window, now, cx)),
                    );
                }
                _ if checking => card_rows.push(self.usage_note_row(
                    crate::tr!("usage.checking").into_owned(),
                    None,
                    cx,
                )),
                _ => card_rows.push(self.usage_note_row(
                    crate::tr!("usage.no_data").into_owned(),
                    None,
                    cx,
                )),
            }
            section = section.child(
                div()
                    .id(SharedString::from(format!("usage-card-{profile_id}")))
                    .child(crate::material::grouped(card_rows, cx)),
            );
        }
        section
    }

    fn usage_card_header(
        &self,
        name: &str,
        kind: agent::ProviderKind,
        plan: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let compact = self.window_state.read(cx).compact;
        crate::material::group_row(cx)
            .text_body(compact, cx)
            .child(crate::provider_card::provider_glyph(kind))
            .child(div().flex_1().child(name.to_owned()))
            .when_some(plan, |row, plan| {
                row.child(crate::material::semantic_chip(
                    crate::usage::plan_label(&plan),
                    cx.theme().muted,
                    muted,
                    cx,
                ))
            })
            .into_any_element()
    }

    fn usage_window_row(
        &self,
        window: &tcode_core::usage::UsageWindow,
        now: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let fill = crate::usage::bar_color(window.used_percent, cx);
        let label = crate::usage::window_label(window);
        crate::material::group_row_stacked(cx)
            .child(
                gpui_base::h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(div().text_caption(cx).child(label.clone()))
                            .when_some(
                                crate::usage::resets_label(window.resets_at, now),
                                |col, label| {
                                    col.child(div().text_secondary(compact, cx).child(label))
                                },
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_meta_mono(compact, cx)
                            .child(crate::usage::percent_label(window.used_percent)),
                    ),
            )
            .child(
                crate::widgets::progress::Progress::new(SharedString::from(format!(
                    "usage-window-{label}"
                )))
                .small()
                .color(fill)
                .value(window.used_percent),
            )
            .into_any_element()
    }

    fn usage_note_row(
        &self,
        label: String,
        detail: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        crate::material::group_row_stacked(cx)
            .text_secondary(compact, cx)
            .child(label)
            .children(detail)
            .into_any_element()
    }

    /// Archived sessions grouped by project with restore and permanent-delete actions.
    fn render_archived(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let full = self.store.read(cx).scope().is_full();
        let groups = self.store.read(cx).archived_groups();
        if groups.is_empty() {
            let loading = self.store.read(cx).archived_loading();
            return v_flex()
                .gap(crate::material::section_gap(cx))
                .child(crate::material::group_caption(
                    crate::tr!("settings.archived_section"),
                    cx,
                ))
                .child(if loading {
                    crate::material::empty_row(crate::tr!("settings.archived_loading"), cx)
                } else {
                    crate::material::empty_state(
                        Icon::new(Section::Archived.icon()),
                        crate::tr!("settings.archived_empty"),
                        crate::tr!("settings.archived_empty_desc"),
                        cx,
                    )
                });
        }

        let flat: Vec<(SharedString, SessionMeta)> = groups
            .into_iter()
            .flat_map(|group| {
                let project_name = SharedString::from(group.project.name);
                group
                    .sessions
                    .into_iter()
                    .map(move |meta| (project_name.clone(), meta))
            })
            .collect();
        let total = flat.len();
        let (page, start, end) = page_bounds(total, self.archived_page, ARCHIVED_PER_PAGE);
        self.archived_page = page;

        let now = now_secs();
        let mut col = v_flex()
            .gap(crate::material::section_gap(cx))
            .child(
                gpui_base::h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_secondary(self.window_state.read(cx).compact, cx)
                            .child(crate::tr!(
                                "settings.archived_range",
                                from = start + 1,
                                to = end,
                                count = total
                            )),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("archived-prev")
                            .outline()
                            .small()
                            .label(crate::tr!("settings.archived_prev"))
                            .disabled(page == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.archived_page = this.archived_page.saturating_sub(1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("archived-next")
                            .outline()
                            .small()
                            .label(crate::tr!("settings.archived_next"))
                            .disabled(end >= total)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.archived_page += 1;
                                cx.notify();
                            })),
                    )
                    .when(full, |row| {
                        row.child(
                            Button::new("archived-delete-all")
                                .danger()
                                .small()
                                .label(crate::tr!("settings.archived_delete_all"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm_delete_all_archived(window, cx);
                                })),
                        )
                    }),
            )
            .child(crate::material::group_caption(
                crate::tr!("settings.archived_section"),
                cx,
            ));
        {
            let mut group_name: Option<SharedString> = None;
            let mut rows: Vec<AnyElement> = Vec::new();
            for (offset, (project_name, meta)) in flat[start..end].iter().enumerate() {
                if group_name.as_ref() != Some(project_name)
                    && let Some(previous) = group_name.replace(project_name.clone())
                {
                    col = col.child(
                        v_flex()
                            .child(crate::material::group_caption(previous, cx))
                            .child(crate::material::group(cx).children(std::mem::take(&mut rows))),
                    );
                }
                let key = start + offset;
                let archived_at = meta.archived_at.unwrap_or(meta.created_at);
                let archived_when = humanize_ago(now.saturating_sub(archived_at));
                let created_when = humanize_ago(now.saturating_sub(meta.created_at));
                let desc = format!(
                    "{} · {}",
                    crate::tr!("settings.archived_at", when = archived_when),
                    crate::tr!("settings.archived_created", when = created_when),
                );
                let id_unarchive = meta.id.clone();
                let id_delete = meta.id.clone();
                let title = meta.title.clone();
                rows.push(
                    crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
                        .child(crate::settings_rows::row_labels(
                            meta.title.clone(),
                            Some(desc.into()),
                            None,
                            self.window_state.read(cx).compact,
                            cx,
                        ))
                        .child(
                            gpui_base::h_flex()
                                .flex_none()
                                .gap_2()
                                .child(
                                    Button::new(("unarchive", key))
                                        .outline()
                                        .small()
                                        .label(crate::tr!("settings.unarchive"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let id = id_unarchive.clone();
                                            this.store.update(cx, |store, _cx| {
                                                store.unarchive_session(id);
                                            });
                                        })),
                                )
                                .when(full, |row| {
                                    row.child(
                                        Button::new(("delete-perm", key))
                                            .danger()
                                            .small()
                                            .label(crate::tr!("settings.delete_permanently"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.confirm_delete_archived(
                                                    &id_delete, &title, window, cx,
                                                );
                                            })),
                                    )
                                }),
                        )
                        .into_any_element(),
                );
            }
            if let Some(previous) = group_name {
                col = col.child(
                    v_flex()
                        .child(crate::material::group_caption(previous, cx))
                        .child(crate::material::group(cx).children(rows)),
                );
            }
        }
        col
    }

    fn confirm_delete_archived(
        &self,
        session_id: &str,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::sidebar::confirm_delete(
            self.store.clone(),
            session_id.to_string(),
            title.to_string(),
            crate::tr!("settings.delete_permanently").into(),
            window,
            cx,
        );
    }

    /// Bulk permanent delete. Always confirms, whatever
    /// `skip_delete_confirmation` says — this destroys every archived thread.
    fn confirm_delete_all_archived(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(deletion) = self.store.read(cx).archived_deletion() else {
            return;
        };
        let (title, description) = if deletion.unarchived == 0 {
            (
                crate::tr!(
                    "settings.archived_delete_all_title",
                    count = deletion.archived
                ),
                crate::tr!("settings.archived_delete_all_description"),
            )
        } else {
            (
                crate::tr!(
                    "settings.archived_delete_all_tree_title",
                    count = deletion.archived,
                    unarchived = deletion.unarchived
                ),
                crate::tr!(
                    "settings.archived_delete_all_tree_description",
                    total = deletion.archived + deletion.unarchived
                ),
            )
        };
        let store = self.store.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let deletion = deletion.clone();
            alert
                .title(title.clone())
                .description(description.clone())
                .button_props(
                    DialogButtons::default()
                        .ok_variant(ButtonVariant::Danger)
                        .ok_text(crate::tr!("settings.delete_permanently"))
                        .cancel_text(crate::tr!("settings.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    let deletion = deletion.clone();
                    store.update(cx, |store, _cx| store.delete_archived(deletion));
                    true
                })
        });
    }

    fn render_computer_use(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let compact = self.window_state.read(cx).compact;
        let settings = self.store.read(cx).settings();
        let enabled_reset = crate::settings_rows::reset_action(
            "reset-cu-enabled",
            settings.computer_use.enabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_enabled(false), cx)
            }),
        );
        let allow_input_reset = crate::settings_rows::reset_action(
            "reset-cu-allow-input",
            !settings.computer_use.allow_input,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_allow_input(true), cx)
            }),
        );
        let allow_foreground_fallback_reset = crate::settings_rows::reset_action(
            "reset-cu-allow-foreground-fallback",
            settings.computer_use.allow_foreground_fallback,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_computer_use_allow_foreground_fallback(false),
                    cx,
                )
            }),
        );
        let show_agent_cursor_reset = crate::settings_rows::reset_action(
            "reset-cu-show-agent-cursor",
            !settings.computer_use.show_agent_cursor,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_computer_use_show_agent_cursor(true), cx)
            }),
        );
        let rows = vec![
            self.toggle_row(
                "cu-enabled",
                crate::tr!("computer_use.enable.title"),
                crate::tr!("computer_use.enable.description"),
                settings.computer_use.enabled,
                enabled_reset,
                cx,
                WorkspaceStore::set_computer_use_enabled,
            ),
            self.image_mode_row(settings.computer_use.image_mode, cx),
            self.toggle_row(
                "cu-allow-input",
                crate::tr!("computer_use.allow_input.title"),
                crate::tr!("computer_use.allow_input.description"),
                settings.computer_use.allow_input,
                allow_input_reset,
                cx,
                WorkspaceStore::set_computer_use_allow_input,
            ),
            self.toggle_row(
                "cu-allow-foreground-fallback",
                crate::tr!("computer_use.allow_foreground_fallback.title"),
                crate::tr!("computer_use.allow_foreground_fallback.description"),
                settings.computer_use.allow_foreground_fallback,
                allow_foreground_fallback_reset,
                cx,
                WorkspaceStore::set_computer_use_allow_foreground_fallback,
            ),
            self.toggle_row(
                "cu-show-agent-cursor",
                crate::tr!("computer_use.show_agent_cursor.title"),
                crate::tr!("computer_use.show_agent_cursor.description"),
                settings.computer_use.show_agent_cursor,
                show_agent_cursor_reset,
                cx,
                WorkspaceStore::set_computer_use_show_agent_cursor,
            ),
        ];
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(
                v_flex()
                    .child(crate::material::group_caption(
                        crate::tr!("computer_use.section"),
                        cx,
                    ))
                    .child(crate::material::group(cx).children(rows))
                    .children(
                        agent::ProviderKind::NATIVE
                            .into_iter()
                            .chain([agent::ProviderKind::Acp])
                            .filter(|kind| kind.caps().native_computer_use)
                            .map(|kind| {
                                div()
                                    .pt(cx.theme().tokens.spacing.sm)
                                    .pl(px(crate::material::CARD_INSET))
                                    .text_secondary(compact, cx)
                                    .child(crate::tr!(
                                        "providers.native_computer_use",
                                        name = kind.display_name()
                                    ))
                            }),
                    ),
            )
            .child(self.permissions_group(cx))
    }

    fn render_browser(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let settings = self.store.read(cx).settings();
        let enabled_reset = crate::settings_rows::reset_action(
            "reset-browser-enabled",
            !settings.browser.enabled,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_browser_enabled(true), cx)
            }),
        );
        let allow_eval_reset = crate::settings_rows::reset_action(
            "reset-browser-allow-eval",
            !settings.browser.allow_evaluate,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_browser_allow_evaluate(true), cx)
            }),
        );
        let rows = vec![
            self.toggle_row(
                "browser-enabled",
                crate::tr!("browser.enable.title"),
                crate::tr!("browser.enable.description"),
                settings.browser.enabled,
                enabled_reset,
                cx,
                WorkspaceStore::set_browser_enabled,
            ),
            self.home_url_row(cx),
            self.toggle_row(
                "browser-allow-eval",
                crate::tr!("browser.allow_evaluate.title"),
                crate::tr!("browser.allow_evaluate.description"),
                settings.browser.allow_evaluate,
                allow_eval_reset,
                cx,
                WorkspaceStore::set_browser_allow_evaluate,
            ),
        ];
        v_flex().gap(crate::material::section_gap(cx)).child(
            v_flex()
                .child(crate::material::group_caption(
                    crate::tr!("browser.section"),
                    cx,
                ))
                .child(crate::material::group(cx).children(rows)),
        )
    }

    fn home_url_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        let overridden = self.store.read(cx).settings().browser.home_url.is_some();
        let reset = crate::settings_rows::reset_action(
            "reset-browser-home-url",
            overridden,
            cx.listener(|this, _, window, cx| {
                this.dispatch_settings(|store| store.set_browser_home_url(None), cx);
                this.home_url_input.push(String::new(), window, cx);
            }),
        );
        crate::settings_rows::row_frame(self.window_state.read(cx).compact, cx)
            .child(crate::settings_rows::row_labels(
                crate::tr!("browser.home_url.title"),
                Some(crate::tr!("browser.home_url.description").into()),
                reset,
                self.window_state.read(cx).compact,
                cx,
            ))
            .child(
                div()
                    .when(compact, |field| field.w_full())
                    .when(!compact, |field| field.w(px(FIELD_WIDTH)))
                    .child(
                        Input::new(&self.home_url_input.state)
                            .placeholder(crate::tr!("browser.home_url.placeholder")),
                    ),
            )
            .into_any_element()
    }

    fn image_mode_row(&self, mode: ImageMode, cx: &mut Context<Self>) -> AnyElement {
        let reset = crate::settings_rows::reset_action(
            "reset-cu-image-mode",
            mode != ImageMode::Auto,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_computer_use_image_mode(ImageMode::Auto),
                    cx,
                );
            }),
        );
        let label = match mode {
            ImageMode::Auto => crate::tr!("computer_use.image_mode.auto"),
            ImageMode::Always => crate::tr!("computer_use.image_mode.always"),
            ImageMode::Never => crate::tr!("computer_use.image_mode.never"),
        };
        let option = |value, label_key: &'static str, desc_key: &'static str| SelectRowOption {
            value,
            id: label_key.into(),
            label: crate::tr!(label_key).into_owned().into(),
            description: Some(crate::tr!(desc_key).into_owned().into()),
            selected: value == mode,
        };
        crate::settings_rows::select_row(
            "cu-image-mode-dropdown",
            "cu-image-mode-popover",
            "cu-image-mode-menu",
            260.,
            crate::tr!("computer_use.image_mode.title")
                .into_owned()
                .into(),
            crate::tr!("computer_use.image_mode.description")
                .into_owned()
                .into(),
            label.into_owned().into(),
            vec![
                option(
                    ImageMode::Auto,
                    "computer_use.image_mode.auto",
                    "computer_use.image_mode.auto_desc",
                ),
                option(
                    ImageMode::Always,
                    "computer_use.image_mode.always",
                    "computer_use.image_mode.always_desc",
                ),
                option(
                    ImageMode::Never,
                    "computer_use.image_mode.never",
                    "computer_use.image_mode.never_desc",
                ),
            ],
            reset,
            self.window_state.read(cx).compact,
            {
                let page = cx.entity();
                move |mode, _, cx| {
                    page.update(cx, |page, cx| {
                        page.dispatch_settings(|store| store.set_computer_use_image_mode(mode), cx)
                    })
                }
            },
            cx,
        )
    }

    /// Status belongs to the attached host; native grant controls stay local.
    fn permissions_group(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let column = v_flex().child(crate::material::group_caption(
            crate::tr!("computer_use.permissions_section"),
            cx,
        ));
        if self.capabilities.can_manage_local_permissions()
            && let Some(rows) = self.local_permission_rows()
        {
            return column.child(rows).into_any_element();
        }
        let rows = self.host_permissions.get_or_insert_with(|| {
            cx.new(|cx| crate::host_permissions::HostPermissions::new(self.store.clone(), cx))
        });
        column.child(rows.clone()).into_any_element()
    }

    #[cfg(all(feature = "local-permissions", target_os = "macos"))]
    fn local_permission_rows(&self) -> Option<AnyElement> {
        self.local_permissions
            .clone()
            .map(gpui::IntoElement::into_any_element)
    }

    #[cfg(not(all(feature = "local-permissions", target_os = "macos")))]
    fn local_permission_rows(&self) -> Option<AnyElement> {
        None
    }

    fn render_source_control(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let remove_credits = self.store.read(cx).settings().remove_agent_credits_on_merge;
        let reset = crate::settings_rows::reset_action(
            "reset-remove-agent-credits",
            remove_credits,
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(|store| store.set_remove_agent_credits_on_merge(false), cx)
            }),
        );
        let row = self.toggle_row(
            "remove-agent-credits",
            crate::tr!("source_control.remove_agent_credits"),
            crate::tr!("source_control.remove_agent_credits_description"),
            remove_credits,
            reset,
            cx,
            WorkspaceStore::set_remove_agent_credits_on_merge,
        );
        v_flex()
            .gap(crate::material::section_gap(cx))
            .child(self.source_control_panel.clone())
            .child(crate::material::group(cx).child(row))
    }

    /// A [`ToggleRow`] for one local setting, written through `intent`.
    #[allow(clippy::too_many_arguments)]
    fn toggle_row(
        &self,
        id: &'static str,
        title: impl Into<SharedString>,
        desc: impl Into<SharedString>,
        checked: bool,
        reset: Option<AnyElement>,
        cx: &mut Context<Self>,
        intent: fn(&mut WorkspaceStore, bool),
    ) -> AnyElement {
        let compact = self.window_state.read(cx).compact;
        ToggleRow::new(
            id,
            title,
            checked,
            compact,
            cx.listener(move |this, checked: &bool, _, cx| {
                this.dispatch_settings(|store| intent(store, *checked), cx);
            }),
        )
        .description(desc)
        .reset(reset)
        .into_any_element()
    }

    fn theme_row(&self, mode: ThemeMode, overridden: bool, cx: &mut Context<Self>) -> AnyElement {
        let reset = crate::settings_rows::reset_action(
            "reset-theme",
            overridden,
            cx.listener(|this, _, window, cx| {
                this.dispatch_settings(|store| store.set_client_theme(None), cx);
                let fallback = this.store.read(cx).settings().theme_mode;
                apply_theme(fallback, window, cx);
            }),
        );
        let label = match mode {
            ThemeMode::System => crate::tr!("settings.theme.system"),
            ThemeMode::Light => crate::tr!("settings.theme.light"),
            ThemeMode::Dark => crate::tr!("settings.theme.dark"),
        };
        let option = |value, id: &'static str, label_key: &'static str| SelectRowOption {
            value,
            id: id.into(),
            label: crate::tr!(label_key).into_owned().into(),
            description: None,
            selected: value == mode,
        };
        crate::settings_rows::select_row(
            "theme-dropdown",
            "theme-popover",
            "theme-options-menu",
            160.,
            crate::tr!("settings.theme.title").into_owned().into(),
            crate::tr!("settings.theme.description").into_owned().into(),
            label.into_owned().into(),
            vec![
                option(
                    ThemeMode::System,
                    "theme-option-system",
                    "settings.theme.system",
                ),
                option(
                    ThemeMode::Light,
                    "theme-option-light",
                    "settings.theme.light",
                ),
                option(ThemeMode::Dark, "theme-option-dark", "settings.theme.dark"),
            ],
            reset,
            self.window_state.read(cx).compact,
            {
                let page = cx.entity();
                move |mode, window, cx| {
                    page.update(cx, |page, cx| {
                        page.dispatch_settings(|store| store.set_client_theme(Some(mode)), cx)
                    });
                    apply_theme(mode, window, cx);
                }
            },
            cx,
        )
    }

    fn language_row(
        &self,
        language: Option<&str>,
        overridden: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = language.map(str::to_owned);
        let reset = crate::settings_rows::reset_action(
            "reset-language",
            overridden,
            cx.listener(|this, _, window, cx| {
                this.dispatch_settings(|store| store.set_client_language(None), cx);
                let fallback = this.store.read(cx).settings().language;
                crate::settings::apply_locale(fallback.as_deref());
                window.refresh();
            }),
        );
        let label = match language {
            Some(LANGUAGE_ENGLISH) => crate::tr!("settings.language.english"),
            Some(LANGUAGE_SIMPLIFIED_CHINESE) => crate::tr!("settings.language.chinese"),
            _ => crate::tr!("settings.language.system"),
        };
        let option = |value, key: &'static str| SelectRowOption {
            value,
            id: key.into(),
            label: crate::tr!(key).into_owned().into(),
            description: None,
            selected: selected.as_deref() == value,
        };
        crate::settings_rows::select_row(
            "language-dropdown",
            "language-popover",
            "language-options-menu",
            160.,
            crate::tr!("settings.language.title").into_owned().into(),
            crate::tr!("settings.language.description")
                .into_owned()
                .into(),
            label.into_owned().into(),
            vec![
                option(None, "settings.language.system"),
                option(Some(LANGUAGE_ENGLISH), "settings.language.english"),
                option(
                    Some(LANGUAGE_SIMPLIFIED_CHINESE),
                    "settings.language.chinese",
                ),
            ],
            reset,
            self.window_state.read(cx).compact,
            {
                let page = cx.entity();
                move |language, window, cx| {
                    page.update(cx, |page, cx| {
                        let preference = Some(language.unwrap_or("system").to_owned());
                        page.dispatch_settings(|store| store.set_client_language(preference), cx);
                        let effective = page.store.read(cx).settings().language;
                        crate::settings::apply_locale(effective.as_deref());
                    });
                    window.refresh();
                }
            },
            cx,
        )
    }
}

impl SettingsPage {
    /// One section's content, as the compact detail page's body.
    pub(crate) fn render_compact_section(
        &mut self,
        destination: crate::window_state::Destination,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // SettingsSection and its project-rules child share SettingsPage. Render
        // the real parent controls without mounting the child editor twice or
        // changing the selection. Each page retains its own scroll handle.
        if destination == crate::window_state::Destination::SettingsSection
            && self.window_state.read(cx).destination()
                == crate::window_state::Destination::SettingsThreadRules
        {
            let column = self.render_threads(cx);
            return self.render_content_column(column, &self.content_scroll, cx);
        }
        if self.section == Section::ProjectThreadRules
            && self.window_state.read(cx).destination()
                != crate::window_state::Destination::SettingsThreadRules
        {
            self.section = Section::Threads;
            self.project_rules_editor = None;
        }
        self.render_content(window, cx)
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // No opaque full-page fill: the nav must sit on the same translucent
        // glass canvas the chat sidebar does (its `sidebar` token shows the
        // T0 blur through its own translucency), so navigating chat↔settings
        // never flips the window material. Only the content column is paper.
        gpui_base::h_flex()
            .size_full()
            .text_color(cx.theme().foreground)
            .child(self.render_nav(window, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(crate::material::content_surface(cx))
                    .shadow_sm()
                    .child(self.render_header(window, cx))
                    .child(self.render_content(window, cx)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext, size};
    use tcode_protocol::SettingsPatch;
    use tcode_runtime::pipe::{HostServices, spawn_host};
    use tcode_services::store::SessionStore;

    use super::*;
    use crate::store::WorkspaceAttachment;

    fn capabilities(
        preview_backend: bool,
        hosting: bool,
        local_permissions: bool,
        remote_attachment: bool,
    ) -> SettingsCapabilities {
        SettingsCapabilities {
            preview_backend,
            hosting,
            local_permissions,
            remote_attachment,
        }
    }

    #[test]
    fn archived_page_bounds_clamp_to_the_last_populated_page() {
        assert_eq!(page_bounds(0, 3, 50), (0, 0, 0), "empty list has no pages");
        assert_eq!(page_bounds(100, 1, 50), (1, 50, 100), "exact multiple");
        assert_eq!(page_bounds(120, 2, 50), (2, 100, 120), "last partial page");
        assert_eq!(
            page_bounds(120, 9, 50),
            (2, 100, 120),
            "a page past the end shows the last page instead of an empty one"
        );
    }

    #[gpui::test]
    fn archived_delete_uses_response_worktree_sharing_and_preserves_the_cleanup_choice(
        cx: &mut TestAppContext,
    ) {
        use tcode_protocol::{
            ArchivedSessions, ClientPayload, Command, HostMessage, Query, QueryResponse,
            decode_client_line, encode_line,
        };
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_ENGLISH));
        cx.update(crate::theme::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, false, cx)
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, _| store.select_session("live".into()));
        let window_state = cx.new(|_| WindowState::new(false));
        let mut page = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
            page = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let page = page.unwrap();
        page.update(cx, |page, cx| page.select_section(Section::Archived, cx));
        cx.run_until_parked();
        let mut archived_query = None;
        while let Ok(line) = outgoing.try_recv() {
            let message = decode_client_line(&line).unwrap();
            match message.payload {
                ClientPayload::Query(Query::ArchivedSessions) => archived_query = Some(message.id),
                ClientPayload::Command(_) => incoming
                    .try_send(
                        encode_line(&HostMessage::Ack {
                            id: message.id,
                            result: Ok(tcode_protocol::CommandResponse::Unit),
                        })
                        .unwrap(),
                    )
                    .unwrap(),
                _ => {}
            }
        }
        let sessions = ["shared", "remove", "keep"]
            .into_iter()
            .map(|id| {
                let mut meta = tcode_core::project::SessionMeta::new(
                    agent::ProviderKind::Codex,
                    std::path::PathBuf::from(format!("/archive/worktree/{id}")),
                    None,
                );
                meta.id = id.into();
                meta.title = id.into();
                meta.archived_at = Some(1);
                meta.worktree = Some(tcode_core::project::WorktreeInfo {
                    root_project_path: "/archive/project".into(),
                    base: "main".into(),
                    branch: format!("tcode/archive/{id}"),
                });
                meta
            })
            .collect();
        incoming
            .try_send(
                encode_line(&HostMessage::QueryResult {
                    id: archived_query.expect("opening Archived queries the host"),
                    result: Ok(QueryResponse::ArchivedSessions(ArchivedSessions {
                        sessions,
                        worktree_shared: std::collections::HashSet::from(["shared".into()]),
                        revision: 0,
                    })),
                })
                .unwrap(),
            )
            .unwrap();
        cx.run_until_parked();
        for (id, cleanup, remove_worktree) in [
            ("shared", false, false),
            ("remove", true, true),
            ("keep", true, false),
        ] {
            cx.update(|window, cx| {
                page.update(cx, |page, cx| {
                    page.confirm_delete_archived(id, id, window, cx)
                });
                _ = window.draw(cx);
                window.dispatch_action(
                    Box::new(gpui_base::actions::Confirm { secondary: false }),
                    cx,
                );
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                _ = window.draw(cx);
            });
            if cleanup {
                assert!(outgoing.is_empty(), "delete waits for the cleanup choice");
                cx.update(|window, cx| {
                    if remove_worktree {
                        window.dispatch_action(
                            Box::new(gpui_base::actions::Confirm { secondary: false }),
                            cx,
                        );
                    } else {
                        window.dispatch_action(Box::new(gpui_base::actions::Cancel), cx);
                    }
                });
                cx.run_until_parked();
            }
            let request =
                decode_client_line(&outgoing.try_recv().expect("delete command")).unwrap();
            assert_eq!(
                request.payload,
                ClientPayload::Command(Command::DeleteSession {
                    session_id: id.into(),
                    remove_worktree,
                })
            );
            incoming
                .try_send(
                    encode_line(&HostMessage::Ack {
                        id: request.id,
                        result: Ok(tcode_protocol::CommandResponse::Unit),
                    })
                    .unwrap(),
                )
                .unwrap();
            cx.run_until_parked();
        }
    }

    struct CompactSettingsProbe {
        page: Entity<SettingsPage>,
    }

    impl Render for CompactSettingsProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.page
                .update(cx, |page, cx| page.render_compact_list(cx))
        }
    }

    #[gpui::test]
    fn add_agent_dialog_keeps_catalog_and_custom_action_inside(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("agent-dialog");
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .unwrap();
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                host.link(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let mut page_handle = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let page = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
            page_handle = Some(page.clone());
            gpui_base::Root::new(page, window, cx)
        });
        cx.update(|window, cx| {
            page_handle
                .unwrap()
                .update(cx, |page, cx| page.open_acp_dialog(window, cx));
        });
        for (width, height) in [(1000., 780.), (393., 600.), (360., 480.)] {
            cx.simulate_resize(size(px(width), px(height)));
            cx.run_until_parked();
            cx.update(|window, cx| {
                _ = window.draw(cx);
            });
            let body = cx.debug_bounds("add-agent-body").unwrap();
            let custom = cx.debug_bounds("acp-custom-open").unwrap();
            assert!(
                custom.bottom() <= body.bottom(),
                "custom action {custom:?} exceeds body {body:?}"
            );
            assert!(custom.right() <= body.right());
            assert!(body.bottom() < px(height));
        }
        crate::store::tests::shutdown_test_host(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn compact_list_groups_sections_omits_inapplicable_and_folds_advanced(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("settings-sections");
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .expect("spawn settings test host");
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                host.link(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        let (probe, cx) = cx.add_window_view(|window, cx| {
            let page = cx.new(|cx| {
                let mut page = SettingsPage::new(store.clone(), window_state.clone(), window, cx);
                page.capabilities = capabilities(false, false, false, true);
                page
            });
            CompactSettingsProbe { page }
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(393.), px(852.)));
        // The navigation consumes capability values supplied by platform
        // owners. Exercise each policy row through its rendered section list;
        // local permission eligibility is a policy check, not an OS grant test.
        for (capabilities, local_permissions, folded, browser, remote) in [
            (
                capabilities(true, true, true, false),
                true,
                false,
                true,
                true,
            ),
            (
                capabilities(true, true, true, true),
                false,
                false,
                true,
                true,
            ),
            (
                capabilities(false, false, false, true),
                false,
                true,
                false,
                false,
            ),
        ] {
            probe.update(cx, |probe, cx| {
                probe.page.update(cx, |page, cx| {
                    page.capabilities = capabilities;
                    page.advanced_expanded = None;
                    assert_eq!(
                        page.capabilities.can_manage_local_permissions(),
                        local_permissions
                    );
                    cx.notify();
                });
                cx.notify();
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.refresh();
                _ = window.draw(cx);
            });
            for selector in [
                "settings-device-caption",
                "settings-machine-caption",
                "settings-nav-general",
                "settings-nav-threads",
                "settings-nav-providers",
                "settings-nav-usage",
                "settings-nav-orchestrate",
                "settings-nav-archived",
            ] {
                assert!(
                    cx.debug_bounds(selector).is_some(),
                    "{capabilities:?}: {selector}"
                );
            }
            assert_eq!(cx.debug_bounds("settings-nav-browser").is_some(), browser);
            #[cfg(any(feature = "remote-hosting", target_family = "wasm"))]
            assert_eq!(cx.debug_bounds("settings-nav-remote").is_some(), remote);
            #[cfg(not(any(feature = "remote-hosting", target_family = "wasm")))]
            let _ = remote;
            assert!(
                cx.debug_bounds("settings-advanced-toggle").is_some(),
                "{capabilities:?}: advanced disclosure remains available in either state"
            );
            for advanced in ["settings-nav-computer-use", "settings-nav-maintenance"] {
                assert_eq!(cx.debug_bounds(advanced).is_some(), !folded, "{advanced}");
            }
            if folded {
                let toggle = cx.debug_bounds("settings-advanced-toggle").unwrap();
                cx.simulate_click(toggle.center(), gpui::Modifiers::default());
                cx.run_until_parked();
                cx.update(|window, cx| {
                    window.refresh();
                    _ = window.draw(cx);
                });
                assert!(cx.debug_bounds("settings-nav-computer-use").is_some());
                assert!(cx.debug_bounds("settings-nav-maintenance").is_some());
                assert!(cx.debug_bounds("settings-nav-browser").is_none());
            }
        }

        // A stale palette/deep-link target is rejected by the same predicate
        // and returns a compact detail route to the root list.
        window_state.update(cx, |state, cx| {
            state.enter_workspace(cx);
            state.open_settings(cx);
            state.go(crate::window_state::Destination::SettingsSection, cx);
            state.pending_settings_section = Some("browser".into());
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            window_state.read_with(cx, |state, _| state.destination()),
            crate::window_state::Destination::Settings
        );

        crate::store::tests::shutdown_test_host(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A client that renders before its host answers must not present its own
    /// defaults as the host's configuration, must adopt the host's values the
    /// moment they arrive, and must never overwrite what the user has since
    /// typed — no matter how many snapshots follow.
    #[gpui::test]
    fn editable_settings_wait_for_the_host_and_then_keep_the_user_edit(cx: &mut TestAppContext) {
        let root = crate::store::tests::scratch_root("settings-hydration");
        let host = spawn_host(
            SessionStore::open_at(root.clone()).unwrap(),
            HostServices::default(),
        )
        .expect("spawn settings test host");
        smol::block_on(host.update_state_for_test(|state, _| {
            state.settings.browser.home_url = Some("https://host.example".into());
        }))
        .expect("seed host settings");

        // No blocking seed: this is the portable client's timeline, where the
        // first snapshot lands after the page already exists.
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                host.link(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        let window_state = cx.new(|_| WindowState::new(false));
        let (page, cx) = cx.add_window_view(|window, cx| {
            SettingsPage::new(store.clone(), window_state.clone(), window, cx)
        });
        let cx: &mut VisualTestContext = cx;

        page.read_with(cx, |page, cx| {
            assert!(!page.hydrated, "nothing has arrived from the host yet");
            assert_eq!(
                page.home_url_input.state.read(cx).value(),
                "",
                "an unhydrated field must stay empty rather than show a local default"
            );
        });

        // The host answers from its own thread; keep draining until the page
        // hydrates rather than a fixed number of turns, so a slow CI runner
        // cannot race the first snapshot.
        let drain = |cx: &mut VisualTestContext| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
                cx.run_until_parked();
                if page.read_with(cx, |page, _| page.hydrated)
                    || std::time::Instant::now() >= deadline
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        drain(cx);

        page.read_with(cx, |page, cx| {
            assert!(page.hydrated);
            assert_eq!(
                page.home_url_input.state.read(cx).value(),
                "https://host.example"
            );
        });

        // The user edits the field: `replace_all` takes the same path typing
        // does, emitting the change the page listens for.
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                page.home_url_input.state.update(cx, |input, cx| {
                    input.replace_all("https://edited", window, cx)
                });
            });
        });
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert!(page.home_url_input.dirty, "a typed value is a user edit");
        });

        crate::store::tests::await_with_events(
            cx,
            &store,
            host.link().command(tcode_protocol::Command::PatchSettings {
                patch: SettingsPatch::BrowserHomeUrl(Some("https://elsewhere.example".into())),
            }),
        )
        .expect("patch settings");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
            cx.run_until_parked();
            if store.read_with(cx, |store, _| {
                store.settings().browser.home_url.as_deref() == Some("https://elsewhere.example")
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "later settings snapshot did not arrive"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        page.read_with(cx, |page, cx| {
            assert_eq!(
                page.home_url_input.state.read(cx).value(),
                "https://edited",
                "a later snapshot must not clobber the field the user is editing"
            );
        });

        crate::store::tests::shutdown_test_host(cx, &store, &host);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn switching_language_relocalizes_the_device_name_placeholder(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link, WorkspaceAttachment::Local, None, None, false, cx)
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let window_state = cx.new(|_| WindowState::new(false));
        let mut page = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| SettingsPage::new(store.clone(), window_state, window, cx));
            page = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let page = page.unwrap();
        crate::settings::assert_follows_language_switch(
            cx,
            "settings.device_name.placeholder",
            |cx| {
                page.read_with(cx, |page, cx| {
                    let input = page.device_name_input.state.read(cx);
                    input.presentation().placeholder().to_string()
                })
            },
        );
    }
}
