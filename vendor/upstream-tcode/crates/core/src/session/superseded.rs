use agent::AgentEvent;

use super::boundaries::TurnBoundaryChange;
use super::{Timeline, TurnBoundaries};
use serde::{Deserialize, Serialize};

/// Follows a fold to name the turn-changes snapshot each new one supersedes.
///
/// A snapshot replaces its turn's changes wholesale, and nothing the fold does
/// between two snapshots reads a turn's diffs, so once a later snapshot lands
/// on the same turn the earlier one's diffs never reach a timeline again. The
/// turn is the one the fold itself resolves the snapshot to, in the same
/// lifetime: a rewind ends a turn's lifetime even when its index is reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSnapshots<K> {
    /// The latest snapshot on each turn the fold holds.
    latest: Vec<Option<K>>,
}

impl<K> Default for TurnSnapshots<K> {
    fn default() -> Self {
        Self { latest: Vec::new() }
    }
}

impl<K> TurnSnapshots<K> {
    /// Apply just the turn policy, retaining the snapshot keys on its turns.
    pub fn apply_boundaries(
        &mut self,
        boundaries: &mut TurnBoundaries,
        event: &AgentEvent,
        key: K,
    ) -> (TurnBoundaryChange, Option<K>) {
        let change = boundaries.apply(event);
        let superseded = self.track(
            boundaries.len(),
            matches!(event, AgentEvent::TurnChangesUpdated { .. })
                .then_some(change.target)
                .flatten(),
            key,
        );
        (change, superseded)
    }

    fn track(&mut self, turns: usize, target: Option<usize>, key: K) -> Option<K> {
        self.latest.truncate(turns);
        self.latest.resize_with(turns, || None);
        self.latest[target?].replace(key)
    }

    /// Fold the event recorded at `ts`, which the caller knows as `key`, into
    /// `fold`. When it is a snapshot that supersedes an earlier one, the
    /// earlier one's key is returned.
    pub fn apply_at(
        &mut self,
        fold: &mut Timeline,
        ts: Option<u64>,
        event: &AgentEvent,
        key: K,
    ) -> Option<K> {
        let target = match event {
            AgentEvent::TurnChangesUpdated { turn_id, .. } => Some(fold.provider_turn(turn_id)),
            _ => None,
        };
        fold.apply_at(ts, event);
        let target = target.map(|target| target.unwrap_or(fold.turns.len() - 1));
        self.track(fold.turns.len(), target, key)
    }
}

/// Drop the diffs of a turn-changes snapshot, keeping its paths, kinds and
/// completeness. Returns whether there was any diff to drop.
pub fn drop_turn_diffs(event: &mut AgentEvent) -> bool {
    let AgentEvent::TurnChangesUpdated { changes, .. } = event else {
        return false;
    };
    let mut dropped = false;
    for change in changes {
        dropped |= change.diff.take().is_some();
    }
    dropped
}

#[cfg(test)]
mod tests {
    use agent::{
        ChangeCompleteness, FileChange, FileChangeKind, ItemContent, RewindMode, ThreadItem,
        TurnStatus,
    };

    use super::*;
    use crate::session::StoredEvent;

    fn at(ts: u64, event: AgentEvent) -> StoredEvent {
        StoredEvent {
            origin: None,
            author: None,
            ts: Some(ts),
            event,
            elided: None,
        }
    }

    fn started(turn: &str) -> AgentEvent {
        AgentEvent::TurnStarted {
            turn_id: turn.into(),
        }
    }

    fn completed(turn: &str) -> AgentEvent {
        AgentEvent::TurnCompleted {
            turn_id: turn.into(),
            status: TurnStatus::Completed,
            usage: None,
        }
    }

    fn checkpoint(turn: &str, checkpoint: &str) -> AgentEvent {
        AgentEvent::TurnCheckpoint {
            turn_id: turn.into(),
            checkpoint_id: checkpoint.into(),
        }
    }

    fn rewind(checkpoint: &str) -> AgentEvent {
        AgentEvent::RewindCompleted {
            checkpoint_id: checkpoint.into(),
            mode: RewindMode::Conversation,
            prefill: None,
        }
    }

    /// A snapshot of the turn's net changes: one file whose diff is `diff`.
    fn changes(turn: &str, diff: &str) -> AgentEvent {
        AgentEvent::TurnChangesUpdated {
            turn_id: turn.into(),
            changes: vec![FileChange {
                path: "f".into(),
                kind: FileChangeKind::Modify,
                diff: Some(diff.into()),
            }],
            completeness: ChangeCompleteness::Exact,
        }
    }

    fn message(id: &str, text: &str) -> AgentEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::AssistantMessage { text: text.into() },
        })
    }

    fn user(id: &str) -> AgentEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content: ItemContent::UserMessage {
                text: id.into(),
                context_len: None,
                attachments: Vec::new(),
            },
        })
    }

    /// The indices of the snapshots a later one supersedes, ascending.
    fn superseded(log: &[StoredEvent]) -> Vec<usize> {
        let mut fold = Timeline::default();
        let mut snapshots = TurnSnapshots::default();
        let mut superseded: Vec<usize> = log
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                snapshots.apply_at(&mut fold, record.ts, &record.event, index)
            })
            .collect();
        superseded.sort_unstable();
        superseded
    }

    #[test]
    fn superseded_snapshots_follow_the_turn_the_fold_lands_them_on() {
        let reused_id = vec![
            at(1, started("x")),
            at(2, changes("x", "a")),
            at(3, message("m1", "one")),
            at(4, completed("x")),
            at(5, started("x")),
            // Lands on the first turn named x, superseding record 1 there.
            at(6, changes("x", "b")),
            // Names no turn, so lands on the current one; superseded by record 8.
            at(7, changes("", "c")),
            at(8, message("m2", "two")),
            at(9, changes("", "d")),
            at(10, message("m3", "three")),
            at(11, completed("x")),
        ];
        let opens_turn = vec![
            at(1, changes("x", "a")),
            at(2, message("m1", "one")),
            at(3, changes("x", "b")),
            at(4, message("m2", "two")),
        ];
        let names_turn = vec![
            at(1, started("t")),
            // No turn is named x yet: lands on the current turn and names it.
            at(2, changes("x", "a")),
            at(3, message("m1", "one")),
            at(4, completed("t")),
            at(5, started("u")),
            // Finds the first turn by the name record 1 gave it.
            at(6, changes("x", "b")),
            at(7, message("m2", "two")),
            at(8, completed("u")),
        ];
        let rewound = vec![
            at(1, user("u1")),
            at(2, started("t1")),
            at(3, checkpoint("t1", "c1")),
            at(4, completed("t1")),
            at(5, user("u2")),
            at(6, started("t2")),
            at(7, checkpoint("t2", "c2")),
            at(8, changes("t2", "a")),
            at(9, message("m1", "one")),
            at(10, completed("t2")),
            // Drops the second turn; the next user message reopens index 1.
            at(11, rewind("c2")),
            at(12, user("u3")),
            at(13, started("t3")),
            at(14, changes("t3", "b")),
            at(15, message("m2", "two")),
            at(16, changes("t3", "c")),
            at(17, message("m3", "three")),
            at(18, completed("t3")),
        ];
        let late_changes = vec![
            at(1, started("t")),
            at(2, changes("t", "a")),
            at(3, message("m", "done")),
            at(4, completed("t")),
            at(5, changes("t", "b")),
        ];
        let cases = [
            (
                "a reused turn id lands on its first turn",
                reused_id,
                vec![1, 6],
            ),
            ("a snapshot that opened its turn", opens_turn, vec![0]),
            ("a snapshot that named its turn", names_turn, vec![1]),
            (
                "a rewind ends a turn's lifetime though its index is reused",
                rewound,
                vec![13],
            ),
            ("a snapshot after its turn completed", late_changes, vec![1]),
        ];
        for (label, log, expected) in cases {
            assert_eq!(superseded(&log), expected, "{label}");
            let mut stubbed = log.clone();
            for &index in &expected {
                assert!(drop_turn_diffs(&mut stubbed[index].event), "{label}");
            }
            assert_eq!(
                Timeline::fold_stored(&stubbed),
                Timeline::fold_stored(&log),
                "{label}: the fold is unchanged"
            );
            assert_eq!(
                superseded(&stubbed),
                expected,
                "{label}: a stubbed log names the same snapshots"
            );
        }
    }
}
