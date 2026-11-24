//! The host-authoritative grid projection published on [`Topic::Terminal`].
//!
//! One projection per live terminal keeps the frame a subscriber would receive
//! right now. Deltas are produced from the same snapshot sequence that advances
//! that frame, so a client attaching at any moment gets frame N and then deltas
//! N+1…: never a duplicate row, never a gap.

use std::time::{Duration, Instant};

use tcode_protocol::terminal::{
    HISTORY_LIMIT, TerminalClipboard, TerminalDelta, TerminalExit, TerminalFrame,
    TerminalHistoryUpdate, TerminalRow, TerminalRowUpdate, TerminalStyle,
};
use term::Projector;

/// At most one delta per frame interval while a terminal is dirty.
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Scrollback rows one delta may stream. A burst larger than this defers to a
/// single `Replaced` history once the flood settles, instead of shipping the
/// whole ring on every frame.
const HISTORY_STREAM_BUDGET: usize = 256;

/// PTY reads can have short gaps during a flood, especially under CPU load.
/// Wait for scrolling to stop before republishing deferred scrollback.
const HISTORY_QUIET_INTERVAL: Duration = Duration::from_millis(100);

/// Rebuild the frame rather than growing the interned style table past this.
/// Cell style ids are `u16`, so an unbounded table would eventually alias.
const STYLE_TABLE_LIMIT: usize = 4096;

/// The foreground process directory is a syscall; a terminal's cwd changes at
/// human speed, so it is refreshed far below the frame rate.
const CWD_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) enum TerminalUpdate {
    Frame(TerminalFrame),
    Delta(TerminalDelta),
}

pub(crate) struct TerminalProjection {
    /// The state a subscriber would receive right now.
    pub(crate) frame: TerminalFrame,
    /// Lines that had scrolled off when `frame` was last advanced.
    scrolled: u64,
    /// A burst exceeded [`HISTORY_STREAM_BUDGET`]. Republish the retained
    /// scrollback after this deadline, postponed by each new scrolled row.
    history_deadline: Option<Instant>,
    /// A projection is already scheduled; further wakeups coalesce into it.
    pub(crate) scheduled: bool,
    pub(crate) last_projected: Instant,
    last_cwd: Instant,
    pub(crate) bell: bool,
    pub(crate) clipboard: Option<TerminalClipboard>,
}

impl TerminalProjection {
    pub(crate) fn new() -> Self {
        Self {
            frame: TerminalFrame::default(),
            scrolled: 0,
            history_deadline: None,
            scheduled: false,
            // Far enough in the past that the first wakeup projects immediately.
            last_projected: Instant::now() - FRAME_INTERVAL,
            last_cwd: Instant::now() - CWD_INTERVAL,
            bell: false,
            clipboard: None,
        }
    }

    /// Rebuild the whole frame from the terminal's current state.
    pub(crate) fn reset(&mut self, terminal: &term::Terminal) -> TerminalFrame {
        let snapshot = terminal.snapshot_since(0, HISTORY_LIMIT);
        let mut projector = Projector::default();
        let visible = (0..snapshot.screen_lines)
            .map(|row| projector.visible_row(&snapshot, row))
            .collect();
        let history = projector.history(&snapshot);
        self.frame = TerminalFrame {
            cols: snapshot.cols.min(u16::MAX as usize) as u16,
            rows: snapshot.screen_lines.min(u16::MAX as usize) as u16,
            modes: term::project::modes(&snapshot, terminal.modify_other_keys()),
            cursor: term::project::cursor(&snapshot),
            styles: projector.into_styles(),
            visible,
            history,
            lines_evicted: snapshot.lines_evicted,
            title: snapshot.title.clone(),
            working_directory: Some(terminal.working_directory()),
            exited: snapshot.exited,
            exit_code: snapshot.exit_code,
        };
        self.scrolled = snapshot.lines_evicted + snapshot.history_size as u64;
        self.history_deadline = None;
        self.last_cwd = Instant::now();
        self.frame.clone()
    }

    /// Diff the terminal's current state into the smallest update that carries
    /// a subscriber from `frame` to it.
    pub(crate) fn update(&mut self, terminal: &term::Terminal) -> Option<TerminalUpdate> {
        let scrolled_now = terminal.grid().scrolled_lines();
        let pending = scrolled_now.saturating_sub(self.scrolled);
        // The alternate screen swaps in a grid with its own scrollback counter,
        // so a decrease means the ring was replaced rather than appended to.
        let swapped = scrolled_now < self.scrolled;
        // A column change reflows the scrollback in place, so appending cannot
        // describe it. Row-only resizes (the drawer's height drag) do not.
        let reflowed = terminal.grid().dimensions().0.min(u16::MAX as usize) as u16
            != self.frame.cols
            && !self.frame.history.is_empty();
        let republish = swapped
            || reflowed
            || (pending == 0
                && self
                    .history_deadline
                    .is_some_and(|deadline| self.last_projected >= deadline));
        let (since, budget) = if republish {
            (0, HISTORY_LIMIT)
        } else {
            (self.scrolled, HISTORY_STREAM_BUDGET)
        };
        let snapshot = terminal.snapshot_since(since, budget);

        let mut projector = Projector::default();
        let cols = snapshot.cols.min(u16::MAX as usize) as u16;
        let rows = snapshot.screen_lines.min(u16::MAX as usize) as u16;
        let resized = (cols, rows) != (self.frame.cols, self.frame.rows);

        let mut rows_replaced = Vec::new();
        for row in 0..snapshot.screen_lines {
            if !resized && !snapshot.row_damage.get(row).copied().unwrap_or(true) {
                continue;
            }
            let projected = projector.visible_row(&snapshot, row);
            let unchanged = self.frame.visible.get(row).is_some_and(|current| {
                same_row(current, &self.frame.styles, &projected, projector.styles())
            });
            if unchanged {
                continue;
            }
            rows_replaced.push(TerminalRowUpdate {
                index: row as u16,
                row: projected,
            });
        }

        let scrolled = snapshot.lines_evicted + snapshot.history_size as u64;
        let appended = scrolled.saturating_sub(self.scrolled) as usize;
        let history = if republish {
            self.history_deadline = None;
            // ponytail: the whole ring goes back on the wire. Diffing reflowed
            // scrollback would need row identity the emulator does not expose;
            // revisit if a width drag over a full ring is measurably heavy.
            Some(TerminalHistoryUpdate::Replaced(
                projector.history(&snapshot),
            ))
        } else if appended == 0 {
            None
        } else if self.history_deadline.is_none() && appended <= snapshot.history_rows.len() {
            let start = snapshot.history_rows.len() - appended;
            Some(TerminalHistoryUpdate::Appended(
                (start..snapshot.history_rows.len())
                    .map(|row| projector.history_row(&snapshot, row))
                    .collect(),
            ))
        } else {
            self.history_deadline = Some(self.last_projected + HISTORY_QUIET_INTERVAL);
            None
        };
        self.scrolled = scrolled;

        let cursor = term::project::cursor(&snapshot);
        let modes = term::project::modes(&snapshot, terminal.modify_other_keys());
        let title = (snapshot.title != self.frame.title).then(|| snapshot.title.clone());
        let exit = (snapshot.exited != self.frame.exited
            || snapshot.exit_code != self.frame.exit_code)
            .then_some(TerminalExit {
                exited: snapshot.exited,
                code: snapshot.exit_code,
            });
        let working_directory = self.refresh_cwd(terminal);

        let delta = TerminalDelta {
            cols,
            rows,
            modes,
            cursor,
            lines_evicted: snapshot.lines_evicted,
            styles: projector.into_styles(),
            rows_replaced,
            history,
            title,
            working_directory,
            exit,
            bell: std::mem::take(&mut self.bell),
            clipboard: self.clipboard.take(),
        };
        let changed = resized
            || !delta.rows_replaced.is_empty()
            || delta.history.is_some()
            || delta.title.is_some()
            || delta.working_directory.is_some()
            || delta.exit.is_some()
            || delta.bell
            || delta.clipboard.is_some()
            || delta.cursor != self.frame.cursor
            || delta.modes != self.frame.modes
            || delta.lines_evicted != self.frame.lines_evicted;
        if !changed {
            return None;
        }
        // The retained frame advances through the same delta the subscriber
        // sees, so both sides stay one state machine.
        self.frame.apply(&delta);
        Some(TerminalUpdate::Delta(delta))
    }

    fn refresh_cwd(&mut self, terminal: &term::Terminal) -> Option<std::path::PathBuf> {
        if self.last_cwd.elapsed() < CWD_INTERVAL {
            return None;
        }
        self.last_cwd = Instant::now();
        let cwd = terminal.working_directory();
        (Some(&cwd) != self.frame.working_directory.as_ref()).then_some(cwd)
    }

    /// Whether the interned style table has grown past what a `u16` cell id can
    /// address safely.
    pub(crate) fn styles_exhausted(&self) -> bool {
        self.frame.styles.len() > STYLE_TABLE_LIMIT
    }

    /// A burst deferred its scrollback. The caller must schedule another
    /// projection, because the terminal may now be idle and produce no further
    /// wakeup of its own.
    pub(crate) fn owes_history(&self) -> bool {
        self.history_deadline.is_some()
    }
}

/// Compare two rows whose style ids come from different tables.
fn same_row(
    current: &TerminalRow,
    current_styles: &[TerminalStyle],
    projected: &TerminalRow,
    projected_styles: &[TerminalStyle],
) -> bool {
    if current.wrapped != projected.wrapped || current.cells.len() != projected.cells.len() {
        return false;
    }
    current
        .cells
        .iter()
        .zip(&projected.cells)
        .all(|(current_cell, projected_cell)| {
            current_cell.text == projected_cell.text
                && current_cell.width == projected_cell.width
                && current_cell.link == projected_cell.link
                && current_styles.get(usize::from(current_cell.style))
                    == projected_styles.get(usize::from(projected_cell.style))
        })
}
