//! Touch selection: the grab handles and the edit menu a long press leaves
//! behind, drawn over an [`crate::widgets::input::Input`], whose gesture
//! gpui-base owns, or over the selection of a Markdown document, whose
//! gesture [`crate::markdown::SelectionDocument`] owns.
//!
//! This module draws what the owner laid out: a handle at each end of the
//! selection, and a row of commands above it. Adapted from gpui-component's
//! Apache-2.0 `touch_selection` module, styled with tcode's own tokens.

mod edit_menu;
mod handle;

use std::rc::Rc;

use gpui::{AnyElement, App, Bounds, ElementId, IntoElement, Pixels, Point, TouchPhase, Window};
use gpui_base::{SelectionEdge, TouchHandle, TouchSelectionSnapshot};

pub(crate) use edit_menu::{EditMenu, EditMenuItem};
pub(crate) use handle::{DragHandler, SelectionHandles, SnapshotSource};

/// A touch selection as its owner laid it out: the carets at its ends,
/// whether each is in view, and the state of its menu and handles.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TouchSelectionLayout {
    pub(crate) start: Bounds<Pixels>,
    pub(crate) end: Bounds<Pixels>,
    pub(crate) start_visible: bool,
    pub(crate) end_visible: bool,
    pub(crate) menu_open: bool,
    pub(crate) dragging: Option<SelectionEdge>,
}

impl TouchSelectionLayout {
    pub(crate) fn edge(&self, edge: SelectionEdge) -> Bounds<Pixels> {
        match edge {
            SelectionEdge::Start => self.start,
            SelectionEdge::End => self.end,
        }
    }

    pub(crate) fn is_edge_visible(&self, edge: SelectionEdge) -> bool {
        match edge {
            SelectionEdge::Start => self.start_visible,
            SelectionEdge::End => self.end_visible,
        }
    }

    /// Whether the selection is a bare caret, which gets a menu but no
    /// handles.
    pub(crate) fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// The smallest box holding the ends in view, for anchoring the menu.
    pub(crate) fn bounds(&self) -> Option<Bounds<Pixels>> {
        match (self.start_visible, self.end_visible) {
            (true, true) => Some(self.start.union(&self.end)),
            (true, false) => Some(self.start),
            (false, true) => Some(self.end),
            (false, false) => None,
        }
    }
}

impl From<TouchSelectionSnapshot> for TouchSelectionLayout {
    fn from(snapshot: TouchSelectionSnapshot) -> Self {
        Self {
            start: snapshot.start(),
            end: snapshot.end(),
            start_visible: snapshot.is_edge_visible(SelectionEdge::Start),
            end_visible: snapshot.is_edge_visible(SelectionEdge::End),
            menu_open: snapshot.is_menu_open(),
            dragging: snapshot.dragging(),
        }
    }
}

/// Draws one touch selection: its handles and, when open, its edit menu.
///
/// The handles read the selection's geometry as they paint, so they follow
/// text that scrolls in the same frame. The menu is placed from the snapshot
/// read here; it steps aside while the text scrolls, so a frame's lag in its
/// anchor never shows.
pub(crate) struct TouchSelectionOverlay {
    id: ElementId,
    source: SnapshotSource,
    items: Vec<EditMenuItem>,
    on_drag: Option<DragHandler>,
}

impl TouchSelectionOverlay {
    pub(crate) fn new(
        id: impl Into<ElementId>,
        source: impl Fn(&Window, &App) -> Option<TouchSelectionLayout> + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            source: Rc::new(source),
            items: Vec::new(),
            on_drag: None,
        }
    }

    /// Draws floating handles, dragged through `on_drag`. An owner whose
    /// text paints its own handles in place leaves this unset.
    pub(crate) fn handles(
        mut self,
        on_drag: impl Fn(SelectionEdge, TouchPhase, Point<Pixels>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_drag = Some(Rc::new(on_drag));
        self
    }

    /// The commands the edit menu offers. With none, no menu is drawn.
    pub(crate) fn items(mut self, items: impl IntoIterator<Item = EditMenuItem>) -> Self {
        self.items.extend(items);
        self
    }

    /// The elements to add to the owner: the handles, and the menu when it
    /// is open. Each floats in window coordinates.
    pub(crate) fn into_elements(self, window: &Window, cx: &App) -> Vec<AnyElement> {
        let Some(snapshot) = (self.source)(window, cx) else {
            return Vec::new();
        };
        let mut elements = Vec::with_capacity(2);
        if let Some(on_drag) = self.on_drag {
            elements.push(SelectionHandles::new(self.source, on_drag).into_any_element());
        }

        if let Some(mut anchor) = snapshot
            .bounds()
            .filter(|_| snapshot.menu_open && !self.items.is_empty())
        {
            // Leave the knobs uncovered: the menu anchors to the selection
            // plus the room its handles take above and below.
            if !snapshot.is_empty() {
                anchor.origin.y -= TouchHandle::EXTENT;
                anchor.size.height += TouchHandle::EXTENT * 2.;
            }
            elements.push(
                EditMenu::new(self.id, anchor)
                    .items(self.items)
                    .into_any_element(),
            );
        }
        elements
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, LongPressEvent, Modifiers,
        ParentElement as _, Render, ScrollDelta, ScrollWheelEvent, Styled as _, TestAppContext,
        TouchDragEvent, TouchPhase, VisualTestContext, Window, div, point, px, size,
    };
    use gpui_base::{SelectionEdge, TouchHandle};

    use crate::widgets::input::{Input, InputState, Textarea, TextareaState};

    struct Probe {
        input: Entity<InputState>,
        textarea: Entity<TextareaState>,
        multiline: bool,
    }

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().p_4().w(px(320.)).child(if self.multiline {
                Textarea::new(&self.textarea).into_any_element()
            } else {
                Input::new(&self.input).into_any_element()
            })
        }
    }

    macro_rules! with_state {
        ($probe:expr, $cx:expr, |$state:ident| $body:expr) => {
            $probe.read_with($cx, |probe, cx| {
                if probe.multiline {
                    let $state = probe.textarea.read(cx);
                    $body
                } else {
                    let $state = probe.input.read(cx);
                    $body
                }
            })
        };
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
    }

    fn mount(cx: &mut TestAppContext, multiline: bool) -> (Entity<Probe>, &mut VisualTestContext) {
        cx.update(crate::theme::init);
        let (probe, cx) = cx.add_window_view(|window, cx| Probe {
            input: cx.new(|cx| InputState::new(window, cx).default_value("quick select value")),
            textarea: cx
                .new(|cx| TextareaState::new(window, cx).default_value("quick select value")),
            multiline,
        });
        cx.run_until_parked();
        draw(cx);
        (probe, cx)
    }

    fn long_press(cx: &mut VisualTestContext, position: gpui::Point<gpui::Pixels>) {
        for phase in [TouchPhase::Started, TouchPhase::Ended] {
            cx.simulate_event(LongPressEvent {
                phase,
                start_position: position,
                position,
            });
            draw(cx);
        }
    }

    fn tap(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is shown"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
        draw(cx);
    }

    fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
        cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
    }

    #[gpui::test]
    fn long_press_offers_the_edit_menu_whose_handles_drag(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for multiline in [false, true] {
            let (probe, cx) = mount(cx, multiline);
            let first = with_state!(probe, cx, |state| state.range_to_bounds(&(0..1))).unwrap();
            long_press(cx, first.center());
            assert_eq!(
                with_state!(probe, cx, |state| state.selected_text().to_string()),
                "quick"
            );
            for item in [
                "edit-menu-cut",
                "edit-menu-copy",
                "edit-menu-paste",
                "edit-menu-select-all",
            ] {
                assert!(cx.debug_bounds(item).is_some(), "{item} is offered");
            }

            // Select All keeps the handles and the menu over the whole text,
            // which leaves Select All itself with nothing to offer.
            tap(cx, "edit-menu-select-all");
            assert_eq!(
                with_state!(probe, cx, |state| state.selected_range()),
                0..18
            );
            assert!(cx.debug_bounds("edit-menu-copy").is_some());
            assert!(cx.debug_bounds("edit-menu-select-all").is_none());

            // Copy goes through the input's action and closes the menu; the
            // selection and its handles stay.
            tap(cx, "edit-menu-copy");
            assert_eq!(clipboard(cx).as_deref(), Some("quick select value"));
            assert!(cx.debug_bounds("edit-menu").is_none());
            let snapshot =
                with_state!(probe, cx, |state| state.touch_selection()).expect("handles remain");
            assert!(!snapshot.is_menu_open());

            // A touch on the start knob drags that end along the text; the
            // release brings the menu back.
            let finger = TouchHandle::hit_bounds(SelectionEdge::Start, snapshot.start()).center();
            cx.simulate_event(TouchDragEvent {
                phase: TouchPhase::Started,
                start_position: finger,
                position: finger,
            });
            assert_eq!(
                with_state!(probe, cx, |state| state
                    .touch_selection()
                    .unwrap()
                    .dragging()),
                Some(SelectionEdge::Start)
            );
            let target = with_state!(probe, cx, |state| state.range_to_bounds(&(6..7))).unwrap();
            for phase in [TouchPhase::Moved, TouchPhase::Ended] {
                cx.simulate_event(TouchDragEvent {
                    phase,
                    start_position: finger,
                    position: point(target.left(), finger.y),
                });
                draw(cx);
            }
            assert_eq!(
                with_state!(probe, cx, |state| state.selected_text().to_string()),
                "select value"
            );
            assert!(cx.debug_bounds("edit-menu").is_some());

            // Cut takes the selection with it; nothing is left to hold a
            // handle or a menu.
            tap(cx, "edit-menu-cut");
            assert_eq!(
                with_state!(probe, cx, |state| state.value().to_string()),
                "quick "
            );
            assert_eq!(clipboard(cx).as_deref(), Some("select value"));
            assert!(with_state!(probe, cx, |state| state.touch_selection()).is_none());
            assert!(cx.debug_bounds("edit-menu").is_none());
        }
    }

    /// An input `top` pixels down a phone-sized window.
    struct PlacedField {
        input: Entity<InputState>,
        top: f32,
    }

    impl Render for PlacedField {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .px_4()
                .pt(px(self.top))
                .child(Input::new(&self.input))
        }
    }

    /// Long-presses the first word of an input `top` pixels down the window,
    /// with the window's top `covered` pixels under a status bar, and returns
    /// the selection, the menu and its items' bounds.
    fn open_menu_at(
        cx: &mut TestAppContext,
        top: f32,
        covered: f32,
    ) -> (
        gpui::Bounds<gpui::Pixels>,
        gpui::Bounds<gpui::Pixels>,
        Vec<gpui::Bounds<gpui::Pixels>>,
    ) {
        cx.update(crate::theme::init);
        let (field, cx) = cx.add_window_view(|window, cx| PlacedField {
            input: cx.new(|cx| InputState::new(window, cx).default_value("quick select value")),
            top,
        });
        let window_size = size(px(390.), px(844.));
        cx.simulate_resize(window_size);
        let handle = gpui::VisualContext::window_handle(cx);
        cx.simulate_window_visual_viewport_change(
            handle,
            gpui::Bounds::new(
                point(px(0.), px(covered)),
                size(window_size.width, window_size.height - px(covered)),
            ),
        );
        cx.run_until_parked();
        draw(cx);
        let input = field.read_with(cx, |field, _| field.input.clone());
        let first = input
            .read_with(cx, |state, _| state.range_to_bounds(&(0..1)))
            .unwrap();
        long_press(cx, first.center());
        let selection = input
            .read_with(cx, |state, _| state.touch_selection())
            .and_then(|snapshot| snapshot.bounds())
            .expect("a long press selects a word");
        let menu = cx.debug_bounds("edit-menu").expect("the menu is open");
        let items = [
            "edit-menu-cut",
            "edit-menu-copy",
            "edit-menu-paste",
            "edit-menu-select-all",
        ]
        .map(|item| {
            cx.debug_bounds(item)
                .unwrap_or_else(|| panic!("{item} is offered"))
        });
        assert!(
            gpui::Bounds::new(point(px(0.), px(covered)), window_size).contains(&menu.origin)
                && menu.bottom_right().y <= window_size.height
                && menu.bottom_right().x <= window_size.width,
            "the menu shows in full: {menu:?}"
        );
        (selection, menu, items.to_vec())
    }

    #[gpui::test]
    fn the_edit_menu_is_one_row_of_finger_sized_items_above_the_handles(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let (selection, menu, items) = open_menu_at(cx, 300., 0.);
        let row = px(crate::material::TOUCH_TARGET);
        for item in &items {
            assert_eq!(item.size.height, row, "{item:?}");
            assert!(item.size.width >= row, "{item:?}");
            assert_eq!(item.top(), items[0].top(), "{item:?}");
        }
        // The pill hugs its row: nothing but its hairline above or below.
        assert_eq!(menu.size.height, row + px(2.));
        assert!(menu.bottom() <= selection.top() - TouchHandle::EXTENT);
    }

    #[gpui::test]
    fn the_edit_menu_stays_out_from_under_a_status_bar(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // Room above in the window, but under a status bar: the menu drops
        // below, clear of the end handle.
        let (selection, menu, _) = open_menu_at(cx, 90., 60.);
        assert!(menu.top() >= selection.bottom() + TouchHandle::EXTENT);
    }

    /// A textarea taller than its field, as the composer is.
    struct OverflowingField {
        textarea: Entity<TextareaState>,
    }

    impl Render for OverflowingField {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .p_4()
                .child(Textarea::new(&self.textarea).h(px(60.)))
        }
    }

    #[gpui::test]
    fn a_pan_over_a_textarea_scrolls_it_and_the_menu_steps_aside_until_the_finger_lifts(
        cx: &mut TestAppContext,
    ) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        cx.update(crate::theme::init);
        let text = (0..40)
            .map(|ix| format!("line {ix} of the draft"))
            .collect::<Vec<_>>()
            .join("\n");
        let textarea = std::rc::Rc::new(std::cell::OnceCell::new());
        let (_, cx) = cx.add_window_view({
            let textarea = textarea.clone();
            move |window, cx| {
                let state = cx.new(|cx| TextareaState::new(window, cx).default_value(&text));
                textarea.set(state.clone()).ok().unwrap();
                OverflowingField { textarea: state }
            }
        });
        let textarea = textarea.get().unwrap().clone();
        cx.simulate_resize(size(px(320.), px(400.)));
        cx.run_until_parked();
        draw(cx);
        draw(cx);

        // The second line: a short pan keeps it in the field's viewport.
        let second = textarea
            .read_with(cx, |state, _| state.range_to_bounds(&(20..21)))
            .unwrap();
        long_press(cx, second.center());
        assert_eq!(
            textarea.read_with(cx, |state, _| state.selected_text().to_string()),
            "line"
        );
        assert!(cx.debug_bounds("edit-menu-cut").is_some());
        let menu_open = |cx: &mut VisualTestContext| {
            textarea.read_with(cx, |state, _| {
                state
                    .touch_selection()
                    .map(|snapshot| snapshot.is_menu_open())
            })
        };
        assert_eq!(menu_open(cx), Some(true));
        let offset =
            |cx: &mut VisualTestContext| textarea.read_with(cx, |state, _| state.scroll_offset().y);
        assert_eq!(offset(cx), px(0.));

        // A finger panning the field reaches the input's own scroll handler:
        // the text moves, and the menu steps aside until the finger lifts,
        // then returns over the handles. The selection stays.
        // The finger lands on the field, clear of the handles' touch targets:
        // a touch that begins on a handle is that handle's drag, not a pan.
        let finger = point(second.left() + px(80.), second.center().y);
        let pan = |cx: &mut VisualTestContext, phase, dy| {
            cx.simulate_event(ScrollWheelEvent {
                position: finger,
                delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
                modifiers: Modifiers::default(),
                touch_phase: phase,
            });
            draw(cx);
        };
        for (phase, open) in [
            (TouchPhase::Started, false),
            (TouchPhase::Moved, false),
            (TouchPhase::Ended, true),
        ] {
            pan(cx, phase, -4.);
            assert_eq!(menu_open(cx), Some(open), "{phase:?}");
            assert_eq!(
                cx.debug_bounds("edit-menu-cut").is_some(),
                open,
                "{phase:?}"
            );
        }
        assert_eq!(offset(cx), px(-12.), "the pan scrolled the textarea");
        assert_eq!(
            textarea.read_with(cx, |state, _| state.selected_text().to_string()),
            "line"
        );
        assert!(cx.debug_bounds("edit-menu-cut").is_some());

        // A pan that carries the selection out of the field takes the
        // handles and the menu with it; the selection itself stays.
        for phase in [TouchPhase::Started, TouchPhase::Moved, TouchPhase::Ended] {
            pan(cx, phase, -40.);
        }
        assert_eq!(menu_open(cx), Some(true));
        assert!(cx.debug_bounds("edit-menu-cut").is_none());
        assert_eq!(
            textarea.read_with(cx, |state, _| state.selected_text().to_string()),
            "line"
        );
    }
}
