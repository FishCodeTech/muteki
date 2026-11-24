//! Recover the Competition Gate projection from durable Run and SharedGraph facts.
use super::*;
use std::collections::{HashMap, HashSet};

fn repair_run_event_tail(state: &AppState, run_id: &str) -> ApiResult<()> {
    let path = super::super::session_dir(state,run_id).join("events.jsonl");
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ApiError::internal(error)),
    };
    if raw.is_empty() || raw.last() == Some(&b'\n') {return Ok(());}
    let start = raw.iter().rposition(|byte|*byte == b'\n').map(|index|index+1).unwrap_or(0);
    if serde_json::from_slice::<Value>(&raw[start..]).is_ok() {
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&path)
            .and_then(|mut file|file.write_all(b"\n")).map_err(ApiError::internal)?;
    } else {
        std::fs::OpenOptions::new().write(true).open(&path)
            .and_then(|file|file.set_len(start as u64)).map_err(ApiError::internal)?;
    }
    Ok(())
}

pub(super) fn reconcile(state: &Arc<AppState>) -> ApiResult<()> {
    let mut latest = HashMap::<String, Value>::new();
    for binding in state.store.list(BINDINGS).map_err(ApiError::internal)? {
        let run_id = field(&binding,"run_id");
        if run_id.is_empty() { continue; }
        let replace = latest.get(run_id).is_none_or(|prior| {
            (binding["execution_generation"].as_u64().unwrap_or(0),field(&binding,"updated_at"),field(&binding,"binding_id"))
                > (prior["execution_generation"].as_u64().unwrap_or(0),field(prior,"updated_at"),field(prior,"binding_id"))
        });
        if replace { latest.insert(run_id.to_owned(),binding); }
    }
    for (run_id,mut binding) in latest {
        let competition_id = field(&binding,"competition_id").to_owned();
        let challenge_id = field(&binding,"competition_challenge_id").to_owned();
        let competition_id = competition_id.as_str();
        let challenge_id = challenge_id.as_str();
        let Some(competition) = state.store.get(COMPETITIONS,competition_id)
            .map_err(ApiError::internal)? else { continue; };
        if competition["historical"] == true || competition["archived"] == true { continue; }
        let Some(challenge) = state.store.get(CHALLENGES,challenge_id)
            .map_err(ApiError::internal)? else { continue; };
        if challenge["tombstoned"] == true || challenge["state"] == "solved" { continue; }
        let Some(run) = state.store.get("runs",&run_id).map_err(ApiError::internal)? else { continue; };
        if challenge["state"] == "paused" && run["outcome_reason"] == "process_restart"
            && scoped(state,CANDIDATES,competition_id)?.iter().any(|candidate|
                candidate["competition_challenge_id"] == challenge_id
                    && matches!(field(candidate,"state"),"awaiting_approval" | "approved")) {
            let mut restored = challenge.clone();
            restored["state"] = json!("candidate_found");
            restored["paused_from"] = Value::Null;
            restored["updated_at"] = json!(now());
            let event = leases::planned_event(competition_id,"challenge",challenge_id,
                "competition.challenge.state_changed","",
                json!({"challenge_id":challenge_id,"from":"paused",
                    "to":"candidate_found","reason":"pending_candidate_recovered"}));
            let _ = put_records_with_events(state,competition_id,
                &[(CHALLENGES,challenge_id,Some(&challenge))],
                &[(CHALLENGES,challenge_id,&restored)],&[event])?;
        }
        let graph_path = super::super::graph::graph_db_path(state,&run_id);
        if !graph_path.is_file() && !super::super::session_dir(state,&run_id).join("events.jsonl").is_file() {
            continue;
        }
        let run_generation = run["execution_generation"].as_u64().unwrap_or(0);
        if run_generation > binding["execution_generation"].as_u64().unwrap_or(0) {
            let mut aligned = binding.clone();
            aligned["execution_generation"] = json!(run_generation);
            if state.store.put_many_if(
                &[(BINDINGS,field(&binding,"binding_id"),Some(&binding))],
                &[(BINDINGS,field(&binding,"binding_id"),&aligned)],
            ).map_err(ApiError::internal)? { binding = aligned; }
            else { continue; }
        }
        let graph_events = if graph_path.is_file() {
            super::super::graph::SharedGraph::open(state,&run_id)
                .map(|graph|graph.events()).unwrap_or_default()
        } else { Vec::new() };
        let invalidated = graph_events.iter()
            .filter(|event| event["kind"] == "flag_invalidated")
            .filter_map(|event| event["payload"]["flag"].as_str().map(str::to_owned))
            .collect::<HashSet<_>>();
        let binding_generation = binding["execution_generation"].as_u64().unwrap_or(0);
        let mut gate_events = super::super::read_jsonl_event_iter(state,&run_id)
            .filter(|event| run_flag_event(event).is_some())
            .collect::<Vec<_>>();
        let mut existing_flags = gate_events.iter()
            .filter_map(|event| run_flag_event(event).map(str::to_owned))
            .collect::<HashSet<_>>();
        for graph_event in graph_events.iter().filter(|event| event["kind"] == "flag_found"
            && event["verified"] == true) {
            let Some(flag) = graph_event["payload"]["flag"].as_str() else { continue; };
            if invalidated.contains(flag) || existing_flags.contains(flag) {
                continue;
            }
            let actor = field(graph_event,"actor");
            if actor.is_empty() { continue; }
            let generation = super::super::worker::worker_execution_generation(actor);
            if binding_generation > 0 && generation < binding_generation { continue; }
            repair_run_event_tail(state,&run_id)?;
            let event = super::super::emit_event(state,&run_id,Some(actor),"blackboard.delta",
                json!({"kind":"flag_found","flag":flag,"actor":actor,"source":actor,
                    "execution_generation":generation,"recovered":true,
                    "shared_graph_seq":graph_event["seq"]}));
            if event["fenced"] != "stale_execution_generation"
                && event["persisted"] != false
                && event["seq"].as_u64().is_some_and(|target_seq| {
                    super::super::read_jsonl_event_scan(state, &run_id, target_seq.saturating_sub(1))
                        .any(|(_, saved)| {
                            saved.is_some_and(|saved| {
                                saved["seq"] == event["seq"]
                                    && saved["event_type"] == "blackboard.delta"
                                    && saved["payload"]["flag"] == flag
                            })
                        })
                }) {
                gate_events.push(event);
                existing_flags.insert(flag.to_owned());
            }
        }
        for event in gate_events {
            let Some(flag) = run_flag_event(&event) else { continue; };
            if invalidated.contains(flag) { continue; }
            let actor = run_flag_actor(&event);
            let generation = run_flag_generation(&event,binding_generation);
            if binding_generation > 0 && generation < binding_generation { continue; }
            if submission_commands::candidate_exists(state,competition_id,challenge_id,1,flag)? {
                continue;
            }
            let seq = event["seq"].as_u64().unwrap_or(0);
            let command_id = format!("competition-run-flag:{}:{generation}:{seq}",
                field(&binding,"binding_id"));
            let payload = json!({"challenge_id":challenge_id,"answer":flag,
                "answer_slot":1,"source_run_id":run_id,"source_kind":"run_event",
                "witness":format!("run_event:{run_id}:{seq}"),
                "execution_generation":generation,"worker_id":actor});
            if let Err(error) = submission_commands::execute(state,competition_id,
                "competition.submission.submit",&command_id,&payload,None,
                "system:competition",&Value::Null,"system") {
                tracing::warn!(run_id,challenge_id,code=%error.code,
                    "Competition Gate recovery could not register candidate");
            }
        }
    }
    Ok(())
}
