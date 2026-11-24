//! Markdown view element adapted from gpui-component's Apache-2.0
//! `text/text_view.rs` implementation.

use gpui::prelude::FluentBuilder as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::widgets::input::SelectAll;
use crate::widgets::menu::{ContextMenuExt as _, CopyText, OpenUrl, PopupMenu};
use gpui::{
    Action, AnyElement, App, Bounds, Element, ElementId, Entity, GlobalElementId, Hitbox,
    HitboxBehavior, InspectorElementId, InteractiveElement as _, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, ParentElement as _, Pixels, StyleRefinement, Styled, Window, div,
};
use gpui_base::StyledExt as _;
use serde::Deserialize;

use super::{
    link_target::LinkTarget,
    selection::{SelectionArea, SelectionDocument},
    state::{MarkdownState, PendingContextTarget},
};

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown, no_json)]
struct OpenImage {
    url: String,
    title: String,
}

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_markdown, no_json)]
struct ShowMathSource {
    path: String,
    source: bool,
}

/// Items a surface adds below the view's own: a message offers to copy or
/// rewind itself, a plan to download itself.
pub type MenuExtension = Rc<dyn Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu>;

/// A GPUI element that renders an [`Entity<MarkdownState>`].
#[derive(Clone)]
pub struct MarkdownView {
    id: ElementId,
    state: Entity<MarkdownState>,
    style: StyleRefinement,
    selectable: Option<bool>,
    selection_entry: Option<(Entity<SelectionDocument>, u64)>,
    compact_headings: Option<bool>,
    base_dir: Option<PathBuf>,
    menu_extension: Option<MenuExtension>,
}

impl MarkdownView {
    /// Create a view for an existing Markdown state entity.
    pub fn new(state: &Entity<MarkdownState>) -> Self {
        Self {
            id: ElementId::Name(state.entity_id().to_string().into()),
            state: state.clone(),
            style: StyleRefinement::default(),
            selectable: None,
            selection_entry: None,
            compact_headings: None,
            base_dir: None,
            menu_extension: None,
        }
    }

    /// Render headings at chat scale (h1 17px / h2 15px / h3+ 13.5px) instead
    /// of document scale.
    pub fn compact_headings(mut self, compact: bool) -> Self {
        self.compact_headings = Some(compact);
        self
    }

    /// Make the text selectable on its own.
    pub fn selectable(mut self, selectable: bool) -> Self {
        self.selectable = Some(selectable);
        self
    }

    /// Make the text entry `entry` of `document`, selected together with the
    /// document's other entries in a [`SelectionArea`] its owner draws.
    pub fn selection_entry(mut self, document: &Entity<SelectionDocument>, entry: u64) -> Self {
        self.selection_entry = Some((document.clone(), entry));
        self
    }

    /// Set the directory used to resolve relative Markdown links.
    pub fn base_dir(mut self, base_dir: impl Into<PathBuf>) -> Self {
        self.base_dir = Some(base_dir.into());
        self
    }

    /// Append the surface's own items to the view's context menu.
    pub fn menu_extension(mut self, extension: MenuExtension) -> Self {
        self.menu_extension = Some(extension);
        self
    }
}

impl Styled for MarkdownView {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl IntoElement for MarkdownView {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// What the view laid out for the frame.
pub struct MarkdownViewPrepaintState {
    hitbox: Hitbox,
}

impl Element for MarkdownView {
    type RequestLayoutState = (Entity<MarkdownState>, AnyElement);
    type PrepaintState = MarkdownViewPrepaintState;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let state = self.state.clone();
        state.update(cx, |state, cx| {
            if let Some((document, entry)) = &self.selection_entry {
                state.set_selection_entry(document, *entry, cx);
            } else if let Some(selectable) = self.selectable {
                state.set_selectable(selectable, cx);
            }
            state.clear_own_selection_on_focus_out(window, cx);
            if let Some(compact) = self.compact_headings {
                state.set_compact_headings(compact, cx);
            }
            if let Some(base_dir) = &self.base_dir {
                state.set_base_dir(Some(base_dir.clone()), cx);
            }
        });
        let focus_handle = state.read(cx).focus_handle.clone();
        let element = div()
            .key_context(super::CONTEXT)
            .track_focus(&focus_handle)
            .w_full()
            .relative()
            .on_action({
                let state = state.clone();
                move |_: &SelectAll, _, cx| {
                    if !select_whole(&state, cx) {
                        cx.propagate();
                    }
                }
            })
            .on_action({
                let state = state.clone();
                move |action: &OpenImage, window, cx| {
                    let source = state
                        .read(cx)
                        .opened_image_source(&action.url.clone().into());
                    crate::image_viewer::open(source, action.title.clone(), window, cx);
                }
            })
            .on_action({
                let state = state.clone();
                move |action: &ShowMathSource, _, cx| {
                    state.update(cx, |state, cx| {
                        state.set_math_source(&action.path, action.source, cx)
                    });
                }
            })
            .child(state.clone())
            .refine_style(&self.style)
            .context_menu({
                let state = state.clone();
                let extension = self.menu_extension.clone();
                move |menu, window, cx| {
                    let (selected, pending, base_dir) = {
                        let markdown = state.read(cx);
                        (
                            markdown
                                .selection
                                .as_ref()
                                .map(|membership| membership.document.read(cx).selected_text(cx)),
                            markdown.pending_context.clone(),
                            markdown.base_dir().map(Path::to_path_buf),
                        )
                    };
                    let menu = match selected {
                        Some(selected) => menu.selection_items(selected, true),
                        None => menu,
                    };
                    let menu = match pending {
                        None => menu,
                        Some(PendingContextTarget::Link(pending)) => match pending.target {
                            LinkTarget::Web(url) => menu
                                .separator()
                                .menu(
                                    crate::tr!("markdown.link_open").into_owned(),
                                    Box::new(OpenUrl(url.clone())),
                                )
                                .menu(
                                    crate::tr!("markdown.link_copy_address").into_owned(),
                                    Box::new(CopyText(pending.raw_url.to_string())),
                                )
                                .menu(
                                    crate::tr!("markdown.link_copy_text").into_owned(),
                                    Box::new(CopyText(pending.text.to_string())),
                                )
                                .when(
                                    extension.is_some()
                                        && tcode_core::pull_request::is_pull_request_url(&url),
                                    |menu| {
                                        menu.menu(
                                            crate::tr!("pull_requests.link_to_thread").into_owned(),
                                            Box::new(crate::pull_requests::LinkUrl(url)),
                                        )
                                    },
                                ),
                            LinkTarget::Local(path) => menu
                                .separator()
                                .path_items(&path.to_string_lossy(), base_dir.as_deref()),
                            LinkTarget::Thread(anchor) => menu
                                .separator()
                                .menu(
                                    crate::tr!("markdown.link_open").into_owned(),
                                    Box::new(anchor),
                                )
                                .menu(
                                    crate::tr!("markdown.link_copy_address").into_owned(),
                                    Box::new(CopyText(pending.raw_url.to_string())),
                                ),
                        },
                        Some(PendingContextTarget::CodeBlock(code)) => menu.separator().menu(
                            crate::tr!("markdown.code_copy").into_owned(),
                            Box::new(CopyText(code)),
                        ),
                        Some(PendingContextTarget::Math {
                            path,
                            latex,
                            show_source,
                        }) => menu
                            .separator()
                            .menu(
                                crate::tr!("markdown.math_copy").into_owned(),
                                Box::new(CopyText(latex)),
                            )
                            .when_some(show_source, |menu, show_source| {
                                menu.menu(
                                    crate::tr!(if show_source {
                                        "markdown.math_show_rendered"
                                    } else {
                                        "markdown.math_show_source"
                                    })
                                    .into_owned(),
                                    Box::new(ShowMathSource {
                                        path,
                                        source: !show_source,
                                    }),
                                )
                            }),
                        Some(PendingContextTarget::Image { url, title }) => {
                            let menu = menu.separator().menu(
                                crate::tr!("markdown.open_image").into_owned(),
                                Box::new(OpenImage {
                                    url: url.to_string(),
                                    title,
                                }),
                            );
                            match state.read(cx).resolve_link(&url) {
                                LinkTarget::Web(address) => menu.menu(
                                    crate::tr!("markdown.image_copy_address").into_owned(),
                                    Box::new(CopyText(address)),
                                ),
                                LinkTarget::Local(path) => menu
                                    .separator()
                                    .path_items(&path.to_string_lossy(), base_dir.as_deref()),
                                LinkTarget::Thread(_) => menu,
                            }
                        }
                    };
                    match &extension {
                        Some(extension) => extension(menu.separator(), window, cx),
                        None => menu,
                    }
                }
            })
            .into_any_element();
        let mut element = match state.read(cx).own_document() {
            Some(document) => SelectionArea::new(&document, element)
                .w_full()
                .into_any_element(),
            None => element,
        };
        let layout_id = element.request_layout(window, cx);
        (layout_id, (state, element))
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        request_layout.0.read(cx).begin_frame();
        request_layout.1.prepaint(window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        if let Some(membership) = &request_layout.0.read(cx).selection {
            membership.document.read(cx).register_participant(
                membership.entry,
                bounds,
                request_layout.0.downgrade(),
            );
        }
        MarkdownViewPrepaintState { hitbox }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let hitbox = &prepaint.hitbox;
        // Capture-phase so this runs before the Inline children's bubble-phase
        // handlers repopulate the pending link: a right-click on a non-link
        // area must not resurface the previous link's context menu.
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let state = request_layout.0.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase.capture()
                    && event.button == MouseButton::Right
                    && hitbox.is_hovered(window)
                {
                    state.update(cx, |state, cx| state.set_pending_context(None, cx));
                }
            }
        });
        request_layout.1.paint(window, cx);
    }
}

/// Select everything the view shows, in its document. Whether it could.
fn select_whole(state: &Entity<MarkdownState>, cx: &mut App) -> bool {
    let Some(membership) = state.read(cx).selection.clone() else {
        return false;
    };
    membership.document.update(cx, |document, cx| {
        document.select_entry(membership.entry, cx)
    });
    true
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{
        AppContext as _, Context, Entity, IntoElement, ListAlignment, ListState, Modifiers,
        MouseButton, Render, StatefulInteractiveElement as _, TestAppContext, VisualTestContext,
        Window, div, point, px,
    };

    use super::*;
    use crate::markdown::selection::SelectionDocument;

    /// What the keyboard's Copy puts on the clipboard.
    fn copied(cx: &mut VisualTestContext) -> String {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new()));
        cx.dispatch_action(gpui_base::input::Copy);
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default()
    }

    /// The text `markdown`'s document has selected.
    fn selected_in(markdown: &Entity<MarkdownState>, cx: &mut VisualTestContext) -> String {
        markdown.read_with(cx, |state, cx| {
            state
                .selection
                .as_ref()
                .map(|membership| membership.document.read(cx).selected_text(cx))
                .unwrap_or_default()
        })
    }

    struct TestRoot {
        markdown: Entity<MarkdownState>,
    }

    struct CrossViewRoot {
        first: Entity<MarkdownState>,
        second: Entity<MarkdownState>,
        document: Entity<SelectionDocument>,
    }

    struct OuterListRoot {
        markdown: Entity<MarkdownState>,
        list_state: ListState,
    }

    impl TestRoot {
        fn new(text: &str, cx: &mut Context<Self>) -> Self {
            let text = text.to_string();
            Self {
                markdown: cx.new(|cx| MarkdownState::new(&text, cx)),
            }
        }
    }

    impl Render for TestRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(320.))
                .child(
                    div()
                        .h(px(24.))
                        .overflow_hidden()
                        .child(MarkdownView::new(&self.markdown).selectable(true)),
                )
                .child(div().h(px(40.)).child("footer"))
        }
    }

    impl CrossViewRoot {
        fn new(cx: &mut Context<Self>) -> Self {
            let first = cx.new(|cx| MarkdownState::new("Hello world", cx));
            let second = cx.new(|cx| MarkdownState::new("Second message", cx));
            let views = [first.clone(), second.clone()];
            let document = cx.new(|_| {
                SelectionDocument::new(Rc::new(move |range, cx| {
                    views
                        .iter()
                        .enumerate()
                        .map(|(entry, view)| (entry as u64, view.read(cx).parsed_root()))
                        .filter(|(entry, _)| range.contains(entry))
                        .collect()
                }))
            });
            Self {
                first,
                second,
                document,
            }
        }
    }

    struct MultiBlockRoot {
        markdown: Entity<MarkdownState>,
    }

    struct RightClickRoot {
        markdown: Entity<MarkdownState>,
        bubbled_right_clicks: Rc<RefCell<usize>>,
    }

    impl RightClickRoot {
        fn new(cx: &mut Context<Self>) -> Self {
            Self {
                markdown: cx
                    .new(|cx| MarkdownState::new("Alpha beta\n\n[click](https://example.com)", cx)),
                bubbled_right_clicks: Rc::default(),
            }
        }
    }

    impl Render for RightClickRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let bubbled = self.bubbled_right_clicks.clone();
            div()
                .size_full()
                .child(MarkdownView::new(&self.markdown).selectable(true))
                .on_mouse_down(MouseButton::Right, move |_, _, _| {
                    *bubbled.borrow_mut() += 1;
                })
        }
    }

    impl MultiBlockRoot {
        fn with_text(text: &str, cx: &mut Context<Self>) -> Self {
            Self {
                markdown: cx.new(|cx| MarkdownState::new(text, cx)),
            }
        }
    }

    impl Render for MultiBlockRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(MarkdownView::new(&self.markdown).selectable(true))
        }
    }

    /// A thread dragged from a list above the text and dropped on a pane
    /// below it: the drop takes the release.
    struct DropRoot {
        markdown: Entity<MarkdownState>,
    }

    #[derive(Clone)]
    struct Carried;

    struct CarriedView;

    impl Render for CarriedView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size(px(20.))
        }
    }

    impl Render for DropRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(
                    div()
                        .id("drag-source")
                        .w_full()
                        .h(px(40.))
                        .on_drag(Carried, |_, _, _, cx| cx.new(|_| CarriedView)),
                )
                .child(
                    div()
                        .h(px(40.))
                        .child(MarkdownView::new(&self.markdown).selectable(true)),
                )
                .child(
                    div()
                        .id("drop-target")
                        .w_full()
                        .h(px(40.))
                        .on_drop(|_: &Carried, _, _| {}),
                )
        }
    }

    impl Render for CrossViewRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            SelectionArea::new(
                &self.document,
                div()
                    .size_full()
                    .pt(px(10.))
                    .child(
                        div().h(px(40.)).child(
                            MarkdownView::new(&self.first).selection_entry(&self.document, 0),
                        ),
                    )
                    .child(
                        div().h(px(40.)).child(
                            MarkdownView::new(&self.second).selection_entry(&self.document, 1),
                        ),
                    ),
            )
            .size_full()
        }
    }

    impl OuterListRoot {
        fn new(text: &str, cx: &mut Context<Self>) -> Self {
            let text = text.to_string();
            Self {
                markdown: cx.new(|cx| MarkdownState::new(&text, cx)),
                list_state: ListState::new(1, ListAlignment::Top, px(100.)),
            }
        }
    }

    impl Render for OuterListRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let markdown = self.markdown.clone();
            div().size_full().child(
                gpui::list(self.list_state.clone(), move |_, _, _| {
                    div()
                        .w_full()
                        .child(MarkdownView::new(&markdown).selectable(true))
                        .into_any_element()
                })
                .size_full(),
            )
        }
    }

    #[gpui::test]
    fn long_blocks_in_a_timeline_row_lay_out_only_their_visible_lines(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let code = (0..3_000)
            .map(|ix| format!("let line_{ix} = {ix};"))
            .collect::<Vec<_>>()
            .join("\n");
        let list = (0..1_000)
            .map(|ix| format!("- item {ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        for (source, near, far, last) in [
            (
                format!("```rust\n{code}\n```"),
                "markdown-code-line-5",
                "markdown-code-line-2500",
                "markdown-code-line-2999",
            ),
            (
                list,
                "markdown-list-item-root-0-5",
                "markdown-list-item-root-0-900",
                "markdown-list-item-root-0-999",
            ),
        ] {
            let (view, cx) = cx.add_window_view(|_, cx| OuterListRoot::new(&source, cx));
            let cx: &mut VisualTestContext = cx;
            cx.simulate_resize(gpui::size(px(393.), px(852.)));
            for _ in 0..2 {
                cx.run_until_parked();
                cx.update(|window, cx| _ = window.draw(cx));
            }
            assert!(cx.debug_bounds(near).is_some());
            assert!(
                cx.debug_bounds(far).is_none(),
                "{far} was laid out while the row shows only the top of the block"
            );

            view.update(cx, |root, _| root.list_state.scroll_to_end());
            cx.update(|window, cx| _ = window.draw(cx));
            assert!(cx.debug_bounds(last).is_some());
            assert!(cx.debug_bounds(near).is_none());
        }
    }

    #[gpui::test]
    fn a_paragraph_flowing_text_around_an_image_lays_out_across_line_breaks(
        cx: &mut TestAppContext,
    ) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        // As GitHub stores a pull request body: CRLF breaks inside one paragraph.
        let source = "Before ![shot](https://example.invalid/shot.png)\r\nafter the image\nand on";
        let (_, cx) = cx.add_window_view(|_, cx| OuterListRoot::new(source, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }
    }

    #[gpui::test]
    fn drag_across_a_long_code_block_copies_lines_never_painted(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let lines = (0..3_000)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>();
        let source = format!("```text\n{}\n```", lines.join("\n"));
        let (view, cx) = cx.add_window_view(|_, cx| OuterListRoot::new(&source, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }

        let anchor = cx.debug_bounds("markdown-code-line-5").unwrap();
        let first = cx.debug_bounds("markdown-code-line-30").unwrap();
        cx.simulate_mouse_down(
            point(anchor.left() + px(1.), anchor.center().y),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_move(
            point(px(380.), first.center().y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.update(|window, cx| _ = window.draw(cx));
        view.update(cx, |root, _| root.list_state.scroll_to_end());
        cx.update(|window, cx| _ = window.draw(cx));
        let last = cx.debug_bounds("markdown-code-line-2999").unwrap();
        let end = point(px(380.), last.center().y);
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));

        let selected = copied(cx);
        assert_eq!(selected, lines[5..].join("\n"));
    }

    #[gpui::test]
    fn math_render_source_switch_and_streaming_fallback(cx: &mut TestAppContext) {
        let (root, cx) = open_timeline_row("```latex\n1+2=3\n```", cx);
        assert!(cx.debug_bounds("markdown-math-root-0").is_some());
        assert!(cx.debug_bounds("markdown-code-line-0").is_none());
        let first = cx.debug_bounds("math-glyph-0").unwrap();
        let end = cx.debug_bounds("math-glyph-3").unwrap();
        cx.simulate_mouse_down(
            point(first.left(), first.center().y),
            MouseButton::Left,
            Modifiers::default(),
        );
        drag_to(cx, point(end.left(), end.center().y));
        assert_eq!(release(cx, point(end.left(), end.center().y)), "1+2");
        let block = cx.debug_bounds("markdown-math-root-0").unwrap();
        cx.simulate_mouse_move(block.center(), None, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        let toggle = cx.debug_bounds("markdown-math-toggle-root-0").unwrap();
        cx.simulate_mouse_move(toggle.center(), None, Modifiers::default());
        cx.simulate_click(toggle.center(), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("markdown-code-line-0").is_some());
        let toggle = cx.debug_bounds("markdown-math-toggle-root-0").unwrap();
        cx.simulate_click(toggle.center(), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("markdown-code-line-0").is_none());
        root.update(cx, |root, cx| {
            root.markdown
                .update(cx, |md, cx| md.set_text("```latex\n\\frac{1}{", cx))
        });
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("markdown-code-line-0").is_some());
        root.update(cx, |root, cx| {
            root.markdown
                .update(cx, |md, cx| md.push_str("2}\n```", cx))
        });
        cx.update(|window, cx| _ = window.draw(cx));
        assert!(cx.debug_bounds("markdown-code-line-0").is_none());
        root.update(cx, |root, cx| {
            root.markdown
                .update(cx, |md, cx| md.set_text("before $1+2=3$ after", cx))
        });
        cx.update(|window, cx| _ = window.draw(cx));
        let first = cx.debug_bounds("math-glyph-0").unwrap();
        let end = point(px(380.), first.center().y);
        cx.simulate_mouse_down(
            point(first.left(), first.center().y),
            MouseButton::Left,
            Modifiers::default(),
        );
        drag_to(cx, end);
        assert_eq!(release(cx, end), "1+2=3 after");
    }

    #[gpui::test]
    fn mermaid_fence_paints_a_diagram_and_unparsable_source_stays_code(cx: &mut TestAppContext) {
        let (_, cx) = open_timeline_row(
            "```mermaid\nflowchart LR\n  A --> B\n```\n\n```mermaid\nnot a diagram\n```",
            cx,
        );
        assert!(cx.debug_bounds("markdown-mermaid-root-0").is_some());
        assert!(cx.debug_bounds("markdown-mermaid-root-1").is_none());
        let fallback = cx.debug_bounds("markdown-code-line-0");
        assert!(
            fallback.is_some(),
            "the fence that does not parse keeps its code"
        );
    }

    #[gpui::test]
    fn a_diagram_wider_than_the_row_shrinks_without_letterboxing(cx: &mut TestAppContext) {
        let nodes = (0..12)
            .map(|ix| format!("N{ix}[node {ix}]"))
            .collect::<Vec<_>>();
        let source = format!("```mermaid\nflowchart LR\n  {}\n```", nodes.join(" --> "));
        let (_, cx) = open_timeline_row(&source, cx);
        let padding = px(12.) * 2.;
        let mut aspects = Vec::new();
        for width in [px(393.), px(600.)] {
            cx.simulate_resize(gpui::size(width, px(852.)));
            cx.update(|window, cx| _ = window.draw(cx));
            let block = cx.debug_bounds("markdown-mermaid-root-0").unwrap();
            aspects.push((block.size.height - padding) / (block.size.width - padding));
        }
        assert!(
            (aspects[0] - aspects[1]).abs() < 0.01,
            "the diagram box keeps the picture's aspect ratio at every row width: {aspects:?}"
        );
    }

    #[gpui::test]
    fn code_block_copy_button_copies_the_whole_fence(cx: &mut TestAppContext) {
        let lines = (0..100).map(|ix| format!("line {ix}")).collect::<Vec<_>>();
        let source = format!("```text\n{}\n```", lines.join("\n"));
        let (_, cx) = open_timeline_row(&source, cx);
        let button = cx.debug_bounds("markdown-code-copy-root-0").unwrap();
        cx.simulate_click(button.center(), Modifiers::default());
        cx.run_until_parked();
        let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(copied.as_deref(), Some(lines.join("\n").as_str()));
    }

    fn open_timeline_row<'a>(
        source: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<OuterListRoot>, &'a mut VisualTestContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let source = source.to_string();
        let (view, cx) = cx.add_window_view(|_, cx| OuterListRoot::new(&source, cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }
        (view, cx)
    }

    fn drag_to(cx: &mut VisualTestContext, to: gpui::Point<gpui::Pixels>) {
        cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
    }

    fn release(cx: &mut VisualTestContext, at: gpui::Point<gpui::Pixels>) -> String {
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        copied(cx)
    }

    fn scroll_to_end(view: &Entity<OuterListRoot>, cx: &mut VisualTestContext) {
        view.update(cx, |root, _| root.list_state.scroll_to_end());
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }
    }

    #[gpui::test]
    fn shift_click_extends_from_an_anchor_scrolled_away(cx: &mut TestAppContext) {
        let lines = (0..3_000)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>();
        let items = (0..1_000)
            .map(|ix| format!("item {ix}"))
            .collect::<Vec<_>>();
        for (source, expected, first, last) in [
            (
                format!("```text\n{}\n```", lines.join("\n")),
                lines[5..].join("\n"),
                "markdown-code-line-5",
                "markdown-code-line-2999",
            ),
            (
                items
                    .iter()
                    .map(|item| format!("- {item}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                items[5..].join("\n"),
                "markdown-list-item-text-root-0-5",
                "markdown-list-item-root-0-999",
            ),
        ] {
            let (view, cx) = open_timeline_row(&source, cx);
            let first = cx.debug_bounds(first).unwrap();
            cx.simulate_click(
                point(first.left() + px(1.), first.center().y),
                Modifiers::default(),
            );
            cx.update(|window, cx| _ = window.draw(cx));
            scroll_to_end(&view, cx);

            let end = point(px(380.), cx.debug_bounds(last).unwrap().center().y);
            let shift = Modifiers {
                shift: true,
                ..Default::default()
            };
            cx.simulate_mouse_down(end, MouseButton::Left, shift);
            cx.update(|window, cx| _ = window.draw(cx));
            cx.simulate_mouse_up(end, MouseButton::Left, shift);
            cx.update(|window, cx| _ = window.draw(cx));
            let selected = copied(cx);
            assert_eq!(selected, expected);
        }
    }

    #[gpui::test]
    fn a_scroll_jump_mid_drag_drops_text_left_behind(cx: &mut TestAppContext) {
        let lines = (0..3_000)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>();
        let items = (0..1_000)
            .map(|ix| format!("item {ix}"))
            .collect::<Vec<_>>();
        // Each drag starts on screen in the second item of the root list and
        // turns back within it before the jump.
        for (source, expected, anchor, back, last) in [
            (
                format!("```text\n{}\n```", lines.join("\n")),
                lines[38..].join("\n"),
                "markdown-code-line-38",
                "markdown-code-line-33",
                "markdown-code-line-2999",
            ),
            (
                items
                    .iter()
                    .map(|item| format!("- {item}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                items[12..].join("\n"),
                "markdown-list-item-text-root-0-12",
                "markdown-list-item-text-root-0-9",
                "markdown-list-item-root-0-999",
            ),
        ] {
            let (view, cx) = open_timeline_row(&source, cx);
            let start = |cx: &mut VisualTestContext, selector| {
                let bounds = cx.debug_bounds(selector).unwrap();
                point(bounds.left() + px(1.), bounds.center().y)
            };
            let (anchor, back) = (start(cx, anchor), start(cx, back));
            cx.simulate_mouse_down(anchor, MouseButton::Left, Modifiers::default());
            cx.update(|window, cx| _ = window.draw(cx));
            drag_to(cx, back);
            scroll_to_end(&view, cx);

            let end = point(px(380.), cx.debug_bounds(last).unwrap().center().y);
            drag_to(cx, end);
            assert_eq!(release(cx, end), expected);
        }
    }

    #[gpui::test]
    fn streamed_append_preserves_earlier_item_measurements(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let code = (0..100)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        // A long streaming code block keeps the items of its settled lines.
        // A block that stops being the last gains a gap below its last item.
        for (source, append, settled) in [
            ("stable block\n\nstreaming block".to_string(), " delta", 0),
            (format!("```text\n{code}"), "\nline 100", 2),
            (
                "stable block\n\nsettled block".to_string(),
                "\n\nnew block",
                0,
            ),
            (format!("```text\n{code}\n```"), "\n\nnew block", 2),
        ] {
            let (view, cx) = cx.add_window_view(|_, cx| TestRoot::new(&source, cx));
            let cx: &mut VisualTestContext = cx;
            cx.run_until_parked();
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });

            assert!(view.read_with(cx, |root, cx| {
                root.markdown.read(cx).has_measured_item(settled + 1)
            }));
            view.update(cx, |root, cx| {
                root.markdown.update(cx, |markdown, cx| {
                    markdown.push_str(append, cx);
                    assert!(markdown.has_measured_item(settled));
                    assert!(!markdown.has_measured_item(settled + 1));
                });
            });
        }
    }

    fn paragraphs(count: usize) -> String {
        (0..count)
            .map(|ix| format!("Paragraph {ix}."))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The layout work of the frames `update` causes. A test window draws
    /// whenever an update leaves it dirty, as the platform's next frame would.
    fn layout_work(
        cx: &mut VisualTestContext,
        update: impl FnOnce(&mut VisualTestContext),
    ) -> gpui::LayoutStats {
        cx.update(|window, _| window.reset_layout_stats());
        update(cx);
        cx.update(|window, _| window.layout_stats())
    }

    #[gpui::test]
    fn streamed_appends_lay_out_only_the_trailing_block(cx: &mut TestAppContext) {
        const BLOCKS: usize = 400;
        let (root, cx) = open_timeline_row(&paragraphs(BLOCKS), cx);
        scroll_to_end(&root, cx);
        for (delta, last_block) in [
            (" grows", "markdown-block-399"),
            (" again", "markdown-block-399"),
            ("\n\nA new block.", "markdown-block-400"),
        ] {
            let work = layout_work(cx, |cx| {
                root.update(cx, |root, cx| {
                    root.markdown.update(cx, |md, cx| md.push_str(delta, cx));
                    // The chat remeasures the row of a message that changed.
                    root.list_state.remeasure_items(0..1);
                });
                cx.run_until_parked();
            });
            // One frame takes the delta. A frame that builds a tree of every
            // block, or one more that replaces it, is work in proportion to
            // the whole message.
            assert!(
                work.frames == 1
                    && work.measure_calls < BLOCKS as u64 / 4
                    && work.nodes_created + work.nodes_reused < BLOCKS as u64 / 2,
                "{delta:?}: {work:?}"
            );
            // The row sizes to the grown message: its last block is painted
            // at the bottom of the row the outer list scrolled to.
            assert!(
                cx.debug_bounds(last_block).is_some(),
                "{delta:?}: the trailing block is outside the row"
            );
        }
    }

    #[gpui::test]
    fn a_diagram_resolving_its_size_remeasures_only_its_block(cx: &mut TestAppContext) {
        const BLOCKS: usize = 400;
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (root, cx) = cx.add_window_view(|_, cx| OuterListRoot::new("", cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        let source = format!(
            "```mermaid\nflowchart LR\n  A --> B\n```\n\n{}",
            paragraphs(BLOCKS)
        );
        // The diagram is decoded off the main thread; the frames this update
        // draws lay it out before it has a size.
        root.update(cx, |root, cx| {
            root.markdown.update(cx, |md, cx| md.set_text(&source, cx));
            root.list_state.remeasure_items(0..1);
        });
        let pending = cx
            .debug_bounds("markdown-mermaid-root-0")
            .expect("the diagram block is painted")
            .size
            .height;

        let work = layout_work(cx, |cx| cx.run_until_parked());
        let resolved = cx
            .debug_bounds("markdown-mermaid-root-0")
            .expect("the diagram block is painted")
            .size
            .height;
        assert!(
            resolved != pending,
            "the diagram resolved its size: {pending:?} -> {resolved:?}"
        );
        assert!(
            work.measure_calls < BLOCKS as u64 / 4
                && work.nodes_created + work.nodes_reused < BLOCKS as u64 / 2,
            "resolving one diagram: {work:?}"
        );
    }

    #[gpui::test]
    fn wide_table_shrinks_to_viewport_and_wraps(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let header = (0..20).map(|ix| format!("column-{ix}")).collect::<Vec<_>>();
        let separator = vec!["---"; header.len()];
        let values = (0..header.len())
            .map(|ix| format!("value-{ix}"))
            .collect::<Vec<_>>();
        let overflowing = format!(
            "| {} |\n| {} |\n| {} |",
            header.join(" | "),
            separator.join(" | "),
            values.join(" | ")
        );
        for (case, markdown, wraps, overflows) in [
            ("wide cells", "| column | value |\n| --- | --- |\n| this-cell-is-deliberately-much-wider-than-the-markdown-viewport | another-wide-value |".to_string(), true, false),
            ("short cells", "| a | b |\n| --- | --- |\n| 1 | 2 |".to_string(), false, false),
            ("many columns", overflowing, false, true),
        ] {
            let (_, cx) = cx.add_window_view(|_, cx| TestRoot::new(&markdown, cx));
            let cx: &mut VisualTestContext = cx;
            cx.run_until_parked();
            cx.update(|window, cx| { let _ = window.draw(cx); });
            let track = cx.debug_bounds("markdown-table-track-root-0").expect("table track was painted");
            if overflows {
                let last_cell = cx.debug_bounds("markdown-table-cell-0-19").expect("last cell was painted");
                assert_eq!(last_cell.right() + px(1.), track.right(), "{case}: {last_cell:?}, {track:?}");
            } else {
                assert_eq!(track.size.width, px(320.), "{case}: table must fit the viewport");
            }
            if wraps {
                assert!(track.size.height > px(68.), "{case}: wrapped height {:?}", track.size.height);
            }
        }
    }

    #[gpui::test]
    fn right_aligned_column_justifies_cell_content(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_, cx) = cx.add_window_view(|_, cx| {
            TestRoot::new("| num | label |\n| ---: | --- |\n| 1 | value |", cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let cell = cx
            .debug_bounds("markdown-table-cell-1-0")
            .expect("right-aligned cell was painted");
        let content = cx
            .debug_bounds("markdown-table-cell-content-1-0")
            .expect("cell content was painted");
        // The stretched column is far wider than "1"; justify_end must push the
        // content against the cell's right padding edge (px_2 = 8px).
        assert!(
            cell.right() - content.right() <= px(9.),
            "content {content:?} is not right-justified inside cell {cell:?}"
        );
        assert!(
            content.left() - cell.left() > px(20.),
            "content {content:?} hugs the left edge of cell {cell:?}"
        );
    }

    #[gpui::test]
    fn streaming_table_growth_updates_track_height(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) =
            cx.add_window_view(|_, cx| TestRoot::new("| column |\n| --- |\n| streaming", cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let initial_height = cx
            .debug_bounds("markdown-table-track-root-0")
            .expect("initial table track was painted")
            .size
            .height;

        view.update(cx, |root, cx| {
            root.markdown.update(cx, |markdown, cx| {
                markdown.push_str(
                    "-cell-that-grows-much-wider-and-keeps-growing-until-it-wraps |",
                    cx,
                );
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let grown_height = cx
            .debug_bounds("markdown-table-track-root-0")
            .expect("grown table track was painted")
            .size
            .height;

        assert!(
            grown_height > initial_height,
            "streamed table height stayed at {initial_height:?} instead of growing: {grown_height:?}"
        );
    }

    #[gpui::test]
    fn clipped_markdown_cannot_start_selection(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_view, cx) =
            cx.add_window_view(|_, cx| TestRoot::new("visible\n\nhidden selection text", cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_down(
            point(px(10.), px(34.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(px(90.), px(34.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(px(90.), px(34.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        let selected = copied(cx);
        assert!(selected.is_empty(), "unexpected selection: {selected:?}");
    }

    #[gpui::test]
    fn a_drag_and_drop_across_the_text_leaves_nothing_selected(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| DropRoot {
            markdown: cx.new(|cx| MarkdownState::new("Hello world", cx)),
        });
        let cx: &mut VisualTestContext = cx;
        cx.update(|window, cx| _ = window.draw(cx));

        cx.simulate_mouse_down(
            point(px(10.), px(20.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        for y in [30., 60., 100.] {
            cx.simulate_mouse_move(
                point(px(80.), px(y)),
                Some(MouseButton::Left),
                Modifiers::default(),
            );
            cx.update(|window, cx| _ = window.draw(cx));
        }
        cx.simulate_mouse_up(
            point(px(80.), px(100.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.update(|window, cx| _ = window.draw(cx));
        // The pointer, free again, crosses the text.
        cx.simulate_mouse_move(point(px(5.), px(60.)), None, Modifiers::default());
        cx.simulate_mouse_move(point(px(90.), px(60.)), None, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        let markdown = view.read_with(cx, |root, _| root.markdown.clone());
        assert_eq!(selected_in(&markdown, cx), "");
    }

    /// Text above a plain strip and a control that takes the focus.
    struct FocusRoot {
        markdown: Entity<MarkdownState>,
        control: gpui::FocusHandle,
    }

    impl Render for FocusRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(
                    div()
                        .h(px(40.))
                        .child(MarkdownView::new(&self.markdown).selectable(true)),
                )
                .child(div().id("plain-strip").w_full().h(px(40.)))
                .child(
                    div()
                        .id("focusable-control")
                        .w_full()
                        .h(px(40.))
                        .track_focus(&self.control),
                )
        }
    }

    #[gpui::test]
    fn the_selection_goes_when_the_focus_leaves_the_text(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| FocusRoot {
            markdown: cx.new(|cx| MarkdownState::new("Hello world", cx)),
            control: cx.focus_handle(),
        });
        let cx: &mut VisualTestContext = cx;
        // Focus moves are reported only in the window that has the focus.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
        let markdown = view.read_with(cx, |root, _| root.markdown.clone());

        cx.simulate_mouse_down(
            point(px(1.), px(10.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(px(300.), px(10.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_up(
            point(px(300.), px(10.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.update(|window, cx| _ = window.draw(cx));
        assert_eq!(selected_in(&markdown, cx), "Hello world");

        // A press on something that takes no focus leaves it.
        cx.simulate_click(point(px(10.), px(60.)), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        assert_eq!(selected_in(&markdown, cx), "Hello world");

        // One on a control that takes the focus puts it away.
        cx.simulate_click(point(px(10.), px(100.)), Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
        assert_eq!(selected_in(&markdown, cx), "");
    }

    #[gpui::test]
    fn cross_view_drag_copies_in_document_order(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (_, cx) = cx.add_window_view(|_, cx| CrossViewRoot::new(cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let start = point(px(1.), px(15.));
        let end = point(px(300.), px(70.));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let selected = copied(cx);
        assert_eq!(selected, "Hello world\nSecond message");
    }

    #[gpui::test]
    fn drag_across_mixed_blocks_copies_every_block_kind(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        for (source, expected) in [
            ("Alpha beta\n\nGamma delta", "Alpha beta\nGamma delta"),
            (
                "# Title\n\nIntro para\n\n- item one\n- item two\n\n```\ncode line\n```\n\nTail para",
                "Title\nIntro para\nitem one\nitem two\ncode line\nTail para",
            ),
        ] {
            let (view, cx) = cx.add_window_view(|_, cx| MultiBlockRoot::with_text(source, cx));
            let cx: &mut VisualTestContext = cx;
            cx.run_until_parked();
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });

            let (start, end) = view.read_with(cx, |root, cx| {
                let state = root.markdown.read(cx);
                let first = state.list_state.bounds_for_item(0).unwrap();
                let count = state.list_state.item_count();
                let last = state.list_state.bounds_for_item(count - 1).unwrap();
                (
                    point(px(1.), first.center().y),
                    point(px(300.), last.center().y),
                )
            });
            cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });

            let selected = copied(cx);
            assert_eq!(selected, expected, "source: {source:?}");
        }
    }

    #[gpui::test]
    fn drag_across_split_code_and_list_copies_the_lines_between(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let code = (0..40)
            .map(|ix| format!("line {ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        let list = (0..12)
            .map(|ix| format!("- item {ix}"))
            .collect::<Vec<_>>()
            .join("\n");
        let source = format!("```text\n{code}\n```\n\n{list}");
        let (_, cx) = cx.add_window_view(|_, cx| MultiBlockRoot::with_text(&source, cx));
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(gpui::size(px(400.), px(1_400.)));
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        }

        let start = cx.debug_bounds("markdown-code-line-30").unwrap();
        let end = cx.debug_bounds("markdown-list-item-root-1-10").unwrap();
        let (start, end) = (
            point(start.left() + px(1.), start.center().y),
            point(px(390.), end.center().y),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));

        let expected = (30..40)
            .map(|ix| format!("line {ix}"))
            .chain((0..=10).map(|ix| format!("item {ix}")))
            .collect::<Vec<_>>()
            .join("\n");
        let selected = copied(cx);
        assert_eq!(selected, expected);
    }

    #[gpui::test]
    fn link_click_survives_pixel_jitter_but_not_a_drag(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| RightClickRoot::new(cx));
        let cx: &mut VisualTestContext = cx;
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let link_pos = view.read_with(cx, |root, cx| {
            let link = root
                .markdown
                .read(cx)
                .list_state
                .bounds_for_item(1)
                .unwrap();
            point(px(5.), link.center().y)
        });

        // A drag that starts on the link is a selection, not a click.
        let far = point(link_pos.x + px(60.), link_pos.y);
        cx.simulate_mouse_down(link_pos, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(far, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(far, MouseButton::Left, Modifiers::default());
        assert_eq!(cx.opened_url(), None, "a drag must not open the link");

        // A press-and-release with a pixel of jitter is still a click.
        let jittered = point(link_pos.x + px(1.), link_pos.y + px(1.));
        cx.simulate_mouse_down(link_pos, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(jittered, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(jittered, MouseButton::Left, Modifiers::default());
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
    }

    /// The view under the window `Root`, which owns Copy for the window
    /// selection, as the shell mounts it.
    fn right_click_window(
        cx: &mut TestAppContext,
    ) -> (Entity<RightClickRoot>, &mut VisualTestContext) {
        let view = Rc::new(std::cell::OnceCell::new());
        let (_, cx) = cx.add_window_view({
            let view = view.clone();
            move |window, cx| {
                let root = cx.new(RightClickRoot::new);
                view.set(root.clone()).ok().unwrap();
                gpui_base::Root::new(root, window, cx)
            }
        });
        (view.get().unwrap().clone(), cx)
    }

    #[gpui::test]
    fn link_context_menu_action_opens_the_url(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        cx.update(crate::widgets::menu::init);
        let (view, cx) = right_click_window(cx);
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let link_pos = view.read_with(cx, |root, cx| {
            let link = root
                .markdown
                .read(cx)
                .list_state
                .bounds_for_item(1)
                .unwrap();
            point(px(5.), link.center().y)
        });
        cx.simulate_mouse_down(link_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(link_pos, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        // Copy is disabled with nothing selected, so the first Down lands on
        // Select All and the second on "Open link"; the action must dispatch
        // through the trigger's ancestor chain to the OpenUrl handler.
        cx.simulate_keystrokes("down down enter");
        cx.run_until_parked();
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
    }

    #[gpui::test]
    fn right_click_on_text_opens_the_selection_menu_and_is_swallowed(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        cx.update(crate::widgets::menu::init);
        let (view, cx) = right_click_window(cx);
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        let (text_pos, link_pos) = view.read_with(cx, |root, cx| {
            let state = root.markdown.read(cx);
            let text = state.list_state.bounds_for_item(0).unwrap();
            let link = state.list_state.bounds_for_item(1).unwrap();
            (
                point(px(5.), text.center().y),
                point(px(5.), link.center().y),
            )
        });

        // Plain (and selectable) text: the view's own menu (Copy, Select
        // All) opens and takes focus, and the press is consumed before the
        // root handler sees it.
        cx.simulate_mouse_down(text_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(text_pos, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let bubbled = view.read_with(cx, |root, _| *root.bubbled_right_clicks.borrow());
        assert_eq!(bubbled, 0, "right-click on plain text must not propagate");
        let markdown_focus =
            view.read_with(cx, |root, cx| root.markdown.read(cx).focus_handle.clone());
        let focused = cx.update(|window, cx| window.focused(cx));
        assert!(
            focused.is_some_and(|focused| focused != markdown_focus),
            "a plain-text right-click must open and focus the context menu"
        );

        // Select All from that menu selects the whole view, and Copy, which
        // was disabled with nothing selected, then copies it.
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let markdown = view.read_with(cx, |root, _| root.markdown.clone());
        assert_eq!(selected_in(&markdown, cx), "Alpha beta\nclick");
        cx.simulate_mouse_down(text_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(text_pos, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("Alpha beta\nclick")
        );

        // A link's menu adds the link items after the view's own; the press
        // is consumed the same way and the menu takes focus.
        cx.simulate_mouse_down(link_pos, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(link_pos, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let bubbled = view.read_with(cx, |root, _| *root.bubbled_right_clicks.borrow());
        assert_eq!(bubbled, 0, "right-click on a link must not propagate");
        let focused = cx.update(|window, cx| window.focused(cx));
        assert!(
            focused.is_some_and(|focused| focused != markdown_focus),
            "a link right-click must open and focus the context menu"
        );
    }

    #[gpui::test]
    fn copy_from_the_menu_of_a_right_click_on_the_selection_copies_it(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        cx.update(crate::widgets::menu::init);
        let (view, cx) = right_click_window(cx);
        view.update(cx, |root, cx| {
            root.markdown.update(cx, |md, cx| {
                md.set_text("```text\nfirst line\nsecond\n```", cx)
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
        let line = cx.debug_bounds("markdown-code-line-0").unwrap();
        let (start, end) = (
            point(line.left() + px(1.), line.center().y),
            point(line.right() - px(1.), line.center().y),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        assert_eq!(copied(cx), "first line");

        let inside = point(line.left() + px(20.), line.center().y);
        cx.simulate_mouse_down(inside, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(inside, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
        let copy = cx.debug_bounds("menu-item-0").expect("menu opens").center();
        cx.simulate_click(copy, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("first line")
        );
    }

    #[gpui::test]
    fn inline_formula_menu_copies_latex_and_switches_to_source(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        cx.update(crate::widgets::menu::init);
        let (view, cx) = right_click_window(cx);
        view.update(cx, |root, cx| {
            root.markdown
                .update(cx, |md, cx| md.set_text("before $x^2$ after", cx))
        });
        cx.run_until_parked();
        cx.update(|window, cx| _ = window.draw(cx));
        let formula = cx.debug_bounds("math-glyph-0").unwrap().center();
        let menu = |keys: &str, cx: &mut VisualTestContext| {
            cx.simulate_mouse_down(formula, MouseButton::Right, Modifiers::default());
            cx.simulate_mouse_up(formula, MouseButton::Right, Modifiers::default());
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
            // Copy is disabled with nothing selected; the first Down lands
            // on Select All, the next on the formula's own items.
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        };

        menu("down down enter", cx);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("x^2")
        );
        menu("down down down enter", cx);
        assert!(cx.debug_bounds("math-glyph-0").is_none());
        menu("down down down enter", cx);
        assert!(cx.debug_bounds("math-glyph-0").is_some());
    }

    struct TouchRoot {
        markdown: Entity<MarkdownState>,
    }

    impl Render for TouchRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(300.))
                .child(MarkdownView::new(&self.markdown).selectable(true))
        }
    }

    fn open_touch_root(cx: &mut TestAppContext) -> (Entity<TouchRoot>, &mut VisualTestContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (view, cx) = cx.add_window_view(|_, cx| TouchRoot {
            markdown: cx.new(|cx| MarkdownState::new("quick select value", cx)),
        });
        cx.run_until_parked();
        draw(cx);
        (view, cx)
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    fn long_press(cx: &mut VisualTestContext, phases: &[gpui::TouchPhase], at: (f32, f32)) {
        let start_position = point(px(at.0), px(at.1));
        for phase in phases {
            cx.simulate_event(gpui::LongPressEvent {
                phase: *phase,
                start_position,
                position: start_position,
            });
            draw(cx);
        }
    }

    fn selected(view: &Entity<TouchRoot>, cx: &mut VisualTestContext) -> String {
        let markdown = view.read_with(cx, |root, _| root.markdown.clone());
        selected_in(&markdown, cx).trim().to_string()
    }

    fn touch_selection(
        view: &Entity<TouchRoot>,
        cx: &mut VisualTestContext,
    ) -> Option<crate::touch_selection::TouchSelectionLayout> {
        view.read_with(cx, |root, cx| {
            root.markdown
                .read(cx)
                .selection
                .as_ref()
                .and_then(|membership| membership.document.read(cx).touch_layout())
        })
    }

    /// Drag the knob at `edge` to `to` with a finger.
    fn drag_knob(
        cx: &mut VisualTestContext,
        edge: gpui_base::SelectionEdge,
        caret: gpui::Bounds<gpui::Pixels>,
        to: gpui::Point<gpui::Pixels>,
    ) {
        use gpui::{TouchDragEvent, TouchPhase};
        let finger = gpui_base::TouchHandle::hit_bounds(edge, caret).center();
        for (phase, position) in [
            (TouchPhase::Started, finger),
            (TouchPhase::Moved, point(to.x, finger.y)),
            (TouchPhase::Ended, point(to.x, finger.y)),
        ] {
            cx.simulate_event(TouchDragEvent {
                phase,
                start_position: finger,
                position,
            });
            draw(cx);
        }
    }

    #[gpui::test]
    fn long_press_release_keeps_handles_which_drag_the_selection(cx: &mut TestAppContext) {
        use gpui::{TouchDragEvent, TouchPhase};
        use gpui_base::{SelectionEdge, TouchHandle};

        for continue_finger in [true, false] {
            let (view, cx) = open_touch_root(cx);
            if continue_finger {
                let start_position = point(px(10.), px(10.));
                cx.simulate_event(gpui::LongPressEvent {
                    phase: TouchPhase::Started,
                    start_position,
                    position: start_position,
                });
                draw(cx);
                assert_eq!(selected(&view, cx), "quick");
                for phase in [TouchPhase::Moved, TouchPhase::Ended] {
                    cx.simulate_event(gpui::LongPressEvent {
                        phase,
                        start_position,
                        position: point(px(220.), px(10.)),
                    });
                }
                draw(cx);
                assert_eq!(selected(&view, cx), "quick select value");
                continue;
            }
            long_press(cx, &[TouchPhase::Started, TouchPhase::Ended], (70., 10.));
            assert_eq!(selected(&view, cx), "select");
            let layout =
                touch_selection(&view, cx).expect("a released long press keeps its handles");
            assert!(layout.menu_open);
            assert!(!layout.is_empty());
            assert!(layout.start.left() < layout.end.left());

            // A touch on the end knob, which hangs below the line, takes it
            // and drags the end along the line to the end of the text.
            let finger = TouchHandle::hit_bounds(SelectionEdge::End, layout.end).center();
            cx.simulate_event(TouchDragEvent {
                phase: TouchPhase::Started,
                start_position: finger,
                position: finger,
            });
            draw(cx);
            let layout = touch_selection(&view, cx).unwrap();
            assert_eq!(layout.dragging, Some(SelectionEdge::End));
            assert!(!layout.menu_open);
            cx.simulate_event(TouchDragEvent {
                phase: TouchPhase::Moved,
                start_position: finger,
                position: point(px(290.), finger.y),
            });
            draw(cx);
            assert_eq!(selected(&view, cx), "select value");
            cx.simulate_event(TouchDragEvent {
                phase: TouchPhase::Ended,
                start_position: finger,
                position: point(px(290.), finger.y),
            });
            draw(cx);
            let layout = touch_selection(&view, cx).unwrap();
            assert!(layout.menu_open);
            assert_eq!(layout.dragging, None);
            let select_start = layout.start.left();

            // Select All from the menu takes the whole message; its handles
            // still drag.
            let select_all = cx.debug_bounds("edit-menu-select-all").unwrap();
            cx.simulate_click(select_all.center(), Modifiers::default());
            cx.run_until_parked();
            draw(cx);
            assert_eq!(selected(&view, cx), "quick select value");
            let layout = touch_selection(&view, cx).expect("select all keeps the touch selection");
            assert!(layout.menu_open);
            assert!(layout.start.left() < select_start);
            drag_knob(
                cx,
                SelectionEdge::Start,
                layout.start,
                point(select_start + px(1.), layout.start.center().y),
            );
            assert_eq!(selected(&view, cx), "select value");

            // A press on the menu leaves the selection alone; a tap on the
            // text between the knobs puts it away.
            let menu = cx.debug_bounds("edit-menu").expect("the menu is open");
            cx.simulate_mouse_down(menu.center(), MouseButton::Left, Modifiers::default());
            assert_eq!(selected(&view, cx), "select value");
            cx.simulate_mouse_up(menu.center(), MouseButton::Left, Modifiers::default());
            let layout = touch_selection(&view, cx).unwrap();
            let between = (layout.start.left() + layout.end.left()) / 2.;
            cx.simulate_click(point(between, px(10.)), Modifiers::default());
            draw(cx);
            assert!(touch_selection(&view, cx).is_none());
            assert_eq!(selected(&view, cx), "");
        }
    }
}
