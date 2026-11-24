//! Platform-neutral accessibility tree, progressive rendering, and diffing.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

pub const MAX_MODEL_BYTES: usize = 48 * 1024;
pub const MAX_MODEL_LINES: usize = 2_000;
pub const PREVIEW_BYTES: usize = 16 * 1024;
pub const PAGE_BYTES: usize = 16 * 1024;
pub const SEARCH_LIMIT: usize = 20;
pub const TEXT_SPARSE_DESCENDANT_THRESHOLD: usize = 3;
pub const TEXT_SPARSE_MIN_FRAME_AREA: f64 = 20_000.0;

const FOLDED_DEPTH: usize = 7;
const FOLDED_LINES: usize = 500;
const EXPANDED_LINES: usize = 1_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Frame {
    pub fn center(self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn has_area(self) -> bool {
        self.w > 0.0 && self.h > 0.0
    }

    pub fn intersects(self, other: Self) -> bool {
        self.has_area()
            && other.has_area()
            && self.x < other.x + other.w
            && self.x + self.w > other.x
            && self.y < other.y + other.h
            && self.y + self.h > other.y
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UiNode {
    pub ref_id: String,
    pub role: String,
    pub title: String,
    pub value: String,
    pub description: String,
    pub frame: Frame,
    pub actions: Vec<String>,
    pub enabled: bool,
    pub focused: bool,
    pub children: Vec<UiNode>,
}

impl UiNode {
    pub fn is_interactive(&self) -> bool {
        is_interactive_role(&self.role) || !self.actions.is_empty()
    }

    /// Total nodes in this subtree, including self. For diagnostics.
    pub fn node_count(&self) -> usize {
        1 + descendant_count(self)
    }

    pub fn text(&self) -> String {
        let mut parts = Vec::new();
        for value in [&self.value, &self.title, &self.description] {
            if !value.is_empty() && !parts.contains(&value) {
                parts.push(value);
            }
        }
        parts
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn find(&self, ref_id: &str) -> Option<&Self> {
        if self.ref_id == ref_id {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(ref_id))
    }
}

pub fn is_interactive_role(role: &str) -> bool {
    matches!(
        canonical_role(role).as_str(),
        "button"
            | "text_field"
            | "text_area"
            | "search_field"
            | "link"
            | "menu"
            | "menu_item"
            | "checkbox"
            | "radio_button"
            | "combo_box"
            | "pop_up_button"
            | "slider"
            | "incrementor"
            | "tab"
    )
}

pub fn canonical_role(role: &str) -> String {
    role.trim()
        .strip_prefix("AX")
        .unwrap_or(role.trim())
        .chars()
        .enumerate()
        .fold(String::new(), |mut result, (index, ch)| {
            if ch.is_ascii_uppercase() && index != 0 {
                result.push('_');
            }
            result.push(ch.to_ascii_lowercase());
            result
        })
}

pub fn interactive_count(root: &UiNode) -> usize {
    let mut count = usize::from(root.is_interactive());
    for child in &root.children {
        count += interactive_count(child);
    }
    count
}

/// A large accessibility tree is text-sparse when fewer than three of its
/// descendants expose a title, value, or description. The root window's own
/// title is deliberately excluded because it does not describe the contents.
pub fn is_text_sparse(root: &UiNode) -> bool {
    let area = root.frame.w * root.frame.h;
    root.frame.has_area()
        && area.is_finite()
        && area >= TEXT_SPARSE_MIN_FRAME_AREA
        && text_bearing_descendant_count(root) < TEXT_SPARSE_DESCENDANT_THRESHOLD
}

fn text_bearing_descendant_count(root: &UiNode) -> usize {
    root.children
        .iter()
        .map(|child| usize::from(node_has_text(child)) + text_bearing_descendant_count(child))
        .sum()
}

fn node_has_text(node: &UiNode) -> bool {
    [&node.title, &node.value, &node.description]
        .into_iter()
        .any(|text| !text.trim().is_empty())
}

pub fn assign_refs(root: &mut UiNode) {
    let mut next = 1_u64;
    walk_mut(root, &mut |node, _| {
        node.ref_id = format!("@e{next}");
        next += 1;
    });
}

/// Preserve refs when a successor has the same role/title at the same path,
/// then use unique role/title matches before allocating fresh refs.
pub fn assign_refs_from_previous(previous: &UiNode, successor: &mut UiNode) {
    let old = flatten(previous);
    let mut by_path = HashMap::new();
    let mut by_signature: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut next = 1_u64;
    for entry in &old {
        by_path.insert(
            entry.path.clone(),
            (entry.signature.clone(), entry.ref_id.clone()),
        );
        by_signature
            .entry(entry.signature.clone())
            .or_default()
            .push(entry.ref_id.clone());
        next = next.max(ref_number(&entry.ref_id).unwrap_or(0) + 1);
    }

    let mut used = HashSet::new();
    walk_mut(successor, &mut |node, path| {
        let signature = node_signature(node);
        let path_match = by_path
            .get(path)
            .filter(|(old_signature, _)| old_signature == &signature)
            .map(|(_, ref_id)| ref_id.clone());
        let unique_match = by_signature.get(&signature).and_then(|refs| {
            let mut available = refs.iter().filter(|ref_id| !used.contains(*ref_id));
            let first = available.next()?;
            available.next().is_none().then(|| first.clone())
        });
        let ref_id = path_match
            .filter(|ref_id| !used.contains(ref_id))
            .or(unique_match)
            .unwrap_or_else(|| {
                let allocated = format!("@e{next}");
                next += 1;
                allocated
            });
        used.insert(ref_id.clone());
        node.ref_id = ref_id;
    });
}

pub fn path_to_ref(root: &UiNode, ref_id: &str) -> Option<Vec<usize>> {
    fn find(node: &UiNode, wanted: &str, path: &mut Vec<usize>) -> bool {
        if node.ref_id == wanted {
            return true;
        }
        for (index, child) in node.children.iter().enumerate() {
            path.push(index);
            if find(child, wanted, path) {
                return true;
            }
            path.pop();
        }
        false
    }

    let mut path = Vec::new();
    find(root, ref_id, &mut path).then_some(path)
}

pub fn render_folded(root: &UiNode) -> String {
    let mut output = Vec::new();
    render_folded_node(root, 0, true, &mut output);
    output.truncate(FOLDED_LINES);
    if output.len() == FOLDED_LINES {
        output.push("… outline capped; use search_ui or expand_ui for the full cached tree".into());
    }
    output.join("\n")
}

fn render_folded_node(node: &UiNode, depth: usize, is_root: bool, output: &mut Vec<String>) {
    if output.len() >= FOLDED_LINES {
        return;
    }
    if !is_root && is_collapsible_container(node) && node.children.len() == 1 {
        render_folded_node(&node.children[0], depth, false, output);
        return;
    }

    output.push(render_line(node, depth));
    if depth < FOLDED_DEPTH {
        for child in &node.children {
            render_folded_node(child, depth + 1, false, output);
        }
        return;
    }

    let before = output.len();
    for child in &node.children {
        render_interactive_descendants(child, depth + 1, output);
    }
    if before == output.len() && !node.children.is_empty() && output.len() < FOLDED_LINES {
        output.push(format!(
            "{}… {} descendants folded",
            "  ".repeat(depth + 1),
            descendant_count(node)
        ));
    }
}

fn render_interactive_descendants(node: &UiNode, depth: usize, output: &mut Vec<String>) {
    if output.len() >= FOLDED_LINES {
        return;
    }
    if node.is_interactive() {
        output.push(render_line(node, depth));
    }
    for child in &node.children {
        render_interactive_descendants(child, depth, output);
    }
}

pub fn render_expanded(root: &UiNode, ref_id: &str, depth: usize) -> Result<String, String> {
    let path = path_to_ref(root, ref_id)
        .ok_or_else(|| format!("element ref {ref_id} is not owned by this state"))?;
    let mut lines = Vec::new();
    let mut node = root;
    lines.push(render_line(node, 0));
    for (level, &index) in path.iter().enumerate() {
        node = &node.children[index];
        lines.push(render_line(node, level + 1));
    }
    render_subtree_children(node, path.len() + 1, depth, &mut lines);
    lines.truncate(EXPANDED_LINES);
    if lines.len() == EXPANDED_LINES {
        lines.push("… expansion capped; narrow the depth or use search_ui".into());
    }
    Ok(lines.join("\n"))
}

fn render_subtree_children(node: &UiNode, indent: usize, depth: usize, lines: &mut Vec<String>) {
    if depth == 0 || lines.len() >= EXPANDED_LINES {
        return;
    }
    for child in &node.children {
        lines.push(render_line(child, indent));
        render_subtree_children(child, indent + 1, depth - 1, lines);
    }
}

pub fn render_line(node: &UiNode, depth: usize) -> String {
    let mut line = format!(
        "{}{} {}",
        "  ".repeat(depth),
        node.ref_id,
        canonical_role(&node.role)
    );
    if !node.title.is_empty() {
        line.push_str(&format!(" \"{}\"", display_string(&node.title, 240)));
    }
    if !node.value.is_empty() && node.value != node.title {
        line.push_str(&format!(" value=\"{}\"", display_string(&node.value, 320)));
    }
    if !node.actions.is_empty() {
        line.push_str(&format!(" [{}]", node.actions.join(",")));
    }
    if !node.enabled {
        line.push_str(" disabled");
    }
    if node.focused {
        line.push_str(" focused");
    }
    line
}

fn display_string(value: &str, max_chars: usize) -> String {
    let mut rendered = String::new();
    let mut chars = value.chars();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else { break };
        match ch {
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '\t' => rendered.push_str("\\t"),
            '\\' => rendered.push_str("\\\\"),
            '"' => rendered.push_str("\\\""),
            other if other.is_control() => rendered.push(' '),
            other => rendered.push(other),
        }
    }
    if chars.next().is_some() {
        rendered.push('…');
    }
    rendered
}

fn is_collapsible_container(node: &UiNode) -> bool {
    !node.is_interactive()
        && matches!(
            canonical_role(&node.role).as_str(),
            "group" | "unknown" | "layout_area" | "scroll_area" | "split_group"
        )
        && node.title.is_empty()
        && node.value.is_empty()
}

fn descendant_count(node: &UiNode) -> usize {
    node.children
        .iter()
        .map(|child| 1 + descendant_count(child))
        .sum()
}

#[derive(Debug, Clone)]
pub struct SearchResult<'a> {
    pub node: &'a UiNode,
    pub score: u16,
}

#[derive(Debug, Clone)]
pub struct SearchResults<'a> {
    pub matches: Vec<SearchResult<'a>>,
    pub total: usize,
}

pub fn search<'a>(root: &'a UiNode, text: Option<&str>, role: Option<&str>) -> SearchResults<'a> {
    let query = text.map(str::trim).filter(|value| !value.is_empty());
    let wanted_role = role.map(canonical_role);
    let mut ranked = Vec::new();
    walk(root, &mut |node, path| {
        if wanted_role
            .as_ref()
            .is_some_and(|wanted| canonical_role(&node.role) != *wanted)
        {
            return;
        }
        let score = match query {
            Some(query) => match_score(node, query),
            None => Some(1),
        };
        if let Some(score) = score {
            ranked.push((score, path.clone(), node));
        }
    });
    ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    let total = ranked.len();
    let matches = ranked
        .into_iter()
        .take(SEARCH_LIMIT)
        .map(|(score, _, node)| SearchResult { node, score })
        .collect();
    SearchResults { matches, total }
}

fn match_score(node: &UiNode, query: &str) -> Option<u16> {
    let query = query.to_lowercase();
    [
        (&node.title, 30_u16),
        (&node.value, 20),
        (&node.description, 10),
    ]
    .into_iter()
    .filter_map(|(candidate, field_bonus)| {
        let candidate = candidate.trim().to_lowercase();
        if candidate.is_empty() {
            return None;
        }
        let quality = if candidate == query {
            400
        } else if candidate.starts_with(&query) {
            300
        } else if candidate.contains(&query) {
            200
        } else if conservative_fuzzy(&candidate, &query) {
            100
        } else {
            return None;
        };
        Some(quality + field_bonus)
    })
    .max()
}

fn conservative_fuzzy(candidate: &str, query: &str) -> bool {
    if query.chars().count() < 4 {
        return false;
    }
    let candidate_word = candidate
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .min_by_key(|word| word.len().abs_diff(query.len()))
        .unwrap_or(candidate);
    let length = candidate_word.chars().count().max(query.chars().count());
    let allowed = if length >= 8 { 2 } else { 1 };
    adjacent_transposition(candidate_word, query)
        || edit_distance_with_limit(candidate_word, query, allowed).is_some()
}

fn adjacent_transposition(left: &str, right: &str) -> bool {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.len() != right.len() || left.len() < 2 {
        return false;
    }
    let differences: Vec<usize> = left
        .iter()
        .zip(&right)
        .enumerate()
        .filter_map(|(index, (left, right))| (left != right).then_some(index))
        .collect();
    differences.len() == 2
        && differences[1] == differences[0] + 1
        && left[differences[0]] == right[differences[1]]
        && left[differences[1]] == right[differences[0]]
}

fn edit_distance_with_limit(left: &str, right: &str, limit: usize) -> Option<usize> {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.len().abs_diff(right.len()) > limit {
        return None;
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, left_ch) in left.iter().enumerate() {
        let mut current = vec![i + 1; right.len() + 1];
        let mut row_min = current[0];
        for (j, right_ch) in right.iter().enumerate() {
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + usize::from(left_ch != right_ch));
            row_min = row_min.min(current[j + 1]);
        }
        if row_min > limit {
            return None;
        }
        previous = current;
    }
    (previous[right.len()] <= limit).then_some(previous[right.len()])
}

#[derive(Debug, Clone)]
pub struct TreeDiff {
    pub text: String,
    pub confidence: f32,
    pub use_full_view: bool,
}

pub fn diff_trees(previous: &UiNode, successor: &UiNode) -> TreeDiff {
    let old = flatten(previous);
    let new = flatten(successor);
    let root_changed = node_signature(previous) != node_signature(successor);
    let mut used_old = HashSet::new();
    let mut matched: Vec<(&FlatNode<'_>, &FlatNode<'_>)> = Vec::new();
    let mut added: Vec<&FlatNode<'_>> = Vec::new();

    for new_entry in &new {
        let path_match = old.iter().enumerate().find(|(index, old_entry)| {
            !used_old.contains(index)
                && old_entry.path == new_entry.path
                && old_entry.signature == new_entry.signature
        });
        let signature_matches: Vec<_> = old
            .iter()
            .enumerate()
            .filter(|(index, old_entry)| {
                !used_old.contains(index) && old_entry.signature == new_entry.signature
            })
            .collect();
        let candidate =
            path_match.or_else(|| (signature_matches.len() == 1).then(|| signature_matches[0]));
        if let Some((index, old_entry)) = candidate {
            used_old.insert(index);
            matched.push((old_entry, new_entry));
        } else {
            added.push(new_entry);
        }
    }
    let removed: Vec<&FlatNode<'_>> = old
        .iter()
        .enumerate()
        .filter(|(index, _)| !used_old.contains(index))
        .map(|(_, entry)| entry)
        .collect();
    let updated: Vec<(&FlatNode<'_>, &FlatNode<'_>)> = matched
        .iter()
        .copied()
        .filter(|(old_entry, new_entry)| node_changed(old_entry.node, new_entry.node))
        .collect();
    let confidence = if old.len().max(new.len()) == 0 {
        1.0
    } else {
        matched.len() as f32 / old.len().max(new.len()) as f32
    };
    let use_full_view = root_changed || confidence < 0.55;

    let mut lines = vec![format!(
        "diff: +{} ~{} -{} (match confidence {:.0}%)",
        added.len(),
        updated.len(),
        removed.len(),
        confidence * 100.0
    )];
    for entry in added.iter().take(80) {
        lines.push(format!("+ {}", render_line(entry.node, 0)));
    }
    for (old_entry, new_entry) in updated.iter().take(80) {
        lines.push(format!(
            "~ {} -> {}",
            old_entry.ref_id,
            render_line(new_entry.node, 0)
        ));
    }
    for entry in removed.iter().take(80) {
        lines.push(format!(
            "- {} {} \"{}\"",
            entry.ref_id,
            canonical_role(&entry.node.role),
            display_string(&entry.node.title, 160)
        ));
    }
    let shown = added.len().min(80) + updated.len().min(80) + removed.len().min(80);
    if shown < added.len() + updated.len() + removed.len() {
        lines.push("… diff entries capped; inspect the successor state for details".into());
    }
    TreeDiff {
        text: lines.join("\n"),
        confidence,
        use_full_view,
    }
}

fn node_changed(left: &UiNode, right: &UiNode) -> bool {
    left.value != right.value
        || left.description != right.description
        || left.frame != right.frame
        || left.actions != right.actions
        || left.enabled != right.enabled
        || left.focused != right.focused
}

struct FlatNode<'a> {
    node: &'a UiNode,
    ref_id: String,
    path: Vec<usize>,
    signature: (String, String),
}

fn flatten(root: &UiNode) -> Vec<FlatNode<'_>> {
    let mut entries = Vec::new();
    walk(root, &mut |node, path| {
        entries.push(FlatNode {
            node,
            ref_id: node.ref_id.clone(),
            path: path.clone(),
            signature: node_signature(node),
        });
    });
    entries
}

fn node_signature(node: &UiNode) -> (String, String) {
    (canonical_role(&node.role), node.title.trim().to_lowercase())
}

fn ref_number(ref_id: &str) -> Option<u64> {
    ref_id.strip_prefix("@e")?.parse().ok()
}

fn walk<'a>(root: &'a UiNode, callback: &mut impl FnMut(&'a UiNode, &Vec<usize>)) {
    fn recurse<'a>(
        node: &'a UiNode,
        path: &mut Vec<usize>,
        callback: &mut impl FnMut(&'a UiNode, &Vec<usize>),
    ) {
        callback(node, path);
        for (index, child) in node.children.iter().enumerate() {
            path.push(index);
            recurse(child, path, callback);
            path.pop();
        }
    }
    recurse(root, &mut Vec::new(), callback);
}

fn walk_mut(root: &mut UiNode, callback: &mut impl FnMut(&mut UiNode, &Vec<usize>)) {
    fn recurse(
        node: &mut UiNode,
        path: &mut Vec<usize>,
        callback: &mut impl FnMut(&mut UiNode, &Vec<usize>),
    ) {
        callback(node, path);
        for (index, child) in node.children.iter_mut().enumerate() {
            path.push(index);
            recurse(child, path, callback);
            path.pop();
        }
    }
    recurse(root, &mut Vec::new(), callback);
}

pub fn output_exceeds_limit(text: &str) -> bool {
    text.len() > MAX_MODEL_BYTES || text.lines().count() > MAX_MODEL_LINES
}

pub fn safe_prefix(text: &str, byte_limit: usize, line_limit: usize) -> (&str, usize) {
    let mut end = text.len().min(byte_limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if line_limit != usize::MAX {
        let mut line_count = 0;
        for (index, byte) in text[..end].bytes().enumerate() {
            if byte == b'\n' {
                line_count += 1;
                if line_count >= line_limit {
                    end = index + 1;
                    break;
                }
            }
        }
    }
    (&text[..end], end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, title: &str, children: Vec<UiNode>) -> UiNode {
        UiNode {
            role: role.into(),
            title: title.into(),
            enabled: true,
            children,
            ..UiNode::default()
        }
    }

    #[test]
    fn folding_collapses_single_child_groups_but_keeps_interactive_nodes() {
        let mut tree = node(
            "window",
            "Document",
            vec![node(
                "group",
                "",
                vec![node("group", "", vec![node("button", "Save", Vec::new())])],
            )],
        );
        tree.children[0].children[0].children[0].actions = vec!["press".into()];
        assign_refs(&mut tree);
        let rendered = render_folded(&tree);
        assert!(rendered.contains("@e4 button \"Save\" [press]"));
        assert!(!rendered.contains("@e2 group"));
        assert!(!rendered.contains("@e3 group"));
    }

    #[test]
    fn search_ranks_exact_prefix_substring_then_fuzzy() {
        let mut tree = node(
            "window",
            "Root",
            vec![
                node("button", "Save", Vec::new()),
                node("button", "Save As", Vec::new()),
                node("button", "Auto Save Settings", Vec::new()),
                node("button", "Svae", Vec::new()),
                node("link", "Save", Vec::new()),
            ],
        );
        assign_refs(&mut tree);
        let results = search(&tree, Some("save"), Some("button"));
        let titles: Vec<_> = results
            .matches
            .iter()
            .map(|result| result.node.title.as_str())
            .collect();
        assert_eq!(titles, ["Save", "Save As", "Auto Save Settings", "Svae"]);
        assert_eq!(results.total, 4);
    }

    #[test]
    fn successor_refs_and_diff_are_stable() {
        let mut old = node("window", "Doc", vec![node("button", "Save", Vec::new())]);
        assign_refs(&mut old);
        let mut new = old.clone();
        new.children[0].value = "done".into();
        new.children.push(node("checkbox", "Autosave", Vec::new()));
        assign_refs_from_previous(&old, &mut new);
        assert_eq!(new.children[0].ref_id, old.children[0].ref_id);
        let diff = diff_trees(&old, &new);
        assert!(diff.text.contains("+1 ~1 -0"));
        assert!(!diff.use_full_view);
        new.children.swap(0, 1);
        assign_refs_from_previous(&old, &mut new);
        assert_eq!(new.children[1].ref_id, old.children[0].ref_id);
        let reordered = new.clone();
        new.children.remove(0);
        assign_refs_from_previous(&reordered, &mut new);
        assert_eq!(new.children[0].ref_id, old.children[0].ref_id);
        assert!(diff_trees(&reordered, &new).text.contains("+0 ~0 -1"));
        assert!(diff_trees(&new, &new).text.contains("+0 ~0 -0"));
        new.title = "Different window".into();
        assert!(diff_trees(&old, &new).use_full_view);
    }

    #[test]
    fn screenshot_fallback_requires_a_large_text_sparse_root() {
        let mut tree = UiNode {
            frame: Frame {
                x: 20.0,
                y: 30.0,
                w: 800.0,
                h: 600.0,
            },
            ..node(
                "window",
                "Root window title",
                vec![
                    node("button", "Menu", Vec::new()),
                    node("static_text", "Score", Vec::new()),
                ],
            )
        };
        assert!(is_text_sparse(&tree));
        tree.children.push(node("static_text", "Ready", Vec::new()));
        assert!(!is_text_sparse(&tree));
        tree.children.clear();
        tree.frame.w = 100.0;
        tree.frame.h = 100.0;
        assert!(!is_text_sparse(&tree));
    }
}
