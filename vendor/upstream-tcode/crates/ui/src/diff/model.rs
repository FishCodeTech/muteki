use std::ops::Range;

use crate::highlight::HighlightTheme;
use agent::FileChangeKind;
use gpui::{HighlightStyle, Hsla};
use tcode_core::session::ReviewSide;

use super::algorithm::line_diff;
use super::parse::{RowKind, is_binary_patch, parse_hunk_header, parse_unified_diff};
use super::rows::pair_rows;
use crate::highlight;
use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone)]
pub struct DiffColors {
    pub added_word_bg: Hsla,
    pub removed_word_bg: Hsla,
}

#[derive(Debug, Clone)]
pub struct RenderedRow {
    pub kind: RowKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
    pub source_text: String,
    pub original: Option<OriginalLine>,
    pub runs: Vec<(Range<usize>, HighlightStyle)>,
}

#[derive(Debug, Clone)]
pub struct OriginalLine {
    pub text: String,
    pub source_text: String,
    pub runs: Vec<(Range<usize>, HighlightStyle)>,
}

impl RenderedRow {
    pub(crate) fn source(&self, side: ReviewSide) -> &str {
        if side == ReviewSide::Old
            && let Some(original) = &self.original
        {
            &original.source_text
        } else {
            &self.source_text
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileNotice {
    Binary,
    Empty,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairedRow {
    pub left: Option<usize>,
    pub right: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RenderedFile {
    pub path: String,
    pub kind: FileChangeKind,
    pub added: u64,
    pub removed: u64,
    pub all_rows: Vec<RenderedRow>,
    pub hunks: Vec<RenderedHunk>,
    pub expanded: Vec<Range<usize>>,
    pub expandable: bool,
    pub pairs: Vec<PairedRow>,
    pub notice: Option<FileNotice>,
}

pub struct FileDiffInput<'a> {
    pub path: &'a str,
    pub kind: FileChangeKind,
    pub old_text: Option<&'a str>,
    pub new_text: Option<&'a str>,
    pub patch: Option<&'a str>,
    pub ignore_whitespace: bool,
    pub show_invisibles: bool,
}

#[derive(Debug, Clone)]
pub struct RenderedHunk {
    pub header: String,
    pub pairs: Range<usize>,
    pub omitted_before: Range<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandDir {
    Up,
    Down,
    All,
}

#[derive(Clone, Copy)]
struct SourceRange {
    start: usize,
    end: usize,
}

struct TextLine<'a> {
    text: &'a str,
    source: SourceRange,
}

fn text_lines(text: &str) -> Vec<TextLine<'_>> {
    let mut offset = 0;
    imara_diff::sources::lines(text)
        .map(|line| {
            let content = line.strip_suffix('\n').unwrap_or(line);
            let content = content.strip_suffix('\r').unwrap_or(content);
            let result = TextLine {
                text: content,
                source: SourceRange {
                    start: offset,
                    end: offset + content.len(),
                },
            };
            offset += line.len();
            result
        })
        .collect()
}

pub fn reconstruct_from_text(new_text: String, patch: &str) -> Option<(String, String)> {
    let old_text = if patch.lines().any(|line| parse_hunk_header(line).is_some()) {
        reconstruct_unified(&new_text, patch)?
    } else {
        reconstruct_bare(&new_text, patch)?
    };
    Some((old_text, new_text))
}

fn reconstruct_unified(new_text: &str, patch: &str) -> Option<String> {
    let parsed = parse_unified_diff(patch);
    let new_starts = patch
        .lines()
        .filter_map(parse_hunk_header)
        .map(|(_, new_start)| new_start)
        .collect::<Vec<_>>();
    if parsed.hunks.len() != new_starts.len() {
        return None;
    }

    let new_lines = text_lines(new_text)
        .into_iter()
        .map(|line| line.text.to_string())
        .collect::<Vec<_>>();
    for (hunk, new_start) in parsed.hunks.iter().zip(&new_starts) {
        let expected = hunk
            .rows
            .iter()
            .filter(|row| matches!(row.kind, RowKind::Context | RowKind::Added))
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>();
        let start = unified_new_index(*new_start, expected.len());
        let end = start.checked_add(expected.len())?;
        if end > new_lines.len()
            || new_lines[start..end]
                .iter()
                .map(String::as_str)
                .ne(expected)
        {
            return None;
        }
    }

    let mut old_lines = new_lines;
    for (hunk, new_start) in parsed.hunks.iter().zip(&new_starts).rev() {
        let new_len = hunk
            .rows
            .iter()
            .filter(|row| matches!(row.kind, RowKind::Context | RowKind::Added))
            .count();
        let start = unified_new_index(*new_start, new_len);
        let end = start.checked_add(new_len)?;
        if end > old_lines.len() {
            return None;
        }
        let replacement = hunk
            .rows
            .iter()
            .filter(|row| matches!(row.kind, RowKind::Removed | RowKind::Context))
            .map(|row| row.text.clone());
        old_lines.splice(start..end, replacement);
    }
    Some(join_reconstructed_lines(&old_lines, new_text))
}

fn unified_new_index(new_start: u32, new_len: usize) -> usize {
    if new_len == 0 {
        new_start as usize
    } else {
        new_start.saturating_sub(1) as usize
    }
}

fn reconstruct_bare(new_text: &str, patch: &str) -> Option<String> {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for line in patch.lines() {
        if line == r"\ No newline at end of file" {
            continue;
        }
        if let Some(line) = line.strip_prefix('-') {
            removed.push(line.to_string());
        } else if let Some(line) = line.strip_prefix('+') {
            added.push(line.to_string());
        }
    }

    if removed.is_empty() {
        let candidate = format!("{}\n", added.join("\n"));
        return same_except_trailing_newline(&candidate, new_text).then(String::new);
    }
    if added.is_empty() {
        return None;
    }

    let mut new_lines = text_lines(new_text)
        .into_iter()
        .map(|line| line.text.to_string())
        .collect::<Vec<_>>();
    let matches = new_lines
        .windows(added.len())
        .enumerate()
        .filter_map(|(index, window)| (window == added).then_some(index))
        .collect::<Vec<_>>();
    let [start] = matches.as_slice() else {
        return None;
    };
    new_lines.splice(*start..*start + added.len(), removed);
    Some(join_reconstructed_lines(&new_lines, new_text))
}

fn same_except_trailing_newline(left: &str, right: &str) -> bool {
    strip_one_trailing_newline(left) == strip_one_trailing_newline(right)
}

fn strip_one_trailing_newline(text: &str) -> &str {
    let Some(text) = text.strip_suffix('\n') else {
        return text;
    };
    text.strip_suffix('\r').unwrap_or(text)
}

fn join_reconstructed_lines(lines: &[String], template: &str) -> String {
    // No lines is an empty text, not one blank line: a new file's old side.
    if lines.is_empty() {
        return String::new();
    }
    let newline = if template.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut text = lines.join(newline);
    if template.ends_with('\n') {
        text.push_str(newline);
    }
    text
}

pub fn build_file(
    input: &FileDiffInput<'_>,
    display_path: String,
    lang: &str,
    theme: &HighlightTheme,
    colors: &DiffColors,
    whitespace_style: &HighlightStyle,
) -> RenderedFile {
    let display_path = if display_path.is_empty() {
        input.path.to_string()
    } else {
        display_path
    };
    let binary = input.patch.is_some_and(is_binary_patch);
    let mut file = if binary {
        RenderedFile {
            path: display_path,
            kind: input.kind,
            added: 0,
            removed: 0,
            all_rows: Vec::new(),
            hunks: Vec::new(),
            expanded: Vec::new(),
            expandable: false,
            pairs: Vec::new(),
            notice: Some(FileNotice::Binary),
        }
    } else {
        match (input.old_text, input.new_text) {
            (Some(old), Some(new)) => build_from_texts(input, display_path, lang, theme, old, new),
            (None, Some(new)) if input.kind == FileChangeKind::Create => {
                build_from_texts(input, display_path, lang, theme, "", new)
            }
            (Some(old), None) if input.kind == FileChangeKind::Delete => {
                build_from_texts(input, display_path, lang, theme, old, "")
            }
            _ => build_from_patch(input, display_path, lang, theme),
        }
    };
    if file.all_rows.is_empty() && file.notice.is_none() {
        file.notice = Some(if input.old_text.is_some() || input.new_text.is_some() {
            FileNotice::Empty
        } else {
            FileNotice::Unavailable
        });
    }

    apply_word_highlights(&mut file, colors);
    for row in &mut file.all_rows {
        let (text, runs) = display_line(
            &row.source_text,
            &row.runs,
            input.show_invisibles,
            whitespace_style,
        );
        row.text = text;
        row.runs = runs;
        if let Some(original) = &mut row.original {
            let (text, runs) = display_line(
                &original.source_text,
                &original.runs,
                input.show_invisibles,
                whitespace_style,
            );
            original.text = text;
            original.runs = runs;
        }
    }
    file
}

fn build_from_texts(
    input: &FileDiffInput<'_>,
    display_path: String,
    lang: &str,
    theme: &HighlightTheme,
    old: &str,
    new: &str,
) -> RenderedFile {
    let hunks = line_diff(old, new, input.ignore_whitespace);
    let old_lines = text_lines(old);
    let new_lines = text_lines(new);
    let old_styles = highlight::highlight_source(old, lang, theme);
    let new_styles = highlight::highlight_source(new, lang, theme);
    let mut rows = Vec::with_capacity(old_lines.len() + new_lines.len());
    let mut old_cursor = 0usize;
    let mut new_cursor = 0usize;

    for hunk in &hunks {
        while old_cursor < hunk.old.start as usize && new_cursor < hunk.new.start as usize {
            push_context_row(
                &mut rows,
                (&old_lines, &old_styles),
                (&new_lines, &new_styles),
                old_cursor,
                new_cursor,
            );
            old_cursor += 1;
            new_cursor += 1;
        }
        for old_index in hunk.old.clone().map(|line| line as usize) {
            let line = &old_lines[old_index];
            rows.push(RenderedRow {
                kind: RowKind::Removed,
                old: Some(old_index as u32 + 1),
                new: None,
                original: None,
                source_text: line.text.to_string(),
                text: line.text.to_string(),
                runs: sub_runs(&old_styles, line.source.start, line.source.end),
            });
        }
        for new_index in hunk.new.clone().map(|line| line as usize) {
            let line = &new_lines[new_index];
            rows.push(RenderedRow {
                kind: RowKind::Added,
                old: None,
                new: Some(new_index as u32 + 1),
                original: None,
                source_text: line.text.to_string(),
                text: line.text.to_string(),
                runs: sub_runs(&new_styles, line.source.start, line.source.end),
            });
        }
        old_cursor = hunk.old.end as usize;
        new_cursor = hunk.new.end as usize;
    }
    while old_cursor < old_lines.len() && new_cursor < new_lines.len() {
        push_context_row(
            &mut rows,
            (&old_lines, &old_styles),
            (&new_lines, &new_styles),
            old_cursor,
            new_cursor,
        );
        old_cursor += 1;
        new_cursor += 1;
    }
    for (old_index, line) in old_lines.iter().enumerate().skip(old_cursor) {
        rows.push(RenderedRow {
            kind: RowKind::Removed,
            old: Some(old_index as u32 + 1),
            new: None,
            original: None,
            source_text: line.text.to_string(),
            text: line.text.to_string(),
            runs: sub_runs(&old_styles, line.source.start, line.source.end),
        });
    }
    for (new_index, line) in new_lines.iter().enumerate().skip(new_cursor) {
        rows.push(RenderedRow {
            kind: RowKind::Added,
            old: None,
            new: Some(new_index as u32 + 1),
            original: None,
            source_text: line.text.to_string(),
            text: line.text.to_string(),
            runs: sub_runs(&new_styles, line.source.start, line.source.end),
        });
    }

    let pairs = pair_rows(&rows, 0..rows.len());
    let hunks = vec![RenderedHunk {
        header: format!("@@ -1,{} +1,{} @@", old_lines.len(), new_lines.len()),
        pairs: 0..pairs.len(),
        omitted_before: 1..1,
    }];
    RenderedFile {
        path: display_path,
        kind: input.kind,
        added: hunks_count(&rows, RowKind::Added),
        removed: hunks_count(&rows, RowKind::Removed),
        all_rows: rows,
        hunks,
        expanded: Vec::new(),
        expandable: true,
        pairs,
        notice: None,
    }
}

fn hunks_count(rows: &[RenderedRow], kind: RowKind) -> u64 {
    rows.iter().filter(|row| row.kind == kind).count() as u64
}

fn push_context_row(
    rows: &mut Vec<RenderedRow>,
    old: (&[TextLine<'_>], &[(Range<usize>, HighlightStyle)]),
    new: (&[TextLine<'_>], &[(Range<usize>, HighlightStyle)]),
    old_index: usize,
    new_index: usize,
) {
    let line = &new.0[new_index];
    let old_line = &old.0[old_index];
    rows.push(RenderedRow {
        kind: RowKind::Context,
        old: Some(old_index as u32 + 1),
        new: Some(new_index as u32 + 1),
        source_text: line.text.to_string(),
        text: line.text.to_string(),
        original: (old_line.text != line.text).then(|| OriginalLine {
            text: old_line.text.to_string(),
            source_text: old_line.text.to_string(),
            runs: sub_runs(old.1, old_line.source.start, old_line.source.end),
        }),
        runs: sub_runs(new.1, line.source.start, line.source.end),
    });
}

fn build_from_patch(
    input: &FileDiffInput<'_>,
    display_path: String,
    lang: &str,
    theme: &HighlightTheme,
) -> RenderedFile {
    let parsed = input.patch.map(parse_unified_diff).unwrap_or_default();
    let mut all_rows = Vec::new();
    let mut pairs = Vec::new();
    let mut hunks = Vec::new();
    let mut new_cursor = 1;
    for hunk in parsed.hunks {
        let omitted_before = new_cursor..new_cursor + hunk.gap_before;
        new_cursor += hunk.gap_before;
        let mut old_source = String::new();
        let mut new_source = String::new();
        let mut sources = Vec::new();
        for row in &hunk.rows {
            let source = if row.kind == RowKind::Removed {
                &mut old_source
            } else {
                &mut new_source
            };
            let start = source.len();
            source.push_str(&row.text);
            let end = source.len();
            source.push('\n');
            sources.push(start..end);
            if row.kind == RowKind::Context {
                old_source.push_str(&row.text);
                old_source.push('\n');
            }
            if row.new_line.is_some() {
                new_cursor += 1;
            }
        }
        // Missing source between hunks may end a string or comment.
        let old_styles = highlight::highlight_source(&old_source, lang, theme);
        let new_styles = highlight::highlight_source(&new_source, lang, theme);
        let start = all_rows.len();
        for (row, source) in hunk.rows.into_iter().zip(sources) {
            let styles = if row.kind == RowKind::Removed {
                &old_styles
            } else {
                &new_styles
            };
            all_rows.push(RenderedRow {
                kind: row.kind,
                old: row.old_line,
                new: row.new_line,
                original: None,
                source_text: row.text.clone(),
                text: row.text,
                runs: sub_runs(styles, source.start, source.end),
            });
        }
        let pair_start = pairs.len();
        pairs.extend(pair_rows(&all_rows, start..all_rows.len()));
        hunks.push(RenderedHunk {
            header: hunk.header,
            pairs: pair_start..pairs.len(),
            omitted_before,
        });
    }
    RenderedFile {
        path: display_path,
        kind: input.kind,
        added: parsed.added.into(),
        removed: parsed.removed.into(),
        all_rows,
        pairs,
        hunks,
        expanded: Vec::new(),
        expandable: false,
        notice: None,
    }
}

fn apply_word_highlights(file: &mut RenderedFile, colors: &DiffColors) {
    for pair in &file.pairs {
        let (Some(old), Some(new)) = (pair.left, pair.right) else {
            continue;
        };
        if old == new {
            continue;
        }
        let old_text = &file.all_rows[old].text;
        let new_text = &file.all_rows[new].text;
        // Bound the work for generated/minified source; whole-line colors still show the change.
        if old_text.len() > 1000 || new_text.len() > 1000 {
            continue;
        }
        let (old_ranges, new_ranges) = changed_words(old_text, new_text);
        for (index, ranges, background) in [
            (old, old_ranges, colors.removed_word_bg),
            (new, new_ranges, colors.added_word_bg),
        ] {
            let row = &mut file.all_rows[index];
            row.runs = overlay_background(row.text.len(), &row.runs, &ranges, background);
        }
    }
}

fn changed_words(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let diff = TextDiff::from_unicode_words(old, new);
    if diff.ratio() < 0.25 {
        return (Vec::new(), Vec::new());
    }
    let (mut old_ranges, mut new_ranges) = (Vec::new(), Vec::new());
    let (mut old_offset, mut new_offset) = (0, 0);
    for change in diff.iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                old_offset += len;
                new_offset += len;
            }
            ChangeTag::Delete => {
                push_word_range(&mut old_ranges, old_offset..old_offset + len, old);
                old_offset += len;
            }
            ChangeTag::Insert => {
                push_word_range(&mut new_ranges, new_offset..new_offset + len, new);
                new_offset += len;
            }
        }
    }
    (old_ranges, new_ranges)
}

fn push_word_range(ranges: &mut Vec<Range<usize>>, range: Range<usize>, text: &str) {
    if let Some(last) = ranges.last_mut()
        && text[last.end..range.start].trim().is_empty()
    {
        last.end = range.end;
    } else {
        ranges.push(range);
    }
}

fn overlay_background(
    text_len: usize,
    runs: &[(Range<usize>, HighlightStyle)],
    highlights: &[Range<usize>],
    background: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if text_len == 0 {
        return Vec::new();
    }
    let mut boundaries = vec![0, text_len];
    for (range, _) in runs {
        boundaries.extend([range.start.min(text_len), range.end.min(text_len)]);
    }
    for range in highlights {
        boundaries.extend([range.start.min(text_len), range.end.min(text_len)]);
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut output = Vec::new();
    for pair in boundaries.windows(2) {
        let range = pair[0]..pair[1];
        if range.is_empty() {
            continue;
        }
        let mut style = runs
            .iter()
            .find(|(candidate, _)| candidate.start <= range.start && candidate.end > range.start)
            .map(|(_, style)| *style)
            .unwrap_or_default();
        if highlights
            .iter()
            .any(|highlight| highlight.start < range.end && highlight.end > range.start)
        {
            style.background_color = Some(background);
        }
        push_style_run(&mut output, range, style);
    }
    output
}

fn push_style_run(
    runs: &mut Vec<(Range<usize>, HighlightStyle)>,
    range: Range<usize>,
    style: HighlightStyle,
) {
    if let Some((last_range, last_style)) = runs.last_mut()
        && last_range.end == range.start
        && *last_style == style
    {
        last_range.end = range.end;
    } else {
        runs.push((range, style));
    }
}

pub(crate) fn sub_runs(
    all: &[(Range<usize>, HighlightStyle)],
    start: usize,
    end: usize,
) -> Vec<(Range<usize>, HighlightStyle)> {
    all.iter()
        .filter(|(range, _)| range.start < end && range.end > start)
        .map(|(range, style)| {
            (
                range.start.max(start) - start..range.end.min(end) - start,
                *style,
            )
        })
        .collect()
}

fn display_line(
    text: &str,
    runs: &[(Range<usize>, HighlightStyle)],
    show_invisibles: bool,
    ws_style: &HighlightStyle,
) -> (String, Vec<(Range<usize>, HighlightStyle)>) {
    let mut output = String::with_capacity(text.len());
    let mut output_runs = Vec::new();
    let mut column = 0;
    for (old_start, ch) in text.char_indices() {
        let start = output.len();
        let width = if ch == '\t' {
            4 - column % 4
        } else {
            unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        match ch {
            ' ' if show_invisibles => output.push('·'),
            '\t' => {
                if show_invisibles {
                    output.push('→');
                } else {
                    output.push(' ');
                }
                output.extend(std::iter::repeat_n(' ', width - 1));
            }
            _ => output.push(ch),
        }
        column += width;
        let mut style = runs
            .iter()
            .find(|(range, _)| range.contains(&old_start))
            .map(|(_, style)| *style)
            .unwrap_or_default();
        if show_invisibles && matches!(ch, ' ' | '\t') {
            style.color = ws_style.color;
        }
        push_style_run(&mut output_runs, start..output.len(), style);
    }
    (output, output_runs)
}

pub fn display_columns(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::super::rows::{DisplayRow, expand, project_file};
    use super::*;

    fn render(old: &str, new: &str) -> RenderedFile {
        build_file(
            &FileDiffInput {
                path: "example.rs",
                kind: FileChangeKind::Modify,
                old_text: Some(old),
                new_text: Some(new),
                patch: None,
                ignore_whitespace: false,
                show_invisibles: false,
            },
            "example.rs".into(),
            "rust",
            &HighlightTheme::default_dark(),
            &colors(),
            &HighlightStyle::default(),
        )
    }

    fn colors() -> DiffColors {
        DiffColors {
            added_word_bg: gpui::hsla(0.3, 0.8, 0.5, 0.3),
            removed_word_bg: gpui::hsla(0., 0.8, 0.5, 0.28),
        }
    }

    fn visible_new(file: &RenderedFile) -> Vec<u32> {
        let mut rows = Vec::new();
        project_file(&mut rows, 0, file, false);
        rows.into_iter()
            .filter_map(|row| match row {
                DisplayRow::Code {
                    right: Some(ix), ..
                } => file.all_rows[ix].new,
                _ => None,
            })
            .collect()
    }
    #[test]
    fn reconstruction_requires_matching_unambiguous_current_text() {
        for (current, patch, old) in [
            (
                "one\ntwo\nthree\nfour\nnew value\nsix\nseven\neight\nnine\nten\n",
                "@@ -3,5 +3,5 @@\n three\n four\n-old value\n+new value\n six\n seven\n",
                Some("one\ntwo\nthree\nfour\nold value\nsix\nseven\neight\nnine\nten\n"),
            ),
            ("alpha\nbeta\n", "+alpha\n+beta", Some("")),
            (
                "pub struct Cart;\n\npub struct Item;\n",
                "@@ -0,0 +1,3 @@\n+pub struct Cart;\n+\n+pub struct Item;\n",
                Some(""),
            ),
            (
                "before\nnew one\nnew two\nafter\n",
                "-old one\n-old two\n+new one\n+new two",
                Some("before\nold one\nold two\nafter\n"),
            ),
            ("new\nbetween\nnew\n", "-old\n+new", None),
            ("unrelated\n", "-old\n+new", None),
            (
                "one\nchanged again\nthree\n",
                "@@ -1,3 +1,3 @@\n one\n-old\n+new\n three\n",
                None,
            ),
        ] {
            assert_eq!(
                reconstruct_from_text(current.into(), patch),
                old.map(|old| (old.into(), current.into())),
                "{patch:?} against {current:?}"
            );
        }
    }

    #[test]
    fn word_emphasis_tracks_a_multibyte_replacement() {
        let file = render("let 名 = \"🍐\"; // 👩🏽‍💻\n", "let 名 = \"🍎\"; // 👩🏽‍💻\n");
        for (row, expected) in file.all_rows.iter().zip(["🍐", "🍎"]) {
            let emphasized = row
                .runs
                .iter()
                .filter(|(_, style)| style.background_color.is_some())
                .map(|(range, _)| {
                    row.text
                        .get(range.clone())
                        .expect("emphasis must land on UTF-8 boundaries")
                })
                .collect::<String>();
            assert_eq!(emphasized, expected);
        }
    }

    #[test]
    fn context_disclosure_reveals_lines_next_to_the_requested_edge() {
        let old = (1..=30).map(|n| format!("line {n}\n")).collect::<String>();
        let new = old.replace("line 15\n", "changed 15\n");
        for split in [false, true] {
            let mut file = render(&old, &new);
            let mut rows = Vec::new();
            project_file(&mut rows, 0, &file, split);
            let leading = rows
                .iter()
                .find_map(|row| match row {
                    DisplayRow::Fold {
                        new_lines,
                        expandable: true,
                        ..
                    } => Some(new_lines.clone()),
                    _ => None,
                })
                .expect("unchanged leading context is folded");
            assert_eq!(visible_new(&file), (12..=18).collect::<Vec<_>>());
            expand(&mut file, leading, ExpandDir::Up, 2);
            assert_eq!(visible_new(&file), (10..=18).collect::<Vec<_>>());
            expand(&mut file, 19..31, ExpandDir::Down, 2);
            assert_eq!(visible_new(&file), (10..=20).collect::<Vec<_>>());
            expand(&mut file, 1..10, ExpandDir::All, 20);
            expand(&mut file, 21..31, ExpandDir::All, 20);
            assert_eq!(visible_new(&file), (1..=30).collect::<Vec<_>>());
        }
    }

    #[test]
    fn metadata_only_changes_have_no_numbered_source() {
        for patch in [
            "diff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs\n",
            "diff --git a/script b/script\nold mode 100644\nnew mode 100755\n",
        ] {
            let file = build_file(
                &FileDiffInput {
                    path: "new.rs",
                    kind: FileChangeKind::Modify,
                    old_text: None,
                    new_text: None,
                    patch: Some(patch),
                    ignore_whitespace: false,
                    show_invisibles: false,
                },
                "new.rs".into(),
                "rust",
                &HighlightTheme::default_dark(),
                &colors(),
                &HighlightStyle::default(),
            );
            assert_eq!(file.notice, Some(FileNotice::Unavailable));
            assert!(file.all_rows.is_empty());
        }
    }

    #[test]
    fn unavailable_patch_context_never_pairs_changes_across_hunks() {
        let file = build_file(
            &FileDiffInput {
                path: "example.rs",
                kind: FileChangeKind::Modify,
                old_text: None,
                new_text: None,
                patch: Some(
                    "@@ -10 +10,0 @@ first\n-old value\n@@ -30,0 +30 @@ second\n+new value\n",
                ),
                ignore_whitespace: false,
                show_invisibles: false,
            },
            "example.rs".into(),
            "rust",
            &HighlightTheme::default_dark(),
            &colors(),
            &HighlightStyle::default(),
        );
        let mut rows = Vec::new();
        project_file(&mut rows, 0, &file, true);
        let code = rows
            .iter()
            .filter_map(|row| match row {
                DisplayRow::Code { left, right, .. } => Some((
                    left.and_then(|ix| file.all_rows[ix].old),
                    right.and_then(|ix| file.all_rows[ix].new),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(code, [(Some(10), None), (None, Some(30))]);
        assert!(rows.iter().any(|row| matches!(
            row,
            DisplayRow::Fold {
                expandable: false,
                ..
            }
        )));
        assert!(
            rows.iter()
                .any(|row| matches!(row, DisplayRow::Hunk { hunk: 1, .. }))
        );
    }

    #[test]
    fn whitespace_options_preserve_the_source_for_copy_and_review() {
        let file = build_file(
            &FileDiffInput {
                path: "example.rs",
                kind: FileChangeKind::Modify,
                old_text: Some("let 名 = 1;\n"),
                new_text: Some("let  名\t=1;\n"),
                patch: None,
                ignore_whitespace: true,
                show_invisibles: true,
            },
            "example.rs".into(),
            "rust",
            &HighlightTheme::default_dark(),
            &colors(),
            &HighlightStyle::default(),
        );
        assert_eq!((file.added, file.removed), (0, 0));
        let row = &file.all_rows[0];
        assert_eq!(row.source_text, "let  名\t=1;");
        assert_eq!(row.text, "let··名→=1;");
        assert!(
            row.runs
                .iter()
                .all(|(range, _)| row.text.get(range.clone()).is_some())
        );
    }
}
