//! One line of text that fades out at its end when it does not fit, instead
//! of being cut to an ellipsis.
//!
//! gpui-base has no fading truncation, and GPUI's `TextOverflow` can only
//! replace the cut text with a string. The fade is painted on the glyphs
//! rather than as a gradient laid over them, because an overlay has to match
//! whatever is behind the text: a hovered or selected row, or a translucent
//! material.

use std::ops::Range;

use gpui::{
    App, Bounds, ContentMask, Element, ElementId, GlobalElementId, HighlightStyle, Hsla,
    InspectorElementId, IntoElement, LayoutId, Overflow, Pixels, ShapedLine, SharedString, Style,
    TextAlign, TextRun, TextStyle, Window, point, px, size,
};

/// How far before the clipped edge the text starts fading, in ems.
const FADE_EMS: f32 = 2.;
/// The width of each band the fade is painted in, each band one alpha.
const FADE_BAND: Pixels = px(1.);
/// More characters than any row shows, so that an unbounded string is not
/// shaped whole on every frame.
const MAX_CHARS: usize = 1000;

/// Text laid out as one unwrapped line in the inherited style, whatever the
/// text contains, that fades out its last [`FADE_EMS`] when it is given less
/// width than the line. Like the `truncate()` it replaces, its container
/// needs `overflow_hidden()` to shrink below the line's width.
pub struct FadeText {
    text: SharedString,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
}

pub fn fade_text(text: impl Into<SharedString>) -> FadeText {
    FadeText {
        text: text.into(),
        highlights: Vec::new(),
    }
}

impl FadeText {
    /// Styles byte ranges of the text over the inherited style, as
    /// `StyledText::with_highlights` does: sorted and not overlapping.
    pub fn with_highlights(
        mut self,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Self {
        self.highlights = highlights.into_iter().collect();
        self
    }
}

pub struct FadeTextLayout {
    line: ShapedLine,
    runs: Vec<TextRun>,
    line_height: Pixels,
}

impl IntoElement for FadeText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for FadeText {
    type RequestLayoutState = FadeTextLayout;
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
        _: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = style.line_height_in_pixels(window.rem_size());
        let shown = match self.text.char_indices().nth(MAX_CHARS) {
            Some((end, _)) => &self.text[..end],
            None => &self.text[..],
        };
        // Same byte lengths, so the highlights still line up with the text.
        let text: SharedString = if shown.len() < self.text.len() || shown.contains(['\n', '\r']) {
            shown.replace(['\n', '\r'], " ").into()
        } else {
            self.text.clone()
        };
        let runs = runs(&style, text.len(), &self.highlights);
        let line = window
            .text_system()
            .shape_line(text, font_size, &runs, None);
        // Whole pixels, as GPUI measures text: layout rounds sizes to the
        // pixel grid, which would otherwise leave a fitting line a fraction
        // too wide and fade it.
        let width = line.width().ceil();
        // Hidden overflow lets a flex container shrink the line below its
        // width, as it would the container.
        let layout_style = Style {
            overflow: point(Overflow::Hidden, Overflow::Hidden),
            ..Default::default()
        };
        let layout_id = window.request_measured_layout(layout_style, move |known, _, _, _| {
            size(
                known.width.unwrap_or(width),
                known.height.unwrap_or(line_height),
            )
        });
        (
            layout_id,
            FadeTextLayout {
                line,
                runs,
                line_height,
            },
        )
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let FadeTextLayout {
            line,
            runs,
            line_height,
        } = layout;
        let line_height = *line_height;
        if line.width() <= bounds.size.width {
            let align = window.text_style().text_align;
            line.paint(
                bounds.origin,
                line_height,
                align,
                Some(bounds.size.width),
                window,
                cx,
            )
            .ok();
            return;
        }

        let fade = (line.font_size * FADE_EMS).min(bounds.size.width);
        let fade_start = bounds.right() - fade;
        let solid = Bounds::from_corners(bounds.origin, point(fade_start, bounds.bottom()));
        window.with_content_mask(Some(ContentMask { bounds: solid }), |window| {
            line.paint(
                bounds.origin,
                line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            )
            .ok();
        });

        // A glyph's ink can start up to about an em before the band it
        // reaches; the scene drops whatever falls outside each band.
        let reach = line.font_size;
        let baseline = bounds.top() + (line_height - line.ascent - line.descent) / 2. + line.ascent;
        let font_size = line.font_size;
        let fading: Vec<_> = line
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(move |glyph| (run.font_id, glyph)))
            .filter_map(|(font_id, glyph)| {
                let x = bounds.left() + glyph.position.x;
                (x + reach >= fade_start && x < bounds.right()).then(|| {
                    let origin = point(x, baseline + glyph.position.y);
                    let color = color_at(runs, glyph.index);
                    (origin, font_id, glyph.id, glyph.is_emoji, color)
                })
            })
            .collect();

        let mut left = fade_start;
        while left < bounds.right() {
            let right = (left + FADE_BAND).min(bounds.right());
            let opacity = 1. - ((left + right) / 2. - fade_start) / fade;
            let band =
                Bounds::from_corners(point(left, bounds.top()), point(right, bounds.bottom()));
            window.with_content_mask(Some(ContentMask { bounds: band }), |window| {
                for &(origin, font_id, glyph_id, is_emoji, color) in &fading {
                    if origin.x >= right || origin.x + reach < left {
                        continue;
                    }
                    // Emoji are painted in their own colors: clipped, not faded.
                    if is_emoji {
                        window
                            .paint_emoji(origin, font_id, glyph_id, font_size)
                            .ok();
                    } else {
                        window
                            .paint_glyph(
                                origin,
                                font_id,
                                glyph_id,
                                font_size,
                                color.opacity(opacity),
                            )
                            .ok();
                    }
                }
            });
            left = right;
        }
    }
}

fn runs(
    style: &TextStyle,
    len: usize,
    highlights: &[(Range<usize>, HighlightStyle)],
) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut ix = 0;
    for (range, highlight) in highlights {
        let range = range.start.min(len)..range.end.min(len);
        if range.is_empty() {
            continue;
        }
        if ix < range.start {
            runs.push(style.to_run(range.start - ix));
        }
        runs.push(style.clone().highlight(*highlight).to_run(range.len()));
        ix = range.end;
    }
    if ix < len {
        runs.push(style.to_run(len - ix));
    }
    runs
}

fn color_at(runs: &[TextRun], index: usize) -> Hsla {
    let mut end = 0;
    runs.iter()
        .find(|run| {
            end += run.len;
            index < end
        })
        .or(runs.last())
        .map_or_else(Hsla::default, |run| run.color)
}
