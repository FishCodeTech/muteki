use std::collections::HashSet;
use std::ops::Range;

/// Before GPUI reports its exact visible range, twelve rows is comfortably
/// more than a typical chat viewport. Its list also pre-measures four viewport
/// heights, so the wider eviction band keeps warm rows resident without tying
/// Markdown lifetime to measurement.
const VIEWPORT_ROW_HINT: usize = 12;
const BUILD_MARGIN_ROWS: usize = 24;
/// Three build margins prevent back-and-forth scrolling from rebuilding the
/// same parsed documents at the edge of the warm window.
const EVICT_MARGIN_ROWS: usize = 72;
/// The composer-adjacent tail stays ready even while inspecting old rows.
const TAIL_PIN_ROWS: usize = 4;

#[derive(Clone, Debug)]
pub(super) struct MarkdownEntry {
    pub id: String,
    /// The timeline row that renders this document.
    pub row: usize,
}

pub(super) struct ResidencyInput<'a> {
    pub row_count: usize,
    pub visible_rows: Range<usize>,
    pub one_shot_row_target: Option<usize>,
    pub entries: &'a [MarkdownEntry],
    pub stream_running: bool,
    pub resident_ids: &'a HashSet<String>,
}

/// Row-only superset of entries that can affect [`decide`].
pub(super) struct ResidencyScope {
    build_rows: Range<usize>,
    keep_rows: Range<usize>,
    tail_start: usize,
    last_row: Option<usize>,
    stream_running: bool,
}

impl ResidencyScope {
    pub(super) fn new(
        row_count: usize,
        visible_rows: Range<usize>,
        one_shot_row_target: Option<usize>,
        stream_running: bool,
    ) -> Self {
        let visible_rows = one_shot_row_target
            .filter(|row| *row < row_count)
            .map(|row| row..(row + VIEWPORT_ROW_HINT).min(row_count))
            .unwrap_or(visible_rows);
        Self {
            build_rows: expand_row_window(visible_rows.clone(), BUILD_MARGIN_ROWS, row_count),
            keep_rows: expand_row_window(visible_rows, EVICT_MARGIN_ROWS, row_count),
            tail_start: row_count.saturating_sub(TAIL_PIN_ROWS),
            last_row: row_count.checked_sub(1),
            stream_running,
        }
    }

    pub(super) fn includes(&self, row: usize) -> bool {
        self.keep_rows.contains(&row) || self.pinned(row)
    }

    /// The tail rows stay resident: the composer sits under them, and a
    /// running turn streams into the last of them. The rest of a running
    /// turn is ordinary history, however long it grows.
    fn pinned(&self, row: usize) -> bool {
        self.last_row.is_some() && row >= self.tail_start
            || self.stream_running && self.last_row == Some(row)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ResidencyDecisions {
    pub build: HashSet<String>,
    pub evict: HashSet<String>,
}

pub(super) fn decide(input: ResidencyInput<'_>) -> ResidencyDecisions {
    let scope = ResidencyScope::new(
        input.row_count,
        input.visible_rows,
        input.one_shot_row_target,
        input.stream_running,
    );

    let build = input
        .entries
        .iter()
        .filter(|entry| scope.build_rows.contains(&entry.row) || scope.pinned(entry.row))
        .map(|entry| entry.id.clone())
        .collect();
    let keep = input
        .entries
        .iter()
        .filter(|entry| scope.includes(entry.row))
        .map(|entry| entry.id.as_str())
        .collect::<HashSet<_>>();
    let evict = input
        .resident_ids
        .iter()
        .filter(|id| !keep.contains(id.as_str()))
        .cloned()
        .collect();

    ResidencyDecisions { build, evict }
}

pub(super) fn tail_row_window(row_count: usize) -> Range<usize> {
    row_count.saturating_sub(VIEWPORT_ROW_HINT)..row_count
}

pub(super) fn viewport_row_window(scroll_top: usize, row_count: usize) -> Range<usize> {
    scroll_top..(scroll_top + VIEWPORT_ROW_HINT).min(row_count)
}

fn expand_row_window(window: Range<usize>, margin: usize, row_count: usize) -> Range<usize> {
    window.start.min(row_count).saturating_sub(margin)
        ..window
            .end
            .min(row_count)
            .saturating_add(margin)
            .min(row_count)
}

#[cfg(test)]
mod tests {
    use super::{MarkdownEntry, ResidencyDecisions, ResidencyInput, decide, tail_row_window};
    use std::collections::HashSet;

    #[test]
    fn residency_stays_bounded_across_small_scrolls_and_distant_jumps() {
        let entries = entries(240);
        let initial = decisions(&entries, tail_row_window(240), None, &HashSet::new());
        assert_eq!(initial.build.len(), 36);
        assert!(initial.evict.is_empty());
        let mut residents = initial.build;
        let shifted = decisions(&entries, 230..238, None, &residents);
        apply(&mut residents, shifted);
        let back_to_tail = decisions(&entries, tail_row_window(240), None, &residents);
        assert!(back_to_tail.build.is_subset(&residents));
        assert!(back_to_tail.evict.is_empty());
        let jump = decisions(&entries, tail_row_window(240), Some(40), &residents);
        apply(&mut residents, jump);
        assert_eq!(residents.len(), 64);
        assert!(residents.contains("assistant-40"));
        assert!(!residents.contains("assistant-230"));
        assert!(residents.contains("assistant-238"));
        assert!(residents.contains("assistant-239"));
    }

    #[test]
    fn the_running_tail_stays_resident() {
        let entries = entries(240);
        let residents = ["assistant-150".to_string()].into_iter().collect();
        let decisions = decide(ResidencyInput {
            row_count: 240,
            visible_rows: 40..48,
            one_shot_row_target: None,
            entries: &entries,
            stream_running: true,
            resident_ids: &residents,
        });

        assert!(!decisions.build.contains("assistant-5"));
        assert!(decisions.build.contains("assistant-238"));
        assert!(decisions.build.contains("assistant-239"));
        assert!(decisions.evict.contains("assistant-150"));
    }

    /// One document per row, as a conversation of single-message rows has.
    fn entries(row_count: usize) -> Vec<MarkdownEntry> {
        (0..row_count)
            .map(|row| MarkdownEntry {
                id: format!("assistant-{row}"),
                row,
            })
            .collect()
    }

    fn decisions(
        entries: &[MarkdownEntry],
        visible_rows: std::ops::Range<usize>,
        one_shot_row_target: Option<usize>,
        resident_ids: &HashSet<String>,
    ) -> ResidencyDecisions {
        decide(ResidencyInput {
            row_count: 240,
            visible_rows,
            one_shot_row_target,
            entries,
            stream_running: false,
            resident_ids,
        })
    }

    fn apply(residents: &mut HashSet<String>, decisions: ResidencyDecisions) {
        residents.retain(|id| !decisions.evict.contains(id));
        residents.extend(decisions.build);
    }
}
