//! Read-only competition advisor using external Agent sessions.
use super::*;
mod executor;

const ADVISOR_CONFIG: &str = "competition_advisor_config";
const ADVISOR_STATE: &str = "competition_advisor_state";

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new().route(
        "/api/settings/competition-advisor",
        get(get_config).put(put_config),
    )
}
pub(crate) async fn get_config(State(state): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .store
            .get("settings", ADVISOR_CONFIG)
            .map_err(ApiError::internal)?
            .unwrap_or_else(|| json!({"enabled":false,"interval_seconds":300})),
    ))
}
pub(crate) async fn put_config(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let enabled = body["enabled"]
        .as_bool()
        .ok_or_else(|| ApiError::invalid("enabled must be a boolean"))?;
    let credential_id = field(&body, "credential_id");
    let model = field(&body, "model");
    let adapter_id = field(&body, "adapter_id");
    let instance_id = field(&body, "instance_id");
    let interval = body["interval_seconds"].as_u64().unwrap_or(300);
    if enabled && adapter_id.is_empty() && (credential_id.is_empty() || model.is_empty()) {
        return Err(ApiError::invalid(
            "Enabled advisor requires an adapter or credential_id and model",
        ));
    }
    let config = json!({"enabled":enabled,"adapter_id":adapter_id,"instance_id":instance_id,
        "credential_id":credential_id,"model":model,"interval_seconds":interval,"updated_at":now()});
    state
        .store
        .put("settings", ADVISOR_CONFIG, &config)
        .map_err(ApiError::internal)?;
    Ok(Json(config))
}

pub(super) async fn consult(state: &Arc<AppState>, competition: &Value) -> Value {
    let Ok(Some(config)) = state.store.get("settings", ADVISOR_CONFIG) else {
        return json!({"status":"disabled","recommendations":[]});
    };
    if config["enabled"] != true {
        return json!({"status":"disabled","recommendations":[]});
    }
    let competition_id = field(competition, "competition_id").to_owned();
    let last = match state.store.get(ADVISOR_STATE, &competition_id) {
        Ok(last) => last,
        Err(error) => {
            tracing::warn!(competition_id,%error,"Competition advisor state could not be read");
            return json!({"status":"error","recommendations":[],"error":error.to_string()});
        }
    };
    let claimed = json!({"last_attempt_at":now(),"status":"running"});
    match state.store.put_many_if(
        &[(ADVISOR_STATE, &competition_id, last.as_ref())],
        &[(ADVISOR_STATE, &competition_id, &claimed)],
    ) {
        Ok(true) => {}
        Ok(false) => return latest(state, &competition_id),
        Err(error) => {
            tracing::warn!(competition_id,%error,"Competition advisor claim could not be saved");
            return json!({"status":"error","recommendations":[],"error":error.to_string()});
        }
    }
    let advice = advise(state, &competition_id, &config).await;
    let completed = json!({"last_attempt_at":now(),"status":advice["status"],"advice":advice});
    let event = leases::planned_event(&competition_id, "competition", &competition_id,
        "competition.advisor.advice", "", advice.clone());
    if let Err(error) = put_records_with_events(state, &competition_id,
        &[(ADVISOR_STATE, &competition_id, Some(&claimed))],
        &[(ADVISOR_STATE, &competition_id, &completed)], &[event]) {
        tracing::warn!(competition_id,code=%error.code,"Competition advisor result could not be saved");
        return json!({"status":"error","recommendations":[],"error":error.message});
    }
    advice
}
pub(super) fn latest(state: &AppState, competition_id: &str) -> Value {
    state
        .store
        .get(ADVISOR_STATE, competition_id)
        .ok()
        .flatten()
        .and_then(|v| v.get("advice").cloned())
        .unwrap_or_else(|| json!({"status":"disabled","recommendations":[]}))
}

fn projection(state: &AppState, competition_id: &str) -> ApiResult<Value> {
    let snapshot = live_snapshot(state, competition_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("competition"))?;
    let challenges: Vec<Value> = snapshot["challenges"].as_array().into_iter().flatten().map(|row| {
        let challenge = &row["challenge"];
        let revision = &row["current_revision"];
        let binding = &row["active_binding"];
        let run_id = binding["run_id"].as_str().unwrap_or("");
        let run = if run_id.is_empty() { None } else { state.store.get("runs",run_id).ok().flatten() };
        let run = if run_id.is_empty() { Value::Null } else {
            json!({"run_id":run_id,"binding_state":binding["state"],
                "execution_generation":binding["execution_generation"],
                "summary":run.map(|value|json!({"status":value["status"],
                    "running":value["running"],"solved":value["solved"]})).unwrap_or_else(||json!({}))})
        };
        let queue = &row["queue_entry"];
        json!({"challenge_id":challenge["challenge_id"],"external_challenge_id":challenge["external_challenge_id"],
            "name":challenge["name"],"category":challenge["category"],"state":challenge["state"],"remote_state":challenge["remote_state"],
            "points":revision["points"],"hints":revision["hints"].as_array().map_or(0,Vec::len),"prerequisites":revision["prerequisites"],
            "candidates":row["candidates"].as_array().map_or(0,Vec::len),"submissions":row["submissions"].as_array().map_or(0,Vec::len),
            "queue":if queue.is_object(){json!({"state":queue["state"],"priority":queue["priority"],"score":queue["score"]})}else{Value::Null},
            "run":run})
    }).collect();
    let budgets: serde_json::Map<String, Value> = snapshot["budgets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            v["kind"].as_str().map(|kind| {
                (
                    kind.to_owned(),
                    json!({"limit":v["limit"],"used":v["used"]}),
                )
            })
        })
        .collect();
    Ok(
        json!({"competition_id":competition_id,"title":snapshot["competition"]["title"],
        "scheduler_state":snapshot["competition"]["scheduler_state"],"automation_mode":snapshot["policy"]["automation_mode"],
        "ends_at":snapshot["competition"]["ends_at"],"budgets":budgets,"challenges":challenges}),
    )
}
fn parse_object(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let mut depth = 0;
    for (offset, character) in text[start..].char_indices() {
        if character == '{' { depth += 1; }
        if character == '}' {
            depth -= 1;
            if depth == 0 {
                return serde_json::from_str::<Value>(&text[start..=start + offset])
                    .ok().filter(Value::is_object);
            }
        }
    }
    None
}
fn parse_advice(competition_id: &str, raw: &str) -> Value {
    let Some(body) = parse_object(raw) else {
        return json!({"advice_id":id("adv"),"competition_id":competition_id,"status":"unparseable","summary":"","recommendations":[],"error":"advisor reply is not a JSON object","created_at":now()});
    };
    let recommendations: Vec<Value> = body["recommendations"].as_array().into_iter().flatten().filter_map(|item| {
        let kind = field(item,"kind");
        let kind = if kind.is_empty() { "select" } else { kind };
        if !matches!(kind,"select"|"budget"|"stop"|"hold") { return None; }
        let priority = item["priority"].as_f64().filter(|n|n.is_finite()).unwrap_or(0.0);
        let confidence = item["confidence"].as_f64().filter(|n|n.is_finite()).unwrap_or(0.0).clamp(0.0,1.0);
        Some(json!({"kind":kind,"challenge_id":field(item,"challenge_id"),"external_challenge_id":field(item,"external_challenge_id"),
            "priority":priority,"rationale":field(item,"rationale"),"confidence":confidence}))
    }).collect();
    json!({"advice_id":id("adv"),"competition_id":competition_id,"status":"ok","summary":field(&body,"summary"),
        "recommendations":recommendations,"error":"","created_at":now()})
}

async fn advise(state: &Arc<AppState>, competition_id: &str, config: &Value) -> Value {
    let projection = match projection(state, competition_id) {
        Ok(value) => value,
        Err(_) => {
            return json!({"advice_id":id("adv"),"competition_id":competition_id,"status":"error","summary":"","recommendations":[],"error":"projection unavailable","created_at":now()});
        }
    };
    let prompt = format!(
        "你是 CTF 比赛调度顾问。根据下面的比赛投影 JSON，输出一个 JSON 对象（不要输出其他内容）：\n{{\"summary\":\"...\",\"recommendations\":[{{\"kind\":\"select|budget|stop|hold\",\"challenge_id\":\"<本地 challenge_id 或空>\",\"external_challenge_id\":\"<平台题目 id 或空>\",\"priority\":0,\"rationale\":\"...\",\"confidence\":0.0}}]}}\n规则：select=建议选题，budget=建议调整预算，stop=建议停止某题或全场，hold=建议挂起。不要建议任何平台写操作；你没有平台写权限。\n比赛投影：\n{}",
        projection,
    );
    if let Some(adapter) = executor::selected(state, config) {
        let result = executor::run(state, competition_id, config, &prompt, &projection, &adapter).await;
        tracing::debug!(competition_id, adapter, events=result.events,
            "Competition Advisor Agent session completed");
        let mut advice = if result.text.is_empty() && !result.error.is_empty() {
            json!({"advice_id":id("adv"),"competition_id":competition_id,"status":"error",
                "summary":"","recommendations":[],"error":result.error,"created_at":now()})
        } else {
            parse_advice(competition_id, &result.text)
        };
        if advice["status"] == "ok" && !result.error.is_empty() {
            advice["status"] = json!("error");
            advice["error"] = json!(result.error);
        }
        advice["session_id"] = json!(result.session_id);
        return advice;
    }
    let provider = match crate::provider::resolve(
        &state.store,
        &json!({"credential_id":config["credential_id"],"model":config["model"]}),
    ) {
        Ok(provider) => provider,
        Err(_) => {
            return json!({"advice_id":id("adv"),"competition_id":competition_id,"status":"error","summary":"","recommendations":[],"error":"advisor HTTP credential unavailable","created_at":now()});
        }
    };
    let client = match crate::network_proxy::http_client_builder()
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return json!({"advice_id":id("adv"),"competition_id":competition_id,"status":"error","summary":"","recommendations":[],"error":"advisor HTTP client unavailable","created_at":now()});
        }
    };
    let mut request = client.post(&provider.endpoint).json(&json!({"model":provider.model,"messages":[{"role":"user","content":prompt}],"max_tokens":1000,"stream":false}));
    if let Some(token) = provider.token.as_deref() {
        request = request.bearer_auth(token);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => {
            return json!({"advice_id":id("adv"),"competition_id":competition_id,"status":"error","summary":"","recommendations":[],"error":"advisor request failed","created_at":now()});
        }
    };
    if !response.status().is_success() {
        return json!({"advice_id":id("adv"),"competition_id":competition_id,"status":"error","summary":"","recommendations":[],"error":format!("advisor HTTP {}",response.status().as_u16()),"created_at":now()});
    }
    let body: Value = match response.json().await {
        Ok(body) => body,
        Err(_) => {
            return json!({"advice_id":id("adv"),"competition_id":competition_id,"status":"unparseable","summary":"","recommendations":[],"error":"advisor response was not JSON","created_at":now()});
        }
    };
    let text = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    parse_advice(competition_id, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn advice_parser_accepts_only_known_recommendations() {
        let advice = parse_advice(
            "comp-test",
            r#"```json
{"summary":"Review web first","recommendations":[{"kind":"select","challenge_id":"web","confidence":1.8},{"kind":"execute","challenge_id":"hidden"}]}
```"#,
        );
        assert_eq!(advice["status"], "ok");
        assert_eq!(advice["recommendations"].as_array().unwrap().len(), 1);
        assert_eq!(advice["recommendations"][0]["confidence"], 1.0);
    }
}
