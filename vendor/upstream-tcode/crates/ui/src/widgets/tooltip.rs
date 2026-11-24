use crate::{theme::ActiveTheme as _, typography::TextRole as _, widgets::Kbd};
use gpui::{
    Action, AnyElement, AnyView, App, AppContext as _, Context, IntoElement, ParentElement as _,
    Render, SharedString, StyleRefinement, Styled, Window, div, prelude::FluentBuilder as _,
};
use gpui_base::StyledExt as _;

enum TooltipContent {
    Text(SharedString),
    Element(ElementBuilder),
}

type ElementBuilder = Box<dyn Fn(&mut Window, &mut App) -> AnyElement>;

pub struct Tooltip {
    style: StyleRefinement,
    content: TooltipContent,
    key_binding: Option<Kbd>,
    action: Option<(Box<dyn Action>, Option<SharedString>)>,
}

impl Tooltip {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self {
            style: StyleRefinement::default(),
            content: TooltipContent::Text(text.into()),
            key_binding: None,
            action: None,
        }
    }

    pub fn element<E, F>(builder: F) -> Self
    where
        E: IntoElement,
        F: Fn(&mut Window, &mut App) -> E + 'static,
    {
        Self {
            style: StyleRefinement::default(),
            content: TooltipContent::Element(Box::new(move |window, cx| {
                builder(window, cx).into_any_element()
            })),
            key_binding: None,
            action: None,
        }
    }

    pub fn action(mut self, action: &dyn Action, context: Option<&str>) -> Self {
        self.action = Some((action.boxed_clone(), context.map(SharedString::new)));
        self
    }

    pub fn key_binding(mut self, key_binding: Option<Kbd>) -> Self {
        self.key_binding = key_binding;
        self
    }

    pub fn build(self, _: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|_| self).into()
    }
}

impl Styled for Tooltip {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Render for Tooltip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let key_binding = self.key_binding.clone().or_else(|| {
            self.action.as_ref().and_then(|(action, context)| {
                Kbd::binding_for_action(action.as_ref(), context.as_deref(), window)
            })
        });
        gpui_base::Tooltip::new("tooltip-popup")
            .h_flex()
            .font_family(cx.theme().font_family.clone())
            .m_3()
            .bg(cx.theme().popover)
            .text_color(cx.theme().foreground)
            .border_1()
            .border_color(cx.theme().border)
            .shadow_md()
            .rounded(crate::material::radius_chip(cx))
            .justify_between()
            .py_0p5()
            .px_2()
            .text_token(&cx.theme().tokens.typography.sm)
            .gap_3()
            .refine_style(&self.style)
            .child(div().child(match &self.content {
                TooltipContent::Text(text) => text.clone().into_any_element(),
                TooltipContent::Element(builder) => builder(window, cx),
            }))
            .when_some(key_binding, |this, kbd| {
                this.child(
                    div()
                        .text_token(&cx.theme().tokens.typography.xs)
                        .flex_shrink_0()
                        .text_color(cx.theme().muted_foreground)
                        .child(kbd.appearance(false)),
                )
            })
    }
}
