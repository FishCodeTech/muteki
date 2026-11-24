//! Grok Build: `grok [--permission-mode M] agent [flags] stdio`, an ACP agent
//! with xAI's `_x.ai/*` dialect.
//!
//! The protocol machinery is [`crate::acp_session`]; this module is Grok's
//! dialect.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use agent_client_protocol::{UntypedMessage, schema::v1 as acp};
use serde_json::{Map, Value, json};

use crate::acp_session::{
    self, Dialect, Established, Launch, Session, Setup, State, TurnEnd, describe, is_auth_required,
    prompt_status, stop_reason_status,
};
use crate::{
    AgentError, AgentEvent, Attachment, Compaction, ItemContent, LaunchEnv, ModelSpec,
    OptionDescriptor, ProviderKind, ResumeCursor, SelectOption, SessionHandle, SessionOptions, TokenUsage, TurnStatus,
    UserInputOption, UserInputQuestion,
};

/// The non-interactive auth method: it validates `XAI_API_KEY` (or the key in
/// Grok's `config.toml`). The other advertised method signs in through a
/// browser and is never driven from here.
const API_KEY_AUTH_METHOD: &str = "xai.api_key";

const ASK_USER_QUESTION: &str = "_x.ai/ask_user_question";
const EXIT_PLAN_MODE: &str = "_x.ai/exit_plan_mode";
const INTERJECT: &str = "_x.ai/interject";
const INTERJECTION: &str = "_x.ai/session/interjection";
const QUEUE_CHANGED: &str = "_x.ai/queue/changed";
const TASK_BACKGROUNDED: &str = "_x.ai/task_backgrounded";
const SESSION_NOTIFICATION: &str = "_x.ai/session_notification";
const FORK: &str = "_x.ai/session/fork";

pub(crate) mod plugins;

/// Start (or resume, or fork) a Grok session.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    let grok = Grok::default();
    acp_session::start(ProviderKind::Grok, grok, opts).await
}

/// The models Grok's `initialize` advertises. Their options arrive over the
/// wire once a session starts ([`crate::OptionDescriptors::Wire`]).
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
    acp_session::query(
        ProviderKind::Grok,
        Grok::default(),
        opts,
        async |_connection, init| Ok(catalog(init)),
    )
    .await
}

/// `initialize`'s `_meta.modelState`.
fn catalog(init: &acp::InitializeResponse) -> Vec<ModelSpec> {
    let Some(state) = model_state(init) else {
        return Vec::new();
    };
    let current = state.get("currentModelId").and_then(Value::as_str);
    available_models(state)
        .filter_map(|model| {
            let id = model.get("modelId")?.as_str()?;
            let meta = &model["_meta"];
            let select = |id: &str, label: &str, options, default_value| OptionDescriptor::Select {
                id: id.into(), label: label.into(), options, default_value,
                role: Default::default(), apply: Default::default(), recommended: None, permissive: None,
            };
            let mut options = Vec::new();
            if meta["supportsReasoningEffort"] == true {
                let efforts: Vec<_> = meta["reasoningEfforts"].as_array().into_iter().flatten()
                    .filter_map(|effort| Some(SelectOption {
                        value: effort["value"].as_str().or_else(|| effort["id"].as_str())?.into(),
                        label: effort["label"].as_str().or_else(|| effort["value"].as_str())?.into(),
                        description: effort["description"].as_str().map(str::to_owned), unavailable: None,
                    })).collect();
                let default = meta["reasoningEfforts"].as_array().into_iter().flatten()
                    .find(|effort| effort["default"] == true)
                    .and_then(|effort| effort["value"].as_str().or_else(|| effort["id"].as_str()))
                    .map(str::to_owned);
                options.push(select("reasoningEffort", "Reasoning effort", efforts, default));
            }
            let windows: Vec<_> = meta["contextWindows"].as_array().into_iter().flatten()
                .filter_map(Value::as_u64).filter(|size| *size > 0)
                .map(|size| SelectOption {
                    value: size.to_string(), label: if size % 1000 == 0 { format!("{}K", size / 1000) } else { size.to_string() },
                    description: None, unavailable: None,
                }).collect();
            if !windows.is_empty() {
                options.push(select("acp:cfg:context", "Context window", windows,
                    meta["totalContextTokens"].as_u64().map(|size| size.to_string())));
            }
            Some(ModelSpec {
                id: id.to_string(),
                display_name: model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                is_default: Some(id) == current,
                options,
            })
        })
        .collect()
}

fn model_state(init: &acp::InitializeResponse) -> Option<&Value> {
    init.meta.as_ref()?.get("modelState")
}

fn available_models(state: &Value) -> impl Iterator<Item = &Value> {
    state
        .get("availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

#[derive(Default)]
struct Grok {
    /// Steers sent with `_x.ai/interject`, oldest first, until Grok echoes
    /// them back as consumed.
    steers: Mutex<VecDeque<(String, String)>>,
    models: Mutex<Models>,
    activity: Mutex<Activity>,
}

/// What Grok runs: its prompts, as `_x.ai/queue/changed` reports them, and
/// its background tasks. Grok runs prompts one at a time and starts some
/// itself: when a background task finishes it runs a prompt reporting it,
/// which no `session/prompt` answers.
#[derive(Default)]
struct Activity {
    /// Prompts the client queued, until they run.
    queued: HashSet<String>,
    /// The running prompt the client sent.
    client: Option<String>,
    /// The running prompt Grok started itself; its turn has its id.
    agent: Option<String>,
    /// Shell tool calls whose command went on in the background. Grok streams
    /// their later output as running updates and never completes them again.
    backgrounded: HashSet<String>,
    background_tasks: usize,
    /// A drop to no background tasks, withheld while a sent turn is open:
    /// Grok follows that turn with one reporting the finished task, and the
    /// runtime must not take the process for idle in between.
    drained: bool,
}

impl Activity {
    fn sent_turn_open(&self, state: &State) -> bool {
        state.turn().is_some() && state.turn() != self.agent.as_deref()
    }
}

/// What usage reporting needs to know about the session's models.
#[derive(Default)]
struct Models {
    /// `totalContextTokens` by model id.
    windows: HashMap<String, u64>,
    /// The model serving the session.
    current: Option<String>,
    /// The latest model call's usage.
    last_call: Option<TokenUsage>,
}

impl Grok {
    fn models(&self) -> MutexGuard<'_, Models> {
        self.models
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn activity(&self) -> MutexGuard<'_, Activity> {
        self.activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn steers(&self) -> MutexGuard<'_, VecDeque<(String, String)>> {
        self.steers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One model call's `response_completed` usage, whose `input_tokens`
    /// excludes the cached prompt.
    fn call_usage(&self, usage: &Value) -> TokenUsage {
        let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        let prompt = count("input_tokens")
            + count("cache_read_input_tokens")
            + count("cache_creation_input_tokens");
        let mut models = self.models();
        let usage = TokenUsage {
            freshness: crate::ContextFreshness::Current,
            input_tokens: Some(prompt),
            cached_input_tokens: Some(count("cache_read_input_tokens")),
            cache_write_tokens: Some(count("cache_creation_input_tokens")),
            output_tokens: Some(count("output_tokens")),
            used_tokens: Some(prompt + count("output_tokens")),
            context_window: models.window(),
            ..TokenUsage::default()
        };
        models.last_call = Some(usage);
        usage
    }

    /// A turn's end as `turn_completed` reports it: its stop reason, and the
    /// turn's usage on top of its last model call's.
    fn reported_turn_end(&self, update: &Value) -> TurnEnd {
        let (status, message) = match update.get("stop_reason").and_then(Value::as_str) {
            Some("error") => (
                TurnStatus::Failed,
                Some(
                    update
                        .get("agent_result")
                        .and_then(Value::as_str)
                        .unwrap_or("Grok's turn failed")
                        .to_string(),
                ),
            ),
            Some(reason) => serde_json::from_value::<acp::StopReason>(json!(reason))
                .map(stop_reason_status)
                .unwrap_or_else(|_| {
                    (
                        TurnStatus::Failed,
                        Some(format!("The agent stopped: {reason}")),
                    )
                }),
            None => (TurnStatus::Completed, None),
        };
        let models = self.models();
        let usage = update.get("usage").map(|usage| TokenUsage {
            freshness: crate::ContextFreshness::Current,
            turn_processed_tokens: usage.get("totalTokens").and_then(Value::as_u64),
            context_window: models.window(),
            ..models.last_call.unwrap_or_default()
        });
        TurnEnd {
            status,
            message,
            usage,
        }
    }

    /// A running prompt the client did not queue opens a turn of its own.
    fn queue_changed(&self, state: &mut State, params: &Value) -> Vec<AgentEvent> {
        let mut activity = self.activity();
        for entry in params
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                activity.queued.insert(id.to_string());
            }
        }
        let Some(running) = params.get("runningPromptId").and_then(Value::as_str) else {
            return Vec::new();
        };
        if [&activity.client, &activity.agent]
            .iter()
            .any(|prompt| prompt.as_deref() == Some(running))
        {
            return Vec::new();
        }
        if activity.queued.remove(running) {
            activity.client = Some(running.to_string());
            return Vec::new();
        }
        activity.agent = Some(running.to_string());
        state.begin_agent_turn(running)
    }

    /// `turn_completed` ends the turn of the prompt it names, in order with
    /// everything else Grok sends; a sent turn's later `session/prompt`
    /// result then completes nothing.
    fn turn_completed(&self, state: &mut State, update: &Value) -> Vec<AgentEvent> {
        let Some(prompt) = update.get("prompt_id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let end = self.reported_turn_end(update);
        let mut activity = self.activity();
        if activity.agent.as_deref() == Some(prompt) {
            activity.agent = None;
            return state.end_turn(prompt, end);
        }
        if activity.client.as_deref() == Some(prompt) {
            activity.client = None;
            if let Some(turn) = state.turn().map(str::to_string)
                && activity.sent_turn_open(state)
            {
                return state.end_turn(&turn, end);
            }
        }
        Vec::new()
    }

    /// The `session/prompt` result's `_meta`: the last model call's figures
    /// (whose `totalTokens` is the context in use) and the turn's `usage`.
    fn turn_usage(&self, meta: &acp::Meta) -> TokenUsage {
        let count = |value: Option<&Value>| value.and_then(Value::as_u64);
        let mut models = self.models();
        if let Some(model) = meta.get("modelId").and_then(Value::as_str) {
            models.current = Some(model.to_string());
        }
        TokenUsage {
            freshness: crate::ContextFreshness::Current,
            turn_processed_tokens: count(
                meta.get("usage").and_then(|usage| usage.get("totalTokens")),
            ),
            input_tokens: count(meta.get("inputTokens")),
            cached_input_tokens: count(meta.get("cachedReadTokens")),
            output_tokens: count(meta.get("outputTokens")),
            // `/compact` reports 0: nothing has been measured since.
            used_tokens: count(meta.get("totalTokens")).filter(|tokens| *tokens > 0),
            context_window: models.window(),
            ..TokenUsage::default()
        }
    }

    /// One `_x.ai/session_notification` update.
    fn session_notification(&self, state: &mut State, update: &Value) -> Vec<AgentEvent> {
        let count = |key: &str| update.get(key).and_then(Value::as_u64);
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("response_completed") => update
                .get("usage")
                .map(|usage| vec![AgentEvent::TokenUsage(self.call_usage(usage))])
                .unwrap_or_default(),
            // A request that failed for good ends the turn, which reports it.
            Some("retry_state")
                if update.get("type").and_then(Value::as_str) == Some("retrying") =>
            {
                vec![AgentEvent::Error {
                    message: retry_message(update),
                    fatal: false,
                }]
            }
            Some("image_dropped") => {
                let notes: Vec<&str> = update
                    .get("notes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if notes.is_empty() {
                    return Vec::new();
                }
                vec![AgentEvent::Warning {
                    message: notes.join("\n"),
                }]
            }
            Some("background_tasks") => {
                let running = update
                    .get("tasks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|task| task.get("status").and_then(Value::as_str) == Some("running"))
                    .count();
                let mut activity = self.activity();
                let drained = running == 0 && activity.background_tasks > 0;
                activity.background_tasks = running;
                activity.drained = drained && activity.sent_turn_open(state);
                if activity.drained {
                    return Vec::new();
                }
                vec![AgentEvent::BackgroundTasksChanged { count: running }]
            }
            Some("turn_completed") => self.turn_completed(state, update),
            Some("auto_compact_started") => vec![AgentEvent::ContextCompacted(Compaction {
                in_progress: true,
                trigger: Some("auto".into()),
                pre_tokens: count("tokens_used"),
                ..Compaction::default()
            })],
            Some("auto_compact_completed") => vec![AgentEvent::ContextCompacted(Compaction {
                in_progress: false,
                pre_tokens: count("tokens_before"),
                post_tokens: count("tokens_after"),
                duration_ms: count("elapsed_ms"),
                ..Compaction::default()
            })],
            _ => Vec::new(),
        }
    }

    fn steer_accepted(&self, params: &Value) -> Vec<AgentEvent> {
        let text = params.get("text").and_then(Value::as_str);
        let mut steers = self.steers();
        let Some(position) = steers
            .iter()
            .position(|(sent, _)| Some(sent.as_str()) == text)
        else {
            return Vec::new();
        };
        let (_, request_id) = steers.remove(position).expect("position is in range");
        vec![AgentEvent::SteerAccepted { request_id }]
    }
}

impl Models {
    fn window(&self) -> Option<u64> {
        self.windows.get(self.current.as_deref()?).copied()
    }
}

impl Dialect for Grok {
    fn name(&self) -> &str {
        "Grok"
    }

    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError> {
        let program = crate::resolve_binary(opts.binary_path.as_deref(), "grok")?;
        // Live switching through x.ai/yolo_mode_changed is unverified.
        let permission_mode = opts
            .option_selections
            .iter()
            .find(|selection| selection.id == "permissionMode")
            .and_then(|selection| selection.value.as_str())
            .unwrap_or("default");
        let mut args = vec![
            "--permission-mode".to_string(),
            permission_mode.to_string(),
            "agent".to_string(),
        ];
        if let Some(model) = &opts.model {
            args.extend(["--model".to_string(), model.clone()]);
        }
        if let Some(effort) = acp_session::config_selection(
            &opts.option_selections,
            "reasoning_effort",
            Some(&acp::SessionConfigOptionCategory::ThoughtLevel),
        ) {
            args.extend(["--reasoning-effort".to_string(), effort.to_string()]);
        }
        args.extend(opts.extra_args.iter().cloned());
        args.push("stdio".to_string());
        Ok(Launch {
            program,
            args,
            env: Vec::new(),
            client_meta: None,
        })
    }

    async fn establish(&self, setup: &Setup<'_>) -> Result<Established, AgentError> {
        let (connection, opts) = (setup.connection, setup.opts);
        if let Some(state) = model_state(setup.init) {
            let mut models = self.models();
            models.windows = available_models(state)
                .filter_map(|model| {
                    let id = model.get("modelId")?.as_str()?;
                    let window = model.pointer("/_meta/totalContextTokens")?.as_u64()?;
                    Some((id.to_string(), window))
                })
                .collect();
            models.current = opts.model.clone().or_else(|| {
                state
                    .get("currentModelId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        }
        let offers_api_key = setup.init.auth_methods.iter().any(|method| {
            let id = match method {
                acp::AuthMethod::Agent(method) => &method.id,
                acp::AuthMethod::EnvVar(method) => &method.id,
                acp::AuthMethod::Terminal(method) => &method.id,
                _ => return false,
            };
            id.0.as_ref() == API_KEY_AUTH_METHOD
        });
        let authenticated = if offers_api_key {
            connection
                .send_request(acp::AuthenticateRequest::new(API_KEY_AUTH_METHOD))
                .block_task()
                .await
                .map(|_| ())
        } else {
            Ok(())
        };
        let auth_failure = |err: &acp::Error| {
            let detail = match &authenticated {
                Err(auth) => describe(auth),
                Ok(()) => describe(err),
            };
            AgentError::Provider(format!(
                "Grok requires authentication: {detail}. Set XAI_API_KEY in Settings → Providers → Grok, or sign in with `grok login`."
            ))
        };

        let mcp_servers = setup.mcp_servers().await;
        let resumed = opts
            .resume
            .as_ref()
            .and_then(|cursor| cursor.str_field(&["session_id"]))
            .map(str::to_string);
        let (session_id, modes, config_options) = match resumed {
            Some(source) => {
                let session_id = if opts.fork {
                    fork(connection, &source, opts).await?
                } else {
                    source
                };
                let resumed = connection
                    .send_request(
                        acp::ResumeSessionRequest::new(session_id.clone(), opts.cwd.clone())
                            .mcp_servers(mcp_servers),
                    )
                    .block_task()
                    .await
                    .map_err(|err| {
                        if is_auth_required(&err) {
                            auth_failure(&err)
                        } else {
                            AgentError::Provider(format!(
                                "Grok could not resume session {session_id}: {}",
                                describe(&err)
                            ))
                        }
                    })?;
                (
                    acp::SessionId::new(session_id),
                    resumed.modes,
                    resumed.config_options,
                )
            }
            None => {
                let created = connection
                    .send_request(
                        acp::NewSessionRequest::new(opts.cwd.clone()).mcp_servers(mcp_servers),
                    )
                    .block_task()
                    .await
                    .map_err(|err| {
                        if is_auth_required(&err) {
                            auth_failure(&err)
                        } else {
                            AgentError::Protocol(format!(
                                "Grok could not start a session: {}",
                                describe(&err)
                            ))
                        }
                    })?;
                (created.session_id, created.modes, created.config_options)
            }
        };

        // Grok advertises contextWindows in model metadata, but changes them
        // through session/set_model's _meta rather than configOptions.
        let selected_window = opts.option_selections.iter().find(|selection| selection.id == "acp:cfg:context")
            .map(|selection| selection.value.as_str().and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| AgentError::Protocol("Invalid Grok context window selection".into())))
            .transpose()?;
        let model = self.models().current.clone();
        let default_window = model.as_ref().and_then(|model| model_state(setup.init).into_iter().flat_map(available_models)
            .find(|row| row["modelId"] == *model))
            .filter(|row| row["_meta"]["contextWindows"].as_array().is_some_and(|sizes| !sizes.is_empty()))
            .and_then(|row| row["_meta"]["totalContextTokens"].as_u64());
        if let Some(window) = selected_window.or(default_window) {
            let model = model.ok_or_else(|| AgentError::Protocol("Grok has no current model".into()))?;
            let supported = model_state(setup.init).into_iter().flat_map(available_models)
                .find(|row| row["modelId"] == model)
                .is_some_and(|row| row["_meta"]["contextWindows"].as_array()
                    .is_some_and(|sizes| sizes.iter().any(|size| size.as_u64() == Some(window))));
            if !supported {
                return Err(AgentError::Protocol(format!("Grok model {model} does not offer context window {window}")));
            }
            setup.raw("session/set_model", json!({"sessionId":session_id.0,"modelId":model,"_meta":{"contextWindow":window}})).await?;
            self.models().windows.insert(model, window);
        }

        if !opts.mcp_servers.is_empty() && setup.init.agent_capabilities.mcp_capabilities.http {
            let seconds = opts.launch_env.env.iter().rev().find(|(key,_)|key == "MUTEKI_GROK_MCP_READY_WAIT_SECONDS")
                .and_then(|(_,value)|value.parse::<f64>().ok()).or_else(||std::env::var("MUTEKI_GROK_MCP_READY_WAIT_SECONDS").ok().and_then(|value|value.parse().ok()))
                .filter(|value|value.is_finite()).unwrap_or(8.0).clamp(0.0,30.0);
            smol::Timer::after(std::time::Duration::from_secs_f64(seconds)).await;
        }
        setup.adopt(modes.as_ref(), config_options.as_deref());
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
        // `session/load` replays history flagged as such; tcode's own log is
        // the conversation of record.
        let replayed = notification
            .meta
            .as_ref()
            .and_then(|meta| meta.get("isReplay"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if replayed {
            return Vec::new();
        }
        let mut update = notification.update;
        let shell = match &mut update {
            acp::SessionUpdate::ToolCallUpdate(update) => {
                let output = update.fields.raw_output.as_ref();
                if output.and_then(|raw| raw.get("type")) == Some(&json!("Bash"))
                    && self
                        .activity()
                        .backgrounded
                        .contains(update.tool_call_id.0.as_ref())
                {
                    update.fields.status = None;
                }
                prepare_tool_update(update)
            }
            _ => None,
        };
        let mut events = state.apply_update(update);
        if let Some(shell) = shell {
            events.iter_mut().for_each(|event| shell.render(event));
        }
        events
    }

    fn turn_end(
        &self,
        _state: &mut State,
        result: Result<acp::PromptResponse, acp::Error>,
    ) -> TurnEnd {
        let (status, message) = match (prompt_status(&result), &result) {
            ((TurnStatus::Failed, _), Err(err)) => (TurnStatus::Failed, Some(failure_message(err))),
            (outcome, _) => outcome,
        };
        TurnEnd {
            status,
            message,
            usage: result
                .as_ref()
                .ok()
                .and_then(|response| response.meta.as_ref())
                .map(|meta| self.turn_usage(meta)),
        }
    }

    async fn steer(
        &self,
        session: &Session,
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    ) {
        if !attachments.is_empty() {
            session.emit(AgentEvent::ControlDelivery {
                request_id, operation: "steer".into(), status: "failed".into(),
                error: Some(json!({"code":"grok.steer_attachments_unsupported","message":"Grok interject accepts text only. Queue the attachments for the next turn."})),
            }).await;
            return;
        }
        let Some(session_id) = session.id() else {
            return;
        };
        self.steers().push_back((text.clone(), request_id.clone()));
        let sent = session
            .connection
            .send_request(UntypedMessage {
                method: INTERJECT.into(),
                params: json!({ "sessionId": session_id.0.to_string(), "text": text }),
            })
            .block_task()
            .await;
        if let Err(err) = sent {
            self.steers().retain(|(_, pending)| pending != &request_id);
            session
                .emit(AgentEvent::Warning {
                    message: format!("Grok did not accept the steer: {}", describe(&err)),
                })
                .await;
        }
    }

    /// Offered `fs/*` and `terminal/*`, Grok runs every read, write and
    /// command through them instead of its own tools.
    fn client_services(&self) -> bool {
        false
    }

    fn owned_config_options(&self) -> &'static [&'static str] {
        &["model"]
    }

    fn handles_request(&self, method: &str) -> bool {
        matches!(method, ASK_USER_QUESTION | EXIT_PLAN_MODE)
    }

    async fn request(
        &self,
        session: Session,
        method: String,
        params: Value,
    ) -> Result<Value, acp::Error> {
        if method == EXIT_PLAN_MODE {
            let content = params
                .get("planContent")
                .and_then(Value::as_str)
                .unwrap_or("");
            let answers = session
                .ask_user(vec![UserInputQuestion { contract: None, 
                    id: "decision".into(),
                    header: method,
                    question: content.to_owned(),
                    options: [
                        ("Approve", "Exit planning and begin implementation"),
                        (
                            "Request changes",
                            "Keep planning; type feedback to request specific changes",
                        ),
                        ("Abandon", "Exit planning without implementation"),
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
            return Ok(match answer {
                Some("Approve") => json!({"outcome": "approved"}),
                Some("Abandon") | None => json!({"outcome": "abandoned"}),
                Some("Request changes") => json!({"outcome": "cancelled"}),
                Some(feedback) => json!({"outcome": "cancelled", "feedback": feedback}),
            });
        }
        let questions = questions(&params)?;
        Ok(match session.ask_user(questions).await {
            Some(answers) => json!({ "outcome": "accepted", "answers": answer_lists(answers) }),
            None => json!({ "outcome": "cancelled" }),
        })
    }

    fn notification(&self, state: &mut State, method: &str, params: Value) -> Vec<AgentEvent> {
        let mut events = match method {
            INTERJECTION => self.steer_accepted(&params),
            QUEUE_CHANGED => self.queue_changed(state, &params),
            TASK_BACKGROUNDED => {
                if let Some(id) = params
                    .pointer("/update/tool_call_id")
                    .and_then(Value::as_str)
                {
                    self.activity().backgrounded.insert(id.to_string());
                }
                Vec::new()
            }
            SESSION_NOTIFICATION => params
                .get("update")
                .map(|update| self.session_notification(state, update))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let mut activity = self.activity();
        if activity.drained && !activity.sent_turn_open(state) {
            activity.drained = false;
            events.push(AgentEvent::BackgroundTasksChanged { count: 0 });
        }
        events
    }
}

/// Grok fails a turn with `-32603 Internal error`, its own words in `data`.
fn failure_message(err: &acp::Error) -> String {
    err.data
        .as_ref()
        .and_then(|data| data.get("message"))
        .and_then(Value::as_str)
        .map_or_else(|| describe(err), str::to_string)
}

fn retry_message(update: &Value) -> String {
    let reason = update
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("Grok's model request failed");
    match (
        update.get("attempt").and_then(Value::as_u64),
        update.get("max_retries").and_then(Value::as_u64),
    ) {
        (Some(attempt), Some(max)) => format!("{reason} (retry {attempt} of {max})"),
        _ => reason.to_string(),
    }
}

/// Grok's shell tool resends the command's whole output with every update.
struct ShellResult {
    item_id: String,
    output: String,
    exit_code: Option<i32>,
}

impl ShellResult {
    fn render(&self, event: &mut AgentEvent) {
        if let AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item) = event
            && item.id == self.item_id
            && let ItemContent::CommandExecution {
                output, exit_code, ..
            } = &mut item.content
        {
            output.clone_from(&self.output);
            *exit_code = self.exit_code;
        }
    }
}

/// Bring a `tool_call_update` into the shape the standard mapping renders
/// faithfully. Grok's `rawOutput` is its typed tool result, not display text,
/// so it must never become an item's output. Returns the shell result the
/// mapped item shows instead.
fn prepare_tool_update(update: &mut acp::ToolCallUpdate) -> Option<ShellResult> {
    let fields = &mut update.fields;
    let raw_type = fields
        .raw_output
        .as_ref()
        .and_then(|raw| raw.get("type"))
        .and_then(Value::as_str);
    match raw_type {
        Some("Bash") => {
            let finished = matches!(
                fields.status,
                Some(acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed)
            );
            // Running updates carry a placeholder exit code.
            let exit_code = fields
                .raw_output
                .as_ref()
                .and_then(|raw| raw.get("exit_code"))
                .and_then(Value::as_i64)
                .filter(|_| finished)
                .map(|code| code as i32);
            Some(ShellResult {
                item_id: update.tool_call_id.0.to_string(),
                output: text_of(fields.content.as_deref().unwrap_or_default()),
                exit_code,
            })
        }
        Some("MCP") => {
            let text = fields
                .raw_output
                .as_ref()
                .and_then(|raw| raw.pointer("/output/OkayOutput"))
                .map(|output| match output {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                });
            if fields.content.is_none()
                && let Some(text) = text
            {
                fields.content = Some(vec![text.into()]);
            }
            None
        }
        // Before a command runs, its content is the model's description of it.
        None if fields.kind == Some(acp::ToolKind::Execute) => {
            fields.content = None;
            None
        }
        _ => None,
    }
}

fn text_of(content: &[acp::ToolCallContent]) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            acp::ToolCallContent::Content(block) => match &block.content {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Fork `source` into a new Grok session in the same directory.
async fn fork(
    connection: &acp_session::Connection,
    source: &str,
    opts: &SessionOptions,
) -> Result<String, AgentError> {
    let cwd = opts.cwd.to_string_lossy();
    let forked = connection
        .send_request(UntypedMessage {
            method: FORK.into(),
            params: json!({ "sourceSessionId": source, "sourceCwd": cwd, "newCwd": cwd }),
        })
        .block_task()
        .await
        .map_err(|err| {
            AgentError::Provider(format!(
                "Grok could not fork session {source}: {}",
                describe(&err)
            ))
        })?;
    forked
        .get("newSessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| AgentError::Protocol(format!("Grok's fork returned no session: {forked}")))
}

/// The questions of an `_x.ai/ask_user_question` request. Grok keys answers
/// by question text.
fn questions(params: &Value) -> Result<Vec<UserInputQuestion>, acp::Error> {
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(acp::Error::invalid_params)?;
    Ok(questions
        .iter()
        .filter_map(|question| {
            let text = question.get("question")?.as_str()?.to_string();
            Some(UserInputQuestion { contract: crate::native_question_contract(question), 
                id: text.clone(),
                header: question
                    .get("header")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                question: text,
                options: question
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|option| {
                        Some(UserInputOption {
                            label: option.get("label")?.as_str()?.to_string(),
                            description: option
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect(),
                multi_select: question
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                prefill: None,
            })
        })
        .collect())
}

/// Grok takes every answer as a list of selected labels.
fn answer_lists(answers: Map<String, Value>) -> Map<String, Value> {
    answers
        .into_iter()
        .map(|(question, answer)| {
            let answer = match answer {
                Value::Array(_) => answer,
                other => Value::Array(vec![other]),
            };
            (question, answer)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead as _, Write as _};
    use std::time::Duration;

    use super::*;
    use crate::{
        ApprovalDecision, ContextFreshness, FileChangeKind, ItemStatus, McpRegistration,
        OptionSelection, SessionCommand, ThreadItem,
    };

    // Grok 1.0.46 sessions recorded on the wire in both directions
    // (provenance: `tests/fixtures/grok/README.md`).
    /// A resumed session under AutoAcceptEdits.
    const RESUMED_TURN: &str = include_str!("../tests/fixtures/grok/resumed_turn.jsonl");
    /// A background task finishing during a sent turn, the turn Grok starts
    /// to report it, then another sent turn.
    const AGENT_STARTED_TURN: &str =
        include_str!("../tests/fixtures/grok/agent_started_turn.jsonl");
    const REPLAY_AGENT: &str = "TCODE_GROK_REPLAY_AGENT";

    /// Stand in for `grok agent stdio`: answer the client with the recorded
    /// agent messages, and require the client to send what Tcode sent, with
    /// the same answers to Grok's own requests.
    fn replay_agent(fixture: &str) {
        let mut client = std::io::stdin().lock().lines();
        let mut agent = std::io::stdout().lock();
        let mut live_ids: HashMap<String, Value> = HashMap::new();
        let mut mode_requests = HashSet::new();
        for record in fixture.lines() {
            let record: Value = serde_json::from_str(record).unwrap();
            let recorded = &record["message"];
            if recorded["method"] == "session/set_mode" {
                mode_requests.insert(recorded["id"].to_string());
                continue;
            }
            if recorded.get("method").is_none()
                && mode_requests.contains(&recorded["id"].to_string())
            {
                continue;
            }
            if record["from"] == "agent" {
                let mut message = recorded.clone();
                if message.get("method").is_none() {
                    message["id"] = live_ids[&message["id"].to_string()].clone();
                }
                writeln!(agent, "{message}").unwrap();
                agent.flush().unwrap();
                continue;
            }
            let live = loop {
                let line = client.next().expect("the client hung up early").unwrap();
                // The client's protocol errors about the test harness's own
                // non-JSON output carry no id.
                if let Ok(live) = serde_json::from_str::<Value>(&line)
                    && !live.get("id").is_some_and(Value::is_null)
                {
                    break live;
                }
            };
            assert_eq!(live.get("method"), recorded.get("method"), "{live}");
            if recorded["method"] == "initialize" {
                let capabilities =
                    |message: &Value| message["params"]["clientCapabilities"].clone();
                assert_eq!(capabilities(&live), capabilities(recorded), "{live}");
            }
            match recorded.get("id") {
                Some(id) if recorded.get("method").is_some() => {
                    live_ids.insert(id.to_string(), live["id"].clone());
                }
                _ => assert_eq!(live["result"], recorded["result"], "{live}"),
            }
        }
        for _ in client {}
    }

    /// A stand-in `grok` that runs the test `test` as [`replay_agent`];
    /// whatever arguments Tcode launches Grok with are left unread.
    fn stand_in_binary(dir: &std::path::Path, test: &str) -> PathBuf {
        let test = format!("grok::tests::{test}");
        let exe = std::env::current_exe().unwrap();
        let exe = exe.display();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let path = dir.join("grok");
            let script = format!("#!/bin/sh\nexec '{exe}' --exact {test} --nocapture\n");
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
        #[cfg(windows)]
        {
            let path = dir.join("grok.cmd");
            std::fs::write(&path, format!("@\"{exe}\" --exact {test} --nocapture\r\n")).unwrap();
            path
        }
    }

    fn command_outputs<'a>(events: &'a [AgentEvent], id: &str) -> Vec<(&'a str, Option<i32>)> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item)
                    if item.id == id =>
                {
                    match &item.content {
                        ItemContent::CommandExecution {
                            output, exit_code, ..
                        } => Some((output.as_str(), *exit_code)),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect()
    }

    fn completed<'a>(events: &'a [AgentEvent], id: &str) -> &'a ItemContent {
        events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ItemCompleted(ThreadItem {
                    id: item, content, ..
                }) if item == id => Some(content),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{id} never completed: {events:#?}"))
    }

    fn completed_last<'a>(events: &'a [AgentEvent], id: &str) -> &'a ItemContent {
        events
            .iter()
            .rev()
            .find_map(|event| match event {
                AgentEvent::ItemCompleted(ThreadItem {
                    id: item, content, ..
                }) if item == id => Some(content),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{id} never completed: {events:#?}"))
    }

    fn streamed(events: &[AgentEvent], id: &str) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Delta { item_id, text, .. } if item_id == id => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Options for a session against a stand-in Grok running `test`, in a
    /// fresh directory.
    fn stand_in_options(test: &str) -> SessionOptions {
        let dir =
            std::env::temp_dir().join(format!("tcode-grok-replay-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        SessionOptions {
            binary_path: Some(stand_in_binary(&dir, test)),
            cwd: dir,
            model: None,
            resume: None,
            fork: false,
            option_selections: Vec::new(),
            mcp_servers: Vec::new(),
            launch_env: LaunchEnv {
                env: vec![(REPLAY_AGENT.into(), "1".into())],
                home: None,
            },
            extra_args: Vec::new(),
            acp: None,
            instructions: None,
        }
    }

    /// What the person driving a replayed session does after a turn completes.
    enum Next {
        Wait,
        Send(&'static str),
        Close,
    }

    /// Run a session as the app does: send `first`, approve once and answer
    /// questions with their first option, and after the `n`th completed turn
    /// do `next(n)`. Returns every event through the close.
    fn replay(opts: SessionOptions, first: &str, next: impl Fn(usize) -> Next) -> Vec<AgentEvent> {
        let dir = opts.cwd.clone();
        let events = smol::block_on(smol::future::or(
            async {
                let handle = start(opts).await.unwrap();
                let send = |delivery_id, text: &str| SessionCommand::SendTurn {
                    delivery_id,
                    text: text.into(),
                    options: None,
                    attachments: Vec::new(),
                };
                handle.commands.send(send(1, first)).await.unwrap();
                let (mut events, mut completed) = (Vec::new(), 0);
                while let Ok(event) = handle.events.recv().await {
                    let command = match &event {
                        AgentEvent::ApprovalRequested(request) => {
                            Some(SessionCommand::RespondApproval {
                                request_id: request.id.clone(),
                                decision: ApprovalDecision::Option(
                                    request
                                        .options
                                        .iter()
                                        .find(|option| {
                                            option.kind == crate::ApprovalOptionKind::AllowOnce
                                        })
                                        .unwrap()
                                        .id
                                        .clone(),
                                ),
                            })
                        }
                        AgentEvent::UserInputRequested {
                            request_id,
                            questions,
                            ..
                        } => Some(SessionCommand::RespondUserInput {
                            request_id: request_id.clone(),
                            answers: questions
                                .iter()
                                .map(|question| {
                                    (question.id.clone(), json!(question.options[0].label))
                                })
                                .collect(),
                        }),
                        AgentEvent::TurnCompleted { .. } => {
                            completed += 1;
                            match next(completed) {
                                Next::Wait => None,
                                Next::Send(text) => Some(send(completed as u64 + 1, text)),
                                Next::Close => Some(SessionCommand::Shutdown),
                            }
                        }
                        _ => None,
                    };
                    let closed = matches!(event, AgentEvent::SessionClosed { .. });
                    events.push(event);
                    if let Some(command) = command {
                        handle.commands.send(command).await.unwrap();
                    }
                    if closed {
                        break;
                    }
                }
                events
            },
            async {
                smol::Timer::after(Duration::from_secs(60)).await;
                panic!("the replayed session did not finish");
            },
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // Every client message and answer matched the recording, or the
        // stand-in would have hung up before the close.
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::SessionClosed { reason: None })
            ),
            "{events:#?}"
        );
        events
    }

    #[test]
    fn recorded_resumed_turn_maps_to_the_canonical_stream() {
        if std::env::var_os(REPLAY_AGENT).is_some() {
            return replay_agent(RESUMED_TURN);
        }
        let opts = SessionOptions {
            resume: Some(ResumeCursor(
                json!({ "session_id": "01a10095-e65c-7983-a817-6884b010f1ae" }),
            )),
            option_selections: vec![OptionSelection {
                id: "reasoningEffort".into(),
                value: json!("low"),
            }],
            mcp_servers: vec![McpRegistration {
                name: "tcode_probe".into(),
                url: "http://127.0.0.1:18433/mcp".into(),
                bearer_token: "tcode-secret".into(),
            }],
            ..stand_in_options("recorded_resumed_turn_maps_to_the_canonical_stream")
        };
        let events = replay(opts, "Count, edit, ask and echo", |_| Next::Close);

        let started = events
            .iter()
            .position(|event| matches!(event, AgentEvent::TurnStarted { .. }))
            .unwrap();
        assert!(
            !events[..started].iter().any(|event| matches!(
                event,
                AgentEvent::Delta { .. }
                    | AgentEvent::ItemStarted(_)
                    | AgentEvent::ItemUpdated(_)
                    | AgentEvent::ItemCompleted(_)
            )),
            "a resumed session replays no history: {:#?}",
            &events[..started]
        );

        assert_eq!(streamed(&events, "thought-1"), "Counting first.");
        assert!(matches!(completed(&events, "thought-1"),
            ItemContent::Reasoning { text } if text == "Counting first."));

        // Each shell update carries the whole output so far; the item shows
        // it, never the command's description or Grok's typed result.
        let shell = command_outputs(&events, "call_33_1");
        assert_eq!(
            shell.last(),
            Some(&("line1\nline2\nline3\n", Some(0))),
            "{shell:?}"
        );
        assert!(
            shell
                .windows(2)
                .all(|pair| pair[1].0.starts_with(pair[0].0) && pair[0].1.is_none()),
            "{shell:?}"
        );
        assert!(shell.iter().any(|(output, _)| *output == "line1\n"));
        assert_eq!(
            command_outputs(&events, "call_34_0").last(),
            Some(&("", Some(0)))
        );

        assert!(matches!(completed(&events, "call_35_0"),
            ItemContent::FileChange { changes, status: ItemStatus::Completed }
                if changes.len() == 1 && changes[0].kind == FileChangeKind::Modify
                    && changes[0].path.ends_with("/in.txt")));
        assert!(events.iter().any(|event| matches!(event,
            AgentEvent::UserInputRequested { questions, .. }
                if questions[0].id == "Which greeting should I use?")));
        assert!(matches!(completed(&events, "call_37_0"),
            ItemContent::ToolCall { name, output: Some(output), status: ItemStatus::Completed, .. }
                if name == "tcode_probe__echo_upper" && output == "HELLO MCP"));

        assert_eq!(
            streamed(&events, "msg-2"),
            "Counted, edited, asked and echoed."
        );
        assert!(matches!(completed(&events, "msg-2"),
            ItemContent::AssistantMessage { text } if text == "Counted, edited, asked and echoed."));

        assert!(events.iter().any(|event| matches!(event,
            AgentEvent::TokenUsage(usage)
                if usage.context_window == Some(500_000) && usage.used_tokens == Some(1234))));
        let completions: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TurnCompleted { status, usage, .. } => Some((*status, *usage)),
                _ => None,
            })
            .collect();
        assert_eq!(completions.len(), 1, "{completions:?}");
        let (status, usage) = completions[0];
        assert_eq!(status, TurnStatus::Completed);
        let usage = usage.expect("the turn reports its usage");
        assert_eq!(usage.freshness, ContextFreshness::Current);
        assert_eq!(usage.turn_processed_tokens, Some(7404));
        assert_eq!(usage.used_tokens, Some(1234));
        assert_eq!(usage.context_window, Some(500_000));
    }

    /// Grok reports a background task that finished during a sent turn with
    /// a turn of its own, which no `session/prompt` answers. It starts while
    /// the sent turn's result is still on its way.
    #[test]
    fn a_turn_grok_starts_itself_runs_between_the_sent_turns() {
        if std::env::var_os(REPLAY_AGENT).is_some() {
            return replay_agent(AGENT_STARTED_TURN);
        }
        let opts = stand_in_options("a_turn_grok_starts_itself_runs_between_the_sent_turns");
        let events = replay(
            opts,
            "Background then foreground",
            |completed| match completed {
                1 => Next::Wait,
                2 => Next::Send("And now?"),
                _ => Next::Close,
            },
        );

        let agent = "task-completed-01a100c2-37c5-7fc2-b92a-c0f8b2c3659b";
        let at = |wanted: &dyn Fn(&AgentEvent) -> bool| {
            events
                .iter()
                .position(wanted)
                .unwrap_or_else(|| panic!("{events:#?}"))
        };
        let lifecycle: Vec<(&str, &str, Option<u64>)> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TurnStarted { turn_id } => Some(("started", turn_id.as_str(), None)),
                AgentEvent::TurnCompleted { turn_id, usage, .. } => Some((
                    "completed",
                    turn_id.as_str(),
                    usage.and_then(|usage| usage.turn_processed_tokens),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            lifecycle,
            [
                ("started", "turn-1", None),
                ("completed", "turn-1", Some(3702)),
                ("started", agent, None),
                ("completed", agent, Some(1234)),
                ("started", "turn-2", None),
                ("completed", "turn-2", Some(1234)),
            ]
        );

        let reply = at(&|event| {
            matches!(event, AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text }, ..
            }) if text == "The background task printed bgdone.")
        });
        let agent_started =
            at(&|event| matches!(event, AgentEvent::TurnStarted { turn_id } if turn_id == agent));
        let agent_completed = at(
            &|event| matches!(event, AgentEvent::TurnCompleted { turn_id, .. } if turn_id == agent),
        );
        assert!(agent_started < reply && reply < agent_completed);

        // The runtime must not take the process for idle before Grok's turn.
        let sent_completed = at(
            &|event| matches!(event, AgentEvent::TurnCompleted { turn_id, .. } if turn_id == "turn-1"),
        );
        let drained = at(&|event| matches!(event, AgentEvent::BackgroundTasksChanged { count: 0 }));
        assert!(sent_completed < drained, "{events:#?}");

        // The backgrounded command's later output leaves its card completed.
        let background = events
            .iter()
            .filter(|event| {
                matches!(event, AgentEvent::ItemUpdated(item) | AgentEvent::ItemCompleted(item)
                    if item.id == "call_68_0")
            })
            .skip_while(|event| !matches!(event, AgentEvent::ItemCompleted(_)))
            .collect::<Vec<_>>();
        assert!(
            background
                .iter()
                .all(|event| matches!(event, AgentEvent::ItemCompleted(_))),
            "{background:#?}"
        );
        assert!(matches!(completed_last(&events, "call_68_0"),
            ItemContent::CommandExecution { output, status: ItemStatus::Completed, .. }
                if output == "bgdone\n"));
    }
}
