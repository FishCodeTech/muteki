//! Synchronous Markdown state, structurally adapted from gpui-component's
//! Apache-2.0 `text/state.rs` implementation.

use std::{
    cell::RefCell,
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::widgets::copy::CopiedMark;
use gpui::{
    Bounds, ClipboardItem, Context, FocusHandle, ImageSource, IntoElement, ListAlignment,
    ListState, ParentElement as _, Pixels, Point, Render, SharedString, SharedUri, Size,
    Styled as _, Window, px,
};
use gpui_base::{ElementExt as _, v_flex};

use super::{
    link_target::{LinkTarget, LinkTargetCache},
    nodes::{BlockNode, TextPosition},
    render::{self, RootItem},
    selection::{Membership, RunOrigin, SelectionDocument},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingLinkMenu {
    pub(super) target: LinkTarget,
    pub(super) text: SharedString,
    pub(super) raw_url: SharedString,
}

/// What the last right-click landed on, recorded by the inline that was hit
/// and read by the view's context menu, which opens after the press settles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PendingContextTarget {
    Link(PendingLinkMenu),
    CodeBlock(String),
    Image {
        url: SharedUri,
        title: String,
    },
    Math {
        path: String,
        latex: String,
        /// `None` when the formula does not lay out, so only its source exists.
        show_source: Option<bool>,
    },
}

pub type ImageSourceFn = Rc<dyn Fn(&str) -> Option<ImageSource>>;
pub type ImageStandInFn =
    Rc<dyn Fn(&str, &str, &mut gpui::Window, &mut gpui::App) -> Option<gpui::AnyElement>>;

/// Images a view reads through the host rather than by URL. The chat supplies none.
#[derive(Clone)]
pub struct ImageResolver {
    /// The source to draw a URL from, for a URL this resolver owns.
    pub source: ImageSourceFn,
    /// What stands in for an owned URL until it can be drawn: a loading box, or why it cannot.
    pub pending: ImageStandInFn,
}

/// State backing a [`super::MarkdownView`].
pub struct MarkdownState {
    pub(super) focus_handle: FocusHandle,
    pub(super) bounds: Bounds<Pixels>,
    pub(super) compact_headings: bool,
    pub(super) base_dir: Option<PathBuf>,
    pub(super) image_resolver: Option<ImageResolver>,
    link_targets: LinkTargetCache,
    pub(super) pending_context: Option<PendingContextTarget>,
    /// Keyed by the copied code block's root path.
    pub(super) copied: CopiedMark,
    pub(super) math_sources: std::collections::HashSet<String>,
    /// Window position of the last left mouse-down that landed on a link;
    /// a mouse-up nearby is a click, anything farther is a drag-selection.
    pub(super) link_press_origin: Option<Point<Pixels>>,
    text: String,
    parsed: Rc<BlockNode>,
    items: Rc<[RootItem]>,
    root_block_starts: Option<Vec<usize>>,
    has_potential_link_reference_definition: bool,
    pub(super) list_state: ListState,
    item_heights: Rc<RefCell<render::ItemHeights>>,
    image_sizes: HashMap<String, Size<Pixels>>,
    layout_revision: usize,
    /// The document this view's text is selected in, when it is selectable.
    pub(super) selection: Option<Membership>,
    /// Whether `selection` is the view's own document rather than one an
    /// owner placed it in.
    owns_selection: bool,
    /// Clears the view's own selection when the focus leaves the view.
    own_selection_focus_out: Option<gpui::Subscription>,
    /// The runs of text painted last frame, which selection ends hit-test.
    pub(super) runs: Rc<RefCell<Vec<PaintedRun>>>,
    #[cfg(test)]
    last_reparse_bytes: usize,
}

impl MarkdownState {
    /// Parse `text` immediately and create a Markdown state entity value.
    pub fn new(text: &str, cx: &mut Context<Self>) -> Self {
        let parsed_document = super::parse::parse_document(text);
        Self::from_parsed(text, parsed_document, cx)
    }

    /// Create a Markdown state from a document parsed on another executor.
    pub(crate) fn from_parsed(
        text: &str,
        parsed_document: super::parse::ParsedDocument,
        cx: &mut Context<Self>,
    ) -> Self {
        let owner = cx.entity_id();
        cx.on_release(move |_, cx| render::clear_code_measurements(owner, cx))
            .detach();
        let parsed = parsed_document.root;
        let items = root_items(&parsed);
        Self {
            focus_handle: cx.focus_handle(),
            bounds: Bounds::default(),
            compact_headings: false,
            base_dir: None,
            image_resolver: None,
            link_targets: LinkTargetCache::default(),
            pending_context: None,
            copied: CopiedMark::default(),
            math_sources: Default::default(),
            link_press_origin: None,
            text: text.to_string(),
            parsed: Rc::new(parsed),
            root_block_starts: parsed_document.root_starts,
            has_potential_link_reference_definition: contains_potential_link_reference_definition(
                text,
            ),
            // Measure every item once so the list has a stable total height,
            // then construct/layout/paint only the visible items on warm frames.
            list_state: ListState::new(items.len(), ListAlignment::Top, px(1000.)).measure_all(),
            item_heights: Rc::new(RefCell::new(render::ItemHeights::new(items.len()))),
            items,
            image_sizes: HashMap::new(),
            layout_revision: 0,
            selection: None,
            owns_selection: false,
            own_selection_focus_out: None,
            runs: Rc::default(),
            #[cfg(test)]
            last_reparse_bytes: text.len(),
        }
    }

    /// Append text, synchronously reparse the one canonical source, and repaint.
    pub fn push_str(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        self.has_potential_link_reference_definition |=
            contains_potential_link_reference_definition(text)
                || self.text.ends_with(']') && text.starts_with(':');
        self.text.push_str(text);
        self.reparse_append(cx);
    }

    /// Replace the canonical source and synchronously reparse it.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.text == text {
            return;
        }
        self.math_sources.clear();
        self.text.clear();
        self.text.push_str(text);
        self.has_potential_link_reference_definition =
            contains_potential_link_reference_definition(text);
        self.reparse_reset(cx);
    }

    /// Return all rendered text, including blocks that were never painted.
    pub fn rendered_text(&self) -> String {
        with_trailing_newline(self.parsed.text())
    }

    /// Enable or disable selection of this view's text in a document of
    /// its own.
    pub fn set_selectable(&mut self, selectable: bool, cx: &mut Context<Self>) {
        // A view an owner placed in its document stays there.
        if selectable == self.selection.is_some() {
            return;
        }
        self.selection = selectable.then(|| Membership {
            document: super::selection::own_document(cx.entity().downgrade(), cx),
            entry: 0,
        });
        self.owns_selection = selectable;
        self.own_selection_focus_out = None;
        cx.notify();
    }

    /// Clear the view's own selection when the focus leaves the view, as a
    /// transcript clears its selection when the focus leaves it.
    pub(super) fn clear_own_selection_on_focus_out(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        if self.own_selection_focus_out.is_some() {
            return;
        }
        let Some(document) = self.own_document() else {
            return;
        };
        self.own_selection_focus_out = Some(window.on_focus_out(
            &self.focus_handle,
            cx,
            move |_, _, cx| document.update(cx, |document, cx| document.clear(cx)),
        ));
    }

    /// Make this view's text entry `entry` of `document`'s selection.
    pub fn set_selection_entry(
        &mut self,
        document: &gpui::Entity<SelectionDocument>,
        entry: u64,
        cx: &mut Context<Self>,
    ) {
        if self
            .selection
            .as_ref()
            .is_some_and(|m| m.document == *document && m.entry == entry)
        {
            return;
        }
        self.selection = Some(Membership {
            document: document.clone(),
            entry,
        });
        self.owns_selection = false;
        self.own_selection_focus_out = None;
        cx.notify();
    }

    /// The selection document this view wraps itself in, when it selects on
    /// its own.
    pub(super) fn own_document(&self) -> Option<gpui::Entity<SelectionDocument>> {
        self.owns_selection
            .then(|| self.selection.as_ref().map(|m| m.document.clone()))
            .flatten()
    }

    pub(crate) fn parsed_root(&self) -> Rc<BlockNode> {
        self.parsed.clone()
    }

    /// Scale headings for a chat message instead of a document. Document scale
    /// (h1 at 2× body) dwarfs a timeline; chat scale keeps them in the flow.
    pub fn set_compact_headings(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact_headings == compact {
            return;
        }
        self.compact_headings = compact;
        cx.notify();
    }

    /// Set the directory used to resolve relative Markdown links.
    pub fn set_base_dir(&mut self, base_dir: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.base_dir == base_dir {
            return;
        }
        self.base_dir = base_dir;
        self.link_targets.clear();
        cx.notify();
    }

    pub fn set_image_resolver(&mut self, resolver: Option<ImageResolver>, cx: &mut Context<Self>) {
        self.image_resolver = resolver;
        cx.notify();
    }

    pub(super) fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub(super) fn resolve_link(&self, url: &str) -> LinkTarget {
        self.link_targets.resolve(url, self.base_dir())
    }

    /// The image drawn in the text. A host file is drawn from its largest preview, whose key
    /// does not depend on the width it lays out at.
    pub(super) fn image_source(&self, uri: &SharedUri) -> ImageSource {
        self.resolve_image(uri, |path| {
            crate::store::image_preview(path, crate::store::MAX_PREVIEW_PIXELS as f32, false)
        })
    }

    /// The image the viewer opens, which reads a host file's original.
    pub(super) fn opened_image_source(&self, uri: &SharedUri) -> crate::image_viewer::ViewerSource {
        self.resolve_image(uri, |path| {
            crate::image_viewer::ViewerSource::original(
                tcode_protocol::OriginalTarget::File { path },
                self.image_source(uri),
            )
        })
    }

    /// Markdown file images belong to the attached host, including relative paths.
    /// Do not check the client's filesystem before asking that host for the bytes.
    fn resolve_image<T: From<ImageSource>>(
        &self,
        uri: &SharedUri,
        host_file: impl FnOnce(PathBuf) -> T,
    ) -> T {
        if let Some(source) = self
            .image_resolver
            .as_ref()
            .and_then(|resolver| (resolver.source)(uri.as_ref()))
        {
            return source.into();
        }
        let path = PathBuf::from(uri.as_ref());
        if path.is_absolute() {
            return host_file(path);
        }
        if let Ok(url) = url::Url::parse(uri.as_ref()) {
            if url.scheme() == "file"
                && let Ok(path) = percent_encoding::percent_decode_str(url.path()).decode_utf8()
            {
                // Decode the host's path without the viewing platform's filesystem rules.
                let path = match url.host_str() {
                    Some(host) => format!("//{host}{path}"),
                    None => path.into_owned(),
                };
                let path = path
                    .strip_prefix('/')
                    .filter(|path| path.as_bytes().get(1) == Some(&b':'))
                    .unwrap_or(&path);
                return host_file(PathBuf::from(path));
            }
            return ImageSource::from(uri.clone()).into();
        }
        let path = match self.base_dir() {
            Some(base_dir) => base_dir.join(path),
            None => path,
        };
        host_file(path)
    }

    pub(super) fn image_layout(&self, path: &str) -> (usize, Option<Size<Pixels>>) {
        (self.layout_revision, self.image_sizes.get(path).copied())
    }

    pub(super) fn image_resized(
        &mut self,
        path: String,
        revision: usize,
        size: Size<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if revision != self.layout_revision {
            return;
        }
        let items = self.items_of(&path);
        // The first size seen is the one the image's item was measured with.
        if self
            .image_sizes
            .insert(path, size)
            .is_none_or(|old| old == size)
        {
            return;
        }
        self.remeasure(items, cx);
    }

    pub(super) fn set_math_source(&mut self, path: &str, source: bool, cx: &mut Context<Self>) {
        self.prepare_reparse();
        // The formula's text becomes its source or its glyphs, so a
        // selection's offsets in it would land elsewhere.
        if let Some(membership) = &self.selection {
            membership
                .document
                .update(cx, |document, cx| document.clear(cx));
        }
        self.layout_revision = self.layout_revision.wrapping_add(1);
        if source {
            self.math_sources.insert(path.into());
        } else {
            self.math_sources.remove(path);
        }
        let items = self.items_of(path);
        self.remeasure(items, cx);
    }

    /// The root items that draw what lies at render `path`: every item when
    /// the path is not in a root block.
    fn items_of(&self, path: &str) -> Range<usize> {
        let Some(block) = render::root_block_of(path) else {
            return 0..self.items.len();
        };
        self.items.partition_point(|item| item.block < block)
            ..self.items.partition_point(|item| item.block <= block)
    }

    /// Measure `items` again; the blocks after them move.
    fn remeasure(&mut self, items: Range<usize>, cx: &mut Context<Self>) {
        self.item_heights.borrow_mut().invalidate(items.clone());
        self.list_state.remeasure_items(items);
        cx.notify();
    }

    pub(super) fn copy_code(&mut self, path: String, code: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(code));
        self.copied.mark(path, |state| &mut state.copied, cx);
    }

    pub(super) fn set_pending_context(
        &mut self,
        pending_context: Option<PendingContextTarget>,
        cx: &mut Context<Self>,
    ) {
        if self.pending_context == pending_context {
            return;
        }
        self.pending_context = pending_context;
        cx.notify();
    }

    fn prepare_reparse(&mut self) {
        self.link_targets.clear();
    }

    fn reparse_append(&mut self, cx: &mut Context<Self>) {
        self.prepare_reparse();
        let reparse_start = self.incremental_reparse_start();
        let parsed_document = super::parse::parse_document(&self.text[reparse_start..]);
        #[cfg(test)]
        {
            self.last_reparse_bytes = self.text.len() - reparse_start;
        }

        let BlockNode::Root {
            children: mut old_blocks,
        } = Rc::unwrap_or_clone(std::mem::replace(
            &mut self.parsed,
            Rc::new(BlockNode::Unknown),
        ))
        else {
            unreachable!("parsed markdown document must have a root")
        };
        let BlockNode::Root {
            children: tail_children,
        } = parsed_document.root
        else {
            unreachable!("parsed markdown tail must have a root")
        };
        let old_block_count = old_blocks.len();
        let (blocks, unchanged, old_suffix) = if reparse_start == 0 {
            // The old last block is compared even when it is unchanged: a
            // block after it changes the gap below it.
            let unchanged = old_blocks
                .iter()
                .zip(&tail_children)
                .take_while(|(old, new)| old == new)
                .count()
                .min(old_block_count.saturating_sub(1));
            self.root_block_starts = parsed_document.root_starts;
            let old_suffix = old_blocks.split_off(unchanged);
            (tail_children, unchanged, old_suffix)
        } else {
            let old_prefix_count = old_blocks.len() - 1;
            let old_suffix = old_blocks.split_off(old_prefix_count);
            let mut blocks = old_blocks;
            blocks.extend(tail_children);

            self.root_block_starts =
                match (self.root_block_starts.take(), parsed_document.root_starts) {
                    (Some(mut prefix), Some(mut tail)) => {
                        prefix.truncate(old_prefix_count);
                        tail.iter_mut().for_each(|start| *start += reparse_start);
                        prefix.extend(tail);
                        Some(prefix)
                    }
                    _ => None,
                };
            (blocks, old_prefix_count, old_suffix)
        };

        for (old, new) in old_suffix.iter().zip(&blocks[unchanged..]) {
            new.reuse_highlights(old);
        }

        // An append can only affect the old trailing block and blocks added
        // after it. Keep the measured items before it, and those of its items
        // that paint as they did.
        let first_changed = self.items.partition_point(|item| item.block < unchanged);
        let mut items = self.items[..first_changed].to_vec();
        render::push_root_items(&blocks[unchanged..], unchanged, &mut items);
        let kept = self.items[first_changed..]
            .iter()
            .zip(&items[first_changed..])
            .take_while(|(old, new)| {
                old == new
                    && render::item_renders_alike(
                        &old_suffix[old.block - unchanged],
                        old.block + 1 == old_block_count,
                        &blocks[new.block],
                        new.block + 1 == blocks.len(),
                        new,
                    )
            })
            .count();
        let invalid = first_changed + kept;
        let old_item_count = self.items.len();
        self.parsed = Rc::new(BlockNode::Root { children: blocks });
        self.items = items.into();
        self.layout_revision = self.layout_revision.wrapping_add(1);

        if invalid < old_item_count || invalid < self.items.len() {
            self.list_state
                .splice(invalid..old_item_count, self.items.len() - invalid);
            // `splice` creates unmeasured items but does not re-arm measure_all.
            self.list_state.remeasure_items(invalid..self.items.len());
            self.item_heights
                .borrow_mut()
                .splice(invalid, self.items.len());
        }
        cx.notify();
    }

    fn incremental_reparse_start(&self) -> usize {
        if self.has_potential_link_reference_definition {
            return 0;
        }
        let block_count = match &*self.parsed {
            BlockNode::Root { children } => children.len(),
            _ => 1,
        };
        self.root_block_starts
            .as_ref()
            .filter(|starts| starts.len() == block_count)
            .and_then(|starts| starts.last().copied())
            .unwrap_or(0)
    }

    fn reparse_reset(&mut self, cx: &mut Context<Self>) {
        self.image_sizes.clear();
        render::clear_code_measurements(cx.entity_id(), cx);
        self.prepare_reparse();
        let parsed_document = super::parse::parse_document(&self.text);
        self.items = root_items(&parsed_document.root);
        self.parsed = Rc::new(parsed_document.root);
        self.root_block_starts = parsed_document.root_starts;
        #[cfg(test)]
        {
            self.last_reparse_bytes = self.text.len();
        }
        self.layout_revision = self.layout_revision.wrapping_add(1);
        // Even an edit that preserves the number of root blocks can change
        // their heights, so every reparse must invalidate the cached sizes.
        self.list_state.reset(self.items.len());
        self.item_heights.borrow_mut().reset(self.items.len());
        cx.notify();
    }

    pub(super) fn is_selectable(&self) -> bool {
        self.selection.is_some()
    }

    /// The part of the run at `origin`, `len` bytes long, that the
    /// selection covers.
    pub(super) fn selected_range(
        &self,
        origin: &RunOrigin,
        len: usize,
        cx: &gpui::App,
    ) -> Option<Range<usize>> {
        let membership = self.selection.as_ref()?;
        membership
            .document
            .read(cx)
            .run_range(membership.entry, origin, len)
    }

    pub(super) fn begin_frame(&self) {
        self.runs.borrow_mut().clear();
    }

    pub(super) fn register_run(&self, run: PaintedRun) {
        self.runs.borrow_mut().push(run);
    }

    /// The character under `window_point` in the text painted last frame;
    /// none off the text.
    pub(super) fn char_at(&self, window_point: Point<Pixels>) -> Option<TextPosition> {
        let runs = self.runs.borrow();
        let run = runs.iter().find(|run| run.bounds.contains(&window_point))?;
        let offset = run.layout.index_for_position(window_point).ok()?;
        Some(TextPosition {
            path: run.origin.path.to_vec(),
            line: run.origin.line,
            offset: run.origin.base + offset,
        })
    }

    /// The position a selection end at `window_point` falls on, from the text
    /// painted last frame: on the run nearest to it, at the character
    /// boundary under it, or at the end of a run above it or the start of one
    /// below.
    pub(super) fn position_at(&self, window_point: Point<Pixels>) -> Option<TextPosition> {
        let runs = self.runs.borrow();
        let distance = |bounds: &Bounds<Pixels>| {
            let dx = (bounds.left() - window_point.x)
                .max(window_point.x - bounds.right())
                .max(px(0.));
            let dy = (bounds.top() - window_point.y)
                .max(window_point.y - bounds.bottom())
                .max(px(0.));
            dx.as_f32().hypot(dy.as_f32())
        };
        let run = runs
            .iter()
            .min_by(|a, b| distance(&a.bounds).total_cmp(&distance(&b.bounds)))?;
        let offset = if window_point.y < run.bounds.top() {
            0
        } else if window_point.y >= run.bounds.bottom() {
            run.text.len()
        } else {
            super::selection::nearest_boundary(&run.text, &run.layout, window_point)
        };
        Some(TextPosition {
            path: run.origin.path.to_vec(),
            line: run.origin.line,
            offset: run.origin.base + offset,
        })
    }

    /// Record where the document was placed; whether its size changed.
    fn update_layout(&mut self, bounds: Bounds<Pixels>) {
        self.bounds = bounds;
    }

    #[cfg(test)]
    pub(super) fn has_measured_item(&self, index: usize) -> bool {
        self.list_state.bounds_for_item(index).is_some()
    }

    #[cfg(test)]
    fn last_reparse_bytes(&self) -> usize {
        self.last_reparse_bytes
    }
}

fn with_trailing_newline(mut text: String) -> String {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn root_items(root: &BlockNode) -> Rc<[RootItem]> {
    let mut items = Vec::new();
    if let BlockNode::Root { children } = root {
        render::push_root_items(children, 0, &mut items);
    }
    items.into()
}

fn contains_potential_link_reference_definition(source: &str) -> bool {
    source.contains("]:")
}

impl Render for MarkdownState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = cx.entity();
        v_flex()
            .w_full()
            .child(render::render_root(
                self.parsed.clone(),
                self.items.clone(),
                self.list_state.clone(),
                self.item_heights.clone(),
                &state,
                window,
                cx,
            ))
            .on_prepaint(move |bounds, _, cx| {
                state.update(cx, |state, _| state.update_layout(bounds));
            })
    }
}

/// A run of text as it was painted, for hit-testing selection ends.
pub(super) struct PaintedRun {
    pub(super) origin: RunOrigin,
    pub(super) text: SharedString,
    pub(super) layout: gpui::TextLayout,
    pub(super) bounds: Bounds<Pixels>,
}

#[cfg(test)]
mod tests {
    use std::{fs, process, time::SystemTime};

    use gpui::{
        AppContext as _, Context, Entity, IntoElement, Render, TestAppContext, VisualTestContext,
        Window, div, px,
    };

    use super::*;

    struct SelectAllRoot {
        markdown: Entity<MarkdownState>,
    }

    impl SelectAllRoot {
        fn new(text: &str, cx: &mut Context<Self>) -> Self {
            let text = text.to_string();
            Self {
                markdown: cx.new(|cx| MarkdownState::new(&text, cx)),
            }
        }
    }

    impl Render for SelectAllRoot {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(400.))
                .child(super::super::MarkdownView::new(&self.markdown).selectable(true))
        }
    }

    #[gpui::test]
    fn streamed_appends_match_full_parse(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let documents = [
            "plain paragraph\ncontinued line\n\n# atx heading\n\nSetext heading\n===\n",
            "- outer\n  - nested one\n  - nested two\n\n1. ordered\n   1. nested ordered\n\n> quoted\n>\n> - with a list\n",
            "````text\na literal ``` inside the longer fence\n````\n\n```rust\nfn main() {}\n```\n",
            "before\n\n```rust\nlet unfinished = true;\n",
            "| left | center | right |\n| :--- | :---: | ---: |\n| a | b | c |\n",
            "An earlier [reference] is resolved later.\n\n[reference]: https://example.com \"title\"\n",
            "An earlier [reference].\n\n[reference]: https://example.com\n\nTail grows",
        ];

        for document in documents {
            let expected = cx.update(|cx| cx.new(|cx| MarkdownState::new(document, cx)));
            let expected_tree = expected.read_with(cx, |state, _| state.parsed.clone());
            for chunk_size in [1, 7, 64] {
                let streamed = cx.update(|cx| cx.new(|cx| MarkdownState::new("", cx)));
                for chunk in document.as_bytes().chunks(chunk_size) {
                    let chunk = std::str::from_utf8(chunk).expect("test corpus is ASCII");
                    streamed.update(cx, |state, cx| state.push_str(chunk, cx));
                }
                streamed.read_with(cx, |state, _| {
                    assert_eq!(
                        state.parsed, expected_tree,
                        "{document:?}, chunk size {chunk_size}"
                    );
                    if document.contains("[reference]") {
                        let BlockNode::Root { children } = state.parsed.as_ref() else {
                            panic!("root")
                        };
                        let BlockNode::Paragraph(paragraph) = &children[0] else {
                            panic!("paragraph")
                        };
                        let urls = paragraph
                            .children
                            .iter()
                            .flat_map(|inline| &inline.marks)
                            .filter_map(|(_, mark)| {
                                mark.link.as_ref().map(|link| link.url.as_ref())
                            })
                            .collect::<Vec<_>>();
                        assert_eq!(urls, ["https://example.com"], "chunk size {chunk_size}");
                    }
                });
            }
        }
    }

    #[gpui::test]
    fn append_reparses_only_the_last_root_block(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let document = (0..120)
            .map(|ix| format!("## Closed block {ix}\n\nParagraph {ix}.\n\n"))
            .collect::<String>();
        assert!(document.len() > 2_000);
        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new(&document, cx)));

        state.update(cx, |state, cx| state.push_str("new tail", cx));
        state.read_with(cx, |state, _| {
            assert!(
                state.last_reparse_bytes() < 64,
                "parsed {} of {} bytes",
                state.last_reparse_bytes(),
                state.text.len()
            );
        });
    }

    #[gpui::test]
    fn link_target_cache_tracks_document_content_and_base_dir(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock should be after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tcode-markdown-state-link-cache-{}-{nonce}",
            process::id()
        ));
        let first_base = root.join("first");
        let second_base = root.join("second");
        fs::create_dir_all(&first_base).expect("create first base directory");
        fs::create_dir_all(&second_base).expect("create second base directory");
        let path = first_base.join("linked.md");
        fs::write(&path, b"linked").expect("create linked file");

        let state = cx.update(|cx| cx.new(|cx| MarkdownState::new("[link](linked.md)", cx)));
        state.update(cx, |state, cx| {
            state.set_base_dir(Some(first_base.clone()), cx);
            assert_eq!(
                state.resolve_link("linked.md"),
                LinkTarget::Local(path.clone())
            );
        });

        fs::remove_file(&path).expect("remove linked file");
        state.update(cx, |state, cx| {
            assert_eq!(
                state.resolve_link("linked.md"),
                LinkTarget::Local(path.clone())
            );
            state.set_text("changed [link](linked.md)", cx);
            assert_eq!(
                state.resolve_link("linked.md"),
                LinkTarget::Web("linked.md".to_string())
            );
        });

        fs::write(&path, b"linked again").expect("recreate linked file");
        state.update(cx, |state, cx| {
            assert_eq!(
                state.resolve_link("linked.md"),
                LinkTarget::Web("linked.md".to_string())
            );
            state.set_base_dir(Some(second_base), cx);
            assert_eq!(
                state.resolve_link("linked.md"),
                LinkTarget::Web("linked.md".to_string())
            );
            state.set_base_dir(Some(first_base), cx);
            assert_eq!(state.resolve_link("linked.md"), LinkTarget::Local(path));
        });

        fs::remove_dir_all(root).expect("remove temporary directory");
    }

    /// Drag from `start` to `end` and read what the view's document holds.
    fn drag(
        view: &Entity<SelectAllRoot>,
        start: gpui::Point<gpui::Pixels>,
        end: gpui::Point<gpui::Pixels>,
        cx: &mut VisualTestContext,
    ) -> String {
        use gpui::{Modifiers, MouseButton};
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| _ = window.draw(cx));
        view.read_with(cx, |root, cx| {
            let state = root.markdown.read(cx);
            let document = &state.selection.as_ref().unwrap().document;
            document.read(cx).selected_text(cx)
        })
    }

    fn open<'a>(
        text: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<SelectAllRoot>, &'a mut VisualTestContext) {
        cx.update(crate::theme::init);
        cx.update(super::super::init);
        let text = text.to_string();
        let (view, cx) = cx.add_window_view(|_, cx| SelectAllRoot::new(&text, cx));
        cx.update(|window, cx| _ = window.draw(cx));
        (view, cx)
    }

    #[gpui::test]
    fn a_drag_ending_off_the_code_lines_ends_at_the_nearest_line(cx: &mut TestAppContext) {
        use gpui::point;

        let (view, cx) = open("```text\nalpha beta gamma\nsecond line\n```", cx);
        let first = cx.debug_bounds("markdown-code-line-0").unwrap();
        let last = cx.debug_bounds("markdown-code-line-1").unwrap();
        // Into the block's padding below its last line.
        let below = point(last.right() - px(4.), last.bottom() + px(4.));
        let from_start = point(first.left() + px(1.), first.center().y);
        assert_eq!(
            drag(&view, from_start, below, cx),
            "alpha beta gamma\nsecond line"
        );
        // From inside the first line, what follows the press, never what
        // precedes it.
        let from_inside = point(first.left() + px(40.), first.center().y);
        let copied = drag(&view, from_inside, below, cx);
        assert!(
            copied.ends_with("\nsecond line")
                && "alpha beta gamma".ends_with(copied.lines().next().unwrap()),
            "{copied:?}"
        );
        // And above the first line, back to where the drag began.
        let above = point(first.left() + px(4.), first.top() - px(4.));
        let from_end = point(last.right() - px(1.), last.center().y);
        assert_eq!(
            drag(&view, from_end, above, cx),
            "alpha beta gamma\nsecond line"
        );
    }

    #[gpui::test]
    fn a_drag_through_a_table_takes_each_cell_whole_before_the_next(cx: &mut TestAppContext) {
        use gpui::point;

        let (view, cx) = open(
            "| name | value |\n| --- | --- |\n\
             | a long first cell whose words wrap over several rows of the table, \
             because the window is far narrower than it | x |",
            cx,
        );
        let first = cx.debug_bounds("markdown-table-cell-content-1-0").unwrap();
        let second = cx.debug_bounds("markdown-table-cell-content-1-1").unwrap();
        // The end lies on the first cell's first row, beside its wrapped rest.
        let start = point(first.left() + px(1.), first.top() + px(4.));
        let end = point(second.right() - px(1.), second.top() + px(4.));
        assert_eq!(
            drag(&view, start, end, cx),
            "a long first cell whose words wrap over several rows of the table, because the \
             window is far narrower than it\tx"
        );
    }
}
