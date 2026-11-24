//! Confirmed SRC control effects for the shared CommandAPI receipt/outbox sequence.
use super::*;

pub(super) const POLICY_APPLIED: &str = "src_policy_applied_commands";
pub(super) const GRANT_APPLIED: &str = "src_grant_revoke_applied_commands";

fn compatible_grant(s: &AppState, id: &str, grant: &Value, next: &Value) -> Api<bool> {
    if grant["policy_digest"] == next["digest"] {
        return Ok(true);
    }
    for reference in grant["rule_refs"].as_array().into_iter().flatten() {
        if !next["rules"].as_array().is_some_and(|rules| {
            rules.iter().any(|rule| {
                reference
                    == &json!(format!(
                        "{}:v{}",
                        rule["rule_id"].as_str().unwrap_or(""),
                        rule["version"].as_u64().unwrap_or(0)
                    ))
            })
        }) {
            return Ok(false);
        }
    }
    let Some(grant_id) = grant["grant_id"].as_str() else {
        return Ok(false);
    };
    let snapshot = match rec(s, id, "task_grant_environment", grant_id) {
        Ok(snapshot) => snapshot,
        Err(error) if error.status == StatusCode::NOT_FOUND => return Ok(false),
        Err(error) => return Err(error),
    };
    let snapshot = &snapshot["payload"];
    Ok(
        snapshot["environment_requirements"] == next["environment_requirements"]
            && snapshot["network_mode"] == next["network_mode"]
            && snapshot["inspection_requirements"] == next["inspection_requirements"],
    )
}

fn policy_followup_records(s: &AppState, id: &str, next: &Value) -> Api<Vec<(String, u64, Value)>> {
    let mut staged = Vec::new();
    for row in records(s, id, "task_grant")? {
        let mut grant = row["payload"].clone();
        if grant["gateway_grant"].is_object()
            || !grant["revoked_at"].is_null()
            || compatible_grant(s, id, &grant, next)?
        {
            continue;
        }
        grant["revoked_at"] = json!(now());
        grant["revocation_reason"] = json!("policy_incompatible");
        staged.push(stage_rec(
            s,
            id,
            "task_grant",
            row["record_id"].as_str().unwrap_or(""),
            grant,
            row["revision"].as_u64(),
        )?);
    }
    for row in records(s, id, "execution_approval")? {
        if row["payload"]["status"] != "pending"
            || row["payload"]["policy_version"] == next["version"]
        {
            continue;
        }
        let mut approval = row["payload"].clone();
        approval["status"] = json!("superseded");
        staged.push(stage_rec(
            s,
            id,
            "execution_approval",
            row["record_id"].as_str().unwrap_or(""),
            approval,
            row["revision"].as_u64(),
        )?);
    }
    Ok(staged)
}

fn policy_marker(s: &AppState, command_id: &str) -> Api<Option<Value>> {
    s.store.get(POLICY_APPLIED, command_id).map_err(db)
}

async fn activate_and_followup_policy(
    s: Arc<AppState>,
    id: &str,
    command_id: &str,
    final_output: Option<Value>,
) -> Api<Value> {
    loop {
        let marker = policy_marker(&s, command_id)?
            .ok_or_else(|| Error::conflict("policy effect marker disappeared"))?;
        if marker["phase"] == "completed" {
            return Ok(marker["output"].clone());
        }
        let version = marker["policy_version"].as_u64().unwrap_or(0);
        let current = s
            .store
            .get(POLICIES, id)
            .map_err(db)?
            .ok_or_else(|| Error::conflict("execution policy disappeared"))?;
        if current["values"]["version"].as_u64() != Some(version) {
            return Err(Error::conflict(
                "execution policy changed during activation",
            ));
        }
        let task_paused = task(&s, id)
            .ok()
            .is_some_and(|t| t["lifecycle"]["state"].as_str() == Some("paused"));
        let mut revised = current["values"].clone();
        if task_paused {
            revised["activation"] = json!({"state":"deferred_paused","generation":version});
        } else {
            let activated = domains::ensure_worker_gateway(&s, id).await;
            let activation = match &activated {
                Ok(environment) => json!({"state":"ready","generation":version,
                    "network_enforced":environment["network_enforced"],
                    "content_enforced":environment["content_enforced"],
                    "gateway_container":environment["gateway_container"]}),
                Err(error) => json!({"state":"failed","generation":version,"error":error.message}),
            };
            revised["activation"] = activation;
            if let Err(error) = activated {
                let failed = json!({"version":current["version"],"values":revised});
                let _ = s
                    .store
                    .put_many_if(
                        &[(POLICIES, id, Some(&current))],
                        &[(POLICIES, id, &failed)],
                    )
                    .map_err(db)?;
                return Err(error);
            }
        }
        domains::revoke_incompatible_inspection_grants(&s, id, &revised["digest"])?;
        let staged = policy_followup_records(&s, id, &revised)?;
        let mut previous = Vec::new();
        let mut documents = Vec::new();
        let mut conflict = false;
        for (rid, record_version, row) in &staged {
            let before = s.store.get(RECORDS, rid).map_err(db)?;
            if before
                .as_ref()
                .and_then(|value| value["version"].as_u64())
                .unwrap_or(0)
                != *record_version
            {
                conflict = true;
                break;
            }
            previous.push(before);
            documents.push(json!({"version":record_version+1,"values":row}));
        }
        if conflict {
            continue;
        }
        let next_policy = json!({"version":current["version"],"values":revised});
        let output_value = match &final_output {
            Some(custom) => {
                let mut merged = custom.clone();
                if let Some(obj) = merged.as_object_mut() {
                    obj.insert("activation".to_owned(), revised["activation"].clone());
                }
                merged
            }
            None => revised.clone(),
        };
        let mut next_marker = marker.clone();
        next_marker["phase"] = json!("completed");
        next_marker["output"] = output_value.clone();
        let mut conditions = vec![
            (POLICIES, id, Some(&current)),
            (POLICY_APPLIED, command_id, Some(&marker)),
        ];
        let mut writes = vec![
            (POLICIES, id, &next_policy),
            (POLICY_APPLIED, command_id, &next_marker),
        ];
        for (index, (rid, _, _)) in staged.iter().enumerate() {
            conditions.push((RECORDS, rid.as_str(), previous[index].as_ref()));
            writes.push((RECORDS, rid.as_str(), &documents[index]));
        }
        if s.store.put_many_if(&conditions, &writes).map_err(db)? {
            return Ok(output_value);
        }
    }
}

pub(super) async fn update_policy(
    s: Arc<AppState>,
    id: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let _browser_guard = browser_mutation_lock().lock().await;
    let _policy_owner=crate::src_resources::activation(&s,&format!("policy:{id}")).await.map_err(|message|Error::new(StatusCode::CONFLICT,"resource_busy",message))?;
    if policy_marker(&s,command_id)?.is_none(){
        let(version,next)=policy_candidate(&s,id,body,actor_id)?;
        let current=s.store.get(POLICIES,id).map_err(db)?.ok_or_else(||Error::conflict("execution policy disappeared"))?;
        if current["version"].as_u64()!=Some(version){return Err(Error::conflict("execution policy changed during validation"))}
        let marker=json!({"command_id":command_id,"task_id":id,"kind":"update_policy","actor_id":actor_id,"body":body,"phase":"quiescing","previous":current,"next":next,"policy_version":version+1,"output":null});
        if !s.store.put_many_if(&[(POLICIES,id,Some(&current)),(POLICY_APPLIED,command_id,None)],&[(POLICY_APPLIED,command_id,&marker)]).map_err(db)?{return Err(Error::conflict("execution policy changed before quiescence"))}
    }
    if let Some(marker)=policy_marker(&s,command_id)?.filter(|marker|marker["phase"]=="quiescing"){
        let previous=&marker["previous"];let current=s.store.get(POLICIES,id).map_err(db)?;
        if current.as_ref()!=Some(previous){return Err(Error::conflict("execution policy changed before quiescence recovery"))}
        domains::quiesce_browser(s.clone(),id).await?;
        domains::quiesce_streams(s.clone(),id,"policy_changed").await?;
        let mut committed=marker.clone();committed["phase"]=json!("committed");
        let next=json!({"version":marker["policy_version"],"values":marker["next"]});
        if !s.store.put_many_if(&[(POLICIES,id,Some(previous)),(POLICY_APPLIED,command_id,Some(&marker))],&[(POLICIES,id,&next),(POLICY_APPLIED,command_id,&committed)]).map_err(db)?{return Err(Error::conflict("execution policy changed before control commit"))}
    }
    activate_and_followup_policy(s, id, command_id, None).await
}

pub(super) async fn renew_authorization(
    s: Arc<AppState>,
    id: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let _browser_guard = browser_mutation_lock().lock().await;
    let _policy_owner = crate::src_resources::activation(&s, &format!("policy:{id}"))
        .await
        .map_err(|message| Error::new(StatusCode::CONFLICT, "resource_busy", message))?;
    if policy_marker(&s, command_id)?.is_none() {
        let current_task = super::task(&s, id)?;
        let current_policy_doc = s
            .store
            .get(POLICIES, id)
            .map_err(db)?
            .ok_or_else(|| Error::conflict("execution policy disappeared"))?;
        let policy_doc_version = current_policy_doc["version"].as_u64().unwrap_or(1);
        let policy_values = &current_policy_doc["values"];
        let base_policy_version = body["base_policy_version"]
            .as_u64()
            .unwrap_or(policy_doc_version);
        if body.get("base_policy_version").and_then(Value::as_u64).is_some()
            && (policy_values["version"].as_u64() != Some(base_policy_version)
                || policy_doc_version != base_policy_version)
        {
            return Err(Error::conflict("授权策略版本已变化，请刷新后重试"));
        }
        let mut candidate_body = json!({
            "base_version": base_policy_version,
            "authorized_targets": body["targets"].clone(),
        });
        if let Some(excluded) = body.get("excluded_targets").filter(|v| !v.is_null()) {
            candidate_body["excluded_targets"] = excluded.clone();
        }
        if let Some(exclusions) = body.get("exclusions").filter(|v| !v.is_null()) {
            candidate_body["exclusions"] = exclusions.clone();
        }
        if let Some(reason) = body.get("scope_fuse_override_reason").filter(|v| !v.is_null()) {
            candidate_body["scope_fuse_override_reason"] = reason.clone();
        }
        let (_, next_policy) = policy_candidate(&s, id, &candidate_body, actor_id)?;
        let prev_targets: Vec<String> = policy_values["authorized_targets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
        let next_targets: Vec<String> = next_policy["authorized_targets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
        let added_targets: Vec<String> = next_targets
            .iter()
            .filter(|t| !prev_targets.contains(*t))
            .cloned()
            .collect();
        let removed_targets: Vec<String> = prev_targets
            .iter()
            .filter(|t| !next_targets.contains(*t))
            .cloned()
            .collect();

        let existing_snaps = records(&s, id, "authorization_snapshot")?;
        let max_snap_ver = existing_snaps
            .iter()
            .filter_map(|row| row["payload"]["version"].as_u64())
            .max()
            .unwrap_or(0);
        let snap_version = max_snap_ver + 1;
        let next_version = next_policy["version"].as_u64().unwrap_or(base_policy_version + 1);
        let snap_id = format!("sauth-v{next_version}-{}", uuid::Uuid::new_v4().simple());
        let stamp = now();
        let conditions_val = body
            .get("conditions")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| {
                current_task["input"]["authorization"]["conditions"].clone()
            });
        let raw_policy_text = body["raw_policy_text"]
            .as_str()
            .or_else(|| body["notes"].as_str())
            .or_else(|| current_task["input"]["authorization"]["notes"].as_str())
            .unwrap_or("")
            .to_owned();
        let diff_summary = body["diff_summary"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                format!(
                    "v{base_policy_version} -> v{next_version}（新增 {}，移除 {}，共 {} 个授权目标）",
                    added_targets.len(),
                    removed_targets.len(),
                    next_targets.len()
                )
            });
        let snap_payload = json!({
            "record_id": snap_id,
            "version": snap_version,
            "policy_version": next_version,
            "policy_digest": next_policy["digest"],
            "allowed_targets": next_policy["authorized_targets"],
            "excluded_targets": next_policy["excluded_targets"],
            "exclusions": next_policy["exclusions"],
            "conditions": if conditions_val.is_null() { json!([]) } else { conditions_val.clone() },
            "raw_policy_text": raw_policy_text,
            "diff_summary": diff_summary,
            "added_targets": added_targets,
            "removed_targets": removed_targets,
            "created_by": actor_id,
            "created_at": stamp,
        });
        let (snap_key, _, snap_row) = stage_rec(&s, id, "authorization_snapshot", &snap_id, snap_payload, Some(0))?;
        let snap_doc = json!({"version": 1, "values": snap_row});

        let mut next_task = current_task.clone();
        let rev = next_task["revision"].as_u64().unwrap_or(1);
        next_task["revision"] = json!(rev + 1);
        next_task["updated_at"] = json!(stamp);
        if !next_task["input"].is_object() {
            next_task["input"] = json!({});
        }
        if !next_task["input"]["authorization"].is_object() {
            next_task["input"]["authorization"] = json!({});
        }
        next_task["input"]["authorization"]["targets"] = next_policy["authorized_targets"].clone();
        next_task["input"]["authorization"]["excluded_targets"] = next_policy["excluded_targets"].clone();
        next_task["input"]["authorization"]["version"] = json!(next_version);
        next_task["input"]["authorization"]["notes"] = json!(raw_policy_text);
        if !conditions_val.is_null() {
            next_task["input"]["authorization"]["conditions"] = conditions_val;
        }
        next_task["input"]["authorization"]["updated_at"] = json!(stamp);

        let output = json!({
            "policy_version": next_version,
            "policy_digest": next_policy["digest"],
            "snapshot_id": snap_id,
            "authorized_targets": next_policy["authorized_targets"],
            "excluded_targets": next_policy["excluded_targets"],
            "added_targets": added_targets,
            "removed_targets": removed_targets,
        });
        let marker = json!({
            "command_id": command_id,
            "task_id": id,
            "kind": "renew_authorization",
            "actor_id": actor_id,
            "body": body,
            "phase": "quiescing",
            "previous": current_policy_doc,
            "previous_task": current_task,
            "next": next_policy,
            "next_task": next_task,
            "snap_key": snap_key,
            "snap_doc": snap_doc,
            "policy_version": next_version,
            "output": output,
        });
        if !s
            .store
            .put_many_if(
                &[
                    (POLICIES, id, Some(&current_policy_doc)),
                    (TASKS, id, Some(&current_task)),
                    (POLICY_APPLIED, command_id, None),
                ],
                &[(POLICY_APPLIED, command_id, &marker)],
            )
            .map_err(db)?
        {
            return Err(Error::conflict("授权策略或任务状态已变化，请刷新后重试"));
        }
    }
    if let Some(marker) = policy_marker(&s, command_id)?.filter(|m| m["phase"] == "quiescing") {
        let previous = &marker["previous"];
        let previous_task = &marker["previous_task"];
        let current = s.store.get(POLICIES, id).map_err(db)?;
        let current_task = s.store.get(TASKS, id).map_err(db)?;
        if current.as_ref() != Some(previous) || current_task.as_ref() != Some(previous_task) {
            return Err(Error::conflict("授权续期停止并发阶段任务状态已变更"));
        }
        domains::quiesce_browser(s.clone(), id).await?;
        domains::quiesce_streams(s.clone(), id, "authorization_renewed").await?;
        let mut committed = marker.clone();
        committed["phase"] = json!("committed");
        let next_policy_doc = json!({"version": marker["policy_version"], "values": marker["next"]});
        let next_task = &marker["next_task"];
        let snap_key = marker["snap_key"].as_str().unwrap_or("");
        let snap_doc = &marker["snap_doc"];
        if !s
            .store
            .put_many_if(
                &[
                    (POLICIES, id, Some(previous)),
                    (TASKS, id, Some(previous_task)),
                    (RECORDS, snap_key, None),
                    (POLICY_APPLIED, command_id, Some(&marker)),
                ],
                &[
                    (POLICIES, id, &next_policy_doc),
                    (TASKS, id, next_task),
                    (RECORDS, snap_key, snap_doc),
                    (POLICY_APPLIED, command_id, &committed),
                ],
            )
            .map_err(db)?
        {
            return Err(Error::conflict("授权续期提交前状态已变更"));
        }
    }
    let output_template = policy_marker(&s, command_id)?.map(|m| m["output"].clone());
    activate_and_followup_policy(s, id, command_id, output_template).await
}

pub(super) fn revoke_grant(s: &AppState, id: &str, body: &Value, command_id: &str) -> Api<Value> {
    if let Some(marker) = s.store.get(GRANT_APPLIED, command_id).map_err(db)? {
        if marker["phase"] == "completed" {
            return Ok(marker["output"].clone());
        }
    }
    let grant_id = require(body, "grant_id")?;
    let row = rec(s, id, "task_grant", grant_id)?;
    if row["payload"]["gateway_grant"].is_object() {
        return domains::revoke_inspection_grant_marked(s, id, body, command_id, GRANT_APPLIED);
    }
    let rev = row["revision"].as_u64().unwrap_or(0);
    if body["base_revision"].as_u64() != Some(rev) {
        return Err(Error::conflict("授权版本已变化，请刷新后重试"));
    }
    let mut grant = row["payload"].clone();
    grant["revoked_at"] = json!(now());
    let (rid, version, row) = stage_rec(
        s,
        id,
        "task_grant",
        row["record_id"].as_str().unwrap_or(grant_id),
        grant,
        Some(rev),
    )?;
    let previous = s.store.get(RECORDS, &rid).map_err(db)?;
    let document = json!({"version":version+1,"values":row});
    let output = json!({"grant_id":grant_id,"revoked":true});
    let marker = json!({"command_id":command_id,"phase":"completed","output":output});
    if s.store
        .put_many_if(
            &[
                (RECORDS, &rid, previous.as_ref()),
                (GRANT_APPLIED, command_id, None),
            ],
            &[
                (RECORDS, &rid, &document),
                (GRANT_APPLIED, command_id, &marker),
            ],
        )
        .map_err(db)?
    {
        return Ok(output);
    }
    if let Some(saved) = s.store.get(GRANT_APPLIED, command_id).map_err(db)? {
        return Ok(saved["output"].clone());
    }
    Err(Error::conflict("SRC grant revision changed"))
}

pub(super) async fn recover(s:&Arc<AppState>)->Api<()>{
    for marker in s.store.list(POLICY_APPLIED).map_err(db)?.into_iter().filter(|marker|marker["phase"]!="completed"){
        let Some(id)=marker["task_id"].as_str()else{continue};let command=require(&marker,"command_id")?;let actor=marker["actor_id"].as_str().unwrap_or("system:recovery");
        let outcome = if marker["kind"] == "renew_authorization" {
            renew_authorization(s.clone(), id, &marker["body"], command, actor).await
        } else {
            update_policy(s.clone(),id,&marker["body"],command,actor).await
        };
        if let Err(error) = outcome {
            tracing::warn!(
                task_id = id,
                command_id = command,
                code = error.code,
                %error.message,
                "SRC policy recovery skipped failed marker"
            );
        }
    }Ok(())
}
