use gpui::{Bounds, Pixels, PlatformInputHandler, Point};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

pub const NOT_FOUND: usize = isize::MAX as usize;

/// UIKit's NSRange uses UTF-16 code units and NSNotFound for an absent range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct TextRange {
    pub location: usize,
    pub length: usize,
}

impl TextRange {
    pub fn range(self) -> Option<Range<usize>> {
        (self.location < NOT_FOUND)
            .then(|| {
                self.location
                    .checked_add(self.length)
                    .filter(|end| *end < NOT_FOUND)
                    .map(|end| self.location..end)
            })
            .flatten()
    }
}

impl From<Option<Range<usize>>> for TextRange {
    fn from(range: Option<Range<usize>>) -> Self {
        match range {
            Some(range) if range.start <= range.end && range.end < NOT_FOUND => Self {
                location: range.start,
                length: range.end - range.start,
            },
            _ => Self {
                location: NOT_FOUND,
                length: 0,
            },
        }
    }
}

/// Read a UTF-16 range without rounding offsets inside surrogate pairs.
pub fn utf16_slice(text: &str, range: Range<usize>) -> Option<&str> {
    fn byte_offset(text: &str, target: usize) -> Option<usize> {
        let mut offset = 0;
        for (index, ch) in text.char_indices() {
            if offset == target {
                return Some(index);
            }
            offset += ch.len_utf16();
        }
        (offset == target).then_some(text.len())
    }
    text.get(byte_offset(text, range.start)?..byte_offset(text, range.end)?)
}

pub fn text_length(handler: &mut PlatformInputHandler) -> Option<usize> {
    handler.text_length_utf16().or_else(|| {
        // gpui-base has no length query and clamps oversized reads to the document.
        handler
            .text_for_range(0..usize::MAX / 2, &mut None)
            .map(|text| text.encode_utf16().count())
    })
}

pub fn read_text(handler: &mut PlatformInputHandler, range: Range<usize>) -> Option<String> {
    let mut adjusted = None;
    let text = handler.text_for_range(range.clone(), &mut adjusted)?;
    let adjusted = adjusted.unwrap_or(range.clone());
    let start = range.start.checked_sub(adjusted.start)?;
    let end = range.end.checked_sub(adjusted.start)?;
    utf16_slice(&text, start..end).map(str::to_owned)
}

pub fn select_text(handler: &mut PlatformInputHandler, range: Range<usize>) {
    handler.set_selected_text_range(range.clone());
    if handler
        .selected_text_range(true)
        .is_some_and(|selection| selection.range == range)
    {
        return;
    }
    // gpui-base inherits the no-op selection setter. Its composition API
    // sets selection; identical replacement text records no undo edit.
    let marked = handler.marked_text_range();
    let context = marked
        .clone()
        .filter(|marked| marked.start <= range.start && range.end <= marked.end)
        .unwrap_or(range.clone());
    if let Some(text) = read_text(handler, context.clone()) {
        handler.replace_and_mark_text_in_range(
            Some(context.clone()),
            &text,
            Some(range.start - context.start..range.end - context.start),
        );
        if marked.is_none() {
            handler.unmark_text();
        }
    }
}

pub fn commit_marked_text(handler: &mut PlatformInputHandler) {
    // gpui-base leaves the mark/undo transaction open on blur, and
    // unmark_text emits no Change. Replacing the preedit commits and emits it.
    if let Some(marked) = handler.marked_text_range()
        && let Some(text) = read_text(handler, marked.clone())
    {
        let selection = handler.selected_text_range(true);
        handler.replace_text_in_range(Some(marked), &text);
        if let Some(selection) = selection {
            select_text(handler, selection.range);
        }
    }
}

/// The caret offset nearest `target`, walking one grapheme at a time from the
/// caret at `from`.
///
/// gpui-base 0.8.0's `character_index_for_point` tests the point against every
/// line as if that line began at the top of the input, so it finds no index
/// below the first line, and none past a line's last glyph. Caret bounds are
/// right for every visible offset, so the walk follows those.
pub fn caret_toward(
    handler: &mut PlatformInputHandler,
    from: usize,
    target: Point<Pixels>,
) -> Option<usize> {
    let length = text_length(handler)?;
    let text = read_text(handler, 0..length)?;
    let mut stops = vec![0];
    stops.extend(text.graphemes(true).scan(0, |end, grapheme| {
        *end += grapheme.encode_utf16().count();
        Some(*end)
    }));
    let mut caret = |index: usize| handler.bounds_for_range(stops[index]..stops[index]);
    let mut at = stops.iter().rposition(|stop| *stop <= from)?;
    let mut here = caret(at)?;
    loop {
        let next = if target.y >= here.bottom() {
            (at + 1..stops.len()).find_map(|index| {
                Some((index, caret(index)?)).filter(|(_, b)| b.top() >= here.bottom())
            })
        } else if target.y < here.top() {
            (0..at).rev().find_map(|index| {
                Some((index, caret(index)?)).filter(|(_, b)| b.bottom() <= here.top())
            })
        } else {
            None
        };
        let Some((index, bounds)) = next else { break };
        (at, here) = (index, bounds);
    }
    let distance = |bounds: &Bounds<Pixels>| (bounds.origin.x - target.x).abs();
    let row = here.top();
    for step in [1, -1] {
        while let Some(index) = at
            .checked_add_signed(step)
            .filter(|index| *index < stops.len())
            && let Some(bounds) = caret(index)
            && bounds.top() == row
            && distance(&bounds) < distance(&here)
        {
            (at, here) = (index, bounds);
        }
    }
    Some(stops[at])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ElementInputHandler, Entity, TestAppContext, VisualTestContext, point, px};
    use gpui_base::input::{InputEvent, TextareaState};
    use std::{cell::Cell, rc::Rc};

    fn textarea(
        cx: &mut TestAppContext,
    ) -> (
        Entity<TextareaState>,
        &mut VisualTestContext,
        PlatformInputHandler,
    ) {
        cx.update(gpui_base::init);
        let (input, cx) = cx.add_window_view(TextareaState::new);
        let handler = cx.update(|window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
            window.draw(cx).clear(cx);
            PlatformInputHandler::new(
                window.to_async(cx),
                Box::new(ElementInputHandler::new(Default::default(), input.clone())),
            )
        });
        (input, cx, handler)
    }

    #[gpui::test]
    fn native_selection_preserves_the_draft_and_undo(cx: &mut TestAppContext) {
        let (input, cx, mut handler) = textarea(cx);
        cx.simulate_input("前😀e\u{301}后");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(text_length(&mut handler), Some(6));
        assert_eq!(read_text(&mut handler, 1..3).as_deref(), Some("😀"));
        assert_eq!(read_text(&mut handler, 1..2), None);
        select_text(&mut handler, 1..3);
        assert_eq!(handler.selected_text_range(true).unwrap().range, 1..3);
        assert_eq!(handler.marked_text_range(), None);
        assert_eq!(
            input.read_with(cx, |input, _| input.value()),
            "前😀e\u{301}后"
        );
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        });
        assert_eq!(input.read_with(cx, |input, _| input.value()), "");
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-shift-z"
        } else {
            "ctrl-y"
        });
        assert_eq!(
            input.read_with(cx, |input, _| input.value()),
            "前😀e\u{301}后"
        );
    }

    #[gpui::test]
    fn committing_preedit_notifies_and_survives_blur(cx: &mut TestAppContext) {
        let (input, cx, mut handler) = textarea(cx);
        let changes = Rc::new(Cell::new(0));
        let observed = changes.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&input, move |_, event, _| {
                if matches!(event, InputEvent::Change) {
                    observed.set(observed.get() + 1);
                }
            })
        });
        handler.replace_and_mark_text_in_range(None, "你好", Some(0..0));
        commit_marked_text(&mut handler);
        assert_eq!(handler.marked_text_range(), None);
        assert_eq!(handler.selected_text_range(true).unwrap().range, 0..0);
        assert_eq!(
            changes.get(),
            1,
            "committing must publish the draft to subscribers"
        );
        select_text(&mut handler, 2..2);
        handler.replace_and_mark_text_in_range(None, "世界", Some(2..2));
        cx.update(|window, cx| {
            window.blur(cx);
            window.draw(cx).clear(cx);
        });
        commit_marked_text(&mut handler);
        assert_eq!(handler.marked_text_range(), None);
        assert_eq!(changes.get(), 2);
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
            window.draw(cx).clear(cx);
        });
        cx.simulate_input("!");
        assert_eq!(input.read_with(cx, |input, _| input.value()), "你好世界!");
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        });
        assert_eq!(input.read_with(cx, |input, _| input.value()), "你好世界");
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-z"
        } else {
            "ctrl-z"
        });
        assert_eq!(input.read_with(cx, |input, _| input.value()), "你好");
    }

    #[gpui::test]
    fn caret_toward_reaches_later_lines_and_line_ends(cx: &mut TestAppContext) {
        let (input, cx, mut handler) = textarea(cx);
        cx.simulate_input("前😀e\u{301}");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("second");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            input.read_with(cx, |input, _| input.value()),
            "前😀e\u{301}\nsecond"
        );
        fn caret(handler: &mut PlatformInputHandler, offset: usize) -> Bounds<Pixels> {
            handler.bounds_for_range(offset..offset).unwrap()
        }
        let (first_end, second) = (caret(&mut handler, 5), caret(&mut handler, 9));
        assert!(second.top() >= first_end.bottom());
        let below_and_beyond = point(second.right() + px(1000.), second.center().y);
        assert_eq!(caret_toward(&mut handler, 0, second.center()), Some(9));
        assert_eq!(caret_toward(&mut handler, 0, below_and_beyond), Some(12));
        let first_line_end = point(first_end.right() + px(1000.), first_end.center().y);
        assert_eq!(caret_toward(&mut handler, 12, first_line_end), Some(5));
        for offset in [0, 1, 3, 5] {
            let target = caret(&mut handler, offset).center();
            assert_eq!(caret_toward(&mut handler, 12, target), Some(offset));
        }
    }

    #[test]
    fn keyboard_ranges_preserve_cjk_emoji_and_combining_text() {
        let text = "前😀e\u{301}后";
        for (range, expected) in [
            (0..1, "前"),
            (1..3, "😀"),
            (3..5, "e\u{301}"),
            (5..6, "后"),
            (6..6, ""),
        ] {
            assert_eq!(utf16_slice(text, range), Some(expected));
        }
        for range in [1..2, 2..3, 0..7, Range { start: 5, end: 3 }] {
            assert_eq!(utf16_slice(text, range), None);
        }
    }

    #[test]
    fn native_ranges_distinguish_absent_empty_and_overflow() {
        assert_eq!(
            TextRange::from(Some(1..3)),
            TextRange {
                location: 1,
                length: 2
            }
        );
        assert_eq!(
            TextRange::from(Some(3..3)),
            TextRange {
                location: 3,
                length: 0
            }
        );
        assert_eq!(TextRange::from(None).range(), None);
        assert_eq!(TextRange::from(None).location, isize::MAX as usize);
        assert_eq!(
            TextRange {
                location: 3,
                length: 0
            }
            .range(),
            Some(3..3)
        );
        assert_eq!(
            TextRange {
                location: 1,
                length: 2
            }
            .range(),
            Some(1..3)
        );
        assert_eq!(
            TextRange {
                location: NOT_FOUND - 1,
                length: 4
            }
            .range(),
            None
        );
        assert_eq!(
            TextRange {
                location: isize::MAX as usize,
                length: 0
            }
            .range(),
            None
        );
    }
}
