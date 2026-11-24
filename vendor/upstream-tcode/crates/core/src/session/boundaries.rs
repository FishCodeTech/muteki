use std::collections::HashMap;

use agent::{AgentEvent, ContextFreshness, ItemContent};
use serde::{Deserialize, Serialize};

/// The part of a timeline needed to assign records to turns. Both the rendered
/// fold and the append transaction advance this state through `apply`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnBoundaries {
    turns: Vec<Boundary>,
    users: HashMap<String, User>,
    compacting: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Boundary {
    provider: Option<String>,
    checkpoint: Option<String>,
    closed: bool,
    running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct User {
    turn: usize,
    pending: bool,
}

/// Decisions the timeline consumes while applying the same event's payload.
#[derive(Debug, Clone, Default)]
pub struct TurnBoundaryChange {
    pub opened: bool,
    pub changed: bool,
    pub target: Option<usize>,
    pub rewind: Option<usize>,
}

impl TurnBoundaries {
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    pub fn current(&self) -> Option<usize> {
        self.turns.len().checked_sub(1)
    }

    pub fn is_open(&self) -> bool {
        self.turns.last().is_some_and(|turn| !turn.closed)
    }

    pub fn provider_turn(&self, id: &str) -> Option<usize> {
        self.turns
            .iter()
            .position(|turn| turn.provider.as_deref() == Some(id))
            .or(self.current())
    }

    pub(super) fn settle_running(&mut self, running: Option<usize>) {
        for (index, turn) in self.turns.iter_mut().enumerate() {
            turn.running = running == Some(index);
        }
    }

    pub fn apply(&mut self, event: &AgentEvent) -> TurnBoundaryChange {
        let opened = self.opens_turn(event);
        if opened {
            self.turns.push(Boundary::default());
        }
        let mut change = TurnBoundaryChange {
            opened,
            changed: opened,
            target: self.current(),
            rewind: None,
        };
        change.changed |= matches!(
            event,
            AgentEvent::TurnStarted { .. }
                | AgentEvent::TurnCompleted { .. }
                | AgentEvent::SessionClosed { .. }
                | AgentEvent::TurnChangesUpdated { .. }
                | AgentEvent::TurnCheckpoint { .. }
                | AgentEvent::RewindCompleted { .. }
                | AgentEvent::SteerRequested { .. }
                | AgentEvent::SteerAccepted { .. }
                | AgentEvent::SteerRejected { .. }
                | AgentEvent::TokenUsage(_)
                | AgentEvent::ContextCompacted(_)
        );
        match event {
            AgentEvent::TurnStarted { turn_id } => {
                if let Some(turn) = self.turns.last_mut() {
                    turn.provider = Some(turn_id.clone());
                    turn.running = true;
                }
            }
            AgentEvent::TurnChangesUpdated { turn_id, .. }
            | AgentEvent::TurnCheckpoint { turn_id, .. } => {
                change.target = self.provider_turn(turn_id);
                if let Some(turn) = change.target.map(|index| &mut self.turns[index]) {
                    if !turn_id.is_empty() || matches!(event, AgentEvent::TurnCheckpoint { .. }) {
                        turn.provider = Some(turn_id.clone());
                    }
                    if let AgentEvent::TurnCheckpoint { checkpoint_id, .. } = event {
                        turn.checkpoint = Some(checkpoint_id.clone());
                    }
                }
            }
            AgentEvent::TurnCompleted { usage, .. } => {
                if let Some(usage) = usage {
                    self.compacting = usage.freshness == ContextFreshness::Compacting;
                }
                if let Some(turn) = self.turns.last_mut() {
                    turn.closed = true;
                    turn.running = false;
                }
            }
            AgentEvent::SessionClosed { .. } => {
                if let Some(turn) = self.turns.last_mut() {
                    turn.running = false;
                }
            }
            AgentEvent::RewindCompleted {
                checkpoint_id,
                mode,
                ..
            } => {
                if mode.includes_conversation()
                    && let Some(target) = self
                        .turns
                        .iter()
                        .position(|turn| turn.checkpoint.as_deref() == Some(checkpoint_id))
                {
                    change.rewind = Some(target);
                    self.turns.truncate(target);
                    self.users.retain(|_, user| user.turn < target);
                    self.compacting = false;
                    change.target = self.current();
                }
            }
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item)
                if item.parent_item_id.is_none() =>
            {
                if matches!(item.content, ItemContent::UserMessage { .. }) {
                    change.changed = true;
                    if let Some(turn) = self.current() {
                        self.users.entry(item.id.clone()).or_insert(User {
                            turn,
                            pending: false,
                        });
                    }
                } else if let Some(user) = self.users.get_mut(&item.id) {
                    user.pending = false;
                    change.changed = true;
                }
            }
            AgentEvent::SteerRequested { request_id, .. } => {
                if let Some(turn) = self.current() {
                    self.users.entry(request_id.clone()).or_insert(User {
                        turn,
                        pending: true,
                    });
                }
            }
            AgentEvent::SteerAccepted { request_id } => {
                let running = self.turns.iter().rposition(|turn| turn.running);
                if let Some(user) = self.users.get_mut(request_id)
                    && user.pending
                {
                    user.pending = false;
                    if let Some(turn) = running {
                        user.turn = turn;
                    }
                    change.target = Some(user.turn);
                } else {
                    change.target = None;
                }
            }
            AgentEvent::SteerRejected { request_id } => {
                if self.users.get(request_id).is_some_and(|user| user.pending) {
                    self.users.remove(request_id);
                }
            }
            AgentEvent::ProposedPlan { item_id, .. } => {
                if let Some(user) = self.users.get_mut(item_id) {
                    user.pending = false;
                    change.changed = true;
                }
            }
            AgentEvent::TokenUsage(usage) => {
                self.compacting = usage.freshness == ContextFreshness::Compacting;
            }
            AgentEvent::ContextCompacted(compaction) => self.compacting = compaction.in_progress,
            _ => {}
        }
        change
    }

    fn opens_turn(&self, event: &AgentEvent) -> bool {
        match event {
            AgentEvent::TurnStarted { .. } | AgentEvent::ProviderRelay { .. } => !self.is_open(),
            AgentEvent::ItemStarted(item)
            | AgentEvent::ItemUpdated(item)
            | AgentEvent::ItemCompleted(item) => {
                item.parent_item_id.is_none()
                    && if matches!(item.content, ItemContent::UserMessage { .. }) {
                        !self.users.contains_key(&item.id) && !self.is_open()
                    } else {
                        self.is_empty()
                    }
            }
            AgentEvent::SteerRequested { .. }
            | AgentEvent::ServedModel { .. }
            | AgentEvent::TurnChangesUpdated { .. }
            | AgentEvent::TurnCheckpoint { .. }
            | AgentEvent::ProviderStartFailed { .. }
            | AgentEvent::Error { .. }
            | AgentEvent::TurnFailed { .. }
            | AgentEvent::SessionClosed { reason: Some(_) }
            | AgentEvent::Delta { .. }
            | AgentEvent::ProposedPlanDelta { .. }
            | AgentEvent::ProposedPlan { .. }
            | AgentEvent::ContextWindowChanged { .. }
            | AgentEvent::ControlAction(_) => self.is_empty(),
            AgentEvent::ContextCompacted(compaction) => {
                !(compaction.in_progress && self.compacting) && self.is_empty()
            }
            AgentEvent::SessionStarted { .. }
            | AgentEvent::TokenUsage(_)
            | AgentEvent::TurnCompleted { .. }
            | AgentEvent::SessionClosed { reason: None }
            | AgentEvent::SteerAccepted { .. }
            | AgentEvent::SteerRejected { .. }
            | AgentEvent::TurnRetry { .. }
            | AgentEvent::TurnAccepted { .. }
            | AgentEvent::TurnBlocked { .. }
            | AgentEvent::McpServersNotAttached
            | AgentEvent::BackgroundTasksChanged { .. }
            | AgentEvent::BackgroundTaskList { .. }
            | AgentEvent::ModelFallbackDetected { .. }
            | AgentEvent::RewindCompleted { .. }
            | AgentEvent::RewindFailed { .. }
            | AgentEvent::RewindPreviewed { .. }
            | AgentEvent::RewindUndone { .. }
            | AgentEvent::RewindUndoFailed { .. }
            | AgentEvent::UnreadableRecord { .. }
            | AgentEvent::ApprovalRequested(_)
            | AgentEvent::ApprovalResolved { .. }
            | AgentEvent::UserInputRequested { .. }
            | AgentEvent::UserInputResolved { .. }
            | AgentEvent::Warning { .. }
            | AgentEvent::PlanResolved { .. }
            | AgentEvent::PlanUpdated { .. }
            | AgentEvent::GoalUpdated { .. }
            | AgentEvent::UsageLimitReached { .. }
            | AgentEvent::ProviderCommands { .. }
            | AgentEvent::ProviderOptions { .. }
            | AgentEvent::ProcessStarted { .. }
            | AgentEvent::NativeAgentUpdate { .. }
            | AgentEvent::RuntimeOperationResult { .. }
            | AgentEvent::RuntimeCapabilities { .. }
            | AgentEvent::ControlDelivery { .. }
            | AgentEvent::UsageDetail { .. }
            | AgentEvent::ContinuationStarted { .. }
            | AgentEvent::Origin { .. } => false,
        }
    }
}
