//! Durable false-positive recovery commands.
use super::*;
pub(super) fn enqueue_recovery(
    state: &AppState,
    run_id: &str,
    command: &str,
) -> Result<(), String> {
    let key = format!("false-positive-recovery:{command}");
    if state
        .store
        .get("run_control_recoveries", &key)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        let record = json!({"command_id":key,"source_command_id":command,"run_id":run_id,"source_execution_generation":state.store.get("runs",run_id).map_err(|e|e.to_string())?.and_then(|r|r["execution_generation"].as_u64()).unwrap_or(1),"state":"pending","created_at":now_secs()});
        state
            .store
            .put_many_if(
                &[("run_control_recoveries", &key, None)],
                &[("run_control_recoveries", &key, &record)],
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
pub fn spawn_recoveries(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if state.admission.draining() {
                continue;
            }
            let records = match state.store.list("run_control_recoveries") {
                Ok(records) => records,
                Err(error) => {
                    tracing::error!(%error,"Control recovery queue read failed");
                    continue;
                }
            };
            for record in records.into_iter().filter(|record| {
                record["state"] == "pending"
                    || record["state"] == "processing"
                        && record["lease_until"].as_f64().unwrap_or(0.) <= now_secs()
            }) {
                let Some(id) = record["run_id"].as_str().map(str::to_owned) else {
                    continue;
                };
                let Some(command) = record["command_id"].as_str().map(str::to_owned) else {
                    continue;
                };
                match state.store.get("runs", &id) {
                    Ok(Some(run)) if crate::business::run_available(&state, &run) => {
                        if record["source_execution_generation"]
                            .as_u64()
                            .is_some_and(|g| run["execution_generation"].as_u64() != Some(g))
                        {
                            let mut superseded = record.clone();
                            superseded["state"] = json!("superseded");
                            if let Err(error) = state.store.put_many_if(
                                &[("run_control_recoveries", &command, Some(&record))],
                                &[("run_control_recoveries", &command, &superseded)],
                            ) {
                                tracing::error!(%error,"Recovery supersession failed");
                            }
                            continue;
                        }
                    }
                    Ok(_) => continue,
                    Err(error) => {
                        tracing::error!(%error,run_id=id,"Recovery owner unavailable");
                        continue;
                    }
                }
                if record["retry_after"].as_f64().unwrap_or(0.) > now_secs() {
                    continue;
                }
                let mut claimed = record.clone();
                claimed["state"] = json!("processing");
                claimed["lease_until"] = json!(now_secs() + 300.);
                claimed["claim_id"] = json!(uuid::Uuid::new_v4().to_string());
                match state.store.put_many_if(
                    &[("run_control_recoveries", &command, Some(&record))],
                    &[("run_control_recoveries", &command, &claimed)],
                ) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        tracing::error!(%error,"Control recovery claim failed");
                        continue;
                    }
                }
                let response=resolve_run(State(state.clone()),Path(id.clone()),Some(Json(json!({"command_id":command,"cold_start":false,"expected_execution_generation":record["source_execution_generation"]})))).await;
                let status = response.status();
                let result = match axum::body::to_bytes(response.into_body(), 256 * 1024).await {
                    Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
                    Err(_) => Value::Null,
                };
                let completed = status.is_success() && result["ok"] == true;
                let retry = !completed
                    && (matches!(status.as_u16(), 409 | 429 | 500 | 502 | 503 | 504)
                        || status.is_success());
                let mut settled = claimed.clone();
                settled["state"] = json!(if completed {
                    "completed"
                } else if retry {
                    "pending"
                } else {
                    "failed"
                });
                settled["status"] = json!(status.as_u16());
                settled["result"] = result;
                settled["lease_until"] = Value::Null;
                if retry {
                    let attempts = record["attempts"].as_u64().unwrap_or(0) + 1;
                    settled["attempts"] = json!(attempts);
                    settled["retry_after"] = json!(now_secs() + (attempts.min(12) * 5) as f64);
                } else {
                    settled["completed_at"] = json!(now_secs());
                }
                if let Err(error) = state.store.put_many_if(
                    &[("run_control_recoveries", &command, Some(&claimed))],
                    &[("run_control_recoveries", &command, &settled)],
                ) {
                    tracing::error!(%error,run_id=id,"Control recovery settlement failed");
                }
                if !completed && !retry {
                    emit_event(
                        &state,
                        &id,
                        None,
                        "worker.error",
                        json!({"reason":"false_positive_recovery_failed","status":status.as_u16(),"command_id":command,"result":settled["result"]}),
                    );
                }
            }
        }
    });
}
