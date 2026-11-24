//! Project a host [`TermSnapshot`] into the replicated wire types.
//!
//! This is the only place rio's grid representation is translated, so the
//! runtime's live projection and the chat command panel's local rendering
//! stay one implementation.

use std::collections::HashMap;

use rio_vt::{
    ansi::CursorShape as RioCursorShape,
    config::colors::{AnsiColor, ColorRgb, NamedColor},
    crosswords::{
        grid::row::Row,
        square::{ContentTag, Square, Wide},
        style::Style,
    },
};
use tcode_protocol::terminal::{
    CellWidth, TerminalCell, TerminalColor, TerminalCursor, TerminalLink, TerminalModes,
    TerminalRow, TerminalStyle,
};

use crate::TermSnapshot;

/// Interns cell styles into one table per wire message.
#[derive(Debug)]
pub struct Projector {
    styles: Vec<TerminalStyle>,
    index: HashMap<TerminalStyle, u16>,
}

impl Default for Projector {
    fn default() -> Self {
        let default = TerminalStyle::default();
        Self {
            styles: vec![default],
            index: HashMap::from([(default, 0)]),
        }
    }
}

impl Projector {
    /// Project one visible row.
    pub fn visible_row(&mut self, snapshot: &TermSnapshot, row: usize) -> TerminalRow {
        match snapshot.visible_rows.get(row) {
            Some(row) => self.row(snapshot, row),
            None => TerminalRow::default(),
        }
    }

    /// Project one retained scrollback row (oldest first).
    pub fn history_row(&mut self, snapshot: &TermSnapshot, row: usize) -> TerminalRow {
        match snapshot.history_rows.get(row) {
            Some(row) => self.row(snapshot, row),
            None => TerminalRow::default(),
        }
    }

    /// Project every retained scrollback row carried by `snapshot`.
    pub fn history(&mut self, snapshot: &TermSnapshot) -> Vec<TerminalRow> {
        (0..snapshot.history_rows.len())
            .map(|row| self.history_row(snapshot, row))
            .collect()
    }

    pub fn styles(&self) -> &[TerminalStyle] {
        &self.styles
    }

    pub fn into_styles(self) -> Vec<TerminalStyle> {
        self.styles
    }

    fn row(&mut self, snapshot: &TermSnapshot, row: &Row<Square>) -> TerminalRow {
        let wrapped = row
            .inner
            .get(snapshot.cols.saturating_sub(1))
            .is_some_and(|square| square.wrapline());
        let mut cells = Vec::with_capacity(snapshot.cols);
        for square in row.inner.iter().take(snapshot.cols) {
            cells.push(self.cell(snapshot, square));
        }
        // Trailing defaults are implied; a reader pads back to `cols`.
        while cells
            .last()
            .is_some_and(|cell| *cell == TerminalCell::default())
        {
            cells.pop();
        }
        TerminalRow { cells, wrapped }
    }

    fn cell(&mut self, snapshot: &TermSnapshot, square: &Square) -> TerminalCell {
        let style = self.intern(wire_style(style_of(square, &snapshot.styles)));
        TerminalCell {
            text: cell_text(square, &snapshot.zero_width),
            width: match square.wide() {
                Wide::Wide => CellWidth::Wide,
                Wide::Spacer => CellWidth::Spacer,
                // A leading spacer is end-of-line padding, not half of a wide
                // character: it paints and hit-tests as an ordinary blank.
                Wide::Narrow | Wide::LeadingSpacer => CellWidth::Narrow,
            },
            style,
            link: link_of(square, &snapshot.links),
        }
    }

    fn intern(&mut self, style: TerminalStyle) -> u16 {
        if let Some(index) = self.index.get(&style) {
            return *index;
        }
        // rio's own style table is u16-indexed, so this cannot realistically
        // overflow; a saturating fallback would silently mis-style instead.
        let index = u16::try_from(self.styles.len()).unwrap_or(u16::MAX);
        self.styles.push(style);
        self.index.insert(style, index);
        index
    }
}

/// The cursor position within the host's visible screen.
pub fn cursor(snapshot: &TermSnapshot) -> Option<TerminalCursor> {
    let (row, col) = snapshot.cursor?;
    Some(TerminalCursor {
        row: row.min(u16::MAX as usize) as u16,
        col: col.min(u16::MAX as usize) as u16,
        shape: match snapshot.cursor_state.content {
            RioCursorShape::Block => tcode_protocol::terminal::CursorShape::Block,
            RioCursorShape::Underline => tcode_protocol::terminal::CursorShape::Underline,
            RioCursorShape::Beam => tcode_protocol::terminal::CursorShape::Beam,
            RioCursorShape::Hidden => tcode_protocol::terminal::CursorShape::Hidden,
        },
        blinking: snapshot.cursor_blinking,
    })
}

pub fn modes(snapshot: &TermSnapshot, modify_other_keys: Option<u8>) -> TerminalModes {
    TerminalModes {
        mode: snapshot.mode.bits(),
        keyboard: snapshot.keyboard_mode.bits(),
        modify_other_keys,
    }
}

fn style_of(square: &Square, styles: &[Style]) -> Style {
    match square.content_tag() {
        ContentTag::Codepoint => styles
            .get(square.style_id() as usize)
            .copied()
            .unwrap_or_default(),
        ContentTag::BgPalette => Style {
            bg: AnsiColor::Indexed(square.bg_palette_index()),
            ..Style::default()
        },
        ContentTag::BgRgb => {
            let (r, g, b) = square.bg_rgb();
            Style {
                bg: AnsiColor::Spec(ColorRgb { r, g, b }),
                ..Style::default()
            }
        }
    }
}

fn wire_style(style: Style) -> TerminalStyle {
    TerminalStyle {
        fg: color(style.fg),
        bg: color(style.bg),
        underline_color: style.underline_color.map(color),
        flags: style.flags.bits(),
    }
}

fn color(color: AnsiColor) -> TerminalColor {
    match color {
        AnsiColor::Named(
            NamedColor::Foreground | NamedColor::LightForeground | NamedColor::DimForeground,
        ) => TerminalColor::Foreground,
        AnsiColor::Named(NamedColor::Background) => TerminalColor::Background,
        AnsiColor::Named(NamedColor::Cursor) => TerminalColor::Cursor,
        AnsiColor::Named(NamedColor::DimBlack) => TerminalColor::Indexed(0),
        AnsiColor::Named(NamedColor::DimRed) => TerminalColor::Indexed(1),
        AnsiColor::Named(NamedColor::DimGreen) => TerminalColor::Indexed(2),
        AnsiColor::Named(NamedColor::DimYellow) => TerminalColor::Indexed(3),
        AnsiColor::Named(NamedColor::DimBlue) => TerminalColor::Indexed(4),
        AnsiColor::Named(NamedColor::DimMagenta) => TerminalColor::Indexed(5),
        AnsiColor::Named(NamedColor::DimCyan) => TerminalColor::Indexed(6),
        AnsiColor::Named(NamedColor::DimWhite) => TerminalColor::Indexed(7),
        AnsiColor::Named(named) => TerminalColor::Indexed(named as u8),
        AnsiColor::Spec(color) => TerminalColor::Rgb {
            r: color.r,
            g: color.g,
            b: color.b,
        },
        AnsiColor::Indexed(index) => TerminalColor::Indexed(index),
    }
}

fn cell_text(square: &Square, zero_width: &HashMap<u16, Vec<char>>) -> String {
    if square.content_tag() != ContentTag::Codepoint {
        return String::new();
    }
    let base = square.c();
    let extras = square
        .extras_id()
        .and_then(|id| zero_width.get(&id))
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    // A blank cell carries no text; the renderer paints its background only.
    if extras.is_empty() && matches!(base, '\0' | ' ') {
        return String::new();
    }
    let mut text = String::new();
    text.push(if base == '\0' { ' ' } else { base });
    text.extend(
        extras
            .iter()
            .copied()
            .map(|ch| if ch == '\0' { ' ' } else { ch }),
    );
    text
}

fn link_of(square: &Square, links: &HashMap<u16, (String, String)>) -> Option<TerminalLink> {
    if !square.has_hyperlink() {
        return None;
    }
    let (uri, id) = links.get(&square.extras_id()?)?;
    Some(TerminalLink {
        uri: uri.clone(),
        id: (!id.is_empty()).then(|| id.clone()),
    })
}

#[cfg(test)]
mod tests {
    use rio_vt::{ansi::KeyboardModes, crosswords::Mode, crosswords::style::StyleFlags};
    use tcode_protocol::terminal::{CellFlags, KeyboardModes as WireKeyboard, TerminalMode};

    /// The wire carries rio's raw bits, so the two definitions must not drift.
    /// Every flag a client acts on is listed by name here, not derived.
    #[test]
    fn projected_mode_keyboard_and_style_bits_match_the_wire() {
        for (rio, wire) in [
            (Mode::SHOW_CURSOR, TerminalMode::SHOW_CURSOR),
            (Mode::APP_CURSOR, TerminalMode::APP_CURSOR),
            (Mode::APP_KEYPAD, TerminalMode::APP_KEYPAD),
            (Mode::MOUSE_REPORT_CLICK, TerminalMode::MOUSE_REPORT_CLICK),
            (Mode::BRACKETED_PASTE, TerminalMode::BRACKETED_PASTE),
            (Mode::SGR_MOUSE, TerminalMode::SGR_MOUSE),
            (Mode::MOUSE_MOTION, TerminalMode::MOUSE_MOTION),
            (Mode::LINE_WRAP, TerminalMode::LINE_WRAP),
            (Mode::LINE_FEED_NEW_LINE, TerminalMode::LINE_FEED_NEW_LINE),
            (Mode::ORIGIN, TerminalMode::ORIGIN),
            (Mode::INSERT, TerminalMode::INSERT),
            (Mode::FOCUS_IN_OUT, TerminalMode::FOCUS_IN_OUT),
            (Mode::ALT_SCREEN, TerminalMode::ALT_SCREEN),
            (Mode::MOUSE_DRAG, TerminalMode::MOUSE_DRAG),
            (Mode::UTF8_MOUSE, TerminalMode::UTF8_MOUSE),
            (Mode::ALTERNATE_SCROLL, TerminalMode::ALTERNATE_SCROLL),
            (Mode::VI, TerminalMode::VI),
            (Mode::URGENCY_HINTS, TerminalMode::URGENCY_HINTS),
            (
                Mode::DISAMBIGUATE_ESC_CODES,
                TerminalMode::DISAMBIGUATE_ESC_CODES,
            ),
            (Mode::REPORT_EVENT_TYPES, TerminalMode::REPORT_EVENT_TYPES),
            (
                Mode::REPORT_ALTERNATE_KEYS,
                TerminalMode::REPORT_ALTERNATE_KEYS,
            ),
            (
                Mode::REPORT_ALL_KEYS_AS_ESC,
                TerminalMode::REPORT_ALL_KEYS_AS_ESC,
            ),
            (
                Mode::REPORT_ASSOCIATED_TEXT,
                TerminalMode::REPORT_ASSOCIATED_TEXT,
            ),
            (Mode::MOUSE_REPORT_X10, TerminalMode::MOUSE_REPORT_X10),
            (Mode::GRAPHEME_CLUSTER, TerminalMode::GRAPHEME_CLUSTER),
            (Mode::MOUSE_MODE, TerminalMode::MOUSE_MODE),
        ] {
            assert_eq!(rio.bits(), wire.bits(), "{rio:?} moved");
        }
        for (rio, wire) in [
            (
                KeyboardModes::DISAMBIGUATE_ESC_CODES,
                WireKeyboard::DISAMBIGUATE_ESC_CODES,
            ),
            (
                KeyboardModes::REPORT_EVENT_TYPES,
                WireKeyboard::REPORT_EVENT_TYPES,
            ),
            (
                KeyboardModes::REPORT_ALTERNATE_KEYS,
                WireKeyboard::REPORT_ALTERNATE_KEYS,
            ),
            (
                KeyboardModes::REPORT_ALL_KEYS_AS_ESC,
                WireKeyboard::REPORT_ALL_KEYS_AS_ESC,
            ),
            (
                KeyboardModes::REPORT_ASSOCIATED_TEXT,
                WireKeyboard::REPORT_ASSOCIATED_TEXT,
            ),
        ] {
            assert_eq!(rio.bits(), wire.bits(), "{rio:?} moved");
        }
        assert_eq!(KeyboardModes::NO_MODE.bits(), WireKeyboard::NO_MODE.bits());

        for (rio, wire) in [
            (StyleFlags::INVERSE, CellFlags::INVERSE),
            (StyleFlags::BOLD, CellFlags::BOLD),
            (StyleFlags::ITALIC, CellFlags::ITALIC),
            (StyleFlags::DIM, CellFlags::DIM),
            (StyleFlags::HIDDEN, CellFlags::HIDDEN),
            (StyleFlags::STRIKEOUT, CellFlags::STRIKEOUT),
            (StyleFlags::UNDERLINE, CellFlags::UNDERLINE),
            (StyleFlags::DOUBLE_UNDERLINE, CellFlags::DOUBLE_UNDERLINE),
            (StyleFlags::UNDERCURL, CellFlags::UNDERCURL),
            (StyleFlags::DOTTED_UNDERLINE, CellFlags::DOTTED_UNDERLINE),
            (StyleFlags::DASHED_UNDERLINE, CellFlags::DASHED_UNDERLINE),
            (StyleFlags::ALL_UNDERLINES, CellFlags::ALL_UNDERLINES),
        ] {
            assert_eq!(rio.bits(), wire.bits(), "{rio:?} moved");
        }
    }

    #[test]
    fn projects_wide_cells_spacers_combining_marks_and_osc8_links() {
        let emulator = crate::GridEmulator::with_size(20, 3);
        emulator.feed(
            "中e\u{301}\x1b]8;;https://example.com/target\x1b\\link\x1b]8;;\x1b\\".as_bytes(),
        );
        let snapshot = emulator.snapshot();
        let mut projector = super::Projector::default();
        let row = projector.visible_row(&snapshot, 0);

        assert_eq!(row.cells[0].text, "中");
        assert_eq!(
            row.cells[0].width,
            tcode_protocol::terminal::CellWidth::Wide
        );
        assert_eq!(row.cells[1].text, "");
        assert_eq!(
            row.cells[1].width,
            tcode_protocol::terminal::CellWidth::Spacer
        );
        assert_eq!(row.cells[2].text, "e\u{301}");
        assert_eq!(
            row.cells[3].link.as_ref().map(|link| link.uri.as_str()),
            Some("https://example.com/target")
        );
        // Trailing blanks are implied rather than transmitted, so the row ends
        // at the last written cell.
        assert_eq!(row.cells.len(), 7);
        assert_eq!(row.cells[6].text, "k");
    }
}
