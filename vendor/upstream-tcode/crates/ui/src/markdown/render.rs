//! Block renderer adapted from gpui-component's Apache-2.0 `text/node.rs` and
//! `text/document.rs`, with rushdown IR and syntect highlighting.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    hash::Hash,
    ops::Range,
    rc::Rc,
    sync::Arc,
};

use crate::highlight::HighlightTheme;
use crate::scroll::ScrollableElement as _;
use crate::theme::ActiveTheme as _;
use crate::widgets::copy::copy_button;
use crate::widgets::tooltip::Tooltip;
use gpui::{
    AnyElement, App, AvailableSpace, Bounds, Display, Element, ElementId, Entity, EntityId,
    FlexDirection, FontStyle, FontWeight, Global, GlobalElementId, HighlightStyle, Image,
    ImageSource, InspectorElementId, InteractiveElement as _, IntoElement, LayoutId, ListState,
    MouseButton, ObjectFit, ParentElement as _, Pixels, Rems, Role, SharedString,
    StatefulInteractiveElement as _, Style, Styled as _, StyledImage as _, StyledText, TextLayout,
    Window, div, img, prelude::FluentBuilder as _, px, relative, rems, size,
};
use gpui_base::{h_flex, v_flex};

use crate::{diff::model::sub_runs, highlight};

use super::{
    fitted_image::FittedImage,
    inline::{Inline, InlineState},
    inline_flow::{InlineCodeStyle, InlineFlow, InlineFlowItem},
    mermaid,
    nodes::{BlockNode, CodeBlock, ColumnumnAlign, Paragraph, Table, TextMark},
    selection::RunOrigin,
    state::{MarkdownState, PendingContextTarget},
    utils::list_item_prefix,
};

const BLOCK_OVERDRAW: Pixels = px(300.);
const CODE_LINES_PER_ITEM: usize = 32;
const LIST_ITEMS_PER_ITEM: usize = 8;
const TABLE_BORDER_PX: f32 = 1.;
type HighlightRuns = Vec<(Range<usize>, HighlightStyle)>;
type SharedHighlightRuns = Arc<highlight::HighlightedSource>;
type LinkRuns = Vec<(Range<usize>, super::nodes::LinkMark)>;
type FontOverrides = Vec<(Range<usize>, SharedString)>;
type ParagraphTextStyle = (String, LinkRuns, HighlightRuns, FontOverrides);
type MarkRuns = (LinkRuns, HighlightRuns, FontOverrides);

#[derive(Clone, Hash, PartialEq, Eq)]
struct CodeCacheKey {
    code: SharedString,
    lang: SharedString,
    theme: HighlightTheme,
}

thread_local! {
    static CODE_HIGHLIGHTS: RefCell<RecentCache<CodeCacheKey, SharedHighlightRuns>> =
        RefCell::new(RecentCache::with_weight_limit(64, 32 * 1024 * 1024, |key, value| {
            key.code.len() + key.lang.len()
                + value.runs.capacity() * std::mem::size_of::<(Range<usize>, HighlightStyle)>()
        }));
}

/// The most recent results of a render step keyed by its inputs, so a repaint
/// reuses the work and a long conversation does not keep every result.
pub(super) struct RecentCache<K, V> {
    capacity: usize,
    entries: HashMap<K, V>,
    order: VecDeque<K>,
    weight: usize,
    weight_limit: usize,
    weigh: fn(&K, &V) -> usize,
}

impl<K: Clone + Eq + Hash, V: Clone> RecentCache<K, V> {
    pub(super) fn new(capacity: usize) -> Self {
        Self::with_weight_limit(capacity, capacity, |_, _| 1)
    }

    fn with_weight_limit(capacity: usize, weight_limit: usize, weigh: fn(&K, &V) -> usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
            weight: 0,
            weight_limit,
            weigh,
        }
    }

    fn remove(&mut self, key: &K) {
        if let Some(value) = self.entries.remove(key) {
            self.weight -= (self.weigh)(key, &value);
            self.order.retain(|entry| entry != key);
        }
    }

    pub(super) fn get_or_insert_with(&mut self, key: K, compute: impl FnOnce() -> V) -> V {
        if let Some(value) = self.entries.get(&key) {
            return value.clone();
        }
        let value = compute();
        let weight = (self.weigh)(&key, &value);
        // Keep the newest result even when it alone exceeds the budget: a single
        // large block's visible slices must share one highlighting pass.
        while !self.entries.is_empty()
            && (self.entries.len() >= self.capacity
                || self.weight.saturating_add(weight) > self.weight_limit)
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(value) = self.entries.remove(&oldest) {
                self.weight -= (self.weigh)(&oldest, &value);
            }
        }
        self.weight = self.weight.saturating_add(weight);
        self.order.push_back(key.clone());
        self.entries.insert(key, value.clone());
        value
    }
}

// These are input-size weights, not a claim about GPUI's allocation size.
const CODE_MEASUREMENT_WEIGHT_LIMIT: usize = 8 * 1024 * 1024;
#[derive(Default)]
struct CodeMeasurement {
    layouts: Vec<TextLayout>,
    source_bytes: usize,
    logical_lines: usize,
}
impl CodeMeasurement {
    fn weight(&self) -> usize {
        self.source_bytes
            .saturating_add(self.logical_lines.saturating_mul(128))
    }
}
struct CodeMeasurements(RecentCache<EntityId, Rc<RefCell<CodeMeasurement>>>);
impl Global for CodeMeasurements {}
impl Default for CodeMeasurements {
    fn default() -> Self {
        Self(RecentCache::with_weight_limit(
            4,
            CODE_MEASUREMENT_WEIGHT_LIMIT,
            |_, value| value.borrow().weight(),
        ))
    }
}
pub(super) fn clear_code_measurements(owner: EntityId, cx: &mut App) {
    cx.default_global::<CodeMeasurements>().0.remove(&owner);
}

#[derive(Clone)]
struct RenderOptions {
    path: String,
    in_list: bool,
    ordered: bool,
    list_start: u32,
    depth: usize,
    is_last: bool,
    measurement: Option<Rc<RefCell<CodeMeasurement>>>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            path: "root".to_string(),
            in_list: false,
            ordered: false,
            list_start: 1,
            depth: 0,
            is_last: true,
            measurement: None,
        }
    }
}

impl RenderOptions {
    fn child(&self, ix: usize, is_last: bool) -> Self {
        Self {
            path: format!("{}-{ix}", self.path),
            is_last,
            ..self.clone()
        }
    }

    fn gap(&self) -> Rems {
        if self.in_list || self.is_last {
            rems(0.)
        } else {
            rems(1.)
        }
    }
}

/// One item of the virtualized root list: a whole root block, or a run of the
/// lines of a long code block or the items of a long list, so a block taller
/// than the viewport is only built where it is visible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RootItem {
    pub(super) block: usize,
    /// The lines or list items of a split block that this item renders.
    pub(super) span: Option<Range<usize>>,
}

/// Append the root list's items for `blocks`, the root blocks from index
/// `first_block`.
pub(super) fn push_root_items(blocks: &[BlockNode], first_block: usize, items: &mut Vec<RootItem>) {
    for (offset, block) in blocks.iter().enumerate() {
        let block_ix = first_block + offset;
        let (len, per_item) = match block {
            BlockNode::CodeBlock(code)
                if mermaid::is_mermaid(code.lang.as_deref())
                    || super::math::is_math(code.lang.as_deref()) =>
            {
                (0, 1)
            }
            BlockNode::CodeBlock(code) => (code.prepared().lines.len(), CODE_LINES_PER_ITEM),
            BlockNode::List { children, .. } => (children.len(), LIST_ITEMS_PER_ITEM),
            _ => (0, 1),
        };
        if len <= per_item {
            items.push(RootItem {
                block: block_ix,
                span: None,
            });
            continue;
        }
        items.extend((0..len).step_by(per_item).map(|start| RootItem {
            block: block_ix,
            span: Some(start..(start + per_item).min(len)),
        }));
    }
}

/// Whether `item` of block `new` paints exactly as the same item of `old`
/// did, so the height measured for it still holds. `old_is_last` and
/// `new_is_last` say whether each block ends its document: the gap below a
/// block's final item depends on it.
pub(super) fn item_renders_alike(
    old: &BlockNode,
    old_is_last: bool,
    new: &BlockNode,
    new_is_last: bool,
    item: &RootItem,
) -> bool {
    let Some(span) = item.span.clone() else {
        return old == new && old_is_last == new_is_last;
    };
    let same_end = |old_len: usize, new_len: usize| {
        let (old_final, new_final) = (span.end == old_len, span.end == new_len);
        old_final == new_final && (!old_final || old_is_last == new_is_last)
    };
    match (old, new) {
        (BlockNode::CodeBlock(old), BlockNode::CodeBlock(new)) => {
            let (old_code, new_code) = (old.prepared(), new.prepared());
            old.lang == new.lang
                && same_end(old_code.lines.len(), new_code.lines.len())
                && span.end <= old_code.lines.len()
                && span.end <= new_code.lines.len()
                && span
                    .clone()
                    .all(|ix| old_code.line(ix) == new_code.line(ix))
        }
        (
            BlockNode::List {
                children: old_items,
                ordered: old_ordered,
                start: old_start,
            },
            BlockNode::List {
                children: new_items,
                ordered: new_ordered,
                start: new_start,
            },
        ) => {
            old_ordered == new_ordered
                && old_start == new_start
                && same_end(old_items.len(), new_items.len())
                && old_items.get(span.clone()) == new_items.get(span)
        }
        _ => false,
    }
}

pub(super) fn render_root(
    node: Rc<BlockNode>,
    items: Rc<[RootItem]>,
    list_state: ListState,
    heights: Rc<RefCell<ItemHeights>>,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    if !matches!(*node, BlockNode::Root { .. }) {
        return render_block(&node, RenderOptions::default(), state, window, cx);
    }
    if list_state.item_count() != items.len() {
        list_state.reset(items.len());
        heights.borrow_mut().reset(items.len());
    }

    div()
        .id("root")
        .w_full()
        .child(VirtualizedBlockList {
            root: node,
            items,
            list_state,
            heights,
            state: state.clone(),
        })
        .into_any_element()
}

/// The root block a render path such as `root-3-1/image/0` lies in.
pub(super) fn root_block_of(path: &str) -> Option<usize> {
    let rest = path.strip_prefix("root-")?;
    let end = rest
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The height of each root item at the width the root list was last laid
/// out at, which the whole document reports as its own height.
///
/// `ListState` measures only while it prepaints, keeps its sizes to itself
/// and cannot be given them, and an outer list lays the rows in its overdraw
/// out without prepainting them; so the document measures the items it has
/// no height for itself, at that width, and the inner list measures its own.
pub(super) struct ItemHeights {
    width: Option<Pixels>,
    heights: Vec<Option<Pixels>>,
}

impl ItemHeights {
    pub(super) fn new(count: usize) -> Self {
        Self {
            width: None,
            heights: vec![None; count],
        }
    }

    /// Forget the items from `start` on, and hold `count` items in all.
    pub(super) fn splice(&mut self, start: usize, count: usize) {
        self.heights.truncate(start);
        self.heights.resize(count, None);
    }

    pub(super) fn invalidate(&mut self, items: Range<usize>) {
        self.heights[items].fill(None);
    }

    pub(super) fn reset(&mut self, count: usize) {
        self.splice(0, count);
    }

    fn total(&self) -> Pixels {
        self.heights
            .iter()
            .flatten()
            .fold(px(0.), |sum, h| sum + *h)
    }
}

enum RootItemLayout {
    Measure(Rc<RefCell<CodeMeasurement>>),
    Visible(Pixels),
}

fn render_root_item(
    root: &BlockNode,
    items: &[RootItem],
    ix: usize,
    layout: RootItemLayout,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let BlockNode::Root { children } = root else {
        unreachable!("the root list renders the children of a root")
    };
    let item = &items[ix];
    let mut options = RenderOptions::default().child(item.block, item.block + 1 == children.len());
    let width = match layout {
        RootItemLayout::Measure(measurement) => {
            options.measurement = Some(measurement);
            px(0.)
        }
        RootItemLayout::Visible(width) => width,
    };
    let content = match (&children[item.block], item.span.clone()) {
        (BlockNode::CodeBlock(code), Some(span)) => {
            render_code_block(code, Some(span), &options, state, window, cx)
        }
        (
            BlockNode::List {
                children,
                ordered,
                start,
            },
            Some(span),
        ) => render_list(children, *ordered, *start, span, options, state, window, cx),
        (block, _) => render_block(block, options, state, window, cx),
    };
    let block = div()
        .w_full()
        .when(width > px(0.), |block| block.w(width))
        .child(content);
    #[cfg(test)]
    {
        block
            .debug_selector(move || format!("markdown-block-{ix}"))
            .into_any_element()
    }
    #[cfg(not(test))]
    {
        block.into_any_element()
    }
}

/// A full-height layout leaf backed by a viewport-sized GPUI list.
///
/// The outer chat list must measure the whole Markdown document as one row, but
/// the inner list must only construct and paint the slice admitted by the
/// outer row's content mask. A regular `Infer` list uses its full layout bounds
/// as its viewport, so nesting it in a virtualized row defeats block culling.
struct VirtualizedBlockList {
    root: Rc<BlockNode>,
    items: Rc<[RootItem]>,
    list_state: ListState,
    heights: Rc<RefCell<ItemHeights>>,
    state: Entity<MarkdownState>,
}

/// What a document laid out before it knew its width: a column of its
/// items, whose heights it keeps once it is placed.
struct ColumnLayout {
    _items: Vec<AnyElement>,
    item_layouts: Vec<LayoutId>,
}

impl VirtualizedBlockList {
    fn measuring<R>(
        &self,
        f: impl FnOnce(&Rc<RefCell<CodeMeasurement>>, &mut App) -> R,
        cx: &mut App,
    ) -> R {
        let measurement = Rc::new(RefCell::new(CodeMeasurement::default()));
        let result = f(&measurement, cx);
        let keep = {
            let measurement = measurement.borrow();
            !measurement.layouts.is_empty() && measurement.weight() <= CODE_MEASUREMENT_WEIGHT_LIMIT
        };
        // The shapes of the last measurement that laid code out stay alive,
        // so the next one reuses the lines it shares with it.
        if keep {
            let owner = self.state.entity_id();
            let measurements = &mut cx.default_global::<CodeMeasurements>().0;
            measurements.remove(&owner);
            measurements.get_or_insert_with(owner, || measurement);
        }
        result
    }

    /// Lay out, at `width`, the items without a height, each under a key of
    /// its own so a frame measuring another item does not take over its node.
    fn measure_missing(&self, width: Pixels, window: &mut Window, cx: &mut App) {
        let missing = {
            let heights = self.heights.borrow();
            heights
                .heights
                .iter()
                .enumerate()
                .filter_map(|(ix, height)| height.is_none().then_some(ix))
                .collect::<Vec<_>>()
        };
        if missing.is_empty() {
            return;
        }
        let measured = self.measuring(
            |measurement, cx| {
                missing
                    .into_iter()
                    .map(|ix| {
                        let item = render_root_item(
                            &self.root,
                            &self.items,
                            ix,
                            RootItemLayout::Measure(measurement.clone()),
                            &self.state,
                            window,
                            cx,
                        );
                        let mut item = div()
                            .id(("markdown-measured-item", ix))
                            .w_full()
                            .child(item)
                            .into_any_element();
                        let size = item.layout_as_root(
                            size(AvailableSpace::Definite(width), AvailableSpace::MinContent),
                            window,
                            cx,
                        );
                        (ix, size.height)
                    })
                    .collect::<Vec<_>>()
            },
            cx,
        );
        let mut heights = self.heights.borrow_mut();
        for (ix, height) in measured {
            heights.heights[ix] = Some(height);
        }
    }
}

impl IntoElement for VirtualizedBlockList {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for VirtualizedBlockList {
    type RequestLayoutState = Option<ColumnLayout>;
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        None
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
        let Some(width) = self.heights.borrow().width else {
            // Before it is first placed, the document does not know its
            // width: a column measures its items at the width its parent
            // gives it, and prepaint keeps their heights.
            let (items, item_layouts): (Vec<_>, Vec<_>) = self.measuring(
                |measurement, cx| {
                    (0..self.items.len())
                        .map(|ix| {
                            let mut item = render_root_item(
                                &self.root,
                                &self.items,
                                ix,
                                RootItemLayout::Measure(measurement.clone()),
                                &self.state,
                                window,
                                cx,
                            );
                            let layout = item.request_layout(window, cx);
                            (item, layout)
                        })
                        .unzip()
                },
                cx,
            );
            let mut style = Style {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                ..Default::default()
            };
            style.size.width = relative(1.).into();
            let layout_id = window.request_layout(style, item_layouts.iter().copied(), cx);
            return (
                layout_id,
                Some(ColumnLayout {
                    _items: items,
                    item_layouts,
                }),
            );
        };
        self.measure_missing(width, window, cx);
        let content_height = self.heights.borrow().total();
        let layout_id = window.request_measured_layout(
            Style::default(),
            move |known_dimensions, available_space, _, _| {
                let width = known_dimensions
                    .width
                    .unwrap_or(match available_space.width {
                        AvailableSpace::Definite(width) => width,
                        AvailableSpace::MinContent | AvailableSpace::MaxContent => px(0.),
                    });
                size(width, content_height)
            },
        );
        (layout_id, None)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        column: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        {
            let mut heights = self.heights.borrow_mut();
            if heights.width != Some(bounds.size.width) {
                let resized = heights.width.is_some();
                heights.width = Some(bounds.size.width);
                let count = heights.heights.len();
                heights.reset(count);
                if let Some(column) = column {
                    for (height, layout) in heights.heights.iter_mut().zip(&column.item_layouts) {
                        *height = Some(window.layout_bounds(*layout).size.height);
                    }
                } else if resized {
                    // This frame was laid out at the old width; the next one
                    // measures the items at this one.
                    drop(heights);
                    self.state.update(cx, |_, cx| cx.notify());
                }
            }
        }

        let visible = bounds.intersect(&window.content_mask().bounds);
        if visible.size.width <= px(0.) || visible.size.height <= px(0.) {
            return None;
        }

        let viewport = Bounds::from_corners(
            gpui::point(
                bounds.left(),
                (visible.top() - BLOCK_OVERDRAW).max(bounds.top()),
            ),
            gpui::point(
                bounds.right(),
                (visible.bottom() + BLOCK_OVERDRAW).min(bounds.bottom()),
            ),
        );
        let desired_offset = viewport.top() - bounds.top();
        let current_offset = -self.list_state.scroll_px_offset_for_scrollbar().y;
        self.list_state.scroll_by(desired_offset - current_offset);

        let root = self.root.clone();
        let items = self.items.clone();
        let state = self.state.clone();
        let width = viewport.size.width;
        let mut list = gpui::list(self.list_state.clone(), move |ix, window, cx| {
            render_root_item(
                &root,
                &items,
                ix,
                RootItemLayout::Visible(width),
                &state,
                window,
                cx,
            )
        })
        .size_full()
        .into_any_element();
        list.layout_as_root(
            size(
                AvailableSpace::Definite(viewport.size.width),
                AvailableSpace::Definite(viewport.size.height),
            ),
            window,
            cx,
        );
        list.prepaint_at(viewport.origin, window, cx);
        Some(list)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        list: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(list) = list {
            list.paint(window, cx);
        }
    }
}

fn render_block(
    node: &BlockNode,
    options: RenderOptions,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let gap = options.gap();
    match node {
        BlockNode::Root { children, .. } => {
            let len = children.len();
            v_flex()
                .id(options.path.clone())
                .w_full()
                .children(children.iter().enumerate().map(|(ix, child)| {
                    render_block(child, options.child(ix, ix + 1 == len), state, window, cx)
                }))
                .into_any_element()
        }
        BlockNode::Paragraph(paragraph) => div()
            .id(options.path.clone())
            .w_full()
            .pb(gap)
            .whitespace_normal()
            .child(
                pending_images(paragraph, &options.path, state, window, cx)
                    .unwrap_or_else(|| render_paragraph(paragraph, &options.path, state, cx)),
            )
            .into_any_element(),
        BlockNode::Heading {
            level, children, ..
        } => {
            let (scale, weight) = match level {
                1 => (2., FontWeight::BOLD),
                2 => (1.5, FontWeight::SEMIBOLD),
                3 => (1.25, FontWeight::SEMIBOLD),
                4 => (1.125, FontWeight::SEMIBOLD),
                5 => (1., FontWeight::SEMIBOLD),
                6 => (1., FontWeight::MEDIUM),
                _ => (1., FontWeight::NORMAL),
            };
            let typography = &cx.theme().tokens.typography;
            // In a chat message a heading is a section label, not a title page.
            let size = if state.read(cx).compact_headings {
                match level {
                    1 => typography.lg.size,
                    2 => typography.md.size,
                    _ => typography.sm.size,
                }
            } else {
                typography.md.size * scale
            };
            div()
                .id(options.path.clone())
                .pb(rems(0.3))
                .whitespace_normal()
                .text_size(size)
                .font_weight(weight)
                .child(render_paragraph(children, &options.path, state, cx))
                .into_any_element()
        }
        BlockNode::Blockquote { children, .. } => {
            let len = children.len();
            div()
                .id(options.path.clone())
                .w_full()
                .pb(gap)
                .child(
                    v_flex()
                        .w_full()
                        .text_color(cx.theme().muted_foreground)
                        .border_l_3()
                        .border_color(cx.theme().secondary_active)
                        .px_4()
                        .children(children.iter().enumerate().map(|(ix, child)| {
                            render_block(child, options.child(ix, ix + 1 == len), state, window, cx)
                        })),
                )
                .into_any_element()
        }
        BlockNode::List {
            children,
            ordered,
            start,
        } => render_list(
            children,
            *ordered,
            *start,
            0..children.len(),
            options,
            state,
            window,
            cx,
        ),
        BlockNode::ListItem { .. } => render_list_item(node, 0, options, state, window, cx),
        BlockNode::CodeBlock(code) => render_code_block(code, None, &options, state, window, cx),
        BlockNode::Table(table) => render_table(table, &options, state, window, cx),
        BlockNode::HorizontalRule => div()
            .id(options.path)
            .pb(gap)
            .child(div().h(px(2.)).w_full().bg(cx.theme().border))
            .into_any_element(),
        BlockNode::Unknown => div().into_any_element(),
    }
}

/// An image-only paragraph whose images a resolver still stands in for: each is drawn as its
/// stand-in until it can be drawn, and as the image once it can.
fn pending_images(
    paragraph: &Paragraph,
    path: &str,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let resolver = state.read(cx).image_resolver.clone()?;
    if paragraph
        .children
        .iter()
        .any(|child| !child.text.is_empty())
    {
        return None;
    }
    let images: Vec<_> = paragraph
        .children
        .iter()
        .filter_map(|child| child.image.as_ref())
        .collect();
    let pending: Vec<_> = images
        .iter()
        .map(|image| (resolver.pending)(image.url.as_ref(), &image.title(), window, cx))
        .collect();
    if pending.iter().all(Option::is_none) {
        return None;
    }
    Some(
        v_flex()
            .w_full()
            .gap_1()
            .children(
                images
                    .into_iter()
                    .zip(pending)
                    .enumerate()
                    .map(|(ix, (image, pending))| {
                        pending.unwrap_or_else(|| {
                            let source = state.read(cx).image_source(&image.url);
                            FittedImage::new(
                                source.clone(),
                                img(source).object_fit(ObjectFit::Contain).with_fallback(
                                    crate::image_viewer::unpreviewed(true, true, cx),
                                ),
                                state,
                                format!("{path}/image/{ix}"),
                                cx,
                            )
                            .into_any_element()
                        })
                    }),
            )
            .into_any_element(),
    )
}

fn render_paragraph(
    paragraph: &Paragraph,
    id: &str,
    view: &Entity<MarkdownState>,
    cx: &mut App,
) -> AnyElement {
    let has_image = paragraph.children.iter().any(|child| child.image.is_some());
    let has_text = paragraph
        .children
        .iter()
        .any(|child| !child.text.is_empty());
    let has_image_link = paragraph
        .children
        .iter()
        .any(|child| super::image_link::for_node(child).is_some());
    if (has_image && has_text)
        || has_image_link
        || paragraph.children.iter().any(|child| child.math.is_some())
    {
        return InlineFlow::new(
            id.to_string(),
            super::selection::render_path(id),
            view.clone(),
            inline_flow_items(paragraph, id, cx),
        )
        .into_any_element();
    }
    if has_image {
        let images = paragraph
            .children
            .iter()
            .filter_map(|child| child.image.as_ref());
        let view = view.clone();
        return h_flex()
            .id(id.to_string())
            .flex_wrap()
            .gap_1()
            .children(images.enumerate().map(move |(ix, image)| {
                let title = image.title();
                let tooltip_title = title.clone();
                let view = view.clone();
                let source = view.read(cx).image_source(&image.url);
                let opened = view.read(cx).opened_image_source(&image.url);
                let image_owner = view.clone();
                let link = image.link.clone();
                let context_view = view.clone();
                let context_url = image.url.clone();
                let context_title = title.clone();
                let label = if !title.trim().is_empty() {
                    title.clone()
                } else if let Some(link) = &link {
                    link.url.to_string()
                } else {
                    crate::tr!("markdown.open_image").into_owned()
                };
                let role = if link.is_some() {
                    Role::Link
                } else {
                    Role::Button
                };
                let image =
                    crate::material::accessible_clickable(img(source.clone()), ix, role, label, cx)
                        .object_fit(ObjectFit::Contain)
                        .with_fallback(crate::image_viewer::unpreviewed(true, true, cx))
                        .cursor_pointer()
                        .when(link.is_some(), |image| {
                            image.tooltip(move |window, cx| {
                                Tooltip::new(tooltip_title.clone()).build(window, cx)
                            })
                        })
                        .on_mouse_down(MouseButton::Right, move |_, _, cx| {
                            context_view.update(cx, |state, cx| {
                                state.set_pending_context(
                                    Some(PendingContextTarget::Image {
                                        url: context_url.clone(),
                                        title: context_title.clone(),
                                    }),
                                    cx,
                                )
                            });
                        })
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            if let Some(link) = &link {
                                view.read(cx).resolve_link(&link.url).open(window, cx);
                            } else {
                                crate::image_viewer::open(
                                    opened.clone(),
                                    title.clone(),
                                    window,
                                    cx,
                                );
                            }
                        });
                FittedImage::new(source, image, &image_owner, format!("{id}/image/{ix}"), cx)
            }))
            .into_any_element();
    }

    let (text, links, highlights, fonts) = paragraph_text_style(paragraph, cx);
    if let Ok(mut inline_state) = paragraph.state.lock() {
        inline_state.set_text(text.into());
    }
    Inline::new(
        id.to_string(),
        view.clone(),
        paragraph.state.clone(),
        links,
        highlights,
        fonts,
    )
    .origin(Some(RunOrigin::new(super::selection::render_path(id))))
    .into_any_element()
}

fn paragraph_text_style(paragraph: &Paragraph, cx: &mut App) -> ParagraphTextStyle {
    let mut text = String::new();
    let mut links = Vec::new();
    let mut highlights = Vec::new();
    let mut fonts = Vec::new();
    for child in &paragraph.children {
        let offset = text.len();
        text.push_str(&child.text);
        let (node_links, node_highlights, node_fonts) = marks_for_node(&child.marks, offset, cx);
        links.extend(node_links);
        highlights = gpui::combine_highlights(highlights, node_highlights).collect();
        fonts.extend(node_fonts);
    }
    (text, links, highlights, fonts)
}

fn marks_for_node(marks: &[(Range<usize>, TextMark)], offset: usize, cx: &mut App) -> MarkRuns {
    let mut links = Vec::new();
    let mut highlights = Vec::new();
    let mut fonts = Vec::new();
    for (range, mark) in marks {
        let range = (offset + range.start)..(offset + range.end);
        let mut highlight = HighlightStyle::default();
        if mark.bold {
            highlight.font_weight = Some(FontWeight::BOLD);
        }
        if mark.italic {
            highlight.font_style = Some(FontStyle::Italic);
        }
        if mark.strikethrough {
            highlight.strikethrough = Some(gpui::StrikethroughStyle {
                thickness: px(1.),
                ..Default::default()
            });
        }
        if mark.code {
            highlight.background_color = Some(cx.theme().tokens.colors.muted);
            fonts.push((range.clone(), cx.theme().mono_font_family.clone()));
        }
        if let Some(link) = mark.link.clone() {
            highlight.color = Some(cx.theme().link);
            highlight.underline = Some(gpui::UnderlineStyle {
                thickness: px(1.),
                ..Default::default()
            });
            links.push((range.clone(), link));
        }
        highlights.push((range, highlight));
    }
    (links, highlights, fonts)
}

fn inline_flow_items(paragraph: &Paragraph, id: &str, cx: &mut App) -> Vec<InlineFlowItem> {
    let mut items = Vec::new();
    let mut text = String::new();
    let mut links = Vec::new();
    let mut highlights = Vec::new();
    let mut fonts = Vec::new();
    let mut segment_state: Option<Arc<std::sync::Mutex<InlineState>>> = None;
    // Where each child starts in the paragraph's text, and where the text
    // being gathered into one segment started.
    let mut child_end = 0;
    let mut segment_base = 0;
    let flush_text = |items: &mut Vec<InlineFlowItem>,
                      text: &mut String,
                      links: &mut Vec<(Range<usize>, super::nodes::LinkMark)>,
                      highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
                      fonts: &mut Vec<(Range<usize>, SharedString)>,
                      segment_state: &mut Option<Arc<std::sync::Mutex<InlineState>>>,
                      base: usize| {
        if text.is_empty() {
            return;
        }
        let state = segment_state
            .take()
            .unwrap_or_else(|| paragraph.state.clone());
        if let Ok(mut inline_state) = state.lock() {
            inline_state.set_text(text.clone().into());
        }
        items.push(InlineFlowItem::Text {
            state,
            base,
            text: std::mem::take(text).into(),
            links: std::mem::take(links),
            highlights: std::mem::take(highlights),
            font_overrides: std::mem::take(fonts),
            code_style: None,
        });
    };
    for (ix, child) in paragraph.children.iter().enumerate() {
        let child_start = child_end;
        child_end += child.rendered_text().len();
        if let Some(math) = &child.math {
            flush_text(
                &mut items,
                &mut text,
                &mut links,
                &mut highlights,
                &mut fonts,
                &mut segment_state,
                segment_base,
            );
            items.push(InlineFlowItem::Math {
                math: math.clone(),
                state: child.state.clone(),
                base: child_start,
                path: format!("{id}-math-{ix}"),
            });
            continue;
        }
        if let Some(link) = super::image_link::for_node(child) {
            flush_text(
                &mut items,
                &mut text,
                &mut links,
                &mut highlights,
                &mut fonts,
                &mut segment_state,
                segment_base,
            );
            items.push(InlineFlowItem::ImageLink {
                url: link.url.clone(),
                label: child.text.clone(),
            });
            continue;
        }
        if let Some(image) = &child.image {
            flush_text(
                &mut items,
                &mut text,
                &mut links,
                &mut highlights,
                &mut fonts,
                &mut segment_state,
                segment_base,
            );
            items.push(InlineFlowItem::Image {
                url: image.url.clone(),
                link: image.link.clone(),
                title: image.title(),
            });
            continue;
        }

        let is_code = child.marks.iter().any(|(_, mark)| mark.code);
        if is_code {
            flush_text(
                &mut items,
                &mut text,
                &mut links,
                &mut highlights,
                &mut fonts,
                &mut segment_state,
                segment_base,
            );
            let (code_links, code_highlights, code_fonts) = marks_for_node(&child.marks, 0, cx);
            if let Ok(mut inline_state) = child.state.lock() {
                inline_state.set_text(child.text.clone());
            }
            items.push(InlineFlowItem::Text {
                state: child.state.clone(),
                base: child_start,
                text: child.text.clone(),
                links: code_links,
                highlights: code_highlights,
                font_overrides: code_fonts,
                code_style: Some(InlineCodeStyle {
                    font_family: cx.theme().mono_font_family.clone(),
                    font_size: cx.theme().tokens.typography.mono_md.size,
                    background: cx.theme().tokens.colors.muted,
                    radius: crate::material::radius_chip(cx),
                }),
            });
            continue;
        }

        if text.is_empty() {
            segment_state = Some(child.state.clone());
            segment_base = child_start;
        }
        let offset = text.len();
        text.push_str(&child.text);
        let (node_links, node_highlights, node_fonts) = marks_for_node(&child.marks, offset, cx);
        links.extend(node_links);
        highlights = gpui::combine_highlights(highlights, node_highlights).collect();
        fonts.extend(node_fonts);
    }
    flush_text(
        &mut items,
        &mut text,
        &mut links,
        &mut highlights,
        &mut fonts,
        &mut segment_state,
        segment_base,
    );
    items
}

/// The id of the element rendering `span` of a block. Parts after the first
/// are its siblings in the root list, and a shared id would share their
/// element state.
fn span_id(path: &str, span: &Range<usize>) -> SharedString {
    if span.start == 0 {
        path.to_string().into()
    } else {
        format!("{path}@{}", span.start).into()
    }
}

#[allow(clippy::too_many_arguments)]
fn render_list(
    items: &[BlockNode],
    ordered: bool,
    start: u32,
    span: Range<usize>,
    options: RenderOptions,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let len = items.len();
    v_flex()
        .id(span_id(&options.path, &span))
        .w_full()
        .when(span.end == len, |list| list.pb(options.gap()))
        .children(items[span.clone()].iter().zip(span).map(|(item, ix)| {
            render_list_item(
                item,
                ix,
                RenderOptions {
                    ordered,
                    list_start: start,
                    is_last: ix + 1 == len,
                    path: format!("{}-{ix}", options.path),
                    ..options.clone()
                },
                state,
                window,
                cx,
            )
        }))
        .into_any_element()
}

fn render_list_item(
    item: &BlockNode,
    item_ix: usize,
    options: RenderOptions,
    state: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let BlockNode::ListItem {
        children,
        spread,
        checked,
        ..
    } = item
    else {
        return div().into_any_element();
    };
    let mut rows = Vec::new();
    for (ix, child) in children.iter().enumerate() {
        let child_options = RenderOptions {
            path: format!("{}-{ix}", options.path),
            in_list: true,
            depth: options.depth + 1,
            is_last: true,
            ..options.clone()
        };
        match child {
            BlockNode::Paragraph(_) if ix == 0 => {
                let content = div().flex_1().min_w_0().child(render_block(
                    child,
                    child_options,
                    state,
                    window,
                    cx,
                ));
                #[cfg(test)]
                let content = {
                    let path = options.path.clone();
                    content.debug_selector(move || format!("markdown-list-item-text-{path}"))
                };
                rows.push(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .items_start()
                        .when(checked.is_none(), |this| {
                            this.child(list_item_prefix(
                                item_ix,
                                options.ordered,
                                options.list_start,
                                options.depth,
                            ))
                        })
                        .when_some(*checked, |this, checked| {
                            this.child(
                                div()
                                    .mt(rems(0.35))
                                    .mr_1p5()
                                    .size(rems(0.875))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(cx.theme().primary)
                                    .when(checked, |this| {
                                        this.bg(cx.theme().tokens.colors.primary)
                                            .text_color(cx.theme().primary_foreground)
                                            .text_xs()
                                            .child("✓")
                                    }),
                            )
                        })
                        .child(content),
                );
            }
            BlockNode::List { .. } => rows.push(div().ml(rems(1.)).child(render_block(
                child,
                child_options,
                state,
                window,
                cx,
            ))),
            _ => rows.push(div().w_full().pl(rems(1.25)).child(render_block(
                child,
                child_options,
                state,
                window,
                cx,
            ))),
        }
    }
    let item = v_flex()
        .id(options.path.clone())
        .w_full()
        .min_w_0()
        .when(*spread, |this| this.gap_2())
        .children(rows);
    #[cfg(test)]
    let item = item.debug_selector(move || format!("markdown-list-item-{}", options.path));
    item.into_any_element()
}

fn cached_highlights(code: &CodeBlock, theme: &HighlightTheme) -> SharedHighlightRuns {
    let prepared = code.prepared();
    let compute = || {
        let key = CodeCacheKey {
            code: prepared.text.clone(),
            lang: code.lang.clone().unwrap_or_else(|| "text".into()),
            theme: theme.clone(),
        };
        CODE_HIGHLIGHTS.with(|cache| {
            cache.borrow_mut().get_or_insert_with(key, || {
                let previous = prepared
                    .previous_highlights
                    .get()
                    .and_then(std::sync::Weak::upgrade);
                Arc::new(highlight::HighlightedSource::new(
                    prepared.text.clone(),
                    code.lang.clone().unwrap_or_else(|| "text".into()),
                    theme,
                    previous.as_deref(),
                ))
            })
        })
    };
    let Ok(mut cache) = prepared.highlights.lock() else {
        return compute();
    };
    if let Some((cached_theme, runs)) = cache.as_ref()
        && cached_theme == theme
        && let Some(runs) = runs.upgrade()
    {
        return runs;
    }
    let runs = compute();
    // The global cache bounds retention; the block's shortcut must not keep evicted runs alive.
    *cache = Some((theme.clone(), Arc::downgrade(&runs)));
    runs
}

fn render_code_lines(
    code: &CodeBlock,
    span: Range<usize>,
    path: &str,
    view: &Entity<MarkdownState>,
    cx: &mut App,
) -> AnyElement {
    let path = super::selection::render_path(path);
    let prepared = code.prepared();
    let highlighted = cached_highlights(code, &cx.theme().highlight_theme);
    let all_runs = &highlighted.runs;
    let states = code.states_for_lines(prepared, span.clone());
    let mut offset = prepared.lines.get(span.start).map_or(0, |line| line.start);
    let span_end = span
        .end
        .checked_sub(1)
        .and_then(|ix| prepared.lines.get(ix))
        .map_or(offset, |line| line.end + 1);
    // Highlight runs are ordered and disjoint; only those in the span matter.
    let span_runs = &all_runs[all_runs.partition_point(|(range, _)| range.end <= offset)
        ..all_runs.partition_point(|(range, _)| range.start < span_end)];
    let mut rendered_lines = Vec::with_capacity(span.len());
    let mono_font_family = cx.theme().mono_font_family.clone();
    for (line_state, ix) in states.into_iter().zip(span.clone()) {
        let end = prepared.lines[ix].end;
        let runs = sub_runs(span_runs, offset, end);
        let line = div()
            .id(("code-line", ix))
            .min_h(px(18.))
            .whitespace_normal()
            .font_family(mono_font_family.clone())
            .text_size(cx.theme().tokens.typography.mono_md.size)
            .child(
                Inline::new(ix, view.clone(), line_state, Vec::new(), runs, Vec::new())
                    .origin(Some(RunOrigin::new(path.clone()).line(ix as u32))),
            );
        #[cfg(test)]
        let line = line.debug_selector(move || format!("markdown-code-line-{ix}"));
        rendered_lines.push(line);
        offset = end.saturating_add(1);
    }
    v_flex()
        .w_full()
        .children(rendered_lines)
        .into_any_element()
}

// Cold outer-list measurement never paints; use GPUI's multiline text layout
// without allocating selection state for every offscreen code line.
struct CodeSliceMeasure {
    measurement: Rc<RefCell<CodeMeasurement>>,
    code: CodeBlock,
    span: Range<usize>,
    path: String,
    view: Entity<MarkdownState>,
}

impl IntoElement for CodeSliceMeasure {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for CodeSliceMeasure {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
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
    ) -> (LayoutId, AnyElement) {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.pixel_snap(
            style
                .line_height
                .to_pixels(font_size.into(), window.rem_size()),
        );
        let prepared = self.code.prepared();
        let mut text_layout = None;
        let mut content = if line_height >= px(18.)
            && style.line_clamp.is_none()
            && style.text_overflow.is_none()
            && !self.span.is_empty()
        {
            let start = prepared.lines[self.span.start].start;
            let end = prepared.lines[self.span.end - 1].end;
            let highlighted = cached_highlights(&self.code, &cx.theme().highlight_theme);
            let all_runs = &highlighted.runs;
            let runs = &all_runs[all_runs.partition_point(|(range, _)| range.end <= start)
                ..all_runs.partition_point(|(range, _)| range.start < end)];
            let runs = sub_runs(runs, start, end);
            let text = StyledText::new(prepared.text[start..end].to_string()).with_highlights(runs);
            text_layout = Some((text.layout().clone(), end - start));
            text.into_any_element()
        } else {
            render_code_lines(&self.code, self.span.clone(), &self.path, &self.view, cx)
        };
        let layout = content.request_layout(window, cx);
        if let Some((text_layout, source_bytes)) = text_layout {
            // GPUI retains shaped lines while a TextLayout owns them. Cold
            // measurement nodes disappear on warm frames; keep one generation
            // alive so the next append can reuse unchanged line shapes.
            let mut measurement = self.measurement.borrow_mut();
            measurement.source_bytes = measurement.source_bytes.saturating_add(source_bytes);
            measurement.logical_lines = measurement.logical_lines.saturating_add(self.span.len());
            if measurement.weight() <= CODE_MEASUREMENT_WEIGHT_LIMIT {
                measurement.layouts.push(text_layout);
            } else {
                measurement.layouts.clear();
            }
        }
        (layout, content)
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        content: &mut AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) {
        content.prepaint_at(bounds.origin, window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        content: &mut AnyElement,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        content.paint(window, cx);
    }
}

fn render_code_block(
    code: &CodeBlock,
    span: Option<Range<usize>>,
    options: &RenderOptions,
    view: &Entity<MarkdownState>,
    window: &Window,
    cx: &mut App,
) -> AnyElement {
    let prepared = code.prepared();
    let code_text = prepared.text.as_ref();
    if mermaid::is_mermaid(code.lang.as_deref())
        && let Some(image) = mermaid::diagram(code_text, cx.theme().mode.is_dark())
    {
        return render_diagram(image, code_text, options, view, cx);
    }
    let span = span.unwrap_or(0..prepared.lines.len());
    let (first, last) = (span.start == 0, span.end == prepared.lines.len());
    let lines = if let Some(measurement) = &options.measurement {
        div()
            .w_full()
            .whitespace_normal()
            .font_family(cx.theme().mono_font_family.clone())
            .text_size(cx.theme().tokens.typography.mono_md.size)
            .child(CodeSliceMeasure {
                measurement: measurement.clone(),
                code: code.clone(),
                span: span.clone(),
                path: options.path.clone(),
                view: view.clone(),
            })
            .into_any_element()
    } else {
        render_code_lines(code, span.clone(), &options.path, view, cx)
    };
    let radius = crate::material::radius_card(cx);
    // The whole fence, not the painted span: a long block is split across
    // list items and the menu copies the code a reader sees as one block.
    let whole_code = prepared.text.clone();
    let context_view = view.clone();
    let is_math = super::math::is_math(code.lang.as_deref());
    // One button per fence, on the item that paints its first line.
    let copy = (first && !is_math).then(|| {
        let copied = view.read(cx).copied.is(&options.path);
        let compact = crate::window_seam::window_is_compact(window, cx);
        let (view, path, code) = (view.clone(), options.path.clone(), whole_code.clone());
        let button = copy_button(
            &format!("code-{}", options.path),
            copied,
            compact,
            move |_, _, cx| {
                cx.stop_propagation();
                view.update(cx, |state, cx| {
                    state.copy_code(
                        path.clone(),
                        code.strip_suffix('\n').unwrap_or(&code).to_string(),
                        cx,
                    )
                });
            },
            cx,
        );
        let corner = div()
            .absolute()
            .block_mouse_except_scroll()
            .top_1()
            .right_1()
            .rounded(crate::material::radius_card(cx))
            .bg(cx.theme().tokens.colors.muted)
            .child(button);
        #[cfg(test)]
        let corner = {
            let path = options.path.clone();
            corner.debug_selector(move || format!("markdown-code-copy-{path}"))
        };
        corner
    });
    let content = div()
        .id(span_id(&options.path, &span))
        .when(last && !options.is_last, |block| block.pb(rems(1.)))
        // A formula's source view takes the formula's menu from its block.
        .when(!is_math, |block| {
            block.on_mouse_down(MouseButton::Right, move |_, _, cx| {
                context_view.update(cx, |state, cx| {
                    state.set_pending_context(
                        Some(PendingContextTarget::CodeBlock(
                            whole_code
                                .strip_suffix('\n')
                                .unwrap_or(&whole_code)
                                .to_string(),
                        )),
                        cx,
                    )
                });
            })
        })
        .child(
            div()
                .px_3()
                .when(first, |block| block.pt_3().rounded_t(radius))
                .when(last, |block| block.pb_3().rounded_b(radius))
                .bg(cx.theme().tokens.colors.muted)
                .relative()
                .child(lines)
                .children(copy),
        )
        .into_any_element();
    if is_math {
        super::math::block(
            code_text,
            &code.formula,
            content,
            &options.path,
            view,
            window,
            cx,
        )
    } else {
        content
    }
}

/// A Mermaid fence whose source laid out: the picture replaces the code, and
/// the context menu still copies the source a reader would otherwise see.
fn render_diagram(
    image: Arc<Image>,
    source: &str,
    options: &RenderOptions,
    view: &Entity<MarkdownState>,
    cx: &mut App,
) -> AnyElement {
    let whole_code = source.strip_suffix('\n').unwrap_or(source).to_string();
    let context_view = view.clone();
    let label: SharedString = crate::tr!("markdown.mermaid_diagram").into_owned().into();
    let lightbox_label = label.clone();
    let source = ImageSource::Image(image);
    let lightbox_source = source.clone();
    let diagram = crate::material::accessible_clickable(
        img(source.clone()),
        SharedString::from(format!("markdown-mermaid-{}", options.path)),
        Role::Button,
        label,
        cx,
    )
    .object_fit(ObjectFit::Contain)
    .cursor_pointer()
    .on_click(move |_, window, cx| {
        cx.stop_propagation();
        crate::image_viewer::open(
            lightbox_source.clone(),
            lightbox_label.to_string(),
            window,
            cx,
        );
    });
    let block = div()
        .id(options.path.clone())
        .when(!options.is_last, |block| block.pb(rems(1.)))
        .on_mouse_down(MouseButton::Right, move |_, _, cx| {
            context_view.update(cx, |state, cx| {
                state.set_pending_context(
                    Some(PendingContextTarget::CodeBlock(whole_code.clone())),
                    cx,
                )
            });
        })
        .child(
            div()
                .p_3()
                .rounded(crate::material::radius_card(cx))
                .bg(cx.theme().tokens.colors.muted)
                .child(FittedImage::new(
                    source,
                    diagram,
                    view,
                    options.path.clone(),
                    cx,
                )),
        );
    #[cfg(test)]
    let block = {
        let path = options.path.clone();
        block.debug_selector(move || format!("markdown-mermaid-{path}"))
    };
    block.into_any_element()
}

fn render_table(
    table: &Table,
    options: &RenderOptions,
    view: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let mut column_count = 0;
    for row in &table.children {
        column_count = column_count.max(row.children.len());
    }
    render_scroll_table(table, column_count, options, view, window, cx)
}

fn table_track_width(column_widths: &[f32]) -> f32 {
    // Column widths cover text and horizontal cell padding. GPUI lays each
    // vertical separator outside that flex basis, while the track's border-box
    // consumes its two outer borders. Include both so the tracked child bounds
    // contain every painted cell: N - 1 separators plus 2 outer borders.
    column_widths.iter().sum::<f32>()
        + TABLE_BORDER_PX * (column_widths.len().saturating_sub(1).saturating_add(2) as f32)
}

#[allow(clippy::too_many_arguments)]
fn render_scroll_table(
    table: &Table,
    column_count: usize,
    options: &RenderOptions,
    view: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    const CELL_PAD_PX: f32 = 16.;
    const CELL_MIN_PX: f32 = 48.;
    const COL_MIN_PX: f32 = 100.;

    let text_style = window.text_style();
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    let mut widths = vec![CELL_MIN_PX; column_count];
    for (row_ix, row) in table.children.iter().enumerate() {
        for (ix, cell) in row.children.iter().enumerate() {
            let Some(slot) = widths.get_mut(ix) else {
                continue;
            };
            let mut width = Pixels::ZERO;
            let mut line_width = Pixels::ZERO;
            for child in &cell.children.children {
                let mut run_style = text_style.clone();
                if row_ix == 0 {
                    run_style.font_weight = FontWeight::SEMIBOLD;
                }
                for (_, mark) in &child.marks {
                    if mark.bold {
                        run_style.font_weight = FontWeight::BOLD;
                    }
                    if mark.italic {
                        run_style.font_style = FontStyle::Italic;
                    }
                    if mark.code {
                        run_style.font_family = cx.theme().mono_font_family.clone();
                    }
                }
                for (line_ix, fragment) in child.text.split('\n').enumerate() {
                    if line_ix > 0 {
                        width = width.max(line_width);
                        line_width = Pixels::ZERO;
                    }
                    if fragment.is_empty() {
                        continue;
                    }
                    let run = run_style.to_run(fragment.len());
                    line_width += window
                        .text_system()
                        .layout_line(fragment, font_size, &[run], None)
                        .width;
                }
            }
            width = width.max(line_width);
            *slot = slot.max(f32::from(width) + CELL_PAD_PX);
        }
    }
    let minimums = widths
        .iter()
        .map(|width| width.min(COL_MIN_PX))
        .collect::<Vec<_>>();
    let total_width = table_track_width(&minimums);
    let row_count = table.children.len();
    let rows = table
        .children
        .iter()
        .enumerate()
        .map(|(row_ix, row)| {
            div()
                .id(("table-row", row_ix))
                .w_full()
                .flex()
                .flex_row()
                .when(row_ix == 0, |this| {
                    this.bg(cx.theme().tokens.colors.muted)
                        .font_weight(FontWeight::SEMIBOLD)
                })
                .when(row_ix + 1 < row_count, |this| {
                    this.border_b_1().border_color(cx.theme().border)
                })
                .children(row.children.iter().enumerate().map(|(ix, cell)| {
                    let align = table.column_align(ix);
                    let width = widths.get(ix).copied().unwrap_or(CELL_MIN_PX);
                    let minimum = minimums.get(ix).copied().unwrap_or(CELL_MIN_PX);
                    let cell_content = div().min_w_0().child(render_paragraph(
                        &cell.children,
                        &format!("{}-{row_ix}-{ix}", options.path),
                        view,
                        cx,
                    ));
                    #[cfg(test)]
                    let cell_content = cell_content.debug_selector(move || {
                        format!("markdown-table-cell-content-{row_ix}-{ix}")
                    });
                    let rendered_cell = div()
                        .id(("table-cell", ix))
                        .flex_basis(px(width))
                        .flex_grow(width)
                        .min_w(px(minimum))
                        .whitespace_normal()
                        .flex()
                        .px_2()
                        .py_1()
                        .when(align == ColumnumnAlign::Center, |this| this.text_center())
                        .when(align == ColumnumnAlign::Right, |this| this.text_right())
                        .when(align == ColumnumnAlign::Center, |this| {
                            this.justify_center()
                        })
                        .when(align == ColumnumnAlign::Right, |this| this.justify_end())
                        .when(ix + 1 < row.children.len(), |this| {
                            this.border_r_1().border_color(cx.theme().border)
                        })
                        .child(cell_content);
                    #[cfg(test)]
                    let rendered_cell = rendered_cell
                        .debug_selector(move || format!("markdown-table-cell-{row_ix}-{ix}"));
                    rendered_cell
                }))
        })
        .collect::<Vec<_>>();
    let track = v_flex()
        .min_w_full()
        .w(px(total_width))
        .border_1()
        .border_color(cx.theme().border)
        .rounded(crate::material::radius_card(cx))
        .overflow_hidden()
        .children(rows);
    #[cfg(test)]
    let track = {
        let selector = format!("markdown-table-track-{}", options.path);
        track.debug_selector(move || selector.clone())
    };
    div()
        .id(options.path.clone())
        .w_full()
        .pb(if options.is_last { rems(0.) } else { rems(1.) })
        .child(
            div()
                .id(SharedString::from(format!("{}-viewport", options.path)))
                .w_full()
                .overflow_x_scroll_area()
                .child(track),
        )
        .into_any_element()
}
