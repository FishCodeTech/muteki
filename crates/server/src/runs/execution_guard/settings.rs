use crate::{AppState, chat, control};
use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Config {
    pub enabled: bool,
    pub provider: String,
    pub endpoint: String,
    pub api_key: String,
    pub timeout_ms: u64,
    pub fail_policy: String,
    pub read_proceed_min: f64,
    pub scope_escalate_min: f64,
    pub reject_deny_min: f64,
    pub last_probe: Option<Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "cronus".into(),
            endpoint: String::new(),
            api_key: String::new(),
            timeout_ms: 5000,
            fail_policy: "fail_closed_deny".into(),
            read_proceed_min: 0.90,
            scope_escalate_min: 0.50,
            reject_deny_min: 0.90,
            last_probe: None,
        }
    }
}

fn invalid(message: impl Into<String>) -> chat::Failure {
    control::failure(
        StatusCode::UNPROCESSABLE_ENTITY,
        "guard.settings.invalid",
        message,
    )
}

fn location(state: &AppState) -> Result<(PathBuf, PathBuf), chat::Failure> {
    let root = crate::host_process::configured_state_dir(state)
        .ok_or_else(|| control::storage("Missing host state directory"))?;
    let path = root.join("execution-guard").join("settings.json");
    Ok((root, path))
}

pub(super) fn load(state: &AppState) -> Result<Config, chat::Failure> {
    let (root, path) = location(state)?;
    if !path.try_exists().map_err(control::storage)? {
        return Ok(Config::default());
    }
    let bytes =
        crate::private_files::read(&root, &path, 2 * 1024 * 1024).map_err(control::storage)?;
    serde_json::from_slice(&bytes).map_err(control::storage)
}

fn save(state: &AppState, config: &Config) -> Result<(), chat::Failure> {
    let (root, path) = location(state)?;
    crate::private_files::write(
        &root,
        &path,
        &serde_json::to_vec(config).map_err(control::storage)?,
    )
    .map_err(control::storage)
}

fn view(config: &Config) -> Value {
    json!({"ok":true, "settings":{
        "enabled":config.enabled, "provider":config.provider, "endpoint":config.endpoint,
        "api_key_configured":!config.api_key.is_empty(),
        "api_key_masked":if config.api_key.is_empty() { "" } else { "••••••••" },
        "timeout_ms":config.timeout_ms, "fail_policy":config.fail_policy,
        "read_proceed_min":config.read_proceed_min, "scope_escalate_min":config.scope_escalate_min,
        "reject_deny_min":config.reject_deny_min, "last_probe":config.last_probe
    }})
}

fn merge(current: &Config, patch: &Value) -> Result<Config, chat::Failure> {
    let fields = patch
        .as_object()
        .ok_or_else(|| invalid("Expected a settings object"))?;
    let mut next = serde_json::to_value(current).map_err(control::storage)?;
    for (key, value) in fields {
        match key.as_str() {
            "clear_api_key" if value.is_boolean() => continue,
            "api_key" if value.as_str() == Some("") => continue,
            "enabled" | "provider" | "endpoint" | "api_key" | "timeout_ms" | "fail_policy"
            | "read_proceed_min" | "scope_escalate_min" | "reject_deny_min" => {
                next[key] = value.clone()
            }
            _ => return Err(invalid(format!("Invalid guard setting {key}"))),
        }
    }
    if patch["clear_api_key"] == true {
        next["api_key"] = json!("");
    }
    let mut next: Config =
        serde_json::from_value(next).map_err(|_| invalid("Invalid guard field type"))?;
    next.endpoint = next.endpoint.trim().trim_end_matches('/').to_owned();
    if next.enabled || !next.endpoint.is_empty() {
        let url =
            reqwest::Url::parse(&next.endpoint).map_err(|_| invalid("Invalid endpoint URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid(
                "Expected an HTTP(S) endpoint without credentials, query or fragment",
            ));
        }
    }
    if next.provider != "cronus"
        || !matches!(
            next.fail_policy.as_str(),
            "fail_closed_ask" | "fail_closed_deny"
        )
    {
        return Err(invalid("Invalid guard provider or failure policy"));
    }
    if next.timeout_ms == 0
        || [
            next.read_proceed_min,
            next.scope_escalate_min,
            next.reject_deny_min,
        ]
        .iter()
        .any(|p| !(0.0..=1.0).contains(p))
    {
        return Err(invalid(
            "Expected a positive timeout and thresholds between 0 and 1",
        ));
    }
    if &next != current {
        next.last_probe = None;
    }
    Ok(next)
}

pub(crate) async fn get(State(state): State<Arc<AppState>>) -> chat::Api {
    Ok(Json(view(&load(&state)?)))
}

pub(crate) async fn put(State(state): State<Arc<AppState>>, Json(patch): Json<Value>) -> chat::Api {
    state
        .store
        .document_scope(|| {
            let config = merge(&load(&state)?, &patch)?;
            save(&state, &config)?;
            Ok(Json(view(&config)))
        })
        .map_err(control::storage)?
}

pub(crate) async fn probe(
    State(state): State<Arc<AppState>>,
    patch: Option<Json<Value>>,
) -> chat::Api {
    let saved = load(&state)?;
    let patch = patch.map(|p| p.0).unwrap_or_else(|| json!({}));
    let mut config = merge(&saved, &patch)?;
    if config.endpoint != saved.endpoint
        && !patch["api_key"].as_str().is_some_and(|key| !key.is_empty())
    {
        config.api_key.clear();
    }
    let result = super::cronus::probe(&config).await;
    if config != saved {
        return Ok(Json(result));
    }
    state
        .store
        .document_scope(|| {
            let mut current = load(&state)?;
            let mut probed = config;
            probed.last_probe = current.last_probe.clone();
            if probed != current {
                return Err(control::failure(
                    StatusCode::CONFLICT,
                    "guard.settings.changed",
                    "Guard settings changed during the probe",
                ));
            }
            current.last_probe = Some(result.clone());
            save(&state, &current)?;
            Ok(Json(result))
        })
        .map_err(control::storage)?
}
