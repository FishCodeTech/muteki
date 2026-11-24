//! List-marker helper adapted from gpui-component's Apache-2.0 text renderer.

const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const LOWER: &str = "abcdefghijklmnopqrstuvwxyz";
const BULLETS: [&str; 5] = ["•", "◦", "▪", "‣", "⁃"];

pub(super) fn list_item_prefix(ix: usize, ordered: bool, start: u32, depth: usize) -> String {
    if ordered {
        match depth {
            0 => format!("{}. ", start as usize + ix),
            1 => format!("{}. ", UPPER.chars().nth(ix % UPPER.len()).unwrap()),
            _ => format!("{}. ", LOWER.chars().nth(ix % LOWER.len()).unwrap()),
        }
    } else {
        format!("{} ", BULLETS[depth.min(BULLETS.len() - 1)])
    }
}
