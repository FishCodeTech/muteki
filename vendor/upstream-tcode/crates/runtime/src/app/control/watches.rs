use super::*;
use crate::app::orchestrate::approval_request_summary;
use control_mcp::{BatchResult, CancelNotifyParams, NotifyWhenParams, Outcome};
use tcode_core::control::{ControlWatch, WatchEvent, action};
use tcode_core::notice::CONTROL_CALLBACK_PREFIX;

/// How much of a question or opening message a callback quotes; the rest is one
/// `get_thread` or `outline` away.
const QUOTE_HEAD_CHARS: usize = 120;

#[derive(Serialize)]
struct WatchAnswer<'a> {
    id: &'a str,
    thread_ids: &'a [String],
    events: &'a BTreeSet<WatchEvent>,
    once: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<BatchResult>,
}

impl<'a> From<&'a ControlWatch> for WatchAnswer<'a> {
    fn from(watch: &'a ControlWatch) -> Self {
        Self {
            id: &watch.id,
            thread_ids: &watch.thread_ids,
            events: &watch.events,
            once: watch.once,
            skipped: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct CancelNotifyAnswer {
    removed: Vec<String>,
    unknown: Vec<String>,
}

fn wire_name(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|name| name.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn one_line_head(text: &str, chars: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= chars {
        return line;
    }
    let mut head: String = line.chars().take(chars).collect();
    head.push('…');
    head
}

/// `[<status>, ]turn <n> from "<opening message>"`: which turn ended, numbered as `outline`
/// numbers them, so a watcher can tell one turn of the thread from the next.
fn last_turn_detail(timeline: &Timeline, event: WatchEvent) -> Option<String> {
    let turn = timeline.turns.len().checked_sub(1)?;
    let status = (event == WatchEvent::TurnCompleted)
        .then_some(timeline.last_turn_status)
        .flatten()
        .map(|status| format!("{}, ", wire_name(status)));
    let opening = timeline
        .entries
        .iter()
        .filter(|entry| entry.turn == turn)
        .find_map(|entry| match &entry.content {
            EntryContent::Item(ItemContent::UserMessage {
                text, context_len, ..
            }) => {
                let own = context_len.and_then(|len| text.get(len..)).unwrap_or(text);
                let (own, _) = tcode_services::thread_index::redact(own);
                Some(format!(
                    " from {}",
                    quoted(&one_line_head(&own, QUOTE_HEAD_CHARS))
                ))
            }
            _ => None,
        });
    Some(format!(
        "{}turn {turn}{}",
        status.unwrap_or_default(),
        opening.unwrap_or_default()
    ))
}

/// `text` as one quoted string literal, so another thread's title or question cannot end
/// the quote and read as part of the callback.
fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

impl AppState {
    pub(super) fn op_notify_when(
        &mut self,
        caller: &Caller,
        params: NotifyWhenParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        if params.thread_ids.is_empty() {
            return Err("notify_when needs at least one thread id".into());
        }
        if params.events.is_empty() {
            return Err("notify_when needs at least one event".into());
        }
        let mut meta = self.find_meta(&caller.thread_id).ok_or("unknown thread")?;
        // A retried call finds the watch its first attempt stored under the same key.
        if let Some(watch) = params.request_id.as_deref().and_then(|request_id| {
            meta.control_watches
                .iter()
                .find(|watch| watch.id == request_id)
        }) {
            return Ok(serde_json::json!(WatchAnswer::from(watch)));
        }
        let mut thread_ids = params.thread_ids;
        thread_ids.sort();
        thread_ids.dedup();
        let mut skipped = Vec::new();
        thread_ids.retain(|target| match self.batch_target(caller, target, true) {
            Ok(_) => true,
            Err(reason) => {
                skipped.push(BatchResult {
                    target: target.clone(),
                    outcome: Outcome::Skipped,
                    reason: Some(reason),
                    side_effects: None,
                });
                false
            }
        });
        if thread_ids.is_empty() {
            return Ok(serde_json::json!({ "skipped": skipped }));
        }
        let watch = ControlWatch {
            id: params
                .request_id
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            thread_ids,
            events: params.events.into_iter().collect(),
            once: params.once.unwrap_or(false),
        };
        let answer = serde_json::json!(WatchAnswer {
            skipped,
            ..WatchAnswer::from(&watch)
        });
        let detail = watch.id.clone();
        let watched = watch.thread_ids.clone();
        meta.control_watches.push(watch);
        self.store_meta(meta, cx);
        for target in &watched {
            self.audit(
                caller,
                target,
                action::NOTIFY_WHEN,
                Some(detail.clone()),
                cx,
            );
        }
        Ok(answer)
    }

    pub(super) fn op_cancel_notify(
        &mut self,
        caller: &Caller,
        params: CancelNotifyParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let mut meta = self.find_meta(&caller.thread_id).ok_or("unknown thread")?;
        let (removed, unknown): (Vec<String>, Vec<String>) = params
            .ids
            .into_iter()
            .partition(|id| meta.control_watches.iter().any(|watch| &watch.id == id));
        if !removed.is_empty() {
            meta.control_watches
                .retain(|watch| !removed.contains(&watch.id));
            self.store_meta(meta, cx);
            let detail = removed.join(", ");
            self.audit(
                caller,
                &caller.thread_id,
                action::CANCEL_NOTIFY,
                Some(detail),
                cx,
            );
        }
        Ok(serde_json::json!(CancelNotifyAnswer { removed, unknown }))
    }

    /// Wake every thread that asked to hear `event` on `target_id`, once per watcher
    /// however many of its watches match; a `once` watch is gone after it fires. A thread
    /// hears its own events only when it named itself.
    pub(in crate::app) fn fire_control_watches(
        &mut self,
        target_id: &str,
        event: WatchEvent,
        cx: &mut HostCx,
    ) {
        if !self.settings.control.enabled
            || (event == WatchEvent::Idle && self.turn_running_for(target_id))
        {
            return;
        }
        let fires = |watch: &ControlWatch| {
            watch.events.contains(&event) && watch.thread_ids.iter().any(|id| id == target_id)
        };
        let watchers: Vec<String> = self
            .sessions
            .iter()
            .filter(|meta| meta.control_watches.iter().any(fires))
            .map(|meta| meta.id.clone())
            .collect();
        if watchers.is_empty() {
            return;
        }
        let Some(target) = self.find_meta(target_id) else {
            return;
        };
        let text = format!(
            "{CONTROL_CALLBACK_PREFIX}thread {target_id} ({}) {}{}",
            quoted(&target.title),
            wire_name(event),
            self.watch_event_detail(&target, event)
        );
        for watcher in watchers {
            let Some(mut meta) = self.find_meta(&watcher) else {
                continue;
            };
            // The watcher's grant may have narrowed since it asked.
            let caller = self.caller_of(&meta);
            if !self.target_reachable(&caller, &caller.grant.scope, &target, true) {
                continue;
            }
            let before = meta.control_watches.len();
            meta.control_watches
                .retain(|watch| !(watch.once && fires(watch)));
            if meta.control_watches.len() != before {
                self.store_meta(meta, cx);
            }
            self.deliver_orchestrate_callback_to_parent(&watcher, text.clone(), cx);
        }
    }

    /// `: <what happened>`, read from the target as it stands when the event fires.
    fn watch_event_detail(&self, target: &SessionMeta, event: WatchEvent) -> String {
        let resident = self.resident(&target.id);
        let detail = match event {
            WatchEvent::Idle | WatchEvent::TurnCompleted => {
                resident.and_then(|session| last_turn_detail(&session.timeline, event))
            }
            WatchEvent::NeedsInput => resident
                .and_then(|session| session.timeline.pending_user_input.as_ref())
                .map(|pending| {
                    let question = pending
                        .questions
                        .first()
                        .map(|question| one_line_head(&question.question, QUOTE_HEAD_CHARS))
                        .unwrap_or_default();
                    format!("{} (request_id: {})", quoted(&question), pending.request_id)
                }),
            WatchEvent::NeedsApproval => self
                .approval_requests(&target.id)
                .last()
                .map(approval_request_summary),
            WatchEvent::Settled => target.settled_at.map(|at| format!("at {at}")),
            WatchEvent::PrChanged => {
                let states: Vec<String> = tcode_core::pull_request::watched(&target.pull_requests)
                    .map(|link| {
                        let state = link
                            .snapshot
                            .as_ref()
                            .map_or_else(|| "unread".into(), |snapshot| wire_name(snapshot.state));
                        format!("#{} {state}", link.key.number)
                    })
                    .collect();
                (!states.is_empty()).then(|| states.join(", "))
            }
        };
        detail.map_or_else(String::new, |detail| format!(": {detail}"))
    }
}
