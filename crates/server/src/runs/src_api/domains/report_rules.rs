//! Source SRC report document, scope, evidence, and GREEN review rules.
use super::*;
use std::collections::BTreeSet;

const FIELDS: [&str; 18] = [
    "title", "target", "target_version", "scope", "attacker", "function",
    "mechanism", "normal_behavior", "actual_behavior", "steps",
    "decisive_evidence", "confirmed_impact", "conditional_impact", "cvss",
    "remediation", "counterevidence", "limitations", "restoration",
];
const LIST_FIELDS: [&str; 5] = [
    "steps", "decisive_evidence", "attachments", "counterevidence", "limitations",
];

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(items) => !items.is_empty(),
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
        _ => true,
    }
}

fn list(value: &Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.clone(),
        Value::String(_) => vec![value.clone()],
        Value::Object(map) => map.keys().map(|key| json!(key)).collect(),
        _ => Vec::new(),
    }
}

pub(super) fn canonical_document(document: &Value) -> Api<Value> {
    let Some(_) = document.as_object() else {
        return Err(Error::invalid("document must be an object"));
    };
    let mut result = document.clone();
    for field in FIELDS {
        if result.get(field).is_none() {
            result[field] = if LIST_FIELDS.contains(&field) { json!([]) } else { json!("") };
        }
    }
    if result.get("attachments").is_none() {
        result["attachments"] = json!([]);
    }
    let steps = list(&result["steps"]);
    result["steps"] = json!(steps.into_iter().map(|step| {
        if step.is_object() { step } else { json!({"action":step.as_str().map(str::to_owned).unwrap_or_else(||step.to_string())}) }
    }).collect::<Vec<_>>());
    for field in LIST_FIELDS.into_iter().filter(|field| *field != "steps") {
        result[field] = json!(list(&result[field]));
    }
    let mut missing = result["missing_requirements"].as_array().cloned().unwrap_or_default()
        .into_iter().filter_map(|value| value.as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    for field in ["title", "target", "mechanism", "steps", "confirmed_impact"] {
        if !truthy(&result[field]) { missing.insert(field.to_owned()); }
    }
    result["missing_requirements"] = json!(missing);
    Ok(result)
}

pub(super) fn scope(s: &AppState, task_id: &str, document: &Value) -> Api<()> {
    let policy = policy(s, task_id)?;
    let targets = policy["authorized_targets"].as_array().cloned().unwrap_or_default();
    if targets.is_empty() {
        return Err(Error::new(StatusCode::FORBIDDEN, "target_drift",
            "report scope has no persisted execution policy"));
    }
    let allowed = targets.iter().filter_map(Value::as_str).filter_map(|value| {
        let text = if value.contains("://") { value.to_owned() } else { format!("https://{value}") };
        let parsed = reqwest::Url::parse(&text).ok()?;
        let host = parsed.host_str()?.to_ascii_lowercase();
        Some((host.trim_start_matches("*.").to_owned(), explicit_port(&text)))
    }).collect::<Vec<_>>();
    let mut urls = vec![document["target"].as_str().unwrap_or("").to_owned()];
    for exchange in list(&document["exchanges"]) {
        urls.push(exchange["observed_url"].as_str().or_else(||exchange["url"].as_str())
            .unwrap_or("").to_owned());
    }
    for value in urls {
        let parsed = reqwest::Url::parse(&value).map_err(|_| Error::new(
            StatusCode::FORBIDDEN, "target_drift", "report evidence target must be an absolute URL"))?;
        let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
        if !allowed.iter().any(|(allowed_host, port)|
            (host == *allowed_host || host.ends_with(&format!(".{allowed_host}")))
                && port.is_none_or(|port| explicit_port(&value) == Some(port))) {
            return Err(Error::new(StatusCode::FORBIDDEN, "target_drift",
                "report target or evidence is outside the authorized scope"));
        }
    }
    Ok(())
}

pub(super) fn evidence_contract(s: &AppState, task_id: &str, document: &Value) -> Api<()> {
    let category = document["vulnerability_type"].as_str()
        .or_else(|| document["category"].as_str()).unwrap_or("").to_ascii_lowercase();
    let exchanges = list(&document["exchanges"]);
    if matches!(category.as_str(), "idor" | "bac" | "broken_access_control" | "authorization") {
        let mut identities = exchanges.iter().filter(|row|
            row["execution_state"] == "completed" && row["capture_state"] == "complete")
            .filter_map(|row| row["context_id"].as_str().or_else(||row["identity_ref"].as_str()))
            .filter(|value| !value.is_empty()).map(str::to_owned).collect::<BTreeSet<_>>();
        for comparison in list(&document["comparison_refs"]) {
            let comparison_id = comparison.as_str().unwrap_or("");
            let comparison = rec(s, task_id, "exchange_comparison", comparison_id)?["payload"].clone();
            for field in ["baseline_id", "experiment_id"] {
                let eid = require(&comparison, field)?;
                let exchange = match rec(s, task_id, "http_exchange", eid) {
                    Ok(found) => found,
                    Err(e) if e.status == StatusCode::NOT_FOUND => {
                        rec(s, task_id, "import_exchange", eid)?
                    }
                    Err(e) => return Err(e),
                };
                if let Some(context) = exchange["payload"]["context_id"]
                    .as_str()
                    .or_else(|| exchange["payload"]["identity_ref"].as_str())
                    .filter(|v| !v.is_empty())
                {
                    identities.insert(context.to_owned());
                }
            }
        }
        if identities.len() < 2 {
            return Err(Error::new(StatusCode::CONFLICT, "identity_comparison_required",
                "IDOR/BAC reports require complete exchanges or a persisted comparison across two identities"));
        }
    }
    if category == "xss" && !exchanges.is_empty() {
        let types = exchanges.iter().flat_map(|row| list(&row["response_headers"]))
            .filter_map(|pair| {
                let values = pair.as_array()?;
                if values.first()?.as_str()?.eq_ignore_ascii_case("content-type") {
                    values.get(1)?.as_str().map(|value| value.to_ascii_lowercase())
                } else { None }
            }).collect::<Vec<_>>();
        if !types.is_empty() && types.iter().all(|value| value.contains("json")) {
            let mut runtime_proof = false;
            for reference in list(&document["runtime_evidence_refs"]) {
                let runtime = rec(s, task_id, "runtime_observation", reference.as_str().unwrap_or(""))?;
                if truthy(&runtime["payload"]["execution_signal"])
                    || truthy(&runtime["payload"]["dom_state"]) {
                    runtime_proof = true;
                    break;
                }
            }
            if !runtime_proof {
                return Err(Error::new(StatusCode::CONFLICT, "evidence_context_missing",
                    "JSON-only reflection is not XSS evidence without HTML/DOM execution evidence"));
            }
        }
    }
    if let Some(profile) = document["profile"].as_str().filter(|v| !v.is_empty()) {
        match profile {
            "overseas_bounty_v1" => {
                if document["reproduction_steps"].as_array().is_none_or(Vec::is_empty)
                    || document["impact_summary"].as_str().is_none_or(|v| v.trim().is_empty())
                {
                    return Err(Error::new(
                        StatusCode::CONFLICT,
                        "report_profile_incomplete",
                        "overseas_bounty_v1 requires non-empty impact_summary and reproduction_steps",
                    ));
                }
            }
            "domestic_src_v1" => {
                if document["remediation"].as_str().is_none_or(|v| v.trim().is_empty())
                    || document["affected_asset"].as_str().is_none_or(|v| v.trim().is_empty())
                {
                    return Err(Error::new(
                        StatusCode::CONFLICT,
                        "report_profile_incomplete",
                        "domestic_src_v1 requires non-empty affected_asset and remediation",
                    ));
                }
            }
            _ => {
                return Err(Error::invalid(format!(
                    "unsupported report profile '{profile}'; expected overseas_bounty_v1 or domestic_src_v1"
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn green(s: &AppState, task_id: &str, finding_id: &str,
    review: &Value, cvss: &Value, evidence: &[Value]) -> Api<()> {
    finding::refresh_staleness(s, task_id, finding_id, review.get("dependencies"))?;
    let status = finding::status(s, task_id, &json!({"finding_id":finding_id}))?;
    let persisted = &status["review"];
    if status["stale"] == true || status["state"] != "GREEN" {
        return Err(Error::conflict("GREEN requires a current, non-stale Finding review"));
    }
    if review["review_id"].as_str().is_none_or(str::is_empty)
        || review["review_id"] != persisted["review_id"] {
        return Err(Error::conflict("GREEN must consume the latest persisted Finding review"));
    }
    let snap_id_matches = match (
        review.get("dependency_snapshot_id").and_then(Value::as_str).filter(|v| !v.is_empty()),
        persisted.get("dependency_snapshot_id").and_then(Value::as_str).filter(|v| !v.is_empty()),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => review["dependency_digest"] == persisted["dependency_digest"],
    };
    let snap_obj_matches = match (
        review.get("dependency_snapshot").filter(|v| v.is_object()),
        persisted.get("dependency_snapshot").filter(|v| v.is_object()),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    if !snap_id_matches || !snap_obj_matches {
        return Err(Error::conflict("GREEN review dependencies do not match"));
    }
    if review["validation_type"] != "live_reproduction" {
        return Err(Error::conflict("GREEN requires live_reproduction"));
    }
    let assessed = finding::assess_cvss(cvss)?;
    if assessed != *cvss || assessed != persisted["cvss"] {
        return Err(Error::conflict("GREEN report CVSS must match its reasoned assessment and latest review"));
    }
    let reproduction = if review["independent_reproduction"].is_object() {
        &review["independent_reproduction"]
    } else {
        &persisted["independent_reproduction"]
    };
    if reproduction["success"] != true {
        return Err(Error::conflict("GREEN requires a successful independent reproduction record"));
    }
    let reproduction_id = require(reproduction, "reproduction_id")?;
    if persisted["independent_reproduction"].is_object()
        && persisted["independent_reproduction"]["reproduction_id"]
            .as_str()
            .is_some_and(|expected| expected != reproduction_id)
    {
        return Err(Error::conflict(
            "GREEN reproduction record differs from the persisted Finding review",
        ));
    }
    let saved = rec(s, task_id, "report_reproduction", reproduction_id)?;
    let saved = &saved["payload"];
    if saved["success"] != true || saved["finding_id"] != finding_id
        || saved["reviewer"] != review["reviewer"]
        || saved["target_version"] != review["target_version"]
        || saved["execution_receipt_ids"].as_array().is_none_or(Vec::is_empty)
        || evidence.is_empty() {
        return Err(Error::conflict("GREEN reproduction or readable evidence does not match the reviewed Finding"));
    }
    if let Some(report_id) = saved["report_id"].as_str().filter(|v| !v.is_empty()) {
        let source_report = rec(s, task_id, "frozen_report", report_id)?;
        if source_report["payload"]["finding_id"] != finding_id {
            return Err(Error::conflict("GREEN reproduction belongs to another Finding"));
        }
    }
    Ok(())
}

pub(super) fn source_run_ids(source_run_id: &str, evidence: &[Value], document: &Value) -> Value {
    let mut ids = BTreeSet::from([source_run_id.to_owned()]);
    for reference in evidence {
        if let Some(id) = reference["source_run_id"].as_str().filter(|id| !id.is_empty()) {
            ids.insert(id.to_owned());
        }
    }
    for exchange in list(&document["exchanges"]) {
        if let Some(id) = exchange["source_run_id"].as_str().filter(|id| !id.is_empty()) {
            ids.insert(id.to_owned());
        }
    }
    json!(ids)
}

pub(super) fn effective_severity(review: &Value, cvss: &Value) -> Value {
    let readiness = review["readiness"].as_str().unwrap_or("").to_ascii_uppercase();
    let (confidence, cap) = match readiness.as_str() {
        "GREEN" => ("confirmed", 10.0),
        "YELLOW" => ("likely", 6.9),
        _ => ("possible", 3.9),
    };
    let assessed = cvss["base_score"]
        .as_f64()
        .or_else(|| cvss["base_score"].as_str().and_then(|value| value.parse::<f64>().ok()))
        .or_else(|| cvss["score"].as_f64())
        .or_else(|| cvss["score"].as_str().and_then(|value| value.parse::<f64>().ok()))
        .unwrap_or(0.0);
    let effective = assessed.min(cap);
    let severity = if effective <= 0.0 { "NONE" } else if effective < 4.0 { "LOW" }
        else if effective < 7.0 { "MEDIUM" } else if effective < 9.0 { "HIGH" } else { "CRITICAL" };
    json!({"assessed_score":assessed,"assessed_severity":cvss["base_severity"].as_str().or_else(|| cvss["severity"].as_str()),
        "confidence":confidence,"effective_score":effective,"effective_severity":severity,
        "capped":effective<assessed,"cap_reason":if effective<assessed {
            format!("{confidence} confidence caps severity at {cap}")
        } else {String::new()},"confidence_is_not_independent_reproduction":true})
}

pub(super) const CANONICAL_FIELDS: [&str; 19] = [
    "title", "target", "target_version", "scope", "attacker", "function", "mechanism",
    "normal_behavior", "actual_behavior", "steps", "decisive_evidence", "confirmed_impact",
    "conditional_impact", "cvss", "remediation", "counterevidence", "limitations",
    "restoration", "attachments",
];

fn explicit_port(raw:&str)->Option<u16>{
    let authority=raw.split_once("://")?.1.split(['/', '?', '#']).next()?.rsplit('@').next()?;
    let port=if authority.starts_with('['){authority.split_once(']')?.1.strip_prefix(':')?}else{authority.rsplit_once(':')?.1};port.parse().ok()
}
