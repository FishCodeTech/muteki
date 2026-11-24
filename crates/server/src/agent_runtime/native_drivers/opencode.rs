//! OpenCode 2's native HTTP and SSE session protocol.

fn supervised_command(program:impl AsRef<std::ffi::OsStr>)->tokio::process::Command{tokio::process::Command::from(agent::supervised_command(program))}

use super::{kill_process_group, spawn_in_process_group};
#[path = "opencode_session.rs"]
mod session;
use crate::agent_runtime::{RuntimeError, access::AccessPlan};
use agent::{
    AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, Compaction, ItemContent, ItemStatus, ProviderKind, ResumeCursor,
    SessionCommand, SessionHandle, ThreadItem, TokenUsage, TurnStatus, UserInputDelivery,
    UserInputOption, UserInputQuestion,
};
use futures_util::StreamExt;
use reqwest::{Client, Method};
use serde_json::{Value, json};
use smol::channel::Sender;
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};

struct ServerGuard(Option<u32>);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        kill_process_group(self.0);
    }
}


#[derive(Clone)]
struct ManagedLaunch { binary:std::path::PathBuf,cwd:std::path::PathBuf,env:Vec<(String,String)>,args:Vec<String>,port:u16,password:String,log:std::sync::Arc<std::sync::Mutex<Vec<u8>>> }
impl ManagedLaunch {
    fn spawn(&self)->Result<tokio::process::Child,RuntimeError>{
        let mut command=supervised_command(&self.binary);
        command.arg("serve").args(&self.args).args(["--hostname","127.0.0.1","--port",&self.port.to_string()]).current_dir(&self.cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        super::apply_env_pairs(&mut command, &self.env);
        command.env("OPENCODE_PASSWORD",&self.password).env("OPENCODE_DISABLE_AUTOUPDATE","1");spawn_in_process_group(&mut command);
        let mut child=command.spawn().map_err(|e|error("opencode2.spawn_failed",e.to_string()))?;
        if let Some(stdout)=child.stdout.take(){capture_log(stdout,self.log.clone());}if let Some(stderr)=child.stderr.take(){capture_log(stderr,self.log.clone());}Ok(child)
    }
}
async fn server_ready(client:&Client,base:&str,password:&str,pid:Option<u32>)->Result<(),RuntimeError>{
    tokio::time::timeout(Duration::from_secs(30),async{loop{
        if let Ok(info)=request(client,base,password,Method::GET,"/api/info",None).await {if text(&info,"version").is_some_and(|v|v.starts_with("2."))&&info["pid"].is_number()&&pid.is_none_or(|leader|info["pid"].as_u64().and_then(|pid|u32::try_from(pid).ok()).is_some_and(|pid|crate::runtime_guardian::verified_member(leader,pid).unwrap_or(false))){return}}
        tokio::time::sleep(Duration::from_millis(200)).await;
    }}).await.map_err(|_|error("opencode2.start_timeout","OpenCode 2 did not become ready"))
}

struct StreamGuard(JoinHandle<()>);
impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn capture_log(reader: impl tokio::io::AsyncRead + Send + Unpin + 'static, tail: std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut reader=reader;let mut buf=[0u8;4096];
        while let Ok(n)=reader.read(&mut buf).await {if n==0{break;}let mut log=tail.lock().unwrap();log.extend_from_slice(&buf[..n]);if log.len()>32768 {let trim=log.len()-32768;log.drain(..trim);}}
    });
}

async fn version_output(binary:&Path)->Result<std::process::Output,RuntimeError>{
    use tokio::io::AsyncReadExt;
    let mut command = supervised_command(binary);
    if let Ok(path_env) = crate::agent_runtime::binaries::launch_path(binary) {
        command.env("PATH", path_env);
    }
    let mut child=command.arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn().map_err(|e|error("opencode2.spawn_failed",e.to_string()))?;
    let _guard=ServerGuard(child.id());
    let stdout=child.stdout.take().ok_or_else(||error("opencode2.io_error","Version stdout unavailable"))?;
    let stderr=child.stderr.take().ok_or_else(||error("opencode2.io_error","Version stderr unavailable"))?;
    let result=tokio::time::timeout(Duration::from_secs(15),async{
        let (status,stdout,stderr)=tokio::join!(child.wait(),async{let mut bytes=Vec::new();stdout.take(65537).read_to_end(&mut bytes).await.map(|_|bytes)},async{let mut bytes=Vec::new();stderr.take(65537).read_to_end(&mut bytes).await.map(|_|bytes)});
        let output=std::process::Output{status:status.map_err(|e|error("opencode2.io_error",e.to_string()))?,stdout:stdout.map_err(|e|error("opencode2.io_error",e.to_string()))?,stderr:stderr.map_err(|e|error("opencode2.io_error",e.to_string()))?};
        if output.stdout.len()>65536||output.stderr.len()>65536{return Err(error("opencode2.version_output_limit","Version output exceeded 64 KiB"));}Ok(output)
    }).await;
    match result{Ok(result)=>result,Err(_)=>{let _=child.kill().await;let _=child.wait().await;Err(error("opencode2.version_timeout","Version query timed out"))}}
}

static LAST_MESSAGE_STAMP: AtomicU64 = AtomicU64::new(0);

fn message_id() -> String {
    let now = chrono::Utc::now().timestamp_millis() as u64 * 4096;
    let mut prior = LAST_MESSAGE_STAMP.load(Ordering::Relaxed);
    let stamp = loop {
        let next = now.max(prior + 1);
        match LAST_MESSAGE_STAMP.compare_exchange_weak(
            prior,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break next,
            Err(observed) => prior = observed,
        }
    };
    format!(
        "msg_{:012x}{}",
        stamp & 0xffff_ffff_ffff,
        &uuid::Uuid::new_v4().simple().to_string()[..14]
    )
}

fn error(code: &'static str, message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(code, message)
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

async fn request(
    client: &Client,
    base: &str,
    password: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, RuntimeError> {
    let mut req = client
        .request(method.clone(), format!("{base}{path}"))
        .basic_auth("opencode", Some(password))
        .timeout(Duration::from_secs(30));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let response = req
        .send()
        .await
        .map_err(|e| error(if e.is_connect(){"opencode2.connection_failed"}else{"runtime.delivery_unknown"}, e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(error(
            "opencode2.http_error",
            format!("OpenCode {method} {path} returned HTTP {status}"),
        ));
    }
    if status == reqwest::StatusCode::NO_CONTENT {
        return Ok(Value::Null);
    }
    response
        .json::<Value>()
        .await
        .map_err(|e| error("opencode2.protocol_invalid", e.to_string()))
}

fn envelope(value: Value, operation: &str) -> Result<Value, RuntimeError> {
    value.get("data").cloned().ok_or_else(|| {
        error(
            "opencode2.protocol_invalid",
            format!("{operation} had no data"),
        )
    })
}

fn model_ref(model: Option<&str>, env: &[(String, String)]) -> Result<Option<Value>, RuntimeError> {
    let model = model.filter(|model| !model.trim().is_empty()).or_else(|| {
        env.iter()
            .rev()
            .find(|(key, value)| key == "MUTEKI_OPENCODE_MODEL" && !value.is_empty())
            .map(|(_, value)| value.as_str())
    });
    let Some(model) = model else {
        if env.iter().rev().find(|(key, _)| key == "MUTEKI_OPENCODE_PROVIDER").is_some_and(|(_, value)| !value.is_empty()) {
            return Err(error("credential.model_required", "OpenCode 自定义端点必须指定模型 ID"));
        }
        return Ok(None);
    };
    let provider = env
        .iter()
        .rev()
        .find(|(key, value)| key == "MUTEKI_OPENCODE_PROVIDER" && !value.is_empty())
        .map(|(_, value)| value.as_str());
    let (provider, model) = if let Some(provider) = provider {
        (
            provider,
            model.strip_prefix(&format!("{provider}/")).unwrap_or(model),
        )
    } else {
        model
            .split_once('/')
            .ok_or_else(|| error("opencode2.model_invalid", "OpenCode 2 needs provider/model"))?
    };
    let (model, variant) = model
        .split_once('#')
        .map_or((model, None), |(id, variant)| (id, Some(variant)));
    let mut selected = json!({"providerID":provider,"id":model});
    if let Some(variant) = variant {
        selected["variant"] = json!(variant);
    }
    Ok(Some(selected))
}

fn rules(mode: &str, plan: bool) -> Value {
    if mode == "full-access" && !plan {
        return json!([{"action":"*","resource":"*","effect":"allow"}]);
    }
    let rules = vec![
        json!({"action":"shell","resource":"*","effect":"ask"}),
        json!({"action":"edit","resource":"*","effect":if plan {"deny"} else if mode == "auto-accept-edits" {"allow"} else {"ask"}}),
        json!({"action":"external_directory","resource":"*","effect":"ask"}),
    ];
    json!(rules)
}

async fn stream(client: Client, base: String, password: String, tx: mpsc::Sender<Value>) {
    let mut attempt=0;
    while attempt<30 {
        let mut connected_at=None;
        let response = client
            .get(format!("{base}/api/event"))
            .basic_auth("opencode", Some(&password))
            .send()
            .await;
        if let Ok(response) = response
            && response.status().is_success()
        {
            connected_at=Some(tokio::time::Instant::now());
            let mut bytes = response.bytes_stream();
            let mut pending = Vec::new();
            let mut frame = Vec::<u8>::new();
            while let Some(chunk) = bytes.next().await {
                let Ok(chunk) = chunk else { break };
                pending.extend_from_slice(&chunk);
                while let Some(index) = pending.iter().position(|byte| *byte == b'\n') {
                    let mut line: Vec<u8> = pending.drain(..=index).collect();
                    if line.last() == Some(&b'\n') {
                        line.pop();
                    }
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    if line.is_empty() {
                        for part in frame.split(|byte| *byte == b'\n') {
                            if let Some(data) = part.strip_prefix(b"data:")
                                && let Ok(value) = serde_json::from_slice::<Value>(data)
                                && tx.send(value).await.is_err()
                            {
                                return;
                            }
                        }
                        frame.clear();
                    } else {
                        frame.extend_from_slice(&line);
                        frame.push(b'\n');
                    }
                }
            }
        }
        if tx.is_closed() {
            return;
        }
        let _ = tx
            .send(json!({"type":"muteki.stream.reconnecting","data":{}}))
            .await;
        if connected_at.is_some_and(|at|at.elapsed()>=Duration::from_secs(5)){attempt=0;}else{attempt+=1;}
        if attempt < 30 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    let _ = tx
        .send(json!({"type":"muteki.stream.closed","data":{}}))
        .await;
}

fn emit(tx: &Sender<AgentEvent>, event: AgentEvent) {
    let _ = tx.try_send(event);
}

struct Turn {
    id: String,
    started: bool,
    usage: TokenUsage,
}

fn event_name(event: &Value) -> &str {
    text(event, "type").unwrap_or("")
}

fn parse_form(form: &Value) -> Vec<UserInputQuestion> {
    form["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|field| {
            let id = text(field, "key")?;
            if !matches!(text(field, "type"), Some("string" | "multiselect"))
                || field["hidden"] == true
            {
                return None;
            }
            Some(UserInputQuestion { contract: Some(json!({"kind":if field["type"]=="multiselect" {"multi_select"}else if field["options"].as_array().is_some_and(|a|!a.is_empty()) {"single_select"}else{"free_text"},"required":field["required"].as_bool().unwrap_or(true),"allow_free_text":field["custom"]==true || field["options"].as_array().is_none_or(|a|a.is_empty()),"options":field["options"].as_array().into_iter().flatten().map(|option|json!({"value":option["value"],"label":option["label"].as_str().or_else(||option["value"].as_str()).unwrap_or(""),"description":option["description"]})).collect::<Vec<_>>(),"requested_schema":field.get("schema").cloned().unwrap_or(Value::Null)})),
                id: id.to_owned(),
                header: text(field, "title")
                    .or_else(|| text(form, "title"))
                    .unwrap_or("Question")
                    .to_owned(),
                question: text(field, "description")
                    .or_else(|| text(field, "title"))
                    .unwrap_or(id)
                    .to_owned(),
                options: field["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|option| {
                        Some(UserInputOption {
                            label: text(option, "label")
                                .or_else(|| text(option, "value"))?
                                .to_owned(),
                            description: text(option, "description").unwrap_or("").to_owned(),
                        })
                    })
                    .collect(),
                multi_select: text(field, "type") == Some("multiselect"),
                prefill: None,
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub async fn start_opencode_v2(
    binary: &Path,
    cwd: &Path,
    model: Option<String>,
    access: &AccessPlan,
    env: &[(String, String)],
    extra_args: &[String],
    resume: Option<Value>,
    instructions: Option<String>,
    mcp_servers: Vec<agent::McpRegistration>,
) -> Result<SessionHandle, RuntimeError> {
    let selected = model_ref(model.as_deref(),env)?;
    let attach = env.iter().rev().find(|(key,_)|key == "MUTEKI_ADAPTER_ENDPOINT").map(|(_,value)|value.trim()).filter(|value|!value.is_empty());
    let log_tail=std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let (base,password,mut child,managed) = if let Some(endpoint)=attach {
        let password=env.iter().rev().find(|(key,_)|key=="OPENCODE_PASSWORD").map(|(_,value)|value.clone()).unwrap_or_default();
        (endpoint.trim_end_matches('/').to_owned(),password,None,None)
    }else{
    let version = version_output(binary).await?;
    let version = String::from_utf8_lossy(&version.stdout);
    if !version
        .split_whitespace()
        .any(|part| part.starts_with("v2."))
    {
        return Err(error(
            "opencode2.major_unsupported",
            format!("OpenCode 2 required; binary reported {}", version.trim()),
        ));
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| error("opencode2.io_error", e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| error("opencode2.io_error", e.to_string()))?
        .port();
    drop(listener);
    let base = format!("http://127.0.0.1:{port}");
    let password = uuid::Uuid::new_v4().simple().to_string();
        let launch=ManagedLaunch{binary:binary.to_path_buf(),cwd:cwd.to_path_buf(),env:env.to_vec(),args:extra_args.to_vec(),port,password:password.clone(),log:log_tail.clone()};
        let child=launch.spawn()?;(base,password,Some(child),Some(launch))
    };
    let server_guard=ServerGuard(child.as_ref().and_then(|child|child.id()));
    let client=crate::network_proxy::http_client_builder().connect_timeout(Duration::from_secs(10)).build().map_err(|e|error("opencode2.client_failed",e.to_string()))?;
    if let Err(mut failure)=server_ready(&client,&base,&password,child.as_ref().and_then(|child|child.id())).await {
        if let Some(child)=child.as_mut(){let _=child.kill().await;let _=child.wait().await;}
        failure.message=format!("{}. Native startup output: {}",failure.message,String::from_utf8_lossy(&log_tail.lock().unwrap()));return Err(failure);
    }
    let (stream_tx, mut stream_rx) = mpsc::channel(1024);
    let stream_guard = StreamGuard(tokio::spawn(stream(
        client.clone(),
        base.clone(),
        password.clone(),
        stream_tx,
    )));
    let connected = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = stream_rx.recv().await {
            if event_name(&event) == "server.connected" {
                return true;
            }
            if event_name(&event) == "muteki.stream.closed" {
                return false;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    if !connected {
        if let Some(child) = child.as_mut() { let _ = child.kill().await; }
        return Err(error(
            "opencode2.sse_connect_timeout",
            "OpenCode 2 event stream did not connect",
        ));
    }
    let cwd = cwd.to_string_lossy().to_string();
    let mut active_rules = rules(&access.mode, false);
    let mut mcp_name = None;
    let mut mcp_registrations=Vec::new();
    for mcp in mcp_servers {
        let name = format!("muteki-{}", uuid::Uuid::new_v4().simple());
        let path = format!(
            "/api/experimental/mcp/{name}?location%5Bdirectory%5D={}",
            url::form_urlencoded::byte_serialize(cwd.as_bytes()).collect::<String>()
        );
        let body = json!({"config":{"type":"remote","url":mcp.url,
            "headers":{"Authorization":format!("Bearer {}",mcp.bearer_token)},"oauth":false}});
        request(&client, &base, &password, Method::PUT, &path, Some(body.clone())).await?;
        mcp_registrations.push((path,body));
        mcp_name = Some(name);
    }
    if let Some(name) = &mcp_name {
        active_rules.as_array_mut().unwrap().extend([
            json!({"action":"muteki-*","resource":"*","effect":"deny"}),
            json!({"action":format!("{name}_*"),"resource":"*","effect":"allow"}),
        ]);
    }
    let session =
        if let Some(resume_id) = resume.as_ref().and_then(|value| text(value, "session_id")) {
            envelope(
                request(
                    &client,
                    &base,
                    &password,
                    Method::GET,
                    &format!("/api/session/{resume_id}"),
                    None,
                )
                .await?,
                "session.get",
            )?
        } else {
            let mut body = json!({"location":{"directory":cwd},"permissions":active_rules});
            if let Some(selected) = &selected {
                body["model"] = selected.clone();
            }
            envelope(
                request(
                    &client,
                    &base,
                    &password,
                    Method::POST,
                    "/api/session",
                    Some(body),
                )
                .await?,
                "session.create",
            )?
        };
    let session_id = text(&session, "id")
        .ok_or_else(|| error("opencode2.protocol_invalid", "session had no id"))?
        .to_owned();
    request(
        &client,
        &base,
        &password,
        Method::PATCH,
        &format!("/api/session/{session_id}"),
        Some(json!({"permissions":active_rules})),
    )
    .await?;
    if let Some(instructions) = &instructions {
        request(
            &client,
            &base,
            &password,
            Method::PUT,
            &format!("/api/experimental/session/{session_id}/instructions/entries/muteki"),
            Some(json!({"value":instructions})),
        )
        .await?;
    }
    let (cmd_tx, cmd_rx) = smol::channel::bounded(64);
    let (evt_tx, evt_rx) = smol::channel::unbounded();
    if let Some(pid)=child.as_ref().and_then(|child|child.id()){let _=evt_tx.try_send(AgentEvent::ProcessStarted{pid});}
    emit(
        &evt_tx,
        AgentEvent::SessionStarted {
            provider_session_id: session_id.clone(),
            resume: ResumeCursor(json!({"session_id":session_id})),
            model: selected.as_ref().map(|value| {
                format!(
                    "{}/{}",
                    value["providerID"].as_str().unwrap_or(""),
                    value["id"].as_str().unwrap_or("")
                )
            }),
        },
    );
    let recovery_instructions=instructions;
    let access_mode = access.mode.clone();
    tokio::spawn(async move {
        let mut server_guard = server_guard;
        let _stream_guard = stream_guard;
        let state = session::State::new(client.clone(),base.clone(),password.clone(),session_id.clone(),cwd,access_mode,mcp_name,selected,evt_tx.clone()).await;
        let mut restarts=std::collections::VecDeque::<tokio::time::Instant>::new();
        let mut monitor=tokio::time::interval(Duration::from_millis(500));
        match state {
            Ok(mut state) => loop {
                if let Some(command)=state.next_turn(){if !state.command(command).await{break;}}
                tokio::select! {
                    command=cmd_rx.recv()=>{let Ok(command)=command else{break};if !state.command(command).await{break;}},
                    frame=stream_rx.recv()=>{let Some(frame)=frame else{break};if !state.frame(frame).await{break;}},
                    _=monitor.tick(),if managed.is_some()=>{
                        let exited=match child.as_mut().map(|child|child.try_wait()){Some(Ok(Some(_)))=>true,Some(Ok(None))=>false,Some(Err(err))=>{state.lost(error("opencode2.process_status_failed",format!("OpenCode process status failed: {err}")));break;},None=>true};
                        if !exited{continue}
                        if !state.frame(json!({"type":"muteki.stream.reconnecting","data":{}})).await{break;}
                        let now=tokio::time::Instant::now();while restarts.front().is_some_and(|at|now.duration_since(*at)>Duration::from_secs(60)){restarts.pop_front();}
                        if restarts.len()>=3{state.lost(error("opencode2.recovery_exhausted","OpenCode managed service repeatedly exited during recovery"));break;}restarts.push_back(now);
                        kill_process_group(server_guard.0.take());
                        let launch=managed.as_ref().unwrap();
                        let restored=async{
                            let next=launch.spawn()?;let pid=next.id();server_guard.0=pid;child=Some(next);
                            if let Some(pid)=pid{emit(&evt_tx,AgentEvent::ProcessStarted{pid});}
                            server_ready(&client,&base,&password,pid).await?;
                            for(path,body)in &mcp_registrations{request(&client,&base,&password,Method::PUT,path,Some(body.clone())).await?;}
                            if let Some(instructions)=&recovery_instructions{request(&client,&base,&password,Method::PUT,&format!("/api/experimental/session/{session_id}/instructions/entries/muteki"),Some(json!({"value":instructions}))).await?;}
                            state.restore_configuration().await
                        }.await;
                        if let Err(err)=restored{state.lost(error("opencode2.recovery_failed",format!("OpenCode managed service recovery failed: {}",err.message)));break;}
                    },
                }
            },
            Err(err) => emit(&evt_tx,AgentEvent::Error{message:err.message,fatal:true}),
        }
        if let Some(child) = child.as_mut() { let _ = child.kill().await; let _=child.wait().await; }
        emit(
            &evt_tx,
            AgentEvent::SessionClosed {
                reason: Some("OpenCode 2 session closed".into()),
            },
        );
    });
    Ok(SessionHandle {
        provider: ProviderKind::OpenCode,
        commands: cmd_tx,
        events: evt_rx,
    })
}

fn configured_models(env: &agent::LaunchEnv) -> (Option<String>, Vec<Value>) {
    let provider = env
        .env
        .iter()
        .rev()
        .find(|(key, _)| key == "MUTEKI_OPENCODE_PROVIDER")
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty());
    let config = env
        .env
        .iter()
        .rev()
        .find(|(key, _)| key == "OPENCODE_CONFIG")
        .and_then(|(_, path)| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    let mut models = Vec::new();
    if let Some(provider) = &provider {
        let default = text(&config, "model");
        if let Some(entries) = config["providers"][provider]["models"].as_object() {
            for (model, value) in entries {
                let id = format!("{provider}/{model}");
                models.push(json!({"id":id,"label":text(value,"name").unwrap_or(model),
                    "provider":provider,"is_default":default == Some(id.as_str()),
                    "reasoning":{"supported":false,"levels":[],"default":"","kind":"effort","source":"opencode2.config"}}));
            }
        }
    }
    (provider, models)
}

pub async fn list_opencode_v2(
    binary: &Path,
    launch_env: &agent::LaunchEnv,
) -> Result<Vec<Value>, RuntimeError> {
    let scratch = tempfile::tempdir().map_err(|e| error("opencode2.io_error", e.to_string()))?;
    let cwd = scratch.path().to_string_lossy().to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| error("opencode2.io_error", e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| error("opencode2.io_error", e.to_string()))?
        .port();
    drop(listener);
    let base = format!("http://127.0.0.1:{port}");
    let password = uuid::Uuid::new_v4().simple().to_string();
    let mut command = supervised_command(binary);
    command
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .current_dir(scratch.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("OPENCODE_PASSWORD", &password)
        .env("OPENCODE_DISABLE_AUTOUPDATE", "1");
    super::apply_env_pairs(&mut command, &launch_env.pairs(ProviderKind::OpenCode));
    spawn_in_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| error("opencode2.spawn_failed", e.to_string()))?;
    let _server_guard = ServerGuard(child.id());
    let client = crate::network_proxy::http_client_builder()
        .build()
        .map_err(|e| error("opencode2.client_failed", e.to_string()))?;
    let (configured_provider, configured) = configured_models(launch_env);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let location = url::form_urlencoded::byte_serialize(cwd.as_bytes()).collect::<String>();
    let result = loop {
        if let Ok(info) = request(&client, &base, &password, Method::GET, "/api/info", None).await
            && info["pid"].is_number()
            && text(&info, "version").is_some_and(|v| v.starts_with("2."))
        {
            let agents = request(
                &client,
                &base,
                &password,
                Method::GET,
                &format!("/api/agent?location%5Bdirectory%5D={location}"),
                None,
            )
            .await
            .and_then(|value| envelope(value, "agent.list"));
            if agents
                .as_ref()
                .ok()
                .and_then(Value::as_array)
                .is_some_and(|rows| !rows.is_empty())
            {
                let models = request(
                    &client,
                    &base,
                    &password,
                    Method::GET,
                    &format!("/api/model?location%5Bdirectory%5D={location}"),
                    None,
                )
                .await
                .and_then(|value| envelope(value, "model.list"))?;
                if let Some(rows) = models.as_array() {
                    let mut discovered = rows.iter().filter_map(|row| {
                        let provider = text(row,"providerID")?;
                        if configured_provider.as_deref().is_some_and(|selected| selected != provider) { return None; }
                        let model = text(row,"modelID")?;
                        let id = format!("{provider}/{model}");
                        Some(json!({"id":id,"label":text(row,"name").unwrap_or(model),
                            "provider":provider,"is_default":row["default"] == true,
                            "reasoning":{"supported":false,"levels":[],"default":"","kind":"effort","source":"opencode2.model_list"}}))
                    }).collect::<Vec<_>>();
                    for declared in &configured {
                        if !discovered.iter().any(|row| row["id"] == declared["id"]) {
                            discovered.push(declared.clone());
                        }
                    }
                    if !discovered.is_empty() {
                        break Ok(discovered);
                    }
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break Err(error(
                "model_catalog.empty",
                "OpenCode 2 model inventory did not settle",
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let _ = child.kill().await;
    result
}
