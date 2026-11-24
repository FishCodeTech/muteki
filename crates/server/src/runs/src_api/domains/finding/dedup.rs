//! Finding duplicate decisions against the readable frozen-report history.
use super::super::task;
use super::*;

fn frozen_history(
    s: &AppState,
    task_id: &str,
    history_task_ids: &[Value],
) -> Api<Option<Vec<Value>>> {
    let principal = task(s, task_id)?["binding"]["principal_id"].clone();
    let mut history = Vec::new();
    let mut visited = std::collections::HashSet::new();
    for candidate in history_task_ids {
        let candidate = candidate.as_str().ok_or_else(|| {
            failure(
                "src.invalid_request",
                "history_task_ids must contain Task IDs",
            )
        })?;
        if !visited.insert(candidate.to_owned()) {
            continue;
        }
        let Ok(binding) = task(s, candidate) else {
            return Ok(None);
        };
        if binding["binding"]["principal_id"] != principal {
            return Ok(None);
        }
        for kind in ["report", "frozen_report", "report_version"] {
            for row in records(s, candidate, kind)? {
                let report = &row["payload"];
                let frozen = report["frozen"] == true
                    || report["frozen_at"].as_str().is_some_and(|v| !v.is_empty())
                    || (report["report_id"].as_str().is_some_and(|v| !v.is_empty())
                        && report["report_seq"].as_u64().is_some_and(|v| v > 0));
                if frozen {
                    history.push(json!({"task_id":candidate,"kind":kind,
                        "record_id":row["record_id"],
                        "report_id":report["report_id"].as_str().filter(|v| !v.is_empty())
                            .unwrap_or(row["record_id"].as_str().unwrap_or("")),
                        "report":report}));
                }
            }
        }
    }
    Ok(Some(history))
}

fn normalized_route_sig(raw: &str) -> String {
    let trimmed = raw.trim();
    let path = reqwest::Url::parse(trimmed)
        .map(|u| u.path().to_owned())
        .unwrap_or_else(|_| trimmed.split('?').next().unwrap_or(trimmed).to_owned());
    let parts = path
        .split('/')
        .filter(|seg| !seg.is_empty())
        .map(|seg| {
            if seg.bytes().all(|b| b.is_ascii_digit()) {
                "{int}"
            } else if seg.len() == 36 && seg.bytes().filter(|b| *b == b'-').count() == 4 {
                "{uuid}"
            } else if seg.len() >= 16 && seg.bytes().all(|b| b.is_ascii_hexdigit()) {
                "{hex}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>();
    format!("/{}", parts.join("/"))
}

fn strong_signature(candidate: &serde_json::Map<String, Value>) -> String {
    let vclass = candidate
        .get("vuln_class")
        .or_else(|| candidate.get("finding_class"))
        .or_else(|| candidate.get("root_cause"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let route_raw = candidate
        .get("normalized_route")
        .or_else(|| candidate.get("route"))
        .or_else(|| candidate.get("endpoint"))
        .or_else(|| candidate.get("attack_path"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let param = candidate
        .get("param_key")
        .or_else(|| candidate.get("parameter"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    format!("{vclass}|{}|{param}", normalized_route_sig(route_raw))
}

pub(super) fn deduplicate(
    s: &AppState,
    task_id: &str,
    input: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    let finding_id = require(input, "finding_id")?;
    let candidate = input["candidate"]
        .as_object()
        .ok_or_else(|| failure("src.invalid_request", "candidate must be an object"))?;
    for (field, code, message) in [
        (
            "root_cause",
            "dedup_root_cause_required",
            "candidate root cause is required",
        ),
        (
            "trust_boundary",
            "dedup_trust_boundary_required",
            "candidate trust boundary is required",
        ),
        (
            "attack_path",
            "dedup_attack_path_required",
            "candidate attack path is required",
        ),
        (
            "fix_point",
            "dedup_fix_point_required",
            "candidate fix point is required",
        ),
    ] {
        if candidate
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(|v| v.trim().is_empty())
        {
            return Err(failure(code, message));
        }
    }
    if !candidate.contains_key("impact") {
        return Err(failure(
            "dedup_impact_required",
            "candidate proven and conditional impact must be explicit",
        ));
    }
    let candidate_value = Value::Object(candidate.clone());
    if evidence(s, task_id, &candidate_value, "evidence_refs")?.is_empty() {
        return Err(failure(
            "evidence_required",
            "dedup requires selected candidate evidence",
        ));
    }
    let assessments = values(input, "assessments")?;
    if assessments.iter().any(|v| !v.is_object()) {
        return Err(failure(
            "src.invalid_request",
            "assessments must contain objects",
        ));
    }
    let history_task_ids = if input["history_task_ids"].is_null() {
        vec![json!(task_id)]
    } else {
        texts(input, "history_task_ids")?
    };
    let reasons = texts(input, "reasons")?;
    let mut history_complete = input["history_complete"].as_bool().unwrap_or(true);
    let mut decision_reasons = reasons.clone();
    let mut outcome = "undetermined";
    let mut comparisons = Vec::new();
    let mut history_refs = Vec::new();
    if !history_complete {
        decision_reasons.push(json!("历史检索缺页、水位落后或来源不可读，重复性未知"));
    } else {
        let history = frozen_history(s, task_id, &history_task_ids)?;
        history_complete = history.is_some();
        let mut by_id = Vec::<(String, Value)>::new();
        if let Some(rows) = history.as_ref() {
            for row in rows {
                let report_id = row["report_id"].as_str().unwrap_or("").to_owned();
                if let Some((_, existing)) = by_id.iter_mut().find(|(id, _)| id == &report_id) {
                    *existing = row.clone();
                } else {
                    by_id.push((report_id, row.clone()));
                }
            }
        }
        let mut supplied = Vec::<(String, Value)>::new();
        for assessment in &assessments {
            let report_id = assessment["report_id"].as_str().unwrap_or("").to_owned();
            if let Some((_, existing)) = supplied.iter_mut().find(|(id, _)| id == &report_id) {
                *existing = assessment.clone();
            } else {
                supplied.push((report_id, assessment.clone()));
            }
        }
        if supplied
            .iter()
            .any(|(id, _)| !by_id.iter().any(|(known, _)| known == id))
        {
            return Err(failure(
                "dedup_history_mismatch",
                "assessment references a non-frozen or unreadable report",
            ));
        }
        if history.is_none() {
            decision_reasons.push(json!("历史来源不可读或权限无法确认，重复性未知"));
        } else if by_id.is_empty() {
            outcome = "independent";
            decision_reasons.push(json!("当前可读冻结报告历史完整，未发现可扩充的既有修复点"));
        } else if supplied.len() != by_id.len() {
            decision_reasons.push(json!("部分冻结报告缺少完整比较，重复性未知"));
        } else {
            for (report_id, row) in &by_id {
                let assessment = supplied
                    .iter()
                    .find(|(id, _)| id == report_id)
                    .map(|(_, v)| v)
                    .expect("matching assessment set");
                let report_task = row["task_id"].as_str().unwrap_or("");
                let comparison_refs = evidence(s, report_task, assessment, "evidence_refs")?;
                let complete =
                    assessment["comparison_complete"] == true && !comparison_refs.is_empty();
                let same_root = assessment["same_root_cause"].as_bool();
                let same_fix = assessment["same_fix_point"].as_bool();
                if !complete || same_root.is_none() || same_fix.is_none() {
                    comparisons.push(json!({"report_id":report_id,"outcome":"undetermined",
                        "reason":assessment["reason"].as_str().filter(|v| !v.is_empty())
                            .unwrap_or("比较字段或证据不完整")}));
                    continue;
                }
                let result = if same_root == Some(true) && same_fix == Some(true) {
                    "extend_existing"
                } else {
                    "independent"
                };
                comparisons.push(json!({"report_id":report_id,"outcome":result,
                    "same_root_cause":same_root,"same_fix_point":same_fix,
                    "reason":assessment["reason"].as_str().unwrap_or(""),
                    "new_impact":assessment["new_impact"]}));
                history_refs.push(json!({"task_id":report_task,"record_id":row["record_id"],
                    "report_id":report_id}));
            }
            if comparisons
                .iter()
                .any(|row| row["outcome"] == "extend_existing")
            {
                if comparisons
                    .iter()
                    .all(|row| row["outcome"] != "undetermined")
                {
                    outcome = "extend_existing";
                }
            } else if comparisons
                .iter()
                .all(|row| row["outcome"] == "independent")
            {
                outcome = "independent";
            }
            decision_reasons.push(json!(
                "同根因且同修复点才扩充既有报告；不同根因或独立修复点保持独立，标题不参与判定"
            ));
        }
    }
    let sig = strong_signature(candidate);
    let mut candidate_queue = Vec::new();
    for hist_task in history_task_ids.iter().filter_map(Value::as_str) {
        for prev in records(s, hist_task, "finding_dedup").unwrap_or_default() {
            if prev["payload"]["strong_signature"] == sig && prev["payload"]["finding_id"] != finding_id {
                candidate_queue.push(json!({
                    "task_id": hist_task,
                    "finding_id": prev["payload"]["finding_id"],
                    "decision_id": prev["payload"]["decision_id"],
                    "strong_signature": sig,
                    "reason": "exact_strong_signature_collision_queued_for_review",
                }));
            }
        }
    }
    loop {
        if let Some(output) = marker_output(s, command_id)? {
            return Ok(output);
        }
        let head = review_head(s, task_id, finding_id)?;
        let decision_id = new_id("sdedup");
        let decision = json!({"schema_version":1,"decision_id":decision_id,"task_id":task_id,
            "finding_id":finding_id,"outcome":outcome,"strong_signature":sig,"candidate_queue":candidate_queue,"candidate":candidate_value,
            "comparisons":comparisons,"history_refs":history_refs,"history_complete":history_complete,
            "reasons":decision_reasons,"created_at":now()});
        if commit(
            s,
            task_id,
            &[("finding_dedup".to_owned(), decision_id, decision.clone())],
            command_id,
            &decision,
            finding_id,
            &head,
            None,
        )? {
            return Ok(decision);
        }
    }
}
