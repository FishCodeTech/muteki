//! The expanded command block: the command line, then its output.
//!
//! No client owns a terminal emulator: the output is the screen the host
//! renders at the width this panel subscribed to, pushed again while the
//! command runs. Both parts paint as terminal grids.

use std::{collections::HashMap, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, App, ContentMask, Hsla, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Rgba, Styled as _, Window, canvas, div, prelude::FluentBuilder as _, px,
};
use gpui_base::ElementExt as _;
use tcode_core::session::TimelineEntry;
use tcode_protocol::{
    STORED_OUTPUT_COLS,
    terminal::{CellWidth, TerminalCell, TerminalFrame, TerminalRow, TerminalStyle},
};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::highlight;
use crate::store::terminal::TerminalModel;
use crate::terminal_drawer::{
    TERMINAL_CELL_HEIGHT, TERMINAL_CELL_WIDTH, TerminalPalette, layout_grid, paint_terminal_grid,
};
use crate::theme::{ActiveTheme as _, HighlightTheme};

const DEFAULT_COLS: u16 = 80;
/// Grid rows one panel shows, command line included.
const MAX_ROWS: usize = 16;
const MAX_COMMAND_ROWS: usize = 4;

pub(crate) type ColsChangeHandler = Box<dyn Fn(&u16, &mut Window, &mut App) + 'static>;

pub(crate) fn clamp_cols(cols: u16) -> u16 {
    cols.clamp(*STORED_OUTPUT_COLS.start(), *STORED_OUTPUT_COLS.end())
}

/// The host's screen for one command, as far as the panel has it.
#[derive(Clone, Copy, Default)]
pub(crate) struct OutputScreen<'a> {
    /// The width subscribed to, once the panel has measured itself.
    pub(crate) watched: Option<u16>,
    /// The latest screen, possibly for an earlier width.
    pub(crate) frame: Option<&'a Rc<TerminalFrame>>,
}

/// What changed about the panels since the last frame was painted.
#[derive(Default)]
pub(crate) struct PanelChanges {
    /// Panels no longer laid out: collapsed, scrolled away or gone.
    pub(crate) unmounted: Vec<String>,
    /// Panels whose height changed, with their turn.
    pub(crate) resized: Vec<(usize, String)>,
}

impl PanelChanges {
    pub(crate) fn is_empty(&self) -> bool {
        self.unmounted.is_empty() && self.resized.is_empty()
    }
}

pub(crate) struct CommandPanelCache {
    entries: HashMap<String, Panel>,
}

impl CommandPanelCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn render(
        &mut self,
        entry: &TimelineEntry,
        command: &str,
        output: &str,
        screen: OutputScreen<'_>,
        on_cols_change: Option<ColsChangeHandler>,
        cx: &App,
    ) -> AnyElement {
        let id = entry.id.as_str();
        let panel = self.entries.entry(entry.id.clone()).or_default();
        panel.turn = entry.turn;
        panel.rendered = true;
        if panel.command != command {
            panel.command = command.to_string();
            panel.command_model = None;
        }
        panel.render(id, output, screen, on_cols_change, cx)
    }

    /// Call once per painted frame, after the rows were laid out.
    pub(crate) fn settle(&mut self) -> PanelChanges {
        let mut changes = PanelChanges::default();
        self.entries.retain(|id, panel| {
            if !std::mem::take(&mut panel.rendered) {
                changes.unmounted.push(id.clone());
                return false;
            }
            if std::mem::take(&mut panel.resized) {
                changes.resized.push((panel.turn, id.clone()));
            }
            true
        });
        changes
    }
}

#[derive(Clone)]
struct CommandTheme {
    foreground: Hsla,
    background: Hsla,
    highlight_theme: Arc<HighlightTheme>,
}

impl CommandTheme {
    fn matches(&self, other: &Self) -> bool {
        self.foreground == other.foreground
            && self.background == other.background
            && Arc::ptr_eq(&self.highlight_theme, &other.highlight_theme)
    }
}

struct OutputModel {
    frame: Rc<TerminalFrame>,
    budget: usize,
    model: TerminalModel,
    rows: usize,
}

#[derive(Default)]
struct Panel {
    turn: usize,
    rendered: bool,
    command: String,
    /// Rendered lazily so the first paint already carries the theme colours.
    command_model: Option<(TerminalModel, usize, u16)>,
    command_theme: Option<CommandTheme>,
    output_model: Option<OutputModel>,
    shown_rows: Option<usize>,
    resized: bool,
}

impl Panel {
    fn render(
        &mut self,
        id: &str,
        output: &str,
        screen: OutputScreen<'_>,
        on_cols_change: Option<ColsChangeHandler>,
        cx: &App,
    ) -> AnyElement {
        let theme = CommandTheme {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            highlight_theme: cx.theme().highlight_theme.clone(),
        };
        if !self
            .command_theme
            .as_ref()
            .is_some_and(|cached| cached.matches(&theme))
        {
            self.command_model = None;
            self.command_theme = Some(theme);
        }
        let cols = screen
            .watched
            .or(screen.frame.map(|frame| frame.cols))
            .unwrap_or(DEFAULT_COLS);
        // The command line wraps at the same width as the output.
        if self
            .command_model
            .as_ref()
            .is_none_or(|(_, _, built)| *built != cols)
        {
            let (model, rows) = command_model(&self.command, cols, self.command_theme.as_ref());
            self.command_model = Some((model, rows, cols));
        }
        let (command_model, command_rows, _) = self.command_model.as_ref().expect("command model");
        let command_rows = *command_rows;
        // Rows the output may use: the panel's budget, less the command line.
        let budget = MAX_ROWS.saturating_sub(command_rows).max(1);

        let palette = TerminalPalette {
            foreground: cx.theme().foreground,
            background: cx.theme().background,
            selection: cx.theme().primary.opacity(0.28),
        };
        let command = grid_element(command_model, command_rows, cols, palette);
        let (output, output_rows) =
            match screen.frame {
                Some(frame) => {
                    if !self.output_model.as_ref().is_some_and(|built| {
                        Rc::ptr_eq(&built.frame, frame) && built.budget == budget
                    }) {
                        let (model, rows) = output_model(frame, budget);
                        self.output_model = Some(OutputModel {
                            frame: frame.clone(),
                            budget,
                            model,
                            rows,
                        });
                    }
                    let built = self.output_model.as_ref().expect("output model");
                    (
                        grid_element(&built.model, built.rows, built.frame.cols, palette),
                        built.rows,
                    )
                }
                // Space for the rows the output is likely to need, so the screen
                // usually lands without moving what follows.
                None => {
                    let rows = placeholder_rows(output, budget);
                    (
                        div()
                            .w_full()
                            .h(px(rows as f32 * TERMINAL_CELL_HEIGHT))
                            .into_any_element(),
                        rows,
                    )
                }
            };
        let rows = command_rows + output_rows;
        self.resized |= self.shown_rows.is_some_and(|shown| shown != rows);
        self.shown_rows = Some(rows);

        let watched = screen.watched;
        let output_id = id.to_string();
        div()
            .w_full()
            .overflow_hidden()
            .bg(palette.background)
            .child(command)
            .when(output_rows > 0, |panel| {
                panel.child(
                    div()
                        .debug_selector(move || format!("command-output-{output_id}"))
                        .overflow_hidden()
                        .child(output),
                )
            })
            .when_some(on_cols_change, |panel, on_cols_change| {
                panel.on_prepaint(move |bounds, window, cx| {
                    let measured = clamp_cols(
                        (f32::from(bounds.size.width) / TERMINAL_CELL_WIDTH)
                            .floor()
                            .clamp(0., f32::from(u16::MAX)) as u16,
                    );
                    if watched != Some(measured) {
                        on_cols_change(&measured, window, cx);
                    }
                })
            })
            .into_any_element()
    }
}

/// The host renders a full screen; the panel shows its last `budget` rows.
fn output_model(frame: &TerminalFrame, budget: usize) -> (TerminalModel, usize) {
    let mut frame = frame.clone();
    let overflow = frame.visible.len().saturating_sub(budget);
    frame.visible.drain(..overflow);
    frame.rows = frame.visible.len().min(u16::MAX as usize) as u16;
    let rows = frame.visible.len();
    let mut model = TerminalModel::new(String::new());
    model.apply_frame(frame);
    (model, rows)
}

/// Roughly the rows the host's screen of `output` will take: one per line of
/// its tail.
fn placeholder_rows(output: &str, budget: usize) -> usize {
    let trimmed = output.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return 0;
    }
    trimmed.rsplit('\n').take(budget).count()
}

fn grid_element(
    model: &TerminalModel,
    rows: usize,
    cols: u16,
    palette: TerminalPalette,
) -> AnyElement {
    let paint_data = layout_grid(model, palette, false, None, false, false);
    canvas(
        |_bounds, _window, _cx| (),
        move |bounds, (), window, cx| {
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                paint_terminal_grid(
                    bounds,
                    window,
                    cx,
                    &paint_data,
                    palette,
                    TERMINAL_CELL_WIDTH,
                    TERMINAL_CELL_HEIGHT,
                    false,
                );
            });
        },
    )
    .w(px(f32::from(cols) * TERMINAL_CELL_WIDTH))
    .h(px(rows as f32 * TERMINAL_CELL_HEIGHT))
    .into_any_element()
}

/// Lay the command out into grid cells so it shares the output's metrics.
///
/// The command is the client's own text, not captured terminal output: it
/// carries no escape sequences, and its colours come from this client's syntax
/// theme, which the host has no business knowing.
fn command_model(
    command: &str,
    cols: u16,
    command_theme: Option<&CommandTheme>,
) -> (TerminalModel, usize) {
    let cols = cols.max(2);
    let command = clamp_command(command, cols);
    let highlights = command_theme
        .map(|theme| highlight::highlight_source(&command, "bash", &theme.highlight_theme))
        .unwrap_or_default();

    let mut styles = vec![TerminalStyle::default()];
    let mut visible = vec![TerminalRow::default()];
    let mut col = 0usize;
    for (offset, text) in command.grapheme_indices(true) {
        if text == "\n" {
            visible.push(TerminalRow::default());
            col = 0;
            continue;
        }
        let width = text.width().min(2);
        if width == 0 {
            continue;
        }
        if col + width > usize::from(cols) {
            visible.push(TerminalRow::default());
            col = 0;
        }
        let style = command_theme.map_or_else(TerminalStyle::default, |theme| {
            let color = highlights
                .iter()
                .find(|(range, _)| range.contains(&offset))
                .and_then(|(_, style)| style.color)
                .unwrap_or(theme.foreground);
            faded_command_style(color, theme.background)
        });
        let style = match styles.iter().position(|existing| *existing == style) {
            Some(index) => index as u16,
            None => {
                styles.push(style);
                (styles.len() - 1) as u16
            }
        };
        let cells = &mut visible.last_mut().expect("a row is always open").cells;
        cells.push(TerminalCell {
            text: text.to_string(),
            style,
            width: if width == 2 {
                CellWidth::Wide
            } else {
                CellWidth::Narrow
            },
            ..TerminalCell::default()
        });
        if width == 2 {
            cells.push(TerminalCell {
                style,
                width: CellWidth::Spacer,
                ..TerminalCell::default()
            });
        }
        col += width;
    }

    let rows = visible.len().max(1);
    let mut model = TerminalModel::new(String::new());
    model.apply_frame(TerminalFrame {
        cols,
        rows: rows.min(u16::MAX as usize) as u16,
        styles,
        visible,
        ..TerminalFrame::default()
    });
    (model, rows)
}

/// The command reads as context, not as output, so it sits at 70% contrast.
fn faded_command_style(foreground: Hsla, background: Hsla) -> TerminalStyle {
    let (r, g, b) = faded_command_rgb(foreground, background);
    TerminalStyle {
        fg: tcode_protocol::terminal::TerminalColor::Rgb { r, g, b },
        ..TerminalStyle::default()
    }
}

fn faded_command_rgb(foreground: Hsla, background: Hsla) -> (u8, u8, u8) {
    let faded = Rgba::from(background).blend(Rgba::from(foreground).opacity(0.7));
    (
        (faded.r * 255.) as u8,
        (faded.g * 255.) as u8,
        (faded.b * 255.) as u8,
    )
}

fn clamp_command(command: &str, cols: u16) -> String {
    let cols = usize::from(cols.max(2));
    let command = command.replace('\r', "");
    let mut result = String::new();
    let mut row = 0;
    let mut col = 0;
    for text in command.graphemes(true) {
        let width = text.width().min(2);
        let next_row = text == "\n" || col + width > cols;
        if next_row && row + 1 == MAX_COMMAND_ROWS {
            // Make room on the last visible row without splitting a grapheme
            // or putting the ellipsis onto a fifth row.
            if col == cols {
                let (offset, _) = result.grapheme_indices(true).next_back().unwrap();
                result.truncate(offset);
            }
            result.push('…');
            break;
        }
        if next_row {
            row += 1;
            col = 0;
        }
        result.push_str(text);
        if text != "\n" {
            col += width;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use gpui::rgb;

    use super::*;

    fn palette() -> TerminalPalette {
        TerminalPalette {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            selection: rgb(0x333333).into(),
        }
    }

    fn dark_command_theme() -> CommandTheme {
        CommandTheme {
            foreground: rgb(0xffffff).into(),
            background: rgb(0x000000).into(),
            highlight_theme: HighlightTheme::default_dark(),
        }
    }

    fn frame(cols: u16, lines: &[&str]) -> TerminalFrame {
        TerminalFrame {
            cols,
            rows: lines.len() as u16,
            visible: lines
                .iter()
                .map(|line| TerminalRow {
                    cells: line
                        .chars()
                        .map(|ch| TerminalCell {
                            text: ch.to_string(),
                            ..TerminalCell::default()
                        })
                        .collect(),
                    wrapped: false,
                })
                .collect(),
            ..TerminalFrame::default()
        }
    }

    #[test]
    fn command_cells_preserve_graphemes_and_cell_width_with_bounded_wrapping() {
        {
            let command = "x".repeat(usize::from(DEFAULT_COLS) * MAX_COMMAND_ROWS + 1);
            let (model, rows) = command_model(&command, DEFAULT_COLS, Some(&dark_command_theme()));
            assert_eq!(rows, MAX_COMMAND_ROWS);
            assert!(
                model
                    .cell(MAX_COMMAND_ROWS - 1, usize::from(DEFAULT_COLS) - 1)
                    .is_some_and(|cell| cell.text == "…")
            );
        }
        {
            let (model, rows) = command_model("a中文b", 8, Some(&dark_command_theme()));
            assert_eq!(rows, 1);
            for (col, text, width) in [
                (0, "a", CellWidth::Narrow),
                (1, "中", CellWidth::Wide),
                (2, "", CellWidth::Spacer),
                (3, "文", CellWidth::Wide),
                (4, "", CellWidth::Spacer),
                (5, "b", CellWidth::Narrow),
            ] {
                let cell = model.cell(0, col).expect("command cell");
                assert_eq!((cell.text.as_str(), cell.width), (text, width));
            }
            let paint = layout_grid(&model, palette(), false, None, false, false);
            assert!(paint.text_runs.iter().any(|run| run.start_col == 3));
            assert!(paint.text_runs.iter().any(|run| run.start_col == 5));
        }
        {
            let (model, rows) = command_model("abc中文e\u{301}👩‍💻z", 4, None);
            assert_eq!(rows, 3);
            for (row, col, text) in [
                (0, 2, "c"),
                (1, 0, "中"),
                (1, 2, "文"),
                (2, 0, "e\u{301}"),
                (2, 1, "👩‍💻"),
                (2, 3, "z"),
            ] {
                assert_eq!(model.cell(row, col).unwrap().text, text);
            }

            for command in ["中文".repeat(5), "中文\n".repeat(5)] {
                let (model, rows) = command_model(&command, 4, None);
                assert_eq!(rows, MAX_COMMAND_ROWS);
                assert_eq!(model.cell(3, 0).unwrap().text, "中");
                assert_eq!(model.cell(3, 2).unwrap().text, "…");
                assert!(model.cell(3, 3).is_none_or(|cell| cell.text.is_empty()));
            }
        }
        {
            let (model, rows) = command_model("abcdefghij", 5, None);
            assert_eq!(rows, 2);
            assert_eq!(
                model.cell(1, 0).map(|cell| cell.text.clone()),
                Some("f".into())
            );
        }
    }

    /// Long output keeps its tail: the panel is a window onto the end of a run.
    #[test]
    fn output_shows_the_last_rows_that_fit() {
        let lines = ["one", "two", "three", "four"];
        for (budget, expected) in [
            (0, vec![]),
            (1, vec!["four"]),
            (2, vec!["three", "four"]),
            (8, vec!["one", "two", "three", "four"]),
        ] {
            let (model, rows) = output_model(&frame(40, &lines), budget);
            let actual = (0..rows)
                .map(|row| {
                    model
                        .row(row)
                        .unwrap()
                        .cells
                        .iter()
                        .map(|cell| cell.text.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "budget {budget}");
        }
    }
}
