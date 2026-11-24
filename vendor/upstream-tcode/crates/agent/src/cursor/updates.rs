//! Cursor's `session/update`s on top of the standard mapping: tool results
//! judged by their `rawOutput`, and task tools with the subagent sessions
//! they run as canonical Subagent items and their children.

use std::collections::HashMap;
use std::path::PathBuf;

use agent_client_protocol::schema::v1 as acp;
use serde_json::Value;

use crate::acp_session::State;
use crate::{AgentEvent, ItemContent, ItemStatus, ThreadItem};

/// One Cursor session and the subagent sessions under it.
pub(super) struct Updates {
    cwd: PathBuf,
    /// Subagent items by item id.
    spawns: HashMap<String, Spawn>,
    /// Subagent sessions by Cursor session id.
    children: HashMap<String, Child>,
}

struct Spawn {
    parent_item_id: Option<String>,
    agent_type: String,
    description: String,
    model: Option<String>,
    status: ItemStatus,
    /// `subagent_state_update` reported how the run ended; it outranks the
    /// task tool's own completion.
    settled: bool,
    announced: bool,
}

struct Child {
    /// The Subagent item this session's activity belongs to.
    spawn_item_id: String,
    state: State,
}

/// Where a subagent session's items go: ids namespaced by its Cursor session
/// id (a child numbers its text blocks from one as the parent does), parented
/// to its Subagent item.
struct Scope<'a> {
    session: &'a str,
    spawn_item_id: &'a str,
}

impl Updates {
    pub(super) fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            spawns: HashMap::new(),
            children: HashMap::new(),
        }
    }

    pub(super) fn is_subagent(&self, session: &acp::SessionId) -> bool {
        self.children.contains_key(session.0.as_ref())
    }

    pub(super) fn session_update(
        &mut self,
        state: &mut State,
        notification: acp::SessionNotification,
    ) -> Vec<AgentEvent> {
        let mut update = notification.update;
        let declined = judge_tool_result(&mut update);
        let session = notification.session_id.0.to_string();
        match self.children.get_mut(&session) {
            None => map(&mut self.spawns, None, state, update, declined),
            Some(child) => {
                let scope = Scope {
                    session: &session,
                    spawn_item_id: &child.spawn_item_id,
                };
                map(
                    &mut self.spawns,
                    Some(&scope),
                    &mut child.state,
                    update,
                    declined,
                )
            }
        }
    }

    /// `subagent_spawned` and `subagent_state_update`, which Cursor sends
    /// when the client negotiates `_meta.subagents`.
    pub(super) fn subagent_update(&mut self, params: &Value) -> Vec<AgentEvent> {
        let (Some(session), Some(update)) = (
            params.get("sessionId").and_then(Value::as_str),
            params.get("update"),
        ) else {
            return Vec::new();
        };
        let Some(subagent) = update.get("subagentSessionId").and_then(Value::as_str) else {
            return Vec::new();
        };
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("subagent_spawned") => {
                let meta = update.pointer("/_meta/cursor");
                let Some(tool_call_id) = meta
                    .and_then(|meta| meta.get("toolCallId"))
                    .and_then(Value::as_str)
                else {
                    return Vec::new();
                };
                let parent = self.children.get(session);
                let scope = parent.map(|parent| Scope {
                    session,
                    spawn_item_id: &parent.spawn_item_id,
                });
                let id = item_id(scope.as_ref(), tool_call_id);
                let parent_item_id = scope.map(|scope| scope.spawn_item_id.to_string());
                let spawn = self
                    .spawns
                    .entry(id.clone())
                    .or_insert_with(|| Spawn::new(parent_item_id));
                if spawn.agent_type.is_empty()
                    && let Some(name) = update.get("name").and_then(Value::as_str)
                {
                    spawn.agent_type = name.to_string();
                }
                if spawn.description.is_empty()
                    && let Some(task) = update.get("task").and_then(Value::as_str)
                {
                    spawn.description = task.to_string();
                }
                if let Some(model) = meta
                    .and_then(|meta| meta.get("model"))
                    .and_then(Value::as_str)
                {
                    spawn.model = Some(model.to_string());
                }
                let event = spawn.event(&id);
                self.children.insert(
                    subagent.to_string(),
                    Child {
                        spawn_item_id: id,
                        state: State::new(self.cwd.clone()),
                    },
                );
                vec![event]
            }
            Some("subagent_state_update") => {
                let status = match update.get("state").and_then(Value::as_str) {
                    Some("completed") => ItemStatus::Completed,
                    Some("failed") => ItemStatus::Failed,
                    Some("cancelled" | "disconnected") => ItemStatus::Interrupted,
                    _ => return Vec::new(),
                };
                let Some(child) = self.children.get_mut(subagent) else {
                    return Vec::new();
                };
                let scope = Scope {
                    session: subagent,
                    spawn_item_id: &child.spawn_item_id,
                };
                let mut events: Vec<AgentEvent> = child
                    .state
                    .flush_text()
                    .into_iter()
                    .filter_map(|event| child_event(&scope, event))
                    .collect();
                if let Some(spawn) = self.spawns.get_mut(&child.spawn_item_id) {
                    spawn.status = status;
                    spawn.settled = true;
                    events.push(spawn.event(&child.spawn_item_id));
                }
                events
            }
            _ => Vec::new(),
        }
    }
}

/// One update of the session `scope` names (the session itself when `None`).
fn map(
    spawns: &mut HashMap<String, Spawn>,
    scope: Option<&Scope<'_>>,
    state: &mut State,
    update: acp::SessionUpdate,
    declined: Option<String>,
) -> Vec<AgentEvent> {
    match &update {
        acp::SessionUpdate::ToolCall(call) if is_task(call.raw_input.as_ref()) => {
            let id = item_id(scope, &call.tool_call_id.0);
            let spawn = spawns
                .entry(id.clone())
                .or_insert_with(|| Spawn::new(scope.map(|scope| scope.spawn_item_id.to_string())));
            spawn.describe(call.raw_input.as_ref());
            return vec![spawn.event(&id)];
        }
        acp::SessionUpdate::ToolCallUpdate(task) => {
            let id = item_id(scope, &task.tool_call_id.0);
            if let Some(spawn) = spawns.get_mut(&id) {
                spawn.describe(task.fields.raw_input.as_ref());
                if !spawn.settled {
                    match task.fields.status {
                        Some(acp::ToolCallStatus::Failed) => spawn.status = ItemStatus::Failed,
                        // A background task returns while its subagent runs on.
                        Some(acp::ToolCallStatus::Completed)
                            if !task
                                .fields
                                .raw_output
                                .as_ref()
                                .and_then(|output| output.get("isBackground"))
                                .and_then(Value::as_bool)
                                .unwrap_or(false) =>
                        {
                            spawn.status = ItemStatus::Completed;
                        }
                        _ => {}
                    }
                }
                return vec![spawn.event(&id)];
            }
        }
        _ => {}
    }
    let mut events = state.apply_update(update);
    if let Some(tool) = declined {
        for event in &mut events {
            if let AgentEvent::ItemUpdated(item) | AgentEvent::ItemCompleted(item) = event
                && item.id == tool
            {
                decline(&mut item.content);
            }
        }
    }
    match scope {
        None => events,
        Some(scope) => events
            .into_iter()
            .filter_map(|event| child_event(scope, event))
            .collect(),
    }
}

/// A subagent session's item, as a child of its Subagent item. Its streamed
/// text, plans, commands, options and usage stay inside it: the item
/// lifecycle is what reaches the subagent's own view.
fn child_event(scope: &Scope<'_>, event: AgentEvent) -> Option<AgentEvent> {
    let adopt = |mut item: ThreadItem| {
        item.id = item_id(Some(scope), &item.id);
        item.parent_item_id = Some(scope.spawn_item_id.to_string());
        item
    };
    match event {
        AgentEvent::ItemStarted(item) => Some(AgentEvent::ItemStarted(adopt(item))),
        AgentEvent::ItemUpdated(item) => Some(AgentEvent::ItemUpdated(adopt(item))),
        AgentEvent::ItemCompleted(item) => Some(AgentEvent::ItemCompleted(adopt(item))),
        _ => None,
    }
}

fn item_id(scope: Option<&Scope<'_>>, id: &str) -> String {
    match scope {
        None => id.to_string(),
        Some(scope) => format!("{}/{id}", scope.session),
    }
}

/// Cursor's task tool, which runs a subagent: its `rawInput` names itself.
fn is_task(raw_input: Option<&Value>) -> bool {
    raw_input
        .and_then(|input| input.get("_toolName"))
        .and_then(Value::as_str)
        == Some("task")
}

impl Spawn {
    fn new(parent_item_id: Option<String>) -> Self {
        Self {
            parent_item_id,
            agent_type: String::new(),
            description: String::new(),
            model: None,
            status: ItemStatus::InProgress,
            settled: false,
            announced: false,
        }
    }

    /// The task tool's `rawInput`: `{prompt, description, subagentType}`.
    fn describe(&mut self, raw_input: Option<&Value>) {
        let field = |key: &str| {
            raw_input
                .and_then(|input| input.get(key))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
        };
        if let Some(agent_type) = field("subagentType") {
            self.agent_type = agent_type.to_string();
        }
        if let Some(description) = field("description") {
            self.description = description.to_string();
        }
    }

    fn event(&mut self, id: &str) -> AgentEvent {
        let item = ThreadItem {
            id: id.to_string(),
            parent_item_id: self.parent_item_id.clone(),
            content: ItemContent::Subagent {
                agent_type: if self.agent_type.is_empty() {
                    "subagent".to_string()
                } else {
                    self.agent_type.clone()
                },
                description: self.description.clone(),
                status: self.status,
                summary: None,
                model: self.model.clone(),
                effort: None,
            },
        };
        if !std::mem::replace(&mut self.announced, true) {
            AgentEvent::ItemStarted(item)
        } else if self.status == ItemStatus::InProgress {
            AgentEvent::ItemUpdated(item)
        } else {
            AgentEvent::ItemCompleted(item)
        }
    }
}

/// Cursor reports every finished tool `completed`; how it went is only in
/// its `rawOutput`: a shell's `exitCode`, a tool's `error`, or a call the
/// user `rejected` or the permissions denied. Also gives a shell result its
/// output as text. Returns the id of a tool that was declined.
fn judge_tool_result(update: &mut acp::SessionUpdate) -> Option<String> {
    let acp::SessionUpdate::ToolCallUpdate(update) = update else {
        return None;
    };
    let fields = &mut update.fields;
    let raw = fields.raw_output.as_ref()?;
    if fields.content.is_none()
        && let Some(output) = shell_output(raw)
    {
        fields.content = Some(vec![acp::ToolCallContent::from(acp::ContentBlock::from(
            output,
        ))]);
    }
    if fields.status != Some(acp::ToolCallStatus::Completed) {
        return None;
    }
    let flag = |key: &str| raw.get(key).and_then(Value::as_bool) == Some(true);
    if flag("rejected") || flag("permissionDenied") {
        fields.status = Some(acp::ToolCallStatus::Failed);
        return Some(update.tool_call_id.0.to_string());
    }
    let errored = raw.get("error").is_some_and(|error| !error.is_null());
    let exit_code = raw.get("exitCode").and_then(Value::as_i64);
    if errored || exit_code.is_some_and(|code| code != 0) {
        fields.status = Some(acp::ToolCallStatus::Failed);
    }
    None
}

/// A shell result's `stdout` and `stderr`, when it printed anything.
fn shell_output(raw: &Value) -> Option<String> {
    let stream = |key: &str| raw.get(key).and_then(Value::as_str);
    if stream("stdout").is_none() && stream("stderr").is_none() {
        return None;
    }
    let output = [stream("stdout"), stream("stderr")]
        .into_iter()
        .flatten()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!output.is_empty()).then_some(output)
}

fn decline(content: &mut ItemContent) {
    match content {
        ItemContent::CommandExecution { status, .. }
        | ItemContent::FileChange { status, .. }
        | ItemContent::ToolCall { status, .. } => *status = ItemStatus::Declined,
        _ => {}
    }
}
