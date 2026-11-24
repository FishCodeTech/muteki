//! The rows a settings form is built from: Settings and the remote panels
//! inside it compose their groups from these, so every form row reads and
//! behaves the same.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClickEvent, InteractiveElement as _, IntoElement, ParentElement as _,
    RenderOnce, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Toggled, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use crate::icon::{Icon, IconName};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use crate::widgets::switch::Switch;

/// The width of a text field beside its row's label; gpui-base has no form-field width token.
pub const FIELD_WIDTH: f32 = 240.;

/// Max width of the settings content column, a reading measure for its forms.
pub const CONTENT_MAX_WIDTH: f32 = 768.;

/// The centred column a settings-form page scrolls: `column` at
/// [`CONTENT_MAX_WIDTH`] inside the page inset.
pub fn content_column(column: gpui::Div, compact: bool, cx: &App) -> gpui::Div {
    let spacing = &cx.theme().tokens.spacing;
    let inset = if compact {
        px(crate::material::COMPACT_PAGE_INSET)
    } else {
        spacing.xl
    };
    h_flex()
        .w_full()
        .justify_center()
        .px(inset)
        .py(spacing.xl)
        // Keep this width definite before capping it. Reversing these
        // constraints makes nested multiline inputs resolve their percentage
        // width to zero when the cap applies.
        .child(column.w(px(CONTENT_MAX_WIDTH)).max_w_full())
}

/// The top strip of a page whose body is a [`content_column`]: the body's
/// inset, so its [`content_header_column`] stays over the content at every
/// width.
pub fn content_header(strip: gpui_base::Toolbar, cx: &App) -> gpui_base::Toolbar {
    strip.px(cx.theme().tokens.spacing.xl).justify_center()
}

/// The column inside a [`content_header`], as wide as the content below it.
pub fn content_header_column() -> gpui::Div {
    h_flex().w(px(CONTENT_MAX_WIDTH)).max_w_full()
}

/// A captioned group of settings rows: the unit every settings page is
/// composed of.
pub fn settings_group(
    caption: impl Into<SharedString>,
    rows: Vec<AnyElement>,
    cx: &App,
) -> gpui::Div {
    v_flex()
        .child(crate::material::group_caption(caption, cx))
        .child(crate::material::group(cx).children(rows))
}

/// The note under a [`settings_group`]'s card, aligned with its rows' text:
/// what holds for the group as a whole.
pub fn footnote(text: impl Into<SharedString>, compact: bool, cx: &App) -> gpui::Div {
    div()
        .pl(px(crate::material::CARD_INSET))
        .pt(cx.theme().tokens.spacing.sm)
        .text_secondary(compact, cx)
        .child(text.into())
}

/// A link off Tcode, beside the text it adds to.
pub fn external_link(id: impl Into<SharedString>, label: SharedString, url: String) -> Button {
    Button::new(id.into())
        .ghost()
        .xsmall()
        .icon(IconName::ExternalLink)
        .label(label)
        .on_click(move |_, _, cx| cx.open_url(&url))
}

/// Left description block (title + muted description). `reset` is the
/// per-item "restore default" affordance and rides inline right after the
/// title, so it reads as belonging to that setting rather than to the row's
/// control on the far right. A row without a description is one whose title
/// and control already say everything.
pub fn row_labels(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    reset: Option<AnyElement>,
    compact: bool,
    cx: &App,
) -> gpui::Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_0p5()
        .child(
            h_flex()
                .min_w_0()
                .items_center()
                .gap_1()
                // A long title wraps rather than running under the control.
                .child(div().min_w_0().text_body(compact, cx).child(title.into()))
                .children(reset),
        )
        .children(description.map(|desc| div().text_secondary(compact, cx).child(desc)))
}

/// The per-item "restore default" button: icon-only, ghost, and rendered
/// only while `overridden` — a row showing its factory value has nothing to
/// restore, so the affordance would just be noise.
pub fn reset_action(
    id: &'static str,
    overridden: bool,
    on_reset: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Option<AnyElement> {
    if !overridden {
        return None;
    }
    let label = crate::tr!("settings.reset_item").into_owned();
    Some(
        Button::new(id)
            .ghost()
            .xsmall()
            .icon(IconName::Undo)
            .tooltip(label.clone())
            .aria_label(label)
            .on_click(move |event, window, cx| {
                // Toggle rows are clickable as a whole; a click that lands
                // on this button must not also flip the switch.
                cx.stop_propagation();
                on_reset(event, window, cx);
            })
            .into_any_element(),
    )
}

/// A row whose control can squeeze its label: stacked on a compact window.
/// A row whose control is a fixed affordance (a switch) is a [`ToggleRow`].
pub fn row_frame(compact: bool, cx: &App) -> gpui::Div {
    if compact {
        crate::material::group_row_stacked(cx)
    } else {
        crate::material::group_row(cx)
    }
}

/// A settings row whose control is a switch. The whole row is the control:
/// it owns click, focus, Space activation and the toggled state, and the
/// switch inside it only draws that state.
type ToggleHandler = Rc<dyn Fn(&bool, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct ToggleRow {
    id: SharedString,
    title: SharedString,
    description: Option<SharedString>,
    error: Option<SharedString>,
    checked: bool,
    compact: bool,
    disabled: bool,
    reset: Option<AnyElement>,
    accessory: Option<AnyElement>,
    on_toggle: ToggleHandler,
}

impl ToggleRow {
    /// `on_toggle` receives the state the row is being switched to.
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        checked: bool,
        compact: bool,
        on_toggle: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: None,
            error: None,
            checked,
            compact,
            disabled: false,
            reset: None,
            accessory: None,
            on_toggle: Rc::new(on_toggle),
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Why the switch did not take, under the title in the danger colour.
    pub fn error(mut self, error: Option<SharedString>) -> Self {
        self.error = error;
        self
    }

    pub fn reset(mut self, reset: Option<AnyElement>) -> Self {
        self.reset = reset;
        self
    }

    /// Status shown between the labels and the switch. A clickable
    /// accessory stops its click from reaching the row.
    pub fn accessory(mut self, accessory: Option<AnyElement>) -> Self {
        self.accessory = accessory;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl RenderOnce for ToggleRow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            title,
            description,
            error,
            checked,
            compact,
            disabled,
            reset,
            accessory,
            on_toggle,
        } = self;
        // The row is the tab stop, but the inline reset button is one too, so
        // the row owns an explicit handle (`accessible_clickable`'s implicit one
        // cannot be queried) and applies the tab order to it by hand.
        let focus = window
            .use_keyed_state(SharedString::from(format!("{id}-focus")), cx, |_, cx| {
                cx.focus_handle().tab_stop(true).tab_index(0)
            })
            .read(cx)
            .clone();
        let row_focus = focus.clone();
        let key_toggle = on_toggle.clone();
        let has_accessory = accessory.is_some();
        let selector = id.clone();
        crate::material::accessible_clickable(
            crate::material::group_row(cx),
            SharedString::from(format!("{id}-row")),
            Role::Switch,
            title.clone(),
            cx,
        )
        .debug_selector(move || selector.to_string())
        .track_focus(&focus)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .when(!disabled, |row| {
            row.cursor_pointer()
                // Handle Space during capture so the native event cannot fall
                // through to scrolling/text input before the row's synthesized
                // click runs. Enter continues to use GPUI's standard
                // focused-click behavior.
                .capture_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                    // Capture runs from the root down, so without this guard
                    // the row would swallow Space aimed at the reset button
                    // inside it.
                    if !row_focus.is_focused(window) {
                        return;
                    }
                    if event.keystroke.key == "space"
                        && !event.is_held
                        && !event.keystroke.modifiers.modified()
                    {
                        window.prevent_default();
                        cx.stop_propagation();
                        key_toggle(&!checked, window, cx);
                    }
                })
                .on_click(move |_, window, cx| on_toggle(&!checked, window, cx))
        })
        .child(
            row_labels(title, description, reset, compact, cx)
                .children(error.map(|error| {
                    div()
                        .text_secondary(compact, cx)
                        .text_color(cx.theme().danger_foreground)
                        .child(error)
                }))
                // An accessory takes the room the labels leave.
                .when(has_accessory, |labels| labels.flex_none()),
        )
        .children(accessory)
        .child(Switch::new(id).checked(checked).disabled(disabled))
    }
}

/// The trigger of a settings select.
pub fn dropdown_trigger(id: &'static str, label: impl Into<SharedString>, cx: &App) -> Button {
    Button::new(id).ghost().outline().compact().child(
        h_flex()
            // One width for every select trigger, so a column of them lines up; gpui-base's
            // Select is not adopted yet.
            .w(px(160.))
            .items_center()
            .justify_between()
            .gap_2()
            .child(label.into())
            .child(Icon::new(IconName::ChevronDown).text_color(cx.theme().muted_foreground)),
    )
}

/// One choice of a [`select_row`].
#[derive(Clone)]
pub struct SelectRowOption<T> {
    pub value: T,
    pub id: SharedString,
    pub label: SharedString,
    pub description: Option<SharedString>,
    pub selected: bool,
}

/// A settings row whose control picks one of `options` from a menu.
/// `menu_width` is the menu's floor, so its longest option fits.
#[allow(clippy::too_many_arguments)]
pub fn select_row<T: Clone + 'static>(
    trigger_id: &'static str,
    popover_id: &'static str,
    menu_id: &'static str,
    menu_width: f32,
    title: SharedString,
    description: SharedString,
    selected_label: SharedString,
    options: Vec<SelectRowOption<T>>,
    reset: Option<AnyElement>,
    compact: bool,
    on_select: impl Fn(T, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let trigger = dropdown_trigger(trigger_id, selected_label, cx);
    let on_select = Rc::new(on_select);
    let menu_label = title.clone();
    let dropdown = crate::material::overlay_popover(popover_id, cx)
        .trigger(trigger)
        .content(move |_, _, cx| {
            v_flex()
                .id(menu_id)
                .role(Role::Menu)
                .aria_label(menu_label.clone())
                .p_1()
                // Each menu names its own floor so its longest option fits; gpui-base's
                // Select is not adopted yet.
                .min_w(px(menu_width))
                .gap_0p5()
                .children(options.clone().into_iter().map(|option| {
                    let popover = cx.entity();
                    let on_select = on_select.clone();
                    let label = option.label.clone();
                    let item = crate::material::accessible_clickable(
                        crate::material::menu_item_frame(compact, cx),
                        option.id,
                        Role::MenuItem,
                        label.clone(),
                        cx,
                    )
                    .aria_selected(option.selected)
                    .w_full()
                    .text_body(compact, cx)
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().muted));
                    let item = if let Some(description) = option.description {
                        item.items_start().child(
                            v_flex()
                                .flex_1()
                                .gap_0p5()
                                .child(div().child(label))
                                .child(div().text_secondary(compact, cx).child(description)),
                        )
                    } else {
                        item.child(div().flex_1().child(label))
                    };
                    item.when(option.selected, |item| {
                        item.child(Icon::new(IconName::Check))
                    })
                    .on_click(move |_, window, cx| {
                        on_select(option.value.clone(), window, cx);
                        popover.update(cx, |state, cx| state.dismiss(window, cx));
                    })
                }))
        });

    row_frame(compact, cx)
        .child(row_labels(title, Some(description), reset, compact, cx))
        .child(dropdown)
        .into_any_element()
}
