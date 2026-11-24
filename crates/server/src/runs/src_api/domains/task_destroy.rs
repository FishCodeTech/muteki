use super::*;
use std::sync::Arc;

const ARTIFACTS: &str = "src_artifacts";
const PROTECTED: &[&str] = &[
    "frozen_report",
    "asset_publication",
    "asset_reference",
    "monitor_baseline",
    "knowledge_publication",
];
const RETAINED: &[&str] = &[
    "task_lifecycle",
    "task_tombstone",
    "artifact_disposal",
    "run_lifecycle",
    "run_disposal",
    "run_binding",
];

pub(super) fn cleanup_cas(s: &AppState, id: &str) -> Api<()> {
    if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\', '\0']) {
        return Err(Error::invalid("invalid Task CAS namespace"));
    }
    let Some(root) = s.store.get("system", "state_dir").map_err(db)?.and_then(|value| value.as_str().map(std::path::PathBuf::from)) else { return Ok(()) };
    let cas = root.join("src-cas");
    let directory = cas.join(id);
    for path in [&cas, &directory] {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {},
            result => return Err(Error::new(StatusCode::CONFLICT, "src.cas_cleanup_failed",
                format!("Task CAS cleanup requires an owned directory: {result:?}"))),
        }
    }
    std::fs::remove_dir_all(&directory).map_err(|error| Error::new(StatusCode::INTERNAL_SERVER_ERROR,
        "src.cas_cleanup_failed", format!("Task records are destroyed but CAS cleanup must be retried: {error}")))
}

fn completed(s: &AppState, command_id: &str) -> Api<Option<Value>> {
    Ok(s.store
        .get(TASK_DESTROY_APPLIED, command_id)
        .map_err(db)?
        .map(|marker| marker["output"].clone()))
}

fn task_record(s: &AppState, id: &str) -> Api<Value> {
    let row = task(s, id)?;
    if row["binding"]["thread_id"]
        .as_str()
        .is_none_or(str::is_empty)
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "SRC Task binding is unavailable",
        ));
    }
    if row["lifecycle"]["state"] != "archived" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "task_not_archived",
            "Task destruction requires an archived Task",
        ));
    }
    Ok(row)
}

pub(super) async fn apply(
    s: Arc<AppState>,
    id: &str,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let _lock = super::task_lifecycle::lock_task(&s, id).await?;
    loop {
        if let Some(output) = completed(&s, command_id)? {
            cleanup_cas(&s, id)?;
            return Ok(output);
        }
        if task(&s, id)?["lifecycle"]["state"] == "destroyed" {
            cleanup_cas(&s, id)?;
            return Ok(rec(&s, id, "task_tombstone", id)?);
        }
        let current_task = task_record(&s, id)?;
        let main_thread_id = current_task["binding"]["thread_id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let mut owned_threads = if main_thread_id.is_empty() {
            Vec::new()
        } else {
            vec![main_thread_id.clone()]
        };
        let key = format!("task_tombstone:{id}");
        let previous = s.store.get(RECORDS, &key).map_err(db)?;
        let version = previous
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0)
            + 1;
        let stamp = now();
        let payload = json!({"state":"destroyed","actor_id":actor_id,"destroyed_at":stamp});
        let output = json!({"record_id":id,"kind":"task_tombstone","task_id":id,
            "revision":version,"parent_id":"","natural_key":"","generation":0,
            "created_at":previous.as_ref().and_then(|row| row["values"]["created_at"].as_str()).unwrap_or(&stamp),
            "updated_at":stamp,"payload":payload});
        let document = json!({"version":version,"values":output});
        let marker = json!({"command_id":command_id,"output":output});
        let mut next_task = current_task.clone();
        next_task["lifecycle"] = payload;
        next_task["binding"] = Value::Null;
        let mut refusal: Option<Error> = None;
        let mut delegated_children = Vec::<String>::new();
        let committed = s
            .store
            .put_many_if_with_deletes(
                &[
                    (TASKS, id, Some(&current_task)),
                    (RECORDS, &key, previous.as_ref()),
                    (TASK_DESTROY_APPLIED, command_id, None),
                ],
                &[
                    (TASKS, id, &next_task),
                    (RECORDS, &key, &document),
                    (TASK_DESTROY_APPLIED, command_id, &marker),
                ],
                |transaction| {
                    let mut statement = transaction.prepare(
                        "SELECT id,value FROM documents WHERE collection=?1 ORDER BY id",
                    )?;
                    let rows = statement.query_map([RECORDS], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?;
                    let mut records = Vec::new();
                    for row in rows {
                        let (storage_id, serialized) = row?;
                        let document: Value = serde_json::from_str(&serialized)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?;
                        records.push((storage_id, document["values"].clone()));
                    }
                    delegated_children = records
                        .iter()
                        .filter(|(_, row)| row["task_id"] == id && row["kind"] == "src_thread_delegation")
                        .filter_map(|(_, row)| row["payload"]["child_thread_id"].as_str().map(str::to_owned))
                        .collect();
                    let protected: Vec<Value> = records
                        .iter()
                        .filter(|(_, row)| {
                            row["task_id"] == id
                                && PROTECTED.iter().any(|kind| row["kind"] == *kind)
                        })
                        .map(|(_, row)| json!({"kind":row["kind"],"record_id":row["record_id"]}))
                        .collect();
                    if !protected.is_empty() {
                        let mut error = Error::new(
                            StatusCode::CONFLICT,
                            "artifact_in_use",
                            "Task is protected by frozen or shared references",
                        );
                        error.detail = json!({"references":protected});
                        refusal = Some(error);
                        return Ok(None);
                    }
                    for (_, binding) in records
                        .iter()
                        .filter(|(_, row)| row["task_id"] == id && row["kind"] == "run_binding")
                    {
                        if binding["payload"]["destroyed_at"].as_str().is_some() {
                            continue;
                        }
                        let run_id = binding["payload"]["run_id"].as_str().unwrap_or("");
                        let archived = records.iter().any(|(_, row)| {
                            row["task_id"] == id
                                && row["kind"] == "run_lifecycle"
                                && row["record_id"] == run_id
                                && row["payload"]["state"] == "archived"
                        });
                        if !archived {
                            refusal = Some(Error::new(
                                StatusCode::CONFLICT,
                                "run_not_archived",
                                format!("Run {run_id} must be archived or destroyed first"),
                            ));
                            return Ok(None);
                        }
                    }
                    let mut artifact_statement = transaction.prepare(
                        "SELECT id,value FROM documents WHERE collection=?1 ORDER BY id",
                    )?;
                    let artifact_rows = artifact_statement.query_map([ARTIFACTS], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?;
                    let mut artifacts = Vec::new();
                    for row in artifact_rows {
                        let (storage_id, serialized) = row?;
                        let artifact: Value = serde_json::from_str(&serialized)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?;
                        if artifact["task_id"] == id {
                            artifacts.push((storage_id, artifact));
                        }
                    }
                    for (_, artifact) in &artifacts {
                        let artifact_id = artifact["artifact_id"].as_str().unwrap_or("");
                        let aliases: Vec<&str> = [
                            artifact_id,
                            artifact["sha256"].as_str().unwrap_or(""),
                        ]
                        .into_iter()
                        .filter(|value| !value.is_empty())
                        .collect();
                        let references: Vec<Value> = records
                            .iter()
                            .filter(|(_, row)| {
                                row["task_id"] != id
                                    && aliases
                                        .iter()
                                        .any(|alias| row["payload"].to_string().contains(alias))
                            })
                            .map(|(_, row)| {
                                json!({"task_id":row["task_id"],"kind":row["kind"],
                            "record_id":row["record_id"]})
                            })
                            .collect();
                        if !references.is_empty() {
                            let mut error = Error::new(
                                StatusCode::CONFLICT,
                                "artifact_in_use",
                                "Task artifacts are referenced by another Task",
                            );
                            error.detail =
                                json!({"artifact_id":artifact_id,"references":references});
                            refusal = Some(error);
                            return Ok(None);
                        }
                    }
                    let mut deletes: Vec<(String, String)> = records
                        .into_iter()
                        .filter(|(_, row)| {
                            row["task_id"] == id
                                && !RETAINED.iter().any(|kind| row["kind"] == *kind)
                        })
                        .map(|(storage_id, _)| (RECORDS.to_owned(), storage_id))
                        .collect();
                    deletes.extend(
                        artifacts
                            .into_iter()
                            .map(|(storage_id, _)| (ARTIFACTS.to_owned(), storage_id)),
                    );
                    deletes.push((super::super::POLICIES.to_owned(), id.to_owned()));
                    let mut config_statement =
                        transaction.prepare("SELECT id FROM documents WHERE collection=?1")?;
                    let config_ids = config_statement
                        .query_map([super::super::CHAT_CONFIG_KEYS], |row| {
                            row.get::<_, String>(0)
                        })?;
                    for key in config_ids {
                        let key = key?;
                        if serde_json::from_str::<Value>(&key)
                            .ok()
                            .is_some_and(|value| value[0] == id)
                        {
                            deletes.push((super::super::CHAT_CONFIG_KEYS.to_owned(), key));
                        }
                    }
                    Ok(Some(deletes))
                },
            )
            .map_err(db)?;
        if let Some(error) = refusal {
            return Err(error);
        }
        if committed {
            for child in delegated_children {
                if !owned_threads.contains(&child) {
                    owned_threads.push(child);
                }
            }
            for tid in &owned_threads {
                s.capability_gateway.release(tid);
            }
            if let Err((_, axum::Json(error))) = crate::chat::purge_src_owned_threads(&s, &owned_threads).await {
                tracing::warn!(task_id = id, %error, "Failed to purge destroyed SRC threads");
            }
            cleanup_cas(&s, id)?;
            return Ok(output);
        }
    }
}
