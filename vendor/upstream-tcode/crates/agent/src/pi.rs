//! Native pi provider: persistent `pi --mode rpc` JSONL transport.
//!
//! RPC records are framed by LF only. Extension confirmation UI is transported generically.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

use serde_json::{Value, json};
use crate::{LaterHistory, RewindFailure, RewindFailureReason};
use smol::channel::{Receiver, Sender};

#[cfg(test)]
use crate::TurnStatus;
use crate::actor::{self, EventSenderExt as _, SessionActor, TransportOutcome};
use crate::process::{ChildOutput, send_json as write_json, spawn_line_reader};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, Attachment, DeltaKind, FileChange, FileChangeKind, ItemContent, ItemStatus,
    LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection, ProviderCommand, ProviderCommandKind,
    ProviderKind, ResumeCursor, SelectOption, SessionCommand, SessionHandle, SessionOptions,
    ThreadItem, TokenUsage, UserInputDelivery, UserInputOption, UserInputQuestion,
};

pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    if opts.fork {
        return Err(AgentError::Protocol(
            "session fork is not supported for this provider".into(),
        ));
    }
    crate::spawn_session(
        ProviderKind::Pi,
        opts,
        run_actor,
        "pi actor exited before reporting startup status",
    )
    .await
}

pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    crate::process::unblock(move || list_models_blocking(binary_path.as_deref(), &launch_env)).await
}

fn list_models_blocking(
    binary_path: Option<&Path>,
    launch_env: &LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    let binary = crate::resolve_binary(binary_path, "pi")?;
    let mut cmd = crate::process::command(&binary);
    cmd.arg("--mode")
        .arg("rpc")
        .arg("--no-session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in launch_env.pairs(ProviderKind::Pi) {
        cmd.env(key, value);
    }
    let mut child = cmd
        .spawn()
        .map_err(|err| AgentError::Spawn(format!("spawning `{}`: {err}", binary.display())))?;
    let mut stdin = BufWriter::new(
        child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Spawn("pi child stdin missing".into()))?,
    );
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Spawn("pi child stdout missing".into()))?;
    send_json(&mut stdin, &json!({"id":"state","type":"get_state"}))?;
    send_json(
        &mut stdin,
        &json!({"id":"models","type":"get_available_models"}),
    )?;

    let mut current = None;
    let mut thinking_level = None;
    let mut catalog = None;
    let mut lines = BufReader::new(stdout).lines();
    while current.is_none() || catalog.is_none() {
        let line = lines.next().transpose()?.ok_or_else(|| {
            AgentError::Protocol("pi closed stdout during model discovery".into())
        })?;
        let message: Value = serde_json::from_str(&line)
            .map_err(|err| AgentError::Protocol(format!("invalid pi model response: {err}")))?;
        match message.get("id").and_then(Value::as_str) {
            Some("state") => {
                ensure_success(&message)?;
                current = Some(model_wire_id(message.pointer("/data/model")));
                thinking_level = message
                    .pointer("/data/thinkingLevel")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("models") => {
                ensure_success(&message)?;
                catalog = Some(
                    message
                        .pointer("/data/models")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            _ => {}
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let current = current.flatten();
    Ok(catalog
        .unwrap_or_default()
        .iter()
        .filter_map(|model| map_model(model, current.as_deref(), thinking_level.as_deref()))
        .collect())
}

fn map_model(
    model: &Value,
    current: Option<&str>,
    thinking_level: Option<&str>,
) -> Option<ModelSpec> {
    let id = model.get("id")?.as_str()?;
    let provider = model.get("provider")?.as_str()?;
    let wire_id = format!("{provider}/{id}");
    let mut options = Vec::new();
    if model
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        // Pi uses provider defaults for missing keys; null disables a level.
        // Extended levels require an explicit mapping (getSupportedThinkingLevels).
        let levels: Vec<_> = ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .filter(|level| {
                let mapped = model
                    .get("thinkingLevelMap")
                    .and_then(|map| map.get(*level));
                if matches!(*level, "xhigh" | "max") {
                    mapped.is_some_and(Value::is_string)
                } else {
                    !mapped.is_some_and(Value::is_null)
                }
            })
            .collect();
        let default_value = thinking_level
            .filter(|level| levels.contains(level))
            .map(str::to_owned);
        options.push(OptionDescriptor::Select {
            id: "reasoningEffort".into(),
            label: "Thinking".into(),
            options: levels
                .into_iter()
                .map(|level| SelectOption {
                    value: level.into(),
                    label: thinking_label(level),
                    description: None,
                    unavailable: None,
                })
                .collect(),
            default_value,
            role: crate::OptionRole::Model,
            apply: crate::ApplyTiming::Live,
            recommended: None,
            permissive: None,
        });
    }
    Some(ModelSpec {
        id: wire_id.clone(),
        display_name: model
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned(),
        is_default: current == Some(wire_id.as_str()),
        options,
    })
}

fn thinking_label(level: &str) -> String {
    if level == "xhigh" {
        return "Extra High".into();
    }
    let mut characters = level.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
        .unwrap_or_default()
}

fn model_wire_id(model: Option<&Value>) -> Option<String> {
    let model = model?;
    Some(format!(
        "{}/{}",
        model.get("provider")?.as_str()?,
        model.get("id")?.as_str()?
    ))
}

async fn run_actor(
    opts: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<AgentEvent>,
    ready: Sender<Result<(), AgentError>>,
) {
    let binary = match crate::resolve_binary(opts.binary_path.as_deref(), "pi") {
        Ok(binary) => binary,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    let mut cmd = crate::process::command(&binary);
    cmd.args(&opts.extra_args).arg("--mode").arg("rpc");
    if let Some(instructions) = &opts.instructions {
        cmd.args(["--append-system-prompt", instructions]);
    }
    if let Some(thinking) = selected_thinking(&opts.option_selections) {
        cmd.arg("--thinking").arg(thinking);
    }
    if let Some(session) = resume_session(&opts.resume) {
        cmd.arg("--session").arg(session);
    }
    cmd.current_dir(&opts.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A Pi turn can have live shell/curl descendants. The source CLI worker
    // launches a process group so dropping one Step kills that whole group.
    #[cfg(unix)]
    cmd.process_group(0);
    for (key, value) in opts.launch_env.pairs(ProviderKind::Pi) {
        cmd.env(key, value);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = ready
                .send(Err(AgentError::Spawn(format!(
                    "spawning `{}`: {err}",
                    binary.display()
                ))))
                .await;
            return;
        }
    };
    let _=events.try_send(AgentEvent::ProcessStarted{pid:child.id()});
    let stdin = match child.stdin.take() {
        Some(stdin) => BufWriter::new(stdin),
        None => {
            let _ = ready
                .send(Err(AgentError::Spawn("pi child stdin missing".into())))
                .await;
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = ready
                .send(Err(AgentError::Spawn("pi child stdout missing".into())))
                .await;
            return;
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = ready
                .send(Err(AgentError::Spawn("pi child stderr missing".into())))
                .await;
            return;
        }
    };
    let (line_rx, _) = spawn_line_reader(stdout, "pi-rpc-stdout", None, true);
    let stderr_tail = crate::process::StderrTail::default();
    let _ = stderr_tail.spawn(stderr, "pi-rpc-stderr", "pi");

    let profile_value = |key: &str| {
        opts.launch_env.pairs(ProviderKind::Pi).into_iter().rev()
            .find(|(name, value)| name == key && !value.trim().is_empty())
            .map(|(_, value)| value)
    };
    let mut actor = PiActor {
        can_fork:false,
        user_baseline:Vec::new(),
        child,
        stdin,
        lines: line_rx,
        events,
        mapper: PiMapper::new(),
        next_request: 1,
        pending_approvals: HashSet::new(),
        pending_dialogs: HashSet::new(),
        pending_steers: HashMap::new(),
        deferred_lines:VecDeque::new(),
        requested_model: opts.model.clone().or_else(|| profile_value("MUTEKI_PI_MODEL")),
        model_provider: profile_value("MUTEKI_PI_PROVIDER"),
        stderr_tail,
    };
    let startup = actor.initialize().await;
    if let Err(err) = startup {
        actor.stop();
        let details = actor.stderr_tail.append_to(err.to_string(), "\nstderr:\n");
        let _ = ready.send(Err(AgentError::Provider(details))).await;
        return;
    }
    let unattached = unattached_servers(&opts);
    if !unattached.is_empty() {
        actor.events
            .emit(AgentEvent::Warning {
                message: format!(
                    "pi has no MCP client of its own, so Tcode's {} tools are unavailable in this session",
                    unattached.join(", ")
                ),
            })
            .await;
    }
    if ready.send(Ok(())).await.is_err() {
        actor.stop();
        return;
    }

    actor.flush_deferred().await;
    actor::run(actor, &commands).await;
}

struct PiActor {
    can_fork:bool,
    user_baseline:Vec<String>,
    child: Child,
    stdin: BufWriter<ChildStdin>,
    lines: Receiver<ChildOutput>,
    events: Sender<AgentEvent>,
    mapper: PiMapper,
    next_request: u64,
    pending_approvals: HashSet<String>,
    pending_dialogs: HashSet<String>,
    /// Native prompt request id -> canonical steering request id. pi's prompt
    /// response is only an acceptance signal, but it is stronger than merely
    /// acknowledging the stdin write.
    pending_steers: HashMap<String, String>,
    deferred_lines:VecDeque<String>,
    requested_model: Option<String>,
    model_provider: Option<String>,
    stderr_tail: crate::process::StderrTail,
}

impl SessionActor for PiActor {
    type TransportItem = ChildOutput;

    fn transport(&self) -> &Receiver<Self::TransportItem> {
        &self.lines
    }

    fn events(&self) -> &Sender<AgentEvent> {
        &self.events
    }

    fn command_failure_reason(&self) -> &'static str {
        "pi protocol write failed"
    }

    async fn handle_command(&mut self,command:SessionCommand)->Result<(),String>{
        let result=self.apply_command(command).await;self.flush_deferred().await;result
    }

    async fn handle_transport(
        &mut self,
        item: Result<ChildOutput, smol::channel::RecvError>,
    ) -> TransportOutcome {
        match item {
            Ok(ChildOutput::Line(line)) => {
                self.handle_line(&line).await;
                self.flush_deferred().await;
                TransportOutcome::Continue
            }
            Ok(ChildOutput::Error(err)) => TransportOutcome::Fatal(err),
            Ok(ChildOutput::Eof) | Err(_) => {
                TransportOutcome::Closed("pi RPC process closed stdout".into())
            }
        }
    }

    async fn settle_shutdown(&mut self) {
        let _ = self.cancel_pending_ui();
    }

    async fn teardown(mut self, reason: Option<String>) -> Option<String> {
        self.stop();
        reason.map(|base| self.stderr_tail.append_to(base, "\nstderr:\n"))
    }
}

impl PiActor {
    async fn apply_command(&mut self, command: SessionCommand) -> Result<(), String> {
        if let SessionCommand::SetOption{id,value}=&command && id=="muteki:operation" {
            let request_id=value["request_id"].as_str().unwrap_or("").to_owned();
            let result=if self.mapper.current_turn.is_some(){Err("Pi native turn is active".into())}else{
                match crate::native_rpc_operation("pi",value["name"].as_str().unwrap_or(""),value["arguments"].as_str().unwrap_or("")){
                    Ok((method,params))=>self.rpc(method,params).await.map(|data|crate::native_rpc_result(method,data)).map_err(|e|e.to_string()),Err(error)=>Err(error)
                }
            };
            self.events.emit(match result{Ok(result)=>AgentEvent::RuntimeOperationResult{request_id,result:Some(result),error:None},Err(error)=>AgentEvent::RuntimeOperationResult{request_id,result:None,error:Some(error)}}).await;return Ok(());
        }
        match command {
            SessionCommand::SendTurn {
                delivery_id,
                text,
                attachments,
                ..
            } => {
                if self.can_fork {self.user_baseline=self.entries().await.unwrap_or_default();}
                let id = self.request_id();
                let mut request = json!({"id":id,"type":"prompt","message":text});
                if self.mapper.current_turn.is_some() {
                    request["streamingBehavior"] = json!("followUp");
                }
                attach_images(&mut request, attachments);
                send_json(&mut self.stdin, &request).map_err(|err| err.to_string())?;
                self.events
                    .emit(AgentEvent::TurnAccepted { delivery_id })
                    .await;
                Ok(())
            }
            SessionCommand::Steer {
                request_id,
                text,
                attachments,
            } => {
                let native_id = self.request_id();
                let mut request = json!({
                    "id": native_id,
                    "type":"prompt",
                    "message":text,
                    "streamingBehavior":"steer"
                });
                attach_images(&mut request, attachments);
                send_json(&mut self.stdin, &request).map_err(|err| err.to_string())?;
                self.pending_steers.insert(native_id, request_id);
                Ok(())
            }
            SessionCommand::Interrupt => {
                self.mapper.interrupt_pending = true;
                self.cancel_pending_ui()?;
                send_json(&mut self.stdin, &json!({"type":"abort"})).map_err(|err| err.to_string())
            }
            SessionCommand::RespondApproval {
                request_id,
                decision,
            } => {
                let confirmed = match &decision {
                    ApprovalDecision::Option(id) if id == "confirm" => true,
                    ApprovalDecision::Option(id) if id == "decline" => false,
                    ApprovalDecision::Cancel => false,
                    ApprovalDecision::Option(_) => return Ok(()),
                };
                if !self.pending_approvals.remove(&request_id) {
                    return Ok(());
                }
                send_json(
                    &mut self.stdin,
                    &json!({"type":"extension_ui_response","id":request_id,"confirmed":confirmed}),
                )
                .map_err(|err| err.to_string())?;
                if decision == ApprovalDecision::Cancel {
                    self.mapper.interrupt_pending = true;
                    self.cancel_pending_ui()?;
                    send_json(&mut self.stdin, &json!({"type":"abort"}))
                        .map_err(|err| err.to_string())?;
                }
                self.events
                    .emit(AgentEvent::ApprovalResolved {
                        request_id,
                        decision,
                    })
                    .await;
                Ok(())
            }
            SessionCommand::SetOption { id, value } if id == "reasoningEffort" => {
                let Some(level) = value.as_str() else {
                    return Ok(());
                };
                let request_id = self.request_id();
                send_json(
                    &mut self.stdin,
                    &json!({"id":request_id,"type":"set_thinking_level","level":level}),
                )
                .map_err(|err| err.to_string())
            }
            SessionCommand::RespondUserInput {
                request_id,
                answers,
            } => {
                let Some(response) = take_extension_dialog_response(
                    &mut self.pending_dialogs,
                    &request_id,
                    &answers,
                ) else {
                    return Ok(());
                };
                send_json(&mut self.stdin, &response).map_err(|err| err.to_string())?;
                self.events.emit(AgentEvent::UserInputResolved{request_id,answers}).await;
                Ok(())
            }
            SessionCommand::Rewind { checkpoint_id,mode } => {
                let result=if !self.can_fork || self.mapper.current_turn.is_some(){Err(AgentError::Protocol("Pi native rewind is unavailable while active or unadvertised".into()))}else{self.rewind_entry(&checkpoint_id).await};
                self.events.emit(match result{Ok(prefill)=>AgentEvent::RewindCompleted{checkpoint_id,mode,prefill,prefill_attachments:Vec::new(),later_history:LaterHistory::Branch,files:None,incomplete:None,undo_token:None},Err(error)=>AgentEvent::RewindFailed{checkpoint_id,mode,failure:RewindFailure::new(RewindFailureReason::Other,Some(error.to_string()))}}).await;Ok(())
            }
            // Progress arrives as `compaction_start` / `compaction_end`.
            SessionCommand::Compact => send_json(
                &mut self.stdin,
                &json!({"id":"tcode-compact","type":"compact"}),
            )
            .map_err(|err| err.to_string()),
            SessionCommand::PreviewRewind { checkpoint_id, mode } => {
                self.events.emit(AgentEvent::RewindPreviewed {
                    checkpoint_id,
                    mode,
                    files: None,
                    reason: None,
                }).await;
                Ok(())
            }
            SessionCommand::UndoRewind { checkpoint_id, undo_token } => {
                let req_id = self.request_id();
                let res = send_json(
                    &mut self.stdin,
                    &json!({"id": req_id, "type": "switch_session", "sessionPath": undo_token}),
                );
                match res {
                    Ok(()) => self.events.emit(AgentEvent::RewindUndone { checkpoint_id }).await,
                    Err(err) => self.events.emit(AgentEvent::RewindUndoFailed {
                        checkpoint_id,
                        failure: RewindFailure::new(RewindFailureReason::Other, Some(err.to_string())),
                    }).await,
                }
                Ok(())
            }
            SessionCommand::StopBackgroundTask { .. }
            | SessionCommand::RetryTurn { .. }
            | SessionCommand::SetOption { .. }
            | SessionCommand::SetGoal { .. }
            | SessionCommand::ClearGoal
            | SessionCommand::Shutdown => Ok(()),
        }
    }

    async fn flush_deferred(&mut self){while let Some(line)=self.deferred_lines.pop_front(){self.handle_line(&line).await;}}
    async fn rpc(&mut self,command:&str,params:Value)->Result<Value,AgentError>{
        let id=self.request_id();let mut request=params;request["id"]=json!(id);request["type"]=json!(command);
        send_json(&mut self.stdin,&request)?;
        let timeout=if command=="compact"{180}else{30};
        smol::future::race(self.wait_response_inner(&id),async move{smol::Timer::after(std::time::Duration::from_secs(timeout)).await;Err(AgentError::Protocol("Pi RPC response timed out".into()))}).await.map(|v|v["data"].clone())
    }
    async fn entries(&mut self)->Result<Vec<String>,AgentError>{
        let data=self.rpc("get_entries",json!({})).await?;
        let entries=data["entries"].as_array().ok_or_else(||AgentError::Protocol("Pi get_entries omitted entries".into()))?;
        let by_id=entries.iter().filter_map(|entry|entry["id"].as_str().map(|id|(id,entry))).collect::<HashMap<_,_>>();
        let mut cursor=data["leafId"].as_str().or_else(||data["lastEntryId"].as_str());let mut seen=HashSet::new();let mut users=Vec::new();
        while let Some(id)=cursor{if !seen.insert(id){return Err(AgentError::Protocol("Pi entry tree contains a cycle".into()));}let Some(entry)=by_id.get(id)else{break};if entry["type"]=="message" && entry["message"]["role"]=="user"{users.push(id.to_owned());}cursor=entry["parentId"].as_str();}
        users.reverse();Ok(users)
    }
    async fn rewind_entry(&mut self,entry:&str)->Result<Option<String>,AgentError>{
        if !self.entries().await?.iter().any(|id|id==entry){return Err(AgentError::Protocol("Pi entry is absent from the active branch".into()));}
        let result=self.rpc("fork",json!({"entryId":entry})).await?;
        if result["cancelled"]==true{return Err(AgentError::Protocol("Pi extension cancelled fork".into()));}
        let state=self.rpc("get_state",json!({})).await?;
        if let Some(window) = crate::json_u64(state.pointer("/model/contextWindow")) {
            self.mapper.context_window = Some(window);
        }
        let file=state["sessionFile"].as_str().filter(|v|!v.is_empty()).ok_or_else(||AgentError::Protocol("Pi fork returned no persisted session file".into()))?;
        let session=state["sessionId"].as_str().unwrap_or(file);
        self.events.emit(AgentEvent::SessionStarted{provider_session_id:session.into(),resume:ResumeCursor(json!({"session_id":session,"session_file":file})),model:model_wire_id(state.get("model"))}).await;
        self.user_baseline=self.entries().await?;
        Ok(result["text"].as_str().map(str::to_owned))
    }

    async fn initialize(&mut self) -> Result<(), AgentError> {
        if let Some(model) = self.requested_model.clone() {
            let (provider, model_id) = if let Some(provider) = &self.model_provider {
                let prefix = format!("{provider}/");
                let model_id = model.strip_prefix(&prefix).unwrap_or(&model);
                (provider.clone(), model_id.to_owned())
            } else if let Some((provider, model_id)) = model.split_once('/') {
                (provider.to_owned(), model_id.to_owned())
            } else {
                send_json(
                    &mut self.stdin,
                    &json!({"id":"tcode-models","type":"get_available_models"}),
                )?;
                let available = self.wait_response("tcode-models").await?;
                let providers: HashSet<String> = available
                    .pointer("/data/models")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|entry| entry["id"] == model)
                    .filter_map(|entry| entry["provider"].as_str().map(str::to_owned))
                    .collect();
                if providers.len() != 1 {
                    return Err(AgentError::Protocol(format!(
                        "pi model `{model}` needs an unambiguous provider/model selection"
                    )));
                }
                (providers.into_iter().next().unwrap(), model.to_owned())
            };
            if provider.trim().is_empty() || model_id.trim().is_empty() {
                return Err(AgentError::Protocol(
                    "pi model provider and id must not be empty".into(),
                ));
            }
            send_json(
                &mut self.stdin,
                &json!({
                    "id":"tcode-model",
                    "type":"set_model",
                    "provider":provider,
                    "modelId":model_id
                }),
            )?;
            self.wait_response("tcode-model").await?;
        }
        send_json(
            &mut self.stdin,
            &json!({"id":"tcode-state","type":"get_state"}),
        )?;
        let state = self.wait_response("tcode-state").await?;
        let data = state.get("data").unwrap_or(&Value::Null);
        self.mapper.context_window = crate::json_u64(data.pointer("/model/contextWindow"));
        let session_id = data
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::Protocol("pi get_state omitted sessionId".into()))?
            .to_owned();
        let mut cursor = serde_json::Map::new();
        cursor.insert("session_id".into(), Value::String(session_id.clone()));
        if let Some(path) = data.get("sessionFile").and_then(Value::as_str) {
            cursor.insert("session_file".into(), Value::String(path.to_owned()));
        }
        let model = model_wire_id(data.get("model"));
        self.events
            .emit(AgentEvent::SessionStarted {
                provider_session_id: session_id,
                resume: ResumeCursor(Value::Object(cursor)),
                model,
            })
            .await;

        send_json(
            &mut self.stdin,
            &json!({"id":"tcode-commands","type":"get_commands"}),
        )?;
        let commands = match self.wait_response("tcode-commands").await {
            Ok(response) => map_commands(&response),
            Err(err) => {
                log::debug!("pi get_commands unavailable: {err}");
                Vec::new()
            }
        };
        self.events.emit(AgentEvent::ProviderCommands { commands }).await;
        self.can_fork=self.rpc("get_fork_messages",json!({})).await.is_ok_and(|data|data["messages"].is_array());
        self.user_baseline=self.entries().await.unwrap_or_default();
        self.events.emit(AgentEvent::RuntimeCapabilities{capabilities:json!({"native_rewind":self.can_fork,"plan":false})}).await;
        Ok(())
    }

    async fn wait_response(&mut self,id:&str)->Result<Value,AgentError>{
        smol::future::race(self.wait_response_inner(id),async{smol::Timer::after(std::time::Duration::from_secs(30)).await;Err(AgentError::Protocol("Pi RPC response timed out".into()))}).await
    }
    async fn wait_response_inner(&mut self, id: &str) -> Result<Value, AgentError> {
        loop {
            match self.lines.recv().await {
                Ok(ChildOutput::Line(line)) => {
                    let message: Value = serde_json::from_str(&line).map_err(|err| {
                        AgentError::Protocol(format!("invalid pi JSON response: {err}"))
                    })?;
                    if message.get("id").and_then(Value::as_str) == Some(id) {
                        ensure_success(&message)?;
                        return Ok(message);
                    }
                    // RPC replies and live notifications share stdout. Preserve every
                    // unrelated record until the current RPC boundary is complete.
                    self.deferred_lines.push_back(line);
                }
                Ok(ChildOutput::Error(err)) => return Err(AgentError::Protocol(err)),
                Ok(ChildOutput::Eof) | Err(_) => {
                    return Err(AgentError::Protocol(
                        "pi closed stdout during startup handshake".into(),
                    ));
                }
            }
        }
    }

    async fn handle_line(&mut self, line: &str) {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(err) => {
                self.events
                    .emit(AgentEvent::Warning {
                        message: format!("ignored invalid pi RPC record: {err}"),
                    })
                    .await;
                return;
            }
        };
        if message.get("type").and_then(Value::as_str) == Some("response") {
            let success = message
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if success {
                if let Some(request_id) = message
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| self.pending_steers.remove(id))
                {
                    self.events
                        .emit(AgentEvent::SteerAccepted { request_id })
                        .await;
                }
            } else {
                self.events
                    .emit(AgentEvent::Error {
                        message: response_error(&message),
                        fatal: false,
                    })
                    .await;
            }
            return;
        }
        if message.get("type").and_then(Value::as_str) == Some("extension_ui_request") {
            self.handle_extension_ui(&message).await;
            return;
        }
        let events = self.mapper.on_message(&message);
        let turn_completed = events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }));
        if self.can_fork && let Some(turn)=events.iter().find_map(|event|match event{AgentEvent::TurnCompleted{turn_id,..}=>Some(turn_id.clone()),_=>None}) {
            match self.entries().await{
                Ok(entries)=>{if let Some(entry)=entries.iter().find(|id|!self.user_baseline.contains(id)){self.events.emit(AgentEvent::TurnCheckpoint{turn_id:turn,checkpoint_id:entry.clone()}).await;}self.user_baseline=entries;},
                Err(error)=>self.events.emit(AgentEvent::Warning{message:format!("Pi checkpoint unavailable: {error}")}).await,
            }
        }
        for event in events {self.events.emit(event).await;}
        if turn_completed {
            let _ = self.cancel_pending_ui();
        }
    }

    async fn handle_extension_ui(&mut self, message: &Value) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if matches!(method, "select" | "input" | "editor") {
            let Some((id, question)) = extension_dialog_question(message) else {
                self.events
                    .emit(AgentEvent::Warning {
                        message: format!("pi extension `{method}` dialog omitted its id"),
                    })
                    .await;
                return;
            };
            self.pending_dialogs.insert(id.clone());
            self.events
                .emit(AgentEvent::UserInputRequested {
                    request_id: id,
                    questions: vec![question],
                    delivery: UserInputDelivery::Blocking,
                })
                .await;
            return;
        }
        if method != "confirm" {
            if let Some(warning) = extension_warning(message) {
                self.events
                    .emit(AgentEvent::Warning { message: warning })
                    .await;
                return;
            }
            if matches!(
                method,
                "notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text"
            ) {
                return;
            }
            let id = message.get("id").and_then(Value::as_str).unwrap_or("");
            let _ = send_json(
                &mut self.stdin,
                &json!({"type":"extension_ui_response","id":id,"cancelled":true}),
            );
            self.events
                .emit(AgentEvent::Warning {
                    message: format!("pi extension requested unsupported UI method `{method}`"),
                })
                .await;
            return;
        }
        let Some(id) = message.get("id").and_then(Value::as_str) else {
            self.events
                .emit(AgentEvent::Warning {
                    message: "pi extension confirmation omitted its id".into(),
                })
                .await;
            return;
        };
        let payload: Value = message
            .get("message")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        let tool_name = payload
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        self.pending_approvals.insert(id.to_owned());
        self.events
            .emit(AgentEvent::ApprovalRequested(ApprovalRequest {native_execution: None, tool_call_id: None, 
                id: id.to_owned(),
                parent_item_id: None,
                turn_id: self.mapper.current_turn.clone(),
                kind: if payload.is_null() {
                    ApprovalKind::ToolUse {
                        name: message
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("Confirmation")
                            .to_owned(),
                        input: Value::Null,
                        detail: message
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    }
                } else {
                    approval_kind(&tool_name, &payload)
                },
                options: vec![
                    ApprovalOption {
                        id: "confirm".into(),
                        label: "Confirm".into(),
                        kind: ApprovalOptionKind::AllowOnce,
                    },
                    ApprovalOption {
                        id: "decline".into(),
                        label: "Decline".into(),
                        kind: ApprovalOptionKind::RejectOnce,
                    },
                ],
            }))
            .await;
    }

    fn request_id(&mut self) -> String {
        let id = format!("tcode-{}", self.next_request);
        self.next_request += 1;
        id
    }

    fn cancel_pending_ui(&mut self) -> Result<(), String> {
        for id in self.pending_approvals.drain() {
            send_json(
                &mut self.stdin,
                &json!({"type":"extension_ui_response","id":id,"confirmed":false}),
            )
            .map_err(|err| err.to_string())?;
        }
        cancel_pending_dialogs(&mut self.stdin, &mut self.pending_dialogs)
            .map_err(|err| err.to_string())
    }

    fn stop(&mut self) {
        stop_child(&mut self.child);
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn killpg(pgrp: i32, sig: i32) -> i32;
}

fn stop_child(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(group) = i32::try_from(child.id()) {
        // The child is the leader of its own group. A failed group signal
        // falls through to the direct-child kill below.
        let _ = unsafe { killpg(group, 9) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

struct PiMapper {
    turn_counter: u64,
    current_turn: Option<String>,
    turn_usage: Option<TokenUsage>,
    cumulative_processed: u64,
    interrupt_pending: bool,
    failed: bool,
    context_window: Option<u64>,
    last_stop_reason: Option<String>,
    compaction_failed_reason: Option<String>,
    tool_items: HashMap<String, PiTool>,
    finalized_messages: HashSet<String>,
    /// Item ids of this turn's assistant messages, keyed by the message
    /// `timestamp` (the only field present from `message_start` through
    /// `turn_end`), with a count so two messages stamped in the same
    /// millisecond still get distinct ids.
    assistant_ids: HashMap<u64, (String, u32)>,
    /// Id of the assistant message being streamed. `message_update` carries
    /// only the delta, so the id is taken from `message_start`.
    streaming_assistant: Option<String>,
}

#[derive(Clone)]
struct PiTool {
    name: String,
    input: Value,
    output: String,
}

impl PiMapper {
    fn new() -> Self {
        Self {
            turn_counter: 0,
            current_turn: None,
            turn_usage: None,
            cumulative_processed: 0,
            interrupt_pending: false,
            failed: false,
            context_window: None,
            last_stop_reason: None,
            compaction_failed_reason: None,
            tool_items: HashMap::new(),
            finalized_messages: HashSet::new(),
            assistant_ids: HashMap::new(),
            streaming_assistant: None,
        }
    }

    fn on_message(&mut self, message: &Value) -> Vec<AgentEvent> {
        match message.get("type").and_then(Value::as_str).unwrap_or("") {
            "agent_start" => self.start_turn(),
            "agent_settled" => self.complete_turn(),
            "message_start" => {
                let message = message.get("message").unwrap_or(&Value::Null);
                if message.get("role").and_then(Value::as_str) == Some("assistant") {
                    let id = self.allocate_assistant_id(message);
                    self.streaming_assistant = Some(id);
                }
                Vec::new()
            }
            "message_update" => self.message_update(message),
            "message_end" => self.message_end(message),
            // Current pi emits both message_end and turn_end. Treat turn_end as
            // a reconciliation fallback; usage is de-duplicated by message id.
            "turn_end" => self.message_end(message),
            "tool_execution_start" => self.tool_event(message, ItemStatus::InProgress, false),
            "tool_execution_update" => self.tool_event(message, ItemStatus::InProgress, true),
            "tool_execution_end" => self.tool_event(
                message,
                if message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    ItemStatus::Failed
                } else {
                    ItemStatus::Completed
                },
                true,
            ),
            "compaction_start" => vec![AgentEvent::Warning {
                message: format!(
                    "pi is compacting context ({})",
                    message
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown reason")
                ),
            }],
            "compaction_end" => {
                if message.get("result").is_some_and(|value| !value.is_null()) {
                    self.compaction_failed_reason = None;
                    vec![AgentEvent::ContextCompacted(Default::default())]
                } else {
                    let reason = message
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .unwrap_or("aborted")
                        .to_owned();
                    self.compaction_failed_reason = Some(reason.clone());
                    vec![AgentEvent::Warning {
                        message: format!("pi context compaction did not complete: {reason}"),
                    }]
                }
            }
            "auto_retry_start" => vec![AgentEvent::Warning {
                message: format!(
                    "pi retry {}/{} in {} ms: {}",
                    crate::json_u64(message.get("attempt")).unwrap_or(0),
                    crate::json_u64(message.get("maxAttempts")).unwrap_or(0),
                    crate::json_u64(message.get("delayMs")).unwrap_or(0),
                    message
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .unwrap_or("transient provider error")
                ),
            }],
            "auto_retry_end"
                if !message
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false) =>
            {
                self.failed = true;
                vec![AgentEvent::Warning {
                    message: format!(
                        "pi retry failed: {}",
                        message
                            .get("finalError")
                            .and_then(Value::as_str)
                            .unwrap_or("provider error")
                    ),
                }]
            }
            "extension_error" => vec![AgentEvent::Warning {
                message: format!(
                    "pi extension error in {}: {}",
                    message
                        .get("event")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown event"),
                    message
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                ),
            }],
            _ => Vec::new(),
        }
    }

    fn start_turn(&mut self) -> Vec<AgentEvent> {
        self.last_stop_reason = None;
        self.compaction_failed_reason = None;
        crate::start_mapped_turn(
            "pi",
            &mut self.turn_counter,
            &mut self.current_turn,
            &mut self.turn_usage,
            &mut self.failed,
        )
    }

    fn complete_turn(&mut self) -> Vec<AgentEvent> {
        self.assistant_ids.clear();
        self.finalized_messages.clear();
        self.streaming_assistant = None;
        let last_stop = self.last_stop_reason.take();
        let compact_err = self.compaction_failed_reason.take();
        let mut events = Vec::new();
        if !self.interrupt_pending && !self.failed && last_stop.as_deref() == Some("length") {
            self.failed = true;
            let message = match compact_err {
                Some(reason) => format!(
                    "Pi 响应因达到上下文或输出上限被截断，且自动压缩未完成：{reason}"
                ),
                None => "Pi 响应因达到上下文或输出上限被截断（stopReason=length），未生成完整回复；请检查模型的最大上下文/最大输出配置或手动压缩上下文后重试".to_owned(),
            };
            events.push(AgentEvent::Error {
                message,
                fatal: false,
            });
        }
        events.extend(crate::complete_mapped_turn(
            &mut self.current_turn,
            &mut self.interrupt_pending,
            self.failed,
            self.turn_usage,
        ));
        events
    }

    fn message_update(&mut self, message: &Value) -> Vec<AgentEvent> {
        let event = message.get("assistantMessageEvent").unwrap_or(&Value::Null);
        let kind = match event.get("type").and_then(Value::as_str) {
            Some("text_delta") => DeltaKind::AssistantText,
            Some("thinking_delta") => DeltaKind::ReasoningText,
            Some("toolcall_end") => {
                let tool = event.get("toolCall").unwrap_or(&Value::Null);
                let id = tool
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("pi-tool")
                    .to_owned();
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                let input = tool
                    .get("arguments")
                    .or_else(|| tool.get("input"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let existed = self
                    .tool_items
                    .insert(
                        id.clone(),
                        PiTool {
                            name: name.clone(),
                            input: input.clone(),
                            output: String::new(),
                        },
                    )
                    .is_some();
                let item = tool_item(&id, &name, input, String::new(), ItemStatus::InProgress);
                return vec![if existed {
                    AgentEvent::ItemUpdated(item)
                } else {
                    AgentEvent::ItemStarted(item)
                }];
            }
            Some("error") => {
                self.failed = event.get("reason").and_then(Value::as_str) != Some("aborted");
                return vec![AgentEvent::Error {
                    message: event
                        .get("error")
                        .or_else(|| event.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("pi assistant stream failed")
                        .to_owned(),
                    fatal: false,
                }];
            }
            _ => return Vec::new(),
        };
        let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
        if delta.is_empty() {
            return Vec::new();
        }
        let index = crate::json_u64(event.get("contentIndex")).unwrap_or(0);
        let message_id = match &self.streaming_assistant {
            Some(id) => id.clone(),
            None => self.assistant_id(message.get("message").unwrap_or(&Value::Null)),
        };
        vec![AgentEvent::Delta {
            item_id: format!("{message_id}:{index}"),
            kind,
            text: delta.to_owned(),
        }]
    }

    fn message_end(&mut self, event: &Value) -> Vec<AgentEvent> {
        let message = event.get("message").unwrap_or(&Value::Null);
        match message.get("role").and_then(Value::as_str) {
            // The app records the canonical user message before sending the
            // prompt. pi echoes it back; mapping that echo would duplicate the
            // user bubble, as with Codex/OpenCode.
            Some("user") => Vec::new(),
            Some("custom") if message.get("display").and_then(Value::as_bool) == Some(true) => {
                let summary = match message.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                if summary.trim().is_empty() {
                    return Vec::new();
                }
                let provider_kind = message
                    .get("customType")
                    .and_then(Value::as_str)
                    .filter(|kind| !kind.trim().is_empty())
                    .unwrap_or("pi-extension")
                    .to_owned();
                vec![AgentEvent::ItemCompleted(ThreadItem {
                    id: custom_message_id(message),
                    parent_item_id: None,
                    content: ItemContent::Other {
                        provider_kind,
                        summary,
                    },
                })]
            }
            Some("assistant") => {
                let mut events = Vec::new();
                let id = self.assistant_id(message);
                let first_finalization = self.finalized_messages.insert(id.clone());
                if first_finalization
                    && let Some(content) = message.get("content").and_then(Value::as_array)
                {
                    for (index, block) in content.iter().enumerate() {
                        let item = match block.get("type").and_then(Value::as_str) {
                            Some("text") => Some(ItemContent::AssistantMessage {
                                text: block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned(),
                            }),
                            Some("thinking") => Some(ItemContent::Reasoning {
                                text: block
                                    .get("thinking")
                                    .or_else(|| block.get("text"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_owned(),
                            }),
                            _ => None,
                        };
                        if let Some(content) = item {
                            events.push(AgentEvent::ItemCompleted(ThreadItem {
                                id: format!("{id}:{index}"),
                                parent_item_id: None,
                                content,
                            }));
                        }
                    }
                }
                if first_finalization && let Some(usage) = map_usage(message.get("usage")) {
                    let processed = crate::processed_tokens(usage);
                    self.cumulative_processed = self.cumulative_processed.saturating_add(processed);
                    let usage = TokenUsage {
                        freshness: crate::ContextFreshness::Current,
                        context_window: usage.context_window.or(self.context_window),
                        total_processed_tokens: Some(self.cumulative_processed),
                        ..usage
                    };
                    self.turn_usage.get_or_insert_default().merge(usage);
                    events.push(AgentEvent::TokenUsage(usage));
                    events.push(AgentEvent::UsageDetail{scope:"message".into(),usage_id:id.clone(),usage:json!({"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"cache_read_tokens":usage.cached_input_tokens,"cache_write_tokens":usage.cache_write_tokens,"reasoning_tokens":usage.reasoning_tokens,"input_includes_cache":false,"reported_cost":message["usage"]["cost"]["total"],"cost_kind":"estimated"})});
                }
                let stop_reason = message.get("stopReason").and_then(Value::as_str);
                if let Some(reason) = stop_reason {
                    self.last_stop_reason = Some(reason.to_owned());
                    if reason != "error" && reason != "length" {
                        self.compaction_failed_reason = None;
                    }
                }
                if stop_reason == Some("error") {
                    self.failed = true;
                }
                events
            }
            _ => Vec::new(),
        }
    }

    /// Names a new assistant message. pi stamps `timestamp` when it creates
    /// the message, before the first delta, and keeps it on the final
    /// message; `responseId` only arrives with the provider response, so it
    /// cannot name the item that was streamed.
    fn allocate_assistant_id(&mut self, message: &Value) -> String {
        let timestamp = crate::json_u64(message.get("timestamp")).unwrap_or(0);
        let (id, count) = self
            .assistant_ids
            .entry(timestamp)
            .or_insert_with(|| (String::new(), 0));
        *count += 1;
        *id = if *count == 1 {
            format!("pi-assistant-{timestamp}")
        } else {
            format!("pi-assistant-{timestamp}-{count}")
        };
        id.clone()
    }

    /// Resolves a completed or reconciled assistant message to the id its
    /// deltas streamed under, naming it here if `message_start` was missed.
    fn assistant_id(&mut self, message: &Value) -> String {
        let timestamp = crate::json_u64(message.get("timestamp")).unwrap_or(0);
        match self.assistant_ids.get(&timestamp) {
            Some((id, _)) => id.clone(),
            None => self.allocate_assistant_id(message),
        }
    }

    fn tool_event(
        &mut self,
        event: &Value,
        status: ItemStatus,
        has_result: bool,
    ) -> Vec<AgentEvent> {
        let id = event
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("pi-tool")
            .to_owned();
        let event_name = event.get("toolName").and_then(Value::as_str);
        let event_input = event.get("args").filter(|value| !value.is_null()).cloned();
        let result = if event.get("type").and_then(Value::as_str) == Some("tool_execution_update") {
            event.get("partialResult")
        } else {
            event.get("result")
        };
        let output = result.map(result_text).unwrap_or_default();
        let was_known = self.tool_items.contains_key(&id);
        let tool = self.tool_items.entry(id.clone()).or_insert_with(|| PiTool {
            name: event_name.unwrap_or("tool").to_owned(),
            input: event_input.clone().unwrap_or(Value::Null),
            output: String::new(),
        });
        if let Some(name) = event_name {
            tool.name = name.to_owned();
        }
        if let Some(input) = event_input {
            tool.input = input;
        }
        if has_result {
            tool.output = output;
        }
        // A failed tool is a result the model reads and recovers from; the
        // turn fails only through assistant or retry errors.
        let mut item = tool_item(
            &id,
            &tool.name,
            tool.input.clone(),
            tool.output.clone(),
            status,
        );
        if let ItemContent::ToolCall { output, .. } = &mut item.content
            && let Some(content) = result.and_then(|r| r.get("content"))
            && content.as_array().is_some_and(|blocks| blocks.iter().any(|block|
                matches!(block["type"].as_str(), Some("image" | "resource" | "resource_link"))))
        {
            *output = Some(content.to_string());
        }
        if status == ItemStatus::Completed
            && crate::image_reads::is_read_tool(&tool.name)
            && let ItemContent::ToolCall { image_reads, .. } = &mut item.content
        {
            *image_reads = crate::image_reads::content_images(
                result
                    .and_then(|r| r.get("content"))
                    .unwrap_or(&Value::Null),
            );
        }
        vec![match status {
            ItemStatus::InProgress if has_result => AgentEvent::ItemUpdated(item),
            ItemStatus::InProgress if was_known => AgentEvent::ItemUpdated(item),
            ItemStatus::InProgress => AgentEvent::ItemStarted(item),
            _ => AgentEvent::ItemCompleted(item),
        }]
    }
}

fn tool_item(id: &str, name: &str, input: Value, output: String, status: ItemStatus) -> ThreadItem {
    let content = match name {
        "bash" => ItemContent::CommandExecution {
            command: input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            output,
            exit_code: None,
            status,
        },
        "edit" | "write" => ItemContent::FileChange {
            changes: input
                .get("path")
                .and_then(Value::as_str)
                .map(|path| {
                    vec![FileChange {
                        path: path.to_owned(),
                        kind: if name == "write" {
                            FileChangeKind::Create
                        } else {
                            FileChangeKind::Modify
                        },
                        diff: None,
                    }]
                })
                .unwrap_or_default(),
            status,
        },
        _ => ItemContent::ToolCall {
            presentation: None,
            image_reads: Vec::new(),
            name: name.to_owned(),
            input,
            output: (!output.is_empty()).then_some(output),
            status,
        },
    };
    ThreadItem {
        id: id.into(),
        parent_item_id: None,
        content,
    }
}

fn approval_kind(tool_name: &str, payload: &Value) -> ApprovalKind {
    let input = payload.get("input").cloned().unwrap_or(Value::Null);
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if payload.get("source").and_then(Value::as_str) == Some("extension") {
        let path = payload
            .get("extensionPath")
            .and_then(Value::as_str)
            .unwrap_or("(unknown path)");
        let mut source = format!("extension tool from {path}");
        if payload
            .get("shadowsBuiltin")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            source.push_str(&format!(" (overrides builtin {tool_name})"));
        }
        let detail = match reason {
            Some(reason) => format!("{reason}; {source}"),
            None => source,
        };
        return ApprovalKind::ToolUse {
            name: tool_name.to_owned(),
            input,
            detail,
        };
    }
    match tool_name {
        "bash" => ApprovalKind::ExecCommand {
            command: input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            cwd: payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_owned),
            reason,
        },
        "edit" | "write" => ApprovalKind::FileChange {
            changes: input
                .get("path")
                .and_then(Value::as_str)
                .map(|path| {
                    vec![FileChange {
                        path: path.to_owned(),
                        kind: if tool_name == "write" {
                            FileChangeKind::Create
                        } else {
                            FileChangeKind::Modify
                        },
                        diff: None,
                    }]
                })
                .unwrap_or_default(),
            reason,
        },
        _ => ApprovalKind::ToolUse {
            name: tool_name.to_owned(),
            detail: reason.unwrap_or_else(|| input.to_string()),
            input,
        },
    }
}

/// The text of a warning-or-worse `notify` from a pi extension.
///
/// pi's fire-and-forget UI methods expect no response, so an extension that
/// reports trouble this way is otherwise invisible to the user — including a
/// user's own installed extension explaining why it could not start.
fn extension_warning(message: &Value) -> Option<String> {
    if message.get("type").and_then(Value::as_str) != Some("extension_ui_request")
        || message.get("method").and_then(Value::as_str) != Some("notify")
    {
        return None;
    }
    let level = message
        .get("notifyType")
        .and_then(Value::as_str)
        .unwrap_or("info");
    if !matches!(level, "warning" | "error") {
        return None;
    }
    let text = message.get("message").and_then(Value::as_str)?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn is_extension_dialog(message: &Value) -> bool {
    message.get("type").and_then(Value::as_str) == Some("extension_ui_request")
        && matches!(
            message.get("method").and_then(Value::as_str),
            Some("select" | "input" | "editor")
        )
}

fn extension_dialog_question(message: &Value) -> Option<(String, UserInputQuestion)> {
    if !is_extension_dialog(message) {
        return None;
    }
    let id = message.get("id")?.as_str()?.to_owned();
    let method = message.get("method")?.as_str()?;
    let title = message
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (question, options, prefill) = match method {
        "select" => (
            title.to_owned(),
            message
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|label| UserInputOption {
                    label: label.to_owned(),
                    description: String::new(),
                })
                .collect(),
            None,
        ),
        "input" => {
            let question = match message.get("placeholder").and_then(Value::as_str) {
                Some(placeholder) => format!("{title} ({placeholder})"),
                None => title.to_owned(),
            };
            (question, Vec::new(), None)
        }
        "editor" => (
            title.to_owned(),
            Vec::new(),
            message
                .get("prefill")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        _ => return None,
    };
    Some((
        id.clone(),
        UserInputQuestion { contract: crate::native_question_contract(&serde_json::json!({"options":message["options"],"allow_free_text":method!="select","required":true})),
            id,
            header: "pi".into(),
            question,
            options,
            multi_select: false,
            prefill,
        },
    ))
}

fn take_extension_dialog_response(
    pending_dialogs: &mut HashSet<String>,
    request_id: &str,
    answers: &serde_json::Map<String, Value>,
) -> Option<Value> {
    if !pending_dialogs.remove(request_id) {
        return None;
    }
    Some(match answers.get(request_id).and_then(Value::as_str) {
        Some(value) => json!({"type":"extension_ui_response","id":request_id,"value":value}),
        None => json!({"type":"extension_ui_response","id":request_id,"cancelled":true}),
    })
}

fn cancel_pending_dialogs(
    writer: &mut impl Write,
    pending_dialogs: &mut HashSet<String>,
) -> Result<(), AgentError> {
    for id in pending_dialogs.drain() {
        send_json(
            writer,
            &json!({"type":"extension_ui_response","id":id,"cancelled":true}),
        )?;
    }
    Ok(())
}

fn result_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn custom_message_id(message: &Value) -> String {
    format!(
        "pi-custom-{}",
        crate::json_u64(message.get("timestamp")).unwrap_or(0)
    )
}

fn map_usage(usage: Option<&Value>) -> Option<TokenUsage> {
    let usage = usage?;
    let input = crate::json_u64(usage.get("input"));
    let output = crate::json_u64(usage.get("output"));
    let cache_read = crate::json_u64(usage.get("cacheRead"));
    (input.is_some() || output.is_some() || cache_read.is_some()).then_some(TokenUsage {
        freshness: crate::ContextFreshness::Current,
        input_tokens: input,
        cached_input_tokens: cache_read,
        cache_write_tokens: crate::json_u64(usage.get("cacheWrite")),
        reasoning_tokens: crate::json_u64(usage.get("reasoning")),
        output_tokens: output,
        used_tokens: crate::json_u64(usage.get("totalTokens")),
        ..TokenUsage::default()
    })
}

fn attach_images(request: &mut Value, attachments: Vec<Attachment>) {
    if attachments.is_empty() {
        return;
    }
    request["images"] = Value::Array(
        attachments
            .into_iter()
            .map(|attachment| {
                json!({
                    "type":"image",
                    "data":attachment.data_base64,
                    "mimeType":attachment.media_type
                })
            })
            .collect(),
    );
}

fn map_commands(response: &Value) -> Vec<ProviderCommand> {
    response
        .pointer("/data/commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|command| {
            let raw_name = command.get("name")?.as_str()?;
            let source = command.get("source").and_then(Value::as_str).unwrap_or("");
            let (name, kind) = if source == "skill" {
                (
                    raw_name.strip_prefix("skill:").unwrap_or(raw_name),
                    ProviderCommandKind::Skill,
                )
            } else {
                (raw_name, ProviderCommandKind::Command)
            };
            (!name.trim().is_empty()).then(|| ProviderCommand {
                name: name.to_owned(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                kind,
            })
        })
        .collect()
}

fn selected_thinking(selections: &[OptionSelection]) -> Option<&str> {
    selections
        .iter()
        .find(|selection| selection.id == "reasoningEffort")
        .and_then(|selection| selection.value.as_str())
}

fn resume_session(resume: &Option<ResumeCursor>) -> Option<&str> {
    resume.as_ref()?.str_field(&["session_file", "session_id"])
}

/// The tcode tool families this session does without, named the way the user
/// knows them from Settings.
///
/// pi has no MCP client (its README: "No MCP"), and its RPC protocol exposes no
/// registration hook, so explicitly enabled tcode MCP servers stay behind.
/// Naming them beats a blanket notice: the person reading it wants to know
/// which opted-in tools went missing, not that a protocol lacks a feature.
fn unattached_servers(opts: &SessionOptions) -> Vec<&'static str> {
    [
        (
            crate::McpRegistration::SERVER_NAME_ORCHESTRATE,
            "orchestration",
        ),
        (
            crate::McpRegistration::SERVER_NAME_ORCHESTRATE_REPORT,
            "result reporting",
        ),
        (
            crate::McpRegistration::SERVER_NAME_COMPUTER_USE,
            "computer-use",
        ),
    ]
    .into_iter()
    .filter_map(|(server_name, label)| {
        opts.mcp_servers
            .iter()
            .any(|server| server.name == server_name)
            .then_some(label)
    })
    .collect()
}

fn ensure_success(message: &Value) -> Result<(), AgentError> {
    if message
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(AgentError::Provider(response_error(message)))
    }
}

fn response_error(message: &Value) -> String {
    message
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .map(str::to_owned)
                .or_else(|| error.get("message")?.as_str().map(str::to_owned))
        })
        .or_else(|| message.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            format!(
                "pi rejected {}",
                message.get("command").unwrap_or(&Value::Null)
            )
        })
}

fn send_json(writer: &mut impl Write, value: &Value) -> Result<(), AgentError> {
    write_json(writer, value, Some("serializing pi request"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn stopping_pi_group_also_stops_its_tool_child() {
        let dir = std::env::temp_dir().join(format!(
            "tcode-pi-stop-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("orphan-wrote-file");
        let mut cmd = crate::process::command("/bin/sh");
        cmd.arg("-c").arg("(sleep 1; touch \"$MUTEKI_PI_STOP_MARKER\") & wait")
            .env("MUTEKI_PI_STOP_MARKER", &marker)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        cmd.process_group(0);
        let mut child = cmd.spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        stop_child(&mut child);
        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert!(!marker.exists(), "a Pi tool descendant survived the Step cancellation");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bash_approval_distinguishes_builtin_execution_from_extension_tools() {
        let input = json!({"command":"x"});
        let builtin = approval_kind(
            "bash",
            &json!({"toolName":"bash", "input":input, "cwd":"/project"}),
        );
        assert!(
            matches!(builtin, ApprovalKind::ExecCommand { command, cwd, .. }
            if command == "x" && cwd.as_deref() == Some("/project"))
        );
        for shadows in [false, true] {
            let approval = approval_kind(
                "bash",
                &json!({
                    "toolName":"bash", "source":"extension", "extensionPath":"/extensions/bash.ts",
                    "shadowsBuiltin":shadows, "input":input, "reason":"requires confirmation"
                }),
            );
            let ApprovalKind::ToolUse {
                name,
                input: actual_input,
                detail,
            } = approval
            else {
                panic!("extension tools must not inherit builtin command semantics");
            };
            assert_eq!(name, "bash");
            assert_eq!(actual_input, input);
            assert!(detail.contains("/extensions/bash.ts"));
            assert!(detail.contains("requires confirmation"));
            assert_eq!(detail.contains("overrides builtin bash"), shadows);
        }
    }

    #[test]
    fn only_warning_notifications_become_session_warnings() {
        let notify = |level: &str| {
            json!({
                "type":"extension_ui_request",
                "id":"1",
                "method":"notify",
                "message":"my-extension could not start",
                "notifyType":level
            })
        };
        assert_eq!(
            extension_warning(&notify("warning")).as_deref(),
            Some("my-extension could not start")
        );
        assert_eq!(
            extension_warning(&notify("error")).as_deref(),
            Some("my-extension could not start")
        );
        assert!(extension_warning(&notify("info")).is_none());
        assert!(
            extension_warning(&json!({
                "type":"extension_ui_request","id":"1","method":"setStatus","statusText":"busy"
            }))
            .is_none()
        );
        assert!(extension_warning(&json!({"type":"agent_start"})).is_none());
    }

    #[test]
    fn maps_extension_dialog_requests_to_native_user_input() {
        let (id, select) = extension_dialog_question(&json!({
            "type":"extension_ui_request",
            "id":"select-id",
            "method":"select",
            "title":"Choose",
            "options":["a","b"],
            "timeout":10000
        }))
        .unwrap();
        assert_eq!(id, "select-id");
        assert_eq!(select.id, "select-id");
        assert_eq!(select.header, "pi");
        assert_eq!(select.question, "Choose");
        assert!(!select.multi_select);
        assert_eq!(select.prefill, None);
        assert_eq!(
            select
                .options
                .iter()
                .map(|option| (option.label.clone(), option.description.clone()))
                .collect::<Vec<_>>(),
            vec![("a".into(), String::new()), ("b".into(), String::new())]
        );

        let (_, input) = extension_dialog_question(&json!({
            "type":"extension_ui_request",
            "id":"input-id",
            "method":"input",
            "title":"Name",
            "placeholder":"type here"
        }))
        .unwrap();
        assert_eq!(input.question, "Name (type here)");
        assert!(input.options.is_empty());
        assert_eq!(input.prefill, None);

        let (_, editor) = extension_dialog_question(&json!({
            "type":"extension_ui_request",
            "id":"editor-id",
            "method":"editor",
            "title":"Edit",
            "prefill":"initial text"
        }))
        .unwrap();
        assert_eq!(editor.question, "Edit");
        assert!(editor.options.is_empty());
        assert_eq!(editor.prefill.as_deref(), Some("initial text"));
    }

    #[test]
    fn responds_to_pending_extension_dialog_with_value_or_cancellation() {
        let mut pending = HashSet::from(["dialog".to_owned()]);
        let mut output = Vec::new();
        cancel_pending_dialogs(&mut output, &mut pending).unwrap();
        assert!(pending.is_empty());
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            response,
            json!({
                "type":"extension_ui_response",
                "id":"dialog",
                "cancelled":true
            })
        );

        let mut pending = HashSet::from(["dialog".to_owned()]);
        let answers = serde_json::Map::from_iter([("dialog".into(), json!("custom answer"))]);
        assert_eq!(
            take_extension_dialog_response(&mut pending, "dialog", &answers),
            Some(json!({
                "type":"extension_ui_response",
                "id":"dialog",
                "value":"custom answer"
            }))
        );
        assert!(pending.is_empty());

        for answers in [
            serde_json::Map::new(),
            serde_json::Map::from_iter([("dialog".into(), json!(["not", "a string"]))]),
        ] {
            pending.insert("dialog".into());
            assert_eq!(
                take_extension_dialog_response(&mut pending, "dialog", &answers),
                Some(json!({
                    "type":"extension_ui_response",
                    "id":"dialog",
                    "cancelled":true
                }))
            );
            assert!(pending.is_empty());
        }
    }

    #[test]
    fn model_catalog_only_offers_and_defaults_to_supported_thinking_levels() {
        let model = json!({
            "id": "test-model",
            "provider": "test-provider",
            "reasoning": false,
            "thinkingLevelMap": {"xhigh": "xhigh", "max": "max"}
        });
        assert!(map_model(&model, None, None).unwrap().options.is_empty());

        // Pi's thinkingLevelMap contract: omitted keys use provider defaults,
        // null disables a level, and xhigh/max require explicit support.
        for (mapping, expected) in [
            (json!(null), vec!["off", "minimal", "low", "medium", "high"]),
            (
                json!({"low": "low"}),
                vec!["off", "minimal", "low", "medium", "high"],
            ),
            (
                json!({"minimal": null, "medium": null, "xhigh": null, "max": "max"}),
                vec!["off", "low", "high", "max"],
            ),
            (
                json!({"off": null, "xhigh": "xhigh", "max": null}),
                vec!["minimal", "low", "medium", "high", "xhigh"],
            ),
        ] {
            let mut model = json!({
                "id": "test-model",
                "provider": "test-provider",
                "reasoning": true
            });
            if !mapping.is_null() {
                model["thinkingLevelMap"] = mapping;
            }
            for selected in ["medium", "xhigh", "unsupported"] {
                let mapped = map_model(&model, None, Some(selected)).unwrap();
                let [
                    OptionDescriptor::Select {
                        options,
                        default_value,
                        ..
                    },
                ] = mapped.options.as_slice()
                else {
                    panic!("reasoning model must expose its supported thinking levels");
                };
                assert_eq!(
                    options
                        .iter()
                        .map(|option| option.value.as_str())
                        .collect::<Vec<_>>(),
                    expected,
                    "{model}"
                );
                assert_eq!(
                    default_value.as_deref(),
                    expected.contains(&selected).then_some(selected)
                );
            }
        }
    }

    /// Replays `pi --mode rpc` output recorded from pi 0.85.1 with a scripted
    /// provider: one prompt, an assistant message that thinks and calls
    /// `bash`, then a second assistant message that thinks and answers.
    fn recorded_rpc_events() -> Vec<Value> {
        include_str!("../tests/fixtures/pi/rpc_events.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn streamed_text(events: &[AgentEvent], kind: DeltaKind) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Delta {
                    kind: delta_kind,
                    text,
                    ..
                } if *delta_kind == kind => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn image_read_results_exclude_failed_reads_and_image_generation() {
        for (name, failed, expected) in [
            ("read", false, 2),
            ("read", true, 0),
            ("generate_image", false, 0),
        ] {
            let mut mapper = PiMapper::new();
            mapper.on_message(&json!({"type":"tool_execution_start","toolCallId":"read","toolName":name,"args":{"path":"image.png"}}));
            let events = mapper.on_message(&json!({"type":"tool_execution_end","toolCallId":"read","isError":failed,"result":{"content":[{"type":"image","mimeType":"image/png","data":"AQID"},{"type":"image","mimeType":"image/jpeg","data":"BAUG"}]}}));
            let AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::ToolCall { image_reads, .. },
                ..
            }) = &events[0]
            else {
                panic!("tool completion: {events:?}")
            };
            assert_eq!(image_reads.len(), expected, "{name}, failed={failed}");
            if expected != 0 {
                assert_eq!(image_reads[1].data_base64, "BAUG");
            }
        }
    }

    #[test]
    fn maps_recorded_rpc_fixture() {
        let mut mapper = PiMapper::new();
        let mut events = Vec::new();
        for message in recorded_rpc_events() {
            events.extend(mapper.on_message(&message));
        }
        let mut streamed: Vec<(String, DeltaKind)> = Vec::new();
        for event in &events {
            if let AgentEvent::Delta { item_id, kind, .. } = event
                && !streamed.iter().any(|(id, _)| id == item_id)
            {
                streamed.push((item_id.clone(), *kind));
            }
        }
        let streamed_ids: Vec<&str> = streamed.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            streamed_ids,
            [
                "pi-assistant-1789592281572:0",
                "pi-assistant-1789592281595:0",
                "pi-assistant-1789592281595:1"
            ],
            "each assistant message streams under its own id"
        );
        for (item_id, kind) in &streamed {
            let completed: Vec<&ItemContent> = events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::ItemCompleted(item) if item.id == *item_id => Some(&item.content),
                    _ => None,
                })
                .collect();
            assert_eq!(completed.len(), 1, "{item_id} completes exactly once");
            assert!(
                matches!(
                    (completed[0], kind),
                    (ItemContent::Reasoning { .. }, DeltaKind::ReasoningText)
                        | (
                            ItemContent::AssistantMessage { .. },
                            DeltaKind::AssistantText
                        )
                ),
                "{item_id} completes as the kind it streamed"
            );
        }
        assert!(
            !events.iter().any(|event| matches!(
                event,
                AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::Reasoning { .. } | ItemContent::AssistantMessage { .. }, .. })
                    if !streamed_ids.contains(&id.as_str())
            )),
            "no assistant part completes under an id that was never streamed"
        );

        assert!(matches!(events[0], AgentEvent::TurnStarted { .. }));
        assert!(!events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::UserMessage { text, .. },
                ..
            }) if text == "DO NOT ECHO"
        )));
        assert_eq!(streamed_text(&events, DeltaKind::AssistantText), "PONG");
        assert_eq!(
            streamed_text(&events, DeltaKind::ReasoningText),
            "CheckingTool done"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemUpdated(ThreadItem { content: ItemContent::CommandExecution { output, .. }, .. }) if output == "ok"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::CommandExecution { command, output, .. }, .. })
                if command == "printf ok" && output == "ok"
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::AssistantMessage { text },
                        ..
                    }) if text == "PONG"
                ))
                .count(),
            1,
            "message_end and turn_end must reconcile the same assistant message"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TokenUsage(TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(3),
                ..
            })
        )));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
    }

    /// Two assistant messages stamped in the same millisecond must not share
    /// an item id, or the second completion overwrites the first entry.
    #[test]
    fn same_timestamp_messages_get_distinct_ids() {
        let mut mapper = PiMapper::new();
        let mut events = Vec::new();
        let start = json!({"type":"message_start","message":{"role":"assistant","content":[],"stopReason":"pending","timestamp":1789592281595_u64}});
        let messages = [
            json!({"type":"agent_start"}),
            start.clone(),
            json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"first"}}),
            json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"first"}],"stopReason":"toolUse","timestamp":1789592281595_u64,"responseId":"resp-1"}}),
            json!({"type":"turn_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"first"}],"stopReason":"toolUse","timestamp":1789592281595_u64,"responseId":"resp-1"},"toolResults":[]}),
            start,
            json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"second"}}),
            json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"second"}],"stopReason":"stop","timestamp":1789592281595_u64,"responseId":"resp-2"}}),
            json!({"type":"turn_end","message":{"role":"assistant","content":[{"type":"text","text":"second"}],"stopReason":"stop","timestamp":1789592281595_u64,"responseId":"resp-2"},"toolResults":[]}),
            json!({"type":"agent_settled"}),
        ];
        for message in messages {
            events.extend(mapper.on_message(&message));
        }
        let lifecycle: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Delta { item_id, text, .. } => Some((item_id.as_str(), text.as_str())),
                AgentEvent::ItemCompleted(ThreadItem {
                    id,
                    content:
                        ItemContent::Reasoning { text } | ItemContent::AssistantMessage { text },
                    ..
                }) => Some((id.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            lifecycle,
            [
                ("pi-assistant-1789592281595:0", "first"),
                ("pi-assistant-1789592281595:0", "first"),
                ("pi-assistant-1789592281595-2:0", "second"),
                ("pi-assistant-1789592281595-2:0", "second"),
            ],
            "each message streams and completes once under its own id"
        );
    }

    #[test]
    fn multi_message_turn_keeps_latest_used_tokens_and_sums_processed() {
        let mut mapper = PiMapper::new();
        mapper.on_message(&json!({"type":"agent_start"}));
        mapper.on_message(&json!({
            "type":"message_end",
            "message":{"role":"assistant","responseId":"one","usage":{"input":10,"output":2,"totalTokens":12},"timestamp":1785400000000_u64}
        }));
        mapper.on_message(&json!({
            "type":"message_end",
            "message":{"role":"assistant","responseId":"two","usage":{"input":20,"output":3,"totalTokens":23},"timestamp":1785400000001_u64}
        }));
        let completed = mapper.on_message(&json!({"type":"agent_settled"}));
        assert!(matches!(
            completed.as_slice(),
            [AgentEvent::TurnCompleted {
                usage: Some(TokenUsage {
                    used_tokens: Some(23),
                    total_processed_tokens: Some(35),
                    ..
                }),
                ..
            }]
        ));
    }

    #[test]
    fn displayed_custom_messages_become_work_log_items() {
        let mut mapper = PiMapper::new();
        for message in [
            json!({"role":"custom","content":"hidden","display":false}),
            json!({"role":"custom","content":"implicit hidden"}),
            json!({"role":"custom","content":[{"type":"image","data":"ignored"}],"display":true}),
        ] {
            assert!(
                mapper
                    .on_message(&json!({"type":"message_end","message":message}))
                    .is_empty()
            );
        }

        let plain = mapper.on_message(&json!({
            "type":"message_end",
            "message":{
                "role":"custom",
                "customType":"my-extension",
                "content":"plain text",
                "display":true,
                "timestamp":1785400000000_u64
            }
        }));
        assert!(matches!(
            plain.as_slice(),
            [AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::Other { provider_kind, summary },
                ..
            })] if provider_kind == "my-extension" && summary == "plain text"
        ));

        let parts = mapper.on_message(&json!({
            "type":"message_end",
            "message":{
                "role":"custom",
                "content":[
                    {"type":"text","text":"first"},
                    {"type":"image","data":"ignored"},
                    {"type":"text","text":"second"}
                ],
                "display":true,
                "timestamp":1785400000001_u64
            }
        }));
        assert!(matches!(
            parts.as_slice(),
            [AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::Other { provider_kind, summary },
                ..
            })] if provider_kind == "pi-extension" && summary == "first\nsecond"
        ));
    }

    #[test]
    fn unrecovered_length_stop_fails_turn_and_surfaces_compaction_reason() {
        let mut mapper = PiMapper::new();
        mapper.context_window = Some(128_000);
        mapper.on_message(&json!({"type":"agent_start"}));
        let usage_events = mapper.on_message(&json!({
            "type":"message_end",
            "message":{
                "role":"assistant",
                "content":[{"type":"thinking","thinking":"x"}],
                "stopReason":"length",
                "usage":{"input":1891,"output":1,"cacheRead":123520,"totalTokens":125412},
                "timestamp":1785400000000_u64
            }
        }));
        assert!(usage_events.iter().any(|event| matches!(
            event,
            AgentEvent::TokenUsage(TokenUsage {
                context_window: Some(128_000),
                ..
            })
        )));
        let settled = mapper.on_message(&json!({"type":"agent_settled"}));
        assert!(matches!(
            settled.as_slice(),
            [
                AgentEvent::Error { message, fatal: false },
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Failed,
                    usage: Some(TokenUsage { context_window: Some(128_000), .. }),
                    ..
                }
            ] if message.contains("stopReason=length")
        ));

        mapper.on_message(&json!({"type":"agent_start"}));
        mapper.on_message(&json!({
            "type":"message_end",
            "message":{
                "role":"assistant",
                "content":[{"type":"thinking","thinking":"x"}],
                "stopReason":"length",
                "usage":{"input":100,"output":1,"cacheRead":123520,"totalTokens":123621},
                "timestamp":1785400000001_u64
            }
        }));
        mapper.on_message(&json!({
            "type":"compaction_end",
            "result": null,
            "errorMessage": "Summarization failed: generation hit the token cap"
        }));
        let settled_with_compact_err = mapper.on_message(&json!({"type":"agent_settled"}));
        assert!(matches!(
            settled_with_compact_err.as_slice(),
            [
                AgentEvent::Error { message, fatal: false },
                AgentEvent::TurnCompleted { status: TurnStatus::Failed, .. }
            ] if message.contains("Summarization failed: generation hit the token cap")
        ));

        mapper.on_message(&json!({"type":"agent_start"}));
        mapper.on_message(&json!({
            "type":"message_end",
            "message":{
                "role":"assistant",
                "content":[{"type":"thinking","thinking":"x"}],
                "stopReason":"length",
                "timestamp":1785400000002_u64
            }
        }));
        mapper.on_message(&json!({
            "type":"compaction_end",
            "result": {"summary":"compacted"}
        }));
        mapper.on_message(&json!({
            "type":"message_end",
            "message":{
                "role":"assistant",
                "content":[{"type":"text","text":"ok"}],
                "stopReason":"stop",
                "timestamp":1785400000003_u64
            }
        }));
        let recovered = mapper.on_message(&json!({"type":"agent_settled"}));
        assert!(matches!(
            recovered.as_slice(),
            [AgentEvent::TurnCompleted { status: TurnStatus::Completed, .. }]
        ));
    }
}
