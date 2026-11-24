//! Mac computer control owned by a chat Thread. The installed CUA MCP service
//! supplies the JavaScript kernel; Muteki owns its lifetime and saved results.

use crate::{AppState, plugins::mcp_client::Client};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest,Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const COMPUTER_SERVER: &str = "cua_repl";
const RESULT_PAGE_BYTES: usize = 4096;
const DROID_DELIVERY_BYTES: usize = 30_000;
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REAP_INTERVAL: Duration = Duration::from_secs(60);
const MAX_EVIDENCE_BYTES: usize = 128*1024*1024;
const CATALOG_RETRY: Duration = Duration::from_secs(60);
pub const STATUS_TOOL: &str = "chat_computer_status";
const SERVICE_SOCKET: &str = "Library/Group Containers/2DC432GLL2.com.openai.sky.CUAService/IPC/computeruse.sock";
const JS_DESCRIPTION: &str = "Control native macOS apps on this Mac by reading or operating their UI (Computer Use). For web pages, follow the user's choice of available browser tool or skill. Muteki's chat_browser_* tools are the default when the user has not specified one. Prefer purpose-built skills, connectors, APIs or CLIs when available.\n\nOn the first invocation, or after a reset, execute exactly one of the API calls below, optionally assigning its result to a variable. Do not add other API calls, waits or snapshots to that invocation.\n\nInventory of running apps and windows:\n\n```javascript\nawait cua.getState();\n```\n\nA specific app, by name, bundle ID or path:\n\n```javascript\nlet app = await cua.getApp(\"Example App\");\n```\n\ngetApp may launch an app. Use getState/listApps for passive inventory, and do not retry a missing or invalid app path.\n\nThe tool result includes documentation and, when selecting an app, its initial UI state. Read that result before continuing. Use only APIs described in the tool instructions or returned documentation. To add other content to the result use nodeRepl.write(value) for text and await mutekiComputer.emitImage(bytes) for images; the APIs above already display their documentation or UI state. If your context begins with a summary of an existing computer use task, call await cua.rewriteDocumentation() before continuing.";

fn sha256(bytes: &[u8]) -> String { format!("{:x}",Sha256::digest(bytes)) }

fn evidence_file(dir: &Path,folder: &str,name: &str,limit: usize) -> Result<Vec<u8>,Value> {
    use std::io::Read;
    let invalid = ||error("chat.computer.integrity_failed","已保存证据的路径或完整性无效");
    if dir.is_symlink() || name.contains(['/', '\\']) || !matches!(folder,"images" | "results") { return Err(invalid()); }
    let parent = dir.join(folder);
    if parent.is_symlink() { return Err(invalid()); }
    let path = parent.join(name);
    let canonical_dir = dir.canonicalize().map_err(|_|invalid())?;
    if !parent.canonicalize().map_err(|_|invalid())?.starts_with(canonical_dir) { return Err(invalid()); }
    let mut options = std::fs::OpenOptions::new(); options.read(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK); }
    let file = options.open(&path).map_err(|_|invalid())?;
    let metadata = file.metadata().map_err(|_|invalid())?;
    if !metadata.is_file() || metadata.len()>limit as u64 { return Err(invalid()); }
    let mut bytes=Vec::new();file.take(limit as u64+1).read_to_end(&mut bytes).map_err(|_|invalid())?;
    if bytes.len()>limit { return Err(invalid()); } Ok(bytes)
}

fn save_evidence(dir: &Path,reference: &str,document: &Value) -> Result<(),Value> {
    let raw = serde_json::to_vec(document).map_err(|_|error("chat.computer.evidence_failed","完整证据无法编码"))?;
    if raw.len()>MAX_EVIDENCE_BYTES { return Err(error("chat.computer.evidence_failed","完整证据超出保存限额；电脑动作已执行，请检查当前状态")); }
    let body = serde_json::to_vec(&document["result"]).map_err(|_|error("chat.computer.evidence_failed","结果无法编码"))?;
    let hashes: Vec<String> = pages(&body).into_iter().map(sha256).collect();
    let manifest=json!({"schema_version":1,"ref":reference,"bytes":raw.len(),"sha256":sha256(&raw),
        "result_bytes":body.len(),"result_sha256":sha256(&body),"page_sha256":hashes});
    write_atomic_0600(&dir.join("results").join(format!("{reference}.json")),&raw)
        .and_then(|_|write_atomic_0600(&dir.join("results").join(format!("{reference}.manifest.json")),&serde_json::to_vec(&manifest).unwrap()))
        .map_err(|_|error("chat.computer.evidence_failed","电脑操作已执行，但完整证据保存失败；不要重复执行"))
}

pub struct Outcome {
    pub result: Value,
    pub images: Vec<Value>,
    pub is_error: bool,
    /// The CUA tool surface (`_meta["codex/toolSurface"]`): which app the call drove.
    pub surface: Value,
}

struct Installed {
    manifest: PathBuf,
    config: Value,
    tools: HashSet<String>,
    startup: Duration,
}

struct Session {
    client: Option<Arc<Client>>,
    manifest: PathBuf,
    turn: Option<String>,
    dir: PathBuf,
    busy: usize,
    last_used: Instant,
}

#[derive(Default)]
struct Inner {
    owner: Option<(String, String)>,
    aborted: HashSet<(String, String)>,
    sessions: HashMap<String, Arc<Mutex<Session>>>,
    active_clients: HashMap<String, Arc<Client>>,
    reaper: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Default)]
pub struct Broker {
    inner: Mutex<Inner>,
    actions: Mutex<()>,
    catalog: Mutex<Option<(PathBuf, Vec<Value>)>>,
    catalog_failure: Mutex<Option<(PathBuf, Instant, Value)>>,
}

fn error(code: &str, message: impl Into<String>) -> Value {
    json!({"code":code,"message":message.into()})
}

pub fn host_instructions() -> String {
    format!(
        "通过 muteki-computer 操作原生 Mac 应用（Finder、系统设置、桌面软件等）时，先调用 chat_computer_status 查看服务就绪与占用情况，再使用 chat_computer_js 或 chat_computer_js_reset。网页与本地 Web 服务按用户指定的可用浏览器工具或技能处理，未指定时优先用 chat_browser_*。用法、授权和证据读取规则见各工具的描述。\n\n{}",
        include_str!("../assets/computer_confirmation.md").trim()
    )
}

fn service_socket() -> Value {
    let Some(home) = std::env::var_os("HOME") else { return json!({"present":false,"reason":"HOME 未设置"}) };
    let path = PathBuf::from(home).join(SERVICE_SOCKET);
    if !path.exists() {
        return json!({"path":path,"present":false,
            "note":"Codex Computer Use 服务尚未运行；首次 chat_computer_js 调用会由插件启动它"});
    }
    #[cfg(unix)]
    let probe = match std::os::unix::net::UnixStream::connect(&path) {
        Ok(_) => json!({"path":path,"present":true,"connectable":true}),
        Err(error) => json!({"path":path,"present":true,"connectable":false,"error":error.to_string()}),
    };
    #[cfg(not(unix))]
    let probe = json!({"path":path,"present":true});
    probe
}

pub fn status_tool() -> Value {
    json!({"name":STATUS_TOOL,
        "description":"Check whether Muteki can control native Mac apps for this chat right now, without touching the UI: plugin and runtime installation, the Computer Use service, the loaded chat_computer_js tools, whether another chat currently owns the Mac, and the next step when something is missing. Call it before the first chat_computer_js call and after a computer error.",
        "input_schema":{"type":"object","properties":{},"additionalProperties":false}})
}

fn write_atomic_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let write = || -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&temp, path)?;
        Ok(())
    };
    let res = write();
    if res.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    res
}

fn installed() -> Result<Installed, Value> {
    if !cfg!(target_os = "macos") {
        return Err(error("chat.computer.unsupported", "电脑操控只支持 macOS 主机"));
    }
    let home = std::env::var_os("CODEX_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".codex")))
        .ok_or_else(|| error("chat.computer.plugin_missing", "本机没有 Codex Computer Use 插件"))?;
    let root = home.join("plugins/cache/openai-bundled/unified-computer-use");
    let mut manifests: Vec<(Vec<u64>, PathBuf)> = std::fs::read_dir(root).ok().into_iter().flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join(".mcp.json").is_file())
        .map(|path| {
            let version = path.file_name().unwrap_or_default().to_string_lossy().split('.')
                .map(|part| part.parse::<u64>().unwrap_or(0)).collect();
            (version, path.join(".mcp.json"))
        }).collect();
    manifests.sort_by(|a, b| a.0.cmp(&b.0));
    let manifest = manifests.pop().map(|(_, path)| path)
        .ok_or_else(|| error("chat.computer.plugin_missing", "本机没有安装 Codex Computer Use 插件"))?;
    let raw: Value = serde_json::from_slice(&std::fs::read(&manifest)
        .map_err(|_| error("chat.computer.plugin_invalid", "电脑操控插件配置无法读取"))?)
        .map_err(|_| error("chat.computer.plugin_invalid", "电脑操控插件配置不是有效 JSON"))?;
    let mut config = raw["mcpServers"][COMPUTER_SERVER].clone();
    if !config.is_object() || config["enabled"] == false {
        return Err(error("chat.computer.plugin_disabled", "电脑操控插件未启用"));
    }
    let command = config["command"].as_str().unwrap_or("").to_owned();
    if !Path::new(&command).is_file() {
        return Err(error("chat.computer.runtime_missing", "电脑操控运行时不存在"));
    }
    let launcher = config["env"]["CODEX_CLI_PATH"].as_str().unwrap_or("").to_owned();
    if !Path::new(&launcher).is_file() {
        return Err(error("chat.computer.launcher_missing", "电脑操控插件缺少 ChatGPT 自带的 Codex CLI，无法通过 Computer Use 服务的进程认证"));
    }
    let mut argv = vec![json!("sandbox"), json!("-c"), json!("sandbox_mode=\"danger-full-access\""), json!("--"), json!(command)];
    argv.extend(config["args"].as_array().cloned().unwrap_or_default());
    config["command"] = json!(launcher);
    config["args"] = json!(argv);
    let tools = config["enabled_tools"].as_array().into_iter().flatten()
        .filter_map(Value::as_str).map(str::to_owned).collect();
    if !config["env"].is_object() { config["env"] = json!({}); }
    config["env"]["NODE_REPL_JS_BANNER"] = json!(include_str!("../assets/computer_bootstrap.js"));
    config["env"]["CUA_REPL_ENABLED_SURFACES"] = json!("computer");
    if let Some(raw) = config["env"]["NODE_REPL_TOOL_OVERRIDES"].as_str()
        && let Ok(mut overrides) = serde_json::from_str::<Value>(raw)
        && overrides["tools"]["js"].is_object()
    {
        overrides["tools"]["js"]["description"] = json!(JS_DESCRIPTION);
        overrides["server_instructions"] = json!("Native macOS app automation through the initialized cua API.");
        config["env"]["NODE_REPL_TOOL_OVERRIDES"] = json!(overrides.to_string());
    }
    let startup = Duration::from_secs_f64(config["startup_timeout_sec"].as_f64().unwrap_or(120.0).max(1.0));
    Ok(Installed { manifest, config, tools, startup })
}

pub fn status() -> Value {
    match installed() {
        Ok(plugin) => json!({"available":true,"version":plugin.manifest.parent()
            .and_then(Path::file_name).unwrap_or_default().to_string_lossy(),
            "tools":plugin.tools.iter().filter(|name| name.as_str() != "turn_ended").count()}),
        Err(reason) => json!({"available":false,"code":reason["code"],"message":reason["message"],"tools":0}),
    }
}

pub fn handles(name: &str) -> bool {
    matches!(name, "chat_computer_js" | "chat_computer_js_reset")
}

pub fn tools() -> Vec<Value> {
    let Ok(plugin) = installed() else { return Vec::new() };
    let mut rows = Vec::new();
    if plugin.tools.contains("js") {
        rows.push(json!({"name":"chat_computer_js","description":
            "Muteki's chat-owned Mac computer JavaScript session. Provide exactly one of code, read_result, or completion_summary. Use completion_summary for the plugin's required UI completion call. Use app.typeText for Unicode and pressKey for shortcuts. Manual images use await mutekiComputer.emitImage(bytes). Full results and images are saved under opaque references; read_result:{ref,page} reads pages without repeating UI actions.",
            "input_schema":{"type":"object","properties":{
                "code":{"type":"string"},"completion_summary":{"type":"string","description":"A short headline in three to six words, starting with a past-tense verb; no code or detailed explanation."},"title":{"type":"string"},
                "timeout_ms":{"type":"integer","minimum":1},
                "read_result":{"type":"object","properties":{"ref":{"type":"string"},"page":{"type":"integer","minimum":0}},
                    "required":["ref","page"],"additionalProperties":false}},"additionalProperties":false}}));
    }
    if plugin.tools.contains("js_reset") {
        rows.push(json!({"name":"chat_computer_js_reset","description":
            "Reset this chat's Mac computer JavaScript kernel. Reacquire app or tab bindings before UI actions.",
            "input_schema":{"type":"object","properties":{},"additionalProperties":false}}));
    }
    rows
}

fn thread_dir(s: &AppState, thread_id: &str) -> Result<PathBuf, Value> {
    let collection = "chat_computer_dirs";
    let prior = s.store.get(collection, thread_id)
        .map_err(|_| error("chat.computer.state_unavailable", "电脑操控状态不可用"))?;
    let id = if let Some(id) = prior.as_ref().and_then(|row| row["id"].as_str()) {
        id.to_owned()
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        let value = json!({"id":id.clone()});
        if s.store.put_many_if(&[(collection,thread_id,prior.as_ref())],&[(collection,thread_id,&value)])
            .map_err(|_| error("chat.computer.state_unavailable", "电脑操控状态不可用"))? {
            id
        } else {
            s.store.get(collection,thread_id).ok().flatten()
                .and_then(|row| row["id"].as_str().map(str::to_owned))
                .ok_or_else(|| error("chat.computer.state_unavailable", "电脑操控状态不可用"))?
        }
    };
    let dir = crate::native_chat::state_dir(s).join("computer-use").join(id);
    std::fs::create_dir_all(&dir).map_err(|_| error("chat.computer.state_unavailable", "电脑操控状态目录不可用"))?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| error("chat.computer.state_unavailable", "电脑操控状态目录不可用"))?;
    }
    Ok(dir)
}

fn metadata(thread_id: &str, turn_id: &str, call_id: &str) -> Value {
    json!({"x-codex-turn-metadata":{"session_id":thread_id,"turn_id":turn_id,"call_id":call_id},
        "openai/confirmation_policies":{"computer_use":include_str!("../assets/computer_confirmation.md")}})
}

async fn ensure(session: &mut Session, plugin: &Installed) -> Result<Arc<Client>, Value> {
    if session.manifest != plugin.manifest || session.client.as_ref().is_none_or(|client| !client.is_alive()) {
        if let Some(old) = session.client.take() { old.stop().await; }
        let package = plugin.manifest.parent().unwrap_or(Path::new("."));
        let client = Client::connect_host(&plugin.config, package, &session.dir, plugin.startup).await
            .map_err(|reason| {
                let log = session.dir.join("mcp-stderr.log");
                tracing::warn!(code = %reason.code, message = %reason.message, stderr_log = %log.display(),
                    "Computer Use session failed to start");
                let mut failure = error(&reason.code, reason.message);
                failure["stderr_log"] = json!(log);
                failure
            })?;
        session.client = Some(Arc::new(client));
        session.manifest = plugin.manifest.clone();
        session.turn = None;
    }
    Ok(session.client.as_ref().unwrap().clone())
}

async fn call_tool(client: &Client, name: &str, arguments: Value, meta: Value, timeout: Duration) -> Result<Value, Value> {
    client.request_with_timeout("tools/call", json!({"name":name,"arguments":arguments,"_meta":meta}), timeout).await
        .map_err(|reason| error(&reason.code, reason.message))
}

fn persist_result(dir: &Path, thread_id: &str, turn_id: &str, call_id: &str, tool: &str,
    arguments: &Value, result: &Value, image_input: bool) -> Result<(String, Value, Vec<Value>), Value> {
    let mut saved = result.clone();
    let mut delivered = Vec::new();
    let image_dir = dir.join("images");
    let mut content = Vec::new();
    for block in result["content"].as_array().into_iter().flatten() {
        if block["type"] != "image" {
            content.push(block.clone());
            continue;
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(block["data"].as_str().unwrap_or(""))
            .map_err(|_| error("chat.computer.image_invalid", "电脑操控返回的图片不是有效字节"))?;
        let mime = crate::image_validation::mime(&bytes).map_err(|_|error("chat.computer.image_invalid","电脑操控返回的图片解码失败"))?;
        if block["mimeType"].as_str().or(block["mime_type"].as_str()).is_some_and(|declared|declared!=mime) {
            return Err(error("chat.computer.image_invalid","图片内容与 MIME 不一致"));
        }
        let extension = match mime { "image/jpeg" => "jpg", "image/webp" => "webp", "image/gif" => "gif", "image/bmp"=>"bmp", "image/avif"=>"avif", "image/x-icon"=>"ico", _ => "png" };
        let id = format!("cimg-{}", uuid::Uuid::new_v4());
        let path = image_dir.join(format!("{id}.{extension}"));
        write_atomic_0600(&path, &bytes).map_err(|_| error("chat.computer.evidence_failed", "图片证据保存失败"))?;
        let mut entry = serde_json::Map::new();
        if let Some(extra) = block.as_object() {
            for (k, v) in extra {
                if !matches!(k.as_str(), "type" | "data" | "mimeType" | "mime_type") {
                    entry.insert(k.clone(), v.clone());
                }
            }
        }
        entry.insert("type".into(), json!("image"));
        entry.insert("artifact_id".into(), json!(id));
        entry.insert("path".into(), json!(path));
        entry.insert("mime_type".into(), json!(mime));
        entry.insert("bytes".into(), json!(bytes.len()));
        entry.insert("sha256".into(),json!(sha256(&bytes)));
        content.push(Value::Object(entry));
        if image_input {
            delivered.push(json!({"type":"image","data":block["data"],"mimeType":mime}));
        }
    }
    saved["content"] = json!(content);
    let ref_id = uuid::Uuid::new_v4().to_string();
    let document = json!({"thread_id":thread_id,"turn_id":turn_id,"invocation_id":call_id,
        "tool":tool,"arguments":arguments,"result":saved});
    save_evidence(dir,&ref_id,&document)?;
    Ok((ref_id, saved, delivered))
}

fn delivery_size(result: &Value, is_error: bool, call_id: &str) -> usize {
    let payload = json!({"ok": !is_error, "invocation_id": call_id, "result": result});
    let text = serde_json::to_string(&payload).unwrap_or_default();
    let envelope = json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": payload,
        "isError": is_error,
    });
    serde_json::to_vec(&envelope).map(|v| v.len()).unwrap_or(usize::MAX)
}

fn decorate_droid_result(
    mut result: Value,
    saved: &Value,
    ref_id: &str,
    call_id: &str,
    image_input: bool,
    is_error: bool,
    has_images: bool,
) -> Result<Value, Value> {
    let needs_paging = delivery_size(&result, is_error, call_id) > DROID_DELIVERY_BYTES;
    let needs_image_readback = is_error && has_images;
    if !needs_paging && !needs_image_readback {
        return Ok(result);
    }
    let body = serde_json::to_vec(saved).unwrap_or_default();
    let image_count = saved["content"]
        .as_array()
        .map(|items| items.iter().filter(|b| b["type"] == "image").count())
        .unwrap_or(0);
    let reference = json!({
        "ref": ref_id,
        "image_count": image_count,
        "bytes": body.len(),
        "sha256": sha256(&body),
        "page_sha256": pages(&body).into_iter().map(sha256).collect::<Vec<_>>(),
        "total_pages": pages(&body).len(),
        "page_max_bytes": RESULT_PAGE_BYTES,
        "format": "canonical.result JSON UTF-8",
    });
    result["full_result_reference"] = reference.clone();
    if needs_image_readback {
        result["image_readback"] = json!({
            "message": "Original execution remains failed. Droid may omit pixels on failed calls; read saved page 0 with chat_computer_js read_result to receive the original image blocks. No UI action or new screenshot occurs.",
            "read_result": {"ref": ref_id, "page": 0},
        });
    }
    if delivery_size(&result, is_error, call_id) > DROID_DELIVERY_BYTES {
        let mut content = vec![json!({
            "type": "text",
            "text": "完整电脑证据已保存。请用 chat_computer_js read_result:{ref,page} 逐页读取；不要重复电脑动作。",
        })];
        for block in saved["content"].as_array().into_iter().flatten() {
            if block["type"] == "image" {
                content.push(block.clone());
            }
        }
        let mut paged = json!({
            "artifact": result["artifact"],
            "model_image_input": image_input,
            "isError": is_error,
            "full_result_reference": reference.clone(),
            "content": content,
        });
        if needs_image_readback {
            paged["image_readback"] = result["image_readback"].clone();
        }
        result = paged;
    }
    if delivery_size(&result, is_error, call_id) > DROID_DELIVERY_BYTES {
        let mut minimal = json!({
            "artifact": result["artifact"],
            "model_image_input": image_input,
            "isError": is_error,
            "full_result_reference": reference.clone(),
            "content": [{"type": "text", "text": "完整电脑证据已保存。请用 chat_computer_js read_result:{ref,page} 逐页读取；不要重复电脑动作。"}],
        });
        if needs_image_readback {
            minimal["image_readback"] = result["image_readback"].clone();
        }
        result = minimal;
    }
    if delivery_size(&result, is_error, call_id) > DROID_DELIVERY_BYTES {
        return Err(error(
            "chat.computer.delivery_overflow",
            "完整证据已保存，但引用与图片元数据仍超出传输预算；不要重复电脑动作",
        ));
    }
    Ok(result)
}

fn pages(raw: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    let mut start = 0;
    while start < raw.len() {
        let mut end = (start + RESULT_PAGE_BYTES).min(raw.len());
        while end < raw.len() && raw[end] & 0xc0 == 0x80 { end -= 1; }
        result.push(&raw[start..end]);
        start = end;
    }
    result
}

fn read_result(dir: &Path, thread_id: &str, request: &Value, image_input: bool) -> Result<Outcome, Value> {
    let ref_id = request["ref"].as_str().ok_or_else(|| error("chat.computer.read_invalid", "read_result.ref 缺失"))?;
    let ref_id = uuid::Uuid::parse_str(ref_id).map_err(|_| error("chat.computer.read_invalid", "read_result.ref 无效"))?;
    let page = request["page"].as_u64().ok_or_else(|| error("chat.computer.read_invalid", "read_result.page 无效"))? as usize;
    let manifest: Value = serde_json::from_slice(&evidence_file(dir,"results",&format!("{ref_id}.manifest.json"),8*1024*1024)?)
        .map_err(|_|error("chat.computer.integrity_failed","证据完整性清单无效"))?;
    let raw = evidence_file(dir,"results",&format!("{ref_id}.json"),MAX_EVIDENCE_BYTES)?;
    if manifest["ref"] != ref_id.to_string() || manifest["bytes"].as_u64()!=Some(raw.len() as u64)
        || manifest["sha256"].as_str()!=Some(sha256(&raw).as_str()) {
        return Err(error("chat.computer.integrity_failed","完整电脑证据摘要或字节数不匹配"));
    }
    let saved: Value = serde_json::from_slice(&raw)
        .map_err(|_| error("chat.computer.read_failed", "完整电脑证据无法解析"))?;
    if saved["thread_id"] != thread_id { return Err(error("chat.computer.read_failed", "电脑证据不属于当前会话")); }
    let body = serde_json::to_vec(&saved["result"]).unwrap_or_default();
    let chunks = pages(&body);
    let data = chunks.get(page).ok_or_else(|| error("chat.computer.read_failed", "电脑证据页不存在"))?;
    let result_hash=sha256(&body);let page_hash=sha256(data);
    if manifest["result_bytes"].as_u64()!=Some(body.len() as u64) || manifest["result_sha256"]!=result_hash
        || manifest["page_sha256"][page]!=page_hash {
        return Err(error("chat.computer.integrity_failed","电脑证据结果或分页摘要不匹配"));
    }
    let mut images = Vec::new();
    if page == 0 && image_input {
        for block in saved["result"]["content"].as_array().into_iter().flatten().filter(|block| block["type"] == "image") {
            let id = block["artifact_id"].as_str().unwrap_or("");
            if id.strip_prefix("cimg-").is_none_or(|id|uuid::Uuid::parse_str(id).is_err()) {
                return Err(error("chat.computer.integrity_failed","图片证据 ID 无效"));
            }
            let mime = block["mime_type"].as_str().unwrap_or("image/png");
            let extension = match mime { "image/jpeg" => "jpg", "image/webp" => "webp", "image/gif" => "gif", "image/bmp"=>"bmp", "image/avif"=>"avif", "image/x-icon"=>"ico", _ => "png" };
            let expected = dir.join("images").join(format!("{id}.{extension}"));
            if block["path"].as_str()!=expected.to_str() { return Err(error("chat.computer.integrity_failed","图片证据路径不匹配")); }
            let bytes = evidence_file(dir,"images",&format!("{id}.{extension}"),MAX_EVIDENCE_BYTES)?;
            if block["bytes"].as_u64()!=Some(bytes.len() as u64) || block["sha256"]!=sha256(&bytes)
                || crate::image_validation::mime(&bytes).ok()!=Some(mime) {
                return Err(error("chat.computer.integrity_failed","图片证据的摘要、大小或格式不匹配"));
            }
            images.push(json!({"type":"image","mimeType":mime,
                "data":base64::engine::general_purpose::STANDARD.encode(bytes)}));
        }
    }
    let result = json!({"source":{"ref":ref_id,"turn_id":saved["turn_id"],"bytes":body.len(),
        "sha256":result_hash,"total_pages":chunks.len(),"original_isError":saved["result"]["isError"],
        "saved_images_replayed":images.len()},"page":page,"byte_offset":chunks[..page].iter().map(|bytes|bytes.len()).sum::<usize>(),
        "page_bytes":data.len(),"page_sha256":page_hash,"data":String::from_utf8_lossy(data),"isError":false});
    Ok(Outcome { result, images, is_error:false, surface: Value::Null })
}

fn persist_read_receipt(dir: &Path, thread_id: &str, turn_id: &str, call_id: &str,
    arguments: &Value, result: &Value) -> Result<Value, Value> {
    let ref_id = uuid::Uuid::new_v4().to_string();
    let folder = dir.join("results");
    let path = folder.join(format!("{ref_id}.json"));
    let document = json!({"thread_id":thread_id,"turn_id":turn_id,"invocation_id":call_id,
        "tool":"chat_computer_js","arguments":arguments,"result":result});
    save_evidence(dir,&ref_id,&document)?;
    Ok(json!({"ref":ref_id,"path":path}))
}

impl Broker {
    pub async fn live_tools(&self, s: &AppState) -> Vec<Value> {
        let Ok(plugin) = installed() else { return Vec::new() };
        let mut cached = self.catalog.lock().await;
        if let Some((_, rows)) = cached.as_ref().filter(|(manifest, _)| manifest == &plugin.manifest) {
            return rows.clone();
        }
        if self.catalog_failure.lock().await.as_ref()
            .is_some_and(|(manifest, at, _)| manifest == &plugin.manifest && at.elapsed() < CATALOG_RETRY) {
            return Vec::new();
        }
        let state = crate::native_chat::state_dir(s).join("computer-use/catalog");
        let package = plugin.manifest.parent().unwrap_or(Path::new("."));
        let log = state.join("mcp-stderr.log");
        let fail = |code: &str, message: &str| {
            tracing::warn!(code, message, stderr_log = %log.display(), manifest = %plugin.manifest.display(),
                "Computer Use catalog unavailable");
            json!({"code":code,"message":message,"stderr_log":log})
        };
        let client = match Client::connect_host(&plugin.config, package, &state, plugin.startup).await {
            Ok(client) => client,
            Err(reason) => {
                let failure = fail(&reason.code, &reason.message);
                *self.catalog_failure.lock().await = Some((plugin.manifest.clone(), Instant::now(), failure));
                return Vec::new();
            }
        };
        let listed = client.request("tools/list", json!({})).await;
        client.stop().await;
        let listed = match listed {
            Ok(listed) => listed,
            Err(reason) => {
                let failure = fail(&reason.code, &reason.message);
                *self.catalog_failure.lock().await = Some((plugin.manifest.clone(), Instant::now(), failure));
                return Vec::new();
            }
        };
        let mut rows = Vec::new();
        for tool in listed["tools"].as_array().into_iter().flatten() {
            let name = tool["name"].as_str().unwrap_or("");
            if !matches!(name, "js" | "js_reset") || !plugin.tools.contains(name)
                || tool["_meta"]["ui"]["visibility"] == json!([]) { continue; }
            let mut schema = tool["inputSchema"].clone();
            if name == "js" {
                if !schema.is_object() { schema = json!({}); }
                if !schema["properties"].is_object() { schema["properties"] = json!({}); }
                schema["properties"]["read_result"] = json!({"type":"object","properties":{
                    "ref":{"type":"string"},"page":{"type":"integer","minimum":0}},
                    "required":["ref","page"],"additionalProperties":false});
                if let Some(map) = schema.as_object_mut() {
                    if let Some(required) = map.get_mut("required").and_then(Value::as_array_mut) {
                        required.retain(|field| field != "code");
                    }
                    for key in ["oneOf","allOf","anyOf"] { map.remove(key); }
                }
                if schema["properties"]["completion_summary"].is_null() {
                    schema["properties"]["completion_summary"] = json!({"type":"string", "description":"A short headline in three to six words, starting with a past-tense verb; use without code or read_result."});
                }
                schema["type"] = json!("object");
            }
            let intro = "Muteki's session-owned Mac control tool for native apps. Use chat_computer_js / chat_computer_js_reset through the muteki-control gateway for native apps. For web pages, follow the user’s choice of available browser tool or skill, defaulting to chat_browser_* if they have no preference. A chat owns this Mac during its computer turn; chat_computer_status reports readiness and the current owner. On macOS, app.typeText pastes literal Unicode; use pressKey for shortcuts. For manual images use await mutekiComputer.emitImage(bytes). getApp may launch an app; use getState/listApps for inventory. Missing or invalid .app paths fail before native lookup; do not retry stale paths. Completion/reset release computer ownership and retire its activity previews. Computer previews reuse explicit screenshots and never launch apps in the background.";
            let extra = if name == "js" {
                " Provide exactly one of code, read_result, or completion_summary. Use completion_summary for the plugin's required UI completion call. Large results may return full_result_reference; read every page using read_result:{ref,page}, no code. References and images use opaque IDs with independent integrity metadata. Readback performs no UI action."
            } else { " Reset clears JavaScript bindings; reacquire app or tab before UI actions." };
            rows.push(json!({"name":format!("chat_computer_{name}"),
                "description":format!("{intro}\n\n{}{}",tool["description"].as_str().unwrap_or(""),extra),
                "input_schema":schema}));
        }
        if rows.is_empty() {
            let failure = fail("chat.computer.catalog_empty", "电脑操控服务没有列出 js/js_reset 工具");
            *self.catalog_failure.lock().await = Some((plugin.manifest.clone(), Instant::now(), failure));
            return rows;
        }
        *self.catalog_failure.lock().await = None;
        *cached = Some((plugin.manifest, rows.clone()));
        rows
    }

    /// Readiness for one thread without any UI action.
    pub async fn diagnose(&self, s: &AppState, thread_id: &str) -> Value {
        let plugin = status();
        let socket = service_socket();
        let tools: Vec<Value> = self.live_tools(s).await.iter().map(|row| row["name"].clone()).collect();
        let catalog_error = self.catalog_failure.lock().await.as_ref().map(|(_, at, failure)| {
            let mut failure = failure.clone();
            failure["age_s"] = json!(at.elapsed().as_secs());
            failure["retry_after_s"] = json!(CATALOG_RETRY.saturating_sub(at.elapsed()).as_secs());
            failure
        });
        let (owner, session) = {
            let inner = self.inner.lock().await;
            (inner.owner.clone(), inner.sessions.get(thread_id).cloned())
        };
        let busy_elsewhere = owner.as_ref().is_some_and(|(owner, _)| owner != thread_id);
        let session = match session {
            Some(session) => match session.try_lock() {
                Ok(session) => json!({"process_alive":session.client.as_ref().is_some_and(|client| client.is_alive()),
                    "busy":session.busy > 0,"idle_s":session.last_used.elapsed().as_secs(),
                    "stderr_log":session.dir.join("mcp-stderr.log")}),
                Err(_) => json!({"busy":true}),
            },
            None => json!({"process_alive":false,"note":"本聊天尚未启动电脑操控进程；首次 chat_computer_js 会启动"}),
        };
        let ready = plugin["available"] == true && tools.iter().any(|name| name == "chat_computer_js") && !busy_elsewhere;
        let next = if plugin["available"] != true {
            format!("{}：安装并启用 Codex 的 Computer Use 插件（ChatGPT 桌面应用）后重试", plugin["message"].as_str().unwrap_or("插件不可用"))
        } else if !tools.iter().any(|name| name == "chat_computer_js") {
            "电脑操控服务未能列出工具；查看 catalog_error 与 stderr_log，确认 macOS 已授予 Codex Computer Use 辅助功能与屏幕录制权限".to_owned()
        } else if busy_elsewhere {
            "另一聊天回合正在操控本机；等待它结束，或请用户停止那个回合".to_owned()
        } else {
            "可以调用 chat_computer_js：首次调用优先执行 await cua.getState()；getApp 可能启动应用，不用于被动检查".to_owned()
        };
        json!({"ready":ready,"plugin":plugin,"surfaces":"computer","service_socket":socket,"tools":tools,
            "catalog_error":catalog_error,"busy_elsewhere":busy_elsewhere,"session":session,"next_step":next})
    }

    async fn check_owner(&self, thread_id: &str, turn_id: &str) -> Result<(), Value> {
        let inner = self.inner.lock().await;
        let current = (thread_id.to_owned(), turn_id.to_owned());
        if inner.aborted.contains(&current) {
            return Err(error("chat.computer.turn_cancelled", "该聊天回合已停止"));
        }
        if inner.owner.as_ref().is_some_and(|owner| owner != &current) {
            return Err(error("chat.computer.busy", "本机电脑正由另一聊天回合操控"));
        }
        Ok(())
    }

    async fn claim(&self, thread_id: &str, turn_id: &str) -> Result<Arc<Mutex<Session>>, Value> {
        let mut inner = self.inner.lock().await;
        let current = (thread_id.to_owned(), turn_id.to_owned());
        if inner.aborted.contains(&current) {
            return Err(error("chat.computer.turn_cancelled", "该聊天回合已停止"));
        }
        if inner.owner.as_ref().is_some_and(|owner| owner != &current) {
            return Err(error("chat.computer.busy", "本机电脑正由另一聊天回合操控"));
        }
        inner.owner = Some(current);
        inner.sessions.get(thread_id).cloned()
            .ok_or_else(|| error("chat.computer.state_unavailable", "电脑操控会话未初始化"))
    }

    pub async fn reap_idle(&self, max_idle: Duration) -> usize {
        let candidates: Vec<(String, Arc<Mutex<Session>>)> = {
            let inner = self.inner.lock().await;
            let owner_tid = inner.owner.as_ref().map(|(tid, _)| tid.as_str());
            inner
                .sessions
                .iter()
                .filter(|(tid, _)| Some(tid.as_str()) != owner_tid)
                .map(|(tid, session)| (tid.clone(), session.clone()))
                .collect()
        };
        let mut reaped = 0;
        for (tid, session_arc) in candidates {
            let Ok(mut session) = session_arc.try_lock() else {
                continue;
            };
            if session.busy > 0 {
                continue;
            }
            let dead = session.client.as_ref().is_none_or(|c| !c.is_alive());
            let idle = session.last_used.elapsed() >= max_idle;
            if !dead && !idle {
                continue;
            }
            let removed = {
                let mut inner = self.inner.lock().await;
                if inner.owner.as_ref().is_some_and(|(o, _)| o == &tid) {
                    false
                } else if inner.sessions.get(&tid).is_some_and(|s| Arc::ptr_eq(s, &session_arc)) {
                    inner.sessions.remove(&tid);
                    inner.active_clients.remove(&tid);
                    true
                } else {
                    false
                }
            };
            if !removed {
                continue;
            }
            let ended_turn = session.turn.take();
            if let (Some(client), Some(turn_id)) = (session.client.as_ref(), ended_turn)
                && client.is_alive()
            {
                let _ = call_tool(
                    client,
                    "turn_ended",
                    json!({"hook_event_name":"Stop","session_id":tid,"turn_id":turn_id}),
                    Value::Null,
                    Duration::from_secs(10),
                )
                .await;
            }
            if let Some(client) = session.client.take() {
                client.stop().await;
            }
            reaped += 1;
        }
        reaped
    }

    pub async fn invoke(&self, s: &AppState, thread_id: &str, turn_id: &str, call_id: &str,
        name: &str, arguments: &Value, image_input: bool, droid: bool) -> Result<Outcome, Value> {
        let _ = self.reap_idle(IDLE_TIMEOUT).await;
        let tool = name.strip_prefix("chat_computer_").unwrap_or("");
        if !matches!(tool, "js" | "js_reset") {
            return Err(error("chat.computer.tool_unknown", "电脑操控工具不存在"));
        }
        let plugin = installed()?;
        if !plugin.tools.contains(tool) {
            return Err(error("chat.computer.tool_unknown", "已安装的电脑操控插件未提供该工具"));
        }
        if !arguments.is_object() {
            return Err(error("chat.computer.arguments_invalid", "电脑操控参数必须是对象"));
        }
        if tool == "js" {
            let code = arguments.get("code");
            let read = arguments.get("read_result");
            let summary = arguments.get("completion_summary");
            if [code.is_some(), read.is_some(), summary.is_some()].into_iter().filter(|present| *present).count() != 1
                || code.is_some_and(|value| !value.is_string()) || summary.is_some_and(|value| !value.is_string()) {
                return Err(error("chat.computer.arguments_invalid", "需要 code、read_result 或 completion_summary，且只能提供其中一种"));
            }
        }
        let dir = thread_dir(s, thread_id)?;
        {
            let mut inner = self.inner.lock().await;
            inner.sessions.entry(thread_id.to_owned()).or_insert_with(|| Arc::new(Mutex::new(Session {
                client: None,
                manifest: PathBuf::new(),
                turn: None,
                dir: dir.clone(),
                busy: 0,
                last_used: Instant::now(),
            })));
        }
        if tool == "js" && arguments.get("read_result").is_some() {
            self.check_owner(thread_id, turn_id).await?;
            let mut outcome = read_result(&dir, thread_id, &arguments["read_result"], image_input)?;
            let artifact = persist_read_receipt(&dir, thread_id, turn_id,
                call_id, arguments, &outcome.result)?;
            outcome.result["artifact"] = artifact;
            return Ok(outcome);
        }
        let _action = self.actions.lock().await;
        let session = self.claim(thread_id, turn_id).await?;
        let mut session = session.lock().await;
        session.busy += 1;
        session.last_used = Instant::now();
        let outcome = async {
            let client = ensure(&mut session, &plugin).await?;
            let aborted = {
                let mut inner = self.inner.lock().await;
                if inner.aborted.contains(&(thread_id.to_owned(), turn_id.to_owned())) {
                    true
                } else {
                    inner.active_clients.insert(thread_id.to_owned(), client.clone());
                    false
                }
            };
            if aborted {
                client.stop().await;
                session.client = None;
                session.turn = None;
                return Err(error("chat.computer.turn_cancelled", "该聊天回合已停止"));
            }
            if let Some(old_turn) = session.turn.as_deref().filter(|old| *old != turn_id) {
                let _ = call_tool(&client,"turn_ended",json!({"hook_event_name":"Stop",
                    "session_id":thread_id,"turn_id":old_turn}),Value::Null,Duration::from_secs(30)).await;
            }
            session.turn = Some(turn_id.to_owned());
            let mut request = arguments.clone();
            let mut epoch = String::new();
            if tool == "js" && arguments.get("code").is_some() {
                epoch = uuid::Uuid::new_v4().to_string();
                let code = arguments["code"].as_str().unwrap_or("");
                request["code"] = json!(format!("globalThis.__mutekiComputerImageInput = {image_input};\n\
                    globalThis.__mutekiComputerImages.begin({});\n{code}",serde_json::to_string(&epoch).unwrap()));
            }
            let timeout_ms = arguments["timeout_ms"].as_u64().unwrap_or(30_000);
            let timeout = Duration::from_millis(timeout_ms.saturating_add(30_000));
            let meta = metadata(thread_id, turn_id, call_id);
            let mut raw = match call_tool(&client, tool, request, meta.clone(), timeout).await {
                Ok(raw) => raw,
                Err(reason) => {
                    if matches!(reason["code"].as_str(), Some("worker_mcp.request_timeout" | "chat.computer.timeout")) {
                        client.stop().await;
                        session.client = None;
                        session.turn = None;
                        self.inner.lock().await.active_clients.remove(thread_id);
                        return Err(error(
                            "chat.computer.timeout",
                            format!("Codex Computer Use 在 {} 秒内没有返回，已重启该会话的进程", timeout.as_secs()),
                        ));
                    }
                    return Err(reason);
                }
            };
            if !epoch.is_empty() && raw["isError"] == true
                && !raw["content"].as_array().into_iter().flatten().any(|block|block["type"] == "image") {
                let recovery = call_tool(&client,"js",json!({"code":format!("await globalThis.__mutekiComputerImages.recover({});",serde_json::to_string(&epoch).unwrap()),
                    "title":"恢复本次调用中已返回的图片证据"}),meta,Duration::from_secs(30)).await;
                if let Ok(value) = recovery {
                    if let Some(images) = value["content"].as_array() {
                        raw["content"].as_array_mut().map(|content| content.extend(images.iter().filter(|block|block["type"] == "image").cloned()));
                    }
                }
            }
            let (ref_id, saved, images) = persist_result(&dir,thread_id,turn_id,call_id,name,arguments,&raw,image_input)?;
            let is_error = raw["isError"] == true;
            let has_images = !images.is_empty()
                || saved["content"].as_array().is_some_and(|c| c.iter().any(|b| b["type"] == "image"));
            let mut result = json!({"artifact":{"ref":ref_id,"path":dir.join("results").join(format!("{ref_id}.json"))},
                "model_image_input":image_input,"content":saved["content"],"isError":is_error});
            if !saved["structuredContent"].is_null() { result["structuredContent"] = saved["structuredContent"].clone(); }
            if droid {
                result = decorate_droid_result(result, &saved, &ref_id, call_id, image_input, is_error, has_images)?;
            }
            Ok(Outcome { result, images, is_error, surface: saved["_meta"]["codex/toolSurface"].clone() })
        }.await;
        session.busy = session.busy.saturating_sub(1);
        session.last_used = Instant::now();
        if (tool == "js_reset" || arguments.get("completion_summary").is_some())
            && outcome.as_ref().is_ok_and(|outcome| !outcome.is_error)
        {
            let ended = if let Some(client) = session.client.as_ref() {
                call_tool(client, "turn_ended", json!({"hook_event_name":"Stop",
                    "session_id":thread_id,"turn_id":turn_id}), Value::Null,
                    Duration::from_secs(10)).await.map(|_| ())
            } else { Ok(()) };
            session.turn = None;
            let mut inner = self.inner.lock().await;
            if inner.owner.as_ref() == Some(&(thread_id.to_owned(), turn_id.to_owned())) {
                inner.owner = None;
            }
            s.activity.clear_computer(thread_id, turn_id);
            ended?;
        }
        outcome
    }

    pub async fn finish_turn(&self, thread_id: &str, turn_id: &str) {
        let _action = self.actions.lock().await;
        let session = {
            let inner = self.inner.lock().await;
            if inner.owner.as_ref() != Some(&(thread_id.to_owned(),turn_id.to_owned())) { return; }
            inner.sessions.get(thread_id).cloned()
        };
        if let Some(session) = session {
            let mut session = session.lock().await;
            if let Some(client) = session.client.as_ref() {
                let _ = call_tool(client,"turn_ended",json!({"hook_event_name":"Stop",
                    "session_id":thread_id,"turn_id":turn_id}),Value::Null,Duration::from_secs(30)).await;
            }
            session.turn = None;
            session.last_used = Instant::now();
        }
        {
            let mut inner = self.inner.lock().await;
            if inner.owner.as_ref() == Some(&(thread_id.to_owned(),turn_id.to_owned())) { inner.owner = None; }
        }
        let _ = self.reap_idle(IDLE_TIMEOUT).await;
    }

    pub async fn abort_turn(&self, thread_id: &str, turn_id: &str) {
        let (client, session) = {
            let mut inner = self.inner.lock().await;
            inner.aborted.insert((thread_id.to_owned(),turn_id.to_owned()));
            if inner.owner.as_ref() == Some(&(thread_id.to_owned(),turn_id.to_owned())) {
                inner.owner = None;
                (inner.active_clients.remove(thread_id), inner.sessions.get(thread_id).cloned())
            } else { (None, None) }
        };
        if let Some(client) = client { client.stop().await; }
        if let Some(session) = session
            && let Ok(mut guard) = session.try_lock()
        {
            guard.client = None;
            guard.turn = None;
        }
    }

    pub async fn close(&self) {
        let (clients, reaper) = {
            let mut inner = self.inner.lock().await;
            inner.owner = None;
            inner.sessions.clear();
            let clients = inner.active_clients.drain().map(|(_, client)| client).collect::<Vec<_>>();
            (clients, inner.reaper.take())
        };
        if let Some(reaper) = reaper {
            reaper.abort();
        }
        for client in clients { client.stop().await; }
    }

    pub async fn spawn_reaper(self: &Arc<Self>) {
        let mut inner = self.inner.lock().await;
        if inner.reaper.as_ref().is_some_and(|task| !task.is_finished()) { return; }
        let weak = Arc::downgrade(self);
        inner.reaper = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(REAP_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(broker) = weak.upgrade() else { break };
                let _ = broker.reap_idle(IDLE_TIMEOUT).await;
            }
        }));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires the installed Mac Computer Use plugin"]
    async fn live_catalog_uses_the_installed_cua_tools() {
        let state_dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(state_dir.path().join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(state_dir.path())).unwrap();
        let state = AppState::new(store);
        let rows = state.computer.live_tools(&state).await;
        assert_eq!(rows.iter().filter(|row| row["name"] == "chat_computer_js").count(), 1);
        assert_eq!(rows.iter().filter(|row| row["name"] == "chat_computer_js_reset").count(), 1);
        let js = rows.iter().find(|row| row["name"] == "chat_computer_js").unwrap();
        assert!(js["description"].as_str().unwrap().contains("Computer Use"));
        assert!(js["input_schema"]["properties"]["read_result"].is_object());
        state.computer.close().await;
    }

    #[test]
    fn read_result_rejects_bad_digest_wrong_owner_and_escaping_path() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let thread = "thr-i134-owner";
        let reference = "11111111-1111-4111-8111-111111111111";
        let text = "page-marker ".repeat(2000);
        let document = json!({"thread_id":thread,"turn_id":"turn-i134","invocation_id":"call-i134",
            "tool":"chat_computer_js","arguments":{},"result":{"isError":false,"content":[{"type":"text","text":text}]}});
        save_evidence(root, reference, &document).unwrap();
        let request = json!({"ref":reference,"page":0});
        fn rejected<T>(result: Result<T, Value>) -> Value {
            match result {
                Ok(_) => panic!("expected the result page to be rejected"),
                Err(error) => error,
            }
        }
        let owner = rejected(read_result(root, "thr-i134-other", &request, false));
        assert_eq!(owner["code"], "chat.computer.read_failed");
        assert_eq!(owner["message"], "电脑证据不属于当前会话");
        let page = read_result(root, thread, &request, false).unwrap();
        assert!(page.result["source"]["total_pages"].as_u64().unwrap() > 1);
        assert_eq!(page.result["source"]["sha256"].as_str().unwrap().len(), 64);
        assert_eq!(page.result["page_sha256"].as_str().unwrap().len(), 64);
        assert_eq!(page.result["page"], 0);
        let stored = root.join("results").join(format!("{reference}.json"));
        let original = std::fs::read(&stored).unwrap();
        let outside = root.join("outside-result.json");
        std::fs::write(&outside, b"escaped").unwrap();
        std::fs::remove_file(&stored).unwrap();
        std::os::unix::fs::symlink(&outside, &stored).unwrap();
        let linked = rejected(read_result(root, thread, &request, false));
        assert_eq!(linked["code"], "chat.computer.integrity_failed");
        std::fs::remove_file(&stored).unwrap();
        std::fs::write(&stored, &original).unwrap();
        let mut tampered = original.clone();
        let index = tampered.len() / 2;
        tampered[index] ^= 0x01;
        std::fs::write(&stored, tampered).unwrap();
        let digest = rejected(read_result(root, thread, &request, false));
        assert_eq!(digest["code"], "chat.computer.integrity_failed");
        assert_eq!(digest["message"], "完整电脑证据摘要或字节数不匹配");
        let escaped = rejected(evidence_file(root, "results", "../outside-result.json", 1024));
        assert_eq!(escaped["code"], "chat.computer.integrity_failed");
        assert_eq!(escaped["message"], "已保存证据的路径或完整性无效");
    }
}
