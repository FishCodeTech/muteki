//! Event-derived activity and thread lifecycle policy. All activity clocks use Unix milliseconds.

use agent::{AgentEvent, ItemContent, ThreadItem, TurnStatus};

use serde::{Deserialize, Serialize};

use crate::project::SessionMeta;
use crate::pull_request::{PullRequestSnapshot, PullRequestState};
use crate::session::{MessageOrigin, StoredEvent};

/// How long a user-role message waits for its run before it stops blocking.
/// The bound is absolute so clock skew either way cannot block a thread forever.
const QUEUED_TURN_START_GRACE_MS: u64 = 2 * 60 * 1000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadActivity {
    pub last_message_at: Option<u64>,
    pub last_human_message_at: Option<u64>,
    pub last_run_started_at: Option<u64>,
    pub last_run_completed_at: Option<u64>,
    /// The latest run ended in an error and nothing has run since.
    pub failed: bool,
    /// The latest run was interrupted and nothing has run since.
    pub interrupted: bool,
}

/// Bump when the persisted activity fold semantics change.
pub const THREAD_ACTIVITY_FOLD_VERSION: u32 = 2;

impl ThreadActivity {
    pub fn fold_stored<'a>(
        records: impl IntoIterator<Item = &'a StoredEvent>,
        has_parent: bool,
    ) -> Self {
        let mut activity = Self::default();
        for record in records {
            activity.apply(record, has_parent);
        }
        activity
    }

    /// Whether the record can move a clock; the fold passes over every other.
    pub fn moves_clock(event: &AgentEvent) -> bool {
        matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::UserMessage { .. },
                ..
            }) | AgentEvent::SteerRequested { .. }
                | AgentEvent::TurnStarted { .. }
                | AgentEvent::TurnCompleted { .. }
                | AgentEvent::ProviderStartFailed { .. }
        )
    }

    pub fn apply(&mut self, record: &StoredEvent, has_parent: bool) {
        let Some(ts) = record.ts else { return };
        match &record.event {
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::UserMessage { .. },
                ..
            })
            | AgentEvent::SteerRequested { .. } => {
                self.last_message_at = self.last_message_at.max(Some(ts));
                if record
                    .origin
                    .unwrap_or_else(|| MessageOrigin::legacy(has_parent))
                    == MessageOrigin::Human
                {
                    self.last_human_message_at = self.last_human_message_at.max(Some(ts));
                }
            }
            AgentEvent::TurnStarted { .. } => {
                self.last_run_started_at = self.last_run_started_at.max(Some(ts));
                self.failed = false;
                self.interrupted = false;
            }
            AgentEvent::TurnCompleted { status, .. } => {
                self.last_run_completed_at = self.last_run_completed_at.max(Some(ts));
                self.failed = *status == TurnStatus::Failed;
                self.interrupted = *status == TurnStatus::Interrupted;
            }
            AgentEvent::ProviderStartFailed { .. } => {
                self.last_run_completed_at = self.last_run_completed_at.max(Some(ts));
                self.failed = true;
                self.interrupted = false;
            }
            _ => {}
        }
    }

    pub fn last_activity_at(&self) -> Option<u64> {
        [
            self.last_message_at,
            self.last_run_started_at,
            self.last_run_completed_at,
        ]
        .into_iter()
        .flatten()
        .max()
    }

    /// A recent user-role message no run has picked up yet, unless the latest run failed.
    fn queued_turn_start(&self, now_ms: u64) -> bool {
        let Some(message_at) = self.last_message_at else {
            return false;
        };
        !self.failed
            && now_ms.abs_diff(message_at) <= QUEUED_TURN_START_GRACE_MS
            && [self.last_run_started_at, self.last_run_completed_at]
                .into_iter()
                .all(|run| run.is_none_or(|run| run < message_at))
    }
}

/// What a dispatched orchestrate child is doing, as its lead's Agents view shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecution {
    Working,
    /// Waiting on a request, or on work of its own, inside the child.
    Waiting,
    Finished,
    Failed,
    Cancelled,
    Interrupted,
}

impl AgentExecution {
    pub fn running(self) -> bool {
        matches!(self, Self::Working | Self::Waiting)
    }
}

/// What a dispatched child owes its lead. Every message the lead sends a child
/// (the dispatch brief, each `send`) opens one expectation; the child's
/// `report_result` call closes it. The child's own turns, wake-ups and
/// background work never reach the lead on their own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentReport {
    /// Nothing is owed: never dispatched a message, or cancelled.
    #[default]
    None,
    /// The lead's message is out and no report has come back.
    Expected,
    /// The child's report reached the lead.
    Reported,
    /// The child went quiet twice without reporting; its final message was
    /// delivered in its place.
    Unreported,
    /// The child cannot deliver: its provider failed to start, closed, or
    /// ended the turn in error.
    Failed,
}

impl AgentReport {
    /// The lead is still waiting for this child's report.
    pub fn pending(self) -> bool {
        self == Self::Expected
    }
}

/// Whether a dispatched child's result has reached its lead. Settling the
/// child is the lead's acknowledgement: a finished child is not delivered
/// until then, and a cancelled or archived one never is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDelivery {
    Running,
    AwaitingSettle,
    Settled,
    NotDelivered,
}

impl AgentDelivery {
    pub fn of(meta: &SessionMeta, execution: AgentExecution) -> Self {
        if meta.is_settled() {
            Self::Settled
        } else if meta.cancelled_at.is_some() || meta.archived_at.is_some() {
            Self::NotDelivered
        } else {
            match meta.report() {
                // A child owed a report runs until it reports, however many
                // turns of its own that takes.
                AgentReport::Expected => Self::Running,
                AgentReport::Reported | AgentReport::Unreported | AgentReport::Failed => {
                    Self::AwaitingSettle
                }
                // Children dispatched before expectations were recorded.
                AgentReport::None if execution.running() => Self::Running,
                AgentReport::None => Self::AwaitingSettle,
            }
        }
    }

    /// Outstanding child work keeps its lead waiting and out of settlement.
    pub fn holds_parent(self) -> bool {
        matches!(self, Self::Running | Self::AwaitingSettle)
    }
}

/// Host facts that keep a thread out of automatic settlement.
#[derive(Debug, Clone, Copy, Default)]
pub struct SettlementBlockers {
    pub pending_approval: bool,
    pub pending_input: bool,
    pub live_run: bool,
    /// Background tasks the provider still runs.
    pub background_work: bool,
    /// Orchestrate children not yet settled.
    pub unsettled_children: bool,
    pub pull_request_watches: bool,
    pub pending_human_message: bool,
    pub scheduled_wake: bool,
    /// An agent or server message waits in the queue; settling would drop it.
    pub pending_agent_message: bool,
}

impl SettlementBlockers {
    fn any(self) -> bool {
        self.pending_approval
            || self.pending_input
            || self.live_run
            || self.background_work
            || self.unsettled_children
            || self.pull_request_watches
            || self.pending_human_message
            || self.scheduled_wake
            || self.pending_agent_message
    }
}

/// When to evaluate automatic settlement and the activity stamp it records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementDue {
    pub due_at: u64,
    pub activity_at: u64,
}

pub fn automatic_settlement_due(
    meta: &SessionMeta,
    activity: &ThreadActivity,
    blockers: SettlementBlockers,
    now_ms: u64,
    days: Option<f64>,
    on_merge: bool,
) -> Option<SettlementDue> {
    let terminal = |snapshot: &PullRequestSnapshot| {
        let at = match snapshot.state {
            PullRequestState::Open => return None,
            PullRequestState::Merged => snapshot.merged_at.as_deref(),
            PullRequestState::Closed => snapshot.closed_at.as_deref(),
        }
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .and_then(|at| u64::try_from(at.timestamp_millis()).ok());
        Some((snapshot.state, at))
    };
    let mut latest = None;
    for link in meta.pull_requests.iter().filter(|link| link.visible()) {
        let (state, terminal_at) = terminal(link.snapshot.as_ref()?)?;
        if latest.is_none_or(|(_, at)| terminal_at > at) {
            latest = Some((state, terminal_at));
        }
    }
    if meta.archived_at.is_some()
        || meta.settled_override.is_some()
        || meta.is_settled()
        || meta.auto_settle_disabled_at.is_some()
        || meta.pinned_at.is_some()
        || meta.parent_session_id.is_some()
        || meta.follow_up.is_some()
        || meta.parked_at.is_some()
        || blockers.any()
    {
        return None;
    }
    let last_activity = activity.last_activity_at();
    if meta
        .settle_suggestion_dismissed_at
        .is_some_and(|dismissed| dismissed.saturating_mul(1000) >= last_activity.unwrap_or(0))
    {
        return None;
    }
    // Only the human resumes a thread; agent and server messages do not hold a finished PR open.
    let anchor = activity
        .last_human_message_at
        .unwrap_or(0)
        .max(meta.created_at.saturating_mul(1000));
    if let Some((state, Some(terminal_at))) = latest
        && (state == PullRequestState::Closed || on_merge)
        && terminal_at >= anchor
    {
        return Some(SettlementDue {
            due_at: if activity.queued_turn_start(now_ms) {
                activity
                    .last_message_at?
                    .saturating_add(QUEUED_TURN_START_GRACE_MS)
                    .saturating_add(1)
            } else {
                now_ms
            },
            activity_at: last_activity.unwrap_or(meta.created_at.saturating_mul(1000)),
        });
    }
    let days = days?;
    let last_activity = last_activity?;
    let mut due_at = (last_activity as f64 + days * 86_400_000.0).floor() as u64;
    due_at = due_at.saturating_add(1);
    if activity.queued_turn_start(now_ms) {
        due_at = due_at.max(
            activity
                .last_message_at?
                .saturating_add(QUEUED_TURN_START_GRACE_MS)
                .saturating_add(1),
        );
    }
    Some(SettlementDue {
        due_at,
        activity_at: last_activity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::SettledOverride;

    fn automatic_settlement_at(
        meta: &SessionMeta,
        activity: &ThreadActivity,
        blockers: SettlementBlockers,
        now: u64,
        days: Option<f64>,
        on_merge: bool,
    ) -> Option<u64> {
        automatic_settlement_due(meta, activity, blockers, now, days, on_merge)
            .filter(|due| due.due_at <= now)
            .map(|due| due.activity_at)
    }

    #[test]
    fn settlement_deadlines_include_future_inactivity_and_queued_turn_grace() {
        let meta = thread(&[]);
        let activity = ThreadActivity {
            last_run_completed_at: Some(NOW - HOUR),
            ..Default::default()
        };
        assert_eq!(
            automatic_settlement_due(
                &meta,
                &activity,
                SettlementBlockers::default(),
                NOW,
                Some(3.0),
                true
            ),
            Some(SettlementDue {
                due_at: NOW - HOUR + 3 * 86_400_000 + 1,
                activity_at: NOW - HOUR
            })
        );
        let merged = thread(&[Some((PullRequestState::Merged, Some(NOW - HOUR)))]);
        let queued = ThreadActivity {
            last_message_at: Some(NOW),
            ..Default::default()
        };
        assert_eq!(
            automatic_settlement_due(
                &merged,
                &queued,
                SettlementBlockers::default(),
                NOW,
                None,
                true
            ),
            Some(SettlementDue {
                due_at: NOW + 120_001,
                activity_at: NOW
            })
        );
    }

    #[test]
    fn inactivity_settles_at_the_last_activity_unless_blocked() {
        let meta = SessionMeta::new(agent::ProviderKind::Codex, "/sample".into(), None);
        let activity = ThreadActivity {
            last_message_at: Some(100_000),
            last_run_completed_at: Some(90_000),
            ..Default::default()
        };
        let three_days = 3 * 86_400_000;
        let settle = |meta: &SessionMeta, activity: &ThreadActivity, blockers, now| {
            automatic_settlement_at(meta, activity, blockers, now, Some(3.0), true)
        };
        let blockers = SettlementBlockers::default();
        assert_eq!(
            settle(&meta, &activity, blockers, 100_000 + three_days + 1),
            Some(100_000)
        );
        assert_eq!(
            settle(&meta, &activity, blockers, 100_000 + three_days),
            None,
            "strictly older than the window"
        );
        let now = 100_000 + three_days + 1;
        assert_eq!(
            automatic_settlement_at(&meta, &activity, blockers, now, None, true),
            None,
            "never"
        );
        assert_eq!(
            settle(&meta, &ThreadActivity::default(), blockers, now),
            None,
            "no activity never ages"
        );
        for blocker in [
            "archived",
            "active",
            "settled",
            "disabled",
            "pinned",
            "child",
            "approval",
            "input",
            "run",
            "background",
            "children",
            "watches",
            "human",
            "wake",
            "agent message",
            "follow-up",
            "parked",
            "dismissed",
        ] {
            let mut blocked = meta.clone();
            match blocker {
                "archived" => blocked.archived_at = Some(1),
                "active" => blocked.settled_override = Some(SettledOverride::Active),
                "settled" => blocked.settled_override = Some(SettledOverride::Settled),
                "disabled" => blocked.auto_settle_disabled_at = Some(1),
                "pinned" => blocked.pinned_at = Some(1),
                "child" => blocked.parent_session_id = Some("lead".into()),
                "follow-up" => {
                    blocked.follow_up = Some(crate::control::FollowUp {
                        since: 1,
                        note: None,
                    })
                }
                "parked" => blocked.parked_at = Some(1),
                "dismissed" => blocked.settle_suggestion_dismissed_at = Some(100),
                _ => {}
            }
            let blockers = SettlementBlockers {
                pending_approval: blocker == "approval",
                pending_input: blocker == "input",
                live_run: blocker == "run",
                background_work: blocker == "background",
                unsettled_children: blocker == "children",
                pull_request_watches: blocker == "watches",
                pending_human_message: blocker == "human",
                scheduled_wake: blocker == "wake",
                pending_agent_message: blocker == "agent message",
            };
            assert_eq!(
                settle(&blocked, &activity, blockers, now),
                None,
                "{blocker}"
            );
        }
        let mut dismissed_earlier = meta.clone();
        dismissed_earlier.settle_suggestion_dismissed_at = Some(99);
        assert_eq!(
            settle(&dismissed_earlier, &activity, blockers, now),
            Some(100_000),
            "activity after a dismissed suggestion ends the dismissal"
        );
    }

    const HOUR: u64 = 3_600_000;
    const NOW: u64 = 1_790_000_000_000;

    fn record(origin: Option<MessageOrigin>, ts: u64, event: AgentEvent) -> StoredEvent {
        StoredEvent {
            origin,
            author: None,
            ts: Some(ts),
            event,
            elided: None,
        }
    }

    fn message(origin: MessageOrigin, ts: u64) -> StoredEvent {
        record(
            Some(origin),
            ts,
            AgentEvent::ItemCompleted(ThreadItem {
                id: format!("message-{ts}"),
                parent_item_id: None,
                content: ItemContent::UserMessage {
                    text: "Sample request".into(),
                    context_len: None,
                    attachments: vec![],
                },
            }),
        )
    }

    fn run(started: u64, completed: u64) -> [StoredEvent; 2] {
        [
            record(
                None,
                started,
                AgentEvent::TurnStarted {
                    turn_id: format!("turn-{started}"),
                },
            ),
            record(
                None,
                completed,
                AgentEvent::TurnCompleted {
                    turn_id: format!("turn-{started}"),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ),
        ]
    }

    /// A human request at -5h whose run ended at -4h.
    fn worked() -> Vec<StoredEvent> {
        let mut records = vec![message(MessageOrigin::Human, NOW - 5 * HOUR)];
        records.extend(run(NOW - 5 * HOUR + 1, NOW - 4 * HOUR));
        records
    }

    /// A thread created at -10h with one link per entry: `None` is unsynced.
    fn thread(links: &[Option<(PullRequestState, Option<u64>)>]) -> SessionMeta {
        use crate::pull_request::{
            Mergeability, PullRequestKey, PullRequestSnapshot, PullRequestSource, link_pull_request,
        };
        let mut meta = SessionMeta::new(agent::ProviderKind::Codex, "/sample".into(), None);
        meta.created_at = (NOW - 10 * HOUR) / 1000;
        for (index, link) in links.iter().enumerate() {
            let number = index as u64 + 1;
            link_pull_request(
                &mut meta.pull_requests,
                PullRequestKey::new("github.com", "sample/project", number),
                format!("https://github.com/sample/project/pull/{number}"),
                PullRequestSource::Agent,
                1,
                true,
            );
            meta.pull_requests[index].snapshot = link.map(|(state, at)| {
                let at = at.map(|at| {
                    chrono::DateTime::from_timestamp_millis(at as i64)
                        .unwrap()
                        .to_rfc3339()
                });
                PullRequestSnapshot {
                    state,
                    title: format!("Change {number}"),
                    head_branch: format!("change-{number}"),
                    base_branch: "main".into(),
                    is_draft: false,
                    updated_at: "2026-10-08T00:00:00Z".into(),
                    synced_at: 1,
                    closed_at: at.clone(),
                    merged_at: (state == PullRequestState::Merged).then_some(at).flatten(),
                    author: None,
                    additions: Some(1),
                    deletions: Some(1),
                    changed_files: Some(1),
                    review_decision: None,
                    checks_state: None,
                    mergeability: Mergeability::Unknown,
                }
            });
        }
        meta
    }

    fn settle_at(
        meta: &SessionMeta,
        records: &[StoredEvent],
        now: u64,
        on_merge: bool,
    ) -> Option<u64> {
        let activity = ThreadActivity::fold_stored(records, false);
        automatic_settlement_at(
            meta,
            &activity,
            SettlementBlockers::default(),
            now,
            Some(3.0),
            on_merge,
        )
    }

    #[test]
    fn the_latest_terminal_pull_request_settles_at_the_last_activity() {
        use PullRequestState::{Closed, Merged, Open};
        let records = worked();
        let aged = NOW + 3 * 86_400_000;
        let worked_at = Some(NOW - 4 * HOUR);
        let merged = thread(&[Some((Merged, Some(NOW - 3 * HOUR)))]);
        assert_eq!(settle_at(&merged, &records, NOW, true), worked_at);
        assert_eq!(
            settle_at(&merged, &records, NOW, false),
            None,
            "a merge settles only with the merge setting"
        );
        assert_eq!(
            settle_at(&merged, &records, aged, false),
            worked_at,
            "inactivity still settles"
        );
        let mut pinned = merged.clone();
        pinned.pinned_at = Some(1);
        assert_eq!(settle_at(&pinned, &records, NOW, true), None, "pinned");
        let closed = thread(&[Some((Closed, Some(NOW - 3 * HOUR)))]);
        assert_eq!(settle_at(&closed, &records, NOW, false), worked_at);
        assert_eq!(
            settle_at(&closed, &[], NOW, false),
            Some(NOW - 10 * HOUR),
            "a thread that never ran settles at its creation"
        );
        for (blocking, label) in [
            (
                thread(&[Some((Closed, Some(NOW - 3 * HOUR))), Some((Open, None))]),
                "open",
            ),
            (
                thread(&[Some((Closed, Some(NOW - 3 * HOUR))), None]),
                "unsynced",
            ),
        ] {
            assert_eq!(settle_at(&blocking, &records, NOW, true), None, "{label}");
            assert_eq!(
                settle_at(&blocking, &records, aged, true),
                None,
                "an {label} link blocks inactivity too"
            );
        }
        let mut dismissed = thread(&[Some((Closed, Some(NOW - 3 * HOUR))), Some((Open, None))]);
        let key = dismissed.pull_requests[1].key.clone();
        crate::pull_request::unlink_pull_request(&mut dismissed.pull_requests, &key);
        assert_eq!(
            settle_at(&dismissed, &records, NOW, true),
            worked_at,
            "an unlinked pull request is not a link"
        );
        let latest_merged = thread(&[
            Some((Closed, Some(NOW - 4 * HOUR + 1))),
            Some((Merged, Some(NOW - 3 * HOUR))),
        ]);
        assert_eq!(settle_at(&latest_merged, &records, NOW, true), worked_at);
        assert_eq!(
            settle_at(&latest_merged, &records, NOW, false),
            None,
            "an older closed link does not stand in for the latest merged one"
        );
    }

    #[test]
    fn only_a_later_human_message_keeps_a_terminal_pull_request_open() {
        let closed = thread(&[Some((PullRequestState::Closed, Some(NOW - 3 * HOUR)))]);
        let mut resumed = worked();
        resumed.push(message(MessageOrigin::Human, NOW - 2 * HOUR));
        resumed.extend(run(NOW - 2 * HOUR + 1, NOW - HOUR));
        assert_eq!(settle_at(&closed, &resumed, NOW, true), None);
        for origin in [MessageOrigin::Agent, MessageOrigin::Server] {
            let mut woken = worked();
            woken.push(message(origin, NOW - 2 * HOUR));
            woken.extend(run(NOW - 2 * HOUR + 1, NOW - HOUR));
            assert_eq!(
                settle_at(&closed, &woken, NOW, true),
                Some(NOW - HOUR),
                "{origin:?}"
            );
        }
    }

    #[test]
    fn a_fresh_user_message_holds_settlement_until_a_run_takes_it() {
        let closed = thread(&[Some((PullRequestState::Closed, Some(NOW - 3 * HOUR)))]);
        let mut queued = worked();
        queued.push(message(MessageOrigin::Server, NOW - 60_000));
        assert_eq!(settle_at(&closed, &queued, NOW, true), None);
        assert_eq!(
            settle_at(&closed, &queued, NOW + 61_000, true),
            Some(NOW - 60_000),
            "the grace lasts two minutes"
        );
        let mut started = queued.clone();
        started.push(record(
            None,
            NOW - 30_000,
            AgentEvent::TurnStarted {
                turn_id: "taken".into(),
            },
        ));
        assert_eq!(settle_at(&closed, &started, NOW, true), Some(NOW - 30_000));
        let mut failed = worked();
        failed.push(record(
            None,
            NOW - 90_000,
            AgentEvent::ProviderStartFailed {
                error: "sample failure".into(),
            },
        ));
        failed.push(message(MessageOrigin::Agent, NOW - 60_000));
        assert_eq!(settle_at(&closed, &failed, NOW, true), Some(NOW - 60_000));
    }
}
