//! Host CLI login detection.
//!
//! Detection reads the files and keychain items each engine's own CLI writes.
//! It never runs a model turn and never copies a credential: a `present`
//! result only means the engine will find a login when it starts, and
//! `unknown` means this build cannot tell, which callers treat as usable.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

pub const PRESENT: &str = "present";
pub const ABSENT: &str = "absent";
pub const UNKNOWN: &str = "unknown";
pub const DISABLED: &str = "disabled";

const CACHE_TTL: Duration = Duration::from_secs(30);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// `MUTEKI_HOST_DISCOVERY=0` turns host login discovery off for this service.
pub fn discovery_enabled() -> bool {
    std::env::var("MUTEKI_HOST_DISCOVERY").map_or(true, |value| value.trim() != "0")
}

fn cache() -> &'static Mutex<HashMap<String, (&'static str, Instant)>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<String, (&'static str, Instant)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn invalidate() {
    if let Ok(mut rows) = cache().lock() {
        rows.clear();
    }
}

fn var(name: &str) -> Option<String> {
    crate::shell_env::var(name)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn home() -> Option<PathBuf> {
    var("HOME").map(PathBuf::from)
}

/// `$VAR` when set (authoritative, no fallback), else `~/<relative>`.
fn root(variable: &str, relative: &str) -> Option<PathBuf> {
    match var(variable) {
        Some(value) => Some(PathBuf::from(value)),
        None => home().map(|home| home.join(relative)),
    }
}

fn exists(path: Option<PathBuf>, file: &str) -> &'static str {
    match path {
        Some(root) => match root.join(file).try_exists() {
            Ok(true) => PRESENT,
            Ok(false) => ABSENT,
            Err(_) => UNKNOWN,
        },
        None => UNKNOWN,
    }
}

async fn output(program: &str, args: &[&str]) -> Option<std::process::Output> {
    let mut cmd = crate::host_process::command(program);
    if let Ok(path_env) = super::binaries::launch_path(Path::new(program)) {
        cmd.env("PATH", path_env);
    }
    let child = cmd
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(COMMAND_TIMEOUT, child).await.ok()?.ok()
}

/// A macOS Keychain generic password, if one exists for `service`.
async fn keychain(service: &str) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = output("/usr/bin/security", &["find-generic-password", "-s", service, "-w"]).await?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Tokens stored by macOS CLIs must be explicitly projected for Linux Workers.
pub(crate) async fn container_tokens(engine: &str) -> Vec<(String, String)> {
    match engine {
        "cursor" if var("CURSOR_API_KEY").is_none() && var("CURSOR_AUTH_TOKEN").is_none() => {
            keychain("cursor-access-token").await.map(|token| vec![("CURSOR_AUTH_TOKEN".into(), token)]).unwrap_or_default()
        }
        "claude" if ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"]
            .iter().all(|key| var(key).is_none()) => {
            let token = keychain("Claude Code-credentials").await
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|value| value.get("claudeAiOauth").or_else(|| value.get("data")).unwrap_or(&value)
                    .get("accessToken").and_then(|token| token.as_str()).map(str::to_owned));
            token.filter(|token| !token.is_empty()).map(|token| vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), token)]).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn oauth_token(raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    let nested = value
        .get("claudeAiOauth")
        .or_else(|| value.get("data"))
        .and_then(|v| v.get("accessToken"));
    nested
        .or_else(|| value.get("accessToken"))
        .and_then(|v| v.as_str())
        .is_some_and(|token| !token.trim().is_empty())
}

async fn claude() -> &'static str {
    if ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"]
        .iter()
        .any(|name| var(name).is_some())
    {
        return PRESENT;
    }
    if let Some(secret) = keychain("Claude Code-credentials").await
        && oauth_token(&secret)
    {
        return PRESENT;
    }
    let Some(dir) = root("CLAUDE_CONFIG_DIR", ".claude") else {
        return UNKNOWN;
    };
    match std::fs::read_to_string(dir.join(".credentials.json")) {
        Ok(raw) if oauth_token(&raw) => PRESENT,
        Ok(_) => ABSENT,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ABSENT,
        Err(_) => UNKNOWN,
    }
}

async fn cursor() -> &'static str {
    if var("CURSOR_API_KEY").is_some() || var("CURSOR_AUTH_TOKEN").is_some() {
        return PRESENT;
    }
    let token = keychain("cursor-access-token").await;
    let user = home()
        .and_then(|home| std::fs::read_to_string(home.join(".cursor/cli-config.json")).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .is_some_and(|config| {
            !config["authInfo"]["userId"].is_null() && config["authInfo"]["userId"] != ""
        });
    if token.is_some() && user { PRESENT } else { ABSENT }
}

fn devin_binary() -> PathBuf {
    var("MUTEKI_DEVIN_BIN")
        .map(PathBuf::from)
        .or_else(|| agent::find_on_path("devin"))
        .or_else(|| home().map(|home| home.join(".local/bin/devin")))
        .unwrap_or_else(|| PathBuf::from("devin"))
}

async fn devin() -> &'static str {
    if var("WINDSURF_API_KEY").is_some() {
        return PRESENT;
    }
    if var("HOME").is_none() && var("XDG_DATA_HOME").is_none() {
        return UNKNOWN;
    }
    if let Some(dir) = root("XDG_DATA_HOME", ".local/share")
        && std::fs::metadata(dir.join("devin/credentials.toml")).is_ok_and(|m| m.is_file() && m.len() > 0)
    {
        return PRESENT;
    }
    let binary = devin_binary();
    let Some(out) = output(&binary.to_string_lossy(), &["auth", "status"]).await else {
        return UNKNOWN;
    };
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .to_ascii_lowercase();
    if text.contains("not logged in") || text.contains("not authenticated") {
        ABSENT
    } else if out.status.success() && text.contains("logged in") {
        PRESENT
    } else {
        UNKNOWN
    }
}

async fn agy() -> &'static str {
    let Some(descriptor) = super::descriptors::find("agy") else { return UNKNOWN };
    let Ok(binary) = super::binaries::resolve(descriptor, None).await else { return ABSENT };
    if let Some(home) = home()
        && std::fs::metadata(home.join(".gemini/jetski-standalone-oauth-token"))
            .is_ok_and(|m| m.is_file() && m.len() > 0)
    {
        return PRESENT;
    }
    if keychain("jetski-standalone-oauth-token").await.is_some() {
        return PRESENT;
    }
    let Ok(mut command) = crate::host_process::binary_command(&binary.path) else {
        return UNKNOWN;
    };
    command.arg("models").stdin(std::process::Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(20), command.output()).await {
        Ok(Ok(output)) if output.status.success() => PRESENT,
        Ok(Ok(output)) => {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .to_ascii_lowercase();
            if text.contains("login") || text.contains("unauthenticated") || text.contains("not logged in") {
                ABSENT
            } else {
                UNKNOWN
            }
        }
        _ => UNKNOWN,
    }
}

async fn uncached(engine: &str) -> &'static str {
    match engine {
        "codex" => {
            if var("OPENAI_API_KEY").is_some() {
                PRESENT
            } else {
                exists(root("CODEX_HOME", ".codex"), "auth.json")
            }
        }
        "claude" => claude().await,
        "cursor" => cursor().await,
        "grok" => {
            if var("XAI_API_KEY").is_some() {
                PRESENT
            } else {
                exists(root("GROK_HOME", ".grok"), "auth.json")
            }
        }
        "opencode" => {
            if var("OPENCODE_API_KEY").is_some() || var("OPENAI_API_KEY").is_some() {
                PRESENT
            } else {
                exists(root("XDG_DATA_HOME", ".local/share"), "opencode/auth.json")
            }
        }
        "pi" => match root("PI_CODING_AGENT_DIR", ".pi/agent") {
            Some(dir) if dir.join("auth.json").exists() || dir.join("models.json").exists() => PRESENT,
            Some(_) => ABSENT,
            None => UNKNOWN,
        },
        "kimi" => {
            if var("KIMI_MODEL_API_KEY").is_some() && var("KIMI_MODEL_NAME").is_some() {
                PRESENT
            } else {
                exists(root("KIMI_CODE_HOME", ".kimi-code"), "credentials/kimi-code.json")
            }
        }
        "omp" => match root("PI_CODING_AGENT_DIR", ".omp/agent") {
            Some(dir) if dir.is_dir() => PRESENT,
            Some(_) => ABSENT,
            None => UNKNOWN,
        },
        "agy" => agy().await,
        "devin" => devin().await,
        "droid" => {
            if var("FACTORY_API_KEY").is_some() {
                PRESENT
            } else {
                exists(home(), ".factory/auth.v2.loginkeychain")
            }
        }
        _ => UNKNOWN,
    }
}

/// `present`, `absent`, `unknown` or `disabled` for the host's own login.
pub async fn detect(engine: &str, fresh: bool) -> &'static str {
    if !discovery_enabled() {
        return DISABLED;
    }
    let engine = engine.trim().to_ascii_lowercase();
    if super::descriptors::find(&engine).is_none() {
        return UNKNOWN;
    }
    if !fresh
        && let Ok(rows) = cache().lock()
        && let Some((status, at)) = rows.get(&engine)
        && at.elapsed() < CACHE_TTL
    {
        return status;
    }
    let file_credential = super::descriptors::find(&engine).is_some_and(|descriptor| {
        descriptor.public()["credentials"]["env_keys"].as_array().into_iter().flatten()
            .filter_map(serde_json::Value::as_str).filter(|key| key.ends_with("_FILE"))
            .any(|key| var(key).and_then(|path| std::fs::read_to_string(path).ok())
                .is_some_and(|value| !value.trim().is_empty()))
    });
    let status = if file_credential { PRESENT } else { uncached(&engine).await };
    if let Ok(mut rows) = cache().lock() {
        rows.insert(engine, (status, Instant::now()));
    }
    status
}

/// The root an engine's host login lives in, for diagnostics only.
pub fn login_root(engine: &str) -> Option<PathBuf> {
    match engine {
        "codex" => root("CODEX_HOME", ".codex"),
        "claude" => root("CLAUDE_CONFIG_DIR", ".claude"),
        "grok" => root("GROK_HOME", ".grok"),
        "pi" => root("PI_CODING_AGENT_DIR", ".pi/agent"),
        "kimi" => root("KIMI_CODE_HOME", ".kimi-code"),
        "omp" => root("PI_CODING_AGENT_DIR", ".omp/agent"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_tokens_are_read_from_either_shape() {
        assert!(oauth_token(r#"{"claudeAiOauth":{"accessToken":"t"}}"#));
        assert!(oauth_token(r#"{"accessToken":"t"}"#));
        assert!(!oauth_token(r#"{"claudeAiOauth":{"accessToken":""}}"#));
        assert!(!oauth_token("not json"));
    }

    #[tokio::test]
    async fn unknown_engines_are_never_reported_present() {
        if discovery_enabled() {
            assert_eq!(detect("no-such-engine", true).await, UNKNOWN);
        }
    }
}
