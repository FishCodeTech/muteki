#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffRow {
    pub kind: RowKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub gap_before: u32,
    pub rows: Vec<DiffRow>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedDiff {
    pub hunks: Vec<Hunk>,
    pub added: u32,
    pub removed: u32,
}

pub(crate) fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@")?;
    let body = rest.split("@@").next().unwrap_or("").trim();
    let mut old_start = None;
    let mut new_start = None;
    for tok in body.split_whitespace() {
        if let Some(v) = tok.strip_prefix('-') {
            old_start = v.split(',').next().and_then(|n| n.parse::<u32>().ok());
        } else if let Some(v) = tok.strip_prefix('+') {
            new_start = v.split(',').next().and_then(|n| n.parse::<u32>().ok());
        }
    }
    Some((old_start?, new_start?))
}

pub(crate) fn is_binary_patch(patch: &str) -> bool {
    patch
        .lines()
        .any(|line| line.starts_with("Binary files ") || line == "GIT binary patch")
}

pub fn parse_unified_diff(diff: &str) -> ParsedDiff {
    if diff.lines().any(|line| parse_hunk_header(line).is_some()) {
        parse_standard(diff)
    } else if diff.lines().any(|line| line.starts_with("diff --git ")) {
        ParsedDiff::default()
    } else {
        parse_bare(diff)
    }
}

fn parse_standard(diff: &str) -> ParsedDiff {
    let mut out = ParsedDiff::default();
    let mut cur: Option<Hunk> = None;
    let mut seen_hunk = false;
    let mut old_cursor = 0u32;
    let mut new_cursor = 0u32;
    let mut prev_new_end = 1u32;

    for line in diff.lines() {
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            if let Some(hunk) = cur.take() {
                prev_new_end = new_cursor;
                out.hunks.push(hunk);
            }
            let gap = new_start.saturating_sub(prev_new_end);
            old_cursor = old_start;
            new_cursor = new_start;
            cur = Some(Hunk {
                header: line.to_string(),
                gap_before: gap,
                rows: Vec::new(),
            });
            seen_hunk = true;
            continue;
        }
        if !seen_hunk {
            continue;
        }
        let Some(hunk) = cur.as_mut() else { continue };
        let mut chars = line.chars();
        match chars.next() {
            Some('+') => {
                out.added += 1;
                hunk.rows.push(DiffRow {
                    kind: RowKind::Added,
                    old_line: None,
                    new_line: Some(new_cursor),
                    text: chars.as_str().to_string(),
                });
                new_cursor += 1;
            }
            Some('-') => {
                out.removed += 1;
                hunk.rows.push(DiffRow {
                    kind: RowKind::Removed,
                    old_line: Some(old_cursor),
                    new_line: None,
                    text: chars.as_str().to_string(),
                });
                old_cursor += 1;
            }
            Some('\\') => {}
            Some(' ') => {
                hunk.rows.push(DiffRow {
                    kind: RowKind::Context,
                    old_line: Some(old_cursor),
                    new_line: Some(new_cursor),
                    text: chars.as_str().to_string(),
                });
                old_cursor += 1;
                new_cursor += 1;
            }
            None => {
                hunk.rows.push(DiffRow {
                    kind: RowKind::Context,
                    old_line: Some(old_cursor),
                    new_line: Some(new_cursor),
                    text: String::new(),
                });
                old_cursor += 1;
                new_cursor += 1;
            }
            Some(_) => {
                hunk.rows.push(DiffRow {
                    kind: RowKind::Context,
                    old_line: Some(old_cursor),
                    new_line: Some(new_cursor),
                    text: line.to_string(),
                });
                old_cursor += 1;
                new_cursor += 1;
            }
        }
    }
    if let Some(hunk) = cur.take() {
        out.hunks.push(hunk);
    }
    out
}

fn parse_bare(diff: &str) -> ParsedDiff {
    let mut out = ParsedDiff::default();
    let mut rows = Vec::new();
    let mut old_cursor = 1u32;
    let mut new_cursor = 1u32;
    for line in diff.lines() {
        if line == r"\ No newline at end of file" {
            continue;
        }
        let mut chars = line.chars();
        match chars.next() {
            Some('+') => {
                out.added += 1;
                rows.push(DiffRow {
                    kind: RowKind::Added,
                    old_line: None,
                    new_line: Some(new_cursor),
                    text: chars.as_str().to_string(),
                });
                new_cursor += 1;
            }
            Some('-') => {
                out.removed += 1;
                rows.push(DiffRow {
                    kind: RowKind::Removed,
                    old_line: Some(old_cursor),
                    new_line: None,
                    text: chars.as_str().to_string(),
                });
                old_cursor += 1;
            }
            _ => {
                rows.push(DiffRow {
                    kind: RowKind::Context,
                    old_line: Some(old_cursor),
                    new_line: Some(new_cursor),
                    text: line.to_string(),
                });
                old_cursor += 1;
                new_cursor += 1;
            }
        }
    }
    if !rows.is_empty() {
        out.hunks.push(Hunk {
            header: String::new(),
            gap_before: 0,
            rows,
        });
    }
    out
}
