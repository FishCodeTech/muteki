//! SRC runtime observations and dataflow records.
use super::*;

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

fn defaulted(body: &Value, name: &str, default: Value) -> Value {
    body.get(name)
        .filter(|value| truthy(value))
        .cloned()
        .unwrap_or(default)
}

fn save(
    s: &AppState,
    id: &str,
    kind: &str,
    payload: Value,
    command_id: Option<&str>,
) -> Api<Value> {
    task(s, id)?;
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_OBSERVATION_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    let record_id = format!("{}-{}", kind.replace('_', "-"), Uuid::new_v4().simple());
    let created_at = now();
    let row = json!({"record_id":record_id,"kind":kind,"task_id":id,"revision":1,
        "parent_id":"","natural_key":record_id,"generation":0,
        "created_at":created_at,"updated_at":created_at,"payload":payload});
    let document = json!({"version":1,"values":row});
    let mut expected = vec![(RECORDS, record_id.as_str(), None)];
    let mut writes = vec![(RECORDS, record_id.as_str(), &document)];
    let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":row}));
    if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
        expected.push((RESEARCH_OBSERVATION_APPLIED, command_id, None));
        writes.push((RESEARCH_OBSERVATION_APPLIED, command_id, marker));
    }
    if s.store.put_many_if(&expected, &writes).map_err(db)? {
        return Ok(row);
    }
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_OBSERVATION_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    Err(Error::conflict("SRC research observation revision changed"))
}

pub(super) fn register_runtime(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    let page_url = field(body, "page_url")?;
    let action = field(body, "action")?;
    let action_type = action
        .as_object()
        .ok_or_else(|| Error::invalid("action must be an object"))?
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let exchange_ids = field(body, "exchange_ids")?;
    let identity_ref = field(body, "identity_ref")?;
    let identity_revision = field(body, "identity_revision")?;
    let browser_generation = field(body, "browser_generation")?;
    let evidence_refs = field(body, "evidence_refs")?;
    if !truthy(&exchange_ids) && !truthy(&evidence_refs) {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "runtime_evidence_missing",
            "runtime observation requires request or page evidence",
        ));
    }
    let active_injection = action_type == "evaluate";
    let observed_at = body
        .get("observed_at")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| json!(now()));
    let payload = json!({"evidence_type":"Observation","evidence_layer":"runtime_observed",
        "page_url":page_url,"action":action,"exchange_ids":exchange_ids,
        "identity_ref":identity_ref,"identity_revision":identity_revision,
        "browser_generation":browser_generation,"evidence_refs":evidence_refs,
        "dom_state":defaulted(body,"dom_state",json!({})),
        "storage_refs":defaulted(body,"storage_refs",json!([])),
        "script_calls":defaulted(body,"script_calls",json!([])),
        "execution_signal":defaulted(body,"execution_signal",json!({})),
        "execution_provenance":if active_injection{"active_injection"}else{"page_observation"},
        "target_page_execution_assessment":if active_injection{"not_target_execution"}else{"not_determined"},
        "observed_at":observed_at,"upgrades_static_candidate":false});
    save(s, id, "runtime_observation", payload, command_id)
}

pub(super) fn ref_exists(s: &AppState, id: &str, reference: &Value) -> Api<bool> {
    let Some(reference) = reference
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(false);
    };
    if s.store.list(RECORDS).map_err(db)?.into_iter().any(|row| {
        let record = &row["values"];
        record["task_id"] == id
            && (record["record_id"] == reference
                || (record["kind"] == "http_exchange"
                    && record["payload"]["exchange_id"] == reference))
    }) {
        return Ok(true);
    }
    let Some(artifact) = s.store.get("src_artifacts", &format!("{id}:{reference}")).map_err(db)? else {
        return Ok(false);
    };
    match verify_ref(s, id, &artifact) {
        Ok(_) => Ok(true),
        Err(error) if error.status == StatusCode::NOT_FOUND => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn register_dataflow(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    let source = field(body, "source")?;
    let transforms = field(body, "transforms")?;
    let sink = field(body, "sink")?;
    let static_refs = field(body, "static_refs")?;
    let runtime_refs = field(body, "runtime_refs")?;
    let normal_control_refs = field(body, "normal_control_refs")?;
    if !source
        .as_object()
        .is_some_and(|value| value.get("location").is_some_and(truthy))
        || !sink
            .as_object()
            .is_some_and(|value| value.get("location").is_some_and(truthy))
    {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "dataflow_location_missing",
            "source and sink require original file locations",
        ));
    }
    let runtime_verified = runtime_refs.as_array().is_some_and(|refs| !refs.is_empty())
        && runtime_refs
            .as_array()
            .unwrap()
            .iter()
            .map(|reference| ref_exists(s, id, reference))
            .collect::<Api<Vec<bool>>>()?
            .into_iter()
            .all(|exists| exists);
    let control_verified = normal_control_refs
        .as_array()
        .is_some_and(|refs| !refs.is_empty())
        && normal_control_refs
            .as_array()
            .unwrap()
            .iter()
            .map(|reference| ref_exists(s, id, reference))
            .collect::<Api<Vec<bool>>>()?
            .into_iter()
            .all(|exists| exists);
    let assessment = defaulted(body, "assessment", json!({}));
    let assessed = assessment
        .as_object()
        .and_then(|value| value.get("status"))
        .filter(|status| truthy(status));
    let status = if runtime_verified {
        assessed
            .map(|status| {
                status
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| status.to_string())
            })
            .unwrap_or_else(|| {
                if control_verified {
                    "validated"
                } else {
                    "candidate"
                }
                .to_owned()
            })
    } else {
        "candidate".to_owned()
    };
    let payload = json!({"evidence_type":"Observation","source":source,
        "transforms":transforms,"sink":sink,"static_refs":static_refs,
        "runtime_refs":runtime_refs,"normal_control_refs":normal_control_refs,
        "frame_origin":body.get("frame_origin").cloned().unwrap_or(json!("")),
        "required_interaction":body.get("required_interaction").cloned().unwrap_or(json!("")),
        "target_version":body.get("target_version").cloned().unwrap_or(json!("")),
        "status":status,"validated_claim":if status=="validated"{"dataflow_observed"}else{"not_validated"},
        "assessment":assessment,"created_at":now()});
    save(s, id, "dataflow", payload, command_id)
}
