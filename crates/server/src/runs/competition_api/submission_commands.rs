//! Source CommandAPI plan and local effects for candidate registration and approval.
use super::*;
use sha2::Sha256;

const RESULTS: &str = "competition_submission_effect_results";
const INDEX: &str = "competition_candidate_value_index";
const OUTBOX: &str = "competition_submission_outbox_live";
const EFFECTS: &str = "platform_effect_receipts";
const VERSIONS: &str = "platform_aggregate_versions";

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub(super) fn candidate_exists(
    state: &AppState,
    competition_id: &str,
    challenge_id: &str,
    answer_slot: u64,
    answer: &str,
) -> ApiResult<bool> {
    let current = digest(answer);
    Ok(scoped(state, CANDIDATES, competition_id)?
        .iter()
        .any(|row| {
            row["competition_challenge_id"] == challenge_id
                && row["answer_slot"] == answer_slot
                && row["digest"] == current
        }))
}

fn request_payload(kind: &str, payload: &Value) -> Value {
    let mut stored = payload.clone();
    if kind == "competition.submission.submit" {
        stored["answer_digest"] = json!(digest(field(payload, "answer")));
        stored["witness_digest"] = json!(digest(field(payload, "witness")));
        if let Some(object) = stored.as_object_mut() {
            object.remove("answer");
            object.remove("witness");
        }
    }
    stored
}

fn receipt(command_id: &str, aggregate_id: &str) -> Value {
    json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,
        "aggregate":{"type":"candidate","id":aggregate_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":now(),
        "completed_at":null,"next":["receipt","snapshot","events","wait"],
        "output":{},"effect_ids":[],"error":null})
}

fn result(
    state: &AppState,
    command_id: &str,
    status: &str,
    output: Value,
    error: Value,
) -> ApiResult<bool> {
    let row = json!({"command_id":command_id,"state":status,"output":output,
        "error":error,"created_at":now()});
    state
        .store
        .put_many_if(
            &[(RESULTS, command_id, None)],
            &[(RESULTS, command_id, &row)],
        )
        .map_err(ApiError::internal)
}

pub(super) fn settle(state: &AppState, command_id: &str) -> ApiResult<Option<Value>> {
    let Some(result) = state
        .store
        .get(RESULTS, command_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(None);
    };
    loop {
        let Some(previous) = state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?
        else {
            return Ok(None);
        };
        if previous["response"]["receipt"]["state"] != "accepted" {
            return Ok(Some(previous["response"]["receipt"].clone()));
        }
        let mut updated = previous.clone();
        let receipt = &mut updated["response"]["receipt"];
        receipt["state"] = result["state"].clone();
        receipt["output"] = result["output"].clone();
        receipt["error"] = result["error"].clone();
        receipt["completed_at"] = json!(now());
        receipt["next"] = json!([]);
        let aggregate_id = field(&previous["plan"], "aggregate_id");
        if let Some(seq) = state
            .store
            .list(EVENTS)
            .map_err(ApiError::internal)?
            .iter()
            .filter(|event| {
                event["command_id"] == command_id
                    && matches!(
                        field(event, "event_type"),
                        "competition.submission.registration_requested"
                            | "competition.submission.approval_requested"
                    )
            })
            .filter_map(|event| event["global_seq"].as_u64())
            .max()
        {
            receipt["event_cursor"] = json!(
                crate::operations_api::competition_event_cursor(
                    state,
                    "candidate",
                    aggregate_id,
                    seq,
                    field(&previous["plan"], "cursor_scope")
                )
                .map_err(ApiError::internal)?
            );
        }
        updated["status"] = json!(
            crate::operations_api::planned_receipt_status(&updated["response"]["receipt"]).as_u16()
        );
        let claim = state
            .store
            .get("platform_command_claims", command_id)
            .map_err(ApiError::internal)?;
        let mut next_claim = claim.clone();
        if let Some(value) = next_claim.as_mut() {
            value["state"] = json!(if result["state"] == "completed" {
                "completed"
            } else {
                "failed"
            });
            value["updated_at"] = json!(now());
        }
        let mut expected = vec![(COMMANDS, command_id, Some(&previous))];
        let mut records = vec![(COMMANDS, command_id, &updated)];
        if let (Some(before), Some(after)) = (claim.as_ref(), next_claim.as_ref()) {
            expected.push(("platform_command_claims", command_id, Some(before)));
            records.push(("platform_command_claims", command_id, after));
        }
        if state
            .store
            .put_many_if(&expected, &records)
            .map_err(ApiError::internal)?
        {
            return Ok(Some(updated["response"]["receipt"].clone()));
        }
    }
}

fn pending(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let mut receipt = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("submission command"))?["response"]["receipt"]
        .clone();
    receipt["error"] = json!({"code":"command.result_pending",
        "message":"Submission local effect needs recovery","category":"state","retryable":true});
    Ok(receipt)
}

fn cleanup_failed_answer_secret(state: &AppState, command_id: &str) -> ApiResult<()> {
    let Some(command) = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if command["request"]["command_type"] != "competition.submission.submit"
        || state
            .store
            .get(RESULTS, command_id)
            .map_err(ApiError::internal)?
            .is_none_or(|result| result["state"] != "failed")
    {
        return Ok(());
    }
    let reference = field(&command["plan"], "answer_ref");
    if reference.is_empty() {
        return Ok(());
    }
    if state
        .store
        .list(CANDIDATES)
        .map_err(ApiError::internal)?
        .iter()
        .any(|candidate| candidate["answer_ref"] == reference)
    {
        return Ok(());
    }
    remove_secret(state, reference)
}

fn failed(
    state: &AppState,
    command_id: &str,
    code: &str,
    message: &str,
    category: &str,
) -> ApiResult<Value> {
    result(
        state,
        command_id,
        "failed",
        json!({}),
        json!({"code":code,
        "message":message,"category":category,"retryable":false}),
    )?;
    let receipt = settle(state, command_id)?.unwrap_or(pending(state, command_id)?);
    if let Err(error) = cleanup_failed_answer_secret(state, command_id) {
        tracing::warn!(command_id,code=%error.code,"Failed submission answer cleanup pending");
    }
    Ok(receipt)
}

fn candidate_output(state: &AppState, candidate: &Value) -> ApiResult<Value> {
    let competition_id = field(candidate, "competition_id");
    let candidate_id = field(candidate, "candidate_id");
    let submission = scoped(state, SUBMISSIONS, competition_id)?
        .into_iter()
        .filter(|row| row["candidate_id"] == candidate_id)
        .last();
    Ok(
        json!({"candidate_id":candidate_id,"candidate_state":candidate["state"],
        "submission_id":submission.as_ref().map(|row|field(row,"submission_id")).unwrap_or(""),
        "approval_required":candidate["state"] == "awaiting_approval"}),
    )
}

fn stage(
    state: &AppState,
    route_competition_id: &str,
    kind: &str,
    command_id: &str,
    payload: &Value,
    expected_version: Option<u64>,
    cursor_scope: &str,
    admission: &Value,
    actor_id: &str,
) -> ApiResult<bool> {
    let (aggregate_id, competition_id, event_type, event_payload, entity) =
        if kind == "competition.submission.submit" {
            let challenge_id = field(payload, "challenge_id");
            let challenge = state
                .store
                .get(CHALLENGES, challenge_id)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::missing("challenge"))?;
            if challenge["tombstoned"] == true {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "competition.challenge.tombstoned",
                    format!("challenge {challenge_id} is tombstoned"),
                ));
            }
            let answer = field(payload, "answer");
            let witness = field(payload, "witness");
            let source_run_id = field(payload, "source_run_id");
            let source_kind = field(payload, "source_kind");
            if answer.is_empty()
                || witness.is_empty()
                || source_run_id.is_empty()
                || source_kind.is_empty()
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "competition.submission.required",
                    "answer, witness, source_run_id and source_kind are required",
                ));
            }
            let slot = payload["answer_slot"]
                .as_u64()
                .filter(|slot| *slot > 0)
                .unwrap_or(1);
            (
                challenge_id.to_owned(),
                field(&challenge, "competition_id").to_owned(),
                "competition.submission.registration_requested",
                json!({"challenge_id":challenge_id,"source_run_id":source_run_id,
                    "source_kind":source_kind,"answer_slot":slot,"digest":digest(answer)}),
                challenge,
            )
        } else {
            let candidate_id = field(payload, "candidate_id");
            if candidate_id.is_empty() {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "competition.submission.candidate_id_required",
                    "candidate_id is required",
                ));
            }
            let candidate = state
                .store
                .get(CANDIDATES, candidate_id)
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::missing("candidate"))?;
            (
                candidate_id.to_owned(),
                field(&candidate, "competition_id").to_owned(),
                "competition.submission.approval_requested",
                json!({"candidate_id":candidate_id}),
                candidate,
            )
        };
    let version_key = format!("candidate\0{aggregate_id}");
    let version = state
        .store
        .get(VERSIONS, &version_key)
        .map_err(ApiError::internal)?;
    if expected_version.is_some_and(|expected| {
        version
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0)
            != expected
    }) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "command.version_conflict",
            "expected_version does not match candidate stream",
        ));
    }
    let answer_secret = if kind == "competition.submission.submit" {
        let competition = live_competition(state, &competition_id)?;
        let key = format!("candidate-intent-{}", uuid::Uuid::new_v4().simple());
        Some(write_secret(
            state,
            field(&competition, "connection_id"),
            &key,
            field(payload, "answer"),
            "",
            1,
        )?)
    } else {
        None
    };
    let answer_ref = answer_secret
        .as_ref()
        .map(|secret| secret["reference"].clone())
        .unwrap_or(Value::Null);
    let stored_payload = request_payload(kind, payload);
    let mut stored = json!({"request":{"command_type":kind,
        "route_competition_id":route_competition_id,"payload":stored_payload,
        "expected_version":expected_version},
        "plan":{"aggregate_id":aggregate_id,"competition_id":competition_id,
            "entity_before":entity,"cursor_scope":cursor_scope,"actor_id":actor_id,
            "answer_ref":answer_ref},
        "response":{"receipt":receipt(command_id,&aggregate_id)}});
    let claim = admission.get("claim").filter(|value| value.is_object());
    if let Some(claim) = claim {
        stored["request_fingerprint"] = claim["request_fingerprint"].clone();
        stored["status"] = json!(StatusCode::ACCEPTED.as_u16());
    }
    let mut expected = vec![
        (COMMANDS, command_id, None),
        (VERSIONS, version_key.as_str(), version.as_ref()),
    ];
    let mut records = vec![(COMMANDS, command_id, &stored)];
    if let Some(claim) = claim {
        expected.push(("platform_command_claims", command_id, None));
        records.push(("platform_command_claims", command_id, claim));
        let key_id = field(admission, "key_id");
        if !key_id.is_empty() {
            expected.push(("platform_idempotency_keys", key_id, None));
            records.push(("platform_idempotency_keys", key_id, &admission["keyed"]));
        }
    }
    let event = leases::planned_event(
        &competition_id,
        "candidate",
        &aggregate_id,
        event_type,
        command_id,
        event_payload,
    );
    let committed =
        match put_records_with_events(state, &competition_id, &expected, &records, &[event]) {
            Ok(committed) => committed,
            Err(error) => {
                if let Some(secret) = answer_secret.as_ref() {
                    let _ = remove_secret(state, field(secret, "reference"));
                }
                return Err(error);
            }
        };
    if !committed && let Some(secret) = answer_secret.as_ref() {
        let _ = remove_secret(state, field(secret, "reference"));
    }
    Ok(committed)
}

fn apply_submit(state: &AppState, command_id: &str, command: &Value) -> ApiResult<Value> {
    let payload = &command["request"]["payload"];
    let challenge_id = field(payload, "challenge_id");
    let answer_ref = field(&command["plan"], "answer_ref");
    let answer = read_secret(state, answer_ref)?;
    let source_run_id = field(payload, "source_run_id");
    let source_kind = field(payload, "source_kind");
    if !matches!(source_kind, "execution_output" | "artifact" | "run_event") {
        return failed(
            state,
            command_id,
            "competition.submission.source_kind_not_allowed",
            "source_kind is not an execution provenance",
            "validation",
        );
    }
    let answer_slot = payload["answer_slot"]
        .as_u64()
        .filter(|slot| *slot > 0)
        .unwrap_or(1);
    let answer_digest = digest(&answer);
    let index_key = format!("{challenge_id}\0{answer_slot}\0{answer_digest}");
    let competition_id = field(&command["plan"], "competition_id");
    let candidate_id = id("cand");
    loop {
        if state
            .store
            .get(RESULTS, command_id)
            .map_err(ApiError::internal)?
            .is_some()
        {
            return Ok(settle(state, command_id)?.unwrap_or(pending(state, command_id)?));
        }
        let challenge = state
            .store
            .get(CHALLENGES, challenge_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::missing("challenge"))?;
        if challenge["tombstoned"] == true {
            return failed(
                state,
                command_id,
                "competition.submission.challenge_tombstoned",
                "challenge is tombstoned",
                "validation",
            );
        }
        if challenge["state"] == "solved" {
            return failed(
                state,
                command_id,
                "competition.submission.challenge_solved",
                "challenge is already solved",
                "validation",
            );
        }
        if let Some(existing) = scoped(state, CANDIDATES, competition_id)?
            .into_iter()
            .find(|row| {
                row["competition_challenge_id"] == challenge_id
                    && row["answer_slot"] == answer_slot
                    && row["digest"] == answer_digest
            })
        {
            let mode = state
                .store
                .get(POLICIES, competition_id)
                .map_err(ApiError::internal)?
                .unwrap_or_else(|| default_policy(competition_id));
            if mode["automation_mode"] == "autonomous" {
                let _ = submission_outbox::admit_autonomous_candidate(
                    state,
                    competition_id,
                    &existing,
                )?;
            }
            let current = state
                .store
                .get(CANDIDATES, field(&existing, "candidate_id"))
                .map_err(ApiError::internal)?
                .unwrap_or(existing);
            result(
                state,
                command_id,
                "completed",
                candidate_output(state, &current)?,
                Value::Null,
            )?;
            if field(&current, "answer_ref") != answer_ref {
                let _ = remove_secret(state, answer_ref);
            }
            return Ok(settle(state, command_id)?.unwrap_or(pending(state, command_id)?));
        }
        let policy = state
            .store
            .get(POLICIES, competition_id)
            .map_err(ApiError::internal)?
            .unwrap_or_else(|| default_policy(competition_id));
        let mode = field(&policy, "automation_mode");
        let status = if mode == "assisted" {
            "awaiting_approval"
        } else {
            "candidate"
        };
        let witness_digest = field(payload, "witness_digest");
        let candidate = json!({"candidate_id":candidate_id,"competition_id":competition_id,
            "competition_challenge_id":challenge_id,"answer_slot":answer_slot,
            "digest":answer_digest,"source_run_id":source_run_id,"source_ref":source_kind,
            "source_kind":source_kind,"gate_verdict":"confirmed",
            "source_execution_generation":payload["execution_generation"].as_u64().unwrap_or(0),
            "source_worker_id":field(payload,"worker_id"),"source_session_id":field(payload,"session_id"),
            "shared_graph_fact_id":field(payload,"shared_graph_fact_id"),
            "witness_digest":witness_digest,
            "witness_artifact_path":if source_kind == "artifact" {field(payload,"artifact_path")} else {""},
            "state":status,"answer_ref":answer_ref,
            "created_at":now(),"updated_at":now()});
        let index = json!({"candidate_id":candidate_id});
        let mut events = vec![leases::planned_event(
            competition_id,
            "candidate",
            &candidate_id,
            "competition.submission.candidate_registered",
            command_id,
            json!({"candidate_id":candidate_id,"source_run_id":source_run_id,
                "source_kind":source_kind,"gate_verdict":"confirmed","answer_slot":answer_slot,
                "execution_generation":candidate["source_execution_generation"],
                "worker_id":candidate["source_worker_id"],"session_id":candidate["source_session_id"],
                "shared_graph_fact_id":candidate["shared_graph_fact_id"],
                "witness_digest":witness_digest}),
        )];
        if mode == "assisted" {
            events.push(leases::planned_event(
                competition_id,
                "candidate",
                &candidate_id,
                "competition.submission.candidate_state_changed",
                command_id,
                json!({"candidate_id":candidate_id,"from":"candidate","to":"awaiting_approval"}),
            ));
        }
        let mut next_challenge = challenge.clone();
        if challenge["state"] == "running" {
            next_challenge["state"] = json!("candidate_found");
            next_challenge["updated_at"] = json!(now());
            events.push(leases::planned_event(
                competition_id,
                "challenge",
                challenge_id,
                "competition.challenge.state_changed",
                command_id,
                json!({"challenge_id":challenge_id,"from":"running","to":"candidate_found"}),
            ));
        }
        let expected = vec![
            (CANDIDATES, candidate_id.as_str(), None),
            (INDEX, index_key.as_str(), None),
            (CHALLENGES, challenge_id, Some(&challenge)),
            (RESULTS, command_id, None),
        ];
        let mut records = vec![
            (CANDIDATES, candidate_id.as_str(), &candidate),
            (INDEX, index_key.as_str(), &index),
        ];
        let completed = json!({"command_id":command_id,"state":"completed",
            "output":{"candidate_id":candidate_id,"candidate_state":status,
                "submission_id":"","approval_required":mode == "assisted"},
            "error":null,"created_at":now()});
        if mode != "autonomous" {
            records.push((RESULTS, command_id, &completed));
        }
        if next_challenge != challenge {
            records.push((CHALLENGES, challenge_id, &next_challenge));
        }
        if !put_records_with_events(state, competition_id, &expected, &records, &events)? {
            continue;
        }
        if mode == "autonomous" {
            let _ =
                submission_outbox::admit_autonomous_candidate(state, competition_id, &candidate)?;
            let current = state
                .store
                .get(CANDIDATES, &candidate_id)
                .map_err(ApiError::internal)?
                .unwrap_or(candidate);
            result(
                state,
                command_id,
                "completed",
                candidate_output(state, &current)?,
                Value::Null,
            )?;
        }
        return Ok(settle(state, command_id)?.unwrap_or(pending(state, command_id)?));
    }
}

fn apply_approve(state: &AppState, command_id: &str, command: &Value) -> ApiResult<Value> {
    let candidate_id = field(&command["request"]["payload"], "candidate_id");
    let Some(candidate) = state
        .store
        .get(CANDIDATES, candidate_id)
        .map_err(ApiError::internal)?
    else {
        return failed(
            state,
            command_id,
            "competition.submission.approval_failed",
            "submission candidate not found",
            "state",
        );
    };
    let from = field(&candidate, "state");
    if !matches!(from, "candidate" | "awaiting_approval") {
        return failed(
            state,
            command_id,
            "competition.submission.approval_failed",
            "candidate cannot transition to approved",
            "state",
        );
    }
    let challenge_id = field(&candidate, "competition_challenge_id");
    let challenge = state
        .store
        .get(CHALLENGES, challenge_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("challenge"))?;
    let finished_source = field(&challenge,"state") == "paused"
        && candidate["gate_verdict"] == "confirmed"
        && scoped(state,BINDINGS,field(&candidate,"competition_id"))?
            .iter().any(|binding| binding["competition_challenge_id"] == challenge_id
                && binding["run_id"] == candidate["source_run_id"]
                && binding["revision_id"] == challenge["current_revision_id"]
                && matches!(field(binding,"state"),"finished"|"solved"))
        && challenge["remote_state"] == "open" && challenge["tombstoned"] != true;
    if !matches!(field(&challenge, "state"), "candidate_found" | "submitting") && !finished_source {
        return failed(
            state,
            command_id,
            "competition.submission.approval_failed",
            "challenge cannot transition to submitting",
            "state",
        );
    }
    let competition_id = field(&candidate, "competition_id");
    let competition = live_competition(state, competition_id)?;
    let connection = state
        .store
        .get(CONNECTIONS, field(&competition, "connection_id"))
        .map_err(ApiError::internal)?;
    let destination = connection
        .as_ref()
        .map(|row| format!("platform.{}", field(row, "platform_kind")))
        .unwrap_or_default();
    let attempt = scoped(state, SUBMISSIONS, competition_id)?
        .iter()
        .filter(|row| {
            row["competition_challenge_id"] == challenge_id
                && row["answer_slot"] == candidate["answer_slot"]
                && row["digest"] == candidate["digest"]
        })
        .count() as u64
        + 1;
    let submission_id = id("sub");
    let outbox_id = id("sob");
    let effect_id = id("effect");
    let idempotency_key = format!("platform_submission.submit:{submission_id}:attempt:{attempt}");
    let submission = json!({"submission_id":submission_id,"competition_id":competition_id,
        "competition_challenge_id":challenge_id,"candidate_id":candidate_id,
        "answer_slot":candidate["answer_slot"],"digest":candidate["digest"],"attempt":attempt,
        "state":"queued","dispatch_count":0,"retry_after_at":null,"remote_receipt":"",
        "last_error":"","answer_ref":candidate["answer_ref"],"created_at":now(),"updated_at":now()});
    let outbox = json!({"outbox_id":outbox_id,"command_id":command_id,"effect_id":effect_id,
        "correlation_id":command_id,"idempotency_key":idempotency_key,
        "aggregate_type":"platform_submission","aggregate_id":submission_id,
        "event_type":"competition.platform_submission.queued","destination":destination,
        "competition_id":competition_id,"challenge_id":challenge_id,"submission_id":submission_id,
        "op":"submit","status":"pending","attempts":0,"max_attempts":8,
        "next_attempt_at":null,"last_error":null,"delivered_at":null,
        "created_at":now(),"updated_at":now()});
    let effect = json!({"schema_version":1,"effect_id":effect_id,"command_id":command_id,
        "acceptance_receipt_id":command["response"]["receipt"]["receipt_id"],
        "domain":"competition","state":"waiting",
        "aggregate":{"type":"platform_submission","id":submission_id},
        "correlation_id":command_id,"outbox_id":outbox_id,"destination":destination,
        "idempotency_key":idempotency_key,"attempts":0,"max_attempts":8,
        "retry_after":null,"object_links":{"aggregate":format!("platform_submission:{submission_id}")},
        "output":{},"error":null,"created_at":now(),"updated_at":now(),"completed_at":null});
    let mut candidate_next = candidate.clone();
    candidate_next["state"] = json!("approved");
    candidate_next["updated_at"] = json!(now());
    let mut challenge_next = challenge.clone();
    if challenge["state"] != "submitting" {
        challenge_next["state"] = json!("submitting");
        challenge_next["paused_from"] = Value::Null;
        challenge_next["updated_at"] = json!(now());
    }
    let output = json!({"candidate_id":candidate_id,"submission_id":submission_id});
    let completed = json!({"command_id":command_id,"state":"completed",
        "output":output,"error":null,"created_at":now()});
    let event_payload = json!({"submission_id":submission_id,"candidate_id":candidate_id,
        "challenge_id":challenge_id,"answer_slot":candidate["answer_slot"],"attempt":attempt});
    let actor = field(&command["plan"], "actor_id");
    let actor = if actor.is_empty() { "operator" } else { actor };
    let mut events = vec![
        leases::planned_event(
            competition_id,
            "candidate",
            candidate_id,
            "competition.submission.candidate_state_changed",
            command_id,
            json!({"candidate_id":candidate_id,"from":from,"to":"approved","actor":actor}),
        ),
        leases::planned_event(
            competition_id,
            "platform_submission",
            &submission_id,
            "competition.platform_submission.approved",
            command_id,
            event_payload.clone(),
        ),
    ];
    let mut queued_payload = event_payload;
    queued_payload["state"] = json!("queued");
    events.push(leases::planned_event(
        competition_id,
        "platform_submission",
        &submission_id,
        "competition.platform_submission.queued",
        command_id,
        queued_payload,
    ));
    if challenge_next != challenge {
        events.push(leases::planned_event(
            competition_id,
            "challenge",
            challenge_id,
            "competition.challenge.state_changed",
            command_id,
            json!({"challenge_id":challenge_id,"from":challenge["state"],"to":"submitting"}),
        ));
    }
    let expected = vec![
        (CANDIDATES, candidate_id, Some(&candidate)),
        (CHALLENGES, challenge_id, Some(&challenge)),
        (SUBMISSIONS, submission_id.as_str(), None),
        (OUTBOX, outbox_id.as_str(), None),
        (EFFECTS, effect_id.as_str(), None),
        (RESULTS, command_id, None),
    ];
    let mut records = vec![
        (CANDIDATES, candidate_id, &candidate_next),
        (SUBMISSIONS, submission_id.as_str(), &submission),
        (OUTBOX, outbox_id.as_str(), &outbox),
        (EFFECTS, effect_id.as_str(), &effect),
        (RESULTS, command_id, &completed),
    ];
    if challenge_next != challenge {
        records.push((CHALLENGES, challenge_id, &challenge_next));
    }
    if put_records_with_events(state, competition_id, &expected, &records, &events)? {
        return Ok(settle(state, command_id)?.unwrap_or(pending(state, command_id)?));
    }
    if state
        .store
        .get(RESULTS, command_id)
        .map_err(ApiError::internal)?
        .is_some()
    {
        return Ok(settle(state, command_id)?.unwrap_or(pending(state, command_id)?));
    }
    failed(
        state,
        command_id,
        "competition.submission.approval_failed",
        "candidate or challenge changed before approval",
        "state",
    )
}

fn apply(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let command = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("submission command"))?;
    if command["response"]["receipt"]["state"] != "accepted" {
        return Ok(command["response"]["receipt"].clone());
    }
    if state
        .store
        .get(RESULTS, command_id)
        .map_err(ApiError::internal)?
        .is_some()
    {
        return Ok(settle(state, command_id)?.unwrap_or(pending(state, command_id)?));
    }
    if field(&command["request"], "command_type") == "competition.submission.submit" {
        apply_submit(state, command_id, &command)
    } else {
        apply_approve(state, command_id, &command)
    }
}

pub(super) fn execute(
    state: &Arc<AppState>,
    route_competition_id: &str,
    kind: &str,
    requested_command_id: &str,
    payload: &Value,
    expected_version: Option<u64>,
    cursor_scope: &str,
    admission: &Value,
    actor_id: &str,
) -> ApiResult<Value> {
    let command_id = if requested_command_id.is_empty() {
        id("cmd")
    } else {
        requested_command_id.to_owned()
    };
    let request = json!({"command_type":kind,"route_competition_id":route_competition_id,
        "payload":request_payload(kind,payload),"expected_version":expected_version});
    loop {
        if let Some(previous) = state
            .store
            .get(COMMANDS, &command_id)
            .map_err(ApiError::internal)?
        {
            if previous["request"] != request {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "command.idempotency_conflict",
                    "command_id was used with different submission content",
                ));
            }
            if previous["response"]["receipt"]["state"] == "accepted" {
                let _ = apply(state, &command_id);
            }
            return prior_receipt(state, &command_id)?
                .ok_or_else(|| ApiError::missing("submission command"));
        }
        if let Some(receipt) = planned_key_receipt(state, admission)? {
            return Ok(receipt);
        }
        if admission.is_object()
            && state
                .store
                .get("platform_command_claims", &command_id)
                .map_err(ApiError::internal)?
                .is_some()
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "command.outcome_unknown",
                "Prior command outcome must be inspected before retry",
            ));
        }
        if !stage(
            state,
            route_competition_id,
            kind,
            &command_id,
            payload,
            expected_version,
            cursor_scope,
            admission,
            actor_id,
        )? {
            continue;
        }
        return apply(state, &command_id);
    }
}

pub(super) fn recover(state: &AppState, command_id: &str) -> ApiResult<Value> {
    apply(state, command_id)
}

pub(super) fn reconcile(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if !matches!(
            field(&command["request"], "command_type"),
            "competition.submission.submit" | "platform_submission.approve"
        ) {
            continue;
        }
        let command_id = field(&command["response"]["receipt"], "command_id");
        if command["response"]["receipt"]["state"] == "accepted" {
            if let Err(error) = apply(state, command_id) {
                tracing::warn!(command_id,code=%error.code,
                    "Submission local effect recovery remains pending");
            }
        }
        if let Err(error) = cleanup_failed_answer_secret(state, command_id) {
            tracing::warn!(command_id,code=%error.code,
                "Failed submission answer cleanup remains pending");
        }
    }
    Ok(())
}
