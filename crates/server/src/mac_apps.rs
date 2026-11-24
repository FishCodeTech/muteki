//! Native macOS apps a composer chip can name as the computer-use target.
//!
//! macOS never hands Dock drags to other applications, so the inventory mirrors
//! the Dock itself (Finder, pinned apps, then other running apps) and the
//! composer offers it as its own draggable strip.

use crate::{AppState, capabilities};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

pub const REF_PREFIX: &str = "mcp:muteki-computer:app:";
const SCRIPT: &str = include_str!("mac_apps.js");
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
pub struct MacApp {
    pub bundle_id: String,
    pub name: String,
    pub path: String,
}

type Failure = (StatusCode, Json<Value>);

/// Apps LaunchServices reported in the latest listing, so a send can re-check
/// a chip without starting osascript while the thread document is held.
fn known() -> &'static Mutex<HashMap<String, MacApp>> {
    static KNOWN: OnceLock<Mutex<HashMap<String, MacApp>>> = OnceLock::new();
    KNOWN.get_or_init(Default::default)
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/conversation/mac-apps", get(list))
}

fn run_script(args: &[&str]) -> Result<Value, String> {
    let mut child = crate::host_process::std_command("osascript")
        .args(["-l", "JavaScript", "-e", SCRIPT])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动 osascript：{error}"))?;
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err("osascript 未提供输出管道".into());
    };
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut message = String::new();
        let _ = stderr.read_to_string(&mut message);
        message
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < SCRIPT_TIMEOUT => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("读取本机应用超过 {} 秒没有完成", SCRIPT_TIMEOUT.as_secs()));
            }
            Err(error) => return Err(format!("等待 osascript 失败：{error}")),
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "读取 osascript 输出的线程异常退出".to_owned())?
        .map_err(|error| format!("读取 osascript 输出失败：{error}"))?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("osascript 退出（{status}）：{}", stderr.trim()));
    }
    serde_json::from_slice(&stdout).map_err(|error| format!("本机应用清单无法解析：{error}"))
}

fn app_of(value: &Value) -> Option<MacApp> {
    let field = |key: &str| value[key].as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned);
    Some(MacApp { bundle_id: field("bundle_id")?, name: field("name")?, path: field("path")? })
}

fn remember(app: &MacApp) {
    known()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(app.bundle_id.clone(), app.clone());
}

/// The capability row a target-app chip carries.
pub fn row(app: &MacApp) -> Value {
    json!({
        "id": format!("{REF_PREFIX}{}", app.bundle_id),
        "kind": "mcp",
        "name": app.name,
        "description": format!("用电脑操控指定应用 · {}", app.bundle_id),
        "source": "Muteki 内置能力",
        "scope": "turn",
        "target": {"type": "mac_app", "bundle_id": app.bundle_id, "name": app.name, "path": app.path},
    })
}

pub fn instruction(app: &MacApp) -> String {
    format!(
        "[用户显式指定电脑操控的目标应用：{name}]\n\
        bundle id：{bundle}\n\
        应用路径：{path}\n\
        本轮请先调用 `chat_computer_status` 检查服务就绪与当前会话占用状态，再通过 `chat_computer_js` 执行 \
        `let app = await cua.getApp(\"{bundle}\");` 选中这个应用，并在其中完成用户请求。\
        用户已指定这个应用；除非用户在消息中另有要求，不要操作其他应用。",
        name = app.name,
        bundle = app.bundle_id,
        path = app.path,
    )
}

fn valid_bundle_id(bundle_id: &str) -> bool {
    !bundle_id.is_empty()
        && bundle_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Re-check a chip's app against this Mac before a turn is admitted.
pub fn resolve(bundle_id: &str) -> Result<MacApp, String> {
    if !valid_bundle_id(bundle_id) {
        return Err("目标应用的 bundle id 无效，请重新选择".into());
    }
    if !cfg!(target_os = "macos") {
        return Err("Muteki 服务不在 macOS 上运行，无法操控本机应用".into());
    }
    let cached = known().lock().unwrap_or_else(|error| error.into_inner()).get(bundle_id).cloned();
    if let Some(app) = cached
        && Path::new(&app.path).join("Contents/Info.plist").is_file()
    {
        return Ok(app);
    }
    let value = run_script(&["resolve", bundle_id])?;
    let app = app_of(&value["app"]).ok_or_else(|| format!("本机未找到应用 {bundle_id}，请重新选择目标应用"))?;
    remember(&app);
    Ok(app)
}

async fn list(State(s): State<Arc<AppState>>) -> Result<Json<Value>, Failure> {
    let unavailable = |code: &str, reason: &str| Json(json!({"available": false, "code": code, "reason": reason, "apps": []}));
    if !cfg!(target_os = "macos") {
        return Ok(unavailable(
            "conversation.mac_apps.unsupported_platform",
            "Muteki 服务不在 macOS 上运行，无法指定本机应用",
        ));
    }
    if !capabilities::computer_enabled(&s) {
        return Ok(unavailable(
            "conversation.mac_apps.computer_disabled",
            "本机电脑操控能力已在设置中关闭，请在「设置 → 内置能力」中启用",
        ));
    }
    let failed = |message: String| {
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"code": "conversation.mac_apps.inventory_failed", "category": "runtime",
                "message": message, "retryable": true}})),
        )
    };
    let listed = tokio::task::spawn_blocking(|| run_script(&["list"]))
        .await
        .map_err(|_| failed("读取本机应用的任务异常退出".into()))?
        .map_err(failed)?;
    let apps: Vec<Value> = listed["apps"]
        .as_array()
        .ok_or_else(|| failed("本机应用清单缺少 apps 列表".into()))?
        .iter()
        .filter_map(|raw| {
            let app = app_of(raw)?;
            remember(&app);
            let mut item = row(&app);
            for key in ["icon", "in_dock", "running"] {
                item[key] = raw[key].clone();
            }
            Some(item)
        })
        .collect();
    Ok(Json(json!({"available": true, "apps": apps})))
}
