//! Durable dispatch of approved Competition submissions.
use super::*;
const RESULTS:&str="competition_submission_delivery_results";
const RECONCILED:&str="competition_submission_reconciled_results";
mod wrong_binding;

const OUTBOX: &str = "competition_submission_outbox_live";
const EFFECTS: &str = "platform_effect_receipts";

pub(super) async fn submission_lock(state: &AppState, submission_id: &str) -> ApiResult<std::fs::File> {
    let path = native_chat::state_dir(state)
        .join("_platform")
        .join("submission-locks")
        .join(format!("{submission_id}.lock"));
    tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)
}

pub(super) fn stage(
    state: &AppState,
    competition_id: &str,
    kind: &str,
    command_id: &str,
    candidate: &Value,
    challenge: &Value,
    connection: &Value,
    attempt: u64,
) -> ApiResult<Value> {
    let command_id = if command_id.is_empty() {
        id("cmd")
    } else {
        command_id.to_owned()
    };
    let submission_id = id("sub");
    let outbox_id = id("sob");
    let effect_id = id("effect");
    let candidate_id = field(candidate, "candidate_id");
    let challenge_id = field(challenge, "challenge_id");
    let event_payload = json!({"submission_id":submission_id,"candidate_id":candidate_id,
        "challenge_id":challenge_id,"answer_slot":candidate["answer_slot"],"attempt":attempt});
    let mut events = vec![leases::planned_event(
        competition_id,
        "platform_submission",
        &submission_id,
        "competition.platform_submission.approved",
        &command_id,
        event_payload.clone(),
    )];
    let mut queued_payload = event_payload;
    queued_payload["state"] = json!("queued");
    events.push(leases::planned_event(
        competition_id,
        "platform_submission",
        &submission_id,
        "competition.platform_submission.queued",
        &command_id,
        queued_payload,
    ));
    let accepted = json!({"schema_version":1,"command_id":command_id,"receipt_id":id("rcpt"),
        "state":"accepted","run_id":null,"aggregate":{"type":"platform_submission","id":submission_id},
        "event_cursor":null,"deduplicated":false,"submitted_at":now(),"completed_at":null,
        "next":["receipt"],"output":{},"effect_ids":[],"error":null});
    let request = json!({"command_type":kind,"competition_id":competition_id,
        "candidate_id":candidate_id,"challenge_id":challenge_id});
    let submission = json!({"submission_id":submission_id,"competition_id":competition_id,
        "competition_challenge_id":challenge_id,"candidate_id":candidate_id,
        "answer_slot":candidate["answer_slot"],"digest":candidate["digest"],"attempt":attempt,
        "state":"queued","dispatch_count":0,"retry_after_at":null,"remote_receipt":"","last_error":"",
        "answer_ref":candidate["answer_ref"],"created_at":now(),"updated_at":now()});
    let destination = format!("platform.{}", field(connection, "platform_kind"));
    let idempotency_key = format!("platform_submission.submit:{submission_id}:attempt:{attempt}");
    let outbox = json!({"outbox_id":outbox_id,"command_id":command_id,"effect_id":effect_id,
        "correlation_id":command_id,"idempotency_key":idempotency_key,
        "aggregate_type":"platform_submission","aggregate_id":submission_id,
        "event_type":"competition.platform_submission.queued","destination":destination,
        "competition_id":competition_id,"challenge_id":challenge_id,"submission_id":submission_id,
        "op":"submit","status":"pending","attempts":0,"max_attempts":8,
        "next_attempt_at":null,"last_error":null,"delivered_at":null,
        "created_at":now(),"updated_at":now()});
    let effect = json!({"schema_version":1,"effect_id":effect_id,"command_id":command_id,
        "acceptance_receipt_id":accepted["receipt_id"],"domain":"competition","state":"waiting",
        "aggregate":{"type":"platform_submission","id":submission_id},
        "correlation_id":command_id,"outbox_id":outbox_id,"destination":destination,
        "idempotency_key":idempotency_key,"attempts":0,"max_attempts":8,
        "retry_after":null,"object_links":{"aggregate":format!("platform_submission:{submission_id}")},
        "output":{},"error":null,"created_at":now(),"updated_at":now(),"completed_at":null});
    let mut candidate_next = candidate.clone();
    candidate_next["state"] = json!("approved");
    candidate_next["updated_at"] = json!(now());
    let mut challenge_next = challenge.clone();
    challenge_next["state"] = json!("submitting");
    challenge_next["paused_from"] = Value::Null;
    challenge_next["updated_at"] = json!(now());
    let mut waiting_command = json!({"request":request,"response":{"receipt":accepted}});
    let waiting = &mut waiting_command["response"]["receipt"];
    waiting["state"] = json!("waiting");
    waiting["effect_ids"] = json!([effect_id]);
    waiting["output"] = json!({"submission_id":submission_id,"candidate_id":candidate_id,
        "state":"queued","outbox_ids":[outbox_id],"effect_ids":[effect_id],"effect_state":"waiting"});
    let receipt = waiting.clone();
    if put_records_with_events_returning(
        state,
        competition_id,
        &[
            (CANDIDATES, candidate_id, Some(candidate)),
            (CHALLENGES, challenge_id, Some(challenge)),
            (COMMANDS, &command_id, None),
            (SUBMISSIONS, &submission_id, None),
            (OUTBOX, &outbox_id, None),
            (EFFECTS, &effect_id, None),
        ],
        &[
            (CANDIDATES, candidate_id, &candidate_next),
            (CHALLENGES, challenge_id, &challenge_next),
            (COMMANDS, &command_id, &waiting_command),
            (SUBMISSIONS, &submission_id, &submission),
            (OUTBOX, &outbox_id, &outbox),
            (EFFECTS, &effect_id, &effect),
        ],
        &events,
    )?.is_none() {
        if let Some(prior) = state.store.get(COMMANDS, &command_id).map_err(ApiError::internal)? {
            if prior["request"] != waiting_command["request"] {
                return Err(ApiError::new(StatusCode::CONFLICT,
                    "competition.command.idempotency_conflict",
                    "Command ID was used with different submission"));
            }
            let mut receipt = prior["response"]["receipt"].clone();
            receipt["deduplicated"] = json!(true);
            return Ok(receipt);
        }
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "competition.submission.concurrent_update",
            "Submission changed concurrently",
        ));
    }
    Ok(receipt)
}

pub(super) async fn reconcile_startup(state: &Arc<AppState>) -> ApiResult<()> {
    for outbox in state.store.list(OUTBOX).map_err(ApiError::internal)? {
        if outbox["status"] != "processing" {
            continue;
        }
        let submission_id = field(&outbox, "submission_id");
        let _lock = submission_lock(state, submission_id).await?;
        if state.store.get(RESULTS,field(&outbox,"outbox_id")).map_err(ApiError::internal)?.is_some(){drop(_lock);settle_recorded(state,field(&outbox,"outbox_id")).await?;continue;}
        let Some(before_outbox) = state
            .store
            .get(OUTBOX, field(&outbox, "outbox_id"))
            .map_err(ApiError::internal)?
        else {
            continue;
        };
        if before_outbox["status"] != "processing" {
            continue;
        }
        let Some(before_submission) = state
            .store
            .get(SUBMISSIONS, submission_id)
            .map_err(ApiError::internal)?
        else {
            continue;
        };
        if !matches!(field(&before_submission,"state"),"queued" | "submitting") {
            let mut delivered = before_outbox.clone();
            delivered["status"] = json!("delivered");
            delivered["delivered_at"] = json!(now());
            delivered["updated_at"] = json!(now());
            let _ = state.store.put_many_if(
                &[(OUTBOX,field(&outbox,"outbox_id"),Some(&before_outbox)),
                  (SUBMISSIONS,submission_id,Some(&before_submission))],
                &[(OUTBOX,field(&outbox,"outbox_id"),&delivered)],
            ).map_err(ApiError::internal)?;
            continue;
        }
        let mut failed = before_outbox.clone();
        failed["status"] = json!("failed");
        failed["last_error"] = json!("consumer interrupted before acknowledgement");
        failed["next_attempt_at"] = json!(now());
        failed["updated_at"] = json!(now());
        let mut unknown = before_submission.clone();
        unknown["state"] = json!("unknown");
        unknown["last_error"] = json!("consumer interrupted before acknowledgement");
        unknown["updated_at"] = json!(now());
        let effect_id = field(&outbox, "effect_id");
        let before_effect = state
            .store
            .get(EFFECTS, effect_id)
            .map_err(ApiError::internal)?;
        let next_effect = before_effect.as_ref().map(|prior| {
            let mut next = prior.clone();
            next["state"] = json!("waiting");
            next["attempts"] = failed["attempts"].clone();
            next["updated_at"] = json!(now());
            next
        });
        let mut expected = vec![
            (OUTBOX, field(&outbox, "outbox_id"), Some(&before_outbox)),
            (SUBMISSIONS, submission_id, Some(&before_submission)),
        ];
        let mut records = vec![
            (OUTBOX, field(&outbox, "outbox_id"), &failed),
            (SUBMISSIONS, submission_id, &unknown),
        ];
        let command_id = field(&outbox, "command_id");
        let before_command = state
            .store
            .get(COMMANDS, command_id)
            .map_err(ApiError::internal)?;
        let next_command = before_command.as_ref().map(|prior| {
            let mut next = prior.clone();
            next["response"]["receipt"]["state"] = json!("waiting");
            next["response"]["receipt"]["output"]["effect_state"] = json!("waiting");
            next
        });
        if let (Some(prior), Some(next)) = (&before_effect, &next_effect) {
            expected.push((EFFECTS, effect_id, Some(prior)));
            records.push((EFFECTS, effect_id, next));
        }
        if let (Some(prior), Some(next)) = (&before_command, &next_command) {
            expected.push((COMMANDS, command_id, Some(prior)));
            records.push((COMMANDS, command_id, next));
        }
        let _ = state
            .store
            .put_many_if(&expected, &records)
            .map_err(ApiError::internal)?;
    }
    for competition in state.store.list(COMPETITIONS).map_err(ApiError::internal)? {
        if competition["archived"] == true && competition["historical"] != true {
            reconcile_graph_verdicts(state, field(&competition, "competition_id"))?;
        }
    }
    Ok(())
}

pub(super) async fn maintenance(state: &Arc<AppState>, competition_id: &str) -> ApiResult<()> {
    reconcile_held_candidates(state, competition_id).await?;
    reconcile_verdict_events(state, competition_id).await?;
    align_correct_verdicts(state, competition_id).await?;
    auto_requeue(state, competition_id)?;
    enqueue_queued_without_pending(state, competition_id)?;
    for outbox in state.store.list(OUTBOX).map_err(ApiError::internal)? {
        if outbox["competition_id"] != competition_id {
            continue;
        }
        if outbox["status"]=="processing" && state.store.get(RESULTS,field(&outbox,"outbox_id")).map_err(ApiError::internal)?.is_some(){
            settle_recorded(state,field(&outbox,"outbox_id")).await?;
        }else if outbox["status"] == "failed" {
            finalize_unknown(state, &outbox).await?;
        } else if outbox["status"] == "pending" {
            if let Err(error) = deliver(state, &outbox).await {
                tracing::warn!(outbox_id=%field(&outbox,"outbox_id"),code=%error.code,
                    "Competition submission delivery failed");
            }
        }
    }
    for submission in scoped(state, SUBMISSIONS, competition_id)? {
        if submission["state"] == "unknown" {
            if let Err(error) = reconcile_unknown(state, &submission).await {
                tracing::warn!(submission_id=%field(&submission,"submission_id"),code=%error.code,
                    "Competition submission poll failed");
            }
        }
    }
    reconcile_graph_verdicts(state, competition_id)?;
    for submission in scoped(state, SUBMISSIONS, competition_id)? {
        if submission["state"] == "wrong"
            || (submission["state"] == "cancelled" && submission["cancel_reason"] == "stale_execution_generation") {
            if let Some(candidate) = state.store.get(CANDIDATES, field(&submission,"candidate_id"))
                .map_err(ApiError::internal)? {
                let _ = project_flag_invalidation(state, &candidate, field(&submission,"remote_receipt"));
                if submission["state"] == "wrong" {
                    if let Err(error) = wrong_binding::progress(state, &submission, &candidate).await {
                        tracing::debug!(submission_id=%field(&submission,"submission_id"),code=%error.code,
                            "Competition wrong-verdict Run continuation failed");
                    }
                }
            }
        }
    }
    for challenge in scoped(state, CHALLENGES, competition_id)? {
        if challenge["state"] == "solved" {
            settle_solved_binding(state, competition_id, field(&challenge,"challenge_id")).await?;
        }
    }
    Ok(())
}

fn enqueue_queued_without_pending(state: &AppState, competition_id: &str) -> ApiResult<()> {
    let outboxes = scoped(state,OUTBOX,competition_id)?;
    for submission in scoped(state,SUBMISSIONS,competition_id)? {
        if submission["state"] != "queued" {continue;}
        let submission_id = field(&submission,"submission_id");
        let previous = outboxes.iter().filter(|row|row["submission_id"] == submission_id)
            .collect::<Vec<_>>();
        if previous.iter().any(|row|matches!(field(row,"status"),"pending" | "processing")) {
            continue;
        }
        let number = previous.len() + 1;
        let outbox_id = format!("sob-retry-{submission_id}-{number}");
        let competition = live_competition(state,competition_id)?;
        let connection = state.store.get(CONNECTIONS,field(&competition,"connection_id"))
            .map_err(ApiError::internal)?;
        let destination = connection.as_ref().map(|row|format!("platform.{}",field(row,"platform_kind")))
            .unwrap_or_default();
        let outbox = json!({"outbox_id":outbox_id,"command_id":null,"effect_id":null,
            "correlation_id":"","idempotency_key":format!("platform_submission.submit:{submission_id}:{number}"),
            "aggregate_type":"platform_submission","aggregate_id":submission_id,
            "event_type":"competition.platform_submission.queued","destination":destination,
            "competition_id":competition_id,"challenge_id":submission["competition_challenge_id"],
            "submission_id":submission_id,"op":"submit","status":"pending",
            "attempts":0,"max_attempts":8,"next_attempt_at":null,"last_error":null,
            "delivered_at":null,"created_at":now(),"updated_at":now()});
        let _ = state.store.put_many_if(&[(SUBMISSIONS,submission_id,Some(&submission)),
            (OUTBOX,&outbox_id,None)],&[(OUTBOX,&outbox_id,&outbox)])
            .map_err(ApiError::internal)?;
    }
    Ok(())
}

async fn align_correct_verdicts(state: &Arc<AppState>, competition_id: &str) -> ApiResult<()> {
    for item in scoped(state, SUBMISSIONS, competition_id)? {
        if !matches!(field(&item,"state"),"correct" | "duplicate_or_solved") { continue; }
        let submission_id = field(&item,"submission_id");
        let challenge_id = field(&item,"competition_challenge_id");
        let _lock = submission_lock(state,&format!("challenge-{challenge_id}")).await?;
        let Some(submission) = state.store.get(SUBMISSIONS,submission_id)
            .map_err(ApiError::internal)? else { continue; };
        let verdict = field(&submission,"state");
        if !matches!(verdict,"correct" | "duplicate_or_solved") { continue; }
        let Some(challenge) = state.store.get(CHALLENGES,challenge_id)
            .map_err(ApiError::internal)? else { continue; };
        let candidate_id = field(&submission,"candidate_id");
        let candidate = state.store.get(CANDIDATES,candidate_id).map_err(ApiError::internal)?;
        let next_candidate = candidate.as_ref().map(|prior| {
            let mut next = prior.clone();
            if next["state"] != "submitted" {
                next["state"] = json!("submitted");
                next["updated_at"] = json!(now());
            }
            next
        });
        let next_challenge = if challenge["state"] == "solved" { challenge.clone() }
            else { challenge_after_verdict(state,competition_id,&challenge,&submission,verdict)? };
        let source_outbox = scoped(state,OUTBOX,competition_id)?.into_iter().find(|outbox| {
            outbox["submission_id"] == submission_id && outbox["status"] == "delivered"
                && !field(outbox,"effect_id").is_empty()
        });
        let effect_id = source_outbox.as_ref().map_or("",|outbox|field(outbox,"effect_id"));
        let command_id = source_outbox.as_ref().map_or("",|outbox|field(outbox,"command_id"));
        let prior_effect = if effect_id.is_empty() { None } else {
            state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)?
        };
        let next_effect = prior_effect.as_ref().and_then(|prior| {
            if prior["state"] == "completed" && prior["output"]["state"] != "unknown" {
                return None;
            }
            let mut next = prior.clone();
            next["state"] = json!("completed");
            next["output"] = json!({"submission_id":submission_id,"state":verdict,
                "remote_receipt":submission["remote_receipt"]});
            next["updated_at"] = json!(now());
            if next["completed_at"].is_null() { next["completed_at"] = json!(now()); }
            Some(next)
        });
        let prior_command = if command_id.is_empty() { None } else {
            state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
        };
        let next_command = prior_command.as_ref().and_then(|prior| {
            let receipt = &prior["response"]["receipt"];
            if receipt["output"]["submission_id"] != submission_id
                || (receipt["state"] == "completed" && receipt["output"]["state"] != "unknown") {
                return None;
            }
            let mut next = prior.clone();
            next["response"]["receipt"]["state"] = json!("completed");
            if next["response"]["receipt"]["completed_at"].is_null() {
                next["response"]["receipt"]["completed_at"] = json!(now());
            }
            next["response"]["receipt"]["output"]["state"] = json!(verdict);
            next["response"]["receipt"]["output"]["effect_state"] = json!("completed");
            next["response"]["receipt"]["output"]["remote_receipt"] = submission["remote_receipt"].clone();
            Some(next)
        });
        let mut expected = vec![(SUBMISSIONS,submission_id,Some(&submission))];
        let mut records = Vec::new();
        let mut events = Vec::new();
        if next_challenge != challenge {
            expected.push((CHALLENGES,challenge_id,Some(&challenge)));
            records.push((CHALLENGES,challenge_id,&next_challenge));
            if next_challenge["state"] != challenge["state"] {
                events.push(leases::planned_event(competition_id,"challenge",challenge_id,
                    "competition.challenge.state_changed","",
                    json!({"challenge_id":challenge_id,"from":challenge["state"],
                        "to":next_challenge["state"],"reason":"platform_verdict_recovered"})));
            }
        }
        if let (Some(prior),Some(next)) = (&candidate,&next_candidate) {
            if next != prior {
                expected.push((CANDIDATES,candidate_id,Some(prior)));
                records.push((CANDIDATES,candidate_id,next));
            }
        }
        if let (Some(prior),Some(next)) = (&prior_effect,&next_effect) {
            expected.push((EFFECTS,effect_id,Some(prior)));
            records.push((EFFECTS,effect_id,next));
        }
        if let (Some(prior),Some(next)) = (&prior_command,&next_command) {
            expected.push((COMMANDS,command_id,Some(prior)));
            records.push((COMMANDS,command_id,next));
        }
        if !records.is_empty() {
            let _ = put_records_with_events(state,competition_id,&expected,&records,&events)?;
        }
    }
    Ok(())
}

pub(super) async fn reconcile_verdict_events(state: &Arc<AppState>, competition_id: &str) -> ApiResult<()> {
    let observed = scoped(state, EVENTS, competition_id)?.into_iter()
        .filter(|event| event["aggregate_type"] == "platform_submission"
            && event["event_type"] == "competition.platform_submission.state_changed")
        .map(|event| (field(&event,"aggregate_id").to_owned(),
            field(&event["payload"],"to").to_owned()))
        .collect::<std::collections::HashSet<_>>();
    for item in scoped(state, SUBMISSIONS, competition_id)? {
        let submission_id = field(&item, "submission_id");
        if !matches!(field(&item, "state"), "correct" | "wrong" | "duplicate_or_solved") {
            continue;
        }
        if observed.contains(&(submission_id.to_owned(), field(&item,"state").to_owned())) {
            continue;
        }
        let _lock = submission_lock(state, submission_id).await?;
        let Some(submission) = state.store.get(SUBMISSIONS, submission_id)
            .map_err(ApiError::internal)? else { continue; };
        let verdict = field(&submission, "state");
        if !matches!(verdict, "correct" | "wrong" | "duplicate_or_solved") {
            continue;
        }
        if scoped(state, EVENTS, competition_id)?.iter().any(|event| {
            event["aggregate_type"] == "platform_submission"
                && event["aggregate_id"] == submission_id
                && event["event_type"] == "competition.platform_submission.state_changed"
                && event["payload"]["to"] == verdict
        }) { continue; }
        append_event(state, competition_id, "platform_submission", submission_id,
            "competition.platform_submission.state_changed", "",
            json!({"submission_id":submission_id,"candidate_id":submission["candidate_id"],
                "challenge_id":submission["competition_challenge_id"],"from":"submitting",
                "to":verdict,"digest":submission["digest"],"recovered":true}))?;
    }
    Ok(())
}

fn max_transient_retries(connection: &Value) -> u64 {
    for value in [&connection["capabilities"]["max_transient_retries"],
        &connection["capabilities"]["detail"]["max_transient_retries"]] {
        if let Some(count) = value.as_u64().or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok())) {
            return count;
        }
    }
    3
}

fn transient_failure_count(state: &AppState, submission: &Value) -> ApiResult<u64> {
    if let Some(count) = submission["transient_failures"].as_u64() {
        return Ok(count);
    }
    let submission_id = field(submission, "submission_id");
    Ok(state.store.list(EVENTS).map_err(ApiError::internal)?.iter()
        .filter(|event| event["aggregate_type"] == "platform_submission"
            && event["aggregate_id"] == submission_id
            && event["event_type"] == "competition.platform_submission.state_changed"
            && event["payload"]["to"] == "transient_failure")
        .count() as u64)
}

fn auto_requeue(state: &Arc<AppState>, competition_id: &str) -> ApiResult<()> {
    for submission in scoped(state, SUBMISSIONS, competition_id)? {
        let state_name = field(&submission, "state");
        let reason = match state_name {
            "rate_limited" => {
                let due = submission["retry_after_at"].as_str()
                    .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok());
                if due.is_none_or(|at| at > chrono::Utc::now()) { continue; }
                "retry_after_reached"
            }
            "transient_failure" => {
                if submission["retry_safe"]!=true {
                    let submission_id=field(&submission,"submission_id");
                    let mut unknown=submission.clone();unknown["state"]=json!("unknown");unknown["certainty"]=json!("unknown");unknown["retry_safe"]=json!(false);
                    unknown["last_error"]=json!("legacy_dispatch_certainty_missing");unknown["updated_at"]=json!(now());
                    let event=leases::planned_event(competition_id,"platform_submission",submission_id,"competition.platform_submission.state_changed","",json!({"submission_id":submission_id,"from":"transient_failure","to":"unknown","reason":"dispatch_certainty_missing"}));
                    put_records_with_events(state,competition_id,&[(SUBMISSIONS,submission_id,Some(&submission))],&[(SUBMISSIONS,submission_id,&unknown)],&[event])?;
                    continue;
                }
                let competition = live_competition(state, competition_id)?;
                let connection = live_connection(state, field(&competition, "connection_id"))?;
                if transient_failure_count(state, &submission)? > max_transient_retries(&connection) {
                    continue;
                }
                "transient_retry"
            }
            _ => continue,
        };
        let submission_id = field(&submission, "submission_id");
        let existing = state.store.list(OUTBOX).map_err(ApiError::internal)?
            .into_iter().filter(|row| row["submission_id"] == submission_id).collect::<Vec<_>>();
        if existing.iter().any(|row| matches!(field(row, "status"), "pending" | "processing")) {
            continue;
        }
        let next_number = existing.len() + 1;
        let outbox_id = id("sob");
        let competition = live_competition(state, competition_id)?;
        let connection = live_connection(state, field(&competition, "connection_id"))?;
        let outbox = json!({"outbox_id":outbox_id,"command_id":null,"effect_id":null,
            "correlation_id":"","idempotency_key":format!("platform_submission.submit:{submission_id}:{next_number}"),
            "aggregate_type":"platform_submission","aggregate_id":submission_id,
            "event_type":"competition.platform_submission.queued",
            "destination":format!("platform.{}",field(&connection,"platform_kind")),
            "competition_id":competition_id,"challenge_id":submission["competition_challenge_id"],
            "submission_id":submission_id,"op":"submit","status":"pending","attempts":0,"max_attempts":8,
            "next_attempt_at":null,"last_error":null,"delivered_at":null,
            "created_at":now(),"updated_at":now()});
        let mut queued = submission.clone();
        queued["state"] = json!("queued");
        queued["retry_after_at"] = Value::Null;
        queued["updated_at"] = json!(now());
        if !state.store.put_many_if(
            &[(SUBMISSIONS, submission_id, Some(&submission)),(OUTBOX, &outbox_id, None)],
            &[(SUBMISSIONS, submission_id, &queued),(OUTBOX, &outbox_id, &outbox)],
        ).map_err(ApiError::internal)? { continue; }
        append_event(state,competition_id,"platform_submission",submission_id,
            "competition.platform_submission.state_changed","",
            json!({"submission_id":submission_id,"from":state_name,"to":"queued","reason":reason}))?;
        append_event(state,competition_id,"platform_submission",submission_id,
            "competition.platform_submission.requeued","",
            json!({"submission_id":submission_id,"reason":reason}))?;
    }
    Ok(())
}

fn active_binding(state: &AppState, competition_id: &str, challenge_id: &str) -> ApiResult<Option<Value>> {
    Ok(scoped(state, BINDINGS, competition_id)?.into_iter()
        .filter(|binding| binding["competition_challenge_id"] == challenge_id)
        .filter(|binding| matches!(field(binding, "state"),
            "planned" | "creating" | "starting" | "running" | "local_finished" |
            "remote_pending" | "rejected" | "resolving" | "paused" | "finished"))
        .max_by(|a,b| field(a,"updated_at").cmp(field(b,"updated_at"))
            .then_with(|| field(a,"binding_id").cmp(field(b,"binding_id")))))
}

async fn reconcile_held_candidates(state: &Arc<AppState>, competition_id: &str) -> ApiResult<()> {
    let policy = state.store.get(POLICIES,competition_id).map_err(ApiError::internal)?;
    if policy.as_ref().is_none_or(|row| row["automation_mode"] != "autonomous") { return Ok(()); }
    let _admission_lock = submission_lock(state,&format!("admission-{competition_id}")).await?;
    for candidate in scoped(state,CANDIDATES,competition_id)? {
        let from = field(&candidate,"state");
        if !matches!(from,"candidate" | "awaiting_approval") { continue; }
        let challenge_id = field(&candidate,"competition_challenge_id");
        let Some(binding) = active_binding(state,competition_id,challenge_id)? else { continue; };
        let source_generation = candidate["source_execution_generation"].as_u64().unwrap_or(0);
        let current_generation = binding["execution_generation"].as_u64().unwrap_or(0);
        if source_generation == 0 || current_generation <= source_generation { continue; }
        let Some(challenge) = state.store.get(CHALLENGES,challenge_id).map_err(ApiError::internal)?
            else { continue; };
        let peers = scoped(state,CANDIDATES,competition_id)?.iter().any(|other| {
            other["competition_challenge_id"] == challenge_id
                && other["candidate_id"] != candidate["candidate_id"]
                && matches!(field(other,"state"),"candidate" | "awaiting_approval" | "approved")
                && (other["source_execution_generation"].as_u64().unwrap_or(0) == 0
                    || other["source_execution_generation"].as_u64().unwrap_or(0) >= current_generation)
        });
        let target = if from == "candidate" { "superseded" } else { "cancelled" };
        let mut next_candidate = candidate.clone();
        next_candidate["state"] = json!(target);
        next_candidate["updated_at"] = json!(now());
        let mut next_challenge = challenge.clone();
        if !peers && challenge["state"] == "candidate_found" {
            next_challenge["state"] = json!("running");
            next_challenge["paused_from"] = Value::Null;
            next_challenge["updated_at"] = json!(now());
        }
        if !state.store.put_many_if(
            &[(CANDIDATES,field(&candidate,"candidate_id"),Some(&candidate)),
                (CHALLENGES,challenge_id,Some(&challenge)),
                (BINDINGS,field(&binding,"binding_id"),Some(&binding))],
            &[(CANDIDATES,field(&candidate,"candidate_id"),&next_candidate),
                (CHALLENGES,challenge_id,&next_challenge)],
        ).map_err(ApiError::internal)? { continue; }
        append_event(state,competition_id,"submission_candidate",field(&candidate,"candidate_id"),
            "competition.submission.candidate_state_changed","",
            json!({"candidate_id":candidate["candidate_id"],"challenge_id":challenge_id,
                "from":from,"to":target,"reason":"stale_execution_generation",
                "source_execution_generation":source_generation,
                "current_execution_generation":current_generation}))?;
        if next_challenge != challenge {
            append_event(state,competition_id,"challenge",challenge_id,
                "competition.challenge.state_changed","",
                json!({"challenge_id":challenge_id,"from":"candidate_found","to":"running",
                    "reason":"stale_execution_generation"}))?;
        }
        let _ = project_flag_invalidation(state,&candidate,
            &format!("candidate expired after dynamic instance replacement generation {source_generation}->{current_generation}"));
    }
    let mut pending = scoped(state,CANDIDATES,competition_id)?.into_iter()
        .filter(|candidate| matches!(field(candidate,"state"),"candidate" | "awaiting_approval"))
        .collect::<Vec<_>>();
    pending.sort_by(|a,b| field(a,"created_at").cmp(field(b,"created_at"))
        .then_with(||field(a,"candidate_id").cmp(field(b,"candidate_id"))));
    for candidate in pending {
        if admit_autonomous_candidate(state,competition_id,&candidate)? { break; }
    }
    Ok(())
}

fn autonomous_admission_allowed(
    state: &AppState, competition_id: &str, connection: &Value,
) -> ApiResult<bool> {
    if connection["status"] != "active"
        || connection["capabilities"]["automation_allowed"] == false
        || connection["capabilities"]["detail"]["automation_allowed"] == false {
        return Ok(false);
    }
    if !matches!(field(&connection,"platform_kind"),"ctfd"|"rctf"|"gzctf"|"generic_browser") && !extension_adapter::supported(state,field(&connection,"platform_kind"))?{return Ok(false);}
    let moment = chrono::Utc::now();
    if connection["capabilities"]["submission_cooldown_until"].as_str()
        .and_then(|text|chrono::DateTime::parse_from_rfc3339(text).ok())
        .is_some_and(|until|until > moment) { return Ok(false); }
    let policy = state.store.get(POLICIES,competition_id).map_err(ApiError::internal)?;
    if policy.as_ref().is_none_or(|row|row["automation_mode"] != "autonomous") {
        return Ok(false);
    }
    let cooldown = policy.as_ref().and_then(|row|row["submission_cooldown_seconds"].as_f64())
        .unwrap_or(0.0);
    let submissions = scoped(state,SUBMISSIONS,competition_id)?;
    if cooldown > 0.0 {
        let last = submissions.iter().filter(|row|row["state"] != "queued")
            .filter_map(|row|row["updated_at"].as_str()
                .and_then(|text|chrono::DateTime::parse_from_rfc3339(text).ok()))
            .max();
        if last.is_some_and(|at| moment.signed_duration_since(at).num_milliseconds()
            < (cooldown * 1000.0) as i64) { return Ok(false); }
    }
    let budget = state.store.get(BUDGETS,&format!("{competition_id}:submissions"))
        .map_err(ApiError::internal)?;
    let limit = budget.as_ref().and_then(|row|row["limit"].as_f64())
        .or_else(||policy.as_ref().and_then(|row|row["budget_limits"]["submissions"].as_f64()))
        .unwrap_or(0.0);
    let used = budget.as_ref().and_then(|row|row["used"].as_f64()).unwrap_or(0.0);
    Ok(limit <= 0.0 || used < limit)
}

pub(super) fn admit_autonomous_candidate(
    state: &AppState, competition_id: &str, candidate: &Value,
) -> ApiResult<bool> {
    let candidate_id = field(candidate,"candidate_id");
    let Some(candidate) = state.store.get(CANDIDATES,candidate_id)
        .map_err(ApiError::internal)? else { return Ok(false); };
    if !matches!(field(&candidate,"state"),"candidate" | "awaiting_approval")
        || scoped(state,SUBMISSIONS,competition_id)?.iter()
            .any(|submission|submission["candidate_id"] == candidate_id) {
        return Ok(false);
    }
    let challenge_id = field(&candidate,"competition_challenge_id");
    let Some(challenge) = state.store.get(CHALLENGES,challenge_id)
        .map_err(ApiError::internal)? else { return Ok(false); };
    if challenge["tombstoned"] == true || challenge["state"] == "solved" { return Ok(false); }
    let Some(competition) = state.store.get(COMPETITIONS,competition_id)
        .map_err(ApiError::internal)? else { return Ok(false); };
    if competition["archived"] == true || competition["historical"] == true { return Ok(false); }
    let Some(connection) = state.store.get(CONNECTIONS,field(&competition,"connection_id"))
        .map_err(ApiError::internal)? else { return Ok(false); };
    if connection["archived"] == true || connection["historical"] == true { return Ok(false); }
    if !autonomous_admission_allowed(state,competition_id,&connection)? { return Ok(false); }
    let command_id = format!("competition-gate:{candidate_id}");
    stage(state,competition_id,"competition.submission.submit",&command_id,
        &candidate,&challenge,&connection,1)?;
    Ok(true)
}

fn project_flag_invalidation(state: &Arc<AppState>, candidate: &Value, receipt: &str) -> ApiResult<bool> {
    let run_id = field(candidate,"source_run_id");
    if run_id.is_empty() { return Ok(false); }
    if !state.runs.claim_execution_owner(state, run_id).map_err(ApiError::internal)? {
        return Ok(false);
    }
    let candidate_id = field(candidate,"candidate_id");
    let projected = scoped(state, EVENTS, field(candidate,"competition_id"))?.iter().any(|event| {
        event["event_type"] == "competition.platform_submission.invalidation_projected"
            && event["payload"]["candidate_id"] == candidate_id
    });
    let Some(run) = state.store.get("runs",run_id).map_err(ApiError::internal)? else { return Ok(false); };
    let Ok(value) = read_secret(state,field(candidate,"answer_ref")) else { return Ok(false); };
    if projected {
        state.runs.handle(state,run_id).rejected_flags.lock().unwrap().insert(value.clone());
        restore_run_invalidation_event(state, candidate, &value);
        return Ok(false);
    }
    let Ok(mut graph) = super::super::graph::SharedGraph::open(state,run_id) else { return Ok(false); };
    let reason = format!("remote verdict wrong (submission of digest {}): {}",
        field(candidate,"digest").chars().take(12).collect::<String>(),
        receipt.chars().take(200).collect::<String>());
    let Ok(reopened) = graph.reopen_after_false_positive("competition",&value,&reason)
        else { return Ok(false); };
    let mut next_run = run.clone();
    let flags = run["flags"].as_array().into_iter().flatten()
        .filter_map(Value::as_str).filter(|flag| *flag != value.as_str()).collect::<Vec<_>>();
    next_run["flags"] = json!(flags);
    next_run["flag"] = flags.first().map_or(Value::Null,|flag| json!(flag));
    next_run["solved"] = json!(false);
    next_run["updated"] = json!(chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
    if !state.store.put_many_if(&[("runs",run_id,Some(&run))],&[("runs",run_id,&next_run)])
        .map_err(ApiError::internal)? { return Ok(false); }
    state.runs.handle(state,run_id).rejected_flags.lock().unwrap().insert(value.clone());
    restore_run_invalidation_event_with_intents(state,candidate,&value,Some(&reopened));
    append_event(state,field(candidate,"competition_id"),"platform_submission",candidate_id,
        "competition.platform_submission.invalidation_projected","",
        json!({"candidate_id":candidate_id,"digest":candidate["digest"],
            "source_run_id":run_id,"challenge_id":candidate["competition_challenge_id"]}))?;
    Ok(true)
}

fn restore_run_invalidation_event(state: &AppState, candidate: &Value, value: &str) {
    restore_run_invalidation_event_with_intents(state, candidate, value, None);
}

fn restore_run_invalidation_event_with_intents(
    state: &AppState, candidate: &Value, value: &str, reopened: Option<&[String]>,
) {
    let run_id = field(candidate,"source_run_id");
    let candidate_id = field(candidate,"candidate_id");
    if super::super::read_jsonl_event_iter(state,run_id).any(|event| {
        event["event_type"] == "blackboard.delta"
            && event["payload"]["kind"] == "flag_invalidated"
            && event["payload"]["actor"] == "competition"
            && (event["payload"]["candidate_id"] == candidate_id
                || event["payload"]["flag"] == value)
    }) { return; }
    let mut payload = json!({"kind":"flag_invalidated","flag":value,
        "actor":"competition","candidate_id":candidate_id});
    if let Some(reopened) = reopened {
        payload["reopened_intents"] = json!(reopened);
    } else {
        payload["recovered"] = json!(true);
    }
    super::super::emit_event(state,run_id,None,"blackboard.delta",payload);
}

pub(super) async fn settle_solved_binding(state: &Arc<AppState>, competition_id: &str, challenge_id: &str) -> ApiResult<()> {
    let binding = match active_binding(state,competition_id,challenge_id)? {
        Some(binding)=>Some(binding),
        None=>scoped(state,BINDINGS,competition_id)?.into_iter()
            .filter(|binding|binding["competition_challenge_id"]==challenge_id && binding["state"]=="solved")
            .max_by(|a,b|field(a,"updated_at").cmp(field(b,"updated_at"))),
    };
    let Some(binding) = binding else { return Ok(()); };
    let run_id = field(&binding,"run_id");
    if run_id.is_empty() { return Ok(()); }
    let run = state.store.get("runs",run_id).map_err(ApiError::internal)?;
    let run_generation = run.as_ref().and_then(|run|run["execution_generation"].as_u64());
    let binding_generation = binding["execution_generation"].as_u64();
    if run_generation.is_none() || run_generation != binding_generation {
        let mut pending = binding.clone();
        pending["gateway_stop_error"] = json!(format!(
            "stale run binding while settling solved challenge: binding generation {:?}, run generation {:?}",
            binding_generation,run_generation));
        if pending != binding {
            let _ = state.store.put_many_if(
                &[(BINDINGS,field(&binding,"binding_id"),Some(&binding))],
                &[(BINDINGS,field(&binding,"binding_id"),&pending)]).map_err(ApiError::internal)?;
        }
        return Ok(());
    }
    if let Some(run) = run {
        if run["running"] == true || run["solved"] != true {
            let command_id = format!("competition-solved:{}:{}:complete",
                field(&binding,"binding_id"),binding["execution_generation"].as_u64().unwrap_or(0));
            let response = super::super::control_run(
                State(state.clone()),Path(run_id.to_owned()),
                Json(json!({"action":"complete","text":"platform_verdict_correct","command_id":command_id})),
            ).await;
            if !response.status().is_success() {
                let mut pending = binding.clone();
                pending["gateway_stop_error"] = json!(format!("Run complete returned HTTP {}",response.status().as_u16()));
                let _ = state.store.put_many_if(
                    &[(BINDINGS,field(&binding,"binding_id"),Some(&binding))],
                    &[(BINDINGS,field(&binding,"binding_id"),&pending)]).map_err(ApiError::internal)?;
                return Ok(());
            }
        }
    }
    let from = field(&binding,"state");
    let to = if matches!(from,"running" | "paused" | "finished" | "local_finished" | "remote_pending" | "solved") {
        "solved"
    } else { "stopped" };
    let previous_queue=state.store.get(QUEUE,challenge_id).map_err(ApiError::internal)?;
    let next_queue=previous_queue.as_ref().filter(|queue|matches!(field(queue,"state"),"queued"|"running"|"dispatching"|"dispatched")).map(|queue|{
        let mut next=queue.clone();next["state"]=json!("done");next["updated_at"]=json!(now());next
    });
    if from==to && next_queue.is_none() && binding["gateway_stop_error"].is_null(){return Ok(());}
    let mut next = binding.clone();
    next["state"] = json!(to);
    next["updated_at"] = json!(now());
    next["gateway_stop_error"] = Value::Null;
    let event = leases::planned_event(competition_id,"run_binding",field(&binding,"binding_id"),
        "competition.run_binding.state_changed","",
        json!({"binding_id":binding["binding_id"],"challenge_id":challenge_id,
            "run_id":run_id,"from":from,"to":to,"reason":"challenge_solved"}));
    let mut expected=vec![(BINDINGS,field(&binding,"binding_id"),Some(&binding))];
    let mut records=vec![(BINDINGS,field(&binding,"binding_id"),&next)];
    if let (Some(previous),Some(next))=(&previous_queue,&next_queue){expected.push((QUEUE,challenge_id,Some(previous)));records.push((QUEUE,challenge_id,next));}
    if !put_records_with_events(state,competition_id,&expected,&records,&[event])? {
        return Ok(());
    }
    Ok(())
}

fn challenge_after_verdict(
    state: &AppState, competition_id: &str, challenge: &Value,
    submission: &Value, verdict: &str,
) -> ApiResult<Value> {
    let mut next = challenge.clone();
    let challenge_id = field(challenge, "challenge_id");
    if matches!(verdict, "correct" | "duplicate_or_solved") {
        let expected = state.store.get(REVISIONS, field(challenge,"current_revision_id"))
            .map_err(ApiError::internal)?.and_then(|revision| revision["expected_flags"].as_u64())
            .unwrap_or(1).max(1);
        let mut digests = std::collections::HashSet::new();
        let mut pending_other = false;
        for other in scoped(state, SUBMISSIONS, competition_id)? {
            if other["competition_challenge_id"] != challenge_id { continue; }
            let same = other["submission_id"] == submission["submission_id"];
            let state_name = if same { verdict } else { field(&other,"state") };
            if !same && matches!(state_name,"queued" | "submitting") { pending_other = true; }
            if state_name == "correct" || (expected == 1 && state_name == "duplicate_or_solved") {
                if let Some(digest) = other["digest"].as_str() { digests.insert(digest.to_owned()); }
            }
        }
        if digests.len() as u64 >= expected {
            next["state"] = json!("solved");
            next["remote_state"] = json!("solved_remote");
            next["paused_from"] = Value::Null;
        } else if !pending_other && next["state"] == "submitting" {
            next["state"] = json!("running");
            next["paused_from"] = Value::Null;
        }
    } else if verdict == "wrong" && next["state"] == "submitting" {
        next["state"] = json!("running");
    }
    if next != *challenge { next["updated_at"] = json!(now()); }
    Ok(next)
}

fn verdict_projection_snapshot(
    state: &AppState, competition_id: &str, challenge: &Value,
    submission: &Value, verdict: &str, next_challenge: &Value,
    detail: &str,
) -> ApiResult<Value> {
    let expected = state.store.get(REVISIONS, field(challenge,"current_revision_id"))
        .map_err(ApiError::internal)?
        .and_then(|revision| revision["expected_flags"].as_u64())
        .filter(|count| *count > 0);
    let threshold = expected.unwrap_or(1);
    let mut confirmed = std::collections::HashSet::new();
    for other in scoped(state, SUBMISSIONS, competition_id)? {
        if other["competition_challenge_id"] != challenge["challenge_id"] { continue; }
        let state_name = if other["submission_id"] == submission["submission_id"] {
            verdict
        } else {
            field(&other,"state")
        };
        if state_name == "correct" || (threshold == 1 && state_name == "duplicate_or_solved") {
            let digest = field(&other,"digest");
            if !digest.is_empty() { confirmed.insert(digest.to_owned()); }
        }
    }
    Ok(json!({"completed":next_challenge["state"] == "solved",
        "correct_flag_count":confirmed.len() as u64,
        "total_flag_count":expected,"detail":detail}))
}

fn historical_verdict_projection(state: &AppState, submission: &Value) -> ApiResult<Value> {
    let competition_id = field(submission,"competition_id");
    let challenge_id = field(submission,"competition_challenge_id");
    let challenge = state.store.get(CHALLENGES,challenge_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("challenge"))?;
    let expected = state.store.get(REVISIONS,field(&challenge,"current_revision_id"))
        .map_err(ApiError::internal)?
        .and_then(|revision| revision["expected_flags"].as_u64())
        .filter(|count| *count > 0);
    let threshold = expected.unwrap_or(1);
    let events = scoped(state,EVENTS,competition_id)?;
    let submission_id = field(submission,"submission_id");
    let verdict = field(submission,"state");
    let settled_seq = events.iter().filter(|event| {
        event["event_type"] == "competition.platform_submission.state_changed"
            && event["payload"]["submission_id"] == submission_id
            && event["payload"]["to"] == verdict
    }).filter_map(|event|event["global_seq"].as_u64()).max();
    let mut confirmed = std::collections::HashSet::new();
    for event in &events {
        if event["event_type"] != "competition.platform_submission.state_changed"
            || event["payload"]["challenge_id"] != challenge_id
            || settled_seq.is_some_and(|seq|event["global_seq"].as_u64().unwrap_or(u64::MAX)>seq)
        { continue; }
        let state_name = field(&event["payload"],"to");
        if state_name == "correct" || (threshold == 1 && state_name == "duplicate_or_solved") {
            let digest = field(&event["payload"],"digest");
            if !digest.is_empty() { confirmed.insert(digest.to_owned()); }
        }
    }
    let completed = settled_seq.map_or(challenge["state"] == "solved", |seq| {
        events.iter().any(|event| {
            event["event_type"] == "competition.challenge.state_changed"
                && event["payload"]["challenge_id"] == challenge_id
                && event["payload"]["to"] == "solved"
                && event["global_seq"].as_u64() == seq.checked_add(1)
        })
    });
    Ok(json!({"completed":completed,"correct_flag_count":confirmed.len() as u64,
        "total_flag_count":expected,"detail":submission["remote_receipt"]}))
}

fn project_platform_verdict(state: &Arc<AppState>, submission: &Value) -> ApiResult<bool> {
    let status = match field(submission,"state") {
        "correct" => "accepted",
        "duplicate_or_solved" => "duplicate",
        "wrong" => "rejected",
        _ => return Ok(false),
    };
    let candidate = state.store.get(CANDIDATES,field(submission,"candidate_id"))
        .map_err(ApiError::internal)?
        .ok_or_else(||ApiError::missing("candidate"))?;
    let run_id = field(&candidate,"source_run_id");
    if run_id.is_empty() || state.store.get("runs",run_id).map_err(ApiError::internal)?.is_none() {
        return Ok(false);
    }
    let submission_id = field(submission,"submission_id");
    let mut graph = super::super::graph::SharedGraph::open(state,run_id)
        .map_err(ApiError::internal)?;
    if graph.platform_verdicts().iter().any(|event|event["submission_id"] == submission_id) {
        return Ok(false);
    }
    let answer = read_secret(state,field(&candidate,"answer_ref"))
        .or_else(|_| {
            let value = field(&candidate,"value");
            if value.is_empty() { Err(ApiError::missing("candidate answer")) }
            else { Ok(value.to_owned()) }
        })?;
    let projection = if submission["platform_verdict_projection"].is_object() {
        submission["platform_verdict_projection"].clone()
    } else {
        historical_verdict_projection(state,submission)?
    };
    graph.record_platform_verdict(submission_id,&answer,status,
        projection["completed"] == true,
        projection["correct_flag_count"].as_u64(),
        projection["total_flag_count"].as_u64(),
        field(&projection,"detail"))
        .map_err(ApiError::internal)?;
    Ok(true)
}

fn reconcile_graph_verdicts(state: &Arc<AppState>, competition_id: &str) -> ApiResult<()> {
    for submission in scoped(state,SUBMISSIONS,competition_id)? {
        if let Err(error) = project_platform_verdict(state,&submission) {
            tracing::warn!(submission_id=%field(&submission,"submission_id"),code=%error.code,
                "Competition platform verdict Graph projection remains pending");
        }
    }
    Ok(())
}

fn cancel_stale_generation(
    state: &Arc<AppState>, outbox: &Value, submission: &Value,
    candidate: &Value, challenge: &Value,
) -> ApiResult<bool> {
    let competition_id = field(submission, "competition_id");
    let challenge_id = field(challenge, "challenge_id");
    let Some(binding) = active_binding(state, competition_id, challenge_id)? else { return Ok(false); };
    let source_generation = candidate["source_execution_generation"].as_u64().unwrap_or(0);
    let current_generation = binding["execution_generation"].as_u64().unwrap_or(0);
    if source_generation == 0 || source_generation >= current_generation { return Ok(false); }
    let submission_id = field(submission, "submission_id");
    let candidate_id = field(candidate, "candidate_id");
    let outbox_id = field(outbox, "outbox_id");
    let mut cancelled = submission.clone();
    cancelled["state"] = json!("cancelled");
    cancelled["cancel_reason"] = json!("stale_execution_generation");
    cancelled["last_error"] = json!("");
    cancelled["updated_at"] = json!(now());
    let mut cancelled_candidate = candidate.clone();
    if candidate["state"] == "approved" {
        cancelled_candidate["state"] = json!("cancelled");
        cancelled_candidate["updated_at"] = json!(now());
    }
    let other_inflight = scoped(state, SUBMISSIONS, competition_id)?.iter().any(|row| {
        row["competition_challenge_id"] == challenge_id && row["submission_id"] != submission_id
            && matches!(field(row,"state"), "queued" | "submitting" | "rate_limited" |
                "transient_failure" | "auth_required" | "unknown")
    });
    let mut next_challenge = challenge.clone();
    if !other_inflight && challenge["state"] == "submitting" {
        next_challenge["state"] = json!("running");
        next_challenge["paused_from"] = Value::Null;
        next_challenge["updated_at"] = json!(now());
    }
    let mut delivered = outbox.clone();
    delivered["status"] = json!("delivered");
    delivered["delivered_at"] = json!(now());
    delivered["updated_at"] = json!(now());
    let effect_id = field(outbox,"effect_id");
    let prior_effect = if effect_id.is_empty() { None } else {
        state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)?
    };
    let next_effect = prior_effect.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!("completed");
        next["output"] = json!({"submission_id":submission_id,"state":"cancelled",
            "reason":"stale_execution_generation"});
        next["completed_at"] = json!(now());
        next["updated_at"] = json!(now());
        next
    });
    let command_id = field(outbox,"command_id");
    let prior_command = if command_id.is_empty() { None } else {
        state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
    };
    let next_command = prior_command.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["response"]["receipt"]["state"] = json!("completed");
        next["response"]["receipt"]["completed_at"] = json!(now());
        next["response"]["receipt"]["output"] = json!({"submission_id":submission_id,
            "state":"cancelled","reason":"stale_execution_generation",
            "outbox_ids":[outbox_id],"effect_ids":[effect_id]});
        next
    });
    let mut expected = vec![(OUTBOX,outbox_id,Some(outbox)),
        (SUBMISSIONS,submission_id,Some(submission)),
        (CANDIDATES,candidate_id,Some(candidate)),
        (CHALLENGES,challenge_id,Some(challenge)),
        (BINDINGS,field(&binding,"binding_id"),Some(&binding))];
    let mut records = vec![(OUTBOX,outbox_id,&delivered),
        (SUBMISSIONS,submission_id,&cancelled),
        (CANDIDATES,candidate_id,&cancelled_candidate),
        (CHALLENGES,challenge_id,&next_challenge)];
    if let (Some(prior),Some(next)) = (&prior_effect,&next_effect) {
        expected.push((EFFECTS,effect_id,Some(prior)));
        records.push((EFFECTS,effect_id,next));
    }
    if let (Some(prior),Some(next)) = (&prior_command,&next_command) {
        expected.push((COMMANDS,command_id,Some(prior)));
        records.push((COMMANDS,command_id,next));
    }
    if !state.store.put_many_if(&expected,&records).map_err(ApiError::internal)? {
        return Err(ApiError::new(StatusCode::CONFLICT,"competition.submission.concurrent_update",
            "Stale submission changed concurrently"));
    }
    let details = json!({"submission_id":submission_id,"challenge_id":challenge_id,
        "reason":"stale_execution_generation","source_execution_generation":source_generation,
        "current_execution_generation":current_generation});
    append_event(state,competition_id,"platform_submission",submission_id,
        "competition.platform_submission.state_changed",command_id,
        json!({"from":"queued","to":"cancelled","candidate_id":candidate_id,
            "digest":candidate["digest"],"detail":details}))?;
    if cancelled_candidate != *candidate {
        append_event(state,competition_id,"submission_candidate",candidate_id,
            "competition.submission.candidate_state_changed",command_id,
            json!({"candidate_id":candidate_id,"challenge_id":challenge_id,"from":"approved",
                "to":"cancelled","reason":"stale_execution_generation",
                "source_execution_generation":source_generation,
                "current_execution_generation":current_generation}))?;
    }
    if next_challenge != *challenge {
        append_event(state,competition_id,"challenge",challenge_id,
            "competition.challenge.state_changed",command_id,
            json!({"challenge_id":challenge_id,"from":"submitting","to":"running",
                "reason":"stale_execution_generation"}))?;
    }
    let _ = project_flag_invalidation(state,candidate,
        &format!("queued candidate expired after dynamic instance replacement generation {source_generation}->{current_generation}"));
    Ok(true)
}

async fn reconcile_unknown(state: &Arc<AppState>, candidate_submission: &Value) -> ApiResult<()> {
    let submission_id = field(candidate_submission, "submission_id");
    let _lock = submission_lock(state, submission_id).await?;
    let Some(submission) = state
        .store
        .get(SUBMISSIONS, submission_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if submission["state"] != "unknown" {
        return Ok(());
    }
    let remote_id = field(&submission, "remote_receipt");

    let challenge_id = field(&submission, "competition_challenge_id");
    let challenge = state
        .store
        .get(CHALLENGES, challenge_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("challenge"))?;
    let competition_id = field(&submission, "competition_id");
    let competition = live_competition(state, competition_id)?;
    let connection = live_connection(state, field(&competition, "connection_id"))?;
    let cached=state.store.get(RECONCILED,submission_id).map_err(ApiError::internal)?;
    let(verdict,reconciled_remote)=if let Some(cached)=cached{(field(&cached,"verdict").to_owned(),field(&cached,"remote_receipt").to_owned())}else if connection["platform_kind"]=="rctf" && matches!(remote_id,"badNotStarted"|"badEnded"){
        ("transient_failure".to_owned(),remote_id.to_owned())
    }else if !matches!(field(&connection,"platform_kind"),"ctfd"|"rctf"|"gzctf"|"generic_browser"){
        extension_adapter::reconcile_submission(state,&connection,&challenge,&submission).await?
    }else if connection["platform_kind"]=="gzctf" && !remote_id.is_empty(){
        external_segment(remote_id)?;
        let(game,cid)=field(&challenge,"external_challenge_id").split_once('/').ok_or_else(||ApiError::invalid("Invalid GZCTF challenge identity"))?;external_segment(game)?;external_segment(cid)?;
        let result=platform_get(state,&connection,&format!("/api/Game/{game}/Challenges/{cid}/Status/{remote_id}")).await?;
        let status=result.as_str().or_else(||result["status"].as_str()).or_else(||result["result"].as_str()).unwrap_or("");
        (match status{"Accepted"=>"correct","WrongAnswer"|"CheatDetected"=>"wrong",_=>"unknown"}.to_owned(),remote_id.to_owned())
    }else{return Ok(())};
    if verdict=="unknown" || (verdict=="transient_failure"&&!(connection["platform_kind"]=="rctf"&&matches!(reconciled_remote.as_str(),"badNotStarted"|"badEnded"))){return Ok(());}
    let confirmed=json!({"submission_id":submission_id,"verdict":verdict,"remote_receipt":reconciled_remote,"recorded_at":now()});
    let _=state.store.put_many_if(&[(RECONCILED,submission_id,None)],&[(RECONCILED,submission_id,&confirmed)]).map_err(ApiError::internal)?;
    let verdict=verdict.as_str();
    let candidate_id = field(&submission, "candidate_id");
    let candidate = state
        .store
        .get(CANDIDATES, candidate_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("candidate"))?;
    let mut next_submission = submission.clone();
    next_submission["state"] = json!(verdict);
    next_submission["remote_receipt"]=json!(reconciled_remote);
    let certainty=if verdict=="transient_failure"&&matches!(reconciled_remote.as_str(),"badNotStarted"|"badEnded"){SubmissionCertainty::Rejected}else if matches!(verdict,"correct"|"wrong"|"partial"|"duplicate_or_solved"){SubmissionCertainty::Confirmed}else{SubmissionCertainty::Unknown};
    next_submission["certainty"]=json!(certainty);next_submission["retry_safe"]=json!(certainty.retry_safe());
    next_submission["last_error"] = json!("");
    next_submission["updated_at"] = json!(now());
    let mut next_candidate = candidate.clone();
    next_candidate["state"] = json!("submitted");
    next_candidate["updated_at"] = json!(now());
    let next_challenge = challenge_after_verdict(state, competition_id, &challenge, &submission, verdict)?;
    next_submission["platform_verdict_projection"] = verdict_projection_snapshot(
        state, competition_id, &challenge, &next_submission, verdict, &next_challenge,
        &reconciled_remote)?;
    let source_outbox = scoped(state, OUTBOX, competition_id)?.into_iter().find(|outbox| {
        outbox["submission_id"] == submission_id && outbox["status"] == "delivered"
            && !field(outbox,"effect_id").is_empty()
    });
    let effect_id = source_outbox.as_ref().map_or("",|outbox| field(outbox,"effect_id"));
    let command_id = source_outbox.as_ref().map_or("",|outbox| field(outbox,"command_id"));
    let prior_effect = if effect_id.is_empty() { None } else {
        state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)?
    };
    let next_effect = prior_effect.as_ref().and_then(|prior| {
        if prior["output"]["state"] != "unknown"
            || prior["output"]["remote_receipt"] != submission["remote_receipt"] { return None; }
        let mut next = prior.clone();
        next["output"]["state"] = json!(verdict);
        next["output"]["remote_receipt"]=json!(reconciled_remote);
        next["updated_at"] = json!(now());
        Some(next)
    });
    let prior_command = if command_id.is_empty() { None } else {
        state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?
    };
    let next_command = prior_command.as_ref().and_then(|prior| {
        if prior["response"]["receipt"]["output"]["submission_id"] != submission_id
            || prior["response"]["receipt"]["output"]["state"] != "unknown" { return None; }
        let mut next = prior.clone();
        next["response"]["receipt"]["output"]["state"] = json!(verdict);
        next["response"]["receipt"]["output"]["remote_receipt"] = json!(reconciled_remote);
        Some(next)
    });
    let mut events = vec![leases::planned_event(
        competition_id, "platform_submission", submission_id,
        "competition.platform_submission.state_changed", field(&submission,"command_id"),
        json!({"submission_id":submission_id,"candidate_id":candidate_id,"challenge_id":challenge_id,
            "from":"unknown","to":verdict,"reconciled":true,"digest":candidate["digest"]}),
    )];
    if next_challenge["state"] != challenge["state"] {
        events.push(leases::planned_event(
            competition_id, "challenge", challenge_id,
            "competition.challenge.state_changed", field(&submission,"command_id"),
            json!({"challenge_id":challenge_id,"from":challenge["state"],
                "to":next_challenge["state"],"reason":"platform_verdict_reconciled"}),
        ));
    }
    let mut expected = vec![
        (SUBMISSIONS, submission_id, Some(&submission)),
        (CANDIDATES, candidate_id, Some(&candidate)),
        (CHALLENGES, challenge_id, Some(&challenge)),
    ];
    let mut records = vec![
        (SUBMISSIONS, submission_id, &next_submission),
        (CANDIDATES, candidate_id, &next_candidate),
        (CHALLENGES, challenge_id, &next_challenge),
    ];
    if let (Some(prior),Some(next)) = (&prior_effect,&next_effect) {
        expected.push((EFFECTS,effect_id,Some(prior)));
        records.push((EFFECTS,effect_id,next));
    }
    if let (Some(prior),Some(next)) = (&prior_command,&next_command) {
        expected.push((COMMANDS,command_id,Some(prior)));
        records.push((COMMANDS,command_id,next));
    }
    if !put_records_with_events(state, competition_id, &expected, &records, &events)? {
        return Ok(());
    }
    drop(_lock);
    if let Err(error) = project_platform_verdict(state,&next_submission) {
        tracing::warn!(submission_id=%submission_id,code=%error.code,
            "Competition reconciled verdict Graph projection remains pending");
    }
    if verdict == "wrong" {
        let _ = project_flag_invalidation(state,&candidate,&reconciled_remote);
        if let Err(error) = wrong_binding::progress(state,&next_submission,&candidate).await {
            tracing::warn!(submission_id=%submission_id,code=%error.code,
                "Competition wrong-verdict Run continuation failed");
        }
    }
    if next_challenge["state"] == "solved" {
        settle_solved_binding(state,competition_id,challenge_id).await?;
    }
    Ok(())
}

async fn finalize_unknown(state: &Arc<AppState>, candidate_outbox: &Value) -> ApiResult<()> {
    let submission_id = field(candidate_outbox, "submission_id");
    let _lock = submission_lock(state, submission_id).await?;
    let outbox_id = field(candidate_outbox, "outbox_id");
    let Some(outbox) = state
        .store
        .get(OUTBOX, outbox_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if outbox["status"] != "failed" {
        return Ok(());
    }
    let Some(submission) = state
        .store
        .get(SUBMISSIONS, submission_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if submission["state"] == "submitting" || submission["state"] == "queued" {
        return Ok(());
    }
    let effect_id = field(&outbox, "effect_id");
    let effect = if effect_id.is_empty() { None } else {
        state.store.get(EFFECTS, effect_id).map_err(ApiError::internal)?
    };
    let command_id = field(&outbox, "command_id");
    let command = state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?;
    let mut delivered = outbox.clone();
    delivered["status"] = json!("delivered");
    delivered["delivered_at"] = json!(now());
    delivered["updated_at"] = json!(now());
    let completed_effect = effect.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!("completed");
        next["output"] = json!({"submission_id":submission_id,"state":submission["state"],
            "remote_receipt":submission["remote_receipt"]});
        next["updated_at"] = json!(now());
        next["completed_at"] = json!(now());
        next
    });
    let next_command = command.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["response"]["receipt"]["state"] = json!("completed");
        next["response"]["receipt"]["completed_at"] = json!(now());
        next["response"]["receipt"]["output"] = json!({"submission_id":submission_id,
            "state":submission["state"],"outbox_ids":[outbox_id],"effect_ids":[effect_id],
            "effect_state":"completed"});
        next
    });
    let mut expected = vec![(OUTBOX, outbox_id, Some(&outbox))];
    let mut records = vec![(OUTBOX, outbox_id, &delivered)];
    if let (Some(prior), Some(next)) = (&effect, &completed_effect) {
        expected.push((EFFECTS, effect_id, Some(prior)));
        records.push((EFFECTS, effect_id, next));
    }
    if let (Some(prior), Some(next)) = (&command, &next_command) {
        expected.push((COMMANDS, command_id, Some(prior)));
        records.push((COMMANDS, command_id, next));
    }
    let _ = state
        .store
        .put_many_if(&expected, &records)
        .map_err(ApiError::internal)?;
    Ok(())
}

async fn deliver(state: &Arc<AppState>, candidate_outbox: &Value) -> ApiResult<()> {
    let submission_id = field(candidate_outbox, "submission_id");
    let _lock = submission_lock(state, submission_id).await?;
    let outbox_id = field(candidate_outbox, "outbox_id");
    let Some(outbox) = state
        .store
        .get(OUTBOX, outbox_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if outbox["status"] != "pending" {
        return Ok(());
    }
    let Some(submission) = state
        .store
        .get(SUBMISSIONS, submission_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    if submission["state"] != "queued" {
        return Ok(());
    }
    let competition_id = field(&submission, "competition_id");
    let challenge_id = field(&submission, "competition_challenge_id");
    let challenge = state
        .store
        .get(CHALLENGES, challenge_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("challenge"))?;
    let candidate = state
        .store
        .get(CANDIDATES, field(&submission, "candidate_id"))
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("candidate"))?;
    if cancel_stale_generation(state, &outbox, &submission, &candidate, &challenge)? {
        return Ok(());
    }
    let competition = live_competition(state, competition_id)?;
    let connection = live_connection(state, field(&competition, "connection_id"))?;
    if connection["status"] != "active" {
        return Ok(());
    }
    let moment = chrono::Utc::now();
    let connection_cooldown = connection["capabilities"]["submission_cooldown_until"]
        .as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok());
    if connection_cooldown.is_some_and(|until| until > moment) {
        return Ok(());
    }
    let policy = state
        .store
        .get(POLICIES, competition_id)
        .map_err(ApiError::internal)?;
    let cooldown = policy
        .as_ref()
        .and_then(|row| row["submission_cooldown_seconds"].as_f64())
        .unwrap_or(0.0);
    if cooldown > 0.0 {
        let last = scoped(state, SUBMISSIONS, competition_id)?
            .into_iter()
            .filter(|row| row["state"] != "queued")
            .filter_map(|row| {
                row["updated_at"]
                    .as_str()
                    .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            })
            .max();
        if last.is_some_and(|at| {
            moment.signed_duration_since(at).num_milliseconds() < (cooldown * 1000.0) as i64
        }) {
            return Ok(());
        }
    }
    let budget_key = format!("{competition_id}:submissions");
    let prior_budget = state
        .store
        .get(BUDGETS, &budget_key)
        .map_err(ApiError::internal)?;
    let observed_used = scoped(state, SUBMISSIONS, competition_id)?
        .iter()
        .map(|row| {
            row["dispatch_count"]
                .as_u64()
                .unwrap_or(if row["state"] == "queued" { 0 } else { 1 })
        })
        .sum::<u64>() as f64;
    let limit = prior_budget.as_ref().and_then(|row| row["limit"].as_f64())
        .or_else(||policy.as_ref().and_then(|row| row["budget_limits"]["submissions"].as_f64()))
        .unwrap_or(0.0);
    let used = prior_budget
        .as_ref()
        .and_then(|row| row["used"].as_f64())
        .unwrap_or(0.0)
        .max(observed_used);
    if limit > 0.0 && used >= limit {
        return Ok(());
    }
    let next_budget = json!({"competition_id":competition_id,"kind":"submissions","limit":limit,
        "used":used+1.0,"measurement_incomplete":false,"window":"competition","resets_at":null,
        "updated_at":now()});
    let answer = read_secret(state, field(&candidate, "answer_ref"))?;
    let effect_id = field(&outbox, "effect_id");
    let effect = if effect_id.is_empty() { None } else {
        Some(state.store.get(EFFECTS, effect_id).map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::missing("effect receipt"))?)
    };
    let command_id = field(&outbox, "command_id");
    let command = if command_id.is_empty() { None } else {
        Some(state.store.get(COMMANDS, command_id).map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::missing("command receipt"))?)
    };
    let mut claimed = outbox.clone();
    claimed["status"] = json!("processing");
    claimed["attempts"] = json!(outbox["attempts"].as_u64().unwrap_or(0) + 1);
    claimed["updated_at"] = json!(now());
    let mut submitting = submission.clone();
    submitting["state"] = json!("submitting");
    submitting["dispatch_count"] = json!(submission["dispatch_count"].as_u64().unwrap_or(0) + 1);
    submitting["updated_at"] = json!(now());
    let running_effect = effect.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!("running");
        next["attempts"] = claimed["attempts"].clone();
        next["retry_after"] = Value::Null;
        next["error"] = Value::Null;
        next["updated_at"] = json!(now());
        next
    });
    let running_command = command.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["response"]["receipt"]["state"] = json!("running");
        next["response"]["receipt"]["output"]["effect_state"] = json!("running");
        next
    });
    let mut conditions = vec![
        (OUTBOX, outbox_id, Some(&outbox)),
        (SUBMISSIONS, submission_id, Some(&submission)),
        (BUDGETS, budget_key.as_str(), prior_budget.as_ref()),
    ];
    let mut records = vec![
        (OUTBOX, outbox_id, &claimed),
        (SUBMISSIONS, submission_id, &submitting),
        (BUDGETS, budget_key.as_str(), &next_budget),
    ];
    if let (Some(prior), Some(next)) = (&effect, &running_effect) {
        conditions.push((EFFECTS, effect_id, Some(prior)));
        records.push((EFFECTS, effect_id, next));
    }
    if let (Some(prior), Some(next)) = (&command, &running_command) {
        conditions.push((COMMANDS, command_id, Some(prior)));
        records.push((COMMANDS, command_id, next));
    }
    if !state
        .store
        .put_many_if(&conditions, &records)
        .map_err(ApiError::internal)?
    {
        return Ok(());
    }
    let result = platform_submit(state, &connection, &challenge, &answer,submission_id).await;
    let (verdict, remote_receipt, retry_after_seconds, last_error, certainty) = match result {
        Ok(outcome) => (outcome.state, outcome.remote_receipt, outcome.retry_after_seconds, String::new(),outcome.certainty),
        Err(error) => {
            let state = if error.status == StatusCode::TOO_MANY_REQUESTS || error.code == "competition.platform_rate_limited" {
                "rate_limited"
            } else if error.status == StatusCode::UNAUTHORIZED || error.code == "competition.auth_required" {
                "auth_required"
            } else if error.code=="competition.platform_transient" {
                "transient_failure"
            } else { "unknown" };
            let certainty=match state {"transient_failure"=>SubmissionCertainty::NotDispatched,"rate_limited"|"auth_required"=>SubmissionCertainty::Rejected,_=>SubmissionCertainty::Unknown};
            (state.to_owned(), String::new(), None, error.message,certainty)
        }
    };
    let verdict = match verdict.as_str() {
        "correct" | "partial" => "correct",
        "wrong" => "wrong",
        "duplicate_or_solved" => "duplicate_or_solved",
        "rate_limited" => "rate_limited",
        "auth_required" => "auth_required",
        "transient_failure" if certainty.retry_safe() => "transient_failure",
        "pending" => "unknown",
        _ => "unknown",
    };
    let result=json!({"outbox_id":outbox_id,"submission_id":submission_id,"verdict":verdict,"remote_receipt":remote_receipt,"retry_after_seconds":retry_after_seconds,"last_error":last_error,"certainty":certainty,"retry_safe":certainty.retry_safe(),"recorded_at":now()});
    let _=state.store.put_many_if(&[(RESULTS,outbox_id,None)],&[(RESULTS,outbox_id,&result)]).map_err(ApiError::internal)?;
    drop(_lock);
    settle_recorded(state,outbox_id).await
}

async fn settle_recorded(state:&Arc<AppState>,outbox_id:&str)->ApiResult<()>{
    for _ in 0..32{
        let result=state.store.get(RESULTS,outbox_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("submission delivery result"))?;
        let claimed=state.store.get(OUTBOX,outbox_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("submission outbox"))?;
        if claimed["status"]=="delivered"{return Ok(());}
        let submission_id=field(&claimed,"submission_id");
        let submitting=state.store.get(SUBMISSIONS,submission_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("submission"))?;
        let submission=submitting.clone();let competition_id=field(&submitting,"competition_id");let challenge_id=field(&submitting,"competition_challenge_id");
        let challenge=state.store.get(CHALLENGES,challenge_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("challenge"))?;
        let candidate=state.store.get(CANDIDATES,field(&submitting,"candidate_id")).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("candidate"))?;
        let competition=live_competition(state,competition_id)?;let connection=live_connection(state,field(&competition,"connection_id"))?;
        let effect_id=field(&claimed,"effect_id");let command_id=field(&claimed,"command_id");
        let running_effect=state.store.get(EFFECTS,effect_id).map_err(ApiError::internal)?;
        let running_command=state.store.get(COMMANDS,command_id).map_err(ApiError::internal)?;
        let verdict=field(&result,"verdict");let remote_receipt=field(&result,"remote_receipt").to_owned();let retry_after_seconds=result["retry_after_seconds"].as_f64();let last_error=field(&result,"last_error").to_owned();
    let mut settled = submitting.clone();
    settled["state"] = json!(verdict);
    settled["certainty"] = result["certainty"].clone();
    settled["retry_safe"] = json!(result["retry_safe"]==true);
    settled["remote_receipt"] = json!(remote_receipt);
    settled["last_error"] = json!(if last_error.is_empty() {
        match verdict { "rate_limited" => "platform rate limited", "auth_required" => "platform authentication required",
            "transient_failure" => "platform transient failure", _ => "" }
    } else { last_error.as_str() });
    settled["updated_at"] = json!(now());
    if verdict == "transient_failure" {
        settled["transient_failures"] = json!(transient_failure_count(state, &submission)? + 1);
    }
    let mut next_connection = connection.clone();
    if verdict == "rate_limited" {
        let fallback = connection["capabilities"]["detail"]["rate_limit"]["cooldown_seconds"]
            .as_f64().filter(|value| value.is_finite() && *value > 0.0);
        let delay = retry_after_seconds.or(fallback).unwrap_or(60.0).max(0.0);
        let retry_at = chrono::Utc::now() + chrono::Duration::milliseconds((delay * 1000.0) as i64);
        settled["retry_after_at"] = json!(retry_at.to_rfc3339());
        settled["retry_after_source"] = json!(if retry_after_seconds.is_some() { "response_header" } else { "connection_default" });
        let existing = connection["capabilities"]["submission_cooldown_until"].as_str()
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok());
        if existing.is_none_or(|until| until < retry_at) {
            next_connection["capabilities"]["submission_cooldown_until"] = json!(retry_at.to_rfc3339());
        }
    } else {
        settled["retry_after_at"] = Value::Null;
    }
    if verdict == "auth_required" {
        next_connection["status"] = json!("auth_required");
        next_connection["last_error"] = settled["last_error"].clone();
    }
    let mut delivered = claimed.clone();
    delivered["status"] = json!("delivered");
    delivered["delivered_at"] = json!(now());
    delivered["updated_at"] = json!(now());
    let completed_effect = running_effect.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["state"] = json!("completed");
        next["output"] = json!({"submission_id":submission_id,"state":verdict,"remote_receipt":remote_receipt});
        next["error"] = Value::Null;
        next["updated_at"] = json!(now());
        next["completed_at"] = json!(now());
        next
    });
    let completed_command = running_command.as_ref().map(|prior| {
        let mut next = prior.clone();
        next["response"]["receipt"]["state"] = json!("completed");
        next["response"]["receipt"]["completed_at"] = json!(now());
        next["response"]["receipt"]["output"] = json!({"submission_id":submission_id,
            "candidate_id":candidate["candidate_id"],"state":verdict,"outbox_ids":[outbox_id],
            "effect_ids":[effect_id],"effect_state":"completed","remote_receipt":remote_receipt});
        next
    });
    let mut candidate_next = candidate.clone();
    let challenge_next = challenge_after_verdict(state, competition_id, &challenge, &submitting, verdict)?;
    if matches!(verdict, "correct" | "duplicate_or_solved" | "wrong") {
        settled["platform_verdict_projection"] = verdict_projection_snapshot(
            state, competition_id, &challenge, &submitting, verdict, &challenge_next,
            &remote_receipt)?;
    }
    if matches!(verdict, "correct" | "duplicate_or_solved" | "wrong") {
        candidate_next["state"] = json!("submitted");
        candidate_next["updated_at"] = json!(now());
    }
    let mut expected = vec![
        (OUTBOX, outbox_id, Some(&claimed)),
        (SUBMISSIONS, submission_id, Some(&submitting)),
        (
            CANDIDATES,
            field(&candidate, "candidate_id"),
            Some(&candidate),
        ),
        (CHALLENGES, challenge_id, Some(&challenge)),
    ];
    let mut records = vec![
        (OUTBOX, outbox_id, &delivered),
        (SUBMISSIONS, submission_id, &settled),
        (
            CANDIDATES,
            field(&candidate, "candidate_id"),
            &candidate_next,
        ),
        (CHALLENGES, challenge_id, &challenge_next),
    ];
    if let (Some(prior), Some(next)) = (&running_effect, &completed_effect) {
        expected.push((EFFECTS, effect_id, Some(prior)));
        records.push((EFFECTS, effect_id, next));
    }
    if let (Some(prior), Some(next)) = (&running_command, &completed_command) {
        expected.push((COMMANDS, command_id, Some(prior)));
        records.push((COMMANDS, command_id, next));
    }
    if next_connection != connection {
        expected.push((CONNECTIONS, field(&connection, "connection_id"), Some(&connection)));
        records.push((CONNECTIONS, field(&connection, "connection_id"), &next_connection));
    }
    let mut events = vec![leases::planned_event(
        competition_id, "platform_submission", submission_id,
        "competition.platform_submission.state_changed", command_id,
        json!({"submission_id":submission_id,"candidate_id":candidate["candidate_id"],
            "challenge_id":challenge_id,"from":"submitting","to":verdict,
            "digest":candidate["digest"],"retry_after_at":settled["retry_after_at"],
            "retry_after_source":settled["retry_after_source"]}),
    )];
    if challenge_next["state"] != challenge["state"] {
        events.push(leases::planned_event(
            competition_id, "challenge", challenge_id,
            "competition.challenge.state_changed", command_id,
            json!({"challenge_id":challenge_id,"from":challenge["state"],
                "to":challenge_next["state"],"reason":"platform_verdict"}),
        ));
    }
    if verdict == "auth_required" && connection["status"] != "auth_required" {
        events.push(leases::planned_event(
            competition_id, "connection", field(&connection, "connection_id"),
            "competition.connection.status_changed", command_id,
            json!({"connection_id":connection["connection_id"],"from":connection["status"],
                "to":"auth_required","reason":"submission_auth_failed"}),
        ));
    }
    if !put_records_with_events(state, competition_id, &expected, &records, &events)? {
        continue;
    }
    if let Err(error) = project_platform_verdict(state,&settled) {
        tracing::warn!(submission_id=%submission_id,code=%error.code,
            "Competition terminal verdict Graph projection remains pending");
    }
    if verdict == "wrong" {
        let _ = project_flag_invalidation(state,&candidate,&remote_receipt);
        if let Err(error) = wrong_binding::progress(state,&settled,&candidate).await {
            tracing::warn!(submission_id=%submission_id,code=%error.code,
                "Competition wrong-verdict Run continuation failed");
        }
    }
    if challenge_next["state"] == "solved" {
        settle_solved_binding(state,competition_id,challenge_id).await?;
    }
    return Ok(());
    }
    Err(ApiError::new(StatusCode::CONFLICT,"competition.submission.projection_pending","Remote result is saved; local projection remains pending"))
}
