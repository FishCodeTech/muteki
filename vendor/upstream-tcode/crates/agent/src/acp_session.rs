//! The Agent Client Protocol session shared by every ACP-speaking client: the
//! registry client (`acp.rs`) and the native Cursor and Grok clients.
//!
//! One child process per session, JSON-RPC over its stdio, driven by an
//! `agent_client_protocol::Client` connection on a dedicated thread running a
//! `LocalExecutor`. This module owns the protocol machinery: child and
//! connection lifetime, stderr and EOF handling, prompt delivery
//! acknowledgement, permission settlement, the standard fs/terminal client
//! services, MCP and option mapping, the standard `session/update` mapping and
//! cancellation. What an agent family does differently is its [`Dialect`].

use std::collections::{HashMap, VecDeque};
#[path = "acp_vendor.rs"]
mod vendor;
use std::fmt::Write as _;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use std::time::Duration;

use agent_client_protocol::{self as sdk, schema::ProtocolVersion, schema::v1 as acp};
use serde::Deserialize as _;
use serde_json::{Map, Value, json};
use smol::channel::{Receiver, Sender};
use smol::future;
use smol::io::{AsyncRead, AsyncReadExt as _, AsyncWrite};
use smol::prelude::*;

use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, Attachment, DeltaKind, FileChange, FileChangeKind, ItemContent, ItemStatus,
    McpRegistration, OptionDescriptor, OptionSelection, PlanStep, PlanStepStatus, ProviderCommand,
    ProviderCommandKind, ProviderKind, ResumeCursor, SelectOption, SessionCommand, SessionHandle,
    SessionOptions, ThreadItem, TokenUsage, TurnStatus, UserInputDelivery, UserInputQuestion,
};

/// Option-descriptor ids. The composer renders an ACP agent's own
/// modes/models/config options through the existing traits picker, so each one
/// needs a stable id that routes back to the right ACP method.
pub(crate) use crate::ACP_MODE_OPTION_ID as MODE_OPTION_ID;
const MODEL_OPTION_ID: &str = "acp:model";
/// A thought-level option takes the id every native provider gives its
/// reasoning effort, so it is presented, inherited and set like theirs.
const EFFORT_OPTION_ID: &str = "reasoningEffort";
pub(crate) const CONFIG_OPTION_PREFIX: &str = "acp:cfg:";

/// The descriptor id of the session config option `config_id` in `category`.
fn config_option_id(
    config_id: &str,
    category: Option<&acp::SessionConfigOptionCategory>,
) -> String {
    match category {
        Some(acp::SessionConfigOptionCategory::Mode) => MODE_OPTION_ID.to_string(),
        Some(acp::SessionConfigOptionCategory::Model) => MODEL_OPTION_ID.to_string(),
        Some(acp::SessionConfigOptionCategory::ThoughtLevel) => EFFORT_OPTION_ID.to_string(),
        _ => format!("{CONFIG_OPTION_PREFIX}{config_id}"),
    }
}

/// The persisted selection for the session config option `config_id` in
/// `category`, as [`SessionOptions::option_selections`] carries it into a new
/// process.
pub(crate) fn config_selection<'a>(
    selections: &'a [OptionSelection],
    config_id: &str,
    category: Option<&acp::SessionConfigOptionCategory>,
) -> Option<&'a str> {
    let id = config_option_id(config_id, category);
    // Selections saved while every config option was `acp:cfg:<id>` still
    // name a thought-level option that way.
    let legacy = format!("{CONFIG_OPTION_PREFIX}{config_id}");
    let find = |id: &str| selections.iter().find(|selection| selection.id == id);
    find(&id)
        .or_else(|| find(&legacy))
        .and_then(|selection| selection.value.as_str())
}

/// Cap on captured terminal output when the agent sets none (1 MiB).
const DEFAULT_TERMINAL_OUTPUT_LIMIT: u64 = 1 << 20;

/// How long we wait for an agent to answer `initialize` before declaring it
/// broken. Generous: an `npm exec` recipe may have to fetch the package first.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) type Connection = sdk::ConnectionTo<sdk::Agent>;

trait MutexExt<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

macro_rules! request_handler {
    ($client:expr, $request:ty, $method:ident) => {{
        let client = $client.clone();
        async move |args: $request, responder, connection| {
            let client = client.clone();
            connection
                .spawn(async move { responder.respond_with_result(client.$method(args).await) })?;
            Ok(())
        }
    }};
    ($client:expr, $request:ty, $method:ident, connection) => {{
        let client = $client.clone();
        async move |args: $request, responder, connection| {
            let client = client.clone();
            let task_connection = connection.clone();
            connection.spawn(async move {
                responder.respond_with_result(client.$method(args, &task_connection).await)
            })?;
            Ok(())
        }
    }};
}

/// What one ACP agent family does on top of the shared protocol machinery.
///
pub(crate) trait Dialect: Send + Sync + 'static {
    /// The agent's name in messages and logs.
    fn name(&self) -> &str;

    /// The process to spawn and how the client introduces itself to it.
    fn launch(&self, opts: &SessionOptions) -> Result<Launch, AgentError>;

    /// Authenticate when needed, then create, load, resume or fork the session
    /// and apply its initial mode. Runs after `initialize` succeeded.
    async fn establish(&self, setup: &Setup<'_>) -> Result<Established, AgentError>;

    /// One `session/update`, including whether a replayed update is dropped.
    /// [`State::apply_update`] is the standard mapping.
    fn session_update(
        &self,
        state: &mut State,
        notification: acp::SessionNotification,
    ) -> Vec<AgentEvent>;

    fn raw_session_update(&self, _state: &mut State, _params: &Value) -> Option<Vec<AgentEvent>> { None }
    fn before_turn(&self, _state: &mut State) {}

    /// How the `session/prompt` result ends the turn, with its usage.
    fn turn_end(
        &self,
        state: &mut State,
        result: Result<acp::PromptResponse, acp::Error>,
    ) -> TurnEnd;

    /// Deliver [`SessionCommand::Steer`] into the running turn.
    async fn steer(
        &self,
        session: &Session,
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    );

    /// Whether an agent→client request outside the standard set belongs to
    /// this dialect, by its literal method name.
    fn handles_request(&self, _method: &str) -> bool {
        false
    }

    /// Answer a request [`Self::handles_request`] claimed, with its raw params.
    /// It runs as its own task, so it may wait for the user without holding
    /// up the connection.
    fn request(
        &self,
        _session: Session,
        method: String,
        _params: Value,
    ) -> impl Future<Output = Result<Value, acp::Error>> + Send {
        async move { Err(acp::Error::method_not_found().data(method)) }
    }

    /// An agent→client notification other than a `session/update` the schema
    /// knows (a vendor `sessionUpdate` kind arrives here as `session/update`),
    /// by its literal method name with raw params. Runs in arrival order with
    /// the session updates.
    fn notification(&self, _state: &mut State, _method: &str, _params: Value) -> Vec<AgentEvent> {
        Vec::new()
    }

    /// Whether `initialize` offers the client's `fs/*` and `terminal/*`
    /// services. An agent offered them runs its file and shell tools through
    /// tcode instead of its own implementation.
    fn client_services(&self) -> bool {
        true
    }

    fn client_filesystem(&self) -> bool { self.client_services() }
    fn shell_terminals(&self) -> bool { false }

    /// Map a protocol model selection to the model identity reported to callers.
    fn reported_model(&self, model: String, _opts: &SessionOptions) -> String {
        model
    }

    /// Session config options, by `configId`, that tcode already controls
    /// elsewhere (the composer's model picker), so they are not surfaced again
    /// in [`AgentEvent::ProviderOptions`].
    fn owned_config_options(&self) -> &'static [&'static str] {
        &[]
    }
}

/// The process a dialect runs and its `initialize` introduction.
pub(crate) struct Launch {
    pub(crate) program: PathBuf,
    /// The complete argument list, including the user's launch arguments.
    pub(crate) args: Vec<String>,
    /// Applied before the profile's configured environment, which wins.
    pub(crate) env: Vec<(String, String)>,
    /// `clientCapabilities._meta` sent with `initialize`.
    pub(crate) client_meta: Option<acp::Meta>,
}

/// The session a dialect settled on.
pub(crate) struct Established {
    pub(crate) session_id: acp::SessionId,
    pub(crate) resume: ResumeCursor,
}

/// The canonical end of one prompt turn.
pub(crate) struct TurnEnd {
    pub(crate) status: TurnStatus,
    /// Surfaced as a non-fatal [`AgentEvent::Error`] before the completion.
    pub(crate) message: Option<String>,
    pub(crate) usage: Option<TokenUsage>,
}

/// What [`Dialect::establish`] works with.
pub(crate) struct Setup<'a> {
    pub(crate) connection: &'a Connection,
    pub(crate) opts: &'a SessionOptions,
    pub(crate) init: &'a acp::InitializeResponse,
    name: &'a str,
    state: &'a Arc<Mutex<State>>,
    events: &'a Sender<AgentEvent>,
}

impl Setup<'_> {
    pub(crate) async fn warn(&self, message: String) {
        let _ = self.events.send(AgentEvent::Warning { message }).await;
    }

    pub(crate) fn option_snapshot(&self)->Vec<Value>{
        self.state.lock_recover().options.records.iter().filter_map(|record|{
            let OptionOrigin::Config(id)=&record.origin else{return None};
            let OptionDescriptor::Select{label,options,..}=&record.descriptor else{return None};
            Some(json!({"id":id.0,"name":label,"currentValue":record.selection.value,
                "category":match record.selection.id.as_str(){MODEL_OPTION_ID=>"model",MODE_OPTION_ID=>"mode",EFFORT_OPTION_ID=>"thought_level",_=>""},
                "options":options.iter().map(|option|json!({"value":option.value,"name":option.label})).collect::<Vec<_>>()}))
        }).collect()
    }
    pub(crate) async fn config(&self,session:&acp::SessionId,id:&str,value:&str)->Result<(),AgentError>{
        let origin=OptionOrigin::Config(acp::SessionConfigId::new(id));
        let options=set_option(self.connection,session,&origin,&json!(value)).await.map_err(|e|AgentError::Protocol(describe(&e)))?;
        if let Some(options)=options.filter(|options|!options.is_empty()){self.state.lock_recover().options.ingest(None,Some(&options));}
        let deadline=std::time::Instant::now()+Duration::from_secs(30);
        loop {
            if self.option_snapshot().iter().any(|option|option["id"]==id && option["currentValue"]==value){return Ok(());}
            if std::time::Instant::now()>deadline{return Err(AgentError::Protocol(format!("Agent did not confirm {id}={value}")));}
            smol::Timer::after(Duration::from_millis(20)).await;
        }
    }
    pub(crate) async fn native_mode(&self,session:&acp::SessionId,mode:&str)->Result<(),AgentError>{
        set_option(self.connection,session,&OptionOrigin::Mode,&json!(mode)).await.map_err(|e|AgentError::Protocol(describe(&e)))?;
        self.state.lock_recover().select_mode(acp::SessionModeId::new(mode));Ok(())
    }
    pub(crate) async fn raw(&self,method:&str,params:Value)->Result<Value,AgentError>{
        let request=sdk::UntypedMessage::new(method,params).map_err(|e|AgentError::Protocol(describe(&e)))?;
        self.connection.send_request(request).block_task().await.map_err(|e|AgentError::Protocol(describe(&e)))
    }

    /// The `mcpServers` for `session/new`, `load` or `resume`. tcode's MCP
    /// servers are loopback streamable-HTTP endpoints, so they are only
    /// offered to agents that speak MCP over HTTP.
    pub(crate) async fn mcp_servers(&self) -> Vec<acp::McpServer> {
        let registrations: Vec<_> = self.opts.mcp_servers.iter().collect();
        let mut servers = mcp_servers(&registrations, &self.init.agent_capabilities);
        if let Some((_,raw)) = self.opts.launch_env.env.iter().rev().find(|(key,_)|key == "MUTEKI_ACP_STDIO_MCP") {
            match serde_json::from_str::<Vec<acp::McpServer>>(raw) {
                Ok(stdio) => servers.extend(stdio),
                Err(error) => self.warn(format!("Invalid owned stdio MCP configuration: {error}")).await,
            }
        }
        if !registrations.is_empty() && servers.is_empty() {
            let _ = self.events.send(AgentEvent::McpServersNotAttached).await;
            log::info!(
                "acp[{}]: no mcpCapabilities.http; Tcode MCP servers are not registered",
                self.name
            );
            self.warn(format!(
                "{} does not support HTTP MCP servers; Tcode MCP tools are unavailable in this session",
                self.name
            ))
            .await;
        }
        servers
    }

    /// `session/load`, with [`State::loading`] set until it answers: an agent
    /// may replay the conversation as `session/update`s before it responds.
    pub(crate) async fn load(
        &self,
        request: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        self.state.lock_recover().loading = true;
        let (loaded_tx, loaded) = smol::channel::bounded(1);
        let state = self.state.clone();
        // Cleared while the response holds the dispatch loop: an update the
        // agent sends right after answering is live, and with `block_task`
        // it could be read before the flag clears and dropped as replay.
        let registered =
            self.connection
                .send_request(request)
                .on_receiving_result(move |result| async move {
                    state.lock_recover().loading = false;
                    let _ = loaded_tx.send(result).await;
                    Ok(())
                });
        let loaded = match registered {
            Ok(()) => loaded.recv().await.unwrap_or_else(|_| {
                Err(acp::Error::internal_error().data("the agent closed before answering"))
            }),
            Err(err) => Err(err),
        };
        self.state.lock_recover().loading = false;
        loaded
    }

    /// Take the modes and config options the session started with.
    pub(crate) fn adopt(
        &self,
        modes: Option<&acp::SessionModeState>,
        config_options: Option<&[acp::SessionConfigOption]>,
    ) {
        let mut state = self.state.lock_recover();
        state.ingest_modes(modes);
        state.options.ingest(None, config_options);
    }
}

/// A live session as dialect hooks see it.
#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) connection: Connection,
    events: Sender<AgentEvent>,
    state: Arc<Mutex<State>>,
    wire_response_id: Option<Value>,
}

impl Session {
    /// The established session's id.
    pub(crate) fn id(&self) -> Option<acp::SessionId> {
        self.with_state(|state| state.session_id.clone())
    }

    pub(crate) async fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event).await;
    }

    pub(crate) fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock_recover())
    }

    /// Ask the user and wait for [`SessionCommand::RespondUserInput`]. `None`
    /// when the request was cancelled by an interrupt or shutdown.
    pub(crate) async fn ask_user(
        &self,
        questions: Vec<UserInputQuestion>,
    ) -> Option<Map<String, Value>> {
        let (answer, answered) = smol::channel::bounded(1);
        let request_id = self.with_state(|state| {
            state.input_seq += 1;
            let request_id = format!("acp-input-{}", state.input_seq);
            state.inputs.insert(request_id.clone(), answer);
            if let Some(wire_id) = &self.wire_response_id { state.control_wire.insert(request_id.clone(),wire_id.to_string()); }
            request_id
        });
        self.emit(AgentEvent::UserInputRequested {
            request_id,
            questions,
            delivery: UserInputDelivery::Blocking,
        })
        .await;
        answered.recv().await.ok()
    }

    pub(crate) async fn ask_form_approval(&self,message:&str,params:&Value)->Option<bool>{
        let(answer,answered)=smol::channel::bounded(1);
        let options=vec![ApprovalOption{id:"approve".into(),label:"Approve".into(),kind:ApprovalOptionKind::AllowOnce},ApprovalOption{id:"deny".into(),label:"Deny".into(),kind:ApprovalOptionKind::RejectOnce}];
        let(request_id,turn)=self.with_state(|state|{state.approval_seq+=1;let id=format!("acp-approval-{}",state.approval_seq);state.approvals.insert(id.clone(),(answer,options.clone()));if let Some(wire)=&self.wire_response_id{state.control_wire.insert(id.clone(),wire.to_string());}(id,state.turn().map(str::to_owned))});
        self.emit(AgentEvent::ApprovalRequested(ApprovalRequest{id:request_id,turn_id:turn,parent_item_id:None,tool_call_id:None,native_execution:Some(json!({"tool_name":"acp.elicitation","action":"acp.elicitation","arguments":params,"resource_kind":"mcp"})),kind:ApprovalKind::ToolUse{name:"acp.elicitation".into(),input:params.clone(),detail:message.into()},options})).await;
        match answered.recv().await.ok()?{acp::RequestPermissionOutcome::Selected(selected)=>Some(selected.option_id.0.as_ref()=="approve"),_=>None}
    }
}

/// Start (or resume) a session with an ACP agent speaking `dialect`.
pub(crate) async fn start<D: Dialect>(
    provider: ProviderKind,
    dialect: D,
    opts: SessionOptions,
) -> Result<SessionHandle, AgentError> {
    let (commands_tx, commands_rx) = smol::channel::unbounded();
    let (events_tx, events_rx) = smol::channel::unbounded();
    let (ready_tx, ready_rx) = smol::channel::bounded(1);
    let dialect = Arc::new(dialect);

    // Keep each ACP connection and all of its callbacks on one dedicated
    // executor thread. The SDK requires Send handlers internally, but the
    // adapter still exposes only channels across this boundary.
    std::thread::Builder::new()
        .name("acp-session".into())
        .spawn(move || {
            let executor = Rc::new(smol::LocalExecutor::new());
            let task = run_actor(
                executor.clone(),
                provider,
                dialect,
                opts,
                commands_rx,
                events_tx,
                ready_tx,
            );
            smol::block_on(executor.run(task));
        })
        .map_err(|err| {
            AgentError::Spawn(format!("could not start the ACP session thread: {err}"))
        })?;

    ready_rx.recv().await.map_err(|_| {
        AgentError::Protocol("ACP actor exited before reporting startup status".into())
    })??;

    Ok(SessionHandle {
        provider,
        commands: commands_tx,
        events: events_rx,
    })
}

/// Run `query` against a freshly started and initialized agent outside any
/// session, then tear the agent down. Model catalogs use it.
pub(crate) async fn query<D, R>(
    provider: ProviderKind,
    dialect: D,
    opts: SessionOptions,
    query: impl AsyncFnOnce(&Connection, &acp::InitializeResponse) -> Result<R, AgentError>
    + Send
    + 'static,
) -> Result<R, AgentError>
where
    D: Dialect,
    R: Send + 'static,
{
    let (result_tx, result) = smol::channel::bounded(1);
    std::thread::Builder::new()
        .name("acp-query".into())
        .spawn(move || {
            let outcome = smol::block_on(async move {
                let launch = dialect.launch(&opts)?;
                let name = dialect.name().to_string();
                let client_services = dialect.client_services();
                let client_filesystem = dialect.client_filesystem();
                let mut child = spawn_agent(&name, provider, &launch, &opts)?;
                let (Some(stdin), Some(stdout), Some(stderr)) =
                    (child.stdin.take(), child.stdout.take(), child.stderr.take())
                else {
                    let _ = child.kill();
                    return Err(AgentError::Spawn(format!(
                        "ACP agent `{name}` started without piped stdio"
                    )));
                };
                // Drained so a chatty agent never blocks on a full stderr pipe.
                smol::spawn(async move {
                    let mut lines = smol::io::BufReader::new(stderr).lines();
                    while let Some(Ok(_)) = lines.next().await {}
                })
                .detach();
                let outcome = sdk::Client
                    .builder()
                    .name(format!("tcode-acp-query-{name}"))
                    .connect_with(sdk::ByteStreams::new(stdin, stdout), async |connection| {
                        Ok(
                            match initialize(&connection, &name, &launch, client_services, client_filesystem).await {
                                Ok(init) => query(&connection, &init).await,
                                Err(err) => Err(err),
                            },
                        )
                    })
                    .await;
                let _ = child.kill();
                let _ = child.status().await;
                outcome.unwrap_or_else(|err| {
                    Err(AgentError::Protocol(format!(
                        "ACP transport error: {}",
                        describe(&err)
                    )))
                })
            });
            let _ = result_tx.send_blocking(outcome);
        })
        .map_err(|err| AgentError::Spawn(format!("could not start the ACP query thread: {err}")))?;
    result
        .recv()
        .await
        .map_err(|_| AgentError::Protocol("ACP query exited without a result".into()))?
}

fn spawn_agent(
    name: &str,
    provider: ProviderKind,
    launch: &Launch,
    opts: &SessionOptions,
) -> Result<smol::process::Child, AgentError> {
    let mut cmd = crate::process::async_command(&launch.program);
    cmd.args(&launch.args)
        .current_dir(&opts.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // The dialect's env first, the user's configured env last (the user always wins).
    for (key, value) in &launch.env {
        cmd.env(key, value);
    }
    for (key, value) in opts.launch_env.pairs(provider) {
        cmd.env(key, value);
    }
    log::debug!(
        "spawning ACP agent {name}: {} {:?}",
        launch.program.display(),
        launch.args
    );
    cmd.spawn().map_err(|err| {
        AgentError::Spawn(format!(
            "could not launch ACP agent `{name}` ({}): {err}",
            launch.program.display()
        ))
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_actor<D: Dialect>(
    executor: Rc<smol::LocalExecutor<'static>>,
    provider: ProviderKind,
    dialect: Arc<D>,
    opts: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<AgentEvent>,
    ready: Sender<Result<(), AgentError>>,
) {
    let launch = match dialect.launch(&opts) {
        Ok(launch) => launch,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    let name = dialect.name().to_string();
    let mut child = match spawn_agent(&name, provider, &launch, &opts) {
        Ok(child) => child,
        Err(err) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
    };
    let _=events.try_send(AgentEvent::ProcessStarted{pid:child.id()});
    let Some(stdin) = child.stdin.take() else {
        let _ = ready
            .send(Err(AgentError::Spawn(format!(
                "ACP agent `{name}` started without piped stdin"
            ))))
            .await;
        return;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = ready
            .send(Err(AgentError::Spawn(format!(
                "ACP agent `{name}` started without piped stdout"
            ))))
            .await;
        return;
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = ready
            .send(Err(AgentError::Spawn(format!(
                "ACP agent `{name}` started without piped stderr"
            ))))
            .await;
        return;
    };

    // The agent's stderr is its log channel: keep the tail so a startup failure
    // can be reported in the agent's own words.
    let stderr_tail = crate::process::StderrTail::default();
    executor
        .spawn({
            let tail = stderr_tail.clone();
            let name = name.clone();
            async move {
                let mut lines = smol::io::BufReader::new(stderr).lines();
                while let Some(Ok(line)) = lines.next().await {
                    log::debug!("acp[{name}] stderr: {line}");
                    tail.push(line);
                }
            }
        })
        .detach();

    let mut state = State::new(opts.cwd.clone());
    state.owned_options = dialect.owned_config_options();
    state.shell_terminals = dialect.shell_terminals();
    state.vendor_configure(&opts);
    state.access_mode = opts.option_selections.iter().find(|s|s.id=="muteki:access_mode").and_then(|s|s.value.as_str()).unwrap_or("supervised").to_owned();
    let state = Arc::new(Mutex::new(state));
    let client = Client {
        events: events.clone(),
        state: state.clone(),
    };
    let (io_done_tx, io_done) = smol::channel::bounded::<String>(1);
    let pending_deliveries = Arc::new(Mutex::new(VecDeque::new()));
    let transport = sdk::ByteStreams::new(
        ObservedWriter::new(stdin, pending_deliveries.clone(), events.clone()).with_state(state.clone()),
        ObservedReader::new(stdout, io_done_tx),
    );
    let session_started = Arc::new(AtomicBool::new(false));
    let connection_result = sdk::Client
        .builder()
        .name(format!("tcode-acp-{name}"))
        // Untyped, so a `sessionUpdate` kind the schema does not know reaches
        // the dialect instead of failing to parse and being dropped.
        .on_receive_notification(
            {
                let client = client.clone();
                let dialect = dialect.clone();
                async move |notification: sdk::UntypedMessage, _connection| {
                    let (method, params) = notification.into_parts();
                    let update = (method == "session/update")
                        .then(|| acp::SessionNotification::deserialize(&params).ok())
                        .flatten();
                    let events = {
                        let mut state = client.state.lock_recover();
                        if method == "session/update" && !state.loading() {
                            let update = &params["update"];
                            if matches!(update["sessionUpdate"].as_str(), Some("tool_call" | "tool_call_update"))
                                && let (Some(id), Some(name)) = (update["toolCallId"].as_str(), update["name"].as_str())
                                && !name.is_empty()
                            {
                                state.tools.entry(id.to_owned()).or_default().name = Some(name.to_owned());
                            }
                        }
                        if method == "session/update" && let Some(events) = dialect.raw_session_update(&mut state, &params) { events } else { match update {
                            Some(update) => dialect.session_update(&mut state, update),
                            None => dialect.notification(&mut state, &method, params),
                        }}
                    };
                    client.emit_all(events).await;
                    Ok(())
                }
            },
            sdk::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let client = client.clone();
                async move |args: acp::RequestPermissionRequest, responder, connection| {
                    let client = client.clone();
                    let wire_id=serde_json::to_value(responder.id()).ok();
                    connection.spawn(async move {
                        responder.respond_with_result(client.request_permission(args,wire_id).await)
                    })?;
                    Ok(())
                }
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(client, acp::ReadTextFileRequest, read_text_file),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(client, acp::WriteTextFileRequest, write_text_file),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(
                client,
                acp::CreateTerminalRequest,
                create_terminal,
                connection
            ),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(client, acp::TerminalOutputRequest, terminal_output),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(
                client,
                acp::WaitForTerminalExitRequest,
                wait_for_terminal_exit
            ),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(client, acp::KillTerminalRequest, kill_terminal),
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            request_handler!(client, acp::ReleaseTerminalRequest, release_terminal),
            sdk::on_receive_request!(),
        )
        // Vendor methods are routed by their literal names. The schema's own
        // extension fallback only recognises `_`-prefixed methods and strips
        // the prefix, so it cannot carry Cursor's `cursor/*` methods.
        .on_receive_request(
            {
                let client = client.clone();
                let dialect = dialect.clone();
                async move |request: sdk::UntypedMessage, responder, connection| {
                    if !dialect.handles_request(&request.method) {
                        return Ok(sdk::Handled::No {
                            message: (request, responder),
                            retry: false,
                        });
                    }
                    let mut session = client.session(connection.clone());
                    session.wire_response_id=serde_json::to_value(responder.id()).ok();
                    let dialect = dialect.clone();
                    let (method, params) = request.into_parts();
                    connection.spawn(async move {
                        responder
                            .respond_with_result(dialect.request(session, method, params).await)
                    })?;
                    Ok(sdk::Handled::Yes)
                }
            },
            sdk::on_receive_request!(),
        )
        .connect_with(transport, {
            let session_started = session_started.clone();
            let stderr_tail = stderr_tail.clone();
            let actor_events = events.clone();
            let actor_ready = ready.clone();
            async move |connection| {
                connected_actor(
                    &executor,
                    provider,
                    connection,
                    dialect.as_ref(),
                    &launch,
                    &opts,
                    &commands,
                    &actor_events,
                    &actor_ready,
                    &client,
                    &stderr_tail,
                    &io_done,
                    &session_started,
                    &pending_deliveries,
                )
                .await
            }
        })
        .await;

    // The connection closure owns the protocol shutdown; this is the hard
    // process boundary for both graceful shutdowns and broken transports. A
    // startup failure is reported only once the agent is gone, so a caller
    // that gives up on the error leaves no process behind.
    let _ = child.kill();
    let _ = child.status().await;

    let connection_result = match connection_result {
        Ok(Exit::StartFailed(err)) => {
            let _ = ready.send(Err(err)).await;
            return;
        }
        Ok(Exit::Closed(reason)) => Ok(reason),
        Err(err) => Err(err),
    };
    if !session_started.load(Ordering::Acquire) {
        if let Err(err) = connection_result {
            let message = format!("ACP transport error: {}", describe(&err));
            let message = stderr_tail.append_to(message, "\n");
            let _ = ready.send(Err(AgentError::Protocol(message))).await;
        }
        return;
    }

    let close_reason = match connection_result {
        Ok(reason) => reason,
        Err(err) => Some(format!("ACP transport error: {}", describe(&err))),
    };
    let close_reason = close_reason.map(|reason| stderr_tail.append_to(reason, "\nstderr:\n"));
    let _ = events
        .send(AgentEvent::SessionClosed {
            reason: close_reason,
        })
        .await;
}

enum Exit {
    StartFailed(AgentError),
    /// The session ended, with a reason unless it was asked to.
    Closed(Option<String>),
}

#[allow(clippy::too_many_arguments)]
async fn connected_actor<D: Dialect>(
    executor: &Rc<smol::LocalExecutor<'static>>,
    provider: ProviderKind,
    connection: Connection,
    dialect: &D,
    launch: &Launch,
    opts: &SessionOptions,
    commands: &Receiver<SessionCommand>,
    events: &Sender<AgentEvent>,
    ready: &Sender<Result<(), AgentError>>,
    client: &Client,
    stderr_tail: &crate::process::StderrTail,
    io_done: &Receiver<String>,
    session_started: &AtomicBool,
    pending_deliveries: &Arc<Mutex<VecDeque<u64>>>,
) -> Result<Exit, acp::Error> {
    enum Startup {
        Handshake(Result<(Established, bool), AgentError>),
        Io(String),
    }

    let state = &client.state;
    let startup = future::or(
        async {
            Startup::Handshake(handshake(&connection, dialect, launch, opts, state, events).await)
        },
        async {
            Startup::Io(
                io_done
                    .recv()
                    .await
                    .unwrap_or_else(|_| "the ACP agent closed its stdio".to_string()),
            )
        },
    )
    .await;
    let (established, can_close) = match startup {
        Startup::Handshake(Ok(established)) => established,
        Startup::Handshake(Err(err)) => {
            let err = match err {
                AgentError::Protocol(message) => {
                    AgentError::Protocol(stderr_tail.append_to(message, "\n"))
                }
                other => other,
            };
            return Ok(Exit::StartFailed(err));
        }
        Startup::Io(reason) => {
            let message = stderr_tail.append_to(reason, "\n");
            return Ok(Exit::StartFailed(AgentError::Protocol(message)));
        }
    };
    let session_id = established.session_id;
    let model = {
        let mut state = state.lock_recover();
        state.session_id = Some(session_id.clone());
        if let Some(mut descriptor) = crate::permission_control(provider) {
            let id = descriptor_id(&descriptor).to_owned();
            let mut value = opts
                .option_selections
                .iter()
                .find(|selection| selection.id == id)
                .map(|selection| selection.value.clone())
                .or_else(|| match &descriptor {
                    OptionDescriptor::Select { default_value, .. } => {
                        default_value.as_ref().map(|value| json!(value))
                    }
                    OptionDescriptor::Boolean { default_value, .. } => Some(json!(default_value)),
                });
            if provider == ProviderKind::Cursor {
                value = Some(json!("unrestricted"));
            } else if provider == ProviderKind::Grok {
                let mut arguments = launch.args.iter();
                while let Some(argument) = arguments.next() {
                    if argument == "--permission-mode" {
                        if let Some(mode) = arguments.next() {
                            value = Some(json!(mode));
                        }
                    } else if let Some(mode) = argument.strip_prefix("--permission-mode=") {
                        value = Some(json!(mode));
                    }
                }
            }
            if let Some(value) = value {
                if let (OptionDescriptor::Select { options, .. }, Some(mode)) =
                    (&mut descriptor, value.as_str())
                    && !options.iter().any(|option| option.value == mode)
                {
                    options.push(SelectOption {
                        value: mode.to_owned(),
                        label: mode.to_owned(),
                        description: None,
                        unavailable: Some("Reported by the launch arguments".into()),
                    });
                }
                state
                    .options
                    .upsert(descriptor, OptionOrigin::Launch, value);
            }
        }
        state.default_mode_id = state.options.default_mode();
        state.options.current_model()
    };
    let model = model.map(|model| dialect.reported_model(model, opts));

    let _ = events
        .send(AgentEvent::SessionStarted {
            provider_session_id: session_id.0.to_string(),
            resume: established.resume,
            model,
        })
        .await;
    emit_provider_options(state, events, false).await;
    let planning=state.lock_recover().options.mode_origin("plan").is_some_and(|origin|origin!=OptionOrigin::Launch);
    let _=events.send(AgentEvent::RuntimeCapabilities{capabilities:json!({"plan":planning,"native_rewind":false})}).await;
    if ready.send(Ok(())).await.is_err() {
        return Ok(Exit::Closed(None));
    }
    session_started.store(true, Ordering::Release);

    let session = client.session(connection.clone());
    let (turn_tx, turn_done) = smol::channel::unbounded::<TurnOutcome>();
    let mut turn_seq: u64 = 0;

    let close_reason = loop {
        enum Input {
            Command(Result<SessionCommand, smol::channel::RecvError>),
            Turn(TurnOutcome),
            Io(String),
        }
        let input = future::or(
            future::or(async { Input::Command(commands.recv().await) }, async {
                match turn_done.recv().await {
                    Ok(outcome) => Input::Turn(outcome),
                    // This loop holds the sender, so the channel cannot close.
                    Err(_) => future::pending().await,
                }
            }),
            async {
                match io_done.recv().await {
                    Ok(reason) => Input::Io(reason),
                    Err(_) => future::pending().await,
                }
            },
        )
        .await;

        match input {
            Input::Command(Ok(SessionCommand::Shutdown)) | Input::Command(Err(_)) => break None,
            Input::Command(Ok(command)) => {
                handle_command(
                    command,
                    dialect,
                    executor,
                    &session,
                    &session_id,
                    &turn_tx,
                    &mut turn_seq,
                    pending_deliveries,
                )
                .await;
            }
            Input::Turn(outcome) => finish_turn(dialect, state, events, outcome).await,
            Input::Io(reason) => break Some(reason),
        }
    };

    // Only a live transport can acknowledge a graceful close. A dead stdout
    // has already ended the session and must not leave us awaiting a response.
    if close_reason.is_none() {
        // An agent blocked on one of our answers may not settle the close.
        cancel_pending(state, events).await;
        if can_close {
            let _ = connection
                .send_request(acp::CloseSessionRequest::new(session_id))
                .block_task()
                .await;
        }
    }

    Ok(Exit::Closed(close_reason))
}

struct TurnOutcome {
    turn_id: String,
    result: Result<acp::PromptResponse, acp::Error>,
}

/// `initialize`, then the dialect's session establishment. Also reports
/// whether the agent can close sessions.
async fn handshake<D: Dialect>(
    connection: &Connection,
    dialect: &D,
    launch: &Launch,
    opts: &SessionOptions,
    state: &Arc<Mutex<State>>,
    events: &Sender<AgentEvent>,
) -> Result<(Established, bool), AgentError> {
    let name = dialect.name();
    let init = initialize(connection, name, launch, dialect.client_services(), dialect.client_filesystem()).await?;
    let setup = Setup {
        connection,
        opts,
        init: &init,
        name,
        state,
        events,
    };
    let established = dialect.establish(&setup).await?;
    let can_close = init.agent_capabilities.session_capabilities.close.is_some();
    Ok((established, can_close))
}

async fn initialize(
    connection: &Connection,
    name: &str,
    launch: &Launch,
    client_services: bool,
    client_filesystem: bool,
) -> Result<acp::InitializeResponse, AgentError> {
    let mut client_capabilities = acp::ClientCapabilities::new()
        .fs(acp::FileSystemCapabilities::new()
            .read_text_file(client_filesystem)
            .write_text_file(client_filesystem))
        .terminal(client_services);
    client_capabilities.meta = launch.client_meta.clone().map(|mut meta|{meta.remove("elicitation");meta});
    let mut wire_capabilities=serde_json::to_value(&client_capabilities).map_err(|error|AgentError::Protocol(error.to_string()))?;
    if let Some(elicitation)=launch.client_meta.as_ref().and_then(|meta|meta.get("elicitation")){wire_capabilities["elicitation"]=elicitation.clone();}
    let initialize_params=json!({"protocolVersion":ProtocolVersion::LATEST,"clientCapabilities":wire_capabilities,"clientInfo":{"name":"muteki","version":env!("CARGO_PKG_VERSION"),"title":"Muteki"}});
    let initialize_request=sdk::UntypedMessage::new("initialize",initialize_params).map_err(|error|AgentError::Protocol(describe(&error)))?;
    // On a leash: an agent that starts but never answers `initialize` (cline
    // 3.0.39 does exactly this) would otherwise hang session startup forever,
    // with the UI stuck on "Starting…".
    let init = future::or(
        async {
            Some(
                connection
                    .send_request(
                        initialize_request,
                    )
                    .block_task()
                    .await.and_then(|response|serde_json::from_value::<acp::InitializeResponse>(response).map_err(|error|acp::Error::invalid_params().data(error.to_string()))),
            )
        },
        async {
            smol::Timer::after(INITIALIZE_TIMEOUT).await;
            None
        },
    )
    .await;
    let init = match init {
        Some(Ok(init)) => init,
        Some(Err(err)) => {
            return Err(AgentError::Protocol(format!(
                "`{name}` failed to initialize: {}",
                describe(&err)
            )));
        }
        None => {
            return Err(AgentError::Protocol(format!(
                "`{name}` did not answer `initialize` within {}s — it may not speak ACP over stdio (check its launch arguments)",
                INITIALIZE_TIMEOUT.as_secs()
            )));
        }
    };
    Ok(init)
}

/// Apply the mode for one turn and await the ACP response before the prompt is
/// written. An absent plan mode or a failed switch leaves the model untouched.
async fn select_turn_mode(
    session: &Session,
    session_id: &acp::SessionId,
    requested: &str,
) -> Result<(), String> {
    if !matches!(requested, "default" | "plan") {
        return Err(format!("unknown interaction mode `{requested}`"));
    }
    let change = {
        let mut state = session.state.lock_recover();
        if state.default_mode_id.is_none() {
            state.default_mode_id = state.options.default_mode();
        }
        let current = state.options.current_mode();
        let target = if requested == "plan" {
            "plan".to_owned()
        } else {
            match state.default_mode_id.clone() {
                Some(default) => default,
                None if current.is_none() => return Ok(()),
                None => return Err("ACP session has no default mode to restore".into()),
            }
        };
        if current.as_deref() == Some(target.as_str()) {
            return Ok(());
        }
        let origin = state
            .options
            .mode_origin(&target)
            .ok_or_else(|| format!("ACP session does not advertise `{target}` mode"))?;
        if origin == OptionOrigin::Launch {
            return Err(format!("ACP session cannot change `{target}` mode live"));
        }
        (target, origin)
    };
    let (target, origin) = change;
    let config = set_option(&session.connection, session_id, &origin, &json!(target))
        .await
        .map_err(|error| format!("ACP mode switch to `{target}` failed: {}", describe(&error)))?;
    {
        let mut state = session.state.lock_recover();
        if let Some(options) = config.as_deref() {
            state.options.ingest(None, Some(options));
        }
        if origin == OptionOrigin::Mode {
            state.select_mode(acp::SessionModeId::new(target.as_str()));
        }
    }
    if matches!(origin,OptionOrigin::Config(_)) {
        let deadline=std::time::Instant::now()+Duration::from_secs(30);
        while session.state.lock_recover().options.current_mode().as_deref()!=Some(target.as_str()) {
            if std::time::Instant::now()>deadline{return Err(format!("ACP did not confirm mode {target}"));}
            smol::Timer::after(Duration::from_millis(20)).await;
        }
    }
    emit_provider_options(&session.state, &session.events, false).await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_command<D: Dialect>(
    command: SessionCommand,
    dialect: &D,
    executor: &Rc<smol::LocalExecutor<'static>>,
    session: &Session,
    session_id: &acp::SessionId,
    turn_tx: &Sender<TurnOutcome>,
    turn_seq: &mut u64,
    pending_deliveries: &Arc<Mutex<VecDeque<u64>>>,
) {
    let connection = &session.connection;
    let state = &session.state;
    let events = &session.events;
    match command {
        SessionCommand::Steer {
            request_id,
            text,
            attachments,
        } => dialect.steer(session, request_id, text, attachments).await,
        SessionCommand::SendTurn {
            delivery_id,
            text,
            attachments,
            options,
        } => {
            *turn_seq += 1;
            let id = format!("turn-{turn_seq}");
            if let Some(requested_mode) = options
                .as_ref()
                .and_then(|options| options.interaction_mode.as_deref())
            {
                if let Err(message) = select_turn_mode(session, session_id, requested_mode).await {
                    let _ = events
                        .send(AgentEvent::TurnStarted {
                            turn_id: id.clone(),
                        })
                        .await;
                    let _ = events
                        .send(AgentEvent::Error {
                            message,
                            fatal: false,
                        })
                        .await;
                    let _ = events
                        .send(AgentEvent::TurnCompleted {
                            turn_id: id,
                            status: TurnStatus::Failed,
                            usage: None,
                        })
                        .await;
                    return;
                }
            }
            let resolved_effort = match options.as_ref().and_then(|options| options.effort.as_deref()) {
                Some("default") => state
                    .lock_recover()
                    .options
                    .records
                    .iter()
                    .find(|record| record.selection.id == EFFORT_OPTION_ID)
                    .and_then(|record| match &record.descriptor {
                        OptionDescriptor::Select { default_value: Some(default_val), .. }
                            if record.selection.value.as_str() != Some(default_val.as_str()) =>
                        {
                            Some(default_val.clone())
                        }
                        _ => None,
                    }),
                Some(effort) if !effort.is_empty() => Some(effort.to_owned()),
                _ => None,
            };
            if let Some(effort) = resolved_effort.as_deref() {
                let origin=state.lock_recover().options.origin(EFFORT_OPTION_ID);
                match origin {
                    Some(origin)=>{
                        match set_option(connection,session_id,&origin,&json!(effort)).await {
                            Ok(Some(config)) if !config.is_empty()=>{state.lock_recover().options.ingest(None,Some(&config));},
                            Ok(_)=>{},
                            Err(error)=>{let _=events.send(AgentEvent::TurnFailed{code:"acp.effort_unconfirmed".into(),message:describe(&error)}).await;let _=events.send(AgentEvent::TurnCompleted{turn_id:id,status:TurnStatus::Failed,usage:None}).await;return;}
                        }
                        let deadline=std::time::Instant::now()+Duration::from_secs(30);
                        loop {let confirmed=state.lock_recover().options.records.iter().any(|record|record.selection.id==EFFORT_OPTION_ID&&record.selection.value==effort);if confirmed{break;}if std::time::Instant::now()>deadline{let _=events.send(AgentEvent::TurnFailed{code:"acp.effort_unconfirmed".into(),message:"Agent did not confirm thinking selection".into()}).await;let _=events.send(AgentEvent::TurnCompleted{turn_id:id,status:TurnStatus::Failed,usage:None}).await;return;}smol::Timer::after(Duration::from_millis(20)).await;}
                    }
                    None=>{let _=events.send(AgentEvent::TurnFailed{code:"acp.effort_unsupported".into(),message:"Agent did not advertise thinking selection".into()}).await;let _=events.send(AgentEvent::TurnCompleted{turn_id:id,status:TurnStatus::Failed,usage:None}).await;return;}
                }
            }
            let blocks = match prompt_blocks(&text, &attachments) {
                Ok(blocks) => blocks,
                Err(message) => {
                    let _ = events.send(AgentEvent::TurnFailed { code: "acp.attachment_invalid".into(), message }).await;
                    let _ = events.send(AgentEvent::TurnCompleted { turn_id: id, status: TurnStatus::Failed, usage: None }).await;
                    return;
                }
            };
            { let mut state = state.lock_recover(); state.turn = Some(id.clone()); state.vendor_turn_started(); dialect.before_turn(&mut state); }

            let request = acp::PromptRequest::new(session_id.clone(), blocks);
            // The observed stdio writer consumes this id when the complete
            // `session/prompt` JSON-RPC line reaches the child.
            pending_deliveries.lock_recover().push_back(delivery_id);
            let request = connection.send_request(request);
            let _ = events
                .send(AgentEvent::TurnStarted {
                    turn_id: id.clone(),
                })
                .await;
            let turn_tx = turn_tx.clone();
            executor
                .spawn(async move {
                    let result = request.block_task().await;
                    let _ = turn_tx
                        .send(TurnOutcome {
                            turn_id: id,
                            result,
                        })
                        .await;
                })
                .detach();
        }
        SessionCommand::Interrupt => {
            // The open turn may be one the agent started itself.
            if state.lock_recover().turn.is_none() {
                return;
            }
            // Every in-flight permission request must be answered with
            // `cancelled` first: the protocol requires it, and the agent will
            // not settle the turn otherwise.
            cancel_pending(state, events).await;
            let _ = connection.send_notification(acp::CancelNotification::new(session_id.clone()));
            // The agent still owes us the `session/prompt` response carrying
            // `stopReason: cancelled`; the turn completes when it lands.
        }
        SessionCommand::RespondApproval {
            request_id,
            decision,
        } => {
            let approval = state.lock_recover().approvals.remove(&request_id);
            let Some((responder, options)) = approval else {
                log::warn!("acp: no pending approval {request_id}");
                return;
            };
            let outcome = match approval_outcome(&decision, &options) {
                Some(outcome) => outcome,
                None => {
                    let _ = events
                        .send(AgentEvent::Warning {
                            message:
                            "the agent offered no matching permission option; cancelling instead"
                                .into(),
                         })
                        .await;
                    acp::RequestPermissionOutcome::Cancelled
                }
            };
            let confirmation=AgentEvent::ApprovalResolved { request_id:request_id.clone(), decision };
            let deferred={let mut state=state.lock_recover();if let Some(wire_id)=state.control_wire.get(&request_id).cloned(){state.pending_wire.insert(wire_id,confirmation.clone());true}else{false}};
            if responder.send(outcome).await.is_err(){
                let _=events.send(AgentEvent::ControlDelivery{request_id,operation:"approval".into(),status:"failed".into(),error:Some(json!({"code":"runtime.request_closed","message":"Native permission request already closed"}))}).await;
            }else if !deferred {let _=events.send(confirmation).await;}
        }
        SessionCommand::SetOption { id, value } => {
            let Some(origin) = state.lock_recover().options.origin(&id) else {
                log::warn!("acp: unknown option id `{id}`");
                return;
            };
            if origin == OptionOrigin::Launch {
                return;
            }
            match set_option(connection, session_id, &origin, &value).await {
                Ok(config_options) => {
                    {
                        let mut state = state.lock_recover();
                        if let Some(options) = config_options.as_deref() {
                            state.options.ingest(None, Some(options));
                        }
                        if origin == OptionOrigin::Mode {
                            if let Some(mode) = value.as_str() {
                                state.select_mode(acp::SessionModeId::new(mode));
                            }
                        } else {
                            state.options.select(&id, value);
                        }
                    }
                    emit_provider_options(state, events, false).await;
                }
                Err(err) => {
                    let _ = events
                        .send(AgentEvent::Warning {
                            message: format!("could not apply `{id}`: {}", describe(&err)),
                        })
                        .await;
                }
            }
        }
        SessionCommand::RespondUserInput {
            request_id,
            answers,
        } => {
            let Some(answer) = state.lock_recover().inputs.remove(&request_id) else {
                log::warn!("acp: no pending user input {request_id}");
                return;
            };
            let confirmation=AgentEvent::UserInputResolved { request_id:request_id.clone(), answers:answers.clone() };
            let deferred={let mut state=state.lock_recover();if let Some(wire_id)=state.control_wire.get(&request_id).cloned(){state.pending_wire.insert(wire_id,confirmation.clone());true}else{false}};
            if answer.send(answers).await.is_err(){
                let _=events.send(AgentEvent::ControlDelivery{request_id,operation:"user_input".into(),status:"failed".into(),error:Some(json!({"code":"runtime.request_closed","message":"Native question request already closed"}))}).await;
            }else if !deferred {let _=events.send(confirmation).await;}
        }
        SessionCommand::Rewind {
            checkpoint_id,
            mode,
        } => {
            let _ = events
                .send(AgentEvent::RewindFailed {
                    checkpoint_id,
                    mode,
                    failure: crate::RewindFailure::new(crate::RewindFailureReason::Unsupported, Some("this ACP agent does not advertise a native rewind operation".into())),
                })
                .await;
        }
        SessionCommand::Compact => {
            let _ = events
                .send(AgentEvent::Warning {
                    message: "this ACP agent does not advertise a native compaction operation"
                        .into(),
                })
                .await;
        }
        SessionCommand::PreviewRewind { checkpoint_id, mode } => {
            let _ = events
                .send(AgentEvent::RewindPreviewed {
                    checkpoint_id,
                    mode,
                    files: None,
                    reason: Some("this ACP agent does not support rewind preview".into()),
                })
                .await;
        }
        SessionCommand::UndoRewind { checkpoint_id, .. } => {
            let _ = events
                .send(AgentEvent::RewindUndoFailed {
                    checkpoint_id,
                    failure: crate::RewindFailure::new(
                        crate::RewindFailureReason::Unsupported,
                        Some("this ACP agent does not support undoing rewind".into()),
                    ),
                })
                .await;
        }
        SessionCommand::StopBackgroundTask { .. }
        | SessionCommand::RetryTurn { .. }
        | SessionCommand::SetGoal { .. }
        | SessionCommand::ClearGoal => {}
        SessionCommand::Shutdown => unreachable!("handled by the caller"),
    }
}

/// Settle every request waiting on the user: permissions answer `cancelled`
/// and questions resolve with no answers.
async fn cancel_pending(state: &Arc<Mutex<State>>, events: &Sender<AgentEvent>) {
    let (approvals, inputs): (Vec<_>, Vec<_>) = {
        let mut state = state.lock_recover();
        (
            state
                .approvals
                .drain()
                .map(|(_, (responder, _))| responder)
                .collect(),
            state
                .inputs
                .drain()
                .map(|(request_id, _)| request_id)
                .collect(),
        )
    };
    for responder in approvals {
        let _ = responder
            .send(acp::RequestPermissionOutcome::Cancelled)
            .await;
    }
    for request_id in inputs {
        let _ = events
            .send(AgentEvent::UserInputResolved {
                request_id,
                answers: Map::new(),
            })
            .await;
    }
}

/// Complete a sent turn with its `session/prompt` result, unless the dialect
/// already completed it from the agent's own signal ([`State::end_turn`]).
async fn finish_turn<D: Dialect>(
    dialect: &D,
    state: &Arc<Mutex<State>>,
    events: &Sender<AgentEvent>,
    outcome: TurnOutcome,
) {
    let completion = {
        let mut state = state.lock_recover();
        if state.turn.as_deref() != Some(outcome.turn_id.as_str()) {
            return;
        }
        let end = dialect.turn_end(&mut state, outcome.result);
        let mut completion=state.take_vendor_events();completion.extend(state.complete_turn(&outcome.turn_id, end));completion
    };
    for event in completion {
        let _ = events.send(event).await;
    }
}

/// The status and message of a `session/prompt` result. A `-32800
/// request_cancelled` error is an agent aborting the prompt outright instead
/// of returning `stopReason: cancelled`.
pub(crate) fn prompt_status(
    result: &Result<acp::PromptResponse, acp::Error>,
) -> (TurnStatus, Option<String>) {
    match result {
        Ok(response) => stop_reason_status(response.stop_reason),
        Err(err) if i32::from(err.code) == -32800 => (TurnStatus::Interrupted, None),
        Err(err) => (TurnStatus::Failed, Some(describe(err))),
    }
}

/// Reports an agent closing (or breaking) stdout while still handing the bytes
/// to the SDK transport. The SDK's `ByteStreams` treats a clean EOF as a
/// completed input stream, which does not end the command loop; observed here,
/// it closes the session at once.
struct ObservedReader {
    inner: smol::process::ChildStdout,
    done: Sender<String>,
}

impl ObservedReader {
    fn new(inner: smol::process::ChildStdout, done: Sender<String>) -> Self {
        Self { inner, done }
    }
}

impl AsyncRead for ObservedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(0)) => {
                let _ = self
                    .done
                    .try_send("the ACP agent closed its stdio".to_string());
                Poll::Ready(Ok(0))
            }
            Poll::Ready(Err(err)) => {
                let _ = self.done.try_send(format!("ACP transport error: {err}"));
                Poll::Ready(Err(err))
            }
            other => other,
        }
    }
}

/// Reports a turn only after the SDK has written its complete JSON-RPC prompt
/// line to the child. `ConnectionTo::send_request` merely queues internally and
/// does not expose a synchronous enqueue failure, so it is not a delivery
/// boundary on its own.
struct ObservedWriter<W> {
    inner: W,
    line: Vec<u8>,
    state: Option<Arc<Mutex<State>>>,
    pending_deliveries: Arc<Mutex<VecDeque<u64>>>,
    events: Sender<AgentEvent>,
}

impl<W> ObservedWriter<W> {
    fn new(
        inner: W,
        pending_deliveries: Arc<Mutex<VecDeque<u64>>>,
        events: Sender<AgentEvent>,
    ) -> Self {
        Self {
            inner,
            line: Vec::new(),
            state: None,
            pending_deliveries,
            events,
        }
    }

    fn with_state(mut self, state: Arc<Mutex<State>>) -> Self { self.state=Some(state); self }

    fn finish_line(&mut self) {
        if let Ok(message)=serde_json::from_slice::<Value>(&self.line) {
            if message.get("method").is_none() && let (Some(id),Some(state))=(message.get("id"),self.state.as_ref()) {
                let key=id.to_string();
                let event={let mut state=state.lock_recover();state.control_wire.retain(|_,value|value!=&key);state.pending_wire.remove(&key)};
                if let Some(event)=event {
                    let event=if let Some(error)=message.get("error") {
                        let(request_id,operation)=match &event{AgentEvent::ApprovalResolved{request_id,..}=>(request_id.clone(),"approval"),AgentEvent::UserInputResolved{request_id,..}=>(request_id.clone(),"user_input"),_=>(String::new(),"response")};
                        AgentEvent::ControlDelivery{request_id,operation:operation.into(),status:"failed".into(),error:Some(error.clone())}
                    }else{event};let _=self.events.try_send(event);
                }
            }
        }
        let is_prompt = serde_json::from_slice::<Value>(&self.line)
            .ok()
            .is_some_and(|message| {
                message.get("method").and_then(Value::as_str) == Some("session/prompt")
            });
        self.line.clear();
        if !is_prompt {
            return;
        }
        if let Some(delivery_id) = self.pending_deliveries.lock_recover().pop_front() {
            let _ = self
                .events
                .try_send(AgentEvent::TurnAccepted { delivery_id });
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for ObservedWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let written = match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(written)) => written,
            other => return other,
        };
        for byte in &buf[..written] {
            if *byte == b'\n' {
                self.finish_line();
            } else {
                self.line.push(*byte);
            }
        }
        Poll::Ready(Ok(written))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

/// ACP's `auth_required` error.
pub(crate) fn is_auth_required(err: &acp::Error) -> bool {
    i32::from(err.code) == -32000
}

pub(crate) fn describe(err: &acp::Error) -> String {
    match &err.data {
        Some(data) => format!("{} ({data})", err.message),
        None => err.message.clone(),
    }
}

/// The `mcpServers` array for `session/new`, gated on `mcpCapabilities.http`.
fn mcp_servers(
    registrations: &[&McpRegistration],
    caps: &acp::AgentCapabilities,
) -> Vec<acp::McpServer> {
    if !caps.mcp_capabilities.http {
        return Vec::new();
    }
    registrations
        .iter()
        .map(|mcp| {
            acp::McpServer::Http(
                acp::McpServerHttp::new(mcp.name.clone(), mcp.url.clone()).headers(vec![
                    acp::HttpHeader::new("Authorization", format!("Bearer {}", mcp.bearer_token)),
                ]),
            )
        })
        .collect()
}

async fn set_option(
    connection: &Connection,
    session_id: &acp::SessionId,
    origin: &OptionOrigin,
    value: &Value,
) -> Result<Option<Vec<acp::SessionConfigOption>>, acp::Error> {
    match origin {
        OptionOrigin::Launch => Ok(None),
        OptionOrigin::Mode => {
            let Some(mode) = value.as_str() else {
                return Err(acp::Error::invalid_params());
            };
            connection
                .send_request(acp::SetSessionModeRequest::new(
                    session_id.clone(),
                    acp::SessionModeId::new(mode),
                ))
                .block_task()
                .await?;
            Ok(None)
        }
        OptionOrigin::Config(config_id) => {
            let value = match value {
                Value::Bool(value) => acp::SessionConfigOptionValue::boolean(*value),
                Value::String(value) => acp::SessionConfigOptionValue::value_id(
                    acp::SessionConfigValueId::new(value.as_str()),
                ),
                _ => return Err(acp::Error::invalid_params()),
            };
            let response = connection
                .send_request(acp::SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    config_id.clone(),
                    value,
                ))
                .block_task()
                .await?;
            Ok(Some(response.config_options))
        }
    }
}

fn approval_outcome(
    decision: &ApprovalDecision,
    options: &[ApprovalOption],
) -> Option<acp::RequestPermissionOutcome> {
    let selected = match decision {
        ApprovalDecision::Cancel => return Some(acp::RequestPermissionOutcome::Cancelled),
        ApprovalDecision::Option(id) => options
            .iter()
            .any(|option| &option.id == id)
            .then(|| id.clone()),
    }?;
    Some(acp::RequestPermissionOutcome::Selected(
        acp::SelectedPermissionOutcome::new(acp::PermissionOptionId::new(selected)),
    ))
}

/// `stopReason` → canonical turn status, plus the message to surface (if any).
pub(crate) fn stop_reason_status(reason: acp::StopReason) -> (TurnStatus, Option<String>) {
    match reason {
        acp::StopReason::EndTurn => (TurnStatus::Completed, None),
        acp::StopReason::Cancelled => (TurnStatus::Interrupted, None),
        acp::StopReason::Refusal => (
            TurnStatus::Failed,
            Some("The agent refused to continue this turn.".into()),
        ),
        acp::StopReason::MaxTokens => (
            TurnStatus::Failed,
            Some("The agent stopped: token limit reached.".into()),
        ),
        acp::StopReason::MaxTurnRequests => (
            TurnStatus::Failed,
            Some("The agent stopped: too many model requests in one turn.".into()),
        ),
        other => (
            TurnStatus::Failed,
            Some(format!("The agent stopped: {other:?}")),
        ),
    }
}

/// Application instructions must use a native instruction channel or MCP
/// server instructions; ACP prompt blocks are user content.
pub(crate) fn prompt_blocks(
    text: &str,
    attachments: &[Attachment],
) -> Result<Vec<acp::ContentBlock>, String> {
    let mut blocks = Vec::with_capacity(2 + attachments.len());
    blocks.push(acp::ContentBlock::Text(acp::TextContent::new(
        text.to_string(),
    )));
    for attachment in attachments {
        if let Some(path) = attachment.workspace_file() {
            let uri = url::Url::from_file_path(path).map_err(|_| "ACP files require an absolute workspace path".to_owned())?;
            let name = Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or(path);
            // OMP renders the link title into its model context instead of resolving its URI.
            let link = acp::ResourceLink::new(name, uri.as_str())
                .title(path.to_owned())
                .mime_type(attachment.media_type.clone());
            blocks.push(acp::ContentBlock::ResourceLink(link));
            continue;
        }
        blocks.push(acp::ContentBlock::Image(acp::ImageContent::new(
            attachment.data_base64.clone(),
            attachment.media_type.clone(),
        )));
    }
    Ok(blocks)
}

async fn emit_provider_options(
    state: &Arc<Mutex<State>>,
    events: &Sender<AgentEvent>,
    emit_empty: bool,
) {
    let event = state.lock_recover().provider_options();
    if matches!(
        &event,
        AgentEvent::ProviderOptions { descriptors, .. } if descriptors.is_empty()
    ) && !emit_empty
    {
        return;
    }
    let _ = events.send(event).await;
}

/// Where a canonical option id came from, so `SetOption` routes to the right
/// ACP method.

#[derive(Debug, Clone, PartialEq)]
enum OptionOrigin {
    Launch,
    Mode,
    Config(acp::SessionConfigId),
}

/// The agent's self-described modes and config options, mapped onto our
/// [`OptionDescriptor`]s (which the composer's traits picker already renders).
#[derive(Default)]
struct OptionRegistry {
    records: Vec<OptionRecord>,
}

struct OptionRecord {
    descriptor: OptionDescriptor,
    selection: OptionSelection,
    origin: OptionOrigin,
}

impl OptionRegistry {
    fn ingest(
        &mut self,
        modes: Option<&acp::SessionModeState>,
        config: Option<&[acp::SessionConfigOption]>,
    ) {
        if let Some(modes) = modes {
            self.upsert(
                OptionDescriptor::Select {
                    id: MODE_OPTION_ID.to_string(),
                    label: "Mode".to_string(),
                    options: modes
                        .available_modes
                        .iter()
                        .map(|mode| SelectOption {
                            value: mode.id.0.to_string(),
                            label: mode.name.clone(),
                            description: mode.description.clone(),
                            unavailable: None,
                        })
                        .collect(),
                    default_value: Some(modes.current_mode_id.0.to_string()),
                    role: crate::OptionRole::Model,
                    apply: crate::ApplyTiming::Live,
                    recommended: None,
                    permissive: None,
                },
                OptionOrigin::Mode,
                Value::String(modes.current_mode_id.0.to_string()),
            );
        }
        let prior_model = self
            .records
            .iter()
            .find(|record| record.selection.id == MODEL_OPTION_ID)
            .and_then(|record| record.selection.value.as_str())
            .map(str::to_owned);
        let incoming_model = config.and_then(|items| {
            items.iter().find_map(|option| {
                if config_option_id(&option.id.0, option.category.as_ref()) != MODEL_OPTION_ID {
                    return None;
                }
                match &option.kind {
                    acp::SessionConfigKind::Select(select) => Some(select.current_value.0.to_string()),
                    _ => None,
                }
            })
        });
        let model_unchanged = match (&prior_model, &incoming_model) {
            (Some(prev), Some(next)) => prev == next,
            (Some(_), None) => true,
            _ => false,
        };
        if let Some(config) = config {
            self.records.retain(|record| match &record.origin {
                OptionOrigin::Config(id) => config.iter().any(|option| &option.id == id),
                OptionOrigin::Mode | OptionOrigin::Launch => true,
            });
        }
        for option in config.unwrap_or_default() {
            // The mode, model and thought-level categories take tcode's
            // canonical ids (the runtime reads `acp:mode`); every option,
            // whatever its category, is set through `session/set_config_option`.
            let id = config_option_id(&option.id.0, option.category.as_ref());
            match &option.kind {
                acp::SessionConfigKind::Select(select) => {
                    let options: Vec<SelectOption> = match &select.options {
                        acp::SessionConfigSelectOptions::Ungrouped(flat) => flat
                            .iter()
                            .map(|option| SelectOption {
                                value: option.value.0.to_string(),
                                label: option.name.clone(),
                                description: option.description.clone(),
                                unavailable: None,
                            })
                            .collect(),
                        acp::SessionConfigSelectOptions::Grouped(groups) => groups
                            .iter()
                            .flat_map(|group| {
                                group.options.iter().map(move |option| SelectOption {
                                    value: option.value.0.to_string(),
                                    label: format!("{} · {}", group.name, option.name),
                                    description: option.description.clone(),
                                    unavailable: None,
                                })
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    let preserved_default = if id == EFFORT_OPTION_ID && model_unchanged {
                        self.records
                            .iter()
                            .find(|record| record.selection.id == EFFORT_OPTION_ID)
                            .and_then(|record| match &record.descriptor {
                                OptionDescriptor::Select { default_value, .. } => default_value.clone(),
                                _ => None,
                            })
                            .filter(|value| options.iter().any(|opt| opt.value == *value))
                    } else {
                        None
                    };
                    self.upsert(
                        OptionDescriptor::Select {
                            id,
                            label: option.name.clone(),
                            options,
                            default_value: preserved_default
                                .or_else(|| Some(select.current_value.0.to_string())),
                            role: crate::OptionRole::Model,
                            apply: crate::ApplyTiming::Live,
                            recommended: None,
                            permissive: None,
                        },
                        OptionOrigin::Config(option.id.clone()),
                        Value::String(select.current_value.0.to_string()),
                    );
                }
                acp::SessionConfigKind::Boolean(boolean) => self.upsert(
                    OptionDescriptor::Boolean {
                        id,
                        label: option.name.clone(),
                        default_value: boolean.current_value,
                        role: crate::OptionRole::Model,
                        apply: crate::ApplyTiming::Live,
                        recommended: None,
                        permissive: None,
                    },
                    OptionOrigin::Config(option.id.clone()),
                    Value::Bool(boolean.current_value),
                ),
                _ => log::warn!("acp: unsupported config-option kind for `{}`", option.id.0),
            }
        }
    }

    fn upsert(&mut self, descriptor: OptionDescriptor, origin: OptionOrigin, value: Value) {
        let id = descriptor_id(&descriptor).to_string();
        match self
            .records
            .iter_mut()
            .find(|record| record.selection.id == id)
        {
            Some(record) => {
                record.descriptor = descriptor;
                record.selection.value = value;
                record.origin = origin;
            }
            None => self.records.push(OptionRecord {
                descriptor,
                selection: OptionSelection { id, value },
                origin,
            }),
        }
    }

    fn select(&mut self, id: &str, value: Value) {
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.selection.id == id)
        {
            record.selection.value = value;
        }
    }

    fn origin(&self, id: &str) -> Option<OptionOrigin> {
        self.records
            .iter()
            .find(|record| record.selection.id == id)
            .map(|record| record.origin.clone())
    }

    fn current_mode(&self) -> Option<String> {
        self.records
            .iter()
            .find(|record| record.selection.id == MODE_OPTION_ID)
            .and_then(|record| record.selection.value.as_str().map(str::to_owned))
    }

    fn mode_origin(&self, wanted: &str) -> Option<OptionOrigin> {
        let record = self
            .records
            .iter()
            .find(|record| record.selection.id == MODE_OPTION_ID)?;
        let OptionDescriptor::Select { options, .. } = &record.descriptor else {
            return None;
        };
        options
            .iter()
            .any(|option| option.value == wanted)
            .then(|| record.origin.clone())
    }

    fn default_mode(&self) -> Option<String> {
        let record = self
            .records
            .iter()
            .find(|record| record.selection.id == MODE_OPTION_ID)?;
        let current = record.selection.value.as_str()?;
        if current != "plan" {
            return Some(current.to_owned());
        }
        let OptionDescriptor::Select { options, .. } = &record.descriptor else {
            return None;
        };
        options
            .iter()
            .find(|option| option.value == "agent")
            .or_else(|| options.iter().find(|option| option.value == "build"))
            .or_else(|| options.iter().find(|option| option.value != "plan"))
            .map(|option| option.value.clone())
    }

    fn current_model(&self) -> Option<String> {
        self.records
            .iter()
            .find(|record| record.selection.id == MODEL_OPTION_ID)
            .and_then(|record| record.selection.value.as_str().map(str::to_string))
    }
}

fn descriptor_id(descriptor: &OptionDescriptor) -> &str {
    match descriptor {
        OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => id,
    }
}

/// A tool call as last known. `tool_call_update` is a partial patch, so the
/// merged state lives here and every update re-renders the whole item.
#[derive(Debug, Clone, Default)]
struct ToolState {
    name: Option<String>,
    title: String,
    kind: acp::ToolKind,
    status: acp::ToolCallStatus,
    content: Vec<acp::ToolCallContent>,
    locations: Vec<acp::ToolCallLocation>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    announced: bool,
}

/// The text block currently streaming (assistant prose or thinking).
struct TextStream {
    id: String,
    kind: DeltaKind,
    text: String,
}

/// One live session's protocol state: the standard mapping's tool, text and
/// option state, the agent's terminals and the requests waiting on the user.
pub(crate) struct State {
    cwd: PathBuf,
    session_id: Option<acp::SessionId>,
    turn: Option<String>,
    last_completed_turn:Option<String>,
    /// True while `session/load` answers, which may replay history first.
    loading: bool,
    tools: HashMap<String, ToolState>,
    text: Option<TextStream>,
    approvals: HashMap<String, (Sender<acp::RequestPermissionOutcome>, Vec<ApprovalOption>)>,
    approval_seq: u64,
    inputs: HashMap<String, Sender<Map<String, Value>>>,
    input_seq: u64,
    text_seq: u64,
    terminal_seq: u64,
    terminals: HashMap<String, Arc<Terminal>>,
    usage: Option<TokenUsage>,
    options: OptionRegistry,
    /// [`Dialect::owned_config_options`].
    owned_options: &'static [&'static str],
    /// A turn the agent started while another was still open.
    next_turn: Option<String>,
    modes: Option<acp::SessionModeState>,
    /// The non-planning mode to restore when a later turn leaves plan mode.
    default_mode_id: Option<String>,
    vendor: vendor::VendorState,
    shell_terminals: bool,
    access_mode: String,
    control_wire: HashMap<String,String>,
    pending_wire: HashMap<String, AgentEvent>,
}

impl State {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            session_id: None,
            turn: None,
            last_completed_turn:None,
            loading: false,
            tools: HashMap::new(),
            text: None,
            approvals: HashMap::new(),
            approval_seq: 0,
            inputs: HashMap::new(),
            input_seq: 0,
            text_seq: 0,
            terminal_seq: 0,
            terminals: HashMap::new(),
            usage: None,
            options: OptionRegistry::default(),
            owned_options: &[],
            next_turn: None,
            modes: None,
            default_mode_id: None,
            vendor: vendor::VendorState::default(),
            shell_terminals: false,
            access_mode: "supervised".into(),
            control_wire: HashMap::new(),
            pending_wire: HashMap::new(),
        }
    }

    /// Whether `session/load` is still answering.
    pub(crate) fn loading(&self) -> bool {
        self.loading
    }

    /// The running turn's id.
    pub(crate) fn turn(&self) -> Option<&str> {
        self.turn.as_deref()
    }

    /// The latest `usage_update` context figure.
    pub(crate) fn usage(&self) -> Option<TokenUsage> {
        self.usage
    }

    /// Open a turn the agent started on its own, which no `session/prompt`
    /// answers. While another turn is still open it opens as soon as that one
    /// completes.
    pub(crate) fn begin_agent_turn(&mut self, turn_id: &str) -> Vec<AgentEvent> {
        if self.turn.is_some() {
            self.next_turn = Some(turn_id.to_string());
            return Vec::new();
        }
        self.turn = Some(turn_id.to_string());
        // Text streamed while no turn was open (a detached subagent's report)
        // is its own item, not the head of this turn's reply.
        let mut events=self.flush_text();if let Some(origin)=self.last_completed_turn.clone(){events.push(AgentEvent::ContinuationStarted{origin_turn_id:origin,turn_id:turn_id.into()});}events.push(AgentEvent::TurnStarted {
            turn_id: turn_id.to_string(),
        });events
    }

    /// Complete `turn_id` from the agent's own signal if it is still the open
    /// turn. A sent turn completed this way ignores its later `session/prompt`
    /// result. A turn the client sent while an agent-started one ran has taken
    /// its place and completes as sent turns do; only the ended turn's streamed
    /// text is closed.
    pub(crate) fn end_turn(&mut self, turn_id: &str, end: TurnEnd) -> Vec<AgentEvent> {
        if self.next_turn.as_deref() == Some(turn_id) {
            self.next_turn = None;
        }
        if self.turn.as_deref() != Some(turn_id) {
            return Vec::new();
        }
        self.complete_turn(turn_id, end)
    }

    /// Close the open turn: its streaming text, the end's message and its
    /// completion, then open the agent-started turn waiting behind it.
    fn complete_turn(&mut self, turn_id: &str, end: TurnEnd) -> Vec<AgentEvent> {
        let mut events = self.flush_text();
        self.last_completed_turn=Some(turn_id.to_string());
        self.turn = None;
        events.extend(end.message.map(|message| AgentEvent::Error {
            message,
            fatal: false,
        }));
        // A provider with client-owned terminals can legitimately leave a
        // command running past the turn (a dev server, a watcher); it is a
        // background task, not a missing completion.
        if self.shell_terminals && end.status == TurnStatus::Completed {
            let running = self.tools.values().filter(|tool|
                tool.kind == acp::ToolKind::Execute
                    && matches!(tool.status, acp::ToolCallStatus::InProgress)
            ).count()
                + self.vendor.nodes.values().filter(|node|
                    !matches!(node["status"].as_str().unwrap_or(""), "completed"|"failed"|"cancelled")
                ).count();
            if running > 0 {
                events.push(AgentEvent::BackgroundTasksChanged { count: running });
            }
        }
        events.push(AgentEvent::TurnCompleted {
            turn_id: turn_id.to_string(),
            status: end.status,
            usage: end.usage,
        });
        let mut events=events.into_iter().map(|event|AgentEvent::Origin{turn_id:turn_id.into(),agent_id:None,event:Box::new(event)}).collect::<Vec<_>>();
        if let Some(next) = self.next_turn.take() {
            events.extend(self.begin_agent_turn(&next));
        }
        events
    }

    fn ingest_modes(&mut self, modes: Option<&acp::SessionModeState>) {
        let Some(modes) = modes else {
            return;
        };
        self.modes = Some(modes.clone());
        self.select_mode(modes.current_mode_id.clone());
        self.options.ingest(Some(modes), None);
    }

    fn select_mode(&mut self, mode: acp::SessionModeId) {
        if let Some(modes) = self.modes.as_mut() {
            modes.current_mode_id = mode.clone();
        }
        self.options
            .select(MODE_OPTION_ID, Value::String(mode.0.to_string()));
    }

    fn provider_options(&self) -> AgentEvent {
        let surfaced = || {
            self.options.records.iter().filter(|record| {
                !matches!(&record.origin, OptionOrigin::Config(id)
                    if self.owned_options.contains(&id.0.as_ref()))
            })
        };
        AgentEvent::ProviderOptions {
            descriptors: surfaced().map(|record| record.descriptor.clone()).collect(),
            selections: surfaced().map(|record| record.selection.clone()).collect(),
        }
    }

    /// Close the open text block, emitting its final `ItemCompleted`.
    pub(crate) fn flush_text(&mut self) -> Vec<AgentEvent> {
        let Some(stream) = self.text.take() else {
            return Vec::new();
        };
        if stream.text.is_empty() {
            return Vec::new();
        }
        let content = match stream.kind {
            DeltaKind::ReasoningText => ItemContent::Reasoning { text: stream.text },
            _ => ItemContent::AssistantMessage { text: stream.text },
        };
        vec![AgentEvent::ItemCompleted(ThreadItem {
            id: stream.id,
            parent_item_id: None,
            content,
        })]
    }

    /// Append a streaming chunk, opening a new item when the block changes.
    fn push_text(&mut self, kind: DeltaKind, text: String) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let same_block = self.text.as_ref().is_some_and(|stream| stream.kind == kind);
        if !same_block {
            events.extend(self.flush_text());
            self.text_seq += 1;
            let prefix = match kind {
                DeltaKind::ReasoningText => "thought",
                _ => "msg",
            };
            self.text = Some(TextStream {
                id: format!("{prefix}-{}", self.text_seq),
                kind,
                text: String::new(),
            });
        }
        let stream = self.text.as_mut().expect("stream just opened");
        stream.text.push_str(&text);
        events.push(AgentEvent::Delta {
            item_id: stream.id.clone(),
            kind,
            text,
        });
        events
    }

    /// Map one `session/update` onto canonical events, merging the tool-call and
    /// option state along the way.
    pub(crate) fn apply_update(&mut self, update: acp::SessionUpdate) -> Vec<AgentEvent> {
        match update {
            // The app synthesizes the canonical user message at send time;
            // rendering the agent's echo of it would double it.
            acp::SessionUpdate::UserMessageChunk(_) => Vec::new(),
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                let text = content_text(&chunk.content);
                if text.is_empty() {
                    return Vec::new();
                }
                self.push_text(DeltaKind::AssistantText, text)
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                let text = content_text(&chunk.content);
                if text.is_empty() {
                    return Vec::new();
                }
                self.push_text(DeltaKind::ReasoningText, text)
            }
            acp::SessionUpdate::ToolCall(call) => {
                let mut events = self.flush_text();
                let id = call.tool_call_id.0.to_string();
                let tool = ToolState {
                    name: self.tools.get(&id).and_then(|tool| tool.name.clone()),
                    title: call.title,
                    kind: call.kind,
                    status: call.status,
                    content: call.content,
                    locations: call.locations,
                    raw_input: call.raw_input,
                    raw_output: call.raw_output,
                    announced: true,
                };
                let item = self.tool_item(&id, &tool);
                let existed = self
                    .tools
                    .insert(id, tool)
                    .is_some_and(|previous| previous.announced);
                events.push(if existed {
                    AgentEvent::ItemUpdated(item)
                } else {
                    AgentEvent::ItemStarted(item)
                });
                events
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                let mut events = self.flush_text();
                let id = update.tool_call_id.0.to_string();
                let entry = self.tools.entry(id.clone()).or_default();
                let fields = update.fields;
                if let Some(title) = fields.title {
                    entry.title = title;
                }
                if let Some(kind) = fields.kind {
                    entry.kind = kind;
                }
                if let Some(status) = fields.status {
                    entry.status = status;
                }
                // `content` and `locations` are whole-array replacements.
                if let Some(content) = fields.content {
                    entry.content = content;
                }
                if let Some(locations) = fields.locations {
                    entry.locations = locations;
                }
                if let Some(raw_input) = fields.raw_input {
                    entry.raw_input = Some(raw_input);
                }
                if let Some(raw_output) = fields.raw_output {
                    entry.raw_output = Some(raw_output);
                }
                let announced = std::mem::replace(&mut entry.announced, true);
                let status = entry.status;
                let tool = entry.clone();
                let item = self.tool_item(&id, &tool);
                events.push(match (announced, status) {
                    (false, _) => AgentEvent::ItemStarted(item),
                    (true, acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed) => {
                        AgentEvent::ItemCompleted(item)
                    }
                    _ => AgentEvent::ItemUpdated(item),
                });
                events
            }
            acp::SessionUpdate::Plan(plan) => vec![AgentEvent::PlanUpdated {
                turn_id: self.turn().map(str::to_owned),
                explanation: None,
                steps: plan.entries.iter().map(plan_step).collect(),
            }],
            acp::SessionUpdate::AvailableCommandsUpdate(update) => {
                vec![AgentEvent::ProviderCommands {
                    commands: update
                        .available_commands
                        .iter()
                        .map(|command| ProviderCommand {
                            name: command.name.clone(),
                            description: Some(command.description.clone()),
                            kind: ProviderCommandKind::Command,
                        })
                        .collect(),
                }]
            }
            acp::SessionUpdate::CurrentModeUpdate(update) => {
                self.select_mode(update.current_mode_id.clone());
                vec![self.provider_options()]
            }
            acp::SessionUpdate::ConfigOptionUpdate(update) => {
                self.options.ingest(None, Some(&update.config_options));
                vec![self.provider_options()]
            }
            acp::SessionUpdate::UsageUpdate(usage) => {
                let usage = TokenUsage {
                    freshness: crate::ContextFreshness::Current,
                    used_tokens: Some(usage.used),
                    context_window: Some(usage.size),
                    cost_usd: None,
                    ..Default::default()
                };
                self.usage = Some(usage);
                vec![AgentEvent::TokenUsage(usage)]
            }
            // Session titles are ours (the sidebar names sessions from the first
            // user message), and unknown variants degrade to nothing.
            _ => Vec::new(),
        }
    }

    /// The canonical item for a tool call, keyed by [`acp::ToolKind`].
    fn tool_item(&self, id: &str, tool: &ToolState) -> ThreadItem {
        let status = map_status(tool.status);
        let content = match tool.kind {
            acp::ToolKind::Execute => ItemContent::CommandExecution {
                command: command_of(tool),
                output: self.tool_output(tool),
                exit_code: self.exit_code_of(tool),
                status,
            },
            acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move => {
                let changes = file_changes(tool);
                if changes.is_empty() {
                    tool_call_content(tool, status, self.tool_output(tool))
                } else {
                    ItemContent::FileChange { changes, status }
                }
            }
            acp::ToolKind::Think => {
                let text = self.tool_output(tool);
                ItemContent::Reasoning {
                    text: if text.is_empty() {
                        tool.title.clone()
                    } else {
                        text
                    },
                }
            }
            _ => tool_call_content(tool, status, self.tool_output(tool)),
        };
        ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        }
    }

    /// Everything the tool produced, as display text: its content blocks plus
    /// the live output of any terminal it embedded.
    fn tool_output(&self, tool: &ToolState) -> String {
        let mut parts: Vec<String> = Vec::new();
        for content in &tool.content {
            match content {
                acp::ToolCallContent::Content(block) => {
                    let text = content_text(&block.content);
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
                acp::ToolCallContent::Terminal(terminal) => {
                    if let Some(terminal) = self.terminals.get(terminal.terminal_id.0.as_ref()) {
                        let output = terminal.output.lock_recover();
                        if !output.is_empty() {
                            parts.push(output.clone());
                        }
                    }
                }
                // Diffs render as FileChange, not as text.
                _ => {}
            }
        }
        if parts.is_empty()
            && let Some(raw) = &tool.raw_output
        {
            match raw.as_str() {
                Some(text) => parts.push(text.to_string()),
                None if !raw.is_null() => parts.push(raw.to_string()),
                None => {}
            }
        }
        parts.join("\n")
    }

    fn exit_code_of(&self, tool: &ToolState) -> Option<i32> {
        for content in &tool.content {
            if let acp::ToolCallContent::Terminal(terminal) = content
                && let Some(terminal) = self.terminals.get(terminal.terminal_id.0.as_ref())
                && let Some(status) = terminal.exit.lock_recover().as_ref()
            {
                return status.exit_code.map(|code| code as i32);
            }
        }
        tool.raw_output
            .as_ref()
            .and_then(|raw| raw.get("exitCode").or_else(|| raw.get("exit_code")))
            .and_then(Value::as_i64)
            .map(|code| code as i32)
    }

    /// Reject any path the agent asks for that escapes the session's cwd.
    fn resolve_path(&self, path: &Path) -> Result<PathBuf, acp::Error> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        let normalized = normalize(&absolute);
        if !normalized.starts_with(normalize(&self.cwd)) {
            return Err(acp::Error::new(
                -32602,
                format!(
                    "path `{}` is outside the session working directory",
                    path.display()
                ),
            ));
        }
        Ok(normalized)
    }
}

/// Lexical path normalization (the file may not exist yet, so `canonicalize` is
/// not an option for writes).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The shell command behind an `execute` tool call: the underlying tool's own
/// arguments when it exposed them, else the agent's title.
fn command_of(tool: &ToolState) -> String {
    tool.raw_input
        .as_ref()
        .and_then(|input| input.get("command").or_else(|| input.get("cmd")))
        .and_then(|command| match command {
            Value::String(command) => Some(command.clone()),
            Value::Array(parts) => Some(
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
        .unwrap_or_else(|| tool.title.clone())
}

fn tool_call_content(tool: &ToolState, status: ItemStatus, output: String) -> ItemContent {
    let media: Vec<_> = tool.content.iter().filter_map(|entry| {
        let acp::ToolCallContent::Content(block) = entry else { return None; };
        match &block.content {
            acp::ContentBlock::Image(_) | acp::ContentBlock::Resource(_) | acp::ContentBlock::ResourceLink(_) => {
                serde_json::to_value(&block.content).ok()
            }
            _ => None,
        }
    }).collect();
    let output = if media.is_empty() { output } else {
        let mut blocks = vec![serde_json::json!({"type":"text","text":output})];
        blocks.extend(media);
        serde_json::to_string(&blocks).expect("ACP content serializes")
    };
    ItemContent::ToolCall {
        presentation: Some(crate::ToolPresentation {
            kind: serde_json::to_value(tool.kind).expect("ACP tool kind serializes").as_str().expect("ACP tool kind is a string").to_owned(),
            title: tool.title.clone(),
            output_unavailable: tool.kind == acp::ToolKind::Read
                && status != ItemStatus::InProgress
                && tool.raw_output.is_none() && tool.content.is_empty(),
        locations: tool.locations.iter().map(|location| serde_json::json!(location)).collect(),
        }),
        image_reads: if tool.kind == acp::ToolKind::Read && status == ItemStatus::Completed {
            let images: Vec<_> = tool
                .content
                .iter()
                .filter_map(|content| match content {
                    acp::ToolCallContent::Content(block) => match &block.content {
                        acp::ContentBlock::Image(image)
                            if image.mime_type.starts_with("image/") && !image.data.is_empty() =>
                        {
                            Some(Attachment {
                                media_type: image.mime_type.clone(),
                                data_base64: image.data.clone(),
                                source_path: None,
                            })
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            if images.is_empty() {
                let raw = tool.raw_output.as_ref().unwrap_or(&Value::Null);
                crate::image_reads::content_images(raw.get("content").unwrap_or(raw))
            } else {
                images
            }
        } else {
            Vec::new()
        },
        name: tool.name.clone().unwrap_or_else(|| tool.title.clone()),
        input: tool.raw_input.clone().unwrap_or(Value::Null),
        output: (!output.is_empty()).then_some(output),
        status,
    }
}

/// `diff` content blocks → canonical [`FileChange`]s (with a unified diff, so
/// the diff panel renders them like every other provider's edits).
fn file_changes(tool: &ToolState) -> Vec<FileChange> {
    let mut changes: Vec<FileChange> = tool
        .content
        .iter()
        .filter_map(|content| match content {
            acp::ToolCallContent::Diff(diff) => {
                let path = diff.path.to_string_lossy().into_owned();
                Some(FileChange {
                    kind: match (tool.kind, diff.old_text.as_deref()) {
                        (acp::ToolKind::Delete, _) => FileChangeKind::Delete,
                        (acp::ToolKind::Move, _) => FileChangeKind::Rename,
                        (_, None | Some("")) => FileChangeKind::Create,
                        _ => FileChangeKind::Modify,
                    },
                    diff: Some(unified_diff(
                        &path,
                        diff.old_text.as_deref().unwrap_or(""),
                        &diff.new_text,
                    )),
                    path,
                })
            }
            _ => None,
        })
        .collect();
    if changes.is_empty() && matches!(tool.kind, acp::ToolKind::Delete | acp::ToolKind::Move) {
        // Deletes and renames carry no diff; the locations are all we get.
        changes = tool
            .locations
            .iter()
            .map(|location| FileChange {
                path: location.path.to_string_lossy().into_owned(),
                kind: match tool.kind {
                    acp::ToolKind::Delete => FileChangeKind::Delete,
                    _ => FileChangeKind::Rename,
                },
                diff: None,
            })
            .collect();
    }
    changes
}

/// A whole-file unified diff. ACP hands us before/after text rather than a
/// patch, and the diff panel wants `@@` hunks; a single hunk covering the file
/// is exactly what the panel's full-file path already renders.
fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut out = String::new();
    if old_lines.is_empty() {
        out.push_str("--- /dev/null\n");
    } else {
        let _ = writeln!(out, "--- a/{path}");
    }
    if new_lines.is_empty() {
        out.push_str("+++ /dev/null\n");
    } else {
        let _ = writeln!(out, "+++ b/{path}");
    }
    let _ = writeln!(
        out,
        "@@ -{},{} +{},{} @@",
        usize::from(!old_lines.is_empty()),
        old_lines.len(),
        usize::from(!new_lines.is_empty()),
        new_lines.len()
    );
    // Trim the common prefix/suffix so the (very common) single-line edit does
    // not render as a whole-file rewrite.
    let prefix = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(old, new)| old == new)
        .count();
    let suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take_while(|(old, new)| old == new)
        .count()
        .min(old_lines.len() - prefix)
        .min(new_lines.len() - prefix);
    for line in &old_lines[..prefix] {
        let _ = writeln!(out, " {line}");
    }
    for line in &old_lines[prefix..old_lines.len() - suffix] {
        let _ = writeln!(out, "-{line}");
    }
    for line in &new_lines[prefix..new_lines.len() - suffix] {
        let _ = writeln!(out, "+{line}");
    }
    for line in &old_lines[old_lines.len() - suffix..] {
        let _ = writeln!(out, " {line}");
    }
    out
}

fn map_status(status: acp::ToolCallStatus) -> ItemStatus {
    match status {
        acp::ToolCallStatus::Pending | acp::ToolCallStatus::InProgress => ItemStatus::InProgress,
        acp::ToolCallStatus::Completed => ItemStatus::Completed,
        acp::ToolCallStatus::Failed => ItemStatus::Failed,
        _ => ItemStatus::InProgress,
    }
}

fn plan_step(entry: &acp::PlanEntry) -> PlanStep {
    PlanStep {
        step: entry.content.clone(),
        status: match entry.status {
            acp::PlanEntryStatus::InProgress => PlanStepStatus::InProgress,
            acp::PlanEntryStatus::Completed => PlanStepStatus::Completed,
            _ => PlanStepStatus::Pending,
        },
    }
}

/// Displayable text for a content block (images/audio degrade to a marker).
fn content_text(block: &acp::ContentBlock) -> String {
    match block {
        acp::ContentBlock::Text(text) => text.text.clone(),
        acp::ContentBlock::ResourceLink(link) => link.uri.clone(),
        acp::ContentBlock::Resource(resource) => match &resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(text) => text.text.clone(),
            _ => String::new(),
        },
        acp::ContentBlock::Image(_) => "[image]".to_string(),
        acp::ContentBlock::Audio(_) => "[audio]".to_string(),
        _ => String::new(),
    }
}

/// The approval an agent's `session/request_permission` becomes.
fn approval_request(
    id: String,
    turn_id: Option<String>,
    tool: &acp::ToolCallUpdate,
    options: &[acp::PermissionOption],
) -> ApprovalRequest {
    let fields = &tool.fields;
    let title = fields.title.clone().unwrap_or_default();
    let kind = fields.kind.unwrap_or_default();
    let raw_input = fields.raw_input.clone().unwrap_or(Value::Null);
    let native_execution=Some(json!({"tool_name":if title.is_empty(){serde_json::to_value(kind).unwrap_or(json!("other")).as_str().unwrap_or("other").to_owned()}else{title.clone()},"action":if title.is_empty(){serde_json::to_value(kind).unwrap_or(json!("other")).as_str().unwrap_or("other").to_owned()}else{title.clone()},"arguments":if raw_input.is_object(){raw_input.clone()}else{json!({"rawInput":raw_input,"toolCall":tool})},"resource_kind":match kind{acp::ToolKind::Execute=>"process",acp::ToolKind::Edit|acp::ToolKind::Delete|acp::ToolKind::Move=>"file",_=>"mcp"}}));
    let approval_kind = match kind {
        acp::ToolKind::Execute => ApprovalKind::ExecCommand {
            command: command_of(&ToolState {
                title: title.clone(),
                raw_input: fields.raw_input.clone(),
                ..Default::default()
            }),
            cwd: None,
            reason: None,
        },
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move => {
            let changes = file_changes(&ToolState {
                title: title.clone(),
                kind,
                content: fields.content.clone().unwrap_or_default(),
                locations: fields.locations.clone().unwrap_or_default(),
                raw_input: fields.raw_input.clone(),
                ..Default::default()
            });
            if changes.is_empty() {
                ApprovalKind::ToolUse {
                    name: title.clone(),
                    input: raw_input,
                    detail: title,
                }
            } else {
                ApprovalKind::FileChange {
                    changes,
                    reason: None,
                }
            }
        }
        acp::ToolKind::Read | acp::ToolKind::Search | acp::ToolKind::Fetch => {
            ApprovalKind::FileRead { detail: title }
        }
        _ => ApprovalKind::ToolUse {
            name: title.clone(),
            input: raw_input,
            detail: title,
        },
    };
    ApprovalRequest {native_execution, tool_call_id: Some(tool.tool_call_id.0.to_string()), 
        id,
        parent_item_id: None,
        turn_id,
        kind: approval_kind,
        options: options
            .iter()
            .map(|option| ApprovalOption {
                id: option.option_id.0.to_string(),
                label: option.name.clone(),
                kind: match option.kind {
                    acp::PermissionOptionKind::AllowOnce => ApprovalOptionKind::AllowOnce,
                    acp::PermissionOptionKind::AllowAlways => ApprovalOptionKind::AllowAlways,
                    acp::PermissionOptionKind::RejectAlways => ApprovalOptionKind::RejectAlways,
                    _ => ApprovalOptionKind::RejectOnce,
                },
            })
            .collect(),
    }
}

/// The standard client services the agent calls into.
#[derive(Clone)]
struct Client {
    events: Sender<AgentEvent>,
    state: Arc<Mutex<State>>,
}

impl Client {
    fn session(&self, connection: Connection) -> Session {
        Session {
            connection,
            events: self.events.clone(),
            state: self.state.clone(),
            wire_response_id: None,
        }
    }

    async fn emit_all(&self, events: Vec<AgentEvent>) {
        for event in events {
            let _ = self.events.send(event).await;
        }
    }

    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
        wire_id: Option<Value>,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        let (request_id, turn) = {
            let mut state = self.state.lock_recover();
            state.approval_seq += 1;
            (
                format!("acp-approval-{}", state.approval_seq),
                state.turn().map(str::to_owned),
            )
        };
        if let Some(wire_id)=wire_id { self.state.lock_recover().control_wire.insert(request_id.clone(),wire_id.to_string()); }
        let mut tool_call=args.tool_call.clone();
        if let Some(prior)=self.state.lock_recover().tools.get(&args.tool_call.tool_call_id.0.to_string()){
            if tool_call.fields.title.is_none(){tool_call.fields.title=Some(prior.title.clone());}if tool_call.fields.kind.is_none(){tool_call.fields.kind=Some(prior.kind);}if tool_call.fields.raw_input.is_none(){tool_call.fields.raw_input=prior.raw_input.clone();}
        }
        let request = approval_request(request_id.clone(), turn, &tool_call, &args.options);
        let (responder, decided) = smol::channel::bounded(1);
        {
            let mut state = self.state.lock_recover();
            state
                .approvals
                .insert(request_id.clone(), (responder, request.options.clone()));
            // Keep the tool card in step with what we are asking about.
            let id = args.tool_call.tool_call_id.0.to_string();
            let entry = state.tools.entry(id).or_default();
            if let Some(title) = args.tool_call.fields.title.clone() {
                entry.title = title;
            }
            if let Some(kind) = args.tool_call.fields.kind {
                entry.kind = kind;
            }
            if let Some(raw_input) = args.tool_call.fields.raw_input.clone() {
                entry.raw_input = Some(raw_input);
            }
        }
        let _ = self
            .events
            .send(AgentEvent::ApprovalRequested(request))
            .await;

        let outcome = decided
            .recv()
            .await
            .unwrap_or(acp::RequestPermissionOutcome::Cancelled);
        { let mut state=self.state.lock_recover(); state.approvals.remove(&request_id); let mut hydrated=args;hydrated.tool_call=tool_call;state.remember_terminal_grant(&hydrated,&outcome); }
        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        let path = self.state.lock_recover().resolve_path(&args.path)?;
        let content = smol::fs::read_to_string(&path).await.map_err(|err| {
            acp::Error::new(-32603, format!("could not read {}: {err}", path.display()))
        })?;
        // `line` is 1-based; `limit` counts lines from there.
        let content = match (args.line, args.limit) {
            (None, None) => content,
            (line, limit) => {
                let start = line.unwrap_or(1).saturating_sub(1) as usize;
                let lines: Vec<&str> = content.lines().skip(start).collect();
                let end = limit.map_or(lines.len(), |limit| lines.len().min(limit as usize));
                lines[..end].join("\n")
            }
        };
        Ok(acp::ReadTextFileResponse::new(content))
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse, acp::Error> {
        let path = self.state.lock_recover().resolve_path(&args.path)?;
        if let Some(parent) = path.parent() {
            smol::fs::create_dir_all(parent).await.map_err(|err| {
                acp::Error::new(
                    -32603,
                    format!("could not create {}: {err}", parent.display()),
                )
            })?;
        }
        smol::fs::write(&path, args.content).await.map_err(|err| {
            acp::Error::new(-32603, format!("could not write {}: {err}", path.display()))
        })?;
        Ok(acp::WriteTextFileResponse::new())
    }

    async fn create_terminal(
        &self,
        args: acp::CreateTerminalRequest,
        connection: &Connection,
    ) -> Result<acp::CreateTerminalResponse, acp::Error> {
        let cwd = match &args.cwd {
            Some(cwd) => self.state.lock_recover().resolve_path(cwd)?,
            None => self.state.lock_recover().cwd.clone(),
        };
        let shell=self.state.lock_recover().shell_terminals;
        let mut command=args.command.clone();let mut argv=args.args.clone();
        if shell {
            let quote=|part:&str|format!("'{}'",part.replace('\'',"'\\''"));
            let source=if argv.is_empty(){command.clone()}else{format!("{} {}",command,argv.iter().map(|arg|quote(arg)).collect::<Vec<_>>().join(" "))};
            if source.trim().is_empty(){return Err(acp::Error::invalid_params().data("terminal/create requires a command"));}
            let (mode,granted)={let state=self.state.lock_recover();(state.access_mode.clone(),state.terminal_granted(source.trim()))};
            if !granted {
                if matches!(mode.as_str(),"supervised"|"auto-accept-edits") {
                    let request:acp::RequestPermissionRequest=serde_json::from_value(json!({"sessionId":args.session_id,"toolCall":{"toolCallId":format!("terminal-{}",uuid::Uuid::new_v4()),"title":source,"kind":"execute","rawInput":{"command":source}},"options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"allow-session","name":"Allow for session","kind":"allow_always"},{"optionId":"reject-once","name":"Reject","kind":"reject_once"}]})).map_err(|error|acp::Error::new(-32603,error.to_string()))?;
                    let response=self.request_permission(request,None).await?;
                    if !matches!(response.outcome,acp::RequestPermissionOutcome::Selected(ref selected) if matches!(selected.option_id.0.as_ref(),"allow-once"|"allow-session")){return Err(acp::Error::new(-32000,"terminal/create rejected"));}
                }else if !matches!(mode.as_str(),"auto"|"full-access"){return Err(acp::Error::new(-32000,"terminal/create disallowed by access mode"));}
            }
            command="/bin/sh".into();argv=vec!["-lc".into(),source];
        }
        let terminal = Terminal::spawn(
            connection,
            &command,
            &argv,
            &args.env,
            &cwd,
            args.output_byte_limit
                .unwrap_or(DEFAULT_TERMINAL_OUTPUT_LIMIT),
        )?;
        let id = {
            let mut state = self.state.lock_recover();
            state.terminal_seq += 1;
            let id = format!("term-{}", state.terminal_seq);
            state.terminals.insert(id.clone(), terminal);
            id
        };
        Ok(acp::CreateTerminalResponse::new(acp::TerminalId::new(id)))
    }

    async fn terminal_output(
        &self,
        args: acp::TerminalOutputRequest,
    ) -> Result<acp::TerminalOutputResponse, acp::Error> {
        let terminal = self.terminal(&args.terminal_id)?;
        let output = terminal.output.lock_recover().clone();
        let truncated = *terminal.truncated.lock_recover();
        let exit_status = terminal.exit.lock_recover().clone();
        Ok(acp::TerminalOutputResponse::new(output, truncated).exit_status(exit_status))
    }

    async fn wait_for_terminal_exit(
        &self,
        args: acp::WaitForTerminalExitRequest,
    ) -> Result<acp::WaitForTerminalExitResponse, acp::Error> {
        let terminal = self.terminal(&args.terminal_id)?;
        // The sender is dropped once the process exits, closing the channel.
        let _ = terminal.done.recv().await;
        let exit_status = terminal
            .exit
            .lock_recover()
            .clone()
            .unwrap_or_else(acp::TerminalExitStatus::new);
        Ok(acp::WaitForTerminalExitResponse::new(exit_status))
    }

    async fn kill_terminal(
        &self,
        args: acp::KillTerminalRequest,
    ) -> Result<acp::KillTerminalResponse, acp::Error> {
        self.terminal(&args.terminal_id)?.kill();
        Ok(acp::KillTerminalResponse::new())
    }

    async fn release_terminal(
        &self,
        args: acp::ReleaseTerminalRequest,
    ) -> Result<acp::ReleaseTerminalResponse, acp::Error> {
        let terminal = self
            .state
            .lock_recover()
            .terminals
            .remove(args.terminal_id.0.as_ref());
        if let Some(terminal) = terminal {
            terminal.kill();
        }
        Ok(acp::ReleaseTerminalResponse::new())
    }
    fn terminal(&self, id: &acp::TerminalId) -> Result<Arc<Terminal>, acp::Error> {
        self.state
            .lock_recover()
            .terminals
            .get(id.0.as_ref())
            .cloned()
            .ok_or_else(|| acp::Error::new(-32002, format!("unknown terminal `{}`", id.0)))
    }
}

/// A command the agent asked us to run. Headless: we capture the output and
/// serve `terminal/output` / `terminal/wait_for_exit` from it, and the text is
/// folded into the owning tool card (`ToolCallContent::Terminal`). Wiring these
/// into the terminal drawer needs a canonical event the contract does not have.
struct Terminal {
    child: Mutex<smol::process::Child>,
    output: Mutex<String>,
    truncated: Mutex<bool>,
    exit: Mutex<Option<acp::TerminalExitStatus>>,
    /// Closed (never sent on) once the process has exited.
    done: Receiver<()>,
}

impl Terminal {
    fn spawn(
        connection: &Connection,
        command: &str,
        args: &[String],
        env: &[acp::EnvVariable],
        cwd: &Path,
        limit: u64,
    ) -> Result<Arc<Self>, acp::Error> {
        let program = crate::resolve_binary(None, command)
            .map_err(|err| acp::Error::new(-32603, err.to_string()))?;
        let mut cmd = crate::process::async_command(&program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for var in env {
            cmd.env(&var.name, &var.value);
        }
        let mut child = cmd
            .spawn()
            .map_err(|err| acp::Error::new(-32603, format!("could not run `{command}`: {err}")))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            acp::Error::new(-32603, format!("`{command}` started without piped stdout"))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            acp::Error::new(-32603, format!("`{command}` started without piped stderr"))
        })?;
        let (done_tx, done) = smol::channel::bounded::<()>(1);

        let terminal = Arc::new(Terminal {
            child: Mutex::new(child),
            output: Mutex::new(String::new()),
            truncated: Mutex::new(false),
            exit: Mutex::new(None),
            done,
        });

        // stdout and stderr interleave into one buffer, as they do in a terminal.
        let streams: [Box<dyn smol::io::AsyncRead + Unpin + Send>; 2] =
            [Box::new(stdout), Box::new(stderr)];
        for stream in streams {
            connection.spawn({
                let terminal = terminal.clone();
                async move {
                    let mut stream = stream;
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => terminal.append(&buf[..read], limit),
                        }
                    }
                    Ok(())
                }
            })?;
        }

        connection.spawn({
            let terminal = terminal.clone();
            async move {
                loop {
                    let status = terminal.child.lock_recover().try_status();
                    match status {
                        Ok(Some(status)) => {
                            *terminal.exit.lock_recover() = Some(exit_status(&status));
                            break;
                        }
                        Err(err) => {
                            log::warn!("acp terminal: {err}");
                            break;
                        }
                        Ok(None) => smol::Timer::after(Duration::from_millis(25)).await,
                    };
                }
                // Closing the channel wakes every `wait_for_exit`.
                drop(done_tx);
                Ok(())
            }
        })?;

        Ok(terminal)
    }

    fn append(&self, bytes: &[u8], limit: u64) {
        let mut output = self.output.lock_recover();
        output.push_str(&String::from_utf8_lossy(bytes));
        let limit = limit as usize;
        if output.len() > limit {
            // Keep the tail (what ACP asks for), cutting on a char boundary.
            let mut cut = output.len() - limit;
            while !output.is_char_boundary(cut) {
                cut += 1;
            }
            output.drain(..cut);
            *self.truncated.lock_recover() = true;
        }
    }

    fn kill(&self) {
        let _ = self.child.lock_recover().kill();
    }
}

fn exit_status(status: &std::process::ExitStatus) -> acp::TerminalExitStatus {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal().map(|signal| signal.to_string())
    };
    #[cfg(not(unix))]
    let signal: Option<String> = None;
    acp::TerminalExitStatus::new()
        .exit_code(status.code().map(|code| code as u32))
        .signal(signal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> State {
        State::new(PathBuf::from("/tmp/tcode-acp-test"))
    }

    fn update(json: Value) -> acp::SessionUpdate {
        serde_json::from_value(json).expect("valid session/update payload")
    }

    #[test]
    fn prompt_acceptance_follows_the_complete_stdio_write() {
        smol::block_on(async {
            use smol::prelude::*;

            let pending = Arc::new(Mutex::new(VecDeque::from([41])));
            let (events, received) = smol::channel::unbounded();
            let inner = smol::io::Cursor::new(Vec::new());
            let mut writer = ObservedWriter::new(inner, pending.clone(), events);

            writer
                .write_all(
                    br#"{"jsonrpc":"2.0","method":"initialize"}
"#,
                )
                .await
                .unwrap();
            writer
                .write_all(br#"{"jsonrpc":"2.0","method":"session/prompt"}"#)
                .await
                .unwrap();
            assert!(
                received.try_recv().is_err(),
                "an incomplete line must not ack"
            );

            writer.write_all(b"\n").await.unwrap();
            assert!(matches!(
                received.recv().await.unwrap(),
                AgentEvent::TurnAccepted { delivery_id: 41 }
            ));
            assert!(pending.lock_recover().is_empty());
        });
    }

    /// The preview MCP server is a loopback HTTP endpoint: it may only be handed
    /// to agents that advertise `mcpCapabilities.http`.
    #[test]
    fn mcp_server_is_gated_on_the_http_capability() {
        let registration = McpRegistration {
            name: McpRegistration::SERVER_NAME_PREVIEW.into(),
            url: "http://127.0.0.1:5321/mcp".into(),
            bearer_token: "tok".into(),
        };
        let mut caps = acp::AgentCapabilities::default();
        assert!(
            mcp_servers(&[&registration], &caps).is_empty(),
            "an agent without mcpCapabilities.http must not be sent the HTTP server"
        );

        caps.mcp_capabilities.http = true;
        let servers = mcp_servers(&[&registration], &caps);
        let value = serde_json::to_value(&servers[0]).unwrap();
        assert_eq!(value["type"], "http");
        assert_eq!(value["name"], "tcode_preview");
        assert_eq!(value["url"], "http://127.0.0.1:5321/mcp");
        assert_eq!(value["headers"][0]["name"], "Authorization");
        assert_eq!(value["headers"][0]["value"], "Bearer tok");

        // No preview server running → nothing to send, capability or not.
        assert!(mcp_servers(&[], &caps).is_empty());

        let orchestrate = McpRegistration {
            name: McpRegistration::SERVER_NAME_ORCHESTRATE.into(),
            url: "http://127.0.0.1:5321/mcp".into(),
            bearer_token: "other".into(),
        };
        assert_eq!(mcp_servers(&[&registration, &orchestrate], &caps).len(), 2);
        assert_eq!(mcp_servers(&[&orchestrate], &caps).len(), 1);

        let computer_use = McpRegistration {
            name: McpRegistration::SERVER_NAME_COMPUTER_USE.into(),
            url: "http://127.0.0.1:5322/mcp".into(),
            bearer_token: "computer-token".into(),
        };
        let servers = mcp_servers(&[&registration, &orchestrate, &computer_use], &caps);
        assert_eq!(servers.len(), 3);
        let value = serde_json::to_value(&servers[2]).unwrap();
        assert_eq!(value["name"], "tcode_computer_use");
        assert_eq!(value["headers"][0]["value"], "Bearer computer-token");
    }

    #[test]
    fn streamed_text_keeps_block_identity_across_reasoning_and_ignores_user_echoes() {
        let mut state = state();
        let mut previous = None;
        for (kind, wire_kind, chunks, expected) in [
            (
                DeltaKind::AssistantText,
                "agent_message_chunk",
                ["Hel", "lo"],
                "Hello",
            ),
            (
                DeltaKind::ReasoningText,
                "agent_thought_chunk",
                ["Think", "ing"],
                "Thinking",
            ),
            (
                DeltaKind::AssistantText,
                "agent_message_chunk",
                ["An", "swer"],
                "Answer",
            ),
        ] {
            let mut id = None;
            for chunk in chunks {
                let mut events = state
                    .apply_update(update(json!({
                        "sessionUpdate": wire_kind, "content": {"type":"text", "text":chunk}
                    })))
                    .into_iter();
                if let Some((previous_id, previous_kind, previous_text)) = previous.take() {
                    let Some(AgentEvent::ItemCompleted(item)) = events.next() else {
                        panic!("changing text kind must complete the previous block");
                    };
                    assert_eq!(item.id, previous_id);
                    match (previous_kind, item.content) {
                        (DeltaKind::AssistantText, ItemContent::AssistantMessage { text })
                        | (DeltaKind::ReasoningText, ItemContent::Reasoning { text }) => {
                            assert_eq!(text, previous_text)
                        }
                        other => panic!("wrong completed block: {other:?}"),
                    }
                }
                let Some(AgentEvent::Delta {
                    item_id,
                    kind: actual_kind,
                    text,
                }) = events.next()
                else {
                    panic!("text chunk must stream a delta");
                };
                assert_eq!(actual_kind, kind);
                assert_eq!(text, chunk);
                if let Some(id) = &id {
                    assert_eq!(&item_id, id);
                }
                id = Some(item_id);
                assert!(events.next().is_none());
                assert!(state.apply_update(update(json!({
                    "sessionUpdate":"user_message_chunk", "content":{"type":"text", "text":"prompt echo"}
                }))).is_empty());
            }
            previous = Some((id.unwrap(), kind, expected));
        }
        let (id, _, expected) = previous.unwrap();
        assert!(matches!(state.flush_text().as_slice(),
            [AgentEvent::ItemCompleted(ThreadItem { id: actual_id, content: ItemContent::AssistantMessage { text }, .. })]
                if actual_id == &id && text == expected));
        assert!(state.flush_text().is_empty());
    }

    #[test]
    fn execute_tool_call_maps_to_a_command_execution() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Run tests",
            "kind": "execute",
            "status": "in_progress",
            "rawInput": { "command": "cargo test" }
        })));
        match &events[0] {
            AgentEvent::ItemStarted(item) => {
                assert_eq!(item.id, "t1");
                match &item.content {
                    ItemContent::CommandExecution {
                        command, status, ..
                    } => {
                        assert_eq!(command, "cargo test");
                        assert_eq!(*status, ItemStatus::InProgress);
                    }
                    other => panic!("expected CommandExecution, got {other:?}"),
                }
            }
            other => panic!("expected ItemStarted, got {other:?}"),
        }

        // A partial patch merges into the merged state and completes the item.
        let events = state.apply_update(update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "ok" } }],
            "rawOutput": { "exitCode": 0 }
        })));
        match &events[0] {
            AgentEvent::ItemCompleted(item) => match &item.content {
                ItemContent::CommandExecution {
                    command,
                    output,
                    exit_code,
                    status,
                } => {
                    assert_eq!(command, "cargo test", "rawInput must survive the patch");
                    assert_eq!(output, "ok");
                    assert_eq!(*exit_code, Some(0));
                    assert_eq!(*status, ItemStatus::Completed);
                }
                other => panic!("expected CommandExecution, got {other:?}"),
            },
            other => panic!("expected ItemCompleted, got {other:?}"),
        }
    }

    #[test]
    fn file_tools_preserve_operation_kind_and_render_available_change_evidence() {
        for (kind, old, new, operation, diff) in [
            (
                "edit",
                Some("same\nbefore\nend\n"),
                "same\nafter\nend\n",
                FileChangeKind::Modify,
                "--- a/file.rs\n+++ b/file.rs\n@@ -1,3 +1,3 @@\n same\n-before\n+after\n end\n",
            ),
            (
                "edit",
                None,
                "new\n",
                FileChangeKind::Create,
                "--- /dev/null\n+++ b/file.rs\n@@ -0,0 +1,1 @@\n+new\n",
            ),
            (
                "edit",
                Some(""),
                "new\n",
                FileChangeKind::Create,
                "--- /dev/null\n+++ b/file.rs\n@@ -0,0 +1,1 @@\n+new\n",
            ),
            (
                "delete",
                Some("old\n"),
                "",
                FileChangeKind::Delete,
                "--- a/file.rs\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-old\n",
            ),
            (
                "move",
                Some("old\n"),
                "new\n",
                FileChangeKind::Rename,
                "--- a/file.rs\n+++ b/file.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n",
            ),
        ] {
            let events = state().apply_update(update(json!({
                "sessionUpdate":"tool_call", "toolCallId":"file", "title":"Change file",
                "kind":kind, "status":"completed",
                "content":[{"type":"diff","path":"file.rs","oldText":old,"newText":new}]
            })));
            let [
                AgentEvent::ItemStarted(ThreadItem {
                    content: ItemContent::FileChange { changes, status },
                    ..
                }),
            ] = events.as_slice()
            else {
                panic!("{kind}: expected file item, got {events:?}");
            };
            assert_eq!(*status, ItemStatus::Completed);
            assert_eq!(
                changes,
                &[FileChange {
                    path: "file.rs".into(),
                    kind: operation,
                    diff: Some(diff.into())
                }]
            );
        }
        for (kind, operation) in [
            ("delete", FileChangeKind::Delete),
            ("move", FileChangeKind::Rename),
        ] {
            let events = state().apply_update(update(json!({
                "sessionUpdate":"tool_call", "toolCallId":"file", "title":"Change file",
                "kind":kind, "status":"completed", "locations":[{"path":"one.rs"},{"path":"two.rs"}]
            })));
            let [
                AgentEvent::ItemStarted(ThreadItem {
                    content: ItemContent::FileChange { changes, .. },
                    ..
                }),
            ] = events.as_slice()
            else {
                panic!("{kind}: expected file locations, got {events:?}");
            };
            assert_eq!(
                changes,
                &[
                    FileChange {
                        path: "one.rs".into(),
                        kind: operation,
                        diff: None
                    },
                    FileChange {
                        path: "two.rs".into(),
                        kind: operation,
                        diff: None
                    },
                ]
            );
        }
        let events = state().apply_update(update(json!({
            "sessionUpdate":"tool_call", "toolCallId":"file", "title":"Opaque edit",
            "kind":"edit", "status":"completed", "rawInput":{"path":"file.rs"}, "rawOutput":"done"
        })));
        assert!(
            matches!(events.as_slice(), [AgentEvent::ItemStarted(ThreadItem {
            content:ItemContent::ToolCall {
            image_reads: _, name, input, output:Some(output), status:ItemStatus::Completed, .. }, ..
        })] if name == "Opaque edit" && input == &json!({"path":"file.rs"}) && output == "done")
        );
    }

    #[test]
    fn image_read_results_follow_partial_updates_and_exclude_generation() {
        for (kind, name, status, expected) in [
            ("read", "file", "completed", 1),
            ("read", "file", "failed", 0),
            ("other", "generate_image", "completed", 0),
        ] {
            let mut mapper = state();
            mapper.apply_update(update(json!({"sessionUpdate":"tool_call","toolCallId":"read","title":"Image operation","kind":kind,"name":name,"status":"in_progress"})));
            let events = mapper.apply_update(update(json!({"sessionUpdate":"tool_call_update","toolCallId":"read","status":status,"content":[{"type":"content","content":{"type":"image","mimeType":"image/png","data":"AQID"}}]})));
            let AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::ToolCall { image_reads, .. },
                ..
            }) = &events[0]
            else {
                panic!("tool completion: {events:?}")
            };
            assert_eq!(image_reads.len(), expected, "{kind}, {name}, {status}");
            if expected != 0 {
                assert_eq!(image_reads[0].data_base64, "AQID");
            }
        }
    }

    #[test]
    fn read_search_fetch_and_other_map_to_tool_cards_with_raw_payloads() {
        for (kind, id) in [
            ("read", "r1"),
            ("search", "r2"),
            ("fetch", "r3"),
            ("other", "r4"),
            ("switch_mode", "r5"),
        ] {
            let mut state = state();
            let events = state.apply_update(update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": "Read file",
                "kind": kind,
                "status": "completed",
                "rawInput": { "path": "/repo/x.rs" },
                "content": [{ "type": "content", "content": { "type": "text", "text": "body" } }]
            })));
            match &events[0] {
                AgentEvent::ItemStarted(item) => match &item.content {
                    ItemContent::ToolCall {
                        name,
                        input,
                        output,
                        status,
                        ..
                    } => {
                        assert_eq!(name, "Read file");
                        assert_eq!(input["path"], "/repo/x.rs", "rawInput must ride along");
                        assert_eq!(output.as_deref(), Some("body"));
                        assert_eq!(*status, ItemStatus::Completed);
                    }
                    other => panic!("{kind}: expected ToolCall, got {other:?}"),
                },
                other => panic!("{kind}: expected ItemStarted, got {other:?}"),
            }
        }
    }

    #[test]
    fn think_tool_calls_become_reasoning() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t4",
            "title": "Thinking",
            "kind": "think",
            "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "step 1" } }]
        })));
        match &events[0] {
            AgentEvent::ItemStarted(item) => match &item.content {
                ItemContent::Reasoning { text } => assert_eq!(text, "step 1"),
                other => panic!("expected Reasoning, got {other:?}"),
            },
            other => panic!("expected ItemStarted, got {other:?}"),
        }
    }

    #[test]
    fn tool_updates_recover_missing_starts_and_preserve_status_and_input() {
        for (wire, expected) in [
            ("pending", ItemStatus::InProgress),
            ("in_progress", ItemStatus::InProgress),
            ("completed", ItemStatus::Completed),
            ("failed", ItemStatus::Failed),
        ] {
            let mut state = state();
            let events = state.apply_update(update(json!({
                "sessionUpdate":"tool_call_update", "toolCallId":"late", "title":"Late tool",
                "kind":"other", "status":"pending", "rawInput":{"path":"a.rs"}
            })));
            assert!(matches!(events.as_slice(), [AgentEvent::ItemStarted(_)]));
            let events = state.apply_update(update(json!({
                "sessionUpdate":"tool_call_update", "toolCallId":"late", "status":wire
            })));
            let item = match events.as_slice() {
                [AgentEvent::ItemUpdated(item)] if expected == ItemStatus::InProgress => item,
                [AgentEvent::ItemCompleted(item)] if expected != ItemStatus::InProgress => item,
                other => panic!("{wire}: incorrect lifecycle event {other:?}"),
            };
            assert!(
                matches!(&item.content, ItemContent::ToolCall { name, input, status, .. }
                if name == "Late tool" && input == &json!({"path":"a.rs"}) && *status == expected)
            );
        }
    }

    #[test]
    fn plan_update_preserves_turn_and_steps() {
        let mut state = state();
        state.turn = Some("turn-1".into());
        let events = state.apply_update(update(json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "Read the code", "priority": "high", "status": "completed" },
                { "content": "Write the fix", "priority": "medium", "status": "in_progress" }
            ]
        })));
        match &events[0] {
            AgentEvent::PlanUpdated {
                turn_id,
                steps,
                explanation,
            } => {
                assert_eq!(turn_id.as_deref(), Some("turn-1"));
                assert!(explanation.is_none());
                assert_eq!(steps.len(), 2);
                assert_eq!(steps[0].step, "Read the code");
                assert_eq!(steps[0].status, PlanStepStatus::Completed);
                assert_eq!(steps[1].status, PlanStepStatus::InProgress);
            }
            other => panic!("expected PlanUpdated, got {other:?}"),
        }
    }

    #[test]
    fn available_commands_become_provider_commands() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                { "name": "review", "description": "Review the diff", "input": null }
            ]
        })));
        match &events[0] {
            AgentEvent::ProviderCommands { commands } => {
                assert_eq!(commands[0].name, "review");
                assert_eq!(commands[0].kind, ProviderCommandKind::Command);
            }
            other => panic!("expected ProviderCommands, got {other:?}"),
        }
    }

    #[test]
    fn usage_update_is_the_context_window() {
        let mut state = state();
        let events = state.apply_update(update(json!({
            "sessionUpdate": "usage_update",
            "used": 1200,
            "size": 200000
        })));
        match &events[0] {
            AgentEvent::TokenUsage(usage) => {
                assert_eq!(usage.used_tokens, Some(1200));
                assert_eq!(usage.context_window, Some(200_000));
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn modes_and_categorized_config_become_provider_options() {
        let mut state = state();
        let modes: acp::SessionModeState = serde_json::from_value(json!({
            "currentModeId": "build",
            "availableModes": [
                { "id": "build", "name": "Build" },
                { "id": "plan", "name": "Plan", "description": "Read-only" }
            ]
        }))
        .unwrap();
        let config: Vec<acp::SessionConfigOption> = serde_json::from_value(json!([
            {
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "sonnet",
                "options": [{ "value": "sonnet", "name": "Sonnet" }]
            },
            {
                "id": "thought_level",
                "name": "Thinking",
                "category": "thought_level",
                "type": "select",
                "currentValue": "medium",
                "options": [
                    { "value": "low", "name": "Low" },
                    { "value": "medium", "name": "Medium" }
                ]
            },
            { "id": "web", "name": "Web search", "type": "boolean", "currentValue": true }
        ]))
        .unwrap();
        state.options.ingest(Some(&modes), Some(&config));
        assert_eq!(state.options.current_model(), Some("sonnet".to_string()));

        let descriptors: Vec<_> = state
            .options
            .records
            .iter()
            .map(|record| record.descriptor.clone())
            .collect();
        let ids: Vec<&str> = descriptors.iter().map(descriptor_id).collect();
        assert_eq!(
            ids,
            vec!["acp:mode", "acp:model", "reasoningEffort", "acp:cfg:web"]
        );
        assert_eq!(state.options.origin("acp:mode"), Some(OptionOrigin::Mode));
        assert_eq!(
            state.options.origin("acp:model"),
            Some(OptionOrigin::Config(acp::SessionConfigId::new("model")))
        );
        assert_eq!(
            state.options.origin("acp:cfg:web"),
            Some(OptionOrigin::Config(acp::SessionConfigId::new("web")))
        );
        assert!(matches!(
            &descriptors[3],
            OptionDescriptor::Boolean {
                default_value: true,
                ..
            }
        ));

        let selections: Vec<_> = state
            .options
            .records
            .iter()
            .map(|record| record.selection.clone())
            .collect();
        assert_eq!(selections[0].value, json!("build"));
        assert_eq!(selections[1].value, json!("sonnet"));
        assert_eq!(selections[2].value, json!("medium"));
        assert_eq!(selections[3].value, json!(true));

        // An agent-initiated mode switch re-publishes the options…
        let events = state.apply_update(update(json!({
            "sessionUpdate": "current_mode_update",
            "currentModeId": "plan"
        })));
        match &events[0] {
            AgentEvent::ProviderOptions { selections, .. } => {
                assert_eq!(selections[0].value, json!("plan"));
            }
            other => panic!("expected ProviderOptions, got {other:?}"),
        }

        // …and so does a config-option push.
        let events = state.apply_update(update(json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [
                { "id": "web", "name": "Web search", "type": "boolean", "currentValue": false }
            ]
        })));
        match &events[0] {
            AgentEvent::ProviderOptions { selections, .. } => {
                let web = selections.iter().find(|s| s.id == "acp:cfg:web").unwrap();
                assert_eq!(web.value, json!(false));
            }
            other => panic!("expected ProviderOptions, got {other:?}"),
        }
    }

    #[test]
    fn a_thought_level_selection_saved_under_its_config_id_still_restores() {
        let thought = Some(&acp::SessionConfigOptionCategory::ThoughtLevel);
        let saved = |id: &str| {
            vec![OptionSelection {
                id: id.into(),
                value: json!("low"),
            }]
        };
        assert_eq!(
            config_selection(&saved("reasoningEffort"), "reasoning_effort", thought),
            Some("low")
        );
        assert_eq!(
            config_selection(
                &saved("acp:cfg:reasoning_effort"),
                "reasoning_effort",
                thought
            ),
            Some("low")
        );
    }

    #[test]
    fn permission_request_carries_the_agents_own_options() {
        let request: acp::RequestPermissionRequest = serde_json::from_value(json!({
            "sessionId": "s1",
            "toolCall": {
                "toolCallId": "t9",
                "title": "rm -rf build",
                "kind": "execute",
                "rawInput": { "command": "rm -rf build" }
            },
            "options": [
                { "optionId": "yes", "name": "Allow", "kind": "allow_once" },
                { "optionId": "always", "name": "Always allow", "kind": "allow_always" },
                { "optionId": "no", "name": "Reject", "kind": "reject_once" }
            ]
        }))
        .unwrap();
        let approval = approval_request(
            "acp-approval-1".into(),
            Some("turn-1".into()),
            &request.tool_call,
            &request.options,
        );
        match &approval.kind {
            ApprovalKind::ExecCommand { command, .. } => assert_eq!(command, "rm -rf build"),
            other => panic!("expected ExecCommand, got {other:?}"),
        }
        assert_eq!(approval.options.len(), 3);
        assert_eq!(approval.options[0].label, "Allow");
        assert_eq!(approval.options[1].kind, ApprovalOptionKind::AllowAlways);
        let selected =
            |decision: ApprovalDecision| match approval_outcome(&decision, &approval.options) {
                Some(acp::RequestPermissionOutcome::Selected(outcome)) => {
                    outcome.option_id.0.to_string()
                }
                other => panic!("expected a selection, got {other:?}"),
            };
        assert_eq!(selected(ApprovalDecision::Option("yes".into())), "yes");
        assert_eq!(selected(ApprovalDecision::Option("no".into())), "no");
        assert_eq!(
            selected(ApprovalDecision::Option("always".into())),
            "always"
        );
        assert!(
            approval_outcome(&ApprovalDecision::Option("other".into()), &approval.options)
                .is_none()
        );
        assert!(matches!(
            approval_outcome(&ApprovalDecision::Cancel, &approval.options),
            Some(acp::RequestPermissionOutcome::Cancelled)
        ));
        let response = acp::RequestPermissionResponse::new(
            approval_outcome(&ApprovalDecision::Option("yes".into()), &approval.options).unwrap(),
        );
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["outcome"]["outcome"], "selected");
        assert_eq!(value["outcome"]["optionId"], "yes");
    }

    #[test]
    fn approvals_classify_by_tool_kind() {
        let build = |kind: &str| {
            let tool: acp::ToolCallUpdate = serde_json::from_value(json!({
                "toolCallId": "t",
                "title": "Read config",
                "kind": kind,
                "rawInput": { "path": "a.rs" }
            }))
            .unwrap();
            approval_request("a1".into(), None, &tool, &[]).kind
        };
        assert!(matches!(build("read"), ApprovalKind::FileRead { .. }));
        assert!(matches!(build("search"), ApprovalKind::FileRead { .. }));
        assert!(matches!(build("fetch"), ApprovalKind::FileRead { .. }));
        assert!(matches!(build("other"), ApprovalKind::ToolUse { .. }));

        let edit: acp::ToolCallUpdate = serde_json::from_value(json!({
            "toolCallId": "t",
            "title": "Edit",
            "kind": "edit",
            "content": [{ "type": "diff", "path": "a.rs", "oldText": "a\n", "newText": "b\n" }]
        }))
        .unwrap();
        match approval_request("a2".into(), None, &edit, &[]).kind {
            ApprovalKind::FileChange { changes, .. } => assert_eq!(changes[0].path, "a.rs"),
            other => panic!("expected FileChange, got {other:?}"),
        }
    }

    #[test]
    fn stop_reasons_map_to_turn_status() {
        assert_eq!(
            stop_reason_status(acp::StopReason::EndTurn).0,
            TurnStatus::Completed
        );
        assert_eq!(
            stop_reason_status(acp::StopReason::Cancelled).0,
            TurnStatus::Interrupted
        );
        let (status, message) = stop_reason_status(acp::StopReason::Refusal);
        assert_eq!(status, TurnStatus::Failed);
        assert!(message.unwrap().contains("refused"));
        let (status, message) = stop_reason_status(acp::StopReason::MaxTokens);
        assert_eq!(status, TurnStatus::Failed);
        assert!(message.unwrap().contains("token limit"));
    }

    #[test]
    fn paths_outside_the_session_cwd_are_rejected() {
        let state = state();
        assert!(state.resolve_path(Path::new("src/main.rs")).is_ok());
        assert!(
            state
                .resolve_path(Path::new("/tmp/tcode-acp-test/../etc/passwd"))
                .is_err()
        );
        assert!(state.resolve_path(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn prompt_carries_text_and_image_blocks() {
        let blocks = prompt_blocks(
            "hello",
            &[Attachment {
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
                source_path: None,
            }],
        );
        let value = serde_json::to_value(blocks.unwrap()).unwrap();
        assert_eq!(value[0]["type"], "text");
        assert_eq!(value[0]["text"], "hello");
        assert_eq!(value[1]["type"], "image");
        assert_eq!(value[1]["mimeType"], "image/png");
        assert_eq!(value[1]["data"], "AAAA");
    }
}
