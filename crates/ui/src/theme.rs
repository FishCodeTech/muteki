//! Original `base.css` and `chat/tokens.css` tokens, kept at their source sizes.
use nsh_core::prelude::*;
#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: Signal<bool>,
    pub system: Signal<bool>,
}
impl Palette {
    fn is_dark(self) -> bool {
        if self.system.get() {
            nsh_core::element::dark()
        } else {
            self.dark.get()
        }
    }
    pub fn bg(self) -> Hsla {
        rgb(if self.is_dark() { 0x15181c } else { 0xf7f8fa }).into()
    }
    pub fn rail(self) -> Hsla {
        rgb(if self.is_dark() { 0x121519 } else { 0xf2f4f6 }).into()
    }
    pub fn sidebar(self) -> Hsla {
        rgb(if self.is_dark() { 0x1a1c1f } else { 0xf1f2f4 }).into()
    }
    pub fn panel(self) -> Hsla {
        rgb(if self.is_dark() { 0x1b1f24 } else { 0xffffff }).into()
    }
    pub fn ink(self) -> Hsla {
        rgb(if self.is_dark() { 0xf0f2f5 } else { 0x171b20 }).into()
    }
    pub fn muted(self) -> Hsla {
        rgb(if self.is_dark() { 0x9aa2ac } else { 0x68717d }).into()
    }
    pub fn line(self) -> Hsla {
        rgb(if self.is_dark() { 0x30343a } else { 0xe5e8ec }).into()
    }
    pub fn hover(self) -> Hsla {
        rgb(if self.is_dark() { 0x2c3035 } else { 0xe9ebee }).into()
    }
}
