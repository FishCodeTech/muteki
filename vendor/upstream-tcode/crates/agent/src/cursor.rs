//! Cursor: `cursor-agent [--force] [flags] acp`, an ACP agent with Cursor's
//! `cursor/*` dialect.
//!
//! The protocol machinery is [`crate::acp_session`]; this module is Cursor's
//! dialect. Its `session/update` mapping beyond the standard one lives in
//! [`updates`].

mod updates;

#[cfg(all(test, unix))]
mod tests;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use agent_client_protocol::{UntypedMessage, schema::v1 as acp};
use serde_json::{Map, Value, json};

use crate::acp_session::{
    self, Dialect, Established, Launch, Session, Setup, State, TurnEnd, describe, is_auth_required,
    prompt_status,
};
use crate::{
    AgentError, AgentEvent, Attachment, LaunchEnv, ModelSpec, OptionDescriptor, PlanStep,
    PlanStepStatus, ProviderKind, ResumeCursor, SelectOption, SessionHandle, SessionOptions,
    TurnStatus, UserInputOption, UserInputQuestion,
};

use updates::Updates;

const ASK_QUESTION: &str = "cursor/ask_question";
const CREATE_PLAN: &str = "cursor/create_plan";
const UPDATE_TODOS: &str = "cursor/update_todos";
const TASK: &str = "cursor/task";
const GENERATE_IMAGE: &str = "cursor/generate_image";
const LIST_AVAILABLE_MODELS: &str = "cursor/list_available_models";

/// The `configOptions` id of Cursor's model selector. The composer's model
/// picker owns the model ([`SessionOptions::model`]); this option is how the
/// session is put on it.
const MODEL_CONFIG_ID: &str = "model";

/// The assistant text Cursor's ACP server sends, as the turn's last output,
/// when its agent run throws; the turn then ends with `end_turn`.
const RUN_ERROR_PREFIX: &str = "\n\nError: ";
/// The same, for a run the backend refused until the account acts.
const RUN_ACTION_REQUIRED: [&str; 4] = [
    "\n\nPlease sign in to continue",
    "\n\nUpgrade your plan to continue",
    "\n\nAdd a payment method to continue",
    "\n\nCheck your settings to continue",
];

/// Start (or resume) a Cursor session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    let cursor = Cursor::new(&opts);
    acp_session::start(ProviderKind::Cursor, cursor, opts).await
}

/// The models `cursor/list_available_models` offers the signed-in account.
/// A running session still reports its parameters over the wire
/// ([`crate::OptionDescriptors::Wire`]); the catalog also lists each model's
/// `configOptions` under the `acp:cfg:<id>` selection ids a session start
/// restores. It marks no default, so a session without a model runs the one
/// Cursor is set to.
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    let opts = SessionOptions {
        cwd: std::env::temp_dir(),
        model: None,
        resume: None,
        fork: false,
        binary_path,
        option_selections: Vec::new(),
        mcp_servers: Vec::new(),
        launch_env,
        extra_args: Vec::new(),
        acp: None,
        instructions: None,
    };
    let cursor = Cursor::new(&opts);
    acp_session::query(
        ProviderKind::Cursor,
        cursor,
        opts,
        async |connection, _init| {
            let listed = connection
                .send_request(UntypedMessage {
                    method: LIST_AVAILABLE_MODELS.into(),
                    params: json!({}),
                })
                .block_task()
                .await
                .map_err(|err| {
                    if is_auth_required(&err) {
                        signed_out()
                    } else {
                        AgentError::Protocol(format!(
                            "Cursor could not list its models: {}",
                            describe(&err)
                        ))
                    }
                })?;
            Ok(catalog(&listed))
        },
    )
    .await
}

/// `{models: [{value, name, configOptions}]}`.
///
/// Cursor puts several of a model's options (`thinking` and `effort`) in the
/// thought-level category, which a session maps to one canonical id; only the
/// `acp:cfg:<id>` form names each of them unambiguously.
fn catalog(listed: &Value) -> Vec<ModelSpec> {
    listed
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model.get("value")?.as_str()?;
            Some(ModelSpec {
                id: id.to_string(),
                display_name: model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                is_default: false,
                options: model
                    .get("configOptions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(config_option)
                    .collect(),
            })
        })
        .collect()
}

fn config_option(option: &Value) -> Option<OptionDescriptor> {
    if option.get("type").and_then(Value::as_str) != Some("select") {
        return None;
    }
    let id = option.get("id")?.as_str()?;
    let options: Vec<SelectOption> = option
        .get("options")?
        .as_array()?
        .iter()
        .filter_map(|choice| {
            let value = choice.get("value")?.as_str()?;
            Some(SelectOption {
                value: value.to_string(),
                label: choice
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(value)
                    .to_string(),
                description: choice
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                unavailable: None,
            })
        })
        .collect();
    (!options.is_empty()).then(|| OptionDescriptor::Select {
        id: format!("{}{id}", acp_session::CONFIG_OPTION_PREFIX),
        label: option
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string(),
        options,
        default_value: option
            .get("currentValue")
            .and_then(Value::as_str)
            .map(str::to_string),
        role: crate::OptionRole::Model,
        apply: crate::ApplyTiming::Live,
        recommended: None,
        permissive: None,
    })
}

fn signed_out() -> AgentError {
    AgentError::Provider(
        "Cursor is not signed in. Run `cursor-agent login`, or set CURSOR_API_KEY in Settings → Providers → Cursor, then try again."
            .into(),
    )
}

struct Cursor {
    updates: Mutex<Updates>,
    /// Cursor's todo list as `cursor/update_todos` last left it.
    todos: Mutex<Vec<Todo>>,
    /// Whether the running turn's latest output is Cursor's run-failure text.
    run_failed: AtomicBool,
}

impl Cursor {
    fn new(opts: &SessionOptions) -> Self {
        Self {
            updates: Mutex::new(Updates::new(opts.cwd.clone())),
            todos: Mutex::new(Vec::new()),
            run_failed: AtomicBool::new(false),
        }
    }

    fn updates(&self) -> MutexGuard<'_, Updates> {
        self.updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn track_run_failure(&self, update: &acp::SessionUpdate) {
        let failed = match update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                acp::ContentBlock::Text(text) => {
                    text.text.starts_with(RUN_ERROR_PREFIX)
                        || RUN_ACTION_REQUIRED.contains(&text.text.as_str())
                }
                _ => false,
            },
            acp::SessionUpdate::AgentThoughtChunk(_)
            | acp::SessionUpdate::ToolCall(_)
            | acp::SessionUpdate::ToolCallUpdate(_)
            | acp::SessionUpdate::Plan(_) => false,
            _ => return,
        };
        self.run_failed.store(failed, Ordering::Relaxed);
    }

    /// `cursor/update_todos`: the list replaces Cursor's todos, or with
    /// `merge` updates them by id.
    fn update_todos(&self, params: &Value) -> Vec<PlanStep> {
        let incoming: Vec<Todo> = params
            .get("todos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Todo::parse)
            .collect();
        let merge = params
            .get("merge")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut todos = self
            .todos
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if merge {
            for todo in incoming {
                match todos.iter_mut().find(|known| known.id == todo.id) {
                    Some(known) => *known = todo,
                    None => todos.push(todo),
                }
            }
        } else {
            *todos = incoming;
        }
        todos.iter().filter_map(Todo::step).collect()
    }
}

impl Dialect for Cursor {
    fn name(&self) -> &str {
        "Cursor"
    }

    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError> {
        // Not the generic `agent` name Cursor also installs: that name is too
        // common to resolve from PATH with confidence.
        let program = crate::resolve_binary(opts.binary_path.as_deref(), "cursor-agent")?;
        let mut args = Vec::new();
        // approvalMode allowlist/auto-review require user config; no documented launch override exists.
        args.push("--force".to_string());
        let pairs = opts.launch_env.pairs(ProviderKind::Cursor);
        let endpoint = pairs.iter().rev().find(|(key, _)| key == "CURSOR_ENDPOINT")
            .map(|(_, value)| value.clone()).or_else(|| std::env::var("CURSOR_ENDPOINT").ok());
        if let Some(endpoint) = endpoint.filter(|value| !value.trim().is_empty()) {
            args.extend(["--endpoint".into(), endpoint]);
        }
        args.extend(opts.extra_args.iter().cloned());
        args.push("acp".to_string());
        let client_meta = json!({ "parameterizedModelPicker": true, "subagents": true });
        Ok(Launch {
            program,
            args,
            env: Vec::new(),
            client_meta: client_meta.as_object().cloned(),
        })
    }

    async fn establish(&self, setup: &Setup<'_>) -> Result<Established, AgentError> {
        let (connection, opts) = (setup.connection, setup.opts);
        if opts.fork {
            return Err(AgentError::Protocol(
                "session fork is not supported for this provider".into(),
            ));
        }
        let mcp_servers = setup.mcp_servers().await;
        let resumed = opts
            .resume
            .as_ref()
            .and_then(|cursor| cursor.str_field(&["session_id"]))
            .map(str::to_string);
        // `authenticate` opens a browser when Cursor is signed out, so it is
        // never driven from here: a missing login is reported instead.
        let (session_id, modes, config_options) = match resumed {
            Some(session_id) => {
                let loaded = setup
                    .load(
                        acp::LoadSessionRequest::new(session_id.clone(), opts.cwd.clone())
                            .mcp_servers(mcp_servers),
                    )
                    .await
                    .map_err(|err| {
                        start_failure(&err, &format!("could not resume session {session_id}"))
                    })?;
                (
                    acp::SessionId::new(session_id),
                    loaded.modes,
                    loaded.config_options,
                )
            }
            None => {
                let created = connection
                    .send_request(
                        acp::NewSessionRequest::new(opts.cwd.clone()).mcp_servers(mcp_servers),
                    )
                    .block_task()
                    .await
                    .map_err(|err| start_failure(&err, "could not start a session"))?;
                (created.session_id, created.modes, created.config_options)
            }
        };
        setup.adopt(modes.as_ref(), config_options.as_deref());
        let mut config_options = config_options.unwrap_or_default();
        if let Some(model) = &opts.model
            && let Some(selected) = select_model(setup, &session_id, model, &config_options).await?
        {
            config_options = selected;
        }
        restore_parameters(setup, &session_id, &config_options).await;
        Ok(Established {
            resume: ResumeCursor(json!({ "session_id": session_id.0.to_string() })),
            session_id,
        })
    }

    fn session_update(
        &self,
        state: &mut State,
        notification: acp::SessionNotification,
    ) -> Vec<AgentEvent> {
        // `session/load` replays the whole conversation before it answers;
        // tcode's own log is the conversation of record.
        if state.loading() {
            return Vec::new();
        }
        let mut updates = self.updates();
        if !updates.is_subagent(&notification.session_id) {
            self.track_run_failure(&notification.update);
        }
        updates.session_update(state, notification)
    }

    fn turn_end(
        &self,
        _state: &mut State,
        result: Result<acp::PromptResponse, acp::Error>,
    ) -> TurnEnd {
        let (mut status, message) = prompt_status(&result);
        // The failure text stays in the transcript as Cursor wrote it.
        if self.run_failed.swap(false, Ordering::Relaxed) && status == TurnStatus::Completed {
            status = TurnStatus::Failed;
        }
        // Cursor reports no usage over ACP.
        TurnEnd {
            status,
            message,
            usage: None,
        }
    }

    async fn steer(
        &self,
        _session: &Session,
        _request_id: String,
        _text: String,
        _attachments: Vec<Attachment>,
    ) {
    }

    fn handles_request(&self, method: &str) -> bool {
        matches!(
            method,
            ASK_QUESTION | CREATE_PLAN | UPDATE_TODOS | TASK | GENERATE_IMAGE
        )
    }

    async fn request(
        &self,
        session: Session,
        method: String,
        params: Value,
    ) -> Result<Value, acp::Error> {
        // None of these is sent while `session/load` replays, but a replay
        // must never put a question to the user.
        let replaying = session.with_state(|state| state.loading());
        match method.as_str() {
            ASK_QUESTION => {
                let questions = Questions::parse(&params)?;
                if replaying {
                    return Ok(json!({ "outcome": { "outcome": "cancelled" } }));
                }
                let answers = session.ask_user(questions.canonical()).await;
                let (reply, unsent) = questions.reply(answers);
                if !unsent.is_empty() {
                    session
                        .emit(AgentEvent::Warning {
                            message: format!(
                                "Cursor takes only the offered answers; the typed answer to {} was not sent.",
                                unsent.join(", ")
                            ),
                        })
                        .await;
                }
                Ok(reply)
            }
            CREATE_PLAN => {
                if replaying {
                    return Ok(json!({"outcome": {"outcome": "cancelled"}}));
                }
                let content = params.get("plan").and_then(Value::as_str).unwrap_or("");
                let answers = session
                    .ask_user(vec![UserInputQuestion { contract: None, 
                        id: "decision".into(),
                        header: method,
                        question: content.to_owned(),
                        options: [
                            ("Accept", "Let Cursor save the plan"),
                            (
                                "Reject",
                                "Decline the plan; type a reason to provide feedback",
                            ),
                        ]
                        .into_iter()
                        .map(|(label, description)| UserInputOption {
                            label: label.into(),
                            description: description.into(),
                        })
                        .collect(),
                        multi_select: false,
                        prefill: None,
                    }])
                    .await;
                let answer = answers
                    .as_ref()
                    .and_then(|answers| answers.get("decision"))
                    .and_then(Value::as_str);
                Ok(match answer {
                    Some("Accept") => json!({"outcome": {"outcome": "accepted"}}),
                    Some("Reject") => json!({"outcome": {"outcome": "rejected"}}),
                    Some(reason) => json!({"outcome": {"outcome": "rejected", "reason": reason}}),
                    None => json!({"outcome": {"outcome": "cancelled"}}),
                })
            }
            UPDATE_TODOS => {
                if !replaying {
                    let steps = self.update_todos(&params);
                    let turn_id = session.with_state(|state| state.turn().map(str::to_owned));
                    session
                        .emit(AgentEvent::PlanUpdated {
                            turn_id,
                            explanation: None,
                            steps,
                        })
                        .await;
                }
                Ok(json!({}))
            }
            // Sent once the task or image tool has finished; Cursor ignores
            // the reply, and the tool's own updates carry its result.
            _ => Ok(json!({})),
        }
    }

    fn notification(&self, state: &mut State, method: &str, params: Value) -> Vec<AgentEvent> {
        if method != "session/update" {
            log::trace!("cursor: {method} {params}");
            return Vec::new();
        }
        if state.loading() {
            return Vec::new();
        }
        self.updates().subagent_update(&params)
    }

    /// Cursor's file and shell tools are its own: it calls none of these
    /// services, and not offering them keeps it that way.
    fn client_services(&self) -> bool {
        false
    }

    fn owned_config_options(&self) -> &'static [&'static str] {
        &[MODEL_CONFIG_ID]
    }
}

fn start_failure(err: &acp::Error, context: &str) -> AgentError {
    if is_auth_required(err) {
        signed_out()
    } else {
        AgentError::Protocol(format!("Cursor {context}: {}", describe(err)))
    }
}

/// Put the session on the composer's model, returning Cursor's options for
/// it. Cursor resumes a loaded conversation on the model it last used, so
/// this follows `session/load` as well as `session/new`.
async fn select_model(
    setup: &Setup<'_>,
    session_id: &acp::SessionId,
    model: &str,
    config_options: &[acp::SessionConfigOption],
) -> Result<Option<Vec<acp::SessionConfigOption>>, AgentError> {
    let current = config_options
        .iter()
        .find(|option| option.id.0.as_ref() == MODEL_CONFIG_ID)
        .and_then(select_value);
    if current == Some(model) {
        return Ok(None);
    }
    let selected = set_config_option(setup, session_id, MODEL_CONFIG_ID, model)
        .await
        .map_err(|err| {
            AgentError::Provider(format!(
                "Cursor did not accept the model `{model}`: {}",
                describe(&err)
            ))
        })?;
    Ok(Some(selected))
}

/// Re-select the model parameters this session chose before: a new process
/// starts on the parameters Cursor last saved for the model, which another
/// session may have changed. A choice the model no longer offers is dropped.
async fn restore_parameters(
    setup: &Setup<'_>,
    session_id: &acp::SessionId,
    config_options: &[acp::SessionConfigOption],
) {
    for option in config_options {
        let id = option.id.0.as_ref();
        let acp::SessionConfigKind::Select(select) = &option.kind else {
            continue;
        };
        let Some(chosen) = acp_session::config_selection(
            &setup.opts.option_selections,
            id,
            option.category.as_ref(),
        ) else {
            continue;
        };
        let offered = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(options) => options
                .iter()
                .any(|option| option.value.0.as_ref() == chosen),
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| &group.options)
                .any(|option| option.value.0.as_ref() == chosen),
            _ => false,
        };
        if id == MODEL_CONFIG_ID || select_value(option) == Some(chosen) || !offered {
            continue;
        }
        if let Err(err) = set_config_option(setup, session_id, id, chosen).await {
            setup
                .warn(format!(
                    "Cursor did not restore {} `{chosen}`: {}",
                    option.name,
                    describe(&err)
                ))
                .await;
        }
    }
}

fn select_value(option: &acp::SessionConfigOption) -> Option<&str> {
    match &option.kind {
        acp::SessionConfigKind::Select(select) => Some(select.current_value.0.as_ref()),
        _ => None,
    }
}

/// `session/set_config_option`, adopting the refreshed options it answers
/// with.
async fn set_config_option(
    setup: &Setup<'_>,
    session_id: &acp::SessionId,
    config_id: &str,
    value: &str,
) -> Result<Vec<acp::SessionConfigOption>, acp::Error> {
    let response = setup
        .connection
        .send_request(acp::SetSessionConfigOptionRequest::new(
            session_id.clone(),
            acp::SessionConfigId::new(config_id),
            acp::SessionConfigOptionValue::value_id(acp::SessionConfigValueId::new(value)),
        ))
        .block_task()
        .await?;
    setup.adopt(None, Some(&response.config_options));
    Ok(response.config_options)
}

/// The questions of a `cursor/ask_question` request. Canonical answers are
/// option labels; Cursor takes option ids.
struct Questions {
    title: String,
    questions: Vec<Question>,
}

struct Question {
    contract: Option<Value>,
    id: String,
    prompt: String,
    /// `(label, id)` in Cursor's order.
    options: Vec<(String, String)>,
    allow_multiple: bool,
}

impl Questions {
    fn parse(params: &Value) -> Result<Self, acp::Error> {
        let questions = params
            .get("questions")
            .and_then(Value::as_array)
            .ok_or_else(acp::Error::invalid_params)?;
        Ok(Self {
            title: params
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            questions: questions
                .iter()
                .filter_map(|question| {
                    Some(Question {
                        contract: crate::native_question_contract(&json!({
                            "options": question["options"].as_array().map(|options|options.iter().map(|option|json!({"value":option["label"],"label":option["label"]})).collect::<Vec<_>>()).unwrap_or_default(),
                            "multi_select": question["allowMultiple"]==true,
                            "allow_free_text": question["allowFreeformInput"].as_bool().unwrap_or(true),
                            "required": question["required"].as_bool().unwrap_or(true),
                            "requested_schema": question["schema"]
                        })),
                        id: question.get("id")?.as_str()?.to_string(),
                        prompt: question.get("prompt")?.as_str()?.to_string(),
                        options: question
                            .get("options")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|option| {
                                Some((
                                    option.get("label")?.as_str()?.to_string(),
                                    option.get("id")?.as_str()?.to_string(),
                                ))
                            })
                            .collect(),
                        allow_multiple: question
                            .get("allowMultiple")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect(),
        })
    }

    fn canonical(&self) -> Vec<UserInputQuestion> {
        self.questions
            .iter()
            .map(|question| UserInputQuestion { contract: question.contract.clone(),
                id: question.id.clone(),
                header: self.title.clone(),
                question: question.prompt.clone(),
                options: question
                    .options
                    .iter()
                    .map(|(label, _)| UserInputOption {
                        label: label.clone(),
                        description: String::new(),
                    })
                    .collect(),
                multi_select: question.allow_multiple,
                prefill: None,
            })
            .collect()
    }

    /// The reply to Cursor, and the prompts whose typed answer matched no
    /// option (Cursor's answers carry option ids only). `None` is a request
    /// cancelled by an interrupt or shutdown.
    fn reply(&self, answers: Option<Map<String, Value>>) -> (Value, Vec<String>) {
        let Some(answers) = answers else {
            return (json!({ "outcome": { "outcome": "cancelled" } }), Vec::new());
        };
        let mut unsent = Vec::new();
        let answered: Vec<Value> = self
            .questions
            .iter()
            .filter_map(|question| {
                let labels: Vec<&str> = match answers.get(&question.id)? {
                    Value::String(label) => vec![label.as_str()],
                    Value::Array(labels) => labels.iter().filter_map(Value::as_str).collect(),
                    _ => Vec::new(),
                };
                let mut selected = Vec::new();
                for label in labels.into_iter().filter(|label| !label.trim().is_empty()) {
                    match question.options.iter().find(|(known, _)| known == label) {
                        Some((_, id)) => selected.push(id.as_str()),
                        None => unsent.push(format!("“{}”", question.prompt)),
                    }
                }
                (!selected.is_empty())
                    .then(|| json!({ "questionId": question.id, "selectedOptionIds": selected }))
            })
            .collect();
        let reply = if answered.is_empty() {
            json!({ "outcome": { "outcome": "skipped" } })
        } else {
            json!({ "outcome": { "outcome": "answered", "answers": answered } })
        };
        (reply, unsent)
    }
}

/// One entry of Cursor's todo list.
#[derive(Clone)]
struct Todo {
    id: String,
    content: String,
    status: String,
}

impl Todo {
    fn parse(todo: &Value) -> Option<Self> {
        Some(Self {
            id: todo.get("id")?.as_str()?.to_string(),
            content: todo.get("content")?.as_str()?.to_string(),
            status: todo
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending")
                .to_string(),
        })
    }

    /// A cancelled todo is no longer part of the plan; the canonical plan
    /// has no cancelled step.
    fn step(&self) -> Option<PlanStep> {
        let status = match self.status.as_str() {
            "in_progress" => PlanStepStatus::InProgress,
            "completed" => PlanStepStatus::Completed,
            "cancelled" => return None,
            _ => PlanStepStatus::Pending,
        };
        Some(PlanStep {
            step: self.content.clone(),
            status,
        })
    }
}
