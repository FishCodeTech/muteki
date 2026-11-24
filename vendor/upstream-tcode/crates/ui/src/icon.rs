use crate::{
    sizing::{Sizable, Size},
    theme::ActiveTheme,
};
use gpui::{
    AnyElement, App, AppContext, Context, Entity, Hsla, IntoElement, Radians, Render, RenderOnce,
    SharedString, StyleRefinement, Styled, Svg, Transformation, Window,
    prelude::FluentBuilder as _, svg,
};

/// Types implementing this trait can automatically be converted to [`Icon`].
pub trait IconNamed {
    fn path(self) -> SharedString;
}

impl<T: IconNamed> From<T> for Icon {
    fn from(value: T) -> Self {
        Icon::build(value)
    }
}

include!(concat!(env!("OUT_DIR"), "/icon_names.rs"));

impl IconName {
    pub fn view(self, cx: &mut App) -> Entity<Icon> {
        Icon::build(self).view(cx)
    }
}

impl From<IconName> for AnyElement {
    fn from(value: IconName) -> Self {
        Icon::build(value).into_any_element()
    }
}

impl RenderOnce for IconName {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        Icon::build(self)
    }
}

/// Tcode's SVG icon primitive.
///
/// An icon with no explicit size takes the current font size, so an icon next
/// to text is sized by that text's token; call sites do not size inline icons.
#[derive(IntoElement)]
pub struct Icon {
    base: Svg,
    style: StyleRefinement,
    path: SharedString,
    text_color: Option<Hsla>,
    size: Option<Size>,
}

impl Default for Icon {
    fn default() -> Self {
        Self {
            base: svg(),
            style: StyleRefinement::default(),
            path: "".into(),
            text_color: None,
            size: None,
        }
    }
}

impl Clone for Icon {
    fn clone(&self) -> Self {
        let mut icon = Self::default().path(self.path.clone());
        icon.style = self.style.clone();
        icon.size = self.size;
        icon.text_color = self.text_color;
        icon
    }
}

impl Icon {
    pub(crate) fn explicit_text_color(&self) -> Option<Hsla> {
        self.text_color
    }

    pub fn new(icon: impl Into<Icon>) -> Self {
        icon.into()
    }

    fn build(name: impl IconNamed) -> Self {
        Self::default().path(name.path())
    }

    pub fn path(mut self, path: impl Into<SharedString>) -> Self {
        self.path = path.into();
        self
    }

    pub fn view(self, cx: &mut App) -> Entity<Icon> {
        cx.new(|_| self)
    }

    pub fn transform(mut self, transformation: Transformation) -> Self {
        self.base = self.base.with_transformation(transformation);
        self
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn rotate(mut self, radians: impl Into<Radians>) -> Self {
        self.base = self
            .base
            .with_transformation(Transformation::rotate(radians));
        self
    }
}

impl Styled for Icon {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }

    fn text_color(mut self, color: impl Into<Hsla>) -> Self {
        self.text_color = Some(color.into());
        self
    }
}

impl Sizable for Icon {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = Some(size.into());
        self
    }
}

impl RenderOnce for Icon {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let text_color = self.text_color.unwrap_or_else(|| window.text_style().color);
        let text_size = window.text_style().font_size.to_pixels(window.rem_size());
        let has_base_size = self.style.size.width.is_some() || self.style.size.height.is_some();
        let mut base = self.base;
        *base.style() = self.style;

        base.flex_shrink_0()
            .text_color(text_color)
            .when(!has_base_size, |this| this.size(text_size))
            .when_some(self.size, apply_size)
            .path(self.path)
    }
}

impl From<Icon> for AnyElement {
    fn from(value: Icon) -> Self {
        value.into_any_element()
    }
}

impl Render for Icon {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text_color = self.text_color.unwrap_or_else(|| cx.theme().foreground);
        let text_size = window.text_style().font_size.to_pixels(window.rem_size());
        let has_base_size = self.style.size.width.is_some() || self.style.size.height.is_some();
        let mut base = svg();
        *base.style() = self.style.clone();

        base.flex_shrink_0()
            .text_color(text_color)
            .when(!has_base_size, |this| this.size(text_size))
            .when_some(self.size, apply_size)
            .path(self.path.clone())
    }
}

fn apply_size(icon: Svg, size: Size) -> Svg {
    match size {
        Size::Size(px) => icon.size(px),
        Size::XSmall => icon.size_3(),
        Size::Small => icon.size_3p5(),
        Size::Medium => icon.size_4(),
        Size::Large => icon.size_6(),
    }
}
