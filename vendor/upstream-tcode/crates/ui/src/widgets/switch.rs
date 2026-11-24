use crate::{
    sizing::{Sizable, Size},
    theme::ActiveTheme as _,
    widgets::Tooltip,
};
use gpui::{
    App, ElementId, Hsla, IntoElement, ParentElement as _, RenderOnce, SharedString,
    StatefulInteractiveElement, StyleRefinement, Styled, Window, div, prelude::FluentBuilder as _,
    px,
};
use gpui_base::StyledExt as _;
use gpui_base::{SwitchThumb, SwitchTrack};
use std::rc::Rc;

#[derive(IntoElement)]
pub struct Switch {
    id: ElementId,
    style: StyleRefinement,
    checked: bool,
    disabled: bool,
    label: Option<SharedString>,
    aria_label: Option<SharedString>,
    on_click: Option<super::ToggleHandler>,
    size: Size,
    color: Option<Hsla>,
    tooltip: Option<SharedString>,
}

impl Switch {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            style: StyleRefinement::default(),
            checked: false,
            disabled: false,
            label: None,
            aria_label: None,
            on_click: None,
            size: Size::Medium,
            color: None,
            tooltip: None,
        }
    }
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
    pub fn on_click(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
    pub fn color(mut self, color: impl Into<Hsla>) -> Self {
        self.color = Some(color.into());
        self
    }
    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}
impl Sizable for Switch {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}
impl Styled for Switch {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Switch {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let checked = self.checked;
        let (width, height, thumb) = match self.size {
            Size::XSmall | Size::Small => (px(28.), px(16.), px(12.)),
            _ => (px(36.), px(20.), px(16.)),
        };
        let inset = px(2.);
        let checked_bg = self.color.unwrap_or(cx.theme().primary);
        let unchecked_bg = cx.theme().muted;
        let thumb_bg = cx.theme().background;
        gpui_base::Switch::new(self.id.clone())
            .checked(checked)
            .disabled(self.disabled)
            .when_some(
                self.aria_label.or_else(|| self.label.clone()),
                |this, label| this.accessibility_label(label),
            )
            .when_some(self.on_click, |this, handler| {
                this.on_change(move |next, _, window, cx| handler(&next, window, cx))
            })
            .h_flex()
            .gap_2()
            .items_center()
            .refine_style(&self.style)
            .child(
                SwitchTrack::new((self.id, "track"))
                    .checked(checked)
                    .disabled(self.disabled)
                    .w(width)
                    .h(height)
                    .rounded(height)
                    .flex()
                    .items_center()
                    .border(inset)
                    .border_color(cx.theme().background.opacity(0.))
                    .bg(unchecked_bg)
                    .styles(|styles| {
                        styles
                            .checked(|style| style.bg(checked_bg))
                            .disabled(|style| style.opacity(0.5))
                    })
                    .child(
                        SwitchThumb::new(checked)
                            .disabled(self.disabled)
                            .size(thumb)
                            .rounded(height)
                            .bg(thumb_bg)
                            .styles(|styles| {
                                styles.checked(|style| style.left(width - thumb - inset * 2))
                            })
                            .left(if checked {
                                width - thumb - inset * 2
                            } else {
                                px(0.)
                            }),
                    )
                    .when_some(self.tooltip, |this, text| {
                        this.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
                    }),
            )
            .when_some(self.label, |this, label| {
                this.child(div().line_height(height).child(label))
            })
    }
}
