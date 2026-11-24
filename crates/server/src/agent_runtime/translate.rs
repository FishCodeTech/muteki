//! Translates a live engine session's canonical events into the conversation
//! document and the `core.*` events the UI folds.
//!
//! The UI rebuilds text, reasoning and tool rows by replaying persisted
//! events, and reads approvals, user input, the plan and the context gauge
//! from the thread state. This module owns both projections so they cannot
//! drift apart. It performs no I/O: the caller loads the document, applies a
//! burst of events, commits once and then carries out the returned effects.

use super::access::{self, AccessPlan};
use agent::{
    AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOptionKind, ApprovalRequest, Compaction,
    DeltaKind, FileChange, ItemContent, ItemStatus, PlanStep, PlanStepStatus, RewindMode,
    SessionCommand, ThreadItem, TokenUsage, TurnStatus, UserInputDelivery, UserInputQuestion,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

/// Receives each `core.*` event in order; it stages the event on the document.
pub type Sink<'a> = dyn FnMut(&mut Value, &str, Value) + 'a;

const TOOL_OUTPUT_SUMMARY_CHARS: usize = 2000;
const EXPIRED_AT_TURN_END: &str = "回合已结束，待审请求已过期";
const EXPIRED_AT_SESSION_CLOSE: &str = "Runtime 会话已关闭，待审请求已过期";

#[derive(Debug, Clone, PartialEq)]
pub struct RewindCompletion {
    pub mode: RewindMode,
    pub prefill: Option<String>,
    pub later_history: agent::LaterHistory,
    pub files: Option<agent::RewindFiles>,
    pub incomplete: Option<agent::RewindIncomplete>,
    pub undo_token: Option<String>,
}
pub type RewindResult = Result<RewindCompletion, agent::RewindFailure>;

/// What the session reported that the caller must act on after committing.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    ControlDelivery { request_id:String, operation:String, status:String, error:Option<Value> },
    TurnAccepted {
        delivery_id: u64,
    },
    TurnEnded {
        turn_id: String,
        status: &'static str,
        usage: Option<Value>,
        error: Option<String>,
    },
    /// The engine never folded the steer into the running turn; the text has
    /// not reached the model and belongs in the queue.
    SteerRejected {
        request_id: String,
        steer: Value,
    },
    Rewind {
        checkpoint_id: String,
        result: RewindResult,
    },
    SessionClosed {
        reason: Option<String>,
        resume_available: bool,
    },
}

#[derive(Debug, Default)]
pub struct Effects {
    /// Answers this server gives on the user's behalf, per the access mode.
    pub commands: Vec<SessionCommand>,
    pub outcomes: Vec<Outcome>,
}

/// Who the session is, as the UI addresses it.
#[derive(Debug, Clone)]
pub struct Identity {
    pub agent_session_id: String,
    pub engine: String,
    pub adapter_id: String,
    pub instance_id: String,
    pub transport: String,
    pub cwd: String,
    /// Bumped whenever the thread's session is replaced, so an answer meant
    /// for an earlier session is recognizably stale.
    pub generation: u64,
    pub resumed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Assistant,
    Reasoning,
    Tool,
}

#[derive(Debug, Clone)]
struct Item {
    turn: String,
    role: Role,
    text: String,
    parent: Option<String>,
    started: Instant,
    ended: Option<Instant>,
    open: bool,
    /// A provider announcement alone is not execution: only a follow-up
    /// update means the tool actually progressed.
    advanced: bool,
    name: String,
    /// The id a message keeps after its item moves to another turn, so live
    /// and stored copies stay one message.
    message_id: Option<String>,
}

#[derive(Clone)]
pub struct Translator {
    identity: Identity,
    access: AccessPlan,
    runtime_turn_id: Option<String>,
    routed_turn: Option<String>,
    routed_agent: Option<String>,
    items: HashMap<String, Item>,
    native_origins: HashMap<String,String>,
    automatic: HashSet<String>,
    turn_error: Option<String>,
    resume_available: bool,
    /// A compaction operation settled on its own event while the provider
    /// still owes the end of the turn it ran it in; that end is not the next
    /// turn's.
    trailing_turn_end: Option<String>,
    /// Top-level items that arrived while no turn ran, such as a detached
    /// subagent's report; the agent-started turn that follows adopts them.
    idle_items: Vec<String>,
    /// Wall-clock start of the foreground turn, for the model/tool time split.
    turn_started: Option<Instant>,
}

/// How a streamed text moved from what was already shown.
enum Growth {
    Append(String),
    Replace(String),
}

fn growth(current: &str, next: &str) -> Option<Growth> {
    if next == current {
        None
    } else if let Some(suffix) = next.strip_prefix(current) {
        Some(Growth::Append(suffix.to_owned()))
    } else {
        Some(Growth::Replace(next.to_owned()))
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn capped(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn snake<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn tool_status(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::InProgress => "running",
        ItemStatus::Completed => "completed",
        ItemStatus::Failed => "failed",
        ItemStatus::Interrupted => "cancelled",
        ItemStatus::Declined => "declined",
    }
}

fn option_kind(kind: ApprovalOptionKind) -> &'static str {
    match kind {
        ApprovalOptionKind::AllowOnce => "allow_once",
        ApprovalOptionKind::AllowAlways => "allow_always",
        ApprovalOptionKind::RejectOnce => "reject_once",
        ApprovalOptionKind::RejectAlways => "reject_always",
    }
}

fn mcp_server(name: &str) -> Option<&str> {
    name.strip_prefix("mcp__")
        .and_then(|rest| rest.split("__").next())
        .filter(|server| !server.is_empty())
}

fn files(changes: &[FileChange]) -> Value {
    json!(
        changes
            .iter()
            .map(|change| json!({"path":change.path,"kind":snake(&change.kind),"old_path":Value::Null,"diff":change.diff}))
            .collect::<Vec<_>>()
    )
}

/// The canonical tool row for an item, or `None` for message-shaped items.
struct ToolView {
    name: String,
    kind: String,
    title: String,
    input: Value,
    output: Option<String>,
    status: ItemStatus,
    exit_code: Option<i32>,
    is_agent: bool,
    model: Option<String>,
    native: Value,
}

fn tool_view(content: &ItemContent) -> Option<ToolView> {
    let view = |name: &str, kind: &str, title: String, input, output, status| ToolView {
        name: name.to_owned(),
        kind: kind.to_owned(),
        title,
        input,
        output,
        status,
        exit_code: None,
        is_agent: false,
        model: None,
        native: json!({}),
    };
    Some(match content {
        ItemContent::UserMessage { .. }
        | ItemContent::AssistantMessage { .. }
        | ItemContent::Reasoning { .. } => return None,
        ItemContent::CommandExecution {
            command,
            output,
            exit_code,
            status,
        } => ToolView {
            exit_code: *exit_code,
            ..view(
                "shell",
                "command",
                command.clone(),
                json!({"command":command}),
                Some(output.clone()),
                *status,
            )
        },
        ItemContent::FileChange { changes, status } => view(
            "file_change",
            "file_change",
            changes
                .iter()
                .map(|change| change.path.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            json!({"changes":files(changes)}),
            None,
            *status,
        ),
        ItemContent::ToolCall {
            presentation,
            name,
            input,
            output,
            status,
            ..
        } => {
            let server = mcp_server(name);
            let lowered = name.to_ascii_lowercase();
            let kind = if server.is_some() {
                "mcp"
            } else if matches!(lowered.as_str(), "webfetch" | "websearch" | "web_fetch" | "web_search" | "fetch") {
                "web"
            } else {
                "other"
            };
            ToolView {
                kind: presentation.as_ref().map_or_else(|| kind.to_owned(), |p| p.kind.clone()),
                title: presentation.as_ref().map_or_else(|| name.clone(), |p| p.title.clone()),
                native: {
                    let mut native = server.map_or_else(|| json!({}), |server| json!({"mcp_server":server}));
                    if let Some(presentation) = presentation {
                        native["output_unavailable"] = json!(presentation.output_unavailable);
                    }
                    native
                },
                ..view(name, kind, name.clone(), input.clone(), output.clone(), *status)
            }
        }
        ItemContent::Subagent {
            agent_type,
            description,
            status,
            summary,
            model,
            effort,
        } => ToolView {
            is_agent: true,
            model: model.clone(),
            native: json!({"effort":effort}),
            ..view(
                if agent_type.is_empty() { "Agent" } else { agent_type },
                "agent",
                description.clone(),
                json!({"description":description,"agent_type":agent_type}),
                summary.clone(),
                *status,
            )
        },
        ItemContent::ImageRead { path, image } => view(
            "image_read",
            "other",
            path.clone(),
            json!({"path": path, "image": image}),
            None,
            ItemStatus::Completed,
        ),
        ItemContent::WebSearch { query } => view(
            "web_search",
            "web",
            query.clone(),
            json!({"query":query}),
            None,
            ItemStatus::Completed,
        ),
        ItemContent::Other {
            provider_kind,
            summary,
        } => view(
            provider_kind,
            "other",
            summary.clone(),
            json!({}),
            Some(summary.clone()),
            ItemStatus::Completed,
        ),
    })
}

pub(crate) fn usage_json(usage: &TokenUsage) -> Value {
    let total = usage.turn_processed_tokens.or_else(|| match (usage.input_tokens, usage.output_tokens) {
        (None, None) => None,
        (input, output) => Some(input.unwrap_or(0) + output.unwrap_or(0)),
    });
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read_tokens": usage.cached_input_tokens,
        "cache_write_tokens": usage.cache_write_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
        "total_tokens": total,
        "model_context_window": usage.context_window,
        "context_used": usage.used_tokens,
        "total_processed_tokens": usage.total_processed_tokens,
        "reported_cost": usage.cost_usd,
        "duration_ms": usage.duration_ms,
        "token_coverage": if usage.input_tokens.is_some() || usage.output_tokens.is_some() { "reported" } else { "unknown" },
    })
}

fn state(doc: &mut Value) -> &mut Value {
    if !doc["thread"]["state"].is_object() {
        doc["thread"]["state"] = json!({});
    }
    &mut doc["thread"]["state"]
}

/// Whether wakeups outlive their engine session depends on the engine (Grok
/// restores them with `session/load`; Claude keeps only durable crons), so a
/// closed session leaves them listed as unconfirmed rather than gone.
pub(crate) fn unconfirm_schedules(doc: &mut Value, reason: &str) -> bool {
    let Some(tasks) = doc["thread"]["state"]["scheduled_tasks"].as_array_mut() else { return false };
    let mut changed = false;
    for task in tasks.iter_mut().filter(|task| task["unconfirmed"].is_null()) {
        task["unconfirmed"] = json!(reason);
        changed = true;
    }
    changed
}

fn current_turn(doc: &Value) -> String {
    doc["thread"]["state"]["running_turn_id"]
        .as_str()
        .filter(|turn| !turn.is_empty())
        .or_else(|| {
            doc["turns"]
                .as_array()
                .and_then(|turns| turns.last())
                .and_then(|turn| turn["turn_id"].as_str())
        })
        .unwrap_or("")
        .to_owned()
}

fn turn_row<'a>(doc: &'a mut Value, turn: &str) -> Option<&'a mut Value> {
    doc["turns"]
        .as_array_mut()?
        .iter_mut()
        .find(|row| row["turn_id"] == turn)
}

/// Point `pending_approval` at the oldest row still waiting on the user.
pub fn refresh_pending(doc: &mut Value) {
    let state = state(doc);
    if !state["pending_approvals"].is_object() {
        state["pending_approvals"] = json!({});
    }
    let oldest = state["pending_approvals"]
        .as_object()
        .into_iter()
        .flat_map(|rows| rows.values())
        .filter(|row| matches!(row["status"].as_str(), Some("pending" | "resolving" | "delivery_unknown")))
        .min_by(|a, b| {
            a["requested_at"]
                .as_str()
                .unwrap_or("")
                .cmp(b["requested_at"].as_str().unwrap_or(""))
        })
        .cloned();
    state["pending_approval"] = oldest.unwrap_or(Value::Null);
    refresh_plan_phase(doc);
}

/// Requests nobody can answer any more stay visible as expired rather than
/// vanishing, so the user sees what the engine had asked. A background
/// subagent's request outlives the turn that spawned it, so `keep_background`
/// spares rows that belong to one.
pub fn expire_pending(doc: &mut Value, reason: &str, keep_background: bool) {
    let state = state(doc);
    if let Some(rows) = state["pending_approvals"].as_object_mut() {
        for row in rows.values_mut() {
            if !matches!(row["status"].as_str(), Some("pending" | "resolving")) {
                continue;
            }
            if keep_background && !row["native"]["parent_item_id"].is_null() {
                continue;
            }
            row["status"] = json!("expired");
            row["expired_reason"] = json!(reason);
            row["response_capability"] = json!({"answerable":false,"reason":reason});
        }
    }
    if !keep_background || state["pending_user_input"]["native"]["parent_item_id"].is_null() {
        state["pending_user_input"] = Value::Null;
    }
    refresh_pending(doc);
}

/// Derive the plan phase from its tasks and whatever waits on the user.
pub fn refresh_plan_phase(doc: &mut Value) {
    let state = state(doc);
    if !state["plan"].is_object() {
        return;
    }
    let waiting = !state["pending_approval"].is_null() || !state["pending_user_input"].is_null();
    let tasks = state["plan"]["tasks"].as_array().cloned().unwrap_or_default();
    let phase = if tasks.is_empty() {
        "cleared"
    } else if waiting {
        "awaiting_decision"
    } else if tasks.iter().all(|task| task["status"] == "completed") {
        "completed"
    } else if tasks.iter().any(|task| task["status"] == "in_progress") {
        "executing"
    } else {
        "proposed"
    };
    state["plan"]["phase"] = json!(phase);
    state["plan"]["awaiting"] = if waiting {
        state["pending_approval"]
            .get("approval_id")
            .map(|id| json!({"kind":"approval","id":id}))
            .or_else(|| {
                state["pending_user_input"]
                    .get("request_id")
                    .map(|id| json!({"kind":"user_input","id":id}))
            })
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
}

/// The reject option offered for an approval whose engine lists none; it
/// answers with the engine's cancellation, which also ends the turn.
const CANCEL_OPTION: &str = "muteki.cancel";

/// Map the UI's approval answer onto one of the engine's own options.
pub fn choose_option(
    row: &Value,
    decision: &str,
    scope: &str,
    option_id: Option<&str>,
) -> Result<ApprovalDecision, &'static str> {
    if decision == "cancel" && option_id.is_none() { return Ok(ApprovalDecision::Cancel); }
    let options = row["options"].as_array().cloned().unwrap_or_default();
    let decide = |id: &str| {
        if id == CANCEL_OPTION {
            ApprovalDecision::Cancel
        } else {
            ApprovalDecision::Option(id.to_owned())
        }
    };
    if let Some(option_id) = option_id.filter(|id| !id.is_empty()) {
        return options
            .iter()
            .find(|option| option["option_id"] == option_id)
            .map(|_| decide(option_id))
            .ok_or("conversation.approval.option_invalid");
    }
    let wanted: &[&str] = match (decision, scope) {
        ("allow", "once" | "") => &["allow_once"],
        ("allow", "session") => &["allow_always"],
        ("deny", "once" | "") => &["reject_once", "reject_always"],
        ("deny", "session") => &["reject_always", "reject_once"],
        ("allow" | "deny", _) => return Err("conversation.approval.scope_invalid"),
        _ => return Err("conversation.approval.invalid"),
    };
    for kind in wanted {
        if let Some(option) = options.iter().find(|option| option["kind"] == *kind) {
            return Ok(decide(option["option_id"].as_str().unwrap_or("")));
        }
    }
    match decision {
        "deny" => Ok(ApprovalDecision::Cancel),
        _ if scope == "session" => Err("conversation.approval.scope_invalid"),
        _ => Err("conversation.approval.option_invalid"),
    }
}

/// Assemble the engine's answer map from the UI's per-question answers. A
/// single-select answer is the chosen label, or the free text that overrides
/// it; a multi-select answer is the list of labels plus any free text.
pub fn assemble_answers(
    row: &Value,
    decision: &str,
    answers: &Value,
) -> Result<Map<String, Value>, &'static str> {
    match decision {
        "submit" => {}
        "cancel" | "decline" => {let mut result=Map::new();if row["user_input_kind"]=="elicitation"{result.insert("$muteki_action".into(),json!(decision));}return Ok(result);},
        _ => return Err("conversation.user_input.decision_invalid"),
    }
    let mut assembled = Map::new();
    for question in row["questions"].as_array().into_iter().flatten() {
        let id = question["question_id"].as_str().unwrap_or("");
        if question["kind"]=="header"{continue;}
        let answer = &answers[id];
        let primitive=if answer.is_string(){answer.as_str().unwrap_or("").to_owned()}else if answer.is_boolean()||answer.is_number(){answer.to_string()}else{String::new()};
        let text = answer["text"].as_str().map(str::trim).unwrap_or(primitive.as_str());
        let values: Vec<String> = answer.get("values").and_then(Value::as_array).or_else(||answer.as_array())
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let labels: Vec<&str> = question["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|option| option["value"].as_str())
            .collect();
        if !labels.is_empty() && values.iter().any(|value| !labels.contains(&value.as_str())) {
            return Err("conversation.user_input.option_invalid");
        }
        if !text.is_empty() && question["allow_free_text"] == false && !matches!(question["kind"].as_str(),Some("boolean"|"number")) {
            return Err("conversation.user_input.free_text_forbidden");
        }
        let value = if question["requested_schema"]["type"]=="array" && labels.is_empty() {
            if answer.is_array(){Some(answer.clone())}else if answer["values"].is_array(){Some(answer["values"].clone())}else if !text.is_empty(){Some(serde_json::from_str(text).map_err(|_|"conversation.user_input.constraints_invalid")?)}else{None}
        } else if question["kind"] == "multi_select" {
            let mut values = values;
            if !text.is_empty() {
                values.push(text.to_owned());
            }
            (!values.is_empty()).then(|| json!(values))
        } else if !text.is_empty() {
            Some(json!(text))
        } else {
            values.into_iter().next().map(Value::String)
        };
        match value {
            Some(mut value) => {
                if question["requested_schema"].is_object() {
                    let raw = value.as_str().unwrap_or("");
                    let wire = if let Some(values)=value.as_array(){json!(values.iter().map(|value|value.as_str().and_then(|key|question["wire_values"].get(key)).cloned().unwrap_or_else(||value.clone())).collect::<Vec<_>>())}else{question["wire_values"].get(raw).cloned().unwrap_or_else(||value.clone())};
                    let typed = match question["requested_schema"]["type"].as_str() {
                        Some("number"|"integer") if wire.is_string() => serde_json::from_str::<Value>(raw).map_err(|_|"conversation.user_input.number_invalid")?,
                        Some("boolean") if wire.is_string() => match raw.to_ascii_lowercase().as_str() { "yes"|"true"=>json!(true), "no"|"false"=>json!(false), _ if value.is_boolean()=>value.clone(), _=>return Err("conversation.user_input.boolean_invalid") },
                        _ => wire,
                    };
                    let validator = jsonschema::validator_for(&question["requested_schema"]).map_err(|_|"conversation.user_input.schema_invalid")?;
                    if !validator.is_valid(&typed) { return Err("conversation.user_input.constraints_invalid"); }
                    if question["typed_answers"]==true {value=typed;}
                }
                assembled.insert(id.to_owned(), value);
            }
            None if question["required"] == true => {
                return Err("conversation.user_input.answer_required");
            }
            None => {}
        }
    }
    if row["requested_schema"].is_object() {
        let validator=jsonschema::validator_for(&row["requested_schema"]).map_err(|_|"conversation.user_input.schema_invalid")?;
        if !validator.is_valid(&Value::Object(assembled.clone())){return Err("conversation.user_input.constraints_invalid");}
    }
    Ok(assembled)
}

impl Translator {
    pub fn new(identity: Identity, access: AccessPlan) -> Self {
        Self {
            identity,
            access,
            runtime_turn_id: None,
            routed_turn: None,
            routed_agent: None,
            items: HashMap::new(),
            native_origins: HashMap::new(),
            automatic: HashSet::new(),
            turn_error: None,
            resume_available: false,
            trailing_turn_end: None,
            idle_items: Vec::new(),
            turn_started: None,
        }
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    fn send(&self, doc: &mut Value, emit: &mut Sink, kind: &str, mut payload: Value) {
        if payload.get("turn_id").is_none() {
            payload["turn_id"] = json!(self.turn(doc));
        }
        if let Some(agent) = &self.routed_agent { payload["agent_id"] = json!(agent); }
        payload["agent_session_id"] = json!(self.identity.agent_session_id);
        let routed=self.turn(doc);let native=doc["turns"].as_array().into_iter().flatten().find(|row|row["turn_id"]==routed).and_then(|row|row["runtime_turn_id"].as_str()).map(str::to_owned).or_else(||self.runtime_turn_id.clone());
        if let Some(turn)=native{payload["runtime_turn_id"]=json!(turn);}
        emit(doc, kind, payload);
    }

    fn native_turn(&self, doc: &Value, native: &str) -> Option<String> {
        self.native_origins.get(native).cloned().or_else(|| {
            doc["turns"].as_array().into_iter().flatten()
                .find(|row| row["runtime_turn_id"] == native
                    && row["agent_session_id"] == self.identity.agent_session_id)
                .and_then(|row| row["turn_id"].as_str()).map(str::to_owned)
        })
    }

    fn turn(&self, doc: &Value) -> String {
        self.routed_turn.clone().unwrap_or_else(|| current_turn(doc))
    }

    fn item_turn(&self, doc: &Value, id: &str, parent: Option<&str>) -> String {
        self.items.get(id).or_else(|| parent.and_then(|id| self.items.get(id)))
            .map(|item| item.turn.clone()).or_else(|| {
                doc["tools"].as_array().into_iter().flatten()
                    .find(|row| row["tool_id"] == id || parent.is_some_and(|p|row["tool_id"] == p))
                    .and_then(|row| row["turn_id"].as_str()).map(str::to_owned)
            }).unwrap_or_else(|| self.turn(doc))
    }

    fn runtime(&self) -> Value {
        json!({"engine":self.identity.engine,"adapter_id":self.identity.adapter_id,"instance_id":self.identity.instance_id})
    }

    pub fn apply(&mut self, doc: &mut Value, event: AgentEvent, emit: &mut Sink, effects: &mut Effects) {
        match event {
            AgentEvent::ProcessStarted {pid} => {doc["native_session"]["process_group"]=json!(pid);}
            AgentEvent::NativeAgentUpdate { mut patch } => {
                patch["turn_id"] = json!(self.turn(doc));
                patch["origin"] = json!("provider_native");
                patch["is_agent"] = json!(true);
                let payload = json!({"source":"runtime","adapter_id":self.identity.adapter_id,"turn_id":self.turn(doc),"patch":true,"agents":[patch]});
                match crate::chat_core::inject_agents(doc, &payload, &now()) {
                    Ok(events) => for (kind, body) in events { self.send(doc, emit, kind, body); },
                    Err((code, message)) => self.runtime_error(doc, emit, code, message),
                }
            }
            AgentEvent::RuntimeOperationResult{..}=>{},
            AgentEvent::RuntimeCapabilities { capabilities } => {
                if doc["native_session"]["capabilities"]!=capabilities { let revision=doc["native_session"]["capability_revision"].as_u64().unwrap_or(0)+1;doc["native_session"]["capability_revision"]=json!(revision); }
                doc["native_session"]["capabilities"]=capabilities.clone();
                self.send(doc,emit,"core.runtime.capabilities_updated",json!({"capabilities":capabilities}));
            }
            AgentEvent::ControlDelivery { request_id, operation, status, error } => {
                if operation=="steer"&&matches!(status.as_str(),"in_flight"|"unknown"|"consumption_unknown")&&doc["pending_steers"][&request_id].is_object(){doc["pending_steers"][&request_id]["consumption_state"]=json!(if status=="in_flight"{"in_flight"}else{"unknown"});doc["pending_steers"][&request_id]["consumption_error"]=json!(error);}
                if operation=="steer"&&status=="accepted"{self.steer_accepted(doc, emit, &request_id);}
                effects.outcomes.push(Outcome::ControlDelivery { request_id:request_id.clone(), operation:operation.clone(), status:status.clone(), error:error.clone() });
                self.send(doc,emit,"core.runtime.control.delivery",json!({"request_id":request_id,"operation":operation,"status":status,"error":error}));
            }
            AgentEvent::UsageDetail { usage, scope, usage_id } => {
                if scope == "context_window" {
                    if self.routed_agent.is_none()
                        && self.turn(doc) == current_turn(doc)
                        && let Some(zones) = usage.get("zones").filter(|v| v.is_array())
                    {
                        if !doc["context_window"].is_object() {
                            doc["context_window"] = json!({"total":Value::Null,"limit":Value::Null,"zones":[],"compacted":[],"compact_status":"idle"});
                        }
                        doc["context_window"]["zones"] = zones.clone();
                        doc["context_window"]["updated_at"] = json!(now());
                    }
                } else {
                    self.send(doc,emit,"core.usage.updated",json!({"usage":usage,"usage_scope":scope,"usage_id":usage_id,"runtime":self.runtime()}));
                }
            }
            AgentEvent::ContinuationStarted { origin_turn_id, turn_id } => {
                if self.native_turn(doc,&turn_id).is_none() && let Some(origin)=self.native_turn(doc,&origin_turn_id) {
                    let source=doc["turns"].as_array().into_iter().flatten().find(|row|row["turn_id"]==origin).cloned().unwrap_or(Value::Null);
                    let epoch=doc["native_session"]["epoch"].clone();
                    let id=uuid::Uuid::new_v4().to_string();let seq=doc["turns"].as_array().map_or(1,|rows|rows.len()+1);
                    doc["turns"].as_array_mut().unwrap().push(json!({"turn_id":id,"seq":seq,"kind":"native_continuation","status":"running","runtime_turn_id":turn_id,"agent_session_id":self.identity.agent_session_id,"runtime_snapshot":source["runtime_snapshot"],"origin_turn_id":origin,"runtime_epoch":epoch,"created_at":now()}));
                    self.native_origins.insert(turn_id.clone(),id.clone());
                    if doc["thread"]["state"]["running_turn_id"].is_null(){self.adopt_idle_items(doc,&id);}else{self.idle_items.clear();}
                    if doc["thread"]["state"]["running_turn_id"].is_null(){state(doc)["running_turn_id"]=json!(id);state(doc)["status"]=json!("running");}
                    self.send(doc,emit,"core.turn.started",json!({"turn_id":id,"kind":"native_continuation","origin_turn_id":origin}));
                }
            }
            AgentEvent::Origin { turn_id, agent_id, event } => {
                let previous_turn = self.routed_turn.clone();
                let previous_agent = self.routed_agent.clone();
                let route=self.native_turn(doc,&turn_id).or(previous_turn.clone());
                let starting=matches!(event.as_ref(),AgentEvent::TurnStarted{..})&&doc["turns"].as_array().into_iter().flatten().find(|row|row["turn_id"]==current_turn(doc)).is_some_and(|row|row["runtime_turn_id"].is_null());
                if route.is_none()&&!turn_id.is_empty()&&!starting{return;}
                self.routed_turn=route;
                self.routed_agent = agent_id;
                self.apply(doc, *event, emit, effects);
                self.routed_turn = previous_turn;
                self.routed_agent = previous_agent;
            }
            AgentEvent::BackgroundTasksChanged { count } => {
                state(doc)["background_tasks"] = json!(count);
                doc["native_session"]["background_tasks"] = json!(count);
                if count == 0 {
                    state(doc)["background_task_list"] = json!([]);
                    doc["native_session"]["background_task_list"] = json!([]);
                }
                self.send(doc, emit, "core.session.background.updated", json!({"count":count}));
            }
            AgentEvent::BackgroundTaskList { tasks } => {
                let count = tasks.len();
                state(doc)["background_tasks"] = json!(count);
                state(doc)["background_task_list"] = json!(tasks);
                doc["native_session"]["background_tasks"] = json!(count);
                doc["native_session"]["background_task_list"] = json!(tasks);
                self.send(doc, emit, "core.session.background.updated", json!({"count": count, "tasks": tasks}));
            }
            AgentEvent::SessionStarted {
                provider_session_id,
                resume,
                model,
            } => {
                self.resume_available = !resume.0.is_null();
                let id = &self.identity;
                if !doc["native_session"].is_object() {
                    doc["native_session"] = json!({"started_at": now()});
                }
                for (key, value) in [
                    ("agent_session_id", json!(id.agent_session_id)),
                    ("engine", json!(id.engine)),
                    ("adapter_id", json!(id.adapter_id)),
                    ("instance_id", json!(id.instance_id)),
                    ("provider_session_id", json!(provider_session_id)),
                    ("resume", resume.0.clone()),
                    ("model", json!(model)),
                    ("cwd", json!(id.cwd)),
                    ("established_at", json!(now())),
                ] {
                    doc["native_session"][key] = value;
                }
                doc["agent_session"] = json!({"agent_session_id":id.agent_session_id,"external_session_id":provider_session_id,"runtime_instance_id":id.instance_id});
                state(doc)["agent_session_id"] = json!(id.agent_session_id);
                if !id.resumed && state(doc)["scheduled_tasks"].as_array().is_some_and(|tasks| !tasks.is_empty()) {
                    state(doc)["scheduled_tasks"] = json!([]);
                    self.send(doc, emit, "core.session.schedules.updated", json!({"scheduled_tasks":[]}));
                }
                let id = &self.identity;
                let kind = if id.resumed { "core.session.resumed" } else { "core.session.started" };
                let payload = json!({"transport":id.transport,"adapter_id":id.adapter_id,"instance_id":id.instance_id,"cwd":id.cwd,"model":model,"runtime":self.runtime(),"provider_session_id":provider_session_id});
                self.send(doc, emit, kind, payload);
            }
            AgentEvent::ServedModel { model, reason } => {
                doc["native_session"]["served_model"] = json!(model);
                if reason.is_some() {
                    self.send(doc, emit, "core.runtime.warning", json!({"kind":"model_rerouted","message":reason,"model":model}));
                }
            }
            AgentEvent::ModelFallbackDetected { expected, actual, .. } => {
                self.send(doc, emit, "core.runtime.warning", json!({"kind":"model_fallback","message":format!("{expected} → {actual}"),"expected":expected,"actual":actual}));
            }
            AgentEvent::TurnBlocked { model, detail, .. } => {
                if self.routed_agent.is_none()&&self.turn(doc)==current_turn(doc){self.turn_error=Some(detail.clone());}
                self.send(doc, emit, "core.runtime.warning", json!({"kind":"turn_blocked","message":detail,"model":model}));
            }
            AgentEvent::TurnFailed { code, message } => {
                self.runtime_error(doc, emit, &code, &message);
            }
            AgentEvent::TurnStarted { turn_id } => {
                if self.routed_agent.is_some() {
                    self.native_origins.insert(turn_id,self.turn(doc));
                } else {
                    if doc["thread"]["state"]["running_turn_id"].is_null() && self.native_turn(doc,&turn_id).is_none() {
                        let origin=doc["turns"].as_array().into_iter().flatten().rev().find_map(|row|row["runtime_turn_id"].as_str()).map(str::to_owned);
                        if let Some(origin)=origin {self.apply(doc,AgentEvent::ContinuationStarted{origin_turn_id:origin,turn_id:turn_id.clone()},emit,effects);}
                    }
                    let turn=self.native_turn(doc,&turn_id).unwrap_or_else(||current_turn(doc));
                    if let Some(row) = turn_row(doc, &turn) { row["runtime_turn_id"] = json!(turn_id); }
                    let foreground=turn==current_turn(doc);self.native_origins.insert(turn_id.clone(),turn);
                    if foreground{self.runtime_turn_id=Some(turn_id);self.turn_started=Some(Instant::now());}
                    self.idle_items.clear();
                }
            }
            AgentEvent::TurnAccepted { delivery_id } => {
                effects.outcomes.push(Outcome::TurnAccepted { delivery_id });
            }
            AgentEvent::McpServersNotAttached => {
                doc["native_session"]["mcp_attached"] = json!(false);
                self.send(doc, emit, "core.runtime.warning", json!({"kind":"mcp_not_attached","message":"该 Runtime 未接受 MCP 服务器，网关工具与会话指令不会送达模型"}));
            }
            AgentEvent::TurnCompleted { turn_id, status, usage } => {
                if self.routed_agent.is_none()&&self.trailing_turn_end.as_deref()==Some(turn_id.as_str()) {
                    self.trailing_turn_end=None;
                    if let Some(usage) = &usage {
                        self.context(doc, usage);
                    }
                } else {
                    let origin=if turn_id.is_empty(){self.turn(doc)}else if let Some(origin)=self.native_turn(doc,&turn_id){origin}else{return;};
                    if self.routed_agent.is_none()&&doc["thread"]["state"]["running_turn_id"] == origin.as_str() {
                        self.end_turn(doc, emit, effects, status, usage);
                    } else {
                        if let Some(row)=turn_row(doc,&origin) && row["kind"]=="native_continuation"{row["status"]=snake(&status);row["ended_at"]=json!(now());}
                        if let Some(usage) = usage {
                            if origin==current_turn(doc){self.context(doc, &usage);}
                            self.send(doc, emit, "core.usage.updated", json!({"turn_id":origin,"usage":usage_json(&usage),"usage_scope":"background","usage_id":format!("{turn_id}:background")}));
                        }
                        self.send(doc, emit, "core.session.background.completed", json!({"turn_id":origin,"runtime_turn_id":turn_id,"status":snake(&status)}));
                    }
                }
            }
            AgentEvent::ItemStarted(item) => self.item(doc, emit, item, false),
            AgentEvent::ItemUpdated(item) => self.item(doc, emit, item, false),
            AgentEvent::ItemCompleted(item) => self.item(doc, emit, item, true),
            AgentEvent::Delta { item_id, kind, text } => self.delta(doc, emit, item_id, kind, text),
            AgentEvent::ApprovalRequested(request) => self.approval(doc, emit, effects, request),
            AgentEvent::ApprovalResolved { request_id, decision } => {
                self.approval_resolved(doc, emit, &request_id, &decision)
            }
            AgentEvent::UserInputRequested {
                request_id,
                questions,
                delivery,
            } => self.user_input(doc, emit, request_id, &questions, delivery),
            AgentEvent::UserInputResolved { request_id, answers } => {
                if doc["thread"]["state"]["pending_user_input"]["request_id"] == request_id.as_str() {
                    state(doc)["pending_user_input"] = Value::Null;
                    refresh_plan_phase(doc);
                    let cancelled = answers.is_empty() || answers.get("$muteki_action").is_some();
                    self.send(doc, emit, "core.user_input.resolved", json!({"request_id":request_id,"answers":answers,"cancelled":cancelled}));
                }
            }
            AgentEvent::TokenUsage(usage) => {
                self.context(doc, &usage);
                let payload = json!({"usage":usage_json(&usage),"usage_scope":"context","usage_native_type":snake(&usage.freshness),"runtime":self.runtime(),"usage_id":format!("{}:context",current_turn(doc))});
                self.send(doc, emit, "core.usage.updated", payload);
            }
            AgentEvent::ContextCompacted(compaction) => self.compaction(doc, emit, effects, &compaction),
            AgentEvent::PlanUpdated {
                explanation, steps, ..
            } => self.plan(doc, emit, explanation, &steps),
            AgentEvent::GoalUpdated { goal } => {
                state(doc)["goal"] = snake(&goal);
            }
            AgentEvent::TurnCheckpoint { turn_id, checkpoint_id } => {
                let target = doc["turns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|row| row["runtime_turn_id"] == turn_id.as_str())
                    .and_then(|row| row["turn_id"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| current_turn(doc));
                if let Some(row) = turn_row(doc, &target) {
                    row["native_checkpoint_id"] = json!(checkpoint_id);
                }
            }
            AgentEvent::RewindCompleted { checkpoint_id, mode, prefill, later_history, files, incomplete, undo_token, .. } => {
                effects.outcomes.push(Outcome::Rewind { checkpoint_id, result: Ok(RewindCompletion { mode, prefill, later_history, files, incomplete, undo_token }) });
            }
            AgentEvent::RewindFailed { checkpoint_id, failure, .. } => {
                effects.outcomes.push(Outcome::Rewind { checkpoint_id, result: Err(failure) });
            }
            AgentEvent::RewindPreviewed { .. }
            | AgentEvent::RewindUndone { .. }
            | AgentEvent::RewindUndoFailed { .. } => {}
            AgentEvent::UnreadableRecord { line } => {
                self.send(doc, emit, "core.runtime.unreadable_record", json!({"line": line}));
            }
            AgentEvent::SteerAccepted { request_id } => self.steer_accepted(doc, emit, &request_id),
            AgentEvent::SteerRejected { request_id } => {
                let steer = doc["pending_steers"]
                    .as_object_mut()
                    .and_then(|rows| rows.remove(&request_id))
                    .unwrap_or(Value::Null);
                if !steer.is_null() {
                    effects.outcomes.push(Outcome::SteerRejected { request_id, steer });
                }
            }
            AgentEvent::TurnChangesUpdated { turn_id, changes, completeness } => {
                let target = doc["turns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|row| row["runtime_turn_id"] == turn_id.as_str())
                    .and_then(|row| row["turn_id"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| current_turn(doc));
                let changes_json = json!({"completeness":snake(&completeness),"files":changes.iter().map(|c| json!({"path":c.path,"kind":snake(&c.kind)})).collect::<Vec<_>>()});
                if let Some(row) = turn_row(doc, &target) {
                    row["native_changes"] = changes_json.clone();
                }
                let mut payload = changes_json;
                payload["turn_id"] = json!(target);
                emit(doc, "core.turn.changes_updated", payload);
            }
            AgentEvent::ProviderOptions { descriptors, selections } => {
                doc["native_session"]["options"] = json!({"descriptors":snake(&descriptors),"selections":snake(&selections)});
            }
            AgentEvent::ProviderCommands { commands } => {
                if doc["native_session"]["commands"]!=snake(&commands){let revision=doc["native_session"]["capability_revision"].as_u64().unwrap_or(0)+1;doc["native_session"]["capability_revision"]=json!(revision);}
                doc["native_session"]["commands"] = snake(&commands);
            }
            AgentEvent::Warning { message } => {
                self.send(doc, emit, "core.runtime.warning", json!({"kind":"warning","message":message}));
            }
            AgentEvent::Error { message, fatal } => {
                self.runtime_error(doc, emit, if fatal { "runtime.fatal" } else { "runtime.provider_error" }, &message);
            }
            AgentEvent::ProviderStartFailed { error } => {
                self.runtime_error(doc, emit, "runtime.start_failed", &error);
            }
            AgentEvent::UsageLimitReached { resets_at } => {
                let turn = current_turn(doc);
                state(doc)["quota_hold"] = json!({"turn_id":turn,"resets_at":resets_at,"kind":"usage_limit","handled":false});
                self.send(doc, emit, "core.runtime.warning", json!({"kind":"rate_limit","message":"已达到用量上限","rate_limit":{"limited":true,"resets_at":resets_at,"kind":"usage_limit"}}));
            }
            AgentEvent::TurnRetry { retry } => {
                let turn = current_turn(doc);
                if let Some(retry) = retry {
                    if let Some(row) = turn_row(doc, &turn) {
                        row["native_retry"] = json!({"runtime_turn_id":retry.turn_id,"retry_at":retry.retry_at});
                    }
                    self.send(doc, emit, "core.turn.retry_scheduled", json!({"turn_id":turn,"runtime_turn_id":retry.turn_id,"retry_at":retry.retry_at}));
                } else {
                    self.send(doc, emit, "core.turn.retry_scheduled", json!({"turn_id":turn,"runtime_turn_id":Value::Null,"retry_at":Value::Null}));
                }
                expire_pending(doc, EXPIRED_AT_TURN_END, true);
            }
            AgentEvent::SessionClosed { reason } => {
                expire_pending(doc, EXPIRED_AT_SESSION_CLOSE, false);
                if unconfirm_schedules(doc, "session_closed") {
                    let tasks = doc["thread"]["state"]["scheduled_tasks"].clone();
                    self.send(doc, emit, "core.session.schedules.updated", json!({"scheduled_tasks":tasks}));
                }
                let resume_available = self.resume_available;
                self.send(doc, emit, "core.session.closed", json!({"reason":reason,"resume_available":resume_available}));
                effects.outcomes.push(Outcome::SessionClosed { reason, resume_available });
            }
            AgentEvent::SteerRequested { .. }
            | AgentEvent::ProviderRelay { .. }
            | AgentEvent::PlanResolved { .. }
            | AgentEvent::ContextWindowChanged { .. }
            | AgentEvent::ControlAction(_)
            | AgentEvent::ProposedPlanDelta { .. }
            | AgentEvent::ProposedPlan { .. } => {}
        }
    }

    fn runtime_error(&mut self, doc: &mut Value, emit: &mut Sink, code: &str, message: &str) {
        let running=self.routed_agent.is_none()&&self.turn(doc)==current_turn(doc)&&!doc["thread"]["state"]["running_turn_id"].is_null();
        if running{self.turn_error=Some(message.to_owned());}
        if running {
            state(doc)["last_error"] = json!({"message":message,"code":code});
        }
        self.send(doc, emit, "core.runtime.error", json!({"code":code,"category":"provider","message":message,"retryable":false}));
    }

    fn end_turn(
        &mut self,
        doc: &mut Value,
        emit: &mut Sink,
        effects: &mut Effects,
        status: TurnStatus,
        usage: Option<TokenUsage>,
    ) {
        let turn = current_turn(doc);
        let missing_tools = self.items.values().any(|item|
            item.open && item.parent.is_none() && item.turn == turn && item.role == Role::Tool && item.advanced);
        let status = if status == TurnStatus::Completed && missing_tools
            && doc["native_session"]["background_tasks"].as_u64().unwrap_or(0) == 0
        {
            self.runtime_error(doc, emit, "runtime.tool_completion_missing",
                "Provider 已结束本轮，但仍有工具未回报完成，无法确认执行结果");
            TurnStatus::Failed
        } else { status };
        let usage = usage.map(|usage| {
            self.context(doc, &usage);
            let value = usage_json(&usage);
            let payload = json!({"usage":value,"usage_scope":"turn","usage_native_type":snake(&usage.freshness),"runtime":self.runtime(),"usage_id":turn});
            self.send(doc, emit, "core.usage.updated", payload);
            value
        });
        let (status, tool_status) = match status {
            TurnStatus::Completed => ("completed", "completed"),
            TurnStatus::Failed => ("failed", "failed"),
            TurnStatus::Interrupted => ("interrupted", "cancelled"),
        };
        let open: Vec<String> = self
            .items
            .iter()
            .filter(|(_, item)| item.open && item.parent.is_none() && item.turn == turn && !(item.role==Role::Tool&&doc["native_session"]["background_tasks"].as_u64().unwrap_or(0)>0&&status=="completed"))
            .map(|(id, _)| id.clone())
            .collect();
        for id in open {
            let item = self.items.get_mut(&id).unwrap();
            item.open = false;
            item.ended = Some(Instant::now());
            if item.role == Role::Tool {
                if let Some(row) = doc["tools"]
                    .as_array_mut()
                    .and_then(|rows| rows.iter_mut().find(|row| row["tool_id"] == id && row["turn_id"] == turn))
                {
                    row["status"] = json!(if item.advanced { tool_status } else { "cancelled" });
                    row["output_complete"] = json!(false);
                }
            }
        }
        expire_pending(doc, EXPIRED_AT_TURN_END, true);
        let error = self.turn_error.take();
        let error = (status == "failed")
            .then(|| error.unwrap_or_else(|| "Runtime 报告本轮失败".to_owned()));
        self.runtime_turn_id = None;
        let mut spans: Vec<(Instant, Instant)> = self.items.values()
            .filter(|item| item.role == Role::Tool && item.parent.is_none() && item.turn == turn)
            .map(|item| (item.started, item.ended.unwrap_or_else(Instant::now)))
            .collect();
        spans.sort();
        let mut tool_ms = 0u64;
        let mut merged: Vec<(Instant, Instant)> = Vec::new();
        for span in spans {
            if let Some(last) = merged.last_mut() && span.0 <= last.1 {
                if span.1 > last.1 { last.1 = span.1; }
                continue;
            }
            merged.push(span);
        }
        for (start, end) in merged {
            tool_ms = tool_ms.saturating_add(end.duration_since(start).as_millis() as u64);
        }
        let wall_ms = self.turn_started.take().map(|start| start.elapsed().as_millis() as u64);
        self.send(doc, emit, "core.turn.timing", json!({"turn_id":turn,"tool_ms":tool_ms,"wall_ms":wall_ms}));
        effects.outcomes.push(Outcome::TurnEnded { turn_id: turn, status, usage, error });
    }

    fn context(&self, doc: &mut Value, usage: &TokenUsage) {
        if self.routed_agent.is_some()||self.turn(doc)!=current_turn(doc){return;}
        if usage.used_tokens.is_none() && usage.context_window.is_none() {
            return;
        }
        let previous = doc["context_window"].clone();
        doc["context_window"] = json!({
            "total": usage.used_tokens.map(Value::from).unwrap_or_else(|| previous["total"].clone()),
            "limit": usage.context_window.map(Value::from).unwrap_or_else(|| previous["limit"].clone()),
            "zones": previous.get("zones").cloned().unwrap_or_else(|| json!([])),
            "compacted": previous.get("compacted").cloned().unwrap_or_else(|| json!([])),
            "compact_status": previous.get("compact_status").cloned().unwrap_or_else(|| json!("idle")),
            "updated_at": now(),
            "source": "usage_event",
        });
    }

    fn compaction(&mut self, doc: &mut Value, emit: &mut Sink, effects: &mut Effects, compaction: &Compaction) {
        let status = if compaction.in_progress { "started" } else {
            match compaction.status.as_deref() {
                Some("failed") => "failed",
                Some("interrupted") => "interrupted",
                _ => "completed",
            }
        };
        let trigger = if compaction.trigger.as_deref() == Some("auto") { "auto" } else { "manual" };
        let turn = self.turn(doc);
        if self.routed_agent.is_some()||turn!=current_turn(doc){self.send(doc,emit,"core.context.compaction",json!({"status":status,"trigger":trigger,"confirmed":true,"tokens_before":compaction.pre_tokens,"tokens_after":compaction.post_tokens,"turn_id":turn}));return;}
        if !doc["context_window"].is_object() {
            doc["context_window"] = json!({"total":Value::Null,"limit":Value::Null,"zones":[],"compacted":[],"compact_status":"idle"});
        }
        let window = &mut doc["context_window"];
        window["compact_status"] = json!(match status {
            "started" => "running", "completed" => "done", other => other,
        });
        let stamp = now();
        window["updated_at"] = json!(stamp);
        window["source"] = json!("compaction");
        if !compaction.in_progress {
            if !window["compacted"].is_array() {
                window["compacted"] = json!([]);
            }
            let mut record = json!({"turn_id":turn,"status":status,"trigger":trigger,"confirmed":true,"occurred_at":stamp});
            if let Some(before) = compaction.pre_tokens { record["tokens_before"] = json!(before); }
            if let Some(after) = compaction.post_tokens { record["tokens_after"] = json!(after); }
            if let Some(summary) = &compaction.summary { record["summary"] = json!(summary); }
            if let Some(message) = &compaction.message { record["message"] = json!(message); }
            window["compacted"].as_array_mut().unwrap().push(record);
            if status == "completed" {
                window["total"] = json!(compaction.post_tokens);
            }
        }
        let mut payload = json!({"status":status,"trigger":trigger,"confirmed":true,"tokens_before":compaction.pre_tokens,"tokens_after":compaction.post_tokens,"duration_ms":compaction.duration_ms});
        if let Some(summary) = &compaction.summary { payload["summary"] = json!(summary); }
        if let Some(message) = &compaction.message { payload["message"] = json!(message); }
        self.send(doc, emit, "core.context.compaction", payload);
        let operation = doc["thread"]["state"]["running_turn_id"] == turn.as_str()
            && turn_row(doc, &turn).is_some_and(|row| row["operation"] == "compact");
        if !compaction.in_progress && operation {
            self.trailing_turn_end = self.runtime_turn_id.clone();
            if status == "failed" {
                self.turn_error = compaction.message.clone();
            }
            self.end_turn(doc, emit, effects, match status {
                "failed" => TurnStatus::Failed,
                "interrupted" => TurnStatus::Interrupted,
                _ => TurnStatus::Completed,
            }, None);
        }
    }

    fn plan(&self, doc: &mut Value, emit: &mut Sink, explanation: Option<String>, steps: &[PlanStep]) {
        let turn = current_turn(doc);
        let tasks: Vec<Value> = steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let status = match step.status {
                    PlanStepStatus::Pending => "pending",
                    PlanStepStatus::InProgress => "in_progress",
                    PlanStepStatus::Completed => "completed",
                };
                json!({"task_id":format!("step-{}", index + 1),"title":step.step,"status":status})
            })
            .collect();
        let revision = doc["thread"]["state"]["plan"]["revision"].as_u64().unwrap_or(0) + 1;
        state(doc)["plan"] = json!({
            "revision": revision, "phase": "proposed", "source": "adapter",
            "adapter_id": self.identity.adapter_id, "agent_session_id": self.identity.agent_session_id,
            "turn_id": turn, "tasks": tasks, "awaiting": Value::Null,
            "last_change_summary": format!("更新 {} 个计划步骤", tasks.len()),
            "markdown": explanation, "updated_at": now(),
        });
        refresh_plan_phase(doc);
        let plan = doc["thread"]["state"]["plan"].clone();
        self.send(doc, emit, "core.plan.updated", json!({"tasks":tasks,"explanation":explanation,"patch":Value::Null,"source":"adapter","plan":plan}));
    }

    fn message_id(turn: &str, item: &str) -> String {
        format!("{turn}:{item}")
    }

    fn note_item(&mut self, doc: &Value, id: &str, parent: Option<&str>) {
        if self.items.contains_key(id) {
            return;
        }
        if doc["thread"]["state"]["running_turn_id"].as_str().is_some_and(|turn| !turn.is_empty()) {
            self.idle_items.clear();
        } else if parent.is_none() && self.routed_turn.is_none() && self.routed_agent.is_none() {
            self.idle_items.push(id.to_owned());
        }
    }

    /// Without this, idle output lands in the last finished turn and reads as
    /// that turn's final reply.
    fn adopt_idle_items(&mut self, doc: &mut Value, turn: &str) {
        for id in std::mem::take(&mut self.idle_items) {
            let Some(item) = self.items.get_mut(&id) else { continue };
            let previous = std::mem::replace(&mut item.turn, turn.to_owned());
            let message_id = item.message_id.get_or_insert_with(|| Self::message_id(&previous, &id)).clone();
            for row in doc["messages"].as_array_mut().into_iter().flatten().filter(|row| row["message_id"] == message_id.as_str()) {
                row["turn_id"] = json!(turn);
            }
            for row in doc["tools"].as_array_mut().into_iter().flatten().filter(|row| row["tool_id"] == id.as_str() && row["turn_id"] == previous.as_str()) {
                row["turn_id"] = json!(turn);
            }
        }
    }

    fn upsert_message(doc: &mut Value, turn: &str, message_id: &str, item: &str, text: &str, status: &str) {
        if !doc["messages"].is_array() {
            doc["messages"] = json!([]);
        }
        let seq = crate::chat::stream_head(doc) + 1;
        let rows = doc["messages"].as_array_mut().unwrap();
        match rows.iter_mut().find(|row| row["message_id"] == message_id) {
            Some(row) => {
                row["text"] = json!(text);
                row["status"] = json!(status);
            }
            None => rows.push(json!({"message_id":message_id,"turn_id":turn,"role":"assistant","text":text,"stream_seq":seq,"status":status,"native_item_id":item,"created_at":now()})),
        }
    }

    fn item(&mut self, doc: &mut Value, emit: &mut Sink, item: ThreadItem, completed: bool) {
        if let ItemContent::Subagent { agent_type, description, status, summary, model, effort } = &item.content {
            let patch = json!({
                "agent_id": item.id, "call_id": item.id, "parent_id": item.parent_item_id,
                "origin": "provider_native", "is_agent": true, "title": agent_type,
                "request": description, "status": tool_status(*status),
                "result": summary, "model": model, "effort": effort,
                "turn_id": self.item_turn(doc, &item.id, item.parent_item_id.as_deref()),
            });
            let payload = json!({"source":"runtime", "adapter_id":self.identity.adapter_id, "patch":true, "agents":[patch]});
            match crate::chat_core::inject_agents(doc, &payload, &now()) {
                Ok(events) => for (kind, body) in events { self.send(doc, emit, kind, body); },
                Err((code, message)) => self.runtime_error(doc, emit, code, message),
            }
        }
        match &item.content {
            ItemContent::UserMessage { .. } => {}
            ItemContent::AssistantMessage { text } => {
                self.text(doc, emit, &item.id, item.parent_item_id.clone(), Role::Assistant, text, false, completed)
            }
            ItemContent::Reasoning { text } => {
                self.text(doc, emit, &item.id, item.parent_item_id.clone(), Role::Reasoning, text, false, completed)
            }
            content => {
                if let Some(view) = tool_view(content) {
                    self.tool(doc, emit, &item.id, item.parent_item_id.clone(), view, completed);
                }
            }
        }
    }

    fn delta(&mut self, doc: &mut Value, emit: &mut Sink, item_id: String, kind: DeltaKind, text: String) {
        match kind {
            DeltaKind::AssistantText => {
                let parent = self.items.get(&item_id).and_then(|item| item.parent.clone());
                self.text(doc, emit, &item_id, parent, Role::Assistant, &text, true, false)
            }
            DeltaKind::ReasoningText => {
                let parent = self.items.get(&item_id).and_then(|item| item.parent.clone());
                self.text(doc, emit, &item_id, parent, Role::Reasoning, &text, true, false)
            }
            DeltaKind::CommandOutput => {
                if !self.items.contains_key(&item_id) {
                    let view = tool_view(&ItemContent::CommandExecution {
                        command: String::new(),
                        output: String::new(),
                        exit_code: None,
                        status: ItemStatus::InProgress,
                    })
                    .unwrap();
                    self.tool(doc, emit, &item_id, None, view, false);
                }
                let item = self.items.get_mut(&item_id).unwrap();
                if !item.open || text.is_empty() {
                    return;
                }
                item.text.push_str(&text);
                let (parent, name, output, turn) = (item.parent.clone(), item.name.clone(), item.text.clone(), item.turn.clone());
                Self::upsert_tool(doc, &turn, &item_id, &name, "running", &output);
                let mut payload = json!({"turn_id":turn,"tool_call_id":item_id,"call_id":item_id,"name":name,"tool":name,"chunk":text,"status":"running"});
                if let Some(parent) = parent {
                    payload["agent_id"] = json!(parent);
                    payload["parent_tool_use_id"] = json!(parent);
                }
                self.send(doc, emit, "core.tool.progress", payload);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        doc: &mut Value,
        emit: &mut Sink,
        item_id: &str,
        parent: Option<String>,
        role: Role,
        text: &str,
        is_delta: bool,
        completed: bool,
    ) {
        if parent.is_some() || self.routed_agent.is_some() {
            if role == Role::Reasoning { return; }
            let agent=self.routed_agent.clone().or(parent).unwrap();
            let id=agent.strip_prefix("agent:").unwrap_or(&agent);
            let previous=doc["thread"]["state"]["agents"]["agents"].as_array().into_iter().flatten().find(|row|row["agent_id"]==id).cloned().unwrap_or(Value::Null);
            let activity=if is_delta{format!("{}{}",previous["activity"].as_str().unwrap_or(""),text)}else{text.to_owned()};
            let patch=json!({"agent_id":id,"turn_id":self.item_turn(doc,item_id,Some(&agent)),"activity":activity,"message_id":item_id});
            if let Ok(events)=crate::chat_core::inject_agents(doc,&json!({"source":"runtime","adapter_id":self.identity.adapter_id,"patch":true,"agents":[patch]}),&now()){for(kind,payload)in events{self.send(doc,emit,kind,payload);}}
            return;
        }
        self.note_item(doc, item_id, parent.as_deref());
        let turn = self.item_turn(doc, item_id, parent.as_deref());
        if matches!(role, Role::Assistant | Role::Reasoning) && !text.is_empty() {
            if let Some(row) = doc["turns"].as_array_mut().and_then(|rows| rows.iter_mut().find(|row| row["turn_id"] == turn)) {
                if row["first_token_at"].is_null() { row["first_token_at"] = json!(now()); }
            }
        }
        let entry = self.items.entry(item_id.to_owned()).or_insert_with(|| Item {
            turn: turn.clone(),
            role,
            text: String::new(),
            parent: None,
            started: Instant::now(),
            ended: None,
            open: true,
            advanced: false,
            name: String::new(),
            message_id: None,
        });
        if !entry.open {
            return;
        }
        let change = if is_delta {
            (!text.is_empty()).then(|| Growth::Append(text.to_owned()))
        } else {
            growth(&entry.text, text)
        };
        match &change {
            Some(Growth::Append(suffix)) => entry.text.push_str(suffix),
            Some(Growth::Replace(full)) => entry.text = full.clone(),
            None => {}
        }
        if completed {
            entry.open = false;
            entry.ended = Some(Instant::now());
        }
        let full = entry.text.clone();
        let elapsed = entry.started.elapsed().as_millis() as u64;
        let turn = entry.turn.clone();
        let stable_message_id = entry.message_id.clone();
        match role {
            Role::Assistant => {
                let message_id = stable_message_id.unwrap_or_else(|| Self::message_id(&turn, item_id));
                if !full.is_empty() {
                    Self::upsert_message(doc, &turn, &message_id, item_id, &full, if completed { "completed" } else { "streaming" });
                }
                match change {
                    Some(Growth::Append(chunk)) => self.send(doc, emit, "core.message.delta", json!({"turn_id":turn,"role":"assistant","text":chunk,"message_id":message_id,"item_id":item_id})),
                    Some(Growth::Replace(full)) => self.send(doc, emit, "core.message.delta", json!({"turn_id":turn,"role":"assistant","text":full,"message_id":message_id,"item_id":item_id,"replace":true})),
                    None => {}
                }
                if completed && !full.trim().is_empty() {
                    self.send(doc, emit, "core.message.completed", json!({"turn_id":turn,"role":"assistant","text":full,"message_id":message_id,"item_id":item_id}));
                }
            }
            Role::Reasoning => {
                if completed {
                    if !full.is_empty() {
                        self.send(doc, emit, "core.reasoning.summary", json!({"turn_id":turn,"text":full,"reasoning_summary":full,"channel":"thinking","partial":false,"item_id":item_id,"duration_ms":elapsed}));
                    }
                } else {
                    match change {
                        Some(Growth::Append(chunk)) => self.send(doc, emit, "core.reasoning.summary", json!({"turn_id":turn,"text":chunk,"reasoning_summary":chunk,"channel":"thinking","partial":true,"item_id":item_id})),
                        Some(Growth::Replace(full)) => self.send(doc, emit, "core.reasoning.summary", json!({"turn_id":turn,"text":full,"reasoning_summary":full,"channel":"thinking","partial":false,"item_id":item_id})),
                        None => {}
                    }
                }
            }
            Role::Tool => {}
        }
    }

    fn upsert_tool(doc: &mut Value, turn: &str, id: &str, name: &str, status: &str, output: &str) {
        if !doc["tools"].is_array() {
            doc["tools"] = json!([]);
        }
        let seq = crate::chat::stream_head(doc) + 1;
        let complete = output.chars().count() <= TOOL_OUTPUT_SUMMARY_CHARS;
        let summary = capped(output, TOOL_OUTPUT_SUMMARY_CHARS);
        let rows = doc["tools"].as_array_mut().unwrap();
        match rows.iter_mut().find(|row| row["tool_id"] == id && row["turn_id"] == turn) {
            Some(row) => {
                row["name"] = json!(name);
                row["status"] = json!(status);
                row["output_summary"] = json!(summary);
                row["output_complete"] = json!(complete);
            }
            None => rows.push(json!({"tool_id":id,"turn_id":turn,"name":name,"status":status,"output_summary":summary,"output_complete":complete,"stream_seq":seq})),
        }
    }

    /// Native agents can arm their own wakeups (Grok `scheduler_*`, Claude
    /// `Cron*`). Fold what their typed tool results report into the thread
    /// state so the conversation shows what will fire next.
    fn track_schedule(&mut self, doc: &mut Value, emit: &mut Sink, turn: &str, name: &str, input: &Value, output: &str) {
        let result: Value = serde_json::from_str(output).unwrap_or(Value::Null);
        let variant = input["variant"].as_str().or(result["type"].as_str()).unwrap_or(name);
        let field = |value: &Value, keys: &[&str]| keys.iter().find_map(|key| value[*key].as_str().filter(|s| !s.is_empty()).map(str::to_owned));
        let previous = doc["thread"]["state"]["scheduled_tasks"].as_array().cloned().unwrap_or_default();
        let mut tasks = previous.clone();
        match variant {
            "SchedulerCreate" | "CronCreate" => {
                let Some(id) = field(&result, &["id", "task_id"]) else { return };
                let existing = tasks.iter().find(|task| task["task_id"] == id.as_str()).cloned().unwrap_or(Value::Null);
                tasks.retain(|task| task["task_id"] != id.as_str());
                tasks.push(json!({
                    "task_id": id,
                    "schedule": field(&result, &["humanSchedule", "schedule"]).or_else(|| field(input, &["interval", "cron", "schedule"])),
                    "recurring": result.get("recurring").or(input.get("recurring")).cloned().unwrap_or(Value::Null),
                    "prompt": field(input, &["prompt"]),
                    "origin_turn_id": existing.get("origin_turn_id").cloned().unwrap_or(json!(turn)),
                    "created_at": existing.get("created_at").cloned().unwrap_or(json!(now())),
                    "updated_at": now(),
                    "engine": self.identity.engine,
                }));
            }
            "SchedulerDelete" | "CronDelete" => {
                if result["success"] == false {
                    return;
                }
                let Some(id) = field(input, &["task_id", "id"]) else { return };
                tasks.retain(|task| task["task_id"] != id.as_str());
            }
            "SchedulerList" | "CronList" => {
                let Some(listed) = result.get("tasks").or(result.get("jobs")).and_then(Value::as_array) else { return };
                tasks = listed.iter().filter_map(|item| {
                    let id = field(item, &["id", "task_id"])?;
                    let mut task = previous.iter().find(|task| task["task_id"] == id.as_str()).cloned().unwrap_or_else(|| json!({"task_id": id, "engine": self.identity.engine}));
                    if let Some(row) = task.as_object_mut() { row.remove("unconfirmed"); }
                    if let Some(schedule) = field(item, &["humanSchedule", "intervalHuman", "schedule", "interval", "cron"]) { task["schedule"] = json!(schedule); }
                    if task["prompt"].is_null() && let Some(prompt) = field(item, &["prompt"]) { task["prompt"] = json!(prompt); }
                    if task["created_at"].is_null() && let Some(created) = field(item, &["createdAt", "created_at"]) { task["created_at"] = json!(created); }
                    if let Some(recurring) = item.get("recurring").filter(|value| value.is_boolean()) { task["recurring"] = recurring.clone(); }
                    Some(task)
                }).collect();
            }
            _ => return,
        }
        if tasks == previous {
            return;
        }
        state(doc)["scheduled_tasks"] = json!(tasks);
        self.send(doc, emit, "core.session.schedules.updated", json!({"scheduled_tasks":tasks}));
    }

    fn tool(&mut self, doc: &mut Value, emit: &mut Sink, id: &str, parent: Option<String>, view: ToolView, completed: bool) {
        self.note_item(doc, id, parent.as_deref());
        let turn = self.item_turn(doc, id, parent.as_deref());
        let fresh = !self.items.contains_key(id);
        let item = self.items.entry(id.to_owned()).or_insert_with(|| Item {
            turn: turn.clone(),
            role: Role::Tool,
            text: String::new(),
            parent: parent.clone(),
            started: Instant::now(),
            ended: None,
            open: true,
            advanced: false,
            name: view.name.clone(),
            message_id: None,
        });
        if !fresh {
            item.advanced = true;
        }
        if !item.open {
            return;
        }
        if view.name != "shell" || item.name.is_empty() {
            item.name = view.name.clone();
        }
        let terminal = completed || view.status != ItemStatus::InProgress;
        let mut base = json!({"turn_id":turn,"tool_call_id":id,"call_id":id,"name":view.name,"tool":view.name,"kind":view.kind,"title":view.title,"input":view.input,"native":view.native,"is_agent":view.is_agent});
        if let Some(parent) = &item.parent {
            base["agent_id"] = json!(parent);
            base["parent_tool_use_id"] = json!(parent);
        }
        if let Some(model) = &view.model {
            base["model"] = json!(model);
        }
        let previous = item.text.clone();
        if let Some(output) = &view.output {
            if !(output.len() < previous.len() && previous.starts_with(output.as_str())) {
                item.text = output.clone();
            }
        }
        let output = item.text.clone();
        let duration = item.started.elapsed().as_millis() as u64;
        if terminal {
            item.open = false;
            item.ended = Some(Instant::now());
        }
        let status = match (terminal, view.status) {
            (false, _) => "running",
            (true, ItemStatus::InProgress) => "completed",
            (true, status) => tool_status(status),
        };
        Self::upsert_tool(doc, &turn, id, &view.name, status, &output);
        if fresh {
            let mut payload = base.clone();
            payload["status"] = json!("running");
            self.send(doc, emit, "core.tool.started", payload);
        }
        if terminal {
            let mut payload = base;
            payload["status"] = json!(status);
            payload["output"] = json!(output);
            payload["exit_code"] = json!(view.exit_code);
            payload["duration_ms"] = json!(duration);
            if status == "failed" {
                payload["is_error"] = json!(true);
            }
            let input = payload["input"].clone();
            self.send(doc, emit, "core.tool.completed", payload);
            if status == "completed" {
                self.track_schedule(doc, emit, &turn, &view.name, &input, &output);
            }
        } else if !fresh || !output.is_empty() {
            match growth(&previous, &output) {
                Some(Growth::Append(chunk)) => {
                    let mut payload = base;
                    payload["status"] = json!("running");
                    payload["chunk"] = json!(chunk);
                    self.send(doc, emit, "core.tool.progress", payload);
                }
                Some(Growth::Replace(full)) => {
                    let mut payload = base;
                    payload["status"] = json!("running");
                    payload["output"] = json!(full);
                    self.send(doc, emit, "core.tool.progress", payload);
                }
                None => {}
            }
        }
    }

    fn approval_row(&self, doc: &Value, request: &ApprovalRequest) -> Value {
        let (kind, title, detail) = match &request.kind {
            ApprovalKind::ExecCommand { command, cwd, reason } => (
                "command_execution",
                "命令执行",
                json!({"command":command,"cwd":cwd,"reason":reason,"tool_name":"shell"}),
            ),
            ApprovalKind::FileChange { changes, reason } => {
                let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
                let diff = changes.iter().filter_map(|change| change.diff.as_deref()).collect::<Vec<_>>().join("\n");
                (
                    "file_change",
                    "文件变更",
                    json!({"path":paths.first(),"paths":paths,"files":files(changes),"diff":diff,"reason":reason,"tool_name":"file_change"}),
                )
            }
            ApprovalKind::FileRead { detail } => ("tool", "Runtime 操作", json!({"reason":detail,"tool_name":"read"})),
            ApprovalKind::ToolUse { name, input, detail } => {
                let (kind, title) = if mcp_server(name).is_some() {
                    ("mcp_tool_call", "MCP 工具调用")
                } else if name == "ExitPlanMode" || input["approval_kind"] == "plan_exit" {
                    ("plan_exit", "计划已就绪")
                } else {
                    ("tool", "Runtime 操作")
                };
                let mut fields = json!({"tool_name":name,"input":input,"reason":detail});
                if kind == "plan_exit" {
                    if let Some(plan) = input["plan"].as_str().or_else(|| input["plan_markdown"].as_str()) {
                        fields["plan_markdown"] = json!(plan);
                    }
                }
                (kind, title, fields)
            }
        };
        let mut options: Vec<Value> = request
            .options
            .iter()
            .map(|option| json!({"option_id":option.id,"label":option.label,"name":option.label,"kind":option_kind(option.kind)}))
            .collect();
        if !request
            .options
            .iter()
            .any(|option| matches!(option.kind, ApprovalOptionKind::RejectOnce | ApprovalOptionKind::RejectAlways))
        {
            options.push(json!({"option_id":CANCEL_OPTION,"label":"Decline","name":"拒绝并停止本轮","kind":"reject_once"}));
        }
        let mut scopes = vec!["once"];
        if request.options.iter().any(|option| option.kind == ApprovalOptionKind::AllowAlways) {
            scopes.push("session");
        }
        let mut row = json!({
            "approval_id": request.id, "approval_kind": kind, "title": title,
            "turn_id":request.turn_id.as_deref().and_then(|native|self.native_turn(doc,native)).unwrap_or_else(||self.item_turn(doc,&request.id,request.parent_item_id.as_deref())), "agent_session_id": self.identity.agent_session_id,
            "tool_call_id":request.tool_call_id, "options": options, "scopes": scopes,
            "status": "pending", "requested_at": now(),
            "response_capability": {"answerable":true,"agent_session_id":self.identity.agent_session_id,"generation":self.identity.generation},
            "native": {"request_id":request.id,"runtime_turn_id":request.turn_id,"parent_item_id":request.parent_item_id},
        });
        for (key, value) in detail.as_object().unwrap() {
            row[key] = value.clone();
        }
        row
    }

    fn approval(&mut self, doc: &mut Value, emit: &mut Sink, effects: &mut Effects, request: ApprovalRequest) {
        let row = self.approval_row(doc, &request);
        let decision = access::decide(&self.access, &request.kind, &request.options);
        if let Some(decision) = decision {
            let option_id = match &decision {
                ApprovalDecision::Option(id) => Some(id.clone()),
                ApprovalDecision::Cancel => None,
            };
            let scope = if request
                .options
                .iter()
                .any(|option| Some(&option.id) == option_id.as_ref() && option.kind == ApprovalOptionKind::AllowAlways)
            {
                "session"
            } else {
                "once"
            };
            let decision_label = if matches!(decision, ApprovalDecision::Cancel) {
                "cancel"
            } else if request.options.iter().any(|option| Some(&option.id) == option_id.as_ref()
                && matches!(option.kind, ApprovalOptionKind::RejectOnce | ApprovalOptionKind::RejectAlways)) {
                "deny"
            } else {
                "allow"
            };
            effects.commands.push(SessionCommand::RespondApproval {
                request_id: request.id.clone(),
                decision,
            });
            self.automatic.insert(request.id.clone());
            let mut payload = row;
            payload["decision"] = json!(decision_label);
            payload["option_id"] = json!(option_id);
            payload["scope"] = json!(scope);
            payload["automatic"] = json!(true);
            payload["access_mode"] = json!(self.access.mode);
            payload["status"] = json!("processing");
            payload["response_capability"]["answerable"] = json!(false);
            if !state(doc)["pending_approvals"].is_object(){state(doc)["pending_approvals"]=json!({});}
            state(doc)["pending_approvals"][&request.id] = payload.clone();
            self.send(doc, emit, "core.approval.delivery_pending", payload);
            return;
        }
        let id = request.id.clone();
        let state = state(doc);
        if !state["pending_approvals"].is_object() {
            state["pending_approvals"] = json!({});
        }
        state["pending_approvals"][&id] = row.clone();
        refresh_pending(doc);
        self.send(doc, emit, "core.approval.requested", row);
    }

    fn approval_resolved(&mut self, doc: &mut Value, emit: &mut Sink, request_id: &str, decision: &ApprovalDecision) {
        let automatic = self.automatic.remove(request_id);
        let Some(row) = state(doc)["pending_approvals"]
            .as_object_mut()
            .and_then(|rows| rows.remove(request_id))
        else {
            return;
        };
        refresh_pending(doc);
        let (decision, option_id, scope) = match decision {
            ApprovalDecision::Cancel => ("cancel", Value::Null, "once"),
            ApprovalDecision::Option(id) => {
                let kind = row["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|option| option["option_id"] == id.as_str())
                    .and_then(|option| option["kind"].as_str())
                    .unwrap_or("");
                (
                    if kind.starts_with("reject") { "deny" } else { "allow" },
                    json!(id),
                    if kind.ends_with("always") { "session" } else { "once" },
                )
            }
        };
        let payload = json!({"approval_id":request_id,"approval_kind":row["approval_kind"],"turn_id":row["turn_id"],"decision":decision,"option_id":option_id,"scope":scope,"status":"resolved","automatic":automatic});
        self.send(doc, emit, "core.approval.resolved", payload);
    }

    fn user_input(
        &mut self,
        doc: &mut Value,
        emit: &mut Sink,
        request_id: String,
        questions: &[UserInputQuestion],
        delivery: UserInputDelivery,
    ) {
        let rows: Vec<Value> = questions.iter().map(|question| {
            let contract = question.contract.as_ref().unwrap_or(&Value::Null);
            let kind = if question.multi_select { "multi_select" } else if question.options.is_empty() { "free_text" } else { "single_select" };
            let options = contract.get("options").cloned().unwrap_or_else(|| json!(question.options.iter().map(|option| json!({"value":option.label,"label":option.label,"description":option.description,"recommended":false})).collect::<Vec<_>>()));
            json!({"question_id":question.id,"kind":contract["kind"].as_str().unwrap_or(kind),
                "header":question.header,"prompt":question.question,
                "required":contract["required"].as_bool().unwrap_or(true),"options":options,
                "allow_free_text":contract["allow_free_text"].as_bool().unwrap_or(true),
                "placeholder":question.prefill,"requested_schema":contract["requested_schema"],"typed_answers":contract["typed_answers"],"wire_values":contract["wire_values"]})
        }).collect();
        let elicitation=questions.iter().any(|question|question.contract.as_ref().is_some_and(|contract|contract["root_schema"].is_object()||contract["elicitation"]==true));
        let first = questions.first();
        let title = first
            .map(|question| question.header.as_str())
            .filter(|header| !header.trim().is_empty())
            .unwrap_or("需要你的回复");
        let row = json!({
            "request_id": request_id, "user_input_kind": if elicitation{"elicitation"}else{"questions"}, "title": title,
            "message": first.map(|question| question.question.as_str()).unwrap_or(""),
            "requested_schema": first.and_then(|q|q.contract.as_ref()).and_then(|c|c.get("root_schema")).cloned().unwrap_or(Value::Null),
            "questions": rows, "response_actions": if elicitation{json!(["submit","decline","cancel"])}else{json!(["submit","cancel"])}, "status": "pending",
            "turn_id": self.turn(doc), "agent_session_id": self.identity.agent_session_id,
            "requested_at": now(),
            "response_capability": {"answerable":true,"agent_session_id":self.identity.agent_session_id,"generation":self.identity.generation},
            "native": {"background": delivery == UserInputDelivery::Async, "parent_item_id": self.routed_agent},
        });
        state(doc)["pending_user_input"] = row.clone();
        refresh_plan_phase(doc);
        self.send(doc, emit, "core.user_input.requested", row);
    }

    fn steer_accepted(&mut self, doc: &mut Value, emit: &mut Sink, request_id: &str) {
        let Some(steer) = doc["pending_steers"]
            .as_object_mut()
            .and_then(|rows| rows.remove(request_id))
        else {
            return;
        };
        crate::control_subagents::open_for_message(doc, &json!({"command_type":"conversation.turn.steer","actor":steer["actor"],"payload":{}}));
        let client_message_id = steer["client_message_id"].as_str().unwrap_or(request_id).to_owned();
        let turn = steer["turn_id"].as_str().map(str::to_owned).unwrap_or_else(|| current_turn(doc));
        if !doc["messages"].is_array() {
            doc["messages"] = json!([]);
        }
        let seq = steer["requested_seq"].as_u64().unwrap_or_else(|| crate::chat::stream_head(doc) + 1);
        doc["messages"].as_array_mut().unwrap().push(json!({
            "message_id": format!("msg-steer-{client_message_id}"), "command_id":steer["command_id"], "turn_id": turn, "role": "user",
            "kind": "steer", "origin":steer["origin"], "text": steer["text"], "attachments": steer.get("attachments").cloned().unwrap_or_else(|| json!([])),
            "capability_refs": steer.get("capability_refs").cloned().unwrap_or_else(|| json!([])),
            "reference_context": steer.get("reference_context").cloned().unwrap_or_else(|| json!([])),
            "context_delivery": steer.get("context_delivery").cloned().unwrap_or_else(|| json!([])),
            "stream_seq": seq, "created_at": steer["requested_at"].as_str().map(str::to_owned).unwrap_or_else(now),
        }));
        self.send(doc, emit, "core.turn.steered", json!({"turn_id":turn,"text":steer["text"],"client_message_id":client_message_id,"request_id":request_id}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{ApprovalOption, ProviderKind, ResumeCursor};

    struct Harness {
        doc: Value,
        events: Vec<(String, Value)>,
        translator: Translator,
        effects: Effects,
    }

    impl Harness {
        fn new(mode: &str) -> Self {
            let declared: Vec<String> = access::ALL_MODES.iter().map(|mode| (*mode).to_owned()).collect();
            let plan = access::plan(ProviderKind::ClaudeCode, "claude.sdk", true, &declared, mode).unwrap();
            let identity = Identity {
                agent_session_id: "session-1".into(),
                engine: "claude".into(),
                adapter_id: "claude.sdk".into(),
                instance_id: "default".into(),
                transport: "structured".into(),
                cwd: "/tmp".into(),
                generation: 3,
                resumed: false,
            };
            Self {
                doc: json!({"thread":{"thread_id":"t","state":{"status":"running","running_turn_id":"turn-1"}},"turns":[{"turn_id":"turn-1","status":"running"}],"messages":[{"message_id":"u","turn_id":"turn-1","role":"user","text":"hi","stream_seq":1}],"event_seq":1}),
                events: Vec::new(),
                translator: Translator::new(identity, plan),
                effects: Effects::default(),
            }
        }

        fn apply(&mut self, event: AgentEvent) {
            let events = &mut self.events;
            let mut sink = |doc: &mut Value, kind: &str, payload: Value| {
                let seq = crate::chat::stream_head(doc) + 1;
                doc["event_seq"] = json!(seq);
                events.push((kind.to_owned(), payload));
            };
            self.translator.apply(&mut self.doc, event, &mut sink, &mut self.effects);
        }

        fn kinds(&self) -> Vec<&str> {
            self.events.iter().map(|(kind, _)| kind.as_str()).collect()
        }
    }

    fn item(id: &str, content: ItemContent) -> ThreadItem {
        ThreadItem { id: id.into(), parent_item_id: None, content }
    }

    fn command(status: ItemStatus, output: &str, exit_code: Option<i32>) -> ItemContent {
        ItemContent::CommandExecution { command: "ls".into(), output: output.into(), exit_code, status }
    }

    fn exec_request(id: &str) -> ApprovalRequest {
        ApprovalRequest {native_execution: None, tool_call_id: None,
            id: id.into(),
            turn_id: None,
            parent_item_id: None,
            kind: ApprovalKind::ExecCommand { command: "rm x".into(), cwd: None, reason: None },
            options: vec![
                ApprovalOption { id: "yes".into(), label: "Yes".into(), kind: ApprovalOptionKind::AllowOnce },
                ApprovalOption { id: "always".into(), label: "Always".into(), kind: ApprovalOptionKind::AllowAlways },
                ApprovalOption { id: "no".into(), label: "No".into(), kind: ApprovalOptionKind::RejectOnce },
            ],
        }
    }

    #[test]
    fn native_plan_exit_approval_carries_the_plan_into_the_card() {
        let h = Harness::new("supervised");
        for (name, input) in [
            ("ExitPlanMode", json!({"plan":"# Claude plan","planFilePath":"/tmp/plan.md"})),
            ("Exit spec mode", json!({"approval_kind":"plan_exit","plan_markdown":"# Droid plan"})),
        ] {
            let request = ApprovalRequest {
                native_execution: None,
                tool_call_id: None,
                id: format!("plan-{name}"),
                turn_id: None,
                parent_item_id: None,
                kind: ApprovalKind::ToolUse { name: name.into(), input: input.clone(), detail: String::new() },
                options: Vec::new(),
            };
            let row = h.translator.approval_row(&h.doc, &request);
            assert_eq!(row["approval_kind"], "plan_exit");
            assert_eq!(row["plan_markdown"].as_str(), input["plan"].as_str().or_else(|| input["plan_markdown"].as_str()));
        }
    }

    #[test]
    fn assistant_snapshots_stream_as_appends_and_a_rewrite_replaces() {
        let mut h = Harness::new("supervised");
        h.apply(AgentEvent::ItemStarted(item("a", ItemContent::AssistantMessage { text: "Hel".into() })));
        h.apply(AgentEvent::ItemUpdated(item("a", ItemContent::AssistantMessage { text: "Hello".into() })));
        h.apply(AgentEvent::ItemUpdated(item("a", ItemContent::AssistantMessage { text: "Goodbye".into() })));
        h.apply(AgentEvent::ItemCompleted(item("a", ItemContent::AssistantMessage { text: "Goodbye".into() })));
        assert_eq!(h.kinds(), ["core.message.delta", "core.message.delta", "core.message.delta", "core.message.completed"]);
        assert_eq!(h.events[1].1["text"], "lo");
        assert_eq!(h.events[2].1["replace"], true);
        assert_eq!(h.events[3].1["text"], "Goodbye");
        assert_eq!(h.events[0].1["agent_session_id"], "session-1");
        let message = &h.doc["messages"][1];
        assert_eq!(message["message_id"], "turn-1:a");
        assert_eq!(message["text"], "Goodbye");
        assert_eq!(message["status"], "completed");
        assert_eq!(h.doc["messages"].as_array().unwrap().len(), 2);
        h.apply(AgentEvent::ItemCompleted(item("a", ItemContent::AssistantMessage { text: "Goodbye".into() })));
        assert_eq!(h.events.len(), 4);
    }

    #[test]
    fn native_turn_ids_are_scoped_to_the_engine_session() {
        let mut h = Harness::new("supervised");
        h.doc["turns"] = json!([
            {"turn_id":"old","status":"completed","runtime_turn_id":"turn-1","agent_session_id":"previous-session"},
            {"turn_id":"current","status":"running","agent_session_id":"session-1"}
        ]);
        h.doc["thread"]["state"]["running_turn_id"] = json!("current");
        h.apply(AgentEvent::TurnStarted { turn_id: "turn-1".into() });
        h.apply(AgentEvent::ItemCompleted(item("answer", ItemContent::AssistantMessage { text: "new reply".into() })));
        assert_eq!(h.doc["turns"][0]["status"], "completed");
        assert_eq!(h.doc["turns"][1]["runtime_turn_id"], "turn-1");
        assert_eq!(h.doc["messages"][1]["turn_id"], "current");
        assert_eq!(h.doc["messages"][1]["text"], "new reply");
    }

    #[test]
    fn deltas_create_the_message_lazily_and_reasoning_stays_out_of_messages() {
        let mut h = Harness::new("supervised");
        h.apply(AgentEvent::Delta { item_id: "r".into(), kind: DeltaKind::ReasoningText, text: "think".into() });
        h.apply(AgentEvent::Delta { item_id: "a".into(), kind: DeltaKind::AssistantText, text: "Hi".into() });
        h.apply(AgentEvent::Delta { item_id: "a".into(), kind: DeltaKind::AssistantText, text: " there".into() });
        h.apply(AgentEvent::ItemCompleted(item("r", ItemContent::Reasoning { text: "thinking".into() })));
        assert_eq!(h.events[0].0, "core.reasoning.summary");
        assert_eq!(h.events[0].1["partial"], true);
        assert_eq!(h.events.last().unwrap().1["partial"], false);
        assert_eq!(h.events.last().unwrap().1["text"], "thinking");
        assert_eq!(h.doc["messages"].as_array().unwrap().len(), 2);
        assert_eq!(h.doc["messages"][1]["text"], "Hi there");
    }

    #[test]
    fn a_command_streams_output_and_completes_with_its_exit_code() {
        let mut h = Harness::new("supervised");
        h.apply(AgentEvent::ItemStarted(item("c", command(ItemStatus::InProgress, "", None))));
        h.apply(AgentEvent::Delta { item_id: "c".into(), kind: DeltaKind::CommandOutput, text: "a\n".into() });
        h.apply(AgentEvent::ItemCompleted(item("c", command(ItemStatus::Failed, "a\nboom", Some(2)))));
        assert_eq!(h.kinds(), ["core.tool.started", "core.tool.progress", "core.tool.completed"]);
        assert_eq!(h.events[0].1["name"], "shell");
        assert_eq!(h.events[0].1["input"]["command"], "ls");
        assert_eq!(h.events[1].1["chunk"], "a\n");
        let done = &h.events[2].1;
        assert_eq!(done["status"], "failed");
        assert_eq!(done["exit_code"], 2);
        assert_eq!(done["is_error"], true);
        assert_eq!(done["output"], "a\nboom");
        assert_eq!(h.doc["tools"][0]["status"], "failed");
        assert_eq!(h.doc["tools"][0]["output_summary"], "a\nboom");
    }

    #[test]
    fn a_subagent_tool_is_routed_to_its_agent_and_its_narration_is_not_a_reply() {
        let mut h = Harness::new("supervised");
        h.apply(AgentEvent::ItemStarted(item("spawn", ItemContent::Subagent {
            agent_type: "explorer".into(), description: "look".into(), status: ItemStatus::InProgress,
            summary: None, model: Some("m".into()), effort: None,
        })));
        h.apply(AgentEvent::ItemStarted(ThreadItem {
            id: "child".into(),
            parent_item_id: Some("spawn".into()),
            content: command(ItemStatus::InProgress, "", None),
        }));
        h.apply(AgentEvent::ItemCompleted(ThreadItem {
            id: "say".into(),
            parent_item_id: Some("spawn".into()),
            content: ItemContent::AssistantMessage { text: "inner".into() },
        }));
        assert_eq!(h.events[0].1["is_agent"], true);
        assert_eq!(h.events[0].1["kind"], "agent");
        assert_eq!(h.events[1].1["agent_id"], "spawn");
        assert_eq!(h.events.len(), 3);
        assert_eq!(h.events[2].0, "core.agent.updated");
        assert_eq!(h.doc["thread"]["state"]["agents"]["agents"][0]["activity"], "inner");
        assert_eq!(h.doc["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn full_access_approval_waits_for_adapter_acknowledgement() {
        let mut h = Harness::new("full-access");
        h.apply(AgentEvent::ApprovalRequested(exec_request("p1")));
        match h.effects.commands.as_slice() {
            [SessionCommand::RespondApproval { request_id, decision }] => {
                assert_eq!(request_id, "p1");
                assert_eq!(decision, &ApprovalDecision::Option("always".into()));
            }
            other => panic!("unexpected commands {other:?}"),
        }
        assert_eq!(h.kinds(), ["core.approval.delivery_pending"]);
        assert_eq!(h.events[0].1["automatic"], true);
        assert_eq!(h.events[0].1["scope"], "session");
        assert_eq!(h.doc["thread"]["state"]["pending_approvals"]["p1"]["status"], "processing");
        assert_eq!(h.doc["thread"]["state"]["pending_approvals"]["p1"]["response_capability"]["answerable"], false);
        h.apply(AgentEvent::ApprovalResolved { request_id: "p1".into(), decision: ApprovalDecision::Option("always".into()) });
        assert_eq!(h.kinds(), ["core.approval.delivery_pending", "core.approval.resolved"]);
        assert!(h.doc["thread"]["state"]["pending_approvals"].get("p1").is_none());
        h.apply(AgentEvent::ApprovalResolved { request_id: "p1".into(), decision: ApprovalDecision::Option("always".into()) });
        assert_eq!(h.events.len(), 2);
    }

    #[test]
    fn automatic_policy_denial_is_never_reported_as_allow() {
        let mut h = Harness::new("full-access");
        h.doc["src_policy_gates"] = json!({"p1":{"effect":"deny"}});
        h.apply(AgentEvent::ApprovalRequested(exec_request("p1")));
        assert!(matches!(h.effects.commands.as_slice(),
            [SessionCommand::RespondApproval { decision: ApprovalDecision::Cancel, .. }]));
        assert_eq!(h.events[0].1["decision"], "cancel");
        assert_eq!(h.doc["thread"]["state"]["pending_approvals"]["p1"]["decision"], "cancel");
        h.apply(AgentEvent::ApprovalResolved { request_id: "p1".into(), decision: ApprovalDecision::Cancel });
        assert_eq!(h.events[1].1["decision"], "cancel");
        assert_eq!(h.events[1].1["automatic"], true);
    }

    #[test]
    fn an_approval_without_a_reject_choice_can_still_be_refused() {
        let mut h = Harness::new("supervised");
        let mut request = exec_request("p1");
        request.options.retain(|option| option.kind == ApprovalOptionKind::AllowOnce);
        h.apply(AgentEvent::ApprovalRequested(request));
        let row = &h.doc["thread"]["state"]["pending_approvals"]["p1"];
        let reject = row["options"].as_array().unwrap().last().unwrap();
        assert_eq!(reject["kind"], "reject_once");
        assert_eq!(choose_option(row, "deny", "once", None), Ok(ApprovalDecision::Cancel));
        assert_eq!(
            choose_option(row, "deny", "once", reject["option_id"].as_str()),
            Ok(ApprovalDecision::Cancel)
        );
        assert_eq!(choose_option(row, "allow", "once", None), Ok(ApprovalDecision::Option("yes".into())));
    }

    #[test]
    fn supervised_approvals_wait_for_the_user_and_resolve_by_option_kind() {
        let mut h = Harness::new("supervised");
        h.apply(AgentEvent::ApprovalRequested(exec_request("p1")));
        assert!(h.effects.commands.is_empty());
        let state = &h.doc["thread"]["state"];
        let row = &state["pending_approvals"]["p1"];
        assert_eq!(row["approval_kind"], "command_execution");
        assert_eq!(row["title"], "命令执行");
        assert_eq!(row["command"], "rm x");
        assert_eq!(row["options"][0]["name"], "Yes");
        assert_eq!(row["scopes"], json!(["once", "session"]));
        assert_eq!(row["response_capability"]["generation"], 3);
        assert_eq!(state["pending_approval"]["approval_id"], "p1");
        assert_eq!(choose_option(row, "allow", "session", None), Ok(ApprovalDecision::Option("always".into())));
        assert_eq!(choose_option(row, "deny", "once", None), Ok(ApprovalDecision::Option("no".into())));
        assert_eq!(choose_option(row, "allow", "forever", None), Err("conversation.approval.scope_invalid"));
        assert_eq!(choose_option(row, "allow", "once", Some("nope")), Err("conversation.approval.option_invalid"));
        h.apply(AgentEvent::ApprovalResolved { request_id: "p1".into(), decision: ApprovalDecision::Option("no".into()) });
        let resolved = &h.events.last().unwrap().1;
        assert_eq!(resolved["decision"], "deny");
        assert_eq!(resolved["scope"], "once");
        assert!(h.doc["thread"]["state"]["pending_approval"].is_null());
        assert!(h.doc["thread"]["state"]["pending_approvals"].get("p1").is_none());
    }

    #[test]
    fn auto_accept_edits_still_asks_about_commands() {
        let mut h = Harness::new("auto-accept-edits");
        h.apply(AgentEvent::ApprovalRequested(exec_request("p1")));
        assert!(h.effects.commands.is_empty());
        let mut edit = exec_request("p2");
        edit.kind = ApprovalKind::FileChange { changes: Vec::new(), reason: None };
        h.apply(AgentEvent::ApprovalRequested(edit));
        assert_eq!(h.effects.commands.len(), 1);
        assert_eq!(h.events.last().unwrap().1["scope"], "once");
    }

    #[test]
    fn a_turn_end_expires_its_requests_and_reports_usage_and_failure() {
        let mut h = Harness::new("supervised");
        h.apply(AgentEvent::TurnStarted { turn_id: "native".into() });
        h.apply(AgentEvent::ApprovalRequested(exec_request("p1")));
        let mut background = exec_request("p2");
        background.parent_item_id = Some("spawn".into());
        h.apply(AgentEvent::ApprovalRequested(background));
        h.apply(AgentEvent::ItemStarted(item("c", command(ItemStatus::InProgress, "", None))));
        h.apply(AgentEvent::Error { message: "boom".into(), fatal: false });
        assert_eq!(h.doc["thread"]["state"]["last_error"]["message"], "boom");
        h.apply(AgentEvent::TurnCompleted {
            turn_id: "native".into(),
            status: TurnStatus::Failed,
            usage: Some(TokenUsage { input_tokens: Some(10), output_tokens: Some(4), used_tokens: Some(900), context_window: Some(2000), cost_usd: Some(0.5), ..TokenUsage::default() }),
        });
        let state = &h.doc["thread"]["state"];
        assert_eq!(state["pending_approvals"]["p1"]["status"], "expired");
        assert_eq!(state["pending_approvals"]["p1"]["response_capability"]["answerable"], false);
        assert_eq!(state["pending_approvals"]["p2"]["status"], "pending");
        assert_eq!(h.doc["tools"][0]["status"], "failed");
        assert_eq!(h.doc["context_window"]["total"], 900);
        assert_eq!(h.doc["context_window"]["limit"], 2000);
        match h.effects.outcomes.last().unwrap() {
            Outcome::TurnEnded { status, usage, error, .. } => {
                assert_eq!(*status, "failed");
                assert_eq!(error.as_deref(), Some("boom"));
                let usage = usage.as_ref().unwrap();
                assert_eq!(usage["total_tokens"], 14);
                assert_eq!(usage["reported_cost"], 0.5);
            }
            other => panic!("unexpected outcome {other:?}"),
        }
        let usage = h.events.iter().find(|(kind, payload)| kind == "core.usage.updated" && payload["usage_scope"] == "turn").unwrap();
        assert_eq!(usage.1["usage"]["context_used"], 900);
    }

    #[test]
    fn a_pre_prompt_mode_failure_keeps_its_typed_code() {
        let mut h = Harness::new("supervised");
        h.apply(AgentEvent::TurnStarted { turn_id: "native-plan".into() });
        h.apply(AgentEvent::TurnFailed {
            code: "permission_mode_switch".into(),
            message: "Claude rejected plan mode".into(),
        });
        h.apply(AgentEvent::TurnCompleted { turn_id: "native-plan".into(), status: TurnStatus::Failed, usage: None });
        assert_eq!(h.doc["thread"]["state"]["last_error"]["code"], "permission_mode_switch");
        assert!(h.events.iter().any(|(kind, payload)| kind == "core.runtime.error"
            && payload["code"] == "permission_mode_switch"));
        assert!(matches!(h.effects.outcomes.last(), Some(Outcome::TurnEnded { status: "failed", error: Some(message), .. })
            if message == "Claude rejected plan mode"));
    }

    #[test]
    fn a_compaction_operation_ends_its_turn_once() {
        let mut h = Harness::new("supervised");
        h.doc["turns"][0]["operation"] = json!("compact");
        h.apply(AgentEvent::TurnStarted { turn_id: "native".into() });
        h.apply(AgentEvent::ContextCompacted(Compaction { in_progress: true, ..Compaction::default() }));
        assert!(h.effects.outcomes.is_empty());
        h.apply(AgentEvent::ContextCompacted(Compaction::default()));
        assert!(matches!(h.effects.outcomes.as_slice(), [Outcome::TurnEnded { status: "completed", .. }]));
        assert_eq!(h.doc["context_window"]["compact_status"], "done");
        h.apply(AgentEvent::TurnCompleted { turn_id: "native".into(), status: TurnStatus::Completed, usage: None });
        assert_eq!(h.effects.outcomes.len(), 1);
        h.apply(AgentEvent::TurnCompleted { turn_id: "unknown".into(), status: TurnStatus::Completed, usage: None });
        assert_eq!(h.effects.outcomes.len(), 1);
        h.doc["turns"].as_array_mut().unwrap().push(json!({"turn_id":"turn-2","status":"running"}));
        h.doc["thread"]["state"]["running_turn_id"] = json!("turn-2");
        h.apply(AgentEvent::TurnStarted { turn_id: "next".into() });
        h.apply(AgentEvent::TurnCompleted { turn_id: "next".into(), status: TurnStatus::Completed, usage: None });
        assert_eq!(h.effects.outcomes.len(), 2);
    }

    #[test]
    fn failed_and_interrupted_compactions_preserve_the_previous_occupancy() {
        for (status, expected_turn) in [("failed", "failed"), ("interrupted", "interrupted")] {
            let mut h = Harness::new("supervised");
            h.doc["turns"][0]["operation"] = json!("compact");
            h.doc["context_window"] = json!({"total":320,"limit":1000,"zones":[],"compacted":[],"compact_status":"idle"});
            h.apply(AgentEvent::TurnStarted { turn_id: "native".into() });
            h.apply(AgentEvent::ContextCompacted(Compaction {
                status: Some(status.into()),
                trigger: Some("manual".into()),
                message: Some("provider declined".into()),
                post_tokens: Some(12),
                ..Default::default()
            }));
            assert_eq!(h.doc["context_window"]["compact_status"], status);
            assert_eq!(h.doc["context_window"]["total"], 320);
            assert_eq!(h.doc["context_window"]["compacted"][0]["status"], status);
            assert_eq!(h.events.last().unwrap().1["status"], status);
            assert!(matches!(h.effects.outcomes.as_slice(), [Outcome::TurnEnded { status: turn, .. }] if *turn == expected_turn));
            h.apply(AgentEvent::TurnCompleted { turn_id: "native".into(), status: TurnStatus::Failed, usage: None });
            assert_eq!(h.effects.outcomes.len(), 1);
        }
    }

    #[test]
    fn user_input_rows_round_trip_into_engine_answers() {
        let mut h = Harness::new("supervised");
        h.apply(AgentEvent::UserInputRequested {
            request_id: "q".into(),
            questions: vec![
                UserInputQuestion { contract: None,  id: "lang".into(), header: "语言".into(), question: "Which?".into(), options: vec![agent::UserInputOption { label: "Rust".into(), description: String::new() }, agent::UserInputOption { label: "Go".into(), description: String::new() }], multi_select: false, prefill: None },
                UserInputQuestion { contract: None,  id: "tags".into(), header: String::new(), question: "Tags?".into(), options: vec![agent::UserInputOption { label: "a".into(), description: String::new() }], multi_select: true, prefill: None },
            ],
            delivery: UserInputDelivery::Blocking,
        });
        let row = h.doc["thread"]["state"]["pending_user_input"].clone();
        assert_eq!(row["title"], "语言");
        assert_eq!(row["questions"][0]["kind"], "single_select");
        assert_eq!(row["questions"][1]["kind"], "multi_select");
        let answers = assemble_answers(&row, "submit", &json!({"lang":{"values":["Rust"]},"tags":{"values":["a"],"text":"b"}})).unwrap();
        assert_eq!(answers["lang"], "Rust");
        assert_eq!(answers["tags"], json!(["a", "b"]));
        let typed = assemble_answers(&row, "submit", &json!({"lang":{"values":["Rust"],"text":"Zig"},"tags":{"values":["a"]}})).unwrap();
        assert_eq!(typed["lang"], "Zig");
        assert_eq!(assemble_answers(&row, "submit", &json!({"lang":{"values":["C"]}})), Err("conversation.user_input.option_invalid"));
        assert_eq!(assemble_answers(&row, "submit", &json!({"lang":{"values":["Go"]}})), Err("conversation.user_input.answer_required"));
        assert!(assemble_answers(&row, "cancel", &json!({})).unwrap().is_empty());
        h.apply(AgentEvent::UserInputResolved { request_id: "q".into(), answers: Map::new() });
        assert!(h.doc["thread"]["state"]["pending_user_input"].is_null());
        assert_eq!(h.events.last().unwrap().1["cancelled"], true);
    }

    #[test]
    fn plan_phase_follows_tasks_and_pending_decisions() {
        let mut h = Harness::new("supervised");
        let steps = |statuses: &[PlanStepStatus]| statuses.iter().enumerate().map(|(i, status)| PlanStep { step: format!("s{i}"), status: *status }).collect::<Vec<_>>();
        h.apply(AgentEvent::PlanUpdated { turn_id: None, explanation: None, steps: steps(&[PlanStepStatus::Completed, PlanStepStatus::InProgress]) });
        let plan = &h.doc["thread"]["state"]["plan"];
        assert_eq!(plan["phase"], "executing");
        assert_eq!(plan["revision"], 1);
        assert_eq!(plan["last_change_summary"], "更新 2 个计划步骤");
        assert_eq!(plan["tasks"][1]["status"], "in_progress");
        h.apply(AgentEvent::ApprovalRequested(exec_request("p1")));
        assert_eq!(h.doc["thread"]["state"]["plan"]["phase"], "awaiting_decision");
        assert_eq!(h.doc["thread"]["state"]["plan"]["awaiting"]["id"], "p1");
        h.apply(AgentEvent::ApprovalResolved { request_id: "p1".into(), decision: ApprovalDecision::Option("yes".into()) });
        h.apply(AgentEvent::PlanUpdated { turn_id: None, explanation: None, steps: steps(&[PlanStepStatus::Completed, PlanStepStatus::Completed]) });
        assert_eq!(h.doc["thread"]["state"]["plan"]["phase"], "completed");
        assert_eq!(h.doc["thread"]["state"]["plan"]["revision"], 2);
    }

    #[test]
    fn an_accepted_steer_becomes_a_steer_message_and_a_rejected_one_is_returned() {
        let mut h = Harness::new("supervised");
        h.doc["pending_steers"] = json!({
            "s1":{"text":"also this","client_message_id":"c1","turn_id":"turn-1"},
            "s2":{"text":"too late","client_message_id":"c2","turn_id":"turn-1"},
        });
        h.apply(AgentEvent::SteerAccepted { request_id: "s1".into() });
        let message = h.doc["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(message["message_id"], "msg-steer-c1");
        assert_eq!(message["kind"], "steer");
        assert_eq!(h.kinds(), ["core.turn.steered"]);
        h.apply(AgentEvent::SteerRejected { request_id: "s2".into() });
        assert!(matches!(&h.effects.outcomes[..], [Outcome::SteerRejected { request_id, .. }] if request_id == "s2"));
        assert!(h.doc["pending_steers"].as_object().unwrap().is_empty());
    }

    #[test]
    fn session_start_and_close_project_the_session_and_expire_everything() {
        let mut h = Harness::new("supervised");
        h.apply(AgentEvent::SessionStarted { provider_session_id: "native-1".into(), resume: ResumeCursor(json!({"id":"native-1"})), model: Some("m".into()) });
        assert_eq!(h.doc["thread"]["state"]["agent_session_id"], "session-1");
        assert_eq!(h.doc["agent_session"]["external_session_id"], "native-1");
        assert_eq!(h.doc["native_session"]["resume"]["id"], "native-1");
        let mut background = exec_request("p2");
        background.parent_item_id = Some("spawn".into());
        h.apply(AgentEvent::ApprovalRequested(background));
        h.apply(AgentEvent::UsageLimitReached { resets_at: 1700 });
        assert_eq!(h.doc["thread"]["state"]["quota_hold"]["resets_at"], 1700);
        h.apply(AgentEvent::SessionClosed { reason: Some("exited".into()) });
        assert_eq!(h.doc["thread"]["state"]["pending_approvals"]["p2"]["status"], "expired");
        assert_eq!(h.kinds()[0], "core.session.started");
        assert_eq!(h.kinds().last().copied(), Some("core.session.closed"));
        assert!(matches!(h.effects.outcomes.last(), Some(Outcome::SessionClosed { resume_available: true, .. })));
    }
}
