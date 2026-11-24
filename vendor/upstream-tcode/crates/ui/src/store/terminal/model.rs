//! The client's replicated terminal grid.
//!
//! The host owns the emulator; this owns everything that is genuinely local to
//! one viewer: the scroll offset, the selection, and link hit-testing. It
//! parses nothing, so a desktop window and a phone attached to the same host
//! render the same grid by construction.

use tcode_protocol::terminal::{
    CellWidth, CursorShape, KeyboardModes, TerminalCell, TerminalClipboard, TerminalDelta,
    TerminalFrame, TerminalMode, TerminalRow, TerminalStyle,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Semantic,
    Lines,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionSide {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HyperlinkMatch {
    pub url: String,
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// Word characters follow alacritty's default separator set.
const WORD_DELIMITERS: &str = " \t,│`|:\"'()[]{}<>";

/// Characters an unlinked URL may not contain, matching the pattern rio's own
/// grid search uses.
fn url_char(ch: char) -> bool {
    !ch.is_control()
        && !ch.is_whitespace()
        && !matches!(
            ch,
            '<' | '>' | '"' | '{' | '|' | '}' | '^' | '⟨' | '⟩' | '`' | '\''
        )
        && !('\u{7f}'..='\u{9f}').contains(&ch)
}

const URL_SCHEMES: [&str; 14] = [
    "ipfs:",
    "ipns:",
    "magnet:",
    "mailto:",
    "gemini://",
    "gopher://",
    "https://",
    "http://",
    "news:",
    "file://",
    "git://",
    "ssh:",
    "ftp://",
    "zed://",
];

/// A selection endpoint in absolute line coordinates, so scrolling and
/// scrollback eviction never move it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Anchor {
    line: u64,
    col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    kind: SelectionKind,
    anchor: Anchor,
    anchor_side: SelectionSide,
    focus: Anchor,
    focus_side: SelectionSide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectionRange {
    start: Anchor,
    end: Anchor,
}

pub struct TerminalModel {
    frame: TerminalFrame,
    /// Rows scrolled back from the bottom. Always 0 on the alternate screen,
    /// which has no scrollback.
    display_offset: usize,
    selection: Option<Selection>,
    /// Screen rows whose content changed since the renderer last read them.
    damage: Vec<bool>,
    /// Set when the whole grid must be re-laid out (resize, scroll, selection).
    full_damage: bool,
    bell: bool,
    clipboard: Option<TerminalClipboard>,
    /// Shown until the host reports a title.
    fallback_title: String,
    blank: TerminalCell,
}

impl TerminalModel {
    pub fn new(fallback_title: String) -> Self {
        Self {
            frame: TerminalFrame::default(),
            display_offset: 0,
            selection: None,
            damage: Vec::new(),
            full_damage: true,
            bell: false,
            clipboard: None,
            fallback_title,
            blank: TerminalCell::default(),
        }
    }

    pub fn apply_frame(&mut self, frame: TerminalFrame) {
        self.frame = frame;
        self.display_offset = 0;
        self.selection = None;
        self.full_damage = true;
        self.damage = vec![true; usize::from(self.frame.rows)];
    }

    pub fn apply_delta(&mut self, delta: &TerminalDelta) {
        let resized = (delta.cols, delta.rows) != (self.frame.cols, self.frame.rows);
        let evicted = delta.lines_evicted.saturating_sub(self.frame.lines_evicted);
        let previous_cursor = self.frame.cursor;
        self.frame.apply(delta);
        if resized {
            self.damage = vec![true; usize::from(self.frame.rows)];
            self.full_damage = true;
            // Row content moves under a scrolled viewport on reflow; the safe
            // reading position is the live one.
            self.display_offset = 0;
            self.selection = None;
        } else {
            self.damage.resize(usize::from(self.frame.rows), true);
            for update in &delta.rows_replaced {
                if let Some(damaged) = self.damage.get_mut(usize::from(update.index)) {
                    *damaged = true;
                }
            }
        }
        // A moved cursor repaints both the row it left and the row it entered.
        if previous_cursor != self.frame.cursor {
            for row in previous_cursor.into_iter().chain(self.frame.cursor) {
                if let Some(damaged) = self.damage.get_mut(usize::from(row.row)) {
                    *damaged = true;
                }
            }
        }
        // Output that scrolls the grid slides a scrolled-back viewport with it,
        // so the reader keeps looking at the same content.
        if self.display_offset > 0
            && let Some(appended) = delta_history_len(delta)
        {
            let capped = self.frame.history.len();
            self.display_offset = (self.display_offset + appended).min(capped);
            self.full_damage = true;
        }
        if evicted > 0 {
            self.full_damage = true;
        }
        self.bell |= delta.bell;
        if let Some(clipboard) = &delta.clipboard {
            self.clipboard = Some(clipboard.clone());
        }
    }

    pub fn set_fallback_title(&mut self, title: String) {
        self.fallback_title = title;
    }

    /// Consume the bell and OSC 52 notices raised since the last read.
    pub fn take_notices(&mut self) -> (bool, Option<TerminalClipboard>) {
        (std::mem::take(&mut self.bell), self.clipboard.take())
    }

    pub fn cols(&self) -> usize {
        usize::from(self.frame.cols)
    }

    pub fn rows(&self) -> usize {
        usize::from(self.frame.rows)
    }

    pub fn title(&self) -> String {
        if self.frame.title.is_empty() {
            self.fallback_title.clone()
        } else {
            self.frame.title.clone()
        }
    }

    pub fn exited(&self) -> bool {
        self.frame.exited
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.frame.exit_code
    }

    pub fn working_directory(&self) -> Option<&std::path::Path> {
        self.frame.working_directory.as_deref()
    }

    pub fn mode(&self) -> TerminalMode {
        self.frame.modes.mode()
    }

    pub fn keyboard_mode(&self) -> KeyboardModes {
        self.frame.modes.keyboard()
    }

    pub fn modify_other_keys(&self) -> Option<u8> {
        self.frame.modes.modify_other_keys
    }

    pub fn history_size(&self) -> usize {
        self.frame.history.len()
    }

    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    pub fn row_damaged(&self, row: usize) -> bool {
        self.full_damage || self.damage.get(row).copied().unwrap_or(true)
    }

    pub fn fully_damaged(&self) -> bool {
        self.full_damage
    }

    /// Called by the renderer once it has consumed the current damage.
    pub fn clear_damage(&mut self) {
        self.full_damage = false;
        self.damage.iter_mut().for_each(|damaged| *damaged = false);
    }

    fn line(&self, index: usize) -> Option<&TerminalRow> {
        let history = self.frame.history.len();
        if index < history {
            self.frame.history.get(index)
        } else {
            self.frame.visible.get(index - history)
        }
    }

    fn lines(&self) -> usize {
        self.frame.history.len() + self.frame.visible.len()
    }

    /// The absolute line index shown at screen row `row`.
    fn line_index(&self, row: usize) -> usize {
        self.frame.history.len() - self.display_offset + row
    }

    pub fn row(&self, row: usize) -> Option<&TerminalRow> {
        self.line(self.line_index(row))
    }

    pub fn row_wrapped(&self, row: usize) -> bool {
        self.row(row).is_some_and(|row| row.wrapped)
    }

    pub fn cell(&self, row: usize, col: usize) -> Option<&TerminalCell> {
        if col >= self.cols() || row >= self.rows() {
            return None;
        }
        Some(self.row(row)?.cells.get(col).unwrap_or(&self.blank))
    }

    pub fn style(&self, cell: &TerminalCell) -> TerminalStyle {
        self.frame.style(cell)
    }

    /// Cursor position within the visible screen, if it is on screen and the
    /// viewport is live.
    pub fn cursor(&self) -> Option<(usize, usize)> {
        if self.display_offset != 0 {
            return None;
        }
        let cursor = self.frame.cursor?;
        Some((usize::from(cursor.row), usize::from(cursor.col)))
    }

    pub fn cursor_shape(&self) -> CursorShape {
        self.frame
            .cursor
            .map_or(CursorShape::Hidden, |cursor| cursor.shape)
    }

    pub fn cursor_blinking(&self) -> bool {
        self.frame.cursor.is_some_and(|cursor| cursor.blinking)
    }

    // -- viewport -----------------------------------------------------------

    pub fn scroll(&mut self, lines: i32) {
        // The alternate screen is not backed by scrollback; rio ignores scroll
        // there and so does every other terminal.
        if self.mode().contains(TerminalMode::ALT_SCREEN) {
            return;
        }
        let offset = (self.display_offset as i64 + i64::from(lines))
            .clamp(0, self.frame.history.len() as i64) as usize;
        if offset != self.display_offset {
            self.display_offset = offset;
            self.full_damage = true;
        }
    }

    /// Local behaviour that accompanies typing: return to the live viewport and
    /// drop the selection. The bytes themselves still go to the host.
    pub fn prepare_input(&mut self) {
        if self.display_offset != 0 || self.selection.is_some() {
            self.display_offset = 0;
            self.selection = None;
            self.full_damage = true;
        }
    }

    // -- selection ----------------------------------------------------------

    fn anchor(&self, point: (usize, usize)) -> Anchor {
        Anchor {
            line: self.frame.lines_evicted + self.line_index(point.0) as u64,
            col: point.1.min(self.cols().saturating_sub(1)),
        }
    }

    pub fn start_selection(
        &mut self,
        kind: SelectionKind,
        point: (usize, usize),
        side: SelectionSide,
    ) {
        let anchor = self.anchor(point);
        self.selection = Some(Selection {
            kind,
            anchor,
            anchor_side: side,
            focus: anchor,
            focus_side: side,
        });
        self.full_damage = true;
    }

    pub fn update_selection(&mut self, point: (usize, usize), side: SelectionSide) {
        let focus = self.anchor(point);
        if let Some(selection) = self.selection.as_mut() {
            selection.focus = focus;
            selection.focus_side = side;
            self.full_damage = true;
        }
    }

    pub fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.full_damage = true;
        }
    }

    pub fn select_all(&mut self) {
        let last = self.lines().saturating_sub(1) as u64;
        self.selection = Some(Selection {
            kind: SelectionKind::Simple,
            anchor: Anchor {
                line: self.frame.lines_evicted,
                col: 0,
            },
            anchor_side: SelectionSide::Left,
            focus: Anchor {
                line: self.frame.lines_evicted + last,
                col: self.cols().saturating_sub(1),
            },
            focus_side: SelectionSide::Right,
        });
        self.full_damage = true;
    }

    pub fn has_selection(&self) -> bool {
        self.range().is_some()
    }

    fn range(&self) -> Option<SelectionRange> {
        let selection = self.selection?;
        let (mut start, start_side, mut end, end_side) =
            if (selection.anchor, selection.anchor_side_rank())
                <= (selection.focus, selection.focus_side_rank())
            {
                (
                    selection.anchor,
                    selection.anchor_side,
                    selection.focus,
                    selection.focus_side,
                )
            } else {
                (
                    selection.focus,
                    selection.focus_side,
                    selection.anchor,
                    selection.anchor_side,
                )
            };
        match selection.kind {
            SelectionKind::Simple => {
                if start == end && start_side == SelectionSide::Right {
                    return None;
                }
                if start_side == SelectionSide::Right {
                    start = self.next_cell(start)?;
                }
                if end_side == SelectionSide::Left {
                    end = self.previous_cell(end)?;
                }
                if start > end {
                    return None;
                }
            }
            SelectionKind::Semantic => {
                start = self.word_start(start);
                end = self.word_end(end);
            }
            SelectionKind::Lines => {
                start.col = 0;
                end.col = self.cols().saturating_sub(1);
            }
        }
        Some(SelectionRange { start, end })
    }

    fn next_cell(&self, mut point: Anchor) -> Option<Anchor> {
        if point.col + 1 < self.cols() {
            point.col += 1;
        } else {
            point.line += 1;
            point.col = 0;
        }
        (point.line < self.frame.lines_evicted + self.lines() as u64).then_some(point)
    }

    fn previous_cell(&self, mut point: Anchor) -> Option<Anchor> {
        if point.col > 0 {
            point.col -= 1;
        } else if point.line > self.frame.lines_evicted {
            point.line -= 1;
            point.col = self.cols().saturating_sub(1);
        } else {
            return None;
        }
        Some(point)
    }

    fn word_start(&self, mut point: Anchor) -> Anchor {
        while point.col > 0 {
            let previous = Anchor {
                col: point.col - 1,
                ..point
            };
            if self.is_word_delimiter(previous) {
                break;
            }
            point = previous;
        }
        point
    }

    fn word_end(&self, mut point: Anchor) -> Anchor {
        while point.col + 1 < self.cols() {
            let next = Anchor {
                col: point.col + 1,
                ..point
            };
            if self.is_word_delimiter(next) {
                break;
            }
            point = next;
        }
        point
    }

    fn is_word_delimiter(&self, point: Anchor) -> bool {
        let Some(cell) = self.absolute_cell(point) else {
            return true;
        };
        match cell.text.chars().next() {
            None => true,
            Some(ch) => WORD_DELIMITERS.contains(ch),
        }
    }

    fn absolute_cell(&self, point: Anchor) -> Option<&TerminalCell> {
        let index = usize::try_from(point.line.checked_sub(self.frame.lines_evicted)?).ok()?;
        Some(
            self.line(index)?
                .cells
                .get(point.col)
                .unwrap_or(&self.blank),
        )
    }

    pub fn is_selected(&self, row: usize, col: usize) -> bool {
        let Some(range) = self.range() else {
            return false;
        };
        let point = Anchor {
            line: self.frame.lines_evicted + self.line_index(row) as u64,
            col,
        };
        if in_range(&range, point) {
            return true;
        }
        // The reserved half of a wide character follows its partner.
        col > 0
            && self
                .cell(row, col)
                .is_some_and(|cell| cell.width == CellWidth::Spacer)
            && in_range(
                &range,
                Anchor {
                    col: col - 1,
                    ..point
                },
            )
    }

    /// The selected text plus the 1-based line span it covers within the
    /// retained buffer, matching what the composer's terminal chip shows.
    pub fn selected_text(&self) -> Option<(usize, usize, String)> {
        let range = self.range()?;
        let start_line =
            usize::try_from(range.start.line.saturating_sub(self.frame.lines_evicted)).unwrap_or(0);
        let end_line =
            usize::try_from(range.end.line.saturating_sub(self.frame.lines_evicted)).unwrap_or(0);
        let mut text = String::new();
        for index in start_line..=end_line.min(self.lines().saturating_sub(1)) {
            let Some(row) = self.line(index) else {
                continue;
            };
            let first = if index == start_line {
                range.start.col
            } else {
                0
            };
            let last = if index == end_line {
                range.end.col
            } else {
                self.cols().saturating_sub(1)
            };
            let mut line = String::new();
            for col in first..=last.min(self.cols().saturating_sub(1)) {
                let cell = row.cells.get(col).unwrap_or(&self.blank);
                if cell.width == CellWidth::Spacer {
                    continue;
                }
                if cell.text.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(&cell.text);
                }
            }
            text.push_str(line.trim_end());
            // A wrapped row continues the same logical line.
            if index != end_line && !row.wrapped {
                text.push('\n');
            }
        }
        let text = text.trim_matches('\n').to_string();
        (!text.is_empty()).then_some((start_line + 1, end_line + 1, text))
    }

    // -- hyperlinks ---------------------------------------------------------

    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<HyperlinkMatch> {
        let cell = self.cell(row, col)?;
        if let Some(link) = &cell.link {
            let mut start = (row, col);
            while let Some(previous) = self.previous_screen_cell(start) {
                if self.cell(previous.0, previous.1)?.link.as_ref() != Some(link) {
                    break;
                }
                start = previous;
            }
            let mut end = (row, col);
            while let Some(next) = self.next_screen_cell(end) {
                if self.cell(next.0, next.1)?.link.as_ref() != Some(link) {
                    break;
                }
                end = next;
            }
            return Some(HyperlinkMatch {
                url: link.uri.clone(),
                start,
                end,
            });
        }
        self.url_at(row, col)
    }

    fn previous_screen_cell(&self, (row, col): (usize, usize)) -> Option<(usize, usize)> {
        if col > 0 {
            Some((row, col - 1))
        } else {
            row.checked_sub(1).map(|row| (row, self.cols() - 1))
        }
    }

    fn next_screen_cell(&self, (row, col): (usize, usize)) -> Option<(usize, usize)> {
        if col + 1 < self.cols() {
            Some((row, col + 1))
        } else {
            (row + 1 < self.rows()).then_some((row + 1, 0))
        }
    }

    /// Match an unlinked URL inside the logical (wrap-joined) line under the
    /// point, mirroring the grid search rio ran on the client's own emulator.
    fn url_at(&self, row: usize, col: usize) -> Option<HyperlinkMatch> {
        let mut first = row;
        while first > 0 && self.row_wrapped(first - 1) {
            first -= 1;
        }
        let mut last = row;
        while last + 1 < self.rows() && self.row_wrapped(last) {
            last += 1;
        }

        let mut text = String::new();
        let mut positions = Vec::new();
        for line in first..=last {
            for column in 0..self.cols() {
                let Some(cell) = self.cell(line, column) else {
                    continue;
                };
                if cell.width == CellWidth::Spacer {
                    continue;
                }
                positions.push((text.len(), (line, column)));
                if cell.text.is_empty() {
                    text.push(' ');
                } else {
                    text.push_str(&cell.text);
                }
            }
        }
        let cursor = positions
            .iter()
            .position(|(_, position)| *position == (row, col))?;

        for (index, (offset, _)) in positions.iter().enumerate() {
            let rest = &text[*offset..];
            if !URL_SCHEMES
                .iter()
                .any(|scheme| rest.starts_with(scheme) || rest.starts_with(&scheme.to_uppercase()))
            {
                continue;
            }
            let mut end = index;
            while end + 1 < positions.len() {
                let (next_offset, _) = positions[end + 1];
                let Some(ch) = text[next_offset..].chars().next() else {
                    break;
                };
                if !url_char(ch) {
                    break;
                }
                end += 1;
            }
            let mut url_end = if end + 1 < positions.len() {
                positions[end + 1].0
            } else {
                text.len()
            };
            let mut url = text[*offset..url_end].to_string();
            while url.ends_with(['.', ',', ':', ';', '!', '?']) {
                url.pop();
                url_end -= 1;
                if end > index {
                    end -= 1;
                }
            }
            if url.len() <= 8 || cursor < index || cursor > end {
                continue;
            }
            return Some(HyperlinkMatch {
                url,
                start: positions[index].1,
                end: positions[end].1,
            });
        }
        None
    }
}

impl Selection {
    fn anchor_side_rank(&self) -> u8 {
        side_rank(self.anchor_side)
    }

    fn focus_side_rank(&self) -> u8 {
        side_rank(self.focus_side)
    }
}

fn side_rank(side: SelectionSide) -> u8 {
    match side {
        SelectionSide::Left => 0,
        SelectionSide::Right => 1,
    }
}

fn in_range(range: &SelectionRange, point: Anchor) -> bool {
    if point.line < range.start.line || point.line > range.end.line {
        return false;
    }
    if point.line == range.start.line && point.col < range.start.col {
        return false;
    }
    if point.line == range.end.line && point.col > range.end.col {
        return false;
    }
    true
}

fn delta_history_len(delta: &TerminalDelta) -> Option<usize> {
    match delta.history.as_ref()? {
        tcode_protocol::terminal::TerminalHistoryUpdate::Appended(rows) => Some(rows.len()),
        // A republished ring is not a scroll: the viewport keeps its offset.
        tcode_protocol::terminal::TerminalHistoryUpdate::Replaced(_) => None,
    }
}
