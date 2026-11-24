//! Keys: GPUI's keystrokes as the virtual key codes ([`crate::input::KeyEvent::vk`],
//! Windows' `VK_*` values) the input, scrolling, keymap and editing code
//! shared with the Windows backend work with.

/// `VK_BACK`.
pub(crate) const VK_BACK: u16 = 0x08;
/// `VK_TAB`.
pub(crate) const VK_TAB: u16 = 0x09;
/// `VK_RETURN`.
pub(crate) const VK_RETURN: u16 = 0x0D;
/// `VK_SHIFT`.
pub(crate) const VK_SHIFT: u16 = 0x10;
/// `VK_CONTROL`.
pub(crate) const VK_CONTROL: u16 = 0x11;
/// `VK_MENU` (Alt).
pub(crate) const VK_MENU: u16 = 0x12;
/// `VK_ESCAPE`.
pub(crate) const VK_ESCAPE: u16 = 0x1B;
/// `VK_SPACE`.
pub(crate) const VK_SPACE: u16 = 0x20;
/// `VK_PRIOR` (Page Up).
pub(crate) const VK_PRIOR: u16 = 0x21;
/// `VK_NEXT` (Page Down).
pub(crate) const VK_NEXT: u16 = 0x22;
/// `VK_END`.
pub(crate) const VK_END: u16 = 0x23;
/// `VK_HOME`.
pub(crate) const VK_HOME: u16 = 0x24;
/// `VK_LEFT`.
pub(crate) const VK_LEFT: u16 = 0x25;
/// `VK_UP`.
pub(crate) const VK_UP: u16 = 0x26;
/// `VK_RIGHT`.
pub(crate) const VK_RIGHT: u16 = 0x27;
/// `VK_DOWN`.
pub(crate) const VK_DOWN: u16 = 0x28;
/// `VK_INSERT`.
pub(crate) const VK_INSERT: u16 = 0x2D;
/// `VK_DELETE`.
pub(crate) const VK_DELETE: u16 = 0x2E;
/// `VK_LWIN` (the Windows / Command / Super key).
pub(crate) const VK_LWIN: u16 = 0x5B;
/// `VK_BROWSER_BACK`.
pub(crate) const VK_BROWSER_BACK: u16 = 0xA6;

/// The virtual key code of GPUI key name `key` (`Keystroke::key`), or
/// `None` for keys nsh does not use.
pub(crate) fn vk_of(key: &str) -> Option<u16> {
    Some(match key {
        "backspace" => VK_BACK,
        "tab" => VK_TAB,
        "enter" => VK_RETURN,
        "shift" => VK_SHIFT,
        "control" => VK_CONTROL,
        "alt" => VK_MENU,
        "platform" | "cmd" | "super" | "win" => VK_LWIN,
        "escape" => VK_ESCAPE,
        "space" => VK_SPACE,
        "pageup" => VK_PRIOR,
        "pagedown" => VK_NEXT,
        "end" => VK_END,
        "home" => VK_HOME,
        "left" => VK_LEFT,
        "up" => VK_UP,
        "right" => VK_RIGHT,
        "down" => VK_DOWN,
        "insert" => VK_INSERT,
        "delete" => VK_DELETE,
        "back" => VK_BROWSER_BACK,
        ";" => 0xBA,
        "=" => 0xBB,
        "," => 0xBC,
        "-" => 0xBD,
        "." => 0xBE,
        "/" => 0xBF,
        "`" => 0xC0,
        "[" => 0xDB,
        "\\" => 0xDC,
        "]" => 0xDD,
        "'" => 0xDE,
        _ => {
            if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<u16>().ok())
                && (1..=24).contains(&n)
            {
                return Some(0x6F + n);
            }
            let mut chars = key.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            match c {
                '0'..='9' => c as u16,
                'a'..='z' => c.to_ascii_uppercase() as u16,
                'A'..='Z' => c as u16,
                _ => return None,
            }
        }
    })
}

/// GPUI's modifiers as the input code's.
pub(crate) fn modifiers(m: &gpui::Modifiers) -> crate::input::Modifiers {
    crate::input::Modifiers {
        shift: m.shift,
        ctrl: m.control,
        alt: m.alt,
        meta: m.platform,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(vk_of("a"), Some(0x41));
        assert_eq!(vk_of("7"), Some(0x37));
        assert_eq!(vk_of("f5"), Some(0x74));
        assert_eq!(vk_of("enter"), Some(VK_RETURN));
        assert_eq!(vk_of("/"), Some(0xBF));
        assert_eq!(vk_of("é"), None);
    }
}
