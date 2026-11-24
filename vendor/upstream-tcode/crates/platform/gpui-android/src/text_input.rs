use gpui::{KeyDownEvent, Keystroke};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// Android's Editable and GPUI use UTF-16 for selections and composing ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextInputState {
    pub text: String,
    pub selection: Range<usize>,
    pub marked: Option<Range<usize>>,
}

#[cfg(target_os = "android")]
impl TextInputState {
    pub(crate) fn read(handler: &mut gpui::PlatformInputHandler) -> Option<Self> {
        let length = handler.text_length_utf16()?;
        Some(Self {
            text: handler.text_for_range(0..length, &mut None)?,
            selection: handler.selected_text_range(true)?.range,
            marked: handler.marked_text_range(),
        })
    }

    pub(crate) fn apply(&self, old: &Self, handler: &mut gpui::PlatformInputHandler) {
        let (old_range, new_range) = replacement_ranges(old, self);
        let text = utf16_slice(&self.text, new_range.clone());
        if self.marked.as_ref() == Some(&new_range)
            && (self.text != old.text || self.marked != old.marked)
        {
            handler.replace_and_mark_text_in_range(Some(old_range), text, None);
        } else {
            if self.text != old.text {
                handler.replace_text_in_range(Some(old_range), text);
            }
            if self.marked != handler.marked_text_range() {
                if let Some(marked) = &self.marked {
                    handler.replace_and_mark_text_in_range(
                        Some(marked.clone()),
                        utf16_slice(&self.text, marked.clone()),
                        None,
                    );
                } else {
                    handler.unmark_text();
                }
            }
        }
        handler.set_selected_text_range(self.selection.clone());
    }
}

fn utf16_slice(text: &str, range: Range<usize>) -> &str {
    let mut offset = 0;
    let start = text
        .char_indices()
        .find_map(|(index, ch)| {
            let found = (offset == range.start).then_some(index);
            offset += ch.len_utf16();
            found
        })
        .unwrap_or(text.len());
    let length = range.end - range.start;
    let mut offset = 0;
    let end = text[start..]
        .char_indices()
        .find_map(|(index, ch)| {
            let found = (offset == length).then_some(start + index);
            offset += ch.len_utf16();
            found
        })
        .unwrap_or(text.len());
    &text[start..end]
}

// Include the whole preedit so each composing update stays in its IME transaction.
fn replacement_ranges(old: &TextInputState, new: &TextInputState) -> (Range<usize>, Range<usize>) {
    let old_length = old.text.encode_utf16().count();
    let new_length = new.text.encode_utf16().count();
    let mut prefix = old
        .text
        .chars()
        .zip(new.text.chars())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf16())
        .sum::<usize>();
    if let Some(marked) = &old.marked {
        prefix = prefix.min(marked.start);
    }
    if let Some(marked) = &new.marked {
        prefix = prefix.min(marked.start);
    }
    let mut suffix_limit = (old_length - prefix).min(new_length - prefix);
    if let Some(marked) = &old.marked {
        suffix_limit = suffix_limit.min(old_length - marked.end);
    }
    if let Some(marked) = &new.marked {
        suffix_limit = suffix_limit.min(new_length - marked.end);
    }
    let mut suffix = 0;
    for (a, b) in old.text.chars().rev().zip(new.text.chars().rev()) {
        if a != b || suffix + a.len_utf16() > suffix_limit {
            break;
        }
        suffix += a.len_utf16();
    }
    (prefix..old_length - suffix, prefix..new_length - suffix)
}

/// A new revision invalidates queued IME edits after GPUI changes the draft or cursor.
#[derive(Default)]
pub(crate) struct InputSync {
    pub revision: u64,
    pub serial: u64,
    pub state: Option<TextInputState>,
}

impl InputSync {
    pub fn observe(&mut self, state: Option<TextInputState>) -> bool {
        if self.state == state {
            return false;
        }
        self.revision += 1;
        self.state = state;
        true
    }

    pub fn accept(&mut self, revision: u64, serial: u64) -> bool {
        self.serial = self.serial.max(serial);
        self.revision == revision
    }
}

fn previous_grapheme_range(before_cursor: &str, cursor: usize) -> Range<usize> {
    let length = before_cursor
        .graphemes(true)
        .next_back()
        .map_or(0, |grapheme| grapheme.encode_utf16().count());
    cursor.saturating_sub(length)..cursor
}

fn delete_from_composition(text: &str, range: Range<usize>) -> (String, Range<usize>) {
    let cursor = range.start;
    let mut units = text.encode_utf16().collect::<Vec<_>>();
    units.drain(range);
    (String::from_utf16_lossy(&units), cursor..cursor)
}

// None distinguishes a key-oriented handler from an empty editable field.
fn backward_delete_range(
    selection: Option<Range<usize>>,
    mut text_before: impl FnMut(usize) -> Option<String>,
) -> Option<Range<usize>> {
    let range = selection?;
    if range.is_empty() {
        Some(previous_grapheme_range(
            &text_before(range.start)?,
            range.start,
        ))
    } else {
        Some(range)
    }
}

#[cfg(target_os = "android")]
pub(crate) fn delete_backward(handler: &mut gpui::PlatformInputHandler) -> bool {
    let selection = handler
        .selected_text_range(true)
        .map(|selection| selection.range);
    let Some(range) = backward_delete_range(selection, |cursor| {
        handler.text_for_range(0..cursor, &mut None)
    }) else {
        return false;
    };
    if range.is_empty() {
        return true;
    }
    if let Some(marked) = handler.marked_text_range()
        && marked.start <= range.start
        && range.end <= marked.end
        && let Some(text) = handler.text_for_range(marked.clone(), &mut None)
    {
        // Keep both the remaining preedit and its cursor coherent with the IME.
        let (text, selection) =
            delete_from_composition(&text, range.start - marked.start..range.end - marked.start);
        handler.replace_and_mark_text_in_range(Some(marked), &text, Some(selection));
    } else {
        handler.replace_text_in_range(Some(range), "");
    }
    true
}

/// Plain multiline Enter is text; other control keys must still reach bindings.
pub(crate) fn ime_key_down(mut keystroke: Keystroke, multi_line: bool) -> KeyDownEvent {
    if multi_line && keystroke.key == "enter" && keystroke.modifiers == gpui::Modifiers::default() {
        keystroke.key_char = Some("\n".into());
    }
    let prefer_character_input = keystroke.key_char.is_some()
        && !keystroke.modifiers.control
        && !keystroke.modifiers.platform
        && !keystroke.modifiers.alt;
    KeyDownEvent {
        keystroke,
        is_held: false,
        prefer_character_input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autocomplete_replaces_the_word_and_preserves_surrounding_unicode() {
        for (before, after, old_marked, new_marked, expected_old, expected_new) in [
            ("an exampl here", "an example here", None, None, 9..9, 9..10),
            (
                "an exmple here",
                "an example here",
                Some(3..9),
                Some(3..10),
                3..9,
                3..10,
            ),
            (
                "an exmple here",
                "an example here",
                Some(3..9),
                None,
                3..9,
                3..10,
            ),
            ("😀 exampl", "😀 example", None, None, 9..9, 9..10),
            ("😀word中", "😀中", None, None, 2..6, 2..2),
        ] {
            let old = TextInputState {
                text: before.into(),
                selection: 0..0,
                marked: old_marked,
            };
            let new = TextInputState {
                text: after.into(),
                selection: 0..0,
                marked: new_marked,
            };
            let (old_range, new_range) = replacement_ranges(&old, &new);
            assert_eq!(old_range, expected_old);
            assert_eq!(new_range, expected_new);
            let mut result = before.encode_utf16().collect::<Vec<_>>();
            result.splice(old_range, utf16_slice(after, new_range).encode_utf16());
            assert_eq!(String::from_utf16(&result).unwrap(), after);
        }
    }

    #[test]
    fn app_clear_invalidates_queued_autocomplete_without_rejecting_ordinary_typing() {
        let draft = TextInputState {
            text: "exampl".into(),
            selection: 6..6,
            marked: None,
        };
        let mut sync = InputSync::default();
        assert!(sync.observe(Some(draft.clone())));
        let keyboard_revision = sync.revision;
        assert!(sync.accept(keyboard_revision, 1));
        assert!(!sync.observe(Some(draft)));
        assert!(sync.accept(keyboard_revision, 2));
        assert!(sync.observe(Some(TextInputState {
            text: String::new(),
            selection: 0..0,
            marked: None
        })));
        assert!(!sync.accept(keyboard_revision, 3));
        assert!(sync.accept(sync.revision, 4));
        assert_eq!(sync.serial, 4);
    }

    #[test]
    fn ime_routes_text_control_keys_and_absent_editors() {
        assert_eq!(backward_delete_range(None, |_| None), None);
        assert_eq!(backward_delete_range(Some(0..0), |_| None), None);
        // An empty editor consumes deletion; it must not receive a second key.
        assert_eq!(
            backward_delete_range(Some(0..0), |_| Some(String::new())),
            Some(0..0)
        );
        for key in [
            "backspace",
            "delete",
            "enter",
            "left",
            "right",
            "up",
            "down",
        ] {
            let event = ime_key_down(
                Keystroke {
                    key: key.into(),
                    key_char: None,
                    modifiers: Default::default(),
                },
                false,
            );
            assert_eq!(event.keystroke.key, key);
            assert!(!event.prefer_character_input);
        }
        let mut stroke = Keystroke {
            key: "a".into(),
            key_char: Some("a".into()),
            modifiers: Default::default(),
        };
        assert!(ime_key_down(stroke.clone(), false).prefer_character_input);
        stroke.modifiers.control = true;
        assert!(!ime_key_down(stroke, false).prefer_character_input);
        let enter = Keystroke {
            key: "enter".into(),
            key_char: None,
            modifiers: Default::default(),
        };
        let event = ime_key_down(enter.clone(), true);
        assert_eq!(event.keystroke.key_char.as_deref(), Some("\n"));
        assert!(event.prefer_character_input);
        let event = ime_key_down(enter.clone(), false);
        assert_eq!(event.keystroke.key_char, None);
        assert!(!event.prefer_character_input);
        for modifiers in [
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        ] {
            let event = ime_key_down(
                Keystroke {
                    modifiers,
                    ..enter.clone()
                },
                true,
            );
            assert_eq!(event.keystroke.key_char, None);
            assert!(!event.prefer_character_input);
        }
    }

    #[test]
    fn backspace_uses_utf16_graphemes_and_preserves_remaining_preedit() {
        assert_eq!(delete_from_composition("中文", 1..2), ("中".into(), 1..1));
        assert_eq!(
            delete_from_composition("nihao", 1..2),
            ("nhao".into(), 1..1)
        );
        assert_eq!(delete_from_composition("😀文", 0..2), ("文".into(), 0..0));
        assert_eq!(delete_from_composition("中", 0..1), (String::new(), 0..0));
        for (text, expected) in [
            ("", 0..0),
            ("abc", 2..3),
            ("中文", 1..2),
            ("a😀", 1..3),
            ("ae\u{301}", 1..3),
            ("a👨‍👩‍👧‍👦", 1..12),
        ] {
            assert_eq!(
                backward_delete_range(
                    Some(text.encode_utf16().count()..text.encode_utf16().count()),
                    |_| Some(text.into())
                ),
                Some(expected)
            );
        }
    }
}
