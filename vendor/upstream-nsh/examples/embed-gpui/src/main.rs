//! NSH + GPUI Kit: a GPUI Kit app embedding NSH apps, the Longbridge Pro /
//! Allsum Desktop case of native UI delivered at run time into a desktop
//! app built with GPUI Kit.
//!
//! The window is GPUI Kit's (its `Root`, a sidebar, buttons, an input, a
//! dialog); the two panes of the resizable split are [`AppView`]s, each
//! running its own NSH engine. The host's theme reaches them on every
//! change ([`theme::nsh_theme`], `observe_global::<Theme>`), Tab moves
//! through the host's widgets and the surfaces' fields alike, GPUI Kit's
//! dialogs and notifications draw above the surfaces, closing a pane
//! drops its view, which stops its engine, and the apps' watchlist and
//! "Extended hours" switch are GPUI Kit widgets this app registers as
//! native components ([`watchlist`], [`switch`]).
//!
//! The host's desk ([`trading`]: the selected symbol, the orders, the
//! positions) is shared both ways: the apps reach it by name
//! (`nativeModule("host/trading")`, served from the set this app gives
//! `set_ui_sets`) and subscribe to it, and the sidebar and toolbar follow
//! and change it, so a symbol or an order chosen anywhere shows
//! everywhere. "Show layers" outlines what is GPUI Kit's and what is
//! NSH's ([`layers`]).
//!
//! ```sh
//! cargo run                          # here
//! cargo run -p example-embed-gpui    # from the repository's root
//! ```
//!
//! `build.rs` bundles and compiles `app/` as `nsh build` does, so the app
//! is built with the example. `cargo run -- <bundle.nshc or its
//! directory>` runs another bundle;
//! `--alone` runs one surface in a window of its own (no host widgets).

mod layers;
mod switch;
mod theme;
mod trading;
mod watchlist;

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui_kit::{
    assets::Assets,
    base::StyledExt,
    component::{
        ActiveTheme, Icon, IconName, Sizable, Theme, ThemeMode, WindowExt,
        button::{Button, ButtonVariants},
        h_flex,
        input::{Input, InputEvent, InputState},
        notification::Notification,
        popover::Popover,
        resizable::{h_resizable, resizable_panel},
        sidebar::{Sidebar, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem},
        switch::Switch,
        v_flex,
    },
    prelude::FluentBuilder,
    *,
};
use nsh_platform_gpui::{App as NshApp, AppOptions, AppView, AssetBase, Bundle};

/// The icon of a ticket, in its pane's title bar and in the sidebar (one of
/// the icons GPUI Kit's `Assets` embeds).
const TICKET: IconName = IconName::FileText;

/// Which panes show.
#[derive(Clone, Copy, PartialEq)]
enum Layout {
    /// Both, side by side.
    Split,
    /// One of them.
    Single(usize),
}

/// One embedded app.
struct Pane {
    title: SharedString,
    /// `None` once closed: dropping the view stopped its engine.
    view: Option<Entity<AppView>>,
}

struct Shell {
    bundle: PathBuf,
    panes: Vec<Pane>,
    layout: Layout,
    search: Entity<InputState>,
    /// The host's desk as last seen ([`trading::watch`]).
    desk: trading::Desk,
    _theme: Subscription,
    _search: Subscription,
    _desk: Task<()>,
    _confirms: Task<()>,
}

impl Shell {
    fn new(bundle: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Symbol, Enter selects"));
        // Enter selects the symbol on the desk, for every ticket.
        let search_events = cx.subscribe_in(
            &search,
            window,
            |this: &mut Shell, search, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let symbol = search.read(cx).value().to_string();
                    this.select(&symbol, window, cx);
                }
            },
        );
        // Every theme change (mode, theme) goes to the embedded apps.
        let theme = cx.observe_global::<Theme>(|this: &mut Shell, cx| this.apply_theme(cx));
        // The desk's changes, from the tickets or from here.
        let mut desk = trading::watch();
        let follow = cx.spawn(async move |this, cx| {
            while desk.changed().await.is_ok() {
                let now = desk.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| {
                        this.desk = now;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        // The tickets' questions (`Trading.confirm`), asked in this window.
        let confirms = cx.spawn_in(window, async move |this, cx| {
            let Some(mut confirms) = trading::confirms() else {
                return;
            };
            while let Some(request) = confirms.recv().await {
                if this
                    .update_in(cx, |_, window, cx| confirm(request, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut shell = Shell {
            bundle,
            panes: Vec::new(),
            layout: Layout::Split,
            search,
            desk: trading::desk(),
            _theme: theme,
            _search: search_events,
            _desk: follow,
            _confirms: confirms,
        };
        for title in ["Ticket A", "Ticket B"] {
            let view = shell.open_app(window, cx);
            shell.panes.push(Pane {
                title: title.into(),
                view,
            });
        }
        shell
    }

    /// A new view running the bundle, in the host's theme.
    fn open_app(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<AppView>> {
        open_app(&self.bundle, window, cx)
    }

    fn apply_theme(&mut self, cx: &mut Context<Self>) {
        let theme = cx.theme();
        let (nsh_theme, appearance) = (theme::nsh_theme(theme), theme::appearance(theme.mode));
        for view in self.panes.iter().filter_map(|p| p.view.clone()) {
            view.update(cx, |view, cx| {
                view.set_theme(Some(nsh_theme.clone()), cx);
                view.set_appearance(appearance, cx);
            });
        }
        cx.notify();
    }

    fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mode = if cx.theme().mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        Theme::change(mode, Some(window), cx);
    }

    /// Selects `symbol` on the desk (every ticket follows), or says why not.
    fn select(&mut self, symbol: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(err) = trading::select(symbol) {
            window.push_notification(Notification::new().message(err), cx);
        }
    }

    /// Shows or hides the layers' outlines, here and in the surfaces'
    /// GPUI widgets (which read the global when they draw).
    fn toggle_layers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = layers::shown(cx);
        cx.set_global(layers::Layers(!shown));
        window.refresh();
    }

    /// Switches to the themes registered from `themes/violet.json` (a GPUI
    /// Kit theme file, in GPUI Kit's registry), or back to GPUI Kit's
    /// default; the surfaces follow ([`theme::nsh_theme`]).
    fn set_violet(&mut self, violet: bool, window: &mut Window, cx: &mut Context<Self>) {
        let registry = gpui_kit::component::ThemeRegistry::global(cx);
        let (light, dark) = if violet {
            let themes = registry.themes();
            match (themes.get("Violet Light"), themes.get("Violet Dark")) {
                (Some(l), Some(d)) => (l.clone(), d.clone()),
                _ => return,
            }
        } else {
            (
                registry.default_light_theme().clone(),
                registry.default_dark_theme().clone(),
            )
        };
        let mode = cx.theme().mode;
        let theme = Theme::global_mut(cx);
        theme.light_theme = light;
        theme.dark_theme = dark;
        Theme::change(mode, Some(window), cx);
    }

    /// Closes pane `i` (its engine stops) or opens it again.
    fn toggle_pane(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes[i].view.take().is_none() {
            self.panes[i].view = self.open_app(window, cx);
        }
        cx.notify();
    }

    /// The toolbar's positions, a button opening the desk's positions and
    /// orders in a GPUI Kit popover that hangs over the tickets.
    fn positions_popover(&self, summary: String) -> impl IntoElement {
        let desk = self.desk.clone();
        Popover::new("positions")
            .anchor(Anchor::TopRight)
            .trigger(
                Button::new("positions-button")
                    .small()
                    .ghost()
                    .label(format!("Positions: {summary}"))
                    .icon(IconName::ChevronDown),
            )
            .content(move |_, _, cx| {
                let theme = cx.theme();
                let muted = theme.muted_foreground;
                let heading = |text: &'static str| div().text_xs().text_color(muted).child(text);
                v_flex()
                    .w(px(340.))
                    .gap_2()
                    .text_sm()
                    .child(heading("Positions"))
                    .children(trading::SYMBOLS.map(|(symbol, price)| {
                        let shares = desk
                            .positions
                            .iter()
                            .find(|p| p.symbol == symbol)
                            .map_or(0, |p| p.shares);
                        h_flex()
                            .justify_between()
                            .child(symbol)
                            .child(format!("{shares:+} · ${:.2}", shares as f64 * price))
                    }))
                    .child(heading("Orders, all tickets"))
                    .children(desk.orders.iter().map(|o| {
                        let (side, color) = match o.side {
                            trading::Side::Buy => ("Buy", theme.chart_bullish),
                            trading::Side::Sell => ("Sell", theme.chart_bearish),
                        };
                        h_flex()
                            .gap_2()
                            .child(div().text_color(muted).child(format!("#{}", o.id)))
                            .child(div().text_color(color).child(side))
                            .child(format!("{} {} at ${:.2}", o.quantity, o.symbol, o.price))
                    }))
                    .when(desk.orders.is_empty(), |d| {
                        d.child(div().text_color(muted).child("None yet"))
                    })
                    .child(
                        div()
                            .pt_2()
                            .border_t_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(muted)
                            .child(
                                "A GPUI Kit popover of the host, drawn above the NSH tickets. \
                                 Click outside or press Esc to close it.",
                            ),
                    )
            })
    }

    fn pane(&self, i: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = &self.panes[i];
        let theme = cx.theme();
        let open = pane.view.is_some();
        // The pane's one name is the host's: the app shows no heading of
        // its own. Next to it, the desk the ticket trades on.
        let orders = match self.desk.orders.len() {
            1 => "1 order".to_owned(),
            n => format!("{n} orders"),
        };
        let session = if open {
            format!("{} ${:.2} · {orders}", self.desk.symbol, self.desk.price)
        } else {
            "Closed".to_owned()
        };
        v_flex()
            .size_full()
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(36.))
                    .pl_3()
                    .pr_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.title_bar)
                    .child(Icon::new(TICKET).small().text_color(theme.muted_foreground))
                    .child(div().text_sm().font_medium().child(pane.title.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(session),
                    )
                    .child(
                        Button::new(("close", i))
                            .ghost()
                            .xsmall()
                            .icon(if open {
                                IconName::Close
                            } else {
                                IconName::Plus
                            })
                            .tooltip(if open { "Close ticket" } else { "Open ticket" })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.toggle_pane(i, window, cx)
                            })),
                    ),
            )
            .child(match &pane.view {
                Some(view) => div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(view.clone())
                    .when(layers::shown(cx), |d| d.child(layers::nsh_overlay(cx))),
                None => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.muted_foreground)
                    .child("Closed: its engine stopped. Open starts a new one."),
            })
    }
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layout = self.layout;
        let item = |label: &'static str, target: Layout, cx: &mut Context<Self>| {
            let shell = cx.entity().downgrade();
            SidebarMenuItem::new(label)
                .active(layout == target)
                .on_click(move |_, _, cx| {
                    let _ = shell.update(cx, |this, cx| {
                        this.layout = target;
                        cx.notify();
                    });
                })
        };
        let menu = SidebarMenu::new().children([
            item("Both tickets", Layout::Split, cx).icon(IconName::LayoutDashboard),
            item("Ticket A", Layout::Single(0), cx).icon(TICKET),
            item("Ticket B", Layout::Single(1), cx).icon(TICKET),
        ]);
        // The host's watchlist: the desk's symbol, picked here or in a
        // ticket.
        let watchlist = SidebarMenu::new().children(trading::SYMBOLS.map(|(symbol, price)| {
            let shell = cx.entity().downgrade();
            SidebarMenuItem::new(symbol)
                .active(self.desk.symbol == symbol)
                .suffix(move |_, cx| {
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("${price:.2}"))
                })
                .on_click(move |_, window, cx| {
                    let _ = shell.update(cx, |this, cx| this.select(symbol, window, cx));
                })
        }));
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        // The desk's orders, from every ticket.
        let orders = v_flex()
            .gap_1()
            .px_2()
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Orders, all tickets"),
            )
            .children(self.desk.orders.iter().map(|o| {
                let (side, color) = match o.side {
                    trading::Side::Buy => ("Buy", theme.chart_bullish),
                    trading::Side::Sell => ("Sell", theme.chart_bearish),
                };
                h_flex()
                    .gap_1()
                    .text_xs()
                    .child(div().text_color(muted).child(format!("#{}", o.id)))
                    .child(div().text_color(color).child(side))
                    .child(format!("{} {}", o.quantity, o.symbol))
            }))
            .when(self.desk.orders.is_empty(), |d| {
                d.child(div().text_xs().text_color(muted).child("None yet"))
            });
        let positions = if self.desk.positions.is_empty() {
            "No positions".to_owned()
        } else {
            self.desk
                .positions
                .iter()
                .map(|p| format!("{} {:+}", p.symbol, p.shares))
                .collect::<Vec<_>>()
                .join(" · ")
        };
        let dark = theme.mode.is_dark();
        let shown = layers::shown(cx);
        let toolbar = h_flex()
            .gap_2()
            .child(div().w(px(200.)).child(Input::new(&self.search).small()))
            .child(
                Button::new("mode")
                    .small()
                    .outline()
                    .label(if dark { "Light" } else { "Dark" })
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_mode(window, cx))),
            )
            .child(
                Button::new("violet")
                    .small()
                    .outline()
                    .label("Violet theme")
                    .on_click(cx.listener(|this, _, window, cx| this.set_violet(true, window, cx))),
            )
            .child(
                Button::new("default")
                    .small()
                    .outline()
                    .label("Default theme")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.set_violet(false, window, cx)),
                    ),
            )
            // A switch, so its state shows: on, the outlines and the legend.
            .child(
                Switch::new("layers")
                    .small()
                    .checked(shown)
                    .label("Show layers")
                    .on_change(
                        cx.listener(|this, _: &bool, window, cx| this.toggle_layers(window, cx)),
                    ),
            )
            .child(
                Button::new("dialog")
                    .small()
                    .primary()
                    .label("Dialog")
                    .on_click(|_, window, cx| {
                        window.open_dialog(cx, |dialog, _, _| {
                            dialog
                                .title("A GPUI Kit dialog")
                                .child("It draws above the NSH surfaces, which keep running.")
                        });
                        window.push_notification(
                            Notification::new().message("A notification above the surfaces"),
                            cx,
                        );
                    }),
            )
            .child(div().flex_1())
            .child(self.positions_popover(positions));
        // A gap on each side of the divider: the panes never touch it.
        let content = match layout {
            Layout::Split => h_resizable("panes")
                .child(resizable_panel().child(div().size_full().pr_2().child(self.pane(0, cx))))
                .child(resizable_panel().child(div().size_full().pl_2().child(self.pane(1, cx))))
                .into_any_element(),
            Layout::Single(i) => self.pane(i, cx).into_any_element(),
        };
        let legend = h_flex()
            .gap_4()
            .child(layers::legend_item("GPUI Kit host", cx.theme().border, cx))
            .child(layers::legend_item(
                "NSH app (TSX)",
                layers::nsh_colors(cx).0,
                cx,
            ))
            .child(layers::legend_item(
                "GPUI widget inside the NSH app",
                layers::widget_colors(cx).0,
                cx,
            ));
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                Sidebar::new("sidebar")
                    .w(px(220.))
                    .header(SidebarHeader::new().child("NSH + GPUI Kit"))
                    .child(SidebarGroup::new("Embedded apps").child(menu))
                    .child(SidebarGroup::new("Watchlist").child(watchlist))
                    .footer(orders),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .p_3()
                    .gap_3()
                    .child(toolbar)
                    .child(div().flex_1().min_h_0().child(content))
                    .when(shown, |d| d.child(legend)),
            )
    }
}

/// Asks a ticket's question (`Trading.confirm`) in GPUI Kit's alert dialog
/// and answers it: OK yes, Cancel or Esc no.
fn confirm(request: trading::Confirm, window: &mut Window, cx: &mut App) {
    // The dialog's builder runs on every frame, and its buttons and
    // closing each answer: the first answer goes.
    let answer = Rc::new(RefCell::new(Some(request.answer)));
    let reply = |answer: &RefCell<Option<_>>, yes: bool| {
        if let Some(answer) = answer.borrow_mut().take() {
            let _: Result<(), bool> = tokio::sync::oneshot::Sender::send(answer, yes);
        }
    };
    let message = SharedString::from(request.message);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (ok, cancel, close) = (answer.clone(), answer.clone(), answer.clone());
        alert
            .title("Place this order?")
            .description(message.clone())
            .confirm()
            .ok_text("Place order")
            .on_ok(move |_, _, _| {
                reply(&ok, true);
                true
            })
            .on_cancel(move |_, _, _| {
                reply(&cancel, false);
                true
            })
            .on_close(move |_, _, _| reply(&close, false))
    });
}

/// A new view running `path`'s bundle, in the host's theme.
fn open_app(path: &Path, window: &mut Window, cx: &mut App) -> Option<Entity<AppView>> {
    // SAFETY: the bundle is this app's own build output (`build.rs`), or
    // one its developer names, the same trust as the app's resources.
    let bundle = match unsafe { Bundle::read_trusted(path) } {
        Ok(bundle) => bundle,
        Err(err) => {
            log::error!("cannot read {}: {err}", path.display());
            return None;
        }
    };
    let options = AppOptions {
        // The build copies imported assets next to the bundle.
        asset_base: path
            .parent()
            .map(|dir| AssetBase::Directory(dir.join("assets"))),
        ..AppOptions::default()
    };
    let view = cx.new(|cx| AppView::new(options, window, cx));
    // The app's switch is GPUI Kit's, drawn by this app.
    view.read(cx)
        .register_component(switch::NAME, std::rc::Rc::new(switch::HostSwitch::make));
    view.read(cx).register_component(
        watchlist::NAME,
        std::rc::Rc::new(watchlist::Watchlist::make),
    );
    let theme = cx.theme();
    let (nsh_theme, appearance) = (theme::nsh_theme(theme), theme::appearance(theme.mode));
    view.update(cx, |view, cx| {
        // Before the app starts: its first frame is in the host's theme.
        view.set_theme(Some(nsh_theme), cx);
        view.set_appearance(appearance, cx);
        view.on_error(|message| log::error!("embedded app: {message}"));
        view.load(NshApp::Js(bundle));
    });
    Some(view)
}

/// The bundle to run: the argument (a `.nshc` or its directory), else
/// `app/` as `build.rs` built it.
fn bundle_path() -> PathBuf {
    let arg = std::env::args_os()
        .skip(1)
        .find(|a| a != "--alone")
        .map(PathBuf::from);
    let path = arg.unwrap_or_else(|| Path::new(env!("OUT_DIR")).join("app"));
    if path.is_dir() {
        path.join("bundle.nshc")
    } else {
        path
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // What the apps reach by name: `nsh/base` and the host's desk
    // (`nativeModule("host/trading")`), served by every surface.
    nsh_platform_gpui::set_ui_sets(vec![nsh_base::registry(), trading::registry()]);
    let bundle = bundle_path();
    gpui_kit::application().with_assets(Assets).run(move |cx| {
        gpui_kit::init(cx);
        cx.set_global(layers::Layers(true));
        // nsh's theme format is gpui-kit's: a theme file with a name per
        // entry loads into GPUI Kit's registry as it is.
        if let Err(err) = gpui_kit::component::ThemeRegistry::global_mut(cx)
            .load_themes_from_str(include_str!("../themes/violet.json"))
        {
            log::error!("violet theme: {err}");
        }
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1280.), px(820.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("NSH + GPUI Kit".into()),
                ..Default::default()
            }),
            // GPUI draws unfocused windows at 30 fps by default, which
            // would halve the embedded apps' animations whenever another
            // window has the keyboard.
            inactive_frame_interval: None,
            ..Default::default()
        };
        if std::env::args().any(|a| a == "--alone") {
            // One surface filling a window with no host widgets: Tab wraps
            // around its stops, as in a standalone app.
            cx.open_window(options, |window, cx| {
                let view = open_app(&bundle, window, cx).expect("the bundle loads");
                window.focus(&view.focus_handle(cx), cx);
                view
            })
            .expect("a window opens");
        } else {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Shell::new(bundle, window, cx))
            })
            .expect("a window opens");
        }
        cx.activate(true);
    });
}
