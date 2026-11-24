//! SRC research questions and observed comparison experiments.
use super::*;
use std::collections::BTreeSet;

fn field(body: &Value, name: &str) -> Api<Value> {
    body.get(name).cloned().ok_or_else(|| {
        Error::new(
            StatusCode::BAD_REQUEST,
            "query.arguments_invalid",
            format!("missing required argument: {name}"),
        )
    })
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

pub(super) fn question(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
    actor_kind: &str,
) -> Api<Value> {
    let task_row = task(s, id)?;
    let mut actor = body["actor"].as_object().cloned().unwrap_or_default();
    let run_id = actor
        .get("run_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if !run_id.is_empty()
        && !records(s, id, "run_binding")?
            .into_iter()
            .any(|row| row["payload"]["run_id"] == run_id)
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.run_not_bound",
            "Run does not belong to this Task",
        ));
    }
    actor.insert("id".to_owned(), json!(actor_id));
    actor.insert("kind".to_owned(), json!(actor_kind));
    actor.insert(
        "thread_id".to_owned(),
        json!(if run_id.is_empty() {
            task_row["binding"]["thread_id"].as_str().unwrap_or("")
        } else {
            ""
        }),
    );
    actor.insert("run_id".to_owned(), json!(run_id));
    let payload = json!({"evidence_type":"Step","question":field(body,"question")?,
        "actor":actor,"object_context":field(body,"object_context")?,
        "action":field(body,"action")?,"expected_boundary":field(body,"expected_boundary")?,
        "evidence_refs":field(body,"evidence_refs")?,
        "missing_comparisons":field(body,"missing_comparisons")?,
        "source_refs":field(body,"source_refs")?,"updated_at":now(),
        "http_200_is_not_proof":true});
    let parent_id = body["question_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("research-question-{}", Uuid::new_v4().simple()));
    save_research_versioned(
        s,
        id,
        "research_question",
        &parent_id,
        research_base_revision(body)?,
        payload,
        RESEARCH_QUESTION_HEADS,
        RESEARCH_WORKFLOW_APPLIED,
        command_id,
        &[],
    )
}

pub(super) fn experiment(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    task(s, id)?;
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_WORKFLOW_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    let hypotheses = field(body, "hypotheses")?;
    let changed_variables = field(body, "changed_variables")?;
    let baseline_exchange_ids = field(body, "baseline_exchange_ids")?;
    let control_exchange_ids = field(body, "control_exchange_ids")?;
    let experiment_exchange_ids = field(body, "experiment_exchange_ids")?;
    let observation_refs = field(body, "observation_refs")?;
    let limitations = field(body, "limitations")?;
    let target_version = field(body, "target_version")?;
    let identity_refs = field(body, "identity_refs")?;
    let refs = observation_refs
        .as_array()
        .into_iter()
        .flatten()
        .chain(baseline_exchange_ids.as_array().into_iter().flatten())
        .chain(experiment_exchange_ids.as_array().into_iter().flatten());
    let mut readable = true;
    for reference in refs {
        readable &= research_observations::ref_exists(s, id, reference)?;
    }
    let has_evidence = observation_refs
        .as_array()
        .is_some_and(|rows| !rows.is_empty())
        && baseline_exchange_ids
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
        && experiment_exchange_ids
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
        && readable;
    let changed_count = changed_variables.as_array().map_or(0, Vec::len);
    let mut gaps = Vec::new();
    if !has_evidence {
        gaps.push("baseline, experiment and actual readable observations are required");
    }
    if changed_count != 1 {
        gaps.push("experiment changes multiple variables or does not declare a single variable");
    }
    let assessment = body
        .get("assessment")
        .filter(|value| truthy(value))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let assessed = assessment
        .as_object()
        .and_then(|value| value.get("status"))
        .filter(|value| truthy(value));
    let status = if !has_evidence {
        "inconclusive".to_owned()
    } else if let Some(value) = assessed {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string())
    } else if changed_count == 1 {
        "differentiating".to_owned()
    } else {
        "observed_multi_variable".to_owned()
    };
    let payload = json!({"evidence_type":"Observation","hypotheses":hypotheses,
        "changed_variables":changed_variables,"baseline_exchange_ids":baseline_exchange_ids,
        "control_exchange_ids":control_exchange_ids,"experiment_exchange_ids":experiment_exchange_ids,
        "observation_refs":observation_refs,"limitations":limitations,
        "target_version":target_version,"identity_refs":identity_refs,
        "status":status,"gaps":gaps,"assessment":assessment,"created_at":now(),
        "difference_is_not_vulnerability":true});
    save_observation(s, id, "experiment", payload, command_id)
}

fn save_observation(
    s: &AppState,
    id: &str,
    kind: &str,
    payload: Value,
    command_id: Option<&str>,
) -> Api<Value> {
    let record_id = format!("{}-{}", kind.replace('_', "-"), Uuid::new_v4().simple());
    let timestamp = now();
    let record = json!({"record_id":record_id,"kind":kind,"task_id":id,
        "revision":1,"parent_id":"","natural_key":record_id,"generation":0,
        "created_at":timestamp,"updated_at":timestamp,"payload":payload});
    let document = json!({"version":1,"values":record});
    let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":record}));
    let mut expected = vec![(RECORDS, record_id.as_str(), None)];
    let mut writes = vec![(RECORDS, record_id.as_str(), &document)];
    if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
        expected.push((RESEARCH_WORKFLOW_APPLIED, command_id, None));
        writes.push((RESEARCH_WORKFLOW_APPLIED, command_id, marker));
    }
    if s.store.put_many_if(&expected, &writes).map_err(db)? {
        return Ok(record);
    }
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_WORKFLOW_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    Err(Error::conflict("SRC research observation revision changed"))
}

fn readable_refs(s: &AppState, id: &str, references: &Value) -> Api<bool> {
    let Some(rows) = references.as_array().filter(|rows| !rows.is_empty()) else {
        return Ok(false);
    };
    for reference in rows {
        if !research_observations::ref_exists(s, id, reference)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn capability_chain(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    task(s, id)?;
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_WORKFLOW_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    let links = field(body, "links")?;
    let connections = field(body, "connections")?;
    let target_version = field(body, "target_version")?;
    let link_rows = links
        .as_array()
        .ok_or_else(|| Error::invalid("links must be a list"))?;
    let connection_rows = connections
        .as_array()
        .ok_or_else(|| Error::invalid("connections must be a list"))?;
    let mut gaps = Vec::new();
    for (index, link) in link_rows.iter().enumerate() {
        if !truthy(&link["fact_step_refs"]) || !readable_refs(s, id, &link["evidence_refs"])? {
            gaps.push(
                json!({"link":index,"missing":"Fact/Step and actual readable evidence references"}),
            );
        }
        if !truthy(&link["identity_state"]) {
            gaps.push(json!({"link":index,"missing":"identity and state preconditions"}));
        }
    }
    if link_rows.len() > 1 && connection_rows.is_empty() {
        gaps.push(json!({"connection":0,"missing":"multi-link capability chain requires verified connections"}));
    }
    for (index, connection) in connection_rows.iter().enumerate() {
        if !readable_refs(s, id, &connection["evidence_refs"])? {
            gaps.push(
                json!({"connection":index,"missing":"end-to-end connection has not been verified"}),
            );
        }
    }
    let status = if gaps.is_empty() && !link_rows.is_empty() {
        "validated"
    } else {
        "incomplete"
    };
    let payload = json!({"evidence_type":"Observation","links":links,
        "connections":connections,"target_version":target_version,
        "status":status,"gaps":gaps,
        "assessment":body.get("assessment").filter(|value|truthy(value))
            .cloned().unwrap_or_else(||json!({})),"created_at":now(),
        "node_semantics":"references_only_not_a_graph_primitive"});
    save_observation(s, id, "capability_chain", payload, command_id)
}

pub(super) fn precondition_trial(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    task(s, id)?;
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_WORKFLOW_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    let base_conditions = field(body, "base_conditions")?;
    let removed_or_simplified = field(body, "removed_or_simplified")?;
    let result_exchange_ids = field(body, "result_exchange_ids")?;
    let evidence_refs = field(body, "evidence_refs")?;
    let remaining_conditions = field(body, "remaining_conditions")?;
    let outcome = field(body, "outcome")?;
    let target_version = field(body, "target_version")?;
    let outcome_text = outcome.as_str().unwrap_or("");
    if !matches!(
        outcome_text,
        "reproduced" | "not_reproduced" | "inconclusive"
    ) {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "precondition_outcome_invalid",
            "outcome must be reproduced, not_reproduced or inconclusive",
        ));
    }
    if outcome_text != "inconclusive" && (!truthy(&result_exchange_ids) || !truthy(&evidence_refs))
    {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "precondition_evidence_missing",
            "a conclusive precondition trial requires execution and evidence",
        ));
    }
    let conclusion = match outcome_text {
        "not_reproduced" => "required_under_current_target_version",
        "reproduced" => "not_required_under_current_target_version",
        _ => "unknown",
    };
    let payload = json!({"evidence_type":"Observation","base_conditions":base_conditions,
        "removed_or_simplified":removed_or_simplified,"result_exchange_ids":result_exchange_ids,
        "evidence_refs":evidence_refs,"remaining_conditions":remaining_conditions,
        "outcome":outcome,"conclusion":conclusion,"target_version":target_version,
        "created_at":now(),"conclusion_is_environment_bound":true});
    save_observation(s, id, "precondition_trial", payload, command_id)
}

pub(super) fn precondition_summary(s: &AppState, id: &str) -> Api<Value> {
    task(s, id)?;
    let mut required = Vec::new();
    let mut optional = Vec::new();
    let mut unknown = Vec::new();
    let mut target_versions = BTreeSet::new();
    for stored in s.store.list_inserted(RECORDS).map_err(db)? {
        let row = &stored["values"];
        if row["task_id"] != id || row["kind"] != "precondition_trial" {
            continue;
        }
        let payload = &row["payload"];
        if let Some(version) = payload["target_version"].as_str() {
            target_versions.insert(version.to_owned());
        }
        let item = json!({"change":payload["removed_or_simplified"],
            "target_version":payload["target_version"],
            "evidence_refs":payload["evidence_refs"],"trial_id":row["record_id"]});
        match payload["conclusion"].as_str().unwrap_or("") {
            "required_under_current_target_version" => required.push(item),
            "not_required_under_current_target_version" => optional.push(item),
            _ => unknown.push(item),
        }
    }
    Ok(json!({"required_with_evidence":required,
        "not_required_with_evidence":optional,"unknown":unknown,
        "target_versions":target_versions,"speculation_is_not_evidence":true}))
}
