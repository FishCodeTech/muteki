use super::*;
use crate::agent_runtime::{StartRequest, access, credentials, pool};
use agent::{AgentEvent, DeltaKind, ItemContent, SessionCommand, ThreadItem, TurnStatus};
use std::{fs, path::PathBuf};

pub(super) struct ResultText {
    pub text: String,
    pub events: u64,
    pub session_id: String,
    pub error: String,
}

impl ResultText {
    fn failed(session_id: String, error: impl Into<String>) -> Self {
        Self { text: String::new(), events: 0, session_id, error: error.into() }
    }
}

pub(super) fn selected(state: &AppState, config: &Value) -> Option<String> {
    if let Some(adapter) = config["adapter_id"].as_str().filter(|value| !value.is_empty()) {
        return Some(adapter.to_owned());
    }
    let account_id = config["credential_id"].as_str()?.strip_prefix("account:")?;
    let account = state.store.get("credential_accounts", account_id).ok().flatten()?;
    let engine = account["engine"].as_str()?;
    crate::agent_runtime::descriptors::find(engine)
        .and_then(|descriptor| descriptor.adapter(""))
        .map(|adapter| adapter.adapter_id.clone())
}

pub(super) async fn run(
    state: &Arc<AppState>,
    competition_id: &str,
    config: &Value,
    prompt: &str,
    projection: &Value,
    adapter: &str,
) -> ResultText {
    if pool::transport(adapter).is_ok() {
        return native(state, competition_id, config, prompt, adapter).await;
    }
    if crate::extensions::ready_capabilities(state, "external-agent-adapter")
        .ok()
        .is_some_and(|rows| rows.iter().any(|entry| entry["id"] == adapter))
    {
        return extension(state, competition_id, config, prompt, projection, adapter).await;
    }
    ResultText::failed(String::new(), format!("Advisor adapter {adapter} is unavailable"))
}

async fn native(
    state: &Arc<AppState>,
    competition_id: &str,
    config: &Value,
    prompt: &str,
    adapter: &str,
) -> ResultText {
    let session_id = format!("asess-{}", uuid::Uuid::new_v4().simple());
    let result = native_turn(state, competition_id, config, prompt, adapter, &session_id).await;
    match result {
        Ok(mut result) => { result.session_id = session_id; result }
        Err(error) => ResultText::failed(session_id, error),
    }
}

async fn native_turn(
    state: &Arc<AppState>,
    competition_id: &str,
    config: &Value,
    prompt: &str,
    adapter: &str,
    session_id: &str,
) -> Result<ResultText, String> {
    let (engine, _, _) = pool::transport(adapter).map_err(|error| error.message)?;
    let credential_id = config["credential_id"].as_str().unwrap_or("");
    let model = config["model"].as_str().filter(|value| !value.is_empty());
    let resolved = credentials::resolve(&state.store, &native_chat::state_dir(state),
        engine, credential_id, model).await.map_err(|error| error.message)?;
    let instance_id = config["instance_id"].as_str().filter(|value| !value.is_empty())
        .unwrap_or("default");
    let instance = crate::runtime_api::instance(&state.store, adapter, instance_id)
        .unwrap_or(Value::Null);
    let mut env = resolved.env;
    for (key, value) in instance["env"].as_object().into_iter().flatten() {
        if let Some(value) = value.as_str() { env.push((key.clone(), value.to_owned())); }
    }
    env.extend(credentials::env_refs_with_platform(state, &instance["env_refs"])
        .map_err(|error| error.message)?);
    env.extend(crate::runtime_api::transport_env(adapter,&instance["transport"],
        instance["adapter_endpoint"].as_str().or_else(||instance["endpoint"].as_str()).unwrap_or("")));
    let mut extra_args: Vec<String> = instance["launch_args"].as_array().into_iter().flatten()
        .chain(instance["extra_args"].as_array().into_iter().flatten())
        .filter_map(Value::as_str).map(str::to_owned).collect();
    extra_args.extend(crate::runtime_api::transport_args(adapter, &instance["transport"]));
    let workspace = native_chat::state_dir(state).join("competition-advisor").join(session_id);
    fs::create_dir_all(&workspace).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    let pool_key = format!("competition-advisor:{competition_id}:{session_id}");
    let request = StartRequest {
        thread_id: pool_key.clone(), adapter_id: adapter.to_owned(),
        instance_id: instance_id.to_owned(), model: model.map(str::to_owned),
        access_mode: access::SUPERVISED.to_owned(), credential_id: resolved.credential_id,
        cwd: workspace.clone(),
        binary_override: instance["binary_path"].as_str().filter(|value| !value.is_empty()).map(PathBuf::from),
        env, home: instance["home"].as_str().filter(|value| !value.is_empty()).map(PathBuf::from),
        extra_args, resume: None, fork: false, instructions: None, mcp_servers: Vec::new(),
    };
    let started = state.agents.start(request).await.map_err(|error| error.message);
    let live_session = started.as_ref().ok().map(|(session, _)| session.clone());
    let result = match started {
        Ok((session, events)) => {
            match session.send(SessionCommand::SendTurn {
                delivery_id: 1, text: prompt.to_owned(), options: None, attachments: Vec::new(),
            }).await {
                Ok(()) => tokio::time::timeout(Duration::from_secs(120), async {
                    let mut text = String::new();
                    let mut count = 0;
                    let mut error = String::new();
                    loop {
                        let event = match events.recv().await {
                            Ok(event) => event,
                            Err(_) => {
                                if error.is_empty() { error = "Advisor event stream closed".to_owned(); }
                                return Ok(ResultText { text, events: count,
                                    session_id: String::new(), error });
                            }
                        };
                        count += 1;
                        session.observe(&event);
                        let Some(event)=event.into_root_event()else{continue};
                        match event {
                            AgentEvent::Delta { kind: DeltaKind::AssistantText, text: chunk, .. } => text.push_str(&chunk),
                            AgentEvent::ItemCompleted(ThreadItem {
                                content: ItemContent::AssistantMessage { text: completed },
                                parent_item_id: None, ..
                            }) => text = completed,
                            AgentEvent::ApprovalRequested(request) => {
                                session.send(SessionCommand::RespondApproval {
                                    request_id: request.id,
                                    decision: agent::ApprovalDecision::Cancel,
                                }).await.map_err(|error| error.message)?;
                            }
                            AgentEvent::UserInputRequested { .. } => {
                                return Err("Advisor requested user input".to_owned());
                            }
                            AgentEvent::TurnFailed { code, .. } => error = code,
                            AgentEvent::ProviderStartFailed { error: failure } => error = failure,
                            AgentEvent::Error { message, fatal: true } => error = message,
                            AgentEvent::TurnCompleted { status, .. } => {
                                if status != TurnStatus::Completed && error.is_empty() {
                                    error = "turn_failed".to_owned();
                                }
                                return Ok(ResultText { text, events: count,
                                    session_id: String::new(), error });
                            }
                            AgentEvent::SessionClosed { reason } => {
                                if error.is_empty() {
                                    error = reason.unwrap_or_else(|| "Advisor session closed".to_owned());
                                }
                                return Ok(ResultText { text, events: count,
                                    session_id: String::new(), error });
                            }
                            _ => {}
                        }
                    }
                }).await.map_err(|_| "Advisor session timed out".to_owned())?,
                Err(error) => Err(error.message),
            }
        }
        Err(error) => Err(error),
    };
    if let Some(session) = live_session {
        let _ = session.shutdown_and_wait(Duration::from_secs(10)).await;
        state.agents.remove_if(&pool_key, &session).await;
    }
    let _ = fs::remove_dir_all(workspace);
    result
}

async fn extension(
    state: &Arc<AppState>,
    competition_id: &str,
    config: &Value,
    prompt: &str,
    projection: &Value,
    adapter: &str,
) -> ResultText {
    let session_id = format!("asess-{}", uuid::Uuid::new_v4().simple());
    let result = extension_turn(state, competition_id, config, prompt, projection,
        adapter, &session_id).await;
    match result {
        Ok(mut result) => { result.session_id = session_id; result }
        Err(error) => ResultText::failed(session_id, error),
    }
}

async fn extension_turn(
    state: &Arc<AppState>,
    competition_id: &str,
    config: &Value,
    prompt: &str,
    projection: &Value,
    adapter: &str,
    session_id: &str,
) -> Result<ResultText, String> {
    let instance_id = config["instance_id"].as_str().filter(|value| !value.is_empty())
        .unwrap_or("default");
    let workspace = native_chat::state_dir(state).join("competition-advisor").join(session_id);
    fs::create_dir_all(&workspace).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    let request = json!({"schema_version":1,"agent_session_id":session_id,
        "thread_id":format!("competition-advisor:{competition_id}:{session_id}"),
        "workspace_id":competition_id,"resume_handle":null,"model":config["model"],
        "access_mode":"supervised","interaction_mode":"default",
        "options":{"schema_version":1,"principal_id":"competition.advisor",
            "thread_mode":"conversation","cwd":workspace}});
    let started = crate::extensions::invoke_capability(state, "external-agent-adapter", adapter,
        "start", json!({"request":request,"injection_plan":null})).await;
    let started = match started {
        Ok(started) if !started["error"].is_object() => started,
        Ok(started) => {
            let _ = fs::remove_dir_all(workspace);
            return Err(started["error"]["code"].as_str().unwrap_or("extension.start_failed").to_owned());
        }
        Err(error) => {
            let _ = fs::remove_dir_all(workspace);
            return Err(error);
        }
    };
    let reference = json!({"schema_version":1,"agent_session_id":session_id,
        "adapter_id":adapter,"external_session_id":started["external_session_id"],
        "runtime_instance_id":instance_id,"resume_handle":started["resume_handle"]});
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        let input = json!({"schema_version":1,"kind":"message","text":prompt,
            "payload":{"schema_version":1,"projection":projection}});
        let sent = crate::extensions::invoke_capability(state, "external-agent-adapter", adapter,
            "send", json!({"session":reference,"input":input})).await?;
        if sent["error"].is_object() {
            return Err(sent["error"]["code"].as_str().unwrap_or("extension.send_failed").to_owned());
        }
        let events = sent["events"].as_array()
            .ok_or_else(|| "Advisor adapter send response lacks events[]".to_owned())?;
        let mut text = String::new();
        let mut error = String::new();
        for event in events {
            let payload = &event["payload"];
            match event["event_type"].as_str().unwrap_or("") {
                "message.delta" if payload["role"] != "user" => {
                    text.push_str(payload["text"].as_str()
                        .or_else(|| payload["delta"].as_str()).unwrap_or(""));
                }
                "message.completed" if payload["role"] != "user" => {
                    let completed = payload["text"].as_str()
                        .or_else(|| payload["content"].as_str()).unwrap_or("");
                    if !completed.is_empty() { text = completed.to_owned(); }
                }
                "runtime.error" | "turn.failed" => {
                    error = payload["code"].as_str().unwrap_or("turn_failed").to_owned();
                }
                _ => {}
            }
        }
        Ok(ResultText { text, events: events.len() as u64,
            session_id: String::new(), error })
    }).await.unwrap_or_else(|_| Err("Advisor session timed out".to_owned()));
    let _ = crate::extensions::invoke_capability(state, "external-agent-adapter", adapter,
        "close", json!({"session":reference})).await;
    let _ = fs::remove_dir_all(workspace);
    result
}
