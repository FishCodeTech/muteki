use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use gpui::{
    Action, AsKeystroke, IntoElement, KeyContext, Keystroke, ParentElement as _, RenderOnce,
    StyleRefinement, Styled, Window, div, prelude::FluentBuilder as _, relative,
};
use gpui_base::StyledExt as _;

#[derive(IntoElement, Clone, Debug)]
pub struct Kbd {
    style: StyleRefinement,
    stroke: Keystroke,
    appearance: bool,
    outline: bool,
}

impl From<Keystroke> for Kbd {
    fn from(stroke: Keystroke) -> Self {
        Self::new(stroke)
    }
}

impl Kbd {
    pub fn new(stroke: Keystroke) -> Self {
        Self {
            style: StyleRefinement::default(),
            stroke,
            appearance: true,
            outline: false,
        }
    }

    pub fn appearance(mut self, appearance: bool) -> Self {
        self.appearance = appearance;
        self
    }

    pub fn outline(mut self) -> Self {
        self.outline = true;
        self
    }

    pub fn binding_for_action(
        action: &dyn Action,
        context: Option<&str>,
        window: &Window,
    ) -> Option<Self> {
        let context = context.and_then(|context| KeyContext::parse(context).ok());
        let binding = match context {
            Some(context) => {
                window.highest_precedence_binding_for_action_in_context(action, context)
            }
            None => window.highest_precedence_binding_for_action(action),
        }?;
        binding
            .keystrokes()
            .first()
            .map(|key| Self::new(key.as_keystroke().clone()))
    }

    pub fn format(key: &Keystroke) -> String {
        #[cfg(target_os = "macos")]
        const SEPARATOR: &str = "";
        #[cfg(not(target_os = "macos"))]
        const SEPARATOR: &str = "+";

        let mut parts = Vec::new();
        if key.modifiers.control {
            parts.push(if cfg!(target_os = "macos") {
                "⌃"
            } else {
                "Ctrl"
            });
        }
        if key.modifiers.alt {
            parts.push(if cfg!(target_os = "macos") {
                "⌥"
            } else {
                "Alt"
            });
        }
        if key.modifiers.shift {
            parts.push(if cfg!(target_os = "macos") {
                "⇧"
            } else {
                "Shift"
            });
        }
        if key.modifiers.platform {
            parts.push(if cfg!(target_os = "macos") {
                "⌘"
            } else {
                "Win"
            });
        }

        let key_name = match key.key.as_str() {
            "space" => "Space".to_string(),
            "backspace" | "delete" if cfg!(target_os = "macos") => "⌫".to_string(),
            "backspace" => "Backspace".to_string(),
            "delete" => "Delete".to_string(),
            "escape" if cfg!(target_os = "macos") => "⎋".to_string(),
            "escape" => "Esc".to_string(),
            "enter" if cfg!(target_os = "macos") => "⏎".to_string(),
            "enter" => "Enter".to_string(),
            "pagedown" => "Page Down".to_string(),
            "pageup" => "Page Up".to_string(),
            "left" if cfg!(target_os = "macos") => "←".to_string(),
            "right" if cfg!(target_os = "macos") => "→".to_string(),
            "up" if cfg!(target_os = "macos") => "↑".to_string(),
            "down" if cfg!(target_os = "macos") => "↓".to_string(),
            key if key.len() == 1 => key.to_uppercase(),
            key => {
                let mut chars = key.chars();
                chars.next().map_or_else(String::new, |first| {
                    format!("{}{}", first.to_uppercase(), chars.collect::<String>())
                })
            }
        };
        parts.push(&key_name);
        parts.join(SEPARATOR)
    }
}

impl Styled for Kbd {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Kbd {
    fn render(self, _: &mut gpui::Window, cx: &mut gpui::App) -> impl IntoElement {
        if !self.appearance {
            return Self::format(&self.stroke).into_any_element();
        }
        div()
            .text_color(cx.theme().muted_foreground)
            .bg(cx.theme().muted)
            .when(self.outline, |this| {
                this.border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
            })
            .py_0p5()
            .px_1()
            .min_w_5()
            .text_center()
            .rounded(crate::material::radius_chip(cx))
            .text_token(&cx.theme().tokens.typography.xs)
            .line_height(relative(1.))
            .whitespace_normal()
            .flex_shrink_0()
            .refine_style(&self.style)
            .child(Self::format(&self.stroke))
            .into_any_element()
    }
}
