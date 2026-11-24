//! The replicated terminal projection.
//!
//! The host owns the emulator; a client renders this projection and encodes
//! input from the replicated mode bits with [`mappings`]. Cells are already
//! resolved (no interned grid tables, no parser state), so a subscriber that
//! attaches at any moment reproduces the host grid exactly.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod mappings;

bitflags::bitflags! {
    /// Mirror of rio's `crosswords::Mode`. The wire carries the raw bits;
    /// `term`'s projection tests assert both definitions still agree.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TerminalMode: u32 {
        const SHOW_CURSOR             = 1;
        const APP_CURSOR              = 1 << 1;
        const APP_KEYPAD              = 1 << 2;
        const MOUSE_REPORT_CLICK      = 1 << 3;
        const BRACKETED_PASTE         = 1 << 4;
        const SGR_MOUSE               = 1 << 5;
        const MOUSE_MOTION            = 1 << 6;
        const LINE_WRAP               = 1 << 7;
        const LINE_FEED_NEW_LINE      = 1 << 8;
        const ORIGIN                  = 1 << 9;
        const INSERT                  = 1 << 10;
        const FOCUS_IN_OUT            = 1 << 11;
        const ALT_SCREEN              = 1 << 12;
        const MOUSE_DRAG              = 1 << 13;
        const UTF8_MOUSE              = 1 << 14;
        const ALTERNATE_SCROLL        = 1 << 15;
        const VI                      = 1 << 16;
        const URGENCY_HINTS           = 1 << 17;
        const DISAMBIGUATE_ESC_CODES  = 1 << 18;
        const REPORT_EVENT_TYPES      = 1 << 19;
        const REPORT_ALTERNATE_KEYS   = 1 << 20;
        const REPORT_ALL_KEYS_AS_ESC  = 1 << 21;
        const REPORT_ASSOCIATED_TEXT  = 1 << 22;
        const MOUSE_REPORT_X10        = 1 << 23;
        const GRAPHEME_CLUSTER        = 1 << 24;
        const MOUSE_MODE = Self::MOUSE_REPORT_CLICK.bits()
                         | Self::MOUSE_MOTION.bits()
                         | Self::MOUSE_DRAG.bits()
                         | Self::MOUSE_REPORT_X10.bits();
    }
}

bitflags::bitflags! {
    /// Mirror of rio's `ansi::KeyboardModes` (the kitty keyboard protocol).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct KeyboardModes: u8 {
        const DISAMBIGUATE_ESC_CODES  = 0b0000_0001;
        const REPORT_EVENT_TYPES      = 0b0000_0010;
        const REPORT_ALTERNATE_KEYS   = 0b0000_0100;
        const REPORT_ALL_KEYS_AS_ESC  = 0b0000_1000;
        const REPORT_ASSOCIATED_TEXT  = 0b0001_0000;
    }
}

impl KeyboardModes {
    pub const NO_MODE: Self = Self::empty();
}

/// Everything an input encoder needs, carried as raw bits so the client never
/// links a parser to learn them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalModes {
    pub mode: u32,
    pub keyboard: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modify_other_keys: Option<u8>,
}

impl TerminalModes {
    pub fn mode(&self) -> TerminalMode {
        TerminalMode::from_bits_truncate(self.mode)
    }

    pub fn keyboard(&self) -> KeyboardModes {
        KeyboardModes::from_bits_truncate(self.keyboard)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCursor {
    /// Row within the host's visible screen (its viewport is always pinned to
    /// the bottom; a client's own scroll offset is local).
    pub row: u16,
    pub col: u16,
    #[serde(default, skip_serializing_if = "is_default")]
    pub shape: CursorShape,
    #[serde(default, skip_serializing_if = "is_false")]
    pub blinking: bool,
}

/// A resolved cell color. Theme-dependent slots stay symbolic; everything else
/// is a palette index or literal RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum TerminalColor {
    Foreground,
    Background,
    Cursor,
    Indexed(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

bitflags::bitflags! {
    /// Mirror of rio's `StyleFlags`, minus its derived aliases.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct CellFlags: u16 {
        const INVERSE          = 1 << 0;
        const BOLD             = 1 << 1;
        const ITALIC           = 1 << 2;
        const DIM              = 1 << 3;
        const HIDDEN           = 1 << 4;
        const STRIKEOUT        = 1 << 5;
        const UNDERLINE        = 1 << 6;
        const DOUBLE_UNDERLINE = 1 << 7;
        const UNDERCURL        = 1 << 8;
        const DOTTED_UNDERLINE = 1 << 9;
        const DASHED_UNDERLINE = 1 << 10;
        const ALL_UNDERLINES   = Self::UNDERLINE.bits()
                               | Self::DOUBLE_UNDERLINE.bits()
                               | Self::UNDERCURL.bits()
                               | Self::DOTTED_UNDERLINE.bits()
                               | Self::DASHED_UNDERLINE.bits();
    }
}

/// A resolved cell style. Index 0 of every style table is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalStyle {
    #[serde(default = "TerminalColor::foreground")]
    pub fg: TerminalColor,
    #[serde(default = "TerminalColor::background")]
    pub bg: TerminalColor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline_color: Option<TerminalColor>,
    /// Raw [`CellFlags`] bits.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub flags: u16,
}

impl TerminalColor {
    fn foreground() -> Self {
        Self::Foreground
    }

    fn background() -> Self {
        Self::Background
    }
}

impl Default for TerminalStyle {
    fn default() -> Self {
        Self {
            fg: TerminalColor::Foreground,
            bg: TerminalColor::Background,
            underline_color: None,
            flags: 0,
        }
    }
}

impl TerminalStyle {
    pub fn flags(&self) -> CellFlags {
        CellFlags::from_bits_truncate(self.flags)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellWidth {
    #[default]
    Narrow,
    /// The left half of a double-width character.
    Wide,
    /// The reserved right half of a double-width character.
    Spacer,
}

/// An OSC 8 hyperlink attached to a cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalLink {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// One grid cell. Everything is skipped at its default, so a blank cell is `{}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCell {
    /// The base character plus any combining marks. Empty means a blank cell.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub width: CellWidth,
    /// Index into the owning message's style table.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub style: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<TerminalLink>,
}

/// One grid row. Trailing default cells are omitted; a reader pads to `cols`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRow {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<TerminalCell>,
    /// The line continues on the next row rather than ending there.
    #[serde(default, skip_serializing_if = "is_false")]
    pub wrapped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRowUpdate {
    pub index: u16,
    pub row: TerminalRow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum TerminalHistoryUpdate {
    /// Rows that scrolled off the screen since the previous delta.
    Appended(Vec<TerminalRow>),
    /// The whole retained scrollback, after a burst too large to stream.
    Replaced(Vec<TerminalRow>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalExit {
    pub exited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalClipboard {
    /// OSC 52 primary selection rather than the system clipboard.
    #[serde(default, skip_serializing_if = "is_false")]
    pub selection: bool,
    pub text: String,
}

/// The complete replicated grid.
///
/// Sent on subscribe for a live terminal, and as every
/// [`crate::ServerEvent::CommandOutputReplaced`] for a command's output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFrame {
    pub cols: u16,
    pub rows: u16,
    pub modes: TerminalModes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<TerminalCursor>,
    pub styles: Vec<TerminalStyle>,
    pub visible: Vec<TerminalRow>,
    /// Retained scrollback, oldest first, capped at [`HISTORY_LIMIT`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<TerminalRow>,
    /// Lines the host dropped from its own scrollback before `history`.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub lines_evicted: u64,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub exited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// An incremental update to a [`TerminalFrame`].
///
/// Dimensions, cursor, modes and eviction count are unconditional: they are a
/// few dozen bytes and they remove every "did this change?" flag from the
/// contract. Everything else appears only when it changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalDelta {
    pub cols: u16,
    pub rows: u16,
    pub modes: TerminalModes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<TerminalCursor>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub lines_evicted: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub styles: Vec<TerminalStyle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows_replaced: Vec<TerminalRowUpdate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<TerminalHistoryUpdate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<TerminalExit>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bell: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<TerminalClipboard>,
}

/// Scrollback rows retained by the host projection and by every client.
pub const HISTORY_LIMIT: usize = 1000;

impl TerminalFrame {
    /// Apply a delta in place. Used by clients and by the host's own retained
    /// projection, so both sides advance through identical state.
    pub fn apply(&mut self, delta: &TerminalDelta) {
        self.cols = delta.cols;
        self.rows = delta.rows;
        self.modes = delta.modes;
        self.cursor = delta.cursor;
        self.lines_evicted = delta.lines_evicted;
        let styles = remap_styles(&mut self.styles, &delta.styles);
        self.visible
            .resize_with(usize::from(delta.rows), Default::default);
        for update in &delta.rows_replaced {
            if let Some(row) = self.visible.get_mut(usize::from(update.index)) {
                *row = restyle_row(update.row.clone(), &styles);
            }
        }
        match &delta.history {
            Some(TerminalHistoryUpdate::Appended(rows)) => {
                self.history
                    .extend(rows.iter().map(|row| restyle_row(row.clone(), &styles)));
                let overflow = self.history.len().saturating_sub(HISTORY_LIMIT);
                self.history.drain(..overflow);
            }
            Some(TerminalHistoryUpdate::Replaced(rows)) => {
                self.history = rows
                    .iter()
                    .map(|row| restyle_row(row.clone(), &styles))
                    .collect();
            }
            None => {}
        }
        if let Some(title) = &delta.title {
            self.title = title.clone();
        }
        if let Some(cwd) = &delta.working_directory {
            self.working_directory = Some(cwd.clone());
        }
        if let Some(exit) = delta.exit {
            self.exited = exit.exited;
            self.exit_code = exit.code;
        }
    }

    /// The style a cell resolves to, or the default for an out-of-range index.
    pub fn style(&self, cell: &TerminalCell) -> TerminalStyle {
        self.styles
            .get(usize::from(cell.style))
            .copied()
            .unwrap_or_default()
    }
}

/// Merge a message-local style table into `table`, returning the index each
/// message-local id maps to.
fn remap_styles(table: &mut Vec<TerminalStyle>, local: &[TerminalStyle]) -> Vec<u16> {
    if table.is_empty() {
        table.push(TerminalStyle::default());
    }
    local
        .iter()
        .map(
            |style| match table.iter().position(|existing| existing == style) {
                Some(index) => index as u16,
                None => {
                    table.push(*style);
                    (table.len() - 1) as u16
                }
            },
        )
        .collect()
}

fn restyle_row(mut row: TerminalRow, styles: &[u16]) -> TerminalRow {
    for cell in &mut row.cells {
        cell.style = styles
            .get(usize::from(cell.style))
            .copied()
            .unwrap_or_default();
    }
    row
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero_u16(value: &u16) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}
