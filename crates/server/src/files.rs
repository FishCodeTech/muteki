//! Uploaded bytes and exports for the existing conversation frontend.
use crate::AppState;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use sha2::{Digest,Sha256};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
};

type Failure = (StatusCode, Json<Value>);
fn fail(status: StatusCode, code: &str, message: &str) -> Failure {
    (
        status,
        Json(json!({"error":{"code":code,"message":message},"detail":message})),
    )
}
fn internal(_: impl std::fmt::Display) -> Failure {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        "conversation.storage.failed",
        "Could not save or read conversation data",
    )
}
fn load(s: &AppState, id: &str) -> Result<Value, Failure> {
    s.store
        .get("conversation_threads", id)
        .map_err(internal)?
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "conversation.thread.not_found",
                "Thread not found",
            )
        })
}
const MAX_SCOPED_IMAGE_BYTES: usize = 24 * 1024 * 1024;

fn state_dir(s: &AppState) -> Result<PathBuf, String> {
    let dir = s
        .store
        .get("system", "state_dir")
        .map_err(|_| "Could not read state location")?;
    Ok(PathBuf::from(
        dir.as_ref().and_then(Value::as_str).unwrap_or(".state"),
    ))
}

fn root(s: &AppState) -> Result<PathBuf, String> {
    Ok(state_dir(s)?.join("uploads"))
}
fn valid_ref(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok()
}
pub(crate) fn remove_unused_attachments(s: &AppState, deleted: &Value) -> Result<(), String> {
    let remaining = s.store.list("conversation_threads").map_err(|error| error.to_string())?;
    let root = root(s)?;
    if root.is_symlink() {
        return Err("Attachment directory is a symbolic link".into());
    }
    for artifact in deleted["artifacts"].as_array().into_iter().flatten() {
        let Some(reference) = artifact["artifact_id"].as_str().filter(|id| valid_ref(id)) else { continue; };
        if remaining.iter().any(|thread| attachment_metadata(thread, reference).is_some()) {
            continue;
        }
        match std::fs::remove_file(root.join(reference)) {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(format!("Could not remove unused attachment {reference}: {error}")),
        }
        s.store.delete("attachment_integrity", reference).map_err(|error| error.to_string())?;
    }
    Ok(())
}
fn legacy_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn attachment_metadata<'a>(thread: &'a Value, reference: &str) -> Option<&'a Value> {
    if !valid_ref(reference) && !legacy_digest(reference) {
        return None;
    }
    thread["artifacts"]
        .as_array()?
        .iter()
        .find(|v| v["artifact_id"] == reference || v["legacy_sha256"] == reference)
}
/// Resolve bytes through a thread-associated opaque reference.
/// Never resolves a client filename, markdown path, or source machine path.
pub fn read_attachment(
    s: &AppState,
    thread: &Value,
    reference: &str,
) -> Result<(Value, Vec<u8>), String> {
    let mut meta = attachment_metadata(thread, reference)
        .ok_or("Attachment not found in this thread")?
        .clone();
    let root=root(s)?;
    if root.is_symlink(){return Err("Attachment directory is a symbolic link".into());}
    let reference = meta["artifact_id"].as_str().filter(|value| valid_ref(value)).ok_or("Attachment has no usable artifact reference")?;
    let path = root.join(reference);
    let info = std::fs::symlink_metadata(&path).map_err(|_| "Attachment bytes unavailable")?;
    if !info.is_file() {
        return Err("Attachment bytes unavailable".into());
    }
    let mut bytes = Vec::new();
    let mut options=std::fs::OpenOptions::new();options.read(true);
    #[cfg(unix)] {use std::os::unix::fs::OpenOptionsExt;options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);}
    options.open(&path)
        .map_err(|_| "Attachment bytes unavailable")?
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read attachment")?;
    let digest=format!("{:x}",Sha256::digest(&bytes));
    if meta["size"].as_u64().is_some_and(|size|size!=bytes.len() as u64){return Err("Attachment size does not match its saved metadata".into());}
    let prior=s.store.get("attachment_integrity",reference).map_err(|_|"Cannot read attachment integrity metadata")?;
    let Some(prior)=prior else {
        return Err("Attachment integrity metadata is missing; select the original file again".into());
    };
    let expected=meta["sha256"].as_str().or_else(||prior["sha256"].as_str());
    if expected.is_some_and(|expected|expected!=digest){return Err("Attachment digest does not match its saved metadata".into());}
    meta["sha256"]=json!(digest);meta["size"]=json!(bytes.len());
    Ok((meta, bytes))
}
pub(crate) fn read_control_attachment(s: &AppState, caller: &str, reference: &str) -> Result<(Value, Vec<u8>), String> {
    let grant = crate::control::Grant::standard();
    for thread in s.store.list("conversation_threads").map_err(|error| error.to_string())? {
        let Some(id) = thread["thread"]["thread_id"].as_str() else { continue; };
        if crate::control::check_target(s, caller, id, &grant).is_ok() && attachment_metadata(&thread, reference).is_some() {
            return read_attachment(s, &thread, reference);
        }
    }
    Err("Attachment not found in a thread within your control scope".into())
}
fn persist(s: &AppState, reference: &str, bytes: &[u8]) -> Result<(), String> {
    let dir = root(s)?;
    std::fs::create_dir_all(&dir).map_err(|_| "Could not create upload directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "Could not protect upload directory")?;
    }
    let target = dir.join(reference);
    let temp = dir.join(format!(".{}.partial", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|_| "Could not create upload")?;
        file.write_all(bytes)
            .map_err(|_| "Could not write complete upload")?;
        file.sync_all().map_err(|_| "Could not flush upload")?;
        std::fs::rename(&temp, &target).map_err(|_| "Could not finish upload")?;
        std::fs::File::open(&dir).and_then(|directory|directory.sync_all()).map_err(|_|"Could not persist upload directory")?;
        s.store.put("attachment_integrity",reference,&json!({"sha256":format!("{:x}",Sha256::digest(bytes)),"size":bytes.len(),"basis":"upload"}))
            .map_err(|_|"Could not save upload integrity metadata")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
pub(crate) fn store_agent_artifact(
    s: &AppState,
    name: &str,
    kind: &str,
    media_type: &str,
    bytes: &[u8],
) -> Result<Value, String> {
    let artifact_id = uuid::Uuid::new_v4().to_string();
    persist(s, &artifact_id, bytes)?;
    Ok(
        json!({"artifact_id":artifact_id,"name":name,"kind":kind,
        "media_type":media_type,"size":bytes.len(),"created_at":chrono::Utc::now().to_rfc3339()}),
    )
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/threads/{id}/uploads",
            post(upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/api/threads/{id}/artifacts/{sha}", get(artifact))
        .route("/api/threads/{id}/images/raw", get(image))
        .route("/api/threads/{id}/turns/{turn}/tool-files/{seq}/{index}", get(recorded_tool_file))
        .route("/api/threads/{id}/export", get(export))
}

async fn upload(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    mut form: Multipart,
) -> Result<Json<Value>, Failure> {
    load(&s, &id)?;
    let mut file = None;
    let mut command = String::new();
    let mut key = String::new();
    while let Some(mut field) = form.next_field().await.map_err(|_| {
        fail(
            StatusCode::BAD_REQUEST,
            "conversation.upload.invalid",
            "Invalid or incomplete upload",
        )
    })? {
        match field.name().unwrap_or("") {
            "file" => {
                if file.is_some() {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "conversation.upload.invalid",
                        "Upload exactly one file",
                    ));
                }
                let name = field
                    .file_name()
                    .unwrap_or("upload")
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(255)
                    .collect::<String>();
                let media = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_ascii_lowercase();
                let mut bytes = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(|_| {
                    fail(
                        StatusCode::BAD_REQUEST,
                        "conversation.upload.incomplete",
                        "Upload was interrupted or is too large",
                    )
                })? {
                    bytes.extend_from_slice(&chunk);
                }
                if bytes.is_empty() {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "conversation.upload.empty",
                        "Empty files cannot be attached",
                    ));
                }
                file = Some((
                    if name.trim().is_empty() {
                        "upload".into()
                    } else {
                        name
                    },
                    media,
                    bytes,
                ));
            }
            "command_id" | "idempotency_key" => {
                let name = field.name().unwrap().to_owned();
                let text = field.text().await.map_err(|_| {
                    fail(
                        StatusCode::BAD_REQUEST,
                        "conversation.upload.invalid",
                        "Invalid upload command",
                    )
                })?;
                if text.len() > 256 {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "conversation.upload.invalid",
                        "Upload command is too long",
                    ));
                }
                if name == "command_id" {
                    command = text.trim().to_owned();
                } else {
                    key = text.trim().to_owned();
                }
            }
            _ => {
                return Err(fail(
                    StatusCode::BAD_REQUEST,
                    "conversation.upload.invalid",
                    "Unknown upload field",
                ));
            }
        }
    }
    let (name, media, bytes) = file.ok_or_else(|| {
        fail(
            StatusCode::BAD_REQUEST,
            "conversation.upload.empty",
            "Select a file to attach",
        )
    })?;
    if command.is_empty() {
        command = uuid::Uuid::new_v4().to_string();
    }
    if key.is_empty() {
        key = command.clone();
    }
    let kind = query
        .get("kind")
        .map(String::as_str)
        .unwrap_or("conversation.upload");
    if kind.len() > 100 {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.upload.invalid",
            "Invalid attachment kind",
        ));
    }
    let request = json!({"name":name,"media_type":media,"kind":kind,"size":bytes.len()});
    let _guard = s.chat.active.lock().await;
    let mut thread = load(&s, &id)?;
    if let Some(old) = thread["upload_commands"].as_array().and_then(|rows| {
        rows.iter()
            .find(|r| r["command_id"] == command || r["idempotency_key"] == key)
    }) {
        if old["request"]["name"] != name || old["request"]["media_type"] != media
            || old["request"]["kind"] != kind {
            return Err(fail(
                StatusCode::CONFLICT,
                "conversation.upload.conflict",
                "Upload command was already used for different content",
            ));
        }
        let old_ref = old["response"]["receipt"]["output"]["artifact_id"].as_str().unwrap_or("");
        let (_, existing) = read_attachment(&s, &thread, old_ref)
            .map_err(|e| fail(StatusCode::CONFLICT, "conversation.upload.incomplete", &e))?;
        if existing != bytes {
            return Err(fail(StatusCode::CONFLICT,"conversation.upload.conflict",
                "Upload command was already used for different content"));
        }
        let mut response = old["response"].clone();
        response["receipt"]["deduplicated"] = json!(true);
        return Ok(Json(response));
    }
    let artifact_id = uuid::Uuid::new_v4().to_string();
    persist(&s, &artifact_id, &bytes).map_err(internal)?;
    if !thread["artifacts"].is_array() {
        thread["artifacts"] = json!([]);
    }
    let seq = thread["event_seq"].as_u64().unwrap_or(0) + 1;
    thread["event_seq"] = json!(seq);
    thread["artifacts"].as_array_mut().unwrap().push(json!({"artifact_id":artifact_id,"name":name,"media_type":media,"size":bytes.len(),"kind":kind,"event_id":uuid::Uuid::new_v4().to_string(),"stream_seq":seq,"created_at":chrono::Utc::now().to_rfc3339(),"turn_id":null}));
    let response = json!({"receipt":{"command_id":command,"command_type":"conversation.artifact.attach","state":"completed","aggregate":{"type":"thread","id":id},"output":{"artifact_id":artifact_id,"name":name,"media_type":media,"size":bytes.len(),"kind":kind},"deduplicated":false}});
    if !thread["upload_commands"].is_array() {
        thread["upload_commands"] = json!([]);
    }
    thread["upload_commands"].as_array_mut().unwrap().push(
        json!({"command_id":command,"idempotency_key":key,"request":request,"response":response}),
    );
    s.store
        .put("conversation_threads", &id, &thread)
        .map_err(internal)?;
    Ok(Json(response))
}
fn disposition(name: &str, download: bool) -> String {
    let name = name
        .chars()
        .filter(|c| !c.is_control() && !"<>:\"/\\|?*".contains(*c))
        .take(120)
        .collect::<String>();
    let fallback = name.chars().filter(|c| c.is_ascii()).collect::<String>();
    let encoded = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect::<String>();
    format!(
        "{}; filename=\"{}\"; filename*=UTF-8''{}",
        if download { "attachment" } else { "inline" },
        if fallback.is_empty() {
            "download"
        } else {
            &fallback
        },
        encoded
    )
}
fn raw(meta: &Value, bytes: Vec<u8>, download: bool, image: bool) -> Response {
    let media = meta["media_type"]
        .as_str()
        .unwrap_or("application/octet-stream");
    let inline = matches!(
        media,
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/bmp"
            | "image/avif"
            | "image/x-icon"
            | "application/pdf"
    ) || media.starts_with("text/plain");
    let content_type = if inline {
        media
    } else {
        "application/octet-stream"
    };
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    if let Ok(value) = HeaderValue::from_str(&disposition(
        meta["name"].as_str().unwrap_or("download"),
        download || !inline,
    )) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if image {
        if let Ok(value) = HeaderValue::from_str(media) {
            headers.insert("X-Muteki-Image-Media-Type", value);
        }
    }
    response
}
async fn artifact(
    State(s): State<Arc<AppState>>,
    Path((id, sha)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Failure> {
    let thread = load(&s, &id)?;
    if legacy_digest(&sha) && attachment_metadata(&thread, &sha).is_none() {
        return Err(fail(
            StatusCode::CONFLICT,
            "conversation.artifact.legacy_digest",
            "Legacy attachment has no usable artifact reference; select the original file again",
        ));
    }
    let (meta, bytes) = read_attachment(&s, &thread, &sha)
        .map_err(|e| fail(StatusCode::NOT_FOUND, "conversation.artifact.not_found", &e))?;
    Ok(raw(
        &meta,
        bytes,
        q.get("download").is_some_and(|v| v == "true" || v == "1"),
        false,
    ))
}
fn read_scoped_image(
    s: &AppState,
    id: &str,
    thread: &Value,
    raw_path: &str,
) -> Result<(Value, Vec<u8>), String> {
    if raw_path.is_empty() || raw_path.contains('\0') {
        return Err("Attachment not found in this thread".into());
    }
    let candidate = FsPath::new(raw_path);
    if candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err("Image path is outside allowed scope".into());
    }
    let visualization_root = crate::plugins::registry(s)
        .ok()
        .and_then(|registry| {
            let context = crate::plugins::visual_context(s, id, thread);
            registry.visualization_root(&context.scope).ok()
        })
        .and_then(|p| std::fs::canonicalize(p).ok());
    let workspace_root = crate::workspace_read_api::bound_root(s, id).ok();
    let captures_root = state_dir(s)
        .ok()
        .map(|d| d.join("browser-captures"))
        .and_then(|p| std::fs::canonicalize(p).ok());

    let computer_root = s.store.get("chat_computer_dirs", id).ok().flatten()
        .and_then(|row| row["id"].as_str().map(str::to_owned))
        .and_then(|id| state_dir(s).ok()?.join("computer-use").join(id).join("images").canonicalize().ok());
    let target = if candidate.is_absolute() {
        let resolved = std::fs::canonicalize(candidate)
            .map_err(|_| "Attachment not found in this thread".to_owned())?;
        let allowed = [
            visualization_root.as_deref(),
            workspace_root.as_deref(),
            captures_root.as_deref(),
            computer_root.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|root| resolved.starts_with(root));
        if !allowed {
            return Err("Image path is outside allowed scope".into());
        }
        resolved
    } else {
        [workspace_root.as_deref(), visualization_root.as_deref()]
            .into_iter()
            .flatten()
            .find_map(|root| {
                let resolved = std::fs::canonicalize(root.join(candidate)).ok()?;
                (resolved.starts_with(root) && resolved.is_file()).then_some(resolved)
            })
            .ok_or_else(|| "Attachment not found in this thread".to_owned())?
    };

    if !target.is_file() {
        return Err("Attachment bytes unavailable".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&target)
        .map_err(|_| "Attachment bytes unavailable".to_owned())?
        .take(MAX_SCOPED_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read image".to_owned())?;
    if bytes.len() > MAX_SCOPED_IMAGE_BYTES {
        return Err("Image exceeds size limit".into());
    }
    let is_svg = target
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    let media_type = match crate::image_validation::mime(&bytes) {
        Ok(mime) => mime,
        Err(_) if is_svg => "image/svg+xml",
        Err(err) => return Err(err),
    };
    let name = target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    Ok((json!({"name": name, "media_type": media_type}), bytes))
}

async fn image(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Failure> {
    let thread = load(&s, &id)?;
    let path = q.get("path").map(String::as_str).unwrap_or("");
    let sha = path.strip_prefix("artifact:").unwrap_or(path);
    if legacy_digest(sha) && attachment_metadata(&thread, sha).is_none() {
        return Err(fail(
            StatusCode::CONFLICT,
            "conversation.image.legacy_digest",
            "Legacy attachment has no usable artifact reference; select the original file again",
        ));
    }
    let (meta, bytes) = if valid_ref(sha) || legacy_digest(sha) {
        let (meta, bytes) = read_attachment(&s, &thread, sha)
            .map_err(|e| fail(StatusCode::NOT_FOUND, "conversation.image.not_found", &e))?;
        if !meta["media_type"]
            .as_str()
            .is_some_and(|m| m.starts_with("image/"))
        {
            return Err(fail(
                StatusCode::NOT_FOUND,
                "conversation.image.not_found",
                "Attachment is not an image",
            ));
        }
        (meta, bytes)
    } else {
        read_scoped_image(&s, &id, &thread, path)
            .map_err(|e| fail(StatusCode::NOT_FOUND, "conversation.image.not_found", &e))?
    };
    Ok(raw(&meta, bytes, false, true))
}
async fn recorded_tool_file(
    State(s): State<Arc<AppState>>,
    Path((id, turn, seq, index)): Path<(String, String, u64, usize)>,
) -> Result<Response, Failure> {
    load(&s, &id)?;
    let missing = |message: &str| fail(StatusCode::NOT_FOUND, "conversation.tool_file.not_found", message);
    let events = s.store.chat_events(&id, Some(&turn), seq.saturating_sub(1), seq, 1)
        .map_err(|error| internal(error.to_string()))?;
    let event = events.first().ok_or_else(|| missing("Recorded tool result not found"))?;
    let (meta, bytes) = crate::tool_resources::recorded(event, index).map_err(|error| missing(&error))?;
    Ok(raw(&meta, bytes, false, false))
}

fn select(value: &Value, fields: &[&str]) -> Value {
    let mut result = serde_json::Map::new();
    for key in fields {
        if let Some(v) = value.get(*key) {
            result.insert((*key).to_owned(), v.clone());
        }
    }
    Value::Object(result)
}
fn clean_paths(value: &mut Value) {
    match value {
        Value::String(s) => {
            let mut output = String::new();
            let mut token = String::new();
            let flush = |token: &mut String, output: &mut String| {
                let t = token.trim_start_matches(['\'', '"', '`', '(']);
                let absolute = t.starts_with('/')
                    || t.starts_with("\\\\")
                    || (t.as_bytes().get(1) == Some(&b':')
                        && matches!(t.as_bytes().get(2), Some(b'/') | Some(b'\\')));
                output.push_str(if absolute { "<path>" } else { token.as_str() });
                token.clear();
            };
            for ch in s.chars() {
                if ch.is_whitespace() {
                    flush(&mut token, &mut output);
                    output.push(ch);
                } else {
                    token.push(ch);
                }
            }
            flush(&mut token, &mut output);
            *s = output;
        }
        Value::Array(a) => a.iter_mut().for_each(clean_paths),
        Value::Object(o) => o.values_mut().for_each(clean_paths),
        _ => {}
    }
}
async fn export(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Failure> {
    let format = q.get("format").map(String::as_str).unwrap_or("markdown");
    if !matches!(format, "markdown" | "jsonl") {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.export.format",
            "Export format must be markdown or jsonl",
        ));
    }
    let exclude_paths = q
        .get("exclude_paths")
        .is_some_and(|v| v == "1" || v == "true");
    let exclude_tools = q
        .get("exclude_tools")
        .is_some_and(|v| v == "1" || v == "true");
    let snapshot = {
        let _guard = s.chat.active.lock().await;
        load(&s, &id)?
    };
    let mut turns = snapshot["turns"].as_array().cloned().ok_or_else(|| {
        fail(
            StatusCode::CONFLICT,
            "conversation.export.incomplete",
            "Saved turns are unavailable",
        )
    })?;
    turns.retain(|v| v["status"] != "superseded" && v["superseded_by_turn_id"].is_null());
    let all_turns = snapshot["turns"].as_array().unwrap();
    let mut messages = snapshot["messages"].as_array().cloned().ok_or_else(|| {
        fail(
            StatusCode::CONFLICT,
            "conversation.export.incomplete",
            "Saved messages are unavailable",
        )
    })?;
    messages.retain(|m| {
        m["turn_id"].is_null()
            || !all_turns.iter().any(|t| t["turn_id"] == m["turn_id"])
            || turns.iter().any(|t| t["turn_id"] == m["turn_id"])
    });
    let mut records = Vec::new();
    for turn in turns.iter() {
        records.push(select(
            turn,
            &[
                "turn_id",
                "seq",
                "kind",
                "status",
                "text",
                "attachments",
                "created_at",
                "completed_at",
                "error",
            ],
        ));
    }
    let turns = records;
    let mut messages = messages
        .iter()
        .map(|m| {
            let mut row = select(
                m,
                &[
                    "message_id",
                    "turn_id",
                    "role",
                    "kind",
                    "text",
                    "status",
                    "attachments",
                    "stream_seq",
                    "created_at",
                    "source_provider",
                    "source_role",
                    "source_content",
                ],
            );
            let turn = turns.iter().find(|turn| turn["turn_id"] == m["turn_id"]);
            row["turn_kind"] = turn
                .and_then(|turn| turn.get("kind"))
                .cloned()
                .unwrap_or(json!("history"));
            row["turn_status"] = turn
                .and_then(|turn| turn.get("status"))
                .cloned()
                .unwrap_or(json!("not_available"));
            if exclude_tools && (m["role"] == "tool" || m["kind"] == "imported_tool") {
                row.as_object_mut().unwrap().remove("source_content");
            }
            row
        })
        .collect::<Vec<_>>();
    let mut artifacts = snapshot["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|a| {
            let mut item = select(
                a,
                &[
                    "artifact_id",
                    "sha256",
                    "name",
                    "kind",
                    "media_type",
                    "size",
                    "turn_id",
                    "event_id",
                    "stream_seq",
                ],
            );
            if item["artifact_id"].as_str().is_none() && item["sha256"].is_string() {
                item["needs_reselect"] = json!(true);
            }
            item
        })
        .collect::<Vec<_>>();
    artifacts
        .retain(|a| a["turn_id"].is_null() || turns.iter().any(|t| t["turn_id"] == a["turn_id"]));
    let runtime = select(
        &snapshot["runtime"],
        &[
            "adapter_id",
            "instance_id",
            "model",
            "mode",
            "reasoning_effort",
        ],
    );
    let mut header = json!({"record":"header","schema_version":3,"title":snapshot["thread"]["title"],"thread_id":id,"model":runtime["model"],"current_runtime":runtime,"exported_at":chrono::Utc::now().to_rfc3339(),"options":{"exclude_tools":exclude_tools,"exclude_paths":exclude_paths},"path_redaction":{"enabled":exclude_paths,"mode":"lexical_best_effort"},"binary_content_included":false});
    let watermark = snapshot["event_seq"].as_u64().unwrap_or(0);
    let mut tools = snapshot["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|t| {
            select(
                t,
                if exclude_tools {
                    &[
                        "tool_id",
                        "turn_id",
                        "name",
                        "status",
                        "output",
                        "summary",
                        "output_summary",
                        "output_complete",
                        "error",
                        "source_events",
                    ][..]
                } else {
                    &[
                        "tool_id",
                        "turn_id",
                        "name",
                        "status",
                        "arguments",
                        "output",
                        "summary",
                        "output_summary",
                        "output_complete",
                        "error",
                        "source_events",
                    ][..]
                },
            )
        })
        .collect::<Vec<_>>();
    tools.retain(|t| {
        t["turn_id"].is_null()
            || !all_turns.iter().any(|turn| turn["turn_id"] == t["turn_id"])
            || turns.iter().any(|turn| turn["turn_id"] == t["turn_id"])
    });
    let metadata = json!({"complete":true,"schema_version":3,"scope":"current_branch","binary_content_included":false,"message_count":messages.len(),"turn_count":turns.len(),"artifact_count":artifacts.len(),"artifact_count_kind":"associations","tool_count":tools.len(),"watermark":watermark,"path_redaction":if exclude_paths{json!("lexical_best_effort")}else{Value::Null}});
    for (k, v) in metadata.as_object().unwrap() {
        if k != "path_redaction" {
            header[k] = v.clone();
        }
    }
    header["event_source"] =
        json!({"aggregate_type":"thread","aggregate_id":id,"watermark":watermark});
    let mut turns = turns;
    let mut tools = tools;
    if exclude_paths {
        clean_paths(&mut header);
        turns.iter_mut().for_each(clean_paths);
        messages.iter_mut().for_each(clean_paths);
        artifacts.iter_mut().for_each(clean_paths);
        tools.iter_mut().for_each(clean_paths);
    }
    let title = if exclude_paths {
        "conversation"
    } else {
        header["title"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or("Conversation")
    };
    let (content, media, ext) = if format == "jsonl" {
        let mut lines = vec![header.to_string()];
        for (kind, rows) in [
            ("turn", &turns),
            ("message", &messages),
            ("artifact", &artifacts),
            ("tool", &tools),
        ] {
            for row in rows {
                let mut row = row.clone();
                row["record"] = json!(kind);
                row["schema_version"] = json!(3);
                lines.push(row.to_string());
            }
        }
        (
            format!("{}\n", lines.join("\n")),
            "application/x-ndjson",
            "jsonl",
        )
    } else {
        let mut lines = vec![
            format!("# {title}"),
            format!("> Thread: {id} | Watermark: #{watermark}"),
            format!(
                "> Current branch: {} messages, {} turns. Binary attachments are indexed only.",
                messages.len(),
                turns.len()
            ),
            String::new(),
        ];
        if exclude_paths {
            lines.push("> Paths use best-effort lexical redaction. Review before sharing.".into());
        }
        for message in &messages {
            let text = message["text"].as_str().unwrap_or("");
            let role = message["role"].as_str().unwrap_or("message");
            lines.extend([
                format!("## {role}"),
                String::new(),
                if text.is_empty() && role == "user" {
                    "(Attachments or context only)".into()
                } else {
                    text.into()
                },
                String::new(),
            ]);
        }
        if !artifacts.is_empty() {
            lines.extend(["## Attachments".into(), String::new()]);
        }
        for artifact in &artifacts {
            lines.push(format!(
                "- {} (`{}`, {} bytes)",
                artifact["name"].as_str().unwrap_or("Attachment"),
                artifact["artifact_id"].as_str()
                    .unwrap_or("legacy: select original file again"),
                artifact["size"]
            ));
        }
        if !tools.is_empty() {
            lines.extend([String::new(), "## Tools".into()]);
        }
        for tool in &tools {
            lines.extend([
                format!("### {}", tool["name"].as_str().unwrap_or("Tool")),
                tool.to_string(),
                String::new(),
            ]);
        }
        (lines.join("\n"), "text/markdown; charset=utf-8", "md")
    };
    let filename = format!("{}-{}.{}", title, chrono::Utc::now().format("%Y%m%d"), ext);
    let mut response = content.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(media));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition(&filename, true)).map_err(internal)?,
    );
    headers.insert(
        "X-Muteki-Export-Metadata",
        HeaderValue::from_str(&metadata.to_string()).map_err(internal)?,
    );
    headers.insert(
        "Access-Control-Expose-Headers",
        HeaderValue::from_static("Content-Disposition, X-Muteki-Export-Metadata"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;

    fn state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(dir.path())).unwrap();
        (dir, AppState::new(store))
    }

    #[test]
    fn missing_attachment_integrity_requires_reselect() {
        let (dir, state) = state();
        let reference = "91ed97b4-dd4c-4805-8e6f-cdac556d14ed";
        let bytes = b"attachment-bytes";
        let uploads = dir.path().join("uploads");
        std::fs::create_dir_all(&uploads).unwrap();
        std::fs::write(uploads.join(reference), bytes).unwrap();
        let thread = json!({"artifacts":[{"artifact_id":reference,"name":"page.js","size":bytes.len(),"media_type":"application/javascript"}]});
        let error = read_attachment(&state, &thread, reference).unwrap_err();
        assert!(error.contains("select the original file again"), "{error}");
        assert!(state.store.get("attachment_integrity", reference).unwrap().is_none());

        state.store.put("attachment_integrity", reference, &json!({"sha256":format!("{:x}", Sha256::digest(b"different-same-length")),"size":bytes.len(),"basis":"upload"})).unwrap();
        let wrong = read_attachment(&state, &thread, reference).unwrap_err();
        assert!(wrong.contains("digest does not match"), "{wrong}");

        let digest = format!("{:x}", Sha256::digest(bytes));
        state.store.put("attachment_integrity", reference, &json!({"sha256":digest,"size":bytes.len(),"basis":"upload"})).unwrap();
        let mut sized = thread.clone();
        sized["artifacts"][0]["size"] = json!(1);
        let length = read_attachment(&state, &sized, reference).unwrap_err();
        assert!(length.contains("size does not match"), "{length}");

        let (meta, read) = read_attachment(&state, &thread, reference).unwrap();
        assert_eq!(read, bytes);
        assert_eq!(meta["sha256"], digest);
        assert_eq!(state.store.get("attachment_integrity", reference).unwrap().unwrap()["basis"], "upload");
    }
}
