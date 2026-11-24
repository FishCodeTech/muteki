//! Agent Client Protocol provider: any agent from the ACP registry.
//!
//! Registry-managed agents share this adapter; native providers retain their
//! richer protocol-specific clients. The protocol machinery lives in
//! [`crate::acp_session`]; this module is the registry's dialect: launching a
//! registry recipe and the generic policies for agents tcode knows nothing
//! more about.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1 as acp;
use serde_json::{Value, json};
use smol::future;

use crate::acp_session::{
    self, Dialect, Established, Launch, Session, Setup, State, TurnEnd, describe, is_auth_required,
    prompt_status,
};
use crate::{
    AcpAgent, AcpLaunch, AgentError, AgentEvent, Attachment, ProviderKind, ResumeCursor,
    SessionHandle, SessionOptions, TokenUsage, UserInputQuestion, UserInputOption,
};

/// How long we let an agent's `authenticate` run before giving up and telling
/// the user to sign in with the agent's own CLI (Gemini's is a browser OAuth
/// flow that otherwise blocks session startup for five minutes).
const AUTH_TIMEOUT: Duration = Duration::from_secs(20);
const KIMI_ENV_MODEL_ALIAS: &str = "__kimi_env_model__";

/// Start (or resume) a session with an ACP agent.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    let is_agy = opts.acp.as_ref().is_some_and(|agent| agent.id == "agy");
    if !is_agy && opts.instructions.as_deref().is_some_and(|text| !text.is_empty()) {
        return Err(AgentError::Protocol("ACP application instructions require a native instruction channel or MCP server instructions; they cannot be sent as user content".into()));
    }
    if opts.fork {
        return Err(AgentError::Protocol(
            "session fork is not supported for this provider".into(),
        ));
    }
    let Some(agent) = opts.acp.clone() else {
        return Err(AgentError::Protocol(
            "no ACP agent selected for this session".into(),
        ));
    };
    acp_session::start(ProviderKind::Acp, Registry { agent, mid_turn_prompts: AtomicBool::new(false) }, opts).await
}

/// An agent from the registry (or a custom command), driven by the generic
/// ACP policies.
struct Registry {
    agent: AcpAgent,
    mid_turn_prompts: AtomicBool,
}

impl Dialect for Registry {
    fn name(&self) -> &str {
        &self.agent.name
    }

    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError> {
        let (program, mut args) = launch_command(&self.agent.launch)?;
        args.extend(opts.extra_args.iter().cloned());
        let env = match &self.agent.launch {
            AcpLaunch::Npx { env, .. }
            | AcpLaunch::Binary { env, .. }
            | AcpLaunch::Custom { env, .. } => env.clone(),
        };
        Ok(Launch {
            program,
            args,
            env,
            client_meta: Some({let mut meta=acp::Meta::new();meta.insert("elicitation".into(),json!({"form":{}}));if self.agent.id=="devin"{meta.insert("cognition.ai/subagentSupport".into(),json!(true));meta.insert("cognition.ai/messageGrouping".into(),json!(true));}meta}),
        })
    }

    async fn establish(&self, setup: &Setup<'_>) -> Result<Established, AgentError> {
        let agent = &self.agent;
        let opts = setup.opts;
        self.mid_turn_prompts.store(setup.init.agent_capabilities.meta.as_ref()
            .and_then(|meta| meta.get("cognition.ai/midTurnPrompts")) == Some(&json!(true)), Ordering::Relaxed);
        let mcp_servers = setup.mcp_servers().await;
        let session_meta = agy_session_meta(agent, opts, setup.init)?;
        let resumed = opts
            .resume
            .as_ref()
            .and_then(|cursor| cursor.0.get("acp_session_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|_| setup.init.agent_capabilities.load_session);

        if opts.resume.is_some() && resumed.is_none(){return Err(AgentError::Protocol(format!("{} does not advertise loading the requested native session",agent.id)));}
        let mut loaded_session = None;
        if let Some(session_id) = resumed {
            // `session/load` replays the whole conversation as `session/update`
            // notifications. Our JSONL log is the authoritative history and the
            // UI has already folded it, so the replay is swallowed (see
            // `session_update`); we only want the session live again.
            let session_id = acp::SessionId::new(session_id);
            let mut load_req = acp::LoadSessionRequest::new(session_id.clone(), opts.cwd.clone())
                .mcp_servers(mcp_servers.clone());
            load_req.meta = session_meta.clone();
            let loaded = setup
                .load(load_req)
                .await;
            match loaded {
                Ok(loaded) => {
                    loaded_session = Some((session_id, loaded.modes, loaded.config_options));
                }
                Err(err) => {return Err(AgentError::Protocol(format!("{} native resume failed: {}; retry with a fresh history handoff",agent.id,describe(&err))));}
            }
        }
        let (session_id, modes, config_options) = match loaded_session {
            Some(session) => session,
            None => {
                let new = new_session(agent, setup, &mcp_servers, session_meta.as_ref()).await?;
                (new.session_id, new.modes, new.config_options)
            }
        };
        setup.adopt(modes.as_ref(), config_options.as_deref());
        configure_family(agent,setup,&session_id).await?;

        Ok(Established {
            resume: ResumeCursor(json!({
                "acp_session_id": session_id.0.to_string(),
                "acp_agent_id": agent.id,
            })),
            session_id,
        })
    }

    fn client_filesystem(&self) -> bool { self.agent.id != "devin" }
    fn shell_terminals(&self) -> bool { self.agent.id == "devin" }
    fn reported_model(&self, model: String, opts: &SessionOptions) -> String {
        // Devin keeps the family representative in `model` after its thought
        // and speed selectors change. configure_devin_model has confirmed the
        // complete requested combination before SessionStarted is emitted.
        if self.agent.id == "devin" && let Some(requested) = &opts.model {
            return requested.clone();
        }
        if self.agent.id == "kimi" && model == KIMI_ENV_MODEL_ALIAS
            && let Some(model) = kimi_env_model(&self.agent, opts)
        {
            return model;
        }
        model
    }

    fn raw_session_update(&self, state: &mut State, params: &Value) -> Option<Vec<AgentEvent>> {
        matches!(self.agent.id.as_str(), "devin"|"omp"|"kimi"|"droid").then(||state.vendor_update(&self.agent.id, params))
    }
    fn before_turn(&self, state: &mut State) { if self.agent.id == "kimi" { state.kimi_usage_checkpoint(); } }

    fn session_update(
        &self,
        state: &mut State,
        notification: acp::SessionNotification,
    ) -> Vec<AgentEvent> {
        // While `session/load` replays the conversation we already have on disk,
        // swallow everything: our JSONL log is the source of truth and the
        // timeline was folded from it before the process even started.
        if state.loading() {
            return Vec::new();
        }
        state.apply_update(notification.update)
    }

    fn turn_end(
        &self,
        state: &mut State,
        result: Result<acp::PromptResponse, acp::Error>,
    ) -> TurnEnd {
        let (status, message) = prompt_status(&result);
        let native_usage = if self.agent.id == "kimi" { state.kimi_usage_finish() } else { None };
        let usage = native_usage.or_else(|| result
            .ok()
            .and_then(|response| response.usage)
            .map(|usage| TokenUsage {
                freshness: crate::ContextFreshness::Current,
                total_processed_tokens: Some(usage.total_tokens),
                input_tokens: Some(usage.input_tokens),
                cached_input_tokens: usage.cached_read_tokens,
                cache_write_tokens: usage.cached_write_tokens,
                output_tokens: Some(usage.output_tokens),
                ..TokenUsage::default()
            })
            // Fall back to the live context-window figure from `usage_update`.
            .or_else(|| state.usage()));
        TurnEnd {
            status,
            message,
            usage,
        }
    }

    fn handles_request(&self,method:&str)->bool{matches!(method,"elicitation/create"|"session/elicitation")}
    fn request(&self,session:Session,_method:String,params:Value)->impl std::future::Future<Output=Result<Value,acp::Error>>+Send {
        async move {
            let schema=params.get("requestedSchema").or_else(||params.get("schema")).cloned().unwrap_or(Value::Null);
            if let Some(properties)=schema["properties"].as_object().filter(|properties|properties.len()==1){
                let(key,field)=properties.iter().next().unwrap();
                if let Some(values)=field["enum"].as_array(){
                    let positive=values.iter().find(|value|value.as_str().is_some_and(|value|matches!(value.trim().to_lowercase().as_str(),"approve"|"allow"|"yes")));
                    let negative=values.iter().any(|value|value.as_str().is_some_and(|value|matches!(value.trim().to_lowercase().as_str(),"deny"|"reject"|"no")));
                    if let Some(positive)=positive.filter(|_|negative){return Ok(match session.ask_form_approval(params["message"].as_str().unwrap_or("Agent requests approval"),&params).await{Some(true)=>{let mut content=serde_json::Map::new();content.insert(key.clone(),positive.clone());json!({"action":"accept","content":content})},Some(false)=>json!({"action":"decline"}),None=>json!({"action":"cancel"})});}
                }
            }
            let questions=form_questions(&schema,params["message"].as_str().unwrap_or(""));
            if questions.is_empty() || schema["properties"].as_object().is_some_and(|properties|!properties.is_empty()&&properties.len()!=questions.len()){return Err(acp::Error::invalid_params().data("No supported elicitation fields"));}
            let Some(answers)=session.ask_user(questions).await else{return Ok(json!({"action":"cancel"}));};
            if let Some(action)=answers.get("$muteki_action").and_then(Value::as_str){return Ok(json!({"action":action}));}
            let mut content=serde_json::Map::new();
            for(key,value)in answers {let kind=schema["properties"][&key]["type"].as_str().unwrap_or("string");let typed=match kind{"number"|"integer"|"boolean" if value.is_string()=>serde_json::from_str(value.as_str().unwrap_or("")).map_err(|_|acp::Error::invalid_params())?,_=>value};content.insert(key,typed);}
            Ok(json!({"action":"accept","content":content}))
        }
    }
    fn notification(&self,_state:&mut State,method:&str,params:Value)->Vec<AgentEvent>{
        if self.agent.id=="devin" && method=="_cognition.ai/compaction" {
            let status=params["status"].as_str().unwrap_or("");if matches!(status,"started"|"completed"|"failed"){return vec![AgentEvent::ContextCompacted(crate::Compaction{in_progress:status=="started",status:Some(status.into()),trigger:Some("auto".into()),summary:params["summary"].as_str().map(str::to_owned),..Default::default()})];}
        }Vec::new()
    }

    async fn steer(
        &self,
        session: &Session,
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    ) {
        if self.agent.id != "devin" || !self.mid_turn_prompts.load(Ordering::Relaxed)
            || !session.with_state(|state| state.turn().is_some())
        {
            session.emit(AgentEvent::SteerRejected { request_id }).await;
            return;
        }
        let Some(session_id) = session.id() else {
            session.emit(AgentEvent::SteerRejected { request_id }).await;
            return;
        };
        // A mid-turn prompt completes with the running prompt. Register its
        // response without blocking approvals, interrupts or further steers.
        // Until it answers, an ending turn must not resend this input.
        let blocks = match acp_session::prompt_blocks(&text, &attachments) {
            Ok(blocks) => blocks,
            Err(message) => {
                session.emit(AgentEvent::ControlDelivery {
                    request_id, operation: "steer".into(), status: "failed".into(),
                    error: Some(json!({"code":"acp.attachment_invalid","message":message})),
                }).await;
                return;
            }
        };
        session.emit(AgentEvent::ControlDelivery {
            request_id: request_id.clone(), operation: "steer".into(), status: "in_flight".into(), error: None,
        }).await;
        let request = acp::PromptRequest::new(session_id, blocks);
        let response_session = session.clone();
        let response_id = request_id.clone();
        let registered = session.connection.send_request(request).on_receiving_result(move |result| async move {
            let event = match result {
                Ok(response) if response.stop_reason != acp::StopReason::Cancelled => AgentEvent::SteerAccepted { request_id: response_id },
                Err(error) if matches!(i32::from(error.code), -32601 | -32602) => {
                    response_session.emit(AgentEvent::Warning { message: format!("Devin rejected the steer: {}", describe(&error)) }).await;
                    AgentEvent::SteerRejected { request_id: response_id }
                }
                result => AgentEvent::ControlDelivery {
                    request_id: response_id, operation: "steer".into(), status: "unknown".into(),
                    error: Some(json!({"code":"devin.steer_consumption_unknown","message":match result {
                        Err(error)=>describe(&error), _=>"Devin was cancelled before confirming steer consumption".into(),
                    }})),
                },
            };
            response_session.emit(event).await;
            Ok(())
        });
        match registered {
            Err(error) => {
                session.emit(AgentEvent::Warning { message: format!("Could not submit Devin steer: {}", describe(&error)) }).await;
                session.emit(AgentEvent::SteerRejected { request_id }).await;
            }
            Ok(_) => {
                // The request is on the wire; the deferred prompt response
                // still confirms consumption when the running turn answers it.
                session.emit(AgentEvent::ControlDelivery {
                    request_id, operation: "steer".into(), status: "accepted".into(), error: None,
                }).await;
            }
        }
    }
}

/// The resolved command line for a launch recipe.
///
/// `Npx` becomes `npm exec --yes -- <package> <args…>` (the registry's own
/// contract); `Binary` / `Custom` run as given.
fn launch_command(launch: &AcpLaunch) -> Result<(PathBuf, Vec<String>), AgentError> {
    match launch {
        AcpLaunch::Npx { package, args, .. } => {
            let npm = crate::resolve_binary(None, "npm")?;
            let mut argv = vec![
                "exec".to_string(),
                "--yes".to_string(),
                "--".to_string(),
                package.clone(),
            ];
            argv.extend(args.iter().cloned());
            Ok((npm, argv))
        }
        AcpLaunch::Binary { command, args, .. } => Ok((command.clone(), args.clone())),
        AcpLaunch::Custom { command, args, .. } => {
            let binary = crate::resolve_binary(None, command)?;
            Ok((binary, args.clone()))
        }
    }
}

fn agy_session_meta(
    agent: &AcpAgent,
    opts: &SessionOptions,
    init: &acp::InitializeResponse,
) -> Result<Option<acp::Meta>, AgentError> {
    if agent.id != "agy" {
        return Ok(None);
    }
    let Some(instructions) = opts.instructions.as_deref().filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    let supported = init
        .agent_capabilities
        .meta
        .as_ref()
        .and_then(|meta| meta.get("antigravity"))
        .and_then(|ag| ag.get("systemPrompt"))
        .and_then(|sp| sp.get("append"))
        == Some(&json!(true));
    if !supported {
        return Err(AgentError::Protocol(
            "AGY did not advertise _meta.antigravity.systemPrompt.append for application instructions".into(),
        ));
    }
    let mut meta = acp::Meta::new();
    meta.insert(
        "antigravity".into(),
        json!({"systemPrompt": {"append": instructions}}),
    );
    Ok(Some(meta))
}

async fn new_session(
    agent: &AcpAgent,
    setup: &Setup<'_>,
    mcp_servers: &[acp::McpServer],
    session_meta: Option<&acp::Meta>,
) -> Result<acp::NewSessionResponse, AgentError> {
    let (connection, opts, init) = (setup.connection, setup.opts, setup.init);
    let request = || {
        let mut req = acp::NewSessionRequest::new(opts.cwd.clone()).mcp_servers(mcp_servers.to_vec());
        req.meta = session_meta.cloned();
        req
    };
    match connection.send_request(request()).block_task().await {
        Ok(response) => Ok(response),
        Err(err) if is_auth_required(&err) => {
            if agent.id=="devin" || (agent.id=="kimi" && std::env::var("MUTEKI_KIMI_ACP_LOGIN").ok().as_deref()!=Some("1")){return Err(AgentError::Provider(auth_hint(agent,init)));}
            // The agent wants credentials. Try its own `authenticate` once —
            // but on a leash: several agents (Gemini) implement it as an
            // interactive browser OAuth flow that blocks for minutes, and we
            // have no auth UI to show meanwhile. On timeout (or failure) we
            // surface a clear error naming the methods the agent offers.
            let Some(method) = preferred_auth_method(&init.auth_methods) else {
                return Err(AgentError::Provider(auth_hint(agent, init)));
            };
            let Some(method_id) = auth_method_id(&method) else {
                return Err(AgentError::Provider(auth_hint(agent, init)));
            };
            log::info!(
                "acp[{}]: session/new needs auth; trying method `{}`",
                agent.id,
                method_id.0
            );
            let authenticated = future::or(
                async {
                    Some(
                        connection
                            .send_request(acp::AuthenticateRequest::new(method_id.clone()))
                            .block_task()
                            .await,
                    )
                },
                async {
                    smol::Timer::after(AUTH_TIMEOUT).await;
                    None
                },
            )
            .await;
            match authenticated {
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    return Err(AgentError::Provider(format!(
                        "{} (authentication via `{}` failed: {})",
                        auth_hint(agent, init),
                        method_id.0,
                        describe(&err)
                    )));
                }
                None => {
                    return Err(AgentError::Provider(format!(
                        "{} (its `{}` flow did not complete within {}s — finish it in the agent's own CLI first)",
                        auth_hint(agent, init),
                        method_id.0,
                        AUTH_TIMEOUT.as_secs()
                    )));
                }
            }
            connection
                .send_request(request())
                .block_task()
                .await
                .map_err(|err| {
                    if is_auth_required(&err) {
                        AgentError::Provider(auth_hint(agent, init))
                    } else {
                        AgentError::Protocol(format!(
                            "`{}` could not start a session: {}",
                            agent.name,
                            describe(&err)
                        ))
                    }
                })
        }
        Err(err) => Err(AgentError::Protocol(format!(
            "`{}` could not start a session: {}",
            agent.name,
            describe(&err)
        ))),
    }
}

/// Which auth method to drive over the protocol: an `env_var` method first (it
/// only validates variables we have already injected, so it is cheap and
/// non-interactive), otherwise the agent's first choice.
fn preferred_auth_method(methods: &[acp::AuthMethod]) -> Option<acp::AuthMethod> {
    methods
        .iter()
        .find(|method| matches!(method, acp::AuthMethod::EnvVar(_)))
        .or_else(|| {
            methods
                .iter()
                .find(|method| auth_method_id(method).is_some())
        })
        .cloned()
}

fn auth_method_id(method: &acp::AuthMethod) -> Option<acp::AuthMethodId> {
    match method {
        acp::AuthMethod::Agent(method) => Some(method.id.clone()),
        acp::AuthMethod::EnvVar(method) => Some(method.id.clone()),
        acp::AuthMethod::Terminal(method) => Some(method.id.clone()),
        _ => None,
    }
}

/// The message shown when an agent demands credentials we cannot supply.
fn auth_hint(agent: &AcpAgent, init: &acp::InitializeResponse) -> String {
    let methods: Vec<String> = init
        .auth_methods
        .iter()
        .map(|method| match method {
            acp::AuthMethod::Agent(method) => method.name.clone(),
            acp::AuthMethod::EnvVar(method) => format!(
                "{} ({})",
                method.name,
                method
                    .vars
                    .iter()
                    .map(|var| var.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            acp::AuthMethod::Terminal(method) => method.name.clone(),
            _ => "unknown".to_string(),
        })
        .collect();
    let offered = if methods.is_empty() {
        String::new()
    } else {
        format!(" It offers: {}.", methods.join("; "))
    };
    format!(
        "`{}` requires authentication.{offered} Sign in with the agent's own CLI, or set its API-key environment variables in Settings → Providers → ACP Agents → {}.",
        agent.name, agent.name
    )
}

pub(crate) fn form_questions(schema:&Value,message:&str)->Vec<UserInputQuestion>{
    let Some(properties)=schema["properties"].as_object()else{return Vec::new()};
    if properties.is_empty(){return vec![UserInputQuestion{id:"$confirm".into(),header:"确认".into(),question:message.into(),options:Vec::new(),multi_select:false,prefill:None,contract:Some(json!({"kind":"header","root_schema":schema,"required":false,"typed_answers":true}))}];}
    properties.iter().filter_map(|(key,field)|{
        let kind=field["type"].as_str().unwrap_or(if field["enum"].is_array(){"string"}else{""});
        if !matches!(kind,"string"|"number"|"integer"|"boolean"|"array"){return None;}
        let choice=if kind=="array"{&field["items"]}else{field};
        let choices:Vec<(Value,String)>=if kind=="boolean"{vec![(json!(true),"True".into()),(json!(false),"False".into())]}else if let Some(values)=choice["enum"].as_array(){values.iter().enumerate().map(|(i,value)|(value.clone(),choice["enumNames"][i].as_str().map(str::to_owned).unwrap_or_else(||value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string())))).collect()}else{choice.get("oneOf").or_else(||choice.get("anyOf")).and_then(Value::as_array).into_iter().flatten().filter_map(|entry|Some((entry.get("const")?.clone(),entry["title"].as_str().map(str::to_owned).unwrap_or_else(||text_value(&entry["const"]))))).collect()};
        let mut wire=serde_json::Map::new();let options=choices.iter().enumerate().map(|(i,(value,label))|{let id=format!("choice-{i}");wire.insert(id.clone(),value.clone());json!({"value":id,"label":label})}).collect::<Vec<_>>();
        Some(UserInputQuestion{contract:Some(json!({"root_schema":schema,"typed_answers":true,"requested_schema":field,"required":schema["required"].as_array().is_some_and(|required|required.iter().any(|v|v==key)),"kind":if !options.is_empty(){if kind=="array"{"multi_select"}else{"single_select"}}else{match kind{"number"|"integer"=>"number","boolean"=>"boolean","array"=>"multi_select",_=>"free_text"}},"allow_free_text":options.is_empty(),"options":options,"wire_values":wire})),id:key.clone(),header:field["title"].as_str().unwrap_or(key).into(),question:field["description"].as_str().filter(|s|!s.is_empty()).unwrap_or(message).into(),options:Vec::<UserInputOption>::new(),multi_select:kind=="array",prefill:None})
    }).collect()
}
fn text_value(value:&Value)->String{value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string())}

fn kimi_env_model(agent: &AcpAgent, opts: &SessionOptions) -> Option<String> {
    let launch_env = match &agent.launch {
        AcpLaunch::Npx { env, .. }
        | AcpLaunch::Binary { env, .. }
        | AcpLaunch::Custom { env, .. } => env,
    };
    opts.launch_env.env.iter().rev()
        .chain(launch_env.iter().rev())
        .find(|(key, _)| key == "KIMI_MODEL_NAME")
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var("KIMI_MODEL_NAME").ok())
        .filter(|value| !value.is_empty())
}

async fn configure_family(agent:&AcpAgent,setup:&Setup<'_>,session:&acp::SessionId)->Result<(),AgentError>{
    let permission=setup.opts.option_selections.iter().find(|s|s.id=="muteki:access_mode").and_then(|s|s.value.as_str()).unwrap_or("supervised");
    let effort=setup.opts.option_selections.iter().find(|s|s.id=="reasoningEffort").and_then(|s|s.value.as_str()).filter(|v|!v.is_empty()&&*v!="default");
    let find=|options:Vec<Value>,id:&str,category:&str|options.into_iter().find(|v|v["id"]==id||(!category.is_empty()&&v["category"]==category));
    let select=async |id:&str,value:&str|->Result<(),AgentError>{
        let option=find(setup.option_snapshot(),id,if id=="model"{"model"}else if id=="mode"{"mode"}else{""}).ok_or_else(||AgentError::Protocol(format!("{} did not advertise {id}",agent.id)))?;
        if !option["options"].as_array().into_iter().flatten().any(|option|option["value"]==value){return Err(AgentError::Protocol(format!("{} did not advertise {id}={value}",agent.id)));}
        setup.config(session,option["id"].as_str().unwrap_or(id),value).await
    };
    match agent.id.as_str(){
        "kimi"=>{
            if let Some(model) = setup.opts.model.as_deref() {
                // Kimi exposes environment-defined models under a synthetic ACP alias.
                let matches_env_model = kimi_env_model(agent, setup.opts).is_some_and(|name| {
                    name == model || model.strip_prefix("kimi-code/") == Some(name.as_str())
                });
                let model_id = if matches_env_model
                    && find(setup.option_snapshot(), "model", "model").is_some_and(|option| {
                        option["options"].as_array().is_some_and(|choices| {
                            choices.iter().any(|choice| choice["value"] == KIMI_ENV_MODEL_ALIAS)
                        })
                    })
                {
                    KIMI_ENV_MODEL_ALIAS
                } else {
                    model
                };
                setup.raw("session/set_model", json!({"sessionId":session.0,"modelId":model_id})).await?;
            }
            setup.native_mode(session,match permission{"full-access"|"yolo"=>"yolo","auto"=>"auto",_=>"default"}).await?;
        }
        "droid"=>{
            select("autonomy_level",match permission{"full-access"|"auto-high"=>"auto-high","auto"|"auto-medium"=>"auto-medium","auto-accept-edits"|"auto-low"=>"auto-low",_=>"normal"}).await?;
            if let Some(model)=&setup.opts.model{select("model",model).await?;}if let Some(effort)=effort{select("reasoning_effort",effort).await?;}
        }
        "agy"=>{
            if let Some(model)=&setup.opts.model{select("model",model).await?;}
            if let Some(effort)=effort{select("thought_level",effort).await?;}
            for selection in &setup.opts.option_selections {
                if let Some(id) = selection.id.strip_prefix("acp:cfg:")
                    && let Some(value) = selection.value.as_str().filter(|v| !v.is_empty())
                {
                    select(id, value).await?;
                }
            }
            setup.native_mode(session,match permission{"auto-accept-edits"=>"accept-edits","auto"=>"auto",_=>"default"}).await?;
        }
        "omp"=>{if let Some(model)=&setup.opts.model{select("model",model).await?;}if let Some(effort)=effort{select("thinking",effort).await?;}}
        "devin"=>{
            if effort.is_some(){return Err(AgentError::Protocol("Devin uses model variants instead of separate effort".into()));}
            if let Some(model)=&setup.opts.model{configure_devin_model(agent,setup,session,model).await?;}
            select("mode",match permission{"full-access"|"bypass"=>"bypass","auto"|"smart"=>"smart","auto-accept-edits"|"accept-edits"=>"accept-edits",_=>"ask"}).await?;
        }
        _=>{}
    }Ok(())
}
/// Extracts `(primary_family_label, primary_label, base_sidekick)` for a Devin Fusion variant.
fn parse_fusion_variant<'a>(
    variant: &'a Value,
    families: &'a [Value],
) -> Option<(&'a str, &'a str, &'a str)> {
    let model_uid = variant["model_uid"].as_str()?;
    let (primary_uid, sidekick_uid) = model_uid.strip_prefix("fusion-")?.split_once("-sidekick-")?;
    let base_sidekick = sidekick_uid.strip_suffix("-priority").unwrap_or(sidekick_uid);
    let label = variant["label"].as_str()?;
    let (primary_label, _) = label
        .strip_prefix("Fusion (")?
        .strip_suffix(')')?
        .split_once(" + ")?;
    let mut non_fusion = families.iter().filter(|fam| {
        fam["slug"].as_str() != Some("fusion") && fam["family_uid"].as_str() != Some("fusion")
    });
    let primary_family_label = non_fusion
        .clone()
        .find_map(|fam| {
            let variants = fam["variants"].as_array()?;
            variants
                .iter()
                .any(|v| v["label"].as_str() == Some(primary_label))
                .then(|| fam["family_label"].as_str())
                .flatten()
        })
        .or_else(|| {
            non_fusion.find_map(|fam| {
                let variants = fam["variants"].as_array()?;
                variants
                    .iter()
                    .any(|v| v["model_uid"].as_str() == Some(primary_uid))
                    .then(|| fam["family_label"].as_str())
                    .flatten()
            })
        })?;
    Some((primary_family_label, primary_label, base_sidekick))
}

/// Resolves the unique ACP `model` representative and `(family_label, variant_label)` pair
/// used to match Devin's `thought_level` and `speed` selectors.
fn select_devin_representative<'a>(
    families: &'a [Value],
    family: &'a Value,
    variant: &'a Value,
    model: &str,
    representatives: &[&'a str],
) -> Result<(&'a str, &'a str, &'a str), AgentError> {
    if let Some((primary_family_label, primary_label, base_sidekick)) =
        parse_fusion_variant(variant, families)
    {
        let family_variants = family["variants"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let filtered: Vec<&'a str> = representatives
            .iter()
            .copied()
            .filter(|&rep| {
                family_variants
                    .iter()
                    .find(|v| v["model_uid"].as_str() == Some(rep))
                    .and_then(|v| parse_fusion_variant(v, families))
                    .is_some_and(|(rep_family, _, rep_sidekick)| {
                        rep_family == primary_family_label && rep_sidekick == base_sidekick
                    })
            })
            .collect();
        if filtered.len() == 1 {
            return Ok((filtered[0], primary_family_label, primary_label));
        }
        return Err(AgentError::Protocol(
            "Devin model family has ambiguous representatives".into(),
        ));
    }

    let family_label = family["family_label"].as_str().unwrap_or("");
    let variant_label = variant["label"].as_str().unwrap_or("");
    let target_label = variant_label.strip_suffix(" 1M").unwrap_or(variant_label);
    if representatives.len() == 1 {
        return Ok((representatives[0], family_label, target_label));
    }

    let is_1m = model.ends_with("-1m") || variant_label.ends_with(" 1M");
    let filtered: Vec<&'a str> = representatives
        .iter()
        .copied()
        .filter(|rep| rep.ends_with("-1m") == is_1m)
        .collect();
    if filtered.len() == 1 {
        return Ok((filtered[0], family_label, target_label));
    }
    Err(AgentError::Protocol(
        "Devin model family has ambiguous representatives".into(),
    ))
}

async fn configure_devin_model(agent:&AcpAgent,setup:&Setup<'_>,session:&acp::SessionId,model:&str)->Result<(),AgentError>{
    let(program,_)=launch_command(&agent.launch)?;
    let mut command=crate::process::async_command(program);command.args(["models","list","--format","json"]);
    for(key,value)in setup.opts.launch_env.pairs(ProviderKind::Acp){command.env(key,value);}
    let output=crate::process::bounded_output(&mut command,Duration::from_secs(30),2*1024*1024).await?;if !output.status.success(){return Err(AgentError::Protocol("Devin model metadata command failed".into()));}
    let data:Value=serde_json::from_slice(&output.stdout).map_err(|e|AgentError::Protocol(e.to_string()))?;
    let families=data["families"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let matches=families.iter().flat_map(|family|family["variants"].as_array().into_iter().flatten().map(move|variant|(family,variant))).filter(|(_,variant)|variant["model_uid"]==model).collect::<Vec<_>>();
    if matches.len()!=1{return Err(AgentError::Protocol("Devin model metadata is ambiguous".into()));}let(family,variant)=matches[0];
    let variants=family["variants"].as_array().into_iter().flatten().filter_map(|v|v["model_uid"].as_str()).collect::<Vec<_>>();
    let deadline=std::time::Instant::now()+Duration::from_secs(30);
    let option=loop{
        let option=setup.option_snapshot().into_iter().find(|v|v["id"]=="model"||v["category"]=="model");
        if let Some(option)=option && option["options"].as_array().into_iter().flatten().any(|v|v["value"].as_str().is_some_and(|id|variants.contains(&id))){break option;}
        if std::time::Instant::now()>deadline{return Err(AgentError::Protocol("Devin model family selector unavailable".into()));}smol::Timer::after(Duration::from_millis(20)).await;
    };
    let id=option["id"].as_str().unwrap_or("model");let representatives=option["options"].as_array().into_iter().flatten().filter_map(|v|v["value"].as_str()).filter(|id|variants.contains(id)).collect::<Vec<_>>();
    if representatives.contains(&model){return setup.config(session,id,model).await;}
    let(representative,target_family_label,target_variant_label)=select_devin_representative(families,family,variant,model,&representatives)?;
    if option["currentValue"]!=representative{setup.config(session,id,representative).await?;}
    let options=setup.option_snapshot();let thought=options.iter().find(|v|v["id"]=="thought_level"||v["category"]=="thought_level").ok_or_else(||AgentError::Protocol("Devin thought selector unavailable".into()))?;
    let speed=options.iter().find(|v|v["id"]=="speed");let mut combinations=Vec::new();
    for level in thought["options"].as_array().into_iter().flatten(){let paces=speed.map(|s|s["options"].as_array().into_iter().flatten().collect::<Vec<_>>()).unwrap_or(vec![&Value::Null]);for pace in paces{
        let suffix=if !pace.is_null()&&pace["value"]!="standard"{format!(" {}",pace["name"].as_str().unwrap_or(""))}else{String::new()};let base=format!("{target_family_label} {}",level["name"].as_str().unwrap_or(""));
        if [format!("{base}{suffix}"),format!("{base} {}{suffix}",thought["name"].as_str().unwrap_or(""))].iter().any(|label|label.as_str()==target_variant_label){
            let mut combination=vec![(thought["id"].as_str().unwrap_or("thought_level").to_owned(),level["value"].as_str().unwrap_or("").to_owned())];if let Some(speed)=speed{combination.push((speed["id"].as_str().unwrap_or("speed").into(),pace["value"].as_str().unwrap_or("").into()));}combinations.push(combination);
        }
    }}
    if combinations.len()!=1{return Err(AgentError::Protocol("Devin native variant could not be mapped uniquely".into()));}
    for(config_id,value)in &combinations[0]{
        if !options.iter().any(|v|v["id"]==config_id.as_str()&&v["currentValue"]==value.as_str()){setup.config(session,config_id,value).await?;}
    }
    let applied=setup.option_snapshot();if !applied.iter().any(|v|v["id"]==id&&v["currentValue"]==representative)||combinations[0].iter().any(|(id,value)|!applied.iter().any(|v|v["id"]==id.as_str()&&v["currentValue"]==value.as_str())){return Err(AgentError::Protocol("Devin model selector combination changed during configuration".into()));}
    Ok(())
}
