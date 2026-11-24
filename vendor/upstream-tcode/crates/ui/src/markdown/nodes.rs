//! Markdown render IR.
//!
//! Selection storage is adapted from gpui-component's Apache-2.0 text node
//! implementation; parsing and the node shapes themselves are tcode-owned.

use std::{
    ops::Range,
    sync::{Arc, Mutex, OnceLock, Weak},
};

use gpui::{SharedString, SharedUri};

use super::inline::InlineState;

/// A place in a document's text: the segment at render `path` (a paragraph,
/// heading, table cell or code block), the `line` of a code block, and the
/// byte `offset` in that segment. Paths number children as the renderer
/// does, so ordering positions orders them as the document reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TextPosition {
    pub(crate) path: Vec<u32>,
    pub(crate) line: u32,
    pub(crate) offset: usize,
}

impl TextPosition {
    /// Before every segment.
    pub(crate) fn start() -> Self {
        Self::default()
    }

    /// After every segment.
    pub(crate) fn end() -> Self {
        Self {
            path: vec![u32::MAX],
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BlockNode {
    Root {
        children: Vec<BlockNode>,
    },
    Paragraph(Paragraph),
    Heading {
        level: u8,
        children: Paragraph,
    },
    Blockquote {
        children: Vec<BlockNode>,
    },
    List {
        children: Vec<BlockNode>,
        ordered: bool,
        /// First item number of an ordered list (CommonMark `start`).
        start: u32,
    },
    ListItem {
        children: Vec<BlockNode>,
        spread: bool,
        checked: Option<bool>,
    },
    CodeBlock(CodeBlock),
    Table(Table),
    HorizontalRule,
    Unknown,
}

impl BlockNode {
    pub(super) fn reuse_highlights(&self, previous: &Self) {
        match (self, previous) {
            (Self::CodeBlock(new), Self::CodeBlock(old)) => {
                let Some(prepared) = old.prepared.get() else {
                    return;
                };
                let cached = prepared
                    .highlights
                    .lock()
                    .ok()
                    .and_then(|cache| cache.as_ref().map(|(_, runs)| runs.clone()))
                    .or_else(|| prepared.previous_highlights.get().cloned());
                if let Some(cached) = cached {
                    // Multiple chunks can arrive before painting. Carry the last
                    // available checkpoint without retaining evicted cache entries.
                    let _ = new.prepared().previous_highlights.set(cached);
                }
            }
            (Self::Root { children: new }, Self::Root { children: old })
            | (Self::Blockquote { children: new }, Self::Blockquote { children: old })
            | (Self::List { children: new, .. }, Self::List { children: old, .. })
            | (Self::ListItem { children: new, .. }, Self::ListItem { children: old, .. }) => {
                for (new, old) in new.iter().zip(old) {
                    new.reuse_highlights(old);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn text(&self) -> String {
        match self {
            Self::Root { children, .. }
            | Self::Blockquote { children, .. }
            | Self::List { children, .. }
            | Self::ListItem { children, .. } => children
                .iter()
                .map(Self::text)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Paragraph(paragraph) => paragraph.text(),
            Self::Heading { children, .. } => children.text(),
            Self::CodeBlock(code) => code
                .formula
                .text()
                .unwrap_or_else(|| rendered_code_text(&code.code)),
            Self::Table(table) => table
                .children
                .iter()
                .map(|row| {
                    row.children
                        .iter()
                        .map(|cell| cell.children.text())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::HorizontalRule | Self::Unknown => String::new(),
        }
    }

    /// Every selectable segment in document order, with its render path:
    /// each paragraph, heading and table cell, and each line of a code block
    /// (a laid-out display formula is one line).
    pub(crate) fn for_each_segment(&self, f: &mut SegmentVisitor<'_>) {
        self.walk_segments(&mut Vec::new(), f);
    }

    fn walk_segments(&self, path: &mut Vec<u32>, f: &mut SegmentVisitor<'_>) {
        match self {
            Self::Root { children }
            | Self::Blockquote { children }
            | Self::List { children, .. }
            | Self::ListItem { children, .. } => {
                for (ix, child) in children.iter().enumerate() {
                    path.push(ix as u32);
                    child.walk_segments(path, f);
                    path.pop();
                }
            }
            Self::Paragraph(paragraph) => f(path, 0, &paragraph.text(), SegmentKind::Prose),
            Self::Heading { children, .. } => f(path, 0, &children.text(), SegmentKind::Prose),
            Self::CodeBlock(code) => match code.formula.text() {
                Some(formula) => f(path, 0, &formula, SegmentKind::Code),
                None => {
                    let prepared = code.prepared();
                    for ix in 0..prepared.lines.len() {
                        f(path, ix as u32, prepared.line(ix), SegmentKind::Code);
                    }
                }
            },
            Self::Table(table) => {
                for (row_ix, row) in table.children.iter().enumerate() {
                    for (ix, cell) in row.children.iter().enumerate() {
                        path.extend([row_ix as u32, ix as u32]);
                        f(path, 0, &cell.children.text(), SegmentKind::Cell);
                        path.truncate(path.len() - 2);
                    }
                }
            }
            Self::HorizontalRule | Self::Unknown => {}
        }
    }

    /// The text of the segment at `path` and `line`.
    pub(crate) fn segment_text(&self, path: &[u32], line: u32) -> Option<String> {
        let mut found = None;
        self.for_each_segment(&mut |at, at_line, text, _| {
            if found.is_none() && at == path && at_line == line {
                found = Some(text.to_string());
            }
        });
        found
    }

    /// The positions before the first character of this document and after
    /// its last.
    pub(crate) fn extent(&self) -> Option<(TextPosition, TextPosition)> {
        let mut extent: Option<(TextPosition, TextPosition)> = None;
        self.for_each_segment(&mut |path, line, text, _| {
            let start = TextPosition {
                path: path.to_vec(),
                line,
                offset: 0,
            };
            let end = TextPosition {
                offset: text.len(),
                ..start.clone()
            };
            match &mut extent {
                Some((_, last)) => *last = end,
                None => extent = Some((start, end)),
            }
        });
        extent
    }

    /// The text from `start` to `end` as a reader would paste it: segments
    /// on their own lines, the cells of a table row separated by tabs. A
    /// segment the range only touches at an edge adds nothing.
    pub(crate) fn text_between(&self, start: &TextPosition, end: &TextPosition) -> String {
        struct Piece {
            text: String,
            row: Option<Vec<u32>>,
        }
        let mut pieces: Vec<Piece> = Vec::new();
        self.for_each_segment(&mut |path, line, text, kind| {
            let first = TextPosition {
                path: path.to_vec(),
                line,
                offset: 0,
            };
            let last = TextPosition {
                offset: text.len(),
                ..first.clone()
            };
            if *end <= first || *start >= last {
                return;
            }
            let at = |position: &TextPosition, default: usize| {
                if position.path == path && position.line == line {
                    floor_char_boundary(text, position.offset)
                } else {
                    default
                }
            };
            let (from, to) = (at(start, 0), at(end, text.len()));
            let piece = text.get(from..to.max(from)).unwrap_or_default();
            if piece.is_empty() && kind == SegmentKind::Prose {
                return;
            }
            pieces.push(Piece {
                text: piece.to_string(),
                row: (kind == SegmentKind::Cell).then(|| path[..path.len() - 1].to_vec()),
            });
        });
        let mut text = String::new();
        for (ix, piece) in pieces.iter().enumerate() {
            if ix > 0 {
                let same_row = piece.row.is_some() && piece.row == pieces[ix - 1].row;
                text.push(if same_row { '\t' } else { '\n' });
            }
            text.push_str(&piece.text);
        }
        text
    }
}

/// Called with each segment's render path, line, text and kind.
pub(crate) type SegmentVisitor<'a> = dyn FnMut(&[u32], u32, &str, SegmentKind) + 'a;

/// What kind of text a segment is, which decides how a copy joins it to its
/// neighbours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SegmentKind {
    Prose,
    Code,
    Cell,
}

fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct LinkMark {
    pub(crate) url: SharedString,
    pub(crate) title: Option<SharedString>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct TextMark {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) strikethrough: bool,
    pub(crate) code: bool,
    pub(crate) link: Option<LinkMark>,
}

impl TextMark {
    pub(crate) fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub(crate) fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub(crate) fn strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }

    pub(crate) fn code(mut self) -> Self {
        self.code = true;
        self
    }

    pub(crate) fn link(mut self, link: impl Into<LinkMark>) -> Self {
        self.link = Some(link.into());
        self
    }

    pub(crate) fn merge(&mut self, other: TextMark) {
        self.bold |= other.bold;
        self.italic |= other.italic;
        self.strikethrough |= other.strikethrough;
        self.code |= other.code;
        if other.link.is_some() {
            self.link = other.link;
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ImageNode {
    pub(crate) url: SharedUri,
    pub(crate) link: Option<LinkMark>,
    pub(crate) title: Option<SharedString>,
    pub(crate) alt: Option<SharedString>,
}

impl ImageNode {
    pub(crate) fn title(&self) -> String {
        self.title
            .clone()
            .unwrap_or_else(|| self.alt.clone().unwrap_or_default())
            .to_string()
    }
}

impl PartialEq for ImageNode {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
            && self.link == other.link
            && self.title == other.title
            && self.alt == other.alt
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct InlineNode {
    pub(crate) text: SharedString,
    pub(crate) image: Option<ImageNode>,
    pub(super) math: Option<super::math_parse::MathSpan>,
    pub(crate) marks: Vec<(Range<usize>, TextMark)>,
    pub(super) state: Arc<Mutex<InlineState>>,
}

impl PartialEq for InlineNode {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.image == other.image
            && self.math == other.math
            && self.marks == other.marks
    }
}

impl InlineNode {
    pub(super) fn rendered_text(&self) -> String {
        self.math
            .as_ref()
            .and_then(|math| math.selection.text())
            .unwrap_or_else(|| self.text.to_string())
    }

    pub(crate) fn new(text: impl Into<SharedString>) -> Self {
        let text = text.into();
        Self {
            state: InlineState::shared(text.clone()),
            text,
            image: None,
            math: None,
            marks: vec![],
        }
    }

    pub(crate) fn image(image: ImageNode) -> Self {
        let mut node = Self::new("");
        node.image = Some(image);
        node
    }

    pub(crate) fn marks(mut self, marks: Vec<(Range<usize>, TextMark)>) -> Self {
        self.marks = marks;
        self
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Paragraph {
    pub(crate) children: Vec<InlineNode>,
    pub(super) state: Arc<Mutex<InlineState>>,
}

impl PartialEq for Paragraph {
    fn eq(&self, other: &Self) -> bool {
        self.children == other.children
    }
}

impl Paragraph {
    pub(crate) fn text(&self) -> String {
        self.children
            .iter()
            .map(|node| node.rendered_text())
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CodeBlock {
    pub(super) formula: super::math_layout::FormulaSelection,
    pub(crate) code: SharedString,
    pub(crate) lang: Option<SharedString>,
    pub(super) line_states: Arc<Mutex<Vec<Arc<Mutex<InlineState>>>>>,
    pub(super) prepared: Arc<OnceLock<PreparedCode>>,
}

pub(super) type CodeHighlightRuns = crate::highlight::HighlightedSource;

#[derive(Debug)]
pub(super) struct PreparedCode {
    pub(super) text: SharedString,
    pub(super) lines: Vec<Range<usize>>,
    pub(super) previous_highlights: OnceLock<Weak<CodeHighlightRuns>>,
    pub(super) highlights:
        Mutex<Option<(crate::highlight::HighlightTheme, Weak<CodeHighlightRuns>)>>,
}

impl PreparedCode {
    pub(super) fn line(&self, index: usize) -> &str {
        &self.text[self.lines[index].clone()]
    }
}

impl PartialEq for CodeBlock {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code && self.lang == other.lang
    }
}

impl CodeBlock {
    pub(super) fn prepared(&self) -> &PreparedCode {
        self.prepared.get_or_init(|| {
            let text: SharedString = if self.code.contains("\r\n") {
                self.code.replace("\r\n", "\n").into()
            } else {
                self.code.clone()
            };
            let mut lines = Vec::new();
            let mut start = 0;
            for (index, byte) in text.bytes().enumerate() {
                if byte == b'\n' {
                    lines.push(start..index);
                    start = index + 1;
                }
            }
            if start < text.len() {
                lines.push(start..text.len());
            }
            PreparedCode {
                text,
                lines,
                highlights: Mutex::new(None),
                previous_highlights: OnceLock::new(),
            }
        })
    }

    pub(super) fn states_for_lines(
        &self,
        code: &PreparedCode,
        span: Range<usize>,
    ) -> Vec<Arc<Mutex<InlineState>>> {
        let Ok(mut states) = self.line_states.lock() else {
            return span
                .map(|ix| InlineState::shared(code.line(ix).to_string().into()))
                .collect();
        };
        if states.len() != code.lines.len() {
            *states = (0..code.lines.len())
                .map(|ix| InlineState::shared(code.line(ix).to_string().into()))
                .collect();
        } else {
            for (state, ix) in states[span.clone()].iter().zip(span.clone()) {
                if let Ok(mut state) = state.lock() {
                    state.set_text(code.line(ix).to_string().into());
                }
            }
        }
        states[span].to_vec()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Table {
    pub(crate) children: Vec<TableRow>,
    pub(crate) column_aligns: Vec<ColumnumnAlign>,
}

impl Table {
    pub(crate) fn column_align(&self, index: usize) -> ColumnumnAlign {
        self.column_aligns.get(index).copied().unwrap_or_default()
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub(crate) enum ColumnumnAlign {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TableRow {
    pub(crate) children: Vec<TableCell>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TableCell {
    pub(crate) children: Paragraph,
}

fn rendered_code_text(code: &str) -> String {
    let mut text = code.replace("\r\n", "\n");
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(path: &[u32], line: u32, offset: usize) -> TextPosition {
        TextPosition {
            path: path.to_vec(),
            line,
            offset,
        }
    }

    const DOCUMENT: &str = "intro text\n\n```text\nfirst code line\nsecond code line\n```\n\n\
                            | name | value |\n| --- | --- |\n| alpha | beta |\n\n- one\n- two";

    #[test]
    fn a_copy_reads_segments_in_document_order_from_the_middle_of_one_to_another() {
        let document = super::super::parse(DOCUMENT);
        assert_eq!(
            document.text_between(&at(&[1], 0, 6), &at(&[1], 1, 6)),
            "code line\nsecond"
        );
        assert_eq!(
            document.text_between(&TextPosition::start(), &TextPosition::end()),
            "intro text\nfirst code line\nsecond code line\nname\tvalue\nalpha\tbeta\none\ntwo"
        );
        assert_eq!(
            document.text_between(&at(&[3, 0, 0], 0, 1), &TextPosition::end()),
            "ne\ntwo"
        );
    }

    #[test]
    fn a_table_is_read_cell_by_cell_along_each_row() {
        let document = super::super::parse(DOCUMENT);
        assert_eq!(
            document.text_between(&at(&[2, 0, 0], 0, 2), &at(&[2, 1, 0], 0, 3)),
            "me\tvalue\nalp"
        );
        assert_eq!(
            document.text_between(&at(&[2, 0, 1], 0, 0), &at(&[2, 0, 1], 0, 3)),
            "val"
        );
    }

    #[test]
    fn a_range_touching_a_segment_only_at_its_edge_adds_nothing_of_it() {
        let document = super::super::parse(DOCUMENT);
        assert_eq!(
            document.text_between(&at(&[1], 0, 15), &at(&[1], 1, 6)),
            "second"
        );
        assert_eq!(
            document.text_between(&at(&[1], 0, 6), &at(&[1], 1, 0)),
            "code line"
        );
    }
}
