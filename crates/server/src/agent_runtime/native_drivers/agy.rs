//! ACP owns configuration and prompt delivery; AGY's native subscription owns
//! execution. AGY 1.5.5 closes the ACP stream at a background yield, before the
//! native conversation is fully idle.
use super::*;
use futures_util::StreamExt;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

#[path = "agy_children.rs"]
mod children;

#[derive(Clone)]
struct NativeOutput {
    sender: Sender<AgentEvent>,
    inputs: std::sync::Arc<tokio::sync::Mutex<InputQueue>>,
}

#[derive(Default)]
struct InputQueue {
    active: Option<AgentEvent>,
    waiting: VecDeque<AgentEvent>,
}

fn question_pending(step: &Value) -> bool {
    step["requestedInteraction"]["askQuestion"].is_object()
        && !matches!(
            step["status"].as_str(),
            Some(
                "CORTEX_STEP_STATUS_DONE"
                    | "CORTEX_STEP_STATUS_ERROR"
                    | "CORTEX_STEP_STATUS_CANCELED"
                    | "CORTEX_STEP_STATUS_INTERRUPTED"
                    | "CORTEX_STEP_STATUS_HALTED"
                    | "CORTEX_STEP_STATUS_CLEARED"
            )
        )
}

fn input_owner(event: &AgentEvent) -> (Option<&str>, &AgentEvent) {
    let mut event = event;
    let mut owner = None;
    while let AgentEvent::Origin {
        agent_id,
        event: inner,
        ..
    } = event
    {
        owner = agent_id.as_deref().or(owner);
        event = inner;
    }
    (owner, event)
}

fn input_id(event: &AgentEvent) -> Option<&str> {
    match input_owner(event).1 {
        AgentEvent::UserInputRequested { request_id, .. } => Some(request_id),
        _ => None,
    }
}

fn withdrawn_input(mut request: AgentEvent) -> AgentEvent {
    let mut event = &mut request;
    while let AgentEvent::Origin { event: inner, .. } = event {
        event = inner;
    }
    if let AgentEvent::UserInputRequested { request_id, .. } = event {
        *event = AgentEvent::UserInputResolved {
            request_id: request_id.clone(),
            answers: serde_json::Map::new(),
        };
    }
    request
}

impl NativeOutput {
    async fn send(&self, event: AgentEvent) {
        let mut queue = self.inputs.lock().await;
        let (owner, inner) = input_owner(&event);
        match inner {
            AgentEvent::UserInputRequested { request_id, .. } => {
                if queue.active.as_ref().and_then(input_id) == Some(request_id.as_str()) {
                    queue.active = Some(event.clone());
                    let _ = self.sender.send(event).await;
                } else if let Some(pending) = queue
                    .waiting
                    .iter_mut()
                    .find(|event| input_id(event) == Some(request_id.as_str()))
                {
                    *pending = event;
                } else {
                    queue.waiting.push_back(event);
                }
            }
            AgentEvent::UserInputResolved { request_id, .. } => {
                queue
                    .waiting
                    .retain(|event| input_id(event) != Some(request_id.as_str()));
                if queue.active.as_ref().and_then(input_id) == Some(request_id.as_str()) {
                    queue.active = None;
                }
                let _ = self.sender.send(event).await;
            }
            AgentEvent::TurnCompleted { .. } | AgentEvent::SessionClosed { .. } => {
                let session_closed =
                    owner.is_none() && matches!(inner, AgentEvent::SessionClosed { .. });
                queue
                    .waiting
                    .retain(|event| !session_closed && input_owner(event).0 != owner);
                if queue
                    .active
                    .as_ref()
                    .is_some_and(|event| session_closed || input_owner(event).0 == owner)
                {
                    let request = queue.active.take().unwrap();
                    let _ = self.sender.send(withdrawn_input(request)).await;
                }
                let _ = self.sender.send(event).await;
            }
            _ => {
                let _ = self.sender.send(event).await;
            }
        }
        if queue.active.is_none()
            && let Some(request) = queue.waiting.pop_front()
        {
            let _ = self.sender.send(request.clone()).await;
            queue.active = Some(request);
        }
    }
}

const SERVICE: &str = "/exa.language_server_pb.LanguageServerService/";
const MAX_FRAME: usize = 64 * 1024 * 1024;

fn failure(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new("agy.native_protocol", message)
}

fn matches_steer_payload(message: &Value, request_id: &str) -> bool {
    if message["sender"] != "user" || request_id.is_empty() {
        return false;
    }
    let Some(encoded) = message["stepPayload"].as_str() else {
        return false;
    };
    let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
    else {
        return false;
    };
    if let Ok(len) = u8::try_from(request_id.len())
        && len < 128
    {
        let mut needle = Vec::with_capacity(3 + request_id.len());
        needle.extend_from_slice(&[0x9a, 0x01, len]);
        needle.extend_from_slice(request_id.as_bytes());
        if bytes.windows(needle.len()).any(|window| window == needle) {
            return true;
        }
    }
    bytes
        .windows(request_id.len())
        .any(|window| window == request_id.as_bytes())
}

fn apply_pending_messages_patch(
    pending_messages: &mut BTreeMap<usize, Value>,
    patch: &Value,
) -> Result<(), RuntimeError> {
    let total = patch["totalLength"].as_u64().unwrap_or(0) as usize;
    if let (Some(indices), Some(messages)) = (
        patch["indices"].as_array(),
        patch["pendingAgentMessages"].as_array(),
    ) {
        if indices.len() != messages.len() {
            return Err(failure(
                "Native pending message indices and payloads differ",
            ));
        }
        for (index, item) in indices.iter().zip(messages) {
            let index = index
                .as_u64()
                .ok_or_else(|| failure("Invalid pending message index"))?
                as usize;
            pending_messages.insert(index, item.clone());
        }
    }
    pending_messages.retain(|&index, _| index < total);
    Ok(())
}

#[derive(Clone)]
struct Native {
    client: reqwest::Client,
    base: String,
    session: String,
}

impl Native {
    async fn call(&self, method: &str, body: Value) -> Result<Value, RuntimeError> {
        let response = self
            .client
            .post(format!("{}{method}", self.base))
            .timeout(Duration::from_secs(30))
            .json(&body)
            .send()
            .await
            .map_err(|e| failure(format!("{method}: {e}")))?;
        let status = response.status();
        let body = response.text().await.map_err(|e| failure(e.to_string()))?;
        if !status.is_success() {
            return Err(failure(format!("{method}: HTTP {status}: {body}")));
        }
        serde_json::from_str(&body).map_err(|e| failure(format!("{method}: {e}")))
    }

    async fn subscribe(&self) -> Result<reqwest::Response, RuntimeError> {
        let json = serde_json::to_vec(&json!({"conversationId":self.session,"subscriberId":format!("muteki-{}",uuid::Uuid::new_v4()),"disableRehydration":false}))
            .map_err(|e| failure(e.to_string()))?;
        let mut body = vec![0];
        body.extend_from_slice(&(json.len() as u32).to_be_bytes());
        body.extend(json);
        let response = self
            .client
            .post(format!("{}StreamAgentStateUpdates", self.base))
            .header("Content-Type", "application/connect+json")
            .header("Connect-Protocol-Version", "1")
            .body(body)
            .send()
            .await
            .map_err(|e| failure(e.to_string()))?;
        if !response.status().is_success() {
            return Err(failure(format!(
                "Native subscription: HTTP {}",
                response.status()
            )));
        }
        Ok(response)
    }

    async fn interact(
        &self,
        trajectory: &str,
        index: usize,
        answer: Value,
    ) -> Result<(), RuntimeError> {
        let mut interaction = answer;
        interaction["trajectoryId"] = json!(trajectory);
        interaction["stepIndex"] = json!(index);
        self.call(
            "HandleCascadeUserInteraction",
            json!({"cascadeId":self.session,"interaction":interaction}),
        )
        .await?;
        Ok(())
    }

    async fn steer(
        &self,
        request_id: &str,
        text: &str,
        attachments: &[agent::Attachment],
    ) -> Result<(), RuntimeError> {
        let mut body = json!({
            "cascadeId": self.session,
            "clientMessageId": request_id,
            "deliveryStrategy": "MESSAGE_DELIVERY_STRATEGY_NEXT_INVOCATION",
        });
        let mut items = Vec::new();
        let mut media = Vec::new();
        if !text.is_empty() {
            items.push(json!({"text":text}));
        }
        for attachment in attachments {
            if let Some(path) = attachment.workspace_file() {
                let uri = url::Url::from_file_path(path)
                    .map_err(|_| failure("AGY file attachments require an absolute workspace path"))?;
                items.push(json!({"item":{"file":{"absoluteUri":uri.as_str()}}}));
            } else {
                media.push(json!({"mimeType":attachment.media_type,"inlineData":attachment.data_base64}));
            }
        }
        if !items.is_empty() { body["items"] = json!(items); }
        if !media.is_empty() { body["media"] = json!(media); }
        let response = self
            .client
            .post(format!("{}SendUserCascadeMessage", self.base))
            .timeout(Duration::from_secs(30))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    failure(format!("SendUserCascadeMessage: {e}"))
                } else {
                    RuntimeError::new(
                        "runtime.delivery_unknown",
                        format!("SendUserCascadeMessage: {e}"),
                    )
                }
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|e| {
            RuntimeError::new(
                "runtime.delivery_unknown",
                format!("SendUserCascadeMessage: {e}"),
            )
        })?;
        if !status.is_success() {
            return Err(failure(format!(
                "SendUserCascadeMessage: HTTP {status}: {text}"
            )));
        }
        serde_json::from_str::<Value>(&text).map_err(|e| {
            RuntimeError::new(
                "runtime.delivery_unknown",
                format!("SendUserCascadeMessage: {e}"),
            )
        })?;
        Ok(())
    }
}

async fn read_stream(
    response: reqwest::Response,
    tx: tokio::sync::mpsc::Sender<Result<Value, RuntimeError>>,
) {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let result = async {
        while let Some(chunk) = stream.next().await {
            buffer.extend(chunk.map_err(|e| failure(e.to_string()))?);
            while buffer.len() >= 5 {
                let size = u32::from_be_bytes(buffer[1..5].try_into().unwrap()) as usize;
                if size > MAX_FRAME {
                    return Err(failure("Native state frame exceeds 64 MiB"));
                }
                if buffer.len() < size + 5 {
                    break;
                }
                let flags = buffer[0];
                let value: Value = serde_json::from_slice(&buffer[5..size + 5])
                    .map_err(|e| failure(e.to_string()))?;
                buffer.drain(..size + 5);
                if flags != 0 {
                    return Err(failure(format!("Native subscription ended: {value}")));
                }
                if tx.send(Ok(value)).await.is_err() {
                    return Ok(());
                }
            }
        }
        Err(failure("Native state subscription closed"))
    }
    .await;
    if let Err(error) = result {
        let _ = tx.send(Err(error)).await;
    }
}

async fn wait_closed(inner: &SessionHandle) {
    while let Ok(event) = inner.events.recv().await {
        if matches!(event, AgentEvent::SessionClosed { .. }) {
            break;
        }
    }
}

async fn shutdown(inner: &SessionHandle, process_group: Option<u32>) {
    let _ = inner.commands.send(SessionCommand::Shutdown).await;
    if tokio::time::timeout(Duration::from_secs(4), wait_closed(inner))
        .await
        .is_err()
    {
        tracing::warn!(
            ?process_group,
            "AGY did not acknowledge shutdown; terminating its owned process group"
        );
        kill_process_group(process_group);
        if tokio::time::timeout(Duration::from_secs(1), wait_closed(inner))
            .await
            .is_err()
        {
            #[cfg(unix)]
            if let Some(pid) = process_group {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }
}

fn parse_http_port(log: &str) -> Option<u16> {
    log.lines().find_map(|line| {
        line.split_once("Language server listening on random port at ")
            .and_then(|(_, value)| value.strip_suffix(" for HTTP"))
            .and_then(|value| value.parse::<u16>().ok())
    })
}

fn worker_pid_for_session(front_log: &str, session: &str) -> Option<u32> {
    let marker = format!("front: session {session} served by worker ");
    let worker = front_log
        .lines()
        .rev()
        .find_map(|line| line.split_once(&marker).map(|(_, rest)| rest.trim()))?;
    let started = format!("front: started worker {worker} (pid ");
    front_log.lines().rev().find_map(|line| {
        line.split_once(&started)
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .and_then(|pid| pid.trim().parse::<u32>().ok())
    })
}

async fn discover_http_port(log_path: &std::path::Path, session: &str) -> Result<u16, RuntimeError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(front) = tokio::fs::read_to_string(log_path).await {
            if let Some(port) = parse_http_port(&front) {
                return Ok(port);
            }
            if let Some(worker_pid) = worker_pid_for_session(&front, session) {
                let worker_log =
                    std::path::PathBuf::from(format!("{}.{worker_pid}", log_path.display()));
                if let Ok(worker) = tokio::fs::read_to_string(&worker_log).await
                    && let Some(port) = parse_http_port(&worker)
                {
                    return Ok(port);
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(failure("AGY did not advertise its native HTTP endpoint"));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub async fn start_agy_acp(
    mut options: agent::SessionOptions,
) -> Result<SessionHandle, RuntimeError> {
    let csrf = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    options
        .launch_env
        .env
        .retain(|(key, _)| key != "ANTIGRAVITY_SERVER_CSRF_TOKEN");
    options
        .launch_env
        .env
        .push(("ANTIGRAVITY_SERVER_CSRF_TOKEN".into(), csrf.clone()));
    let dir = tempfile::Builder::new()
        .prefix("muteki-agy-")
        .tempdir()
        .map_err(|e| failure(e.to_string()))?;
    let log = dir.path().join("native.log");
    let Some(agent::AcpAgent {
        launch: agent::AcpLaunch::Custom { args, .. },
        ..
    }) = options.acp.as_mut()
    else {
        return Err(failure("AGY requires an owned executable launch"));
    };
    if args.iter().chain(options.extra_args.iter()).any(|arg| {
        arg == "--log-file"
            || arg == "--cli_log_file"
            || arg.starts_with("--log-file=")
            || arg.starts_with("--cli_log_file=")
    }) {
        return Err(failure(
            "AGY native subscription owns the session log destination",
        ));
    }
    if !args
        .iter()
        .chain(options.extra_args.iter())
        .any(|arg| arg == "--process-per-session")
    {
        args.push("--process-per-session".into());
    }
    args.extend(["--log-file".into(), log.to_string_lossy().into_owned()]);
    let inner = agent::start_session(ProviderKind::Acp, options)
        .await
        .map_err(|e| failure(e.to_string()))?;
    let mut process_group = None;
    let setup = async {
        let mut startup = Vec::new();
        let session = loop {
            let event = inner
                .events
                .recv()
                .await
                .map_err(|_| failure("ACP closed during startup"))?;
            if let AgentEvent::ProcessStarted { pid } = &event {
                process_group = Some(*pid);
            }
            let id = match &event {
                AgentEvent::SessionStarted {
                    provider_session_id,
                    ..
                } => Some(provider_session_id.clone()),
                _ => None,
            };
            if let AgentEvent::SessionClosed { reason } = &event {
                return Err(failure(format!("ACP startup: {reason:?}")));
            }
            startup.push(event);
            if let Some(id) = id {
                break id;
            }
        };
        let port = discover_http_port(&log, &session).await?;
        let leader =
            process_group.ok_or_else(|| failure("AGY process ownership was not reported"))?;
        let owned = tokio::task::spawn_blocking(move || {
            crate::runtime_guardian::owns_tcp_listener(leader, port)
        })
        .await
        .map_err(|e| failure(e.to_string()))?
        .map_err(|e| failure(e.to_string()))?;
        if !owned {
            return Err(failure(
                "AGY native endpoint is not owned by its supervised process group",
            ));
        }
        let mut headers = reqwest::header::HeaderMap::new();
        let mut secret =
            reqwest::header::HeaderValue::from_str(&csrf).map_err(|e| failure(e.to_string()))?;
        secret.set_sensitive(true);
        headers.insert("x-codeium-csrf-token", secret);
        let native = Native {
            client: crate::network_proxy::direct_client_builder()
                .default_headers(headers)
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| failure(e.to_string()))?,
            base: format!("http://127.0.0.1:{port}{SERVICE}"),
            session,
        };
        let response = native.subscribe().await?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let reader = tokio::spawn(read_stream(response, tx));
        let initial = rx
            .recv()
            .await
            .ok_or_else(|| failure("No native state snapshot"))??;
        Ok((startup, native, initial, rx, reader))
    };
    let (startup, native, initial, mut rx, reader) =
        match tokio::time::timeout(Duration::from_secs(30), setup).await {
            Ok(Ok(value)) => value,
            result => {
                shutdown(&inner, process_group).await;
                return Err(match result {
                    Ok(Err(error)) => error,
                    _ => failure("Native subscription startup timed out"),
                });
            }
        };
    let (commands, input) = smol::channel::unbounded();
    let (output, events) = smol::channel::unbounded();
    tokio::spawn(async move {
        let _dir = dir;
        let commands_lifetime = input.clone();
        let mut actor = Actor::new(native, inner, output);
        for event in startup {
            actor.emit(event).await;
        }
        let result = actor.run(input, &mut rx, initial).await;
        actor.children.cancel(None).await;
        actor.children.disconnected().await;
        let needs_cancel = actor.turn.is_some();
        if let Err(error) = result {
            actor
                .emit(AgentEvent::TurnFailed {
                    code: error.code.into(),
                    message: error.message.clone(),
                })
                .await;
            if let Some(turn) = actor.turn.take() {
                actor
                    .emit(AgentEvent::TurnCompleted {
                        turn_id: turn.id,
                        status: TurnStatus::Failed,
                        usage: None,
                    })
                    .await;
            }
            actor
                .emit(AgentEvent::SessionClosed {
                    reason: Some(error.message),
                })
                .await;
        }
        if needs_cancel {
            let cancelled = tokio::time::timeout(
                Duration::from_secs(2),
                actor.native.call(
                    "CancelCascadeInvocation",
                    json!({"cascadeId":actor.native.session,"killBackgroundTasks":true}),
                ),
            )
            .await;
            let idle = tokio::time::timeout(Duration::from_secs(2), async {
                while let Some(update) = rx.recv().await {
                    let update = update?;
                    if let Some(patch) = update["update"].get("pendingAgentMessagesUpdate") {
                        let _ = apply_pending_messages_patch(&mut actor.pending_messages, patch);
                    }
                    if update["update"]["fullyIdle"] == true {
                        return Ok::<(), RuntimeError>(());
                    }
                }
                Err(failure(
                    "Native subscription closed before cancellation settled",
                ))
            })
            .await;
            let pending_ids: Vec<String> = actor
                .pending_messages
                .values()
                .filter(|message| message["sender"] == "user")
                .filter_map(|message| message["id"].as_str().map(str::to_owned))
                .collect();
            for id in pending_ids {
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    actor.native.call(
                        "DeleteAgentMessage",
                        json!({"messageId": id, "recipient": actor.native.session}),
                    ),
                )
                .await;
            }
            if !matches!(cancelled, Ok(Ok(_))) || !matches!(idle, Ok(Ok(_))) {
                tracing::warn!(session=%actor.native.session, "AGY native background cancellation was not confirmed during shutdown");
            }
        }
        reader.abort();
        shutdown(&actor.inner, process_group).await;
        drop(commands_lifetime);
    });
    Ok(SessionHandle {
        provider: ProviderKind::Acp,
        commands,
        events,
    })
}

#[derive(Default)]
struct CheckpointState {
    started_emitted: bool,
    terminal_status: Option<&'static str>,
    summary: Option<String>,
    duration_ms: Option<u64>,
    completed_emitted: bool,
}

fn u64_field(value: &Value, key: &str) -> Option<u64> {
    value[key]
        .as_u64()
        .or_else(|| value[key].as_str().and_then(|s| s.parse::<u64>().ok()))
}

struct Turn {
    id: String,
    end: Option<(TurnStatus, Option<TokenUsage>)>,
    baseline: usize,
    items: HashMap<String, (ItemContent, bool)>,
    requested: HashSet<usize>,
    saw_busy: bool,
    usage_baseline: usize,
    steers: BTreeSet<String>,
    uncertain_steers: HashSet<String>,
    is_manual_compact: bool,
    pre_compact_tokens: Option<u64>,
    compacted_tokens: Option<u64>,
    checkpoints: BTreeMap<usize, CheckpointState>,
}
struct Actor {
    native: Native,
    inner: SessionHandle,
    output: NativeOutput,
    steps: BTreeMap<usize, Value>,
    length: usize,
    trajectory: String,
    fully_idle: bool,
    turn: Option<Turn>,
    pending_baseline: usize,
    pending_busy: bool,
    approvals: HashMap<String, usize>,
    usage: BTreeMap<usize, Value>,
    usage_length: usize,
    generator_errors: BTreeMap<usize, Value>,
    pending_usage_baseline: usize,
    inputs: HashMap<String, (usize, Vec<Value>)>,
    pending_messages: BTreeMap<usize, Value>,
    context_used: Option<u64>,
    context_window: Option<u64>,
    context_zones: Vec<Value>,
    pending_manual_compact: bool,
    children: children::Children,
}
impl Actor {
    fn new(native: Native, inner: SessionHandle, output: Sender<AgentEvent>) -> Self {
        let output = NativeOutput {
            sender: output,
            inputs: std::sync::Arc::new(tokio::sync::Mutex::new(InputQueue::default())),
        };
        Self {
            native,
            inner,
            children: children::Children::new(output.clone()),
            output,
            steps: BTreeMap::new(),
            length: 0,
            trajectory: String::new(),
            fully_idle: false,
            turn: None,
            pending_baseline: 0,
            pending_busy: false,
            approvals: HashMap::new(),
            usage: BTreeMap::new(),
            usage_length: 0,
            generator_errors: BTreeMap::new(),
            pending_usage_baseline: 0,
            inputs: HashMap::new(),
            pending_messages: BTreeMap::new(),
            context_used: None,
            context_window: None,
            context_zones: Vec::new(),
            pending_manual_compact: false,
        }
    }
    async fn emit(&self, event: AgentEvent) {
        self.output.send(event).await;
    }
    async fn origin(&self, event: AgentEvent) {
        if let Some(turn) = &self.turn {
            self.emit(AgentEvent::Origin {
                turn_id: turn.id.clone(),
                agent_id: None,
                event: Box::new(event),
            })
            .await;
        }
    }
    async fn run(
        &mut self,
        commands: Receiver<SessionCommand>,
        updates: &mut tokio::sync::mpsc::Receiver<Result<Value, RuntimeError>>,
        initial: Value,
    ) -> Result<(), RuntimeError> {
        self.update(initial).await?;
        loop {
            tokio::select! {
                Some(update) = self.children.rx.recv() => self.children.update(update).await,
                update = updates.recv() => self.update(update.ok_or_else(||failure("Native subscription disappeared"))??).await?,
                event = self.inner.events.recv() => {
                    let event = event.map_err(|_|failure("ACP event stream closed"))?;
                    self.acp(event).await?;
                },
                command = commands.recv() => {
                    let Ok(command) = command else { break; };
                    if matches!(command,SessionCommand::Shutdown) { break; }
                    self.command(command).await?;
                }
            }
        }
        Ok(())
    }
    async fn command(&mut self, command: SessionCommand) -> Result<(), RuntimeError> {
        if let SessionCommand::RespondApproval { request_id, decision } = &command
            && self.children.approval(request_id, decision).await?
        { return Ok(()); }
        if let SessionCommand::RespondUserInput { request_id, answers } = &command
            && self.children.answer(request_id, answers).await?
        { return Ok(()); }
        match command {
            SessionCommand::RespondUserInput {
                ref request_id,
                ref answers,
            } if self.inputs.contains_key(request_id) => {
                let (index, questions) = self.inputs.get(request_id).unwrap();
                let responses: Vec<Value> = if answers.is_empty() {
                    Vec::new()
                } else {
                    questions.iter().enumerate().map(|(i, question)| {
                    let answer=answers.get(&i.to_string()).unwrap_or(&Value::Null);
                    let texts: Vec<&str>=answer.as_array().into_iter().flatten().filter_map(Value::as_str)
                        .chain(answer.as_str()).collect();
                    let options: Vec<&Value>=question["options"].as_array().into_iter().flatten().collect();
                    let selected: Vec<Value>=options.iter().filter(|v|texts.iter().any(|t|v["text"]==*t||v["id"]==*t)).map(|v|v["id"].clone()).collect();
                    let written: Vec<&str>=texts.into_iter().filter(|t|!options.iter().any(|v|v["text"]==*t||v["id"]==*t)).collect();
                    json!({"question":question["question"],"selectedOptionIds":selected,"writeInResponse":written.join("\n"),"skipped":answer.is_null()})
                }).collect()
                };
                self.native.interact(&self.trajectory,*index,json!({"askQuestion":{"responses":responses,"cancelled":answers.is_empty()}})).await?;
                self.inputs.remove(request_id);
                self.origin(AgentEvent::UserInputResolved {
                    request_id: request_id.clone(),
                    answers: answers.clone(),
                })
                .await;
            }
            SessionCommand::RespondApproval {
                ref request_id,
                ref decision,
            } if self.approvals.contains_key(request_id) => {
                let index = self.approvals[request_id];
                let allow = matches!(decision, ApprovalDecision::Option(id) if id=="allow");
                self.native
                    .interact(
                        &self.trajectory,
                        index,
                        json!({"permission":{"allow":allow,"persistGrants":{}}}),
                    )
                    .await?;
                self.approvals.remove(request_id);
                self.origin(AgentEvent::ApprovalResolved {
                    request_id: request_id.clone(),
                    decision: decision.clone(),
                })
                .await;
            }
            SessionCommand::Steer {
                request_id,
                text,
                attachments,
            } => {
                let can_steer = self.turn.as_ref().is_some_and(|turn| {
                    !matches!(
                        turn.end,
                        Some((TurnStatus::Interrupted | TurnStatus::Failed, _))
                    ) && !(self.fully_idle && turn.end.is_some())
                        && (turn.saw_busy || self.length > turn.baseline)
                });
                if !can_steer {
                    self.emit(AgentEvent::SteerRejected { request_id }).await;
                    return Ok(());
                }
                match self.native.steer(&request_id, &text, &attachments).await {
                    Ok(()) => {
                        self.fully_idle = false;
                        self.pending_busy = true;
                        if let Some(turn) = &mut self.turn {
                            turn.saw_busy = true;
                            turn.steers.insert(request_id.clone());
                        }
                        self.emit(AgentEvent::ControlDelivery {
                            request_id,
                            operation: "steer".into(),
                            status: "accepted".into(),
                            error: None,
                        })
                        .await;
                    }
                    Err(error) if error.code == "runtime.delivery_unknown" => {
                        if let Some(turn) = &mut self.turn {
                            turn.steers.insert(request_id.clone());
                            turn.uncertain_steers.insert(request_id.clone());
                        }
                        let detail = Some(json!({"code": error.code, "message": error.message}));
                        self.emit(AgentEvent::ControlDelivery {
                            request_id: request_id.clone(),
                            operation: "steer".into(),
                            status: "consumption_unknown".into(),
                            error: detail.clone(),
                        })
                        .await;
                        self.emit(AgentEvent::ControlDelivery {
                            request_id,
                            operation: "steer".into(),
                            status: "unknown".into(),
                            error: detail,
                        })
                        .await;
                    }
                    Err(error) => {
                        self.emit(AgentEvent::ControlDelivery {
                            request_id,
                            operation: "steer".into(),
                            status: "failed".into(),
                            error: Some(json!({"code": error.code, "message": error.message})),
                        })
                        .await;
                    }
                }
            }
            SessionCommand::StopBackgroundTask { task_id } => {
                self.children.cancel(task_id.as_deref()).await;
            }
            SessionCommand::Interrupt => {
                self.children.cancel(None).await;
                self.cancel_unconsumed_steers(false).await;
                self.native
                    .call(
                        "CancelCascadeInvocation",
                        json!({"cascadeId":self.native.session,"killBackgroundTasks":true}),
                    )
                    .await?;
                self.inner
                    .commands
                    .send(SessionCommand::Interrupt)
                    .await
                    .map_err(|_| failure("ACP control channel closed"))?;
                if let Some(turn) = &mut self.turn {
                    turn.end = Some((TurnStatus::Interrupted, None));
                }
            }
            command => {
                if let SessionCommand::SendTurn { ref text, .. } = command {
                    self.pending_baseline = self.length;
                    self.pending_busy = false;
                    self.pending_usage_baseline = self.usage_length;
                    self.pending_manual_compact = text.trim() == "/compact";
                    self.steps.clear();
                    self.usage.clear();
                    self.generator_errors.clear();
                }
                self.inner
                    .commands
                    .send(command)
                    .await
                    .map_err(|_| failure("ACP control channel closed"))?;
            }
        }
        self.settle().await
    }
    async fn acp(&mut self, mut event: AgentEvent) -> Result<(), RuntimeError> {
        while let AgentEvent::Origin { event: inner, .. } = event {
            event = *inner;
        }
        match event {
            AgentEvent::TurnStarted { turn_id } => {
                self.turn = Some(Turn {
                    id: turn_id.clone(),
                    end: None,
                    baseline: self.pending_baseline,
                    items: HashMap::new(),
                    requested: HashSet::new(),
                    saw_busy: self.pending_busy,
                    usage_baseline: self.pending_usage_baseline,
                    steers: BTreeSet::new(),
                    uncertain_steers: HashSet::new(),
                    is_manual_compact: std::mem::take(&mut self.pending_manual_compact),
                    pre_compact_tokens: self.context_used,
                    compacted_tokens: None,
                    checkpoints: BTreeMap::new(),
                });
                self.emit(AgentEvent::TurnStarted { turn_id }).await;
                if self.context_used.is_some() || self.context_window.is_some() {
                    if !self.context_zones.is_empty() {
                        self.origin(AgentEvent::UsageDetail {
                            scope: "context_window".into(),
                            usage_id: String::new(),
                            usage: json!({ "zones": self.context_zones }),
                        })
                        .await;
                    }
                    self.origin(AgentEvent::TokenUsage(TokenUsage {
                        used_tokens: self.context_used,
                        context_window: self.context_window,
                        ..TokenUsage::default()
                    }))
                    .await;
                }
                self.publish().await?;
            }
            AgentEvent::TurnCompleted {
                turn_id,
                status,
                usage,
            } => {
                if self.turn.is_none()
                    || (status != TurnStatus::Completed
                        && self
                            .turn
                            .as_ref()
                            .is_some_and(|t| !t.saw_busy && self.length <= t.baseline))
                {
                    self.cancel_unconsumed_steers(true).await;
                    self.turn = None;
                    self.emit(AgentEvent::TurnCompleted {
                        turn_id,
                        status,
                        usage,
                    })
                    .await;
                    return Ok(());
                }
                if let Some(turn) = &mut self.turn {
                    if !matches!(turn.end, Some((TurnStatus::Interrupted, _))) {
                        turn.end = Some((status, usage));
                    }
                }
                self.publish().await?;
                self.settle().await?;
            }
            AgentEvent::Delta { .. }
            | AgentEvent::ItemStarted(_)
            | AgentEvent::ItemUpdated(_)
            | AgentEvent::ItemCompleted(_)
            | AgentEvent::TokenUsage(_) => {}
            AgentEvent::SessionClosed { reason } => {
                return Err(failure(format!("ACP closed: {reason:?}")));
            }
            event => self.emit(event).await,
        }
        Ok(())
    }
    async fn update(&mut self, message: Value) -> Result<(), RuntimeError> {
        let update = &message["update"];
        if update["conversationId"] != self.native.session {
            return Err(failure("Native update belongs to a different conversation"));
        }
        self.trajectory = update["trajectoryId"]
            .as_str()
            .ok_or_else(|| failure("Missing native trajectory identity"))?
            .into();
        self.fully_idle = update["fullyIdle"].as_bool().unwrap_or(false);
        self.pending_busy |= !self.fully_idle;
        if let Some(turn) = &mut self.turn {
            if !self.fully_idle {
                turn.saw_busy = true;
            }
        }
        if let Some(patch) = update.get("pendingAgentMessagesUpdate") {
            apply_pending_messages_patch(&mut self.pending_messages, patch)?;
        }
        let patch = &update["mainTrajectoryUpdate"]["stepsUpdate"];
        if let Some(length) = patch["totalLength"].as_u64() {
            self.length = length as usize;
        }
        if let (Some(indices), Some(steps)) =
            (patch["indices"].as_array(), patch["steps"].as_array())
        {
            if indices.len() != steps.len() {
                return Err(failure("Native step indices and payloads differ"));
            }
            for (index, step) in indices.iter().zip(steps) {
                let index = index
                    .as_u64()
                    .ok_or_else(|| failure("Invalid native step index"))?
                    as usize;
                self.length = self.length.max(index + 1);
                self.steps.insert(index, step.clone());
            }
        }
        let patch = &update["mainTrajectoryUpdate"]["generatorMetadatasUpdate"];
        if let Some(length) = patch["totalLength"].as_u64() {
            self.usage_length = length as usize;
        }
        if let (Some(indices), Some(values)) = (
            patch["indices"].as_array(),
            patch["generatorMetadatas"].as_array(),
        ) {
            if indices.len() != values.len() {
                return Err(failure("Native usage indices and payloads differ"));
            }
            for (index, value) in indices.iter().zip(values) {
                let index = index
                    .as_u64()
                    .ok_or_else(|| failure("Invalid usage index"))?
                    as usize;
                self.usage_length = self.usage_length.max(index + 1);
                self.usage
                    .insert(index, value["chatModel"]["usage"].clone());
            }
        }
        if let Some(context) = update.get("contextWindowUsage") {
            let prev_used = self.context_used;
            let window = u64_field(context, "maxContextTokens");
            let used = u64_field(context, "estimatedTokensUsed")
                .or_else(|| window.filter(|_| self.context_used.is_none()).map(|_| 0));
            if used.is_some() {
                self.context_used = used;
            }
            if window.is_some() {
                self.context_window = window;
            }
            if let Some(groups) = context["tokenGroups"].as_array() {
                self.context_zones = groups
                    .iter()
                    .filter_map(|group| {
                        let label = group["name"].as_str()?.trim();
                        if label.is_empty() {
                            return None;
                        }
                        let tokens = u64_field(group, "numTokens").unwrap_or(0);
                        Some(json!({ "label": label, "tokens": tokens }))
                    })
                    .collect();
            }
            if let Some(turn) = &mut self.turn {
                if context["estimatedAfterCompaction"] == true {
                    if turn.pre_compact_tokens.is_none() {
                        turn.pre_compact_tokens = prev_used;
                    }
                    turn.compacted_tokens = used.or(self.context_used);
                } else if turn.checkpoints.values().all(|cp| cp.completed_emitted) {
                    if used.is_some() {
                        turn.pre_compact_tokens = used;
                    }
                    turn.compacted_tokens = None;
                }
            }
            if !self.context_zones.is_empty() {
                self.origin(AgentEvent::UsageDetail {
                    scope: "context_window".into(),
                    usage_id: String::new(),
                    usage: json!({ "zones": self.context_zones }),
                })
                .await;
            }
            self.origin(AgentEvent::TokenUsage(TokenUsage {
                used_tokens: self.context_used,
                context_window: self.context_window,
                ..TokenUsage::default()
            }))
            .await;
        }
        self.publish().await?;
        self.settle().await
    }
    async fn publish(&mut self) -> Result<(), RuntimeError> {
        if let Some(turn) = &self.turn {
            self.children.discover(&self.native, &self.steps, &turn.id, None).await?;
        }

        let withdrawn: Vec<String> = self
            .inputs
            .iter()
            .filter(|(_, (index, _))| !self.steps.get(index).is_some_and(question_pending))
            .map(|(id, _)| id.clone())
            .collect();
        for request_id in withdrawn {
            self.inputs.remove(&request_id);
            self.origin(AgentEvent::UserInputResolved {
                request_id,
                answers: serde_json::Map::new(),
            })
            .await;
        }
        let Some(turn) = &mut self.turn else {
            return Ok(());
        };
        let mut events = Vec::new();
        for (&index, step) in self.steps.range(turn.baseline..) {
            if step["type"] == "CORTEX_STEP_TYPE_USER_INPUT" {
                if let Some(cid) = step["userInput"]["clientMessageId"].as_str()
                    && turn.steers.remove(cid)
                {
                    turn.uncertain_steers.remove(cid);
                    events.push(AgentEvent::SteerAccepted {
                        request_id: cid.to_owned(),
                    });
                }
                if step["userInput"]["items"].as_array().is_some_and(|items| {
                    items.len() == 1
                        && items[0]["text"]
                            .as_str()
                            .is_some_and(|text| text.trim() == "/compact")
                }) {
                    turn.is_manual_compact = true;
                }
            }
            if step["type"] == "CORTEX_STEP_TYPE_CHECKPOINT"
                && step["checkpoint"]["intentOnly"] != true
                && step["checkpoint"]["intent_only"] != true
            {
                let trigger = if turn.is_manual_compact { "manual" } else { "auto" };
                let cp = turn.checkpoints.entry(index).or_default();
                if !cp.started_emitted {
                    cp.started_emitted = true;
                    events.push(AgentEvent::ContextCompacted(Compaction {
                        in_progress: true,
                        trigger: Some(trigger.into()),
                        pre_tokens: turn.pre_compact_tokens,
                        ..Compaction::default()
                    }));
                }
                if let Some(summary) = step["checkpoint"]["sessionSummary"]
                    .as_str()
                    .or_else(|| step["checkpoint"]["session_summary"].as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    cp.summary = Some(summary.to_owned());
                }
                if cp.duration_ms.is_none()
                    && let (Some(start), Some(end)) = (
                        step["metadata"]["startedAt"]
                            .as_str()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()),
                        step["metadata"]["completedAt"]
                            .as_str()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()),
                    )
                    && let Ok(ms) = u64::try_from((end - start).num_milliseconds())
                {
                    cp.duration_ms = Some(ms);
                }
                cp.terminal_status = match step["status"].as_str() {
                    Some("CORTEX_STEP_STATUS_DONE") => Some("completed"),
                    Some("CORTEX_STEP_STATUS_CANCELED" | "CORTEX_STEP_STATUS_INTERRUPTED") => {
                        Some("interrupted")
                    }
                    Some("CORTEX_STEP_STATUS_ERROR" | "CORTEX_STEP_STATUS_HALTED") => {
                        Some("failed")
                    }
                    _ => None,
                };
                if !cp.completed_emitted
                    && let Some(st) = cp.terminal_status
                    && (st != "completed" || turn.compacted_tokens.is_some())
                {
                    cp.completed_emitted = true;
                    let post_tokens = (st == "completed").then_some(turn.compacted_tokens).flatten();
                    let dropped_tokens = match (turn.pre_compact_tokens, post_tokens) {
                        (Some(pre), Some(post)) if pre >= post => Some(pre - post),
                        _ => None,
                    };
                    events.push(AgentEvent::ContextCompacted(Compaction {
                        in_progress: false,
                        status: Some(st.into()),
                        trigger: Some(trigger.into()),
                        pre_tokens: turn.pre_compact_tokens,
                        post_tokens,
                        dropped_tokens,
                        duration_ms: cp.duration_ms,
                        summary: cp.summary.clone(),
                        message: None,
                    }));
                }
            }
            for (id, content, done) in items(index, step)? {
                if turn
                    .items
                    .get(&id)
                    .is_some_and(|previous| previous == &(content.clone(), done))
                {
                    continue;
                }
                let existed = turn
                    .items
                    .insert(id.clone(), (content.clone(), done))
                    .is_some();
                let item = ThreadItem {
                    id,
                    parent_item_id: None,
                    content,
                };
                events.push(if done {
                    AgentEvent::ItemCompleted(item)
                } else if existed {
                    AgentEvent::ItemUpdated(item)
                } else {
                    AgentEvent::ItemStarted(item)
                });
            }
            if turn.end.is_some()
                && step["requestedInteraction"]["permission"].is_object()
                && !turn.requested.contains(&index)
            {
                let request = children::permission_request(step, &self.trajectory, index, &turn.id, None)?;
                turn.requested.insert(index);
                self.approvals.insert(request.id.clone(), index);
                events.push(AgentEvent::ApprovalRequested(request));
            }
            if turn.end.is_some() && question_pending(step) && !turn.requested.contains(&index) {
                let raw = step["requestedInteraction"]["askQuestion"]["questions"]
                    .as_array()
                    .ok_or_else(|| failure("Missing native questions"))?
                    .clone();
                let questions = raw
                    .iter()
                    .enumerate()
                    .map(|(i, q)| UserInputQuestion {
                        id: i.to_string(),
                        header: String::new(),
                        question: q["question"].as_str().unwrap_or("").into(),
                        options: q["options"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|o| agent::UserInputOption {
                                label: o["text"].as_str().unwrap_or("").into(),
                                description: String::new(),
                            })
                            .collect(),
                        multi_select: q["isMultiSelect"].as_bool().unwrap_or(false),
                        prefill: None,
                        contract: None,
                    })
                    .collect();
                let request_id = format!("agy-question-{}-{index}", self.trajectory);
                self.inputs.insert(request_id.clone(), (index, raw));
                turn.requested.insert(index);
                events.push(AgentEvent::UserInputRequested {
                    request_id,
                    questions,
                    delivery: UserInputDelivery::Blocking,
                });
            }
            if turn.end.is_some()
                && step["requestedInteraction"]
                    .as_object()
                    .is_some_and(|v| !v.is_empty())
                && !turn.requested.contains(&index)
            {
                return Err(failure(format!(
                    "Unsupported native interaction at step {index}: {}",
                    step["requestedInteraction"]
                )));
            }
        }
        for event in events {
            self.origin(event).await;
        }
        Ok(())
    }
    async fn cancel_unconsumed_steers(&mut self, final_settle: bool) {
        let Some(ids) = self
            .turn
            .as_ref()
            .map(|turn| turn.steers.iter().cloned().collect::<Vec<_>>())
        else {
            return;
        };
        let mut emitted = Vec::new();
        for request_id in ids {
            let matched: Vec<(usize, String)> = self
                .pending_messages
                .iter()
                .filter(|(_, message)| matches_steer_payload(message, &request_id))
                .filter_map(|(&index, message)| {
                    message["id"].as_str().map(|id| (index, id.to_owned()))
                })
                .collect();
            if !matched.is_empty() {
                let mut error = None;
                for (index, id) in matched {
                    match self
                        .native
                        .call(
                            "DeleteAgentMessage",
                            json!({"messageId": id, "recipient": self.native.session}),
                        )
                        .await
                    {
                        Ok(_) => {
                            self.pending_messages.remove(&index);
                        }
                        Err(err) => error = Some(err),
                    }
                }
                if let Some(turn) = &mut self.turn {
                    turn.steers.remove(&request_id);
                    turn.uncertain_steers.remove(&request_id);
                }
                if let Some(err) = error {
                    emitted.push(AgentEvent::ControlDelivery {
                        request_id,
                        operation: "steer".into(),
                        status: "consumption_unknown".into(),
                        error: Some(json!({"code": err.code, "message": err.message})),
                    });
                } else {
                    emitted.push(AgentEvent::SteerRejected { request_id });
                }
            } else if final_settle {
                let uncertain = self
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.uncertain_steers.contains(&request_id))
                    || self
                        .pending_messages
                        .values()
                        .any(|message| message["sender"] == "user");
                if let Some(turn) = &mut self.turn {
                    turn.steers.remove(&request_id);
                    turn.uncertain_steers.remove(&request_id);
                }
                if uncertain {
                    emitted.push(AgentEvent::ControlDelivery {
                        request_id,
                        operation: "steer".into(),
                        status: "consumption_unknown".into(),
                        error: Some(json!({
                            "code": "agy.steer_consumption_unknown",
                            "message": "Turn ended with unverified pending user message state"
                        })),
                    });
                } else {
                    emitted.push(AgentEvent::SteerRejected { request_id });
                }
            }
        }
        for event in emitted {
            self.emit(event).await;
        }
    }
    async fn settle(&mut self) -> Result<(), RuntimeError> {
        let ready = self.fully_idle
            && self.inputs.is_empty()
            && self.turn.as_ref().is_some_and(|t| {
                t.saw_busy
                    && match &t.end {
                        Some((TurnStatus::Completed, _)) => {
                            t.steers.is_empty()
                                || t.steers.iter().all(|id| {
                                    t.uncertain_steers.contains(id)
                                        || self
                                            .pending_messages
                                            .values()
                                            .any(|m| matches_steer_payload(m, id))
                                })
                        }
                        Some(_) => true,
                        None => false,
                    }
            });
        if !ready {
            return Ok(());
        }
        self.cancel_unconsumed_steers(true).await;
        let mut pending_compactions = Vec::new();
        if let Some(turn) = &mut self.turn {
            let trigger = if turn.is_manual_compact { "manual" } else { "auto" };
            let fallback_post = turn.compacted_tokens.or(self.context_used);
            for cp in turn.checkpoints.values_mut() {
                if !cp.completed_emitted
                    && let Some(st) = cp.terminal_status
                {
                    cp.completed_emitted = true;
                    let post_tokens = (st == "completed").then_some(fallback_post).flatten();
                    let dropped_tokens = match (turn.pre_compact_tokens, post_tokens) {
                        (Some(pre), Some(post)) if pre >= post => Some(pre - post),
                        _ => None,
                    };
                    pending_compactions.push(AgentEvent::ContextCompacted(Compaction {
                        in_progress: false,
                        status: Some(st.into()),
                        trigger: Some(trigger.into()),
                        pre_tokens: turn.pre_compact_tokens,
                        post_tokens,
                        dropped_tokens,
                        duration_ms: cp.duration_ms,
                        summary: cp.summary.clone(),
                        message: None,
                    }));
                }
            }
        }
        for event in pending_compactions {
            self.origin(event).await;
        }
        let turn = self.turn.take().unwrap();
        let (mut status, mut usage) = turn.end.unwrap();
        if let Some((&index, error)) = self.generator_errors.last_key_value()
            && self
                .usage
                .last_key_value()
                .is_none_or(|(&last, _)| index >= last)
        {
            status = TurnStatus::Failed;
            self.emit(AgentEvent::TurnFailed {
                code: "agy.native_generation_failed".into(),
                message: error.to_string(),
            })
            .await;
        }
        let counts: Vec<_> = self
            .usage
            .range(turn.usage_baseline..)
            .map(|(_, value)| value)
            .filter(|v| v.get("inputTokens").is_some())
            .collect();
        if !counts.is_empty() {
            let sum = |field: &str| {
                counts
                    .iter()
                    .map(|v| {
                        v[field]
                            .as_u64()
                            .or_else(|| v[field].as_str().and_then(|s| s.parse::<u64>().ok()))
                            .unwrap_or(0)
                    })
                    .sum::<u64>()
            };
            let input = sum("inputTokens");
            let output = sum("outputTokens");
            let cache = sum("cacheReadTokens");
            usage = Some(TokenUsage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                cached_input_tokens: Some(cache),
                reasoning_tokens: Some(sum("thinkingOutputTokens")),
                turn_processed_tokens: Some(input + output + cache),
                used_tokens: self.context_used,
                context_window: self.context_window,
                ..TokenUsage::default()
            });
        } else if let Some(ref mut u) = usage {
            if self.context_used.is_some() {
                u.used_tokens = self.context_used;
            }
            if self.context_window.is_some() {
                u.context_window = self.context_window;
            }
        } else if self.context_used.is_some() || self.context_window.is_some() {
            usage = Some(TokenUsage {
                used_tokens: self.context_used,
                context_window: self.context_window,
                ..TokenUsage::default()
            });
        }
        self.emit(AgentEvent::TurnCompleted {
            turn_id: turn.id,
            status,
            usage,
        })
        .await;
        self.approvals.clear();
        self.inputs.clear();
        Ok(())
    }
}

fn arguments(call: &Value) -> Result<Value, RuntimeError> {
    match call["argumentsJson"].as_str() {
        Some(value) => {
            serde_json::from_str(value).map_err(|e| failure(format!("Native tool arguments: {e}")))
        }
        None => Ok(json!({})),
    }
}
fn items(index: usize, step: &Value) -> Result<Vec<(String, ItemContent, bool)>, RuntimeError> {
    let status = step["status"].as_str().unwrap_or("");
    let done = matches!(
        status,
        "CORTEX_STEP_STATUS_DONE"
            | "CORTEX_STEP_STATUS_ERROR"
            | "CORTEX_STEP_STATUS_CANCELED"
            | "CORTEX_STEP_STATUS_INTERRUPTED"
            | "CORTEX_STEP_STATUS_HALTED"
            | "CORTEX_STEP_STATUS_CLEARED"
    );
    let item_status = match status {
        "CORTEX_STEP_STATUS_DONE" => ItemStatus::Completed,
        "CORTEX_STEP_STATUS_ERROR" | "CORTEX_STEP_STATUS_HALTED" => ItemStatus::Failed,
        "CORTEX_STEP_STATUS_CANCELED" | "CORTEX_STEP_STATUS_INTERRUPTED" => ItemStatus::Interrupted,
        "CORTEX_STEP_STATUS_CLEARED" => ItemStatus::Declined,
        _ => ItemStatus::InProgress,
    };
    let mut result = Vec::new();
    if step["type"] == "CORTEX_STEP_TYPE_PLANNER_RESPONSE" {
        for (field, kind) in [("thinking", true), ("response", false)] {
            let text = step["plannerResponse"][field].as_str().unwrap_or("");
            if !text.is_empty() {
                result.push((
                    format!("agy-{index}-{field}"),
                    if kind {
                        ItemContent::Reasoning { text: text.into() }
                    } else {
                        ItemContent::AssistantMessage { text: text.into() }
                    },
                    done,
                ));
            }
        }
    } else if step["metadata"]["toolCall"].is_object() {
        let call = &step["metadata"]["toolCall"];
        let id = call["id"]
            .as_str()
            .ok_or_else(|| failure("Native tool has no call id"))?
            .to_owned();
        let name = call["name"]
            .as_str()
            .ok_or_else(|| failure("Native tool has no name"))?;
        let input = arguments(call)?;
        let content = if step["runCommand"].is_object() {
            let command = &step["runCommand"];
            ItemContent::CommandExecution {
                command: command["commandLine"].as_str().unwrap_or("").into(),
                output: command["combinedOutput"]["full"]
                    .as_str()
                    .unwrap_or("")
                    .into(),
                exit_code: command["exitCode"].as_i64().map(|v| v as i32),
                status: item_status,
            }
        } else if step["codeAction"]["actionResult"]["edit"].is_object() {
            let edit = &step["codeAction"]["actionResult"]["edit"];
            let uri = edit["absoluteUri"]
                .as_str()
                .ok_or_else(|| failure("File edit has no absolute URI"))?;
            let path = reqwest::Url::parse(uri)
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .ok_or_else(|| failure("Invalid native file URI"))?;
            let diff = edit["diff"]["unifiedDiff"]["lines"]
                .as_array()
                .map(|lines| {
                    lines
                        .iter()
                        .map(|line| {
                            let prefix = match line["type"].as_str() {
                                Some("UNIFIED_DIFF_LINE_TYPE_INSERT") => "+",
                                Some("UNIFIED_DIFF_LINE_TYPE_DELETE") => "-",
                                _ => " ",
                            };
                            format!("{prefix}{}\n", line["text"].as_str().unwrap_or(""))
                        })
                        .collect::<String>()
                });
            ItemContent::FileChange {
                changes: vec![agent::FileChange {
                    path: path.to_string_lossy().into_owned(),
                    kind: if edit["createFile"] == true {
                        agent::FileChangeKind::Create
                    } else if edit["deleteFile"] == true {
                        agent::FileChangeKind::Delete
                    } else {
                        agent::FileChangeKind::Modify
                    },
                    diff,
                }],
                status: item_status,
            }
        } else {
            let native_error = (item_status == ItemStatus::Failed)
                .then(|| step["error"]["fullError"].as_str())
                .flatten()
                .filter(|error| !error.is_empty());
            let output = native_error.map(str::to_owned).or_else(|| {
                step["viewFile"]
                    .get("content")
                    .or_else(|| step["generic"].get("output"))
                    .or_else(|| step["generic"].get("result"))
                    .map(|value| value.as_str().map_or_else(|| value.to_string(), str::to_owned))
            });
            ItemContent::ToolCall {
                presentation: None,
                image_reads: Vec::new(),
                name: name.into(),
                input,
                output,
                status: item_status,
            }
        };
        result.push((id, content, done));
    }
    Ok(result)
}
