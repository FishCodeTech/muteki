use super::{
    inline::{Inline, InlineState},
    selection::RunOrigin,
    state::MarkdownState,
};
#[cfg(test)]
use gpui::InteractiveElement as _;
use gpui::{
    AnyElement, App, Entity, GlyphId, Hsla, IntoElement, ParentElement as _, Styled as _, div, px,
};
use latex_rust::{BoxContent, Dim, MathBox};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default, Clone)]
pub(super) struct FormulaSelection(Arc<Mutex<SelectionData>>);
#[derive(Debug, Default)]
struct SelectionData {
    active: bool,
    glyphs: Vec<Arc<Mutex<InlineState>>>,
}
impl FormulaSelection {
    pub(super) fn set_active(&self, active: bool) {
        self.0.lock().unwrap().active = active;
    }
    pub(super) fn text(&self) -> Option<String> {
        let data = self.0.lock().unwrap();
        data.active.then(|| {
            data.glyphs
                .iter()
                .map(|state| state.lock().unwrap().text.to_string())
                .collect()
        })
    }
}

pub(super) fn em(dim: &Dim) -> f32 {
    f32::from_bits(dim.to_ieee32_bits())
}

/// Lay out a formula; `origin` is where its glyphs' text starts in the
/// document, when it is selectable.
pub(super) fn render(
    tree: &MathBox,
    selection: &FormulaSelection,
    origin: Option<RunOrigin>,
    view: &Entity<MarkdownState>,
    cx: &App,
) -> AnyElement {
    let mut glyphs = Vec::new();
    let mut elements = Vec::new();
    let mut data = selection.0.lock().unwrap();
    data.active = true;
    let color = crate::theme::ActiveTheme::theme(cx).foreground;
    emit(
        tree,
        0.,
        em(&tree.height),
        color,
        &data.glyphs,
        &mut glyphs,
        &mut elements,
        (&origin, &mut 0),
        view,
    );
    data.glyphs = glyphs;
    div()
        .relative()
        .flex_none()
        .w(px(em(&tree.width) * 16.))
        .h(px(em(&(&tree.height + &tree.depth)) * 16.))
        .children(elements)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn emit(
    bx: &MathBox,
    x: f32,
    parent_baseline: f32,
    color: Hsla,
    previous: &[Arc<Mutex<InlineState>>],
    glyphs: &mut Vec<Arc<Mutex<InlineState>>>,
    elements: &mut Vec<AnyElement>,
    (origin, offset): (&Option<RunOrigin>, &mut usize),
    view: &Entity<MarkdownState>,
) {
    let baseline = parent_baseline - em(&bx.shift);
    let width = em(&bx.width) * 16.;
    let height = em(&(&bx.height + &bx.depth)) * 16.;
    let top = (baseline - em(&bx.height)) * 16.;
    let rect = |color: Hsla| {
        div()
            .absolute()
            .left(px(x * 16.))
            .top(px(top))
            .w(px(width))
            .h(px(height))
            .bg(color)
    };
    match &bx.content {
        BoxContent::Empty | BoxContent::Kern(_) => {}
        BoxContent::Glyph {
            ch,
            glyph_id,
            scale,
            ..
        } => {
            let ix = glyphs.len();
            let text = ch.to_string();
            let text_len = text.len();
            let state = previous
                .get(ix)
                .filter(|state| state.lock().unwrap().text.as_ref() == text)
                .cloned()
                .unwrap_or_else(|| InlineState::shared(text.into()));
            let font_size = px(em(scale) * 16.);
            let glyph = Inline::new(
                ix,
                view.clone(),
                state.clone(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
            .origin(origin.clone().map(|origin| origin.offset(*offset)))
            .math_glyph(GlyphId(*glyph_id as u32), px(em(&bx.height) * 16.));
            *offset += text_len;
            let element = div()
                .absolute()
                .left(px(x * 16.))
                .top(px(top))
                .w(px(width))
                .h(px(height))
                .font_family(latex_rust::STIX_TWO_MATH_NAME)
                .text_size(font_size)
                .line_height(px(height))
                .text_color(color)
                .child(glyph);
            #[cfg(test)]
            let element = element.debug_selector(move || format!("math-glyph-{ix}"));
            elements.push(element.into_any_element());
            glyphs.push(state);
        }
        BoxContent::Rule => elements.push(rect(color).into_any_element()),
        BoxContent::HList(children) => {
            let mut x = x;
            for child in children {
                emit(
                    child,
                    x,
                    baseline,
                    color,
                    previous,
                    glyphs,
                    elements,
                    (origin, &mut *offset),
                    view,
                );
                x += em(&child.width);
            }
        }
        BoxContent::VList(children) => {
            if let Some(first) = children.first() {
                emit(
                    first,
                    x,
                    baseline,
                    color,
                    previous,
                    glyphs,
                    elements,
                    (origin, &mut *offset),
                    view,
                );
                let mut below = em(&first.depth);
                for child in &children[1..] {
                    emit(
                        child,
                        x,
                        baseline + below + em(&child.height),
                        color,
                        previous,
                        glyphs,
                        elements,
                        (origin, &mut *offset),
                        view,
                    );
                    below += em(&(&child.height + &child.depth));
                }
            }
        }
        BoxContent::Overlap(children) => {
            for child in children {
                emit(
                    child,
                    x,
                    baseline,
                    color,
                    previous,
                    glyphs,
                    elements,
                    (origin, &mut *offset),
                    view,
                );
            }
        }
        BoxContent::Color(c, child) => emit(
            child,
            x,
            baseline,
            to_color(*c),
            previous,
            glyphs,
            elements,
            (origin, &mut *offset),
            view,
        ),
        BoxContent::BackColor(c, child) => {
            elements.push(rect(to_color(*c)).into_any_element());
            emit(
                child,
                x,
                baseline,
                color,
                previous,
                glyphs,
                elements,
                (origin, &mut *offset),
                view,
            );
        }
        BoxContent::Frame {
            thickness,
            stroke,
            inner,
        } => {
            elements.push(
                div()
                    .absolute()
                    .left(px(x * 16.))
                    .top(px(top))
                    .w(px(width))
                    .h(px(height))
                    .border(px(em(thickness) * 16.))
                    .border_color(stroke.map(to_color).unwrap_or(color))
                    .into_any_element(),
            );
            emit(
                inner,
                x,
                baseline,
                color,
                previous,
                glyphs,
                elements,
                (origin, &mut *offset),
                view,
            );
        }
        BoxContent::Line {
            x1,
            y1,
            x2,
            y2,
            thickness,
        } => {
            let (from, to, t) = (
                gpui::point(px((x + em(x1)) * 16.), px((baseline - em(y1)) * 16.)),
                gpui::point(px((x + em(x2)) * 16.), px((baseline - em(y2)) * 16.)),
                px(em(thickness) * 16.),
            );
            elements.push(
                gpui::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let mut path = gpui::PathBuilder::stroke(t);
                        path.move_to(bounds.origin + from);
                        path.line_to(bounds.origin + to);
                        if let Ok(path) = path.build() {
                            window.paint_path(path, color);
                        }
                    },
                )
                .absolute()
                .size_full()
                .into_any_element(),
            );
        }
    }
}
fn to_color(color: latex_rust::Color) -> Hsla {
    let [r, g, b, a] = color.to_rgba8();
    gpui::rgba(u32::from_be_bytes([r, g, b, a])).into()
}
