//! Platform-neutral HTTP. The browser and AppKit use NSH's own fetch service.
use nsh_core::prelude::*;
use serde_json::Value;

pub fn url(path: &str) -> String {
    if let Some(base) = option_env!("MUTEKI_API_URL") {
        return format!("{}{path}", base.trim_end_matches('/'));
    }
    #[cfg(target_arch = "wasm32")]
    {
        return path.to_owned();
    }
    #[cfg(not(target_arch = "wasm32"))]
    { format!("http://127.0.0.1:8100{path}") }
}
pub async fn request(method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let mut req = HttpRequest::new(method, url(path));
    if let Some(body) = body {
        req = req
            .header("Content-Type", "application/json")
            .body(body.to_string());
    }
    let response = fetch(req).await?;
    let data: Value =
        serde_json::from_str(&response.text()).map_err(|_| "服务器返回了无效的 JSON".to_owned())?;
    if !response.ok() {
        return Err(data["error"]["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("请求失败 (HTTP {})", response.status())));
    }
    Ok(data)
}
pub fn command_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    format!(
        "rust-ui-{}-{}",
        nsh_core::time::unix_ms(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}
pub fn command(kind: &str, payload: Value) -> Value {
    serde_json::json!({"command_id":command_id(),"command_type":kind,"payload":payload})
}
