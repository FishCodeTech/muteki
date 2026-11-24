//! UI-agnostic terminal process management and emulation.

use std::collections::HashMap;
#[cfg(feature = "pty")]
use std::{
    io,
    path::{Path, PathBuf},
    thread,
};

pub use rio_vt;
use rio_vt::{
    ansi::KeyboardModes,
    clipboard::ClipboardType,
    config::colors::{AnsiColor, ColorRgb},
    crosswords::{
        Mode,
        grid::{Indexed, row::Row},
        pos::{Column, CursorState, Line, Pos},
        square::{ContentTag, Square, Wide},
        style::Style,
    },
    event::TerminalDamage,
    selection::SelectionRange,
};

mod grid_emulator;
mod hyperlinks;
pub mod project;
#[cfg(feature = "pty")]
mod pty;
#[cfg(feature = "pty")]
mod pty_info;
mod sync;

pub use hyperlinks::HyperlinkMatch;
pub use project::Projector;

pub use grid_emulator::GridEmulator;
pub use grid_emulator::GridEvent;
#[cfg(feature = "pty")]
use pty::{PtyEvent, PtyHandle};

#[cfg(any(feature = "pty", test))]
const DEFAULT_COLS: usize = 80;
#[cfg(any(feature = "pty", test))]
const DEFAULT_ROWS: usize = 24;
/// Scrollback rows a diagnostic peek copies; the wire cap is the same.
pub const HISTORY_PEEK_LIMIT: usize = 1000;
const DEFAULT_CELL_WIDTH_PX: u32 = 8;
const DEFAULT_CELL_HEIGHT_PX: u32 = 17;

/// A rendering-relevant event emitted by a [`Terminal`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermEvent {
    /// The terminal grid or cursor changed.
    Wakeup,
    /// The emulated terminal rang the bell.
    Bell,
    /// The host-side child process exited.
    Exited,
    /// An OSC 52 request to store decoded UTF-8 text in a clipboard.
    ClipboardStore { kind: ClipboardType, text: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedText {
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct TermSnapshot {
    pub cols: usize,
    pub screen_lines: usize,
    pub visible_rows: Vec<Row<Square>>,
    /// Scrollback rows requested by [`GridEmulator::snapshot_since`], oldest
    /// first. Empty for the plain [`GridEmulator::snapshot`].
    pub history_rows: Vec<Row<Square>>,
    pub row_damage: Vec<bool>,
    pub damage: TerminalDamage,
    pub cursor_state: CursorState,
    pub cursor: Option<(usize, usize)>,
    pub cursor_blinking: bool,
    pub title: String,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub display_offset: usize,
    pub history_size: usize,
    /// Lines evicted from rio's scrollback ring before the retained history.
    pub lines_evicted: u64,
    pub mode: Mode,
    pub keyboard_mode: KeyboardModes,
    pub selection: Option<SelectionRange>,
    /// Snapshot of rio's interned style table, indexed by `Square::style_id`.
    pub styles: Vec<Style>,
    /// Zero-width continuations for snapshotted squares, indexed by rio extras id.
    pub zero_width: HashMap<u16, Vec<char>>,
    /// OSC 8 targets `(uri, id)` for snapshotted squares, indexed by extras id.
    pub links: HashMap<u16, (String, String)>,
}

impl TermSnapshot {
    pub fn cell(&self, row: usize, col: usize) -> Option<&Square> {
        self.visible_rows.get(row)?.inner.get(col)
    }

    pub fn style(&self, row: usize, col: usize) -> Option<Style> {
        let square = *self.cell(row, col)?;
        Some(match square.content_tag() {
            ContentTag::Codepoint => self
                .styles
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
        })
    }

    /// The cell's base character followed by its rio zero-width continuation.
    pub fn cell_text(&self, row: usize, col: usize) -> Option<String> {
        let square = *self.cell(row, col)?;
        if square.content_tag() != ContentTag::Codepoint {
            return Some(" ".to_string());
        }
        let mut text = String::new();
        text.push(display_char(square.c()));
        if let Some(extra) = square.extras_id().and_then(|id| self.zero_width.get(&id)) {
            text.extend(extra.iter().copied().map(display_char));
        }
        Some(text)
    }

    pub fn is_selected(&self, row: usize, col: usize) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let Some(square) = self.cell(row, col) else {
            return false;
        };
        let pos = Pos::new(Line(row as i32 - self.display_offset as i32), Column(col));
        selection.contains_square(
            &Indexed { pos, square },
            self.cursor_state.pos,
            self.cursor_state.content,
        )
    }

    pub fn text(&self) -> String {
        self.visible_rows
            .iter()
            .enumerate()
            .map(|row| {
                let (row_index, row) = row;
                let text = row
                    .inner
                    .iter()
                    .take(self.cols)
                    .enumerate()
                    .filter(|(_, square)| square.wide() != Wide::Spacer)
                    .filter_map(|(col, _)| self.cell_text(row_index, col))
                    .collect::<String>();
                text.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn display_char(ch: char) -> char {
    if ch == '\0' { ' ' } else { ch }
}

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

#[cfg(feature = "pty")]
pub struct Terminal {
    pty: PtyHandle,
    emulator: GridEmulator,
    notifications: async_channel::Sender<TermEvent>,
    events: async_channel::Receiver<TermEvent>,
}

#[cfg(feature = "pty")]
impl Terminal {
    /// Spawn the platform's default interactive shell in `cwd`.
    pub fn spawn(cwd: impl AsRef<Path>) -> io::Result<Self> {
        Self::from_pty(PtyHandle::spawn(cwd)?)
    }

    #[cfg(test)]
    fn spawn_command(
        cwd: impl AsRef<Path>,
        program: String,
        args: Vec<String>,
        shell_name: String,
    ) -> io::Result<Self> {
        Self::from_pty(PtyHandle::spawn_command(cwd, program, args, shell_name)?)
    }

    fn from_pty(pty: PtyHandle) -> io::Result<Self> {
        let emulator = GridEmulator::with_size_and_title(DEFAULT_COLS, DEFAULT_ROWS, pty.label());
        Self::from_parts(pty, emulator, None)
    }

    pub fn grid(&self) -> &GridEmulator {
        &self.emulator
    }

    /// Spawn with a single raw-output consumer, installed before any output is read.
    pub fn spawn_with_output(
        cwd: impl AsRef<Path>,
    ) -> io::Result<(Self, async_channel::Receiver<Vec<u8>>)> {
        let pty = PtyHandle::spawn(cwd)?;
        let emulator = GridEmulator::with_size_and_title(DEFAULT_COLS, DEFAULT_ROWS, pty.label());
        let (tx, rx) = async_channel::unbounded();
        Ok((Self::from_parts(pty, emulator, Some(tx))?, rx))
    }

    fn from_parts(
        pty: PtyHandle,
        emulator: GridEmulator,
        output: Option<async_channel::Sender<Vec<u8>>>,
    ) -> io::Result<Self> {
        emulator.set_fallback_title(pty.label());

        let (notifications, events) = async_channel::unbounded();
        let pty_events = pty.events();
        let output_emulator = emulator.clone();
        let pty_notifications = notifications.clone();
        thread::Builder::new()
            .name("tcode-terminal-output-bridge".into())
            .spawn(move || {
                while let Ok(event) = pty_events.recv_blocking() {
                    match event {
                        PtyEvent::Output(bytes) => {
                            output_emulator.feed(&bytes);
                            if let Some(output) = &output {
                                let _ = output.try_send(bytes);
                            }
                        }
                        PtyEvent::ProcessInfoChanged { name, .. } => {
                            output_emulator.set_fallback_title(name);
                            let _ = pty_notifications.try_send(TermEvent::Wakeup);
                        }
                        PtyEvent::Exited { exit_code } => {
                            output_emulator.set_exited(exit_code);
                            if let Some(output) = &output {
                                let _ = output.try_send(Vec::new());
                            }
                            let _ = pty_notifications.try_send(TermEvent::Exited);
                            let _ = pty_notifications.try_send(TermEvent::Wakeup);
                        }
                    }
                }
            })?;

        let grid_events = emulator.events();
        let writer = pty.writer();
        let grid_notifications = notifications.clone();
        thread::Builder::new()
            .name("tcode-terminal-grid-events".into())
            .spawn(move || {
                while let Ok(event) = grid_events.recv_blocking() {
                    let notification = match event {
                        GridEvent::Wakeup => Some(TermEvent::Wakeup),
                        GridEvent::TitleChanged(_) | GridEvent::CursorBlinkingChanged => {
                            Some(TermEvent::Wakeup)
                        }
                        GridEvent::Bell => Some(TermEvent::Bell),
                        GridEvent::Input(bytes) => {
                            writer.write_raw(bytes);
                            None
                        }
                        GridEvent::ClipboardStore { kind, text } => {
                            Some(TermEvent::ClipboardStore { kind, text })
                        }
                    };
                    if let Some(notification) = notification {
                        let _ = grid_notifications.try_send(notification);
                    }
                }
            })?;

        Ok(Self {
            pty,
            emulator,
            notifications,
            events,
        })
    }

    pub fn cwd(&self) -> &Path {
        self.pty.cwd()
    }

    pub fn working_directory(&self) -> PathBuf {
        self.pty.working_directory()
    }

    /// Return a receiver for rendering-relevant terminal events.
    ///
    /// Cloned receivers compete for events; callers should create one draining
    /// task per terminal and fan notifications out from there when necessary.
    pub fn events(&self) -> async_channel::Receiver<TermEvent> {
        self.events.clone()
    }

    pub fn label(&self) -> String {
        self.emulator
            .osc_title()
            .unwrap_or_else(|| self.pty.label())
    }

    pub fn write_input(&self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        self.emulator.prepare_input();
        if self.pty.write_input_inner(bytes).unwrap_or(false) {
            let _ = self.notifications.try_send(TermEvent::Wakeup);
        }
    }

    /// Send terminal protocol bytes to the PTY without changing viewport or selection state.
    pub fn write_raw(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.pty.write_raw(bytes);
    }

    pub fn resize(&self, cols: usize, rows: usize) {
        if self.emulator.resize_if_changed(cols, rows) {
            let _ = self.pty.resize(self.emulator.window_size());
        }
    }

    /// Resize the grid and update its physical cell metrics as one operation.
    pub fn resize_with_cell_size(&self, cols: usize, rows: usize, width_px: u32, height_px: u32) {
        if self
            .emulator
            .resize_with_cell_size_if_changed(cols, rows, width_px, height_px)
        {
            let _ = self.pty.resize(self.emulator.window_size());
        }
    }

    pub fn kill(&self) {
        let _ = self.pty.kill();
    }

    pub fn scroll(&self, lines: i32) {
        self.emulator.scroll(lines);
    }

    pub fn select(&self, start: (usize, usize), end: (usize, usize)) {
        self.emulator.select(start, end);
    }

    pub fn start_selection(&self, kind: SelectionKind, point: (usize, usize), side: SelectionSide) {
        self.emulator.start_selection(kind, point, side);
    }

    pub fn update_selection(&self, point: (usize, usize), side: SelectionSide) {
        self.emulator.update_selection(point, side);
    }

    pub fn clear_selection(&self) {
        self.emulator.clear_selection();
    }

    pub fn select_all(&self) {
        self.emulator.select_all();
    }

    pub fn clear(&self) {
        self.emulator.clear();
    }

    pub fn selected_text(&self) -> Option<SelectedText> {
        self.emulator.selected_text()
    }

    /// Read terminal modes without consuming renderer damage.
    pub fn mode(&self) -> Mode {
        self.emulator.mode()
    }

    /// Read kitty keyboard protocol flags without consuming renderer damage.
    pub fn keyboard_mode(&self) -> KeyboardModes {
        self.emulator.keyboard_mode()
    }

    /// Read the active xterm modifyOtherKeys level without consuming damage.
    pub fn modify_other_keys(&self) -> Option<u8> {
        self.emulator.modify_other_keys()
    }

    /// Read scrollback size without consuming renderer damage.
    pub fn history_size(&self) -> usize {
        self.emulator.history_size()
    }

    /// A counter that grows with every input and output, for telling whether
    /// the terminal was used between two reads.
    pub fn activity_mark(&self) -> u64 {
        self.pty.activity_mark()
    }

    /// Inspect the process tree now; blocking. `Some(true)` only when the
    /// shell itself is in the foreground and has no child process.
    pub fn idle_shell(&self) -> Option<bool> {
        self.pty.idle_shell()
    }

    /// Read host process exit state without consuming renderer damage.
    pub fn exited(&self) -> bool {
        self.pty.exited()
    }

    pub fn snapshot(&self) -> TermSnapshot {
        self.snapshot_since(0, 0)
    }

    /// See [`GridEmulator::snapshot_since`].
    pub fn snapshot_since(&self, scrolled_before: u64, budget: usize) -> TermSnapshot {
        let mut snapshot = self.emulator.snapshot_since(scrolled_before, budget);
        snapshot.title = self.label();
        snapshot
    }

    /// See [`GridEmulator::peek_snapshot`].
    pub fn peek_snapshot(&self) -> TermSnapshot {
        let mut snapshot = self.emulator.peek_snapshot();
        snapshot.title = self.label();
        snapshot
    }

    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<HyperlinkMatch> {
        self.emulator.hyperlink_at(row, col)
    }
}

#[cfg(all(test, feature = "pty"))]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    #[cfg(target_os = "linux")]
    use crate::pty::unix_shell;

    fn command(script: &str) -> Terminal {
        #[cfg(windows)]
        let (program, args, name) = (
            "cmd.exe".to_string(),
            vec!["/c".to_string(), script.to_string()],
            "cmd".to_string(),
        );
        #[cfg(not(windows))]
        let (program, args, name) = (
            "/bin/sh".to_string(),
            vec!["-c".to_string(), script.to_string()],
            "sh".to_string(),
        );
        Terminal::spawn_command(std::env::temp_dir(), program, args, name).unwrap()
    }

    fn wait_until(terminal: &Terminal, predicate: impl Fn(&TermSnapshot) -> bool) -> TermSnapshot {
        let start = Instant::now();
        loop {
            let state = terminal.snapshot();
            if predicate(&state) {
                return state;
            }
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "terminal timed out: {:?}",
                state.text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_shell_resolves_explicit_paths_and_blank_fallbacks() {
        assert_eq!(unix_shell(Some("/usr/bin/fish")), "/usr/bin/fish");
        assert_eq!(unix_shell(None), "/bin/sh");
        assert_eq!(unix_shell(Some("")), "/bin/sh");
        assert_eq!(unix_shell(Some("  \t")), "/bin/sh");
    }

    #[cfg(unix)]
    #[test]
    fn interactive_pty_delivers_input_resize_and_final_output_before_exit() {
        let terminal = command(
            "stty -echo; printf 'ready\\n'; read line; stty size; printf 'received:%s\\n' \"$line\"; read proceed; i=1; while [ \"$i\" -le 5000 ]; do printf 'output-%s\\n' \"$i\"; i=$((i + 1)); done; exit 37",
        );
        wait_until(&terminal, |state| state.text().contains("ready"));
        terminal.resize(42, 9);
        terminal.write_input(b"tcode-term-ok\r".to_vec());
        let response = wait_until(&terminal, |state| {
            state.text().contains("received:tcode-term-ok")
        });
        assert!(response.text().contains("9 42"), "{}", response.text());
        terminal.write_input(b"continue\r".to_vec());
        let state = wait_until(&terminal, |state| state.exited);
        assert_eq!(state.exit_code, Some(37), "{}", state.text());
        assert_eq!((state.cols, state.screen_lines), (42, 9));
        assert!(state.text().contains("output-5000"), "{}", state.text());
        terminal.scroll(i32::MAX);
        let beginning = terminal.snapshot();
        assert!(beginning.display_offset > 0);
        assert!(!beginning.text().contains("output-5000"));
    }

    #[cfg(unix)]
    #[test]
    fn pty_kill_emits_exit_data_event() {
        let pty = PtyHandle::spawn_command(
            std::env::temp_dir(),
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "sleep 10".to_string()],
            "sh".to_string(),
        )
        .unwrap();
        let events = pty.events();
        pty.kill().unwrap();
        let start = Instant::now();
        loop {
            match events.try_recv() {
                Ok(PtyEvent::Exited { exit_code }) => {
                    assert_eq!(exit_code, None, "SIGHUP is not a normal exit code");
                    break;
                }
                Ok(PtyEvent::Output(_) | PtyEvent::ProcessInfoChanged { .. }) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(
                        start.elapsed() < Duration::from_secs(120),
                        "PTY did not emit an exit event after kill"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("PTY event stream closed before kill exit")
                }
            }
        }
        assert!(pty.exited());
    }

    #[cfg(unix)]
    #[test]
    fn compatibility_events_cover_consumed_intents() {
        let terminal = command("printf '\\033]2;wire-title\\007\\007\\033]52;c;dGNvZGU=\\007'");
        let events = terminal.events();
        let start = Instant::now();
        let mut emitted = Vec::new();
        while !emitted.contains(&TermEvent::Bell)
            || !emitted.contains(&TermEvent::Exited)
            || !emitted.contains(&TermEvent::Wakeup)
            || !emitted.contains(&TermEvent::ClipboardStore {
                kind: ClipboardType::Clipboard,
                text: "tcode".into(),
            })
        {
            match events.try_recv() {
                Ok(event) => emitted.push(event),
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(
                        start.elapsed() < Duration::from_secs(120),
                        "missing compatibility events: {emitted:?}"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("compatibility event stream closed: {emitted:?}")
                }
            }
        }
        let state = terminal.snapshot();
        assert_eq!(state.title, "wire-title");
        assert!(state.exited);
    }

    #[cfg(unix)]
    #[test]
    fn forwards_primary_device_attribute_response_to_pty() {
        let terminal = command(
            "saved=$(stty -g); stty raw -echo; printf '\\033[c'; response=$(dd bs=1 count=16 2>/dev/null); stty \"$saved\"; printf '%s' \"$response\" | od -An -tx1; printf '\\n'",
        );
        let state = wait_until(&terminal, |state| {
            let text = state.text();
            let fields = text.split_whitespace().collect::<Vec<_>>();
            state.exited
                && fields.windows(16).any(|window| {
                    window
                        == [
                            "1b", "5b", "3f", "36", "32", "3b", "34", "3b", "36", "3b", "32", "32",
                            "3b", "35", "32", "63",
                        ]
                })
        });
        assert_eq!(state.exit_code, Some(0));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "queries a real foreground PTY process group"]
    fn tracks_real_pty_foreground_cwd() {
        let terminal = Terminal::spawn_command(
            PathBuf::from("/"),
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "stty -echo; printf 'ready\\n'; read proceed; cd /tmp; printf 'moved\\n'; read finish"
                    .into(),
            ],
            "sh".into(),
        )
        .unwrap();
        wait_until(&terminal, |state| state.text().contains("ready"));
        assert_eq!(terminal.working_directory(), PathBuf::from("/"));
        terminal.write_input(b"continue\r".to_vec());
        wait_until(&terminal, |state| state.text().contains("moved"));
        let expected = std::fs::canonicalize("/tmp").unwrap();
        let start = Instant::now();
        while terminal.working_directory() != expected {
            assert!(start.elapsed() < Duration::from_secs(10));
            thread::sleep(Duration::from_millis(50));
        }
        terminal.write_input(b"finish\r".to_vec());
        assert_eq!(
            wait_until(&terminal, |state| state.exited).exit_code,
            Some(0)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_invalid_spawn_without_hanging() {
        let executable = std::env::current_exe().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for (cwd, program) in [
                (executable.clone(), "cmd.exe".to_string()),
                (executable.join("missing-project"), "cmd.exe".to_string()),
                (
                    std::env::temp_dir(),
                    executable
                        .join("missing-shell.exe")
                        .to_string_lossy()
                        .into_owned(),
                ),
            ] {
                let result = Terminal::spawn_command(cwd, program, vec![], "cmd".into());
                sender.send(result.is_err()).unwrap();
            }
        });
        for _ in 0..3 {
            assert!(
                receiver
                    .recv_timeout(Duration::from_secs(10))
                    .expect("PTY spawn did not return")
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_kills_child_with_pending_input() {
        let terminal = command("echo tcode-ready & ping -n 30 127.0.0.1 >nul");
        wait_until(&terminal, |state| state.text().contains("tcode-ready"));
        terminal.write_input(vec![b'x'; 1024 * 1024]);
        terminal.kill();
        let state = wait_until(&terminal, |state| state.exited);
        assert_eq!(state.exit_code, Some(1));
    }
}
