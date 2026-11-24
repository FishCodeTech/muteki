//! Read-only release checks for installed engine CLIs.
//!
//! Only release metadata is read, or the vendor's own `update --check`
//! command run. Nothing here installs, upgrades or rewrites configuration, and
//! a failed check never changes an engine's health.

use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

/// Release metadata changes slowly; one lookup per install per five minutes.
const TTL: Duration = Duration::from_secs(300);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(12);

fn npm_package(engine: &str) -> Option<&'static str> {
    match engine {
        "claude" => Some("@anthropic-ai/claude-code"),
        "codex" => Some("@openai/codex"),
        "pi" => Some("@earendil-works/pi-coding-agent"),
        _ => None,
    }
}

fn version_pattern() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\d+(?:\.\d+){1,3}(?:(?:a|b|alpha|beta|rc|dev)\d+)?(?:-[0-9a-z][0-9a-z._-]*)?").unwrap()
    })
}

fn cursor_pattern() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)\b\d{4}\.\d{2}\.\d{2}-[0-9a-z]+\b").unwrap())
}

fn cursor_policy_pattern() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)^v?(\d{4})\.(\d{2})\.(\d{2})(?:-([0-9a-z]+))?$").unwrap())
}

fn parse_cursor_date(value: &str) -> Option<(u64, u64, u64)> {
    let caps = cursor_policy_pattern().captures(value.trim())?;
    let year: u64 = caps.get(1)?.as_str().parse().ok()?;
    let month: u64 = caps.get(2)?.as_str().parse().ok()?;
    let day: u64 = caps.get(3)?.as_str().parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// The comparable version inside raw `--version` output.
pub fn installed_version(engine: &str, raw: &str) -> String {
    let text = raw.trim();
    if engine == "cursor" {
        if let Some(m) = cursor_pattern().find(text) {
            return m.as_str().to_owned();
        }
        if parse_cursor_date(text).is_some() {
            return text.trim_start_matches(['v', 'V']).to_owned();
        }
        return String::new();
    }
    version_pattern().find(text).map(|m| m.as_str().to_owned()).unwrap_or_default()
}

type Parsed = (Vec<u64>, Option<(u8, u64)>);

fn parse(value: &str) -> Option<Parsed> {
    let digits = value.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(value.len());
    let (core, suffix) = value.split_at(digits);
    let core = core.trim_end_matches('.');
    let mut parts: Vec<u64> = core.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    if !(2..=4).contains(&parts.len()) {
        return None;
    }
    parts.resize(4, 0);
    let suffix = suffix.trim_start_matches(['-', '+', '.', '_']);
    if suffix.is_empty() {
        return Some((parts, Some((4, 0))));
    }
    let tag_end = suffix.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(suffix.len());
    let (tag, rest) = suffix.split_at(tag_end);
    let rank = match tag {
        "dev" => Some(0),
        "a" | "alpha" => Some(1),
        "b" | "beta" => Some(2),
        "rc" => Some(3),
        _ => None,
    };
    let rest = rest.trim_start_matches(['.', '-']);
    let number = if rest.is_empty() { Some(0) } else { rest.parse().ok() };
    Some((parts, rank.zip(number)))
}

/// -1/0/1, or `None` when the two versions cannot be compared reliably.
pub fn compare(installed: &str, latest: &str) -> Option<i8> {
    let left = installed.trim().to_ascii_lowercase();
    let right = latest.trim().to_ascii_lowercase();
    let (left, right) = (left.trim_start_matches('v'), right.trim_start_matches('v'));
    if left == right {
        return Some(0);
    }
    let (lc, lp) = parse(left)?;
    let (rc, rp) = parse(right)?;
    if lc != rc {
        return Some(if lc < rc { -1 } else { 1 });
    }
    let (lp, rp) = (lp?, rp?);
    Some(match lp.cmp(&rp) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

/// Engine-aware version comparison (`-1`/`0`/`1`), supporting `YYYY.MM.DD[-sha]`
/// release identifiers for `cursor` in addition to standard semver.
pub fn compare_for_engine(engine: &str, installed: &str, target: &str) -> Option<i8> {
    if let Some(ordering) = compare(installed, target) {
        return Some(ordering);
    }
    if engine == "cursor" {
        let left = parse_cursor_date(installed)?;
        let right = parse_cursor_date(target)?;
        return Some(match left.cmp(&right) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        });
    }
    None
}

#[derive(Debug)]
struct Failure {
    code: &'static str,
    message: String,
}

fn request_failed(error: impl std::fmt::Display) -> Failure {
    Failure { code: "version_check.request_failed", message: error.to_string() }
}

fn metadata_invalid(message: impl Into<String>) -> Failure {
    Failure { code: "version_check.metadata_invalid", message: message.into() }
}

async fn http_text(url: &str) -> Result<String, Failure> {
    let client = crate::network_proxy::http_client_builder()
        .timeout(Duration::from_secs(6))
        .connect_timeout(Duration::from_secs(4))
        .build()
        .map_err(request_failed)?;
    let response = client
        .get(url)
        .header("Accept", "application/json, text/plain;q=0.9, */*;q=0.8")
        .send()
        .await
        .map_err(request_failed)?
        .error_for_status()
        .map_err(request_failed)?;
    response.text().await.map_err(request_failed)
}

fn npm_url(package: &str, latest: bool) -> String {
    let encoded = package.replace('/', "%2F");
    if latest {
        format!("https://registry.npmjs.org/{encoded}/latest")
    } else {
        format!("https://registry.npmjs.org/{encoded}")
    }
}

async fn npm_latest(package: &str, major: Option<u64>) -> Result<String, Failure> {
    let payload: Value = serde_json::from_str(&http_text(&npm_url(package, true)).await?)
        .map_err(|e| metadata_invalid(e.to_string()))?;
    let latest = payload["version"].as_str().unwrap_or("").trim().to_owned();
    let Some(major) = major else {
        return Ok(latest);
    };
    if latest.split('.').next() == Some(major.to_string().as_str()) {
        return Ok(latest);
    }
    let metadata: Value = serde_json::from_str(&http_text(&npm_url(package, false)).await?)
        .map_err(|e| metadata_invalid(e.to_string()))?;
    metadata["versions"]
        .as_object()
        .into_iter()
        .flat_map(|versions| versions.keys())
        .filter_map(|value| {
            let parts: Vec<u64> = value.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
            (parts.len() == 3 && parts[0] == major).then(|| (parts, value.clone()))
        })
        .max()
        .map(|(_, value)| value)
        .ok_or_else(|| metadata_invalid(format!("No stable {package} release for installed major {major}")))
}

async fn check_only(binary: &Path, args: &[&str]) -> Result<String, Failure> {
    let mut cmd = crate::host_process::binary_command(binary)
        .map_err(|error| Failure { code: "version_check.command_failed", message: error.message })?;
    let child = cmd
        .args(args)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("CI", "1")
        .env("OMP_SKIP_SETUP", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(COMMAND_TIMEOUT, child)
        .await
        .map_err(|_| Failure { code: "version_check.command_timeout", message: format!("{} timed out", args.join(" ")) })?
        .map_err(|e| Failure { code: "version_check.command_failed", message: e.to_string() })?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !output.status.success() {
        return Err(Failure {
            code: "version_check.command_failed",
            message: format!("{} exited with {}: {}", args.join(" "), output.status, if stderr.is_empty() { &stdout } else { &stderr }),
        });
    }
    Ok(if stdout.is_empty() { stderr } else { stdout })
}

fn capture(pattern: &str, text: &str) -> Option<String> {
    regex::Regex::new(pattern).ok()?.captures(text).map(|c| c[1].to_owned())
}

async fn latest_for(engine: &str, binary: &Path, installed: &str) -> Result<(String, String), Failure> {
    if engine == "opencode" {
        let major = installed.split('.').next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(2).max(2);
        let package = "@opencode/cli";
        return Ok((npm_latest(package, Some(major)).await?, format!("npm:{package}")));
    }
    if let Some(package) = npm_package(engine) {
        return Ok((npm_latest(package, None).await?, format!("npm:{package}")));
    }
    match engine {
        "omp" => {
            let output = check_only(binary, &["update", "--check"]).await?;
            let found = capture(r"(?i)New version available:\s*v?(\S+)", &output)
                .or_else(|| capture(r"(?i)Current version:\s*v?(\S+)", &output))
                .unwrap_or_default();
            Ok((found, "omp:check".into()))
        }
        "grok" => {
            let output = check_only(binary, &["update", "--check", "--json"]).await?;
            let payload: Value = serde_json::from_str(&output).map_err(|e| metadata_invalid(e.to_string()))?;
            Ok((payload["latestVersion"].as_str().unwrap_or("").trim().to_owned(), "grok:check".into()))
        }
        "kimi" => Ok((http_text("https://code.kimi.ai/kimi-code/latest").await?.trim().to_owned(), "kimi:release".into())),
        "cursor" => {
            let script = http_text("https://cursor.com/install").await?;
            let found = capture(r#"(?i)https://downloads\.cursor\.com/lab/([^/"'\s]+)/"#, &script).unwrap_or_default();
            Ok((found, "cursor:installer-metadata".into()))
        }
        _ => Err(metadata_invalid(format!("没有 {engine} 的版本元数据来源"))),
    }
}

/// One read-only version check; every failure becomes `unknown`.
pub async fn check(engine: &str, binary: &Path, runtime_version: &str) -> Value {
    let installed = installed_version(engine, runtime_version);
    let mut result = json!({
        "installed_version": installed, "latest_version": "", "status": "unknown",
        "update_available": false, "checked_at": chrono::Utc::now().to_rfc3339(),
        "source": "", "detail": "最新版本未获取", "stale": false,
    });
    if installed.is_empty() {
        result["detail"] = json!("尚未取得已安装版本");
        return result;
    }
    if binary.as_os_str().is_empty() {
        result["detail"] = json!("尚未找到本机 CLI");
        return result;
    }
    match latest_for(engine, binary, &installed).await {
        Ok((raw, source)) => {
            let latest = installed_version(engine, &raw);
            if latest.is_empty() {
                result["error"] = json!("发布元数据中没有可识别的版本号");
                result["error_code"] = json!("version_check.metadata_invalid");
                return result;
            }
            result["latest_version"] = json!(latest);
            result["source"] = json!(source);
            match compare(&installed, &latest) {
                None => result["detail"] = json!(format!("已获取最新版本 {latest}，版本格式暂无法比较")),
                Some(-1) => {
                    result["status"] = json!("update_available");
                    result["update_available"] = json!(true);
                    result["detail"] = json!(format!("可更新至 {latest}"));
                }
                Some(_) => {
                    result["status"] = json!("current");
                    result["detail"] = json!("已是最新版本");
                }
            }
        }
        Err(failure) => {
            result["error"] = json!(failure.message);
            result["error_code"] = json!(failure.code);
        }
    }
    result
}

pub fn lookups_disabled(store: &crate::store::Store) -> Result<bool, String> {
    let ui = store.get("settings", "ui").map_err(|error| error.to_string())?;
    Ok(ui.is_some_and(|doc| doc["values"]["providerUpdateChecksDisabled"] == true))
}

/// The check result while lookups are off: the installed version is still read, nothing is requested.
pub fn disabled(engine: &str, runtime_version: &str) -> Value {
    json!({
        "installed_version": installed_version(engine, runtime_version), "latest_version": "", "status": "disabled",
        "update_available": false, "checked_at": chrono::Utc::now().to_rfc3339(),
        "source": "", "detail": "已在设置中关闭更新检查", "stale": false,
    })
}

type Key = (String, String, String);

fn flights() -> &'static Mutex<HashMap<Key, Arc<tokio::sync::OnceCell<(Instant, Value)>>>> {
    static CELLS: OnceLock<Mutex<HashMap<Key, Arc<tokio::sync::OnceCell<(Instant, Value)>>>>> = OnceLock::new();
    CELLS.get_or_init(Default::default)
}

/// [`check`] with a per-install TTL and single flight, keyed by engine,
/// binary and installed version so an upgrade is checked again at once.
pub async fn cached(engine: &str, binary: &Path, runtime_version: &str, force: bool) -> Value {
    let key = (engine.to_owned(), binary.to_string_lossy().into_owned(), runtime_version.to_owned());
    let cell = {
        let mut cells = flights().lock().unwrap();
        let fresh = cells
            .get(&key)
            .and_then(|cell| cell.get())
            .is_some_and(|(at, _)| at.elapsed() < TTL);
        if force || !fresh && cells.get(&key).is_some_and(|cell| cell.initialized()) {
            cells.remove(&key);
        }
        cells.entry(key).or_default().clone()
    };
    cell.get_or_init(|| async { (Instant::now(), check(engine, binary, runtime_version).await) })
        .await
        .1
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_versions_are_extracted_from_raw_output() {
        assert_eq!(installed_version("codex", "codex-cli 0.46.0"), "0.46.0");
        assert_eq!(installed_version("claude", "2.0.14 (Claude Code)"), "2.0.14");
        assert_eq!(installed_version("cursor", "2025.09.18-7ae6800"), "2025.09.18-7ae6800");
        assert_eq!(installed_version("pi", "no version"), "");
    }

    #[test]
    fn versions_compare_with_prereleases_and_refuse_unknown_suffixes() {
        assert_eq!(compare("1.2.3", "1.2.4"), Some(-1));
        assert_eq!(compare("v1.2.3", "1.2.3"), Some(0));
        assert_eq!(compare("1.3", "1.2.9"), Some(1));
        assert_eq!(compare("1.2.3-rc1", "1.2.3"), Some(-1));
        assert_eq!(compare("1.2.3-nightly", "1.2.3"), None);
    }

    #[tokio::test]
    async fn a_missing_install_is_reported_without_a_network_lookup() {
        let result = check("codex", Path::new("/bin/codex"), "").await;
        assert_eq!(result["status"], "unknown");
        assert_eq!(result["detail"], "尚未取得已安装版本");
    }
}
