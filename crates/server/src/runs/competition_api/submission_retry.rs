//! Source CommandAPI manual retry: one submission returns to queued.
use super::*;

const RESULTS: &str = "competition_submission_retry_results";
const VERSIONS: &str = "platform_aggregate_versions";

fn accepted(command_id: &str, submission_id: &str) -> Value {
    json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,
        "aggregate":{"type":"platform_submission","id":submission_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":now(),
        "completed_at":null,"next":["receipt","snapshot","events","wait"],
        "output":{},"effect_ids":[],"error":null})
}

fn finish(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let result = state
        .store
        .get(RESULTS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("submission retry result"))?;
    loop {
        let previous = state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::missing("submission retry command"))?;
        if previous["response"]["receipt"]["state"] != "accepted" {
            return Ok(previous["response"]["receipt"].clone());
        }
        let mut next = previous.clone();
        let receipt = &mut next["response"]["receipt"];
        receipt["state"] = result["state"].clone();
        receipt["output"] = result["output"].clone();
        receipt["error"] = result["error"].clone();
        receipt["completed_at"] = json!(now());
        receipt["next"] = json!([]);
        let submission_id = field(&previous["plan"], "submission_id");
        if let Some(seq) = state
            .store
            .list(EVENTS)
            .map_err(ApiError::internal)?
            .iter()
            .filter(|event| {
                event["command_id"] == command_id
                    && event["event_type"] == "competition.platform_submission.retry_requested"
            })
            .filter_map(|event| event["global_seq"].as_u64())
            .max()
        {
            receipt["event_cursor"] = json!(
                crate::operations_api::competition_event_cursor(
                    state,
                    "platform_submission",
                    submission_id,
                    seq,
                    field(&previous["plan"], "cursor_scope")
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
        let mut expected = vec![(COMMANDS, command_id, Some(&previous))];
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
            return Ok(next["response"]["receipt"].clone());
        }
    }
}

fn result(state: &AppState, command_id: &str, status: &str, error: Value) -> ApiResult<Value> {
    let row = json!({"command_id":command_id,"state":status,"output":{},
        "error":error,"created_at":now()});
    state
        .store
        .put_many_if(
            &[(RESULTS, command_id, None)],
            &[(RESULTS, command_id, &row)],
        )
        .map_err(ApiError::internal)?;
    finish(state, command_id)
}

fn stage(
    state: &AppState,
    route_competition_id: &str,
    command_id: &str,
    payload: &Value,
    expected_version: Option<u64>,
    cursor_scope: &str,
    admission: &Value,
) -> ApiResult<bool> {
    let submission_id = field(payload, "submission_id");
    if submission_id.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.submission.submission_id_required",
            "submission_id is required",
        ));
    }
    let submission = state
        .store
        .get(SUBMISSIONS, submission_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("submission"))?;
    let from = field(&submission, "state");
    if !matches!(from, "rate_limited" | "transient_failure" | "auth_required") {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "competition.submission.illegal_retry",
            format!("submission cannot transition from {from} to queued"),
        ));
    }
    let competition_id = field(&submission, "competition_id");
    let version_key = format!("platform_submission\0{submission_id}");
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
            "expected_version does not match submission stream",
        ));
    }
    let request = json!({"command_type":"platform_submission.retry",
        "route_competition_id":route_competition_id,"payload":payload,
        "expected_version":expected_version});
    let mut stored = json!({"request":request,
        "plan":{"submission_id":submission_id,"competition_id":competition_id,
            "submission_before":submission,"cursor_scope":cursor_scope},
        "response":{"receipt":accepted(command_id,submission_id)}});
    let claim = admission.get("claim").filter(|value| value.is_object());
    if let Some(claim) = claim {
        stored["request_fingerprint"] = claim["request_fingerprint"].clone();
        stored["status"] = json!(StatusCode::ACCEPTED.as_u16());
    }
    let mut expected = vec![
        (SUBMISSIONS, submission_id, Some(&submission)),
        (VERSIONS, version_key.as_str(), version.as_ref()),
        (COMMANDS, command_id, None),
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
        "platform_submission",
        submission_id,
        "competition.platform_submission.retry_requested",
        command_id,
        json!({"submission_id":submission_id,"from":from,"to":"queued"}),
    );
    put_records_with_events(state, competition_id, &expected, &records, &[event])
}

fn apply(state: &AppState, command_id: &str) -> ApiResult<Value> {
    let command = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("submission retry command"))?;
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
    let submission_id = field(&command["plan"], "submission_id");
    let before = &command["plan"]["submission_before"];
    let current = state
        .store
        .get(SUBMISSIONS, submission_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("submission"))?;
    if current["state"] == "queued" {
        return result(state, command_id, "completed", Value::Null);
    }
    if current != *before {
        return result(
            state,
            command_id,
            "failed",
            json!({"code":"command.side_effect_failed",
            "message":"submission changed before retry was applied",
            "category":"state","retryable":false}),
        );
    }
    let mut queued = current.clone();
    queued["state"] = json!("queued");
    queued["retry_after_at"] = Value::Null;
    queued["updated_at"] = json!(now());
    let done = json!({"command_id":command_id,"state":"completed",
        "output":{},"error":null,"created_at":now()});
    if state
        .store
        .put_many_if(
            &[
                (SUBMISSIONS, submission_id, Some(&current)),
                (RESULTS, command_id, None),
            ],
            &[
                (SUBMISSIONS, submission_id, &queued),
                (RESULTS, command_id, &done),
            ],
        )
        .map_err(ApiError::internal)?
    {
        return finish(state, command_id);
    }
    apply(state, command_id)
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
) -> ApiResult<Value> {
    let command_id = if requested_command_id.is_empty() {
        id("cmd")
    } else {
        requested_command_id.to_owned()
    };
    let request = json!({"command_type":"platform_submission.retry",
        "route_competition_id":route_competition_id,"payload":payload,
        "expected_version":expected_version});
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
                    "command_id was used with different submission retry content",
                ));
            }
            if previous["response"]["receipt"]["state"] == "accepted" {
                let _ = apply(state, &command_id);
            }
            return prior_receipt(state, &command_id)?
                .ok_or_else(|| ApiError::missing("submission retry command"));
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
        )? {
            continue;
        }
        return apply(state, &command_id);
    }
}

pub(super) fn reconcile(state: &Arc<AppState>) -> ApiResult<()> {
    for command in state.store.list(COMMANDS).map_err(ApiError::internal)? {
        if field(&command["request"], "command_type") != "platform_submission.retry"
            || command["response"]["receipt"]["state"] != "accepted"
        {
            continue;
        }
        let command_id = field(&command["response"]["receipt"], "command_id");
        if let Err(error) = apply(state, command_id) {
            tracing::warn!(command_id,code=%error.code,"Submission retry recovery remains pending");
        }
    }
    Ok(())
}
