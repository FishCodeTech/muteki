//! Pure policy of the desktop bridge: errors, manifest, validation and
//! containment rules shared by every native handler.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq)]
pub struct BridgeError {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl BridgeError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), retryable: false }
    }
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
    pub fn to_json(&self) -> Value {
        json!({"code": self.code, "message": self.message, "retryable": self.retryable, "source": "desktop"})
    }
}

pub type Outcome = Result<Value, BridgeError>;

pub fn manifest() -> Value {
    let preview_sub = json!({
        "navigation": true,
        "screenshot": true,
        "consoleLogs": true,
        "networkLogs": true,
        "devtools": true,
        "clearWebsiteData": true,
        "clearHttpAuth": true,
    });
    let preview_cap = |supported: bool| json!({"supported": supported, "host": "desktop-client"});
    json!({
        "version": 1,
        "host": "desktop-client",
        "deepLink": {
            "supported": true,
            "host": "desktop-client",
            "schemes": ["muteki"],
        },
        "entries": {
            "pathSelection": {"supported": true, "host": "desktop-client"},
            "workspaceFileActions": {"supported": true, "host": "desktop-client", "code": "desktop.mapping_manual", "reason": "需要用户选择本机对应目录；文件同步尚未验证。"},
            "preview": {
                "supported": true,
                "host": "desktop-client",
                "navigation": true,
                "screenshot": true,
                "consoleLogs": true,
                "networkLogs": true,
                "devtools": true,
                "clearWebsiteData": true,
                "clearHttpAuth": true,
                "features": preview_sub,
                "capabilities": {
                    "navigation": preview_cap(true),
                    "screenshot": preview_cap(true),
                    "consoleLogs": preview_cap(true),
                    "networkLogs": preview_cap(true),
                    "devtools": preview_cap(true),
                    "clearWebsiteData": preview_cap(true),
                    "clearHttpAuth": preview_cap(true),
                }
            },
            "attachmentCache": {"supported": true, "host": "desktop-client"},
            "terminal": {"supported": true, "host": "service"},
            "microphone": {"supported": true, "host": "desktop-client"},
            "notifications": {"supported": true, "host": "desktop-client"},
            "deepLinks": {"supported": true, "host": "desktop-client", "schemes": ["muteki"]},
            "deepLink": {"supported": true, "host": "desktop-client", "schemes": ["muteki"]},
        }
    })
}

pub fn string<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

pub fn required<'a>(params: &'a Value, key: &str, code: &'static str, message: &str) -> Result<&'a str, BridgeError> {
    string(params, key).filter(|value| !value.is_empty()).ok_or_else(|| BridgeError::new(code, message))
}

/// A workspace-relative POSIX path: never absolute, never escaping.
pub fn relative_path(value: &str) -> Result<&str, BridgeError> {
    let bad = value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains('\0')
        || value.as_bytes().get(1) == Some(&b':')
        || value.split('/').any(|part| part.is_empty() || part == "." || part == "..");
    if bad {
        return Err(BridgeError::new("desktop.workspace_file_invalid", "请选择工作区内的相对文件路径。"));
    }
    Ok(value)
}

/// Resolves `relative` (or the root itself) inside a granted client root.
/// Both sides are canonicalized so a symlink cannot leave the grant.
pub fn resolve_in_root(granted: &Path, relative: Option<&str>) -> Result<PathBuf, BridgeError> {
    let root = granted
        .canonicalize()
        .map_err(|error| BridgeError::new("desktop.workspace_scope_changed", format!("本机目录不可用：{error}")))?;
    if root != granted {
        return Err(BridgeError::new("desktop.workspace_scope_changed", "本机目录映射已改变，请重新选择。"));
    }
    let Some(relative) = relative else { return Ok(root) };
    let target = root
        .join(relative_path(relative)?)
        .canonicalize()
        .map_err(|error| BridgeError::new("desktop.workspace_file_invalid", format!("文件不可用：{error}")))?;
    if !target.starts_with(&root) || target == root {
        return Err(BridgeError::new("desktop.workspace_file_outside", "文件不属于已选择的本机工作区。"));
    }
    Ok(target)
}

pub const EDITORS: [(&str, &str, &[&str]); 4] = [
    ("code", "VS Code", &["/usr/local/bin/code", "/opt/homebrew/bin/code", "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"]),
    ("cursor", "Cursor", &["/usr/local/bin/cursor", "/opt/homebrew/bin/cursor", "/Applications/Cursor.app/Contents/Resources/app/bin/cursor"]),
    ("windsurf", "Windsurf", &["/usr/local/bin/windsurf", "/opt/homebrew/bin/windsurf", "/Applications/Windsurf.app/Contents/Resources/app/bin/windsurf"]),
    ("zed", "Zed", &["/usr/local/bin/zed", "/opt/homebrew/bin/zed", "/Applications/Zed.app/Contents/MacOS/cli"]),
];

#[derive(Debug, PartialEq)]
pub struct EditorCli {
    pub name: &'static str,
    pub label: &'static str,
    pub file: PathBuf,
}

/// The editor CLI to launch; `None` means the system default application.
/// GUI-launched apps do not inherit the login shell PATH, so the usual
/// install locations are probed explicitly.
pub fn find_editor(preferred: &str, path_env: &str, executable: impl Fn(&Path) -> bool) -> Result<Option<EditorCli>, BridgeError> {
    if preferred != "auto" && preferred != "system" && !EDITORS.iter().any(|(name, ..)| *name == preferred) {
        return Err(BridgeError::new("desktop.editor_invalid", "不支持的编辑器选项。"));
    }
    if preferred == "system" {
        return Ok(None);
    }
    for (name, label, extra) in EDITORS {
        if preferred != "auto" && preferred != name {
            continue;
        }
        let candidates = path_env
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| Path::new(dir).join(name))
            .chain(extra.iter().map(PathBuf::from));
        for candidate in candidates {
            if executable(&candidate) {
                return Ok(Some(EditorCli { name, label, file: candidate }));
            }
        }
    }
    if preferred != "auto" {
        let label = EDITORS.iter().find(|(name, ..)| *name == preferred).map_or(preferred, |(_, label, _)| label);
        return Err(BridgeError::new("desktop.editor_not_found", format!("未找到 {label} 命令行，请安装后重试，或在设置中改用其他编辑器。")));
    }
    Ok(None)
}

/// Zed takes `path:line`; the VS Code family needs `--goto`.
pub fn editor_args(name: &str, target: &Path, line: Option<u64>) -> Vec<String> {
    let path = target.to_string_lossy().into_owned();
    match line {
        None => vec![path],
        Some(line) if name == "zed" => vec![format!("{path}:{line}")],
        Some(line) => vec!["--goto".into(), format!("{path}:{line}")],
    }
}

/// An http(s) URL without embedded credentials.
pub fn web_url(value: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some() && url.username().is_empty() && url.password().is_none()).then_some(url)
}

#[allow(dead_code)]
#[path = "browser_policy.rs"]
mod browser_policy;

pub fn preview_url(value: &str, _ui_origin: &str, _api_port: u16) -> Result<reqwest::Url, BridgeError> {
    web_url(value).ok_or_else(|| BridgeError::new("desktop.preview_url_invalid", "预览仅支持 HTTP/HTTPS 地址。"))
}

/// Whether opening `url` externally requires explicit user confirmation.
/// Same-origin UI URLs do not prompt; external http(s) URLs prompt the user.
pub fn needs_external_url_confirmation(url: &reqwest::Url, ui_origin: &str) -> bool {
    ui_origin.is_empty() || url.origin().ascii_serialization() != ui_origin
}

pub const DEFAULT_ACCELERATORS: [(&str, &str); 3] = [("new-chat", "CmdOrCtrl+Shift+N"), ("search", "CmdOrCtrl+K"), ("sidebar", "CmdOrCtrl+B")];

pub fn default_accelerators() -> BTreeMap<String, String> {
    DEFAULT_ACCELERATORS.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuLabels {
    pub about: &'static str,
    pub settings: &'static str,
    pub shortcuts: &'static str,
    pub hide: &'static str,
    pub hide_others: &'static str,
    pub show_all: &'static str,
    pub quit: &'static str,
    pub edit: &'static str,
    pub undo: &'static str,
    pub redo: &'static str,
    pub cut: &'static str,
    pub copy: &'static str,
    pub paste: &'static str,
    pub select_all: &'static str,
    pub chat: &'static str,
    pub new_chat: &'static str,
    pub new_window: &'static str,
    pub search: &'static str,
    pub view: &'static str,
    pub sidebar: &'static str,
    pub back: &'static str,
    pub forward: &'static str,
    pub reload: &'static str,
    pub full_screen: &'static str,
    pub window: &'static str,
    pub minimize: &'static str,
    pub zoom: &'static str,
    pub close_window: &'static str,
}

pub fn menu_labels(locale: &str) -> MenuLabels {
    if locale.trim().to_ascii_lowercase().starts_with("en") {
        MenuLabels {
            about: "About Muteki",
            settings: "Settings…",
            shortcuts: "Keyboard Shortcuts",
            hide: "Hide Muteki",
            hide_others: "Hide Others",
            show_all: "Show All",
            quit: "Quit Muteki",
            edit: "Edit",
            undo: "Undo",
            redo: "Redo",
            cut: "Cut",
            copy: "Copy",
            paste: "Paste",
            select_all: "Select All",
            chat: "Chat",
            new_chat: "New Chat",
            new_window: "New Window",
            search: "Search Chats",
            view: "View",
            sidebar: "Toggle Sidebar",
            back: "Back",
            forward: "Forward",
            reload: "Reload Page",
            full_screen: "Enter Full Screen",
            window: "Window",
            minimize: "Minimize",
            zoom: "Zoom",
            close_window: "Close Window",
        }
    } else {
        MenuLabels {
            about: "关于 Muteki",
            settings: "设置…",
            shortcuts: "键盘快捷键",
            hide: "隐藏 Muteki",
            hide_others: "隐藏其他",
            show_all: "全部显示",
            quit: "退出 Muteki",
            edit: "编辑",
            undo: "撤销",
            redo: "重做",
            cut: "剪切",
            copy: "拷贝",
            paste: "粘贴",
            select_all: "全选",
            chat: "对话",
            new_chat: "新建对话",
            new_window: "新建窗口",
            search: "搜索对话",
            view: "视图",
            sidebar: "切换会话侧栏",
            back: "后退",
            forward: "前进",
            reload: "刷新当前页面",
            full_screen: "进入全屏幕",
            window: "窗口",
            minimize: "最小化",
            zoom: "缩放",
            close_window: "关闭窗口",
        }
    }
}

/// The nsh keystroke for an Electron-style accelerator; `None` when the
/// accelerator is not one this menu can express.
pub fn accelerator_to_shortcut(accelerator: &str) -> Option<String> {
    let mut parts: Vec<&str> = accelerator.split('+').collect();
    if accelerator.ends_with("++") || accelerator.is_empty() {
        return None;
    }
    let key = parts.pop()?;
    if parts.len() > 4 {
        return None;
    }
    let mut out = Vec::new();
    for part in parts {
        let name = match part {
            "CmdOrCtrl" => "mod",
            "Ctrl" => "ctrl",
            "Alt" => "alt",
            "Shift" => "shift",
            _ => return None,
        };
        if out.contains(&name) {
            return None;
        }
        out.push(name);
    }
    let key = match key {
        k if k.len() == 1 && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) => k.to_ascii_lowercase(),
        k if k.len() == 1 && "[],./\\;'`=-".contains(k) => k.to_owned(),
        k if k.starts_with('F') && k[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) && !k[1..].starts_with('0') => k.to_ascii_lowercase(),
        "Up" | "Down" | "Left" | "Right" | "Enter" | "Escape" | "Space" | "Tab" | "Backspace" | "Delete" => key.to_ascii_lowercase(),
        _ => return None,
    };
    out.push(&key);
    Some(out.join("+"))
}

/// Synchronizes menu accelerators from a renderer request using full-replace
/// semantics: resets from [`default_accelerators`] on each sync so omitted or
/// `null` keys revert to their defaults, while `""` unbinds the menu shortcut.
pub fn merge_accelerators(_current: &BTreeMap<String, String>, input: &Value) -> Result<BTreeMap<String, String>, BridgeError> {
    let mut next = default_accelerators();
    let source = input
        .get("accelerators")
        .or_else(|| input.get("shortcuts"))
        .filter(|v| v.is_object())
        .unwrap_or(input);
    for (key, _) in DEFAULT_ACCELERATORS {
        match source.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::String(value)) if value.is_empty() || accelerator_to_shortcut(value).is_some() => {
                next.insert(key.into(), value.clone());
            }
            Some(other) => {
                let text: String = other.to_string().chars().take(40).collect();
                return Err(BridgeError::new("desktop.accelerator_invalid", format!("无效的菜单快捷键：{text}")));
            }
        }
    }
    Ok(next)
}

pub fn sync_accelerators(input: &Value) -> Result<(BTreeMap<String, String>, Option<String>), BridgeError> {
    let accelerators = merge_accelerators(&default_accelerators(), input)?;
    let locale = string(input, "locale").filter(|s| !s.is_empty()).map(str::to_owned);
    Ok((accelerators, locale))
}

/// The native terminal relay accepts only this thread's own stream path.
pub fn terminal_path(thread_id: &str, path: &str) -> Result<(), BridgeError> {
    let invalid = || BridgeError::new("desktop.terminal_scope_invalid", "终端必须属于当前工作台会话。");
    let (route, query) = path.split_once('?').ok_or_else(invalid)?;
    let segments: Vec<&str> = route.split('/').collect();
    let thread = percent_encode(thread_id);
    let ok = segments.len() == 9
        && segments[0].is_empty()
        && segments[1] == "api"
        && segments[2] == "threads"
        && segments[3] == thread
        && segments[4] == "workspace"
        && segments[5] == "terminal"
        && segments[6] == "sessions"
        && !segments[7].is_empty()
        && segments[8] == "stream"
        && query.split('&').all(|pair| pair.split_once('=').is_some_and(|(key, value)| key == "ticket" && !value.is_empty()));
    if ok { Ok(()) } else { Err(invalid()) }
}

/// `encodeURIComponent`.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The visible chat thread of a UI route (`/chat/<id>` or `/src` with thread parameter).
pub fn route_thread(route: &str) -> Option<String> {
    let without_hash = route.split('#').next()?;
    let (path, query) = without_hash.split_once('?').map_or((without_hash, ""), |(p, q)| (p, q));
    if let Some(rest) = path.strip_prefix("/chat/") {
        let id = rest.split('/').next()?;
        if !id.is_empty() {
            return Some(percent_decode(id));
        }
    }
    if path == "/src" || path.starts_with("/src/") {
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                if matches!(key, "thread" | "threadId" | "id") && !value.is_empty() {
                    return Some(percent_decode(value));
                }
            }
        }
    }
    None
}

/// Combines the route's thread with explicit thread IDs reported by the frontend
/// (for example multi-pane or `/src` workbench views), deduplicated in order.
pub fn collect_visible_threads(route: &str, explicit: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(from_route) = route_thread(route) {
        out.push(from_route);
    }
    for id in explicit {
        let trimmed = id.trim();
        if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_owned());
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepLink {
    pub url: String,
    pub route: String,
    pub thread_id: Option<String>,
    pub service_id: Option<String>,
    pub identity_id: Option<String>,
}

fn safe_route_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.contains('\\')
        && !segment.contains('\0')
        && !segment.chars().any(char::is_control)
}

/// Parses and validates a `muteki://` deep-link URL into an internal UI route.
pub fn parse_deep_link(value: &str) -> Result<DeepLink, BridgeError> {
    let invalid = || BridgeError::new("desktop.deep_link_invalid", "无效的 Muteki 链接。");
    let raw = value.trim();
    if raw.is_empty() || raw.contains('\\') || raw.contains('\0') || raw.chars().any(char::is_control) {
        return Err(invalid());
    }
    let Some(after_scheme) = raw.strip_prefix("muteki://") else {
        return Err(invalid());
    };
    let raw_before_query = after_scheme.split(['?', '#']).next().unwrap_or("");
    let raw_parts: Vec<String> = raw_before_query
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    if raw_parts.iter().any(|p| p == "." || p == ".." || !safe_route_segment(p)) {
        return Err(invalid());
    }
    let parsed = reqwest::Url::parse(raw).map_err(|_| invalid())?;
    if parsed.scheme() != "muteki" || !parsed.username().is_empty() || parsed.password().is_some() || parsed.port().is_some() {
        return Err(invalid());
    }

    let mut service_id = None;
    let mut identity_id = None;
    let mut query_route = None;
    let mut query_thread = None;
    let mut passthrough_query: Vec<(String, String)> = Vec::new();

    for (k, v) in parsed.query_pairs() {
        let key = k.trim();
        let val = v.trim();
        if val.is_empty() || val.contains('\0') || val.chars().any(char::is_control) {
            continue;
        }
        match key {
            "serviceId" | "service" | "service_id" => service_id = Some(val.to_owned()),
            "identityId" | "identity" | "identity_id" => identity_id = Some(val.to_owned()),
            "path" | "route" => query_route = Some(val.to_owned()),
            "thread" | "threadId" | "thread_id" => {
                query_thread = Some(val.to_owned());
                passthrough_query.push((key.to_owned(), val.to_owned()));
            }
            _ => passthrough_query.push((key.to_owned(), val.to_owned())),
        }
    }

    let host = parsed.host_str().unwrap_or("");
    let raw_path = parsed.path();
    if raw_path.contains('\\') || raw_path.contains('\0') || raw_path.contains("//") {
        return Err(invalid());
    }

    let path_segments: Vec<String> = raw_path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    if path_segments.iter().any(|s| !safe_route_segment(s)) {
        return Err(invalid());
    }

    let (route, thread_id) = match (host, path_segments.as_slice()) {
        ("chat" | "thread" | "threads", [thread]) => {
            let encoded = percent_encode(thread);
            (format!("/chat/{encoded}"), Some(thread.clone()))
        }
        ("", [kind, thread]) if matches!(kind.as_str(), "chat" | "thread" | "threads") => {
            let encoded = percent_encode(thread);
            (format!("/chat/{encoded}"), Some(thread.clone()))
        }
        ("src", []) => {
            let mut route = "/src".to_owned();
            if let Some(t) = &query_thread {
                if !safe_route_segment(t) {
                    return Err(invalid());
                }
                route = format!("/src?thread={}", percent_encode(t));
            }
            (route, query_thread)
        }
        ("", [single]) if single == "src" => {
            let mut route = "/src".to_owned();
            if let Some(t) = &query_thread {
                if !safe_route_segment(t) {
                    return Err(invalid());
                }
                route = format!("/src?thread={}", percent_encode(t));
            }
            (route, query_thread)
        }
        ("src", [thread]) => {
            let encoded = percent_encode(thread);
            (format!("/src?thread={encoded}"), Some(thread.clone()))
        }
        ("", [first, thread]) if first == "src" => {
            let encoded = percent_encode(thread);
            (format!("/src?thread={encoded}"), Some(thread.clone()))
        }
        ("settings", rest) if rest.len() <= 1 => {
            let route = match rest.first() {
                Some(section) => format!("/settings/{}", percent_encode(section)),
                None => "/settings".to_owned(),
            };
            (route, None)
        }
        ("", [first, rest @ ..]) if first == "settings" && rest.len() <= 1 => {
            let route = match rest.first() {
                Some(section) => format!("/settings/{}", percent_encode(section)),
                None => "/settings".to_owned(),
            };
            (route, None)
        }
        ("open" | "navigate", []) => {
            if let Some(target_route) = query_route {
                if !target_route.starts_with('/') || target_route.starts_with("//") || target_route.contains("..") || target_route.contains('\\') {
                    return Err(invalid());
                }
                let thread = route_thread(&target_route);
                let base = target_route.split(['?', '#']).next().unwrap_or("");
                let allowed = base == "/src"
                    || base == "/settings"
                    || base.starts_with("/settings/")
                    || (base.starts_with("/chat/") && thread.is_some());
                if !allowed {
                    return Err(invalid());
                }
                (target_route, thread)
            } else if let Some(thread) = query_thread {
                if !safe_route_segment(&thread) {
                    return Err(invalid());
                }
                (format!("/chat/{}", percent_encode(&thread)), Some(thread))
            } else {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    };

    Ok(DeepLink {
        url: raw.to_owned(),
        route,
        thread_id,
        service_id,
        identity_id,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeState {
    pub origin: String,
    pub service_id: String,
    pub identity_id: String,
    pub scope_version: u64,
    pub connection_version: u64,
}

impl Default for ScopeState {
    fn default() -> Self {
        Self {
            origin: String::new(),
            service_id: String::new(),
            identity_id: String::new(),
            scope_version: 1,
            connection_version: 1,
        }
    }
}

impl ScopeState {
    #[cfg(test)]
    pub fn new(origin: impl Into<String>) -> Self {
        Self {
            origin: origin.into(),
            service_id: String::new(),
            identity_id: String::new(),
            scope_version: 1,
            connection_version: 1,
        }
    }

    pub fn update(&mut self, origin: &str, service_id: &str, identity_id: &str) -> bool {
        if self.origin == origin && self.service_id == service_id && self.identity_id == identity_id {
            return false;
        }
        let initial_identity_bind = self.service_id.is_empty()
            && self.identity_id.is_empty()
            && (self.origin.is_empty() || self.origin == origin)
            && !service_id.is_empty()
            && !identity_id.is_empty();
        self.origin = origin.to_owned();
        self.service_id = service_id.to_owned();
        self.identity_id = identity_id.to_owned();
        if initial_identity_bind {
        } else {
            self.scope_version += 1;
            self.connection_version += 1;
        }
        true
    }
}

pub fn is_scope_bound_method(method: &str) -> bool {
    matches!(
        method,
        "selectWorkspaceRoot"
            | "openWorkspaceFile"
            | "openWorkspaceInEditor"
            | "notification.status"
            | "notification.request"
            | "notification.send"
            | "notification.test"
            | "notification.sound"
            | "notification.badge"
            | "attachment.cache"
            | "attachment.restore"
            | "attachment.remove"
            | "terminal.authorize"
            | "preview.open"
            | "preview.exec"
            | "preview.input"
            | "preview.navigate"
            | "preview.action"
            | "reportPreview.present"
            | "reportPreview.hide"
            | "reportPreview.scroll"
            | "reportPreview.scrollBy"
            | "reportPreview.focus"
            | "reportPreview.pointer"
            | "reportPreview.close"
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallEnvelope {
    pub id: Value,
    pub window_id: String,
    pub document_id: String,
    pub scope_version: u64,
    pub connection_version: u64,
    pub method: String,
    pub scope_bound: bool,
}

impl CallEnvelope {
    pub fn validate_reply(&self, current_document_id: &str, current_scope_version: u64) -> Result<(), BridgeError> {
        if !self.document_id.is_empty() && !current_document_id.is_empty() && self.document_id != current_document_id {
            return Err(BridgeError::new("desktop.document_stale", "页面已刷新或切换，操作结果已过期。"));
        }
        if self.scope_bound && self.scope_version != current_scope_version {
            return Err(BridgeError::new("desktop.scope_changed", "工作台服务或身份已改变，请在当前工作台重试。"));
        }
        Ok(())
    }
}

/// Normalizes a console log entry to match the desktop preview contract:
/// `{ level, message, text, url, timestamp, at }`.
pub fn normalize_console_log(raw: &Value) -> Value {
    let raw_level = raw["level"].as_str().unwrap_or("info");
    let level = match raw_level {
        "debug" | "verbose" => "verbose",
        "warn" | "warning" => "warning",
        "error" => "error",
        _ => "info",
    };
    let text = raw["message"]
        .as_str()
        .or_else(|| raw["text"].as_str())
        .unwrap_or("")
        .to_owned();
    let url = raw["url"].as_str().unwrap_or("").to_owned();
    let timestamp = raw["timestamp"]
        .as_str()
        .or_else(|| raw["at"].as_str())
        .unwrap_or("")
        .to_owned();
    let mut out = json!({
        "level": level,
        "message": text,
        "text": text,
        "url": url,
        "timestamp": timestamp,
        "at": timestamp,
    });
    if let Some(line) = raw.get("line").and_then(Value::as_u64) {
        out["line"] = json!(line);
    }
    out
}

/// Normalizes a network log entry to match the desktop preview contract:
/// `{ method, url, status, statusText, ok, duration, initiatorType, timestamp, at, error? }`.
pub fn normalize_network_log(raw: &Value) -> Value {
    let method = raw["method"].as_str().unwrap_or("GET").to_ascii_uppercase();
    let url = raw["url"].as_str().unwrap_or("").to_owned();
    let status = raw["status"].as_u64().unwrap_or(0);
    let status_text = raw["statusText"].as_str().unwrap_or("").to_owned();
    let ok = raw["ok"].as_bool().unwrap_or((200..300).contains(&status));
    let duration = raw["duration"].as_f64().unwrap_or(0.0).max(0.0).round() as u64;
    let initiator = raw["initiatorType"].as_str().unwrap_or("other").to_owned();
    let timestamp = raw["timestamp"]
        .as_str()
        .or_else(|| raw["at"].as_str())
        .unwrap_or("")
        .to_owned();
    let mut out = json!({
        "method": method,
        "url": url,
        "status": status,
        "statusText": status_text,
        "ok": ok,
        "duration": duration,
        "initiatorType": initiator,
        "timestamp": timestamp,
        "at": timestamp,
    });
    if let Some(error) = raw["error"].as_str().filter(|e| !e.is_empty()) {
        out["error"] = json!(error);
    }
    out
}

pub fn paginate_logs(rows: &[Value], offset: usize, limit: usize) -> Value {
    let limit = limit.max(1);
    let total = rows.len();
    let start = offset.min(total);
    let end = (start + limit).min(total);
    let next_offset = (end < total).then_some(end);
    json!({
        "total": total,
        "offset": offset,
        "next_offset": next_offset,
        "entries": &rows[start..end],
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(byte) = value.get(i + 1..i + 3).and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What a page's request for a new window opens.
#[derive(Debug, PartialEq)]
pub enum NewWindow {
    /// A web address: the system browser.
    External(String),
    /// A blank or blob document the page fills itself (downloads, print previews).
    Child,
    Deny,
}

pub fn new_window(url: &str) -> NewWindow {
    if url.is_empty() || url == "about:blank" || url.starts_with("blob:http://127.0.0.1:") {
        return NewWindow::Child;
    }
    match web_url(url) {
        Some(url) => NewWindow::External(url.to_string()),
        None => NewWindow::Deny,
    }
}

/// Where a main-window navigation may go.
#[derive(Debug, PartialEq)]
pub enum MainNavigation {
    Allow,
    /// Another site: the system browser, the app stays put.
    External(String),
    Deny,
}

/// The main window shows only the UI origin; blank documents and blob
/// downloads stay with WebKit.
pub fn main_navigation(url: &str, origin: &str) -> MainNavigation {
    if url.is_empty() || url == "about:blank" || url.starts_with("about:srcdoc") || url.starts_with("blob:") {
        return MainNavigation::Allow;
    }
    match web_url(url) {
        Some(parsed) if !origin.is_empty() && parsed.origin().ascii_serialization() == origin => MainNavigation::Allow,
        Some(parsed) => MainNavigation::External(parsed.to_string()),
        None => MainNavigation::Deny,
    }
}

/// AppKit modifier flags of browser-control modifier names.
pub fn modifier_flags(names: &[String]) -> Result<usize, BridgeError> {
    let mut flags = 0;
    for name in names {
        flags |= match name.to_ascii_lowercase().as_str() {
            "shift" => 1 << 17,
            "control" | "ctrl" => 1 << 18,
            "alt" | "option" => 1 << 19,
            "meta" | "cmd" | "command" => 1 << 20,
            _ => return Err(BridgeError::new("desktop.browser_key_invalid", format!("不支持的修饰键：{name}"))),
        };
    }
    Ok(flags)
}

/// The macOS virtual key code and characters of a DOM key name.
pub fn key_event(key: &str) -> Result<(u16, String), BridgeError> {
    let function = |code: u32| char::from_u32(code).expect("function key").to_string();
    let named = match key {
        "Enter" | "Return" => Some((36, "\r".to_owned())),
        "Tab" => Some((48, "\t".to_owned())),
        "Escape" | "Esc" => Some((53, "\u{1b}".to_owned())),
        "Backspace" => Some((51, "\u{7f}".to_owned())),
        "Delete" => Some((117, function(0xF728))),
        " " | "Space" => Some((49, " ".to_owned())),
        "ArrowUp" | "Up" => Some((126, function(0xF700))),
        "ArrowDown" | "Down" => Some((125, function(0xF701))),
        "ArrowLeft" | "Left" => Some((123, function(0xF702))),
        "ArrowRight" | "Right" => Some((124, function(0xF703))),
        "Home" => Some((115, function(0xF729))),
        "End" => Some((119, function(0xF72B))),
        "PageUp" => Some((116, function(0xF72C))),
        "PageDown" => Some((121, function(0xF72D))),
        _ => None,
    };
    if let Some(named) = named {
        return Ok(named);
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => Ok((0, c.to_string())),
        _ => Err(BridgeError::new("desktop.browser_key_invalid", format!("不支持的按键：{key}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_windows_split_web_addresses_from_page_documents() {
        assert_eq!(new_window("https://github.com/x"), NewWindow::External("https://github.com/x".into()));
        assert_eq!(new_window("http://127.0.0.1:3101/chat/a"), NewWindow::External("http://127.0.0.1:3101/chat/a".into()));
        assert_eq!(new_window("about:blank"), NewWindow::Child);
        assert_eq!(new_window(""), NewWindow::Child);
        assert_eq!(new_window("blob:http://127.0.0.1:3101/1f2e"), NewWindow::Child);
        for bad in ["blob:https://evil.example/1", "file:///etc/hosts", "javascript:alert(1)", "data:text/html,x"] {
            assert_eq!(new_window(bad), NewWindow::Deny, "{bad}");
        }
    }

    #[test]
    fn main_window_stays_on_the_ui_origin() {
        let origin = "http://127.0.0.1:3101";
        assert_eq!(main_navigation("http://127.0.0.1:3101/chat/a?x=1", origin), MainNavigation::Allow);
        assert_eq!(main_navigation("blob:http://127.0.0.1:3101/1f", origin), MainNavigation::Allow);
        assert_eq!(main_navigation("about:blank", origin), MainNavigation::Allow);
        assert_eq!(main_navigation("http://127.0.0.1:8100/api/health", origin), MainNavigation::External("http://127.0.0.1:8100/api/health".into()));
        assert_eq!(main_navigation("https://github.com/a", origin), MainNavigation::External("https://github.com/a".into()));
        for bad in ["file:///etc/hosts", "javascript:alert(1)", "muteki://x", "https://u:p@github.com/"] {
            assert_eq!(main_navigation(bad, origin), MainNavigation::Deny, "{bad}");
        }
        assert_eq!(main_navigation("http://127.0.0.1:3101/", ""), MainNavigation::External("http://127.0.0.1:3101/".into()));
    }

    #[test]
    fn keys_map_to_appkit_events() {
        assert_eq!(key_event("Enter").unwrap(), (36, "\r".into()));
        assert_eq!(key_event("ArrowLeft").unwrap().0, 123);
        assert_eq!(key_event("a").unwrap(), (0, "a".into()));
        assert_eq!(key_event("é").unwrap(), (0, "é".into()));
        for bad in ["", "F13", "ab", "\u{7}"] {
            assert_eq!(key_event(bad).unwrap_err().code, "desktop.browser_key_invalid", "{bad:?}");
        }
        assert_eq!(modifier_flags(&["shift".into(), "Meta".into()]).unwrap(), (1 << 17) | (1 << 20));
        assert_eq!(modifier_flags(&["hyper".into()]).unwrap_err().code, "desktop.browser_key_invalid");
    }

    #[test]
    fn route_decoding_keeps_partial_escapes() {
        assert_eq!(route_thread("/chat/ab%2").as_deref(), Some("ab%2"));
        assert_eq!(route_thread("/chat/ab%").as_deref(), Some("ab%"));
    }

    #[test]
    fn manifest_lists_every_capability_with_a_host() {
        let value = manifest();
        let entries = value["entries"].as_object().unwrap();
        for id in ["pathSelection", "workspaceFileActions", "preview", "attachmentCache", "terminal", "microphone", "notifications", "deepLinks"] {
            let entry = &entries[id];
            assert!(entry["supported"].is_boolean(), "{id}");
            assert!(["desktop-client", "service"].contains(&entry["host"].as_str().unwrap()), "{id}");
        }
        assert_eq!(entries["terminal"]["host"], "service");
        assert_eq!(entries["notifications"]["supported"], true);
    }

    #[test]
    fn relative_paths_cannot_escape() {
        assert!(relative_path("src/main.rs").is_ok());
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "./a", "a\\b", "C:/x", "a\0b"] {
            assert_eq!(relative_path(bad).unwrap_err().code, "desktop.workspace_file_invalid", "{bad:?}");
        }
    }

    #[test]
    fn grants_reject_symlinks_out_of_the_root() {
        let base = std::env::temp_dir().join(format!("muteki-grant-{}", std::process::id()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("src/a.txt"), "a").unwrap();
        std::fs::write(outside.join("secret"), "s").unwrap();
        let _ = std::fs::remove_file(root.join("link"));
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let root = root.canonicalize().unwrap();
        assert_eq!(resolve_in_root(&root, Some("src/a.txt")).unwrap(), root.join("src/a.txt"));
        assert_eq!(resolve_in_root(&root, None).unwrap(), root);
        assert_eq!(resolve_in_root(&root, Some("link/secret")).unwrap_err().code, "desktop.workspace_file_outside");
        assert_eq!(resolve_in_root(&root.join("src/.."), None).unwrap_err().code, "desktop.workspace_scope_changed");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn editors_probe_path_then_bundles() {
        let exists = |found: &'static str| move |path: &Path| path == Path::new(found);
        let cli = find_editor("auto", "/bin:/home/u/bin", exists("/home/u/bin/cursor")).unwrap().unwrap();
        assert_eq!((cli.name, cli.label), ("cursor", "Cursor"));
        let zed = find_editor("zed", "", exists("/Applications/Zed.app/Contents/MacOS/cli")).unwrap().unwrap();
        assert_eq!(zed.name, "zed");
        assert_eq!(find_editor("auto", "/bin", |_| false).unwrap(), None);
        assert_eq!(find_editor("system", "/bin", |_| true).unwrap(), None);
        assert_eq!(find_editor("code", "/bin", |_| false).unwrap_err().code, "desktop.editor_not_found");
        assert_eq!(find_editor("vim", "/bin", |_| true).unwrap_err().code, "desktop.editor_invalid");
        assert_eq!(editor_args("zed", Path::new("/r/a.rs"), Some(3)), ["/r/a.rs:3"]);
        assert_eq!(editor_args("code", Path::new("/r/a.rs"), Some(3)), ["--goto", "/r/a.rs:3"]);
        assert_eq!(editor_args("code", Path::new("/r"), None), ["/r"]);
    }

    #[test]
    fn external_urls_are_plain_web_addresses() {
        assert!(web_url("https://github.com/a/b/pull/1").is_some());
        assert!(web_url("http://127.0.0.1:5173/").is_some());
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "https://u:p@host/", "muteki://chat/x", "not a url"] {
            assert!(web_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn accelerators_convert_and_validate() {
        assert_eq!(accelerator_to_shortcut("CmdOrCtrl+Shift+N").as_deref(), Some("mod+shift+n"));
        assert_eq!(accelerator_to_shortcut("CmdOrCtrl+K").as_deref(), Some("mod+k"));
        assert_eq!(accelerator_to_shortcut("Alt+Left").as_deref(), Some("alt+left"));
        assert_eq!(accelerator_to_shortcut("Ctrl+F12").as_deref(), Some("ctrl+f12"));
        assert_eq!(accelerator_to_shortcut("CmdOrCtrl+/").as_deref(), Some("mod+/"));
        for bad in ["", "Cmd+K", "CmdOrCtrl+?", "CmdOrCtrl+k", "F13", "Shift+Shift+A", "CmdOrCtrl++"] {
            assert_eq!(accelerator_to_shortcut(bad), None, "{bad}");
        }
        for shortcut in DEFAULT_ACCELERATORS.iter().filter_map(|(_, a)| accelerator_to_shortcut(a)) {
            assert!(nsh_core::desktop::Shortcut::parse(&shortcut, true).is_ok(), "{shortcut}");
        }
        let current = default_accelerators();
        let next = merge_accelerators(&current, &json!({"search": "CmdOrCtrl+P", "sidebar": "", "new-chat": null, "other": 1})).unwrap();
        assert_eq!(next["search"], "CmdOrCtrl+P");
        assert_eq!(next["sidebar"], "");
        assert_eq!(next["new-chat"], "CmdOrCtrl+Shift+N");
        assert_eq!(merge_accelerators(&current, &json!({"search": "Cmd+?"})).unwrap_err().code, "desktop.accelerator_invalid");
        assert_eq!(merge_accelerators(&current, &json!({"search": 3})).unwrap_err().code, "desktop.accelerator_invalid");
    }

    #[test]
    fn terminal_relay_accepts_only_the_thread_stream() {
        let ok = "/api/threads/t%201/workspace/terminal/sessions/s1/stream?ticket=abc";
        assert!(terminal_path("t 1", ok).is_ok());
        for bad in [
            "/api/threads/t%201/workspace/terminal/sessions/s1/stream",
            "/api/threads/other/workspace/terminal/sessions/s1/stream?ticket=abc",
            "/api/threads/t%201/workspace/terminal/sessions//stream?ticket=abc",
            "/api/threads/t%201/workspace/terminal/sessions/s1/stream?ticket=abc&x=1",
            "//evil/api/threads/t%201/workspace/terminal/sessions/s1/stream?ticket=abc",
        ] {
            assert!(terminal_path("t 1", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn routes_name_their_thread() {
        assert_eq!(route_thread("/chat/abc%20d?x=1").as_deref(), Some("abc d"));
        assert_eq!(route_thread("/src?thread=thread-1").as_deref(), Some("thread-1"));
        assert_eq!(route_thread("/src?x=1&threadId=t%202").as_deref(), Some("t 2"));
        assert_eq!(route_thread("/src/tasks/task-3"), None);
        assert_eq!(route_thread("/chat"), None);
        assert_eq!(route_thread("/settings"), None);
        assert_eq!(
            collect_visible_threads("/src?thread=t1", &["t2".into(), "t1".into(), "t3".into()]),
            vec!["t1".to_owned(), "t2".to_owned(), "t3".to_owned()]
        );
    }

    #[test]
    fn manifest_exposes_preview_subcapabilities_microphone_and_deeplinks() {
        let value = manifest();
        assert_eq!(value["deepLink"]["supported"], true);
        assert_eq!(value["deepLink"]["schemes"], json!(["muteki"]));
        let entries = &value["entries"];
        assert_eq!(entries["microphone"]["supported"], true);
        assert_eq!(entries["deepLinks"]["supported"], true);
        assert_eq!(entries["deepLinks"]["schemes"], json!(["muteki"]));
        let preview = &entries["preview"];
        for sub in [
            "navigation",
            "screenshot",
            "consoleLogs",
            "networkLogs",
            "devtools",
            "clearWebsiteData",
            "clearHttpAuth",
        ] {
            assert_eq!(preview[sub], true, "{sub}");
            assert_eq!(preview["features"][sub], true, "features.{sub}");
            assert_eq!(preview["capabilities"][sub]["supported"], true, "capabilities.{sub}");
        }
    }

    #[test]
    fn deep_links_parse_and_validate_routes() {
        let chat = parse_deep_link("muteki://chat/thread%201?serviceId=svc-a&identityId=id-1").unwrap();
        assert_eq!(chat.route, "/chat/thread%201");
        assert_eq!(chat.thread_id.as_deref(), Some("thread 1"));
        assert_eq!(chat.service_id.as_deref(), Some("svc-a"));
        assert_eq!(chat.identity_id.as_deref(), Some("id-1"));

        let thread_alias = parse_deep_link("muteki://thread/abc-123").unwrap();
        assert_eq!(thread_alias.route, "/chat/abc-123");
        assert_eq!(thread_alias.thread_id.as_deref(), Some("abc-123"));

        let slash_chat = parse_deep_link("muteki:///chat/xyz").unwrap();
        assert_eq!(slash_chat.route, "/chat/xyz");

        let src = parse_deep_link("muteki://src?thread=w1&service=svc-b").unwrap();
        assert_eq!(src.route, "/src?thread=w1");
        assert_eq!(src.thread_id.as_deref(), Some("w1"));
        assert_eq!(src.service_id.as_deref(), Some("svc-b"));

        let settings = parse_deep_link("muteki://settings/notifications").unwrap();
        assert_eq!(settings.route, "/settings/notifications");
        assert_eq!(settings.thread_id, None);

        let open_route = parse_deep_link("muteki://open?path=/chat/t99&serviceId=svc-1").unwrap();
        assert_eq!(open_route.route, "/chat/t99");
        assert_eq!(open_route.thread_id.as_deref(), Some("t99"));
        assert_eq!(open_route.service_id.as_deref(), Some("svc-1"));

        for bad in [
            "",
            "https://example.com/chat/1",
            "muteki://user:pass@chat/1",
            "muteki://chat",
            "muteki://chat/",
            "muteki://chat/../etc",
            "muteki://chat/a/b",
            "muteki://unknown/route",
            "muteki://open?path=//evil.com",
            "muteki://open?path=/chat/../secret",
        ] {
            assert_eq!(parse_deep_link(bad).unwrap_err().code, "desktop.deep_link_invalid", "{bad}");
        }
    }

    #[test]
    fn scope_generation_and_envelope_validation() {
        let mut scope = ScopeState::new("http://127.0.0.1:8100");
        assert_eq!((scope.scope_version, scope.connection_version), (1, 1));

        assert!(scope.update("http://127.0.0.1:8100", "svc-1", "id-1"));
        assert_eq!((scope.scope_version, scope.connection_version), (1, 1));
        assert!(!scope.update("http://127.0.0.1:8100", "svc-1", "id-1"));

        assert!(scope.update("http://127.0.0.1:8100", "svc-1", "id-2"));
        assert_eq!((scope.scope_version, scope.connection_version), (2, 2));

        let env = CallEnvelope {
            id: json!(7),
            window_id: "main".into(),
            document_id: "doc-1".into(),
            scope_version: 2,
            connection_version: 2,
            method: "selectWorkspaceRoot".into(),
            scope_bound: is_scope_bound_method("selectWorkspaceRoot"),
        };
        assert!(env.validate_reply("doc-1", 2).is_ok());
        assert_eq!(env.validate_reply("doc-2", 2).unwrap_err().code, "desktop.document_stale");
        assert_eq!(env.validate_reply("doc-1", 3).unwrap_err().code, "desktop.scope_changed");
    }

    #[test]
    fn shortcut_sync_resets_defaults_and_localizes_labels() {
        let customized = merge_accelerators(&default_accelerators(), &json!({"search": "CmdOrCtrl+P", "sidebar": ""})).unwrap();
        assert_eq!(customized["search"], "CmdOrCtrl+P");
        assert_eq!(customized["sidebar"], "");

        let (reset, locale) = sync_accelerators(&json!({"accelerators": {"search": null}, "locale": "en-US"})).unwrap();
        assert_eq!(reset["search"], "CmdOrCtrl+K");
        assert_eq!(reset["sidebar"], "CmdOrCtrl+B");
        assert_eq!(reset["new-chat"], "CmdOrCtrl+Shift+N");
        assert_eq!(locale.as_deref(), Some("en-US"));

        let en = menu_labels("en-US");
        let zh = menu_labels("zh-CN");
        assert_eq!(en.new_chat, "New Chat");
        assert_eq!(zh.new_chat, "新建对话");
    }

    #[test]
    fn preview_logs_normalize_and_paginate() {
        let c1 = normalize_console_log(&json!({"level": "warn", "text": "careful", "url": "https://a.example/", "at": "2026-01-01T00:00:00Z", "line": 12}));
        assert_eq!(c1["level"], "warning");
        assert_eq!(c1["message"], "careful");
        assert_eq!(c1["text"], "careful");
        assert_eq!(c1["timestamp"], "2026-01-01T00:00:00Z");
        assert_eq!(c1["line"], 12);

        let n1 = normalize_network_log(&json!({
            "method": "post",
            "url": "https://a.example/api",
            "status": 201,
            "statusText": "Created",
            "duration": 42.6,
            "initiatorType": "fetch",
            "timestamp": "2026-01-01T00:00:01Z"
        }));
        assert_eq!(n1["method"], "POST");
        assert_eq!(n1["status"], 201);
        assert_eq!(n1["ok"], true);
        assert_eq!(n1["duration"], 43);
        assert_eq!(n1["initiatorType"], "fetch");

        let rows = vec![c1.clone(), n1.clone(), c1];
        let page1 = paginate_logs(&rows, 0, 2);
        assert_eq!(page1["total"], 3);
        assert_eq!(page1["next_offset"], json!(2));
        assert_eq!(page1["entries"].as_array().unwrap().len(), 2);

        let page2 = paginate_logs(&rows, 2, 2);
        assert_eq!(page2["next_offset"], Value::Null);
        assert_eq!(page2["entries"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn external_url_confirmation_policy() {
        let origin = "http://127.0.0.1:3101";
        let internal = web_url("http://127.0.0.1:3101/chat/1").unwrap();
        let external = web_url("https://example.com/docs").unwrap();
        assert!(!needs_external_url_confirmation(&internal, origin));
        assert!(needs_external_url_confirmation(&external, origin));
    }
}
