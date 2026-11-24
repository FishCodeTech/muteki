//! Bounded immutable observations and independently bounded output pages.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use crate::backend::RootInfo;
use crate::outline::{
    MAX_MODEL_BYTES, MAX_MODEL_LINES, PAGE_BYTES, PREVIEW_BYTES, UiNode, assign_refs,
    assign_refs_from_previous, canonical_role, output_exceeds_limit, safe_prefix,
};

pub const OBSERVATION_CAPACITY: usize = 8;
pub const OUTPUT_CAPACITY: usize = 32;

const RECENT_ACTION_CAPACITY: usize = 8;
const CANDIDATE_TARGET_CAPACITY: usize = 64;
const DELTA_ENTRY_CAPACITY: usize = 16;
const STABLE_LABEL_CAPACITY: usize = 3_000;
const STABLE_LABEL_MAX_BYTES: usize = 512;
const DISPLAY_LABEL_MAX_CHARS: usize = 160;
const DISPLAY_LABEL_MAX_BYTES: usize = 200;
const ROLE_MAX_BYTES: usize = 48;
const ACTION_DESCRIPTION_MAX_BYTES: usize = 256;

#[derive(Debug)]
pub struct Observation {
    pub state_id: String,
    pub root: RootInfo,
    pub root_epoch: u64,
    pub tree: UiNode,
    pub harness_annotation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StableLabel {
    key: String,
    display: String,
}

#[derive(Debug, Default)]
struct HarnessHistory {
    observation_sequence: u64,
    initial_labels: Option<Vec<StableLabel>>,
    previous_labels: Option<Vec<StableLabel>>,
    recent_actions: VecDeque<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    Evicted(String),
    Stale {
        state_id: String,
        state_epoch: u64,
        current_epoch: u64,
    },
    UnknownElement(String),
    UnknownOutput(String),
    OutputOwnerMismatch {
        output_ref: String,
        expected: String,
        actual: Option<String>,
    },
    InvalidOffset(usize),
}

impl fmt::Display for StateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Evicted(state_id) => write!(
                formatter,
                "state {state_id} was evicted from the observation cache; call observe_ui again"
            ),
            Self::Stale {
                state_id,
                state_epoch,
                current_epoch,
            } => write!(
                formatter,
                "state {state_id} is stale (root epoch {state_epoch}, current {current_epoch}); call observe_ui again"
            ),
            Self::UnknownElement(ref_id) => write!(
                formatter,
                "element ref {ref_id} is not owned by this state; call observe_ui again if the UI changed"
            ),
            Self::UnknownOutput(output_ref) => write!(
                formatter,
                "output continuation {output_ref} was evicted or does not exist; rerun the originating tool"
            ),
            Self::OutputOwnerMismatch {
                output_ref,
                expected,
                actual,
            } => write!(
                formatter,
                "output continuation {output_ref} belongs to state {}, not {expected}",
                actual.as_deref().unwrap_or("<none>")
            ),
            Self::InvalidOffset(offset) => write!(
                formatter,
                "byte offset {offset} is outside the text or splits a UTF-8 character"
            ),
        }
    }
}

impl std::error::Error for StateError {}

#[derive(Debug, Clone)]
struct OutputEntry {
    owner_state: Option<String>,
    text: Arc<str>,
    initial_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPage {
    pub output_ref: String,
    pub owner_state: Option<String>,
    pub text: String,
    pub offset: usize,
    pub next_offset: usize,
    pub total_bytes: usize,
    pub eof: bool,
}

#[derive(Debug)]
pub struct StateStore {
    next_state: u64,
    next_output: u64,
    observations: HashMap<String, Arc<Observation>>,
    observation_lru: VecDeque<String>,
    root_epochs: HashMap<String, u64>,
    harness_histories: HashMap<String, HarnessHistory>,
    outputs: HashMap<String, OutputEntry>,
    output_lru: VecDeque<String>,
}

impl Default for StateStore {
    fn default() -> Self {
        Self {
            next_state: 1,
            next_output: 1,
            observations: HashMap::new(),
            observation_lru: VecDeque::new(),
            root_epochs: HashMap::new(),
            harness_histories: HashMap::new(),
            outputs: HashMap::new(),
            output_lru: VecDeque::new(),
        }
    }
}

impl StateStore {
    pub fn insert_observation(&mut self, root: RootInfo, mut tree: UiNode) -> Arc<Observation> {
        let identity = root.identity();
        let previous = self
            .observation_lru
            .iter()
            .rev()
            .filter_map(|state_id| self.observations.get(state_id))
            .find(|observation| observation.root.identity() == identity)
            .cloned();
        if let Some(previous) = previous {
            assign_refs_from_previous(&previous.tree, &mut tree);
        } else {
            assign_refs(&mut tree);
        }

        let harness_annotation = self.record_harness_observation(&root, &tree);

        let epoch = self.root_epochs.entry(identity.clone()).or_default();
        *epoch += 1;
        let state_id = format!("S{}", self.next_state);
        self.next_state += 1;
        let observation = Arc::new(Observation {
            state_id: state_id.clone(),
            root,
            root_epoch: *epoch,
            tree,
            harness_annotation,
        });
        self.observations
            .insert(state_id.clone(), Arc::clone(&observation));
        touch(&mut self.observation_lru, &state_id);
        while self.observations.len() > OBSERVATION_CAPACITY {
            if let Some(evicted) = self.observation_lru.pop_front()
                && let Some(observation) = self.observations.remove(&evicted)
            {
                self.drop_harness_history_if_root_evicted(&observation.root.identity());
            }
        }
        observation
    }

    pub fn record_actions(
        &mut self,
        root: &RootInfo,
        descriptions: impl IntoIterator<Item = String>,
    ) {
        let history = self.harness_histories.entry(root.identity()).or_default();
        for description in descriptions {
            history
                .recent_actions
                .push_back(truncate_plain(&description, ACTION_DESCRIPTION_MAX_BYTES));
            while history.recent_actions.len() > RECENT_ACTION_CAPACITY {
                history.recent_actions.pop_front();
            }
        }
    }

    fn record_harness_observation(&mut self, root: &RootInfo, tree: &UiNode) -> String {
        let current_labels = stable_labels(tree);
        let candidate_targets = candidate_target_lines(tree);
        let history = self.harness_histories.entry(root.identity()).or_default();
        history.observation_sequence = history.observation_sequence.saturating_add(1);
        let initial_labels = history
            .initial_labels
            .get_or_insert_with(|| current_labels.clone());
        let annotation = harness_annotation(
            root,
            history.observation_sequence,
            initial_labels,
            history.previous_labels.as_deref(),
            &history.recent_actions,
            &candidate_targets,
            &current_labels,
        );
        history.previous_labels = Some(current_labels);
        annotation
    }

    fn drop_harness_history_if_root_evicted(&mut self, identity: &str) {
        let root_remains = self
            .observations
            .values()
            .any(|observation| observation.root.identity() == identity);
        if !root_remains {
            self.harness_histories.remove(identity);
        }
    }

    pub fn get(&mut self, state_id: &str) -> Result<Arc<Observation>, StateError> {
        let observation = self
            .observations
            .get(state_id)
            .cloned()
            .ok_or_else(|| StateError::Evicted(state_id.to_string()))?;
        touch(&mut self.observation_lru, state_id);
        Ok(observation)
    }

    pub fn validate_for_action(&mut self, state_id: &str) -> Result<Arc<Observation>, StateError> {
        let observation = self.get(state_id)?;
        let current_epoch = self
            .root_epochs
            .get(&observation.root.identity())
            .copied()
            .unwrap_or_default();
        if current_epoch != observation.root_epoch {
            return Err(StateError::Stale {
                state_id: state_id.to_string(),
                state_epoch: observation.root_epoch,
                current_epoch,
            });
        }
        Ok(observation)
    }

    pub fn register_output(
        &mut self,
        owner_state: Option<&str>,
        text: impl Into<Arc<str>>,
        initial_offset: usize,
    ) -> String {
        let output_ref = format!("@o{}", self.next_output);
        self.next_output += 1;
        self.outputs.insert(
            output_ref.clone(),
            OutputEntry {
                owner_state: owner_state.map(str::to_owned),
                text: text.into(),
                initial_offset,
            },
        );
        touch(&mut self.output_lru, &output_ref);
        while self.outputs.len() > OUTPUT_CAPACITY {
            if let Some(evicted) = self.output_lru.pop_front() {
                self.outputs.remove(&evicted);
            }
        }
        output_ref
    }

    pub fn bound_model_text(&mut self, owner_state: Option<&str>, text: String) -> String {
        if !output_exceeds_limit(&text) {
            return text;
        }
        let (preview, offset) = safe_prefix(&text, PREVIEW_BYTES, MAX_MODEL_LINES);
        let preview = preview.to_string();
        let output_ref = self.register_output(owner_state, Arc::<str>::from(text), offset);
        format!(
            "{preview}\n\n[output truncated: use read_text with ref {output_ref} and offset {offset}]"
        )
    }

    pub fn read_output(
        &mut self,
        output_ref: &str,
        state_id: Option<&str>,
        offset: Option<usize>,
    ) -> Result<OutputPage, StateError> {
        let entry = self
            .outputs
            .get(output_ref)
            .cloned()
            .ok_or_else(|| StateError::UnknownOutput(output_ref.to_string()))?;
        if let Some(expected) = state_id
            && entry.owner_state.as_deref() != Some(expected)
        {
            return Err(StateError::OutputOwnerMismatch {
                output_ref: output_ref.to_string(),
                expected: expected.to_string(),
                actual: entry.owner_state,
            });
        }
        touch(&mut self.output_lru, output_ref);
        page(
            output_ref,
            entry.owner_state,
            &entry.text,
            offset.unwrap_or(entry.initial_offset),
        )
    }

    pub fn page_element_text(
        &mut self,
        state_id: &str,
        ref_id: &str,
        offset: usize,
    ) -> Result<OutputPage, StateError> {
        let observation = self.get(state_id)?;
        let node = observation
            .tree
            .find(ref_id)
            .ok_or_else(|| StateError::UnknownElement(ref_id.to_string()))?;
        let text: Arc<str> = Arc::from(node.text());
        if text.len().saturating_sub(offset) > PAGE_BYTES {
            let output_ref = self.register_output(Some(state_id), Arc::clone(&text), offset);
            self.read_output(&output_ref, Some(state_id), Some(offset))
        } else {
            page(ref_id, Some(state_id.to_string()), &text, offset)
        }
    }
}

pub(crate) fn harness_action_description(
    action_name: &str,
    target_ref: Option<&str>,
    tree: &UiNode,
) -> String {
    let mut description = action_name.to_string();
    if let Some(ref_id) = target_ref {
        description.push(' ');
        description.push_str(&truncate_plain(ref_id, 32));
        if let Some(node) = tree.find(ref_id)
            && let Some(label) = candidate_label(node)
        {
            description.push_str(" \"");
            description.push_str(&display_label(&label));
            description.push('"');
        }
    }
    truncate_plain(&description, ACTION_DESCRIPTION_MAX_BYTES)
}

fn harness_annotation(
    root: &RootInfo,
    observation_sequence: u64,
    initial_labels: &[StableLabel],
    previous_labels: Option<&[StableLabel]>,
    recent_actions: &VecDeque<String>,
    candidate_targets: &[String],
    current_labels: &[StableLabel],
) -> String {
    let mut lines = vec![
        "<computer_use_harness>".to_string(),
        format!("observation_sequence: {observation_sequence}"),
        format!("root: pid={} window_id={}", root.pid, root.window_id),
        "<recent_actions>".to_string(),
    ];
    if recent_actions.is_empty() {
        lines.push("none".to_string());
    } else {
        lines.extend(recent_actions.iter().map(|action| format!("- {action}")));
    }
    lines.push("</recent_actions>".to_string());
    lines.push("<candidate_targets>".to_string());
    if candidate_targets.is_empty() {
        lines.push("none".to_string());
    } else {
        lines.extend(candidate_targets.iter().cloned());
    }
    lines.push("</candidate_targets>".to_string());
    lines.push("<state_delta since=\"previous\">".to_string());
    lines.extend(harness_delta_lines(current_labels, previous_labels));
    lines.push("</state_delta>".to_string());
    lines.push("<state_delta since=\"initial\">".to_string());
    lines.extend(harness_delta_lines(current_labels, Some(initial_labels)));
    lines.push("</state_delta>".to_string());
    lines.push("</computer_use_harness>".to_string());
    let annotation = lines.join("\n");
    debug_assert!(annotation.len() <= MAX_MODEL_BYTES);
    annotation
}

fn stable_labels(root: &UiNode) -> Vec<StableLabel> {
    fn visit(node: &UiNode, seen: &mut HashSet<String>, labels: &mut Vec<StableLabel>) {
        if labels.len() >= STABLE_LABEL_CAPACITY {
            return;
        }
        if let Some(display) = stable_label(node) {
            let key = truncate_plain(&display.to_lowercase(), STABLE_LABEL_MAX_BYTES);
            if seen.insert(key.clone()) {
                labels.push(StableLabel { key, display });
            }
        }
        for child in &node.children {
            visit(child, seen, labels);
            if labels.len() >= STABLE_LABEL_CAPACITY {
                break;
            }
        }
    }

    let mut seen = HashSet::new();
    let mut labels = Vec::new();
    visit(root, &mut seen, &mut labels);
    labels
}

fn stable_label(node: &UiNode) -> Option<String> {
    choose_label(&node.title, &node.description, None)
}

fn candidate_label(node: &UiNode) -> Option<String> {
    choose_label(&node.title, &node.description, Some(&node.value))
}

fn choose_label(title: &str, description: &str, value: Option<&str>) -> Option<String> {
    let title = normalize_label(title);
    let description = normalize_label(description);
    let value = value.map(normalize_label).unwrap_or_default();
    let description_is_richer = !description.is_empty()
        && (title.is_empty()
            || description.chars().count() > title.chars().count().saturating_add(8)
            || description.to_lowercase().contains(&title.to_lowercase()));
    let selected = if description_is_richer {
        description
    } else if !title.is_empty() {
        title
    } else if !description.is_empty() {
        description
    } else {
        value
    };
    (!selected.is_empty()).then_some(selected)
}

fn normalize_label(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_plain(&normalized, STABLE_LABEL_MAX_BYTES)
}

fn candidate_target_lines(root: &UiNode) -> Vec<String> {
    fn visit(node: &UiNode, lines: &mut Vec<String>) {
        if lines.len() >= CANDIDATE_TARGET_CAPACITY {
            return;
        }
        if node.is_interactive() {
            let ref_id = truncate_plain(&node.ref_id, 32);
            let role = truncate_plain(&canonical_role(&node.role), ROLE_MAX_BYTES);
            let label = candidate_label(node)
                .map(|label| display_label(&label))
                .unwrap_or_default();
            lines.push(format!("- {ref_id} {role} \"{label}\""));
        }
        for child in &node.children {
            visit(child, lines);
            if lines.len() >= CANDIDATE_TARGET_CAPACITY {
                break;
            }
        }
    }

    let mut lines = Vec::new();
    visit(root, &mut lines);
    lines
}

fn harness_delta_lines(current: &[StableLabel], baseline: Option<&[StableLabel]>) -> Vec<String> {
    let Some(baseline) = baseline else {
        return vec!["initial observation for this root.".to_string()];
    };
    let current_keys: HashSet<_> = current.iter().map(|label| label.key.as_str()).collect();
    let baseline_keys: HashSet<_> = baseline.iter().map(|label| label.key.as_str()).collect();
    let added: Vec<_> = current
        .iter()
        .filter(|label| !baseline_keys.contains(label.key.as_str()))
        .collect();
    let removed: Vec<_> = baseline
        .iter()
        .filter(|label| !current_keys.contains(label.key.as_str()))
        .collect();
    if added.is_empty() && removed.is_empty() {
        return vec!["no stable label changes.".to_string()];
    }

    let mut lines = Vec::new();
    lines.extend(
        added
            .iter()
            .take(DELTA_ENTRY_CAPACITY)
            .map(|label| format!("+ \"{}\"", display_label(&label.display))),
    );
    if added.len() > DELTA_ENTRY_CAPACITY {
        lines.push(format!("+ ... {} more", added.len() - DELTA_ENTRY_CAPACITY));
    }
    lines.extend(
        removed
            .iter()
            .take(DELTA_ENTRY_CAPACITY)
            .map(|label| format!("- \"{}\"", display_label(&label.display))),
    );
    if removed.len() > DELTA_ENTRY_CAPACITY {
        lines.push(format!(
            "- ... {} more",
            removed.len() - DELTA_ENTRY_CAPACITY
        ));
    }
    lines
}

fn display_label(value: &str) -> String {
    let mut output = String::new();
    let mut chars = value.chars().peekable();
    let mut truncated = false;
    for (count, ch) in chars.by_ref().enumerate() {
        if count >= DISPLAY_LABEL_MAX_CHARS {
            truncated = true;
            break;
        }
        let escaped = match ch {
            '\\' => "\\\\".to_string(),
            '"' => "\\\"".to_string(),
            other if other.is_control() => " ".to_string(),
            other => other.to_string(),
        };
        if output.len() + escaped.len() > DISPLAY_LABEL_MAX_BYTES.saturating_sub('…'.len_utf8()) {
            truncated = true;
            break;
        }
        output.push_str(&escaped);
    }
    if chars.peek().is_some() {
        truncated = true;
    }
    if truncated {
        output.push('…');
    }
    output
}

fn truncate_plain(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let ellipsis = '…';
    let mut end = max_bytes
        .saturating_sub(ellipsis.len_utf8())
        .min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut output = value[..end].to_string();
    if max_bytes >= ellipsis.len_utf8() {
        output.push(ellipsis);
    }
    output
}

fn page(
    output_ref: &str,
    owner_state: Option<String>,
    text: &str,
    offset: usize,
) -> Result<OutputPage, StateError> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return Err(StateError::InvalidOffset(offset));
    }
    let remaining = &text[offset..];
    let (content, consumed) = safe_prefix(remaining, PAGE_BYTES, MAX_MODEL_LINES);
    let next_offset = offset + consumed;
    Ok(OutputPage {
        output_ref: output_ref.to_string(),
        owner_state,
        text: content.to_string(),
        offset,
        next_offset,
        total_bytes: text.len(),
        eof: next_offset == text.len(),
    })
}

fn touch(lru: &mut VecDeque<String>, key: &str) {
    if let Some(index) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(index);
    }
    lru.push_back(key.to_string());
}

static STORE: OnceLock<Mutex<StateStore>> = OnceLock::new();

pub fn global() -> &'static Mutex<StateStore> {
    STORE.get_or_init(|| Mutex::new(StateStore::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::Frame;

    fn root(window_id: u32) -> RootInfo {
        RootInfo {
            app_name: "Test".into(),
            pid: 42,
            title: format!("Window {window_id}"),
            window_id,
            frame: Frame {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
            },
            ..RootInfo::default()
        }
    }

    fn tree(title: &str) -> UiNode {
        UiNode {
            role: "window".into(),
            title: title.into(),
            enabled: true,
            ..UiNode::default()
        }
    }

    #[test]
    fn lru_evicts_least_recently_used_observation() {
        let mut store = StateStore::default();
        let first = store.insert_observation(root(1), tree("one"));
        for window_id in 2..=OBSERVATION_CAPACITY as u32 {
            store.insert_observation(root(window_id), tree(&format!("Window {window_id}")));
        }
        store.get(&first.state_id).unwrap();
        let newest =
            store.insert_observation(root(OBSERVATION_CAPACITY as u32 + 1), tree("newest"));
        assert!(store.get(&first.state_id).is_ok());
        assert!(store.get("S2").is_err());
        assert!(store.get(&newest.state_id).is_ok());
        assert!(matches!(store.get("S2"), Err(StateError::Evicted(_))));
        assert!(store.harness_histories.contains_key(&root(1).identity()));
        assert!(!store.harness_histories.contains_key(&root(2).identity()));
        assert_eq!(store.harness_histories.len(), OBSERVATION_CAPACITY);
    }

    #[test]
    fn newer_root_epoch_rejects_actions_from_old_state() {
        let mut store = StateStore::default();
        let first = store.insert_observation(root(1), tree("one"));
        let second = store.insert_observation(root(1), tree("one changed"));
        assert!(matches!(
            store.validate_for_action(&first.state_id),
            Err(StateError::Stale { .. })
        ));
        assert!(store.validate_for_action(&second.state_id).is_ok());
        let other = store.insert_observation(root(2), tree("other"));
        assert!(store.validate_for_action(&second.state_id).is_ok());
        assert!(store.validate_for_action(&other.state_id).is_ok());
        assert!(first.harness_annotation.contains("observation_sequence: 1"));
        assert!(
            second
                .harness_annotation
                .contains("observation_sequence: 2")
        );
        assert!(other.harness_annotation.contains("observation_sequence: 1"));
    }

    #[test]
    fn bounded_output_continuation_round_trips_without_mutation() {
        for (original, line_limited) in [
            ("0123456789α中文😀\n".repeat(4_000), false),
            ("β\n".repeat(4_001), true),
        ] {
            let mut store = StateStore::default();
            let visible = store.bound_model_text(Some("S9"), original.clone());
            assert!(visible.len() < 20 * 1024);
            let output_ref = visible
                .split_whitespace()
                .find(|part| part.starts_with("@o"))
                .unwrap();
            assert!(matches!(
                store.read_output(output_ref, Some("other-state"), None),
                Err(StateError::OutputOwnerMismatch { .. })
            ));
            let split_character = original
                .char_indices()
                .find(|(_, ch)| ch.len_utf8() > 1)
                .unwrap()
                .0
                + 1;
            for offset in [split_character, original.len() + 1] {
                assert!(
                    matches!(store.read_output(output_ref, Some("S9"), Some(offset)), Err(StateError::InvalidOffset(actual)) if actual == offset)
                );
            }
            let mut rebuilt = visible
                .split("\n\n[output truncated")
                .next()
                .unwrap()
                .to_string();
            assert!(rebuilt.len() <= PREVIEW_BYTES);
            assert!(rebuilt.lines().count() <= MAX_MODEL_LINES);
            if line_limited {
                assert_eq!(rebuilt, "β\n".repeat(2_000));
            }
            let mut offset = rebuilt.len();
            loop {
                let page = store
                    .read_output(output_ref, Some("S9"), Some(offset))
                    .unwrap();
                assert!(page.text.len() <= PAGE_BYTES);
                assert!(page.text.lines().count() <= MAX_MODEL_LINES);
                assert!(page.next_offset > page.offset || page.eof);
                if line_limited && !page.eof {
                    assert!(page.text.ends_with('\n'));
                }
                rebuilt.push_str(&page.text);
                if page.eof {
                    break;
                }
                offset = page.next_offset;
            }
            assert_eq!(rebuilt, original);

            let repeated = store
                .read_output(output_ref, Some("S9"), Some(offset))
                .unwrap();
            let repeated_again = store
                .read_output(output_ref, Some("S9"), Some(offset))
                .unwrap();
            assert_eq!(repeated, repeated_again);
        }
    }

    #[test]
    fn harness_observations_keep_deltas_targets_and_recent_actions() {
        let mut store = StateStore::default();
        let observation_tree = |label: &str| UiNode {
            role: "window".into(),
            title: "Test".into(),
            enabled: true,
            children: vec![
                UiNode {
                    role: "button".into(),
                    title: "Save".into(),
                    enabled: true,
                    ..UiNode::default()
                },
                UiNode {
                    role: "group".into(),
                    description: "Action group".into(),
                    actions: vec!["press".into()],
                    enabled: true,
                    ..UiNode::default()
                },
                UiNode {
                    role: "static_text".into(),
                    title: label.into(),
                    enabled: true,
                    ..UiNode::default()
                },
            ],
            ..UiNode::default()
        };
        let first = store.insert_observation(root(1), observation_tree("Removed"));
        assert!(first.harness_annotation.contains(
            "<state_delta since=\"previous\">\ninitial observation for this root.\n</state_delta>"
        ));
        assert!(
            first
                .harness_annotation
                .contains("<recent_actions>\nnone\n</recent_actions>")
        );
        store.record_actions(&root(1), (0..11).map(|index| format!("press @e{index}")));
        let second = store.insert_observation(root(1), observation_tree("Added"));
        let annotation = &second.harness_annotation;
        assert!(annotation.contains(
            "<recent_actions>\n- press @e3\n- press @e4\n- press @e5\n- press @e6\n- press @e7\n- press @e8\n- press @e9\n- press @e10\n</recent_actions>"
        ));
        for observation in [&first, &second] {
            let button_ref = &observation.tree.children[0].ref_id;
            let action_ref = &observation.tree.children[1].ref_id;
            assert!(!button_ref.is_empty());
            assert!(!action_ref.is_empty());
            assert_ne!(button_ref, action_ref);
            assert!(observation.harness_annotation.contains(&format!(
                "<candidate_targets>\n- {button_ref} button \"Save\"\n- {action_ref} group \"Action group\"\n</candidate_targets>"
            )));
        }
        for baseline in ["previous", "initial"] {
            assert!(annotation.contains(&format!(
                "<state_delta since=\"{baseline}\">\n+ \"Added\"\n- \"Removed\"\n</state_delta>"
            )));
        }
        let unchanged = store.insert_observation(root(1), observation_tree("Added"));
        assert!(unchanged.harness_annotation.contains(
            "<state_delta since=\"previous\">\nno stable label changes.\n</state_delta>"
        ));
    }
}
