use crate::widgets::kbd::Kbd;
use gpui::{Action, App, KeyBinding, Keystroke, Modifiers, NoAction};
use serde::Deserialize;

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode, no_json)]
pub(crate) enum NavigateThread {
    Index(usize),
    Next,
    Previous,
}

pub(crate) fn init(cx: &mut App) {
    cx.on_action(crate::shell::navigate_thread);
    // With nothing focused the shell's own Undo listener is off the dispatch path.
    cx.on_action(crate::shell::undo_thread_action);
    cx.bind_keys([
        KeyBinding::new("secondary-z", crate::sidebar::ThreadUndo, None),
        KeyBinding::new("secondary-c", gpui_base::input::Copy, Some("TcodeDiff")),
        #[cfg(target_os = "ios")]
        KeyBinding::new("cmd-c", gpui_base::input::Copy, Some("TcodeDiff")),
        // GPUI gives later bindings precedence, including a global binding over
        // the editor's earlier contextual one. Keep the editor's native action.
        KeyBinding::new("secondary-z", gpui_base::input::Undo, Some("Input")),
    ]);
    // gpui-base gates Apple's editor bindings to macOS. iOS hardware
    // keyboards use Command too, while GPUI's secondary modifier is Control.
    #[cfg(target_os = "ios")]
    cx.bind_keys([
        KeyBinding::new("cmd-a", gpui_base::input::SelectAll, Some("Input")),
        KeyBinding::new("cmd-c", gpui_base::input::Copy, Some("Input")),
        KeyBinding::new("cmd-x", gpui_base::input::Cut, Some("Input")),
        KeyBinding::new("cmd-v", gpui_base::input::Paste, Some("Input")),
        KeyBinding::new("cmd-z", gpui_base::input::Undo, Some("Input")),
        KeyBinding::new("cmd-shift-z", gpui_base::input::Redo, Some("Input")),
    ]);
    // A browser hands the clipboard only to its own paste event, which the
    // key fires once no binding claims it; gpui-base's Paste would read the
    // clipboard itself, which a page without a secure context cannot.
    #[cfg(target_family = "wasm")]
    cx.bind_keys([KeyBinding::new("ctrl-v", NoAction, Some("Input"))]);
    for number in 1..=9 {
        let key = format!("secondary-{number}");
        cx.bind_keys([
            KeyBinding::new(&key, NavigateThread::Index(number - 1), None),
            KeyBinding::new(&key, NoAction, Some("ModelPicker || ModelPicker > Input")),
        ]);
    }
    cx.bind_keys([
        KeyBinding::new("ctrl-tab", NavigateThread::Next, None),
        KeyBinding::new("ctrl-shift-tab", NavigateThread::Previous, None),
    ]);
    // Bindings dispatch before key-down listeners, and the terminal forwards
    // raw keystrokes (Tab, Ctrl-C as an interrupt) from its listener, so the
    // window root's focus traversal and selection copy must not claim them.
    let terminal = Some(crate::terminal_drawer::CONTEXT);
    cx.bind_keys([
        KeyBinding::new("secondary-z", NoAction, terminal),
        KeyBinding::new("tab", NoAction, terminal),
        KeyBinding::new("shift-tab", NoAction, terminal),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", NoAction, terminal),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", NoAction, terminal),
    ]);
}

/// Format a shortcut using GPUI's semantic secondary modifier.
///
/// The secondary modifier is Command on macOS and Control on Windows/Linux.
pub(crate) fn format_secondary_shortcut(key: &str) -> String {
    Kbd::format(&Keystroke {
        modifiers: Modifiers::secondary_key(),
        key: key.to_owned(),
        key_char: None,
    })
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{
        AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
        KeyDownEvent, ParentElement as _, Render, TestAppContext, VisualTestContext, Window, div,
    };

    struct Harness {
        terminal: FocusHandle,
        composer: FocusHandle,
        keys: Vec<String>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .child(
                    div()
                        .key_context(crate::terminal_drawer::CONTEXT)
                        .track_focus(&self.terminal)
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, _| {
                            this.keys.push(event.keystroke.unparse());
                        })),
                )
                .child(
                    div()
                        .key_context(crate::composer::CONTEXT)
                        .child(div().track_focus(&self.composer)),
                )
        }
    }

    fn mount(cx: &mut TestAppContext) -> (Entity<Harness>, &mut VisualTestContext) {
        cx.update(|cx| {
            crate::theme::init(cx);
            super::init(cx);
        });
        let built = Rc::new(RefCell::new(None));
        let capture = built.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let harness = cx.new(|cx| Harness {
                terminal: cx.focus_handle().tab_stop(true),
                composer: cx.focus_handle().tab_stop(true),
                keys: Vec::new(),
            });
            *capture.borrow_mut() = Some(harness.clone());
            gpui_base::Root::new(harness, window, cx)
        });
        let harness = built.borrow_mut().take().unwrap();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (harness, cx)
    }

    #[gpui::test]
    fn root_focus_traversal_leaves_terminal_its_keys(cx: &mut TestAppContext) {
        let (harness, cx) = mount(cx);
        let terminal = harness.read_with(cx, |harness, _| harness.terminal.clone());
        cx.update(|window, cx| terminal.focus(window, cx));
        cx.simulate_keystrokes("tab shift-tab");
        harness.read_with(cx, |harness, _| {
            assert_eq!(harness.keys, ["tab", "shift-tab"]);
        });
        assert!(cx.update(|window, _| terminal.is_focused(window)));
    }
}
