//! One bootstrap for every client.
//!
//! A platform entry point does two things and then hands over: build its
//! `ClientHost` and call [`run_shell`]. Everything after that — fonts, theme,
//! markdown, keybindings, the window state seed, the overlay host and the shell
//! itself — is the same code everywhere. The window's seam (system insets, the
//! software keyboard) is GPUI's own: see `crate::window_seam`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AnyWindowHandle, App, AppContext as _, Entity, KeyBinding, Render, SharedString, Window,
    WindowOptions,
};
use tcode_client::host::ClientHost;

use crate::remote::{AttachmentTarget, ClientAttachment};
use crate::shell::{AppShell, ShellSetup, TogglePalette};
use crate::theme;
use crate::window_state::WindowState;

/// The embedded palette every client renders with.
pub const THEME_JSON: &str = include_str!("../../../themes/tcode.json");

/// The palette with its translucent canvas flattened to solid RGB, for a client
/// whose window is opaque. Over a native backdrop those colors composite with
/// the material behind them; over black they just look muddy.
pub fn flattened_theme_json() -> String {
    let flattened = THEME_JSON
        .replace("#F2F4F7C7", "#F2F4F7")
        .replace("#15171CC7", "#15171C");
    debug_assert_ne!(flattened, THEME_JSON, "canvas colors moved; update flatten");
    flattened
}

/// Where a client should attach at launch: the host it used last, if that
/// record still exists. A client with no such record opens on the hosts list.
pub fn last_host_target(host: &dyn ClientHost) -> Option<AttachmentTarget> {
    let id = host.last_host_id()?;
    host.load_hosts()
        .into_iter()
        .find(|saved| saved.host_id == id)
        .map(|host| AttachmentTarget::Remote(Box::new(host)))
}

pub struct ShellOptions {
    pub window: WindowOptions,
    /// The title the window carries until an attachment names one.
    pub title: SharedString,
    pub fonts: Vec<Cow<'static, [u8]>>,
    /// The palette this client renders with. Clients whose window is opaque
    /// hand over a theme with the translucent canvas already flattened.
    pub theme_json: Cow<'static, str>,
    /// Whether this client should bring itself to the front on launch.
    pub activate: bool,
    /// The user's first OS-configured language when the platform has a more
    /// authoritative API than `sys_locale`. Desktop leaves this as `None`.
    pub system_locale: Option<String>,
    /// The platform whose application lifecycle can suspend this client. Its
    /// foreground transitions wake the connection (see [`lifecycle_wake`]);
    /// a desktop window is never suspended and leaves this as `None`.
    pub lifecycle: Option<Rc<dyn gpui::Platform>>,
    pub setup: ShellSetup,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            window: WindowOptions::default(),
            title: crate::tr!("app.name").into(),
            fonts: vec![Cow::Borrowed(crate::assets::DM_SANS)],
            theme_json: Cow::Borrowed(THEME_JSON),
            activate: false,
            system_locale: None,
            lifecycle: None,
            setup: ShellSetup::default(),
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios", target_family = "wasm"))]
impl ShellOptions {
    /// Supply monospace on platforms without a system monospace font.
    /// Register the bundled family and select it anywhere the shared
    /// theme asks for the desktop-only SF Mono family.
    pub fn with_bundled_monospace(mut self) -> Self {
        self.fonts.extend([
            Cow::Borrowed(crate::assets::LILEX_REGULAR),
            Cow::Borrowed(crate::assets::LILEX_BOLD),
            Cow::Borrowed(crate::assets::LILEX_ITALIC),
            Cow::Borrowed(crate::assets::LILEX_BOLD_ITALIC),
        ]);
        #[cfg(target_family = "wasm")]
        self.fonts
            .push(Cow::Borrowed(crate::assets::TERMINAL_SYMBOLS));
        self.theme_json = Cow::Owned(self.theme_json.replace("SF Mono", "Lilex"));
        self
    }
}

/// Process-wide setup every window of this client needs: locale, fonts,
/// theme, markdown and keybindings. [`run_shell`] runs it; a bootstrap that
/// opens a window of its own before the shell runs it first, and the second
/// call does nothing.
pub fn init_client(cx: &mut App, host: &dyn ClientHost, options: &ShellOptions) {
    struct Initialized;
    impl gpui::Global for Initialized {}
    if cx.has_global::<Initialized>() {
        return;
    }
    cx.set_global(Initialized);
    // Browser bootstrap supplies its Fetch client; native image URLs need an HTTP client too.
    #[cfg(not(target_family = "wasm"))]
    {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to initialize image HTTP client");
        cx.set_http_client(std::sync::Arc::new(reqwest_client::ReqwestClient::from(
            client,
        )));
    }
    crate::i18n::set_platform_system_locale(options.system_locale.as_deref());
    let language = host.load_preferences().language;
    crate::i18n::apply_locale(match language.as_deref() {
        Some("system") | None => None,
        override_locale => override_locale,
    });
    cx.text_system()
        .add_fonts(options.fonts.clone())
        .expect("failed to register bundled application fonts");
    theme::init_with_json(&options.theme_json, cx);
    crate::markdown::init(cx);
    crate::image_viewer::init(cx);
    crate::shortcut::init(cx);
    // Global ⌘K / Ctrl-K opens/closes the command palette (handled by
    // AppShell). `secondary` is gpui's platform modifier: command on macOS,
    // control on Windows/Linux — where a literal `cmd-` binding would mean the
    // Super/Win key, which the OS intercepts.
    cx.bind_keys([KeyBinding::new("secondary-k", TogglePalette, None)]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([KeyBinding::new("cmd-q", crate::shell::Quit, None)]);
}

/// Open a window of this client around the view `build` returns: titled, in
/// the system appearance, on the platform material its options ask for, and
/// with the overlay host every dialog and toast needs.
pub fn open_client_window<V: Render>(
    cx: &mut App,
    options: WindowOptions,
    title: SharedString,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V> + 'static,
) -> AnyWindowHandle {
    #[cfg(target_os = "macos")]
    let window_background = options.window_background;
    let window = cx
        .open_window(options, move |window, cx| {
            window.set_window_title(&title);
            theme::sync_system_appearance(Some(window), cx);
            let view = build(window, cx);
            cx.new(|cx| gpui_base::Root::new(view, window, cx))
        })
        .expect("failed to open the tcode window");
    // A transparent macOS window gets Liquid Glass underneath, which GPUI's
    // `Blurred` path cannot give; see `macos_backdrop`.
    #[cfg(target_os = "macos")]
    if window_background == gpui::WindowBackgroundAppearance::Transparent {
        use crate::theme::ActiveTheme as _;
        let _ = window.update(cx, |_, window, cx| {
            crate::macos_backdrop::install(window, cx);
            theme::change_mode(cx.theme().mode, Some(window), cx);
        });
    }
    window.into()
}

/// Open this client's window on the shared shell.
///
/// Returns the window and its shell, so bootstrap can keep doing whatever is
/// genuinely its own (a smoke run, a launch flag, a platform Back callback).
pub fn run_shell(
    cx: &mut App,
    host: Rc<dyn ClientHost>,
    options: ShellOptions,
) -> (AnyWindowHandle, Entity<AppShell>) {
    init_client(cx, host.as_ref(), &options);
    if options.activate {
        cx.activate(true);
    }

    let title = options.title;
    let has_local = options.setup.local.is_some();
    let mut setup = options.setup;
    setup.restore_navigation |= gpui_base::is_mobile();
    let setup = Rc::new(RefCell::new(Some(setup)));
    let mounted: Rc<RefCell<Option<Entity<AppShell>>>> = Rc::new(RefCell::new(None));
    // Who this client is, and how it re-points at another host. Installed
    // *before* the window, because the views the window builds — the pair form
    // most of all — ask this global what kind of client they are on.
    // Switching resolves the window's shell when it is actually asked to, not
    // when this global is installed: the shell does not exist yet here, and the
    // cell below is emptied as soon as the window hands it over.
    cx.set_global(ClientAttachment::new(
        host,
        has_local,
        |target, window, cx| {
            crate::shell::switch_current(target, window, cx);
        },
    ));
    let captured = mounted.clone();
    let window = open_client_window(cx, options.window, title, move |window, cx| {
        let window_state = cx.new(|_| WindowState::new(false));
        let shell = cx.new(|cx| {
            AppShell::new(
                window_state,
                setup.borrow_mut().take().unwrap_or_default(),
                window,
                cx,
            )
        });
        *captured.borrow_mut() = Some(shell.clone());
        shell
    });
    let shell = mounted
        .borrow_mut()
        .take()
        .expect("the shell is built while the window opens");
    crate::shell::set_back_target(window, &shell, cx);
    if let Some(wakes) = options.lifecycle.as_deref().map(lifecycle_wakes) {
        let shell = shell.downgrade();
        cx.spawn(async move |cx| {
            while let Ok(wake) = wakes.recv().await {
                if shell
                    .update(cx, |shell, _| shell.wake_connection(wake))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
    let _ = window.update(cx, |_, window, _| {
        if options.activate {
            window.activate_window();
        }
    });
    (window, shell)
}

/// Application recovery policy for a client the OS can suspend: every
/// foreground transition probes or reconnects the attachment once, depending
/// on how long the client was away.
fn lifecycle_wakes(
    platform: &dyn gpui::Platform,
) -> async_channel::Receiver<tcode_client::recovery::Wake> {
    let (sender, receiver) = async_channel::unbounded();
    let mut lifecycle = tcode_client::recovery::Lifecycle::default();
    platform.on_app_lifecycle(Box::new(move |phase| {
        // Wall time includes device sleep. A backward clock correction
        // saturates to a short absence and still gets a bounded probe.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if let Some(wake) = lifecycle_wake(&mut lifecycle, phase, now) {
            let _ = sender.try_send(wake);
        }
    }));
    receiver
}

fn lifecycle_wake(
    lifecycle: &mut tcode_client::recovery::Lifecycle,
    phase: gpui::AppLifecyclePhase,
    now_ms: u64,
) -> Option<tcode_client::recovery::Wake> {
    match phase {
        gpui::AppLifecyclePhase::Background => {
            lifecycle.background(now_ms);
            None
        }
        gpui::AppLifecyclePhase::Foreground | gpui::AppLifecyclePhase::Active => {
            lifecycle.foreground(now_ms)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_lifecycle_phase_hook_probes_or_reconnects_once() {
        use gpui::AppLifecyclePhase as Phase;
        use tcode_client::recovery::{Lifecycle, Wake};
        let mut lifecycle = Lifecycle::default();
        assert_eq!(lifecycle_wake(&mut lifecycle, Phase::Background, 0), None);
        assert_eq!(
            lifecycle_wake(&mut lifecycle, Phase::Foreground, 9_999),
            Some(Wake::Probe)
        );
        assert_eq!(lifecycle_wake(&mut lifecycle, Phase::Active, 10_000), None);
        assert_eq!(
            lifecycle_wake(&mut lifecycle, Phase::Background, 20_000),
            None
        );
        assert_eq!(
            lifecycle_wake(&mut lifecycle, Phase::Active, 30_000),
            Some(Wake::Reconnect)
        );
    }
}
