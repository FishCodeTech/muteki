use crate::{
    icon::{Icon, IconName},
    sizing::{Sizable, Size},
};
use gpui::{
    Animation, AnimationExt as _, App, Hsla, IntoElement, ParentElement as _, RenderOnce,
    Styled as _, Transformation, Window, div, ease_in_out, percentage, prelude::FluentBuilder as _,
};
use std::time::Duration;

/// A rotating loader icon. With no explicit size it takes the current font
/// size, like [`Icon`].
#[derive(IntoElement)]
pub struct Spinner {
    size: Option<Size>,
    icon: Icon,
    color: Option<Hsla>,
}

impl Spinner {
    pub fn new() -> Self {
        Self {
            size: None,
            icon: Icon::new(IconName::Loader),
            color: None,
        }
    }

    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = icon.into();
        self
    }

    pub fn color(mut self, color: Hsla) -> Self {
        self.color = Some(color);
        self
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

impl Sizable for Spinner {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = Some(size.into());
        self
    }
}

impl RenderOnce for Spinner {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div().child(
            self.icon
                .when_some(self.size, |icon, size| icon.with_size(size))
                .when_some(self.color, |icon, color| icon.text_color(color))
                .with_animation(
                    "circle",
                    Animation::new(Duration::from_secs_f64(0.8))
                        .repeat()
                        .with_easing(ease_in_out),
                    |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
                ),
        )
    }
}
