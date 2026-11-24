//! Source manual submission override: accepted plan before one local outbox effect.
use super::*;
use sha2::Sha256;
use std::collections::HashSet;

const RESULTS: &str = "competition_manual_override_results";
const INDEX: &str = "competition_candidate_value_index";
const OUTBOX: &str = "competition_submission_outbox_live";
const EFFECTS: &str = "platform_effect_receipts";
const VERSIONS: &str = "platform_aggregate_versions";

fn intent_lock(state: &AppState) -> ApiResult<fs::File> {
    let path = native_chat::state_dir(state)
        .join("_platform")
        .join("manual-override-intents.lock");
    fs::create_dir_all(path.parent().unwrap()).map_err(ApiError::internal)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(ApiError::internal)?;
    fs2::FileExt::lock_exclusive(&file).map_err(ApiError::internal)?;
    Ok(file)
}

fn cleanup_unplanned_answers(state: &AppState) -> ApiResult<()> {
    let mut referenced = HashSet::new();
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if command["request"]["command_type"] == "competition.submission.manual_override" {
            let reference = field(&command["plan"], "answer_ref");
            if !reference.is_empty() {
                referenced.insert(reference.to_owned());
            }
        }
    }
    for candidate in state.store.list(CANDIDATES).map_err(ApiError::internal)? {
        let reference = field(&candidate, "answer_ref");
        if !reference.is_empty() {
            referenced.insert(reference.to_owned());
        }
    }
    let root = native_chat::state_dir(state).join("control/competition/secrets");
    if root.exists() {
        for connection in fs::read_dir(root).map_err(ApiError::internal)? {
            let connection = connection.map_err(ApiError::internal)?;
            if !connection.file_type().map_err(ApiError::internal)?.is_dir() {
                continue;
            }
            let connection_id = connection.file_name().to_string_lossy().into_owned();
            for entry in fs::read_dir(connection.path()).map_err(ApiError::internal)? {
                let entry = entry.map_err(ApiError::internal)?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with("manual-override-intent-")
                    || !entry.file_type().map_err(ApiError::internal)?.is_file()
                {
                    continue;
                }
                if let Some(key) = name.strip_suffix(".secret") {
                    let reference = secret_ref(&connection_id, key);
                    if !referenced.contains(&reference) {
                        remove_secret(state, &reference)?;
                    }
                } else if name.ends_with(".tmp") {
                    fs::remove_file(entry.path()).map_err(ApiError::internal)?;
                }
            }
        }
    }
    for secret in state.store.list(SECRETS).map_err(ApiError::internal)? {
        if field(&secret, "key").starts_with("manual-override-intent-") {
            let reference = field(&secret, "reference");
            if !referenced.contains(reference) {
                remove_secret(state, reference)?;
            }
        }
    }
    Ok(())
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn request_payload(payload: &Value) -> Value {
    let mut saved = payload.clone();
    saved["answer_digest"] = json!(digest(field(payload, "answer")));
    if let Some(object) = saved.as_object_mut() {
        object.remove("answer");
        object.remove("confirm_answer");
    }
    saved
}

fn accepted(command_id: &str, challenge_id: &str) -> Value {
    json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,
        "aggregate":{"type":"candidate","id":challenge_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":now(),
        "completed_at":null,"next":["receipt","snapshot","events","wait"],
        "output":{},"effect_ids":[],"error":null})
}

fn cleanup_unused_answer(state: &AppState, command_id: &str) -> ApiResult<()> {
    let Some(command) = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if command["request"]["command_type"] != "competition.submission.manual_override"
        || state
            .store
            .get(RESULTS, command_id)
            .map_err(ApiError::internal)?
            .is_none()
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

fn finish(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let result = state
        .store
        .get(RESULTS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("manual override result"))?;
    loop {
        let command = state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::missing("manual override command"))?;
        if command["response"]["receipt"]["state"] != "accepted" {
            return Ok(command["response"]["receipt"].clone());
        }
        let mut next = command.clone();
        let receipt = &mut next["response"]["receipt"];
        receipt["state"] = result["state"].clone();
        receipt["output"] = result["output"].clone();
        receipt["error"] = result["error"].clone();
        receipt["completed_at"] = json!(now());
        receipt["next"] = json!([]);
        let challenge_id = field(&command["plan"], "challenge_id");
        if let Some(seq) = state
            .store
            .list(EVENTS)
            .map_err(ApiError::internal)?
            .iter()
            .filter(|event| {
                event["command_id"] == command_id
                    && event["event_type"] == "competition.submission.manual_override_requested"
            })
            .filter_map(|event| event["global_seq"].as_u64())
            .max()
        {
            receipt["event_cursor"] = json!(
                crate::operations_api::competition_event_cursor(
                    state,
                    "candidate",
                    challenge_id,
                    seq,
                    field(&command["plan"], "cursor_scope")
                )
                .map_err(ApiError::internal)?
            );
        }
        next["status"] = json!(
            crate::operations_api::planned_receipt_status(&next["response"]["receipt"]).as_u16()
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
        let mut expected = vec![(COMMANDS, command_id, Some(&command))];
        let mut records = vec![(COMMANDS, command_id, &next)];
        if let (Some(before), Some(after)) = (claim.as_ref(), next_claim.as_ref()) {
            expected.push(("platform_command_claims", command_id, Some(before)));
            records.push(("platform_command_claims", command_id, after));
        }
        if state
            .store
            .put_many_if(&expected, &records)
            .map_err(ApiError::internal)?
        {
            if let Err(error) = cleanup_unused_answer(state, command_id) {
                tracing::warn!(command_id,code=%error.code,"Manual override answer cleanup pending");
            }
            return Ok(next["response"]["receipt"].clone());
        }
    }
}

fn journal_failure(
    state: &AppState,
    command_id: &str,
    code: &str,
    message: &str,
    category: &str,
) -> ApiResult<Value> {
    let result = json!({"command_id":command_id,"state":"failed","output":{},
        "error":{"code":code,"message":message,"category":category,"retryable":false},
        "created_at":now()});
    state
        .store
        .put_many_if(
            &[(RESULTS, command_id, None)],
            &[(RESULTS, command_id, &result)],
        )
        .map_err(ApiError::internal)?;
    finish(state, command_id)
}

fn load_challenge(state: &AppState, payload: &Value) -> ApiResult<Value> {
    let challenge_id = field(payload, "challenge_id");
    let challenge = if !challenge_id.is_empty() {
        state
            .store
            .get(CHALLENGES, challenge_id)
            .map_err(ApiError::internal)?
    } else {
        let competition_id = field(payload, "competition_id");
        let external_id = field(payload, "external_challenge_id");
        if competition_id.is_empty() || external_id.is_empty() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "competition.challenge.external_challenge_id_required",
                "challenge_id or competition_id and external_challenge_id are required",
            ));
        }
        ordered_scoped(state, CHALLENGES, competition_id)?
            .into_iter()
            .find(|row| field(row, "external_challenge_id") == external_id)
    };
    challenge.ok_or_else(|| ApiError::missing("challenge"))
}

fn stage(
    state: &AppState,
    route_competition_id: &str,
    command_id: &str,
    payload: &Value,
    expected_version: Option<u64>,
    cursor_scope: &str,
    admission: &Value,
    actor_id: &str,
) -> ApiResult<bool> {
    if actor_id.is_empty() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "competition.submission.manual_override_operator_required",
            "manual submission override requires an authenticated operator",
        ));
    }
    let answer = field(payload, "answer");
    let confirmation = field(payload, "confirm_answer");
    if answer.is_empty() || confirmation.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.submission.manual_override.answer_required",
            "answer and confirm_answer are required",
        ));
    }
    if answer != confirmation {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.submission.manual_override_confirmation_mismatch",
            "confirm_answer must exactly match answer",
        ));
    }
    if payload["acknowledge_unverified"] != true {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.submission.manual_override_ack_required",
            "acknowledge_unverified must be true",
        ));
    }
    let challenge = load_challenge(state, payload)?;
    let challenge_id = field(&challenge, "challenge_id");
    let competition_id = field(&challenge, "competition_id");
    let slot = payload["answer_slot"]
        .as_u64()
        .filter(|slot| *slot > 0)
        .unwrap_or(1);
    let version_key = format!("candidate\0{challenge_id}");
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
    let competition = state
        .store
        .get(COMPETITIONS, competition_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("competition"))?;
    let _intent_lock = intent_lock(state)?;
    let secret_key = format!("manual-override-intent-{}", uuid::Uuid::new_v4().simple());
    let secret = write_secret(
        state,
        field(&competition, "connection_id"),
        &secret_key,
        answer,
        "",
        1,
    )?;
    let request = json!({"command_type":"competition.submission.manual_override",
        "route_competition_id":route_competition_id,
        "payload":request_payload(payload),"expected_version":expected_version});
    let mut stored = json!({"request":request,
        "plan":{"challenge_id":challenge_id,"competition_id":competition_id,
            "answer_ref":secret["reference"],"answer_slot":slot,
            "actor_id":actor_id,"cursor_scope":cursor_scope},
        "response":{"receipt":accepted(command_id,challenge_id)}});
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
        competition_id,
        "candidate",
        challenge_id,
        "competition.submission.manual_override_requested",
        command_id,
        json!({"challenge_id":challenge_id,"answer_slot":slot,"digest":digest(answer),
            "actor":actor_id,"acknowledged_unverified":true}),
    );
    let committed =
        match put_records_with_events(state, competition_id, &expected, &records, &[event]) {
            Ok(committed) => committed,
            Err(error) => {
                let _ = remove_secret(state, field(&secret, "reference"));
                return Err(error);
            }
        };
    if !committed {
        let _ = remove_secret(state, field(&secret, "reference"));
    }
    Ok(committed)
}

fn apply(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let command = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("manual override command"))?;
    if command["response"]["receipt"]["state"] != "accepted" {
        return Ok(command["response"]["receipt"].clone());
    }
    if state
        .store
        .get(RESULTS, command_id)
        .map_err(ApiError::internal)?
        .is_some()
    {
        return finish(state, command_id);
    }
    let challenge_id = field(&command["plan"], "challenge_id");
    let competition_id = field(&command["plan"], "competition_id");
    let answer_ref = field(&command["plan"], "answer_ref");
    let answer = read_secret(state, answer_ref)?;
    let answer = answer.trim();
    let answer_digest = digest(answer);
    let slot = command["plan"]["answer_slot"].as_u64().unwrap_or(1);
    let index_key = format!("{challenge_id}\0{slot}\0{answer_digest}");
    loop {
        if state
            .store
            .get(RESULTS, command_id)
            .map_err(ApiError::internal)?
            .is_some()
        {
            return finish(state, command_id);
        }
        let challenge = state
            .store
            .get(CHALLENGES, challenge_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::missing("challenge"))?;
        if challenge["tombstoned"] == true {
            return journal_failure(
                state,
                command_id,
                "competition.submission.challenge_tombstoned",
                "challenge is tombstoned",
                "validation",
            );
        }
        if challenge["state"] == "solved" {
            return journal_failure(
                state,
                command_id,
                "competition.submission.challenge_solved",
                "challenge is already solved",
                "validation",
            );
        }
        if answer.is_empty() {
            return journal_failure(
                state,
                command_id,
                "competition.submission.empty_value",
                "manual answer is empty",
                "validation",
            );
        }
        let existing = scoped(state, CANDIDATES, competition_id)?
            .into_iter()
            .find(|row| {
                row["competition_challenge_id"] == challenge_id
                    && row["answer_slot"] == slot
                    && row["digest"] == answer_digest
            });
        if let Some(candidate) = existing.as_ref() {
            let submissions = scoped(state, SUBMISSIONS, competition_id)?;
            if let Some(submission) = submissions
                .into_iter()
                .filter(|row| row["candidate_id"] == candidate["candidate_id"])
                .last()
            {
                let output = json!({"candidate_id":candidate["candidate_id"],
                    "submission_id":submission["submission_id"],"manual_override":true});
                let result = json!({"command_id":command_id,"state":"completed",
                    "output":output,"error":null,"created_at":now()});
                let _ = state
                    .store
                    .put_many_if(
                        &[(RESULTS, command_id, None)],
                        &[(RESULTS, command_id, &result)],
                    )
                    .map_err(ApiError::internal)?;
                return finish(state, command_id);
            }
        }
        let candidate_id = existing
            .as_ref()
            .map(|row| field(row, "candidate_id").to_owned())
            .unwrap_or_else(|| id("cand"));
        let mut candidate = existing.clone().unwrap_or_else(|| {
            json!({
            "candidate_id":candidate_id,"competition_id":competition_id,
            "competition_challenge_id":challenge_id,"answer_slot":slot,
            "digest":answer_digest,"source_run_id":null,
            "source_ref":"operator_override","gate_verdict":"manual_override:unverified",
            "source_execution_generation":0,"answer_ref":answer_ref,
            "created_at":now()})
        });
        candidate["state"] = json!("approved");
        candidate["updated_at"] = json!(now());
        let previous = scoped(state, SUBMISSIONS, competition_id)?;
        let attempt = previous
            .iter()
            .filter(|row| {
                row["competition_challenge_id"] == challenge_id
                    && row["answer_slot"] == slot
                    && row["digest"] == answer_digest
            })
            .count() as u64
            + 1;
        let submission_id = id("sub");
        let outbox_id = id("sob");
        let effect_id = id("effect");
        let competition = state
            .store
            .get(COMPETITIONS, competition_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::missing("competition"))?;
        let connection = state
            .store
            .get(CONNECTIONS, field(&competition, "connection_id"))
            .map_err(ApiError::internal)?;
        let destination = connection
            .as_ref()
            .map(|row| format!("platform.{}", field(row, "platform_kind")))
            .unwrap_or_default();
        let outbox_key =
            format!("platform_submission.manual_override:{challenge_id}:{slot}:{answer_digest}");
        let correlation_id = field(&command["request"]["payload"], "correlation_id")
            .to_owned()
            .if_empty(command_id);
        let submission = json!({"submission_id":submission_id,"competition_id":competition_id,
            "competition_challenge_id":challenge_id,"candidate_id":candidate_id,
            "answer_slot":candidate["answer_slot"],"digest":candidate["digest"],"attempt":attempt,
            "state":"queued","dispatch_count":0,"retry_after_at":null,
            "remote_receipt":"","last_error":"","answer_ref":candidate["answer_ref"],
            "created_at":now(),"updated_at":now()});
        let outbox = json!({"outbox_id":outbox_id,"command_id":command_id,"effect_id":effect_id,
            "correlation_id":correlation_id,"idempotency_key":outbox_key,
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
            "correlation_id":correlation_id,"outbox_id":outbox_id,"destination":destination,
            "idempotency_key":outbox_key,"attempts":0,"max_attempts":8,
            "retry_after":null,"object_links":{"aggregate":format!("platform_submission:{submission_id}")},
            "output":{},"error":null,"created_at":now(),"updated_at":now(),"completed_at":null});
        let mut next_challenge = challenge.clone();
        next_challenge["state"] = json!("submitting");
        next_challenge["paused_from"] = Value::Null;
        next_challenge["updated_at"] = json!(now());
        let output = json!({"candidate_id":candidate_id,"submission_id":submission_id,
            "manual_override":true});
        let done = json!({"command_id":command_id,"state":"completed",
            "output":output,"error":null,"created_at":now()});
        let actor = field(&command["plan"], "actor_id");
        let events = vec![
            leases::planned_event(
                competition_id,
                "candidate",
                &candidate_id,
                "competition.submission.manual_override_applied",
                command_id,
                json!({"candidate_id":candidate_id,"challenge_id":challenge_id,
                    "digest":answer_digest,"answer_slot":slot,"actor":actor,
                    "source_kind":candidate["source_ref"],"gate_verdict":candidate["gate_verdict"],
                    "acknowledged_unverified":true}),
            ),
            leases::planned_event(
                competition_id,
                "platform_submission",
                &submission_id,
                "competition.platform_submission.approved",
                command_id,
                json!({"submission_id":submission_id,"candidate_id":candidate_id,
                    "challenge_id":challenge_id,"answer_slot":slot,"attempt":attempt,
                    "approval_kind":"manual_override","actor":actor}),
            ),
            leases::planned_event(
                competition_id,
                "platform_submission",
                &submission_id,
                "competition.platform_submission.queued",
                command_id,
                json!({"submission_id":submission_id,"state":"queued",
                    "approval_kind":"manual_override"}),
            ),
            leases::planned_event(
                competition_id,
                "challenge",
                challenge_id,
                "competition.challenge.state_changed",
                command_id,
                json!({"challenge_id":challenge_id,"from":challenge["state"],
                    "to":"submitting","reason":"manual_override"}),
            ),
        ];
        let mut expected = vec![
            (CANDIDATES, candidate_id.as_str(), existing.as_ref()),
            (CHALLENGES, challenge_id, Some(&challenge)),
            (SUBMISSIONS, submission_id.as_str(), None),
            (OUTBOX, outbox_id.as_str(), None),
            (EFFECTS, effect_id.as_str(), None),
            (RESULTS, command_id, None),
        ];
        let mut records = vec![
            (CANDIDATES, candidate_id.as_str(), &candidate),
            (CHALLENGES, challenge_id, &next_challenge),
            (SUBMISSIONS, submission_id.as_str(), &submission),
            (OUTBOX, outbox_id.as_str(), &outbox),
            (EFFECTS, effect_id.as_str(), &effect),
            (RESULTS, command_id, &done),
        ];
        let index = json!({"candidate_id":candidate_id});
        if existing.is_none() {
            expected.push((INDEX, index_key.as_str(), None));
            records.push((INDEX, index_key.as_str(), &index));
        }
        if put_records_with_events(state, competition_id, &expected, &records, &events)? {
            return finish(state, command_id);
        }
    }
}

pub(super) fn recover(state: &AppState, command_id: &str) -> ApiResult<Value> {
    apply(state, command_id)
}

pub(super) fn execute(
    state: &Arc<AppState>,
    route_competition_id: &str,
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
    let request = json!({"command_type":"competition.submission.manual_override",
        "route_competition_id":route_competition_id,
        "payload":request_payload(payload),"expected_version":expected_version});
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
                    "command_id was used with different manual override content",
                ));
            }
            if previous["response"]["receipt"]["state"] == "accepted" {
                let _ = apply(state, &command_id);
            }
            return prior_receipt(state, &command_id)?
                .ok_or_else(|| ApiError::missing("manual override command"));
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

pub(super) fn reconcile(state: &Arc<AppState>) -> ApiResult<()> {
    {
        let _intent_lock = intent_lock(state)?;
        cleanup_unplanned_answers(state)?;
    }
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if command["request"]["command_type"] != "competition.submission.manual_override" {
            continue;
        }
        let command_id = field(&command["response"]["receipt"], "command_id");
        if command["response"]["receipt"]["state"] == "accepted" {
            if let Err(error) = apply(state, command_id) {
                tracing::warn!(command_id,code=%error.code,
                    "Manual override local effect recovery remains pending");
            }
        }
        if let Err(error) = cleanup_unused_answer(state, command_id) {
            tracing::warn!(command_id,code=%error.code,
                "Manual override unused answer cleanup remains pending");
        }
    }
    Ok(())
}
