//! Source knowledge projections: candidates, pinned evidence, and version history.
use super::*;

pub(super) fn read(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let publication_id = require(params, "publication_id")?;
    let dataset_id = require(params,"dataset_id")?;
    let dataset = dataset(s, task_id, principal(params), &dataset_id)?;
    let owner = require(&dataset, "owner_task_id")?;
    let mut publication =
        rec(s, owner, "knowledge_publication", publication_id)?["payload"].clone();
    if publication["dataset_id"] != dataset_id {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "policy_denied",
            "publication does not belong to this dataset",
        ));
    }
    let availability = knowledge_availability(
        s,
        &publication,
        params.get("context").filter(|v| v.is_object()),
    );
    match availability["state"].as_str() {
        Some("revoked") => {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "source_revoked",
                availability["reason"]
                    .as_str()
                    .unwrap_or("publication revoked"),
            ));
        }
        Some("unreadable") => {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                availability["reason"].as_str().unwrap_or("source_missing"),
            ));
        }
        _ => {}
    }
    publication["availability"] = availability;
    publication["reference_only"] = json!(true);
    publication["does_not_create_fact"] = json!(true);
    if params["include_bytes"] != false {
        let mut contents = Vec::new();
        for reference in publication["evidence"].as_array().into_iter().flatten() {
            let verified = verify_ref(s, owner, reference)?;
            contents.push(json!({"ref":reference,"encoding":"base64","data":verified["data"],"size":verified["size"]}));
        }
        publication["evidence_bytes"] = json!(contents);
    }
    Ok(publication)
}

pub(super) fn source(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    authorize(s, task_id, principal(params))?;
    let record = rec(
        s,
        task_id,
        require(params, "kind")?,
        require(params, "record_id")?,
    )?;
    if record["payload"]["revoked_at"]
        .as_str()
        .is_some_and(|v| !v.is_empty())
        || record["payload"]["status"] == "revoked"
    {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "source_revoked",
            "source was revoked",
        ));
    }
    Ok(record)
}

pub(super) fn history(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let dataset_id = require(params, "dataset_id")?;
    let entry_id = require(params, "entry_id")?;
    let dataset = dataset(s, task_id, principal(params), dataset_id)?;
    let mut history = Vec::new();
    for mut record in records(
        s,
        require(&dataset, "owner_task_id")?,
        "knowledge_publication",
    )? {
        let publication = &record["payload"];
        if publication["entry_id"] != entry_id || publication["dataset_id"] != dataset_id {
            continue;
        }
        let availability = knowledge_availability(s, publication, None);
        if matches!(
            availability["state"].as_str(),
            Some("revoked" | "unreadable")
        ) {
            for (field, value) in [
                ("entry_snapshot", json!({})),
                ("source_snapshot", json!({})),
                ("evidence", json!([])),
            ] {
                record[field] = value.clone();
                record["payload"][field] = value;
            }
            record.as_object_mut().unwrap().remove("snapshot");
            if let Some(payload) = record["payload"].as_object_mut() {
                payload.remove("snapshot");
            }
        }
        record["availability"] = availability;
        history.push(record);
    }
    Ok(json!(history))
}

fn principal(params: &Value) -> &str {
    params["principal_id"].as_str().unwrap_or("local-user")
}

pub(super) fn authorize(s: &AppState, owner: &str, _principal: &str) -> Api<()> {
    let _ = task(s, owner)?;
    Ok(())
}

fn dataset(s: &AppState, task_id: &str, principal: &str, dataset_id: &str) -> Api<Value> {
    let dataset = find_dataset_record(s, dataset_id)?["payload"].clone();
    check_dataset_acl(s, &dataset, task_id, principal, "read")?;
    Ok(dataset)
}

fn candidate(publication: &Value, availability: &Value) -> Value {
    let mut value = json!({"publication_id":publication["publication_id"],
        "entry_id":publication["entry_id"],"version":publication["version"],
        "kind":publication["kind"],"status":availability["state"],
        "reason":availability["reason"],"reference_only":true});
    if !matches!(
        availability["state"].as_str(),
        Some("revoked" | "unreadable")
    ) {
        for (field, default) in [
            ("title", json!("")),
            ("problem_pattern", json!("")),
            ("business_preconditions", json!({})),
            ("applicability", json!({})),
        ] {
            value[field] = publication["entry_snapshot"]
                .get(field)
                .cloned()
                .unwrap_or(default);
        }
    }
    value
}

pub(super) fn search(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let needle = caseless::default_case_fold_str(params["query"].as_str().unwrap_or(""));
    let kind = params["kind"].as_str().unwrap_or("");
    let context = params.get("context").filter(|v| v.is_object());
    let mut results = Vec::new();
    for stored in s.store.list(RECORDS).map_err(db)? {
        let record = &stored["values"];
        if record["kind"] != "knowledge_dataset" {
            continue;
        }
        let dataset_id = require(&record["payload"], "dataset_id")?;
        let Ok(dataset) = dataset(s, task_id, principal(params), dataset_id) else {
            continue;
        };
        let owner = require(&dataset, "owner_task_id")?;
        for publication in payloads(s, owner, "knowledge_publication")? {
            if publication["dataset_id"] != dataset_id
                || (!kind.is_empty() && publication["kind"] != kind)
            {
                continue;
            }
            let availability = knowledge_availability(s, &publication, context);
            let mut value = candidate(&publication, &availability);
            if !needle.is_empty()
                && !caseless::default_case_fold_str(&value.to_string()).contains(&needle)
            {
                continue;
            }
            let reasons: Vec<String> = [
                "title",
                "problem_pattern",
                "business_preconditions",
                "applicability",
            ]
            .into_iter()
            .filter(|field| {
                !needle.is_empty()
                    && caseless::default_case_fold_str(
                        &value.get(*field).cloned().unwrap_or(json!("")).to_string(),
                    )
                    .contains(&needle)
            })
            .map(|field| format!("matched:{field}"))
            .collect();
            value["dataset_id"] = json!(dataset_id);
            value["match_reasons"] = json!(reasons);
            results.push(value);
        }
    }
    results.sort_by(|a, b| {
        a["entry_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["entry_id"].as_str().unwrap_or(""))
            .then_with(|| {
                b["version"]
                    .as_u64()
                    .unwrap_or(0)
                    .cmp(&a["version"].as_u64().unwrap_or(0))
            })
    });
    Ok(json!(results))
}
