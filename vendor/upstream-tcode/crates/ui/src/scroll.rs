//! Tcode-owned scroll-area compositions over gpui-base's scrollbar, scroll
//! mask and edge-bounce behavior.
//!
//! GPUI's own scroll containers apply every wheel or pan delta that reaches
//! them, so two nested viewports would both move on one gesture, and a
//! horizontal-only container maps vertical input onto its axis. The areas here
//! pair a viewport with a [`ScrollableMask`] sibling: the mask consumes the
//! events its viewport can use in the capture phase and lets the rest bubble
//! to the ancestor scroller.

use std::{ops::Range, panic::Location, rc::Rc};

use gpui::{
    AnyElement, App, Axis, Div, Element, ElementId, InteractiveElement, IntoElement, ListAlignment,
    ListOffset, ListSizingBehavior, ListState, ParentElement, Pixels, Refineable as _, RenderOnce,
    ScrollHandle, ScrollStrategy, Stateful, StatefulInteractiveElement, StyleRefinement, Styled,
    UniformListScrollHandle, Window, canvas, div, list, prelude::FluentBuilder as _, px,
    uniform_list,
};
use gpui_base::{
    InteractiveElementExt as _, ScrollBounce, ScrollableMask, Scrollbar, ScrollbarHandle,
    StyledExt as _,
};

pub(crate) trait ScrollableElement:
    InteractiveElement + Styled + ParentElement + Element
{
    /// A vertical viewport with Tcode's overlay scrollbar. Nested inside
    /// another scroller it owns vertical input while it can move and chains to
    /// the ancestor at its edges.
    #[track_caller]
    fn overflow_y_scrollbar(self) -> Scrollable<Self> {
        Scrollable::new(self)
    }

    /// A bounded vertical viewport without a scrollbar, for menus, option
    /// lists and detail panes that can sit inside a page or the timeline.
    /// It owns vertical input while it can move and chains at its edges.
    #[track_caller]
    fn overflow_y_scroll_area(self) -> ScrollArea<Self> {
        ScrollArea::new(self, Axis::Vertical)
    }

    /// A horizontal strip (tables, key bars, segmented tracks). It owns
    /// horizontal input even at its edges, so a horizontal gesture never moves
    /// the page, and leaves vertical input to the ancestor scroller.
    #[track_caller]
    fn overflow_x_scroll_area(self) -> ScrollArea<Self> {
        ScrollArea::new(self, Axis::Horizontal)
    }
}

/// A page-level vertical viewport with gpui-base's edge bounce.
/// `handle` must be the scroll handle of `element`.
/// Scrollbars and toolbars stay outside; never nest one inside a masked area.
#[track_caller]
pub(crate) fn page_viewport<H: ScrollbarHandle + Clone>(
    id: impl Into<ElementId>,
    handle: &H,
    element: impl IntoElement,
) -> ScrollBounce {
    // gpui-base 0.8's own wheel handling stretches the edge away from both
    // ends: a drag let go while moving throws it, and f32 rounding of the
    // delta the content used reads as reaching it. The next drag then moves
    // that stretch instead of the content. Scrolling itself, the content
    // stretches only by what is left over at an end.
    ScrollBounce::new(id, handle, element).scroll_wheel(false)
}

#[derive(IntoElement)]
pub(crate) struct Scrollable<E: InteractiveElement + Styled + ParentElement + Element> {
    id: ElementId,
    element: E,
}

impl<E> Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    #[track_caller]
    fn new(element: E) -> Self {
        Self {
            id: caller_id(),
            element,
        }
    }
}

impl<E> Styled for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl<E> ParentElement for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.element.extend(elements);
    }
}

impl<E> InteractiveElement for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.element.interactivity()
    }
}

impl<E> RenderOnce for Scrollable<E>
where
    E: InteractiveElement + Styled + ParentElement + Element + 'static,
{
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let scroll_handle = window
            .use_keyed_state(self.id.clone(), cx, |_, _| ScrollHandle::default())
            .read(cx)
            .clone();
        let root_style = root_style_from(self.element.style());
        let content = self
            .element
            .id((self.id.clone(), "content"))
            .flex_none()
            .h_auto()
            .min_h_full();
        let scroll_area = div()
            .id((self.id.clone(), "area"))
            .size_full()
            .flex()
            .flex_col()
            .track_scroll(&scroll_handle)
            .overflow_y_scroll()
            .lock_scroll_axis()
            .child(content);

        div()
            .id(self.id.clone())
            .size_full()
            .refine_style(&root_style)
            .relative()
            .child(scroll_area)
            .child(
                ScrollableMask::new(Axis::Vertical, &scroll_handle).id((self.id.clone(), "mask")),
            )
            .child(ScrollbarLayer {
                id: (self.id, "scrollbar").into(),
                scroll_handle: Rc::new(scroll_handle),
            })
    }
}

impl ScrollableElement for Div {}
impl<E> ScrollableElement for Stateful<E>
where
    E: ParentElement + Styled + Element,
    Self: InteractiveElement,
{
}

/// A masked viewport: the element keeps its id, styles and children and
/// becomes the scrolled viewport; a wrapper carries its sizing in the parent
/// layout and hosts the mask sibling.
#[derive(IntoElement)]
pub(crate) struct ScrollArea<E: InteractiveElement + Styled + ParentElement + Element> {
    id: ElementId,
    axis: Axis,
    element: E,
    handle: Option<ScrollHandle>,
}

impl<E> ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    #[track_caller]
    fn new(element: E, axis: Axis) -> Self {
        let fallback = caller_id();
        Self {
            id: Element::id(&element).unwrap_or(fallback),
            axis,
            element,
            handle: None,
        }
    }

    /// Scroll through `handle` instead of the area's own, for an owner that
    /// moves the area itself (e.g. to keep a keyboard selection visible).
    pub(crate) fn track_scroll(mut self, handle: &ScrollHandle) -> Self {
        self.handle = Some(handle.clone());
        self
    }
}

impl<E> Styled for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn style(&mut self) -> &mut StyleRefinement {
        self.element.style()
    }
}

impl<E> ParentElement for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn extend(&mut self, elements: impl IntoIterator<Item = gpui::AnyElement>) {
        self.element.extend(elements);
    }
}

impl<E> InteractiveElement for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element,
{
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.element.interactivity()
    }
}

impl<E> RenderOnce for ScrollArea<E>
where
    E: InteractiveElement + Styled + ParentElement + Element + 'static,
{
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let scroll_handle = self.handle.take().unwrap_or_else(|| {
            window
                .use_keyed_state((self.id.clone(), "scroll"), cx, |_, _| {
                    ScrollHandle::default()
                })
                .read(cx)
                .clone()
        });
        let root_style = root_style_from(self.element.style());
        // The wrapper takes the element's place in its parent's layout; the
        // viewport fills it unless a maximum height of its own bounds it.
        let bounded = self.element.style().max_size.height.is_some();
        let viewport = self
            .element
            .id(self.id.clone())
            .track_scroll(&scroll_handle);
        let viewport = match self.axis {
            // The mask moves the offset; the viewport only clips. GPUI's own
            // horizontal container would map vertical input onto this axis.
            Axis::Horizontal => viewport.overflow_hidden(),
            Axis::Vertical => viewport
                .when(!bounded, |viewport| viewport.max_h_full())
                .overflow_y_scroll()
                .lock_scroll_axis(),
        };
        div()
            .relative()
            .refine_style(&root_style)
            .child(viewport)
            .child(ScrollableMask::new(self.axis, &scroll_handle).id((self.id, "mask")))
    }
}

/// Give `state`'s unmeasured rows `row_height` once the list has laid out, so
/// wheel, bounce and scrollbar extents include rows not yet in view. Place it
/// after the list: GPUI clears height hints on the first layout and on width
/// changes.
pub(crate) fn list_height_hint(state: &ListState, row_height: Pixels) -> impl IntoElement {
    let list = state.clone();
    canvas(
        move |_, _, _| {
            if list.is_scrolled_to_end().is_none() && list.max_offset_for_scrollbar().y > px(0.) {
                list.clone().with_uniform_item_height(row_height);
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

type RenderRows = Box<dyn Fn(Range<usize>, &mut Window, &mut App) -> Vec<AnyElement>>;
type RenderRow = Box<dyn FnMut(usize, &mut Window, &mut App) -> AnyElement>;

enum Rows {
    /// Every row is as tall as row `measure`, which also sets the width; rows
    /// never wrap.
    Uniform { render: RenderRows, measure: usize },
    /// Rows are measured as they come into view.
    Measured(RenderRow),
}

/// A bounded vertical viewport that lays out only the rows in view, for
/// lists that can grow to hundreds of rows. It scrolls like
/// [`ScrollableElement::overflow_y_scroll_area`], keeps its position while it
/// stays rendered, and without a definite height shrinks to its rows up to its
/// maximum height. Row gaps belong to the rows.
#[derive(IntoElement)]
pub(crate) struct VirtualList {
    id: ElementId,
    count: usize,
    rows: Rows,
    style: StyleRefinement,
    scrollbar: bool,
    reveal: Option<usize>,
}

impl VirtualList {
    pub(crate) fn uniform<R: IntoElement>(
        id: impl Into<ElementId>,
        count: usize,
        render: impl Fn(Range<usize>, &mut Window, &mut App) -> Vec<R> + 'static,
    ) -> Self {
        Self::new(
            id,
            count,
            Rows::Uniform {
                render: Box::new(move |range, window, cx| {
                    render(range, window, cx)
                        .into_iter()
                        .map(|row| div().whitespace_nowrap().child(row).into_any_element())
                        .collect()
                }),
                measure: 0,
            },
        )
    }

    pub(crate) fn measured<R: IntoElement>(
        id: impl Into<ElementId>,
        count: usize,
        mut render: impl FnMut(usize, &mut Window, &mut App) -> R + 'static,
    ) -> Self {
        Self::new(
            id,
            count,
            Rows::Measured(Box::new(move |index, window, cx| {
                render(index, window, cx).into_any_element()
            })),
        )
    }

    fn new(id: impl Into<ElementId>, count: usize, rows: Rows) -> Self {
        Self {
            id: id.into(),
            count,
            rows,
            style: StyleRefinement::default(),
            scrollbar: false,
            reveal: None,
        }
    }

    /// Scroll row `index` into view whenever it changes, as a keyboard
    /// highlight moves.
    pub(crate) fn reveal(mut self, index: Option<usize>) -> Self {
        self.reveal = index;
        self
    }

    /// Size uniform rows, and a list without a definite width, by row `index`.
    pub(crate) fn width_from_row(mut self, index: usize) -> Self {
        if let Rows::Uniform { measure, .. } = &mut self.rows {
            *measure = index;
        }
        self
    }

    /// Overlay Tcode's scrollbar, as [`ScrollableElement::overflow_y_scrollbar`] does.
    pub(crate) fn scrollbar(mut self) -> Self {
        self.scrollbar = true;
        self
    }
}

impl Styled for VirtualList {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for VirtualList {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let root_style = root_style_from(&self.style);
        let revealed = window.use_keyed_state((self.id.clone(), "revealed"), cx, |_, _| None);
        let mut last_revealed = *revealed.read(cx);
        let mut width_probe = None;
        // A viewport under a cap it shares with pinned siblings shrinks to
        // what they leave.
        let (viewport, mask, scrollbar) = match self.rows {
            Rows::Uniform { render, measure } => {
                let scroll = window
                    .use_keyed_state((self.id.clone(), "scroll"), cx, |_, _| {
                        UniformListScrollHandle::new()
                    })
                    .read(cx)
                    .clone();
                let viewport = uniform_list(self.id.clone(), self.count, render)
                    .with_sizing_behavior(ListSizingBehavior::Infer)
                    .with_width_from_item(Some(measure))
                    .track_scroll(&scroll)
                    .min_h_0()
                    .refine_style(&self.style);
                if let Some(index) = reveal_next(self.reveal, self.count, last_revealed) {
                    scroll.scroll_to_item(index, ScrollStrategy::Nearest);
                }
                let base = scroll.0.borrow().base_handle.clone();
                let (mask, scrollbar) = overlays(&self.id, &base, self.scrollbar);
                (viewport.into_any_element(), mask, scrollbar)
            }
            Rows::Measured(mut render) => {
                let count = self.count;
                // A list that shrinks to its rows sizes itself from the rows
                // it measured; measuring a viewport past its last height lets
                // a longer list always reach its cap.
                let state = window
                    .use_keyed_state((self.id.clone(), "list"), cx, |window, _| {
                        ListState::new(count, ListAlignment::Top, window.viewport_size().height)
                    })
                    .read(cx)
                    .clone();
                if state.item_count() != count {
                    state.reset(count);
                    last_revealed = None;
                }
                if let Some(index) = reveal_next(self.reveal, self.count, last_revealed) {
                    reveal_row(&state, index);
                }
                // GPUI sizes a list that shrinks to its rows by measuring them
                // at their widest, so a wrapping row would be measured as one
                // line. Rows keep the width the list last laid out at, and a
                // new width lays the list out again.
                let bounds = state.viewport_bounds();
                let mut style = gpui::Style::default();
                style.refine(&self.style);
                let padding = style
                    .padding
                    .to_pixels(bounds.size.into(), window.rem_size());
                let width = bounds.size.width;
                let row_width = (width > px(0.)).then(|| width - padding.left - padding.right);
                width_probe = Some(width_probe_for(state.clone(), width));
                let render = move |index, window: &mut Window, cx: &mut App| {
                    let row = render(index, window, cx);
                    match row_width {
                        Some(width) => div().w(width).child(row).into_any_element(),
                        None => row,
                    }
                };
                let viewport = list(state.clone(), render)
                    .with_sizing_behavior(ListSizingBehavior::Infer)
                    .min_h_0()
                    .refine_style(&self.style);
                let (mask, scrollbar) = overlays(&self.id, &state, self.scrollbar);
                (viewport.into_any_element(), mask, scrollbar)
            }
        };
        let reveal = self.reveal.or(last_revealed);
        revealed.update(cx, |revealed, _| *revealed = reveal);
        div()
            .id((self.id, "area"))
            .relative()
            .flex()
            .flex_col()
            .refine_style(&root_style)
            .child(viewport)
            .children(width_probe)
            .child(mask)
            .children(scrollbar)
    }
}

/// The row a list should scroll to: one requested, in range and not already
/// revealed.
fn reveal_next(request: Option<usize>, count: usize, last: Option<usize>) -> Option<usize> {
    request.filter(|index| *index < count && last != Some(*index))
}

/// Scroll a measured list so row `index` is fully in view. A row not yet
/// measured below the top has no known position, so it moves to the top.
fn reveal_row(state: &ListState, index: usize) {
    let viewport = state.viewport_bounds();
    match state.bounds_for_item(index) {
        Some(row) if row.top() >= viewport.top() && row.bottom() <= viewport.bottom() => {}
        None if index > state.logical_scroll_top().item_ix => state.scroll_to(ListOffset {
            item_ix: index,
            offset_in_item: px(0.),
        }),
        _ => state.scroll_to_reveal_item(index),
    }
}

/// Renders the list's view again once the list lays out at a width other than
/// `width`, the one its rows were given.
fn width_probe_for(state: ListState, width: Pixels) -> AnyElement {
    canvas(
        move |_, window, _| {
            if state.viewport_bounds().size.width != width {
                window.request_animation_frame();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .into_any_element()
}

/// The mask that routes wheel input to a virtual list, and its scrollbar.
fn overlays<H: ScrollbarHandle + Clone>(
    id: &ElementId,
    handle: &H,
    scrollbar: bool,
) -> (AnyElement, Option<AnyElement>) {
    let mask = ScrollableMask::new(Axis::Vertical, handle)
        .id((id.clone(), "mask"))
        .into_any_element();
    let scrollbar = scrollbar.then(|| {
        ScrollbarLayer {
            id: (id.clone(), "scrollbar").into(),
            scroll_handle: Rc::new(handle.clone()),
        }
        .into_any_element()
    });
    (mask, scrollbar)
}

#[derive(IntoElement)]
struct ScrollbarLayer<H: ScrollbarHandle + Clone> {
    id: ElementId,
    scroll_handle: Rc<H>,
}

impl<H> RenderOnce for ScrollbarLayer<H>
where
    H: ScrollbarHandle + Clone + 'static,
{
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        if window.is_inspector_picking(cx) {
            return div();
        }
        div().absolute().inset_0().child(
            Scrollbar::vertical(self.scroll_handle.as_ref())
                .id(self.id)
                .viewport_from_layout(),
        )
    }
}

#[track_caller]
fn caller_id() -> ElementId {
    ElementId::CodeLocation(*Location::caller())
}

fn root_style_from(style: &StyleRefinement) -> StyleRefinement {
    StyleRefinement {
        size: style.size.clone(),
        min_size: style.min_size.clone(),
        max_size: style.max_size.clone(),
        flex_grow: style.flex_grow,
        flex_shrink: style.flex_shrink,
        flex_basis: style.flex_basis,
        align_self: style.align_self,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Context, ListAlignment, ListOffset, ListState, PlatformInput, Render, TestAppContext,
        TouchEvent, TouchId, TouchPhase, VisualTestContext, list, point, px,
    };

    /// A timeline-shaped nesting: a horizontal strip and a bounded vertical
    /// area inside `gpui::list` rows.
    struct NestedAreas(ListState);

    impl Render for NestedAreas {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(200.)).h(px(300.)).child(
                list(self.0.clone(), |ix, _, _| match ix {
                    0 => div()
                        .id("strip")
                        .debug_selector(|| "strip".into())
                        .w_full()
                        .h(px(60.))
                        .overflow_x_scroll_area()
                        .child(
                            div()
                                .debug_selector(|| "strip-content".into())
                                .w(px(800.))
                                .h(px(60.))
                                .flex_none(),
                        )
                        .into_any_element(),
                    1 => div()
                        .id("inner")
                        .debug_selector(|| "inner".into())
                        .w_full()
                        .max_h(px(100.))
                        .overflow_y_scroll_area()
                        .child(
                            div()
                                .debug_selector(|| "inner-content".into())
                                .w_full()
                                .h(px(400.))
                                .flex_none(),
                        )
                        .into_any_element(),
                    _ => div().w_full().h(px(80.)).into_any_element(),
                })
                .w_full()
                .h_full(),
            )
        }
    }

    struct VirtualLists(usize);

    impl Render for VirtualLists {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let count = self.0;
            let row = |kind: &'static str, index: usize, height: f32| {
                div()
                    .debug_selector(move || format!("{kind}-{index}"))
                    .h(px(height))
                    .flex_none()
            };
            div()
                .w(px(200.))
                .h(px(600.))
                .child(
                    div().debug_selector(|| "uniform".into()).child(
                        VirtualList::uniform("uniform-list", count, move |range, _, _| {
                            range.map(|index| row("uniform", index, 20.)).collect()
                        })
                        .w_full()
                        .max_h(px(100.))
                        .py(px(4.)),
                    ),
                )
                .child(
                    div().debug_selector(|| "measured".into()).child(
                        VirtualList::measured("measured-list", count, move |index, _, _| {
                            row("measured", index, if index % 2 == 0 { 20. } else { 30. })
                        })
                        .w_full()
                        .max_h(px(100.))
                        .py(px(4.)),
                    ),
                )
                .child(
                    div()
                        .debug_selector(|| "capped".into())
                        .flex()
                        .flex_col()
                        .max_h(px(100.))
                        .child(div().h(px(20.)).flex_none())
                        .child(
                            VirtualList::uniform("capped-list", count, move |range, _, _| {
                                range.map(|index| row("capped", index, 20.)).collect()
                            })
                            .w_full()
                            .min_h_0(),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .h(px(150.))
                        .child(div().h(px(50.)).flex_none())
                        .child(
                            div().flex().flex_col().flex_1().min_h_0().child(
                                VirtualList::measured("flexed-list", count, move |index, _, _| {
                                    row("flexed", index, 20.)
                                })
                                .flex_1()
                                .min_h_0(),
                            ),
                        ),
                )
        }
    }

    #[gpui::test]
    fn virtual_lists_shrink_to_short_lists_and_cap_long_ones(cx: &mut TestAppContext) {
        // A capped column shares its cap with a pinned caption; a flexed
        // slot takes a definite height from its column.
        for (count, uniform, measured, capped) in [(3, 68., 78., 80.), (300, 100., 100., 100.)] {
            let (_, cx) = cx.add_window_view(|_, _| VirtualLists(count));
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            assert_eq!(cx.debug_bounds("uniform").unwrap().size.height, px(uniform));
            assert_eq!(
                cx.debug_bounds("measured").unwrap().size.height,
                px(measured)
            );
            assert_eq!(cx.debug_bounds("capped").unwrap().size.height, px(capped));
            assert!(cx.debug_bounds("uniform-0").is_some());
            assert!(cx.debug_bounds("measured-0").is_some());
            assert!(cx.debug_bounds("uniform-250").is_none());
            assert!(cx.debug_bounds("measured-250").is_none());
            assert!(cx.debug_bounds("capped-250").is_none());
            assert!(cx.debug_bounds("flexed-0").is_some());
            assert!(cx.debug_bounds("flexed-250").is_none());
        }
    }

    struct NarrowRows;

    impl Render for NarrowRows {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(120.)).child(
                VirtualList::uniform("narrow-list", 3, |range, _, _| {
                    range
                        .map(|index| {
                            div()
                                .debug_selector(move || format!("narrow-{index}"))
                                .child("refactor/host-owned-live-turn")
                        })
                        .collect()
                })
                .max_h(px(200.)),
            )
        }
    }

    #[gpui::test]
    fn uniform_rows_stay_one_line_when_their_text_is_too_wide(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| NarrowRows);
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let first = cx.debug_bounds("narrow-0").unwrap();
        let second = cx.debug_bounds("narrow-1").unwrap();
        assert!(
            second.top() >= first.bottom(),
            "{first:?} overlaps {second:?}"
        );
    }

    fn touch(cx: &mut VisualTestContext, phase: TouchPhase, x: f32, y: f32) {
        cx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::Touch(TouchEvent {
                    id: TouchId(1),
                    phase,
                    position: point(px(x), px(y)),
                    predicted_position: None,
                    force: None,
                }),
                cx,
            );
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    /// One finger pan without release momentum.
    fn pan(cx: &mut VisualTestContext, from: (f32, f32), to: (f32, f32)) {
        touch(cx, TouchPhase::Started, from.0, from.1);
        touch(cx, TouchPhase::Moved, to.0, to.1);
        touch(cx, TouchPhase::Cancelled, to.0, to.1);
    }

    fn top(state: &ListState) -> (usize, gpui::Pixels) {
        let top = state.logical_scroll_top();
        (top.item_ix, top.offset_in_item)
    }

    #[gpui::test]
    fn strips_and_bounded_areas_inside_a_list_own_only_their_axis(cx: &mut TestAppContext) {
        let state = ListState::new(20, ListAlignment::Top, px(0.)).measure_all();
        let (_, cx) = cx.add_window_view({
            let state = state.clone();
            move |_, _| NestedAreas(state)
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let strip = cx.debug_bounds("strip").expect("strip viewport");
        let content = cx.debug_bounds("strip-content").expect("strip content");
        assert_eq!(strip.size.width, px(200.));

        // A horizontal pan over the strip moves the strip alone, and keeps
        // moving it at its edge rather than handing the rest to the list.
        pan(cx, (150., 30.), (30., 34.));
        let moved = cx.debug_bounds("strip-content").expect("strip content");
        assert!(moved.left() < content.left(), "strip scrolls sideways");
        assert_eq!(top(&state), (0, px(0.)));
        assert_eq!(cx.debug_bounds("strip").unwrap(), strip);
        pan(cx, (190., 30.), (10., 30.));
        pan(cx, (190., 30.), (10., 30.));
        pan(cx, (190., 30.), (10., 30.));
        pan(cx, (190., 30.), (10., 30.));
        assert_eq!(
            cx.debug_bounds("strip-content").unwrap().right(),
            strip.right(),
            "the strip clamps at its end"
        );
        assert_eq!(top(&state), (0, px(0.)));

        // A vertical pan over the strip scrolls the list.
        pan(cx, (100., 30.), (104., -100.));
        assert_ne!(top(&state), (0, px(0.)));
        state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });

        // A vertical pan over the bounded area moves it alone while it can
        // scroll, then chains to the list at its edge.
        let inner = cx.debug_bounds("inner").expect("inner viewport");
        let content = cx.debug_bounds("inner-content").expect("inner content");
        assert_eq!(inner.size.height, px(100.));
        let y = f32::from(inner.center().y);
        pan(cx, (100., y), (100., y - 50.));
        assert!(cx.debug_bounds("inner-content").unwrap().top() < content.top());
        assert_eq!(top(&state), (0, px(0.)));
        assert_eq!(cx.debug_bounds("inner").unwrap(), inner);
        pan(cx, (100., y), (100., y - 1000.));
        assert_eq!(top(&state), (0, px(0.)), "one gesture stays with the area");
        assert_eq!(
            cx.debug_bounds("inner-content").unwrap().bottom(),
            inner.bottom(),
            "the area clamps at its end"
        );
        pan(cx, (100., y), (100., y - 50.));
        assert_ne!(
            top(&state),
            (0, px(0.)),
            "the next gesture reaches the list"
        );
    }
}
