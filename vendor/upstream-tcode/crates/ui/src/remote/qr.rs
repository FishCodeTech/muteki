use gpui::{AnyElement, App, IntoElement as _, ParentElement as _, Styled as _, div, px};
use gpui_base::{h_flex, v_flex};

use crate::theme::ActiveTheme as _;

/// A QR code as `(width_in_modules, dark_module_flags)`, row-major.
fn qr_modules(payload: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(payload.as_bytes()).ok()?;
    let width = code.width();
    let modules = code
        .into_colors()
        .into_iter()
        .map(|color| color == qrcode::Color::Dark)
        .collect();
    Some((width, modules))
}

/// A link's QR next to the text about it. Compact stacks the QR over the
/// text rather than putting a fixed-size image beside text that then has
/// nowhere to wrap; a wide layout too narrow for both wraps the QR under it.
pub(super) fn beside_qr(
    text: impl gpui::IntoElement,
    payload: &str,
    compact: bool,
    cx: &App,
) -> gpui::Div {
    let qr = qr_element(payload, compact, cx);
    let layout = if compact {
        v_flex().items_center().children(qr).child(text)
    } else {
        h_flex()
            .flex_wrap()
            .items_start()
            // The narrowest the text column gets before the QR wraps under it;
            // a layout breakpoint, not a size any owner supplies.
            .child(div().flex_1().min_w(px(200.)).child(text))
            .children(qr)
    };
    layout.w_full().gap(crate::material::section_gap(cx))
}

/// Paint the matrix as one flex row per module row, collapsing consecutive
/// same-colour modules into a single box — a per-module element would be
/// thousands of nodes repainting every countdown tick.
///
/// A module is a whole number of points, so it stays crisp: 4 on a compact
/// layout, where the code is held up to another phone's camera, 3 on a wide
/// one, where it sits beside its text without outgrowing it.
pub(super) fn qr_element(payload: &str, compact: bool, cx: &App) -> Option<AnyElement> {
    let module: f32 = if compact { 4. } else { 3. };
    let (width, modules) = qr_modules(payload)?;
    // A QR is scanned by a camera, not read by a human: it must stay black on
    // white in both themes, so neither colour comes from the palette.
    let dark = gpui::black();
    let mut grid = v_flex().flex_none();
    for row in modules.chunks(width) {
        let mut line = h_flex().flex_none().h(px(module));
        let mut start = 0;
        while start < row.len() {
            let mut end = start + 1;
            while end < row.len() && row[end] == row[start] {
                end += 1;
            }
            // The row centers its children, so a run without an explicit
            // height would collapse to nothing and paint no modules at all.
            let run = div()
                .flex_none()
                .h(px(module))
                .w(px((end - start) as f32 * module));
            line = line.child(if row[start] { run.bg(dark) } else { run });
            start = end;
        }
        grid = grid.child(line);
    }
    Some(
        div()
            .flex_none()
            .p(px(crate::material::CARD_INSET))
            .rounded(crate::material::radius_card(cx))
            .bg(gpui::white())
            // On a light card the white would not show where the code ends.
            .border_1()
            .border_color(cx.theme().border)
            .child(grid)
            .into_any_element(),
    )
}
