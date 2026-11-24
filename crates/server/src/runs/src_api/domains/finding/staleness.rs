//! Explicit source Finding dependency refresh using structured versioned snapshots.
use super::*;

fn record_tombstoned_or_revoked(s: &AppState, task_id: &str, kind: &str, record_id: &str) -> bool {
    if kind == "asset"
        && rec(s, task_id, "asset_tombstone", record_id).is_ok()
    {
        return true;
    }
    if kind == "run_binding"
        && rec(s, task_id, "run_disposal", record_id)
            .ok()
            .is_some_and(|r| r["payload"]["state"] == "destroyed")
    {
        return true;
    }
    let Ok(row) = rec(s, task_id, kind, record_id) else {
        return true;
    };
    let payload = &row["payload"];
    !payload["revoked_at"].is_null()
        || !payload["destroyed_at"].is_null()
        || !payload["deleted_at"].is_null()
        || matches!(
            payload["status"].as_str(),
            Some("revoked" | "deleted" | "destroyed")
        )
        || matches!(payload["lifecycle"].as_str(), Some("destroyed"))
}

pub(super) fn refresh_staleness(
    s: &AppState,
    task_id: &str,
    finding_id: &str,
    dependencies: Option<&Value>,
) -> Api<bool> {
    let Some(review_row) = latest_review(s, task_id, finding_id)? else {
        return Ok(false);
    };
    let review = &review_row["payload"];
    let mut selected = values(review, "claim_versions")?;
    if selected.is_empty() {
        selected = current_claims(s, task_id, finding_id)?;
    }
    let evidence_snapshot = values(review, "evidence_snapshot")?;
    let selected_dependencies = dependencies
        .filter(|value| value.as_object().is_some_and(|v| !v.is_empty()))
        .unwrap_or(&review["dependencies"]);
    let selected_dependencies = if selected_dependencies.is_null() {
        json!({})
    } else {
        selected_dependencies.clone()
    };

    let current_claims_list = current_claims(s, task_id, finding_id)?.into_iter().filter(|claim|selected.iter().any(|item|item["claim_id"]==claim["claim_id"])).collect::<Vec<_>>();
    let current_snapshot = build_dependency_snapshot(
        s,
        task_id,
        finding_id,
        &current_claims_list,
        &evidence_snapshot,
        &selected_dependencies,
        &[],
        &values(review, "identity_refs").unwrap_or_default(),
        &values(review, "execution_receipt_ids").unwrap_or_default(),
        review["retest_id"].as_str().unwrap_or(""),
    )?;

    if evidence(s, task_id, review, "evidence_snapshot").is_err() {
        let refs: Vec<_> = evidence_snapshot
            .iter()
            .filter_map(|item| item["artifact_id"].as_str().map(|id| json!(id)))
            .collect();
        let snap_id = format!("depsnap:revoked:{}", review["review_id"].as_str().unwrap_or("review"));
        invalidate(
            s,
            task_id,
            &json!({
                "finding_id": finding_id,
                "reason": "source_revoked",
                "dependency_snapshot_id": snap_id,
                "dependency_snapshot": current_snapshot,
                "source_refs": refs,
            }),
            None,
        )?;
        return Ok(true);
    }

    let baseline_snapshot = if review["dependency_snapshot"].is_object() {
        review["dependency_snapshot"].clone()
    } else {
        build_dependency_snapshot(
            s,
            task_id,
            finding_id,
            &selected,
            &evidence_snapshot,
            &review["dependencies"],
            &[],
            &values(review, "identity_refs").unwrap_or_default(),
            &values(review, "execution_receipt_ids").unwrap_or_default(),
            review["retest_id"].as_str().unwrap_or(""),
        )?
    };

    for art in baseline_snapshot["artifacts"].as_array().into_iter().flatten() {
        let art_task = art["task_id"].as_str().unwrap_or(task_id);
        let Some(art_id) = art["artifact_id"].as_str() else {
            continue;
        };
        let Ok((_, saved)) = super::super::artifact_record(
            s,
            art_task,
            &json!({"task_id": art_task, "artifact_id": art_id}),
        ) else {
            let snap_id = format!("depsnap:artifact-revoked:{art_id}");
            invalidate(
                s,
                task_id,
                &json!({
                    "finding_id": finding_id,
                    "reason": "source_revoked",
                    "dependency_snapshot_id": snap_id,
                    "dependency_snapshot": current_snapshot,
                    "source_refs": [art_id],
                }),
                None,
            )?;
            return Ok(true);
        };
        if saved["version"].as_u64().unwrap_or(1) != art["version"].as_u64().unwrap_or(1) {
            let snap_id = format!("depsnap:artifact-version:{art_id}:v{}", saved["version"].as_u64().unwrap_or(1));
            invalidate(
                s,
                task_id,
                &json!({
                    "finding_id": finding_id,
                    "reason": "claim_or_evidence_changed",
                    "dependency_snapshot_id": snap_id,
                    "dependency_snapshot": current_snapshot,
                    "source_refs": [art_id],
                }),
                None,
            )?;
            return Ok(true);
        }
    }

    for rec_dep in baseline_snapshot["records"].as_array().into_iter().flatten() {
        let r_task = rec_dep["task_id"].as_str().unwrap_or(task_id);
        let r_kind = rec_dep["kind"].as_str().unwrap_or("");
        let r_id = rec_dep["record_id"].as_str().unwrap_or("");
        if r_kind.is_empty() || r_id.is_empty() {
            continue;
        }
        if record_tombstoned_or_revoked(s, r_task, r_kind, r_id) {
            let snap_id = format!("depsnap:record-revoked:{r_kind}:{r_id}");
            invalidate(
                s,
                task_id,
                &json!({
                    "finding_id": finding_id,
                    "reason": "source_revoked",
                    "dependency_snapshot_id": snap_id,
                    "dependency_snapshot": current_snapshot,
                    "source_refs": [r_id],
                }),
                None,
            )?;
            return Ok(true);
        }
        let row = rec(s, r_task, r_kind, r_id)?;
        if row["revision"].as_u64().unwrap_or(1) != rec_dep["revision"].as_u64().unwrap_or(1) {
            let snap_id = format!(
                "depsnap:record-revision:{r_kind}:{r_id}:r{}",
                row["revision"].as_u64().unwrap_or(1)
            );
            invalidate(
                s,
                task_id,
                &json!({
                    "finding_id": finding_id,
                    "reason": "dependencies_changed",
                    "dependency_snapshot_id": snap_id,
                    "dependency_snapshot": current_snapshot,
                    "source_refs": [r_id],
                }),
                None,
            )?;
            return Ok(true);
        }
    }

    if current_snapshot["claims"] != baseline_snapshot["claims"] {
        let refs: Vec<_> = current_snapshot["claims"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["record_id"].as_str().map(|id| json!(id)))
            .collect();
        let snap_id = format!("depsnap:claims:{}", refs.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(","));
        invalidate(
            s,
            task_id,
            &json!({
                "finding_id": finding_id,
                "reason": "claim_or_evidence_changed",
                "dependency_snapshot_id": snap_id,
                "dependency_snapshot": current_snapshot,
                "source_refs": refs,
            }),
            None,
        )?;
        return Ok(true);
    }

    if current_snapshot["external_versions"] != baseline_snapshot["external_versions"]
        || current_snapshot["records"] != baseline_snapshot["records"]
    {
        let refs: Vec<_> = recorded(s, task_id, "finding_claim", finding_id)?
            .into_iter()
            .map(|row| row["natural_key"].clone())
            .collect();
        let snap_id = format!(
            "depsnap:ext:{}",
            super::super::browser_runtime::canonical_json(&current_snapshot["external_versions"])
        );
        invalidate(
            s,
            task_id,
            &json!({
                "finding_id": finding_id,
                "reason": "dependencies_changed",
                "dependency_snapshot_id": snap_id,
                "dependency_snapshot": current_snapshot,
                "source_refs": refs,
            }),
            None,
        )?;
        return Ok(true);
    }

    Ok(false)
}
