//! The original SRC CycleRunner contract for pinned asset query connectors.
use super::*;
use regex::Regex;
use std::{collections::BTreeSet, net::IpAddr};

fn source_error(code: &'static str, message: impl Into<String>) -> Error {
    Error::new(StatusCode::FORBIDDEN, code, message)
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn cidr_matches(actual: &Value, expected: &Value) -> bool {
    let Some((address, bits)) = expected.as_str().and_then(|value| value.split_once('/')) else {
        return false;
    };
    let Ok(actual) = text(actual).parse::<IpAddr>() else {
        return false;
    };
    let Ok(address) = address.parse::<IpAddr>() else {
        return false;
    };
    let Ok(bits) = bits.parse::<u32>() else {
        return false;
    };
    match (actual, address) {
        (IpAddr::V4(actual), IpAddr::V4(address)) if bits <= 32 => {
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            (u32::from(actual) & mask) == (u32::from(address) & mask)
        }
        (IpAddr::V6(actual), IpAddr::V6(address)) if bits <= 128 => {
            let mask = if bits == 0 {
                0
            } else {
                u128::MAX << (128 - bits)
            };
            (u128::from(actual) & mask) == (u128::from(address) & mask)
        }
        _ => false,
    }
}

fn compare(actual: &Value, operator: &str, expected: &Value) -> Api<bool> {
    let actual_text = text(actual);
    let expected_text = text(expected);
    let result = match operator {
        "exists" => !actual.is_null() == expected.as_bool().unwrap_or(false),
        "eq" => actual == expected,
        "neq" => actual != expected,
        "in" | "not_in" => {
            let contained = expected
                .as_array()
                .map_or(actual == expected, |rows| rows.contains(actual));
            if operator == "in" {
                contained
            } else {
                !contained
            }
        }
        "contains" => actual_text.contains(&expected_text),
        "not_contains" => !actual_text.contains(&expected_text),
        "prefix" => actual_text.starts_with(&expected_text),
        "wildcard" => {
            let pattern = format!(
                "^{}$",
                expected_text
                    .split('*')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(".*")
            );
            Regex::new(&pattern)
                .map_err(|_| Error::invalid("invalid control wildcard"))?
                .is_match(&actual_text)
        }
        "regex" => Regex::new(&expected_text)
            .map_err(|_| Error::invalid("invalid control regex"))?
            .is_match(&actual_text),
        "cidr" => cidr_matches(actual, expected),
        "lt" | "lte" | "gt" | "gte" => {
            let left = actual.as_f64().or_else(|| actual_text.parse().ok());
            let right = expected.as_f64().or_else(|| expected_text.parse().ok());
            match (left, right) {
                (Some(left), Some(right)) => match operator {
                    "lt" => left < right,
                    "lte" => left <= right,
                    "gt" => left > right,
                    _ => left >= right,
                },
                _ => false,
            }
        }
        _ => {
            return Err(source_error(
                "authorization_unresolved",
                format!("unsupported control selector {operator}"),
            ));
        }
    };
    Ok(result)
}

enum Match {
    No,
    Yes,
    Unknown,
}

fn match_mapping(mapping: &Value, facts: &Value) -> Api<Match> {
    let Some(mapping) = mapping.as_object() else {
        return Err(Error::invalid("control rule selector must be an object"));
    };
    let mut unknown = false;
    for (field, condition) in mapping {
        let Some(actual) = facts.get(field) else {
            unknown = true;
            continue;
        };
        if actual.is_null() {
            unknown = true;
            continue;
        }
        if let Some(operators) = condition.as_object() {
            if operators.is_empty() {
                return Ok(Match::No);
            }
            for (operator, expected) in operators {
                if !compare(actual, operator, expected)? {
                    return Ok(Match::No);
                }
            }
        } else if !compare(actual, "eq", condition)? {
            return Ok(Match::No);
        }
    }
    Ok(if unknown { Match::Unknown } else { Match::Yes })
}

fn authorize_control(policy: &Value, facts: &Value) -> Api<Value> {
    let mut denies = Vec::new();
    let mut allows = Vec::new();
    for rule in policy["rules"].as_array().into_iter().flatten() {
        if rule["enabled"] == false
            || rule["mode"] != "enforce"
            || rule["resource_kind"] != "control"
            || rule["evaluation_point"] != "tool"
            || rule["actions"]
                .as_array()
                .is_some_and(|actions| !actions.is_empty() && !actions.contains(&json!("query")))
        {
            continue;
        }
        let selector = match_mapping(&rule["selector"], facts)?;
        let conditions = match_mapping(&rule["conditions"], facts)?;
        if matches!(selector, Match::No)
            || matches!(conditions, Match::No)
            || matches!(selector, Match::Unknown)
            || matches!(conditions, Match::Unknown)
        {
            continue;
        }
        let reference = format!(
            "{}:v{}",
            rule["rule_id"].as_str().unwrap_or("rule"),
            rule["version"].as_u64().unwrap_or(1)
        );
        match rule["effect"].as_str() {
            Some("deny") => denies.push(reference),
            Some("allow") => allows.push(reference),
            _ => {}
        }
    }
    if !denies.is_empty() {
        return Err(source_error(
            "policy_denied",
            "monitor connector query is denied by policy",
        ));
    }
    if allows.is_empty() {
        allows.push("policy_default_allow".to_owned());
    }
    Ok(json!({"matched_rule_refs": allows}))
}

pub(super) fn preflight(s: &AppState, id: &str, definition: &Value) -> Api<Value> {
    let source = &definition["source"];
    let connector_id = require(source, "connector_id")?;
    let manifest = super::super::connector_manifest(connector_id)?;
    if !matches!(definition["chain_type"].as_str(), Some("asset" | "rule"))
        || !manifest["capabilities"]
            .as_array()
            .is_some_and(|items| items.contains(&json!("query")))
    {
        return Err(unavailable(
            "this source has no executable asset query monitor capability",
        ));
    }
    if source["capability_manifest_ref"] != manifest["version"] {
        return Err(unavailable("monitor source capability version changed"));
    }
    let config_version = source["config_version"]
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or_else(|| unavailable("monitor source configuration version is missing"))?;
    let config = connector_config(
        s,
        id,
        &json!({"connector_id":connector_id,"version":config_version}),
    )?;
    if config["version"] != config_version
        || config["manifest"]["version"] != manifest["version"]
        || config["revoked_at"].is_string()
    {
        return Err(unavailable(
            "monitor source configuration changed or was revoked",
        ));
    }
    let mut credential_refs = serde_json::Map::new();
    let settings = &config["config"];
    if manifest["credentials"]["required"] == true {
        let mut missing = Vec::new();
        for field in manifest["credentials"]["reference_fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let val = settings[field].as_str().map(str::trim).unwrap_or("");
            if val.is_empty() {
                missing.push(field.to_owned());
            }
        }
        if !missing.is_empty() {
            return Err(Error::invalid(format!(
                "连接器 {connector_id} 缺少凭据配置: {}",
                missing.join("、")
            )));
        }
    }
    for field in manifest["credentials"]["reference_fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(reference) = settings[field]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if reference.starts_with("secret://") {
                super::super::credential_store::version(s, id, reference)?;
            }
            credential_refs.insert(field.to_owned(), json!(reference));
        }
    }
    if let Some(map) = settings.as_object() {
        for (key, value) in map {
            if let Some(reference) = value
                .as_str()
                .map(str::trim)
                .filter(|v| v.starts_with("secret://"))
                && !credential_refs.contains_key(key)
            {
                super::super::credential_store::version(s, id, reference)?;
                credential_refs.insert(key.clone(), json!(reference));
            }
        }
    }
    if !source["credential_refs"].is_null()
        && source["credential_refs"] != Value::Object(credential_refs.clone())
    {
        return Err(source_error(
            "authorization_unresolved",
            "monitor credential references differ from the pinned source configuration",
        ));
    }
    let plan_ref_value = if let Some(plan_ref) = definition["plan_ref"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let plan = rec(s, id, "research_plan", plan_ref).map_err(|_| {
            source_error(
                "authorization_unresolved",
                "monitor research plan is missing or stale",
            )
        })?;
        if records(s, id, "research_plan")?.iter().any(|row| {
            row["parent_id"] == plan["parent_id"]
                && row["payload"]["version"].as_u64().unwrap_or(0)
                    > plan["payload"]["version"].as_u64().unwrap_or(0)
        }) {
            return Err(source_error(
                "authorization_unresolved",
                "monitor research plan binding is stale",
            ));
        }
        if definition["plan_revision"]
            .as_u64()
            .is_some_and(|revision| plan["revision"] != revision)
        {
            return Err(source_error(
                "authorization_unresolved",
                "monitor research plan version is stale",
            ));
        }
        json!(plan_ref)
    } else {
        Value::Null
    };
    let current_policy = policy(s, id)?;
    let authorization_ref = format!(
        "{}:v{}",
        current_policy["policy_id"].as_str().unwrap_or(""),
        current_policy["version"].as_u64().unwrap_or(0)
    );
    let base_url = super::super::connector_config::connector_base_url(&manifest, Some(settings));
    if !base_url.is_empty()
        && current_policy["excluded_targets"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|v| {
                    v.as_str()
                        .is_some_and(|ex| crate::runs::src_api::target_matches(&base_url, ex))
                })
            })
    {
        return Err(source_error(
            "authorization_unresolved",
            "monitor connector base_url is in excluded_targets",
        ));
    }
    let query = source["query"].as_str().unwrap_or("").trim();
    if connector_id != "scopesentry" && connector_id != "aws_ec2" && query.is_empty() {
        return Err(unavailable("monitor source query is missing"));
    }
    let call_limit = definition["budget"]["call_limit"]
        .as_u64()
        .filter(|v| *v > 0)
        .or_else(|| definition["budget"]["per_cycle"].as_u64().filter(|v| *v > 0))
        .or_else(|| {
            definition["budget"]["per_cycle"]
                .as_f64()
                .filter(|v| *v > 0.0)
                .map(|v| v.ceil() as u64)
        })
        .unwrap_or(0);
    if call_limit == 0 {
        return Err(source_error(
            "budget_exhausted",
            "monitor requires a positive per-cycle call limit",
        ));
    }
    let _ = super::super::connector_config::enforce_grant_for_purpose(
        s,
        id,
        connector_id,
        "monitor",
        false,
    )?;
    let request_args = json!({"connector_id":connector_id,"query":query,
        "config_version":config_version,"credential_refs":credential_refs});
    let identity_ref = if credential_refs.is_empty() {
        String::new()
    } else {
        format!(
            "connector:{connector_id}:{}",
            digest(&json!(credential_refs))
        )
    };
    let mut facts = json!({"task_id":id,"tool_name":"src.connector.query","resource_kind":"control",
        "action":"query","connector_id":connector_id,"config_version":config_version,"query":query,
        "credential_refs":credential_refs.clone(),"identity_ref":identity_ref,"arguments_hash":digest(&request_args)});
    for (field, reference) in &credential_refs {
        facts[field] = reference.clone();
    }
    let decision = authorize_control(&current_policy, &facts)?;
    let max_items = source["max_items"]
        .as_u64()
        .unwrap_or(10_000)
        .clamp(1, 10_000);
    Ok(
        json!({"connector_id":connector_id,"config_version":config_version,"query":query,
        "max_pages":call_limit.min(100),"max_items":max_items,"extra":source["extra"],
        "plan_ref":plan_ref_value,"authorization_ref":authorization_ref,
        "policy_digest":current_policy["digest"],"matched_rule_refs":decision["matched_rule_refs"]}),
    )
}

fn snapshot(batch: &Value) -> Api<Value> {
    let items = batch["items"]
        .as_array()
        .ok_or_else(|| Error::invalid("connector batch items are missing"))?;
    let mut result = serde_json::Map::new();
    for (index, item) in items.iter().enumerate() {
        let key = item["input_key"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| index.to_string());
        result.insert(
            key,
            item.get("mapped")
                .cloned()
                .unwrap_or_else(|| item["raw"].clone()),
        );
    }
    Ok(Value::Object(result))
}

fn definition_signature(definition: &Value) -> String {
    digest(
        &json!({"chain_type":definition["chain_type"],"query_ref":definition["query_ref"],
        "source":definition["source"],"authorization_ref":definition["authorization_ref"],
        "tool_config_ref":definition["tool_config_ref"],"target_selection":definition["target_selection"],
        "denoise":definition["baseline_policy"]["denoise"]}),
    )
}

fn coverage_signature(definition: &Value) -> String {
    let selection = &definition["target_selection"];
    if selection.as_object().is_some_and(|value| !value.is_empty()) {
        digest(&json!({"chain_type":definition["chain_type"],"target_selection":selection}))
    } else {
        "asset-selection".to_owned()
    }
}

fn denoise(value: &Value, rules: &Value, path: &str,ignored:&mut Vec<String>) -> Value {
    let ignored_fields = rules["ignore_fields"].as_array();
    let ignored_keys = rules["ignore_keys"].as_array();
    match value {
        Value::Object(values) => {
            let mut output = serde_json::Map::new();
            for (key, child) in values {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if ignored_fields.is_some_and(|rows| rows.contains(&json!(child_path)))
                    || ignored_keys.is_some_and(|rows| rows.contains(&json!(key)))
                {
                    ignored.push(if ignored_fields.is_some_and(|rows|rows.contains(&json!(child_path))){child_path.clone()}else{key.clone()});continue;
                }
                output.insert(key.clone(), denoise(child, rules, &child_path,ignored));
            }
            Value::Object(output)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(index, child)| denoise(child, rules, &format!("{path}.{index}"),ignored))
                .collect(),
        ),
        Value::String(value) => {
            let mut result = value.clone();
            for pattern in rules["ignore_patterns"].as_array().into_iter().flatten() {
                if pattern["path"] != path {
                    continue;
                }
                if let Some(source) = pattern["pattern"].as_str() {
                    if let Ok(regex) = Regex::new(source) {
                        let previous=result.clone();
                        result = regex
                            .replace_all(
                                &result,
                                pattern["replacement"].as_str().unwrap_or("<ignored>"),
                            )
                            .into_owned();
                        if previous!=result{ignored.push(path.to_owned());}
                    }
                }
            }
            json!(result)
        }
        _ => value.clone(),
    }
}

fn diff(before: &Value, after: &Value, path: &str, output: &mut Vec<Value>) {
    if before == after {
        return;
    }
    if let (Some(before), Some(after)) = (before.as_object(), after.as_object()) {
        let keys = before
            .keys()
            .chain(after.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for key in keys {
            let child_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            match (before.get(&key), after.get(&key)) {
                (Some(left), Some(right)) => diff(left, right, &child_path, output),
                (Some(left), None) => output.push(
                    json!({"path":child_path,"before":left,"after":null,"operation":"removed"}),
                ),
                (None, Some(right)) => output.push(
                    json!({"path":child_path,"before":null,"after":right,"operation":"added"}),
                ),
                _ => {}
            }
        }
    } else {
        output.push(json!({"path":path,"before":before,"after":after}));
    }
}

fn queue_change_notice(
    s: &AppState,
    id: &str,
    monitor_id: &str,
    definition: &Value,
    change_id: &str,
    candidate_id: &str,
    key: &str,
) -> Api<()> {
    if definition["notification"]["enabled"] == false {
        return Ok(());
    }
    let window = definition["notification"]["merge_window_seconds"]
        .as_i64()
        .unwrap_or(0)
        .max(0);
    if let Some(row) = records(s, id, "monitor_notification")?
        .into_iter()
        .rev()
        .find(|row| {
            row["payload"]["monitor_id"] == monitor_id
                && row["payload"]["root_object_key"] == key
                && row["payload"]["status"] == "pending"
                && row["payload"]["window_start"]
                    .as_str()
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .is_some_and(|started| {
                        Utc::now().signed_duration_since(started).num_seconds() <= window
                    })
        })
    {
        let mut notice = row["payload"].clone();
        if !notice["change_ids"]
            .as_array()
            .is_some_and(|rows| rows.contains(&json!(change_id)))
        {
            notice["change_ids"]
                .as_array_mut()
                .unwrap()
                .push(json!(change_id));
            notice["candidate_ids"]
                .as_array_mut()
                .unwrap()
                .push(json!(candidate_id));
            notice["window_end"] = json!((Utc::now() + Duration::seconds(window)).to_rfc3339());
            save_rec(
                s,
                id,
                "monitor_notification",
                row["natural_key"].as_str().unwrap_or(""),
                notice,
                row["revision"].as_u64(),
            )?;
        }
        return Ok(());
    }
    let notification_id = stable("smnotice_", &format!("{id}:{monitor_id}:{change_id}"));
    record_once(
        s,
        id,
        "monitor_notification",
        &notification_id,
        json!({
            "notification_id":notification_id,"task_id":id,"monitor_id":monitor_id,
            "root_object_key":key,"kind":"meaningful_change","change_ids":[change_id],
            "candidate_ids":[candidate_id],"window_start":now(),
            "window_end":(Utc::now()+Duration::seconds(window)).to_rfc3339(),
            "status":"pending","channel":if definition["notification"]["external_enabled"]==true{"external"}else{"task"},
            "external_channel":if definition["notification"]["external_enabled"]==true{definition["notification"]["external_channel"].clone()}else{json!("")},
            "created_at":now(),"delivered_at":null
        }),
    )?;
    Ok(())
}

fn record_change(
    batch: &mut MonitorBatch<'_>,
    id: &str,
    monitor_id: &str,
    cycle_id: &str,
    definition: &Value,
    key: &str,
    kind: &str,
    before: &Value,
    after: &Value,
    old_ref: &Value,
    new_ref: &Value,
) -> Api<String> {
    let mut raw_diff = Vec::new();
    diff(before, after, "", &mut raw_diff);
    let rules = &definition["baseline_policy"]["denoise"];
    let mut before_ignored=Vec::new();let mut after_ignored=Vec::new();
    let before_denoised=denoise(before,rules,"",&mut before_ignored);
    let after_denoised=denoise(after,rules,"",&mut after_ignored);
    before_ignored.sort();before_ignored.dedup();after_ignored.sort();after_ignored.dedup();
    let mut denoised_diff = Vec::new();
    diff(&before_denoised, &after_denoised, "", &mut denoised_diff);
    let meaningful = kind != "changed" || !denoised_diff.is_empty();
    let change_id = stable(
        "smchange_",
        &format!("{id}:{monitor_id}:{cycle_id}:{key}:{kind}"),
    );
    let candidate_id = stable(
        "smcandidate_",
        &format!("{id}:{monitor_id}:{cycle_id}:{key}"),
    );
    if meaningful {
        batch.once("monitor_candidate", &candidate_id, json!({
            "candidate_id":candidate_id,"task_id":id,"monitor_id":monitor_id,"cycle_id":cycle_id,
            "change_id":change_id,"projection_kind":"observation",
            "question":format!("核对 {key} 的变化是否改变当前研究前提"),
            "related_objects":[key],"evidence_refs":[change_id],"missing_conditions":[],
            "suggested_action":"review_evidence","status":"pending","created_at":now()
        }))?;
    }
    batch.once("monitor_change", &change_id, json!({
        "change_id":change_id,"task_id":id,"monitor_id":monitor_id,"cycle_id":cycle_id,
        "chain_type":definition["chain_type"].as_str().unwrap_or("asset"),"root_object_key":key,"kind":kind,"change_type":kind,
        "before":before,"after":after,"before_artifact":old_ref,"after_artifact":new_ref,
        "raw_diff":raw_diff,"denoised_diff":denoised_diff,
        "denoise":{"before":{"version":rules["version"].as_str().unwrap_or("v1"),"rules":rules,"ignored":before_ignored},"after":{"version":rules["version"].as_str().unwrap_or("v1"),"rules":rules,"ignored":after_ignored}},
        "meaningful":meaningful,"comparable":true,"candidate_id":if meaningful{json!(candidate_id)}else{Value::Null},
        "detected_at":now(),"created_at":now()
    }))?;
    if meaningful {
        batch.notice_change(&change_id, &candidate_id, key);
    }
    Ok(change_id)
}

fn record_incomparable(
    batch: &mut MonitorBatch<'_>,
    id: &str,
    monitor_id: &str,
    cycle_id: &str,
    definition: &Value,
    root: &str,
    kind: &str,
    before: &Value,
    after: &Value,
    gap: &str,
) -> Api<String> {
    let change_id = stable(
        "smchange_",
        &format!("{id}:{monitor_id}:{cycle_id}:{root}:{kind}"),
    );
    let candidate_id = stable(
        "smcandidate_",
        &format!("{id}:{monitor_id}:{cycle_id}:{root}"),
    );
    batch.once(
        "monitor_candidate",
        &candidate_id,
        json!({
            "candidate_id":candidate_id,"task_id":id,"monitor_id":monitor_id,"cycle_id":cycle_id,
            "change_id":change_id,"projection_kind":"observation",
            "question":format!("核对 {root} 的变化是否改变当前研究前提"),
            "related_objects":[root],"evidence_refs":[change_id],"missing_conditions":[gap],
            "suggested_action":"review_evidence","status":"pending","created_at":now()
        }),
    )?;
    batch.once("monitor_change", &change_id, json!({
        "change_id":change_id,"task_id":id,"monitor_id":monitor_id,"cycle_id":cycle_id,
        "chain_type":definition["chain_type"],"root_object_key":root,"kind":kind,"change_type":kind,
        "before":before,"after":after,"comparable":false,"meaningful":true,
        "gaps":[gap],"candidate_id":candidate_id,"detected_at":now(),"created_at":now()
    }))?;
    batch.notice_change(&change_id, &candidate_id, root);
    Ok(change_id)
}

fn missing_cycles(
    s: &AppState,
    id: &str,
    monitor_id: &str,
    previous_cycle_id: &str,
    cycle_id: &str,
) -> Api<Vec<Value>> {
    let mut seen_previous = false;
    let mut missing = Vec::new();
    for row in records(s, id, "monitor_cycle")? {
        if row["payload"]["monitor_id"] != monitor_id {
            continue;
        }
        let current = row["payload"]["cycle_id"].as_str().unwrap_or("");
        if current == previous_cycle_id {
            seen_previous = true;
            continue;
        }
        if current == cycle_id {
            break;
        }
        if seen_previous {
            missing.push(json!({"cycle_id":current,"status":row["payload"]["status"],
                "completeness":row["payload"]["completeness"]}));
        }
    }
    Ok(missing)
}

pub(super) fn finish(
    s: &AppState,
    id: &str,
    monitor_id: &str,
    cycle_id: &str,
    batch: &Value,
    preflight: &Value,
) -> Api<Value> {
    if batch["schema"] != "muteki.src.connector-batch.v1"
        || batch["task_id"] != id
        || batch["connector_id"] != preflight["connector_id"]
        || batch["config_version"] != preflight["config_version"]
    {
        return Err(Error::conflict("monitor connector batch binding changed"));
    }
    let refs = batch["source_refs"]
        .as_array()
        .ok_or_else(|| Error::invalid("monitor source refs missing"))?;
    for reference in refs {
        verify_ref(s, id, reference)?;
    }
    let current = monitor_row(s, id, monitor_id)?;
    let mut monitor = current["payload"].clone();
    let version = rec(s, id, "monitor_cycle", cycle_id)?["payload"]["definition_version"]
        .as_u64()
        .unwrap_or_else(|| monitor["version"].as_u64().unwrap_or(1));
    let source_definition = cycle_definition(s, id, monitor_id, version, &monitor)?;
    if monitor["status"] != "active"
        || rec(s, id, "monitor_cycle", cycle_id)?["payload"]["cancel_requested"] == true
    {
        let calls = batch["acquisition"]["pages"].as_u64().unwrap_or(0);
        let cycle_record = rec(s, id, "monitor_cycle", cycle_id)?;
        let mut cycle = cycle_record["payload"].clone();
        cycle["status"] = json!("cancelled");
        cycle["completeness"] = json!("unavailable");
        cycle["end_reason"] = json!("cancelled_by_user");
        cycle["external_state"] = json!("completed");
        cycle["dispatch_state"] = json!("finished");
        cycle["source_refs"] = json!(refs);
        cycle["finished_at"] = json!(now());
        cycle["cost"]["calls"] = json!(calls);
        cycle["cost"]["actual"] = json!(batch["cost"]["actual"].as_f64().unwrap_or(0.0));
        cycle["cost"]["unknown"] = json!(batch["cost"]["actual"].is_null());
        monitor["current_cycle_id"] = Value::Null;
        monitor["last_cycle_id"] = json!(cycle_id);
        monitor["next_wake_at"] = Value::Null;
        monitor["calls_used"] = json!(monitor["calls_used"].as_u64().unwrap_or(0) + calls);
        let notice = cycle_notice_payload(s, id, monitor_id, &cycle)?;
        let mut atomic = MonitorBatch::new(s, id);
        atomic.update(
            "monitor",
            monitor_id,
            monitor,
            current["revision"].as_u64().unwrap_or(0),
        );
        atomic.update(
            "monitor_cycle",
            cycle_id,
            cycle.clone(),
            cycle_record["revision"].as_u64().unwrap_or(0),
        );
        if let Some((key, payload)) = notice {
            atomic.once("monitor_notification", &key, payload)?;
        }
        atomic.commit()?;
        return Ok(cycle);
    }
    let snapshot = snapshot(batch)?;
    let snapshot_bytes = serde_json::to_vec(&snapshot).map_err(crate::runs::src_api::db)?;
    let raw_ref = super::super::put_artifact(
        s,
        id,
        &snapshot_bytes,
        &format!("{cycle_id}-source.json"),
        "application/json",
    )?;
    let receipt_id = stable(
        "sreceipt_",
        &format!("{id}:{cycle_id}:{}", batch["batch_id"]),
    );
    let complete =
        batch["external_state"] == "completed" && batch["acquisition"]["complete"] == true;
    let completeness = if complete {
        "complete"
    } else if !snapshot.as_object().unwrap().is_empty() {
        "partial"
    } else {
        "unavailable"
    };
    let calls = batch["acquisition"]["pages"].as_u64().unwrap_or(0);
    let actual = batch["cost"]["actual"].as_f64().unwrap_or(0.0);
    if actual < 0.0 || !actual.is_finite() {
        return Err(Error::invalid("monitor source cost is invalid"));
    }
    let unknown = batch["cost"]["actual"].is_null() || batch["cost"]["unknown"] == true;
    let watermarks =
        json!({"batch_id":batch["batch_id"],"next_token":batch["acquisition"]["next_token"]});
    let mut atomic = MonitorBatch::new(s, id);
    let source_receipt = atomic.once("monitor_source_receipt", &receipt_id, json!({
        "receipt_id":receipt_id,"task_id":id,"cycle_id":cycle_id,"source":source_definition["source"],
        "raw_artifact":raw_ref,"source_receipt":{"batch_id":batch["batch_id"],
            "source_refs":refs,"external_state":batch["external_state"],"cost":batch["cost"]},
        "source_watermarks":watermarks,"complete":complete,"created_at":now()
    }))?;
    atomic.once("monitor_cost", &format!("{cycle_id}:actual"), json!({
        "ledger_id":stable("scost_", &format!("{id}:{cycle_id}:actual")),
        "task_id":id,"cycle_id":cycle_id,"kind":"actual","amount":actual,
        "unknown":unknown,"detail":{"source_receipt":receipt_id,"calls":calls},"created_at":now()
    }))?;
    let items = batch["items"].as_array().unwrap();
    for (index, item) in items.iter().enumerate() {
        let key = item["input_key"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| index.to_string());
        let observation_id = stable("smob_", &format!("{id}:{cycle_id}:{key}"));
        atomic.once(
            "monitor_observation",
            &observation_id,
            json!({
                "observation_id":observation_id,"task_id":id,"cycle_id":cycle_id,"key":key,
                "observed_at":null,"ingested_at":now(),"attributes":snapshot[&key],
                "raw_values":item["raw"],"relations":[],"source_artifact":raw_ref,
                "completeness":if complete{"complete"}else{"partial"},"success":true
            }),
        )?;
    }
    let definition = source_definition.clone();
    let limit = definition["budget"]["call_limit"]
        .as_u64()
        .filter(|v| *v > 0)
        .or_else(|| definition["budget"]["per_cycle"].as_u64().filter(|v| *v > 0))
        .or_else(|| {
            definition["budget"]["per_cycle"]
                .as_f64()
                .filter(|v| *v > 0.0)
                .map(|v| v.ceil() as u64)
        })
        .unwrap_or(0);
    let reserved = rec(s, id, "monitor_cycle", cycle_id)?["payload"]["cost"]["reserved"]
        .as_f64()
        .unwrap_or(0.0);
    let over_budget = calls > limit || !unknown && actual > reserved;
    let errors = batch["errors"].as_array().cloned().unwrap_or_default();
    let status = if over_budget {
        "budget_exhausted"
    } else if complete && errors.is_empty() {
        "completed"
    } else if completeness == "partial" {
        "partial"
    } else {
        "failed"
    };
    let end_reason = if over_budget {
        "budget_exhausted"
    } else if unknown && calls > 0 {
        "cost_unknown"
    } else if status == "completed" {
        ""
    } else {
        "source_incomplete"
    };
    let source_signature = definition_signature(&definition);
    let coverage = coverage_signature(&definition);
    let previous = monitor["baseline"].clone();
    let mut baseline_id = Value::Null;
    let mut compared_baseline_id = Value::Null;
    let mut comparison = json!({});
    if status == "completed" {
        if !previous.is_null() {
            compared_baseline_id = previous["baseline_id"].clone();
        }
        let definition_changed = !previous.is_null()
            && previous["definition_signature"]
                .as_str()
                .or_else(|| previous["source_signature"].as_str())
                != Some(source_signature.as_str());
        let coverage_changed = !previous.is_null()
            && previous["coverage_signature"]
                .as_str()
                .is_some_and(|value| value != coverage);
        if definition_changed
            && definition["baseline_policy"]["rebase_on_definition_change"] == false
        {
            let change_id = record_incomparable(
                &mut atomic,
                id,
                monitor_id,
                cycle_id,
                &definition,
                "monitor-definition",
                "incomparable",
                &previous,
                &json!({"definition_version":version}),
                "查询、来源、权限、采样或提取规则已变化，不能与旧基线直接比较",
            )?;
            comparison = json!({"comparable":false,"reason":"definition_changed",
                "baseline_id":null,"compared_baseline_id":compared_baseline_id,"changes":[change_id]});
        } else if !definition_changed && coverage_changed {
            let change_id = record_incomparable(
                &mut atomic,
                id,
                monitor_id,
                cycle_id,
                &definition,
                "coverage",
                "coverage_changed",
                &previous["coverage_signature"],
                &json!(coverage),
                "覆盖对象集合变化；新增覆盖对象不能称为刚上线，也不能据旧集合报告删除",
            )?;
            comparison = json!({"comparable":false,"reason":"coverage_changed",
                "baseline_id":null,"compared_baseline_id":compared_baseline_id,"changes":[change_id]});
        } else {
            let comparable = !definition_changed;
            let mut changes = Vec::new();
            if comparable && !previous.is_null() {
                let before = previous["snapshot"]
                    .as_object()
                    .ok_or_else(|| Error::invalid("previous monitor baseline snapshot invalid"))?;
                let after = snapshot.as_object().unwrap();
                let keys = before
                    .keys()
                    .chain(after.keys())
                    .cloned()
                    .collect::<BTreeSet<_>>();
                for key in keys {
                    let old = before.get(&key).cloned().unwrap_or(Value::Null);
                    let new = after.get(&key).cloned().unwrap_or(Value::Null);
                    if old == new {
                        continue;
                    }
                    let kind = if !before.contains_key(&key) {
                        "added"
                    } else if !after.contains_key(&key) {
                        "removed"
                    } else {
                        "changed"
                    };
                    changes.push(record_change(
                        &mut atomic,
                        id,
                        monitor_id,
                        cycle_id,
                        &definition,
                        &key,
                        kind,
                        &old,
                        &new,
                        &previous["raw_artifact"],
                        &raw_ref,
                    )?);
                }
            }
            let next_baseline_id = stable("smbase_", &format!("{id}:{monitor_id}:{cycle_id}"));
            let denoised = denoise(&snapshot, &definition["baseline_policy"]["denoise"], "", &mut Vec::new());
            let denoised_bytes = serde_json::to_vec(&denoised).map_err(crate::runs::src_api::db)?;
            let denoised_ref = super::super::put_artifact(
                s,
                id,
                &denoised_bytes,
                &format!("{cycle_id}-denoised.json"),
                "application/json",
            )?;
            let baseline_reason = if previous.is_null() {
                "first_complete_cycle"
            } else if definition_changed {
                "definition_changed_new_baseline"
            } else {
                "next_complete_cycle"
            };
            let baseline = atomic.once("monitor_baseline", &next_baseline_id, json!({
                "baseline_id":next_baseline_id,"task_id":id,"monitor_id":monitor_id,"cycle_id":cycle_id,
                "definition_version":version,"created_at":now(),"reason":baseline_reason,
                "raw_artifact":raw_ref,"denoised_artifact":denoised_ref,"snapshot":snapshot,
                "definition_signature":source_signature,"source_signature":source_signature,
                "coverage_signature":coverage,"snapshot_digest":digest(&snapshot),
                "source_refs":refs,"completeness":"complete"
            }))?;
            baseline_id = baseline["baseline_id"].clone();
            monitor["baseline"] = baseline;
            let reason = if previous.is_null() {
                "first_complete_cycle"
            } else if definition_changed {
                "definition_changed_new_baseline"
            } else {
                "compared_last_valid_baseline"
            };
            let gaps = if previous.is_null() || definition_changed {
                Vec::new()
            } else {
                missing_cycles(
                    s,
                    id,
                    monitor_id,
                    previous["cycle_id"].as_str().unwrap_or(""),
                    cycle_id,
                )?
            };
            comparison = json!({"comparable":comparable,"reason":reason,"baseline_id":baseline_id,
                "compared_baseline_id":compared_baseline_id,"changes":changes,"missing_cycles":gaps});
        }
    }
    let total = definition["budget"]["total"].as_f64().unwrap_or(0.0);
    let cost_used = monitor["cost_used"].as_f64().unwrap_or(0.0) + actual;
    let blocked = if end_reason == "cost_unknown" {
        "cost_unknown"
    } else if over_budget || total > 0.0 && cost_used >= total {
        "budget_exhausted"
    } else {
        ""
    };
    monitor["current_cycle_id"] = Value::Null;
    monitor["last_cycle_id"] = json!(cycle_id);
    monitor["calls_used"] = json!(monitor["calls_used"].as_u64().unwrap_or(0) + calls);
    monitor["cost_used"] = json!(cost_used);
    monitor["cost_unknown"] = json!(unknown || monitor["cost_unknown"] == true);
    monitor["blocked_reason"] = json!(blocked);
    monitor["next_wake_at"] = if blocked.is_empty() {
        json!(
            (Utc::now()
                + Duration::seconds(
                    definition["schedule"]["interval_seconds"]
                        .as_i64()
                        .unwrap_or(1)
                        .max(1)
                ))
            .to_rfc3339()
        )
    } else {
        Value::Null
    };
    if status == "completed" {
        monitor["last_success_at"] = json!(now());
    }
    monitor["updated_at"] = json!(now());
    let mut cycle = rec(s, id, "monitor_cycle", cycle_id)?["payload"].clone();
    cycle["status"] = json!(status);
    cycle["completeness"] = json!(completeness);
    cycle["end_reason"] = json!(end_reason);
    cycle["error"] = json!(if errors.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&errors).unwrap_or_default()
    });
    cycle["finished_at"] = json!(now());
    cycle["dispatch_state"] = json!("finished");
    cycle["external_state"] = json!("completed");
    cycle["source_refs"] = json!(refs);
    cycle["source_receipts"] = json!([source_receipt["receipt_id"]]);
    cycle["source_watermarks"] = watermarks;
    cycle["observation_count"] = json!(items.len());
    cycle["cost"] = json!({"reserved":reserved,"actual":actual,"unknown":unknown,"calls":calls,
        "currency":definition["budget"]["currency"].as_str().unwrap_or("USD")});
    cycle["revalidation"] = json!({"source_available":true,"permission_valid":true,
        "plan_valid":true,"authorization_valid":true,"plan_ref":preflight["plan_ref"],
        "authorization_ref":preflight["authorization_ref"],"config_version":preflight["config_version"]});
    cycle["baseline_id"] = baseline_id;
    cycle["compared_baseline_id"] = compared_baseline_id;
    cycle["comparison"] = comparison;
    let cycle_revision = rec(s, id, "monitor_cycle", cycle_id)?["revision"]
        .as_u64()
        .unwrap_or(0);
    atomic.update(
        "monitor",
        monitor_id,
        monitor,
        current["revision"].as_u64().unwrap_or(0),
    );
    atomic.update("monitor_cycle", cycle_id, cycle.clone(), cycle_revision);
    if let Some((notice_id, notice)) = cycle_notice_payload(s, id, monitor_id, &cycle)? {
        atomic.once("monitor_notification", &notice_id, notice)?;
    }
    let notices = atomic.commit()?;
    for (change_id, candidate_id, key) in notices {
        queue_change_notice(
            s,
            id,
            monitor_id,
            &definition,
            &change_id,
            &candidate_id,
            &key,
        )?;
    }
    Ok(cycle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_query_requires_the_matching_explicit_allow_rule() {
        let facts = json!({"task_id":"src-test","tool_name":"src.connector.query",
            "resource_kind":"control","action":"query","connector_id":"aws_ec2",
            "config_version":1,"query":"region=us-east-1"});
        let mut policy = json!({"defaults":{"effect":"allow"},"rules":[{
            "rule_id":"allow-ec2","version":1,"enabled":true,"mode":"enforce",
            "resource_kind":"control","evaluation_point":"tool","actions":["query"],
            "selector":{"connector_id":{"eq":"aws_ec2"}},"conditions":{},"effect":"allow"
        }]});
        assert_eq!(
            authorize_control(&policy, &facts).unwrap()["matched_rule_refs"],
            json!(["allow-ec2:v1"])
        );
        let mut wrong = facts.clone();
        wrong["connector_id"] = json!("shodan");
        assert_eq!(
            authorize_control(&policy, &wrong).unwrap_err().code,
            "authorization_unresolved"
        );
        policy["rules"][0]["effect"] = json!("deny");
        assert_eq!(
            authorize_control(&policy, &facts).unwrap_err().code,
            "policy_denied"
        );
    }
}
