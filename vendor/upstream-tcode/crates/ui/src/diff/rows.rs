use std::ops::Range;

use super::model::{ExpandDir, PairedRow, RenderedFile, RenderedRow};
use super::parse::RowKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DisplayRow {
    Header(usize),
    Notice(usize),
    Hunk {
        file: usize,
        hunk: usize,
    },
    Fold {
        file: usize,
        new_lines: Range<u32>,
        expandable: bool,
    },
    Code {
        file: usize,
        left: Option<usize>,
        right: Option<usize>,
    },
}

impl DisplayRow {
    pub(crate) fn file(&self) -> usize {
        match *self {
            Self::Header(file) | Self::Notice(file) => file,
            Self::Hunk { file, .. } | Self::Fold { file, .. } | Self::Code { file, .. } => file,
        }
    }
}

/// Counterparts are paired within a hunk, never across missing source.
pub(super) fn pair_rows(rows: &[RenderedRow], range: Range<usize>) -> Vec<PairedRow> {
    let mut pairs = Vec::new();
    let mut ix = range.start;
    while ix < range.end {
        if rows[ix].kind == RowKind::Context {
            pairs.push(PairedRow {
                left: Some(ix),
                right: Some(ix),
            });
            ix += 1;
            continue;
        }
        let start = ix;
        while ix < range.end && rows[ix].kind != RowKind::Context {
            ix += 1;
        }
        let removed = (start..ix)
            .filter(|&row| rows[row].kind == RowKind::Removed)
            .collect::<Vec<_>>();
        let added = (start..ix)
            .filter(|&row| rows[row].kind == RowKind::Added)
            .collect::<Vec<_>>();
        for offset in 0..removed.len().max(added.len()) {
            pairs.push(PairedRow {
                left: removed.get(offset).copied(),
                right: added.get(offset).copied(),
            });
        }
    }
    pairs
}

fn changed(pair: &PairedRow) -> bool {
    pair.left != pair.right
}

/// Change windows and reader expansions form intervals over the source pairs.
/// Boundaries keep projection linear even when context windows overlap.
fn visible_pairs(file: &RenderedFile, hunk: Range<usize>) -> Vec<bool> {
    let mut boundaries = vec![0isize; hunk.len() + 1];
    if !file.expandable {
        return vec![true; hunk.len()];
    }
    let mut ix = hunk.start;
    while ix < hunk.end {
        if !changed(&file.pairs[ix]) {
            ix += 1;
            continue;
        }
        let start = ix;
        while ix < hunk.end && changed(&file.pairs[ix]) {
            ix += 1;
        }
        boundaries[start.saturating_sub(3).max(hunk.start) - hunk.start] += 1;
        boundaries[ix.saturating_add(3).min(hunk.end) - hunk.start] -= 1;
    }
    for range in &file.expanded {
        let start = range.start.max(hunk.start).min(hunk.end);
        let end = range.end.min(hunk.end);
        if start < end {
            boundaries[start - hunk.start] += 1;
            boundaries[end - hunk.start] -= 1;
        }
    }
    let mut active = 0;
    let mut visible = boundaries[..hunk.len()]
        .iter()
        .map(|boundary| {
            active += boundary;
            active > 0
        })
        .collect::<Vec<_>>();
    let mut ix = 0;
    while ix < visible.len() {
        if visible[ix] {
            ix += 1;
            continue;
        }
        let start = ix;
        while ix < visible.len() && !visible[ix] {
            ix += 1;
        }
        if ix - start < 2 {
            visible[start..ix].fill(true);
        }
    }
    visible
}

pub(crate) fn project_file(
    rows: &mut Vec<DisplayRow>,
    file_index: usize,
    file: &RenderedFile,
    split: bool,
) {
    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        if !hunk.omitted_before.is_empty() {
            rows.push(DisplayRow::Fold {
                file: file_index,
                new_lines: hunk.omitted_before.clone(),
                expandable: false,
            });
        }
        if !hunk.header.is_empty() {
            rows.push(DisplayRow::Hunk {
                file: file_index,
                hunk: hunk_index,
            });
        }
        let visible = visible_pairs(file, hunk.pairs.clone());
        let mut ix = hunk.pairs.start;
        while ix < hunk.pairs.end {
            if !visible[ix - hunk.pairs.start] {
                let start = ix;
                while ix < hunk.pairs.end && !visible[ix - hunk.pairs.start] {
                    ix += 1;
                }
                let first = file.pairs[start]
                    .right
                    .expect("folded context has a new line");
                let last = file.pairs[ix - 1]
                    .right
                    .expect("folded context has a new line");
                rows.push(DisplayRow::Fold {
                    file: file_index,
                    new_lines: file.all_rows[first].new.expect("context line")
                        ..file.all_rows[last].new.expect("context line") + 1,
                    expandable: true,
                });
            } else if !split && changed(&file.pairs[ix]) {
                let start = ix;
                while ix < hunk.pairs.end && changed(&file.pairs[ix]) {
                    ix += 1;
                }
                for pair in &file.pairs[start..ix] {
                    if let Some(left) = pair.left {
                        rows.push(DisplayRow::Code {
                            file: file_index,
                            left: Some(left),
                            right: None,
                        });
                    }
                }
                for pair in &file.pairs[start..ix] {
                    if let Some(right) = pair.right {
                        rows.push(DisplayRow::Code {
                            file: file_index,
                            left: None,
                            right: Some(right),
                        });
                    }
                }
            } else {
                let pair = file.pairs[ix];
                rows.push(DisplayRow::Code {
                    file: file_index,
                    left: pair.left,
                    right: pair.right,
                });
                ix += 1;
            }
        }
    }
}

pub(crate) fn expand(
    file: &mut RenderedFile,
    lines: Range<u32>,
    direction: ExpandDir,
    amount: u32,
) {
    if !file.expandable {
        return;
    }
    let context = file
        .pairs
        .iter()
        .enumerate()
        .filter_map(|(ix, pair)| {
            if changed(pair) {
                return None;
            }
            let line = file.all_rows[pair.right?].new?;
            lines.contains(&line).then_some(ix)
        })
        .collect::<Vec<_>>();
    let (Some(&first), Some(&last)) = (context.first(), context.last()) else {
        return;
    };
    let range = match direction {
        ExpandDir::Up => (last + 1).saturating_sub(amount as usize).max(first)..last + 1,
        ExpandDir::Down => first..first.saturating_add(amount as usize).min(last + 1),
        ExpandDir::All => first..last + 1,
    };
    file.expanded.push(range);
    file.expanded.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in file.expanded.drain(..) {
        if let Some(last) = merged.last_mut()
            && last.end >= range.start
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    file.expanded = merged;
}
