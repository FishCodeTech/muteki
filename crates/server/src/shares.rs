//! Fixed, revocable snapshots for the ordinary local conversation UI.
use crate::{AppState, access_api::Access};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    sync::{Arc, Mutex},
};

const MAX_MESSAGES: usize = 500;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_ATTACHMENT: usize = 256 * 1024;
const PREVIEW_TTL: f64 = 900.0;
static WRITER: Mutex<()> = Mutex::new(());
type Failure = (StatusCode, Json<Value>);
type Result<T> = std::result::Result<T, Failure>;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/threads/{id}/shares/source", get(source))
        .route("/api/threads/{id}/shares/preview", post(preview))
        .route("/api/threads/{id}/shares", get(list).post(create))
        .route("/api/threads/{id}/shares/{share}", delete(revoke))
        .route("/api/conversation-shares/{share}", get(read))
        .route(
            "/api/conversation-shares/{share}/attachments/{attachment}",
            get(attachment),
        )
        .layer(axum::middleware::from_fn(private_headers))
}
async fn private_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-robots-tag", "noindex, nofollow"),
        ("x-content-type-options", "nosniff"),
    ] {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}
fn fail(status: StatusCode, message: &str) -> Failure {
    (
        status,
        Json(
            json!({"detail":message,"error":{"code":"conversation.share.failed","message":message}}),
        ),
    )
}
fn internal(_: impl std::fmt::Display) -> Failure {
    fail(StatusCode::INTERNAL_SERVER_ERROR, "无法保存或读取会话快照")
}
fn now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}
fn random_bytes(bytes: usize) -> Result<Vec<u8>> {
    let mut data = vec![0; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut data))
        .map_err(internal)?;
    Ok(data)
}
fn random(bytes: usize) -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes(bytes)?))
}
fn attachment_id() -> Result<String> {
    Ok(random_bytes(12)?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
fn rows<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn load(s: &AppState, id: &str) -> Result<Value> {
    s.store
        .get("conversation_threads", id)
        .map_err(internal)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "对话不存在"))
}
fn records(s: &AppState, id: &str) -> Result<Value> {
    Ok(s.store
        .get("conversation_share_records", id)
        .map_err(internal)?
        .unwrap_or_else(|| json!({"thread_id":id,"previews":[],"shares":[]})))
}
fn clean(input: &str, redact: bool) -> String {
    if !redact {
        return input.to_owned();
    }
    let chars: Vec<char> = input.chars().collect();
    let mut result = String::new();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        let boundary = index == 0
            || !(chars[index - 1].is_alphanumeric() || matches!(chars[index - 1], '_' | ':' | '/'));
        let absolute = boundary
            && ((c == '/' && chars.get(index + 1) != Some(&'/'))
                || (c == '\\' && chars.get(index + 1) == Some(&'\\'))
                || (c.is_ascii_alphabetic()
                    && chars.get(index + 1) == Some(&':')
                    && matches!(chars.get(index + 2), Some('/' | '\\'))));
        if absolute {
            result.push_str("<path>");
            while index < chars.len()
                && !chars[index].is_whitespace()
                && !matches!(chars[index], '"' | '\'' | '`' | '<' | '>')
            {
                index += 1;
            }
        } else {
            result.push(c);
            index += 1;
        }
    }
    result
}
#[derive(Deserialize)]
struct PreviewRequest {
    message_ids: Option<Vec<String>>,
    #[serde(default)]
    attachment_ids: Vec<String>,
    #[serde(default = "yes")]
    redact_paths: bool,
    #[serde(default)]
    include_tool_summaries: bool,
}
fn yes() -> bool {
    true
}
impl Default for PreviewRequest {
    fn default() -> Self {
        Self {
            message_ids: None,
            attachment_ids: vec![],
            redact_paths: true,
            include_tool_summaries: false,
        }
    }
}
fn messages(doc: &Value) -> Vec<Value> {
    crate::chat_core::fold_messages(&crate::chat::current_messages(doc))
        .into_iter()
        .filter(|m| {
            matches!(text(m, "role"), "user" | "assistant")
                && matches!(text(m, "kind"), "message" | "steer" | "")
        })
        .collect()
}
fn snapshot(s: &AppState, doc: &Value, body: &PreviewRequest) -> Result<(Value, Value)> {
    if body
        .message_ids
        .as_ref()
        .is_some_and(|ids| ids.len() > MAX_MESSAGES)
        || body.attachment_ids.len() > 10
    {
        return Err(fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "消息或附件数量超过限制",
        ));
    }
    let mut selected = messages(doc);
    if let Some(ids) = &body.message_ids {
        if ids.is_empty()
            || ids
                .iter()
                .any(|id| !selected.iter().any(|m| m["message_id"] == id.as_str()))
        {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "请选择当前有效分支中的正文消息",
            ));
        }
        selected.retain(|m| ids.iter().any(|id| m["message_id"] == id.as_str()));
    }
    if selected.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "当前有效分支还没有可分享的正文",
        ));
    }
    if selected.len() > MAX_MESSAGES {
        return Err(fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            "消息超过 500 条，请选择部分回复及必要上下文",
        ));
    }
    let turns: HashSet<&str> = selected
        .iter()
        .filter_map(|m| m["turn_id"].as_str().filter(|id| !id.is_empty()))
        .collect();
    let referenced: HashSet<&str> = rows(doc, "turns")
        .iter()
        .filter(|t| turns.contains(text(t, "turn_id")))
        .flat_map(|t| rows(t, "attachments"))
        .filter_map(Value::as_str)
        .collect();
    let allowed: Vec<&Value> = rows(doc, "artifacts")
        .iter()
        .filter(|a| referenced.contains(text(a, "artifact_id")))
        .collect();
    if body.attachment_ids.iter().any(|id| id.len() == 64) {
        return Err(fail(StatusCode::CONFLICT, "旧版附件缺少可读取的工件引用，请重新选择原文件"));
    }
    if body
        .attachment_ids
        .iter()
        .any(|id| !allowed.iter().any(|a| a["artifact_id"] == id.as_str()))
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "附件不属于所选正文，请重新选择",
        ));
    }
    let mut candidates = Vec::new();
    let mut attachments = Vec::new();
    let mut seen = HashSet::new();
    for artifact in allowed {
        let digest = text(artifact, "artifact_id");
        if !seen.insert(digest) {
            continue;
        }
        let name = text(artifact, "name").replace('\\', "/");
        let name = name
            .rsplit('/')
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or("附件.txt");
        let content = if artifact["size"].as_u64().unwrap_or(0) > MAX_ATTACHMENT as u64 {
            None
        } else {
            crate::files::read_attachment(s, doc, digest)
                .ok()
                .map(|(_, bytes)| bytes)
        };
        let reason;
        let visible = match content.as_deref() {
            Some(bytes) if bytes.len() > MAX_ATTACHMENT => {
                reason = "仅支持不超过 256 KiB 的 UTF-8 文本附件";
                None
            }
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(value) if !value.contains('\0') => {
                    reason = "";
                    Some(value)
                }
                _ => {
                    reason = "仅支持可预览的 UTF-8 文本附件";
                    None
                }
            },
            None => {
                reason = if artifact["size"].as_u64().unwrap_or(0) > MAX_ATTACHMENT as u64 {
                    "仅支持不超过 256 KiB 的 UTF-8 文本附件"
                } else {
                    "原附件已不可用"
                };
                None
            }
        };
        candidates.push(json!({"id":digest,"name":clean(name,body.redact_paths),"size":content.as_ref().map(Vec::len).unwrap_or(artifact["size"].as_u64().unwrap_or(0) as usize),"unavailable_reason":reason}));
        if body.attachment_ids.iter().any(|id| id == digest) {
            let value = visible
                .ok_or_else(|| fail(StatusCode::BAD_REQUEST, &format!("附件 {name}：{reason}")))?;
            let value = clean(value, body.redact_paths);
            attachments.push(json!({"id":attachment_id()?,"name":clean(name,body.redact_paths),"size":value.len(),"text":value}));
        }
    }
    let tools: Vec<Value> = if body.include_tool_summaries {
        rows(doc,"tools").iter().filter(|tool| turns.contains(text(tool,"turn_id"))).map(|tool| {
            let mut status = match text(tool,"status") { "completed"=>"completed", "failed"=>"failed", "cancelled"=>"cancelled", "declined"=>"declined", _=>"ended" };
            if !matches!(status,"cancelled"|"declined") && ((!tool["error"].is_null() && tool["error"] != "") || tool["exit_code"].as_i64().is_some_and(|code| code != 0)) { status="failed"; }
            json!({"name":clean(&text(tool,"name").chars().take(120).collect::<String>(),body.redact_paths),"status":status})
        }).collect()
    } else {
        vec![]
    };
    let title = text(&doc["thread"], "title");
    let value = json!({"version":1,"title":clean(if title.is_empty(){"会话快照"}else{title},body.redact_paths),"captured_at":now(),"watermark":doc["event_seq"].as_u64().unwrap_or(0),"branch":"current","redact_paths":body.redact_paths,"tools_exclude_parameters":true,
        "messages":selected.iter().enumerate().map(|(i,m)| json!({"id":format!("message-{}",i+1),"role":m["role"],"text":clean(text(m,"text"),body.redact_paths)})).collect::<Vec<_>>(),"tool_summaries":tools,"attachments":attachments});
    if serde_json::to_vec(&value).map_err(internal)?.len() > MAX_BYTES {
        return Err(fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            "快照超过 2 MiB，请减少选中的消息或附件",
        ));
    }
    let choices = json!({"messages":selected.iter().map(|m| json!({"id":m["message_id"],"role":m["role"],"preview":clean(text(m,"text"),body.redact_paths).chars().take(160).collect::<String>()})).collect::<Vec<_>>(),"attachments":candidates});
    Ok((value, choices))
}
async fn source(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let offset = q
        .get("offset")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| fail(StatusCode::UNPROCESSABLE_ENTITY, "offset 必须为非负整数"))?
        .unwrap_or(0);
    let doc = load(&s, &id)?;
    let messages = messages(&doc);
    let end = messages.len().saturating_sub(offset);
    let ids: Vec<String> = messages[end.saturating_sub(MAX_MESSAGES)..end]
        .iter()
        .map(|m| text(m, "message_id").to_owned())
        .collect();
    if ids.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            if messages.is_empty() {
                "当前有效分支还没有可分享的正文"
            } else {
                "该消息页已不存在，请返回最新一页"
            },
        ));
    }
    let (_, choices) = snapshot(
        &s,
        &doc,
        &PreviewRequest {
            message_ids: Some(ids.clone()),
            ..Default::default()
        },
    )?;
    Ok(Json(
        json!({"choices":choices,"initial_message_ids":if messages.len()>MAX_MESSAGES{json!(ids)}else{Value::Null},"total_message_count":messages.len(),"offset":offset,"has_older":offset.saturating_add(MAX_MESSAGES)<messages.len()}),
    ))
}
async fn preview(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PreviewRequest>,
) -> Result<Json<Value>> {
    let doc = load(&s, &id)?;
    let (snapshot, choices) = snapshot(&s, &doc, &body)?;
    let _guard = WRITER.lock().map_err(internal)?;
    let mut record = records(&s, &id)?;
    let time = now();
    let previews = record["previews"]
        .as_array_mut()
        .ok_or_else(|| internal("Invalid sharing data"))?;
    previews.retain(|p| p["expires"].as_f64().unwrap_or(0.0) > time);
    let pid = random(18)?;
    previews.push(json!({"id":pid,"snapshot":snapshot,"expires":time+PREVIEW_TTL}));
    s.store
        .put("conversation_share_records", &id, &record)
        .map_err(internal)?;
    Ok(Json(
        json!({"preview_id":pid,"snapshot":snapshot,"choices":choices,"preview_expires_in_seconds":900}),
    ))
}
#[derive(Deserialize)]
struct CreateRequest {
    preview_id: String,
    access_mode: String,
    expires_in_seconds: u64,
}
async fn create(
    State(s): State<Arc<AppState>>,
    Extension(access): Extension<Arc<Access>>,
    Path(id): Path<String>,
    Json(body): Json<CreateRequest>,
) -> Result<Response> {
    load(&s, &id)?;
    if body.preview_id.len() != 24
        || !body
            .preview_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        || !matches!(body.access_mode.as_str(), "authenticated" | "link")
        || !(300..=604800).contains(&body.expires_in_seconds)
    {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "分享参数无效"));
    }
    if body.access_mode == "authenticated" && !access.enabled()? {
        return Err(fail(
            StatusCode::CONFLICT,
            "工作台尚未启用登录认证，请先配置认证，或明确选择链接持有人访问",
        ));
    }
    let _guard = WRITER.lock().map_err(internal)?;
    let mut record = records(&s, &id)?;
    let time = now();
    let snapshot = rows(&record, "previews")
        .iter()
        .find(|p| p["id"] == body.preview_id && p["expires"].as_f64().unwrap_or(0.0) > time)
        .map(|p| p["snapshot"].clone())
        .ok_or_else(|| fail(StatusCode::CONFLICT, "预览已过期或已创建分享，请重新预览"))?;
    if rows(&record, "shares")
        .iter()
        .filter(|p| p["revoked"].is_null() && p["expires_at"].as_f64().unwrap_or(0.0) > time)
        .count()
        >= 50
    {
        return Err(fail(
            StatusCode::CONFLICT,
            "本会话有效分享已达 50 条，请先撤销旧分享",
        ));
    }
    let share_id = random(18)?;
    let token = random(32)?;
    let expires = time + body.expires_in_seconds as f64;
    let share = json!({"share_id":share_id,"token_hash":format!("{:x}",Sha256::digest(token.as_bytes())),"snapshot":snapshot,"access_mode":body.access_mode,"created_at":time,"expires_at":expires,"revoked":null});
    record["shares"]
        .as_array_mut()
        .ok_or_else(|| internal("Invalid sharing data"))?
        .push(share);
    record["previews"]
        .as_array_mut()
        .ok_or_else(|| internal("Invalid sharing data"))?
        .retain(|p| p["id"] != body.preview_id);
    s.store
        .put("conversation_share_records", &id, &record)
        .map_err(internal)?;
    Ok((StatusCode::CREATED,Json(json!({"share_id":share_id,"token":token,"access_mode":body.access_mode,"created_at":time,"expires_at":expires}))).into_response())
}
pub(crate) async fn create_direct(
    s: Arc<AppState>,
    id: String,
    body: Value,
) -> Result<Json<Value>> {
    let doc = load(&s, &id)?;
    let preview_req: PreviewRequest = serde_json::from_value(body.clone())
        .map_err(|_| fail(StatusCode::UNPROCESSABLE_ENTITY, "分享参数无效"))?;
    let access_mode = body["access_mode"].as_str().unwrap_or("").trim().to_owned();
    let expires_in_seconds = body["expires_in_seconds"].as_u64().unwrap_or(86400);
    if !matches!(access_mode.as_str(), "authenticated" | "link")
        || !(300..=604800).contains(&expires_in_seconds)
    {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "分享参数无效"));
    }
    if access_mode == "authenticated" {
        let auth_enabled =
            crate::access_api::Access::enabled_for_state(&crate::native_chat::state_dir(&s)).map_err(
                |_| {
                    fail(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "访问配置不可用，请修复后重启服务。",
                    )
                },
            )?;
        if !auth_enabled {
            return Err(fail(
                StatusCode::CONFLICT,
                "工作台尚未启用登录认证，请先配置认证，或明确选择链接持有人访问",
            ));
        }
    }
    let (snapshot, _) = snapshot(&s, &doc, &preview_req)?;
    let _guard = WRITER.lock().map_err(internal)?;
    let mut record = records(&s, &id)?;
    let time = now();
    if rows(&record, "shares")
        .iter()
        .filter(|p| p["revoked"].is_null() && p["expires_at"].as_f64().unwrap_or(0.0) > time)
        .count()
        >= 50
    {
        return Err(fail(
            StatusCode::CONFLICT,
            "本会话有效分享已达 50 条，请先撤销旧分享",
        ));
    }
    let share_id = random(18)?;
    let token = random(32)?;
    let expires = time + expires_in_seconds as f64;
    let share = json!({"share_id":share_id,"token_hash":format!("{:x}",Sha256::digest(token.as_bytes())),"snapshot":snapshot,"access_mode":access_mode,"created_at":time,"expires_at":expires,"revoked":null});
    record["shares"]
        .as_array_mut()
        .ok_or_else(|| internal("Invalid sharing data"))?
        .push(share);
    s.store
        .put("conversation_share_records", &id, &record)
        .map_err(internal)?;
    Ok(Json(json!({"share_id":share_id,"token":token,"access_mode":access_mode,"created_at":time,"expires_at":expires})))
}
fn summary(share: &Value) -> Value {
    let mut value = crate::conversation_view::select(
        share,
        &["share_id", "access_mode", "created_at", "expires_at"],
    );
    value["status"] = json!(if !share["revoked"].is_null() {
        "revoked"
    } else if share["expires_at"].as_f64().unwrap_or(0.0) <= now() {
        "expired"
    } else {
        "active"
    });
    value["message_count"] = json!(rows(&share["snapshot"], "messages").len());
    value["watermark"] = share["snapshot"]["watermark"].clone();
    value
}
pub(crate) async fn list(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Result<Json<Value>> {
    load(&s, &id)?;
    let record = records(&s, &id)?;
    let mut shares: Vec<Value> = rows(&record, "shares").iter().map(summary).collect();
    shares.sort_by(|a, b| {
        (b["status"] == "active")
            .cmp(&(a["status"] == "active"))
            .then_with(|| {
                b["created_at"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .total_cmp(&a["created_at"].as_f64().unwrap_or(0.0))
            })
    });
    shares.truncate(100);
    Ok(Json(json!({"shares":shares})))
}
pub(crate) async fn revoke(
    State(s): State<Arc<AppState>>,
    Path((id, share)): Path<(String, String)>,
) -> Result<Json<Value>> {
    load(&s, &id)?;
    let _guard = WRITER.lock().map_err(internal)?;
    let mut record = records(&s, &id)?;
    let item = record["shares"]
        .as_array_mut()
        .ok_or_else(|| internal("Invalid sharing data"))?
        .iter_mut()
        .find(|v| v["share_id"] == share)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "分享不存在"))?;
    if item["revoked"].is_null() {
        item["revoked"] = json!(now());
    }
    s.store
        .put("conversation_share_records", &id, &record)
        .map_err(internal)?;
    Ok(Json(json!({"revoked":true})))
}
fn read_scope(s: &AppState, access: &Access, id: &str, headers: &HeaderMap) -> Result<Value> {
    let token = headers
        .get("x-muteki-share-token")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let missing = || fail(StatusCode::NOT_FOUND, "分享不存在或链接不完整");
    if id.len() != 24 || token.len() != 43 {
        return Err(missing());
    }
    let hash = format!("{:x}", Sha256::digest(token.as_bytes()));
    let share = s.store.conversation_share(id, &hash).map_err(internal)?.ok_or_else(missing)?;
    if !share["revoked"].is_null() {
        return Err(fail(StatusCode::GONE, "此分享已撤销"));
    }
    if share["expires_at"].as_f64().unwrap_or(0.0) <= now() {
        return Err(fail(StatusCode::GONE, "此分享已过期"));
    }
    if share["access_mode"] == "authenticated" && !access.authenticated(headers)? {
        return Err(fail(
            StatusCode::UNAUTHORIZED,
            "此分享仅允许已登录的工作台用户查看",
        ));
    }
    let mut public = crate::conversation_view::select(
        &share,
        &[
            "share_id",
            "access_mode",
            "created_at",
            "expires_at",
            "snapshot",
        ],
    );
    if !public["snapshot"].is_object() {
        public["snapshot"] = json!({});
    }
    Ok(public)
}
async fn read(
    State(s): State<Arc<AppState>>,
    Extension(access): Extension<Arc<Access>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    Ok(Json(read_scope(&s, &access, &id, &headers)?))
}
async fn attachment(
    State(s): State<Arc<AppState>>,
    Extension(access): Extension<Arc<Access>>,
    Path((id, attachment)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let share = read_scope(&s, &access, &id, &headers)?;
    let item = rows(&share["snapshot"], "attachments")
        .iter()
        .find(|a| a["id"] == attachment)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "此快照没有所选附件"))?;
    let name: String = text(item, "name")
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    let mut response = text(item, "text").to_owned().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_str(&format!(
            "attachment; filename=\"attachment.txt\"; filename*=UTF-8''{name}"
        ))
        .map_err(internal)?,
    );
    Ok(response)
}
