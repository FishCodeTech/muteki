//! `host.switch`: a GPUI Kit `Switch` inside the nsh app, as a native
//! component. nsh lays its box out like any view, at the size the switch
//! reports (its track and label); the switch draws, takes its own clicks
//! and keys (Space or Enter while focused), is a Tab stop among the app's
//! text fields, and tells JS (`onChange`, `{ checked }`). While the
//! layers show it is framed as a GPUI widget ([`crate::layers`]).

use std::{cell::Cell, rc::Rc};

use gpui_kit::{
    AnyElement, App, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Styled, TextRun, Window,
    component::{ActiveTheme, switch::Switch},
    div, px,
};
use nsh_platform_gpui::{ComponentContext, NativeComponent};
use serde_json::{Value, json};

/// The registered name, which JS declares with `nativeComponent`.
pub const NAME: &str = "host.switch";

/// One switch.
pub struct HostSwitch {
    cx: ComponentContext,
    label: String,
    /// Controlled by JS (`checked`), and changed at once by a click so the
    /// switch does not wait for JS to answer.
    checked: Rc<Cell<bool>>,
    /// The size last reported to nsh.
    reported: (f32, f32),
    /// The focus handle Tab reaches (made with the first frame). The
    /// `Switch` keeps its own handle internally, so this one is a wrapper's
    /// and Space / Enter toggle through it.
    focus: Option<FocusHandle>,
}

/// The switch's track and the gap to its label (GPUI Kit's medium size).
const TRACK: (f32, f32) = (36.0, 20.0);
const GAP: f32 = 8.0;
/// The focus ring around it.
const BORDER: f32 = 1.0;

impl HostSwitch {
    /// The component's factory (`AppView::register_component`).
    pub fn make(cx: ComponentContext) -> Box<dyn NativeComponent> {
        Box::new(HostSwitch {
            cx,
            label: String::new(),
            checked: Rc::default(),
            reported: (0.0, 0.0),
            focus: None,
        })
    }
}

impl NativeComponent for HostSwitch {
    fn render(&mut self, window: &mut Window, cx: &mut App) -> Option<AnyElement> {
        // Its natural size: nsh lays the box out at it (the app's style
        // leaves both dimensions open).
        let font_size = cx.theme().font_size;
        let run = TextRun {
            len: self.label.len(),
            font: window.text_style().font(),
            color: cx.theme().foreground,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let label =
            window
                .text_system()
                .shape_line(self.label.clone().into(), font_size, &[run], None);
        let line_height = f32::from(window.line_height().max(px(TRACK.1)));
        let mut wanted = (
            (TRACK.0 + GAP + f32::from(label.width) + 2.0 * BORDER).ceil(),
            (line_height + 2.0 * BORDER).ceil(),
        );
        if crate::layers::shown(cx) {
            let frame = 2.0 * crate::layers::WIDGET_BORDER;
            wanted = (
                wanted.0 + crate::layers::WIDGET_TAG + frame,
                wanted.1 + frame,
            );
        }
        if wanted != self.reported {
            self.reported = wanted;
            self.cx.set_size(wanted.0, wanted.1);
        }
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        let set = {
            let (ncx, checked) = (self.cx.clone(), self.checked.clone());
            move |value: bool| {
                checked.set(value);
                ncx.emit("change", json!({ "checked": value }));
                ncx.redraw();
            }
        };
        let toggle = {
            let (set, checked) = (set.clone(), self.checked.clone());
            move |e: &KeyDownEvent, _: &mut Window, _: &mut App| {
                if matches!(e.keystroke.key.as_str(), "space" | "enter") {
                    set(!checked.get());
                }
            }
        };
        let ring = cx.theme().ring;
        let radius = cx.theme().radius;
        let click_focus = focus.clone();
        let switch = div()
            .size_full()
            .rounded(radius)
            .track_focus(&focus)
            // A ring while focused (the switch's own shows for its own
            // handle only).
            .border_1()
            .border_color(gpui_kit::transparent_black())
            .focus(move |s| s.border_color(ring))
            .on_key_down(toggle)
            // The switch focuses its own handle on a press; the focus
            // that Tab and the keys use is this one.
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                window.focus(&click_focus, cx);
            })
            .child(
                Switch::new("host-switch")
                    .checked(self.checked.get())
                    .label(self.label.clone())
                    .tab_stop(false)
                    .on_change(move |value: &bool, _, _| set(*value)),
            );
        Some(crate::layers::widget_frame(switch, cx).into_any_element())
    }

    fn focus_handle(&self) -> Option<FocusHandle> {
        self.focus.clone()
    }

    fn set_prop(&mut self, name: &str, value: Option<&Value>, _window: &mut Window, _cx: &mut App) {
        match name {
            "label" => self.label = value.and_then(Value::as_str).unwrap_or("").to_owned(),
            "checked" => self
                .checked
                .set(value.and_then(Value::as_bool).unwrap_or(false)),
            _ => {}
        }
    }
}
