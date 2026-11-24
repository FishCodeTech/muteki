//! Composer inline-trigger detection and mention serialization.
//!
//! Detects `@file`, `$skill` and `/command` at a UTF-8 cursor offset, and
//! serializes selected paths as Markdown links.

use std::ops::Range;

/// The kind of inline trigger active at the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// `@` file/folder mention.
    Path,
    /// `/` provider/built-in command.
    SlashCommand,
    /// The special `/model` command (opens the model picker).
    SlashModel,
    /// `$` skill.
    Skill,
}

/// A detected trigger: its kind, the query typed after the trigger char, and the
/// byte range (trigger char … cursor) that a selection replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerTrigger {
    pub kind: TriggerKind,
    pub query: String,
    pub range: Range<usize>,
}

/// ASCII whitespace token boundary.
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\t' | b'\r')
}

/// Detect an active trigger at `cursor` (a UTF-8 byte offset into `text`).
///
/// `/` is recognized only at the start of the message: Claude Code expands a
/// command only there, and Tcode's own commands are whole-message too. A `/`
/// token with a second `/` in it is a path. `@` and `$` follow whitespace.
pub fn detect_composer_trigger(text: &str, cursor: usize) -> Option<ComposerTrigger> {
    let cursor = cursor.min(text.len());
    // Snap to a char boundary defensively (byte offsets from the input are on
    // boundaries, but clamping above could land mid-char in pathological input).
    let cursor = (0..=cursor).rev().find(|&i| text.is_char_boundary(i))?;

    let prefix = &text[..cursor];

    if let Some(rest) = prefix.strip_prefix('/') {
        // `^/([^\s/]*)$`: a slash command with no whitespace after the slash.
        if !rest.bytes().any(is_ws) {
            if rest.contains('/') {
                return None;
            }
            if rest.eq_ignore_ascii_case("model") {
                return Some(ComposerTrigger {
                    kind: TriggerKind::SlashModel,
                    query: String::new(),
                    range: 0..cursor,
                });
            }
            return Some(ComposerTrigger {
                kind: TriggerKind::SlashCommand,
                query: rest.to_string(),
                range: 0..cursor,
            });
        }
        // `^/model(?:\s+(.*))?$`: `/model <query>`.
        if let Some(after) = prefix.strip_prefix("/model")
            && after.starts_with(|c: char| c.is_whitespace())
        {
            return Some(ComposerTrigger {
                kind: TriggerKind::SlashModel,
                query: after.trim().to_string(),
                range: 0..cursor,
            });
        }
        // A `/word …` that is not a bare command and not `/model`: fall through.
    }

    let bytes = text.as_bytes();
    let mut token_start = cursor;
    while token_start > 0 && !is_ws(bytes[token_start - 1]) {
        token_start -= 1;
    }
    let token = &text[token_start..cursor];
    if let Some(query) = token.strip_prefix('$') {
        return Some(ComposerTrigger {
            kind: TriggerKind::Skill,
            query: query.to_string(),
            range: token_start..cursor,
        });
    }
    if let Some(query) = token.strip_prefix('@') {
        return Some(ComposerTrigger {
            kind: TriggerKind::Path,
            query: query.to_string(),
            range: token_start..cursor,
        });
    }
    None
}

/// The basename of a `/`- or `\`-separated path.
pub fn basename(path: &str) -> &str {
    let idx = path.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
    &path[idx..]
}

fn escape_markdown_link_label(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

/// Percent-encode like JS `encodeURI`: keep unreserved + reserved URI chars,
/// escape everything else per UTF-8 byte.
fn encode_uri(s: &str) -> String {
    const KEEP: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'();,/?:@&=+$#";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if KEEP.contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

fn encode_markdown_link_destination(path: &str) -> String {
    encode_uri(path)
        .replace('(', "%28")
        .replace(')', "%29")
        .replace('#', "%23")
        .replace('?', "%3F")
        .replace('\\', "%5C")
}

/// Serialize a selected path as `[basename](encoded-path)`.
pub fn serialize_composer_file_link(path: &str) -> String {
    let label = escape_markdown_link_label(basename(path));
    let dest = encode_markdown_link_destination(path);
    format!("[{label}]({dest})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_respect_token_boundaries_and_the_utf8_cursor() {
        for (text, cursor, kind, query, range) in [
            ("look at @src/ma", 15, TriggerKind::Path, "src/ma", 8..15),
            ("use $rev", 8, TriggerKind::Skill, "rev", 4..8),
            ("\t@文件 suffix", 5, TriggerKind::Path, "文", 1..5),
            ("@文", 2, TriggerKind::Path, "", 0..1),
            ("@file", usize::MAX, TriggerKind::Path, "file", 0..5),
            ("/de", 3, TriggerKind::SlashCommand, "de", 0..3),
            ("/a:b", 4, TriggerKind::SlashCommand, "a:b", 0..4),
            ("/", 1, TriggerKind::SlashCommand, "", 0..1),
            ("/model", 6, TriggerKind::SlashModel, "", 0..6),
            ("/MODEL", 6, TriggerKind::SlashModel, "", 0..6),
            ("/model gpt", 10, TriggerKind::SlashModel, "gpt", 0..10),
        ] {
            assert_eq!(
                detect_composer_trigger(text, cursor),
                Some(ComposerTrigger {
                    kind,
                    query: query.into(),
                    range
                }),
                "{text:?} at {cursor}",
            );
        }
        for text in [
            "",
            "foo@bar",
            "cost$rev",
            "hello /pla",
            " /pla",
            "hi\n/de",
            "/plan done",
            "/Users/me/file.txt",
            "/tmp/",
            "@file ",
        ] {
            assert_eq!(detect_composer_trigger(text, text.len()), None, "{text:?}");
        }
    }

    #[test]
    fn file_links_preserve_paths_without_creating_markdown_or_url_syntax() {
        for (path, expected) in [
            ("src/main.rs", "[main.rs](src/main.rs)"),
            ("my file.txt", "[my file.txt](my%20file.txt)"),
            ("a(b).js", "[a(b).js](a%28b%29.js)"),
            ("weird#name?.md", "[weird#name?.md](weird%23name%3F.md)"),
            (
                r"C:\src\[稿].md",
                r"[\[稿\].md](C:%5Csrc%5C%5B%E7%A8%BF%5D.md)",
            ),
            ("100%.txt", "[100%.txt](100%25.txt)"),
        ] {
            assert_eq!(serialize_composer_file_link(path), expected, "{path}");
        }
    }
}
