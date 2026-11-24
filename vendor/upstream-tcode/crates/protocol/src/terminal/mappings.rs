//! Classic xterm input encoders, kept independent of the UI framework.
//!
//! These are pure functions of the replicated mode bits, so a client encodes
//! input without linking a terminal emulator.

use super::{KeyboardModes, TerminalMode as Mode};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub platform: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridPoint {
    pub row: usize,
    pub column: usize,
}

pub fn routes_mouse(mode: Mode, shift: bool) -> bool {
    mode.intersects(Mode::MOUSE_MODE) && !shift
}

fn modifier_bits(modifiers: Modifiers) -> u8 {
    u8::from(modifiers.shift) * 4 + u8::from(modifiers.alt) * 8 + u8::from(modifiers.control) * 16
}

fn button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

pub fn mouse_button_report(
    point: GridPoint,
    button: MouseButton,
    modifiers: Modifiers,
    pressed: bool,
    mode: Mode,
) -> Option<Vec<u8>> {
    mode.intersects(Mode::MOUSE_MODE)
        .then(|| mouse_report(point, button_code(button), modifiers, pressed, mode))?
}

pub fn mouse_move_report(
    point: GridPoint,
    button: Option<MouseButton>,
    modifiers: Modifiers,
    mode: Mode,
) -> Option<Vec<u8>> {
    if !(mode.contains(Mode::MOUSE_MOTION) || mode.contains(Mode::MOUSE_DRAG) && button.is_some()) {
        return None;
    }
    let code = button.map_or(35, |button| button_code(button) + 32);
    mouse_report(point, code, modifiers, true, mode)
}

pub fn scroll_report(
    point: GridPoint,
    lines: i32,
    modifiers: Modifiers,
    mode: Mode,
) -> Option<Vec<u8>> {
    if !mode.intersects(Mode::MOUSE_MODE) || lines == 0 {
        return None;
    }
    let code = if lines > 0 { 64 } else { 65 };
    let report = mouse_report(point, code, modifiers, true, mode)?;
    Some(report.repeat(lines.unsigned_abs() as usize))
}

pub fn alt_scroll(lines: i32) -> Vec<u8> {
    let suffix = if lines > 0 { b'A' } else { b'B' };
    let mut bytes = Vec::with_capacity(lines.unsigned_abs() as usize * 3);
    for _ in 0..lines.unsigned_abs() {
        bytes.extend_from_slice(&[0x1b, b'O', suffix]);
    }
    bytes
}

fn mouse_report(
    point: GridPoint,
    button: u8,
    modifiers: Modifiers,
    pressed: bool,
    mode: Mode,
) -> Option<Vec<u8>> {
    let button = button + modifier_bits(modifiers);
    if mode.contains(Mode::SGR_MOUSE) {
        let suffix = if pressed { 'M' } else { 'm' };
        Some(
            format!(
                "\x1b[<{button};{};{}{suffix}",
                point.column + 1,
                point.row + 1
            )
            .into_bytes(),
        )
    } else {
        normal_mouse_report(
            point,
            if pressed {
                button
            } else {
                3 + modifier_bits(modifiers)
            },
            mode.contains(Mode::UTF8_MOUSE),
        )
    }
}

fn normal_mouse_report(point: GridPoint, button: u8, utf8: bool) -> Option<Vec<u8>> {
    let max_point = if utf8 { 2015 } else { 223 };
    if point.row >= max_point || point.column >= max_point {
        return None;
    }
    let mut bytes = vec![0x1b, b'[', b'M', 32 + button];
    let encode = |position: usize| {
        let position = 33 + position;
        vec![(0xc0 + position / 64) as u8, (0x80 + (position & 63)) as u8]
    };
    if utf8 && point.column >= 95 {
        bytes.extend(encode(point.column));
    } else {
        bytes.push(33 + point.column as u8);
    }
    if utf8 && point.row >= 95 {
        bytes.extend(encode(point.row));
    } else {
        bytes.push(33 + point.row as u8);
    }
    Some(bytes)
}

pub fn key_bytes(
    key: &str,
    modifiers: Modifiers,
    mode: Mode,
    keyboard_mode: KeyboardModes,
    modify_other_keys: Option<u8>,
    option_as_meta: bool,
) -> Option<Vec<u8>> {
    if let Some(bytes) = kitty_key_bytes(key, modifiers, keyboard_mode) {
        return Some(bytes);
    }
    if keyboard_mode == KeyboardModes::NO_MODE
        && let Some(bytes) = modify_other_keys_bytes(key, modifiers, modify_other_keys)
    {
        return Some(bytes);
    }
    legacy_key_bytes(key, modifiers, mode, option_as_meta)
}

fn kitty_key_bytes(key: &str, modifiers: Modifiers, mode: KeyboardModes) -> Option<Vec<u8>> {
    let kitty_sequence = mode.intersects(
        KeyboardModes::DISAMBIGUATE_ESC_CODES
            | KeyboardModes::REPORT_EVENT_TYPES
            | KeyboardModes::REPORT_ALL_KEYS_AS_ESC,
    );
    if !kitty_sequence {
        return None;
    }

    let associated_text = mode
        .contains(KeyboardModes::REPORT_ASSOCIATED_TEXT)
        .then(|| associated_text(key, modifiers))
        .flatten();
    let modifier = 1
        + u8::from(modifiers.shift)
        + 2 * u8::from(modifiers.alt)
        + 4 * u8::from(modifiers.control)
        + 8 * u8::from(modifiers.platform);
    let has_modifiers = modifier != 1;

    let (payload, terminator) = kitty_functional_key(key)
        .or_else(|| kitty_legacy_functional_key(key, has_modifiers || associated_text.is_some()))
        .or_else(|| kitty_control_key(key))
        .or_else(|| kitty_all_key(key, mode))
        .or_else(|| kitty_text_key(key, modifiers, mode))?;

    let mut sequence = format!("\x1b[{payload}");
    if has_modifiers || associated_text.is_some() {
        sequence.push_str(&format!(";{modifier}"));
    }
    // tcode currently receives presses only. Kitty's `:1` press suffix is
    // optional, so REPORT_EVENT_TYPES does not change a press encoding.
    if let Some(text) = associated_text {
        sequence.push(';');
        let mut codepoints = text.chars().map(u32::from);
        sequence.push_str(&codepoints.next()?.to_string());
        for codepoint in codepoints {
            sequence.push(':');
            sequence.push_str(&codepoint.to_string());
        }
    }
    sequence.push(terminator);
    Some(sequence.into_bytes())
}

fn kitty_functional_key(key: &str) -> Option<(String, char)> {
    if let Some(number) = key
        .strip_prefix('f')
        .and_then(|number| number.parse::<u32>().ok())
        && (13..=35).contains(&number)
    {
        return Some(((57_363 + number).to_string(), 'u'));
    }
    let code = match key {
        // Kitty deliberately differs from traditional terminfo for F3.
        "f3" => return Some(("13".to_string(), '~')),
        "scrolllock" => 57_359,
        "printscreen" => 57_361,
        "pause" => 57_362,
        "contextmenu" => 57_363,
        "mediaplay" => 57_428,
        "mediapause" => 57_429,
        "mediaplaypause" => 57_430,
        "mediastop" => 57_432,
        "mediafastforward" => 57_433,
        "mediarewind" => 57_434,
        "medianext" => 57_435,
        "mediaprevious" => 57_436,
        "mediarecord" => 57_437,
        "volumedown" => 57_438,
        "volumeup" => 57_439,
        "volumemute" => 57_440,
        _ => return None,
    };
    Some((code.to_string(), 'u'))
}

fn kitty_legacy_functional_key(key: &str, explicit_one: bool) -> Option<(String, char)> {
    let one = if explicit_one { "1" } else { "" };
    let (payload, terminator) = match key {
        "pageup" => ("5", '~'),
        "pagedown" => ("6", '~'),
        "insert" => ("2", '~'),
        "delete" => ("3", '~'),
        "home" => (one, 'H'),
        "end" => (one, 'F'),
        "left" => (one, 'D'),
        "right" => (one, 'C'),
        "up" => (one, 'A'),
        "down" => (one, 'B'),
        "f1" => (one, 'P'),
        "f2" => (one, 'Q'),
        "f4" => (one, 'S'),
        "f5" => ("15", '~'),
        "f6" => ("17", '~'),
        "f7" => ("18", '~'),
        "f8" => ("19", '~'),
        "f9" => ("20", '~'),
        "f10" => ("21", '~'),
        "f11" => ("23", '~'),
        "f12" => ("24", '~'),
        _ => return None,
    };
    Some((payload.to_string(), terminator))
}

fn kitty_control_key(key: &str) -> Option<(String, char)> {
    let code = match key {
        "tab" => 9,
        "enter" => 13,
        "escape" => 27,
        "space" => 32,
        "backspace" | "back" => 127,
        _ => return None,
    };
    Some((code.to_string(), 'u'))
}

fn kitty_all_key(key: &str, mode: KeyboardModes) -> Option<(String, char)> {
    if !mode.contains(KeyboardModes::REPORT_ALL_KEYS_AS_ESC) {
        return None;
    }
    // GPUI does not expose left/right key location here, so generic modifier
    // names use kitty's left-side codes.
    let code = match key {
        "capslock" => 57_358,
        "numlock" => 57_360,
        "shift" => 57_441,
        "control" | "ctrl" => 57_442,
        "alt" => 57_443,
        "super" | "command" => 57_444,
        "hyper" => 57_445,
        "meta" => 57_446,
        _ => return None,
    };
    Some((code.to_string(), 'u'))
}

fn kitty_text_key(key: &str, modifiers: Modifiers, mode: KeyboardModes) -> Option<(String, char)> {
    let ch = single_printable_char(key)?;
    let (base, shifted) = shifted_key(ch, modifiers.shift);
    let payload = if mode.contains(KeyboardModes::REPORT_ALTERNATE_KEYS) && base != shifted {
        format!("{}:{}", u32::from(base), u32::from(shifted))
    } else {
        u32::from(base).to_string()
    };
    Some((payload, 'u'))
}

fn associated_text(key: &str, modifiers: Modifiers) -> Option<String> {
    if key == "space" {
        return Some(" ".to_string());
    }
    let ch = single_printable_char(key)?;
    Some(shifted_key(ch, modifiers.shift).1.to_string())
}

fn single_printable_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let ch = chars.next()?;
    (chars.next().is_none() && !ch.is_control()).then_some(ch)
}

fn shifted_key(ch: char, shift: bool) -> (char, char) {
    if !shift {
        return (ch, ch);
    }
    if ch.is_ascii_alphabetic() {
        return (ch.to_ascii_lowercase(), ch.to_ascii_uppercase());
    }
    const PAIRS: &[(char, char)] = &[
        ('`', '~'),
        ('1', '!'),
        ('2', '@'),
        ('3', '#'),
        ('4', '$'),
        ('5', '%'),
        ('6', '^'),
        ('7', '&'),
        ('8', '*'),
        ('9', '('),
        ('0', ')'),
        ('-', '_'),
        ('=', '+'),
        ('[', '{'),
        (']', '}'),
        ('\\', '|'),
        (';', ':'),
        ('\'', '"'),
        (',', '<'),
        ('.', '>'),
        ('/', '?'),
    ];
    PAIRS
        .iter()
        .find_map(|&(base, shifted)| (ch == base || ch == shifted).then_some((base, shifted)))
        .unwrap_or((ch, ch))
}

fn modify_other_keys_bytes(key: &str, modifiers: Modifiers, level: Option<u8>) -> Option<Vec<u8>> {
    // Level 1's compatibility exception table is intentionally left to the
    // legacy encoder. Level 2 has unambiguous semantics and covers the input
    // values GPUI exposes without physical-key/location information.
    if level != Some(2)
        || !(modifiers.shift || modifiers.alt || modifiers.control || modifiers.platform)
    {
        return None;
    }
    let codepoint = match key {
        "enter" => 13,
        "tab" => 9,
        "backspace" | "back" => 127,
        "escape" => 27,
        "space" => 32,
        _ => u32::from(shifted_key(single_printable_char(key)?, modifiers.shift).0),
    };
    let modifier = 1
        + u8::from(modifiers.shift)
        + 2 * u8::from(modifiers.alt)
        + 4 * u8::from(modifiers.control)
        + 8 * u8::from(modifiers.platform);
    Some(format!("\x1b[27;{modifier};{codepoint}~").into_bytes())
}

fn legacy_key_bytes(
    key: &str,
    modifiers: Modifiers,
    mode: Mode,
    option_as_meta: bool,
) -> Option<Vec<u8>> {
    let none = !modifiers.shift && !modifiers.alt && !modifiers.control && !modifiers.platform;
    let only = |shift, alt, control| {
        modifiers.shift == shift
            && modifiers.alt == alt
            && modifiers.control == control
            && !modifiers.platform
    };
    let fixed = match key {
        "tab" if none => Some("\t"),
        "escape" if none => Some("\x1b"),
        "enter" if none => Some("\r"),
        "enter" if only(true, false, false) => Some("\n"),
        "enter" if only(false, true, false) => Some("\x1b\r"),
        "backspace" if none || only(true, false, false) => Some("\x7f"),
        "tab" if only(true, false, false) => Some("\x1b[Z"),
        "backspace" if only(false, false, true) => Some("\x08"),
        "backspace" if only(false, true, false) => Some("\x1b\x7f"),
        "space" | "@" if only(false, false, true) => Some("\0"),
        "home" if none => Some(if mode.contains(Mode::APP_CURSOR) {
            "\x1bOH"
        } else {
            "\x1b[H"
        }),
        "end" if none => Some(if mode.contains(Mode::APP_CURSOR) {
            "\x1bOF"
        } else {
            "\x1b[F"
        }),
        "up" if none => Some(if mode.contains(Mode::APP_CURSOR) {
            "\x1bOA"
        } else {
            "\x1b[A"
        }),
        "down" if none => Some(if mode.contains(Mode::APP_CURSOR) {
            "\x1bOB"
        } else {
            "\x1b[B"
        }),
        "right" if none => Some(if mode.contains(Mode::APP_CURSOR) {
            "\x1bOC"
        } else {
            "\x1b[C"
        }),
        "left" if none => Some(if mode.contains(Mode::APP_CURSOR) {
            "\x1bOD"
        } else {
            "\x1b[D"
        }),
        "back" if none => Some("\x7f"),
        "insert" if none => Some("\x1b[2~"),
        "delete" if none => Some("\x1b[3~"),
        "pageup" if none => Some("\x1b[5~"),
        "pagedown" if none => Some("\x1b[6~"),
        "f1" if none => Some("\x1bOP"),
        "f2" if none => Some("\x1bOQ"),
        "f3" if none => Some("\x1bOR"),
        "f4" if none => Some("\x1bOS"),
        "f5" if none => Some("\x1b[15~"),
        "f6" if none => Some("\x1b[17~"),
        "f7" if none => Some("\x1b[18~"),
        "f8" if none => Some("\x1b[19~"),
        "f9" if none => Some("\x1b[20~"),
        "f10" if none => Some("\x1b[21~"),
        "f11" if none => Some("\x1b[23~"),
        "f12" if none => Some("\x1b[24~"),
        "f13" if none => Some("\x1b[25~"),
        "f14" if none => Some("\x1b[26~"),
        "f15" if none => Some("\x1b[28~"),
        "f16" if none => Some("\x1b[29~"),
        "f17" if none => Some("\x1b[31~"),
        "f18" if none => Some("\x1b[32~"),
        "f19" if none => Some("\x1b[33~"),
        "f20" if none => Some("\x1b[34~"),
        _ => None,
    };
    if let Some(fixed) = fixed {
        return Some(fixed.as_bytes().to_vec());
    }
    if (only(false, false, true) || only(true, false, true)) && key.chars().count() == 1 {
        let ch = key.chars().next()?;
        let control = match ch {
            'a'..='z' | 'A'..='Z' => (ch.to_ascii_uppercase() as u8) - b'@',
            '[' => 27,
            '\\' => 28,
            ']' => 29,
            '^' => 30,
            '_' => 31,
            '?' => 127,
            _ => return None,
        };
        return Some(vec![control]);
    }
    if modifiers.shift || modifiers.alt || modifiers.control || modifiers.platform {
        let code = 1
            + u8::from(modifiers.shift)
            + 2 * u8::from(modifiers.alt)
            + 4 * u8::from(modifiers.control);
        let sequence = match key {
            "up" => format!("\x1b[1;{code}A"),
            "down" => format!("\x1b[1;{code}B"),
            "right" => format!("\x1b[1;{code}C"),
            "left" => format!("\x1b[1;{code}D"),
            "home" => format!("\x1b[1;{code}H"),
            "end" => format!("\x1b[1;{code}F"),
            "f1" => format!("\x1b[1;{code}P"),
            "f2" => format!("\x1b[1;{code}Q"),
            "f3" => format!("\x1b[1;{code}R"),
            "f4" => format!("\x1b[1;{code}S"),
            "insert" => format!("\x1b[2;{code}~"),
            "delete" => format!("\x1b[3;{code}~"),
            "pageup" => format!("\x1b[5;{code}~"),
            "pagedown" => format!("\x1b[6;{code}~"),
            key @ ("f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" | "f13" | "f14"
            | "f15" | "f16" | "f17" | "f18" | "f19" | "f20") => {
                let prefix = [
                    15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34,
                ][key[1..].parse::<usize>().ok()? - 5];
                format!("\x1b[{prefix};{code}~")
            }
            _ => {
                let alt_meta = modifiers.alt
                    && !modifiers.control
                    && !modifiers.platform
                    && (cfg!(not(target_os = "macos")) || option_as_meta);
                if alt_meta && key.is_ascii() {
                    return Some(
                        format!(
                            "\x1b{}",
                            if modifiers.shift {
                                key.to_ascii_uppercase()
                            } else {
                                key.to_string()
                            }
                        )
                        .into_bytes(),
                    );
                }
                return None;
            }
        };
        return Some(sequence.into_bytes());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_reports_follow_negotiated_modes_coordinates_and_drag_state() {
        let plain = Modifiers::default();
        let modified = Modifiers {
            shift: true,
            alt: true,
            control: true,
            ..plain
        };
        let click = Mode::MOUSE_REPORT_CLICK;
        let sgr = click | Mode::SGR_MOUSE;
        let origin = GridPoint { row: 0, column: 0 };
        for (mode, point, modifiers, pressed, expected) in [
            (Mode::empty(), origin, plain, true, None),
            (click, origin, plain, true, Some(b"\x1b[M !!".as_slice())),
            (click, origin, plain, false, Some(b"\x1b[M#!!".as_slice())),
            (
                sgr,
                GridPoint { row: 2, column: 4 },
                plain,
                true,
                Some(b"\x1b[<0;5;3M".as_slice()),
            ),
            (
                sgr,
                GridPoint { row: 2, column: 4 },
                plain,
                false,
                Some(b"\x1b[<0;5;3m".as_slice()),
            ),
            (
                sgr,
                origin,
                modified,
                true,
                Some(b"\x1b[<28;1;1M".as_slice()),
            ),
            (
                click | Mode::UTF8_MOUSE,
                GridPoint {
                    row: 95,
                    column: 95,
                },
                plain,
                true,
                Some(b"\x1b[M \xc2\x80\xc2\x80".as_slice()),
            ),
            (
                click,
                GridPoint {
                    row: 222,
                    column: 222,
                },
                plain,
                true,
                Some(b"\x1b[M \xff\xff".as_slice()),
            ),
            (
                click,
                GridPoint {
                    row: 223,
                    column: 0,
                },
                plain,
                true,
                None,
            ),
            (
                click | Mode::UTF8_MOUSE,
                GridPoint {
                    row: 0,
                    column: 2015,
                },
                plain,
                true,
                None,
            ),
        ] {
            assert_eq!(
                mouse_button_report(point, MouseButton::Left, modifiers, pressed, mode),
                expected.map(<[u8]>::to_vec),
                "{mode:?} {point:?} {pressed}"
            );
        }
        for (mode, button, expected) in [
            (click, Some(MouseButton::Left), None),
            (Mode::MOUSE_DRAG | Mode::SGR_MOUSE, None, None),
            (
                Mode::MOUSE_DRAG | Mode::SGR_MOUSE,
                Some(MouseButton::Right),
                Some(b"\x1b[<62;1;1M".as_slice()),
            ),
            (
                Mode::MOUSE_MOTION | Mode::SGR_MOUSE,
                None,
                Some(b"\x1b[<63;1;1M".as_slice()),
            ),
        ] {
            assert_eq!(
                mouse_move_report(origin, button, modified, mode),
                expected.map(<[u8]>::to_vec)
            );
        }
        assert!(routes_mouse(sgr, false));
        assert!(!routes_mouse(sgr, true));
        assert!(!routes_mouse(Mode::empty(), false));
        assert_eq!(
            scroll_report(origin, -2, plain, sgr),
            Some(b"\x1b[<65;1;1M\x1b[<65;1;1M".to_vec())
        );
        assert_eq!(
            scroll_report(origin, 1, plain, sgr),
            Some(b"\x1b[<64;1;1M".to_vec())
        );
        assert_eq!(scroll_report(origin, 0, plain, sgr), None);
        assert_eq!(scroll_report(origin, 1, plain, Mode::empty()), None);
        assert_eq!(alt_scroll(2), b"\x1bOA\x1bOA");
        assert_eq!(alt_scroll(-1), b"\x1bOB");
        assert!(alt_scroll(0).is_empty());
    }

    #[test]
    fn key_encoding_negotiates_legacy_kitty_and_modify_other_keys() {
        let plain = Modifiers::default();
        let ctrl = Modifiers {
            control: true,
            ..plain
        };
        let shift = Modifiers {
            shift: true,
            ..plain
        };
        let alt = Modifiers { alt: true, ..plain };
        let shift_ctrl = Modifiers {
            shift: true,
            control: true,
            ..plain
        };
        let legacy = KeyboardModes::NO_MODE;
        let kitty = KeyboardModes::DISAMBIGUATE_ESC_CODES;
        let text =
            kitty | KeyboardModes::REPORT_ALTERNATE_KEYS | KeyboardModes::REPORT_ASSOCIATED_TEXT;
        for (key, modifiers, mode, keyboard, other_keys, expected) in [
            ("a", plain, Mode::empty(), legacy, None, None),
            ("c", ctrl, Mode::empty(), legacy, None, Some("\x03")),
            ("?", ctrl, Mode::empty(), legacy, None, Some("\x7f")),
            ("up", plain, Mode::empty(), legacy, None, Some("\x1b[A")),
            ("up", plain, Mode::APP_CURSOR, legacy, None, Some("\x1bOA")),
            (
                "up",
                shift_ctrl,
                Mode::APP_CURSOR,
                legacy,
                None,
                Some("\x1b[1;6A"),
            ),
            ("f1", plain, Mode::empty(), legacy, None, Some("\x1bOP")),
            ("f20", plain, Mode::empty(), legacy, None, Some("\x1b[34~")),
            ("f5", alt, Mode::empty(), legacy, None, Some("\x1b[15;3~")),
            ("enter", shift, Mode::empty(), legacy, None, Some("\n")),
            ("tab", shift, Mode::empty(), legacy, None, Some("\x1b[Z")),
            (
                "escape",
                plain,
                Mode::empty(),
                kitty,
                None,
                Some("\x1b[27u"),
            ),
            ("enter", plain, Mode::empty(), kitty, None, Some("\x1b[13u")),
            ("tab", plain, Mode::empty(), kitty, None, Some("\x1b[9u")),
            (
                "a",
                shift_ctrl,
                Mode::empty(),
                kitty,
                None,
                Some("\x1b[97;6u"),
            ),
            ("up", alt, Mode::APP_CURSOR, kitty, None, Some("\x1b[1;3A")),
            (
                "a",
                shift,
                Mode::empty(),
                text,
                None,
                Some("\x1b[97:65;2;65u"),
            ),
            (
                "!",
                shift,
                Mode::empty(),
                text,
                None,
                Some("\x1b[49:33;2;33u"),
            ),
            (
                "中",
                plain,
                Mode::empty(),
                text,
                None,
                Some("\x1b[20013;1;20013u"),
            ),
            ("f13", plain, Mode::empty(), text, None, Some("\x1b[57376u")),
            ("f3", plain, Mode::empty(), kitty, None, Some("\x1b[13~")),
            (
                "a",
                ctrl,
                Mode::empty(),
                legacy,
                Some(2),
                Some("\x1b[27;5;97~"),
            ),
            (
                "enter",
                ctrl,
                Mode::empty(),
                legacy,
                Some(2),
                Some("\x1b[27;5;13~"),
            ),
            ("a", ctrl, Mode::empty(), legacy, Some(1), Some("\x01")),
            ("a", plain, Mode::empty(), legacy, Some(2), None),
            ("a", ctrl, Mode::empty(), kitty, Some(2), Some("\x1b[97;5u")),
            ("ab", plain, Mode::empty(), kitty, None, None),
        ] {
            assert_eq!(
                key_bytes(key, modifiers, mode, keyboard, other_keys, true),
                expected.map(|text| text.as_bytes().to_vec()),
                "{key} {modifiers:?} {keyboard:?} {other_keys:?}"
            );
        }
        assert_eq!(
            key_bytes("a", alt, Mode::empty(), legacy, None, true),
            Some(b"\x1ba".to_vec())
        );
        assert_eq!(
            key_bytes("a", alt, Mode::empty(), legacy, None, false),
            if cfg!(target_os = "macos") {
                None
            } else {
                Some(b"\x1ba".to_vec())
            }
        );
    }
}
