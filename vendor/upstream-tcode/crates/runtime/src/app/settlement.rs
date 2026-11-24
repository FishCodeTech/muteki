use super::*;
use tcode_core::control::SettlementAutomation;
use tcode_core::settlement::{SettlementBlockers, SettlementDue, automatic_settlement_due};

const ACTIVITY_BATCH_SIZE: usize = 16;

impl AppState {
    pub(crate) fn start_activity(&mut self, cx: &mut HostCx) {
        self.evaluate_all_settlement(cx);
        self.load_next_activity_batch(cx);
    }

    fn activity_streaming(&self, id: &str) -> bool {
        self.resident(id).is_some_and(|session| {
            session.turn_in_flight
                || session.delivery_in_flight.is_some()
                || matches!(session.runtime, Runtime::Starting { .. })
                || (session.timeline.turn_running && matches!(session.runtime, Runtime::Live(_)))
        })
    }

    pub(super) fn queue_activity_load(&mut self, id: &str, cx: &mut HostCx) {
        if self.activity_in_flight.contains(id) {
            return;
        }
        if self.activity_streaming(id) {
            self.activity_deferred.insert(id.to_owned());
            return;
        }
        self.activity_deferred.remove(id);
        if self.activity_queued.insert(id.to_owned()) {
            self.activity_priority.push_back(id.to_owned());
        } else if !self.activity_priority.iter().any(|pending| pending == id) {
            self.activity_pending.retain(|pending| pending != id);
            self.activity_priority.push_back(id.to_owned());
        }
        self.load_next_activity_batch(cx);
    }

    fn load_next_activity_batch(&mut self, cx: &mut HostCx) {
        if !self.activity_in_flight.is_empty() || self.store_failed {
            return;
        }
        let mut revisions = Vec::new();
        while revisions.len() < ACTIVITY_BATCH_SIZE {
            let Some(id) = self
                .activity_priority
                .pop_front()
                .or_else(|| self.activity_pending.pop_front())
            else {
                break;
            };
            self.activity_queued.remove(&id);
            if self.activity_streaming(&id) {
                self.activity_deferred.insert(id);
                continue;
            }
            revisions.push((id.clone(), self.decision_revision(&id)));
            self.activity_in_flight.insert(id);
        }
        if revisions.is_empty() {
            if self.activity_deferred.is_empty()
                && let Some(started) = self.activity_backfill_started.take()
            {
                log::info!(
                    "thread activity and turn index backfill complete: {} activity rows, {} ms",
                    self.sessions
                        .iter()
                        .filter(|meta| self.thread_activity.contains_key(&meta.id))
                        .count(),
                    started.elapsed().as_millis()
                );
            }
            return;
        }
        let ids: Vec<_> = revisions.iter().map(|(id, _)| id.clone()).collect();
        let store = self.store.clone();
        let flush = self.store_write_barrier(cx);
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = match flush.recv().await {
                Ok(Ok(())) => {
                    host_cx
                        .unblock(move || store.backfill_thread_activity(&ids))
                        .await
                }
                error => Err(std::io::Error::other(format!(
                    "activity write barrier failed: {error:?}"
                ))),
            };
            host_cx.enqueue(move |state, cx| {
                state.activity_in_flight.clear();
                let failed = result.is_err();
                let mut activities = match result {
                    Ok(activities) => activities,
                    Err(error) => {
                        log::warn!("thread activity backfill: {error}");
                        HashMap::new()
                    }
                };
                let mut changed = false;
                for (id, revision) in revisions {
                    if state.find_meta(&id).is_none() {
                        continue;
                    }
                    let activity = activities.remove(&id);
                    if activity.is_none() && !failed {
                        continue;
                    }
                    if state.decision_revision(&id) == revision
                        && let Some(Some(activity)) = activity
                    {
                        state.thread_activity.insert(id.clone(), activity);
                        changed = true;
                    } else if state.activity_streaming(&id) {
                        state.activity_deferred.insert(id);
                    } else if state.activity_queued.insert(id.clone()) {
                        state.activity_priority.push_back(id);
                    }
                }
                if changed {
                    state.evaluate_all_settlement(cx);
                }
                if !failed {
                    state.load_next_activity_batch(cx);
                }
            });
        });
    }

    pub(super) fn evaluate_all_settlement(&mut self, cx: &mut HostCx) {
        let now = now_millis();
        let projection = super::snapshots::ActivityProjection::new(self);
        let decisions: Vec<_> = self
            .sessions
            .iter()
            .map(|meta| (meta.id.clone(), self.settlement_due(meta, now, &projection)))
            .collect();
        self.settlement_deadlines.clear();
        for (id, due) in decisions {
            self.record_settlement_deadline(&id, due, now, cx);
        }
        self.rearm_settlement_timer(cx);
    }

    pub(super) fn evaluate_thread_settlement(&mut self, id: &str, cx: &mut HostCx) {
        self.settle_self_if_due(id, false, cx);
        let mut next = Some(id.to_owned());
        let mut visited = HashSet::new();
        while let Some(id) = next.take() {
            if !visited.insert(id.clone()) {
                break;
            }
            if self.activity_deferred.contains(&id) && !self.activity_streaming(&id) {
                self.queue_activity_load(&id, cx);
            }
            let meta = self.find_meta(&id);
            next = meta
                .as_ref()
                .and_then(|meta| meta.parent_session_id.clone());
            let now = now_millis();
            let projection = super::snapshots::ActivityProjection::new(self);
            let due = meta
                .as_ref()
                .and_then(|meta| self.settlement_due(meta, now, &projection));
            self.record_settlement_deadline(&id, due, now, cx);
        }
        self.rearm_settlement_timer(cx);
    }

    fn record_settlement_deadline(
        &mut self,
        id: &str,
        due: Option<SettlementDue>,
        now: u64,
        cx: &mut HostCx,
    ) {
        self.settlement_deadlines.remove(id);
        if let Some(due) = due {
            if due.due_at <= now {
                self.settle_session_at(id, due.activity_at / 1000, cx);
            } else {
                self.settlement_deadlines.insert(id.to_owned(), due.due_at);
            }
        }
    }

    pub(super) fn rearm_settlement_timer(&mut self, cx: &mut HostCx) {
        let now = now_millis();
        let deadline = self.settlement_deadlines.values().copied().min();
        if self.settlement_timer.as_ref().map(|(at, _)| *at) == deadline {
            return;
        }
        self.settlement_timer = None;
        let Some(deadline) = deadline else {
            return;
        };
        // Monotonic timers exclude system sleep; bound the wait before checking wall time again.
        let delay =
            Duration::from_millis(deadline.saturating_sub(now)).min(Duration::from_secs(5 * 60));
        let host_cx = cx.clone();
        let task = cx.spawn_background(async move {
            smol::Timer::after(delay).await;
            host_cx.enqueue(move |state, cx| {
                if state
                    .settlement_timer
                    .as_ref()
                    .is_some_and(|(at, _)| *at == deadline)
                {
                    state.settlement_timer = None;
                    state.evaluate_all_settlement(cx);
                }
            });
        });
        self.settlement_timer = Some((deadline, task));
    }

    pub(super) fn settlement_due(
        &self,
        meta: &SessionMeta,
        now: u64,
        projection: &super::snapshots::ActivityProjection<'_>,
    ) -> Option<SettlementDue> {
        if self.settings.settlement_automation != SettlementAutomation::ExplicitSignals {
            return None;
        }
        let activity = self.thread_activity.get(&meta.id)?;
        let blockers = self.settlement_blockers(meta, projection);
        let rule = self.settlement_rule(meta);
        automatic_settlement_due(
            meta,
            activity,
            blockers,
            now,
            rule.after_days,
            rule.on_merge,
        )
    }

    /// The host facts that hold a thread out of automatic settlement, one by one.
    pub(super) fn settlement_blockers(
        &self,
        meta: &SessionMeta,
        projection: &super::snapshots::ActivityProjection<'_>,
    ) -> SettlementBlockers {
        let resident = self.resident(&meta.id);
        SettlementBlockers {
            pending_approval: resident
                .is_some_and(|session| !session.timeline.pending_approvals.is_empty()),
            pending_input: resident
                .is_some_and(|session| session.timeline.pending_user_input.is_some()),
            live_run: resident.is_some_and(|session| {
                session.preparing_worktree
                    || matches!(session.runtime, Runtime::Starting { .. })
                    || session.turn_in_flight
                    || session.delivery_in_flight.is_some()
                    || session.timeline.turn_running
            }),
            background_work: resident.is_some_and(|session| session.background_task_count > 0),
            unsettled_children: projection.children_hold(&meta.id),
            pull_request_watches: !meta.pull_request_watches_paused
                && tcode_core::pull_request::watched(&meta.pull_requests)
                    .next()
                    .is_some(),
            pending_human_message: resident.is_some_and(|session| {
                session
                    .queue
                    .iter()
                    .any(|message| message.origin == MessageOrigin::Human)
            }),
            scheduled_wake: resident.is_some_and(|session| {
                session
                    .queue
                    .iter()
                    .any(|message| message.not_before.is_some())
            }),
            pending_agent_message: resident.is_some_and(|session| {
                session
                    .queue
                    .iter()
                    .any(|message| message.origin != MessageOrigin::Human)
            }),
        }
    }

    /// The time and merge rules in force for a thread: its project's overrides, else the
    /// settings.
    pub(super) fn settlement_rule(&self, meta: &SessionMeta) -> SettlementRule {
        let project = meta
            .project_id
            .as_deref()
            .and_then(|id| self.settings.project_settlement_overrides.get(id));
        let days = project.and_then(|settings| settings.auto_settle_after_days);
        let on_merge = project.and_then(|settings| settings.auto_settle_on_merge);
        SettlementRule {
            after_days: days.unwrap_or(self.settings.auto_settle_after_days),
            after_days_from_project: days.is_some(),
            on_merge: on_merge.unwrap_or(self.settings.auto_settle_on_merge),
            on_merge_from_project: on_merge.is_some(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SettlementRule {
    pub(super) after_days: Option<f64>,
    pub(super) after_days_from_project: bool,
    pub(super) on_merge: bool,
    pub(super) on_merge_from_project: bool,
}
