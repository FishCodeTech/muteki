//! Operator RunBinding commands and their durable RunGateway delivery.
use super::*;
use sha2::{Digest, Sha256};

const OUTBOX: &str = "competition_binding_outbox_live";
const EFFECTS: &str = "platform_effect_receipts";
const VERSIONS: &str = "platform_aggregate_versions";

pub(super) fn is_binding_command(kind: &str) -> bool {
    matches!(kind, "run_binding.redirect" | "run_binding.pause" | "run_binding.resolve")
}

fn conflict(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

fn canonical_request(
    kind: &str,
    hint: Option<&str>,
    payload: &Value,
    expected_version: Option<u64>,
) -> Value {
    let mut clean = payload.as_object().cloned().unwrap_or_default();
    for transient in [
        "command_id",
        "idempotency_key",
        "expected_version",
        "_planned_admission",
        "cursor_scope",
        "actor_id",
        "command_type",
        "kind",
    ] {
        clean.remove(transient);
    }
    if let Some(comp_id) = hint.filter(|s| !s.is_empty()) {
        clean
            .entry("competition_id".to_owned())
            .or_insert_with(|| json!(comp_id));
    }
    json!({
        "command_type": kind,
        "payload": Value::Object(clean),
        "expected_version": expected_version,
    })
}

fn request_fingerprint_for(canonical_req: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(canonical_req).unwrap_or_default())
    )
}

fn same_binding_request(prior:&Value,canonical_req:&Value,_req_fp:&str)->bool{prior["plan"]["canonical_request"].is_object()&&prior["plan"]["canonical_request"]==*canonical_req}

fn find_prior_by_key(
    state: &AppState,
    _idempotency_key: &str,
    admission: &Value,
) -> ApiResult<Option<Value>> {
    let key_id = field(admission, "key_id");
    if !key_id.is_empty()
        && let Some(keyed) = state
            .store
            .get("platform_idempotency_keys", key_id)
            .map_err(ApiError::internal)?
    {
        let prior_id = field(&keyed, "command_id");
        if !prior_id.is_empty() {
            return state
                .store
                .get(COMMANDS, prior_id)
                .map_err(ApiError::internal);
        }
    }
    Ok(None)
}

fn binding_for(state: &AppState, hint: Option<&str>, payload: &Value) -> ApiResult<Value> {
    let binding_id = field(payload, "binding_id");
    let binding = if !binding_id.is_empty() {
        state
            .store
            .get(BINDINGS, binding_id)
            .map_err(ApiError::internal)?
    } else {
        let challenge_id = field(payload, "challenge_id");
        if challenge_id.is_empty() {
            return Err(ApiError::invalid("binding_id or challenge_id is required"));
        }
        state
            .store
            .list(BINDINGS)
            .map_err(ApiError::internal)?
            .into_iter()
            .filter(|item| {
                item["competition_challenge_id"] == challenge_id
                    && hint.is_none_or(|id| item["competition_id"] == id)
                    && matches!(
                        field(item, "state"),
                        "planned"
                            | "creating"
                            | "starting"
                            | "running"
                            | "active"
                            | "local_finished"
                            | "remote_pending"
                            | "rejected"
                            | "resolving"
                            | "paused"
                    )
            })
            .max_by(|left, right| field(left, "created_at").cmp(field(right, "created_at")))
    }
    .ok_or_else(|| ApiError::missing("run binding"))?;
    if hint.is_some_and(|id| binding["competition_id"] != id) {
        return Err(ApiError::missing("run binding"));
    }
    Ok(binding)
}

fn transition_allowed(from: &str, target: &str) -> bool {
    matches!(
        (from, target),
        ("running" | "active", "paused" | "resolving")
            | ("rejected", "resolving")
            | ("paused", "resolving")
    )
}

#[allow(clippy::too_many_arguments)]
fn new_outbox_with_ids(
    outbox_id: &str,
    child_run_command_id: &str,
    competition_id: &str,
    challenge_id: &str,
    binding_id: &str,
    run_id: &str,
    generation: u64,
    kind: &str,
    command_id: &str,
) -> Value {
    json!({
        "outbox_id": outbox_id,
        "command_id": command_id,
        "idempotency_key": command_id,
        "child_run_command_id": child_run_command_id,
        "aggregate_type": "run_binding",
        "aggregate_id": binding_id,
        "event_type": "competition.run_binding.state_changed",
        "destination": "run_gateway",
        "competition_id": competition_id,
        "challenge_id": challenge_id,
        "binding_id": binding_id,
        "run_id": run_id,
        "execution_generation": generation,
        "op": kind,
        "status": "pending",
        "attempts": 0,
        "max_attempts": 8,
        "next_attempt_at": null,
        "last_error": null,
        "created_at": now(),
        "updated_at": now(),
        "delivered_at": null
    })
}

fn new_outbox(
    competition_id: &str,
    challenge_id: &str,
    binding_id: &str,
    run_id: &str,
    generation: u64,
    kind: &str,
    command_id: &str,
) -> Value {
    let outbox_id = id("outbox");
    let run_command = if kind == "run_binding.pause" { "pause" } else if kind=="run_binding.start"{"start"}else { "resolve" };
    let child_run_command_id = format!("{outbox_id}:{run_command}");
    new_outbox_with_ids(
        &outbox_id,
        &child_run_command_id,
        competition_id,
        challenge_id,
        binding_id,
        run_id,
        generation,
        kind,
        command_id,
    )
}

pub(super) async fn command(
    state: &Arc<AppState>,
    hint: Option<&str>,
    kind: &str,
    requested_command_id: &str,
    payload: &Value,
    admission: &Value,
) -> ApiResult<Value> {
    let expected_version = admission["expected_version"]
        .as_u64()
        .or_else(|| payload["expected_version"].as_u64());
    let cursor_scope = payload["cursor_scope"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| admission["cursor_scope"].as_str().filter(|s| !s.is_empty()))
        .unwrap_or("operator:local-user");
    let idempotency_key = field(payload, "idempotency_key");
    let canonical_req = canonical_request(kind, hint, payload, expected_version);
    let computed_fp = request_fingerprint_for(&canonical_req);
    let req_fp = field(&admission["claim"], "request_fingerprint")
        .to_owned()
        .if_empty(&computed_fp);

    if !requested_command_id.is_empty()
        && let Some(previous) = state
            .store
            .get(COMMANDS, requested_command_id)
            .map_err(ApiError::internal)?
    {
        if !same_binding_request(&previous, &canonical_req, &req_fp) {
            return Err(conflict(
                "command.idempotency_conflict",
                "command_id was used with different run_binding content",
            ));
        }
        recover_missing(state, requested_command_id)?;
        let prior = prior_receipt(state, requested_command_id)?
            .ok_or_else(|| ApiError::missing("binding command receipt"))?;
        return Ok(prior);
    }

    if let Some(previous) = find_prior_by_key(state, idempotency_key, admission)? {
        if !same_binding_request(&previous, &canonical_req, &req_fp) {
            return Err(conflict(
                "command.idempotency_conflict",
                "idempotency_key was used with different run_binding content",
            ));
        }
        let prior_id = field(&previous["response"]["receipt"], "command_id");
        recover_missing(state, prior_id)?;
        let prior = prior_receipt(state, prior_id)?
            .ok_or_else(|| ApiError::missing("binding command receipt"))?;
        return Ok(prior);
    }
    if let Some(prior) = planned_key_receipt(state, admission)? {
        let prior_id = field(&prior, "command_id");
        if !prior_id.is_empty() {
            recover_missing(state, prior_id)?;
            return Ok(prior_receipt(state, prior_id)?.unwrap_or(prior));
        }
        return Ok(prior);
    }

    let command_id = if requested_command_id.is_empty() {
        id("cmd")
    } else {
        requested_command_id.to_owned()
    };

    let binding = binding_for(state, hint, payload)?;
    let competition_id = field(&binding, "competition_id").to_owned();
    let _competition = live_competition(state, &competition_id)?;
    let binding_id = field(&binding, "binding_id").to_owned();
    let run_id = field(&binding, "run_id").to_owned();
    let challenge_id = field(&binding, "competition_challenge_id").to_owned();
    let from = field(&binding, "state").to_owned();
    let target = if kind == "run_binding.pause" {
        "paused"
    } else {
        "resolving"
    };
    if !transition_allowed(&from, target) {
        return Err(conflict(
            "competition.binding.illegal_transition",
            format!("{kind} cannot move binding from {from} to {target}"),
        ));
    }

    let version_key = format!("run_binding\0{binding_id}");
    let version = state
        .store
        .get(VERSIONS, &version_key)
        .map_err(ApiError::internal)?;
    let current_version = version
        .as_ref()
        .and_then(|row| row["version"].as_u64())
        .unwrap_or(0);
    if let Some(expected) = expected_version
        && expected != current_version
    {
        return Err(conflict(
            "command.version_conflict",
            format!("Expected version {expected}, current version {current_version}"),
        ));
    }

    let generation = binding["execution_generation"].as_u64().unwrap_or(0);
    if payload["expected_execution_generation"].as_u64().is_some_and(|expected| expected != generation)
        || payload["expected_run_id"].as_str().is_some_and(|expected| expected != run_id) {
        return Err(conflict("competition.binding.stale_execution","The bound Run execution changed before command admission"));
    }
    let next_global_seq = state
        .store
        .list(EVENTS)
        .map_err(ApiError::internal)?
        .iter()
        .filter_map(|event| event["global_seq"].as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    let stream_seq = current_version + 1;
    let event_cursor = crate::operations_api::competition_event_cursor(
        state,
        "run_binding",
        &binding_id,
        next_global_seq,
        cursor_scope,
    )
    .map_err(ApiError::internal)?;

    let outbox = new_outbox(
        &competition_id,
        &challenge_id,
        &binding_id,
        &run_id,
        generation,
        kind,
        &command_id,
    );
    let outbox_id = field(&outbox, "outbox_id").to_owned();
    let child_run_command_id = field(&outbox, "child_run_command_id").to_owned();
    let run_command = if kind == "run_binding.pause" { "pause" } else if kind=="run_binding.start"{"start"}else { "resolve" };
    let eid = effect_id(&outbox);

    let receipt = json!({
        "schema_version": 1,
        "command_id": command_id,
        "receipt_id": id("rcpt"),
        "state": "accepted",
        "run_id": run_id,
        "aggregate": {"type": "run_binding", "id": binding_id},
        "event_cursor": event_cursor,
        "deduplicated": false,
        "submitted_at": now(),
        "completed_at": null,
        "next": ["receipt"],
        "output": {},
        "effect_ids": [],
        "error": null
    });
    let mut updated = binding.clone();
    updated["state"] = json!(target);
    updated["updated_at"] = json!(now());

    let event_from = if from == "running" { "active" } else { &from };
    let plan = json!({
        "canonical_request": canonical_req,
        "expected_version": expected_version,
        "expected_aggregate_version": current_version,
        "source_execution_generation": generation,
        "execution_generation": generation,
        "planned_transition": {
            "from": from,
            "to": target,
            "event_from": event_from,
            "reason": kind,
        },
        "outbox_id": outbox_id,
        "child_run_command_id": child_run_command_id,
        "run_command_id": child_run_command_id,
        "run_command": run_command,
        "cursor_scope": cursor_scope,
        "event_coordinate": {
            "aggregate_type": "run_binding",
            "aggregate_id": binding_id,
            "competition_id": competition_id,
            "global_seq": next_global_seq,
            "stream_seq": stream_seq,
            "event_cursor": event_cursor,
        },
        "competition_id": competition_id,
        "challenge_id": challenge_id,
        "binding_id": binding_id,
        "run_id": run_id,
        "binding_before": binding,
        "binding_after": updated,
        "outbox": outbox,
    });
    let request = json!({
        "command_type": kind,
        "competition_id": competition_id,
        "payload": payload,
    });
    let mut command = json!({
        "request": request,
        "request_fingerprint": req_fp,
        "plan": plan,
        "status": StatusCode::ACCEPTED.as_u16(),
        "response": {"receipt": receipt},
    });
    let event = leases::planned_event(
        &competition_id,
        "run_binding",
        &binding_id,
        "competition.run_binding.state_changed",
        &command_id,
        json!({
            "binding_id": binding_id,
            "run_id": run_id,
            "challenge_id": challenge_id,
            "from": event_from,
            "to": target,
            "reason": kind,
            "execution_generation": generation
        }),
    );
    let effect = initial_effect(&outbox, &command);

    let existing_claim = state
        .store
        .get("platform_command_claims", &command_id)
        .map_err(ApiError::internal)?;
    let mut claim_record = existing_claim
        .clone()
        .or_else(|| admission.get("claim").filter(|v| v.is_object()).cloned())
        .unwrap_or_else(|| {
            json!({
                "command_id": command_id,
                "request_fingerprint": req_fp,
                "state": "accepted",
                "created_at": now(),
                "updated_at": now(),
            })
        });
    if claim_record.get("request_fingerprint").is_none() {
        claim_record["request_fingerprint"] = json!(req_fp);
    }
    command["request_fingerprint"] = claim_record["request_fingerprint"].clone();

    let key_id = field(admission, "key_id")
        .to_owned()
        .if_empty(&if idempotency_key.is_empty() {
            String::new()
        } else {
            format!("run_binding\0{idempotency_key}")
        });
    let existing_key = if key_id.is_empty() {
        None
    } else {
        state
            .store
            .get("platform_idempotency_keys", &key_id)
            .map_err(ApiError::internal)?
    };
    let mut keyed_record = admission
        .get("keyed")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| {
            json!({
                "command_id": command_id,
                "idempotency_key": idempotency_key,
                "semantic_fingerprint": computed_fp,
                "created_at": now(),
            })
        });
    keyed_record["command_id"] = json!(command_id);
    if keyed_record.get("idempotency_key").is_none() && !idempotency_key.is_empty() {
        keyed_record["idempotency_key"] = json!(idempotency_key);
    }
    if keyed_record.get("semantic_fingerprint").is_none() {
        keyed_record["semantic_fingerprint"] = json!(computed_fp);
    }

    let mut expected = vec![
        (COMMANDS, command_id.as_str(), None),
        (BINDINGS, binding_id.as_str(), Some(&binding)),
        (OUTBOX, outbox_id.as_str(), None),
        (EFFECTS, eid.as_str(), None),
        (VERSIONS, version_key.as_str(), version.as_ref()),
        (
            "platform_command_claims",
            command_id.as_str(),
            existing_claim.as_ref(),
        ),
    ];
    let mut records = vec![
        (COMMANDS, command_id.as_str(), &command),
        (BINDINGS, binding_id.as_str(), &updated),
        (OUTBOX, outbox_id.as_str(), &outbox),
        (EFFECTS, eid.as_str(), &effect),
        (
            "platform_command_claims",
            command_id.as_str(),
            &claim_record,
        ),
    ];
    if !key_id.is_empty() && existing_key.is_none() {
        expected.push(("platform_idempotency_keys", key_id.as_str(), None));
        records.push(("platform_idempotency_keys", key_id.as_str(), &keyed_record));
    }
    if !put_records_with_events(state, &competition_id, &expected, &records, &[event])? {
        if let Some(prior) = state
            .store
            .get(COMMANDS, &command_id)
            .map_err(ApiError::internal)?
        {
            if !same_binding_request(&prior, &canonical_req, &req_fp) {
                return Err(conflict(
                    "command.idempotency_conflict",
                    "command_id was used with different run_binding content",
                ));
            }
            return prior_receipt(state, &command_id)?
                .ok_or_else(|| ApiError::missing("binding command receipt"));
        }
        if let Some(prior) = planned_key_receipt(state, admission)? {
            return Ok(prior);
        }
        return Err(conflict(
            "competition.binding.concurrent_update",
            "Run binding changed while scheduling its gateway effect",
        ));
    }
    Ok(receipt)
}

fn due(outbox: &Value) -> bool {
    outbox["next_attempt_at"]
        .as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .is_none_or(|at| at <= chrono::Utc::now())
}

fn effect_id(outbox: &Value) -> String {
    format!("effect-{}", field(outbox, "outbox_id"))
}

fn initial_effect(outbox: &Value, command: &Value) -> Value {
    let eid = effect_id(outbox);
    let command_id = field(outbox, "command_id");
    let binding_id = field(outbox, "binding_id");
    let status = field(outbox, "status");
    json!({"schema_version":1,"effect_id":eid,"command_id":command_id,
        "acceptance_receipt_id":command["response"]["receipt"]["receipt_id"],
        "domain":"competition","state":match status {
            "processing" => "running", "delivered" => "completed",
            "cancelled" => "cancelled", "dead_letter" => "dead_letter", _ => "waiting"},
        "aggregate":{"type":"run_binding","id":binding_id},
        "correlation_id":command_id,"outbox_id":outbox["outbox_id"],
        "destination":"run_gateway","idempotency_key":outbox["idempotency_key"],
        "attempts":outbox["attempts"],"max_attempts":8,
        "retry_after":outbox["next_attempt_at"],
        "object_links":{"aggregate":format!("run_binding:{binding_id}")},
        "output":{},"error":null,"created_at":now(),"updated_at":now(),
        "completed_at":if matches!(status,"delivered" | "cancelled" | "dead_letter") {
            json!(now()) } else {Value::Null}})
}

/// Complete the local side of an accepted RunBinding command using its stored plan.
pub(super) fn recover_missing(state: &AppState, command_id: &str) -> ApiResult<()> {
    let Some(command) = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    let kind = field(&command["request"], "command_type");
    if !matches!(
        kind,
        "run_binding.redirect"
            | "run_binding.pause"
            | "run_binding.resolve"
    ) || command["response"]["receipt"]["state"] != "accepted"
    {
        return Ok(());
    }
    let existing_outbox = state
        .store
        .list(OUTBOX)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|row| row["command_id"] == command_id);

    let plan = &command["plan"];
    let event = state
        .store
        .list(EVENTS)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|row| {
            row["command_id"] == command_id
                && row["aggregate_type"] == "run_binding"
                && row["event_type"] == "competition.run_binding.state_changed"
        });

    if !plan.is_object() {
        return Err(conflict(
            "competition.binding.plan_missing",
            "Accepted RunBinding command has no stored plan",
        ));
    }

    let binding_id = field(plan, "binding_id").to_owned();
    let competition_id = field(plan, "competition_id").to_owned();
    let challenge_id = field(plan, "challenge_id").to_owned();
    let run_id = field(plan, "run_id").to_owned();
    let raw_from = field(&plan["planned_transition"], "from");
    let from = if raw_from == "active" {
        "running"
    } else {
        raw_from
    };
    let target = field(&plan["planned_transition"], "to").to_owned();
    let generation = plan["source_execution_generation"]
        .as_u64()
        .or_else(|| plan["execution_generation"].as_u64())
        .unwrap_or(0);

    let Some(binding) = state
        .store
        .get(BINDINGS, &binding_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::missing("run binding"));
    };
    let current_state = field(&binding, "state");
    let current_state_norm = if current_state == "active" {
        "running"
    } else {
        current_state
    };
    if (current_state_norm != from && current_state != target)
        || binding["execution_generation"].as_u64().unwrap_or(0) != generation
    {
        return Err(conflict(
            "competition.binding.outcome_unknown",
            "RunBinding changed after its accepted command; inspect before replay",
        ));
    }

    let outbox = existing_outbox.as_ref().cloned()
        .or_else(||plan["outbox"].as_object().map(|_|plan["outbox"].clone()))
        .ok_or_else(||conflict("competition.binding.plan_missing",
            "Accepted RunBinding command has no stored outbox plan"))?;
    let outbox_id = field(&outbox, "outbox_id").to_owned();
    let effect = initial_effect(&outbox, &command);
    let eid = field(&effect, "effect_id").to_owned();
    let existing_effect = state
        .store
        .get(EFFECTS, &eid)
        .map_err(ApiError::internal)?;

    let mut updated_command = command.clone();
    if updated_command["response"]["receipt"]["event_cursor"].is_null() {
        let cursor_scope = field(plan, "cursor_scope")
            .to_owned()
            .if_empty("operator:local-user");
        let saved_cursor = field(&plan["event_coordinate"], "event_cursor");
        if !saved_cursor.is_empty() {
            updated_command["response"]["receipt"]["event_cursor"] = json!(saved_cursor);
        } else if let Some(seq) = plan["event_coordinate"]["global_seq"].as_u64()
        {
            let cursor = crate::operations_api::competition_event_cursor(
                state,
                "run_binding",
                &binding_id,
                seq,
                &cursor_scope,
            )
            .map_err(ApiError::internal)?;
            updated_command["response"]["receipt"]["event_cursor"] = json!(cursor);
        }
    }

    if existing_outbox.is_some()
        && existing_effect.is_some()
        && current_state == target
        && updated_command == command
    {
        return Ok(());
    }

    let mut updated_binding = binding.clone();
    if current_state != target {
        updated_binding["state"] = json!(target);
        updated_binding["updated_at"] = json!(now());
    }

    let mut expected = vec![
        (COMMANDS, command_id, Some(&command)),
        (BINDINGS, binding_id.as_str(), Some(&binding)),
    ];
    let mut records = vec![
        (COMMANDS, command_id, &updated_command),
        (BINDINGS, binding_id.as_str(), &updated_binding),
    ];
    if existing_outbox.is_none() {
        expected.push((OUTBOX, outbox_id.as_str(), None));
        records.push((OUTBOX, outbox_id.as_str(), &outbox));
    }
    if existing_effect.is_none() {
        expected.push((EFFECTS, eid.as_str(), None));
        records.push((EFFECTS, eid.as_str(), &effect));
    }

    let events = if event.is_none() {
        let event_from = if from == "running" { "active" } else { from };
        vec![leases::planned_event(
            &competition_id,
            "run_binding",
            &binding_id,
            "competition.run_binding.state_changed",
            command_id,
            json!({
                "binding_id": binding_id,
                "run_id": run_id,
                "challenge_id": challenge_id,
                "from": event_from,
                "to": target,
                "reason": kind,
                "execution_generation": generation,
            }),
        )]
    } else {
        Vec::new()
    };

    let committed = if events.is_empty() {
        state
            .store
            .put_many_if(&expected, &records)
            .map_err(ApiError::internal)?
    } else {
        put_records_with_events(state, &competition_id, &expected, &records, &events)?
    };
    if !committed {
        if state
            .store
            .list(OUTBOX)
            .map_err(ApiError::internal)?
            .iter()
            .any(|row| row["command_id"] == command_id)
        {
            return Ok(());
        }
        return Err(conflict(
            "competition.binding.concurrent_update",
            "RunBinding changed while recovering its gateway effect",
        ));
    }
    Ok(())
}

fn effect_event(outbox: &Value, event_type: &str, effect_id: &str, details: Value) -> Value {
    let mut payload = json!({"outbox_id":outbox["outbox_id"],"destination":"run_gateway",
        "aggregate_type":"run_binding","aggregate_id":outbox["binding_id"]});
    if let (Some(payload), Some(details)) = (payload.as_object_mut(), details.as_object()) {
        payload.extend(details.clone());
    }
    leases::planned_event(
        field(outbox, "competition_id"),
        "effect",
        effect_id,
        event_type,
        field(outbox, "command_id"),
        payload,
    )
}

fn claim(state: &AppState, outbox: &Value) -> ApiResult<Option<Value>> {
    let outbox_id = field(outbox, "outbox_id");
    let Some(before) = state
        .store
        .get(OUTBOX, outbox_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(None);
    };
    if !matches!(field(&before, "status"), "pending" | "failed" | "awaiting_confirmation") || !due(&before) {
        return Ok(None);
    }
    let command_id = field(&before, "command_id");
    let Some(command) = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::missing("binding command receipt"));
    };
    let eid = effect_id(&before);
    let prior_effect = state.store.get(EFFECTS, &eid).map_err(ApiError::internal)?;
    let effect = prior_effect
        .clone()
        .unwrap_or_else(|| initial_effect(&before, &command));
    let mut processing = before.clone();
    processing["status"] = json!("processing");
    processing["attempts"] = json!(before["attempts"].as_u64().unwrap_or(0) + 1);
    processing["next_attempt_at"] = Value::Null;
    processing["updated_at"] = json!(now());
    let mut running_effect = effect.clone();
    running_effect["state"] = json!("running");
    running_effect["attempts"] = processing["attempts"].clone();
    running_effect["retry_after"] = Value::Null;
    running_effect["error"] = Value::Null;
    running_effect["updated_at"] = json!(now());
    let mut running_command = command.clone();
    running_command["response"]["receipt"]["state"] = json!("running");
    running_command["response"]["receipt"]["effect_ids"] = json!([eid]);
    running_command["response"]["receipt"]["output"] = json!({
        "outbox_ids":[outbox_id],"effect_ids":[eid],"effect_state":"running"});
    let event = effect_event(
        &processing,
        "competition.effect.running",
        &eid,
        json!({"attempt":processing["attempts"]}),
    );
    let expected = [
        (OUTBOX, outbox_id, Some(&before)),
        (COMMANDS, command_id, Some(&command)),
        (EFFECTS, eid.as_str(), prior_effect.as_ref()),
    ];
    let records = [
        (OUTBOX, outbox_id, &processing),
        (COMMANDS, command_id, &running_command),
        (EFFECTS, eid.as_str(), &running_effect),
    ];
    if !put_records_with_events(
        state,
        field(&processing, "competition_id"),
        &expected,
        &records,
        &[event],
    )? {
        return Ok(None);
    }
    Ok(Some(processing))
}

fn retry_time(attempts: u64) -> String {
    let base = (0.5 * 2_f64.powi(attempts.saturating_sub(1).min(12) as i32)).min(300.0);
    let jitter = uuid::Uuid::new_v4().as_u128() as u64 as f64 / u64::MAX as f64;
    (chrono::Utc::now()
        + chrono::Duration::milliseconds(((base + jitter * base * 0.2) * 1000.0) as i64))
    .to_rfc3339()
}

fn failure_with_detail(
    state: &AppState,
    claimed: &Value,
    status: StatusCode,
    detail_message: Option<String>,
    awaiting_confirmation: bool,
) -> ApiResult<()> {
    let outbox_id = field(claimed, "outbox_id");
    let Some(before) = state
        .store
        .get(OUTBOX, outbox_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if before["status"] != "processing" {
        return Ok(());
    }
    let attempts = before["attempts"].as_u64().unwrap_or(0);
    let retryable = status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS;
    let terminal = !awaiting_confirmation && (!retryable || attempts >= 8);
    let code = if awaiting_confirmation {
        "competition.effect.awaiting_confirmation"
    } else if status == StatusCode::NOT_FOUND {
        "competition.effect.not_found"
    } else {
        "competition.effect.run_gateway_failed"
    };
    let message = detail_message
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("RunGateway returned HTTP {}", status.as_u16()));
    let error = json!({"schema_version":1,"code":code,
        "message":message,
        "category":if status == StatusCode::NOT_FOUND {"not_found"} else {"state"},
        "recovery_hint":if terminal {"inspect the effect and retry manually"}
            else {"retry the effect after the recorded retry window"},
        "correlation_id":field(&before,"command_id"),"retryable":!terminal,"detail":{}});
    let eid = effect_id(&before);
    let Some(effect) = state.store.get(EFFECTS, &eid).map_err(ApiError::internal)? else {
        return Err(ApiError::missing("binding effect receipt"));
    };
    let command_id = field(&before, "command_id");
    let Some(command) = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::missing("binding command receipt"));
    };
    let mut failed = before.clone();
    failed["status"] = json!(if awaiting_confirmation {"awaiting_confirmation"} else if terminal { "dead_letter" } else { "failed" });
    if awaiting_confirmation { failed["delivery_phase"] = json!("confirm"); }
    failed["next_attempt_at"] = if terminal {
        Value::Null
    } else {
        json!(retry_time(if awaiting_confirmation {3} else {attempts}))
    };
    failed["last_error"] = json!(code);
    failed["updated_at"] = json!(now());
    let mut next_effect = effect.clone();
    next_effect["state"] = json!(if terminal { "dead_letter" } else { "waiting" });
    next_effect["retry_after"] = failed["next_attempt_at"].clone();
    next_effect["error"] = error.clone();
    next_effect["updated_at"] = json!(now());
    next_effect["completed_at"] = if terminal { json!(now()) } else { Value::Null };
    let mut next_command = command.clone();
    next_command["response"]["receipt"]["state"] =
        json!(if terminal { "failed" } else { "waiting" });
    next_command["response"]["receipt"]["error"] = error;
    next_command["response"]["receipt"]["completed_at"] =
        if terminal { json!(now()) } else { Value::Null };
    next_command["response"]["receipt"]["output"] = json!({"binding_id":before["binding_id"],
        "run_id":before["run_id"],"state":state.store.get(BINDINGS,field(&before,"binding_id"))
            .map_err(ApiError::internal)?.map(|binding|binding["state"].clone()).unwrap_or(Value::Null),
        "execution_generation":before["execution_generation"],"outbox_ids":[outbox_id],
        "effect_ids":[eid],"effect_state":if terminal {"failed"} else {"waiting"}});
    next_command["status"] = json!(
        crate::operations_api::planned_receipt_status(&next_command["response"]["receipt"]).as_u16()
    );
    let event = effect_event(
        &failed,
        if terminal {
            "competition.effect.dead_letter"
        } else {
            "competition.effect.retry_scheduled"
        },
        &eid,
        json!({"attempt":attempts,"error_code":code,
            "retry_after_seconds":failed["next_attempt_at"].as_str()
                .and_then(|text|chrono::DateTime::parse_from_rfc3339(text).ok())
                .map(|at|(at.with_timezone(&chrono::Utc)-chrono::Utc::now())
                    .num_milliseconds().max(0) as f64 / 1000.0)}),
    );
    let previous_binding=if terminal && matches!(field(&before,"op"), "run_binding.start" | "run_binding.resolve"){state.store.get(BINDINGS,field(&before,"binding_id")).map_err(ApiError::internal)?}else{None};
    let paused_binding=previous_binding.as_ref().map(|row|{let mut next=row.clone();next["state"]=json!("paused");next["updated_at"]=json!(now());next});
    if let Some(binding) = &paused_binding {
        next_command["response"]["receipt"]["output"]["state"] = binding["state"].clone();
    }
    let challenge_id=field(&before,"challenge_id");let previous_queue=if paused_binding.is_some(){state.store.get(QUEUE,challenge_id).map_err(ApiError::internal)?}else{None};
    let held_queue=previous_queue.as_ref().map(|row|{let mut next=row.clone();next["state"]=json!("held");next["not_before"]=json!((chrono::Utc::now()+chrono::Duration::minutes(5)).to_rfc3339());next["admission_decision"]["action"]=json!("hold");next["admission_decision"]["reason"]=json!("run_preflight_failed");next});
    let mut expected=vec![(OUTBOX,outbox_id,Some(&before)),(EFFECTS,eid.as_str(),Some(&effect)),(COMMANDS,command_id,Some(&command))];let mut records=vec![(OUTBOX,outbox_id,&failed),(EFFECTS,eid.as_str(),&next_effect),(COMMANDS,command_id,&next_command)];
    if let(Some(before),Some(after))=(&previous_binding,&paused_binding){expected.push((BINDINGS,field(before,"binding_id"),Some(before)));records.push((BINDINGS,field(before,"binding_id"),after));}
    if let(Some(before),Some(after))=(&previous_queue,&held_queue){expected.push((QUEUE,challenge_id,Some(before)));records.push((QUEUE,challenge_id,after));}
    if !put_records_with_events(state,field(&before,"competition_id"),&expected,&records,&[event])? {
        return Err(conflict("competition.binding.concurrent_update","Binding delivery receipt changed before settlement"));
    }
    Ok(())
}

enum GatewayResult { Confirmed, Pending(String), Rejected(StatusCode,Option<String>) }
async fn inspect_gateway_response(response: Response) -> GatewayResult {
    let status = response.status();
    let bytes = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => return GatewayResult::Rejected(StatusCode::BAD_GATEWAY, Some(error.to_string())),
    };
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(error) => return GatewayResult::Rejected(StatusCode::BAD_GATEWAY,Some(format!("Invalid RunGateway response: {error}"))),
    };
    let receipt_state = field(&body["receipt"], "state");
    let top_state = field(&body, "state");
    let pending = matches!(receipt_state,"unknown"|"partial"|"projection_conflict"|"accepted"|"processing"|"running"|"waiting")
        || matches!(top_state,"unknown"|"partial"|"projection_conflict"|"accepted"|"processing"|"waiting")
        || matches!(field(&body,"status"),"unknown"|"partial"|"projection_conflict");
    if status.is_success() && pending {
        return GatewayResult::Pending(field(&body,"detail").to_owned()
            .if_empty(field(&body["error"],"message")).if_empty("RunGateway is awaiting physical effect confirmation"));
    }
    let has_error = body.get("error").is_some_and(|err| {
        !err.is_null() && (!err.is_string() || !err.as_str().unwrap_or("").trim().is_empty())
    }) || body["receipt"]
        .get("error")
        .is_some_and(|err| !err.is_null());
    let body_failed = body.get("ok") == Some(&Value::Bool(false))
        || matches!(
            receipt_state,
            "failed" | "conflict" | "cancelled" | "dead_letter"
        )
        || matches!(
            top_state,
            "failed" | "conflict" | "cancelled" | "dead_letter"
        )
        || has_error;

    if status.is_success() && !body_failed {
        return GatewayResult::Confirmed;
    }

    let message = field(&body["error"], "message")
        .to_owned()
        .if_empty(body["error"].as_str().unwrap_or(""))
        .if_empty(field(&body["receipt"]["error"], "message"))
        .if_empty(field(&body, "message"))
        .if_empty(field(&body, "detail"))
        .if_empty(&format!(
            "RunGateway returned HTTP {}{}",
            status.as_u16(),
            if body.get("ok") == Some(&Value::Bool(false)) {
                " (ok=false)"
            } else {
                ""
            }
        ));

    let effective_status = if !status.is_success() {
        status
    } else if body["retryable"] == true
        || body["error"]["retryable"] == true
        || body["receipt"]["error"]["retryable"] == true
    {
        StatusCode::BAD_GATEWAY
    } else {
        StatusCode::CONFLICT
    };
    GatewayResult::Rejected(effective_status, Some(message))
}

async fn deliver(state: &Arc<AppState>, candidate: &Value) -> ApiResult<()> {
    let outbox_id = field(candidate, "outbox_id").to_owned();
    let _lock =
        submission_outbox::submission_lock(state, &format!("binding-outbox-{outbox_id}")).await?;
    if !state
        .runs
        .claim_execution_owner(state, field(candidate, "run_id"))
        .map_err(ApiError::internal)?
    {
        return Ok(());
    }
    let Some(claimed) = claim(state, candidate)? else {
        return Ok(());
    };
    let run_id = field(&claimed, "run_id").to_owned();
    let binding_id = field(&claimed, "binding_id").to_owned();
    let old_generation = claimed["execution_generation"].as_u64().unwrap_or(0);
    let operation = field(&claimed, "op");
    let run_command=if operation=="run_binding.pause"{"pause"}else if operation=="run_binding.start"{"start"}else{"resolve"};
    let command_id = field(&claimed, "command_id");
    let stored_command = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?;
    let run_command_id = field(&claimed, "child_run_command_id")
        .to_owned()
        .if_empty(
            stored_command
                .as_ref()
                .map(|cmd| field(&cmd["plan"], "child_run_command_id"))
                .unwrap_or(""),
        )
        .if_empty(&format!("{outbox_id}:{run_command}"));
    let run = state
        .store
        .get("runs", &run_id)
        .map_err(ApiError::internal)?;
    let gateway=if run_command=="start"{
        let mut body=stored_command.as_ref().map(|command|command["plan"]["start_body"].clone()).filter(Value::is_object).ok_or_else(||ApiError::missing("planned Run start body"))?;body["command_id"]=json!(run_command_id);
        inspect_gateway_response(super::super::start_run(State(state.clone()),Path(run_id.clone()),Json(body)).await).await
    }else if let Some(run) = run {
        let current_generation = run["execution_generation"].as_u64().unwrap_or(0);
        let prior_run_command = state
            .store
            .get("run_execution_commands", &run_command_id)
            .map_err(ApiError::internal)?;
        let already_resolved = prior_run_command.as_ref().is_some_and(|record| {
            record["run_id"] == run_id
                && record["action"] == "resolve"
                && record["generation"] == old_generation + 1
                && current_generation == old_generation + 1
        });
        if current_generation != old_generation && !already_resolved {
            GatewayResult::Rejected(StatusCode::CONFLICT,Some(format!(
                "stale run binding: binding generation {old_generation}, run generation {current_generation}")))
        } else if run_command == "pause" && claimed["delivery_phase"] == "confirm" {
            inspect_gateway_response(super::super::get_control_command(State(state.clone()),
                Path((run_id.clone(),run_command_id.clone()))).await).await
        } else if run_command == "pause" {
            let response = super::super::control_run(
                State(state.clone()),
                Path(run_id.clone()),
                Json(json!({
                    "action": "stop",
                    "text": "competition_binding_pause",
                    "command_id": run_command_id,
                    "expected_generation": run["control_generation"]
                })),
            )
            .await;
            inspect_gateway_response(response).await
        } else {
            let response = super::super::resolve_run(
                State(state.clone()),
                Path(run_id.clone()),
                Some(Json({let mut request=stored_command.as_ref().map(|command|command["request"]["payload"].clone()).filter(Value::is_object).unwrap_or_else(||json!({}));request["command_id"]=json!(run_command_id);request["expected_execution_generation"]=run["execution_generation"].clone();request["expected_generation"]=run["control_generation"].clone();if let Some(prior)=state.store.get("run_execution_commands",&run_command_id).map_err(ApiError::internal)?{if prior["request"].is_object(){request=prior["request"].clone();}}if operation=="run_binding.resolve" && request["visit_timebox_s"].is_number(){request["_competition_visit_command"]=json!(command_id);}request})),
            )
            .await;
            inspect_gateway_response(response).await
        }
    } else {
        GatewayResult::Rejected(StatusCode::NOT_FOUND,None)
    };
    let (status,error_detail,awaiting_confirmation) = match gateway {
        GatewayResult::Confirmed => (StatusCode::OK,None,false),
        GatewayResult::Pending(message) => (StatusCode::SERVICE_UNAVAILABLE,Some(message),true),
        GatewayResult::Rejected(status,message) => (status,message,false),
    };
    if !status.is_success() {
        failure_with_detail(state, &claimed, status, error_detail,awaiting_confirmation)?;
        if run_command=="start" && status.is_client_error() && status!=StatusCode::TOO_MANY_REQUESTS{
            if let Some(binding)=state.store.get(BINDINGS,&binding_id).map_err(ApiError::internal)?{if let Some(lease)=binding["lease_id"].as_str().filter(|v|!v.is_empty()){
                if let Err(error)=Box::pin(instance_command(state,field(&claimed,"competition_id"),"instance.stop","",&json!({"lease_id":lease}))).await{tracing::warn!(%lease,code=%error.code,"Failed scheduler start still owns its instance lease");}
            }}
        }
        return Ok(());
    }
    let eid = effect_id(&claimed);
    let Some(before_outbox) = state
        .store
        .get(OUTBOX, &outbox_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if before_outbox["status"] != "processing" {
        return Ok(());
    }
    let Some(effect) = state.store.get(EFFECTS, &eid).map_err(ApiError::internal)? else {
        return Err(ApiError::missing("binding effect receipt"));
    };
    let Some(command) = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::missing("binding command receipt"));
    };
    let Some(binding) = state
        .store
        .get(BINDINGS, &binding_id)
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::missing("run binding"));
    };
    let mut next_binding = binding.clone();
    if (run_command=="resolve" && binding["state"]=="resolving") || (run_command=="start" && matches!(field(&binding,"state"),"planned"|"starting")) {
        next_binding["state"] = json!("running");
        next_binding["execution_generation"] = json!(old_generation + 1);
        next_binding["updated_at"] = json!(now());
    }
    let output = json!({"run_id":run_id,"run_command_id":run_command_id,"state":"completed"});
    let mut delivered = before_outbox.clone();
    delivered["status"] = json!("delivered");
    delivered["last_error"] = Value::Null;
    delivered["delivered_at"] = json!(now());
    delivered["updated_at"] = json!(now());
    let mut next_effect = effect.clone();
    next_effect["state"] = json!("completed");
    next_effect["output"] = output.clone();
    next_effect["error"] = Value::Null;
    next_effect["completed_at"] = json!(now());
    next_effect["updated_at"] = json!(now());
    let mut next_command = command.clone();
    next_command["response"]["receipt"]["state"] = json!("completed");
    next_command["response"]["receipt"]["completed_at"] = json!(now());
    next_command["response"]["receipt"]["error"] = Value::Null;
    next_command["response"]["receipt"]["effect_ids"] = json!([eid]);
    next_command["response"]["receipt"]["output"] = json!({"binding_id":binding_id,
        "run_id":run_id,"state":next_binding["state"],
        "execution_generation":next_binding["execution_generation"],
        "outbox_ids":[outbox_id],"effect_ids":[eid],
        "effect_state":"completed","run_command":output});
    next_command["status"] = json!(StatusCode::OK.as_u16());
    let event = effect_event(
        &delivered,
        "competition.effect.completed",
        &eid,
        json!({"attempt":delivered["attempts"],"output":output}),
    );
    let challenge_id=field(&claimed,"challenge_id");
    let previous_challenge=if matches!(run_command,"start"|"resolve"){state.store.get(CHALLENGES,challenge_id).map_err(ApiError::internal)?}else{None};
    let next_challenge=previous_challenge.as_ref().map(|row|{let mut next=row.clone();if !matches!(field(row,"state"),"candidate_found"|"submitting"|"solved"){next["state"]=json!("running");}next["updated_at"]=json!(now());next});
    let previous_queue=if matches!(run_command,"start"|"resolve"){state.store.get(QUEUE,challenge_id).map_err(ApiError::internal)?}else{None};
    let policy=state.store.get(POLICIES,field(&claimed,"competition_id")).map_err(ApiError::internal)?.unwrap_or(Value::Null);
    let next_queue=previous_queue.as_ref().map(|row|{let mut next=row.clone();next["state"]=json!("dispatching");next["admission_decision"]["action"]=json!("admit");next["admission_decision"]["reason"]=json!("admitted");scheduler::admitted_entry(state,field(&claimed,"competition_id"),&next,&run_id,&binding_id,&policy,next_challenge.as_ref().unwrap_or(&Value::Null))}).transpose()?;
    let mut expected=vec![(OUTBOX,outbox_id.as_str(),Some(&before_outbox)),(EFFECTS,eid.as_str(),Some(&effect)),(COMMANDS,command_id,Some(&command)),(BINDINGS,binding_id.as_str(),Some(&binding))];
    let mut records=vec![(OUTBOX,outbox_id.as_str(),&delivered),(EFFECTS,eid.as_str(),&next_effect),(COMMANDS,command_id,&next_command),(BINDINGS,binding_id.as_str(),&next_binding)];
    if let(Some(before),Some(after))=(&previous_challenge,&next_challenge){expected.push((CHALLENGES,challenge_id,Some(before)));records.push((CHALLENGES,challenge_id,after));}
    if let(Some(before),Some(after))=(&previous_queue,&next_queue){expected.push((QUEUE,challenge_id,Some(before)));records.push((QUEUE,challenge_id,after));}
    let saved = put_records_with_events(
        state,
        field(&claimed, "competition_id"),
        &expected,&records,
        &{let mut events=vec![event];if let Some(queue)=&next_queue{events.push(leases::planned_event(field(&claimed,"competition_id"),"competition",field(&claimed,"competition_id"),"competition.scheduler.admission_decision",command_id,queue["admission_decision"].clone()));}events},
    )?;
    if !saved{let mut retry=before_outbox.clone();retry["status"]=json!("failed");retry["next_attempt_at"]=json!(now());retry["last_error"]=json!("Run command completed; local binding projection pending");let _=state.store.put_many_if(&[(OUTBOX,outbox_id.as_str(),Some(&before_outbox))],&[(OUTBOX,outbox_id.as_str(),&retry)]).map_err(ApiError::internal)?;}
    if saved && matches!(run_command,"start"|"resolve") && next_binding["state"] == "running" {
        spawn_run_watcher(
            state.clone(),
            field(&claimed, "competition_id").to_owned(),
            field(&claimed, "challenge_id").to_owned(),
            run_id,
            binding_id,
            next_binding["execution_generation"].as_u64().unwrap_or(0),
        );
    }
    Ok(())
}

pub(super) async fn internal(state:&Arc<AppState>,competition_id:&str,kind:&str,command_id:&str,payload:&Value)->ApiResult<Value>{
    let receipt=command(state,Some(competition_id),kind,command_id,payload,&Value::Null).await?;let command_id=field(&receipt,"command_id");
    for outbox in state.store.list(OUTBOX).map_err(ApiError::internal)?.into_iter().filter(|row|row["command_id"]==command_id && matches!(field(row,"status"),"pending"|"failed")){deliver(state,&outbox).await?;}
    let receipt=prior_receipt(state,command_id)?.unwrap_or(receipt);
    if receipt["state"]=="failed"{return Err(ApiError::new(StatusCode::CONFLICT,"competition.binding.execution_failed",field(&receipt["error"],"message")));}Ok(receipt)
}

pub(super) async fn ensure_scheduler_start(state:&Arc<AppState>,competition_id:&str,challenge:&Value,revision:&Value,run_id:&str,lease_id:&Value,entry:&Value,start_body:&Value)->ApiResult<Value>{
    let challenge_id=field(challenge,"challenge_id");let revision_id=field(revision,"revision_id");let command_id=format!("scheduler-start:{competition_id}:{challenge_id}:{revision_id}");
    if let Some(command)=state.store.get(COMMANDS,&command_id).map_err(ApiError::internal)?{
        let binding_id=field(&command["plan"],"binding_id");return state.store.get(BINDINGS,binding_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("planned scheduler binding"));
    }
    let binding_id=id("bind");let binding=json!({"binding_id":binding_id,"competition_id":competition_id,"competition_challenge_id":challenge_id,"revision_id":revision_id,"run_id":run_id,"execution_generation":0,"lease_id":lease_id,"state":"planned","created_at":now(),"updated_at":now()});
    let outbox=new_outbox(competition_id,challenge_id,&binding_id,run_id,0,"run_binding.start",&command_id);let outbox_id=field(&outbox,"outbox_id");let eid=effect_id(&outbox);
    let command=json!({"request":{"command_type":"run_binding.start","competition_id":competition_id,"payload":{"challenge_id":challenge_id}},"plan":{"binding_id":binding_id,"competition_id":competition_id,"challenge_id":challenge_id,"run_id":run_id,"start_body":start_body,"outbox":outbox},"status":202,"response":{"receipt":{"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),"state":"accepted","run_id":run_id,"aggregate":{"type":"run_binding","id":binding_id},"submitted_at":now(),"completed_at":null,"output":{},"effect_ids":[eid],"error":null}}});
    let effect=initial_effect(&outbox,&command);let mut queued=entry.clone();queued["state"]=json!("held");queued["admission_decision"]["action"]=json!("hold");queued["admission_decision"]["reason"]=json!("binding_start_pending");queued["admission_decision"]["binding_id"]=json!(binding_id);queued["admission_decision"]["run_id"]=json!(run_id);
    let event=leases::planned_event(competition_id,"run_binding",&binding_id,"competition.run_binding.created",&command_id,json!({"binding_id":binding_id,"run_id":run_id,"challenge_id":challenge_id,"state":"planned"}));
    if !put_records_with_events(state,competition_id,&[(COMMANDS,&command_id,None),(BINDINGS,&binding_id,None),(OUTBOX,outbox_id,None),(EFFECTS,&eid,None),(CHALLENGES,challenge_id,Some(challenge)),(QUEUE,challenge_id,Some(entry))],&[(COMMANDS,&command_id,&command),(BINDINGS,&binding_id,&binding),(OUTBOX,outbox_id,&outbox),(EFFECTS,&eid,&effect),(QUEUE,challenge_id,&queued)],&[event])?{return Err(conflict("competition.binding.concurrent_update","Scheduler admission changed before Run start"));}
    if let Err(error)=deliver(state,&outbox).await{tracing::warn!(%command_id,code=%error.code,"Scheduler Run start remains in its durable outbox");}
    state.store.get(BINDINGS,&binding_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("scheduler binding"))
}

pub(super) async fn reconcile_startup(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if !is_binding_command(field(&command["request"], "command_type"))
            || command["response"]["receipt"]["state"] != "accepted"
        {
            continue;
        }
        let command_id = field(&command["response"]["receipt"], "command_id");
        if let Err(error) = recover_missing(state, command_id) {
            tracing::warn!(command_id, code=%error.code,
                "Accepted RunBinding local effect recovery needs inspection");
        }
    }
    for item in state.store.list(OUTBOX).map_err(ApiError::internal)? {
        if item["status"] != "processing" {
            continue;
        }
        let outbox_id = field(&item, "outbox_id").to_owned();
        let _lock =
            submission_outbox::submission_lock(state, &format!("binding-outbox-{outbox_id}"))
                .await?;
        let Some(before) = state
            .store
            .get(OUTBOX, &outbox_id)
            .map_err(ApiError::internal)?
        else {
            continue;
        };
        if before["status"] != "processing" {
            continue;
        }
        let mut failed = before.clone();
        failed["status"] = json!("failed");
        failed["next_attempt_at"] = json!(now());
        failed["last_error"] = json!("consumer interrupted before acknowledgement");
        failed["updated_at"] = json!(now());
        let _ = state
            .store
            .put_many_if(
                &[(OUTBOX, &outbox_id, Some(&before))],
                &[(OUTBOX, &outbox_id, &failed)],
            )
            .map_err(ApiError::internal)?;
    }
    for outbox in state.store.list(OUTBOX).map_err(ApiError::internal)? {
        let eid = effect_id(&outbox);
        if state
            .store
            .get(EFFECTS, &eid)
            .map_err(ApiError::internal)?
            .is_some()
        {
            continue;
        }
        let command_id = field(&outbox, "command_id");
        let Some(command) = state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?
        else {
            continue;
        };
        let effect = initial_effect(&outbox, &command);
        let _ = state
            .store
            .put_many_if(&[(EFFECTS, &eid, None)], &[(EFFECTS, &eid, &effect)])
            .map_err(ApiError::internal)?;
    }
    Ok(())
}

pub(super) async fn maintenance(state: &Arc<AppState>) -> ApiResult<()> {
    for outbox in state.store.list(OUTBOX).map_err(ApiError::internal)? {
        if matches!(field(&outbox, "status"), "pending" | "failed" | "awaiting_confirmation") && due(&outbox) {
            deliver(state, &outbox).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::transition_allowed;

    #[test]
    fn binding_operator_transitions_match_source_table() {
        assert!(transition_allowed("running", "paused"));
        assert!(transition_allowed("running", "resolving"));
        assert!(transition_allowed("rejected", "resolving"));
        assert!(transition_allowed("paused", "resolving"));
        assert!(!transition_allowed("paused", "paused"));
        assert!(!transition_allowed("solved", "resolving"));
    }
}
