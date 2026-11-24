//! Touch-only terminal keys that a software keyboard cannot produce.
//!
//! This owns only the sticky modifier state and presentation. Byte encoding
//! remains in `tcode_protocol::terminal::mappings`, fed by the same replicated
//! terminal modes as hardware key events.

use crate::material;
use crate::scroll::ScrollableElement as _;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};
use gpui::{
    App, Context, EventEmitter, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, px,
};
use gpui_base::h_flex;
use tcode_protocol::terminal::{
    KeyboardModes, TerminalMode,
    mappings::{self, Modifiers},
};

/// The scrolling symbol tail, ordered by how often a shell line needs the
/// character: what falls past the scroll edge is what is reached for least.
const SYMBOL_KEYS: [(&str, char); 7] = [
    ("terminal-key-dash", '-'),
    ("terminal-key-slash", '/'),
    ("terminal-key-pipe", '|'),
    ("terminal-key-tilde", '~'),
    ("terminal-key-colon", ':'),
    ("terminal-key-period", '.'),
    ("terminal-key-underscore", '_'),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalKey {
    Escape,
    Tab,
    Left,
    Up,
    Down,
    Right,
    Symbol(char),
    /// A one-tap Control combination: interrupt and end-of-file, the two the
    /// sticky Ctrl modifier is reached for most.
    Control(char),
}

impl TerminalKey {
    fn mapping_name(self) -> String {
        match self {
            Self::Escape => "escape".to_owned(),
            Self::Tab => "tab".to_owned(),
            Self::Left => "left".to_owned(),
            Self::Up => "up".to_owned(),
            Self::Down => "down".to_owned(),
            Self::Right => "right".to_owned(),
            Self::Symbol(symbol) | Self::Control(symbol) => symbol.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TerminalKeyBarEvent(pub(crate) TerminalKey);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StickyModifiers {
    control: bool,
    alt: bool,
}

impl StickyModifiers {
    fn take(&mut self) -> Modifiers {
        let modifiers = Modifiers {
            control: self.control,
            alt: self.alt,
            ..Modifiers::default()
        };
        *self = Self::default();
        modifiers
    }
}

pub(crate) struct TerminalKeyBar {
    terminal_focus: FocusHandle,
    encoder: TerminalKeyEncoder,
}

#[derive(Debug, Default)]
struct TerminalKeyEncoder {
    modifiers: StickyModifiers,
}

impl TerminalKeyEncoder {
    fn new() -> Self {
        Self {
            modifiers: StickyModifiers::default(),
        }
    }

    fn encode_key(
        &mut self,
        key: TerminalKey,
        mode: TerminalMode,
        keyboard_mode: KeyboardModes,
        modify_other_keys: Option<u8>,
    ) -> Vec<u8> {
        // A combo carries its own Control. It still clears whatever the user
        // had made sticky, so Ctrl · Ctrl+C never encodes a double modifier
        // and never leaves Ctrl armed for the next key.
        let control = matches!(key, TerminalKey::Control(_));
        let key = key.mapping_name();
        let mut modifiers = self.modifiers.take();
        modifiers.control |= control;
        mappings::key_bytes(
            &key,
            modifiers,
            mode,
            keyboard_mode,
            modify_other_keys,
            true,
        )
        .unwrap_or_else(|| key.into_bytes())
    }

    /// Encode committed software-keyboard text. Sticky modifiers apply to the
    /// first character only; every character still passes through the shared
    /// mapping so kitty and modifyOtherKeys modes match hardware input.
    fn encode_text(
        &mut self,
        text: &str,
        mode: TerminalMode,
        keyboard_mode: KeyboardModes,
        modify_other_keys: Option<u8>,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, character) in text.chars().enumerate() {
            let key = match character {
                '\n' | '\r' => "enter".to_owned(),
                '\t' => "tab".to_owned(),
                character => character.to_string(),
            };
            let modifiers = if index == 0 {
                self.modifiers.take()
            } else {
                Modifiers::default()
            };
            if let Some(encoded) = mappings::key_bytes(
                &key,
                modifiers,
                mode,
                keyboard_mode,
                modify_other_keys,
                true,
            ) {
                bytes.extend(encoded);
            } else {
                bytes.extend(character.to_string().into_bytes());
            }
        }
        bytes
    }
}

impl TerminalKeyBar {
    pub(crate) fn new(terminal_focus: FocusHandle) -> Self {
        Self {
            terminal_focus,
            encoder: TerminalKeyEncoder::new(),
        }
    }

    pub(crate) fn encode_key(
        &mut self,
        key: TerminalKey,
        mode: TerminalMode,
        keyboard_mode: KeyboardModes,
        modify_other_keys: Option<u8>,
    ) -> Vec<u8> {
        self.encoder
            .encode_key(key, mode, keyboard_mode, modify_other_keys)
    }

    pub(crate) fn encode_text(
        &mut self,
        text: &str,
        mode: TerminalMode,
        keyboard_mode: KeyboardModes,
        modify_other_keys: Option<u8>,
    ) -> Vec<u8> {
        self.encoder
            .encode_text(text, mode, keyboard_mode, modify_other_keys)
    }

    fn key_button(
        &self,
        id: &'static str,
        label: impl Into<gpui::SharedString>,
        accessibility_label: String,
        key: TerminalKey,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let focus = self.terminal_focus.clone();
        Self::key(id, label, accessibility_label).on_click(cx.listener(move |_, _, window, cx| {
            focus.focus(window, cx);
            window.request_virtual_keyboard();
            cx.emit(TerminalKeyBarEvent(key));
        }))
    }

    fn modifier_button(
        &self,
        id: &'static str,
        label: &'static str,
        accessibility_label: String,
        selected: bool,
        control: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let focus = self.terminal_focus.clone();
        Self::key(id, label, accessibility_label)
            .selected(selected)
            .on_click(cx.listener(move |this, _, window, cx| {
                if control {
                    this.encoder.modifiers.control = !this.encoder.modifiers.control;
                } else {
                    this.encoder.modifiers.alt = !this.encoder.modifiers.alt;
                }
                focus.focus(window, cx);
                window.request_virtual_keyboard();
                cx.notify();
            }))
    }

    /// A key leaves focus on the terminal it types into.
    fn key(
        id: &'static str,
        label: impl Into<gpui::SharedString>,
        accessibility_label: String,
    ) -> Button {
        Button::new(id)
            .ghost()
            .with_size(px(material::TOUCH_TARGET))
            .touch(true)
            .focusable(false)
            .label(label)
            .aria_label(accessibility_label)
            .debug_selector(move || id.into())
    }
}

impl EventEmitter<TerminalKeyBarEvent> for TerminalKeyBar {}

impl Render for TerminalKeyBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fixed = h_flex()
            .flex_none()
            .child(self.key_button(
                "terminal-key-escape",
                "Esc",
                crate::tr!("terminal.key_escape").into_owned(),
                TerminalKey::Escape,
                cx,
            ))
            .child(self.key_button(
                "terminal-key-tab",
                "Tab",
                crate::tr!("terminal.key_tab").into_owned(),
                TerminalKey::Tab,
                cx,
            ))
            .child(self.modifier_button(
                "terminal-key-control",
                "Ctrl",
                crate::tr!("terminal.key_control").into_owned(),
                self.encoder.modifiers.control,
                true,
                cx,
            ))
            .child(self.modifier_button(
                "terminal-key-alt",
                "Alt",
                crate::tr!("terminal.key_alt").into_owned(),
                self.encoder.modifiers.alt,
                false,
                cx,
            ));
        let arrows = [
            (
                "terminal-key-left",
                "←",
                "terminal.key_left",
                TerminalKey::Left,
            ),
            ("terminal-key-up", "↑", "terminal.key_up", TerminalKey::Up),
            (
                "terminal-key-down",
                "↓",
                "terminal.key_down",
                TerminalKey::Down,
            ),
            (
                "terminal-key-right",
                "→",
                "terminal.key_right",
                TerminalKey::Right,
            ),
        ];
        let mut tail = h_flex().flex_none().pr(px(material::COMPACT_PAGE_INSET));
        for (id, label, translation, key) in arrows {
            tail = tail.child(self.key_button(
                id,
                label,
                crate::tr!(translation).into_owned(),
                key,
                cx,
            ));
        }

        // The two combos the sticky Ctrl modifier is otherwise used for, one
        // tap each, right after the arrows they sit next to on a keyboard.
        for (id, label, translation, key) in [
            (
                "terminal-key-ctrl-c",
                "^C",
                "terminal.key_ctrl_c",
                TerminalKey::Control('c'),
            ),
            (
                "terminal-key-ctrl-d",
                "^D",
                "terminal.key_ctrl_d",
                TerminalKey::Control('d'),
            ),
        ] {
            tail = tail.child(self.key_button(
                id,
                label,
                crate::tr!(translation).into_owned(),
                key,
                cx,
            ));
        }

        for (id, symbol) in SYMBOL_KEYS {
            tail = tail.child(self.key_button(
                id,
                symbol.to_string(),
                crate::tr!("terminal.key_symbol", symbol = symbol).into_owned(),
                TerminalKey::Symbol(symbol),
                cx,
            ));
        }

        // The key tail scrolls to the window edge; its own trailing inset
        // replaces the bar's.
        material::bar("terminal-key-bar", material::Bar::Toolbar, true, cx)
            .debug_selector(|| "terminal-key-bar".into())
            .aria_label(crate::tr!("terminal.key_bar"))
            .pr_0()
            .min_w_0()
            .overflow_hidden()
            .text_body(true, cx)
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .child(fixed)
            .child(
                h_flex()
                    .id("terminal-key-scroll")
                    .debug_selector(|| "terminal-key-scroll".into())
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_x_scroll_area()
                    .child(tail),
            )
    }
}

pub(crate) fn should_show_terminal_key_bar(terminal_focused: bool, cx: &App) -> bool {
    crate::window_seam::is_mobile(cx) && terminal_focused
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext, div};

    #[test]
    fn sticky_terminal_modifiers_encode_once_using_replicated_modes() {
        {
            let mut state = TerminalKeyEncoder {
                modifiers: StickyModifiers {
                    control: true,
                    alt: false,
                },
            };
            assert_eq!(
                state.encode_text("ca", TerminalMode::empty(), KeyboardModes::NO_MODE, None),
                vec![0x03, b'a']
            );
            assert_eq!(state.modifiers, StickyModifiers::default());
            assert_eq!(
                state.encode_text("c", TerminalMode::empty(), KeyboardModes::NO_MODE, None),
                b"c"
            );
        }
        {
            let mut state = TerminalKeyEncoder {
                modifiers: StickyModifiers::default(),
            };
            assert_eq!(
                state.encode_key(
                    TerminalKey::Up,
                    TerminalMode::empty(),
                    KeyboardModes::NO_MODE,
                    None
                ),
                b"\x1b[A"
            );
            assert_eq!(
                state.encode_key(
                    TerminalKey::Up,
                    TerminalMode::APP_CURSOR,
                    KeyboardModes::NO_MODE,
                    None,
                ),
                b"\x1bOA"
            );
        }
        {
            let mut state = TerminalKeyEncoder {
                modifiers: StickyModifiers::default(),
            };
            for (key, byte) in [
                (TerminalKey::Control('c'), 0x03),
                (TerminalKey::Control('d'), 0x04),
            ] {
                assert_eq!(
                    state.encode_key(key, TerminalMode::empty(), KeyboardModes::NO_MODE, None),
                    vec![byte]
                );
            }

            state.modifiers = StickyModifiers {
                control: true,
                alt: false,
            };
            assert_eq!(
                state.encode_key(
                    TerminalKey::Control('c'),
                    TerminalMode::empty(),
                    KeyboardModes::NO_MODE,
                    None,
                ),
                vec![0x03],
                "a sticky Ctrl on top of the combo is still one Control-C"
            );
            assert_eq!(state.modifiers, StickyModifiers::default());
            assert_eq!(
                state.encode_text("c", TerminalMode::empty(), KeyboardModes::NO_MODE, None),
                b"c",
                "the combo must not leave Ctrl armed for the next key"
            );
        }
    }

    struct NarrowBarProbe {
        width: f32,
        bar: Entity<TerminalKeyBar>,
    }

    impl Render for NarrowBarProbe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(self.width)).child(self.bar.clone())
        }
    }

    #[gpui::test]
    fn phone_bar_scrolls_without_losing_pinned_keys_or_vertical_motion(cx: &mut TestAppContext) {
        use gpui::{PlatformInput, TouchEvent, TouchId, TouchPhase, point};
        cx.update(crate::theme::init);
        for width in [360., 393.] {
            let (_, cx) = cx.add_window_view(|_, cx| NarrowBarProbe {
                width,
                bar: cx.new(|cx| TerminalKeyBar::new(cx.focus_handle())),
            });
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
            let viewport = cx.debug_bounds("terminal-key-scroll").unwrap();
            let pinned = cx.debug_bounds("terminal-key-escape").unwrap();
            let first = cx.debug_bounds("terminal-key-left").unwrap();
            let last = cx.debug_bounds("terminal-key-underscore").unwrap();
            assert_eq!(
                cx.debug_bounds("terminal-key-bar").unwrap().size.width,
                px(width)
            );
            assert!(pinned.size.width >= px(material::TOUCH_TARGET));
            assert!(pinned.size.height >= px(material::TOUCH_TARGET));
            assert!(
                last.right() > viewport.right(),
                "content must exceed the viewport"
            );
            assert!(viewport.right() <= px(width));
            let send = |phase, x, y, cx: &mut VisualTestContext| {
                cx.update(|window, cx| {
                    window.dispatch_event(
                        PlatformInput::Touch(TouchEvent {
                            id: TouchId(1),
                            phase,
                            position: point(px(x), px(y)),
                            predicted_position: None,
                            force: None,
                        }),
                        cx,
                    );
                    let _ = window.draw(cx);
                });
            };
            send(TouchPhase::Started, width - 20., 20., cx);
            send(TouchPhase::Moved, 190., 20., cx);
            send(TouchPhase::Cancelled, 190., 20., cx);
            let moved = cx.debug_bounds("terminal-key-left").unwrap();
            assert!(moved.left() < first.left(), "horizontal swipe must scroll");
            assert_eq!(moved.top(), first.top());
            assert_eq!(cx.debug_bounds("terminal-key-escape").unwrap(), pinned);
            let inside = f32::from(viewport.left()) + 10.;
            send(TouchPhase::Started, inside, 20., cx);
            send(TouchPhase::Moved, width + 300., 20., cx);
            send(TouchPhase::Cancelled, width + 300., 20., cx);
            assert_eq!(cx.debug_bounds("terminal-key-left").unwrap(), first);
        }
    }

    #[gpui::test]
    fn bar_renders_only_for_a_focused_soft_keyboard_terminal(cx: &mut TestAppContext) {
        use gpui::Focusable as _;
        use tcode_protocol::{EventEnvelope, HostMessage, ServerEvent, Topic, encode_line};
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("terminal-keybar");
        let host = tcode_runtime::pipe::spawn_host(
            tcode_services::store::SessionStore::open_at(root.clone()).unwrap(),
            tcode_runtime::pipe::HostServices::default(),
        )
        .unwrap();
        let mut status = smol::block_on(host.update_state_for_test(|state, cx| {
            let id = state.start_draft("terminal".into(), std::env::temp_dir(), cx);
            state.session_status_snapshot(&id).unwrap()
        }))
        .unwrap();
        host.shutdown_blocking().unwrap();
        std::fs::remove_dir_all(root).unwrap();
        status.terminals = vec![tcode_protocol::TerminalStatus {
            id: 7,
            title: "shell".into(),
            exited: false,
        }];
        status.active_terminal_id = Some(7);
        status.terminal_open = true;
        let session_id = status.session_id.clone();
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump.pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            crate::store::WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, Vec::new(), cx);
        store.update(cx, |store, _| store.select_session(session_id.clone()));
        let (drawer, cx) = cx.add_window_view(|window, cx| {
            crate::terminal_drawer::TerminalDrawer::new(store.clone(), window, cx)
        });
        for (topic, event) in [
            (
                Topic::SessionStatus { session_id },
                ServerEvent::SessionStatusReplaced(Box::new(status)),
            ),
            (
                Topic::Terminal { terminal_id: 7 },
                ServerEvent::TerminalFrame {
                    terminal_id: 7,
                    frame: Box::new(tcode_protocol::terminal::TerminalFrame {
                        cols: 1,
                        rows: 1,
                        styles: vec![tcode_protocol::terminal::TerminalStyle::default()],
                        visible: vec![tcode_protocol::terminal::TerminalRow {
                            cells: vec![tcode_protocol::terminal::TerminalCell {
                                text: "x".into(),
                                ..Default::default()
                            }],
                            wrapped: false,
                        }],
                        ..Default::default()
                    }),
                },
            ),
        ] {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        assert_eq!(
            store.read_with(cx, |store, _| store.with_terminal_workspace(|workspace| {
                workspace.active().map(|entry| entry.id)
            })),
            Some(Some(7)),
            "the drawer must have a replicated active terminal before focus is tested"
        );
        let focus = drawer.read_with(cx, |drawer, cx| drawer.focus_handle(cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        for (mobile, focused, visible) in [
            (false, true, false),
            (true, false, false),
            (true, true, true),
            (true, false, false),
        ] {
            cx.update(|window, cx| {
                crate::window_seam::override_mobile_for_test(cx, mobile);
                if focused {
                    focus.focus(window, cx);
                } else {
                    window.blur(cx);
                }
                window.refresh();
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                _ = window.draw(cx);
            });
            let bar = cx.debug_bounds("terminal-key-bar");
            assert_eq!(bar.is_some(), visible, "mobile={mobile}, focused={focused}");
        }
    }
}
