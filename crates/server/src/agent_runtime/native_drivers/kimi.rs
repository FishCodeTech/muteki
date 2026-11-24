//! Kimi Local Server REST commands and durable WebSocket events.
fn supervised_command(program:impl AsRef<std::ffi::OsStr>)->tokio::process::Command{tokio::process::Command::from(agent::supervised_command(program))}

use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone)]
struct Api {
    client: reqwest::Client,
    base: String,
    token: String,
    session: String,
    paths: Vec<String>,
}
impl Api {
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, RuntimeError> {
        let mut req = self.client.request(method, format!("{}{path}", self.base));
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let response = req.send().await.map_err(protocol_error)?;
        let status = response.status();
        let envelope: Value = response.json().await.map_err(protocol_error)?;
        if !status.is_success()
            || envelope
                .get("code")
                .is_some_and(|code| !code.is_null() && code != 0 && code != "0")
        {
            return Err(RuntimeError::new(
                "kimi.local_server.request_failed",
                format!(
                    "Kimi {path}: HTTP {status}, code={}, {}",
                    envelope["code"],
                    text(&envelope["msg"])
                ),
            ));
        }
        Ok(envelope.get("data").cloned().unwrap_or(envelope))
    }
    async fn post(&self, suffix: &str, body: Value) -> Result<Value, RuntimeError> {
        self.request(
            reqwest::Method::POST,
            &format!("/api/v1/sessions/{}{suffix}", self.session),
            Some(body),
        )
        .await
    }
    async fn connect(&self, cursor: &mut Value) -> Result<Socket, RuntimeError> {
        let url = format!("{}/api/v1/ws", self.base.replacen("http", "ws", 1));
        let mut request = url.into_client_request().map_err(protocol_error)?;
        if !self.token.is_empty() {
            request.headers_mut().insert(
                "authorization",
                format!("Bearer {}", self.token)
                    .parse()
                    .map_err(protocol_error)?,
            );
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut ws, _) = tokio_tungstenite::connect_async(request).await.map_err(protocol_error)?;
            loop {
                let message = ws.next().await.ok_or_else(|| protocol_error("Kimi WebSocket closed before subscription"))?.map_err(protocol_error)?;
                if let Message::Ping(bytes) = message { ws.send(Message::Pong(bytes)).await.map_err(protocol_error)?; continue; }
                let Message::Text(raw) = message else { continue; };
                let frame: Value = serde_json::from_str(&raw).map_err(protocol_error)?;
                match frame["type"].as_str().unwrap_or("") {
                    "server_hello" => subscribe(&mut ws, &self.session, cursor).await?,
                    "resync_required" => {
                        *cursor = json!({"seq":frame["payload"]["current_seq"].as_u64().unwrap_or(0),"epoch":frame["payload"]["epoch"].as_str().unwrap_or("")});
                        subscribe(&mut ws, &self.session, cursor).await?;
                    }
                    "ping" => pong(&mut ws, &frame).await?,
                    "ack" => return Ok(ws),
                    _ => {}
                }
            }
        }).await.map_err(|_| protocol_error("Kimi WebSocket subscription timed out"))?
    }
}
fn protocol_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::new("kimi.local_server.protocol_error", error.to_string())
}
fn text(value: &Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| {
        if value.is_null() {
            String::new()
        } else {
            value.to_string()
        }
    })
}
async fn subscribe(ws: &mut Socket, session: &str, cursor: &Value) -> Result<(), RuntimeError> {
    ws.send(Message::Text(
        json!({"type":"subscribe","id":format!("sub-{}",uuid::Uuid::new_v4().simple()),
        "payload":{"session_ids":[session],"cursors":{session:cursor}}})
        .to_string()
        .into(),
    ))
    .await
    .map_err(protocol_error)
}
async fn pong(ws: &mut Socket, frame: &Value) -> Result<(), RuntimeError> {
    ws.send(Message::Text(
        json!({"type":"pong","payload":{"nonce":frame["payload"]["nonce"]}})
            .to_string()
            .into(),
    ))
    .await
    .map_err(protocol_error)
}

#[derive(Default)]
struct Projection {
    cursor: Value,
    turn: Option<String>,
    prompt: Option<String>,
    message: String,
    text: String,
    tools: HashMap<String, ThreadItem>,
    agents: HashMap<String, ThreadItem>,
    pending: HashMap<String, Option<String>>,
    questions: HashMap<String, Vec<Value>>,
    usage: TokenUsage,
    compaction_trigger: Option<String>,
}
impl Projection {
    fn emit(tx: &Sender<AgentEvent>, event: AgentEvent) {
        let _ = tx.try_send(event);
    }
    fn finish(&mut self, tx: &Sender<AgentEvent>, status: TurnStatus) {
        let Some(turn_id) = self.turn.take() else {
            return;
        };
        self.flush_text(tx);
        self.prompt = None;
        Self::emit(
            tx,
            AgentEvent::TurnCompleted {
                turn_id,
                status,
                usage: Some(self.usage),
            },
        );
    }
    fn flush_text(&mut self, tx: &Sender<AgentEvent>) {
        if !self.text.is_empty() {
            Self::emit(
                tx,
                AgentEvent::ItemCompleted(ThreadItem {
                    id: self.message.clone(),
                    parent_item_id: None,
                    content: ItemContent::AssistantMessage {
                        text: std::mem::take(&mut self.text),
                    },
                }),
            );
        }
        self.message = format!("msg-{}", uuid::Uuid::new_v4().simple());
    }
    fn frame(&mut self, tx: &Sender<AgentEvent>, frame: &Value) {
        if frame["volatile"] != true
            && let Some(seq) = frame["seq"].as_u64()
        {
            let same_epoch = frame["epoch"]
                .as_str()
                .is_none_or(|epoch| self.cursor["epoch"] == epoch);
            if same_epoch && seq <= self.cursor["seq"].as_u64().unwrap_or(0) {
                return;
            }
            self.cursor["seq"] = json!(seq);
            if frame["epoch"].is_string() {
                self.cursor["epoch"] = frame["epoch"].clone();
            }
        }
        let mut kind = frame["type"].as_str().unwrap_or("");
        let mut data = frame
            .get("payload")
            .or_else(|| frame.get("data"))
            .unwrap_or(frame)
            .clone();
        if kind == "interaction" {
            let id = text(&data["interaction_id"]);
            if data["kind"] == "question" {
                if data["status"] == "pending" {
                    let questions = data["request"]["questions"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    if self.questions.contains_key(&id) {
                        return;
                    }
                    let projected = questions
                        .iter()
                        .map(|q| UserInputQuestion { contract: agent::native_question_contract(q),
                            id: text(&q["id"]),
                            header: text(&q["header"]),
                            question: text(&q["question"]),
                            options: q["options"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|option| agent::UserInputOption {
                                    label: text(&option["label"]),
                                    description: text(&option["description"]),
                                })
                                .collect(),
                            multi_select: q["multi_select"] == true,
                            prefill: None,
                        })
                        .collect();
                    self.questions.insert(id.clone(), questions);
                    Self::emit(
                        tx,
                        AgentEvent::UserInputRequested {
                            request_id: id,
                            questions: projected,
                            delivery: UserInputDelivery::Blocking,
                        },
                    );
                } else if let Some(questions) = self.questions.remove(&id) {
                    let answers = data["response"]["answers"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(key, value)| {
                            let id = questions
                                .iter()
                                .find(|q| q["question"] == key.as_str())
                                .and_then(|q| q["id"].as_str())
                                .unwrap_or(key);
                            (id.to_owned(), value.clone())
                        })
                        .collect();
                    Self::emit(
                        tx,
                        AgentEvent::UserInputResolved {
                            request_id: id,
                            answers,
                        },
                    );
                }
                return;
            }
            if data["kind"] != "approval" {
                return;
            }
            if data["status"] == "pending" {
                if self.pending.contains_key(&id) {
                    return;
                }
                let owner = data["agent_id"].clone();
                let mut request = data["request"].clone();
                request["approval_id"] = json!(id);
                request["agent_id"] = owner;
                request["input"] = request["tool_input_display"].clone();
                data = request;
                kind = "event.approval.requested";
            } else {
                data["approval_id"] = json!(id);
                data["decision"] = data["response"]["decision"]
                    .as_str()
                    .or_else(|| data["outcome"].as_str())
                    .map_or(Value::Null, |value| json!(value));
                kind = "event.approval.resolved";
            }
        }
        let owner = data["agentId"]
            .as_str()
            .or_else(|| data["agent_id"].as_str())
            .filter(|id| *id != "main" && !id.is_empty());
        if kind.starts_with("subagent.") {
            let Some(id) = data["subagentId"].as_str() else {
                return;
            };
            let item = self
                .agents
                .entry(id.to_owned())
                .or_insert_with(|| ThreadItem {
                    id: id.to_owned(),
                    parent_item_id: data["parentAgentId"]
                        .as_str()
                        .filter(|id| *id != "main")
                        .map(str::to_owned),
                    content: ItemContent::Subagent {
                        agent_type: text(&data["subagentName"]),
                        description: text(&data["description"]),
                        status: ItemStatus::InProgress,
                        summary: None,
                        model: data["model"].as_str().map(str::to_owned),
                        effort: None,
                    },
                });
            if let ItemContent::Subagent {
                status, summary, ..
            } = &mut item.content
            {
                *status = match kind {
                    "subagent.completed" => ItemStatus::Completed,
                    "subagent.failed" => ItemStatus::Failed,
                    "subagent.cancelled" => ItemStatus::Interrupted,
                    _ => ItemStatus::InProgress,
                };
                if let Some(result) = data.get("resultSummary").or_else(|| data.get("error")) {
                    *summary = Some(text(result));
                }
            }
            Self::emit(tx, AgentEvent::ItemUpdated(item.clone()));
            return;
        }
        if let Some(owner) = owner {
            if matches!(kind, "assistant.delta" | "assistant.message") {
                let item = self
                    .agents
                    .entry(owner.to_owned())
                    .or_insert_with(|| ThreadItem {
                        id: owner.to_owned(),
                        parent_item_id: None,
                        content: ItemContent::Subagent {
                            agent_type: "kimi".into(),
                            description: owner.to_owned(),
                            status: ItemStatus::InProgress,
                            summary: None,
                            model: None,
                            effort: None,
                        },
                    });
                if let ItemContent::Subagent { summary, .. } = &mut item.content {
                    let delta = text(
                        data.get("delta")
                            .or_else(|| data.get("text"))
                            .unwrap_or(&Value::Null),
                    );
                    if kind == "assistant.message" {
                        *summary = Some(delta);
                    } else {
                        summary.get_or_insert_default().push_str(&delta);
                    }
                }
                Self::emit(tx, AgentEvent::ItemUpdated(item.clone()));
                return;
            }
            if !matches!(
                kind,
                "tool.call.started"
                    | "tool.result"
                    | "event.approval.requested"
                    | "event.approval.resolved"
            ) {
                return;
            }
        }
        match kind {
            "turn.ended" => {
                let reason = data["reason"]
                    .as_str()
                    .or_else(|| data["stop_reason"].as_str())
                    .unwrap_or("end_turn");
                let status = match reason {
                    "cancelled" | "aborted" | "interrupted" => TurnStatus::Interrupted,
                    "failed" | "error" => {
                        Self::emit(
                            tx,
                            AgentEvent::TurnFailed {
                                code: "kimi.turn.failed".into(),
                                message: text(data.get("error").unwrap_or(&json!(reason))),
                            },
                        );
                        TurnStatus::Failed
                    }
                    _ => TurnStatus::Completed,
                };
                self.finish(tx, status);
            }
            "assistant.delta" | "assistant.message" if self.turn.is_some() => {
                let delta = text(
                    data.get("delta")
                        .or_else(|| data.get("text"))
                        .unwrap_or(&Value::Null),
                );
                if kind == "assistant.message" {
                    self.text = delta;
                    self.flush_text(tx);
                } else if !delta.is_empty() {
                    self.text.push_str(&delta);
                    Self::emit(
                        tx,
                        AgentEvent::Delta {
                            item_id: self.message.clone(),
                            kind: DeltaKind::AssistantText,
                            text: delta,
                        },
                    );
                }
            }
            "thinking.delta" if self.turn.is_some() => Self::emit(
                tx,
                AgentEvent::Delta {
                    item_id: format!("{}-thinking", self.message),
                    kind: DeltaKind::ReasoningText,
                    text: text(
                        data.get("delta")
                            .or_else(|| data.get("text"))
                            .unwrap_or(&Value::Null),
                    ),
                },
            ),
            "tool.call.started" => {
                if owner.is_none() {
                    self.flush_text(tx);
                }
                let id = data["toolCallId"]
                    .as_str()
                    .or_else(|| data["call_id"].as_str())
                    .or_else(|| data["id"].as_str())
                    .unwrap_or("")
                    .to_owned();
                let item = ThreadItem {
                    id: id.clone(),
                    parent_item_id: owner.map(str::to_owned),
                    content: ItemContent::ToolCall {
                        presentation: None,
                        image_reads: Vec::new(),
                        name: text(
                            data.get("name")
                                .or_else(|| data.get("tool"))
                                .unwrap_or(&Value::Null),
                        ),
                        input: data
                            .get("args")
                            .or_else(|| data.get("arguments"))
                            .cloned()
                            .unwrap_or(Value::Null),
                        output: None,
                        status: ItemStatus::InProgress,
                    },
                };
                self.tools.insert(id, item.clone());
                Self::emit(tx, AgentEvent::ItemStarted(item));
            }
            "tool.result" => {
                let id = data["toolCallId"]
                    .as_str()
                    .or_else(|| data["call_id"].as_str())
                    .or_else(|| data["id"].as_str())
                    .unwrap_or("");
                if let Some(mut item) = self.tools.remove(id) {
                    if let ItemContent::ToolCall { output, status, .. } = &mut item.content {
                        *output = Some(text(
                            data.get("output")
                                .or_else(|| data.get("content"))
                                .unwrap_or(&Value::Null),
                        ));
                        *status = if data["is_error"] == true {
                            ItemStatus::Failed
                        } else {
                            ItemStatus::Completed
                        };
                    }
                    Self::emit(tx, AgentEvent::ItemCompleted(item));
                }
            }
            "event.approval.requested" => {
                let id = data["approval_id"]
                    .as_str()
                    .or_else(|| data["id"].as_str())
                    .unwrap_or("")
                    .to_owned();
                let tool = text(&data["tool_name"]);
                let input = data
                    .get("input")
                    .or_else(|| data.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| data.clone());
                let native_execution=Some(json!({"tool_name":tool,"action":data["action"].as_str().filter(|value|!value.is_empty()).unwrap_or(&tool),"arguments":if input.is_object(){input.clone()}else{data.clone()},"resource_kind":match tool.as_str(){"Write"|"Edit"|"MultiEdit"=>"file","Bash"|"Shell"=>"process",_=>"mcp"},"cwd":data["cwd"]}));
                let approval_kind = match tool.as_str() {
                    "Write" | "Edit" => ApprovalKind::FileChange {
                        changes: Vec::new(),
                        reason: Some(text(&data["action"])),
                    },
                    "Bash" | "Shell" => ApprovalKind::ExecCommand {
                        command: input["command"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| text(&data["action"])),
                        cwd: data["cwd"].as_str().map(str::to_owned),
                        reason: None,
                    },
                    _ => ApprovalKind::ToolUse {
                        name: tool,
                        input,
                        detail: text(&data["action"]),
                    },
                };
                self.pending.insert(id.clone(), owner.map(str::to_owned));
                Self::emit(
                    tx,
                    AgentEvent::ApprovalRequested(ApprovalRequest {native_execution, tool_call_id: data["toolCallId"].as_str().or_else(||data["call_id"].as_str()).map(str::to_owned),
                        id,
                        turn_id: self.turn.clone(),
                        parent_item_id: owner.map(str::to_owned),
                        kind: approval_kind,
                        options: vec![
                            ApprovalOption {
                                id: "approved".into(),
                                label: "Allow".into(),
                                kind: ApprovalOptionKind::AllowOnce,
                            },
                            ApprovalOption {
                                id: "rejected".into(),
                                label: "Reject".into(),
                                kind: ApprovalOptionKind::RejectOnce,
                            },
                        ],
                    }),
                );
            }
            "event.approval.resolved" => {
                let id = text(&data["approval_id"]);
                if self.pending.remove(&id).is_some() {
                    Self::emit(
                        tx,
                        AgentEvent::ApprovalResolved {
                            request_id: id,
                            decision: ApprovalDecision::Option(text(&data["decision"])),
                        },
                    );
                }
            }
            "usage" | "turn.step.completed" => {
                let usage = if kind == "usage" {
                    &data
                } else {
                    &data["usage"]
                };
                let input = ["inputOther", "inputCacheRead", "inputCacheCreation"]
                    .iter()
                    .filter_map(|key| usage[*key].as_u64())
                    .reduce(|a, b| a + b);
                let step = TokenUsage {
                    input_tokens: input,
                    output_tokens: usage["output"].as_u64(),
                    cached_input_tokens: usage["inputCacheRead"].as_u64(),
                    ..Default::default()
                };
                for (total, next) in [
                    (&mut self.usage.input_tokens, step.input_tokens),
                    (&mut self.usage.output_tokens, step.output_tokens),
                    (
                        &mut self.usage.cached_input_tokens,
                        step.cached_input_tokens,
                    ),
                ] {
                    if let Some(next) = next {
                        *total = Some(total.unwrap_or(0) + next);
                    }
                }
                Self::emit(tx, AgentEvent::TokenUsage(step));
            }
            "agent.status.updated" => {
                let used = data["contextTokens"].as_u64();
                let window = data["maxContextTokens"].as_u64();
                if (used, window) != (self.usage.used_tokens, self.usage.context_window) {
                    self.usage.used_tokens = used;
                    self.usage.context_window = window;
                    Self::emit(
                        tx,
                        AgentEvent::TokenUsage(TokenUsage {
                            used_tokens: used,
                            context_window: window,
                            freshness: agent::ContextFreshness::Current,
                            ..Default::default()
                        }),
                    );
                }
            }
            "compaction.started"
            | "compaction.completed"
            | "compaction.cancelled"
            | "compaction.blocked" => {
                if kind == "compaction.started" {
                    self.compaction_trigger =
                        Some(data["trigger"].as_str().unwrap_or("auto").to_owned());
                }
                Self::emit(
                    tx,
                    AgentEvent::ContextCompacted(Compaction {
                        in_progress: kind == "compaction.started",
                        status: match kind {
                            "compaction.cancelled" => Some("interrupted".into()),
                            "compaction.blocked" => Some("failed".into()),
                            _ => None,
                        },
                        trigger: self.compaction_trigger.clone(),
                        pre_tokens: data["result"]["tokensBefore"].as_u64(),
                        post_tokens: data["result"]["tokensAfter"].as_u64(),
                        summary: data["result"]["summary"].as_str().map(str::to_owned),
                        message: match kind {
                            "compaction.cancelled" => Some("Kimi 压缩已取消".into()),
                            "compaction.blocked" => Some("Kimi 当前回合阻止压缩".into()),
                            _ => None,
                        },
                        ..Default::default()
                    }),
                );
            }
            _ => {}
        }
    }
}

async fn stream(
    mut ws: Socket,
    api: Api,
    projection: Arc<Mutex<Projection>>,
    tx: Sender<AgentEvent>,
) {
    loop {
        while let Some(message) = ws.next().await {
            let frame = match message {
                Ok(Message::Text(raw)) => serde_json::from_str::<Value>(&raw).ok(),
                Ok(Message::Ping(bytes)) => {
                    if ws.send(Message::Pong(bytes)).await.is_err() {
                        break;
                    }
                    None
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => None,
            };
            let Some(frame) = frame else {
                continue;
            };
            match frame["type"].as_str().unwrap_or("") {
                "ping" => {
                    if pong(&mut ws, &frame).await.is_err() {
                        break;
                    }
                }
                "ack" => {}
                "resync_required" => {
                    let cursor = json!({"seq":frame["payload"]["current_seq"].as_u64().unwrap_or(0),"epoch":frame["payload"]["epoch"].as_str().unwrap_or("")});
                    projection.lock().await.cursor = cursor.clone();
                    Projection::emit(
                        &tx,
                        AgentEvent::Warning {
                            message:
                                "Kimi event cursor changed; resubscribing at the server watermark"
                                    .into(),
                        },
                    );
                    if subscribe(&mut ws, &api.session, &cursor).await.is_err() {
                        break;
                    }
                }
                _ => projection.lock().await.frame(&tx, &frame),
            }
        }
        if tx.is_closed() {
            return;
        }
        Projection::emit(
            &tx,
            AgentEvent::Warning {
                message: "Kimi event stream disconnected; reconnecting with the last event cursor"
                    .into(),
            },
        );
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut cursor = projection.lock().await.cursor.clone();
            if let Ok(connected) = api.connect(&mut cursor).await {
                projection.lock().await.cursor = cursor;
                ws = connected;
                break;
            }
            if tx.is_closed() {
                return;
            }
        }
    }
}

fn prompt_body(
    text: String,
    attachments: Vec<agent::Attachment>,
    model: &Option<String>,
    permission: &str,
) -> Value {
    let mut content = vec![json!({"type":"text","text":text})];
    for attachment in attachments {
        let part = match attachment.workspace_file() {
            Some(path) => json!({"type":"file","path":path,"media_type":attachment.media_type}),
            None => json!({"type":"image","source":{"kind":"base64","media_type":attachment.media_type,"data":attachment.data_base64}}),
        };
        content.push(part);
    }
    let mut body = json!({"content":content,"permission_mode":permission});
    if let Some(model) = model {
        body["model"] = json!(model);
    }
    body
}

pub async fn start_kimi_local_server(
    binary: &Path,
    cwd: &Path,
    model: Option<String>,
    access: &AccessPlan,
    env: &[(String, String)],
    home: Option<&Path>,
    extra_args: &[String],
    resume: Option<Value>,
) -> Result<SessionHandle, RuntimeError> {
    validate_kimi_access_mode("kimi.local_server", &access.mode, cwd)?;
    let permission = kimi_permission_mode(&access.mode)?;
    let endpoint = env
        .iter()
        .rev()
        .find(|(key, value)| key == "MUTEKI_ADAPTER_ENDPOINT" && !value.trim().is_empty())
        .map(|(_, value)| value.trim().trim_end_matches('/').to_owned());
    let mut child = None;
    let base = if let Some(endpoint) = endpoint {
        endpoint
    } else {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(protocol_error)?;
        let port = listener.local_addr().map_err(protocol_error)?.port();
        drop(listener);
        let mut command = supervised_command(binary);
        command
            .args([
                "web",
                "--no-open",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .args(extra_args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        super::apply_env_pairs(&mut command, env);
        spawn_in_process_group(&mut command);
        child = Some(command.spawn().map_err(protocol_error)?);
        format!("http://127.0.0.1:{port}")
    };
    let client = crate::network_proxy::http_client_builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(protocol_error)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let openapi = loop {
        let token = resolve_kimi_server_token(env, home).unwrap_or_default();
        let request = client
            .get(format!("{base}/openapi.json"))
            .bearer_auth(token);
        if let Ok(response) = request.send().await
            && response.status().is_success()
            && let Ok(doc) = response.json::<Value>().await
        {
            break doc;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RuntimeError::new(
                "kimi.local_server.unavailable",
                "Kimi Local Server did not become ready",
            ));
        }
        if let Some(child) = child.as_mut()
            && child.try_wait().map_err(protocol_error)?.is_some()
        {
            return Err(protocol_error(
                "Kimi Local Server exited before becoming ready",
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let mut api = Api {
        client,
        base,
        token: resolve_kimi_server_token(env, home).unwrap_or_default(),
        session: String::new(),
        paths: openapi["paths"]
            .as_object()
            .map(|paths| paths.keys().cloned().collect())
            .unwrap_or_default(),
    };
    if !openapi.to_string().contains("permission_mode") {
        return Err(RuntimeError::unsupported(
            "Kimi OpenAPI has no prompt permission_mode",
        ));
    }
    let native_model = if api.paths.iter().any(|path| path == "/api/v1/config") {
        let config = api
            .request(reqwest::Method::GET, "/api/v1/config", None)
            .await?;
        match model.as_deref() {
            None => config["default_model"]
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            Some(selected) if config["models"].get(selected).is_some() => Some(selected.to_owned()),
            Some(selected) => {
                let matching: Vec<_> = config["models"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(_, definition)| definition["model"] == selected)
                    .map(|(alias, _)| alias.clone())
                    .collect();
                if matching.len() == 1 {
                    matching.into_iter().next()
                } else {
                    Some(selected.to_owned())
                }
            }
        }
    } else {
        model.clone()
    };
    let resumed = resume.as_ref().and_then(|r| {
        r["session_id"]
            .as_str()
            .or_else(|| r["id"].as_str())
            .or_else(|| r.as_str())
    });
    if let Some(id) = resumed {
        api.request(
            reqwest::Method::GET,
            &format!("/api/v1/sessions/{id}/snapshot"),
            None,
        )
        .await?;
        api.session = id.to_owned();
    } else {
        let created = api
            .request(
                reqwest::Method::POST,
                "/api/v1/sessions",
                Some(json!({"metadata":{"cwd":cwd}})),
            )
            .await?;
        api.session = created["id"]
            .as_str()
            .or_else(|| created["session_id"].as_str())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| protocol_error("Kimi create session returned no session id"))?
            .to_owned();
    }
    let mut cursor = resume
        .as_ref()
        .and_then(|r| r.get("cursor"))
        .cloned()
        .unwrap_or_else(|| json!({"seq":0,"epoch":""}));
    let ws = api.connect(&mut cursor).await?;
    let projection = Arc::new(Mutex::new(Projection {
        cursor: cursor.clone(),
        ..Default::default()
    }));
    let (cmd_tx, cmd_rx) = smol::channel::unbounded();
    let (evt_tx, evt_rx) = smol::channel::unbounded();
    if let Some(pid)=child.as_ref().and_then(|child|child.id()){let _=evt_tx.try_send(AgentEvent::ProcessStarted{pid});}
    Projection::emit(
        &evt_tx,
        AgentEvent::SessionStarted {
            provider_session_id: api.session.clone(),
            resume: ResumeCursor(json!({"session_id":api.session,"cursor":cursor})),
            model: model.clone(),
        },
    );
    let reader = tokio::spawn(stream(ws, api.clone(), projection.clone(), evt_tx.clone()));
    tokio::spawn(async move {
        while let Ok(command) = cmd_rx.recv().await {
            let result = match command {
                SessionCommand::SendTurn {
                    delivery_id,
                    text,
                    attachments,
                    ..
                } => {
                    let turn_id = format!("turn-{}", uuid::Uuid::new_v4().simple());
                    {
                        let mut state = projection.lock().await;
                        state.turn = Some(turn_id.clone());
                        state.text.clear();
                        state.usage = TokenUsage::default();
                        state.message = format!("msg-{}", uuid::Uuid::new_v4().simple());
                    }
                    Projection::emit(
                        &evt_tx,
                        AgentEvent::TurnStarted {
                            turn_id: turn_id.clone(),
                        },
                    );
                    match api
                        .post(
                            "/prompts",
                            prompt_body(text, attachments, &native_model, permission),
                        )
                        .await
                    {
                        Ok(data) => {
                            let mut state = projection.lock().await;
                            if state.turn.as_deref() == Some(&turn_id) {
                                state.prompt = data["prompt_id"].as_str().map(str::to_owned);
                            }
                            Projection::emit(&evt_tx, AgentEvent::TurnAccepted { delivery_id });
                            Ok(())
                        }
                        Err(error) => {
                            Projection::emit(
                                &evt_tx,
                                AgentEvent::TurnFailed {
                                    code: error.code.into(),
                                    message: error.message,
                                },
                            );
                            projection.lock().await.finish(&evt_tx, TurnStatus::Failed);
                            Ok(())
                        }
                    }
                }
                SessionCommand::Steer {
                    request_id,
                    text,
                    attachments,
                } => {
                    let result = async {
                        let session_steer = api
                            .paths
                            .iter()
                            .any(|path| path.ends_with("/prompts:steer"));
                        if !session_steer
                            && !api
                                .paths
                                .iter()
                                .any(|path| path.contains("/prompts/{") && path.ends_with(":steer"))
                        {
                            return Err(RuntimeError::unsupported(
                                "Kimi instance does not declare prompt steer",
                            ));
                        }
                        let prompt = api
                            .post(
                                "/prompts",
                                prompt_body(text, attachments, &native_model, permission),
                            )
                            .await?;
                        let id = prompt["prompt_id"]
                            .as_str()
                            .ok_or_else(|| protocol_error("Kimi steer prompt has no id"))?;
                        if session_steer {
                            api.post("/prompts:steer", json!({"prompt_ids":[id]}))
                                .await?;
                        } else {
                            api.post(&format!("/prompts/{id}:steer"), json!({})).await?;
                        }
                        Ok(())
                    }
                    .await;
                    Projection::emit(
                        &evt_tx,
                        if result.is_ok() {
                            AgentEvent::SteerAccepted { request_id }
                        } else {
                            AgentEvent::SteerRejected { request_id }
                        },
                    );
                    result
                }
                SessionCommand::RespondApproval {
                    request_id,
                    decision,
                } => {
                    let native = match &decision {
                        ApprovalDecision::Cancel => "cancelled",
                        ApprovalDecision::Option(id)
                            if matches!(
                                id.as_str(),
                                "approved" | "approve" | "approve_for_session"
                            ) =>
                        {
                            "approved"
                        }
                        _ => "rejected",
                    };
                    let interactions = api.paths.iter().any(|path| path.contains("/interactions/"));
                    let mut body = json!({"decision":native});
                    if interactions {
                        body["kind"] = json!("approval");
                    }
                    if matches!(&decision, ApprovalDecision::Option(id) if id == "approve_for_session")
                    {
                        body["scope"] = json!("session");
                    }
                    let route = if interactions {
                        "interactions"
                    } else {
                        "approvals"
                    };
                    match api.post(&format!("/{route}/{request_id}"), body).await {
                        Ok(_) => {
                            if projection
                                .lock()
                                .await
                                .pending
                                .remove(&request_id)
                                .is_some()
                            {
                                Projection::emit(
                                    &evt_tx,
                                    AgentEvent::ApprovalResolved {
                                        request_id,
                                        decision,
                                    },
                                );
                            }
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
                SessionCommand::RespondUserInput {
                    request_id,
                    answers,
                } => {
                    let questions = projection
                        .lock()
                        .await
                        .questions
                        .get(&request_id)
                        .cloned()
                        .unwrap_or_default();
                    let mut encoded = serde_json::Map::new();
                    for (qid, answer) in &answers {
                        let options = questions
                            .iter()
                            .find(|q| q["id"] == qid.as_str())
                            .and_then(|q| q["options"].as_array());
                        let selected: Vec<&str> = if let Some(values) = answer.as_array() {
                            values.iter().filter_map(Value::as_str).collect()
                        } else {
                            answer.as_str().into_iter().collect()
                        };
                        let mut ids = Vec::new();
                        let mut other = Vec::new();
                        for value in selected {
                            if let Some(id) = options
                                .into_iter()
                                .flatten()
                                .find(|o| o["label"] == value || o["id"] == value)
                                .and_then(|o| o["id"].as_str())
                            {
                                ids.push(id);
                            } else {
                                other.push(value);
                            }
                        }
                        let wire = if ids.is_empty() {
                            json!({"kind":"other","text":other.join(", ")})
                        } else if !other.is_empty() {
                            json!({"kind":"multi_with_other","option_ids":ids,"other_text":other.join(", ")})
                        } else if answer.is_array() {
                            json!({"kind":"multi","option_ids":ids})
                        } else {
                            json!({"kind":"single","option_id":ids[0]})
                        };
                        encoded.insert(qid.clone(), wire);
                    }
                    match api
                        .post(
                            &format!("/interactions/{request_id}"),
                            json!({"kind":"question","answers":encoded}),
                        )
                        .await
                    {
                        Ok(_) => {
                            if projection
                                .lock()
                                .await
                                .questions
                                .remove(&request_id)
                                .is_some()
                            {
                                Projection::emit(
                                    &evt_tx,
                                    AgentEvent::UserInputResolved {
                                        request_id,
                                        answers,
                                    },
                                );
                            }
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
                SessionCommand::Compact => api.post(":compact", json!({})).await.map(|_| ()),
                SessionCommand::Interrupt => {
                    let prompt = projection.lock().await.prompt.clone();
                    let path = if let Some(id) = prompt.filter(|_| {
                        api.paths.iter().any(|path| {
                            path.ends_with("/prompts/{tail}")
                                || (path.contains("/prompts/") && path.ends_with(":abort"))
                        })
                    }) {
                        Some(format!("/prompts/{id}:abort"))
                    } else if api
                        .paths
                        .iter()
                        .any(|path| path.ends_with(":abort") && !path.contains("/prompts/"))
                    {
                        Some(":abort".into())
                    } else {
                        None
                    };
                    if let Some(path) = path {
                        api.post(&path, json!({})).await.map(|_| ())
                    } else {
                        Err(RuntimeError::unsupported(
                            "Kimi instance has no active prompt abort route",
                        ))
                    }
                }
                SessionCommand::Shutdown => break,
                _ => Err(RuntimeError::unsupported(
                    "Operation is not provided by Kimi Local Server",
                )),
            };
            if let Err(error) = result {
                Projection::emit(
                    &evt_tx,
                    AgentEvent::Error {
                        message: error.message,
                        fatal: false,
                    },
                );
            }
        }
        reader.abort();
        if let Some(mut child) = child {
            kill_process_group(child.id());
            let _ = child.kill().await;
        }
        Projection::emit(
            &evt_tx,
            AgentEvent::SessionClosed {
                reason: Some("Kimi session closed".into()),
            },
        );
    });
    Ok(SessionHandle {
        provider: ProviderKind::Acp,
        commands: cmd_tx,
        events: evt_rx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_compaction_cancel_and_block_keep_their_outcomes() {
        let (tx, rx) = async_channel::bounded(8);
        let mut projection = Projection::default();
        projection.frame(
            &tx,
            &json!({"type":"compaction.started","payload":{"trigger":"manual"}}),
        );
        projection.frame(&tx, &json!({"type":"compaction.cancelled","payload":{}}));
        projection.frame(&tx, &json!({"type":"compaction.blocked","payload":{}}));
        assert!(matches!(rx.try_recv().unwrap(), AgentEvent::ContextCompacted(c) if c.in_progress));
        assert!(
            matches!(rx.try_recv().unwrap(), AgentEvent::ContextCompacted(c) if c.status.as_deref() == Some("interrupted"))
        );
        assert!(
            matches!(rx.try_recv().unwrap(), AgentEvent::ContextCompacted(c) if c.status.as_deref() == Some("failed"))
        );
    }

    #[tokio::test]
    #[ignore = "requires a real Kimi Local Server with an empty isolated profile"]
    async fn live_kimi_websocket_terminal_failure_and_resume() {
        let endpoint =
            std::env::var("MUTEKI_LIVE_KIMI_ENDPOINT").expect("real isolated Kimi endpoint");
        let home = PathBuf::from(
            std::env::var("MUTEKI_LIVE_KIMI_HOME").expect("real isolated Kimi profile"),
        );
        let env = vec![
            ("MUTEKI_ADAPTER_ENDPOINT".into(), endpoint),
            ("KIMI_CODE_HOME".into(), home.to_string_lossy().into_owned()),
        ];
        let access = AccessPlan {
            mode: access::FULL_ACCESS.into(),
            selections: Vec::new(),
            auto_allow: Default::default(),
        };
        let cwd = tempfile::tempdir().unwrap();
        let handle = start_kimi_local_server(
            Path::new("kimi"),
            cwd.path(),
            None,
            &access,
            &env,
            None,
            &[],
            None,
        )
        .await
        .unwrap();
        let resume = match handle.events.recv().await.unwrap() {
            AgentEvent::SessionStarted { resume, .. } => resume.0,
            event => panic!("{event:?}"),
        };
        handle
            .commands
            .send(SessionCommand::SendTurn {
                delivery_id: 1,
                text: "Reply with OK".into(),
                options: None,
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        let events = tokio::time::timeout(Duration::from_secs(30), async {
            let mut events = Vec::new();
            loop {
                let event = handle.events.recv().await.unwrap();
                let terminal = matches!(event, AgentEvent::TurnCompleted { .. });
                events.push(event);
                if terminal {
                    return events;
                }
            }
        })
        .await
        .unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::TurnAccepted { delivery_id: 1 })),
            "{events:?}"
        );
        assert!(
            events.iter().any(
                |event| matches!(event,AgentEvent::TurnFailed {code,..} if code=="kimi.turn.failed")
            ),
            "{events:?}"
        );
        assert!(
            matches!(
                events.last().unwrap(),
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Failed,
                    ..
                }
            ),
            "{events:?}"
        );
        handle
            .commands
            .send(SessionCommand::Shutdown)
            .await
            .unwrap();
        while !matches!(
            handle.events.recv().await.unwrap(),
            AgentEvent::SessionClosed { .. }
        ) {}
        let resumed = start_kimi_local_server(
            Path::new("kimi"),
            cwd.path(),
            None,
            &access,
            &env,
            None,
            &[],
            Some(resume.clone()),
        )
        .await
        .unwrap();
        assert!(
            matches!(resumed.events.recv().await.unwrap(),AgentEvent::SessionStarted {provider_session_id,..} if provider_session_id==resume["session_id"].as_str().unwrap())
        );
        resumed
            .commands
            .send(SessionCommand::Shutdown)
            .await
            .unwrap();
        while !matches!(
            resumed.events.recv().await.unwrap(),
            AgentEvent::SessionClosed { .. }
        ) {}
    }

    #[tokio::test]
    #[ignore = "requires a real Kimi Local Server configured with the source Ollama/Ark account"]
    async fn live_kimi_ollama_stream_and_tool() {
        let endpoint = std::env::var("MUTEKI_LIVE_KIMI_ENDPOINT")
            .expect("real Ollama/Ark-backed Kimi endpoint");
        let home = std::env::var("MUTEKI_LIVE_KIMI_HOME").expect("isolated Kimi profile");
        let env = vec![
            ("MUTEKI_ADAPTER_ENDPOINT".into(), endpoint),
            ("KIMI_CODE_HOME".into(), home),
        ];
        let access = AccessPlan {
            mode: access::FULL_ACCESS.into(),
            selections: Vec::new(),
            auto_allow: Default::default(),
        };
        let cwd = tempfile::tempdir().unwrap();
        let handle = start_kimi_local_server(
            Path::new("kimi"),
            cwd.path(),
            None,
            &access,
            &env,
            None,
            &[],
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            handle.events.recv().await.unwrap(),
            AgentEvent::SessionStarted { .. }
        ));
        handle.commands.send(SessionCommand::SendTurn { delivery_id: 1,
            text: "Use your shell tool to execute `printf MUTEKI_KIMI_LIVE_OK`. Then reply with exactly MUTEKI_KIMI_LIVE_OK. This checks the local runtime tool and stream.".into(),
            options: None, attachments: Vec::new() }).await.unwrap();
        let mut tool = false;
        let mut reply = String::new();
        let mut accepted = false;
        let status = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                match handle.events.recv().await.unwrap() {
                    AgentEvent::TurnAccepted { .. } => accepted = true,
                    AgentEvent::ItemCompleted(ThreadItem {
                        content:
                            ItemContent::ToolCall {
                                output,
                                status: ItemStatus::Completed,
                                ..
                            },
                        ..
                    }) => {
                        tool |= output
                            .as_deref()
                            .is_some_and(|output| output.contains("MUTEKI_KIMI_LIVE_OK"));
                    }
                    AgentEvent::ItemCompleted(ThreadItem {
                        content: ItemContent::AssistantMessage { text },
                        ..
                    }) => reply.push_str(&text),
                    AgentEvent::TurnCompleted { status, .. } => break status,
                    AgentEvent::TurnFailed { code, message } => panic!("{code}: {message}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        handle
            .commands
            .send(SessionCommand::Shutdown)
            .await
            .unwrap();
        while !matches!(
            handle.events.recv().await.unwrap(),
            AgentEvent::SessionClosed { .. }
        ) {}
        assert_eq!(status, TurnStatus::Completed);
        assert!(
            accepted && tool && reply.contains("MUTEKI_KIMI_LIVE_OK"),
            "accepted={accepted}, tool={tool}, reply={reply}"
        );
    }

    #[test]
    fn volatile_deltas_share_the_durable_watermark() {
        let (tx, rx) = smol::channel::unbounded();
        let mut state = Projection {
            cursor: json!({"seq":10,"epoch":"one"}),
            turn: Some("main".into()),
            message: "message".into(),
            ..Default::default()
        };
        for (offset, text) in [(1, "hello "), (2, "world")] {
            state.frame(&tx, &json!({"type":"assistant.delta","seq":10,"epoch":"one", "volatile":true,"offset":offset,
                "payload":{"agentId":"main","delta":text}}));
        }
        assert_eq!(state.cursor["seq"], 10);
        state.frame(&tx, &json!({"type":"turn.ended","seq":11,"epoch":"one","payload":{"agentId":"main","reason":"end_turn"}}));
        let mut completed = None;
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::AssistantMessage { text },
                ..
            }) = event
            {
                completed = Some(text);
            }
        }
        assert_eq!(completed.as_deref(), Some("hello world"));
    }

    #[test]
    fn child_terminal_and_durable_replay_do_not_finish_main_turn() {
        let (tx, rx) = smol::channel::unbounded();
        let mut state = Projection {
            cursor: json!({"seq":0,"epoch":"one"}),
            turn: Some("main-turn".into()),
            message: "main-message".into(),
            ..Default::default()
        };
        let delta = json!({"type":"assistant.delta","seq":1,"epoch":"one","payload":{"agentId":"main","delta":"hello"}});
        state.frame(&tx, &delta);
        state.frame(&tx, &delta);
        state.frame(&tx,&json!({"type":"turn.ended","seq":2,"epoch":"one","payload":{"agentId":"agent-1","reason":"completed"}}));
        assert_eq!(state.text, "hello");
        assert_eq!(state.turn.as_deref(), Some("main-turn"));
        state.frame(&tx,&json!({"type":"turn.ended","seq":3,"epoch":"one","payload":{"agentId":"main","reason":"end_turn"}}));
        let mut completed = 0;
        let mut deltas = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::TurnCompleted { .. } => completed += 1,
                AgentEvent::Delta { .. } => deltas += 1,
                _ => {}
            }
        }
        assert_eq!((completed, deltas), (1, 1));
    }
}
