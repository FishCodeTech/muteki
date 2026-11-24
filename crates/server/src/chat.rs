use crate::{AppState, chat_core};
mod extension_chat;
pub(crate) mod run_side;
pub(crate) use run_side::purge_run_side_chats;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    convert::Infallible,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, broadcast, watch};
use uuid::Uuid;

pub(crate) async fn extension_prompt_model(s:&Arc<AppState>,runtime:&Value,prompt:&str)->Option<Result<Value,String>>{
    if !extension_chat::registered(s,runtime["adapter_id"].as_str().unwrap_or("")){return None}
    Some(extension_chat::prompt_model(s,runtime,prompt).await)
}
pub(crate) const COLLECTION: &str = "conversation_threads";
pub(crate) type Failure = (StatusCode, Json<Value>);
pub(crate) type Api = Result<Json<Value>, Failure>;
/// Serializes short read/modify/write operations; provider I/O never holds this lock.
pub struct Runtime {
    pub(crate) active: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    pub(crate) checkout_roots: parking_lot::Mutex<HashSet<std::path::PathBuf>>,
    inbox_seq: watch::Sender<u64>,
    epoch: String,
    updates: broadcast::Sender<(String, String, Value)>,
}
impl Runtime {
    /// Identifies this server process, so state a previous one left behind is
    /// recognizable as stale instead of being treated as live.
    pub fn epoch(&self) -> &str {
        &self.epoch
    }
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<(String, String, Value)> {
        self.updates.subscribe()
    }
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            active: Mutex::new(HashMap::new()),
            checkout_roots: parking_lot::Mutex::new(HashSet::new()),
            inbox_seq: watch::channel(0).0,
            epoch: id(),
            updates: broadcast::channel(256).0,
        }
    }
}
thread_local! {
    static CHAT_LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
}
/// Marks that this thread already holds `chat.active`, so a nested approval
/// projection must not lock it again.
pub(crate) struct ChatLockFlag;
impl ChatLockFlag {
    pub(crate) fn enter() -> Self {
        CHAT_LOCK_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }
}
impl Drop for ChatLockFlag {
    fn drop(&mut self) {
        CHAT_LOCK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}
pub(crate) fn chat_lock_held() -> bool {
    CHAT_LOCK_DEPTH.with(|depth| depth.get() > 0)
}
fn fail(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error":{"message":message}})))
}
/// A refusal with a stable code; the UI branches on `code`, never on text.
pub(crate) fn fail_code(status: StatusCode, code: &str, message: &str) -> Failure {
    let category = match status {
        StatusCode::BAD_REQUEST => "validation",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => "permission",
        StatusCode::CONFLICT => {
            if code == "command.version_conflict" {
                "conflict"
            } else {
                "state"
            }
        }
        StatusCode::UNPROCESSABLE_ENTITY => "capability",
        _ => "state",
    };
    (
        status,
        Json(
            json!({"error":{"code":code,"category":category,"message":message,"retryable":false}}),
        ),
    )
}
fn semantic_request(body: &Value) -> Value {
    let mut s = body.clone();
    if let Some(obj) = s.as_object_mut() {
        obj.remove("command_id");
        obj.remove("idempotency_key");
    }
    s
}
fn db(error: rusqlite::Error) -> (StatusCode, Json<Value>) {
    tracing::error!(%error, "conversation persistence failed");
    fail_code(StatusCode::INTERNAL_SERVER_ERROR, "conversation.persistence_failed", &error.to_string())
}
fn id() -> String {
    Uuid::new_v4().to_string()
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn receipt(body: &Value, thread: &str, output: Value) -> Json<Value> {
    Json(
        json!({"receipt":{"command_id":body["command_id"],"state":"completed","aggregate":{"type":"thread","id":thread},"output":output}}),
    )
}
fn command_id(body: &Value) -> Result<&str, (StatusCode, Json<Value>)> {
    body["command_id"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "command_id is required"))
}
fn thread_not_found() -> Failure {
    fail_code(StatusCode::NOT_FOUND, "conversation.thread.not_found", "Thread not found")
}

fn normalize_thread_head(value: &mut Value) {
    let head = stream_head(value);
    value["event_seq"] = json!(head);
    value["thread"]["state"]["head_stream_seq"] = json!(head);
}

fn legacy_agent_timing(node: &Value) -> bool {
    node["completion_source"] == "persisted_tool_result" && node["timing_source"].is_null()
}

fn stale_history_mutation(epoch: &str, v: &Value) -> bool {
    let mutation = &v["thread"]["state"]["history_mutation"];
    mutation.is_object() && mutation["epoch"] != epoch
}

fn stale_queue(epoch: &str, v: &Value) -> bool {
    queue_len(v) > 0 && v["queue_epoch"] != epoch && v["thread"]["state"]["queue_pause_reason"] != "task_archived"
}

fn stale_turn(epoch: &str, v: &Value) -> bool {
    v["thread"]["state"]["status"] == "running" && v["runtime_epoch"] != epoch
}

async fn load_timeline(s: &AppState, id: &str) -> Result<Value, Failure> {
    let mut v = s.store.read_committed(COLLECTION, id).map_err(db)?.ok_or_else(thread_not_found)?;
    let epoch = &s.chat.epoch;
    if crate::native_chat::needs_recovery(epoch, &v)
        || stale_history_mutation(epoch, &v)
        || stale_queue(epoch, &v)
        || stale_turn(epoch, &v)
        || v["thread"]["state"]["agents"]["agents"].as_array().is_some_and(|agents| agents.iter().any(legacy_agent_timing))
    {
        let _guard = s.chat.active.lock().await;
        return load(s, id);
    }
    normalize_thread_head(&mut v);
    Ok(v)
}

pub(crate) fn load(state: &AppState, id: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    let mut value = state
        .store
        .get(COLLECTION, id)
        .map_err(db)?
        .ok_or_else(thread_not_found)?;
    normalize_thread_head(&mut value);
    let mut recovered = false;
    let agents_before=value["thread"]["state"]["agents"].clone();
    if let Some(closed) = crate::native_chat::recover(&state.chat.epoch, &mut value) {
        emit(state, id, &mut value, "core.session.closed", closed);
        recovered = true;
    }
    if let Some(nodes)=value["thread"]["state"]["agents"]["agents"].as_array_mut(){
        for node in nodes.iter_mut().filter(|node| legacy_agent_timing(node)){
            let call=format!("agent:{}",node["agent_id"].as_str().unwrap_or(""));
            let turn=node["turn_id"].as_str().unwrap_or("");
            let events=state.store.chat_events(id,Some(turn),0,i64::MAX as u64,10000).map_err(db)?;
            let terminal=events.iter().rev().find(|event|event["event_type"]=="core.tool.completed" && event["payload"]["tool_call_id"]==call);
            node["completed_at"]=terminal.map(|event|event["occurred_at"].clone()).unwrap_or(Value::Null);
            node["duration_ms"]=terminal.map(|event|event["payload"]["duration_ms"].clone()).unwrap_or(Value::Null);
            node["timing_source"]=json!(if terminal.is_some(){"persisted_tool_event"}else{"unknown"});
        }
    }
    if value["thread"]["state"]["agents"]!=agents_before{
        let time=now();
        let agents=&mut value["thread"]["state"]["agents"];
        agents["revision"]=json!(agents["revision"].as_u64().unwrap_or(0)+1);
        agents["updated_at"]=json!(time);
        if let Some(nodes)=agents["agents"].as_array_mut(){for node in nodes.iter_mut().filter(|node|!node["completion_source"].is_null()){node["updated_at"]=json!(time);}}
        let payload=agents.clone();
        emit(state,id,&mut value,"core.agent.updated",payload);
        recovered=true;
    }
    if stale_history_mutation(&state.chat.epoch, &value) {
        value["thread"]["state"]
            .as_object_mut()
            .unwrap()
            .remove("history_mutation");
        recovered = true;
    }
    if stale_queue(&state.chat.epoch, &value) {
        pause_queue(&mut value, "process_restarted");
        value["queue_epoch"] = json!(state.chat.epoch);
        recovered = true;
    }
    if value["thread"]["state"]["status"] == "running" && value["runtime_epoch"] != state.chat.epoch
    {
        let turn = value["thread"]["state"]["running_turn_id"].clone();
        finish_value(
            &mut value,
            &turn,
            "interrupted",
            Some("Server restarted; the interrupted reply was not resumed"),
        );
        emit(
            state,
            id,
            &mut value,
            "core.turn.interrupted",
            json!({"turn_id":turn,"status":"interrupted","error":{"message":"Server restarted; the interrupted reply was not resumed"}}),
        );
        recovered = true;
    }
    if recovered {
        save(state, id, &mut value)?;
    }
    Ok(value)
}
pub(crate) fn save(
    state: &AppState,
    id: &str,
    v: &mut Value,
) -> Result<(), (StatusCode, Json<Value>)> {
    let (events, watermark) = state.store.chat_commit(id, v).map_err(db)?;
    finish_save(state, id, v, events, watermark);
    Ok(())
}

pub(crate) fn finish_save(state: &AppState, id: &str, v: &mut Value, events: Vec<Value>, watermark: u64) {
    v.as_object_mut().unwrap().remove("_pending_events");
    v["event_watermark"] = json!(watermark);
    let changed_without_events = events.is_empty();
    let updates=state.chat.updates.clone();let thread=id.to_owned();
    let activity=state.activity.clone();let turn=v["thread"]["state"]["running_turn_id"].as_str().unwrap_or("").to_owned();
    state.store.after_commit(move||{activity.retain_turn(&thread,&turn);for event in events{let _=updates.send((thread.clone(),"event".into(),event));}});
    notify_snapshot_change(state, id, changed_without_events);
    if v["thread"]["state"]["lineage"]["origin"] == "app_owned"
        && let Err((_, Json(body))) = crate::chat_subagents::sync_node(state, id)
    {
        tracing::warn!(thread_id=id, error=%body, "Could not mirror child Thread state to its parent");
    }
}
/// Publish a committed conversation without rewriting its atomic import.
pub(crate) fn notify_imported(state: &AppState, id: &str) {
    notify_snapshot_change(state, id, true);
}

fn notify_snapshot_change(state: &AppState, id: &str, changed_without_events: bool) {
    let updates = state.chat.updates.clone();
    let sequence = state.chat.inbox_seq.clone();
    let thread = id.to_owned();
    state.store.after_commit(move || {
        sequence.send_modify(|seq| *seq += 1);
        let _ = updates.send((thread, "snapshot".into(), json!(changed_without_events)));
    });
}
fn notify_deleted(state:&AppState,tid:&str){
    let updates=state.chat.updates.clone();let sequence=state.chat.inbox_seq.clone();let thread=tid.to_owned();
    let activity=state.activity.clone();
    state.store.after_commit(move||{activity.retain_turn(&thread,"");sequence.send_modify(|seq| *seq += 1);let _=updates.send((thread.clone(),"deleted".into(),json!({"thread_id":thread})));});
}
/// Settle runtime state for threads whose documents another transaction already removed.
pub(crate) fn release_deleted(
    s: &Arc<AppState>,
    active: &mut HashMap<String, tokio::task::AbortHandle>,
    ids: &[String],
) -> Vec<crate::native_chat::Post> {
    ids.iter()
        .map(|tid| {
            if let Some(task) = active.remove(tid) {
                task.abort();
            }
            notify_deleted(s, tid);
            crate::native_chat::close(s, tid, "thread_deleted")
        })
        .collect()
}

pub(crate) async fn purge_src_owned_threads(
    s: &Arc<AppState>,
    ids: &[String],
) -> Result<(), Failure> {
    let mut active = s.chat.active.lock().await;
    let mut posts = Vec::new();
    let mut purged = Vec::new();
    for tid in ids {
        if tid.is_empty() {
            continue;
        }
        if let Some(task) = active.remove(tid) {
            task.abort();
        }
        if let Some(doc) = s.store.get(COLLECTION, tid).map_err(db)? {
            s.store.purge_conversation(tid).map_err(db)?;
            forget_deleted_child(s, tid)?;
            let _ = crate::files::remove_unused_attachments(s, &doc);
            posts.extend(purge_side_chats(s, &mut active, tid)?);
            purged.push(tid.clone());
        }
    }
    posts.extend(release_deleted(s, &mut active, &purged));
    drop(active);
    for post in posts {
        tokio::spawn(post);
    }
    Ok(())
}

fn bound_live_src_task(s: &AppState, tid: &str) -> Result<Option<String>, Failure> {
    Ok(s.store
        .list("src_tasks")
        .map_err(db)?
        .into_iter()
        .find(|row| row["lifecycle"]["state"] != "destroyed" && row["binding"]["thread_id"] == tid)
        .and_then(|row| row["task_id"].as_str().map(str::to_owned)))
}

pub(crate) fn stream_head(v: &Value) -> u64 {
    let message_head = v["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["stream_seq"].as_u64())
        .max()
        .unwrap_or(0);
    message_head.max(v["event_seq"].as_u64().unwrap_or(0)).max(
        v["thread"]["state"]["head_stream_seq"]
            .as_u64()
            .unwrap_or(0),
    )
}
pub(crate) fn emit(state: &AppState, tid: &str, v: &mut Value, kind: &str, payload: Value) {
    crate::chat_metadata::observe(state,tid,v,kind,&payload);
    let producer = if crate::native_chat::is_native(&v["runtime"])
        || (v["extension_session"].is_object()
            && v["extension_session"]["adapter_id"] == v["runtime"]["adapter_id"])
        || extension_chat::registered(state, v["runtime"]["adapter_id"].as_str().unwrap_or(""))
    {
        "builtin.conversation"
    } else {
        "builtin.http-chat"
    };
    let seq = stream_head(v) + 1;
    v["event_seq"] = json!(seq);
    v["thread"]["state"]["head_stream_seq"] = json!(seq);
    if !v["_pending_events"].is_array() {
        v["_pending_events"] = json!([]);
    }
    v["_pending_events"].as_array_mut().unwrap().push(json!({
        "event_type":kind,"event_id":id(),"stream_seq":seq,"occurred_at":now(),
        "aggregate_type":"thread","aggregate_id":tid,"producer":producer,
        "actor_id":"system","payload":payload
    }));
}
fn emit_command(
    state: &AppState,
    tid: &str,
    v: &mut Value,
    kind: &str,
    payload: Value,
    body: &Value,
) {
    emit(state, tid, v, kind, payload);
    if let Some(event) = v["_pending_events"]
        .as_array_mut()
        .and_then(|events| events.last_mut())
    {
        let command_id = body["command_id"].clone();
        event["command_id"] = command_id.clone();
        event["causation_id"] = command_id.clone();
        event["correlation_id"] = command_id;
        event["actor_id"] = json!(body["actor"]["id"].as_str().unwrap_or("local-user"));
        if let Some(key) = body["idempotency_key"].as_str() {
            event["idempotency_key"] = json!(key);
        }
    }
}
fn view(state: &AppState, v: &Value, timeline_limit: Option<usize>) -> Value {
    let mut result = match timeline_limit {
        Some(limit) => crate::conversation_view::timeline(v, limit),
        None => crate::conversation_view::snapshot(v),
    };
    if let Some((connection, rewind)) = crate::native_chat::connection(&state.chat.epoch, v) {
        result["runtime_connection"] = connection;
        result["rewind_capability"] = rewind;
    }
    if let Some(workspace_id) = v["thread"]["workspace_id"]
        .as_str()
        .filter(|id| !id.is_empty())
    {
        let workspace = if timeline_limit.is_some() {
            state.store.read_committed("workspaces", workspace_id)
        } else {
            state.store.get("workspaces", workspace_id)
        };
        match workspace {
            Ok(Some(workspace)) => {
                result["workspace"] = crate::conversation_view::select(
                    &workspace,
                    &[
                        "workspace_id",
                        "project_id",
                        "kind",
                        "root_path",
                        "settings",
                        "created_at",
                    ],
                )
            }
            Ok(None) => result["workspace"] = Value::Null,
            Err(error) => {
                tracing::error!(%error, "Could not read conversation workspace");
                result["workspace_error"] = json!("Workspace metadata is unavailable");
            }
        }
    }
    result
}
fn stream_view(state: &AppState, value: &Value) -> Value {
    let mut snapshot = view(state, value, None);
    crate::conversation_view::page(&mut snapshot, 50, None, None, true);
    snapshot
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/threads", get(list).post(create))
        .route("/api/threads/inbox/events", get(inbox))
        .route("/api/threads/{id}", get(get_thread).delete(delete_thread))
        .route("/api/threads/{id}/messages", get(messages))
        .route("/api/threads/{id}/events", get(events))
        .route("/api/threads/{id}/commands", post(command))
        .route("/api/runs/{id}/side-chats", get(run_side::list).post(run_side::create))
        .route("/api/runs/{id}/side-chats/{tid}/target", post(run_side::select_target))
        .route("/api/src/tasks/{id}/side-chats", get(run_side::list_src_task).post(run_side::create_src_task))
}
pub(crate) async fn list(State(s): State<Arc<AppState>>, Query(query): Query<HashMap<String, String>>) -> Api {
    let project = query.get("project_id").map(String::as_str).unwrap_or("");
    let workspace_overview = query
        .get("view")
        .is_some_and(|view| view == "workspace-overview");
    let inbox_view = query.get("view").is_some_and(|view| view == "inbox");
    let include_ctf = query.get("include_ctf").is_some_and(|v| v == "1" || v == "true");
    let side_of = query.get("side_of").map(String::as_str).filter(|id| !id.is_empty());
    let threads = s
        .store
        .conversation_summaries()
        .map_err(db)?
        .into_iter()
        .filter(|v| project.is_empty() || v["thread"]["project_id"] == project)
        .filter(|v| include_ctf || !is_ctf_thread(&v["thread"]))
        .filter(|v| match side_of {
            Some(parent) => v["thread"]["side_chat"]["parent_thread_id"] == parent,
            None => !is_side_chat(&v["thread"]),
        })
        .map(|v| {
            let mut thread = if workspace_overview {
                workspace_overview_thread(&v["thread"])
            } else if inbox_view {
                inbox_thread(&v)
            } else {
                listed_thread(&v)
            };
            if thread["state"].is_object() {
                if thread["state"].get("unread").is_none() {
                    thread["state"]["unread"] = json!(false);
                }
                if thread["state"].get("queued_turn").is_none() {
                    let queued = thread["state"]["queue_count"].as_u64().unwrap_or(0) > 0
                        && thread["state"]["queue_paused"] != true;
                    thread["state"]["queued_turn"] = json!(queued);
                }
            }
            thread
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"threads":threads,"count":threads.len()})))
}
pub(crate) fn is_side_chat(thread: &Value) -> bool {
    thread["side_chat"].is_object()
}
pub fn is_ctf_thread(thread: &Value) -> bool {
    thread["ctf_run_id"].as_str().is_some_and(|id| !id.is_empty())
        || thread["thread_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("ctf-pi-"))
}
/// Turn-scoped policy for side chats. Inherited history reaches the model as a
/// provider fork or a structured handoff; this block marks where it ends.
const SIDE_CHAT_POLICY: &str = "侧边聊天边界。\n\n你正在一个侧边聊天中，不是主对话。此前的对话历史（原生会话中已有的内容，或标注“主对话”的历史交接）继承自主对话，只作参考上下文，不是你当前的任务。\n\n不要继续、执行或完成边界之前出现的任何指令、计划、工具调用、审批、修改或请求。只有用户在侧边聊天中提交的消息才是有效指令。侧边聊天用于回答问题和做轻量、不产生修改的探索，不能打扰主对话，也不要把自己表现为在继续主对话的任务。\n\n可以按当前权限做只读检查，例如读取或搜索文件、运行不改变仓库跟踪文件的检查。边界之前出现的工具调用和输出发生在主对话中，只作参考，不要从中推断有效指令。\n\n侧边聊天中不得使用子 Agent，也不要与任何已有或新建的子 Agent 交互。\n\n除非用户在侧边聊天中明确要求，不要修改文件、源代码、git 状态、权限、配置或其他工作区状态，也不要申请提升权限或更大的沙箱访问。用户明确要求修改时，只做最小且局限于该请求的修改，避免干扰主对话。\n\n侧边聊天不能设置定时任务；用户提出时，说明应在主对话或新对话中设置。";
fn side_chat_part() -> Value {
    json!({"key":"muteki.side_chat","kind":"side_conversation","trust":"application","scope":"turn",
        "channel":"text_blocks","chars":SIDE_CHAT_POLICY.chars().count(),"text":SIDE_CHAT_POLICY})
}
/// A side chat is a fork whose inherited records stay out of its own transcript.
fn side_chat_value(mut fork: Value, source: &Value) -> Value {
    let fork_sources: std::collections::HashSet<String> = fork["messages"]
        .as_array().into_iter().flatten()
        .filter_map(|message| message["source_message_id"].as_str().map(str::to_owned))
        .collect();
    fork["messages"] = json!([]);
    let running = &source["thread"]["state"]["running_turn_id"];
    let inherited: Vec<Value> = current_messages(source).into_iter()
        .filter(|message| matches!(message["role"].as_str(), Some("user" | "assistant")))
        .map(|message| {
            let mut row = crate::conversation_view::select(&message, &["role", "text", "attachments", "status", "created_at"]);
            row["message_id"] = json!(id());
            row["turn_id"] = Value::Null;
            row["source_message_id"] = message["message_id"].clone();
            row["source_thread_id"] = source["thread"]["thread_id"].clone();
            row["fork_supplement"] = json!(!message["message_id"].as_str().is_some_and(|id| fork_sources.contains(id)));
            row["in_progress"] = json!(running.is_string() && message["turn_id"] == *running);
            row
        }).collect();
    let count = inherited.len();
    fork["side_chat_inherited"] = json!(inherited);
    fork["turns"] = json!([]);
    fork["artifacts"] = json!([]);
    fork["thread"]["title"] = json!("侧边聊天");
    fork["thread"]["side_chat"] = json!({
        "parent_thread_id": source["thread"]["thread_id"],
        "from_turn_id": fork["thread"]["from_turn_id"],
        "inherited_messages": count,
        "parent_running": source["thread"]["state"]["running_turn_id"].is_string(),
        "created_at": now(),
    });
    fork
}
/// Side chats belong to their parent and are removed with it.
fn purge_side_chats(
    s: &Arc<AppState>,
    active: &mut HashMap<String, tokio::task::AbortHandle>,
    parent: &str,
) -> Result<Vec<crate::native_chat::Post>, Failure> {
    purge_side_chats_where(s, active, |side| side["parent_thread_id"] == parent)
}

fn purge_side_chats_where(
    s: &Arc<AppState>,
    active: &mut HashMap<String, tokio::task::AbortHandle>,
    owned: impl Fn(&Value) -> bool,
) -> Result<Vec<crate::native_chat::Post>, Failure> {
    purge_threads_where(s, active, |thread| {
        is_side_chat(thread) && owned(&thread["side_chat"])
    })
}

fn purge_threads_where(
    s: &Arc<AppState>,
    active: &mut HashMap<String, tokio::task::AbortHandle>,
    owned: impl Fn(&Value) -> bool,
) -> Result<Vec<crate::native_chat::Post>, Failure> {
    let mut ids = Vec::new();
    for summary in s.store.conversation_summaries().map_err(db)? {
        if !owned(&summary["thread"]) {
            continue;
        }
        let Some(id) = summary["thread"]["thread_id"].as_str() else { continue };
        let doc = s.store.get(COLLECTION, id).map_err(db)?;
        if s.store.purge_conversation(id).map_err(db)? {
            if let Some(doc) = &doc {
                crate::files::remove_unused_attachments(s, doc).map_err(|error| fail_code(StatusCode::INTERNAL_SERVER_ERROR,"conversation.attachment.cleanup_failed", &error))?;
            }
            ids.push(id.to_owned());
        }
    }
    Ok(release_deleted(s, active, &ids))
}

pub(crate) async fn purge_ctf_run_threads(s: &Arc<AppState>, run_id: &str) -> Result<(), String> {
    let expected_tid = format!("ctf-pi-{run_id}");
    let mut active = s.chat.active.lock().await;
    let posts = purge_threads_where(s, &mut active, |thread| {
        thread["ctf_run_id"].as_str() == Some(run_id)
            || thread["thread_id"].as_str() == Some(expected_tid.as_str())
    })
    .map_err(|(_, Json(error))| {
        error["error"]["message"]
            .as_str()
            .unwrap_or("CTF internal thread cleanup failed")
            .to_owned()
    })?;
    drop(active);
    for post in posts {
        tokio::spawn(post);
    }
    Ok(())
}
/// List rows carry the runtime that picks each conversation's engine mark.
fn listed_thread(summary: &Value) -> Value {
    let mut thread = summary["thread"].clone();
    if thread.is_object() && summary["runtime"]["adapter_id"].is_string() {
        thread["runtime"] = summary["runtime"].clone();
    }
    thread
}

fn inbox_thread(summary: &Value) -> Value {
    let mut thread = listed_thread(summary);
    if let Some(state) = thread.get_mut("state").and_then(Value::as_object_mut) {
        state.remove("subagent_report");
        if let Some(nodes) = state.get_mut("subagents").and_then(Value::as_array_mut) {
            for node in nodes {
                *node = crate::conversation_view::select(node, &["agent_id", "thread_id", "status", "origin"]);
            }
        }
    }
    thread
}

/// Workspace activity needs lineage, queue and attention state. Keep the
/// full report, session, and runtime fields on the standard thread list.
fn workspace_overview_thread(thread: &Value) -> Value {
    use crate::conversation_view::select;

    let mut row = select(
        thread,
        &[
            "thread_id",
            "title",
            "mode",
            "created_at",
            "updated_at",
            "control_lifecycle",
        ],
    );
    if let Some(state) = thread.get("state") {
        let mut overview_state = select(
            state,
            &[
                "status",
                "running_turn_id",
                "queued_turn",
                "queue_count",
                "queue_paused",
                "queue_failed_item_id",
                "quota_resume",
                "pending_approval",
                "pending_approvals",
                "pending_user_input",
                "unread",
                "lineage",
            ],
        );
        if let Some(subagents) = state["subagents"].as_array() {
            overview_state["subagents"] = Value::Array(
                subagents
                    .iter()
                    .map(|node| select(node, &["agent_id", "thread_id", "title", "status", "origin"]))
                    .collect(),
            );
        }
        row["state"] = overview_state;
    }
    row
}

pub(crate) fn turn_process(s: &AppState, tid: &str, params: &Value) -> Result<Value, Failure> {
    let turn_id = params["turn_id"].as_str().map(str::trim).unwrap_or("");
    let doc = s
        .store
        .get(COLLECTION, tid)
        .map_err(db)?
        .ok_or_else(|| fail_code(StatusCode::NOT_FOUND, "conversation.turn.not_found", "Thread or turn not found"))?;
    let turn = doc["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["turn_id"] == turn_id)
        .cloned()
        .ok_or_else(|| fail_code(StatusCode::NOT_FOUND, "conversation.turn.not_found", "Thread or turn not found"))?;
    if turn["status"] == "superseded" {
        return Err(fail_code(StatusCode::NOT_FOUND, "conversation.turn.not_found", "Turn is superseded"));
    }
    let parse_u64 = |key: &str, default: Option<u64>| -> Result<Option<u64>, Failure> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| {
                fail_code(StatusCode::BAD_REQUEST, "conversation.query.invalid", "Invalid pagination parameter")
            }),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(default),
            Some(Value::String(s)) => s.trim().parse::<u64>().map(Some).map_err(|_| {
                fail_code(StatusCode::BAD_REQUEST, "conversation.query.invalid", "Invalid pagination parameter")
            }),
            Some(_) => Err(fail_code(StatusCode::BAD_REQUEST, "conversation.query.invalid", "Invalid pagination parameter")),
        }
    };
    let limit = parse_u64("limit", Some(2000))?.unwrap_or(2000).clamp(1, 2000) as usize;
    let after = parse_u64("after_seq", Some(0))?.unwrap_or(0);
    let requested = parse_u64("watermark", None)?;
    let head = doc["event_seq"].as_u64().unwrap_or(0);
    let watermark = requested.unwrap_or(head).min(head);
    if after > watermark || requested.is_some_and(|v| v > head) {
        return Err(fail_code(
            StatusCode::BAD_REQUEST,
            "conversation.query.invalid",
            "Cursor is outside the recorded stream",
        ));
    }
    let mut events: Vec<Value> = s
        .store
        .chat_events(tid, Some(turn_id), after, watermark, limit + 1)
        .map_err(db)?
        .into_iter()
        .map(|ev| {
            let seq = ev["stream_seq"].as_u64().unwrap_or(0);
            json!({
                "event_id": ev["event_id"],
                "event_type": ev["event_type"],
                "occurred_at": ev["occurred_at"],
                "seq": seq,
                "stream_seq": seq,
                "payload": ev["payload"],
            })
        })
        .collect();
    let more = events.len() > limit;
    events.truncate(limit);
    let next = if more {
        events.last().and_then(|v| v["stream_seq"].as_u64())
    } else {
        None
    };
    let pub_turn = crate::conversation_view::select(
        &turn,
        &[
            "turn_id",
            "seq",
            "kind",
            "status",
            "original_status",
            "text",
            "interaction_mode",
            "created_at",
            "completed_at",
            "usage",
            "context_delivery",
            "historical",
        ],
    );
    Ok(json!({
        "thread_id": tid,
        "turn_id": turn_id,
        "turn": pub_turn,
        "count": events.len(),
        "events": events,
        "truncated": more,
        "has_more": more,
        "after_seq": after,
        "next_after_seq": next,
        "watermark": watermark,
        "recorded": doc["historical"] == true || turn["process_recorded"] == true,
    }))
}
pub(crate) async fn create(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Api {
    create_with(&s, body, |_| Ok(())).await
}

/// Creates a thread whose document `prepare` completes inside the same commit.
async fn create_with(s: &Arc<AppState>, body: Value, prepare: impl FnOnce(&mut Value) -> Result<(), Failure>) -> Api {
    let cid = command_id(&body)?;
    let idem = body["idempotency_key"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let _guard = s.chat.active.lock().await;
    let key = format!("create:{cid}");
    if let Some(old) = s.store.get("conversation_commands", &key).map_err(db)? {
        if old["request"] != body {
            return Err(fail_code(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "command_id was already used for a different request",
            ));
        }
        return Ok(Json(old["response"].clone()));
    }
    let idem_key = idem.as_ref().map(|k| format!("idem:create:{k}"));
    if let Some(ik) = idem_key.as_ref()
        && let Some(old) = s.store.get("conversation_commands", ik).map_err(db)?
    {
        if semantic_request(&old["request"]) != semantic_request(&body) {
            return Err(fail_code(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "idempotency_key was already used for a different request",
            ));
        }
        return Ok(Json(old["response"].clone()));
    }
    let (tid, mut v, response) = prepare_created_thread(s, &body, "conversation")?;
    prepare(&mut v)?;
    if s.store.get(COLLECTION, &tid).map_err(db)?.is_some() {
        return Err(fail(StatusCode::CONFLICT, "Thread already exists"));
    }
    let record = json!({"request":body,"response":response});
    let mut expected = vec![(COLLECTION, tid.as_str(), None), ("conversation_commands", key.as_str(), None)];
    let mut records = vec![("conversation_commands", key.as_str(), &record)];
    if let Some(ik) = idem_key.as_ref() {
        expected.push(("conversation_commands", ik.as_str(), None));
        records.push(("conversation_commands", ik.as_str(), &record));
    }
    let (events, watermark) = s
        .store
        .chat_commit_with_documents(&tid, &v, None, None, &expected, &records)
        .map_err(db)?
        .ok_or_else(|| fail(StatusCode::CONFLICT, "Thread already exists"))?;
    finish_save(&s, &tid, &mut v, events, watermark);
    Ok(Json(response))
}

pub(crate) fn prepare_created_thread(
    s: &AppState,
    body: &Value,
    mode: &str,
) -> Result<(String, Value, Value), Failure> {
    command_id(body)?;
    if !body.is_object() || body.get("runtime").is_some_and(|v| !v.is_object()) {
        return Err(fail(StatusCode::BAD_REQUEST, "Invalid thread request"));
    }
    let tid = body["thread_id"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(id);
    let time = now();
    let title = body["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("New conversation");
    let mut v = json!({"thread":{"thread_id":tid,"title":title,"title_source":body.get("title_source").unwrap_or(&json!("fallback")),"mode":mode,"project_id":body["project_id"],"workspace_id":body["workspace_id"],"created_at":time,"updated_at":time,"archived":false,"state":{"status":"idle","running_turn_id":null,"queue_count":0,"queue_paused":false,"queue_pause_reason":"","queue_revision":0,"queue_failed_item_id":null}},"queue":[],"runtime":body.get("runtime").unwrap_or(&json!({"adapter_id":"openai","instance_id":"text"})),"messages":[],"turns":[],"event_seq":0});
    if let Some(ctf_run_id) = body["ctf_run_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        v["thread"]["ctf_run_id"] = json!(ctf_run_id);
    }
    emit_command(
        &s,
        &tid,
        &mut v,
        "core.thread.created",
        json!({"thread_id":tid,"project_id":body["project_id"],"workspace_id":body["workspace_id"],"title":title,"title_source":body.get("title_source").unwrap_or(&json!("fallback")),"mode":mode,"runtime":body.get("runtime").unwrap_or(&json!({"adapter_id":"openai","instance_id":"text"}))}),
        &body,
    );
    let response = receipt(body, &tid, json!({})).0;
    Ok((tid, v, response))
}

/// Create a SRC-owned thread and its Task documents in the same SQLite commit.
pub(crate) async fn create_src_thread_with_records(
    s: &Arc<AppState>,
    body: &Value,
    expected: &[(&str, &str, Option<&Value>)],
    records: &[(&str, &str, &Value)],
) -> Result<bool, Failure> {
    crate::business::require(s,crate::business::Module::Chat).map_err(|e|e.failure())?;
    let _guard = s.chat.active.lock().await;
    let (tid, mut thread, response) = prepare_created_thread(s, body, "src")?;
    let key = format!("create:{}", command_id(body)?);
    let command = json!({"request":body,"response":response});
    let mut all_expected = expected.to_vec();
    all_expected.push((COLLECTION, &tid, None));
    all_expected.push(("conversation_commands", &key, None));
    let mut all_records = records.to_vec();
    all_records.push(("conversation_commands", &key, &command));
    let Some((events, watermark)) = s
        .store
        .chat_commit_with_documents(&tid, &thread, None, None, &all_expected, &all_records)
        .map_err(db)?
    else {
        return Ok(false);
    };
    finish_save(s, &tid, &mut thread, events, watermark);
    Ok(true)
}
pub(crate) async fn get_thread(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Api {
    let (limit, before, after) = message_query(&query, "messages_limit", true)?;
    let mark_read = query.get("mark_read").is_none_or(|value| value == "true" || value == "1");
    let timeline = before.is_none() && after.is_none() && query.get("view").is_some_and(|view| view == "timeline");
    if timeline && !mark_read {
        let v = load_timeline(&s, &id).await?;
        return Ok(Json(view(&s, &v, Some(limit))));
    }
    let guard = s.chat.active.lock().await;
    let mut v = load(&s, &id)?;
    if mark_read {
        let head = stream_head(&v);
        let previous = v["thread"]["state"]["read_stream_seq"].as_u64().unwrap_or(0);
        let through = query.get("read_through").map(|value| {
            value.parse::<u64>().map_err(|_| fail(StatusCode::BAD_REQUEST, "Invalid read cursor"))
        }).transpose()?;
        let read = through.map_or(head, |seq| seq.min(head).max(previous));
        let unread = read < head;
        if v["thread"]["state"]["unread"] != unread
            || v["thread"]["state"]["read_stream_seq"] != read
            || v["thread"]["state"]["head_stream_seq"] != head
        {
            v["thread"]["state"]["unread"] = json!(unread);
            v["thread"]["state"]["head_stream_seq"] = json!(head);
            v["thread"]["state"]["read_stream_seq"] = json!(read);
            save(&s, &id, &mut v)?;
        }
    }
    drop(guard);
    if query.get("view").is_some_and(|view| view == "read-receipt") {
        return Ok(Json(json!({"unread":v["thread"]["state"]["unread"]})));
    }
    if timeline {
        return Ok(Json(view(&s, &v, Some(limit))));
    }
    let mut result = view(&s, &v, None);
    crate::conversation_view::page(&mut result, limit, before, after, true);
    Ok(Json(result))
}
fn message_query(
    query: &HashMap<String, String>,
    limit_key: &str,
    full_allowed: bool,
) -> Result<(usize, Option<i64>, Option<i64>), (StatusCode, Json<Value>)> {
    let raw = query.get(limit_key).map(|value| value.trim()).unwrap_or("");
    let parsed = match raw.to_ascii_lowercase().as_str() {
        "" => 50,
        "all" | "full" => 5000,
        _ => raw
            .parse::<u64>()
            .map_err(|_| fail(StatusCode::BAD_REQUEST, "Invalid message limit"))?
            .min(5000) as usize,
    };
    let parsed = if parsed == 0 { 5000 } else { parsed };
    let limit = if full_allowed && parsed >= 5000 {
        5000
    } else {
        parsed.min(500)
    };
    let cursor = |key: &str| -> Result<Option<i64>, (StatusCode, Json<Value>)> {
        query
            .get(key)
            .map(|value| {
                value
                    .parse::<i64>()
                    .map_err(|_| fail(StatusCode::BAD_REQUEST, "Invalid message cursor"))
            })
            .transpose()
    };
    let before = cursor("before_stream_seq")?;
    let after = cursor("after_stream_seq")?;
    if before.is_some() && after.is_some() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "before_stream_seq and after_stream_seq are mutually exclusive",
        ));
    }
    Ok((limit, before, after))
}
pub(crate) async fn messages(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Api {
    let (limit, before, after) = message_query(&query, "limit", false)?;
    let _guard = s.chat.active.lock().await;
    let mut v = view(&s, &load(&s, &id)?, None);
    let around = query
        .get("around_message_id")
        .map(|id| id.trim())
        .unwrap_or("");
    if around.is_empty() {
        crate::conversation_view::page(&mut v, limit, before, after, false);
    } else {
        if before.is_some() || after.is_some() {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "around_message_id cannot combine with before/after cursors",
            ));
        }
        let historical_messages = v["superseded_messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        v["messages"]
            .as_array_mut()
            .unwrap()
            .extend(historical_messages);
        let historical_turns = v["superseded_turns"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        v["turns"].as_array_mut().unwrap().extend(historical_turns);
        let seq = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["message_id"] == around)
            .and_then(|message| message["stream_seq"].as_i64())
            .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "Unknown message"))?;
        let mut older = v.clone();
        crate::conversation_view::page(
            &mut older,
            (limit / 2).max(1),
            Some(seq.saturating_add(1)),
            None,
            false,
        );
        let older_messages = older["messages"].as_array().unwrap();
        let mut newer = v.clone();
        crate::conversation_view::page(
            &mut newer,
            limit.saturating_sub(older_messages.len()).max(1),
            None,
            Some(seq),
            false,
        );
        let mut rows = older_messages.clone();
        rows.extend(newer["messages"].as_array().unwrap().iter().cloned());
        let turn_ids: std::collections::HashSet<_> = rows
            .iter()
            .filter_map(|message| message["turn_id"].as_str())
            .collect();
        v["turns"] = json!(
            v["turns"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|turn| turn["turn_id"]
                    .as_str()
                    .is_some_and(|id| turn_ids.contains(id)))
                .cloned()
                .collect::<Vec<_>>()
        );
        let oldest = rows.first().cloned().unwrap_or(Value::Null);
        let newest = rows.last().cloned().unwrap_or(Value::Null);
        v["messages_page"] = json!({"limit":limit,"around_message_id":around,
            "has_more_before":older["messages_page"]["has_more_before"],"has_more_after":newer["messages_page"]["has_more_after"],
            "oldest_stream_seq":oldest["stream_seq"],"newest_stream_seq":newest["stream_seq"],
            "oldest_message_id":oldest["message_id"],"newest_message_id":newest["message_id"]});
        v["messages"] = json!(rows);
    }
    Ok(Json(
        json!({"messages":v["messages"],"turns":v["turns"],"messages_page":v["messages_page"]}),
    ))
}
pub(crate) fn forget_deleted_child(s:&AppState,id:&str)->Result<(),Failure>{
    for mut parent in s.store.list(COLLECTION).map_err(db)? {
        let mut changed=false;
        for field in ["subagents","agents"] {
            let rows=if field=="agents"{&mut parent["thread"]["state"]["agents"]["agents"]}else{&mut parent["thread"]["state"][field]};
            if let Some(rows)=rows.as_array_mut(){let before=rows.len();rows.retain(|row|row["thread_id"]!=id && row["agent_id"]!=id);changed|=rows.len()!=before;}
        }
        if changed {let parent_id=parent["thread"]["thread_id"].as_str().unwrap().to_owned();emit(s,&parent_id,&mut parent,"core.subagent.removed",json!({"thread_id":id}));save(s,&parent_id,&mut parent)?;}
    }
    Ok(())
}

async fn delete_thread(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Api {
    let mut active = s.chat.active.lock().await;
    let deleted = load(&s, &id)?;
    let running_turn = deleted["thread"]["state"]["running_turn_id"].as_str().map(str::to_owned);
    if !s.store.purge_conversation(&id).map_err(db)? {
        return Err(fail(StatusCode::NOT_FOUND, "Thread not found"));
    }
    if let Some(task) = active.remove(&id) {
        task.abort();
    }
    forget_deleted_child(&s,&id)?;
    crate::files::remove_unused_attachments(&s, &deleted).map_err(|error| fail_code(StatusCode::INTERNAL_SERVER_ERROR,"conversation.attachment.cleanup_failed", &error))?;
    notify_deleted(&s, &id);
    let side_posts = purge_side_chats(&s, &mut active, &id)?;
    drop(active);
    for post in side_posts {
        tokio::spawn(post);
    }
    if let Some(turn) = running_turn {
        s.computer.abort_turn(&id, &turn).await;
    }
    crate::native_chat::close(&s, &id, "thread_deleted").await;
    Ok(Json(json!({"deleted":true})))
}
fn event_cursor(
    query: &HashMap<String, String>,
    headers: &HeaderMap,
) -> Result<(u64, bool, u64), (StatusCode, Json<Value>)> {
    let invalid = || fail(StatusCode::BAD_REQUEST, "Invalid conversation event cursor");
    let after = query
        .get("after")
        .map(|v| v.parse::<u64>())
        .transpose()
        .map_err(|_| invalid())?
        .unwrap_or(0);
    let tail = query
        .get("tail")
        .map(|v| v.parse::<u64>())
        .transpose()
        .map_err(|_| invalid())?
        .unwrap_or(0);
    let snapshot = match query.get("snapshot").map(String::as_str) {
        None | Some("true" | "1") => true,
        Some("false" | "0") => false,
        _ => return Err(invalid()),
    };
    let header = headers
        .get("last-event-id")
        .map(|v| v.to_str().map_err(|_| invalid()))
        .transpose()?
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.trim().parse::<u64>().map_err(|_| invalid()))
        .transpose()?;
    Ok((
        if snapshot {
            after
        } else {
            header.unwrap_or(after)
        },
        snapshot,
        tail,
    ))
}

async fn events(
    State(s): State<Arc<AppState>>,
    Path(tid): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let (mut cursor, snapshot, tail) = match event_cursor(&query, &headers) {
        Ok(options) => options,
        Err(error) => return error.into_response(),
    };
    let timeline = !snapshot && query.get("view").is_some_and(|view| view == "timeline");
    let receiver = s.chat.updates.subscribe();
    let initial = if timeline {
        match s.store.conversation_exists(&tid) {
            Ok(true) => None,
            Ok(false) => return fail_code(StatusCode::NOT_FOUND, "conversation.thread.not_found", "Thread not found").into_response(),
            Err(error) => return db(error).into_response(),
        }
    } else {
        let guard = s.chat.active.lock().await;
        let document = match load(&s, &tid) {
            Ok(v) => v,
            Err(error) => return error.into_response(),
        };
        if snapshot && cursor == 0 && tail > 0 {
            cursor = match s.store.chat_tail_cursor(
                &tid, tail, document["thread"]["state"]["running_turn_id"].as_str().unwrap_or(""),
            ) {
                Ok(cursor) => cursor,
                Err(error) => return db(error).into_response(),
            };
        }
        drop(guard);
        snapshot.then(|| stream_view(&s, &document))
    };
    let browser = query
        .get("browser_host")
        .and_then(|host| s.browser.subscribe(&tid, host));
    let stream = futures_util::stream::unfold(
        (
            initial,
            receiver,
            s,
            tid,
            cursor,
            VecDeque::<Value>::new(),
            browser,
        ),
        move |(mut initial, mut receiver, s, tid, mut cursor, mut pending, mut browser)| async move {
            if let Some(value) = initial.take() {
                return Some((
                    Ok::<_, Infallible>(Event::default().event("snapshot").data(value.to_string())),
                    (initial, receiver, s, tid, cursor, pending, browser),
                ));
            }
            loop {
                if let Some((_, rx)) = browser.as_mut() {
                    if let Ok(item) = rx.try_recv() {
                        return Some((
                            Ok(Event::default()
                                .event("browser_request")
                                .data(item.to_string())),
                            (None, receiver, s, tid, cursor, pending, browser),
                        ));
                    }
                }
                if pending.is_empty() {
                    pending = s
                        .store
                        .chat_events(&tid, None, cursor, i64::MAX as u64, 256)
                        .ok()?
                        .into();
                }
                if let Some(event) = pending.pop_front() {
                    cursor = event["seq"].as_u64()?;
                    return Some((
                        Ok(Event::default()
                            .id(cursor.to_string())
                            .event("event")
                            .data(event.to_string())),
                        (None, receiver, s, tid, cursor, pending, browser),
                    ));
                }
                let received = loop {
                    let next = if let Some((_, rx)) = browser.as_mut() {
                        tokio::select! {
                            item = rx.recv() => Ok(item),
                            received = receiver.recv() => Err(received),
                        }
                    } else {
                        Err(receiver.recv().await)
                    };
                    if let Err(Ok((thread, _, _))) = &next && thread != &tid {
                        continue;
                    }
                    break next;
                };
                let update = match received {
                    Ok(Some(item)) => Some(Event::default().event("browser_request").data(item.to_string())),
                    Ok(None) => { browser = None; None }
                    Err(Ok((thread, kind, value))) if thread == tid && kind == "snapshot" => {
                        if timeline {
                            (value == true).then(|| Event::default().event("changed").data("null"))
                        } else {
                            match s.store.get(COLLECTION, &tid) {
                                Ok(Some(document)) => Some(Event::default().event("snapshot").data(stream_view(&s, &document).to_string())),
                                Ok(None) => None,
                                Err(error) => {
                                    tracing::error!(%error, thread_id = %tid, "Could not refresh conversation stream snapshot");
                                    return None;
                                }
                            }
                        }
                    }
                    Err(Ok((thread, kind, value))) if thread == tid && kind != "event" => {
                        Some(Event::default().event(kind).data(value.to_string()))
                    }
                    Err(Err(broadcast::error::RecvError::Lagged(_))) if timeline => {
                        Some(Event::default().event("changed").data("null"))
                    }
                    Err(Err(broadcast::error::RecvError::Closed)) => return None,
                    _ => None,
                };
                if let Some(update) = update {
                    return Some((Ok(update), (None, receiver, s, tid, cursor, pending, browser)));
                }
            }
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
pub(crate) async fn command(
    State(s): State<Arc<AppState>>,
    Path(tid): Path<String>,
    Json(body): Json<Value>,
) -> Api {
    command_inner(s, tid, body, None).await
}

/// The generic CommandAPI uses the same typed chat mutation, with a durable
/// snapshot comparison at commit so two server processes cannot lose a write.
pub(crate) async fn command_from_platform(
    s: Arc<AppState>,
    tid: String,
    body: Value,
    expected_version: Option<u64>,
) -> Api {
    command_inner(s, tid, body, Some(expected_version)).await
}

async fn command_inner(
    s: Arc<AppState>,
    tid: String,
    body: Value,
    platform_expected: Option<Option<u64>>,
) -> Api {
    if !crate::business::safety_command(body["command_type"].as_str().unwrap_or("")){
        crate::business::require(&s,crate::business::Module::Chat).map_err(|e|e.failure())?;
    }
    if crate::chat_subagents::supports_command(body["command_type"].as_str().unwrap_or("")) {
        return crate::chat_subagents::command(s, tid, body, platform_expected).await;
    }
    let cid = command_id(&body)?;
    let idem = body["idempotency_key"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let mut active = s.chat.active.lock().await;
    let key = format!("thread:{tid}:{cid}");
    if let Some(old) = s.store.get("conversation_commands", &key).map_err(db)? {
        if old["request"] != body {
            return Err(fail_code(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "command_id was already used for a different request",
            ));
        }
        return Ok(Json(old["response"].clone()));
    }
    let idem_key = idem.as_ref().map(|k| format!("idem:thread:{tid}:{k}"));
    if let Some(ik) = idem_key.as_ref()
        && let Some(old) = s.store.get("conversation_commands", ik).map_err(db)?
    {
        if semantic_request(&old["request"]) != semantic_request(&body) {
            return Err(fail_code(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "idempotency_key was already used for a different request",
            ));
        }
        return Ok(Json(old["response"].clone()));
    }
    let mut v = load(&s, &tid)?;
    if v["thread"]["state"]["lineage"]["native_subagent"].is_string() && !matches!(body["command_type"].as_str(),Some("conversation.thread.archive"|"conversation.thread.unarchive"|"conversation.thread.rename"|"conversation.thread.delete")) {
        return Err(fail_code(StatusCode::CONFLICT,"conversation.native_mirror.read_only","Provider-native subagent transcripts are read-only."));
    }
    if v["thread"]["state"]["history_mutation"].is_object() {
        return Err(fail_code(
            StatusCode::CONFLICT,
            "conversation.history.busy",
            "正在回退聊天历史，请稍后重试",
        ));
    }
    let before = (platform_expected.is_some()
        || matches!(
            body["command_type"].as_str(),
            Some(
                "conversation.thread.rename"
                    | "conversation.thread.archive"
                    | "conversation.thread.unarchive"
            )
        ))
    .then(|| v.clone());
    let payload = &body["payload"];
    if !payload.is_object() {
        return Err(fail(StatusCode::BAD_REQUEST, "payload must be an object"));
    }
    let command_type = body["command_type"].as_str().unwrap_or("");
    if matches!(
        command_type,
        "conversation.thread.archive"
            | "conversation.thread.unarchive"
            | "conversation.thread.delete"
    ) && payload["internal_src_lifecycle"] != true
        && let Some(src_task_id) = bound_live_src_task(&s, &tid)?
    {
        return Err(fail_code(
            StatusCode::CONFLICT,
            "src.task_owns_thread",
            &format!("SRC 主对话归属研究任务 {src_task_id}，请通过 SRC 研究任务的归档/恢复/销毁管理生命周期"),
        ));
    }
    let mut output = json!({});
    let mut spawn = None;
    let mut deleted = false;
    let mut receipt_thread = tid.clone();
    let mut posts: Vec<crate::native_chat::Post> = Vec::new();
    let cascade_thread = tid.clone();
    let cascade_turn = v["thread"]["state"]["running_turn_id"].as_str().unwrap_or("").to_owned();
    let native = crate::native_chat::is_native(&v["runtime"]);
    let extension = v["extension_session"]["open"] == true
        && extension_chat::registered(
            &s,
            v["extension_session"]["adapter_id"].as_str().unwrap_or(""),
        );
    match command_type {
        "conversation.artifact.attach" => {
            use base64::Engine;
            let name = payload["name"].as_str().map(str::trim).unwrap_or("");
            if name.is_empty() {
                return Err(fail_code(
                    StatusCode::BAD_REQUEST,
                    "conversation.artifact.name_required",
                    "artifact.attach requires payload.name",
                ));
            }
            let bytes = if let Some(b64) = payload["content_base64"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|_| fail_code(StatusCode::BAD_REQUEST, "conversation.artifact.invalid", "Invalid content_base64"))?
            } else if let Some(text) = payload["content"].as_str().filter(|s| !s.is_empty()) {
                text.as_bytes().to_vec()
            } else if let Some(path) = payload["path"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                let expanded = if let Some(suffix) = path.strip_prefix("~/") {
                    std::env::var_os("HOME")
                        .map(|h| std::path::PathBuf::from(h).join(suffix))
                        .unwrap_or_else(|| std::path::PathBuf::from(path))
                } else {
                    std::path::PathBuf::from(path)
                };
                std::fs::read(&expanded)
                    .map_err(|e| fail_code(StatusCode::BAD_REQUEST, "conversation.artifact.invalid", &e.to_string()))?
            } else {
                return Err(fail_code(
                    StatusCode::BAD_REQUEST,
                    "conversation.artifact.invalid",
                    "artifact.attach requires content_base64, content, or path",
                ));
            };
            if bytes.is_empty() {
                return Err(fail_code(
                    StatusCode::BAD_REQUEST,
                    "conversation.artifact.invalid",
                    "Empty artifact content cannot be attached",
                ));
            }
            let kind = payload["kind"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("conversation.upload");
            let media_type = payload["media_type"].as_str().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("application/octet-stream");
            let run_id = payload["run_id"].as_str().map(str::trim).filter(|s| !s.is_empty());
            let meta = crate::files::store_agent_artifact(&s, name, kind, media_type, &bytes)
                .map_err(|e| fail_code(StatusCode::BAD_REQUEST, "conversation.artifact.invalid", &e))?;
            emit_command(&s, &tid, &mut v, "core.artifact.attach_requested", json!({"name": name}), &body);
            emit_command(
                &s,
                &tid,
                &mut v,
                "core.artifact.attached",
                json!({
                    "artifact_id": meta["artifact_id"],
                    "name": name,
                    "kind": kind,
                    "media_type": media_type,
                    "size": bytes.len(),
                    "run_id": run_id,
                }),
                &body,
            );
            let seq = stream_head(&v);
            let event_id = v["_pending_events"]
                .as_array()
                .and_then(|evs| evs.last())
                .and_then(|ev| ev["event_id"].as_str())
                .unwrap_or("")
                .to_owned();
            if !v["artifacts"].is_array() {
                v["artifacts"] = json!([]);
            }
            v["artifacts"].as_array_mut().unwrap().push(json!({
                "artifact_id": meta["artifact_id"],
                "name": name,
                "kind": kind,
                "media_type": media_type,
                "size": bytes.len(),
                "run_id": run_id,
                "turn_id": payload.get("turn_id").and_then(Value::as_str),
                "event_id": event_id,
                "stream_seq": seq,
                "created_at": meta["created_at"],
            }));
            output = json!({
                "artifact_id": meta["artifact_id"],
                "name": name,
                "kind": kind,
                "media_type": media_type,
                "size": bytes.len(),
            });
        }
        "conversation.memory.record" => {
            let (memory_id, seq, kind) = crate::memory_api::record_from_command(&s, &tid, payload, &body)?;
            emit_command(&s, &tid, &mut v, "core.memory.record_requested", json!({"memory_id": memory_id}), &body);
            emit_command(&s, &tid, &mut v, "core.memory.recorded", json!({"memory_id": memory_id, "kind": kind}), &body);
            output = json!({"memory_id": memory_id, "graph_stream_seq": seq});
        }
        "conversation.memory.delete" => {
            let (memory_id, seq) = crate::memory_api::delete_from_command(&s, &tid, payload, &body)?;
            emit_command(&s, &tid, &mut v, "core.memory.delete_requested", json!({"memory_id": memory_id}), &body);
            emit_command(&s, &tid, &mut v, "core.memory.deleted", json!({"memory_id": memory_id}), &body);
            output = json!({"memory_id": memory_id, "graph_stream_seq": seq});
        }
        "conversation.plan.inject" => {
            let time = now();
            let (ev_type, ev_body) = chat_core::inject_plan(&mut v["thread"]["state"], payload, &time)
                .map_err(|(code, msg)| fail_code(StatusCode::BAD_REQUEST, code, msg))?;
            emit_command(&s, &tid, &mut v, ev_type, ev_body, &body);
        }
        "conversation.plan.amend" => {
            let time = now();
            let (steer_text, overlay, ev_body) = chat_core::amend_plan(&mut v["thread"]["state"], payload, cid, &time)
                .map_err(|(code, msg)| fail_code(StatusCode::BAD_REQUEST, code, msg))?;
            emit_command(&s, &tid, &mut v, "core.plan.updated", ev_body, &body);
            let running = v["thread"]["state"]["running_turn_id"].as_str().unwrap_or("").to_owned();
            let client_msg_id = payload["client_message_id"].as_str().filter(|s| !s.is_empty()).unwrap_or(cid).to_owned();
            let mut steered = false;
            if !running.is_empty() && native {
                let steer_body = json!({
                    "command_id": body["command_id"],
                    "payload": {
                        "text": steer_text,
                        "expected_turn_id": running,
                        "client_message_id": client_msg_id,
                    }
                });
                if let Ok((_, post)) = crate::native_chat::steer(&s, &tid, &mut v, &steer_body, false).await {
                    posts.extend(post);
                    emit_command(
                        &s,
                        &tid,
                        &mut v,
                        "core.turn.steered",
                        json!({
                            "turn_id": running,
                            "text": steer_text,
                            "client_message_id": client_msg_id,
                            "via": "plan.amend",
                        }),
                        &body,
                    );
                    steered = true;
                }
            }
            output = json!({"pending_amendment": overlay, "steered": steered});
        }
        "conversation.agents.inject" => {
            let time = now();
            let events = chat_core::inject_agents(&mut v, payload, &time)
                .map_err(|(code, msg)| fail_code(StatusCode::BAD_REQUEST, code, msg))?;
            for (ev_type, ev_body) in events {
                emit_command(&s, &tid, &mut v, ev_type, ev_body, &body);
            }
        }
        "conversation.approval.inject" => {
            let time = now();
            let events = chat_core::inject_approvals(&mut v, payload, &time)
                .map_err(|(status, code, msg)| fail_code(status, code, &msg))?;
            for (ev_type, ev_body) in events {
                emit_command(&s, &tid, &mut v, ev_type, ev_body, &body);
            }
        }
        "conversation.user_input.inject" => {
            let time = now();
            let (events, result) = chat_core::inject_user_input(&mut v, payload, &s.chat.epoch, &time)
                .map_err(|(status, code, msg)| fail_code(status, code, &msg))?;
            for (ev_type, ev_body) in events {
                emit_command(&s, &tid, &mut v, ev_type, ev_body, &body);
            }
            output = result;
        }
        "conversation.turn.steer" | "conversation.queue.steer" if extension => {
            drop(active);
            return extension_chat::control(&s, &tid, &body, &key).await;
        }
        "conversation.turn.steer" | "conversation.queue.steer" if native => {
            let (result, post) = crate::native_chat::steer(
                &s,
                &tid,
                &mut v,
                &body,
                command_type == "conversation.queue.steer",
            )
            .await?;
            output = result;
            posts.extend(post);
        }
        "conversation.turn.steer" | "conversation.queue.steer" => {
            return Err(fail_code(
                StatusCode::BAD_REQUEST,
                "capability.unsupported",
                "In-flight steering is unavailable for this runtime; send a queued message instead",
            ));
        }
        "conversation.approval.resolve" if s.conversation_policy.managed_approval(&v,payload) => {
            let approval=s.conversation_policy.decide_managed(&s,&tid,&mut v,payload).map_err(|error|fail_code(error.status,error.code,&error.message))?;
            output=json!({"approval_id":payload["approval_id"],"status":"resolved","src_approval":approval});
        }
        "conversation.approval.resolve" | "conversation.user_input.resolve" if native => {
            let (result, post) = if command_type == "conversation.approval.resolve" {
                crate::native_chat::resolve_approval(&s, &tid, &mut v, payload).await?
            } else {
                crate::native_chat::resolve_user_input(&s, &tid, &mut v, payload).await?
            };
            output = result;
            posts.extend(post);
        }
        "conversation.approval.resolve" | "conversation.user_input.resolve" if extension => {
            drop(active);
            return extension_chat::control(&s, &tid, &body, &key).await;
        }
        "conversation.approval.resolve" => {
            return Err(fail_code(
                StatusCode::NOT_FOUND,
                "conversation.approval.not_found",
                "This runtime raises no approval requests",
            ));
        }
        "conversation.user_input.resolve" => {
            return Err(fail_code(
                StatusCode::NOT_FOUND,
                "conversation.user_input.not_found",
                "This runtime raises no user-input requests",
            ));
        }
        "conversation.thread.resume" | "conversation.turn.resume" => {
            let last = v["turns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|t| t["status"] != "superseded")
                .last();
            if !last.is_some_and(|t| {
                matches!(
                    t["status"].as_str(),
                    Some("failed" | "interrupted" | "cancelled")
                )
            }) {
                return Err(fail_code(
                    StatusCode::CONFLICT,
                    "conversation.turn.resume_not_available",
                    "只有失败或中断的最新 Turn 可以继续执行",
                ));
            }
            if v["thread"]["state"]["status"] == "running" || queue_len(&v) > 0 {
                return Err(fail_code(
                    StatusCode::CONFLICT,
                    "conversation.turn.running",
                    "当前仍有执行中或排队的消息",
                ));
            }
            let mut runtime = v["runtime"].clone();
            match payload.get("runtime") {
                None | Some(Value::Null) => {}
                Some(Value::Object(updates)) => {
                    for (key, value) in updates {
                        runtime[key] = value.clone();
                    }
                }
                Some(_) => return Err(fail(StatusCode::BAD_REQUEST, "runtime must be an object")),
            }
            let resume = json!({"payload":{"text":payload.get("text").cloned().unwrap_or(Value::Null),"runtime":runtime}});
            let (result, turn, dispatch) =
                prepare_turn(&s, &tid, &mut v, &resume, "conversation.thread.resume")?;
            output = result;
            spawn = Some((turn, dispatch));
        }
        "conversation.queue.update"
        | "conversation.queue.delete"
        | "conversation.queue.reorder"
        | "conversation.queue.pause"
        | "conversation.queue.resume" => {
            queue_command(&s, &tid, &mut v, command_type, payload)?;
            if command_type == "conversation.queue.resume"
                || (command_type == "conversation.queue.update"
                    && (payload["resume_after_edit"] == true || payload.get("not_before").is_some())) {
                spawn = promote_queue(&s, &tid, &mut v);
            }
        }
        "conversation.thread.quota_resume" => {
            output =
                crate::quota_resume::apply(&s, &tid, &mut v, payload, crate::quota_resume::now())?;
        }
        "conversation.thread.fork" => {
            require_keep_files(payload)?;
            if is_side_chat(&v["thread"]) {
                return Err(fail_code(StatusCode::CONFLICT, "conversation.side_chat.fork_forbidden",
                    "侧边聊天不能再分叉或开启侧边聊天"));
            }
            let side = payload["side_chat"] == true;
            if side && v["turns"].as_array().is_none_or(|turns| turns.iter().all(|t| t["status"] == "superseded")) {
                return Err(fail_code(StatusCode::CONFLICT, "conversation.side_chat.not_started",
                    "当前对话开始后才能使用侧边聊天，请先发送一条消息"));
            }
            let mut fork = fork_value(&v, payload)?;
            if side {
                fork = side_chat_value(fork, &v);
            }
            let mut restrictions = crate::control::ancestor_restrictions(&s, &tid).map_err(|error|fail_code(StatusCode::INTERNAL_SERVER_ERROR,"control.storage_failed",&error.to_string()))?;
            if body["actor"]["kind"] == "agent" {
                let caller = body["actor"]["id"].as_str().unwrap_or("");
                restrictions.extend(crate::control::ancestor_restrictions(&s, caller).map_err(|error|fail_code(StatusCode::INTERNAL_SERVER_ERROR,"control.storage_failed",&error.to_string()))?);
                fork["thread"]["started_by"] = json!({"thread_id":caller,"request_id":body["command_id"]});
            }
            fork["thread"]["control_ancestor_restrictions"] = json!(restrictions);
            receipt_thread = fork["thread"]["thread_id"].as_str().unwrap().to_owned();
            output = json!({"thread_id":receipt_thread,"source_thread_id":tid,"from_turn_id":fork["thread"]["from_turn_id"]});
            save(&s, &receipt_thread, &mut fork)?;
            deleted = true;
        }
        "conversation.turn.native_rewind" if native => {
            drop(active);
            return crate::native_chat::rewind(&s, &tid, &body, &key).await;
        }
        "conversation.background_task.stop" if native => {
            drop(active);
            return crate::native_chat::stop_background_task(&s, &tid, &body, &key).await;
        }
        "conversation.background_task.stop" => {
            return Err(fail_code(
                StatusCode::BAD_REQUEST,
                "capability.unsupported",
                "Background task control is unavailable for text chat",
            ));
        }
        "conversation.turn.native_rewind" => {
            return Err(fail_code(
                StatusCode::BAD_REQUEST,
                "capability.unsupported",
                "Native provider rewind is unavailable for text chat",
            ));
        }
        "conversation.thread.rename" if is_side_chat(&v["thread"]) && !run_side::owned_by_run(&v["thread"]) => {
            return Err(fail_code(StatusCode::CONFLICT, "conversation.side_chat.rename_forbidden",
                "侧边聊天不能重命名"));
        }
        "conversation.thread.rename" => {
            let title = payload["title"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    fail_code(
                        StatusCode::BAD_REQUEST,
                        "conversation.thread.title_required",
                        "thread.rename requires payload.title",
                    )
                })?;
            emit_command(
                &s,
                &tid,
                &mut v,
                "core.thread.rename_requested",
                json!({"thread_id":tid}),
                &body,
            );
            v["thread"]["title"] = json!(title);
            v["thread"]["title_source"] = json!("user");
            emit_command(
                &s,
                &tid,
                &mut v,
                "core.thread.renamed",
                json!({"thread_id":tid,"title":title}),
                &body,
            );
        }
        "conversation.thread.archive" => {
            if v["thread"]["state"]["status"] == "running" {
                if payload["close_running"] != true {
                    return Err(fail(StatusCode::CONFLICT, "A turn is running"));
                }
                if let Some(task) = active.remove(&tid) {
                    task.abort();
                }
                let turn = v["thread"]["state"]["running_turn_id"].clone();
                finish_value(&mut v, &turn, "cancelled", None);
                emit(&s, &tid, &mut v, "core.turn.interrupted",
                    json!({"turn_id":turn,"status":"cancelled"}));
            }
            let noop = v["thread"]["archived"] == true;
            emit_command(
                &s,
                &tid,
                &mut v,
                "core.thread.archive_requested",
                json!({"thread_id":tid,"noop":noop}),
                &body,
            );
            if queue_len(&v) > 0 {
                pause_queue(&mut v, payload["queue_pause_reason"].as_str().unwrap_or("thread_archived"));
            }
            v["thread"]["archived"] = json!(true);
            v["thread"]["state"]["status"] = json!("archived");
            crate::quota_resume::clear(&mut v);
            if v["native_session"]["open"] == true {
                v["native_session"]["open"] = json!(false);
                v["native_session"]["close_reason"] = json!("thread_archived");
            }
            posts.push(crate::native_chat::close(&s, &tid, "thread_archived"));
            if v["extension_session"]["open"] == true {
                let reference = v["extension_session"]["reference"].clone();
                v["extension_session"]["open"] = json!(false);
                posts.push(extension_chat::close(&s, &tid, &reference));
            }
            emit_command(
                &s,
                &tid,
                &mut v,
                "core.thread.archived",
                json!({"thread_id":tid,"noop":noop}),
                &body,
            );
        }
        "conversation.thread.unarchive" => {
            let noop = v["thread"]["archived"] != true;
            v["thread"]["archived"] = json!(false);
            if v["thread"]["state"]["status"] != "running" {
                v["thread"]["state"]["status"] = json!("idle");
            }
            emit_command(
                &s,
                &tid,
                &mut v,
                "core.thread.unarchived",
                json!({"thread_id":tid,"noop":noop}),
                &body,
            );
        }
        "conversation.thread.delete" => {
            if v["extension_session"]["open"] == true {
                posts.push(extension_chat::close(
                    &s,
                    &tid,
                    &v["extension_session"]["reference"],
                ));
            }
            if let Some(task) = active.remove(&tid) {
                task.abort();
            }
            s.store.purge_conversation(&tid).map_err(db)?;
            forget_deleted_child(&s,&tid)?;
            crate::files::remove_unused_attachments(&s, &v).map_err(|error| fail_code(StatusCode::INTERNAL_SERVER_ERROR,"conversation.attachment.cleanup_failed", &error))?;
            notify_deleted(&s, &tid);
            deleted = true;
            posts.push(crate::native_chat::close(&s, &tid, "thread_deleted"));
            posts.extend(purge_side_chats(&s, &mut active, &tid)?);
        }
        "conversation.turn.interrupt" if native => {
            let (result, post) = crate::native_chat::interrupt(&s, &tid, &mut v, payload).await?;
            output = result;
            posts.extend(post);
        }
        "conversation.turn.interrupt" if extension => {
            drop(active);
            let result = extension_chat::control(&s, &tid, &body, &key).await;
            if !cascade_turn.is_empty() {
                s.computer.abort_turn(&tid, &cascade_turn).await;
            }
            if v["thread"]["state"]["subagents"].as_array().is_some_and(|nodes| !nodes.is_empty()) {
                if let Err((_, Json(error))) = Box::pin(crate::chat_subagents::cascade(
                    s.clone(), tid.clone(), command_type.to_owned(), cascade_turn)).await {
                    tracing::warn!(error=%error, "Could not cascade parent conversation stop to subagents");
                }
            }
            return result;
        }
        "conversation.turn.interrupt" => {
            if let Some(task) = active.remove(&tid) {
                task.abort();
            }
            if v["thread"]["state"]["status"] == "running" {
                let turn = v["thread"]["state"]["running_turn_id"].clone();
                finish_value(&mut v, &turn, "cancelled", None);
                if queue_len(&v) > 0 {
                    pause_queue(&mut v, "turn_interrupted");
                }
                emit(
                    &s,
                    &tid,
                    &mut v,
                    "core.turn.interrupted",
                    json!({"turn_id":turn,"status":"cancelled"}),
                );
            }
        }
        "conversation.turn.send" | "conversation.turn.retry" | "conversation.turn.edit_resend" => {
            if command_type == "conversation.turn.send"
                && v["thread"]["state"]["status"] == "running"
                && bare_compact(payload)
            {
                return Err(fail_code(
                    StatusCode::CONFLICT,
                    "conversation.compact.busy",
                    "当前回合仍在执行，不能压缩",
                ));
            }
            if command_type == "conversation.turn.send"
                && (v["thread"]["state"]["status"] == "running"
                    || v["thread"]["state"]["queue_paused"] == true
                    || queue_len(&v) > 0
                    || payload["not_before"].is_number())
            {
                output = enqueue(&s, &tid, &mut v, &body)?;
            } else {
                if command_type != "conversation.turn.send"
                    && replacement_file_mode(payload)? == "restore_files"
                {
                    prepare_turn(&s, &tid, &mut v.clone(), &body, command_type)?;
                    let target = payload["turn_id"].as_str().unwrap_or("");
                    crate::checkpoints::restore(&workspace_root(&s, &v), &tid, target)
                        .await
                        .map_err(|error| {
                            fail_code(StatusCode::CONFLICT, error.code, &error.message)
                        })?;
                }
                let (result, turn, dispatch) = prepare_turn(&s, &tid, &mut v, &body, command_type)?;
                output = result;
                spawn = Some((turn, dispatch));
            }
        }
        _ => {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "Unsupported conversation command",
            ));
        }
    }
    let control_update = crate::control::reactivate_message(&s, &tid, &mut v, &body)?;
    v["thread"]["updated_at"] = json!(now());
    let mut response = receipt(&body, &receipt_thread, output);
    if native && response.0["receipt"]["output"]["src_approval"].is_null() && matches!(command_type,"conversation.turn.steer"|"conversation.queue.steer"|"conversation.approval.resolve"|"conversation.user_input.resolve") {
        let request=response.0["receipt"]["output"]["request_id"].as_str().or_else(||response.0["receipt"]["output"]["approval_id"].as_str()).unwrap_or("").to_owned();
        let operation=if command_type=="conversation.approval.resolve"{"approval"}else if command_type=="conversation.user_input.resolve"{"user_input"}else{"steer"};
        response.0["receipt"]["state"]=json!("processing");
        if !v["native_controls"].is_object(){v["native_controls"]=json!({});}
        v["native_controls"][&request]=json!({"request_id":request,"operation":operation,"command_id":body["command_id"],"key":key,"idem_key":idem_key,"epoch":s.chat.epoch(),"agent_session_id":v["native_session"]["agent_session_id"],"state":"processing","created_at":now()});
    }
    let record = json!({"request":body,"response":response.0});
    if !deleted {
        let mut records = vec![("conversation_commands", key.as_str(), &record)];
        if let Some(control) = &control_update { records.push((crate::control::THREADS, tid.as_str(), control)); }
        if let Some(ik) = idem_key.as_ref() {
            records.push(("conversation_commands", ik.as_str(), &record));
        }
        let (events, watermark) = s
            .store
            .chat_commit_with_documents(
                &tid,
                &v,
                before.as_ref(),
                platform_expected.flatten(),
                &[],
                &records,
            )
            .map_err(db)?
            .ok_or_else(|| {
                fail_code(
                    StatusCode::CONFLICT,
                    "command.version_conflict",
                    "Thread changed while this command was being applied",
                )
            })?;
        finish_save(&s, &tid, &mut v, events, watermark);
    } else {
        s.store
            .put("conversation_commands", &key, &record)
            .map_err(db)?;
        if let Some(ik) = idem_key.as_ref() {
            s.store
                .put("conversation_commands", ik, &record)
                .map_err(db)?;
        }
    }
    if let Some((turn, dispatch)) = spawn {
        match dispatch {
            Dispatch::Extension(plan) => spawn_local(&s, &mut active, tid, turn, plan),
            Dispatch::Native(plan) => {
                crate::native_chat::spawn_dispatch(s.clone(), tid, turn, plan)
            }
        }
    }
    drop(active);
    if !cascade_turn.is_empty()
        && matches!(command_type, "conversation.turn.interrupt" | "conversation.thread.archive" | "conversation.thread.delete") {
        s.computer.abort_turn(&cascade_thread, &cascade_turn).await;
    }
    for post in posts {
        tokio::spawn(post);
    }
    if command_type == "conversation.turn.interrupt" {
        let _ = crate::control_pr::set_watches_paused(&s, &cascade_thread, true);
    }
    if matches!(command_type,"conversation.turn.interrupt" | "conversation.thread.archive")
        && v["thread"]["state"]["subagents"].as_array().is_some_and(|nodes|!nodes.is_empty())
    {
        if let Err((_, Json(body))) = Box::pin(crate::chat_subagents::cascade(
            s.clone(),cascade_thread,command_type.to_owned(),cascade_turn)).await {
            tracing::warn!(error=%body,"Could not cascade parent conversation stop to subagents");
        }
    }
    Ok(response)
}
/// Where an admitted turn runs: an extension adapter or a live engine session.
pub(crate) enum Dispatch {
    Native(crate::native_chat::TurnPlan),
    Extension(extension_chat::Plan),
}
pub(crate) fn resolve_runtime(s: &AppState, runtime: &Value) -> Result<Dispatch, Failure> {
    if let Some(plan) = extension_chat::plan(s, runtime)? {
        Ok(Dispatch::Extension(plan))
    } else if crate::native_chat::is_native(runtime) {
        crate::native_chat::plan(runtime)
            .map(Dispatch::Native)
            .map_err(|error| crate::native_chat::failure(&error))
    } else {
        Err(fail_code(
            StatusCode::BAD_REQUEST,
            "conversation.runtime.unsupported",
            "此对话的运行时已不再支持（HTTP 文本对话已移除）；请新建对话并选择已添加的 Agent",
        ))
    }
}

pub(crate) fn require_interaction_mode(
    s: &AppState,
    runtime: &Value,
    mode: &str,
) -> Result<(), Failure> {
    if !matches!(mode, "default" | "plan") {
        return Err(fail_code(
            StatusCode::BAD_REQUEST,
            "conversation.interaction_mode_invalid",
            "interaction_mode must be default or plan",
        ));
    }
    if mode == "default" {
        return Ok(());
    }
    let mut selected = runtime.clone();
    selected["interaction_mode"] = json!(mode);
    if crate::native_chat::is_native(&selected){return crate::native_chat::plan(&selected).map(|_|()).map_err(|err|crate::native_chat::failure(&err));}
    match extension_chat::plan(s, &selected) {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err((_, Json(body))) => {
            return Err(fail_code(
                StatusCode::BAD_REQUEST,
                body["error"]["code"]
                    .as_str()
                    .unwrap_or("conversation.interaction_mode_unsupported"),
                body["error"]["message"]
                    .as_str()
                    .unwrap_or("This runtime does not support plan mode"),
            ));
        }
    }
    Err(fail_code(
        StatusCode::BAD_REQUEST,
        "conversation.interaction_mode_unsupported",
        "This runtime does not support plan mode",
    ))
}
fn spawn_local(
    s: &Arc<AppState>,
    active: &mut HashMap<String, tokio::task::AbortHandle>,
    tid: String,
    turn: String,
    configured: extension_chat::Plan,
) {
    let st = s.clone();
    let sid = tid.clone();
    let task = tokio::spawn(async move {
        complete(st, sid, turn, configured).await;
    });
    active.insert(tid, task.abort_handle());
}
/// Start a turn admitted outside a command, such as one promoted from the queue.
pub(crate) async fn start(s: Arc<AppState>, tid: String, turn: String, dispatch: Dispatch) {
    match dispatch {
        Dispatch::Extension(plan) => {
            let mut active = s.chat.active.lock().await;
            spawn_local(&s, &mut active, tid, turn, plan);
        }
        Dispatch::Native(plan) => crate::native_chat::spawn_dispatch(s, tid, turn, plan),
    }
}
/// Store a completed command's receipt so a replay with the same id returns it.
pub(crate) fn finish_command(
    s: &AppState,
    key: &str,
    body: &Value,
    thread: &str,
    output: Value,
) -> Api {
    let response = receipt(body, thread, output);
    s.store
        .put(
            "conversation_commands",
            key,
            &json!({"request":body,"response":response.0}),
        )
        .map_err(db)?;
    Ok(response)
}
fn prepare_turn(
    s: &AppState,
    tid: &str,
    v: &mut Value,
    body: &Value,
    command_type: &str,
) -> Result<(Value, String, Dispatch), (StatusCode, Json<Value>)> {
    crate::control_subagents::stage_finished_report(v, tid);
    if command_type == "conversation.turn.send" { crate::control_subagents::open_for_message(v, body); }
    if v["thread"]["workspace_id"].as_str().is_some_and(|id| !id.is_empty()) {
        let root = crate::workspace_read_api::bound_root(s, tid)?;
        if s.chat.checkout_roots.lock().contains(&root) {
            return Err(fail_code(StatusCode::CONFLICT, "conversation.git.checkout_busy",
                "工作目录正在切换分支，请稍后重新发送"));
        }
    }
    let payload = &body["payload"];
    let replacement = matches!(
        command_type,
        "conversation.turn.retry" | "conversation.turn.edit_resend"
    );
    let resume = command_type == "conversation.thread.resume";
    let file_mode = if replacement {
        replacement_file_mode(payload)?
    } else {
        "keep_files"
    };
    let target = if replacement {
        let target_id = payload["turn_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "turn_id is required"))?;
        let target = v["turns"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["turn_id"] == target_id && t["status"] != "superseded")
            .cloned()
            .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Turn not found in current branch"))?;
        if !matches!(
            target["status"].as_str(),
            Some("completed" | "failed" | "interrupted" | "cancelled")
        ) {
            return Err(fail(
                StatusCode::CONFLICT,
                "Turn cannot be retried in its current state",
            ));
        }
        Some(target)
    } else {
        None
    };
    let mut input = payload.clone();
    if let Some(target) = &target {
        if command_type == "conversation.turn.edit_resend" && !input["text"].is_string() {
            return Err(fail(StatusCode::BAD_REQUEST, "edit_resend requires text"));
        }
        if input.get("text").is_none_or(Value::is_null) {
            input["text"] = target["text"].clone();
        }
        input["attachments"] = target
            .get("attachments")
            .cloned()
            .unwrap_or_else(|| json!([]));
        if input.get("capability_refs").is_none_or(Value::is_null) {
            input["capability_refs"] = target
                .get("capability_refs")
                .cloned()
                .unwrap_or_else(|| json!([]));
        }
    }
    let payload = &input;
    if payload
        .get("text")
        .is_some_and(|text| !text.is_string() && !text.is_null())
    {
        return Err(fail(StatusCode::BAD_REQUEST, "text must be a string"));
    }
    let text = match payload["text"].as_str().unwrap_or("") {
        empty if resume && empty.trim().is_empty() => "请继续此前未完成的工作。",
        text => text,
    };
    let attachments = match payload.get("attachments") {
        None => Vec::new(),
        Some(Value::Array(rows)) => rows
            .iter()
            .map(|row| {
                row.as_str().map(str::to_owned).ok_or_else(|| {
                    fail(
                        StatusCode::BAD_REQUEST,
                        "attachments must contain uploaded artifact references",
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "attachments must be an array",
            ));
        }
    };
    if text.trim().is_empty()
        && attachments.is_empty()
        && payload["capability_refs"].as_array().is_none_or(Vec::is_empty)
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "Message or attachment is required",
        ));
    }
    if v["thread"]["archived"] == true {
        return Err(fail(StatusCode::CONFLICT, "Thread is archived"));
    }
    if v["thread"]["state"]["status"] == "running" {
        return Err(fail(StatusCode::CONFLICT, "A turn is already running"));
    }
    if v["thread"]["state"]["history_mutation"].is_object() {
        return Err(fail_code(
            StatusCode::CONFLICT,
            "conversation.thread.history_busy",
            "对话历史正在回退，完成后再发送",
        ));
    }
    let files = attachments
        .iter()
        .map(|sha| crate::files::read_attachment(&s, &v, sha))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| fail(StatusCode::BAD_REQUEST, &e))?;
    let runtime = payload
        .get("runtime")
        .or_else(|| body.get("runtime"))
        .unwrap_or(&v["runtime"])
        .clone();
    if !runtime.is_object() {
        return Err(fail(StatusCode::BAD_REQUEST, "runtime must be an object"));
    }
    let runtime = if run_side::owned_by_run(&v["thread"]) {
        run_side::admit_turn_runtime(s, v, &runtime)?
    } else {
        runtime
    };
    let mut dispatch = resolve_runtime(s, &runtime)?;
    if let Dispatch::Native(plan) = &mut dispatch {
        plan.check_attachments(&files)
            .map_err(|error| crate::native_chat::failure(&error))?;
        if let Some(target) = &target {
            plan.replacement(v, target);
        }
    }
    if !matches!(&dispatch,Dispatch::Native(_)) {
        chat_core::attachment_content(text,files).map_err(|error|fail(StatusCode::BAD_REQUEST,&error))?;
    }
    let references = resolve_references(s, v, &dispatch, &payload["capability_refs"])?;
    let runtime_invocation=if matches!(&dispatch,Dispatch::Native(_)){let mut selected=v.clone();selected["runtime"]=runtime.clone();crate::native_chat::runtime_invocation(&selected,payload)?}else{None};
    v["runtime"] = runtime;
    let mut runtime_snapshot = v["runtime"].clone();
    match &dispatch {
        Dispatch::Extension(plan) => {
            runtime_snapshot["adapter_id"] = json!(plan.adapter_id);
            runtime_snapshot["instance_id"] = json!(plan.instance_id);
            runtime_snapshot["model"] = json!(plan.model);
            runtime_snapshot["access_mode"] = json!(plan.access_mode);
        }
        Dispatch::Native(plan) => {
            runtime_snapshot["adapter_id"] = json!(plan.adapter_id);
            runtime_snapshot["instance_id"] = json!(plan.instance_id);
            runtime_snapshot["model"] = json!(plan.model);
            runtime_snapshot["access_mode"] = json!(plan.access_mode);
        }
    }
    let turn = id();
    let mut superseded = Vec::new();
    let kind = if let Some(target) = &target {
        let mut cut = false;
        for old in v["turns"].as_array_mut().unwrap() {
            if old["turn_id"] == target["turn_id"] {
                cut = true;
            }
            if cut && old["status"] != "superseded" {
                superseded.push(old["turn_id"].clone());
                old["original_status"] = old["status"].clone();
                old["status"] = json!("superseded");
                old["superseded_by_turn_id"] = json!(turn);
            }
        }
        if text != target["text"].as_str().unwrap_or("") {
            "edit_resend"
        } else {
            "retry"
        }
    } else if resume {
        "resume"
    } else {
        "send"
    };
    let mut reference_context = references.parts();
    if run_side::owned_by_run(&v["thread"]) {
        reference_context.push(run_side::turn_part(s, tid, v)?);
    } else if is_side_chat(&v["thread"]) {
        reference_context.push(side_chat_part());
    }
    let msg = id();
    let seq = stream_head(v) + 1;
    let origin = match body["actor"]["kind"].as_str() { Some("agent")=>"agent",Some("system")=>"server",_=>"human" };
    v["messages"].as_array_mut().unwrap().push(json!({"message_id":msg,"turn_id":turn,"role":"user","origin":origin,"text":text,"attachments":attachments,"stream_seq":seq,"created_at":now()}));
    let seq = v["turns"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["seq"].as_u64())
        .max()
        .unwrap_or(v["turns"].as_array().unwrap().len() as u64)
        + 1;
    v["turns"].as_array_mut().unwrap().push(json!({"turn_id":turn,"seq":seq,"kind":kind,"status":"running","process_recorded":true,"command_id":body["command_id"],"client_message_id":payload["client_message_id"],"runtime_invocation":runtime_invocation,"text":text,"attachments":attachments,"runtime_snapshot":runtime_snapshot,"retry_of_turn_id":target.as_ref().map(|t| &t["turn_id"]),"capability_refs":references.selected,"reference_context":reference_context,"created_at":now()}));
    if replacement {
        v["thread"]["state"]["usage"] = json!({});
        emit(
            s,
            tid,
            v,
            if kind == "edit_resend" {
                "core.turn.edit_resent"
            } else {
                "core.turn.retried"
            },
            json!({"turn_id":turn,"replacement_turn_id":turn,"retry_of_turn_id":target.as_ref().map(|t| &t["turn_id"]),"superseded_turn_ids":superseded,"edited":kind == "edit_resend","workspace_policy":file_mode}),
        );
    }
    if resume {
        emit(
            s,
            tid,
            v,
            "core.thread.resumed",
            json!({"thread_id":tid,"turn_id":turn,"runtime_override":payload.get("runtime").cloned().unwrap_or(Value::Null)}),
        );
    }
    if let Some(row) = v["turns"].as_array_mut().and_then(|rows| rows.iter_mut().find(|row| row["turn_id"] == turn)) {
        row["started_at"] = json!(now());
    }
    v["thread"]["state"]["status"] = json!("running");
    v["runtime_epoch"] = json!(s.chat.epoch);
    v["thread"]["state"]["running_turn_id"] = json!(turn);
    v["thread"]["state"]["last_error"] = Value::Null;
    crate::quota_resume::clear(v);
    emit(
        s,
        tid,
        v,
        "core.turn.started",
        json!({"turn_id":turn,"runtime":runtime_snapshot}),
    );
    let mut output = json!({"thread_id":tid,"turn_id":turn});
    if replacement {
        output["impact"] = json!({"mode":kind,"superseded_turn_ids":superseded,"workspace_policy":file_mode,"external_side_effects":"cannot_undo"});
    }
    Ok((output, turn, dispatch))
}
/// Pin the workspace as it is before `turn` runs. The turn still runs when
/// this fails; only restoring its files is lost, and the thread is told so.
pub(crate) async fn capture_checkpoint(s: &AppState, tid: &str, turn: &str) {
    let root = match load(s, tid) {
        Ok(v) => workspace_root(s, &v),
        Err(_) => return,
    };
    if root.is_empty() {
        return;
    }
    let Err(error) = crate::checkpoints::capture(&root, tid, turn).await else {
        return;
    };
    tracing::warn!(thread = tid, turn, %error, "Turn checkpoint failed");
    let _guard = s.chat.active.lock().await;
    let Ok(mut v) = load(s, tid) else { return };
    emit(
        s,
        tid,
        &mut v,
        "core.runtime.warning",
        json!({"turn_id":turn,"severity":"warning","code":error.code,"message":format!("未能保存本轮开始前的文件检查点，之后无法回退这一轮的文件：{error}")}),
    );
    if let Err(error) = save(s, tid, &mut v) {
        tracing::error!(?error, "Could not record a checkpoint warning");
    }
}
pub(crate) fn workspace_root(s: &AppState, v: &Value) -> String {
    v["thread"]["workspace_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .and_then(|id| s.store.get("workspaces", id).ok().flatten())
        .and_then(|ws| ws["root_path"].as_str().map(str::to_owned))
        .or_else(|| v["thread"]["side_chat"]["cwd"].as_str().filter(|cwd| !cwd.is_empty()).map(str::to_owned))
        .unwrap_or_default()
}
/// Resolve the composer references a turn carries against the engine it will
/// run on, so a stale or foreign selection is refused before admission.
pub(crate) fn resolve_references(
    s: &AppState,
    v: &Value,
    dispatch: &Dispatch,
    refs: &Value,
) -> Result<crate::composer::Resolved, Failure> {
    let engine = match dispatch {
        Dispatch::Native(plan) => crate::agent_runtime::descriptors::for_adapter(&plan.adapter_id)
            .map(|(engine, _)| engine.engine().to_owned())
            .unwrap_or_default(),
        Dispatch::Extension(_) => String::new(),
    };
    crate::composer::resolve(s, &engine, &workspace_root(s, v), v, refs)
}
pub(crate) fn queue_len(v: &Value) -> usize {
    v["queue"].as_array().map_or(0, Vec::len)
}
pub(crate) fn queue_changed(v: &mut Value) {
    let count = queue_len(v);
    v["thread"]["state"]["queue_count"] = json!(count);
    let revision = v["thread"]["state"]["queue_revision"].as_u64().unwrap_or(0) + 1;
    v["thread"]["state"]["queue_revision"] = json!(revision);
    if let Some(items) = v["queue"].as_array_mut() {
        for (position, item) in items.iter_mut().enumerate() {
            item["position"] = json!(position);
        }
    }
}
pub(crate) fn pause_queue(v: &mut Value, reason: &str) {
    v["thread"]["state"]["queue_paused"] = json!(true);
    v["thread"]["state"]["queue_pause_reason"] = json!(reason);
    queue_changed(v);
}
fn queue_revision(v: &Value, payload: &Value) -> Result<(), (StatusCode, Json<Value>)> {
    if let Some(expected) = payload.get("expected_revision") {
        let expected = expected.as_u64().ok_or_else(|| {
            fail(
                StatusCode::BAD_REQUEST,
                "expected_revision must be a non-negative integer",
            )
        })?;
        if expected != v["thread"]["state"]["queue_revision"].as_u64().unwrap_or(0) {
            return Err(fail(
                StatusCode::CONFLICT,
                "Message queue changed; refresh and try again",
            ));
        }
    }
    Ok(())
}
/// `/compact` with no attachments. Attachments keep the normal send path.
fn bare_compact(payload: &Value) -> bool {
    payload
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.trim() == "/compact")
        && match payload.get("attachments") {
            None | Some(Value::Null) => true,
            Some(Value::Array(rows)) => rows.is_empty(),
            Some(_) => false,
        }
}
pub(crate) fn enqueue(
    s: &AppState,
    tid: &str,
    v: &mut Value,
    body: &Value,
) -> Result<Value, (StatusCode, Json<Value>)> {
    if v["thread"]["archived"] == true {
        return Err(fail(StatusCode::CONFLICT, "Thread is archived"));
    }
    let payload = &body["payload"];
    let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
    if payload
        .get("text")
        .is_some_and(|t| !t.is_string() && !t.is_null())
    {
        return Err(fail(StatusCode::BAD_REQUEST, "text must be a string"));
    }
    let attachments = payload
        .get("attachments")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let rows = attachments
        .as_array()
        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "attachments must be an array"))?;
    if text.trim().is_empty()
        && rows.is_empty()
        && payload["capability_refs"].as_array().is_none_or(Vec::is_empty)
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "Message or attachment is required",
        ));
    }
    let files = rows
        .iter()
        .map(|sha| {
            let sha = sha.as_str().ok_or_else(|| {
                fail(
                    StatusCode::BAD_REQUEST,
                    "attachments must contain uploaded artifact references",
                )
            })?;
            crate::files::read_attachment(s, v, sha).map_err(|e| fail(StatusCode::BAD_REQUEST, &e))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let runtime = payload
        .get("runtime")
        .or_else(|| body.get("runtime"))
        .unwrap_or(&v["runtime"])
        .clone();
    if !runtime.is_object() {
        return Err(fail(StatusCode::BAD_REQUEST, "runtime must be an object"));
    }
    let runtime = if run_side::owned_by_run(&v["thread"]) {
        run_side::admit_turn_runtime(s, v, &runtime)?
    } else {
        runtime
    };
    let dispatch = resolve_runtime(s, &runtime)?;
    if let Dispatch::Native(plan) = &dispatch {
        plan.check_attachments(&files)
            .map_err(|error| crate::native_chat::failure(&error))?;
    }
    if !matches!(&dispatch,Dispatch::Native(_)) {
        chat_core::attachment_content(text,files).map_err(|error|fail(StatusCode::BAD_REQUEST,&error))?;
    }
    let refs = payload
        .get("capability_refs")
        .cloned()
        .unwrap_or_else(|| json!([]));
    resolve_references(s, v, &dispatch, &refs).map_err(|(status, Json(mut body))| {
        body["error"]["code"] = json!("conversation.queue.reference_invalid");
        (status, Json(body))
    })?;
    if !v["queue"].is_array() {
        v["queue"] = json!([]);
    }
    let queue_id = next_queue_id(v);
    let item = json!({"queue_id":queue_id,"not_before":payload["not_before"],"control_scheduled":payload["control_scheduled"],"thread_id":tid,"origin":match body["actor"]["kind"].as_str(){Some("system")=>"server",Some("agent")=>"agent",_=>"human"},"actor":body["actor"],"client_message_id":payload.get("client_message_id").unwrap_or(&body["command_id"]),"command_id":body["command_id"],"text":text,"attachments":attachments,"capability_refs":refs,"runtime":runtime,"runtime_capability":payload["runtime_capability"],"runtime_command_arguments":payload["runtime_command_arguments"],"capability_revision":payload["capability_revision"],"position":queue_len(v),"status":"queued","created_at":now(),"updated_at":now()});
    v["queue"].as_array_mut().unwrap().push(item.clone());
    v["queue_epoch"] = json!(s.chat.epoch);
    queue_changed(v);
    let mut event = item;
    event["queue_revision"] = v["thread"]["state"]["queue_revision"].clone();
    emit(s, tid, v, "core.queue.added", event);
    Ok(json!({"thread_id":tid,"queue_id":queue_id,"queued":true}))
}
pub(crate) fn next_queue_id(doc:&mut Value)->String {
    let id=doc["next_queue_id"].as_u64().unwrap_or(0)+1;
    doc["next_queue_id"]=json!(id);
    id.to_string()
}

fn is_ready_human_burst_item(v: &Value, item: &Value, now_ts: i64) -> bool {
    item["origin"] == "human"
        && item["status"] == "queued"
        && item["runtime_capability"].is_null()
        && (item["runtime"].is_null() || item["runtime"] == v["runtime"])
        && item["not_before"].as_i64().is_none_or(|at| at <= now_ts)
        && !bare_compact(&json!({"text": item["text"], "attachments": item["attachments"]}))
}

fn next_ready_human_burst_index(v: &Value, now_ts: i64) -> Option<usize> {
    if v["thread"]["state"]["queue_paused"] == true || v["thread"]["archived"] == true {
        return None;
    }
    for (idx, item) in v["queue"].as_array()?.iter().enumerate() {
        if item["origin"] == "server" {
            continue;
        }
        return is_ready_human_burst_item(v, item, now_ts).then_some(idx);
    }
    None
}

pub(crate) fn ready_human_burst_count(v: &Value) -> usize {
    if v["thread"]["state"]["queue_paused"] == true || v["thread"]["archived"] == true {
        return 0;
    }
    let now_ts = chrono::Utc::now().timestamp();
    let mut count = 0usize;
    for item in v["queue"].as_array().into_iter().flatten() {
        if item["origin"] == "server" {
            continue;
        }
        if is_ready_human_burst_item(v, item, now_ts) {
            count += 1;
        } else {
            break;
        }
    }
    count
}

pub(crate) async fn wait_for_human_burst_settle(s: &AppState, tid: &str, turn: &str) {
    if !crate::runs::src_api::is_src_coordinator_thread(&s.store, tid) {
        return;
    }
    let should_debounce = {
        let _guard = s.chat.active.lock().await;
        load(s, tid).ok().is_some_and(|v| {
            v["thread"]["state"]["running_turn_id"] == turn
                && v["turns"]
                    .as_array()
                    .and_then(|rows| rows.iter().find(|r| r["turn_id"] == turn))
                    .is_some_and(|row| {
                        row["kind"] == "send"
                            && row["runtime_invocation"].is_null()
                            && row["text"].as_str().unwrap_or("").trim() != "/compact"
                    })
                && v["messages"]
                    .as_array()
                    .and_then(|msgs| msgs.iter().find(|m| m["turn_id"] == turn && m["role"] == "user"))
                    .is_some_and(|msg| msg["origin"] == "human")
        })
    };
    if !should_debounce {
        return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
    let mut prev_ready = 0usize;
    for _ in 0..3 {
        let cur_ready = {
            let _guard = s.chat.active.lock().await;
            load(s, tid)
                .ok()
                .filter(|v| v["thread"]["state"]["running_turn_id"] == turn)
                .map(|v| ready_human_burst_count(&v))
                .unwrap_or(0)
        };
        if cur_ready == 0 || cur_ready == prev_ready {
            break;
        }
        prev_ready = cur_ready;
        tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    }
}

pub(crate) fn absorb_queued_human_burst(
    s: &AppState,
    tid: &str,
    v: &mut Value,
    turn_id: &str,
    dispatch: &Dispatch,
) -> usize {
    if !crate::runs::src_api::is_src_coordinator_thread(&s.store, tid)
        || v["thread"]["state"]["queue_paused"] == true
        || v["thread"]["archived"] == true
    {
        return 0;
    }
    let is_human_send_turn = v["turns"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["turn_id"] == turn_id))
        .is_some_and(|row| row["kind"] == "send" && row["runtime_invocation"].is_null())
        && v["messages"]
            .as_array()
            .and_then(|rows| rows.iter().find(|msg| msg["turn_id"] == turn_id && msg["role"] == "user"))
            .is_some_and(|msg| msg["origin"] == "human");
    if !is_human_send_turn {
        return 0;
    }

    let mut absorbed = 0usize;
    while absorbed < 8 {
        let now_ts = chrono::Utc::now().timestamp();
        let Some(pos) = next_ready_human_burst_index(v, now_ts) else {
            break;
        };
        let Some(item) = v["queue"].as_array().and_then(|rows| rows.get(pos)).cloned() else {
            break;
        };
        let extra_text = item["text"].as_str().unwrap_or("");
        let extra_attachments: Vec<String> = match item["attachments"].as_array() {
            Some(rows) => match rows.iter().map(|r| r.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>() {
                Some(list) => list,
                None => break,
            },
            None => Vec::new(),
        };
        let Ok(files) = extra_attachments
            .iter()
            .map(|sha| crate::files::read_attachment(s, v, sha))
            .collect::<Result<Vec<_>, _>>()
        else {
            break;
        };
        if let Dispatch::Native(plan) = dispatch {
            if plan.check_attachments(&files).is_err() {
                break;
            }
        } else if chat_core::attachment_content(extra_text, files).is_err() {
            break;
        }
        let Ok(references) = resolve_references(s, v, dispatch, &item["capability_refs"]) else {
            break;
        };

        v["queue"].as_array_mut().unwrap().remove(pos);
        queue_changed(v);

        let msg_id = id();
        let seq = stream_head(v) + 1;
        v["messages"].as_array_mut().unwrap().push(json!({
            "message_id": msg_id,
            "turn_id": turn_id,
            "role": "user",
            "origin": "human",
            "text": extra_text,
            "attachments": extra_attachments,
            "stream_seq": seq,
            "created_at": now(),
        }));
        if let Some(turn_row) = v["turns"]
            .as_array_mut()
            .and_then(|rows| rows.iter_mut().find(|row| row["turn_id"] == turn_id))
        {
            let base_text = turn_row["text"].as_str().unwrap_or("");
            let merged_text = match (base_text.is_empty(), extra_text.is_empty()) {
                (true, _) => extra_text.to_owned(),
                (_, true) => base_text.to_owned(),
                (false, false) => format!("{base_text}\n{extra_text}"),
            };
            turn_row["text"] = json!(merged_text);
            if !turn_row["attachments"].is_array() {
                turn_row["attachments"] = json!([]);
            }
            turn_row["attachments"].as_array_mut().unwrap().extend(extra_attachments.into_iter().map(Value::String));
            let ref_parts = references.parts();
            if !turn_row["capability_refs"].is_array() {
                turn_row["capability_refs"] = json!([]);
            }
            turn_row["capability_refs"].as_array_mut().unwrap().extend(references.selected);
            if !turn_row["reference_context"].is_array() {
                turn_row["reference_context"] = json!([]);
            }
            turn_row["reference_context"].as_array_mut().unwrap().extend(ref_parts);
            if !turn_row["coalesced_queue_ids"].is_array() {
                turn_row["coalesced_queue_ids"] = json!([]);
            }
            turn_row["coalesced_queue_ids"].as_array_mut().unwrap().push(item["queue_id"].clone());
        }
        let event = json!({
            "queue_id": item["queue_id"],
            "turn_id": turn_id,
            "coalesced": true,
            "queue_revision": v["thread"]["state"]["queue_revision"],
        });
        emit(s, tid, v, "core.queue.dispatched", event);
        absorbed += 1;
    }
    absorbed
}

/// Promote only persisted requests explicitly submitted by the user. Admission
/// and provider validation finish under the short mutation lock; HTTP runs later.
pub(crate) fn promote_queue(s: &AppState, tid: &str, v: &mut Value) -> Option<(String, Dispatch)> {
    crate::control_subagents::stage_finished_report(v, tid);
    if queue_len(v) == 0
        || v["thread"]["state"]["queue_paused"] == true
        || v["thread"]["state"]["status"] == "running"
        || v["thread"]["archived"] == true
    {
        return None;
    }
    let index=v["queue"].as_array()?.iter().position(|item|item["not_before"].as_i64().is_none_or(|at|at<=chrono::Utc::now().timestamp()))?;
    let item = v["queue"][index].clone();
    let body = json!({"command_type":"conversation.turn.send","command_id":item["command_id"],"actor":if item["actor"].is_object(){item["actor"].clone()}else{json!({"kind":match item["origin"].as_str(){Some("agent")=>"agent",Some("server")=>"system",_=>"operator"}})},"payload":{"control_scheduled":item["control_scheduled"] == true || item["not_before"].is_number(),"client_message_id":item["client_message_id"],"text":item["text"],"attachments":item["attachments"],"capability_refs":item["capability_refs"],"runtime":item["runtime"],"runtime_capability":item["runtime_capability"],"runtime_command_arguments":item["runtime_command_arguments"],"capability_revision":item["capability_revision"]}});
    match prepare_turn(s, tid, v, &body, "conversation.turn.send") {
        Ok((_, turn, configured)) => {
            v["queue"].as_array_mut().unwrap().remove(index);
            queue_changed(v);
            v["thread"]["state"]["queue_failed_item_id"] = Value::Null;
            if let Some(row) = v["turns"].as_array_mut().unwrap().last_mut() {
                row["queue_id"] = item["queue_id"].clone();
            }
            let event = json!({"queue_id":item["queue_id"],"turn_id":turn,"queue_revision":v["thread"]["state"]["queue_revision"]});
            emit(s, tid, v, "core.queue.dispatched", event);
            if item["origin"] == "human" && item["runtime_capability"].is_null() {
                let _ = absorb_queued_human_burst(s, tid, v, &turn, &configured);
            }
            Some((turn, configured))
        }
        Err((_, error)) => {
            v["queue"][index]["status"] = json!("failed");
            v["queue"][index]["error"] = error.0["error"].clone();
            v["thread"]["state"]["queue_failed_item_id"] = item["queue_id"].clone();
            pause_queue(v, "dispatch_failed");
            let event = json!({"reason":"dispatch_failed","queue_id":item["queue_id"],"error":error.0["error"],"queue_revision":v["thread"]["state"]["queue_revision"]});
            emit(s, tid, v, "core.queue.paused", event);
            None
        }
    }
}
fn queue_command(
    s: &AppState,
    tid: &str,
    v: &mut Value,
    kind: &str,
    payload: &Value,
) -> Result<(), (StatusCode, Json<Value>)> {
    queue_revision(v, payload)?;
    let event;
    let mut data = json!({});
    match kind {
        "conversation.queue.pause" => {
            let reason = payload["reason"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("user_paused");
            if reason == "queue_editing" && v["thread"]["state"]["queue_paused"] == true {
                return Ok(());
            }
            pause_queue(v, reason);
            data["reason"] = json!(reason);
            event = "core.queue.paused";
        }
        "conversation.queue.resume" => {
            if let Some(reason) = payload["only_if_reason"].as_str()
                && v["thread"]["state"]["queue_pause_reason"] != reason {
                return Ok(());
            }
            if v["thread"]["archived"] == true {
                return Err(fail(StatusCode::CONFLICT, "Thread is archived"));
            }
            v["thread"]["state"]["queue_paused"] = json!(false);
            v["thread"]["state"]["queue_pause_reason"] = json!("");
            v["thread"]["state"]["queue_failed_item_id"] = Value::Null;
            queue_changed(v);
            event = "core.queue.resumed";
        }
        "conversation.queue.reorder" => {
            let ids = payload["queue_ids"]
                .as_array()
                .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "queue_ids must be an array"))?;
            let old = v["queue"].as_array().cloned().unwrap_or_default();
            let mut reordered = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for id in ids {
                let id = id.as_str().ok_or_else(|| {
                    fail(StatusCode::BAD_REQUEST, "queue_ids must contain strings")
                })?;
                if !seen.insert(id) {
                    return Err(fail(StatusCode::CONFLICT, "Duplicate queue item"));
                }
                reordered.push(
                    old.iter()
                        .find(|i| i["queue_id"] == id)
                        .cloned()
                        .ok_or_else(|| fail(StatusCode::CONFLICT, "Queue item not found"))?,
                );
            }
            if reordered.len() != old.len() {
                return Err(fail(
                    StatusCode::CONFLICT,
                    "Reorder must include every queue item",
                ));
            }
            v["queue"] = json!(reordered);
            queue_changed(v);
            data["queue_ids"] = json!(ids);
            event = "core.queue.reordered";
        }
        "conversation.queue.update" | "conversation.queue.delete" => {
            let index = v["queue"]
                .as_array()
                .into_iter()
                .flatten()
                .position(|i| {
                    payload["queue_id"].is_string() && i["queue_id"] == payload["queue_id"]
                })
                .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Queue item not found"))?;
            data["queue_id"] = payload["queue_id"].clone();
            if kind == "conversation.queue.delete" {
                v["queue"].as_array_mut().unwrap().remove(index);
                if v["thread"]["state"]["queue_failed_item_id"] == payload["queue_id"] {
                    v["thread"]["state"]["queue_failed_item_id"] = Value::Null;
                }
                event = "core.queue.deleted";
            } else {
                let mut item = v["queue"][index].clone();
                if let Some(text) = payload.get("text") {
                    let text = text
                        .as_str()
                        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "text must be a string"))?;
                    if text.trim().is_empty()
                        && item["attachments"].as_array().is_none_or(Vec::is_empty)
                    {
                        return Err(fail(
                            StatusCode::BAD_REQUEST,
                            "Message or attachment is required",
                        ));
                    }
                    item["text"] = json!(text);
                }
                if let Some(runtime) = payload.get("runtime") {
                    let updates = runtime.as_object().ok_or_else(|| {
                        fail(StatusCode::BAD_REQUEST, "runtime must be an object")
                    })?;
                    for (key, val) in updates {
                        item["runtime"][key] = val.clone();
                    }
                    resolve_runtime(s, &item["runtime"])?;
                }
                if let Some(not_before) = payload.get("not_before") {
                    if !not_before.is_null() && !not_before.is_u64() {
                        return Err(fail(
                            StatusCode::BAD_REQUEST,
                            "not_before must be a Unix timestamp or null",
                        ));
                    }
                    item["not_before"] = not_before.clone();
                }
                item["status"] = json!("queued");
                item["error"] = Value::Null;
                item["updated_at"] = json!(now());
                v["queue"][index] = item.clone();
                data = item;
                event = "core.queue.updated";
            }
            queue_changed(v);
        }
        _ => unreachable!(),
    }
    data["queue_revision"] = v["thread"]["state"]["queue_revision"].clone();
    emit(s, tid, v, event, data);
    if kind == "conversation.queue.update" && payload["resume_after_edit"] == true
        && v["thread"]["state"]["queue_pause_reason"] == "queue_editing" {
        v["thread"]["state"]["queue_paused"] = json!(false);
        v["thread"]["state"]["queue_pause_reason"] = json!("");
        queue_changed(v);
        emit(s, tid, v, "core.queue.resumed", json!({"reason":"edit_finished","queue_revision":v["thread"]["state"]["queue_revision"]}));
    }
    Ok(())
}

/// Retry and edit-resend may also bring the workspace back to the target
/// turn's checkpoint.
fn replacement_file_mode(payload: &Value) -> Result<&'static str, Failure> {
    match payload.get("file_mode") {
        None | Some(Value::Null) => Ok("keep_files"),
        Some(mode) if mode == "keep_files" => Ok("keep_files"),
        Some(mode) if mode == "restore_files" => Ok("restore_files"),
        Some(_) => Err(fail_code(
            StatusCode::BAD_REQUEST,
            "conversation.turn.invalid_file_mode",
            "file_mode 只能是 keep_files 或 restore_files",
        )),
    }
}
pub(crate) fn require_keep_files(payload: &Value) -> Result<(), (StatusCode, Json<Value>)> {
    if payload
        .get("file_mode")
        .is_some_and(|mode| mode != "keep_files")
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "Only keep_files is supported for text chat; file restore is unavailable",
        ));
    }
    Ok(())
}

/// Superseded rows remain stored for audit, but never enter a new HTTP prompt.
pub(crate) fn current_messages(v: &Value) -> Vec<Value> {
    let superseded: std::collections::HashSet<_> = v["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["status"] == "superseded")
        .filter_map(|t| t["turn_id"].as_str())
        .collect();
    v["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            !m["turn_id"]
                .as_str()
                .is_some_and(|id| superseded.contains(id))
        })
        .cloned()
        .collect()
}

/// Copy only local conversation records. Provider sessions and recorded tools
/// are deliberately not inherited or invoked by an ordinary text-chat branch.
fn fork_value(v: &Value, payload: &Value) -> Result<Value, (StatusCode, Json<Value>)> {
    let turns: Vec<_> = v["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["status"] != "superseded")
        .collect();
    let boundary = match payload.get("from_turn_id") {
        Some(Value::String(id)) if !id.trim().is_empty() => Some(
            turns
                .iter()
                .position(|t| t["turn_id"] == *id)
                .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Turn not found in current branch"))?,
        ),
        None | Some(Value::Null) | Some(Value::String(_)) => {
            turns.iter().rposition(|t| t["status"] == "completed")
        }
        _ => {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "from_turn_id must be a string",
            ));
        }
    };
    if boundary.is_some_and(|index| turns[index]["status"] == "running") {
        return Err(fail(StatusCode::CONFLICT, "Cannot fork a running turn"));
    }
    let source_turn = boundary.map(|index| turns[index]["turn_id"].clone());
    let tid = id();
    let time = now();
    let source_id = &v["thread"]["thread_id"];
    let title = match payload.get("title") {
        Some(Value::String(title)) if !title.trim().is_empty() => title.clone(),
        None | Some(Value::Null) | Some(Value::String(_)) => format!(
            "{}（fork）",
            v["thread"]["title"].as_str().unwrap_or("Conversation")
        ),
        _ => return Err(fail(StatusCode::BAD_REQUEST, "title must be a string")),
    };
    let mut copied_turns = Vec::new();
    let mut turn_ids = HashMap::new();
    for source in boundary.map(|index| &turns[..=index]).unwrap_or(&[]) {
        let turn = id();
        turn_ids.insert(
            source["turn_id"].as_str().unwrap_or("").to_owned(),
            turn.clone(),
        );
        let mut row = crate::conversation_view::select(
            source,
            &[
                "kind",
                "status",
                "text",
                "attachments",
                "created_at",
                "completed_at",
                "usage",
                "context_delivery",
                "context_ledger",
                "capability_refs",
                "reference_context",
            ],
        );
        if row["context_ledger"].is_object() {
            row["context_ledger"]["thread_id"] = json!(tid);
            row["context_ledger"]["turn_id"] = json!(turn);
        }
        row["turn_id"] = json!(turn);
        row["seq"] = json!(copied_turns.len() + 1);
        row["source_turn_id"] = source["turn_id"].clone();
        row["source_thread_id"] = source_id.clone();
        row["runtime_snapshot"] = crate::conversation_view::runtime(&source["runtime_snapshot"]);
        copied_turns.push(row);
    }
    let mut copied_messages = Vec::new();
    let mut digests = std::collections::HashSet::new();
    for source in current_messages(v) {
        let source_turn = source["turn_id"].as_str().unwrap_or("");
        if !source_turn.is_empty() && !turn_ids.contains_key(source_turn) {
            continue;
        }
        let mut row = crate::conversation_view::select(
            &source,
            &["role", "text", "attachments", "status", "created_at"],
        );
        row["message_id"] = json!(id());
        row["turn_id"] = turn_ids
            .get(source_turn)
            .map(|id| json!(id))
            .unwrap_or(Value::Null);
        row["source_message_id"] = source["message_id"].clone();
        row["source_thread_id"] = source_id.clone();
        row["stream_seq"] = json!(copied_messages.len() + 1);
        for sha in row["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            digests.insert(sha.to_owned());
        }
        copied_messages.push(row);
    }
    let artifacts: Vec<_> = v["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| {
            let attached = a["artifact_id"]
                .as_str()
                .or_else(|| a["sha256"].as_str())
                .is_some_and(|reference| digests.contains(reference));
            let generated_in_copied_turn = a["kind"] == "conversation.generated_image"
                && a["turn_id"]
                    .as_str()
                    .is_some_and(|turn| turn_ids.contains_key(turn));
            attached || generated_in_copied_turn
        })
        .map(|a| {
            let mut row = crate::conversation_view::select(
                a,
                &[
                    "artifact_id",
                    "sha256",
                    "name",
                    "kind",
                    "media_type",
                    "size",
                    "created_at",
                ],
            );
            row["turn_id"] = a["turn_id"]
                .as_str()
                .and_then(|id| turn_ids.get(id))
                .map(|id| json!(id))
                .unwrap_or(Value::Null);
            row
        })
        .collect();
    let native_fork=if v["native_session"]["adapter_id"]=="codex.app_server" && v["native_session"]["capabilities"]["fork"]==true {
        boundary.and_then(|index|turns[index]["runtime_turn_id"].as_str()).and_then(|last|v["native_session"]["provider_session_id"].as_str().map(|provider|json!({"thread_id":provider,"last_turn_id":last,"consumed":false})))
    }else{None};
    Ok(
        json!({"native_fork":native_fork,"thread":{"thread_id":tid,"title":title,"title_source":"user","mode":"conversation",
        "project_id":v["thread"]["project_id"],"workspace_id":v["thread"]["workspace_id"],
        "source_thread_id":source_id,"from_turn_id":source_turn,"created_at":time,"updated_at":time,
        "archived":false,"state":{"status":"idle","running_turn_id":null,"unread":false}},
        "runtime":crate::conversation_view::runtime(&v["runtime"]),"messages":copied_messages,"turns":copied_turns,"artifacts":artifacts,"event_seq":0}),
    )
}

pub(crate) fn finish_value(v: &mut Value, turn: &Value, status: &str, error: Option<&str>) {
    v["thread"]["state"]["status"] = json!("idle");
    v["thread"]["state"]["running_turn_id"] = Value::Null;
    v["thread"]["state"]["unread"] = json!(true);
    if let Some(t) = v["turns"]
        .as_array_mut()
        .and_then(|rows| rows.iter_mut().find(|t| t["turn_id"] == *turn))
    {
        t["status"] = json!(status);
        t["completed_at"] = json!(now());
        if let Some(error) = error {
            t["error"] = json!({"message":error});
        }
    }
    for message in v["messages"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .filter(|m| m["turn_id"] == *turn && m["role"] == "assistant")
    {
        message["status"] = json!(status);
    }
    if let Some(error) = error {
        v["thread"]["state"]["last_error"] = json!({"message":error});
    }
    v["thread"]["updated_at"] = json!(now());
}
async fn complete(s: Arc<AppState>, tid: String, mut turn: String, mut configured: extension_chat::Plan) {
    loop {
        capture_checkpoint(&s, &tid, &turn).await;
        let result = extension_chat::provider(&s, &tid, &turn, &configured).await;
        let mut active = s.chat.active.lock().await;
        let Ok(mut v) = load(&s, &tid) else {
            active.remove(&tid);
            return;
        };
        if v["thread"]["state"]["running_turn_id"] != turn {
            return;
        }
        let succeeded = matches!(&result, Ok(extension_chat::Progress::Completed(_)));
        match result {
            Ok(extension_chat::Progress::Waiting) => {
                active.remove(&tid);
                return;
            }
            Ok(extension_chat::Progress::Completed(usage)) => {
                let message_already_completed = v["messages"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .rev()
                        .find(|message| {
                            message["turn_id"] == turn && message["role"] == "assistant"
                        })
                        .is_some_and(|message| message["status"] == "completed");
                finish_value(&mut v, &json!(turn), "completed", None);
                if let Some(usage) = usage {
                    if let Some(t) = v["turns"]
                        .as_array_mut()
                        .unwrap()
                        .iter_mut()
                        .find(|t| t["turn_id"] == turn)
                    {
                        t["usage"] = usage.clone();
                    }
                    v["thread"]["state"]["usage"] = usage.clone();
                    let mut record = json!({"id":turn,"at":chrono::Utc::now().timestamp_millis() as f64 / 1000.0,"engine":v["runtime"]["adapter_id"].as_str().unwrap_or(""),"role":"assistant","workspace_kind":"conversation","run_id":tid,"thread_id":tid,"turn_id":turn,"credential_id":"","model":configured.model,"provider":configured.adapter_id,"created_at":now(),"input_tokens":usage["prompt_tokens"],"output_tokens":usage["completion_tokens"],"total_tokens":usage["total_tokens"],"usage":usage,"token_coverage":"reported"});
                    crate::usage_history::capture(&mut record,&usage);
                    if s.store.put("usage", &turn, &record).is_err() {
                        v["thread"]["state"]["last_error"] =
                            json!({"message":"Response completed, but usage could not be saved"});
                    }
                }
                let completion_payload = v["messages"].as_array().unwrap().iter()
                    .rev().find(|m| m["turn_id"] == turn && m["role"] == "assistant")
                    .map(|m| json!({"turn_id":turn,"role":"assistant","message_id":m["message_id"],"text":m["text"]}))
                    .unwrap_or_else(|| json!({"turn_id":turn,"role":"assistant","text":""}));
                if !message_already_completed {
                    emit(
                        &s,
                        &tid,
                        &mut v,
                        "core.message.completed",
                        completion_payload,
                    );
                }
                emit(
                    &s,
                    &tid,
                    &mut v,
                    "core.turn.completed",
                    json!({"turn_id":turn}),
                );
            }
            Ok(extension_chat::Progress::Failed(error)) => {
                finish_value(&mut v, &json!(turn), "failed", error["message"].as_str());
                emit(
                    &s,
                    &tid,
                    &mut v,
                    "core.turn.failed",
                    json!({"turn_id":turn,"error":error}),
                );
            }
            Err(error) => {
                finish_value(&mut v, &json!(turn), "failed", Some(&error));
                emit(
                    &s,
                    &tid,
                    &mut v,
                    "core.turn.failed",
                    json!({"turn_id":turn,"error":{"message":error}}),
                );
            }
        }
        if !succeeded && queue_len(&v) > 0 {
            pause_queue(&mut v, "turn_failed");
        }
        let next = if succeeded {
            promote_queue(&s, &tid, &mut v)
        } else {
            None
        };
        if save(&s, &tid, &mut v).is_err() {
            active.remove(&tid);
            return;
        }
        if matches!(&next, None | Some((_, Dispatch::Native(_)))) {
            active.remove(&tid);
        }
        drop(active);
        s.computer.finish_turn(&tid, &turn).await;
        match next {
            Some((next_turn, Dispatch::Extension(next_plan))) => {
                turn = next_turn;
                configured = next_plan;
            }
            Some((next_turn, Dispatch::Native(plan))) => {
                crate::native_chat::spawn_dispatch(s.clone(), tid, next_turn, plan);
                return;
            }
            None => {
                return;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fork_keeps_context_snapshot_with_new_turn_identity() {
        let source = json!({"thread":{"thread_id":"source","title":"Source"},"turns":[{"turn_id":"original","status":"completed","text":"hello","seq":9,"context_delivery":[{"key":"attachment","chars":3}],"context_ledger":{"thread_id":"source","turn_id":"original","user_text":"hello","parts":[{"key":"attachment","text":"abc"}],"delivery_status":"request_sent"}}],"messages":[],"artifacts":[]});
        let fork = fork_value(&source, &json!({})).unwrap();
        assert_eq!(fork["turns"][0]["seq"], 1);
        assert_ne!(fork["turns"][0]["turn_id"], "original");
        assert_eq!(
            fork["turns"][0]["context_ledger"]["thread_id"],
            fork["thread"]["thread_id"]
        );
        assert_eq!(
            fork["turns"][0]["context_ledger"]["turn_id"],
            fork["turns"][0]["turn_id"]
        );
        assert_eq!(
            fork["turns"][0]["context_ledger"]["parts"][0]["text"],
            "abc"
        );
        assert_eq!(fork["turns"][0]["source_turn_id"], "original");
        assert_eq!(fork["turns"][0]["context_delivery"][0]["key"], "attachment");
    }
}

fn inbox_snapshot(s: &AppState, project: &str, seq: u64, compact: bool) -> Result<Value, rusqlite::Error> {
    inbox_snapshot_from_store(&s.store, &s.chat.epoch, project, seq, compact)
}

pub fn inbox_snapshot_from_store(
    store: &crate::store::Store,
    broker_epoch: &str,
    project: &str,
    seq: u64,
    compact: bool,
) -> Result<Value, rusqlite::Error> {
    let rows = store
        .conversation_summaries()?
        .into_iter()
        .filter(|v| project.is_empty() || v["thread"]["project_id"] == project)
        .filter(|v| !is_side_chat(&v["thread"]) && !is_ctf_thread(&v["thread"]))
        .collect::<Vec<_>>();
    let threads = rows.iter().map(if compact { inbox_thread } else { listed_thread }).collect::<Vec<_>>();
    let listed: HashMap<&str, &Value> = rows.iter().filter_map(|row| {
        row["thread"]["thread_id"].as_str().map(|id| (id, &row["thread"]))
    }).collect();
    let mut owners = HashMap::<String, String>::new();
    let mut delegated = HashMap::<String, Vec<Value>>::new();
    for row in &rows {
        let thread = &row["thread"];
        let child_id = thread["thread_id"].as_str().unwrap_or("");
        let lineage = &thread["state"]["lineage"];
        let root_id = lineage["root_thread_id"].as_str().unwrap_or("");
        let Some(root) = listed.get(root_id) else { continue };
        if child_id == root_id || root["archived"] == true { continue; }
        owners.insert(child_id.to_owned(), root_id.to_owned());
        if thread["archived"] == true { continue; }
        let (kind, pending_id) = inbox_pending(&thread["state"]);
        if !matches!(kind, "approval" | "user_input") { continue; }
        delegated.entry(root_id.to_owned()).or_default().push(json!({
            "thread_id": child_id,
            "parent_thread_id": lineage["parent_thread_id"],
            "depth": lineage["depth"],
            "title": thread["title"],
            "pending_kind": kind,
            "pending_id": pending_id,
        }));
    }
    for items in delegated.values_mut() {
        items.sort_by_key(|item| if item["pending_kind"] == "approval" { 0 } else { 1 });
    }
    let summaries=rows.iter().map(|v| {
        let thread=&v["thread"]; let status=&thread["state"];
        let unread=status["unread"].as_bool().unwrap_or(false);
        let running=!status["running_turn_id"].is_null() || status["status"]=="running";
        let (own_pending, own_pending_id) = inbox_pending(status);
        let id = thread["thread_id"].as_str().unwrap_or("");
        let subagent_attention = delegated.get(id).cloned().unwrap_or_default();
        let first = subagent_attention.first();
        let pending = if own_pending == "none" { first.and_then(|item| item["pending_kind"].as_str()).unwrap_or("none") } else { own_pending };
        let pending_id = if own_pending == "none" { first.map(|item| item["pending_id"].clone()).unwrap_or(Value::Null) } else { own_pending_id };
        let pending_thread_id = if own_pending == "none" { first.map(|item| item["thread_id"].clone()).unwrap_or(Value::Null) }
            else { json!(id) };
        let owner = owners.get(id).map(String::as_str).unwrap_or(id);
        let revision=v["event_seq"].as_u64().unwrap_or(0);
        let preview=v["preview"].as_str().unwrap_or("");
        let work_pending = status["queue_count"].as_u64().unwrap_or(0) > 0
            && status["queue_paused"] != true && status["queue_failed_item_id"].is_null();
        let fingerprint=json!([id,revision,status["status"],running,work_pending,unread,pending,
            pending_id,thread["title"],pending_thread_id,owner,subagent_attention]).to_string();
        json!({"thread_id":thread["thread_id"],"revision":revision,"status":status["status"],"running":running,
            "unread":unread,"pending_kind":pending,"pending_id":pending_id,"pending_thread_id":pending_thread_id,
            "attention_owner_id":owner,"subagent_attention":subagent_attention,"title":thread["title"],
            "preview":preview,"updated_at":thread["updated_at"],"fingerprint":fingerprint})
    }).collect::<Vec<_>>();
    Ok(
        json!({"inbox_seq":seq,"broker_epoch":broker_epoch,"cursor_reset":true,"count":threads.len(),"threads":threads,"summaries":summaries}),
    )
}

fn inbox_rows(value: &Value) -> impl Iterator<Item = (&str, &Value)> {
    value.as_array().into_iter().flatten().filter_map(|row| {
        row["thread_id"].as_str().map(|id| (id, row))
    })
}

fn changed_inbox_rows<'a>(previous: &Value, next: &'a Value) -> Vec<&'a Value> {
    let previous = inbox_rows(previous).collect::<HashMap<_, _>>();
    inbox_rows(next).filter_map(|(id, row)| (previous.get(id).copied() != Some(row)).then_some(row)).collect()
}

fn inbox_patch(previous: &Value, next: &Value) -> Option<Value> {
    let threads = changed_inbox_rows(&previous["threads"], &next["threads"]);
    let summaries = changed_inbox_rows(&previous["summaries"], &next["summaries"]);
    let remaining = inbox_rows(&next["threads"]).map(|(id, _)| id).collect::<HashSet<_>>();
    let removed = inbox_rows(&previous["threads"]).filter_map(|(id, _)| (!remaining.contains(id)).then_some(id)).collect::<Vec<_>>();
    if threads.is_empty() && summaries.is_empty() && removed.is_empty() {
        return None;
    }
    Some(json!({"base_seq":previous["inbox_seq"],"inbox_seq":next["inbox_seq"],"broker_epoch":next["broker_epoch"],
        "threads":threads,"summaries":summaries,"removed_thread_ids":removed}))
}

fn inbox_pending(state: &Value) -> (&'static str, Value) {
    if let Some(approvals) = state["pending_approvals"].as_object() {
        for row in approvals.values() {
            if row.is_object() && row["status"].as_str().is_none_or(|status| status == "pending") {
                return ("approval", inbox_pending_id(row, &["approval_id", "id", "request_id"]));
            }
        }
    }
    let approval = &state["pending_approval"];
    if approval.as_object().is_some_and(|fields| !fields.is_empty())
        && approval["status"].as_str().is_none_or(|status| status == "pending") {
        return ("approval", inbox_pending_id(approval, &["approval_id", "id", "request_id"]));
    }
    let input = &state["pending_user_input"];
    if input.as_object().is_some_and(|fields| !fields.is_empty()) {
        return ("user_input", inbox_pending_id(input, &["request_id", "id", "approval_id"]));
    }
    if state["unread"] == true && state["last_error"].is_object() {
        return ("failed", Value::Null);
    }
    ("none", Value::Null)
}

fn inbox_pending_id(row: &Value, keys: &[&str]) -> Value {
    keys.iter().find_map(|key| row[*key].as_str().map(str::trim).filter(|id| !id.is_empty()))
        .map_or(Value::Null, |id| json!(id))
}
async fn inbox(
    State(s): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let project = query.get("project_id").cloned().unwrap_or_default();
    let delta = query.get("updates").is_some_and(|mode| mode == "delta");
    let mut receiver = s.chat.inbox_seq.subscribe();
    let seq = *receiver.borrow_and_update();
    let initial = match inbox_snapshot(&s, &project, seq, delta) {
        Ok(v) => v,
        Err(e) => return db(e).into_response(),
    };
    let stream = futures_util::stream::unfold(
        (Some(initial), None, receiver, s, project, tokio::time::Instant::now()),
        move |(mut first, previous, mut receiver, s, project, mut next_snapshot)| async move {
            let (kind, data, value) = if let Some(value) = first.take() {
                ("snapshot", value.to_string(), value)
            } else {
                loop {
                    receiver.changed().await.ok()?;
                    tokio::time::sleep_until(next_snapshot).await;
                    let seq = *receiver.borrow_and_update();
                    let value = match inbox_snapshot(&s, &project, seq, delta) {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::error!(%error, "Could not refresh conversation inbox");
                            return None;
                        }
                    };
                    next_snapshot = tokio::time::Instant::now() + Duration::from_millis(250);
                    if let Some(previous) = &previous {
                        if let Some(patch) = inbox_patch(previous, &value) {
                            break ("patch", patch.to_string(), value);
                        }
                    } else {
                        break ("snapshot", value.to_string(), value);
                    }
                }
            };
            Some((
                Ok::<_, Infallible>(Event::default().event(kind).data(data)),
                (None, delta.then_some(value), receiver, s, project, tokio::time::Instant::now() + Duration::from_millis(250)),
            ))
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
