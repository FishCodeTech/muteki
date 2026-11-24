//! Public catalog pricing and explicit local price configuration.
//!
//! Refresh/cache semantics carried forward from the legacy Python provider usage store.
//! Its upstream pricing attribution is retained here:
//! MIT License, Copyright (c) 2026 T3 Tools Inc.
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.
use crate::store::Store;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const RATE_URL: &str =
    "https://raw.githubusercontent.com/jiayuqi7813/model-prices/main/prices.json";
const LIVE_COLLECTION: &str = "pricing_live_cache";
const LIVE_KEY: &str = "public_model_prices";
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}
fn generated_timestamp(value: &Value) -> Option<f64> {
    if let Some(value) = number(value) {
        return Some(if value > 1e12 { value / 1000.0 } else { value });
    }
    let value = value.as_str()?;
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|date| date.timestamp_millis() as f64 / 1000.0)
        .ok()
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|date| date.and_utc().timestamp_millis() as f64 / 1000.0)
        })
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .map(|date| date.and_utc().timestamp_millis() as f64 / 1000.0)
        })
}
fn validate_feed(document: &Value) -> Result<(), String> {
    if document["_meta"]["schema_version"].as_u64() != Some(1)
        || generated_timestamp(&document["_meta"]["generated_at"]).is_none()
    {
        return Err("Invalid public pricing feed metadata".into());
    }
    if parse_rates(document).is_empty() {
        return Err("Public pricing feed contains no valid prices".into());
    }
    Ok(())
}
fn selected_snapshot(
    store: &Store,
    live: &Value,
) -> rusqlite::Result<(Value, BTreeMap<String, Value>)> {
    if validate_feed(&live["document"]).is_ok() {
        return Ok((live.clone(), parse_rates(&live["document"])));
    }
    let imported = store
        .get("import_metadata", "_usage_model_rates.json")?
        .unwrap_or(Value::Null);
    let parsed = parse_rates(&imported["document"]);
    Ok((imported, parsed))
}
fn describe_snapshot(saved: &Value, live: &Value, available: bool, at: f64) -> Value {
    let generated = &saved["document"]["_meta"]["generated_at"];
    let stale = generated_timestamp(generated).is_some_and(|time| at - time > 48.0 * 3600.0);
    let imported =
        available && (saved["source"] != RATE_URL || saved.get("last_attempt_at").is_none());
    let error = live["last_error"].as_str();
    let status = if !available {
        "unavailable"
    } else if imported || stale || error.is_some() {
        "cached"
    } else {
        "fresh"
    };
    let message = if let Some(error) = error {
        format!(
            "价格表更新失败（{error}）；{}",
            if available {
                "使用已缓存价格。"
            } else {
                "未知模型保持未定价。"
            }
        )
    } else if stale {
        format!(
            "价格表生成于 {}，已超过 48 小时未更新；继续使用上次有效价格。",
            generated
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| generated.to_string())
        )
    } else if available {
        "每日同步公开目录，仅用于费用估算；历史记录保留已捕获费用。".into()
    } else {
        "尚无可用公开价格；未知模型保持未定价。".into()
    };
    json!({"status":status,
        "source":if imported { saved["source"].as_str().unwrap_or("imported_saved_pricing") } else { RATE_URL },
        "fetched_at":saved["fetched_at"].as_f64(),"generated_at":generated,
        "last_attempt_at":live["last_attempt_at"],"last_error":live["last_error"],
        "refresh_supported":true,"read_only":true,"historical":imported,
        "message":message})
}
fn should_refresh(snapshot: &Value, live: &Value, refresh: bool, at: f64) -> bool {
    let max_age = if refresh { 60.0 } else { 86400.0 };
    if number(&live["last_attempt_at"]).is_some_and(|last| at - last < 60.0) {
        return false;
    }
    !(snapshot["source"] == RATE_URL
        && !parse_rates(&snapshot["document"]).is_empty()
        && number(&snapshot["fetched_at"]).is_some_and(|last| at - last < max_age))
}
async fn fetch_feed() -> Result<Value, String> {
    let client = crate::network_proxy::http_client_builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Could not initialize public pricing connection")?;
    let mut response = client.get(RATE_URL).send().await.map_err(|error| {
        if error.is_timeout() {
            "Public pricing request timed out"
        } else {
            "Could not connect to public pricing feed"
        }
        .to_string()
    })?;
    if !response.status().is_success() {
        return Err(format!(
            "Public pricing feed returned HTTP {}",
            response.status().as_u16()
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Could not read public pricing feed")?
    {
        bytes.extend_from_slice(&chunk);
    }
    let document: Value =
        serde_json::from_slice(&bytes).map_err(|_| "Public pricing feed returned invalid JSON")?;
    validate_feed(&document)?;
    Ok(document)
}
fn publish_attempt(
    store: &Store,
    mut previous: Value,
    result: Result<Value, String>,
    at: f64,
) -> rusqlite::Result<()> {
    if !previous.is_object() {
        previous = json!({});
    }
    match result {
        Ok(document) => {
            previous = json!({"source":RATE_URL,"document":document,"fetched_at":at,"last_attempt_at":at,"last_error":null});
        }
        Err(message) => {
            previous["last_attempt_at"] = json!(at);
            previous["last_error"] = json!(message);
        }
    }
    store.put(LIVE_COLLECTION, LIVE_KEY, &previous)
}
/// Synchronize the fixed public feed before catalog/resolve reads. `refresh`
/// reduces cache lifetime to 60 seconds; every failed attempt also backs off 60s.
/// Readers keep their previous complete snapshot while an ordinary refresh runs.
/// On an initial load, concurrent callers join the refresh instead of quoting a
/// temporary empty catalog. Transport/feed failures become metadata, storage
/// failures are returned to the caller.
pub async fn ensure(store: &Store, refresh: bool) -> rusqlite::Result<()> {
    let _guard = match REFRESH_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(_) if !refresh && !rates(store)?.0.is_empty() => return Ok(()),
        Err(_) => REFRESH_LOCK.lock().await,
    };
    let live = store.get(LIVE_COLLECTION, LIVE_KEY)?.unwrap_or(Value::Null);
    let (snapshot, _) = selected_snapshot(store, &live)?;
    let at = now();
    if should_refresh(&snapshot, &live, refresh, at) {
        let result = fetch_feed().await;
        publish_attempt(store, live, result, at)?;
    }
    Ok(())
}

const FIELDS: [(&str, &str); 4] = [
    ("input", "input_cost_per_token"),
    ("output", "output_cost_per_token"),
    ("cache_read", "cache_read_input_token_cost"),
    ("cache_write", "cache_creation_input_token_cost"),
];
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("").trim()
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}
fn engine(raw: &str) -> String {
    let name = raw.trim().to_lowercase();
    let name = name
        .split(':')
        .next()
        .unwrap_or("")
        .strip_prefix("cli.")
        .unwrap_or(name.split(':').next().unwrap_or(""));
    match name.split('.').next().unwrap_or("") {
        "claude-code" | "claude_code" => "claude",
        "open-code" | "open_code" => "opencode",
        "cursor-agent" => "cursor",
        "codex-cli" => "codex",
        other => other,
    }
    .to_string()
}
fn identity(raw: &Value) -> Result<Value, String> {
    if !raw.is_object() || text(raw, "engine").is_empty() || text(raw, "model").is_empty() {
        return Err("engine and model are required".into());
    }
    let mut item =
        json!({"engine":engine(text(raw,"engine")),"model":text(raw,"model").to_lowercase()});
    if raw.get("speed").is_some_and(|v| !v.is_null()) {
        if text(raw, "speed").is_empty() {
            return Err("speed must be a nonempty identifier".into());
        }
        let speed = text(raw, "speed").to_lowercase();
        if speed != "standard" {
            item["speed"] = json!(speed);
        }
    }
    for field in ["credential_id", "provider_id"] {
        if raw.get(field).is_some_and(|v| !v.is_null()) {
            let id = text(raw, field);
            if id.is_empty() || id.contains("://") || id.len() > 256 || id.contains(['\r', '\n']) {
                return Err(format!("{field} must be a local identifier"));
            }
            item[field] = json!(id);
        }
    }
    Ok(item)
}
/// Keep only the public pricing contract; never project arbitrary imported keys.
pub fn config_entry(kind: &str, raw: &Value) -> Result<Value, String> {
    let mut item = identity(raw)?;
    if kind == "override" {
        if !raw["rates"].is_object() {
            return Err("Invalid pricing rates".into());
        }
        if raw["rates"]
            .as_object()
            .unwrap()
            .keys()
            .any(|key| !FIELDS.iter().any(|(field, _)| key == field))
        {
            return Err("rates accepts only input, output, cache_read and cache_write".into());
        }
        let mut rates = json!({});
        for (field, _) in FIELDS {
            let value = &raw["rates"][field];
            if !value.is_null() && number(value).is_none() {
                return Err(format!(
                    "{field} must be a finite nonnegative number or null"
                ));
            }
            rates[field] = number(value).map_or(Value::Null, |n| json!(n));
        }
        if rates.as_object().unwrap().values().all(Value::is_null) {
            return Err("No valid saved pricing rates".into());
        }
        item["rates"] = rates;
    } else if kind == "reference" {
        if text(raw, "reference_model").is_empty() {
            return Err("reference_model is required".into());
        }
        item["reference_model"] = json!(text(raw, "reference_model").to_lowercase());
    } else {
        return Err("Unknown pricing entry kind".into());
    }
    Ok(item)
}
#[derive(Debug)]
pub enum ConfigError {
    Invalid(String),
    Storage,
}
fn config_key(item: &Value) -> String {
    let ordered: BTreeMap<_, _> = item
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key, value))
        .collect();
    let encoded = serde_json::to_string(&ordered).unwrap();
    let mut ascii = String::new();
    for ch in encoded.chars() {
        if ch.is_ascii() && ch != '\u{007f}' {
            ascii.push(ch);
        } else {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                ascii.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    format!("{:x}", Sha256::digest(ascii.as_bytes()))
}
pub fn write_config(
    store: &Store,
    kind: &str,
    raw: &Value,
    delete: bool,
) -> Result<Value, ConfigError> {
    if !["override", "reference"].contains(&kind) {
        return Err(ConfigError::Invalid("Unknown pricing entry kind".into()));
    }
    let identity = identity(raw).map_err(ConfigError::Invalid)?;
    let key = config_key(&identity);
    let collection = format!("pricing_{kind}");
    if delete {
        let deleted = store
            .delete(&collection, &key)
            .map_err(|_| ConfigError::Storage)?;
        Ok(json!({"ok":true,"deleted":deleted}))
    } else {
        let entry = config_entry(kind, raw).map_err(ConfigError::Invalid)?;
        store
            .put(&collection, &key, &entry)
            .map_err(|_| ConfigError::Storage)?;
        Ok(entry)
    }
}

fn tier(raw: &Value, suffix: &str) -> Option<Value> {
    for key in ["input_cost_per_token", "output_cost_per_token"] {
        let scaled = number(&raw[format!("{key}{suffix}")])? * 1e6;
        if !scaled.is_finite() {
            return None;
        }
    }
    let mut rates = json!({});
    for (field, key) in FIELDS {
        rates[field] = number(&raw[format!("{key}{suffix}")])
            .and_then(|n| {
                let n = n * 1e6;
                n.is_finite().then_some(n)
            })
            .map_or(Value::Null, |n| json!(n));
    }
    Some(rates)
}
fn parse_rates(document: &Value) -> BTreeMap<String, Value> {
    let mut rates = BTreeMap::new();
    if let Some(doc) = document.as_object() {
        for (model, raw) in doc {
            if let Some(standard) = tier(raw, "") {
                let mut row = json!({"standard":standard});
                if let Some(factor) =
                    number(&raw["provider_specific_entry"]["fast"]).filter(|n| *n > 0.0)
                {
                    let mut fast = row["standard"].clone();
                    for value in fast.as_object_mut().unwrap().values_mut() {
                        *value = number(value)
                            .and_then(|n| {
                                let n = n * factor;
                                n.is_finite().then_some(n)
                            })
                            .map_or(Value::Null, |n| json!(n));
                    }
                    row["fast"] = fast;
                } else if let Some(fast) = tier(raw, "_priority") {
                    row["fast"] = fast;
                }
                if let Some(fast) = tier(raw, "_ultrafast") {
                    row["ultrafast"] = fast;
                }
                let key = model.trim().to_lowercase();
                if key.starts_with("xai/grok-") || key.starts_with("grok/grok-") {
                    if let Some(long) = tier(raw, "_above_200k_tokens") {
                        row["long_context"] = long;
                    }
                }
                if key.starts_with("kimi-code-plan-cn/")
                    || key.starts_with("kimi-code-plan-global/")
                {
                    row = json!({"standard":{"input":null,"output":null,"cache_read":null,"cache_write":null},"pricing_status":"subscription_unknown"});
                }
                if key.starts_with("cursor/") {
                    let mut metadata = json!({});
                    for field in [
                        "fast_model_id",
                        "variant",
                        "long_context_threshold_input_tokens",
                        "long_context_model_id",
                        "short_context_model_id",
                    ] {
                        metadata[field] = raw["_cursor_pricing"][field].clone();
                    }
                    row["cursor_pricing"] = metadata;
                }
                if !key.is_empty() && key != "_meta" {
                    rates.insert(key, row);
                }
            }
        }
    }
    rates
}
fn rates(store: &Store) -> rusqlite::Result<(BTreeMap<String, Value>, Value)> {
    let live = store.get(LIVE_COLLECTION, LIVE_KEY)?.unwrap_or(Value::Null);
    let (saved, parsed) = selected_snapshot(store, &live)?;
    let meta = describe_snapshot(&saved, &live, !parsed.is_empty(), now());
    Ok((parsed, meta))
}

fn entries(store: &Store, kind: &str) -> rusqlite::Result<Vec<Value>> {
    Ok(store
        .list(&format!("pricing_{kind}"))?
        .into_iter()
        .filter_map(|v| config_entry(kind, &v).ok())
        .collect())
}
pub fn catalog(store: &Store, search: &str, selected_engine: &str) -> rusqlite::Result<Value> {
    let (rates, meta) = rates(store)?;
    let query = search.to_lowercase();
    let catalog: Vec<_> = rates
        .iter()
        .filter(|(key, _)| key.contains(&query))
        .map(|(key, row)| {
            let tiers: Vec<_> = ["standard", "fast", "ultrafast"]
                .into_iter()
                .filter(|tier| row.get(*tier).is_some())
                .collect();
            json!({"model":key,"status":row["pricing_status"].as_str().unwrap_or("catalog"),
                "rates":row["standard"],"tiers":tiers})
        })
        .collect();
    let select = |kind| -> rusqlite::Result<Vec<Value>> {
        Ok(entries(store, kind)?
            .into_iter()
            .filter(|row| {
                (selected_engine.is_empty() || text(row, "engine") == selected_engine)
                    && text(row, "model").contains(&query)
            })
            .collect())
    };
    Ok(
        json!({"catalog":catalog,"overrides":select("override")?,"references":select("reference")?,"meta":meta}),
    )
}
fn matches<'a>(rows: &'a [Value], item: &Value) -> Result<Option<&'a Value>, ()> {
    let rows: Vec<_> = rows
        .iter()
        .filter(|row| {
            row["engine"] == item["engine"]
                && row["model"] == item["model"]
                && row["speed"].as_str().unwrap_or("standard")
                    == item["speed"].as_str().unwrap_or("standard")
                && ["credential_id", "provider_id"]
                    .iter()
                    .all(|field| row.get(*field).is_none() || row[*field] == item[*field])
        })
        .collect();
    let specificity = |row: &Value| {
        ["credential_id", "provider_id"]
            .iter()
            .filter(|field| row.get(**field).is_some())
            .count()
    };
    let best = rows.iter().map(|v| specificity(v)).max();
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|v| Some(specificity(v)) == best)
        .collect();
    if rows.len() > 1 {
        Err(())
    } else {
        Ok(rows.first().copied())
    }
}
pub fn resolve(store: &Store, items: &[Value]) -> Result<Value, String> {
    let (rates, _) = rates(store).map_err(|_| "Could not read pricing metadata")?;
    let overrides = entries(store, "override").map_err(|_| "Could not read pricing overrides")?;
    let references =
        entries(store, "reference").map_err(|_| "Could not read pricing references")?;
    let mut results = Vec::new();
    for submitted in items {
        let mut routed = submitted.clone();
        if engine(text(submitted, "engine")) == "pi" {
            if let Some(account) =
                crate::provider::matching_account(store, text(submitted, "credential_id"))
                    .map_err(|_| "Could not read pricing route metadata")?
            {
                if text(submitted, "provider_id").is_empty()
                    || text(submitted, "provider_id") == text(&account, "provider")
                {
                    if reqwest::Url::parse(text(&account, "base_url"))
                        .ok()
                        .is_some_and(|url| {
                            url.scheme() == "https"
                                && url.host_str() == Some("ollama.com")
                                && url.port_or_known_default() == Some(443)
                        })
                    {
                        routed["catalog_provider"] = json!("ollama-cloud");
                        routed["route_provenance"] =
                            json!({"source":"current_selected_account_endpoint"});
                    }
                }
            }
        }
        let raw = &routed;
        let item = identity(raw)?;
        let speed = text(&item, "speed");
        let speed = if speed.is_empty() { "standard" } else { speed };
        let related = overrides.iter().chain(references.iter()).filter(|row| {
            row["engine"] == item["engine"]
                && row["model"] == item["model"]
                && row["speed"].as_str().unwrap_or("standard") == speed
        });
        let mut missing = BTreeSet::new();
        for row in related {
            for field in ["credential_id", "provider_id"] {
                if row.get(field).is_some() && item.get(field).is_none() {
                    missing.insert(field);
                }
            }
        }
        let mut result = item.clone();
        result["status"] = json!("missing");
        result["rates"] = Value::Null;
        result["candidates"] = json!([]);
        result["missing_scope"] = json!(missing);
        if raw.get("speed").is_some_and(|v| !v.is_null()) {
            result["speed"] = json!(speed);
        }
        match matches(&overrides, &item) {
            Err(()) => {
                result["status"] = json!("ambiguous");
                results.push(result);
                continue;
            }
            Ok(Some(custom)) => {
                result["status"] = json!("custom");
                result["rates"] = custom["rates"].clone();
                results.push(result);
                continue;
            }
            _ => {}
        }
        let reference = match matches(&references, &item) {
            Err(()) => {
                result["status"] = json!("ambiguous");
                results.push(result);
                continue;
            }
            Ok(row) => row,
        };
        let model = reference
            .map(|row| text(row, "reference_model"))
            .unwrap_or(text(&item, "model"));
        if reference.is_some() {
            result["reference_model"] = json!(model);
        }
        let mut key = model.to_string();
        let eng = text(&item, "engine");
        if matches!(eng, "grok" | "agy") && reference.is_none()
            && let Ok((base, selections)) = crate::agent_runtime::catalog::configured_model_selection(model)
            && selections.iter().all(|selection| matches!(selection.id.as_str(), "acp:cfg:context" | "acp:cfg:context_window" | "acp:cfg:contextWindow"))
        {
            key = base;
        }
        if reference.is_none() {
            if text(raw, "catalog_provider") == "ollama-cloud"
                && raw
                    .get("route_provenance")
                    .is_some_and(|v| !v.is_null() && v != &json!(false))
                && !key.contains('/')
            {
                key = format!("ollama-cloud/{key}");
            } else if ["cursor", "devin"].contains(&eng) && !key.starts_with(&format!("{eng}/")) {
                key = format!("{eng}/{key}");
            } else if eng == "grok"
                && text(&item, "credential_id") == "system:grok"
                && item.get("provider_id").is_none()
                && !key.contains('/')
            {
                key = if rates.contains_key(&format!("grok/{key}")) {
                    format!("grok/{key}")
                } else {
                    format!("xai/{key}")
                };
            } else if !key.contains('/') && item.get("provider_id").is_some() {
                key = format!("{}/{key}", text(&item, "provider_id").to_lowercase());
            }
        }
        let candidates: Vec<_> = rates
            .keys()
            .filter(|name| {
                name.rsplit('/').next() == key.rsplit('/').next()
                    && (!["cursor", "devin"].contains(&eng) || name.starts_with(&format!("{eng}/")))
            })
            .cloned()
            .collect();
        result["candidates"] = json!(candidates);
        if reference.is_none()
            && [
                "<synthetic>",
                "synthetic",
                "opus",
                "sonnet",
                "haiku",
                "fable",
                "auto",
                "default",
                "unknown",
            ]
            .contains(&model.rsplit('/').next().unwrap_or(model))
        {
            results.push(result);
            continue;
        }
        let Some(mut rate) = rates.get(&key) else {
            result["status"] = json!(if !candidates.is_empty()
                && candidates
                    .iter()
                    .all(|k| rates[k]["pricing_status"] == "subscription_unknown")
            {
                "subscription_unknown"
            } else if candidates.len() > 1 {
                "ambiguous"
            } else {
                "missing"
            });
            results.push(result);
            continue;
        };
        result["catalog_model"] = json!(key);
        if rate["pricing_status"] == "subscription_unknown" {
            result["status"] = json!("subscription_unknown");
            results.push(result);
            continue;
        }
        let prompt = number(&raw["input_tokens"]).or_else(|| {
            Some(
                number(&raw["uncached_input"])?
                    + number(&raw["cache_read"])?
                    + number(&raw["cache_write"])?,
            )
        });
        let mut unknown = false;
        if eng == "cursor" {
            let meta = &rate["cursor_pricing"];
            if (raw.get("speed").is_none() && !text(meta, "fast_model_id").is_empty())
                || !["standard", "fast"].contains(&speed)
            {
                unknown = true;
            }
            if speed == "fast" && !text(meta, "variant").contains("fast") {
                if let Some(next) = rates.get(&format!("cursor/{}", text(meta, "fast_model_id"))) {
                    rate = next;
                } else {
                    unknown = true;
                }
            }
            if let Some(threshold) =
                number(&rate["cursor_pricing"]["long_context_threshold_input_tokens"])
            {
                if let Some(prompt) = prompt {
                    let target = text(
                        &rate["cursor_pricing"],
                        if prompt > threshold {
                            "long_context_model_id"
                        } else {
                            "short_context_model_id"
                        },
                    );
                    if !target.is_empty() {
                        if let Some(next) = rates.get(&format!("cursor/{target}")) {
                            rate = next;
                        } else {
                            unknown = true;
                        }
                    }
                } else {
                    unknown = true;
                }
            }
        }
        let tier = if eng == "cursor" {
            "standard"
        } else if rate.get("long_context").is_some() && prompt.unwrap_or(0.0) > 200000.0 {
            if speed == "standard" {
                "long_context"
            } else {
                "unsupported"
            }
        } else {
            speed
        };
        if unknown || rate.get(tier).is_none() {
            result["status"] = json!("unknown-tier");
        } else {
            result["status"] = json!("catalog");
            result["rates"] = rate[tier].clone();
        }
        results.push(result);
    }
    Ok(json!({"items":results}))
}

/// Describe catalog availability without repricing captured usage records.
pub fn metadata(store: &Store) -> rusqlite::Result<Value> {
    Ok(rates(store)?.1)
}

#[cfg(test)]
mod refresh_tests {
    use super::*;

    fn feed() -> Value {
        json!({"_meta":{"schema_version":1,"generated_at":"2026-10-29T00:00:00Z"},
            "provider/model":{"input_cost_per_token":0.000002,"output_cost_per_token":0.000004,
                "cache_read_input_token_cost":0.0000005,"provider_specific_entry":{"fast":2}}})
    }
    #[test]
    fn validates_feed_and_preserves_tiers_and_unknown_cache_prices() {
        let document = feed();
        validate_feed(&document).unwrap();
        let rows = parse_rates(&document);
        assert_eq!(rows["provider/model"]["standard"]["input"], 2.0);
        assert_eq!(rows["provider/model"]["fast"]["output"], 8.0);
        assert!(rows["provider/model"]["standard"]["cache_write"].is_null());
        for metadata in [
            json!({"schema_version":2,"generated_at":"2026-10-29"}),
            json!({"schema_version":1.0,"generated_at":"2026-10-29"}),
            json!({"schema_version":1,"generated_at":"invalid"}),
        ] {
            let mut invalid = document.clone();
            invalid["_meta"] = metadata;
            assert!(validate_feed(&invalid).is_err());
        }
        let mut empty = document.clone();
        empty.as_object_mut().unwrap().remove("provider/model");
        assert!(validate_feed(&empty).is_err());
        let mut overflow = document;
        overflow["provider/model"]["input_cost_per_token"] = json!(1e308);
        assert!(validate_feed(&overflow).is_err());
    }
    #[test]
    fn cache_windows_backoff_and_failure_preserve_complete_snapshot() {
        let store = Store::open(":memory:").unwrap();
        let document = feed();
        let imported = json!({"document":document,"fetched_at":100});
        store
            .put("import_metadata", "_usage_model_rates.json", &imported)
            .unwrap();
        let attempt = 100000.0;
        assert!(should_refresh(&Value::Null, &Value::Null, false, attempt));
        publish_attempt(&store, Value::Null, Err("HTTP 503".into()), attempt).unwrap();
        let (rows, meta) = rates(&store).unwrap();
        assert_eq!(rows["provider/model"]["standard"]["input"], 2.0);
        assert_eq!(meta["status"], "cached");
        assert_eq!(meta["last_error"], "HTTP 503");
        let failed = store.get(LIVE_COLLECTION, LIVE_KEY).unwrap().unwrap();
        assert!(!should_refresh(&failed, &failed, true, attempt + 59.0));
        assert!(should_refresh(&failed, &failed, true, attempt + 60.0));
        publish_attempt(&store, failed, Ok(document.clone()), attempt + 60.0).unwrap();
        let valid = store.get(LIVE_COLLECTION, LIVE_KEY).unwrap().unwrap();
        assert!(!should_refresh(&valid, &valid, true, attempt + 119.0));
        assert!(should_refresh(&valid, &valid, true, attempt + 120.0));
        assert!(!should_refresh(&valid, &valid, false, attempt + 86459.0));
        assert!(should_refresh(&valid, &valid, false, attempt + 86460.0));
        let generated = generated_timestamp(&document["_meta"]["generated_at"]).unwrap();
        assert_eq!(
            describe_snapshot(&valid, &valid, true, generated)["status"],
            "fresh"
        );
        assert_eq!(
            describe_snapshot(&valid, &valid, true, generated + 172801.0)["status"],
            "cached"
        );
        publish_attempt(
            &store,
            valid.clone(),
            Err("invalid JSON".into()),
            attempt + 120.0,
        )
        .unwrap();
        let failed = store.get(LIVE_COLLECTION, LIVE_KEY).unwrap().unwrap();
        assert_eq!(failed["document"], valid["document"]);
        assert_eq!(failed["fetched_at"], valid["fetched_at"]);
        assert_eq!(rates(&store).unwrap().1["last_error"], "invalid JSON");
        assert_eq!(
            store
                .get("import_metadata", "_usage_model_rates.json")
                .unwrap()
                .unwrap(),
            imported
        );
    }
    #[test]
    fn fresh_imported_snapshot_respects_cache_window_and_live_attempt_backoff() {
        let store = Store::open(":memory:").unwrap();
        let imported = json!({"source":RATE_URL,"document":feed(),"fetched_at":100000.0});
        store
            .put("import_metadata", "_usage_model_rates.json", &imported)
            .unwrap();
        let (snapshot, _) = selected_snapshot(&store, &Value::Null).unwrap();
        assert!(!should_refresh(&snapshot, &Value::Null, false, 100061.0));
        assert!(should_refresh(&snapshot, &Value::Null, true, 100061.0));
        let failed = json!({"last_attempt_at":100060.0,"last_error":"timeout"});
        assert!(!should_refresh(&snapshot, &failed, true, 100061.0));
        assert!(should_refresh(&snapshot, &failed, true, 100120.0));
        assert_eq!(
            store
                .get("import_metadata", "_usage_model_rates.json")
                .unwrap()
                .unwrap(),
            imported
        );
        assert!(store.get(LIVE_COLLECTION, LIVE_KEY).unwrap().is_none());
    }
    #[test]
    fn failed_first_load_reports_unavailable() {
        let store = Store::open(":memory:").unwrap();
        publish_attempt(&store, Value::Null, Err("timeout".into()), 100000.0).unwrap();
        let (rows, meta) = rates(&store).unwrap();
        assert!(rows.is_empty());
        assert_eq!(meta["status"], "unavailable");
        assert_eq!(meta["last_error"], "timeout");
    }
}
