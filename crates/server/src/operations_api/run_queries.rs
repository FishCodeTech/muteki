//! Run read queries for the shared Command/Query API.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::collections::HashSet;

pub(super) fn shared_graph(state: &AppState, run_id: &str, params: &Value) -> Result<Value,Failure> {
    let unavailable = ||fail(StatusCode::NOT_FOUND,"graph.unavailable",format!("no shared graph available for run {run_id:?}"));
    if run_id.trim().is_empty() { return Err(fail(StatusCode::BAD_REQUEST,"graph.run_id_required","graph.shared.read requires params.run_id")); }
    let run = state.store.get("runs",run_id).map_err(storage)?.ok_or_else(unavailable)?;
    let path = crate::runs::graph::graph_db_path(state,run_id);
    if !path.is_file() {
        let run = crate::runs::project_run_summary(state,run);
        if !run["started"].as_bool().unwrap_or(false) || run["finished"].as_bool().unwrap_or(false) { return Err(unavailable()); }
    }
    drop(crate::runs::graph::SharedGraph::open(state,run_id).map_err(storage)?);
    let mut snapshot = crate::runs::graph::read_snapshot(&path,run_id).map_err(storage)?;
    let mut available: Vec<String> = snapshot["state"].as_object().into_iter().flatten().map(|(key,_)|key.clone()).collect();
    available.sort();
    if let Some(sections) = params.get("sections").filter(|value|!value.is_null()) {
        let selected = sections.as_array().filter(|rows|!rows.is_empty()).filter(|rows|
            rows.iter().all(|value|value.as_str().is_some_and(|name|available.iter().any(|key|key==name))));
        let Some(selected) = selected else {
            let mut error = fail(StatusCode::BAD_REQUEST,"graph.sections_invalid","sections 必须是 available_sections 中的区块名称列表");
            error.1.0["error"]["recovery_hint"] = json!("从 detail.available_sections 选择需要的区块。");
            error.1.0["error"]["detail"] = json!({"available_sections":available});
            return Err(error);
        };
        snapshot["state"].as_object_mut().unwrap().retain(|key,_|selected.iter().any(|section|section.as_str()==Some(key)));
    }
    snapshot["available_sections"] = json!(available);
    Ok(snapshot)
}

fn text<'a>(row: &'a Value, key: &str) -> &'a str {
    row[key].as_str().unwrap_or("").trim()
}

pub(super) fn summary(run: &Value) -> Value {
    let status = text(run, "status");
    let name = if status == "draft"
        && run["custom_name"].is_null()
        && run["original_name"] == run["run_id"]
    {
        ""
    } else {
        run["custom_name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .or_else(|| run["name"].as_str())
            .unwrap_or("")
    };
    json!({
        "run_id":run["run_id"],
        "name":name,
        "category":run["category"].as_str().unwrap_or(""),
        "mode":run["mode"],
        "started":run["started"].as_bool().unwrap_or(status != "draft"),
        "finished":run["finished"].as_bool().unwrap_or(matches!(status,"solved" | "finished" | "failed")),
        "solved":run["solved"].as_bool().unwrap_or(false),
        "paused":run["paused"].as_bool().unwrap_or(status == "paused"),
        "awaiting_help":run["awaiting_help"].as_bool().unwrap_or(false),
        "help_text":run["help_text"],
        "runtime_incomplete":run["runtime_incomplete"].as_bool().unwrap_or(false),
        "runtime_error":run["runtime_error"],
        "status":status,
        "flag":run["flag"],
        "flags":run["flags"],
        "expected_flags":run["expected_flags"],
        "multi_flag":run["multi_flag"],
        "pinned":run["pinned"],
        "pinned_at":run["pinned_at"],
        "archived":run["archived"],
        "folder_id":run["folder_id"],
        "order":if run["order"].is_null(){&run["created_seq"]}else{&run["order"]},
        "updated":run["updated"],
        "updated_at":if run["updated_at"].is_null(){&run["updated"]}else{&run["updated_at"]},
    })
}

fn page_hash(value: &Value) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(value).unwrap_or_default()))
}

fn page_error(code: &'static str, message: &'static str) -> Failure {
    fail(StatusCode::BAD_REQUEST, code, message)
}

pub(super) fn list(state: &AppState, params: &Value, principal: &str, actor: &Value) -> Result<Value, Failure> {
    let include_archived = match &params["include_archived"] {
        Value::Bool(value) => *value,
        Value::String(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        _ => true,
    };
    let mut rows = crate::runs::list_runs(state, include_archived)
        .map_err(|error| {
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage.unavailable",
                error,
            )
        })?
        .into_iter()
        .filter(|run| {
            run["started"]
                .as_bool()
                .unwrap_or(text(run, "status") != "draft")
        })
        .map(|run| summary(&run))
        .collect::<Vec<_>>();
    let category = text(params, "category");
    let status = text(params, "status");
    let search = caseless::default_case_fold_str(text(params, "query"));
    let solved = match &params["solved"] {
        Value::Bool(value) => Some(*value),
        Value::String(value) => Some(matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )),
        _ => None,
    };
    rows.retain(|row| {
        (category.is_empty() || text(row, "category") == category)
            && (status.is_empty() || text(row, "status") == status)
            && solved.is_none_or(|value| row["solved"].as_bool().unwrap_or(false) == value)
            && (search.is_empty()
                || caseless::default_case_fold_str(&format!(
                    "{} {} {}",
                    text(row, "run_id"),
                    text(row, "name"),
                    text(row, "category")
                )).contains(&search))
    });
    let total = rows.len();
    let limit = match &params["limit"] {
        Value::Bool(true) => Some(1),
        Value::Number(number) => number.as_i64()
            .or_else(||number.as_f64().map(|value|value.trunc() as i64)),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
        .filter(|value| *value != 0)
        .unwrap_or(500)
        .clamp(1, 2000) as usize;
    let ids: Vec<String> = rows.iter().map(|row| text(row, "run_id").to_owned()).collect();
    if ids.iter().any(|id|id.is_empty()) || ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        return Err(fail(StatusCode::INTERNAL_SERVER_ERROR,"query.page.identity_invalid","列表记录缺少唯一 ID"));
    }
    let _ = (principal, actor);
    let mut filters = params.as_object().cloned().unwrap_or_default();
    filters.remove("limit");
    filters.remove("cursor");
    let expected = json!({"query":"run.list","filters":page_hash(&Value::Object(filters))});
    let cursor = text(params, "cursor");
    let key = if !cursor.is_empty() || rows.len() > limit {
        Some(platform_cursor_key(state).map_err(|error|fail(StatusCode::INTERNAL_SERVER_ERROR,
            "query.cursor.key_unavailable",error))?)
    } else { None };
    let mut head = ids.first().cloned().unwrap_or_default();
    let mut start = 0;
    if !cursor.is_empty() {
        let parts: Vec<&str> = cursor.split('.').collect();
        if parts.len()!=3 || parts[0]!="qpg1" || parts[1].len()>8192 {
            return Err(page_error("query.cursor.invalid","列表游标格式或签名无效"));
        }
        let raw = URL_SAFE_NO_PAD.decode(parts[1]).map_err(|_|page_error(
            "query.cursor.invalid","列表游标格式或签名无效"))?;
        let signature = (0..parts[2].len()).step_by(2).map(|index|
            parts[2].get(index..index+2).and_then(|pair|u8::from_str_radix(pair,16).ok()))
            .collect::<Option<Vec<_>>>().ok_or_else(||page_error(
                "query.cursor.invalid","列表游标格式或签名无效"))?;
        let mut mac = Hmac::<Sha256>::new_from_slice(key.as_deref().unwrap_or_default())
            .map_err(|_|page_error("query.cursor.invalid","列表游标格式或签名无效"))?;
        mac.update(&raw);
        mac.verify_slice(&signature).map_err(|_|page_error(
            "query.cursor.invalid","列表游标格式或签名无效"))?;
        let data: Value = serde_json::from_slice(&raw).map_err(|_|page_error(
            "query.cursor.invalid","列表游标格式或签名无效"))?;
        if !data.is_object() || ["query","filters"].iter()
            .any(|field|data[*field]!=expected[*field]) {
            return Err(page_error("query.cursor.mismatch","列表游标属于不同的查询或筛选条件"));
        }
        head = text(&data,"head").to_owned();
        let after = text(&data,"after");
        let head_index = ids.iter().position(|id|id==&head);
        let after_index = ids.iter().position(|id|id.as_str()==after);
        let (Some(head_index),Some(after_index)) = (head_index,after_index) else {
            return Err(page_error("query.cursor.stale","列表发生变化，续读位置已失效"));
        };
        if after_index<head_index || data["order"]!=page_hash(&json!(ids[head_index..])) {
            return Err(page_error("query.cursor.stale","列表记录或顺序发生变化，请重新查询"));
        }
        start = after_index+1;
    }
    let selected: Vec<Value> = rows.into_iter().skip(start).take(limit).collect();
    let has_more = start + selected.len() < total;
    let next_cursor = if has_more {
        let head_index = ids.iter().position(|id|id==&head).unwrap_or(0);
        let mut data = expected.clone();
        data["head"] = json!(head);
        data["after"] = selected.last().map(|row|row["run_id"].clone()).unwrap_or(Value::Null);
        data["order"] = json!(page_hash(&json!(ids[head_index..])));
        let raw = serde_json::to_vec(&data).unwrap_or_default();
        let mut mac = Hmac::<Sha256>::new_from_slice(key.as_deref().unwrap_or_default())
            .map_err(|error|fail(StatusCode::INTERNAL_SERVER_ERROR,"query.cursor.key_unavailable",error.to_string()))?;
        mac.update(&raw);
        Some(format!("qpg1.{}.{:x}",URL_SAFE_NO_PAD.encode(&raw),mac.finalize().into_bytes()))
    } else { None };
    Ok(
        json!({"runs":selected,"total":total,"returned":selected.len(),
        "next_cursor":next_cursor,"has_more":has_more}),
    )
}

fn datetime(value: &Value) -> Value {
    let Some(seconds) = value.as_f64() else {
        return Value::Null;
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis((seconds * 1000.0) as i64)
        .map(|time| json!(time.to_rfc3339()))
        .unwrap_or(Value::Null)
}

pub(super) fn snapshot(state: &AppState, run_id: &str) -> Result<Value, Failure> {
    let run = state
        .store
        .get("runs", run_id)
        .map_err(storage)?
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "run.not_found",
                format!("unknown run {run_id:?}"),
            )
        })?;
    let run = crate::runs::project_run_summary(state, run);
    let bound_key = state.store.get("bound_run_ids", run_id).map_err(storage)?;
    let bound = bound_key
        .as_ref()
        .and_then(|row| row["binding_key"].as_str())
        .map(|key| state.store.get("bound_runs", key))
        .transpose()
        .map_err(storage)?
        .flatten();
    let src_binding = if bound.is_none() {
        state
            .store
            .list("src_live_records")
            .map_err(storage)?
            .into_iter()
            .find(|row| {
                row["values"]["kind"] == "run_binding"
                    && row["values"]["payload"]["run_id"] == run_id
            })
    } else {
        None
    };
    let competition_binding = if bound.is_none() && src_binding.is_none() {
        state
            .store
            .list("run_bindings")
            .map_err(storage)?
            .into_iter()
            .find(|row| row["run_id"] == run_id)
    } else {
        None
    };
    let binding = bound
        .as_ref()
        .or_else(|| src_binding.as_ref().map(|row| &row["values"]["payload"]))
        .or(competition_binding.as_ref());
    let task_id = binding
        .map(|row| row["task_id"].clone())
        .filter(|value| !value.is_null())
        .or_else(|| binding.map(|row| row["competition_challenge_id"].clone()))
        .unwrap_or(Value::Null);
    let competition_revision = competition_binding
        .as_ref()
        .and_then(|row| row["revision_id"].as_str())
        .map(|revision_id| state.store.get("competition_revisions_live", revision_id))
        .transpose()
        .map_err(storage)?
        .flatten();
    let task_revision = binding
        .map(|row| row["task_revision"].clone())
        .filter(|value| !value.is_null())
        .or_else(|| binding.map(|row| row["binding_revision"].clone()))
        .filter(|value| !value.is_null())
        .or_else(|| {
            competition_revision
                .as_ref()
                .map(|row| row["revision_seq"].clone())
        })
        .unwrap_or(Value::Null);
    let binding_key = bound
        .as_ref()
        .map(|row| row["binding_key"].clone())
        .or_else(|| {
            src_binding
                .as_ref()
                .map(|row| row["values"]["payload"]["binding_key"].clone())
        })
        .or_else(|| {
            competition_binding.as_ref().map(|row| {
                json!(format!(
                    "comp:{}:cch:{}:rev:{}",
                    text(row, "competition_id"),
                    text(row, "competition_challenge_id"),
                    text(row, "revision_id")
                ))
            })
        })
        .unwrap_or(Value::Null);
    Ok(json!({
        "run_id":run_id,"state":run["status"],"control_mode":run["control_mode"],"generation":run["execution_generation"].as_u64().unwrap_or(0),
        "task_kind":bound.as_ref().and_then(|row|row["task_kind"].as_str()).unwrap_or_else(||
            if src_binding.is_some(){"pentest.target"}else if competition_binding.is_some(){"ctf"}else{""}),
        "executor_id":binding.map(|row|row["executor_id"].clone()).unwrap_or(Value::Null),
        "created_at":binding.map(|row|datetime(&row["created_at"])).unwrap_or(Value::Null),
        "updated_at":datetime(&run["updated"]),
        "detail":{
            "summary":summary(&run),"control_generation":run["control_generation"].as_u64().unwrap_or(0),
            "binding_key":binding_key,
            "task_id":task_id,"task_revision":task_revision,
            "flag_progress":{"flags":run["flags"],"expected_flags":run["expected_flags"],"solved":run["solved"]}
        }
    }))
}
