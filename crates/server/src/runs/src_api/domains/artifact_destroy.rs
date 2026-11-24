use super::*;
use std::sync::Arc;

const ARTIFACTS: &str = "src_artifacts";

fn completed(s: &AppState, command_id: &str) -> Api<Option<Value>> {
    Ok(s.store
        .get(ARTIFACT_DESTROY_APPLIED, command_id)
        .map_err(db)?
        .map(|marker| marker["output"].clone()))
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
    if body.get("sha256").is_some() || body.get("sha512").is_some() {
        return Err(Error::invalid("artifact_id must identify the artifact"));
    }
    let artifact_id = require(body, "artifact_id")?;
    loop {
        let current_task = task(&s, task_id)?;
        if current_task["binding"].is_null() {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "task_binding_invalid",
                "SRC Task binding is unavailable",
            ));
        }
        if current_task["lifecycle"]["state"] != "archived" {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "task_not_archived",
                "Artifact destruction requires an archived Task",
            ));
        }
        let key = format!("artifact_disposal:{task_id}:{artifact_id}");
        let previous = s.store.get(RECORDS, &key).map_err(db)?;
        let revision = previous
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0)
            + 1;
        let stamp = now();
        let output = json!({"record_id":artifact_id,"kind":"artifact_disposal","task_id":task_id,
            "revision":revision,"parent_id":"","natural_key":"","generation":0,
            "created_at":previous.as_ref().and_then(|row| row["values"]["created_at"].as_str()).unwrap_or(&stamp),
            "updated_at":stamp,
            "payload":{"artifact_id":artifact_id,"state":"destroyed","actor_id":actor_id,"destroyed_at":stamp}});
        let document = json!({"version":revision,"values":output});
        let marker = json!({"command_id":command_id,"output":output});
        let mut refusal: Option<Error> = None;
        let mut cas_to_clean: Option<String> = None;
        let committed = s
            .store
            .put_many_if_with_deletes(
                &[
                    (TASKS, task_id, Some(&current_task)),
                    (RECORDS, &key, previous.as_ref()),
                    (ARTIFACT_DESTROY_APPLIED, command_id, None),
                ],
                &[
                    (RECORDS, &key, &document),
                    (ARTIFACT_DESTROY_APPLIED, command_id, &marker),
                ],
                |transaction| {
                    let mut statement = transaction.prepare(
                        "SELECT id,value FROM documents WHERE collection=?1 ORDER BY id",
                    )?;
                    let rows = statement.query_map([ARTIFACTS], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?;
                    let mut selected = None;
                    for row in rows {
                        let (storage_id, serialized) = row?;
                        let artifact: Value = serde_json::from_str(&serialized)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?;
                        if artifact["task_id"] == task_id && artifact["artifact_id"] == artifact_id
                        {
                            selected = Some((storage_id, artifact));
                            break;
                        }
                    }
                    let Some((storage_id, artifact)) = selected else {
                        refusal = Some(Error::new(
                            StatusCode::NOT_FOUND,
                            "artifact_missing",
                            "Artifact bytes are unavailable",
                        ));
                        return Ok(None);
                    };
                    let aliases: Vec<&str> = [
                        artifact_id,
                        artifact["sha256"].as_str().unwrap_or(""),
                    ]
                    .into_iter()
                    .filter(|value| !value.is_empty())
                    .collect();
                    let mut statement = transaction
                        .prepare("SELECT value FROM documents WHERE collection=?1 ORDER BY id")?;
                    let rows = statement.query_map([RECORDS], |row| row.get::<_, String>(0))?;
                    let mut references = Vec::new();
                    for row in rows {
                        let serialized = row?;
                        let document: Value = serde_json::from_str(&serialized)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?;
                        let record = &document["values"];
                        if record["kind"] != "artifact_disposal"
                            && aliases
                                .iter()
                                .any(|alias| record["payload"].to_string().contains(alias))
                        {
                            references.push(json!({"task_id":record["task_id"],
                                "kind":record["kind"],"record_id":record["record_id"]}));
                        }
                    }
                    if !references.is_empty() {
                        let mut error = Error::new(
                            StatusCode::CONFLICT,
                            "artifact_in_use",
                            "Artifact is referenced and cannot be destroyed",
                        );
                        error.detail = json!({"references":references});
                        refusal = Some(error);
                        return Ok(None);
                    }
                    cas_to_clean = artifact["cas_sha256"]
                        .as_str()
                        .or_else(|| artifact["sha256"].as_str())
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned);
                    Ok(Some(vec![(ARTIFACTS.to_owned(), storage_id)]))
                },
            )
            .map_err(db)?;
        if let Some(error) = refusal {
            return Err(error);
        }
        if committed {
            if let Some(sha) = cas_to_clean.as_deref() {
                super::remove_cas_if_unreferenced(&s, task_id, sha);
            }
            return Ok(output);
        }
        if let Some(output) = completed(&s, command_id)? {
            return Ok(output);
        }
    }
}
