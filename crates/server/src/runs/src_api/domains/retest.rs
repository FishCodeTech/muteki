//! Finding retests are bound to the frozen report and its original SRC Run.
use super::{
    Api, Error, RECORDS, db, in_policy, now, policy, rec, records, report, require,
    save_rec, stage_rec, task, verify_ref,
};
use crate::{AppState, runs};
use axum::{
    Json,
    body::to_bytes,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

fn request_fields(value: &Value) -> Value {
    json!({
        "report_id": value["report_id"],
        "finding_id": value["finding_id"],
        "repair_statement": value["repair_statement"],
        "expected_target_version": value["expected_target_version"],
        "authorization_ref": value["authorization_ref"],
        "requester": value["requester"].as_str().unwrap_or("local-operator"),
        "identity_refs": value["identity_refs"].as_array().cloned().unwrap_or_default(),
        "original_poc_refs": value["original_poc_refs"].as_array().cloned().unwrap_or_default(),
        "baseline_refs": value["baseline_refs"].as_array().cloned().unwrap_or_default(),
        "budget": value["budget"].as_object().cloned().unwrap_or_default(),
    })
}

fn auth_ref(p: &Value) -> String {
    format!(
        "{}:v{}",
        p["policy_id"].as_str().unwrap_or("src-policy"),
        p["version"].as_u64().unwrap_or(0)
    )
}
fn workbench_source(source: &str) -> bool {
    source.starts_with("workbench-")
}
fn task_authorizes_retest_target(s: &AppState, id: &str, target: &str) -> Api<()> {
    let t = task(s, id)?;
    let p = policy(s, id)?;
    if t["lifecycle"]["state"] != "active"
        || !in_policy(&p, target)
        || !t["input"]["authorization"]["targets"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|v| {
                    v.as_str().is_some_and(|allowed| {
                        crate::runs::src_api::target_matches(target, allowed)
                    })
                })
            })
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "original Run target is outside current Task scope",
        ));
    }
    Ok(())
}
fn original_run(s: &AppState, id: &str, source: &str, target: &str) -> Api<Value> {
    let binding = records(s, id, "run_binding")?
        .into_iter()
        .find(|v| v["payload"]["run_id"] == source)
        .ok_or_else(|| {
            Error::new(
                StatusCode::FORBIDDEN,
                "src.run_not_bound",
                "original Run is outside this SRC Task",
            )
        })?;
    if binding["payload"]["contract"]["target"] != target {
        return Err(Error::conflict(
            "report target differs from original Run contract",
        ));
    }
    let run = s
        .store
        .get("runs", source)
        .map_err(crate::runs::src_api::db)?
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.run_not_found",
                "original Run unavailable",
            )
        })?;
    if run["src_task_id"] != id
        || run["bound_to_src"] != true
        || run["mode"] != "pentest"
        || run["archived"] == true
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.run_not_bound",
            "original Run is not an active SRC Pentest binding",
        ));
    }
    task_authorizes_retest_target(s, id, target)?;
    Ok(run)
}
pub(super) fn request(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    require(body,"requester")?;
    for field in ["identity_refs","original_poc_refs","baseline_refs"]{if let Some(value)=body.get(field).filter(|value|!value.is_null()){if !value.as_array().is_some_and(|rows|rows.iter().all(Value::is_string)){return Err(Error::invalid(format!("{field} must be an array of identities")));}}}
    if body.get("budget").is_some_and(|value|!value.is_null() && !value.is_object()){return Err(Error::invalid("retest budget must be an object"));}
    let report_id = require(body, "report_id")?;
    let frozen = report(s, id, report_id)?;
    let finding = require(body, "finding_id")?;
    if frozen["finding_id"] != finding {
        return Err(Error::conflict("Finding differs from frozen report"));
    }
    let source = require(&frozen, "source_run_id")?;
    let target = require(&frozen["document"], "target")?;
    if workbench_source(source) {
        task_authorizes_retest_target(s, id, target)?;
    } else {
        original_run(s, id, source, target)?;
    }
    require(body, "repair_statement")?;
    require(body, "expected_target_version")?;
    let p = policy(s, id)?;
    let authorization = body["authorization_ref"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| auth_ref(&p));
    let key = require(body, "idempotency_key")?;
    let mut request = request_fields(body);
    request["authorization_ref"] = json!(authorization);
    let mut identity = request.clone();
    identity["idempotency_key"] = json!(key);
    let digest = crate::runs::src_api::digest(&identity);
    let retest_id = format!("sretest-{}", &digest[..24]);
    if let Ok(existing) = rec(s, id, "finding_retest", key) {
        if request_fields(&existing["payload"]) != request {
            return Err(Error::conflict(
                "retest idempotency key was used for different input",
            ));
        }
        return Ok(existing["payload"].clone());
    }
    let mut value = request;
    value["retest_id"] = json!(retest_id);
    value["task_id"] = json!(id);
    value["source_run_id"] = json!(source);
    value["target_url"] = json!(target);
    value["idempotency_key"] = json!(key);
    value["result"] = Value::Null;
    value["status"] = json!("requested");
    value["evidence_refs"] = json!([]);
    value["gaps"] = json!([]);
    value["restoration"] = json!({"status":"not_applicable"});
    value["created_at"] = json!(now());
    value["finished_at"] = Value::Null;
    if let Err(error) = save_rec(s, id, "finding_retest", key, value.clone(), Some(0)) {
        if let Ok(existing) = rec(s, id, "finding_retest", key) {
            if request_fields(&existing["payload"]) == request_fields(&value) {
                return Ok(existing["payload"].clone());
            }
            return Err(Error::conflict("retest idempotency key was used for different input"));
        }
        return Err(error);
    }
    Ok(value)
}
pub(super) async fn dispatch(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let retest_id = require(body, "retest_id")?;
    let row = rec(&s, id, "finding_retest", retest_id)?;
    let mut value = row["payload"].clone();
    if !matches!(value["status"].as_str(),Some("requested"|"dispatch_failed")) || value["recovery_required"]==true{return Ok(value);}
    let report_id = require(&value, "report_id")?;
    let frozen = report(&s, id, report_id)?;
    if frozen["finding_id"] != value["finding_id"]
        || frozen["source_run_id"] != value["source_run_id"]
    {
        return Err(Error::conflict("retest report or original Run changed"));
    }
    let target = require(&value, "target_url")?.to_owned();
    if frozen["document"]["target"] != target {
        return Err(Error::conflict("retest target differs from frozen report"));
    }
    let source = require(&value, "source_run_id")?.to_owned();
    let _ = policy(&s, id)?;
    if workbench_source(&source) {
        task_authorizes_retest_target(&s, id, &target)?;
        value["status"] = json!("dispatched");
        value["directive_receipt"] = json!({
            "ok": true,
            "source": "workbench_manual",
            "detail": "Workbench retest is completed manually; no Run directive was issued"
        });
        value["dispatch_error"] = Value::Null;
        value["recovery_required"] = json!(false);
        value["dispatch_execution_state"] = json!("workbench_manual");
        value["dispatched_at"] = json!(now());
        save_rec(
            &s,
            id,
            "finding_retest",
            require(&value, "idempotency_key")?,
            value.clone(),
            row["revision"].as_u64(),
        )?;
        return Ok(value);
    }
    let run = original_run(&s, id, &source, &target)?;
    let directive = json!({"kind":"src_finding_retest","retest_id":retest_id,
        "finding_id":value["finding_id"],"report_id":report_id,"target_url":target,
        "repair_statement":value["repair_statement"],"expected_target_version":value["expected_target_version"],
        "authorization_ref":value["authorization_ref"],"identity_refs":value["identity_refs"],
        "original_poc_refs":value["original_poc_refs"],"baseline_refs":value["baseline_refs"],
        "budget":value["budget"],"instruction":"在原 Run/SharedGraph 上核验此 Finding 的修复路径；先核验身份、对象和目标版本，保留正常对照、执行回执及恢复读回；不要创建新 Run。"});
    let command_id = format!("retest-{retest_id}");
    let response = runs::control_run(
        State(s.clone()),
        Path(source.to_owned()),
        Json(json!({
        "action":"directive","scope":"global","actor_id":value["requester"],"command_id":command_id,
        "text":directive.to_string(),"standing":true,"preempt_policy":"soft_rebind"})),
    )
    .await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(crate::runs::src_api::db)?;
    let receipt: Value = serde_json::from_slice(&bytes).map_err(crate::runs::src_api::db)?;
    let success = status.is_success() && receipt["ok"] == true;
    value["status"] = json!(if success {
        "dispatched"
    } else {
        "dispatch_failed"
    });
    value["directive_receipt"] = receipt.clone();
    value["dispatch_error"] = if success {
        Value::Null
    } else {
        json!({"http_status":status.as_u16(),"detail":receipt["detail"]})
    };
    value["recovery_required"] = json!(!success);
    value["dispatch_execution_state"] = json!(if run["running"] == true {
        "delivered_to_running_run"
    } else {
        "journaled_for_original_run"
    });
    value["dispatched_at"] = json!(now());
    save_rec(
        &s,
        id,
        "finding_retest",
        require(&value, "idempotency_key")?,
        value.clone(),
        row["revision"].as_u64(),
    )?;
    if !success {
        let _ = super::coordination::enqueue_wakeup(
            &s,
            id,
            &format!("retest_dispatch_failed:{retest_id}"),
            "retest",
            &format!("复测 `{retest_id}` 下发原 Run 失败"),
            "原 Run 未接收复测指令，需核对执行回执或重试",
            json!({
                "retest_id": retest_id,
                "finding_id": value["finding_id"],
                "report_id": value["report_id"],
                "run_id": source,
                "target_url": target
            }),
        );
    }
    Ok(value)
}
fn verdict_input(body:&Value)->Api<Value>{
    for field in ["preconditions_valid","target_version_confirmed"]{if !body[field].is_boolean(){return Err(Error::invalid(format!("{field} must be boolean")));}}
    for field in ["original_path_impact","normal_control_works"]{if body.get(field).is_none() || !(body[field].is_null() || body[field].is_boolean()){return Err(Error::invalid(format!("{field} must be boolean or null")));}}
    require(body,"reviewer")?;
    if !body["evidence_refs"].is_array(){return Err(Error::invalid("evidence_refs must be an array"));}
    for field in ["gaps","execution_receipt_ids"]{if let Some(value)=body.get(field).filter(|value|!value.is_null()){if !value.as_array().is_some_and(|rows|rows.iter().all(Value::is_string)){return Err(Error::invalid(format!("{field} must be a text array")));}}}
    for field in ["restoration","substitute_poc"]{if body.get(field).is_some_and(|value|!value.is_null() && !value.is_object()){return Err(Error::invalid(format!("{field} must be an object or null")));}}
    if body.get("outcome_unknown").is_some_and(|value|!value.is_boolean()){return Err(Error::invalid("outcome_unknown must be boolean"));}
    Ok(json!({"preconditions_valid":body["preconditions_valid"],"target_version_confirmed":body["target_version_confirmed"],"original_path_impact":body["original_path_impact"],"normal_control_works":body["normal_control_works"],"evidence_refs":body["evidence_refs"],"reviewer":body["reviewer"],"gaps":body["gaps"].as_array().cloned().unwrap_or_default(),"restoration":body["restoration"].as_object().cloned().unwrap_or_default(),"outcome_unknown":body["outcome_unknown"]==true,"substitute_poc":body["substitute_poc"],"execution_receipt_ids":body["execution_receipt_ids"].as_array().cloned().unwrap_or_default()}))
}
pub(super) fn recover(s:&AppState,id:&str)->Api<Value>{
    let mut recovered=Vec::new();
    for row in records(s,id,"finding_retest")?{
        let mut value=row["payload"].clone();if value["status"]!="dispatched" || !value["result"].is_null(){continue;}
        if value["recovery_required"]!=true{
            value["recovery_required"]=json!(true);value["recovery_note"]=json!("已有下发记录但无最终复测结论；先核对原 Run 回执，再进行人工确认");
            save_rec(s,id,"finding_retest",require(&value,"idempotency_key")?,value.clone(),row["revision"].as_u64())?;
        }recovered.push(value);
    }Ok(json!(recovered))
}

pub(super) fn complete(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let retest_id = require(body, "retest_id")?;
    let row = rec(s, id, "finding_retest", retest_id)?;
    let mut value = row["payload"].clone();
    let normalized=verdict_input(body)?;
    let mut refs = Vec::new();
    for reference in body["evidence_refs"].as_array().into_iter().flatten() {
        verify_ref(s, id, reference)?;
        refs.push(reference.clone());
    }
    let mut receipt_ids = Vec::new();
    for reference in body["execution_receipt_ids"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let rid = reference
            .as_str()
            .ok_or_else(|| Error::invalid("execution receipt ID must be a string"))?;
        let receipt = rec(s, id, "execution_receipt", rid)?["payload"].clone();
        if receipt["dispatch_state"] != "completed"
            || receipt["outcome_known"] != true
            || (receipt["result_ref"].is_null() && receipt["result"].is_null())
        {
            return Err(Error::conflict(
                "retest execution is not complete with a persisted known outcome",
            ));
        }
        receipt_ids.push(rid.to_owned());
    }
    if value["status"] == "completed" {
        if value["verdict_input"] == normalized {
            return Ok(value);
        }
        return Err(Error::conflict(
            "retest is complete with different evidence",
        ));
    }
    let restoration = if body["restoration"].as_object().is_some_and(|value|!value.is_empty()) {
        body["restoration"].clone()
    } else {
        json!({"status":"not_applicable"})
    };
    if !matches!(
        restoration["status"].as_str(),
        Some("restored" | "partially_restored" | "not_restored" | "unknown" | "not_applicable")
    ) {
        return Err(Error::invalid("invalid restoration status"));
    }
    let mut gaps: Vec<Value> = body["gaps"].as_array().cloned().unwrap_or_default();
    let mut gap = |text: &str| {
        if !gaps.iter().any(|v| v == text) {
            gaps.push(json!(text));
        }
    };
    if value["status"] != "dispatched" || value["directive_receipt"].is_null() {
        gap("复测尚未成功下发到原 Run");
    }
    if body["force_fixed"] == true {
        return Err(Error::invalid(
            "force_fixed is disallowed; retest outcome is derived from verified receipts and gaps",
        ));
    }
    if receipt_ids.is_empty() {
        gap("缺少本次复测的真实执行回执");
    } else {
        for rid in &receipt_ids {
            if let Ok(rrow) = rec(s, id, "execution_receipt", rid) {
                let p = &rrow["payload"];
                if p["dispatch_state"] != "completed"
                    || p["outcome_known"] != true
                    || p["error"].as_str().is_some_and(|e| !e.is_empty())
                {
                    gap("复测执行回执存在错误、超时或未终态结果，严格判为 inconclusive");
                }
            }
        }
    }
    if restoration["status"] == "not_restored" {
        gap("实验验证对象尚未恢复清理");
    }
    if body["preconditions_valid"] != true {
        gap("原复现前提未成立");
    }
    if body["target_version_confirmed"] != true {
        gap("修复目标版本未确认");
    }
    if body["normal_control_works"] != true {
        gap("正常业务对照未确认工作");
    }
    if !body["original_path_impact"].is_boolean() {
        gap("原攻击路径结果未知");
    }
    if body["outcome_unknown"] == true {
        gap("执行结果未知，需要核对原调用回执");
    }
    if refs.is_empty() {
        gap("缺少现场证据");
    }
    let result = if gaps.is_empty() && body["original_path_impact"] == false {
        "fixed"
    } else if gaps.is_empty() && body["original_path_impact"] == true {
        "still_vulnerable"
    } else {
        "inconclusive"
    };
    value["result"] = json!(result);
    value["status"] = json!("completed");
    value["evidence_refs"] = json!(refs);
    value["execution_receipt_ids"] = json!(receipt_ids);
    value["gaps"] = json!(gaps);
    value["restoration"] = restoration;
    value["reviewer"] = json!(body["reviewer"].as_str().unwrap_or("local-operator"));
    value["verdict_input"] = normalized.clone();
    value["substitute_poc"]=normalized["substitute_poc"].clone();
    value["scope_limit"] =
        json!("仅针对原报告路径、已确认目标版本、身份和已测影响；不证明整个资产无漏洞");
    value["recovery_required"] = json!(body["outcome_unknown"] == true);
    value["finished_at"] = json!(now());
    value["verification"] = json!("operator_retest_with_persisted_receipts");
    let review_id = format!("sreview-{retest_id}");
    let review = json!({"review_id":review_id,"finding_id":value["finding_id"],"report_id":value["report_id"],
        "validation_type":"repair_retest","result":result,"reviewer":value["reviewer"],
        "evidence_refs":value["evidence_refs"],"gaps":value["gaps"],"created_at":value["finished_at"],"retest_id":retest_id});
    let retest_key = require(&value, "idempotency_key")?;
    let (retest_storage_id, retest_version, retest_row) = stage_rec(
        s, id, "finding_retest", retest_key, value.clone(), row["revision"].as_u64(),
    )?;
    let (review_storage_id, review_version, review_row) = stage_rec(
        s, id, "retest_review", &review_id, review, Some(0),
    )?;
    let before = s.store.get(RECORDS, &retest_storage_id).map_err(db)?;
    if before.as_ref().and_then(|record| record["version"].as_u64()) != Some(retest_version) {
        return Err(Error::conflict("retest changed before completion"));
    }
    let retest_doc = json!({"version":retest_version+1,"values":retest_row});
    let review_doc = json!({"version":review_version+1,"values":review_row});
    let committed = s.store.put_many_if(
        &[(RECORDS, &retest_storage_id, before.as_ref()),
            (RECORDS, &review_storage_id, None)],
        &[(RECORDS, &retest_storage_id, &retest_doc),
            (RECORDS, &review_storage_id, &review_doc)],
    ).map_err(db)?;
    if !committed {
        let latest = rec(s, id, "finding_retest", retest_id)?["payload"].clone();
        if latest["status"] == "completed" && latest["verdict_input"] == normalized {
            return Ok(latest);
        }
        return Err(Error::conflict("retest changed before completion"));
    }
    let _ = super::coordination::enqueue_wakeup(
        s,
        id,
        &format!("retest_completed:{retest_id}"),
        "retest",
        &format!("复测 `{retest_id}` 已完成（{result}）"),
        &format!("Finding `{}` 复测结论为 `{result}`", value["finding_id"].as_str().unwrap_or("")),
        json!({
            "retest_id": retest_id,
            "finding_id": value["finding_id"],
            "report_id": value["report_id"],
            "run_id": value["source_run_id"],
            "target_url": value["target_url"],
            "result": result
        }),
    );
    Ok(value)
}
