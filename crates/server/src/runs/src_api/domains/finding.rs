//! Versioned SRC claims and evidence-backed reviews, following the source Finding service.
mod cvss;
mod dedup;
mod staleness;

pub(super) fn assess_cvss(input: &Value) -> Api<Value> {
    cvss::evaluate(input)
}
pub(super) fn refresh_staleness(
    s: &AppState,
    task_id: &str,
    finding_id: &str,
    dependencies: Option<&Value>,
) -> Api<bool> {
    staleness::refresh_staleness(s, task_id, finding_id, dependencies)
}
pub(super) fn capture_dependency_snapshot(
    s: &AppState,
    task_id: &str,
    finding_id: &str,
) -> Api<(String, Value)> {
    let claims = current_claims(s, task_id, finding_id)?;
    let snapshot = build_dependency_snapshot(
        s,
        task_id,
        finding_id,
        &claims,
        &[],
        &json!({}),
        &[],
        &[],
        &[],
        "",
    )?;
    let snapshot_id = new_id("sdepsnap");
    Ok((snapshot_id, snapshot))
}

const FINDING_HEADS: &str = "src_finding_heads";

use super::{
    Api, Error, FINDING_APPLIED, RECORDS, db, now, rec, rec_key, records, require, stage_rec,
    verify_ref,
};
use crate::AppState;
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

fn failure(code: &'static str, message: impl Into<String>) -> Error {
    Error::new(StatusCode::BAD_REQUEST, code, message)
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn values(value: &Value, field: &str) -> Api<Vec<Value>> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        _ => Err(failure(
            "src.invalid_request",
            format!("{field} must be an array"),
        )),
    }
}

fn texts(value: &Value, field: &str) -> Api<Vec<Value>> {
    let items = values(value, field)?;
    if items.iter().any(|item| !item.is_string()) {
        return Err(failure(
            "src.invalid_request",
            format!("{field} must contain strings"),
        ));
    }
    Ok(items)
}

fn evidence(s: &AppState, task_id: &str, value: &Value, field: &str) -> Api<Vec<Value>> {
    let refs = values(value, field)?;
    let mut normalized = Vec::with_capacity(refs.len());
    for reference in &refs {
        if !reference.is_object()
            || reference["artifact_id"].as_str().is_none()
        {
            return Err(failure("src.invalid_request", "invalid artifact reference"));
        }
        if reference["task_id"] != task_id {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "task_binding_invalid",
                "evidence belongs to another Task",
            ));
        }
        verify_ref(s, task_id, reference)?;
        normalized.push(json!({"schema_version":1,"artifact_id":reference["artifact_id"],
            "media_type":reference["media_type"].as_str().unwrap_or("application/octet-stream"),
            "size":reference["size"].as_u64().unwrap_or(0),"task_id":task_id,
            "source_run_id":reference["source_run_id"],"source_event_seq":reference["source_event_seq"],
            "version":reference["version"].as_u64().unwrap_or(1),
            "original":reference["original"].as_bool().unwrap_or(true),
            "complete":reference["complete"].as_str().unwrap_or("complete"),
            "name":reference["name"].as_str().unwrap_or("")}));
    }
    Ok(normalized)
}

fn recorded(s: &AppState, task_id: &str, kind: &str, finding_id: &str) -> Api<Vec<Value>> {
    Ok(records(s, task_id, kind)?
        .into_iter()
        .filter(|row| row["payload"]["finding_id"] == finding_id)
        .collect())
}

fn latest_review(s: &AppState, task_id: &str, finding_id: &str) -> Api<Option<Value>> {
    if let Some(head) = review_head(s, task_id, finding_id)? {
        if let Some(record_id) = head["review_record_id"].as_str() {
            return rec(s, task_id, "finding_review", record_id).map(Some);
        }
    }
    Ok(recorded(s, task_id, "finding_review", finding_id)?.pop())
}

fn review_head(s: &AppState, task_id: &str, finding_id: &str) -> Api<Option<Value>> {
    s.store
        .get(FINDING_HEADS, &format!("{task_id}:{finding_id}"))
        .map_err(db)
}

fn marker_output(s: &AppState, command_id: Option<&str>) -> Api<Option<Value>> {
    command_id
        .map(|command_id| {
            s.store
                .get(FINDING_APPLIED, command_id)
                .map_err(db)
                .map(|row| row.map(|row| row["output"].clone()))
        })
        .transpose()
        .map(|row| row.flatten())
}

/// A Finding mutation and its command attribution are one SQLite commit. The caller
/// recomputes a Claim version if another process won the same natural key.
fn commit(
    s: &AppState,
    task_id: &str,
    entries: &[(String, String, Value)],
    command_id: Option<&str>,
    output: &Value,
    finding_id: &str,
    expected_head: &Option<Value>,
    next_review: Option<&str>,
) -> Api<bool> {
    let mut staged = Vec::new();
    for (kind, key, payload) in entries {
        let rid = rec_key(task_id, kind, key);
        if s.store.get(RECORDS, &rid).map_err(db)?.is_some() {
            return Ok(false);
        }
        let (_, version, row) = match stage_rec(s, task_id, kind, key, payload.clone(), Some(0)) {
            Ok(staged) => staged,
            Err(error) if error.status == StatusCode::CONFLICT => return Ok(false),
            Err(error) => return Err(error),
        };
        staged.push((rid, json!({"version":version+1,"values":row})));
    }
    let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":output}));
    let mut conditions: Vec<(&str, &str, Option<&Value>)> = staged
        .iter()
        .map(|(rid, _)| (RECORDS, rid.as_str(), None))
        .collect();
    let mut writes: Vec<(&str, &str, &Value)> = staged
        .iter()
        .map(|(rid, document)| (RECORDS, rid.as_str(), document))
        .collect();
    let head_key = format!("{task_id}:{finding_id}");
    let mut next_head = expected_head
        .clone()
        .unwrap_or_else(|| json!({"revision":0,"review_record_id":null}));
    next_head["revision"] = json!(next_head["revision"].as_u64().unwrap_or(0) + 1);
    if let Some(record_id) = next_review {
        next_head["review_record_id"] = json!(record_id);
    }
    conditions.push((FINDING_HEADS, &head_key, expected_head.as_ref()));
    writes.push((FINDING_HEADS, &head_key, &next_head));
    if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
        conditions.push((FINDING_APPLIED, command_id, None));
        writes.push((FINDING_APPLIED, command_id, marker));
    }
    s.store.put_many_if(&conditions, &writes).map_err(db)
}

fn current_claims(s: &AppState, task_id: &str, finding_id: &str) -> Api<Vec<Value>> {
    let mut latest = Vec::<(Value, u64)>::new();
    for row in recorded(s, task_id, "finding_claim", finding_id)? {
        let claim = row["payload"].clone();
        let revision = row["revision"].as_u64().unwrap_or(1);
        let Some(claim_id) = claim["claim_id"].as_str() else {
            continue;
        };
        if let Some((old, old_rev)) = latest.iter_mut().find(|(c, _)| c["claim_id"] == claim_id) {
            if claim["version"].as_u64().unwrap_or(0) > old["version"].as_u64().unwrap_or(0) {
                *old = claim;
                *old_rev = revision;
            }
        } else {
            latest.push((claim, revision));
        }
    }
    latest.sort_by(|(a, _), (b, _)| {
        a["claim_id"]
            .as_str()
            .cmp(&b["claim_id"].as_str())
            .then(a["version"].as_u64().cmp(&b["version"].as_u64()))
    });
    Ok(latest
        .into_iter()
        .map(|(claim, revision)| {
            let cid = claim["claim_id"].as_str().unwrap_or("");
            let ver = claim["version"].as_u64().unwrap_or(1);
            json!({
                "claim_id": claim["claim_id"],
                "version": claim["version"],
                "record_id": format!("{cid}:v{ver}"),
                "revision": revision,
                "digest": claim["digest"],
            })
        })
        .collect())
}

fn claim_version_row(
    s: &AppState,
    task_id: &str,
    finding_id: &str,
    claim_id: &str,
    version: Option<u64>,
) -> Api<Value> {
    let matching: Vec<_> = recorded(s, task_id, "finding_claim", finding_id)?
        .into_iter()
        .filter(|row| row["payload"]["claim_id"] == claim_id)
        .collect();
    if matching.is_empty() {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "artifact_missing",
            "claim is unavailable",
        ));
    }
    let selected = if let Some(version) = version {
        matching
            .into_iter()
            .find(|row| row["payload"]["version"] == version)
    } else {
        matching
            .into_iter()
            .max_by_key(|row| row["payload"]["version"].as_u64().unwrap_or(0))
    };
    selected.ok_or_else(|| {
        Error::new(
            StatusCode::CONFLICT,
            "version_conflict",
            "claim version is unavailable",
        )
    })
}

fn claim_version(
    s: &AppState,
    task_id: &str,
    finding_id: &str,
    claim_id: &str,
    version: Option<u64>,
) -> Api<Value> {
    Ok(claim_version_row(s, task_id, finding_id, claim_id, version)?["payload"].clone())
}

fn build_dependency_snapshot(
    s: &AppState,
    task_id: &str,
    finding_id: &str,
    claims: &[Value],
    evidence_snapshot: &[Value],
    dependencies: &Value,
    exchange_refs: &[Value],
    identity_refs: &[Value],
    receipt_ids: &[Value],
    retest_id: &str,
) -> Api<Value> {
    let mut claim_entries = Vec::new();
    let mut seen_artifacts = std::collections::BTreeSet::new();
    let mut artifact_entries = Vec::new();
    let mut add_artifact = |reference: &Value| {
        let art_task = reference["task_id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or(task_id)
            .to_owned();
        let Some(art_id) = reference["artifact_id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
        else {
            return;
        };
        if !seen_artifacts.insert((art_task.clone(), art_id.clone())) {
            return;
        }
        let ver = super::artifact_record(s, &art_task, reference)
            .ok()
            .and_then(|(_, saved)| saved["version"].as_u64())
            .or_else(|| reference["version"].as_u64())
            .unwrap_or(1);
        artifact_entries.push(json!({
            "task_id": art_task,
            "artifact_id": art_id,
            "version": ver,
        }));
    };

    for item in claims {
        let Some(cid) = item["claim_id"].as_str().filter(|v| !v.is_empty()) else {
            continue;
        };
        let requested_ver = item["version"].as_u64();
        if let Ok(row) = claim_version_row(s, task_id, finding_id, cid, requested_ver) {
            let ver = row["payload"]["version"].as_u64().unwrap_or(1);
            let rev = row["revision"].as_u64().unwrap_or(1);
            for ev in row["payload"]["evidence_refs"]
                .as_array()
                .into_iter()
                .flatten()
            {
                add_artifact(ev);
            }
            claim_entries.push(json!({
                "claim_id": cid,
                "version": ver,
                "record_id": format!("{cid}:v{ver}"),
                "revision": rev,
            }));
        } else {
            let ver = requested_ver.unwrap_or(1);
            let rev = item["revision"].as_u64().unwrap_or(1);
            claim_entries.push(json!({
                "claim_id": cid,
                "version": ver,
                "record_id": format!("{cid}:v{ver}"),
                "revision": rev,
            }));
        }
    }
    claim_entries.sort_by(|a, b| {
        a["claim_id"]
            .as_str()
            .cmp(&b["claim_id"].as_str())
            .then(a["version"].as_u64().cmp(&b["version"].as_u64()))
    });

    for ev in evidence_snapshot {
        add_artifact(ev);
    }
    if let Some(dep_arts) = dependencies["artifacts"].as_array() {
        for ev in dep_arts {
            add_artifact(ev);
        }
    }

    let mut seen_records = std::collections::BTreeSet::new();
    let mut record_entries = Vec::new();
    let mut add_record = |r_task: &str, r_kind: &str, r_id: &str, fallback_rev: Option<u64>| {
        if r_kind.is_empty() || r_id.is_empty() {
            return;
        }
        let resolved = rec(s, r_task, r_kind, r_id).ok();
        let actual_id = resolved
            .as_ref()
            .and_then(|r| r["record_id"].as_str())
            .unwrap_or(r_id)
            .to_owned();
        if !seen_records.insert((r_task.to_owned(), r_kind.to_owned(), actual_id.clone())) {
            return;
        }
        let rev = resolved
            .as_ref()
            .and_then(|r| r["revision"].as_u64())
            .or(fallback_rev)
            .unwrap_or(1);
        record_entries.push(json!({
            "task_id": r_task,
            "kind": r_kind,
            "record_id": actual_id,
            "revision": rev,
        }));
    };

    for ex in exchange_refs.iter().filter_map(Value::as_str) {
        add_record(task_id, "http_exchange", ex, None);
    }
    for id_ref in identity_refs.iter().filter_map(Value::as_str) {
        add_record(task_id, "browser_identity", id_ref, None);
    }
    for rcpt in receipt_ids.iter().filter_map(Value::as_str) {
        add_record(task_id, "execution_receipt", rcpt, None);
    }
    if !retest_id.is_empty() {
        add_record(task_id, "finding_retest", retest_id, None);
    }
    if let Some(dep_recs) = dependencies["records"].as_array() {
        for item in dep_recs {
            let r_task = item["task_id"]
                .as_str()
                .filter(|v| !v.is_empty())
                .unwrap_or(task_id);
            let r_kind = item["kind"].as_str().unwrap_or("");
            let r_id = item["record_id"].as_str().unwrap_or("");
            add_record(r_task, r_kind, r_id, item["revision"].as_u64());
        }
    }
    let mut external_versions = serde_json::Map::new();
    if let Some(ext) = dependencies["external_versions"].as_object() {
        for (k, v) in ext {
            external_versions.insert(k.clone(), v.clone());
        }
    }
    if let Some(dep_map) = dependencies.as_object() {
        for (k, v) in dep_map {
            if matches!(
                k.as_str(),
                "claims" | "records" | "artifacts" | "external_versions"
            ) {
                continue;
            }
            if let Some((kind_part, id_part)) = k.split_once(':') {
                if rec(s, task_id, kind_part, id_part).is_ok() {
                    add_record(task_id, kind_part, id_part, v.as_u64());
                    continue;
                }
            }
            external_versions.insert(k.clone(), v.clone());
        }
    }

    Ok(json!({
        "claims": claim_entries,
        "records": record_entries,
        "artifacts": artifact_entries,
        "external_versions": Value::Object(external_versions),
    }))
}

fn stale_payload(
    review: &Value,
    finding_id: &str,
    reason: &str,
    dependency_snapshot_id: &str,
    dependency_snapshot: &Value,
    source_refs: &[Value],
) -> (String, Value) {
    let refs_key = source_refs
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(",");
    let review_id = review["review_id"].as_str().unwrap_or("unreviewed");
    let key = format!("stale:{finding_id}:{review_id}:{reason}:{dependency_snapshot_id}:{refs_key}");
    let payload = json!({
        "finding_id": finding_id,
        "review_id": review["review_id"],
        "reason": reason,
        "dependency_snapshot_id": dependency_snapshot_id,
        "dependency_snapshot": dependency_snapshot,
        "dependency_digest": dependency_snapshot_id,
        "source_refs": source_refs,
        "created_at": now(),
        "cleared": false,
    });
    (key, payload)
}

fn create_claim(
    s: &AppState,
    task_id: &str,
    input: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    let status = require(input, "status")?;
    if !matches!(status, "observed" | "supported_inference" | "unverified") {
        return Err(failure("claim_status_invalid", "unsupported claim status"));
    }
    let text = require(input, "text")
        .map_err(|_| failure("claim_text_required", "claim text is required"))?;
    let evidence_refs = evidence(s, task_id, input, "evidence_refs")?;
    let counterevidence_refs = evidence(s, task_id, input, "counterevidence_refs")?;
    if status != "unverified" && evidence_refs.is_empty() {
        return Err(failure(
            "evidence_required",
            "an observed or inferred claim needs source evidence",
        ));
    }
    let exchange_refs = texts(input, "exchange_refs")?;
    let identity_refs = texts(input, "identity_refs")?;
    let limitations = texts(input, "limitations")?;
    let missing_proof = values(input, "missing_proof")?;
    let finding_id = input["finding_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| new_id("sfind"));
    let claim_id = input["claim_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| new_id("sclaim"));
    loop {
        if let Some(output) = marker_output(s, command_id)? {
            return Ok(output);
        }
        let head = review_head(s, task_id, &finding_id)?;
        let existing: Vec<_> = recorded(s, task_id, "finding_claim", &finding_id)?
            .into_iter()
            .filter(|row| row["payload"]["claim_id"] == claim_id)
            .collect();
        let previous = existing
            .iter()
            .max_by_key(|row| row["payload"]["version"].as_u64().unwrap_or(0));
        let version = previous
            .and_then(|row| row["payload"]["version"].as_u64())
            .unwrap_or(0)
            + 1;
        let supersedes = previous
            .and_then(|row| row["natural_key"].as_str())
            .unwrap_or("")
            .to_owned();
        let claim_record_id = format!("{claim_id}:v{version}");
        let mut claim = json!({"schema_version":1,"claim_id":claim_id,"finding_id":finding_id,"task_id":task_id,
            "version":version,"text":text,"status":status,"evidence_refs":evidence_refs,
            "counterevidence_refs":counterevidence_refs,"source_run_id":input["source_run_id"],
            "fact_ref":input["fact_ref"].as_str().unwrap_or(""),"exchange_refs":exchange_refs,
            "target_version":input["target_version"].as_str().unwrap_or(""),
            "identity_refs":identity_refs,"limitations":limitations,"missing_proof":missing_proof,
            "created_at":now(),"supersedes":supersedes,"digest":format!("claim-version:{claim_record_id}")});
        claim["record_id"] = json!(claim_record_id);
        let mut entries = vec![(
            "finding_claim".to_owned(),
            claim_record_id.clone(),
            claim.clone(),
        )];
        if let Some(review) = latest_review(s, task_id, &finding_id)? {
            let selected = values(&review["payload"], "claim_versions")?;
            let selected_claim_changed = selected.is_empty()
                || selected.iter().any(|claim| claim["claim_id"] == claim_id);
            if !selected_claim_changed {
                if commit(
                    s,
                    task_id,
                    &entries,
                    command_id,
                    &claim,
                    &finding_id,
                    &head,
                    None,
                )? {
                    return Ok(claim);
                }
                continue;
            }
            let mut snap_claims = current_claims(s, task_id, &finding_id)?
                .into_iter()
                .filter(|claim| {
                    selected.is_empty()
                        || selected
                            .iter()
                            .any(|item| item["claim_id"] == claim["claim_id"])
                })
                .collect::<Vec<_>>();
            snap_claims.retain(|c| c["claim_id"] != claim_id);
            snap_claims.push(json!({
                "claim_id": claim_id,
                "version": version,
                "record_id": claim_record_id,
                "revision": 1,
            }));
            let snapshot = build_dependency_snapshot(
                s,
                task_id,
                &finding_id,
                &snap_claims,
                &evidence_refs,
                &review["payload"]["dependencies"],
                &exchange_refs,
                &identity_refs,
                &[],
                "",
            )?;
            let snapshot_id = format!("depsnap:{claim_record_id}");
            let (key, marker) = stale_payload(
                &review["payload"],
                &finding_id,
                "claim_or_evidence_changed",
                &snapshot_id,
                &snapshot,
                &[json!(claim_record_id)],
            );
            entries.push(("finding_stale".to_owned(), key, marker));
        }
        if commit(
            s,
            task_id,
            &entries,
            command_id,
            &claim,
            &finding_id,
            &head,
            None,
        )? {
            return Ok(claim);
        }
    }
}

fn verify_execution(s: &AppState, task_id: &str, receipt_ids: &[Value]) -> Api<()> {
    for receipt_id in receipt_ids {
        let receipt_id = receipt_id.as_str().ok_or_else(|| {
            failure(
                "src.invalid_request",
                "execution_receipt_ids must contain strings",
            )
        })?;
        let receipt = rec(s, task_id, "execution_receipt", receipt_id)?["payload"].clone();
        if receipt["dispatch_state"] != "completed" || receipt["outcome_known"] != true {
            return Err(failure(
                "review_not_live",
                "live reproduction needs a known completed execution receipt",
            ));
        }
        if receipt["result_ref"].is_null() && receipt["result"].is_null() {
            return Err(failure(
                "review_not_live",
                "live reproduction receipt has no persisted execution result",
            ));
        }
        if !receipt["result_ref"].is_null() {
            verify_ref(s, task_id, &receipt["result_ref"])?;
        }
    }
    Ok(())
}

fn record_review(
    s: &AppState,
    task_id: &str,
    input: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    let finding_id = require(input, "finding_id")?;
    let reviewer = require(input, "reviewer")
        .map_err(|_| failure("reviewer_required", "reviewer identity is required"))?;
    let validation_type = require(input, "validation_type")?;
    if !matches!(
        validation_type,
        "artifact_review" | "live_reproduction" | "repair_retest"
    ) {
        return Err(failure(
            "validation_type_invalid",
            "unknown validation type",
        ));
    }
    let readiness = require(input, "readiness")?;
    if !matches!(readiness, "DRAFT" | "GREEN" | "YELLOW" | "RED") {
        return Err(failure("readiness_invalid", "unknown readiness"));
    }
    let reasons = texts(input, "reasons")?;
    let mut missing = values(input, "missing")?;
    let executed_steps = texts(input, "executed_steps")?;
    let skipped_steps = values(input, "skipped_steps")?;
    let identity_refs = texts(input, "identity_refs")?;
    let execution_receipt_ids = texts(input, "execution_receipt_ids")?;
    let evidence_snapshot = evidence(s, task_id, input, "evidence_snapshot")?;
    let has_baseline = execution_receipt_ids.len() >= 2
        || evidence_snapshot.len() >= 2
        || input["baseline_confirmed"] == true
        || input["comparison_refs"]
            .as_array()
            .is_some_and(|arr| !arr.is_empty());
    let has_quantitative = input["impact_metrics"].is_object()
        || input["quantitative_proof"]
            .as_str()
            .is_some_and(|v| !v.trim().is_empty())
        || input["affected_count"].as_u64().is_some();
    let evidence_level = if validation_type == "live_reproduction"
        && input["independent_context"] == true
        && !execution_receipt_ids.is_empty()
        && has_baseline
        && has_quantitative
    {
        "L4"
    } else if has_quantitative {
        "L3"
    } else if has_baseline {
        "L2"
    } else {
        "L1"
    };
    let auto_draft = readiness == "DRAFT"
        || (readiness == "YELLOW"
            && input["auto_draft_if_incomplete"] == true
            && (!has_baseline || !has_quantitative));
    let effective_readiness = if auto_draft { "DRAFT" } else { readiness };
    let draft_state = if effective_readiness == "DRAFT" {
        if !has_baseline && !missing.iter().any(|m| m["code"] == "missing_baseline_control") {
            missing.push(json!({"code": "missing_baseline_control", "action": "补抓正常账号或合法基准对照请求"}));
        }
        if !has_quantitative && !missing.iter().any(|m| m["code"] == "missing_quantitative_impact") {
            missing.push(json!({"code": "missing_quantitative_impact", "action": "补充可量化影响范围（如可越权访问记录数/响应字节差异）"}));
        }
        json!("needs_supplement")
    } else {
        Value::Null
    };
    let mut claims = values(input, "claim_versions")?;
    if claims.is_empty() {
        claims = current_claims(s, task_id, finding_id)?;
    }
    let mut normalized_claims = Vec::with_capacity(claims.len());
    for selected in &claims {
        let selected_id = require(selected, "claim_id")?;
        let version = selected["version"].as_u64().unwrap_or(0);
        let claim_row = claim_version_row(s, task_id, finding_id, selected_id, Some(version))?;
        let claim = &claim_row["payload"];
        if let Some(expected_digest) = selected["digest"].as_str().filter(|v| !v.is_empty()) {
            if claim["digest"].as_str() != Some(expected_digest) {
                return Err(Error::new(
                    StatusCode::CONFLICT,
                    "artifact_hash_mismatch",
                    "claim digest changed",
                ));
            }
        }
        if let Some(expected_rev) = selected["revision"].as_u64() {
            if claim_row["revision"].as_u64() != Some(expected_rev) {
                return Err(Error::new(
                    StatusCode::CONFLICT,
                    "version_conflict",
                    "claim revision changed",
                ));
            }
        }
        normalized_claims.push(json!({
            "claim_id": selected_id,
            "version": version,
            "record_id": format!("{selected_id}:v{version}"),
            "revision": claim_row["revision"].as_u64().unwrap_or(1),
            "digest": claim["digest"],
        }));
    }
    let claims = normalized_claims;
    let mut alternatives = Vec::new();
    for alternative in values(input, "alternative_explanations")? {
        if !alternative.is_object() {
            return Err(failure(
                "src.invalid_request",
                "invalid alternative explanation",
            ));
        }
        let refs = if alternative["evidence_refs"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
        {
            evidence(s, task_id, &alternative, "evidence_refs")?
        } else {
            evidence(s, task_id, &alternative, "counterevidence_refs")?
        };
        if readiness == "GREEN" && refs.is_empty() {
            return Err(failure(
                "counterevidence_required",
                "addressed alternatives need original evidence",
            ));
        }
        let mut normalized = alternative;
        normalized["evidence_refs"] = json!(refs);
        alternatives.push(normalized);
    }
    let cvss = if input["cvss"].is_null() {
        Value::Null
    } else {
        cvss::evaluate(&input["cvss"])?
    };
    if validation_type == "repair_retest" {
        let retest_id = require(input, "retest_id")
            .map_err(|_| failure("retest_required", "repair_retest requires retest_id"))?;
        let retest = rec(s, task_id, "finding_retest", retest_id)
            .or_else(|_| rec(s, task_id, "retest", retest_id))?["payload"]
            .clone();
        if !matches!(
            retest["result"].as_str(),
            Some("fixed" | "still_vulnerable" | "inconclusive")
        ) {
            return Err(failure(
                "retest_incomplete",
                "retest has no terminal result",
            ));
        }
    }
    match effective_readiness {
        "DRAFT" => {}
        "RED" if reasons.is_empty() => {
            return Err(failure(
                "review_reason_required",
                "RED requires concrete reasons",
            ));
        }
        "YELLOW" => {
            if evidence_snapshot.is_empty() {
                return Err(failure(
                    "evidence_required",
                    "YELLOW requires supporting evidence",
                ));
            }
            if missing.is_empty() {
                return Err(failure(
                    "review_gap_required",
                    "YELLOW requires precise evidence gaps",
                ));
            }
            for gap in &missing {
                if gap["code"].as_str().is_none_or(|v| v.trim().is_empty())
                    || gap["action"].as_str().is_none_or(|v| v.trim().is_empty())
                {
                    return Err(failure(
                        "review_gap_invalid",
                        "each gap needs code and next action",
                    ));
                }
            }
        }
        "GREEN" => {
            if validation_type != "live_reproduction" {
                return Err(failure(
                    "review_not_live",
                    "GREEN requires independent live reproduction, not artifact review or repair retest",
                ));
            }
            if input["independent_context"] != true {
                return Err(failure(
                    "review_not_independent",
                    "GREEN requires a reviewer context that did not perform the original exploration",
                ));
            }
            if executed_steps.is_empty() || !skipped_steps.is_empty() {
                return Err(failure(
                    "review_steps_incomplete",
                    "GREEN requires all reproduction steps to be executed",
                ));
            }
            if input["target_version"].as_str().is_none_or(str::is_empty) {
                return Err(failure(
                    "target_version_required",
                    "GREEN requires the verified target version",
                ));
            }
            if execution_receipt_ids.is_empty() {
                return Err(failure(
                    "review_not_live",
                    "GREEN requires live execution receipts",
                ));
            }
            verify_execution(s, task_id, &execution_receipt_ids)?;
            if evidence_snapshot.is_empty() {
                return Err(failure(
                    "evidence_required",
                    "GREEN requires readable validation evidence",
                ));
            }
            if reasons.is_empty() {
                return Err(failure(
                    "review_reason_required",
                    "GREEN requires the verification rationale",
                ));
            }
            if !missing.is_empty() {
                return Err(failure(
                    "review_gap_unresolved",
                    "GREEN cannot retain unresolved evidence gaps",
                ));
            }
            if alternatives.is_empty() {
                return Err(failure(
                    "counterevidence_required",
                    "GREEN requires review of the strongest alternative explanation",
                ));
            }
            for alternative in &alternatives {
                if alternative["explanation"]
                    .as_str()
                    .is_none_or(|v| v.trim().is_empty())
                {
                    return Err(failure(
                        "counterevidence_invalid",
                        "alternative explanation text is required",
                    ));
                }
                if alternative["status"] != "addressed" {
                    return Err(failure(
                        "counterevidence_unresolved",
                        "GREEN cannot retain an unaddressed alternative explanation",
                    ));
                }
                if alternative["assessment"]
                    .as_str()
                    .is_none_or(|v| v.trim().is_empty())
                {
                    return Err(failure(
                        "counterevidence_invalid",
                        "addressed alternatives need an assessment",
                    ));
                }
            }
            if cvss.is_null() {
                return Err(failure(
                    "cvss_required",
                    "GREEN requires a reasoned CVSS 3.1 assessment",
                ));
            }
            if let Some(repro) = input.get("independent_reproduction").filter(|v| v.is_object()) {
                let repro_id = repro["reproduction_id"].as_str().unwrap_or("");
                let saved = rec(s, task_id, "report_reproduction", repro_id).map_err(|_| {
                    failure("reproduction_missing", "referenced report_reproduction does not exist")
                })?;
                if saved["payload"]["success"] != true || saved["payload"]["finding_id"] != finding_id {
                    return Err(failure(
                        "reproduction_invalid",
                        "independent_reproduction must succeed and belong to this Finding",
                    ));
                }
            }
        }
        _ => {}
    }
    let dependencies = if input["dependencies"].is_null() {
        json!({})
    } else {
        if !input["dependencies"].is_object() {
            return Err(failure(
                "src.invalid_request",
                "dependencies must be an object",
            ));
        }
        input["dependencies"].clone()
    };
    loop {
        if let Some(output) = marker_output(s, command_id)? {
            return Ok(output);
        }
        let head = review_head(s, task_id, finding_id)?;
        let previous = latest_review(s, task_id, finding_id)?;
        let review_id = new_id("sreview");
        let review_record_id = rec_key(task_id, "finding_review", &review_id);
        let dependency_snapshot = build_dependency_snapshot(
            s,
            task_id,
            finding_id,
            &claims,
            &evidence_snapshot,
            &dependencies,
            &[],
            &identity_refs,
            &execution_receipt_ids,
            input["retest_id"].as_str().unwrap_or(""),
        )?;
        let dependency_snapshot_id = new_id("sdepsnap");
        let review = json!({"schema_version":1,"review_id":review_id,"finding_id":finding_id,
            "report_id":input["report_id"],"validation_type":validation_type,"readiness":effective_readiness,
            "draft_state":draft_state,"evidence_level":evidence_level,
            "reasons":reasons,"missing":missing,"evidence_snapshot":evidence_snapshot,
            "executed_steps":executed_steps,"skipped_steps":skipped_steps,"reviewer":reviewer,
            "target_version":input["target_version"].as_str().unwrap_or(""),
            "identity_refs":identity_refs,"created_at":now(),"stale_reason":"",
            "claim_versions":claims,"alternative_explanations":alternatives,
            "independent_context":input["independent_context"] == true,
            "execution_receipt_ids":execution_receipt_ids,
            "retest_id":input["retest_id"].as_str().unwrap_or(""),"dependencies":dependencies,
            "dependency_snapshot_id":dependency_snapshot_id,
            "dependency_snapshot":dependency_snapshot,
            "dependency_digest":dependency_snapshot_id,
            "evidence_key_version":"artifact-id.v1","cvss":cvss,
            "independent_reproduction":input.get("independent_reproduction").cloned().unwrap_or(Value::Null),
            "supersedes":previous.as_ref().map(|v|v["payload"]["review_id"].clone()).unwrap_or(json!(""))});
        if commit(
            s,
            task_id,
            &[("finding_review".to_owned(), review_id.clone(), review.clone())],
            command_id,
            &review,
            finding_id,
            &head,
            Some(&review_record_id),
        )? {
            if effective_readiness == "DRAFT" {
                let sug_id = format!("ssug_supp_{}", finding_id);
                let sug_rev = rec(s, task_id, "workbench_suggestion", &sug_id)
                    .ok()
                    .and_then(|r| r["revision"].as_u64());
                let actions = missing
                    .iter()
                    .filter_map(|m| m["action"].as_str())
                    .collect::<Vec<_>>();
                let summary = if actions.is_empty() {
                    "发现暂存为 DRAFT，补齐正常对照与量化证据后可升级复核".to_owned()
                } else {
                    actions.join("；")
                };
                let stamped = now();
                let sug = json!({
                    "suggestion_id": sug_id,
                    "task_id": task_id,
                    "kind": "supplement_evidence",
                    "status": "pending",
                    "decision": "pending",
                    "decided_by": null,
                    "decided_at": null,
                    "applied_ref": null,
                    "proposed_by": reviewer,
                    "proposed_at": stamped,
                    "finding_id": finding_id,
                    "evidence_level": evidence_level,
                    "title": format!("草稿发现待补证: {finding_id} ({evidence_level})"),
                    "summary": summary,
                    "rationale": summary,
                    "proposal": {
                        "finding_id": finding_id,
                        "evidence_level": evidence_level,
                        "missing": missing,
                    },
                    "missing": missing,
                    "updated_at": stamped,
                });
                crate::runs::src_api::save_rec(s, task_id, "workbench_suggestion", &sug_id, sug, sug_rev)?;
            }
            if effective_readiness == "RED" {
                let reason_text = reasons
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join("；");
                let _ = super::coordination::enqueue_wakeup(
                    s,
                    task_id,
                    &format!("finding_refuted:{review_id}"),
                    "finding_invalidated",
                    &format!("发现 `{finding_id}` 复核为 RED（假设被推翻）"),
                    &reason_text,
                    json!({
                        "finding_id": finding_id,
                        "review_id": review_id,
                        "reasons": reasons,
                    }),
                );
            }
            return Ok(review);
        }
    }
}

fn invalidate(s: &AppState, task_id: &str, input: &Value, command_id: Option<&str>) -> Api<Value> {
    let finding_id = require(input, "finding_id")?;
    let reason = require(input, "reason").map_err(|_| {
        failure(
            "validation_failed",
            "stale reason and dependency snapshot are required",
        )
    })?;
    let supplied_id = input["dependency_snapshot_id"]
        .as_str()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            input["dependency_digest"]
                .as_str()
                .filter(|v| !v.trim().is_empty())
        });
    let has_snapshot_obj = input
        .get("dependency_snapshot")
        .is_some_and(Value::is_object);
    if supplied_id.is_none() && !has_snapshot_obj {
        return Err(failure(
            "validation_failed",
            "stale reason and dependency snapshot are required",
        ));
    }
    let source_refs = texts(input, "source_refs")?;
    loop {
        if let Some(output) = marker_output(s, command_id)? {
            return Ok(output);
        }
        let head = review_head(s, task_id, finding_id)?;
        let (captured_id, captured_snapshot) = capture_dependency_snapshot(s, task_id, finding_id)?;
        let snapshot_id = supplied_id.unwrap_or(&captured_id).to_owned();
        let snapshot = if has_snapshot_obj {
            input["dependency_snapshot"].clone()
        } else {
            captured_snapshot
        };
        let entries = if let Some(review) = latest_review(s, task_id, finding_id)? {
            let (key, marker) = stale_payload(
                &review["payload"],
                finding_id,
                reason,
                &snapshot_id,
                &snapshot,
                &source_refs,
            );
            if s.store
                .get(RECORDS, &rec_key(task_id, "finding_stale", &key))
                .map_err(db)?
                .is_some()
            {
                Vec::new()
            } else {
                vec![("finding_stale".to_owned(), key, marker)]
            }
        } else {
            Vec::new()
        };
        if commit(
            s,
            task_id,
            &entries,
            command_id,
            &Value::Null,
            finding_id,
            &head,
            None,
        )? {
            if !entries.is_empty() {
                let _ = super::coordination::enqueue_wakeup(
                    s,
                    task_id,
                    &format!("finding_stale:{finding_id}:{snapshot_id}"),
                    "finding_invalidated",
                    &format!("发现 `{finding_id}` 依赖证据已失效，需重新判断"),
                    reason,
                    json!({
                        "finding_id": finding_id,
                        "reason": reason,
                        "snapshot_id": snapshot_id,
                        "source_refs": source_refs,
                    }),
                );
            }
            return Ok(Value::Null);
        }
    }
}

pub(super) fn apply(
    s: &AppState,
    task_id: &str,
    kind: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    if let Some(output) = marker_output(s, command_id)? {
        return Ok(output);
    }
    let input = body.get("payload").unwrap_or(body);
    match kind {
        "src.finding.claim.create" => create_claim(s, task_id, input, command_id),
        "src.finding.review.record" | "src.review.record" => {
            record_review(s, task_id, input, command_id)
        }
        "src.finding.invalidate" => invalidate(s, task_id, input, command_id),
        "src.finding.deduplicate" => dedup::deduplicate(s, task_id, input, command_id),
        _ => Err(failure(
            "capability.unsupported",
            "unsupported Finding command",
        )),
    }
}

pub(super) fn claims(s: &AppState, task_id: &str, input: &Value) -> Api<Value> {
    Ok(json!(
        recorded(s, task_id, "finding_claim", require(input, "finding_id")?)?
            .into_iter()
            .map(|row| row["payload"].clone())
            .collect::<Vec<_>>()
    ))
}

pub(super) fn claim(s: &AppState, task_id: &str, input: &Value) -> Api<Value> {
    let finding_id = require(input, "finding_id")?;
    let claim_id = require(input, "claim_id")?;
    claim_version(s, task_id, finding_id, claim_id, input["version"].as_u64())
}

pub(super) fn reviews(s: &AppState, task_id: &str, input: &Value) -> Api<Value> {
    Ok(json!(
        recorded(s, task_id, "finding_review", require(input, "finding_id")?)?
            .into_iter()
            .map(|row| row["payload"].clone())
            .collect::<Vec<_>>()
    ))
}

pub(super) fn deduplications(s: &AppState, task_id: &str, input: &Value) -> Api<Value> {
    Ok(json!(
        recorded(s, task_id, "finding_dedup", require(input, "finding_id")?)?
            .into_iter()
            .map(|row| row["payload"].clone())
            .collect::<Vec<_>>()
    ))
}

pub(super) fn status(s: &AppState, task_id: &str, input: &Value) -> Api<Value> {
    let finding_id = require(input, "finding_id")?;
    let _ = staleness::refresh_staleness(s, task_id, finding_id, input.get("dependencies"))?;
    let Some(review) = latest_review(s, task_id, finding_id)? else {
        return Ok(
            json!({"finding_id":finding_id,"state":"unreviewed","review":null,"stale":false}),
        );
    };
    let newer: Vec<_> = recorded(s, task_id, "finding_stale", finding_id)?
        .into_iter()
        .filter(|row| {
            row["payload"]["cleared"] != true
                && (row["payload"]["review_id"] == review["payload"]["review_id"]
                    || row["created_at"].as_str().unwrap_or("")
                        > review["created_at"].as_str().unwrap_or(""))
        })
        .map(|row| row["payload"].clone())
        .collect();
    let stale = !newer.is_empty();
    Ok(json!({"finding_id":finding_id,"review":review["payload"],
        "state":if stale {json!("stale")} else {review["payload"]["readiness"].clone()},
        "stale":stale,"stale_reasons":newer.iter().map(|row|row["reason"].clone()).collect::<Vec<_>>(),
        "stale_refs":newer}))
}
