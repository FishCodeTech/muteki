//! `host.watchlist`: GPUI Kit buttons inside the NSH app, as a native
//! component. The app gives it the desk's symbol (`symbol`) and hears the
//! one picked (`onSelect`, `{ symbol }`, by a click or Left / Right /
//! Home / End while focused), which it hands to the host
//! (`Trading.select`); the host's desk then selects it everywhere. Its info
//! button opens the selected symbol's details in a GPUI Kit popover, which
//! hangs over the NSH layout around the widget. While the layers show it
//! is framed as a GPUI widget ([`crate::layers`]).

use gpui_kit::{
    Anchor, AnyElement, App, FocusHandle, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Styled, Window,
    component::{
        ActiveTheme, IconName, Sizable,
        button::{Button, ButtonVariants},
        h_flex,
        popover::Popover,
        v_flex,
    },
    div, px,
};
use nsh_platform_gpui::{ComponentContext, NativeComponent};
use serde_json::{Value, json};

use crate::trading::SYMBOLS;

/// The registered name, which JS declares with `nativeComponent`.
pub(crate) const NAME: &str = "host.watchlist";

/// One watchlist.
pub(crate) struct Watchlist {
    context: ComponentContext,
    symbol: String,
    focus: Option<FocusHandle>,
    /// The width last reported to nsh.
    reported: f32,
}

/// The buttons' row and the info button, with its focus ring (the app's
/// style gives the height).
const ROW: f32 = 222.0;

impl Watchlist {
    /// The component's factory (`AppView::register_component`).
    pub(crate) fn make(context: ComponentContext) -> Box<dyn NativeComponent> {
        Box::new(Self {
            context,
            symbol: "AAPL".into(),
            focus: None,
            reported: 0.0,
        })
    }
}

impl NativeComponent for Watchlist {
    fn focus_handle(&self) -> Option<FocusHandle> {
        self.focus.clone()
    }

    fn set_prop(&mut self, name: &str, value: Option<&Value>, _: &mut Window, _: &mut App) {
        if name == "symbol" {
            self.symbol = value.and_then(Value::as_str).unwrap_or("AAPL").to_owned();
        }
    }

    fn render(&mut self, _: &mut Window, cx: &mut App) -> Option<AnyElement> {
        let width = if crate::layers::shown(cx) {
            ROW + crate::layers::WIDGET_TAG + 2.0 * crate::layers::WIDGET_BORDER
        } else {
            ROW
        };
        if width != self.reported {
            self.reported = width;
            self.context.set_size(width, 32.);
        }
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        let click_focus = focus.clone();
        let context = self.context.clone();
        let selected = self.symbol.clone();
        let ring = cx.theme().ring;
        let mut row = h_flex()
            .id("watchlist")
            .track_focus(&focus)
            .border_1()
            .border_color(gpui_kit::transparent_black())
            .rounded(cx.theme().radius)
            .focus(move |style| style.border_color(ring))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                window.focus(&click_focus, cx)
            })
            .on_key_down(move |event, _, _| {
                let symbols = SYMBOLS.map(|(s, _)| s);
                let current = symbols
                    .iter()
                    .position(|symbol| *symbol == selected)
                    .unwrap_or(0);
                let next = match event.keystroke.key.as_str() {
                    "left" => (current + symbols.len() - 1) % symbols.len(),
                    "right" => (current + 1) % symbols.len(),
                    "home" => 0,
                    "end" => symbols.len() - 1,
                    _ => return,
                };
                context.emit("select", json!({ "symbol": symbols[next] }));
            })
            .size_full()
            .gap_2()
            .text_color(cx.theme().foreground);
        for (symbol, _) in SYMBOLS {
            let context = self.context.clone();
            let button = Button::new(symbol)
                .small()
                .tab_stop(false)
                .label(symbol)
                .on_click(move |_, _, _| context.emit("select", json!({ "symbol": symbol })));
            row = row.child(if self.symbol == symbol {
                button.primary()
            } else {
                button.outline()
            });
        }
        let symbol = self.symbol.clone();
        row = row.child(
            Popover::new("details")
                .anchor(Anchor::TopLeft)
                .trigger(
                    Button::new("details-button")
                        .small()
                        .ghost()
                        .tab_stop(false)
                        .icon(IconName::Info)
                        .tooltip("Symbol details"),
                )
                .content(move |_, _, cx| details(&symbol, cx)),
        );
        Some(crate::layers::widget_frame(row, cx).into_any_element())
    }
}

/// The details of `symbol` on the host's desk, as it is now.
fn details(symbol: &str, cx: &App) -> gpui_kit::Div {
    let desk = crate::trading::desk();
    let theme = cx.theme();
    let price = SYMBOLS
        .iter()
        .find(|(s, _)| *s == symbol)
        .map_or(0.0, |&(_, p)| p);
    let held = desk
        .positions
        .iter()
        .find(|p| p.symbol == symbol)
        .map_or(0, |p| p.shares);
    let orders = desk.orders.iter().filter(|o| o.symbol == symbol).count();
    let line = |label: &'static str, value: String| {
        h_flex()
            .justify_between()
            .child(div().text_color(theme.muted_foreground).child(label))
            .child(value)
    };
    v_flex()
        .w(px(260.))
        .gap_1()
        .text_sm()
        .child(div().text_lg().child(symbol.to_owned()))
        .child(line("Sample price", format!("${price:.2}")))
        .child(line("Held", format!("{held:+} shares")))
        .child(line("Orders on the desk", orders.to_string()))
        .child(
            div()
                .pt_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("A GPUI Kit popover, opened by a GPUI widget inside the NSH app."),
        )
}
