//! Copy a host CLI login into a stored credential account.
//!
//! Only the files an engine needs to authenticate are copied (never caches,
//! sessions or binaries), so a session started with the account sees a
//! login that no longer depends on the operator's home directory. A staged
//! directory replaces the account material only after the account row saved.

use crate::{
    AppState,
    agent_runtime::{descriptors, host_login},
    native_chat,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde_json::{Value, json};
use std::{
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

#[derive(Debug)]
pub(crate) struct ImportError {
    status: StatusCode,
    detail: String,
}

fn bad(detail: impl Into<String>) -> ImportError {
    ImportError { status: StatusCode::BAD_REQUEST, detail: detail.into() }
}

fn io_failure(context: &str, e: &std::io::Error) -> ImportError {
    ImportError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        detail: format!("{context}（{:?}）", e.kind()),
    }
}

pub(crate) fn reply(e: ImportError) -> (StatusCode, Json<Value>) {
    (e.status, Json(json!({"ok": false, "detail": e.detail})))
}

pub(crate) fn valid_account_id(id: &str) -> bool {
    id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Engines whose descriptor declares a copyable host login besides Codex.
fn login_engines() -> Vec<&'static str> {
    descriptors::all()
        .iter()
        .filter(|d| matches!(d.login.host_login_import.as_str(), "claude_settings_env" | "kimi_home_copy" | "grok_home_copy"))
        .map(|d| d.engine())
        .collect()
}

fn private_dir(path: &FsPath) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn copy_private(source: &FsPath, target: &FsPath) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        private_dir(parent)?;
    }
    std::fs::copy(source, target)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn accounts_root(state_dir: &FsPath) -> PathBuf {
    state_dir.join("imported-credentials/accounts")
}

/// A private directory beside the live account that becomes its material.
struct Staged {
    path: PathBuf,
}

impl Staged {
    fn new(state_dir: &FsPath, id: &str) -> Result<Self, ImportError> {
        let root = accounts_root(state_dir);
        private_dir(&root).map_err(|e| io_failure("无法创建凭据目录", &e))?;
        let path = root.join(format!(".stage-{id}-{}", uuid::Uuid::new_v4().simple()));
        private_dir(&path).map_err(|e| io_failure("无法创建凭据暂存目录", &e))?;
        Ok(Self { path })
    }

}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

fn login_row(id: &str, engine: &str, home: &str) -> Value {
    json!({
        "account_id": id, "label": id, "engine": engine, "worker_engine": engine,
        "connection": "official", "imported_home": home, "import_source": "host",
        "present": true, "updated_at": now(),
    })
}

/// What one import writes: the account row and the staged material.
struct Plan {
    row: Value,
    staged: Staged,
    suggested_model: Option<String>,
}

fn plan_codex(state_dir: &FsPath, id: &str, root: &FsPath) -> Result<Plan, ImportError> {
    let auth = root.join("auth.json");
    let content = match std::fs::read_to_string(&auth) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(bad(format!("未找到宿主 Codex 登录（{}），请先执行 codex login", auth.display())));
        }
        Err(e) => return Err(bad(format!("无法读取 {}（{:?}）", auth.display(), e.kind()))),
    };
    if !serde_json::from_str::<Value>(&content).is_ok_and(|v| v.is_object()) {
        return Err(bad(format!("{} 不是有效的 JSON 对象", auth.display())));
    }
    let staged = Staged::new(state_dir, id)?;
    let home = staged.path.join("codex-home");
    copy_private(&auth, &home.join("auth.json")).map_err(|e| io_failure("无法复制 Codex 登录", &e))?;
    let config = root.join("config.toml");
    if config.is_file() {
        copy_private(&config, &home.join("config.toml")).map_err(|e| io_failure("无法复制 Codex 配置", &e))?;
    }
    Ok(Plan { row: login_row(id, "codex", "codex-home"), staged, suggested_model: None })
}

fn plan_claude(state_dir: &FsPath, id: &str, root: &FsPath) -> Result<Plan, ImportError> {
    let path = root.join("settings.json");
    let settings: Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|_| bad(format!("无法读取宿主 Claude 配置：{} 不是有效 JSON", path.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(bad(format!(
                "未找到宿主 Claude 配置（{}）；官方账户请执行 claude setup-token 后粘贴 token",
                path.display()
            )));
        }
        Err(e) => return Err(bad(format!("无法读取宿主 Claude 配置（{:?}）", e.kind()))),
    };
    let env = &settings["env"];
    let field = |key: &str| env[key].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let secret = field("ANTHROPIC_AUTH_TOKEN").or_else(|| field("ANTHROPIC_API_KEY"));
    let (Some(secret), Some(base_url)) = (secret, field("ANTHROPIC_BASE_URL")) else {
        return Err(bad(
            "宿主 Claude 配置未同时包含 ANTHROPIC_AUTH_TOKEN/ANTHROPIC_API_KEY 与 ANTHROPIC_BASE_URL；官方账户请执行 claude setup-token 后粘贴 token",
        ));
    };
    let suggested = field("ANTHROPIC_MODEL").or_else(|| field("ANTHROPIC_DEFAULT_SONNET_MODEL"));
    let mut row = json!({
        "account_id": id, "label": id, "engine": "claude", "worker_engine": "claude",
        "target_engine": "claude", "connection": "custom_endpoint", "provider": "宿主 Claude 配置",
        "base_url": base_url, "secret": secret, "import_source": "host", "updated_at": now(),
    });
    if let Some(model) = &suggested {
        row["default_model"] = json!(model);
        row["target_model"] = json!(model);
        row["models"] = json!([model]);
    }
    Ok(Plan { row, staged: Staged::new(state_dir, id)?, suggested_model: suggested })
}

fn plan_home_copy(
    state_dir: &FsPath,
    id: &str,
    engine: &str,
    root: &FsPath,
) -> Result<Plan, ImportError> {
    let (home, required, files, hint): (&str, &str, &[&str], &str) = match engine {
        "kimi" => (
            "kimi-home",
            "credentials/kimi-code.json",
            &["config.toml", "device_id", "credentials/kimi-code.json", "oauth/kimi-code"],
            "请先运行 kimi 并完成 /login",
        ),
        _ => ("grok-home", "auth.json", &["auth.json", "config.toml"], "请先执行 grok login"),
    };
    let required_path = root.join(required);
    if !required_path.is_file() {
        return Err(bad(format!("未找到宿主 {engine} 登录（{}），{hint}", required_path.display())));
    }
    let staged = Staged::new(state_dir, id)?;
    for relative in files {
        let source = root.join(relative);
        if source.is_file() {
            copy_private(&source, &staged.path.join(home).join(relative))
                .map_err(|e| io_failure("无法复制宿主登录文件", &e))?;
        }
    }
    Ok(Plan { row: login_row(id, engine, home), staged, suggested_model: None })
}

/// Import `engine`'s login found under `root` into account `id`.
pub(crate) fn import(
    s: &AppState,
    state_dir: &FsPath,
    id: &str,
    engine: &str,
    root: &FsPath,
) -> Result<Value, ImportError> {
    if !valid_account_id(id) {
        return Err(bad("凭据 ID 需为 1-64 位字母、数字、_、.、-，且以字母或数字开头"));
    }
    let plan = match engine {
        "codex" => plan_codex(state_dir, id, root)?,
        "claude" => plan_claude(state_dir, id, root)?,
        "kimi" | "grok" => plan_home_copy(state_dir, id, engine, root)?,
        _ => return Err(bad(format!("宿主登录导入仅支持：codex, {}", login_engines().join(", ")))),
    };
    let saved = crate::account_material::save(s,id,plan.row,false,None,false,Some(&plan.staged.path))
        .map_err(|error|ImportError {status:StatusCode::INTERNAL_SERVER_ERROR,detail:format!("无法保存凭据（{error}）")})?
        .map_err(|()|ImportError {status:StatusCode::CONFLICT,detail:"该凭据 ID 已被另一个模型端点占用，请换一个 ID".into()})?;
    let mut account = crate::compat::public_account(saved);
    if let Some(model) = plan.suggested_model {
        account["suggested_model"] = json!(model);
    }
    Ok(account)
}

pub(crate) fn host_root(engine: &str) -> Result<PathBuf, ImportError> {
    if !host_login::discovery_enabled() {
        return Err(ImportError {
            status: StatusCode::CONFLICT,
            detail: "此服务未启用本机登录发现，无法导入宿主登录；请粘贴凭据".into(),
        });
    }
    host_login::login_root(engine).ok_or_else(|| bad("无法确定宿主登录目录（HOME 未设置）"))
}

async fn import_codex(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    let root = host_root("codex").map_err(reply)?;
    let state_dir = native_chat::state_dir(&s);
    let account = import(&s, &state_dir, &id, "codex", &root).map_err(reply)?;
    Ok(Json(json!({"ok": true, "account": account})))
}

async fn import_login(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult {
    let engine = body["engine"].as_str().unwrap_or("").trim().to_ascii_lowercase();
    let engines = login_engines();
    if !engines.contains(&engine.as_str()) {
        return Err(reply(bad(format!("engine 必须是以下之一：{}", engines.join(", ")))));
    }
    let root = host_root(&engine).map_err(reply)?;
    let state_dir = native_chat::state_dir(&s);
    let account = import(&s, &state_dir, &id, &engine, &root).map_err(reply)?;
    Ok(Json(json!({"ok": true, "account": account})))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings/credential-accounts/{id}/import-host-codex", post(import_codex))
        .route("/api/settings/credential-accounts/{id}/import-host-login", post(import_login))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runtime::credentials;

    fn state(dir: &FsPath) -> AppState {
        let store = crate::store::Store::open(dir.join("muteki.db")).unwrap();
        store.put("system", "state_dir", &json!(dir.join("state"))).unwrap();
        AppState::new(store)
    }

    fn write(path: &FsPath, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[tokio::test]
    async fn codex_import_replaces_an_api_key_account_with_the_host_login() {
        let dir = tempfile::tempdir().unwrap();
        let (state_dir, host) = (dir.path().join("state"), dir.path().join("host"));
        let s = state(dir.path());
        s.store
            .save_account("work", json!({"account_id": "work", "engine": "codex", "secret": "sk-old"}), false, None)
            .unwrap()
            .unwrap();
        write(&accounts_root(&state_dir).join("work/API_KEY"), "sk-old");
        write(&host.join("auth.json"), r#"{"tokens":{"account_id":"acct"}}"#);
        write(&host.join("config.toml"), "model = \"gpt-5\"\n");
        write(&host.join("sessions/x.jsonl"), "{}");

        let account = import(&s, &state_dir, "work", "codex", &host).unwrap();
        assert_eq!(account["present"], true);
        let row = s.store.get("credential_accounts", "work").unwrap().unwrap();
        let base = crate::account_material::base(&state_dir, "work", &row).unwrap();
        assert!(base.join("codex-home/auth.json").is_file());
        assert!(base.join("codex-home/config.toml").is_file());
        assert!(!base.join("codex-home/sessions").exists());
        assert!(!base.join("API_KEY").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(base.join("codex-home/auth.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let row = s.store.get("credential_accounts", "work").unwrap().unwrap();
        assert!(row.get("secret").is_none(), "an imported login must not keep the old API key");

        let descriptor = descriptors::find("codex").unwrap();
        let resolved = credentials::resolve(&s.store, &state_dir, descriptor, "account:work", None).await.unwrap();
        let home = resolved.env.iter().rev().find(|(k, _)| k == "CODEX_HOME").map(|(_, v)| v.clone()).unwrap();
        assert_eq!(PathBuf::from(home), base.join("codex-home").canonicalize().unwrap());
        assert!(!resolved.env.iter().any(|(k, v)| k == "OPENAI_API_KEY" && !v.is_empty()));
    }

    #[test]
    fn missing_or_invalid_host_logins_are_rejected_without_touching_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let (state_dir, host) = (dir.path().join("state"), dir.path().join("host"));
        let s = state(dir.path());
        assert!(import(&s, &state_dir, "a", "codex", &host).unwrap_err().detail.contains("codex login"));
        write(&host.join("auth.json"), "[]");
        assert_eq!(import(&s, &state_dir, "a", "codex", &host).unwrap_err().status, StatusCode::BAD_REQUEST);
        assert!(import(&s, &state_dir, "../a", "codex", &host).is_err());
        assert!(import(&s, &state_dir, "a", "grok", &dir.path().join("empty")).unwrap_err().detail.contains("grok login"));
        assert!(s.store.list("credential_accounts").unwrap().is_empty());
        let leftovers: Vec<_> = std::fs::read_dir(accounts_root(&state_dir))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "failed imports leave no staged material");
    }

    #[test]
    fn claude_gateway_settings_become_a_custom_endpoint_account() {
        let dir = tempfile::tempdir().unwrap();
        let (state_dir, host) = (dir.path().join("state"), dir.path().join("host"));
        let s = state(dir.path());
        write(&host.join("settings.json"), r#"{"env":{"ANTHROPIC_BASE_URL":"https://gw.example/v1"}}"#);
        assert!(import(&s, &state_dir, "gw", "claude", &host).unwrap_err().detail.contains("ANTHROPIC_BASE_URL"));
        write(
            &host.join("settings.json"),
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"tok","ANTHROPIC_BASE_URL":"https://gw.example/v1","ANTHROPIC_MODEL":"glm-5"}}"#,
        );
        let account = import(&s, &state_dir, "gw", "claude", &host).unwrap();
        assert_eq!(account["suggested_model"], "glm-5");
        assert_eq!(account["connection"], "custom_endpoint");
        assert!(account.get("secret").is_none());
        let row = s.store.get("credential_accounts", "gw").unwrap().unwrap();
        assert_eq!((row["secret"].as_str(), row["base_url"].as_str()), (Some("tok"), Some("https://gw.example/v1")));
    }

    #[test]
    fn kimi_import_copies_only_the_declared_login_files() {
        let dir = tempfile::tempdir().unwrap();
        let (state_dir, host) = (dir.path().join("state"), dir.path().join("host"));
        let s = state(dir.path());
        write(&host.join("credentials/kimi-code.json"), "{}");
        write(&host.join("device_id"), "d");
        write(&host.join("logs/big.log"), "x");
        import(&s, &state_dir, "k", "kimi", &host).unwrap();
        let row = s.store.get("credential_accounts", "k").unwrap().unwrap();
        let home = crate::account_material::base(&state_dir, "k", &row).unwrap().join("kimi-home");
        assert!(home.join("credentials/kimi-code.json").is_file());
        assert!(home.join("device_id").is_file());
        assert!(!home.join("logs").exists());
        assert_eq!(s.store.get("credential_accounts", "k").unwrap().unwrap()["imported_home"], "kimi-home");
    }
}
