//! Tcode's tab bars: styled compositions of `gpui_base::Tabs`/`Tab`.
//!
//! [`TabBar`] has three forms and one geometry per form: [`TabBar::underline`]
//! for page and view navigation, [`TabBar::segmented`] for a view switch
//! inside a panel, and [`TabBar::rail`] for a vertical navigation list. The
//! segmented drawing is shared with [`super::choice::ChoiceGroup`].
use crate::{
    icon::Icon,
    material::{self, TOUCH_TARGET, focus_ring},
    scroll::{ScrollArea, ScrollableElement as _},
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        Tooltip,
        menu::{ContextMenuExt as _, PopupMenu},
    },
};
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, InteractiveElement, IntoElement,
    ParentElement, Pixels, RenderOnce, SharedString, Stateful, StatefulInteractiveElement as _,
    StyleRefinement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_base::{StyledExt as _, Tab, Tabs};
use std::rc::Rc;

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type MenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// The underline indicator's weight.
const INDICATOR: f32 = 2.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Underline,
    Segmented,
    Rail,
}

/// One tab of a [`TabBar`].
pub struct TabItem {
    id: ElementId,
    label: SharedString,
    icon: Option<Icon>,
    selected: bool,
    count: Option<usize>,
    trailing: Option<AnyElement>,
    on_click: Option<ClickHandler>,
    context_menu: Option<MenuBuilder>,
}

impl TabItem {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            selected: false,
            count: None,
            trailing: None,
            on_click: None,
            context_menu: None,
        }
    }
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(icon.into());
        self
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    /// A muted count after the label, also read out with it.
    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }
    /// A control or mark beside the label, such as a close button. It shares
    /// the tab's surface but not its click, focus or accessible name.
    pub fn trailing(mut self, element: impl IntoElement) -> Self {
        self.trailing = Some(element.into_any_element());
        self
    }
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
    /// A right-click menu on the whole surface, including its trailing control.
    pub fn context_menu(
        mut self,
        builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    ) -> Self {
        self.context_menu = Some(Rc::new(builder));
        self
    }
}

/// The tab list of a page, panel or navigation rail.
///
/// Its own styles place the bar (width, flex, margins); the form owns
/// everything inside it.
#[derive(IntoElement)]
pub struct TabBar {
    id: ElementId,
    form: Form,
    compact: bool,
    icon_only: bool,
    hairline: bool,
    label_max_w: Option<Pixels>,
    aria_label: Option<SharedString>,
    items: Vec<TabItem>,
    style: StyleRefinement,
}

impl TabBar {
    /// An underline bar, the default form.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            form: Form::Underline,
            compact: false,
            icon_only: false,
            hairline: true,
            label_max_w: None,
            aria_label: None,
            items: Vec::new(),
            style: StyleRefinement::default(),
        }
    }
    /// Horizontal page or view navigation over a hairline.
    pub fn underline(mut self) -> Self {
        self.form = Form::Underline;
        self
    }
    /// Whether the underline bar draws its separator. Disable when its toolbar
    /// draws the same hairline so translucent borders are painted only once.
    pub fn hairline(mut self, hairline: bool) -> Self {
        self.hairline = hairline;
        self
    }
    /// A horizontal view switch inside a panel: a pill track.
    pub fn segmented(mut self) -> Self {
        self.form = Form::Segmented;
        self
    }
    /// A vertical navigation list.
    pub fn rail(mut self) -> Self {
        self.form = Form::Rail;
        self
    }
    /// Touch geometry: every tab is at least a [`TOUCH_TARGET`] tall.
    pub fn compact(mut self, compact: bool) -> Self {
        self.compact = compact;
        self
    }
    /// Tabs show only their icons, each named by a tooltip with its label.
    pub fn icon_only(mut self, icon_only: bool) -> Self {
        self.icon_only = icon_only;
        self
    }
    /// Clamp visible labels while keeping their full accessible names.
    /// Unset by default; overflowing bars still scroll.
    pub fn label_max_w(mut self, width: impl Into<Pixels>) -> Self {
        self.label_max_w = Some(width.into());
        self
    }
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }
    pub fn item(mut self, item: TabItem) -> Self {
        self.items.push(item);
        self
    }
    pub fn items(mut self, items: impl IntoIterator<Item = TabItem>) -> Self {
        self.items.extend(items);
        self
    }
}

impl Styled for TabBar {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

/// Per-form sizes. Desktop tabs share the 28pt of a small toolbar control;
/// compact tabs are a full touch target. A rail row's height, padding and
/// gap are [`material::selectable_row`]'s.
struct Metrics {
    /// The height of each tab's surface.
    item: f32,
    /// The surface's inline padding.
    pad: f32,
    /// The bar's height, for horizontal forms.
    bar: f32,
    /// An icon-only tab's glyph; beside a label the icon takes the text size.
    icon: f32,
}

fn metrics(form: Form, compact: bool) -> Metrics {
    match (form, compact) {
        (Form::Underline, false) => Metrics {
            item: 28.,
            pad: 10.,
            bar: 40.,
            icon: 16.,
        },
        (Form::Underline, true) => Metrics {
            item: TOUCH_TARGET,
            pad: 12.,
            bar: TOUCH_TARGET,
            icon: 18.,
        },
        (Form::Segmented, compact) => {
            let item = segment_height(compact);
            Metrics {
                item,
                pad: if compact { 14. } else { 12. },
                bar: item + 2. * TRACK_INSET,
                icon: if compact { 18. } else { 16. },
            }
        }
        (Form::Rail, compact) => Metrics {
            item: material::selectable_row_height(compact),
            pad: 0.,
            bar: 0.,
            icon: if compact { 20. } else { 16. },
        },
    }
}

fn segment_height(compact: bool) -> f32 {
    if compact { TOUCH_TARGET } else { 28. }
}

impl RenderOnce for TabBar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            form,
            compact,
            icon_only,
            hairline,
            label_max_w,
            aria_label,
            items,
            style,
        } = self;
        let m = metrics(form, compact);
        let count = items.len();
        let tabs = items.into_iter().enumerate().map(|(ix, item)| {
            render_item(
                item,
                ix,
                count,
                form,
                compact,
                icon_only,
                label_max_w,
                &m,
                cx,
            )
        });
        let list = Tabs::new(id.clone())
            .when_some(aria_label, |this, label| this.aria_label(label))
            .children(tabs);
        match form {
            Form::Underline => {
                let list = list
                    .flex()
                    .items_center()
                    .h_full()
                    .min_w_full()
                    .flex_shrink_0()
                    .gap_1();
                let viewport = div()
                    .id((id.clone(), "viewport"))
                    .flex()
                    .h(px(m.bar))
                    .overflow_x_scroll_area()
                    .child(list);
                div()
                    .debug_selector(|| id.to_string())
                    .relative()
                    .min_w_0()
                    .max_w_full()
                    .h(px(m.bar))
                    .min_h(px(m.bar))
                    .when(hairline, |bar| {
                        bar.child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                .h(px(1.))
                                .bg(cx.theme().border),
                        )
                    })
                    .child(viewport)
                    .refine_style(&style)
                    .into_any_element()
            }
            Form::Segmented => segmented_track(id.clone(), list, compact, cx)
                .debug_selector(|| id.to_string())
                .refine_style(&style)
                .into_any_element(),
            Form::Rail => list
                .debug_selector(|| id.to_string())
                .flex()
                .flex_col()
                .when(icon_only, |this| this.items_center())
                .gap(px(2.))
                .refine_style(&style)
                .into_any_element(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_item(
    item: TabItem,
    ix: usize,
    count: usize,
    form: Form,
    compact: bool,
    icon_only: bool,
    label_max_w: Option<Pixels>,
    m: &Metrics,
    cx: &App,
) -> AnyElement {
    let TabItem {
        id,
        label,
        icon,
        selected,
        count: tally,
        trailing,
        on_click,
        context_menu,
    } = item;
    let theme = cx.theme();
    let ring = focus_ring(cx);
    let accessible = match tally {
        Some(tally) => crate::tr!("tabs.with_count", label = label.clone(), count = tally)
            .into_owned()
            .into(),
        None => label.clone(),
    };
    // A rail reads as a list: its labels are body text, its icons muted.
    let (fg, icon_fg) = match form {
        _ if selected => (theme.foreground, theme.foreground),
        Form::Rail => (theme.foreground, theme.muted_foreground),
        _ => (theme.muted_foreground, theme.muted_foreground),
    };
    let trailing = trailing.filter(|_| !icon_only);
    let gap = px(if compact { 8. } else { 6. });
    // The item id doubles as its debug selector, so a test finds a tab by the
    // id its call site gave it.
    let selector = id.to_string();
    let tab = Tab::new(id.clone())
        .debug_selector(move || selector)
        .selected(selected)
        .set_position(ix + 1, count)
        .accessibility_label(accessible)
        .tab_index(0)
        .focus_visible(move |style| style.shadow(vec![ring]))
        .cursor_pointer()
        .gap(gap)
        .when_some(icon, |this, icon| {
            let color = icon.explicit_text_color().unwrap_or(icon_fg);
            let icon = if icon_only {
                icon.size(px(m.icon))
            } else {
                icon
            };
            this.child(icon.flex_none().text_color(color))
        })
        .when(!icon_only, |this| {
            this.child(
                div()
                    .flex_none()
                    .when_some(label_max_w, |label, width| {
                        label.max_w(width).overflow_hidden()
                    })
                    .child(super::fade_text(label.clone())),
            )
            .when_some(tally, |this, tally| {
                this.child(
                    div()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(tally.to_string()),
                )
            })
        })
        .when(icon_only, |this| {
            this.tooltip(move |window, cx| Tooltip::new(label.clone()).build(window, cx))
        })
        .when_some(on_click, |tab, handler| {
            tab.on_click(move |event, window, cx| handler(event, window, cx))
        });
    let Some(trailing) = trailing else {
        let tab = tab.text_color(fg);
        let tab = surface(tab, form, selected, compact, icon_only, false, m, cx);
        return match context_menu {
            Some(builder) => tab
                .context_menu(move |menu, window, cx| builder(menu, window, cx))
                .into_any_element(),
            None => tab.into_any_element(),
        };
    };
    // The trailing control shares the tab's surface but sits beside the tab,
    // outside its click, focus and accessible name.
    let row = div()
        .id((id, "surface"))
        .text_color(fg)
        .child(
            tab.h_full()
                .rounded(surface_radius(form, cx))
                .when(form == Form::Rail, |tab| {
                    tab.flex_1().min_w_0().justify_start()
                }),
        )
        .child(div().flex_none().flex().items_center().child(trailing));
    let row = surface(row, form, selected, compact, icon_only, true, m, cx);
    match context_menu {
        Some(builder) => row
            .context_menu(move |menu, window, cx| builder(menu, window, cx))
            .into_any_element(),
        None => row.into_any_element(),
    }
}

/// The visible surface of one tab: the tab itself, or the row holding the tab
/// and its trailing control.
#[allow(clippy::too_many_arguments)]
fn surface<E: Styled + InteractiveElement + ParentElement + FluentBuilder>(
    el: E,
    form: Form,
    selected: bool,
    compact: bool,
    icon_only: bool,
    trailing: bool,
    m: &Metrics,
    cx: &App,
) -> E {
    let theme = cx.theme();
    let foreground = theme.foreground;
    let end = if trailing {
        if compact { 8. } else { 4. }
    } else {
        m.pad
    };
    let el = el
        .relative()
        .flex()
        .items_center()
        .flex_none()
        .text_body(compact, cx);
    match form {
        // Only a rail may change weight with selection: its labels do not
        // share a line.
        Form::Rail => material::selectable_row(el, selected, compact, cx)
            .when(selected, |this| this.font_medium())
            .when(icon_only, |this| this.w(px(m.item)).px_0().justify_center())
            .when(!icon_only, |this| this.w_full().justify_start()),
        // Weight does not change with selection on a horizontal bar: a bolder
        // selected label would widen its tab and shift its neighbours.
        Form::Underline | Form::Segmented => el
            .font_medium()
            .gap(px(if compact { 8. } else { 6. }))
            .map(|this| {
                if icon_only {
                    this.min_w(px(m.item)).justify_center()
                } else {
                    this.pl(px(m.pad)).pr(px(end))
                }
            })
            .map(|this| {
                if form == Form::Segmented {
                    return segment(this, selected, true, compact, cx);
                }
                this.h(px(m.item))
                    .rounded(surface_radius(form, cx))
                    .when(!selected, |this| {
                        this.hover(move |style| style.bg(theme.muted).text_color(foreground))
                    })
                    .when(selected, |this| {
                        // Bottom-aligned with the bar's hairline, which it covers.
                        let (start, end) = if icon_only { (6., 6.) } else { (m.pad, end) };
                        this.child(
                            div()
                                .absolute()
                                .left(px(start))
                                .right(px(end))
                                .bottom(px(-(m.bar - m.item) / 2.))
                                .h(px(INDICATOR))
                                .rounded(px(INDICATOR / 2.))
                                .bg(foreground),
                        )
                    })
            }),
    }
}

fn surface_radius(form: Form, cx: &App) -> Pixels {
    match form {
        Form::Segmented => segment_radius(cx),
        Form::Underline | Form::Rail => material::radius_button(cx),
    }
}

/// The track's padding around its segments.
const TRACK_INSET: f32 = 2.;

/// A segment's corners sit concentric inside the track's.
fn segment_radius(cx: &App) -> Pixels {
    material::radius_input(cx) - px(TRACK_INSET)
}

/// The pill track shared by [`TabBar::segmented`] and
/// [`ChoiceGroup`](super::choice::ChoiceGroup): `list` holds the segments and
/// scrolls sideways once they no longer fit, and otherwise its segments divide
/// the width the track is given.
pub(crate) fn segmented_track(
    id: impl Into<ElementId>,
    list: impl Styled + ParentElement + IntoElement,
    compact: bool,
    cx: &App,
) -> ScrollArea<Stateful<Div>> {
    let id = id.into();
    let height = segment_height(compact) + 2. * TRACK_INSET;
    div()
        .id((id, "viewport"))
        .flex()
        .h(px(height))
        .min_h(px(height))
        .min_w_0()
        .overflow_x_scroll_area()
        .child(
            list.flex()
                .items_center()
                .h_full()
                .min_w_full()
                .flex_shrink_0()
                .gap(px(2.))
                .p(px(TRACK_INSET))
                .rounded(material::radius_input(cx))
                .bg(cx.theme().secondary),
        )
}

/// One segment's surface: transparent at rest, a raised pill when selected.
/// `interactive` is false for a segment that cannot be chosen.
pub(crate) fn segment<E: Styled + InteractiveElement + FluentBuilder>(
    el: E,
    selected: bool,
    interactive: bool,
    compact: bool,
    cx: &App,
) -> E {
    let theme = cx.theme();
    // The popover surface is the lightest plane in the light theme but
    // matches the track in the dark one, where the pill is lifted with the
    // foreground instead.
    let raised = if theme.mode.is_dark() {
        theme.foreground.opacity(0.12)
    } else {
        theme.popover
    };
    let (muted, foreground) = (theme.muted, theme.foreground);
    el.h(px(segment_height(compact)))
        .flex_grow(1.)
        .flex_basis(px(0.))
        .flex_shrink_0()
        .justify_center()
        .rounded(segment_radius(cx))
        .when(selected, |this| {
            this.bg(raised).text_color(foreground).shadow_xs()
        })
        .when(!selected && interactive, |this| {
            this.hover(move |style| style.bg(muted).text_color(foreground))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Modifiers, Render, TestAppContext, VisualTestContext};
    use std::cell::Cell;

    struct Segments(Vec<&'static str>);

    impl Render for Segments {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(320.)).child(
                TabBar::new("track").segmented().items(
                    self.0
                        .iter()
                        .enumerate()
                        .map(|(ix, label)| TabItem::new(("segment", ix), *label).selected(ix == 0)),
                ),
            )
        }
    }

    fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<Pixels> {
        cx.debug_bounds(selector).unwrap()
    }

    #[gpui::test]
    fn short_segments_divide_the_track_and_long_ones_overflow_it(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let (_, cx) = cx.add_window_view(|_, _| Segments(vec!["One", "Two", "Three"]));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let track = bounds(cx, "track");
        let segments = ["segment-0", "segment-1", "segment-2"].map(|s| bounds(cx, s));
        assert_eq!(track.size.width, px(320.));
        assert_eq!(segments[0].left(), track.left() + px(TRACK_INSET));
        assert_eq!(segments[2].right(), track.right() - px(TRACK_INSET));
        assert!(
            segments
                .windows(2)
                .all(|pair| pair[1].left() == pair[0].right() + px(2.)),
            "{segments:?}"
        );

        let long = "A label too long to share a phone-width track";
        let (_, cx) = cx.add_window_view(move |_, _| Segments(vec![long, long, long]));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let track = bounds(cx, "track");
        let last = bounds(cx, "segment-2");
        assert_eq!(track.size.width, px(320.));
        assert!(last.right() > track.right(), "{last:?}");
    }

    struct Closable(Rc<Cell<(usize, usize)>>);

    impl Render for Closable {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let (select, close) = (self.0.clone(), self.0.clone());
            TabBar::new("bar").item(
                TabItem::new("tab", "Terminal")
                    .on_click(move |_, _, _| select.set((select.get().0 + 1, select.get().1)))
                    .trailing(
                        crate::widgets::Button::new("close")
                            .debug_selector(|| "close".into())
                            .label("x")
                            .on_click(move |_, _, _| close.set((close.get().0, close.get().1 + 1))),
                    ),
            )
        }
    }

    #[gpui::test]
    fn a_trailing_control_does_not_activate_its_tab(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let clicks = Rc::new(Cell::new((0, 0)));
        let (_, cx) = cx.add_window_view({
            let clicks = clicks.clone();
            move |_, _| Closable(clicks)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let close = bounds(cx, "close");
        cx.simulate_click(close.center(), Modifiers::default());
        assert_eq!(clicks.get(), (0, 1));
        let tab = bounds(cx, "tab");
        cx.simulate_click(tab.center(), Modifiers::default());
        assert_eq!(clicks.get(), (1, 1));
    }
}
