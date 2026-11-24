//! NSH's document renderer provides Markdown, selectable code, copy and syntect.
use crate::theme::Palette;
use nsh_base::{TextViewCodeAction, TextViewPart, TextViewParts, text_view};
use nsh_core::prelude::*;
pub fn reply(p: Palette, content: impl Fn() -> String + 'static) -> impl IntoElement {
    provide_context(
        TextViewParts::new()
            .part(TextViewPart::Heading1, move |s| {
                s.text_size(px(22.0))
                    .line_height(px(30.0))
                    .text_color(move || p.ink())
            })
            .part(TextViewPart::Heading2, move |s| {
                s.text_size(px(18.0))
                    .line_height(px(26.0))
                    .text_color(move || p.ink())
            })
            .part(TextViewPart::Heading3, move |s| {
                s.text_size(px(16.0))
                    .line_height(px(24.0))
                    .text_color(move || p.ink())
            })
            .part(TextViewPart::CodeBlock, move |s| {
                s.bg(move || p.sidebar()).rounded(px(8.0))
            })
            .part(TextViewPart::CodeBlockCode, move |s| {
                s.font_family("Menlo")
                    .text_size(px(12.0))
                    .line_height(px(20.0))
            })
            .part(TextViewPart::Link, |s| s.text_color(rgb(0x56779f))),
    );
    text_view()
        .markdown(move || Some(SharedString::from(content())))
        .selectable(true)
        .code_block_actions(vec![TextViewCodeAction::new("copy").with_label("复制代码")])
        .gap(px(12.0))
        .text_size(px(14.0))
        .line_height(px(23.0))
        .text_color(move || p.ink())
}
