//! Render a command's captured output into a [`TerminalFrame`].
//!
//! Clients keep no emulator, so the width they can show is a subscription and
//! the wrapped, styled grid is what the host pushes. This runs on the host's
//! blocking-I/O executor: it is pure CPU over one string.

use tcode_protocol::{STORED_OUTPUT_COLS, STORED_OUTPUT_ROWS, terminal::TerminalFrame};
use term::{GridEmulator, Projector};

/// Output bytes replayed into the emulator. Everything before this has already
/// scrolled off a [`STORED_OUTPUT_ROWS`]-row screen.
const TAIL_BYTES: usize = 32 * 1024;

/// Render `output` as it would have looked in a `cols`-wide terminal.
///
/// `cols` is clamped into [`STORED_OUTPUT_COLS`], so the frame's own `cols` is
/// the authority on what the caller actually got.
pub fn render(output: &str, cols: u16) -> TerminalFrame {
    let cols = cols.clamp(*STORED_OUTPUT_COLS.start(), *STORED_OUTPUT_COLS.end());
    let emulator = GridEmulator::with_size(usize::from(cols), usize::from(STORED_OUTPUT_ROWS));
    emulator.feed(&normalized_tail(output.as_bytes()));

    let snapshot = emulator.snapshot();
    let mut projector = Projector::default();
    let mut visible = (0..snapshot.screen_lines)
        .map(|row| projector.visible_row(&snapshot, row))
        .collect::<Vec<_>>();
    // A command block sizes to its content: trailing blank rows are not part of
    // the output, they are the unused bottom of the screen it ran on.
    // The projector drops trailing default cells, so an untouched row is empty.
    while visible.last().is_some_and(|row| row.cells.is_empty()) {
        visible.pop();
    }

    TerminalFrame {
        cols,
        rows: visible.len().min(u16::MAX as usize) as u16,
        styles: projector.into_styles(),
        visible,
        ..TerminalFrame::default()
    }
}

/// Captured output is a log, not a PTY stream: a bare LF means "next line at
/// column zero". Feeding it raw would leave every line indented by the last.
fn normalized_tail(bytes: &[u8]) -> Vec<u8> {
    let start = bytes.len().saturating_sub(TAIL_BYTES);
    let mut previous_was_cr = start > 0 && bytes[start - 1] == b'\r';
    let mut normalized = Vec::with_capacity(bytes.len() - start);
    for &byte in &bytes[start..] {
        if byte == b'\n' && !previous_was_cr {
            normalized.push(b'\r');
        }
        normalized.push(byte);
        previous_was_cr = byte == b'\r';
    }
    normalized
}
