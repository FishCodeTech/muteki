//! Confirmed local/external results survive a crash before receipt settlement.
use super::*;

const RESULTS: &str = "platform_command_results";

pub(super) async fn record(
    state: &AppState,
    envelope: &Value,
    effect_id: Option<&str>,
    principal: &str,
    version_before: u64,
    response: Response,
) -> Result<Response, Failure> {
    let command_id = envelope["command_id"].as_str().unwrap_or("");
    record_attempt(state,command_id,envelope,effect_id,principal,version_before,response).await
}

pub(super) async fn record_attempt(
    state: &AppState,
    journal_id: &str,
    envelope: &Value,
    effect_id: Option<&str>,
    principal: &str,
    version_before: u64,
    response: Response,
) -> Result<Response,Failure> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.map_err(storage)?;
    let body: Value = serde_json::from_slice(&bytes).map_err(storage)?;
    let command_id = envelope["command_id"].as_str().unwrap_or("");
    let safe_envelope = json!({"command_id":command_id,"command_type":envelope["command_type"],
        "aggregate_type":envelope["aggregate_type"],"aggregate_id":envelope["aggregate_id"],"payload":{}});
    let result = json!({"journal_id":journal_id,"command_id":command_id,"envelope":safe_envelope,
        "request_fingerprint":command_fingerprint(envelope),"effect_id":effect_id,"principal":principal,
        "http_status":status.as_u16(),"body":body,"version_before":version_before,"state":"confirmed","confirmed_at":now()});
    if !state.store.put_many_if(&[(RESULTS,journal_id,None)],&[(RESULTS,journal_id,&result)]).map_err(storage)? {
        let prior = state.store.get(RESULTS,journal_id).map_err(storage)?.ok_or_else(|| storage("Command result disappeared"))?;
        if prior["request_fingerprint"] != result["request_fingerprint"] || prior["body"] != result["body"] {
            return Err(fail(StatusCode::CONFLICT,"command.result_conflict","A different confirmed result already exists"));
        }
    }
    Ok((status,Json(body)).into_response())
}

pub(super) fn settled(state: &AppState, command_id: &str) -> Result<(), Failure> {
    if let Some(prior) = state.store.get(RESULTS,command_id).map_err(storage)? {
        let mut next = prior.clone();
        next["state"] = json!("settled");
        next["settled_at"] = json!(now());
        state.store.put_many_if(&[(RESULTS,command_id,Some(&prior))],&[(RESULTS,command_id,&next)]).map_err(storage)?;
    }
    Ok(())
}

pub(super) async fn recover(state: &AppState, command_id: &str) -> Result<(), Failure> {
    if state.store.get(RESULTS,command_id).map_err(storage)?.is_none() {
        for job in state.store.list("task_extension_uninstalls").map_err(storage)? {
            if job["state"] == "completed" && job["original_envelope"]["command_id"] == command_id {
                let envelope = &job["original_envelope"];
                let output = json!({"result":{"extension_id":job["extension_id"],"uninstalled":true,"retention":job["retention"]}});
                let response = normalize_extension_response(state,envelope,Json(output).into_response()).await?;
                let before = state.store.get(COMMANDS,command_id).map_err(storage)?
                    .and_then(|row| row["version_before"].as_u64()).unwrap_or(0);
                record(state,envelope,None,envelope["_verified_scope"].as_str().unwrap_or("operator:local-user"),before,response).await?;
                break;
            }
        }
    }
    let Some(result) = state.store.get(RESULTS,command_id).map_err(storage)? else { return Ok(()) };
    if result["state"] != "confirmed" { return Ok(()); }
    let journal_id = command_id;
    let command_id = result["command_id"].as_str().unwrap_or(command_id);
    let envelope = &result["envelope"];
    let digest = result["request_fingerprint"].as_str().unwrap_or("");
    let status = StatusCode::from_u16(result["http_status"].as_u64().unwrap_or(500) as u16).map_err(storage)?;
    let effect_id = result["effect_id"].as_str();
    let mut response = (status,Json(result["body"].clone())).into_response();
    if let Some(effect_id) = effect_id {
        response = platform_effects::finish_response(state,effect_id,response).await?;
    }
    let principal = result["principal"].as_str().unwrap_or("operator:local-user");
    settle_command_response(state,envelope,digest,digest,None,effect_id,principal,false,response).await?;
    let receipt = state.store.get(COMMANDS,command_id).map_err(storage)?.ok_or_else(|| storage("Recovered receipt disappeared"))?;
    if receipt["response"]["receipt"]["state"] == "completed" {
        let aggregate = &receipt["response"]["receipt"]["aggregate"];
        let kind = aggregate["type"].as_str().unwrap_or("");
        let id = aggregate["id"].as_str().unwrap_or("");
        let before = if aggregate["type"] == envelope["aggregate_type"] && aggregate["id"] == envelope["aggregate_id"] {
            result["version_before"].as_u64().unwrap_or(0)
        } else { 0 };
        save_aggregate_version(state,kind,id,before)?;
    }
    let claim = json!({"command_id":command_id,"request_fingerprint":digest,
        "state":receipt["response"]["receipt"]["state"],"updated_at":now()});
    state.store.put(COMMAND_CLAIMS,command_id,&claim).map_err(storage)?;
    settled(state,journal_id)
}

pub(super) async fn recover_all(state: &AppState,module:Option<crate::business::Module>) -> Result<(), Failure> {
    for job in state.store.list("task_extension_uninstalls").map_err(storage)? {
        if module.is_none() && job["state"] == "completed" {
            if let Some(command_id) = job["original_envelope"]["command_id"].as_str() {
                recover(state,command_id).await?;
            }
        }
    }
    for result in state.store.list(RESULTS).map_err(storage)? {
        let owner=crate::business::operation_module(state,result["envelope"]["command_type"].as_str().unwrap_or(""),&result["envelope"]).map_err(|e|e.failure())?;
        if owner==module && result["state"] == "confirmed" {
            recover(state,result["journal_id"].as_str().or_else(||result["command_id"].as_str()).unwrap_or("")).await?;
        }
    }
    Ok(())
}
