use super::*;
use std::sync::Arc;

const RUNS: &str = "runs";

fn committed(s: &AppState, command_id: &str) -> Api<Option<Value>> {
    Ok(s.store
        .get(RUN_LIFECYCLE_APPLIED, command_id)
        .map_err(db)?
        .map(|marker| marker["output"].clone()))
}

pub(super) async fn apply(
    s: Arc<AppState>,
    task_id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let _lock = super::task_lifecycle::lock_task(&s, task_id).await?;
    if let Some(output) = committed(&s, command_id)? {
        return Ok(output);
    }
    let run_id = require(body, "run_id")?;
    let task_row = task(&s, task_id)?;
    if task_row["binding"].is_null() {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "SRC Task binding is unavailable",
        ));
    }
    if kind == "src.run.restore" && task_row["lifecycle"]["state"] != "active" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "task_archived",
            "SRC Task is archived; restore it before new work",
        ));
    }
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
    loop {
        let current_run = s.store.get(RUNS, run_id).map_err(db)?.ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "run.not_found",
                format!("unknown run {run_id:?}"),
            )
        })?;
        if current_run["src_task_id"] != task_id {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "task_binding_invalid",
                "Run belongs to another SRC Task",
            ));
        }
        let record_key = format!("run_lifecycle:{task_id}:{run_id}");
        let current_record = s.store.get(RECORDS, &record_key).map_err(db)?;
        let revision = current_record
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0)
            + 1;
        let stamp = now();
        let archived = kind == "src.run.archive";
        let mut payload = json!({"state":if archived {"archived"} else {"active"},
        "actor_id":actor_id});
        payload[if archived {
            "archived_at"
        } else {
            "restored_at"
        }] = json!(stamp);
        let output = json!({"record_id":run_id,"kind":"run_lifecycle","task_id":task_id,
        "revision":revision,"parent_id":task_id,"natural_key":"","generation":0,
        "created_at":current_record.as_ref().and_then(|row| row["values"]["created_at"].as_str()).unwrap_or(&stamp),
        "updated_at":stamp,"payload":payload});
        let record = json!({"version":revision,"values":output});
        let mut next_run = current_run.clone();
        let updated = crate::runs::now_secs();
        next_run["archived"] = json!(archived);
        next_run["archived_at"] = if archived {
            json!(updated)
        } else {
            Value::Null
        };
        next_run["updated"] = json!(updated);
        if archived {
            next_run["pinned"] = json!(false);
            next_run["pinned_at"] = Value::Null;
        }
        let marker = json!({"command_id":command_id,"output":output});
        if s.store
            .put_many_if(
                &[
                    (RUNS, run_id, Some(&current_run)),
                    (RECORDS, &record_key, current_record.as_ref()),
                    (RUN_LIFECYCLE_APPLIED, command_id, None),
                ],
                &[
                    (RUNS, run_id, &next_run),
                    (RECORDS, &record_key, &record),
                    (RUN_LIFECYCLE_APPLIED, command_id, &marker),
                ],
            )
            .map_err(db)?
        {
            return Ok(output);
        }
        if let Some(output) = committed(&s, command_id)? {
            return Ok(output);
        }
    }
}
