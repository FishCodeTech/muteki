//! Source-shaped research backlog and chronological history projections.
use super::*;

fn all_records(s: &AppState, task_id: &str) -> Api<Vec<Value>> {
    task(s, task_id)?;
    Ok(s.store
        .list_inserted(RECORDS)
        .map_err(db)?
        .into_iter()
        .map(|stored| stored["values"].clone())
        .filter(|row| row["task_id"] == task_id)
        .collect())
}

fn payload_view(record: &Value) -> Value {
    let mut result = json!({
        "record_id":record["record_id"], "kind":record["kind"],
        "revision":record["revision"], "parent_id":record["parent_id"],
        "version":record["payload"].get("version").unwrap_or(&record["revision"]),
    });
    if let (Some(result), Some(payload)) = (result.as_object_mut(), record["payload"].as_object()) {
        result.extend(payload.clone());
    }
    result
}

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

pub(super) fn backlog(s: &AppState, task_id: &str) -> Api<Value> {
    let all = all_records(s, task_id)?;
    let mut result = Vec::new();
    for question in all.iter().filter(|row| row["kind"] == "research_question") {
        let refs = question["payload"]["evidence_refs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let related: Vec<&Value> = all
            .iter()
            .filter(|row| {
                refs.iter().any(|reference| {
                    row["record_id"] == *reference || row["parent_id"] == *reference
                })
            })
            .collect();
        let has_kind = |kinds: &[&str]| {
            related.iter().any(|row| {
                row["kind"]
                    .as_str()
                    .is_some_and(|kind| kinds.contains(&kind))
            })
        };
        let state = if has_kind(&["review", "review_record"]) {
            "reviewed"
        } else if has_kind(&["finding", "report"]) {
            "finding"
        } else if has_kind(&["dead_end", "deadend"]) {
            "excluded"
        } else if truthy(&question["payload"]["missing_comparisons"]) {
            "blocked"
        } else if has_kind(&[
            "step",
            "tool_call",
            "exchange",
            "http_exchange",
            "experiment",
        ]) {
            "attempted"
        } else if has_kind(&[
            "observation",
            "runtime_observation",
            "exchange",
            "http_exchange",
        ]) {
            "observed"
        } else {
            "pending"
        };
        let mut item = payload_view(question);
        item["derived_state"] = json!(state);
        item["state_evidence_refs"] = json!(
            related
                .iter()
                .map(|row| row["record_id"].clone())
                .collect::<Vec<_>>()
        );
        item["asset_fully_tested"] = json!(false);
        result.push(item);
    }
    Ok(json!(result))
}

fn number(params: &Value, field: &str, default: i64) -> Api<i64> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(value)) => value
            .as_i64()
            .ok_or_else(|| Error::invalid(format!("{field} must be an integer"))),
        Some(Value::String(value)) => value
            .parse::<i64>()
            .map_err(|_| Error::invalid(format!("{field} must be an integer"))),
        _ => Err(Error::invalid(format!("{field} must be an integer"))),
    }
}

fn observed_at(row: &Value) -> &str {
    row["observed_at"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| row["source"]["occurred_at"].as_str())
        .unwrap_or("")
}

pub(super) fn history(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let run_id = params["run_id"].as_str().filter(|value| !value.is_empty());
    let asset_id = params["asset_id"]
        .as_str()
        .filter(|value| !value.is_empty());
    let mut rows = Vec::new();
    for record in all_records(s, task_id)? {
        let relevant = matches!(
            record["kind"].as_str().unwrap_or(""),
            "step"
                | "observation"
                | "dead_end"
                | "fact"
                | "tool_call"
                | "run_error"
                | "report_version"
                | "runtime_observation"
                | "page_action"
                | "experiment"
                | "dataflow"
                | "http_exchange"
                | "exchange"
        );
        if !relevant {
            continue;
        }
        if let Some(run_id) = run_id {
            if record["parent_id"] != run_id && record["payload"]["source_run_id"] != run_id {
                continue;
            }
        }
        if let Some(asset_id) = asset_id {
            let payload = &record["payload"];
            let referenced = payload["asset_id"] == asset_id
                || payload["object_refs"]["asset_id"] == asset_id
                || payload["asset_ids"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item == asset_id));
            if !referenced && !payload.to_string().contains(asset_id) {
                continue;
            }
        }
        rows.push(payload_view(&record));
    }
    rows.sort_by(|left, right| {
        observed_at(left)
            .cmp(observed_at(right))
            .then(left["record_id"].as_str().cmp(&right["record_id"].as_str()))
    });
    let start = number(params, "offset", 0)?.max(0) as usize;
    let limit = number(params, "limit", 100)?.max(1) as usize;
    let total = rows.len();
    let selected: Vec<_> = rows.into_iter().skip(start).take(limit).collect();
    let next_offset = if start + selected.len() < total {
        Some(start + selected.len())
    } else {
        None
    };
    Ok(
        json!({"items":selected,"total":total,"returned":selected.len(),
        "next_offset":next_offset,"complete":next_offset.is_none(),
        "watermarks":super::super::source_watermarks(s, task_id)?,
        "cross_run_facts_are_local":false}),
    )
}
