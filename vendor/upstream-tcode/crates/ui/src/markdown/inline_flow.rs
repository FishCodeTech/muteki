//! Mixed text, image and badge inline layout adapted from gpui-component's Apache-2.0
//! `text/inline_flow.rs` implementation.

use std::{
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
};

use crate::widgets::tooltip::Tooltip;
use gpui::{
    AbsoluteLength, AnyElement, App, AvailableSpace, Bounds, Element, ElementId, Entity,
    GlobalElementId, HighlightStyle, Hsla, InspectorElementId, InteractiveElement as _,
    IntoElement, LayoutId, LineFragment as WrapLineFragment, MouseButton, ObjectFit,
    ParentElement as _, Pixels, Role, SharedString, SharedUri, Size,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, TextRun, TextStyle, WhiteSpace,
    Window, div, img, point, prelude::FluentBuilder as _, px, relative, size,
};

use super::{
    inline::{Inline, InlineState},
    nodes::LinkMark,
    selection::RunOrigin,
    state::{MarkdownState, PendingContextTarget},
};

const ELEMENT_LEN: usize = 1;
const INLINE_CODE_PADDING_X: Pixels = px(3.);
const INLINE_CODE_PADDING_Y: Pixels = px(1.);

#[derive(Clone)]
pub(super) struct InlineCodeStyle {
    pub(super) font_family: SharedString,
    pub(super) font_size: Pixels,
    pub(super) background: Hsla,
    pub(super) radius: Pixels,
}

impl InlineCodeStyle {
    fn line_height(&self) -> Pixels {
        self.font_size + px(3.)
    }
}

pub(super) struct InlineFlow {
    id: ElementId,
    /// The paragraph's render path, which its runs' origins are in.
    path: Rc<[u32]>,
    view: Entity<MarkdownState>,
    items: Vec<InlineFlowItem>,
}

pub(super) enum InlineFlowItem {
    Math {
        math: super::math_parse::MathSpan,
        state: Arc<Mutex<InlineState>>,
        /// Where the formula starts in the paragraph's text.
        base: usize,
        path: String,
    },
    Text {
        state: Arc<Mutex<InlineState>>,
        /// Where the text starts in the paragraph's text.
        base: usize,
        text: SharedString,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        font_overrides: Vec<(Range<usize>, SharedString)>,
        code_style: Option<InlineCodeStyle>,
    },
    Image {
        url: SharedUri,
        link: Option<LinkMark>,
        title: String,
    },
    ImageLink {
        url: SharedString,
        label: SharedString,
    },
}

#[derive(Default)]
pub(super) struct InlineFlowLayoutState {
    layout: Arc<Mutex<Option<InlineFlowLayout>>>,
}

#[derive(Default)]
struct InlineFlowLayout {
    fragments: Vec<PositionedFragment>,
    size: Size<Pixels>,
}

#[derive(Clone)]
enum PositionedFragment {
    Text {
        item_ix: usize,
        origin: gpui::Point<Pixels>,
        size: Size<Pixels>,
        source_range: Range<usize>,
        text: SharedString,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        font_overrides: Vec<(Range<usize>, SharedString)>,
        code_style: Option<InlineCodeStyle>,
    },
    Element {
        item_ix: usize,
        origin: gpui::Point<Pixels>,
        size: Size<Pixels>,
    },
}

enum MeasureItem {
    Text {
        text: SharedString,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        font_overrides: Vec<(Range<usize>, SharedString)>,
        code_style: Option<InlineCodeStyle>,
    },
    Image,
    ImageLink,
}

struct LineFragmentLayout {
    item_ix: usize,
    kind: LineFragmentKind,
    size: Size<Pixels>,
    source_range: Range<usize>,
}

enum LineFragmentKind {
    Text {
        text: SharedString,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        font_overrides: Vec<(Range<usize>, SharedString)>,
        code_style: Option<InlineCodeStyle>,
    },
    Element,
}

impl InlineFlow {
    pub(super) fn new(
        id: impl Into<ElementId>,
        path: Rc<[u32]>,
        view: Entity<MarkdownState>,
        items: Vec<InlineFlowItem>,
    ) -> Self {
        Self {
            id: id.into(),
            path,
            view,
            items,
        }
    }

    fn image_element(
        ix: usize,
        view: Entity<MarkdownState>,
        url: &SharedUri,
        link: &Option<LinkMark>,
        title: &str,
        size: Size<Pixels>,
        cx: &App,
    ) -> AnyElement {
        let source = view.read(cx).image_source(url);
        let opened = view.read(cx).opened_image_source(url);
        let link = link.clone();
        let title = title.to_string();
        let tooltip_title = title.clone();
        let context_view = view.clone();
        let context_url = url.clone();
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
        crate::material::accessible_clickable(img(source), ix, role, label, cx)
            .object_fit(ObjectFit::Contain)
            .with_fallback(crate::image_viewer::unpreviewed(false, true, cx))
            .max_w(relative(1.))
            .w(size.width)
            .h(size.height)
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
                    crate::image_viewer::open(opened.clone(), title.clone(), window, cx);
                }
            })
            .into_any_element()
    }
}

impl IntoElement for InlineFlow {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InlineFlow {
    type RequestLayoutState = InlineFlowLayoutState;
    type PrepaintState = Vec<AnyElement>;

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
        let measure_items = self.items.iter().map(MeasureItem::from).collect::<Vec<_>>();
        let line_height = window.line_height();
        let element_sizes = self
            .items
            .iter()
            .enumerate()
            .map(|(ix, item)| match item {
                InlineFlowItem::Image { url, .. } => Some(inline_image_size_for_line(
                    intrinsic_image_size(ix, url, &self.view, window, cx),
                    line_height,
                )),
                InlineFlowItem::ImageLink { url, label } => {
                    let mut badge = super::image_link::badge(
                        ix,
                        self.view.clone(),
                        url.clone(),
                        label.clone(),
                        window,
                        cx,
                    )
                    .into_any_element();
                    Some(badge.layout_as_root(AvailableSpace::min_size(), window, cx))
                }
                InlineFlowItem::Math {
                    math,
                    state,
                    base,
                    path,
                } => {
                    let origin = RunOrigin::new(self.path.clone()).offset(*base);
                    let mut element =
                        super::math::inline(math, state, path, origin, &self.view, window, cx);
                    Some(element.layout_as_root(AvailableSpace::min_size(), window, cx))
                }
                InlineFlowItem::Text { .. } => None,
            })
            .collect::<Vec<_>>();
        let state = InlineFlowLayoutState::default();
        let layout_ref = state.layout.clone();
        let layout_id = window.request_measured_layout(Default::default(), {
            move |known_dimensions, available_space, window, _| {
                let text_style = window.text_style();
                let wrap_width = if text_style.white_space == WhiteSpace::Normal {
                    known_dimensions.width.or(match available_space.width {
                        AvailableSpace::Definite(width) => Some(width),
                        _ => None,
                    })
                } else {
                    None
                };
                let element_sizes = element_sizes
                    .iter()
                    .enumerate()
                    .map(|(ix, measured)| {
                        let mut measured = *measured;
                        if matches!(measure_items[ix], MeasureItem::ImageLink)
                            && let (Some(size), Some(width)) = (&mut measured, wrap_width)
                        {
                            size.width = size.width.min(width);
                        }
                        measured
                    })
                    .collect::<Vec<_>>();
                let layout = layout_flow(
                    &measure_items,
                    &element_sizes,
                    &text_style,
                    wrap_width,
                    window,
                );
                let size = layout.size;
                if let Ok(mut slot) = layout_ref.lock() {
                    *slot = Some(layout);
                }
                size
            }
        });
        (layout_id, state)
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
        let fragments = request_layout
            .layout
            .lock()
            .ok()
            .and_then(|layout| layout.as_ref().map(|layout| layout.fragments.clone()))
            .unwrap_or_default();
        let mut elements = Vec::with_capacity(fragments.len());
        for fragment in fragments {
            match fragment {
                PositionedFragment::Text {
                    item_ix,
                    origin,
                    size: fragment_size,
                    source_range,
                    text,
                    links,
                    highlights,
                    font_overrides,
                    code_style,
                } => {
                    let (state, base) = match &self.items[item_ix] {
                        InlineFlowItem::Text {
                            state,
                            base,
                            text: source,
                            ..
                        } if source_range == (0..source.len()) => (state.clone(), *base),
                        InlineFlowItem::Text { base, .. } => {
                            (Arc::new(Mutex::new(InlineState::default())), *base)
                        }
                        _ => (Arc::new(Mutex::new(InlineState::default())), 0),
                    };
                    if let Ok(mut state) = state.lock() {
                        state.set_text(text);
                    }
                    let inline = Inline::new(
                        elements.len(),
                        self.view.clone(),
                        state,
                        links,
                        highlights,
                        font_overrides,
                    )
                    .origin(Some(
                        RunOrigin::new(self.path.clone()).offset(base + source_range.start),
                    ));
                    let mut element = match code_style {
                        Some(code) => {
                            let line_height = code.line_height();
                            div()
                                .flex_none()
                                .px(INLINE_CODE_PADDING_X)
                                .py(INLINE_CODE_PADDING_Y)
                                .rounded(code.radius)
                                .bg(code.background)
                                .font_family(code.font_family)
                                .text_size(code.font_size)
                                .line_height(line_height)
                                .child(inline)
                                .into_any_element()
                        }
                        None => inline.into_any_element(),
                    };
                    element.prepaint_as_root(
                        bounds.origin + origin,
                        size(
                            AvailableSpace::Definite(fragment_size.width),
                            AvailableSpace::Definite(fragment_size.height),
                        ),
                        window,
                        cx,
                    );
                    elements.push(element);
                }
                PositionedFragment::Element {
                    item_ix,
                    origin,
                    size: fragment_size,
                } => {
                    let mut element = match &self.items[item_ix] {
                        InlineFlowItem::Image { url, link, title } => Self::image_element(
                            elements.len(),
                            self.view.clone(),
                            url,
                            link,
                            title,
                            fragment_size,
                            cx,
                        ),
                        InlineFlowItem::ImageLink { url, label } => super::image_link::badge(
                            item_ix,
                            self.view.clone(),
                            url.clone(),
                            label.clone(),
                            window,
                            cx,
                        )
                        .w(fragment_size.width)
                        .h(fragment_size.height)
                        .into_any_element(),
                        InlineFlowItem::Math {
                            math,
                            state,
                            base,
                            path,
                        } => {
                            let origin = RunOrigin::new(self.path.clone()).offset(*base);
                            super::math::inline(math, state, path, origin, &self.view, window, cx)
                        }
                        InlineFlowItem::Text { .. } => unreachable!(),
                    };
                    element.prepaint_as_root(
                        bounds.origin + origin,
                        size(
                            AvailableSpace::Definite(fragment_size.width),
                            AvailableSpace::Definite(fragment_size.height),
                        ),
                        window,
                        cx,
                    );
                    elements.push(element);
                }
            }
        }
        elements
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        elements: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for element in elements {
            element.paint(window, cx);
        }
    }
}

impl From<&InlineFlowItem> for MeasureItem {
    fn from(item: &InlineFlowItem) -> Self {
        match item {
            InlineFlowItem::Text {
                text,
                links,
                highlights,
                font_overrides,
                code_style,
                ..
            } => Self::Text {
                // The flow places every row itself, so a break inside the paragraph is a
                // space of the same byte length: the wrapper, the shaper (which panics on a
                // break) and the painted fragment (which would start a row of its own) all
                // see one row, and ranges keep their offsets.
                text: if text.contains(['\n', '\r']) {
                    text.replace(['\n', '\r'], " ").into()
                } else {
                    text.clone()
                },
                links: links.clone(),
                highlights: highlights.clone(),
                font_overrides: font_overrides.clone(),
                code_style: code_style.clone(),
            },
            InlineFlowItem::Math { .. } | InlineFlowItem::Image { .. } => Self::Image,
            InlineFlowItem::ImageLink { .. } => Self::ImageLink,
        }
    }
}

impl MeasureItem {
    fn len(&self) -> usize {
        match self {
            Self::Text { text, .. } => text.len(),
            Self::Image | Self::ImageLink => ELEMENT_LEN,
        }
    }
}

fn layout_flow(
    items: &[MeasureItem],
    element_sizes: &[Option<Size<Pixels>>],
    text_style: &TextStyle,
    wrap_width: Option<Pixels>,
    window: &mut Window,
) -> InlineFlowLayout {
    let line_height = window.line_height();
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    let total_len = items.iter().map(MeasureItem::len).sum::<usize>();
    if total_len == 0 {
        return InlineFlowLayout::default();
    }
    let mut fragments = Vec::new();
    let mut max_width = Pixels::ZERO;
    let mut y = Pixels::ZERO;
    for line_range in line_ranges(items, element_sizes, text_style, wrap_width, window) {
        let mut line_fragments = Vec::new();
        let mut line_width = Pixels::ZERO;
        let mut actual_line_height = line_height;
        let mut item_start = 0;
        for (item_ix, item) in items.iter().enumerate() {
            let item_end = item_start + item.len();
            if item_end <= line_range.start {
                item_start = item_end;
                continue;
            }
            if item_start >= line_range.end {
                break;
            }
            match item {
                MeasureItem::Text {
                    text,
                    links,
                    highlights,
                    font_overrides,
                    code_style,
                } => {
                    let local_start = line_range.start.max(item_start) - item_start;
                    let local_end = line_range.end.min(item_end) - item_start;
                    if local_start < local_end {
                        let subtext: SharedString = text[local_start..local_end].to_string().into();
                        let highlights =
                            slice_ranges(highlights, local_start, local_end, |range, style| {
                                (range, *style)
                            });
                        let links = slice_ranges(links, local_start, local_end, |range, link| {
                            (range, link.clone())
                        });
                        let font_overrides = slice_ranges(
                            font_overrides,
                            local_start,
                            local_end,
                            |range, family| (range, family.clone()),
                        );
                        let (fragment_style, fragment_font_size, padding, height) = code_style
                            .as_ref()
                            .map(|code| {
                                let mut text_style = text_style.clone();
                                text_style.font_family = code.font_family.clone();
                                text_style.font_size = AbsoluteLength::Pixels(code.font_size);
                                (
                                    text_style,
                                    code.font_size,
                                    INLINE_CODE_PADDING_X * 2.,
                                    code.line_height() + INLINE_CODE_PADDING_Y * 2.,
                                )
                            })
                            .unwrap_or_else(|| {
                                (text_style.clone(), font_size, Pixels::ZERO, line_height)
                            });
                        let runs = runs_for_highlights(&subtext, &fragment_style, &highlights);
                        let shaped = window.text_system().shape_line(
                            subtext.clone(),
                            fragment_font_size,
                            &runs,
                            None,
                        );
                        let width = shaped.width() + padding;
                        line_width += width;
                        actual_line_height = actual_line_height.max(height);
                        line_fragments.push(LineFragmentLayout {
                            item_ix,
                            kind: LineFragmentKind::Text {
                                text: subtext,
                                links,
                                highlights,
                                font_overrides,
                                code_style: code_style.clone(),
                            },
                            size: size(width, height),
                            source_range: local_start..local_end,
                        });
                    }
                }
                MeasureItem::Image | MeasureItem::ImageLink => {
                    if line_range.start <= item_start && item_end <= line_range.end {
                        let image_size =
                            element_sizes[item_ix].unwrap_or(size(line_height, line_height));
                        line_width += image_size.width;
                        actual_line_height = actual_line_height.max(image_size.height);
                        line_fragments.push(LineFragmentLayout {
                            item_ix,
                            kind: LineFragmentKind::Element,
                            size: image_size,
                            source_range: 0..ELEMENT_LEN,
                        });
                    }
                }
            }
            item_start = item_end;
        }
        let mut x = Pixels::ZERO;
        for fragment in line_fragments {
            let origin = point(x, y + (actual_line_height - fragment.size.height) / 2.);
            let positioned = match fragment.kind {
                LineFragmentKind::Text {
                    text,
                    links,
                    highlights,
                    font_overrides,
                    code_style,
                } => PositionedFragment::Text {
                    item_ix: fragment.item_ix,
                    origin,
                    size: fragment.size,
                    source_range: fragment.source_range,
                    text,
                    links,
                    highlights,
                    font_overrides,
                    code_style,
                },
                LineFragmentKind::Element => PositionedFragment::Element {
                    item_ix: fragment.item_ix,
                    origin,
                    size: fragment.size,
                },
            };
            x += fragment.size.width;
            fragments.push(positioned);
        }
        max_width = max_width.max(line_width);
        y += actual_line_height;
    }
    InlineFlowLayout {
        fragments,
        size: size(max_width, y),
    }
}

fn line_ranges(
    items: &[MeasureItem],
    element_sizes: &[Option<Size<Pixels>>],
    text_style: &TextStyle,
    wrap_width: Option<Pixels>,
    window: &mut Window,
) -> Vec<Range<usize>> {
    let total_len = items.iter().map(MeasureItem::len).sum::<usize>();
    let Some(wrap_width) = wrap_width else {
        return std::iter::once(0..total_len).collect();
    };
    let fragments = items
        .iter()
        .enumerate()
        .map(|(ix, item)| match item {
            MeasureItem::Text {
                text,
                highlights,
                code_style: Some(code),
                ..
            } => {
                let mut code_text_style = text_style.clone();
                code_text_style.font_family = code.font_family.clone();
                code_text_style.font_size = AbsoluteLength::Pixels(code.font_size);
                let runs = runs_for_highlights(text, &code_text_style, highlights);
                let width = window
                    .text_system()
                    .shape_line(text.clone(), code.font_size, &runs, None)
                    .width()
                    + INLINE_CODE_PADDING_X * 2.;
                WrapLineFragment::element(width, text.len())
            }
            MeasureItem::Text { text, .. } => WrapLineFragment::text(text),
            MeasureItem::Image | MeasureItem::ImageLink => {
                WrapLineFragment::element(element_sizes[ix].unwrap_or_default().width, ELEMENT_LEN)
            }
        })
        .collect::<Vec<_>>();
    let font_size = text_style.font_size.to_pixels(window.rem_size());
    let boundaries = window
        .text_system()
        .line_wrapper(text_style.font(), font_size)
        .wrap_line(&fragments, wrap_width, Default::default())
        .map(|boundary| boundary.ix.min(total_len))
        .collect::<Vec<_>>();
    let mut ranges = Vec::with_capacity(boundaries.len() + 1);
    let mut start = 0;
    for end in boundaries {
        if start < end {
            ranges.push(start..end);
        }
        start = end;
    }
    if start < total_len {
        ranges.push(start..total_len);
    }
    ranges
}

fn intrinsic_image_size(
    ix: usize,
    url: &SharedUri,
    view: &Entity<MarkdownState>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Size<Pixels>> {
    let mut image = img(view.read(cx).image_source(url))
        .id(ix)
        .object_fit(ObjectFit::Contain)
        .max_w(relative(1.))
        .into_any_element();
    let measured = image.layout_as_root(AvailableSpace::min_size(), window, cx);
    (measured.width > Pixels::ZERO && measured.height > Pixels::ZERO).then_some(measured)
}

fn inline_image_size_for_line(
    intrinsic: Option<Size<Pixels>>,
    line_height: Pixels,
) -> Size<Pixels> {
    let height = line_height * 0.75;
    let ratio = intrinsic
        .filter(|size| size.width > Pixels::ZERO && size.height > Pixels::ZERO)
        .map(|size| size.width / size.height)
        .unwrap_or(1.);
    size((height * ratio).max(px(1.)), height.max(px(1.)))
}

fn runs_for_highlights(
    text: &str,
    style: &TextStyle,
    highlights: &[(Range<usize>, HighlightStyle)],
) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut ix = 0;
    for (range, highlight) in highlights {
        if ix < range.start {
            runs.push(style.clone().to_run(range.start - ix));
        }
        runs.push(style.clone().highlight(*highlight).to_run(range.len()));
        ix = range.end;
    }
    if ix < text.len() {
        runs.push(style.to_run(text.len() - ix));
    }
    runs
}

fn slice_ranges<T, U>(
    ranges: &[(Range<usize>, T)],
    start: usize,
    end: usize,
    map: impl Fn(Range<usize>, &T) -> U,
) -> Vec<U> {
    ranges
        .iter()
        .filter_map(|(range, value)| {
            let clipped_start = range.start.max(start);
            let clipped_end = range.end.min(end);
            (clipped_start < clipped_end)
                .then(|| map((clipped_start - start)..(clipped_end - start), value))
        })
        .collect()
}
