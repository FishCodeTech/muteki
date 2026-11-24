//! `/api/capability`: the Muteki control MCP server an engine session reaches
//! over loopback with the bearer token its session was started with.
//!
//! The endpoint is stateless per request: the token names the session, the
//! session's binding names the tools, and every call runs the same handler
//! the workbench page calls. Two protocol eras are served, because the
//! engines in use still handshake with `initialize` (2025-03-26 through
//! 2025-11-25) while the current revision (2026-07-28) removed it.

use crate::{AppState, browser, capabilities, chat_computer, plugins};
use axum::{
    Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

pub const PROTOCOL_VERSION: &str = "2026-07-28";
const LEGACY_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const SERVER_NAME: &str = "muteki-control";
const SERVER_VERSION: &str = "1.0.0";
pub const PLATFORM_DISCOVER_TOOL: &str = "muteki_platform_discover";
pub const PLATFORM_CALL_TOOL: &str = "muteki_platform_call";
const COLLAB_DISCOVER_TOOL: &str = "muteki_collab_discover";
const COLLAB_CALL_TOOL: &str = "muteki_collab_call";
const TOOLS_TTL_MS: u64 = 5000;
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const HEADER_MISMATCH: i64 = -32020;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// The engine session a bearer token belongs to.
#[derive(Clone)]
struct Grant {
    owner: String,
    engine: String,
    binding_revision: Option<String>,
    instructions: Option<String>,
    primary_surface: Option<String>,
}

/// Live gateway credentials. A token lives as long as the session that was
/// started with it, so it is kept in this process only.
#[derive(Default)]
pub struct Gateway {
    tokens: Mutex<HashMap<String, Grant>>,
    searches: Mutex<HashMap<SearchKey, Arc<Notify>>>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct SearchKey {
    token: String,
    surface: Option<String>,
    request_id: String,
}

impl SearchKey {
    fn new(token: &str, surface: Option<&str>, request_id: &Value) -> Self {
        Self {
            token: token.to_owned(),
            surface: surface.map(str::to_owned),
            request_id: request_id.to_string(),
        }
    }
}

struct ActiveSearch<'a> {
    gateway: &'a Gateway,
    key: SearchKey,
    cancelled: Arc<Notify>,
}

impl Drop for ActiveSearch<'_> {
    fn drop(&mut self) {
        self.gateway.searches.lock().unwrap().remove(&self.key);
    }
}

impl Gateway {
    fn begin_search(&self, key: SearchKey) -> Option<ActiveSearch<'_>> {
        let mut searches = self.searches.lock().unwrap();
        let cancelled = Arc::new(Notify::new());
        let std::collections::hash_map::Entry::Vacant(entry) = searches.entry(key.clone()) else {
            return None;
        };
        entry.insert(cancelled.clone());
        Some(ActiveSearch {
            gateway: self,
            key,
            cancelled,
        })
    }

    fn cancel_search(&self, key: SearchKey) {
        if let Some(cancelled) = self.searches.lock().unwrap().get(&key) {
            cancelled.notify_one();
        }
    }
}

pub fn digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

impl Gateway {
    /// A fresh token for `owner`, replacing any token that owner still held.
    pub fn issue(&self, owner: &str, engine: &str) -> Option<String> {
        self.issue_with_revision(owner, engine, None)
    }

    pub fn issue_with_revision(
        &self,
        owner: &str,
        engine: &str,
        binding_revision: Option<String>,
    ) -> Option<String> {
        let mut bytes = [0u8; 32];
        if getrandom::getrandom(&mut bytes).is_err() {
            tracing::error!("OS randomness unavailable; the control gateway was not attached");
            return None;
        }
        let token = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut tokens = self.tokens.lock().unwrap();
        tokens.retain(|_, grant| grant.owner != owner);
        tokens.insert(
            token.clone(),
            Grant {
                owner: owner.to_owned(),
                engine: engine.to_owned(),
                binding_revision,
                instructions: None,
                primary_surface: None,
            },
        );
        Some(token)
    }

    /// Move a prewarmed session's token to the thread that adopted it.
    pub fn rebind(&self, from: &str, to: &str) {
        self.rebind_with_revision(from, to, None);
    }

    pub fn rebind_with_revision(&self, from: &str, to: &str, binding_revision: Option<String>) {
        let mut tokens = self.tokens.lock().unwrap();
        tokens.retain(|_, grant| from == to || grant.owner != to);
        for grant in tokens.values_mut().filter(|grant| grant.owner == from) {
            grant.owner = to.to_owned();
            if binding_revision.is_some() {
                grant.binding_revision = binding_revision.clone();
            }
        }
    }

    pub(crate) fn set_binding_revision(&self, owner: &str, revision: String) {
        for grant in self.tokens.lock().unwrap().values_mut().filter(|grant|grant.owner == owner) {
            grant.binding_revision = Some(revision.clone());
        }
    }

    pub fn release(&self, owner: &str) {
        self.tokens
            .lock()
            .unwrap()
            .retain(|_, grant| grant.owner != owner);
    }

    pub(crate) fn set_instructions(&self, owner: &str, instructions: Option<String>) {
        for grant in self.tokens.lock().unwrap().values_mut().filter(|grant| grant.owner == owner) {
            grant.instructions = instructions.clone();
            tracing::debug!(%owner, instruction_chars = instructions.as_deref().map(str::len).unwrap_or(0), "MCP server instructions configured");
        }
    }

    fn set_primary_surface(&self, owner: &str, primary_surface: Option<String>) {
        for grant in self.tokens.lock().unwrap().values_mut().filter(|grant| grant.owner == owner) {
            grant.primary_surface = primary_surface.clone();
        }
    }

    pub fn release_all(&self) {
        self.tokens.lock().unwrap().clear();
    }

    /// The digest of the token `owner` currently holds, for the grant record
    /// the capability page shows. The token itself never leaves this process.
    pub fn token_digest(&self, owner: &str) -> Option<String> {
        let tokens = self.tokens.lock().unwrap();
        tokens
            .iter()
            .find(|(_, grant)| grant.owner == owner)
            .map(|(token, _)| digest(token))
    }

    fn resolve(&self, token: &str) -> Option<Grant> {
        self.tokens.lock().unwrap().get(token).cloned()
    }
    pub(crate) fn owner_for_token(&self, token: &str) -> Option<String> {
        self.resolve(token).map(|grant| grant.owner)
    }
}

/// Legacy single-endpoint registration, preserved for unit tests and single-endpoint adapters.
pub fn registration(
    s: &Arc<AppState>,
    owner: &str,
    engine: &str,
    adapter_id: &str,
) -> Option<agent::McpRegistration> {
    if !crate::agent_runtime::pool::attaches_mcp(adapter_id) || !enabled(s) {
        return None;
    }
    let port = s.store.get("system", "port").ok().flatten()?.as_u64()?;
    let rev = capabilities::thread_binding_revision(&s.store, owner);
    let token = s.capability_gateway.issue_with_revision(owner, engine, Some(rev))?;
    Some(agent::McpRegistration {
        name: SERVER_NAME.into(),
        url: format!("http://127.0.0.1:{port}/api/capability"),
        bearer_token: token,
    })
}

fn packages_enabled_for_thread(store: &crate::store::Store, thread_id: &str) -> bool {
    if capabilities::thread_mode(store, thread_id) != "src" {
        return true;
    }
    matches!(
        crate::runs::src_api::resolve_authorized_src_caller_from_store(store, thread_id),
        Ok(Some(caller))
            if caller.delegation.as_ref().and_then(|d| d["profile"].as_str()) == Some("scoped_execution")
    )
}

/// Domain-scoped MCP registrations sharing one session-owned bearer token:
/// `muteki-collab`, `muteki-browser`, `muteki-computer`, `muteki-platform`
/// (progressive disclosure), and `muteki-packages` (when chat packages exist).
pub async fn registrations(
    s: &Arc<AppState>,
    owner: &str,
    engine: &str,
    adapter_id: &str,
) -> Vec<agent::McpRegistration> {
    if !crate::agent_runtime::pool::attaches_mcp(adapter_id) {
        return Vec::new();
    }
    let Some(port) = s.store.get("system", "port").ok().flatten().and_then(|v| v.as_u64()) else {
        return Vec::new();
    };
    let bound = capabilities::effective_tool_set(&s.store, owner);
    let mode = capabilities::thread_mode(&s.store, owner);
    let plugin_control_ok = plugins::registry(s)
        .map(|registry| registry.control_enabled())
        .unwrap_or(true);
    let mut active_surfaces: Vec<(&'static str, &'static str)> = Vec::new();
    if plugin_control_ok
        && capabilities::collab_enabled_for_thread(s, owner)
        && bound.iter().any(|name| capabilities::is_collab_tool(name))
    {
        active_surfaces.push((capabilities::COLLAB_SERVER, "collab"));
    }
    if capabilities::browser_enabled(s) && bound.iter().any(|name| browser::handles(name)) {
        active_surfaces.push((capabilities::BROWSER_SERVER, "browser"));
    }
    if capabilities::computer_enabled(s) && bound.iter().any(|name| chat_computer::handles(name)) {
        active_surfaces.push((capabilities::COMPUTER_SERVER, "computer"));
    }
    if plugin_control_ok
        && (mode != "conversation" || capabilities::platform_enabled_for_thread(s, owner))
        && bound.iter().any(|name| capabilities::is_platform_tool(name))
    {
        active_surfaces.push((capabilities::PLATFORM_SERVER, "platform"));
    }
    let has_packages = if packages_enabled_for_thread(&s.store, owner)
        && let Ok(registry) = plugins::registry(s)
    {
        registry
            .prepare_tools(engine, "chat", "")
            .await
            .is_ok_and(|rows| !rows.is_empty())
    } else {
        false
    };
    if has_packages {
        active_surfaces.push((capabilities::PACKAGES_SERVER, "packages"));
    }
    if active_surfaces.is_empty() {
        return Vec::new();
    }
    let rev = capabilities::thread_binding_revision(&s.store, owner);
    let Some(token) = s.capability_gateway.issue_with_revision(owner, engine, Some(rev)) else {
        return Vec::new();
    };
    s.capability_gateway
        .set_primary_surface(owner, active_surfaces.first().map(|(_, surf)| (*surf).to_owned()));
    active_surfaces
        .into_iter()
        .map(|(name, surf)| agent::McpRegistration {
            name: name.to_owned(),
            url: format!("http://127.0.0.1:{port}/api/capability/{surf}"),
            bearer_token: token.clone(),
        })
        .collect()
}

/// Whether at least one modular gateway surface is enabled for `owner` (used by
/// single-endpoint bridges such as `pi.rpc`).
pub fn any_surface_enabled(s: &AppState, owner: &str) -> bool {
    let mode = capabilities::thread_mode(&s.store, owner);
    let plugin_control_ok = plugins::registry(s)
        .map(|registry| registry.control_enabled())
        .unwrap_or(true);
    (plugin_control_ok && capabilities::collab_enabled_for_thread(s, owner))
        || capabilities::browser_enabled(s)
        || capabilities::computer_enabled(s)
        || (plugin_control_ok
            && (mode != "conversation" || capabilities::platform_enabled_for_thread(s, owner)))
}

/// Both switches the legacy control endpoint answers to: the capability policy
/// and the chat plugin page's own toggle.
pub fn enabled(s: &AppState) -> bool {
    capabilities::enabled(s, "mcp", SERVER_NAME)
        && plugins::registry(s)
            .map(|registry| registry.control_enabled())
            .unwrap_or(true)
}


fn body(
    status: StatusCode,
    value: Value,
    headers: Vec<(header::HeaderName, &'static str)>,
) -> Response {
    let mut response = (status, axum::Json(value)).into_response();
    for (name, value) in headers {
        if let Ok(value) = header::HeaderValue::from_str(value) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

fn result(id: &Value, value: Value) -> Response {
    body(
        StatusCode::OK,
        json!({"jsonrpc": "2.0", "id": id, "result": value}),
        Vec::new(),
    )
}

fn error(
    status: StatusCode,
    id: &Value,
    code: i64,
    message: &str,
    data: Option<Value>,
) -> Response {
    let mut failure = json!({"code": code, "message": message});
    if let Some(data) = data {
        failure["data"] = data;
    }
    body(
        status,
        json!({"jsonrpc": "2.0", "id": id, "error": failure}),
        Vec::new(),
    )
}

fn server_name_for(surface: Option<&str>) -> &'static str {
    match surface {
        Some("collab") => capabilities::COLLAB_SERVER,
        Some("browser") => capabilities::BROWSER_SERVER,
        Some("computer") => capabilities::COMPUTER_SERVER,
        Some("platform") => capabilities::PLATFORM_SERVER,
        Some("packages") => capabilities::PACKAGES_SERVER,
        _ => SERVER_NAME,
    }
}

fn server_info_for(surface: Option<&str>) -> Value {
    json!({"name": server_name_for(surface), "version": SERVER_VERSION})
}

fn result_meta_for(surface: Option<&str>) -> Value {
    json!({META_SERVER_INFO: server_info_for(surface)})
}

fn surface_instructions(surface: &str) -> &'static str {
    match surface {
        "collab" => "Muteki 多模型协作与跨会话控制网关（muteki-collab）：用于读取跨会话锚点（muteki_outline → muteki_read）、通过 muteki_collab_discover 查明参数，再用 muteki_collab_call 委派线程（tool=muteki_start_thread）、发送消息、提交完整报告及验收收尾。讨论和执行使用同一套线程工具；子线程不得访问祖先线程。",
        "browser" => "Muteki 内置预览与后台无头浏览器网关（muteki-browser）：先调 chat_browser_status 查看宿主，再调 chat_browser_open / chat_browser_read / chat_browser_click / chat_browser_type / chat_browser_screenshot 交互或验收页面。",
        "computer" => "Muteki 本机电脑操控网关（muteki-computer）：先调 chat_computer_status 检查可用性与占用，再调 chat_computer_js 操控原生桌面应用。网页操作优先采用用户指定的可用工具，未指定时推荐 muteki-browser。",
        "platform" => "Muteki 工作台与安全平台渐进式披露网关（muteki-platform）：覆盖模型凭据/端点刷新（models）、Runtime 实例管理（runtimes）、扩展/插件/内置能力开关（extensions）、界面/偏好/治理只读状态与历史导入（settings）、工作区/Worktree/目录/记忆/分享（workspace）、运维维护与模块调度（operations），以及 CTF/Pentest/比赛/Worker/SRC（90+ 项工具）。禁止猜测参数：先调 muteki_platform_discover 浏览领域目录或展开目标工具完整 JSON Schema，再调 muteki_platform_call 执行。",
        "packages" => "Muteki 已安装聊天扩展包工具网关（muteki-packages）。",
        _ => "",
    }
}

fn effective_instructions(s: &Arc<AppState>, grant: &Grant, surface: Option<&str>) -> Option<String> {
    let Some(surf) = surface else {
        return grant.instructions.clone();
    };
    let base = if surf == "collab" {
        format!("{}\n\n{}", surface_instructions("collab"), crate::control_tools::instructions_for_thread(s, &grant.owner))
    } else {
        surface_instructions(surf).to_owned()
    };
    let carries_session = grant
        .primary_surface
        .as_deref()
        .map(|primary| primary == surf)
        .unwrap_or(surf == "collab");
    match (base.is_empty(), carries_session.then_some(grant.instructions.as_deref()).flatten()) {
        (true, None) => None,
        (false, None) => Some(base.to_owned()),
        (true, Some(extra)) => Some(extra.to_owned()),
        (false, Some(extra)) => Some(format!("{base}\n\n{extra}")),
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or("")
}


pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/capability", post(serve))
        .route("/api/capability/{surface}", post(serve_surface))
}

pub fn valid_surface(surface: &str) -> bool {
    matches!(
        surface,
        "collab" | "browser" | "computer" | "platform" | "packages" | "control"
    )
}

pub fn gateway_path(path: &str) -> bool {
    path == "/api/capability"
        || path
            .strip_prefix("/api/capability/")
            .is_some_and(valid_surface)
}

async fn serve(State(s): State<Arc<AppState>>, headers: HeaderMap, payload: Bytes) -> Response {
    serve_on(s, None, headers, payload).await
}

async fn serve_surface(
    State(s): State<Arc<AppState>>,
    Path(surface): Path<String>,
    headers: HeaderMap,
    payload: Bytes,
) -> Response {
    let normalized = surface.trim();
    if !valid_surface(normalized) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mapped = (normalized != "control").then_some(normalized);
    serve_on(s, mapped, headers, payload).await
}

async fn serve_on(
    s: Arc<AppState>,
    surface: Option<&str>,
    headers: HeaderMap,
    payload: Bytes,
) -> Response {
    let origin = header_text(&headers, "origin");
    if !origin.is_empty() && !loopback(origin) {
        return body(
            StatusCode::FORBIDDEN,
            json!({"jsonrpc": "2.0", "id": Value::Null, "error": {"code": INVALID_REQUEST, "message": "Origin is not allowed for this MCP endpoint"}}),
            vec![(header::VARY, "Origin")],
        );
    }
    let Some(token) = header_text(&headers, "authorization")
        .strip_prefix("Bearer ")
        .map(str::trim)
    else {
        return body(
            StatusCode::UNAUTHORIZED,
            json!({"error": "missing bearer token"}),
            vec![(header::WWW_AUTHENTICATE, "Bearer realm=\"muteki-control\"")],
        );
    };
    let Some(grant) = s.capability_gateway.resolve(token) else {
        return body(
            StatusCode::UNAUTHORIZED,
            json!({"error": "unknown or retired session token", "code": "capability.token_invalid"}),
            vec![(
                header::WWW_AUTHENTICATE,
                "Bearer realm=\"muteki-control\", error=\"invalid_token\"",
            )],
        );
    };
    let Ok(request) = serde_json::from_slice::<Value>(&payload) else {
        return error(
            StatusCode::BAD_REQUEST,
            &Value::Null,
            PARSE_ERROR,
            "request body is not valid JSON",
            None,
        );
    };
    let Some(object) = request.as_object() else {
        return error(
            StatusCode::BAD_REQUEST,
            &Value::Null,
            INVALID_REQUEST,
            "batch requests are not supported",
            None,
        );
    };
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    if object.get("jsonrpc") != Some(&json!("2.0")) {
        return error(
            StatusCode::BAD_REQUEST,
            &id,
            INVALID_REQUEST,
            "jsonrpc must be '2.0'",
            None,
        );
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return error(
            StatusCode::BAD_REQUEST,
            &id,
            INVALID_REQUEST,
            "method/params have invalid types",
            None,
        );
    };
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    if !params.is_object() {
        return error(
            StatusCode::BAD_REQUEST,
            &id,
            INVALID_REQUEST,
            "method/params have invalid types",
            None,
        );
    };
    if method.starts_with("muteki.") {
        if !object.contains_key("id") { return StatusCode::ACCEPTED.into_response(); }
        return private_rpc(&s,&grant,token,&id,method,&params).await;
    }
    let version = header_text(&headers, "mcp-protocol-version");
    let legacy = method == "initialize" || version.is_empty() || LEGACY_VERSIONS.contains(&version);
    let is_notification = !object.contains_key("id");
    if !legacy {
        let accept = header_text(&headers, "accept");
        if !accept.contains("application/json") || !accept.contains("text/event-stream") {
            return body(
                StatusCode::NOT_ACCEPTABLE,
                json!({"error": "Accept must include application/json and text/event-stream"}),
                Vec::new(),
            );
        }
        if let Some(failure) = protocol_headers(&headers, &id, method, &params, is_notification) {
            return failure;
        }
    }
    if is_notification {
        if method == "notifications/cancelled" {
            if let Some(request_id) = params.get("requestId") {
                s.capability_gateway.cancel_search(SearchKey::new(token, surface, request_id));
            }
            return StatusCode::ACCEPTED.into_response();
        }
        if legacy
            || matches!(
                method,
                "notifications/roots/list_changed"
                    | "notifications/progress"
                    | "notifications/initialized"
            )
        {
            return StatusCode::ACCEPTED.into_response();
        }
        return error(
            StatusCode::NOT_FOUND,
            &Value::Null,
            METHOD_NOT_FOUND,
            &format!("unknown notification method {method:?}"),
            None,
        );
    }
    match method {
        "initialize" if legacy => {
            let requested = params["protocolVersion"].as_str().unwrap_or("");
            let version = if LEGACY_VERSIONS.contains(&requested) {
                requested
            } else {
                LEGACY_VERSIONS[0]
            };
            result(
                &id,
                json!({"protocolVersion": version, "capabilities": {"tools": {"listChanged": false}}, "serverInfo": server_info_for(surface), "instructions": effective_instructions(&s, &grant, surface)}),
            )
        }
        "ping" if legacy => result(&id, json!({})),
        "server/discover" => result(
            &id,
            json!({"resultType": "complete", "supportedVersions": [PROTOCOL_VERSION],
                   "capabilities": {"tools": {"listChanged": false}}, "_meta": {META_SERVER_INFO: server_info_for(surface)},
                   "instructions": effective_instructions(&s, &grant, surface),
                   "ttlMs": TOOLS_TTL_MS, "cacheScope": "private"}),
        ),
        "tools/list" => {
            let tools = catalog_for_surface(&s, &grant, surface).await;
            match legacy {
                true => result(&id, json!({"tools": tools})),
                false => result(
                    &id,
                    json!({"tools": tools, "resultType": "complete", "ttlMs": TOOLS_TTL_MS,
                           "cacheScope": "private", "_meta": result_meta_for(surface)}),
                ),
            }
        }
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("").trim().to_owned();
            if name.is_empty() {
                return error(
                    StatusCode::OK,
                    &id,
                    INVALID_PARAMS,
                    "params.name is required",
                    None,
                );
            }
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !arguments.is_object() {
                return error(
                    StatusCode::OK,
                    &id,
                    INVALID_PARAMS,
                    "params.arguments must be an object",
                    None,
                );
            }
            let meta_image_input = params["_meta"]["muteki/modelCapabilities"]["imageInput"].as_bool();
            let run = call(&s, &grant, token, &name, &arguments, meta_image_input);
            let mut payload = if name == "muteki_search"
                || (name == COLLAB_CALL_TOOL && arguments["tool"] == "muteki_search")
            {
                let Some(active) = s.capability_gateway.begin_search(SearchKey::new(token, surface, &id)) else {
                    return error(StatusCode::OK, &id, INVALID_REQUEST, "Search request ID is already active", None);
                };
                tokio::select! {
                    biased;
                    _ = active.cancelled.notified() => json!({
                        "ok":false,
                        "error":{"code":"control.search_cancelled","message":"Conversation search was cancelled"}
                    }),
                    payload = run => payload,
                }
            } else {
                run.await
            };
            let images = payload.as_object_mut().and_then(|row| row.remove("_mcp_images"))
                .and_then(|value| value.as_array().cloned()).unwrap_or_default();
            let is_error = payload["ok"] != json!(true);
            let text = serde_json::to_string(&payload).unwrap_or_default();
            let mut content = vec![json!({"type": "text", "text": text})];
            content.extend(images);
            match legacy {
                true => result(
                    &id,
                    json!({"content": content, "structuredContent": payload, "isError": is_error}),
                ),
                false => result(
                    &id,
                    json!({"resultType": "complete", "content": content, "structuredContent": payload,
                           "isError": is_error, "_meta": result_meta_for(surface)}),
                ),
            }
        }
        _ => error(
            if legacy {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            },
            &id,
            METHOD_NOT_FOUND,
            &format!("unknown method {method:?}"),
            None,
        ),
    }
}

fn loopback(origin: &str) -> bool {
    reqwest::Url::parse(origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .map(|address| address.is_loopback())
                    .unwrap_or(false)
        })
}

async fn private_rpc(s: &Arc<AppState>, grant: &Grant, token: &str, id: &Value, method: &str, params: &Value) -> Response {
    let binding = match capabilities::authorize_gateway_grant(s,&grant.owner,&digest(token)) {
        Ok((_,binding)) => binding,
        Err((_,axum::Json(body))) => {
            let envelope = &body["error"];
            let code = match envelope["category"].as_str().unwrap_or("internal") {
                "validation" | "not_found" => INVALID_PARAMS,
                "internal" => -32603,
                _ => -32001,
            };
            return error(StatusCode::OK,id,code,envelope["message"].as_str().unwrap_or("Capability request failed"),Some(json!({"error":envelope})));
        }
    };
    if binding["mode"] == "conversation" {
        return error(StatusCode::OK,id,METHOD_NOT_FOUND,"Chat product tools use MCP tools/list and tools/call on this endpoint.",
            Some(json!({"code":"conversation.mcp_required"})));
    }
    match method {
        "muteki.describe" => {
            let tools: Vec<Value> = capabilities::catalog().into_iter()
                .filter(|tool|binding["tool_set"].as_array().is_some_and(|names|names.iter().any(|name|name == &tool["name"])))
                .map(|tool|json!({"schema_version":1,"name":tool["name"],"description":tool["description"],"input_schema":tool["input_schema"]})).collect();
            result(id,json!({"schema_version":1,"binding_id":binding["binding_id"],"binding_version":binding["binding_version"],
                "mode":binding["mode"],"tools":tools,"allowed_commands":binding["allowed_commands"],
                "allowed_queries":binding["allowed_queries"],"resource_scopes":binding["resource_scopes"]}))
        },
        "muteki.invoke" => {
            let name = params["tool_name"].as_str().unwrap_or("").trim();
            if name.is_empty() { return error(StatusCode::OK,id,INVALID_PARAMS,"params.tool_name is required",None); }
            let arguments = params.get("arguments").filter(|value| !value.is_null()).cloned().unwrap_or_else(||json!({}));
            if !arguments.is_object() { return error(StatusCode::OK,id,INVALID_PARAMS,"params.arguments must be an object",None); }
            let mut payload = call_identified(s,grant,token,name,&arguments,None,params["invocation_id"].as_str()).await;
            if payload["error"].is_object() && payload["error"]["correlation_id"].is_null() {
                payload["error"]["correlation_id"] = params.get("correlation_id").cloned().unwrap_or_else(||payload["invocation_id"].clone());
            }
            if let Some(images) = payload.as_object_mut().and_then(|object|object.remove("_mcp_images")) { payload["images"] = images; }
            result(id,payload)
        },
        _ => error(StatusCode::OK,id,METHOD_NOT_FOUND,&format!("unknown method {method:?}"),None),
    }
}

/// The headers and `params._meta` the current revision makes mandatory.
fn protocol_headers(
    headers: &HeaderMap,
    id: &Value,
    method: &str,
    params: &Value,
    is_notification: bool,
) -> Option<Response> {
    let mismatch = |message: &str| {
        Some(error(
            StatusCode::BAD_REQUEST,
            id,
            HEADER_MISMATCH,
            message,
            Some(json!({"error": {"code": "mcp.header_mismatch", "message": message}})),
        ))
    };
    let version = header_text(headers, "mcp-protocol-version");
    if version != PROTOCOL_VERSION {
        return Some(error(
            StatusCode::BAD_REQUEST,
            id,
            UNSUPPORTED_PROTOCOL_VERSION,
            &format!("unsupported protocol version {version:?}"),
            Some(json!({"supported": [PROTOCOL_VERSION], "requested": version})),
        ));
    }
    if !is_notification {
        let meta = &params["_meta"];
        if !meta.is_object() {
            return mismatch("params._meta is required");
        }
        match meta[META_PROTOCOL_VERSION].as_str() {
            None => return mismatch(&format!("params._meta.{META_PROTOCOL_VERSION} is required")),
            Some(declared) if declared != version => {
                return mismatch("MCP-Protocol-Version header does not match params._meta");
            }
            Some(_) => {}
        }
        if !meta[META_CLIENT_CAPABILITIES].is_object() {
            return Some(error(
                StatusCode::BAD_REQUEST,
                id,
                INVALID_PARAMS,
                &format!("params._meta.{META_CLIENT_CAPABILITIES} must be an object"),
                None,
            ));
        }
    }
    let declared_method = header_text(headers, "mcp-method");
    if declared_method.is_empty() {
        return mismatch("Mcp-Method header is required");
    }
    if declared_method != method {
        return mismatch("Mcp-Method header does not match request method");
    }
    if !is_notification && method == "tools/call" {
        let name = header_text(headers, "mcp-name");
        if name.is_empty() {
            return mismatch("Mcp-Name header is required for tools/call");
        }
        if params["name"]
            .as_str()
            .is_some_and(|declared| declared != name)
        {
            return mismatch("Mcp-Name header does not match request params.name");
        }
    }
    None
}


fn format_catalog_tool(tool: &Value, engine: &str) -> Value {
    let name = tool["name"].as_str().unwrap_or("");
    let schema = &tool["input_schema"];
    let mut description = tool["description"].as_str().unwrap_or("").to_owned();
    if !crate::control_tools::handles(name) && tool["target_kind"] == "command" {
        description.push_str("\n\n返回结构：structuredContent.receipt.state 是命令状态；completed 时业务结果在 structuredContent.receipt.output.result，SRC 记录正文在该结果的 payload。顶层没有 result 并不表示写入失败。先读取回执和输出；成功后使用查询工具回读，禁止为查找返回字段而再次提交写入。重试同一逻辑操作必须复用 idempotency_key。");
    } else {
        description.push_str("\n\n返回结构：查询业务结果在 structuredContent.result，失败信息在 structuredContent.error；先检查 structuredContent.ok。");
    }
    if (engine == "omp" || engine.starts_with("omp."))
        && ["allOf", "anyOf", "oneOf"].iter().any(|key| schema.get(*key).is_some())
    {
        description.push_str("\n\nComplete input JSON Schema (including composite constraints):\n```json\n");
        description.push_str(&schema.to_string());
        description.push_str("\n```");
    }
    json!({"name": name, "description": description, "inputSchema": schema})
}

pub(crate) fn collab_discover_tool() -> Value {
    json!({
        "name": COLLAB_DISCOVER_TOOL,
        "description": "Discover available cross-provider collaboration tools and their complete argument schemas. Use tool=muteki_start_thread to discover managed role-based delegation; omit tool to list available tools. Execute through muteki_collab_call.",
        "inputSchema": {"type":"object","properties":{"tool":{"type":"string"}},"additionalProperties":false}
    })
}

pub(crate) fn collab_call_tool() -> Value {
    json!({
        "name": COLLAB_CALL_TOOL,
        "description": "Call a collaboration tool discovered with muteki_collab_discover. For configured cross-provider delegation use tool=muteki_start_thread and arguments containing request_id, subagent=true, role and message. Preserves the target tool's permissions, validation and audit.",
        "inputSchema": {"type":"object","properties":{"tool":{"type":"string"},"arguments":{"type":"object"}},"required":["tool","arguments"],"additionalProperties":false}
    })
}

pub(crate) fn platform_discover_tool() -> Value {
    json!({
        "name": PLATFORM_DISCOVER_TOOL,
        "description": "渐进式披露发现入口（muteki-platform）：按领域目录浏览或按需展开工作台配置（模型凭据与端点刷新、Runtime、能力/插件/扩展、UI 与对话偏好、工作区/目录/记忆/分享、运维与系统状态）及 CTF/Pentest/比赛/Worker/SRC 工具的完整参数 JSON Schema。省略参数或 domain=\"overview\" 时返回全部业务子域概览；传 domain、query 或 tools 数组时返回完整 Schema 与返回契约。\n\n返回结构：查询业务结果在 structuredContent.result，失败信息在 structuredContent.error；先检查 structuredContent.ok。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "domain": {
                    "type": "string",
                    "enum": [
                        "overview",
                        "models",
                        "runtimes",
                        "extensions",
                        "settings",
                        "workspace",
                        "operations",
                        "runs",
                        "pentest",
                        "competitions",
                        "workers",
                        "src",
                        "src-auth",
                        "src-assets",
                        "src-findings",
                        "src-reports",
                        "src-traffic",
                        "src-connectors",
                        "src-monitors",
                        "src-identity",
                        "src-research",
                        "src-workbench",
                        "all"
                    ],
                    "description": "业务子域：overview（默认概览目录）、models、runtimes、extensions、settings、workspace、operations、runs、pentest、competitions、workers、src（返回 SRC 10 子域索引）、src-auth/src-assets/src-findings/src-reports/src-traffic/src-connectors/src-monitors/src-identity/src-research/src-workbench（按 SRC 子域精确展开）、all（全部）"
                },
                "query": {
                    "type": "string",
                    "description": "可选：按工具名或中文说明关键词过滤平台工具"
                },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "可选：精确指定要读取完整 JSON Schema 的平台工具名列表（如 [\"muteki_list_runs\", \"muteki_dispatch_challenge\"]）"
                }
            },
            "additionalProperties": false
        }
    })
}

pub(crate) fn platform_call_tool() -> Value {
    json!({
        "name": PLATFORM_CALL_TOOL,
        "description": "渐进式披露执行入口（muteki-platform）：执行已通过 muteki_platform_discover 查明参数契约的 Muteki 平台工具（如 muteki_list_runs、muteki_dispatch_challenge、muteki_list_competitions 等）。权限校验、参数 Schema 校验、异步 CommandReceipt 与审计均与底层工具完全一致。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tool": {
                    "type": "string",
                    "description": "目标 Muteki 平台工具名（如 muteki_list_runs）"
                },
                "arguments": {
                    "type": "object",
                    "description": "传给目标平台工具的参数对象（默认 {}）"
                }
            },
            "required": ["tool"],
            "additionalProperties": false
        }
    })
}

pub(crate) fn is_src_platform_core_tool(name: &str) -> bool {
    matches!(
        name,
        "muteki_get_src_task"
            | "muteki_get_src_policy"
            | "muteki_renew_src_authorization"
            | "muteki_src_workbench_snapshot"
            | "muteki_list_chat_plugins"
            | "muteki_list_platform_extensions"
            | "muteki_list_src_records"
            | "muteki_get_src_record"
            | "muteki_asset_query"
            | "muteki_asset_detail"
            | "muteki_src_asset_versions"
            | "muteki_finding_claims"
            | "muteki_finding_claim_get"
            | "muteki_finding_status"
            | "muteki_finding_reviews"
            | "muteki_src_finding_meta"
            | "muteki_src_duplicates"
            | "muteki_report_list"
            | "muteki_report_get"
            | "muteki_src_report_diff"
            | "muteki_exchange_query"
            | "muteki_exchange_read"
            | "muteki_exchange_compare"
            | "muteki_traffic_search"
            | "muteki_traffic_read_bytes"
            | "muteki_src_review_todos"
            | "muteki_src_review_drafts"
            | "muteki_src_review_suggestions"
            | "muteki_src_evidence_verify"
            | "muteki_artifact_read"
            | "muteki_workbench_suggestion_propose"
            | "muteki_src_todo_save"
            | "muteki_note_save"
            | "muteki_src_project_memory_get"
            | "muteki_src_project_memory_save"
            | "muteki_coverage_audit"
            | "muteki_seed_src_coverage"
            | "muteki_research_plan_current"
            | "muteki_research_plan_save"
            | "muteki_research_backlog"
            | "muteki_research_precondition_summary"
            | "muteki_research_records"
            | "muteki_research_record_get"
            | "muteki_list_src_runs"
            | "muteki_ensure_src_run"
            | "muteki_start_src_run"
            | "muteki_src_run_intervene"
            | "muteki_get_src_coordination_status"
            | "muteki_src_coordination_decide"
            | "muteki_ack_src_coordination_notice"
            | "muteki_src_connector_batches"
            | "muteki_exchange_draft_save"
            | "muteki_exchange_draft_execute"
            | "muteki_exchange_record"
            | "muteki_asset_observe"
            | "muteki_finding_claim_create"
    )
}

/// Domain-scoped tool catalog for `/api/capability/{surface}` and legacy `/api/capability`.
async fn catalog_for_surface(s: &Arc<AppState>, grant: &Grant, surface: Option<&str>) -> Vec<Value> {
    let mut tools = Vec::new();
    let plugin_control_ok = plugins::registry(s)
        .map(|registry| registry.control_enabled())
        .unwrap_or(true);
    let allowed = capabilities::effective_tool_set(&s.store, &grant.owner);
    let mode = capabilities::thread_mode(&s.store, &grant.owner);

    let include_collab = matches!(surface, Some("collab") | None)
        && plugin_control_ok
        && capabilities::collab_enabled_for_thread(s, &grant.owner);
    let include_platform = matches!(surface, Some("platform") | None)
        && plugin_control_ok
        && (mode != "conversation" || capabilities::platform_enabled_for_thread(s, &grant.owner));
    let include_computer =
        matches!(surface, Some("computer") | None) && capabilities::computer_enabled(s);
    let include_browser = matches!(surface, Some("browser") | None) && capabilities::browser_enabled(s);
    let include_packages = matches!(surface, Some("packages") | None)
        && packages_enabled_for_thread(&s.store, &grant.owner);

    let full_catalog = if include_collab || (include_platform && mode != "conversation") {
        capabilities::catalog()
    } else {
        Vec::new()
    };

    if include_collab {
        if allowed.iter().any(|name| capabilities::is_collab_tool(name)) {
            tools.push(collab_discover_tool());
            tools.push(collab_call_tool());
        }
        for tool in &full_catalog {
            let name = tool["name"].as_str().unwrap_or("");
            if capabilities::is_collab_tool(name)
                && allowed.iter().any(|selected| selected == name)
                && crate::control::visible_in_collab_list(s, &grant.owner, name)
            {
                tools.push(format_catalog_tool(tool, &grant.engine));
            }
        }
    }

    if include_platform {
        let publish_src_core_only = mode == "src";
        if mode != "conversation" {
            for tool in &full_catalog {
                let name = tool["name"].as_str().unwrap_or("");
                if capabilities::is_platform_tool(name)
                    && allowed.iter().any(|selected| selected == name)
                    && (!publish_src_core_only || is_src_platform_core_tool(name))
                {
                    tools.push(format_catalog_tool(tool, &grant.engine));
                }
            }
        }
        if allowed.iter().any(|selected| capabilities::is_platform_tool(selected)) {
            tools.push(platform_discover_tool());
            tools.push(platform_call_tool());
        }
    }

    if include_computer && allowed.iter().any(|name| chat_computer::handles(name)) {
        let status = chat_computer::status_tool();
        tools.push(json!({
            "name": status["name"],
            "description": status["description"],
            "inputSchema": status["input_schema"]
        }));
        for row in s.computer.live_tools(s).await {
            let name = row["name"].as_str().unwrap_or("");
            if allowed.iter().any(|selected| selected == name) {
                tools.push(json!({
                    "name": name,
                    "description": row["description"],
                    "inputSchema": row["input_schema"]
                }));
            }
        }
    }

    if include_browser {
        for tool in browser::tools()
            .into_iter()
            .filter(|tool| allowed.iter().any(|name| tool["name"] == name.as_str()))
        {
            tools.push(json!({
                "name": tool["name"],
                "description": tool["description"],
                "inputSchema": tool["input_schema"]
            }));
        }
    }

    if include_packages
        && let Ok(registry) = plugins::registry(s)
    {
        match registry.prepare_tools(&grant.engine, "chat", "").await {
            Ok(rows) => tools.extend(rows.into_iter().map(|tool| {
                json!({"name": tool["name"], "description": tool["description"], "inputSchema": tool["input_schema"]})
            })),
            Err(error) => tracing::warn!(code = %error.code, "Package tool catalog unavailable"),
        }
    }

    for tool in &mut tools {
        tool["inputSchema"] = normalize_input_schema(&tool["inputSchema"]);
        if ALWAYS_LOAD.contains(&tool["name"].as_str().unwrap_or("")) {
            tool["_meta"] = json!({"anthropic/alwaysLoad": true});
        }
    }
    tools
}

const ALWAYS_LOAD: [&str; 17] = [
    "chat_browser_status",
    "chat_browser_open",
    "chat_computer_status",
    "chat_computer_js",
    "muteki_start_thread",
    "muteki_send_message",
    "muteki_control_thread",
    "muteki_get_subagents",
    "muteki_outline",
    "muteki_report_result",
    "muteki_src_todo_save",
    "muteki_src_project_memory_get",
    "muteki_src_project_memory_save",
    PLATFORM_DISCOVER_TOOL,
    PLATFORM_CALL_TOOL,
    COLLAB_DISCOVER_TOOL,
    COLLAB_CALL_TOOL,
];

/// One input-schema shape every engine accepts: an object root with
/// properties and `required` only as a non-empty list (Droid skips a tool
/// whose `required` is empty or null).
fn normalize_input_schema(schema: &Value) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if map.get("required").is_some_and(|required| required.as_array().is_none_or(Vec::is_empty)) {
                    map.remove("required");
                }
                for (key, child) in map.iter_mut() {
                    match key.as_str() {
                        "enum" | "const" | "default" | "examples" => {}
                        "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" =>
                            for schema in child.as_object_mut().into_iter().flat_map(|named| named.values_mut()) { strip(schema); },
                        _ => strip(child),
                    }
                }
            }
            Value::Array(items) => for item in items { strip(item); },
            _ => {}
        }
    }
    let mut schema = if schema.is_object() { schema.clone() } else { json!({}) };
    if let Some(map) = schema.as_object_mut() {
        map.remove("$schema");
        map.insert("type".into(), json!("object"));
        if !map.get("properties").is_some_and(Value::is_object) {
            map.insert("properties".into(), json!({}));
        }
    }
    strip(&mut schema);
    schema
}

fn failed_invocation(invocation:&str, code:&str,message:&str)->Value {
    json!({"ok": false, "invocation_id": invocation,
           "error": {"code": code, "message": message}})
}

fn handle_platform_discover(
    s: &Arc<AppState>,
    grant: &Grant,
    arguments: &Value,
    invocation: &str,
) -> Value {
    let plugin_ok = plugins::registry(s)
        .map(|registry| registry.control_enabled())
        .unwrap_or(true);
    let mode = capabilities::thread_mode(&s.store, &grant.owner);
    if !plugin_ok
        || (mode == "conversation" && !capabilities::platform_enabled_for_thread(s, &grant.owner))
    {
        return failed_invocation(
            invocation,
            "chat.platform.disabled",
            "安全平台桥接能力已停用；请在「设置 → 内置能力」开启，或在输入框使用 /ctf、/pentest、@Run 按需唤醒",
        );
    }
    let allowed = capabilities::effective_tool_set(&s.store, &grant.owner);
    let requested_domain = arguments["domain"].as_str().map(str::trim).unwrap_or("");
    let query = arguments["query"].as_str().map(str::trim).unwrap_or("").to_lowercase();
    let requested_tools: Vec<&str> = arguments["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect();

    let all_platform_rows: Vec<Value> = capabilities::catalog()
        .into_iter()
        .filter(|tool| {
            let name = tool["name"].as_str().unwrap_or("");
            (capabilities::is_platform_tool(name) || name == "muteki_get_command_receipt")
                && allowed.iter().any(|selected| selected == name)
        })
        .collect();

    let is_overview = (requested_domain.is_empty() || requested_domain == "overview")
        && query.is_empty()
        && requested_tools.is_empty();

    if is_overview {
        let domain_defs = [
            ("models", "模型凭据账号、无密钥自定义端点新增/刷新/测活、Local Pi、配额用量与定价"),
            ("runtimes", "Agent Runtime 描述符与实例增删、探活及能力/鉴权刷新"),
            ("extensions", "内置能力开关、会话能力绑定、聊天插件/MCP 与平台扩展生命周期"),
            ("settings", "UI 外观与对话偏好设置、Control 治理只读快照、Claude/Codex 历史导入"),
            ("workspace", "项目删除、工作区/Worktree 绑定与清理、分组目录、长期记忆、会话分享与回合追踪"),
            ("operations", "运维总览、Outbox、恢复与维护清理、平台 Effect、模块启停、资源配额与系统状态"),
            ("runs", "CTF 题库、Task 登记与 Run 执行监控/干预"),
            ("pentest", "授权渗透测试目标登记与独立执行流"),
            ("competitions", "比赛连接、题目同步、自动调度器与动态实例租约"),
            ("workers", "Worker 档案、模型健康、容器后端与镜像、网络/VPN 与保留策略"),
            ("src-auth", "SRC 目标范围授权、策略更新/预览、暂停与恢复"),
            ("src-assets", "SRC 资产观测、纠偏、研判、合并拆分与覆盖度审计"),
            ("src-findings", "SRC 漏洞线索、主张、重验复测、对象实验与查重"),
            ("src-reports", "SRC 报告草稿、正式报告、证据验证与产出读取"),
            ("src-traffic", "SRC HTTP 流量检索/对比/重放、竞态探测、OAST、流式连接与代理桥"),
            ("src-connectors", "SRC 外部连接器状态/验证/批次摄入与情报源管理"),
            ("src-monitors", "SRC 持续监控规则、预检调度与变更通知"),
            ("src-identity", "SRC 多角色身份上下文、登录配方与会话凭据导入/校验"),
            ("src-research", "SRC 研究计划、知识库、项目记忆、JS/SourceMap 与 GraphQL/gRPC 解析"),
            ("src-workbench", "SRC 工作台快照、待办建议、Run 调度/干预、反馈与诊断工具"),
        ];
        let mut domains = Vec::new();
        for (domain_id, label) in domain_defs {
            let names: Vec<String> = all_platform_rows
                .iter()
                .filter_map(|row| {
                    let name = row["name"].as_str().unwrap_or("");
                    let row_domain = if name == "muteki_get_command_receipt" {
                        "runs"
                    } else {
                        capabilities::platform_domain(name)
                    };
                    (row_domain == domain_id).then(|| name.to_owned())
                })
                .collect();
            if !names.is_empty() {
                domains.push(json!({
                    "domain": domain_id,
                    "summary": label,
                    "tool_count": names.len(),
                    "tools": names,
                }));
            }
        }
        return json!({
            "ok": true,
            "invocation_id": invocation,
            "result": {
                "mode": "progressive_disclosure_overview",
                "total_tools": all_platform_rows.len(),
                "domains": domains,
                "next_step": "按需传入 domain（如 \"src-auth\" / \"src-assets\" / \"src-findings\" / \"src-reports\" / \"src-traffic\" / \"models\" / \"runs\"）或 tools 数组再次调用 muteki_platform_discover 获取完整 JSON Schema，然后调用 muteki_platform_call 执行。"
            }
        });
    }

    if requested_domain == "src" && query.is_empty() && requested_tools.is_empty() {
        let src_subdomains = [
            ("src-auth", "SRC 目标范围授权、策略更新/预览、暂停与恢复"),
            ("src-assets", "SRC 资产观测、纠偏、研判、合并拆分与覆盖度审计"),
            ("src-findings", "SRC 漏洞线索、主张、重验复测、对象实验与查重"),
            ("src-reports", "SRC 报告草稿、正式报告、证据验证与产出读取"),
            ("src-traffic", "SRC HTTP 流量检索/对比/重放、竞态探测、OAST、流式连接与代理桥"),
            ("src-connectors", "SRC 外部连接器状态/验证/批次摄入与情报源管理"),
            ("src-monitors", "SRC 持续监控规则、预检调度与变更通知"),
            ("src-identity", "SRC 多角色身份上下文、登录配方与会话凭据导入/校验"),
            ("src-research", "SRC 研究计划、知识库、项目记忆、JS/SourceMap 与 GraphQL/gRPC 解析"),
            ("src-workbench", "SRC 工作台快照、待办建议、Run 调度/干预、反馈与诊断工具"),
        ];
        let mut domains = Vec::new();
        let mut total_src_tools = 0usize;
        for (domain_id, label) in src_subdomains {
            let names: Vec<String> = all_platform_rows
                .iter()
                .filter_map(|row| {
                    let name = row["name"].as_str().unwrap_or("");
                    (capabilities::platform_domain(name) == domain_id).then(|| name.to_owned())
                })
                .collect();
            if !names.is_empty() {
                total_src_tools += names.len();
                domains.push(json!({
                    "domain": domain_id,
                    "summary": label,
                    "tool_count": names.len(),
                    "tools": names,
                }));
            }
        }
        return json!({
            "ok": true,
            "invocation_id": invocation,
            "result": {
                "mode": "src_subdomain_overview",
                "total_tools": total_src_tools,
                "domains": domains,
                "next_step": "SRC 领域已拆分为 10 个子域，请传入具体 sub-domain（如 domain=\"src-findings\" 或 \"src-auth\"）、关键词 query 或 tools 数组再次调用 muteki_platform_discover 获取完整 JSON Schema。"
            }
        });
    }

    let matched: Vec<Value> = all_platform_rows
        .into_iter()
        .filter(|row| {
            let name = row["name"].as_str().unwrap_or("");
            let desc = row["description"].as_str().unwrap_or("");
            let row_domain = if name == "muteki_get_command_receipt" {
                "runs"
            } else {
                capabilities::platform_domain(name)
            };
            if !requested_tools.is_empty() && !requested_tools.contains(&name) {
                return false;
            }
            if !requested_domain.is_empty()
                && requested_domain != "all"
                && requested_domain != "overview"
                && !(requested_domain == "src" && row_domain.starts_with("src-"))
                && row_domain != requested_domain
            {
                return false;
            }
            if !query.is_empty()
                && !name.to_lowercase().contains(&query)
                && !desc.to_lowercase().contains(&query)
            {
                return false;
            }
            true
        })
        .map(|row| {
            let formatted = format_catalog_tool(&row, &grant.engine);
            let name = row["name"].as_str().unwrap_or("");
            json!({
                "name": name,
                "domain": if name == "muteki_get_command_receipt" { "runs" } else { capabilities::platform_domain(name) },
                "target_kind": row["target_kind"],
                "description": formatted["description"],
                "input_schema": normalize_input_schema(&formatted["inputSchema"]),
            })
        })
        .collect();

    json!({
        "ok": true,
        "invocation_id": invocation,
        "result": {
            "count": matched.len(),
            "tools": matched,
            "invoke_via": PLATFORM_CALL_TOOL,
        }
    })
}

async fn call(s: &Arc<AppState>, grant: &Grant, token: &str, name: &str, arguments: &Value, meta_image_input: Option<bool>) -> Value {
    call_identified(s,grant,token,name,arguments,meta_image_input,None).await
}

async fn call_identified(s: &Arc<AppState>, grant: &Grant, token: &str, name: &str, arguments: &Value, meta_image_input: Option<bool>, invocation_id: Option<&str>) -> Value {
    let invocation = invocation_id.map(str::trim).filter(|value|!value.is_empty()).map(str::to_owned)
        .unwrap_or_else(||format!("inv-{}",uuid::Uuid::new_v4().simple()));
    if name == COLLAB_DISCOVER_TOOL || name == COLLAB_CALL_TOOL {
        let plugin_ok = plugins::registry(s).map(|registry| registry.control_enabled()).unwrap_or(true);
        if !plugin_ok || !capabilities::collab_enabled_for_thread(s, &grant.owner) {
            return failed_invocation(&invocation, "chat.control.disabled", "跨会话协作网关已停用");
        }
        let schema = if name == COLLAB_DISCOVER_TOOL { collab_discover_tool() } else { collab_call_tool() };
        let validator = match jsonschema::draft202012::options().build(&schema["inputSchema"]) {
            Ok(validator) => validator,
            Err(error) => return failed_invocation(&invocation, "capability.schema_invalid", &error.to_string()),
        };
        if let Err(error) = validator.validate(arguments) {
            return failed_invocation(&invocation, "capability.arguments_invalid", &error.to_string());
        }
        let target = arguments["tool"].as_str().unwrap_or("");
        if name == COLLAB_DISCOVER_TOOL {
            let allowed = capabilities::effective_tool_set(&s.store, &grant.owner);
            let rows: Vec<Value> = capabilities::catalog().iter().filter(|tool| {
                let tool_name = tool["name"].as_str().unwrap_or("");
                capabilities::is_collab_tool(tool_name)
                    && allowed.iter().any(|selected| selected == tool_name)
                    && crate::control::visible_in_collab_list(s, &grant.owner, tool_name)
                    && (target.is_empty() || target == tool_name)
            }).map(|tool| format_catalog_tool(tool, &grant.engine)).collect();
            return json!({"ok":true,"invocation_id":invocation,"result":{"tools":rows,"invoke_via":COLLAB_CALL_TOOL}});
        }
        if !capabilities::is_collab_tool(target) {
            return failed_invocation(&invocation, "capability.tool_domain_mismatch", "muteki_collab_call 仅调用协作工具；先使用 muteki_collab_discover 查询契约");
        }
        return Box::pin(call_identified(s, grant, token, target, &arguments["arguments"], meta_image_input, Some(&invocation))).await;
    }
    if name == PLATFORM_DISCOVER_TOOL {
        return handle_platform_discover(s, grant, arguments, &invocation);
    }
    if name == PLATFORM_CALL_TOOL {
        let plugin_ok = plugins::registry(s)
            .map(|registry| registry.control_enabled())
            .unwrap_or(true);
        let mode = capabilities::thread_mode(&s.store, &grant.owner);
        if !plugin_ok
            || (mode == "conversation"
                && !capabilities::platform_enabled_for_thread(s, &grant.owner))
        {
            return failed_invocation(
                &invocation,
                "chat.platform.disabled",
                "安全平台桥接能力已停用；请在「设置 → 内置能力」开启，或在输入框使用 /ctf、/pentest、@Run 按需唤醒",
            );
        }
        let target = arguments["tool"].as_str().unwrap_or("").trim();
        if target.is_empty() {
            return failed_invocation(&invocation, "capability.arguments_invalid", "muteki_platform_call 需要提供非空 tool 字段");
        }
        if !capabilities::is_platform_tool(target) && target != "muteki_get_command_receipt" {
            return failed_invocation(
                &invocation,
                "capability.tool_domain_mismatch",
                "muteki_platform_call 仅用于调用 Muteki 安全平台工具；跨会话协作、浏览器与电脑操控请直接调用各自服务工具",
            );
        }
        let inner_args = arguments
            .get("arguments")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !inner_args.is_object() {
            return failed_invocation(&invocation, "capability.arguments_invalid", "muteki_platform_call 的 arguments 字段必须是 JSON 对象");
        }
        return Box::pin(call_identified(
            s,
            grant,
            token,
            target,
            &inner_args,
            meta_image_input,
            Some(&invocation),
        ))
        .await;
    }
    if capabilities::spec(name).is_some() {if let Err(error)=capabilities::validate_arguments(name,arguments){return json!({"ok":false,"invocation_id":invocation,"error":error});}}
    let binding = json!({
        "thread_id": grant.owner,
        "mode": capabilities::thread_mode(&s.store, &grant.owner),
        "principal_id": "local-user",
        "binding_id": format!("bind-{}", grant.owner),
        "binding_version": 1,
        "tool_set": capabilities::effective_tool_set(&s.store, &grant.owner),
    });
    dispatch_bound(s,grant,token,name,arguments,meta_image_input,&invocation,&binding).await
}

async fn dispatch_bound(s:&Arc<AppState>,grant:&Grant,token:&str,name:&str,arguments:&Value,meta_image_input:Option<bool>,invocation:&str,binding:&Value)->Value {
    if name == chat_computer::STATUS_TOOL {
        if !capabilities::computer_enabled(s) {
            return failed_invocation(invocation,"chat.computer.disabled", "电脑操控能力已停用；请在「设置 → 内置能力」启用 computer-use");
        }
        if !capabilities::effective_tool_set(&s.store, &grant.owner).iter().any(|name| chat_computer::handles(name)) {
            return failed_invocation(invocation,"capability.tool_not_bound","该会话的能力绑定未包含电脑操控工具，请在能力管理页调整后重试");
        }
        return json!({"ok": true, "invocation_id": invocation, "result": s.computer.diagnose(s, &grant.owner).await});
    }
    if browser::handles(name) {
        if !capabilities::browser_enabled(s) {
            return failed_invocation(invocation, "chat.browser.disabled", "内置浏览器能力已停用；请在「设置 → 内置能力」启用 muteki-browser");
        }
        if !capabilities::effective_tool_set(&s.store, &grant.owner).iter().any(|selected| selected == name) {
            return failed_invocation(invocation, "capability.tool_not_bound", "该会话的能力绑定未包含此浏览器工具，请在能力管理页调整后重试");
        }
        return match s.browser.invoke(s, &grant.owner, name, arguments).await {
            Ok(result) => {
                if name != "chat_browser_status" {
                    s.activity.browser(s, &grant.owner, &crate::activity::running_turn(s, &grant.owner), &result).await;
                }
                json!({"ok": true, "invocation_id": invocation, "result": result})
            }
            Err(error) => json!({"ok": false, "invocation_id": invocation, "error": error}),
        };
    }
    if capabilities::spec(name).is_some() {
        let plugin_ok = plugins::registry(s)
            .map(|registry| registry.control_enabled())
            .unwrap_or(true);
        if chat_computer::handles(name) {
            if !capabilities::computer_enabled(s) {
                return failed_invocation(invocation, "chat.computer.disabled", "电脑操控能力已停用；请在「设置 → 内置能力」启用 computer-use");
            }
        } else if name == "muteki_get_command_receipt" {
            let mode = capabilities::thread_mode(&s.store, &grant.owner);
            let collab_ok = capabilities::collab_enabled_for_thread(s, &grant.owner);
            let platform_ok = mode != "conversation" || capabilities::platform_enabled_for_thread(s, &grant.owner);
            if !plugin_ok || (!collab_ok && !platform_ok) {
                return failed_invocation(
                    invocation,
                    "chat.control.disabled",
                    "跨会话协作（muteki-collab）与安全平台桥接（muteki-platform）均已停用；请在「设置」中开启对应能力",
                );
            }
        } else if capabilities::is_collab_tool(name) {
            if !plugin_ok || !capabilities::collab_enabled_for_thread(s, &grant.owner) {
                return failed_invocation(invocation, "chat.control.disabled", "跨会话协作网关（muteki-collab）已停用；请在「设置 → 多模型协作」中启用");
            }
        } else if capabilities::is_platform_tool(name) {
            let mode = capabilities::thread_mode(&s.store, &grant.owner);
            if !plugin_ok || (mode == "conversation" && !capabilities::platform_enabled_for_thread(s, &grant.owner)) {
                return failed_invocation(invocation, "chat.platform.disabled", "安全平台桥接能力已停用；请在「设置 → 内置能力」开启，或在输入框使用 /ctf、/pentest、@Run 按需唤醒");
            }
        }
        if !capabilities::effective_tool_set(&s.store, &grant.owner)
            .iter()
            .any(|selected| selected == name)
        {
            if crate::runs::src_api::is_src_coordinator_thread(&s.store, &grant.owner)
                && capabilities::is_src_inline_worker_only_tool(name)
            {
                return failed_invocation(
                    invocation,
                    "src.coordinator.delegate_required",
                    "SRC 主协调线程不直接内联执行探测工具；请通过 muteki_start_thread(request_id=..., subagent=true, delegation_profile=\"scoped_execution\", wait=false, todo_id=..., message=...) 并行委派子线程执行，或启动后台 SRC Run。",
                );
            }
            return failed_invocation(invocation,
                "capability.tool_not_bound",
                "该会话的能力绑定未包含此工具，请在能力管理页调整后重试",
            );
        }
        if let Err(error) = capabilities::validate_arguments(name,arguments) {
            return json!({"ok":false,"invocation_id":invocation,"error":error});
        }
        if let Err(error) = crate::control::authorize_call(s, &grant.owner, name, arguments) {
            return json!({"ok":false,"invocation_id":invocation,"error":error});
        }
        let rewritten_self;
        let arguments = if arguments["thread_id"].as_str().map(str::trim) == Some(crate::control::OWN_THREAD) {
            let mut next = arguments.clone();
            next["thread_id"] = json!(grant.owner);
            rewritten_self = next;
            &rewritten_self
        } else {
            arguments
        };
        if crate::control_tools::handles(name) {
            return match crate::control_tools::invoke(s, &grant.owner, name, arguments).await {
                Ok(result) => json!({"ok": true, "invocation_id": invocation, "result": result}),
                Err(error) => json!({"ok": false, "invocation_id": invocation, "error": error}),
            };
        }
        if chat_computer::handles(name) {
            if !capabilities::enabled(s, "mcp", "computer-use") {
                return failed_invocation(invocation,"chat.computer.disabled", "电脑操控能力已停用");
            }
            let doc = s.store.get("conversation_threads", &grant.owner).ok().flatten().unwrap_or(Value::Null);
            let turn_id = doc["thread"]["state"]["running_turn_id"].as_str().unwrap_or("");
            if turn_id.is_empty() {
                return failed_invocation(invocation,"chat.computer.turn_missing", "当前聊天没有正在执行的回合");
            }
            let adapter_id = doc["runtime"]["adapter_id"].as_str().unwrap_or("");
            let image_input = meta_image_input
                .or_else(|| crate::agent_runtime::credentials::runtime_model_image_input(
                    &s.store,
                    &crate::native_chat::state_dir(s),
                    &doc["runtime"],
                ))
                .unwrap_or_else(|| {
                    crate::agent_runtime::descriptors::for_adapter(adapter_id)
                        .is_some_and(|(_, adapter)| adapter.capabilities.image_input)
                });
            return match s.computer.invoke(s, &grant.owner, turn_id, &invocation, name,
                arguments, image_input, grant.engine == "droid").await {
                Ok(outcome) => {
                    if !outcome.is_error && name != "chat_computer_js_reset"
                        && arguments.get("completion_summary").is_none()
                    {
                        s.activity.computer(s, &grant.owner, turn_id, &outcome.surface, &outcome.result).await;
                    }
                    json!({"ok":!outcome.is_error,"invocation_id":invocation,
                        "result":outcome.result,"_mcp_images":outcome.images})
                }
                Err(reason) => json!({"ok":false,"invocation_id":invocation,"error":reason}),
            };
        }
        if let Some(kind) = capabilities::src_query_type(name) {
            return match crate::operations_api::src_tool_query(s, token, &grant.owner, kind, arguments) {
                Ok(result) => json!({"ok": true, "invocation_id": invocation, "result": result}),
                Err(error) => json!({"ok": false, "invocation_id": invocation, "error": error}),
            };
        }
        if let Some(kind) = capabilities::src_command_type(name) {
            return match crate::operations_api::src_tool_command(s, token, &grant.owner, kind, arguments).await {
                Ok(receipt) => json!({"ok":receipt["error"].is_null(),
                    "invocation_id":invocation,"error":receipt["error"],"receipt":receipt}),
                Err(error) => json!({"ok":false,"invocation_id":invocation,"error":error}),
            };
        }
        if let Some(kind) = crate::chat_subagents::tool_query_type(name) {
            return match crate::chat_subagents::query(s,&grant.owner,kind,&crate::control_tools::engine_arguments(arguments)).await {
                Ok(result) => json!({"ok":true,"invocation_id":invocation,"result":result}),
                Err((_, axum::Json(body))) => json!({"ok":false,"invocation_id":invocation,"error":body["error"]}),
            };
        }
        if let Some(kind) = crate::chat_subagents::tool_command_type(name) {
            let command_id = arguments["idempotency_key"].as_str().filter(|value|!value.is_empty())
                .map(str::to_owned).unwrap_or_else(||format!("cmd-{}",uuid::Uuid::new_v4()));
            let body = json!({"command_id":command_id,"command_type":kind,"payload":crate::control_tools::engine_arguments(arguments),
                "actor":{"kind":"agent","id":grant.owner}});
            return match crate::chat_subagents::command(s.clone(),grant.owner.clone(),body,None).await {
                Ok(axum::Json(value)) => {
                    let target = arguments["thread_id"].as_str()
                        .or_else(|| value["receipt"]["output"]["thread_id"].as_str())
                        .unwrap_or(&grant.owner);
                    crate::control::audit(s,&grant.owner,target,name,
                        json!({"command_id":command_id,"state":value["receipt"]["state"]}));
                    json!({"ok":true,"invocation_id":invocation,"receipt":value["receipt"]})
                },
                Err((_, axum::Json(body))) => json!({"ok":false,"invocation_id":invocation,"error":body["error"]}),
            };
        }
        return match crate::operations_api::invoke_bound_tool(s,token,&binding,name,arguments).await {
            Ok(mut result) => {
                result["ok"] = json!(result["receipt"]["error"].is_null());
                result["invocation_id"] = json!(invocation);
                if result["receipt"]["error"].is_object() { result["error"] = result["receipt"]["error"].clone(); }
                result
            },
            Err((_,axum::Json(body))) => json!({"ok":false,"invocation_id":invocation,"error":body["error"]}),
        };
    }
    match crate::runs::src_api::resolve_authorized_src_caller(s, &grant.owner) {
        Ok(None) => {}
        Ok(Some(caller)) => match caller.task["lifecycle"]["state"].as_str() {
            Some("paused") => {
                return failed_invocation(
                    invocation,
                    "src.task_paused",
                    "SRC Task 已暂停，请先恢复后再调用插件工具",
                );
            }
            Some("archived" | "destroyed") => {
                return failed_invocation(
                    invocation,
                    "src.task_inactive",
                    "SRC Task 已归档或销毁，禁止调用插件工具",
                );
            }
            _ => {}
        },
        Err(error) => {
            return failed_invocation(invocation, error.code, &error.message);
        }
    }
    let Ok(registry) = plugins::registry(s) else {
        return failed_invocation(invocation,
            "chat.plugin.unavailable",
            "聊天插件调用失败或已停用，请在聊天插件页检查连接",
        );
    };
    match registry
        .invoke(&grant.engine, name, arguments.clone(), "chat", "")
        .await
    {
        Ok(result) => json!({"ok": true, "invocation_id": invocation, "result": result}),
        Err(error) => {
            json!({"ok": false, "invocation_id": invocation, "error": {"code": error.code, "message": error.message}})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use axum::http::HeaderValue;

    fn state() -> (tempfile::TempDir, Arc<AppState>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store
            .put("system", "state_dir", &json!(dir.path()))
            .unwrap();
        store.put("system", "port", &json!(8100)).unwrap();
        (dir, Arc::new(AppState::new(store)))
    }

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    async fn text_of(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn a_session_token_is_required_and_scoped_to_one_session() {
        let (_dir, s) = state();
        let response = serve(State(s.clone()), HeaderMap::new(), Bytes::new()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            "Bearer realm=\"muteki-control\""
        );
        let unknown = serve(State(s.clone()), headers("nope"), Bytes::new()).await;
        assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
        let token = s.capability_gateway.issue("t1", "codex").unwrap();
        let replaced = s.capability_gateway.issue("t1", "codex").unwrap();
        assert!(
            s.capability_gateway.resolve(&token).is_none(),
            "a new session retires the old token"
        );
        s.capability_gateway.rebind("t1", "t2");
        assert_eq!(s.capability_gateway.resolve(&replaced).unwrap().owner, "t2");
        s.capability_gateway.release("t2");
        assert!(s.capability_gateway.resolve(&replaced).is_none());
    }

    #[tokio::test]
    async fn the_legacy_handshake_lists_the_bound_tools() {
        let (_dir, s) = state();
        let token = s.capability_gateway.issue("t1", "codex").unwrap();
        let request = |value: Value| Bytes::from(value.to_string());
        let initialize = serve(
            State(s.clone()),
            headers(&token),
            request(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})),
        )
        .await;
        assert_eq!(initialize.status(), StatusCode::OK);
        let body = text_of(initialize).await;
        assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(body["result"]["serverInfo"]["name"], SERVER_NAME);
        let listed = serve(
            State(s.clone()),
            headers(&token),
            request(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})),
        )
        .await;
        let body = text_of(listed).await;
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(
            names.contains(&"muteki_list_threads"),
            "control tools are published: {names:?}"
        );
        let notification = serve(
            State(s.clone()),
            headers(&token),
            request(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
        )
        .await;
        assert_eq!(notification.status(), StatusCode::ACCEPTED);
        let unknown = serve(
            State(s.clone()),
            headers(&token),
            request(json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})),
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::OK);
        assert_eq!(text_of(unknown).await["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn src_main_thread_lists_and_executes_its_source_tool_set() {
        let (_dir, s) = state();
        let created = crate::runs::src_api::generic_task_create(
            s.clone(),
            &json!({"command_id":"src-gateway-create","payload":{
                "title":"SRC gateway acceptance","goal":"Inspect an authorized local target",
                "authorization":{"targets":["http://127.0.0.1:4280/"]}}}),
            "operator:local-user",
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK);
        let created = text_of(created).await;
        let task_id = created["receipt"]["output"]["task_id"].as_str().unwrap();
        let thread_id = created["receipt"]["output"]["thread_id"].as_str().unwrap();
        let token = s.capability_gateway.issue(thread_id, "pi").unwrap();
        capabilities::issue_grant(&s, thread_id,
            &json!({"agent_session_id":"src-gateway-session","instance_id":"test","adapter_id":"pi.rpc"}),
            &digest(&token),
        ).unwrap();
        let request = |id: u64, method: &str, params: Value| {
            Bytes::from(
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
            )
        };
        let listed = text_of(
            serve(
                State(s.clone()),
                headers(&token),
                request(1, "tools/list", json!({})),
            )
            .await,
        )
        .await;
        let names: std::collections::HashSet<&str> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        let src_names: Vec<String> = capabilities::catalog()
            .iter()
            .filter(|tool| tool["default_modes"] == json!(["src"]))
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect();
        assert!(!src_names.is_empty());
        assert!(
            src_names.iter().all(|name| names.contains(name.as_str())),
            "SRC tools missing from main chat"
        );
        assert!(!names.contains("muteki_run_ensure"));
        assert!(!names.contains("muteki_run_start"));

        let task = text_of(
            serve(
                State(s.clone()),
                headers(&token),
                request(
                    2,
                    "tools/call",
                    json!({"name":"muteki_get_src_task","arguments":{}}),
                ),
            )
            .await,
        )
        .await;
        assert_eq!(task["result"]["structuredContent"]["ok"], true, "{task}");
        assert_eq!(
            task["result"]["structuredContent"]["result"]["task_id"],
            task_id
        );
        let binding = text_of(serve(State(s.clone()),headers(&token),request(4,"tools/call",
            json!({"name":"muteki_list_src_records","arguments":{"kind":"task_binding"}}))).await).await;
        assert_eq!(binding["result"]["structuredContent"]["ok"],true,"{binding}");
        assert_eq!(binding["result"]["structuredContent"]["result"]["records"][0]["payload"]["thread_id"],thread_id);
        let saved = text_of(
            serve(
                State(s.clone()),
                headers(&token),
                request(
                    3,
                    "tools/call",
                    json!({"name":"muteki_coverage_item_save","arguments":{
                "item_id":"gateway-item","method":"GET","target":"http://127.0.0.1:4280/",
                "hypothesis":"Check the local test target"}}),
                ),
            )
            .await,
        )
        .await;
        assert_eq!(saved["result"]["structuredContent"]["ok"], true, "{saved}");
    }

    #[tokio::test]
    async fn the_current_revision_requires_its_headers() {
        let (_dir, s) = state();
        let token = s.capability_gateway.issue("t1", "codex").unwrap();
        let mut head = headers(&token);
        head.insert(
            "mcp-protocol-version",
            HeaderValue::from_static(PROTOCOL_VERSION),
        );
        head.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        let call = |head: HeaderMap, value: Value| {
            serve(State(s.clone()), head, Bytes::from(value.to_string()))
        };
        let missing_meta = call(
            head.clone(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        assert_eq!(missing_meta.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            text_of(missing_meta).await["error"]["code"],
            HEADER_MISMATCH
        );
        let mut stale = head.clone();
        stale.insert(
            "mcp-protocol-version",
            HeaderValue::from_static("2027-01-01"),
        );
        let unsupported = call(
            stale,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        assert_eq!(
            text_of(unsupported).await["error"]["code"],
            UNSUPPORTED_PROTOCOL_VERSION
        );
        let mut complete = head.clone();
        complete.insert("mcp-method", HeaderValue::from_static("tools/list"));
        let meta = json!({"_meta": {META_PROTOCOL_VERSION: PROTOCOL_VERSION, META_CLIENT_CAPABILITIES: {}}});
        let listed = call(
            complete,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": meta}),
        )
        .await;
        assert_eq!(listed.status(), StatusCode::OK);
        let body = text_of(listed).await;
        assert_eq!(body["result"]["cacheScope"], "private");
        assert_eq!(
            body["result"]["_meta"][META_SERVER_INFO]["name"],
            SERVER_NAME
        );
        let no_accept = {
            let mut head = headers(&token);
            head.insert(
                "mcp-protocol-version",
                HeaderValue::from_static(PROTOCOL_VERSION),
            );
            call(
                head,
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            )
            .await
        };
        assert_eq!(no_accept.status(), StatusCode::NOT_ACCEPTABLE);
    }

    #[tokio::test]
    async fn a_tool_call_runs_the_product_handler_and_reports_refusals() {
        let (_dir, s) = state();
        let token = s.capability_gateway.issue("t1", "codex").unwrap();
        capabilities::issue_grant(&s, "t1", &json!({
            "agent_session_id":"test-session","instance_id":"test","adapter_id":"codex.app_server"
        }), &digest(&token)).unwrap();
        let call = |value: Value| {
            serve(
                State(s.clone()),
                headers(&token),
                Bytes::from(value.to_string()),
            )
        };
        let created = call(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "muteki_create_project", "arguments": {"name": "Gateway project", "idempotency_key": "cmd-1"}}}))
        .await;
        let body = text_of(created).await;
        assert_eq!(body["result"]["isError"], false, "{body}");
        assert_eq!(body["result"]["structuredContent"]["ok"], true);
        let projects = call(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "muteki_list_projects", "arguments": {}}}))
        .await;
        let body = text_of(projects).await;
        assert_eq!(body["result"]["structuredContent"]["ok"], true);
        let missing = call(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "muteki_get_thread", "arguments": {"thread_id": "absent"}}}))
        .await;
        let body = text_of(missing).await;
        assert_eq!(body["result"]["isError"], true);
        assert_eq!(body["result"]["structuredContent"]["error"]["code"], "conversation.thread.not_found", "{body}");
        let unknown = call(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "nope_tool", "arguments": {}}}))
        .await;
        let body = text_of(unknown).await;
        assert_eq!(
            body["result"]["structuredContent"]["error"]["code"],
            "chat.plugin.unavailable"
        );
    }

    #[tokio::test]
    async fn disabling_the_control_server_withdraws_its_tools() {
        let (_dir, s) = state();
        let token = s.capability_gateway.issue("t1", "codex").unwrap();
        capabilities::issue_grant(&s, "t1", &json!({
            "agent_session_id":"test-session","instance_id":"test","adapter_id":"codex.app_server"
        }), &digest(&token)).unwrap();
        let path = crate::native_chat::state_dir(&s).join("capability_management.json");
        std::fs::write(&path, json!({"mcp": {SERVER_NAME: false}}).to_string()).unwrap();
        assert!(!enabled(&s));
        let listed = serve(
            State(s.clone()),
            headers(&token),
            Bytes::from(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string()),
        )
        .await;
        let body = text_of(listed).await;
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(!names.is_empty());
        assert!(
            !names.iter().any(|name| capabilities::is_collab_tool(name)),
            "collab tools should withdraw: {names:?}"
        );
        assert!(
            names.iter().any(|name| name.starts_with("chat_browser_")),
            "{names:?}"
        );
        assert!(
            names.contains(&"chat_computer_status"),
            "{names:?}"
        );
        assert!(
            names.contains(&PLATFORM_DISCOVER_TOOL) && names.contains(&PLATFORM_CALL_TOOL),
            "{names:?}"
        );
        let refused = serve(
            State(s.clone()),
            headers(&token),
            Bytes::from(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "muteki_list_projects", "arguments": {}}}).to_string()),
        )
        .await;
        assert_eq!(
            text_of(refused).await["result"]["structuredContent"]["error"]["code"],
            "chat.control.disabled"
        );
        assert!(registration(&s, "t1", "codex", "codex.app_server").is_none());
    }
}
