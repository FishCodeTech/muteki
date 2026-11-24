//! Translates physical presses and plain UIKit deletion callbacks into GPUI keys.
use gpui::{KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, PlatformInput};
use std::cell::Cell;

#[derive(Default)]
pub(crate) struct Keyboard {
    modifiers: Cell<Modifiers>,
}

impl Keyboard {
    pub(crate) fn modifiers(&self) -> Modifiers {
        self.modifiers.get()
    }

    pub(crate) fn delete_backward(&self) -> [PlatformInput; 2] {
        let keystroke = Keystroke {
            // UIKeyInput has no modifiers; modified physical presses use key_event.
            modifiers: Modifiers::default(),
            key: "backspace".to_owned(),
            key_char: None,
        };
        [
            PlatformInput::KeyDown(KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            }),
            PlatformInput::KeyUp(KeyUpEvent { keystroke }),
        ]
    }

    pub(crate) fn key_event(
        &self,
        key: String,
        key_char: Option<String>,
        modifiers: Modifiers,
        down: bool,
        repeat: bool,
    ) -> PlatformInput {
        self.modifiers.set(modifiers);
        let keystroke = Keystroke {
            modifiers,
            key,
            key_char,
        };
        if down {
            PlatformInput::KeyDown(KeyDownEvent {
                prefer_character_input: keystroke.key_char.as_deref().is_some_and(|text| {
                    !text.is_empty() && text.chars().all(|character| !character.is_control())
                }) && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform,
                keystroke,
                is_held: repeat,
            })
        } else {
            PlatformInput::KeyUp(KeyUpEvent { keystroke })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{EntityInputHandler as _, TestAppContext};
    use gpui_base::input::TextareaState;

    #[gpui::test]
    fn software_delete_after_a_modified_physical_press_removes_one_character(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_base::init);
        let (input, cx) = cx.add_window_view(TextareaState::new);
        let keyboard = Keyboard::default();
        for modifiers in [
            Modifiers::command(),
            Modifiers::control(),
            Modifiers::alt(),
            Modifiers::shift(),
        ] {
            cx.update(|window, cx| {
                input.update(cx, |input, cx| {
                    input.set_value("ab", window, cx);
                    input.focus(window, cx);
                    input.replace_text_in_range(Some(0..2), "ab", window, cx);
                });
                window.draw(cx).clear(cx);
            });
            for down in [true, false] {
                keyboard.key_event("v".into(), Some("v".into()), modifiers, down, false);
            }
            assert_eq!(keyboard.modifiers(), modifiers);
            for event in keyboard.delete_backward() {
                match event {
                    PlatformInput::KeyDown(event) => cx.simulate_event(event),
                    PlatformInput::KeyUp(event) => cx.simulate_event(event),
                    _ => unreachable!(),
                }
            }
            assert_eq!(
                input.read_with(cx, |input, _| input.value()),
                "a",
                "{modifiers:?}"
            );
        }
    }

    #[gpui::test]
    fn physical_enter_submits_the_unchanged_draft_once(cx: &mut TestAppContext) {
        use std::{cell::RefCell, rc::Rc};
        cx.update(gpui_base::init);
        let (input, cx) =
            cx.add_window_view(|window, cx| TextareaState::new(window, cx).submit_on_enter(true));
        let submissions = Rc::new(RefCell::new(Vec::new()));
        let observed = submissions.clone();
        cx.update(|window, cx| {
            cx.subscribe(&input, move |input, event, cx| {
                if matches!(
                    event,
                    gpui_base::input::InputEvent::PressEnter { shift: false, .. }
                ) {
                    observed
                        .borrow_mut()
                        .push(input.read(cx).value().to_string());
                }
            })
            .detach();
            input.update(cx, |input, cx| {
                input.set_value("hello", window, cx);
                input.set_selected_range(2..2, cx);
                input.focus(window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let keyboard = Keyboard::default();
        for down in [true, false] {
            match keyboard.key_event(
                "enter".into(),
                Some("\n".into()),
                Modifiers::default(),
                down,
                false,
            ) {
                PlatformInput::KeyDown(event) => cx.simulate_event(event),
                PlatformInput::KeyUp(event) => cx.simulate_event(event),
                _ => unreachable!(),
            }
        }
        assert_eq!(&*submissions.borrow(), &["hello"]);
        assert_eq!(input.read_with(cx, |input, _| input.value()), "hello");
    }
}
