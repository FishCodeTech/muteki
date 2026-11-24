//! Upstream proxy pool configuration, credential indirection, and circuit breaker state.

use super::credential_store;
use crate::AppState;
use crate::runs::src_api::{Api, Error, digest, now, rec, records, require, save_rec, task};
use axum::http::StatusCode;
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{OnceLock, atomic::{AtomicUsize, Ordering}};

const DEFAULT_FAILURE_THRESHOLD: u64 = 5;
const DEFAULT_COOLDOWN_SECS: i64 = 600;

#[derive(Clone, Debug)]
pub(in crate::runs::src_api::domains) struct ResolvedUpstream {
    pub node_id: String,
    pub label: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl ResolvedUpstream {
    pub fn endpoint_label(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }
}

fn rr_cursor(task_id: &str) -> usize {
    static CURSORS: OnceLock<std::sync::Mutex<HashMap<String, usize>>> = OnceLock::new();
    let map = CURSORS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let Ok(mut guard) = map.lock() else {
        static FALLBACK: AtomicUsize = AtomicUsize::new(0);
        return FALLBACK.fetch_add(1, Ordering::Relaxed);
    };
    let entry = guard.entry(task_id.to_owned()).or_insert(0);
    let current = *entry;
    *entry = current.wrapping_add(1);
    current
}

fn validate_scheme(raw: &str) -> Api<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "http" => Ok("http"),
        "https" => Ok("https"),
        "socks5" => Ok("socks5"),
        "socks5h" => Ok("socks5h"),
        _ => Err(Error::invalid(
            "upstream proxy scheme must be one of http, https, socks5, socks5h",
        )),
    }
}

fn validate_host(raw: &str) -> Api<String> {
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('@')
        || trimmed.chars().any(char::is_whitespace)
    {
        return Err(Error::invalid("upstream proxy host is invalid"));
    }
    Ok(trimmed.to_ascii_lowercase())
}

fn config_record(s: &AppState, id: &str) -> Api<Value> {
    match rec(s, id, "upstream_proxypool", "default") {
        Ok(row) => Ok(row["payload"].clone()),
        Err(error) if error.status == StatusCode::NOT_FOUND => Ok(json!({
            "enabled": false,
            "selection": "failover",
            "failure_threshold": DEFAULT_FAILURE_THRESHOLD,
            "cooldown_secs": DEFAULT_COOLDOWN_SECS,
            "fallback_direct": false,
            "nodes": [],
        })),
        Err(error) => Err(error),
    }
}

fn state_map(s: &AppState, id: &str) -> Api<BTreeMap<String, Value>> {
    let mut map = BTreeMap::new();
    for row in records(s, id, "upstream_proxypool_state")? {
        if let Some(node_id) = row["payload"]["node_id"].as_str() {
            map.insert(node_id.to_owned(), row["payload"].clone());
        }
    }
    Ok(map)
}

fn circuit_open_until(state: Option<&Value>) -> Option<chrono::DateTime<Utc>> {
    let st = state?;
    if st["circuit_state"] != "open" {
        return None;
    }
    let raw = st["cooldown_until"].as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn is_node_healthy(state: Option<&Value>, now_ts: chrono::DateTime<Utc>) -> bool {
    match circuit_open_until(state) {
        Some(until) => until <= now_ts,
        None => true,
    }
}

pub(super) fn save(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let existing_doc = rec(s, id, "upstream_proxypool", "default").ok();
    let existing_rev = existing_doc
        .as_ref()
        .and_then(|r| r["revision"].as_u64());
    let existing_creds: std::collections::HashMap<String, String> = existing_doc
        .as_ref()
        .and_then(|r| r["payload"]["nodes"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|node| {
            let nid = node["node_id"].as_str()?.trim();
            let cref = node["credential_ref"].as_str()?.trim();
            if nid.is_empty() || cref.is_empty() {
                None
            } else {
                Some((nid.to_owned(), cref.to_owned()))
            }
        })
        .collect();
    let enabled = body["enabled"].as_bool().unwrap_or(true);
    let selection = match body["selection"].as_str().unwrap_or("failover") {
        "failover" => "failover",
        "round_robin" => "round_robin",
        _ => {
            return Err(Error::invalid(
                "selection must be 'failover' or 'round_robin'",
            ))
        }
    };
    let failure_threshold = body["failure_threshold"]
        .as_u64()
        .unwrap_or(DEFAULT_FAILURE_THRESHOLD);
    if !(1..=50).contains(&failure_threshold) {
        return Err(Error::invalid("failure_threshold must be between 1 and 50"));
    }
    let cooldown_secs = body["cooldown_secs"]
        .as_i64()
        .unwrap_or(DEFAULT_COOLDOWN_SECS);
    if !(10..=86400).contains(&cooldown_secs) {
        return Err(Error::invalid("cooldown_secs must be between 10 and 86400"));
    }
    let fallback_direct = body["fallback_direct"].as_bool().unwrap_or(false);
    let raw_nodes = body["nodes"]
        .as_array()
        .ok_or_else(|| Error::invalid("nodes array is required"))?;
    if raw_nodes.len() > 64 {
        return Err(Error::invalid("maximum 64 upstream proxy nodes supported"));
    }

    let mut nodes = Vec::with_capacity(raw_nodes.len());
    for (idx, item) in raw_nodes.iter().enumerate() {
        let scheme = validate_scheme(item["scheme"].as_str().unwrap_or("http"))?;
        let host = validate_host(require(item, "host")?)?;
        let port = item["port"]
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .filter(|p| *p > 0)
            .ok_or_else(|| Error::invalid("upstream proxy port must be 1..=65535"))?;
        let username = item["username"].as_str().unwrap_or("").trim().to_owned();
        let node_id = item["node_id"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "up_{}",
                    &digest(&json!([scheme, host, port, username, idx]))[..16]
                )
            });
        let label = item["label"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("{scheme}://{host}:{port}"));

        let clear_cred = item["clear_credential"].as_bool() == Some(true);
        let mut credential_ref = item["credential_ref"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                if clear_cred {
                    String::new()
                } else {
                    existing_creds.get(&node_id).cloned().unwrap_or_default()
                }
            });

        if let Some(password) = item["password"].as_str().filter(|v| !v.is_empty()) {
            let alias = format!("proxypool_{}", digest(&json!(node_id))[..16].to_owned());
            let saved = credential_store::put(
                s,
                id,
                &json!({
                    "alias": alias,
                    "value": password,
                    "kind": "upstream_proxy_password",
                    "label": format!("Proxy pool {label}"),
                }),
            )?;
            credential_ref = saved["reference"]
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| credential_store::managed_reference(id, &alias).unwrap_or_default());
        }

        if !credential_ref.is_empty() {
            let _ = credential_store::version(s, id, &credential_ref)?;
        }

        nodes.push(json!({
            "node_id": node_id,
            "label": label,
            "scheme": scheme,
            "host": host,
            "port": port,
            "username": username,
            "credential_ref": if credential_ref.is_empty() { Value::Null } else { json!(credential_ref) },
            "enabled": item["enabled"].as_bool().unwrap_or(true),
            "priority": item["priority"].as_i64().unwrap_or((idx as i64) + 1),
        }));
    }

    let doc = json!({
        "enabled": enabled,
        "selection": selection,
        "failure_threshold": failure_threshold,
        "cooldown_secs": cooldown_secs,
        "fallback_direct": fallback_direct,
        "nodes": nodes,
        "updated_at": now(),
    });
    save_rec(s, id, "upstream_proxypool", "default", doc, existing_rev)?;
    status(s, id)
}

pub(super) fn status(s: &AppState, id: &str) -> Api<Value> {
    let _ = task(s, id)?;
    let cfg = config_record(s, id)?;
    let states = state_map(s, id)?;
    let now_ts = Utc::now();
    let mut out_nodes = Vec::new();
    let mut healthy_count = 0usize;

    for node in cfg["nodes"].as_array().into_iter().flatten() {
        let node_id = node["node_id"].as_str().unwrap_or("");
        let st = states.get(node_id);
        let enabled = node["enabled"].as_bool().unwrap_or(true);
        let open_until = circuit_open_until(st);
        let circuit_state = match open_until {
            Some(until) if until > now_ts => "open",
            Some(_) => "half_open",
            None => st
                .and_then(|v| v["circuit_state"].as_str())
                .unwrap_or("closed"),
        };
        if enabled && circuit_state != "open" {
            healthy_count += 1;
        }
        let cooldown_remaining_secs = open_until
            .map(|until| (until - now_ts).num_seconds().max(0))
            .unwrap_or(0);
        out_nodes.push(json!({
            "node_id": node_id,
            "label": node["label"],
            "scheme": node["scheme"],
            "host": node["host"],
            "port": node["port"],
            "username": node["username"],
            "has_credential": node["credential_ref"].as_str().is_some_and(|v| !v.is_empty()),
            "enabled": enabled,
            "priority": node["priority"],
            "circuit_state": circuit_state,
            "consecutive_failures": st.and_then(|v| v["consecutive_failures"].as_u64()).unwrap_or(0),
            "cooldown_until": st.map(|v| v["cooldown_until"].clone()).unwrap_or(Value::Null),
            "cooldown_remaining_secs": cooldown_remaining_secs,
            "last_error": st.map(|v| v["last_error"].clone()).unwrap_or(Value::Null),
            "last_used_at": st.map(|v| v["last_used_at"].clone()).unwrap_or(Value::Null),
            "success_count": st.and_then(|v| v["success_count"].as_u64()).unwrap_or(0),
            "failure_count": st.and_then(|v| v["failure_count"].as_u64()).unwrap_or(0),
        }));
    }

    Ok(json!({
        "enabled": cfg["enabled"],
        "selection": cfg["selection"],
        "failure_threshold": cfg["failure_threshold"],
        "cooldown_secs": cfg["cooldown_secs"],
        "fallback_direct": cfg["fallback_direct"],
        "healthy_count": healthy_count,
        "total_count": out_nodes.len(),
        "nodes": out_nodes,
        "updated_at": cfg["updated_at"],
    }))
}

pub(super) fn reset_circuit(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let target = body["node_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let rows = records(s, id, "upstream_proxypool_state")?;
    let mut reset_ids = Vec::new();
    for row in rows {
        let node_id = row["payload"]["node_id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .or_else(|| row["natural_key"].as_str().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| row["record_id"].as_str().unwrap_or(""));
        if target.is_some_and(|wanted| wanted != node_id) {
            continue;
        }
        let mut payload = row["payload"].clone();
        payload["consecutive_failures"] = json!(0);
        payload["circuit_state"] = json!("closed");
        payload["cooldown_until"] = Value::Null;
        payload["last_error"] = Value::Null;
        save_rec(
            s,
            id,
            "upstream_proxypool_state",
            node_id,
            payload,
            row["revision"].as_u64(),
        )?;
        reset_ids.push(json!(node_id));
    }
    Ok(json!({
        "reset_node_ids": reset_ids,
        "status": status(s, id)?,
    }))
}

pub(super) fn select_upstream(s: &AppState, id: &str) -> Api<Option<ResolvedUpstream>> {
    let cfg = config_record(s, id)?;
    if cfg["enabled"] != true {
        return Ok(None);
    }
    let fallback_direct = cfg["fallback_direct"].as_bool().unwrap_or(false);
    let states = state_map(s, id)?;
    let now_ts = Utc::now();

    let mut candidates = cfg["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| n["enabled"] != false)
        .filter(|n| {
            let nid = n["node_id"].as_str().unwrap_or("");
            is_node_healthy(states.get(nid), now_ts)
        })
        .cloned()
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        if fallback_direct {
            return Ok(None);
        }
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "proxypool_exhausted",
            "All configured upstream proxy nodes have open circuit breakers or are disabled",
        ));
    }

    let chosen = if cfg["selection"] == "round_robin" {
        let idx = rr_cursor(id) % candidates.len();
        candidates.swap_remove(idx)
    } else {
        candidates.sort_by_key(|n| n["priority"].as_i64().unwrap_or(100));
        candidates.swap_remove(0)
    };

    let node_id = require(&chosen, "node_id")?.to_owned();
    let cred_ref = chosen["credential_ref"].as_str().unwrap_or("");
    let password = if cred_ref.is_empty() {
        String::new()
    } else {
        credential_store::resolve(s, id, cred_ref, None)?
    };

    Ok(Some(ResolvedUpstream {
        node_id,
        label: chosen["label"].as_str().unwrap_or("").to_owned(),
        scheme: chosen["scheme"].as_str().unwrap_or("http").to_owned(),
        host: require(&chosen, "host")?.to_owned(),
        port: chosen["port"]
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .ok_or_else(|| Error::invalid("upstream proxy port invalid"))?,
        username: chosen["username"].as_str().unwrap_or("").to_owned(),
        password,
    }))
}

pub(super) fn record_outcome(
    s: &AppState,
    id: &str,
    node_id: &str,
    ok: bool,
    err_msg: Option<&str>,
) -> Api<()> {
    let cfg = config_record(s, id)?;
    let threshold = cfg["failure_threshold"]
        .as_u64()
        .unwrap_or(DEFAULT_FAILURE_THRESHOLD);
    let cooldown_secs = cfg["cooldown_secs"]
        .as_i64()
        .unwrap_or(DEFAULT_COOLDOWN_SECS);
    let existing = rec(s, id, "upstream_proxypool_state", node_id).ok();
    let rev = existing.as_ref().and_then(|r| r["revision"].as_u64());
    let mut state = existing
        .map(|r| r["payload"].clone())
        .unwrap_or_else(|| {
            json!({
                "node_id": node_id,
                "consecutive_failures": 0,
                "circuit_state": "closed",
                "cooldown_until": Value::Null,
                "last_error": Value::Null,
                "last_used_at": Value::Null,
                "success_count": 0,
                "failure_count": 0,
            })
        });

    state["last_used_at"] = json!(now());
    if ok {
        state["consecutive_failures"] = json!(0);
        state["circuit_state"] = json!("closed");
        state["cooldown_until"] = Value::Null;
        state["last_error"] = Value::Null;
        state["success_count"] = json!(state["success_count"].as_u64().unwrap_or(0) + 1);
    } else {
        let fails = state["consecutive_failures"].as_u64().unwrap_or(0) + 1;
        state["consecutive_failures"] = json!(fails);
        state["failure_count"] = json!(state["failure_count"].as_u64().unwrap_or(0) + 1);
        state["last_error"] = json!(err_msg.unwrap_or("upstream_connect_failed"));
        if fails >= threshold {
            let until = (Utc::now() + ChronoDuration::seconds(cooldown_secs)).to_rfc3339();
            state["circuit_state"] = json!("open");
            state["cooldown_until"] = json!(until);
        }
    }
    save_rec(s, id, "upstream_proxypool_state", node_id, state, rev)?;
    Ok(())
}

/// Container gateway policy projection (no credential secrets logged; passed only into private `policy.json`).
pub(super) fn policy_projection(s: &AppState, id: &str) -> Api<Value> {
    let cfg = config_record(s, id)?;
    if cfg["enabled"] != true {
        return Ok(json!({
            "enabled": false,
            "selection": cfg["selection"],
            "failure_threshold": cfg["failure_threshold"],
            "cooldown_secs": cfg["cooldown_secs"],
            "fallback_direct": cfg["fallback_direct"],
            "nodes": [],
        }));
    }
    let states = state_map(s, id)?;
    let now_ts = Utc::now();
    let mut nodes = Vec::new();
    for node in cfg["nodes"].as_array().into_iter().flatten() {
        if node["enabled"] == false {
            continue;
        }
        let nid = node["node_id"].as_str().unwrap_or("");
        if !is_node_healthy(states.get(nid), now_ts) {
            continue;
        }
        let reference = node["credential_ref"].as_str().unwrap_or("");
        let password = if reference.is_empty() { String::new() } else { credential_store::resolve(s, id, reference, None)? };
        nodes.push(json!({
            "node_id": nid,
            "scheme": node["scheme"],
            "host": node["host"],
            "port": node["port"],
            "username": node["username"],
            "password": password,
            "priority": node["priority"],
        }));
    }
    Ok(json!({
        "enabled": true,
        "selection": cfg["selection"],
        "failure_threshold": cfg["failure_threshold"],
        "cooldown_secs": cfg["cooldown_secs"],
        "fallback_direct": cfg["fallback_direct"],
        "nodes": nodes,
    }))
}
