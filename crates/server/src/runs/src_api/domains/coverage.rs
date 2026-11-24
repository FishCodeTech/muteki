//! Task-local coverage claims, attempts and evidence-backed audit.
use super::{Api, Error, RECORDS, db, now, rec, rec_key, records, require, stage_rec};
use super::super::COVERAGE_APPLIED;
use crate::AppState;
use axum::http::StatusCode;
use serde_json::{Value, json};

fn refs(s: &AppState, id: &str, value: &Value, kinds: &[&str]) -> Api<Vec<String>> {
    let mut out = Vec::new();
    for row in value.as_array().into_iter().flatten() {
        let reference = row.as_str().map(str::to_owned).unwrap_or_else(|| row.to_string());
        if reference.is_empty() {
            continue;
        }
        if !kinds.iter().any(|kind| rec(s, id, kind, &reference).is_ok()) {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "src.evidence_missing",
                format!("evidence record unavailable: {reference}"),
            ));
        }
        if !out.iter().any(|v| v == &reference) {
            out.push(reference);
        }
    }
    Ok(out)
}
fn found(s: &AppState, id: &str, kind: &str, key: &str) -> Api<Option<Value>> {
    Ok(records(s, id, kind)?
        .into_iter()
        .find(|v| v["natural_key"] == key))
}
fn matching_attempt(s: &AppState, id: &str, identity: &Value) -> Api<Option<Value>> {
    Ok(s.store.list_inserted(RECORDS).map_err(db)?.into_iter()
        .map(|stored| stored["values"].clone())
        .find(|row| {
            if row["task_id"] != id || row["kind"] != "coverage_attempt" { return false; }
            let payload = &row["payload"];
            ["item_id", "exchange_ids", "observation_refs", "execution_receipt_ids",
                "outcome", "not_applicable_reason"].iter()
                .all(|field| payload[*field] == identity[*field])
        }))
}
struct Change {
    kind: &'static str,
    key: String,
    payload: Value,
    expected: u64,
    parent_id: String,
}

fn applied(s: &AppState, command_id: Option<&str>) -> Api<Option<Value>> {
    command_id
        .map(|command_id| {
            s.store
                .get(COVERAGE_APPLIED, command_id)
                .map_err(db)
                .map(|marker| marker.map(|marker| marker["output"].clone()))
        })
        .transpose()
        .map(|value| value.flatten())
}

fn commit(
    s: &AppState,
    id: &str,
    changes: Vec<Change>,
    command_id: Option<&str>,
    output: Value,
) -> Api<Value> {
    if let Some(saved) = applied(s, command_id)? {
        return Ok(saved);
    }
    let mut expected = Vec::new();
    let mut writes = Vec::new();
    for change in changes {
        let rid = rec_key(id, change.kind, &change.key);
        let prior = s.store.get(RECORDS, &rid).map_err(db)?;
        let staged = stage_rec(
            s,
            id,
            change.kind,
            &change.key,
            change.payload,
            Some(change.expected),
        );
        let (_, version, mut row) = match staged {
            Ok(staged) => staged,
            Err(error) => {
                if let Some(saved) = applied(s, command_id)? {
                    return Ok(saved);
                }
                return Err(error);
            }
        };
        row["parent_id"] = json!(change.parent_id);
        row["record_id"] = prior
            .as_ref()
            .map(|prior| prior["values"]["record_id"].clone())
            .unwrap_or_else(|| json!(change.key));
        expected.push((rid.clone(), prior));
        writes.push((rid, json!({"version":version+1,"values":row})));
    }
    let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":output}));
    let mut conditions: Vec<_> = expected
        .iter()
        .map(|(rid, prior)| (RECORDS, rid.as_str(), prior.as_ref()))
        .collect();
    let mut updates: Vec<_> = writes
        .iter()
        .map(|(rid, value)| (RECORDS, rid.as_str(), value))
        .collect();
    if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
        conditions.push((COVERAGE_APPLIED, command_id, None));
        updates.push((COVERAGE_APPLIED, command_id, marker));
    }
    if s.store.put_many_if(&conditions, &updates).map_err(db)? {
        return Ok(output);
    }
    if let Some(saved) = applied(s, command_id)? {
        return Ok(saved);
    }
    Err(Error::conflict("coverage record revision changed"))
}

pub(super) fn apply_command(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
) -> Api<Value> {
    if let Some(saved) = applied(s, Some(command_id))? {
        return Ok(saved);
    }
    match kind {
        "src.coverage.item.save" => save_item_inner(s, id, body, Some(command_id)),
        "src.coverage.item.claim" => claim_inner(s, id, body, Some(command_id)),
        "src.coverage.attempt.record" => record_attempt_inner(s, id, body, Some(command_id)),
        "src.coverage.blocker.record" => record_blocker_inner(s, id, body, Some(command_id)),
        "src.coverage.seed" => seed_from_surface(s, id, body, Some(command_id)),
        _ => Err(Error::invalid("unsupported coverage command")),
    }
}
pub(super) fn seed(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    seed_from_surface(s, id, body, None)
}
fn resolve_seed_base_url(s: &AppState, id: &str, body: &Value) -> Api<reqwest::Url> {
    if let Some(explicit) = body["base_url"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
        let parsed = reqwest::Url::parse(explicit)
            .map_err(|_| Error::invalid("base_url must be an absolute HTTP(S) URL"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(Error::invalid("base_url must be an absolute HTTP(S) URL"));
        }
        target_within_authorized_origins(s, id, &parsed)?;
        return Ok(parsed);
    }
    let t = super::task(s, id)?;
    let p = super::policy(s, id)?;
    let candidates = t["input"]["authorization"]["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(p["authorized_targets"].as_array().into_iter().flatten())
        .filter_map(Value::as_str);
    for raw in candidates {
        let candidate = raw.trim().trim_start_matches("*.");
        if candidate.is_empty() {
            continue;
        }
        let with_scheme = if candidate.contains("://") {
            candidate.to_owned()
        } else if candidate.starts_with("127.0.0.1") || candidate.starts_with("localhost") || candidate.contains(':') {
            format!("http://{candidate}")
        } else {
            format!("https://{candidate}")
        };
        if let Ok(mut parsed) = reqwest::Url::parse(&with_scheme)
            && matches!(parsed.scheme(), "http" | "https")
            && parsed.host_str().is_some()
        {
            parsed.set_path("/");
            parsed.set_query(None);
            parsed.set_fragment(None);
            return Ok(parsed);
        }
    }
    Err(Error::new(
        StatusCode::PRECONDITION_FAILED,
        "coverage_base_url_unresolved",
        "Task authorized targets do not specify an HTTP(S) base URL for coverage seeding",
    ))
}
fn valid_seed_source_refs(s: &AppState, id: &str, raw_refs: &Value, context_id: &str) -> Vec<String> {
    const KINDS: &[&str] = &[
        "research_document",
        "static_candidate",
        "openapi_operation",
        "runtime_observation",
        "http_exchange",
        "exchange",
        "experiment",
        "business_model",
        "business_context",
    ];
    let mut out = Vec::new();
    for v in raw_refs.as_array().into_iter().flatten() {
        let Some(r) = v.as_str().filter(|s| !s.is_empty()) else { continue };
        if KINDS.iter().any(|k| rec(s, id, k, r).is_ok()) && !out.iter().any(|x| x == r) {
            out.push(r.to_owned());
        }
    }
    if out.is_empty() && !context_id.is_empty() {
        out.push(context_id.to_owned());
    }
    out
}
pub(super) fn seed_from_surface(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    if let Some(saved) = applied(s, command_id)? {
        return Ok(saved);
    }
    let existing_ctx = super::records_with_rowid(s, id, "business_context")?
        .into_iter()
        .max_by_key(|(rowid, _)| *rowid)
        .map(|(_, row)| row);
    let context = if body["derive"].as_bool().unwrap_or(true) || existing_ctx.is_none() {
        super::research_business::derive(s, id, &json!({}), None)?
    } else {
        existing_ctx.unwrap()
    };
    let context_id = context["record_id"].as_str().unwrap_or("").to_owned();
    let base_url = resolve_seed_base_url(s, id, body)?;
    let existing_items = records(s, id, "coverage_item")?;
    let mut seen_ids: std::collections::HashSet<String> = existing_items
        .iter()
        .filter_map(|r| r["payload"]["item_id"].as_str().map(str::to_owned))
        .collect();
    let mut seen_targets: std::collections::HashSet<(String, String)> = existing_items
        .iter()
        .filter_map(|r| {
            let m = r["payload"]["method"].as_str()?.to_uppercase();
            let t = r["payload"]["target"].as_str()?.to_owned();
            Some((m, t))
        })
        .collect();

    let mut changes = Vec::new();
    let mut created_items = Vec::new();
    let mut skipped_existing_count = 0usize;
    let stamp = now();

    let mut push_candidate = |item_id: String,
                                  method: String,
                                  target_url: reqwest::Url,
                                  hypothesis: String,
                                  priority: &str,
                                  source_refs: Vec<String>| {
        let target_str = target_url.to_string();
        if seen_ids.contains(&item_id) || seen_targets.contains(&(method.clone(), target_str.clone())) {
            skipped_existing_count += 1;
            return;
        }
        seen_ids.insert(item_id.clone());
        seen_targets.insert((method.clone(), target_str.clone()));
        let parent_id = source_refs.first().cloned().unwrap_or_else(|| id.to_owned());
        let item = json!({
            "item_id": item_id,
            "method": method,
            "target": target_str,
            "hypothesis": hypothesis,
            "priority": priority,
            "source_refs": source_refs,
            "expected_evidence": ["http_exchange", "authorization_matrix"],
            "declared_state": "pending",
            "seeded_from_surface": true,
            "created_at": stamp,
            "updated_at": stamp,
            "verified_means": "effective_test_evidence_not_vulnerability_or_safety_proof"
        });
        created_items.push(item.clone());
        changes.push(Change {
            kind: "coverage_item",
            key: item["item_id"].as_str().unwrap_or("").to_owned(),
            payload: item,
            expected: 0,
            parent_id,
        });
    };

    for domain_row in context["payload"]["domains"].as_array().into_iter().flatten() {
        let domain = domain_row["domain"].as_str().unwrap_or("root");
        for ep in domain_row["endpoints"].as_array().into_iter().flatten() {
            let method = ep["method"].as_str().unwrap_or("GET").to_uppercase();
            let raw_path = ep["path"].as_str().unwrap_or("/");
            let resolved_url = if raw_path.contains("://") {
                reqwest::Url::parse(raw_path).ok()
            } else {
                base_url.join(raw_path).ok()
            };
            let Some(target_url) = resolved_url else { continue };
            if !matches!(target_url.scheme(), "http" | "https")
                || target_within_authorized_origins(s, id, &target_url).is_err()
            {
                continue;
            }
            let target_str = target_url.to_string();
            let item_id = format!("cov-{}", &crate::runs::src_api::digest(&json!([method, target_str]))[..12]);
            let lower_path = raw_path.to_ascii_lowercase();
            let has_params = !ep["query_params"].as_array().is_some_and(Vec::is_empty)
                || !ep["body_params"].as_array().is_some_and(Vec::is_empty);
            let high_risk = method != "GET"
                || has_params
                || ["admin", "auth", "user", "order", "pay", "role", "token", "transfer", "export"]
                    .iter()
                    .any(|kw| lower_path.contains(kw));
            let priority = if high_risk { "high" } else { "medium" };
            let hypothesis = format!(
                "{method} {raw_path} ({domain}): 验证未授权/越权访问控制与输入边界"
            );
            let source_refs = valid_seed_source_refs(s, id, &ep["source_refs"], &context_id);
            push_candidate(item_id, method, target_url, hypothesis, priority, source_refs);
        }
    }

    for edge in context["payload"]["idFlowGraph"].as_array().into_iter().flatten() {
        let endpoint = edge["to_endpoint"]
            .as_str()
            .or_else(|| edge["endpoint"].as_str())
            .or_else(|| edge["from_endpoint"].as_str())
            .unwrap_or("");
        if endpoint.is_empty() {
            continue;
        }
        let method = edge["method"].as_str().unwrap_or("GET").to_uppercase();
        let resolved_url = if endpoint.contains("://") {
            reqwest::Url::parse(endpoint).ok()
        } else {
            base_url.join(endpoint).ok()
        };
        let Some(target_url) = resolved_url else { continue };
        if !matches!(target_url.scheme(), "http" | "https")
            || target_within_authorized_origins(s, id, &target_url).is_err()
        {
            continue;
        }
        let obj = edge["object_type"]
            .as_str()
            .or_else(|| edge["id_field"].as_str())
            .unwrap_or("resource");
        let item_id = format!(
            "cov-flow-{}",
            &crate::runs::src_api::digest(&json!([method, target_url.as_str(), obj]))[..10]
        );
        let hypothesis = format!("ID流 ({obj}) -> {method} {endpoint}: 验证跨租户/跨身份 IDOR 越权");
        let source_refs = valid_seed_source_refs(s, id, &edge["source_refs"], &context_id);
        push_candidate(item_id, method, target_url, hypothesis, "high", source_refs);
    }

    let seeded_count = created_items.len();
    let total_items = existing_items.len() + seeded_count;
    let output = json!({
        "seeded_count": seeded_count,
        "skipped_existing_count": skipped_existing_count,
        "total_items": total_items,
        "base_url": base_url.as_str(),
        "business_context_id": context_id,
        "items": created_items,
    });
    commit(s, id, changes, command_id, output)
}
pub(super) fn save_item(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    save_item_inner(s, id, body, None)
}
fn target_within_authorized_origins(s: &AppState, id: &str, url: &reqwest::Url) -> Api<()> {
    let p = super::policy(s, id)?;
    let Some(targets) = p["authorized_targets"].as_array().filter(|rows| !rows.is_empty()) else {
        return Ok(());
    };
    let actual_host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let actual_port = url.port_or_known_default();
    let allowed = targets.iter().filter_map(Value::as_str).any(|raw| {
        let candidate = if raw.contains("://") {
            raw.to_owned()
        } else {
            format!("https://{raw}")
        };
        let Ok(parsed) = reqwest::Url::parse(&candidate) else {
            return false;
        };
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let base = host.to_ascii_lowercase();
        let base = base.trim_start_matches("*.");
        let host_ok = actual_host == base || actual_host.ends_with(&format!(".{base}"));
        let port_ok = parsed.port().is_none_or(|port| actual_port == Some(port));
        host_ok && port_ok
    });
    if !allowed {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "target_drift",
            "coverage target host/port is outside Task authorized scope",
        ));
    }
    Ok(())
}

fn path_segments_match(pattern_path: &str, actual_path: &str) -> bool {
    let clean = |p: &str| -> Vec<String> {
        p.trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.replace("%7B", "{")
                    .replace("%7b", "{")
                    .replace("%7D", "}")
                    .replace("%7d", "}")
            })
            .collect()
    };
    let pat = clean(pattern_path);
    let act = clean(actual_path);
    if pat.len() != act.len() {
        return false;
    }
    pat.iter().zip(act.iter()).all(|(p, a)| {
        p == a
            || (p.starts_with('{') && p.ends_with('}'))
            || p.starts_with(':')
            || p == "*"
    })
}

fn exchange_matches_item(item: &Value, exchange: &Value) -> bool {
    let item_method = item["method"].as_str().unwrap_or("GET");
    let ex_method = exchange["method"].as_str().unwrap_or("");
    if !item_method.eq_ignore_ascii_case(ex_method) {
        return false;
    }
    let Some(item_url) = item["target"].as_str().and_then(|v| reqwest::Url::parse(v).ok()) else {
        return false;
    };
    let Some(ex_url) = exchange["observed_url"]
        .as_str()
        .or_else(|| exchange["declared_target"].as_str())
        .and_then(|v| reqwest::Url::parse(v).ok())
    else {
        return false;
    };
    item_url.scheme().eq_ignore_ascii_case(ex_url.scheme())
        && item_url
            .host_str()
            .zip(ex_url.host_str())
            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        && item_url.port_or_known_default() == ex_url.port_or_known_default()
        && path_segments_match(item_url.path(), ex_url.path())
}

fn save_item_inner(s: &AppState, id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    let item_id = require(body, "item_id")?;
    let target = require(body, "target")?;
    let url = reqwest::Url::parse(target).map_err(|_| Error::invalid("coverage target must be an absolute HTTP URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::invalid("coverage target must be an absolute HTTP URL"));
    }
    target_within_authorized_origins(s, id, &url)?;
    let hypothesis = require(body, "hypothesis")?;
    let priority = body["priority"].as_str().unwrap_or("medium");
    if !matches!(priority, "low" | "medium" | "high" | "critical") {
        return Err(Error::invalid("unsupported coverage priority"));
    }
    let source_refs = refs(
        s,
        id,
        &body["source_refs"],
        &[
            "research_document",
            "static_candidate",
            "runtime_observation",
            "http_exchange",
            "exchange",
            "experiment",
        ],
    )?;
    let prior = found(s, id, "coverage_item", item_id)?;
    let value = json!({"item_id":item_id,"method":body["method"].as_str().unwrap_or("GET").to_ascii_uppercase(),
        "target":target,"hypothesis":hypothesis,"priority":priority,"source_refs":source_refs,
        "expected_evidence":body["expected_evidence"].as_array().cloned().unwrap_or_default(),
        "declared_state":prior.as_ref().map(|v|v["payload"]["declared_state"].clone()).unwrap_or(json!("pending")),
        "created_at":prior.as_ref().map(|v|v["payload"]["created_at"].clone()).unwrap_or(json!(now())),
        "updated_at":now(),"verified_means":"effective_test_evidence_not_vulnerability_or_safety_proof"});
    let parent_id = source_refs.first().map(String::as_str).unwrap_or(id);
    commit(s, id, vec![Change {
        kind: "coverage_item",
        key: item_id.to_owned(),
        payload: value.clone(),
        expected: prior.as_ref().and_then(|v| v["revision"].as_u64()).unwrap_or(0),
        parent_id: parent_id.to_owned(),
    }], command_id, value)
}
pub(super) fn claim(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    claim_inner(s, id, body, None)
}
fn claim_inner(s: &AppState, id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    let item_id = require(body, "item_id")?;
    let row = rec(s, id, "coverage_item", item_id)?;
    let mut value = row["payload"].clone();
    value["declared_state"] = json!("claimed");
    value["claim_note"] = json!(body["note"].as_str().unwrap_or(""));
    value["claim_evidence_refs"] = json!(refs(
        s,
        id,
        &body["evidence_refs"],
        &["observation", "runtime_observation", "experiment"]
    )?);
    value["updated_at"] = json!(now());
    commit(s, id, vec![Change {
        kind: "coverage_item",
        key: item_id.to_owned(),
        payload: value.clone(),
        expected: row["revision"].as_u64().unwrap_or(0),
        parent_id: row["parent_id"].as_str().unwrap_or("").to_owned(),
    }], command_id, value)
}
pub(super) fn record_attempt(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    record_attempt_inner(s, id, body, None)
}
fn record_attempt_inner(s: &AppState, id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    let item_id = require(body, "item_id")?;
    let mut row = rec(s, id, "coverage_item", item_id)?;
    let reason = body["not_applicable_reason"].as_str().unwrap_or("");
    let outcome = body["outcome"].as_str().unwrap_or("");
    if reason.is_empty()
        && !matches!(
            outcome,
            "negative" | "candidate" | "confirmed" | "inconclusive"
        )
    {
        return Err(Error::invalid("unsupported coverage outcome"));
    }
    let exchanges = refs(s, id, &body["exchange_ids"], &["http_exchange", "exchange"])?;
    let observations = refs(
        s,
        id,
        &body["observation_refs"],
        &["observation", "runtime_observation", "experiment"],
    )?;
    let receipts = refs(
        s,
        id,
        &body["execution_receipt_ids"],
        &["execution_receipt"],
    )?;
    for rid in &receipts {
        let receipt = rec(s, id, "execution_receipt", rid)?["payload"].clone();
        if receipt["dispatch_state"] != "completed" || receipt["outcome_known"] != true {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "src.coverage_evidence_invalid",
                "execution receipt has no known completed outcome",
            ));
        }
    }
    let mut effective = Vec::new();
    let mut imported_only = false;
    for eid in &exchanges {
        let exchange = rec(s, id, "http_exchange", eid)
            .or_else(|_| rec(s, id, "exchange", eid))?["payload"].clone();
        let complete = exchange["capture_state"] == "complete";
        let live_source = exchange["source_kind"].as_str() != Some("import");
        if exchange["execution_state"] == "completed"
            && complete
            && !exchange["status_code"].is_null()
            && exchange_matches_item(&row["payload"], &exchange)
        {
            if live_source {
                effective.push(eid.clone());
            } else {
                imported_only = true;
            }
        }
    }
    let state = if !reason.is_empty() {
        if exchanges.is_empty() && observations.is_empty() {
            return Err(Error::invalid(
                "not_applicable requires environment evidence",
            ));
        }
        "not_applicable"
    } else if !effective.is_empty() && outcome != "inconclusive" {
        "verified"
    } else if imported_only {
        "claimed"
    } else if !exchanges.is_empty() || !receipts.is_empty() {
        "failed"
    } else if !observations.is_empty() {
        "claimed"
    } else {
        "pending"
    };
    let identity = json!({"item_id":item_id,"exchange_ids":exchanges,
        "observation_refs":observations,"execution_receipt_ids":receipts,
        "outcome":outcome,"not_applicable_reason":reason});
    loop {
        if let Some(saved) = applied(s, command_id)? { return Ok(saved); }
        let prior = matching_attempt(s, id, &identity)?;
        let attempt_id = prior.as_ref()
            .and_then(|row| row["payload"]["attempt_id"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("sattempt_{}", uuid::Uuid::new_v4().simple()));
        let attempt_key = prior.as_ref()
            .and_then(|row| row["natural_key"].as_str())
            .unwrap_or(&attempt_id);
        let value = json!({"attempt_id":attempt_id,"item_id":item_id,"outcome":outcome,"exchange_ids":exchanges,
            "effective_exchange_ids":effective,"observation_refs":observations,"execution_receipt_ids":receipts,
            "evidence_state":state,"notes":body["notes"],"not_applicable_reason":reason,"created_at":now(),
            "difference_is_not_vulnerability":true});
        let mut item = row["payload"].clone();
        if state == "not_applicable" { item["declared_state"] = json!(state); }
        item["latest_attempt_id"] = json!(attempt_id);
        item["updated_at"] = json!(now());
        match commit(s, id, vec![
            Change {kind:"coverage_attempt",key:attempt_key.to_owned(),payload:value.clone(),
                expected:prior.as_ref().and_then(|row|row["revision"].as_u64()).unwrap_or(0),
                parent_id:item_id.to_owned()},
            Change {kind:"coverage_item",key:item_id.to_owned(),payload:item,
                expected:row["revision"].as_u64().unwrap_or(0),
                parent_id:row["parent_id"].as_str().unwrap_or("").to_owned()},
        ], command_id, value) {
            Ok(saved) => return Ok(saved),
            Err(error) if error.status == StatusCode::CONFLICT => {
                row = rec(s, id, "coverage_item", item_id)?;
            }
            Err(error) => return Err(error),
        }
    }
}
pub(super) fn record_blocker(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    record_blocker_inner(s, id, body, None)
}
fn record_blocker_inner(s: &AppState, id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    let blocker_id = require(body, "blocker_id")?;
    let category = require(body, "category")?;
    if !matches!(
        category,
        "target_unreachable" | "authentication" | "authorization" | "capture" | "environment"
    ) {
        return Err(Error::invalid("unsupported target blocker category"));
    }
    let target = body["target"].as_str().unwrap_or("");
    let detail = require(body, "detail")?;
    let exchanges = refs(s, id, &body["exchange_ids"], &["http_exchange", "exchange"])?;
    let observations = refs(
        s,
        id,
        &body["observation_refs"],
        &[
            "observation",
            "runtime_observation",
            "identity_verification",
        ],
    )?;
    if exchanges.is_empty() && observations.is_empty() {
        return Err(Error::invalid(
            "target blocker requires environment evidence",
        ));
    }
    let prior = found(s, id, "target_blocker", blocker_id)?;
    let value = json!({"blocker_id":blocker_id,"category":category,"target":target,"detail":detail,
        "exchange_ids":exchanges,"observation_refs":observations,"state":if body["resolved"]==true{"resolved"}else{"open"},
        "is_vulnerability":false,"updated_at":now()});
    commit(s, id, vec![Change {
        kind: "target_blocker",
        key: blocker_id.to_owned(),
        payload: value.clone(),
        expected: prior.as_ref().and_then(|v| v["revision"].as_u64()).unwrap_or(0),
        parent_id: id.to_owned(),
    }], command_id, value)
}
pub(super) fn audit(s: &AppState, id: &str) -> Api<Value> {
    let _ = super::sync_inspection(s, id);
    let mut items = Vec::new();
    let mut counts =
        json!({"pending":0,"claimed":0,"verified":0,"not_applicable":0,"blocked":0,"failed":0});
    let attempts = records(s, id, "coverage_attempt")?;
    for row in records(s, id, "coverage_item")? {
        let mut item = row["payload"].clone();
        let latest = attempts
            .iter()
            .filter(|v| v["payload"]["item_id"] == item["item_id"])
            .max_by_key(|v| v["payload"]["created_at"].as_str())
            .map(|v| v["payload"].clone());
        let declared = item["declared_state"].as_str().unwrap_or("pending");
        let state = if declared == "not_applicable" {
            "not_applicable".to_owned()
        } else {
            latest
                .as_ref()
                .and_then(|v| v["evidence_state"].as_str())
                .unwrap_or(if declared == "claimed" {
                    "claimed"
                } else {
                    "pending"
                })
                .to_owned()
        };
        let state = if counts.get(&state).is_some() {
            state
        } else {
            "pending".to_owned()
        };
        counts[&state] = json!(counts[&state].as_u64().unwrap_or(0) + 1);
        item["evidence_state"] = json!(state);
        item["latest_attempt"] = latest.unwrap_or(Value::Null);
        item["is_verified_coverage"] = json!(state == "verified");
        items.push(item);
    }
    let blockers: Vec<Value> = records(s, id, "target_blocker")?
        .into_iter()
        .map(|v| v["payload"].clone())
        .collect();
    let open: Vec<Value> = blockers
        .iter()
        .filter(|v| v["state"] == "open")
        .cloned()
        .collect();
    let unresolved: Vec<Value> = items
        .iter()
        .filter(|v| {
            matches!(
                v["evidence_state"].as_str(),
                Some("pending" | "claimed" | "failed" | "blocked")
            )
        })
        .cloned()
        .collect();
    let critical: Vec<Value> = unresolved
        .iter()
        .filter(|v| matches!(v["priority"].as_str(), Some("high" | "critical")))
        .cloned()
        .collect();
    let applicable = items.len() as u64 - counts["not_applicable"].as_u64().unwrap_or(0);
    let verified = counts["verified"].as_u64().unwrap_or(0);
    let status = if !open.is_empty() {
        "blocked"
    } else if applicable == 0 {
        "unmodeled"
    } else if !unresolved.is_empty() {
        "coverage_incomplete"
    } else {
        "complete"
    };
    Ok(
        json!({"status":status,
        "items":items,"counts":counts,"total_items":items.len(),"applicable_items":applicable,
        "verified_items":verified,"claimed_items":counts["claimed"],"verified_coverage_ratio":if applicable>0{json!(verified as f64/applicable as f64)}else{Value::Null},
        "low_denominator_warning":applicable<15,"unresolved_items":unresolved,"critical_unresolved_items":critical,
        "blockers":blockers,"open_blockers":open,"coverage_is_not_safety_proof":true,"generated_at":now()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[tokio::test]
    async fn coverage_audit_requires_effective_evidence_and_keeps_blockers_distinct() {
        let state = std::sync::Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let target = "http://127.0.0.1:4280/demo";
        let created = crate::runs::src_api::create_task_inner(
            &state,
            &json!({"title":"Coverage","goal":"Check one path",
            "authorization":{"targets":[target]},"idempotency_key":"coverage-audit-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let outside = save_item(
            &state,
            id,
            &json!({"item_id":"outside","target":"http://127.0.0.1:4280/other",
                "hypothesis":"Check it"}),
        )
        .unwrap();
        assert_eq!(outside["target"], "http://127.0.0.1:4280/other");
        assert_eq!(rec(&state, id, "coverage_item", "outside").unwrap()["record_id"], "outside");
        assert_eq!(
            save_item(
                &state,
                id,
                &json!({"item_id":"not-http","target":"ws://127.0.0.1:4280/other",
                    "hypothesis":"Check it"}),
            )
            .unwrap_err()
            .status,
            StatusCode::BAD_REQUEST
        );
        save_item(
            &state,
            id,
            &json!({"item_id":"auth","target":target,"hypothesis":"Access is correctly bounded",
            "priority":"high"}),
        )
        .unwrap();
        assert_eq!(audit(&state, id).unwrap()["verified_items"], 0);
        let raw = b"GET /demo HTTP/1.1\r\nHost: 127.0.0.1:4280\r\n\r\n";
        let partial = super::super::record_exchange(
            &state,
            id,
            &json!({"request_raw_b64":base64::engine::general_purpose::STANDARD.encode(raw),
            "observed_url":target,"status_code":200}),
        )
        .unwrap();
        assert_eq!(partial["capture_state"], "partial");
        assert!(super::super::record_exchange(&state, id, &json!({
            "request_raw_b64":base64::engine::general_purpose::STANDARD.encode(raw),
            "observed_url":target,"status_code":200,"capture_state":"complete"
        })).is_err());
        let attempt = record_attempt(
            &state,
            id,
            &json!({"item_id":"auth","outcome":"negative","exchange_ids":[partial["exchange_id"]]}),
        )
        .unwrap();
        assert_eq!(attempt["evidence_state"], "failed");
        let complete=super::super::record_exchange(&state,id,&json!({"request_raw_b64":base64::engine::general_purpose::STANDARD.encode(raw),
            "response_raw_b64":base64::engine::general_purpose::STANDARD.encode(b"HTTP/1.1 200 OK\r\n\r\nseen"),
            "observed_url":target,"status_code":200})).unwrap();
        let attempt=record_attempt(&state,id,&json!({"item_id":"auth","outcome":"negative","exchange_ids":[complete["exchange_id"]]})).unwrap();
        assert_eq!(attempt["evidence_state"], "verified");
        assert_eq!(audit(&state, id).unwrap()["verified_items"], 1);
        record_blocker(
            &state,
            id,
            &json!({"blocker_id":"login","category":"authentication","target":target,
            "detail":"account unavailable","exchange_ids":[complete["exchange_id"]]}),
        )
        .unwrap();
        let result = audit(&state, id).unwrap();
        assert_eq!(result["status"], "blocked");
        assert_eq!(result["verified_items"], 1);
        assert_eq!(result["open_blockers"].as_array().unwrap().len(), 1);
    }
}
