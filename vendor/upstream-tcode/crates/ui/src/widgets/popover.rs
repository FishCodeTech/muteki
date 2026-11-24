use crate::material;
use crate::scroll::ScrollableElement as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::{Button, ButtonVariants as _};
use gpui::{
    Anchor, AnyElement, App, Context, ElementId, FocusHandle, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement, RenderOnce, StyleRefinement, Styled, Window,
    prelude::FluentBuilder as _,
};
use gpui::{Focusable as _, Role, SharedString, StatefulInteractiveElement, deferred, div, px};
use gpui_base::StyledExt as _;
use std::rc::Rc;

pub use gpui_base::PopoverState;

/// Styled tcode facade over the headless gpui-base popover.
#[derive(IntoElement)]
pub struct Popover {
    id: ElementId,
    sheet_title: Option<SharedString>,
    anchor: Anchor,
    default_open: bool,
    open: Option<bool>,
    tracked_focus: Option<FocusHandle>,
    trigger: Option<TriggerBuilder>,
    content: Option<ContentBuilder>,
    children: Vec<AnyElement>,
    style: StyleRefinement,
    mouse_button: MouseButton,
    overlay_closable: bool,
    appearance: bool,
    on_open_change: Option<super::ToggleHandler>,
}

type TriggerBuilder = Box<dyn FnOnce(bool, Option<SheetToggle>, &Window, &App) -> AnyElement>;
pub(crate) type SheetToggle = Rc<dyn Fn(&mut Window, &mut App)>;
type ContentBuilder =
    Box<dyn FnOnce(&mut PopoverState, &mut Window, &mut Context<PopoverState>) -> AnyElement>;

impl Popover {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            sheet_title: None,
            anchor: Anchor::TopLeft,
            default_open: false,
            open: None,
            tracked_focus: None,
            trigger: None,
            content: None,
            children: Vec::new(),
            style: StyleRefinement::default(),
            mouse_button: MouseButton::Left,
            overlay_closable: true,
            appearance: true,
            on_open_change: None,
        }
    }
    /// Present the same picker state and content in a touch-sized bottom sheet.
    pub fn bottom_sheet(mut self, title: impl Into<SharedString>) -> Self {
        self.sheet_title = Some(title.into());
        self
    }
    pub fn anchor(mut self, anchor: impl Into<Anchor>) -> Self {
        self.anchor = anchor.into();
        self
    }
    pub fn mouse_button(mut self, button: MouseButton) -> Self {
        self.mouse_button = button;
        self
    }
    pub fn default_open(mut self, open: bool) -> Self {
        self.default_open = open;
        self
    }
    pub fn open(mut self, open: bool) -> Self {
        self.open = Some(open);
        self
    }
    pub fn overlay_closable(mut self, value: bool) -> Self {
        self.overlay_closable = value;
        self
    }
    pub fn appearance(mut self, value: bool) -> Self {
        self.appearance = value;
        self
    }
    pub fn track_focus(mut self, focus: &FocusHandle) -> Self {
        self.tracked_focus = Some(focus.clone());
        self
    }
    pub fn on_open_change(
        mut self,
        callback: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_open_change = Some(Rc::new(callback));
        self
    }
    pub fn trigger(mut self, trigger: super::Button) -> Self {
        self.trigger = Some(Box::new(move |open, toggle, _, _| {
            let is_open = gpui_base::Selectable::is_open(&trigger);
            let trigger = gpui_base::Selectable::open(trigger, is_open || open);
            match toggle {
                Some(toggle) => trigger.press_action(toggle).into_any_element(),
                None => trigger.into_any_element(),
            }
        }));
        self
    }
    pub(crate) fn trigger_with(
        mut self,
        trigger: impl FnOnce(bool, Option<SheetToggle>, &Window, &App) -> AnyElement + 'static,
    ) -> Self {
        self.trigger = Some(Box::new(trigger));
        self
    }
    pub fn content<F, E>(mut self, builder: F) -> Self
    where
        F: FnOnce(&mut PopoverState, &mut Window, &mut Context<PopoverState>) -> E + 'static,
        E: IntoElement,
    {
        self.content = Some(Box::new(move |state, window, cx| {
            builder(state, window, cx).into_any_element()
        }));
        self
    }
}

impl ParentElement for Popover {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl Styled for Popover {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Popover {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        if self.sheet_title.is_some() {
            return self.render_sheet(window, cx);
        }

        // gpui-base's `overlay_closable` dismisses on the outside press and
        // lets it through: it would also focus, select or press what the
        // popup uncovered. A press that closes a picker or a menu does
        // nothing else, so the press and its release are consumed here.
        let dismissal = crate::overlay::OutsideDismissal::new(self.id.clone(), window, cx);
        let release = dismissal.clone();
        let closable = self.overlay_closable;
        let appearance = self.appearance;
        let content = self.content;
        let children = self.children;
        let style = self.style;
        let state = window.use_keyed_state(self.id.clone(), cx, |_, cx| {
            PopoverState::new(self.default_open, cx)
        });
        gpui_base::Popover::new(self.id)
            .anchor(self.anchor)
            .mouse_button(self.mouse_button)
            .default_open(self.default_open)
            .overlay_closable(false)
            .when_some(self.open, |base, open| base.open(open))
            .when_some(self.tracked_focus, |base, focus| base.track_focus(&focus))
            .when_some(self.trigger, |base, trigger| {
                base.trigger_with(move |open, window, cx| {
                    let parent = window.current_view();
                    // gpui-base Popover::trigger installs no activation callback
                    // on the trigger element.
                    let toggle: SheetToggle = Rc::new(move |window, cx| {
                        state.update(cx, |state, cx| state.toggle_open(window, cx));
                        cx.notify(parent);
                    });
                    div()
                        .child(trigger(open, Some(toggle), window, cx))
                        .child(release.release_listener())
                        .into_any_element()
                })
            })
            .when_some(self.on_open_change, |base, callback| {
                base.on_open_change(move |open, window, cx| callback(open, window, cx))
            })
            .content(move |state, window, cx| {
                let popover = cx.entity();
                let parent = window.current_view();
                gpui_base::v_flex()
                    .when(appearance, |el| {
                        material::overlay_contour(el, cx)
                            .p_1()
                            .rounded(material::radius_overlay(cx))
                    })
                    .id("tcode-popover-content")
                    .when(closable, |el| {
                        el.on_mouse_down_out(move |_, window, cx| {
                            dismissal.consume(window, cx);
                            popover.update(cx, |state, cx| state.dismiss(window, cx));
                            cx.notify(parent);
                        })
                    })
                    .occlude()
                    .when_some(content, |el, builder| el.child(builder(state, window, cx)))
                    .children(children)
                    .refine_style(&style)
            })
            // Keep the base in this render scope so the activation callback
            // and gpui-base resolve the same keyed PopoverState.
            .render(window, cx)
            .into_any_element()
    }
}

impl Popover {
    fn render_sheet(self, window: &mut Window, cx: &mut App) -> AnyElement {
        let state = window.use_keyed_state(self.id.clone(), cx, |_, cx| {
            PopoverState::new(self.default_open, cx)
        });
        state.update(cx, |state, cx| {
            state.track_focus(self.tracked_focus);
            state.set_on_open_change(self.on_open_change);
            if let Some(open) = self.open {
                state.sync_open(open, window, cx);
            }
        });
        let dismissal = crate::overlay::OutsideDismissal::new(self.id.clone(), window, cx);
        let closable = self.overlay_closable;
        let open = state.read(cx).is_open();
        let parent = window.current_view();
        let close = state.clone();
        let binding = crate::overlay::bind_sheet(
            self.id.clone(),
            state.read(cx).focus_handle(cx),
            open,
            move |window, cx| {
                close.update(cx, |state, cx| state.dismiss(window, cx));
                cx.notify(parent);
            },
            window,
            cx,
        );
        let drag = binding.read(cx);
        let sheet_height = drag.height;
        let drag_progress = crate::edge_back::settle(
            (self.id.clone(), "edge-offset"),
            drag.progress,
            drag.dragging,
            window,
            cx,
        );
        let presence = gpui_base::motion::Presence::new(
            gpui::SharedString::from(format!("{:?}-sheet", self.id)),
            open,
        )
        .transition(gpui_base::motion::Transition::new(crate::edge_back::SETTLE))
        .sample(window, cx);
        let progress = presence.progress;
        let toggle: SheetToggle = Rc::new({
            let state = state.clone();
            move |window, cx| {
                state.update(cx, |state, cx| state.toggle_open(window, cx));
                cx.notify(parent);
            }
        });
        let mut root = div()
            .id(self.id)
            .on_click({
                let toggle = toggle.clone();
                move |_, window, cx| {
                    cx.stop_propagation();
                    toggle(window, cx)
                }
            })
            .when_some(self.trigger, |el, trigger| {
                el.child(trigger(open, Some(toggle), window, cx))
            })
            .child(dismissal.release_listener());
        // Presence initially samples a closed sheet as a zero-opacity exit.
        // Do not mount invisible hit targets over the trigger.
        if presence.should_render() && (open || progress > 0.) {
            let viewport = window.viewport_size();
            let insets = crate::window_seam::content_insets(window);
            // The sheet stops a window strip short of the top so the page it
            // covers stays in view; no owner names that reserve yet.
            let max_height = (viewport.height - insets.top - px(52.) - insets.bottom).max(px(0.));
            // The grabber, the title row and the hairline above the content.
            let chrome_height =
                px(material::SHEET_GRABBER_HEIGHT + material::SHEET_TITLE_HEIGHT + 1.);
            let close = state.clone();
            let focus = state.read(cx).focus_handle(cx);
            let content = self
                .content
                .map(|content| state.update(cx, |state, cx| content(state, window, cx)));
            let surface = material::sheet_contour(gpui_base::v_flex(), cx)
                .id("touch-picker-sheet")
                .debug_selector(|| "touch-picker-sheet".into())
                .role(Role::Group)
                .aria_label(self.sheet_title.clone().unwrap_or_default())
                .occlude()
                .opacity(progress)
                .w(viewport.width)
                .max_w(viewport.width)
                .flex_none()
                .max_h(max_height)
                .text_color(cx.theme().foreground)
                .child(material::sheet_grabber(cx))
                .child(
                    gpui_base::h_flex()
                        .h(px(material::SHEET_TITLE_HEIGHT))
                        .flex_none()
                        .px(px(material::COMPACT_PAGE_INSET))
                        .items_center()
                        .text_body(true, cx)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_title(cx)
                                .overflow_hidden()
                                .children(self.sheet_title.map(super::fade_text)),
                        )
                        .child(
                            Button::new("touch-picker-close")
                                .text()
                                .touch(true)
                                .label(crate::tr!("mobile.cancel"))
                                .on_click(move |_, window, cx| {
                                    close.update(cx, |state, cx| state.dismiss(window, cx));
                                    cx.notify(parent);
                                }),
                        ),
                )
                .child(material::faded_hairline(cx))
                .child(
                    div()
                        .id("touch-picker-content")
                        .debug_selector(|| "touch-picker-content".into())
                        .w_full()
                        // A bounded vertical area inside the sheet, with its
                        // own resolved height: a diagonal pan stays on its
                        // axis, and an edge hands the gesture on.
                        .max_h((max_height - chrome_height).max(px(0.)))
                        .overflow_y_scroll_area()
                        .p(px(material::COMPACT_PAGE_INSET))
                        .children(content)
                        .children(self.children),
                );
            let request_close = state.clone();
            root = root.child(
                deferred(
                    gpui_base::Sheet::new(cx)
                        .focus_handle(focus)
                        .overlay_closable(closable)
                        .request_close(move |window, cx| {
                            dismissal.dismissed();
                            request_close.update(cx, |state, cx| state.dismiss(window, cx));
                            cx.notify(parent);
                        })
                        .overlay(
                            div()
                                .id("touch-picker-backdrop")
                                .debug_selector(|| "touch-picker-backdrop".into())
                                .absolute()
                                .inset_0()
                                .occlude()
                                .bg(material::scrim(progress * (1. - drag_progress), cx)),
                        )
                        .surface(
                            div()
                                .absolute()
                                .left_0()
                                .bottom(
                                    insets.bottom
                                        - crate::edge_back::BackProgress::offset(
                                            if open {
                                                drag_progress
                                            } else {
                                                drag_progress.max(1. - progress)
                                            },
                                            sheet_height,
                                        ),
                                )
                                .w(viewport.width)
                                .on_children_prepainted(move |bounds, _, cx| {
                                    if let Some(bounds) = bounds.first() {
                                        binding.update(cx, |binding, _| {
                                            binding.height = bounds.size.height
                                        });
                                    }
                                })
                                .child(surface),
                        ),
                )
                .with_priority(gpui_base::POPUP_PRIORITY),
            );
        }
        root.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Render, TestAppContext, point};
    use std::cell::Cell;

    struct SheetHarness(Rc<Cell<usize>>);
    impl Render for SheetHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let selected = self.0.clone();
            div().size_full().flex().flex_col().justify_end().child(
                div().h(px(44.)).w_full().overflow_hidden().child(
                    Popover::new("sheet-test")
                        .bottom_sheet("Model")
                        .default_open(false)
                        .trigger(
                            crate::widgets::button::Button::new("trigger")
                                .label("Model")
                                .debug_selector(|| "sheet-trigger".into()),
                        )
                        .content(move |_, _, _| {
                            div()
                                .id("sheet-option")
                                .debug_selector(|| "sheet-option".into())
                                .h(px(48.))
                                .w_full()
                                .child("Choose")
                                .on_click(move |_, _, _| selected.set(selected.get() + 1))
                        }),
                ),
            )
        }
    }

    struct TallSheet;
    impl Render for TallSheet {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Popover::new("tall-sheet")
                .bottom_sheet("Options")
                .default_open(true)
                .content(|_, _, _| {
                    div()
                        .h(px(2000.))
                        .w_full()
                        .debug_selector(|| "tall-content".into())
                })
        }
    }

    #[gpui::test]
    fn tall_sheet_scrolls_below_navigation_and_above_keyboard(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| TallSheet);
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        crate::window_seam::occlude_for_test(
            cx,
            gpui::Edges {
                top: px(47.),
                bottom: px(300.),
                ..Default::default()
            },
        );
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let sheet = cx.debug_bounds("touch-picker-sheet").unwrap();
        assert_eq!(sheet.top(), px(99.));
        assert_eq!(sheet.bottom(), px(552.));
        let content = cx.debug_bounds("tall-content").unwrap();
        let viewport = cx.debug_bounds("touch-picker-content").unwrap();
        assert!(viewport.bottom() <= sheet.bottom());
        assert!(viewport.size.height < content.size.height);
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: viewport.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-200.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("tall-content").unwrap().top() < content.top());
        assert_eq!(cx.debug_bounds("touch-picker-sheet").unwrap(), sheet);
    }

    #[derive(Clone, Copy)]
    enum Kind {
        Popover,
        Sheet,
        Dropdown,
    }
    struct OutsideHarness {
        kind: Kind,
        clicks: Rc<Cell<usize>>,
        releases: Rc<Cell<usize>>,
    }
    impl Render for OutsideHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            use crate::widgets::menu::DropdownMenu as _;
            let button = crate::widgets::Button::new("open")
                .label("Open")
                .debug_selector(|| "outside-trigger".into());
            let trigger = match self.kind {
                Kind::Dropdown => button
                    .dropdown_menu(|menu, _, _| {
                        menu.menu("Choose", Box::new(gpui_base::actions::Cancel))
                    })
                    .into_any_element(),
                kind => Popover::new("outside-popover")
                    .when(matches!(kind, Kind::Sheet), |popover| {
                        popover.bottom_sheet("Choose")
                    })
                    .trigger(button)
                    .content(|_, _, _| div().size(px(60.)).child("Option"))
                    .into_any_element(),
            };
            let clicks = self.clicks.clone();
            let releases = self.releases.clone();
            div().size_full().child(trigger).child(
                div()
                    .id("outside-target")
                    .absolute()
                    .top(px(300.))
                    .left(px(280.))
                    .size(px(60.))
                    .debug_selector(|| "outside-target".into())
                    .on_mouse_up(MouseButton::Left, move |_, _, _| {
                        releases.set(releases.get() + 1)
                    })
                    .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
            )
        }
    }

    #[gpui::test]
    fn all_popovers_consume_dismissal_release_even_after_redraw(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        for kind in [Kind::Popover, Kind::Sheet, Kind::Dropdown] {
            for redraw in [false, true] {
                let clicks = Rc::new(Cell::new(0));
                let releases = Rc::new(Cell::new(0));
                let (_, cx) = cx.add_window_view({
                    let clicks = clicks.clone();
                    let releases = releases.clone();
                    move |_, _| OutsideHarness {
                        kind,
                        clicks,
                        releases,
                    }
                });
                cx.simulate_resize(gpui::size(px(393.), px(852.)));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                let trigger = cx.debug_bounds("outside-trigger").unwrap().center();
                cx.simulate_click(trigger, Default::default());
                cx.update(|window, cx| window.draw(cx).clear(cx));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                let position = cx.debug_bounds("outside-target").unwrap().center();
                cx.simulate_event(gpui::MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                });
                if redraw {
                    cx.update(|window, cx| window.draw(cx).clear(cx));
                }
                cx.simulate_event(gpui::MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                });
                assert_eq!(clicks.get(), 0);
                assert_eq!(
                    releases.get(),
                    0,
                    "dismissed popup must retain its release guard"
                );
                cx.run_until_parked();
                cx.executor()
                    .advance_clock(std::time::Duration::from_millis(200));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                cx.simulate_click(position, Default::default());
                assert_eq!(clicks.get(), 1);
                assert_eq!(releases.get(), 1);
            }
        }
    }

    #[gpui::test]
    fn a_dismissal_without_a_press_leaves_the_next_click_alone(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let clicks = Rc::new(Cell::new(0));
        let releases = Rc::new(Cell::new(0));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            let releases = releases.clone();
            move |_, _| OutsideHarness {
                kind: Kind::Sheet,
                clicks,
                releases,
            }
        });
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let trigger = cx.debug_bounds("outside-trigger").unwrap().center();
        cx.simulate_click(trigger, Default::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("touch-picker-sheet").is_some());

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("touch-picker-sheet").is_none());

        let position = cx.debug_bounds("outside-target").unwrap().center();
        cx.simulate_click(position, Default::default());
        assert_eq!((clicks.get(), releases.get()), (1, 1));
    }

    #[gpui::test]
    fn bottom_sheet_options_receive_clicks_above_the_backdrop(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let selected = Rc::new(Cell::new(0));
        let (_, window) = cx.add_window_view({
            let selected = selected.clone();
            move |_, _| SheetHarness(selected)
        });
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.update(|window, cx| window.draw(cx).clear(cx));
        let trigger = window.debug_bounds("sheet-trigger").expect("trigger");
        window.simulate_click(trigger.center(), gpui::Modifiers::default());
        // Let the 180ms present transition finish so the sheet is at its
        // resting position before the option is hit-tested.
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            selected.get(),
            0,
            "opening must not select an overlapping option"
        );
        let bounds = window
            .debug_bounds("sheet-option")
            .expect("sheet option is laid out");
        window.simulate_click(bounds.center(), gpui::Modifiers::default());
        assert_eq!(selected.get(), 1);
    }
}
