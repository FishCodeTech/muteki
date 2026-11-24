use crate::{
    icon::Icon,
    sizing::{Sizable, Size},
    theme::ActiveTheme as _,
    widgets::{Spinner, Tooltip},
};
use gpui::{
    AnyElement, App, ClickEvent, Corners, ElementId, InteractiveElement, Interactivity,
    IntoElement, ParentElement, Pixels, RenderOnce, SharedString, StatefulInteractiveElement,
    StyleRefinement, Styled, Window, prelude::FluentBuilder as _, px,
};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonVariant {
    #[default]
    Default,
    Primary,
    Secondary,
    Danger,
    Success,
    Ghost,
    Text,
}

pub trait ButtonVariants: Sized {
    fn with_variant(self, variant: ButtonVariant) -> Self;
    fn primary(self) -> Self {
        self.with_variant(ButtonVariant::Primary)
    }
    fn secondary(self) -> Self {
        self.with_variant(ButtonVariant::Secondary)
    }
    fn danger(self) -> Self {
        self.with_variant(ButtonVariant::Danger)
    }
    fn success(self) -> Self {
        self.with_variant(ButtonVariant::Success)
    }
    fn ghost(self) -> Self {
        self.with_variant(ButtonVariant::Ghost)
    }
    fn text(self) -> Self {
        self.with_variant(ButtonVariant::Text)
    }
}

#[derive(Clone, Copy, Default)]
enum ButtonRounded {
    #[default]
    Medium,
    Chip,
    Size(Pixels),
    /// One radius per corner, for a button that fills the end of a bar and
    /// takes the bar's own corners on that side.
    Corners(Corners<Pixels>),
}

impl From<Pixels> for ButtonRounded {
    fn from(value: Pixels) -> Self {
        Self::Size(value)
    }
}

#[derive(IntoElement)]
pub struct Button {
    base: gpui_base::Button,
    icon: Option<Icon>,
    label: Option<SharedString>,
    children: Vec<AnyElement>,
    disabled: bool,
    selected: bool,
    variant: ButtonVariant,
    rounded: ButtonRounded,
    outline: bool,
    compact: bool,
    /// Owes a finger a [`TOUCH_TARGET`](crate::material::TOUCH_TARGET)
    /// square whatever the button's size draws.
    touch: bool,
    /// The composer's chip control: 28 tall at the chip radius.
    chip: bool,
    loading: bool,
    size: Size,
    tooltip: Option<SharedString>,
    /// Accessible name for icon-only buttons, which have no visible label to
    /// derive one from.
    aria_label: Option<SharedString>,
    on_click: Option<ClickHandler>,
    on_hover: Option<super::ToggleHandler>,
    press_then: Option<super::popover::SheetToggle>,
}

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// Height of a [`Button::chip`].
const CHIP_HEIGHT: f32 = 28.;

impl Button {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            base: gpui_base::Button::new(id),
            icon: None,
            label: None,
            children: Vec::new(),
            disabled: false,
            selected: false,
            variant: ButtonVariant::Default,
            rounded: ButtonRounded::Medium,
            outline: false,
            compact: false,
            touch: false,
            chip: false,
            loading: false,
            size: Size::Medium,
            tooltip: None,
            aria_label: None,
            on_click: None,
            on_hover: None,
            press_then: None,
        }
    }
    pub fn outline(mut self) -> Self {
        self.outline = true;
        self
    }
    /// Keep a [`TOUCH_TARGET`](crate::material::TOUCH_TARGET) hit box on a
    /// touch layout; the button's size still decides what it draws.
    pub fn touch(mut self, touch: bool) -> Self {
        self.touch = touch;
        self
    }
    /// The chip form used by the composer's inline controls: 28pt tall at the
    /// chip radius. gpui-base's `Button` carries no sizes, so this is the one
    /// place the chip's geometry is written.
    pub fn chip(mut self) -> Self {
        self.chip = true;
        self.rounded = ButtonRounded::Chip;
        self
    }
    pub fn rounded(mut self, rounded: impl Into<Pixels>) -> Self {
        self.rounded = ButtonRounded::Size(rounded.into());
        self
    }
    pub fn rounded_corners(mut self, corners: Corners<Pixels>) -> Self {
        self.rounded = ButtonRounded::Corners(corners);
        self
    }
    /// Whether pressing the button moves focus onto it; see
    /// [`gpui_base::Button::focusable`]. A menu that acts on a focused
    /// input's selection must leave that focus where it is.
    pub fn focusable(mut self, focusable: bool) -> Self {
        self.base = self.base.focusable(focusable);
        self
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(icon.into());
        self
    }
    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }
    /// Name an icon-only button for assistive technology. Buttons with a
    /// visible label already announce it and need no override.
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
    pub fn on_hover(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Some(Rc::new(handler));
        self
    }
    /// A press action for assistive technology that does what a click on the button does: its
    /// own handler, then `then`. A disabled or loading button gets none. GPUI's default press
    /// clicks the node's centre, which misses a button scrolled or clipped out of view.
    pub(crate) fn press_action(mut self, then: super::popover::SheetToggle) -> Self {
        self.press_then = Some(then);
        self
    }
}

impl ButtonVariants for Button {
    fn with_variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }
}

impl gpui_base::Selectable for Button {
    fn selected(self, selected: bool) -> Self {
        Button::selected(self, selected)
    }
    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl Sizable for Button {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl Styled for Button {
    fn style(&mut self) -> &mut StyleRefinement {
        self.base.style()
    }
}

impl ParentElement for Button {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl InteractiveElement for Button {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl StatefulInteractiveElement for Button {
    fn on_a11y_action(
        mut self,
        action: gpui::accesskit::Action,
        listener: impl FnMut(Option<&gpui::accesskit::ActionData>, &mut Window, &mut App) + 'static,
    ) -> Self {
        if action == gpui::accesskit::Action::Click {
            let listener = std::cell::RefCell::new(listener);
            return self.press_action(Rc::new(move |window, cx| {
                listener.borrow_mut()(None, window, cx);
            }));
        }
        self.base = self.base.on_a11y_action(action, listener);
        self
    }
}

impl RenderOnce for Button {
    fn render(mut self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let transparent = theme.background.opacity(0.);
        let (bg, fg, hover, border) = match self.variant {
            ButtonVariant::Default => {
                (theme.background, theme.foreground, theme.muted, theme.input)
            }
            ButtonVariant::Primary => (
                theme.primary,
                theme.primary_foreground,
                theme.primary.opacity(0.85),
                theme.primary,
            ),
            ButtonVariant::Secondary => (
                theme.secondary,
                theme.foreground,
                theme.secondary_active,
                theme.border,
            ),
            ButtonVariant::Danger => (
                theme.danger.opacity(0.2),
                theme.danger,
                theme.danger.opacity(0.3),
                theme.danger,
            ),
            ButtonVariant::Success => (
                theme.success.opacity(0.2),
                theme.success,
                theme.success.opacity(0.3),
                theme.success,
            ),
            // Transparent at rest; the hairline only shows with `.outline()`
            // (border width is 0 otherwise), which is how select/dropdown
            // triggers read as interactive controls.
            ButtonVariant::Ghost => (transparent, theme.foreground, theme.muted, theme.border),
            ButtonVariant::Text => (
                transparent,
                theme.foreground.opacity(0.9),
                theme.muted,
                transparent,
            ),
        };
        let (bg, fg) = if self.outline {
            let outline_fg = match self.variant {
                ButtonVariant::Primary => theme.primary,
                ButtonVariant::Danger => theme.danger,
                ButtonVariant::Success => theme.success,
                _ => fg,
            };
            (bg.opacity(0.1), outline_fg)
        } else {
            (bg, fg)
        };
        let corners = match self.rounded {
            ButtonRounded::Medium => Corners::all(theme.tokens.radius.md),
            ButtonRounded::Chip => Corners::all(crate::material::radius_chip(cx)),
            ButtonRounded::Size(value) => Corners::all(value),
            ButtonRounded::Corners(corners) => corners,
        };
        let icon_only = self.label.is_none() && self.children.is_empty();
        // A custom size sets the hit box, not the glyph: a 44pt touch control
        // keeps the medium icon rather than a 33pt one.
        let icon_size = match self.size {
            Size::Size(_) => Size::Medium,
            size => size,
        };
        // A caller that colours the button's text (a muted disclosure label)
        // keeps that colour; the variant supplies the default.
        let fg = self.base.style().text.color.unwrap_or(fg);
        let accessibility_label = self.aria_label.clone().or_else(|| self.label.clone());
        let ring = crate::material::focus_ring(cx);
        let press = self
            .press_then
            .filter(|_| !self.disabled && !self.loading)
            .map(|then| {
                let on_click = self.on_click.clone();
                move |_: Option<&gpui::accesskit::ActionData>, window: &mut Window, cx: &mut App| {
                    if let Some(on_click) = &on_click {
                        on_click(&ClickEvent::default(), window, cx);
                    }
                    then(window, cx);
                }
            });
        let content = gpui_base::h_flex()
            .size_full()
            .items_center()
            .justify_center()
            .map(|this| match self.size {
                Size::XSmall | Size::Small => this.gap_1(),
                _ => this.gap_2(),
            })
            .when_some(self.icon, |this, icon| {
                this.child(if self.loading {
                    Spinner::new().with_size(icon_size).into_any_element()
                } else {
                    icon.with_size(icon_size).into_any_element()
                })
            })
            .when_some(self.label, |this, label| this.child(label))
            .children(self.children);
        self.base
            .disabled(self.disabled || self.loading)
            .selected(self.selected)
            .when_some(accessibility_label, |this, label| {
                this.accessibility_label(label)
            })
            .styles(|styles| {
                styles
                    .selected(|style| style.bg(theme.muted).border_color(theme.primary))
                    .disabled(|style| style.opacity(0.5))
            })
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded_tl(corners.top_left)
            .rounded_tr(corners.top_right)
            .rounded_bl(corners.bottom_left)
            .rounded_br(corners.bottom_right)
            .when(self.chip, |this| this.h(px(CHIP_HEIGHT)).px_2())
            .when(!self.chip && icon_only, |this| match self.size {
                Size::XSmall => this.size_5(),
                Size::Small => this.size_6(),
                Size::Size(v) => this.size(v),
                _ => this.size_8(),
            })
            .when(
                !self.chip && !icon_only && !matches!(self.variant, ButtonVariant::Text),
                |this| match self.size {
                    Size::XSmall => this.h_5().px_1(),
                    Size::Small => this.h_6().px_2(),
                    Size::Size(v) => this.h(v).px(v * 0.2),
                    _ => this.h_8().px(if self.compact { px(8.) } else { px(10.) }),
                },
            )
            .when(self.touch, |this| {
                this.min_w(px(crate::material::TOUCH_TARGET))
                    .min_h(px(crate::material::TOUCH_TARGET))
            })
            .focus_visible(move |style| style.shadow(vec![ring]))
            .when(
                matches!(self.variant, ButtonVariant::Default) || self.outline,
                |this| this.border_1(),
            )
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .hover(move |this| this.bg(hover))
            .when_some(self.on_click, |this, handler| {
                this.on_click(move |event, window, cx| handler(event, window, cx))
            })
            .when_some(press, |this, press| {
                this.on_a11y_action(gpui::accesskit::Action::Click, press)
            })
            .when_some(self.on_hover, |this, handler| {
                this.on_hover(move |hovered, window, cx| handler(hovered, window, cx))
            })
            .child(content)
            .when_some(self.tooltip, |this, text| {
                this.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
            })
    }
}
