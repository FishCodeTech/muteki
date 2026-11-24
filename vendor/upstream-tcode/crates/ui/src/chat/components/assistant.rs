use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::copy::copy_button;
use crate::widgets::menu::{ContextMenuExt as _, CopyText, PopupMenu};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AppContext as _, ClickEvent, Context, Div,
    Entity, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};

use crate::markdown::parse::ParsedDocument;
use crate::markdown::{MarkdownState, MarkdownView, MenuExtension};

use super::super::model::{MdSync, md_sync};

/// Markdown state mirrored from a timeline entry.
pub(crate) struct MdState {
    pub(crate) state: Entity<MarkdownState>,
    pub(crate) synced: Arc<str>,
}

impl MdState {
    pub(crate) fn new(text: &str, cx: &mut App) -> Self {
        Self {
            state: cx.new(|cx| MarkdownState::new(text, cx)),
            synced: Arc::from(text),
        }
    }

    pub(crate) fn from_parsed(text: &str, parsed: ParsedDocument, cx: &mut App) -> Self {
        Self {
            state: cx.new(|cx| MarkdownState::from_parsed(text, parsed, cx)),
            synced: Arc::from(text),
        }
    }

    pub(crate) fn sync(&mut self, text: String, cx: &mut App) {
        match md_sync(&self.synced, &text) {
            MdSync::Noop => {}
            MdSync::Push(delta) => {
                self.state
                    .update(cx, |state, cx| state.push_str(&delta, cx));
                self.synced = Arc::from(text);
            }
            MdSync::Reset => {
                self.state.update(cx, |state, cx| state.set_text(&text, cx));
                self.synced = Arc::from(text);
            }
        }
    }
}

pub(crate) struct AssistantData<'a> {
    pub(crate) id: &'a str,
    pub(crate) text: &'a str,
    pub(crate) cwd: &'a Path,
    pub(crate) markdown: Option<Entity<MarkdownState>>,
    pub(crate) pinned: bool,
    pub(crate) compact: bool,
    pub(crate) show_actions: bool,
    pub(crate) copied: bool,
}

/// The message's own context-menu item: the whole text, as the hover Copy
/// button copies it. Offered inside the Markdown (below the view's own
/// items) and on the margin around it.
pub(crate) fn copy_message_items(text: Arc<str>) -> MenuExtension {
    Rc::new(move |menu: PopupMenu, _: &mut Window, _: &mut App| {
        menu.menu(
            crate::tr!("chat.copy_message").into_owned(),
            Box::new(CopyText(text.to_string())),
        )
    })
}

/// An assistant message: rendered markdown plus a hover-revealed Copy action.
pub(crate) fn assistant(
    data: AssistantData<'_>,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let AssistantData {
        id,
        text,
        cwd,
        markdown,
        pinned,
        compact,
        show_actions,
        copied,
    } = data;
    let menu_items = copy_message_items(Arc::from(text));
    let content = markdown.map_or_else(
        || div().child(text.to_string()).into_any_element(),
        |markdown| {
            MarkdownView::new(&markdown)
                .compact_headings(true)
                .selectable(true)
                .base_dir(cwd)
                .menu_extension(menu_items.clone())
                .into_any_element()
        },
    );
    let message = v_flex().w_full().items_start().gap(px(2.)).child(
        div()
            .w_full()
            .text_token(&cx.theme().tokens.typography.md)
            .child(content),
    );
    let margin_menu = move |menu: PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
        menu_items(menu, window, cx)
    };

    if !show_actions {
        return message.context_menu(margin_menu).into_any_element();
    }

    let group_key = SharedString::from(format!("assistant-{id}"));
    let actions = h_flex().gap(px(2.)).items_center().child(copy_button(
        &format!("assistant:{id}"),
        copied,
        compact,
        on_copy,
        cx,
    ));
    message
        .group(group_key.clone())
        .child(
            reserve_action_row(actions, group_key, pinned, compact).with_animation(
                SharedString::from(format!("assistant-actions-{id}")),
                Animation::new(Duration::from_millis(400)),
                |element, delta| element.opacity(delta),
            ),
        )
        .context_menu(margin_menu)
        .into_any_element()
}

/// Reserve action-row height so hover visibility never shifts the timeline.
pub(crate) fn reserve_action_row(
    actions: Div,
    group_key: SharedString,
    pinned: bool,
    compact: bool,
) -> Div {
    div()
        .h(px(if compact {
            crate::material::TOUCH_TARGET
        } else {
            crate::material::CHAT_ACTION_ROW_HEIGHT
        }))
        .flex()
        .items_center()
        .when(!pinned && !compact, |this| {
            this.invisible()
                .group_hover(group_key, |style| style.visible())
        })
        .child(actions)
}

/// The geometry every message action shares: a 24px icon-only square with a
/// 6px radius, quiet until hovered. The label lives in the tooltip, so a row of
/// actions reads as icons, not as a row of little labelled controls.
///
/// The icon carries its own muted color because a Ghost button paints
/// `foreground` over any color set on the button itself.
#[cfg(test)]
mod tests {
    use super::MdState;
    use crate::chat::model::plain_text_as_markdown;
    use crate::markdown::MarkdownState;
    use gpui::{AppContext as _, Entity, TestAppContext};

    fn rendered(state: &Entity<MarkdownState>, cx: &mut TestAppContext) -> String {
        state.read_with(cx, |state, _| state.rendered_text())
    }

    #[gpui::test]
    fn plain_text_markdown_escapes_inline_and_block_syntax(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let cases = [
            "*not italic* _nor this_ **nor bold**",
            "# not a heading",
            "- not a list",
            "1. not a list",
            "`not code`",
            "before\n``` not a fence\n``` still not a fence",
            "[not a link](https://example.com)",
            "你好，世界！（测试）",
            "&#32; literal entity",
            "<div>not html</div>",
            r"Literal \(x_i\), \[y^2\], $z$ and $$w$$",
        ];

        let cases = cases
            .into_iter()
            .map(|input| (input, format!("{input}\n")))
            .chain([
                ("line one\nline two", "line one\nline two\n".into()),
                ("para one\n\npara two", "para one\npara two\n".into()),
                (
                    "    let x = 1;\n        nested();",
                    "    let x = 1;\n        nested();\n".into(),
                ),
                ("\tindented with a tab", "\tindented with a tab\n".into()),
            ]);
        for (input, expected) in cases {
            let markdown = plain_text_as_markdown(input);
            let state = cx.update(|cx| cx.new(|cx| MarkdownState::new(&markdown, cx)));
            assert_eq!(rendered(&state, cx), expected, "input: {input:?}");
        }
    }

    #[gpui::test]
    fn markdown_mirror_keeps_rendered_text_coherent_across_append_rewrite_shrink_and_clear(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let mut md = cx.update(|cx| MdState::new("", cx));
        for (text, expected) in [
            ("", ""),
            ("Seed", "Seed\n"),
            ("Seed", "Seed\n"),
            ("Seed tail 文", "Seed tail 文\n"),
            ("Replacement", "Replacement\n"),
            ("Replace", "Replace\n"),
            ("new", "new\n"),
            ("new **value**", "new value\n"),
            ("", ""),
        ] {
            cx.update(|cx| md.sync(text.into(), cx));
            assert_eq!(md.synced.as_ref(), text);
            assert_eq!(rendered(&md.state, cx), expected);
        }
    }
}
