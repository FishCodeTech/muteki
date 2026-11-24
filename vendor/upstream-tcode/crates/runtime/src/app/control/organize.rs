use super::*;
use crate::app::sessions::SettleEffects;
use control_mcp::{
    BatchResult, Expect, Outcome, RecordReviewParams, SettleSelfParams, ThreadChange, ThreadOp,
    UpdateThreadsParams,
};
use tcode_core::control::{
    FollowUp, ReviewItem, ReviewRecord, SelfSettleGrant, SelfSettleWhen, action,
};
use tcode_core::project::SettledOverride;
use tcode_core::pull_request::PullRequestState;
use tcode_protocol::{Command, Principal};

/// How many reviews are kept; the oldest goes first.
const REVIEW_RECORD_LIMIT: usize = 50;
const REVIEW_GROUPS: [&str; 6] = [
    "continue",
    "needs_decision",
    "waiting_on_others",
    "can_settle",
    "parked_or_superseded",
    "unsure",
];
const REVIEW_CONFIDENCE: [&str; 3] = ["clear_evidence", "inference", "unsure"];
const UNSETTLE_NOTE: &str = "override becomes active; nothing is restored";

#[derive(Serialize)]
struct UpdateThreadsAnswer {
    dry_run: bool,
    results: Vec<BatchResult>,
}

impl AppState {
    /// An `expect.max_position` is checked against the store's log end, which is only known
    /// once every queued write has committed; such a batch answers after that read.
    pub(super) fn op_update_threads(
        &mut self,
        caller: &Caller,
        params: UpdateThreadsParams,
        cx: &mut HostCx,
    ) -> Answer {
        let positioned: BTreeSet<String> = params
            .changes
            .iter()
            .filter(|change| {
                change
                    .expect
                    .as_ref()
                    .is_some_and(|expect| expect.max_position.is_some())
            })
            .map(|change| change.thread_id.clone())
            .collect();
        if positioned.is_empty() {
            return Answer::Now(Ok(self.update_threads(caller, params, &HashMap::new(), cx)));
        }
        let store = self.store.clone();
        let flush = self.store_write_barrier(cx);
        let host_cx = cx.clone();
        let caller = caller.clone();
        Answer::Later(cx.spawn_background(async move {
            let ends = match flush.recv().await {
                Ok(Ok(())) => host_cx
                    .unblock(move || {
                        positioned
                            .into_iter()
                            .map(|id| {
                                let end = store.snapshot_log(&id)?.end;
                                Ok((id, end))
                            })
                            .collect::<std::io::Result<HashMap<_, _>>>()
                    })
                    .await
                    .map_err(|error| error.to_string()),
                error => Err(format!("store write barrier failed: {error:?}")),
            };
            finish_on_host(host_cx, move |state, cx| {
                ends.map(|ends| state.update_threads(&caller, params, &ends, cx))
                    .map_err(|error| format!("could not read the threads' logs: {error}"))
            })
            .await
        }))
    }

    fn update_threads(
        &mut self,
        caller: &Caller,
        params: UpdateThreadsParams,
        log_ends: &HashMap<String, u64>,
        cx: &mut HostCx,
    ) -> serde_json::Value {
        let dry_run = params.dry_run.unwrap_or(false);
        let mut cx = agent_cx(cx);
        let results = params
            .changes
            .into_iter()
            .map(|change| self.update_thread(caller, change, dry_run, log_ends, &mut cx))
            .collect();
        serde_json::to_value(UpdateThreadsAnswer { dry_run, results }).unwrap_or_default()
    }

    fn update_thread(
        &mut self,
        caller: &Caller,
        change: ThreadChange,
        dry_run: bool,
        log_ends: &HashMap<String, u64>,
        cx: &mut HostCx,
    ) -> BatchResult {
        let target = change.thread_id;
        let skipped = |reason: String| BatchResult {
            target: target.clone(),
            outcome: Outcome::Skipped,
            reason: Some(reason),
            side_effects: None,
        };
        let meta = match self.batch_target(caller, &target, false) {
            Ok(meta) => meta,
            Err(reason) => return skipped(reason),
        };
        if let Some(expect) = &change.expect
            && !self.expect_holds(&meta, expect, log_ends)
        {
            return skipped("changed_since_preview".into());
        }
        let command = match self.thread_command(&meta, &change.op) {
            Ok(command) => command,
            Err(reason) => return skipped(reason),
        };
        if let Err(error) = self.validate_command_target(&command) {
            return skipped(if error.code == "thread_busy" {
                "busy".into()
            } else {
                error.message
            });
        }
        let side_effects = if dry_run {
            match change.op {
                ThreadOp::Settle => describe_settle(self.settle_effects(&target)),
                ThreadOp::Unsettle => vec![UNSETTLE_NOTE.into()],
                _ => Vec::new(),
            }
        } else {
            let side_effects = self.apply_thread_op(&meta, &change.op, cx);
            let detail = match &change.op {
                ThreadOp::Rename { title } => Some(title.trim().to_owned()),
                ThreadOp::FollowUp { on: true, note } => note.clone(),
                _ => None,
            };
            self.audit(caller, &target, thread_op_name(&change.op), detail, cx);
            side_effects
        };
        BatchResult {
            target: target.clone(),
            outcome: if dry_run {
                Outcome::WouldApply
            } else {
                Outcome::Applied
            },
            reason: None,
            side_effects: (!side_effects.is_empty()).then_some(side_effects),
        }
    }

    fn expect_holds(
        &self,
        meta: &SessionMeta,
        expect: &Expect,
        log_ends: &HashMap<String, u64>,
    ) -> bool {
        expect.last_activity_at.is_none_or(|at| {
            self.thread_activity
                .get(&meta.id)
                .and_then(|activity| activity.last_activity_at())
                == Some(at)
        }) && expect.max_position.is_none_or(|position| {
            log_ends.get(&meta.id).and_then(|end| end.checked_sub(1)) == Some(position)
        }) && expect
            .state
            .as_deref()
            .is_none_or(|state| state.eq_ignore_ascii_case(thread_state(meta)))
    }

    /// The pipe command an op is, or why it would change nothing.
    fn thread_command(&self, meta: &SessionMeta, op: &ThreadOp) -> Result<Command, String> {
        let session_id = meta.id.clone();
        let archived = meta.archived_at.is_some();
        Ok(match op {
            ThreadOp::Settle if meta.is_settled() && !archived => {
                return Err("already settled".into());
            }
            ThreadOp::Settle => Command::SettleSession { session_id },
            ThreadOp::Unsettle if meta.settled_override == Some(SettledOverride::Active) => {
                return Err("already held active".into());
            }
            ThreadOp::Unsettle => Command::UnsettleSession { session_id },
            ThreadOp::Archive if archived => return Err("already archived".into()),
            ThreadOp::Archive if self.turn_running_for(&meta.id) => return Err("busy".into()),
            ThreadOp::Archive => Command::ArchiveSession { session_id },
            ThreadOp::Unarchive if !archived => return Err("not archived".into()),
            ThreadOp::Unarchive => Command::UnarchiveSession { session_id },
            ThreadOp::Pin if meta.pinned_at.is_some() && !meta.is_settled() => {
                return Err("already pinned".into());
            }
            ThreadOp::Pin => Command::PinSession {
                session_id,
                order_key: None,
            },
            ThreadOp::Unpin if meta.pinned_at.is_none() && !archived => {
                return Err("not pinned".into());
            }
            ThreadOp::Unpin => Command::UnpinSession { session_id },
            ThreadOp::Rename { title } if title.trim().is_empty() => {
                return Err("a title cannot be empty".into());
            }
            ThreadOp::Rename { title } => Command::RenameSession {
                session_id,
                title: title.clone(),
            },
            ThreadOp::MarkRead => Command::MarkSessionRead {
                session_id,
                through: meta.updated_at,
            },
            ThreadOp::MarkUnread => Command::MarkSessionUnread { session_id },
            ThreadOp::FollowUp { on: false, .. } if meta.follow_up.is_none() => {
                return Err("not on Follow-up".into());
            }
            ThreadOp::FollowUp { on, note } => Command::SetFollowUp {
                session_id,
                follow_up: on.then(|| follow_up(meta, note)),
            },
            ThreadOp::Park { on } if *on == meta.parked_at.is_some() => {
                return Err(if *on { "already parked" } else { "not parked" }.into());
            }
            ThreadOp::Park { on } => Command::SetParked {
                session_id,
                parked: *on,
            },
            ThreadOp::Keep { .. } if archived => {
                return Err("Unarchive this thread before changing its lifecycle.".into());
            }
            ThreadOp::Keep { on } if *on == meta.auto_settle_disabled_at.is_some() => {
                return Err(if *on { "already kept" } else { "not kept" }.into());
            }
            ThreadOp::Keep { on } => Command::SetAutoSettle {
                session_id,
                enabled: !on,
            },
        })
    }

    /// Perform a validated op through the method the pipe calls; a settle reports what it did.
    fn apply_thread_op(
        &mut self,
        meta: &SessionMeta,
        op: &ThreadOp,
        cx: &mut HostCx,
    ) -> Vec<String> {
        let id = meta.id.as_str();
        match op {
            ThreadOp::Settle => {
                return self
                    .settle_session(id, cx)
                    .map(describe_settle)
                    .unwrap_or_default();
            }
            ThreadOp::Unsettle => {
                self.unsettle_session(id, cx);
                return vec![UNSETTLE_NOTE.into()];
            }
            ThreadOp::Archive => self.archive_session(id, cx),
            ThreadOp::Unarchive => self.unarchive_session(id, cx),
            ThreadOp::Pin => self.pin_session(id, None, cx),
            ThreadOp::Unpin => self.unpin_session(id, cx),
            ThreadOp::Rename { title } => self.rename_session(id, title, cx),
            ThreadOp::MarkRead => self.mark_session_read(id, meta.updated_at, cx),
            ThreadOp::MarkUnread => self.mark_session_unread(id, cx),
            ThreadOp::FollowUp { on, note } => {
                self.set_follow_up(id, on.then(|| follow_up(meta, note)), cx)
            }
            ThreadOp::Park { on } => self.set_parked(id, *on, cx),
            ThreadOp::Keep { on } => self.set_auto_settle(id, !on, cx),
        }
        Vec::new()
    }

    pub(super) fn op_record_review(
        &mut self,
        caller: &Caller,
        params: RecordReviewParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let answer = |record: &ReviewRecord| {
            serde_json::json!({
                "status": "recorded",
                "id": record.id,
                "items": record.items.len(),
                "coverage": record.coverage,
            })
        };
        if let Some(existing) = params.request_id.as_ref().and_then(|id| {
            self.control
                .review_records
                .iter()
                .find(|record| &record.id == id && record.thread_id == caller.thread_id)
        }) {
            return Ok(answer(existing));
        }
        if params.coverage.trim().is_empty() {
            return Err("coverage is required: say what the review looked at".into());
        }
        let mut items = Vec::with_capacity(params.items.len());
        for (index, item) in params.items.into_iter().enumerate() {
            if !self
                .find_meta(&item.thread_id)
                .is_some_and(|meta| self.target_reachable(caller, &caller.grant.scope, &meta, true))
            {
                return Err(format!(
                    "item {index}: thread {} is unknown or outside your grant's scope",
                    item.thread_id
                ));
            }
            if !REVIEW_GROUPS.contains(&item.group.as_str()) {
                return Err(format!(
                    "item {index}: group must be one of {}",
                    REVIEW_GROUPS.join(", ")
                ));
            }
            if !REVIEW_CONFIDENCE.contains(&item.confidence.as_str()) {
                return Err(format!(
                    "item {index}: confidence must be one of {}",
                    REVIEW_CONFIDENCE.join(", ")
                ));
            }
            items.push(ReviewItem {
                thread_id: item.thread_id,
                group: item.group,
                confidence: item.confidence,
                evidence: item.evidence,
                suggestion: item.suggestion,
            });
        }
        let record = ReviewRecord {
            id: params
                .request_id
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            thread_id: caller.thread_id.clone(),
            recorded_at: now_secs(),
            coverage: params.coverage,
            items,
        };
        let records = &mut self.control.review_records;
        records.push(record.clone());
        let excess = records.len().saturating_sub(REVIEW_RECORD_LIMIT);
        records.drain(..excess);
        let records = records.clone();
        self.enqueue_store_write(StoreWrite::WriteReviewRecords(records), cx);
        emit_runtime(
            cx,
            RuntimeEvent::Notice(RuntimeNotice::ReviewRecorded {
                items: record.items.len(),
                coverage: record.coverage.clone(),
            }),
        );
        let mut cx = agent_cx(cx);
        let detail = record.items.len().to_string();
        self.audit(
            caller,
            &caller.thread_id,
            action::RECORD_REVIEW,
            Some(detail),
            &mut cx,
        );
        Ok(answer(&record))
    }

    pub(super) fn op_settle_self(
        &mut self,
        caller: &Caller,
        params: SettleSelfParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let consent_quote = params.consent_quote.trim();
        if consent_quote.is_empty() {
            return Err(
                "consent_required: quote the user's own words that let this thread \
                        settle itself; a question about settling is not consent"
                    .into(),
            );
        }
        let meta = self.find_meta(&caller.thread_id).ok_or("unknown thread")?;
        if !self.user_said(&meta, consent_quote) {
            return Err("consent_required: the quote is not something the user said here".into());
        }
        if let SelfSettleWhen::PrMerged { url } | SelfSettleWhen::PrClosed { url } = &params.when
            && pull_request_state(&meta, url).is_none()
        {
            return Err(format!(
                "no pull request {url} is linked to this thread; link it first"
            ));
        }
        let grant = SelfSettleGrant {
            when: params.when,
            consent_quote: consent_quote.to_owned(),
            granted_at: now_secs(),
        };
        let when = describe_when(&grant.when);
        let detail = grant.consent_quote.clone();
        let mut cx = agent_cx(cx);
        self.change_meta(&caller.thread_id, &mut cx, |meta| {
            meta.self_settle = Some(grant)
        });
        self.audit(
            caller,
            &caller.thread_id,
            action::SETTLE_SELF,
            Some(detail),
            &mut cx,
        );
        Ok(serde_json::json!({
            "status": "armed",
            "when": when,
            "note": "The host settles this thread only after its current turn has ended. A new \
                     message from the user withdraws the grant, and the user can revoke it.",
        }))
    }

    /// Whether `quote` appears, ignoring case and runs of whitespace, in a message the user
    /// sent in `meta`'s own thread; agent and host messages do not count.
    fn user_said(&self, meta: &SessionMeta, quote: &str) -> bool {
        let collapse = |text: &str| {
            text.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        };
        let quote = collapse(quote);
        let unmarked = MessageOrigin::legacy(meta.parent_session_id.is_some());
        self.event_records.get(&meta.id).is_some_and(|log| {
            log.records().iter().any(|record| {
                let text = match &record.event {
                    AgentEvent::ItemCompleted(agent::ThreadItem {
                        content: agent::ItemContent::UserMessage { text, .. },
                        ..
                    })
                    | AgentEvent::SteerRequested { text, .. } => text,
                    _ => return false,
                };
                record.origin.unwrap_or(unmarked) == MessageOrigin::Human
                    && collapse(text).contains(&quote)
            })
        })
    }

    /// Settle a thread whose self-settle grant is met: `end_of_turn` once `turn_ended`, a pull
    /// request condition once its pull request is in that state and no turn runs. A human
    /// message waiting in the queue withdraws the grant instead.
    pub(in crate::app) fn settle_self_if_due(
        &mut self,
        id: &str,
        turn_ended: bool,
        cx: &mut HostCx,
    ) {
        let Some(meta) = self.find_meta(id) else {
            return;
        };
        let Some(grant) = meta.self_settle.clone() else {
            return;
        };
        if meta.archived_at.is_some() || meta.is_settled() {
            return;
        }
        let mut cx = agent_cx(cx);
        if self.resident(id).is_some_and(|session| {
            session
                .queue
                .iter()
                .any(|message| message.origin == MessageOrigin::Human)
        }) {
            self.change_meta(id, &mut cx, |meta| meta.self_settle = None);
            return;
        }
        let due = match &grant.when {
            SelfSettleWhen::EndOfTurn => turn_ended,
            SelfSettleWhen::PrMerged { url } => {
                pull_request_state(&meta, url) == Some(Some(PullRequestState::Merged))
            }
            SelfSettleWhen::PrClosed { url } => {
                pull_request_state(&meta, url) == Some(Some(PullRequestState::Closed))
            }
        };
        if !due || self.settle_thread_busy(id) {
            return;
        }
        if self.settle_session(id, &mut cx).is_none() {
            return;
        }
        self.change_meta(id, &mut cx, |meta| meta.self_settle = None);
        let caller = self.caller_of(&meta);
        self.audit(
            &caller,
            id,
            action::SETTLED_ITSELF,
            Some(grant.consent_quote),
            &mut cx,
        );
    }

    pub fn set_follow_up(
        &mut self,
        session_id: &str,
        follow_up: Option<FollowUp>,
        cx: &mut HostCx,
    ) {
        self.change_meta(session_id, cx, |meta| meta.follow_up = follow_up);
    }

    pub fn set_parked(&mut self, session_id: &str, parked: bool, cx: &mut HostCx) {
        self.change_meta(session_id, cx, |meta| {
            if parked != meta.parked_at.is_some() {
                meta.parked_at = parked.then(now_secs);
            }
        });
    }

    pub fn dismiss_settle_suggestion(&mut self, session_id: &str, cx: &mut HostCx) {
        self.change_meta(session_id, cx, |meta| {
            meta.settle_suggestion_dismissed_at = Some(now_secs());
        });
    }

    pub fn revoke_self_settle(&mut self, session_id: &str, cx: &mut HostCx) {
        self.change_meta(session_id, cx, |meta| meta.self_settle = None);
    }

    /// Apply `change` to a stored thread's meta and persist it when it changed anything;
    /// persisting re-evaluates the thread's settlement.
    pub(super) fn change_meta(
        &mut self,
        session_id: &str,
        cx: &mut HostCx,
        change: impl FnOnce(&mut SessionMeta),
    ) {
        let Some(before) = self.find_meta(session_id) else {
            return;
        };
        let mut meta = before.clone();
        change(&mut meta);
        if meta == before {
            return;
        }
        meta.updated_at = now_secs();
        self.store_meta(meta, cx);
    }
}

/// The context a `tcode_control` write runs under: full authority, agent-authored.
fn agent_cx(cx: &HostCx) -> HostCx {
    let mut cx = cx.clone();
    cx.principal = Principal::Full;
    cx.origin = Some(MessageOrigin::Agent);
    cx.author = None;
    cx
}

/// A thread already on Follow-up keeps the time it was added.
fn follow_up(meta: &SessionMeta, note: &Option<String>) -> FollowUp {
    FollowUp {
        since: meta
            .follow_up
            .as_ref()
            .map_or_else(now_secs, |follow_up| follow_up.since),
        note: note.clone(),
    }
}

fn thread_state(meta: &SessionMeta) -> &'static str {
    if meta.archived_at.is_some() {
        "archived"
    } else if meta.is_settled() {
        "settled"
    } else {
        "active"
    }
}

fn thread_op_name(op: &ThreadOp) -> &'static str {
    match op {
        ThreadOp::Settle => action::SETTLE,
        ThreadOp::Unsettle => action::UNSETTLE,
        ThreadOp::Archive => action::ARCHIVE,
        ThreadOp::Unarchive => action::UNARCHIVE,
        ThreadOp::Pin => action::PIN,
        ThreadOp::Unpin => action::UNPIN,
        ThreadOp::Rename { .. } => action::RENAME,
        ThreadOp::MarkRead => action::MARK_READ,
        ThreadOp::MarkUnread => action::MARK_UNREAD,
        ThreadOp::FollowUp { on: true, .. } => action::FOLLOW_UP,
        ThreadOp::FollowUp { on: false, .. } => action::FOLLOW_UP_OFF,
        ThreadOp::Park { on: true } => action::PARK,
        ThreadOp::Park { on: false } => action::PARK_OFF,
        ThreadOp::Keep { on: true } => action::KEEP,
        ThreadOp::Keep { on: false } => action::KEEP_OFF,
    }
}

fn describe_when(when: &SelfSettleWhen) -> String {
    match when {
        SelfSettleWhen::EndOfTurn => "end of turn".into(),
        SelfSettleWhen::PrMerged { url } => format!("when {url} merges"),
        SelfSettleWhen::PrClosed { url } => format!("when {url} closes"),
    }
}

fn describe_settle(effects: SettleEffects) -> Vec<String> {
    let count =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut described = Vec::new();
    if effects.provider_detached {
        described.push("provider detached".into());
    }
    if effects.pull_request_watches_ended > 0 {
        described.push(format!(
            "{} ended",
            count(effects.pull_request_watches_ended, "PR watch", "PR watches")
        ));
    }
    if effects.queued_dropped > 0 {
        described.push(format!(
            "{} dropped",
            count(
                effects.queued_dropped,
                "non-Human queued message",
                "non-Human queued messages"
            )
        ));
    }
    if effects.terminals_checked > 0 {
        described.push(format!(
            "{} checked; those at an idle shell closed",
            count(effects.terminals_checked, "terminal", "terminals")
        ));
    }
    if effects.unpinned {
        described.push("unpinned".into());
    }
    if effects.input_answered {
        described.push("pending input answered with empty answers".into());
    }
    described
}

/// The state of the thread's pull request at `url`: `None` when none is linked there,
/// `Some(None)` when it has not been read yet.
fn pull_request_state(meta: &SessionMeta, url: &str) -> Option<Option<PullRequestState>> {
    let same = |other: &str| other.trim_end_matches('/') == url.trim_end_matches('/');
    meta.pull_requests
        .iter()
        .filter(|link| link.visible())
        .find(|link| same(&link.url))
        .map(|link| link.snapshot.as_ref().map(|snapshot| snapshot.state))
}

#[cfg(test)]
#[path = "organize_tests.rs"]
mod tests;
