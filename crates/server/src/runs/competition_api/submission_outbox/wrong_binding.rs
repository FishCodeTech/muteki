//! Resume the same Competition Run after a remote wrong verdict.
use super::*;

fn transition(
    state: &Arc<AppState>, competition_id: &str, challenge_id: &str,
    binding: &Value, to: &str, submission_id: &str, generation: Option<u64>,
) -> ApiResult<Option<Value>> {
    let from = field(binding, "state");
    let binding_id = field(binding, "binding_id");
    let mut next = binding.clone();
    next["state"] = json!(to);
    next["updated_at"] = json!(now());
    if let Some(generation) = generation {
        next["execution_generation"] = json!(generation);
        next["wrong_resolution_submission_id"] = json!(submission_id);
    }
    if !state.store.put_many_if(
        &[(BINDINGS, binding_id, Some(binding))],
        &[(BINDINGS, binding_id, &next)],
    ).map_err(ApiError::internal)? {
        return Ok(None);
    }
    append_event(state, competition_id, "run_binding", binding_id,
        "competition.run_binding.state_changed", "",
        json!({"binding_id":binding_id,"challenge_id":challenge_id,
            "run_id":binding["run_id"],"from":from,"to":to,
            "reason":"submission_wrong","execution_generation":next["execution_generation"]}))?;
    Ok(Some(next))
}

fn compiled_challenge(state: &AppState, competition_id: &str, binding: &Value) -> ApiResult<Value> {
    let challenge_id = field(binding, "competition_challenge_id");
    let revision = state.store.get(REVISIONS, field(binding, "revision_id"))
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("challenge revision"))?;
    let mut description = field(&revision, "description").to_owned();
    if let Some(hints) = revision["hints"].as_array() {
        let rendered = hints.iter().filter_map(Value::as_str)
            .map(|hint| format!("- {hint}")).collect::<Vec<_>>().join("\n");
        if !rendered.is_empty() {
            description = format!("{description}\n\nHints:\n{rendered}").trim().to_owned();
        }
    }
    let target = scoped(state, LEASES, competition_id)?
        .into_iter()
        .filter(|lease| lease["competition_challenge_id"] == challenge_id
            && lease["state"] == "active")
        .max_by(|a, b| field(a,"updated_at").cmp(field(b,"updated_at")))
        .map(|lease| field(&lease,"address").to_owned())
        .filter(|address| !address.is_empty())
        .unwrap_or_else(|| field(&revision,"target").to_owned());
    let attachments = artifacts::stage_for_run(state, &revision, field(binding,"run_id"))?;
    Ok(json!({"mode":"ctf","name":revision["name"],"category":revision["category"],
        "points":revision["points"],"description":description,"target":target,
        "flag_format":revision["flag_format"],"expected_flags":revision["expected_flags"],
        "multi_flag":revision["multi_flag"],"attachments":attachments,
        "platform_confirmation_required":true}))
}

fn resume_challenge(state: &Arc<AppState>, competition_id: &str, challenge_id: &str) -> ApiResult<()> {
    let Some(challenge) = state.store.get(CHALLENGES, challenge_id).map_err(ApiError::internal)? else {
        return Ok(());
    };
    if challenge["state"] != "paused" { return Ok(()); }
    let mut next = challenge.clone();
    next["state"] = json!("running");
    next["paused_from"] = Value::Null;
    next["updated_at"] = json!(now());
    if state.store.put_many_if(&[(CHALLENGES,challenge_id,Some(&challenge))],
        &[(CHALLENGES,challenge_id,&next)]).map_err(ApiError::internal)? {
        append_event(state,competition_id,"challenge",challenge_id,
            "competition.challenge.state_changed","",
            json!({"challenge_id":challenge_id,"from":"paused","to":"running",
                "reason":"submission_wrong"}))?;
    }
    Ok(())
}

pub(super) async fn progress(
    state: &Arc<AppState>, submission: &Value, candidate: &Value,
) -> ApiResult<()> {
    if submission["state"] != "wrong" { return Ok(()); }
    let competition_id = field(submission, "competition_id");
    let challenge_id = field(submission, "competition_challenge_id");
    let submission_id = field(submission, "submission_id");
    let Some(initial) = active_binding(state, competition_id, challenge_id)? else {
        return Ok(());
    };
    let binding_id = field(&initial, "binding_id").to_owned();
    let _lock = submission_lock(state, &format!("binding-{binding_id}")).await?;
    let Some(mut binding) = state.store.get(BINDINGS, &binding_id).map_err(ApiError::internal)? else {
        return Ok(());
    };
    if active_binding(state, competition_id, challenge_id)?
        .is_none_or(|current| current["binding_id"] != binding_id) {
        return Ok(());
    }
    if binding["wrong_resolution_submission_id"] == submission_id {
        return Ok(());
    }
    let source_generation = candidate["source_execution_generation"].as_u64().unwrap_or(0);
    let binding_generation = binding["execution_generation"].as_u64().unwrap_or(0);
    if source_generation > 0 && source_generation < binding_generation {
        return Ok(());
    }
    let run_id = field(&binding,"run_id").to_owned();
    if run_id.is_empty() { return Ok(()); }
    if !state.runs.claim_execution_owner(state, &run_id).map_err(ApiError::internal)? {
        return Ok(());
    }
    let Some(run) = state.store.get("runs",&run_id).map_err(ApiError::internal)? else {
        return Ok(());
    };
    let run_generation = run["execution_generation"].as_u64().unwrap_or(0);
    if run_generation > binding_generation {
        if matches!(field(&binding,"state"), "paused" | "rejected" | "resolving") {
            if transition(state,competition_id,challenge_id,&binding,"running",submission_id,
                Some(run_generation))?.is_some() {
                resume_challenge(state,competition_id,challenge_id)?;
                super::super::spawn_run_watcher(state.clone(),competition_id.to_owned(),
                    challenge_id.to_owned(),run_id,binding_id,run_generation);
            }
        }
        return Ok(());
    }
    if run_generation < binding_generation { return Ok(()); }

    match field(&binding,"state") {
        "finished" | "local_finished" => {
            let Some(next) = transition(state,competition_id,challenge_id,&binding,
                "remote_pending",submission_id,None)? else { return Ok(()); };
            binding = next;
        }
        _ => {}
    }
    if binding["state"] == "remote_pending" {
        let Some(next) = transition(state,competition_id,challenge_id,&binding,
            "rejected",submission_id,None)? else { return Ok(()); };
        binding = next;
    }
    if binding["state"] == "running" {
        if run["running"] == true || state.runs.active_worker_count(&run_id) > 0 {
            let control_generation = run["control_generation"].as_u64().unwrap_or(0);
            let command_id = format!("competition-wrong:{binding_id}:{binding_generation}:{control_generation}:park");
            let response = super::super::super::control_run(
                State(state.clone()),Path(run_id.clone()),
                Json(json!({"action":"stop","text":"competition_binding_pause",
                    "command_id":command_id,"expected_generation":control_generation})),
            ).await;
            if !response.status().is_success() {
                return Err(ApiError::new(StatusCode::CONFLICT,"competition.binding.pause_failed",
                    format!("Run stop returned HTTP {}",response.status().as_u16())));
            }
        }
        let Some(next) = transition(state,competition_id,challenge_id,&binding,
            "paused",submission_id,None)? else { return Ok(()); };
        binding = next;
    }
    if !matches!(field(&binding,"state"), "paused" | "rejected") { return Ok(()); }
    if state.runs.active_worker_count(&run_id) > 0 { return Ok(()); }
    let challenge = compiled_challenge(state,competition_id,&binding)?;
    let command_id=format!("wrong-binding-resolve:{submission_id}:{binding_generation}");
    let current=state.store.get("runs",&run_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::new(StatusCode::NOT_FOUND,"run.not_found","Run missing"))?;
    let request=state.store.get("run_execution_commands",&command_id).map_err(ApiError::internal)?.and_then(|r|r.get("request").filter(|v|v.is_object()).cloned()).unwrap_or_else(||json!({"challenge":challenge,"command_id":command_id,"expected_execution_generation":current["execution_generation"],"expected_generation":current["control_generation"]}));
    let response = super::super::super::resolve_run(State(state.clone()),Path(run_id.clone()),Some(Json(request))).await;
    if !response.status().is_success() {
        return Err(ApiError::new(StatusCode::CONFLICT,"competition.binding.resolve_failed",
            format!("Run resolve returned HTTP {}",response.status().as_u16())));
    }
    let Some(run) = state.store.get("runs",&run_id).map_err(ApiError::internal)? else {
        return Ok(());
    };
    let generation = run["execution_generation"].as_u64().unwrap_or(0);
    if generation <= binding_generation { return Ok(()); }
    let Some(resolving) = transition(state,competition_id,challenge_id,&binding,
        "resolving",submission_id,None)? else { return Ok(()); };
    if transition(state,competition_id,challenge_id,&resolving,"running",submission_id,
        Some(generation))?.is_some() {
        resume_challenge(state,competition_id,challenge_id)?;
        super::super::spawn_run_watcher(state.clone(),competition_id.to_owned(),
            challenge_id.to_owned(),run_id,binding_id,generation);
    }
    Ok(())
}
