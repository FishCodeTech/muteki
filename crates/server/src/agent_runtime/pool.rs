//! The live session pool: one engine session per conversation thread.
//!
//! A session is started by the vendored tcode adapter for its transport and
//! kept here for as long as the thread is active. The adapters' channels are
//! `async-channel` ends, which are executor-agnostic: the actor driving the
//! child process runs on smol's global executor, and this server awaits the
//! same channels from tokio. No bridging task sits in between.
//!
//! Exactly one consumer may drive a session's event stream, so [`Pool::start`]
//! hands the receiver to its caller instead of keeping it here.

use super::{
    RuntimeError,
    access::{self, AccessPlan},
    binaries,
    descriptors::{self, AdapterDescriptor, CustomTransport, EngineDescriptor, NativeTransport},
    native_drivers,
};
use agent::{
    AcpAgent, AcpLaunch, AgentError, AgentEvent, LaunchEnv, ProviderKind, ResumeCursor,
    SessionCommand, SessionOptions,
};
use serde_json::{Value, json};
use smol::channel::{Receiver, Sender};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub thread_id: String,
    /// Engine id or adapter id; an engine id selects its default adapter.
    pub adapter_id: String,
    pub instance_id: String,
    pub model: Option<String>,
    pub access_mode: String,
    /// The canonical credential id the launch environment was resolved from.
    pub credential_id: String,
    /// The session's working directory. The adapters refuse a missing one.
    pub cwd: PathBuf,
    /// A binary path configured for this instance; trusted as given.
    pub binary_override: Option<PathBuf>,
    /// Per-instance environment merged into the child.
    pub env: Vec<(String, String)>,
    /// Native home override for this identity, where the engine has one.
    pub home: Option<PathBuf>,
    pub extra_args: Vec<String>,
    /// The engine's own resume cursor from an earlier session, if any.
    pub resume: Option<Value>,
    /// Branch the resumed engine session instead of continuing it in place.
    pub fork: bool,
    /// Instructions delivered through the engine's own instruction channel;
    /// never part of a user turn's text.
    pub instructions: Option<String>,
    /// Host MCP servers the session may reach, already filtered to what this
    /// engine accepts.
    pub mcp_servers: Vec<agent::McpRegistration>,
}

/// What the session reported about itself after it started.
#[derive(Debug, Clone, Default)]
struct Established {
    provider_session_id: Option<String>,
    resume: Option<Value>,
    served_model: Option<String>,
    closed: Option<String>,
    process_group: Option<u32>,
}

#[derive(Debug)]
pub struct Session {
    pub thread_id: String,
    pub engine: String,
    pub adapter_id: String,
    pub instance_id: String,
    pub provider: ProviderKind,
    pub acp_agent_id: Option<String>,
    pub model: Option<String>,
    pub access: AccessPlan,
    pub credential_id: String,
    pub credential_revision: Option<String>,
    pub cwd: PathBuf,
    pub binary_path: PathBuf,
    pub binary_source: &'static str,
    pub started_at: String,
    pub application_instructions: Option<Value>,
    _droid_mcp_bridge: Option<native_drivers::DroidMcpBridge>,
    pi_files: Option<Mutex<PiFileIndex>>,
    commands: Sender<SessionCommand>,
    controls: Mutex<HashMap<String,tokio::sync::oneshot::Sender<Result<Value,RuntimeError>>>>,
    established: Mutex<Established>,
}

/// The Pi extension consumes this session's index into a separate custom message.
#[derive(Debug)]
struct PiFileIndex {
    directory: tempfile::TempDir,
    pending_turns: usize,
    published: bool,
}

impl PiFileIndex {
    fn publish(&mut self, paths: &[&str]) -> Result<(), RuntimeError> {
        if self.pending_turns != 0 && (self.published || !paths.is_empty()) {
            return Err(RuntimeError::new("conversation.attachment_turn_busy", "请将文件排入下一轮，当前回合仍在使用附件。"));
        }
        let path = self.directory.path().join("attachments.json");
        let io_error = |error: std::io::Error| RuntimeError::new("conversation.attachment_index_unavailable", error.to_string());
        if !paths.is_empty() {
            if !self.directory.path().join("ready").is_file() {
                return Err(RuntimeError::new("conversation.attachment_unavailable", "Pi 未加载文件附件扩展。"));
            }
            let mut staged = tempfile::NamedTempFile::new_in(self.directory.path()).map_err(io_error)?;
            serde_json::to_writer(&mut staged, paths)
                .map_err(|error| RuntimeError::new("conversation.attachment_index_unavailable", error.to_string()))?;
            staged.persist(&path).map_err(|error| io_error(error.error))?;
        } else if self.published {
            if let Err(error) = std::fs::remove_file(&path) && error.kind() != std::io::ErrorKind::NotFound {
                return Err(io_error(error));
            }
        }
        self.published = !paths.is_empty();
        Ok(())
    }
}

impl Session {
    /// Check a steer before removing its draft or queue entry.
    pub fn validate_steer_attachments(&self, attachments: &[agent::Attachment]) -> Result<(), RuntimeError> {
        if matches!(self.adapter_id.as_str(), "pi.rpc" | "omp.rpc_v2")
            && attachments.iter().any(|attachment| attachment.workspace_file().is_some()) {
            return Err(RuntimeError::new("conversation.attachment_steer_unsupported", "此 RPC 的引导通道只支持文字和图片，请将普通文件排入下一轮。"));
        }
        Ok(())
    }

    pub async fn send(&self, mut command: SessionCommand) -> Result<(), RuntimeError> {
        if let SessionCommand::Steer { attachments, .. } = &command {
            self.validate_steer_attachments(attachments)?;
        }
        if let SessionCommand::SendTurn { attachments, .. } = &mut command {
            if let Some(index) = &self.pi_files {
                let mut index = index.lock().unwrap();
                let files: Vec<_> = attachments.iter().filter_map(agent::Attachment::workspace_file).collect();
                index.publish(&files)?;
                attachments.retain(|attachment| attachment.workspace_file().is_none());
                self.commands.try_send(command).map_err(|_| RuntimeError::new("runtime.session_closed", "Pi RPC session closed before delivery"))?;
                index.pending_turns += 1;
                return Ok(());
            }
            if matches!(self.adapter_id.as_str(), "pi.rpc" | "omp.rpc_v2")
                && attachments.iter().any(|attachment| attachment.workspace_file().is_some()) {
                return Err(RuntimeError::new("conversation.attachment_unavailable", "此会话没有可读取工作区附件的工具。"));
            }
        }
        self.commands.send(command).await.map_err(|_| {
            RuntimeError::new(
                "runtime.session_closed",
                format!("the {} session is no longer running", self.adapter_id),
            )
        })
    }

    /// Wait for a provider-level response; channel enqueue is only admission.
    pub async fn send_confirmed(&self,command:SessionCommand,request_id:&str)->Result<Value,RuntimeError>{
        let (tx,rx)=tokio::sync::oneshot::channel();
        self.controls.lock().unwrap().insert(request_id.into(),tx);
        let seconds=if matches!(&command,SessionCommand::SetOption{id,..}if id=="muteki:operation"){180}else{45};
        if let Err(err)=self.send(command).await{self.controls.lock().unwrap().remove(request_id);return Err(err);}
        match tokio::time::timeout(std::time::Duration::from_secs(seconds),rx).await{
            Ok(Ok(result))=>result,
            _=>{self.controls.lock().unwrap().remove(request_id);Err(RuntimeError::new("runtime.delivery_unknown","Provider delivery response was not observed; request retained for reconciliation"))}
        }
    }

    pub fn process_group(&self)->Option<u32>{self.established.lock().ok()?.process_group}

    pub fn provider_session_id(&self) -> Option<String> {
        self.established
            .lock()
            .ok()?
            .provider_session_id
            .clone()
    }

    pub fn resume_cursor(&self) -> Option<Value> {
        self.established.lock().ok()?.resume.clone()
    }

    pub fn served_model(&self) -> Option<String> {
        self.established.lock().ok()?.served_model.clone()
    }

    pub fn close_reason(&self) -> Option<String> {
        self.established.lock().ok()?.closed.clone()
    }

    /// Ask the actor to stop and wait, bounded, until it has torn its child
    /// down and released the command channel.
    pub async fn shutdown_and_wait(&self, grace: std::time::Duration) -> bool {
        let _ = self.commands.send(SessionCommand::Shutdown).await;
        self.wait_closed(grace).await
    }

    async fn wait_closed(&self, grace: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + grace;
        while !self.commands.is_closed() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        true
    }

    pub fn is_open(&self) -> bool {
        !self.commands.is_closed() && self.close_reason().is_none()
    }

    /// Fold what the session reports about itself. The consumer of the event
    /// stream calls this so a restart and the UI see the same facts.
    pub fn observe(&self, event: &AgentEvent) {
        if let AgentEvent::Origin { event, .. } = event { self.observe(event); return; }
        if matches!(event, AgentEvent::TurnCompleted { .. }) && let Some(index) = &self.pi_files {
            let mut index = index.lock().unwrap();
            index.pending_turns = index.pending_turns.saturating_sub(1);
        }
        let delivery=match event {
            AgentEvent::RuntimeOperationResult{request_id,result,error}=>Some((request_id.clone(),if let Some(error)=error{Err(RuntimeError::new("runtime.operation_failed",error))}else{Ok(result.clone().unwrap_or(Value::Null))})),
            AgentEvent::ApprovalResolved{request_id,..}|AgentEvent::UserInputResolved{request_id,..}|AgentEvent::SteerAccepted{request_id}=>Some((request_id.clone(),Ok(json!({"status":"applied"})))),
            AgentEvent::SteerRejected{request_id}=>Some((request_id.clone(),Err(RuntimeError::new("runtime.delivery_rejected","Provider rejected the pending steer")))),
            AgentEvent::ControlDelivery{request_id,operation,status,error,..} if status != "in_flight" =>Some((request_id.clone(),if status=="unknown" {Err(RuntimeError::new("runtime.delivery_unknown",error.as_ref().and_then(|error|error["message"].as_str()).unwrap_or("Provider response delivery is unknown")))}else if status=="failed" {Err(RuntimeError::new(if operation=="runtime_operation"{"runtime.operation_failed"}else{"runtime.delivery_failed"},error.as_ref().and_then(|e|e["message"].as_str()).unwrap_or("Provider rejected the control")))}else{Ok(json!({"status":status}))})),
            _=>None,
        };
        if let Some((id,result))=delivery && let Some(waiter)=self.controls.lock().unwrap().remove(&id){let _=waiter.send(result);}
        if matches!(event,AgentEvent::SessionClosed{..}|AgentEvent::Error{fatal:true,..}){
            for (_,waiter) in self.controls.lock().unwrap().drain(){let _=waiter.send(Err(RuntimeError::new("runtime.delivery_unknown","Session closed before delivery was confirmed")));}
        }
        let Ok(mut established) = self.established.lock() else {
            return;
        };
        match event {
            AgentEvent::ProcessStarted {pid} => {established.process_group=Some(*pid);}
            AgentEvent::SessionStarted {
                provider_session_id,
                resume,
                model,
            } => {
                established.provider_session_id = Some(provider_session_id.clone());
                established.resume = Some(resume.0.clone());
                if let Some(model) = model {
                    established.served_model = Some(model.clone());
                }
            }
            AgentEvent::ServedModel { model, .. } => {
                established.served_model = Some(model.clone());
            }
            AgentEvent::SessionClosed { reason } => {
                established.closed =
                    Some(reason.clone().unwrap_or_else(|| "session_closed".into()));
            }
            _ => {}
        }
    }

    /// Whitelisted session metadata for the browser; carries no credential.
    pub fn snapshot(&self) -> Value {
        json!({
            "thread_id": self.thread_id,
            "engine": self.engine,
            "adapter_id": self.adapter_id,
            "instance_id": self.instance_id,
            "acp_agent_id": self.acp_agent_id,
            "model": self.model,
            "served_model": self.served_model(),
            "access_mode": self.access.mode,
            "credential_id": self.credential_id,
            "cwd": self.cwd.to_string_lossy(),
            "binary_path": self.binary_path.to_string_lossy(),
            "binary_source": self.binary_source,
            "provider_session_id": self.provider_session_id(),
            "started_at": self.started_at,
            "open": self.is_open(),
            "close_reason": self.close_reason(),
        })
    }
}

const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Default)]
pub struct Pool {
    sessions: tokio::sync::Mutex<HashMap<String, Arc<Session>>>,
}

/// Resolve an adapter id to the descriptors that run it, refusing one this
/// build has no transport for instead of quietly using another.
pub fn transport(
    adapter_id: &str,
) -> Result<(&'static EngineDescriptor, &'static AdapterDescriptor, NativeTransport), RuntimeError> {
    let (engine, adapter) =
        descriptors::for_adapter(adapter_id).ok_or_else(|| RuntimeError::unknown_provider(adapter_id))?;
    let native = adapter.native.ok_or_else(|| {
        RuntimeError::unsupported(format!(
            "{} declares no conversation transport in this build",
            adapter.adapter_id
        ))
    })?;
    Ok((engine, adapter, native))
}

/// An admitted adapter and access mode: the checks a session start makes
/// before anything is spawned, available to callers that must refuse a turn
/// before admitting it.
pub struct Admitted {
    pub engine: &'static EngineDescriptor,
    pub adapter: &'static AdapterDescriptor,
    pub native: NativeTransport,
    pub provider: ProviderKind,
    pub access: AccessPlan,
}

pub fn admit(adapter_id: &str, access_mode: &str) -> Result<Admitted, RuntimeError> {
    let (engine, adapter, native) = transport(adapter_id)?;
    let provider = provider_of(native);
    let access = access::plan(
        provider,
        &adapter.adapter_id,
        adapter.capabilities.approval,
        &adapter.capabilities.access_modes,
        access_mode,
    )?;
    Ok(Admitted {
        engine,
        adapter,
        native,
        provider,
        access,
    })
}

fn start_error(adapter_id: &str, error: AgentError) -> RuntimeError {
    let code = match error {
        AgentError::Spawn(_) => "runtime.spawn_failed",
        AgentError::Protocol(_) => "runtime.protocol_error",
        AgentError::Provider(_) => "runtime.provider_error",
        AgentError::Io(_) => "runtime.io_error",
    };
    RuntimeError::new(code, format!("{adapter_id} could not start: {error}"))
}

impl Pool {
    pub async fn get(&self, thread_id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().await.get(thread_id).cloned()
    }

    /// The live session for a thread, or a typed error naming what is missing.
    pub async fn require(&self, thread_id: &str) -> Result<Arc<Session>, RuntimeError> {
        match self.get(thread_id).await {
            Some(session) if session.is_open() => Ok(session),
            Some(session) => Err(RuntimeError::new(
                "runtime.session_closed",
                session
                    .close_reason()
                    .unwrap_or_else(|| "the engine session has closed".into()),
            )),
            None => Err(RuntimeError::new(
                "runtime.session_missing",
                "no engine session is running for this conversation",
            )),
        }
    }

    pub async fn open_threads(&self) -> Vec<String> {
        self.sessions.lock().await.keys().cloned().collect()
    }

    /// Start the engine session for a thread, replacing any session it already
    /// has. A thread runs exactly one engine session at a time.
    pub async fn start(
        &self,
        mut request: StartRequest,
    ) -> Result<(Arc<Session>, Receiver<AgentEvent>), RuntimeError> {
        if !request.cwd.is_dir() {
            return Err(RuntimeError::new(
                "runtime.workspace_missing",
                format!(
                    "the working directory {} does not exist",
                    request.cwd.display()
                ),
            ));
        }
        if request.adapter_id.starts_with("omp.") && !request.mcp_servers.is_empty() {
            let guidance = "OMP MCP tool guidance: xd:// paths are virtual tool devices, not shell files. Read their docs with the native read tool and invoke them with the native write tool or eval tool.write; never use bash cp or shell redirection for xd://. In eval, read MCP JSON from result.details.xdev.inner.structuredContent (or result.details.structuredContent for a direct call). Queries return structuredContent.result. Commands return structuredContent.receipt: check receipt.state, then read receipt.output.result (SRC record body is its payload). A missing top-level result is normal for commands, not a reason to invoke again. Inspect the returned object's keys and receipt in the same eval call. Rendered result.text may be truncated and must not be parsed as complete JSON. Print only the requested fields from the structured object. If a tool has already returned a successful durable receipt, query that result rather than repeating the mutation. Retry the same logical mutation only with the same idempotency_key. For composite argument constraints, the tool description includes the complete original JSON Schema.";
            request.instructions = Some(match request.instructions.take() {
                Some(instructions) if !instructions.is_empty() => format!("{instructions}\n\n{guidance}"),
                _ => guidance.to_owned(),
            });
        }
        let is_worker_or_probe = request.thread_id.starts_with("probe:")
            || request.thread_id.starts_with("worker:")
            || matches!(request.access_mode.as_str(), "probe" | "worker");
        let Admitted {
            engine,
            adapter,
            native,
            provider,
            mut access,
        } = if is_worker_or_probe && matches!(request.adapter_id.as_str(), "omp.rpc_v2" | "omp.rpc") {
            let (engine, adapter, native) = transport(&request.adapter_id)?;
            Admitted {
                engine,
                adapter,
                native,
                provider: provider_of(native),
                access: AccessPlan {
                    mode: access::SUPERVISED.to_owned(),
                    selections: Vec::new(),
                    auto_allow: access::AutoAllow::default(),
                },
            }
        } else {
            admit(&request.adapter_id, &request.access_mode)?
        };
        let pi_files = if !is_worker_or_probe
            && adapter.adapter_id == "pi.rpc"
            && !request.extra_args.iter().any(|arg| arg == "--no-tools") {
            let mut temporary = tempfile::Builder::new();
            temporary.prefix("muteki-session-files-");
            #[cfg(unix)] {
                use std::os::unix::fs::PermissionsExt;
                temporary.permissions(std::fs::Permissions::from_mode(0o700));
            }
            let directory = temporary.tempdir()
                .map_err(|error| RuntimeError::new("conversation.attachment_index_unavailable", error.to_string()))?;
            let extension = directory.path().join("workspace-files.ts");
            std::fs::write(&extension, include_str!("../../assets/pi-workspace-files.ts"))
                .map_err(|error| RuntimeError::new("conversation.attachment_index_unavailable", error.to_string()))?;
            request.extra_args.extend(["--extension".to_owned(), extension.to_string_lossy().into_owned()]);
            Some(Mutex::new(PiFileIndex { directory, pending_turns: 0, published: false }))
        } else { None };
        if adapter.adapter_id == "claude.sdk" && request.access_mode.trim().is_empty() {
            if let Some((_,mode)) = request.env.iter().rev().find(|(key,_)|key == "MUTEKI_CLAUDE_PERMISSION_MODE") {
                access.selections.retain(|selection|selection.id != "permissionMode");
                access.selections.push(agent::OptionSelection {id:"permissionMode".into(),value:json!(mode)});
            }
        }
        if matches!(adapter.adapter_id.as_str(),"kimi.local_server"|"kimi.acp") {
            native_drivers::validate_kimi_access_mode(&adapter.adapter_id, &access.mode, &request.cwd)?;
        }
        let attached = matches!(adapter.adapter_id.as_str(),"opencode.server" | "kimi.local_server")
            && request.env.iter().rev().find(|(key,_)|key == "MUTEKI_ADAPTER_ENDPOINT").is_some_and(|(_,value)|!value.is_empty());
        let binary = if attached { binaries::Resolved {path:request.binary_override.clone().unwrap_or_default(),source:"external-service"} }
            else { binaries::resolve(engine,request.binary_override.as_deref()).await? };
        if !attached && !request.env.iter().any(|(key, _)| key == "PATH") {
            request.env.push(("PATH".into(), binaries::launch_path(&binary.path)?));
        }
        let (native_model, model_selections) = match request.model.as_deref() {
            Some(model) if matches!(native, NativeTransport::Provider(ProviderKind::Cursor | ProviderKind::Grok) | NativeTransport::Acp { agent_id: "cursor", .. }) => {
                let (model, selections) = super::catalog::configured_model_selection(model)?;
                (Some(model), selections)
            }
            _ => (request.model.clone(), Vec::new()),
        };
        let previous = self.sessions.lock().await.remove(&request.thread_id);
        if let Some(previous) = previous {
            previous.shutdown_and_wait(SHUTDOWN_GRACE).await;
        }
        let mut droid_mcp_bridge = None;
        let handle = match native {
            NativeTransport::Custom(CustomTransport::DroidRpc) => {
                native_drivers::start_droid_rpc(
                    &binary.path,
                    &request.cwd,
                    request.model.clone(),
                    &access,
                    &request.env,
                    &request.extra_args,
                    request.resume.clone(),
                    request.instructions.clone(),
                    &request.mcp_servers,
                )
                .await?
            }
            NativeTransport::Custom(CustomTransport::KimiLocalServer) => {
                native_drivers::start_kimi_local_server(
                    &binary.path,
                    &request.cwd,
                    request.model.clone(),
                    &access,
                    &request.env,
                    request.home.as_deref(),
                    &request.extra_args,
                    request.resume.clone(),
                )
                .await?
            }
            NativeTransport::Custom(CustomTransport::OmpRpcV2) => {
                native_drivers::start_omp_rpc_v2(
                    &binary.path,
                    &request.cwd,
                    request.model.clone(),
                    &request.env,
                    &request.extra_args,
                    request.resume.clone(),
                    request.instructions.clone(),
                    request.mcp_servers.clone(),
                )
                .await?
            }
            NativeTransport::Custom(CustomTransport::OpenCodeV2) => {
                native_drivers::start_opencode_v2(
                    &binary.path,
                    &request.cwd,
                    request.model.clone(),
                    &access,
                    &request.env,
                    &request.extra_args,
                    request.resume.clone(),
                    request.instructions.clone(),
                    request.mcp_servers.clone(),
                )
                .await?
            }
            NativeTransport::Acp { agent_id, args } => {
                let mut acp_args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
                let mut acp_model = native_model.clone();
                if agent_id == "cursor" {
                    let endpoint = request.env.iter().rev().find(|(key, _)| key == "CURSOR_ENDPOINT")
                        .map(|(_, value)| value.clone()).or_else(|| std::env::var("CURSOR_ENDPOINT").ok());
                    if let Some(endpoint) = endpoint.filter(|value| !value.trim().is_empty()) {
                        acp_args.splice(0..0, ["--endpoint".into(), endpoint]);
                    }
                }
                if agent_id == "omp" {
                    if let (Some((_, provider)), Some((_, model))) = (
                        request.env.iter().rev().find(|(key, _)| key == "MUTEKI_OMP_PROVIDER"),
                        request.env.iter().rev().find(|(key, _)| key == "MUTEKI_OMP_MODEL"),
                    ) {
                        let selected = request.model.as_deref().unwrap_or(model);
                        let native_model = selected.strip_prefix(&format!("{provider}/")).unwrap_or(selected);
                        acp_args.splice(0..0, ["--provider".into(), provider.clone(), "--model".into(), native_model.into()]);
                        acp_model = Some(format!("{provider}/{native_model}"));
                    }
                }
                if agent_id=="omp" {
                    let approval=match access.mode.as_str(){"supervised"=>"always-ask","auto-accept-edits"=>"write","full-access"=>"yolo",_=>return Err(RuntimeError::unsupported("OMP ACP has no native auto approval policy"))};
                    acp_args.splice(0..0,["--approval-mode".into(),approval.into()]);
                    if let Some(instructions)=&request.instructions{acp_args.insert(0,format!("--append-system-prompt={instructions}"));}
                }
                if agent_id=="devin" && let Some(model)=&request.model{acp_args.extend(["--model".into(),model.clone()]);}
                if agent_id == "droid" && let Some(instructions) = &request.instructions {
                    acp_args.extend(["--append-system-prompt".into(), instructions.clone()]);
                }
                let mut mcp_servers = request.mcp_servers.clone();
                let mut launch_env = request.env.clone();
                if agent_id == "droid" && !mcp_servers.is_empty() {
                    let bridge = native_drivers::shape_droid_mcp(&mcp_servers)?;
                    launch_env.push(("MUTEKI_ACP_STDIO_MCP".into(),serde_json::to_string(&bridge.servers)
                        .map_err(|error|RuntimeError::new("runtime.mcp_config_invalid",error.to_string()))?));
                    mcp_servers.clear();
                    droid_mcp_bridge = Some(bridge);
                }
                let acp = Some(AcpAgent {
                    id: agent_id.to_owned(),
                    name: engine.identity.display_name.clone(),
                    launch: AcpLaunch::Custom {
                        command: binary.path.to_string_lossy().into_owned(),
                        args: acp_args.drain(..).collect(),
                        env: Vec::new(),
                    },
                });
                let mut selections=access.selections.clone();
                selections.extend(model_selections.iter().cloned());
                selections.push(agent::OptionSelection{id:"muteki:access_mode".into(),value:json!(access.mode)});
                let options = SessionOptions {
                    cwd: request.cwd.clone(),
                    model: acp_model,
                    resume: request.resume.clone().map(ResumeCursor),
                    fork: request.fork,
                    binary_path: None,
                    option_selections: selections,
                    mcp_servers,
                    launch_env: LaunchEnv {
                        env: launch_env,
                        home: request.home.clone(),
                    },
                    extra_args: request.extra_args.clone(),
                    acp,
                    instructions: if agent_id == "agy" { request.instructions.clone() } else { None },
                };
                if agent_id == "agy" {
                    native_drivers::start_agy_acp(options).await?
                } else {
                    agent::start_session(provider, options)
                        .await
                        .map_err(|error| start_error(&adapter.adapter_id, error))?
                }
            }
            NativeTransport::Provider(_) => {
                let options = SessionOptions {
                    cwd: request.cwd.clone(),
                    model: native_model.clone(),
                    resume: request.resume.clone().map(ResumeCursor),
                    fork: request.fork,
                    binary_path: Some(binary.path.clone()),
                    option_selections: access.selections.iter().chain(&model_selections).cloned().collect(),
                    mcp_servers: request.mcp_servers.clone(),
                    launch_env: LaunchEnv {
                        env: request.env.clone(),
                        home: request.home.clone(),
                    },
                    extra_args: request.extra_args.clone(),
                    acp: None,
                    instructions: if instruction_channel(&adapter.adapter_id) == "mcp_server_instructions" { None } else { request.instructions.clone() },
                };
                agent::start_session(provider, options)
                    .await
                    .map_err(|error| start_error(&adapter.adapter_id, error))?
            }
        };
        let session = Arc::new(Session {
            thread_id: request.thread_id.clone(),
            engine: engine.identity.engine.clone(),
            adapter_id: adapter.adapter_id.clone(),
            instance_id: if request.instance_id.trim().is_empty() {
                "default".to_owned()
            } else {
                request.instance_id.clone()
            },
            provider,
            acp_agent_id: match native {
                NativeTransport::Acp { agent_id, .. } => Some(agent_id.to_owned()),
                NativeTransport::Provider(_) | NativeTransport::Custom(_) => None,
            },
            model: request.model.clone(),
            access,
            credential_id: request.credential_id.clone(),
            credential_revision: request.env.iter().rev().find(|(key,_)|key == "MUTEKI_CREDENTIAL_CONFIGURATION_REVISION").map(|(_,value)|value.clone()),
            cwd: request.cwd.clone(),
            binary_path: binary.path.clone(),
            binary_source: binary.source,
            started_at: chrono::Utc::now().to_rfc3339(),
            application_instructions: request.instructions.as_ref().map(|text| {
                let channel = instruction_channel(&adapter.adapter_id);
                let channel = if channel == "mcp_server_instructions" && request.mcp_servers.is_empty() { "unsupported" } else { channel };
                json!({"key":"muteki.application","kind":"host_instructions","trust":"application","scope":"session",
                    "channel":channel,"chars":text.chars().count(),"text":text})
            }),
            _droid_mcp_bridge: droid_mcp_bridge,
            pi_files,
            commands: handle.commands,
            controls: Mutex::new(HashMap::new()),
            established: Mutex::new(Established::default()),
        });
        let mut sessions = self.sessions.lock().await;
        if let Some(previous) = sessions.insert(request.thread_id.clone(), session.clone()) {
            let _ = previous.send(SessionCommand::Shutdown).await;
        }
        Ok((session, handle.events))
    }

    /// Shut the thread's session down and wait for its child process to exit.
    pub async fn close(&self, thread_id: &str) -> bool {
        let session = self.sessions.lock().await.get(thread_id).cloned();
        match session {
            Some(session) => {
                if session.shutdown_and_wait(SHUTDOWN_GRACE).await {
                    self.remove_if(thread_id, &session).await;
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    pub async fn close_in_workspaces(&self, roots: &[PathBuf]) -> Result<(), String> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().await.values()
            .filter(|session| roots.iter().any(|root| session.cwd.starts_with(root)))
            .cloned().collect();
        for session in sessions {
            if !session.shutdown_and_wait(SHUTDOWN_GRACE).await {
                return Err(format!("Agent session exit is unconfirmed: {}", session.thread_id));
            }
            self.remove_if(&session.thread_id, &session).await;
        }
        Ok(())
    }

    /// Drop `session` from the pool if it is still the thread's session.
    /// A session that ended on its own must not evict its replacement.
    pub async fn remove_if(&self, thread_id: &str, session: &Arc<Session>) -> bool {
        let mut sessions = self.sessions.lock().await;
        if sessions
            .get(thread_id)
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            sessions.remove(thread_id);
            true
        } else {
            false
        }
    }

    /// Move a session started under `from` (a prewarmed slot) to `thread_id`.
    /// Fails if `from` no longer holds exactly that session.
    pub async fn adopt(&self, from: &str, thread_id: &str, session: &Arc<Session>) -> bool {
        let mut sessions = self.sessions.lock().await;
        if !sessions.get(from).is_some_and(|current| Arc::ptr_eq(current, session)) {
            return false;
        }
        sessions.remove(from);
        if let Some(previous) = sessions.insert(thread_id.to_owned(), session.clone()) {
            let _ = previous.send(SessionCommand::Shutdown).await;
        }
        true
    }

    pub async fn is_current(&self, thread_id: &str, session: &Arc<Session>) -> bool {
        self.sessions
            .lock()
            .await
            .get(thread_id)
            .is_some_and(|current| Arc::ptr_eq(current, session))
    }

    /// Shut every session down, for process teardown.
    pub async fn close_all(&self) -> usize {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().await.drain().map(|(_, s)| s).collect();
        for session in &sessions {
            let _ = session.send(SessionCommand::Shutdown).await;
        }
        let closed = futures_util::future::join_all(
            sessions.iter().map(|session| session.wait_closed(SHUTDOWN_GRACE))
        ).await;
        for (session, closed) in sessions.iter().zip(closed) {
            if !closed {
                tracing::warn!(thread_id=%session.thread_id, adapter=%session.adapter_id,
                    "Agent did not confirm shutdown before process teardown");
            }
        }
        sessions.len()
    }
}

/// Whether the adapter can be asked to compact its context as an operation.
pub fn manual_compaction(adapter_id: &str) -> bool {
    transport(adapter_id).is_ok_and(|(_, adapter, native)| {
        adapter.capabilities.compaction
            && match native {
                NativeTransport::Custom(_) => true,
                _ => provider_of(native).caps().manual_compaction,
            }
    })
}

/// The generic provider table does not describe engine-specific transports.
pub fn supports_steering(adapter_id: &str) -> bool {
    transport(adapter_id).is_ok_and(|(_, adapter, native)| {
        adapter.capabilities.steer
            && match native {
                NativeTransport::Custom(CustomTransport::DroidRpc | CustomTransport::KimiLocalServer)
                | NativeTransport::Acp {
                    agent_id: "devin" | "agy",
                    ..
                } => true,
                _ => provider_of(native).caps().supports_steering,
            }
    })
}

/// Whether the adapter's session transport can attach an HTTP or bridged MCP server.
pub fn attaches_mcp(adapter_id: &str) -> bool {
    transport(adapter_id).is_ok_and(|(_, adapter, native)| match native {
        NativeTransport::Custom(CustomTransport::DroidRpc | CustomTransport::OmpRpcV2 | CustomTransport::OpenCodeV2) => {
            adapter.capability_gateway
        }
        NativeTransport::Custom(CustomTransport::KimiLocalServer) => false,
        NativeTransport::Acp { agent_id: "droid", .. } => adapter.capability_gateway,
        _ => provider_of(native).caps().mcp_servers,
    })
}

pub fn instruction_channel(adapter_id: &str) -> &'static str {
    match adapter_id {
        "codex.app_server" => "codex_additional_context",
        "claude.sdk" | "pi.rpc" | "omp.rpc_v2" | "omp.acp" | "droid.rpc" | "droid.acp" | "agy.acp" => "system_prompt",
        "opencode.server" => "opencode_instructions",
        "grok.acp" | "kimi.acp" | "kimi.local_server" => "unsupported",
        _ => "mcp_server_instructions",
    }
}

pub fn provider_of(native: NativeTransport) -> ProviderKind {
    match native {
        NativeTransport::Provider(provider) => provider,
        NativeTransport::Acp { .. } => ProviderKind::Acp,
        NativeTransport::Custom(CustomTransport::OmpRpcV2) => ProviderKind::Pi,
        NativeTransport::Custom(CustomTransport::OpenCodeV2) => ProviderKind::OpenCode,
        NativeTransport::Custom(CustomTransport::DroidRpc | CustomTransport::KimiLocalServer) => {
            ProviderKind::Acp
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_transports_map_to_their_provider() {
        assert_eq!(
            provider_of(transport("codex").unwrap().2),
            ProviderKind::Codex
        );
        assert_eq!(
            provider_of(transport("claude.agent_sdk").unwrap().2),
            ProviderKind::ClaudeCode
        );
        for adapter_id in ["kimi.acp", "omp.acp", "devin.acp", "droid.acp", "cursor.acp"] {
            assert_eq!(
                provider_of(transport(adapter_id).unwrap().2),
                ProviderKind::Acp,
                "{adapter_id}"
            );
        }
        assert_eq!(
            transport("droid").unwrap().2,
            NativeTransport::Custom(CustomTransport::DroidRpc)
        );
        assert_eq!(
            transport("kimi.local_server").unwrap().2,
            NativeTransport::Custom(CustomTransport::KimiLocalServer)
        );
        assert_eq!(
            transport("omp.rpc_v2").unwrap().2,
            NativeTransport::Custom(CustomTransport::OmpRpcV2)
        );
    }

    #[test]
    fn an_adapter_without_a_transport_is_refused_by_code() {
        for adapter_id in ["cli.codex", "cli.droid"] {
            let error = transport(adapter_id).expect_err(adapter_id);
            assert_eq!(error.code, "capability.unsupported", "{adapter_id}");
        }
        for adapter_id in ["nope", "zcode", "cli.zcode"] {
            assert_eq!(transport(adapter_id).unwrap_err().code, "provider.descriptor_unknown");
        }
    }

    #[test]
    fn acp_engines_launch_their_own_command_so_the_binary_is_not_passed_twice() {
        let (engine, _, native) = transport("kimi.acp").unwrap();
        assert_eq!(engine.binary.basename, "kimi");
        match native {
            NativeTransport::Acp { agent_id, args } => {
                assert_eq!(agent_id, "kimi");
                assert_eq!(args, ["acp"]);
            }
            _ => panic!("kimi.acp speaks ACP"),
        }
    }

    #[tokio::test]
    async fn a_missing_session_reports_a_typed_error() {
        let pool = Pool::default();
        assert!(pool.get("absent").await.is_none());
        assert_eq!(
            pool.require("absent").await.unwrap_err().code,
            "runtime.session_missing"
        );
        assert!(!pool.close("absent").await);
    }

    #[tokio::test]
    async fn a_session_refuses_to_start_outside_an_existing_directory() {
        let pool = Pool::default();
        let error = pool
            .start(StartRequest {
                thread_id: "t".into(),
                adapter_id: "codex".into(),
                instance_id: "default".into(),
                model: None,
                access_mode: "supervised".into(),
                credential_id: "system:codex".into(),
                cwd: PathBuf::from("/nonexistent-muteki-workspace"),
                binary_override: None,
                env: Vec::new(),
                home: None,
                extra_args: Vec::new(),
                resume: None,
                fork: false,
                instructions: None,
                mcp_servers: Vec::new(),
            })
            .await
            .expect_err("a missing workspace cannot host a session");
        assert_eq!(error.code, "runtime.workspace_missing");
    }

    #[tokio::test]
    async fn an_unhonored_access_mode_is_refused_before_anything_is_spawned() {
        let pool = Pool::default();
        let error = pool
            .start(StartRequest {
                thread_id: "t".into(),
                adapter_id: "cursor.acp".into(),
                instance_id: "default".into(),
                model: None,
                access_mode: "auto".into(),
                credential_id: "system:cursor".into(),
                cwd: std::env::temp_dir(),
                binary_override: None,
                env: Vec::new(),
                home: None,
                extra_args: Vec::new(),
                resume: None,
                fork: false,
                instructions: None,
                mcp_servers: Vec::new(),
            })
            .await
            .expect_err("cursor.acp rejects auto mode");
        assert_eq!(error.code, "capability.unsupported");
    }
}
