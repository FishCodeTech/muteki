use super::*;
use std::time::Duration;

fn looks_like_host_port(value: &str) -> bool {
    if value.contains(char::is_whitespace) {
        return false;
    }
    if value.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if let Some((host, port)) = value.rsplit_once(':')
        && !host.is_empty()
        && !port.is_empty()
        && port.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    false
}

pub fn should_auto_title(custom_name: Option<&str>, raw_challenge: &Value) -> bool {
    if custom_name.is_some_and(|s| !s.trim().is_empty()) {
        return false;
    }
    let raw_name = raw_challenge["name"].as_str().map(str::trim).unwrap_or("");
    if raw_name.is_empty() {
        return true;
    }
    let raw_target = raw_challenge["target"]
        .as_str()
        .map(str::trim)
        .unwrap_or("");
    if !raw_target.is_empty() && raw_name == raw_target {
        return true;
    }
    let lower = raw_name.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ws://")
        || lower.starts_with("wss://")
        || lower.starts_with("run-")
    {
        return true;
    }
    looks_like_host_port(raw_name)
}

fn strip_think_blocks(raw: &str) -> String {
    let mut text = raw.to_owned();
    while let Some(start) = text.find("<think>") {
        if let Some(end_rel) = text[start + 7..].find("</think>") {
            let end = start + 7 + end_rel + 8;
            text.replace_range(start..end, "");
        } else {
            text.truncate(start);
            break;
        }
    }
    text
}

fn clean_title_text(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`'))
        .trim();
    if trimmed.is_empty() {
        return None;
    }
    let truncated: String = trimmed.chars().take(80).collect();
    let final_title = truncated.trim().to_owned();
    (!final_title.is_empty()).then_some(final_title)
}

pub fn parse_title_response(raw: &str) -> Option<String> {
    let without_think = strip_think_blocks(raw);
    let trimmed = without_think.trim();
    if trimmed.is_empty() {
        return None;
    }
    let unfenced = if trimmed.starts_with("```") {
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```JSON")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    if let (Some(start), Some(end)) = (unfenced.find('{'), unfenced.rfind('}'))
        && end >= start
    {
        if let Ok(parsed) = serde_json::from_str::<Value>(&unfenced[start..=end])
            && let Some(title) = parsed["title"].as_str()
        {
            return clean_title_text(title);
        }
    }
    if !unfenced.contains('{') && !unfenced.contains('}') {
        let first_line = unfenced
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("");
        return clean_title_text(first_line);
    }
    None
}

pub fn apply_generated_title(
    state: &AppState,
    run_id: &str,
    title: &str,
) -> Result<bool, String> {
    let Some(clean) = clean_title_text(title) else {
        return Ok(false);
    };
    let Some(mut run_doc) = state.store.get("runs", run_id).map_err(|e| e.to_string())? else {
        return Ok(false);
    };
    if run_doc["custom_name"]
        .as_str()
        .is_some_and(|s| !s.trim().is_empty())
        || run_doc["name_autogen"].as_bool() == Some(false)
    {
        return Ok(false);
    }
    run_doc["name"] = json!(clean);
    run_doc["name_autogen"] = json!(true);
    run_doc["updated"] = json!(now_secs());
    state
        .store
        .put("runs", run_id, &run_doc)
        .map_err(|e| e.to_string())?;
    emit_event(
        state,
        run_id,
        None,
        "run.titled",
        json!({"title": clean, "name": clean, "autogen": true}),
    );
    Ok(true)
}

fn secret_file(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn overlay_account_files(state: &AppState, id: &str, account: &mut Value) {
    let root = crate::native_chat::state_dir(state);
    let private = root.join("_secrets").join("accounts").join(id);
    let imported = root.join("imported-credentials").join("accounts").join(id);
    if let Some(secret) = secret_file(&private.join("API_KEY"))
        .or_else(|| secret_file(&imported.join("API_KEY")))
    {
        account["secret"] = json!(secret);
    }
    if let Some(base_url) = secret_file(&private.join("BASE_URL"))
        .or_else(|| secret_file(&imported.join("BASE_URL")))
    {
        account["base_url"] = json!(base_url);
        account["connection"] = json!("custom_endpoint");
    }
    if let Some(raw) = secret_file(&private.join("MODELS.json"))
        .or_else(|| secret_file(&imported.join("MODELS.json")))
        && let Ok(models) = serde_json::from_str::<Value>(&raw)
        && models.is_array()
    {
        account["models"] = models;
    }
    if let Some(default_model) = secret_file(&private.join("DEFAULT_MODEL"))
        .or_else(|| secret_file(&imported.join("DEFAULT_MODEL")))
    {
        account["default_model"] = json!(default_model);
    }
}

pub(crate) fn configured_profile(state: &AppState, run_id: Option<&str>) -> Option<Value> {
    let run_profile = run_id
        .and_then(|id| super::launch_config::settings(state, id).ok())
        .and_then(|cfg| cfg.get("llm_profiles").and_then(|lp| lp.get("titler")).cloned())
        .filter(|val| val.is_object());
    let global_profile = crate::settings_metadata::configuration(state)
        .ok()
        .and_then(|cfg| cfg.get("llm_profiles").and_then(|lp| lp.get("titler")).cloned())
        .filter(|val| val.is_object());
    let profile = match (run_profile, global_profile) {
        (Some(rp), Some(gp)) => {
            let mut merged = gp;
            if let (Some(dst), Some(src)) = (merged.as_object_mut(), rp.as_object()) {
                for (k, v) in src {
                    let keep_existing = v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty());
                    if !keep_existing {
                        dst.insert(k.clone(), v.clone());
                    }
                }
            }
            merged
        }
        (Some(rp), None) => rp,
        (None, Some(gp)) => gp,
        (None, None) => return None,
    };
    let has_endpoint = profile["endpoint_id"]
        .as_str()
        .or_else(|| profile["credential_id"].as_str())
        .or_else(|| profile["base_url"].as_str())
        .is_some_and(|s| !s.trim().is_empty());
    has_endpoint.then_some(profile)
}

enum TitlerTarget {
    Http {
        provider: crate::provider::TextProvider,
        engine: String,
        account_id: String,
        temperature: Option<f64>,
    },
    Native {
        runtime: Value,
        engine: String,
        account_id: String,
    },
}

fn resolve_titler_target(state: &AppState, profile: &Value) -> Result<Option<TitlerTarget>, String> {
    let endpoint_id = profile["endpoint_id"]
        .as_str()
        .or_else(|| profile["credential_id"].as_str())
        .map(str::trim)
        .unwrap_or("");
    let direct_base = profile["base_url"].as_str().map(str::trim).unwrap_or("");
    let profile_model = profile["model"].as_str().map(str::trim).unwrap_or("");
    if endpoint_id.is_empty() && direct_base.is_empty() {
        return Ok(None);
    }

    let raw_id = endpoint_id
        .strip_prefix("endpoint:")
        .or_else(|| endpoint_id.strip_prefix("account:"))
        .unwrap_or(endpoint_id);
    let canonical = if endpoint_id.is_empty() {
        "endpoint:titler".to_owned()
    } else if endpoint_id.contains(':') {
        endpoint_id.to_owned()
    } else {
        format!("account:{raw_id}")
    };

    let mut account = if endpoint_id.is_empty() {
        json!({"account_id": "titler", "id": "titler"})
    } else {
        crate::provider::matching_account(&state.store, &canonical)
            .ok()
            .flatten()
            .or_else(|| {
                crate::provider::matching_account(&state.store, &format!("account:{raw_id}"))
                    .ok()
                    .flatten()
            })
            .or_else(|| {
                crate::provider::matching_account(&state.store, &format!("endpoint:{raw_id}"))
                    .ok()
                    .flatten()
            })
            .unwrap_or_else(|| json!({"account_id": raw_id, "id": raw_id}))
    };

    if !raw_id.is_empty() {
        overlay_account_files(state, raw_id, &mut account);
    }
    if !direct_base.is_empty() {
        account["base_url"] = json!(direct_base);
        account["connection"] = json!("custom_endpoint");
    }
    if let Some(secret) = profile["api_key"]
        .as_str()
        .or_else(|| profile["secret"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        account["secret"] = json!(secret);
    }

    let model = if !profile_model.is_empty() {
        profile_model.to_owned()
    } else {
        account["default_model"]
            .as_str()
            .map(str::trim)
            .unwrap_or("")
            .to_owned()
    };
    if model.is_empty() {
        return Ok(None);
    }

    let engine = account["worker_engine"]
        .as_str()
        .or_else(|| account["engine"].as_str())
        .or_else(|| profile["provider"].as_str())
        .or_else(|| canonical.strip_prefix("system:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("openai")
        .to_owned();
    let account_id = account["account_id"]
        .as_str()
        .unwrap_or(raw_id)
        .to_owned();

    if account["base_url"]
        .as_str()
        .is_some_and(|value| !value.trim().is_empty())
    {
        account["connection"] = json!("custom_endpoint");
        let provider = crate::provider::provider_from(&account, &canonical, Some(&model))
            .map_err(|e| e.to_string())?;
        let temperature = if profile["temperature_mode"] == "custom" {
            profile["temperature"]
                .as_f64()
                .filter(|v| v.is_finite() && (0.0..=2.0).contains(v))
        } else {
            None
        };
        return Ok(Some(TitlerTarget::Http {
            provider,
            engine,
            account_id,
            temperature,
        }));
    }

    let runtime_key = crate::runtime_api::default_runtime_key(&engine)
        .ok_or_else(|| format!("Titler engine {engine} has no default runtime"))?;
    let runtime = crate::native_credentials::probe_runtime(&canonical, &runtime_key, &model, "");
    Ok(Some(TitlerTarget::Native {
        runtime,
        engine,
        account_id,
    }))
}

pub(crate) fn native_runtime_from_profile(
    state: &AppState,
    profile: &Value,
) -> Result<Option<Value>, String> {
    match resolve_titler_target(state, profile)? {
        Some(TitlerTarget::Native { runtime, .. }) => Ok(Some(runtime)),
        Some(TitlerTarget::Http {
            provider,
            engine,
            ..
        }) => Ok(Some(json!({
            "credential_id": provider.credential_id,
            "model": provider.model,
            "adapter_id": "http",
            "engine": engine,
            "_titler_profile": profile,
        }))),
        None => Ok(None),
    }
}

fn extract_usage(raw_usage: &Value) -> Value {
    if !raw_usage.is_object() {
        return Value::Null;
    }
    let input = raw_usage
        .get("input_tokens")
        .or_else(|| raw_usage.get("prompt_tokens"))
        .cloned()
        .unwrap_or(Value::Null);
    let output = raw_usage
        .get("output_tokens")
        .or_else(|| raw_usage.get("completion_tokens"))
        .cloned()
        .unwrap_or(Value::Null);
    let cache_read = raw_usage
        .get("cache_read_tokens")
        .or_else(|| raw_usage.pointer("/prompt_tokens_details/cached_tokens"))
        .cloned()
        .unwrap_or(Value::Null);
    let reasoning = raw_usage
        .get("reasoning_tokens")
        .or_else(|| raw_usage.pointer("/completion_tokens_details/reasoning_tokens"))
        .cloned()
        .unwrap_or(Value::Null);
    let reported_cost = raw_usage
        .get("cost")
        .or_else(|| raw_usage.get("cost_usd"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "cache_read_tokens": cache_read,
        "cache_write_tokens": raw_usage.get("cache_write_input_tokens").or_else(|| raw_usage.get("cache_creation_input_tokens")).cloned().unwrap_or(Value::Null),
        "reasoning_tokens": reasoning,
        "reported_cost": reported_cost,
        "input_includes_cache": true,
    })
}

pub(crate) async fn prompt_http_endpoint(
    provider: &crate::provider::TextProvider,
    prompt: &str,
    max_tokens: u32,
    temperature: Option<f64>,
) -> Result<(String, Value), String> {
    let mut body = json!({
        "model": provider.model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "stream": false,
    });
    if let Some(temp) = temperature {
        body["temperature"] = json!(temp);
    }
    let client = crate::network_proxy::http_client_builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("titler HTTP client failed: {e}"))?;
    let mut request = client.post(&provider.endpoint).json(&body);
    if let Some(token) = provider.token.as_ref() {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("titler HTTP request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("titler HTTP status {status}"));
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|e| format!("titler HTTP invalid JSON: {e}"))?;
    let content = &payload["choices"][0]["message"]["content"];
    let raw_text = if let Some(s) = content.as_str() {
        s.to_owned()
    } else if let Some(parts) = content.as_array() {
        parts
            .iter()
            .filter_map(|p| {
                p.as_str()
                    .or_else(|| p["text"].as_str())
                    .or_else(|| p["content"].as_str())
            })
            .collect::<Vec<_>>()
            .join("")
    } else {
        String::new()
    };
    Ok((raw_text, extract_usage(&payload["usage"])))
}

pub(crate) async fn prompt_profile_http(
    state: &AppState,
    profile: &Value,
    prompt: &str,
    max_tokens: u32,
) -> Result<(String, Value, String), String> {
    let Some(TitlerTarget::Http {
        provider,
        temperature,
        ..
    }) = resolve_titler_target(state, profile)?
    else {
        return Err("titler profile is not an HTTP endpoint".to_owned());
    };
    let (reply, usage) = prompt_http_endpoint(&provider, prompt, max_tokens, temperature).await?;
    Ok((reply, usage, provider.model))
}

fn build_run_title_prompt(challenge: &Value) -> String {
    let mode = challenge["mode"].as_str().unwrap_or("ctf").trim();
    let target = challenge["target"].as_str().unwrap_or("").trim();
    let category = challenge["category"].as_str().unwrap_or("").trim();
    let desc: String = challenge["description"]
        .as_str()
        .unwrap_or("")
        .trim()
        .chars()
        .take(1200)
        .collect();
    let attachments: Vec<&str> = challenge["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            v.as_str()
                .or_else(|| v["name"].as_str())
                .or_else(|| v["path"].as_str())
        })
        .take(10)
        .collect();
    let payload = json!({
        "mode": mode,
        "category": category,
        "target": target,
        "description": desc,
        "attachments": attachments,
    });
    format!(
        "Generate a concise task title matching the primary language of the input (prefer Chinese or concise technical terms, at most 36 characters). Return ONLY a JSON object with a single \"title\" string key, e.g. {{\"title\": \"...\"}}, and do not use tools.\n{payload}"
    )
}

async fn execute_once(
    state: &Arc<AppState>,
    run_id: &str,
    profile: &Value,
    prompt: &str,
) -> Result<(Option<String>, Value, String, String, String, String), String> {
    let target = resolve_titler_target(state, profile)?
        .ok_or_else(|| "titler target not configured".to_owned())?;
    match target {
        TitlerTarget::Http {
            provider,
            engine,
            account_id,
            temperature,
        } => {
            let (raw_text, usage) =
                prompt_http_endpoint(&provider, prompt, 1024, temperature).await?;
            let title = parse_title_response(&raw_text);
            Ok((
                title,
                usage,
                engine,
                provider.model,
                account_id,
                provider.credential_id,
            ))
        }
        TitlerTarget::Native {
            runtime,
            engine,
            account_id,
        } => {
            let reply = if let Some(res) = crate::chat::extension_prompt_model(state, &runtime, prompt).await
            {
                res?
            } else if crate::native_chat::is_native(&runtime) {
                crate::native_chat::prompt_model(state, &runtime, prompt)
                    .await
                    .map_err(|e| e.message)?
            } else {
                return Err(format!("titler native runtime unsupported for {engine}"));
            };
            let title = reply["reply"]
                .as_str()
                .and_then(parse_title_response);
            let usage = extract_usage(&reply["usage"]);
            let model = reply["model"]
                .as_str()
                .or_else(|| runtime["model"].as_str())
                .unwrap_or("")
                .to_owned();
            let cred_id = runtime["credential_id"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            let _ = run_id;
            Ok((title, usage, engine, model, account_id, cred_id))
        }
    }
}

pub fn spawn_if_needed(state: Arc<AppState>, run_id: String, challenge: Value) {
    let Some(profile) = configured_profile(&state, Some(&run_id)) else {
        return;
    };
    let prompt = build_run_title_prompt(&challenge);
    tokio::spawn(async move {
        let invocation = format!("titler-{}", uuid::Uuid::new_v4().simple());
        for attempt_idx in 0..2 {
            let attempt = format!("attempt-{}", attempt_idx + 1);
            let call = tokio::time::timeout(
                Duration::from_secs(20),
                execute_once(&state, &run_id, &profile, &prompt),
            )
            .await;
            match call {
                Ok(Ok((maybe_title, usage, engine, model, account_id, cred_id))) => {
                    if usage.is_object() {
                        let spec = super::worker::WorkerSpec {
                            solver_id: "titler".to_owned(),
                            profile_id: "titler".to_owned(),
                            profile_label: "titler".to_owned(),
                            raw_engine: engine.clone(),
                            norm_engine: engine,
                            adapter_id: "http".to_owned(),
                            instance_id: "default".to_owned(),
                            model: (!model.is_empty()).then_some(model),
                            effort: None,
                            account_id,
                            effective_credential_id: cred_id,
                            backend: "http".to_owned(),
                            profile: profile.clone(),
                        };
                        let _ = super::usage::record(
                            &state,
                            &run_id,
                            &format!("{invocation}:{attempt}"),
                            &spec,
                            "titler",
                            &attempt,
                            "turn_snapshot",
                            &attempt,
                            &usage,
                        );
                    }
                    if let Some(title) = maybe_title {
                        let _ = apply_generated_title(&state, &run_id, &title);
                        return;
                    }
                }
                Ok(Err(error)) => {
                    tracing::debug!(run_id = %run_id, attempt = %attempt, error = %error, "Run titler attempt failed");
                }
                Err(_) => {
                    tracing::debug!(run_id = %run_id, attempt = %attempt, "Run titler attempt timed out");
                }
            }
        }
    });
}
