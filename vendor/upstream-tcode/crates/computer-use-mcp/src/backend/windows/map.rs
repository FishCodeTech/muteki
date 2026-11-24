use crate::backend::{KeyChord, RootKind};
use crate::outline::Frame;

pub(super) const UIA_BUTTON: i32 = 50_000;
pub(super) const UIA_CALENDAR: i32 = 50_001;
pub(super) const UIA_CHECK_BOX: i32 = 50_002;
pub(super) const UIA_COMBO_BOX: i32 = 50_003;
pub(super) const UIA_EDIT: i32 = 50_004;
pub(super) const UIA_HYPERLINK: i32 = 50_005;
pub(super) const UIA_IMAGE: i32 = 50_006;
pub(super) const UIA_LIST_ITEM: i32 = 50_007;
pub(super) const UIA_LIST: i32 = 50_008;
pub(super) const UIA_MENU: i32 = 50_009;
pub(super) const UIA_MENU_BAR: i32 = 50_010;
pub(super) const UIA_MENU_ITEM: i32 = 50_011;
pub(super) const UIA_PROGRESS_BAR: i32 = 50_012;
pub(super) const UIA_RADIO_BUTTON: i32 = 50_013;
pub(super) const UIA_SCROLL_BAR: i32 = 50_014;
pub(super) const UIA_SLIDER: i32 = 50_015;
pub(super) const UIA_SPINNER: i32 = 50_016;
pub(super) const UIA_STATUS_BAR: i32 = 50_017;
pub(super) const UIA_TAB: i32 = 50_018;
pub(super) const UIA_TAB_ITEM: i32 = 50_019;
pub(super) const UIA_TEXT: i32 = 50_020;
pub(super) const UIA_TOOL_BAR: i32 = 50_021;
pub(super) const UIA_TOOL_TIP: i32 = 50_022;
pub(super) const UIA_TREE: i32 = 50_023;
pub(super) const UIA_TREE_ITEM: i32 = 50_024;
pub(super) const UIA_CUSTOM: i32 = 50_025;
pub(super) const UIA_GROUP: i32 = 50_026;
pub(super) const UIA_THUMB: i32 = 50_027;
pub(super) const UIA_DATA_GRID: i32 = 50_028;
pub(super) const UIA_DATA_ITEM: i32 = 50_029;
pub(super) const UIA_DOCUMENT: i32 = 50_030;
pub(super) const UIA_SPLIT_BUTTON: i32 = 50_031;
pub(super) const UIA_WINDOW: i32 = 50_032;
pub(super) const UIA_PANE: i32 = 50_033;
pub(super) const UIA_HEADER: i32 = 50_034;
pub(super) const UIA_HEADER_ITEM: i32 = 50_035;
pub(super) const UIA_TABLE: i32 = 50_036;
pub(super) const UIA_TITLE_BAR: i32 = 50_037;
pub(super) const UIA_SEPARATOR: i32 = 50_038;
pub(super) const UIA_SEMANTIC_ZOOM: i32 = 50_039;
pub(super) const UIA_APP_BAR: i32 = 50_040;

pub(super) fn role_for_control_type(control_type: i32) -> &'static str {
    match control_type {
        UIA_BUTTON => "button",
        UIA_CALENDAR => "calendar",
        UIA_CHECK_BOX => "checkbox",
        UIA_COMBO_BOX => "combo_box",
        UIA_EDIT => "text_field",
        UIA_HYPERLINK => "link",
        UIA_IMAGE => "image",
        UIA_LIST_ITEM => "row",
        UIA_LIST => "list",
        UIA_MENU => "menu",
        UIA_MENU_BAR => "menu_bar",
        UIA_MENU_ITEM => "menu_item",
        UIA_PROGRESS_BAR => "progress_indicator",
        UIA_RADIO_BUTTON => "radio_button",
        UIA_SCROLL_BAR => "scroll_bar",
        UIA_SLIDER => "slider",
        UIA_SPINNER => "incrementor",
        UIA_STATUS_BAR => "group",
        UIA_TAB => "tab_group",
        UIA_TAB_ITEM => "tab",
        UIA_TEXT => "static_text",
        UIA_TOOL_BAR | UIA_APP_BAR => "toolbar",
        UIA_TOOL_TIP => "help_tag",
        UIA_TREE => "outline",
        UIA_TREE_ITEM => "row",
        UIA_CUSTOM | UIA_GROUP | UIA_PANE | UIA_TITLE_BAR | UIA_SEMANTIC_ZOOM => "group",
        UIA_THUMB => "value_indicator",
        UIA_DATA_GRID | UIA_TABLE => "table",
        UIA_DATA_ITEM => "cell",
        UIA_DOCUMENT => "text_area",
        UIA_SPLIT_BUTTON => "pop_up_button",
        UIA_WINDOW => "window",
        UIA_HEADER => "group",
        UIA_HEADER_ITEM => "column",
        UIA_SEPARATOR => "splitter",
        _ => "unknown",
    }
}

pub(super) fn root_kind_for_control_type(
    control_type: i32,
    localized_control_type: &str,
    class_name: &str,
) -> Option<RootKind> {
    let hint = format!("{localized_control_type} {class_name}").to_ascii_lowercase();
    if hint.contains("sheet") {
        return Some(RootKind::Sheet);
    }
    if control_type == UIA_MENU || hint.contains("menu") {
        return Some(RootKind::Menu);
    }
    if hint.contains("dialog") || class_name.eq_ignore_ascii_case("#32770") {
        return Some(RootKind::Dialog);
    }
    if matches!(control_type, UIA_TOOL_TIP | UIA_APP_BAR)
        || hint.contains("popover")
        || hint.contains("flyout")
        || hint.contains("tooltip")
    {
        return Some(RootKind::Popover);
    }
    matches!(control_type, UIA_WINDOW | UIA_PANE).then_some(RootKind::Window)
}

pub(super) fn frame_from_uia_rect(left: i32, top: i32, right: i32, bottom: i32) -> Frame {
    Frame {
        x: f64::from(left),
        y: f64::from(top),
        w: (i64::from(right) - i64::from(left)).max(0) as f64,
        h: (i64::from(bottom) - i64::from(top)).max(0) as f64,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct PatternSupport {
    pub invoke: bool,
    pub value_writable: bool,
    pub range_value_writable: bool,
    pub toggle: bool,
    pub expand_collapse: bool,
    pub scroll: bool,
    pub scroll_item: bool,
    pub selection_item: bool,
    pub legacy_default: bool,
}

pub(super) fn actions_for_patterns(support: PatternSupport) -> Vec<String> {
    let mut actions = Vec::new();
    if support.invoke
        || support.toggle
        || support.expand_collapse
        || support.selection_item
        || support.legacy_default
    {
        actions.push("press");
    }
    if support.value_writable {
        actions.push("set_text");
    }
    if support.range_value_writable {
        actions.extend(["set_text", "set_value"]);
    }
    if support.toggle {
        actions.push("toggle");
    }
    if support.expand_collapse {
        actions.extend(["collapse", "expand"]);
    }
    if support.scroll {
        actions.push("scroll");
    }
    if support.scroll_item {
        actions.push("scroll_to_visible");
    }
    if support.selection_item {
        actions.push("select");
    }
    actions.sort_unstable();
    actions.dedup();
    actions.into_iter().map(str::to_string).collect()
}

/// Converts the shared Windows virtual-key representation into the expression
/// accepted by `uiautomation::inputs::Keyboard::send_keys`.
pub(super) fn key_expression_for_chord(chord: KeyChord) -> Result<String, String> {
    if chord.modifiers.function {
        return Err("the fn modifier has no Windows virtual-key equivalent".into());
    }

    let mut expression = String::new();
    if chord.modifiers.command {
        expression.push_str("{win}");
    }
    if chord.modifiers.control {
        expression.push_str("{ctrl}");
    }
    if chord.modifiers.option {
        expression.push_str("{alt}");
    }
    if chord.modifiers.shift {
        expression.push_str("{shift}");
    }

    match library_key_for_virtual_key(chord.keycode)? {
        LibraryKey::Character(character) if expression.is_empty() => expression.push(character),
        LibraryKey::Character(character) => {
            expression.push('(');
            expression.push(character);
            expression.push(')');
        }
        LibraryKey::Special(name) => {
            expression.push('{');
            expression.push_str(name);
            expression.push('}');
        }
    }
    Ok(expression)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibraryKey {
    Character(char),
    Special(&'static str),
}

fn library_key_for_virtual_key(keycode: u16) -> Result<LibraryKey, String> {
    if let Ok(letter) = u8::try_from(keycode)
        && letter.is_ascii_uppercase()
    {
        return Ok(LibraryKey::Character(char::from(
            letter.to_ascii_lowercase(),
        )));
    }
    if let Ok(digit) = u8::try_from(keycode)
        && digit.is_ascii_digit()
    {
        return Ok(LibraryKey::Character(char::from(digit)));
    }
    if (0x70..=0x83).contains(&keycode) {
        const FUNCTION_KEYS: [&str; 20] = [
            "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13",
            "f14", "f15", "f16", "f17", "f18", "f19", "f20",
        ];
        return Ok(LibraryKey::Special(
            FUNCTION_KEYS[usize::from(keycode - 0x70)],
        ));
    }

    Ok(match keycode {
        0x08 => LibraryKey::Special("backspace"),
        0x09 => LibraryKey::Special("tab"),
        0x0D => LibraryKey::Special("enter"),
        0x1B => LibraryKey::Special("escape"),
        0x20 => LibraryKey::Special("space"),
        0x21 => LibraryKey::Special("page_up"),
        0x22 => LibraryKey::Special("page_down"),
        0x23 => LibraryKey::Special("end"),
        0x24 => LibraryKey::Special("home"),
        0x25 => LibraryKey::Special("left"),
        0x26 => LibraryKey::Special("up"),
        0x27 => LibraryKey::Special("right"),
        0x28 => LibraryKey::Special("down"),
        0x2D => LibraryKey::Special("insert"),
        0x2E => LibraryKey::Special("delete"),
        0xBA => LibraryKey::Character(';'),
        0xBB => LibraryKey::Character('='),
        0xBC => LibraryKey::Character(','),
        0xBD => LibraryKey::Character('-'),
        0xBE => LibraryKey::Character('.'),
        0xBF => LibraryKey::Character('/'),
        0xC0 => LibraryKey::Character('`'),
        0xDB => LibraryKey::Character('['),
        0xDC => LibraryKey::Character('\\'),
        0xDD => LibraryKey::Character(']'),
        0xDE => LibraryKey::Character('\''),
        _ => {
            return Err(format!(
                "unsupported Windows virtual-key code: 0x{keycode:02X}"
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::KeyModifiers;

    #[test]
    fn uia_controls_and_hints_translate_to_shared_roles_and_root_kinds() {
        for (control, localized, class, role, kind) in [
            (UIA_BUTTON, "button", "", "button", None),
            (UIA_EDIT, "edit", "", "text_field", None),
            (UIA_DOCUMENT, "document", "", "text_area", None),
            (UIA_HYPERLINK, "link", "", "link", None),
            (UIA_TAB_ITEM, "tab", "", "tab", None),
            (
                UIA_WINDOW,
                "window",
                "Chrome_WidgetWin_1",
                "window",
                Some(RootKind::Window),
            ),
            (
                UIA_WINDOW,
                "dialog",
                "#32770",
                "window",
                Some(RootKind::Dialog),
            ),
            (UIA_WINDOW, "SHEET", "", "window", Some(RootKind::Sheet)),
            (UIA_MENU, "menu", "#32768", "menu", Some(RootKind::Menu)),
            (
                UIA_PANE,
                "flyout",
                "Popup",
                "group",
                Some(RootKind::Popover),
            ),
            (-1, "", "", "unknown", None),
        ] {
            assert_eq!(role_for_control_type(control), role);
            assert_eq!(root_kind_for_control_type(control, localized, class), kind);
        }
    }

    #[test]
    fn uia_rect_edges_convert_to_nonnegative_frames() {
        assert_eq!(
            frame_from_uia_rect(-100, 20, 300, 220),
            Frame {
                x: -100.0,
                y: 20.0,
                w: 400.0,
                h: 200.0,
            }
        );
        assert_eq!(
            frame_from_uia_rect(10, 20, 5, 15),
            Frame {
                x: 10.0,
                y: 20.0,
                w: 0.0,
                h: 0.0,
            }
        );
    }

    #[test]
    fn supported_patterns_produce_shared_action_names() {
        for (support, expected) in [
            (PatternSupport::default(), vec![]),
            (
                PatternSupport {
                    invoke: true,
                    ..Default::default()
                },
                vec!["press"],
            ),
            (
                PatternSupport {
                    value_writable: true,
                    ..Default::default()
                },
                vec!["set_text"],
            ),
            (
                PatternSupport {
                    selection_item: true,
                    scroll_item: true,
                    ..Default::default()
                },
                vec!["press", "scroll_to_visible", "select"],
            ),
        ] {
            assert_eq!(actions_for_patterns(support), expected);
        }
        let actions = actions_for_patterns(PatternSupport {
            invoke: true,
            value_writable: true,
            range_value_writable: true,
            toggle: true,
            expand_collapse: true,
            scroll: true,
            ..PatternSupport::default()
        });
        assert_eq!(
            actions,
            [
                "collapse",
                "expand",
                "press",
                "scroll",
                "set_text",
                "set_value",
                "toggle"
            ]
        );
    }

    #[test]
    fn virtual_key_chords_map_to_uiautomation_keyboard_expressions() {
        assert!(
            key_expression_for_chord(KeyChord {
                keycode: 0x53,
                modifiers: KeyModifiers {
                    function: true,
                    ..Default::default()
                }
            })
            .is_err()
        );
        assert!(
            key_expression_for_chord(KeyChord {
                keycode: 0xffff,
                modifiers: KeyModifiers::default()
            })
            .is_err()
        );
        assert_eq!(
            key_expression_for_chord(KeyChord {
                keycode: 0x53,
                modifiers: KeyModifiers {
                    control: true,
                    shift: true,
                    ..KeyModifiers::default()
                },
            }),
            Ok("{ctrl}{shift}(s)".into())
        );
        assert_eq!(
            key_expression_for_chord(KeyChord {
                keycode: 0x7B,
                modifiers: KeyModifiers::default(),
            }),
            Ok("{f12}".into())
        );
        assert_eq!(
            key_expression_for_chord(KeyChord {
                keycode: 0x2E,
                modifiers: KeyModifiers::default(),
            }),
            Ok("{delete}".into())
        );
        assert_eq!(
            key_expression_for_chord(KeyChord {
                keycode: 0xBA,
                modifiers: KeyModifiers {
                    option: true,
                    ..KeyModifiers::default()
                },
            }),
            Ok("{alt}(;)".into())
        );
    }
}
