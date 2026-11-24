//! Source-shaped SRC business model and derived context versions.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

pub(super) fn save(s: &AppState, id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    let input = body["payload"].as_object().ok_or_else(|| {
        Error::new(
            StatusCode::BAD_REQUEST,
            "query.arguments_invalid",
            "payload must be an object",
        )
    })?;
    if input.get("idFlowGraph").is_some_and(|value| !value.is_null() && !value.is_array()) {
        return Err(Error::new(StatusCode::BAD_REQUEST, "business_model_invalid", "idFlowGraph must be an array"));
    }
    for key in [
        "roles",
        "accounts",
        "tenants",
        "object_types",
        "ownership",
        "state_transitions",
        "sensitive_actions",
        "constraints",
    ] {
        let Some(value) = input.get(key).filter(|value| truthy(value)) else {
            continue;
        };
        let rows = value.as_array().ok_or_else(|| {
            Error::new(
                StatusCode::BAD_REQUEST,
                "business_model_invalid",
                format!("{key} must be a list"),
            )
        })?;
        for (index, row) in rows.iter().enumerate() {
            let provenance = row["provenance"].as_str().unwrap_or("");
            if !matches!(
                provenance,
                "user_description" | "document" | "observation" | "model_inference"
            ) {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "business_provenance_invalid",
                    format!("{key}[{index}] requires a valid provenance"),
                ));
            }
            if provenance != "model_inference" && !truthy(&row["source_refs"]) {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "business_source_missing",
                    format!("{key}[{index}] requires source refs"),
                ));
            }
        }
    }
    let parent_id = body["model_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("business-model-{}", Uuid::new_v4().simple()));
    let mut payload = Value::Object(input.clone());
    payload["updated_at"] = json!(now());
    payload["coverage_claim"] = json!("not_claimed");
    save_research_versioned(
        s,
        id,
        "business_model",
        &parent_id,
        research_base_revision(body)?,
        payload,
        RESEARCH_BUSINESS_MODEL_HEADS,
        RESEARCH_BUSINESS_APPLIED,
        command_id,
        &[],
    )
}

fn normalize_runtime_path(raw: &str) -> String {
    let trimmed = raw.split(['?', '#']).next().unwrap_or("").trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = trimmed
        .split('/')
        .map(|seg| {
            let is_numeric = !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit());
            let is_uuid = seg.len() == 36
                && seg.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
                && seg.matches('-').count() == 4;
            let is_hex_id = seg.len() >= 20 && seg.bytes().all(|b| b.is_ascii_hexdigit());
            if is_numeric || is_uuid || is_hex_id || (seg.starts_with(':') && seg.len() > 1) {
                "{id}"
            } else {
                seg
            }
        })
        .collect();
    let joined = parts.join("/");
    if joined.starts_with('/') {
        joined
    } else {
        format!("/{joined}")
    }
}

fn merge_string_arrays(existing: &Value, incoming: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for item in existing
        .as_array()
        .into_iter()
        .flatten()
        .chain(incoming.as_array().into_iter().flatten())
        .filter_map(Value::as_str)
    {
        if !item.is_empty() && !out.iter().any(|x| x == item) {
            out.push(item.to_owned());
        }
    }
    out
}

fn endpoints(s: &AppState, id: &str) -> Api<BTreeMap<(String, String), Value>> {
    task(s, id)?;
    let _ = super::sync_inspection(s, id);
    let rows = s.store.list_inserted(RECORDS).map_err(db)?;
    let mut endpoints = BTreeMap::new();

    for stored in &rows {
        let row = &stored["values"];
        if row["task_id"] != id || row["kind"] != "static_candidate" {
            continue;
        }
        let payload = &row["payload"];
        let raw_val = payload["resolved_path"]
            .as_str()
            .or_else(|| payload["normalized_path"].as_str())
            .or_else(|| payload["value"].as_str())
            .unwrap_or("");
        let path_part = if raw_val.contains("://") {
            reqwest::Url::parse(raw_val)
                .ok()
                .map(|u| u.path().to_owned())
                .unwrap_or_default()
        } else {
            raw_val.to_owned()
        };
        let clean_lower = path_part.to_ascii_lowercase();
        if path_part.is_empty()
            || clean_lower.ends_with(".js")
            || clean_lower.ends_with(".mjs")
            || clean_lower.ends_with(".css")
            || clean_lower.ends_with(".png")
            || clean_lower.ends_with(".svg")
            || clean_lower.ends_with(".ico")
            || clean_lower.ends_with(".woff2")
        {
            continue;
        }
        let path = normalize_runtime_path(&path_part);
        let method = payload["method"]
            .as_str()
            .filter(|m| !m.is_empty())
            .unwrap_or("GET")
            .to_uppercase();
        let key = (method.clone(), path.clone());
        let prior = endpoints.get(&key);
        let mut refs = prior
            .and_then(|r: &Value| r["source_refs"].as_array())
            .cloned()
            .unwrap_or_default();
        if !refs.contains(&row["record_id"]) {
            refs.push(row["record_id"].clone());
        }
        let q_params = merge_string_arrays(
            prior.map(|r| &r["query_params"]).unwrap_or(&Value::Null),
            &payload["query_params"],
        );
        let b_params = merge_string_arrays(
            prior.map(|r| &r["body_params"]).unwrap_or(&Value::Null),
            &payload["body_params"],
        );
        endpoints.insert(
            key,
            json!({
                "method": method,
                "path": path,
                "query_params": q_params,
                "body_params": b_params,
                "source_refs": refs,
                "evidence_state": "static_candidate"
            }),
        );
    }

    for stored in &rows {
        let row = &stored["values"];
        if row["task_id"] != id || row["kind"] != "openapi_operation" {
            continue;
        }
        let payload = &row["payload"];
        let raw_path = payload["path"].as_str().unwrap_or("");
        let method = payload["method"].as_str().unwrap_or("").to_uppercase();
        if raw_path.is_empty() || method.is_empty() {
            continue;
        }
        let path = normalize_runtime_path(raw_path);
        let key = (method.clone(), path.clone());
        let prior = endpoints.get(&key);
        let mut refs = prior
            .and_then(|r: &Value| r["source_refs"].as_array())
            .cloned()
            .unwrap_or_default();
        if !refs.contains(&row["record_id"]) {
            refs.push(row["record_id"].clone());
        }
        let q_params = prior
            .and_then(|r| r["query_params"].as_array())
            .cloned()
            .unwrap_or_default();
        let b_params = prior
            .and_then(|r| r["body_params"].as_array())
            .cloned()
            .unwrap_or_default();
        endpoints.insert(
            key,
            json!({
                "method": method,
                "path": path,
                "query_params": q_params,
                "body_params": b_params,
                "source_refs": refs,
                "evidence_state": "document_declared"
            }),
        );
    }

    for stored in &rows {
        let row = &stored["values"];
        if row["task_id"] != id || row["kind"] != "http_exchange" {
            continue;
        }
        let payload = &row["payload"];
        // A saved request template or denied/pending request was never an
        // observed HTTP response. Do not mark its static endpoint as probed.
        if payload["execution_state"] != "completed" || !payload["status_code"].is_number()
            || !payload["response_raw_ref"].is_object() { continue; }
        let (raw_path, url_query_keys) = payload["observed_url"]
            .as_str()
            .and_then(|value| reqwest::Url::parse(value).ok())
            .map(|url| {
                let qk: Vec<Value> = url.query_pairs().map(|(k, _)| json!(k.into_owned())).collect();
                (url.path().to_owned(), Value::Array(qk))
            })
            .unwrap_or_else(|| (String::new(), json!([])));
        let method = payload["method"].as_str().unwrap_or("").to_uppercase();
        if raw_path.is_empty() || method.is_empty() {
            continue;
        }
        let path = normalize_runtime_path(&raw_path);
        let key = (method.clone(), path.clone());
        let prior = endpoints.get(&key);
        let mut refs = prior
            .and_then(|row: &Value| row["source_refs"].as_array())
            .cloned()
            .unwrap_or_default();
        if !refs.contains(&row["record_id"]) {
            refs.push(row["record_id"].clone());
        }
        let q_params = merge_string_arrays(
            prior.map(|r| &r["query_params"]).unwrap_or(&Value::Null),
            &url_query_keys,
        );
        let b_params = merge_string_arrays(
            prior.map(|r| &r["body_params"]).unwrap_or(&Value::Null),
            &json!([]),
        );
        endpoints.insert(
            key,
            json!({
                "method": method,
                "path": path,
                "query_params": q_params,
                "body_params": b_params,
                "source_refs": refs,
                "evidence_state": "runtime_observed"
            }),
        );
    }
    Ok(endpoints)
}

pub(super) fn derive(s: &AppState, id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    let endpoints = endpoints(s, id)?;
    let mut domains: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut unprobed_static = Vec::new();
    for endpoint in endpoints.values() {
        if endpoint["evidence_state"] == "static_candidate" {
            unprobed_static.push(endpoint.clone());
        }
        let domain = endpoint["path"]
            .as_str()
            .unwrap_or("")
            .split('/')
            .filter(|part| !part.is_empty() && *part != "{id}")
            .find(|part| {
                !matches!(
                    part.to_ascii_lowercase().as_str(),
                    "api" | "v1" | "v2" | "v3" | "rest" | "trpc"
                )
            })
            .unwrap_or("root")
            .to_owned();
        domains.entry(domain).or_default().push(endpoint.clone());
    }
    let rank = |row: &Value| match row["method"].as_str().unwrap_or("") {
        "GET" => 0,
        "POST" => 1,
        "PUT" | "PATCH" => 2,
        "DELETE" => 3,
        _ => 4,
    };
    let mut domain_rows = Vec::new();
    for (domain, mut rows) in domains {
        rows.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then(a["path"].as_str().cmp(&b["path"].as_str()))
        });
        domain_rows.push(json!({"domain":domain,"endpoints":rows}));
    }
    // URL/parameter spelling does not prove object identity or data flow.
    // Only carry relationships explicitly proposed in the business model.
    let declared_model = records_with_rowid(s, id, "business_model")?.into_iter()
        .max_by_key(|(rowid, _)| *rowid).map(|(_, row)| row);
    let (id_flow, relation_error) = match declared_model.as_ref().and_then(|row| row["payload"].get("idFlowGraph")).filter(|value| !value.is_null()) {
        None => (json!([]), None),
        Some(value) if value.is_array() => (value.clone(), None),
        Some(_) => (json!([]), Some(json!({"code":"invalid_id_flow_graph",
            "message":"Stored model relations are not an array; inspect business_model_ref and save a corrected version"}))),
    };
    let mut source_record_ids = endpoints
        .values()
        .flat_map(|row| {
            row["source_refs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    if let Some(model) = &declared_model {
        if let Some(record_id) = model["record_id"].as_str() { source_record_ids.insert(record_id.to_owned()); }
    }
    let payload = json!({"domains":domain_rows,"idFlowGraph":id_flow,
        "unprobed_static_endpoints":unprobed_static,
        "endpoint_count":endpoints.len(),"source_record_ids":source_record_ids,
        "derived_at":now(),"coverage_claim":"not_claimed","relation_is_candidate":true,"relation_source":"declared_business_model","relation_validation_error":relation_error,
        "business_model_ref":declared_model.as_ref().map(|row|json!({"record_id":row["record_id"],"revision":row["revision"],"version":row["payload"]["version"]}))});
    let parent_id = body["model_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("derived-business-context");
    save_research_versioned(
        s,
        id,
        "business_context",
        parent_id,
        None,
        payload,
        RESEARCH_BUSINESS_CONTEXT_HEADS,
        RESEARCH_BUSINESS_APPLIED,
        command_id,
        &[],
    )
}
