use super::*;
use axum::{
    body::to_bytes,
    extract::{Path, State},
};
use std::sync::Arc;

const STAGES: &str = "src_run_destroy_stages";
const RUNS: &str = "runs";
const PROTECTED: &[&str] = &[
    "frozen_report",
    "asset_publication",
    "asset_reference",
    "monitor_baseline",
    "knowledge_publication",
];

fn disposal_key(task_id: &str, run_id: &str) -> String {
    format!("run_disposal:{task_id}:{run_id}")
}

fn disposal_record(task_id: &str, run_id: &str, payload: Value, previous: Option<&Value>) -> Value {
    let stamp = now();
    let version = previous
        .and_then(|row| row["version"].as_u64())
        .unwrap_or(0)
        + 1;
    let created = previous
        .and_then(|row| row["values"]["created_at"].as_str())
        .unwrap_or(&stamp);
    let output = json!({"record_id":run_id,"kind":"run_disposal","task_id":task_id,
        "revision":version,"parent_id":task_id,"natural_key":"","generation":0,
        "created_at":created,"updated_at":stamp,"payload":payload});
    json!({"version":version,"values":output})
}

fn stage_approved(
    s: &AppState,
    task_id: &str,
    run_id: &str,
    command_id: &str,
    actor_id: &str,
) -> Api<bool> {
    let key = disposal_key(task_id, run_id);
    loop {
        if s.store.get(STAGES, command_id).map_err(db)?.is_some() {
            return Ok(true);
        }
        let previous = s.store.get(RECORDS, &key).map_err(db)?;
        let document = disposal_record(
            task_id,
            run_id,
            json!({"state":"approved","actor_id":actor_id}),
            previous.as_ref(),
        );
        let stage = json!({"command_id":command_id,"task_id":task_id,"run_id":run_id});
        if s.store
            .put_many_if(
                &[
                    (RECORDS, &key, previous.as_ref()),
                    (STAGES, command_id, None),
                ],
                &[(RECORDS, &key, &document), (STAGES, command_id, &stage)],
            )
            .map_err(db)?
        {
            return Ok(false);
        }
    }
}

fn mark_failed(s: &AppState, task_id: &str, run_id: &str, actor_id: &str, error: &str) -> Api<()> {
    let key = disposal_key(task_id, run_id);
    loop {
        let previous = s.store.get(RECORDS, &key).map_err(db)?;
        let document = disposal_record(
            task_id,
            run_id,
            json!({"state":"failed","actor_id":actor_id,"error":error}),
            previous.as_ref(),
        );
        if s.store
            .put_many_if(
                &[(RECORDS, &key, previous.as_ref())],
                &[(RECORDS, &key, &document)],
            )
            .map_err(db)?
        {
            return Ok(());
        }
    }
}

fn completed(s: &AppState, command_id: &str) -> Api<Option<Value>> {
    Ok(s.store
        .get(RUN_DESTROY_APPLIED, command_id)
        .map_err(db)?
        .map(|marker| marker["output"].clone()))
}

fn protected_references(s: &AppState, task_id: &str, run_id: &str) -> Api<Vec<Value>> {
    let mut references = Vec::new();
    for kind in PROTECTED {
        for row in records(s, task_id, kind)? {
            if row["payload"].to_string().contains(run_id) {
                references.push(json!({"kind":kind,"record_id":row["record_id"]}));
            }
        }
    }
    Ok(references)
}

fn finalize(
    s: &AppState,
    task_id: &str,
    run_id: &str,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let disposal_key = disposal_key(task_id, run_id);
    loop {
        if let Some(output) = completed(s, command_id)? {
            return Ok(output);
        }
        let binding = records(s, task_id, "run_binding")?
            .into_iter()
            .find(|row| row["payload"]["run_id"] == run_id)
            .ok_or_else(|| {
                Error::new(
                    StatusCode::NOT_FOUND,
                    "artifact_missing",
                    format!("run_binding/{run_id} is unavailable"),
                )
            })?;
        let binding_key = rec_key(
            task_id,
            "run_binding",
            binding["natural_key"].as_str().unwrap_or(""),
        );
        let previous_binding =
            s.store
                .get(RECORDS, &binding_key)
                .map_err(db)?
                .ok_or_else(|| {
                    Error::new(
                        StatusCode::NOT_FOUND,
                        "artifact_missing",
                        "Run binding document is unavailable",
                    )
                })?;
        let previous_disposal = s.store.get(RECORDS, &disposal_key).map_err(db)?;
        let stamp = now();
        let mut next_binding = previous_binding.clone();
        let revision = previous_binding["version"].as_u64().unwrap_or(0) + 1;
        next_binding["version"] = json!(revision);
        next_binding["values"]["revision"] = json!(revision);
        next_binding["values"]["updated_at"] = json!(stamp);
        next_binding["values"]["payload"]["destroyed_at"] = json!(stamp);
        let document = disposal_record(
            task_id,
            run_id,
            json!({"state":"destroyed","actor_id":actor_id,"destroyed_at":stamp}),
            previous_disposal.as_ref(),
        );
        let output = document["values"].clone();
        let marker = json!({"command_id":command_id,"output":output});
        if s.store
            .put_many_if(
                &[
                    (RECORDS, &binding_key, Some(&previous_binding)),
                    (RECORDS, &disposal_key, previous_disposal.as_ref()),
                    (RUN_DESTROY_APPLIED, command_id, None),
                ],
                &[
                    (RECORDS, &binding_key, &next_binding),
                    (RECORDS, &disposal_key, &document),
                    (RUN_DESTROY_APPLIED, command_id, &marker),
                ],
            )
            .map_err(db)?
        {
            return Ok(output);
        }
    }
}

pub(super) async fn apply(
    s: Arc<AppState>,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let _lock = super::task_lifecycle::lock_task(&s, task_id).await?;
    if let Some(output) = completed(&s, command_id)? {
        return Ok(output);
    }
    if task(&s, task_id)?["binding"].is_null() {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "SRC Task binding is unavailable",
        ));
    }
    let run_id = require(body, "run_id")?;
    let binding = records(&s, task_id, "run_binding")?
        .into_iter()
        .find(|row| row["payload"]["run_id"] == run_id)
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                format!("run_binding/{run_id} is unavailable"),
            )
        })?;
    if binding["payload"]["task_id"] != task_id {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "Run binding belongs to another SRC Task",
        ));
    }
    let lifecycle = records(&s, task_id, "run_lifecycle")?
        .into_iter()
        .find(|row| row["record_id"] == run_id)
        .ok_or_else(|| {
            Error::new(
                StatusCode::CONFLICT,
                "run_not_archived",
                "Run destruction requires an archived Run",
            )
        })?;
    if lifecycle["payload"]["state"] != "archived" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "run_not_archived",
            "Run destruction requires an archived Run",
        ));
    }
    let references = protected_references(&s, task_id, run_id)?;
    if !references.is_empty() {
        let mut error = Error::new(
            StatusCode::CONFLICT,
            "artifact_in_use",
            "Run is protected by frozen or shared references",
        );
        error.detail = json!({"references":references});
        return Err(error);
    }
    let existed = s.store.get(RUNS, run_id).map_err(db)?.is_some();
    let resumed = stage_approved(&s, task_id, run_id, command_id, actor_id)?;
    if !existed && !resumed {
        mark_failed(
            &s,
            task_id,
            run_id,
            actor_id,
            "run.not_found: Run is unavailable",
        )?;
        return Err(Error::new(
            StatusCode::CONFLICT,
            "run_destroy_failed",
            "Run could not be destroyed",
        ));
    }
    let paths = [
        crate::runs::session_dir(&s, run_id),
        crate::runs::control_dir(&s, run_id),
        crate::runs::graph::graph_dir(&s, run_id),
    ];
    if existed {
        let response = crate::runs::delete_run(State(s.clone()), Path(run_id.to_owned())).await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .map_err(db)?;
        let result: Value = serde_json::from_slice(&bytes).map_err(db)?;
        if !status.is_success()
            || result["ok"] != true
            || s.store.get(RUNS, run_id).map_err(db)?.is_some()
            || paths.iter().any(|path| path.exists())
        {
            mark_failed(
                &s,
                task_id,
                run_id,
                actor_id,
                "run_destroy_failed: Run delete did not settle",
            )?;
            return Err(Error::new(
                StatusCode::CONFLICT,
                "run_destroy_failed",
                "Run could not be destroyed",
            ));
        }
    } else {
        crate::runs::container_exec::teardown_run_container(run_id).await;
        for path in &paths {
            if path.exists() {
                tokio::fs::remove_dir_all(path).await.map_err(|error| {
                    Error::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "run_destroy_failed",
                        format!("Run artifact cleanup failed: {error}"),
                    )
                })?;
            }
        }
    }
    finalize(&s, task_id, run_id, command_id, actor_id)
}
