//! Credential bytes stay on HTTPS/private files; command results contain paths and metadata only.
use super::{
    Execution, endpoint, http,
    transfer::{self, Connection},
};
use crate::runner_remote::{
    commands::{Authority, Command, Operation},
    credentials::{Grant, MAX_BYTES, Material},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
const TABLE: &str = "runner_credential_projections";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    grant: Grant,
    material: Material,
}
fn root(execution: &Execution) -> Result<PathBuf, String> {
    serde_json::from_value(
        execution
            .processor
            .store()
            .get("system", "state_dir")
            .map_err(|e| e.to_string())?
            .ok_or("Node root missing")?,
    )
    .map_err(|e| e.to_string())
}
fn matching(grant: &Grant, command: &Command) -> Result<(), String> {
    if grant.scope.node_id != command.node_id
        || grant.scope.project_id != command.project_id
        || grant.scope.run_id != command.run_id
        || grant.scope.generation != command.generation
        || grant.state != "active"
        || grant.expires_at <= chrono::Utc::now().timestamp()
    {
        return Err("Credential belongs to a different or expired execution scope".into());
    }
    Ok(())
}
fn deadline(grant: &Grant, requested: tokio::time::Instant) -> tokio::time::Instant {
    let ttl = (grant.expires_at - chrono::Utc::now().timestamp()).clamp(0, 15) as u64;
    requested + Duration::from_secs(ttl)
}
async fn get(
    execution: &Execution,
    connection: &Connection,
    command: &Command,
    path: &str,
    limit: usize,
) -> Result<Value, String> {
    let client = http(connection.ca_pem.as_deref()).map_err(|e| e.to_string())?;
    tokio::time::timeout(Duration::from_secs(5), async {
        let response = transfer::get(execution, connection, command, &client, path).await?;
        serde_json::from_slice(&transfer::bytes(response, limit).await?)
            .map_err(|_| "Invalid credential response".into())
    })
    .await
    .map_err(|_| "Credential authority request timed out".to_string())?
}
pub(super) async fn dispatch(
    execution: Arc<Execution>,
    connection: Connection,
    command: Command,
    initial: Authority,
) -> Result<Value, String> {
    let id = match &command.action {
        Operation::CredentialInstall { grant_id, .. }
        | Operation::CredentialRefresh { grant_id }
        | Operation::CredentialStatus { grant_id } => grant_id.clone(),
        _ => return Err("Not a credential action".into()),
    };
    if id.len() > 128
        || id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err("Invalid credential grant identity".into());
    }
    execution.processor.file_scope(&command, &initial, true)?;
    let gate = execution.processor.gate(&format!("credential:{id}")).await;
    let _job = gate.lock().await;
    let prior = execution
        .processor
        .store()
        .get(TABLE, &id)
        .map_err(|e| e.to_string())?;
    if let Some(row) = &prior {
        let grant: Grant =
            serde_json::from_value(row["grant"].clone()).map_err(|e| e.to_string())?;
        if grant.scope.node_id != command.node_id
            || grant.scope.project_id != command.project_id
            || grant.scope.run_id != command.run_id
            || grant.scope.generation != command.generation
        {
            return Err("Credential projection belongs to another execution".into());
        }
    }
    if matches!(command.action, Operation::CredentialStatus { .. }) {
        return prior.ok_or("Credential projection is unknown".into());
    }
    let _slot = execution
        .files
        .clone()
        .try_acquire_owned()
        .map_err(|_| "Credential transfer slots are busy")?;
    if matches!(command.action, Operation::CredentialRefresh { .. }) {
        return refresh(
            execution,
            connection,
            command,
            &id,
            prior.ok_or("Credential is not installed")?,
        )
        .await;
    }
    let requested = tokio::time::Instant::now();
    let envelope: Envelope = serde_json::from_value(
        get(
            &execution,
            &connection,
            &command,
            &format!("/v1/runner/credential-grants/{id}"),
            MAX_BYTES * 2,
        )
        .await?,
    )
    .map_err(|_| "Invalid credential bundle")?;
    let grant = envelope.grant;
    let material = envelope.material;
    matching(&grant, &command)?;
    if let Operation::CredentialInstall { revision, .. } = &command.action {
        if *revision != grant.revision {
            return Err("Credential grant version changed before installation".into());
        }
    }
    if grant.id != id || material.revision()? != grant.revision {
        return Err("Credential bundle revision mismatch".into());
    }
    let authority_until = deadline(&grant, requested);
    let run_gate = execution.processor.gate(&command.run_id).await;
    let guard = run_gate.lock().await;
    transfer::authority(&execution, &command)?;
    if tokio::time::Instant::now() >= authority_until {
        return Err("Credential validation expired while waiting for execution".into());
    }
    let binding = execution
        .processor
        .store()
        .get("runner_remote_bindings", &command.run_id)
        .map_err(|e| e.to_string())?
        .ok_or("Run binding missing")?;
    if !binding["credential_materials"]
        .as_array()
        .is_some_and(|refs| refs.iter().any(|id| id == &grant.material_id))
    {
        return Err("Credential was not declared in this runtime's credential trust domain".into());
    }
    if let Some(row) = prior {
        if row["state"] == "active" && row["grant"]["revision"] == grant.revision {
            execution
                .credential_deadlines
                .lock()
                .unwrap()
                .insert(id, authority_until);
            return Ok(row);
        }
        if row["state"] != "installing" {
            return Err("Credential projection requires explicit reconciliation".into());
        }
    }
    let workspace = execution.processor.file_workspace(&command)?;
    let home = workspace.join("homes").join(format!("credential-{id}"));
    let node_root = root(&execution)?;
    let contract = execution
        .processor
        .store()
        .get("runner_runs", &command.run_id)
        .map_err(|e| e.to_string())?
        .ok_or("Runtime contract missing")?;
    let container_home = format!(
        "{}/homes/credential-{id}",
        contract["container_workspace"]
            .as_str()
            .ok_or("Runtime needs upgraded path metadata")?
    );
    let uid = contract["workspace_uid"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or("Workspace UID missing")?;
    let gid = contract["workspace_gid"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or("Workspace GID missing")?;
    let mut env = BTreeMap::from([("HOME".to_owned(), container_home.clone())]);
    for (key, path) in material.env_files.iter().chain(material.env_dirs.iter()) {
        env.insert(key.clone(), format!("{container_home}/{path}"));
    }
    let mut row = json!({"grant":grant,"state":"installing","home_relative":home.strip_prefix(&node_root).map_err(|_|"Credential home leaves node root")?,"env":env,
        "kind":material.kind,"files":material.files.keys().collect::<Vec<_>>(),"env_files":material.env_files,"env_dirs":material.env_dirs,"command":command});
    row["bytes"] = json!(
        material
            .files
            .values()
            .map(|value| STANDARD
                .decode(value)
                .map(|data| data.len() as u64)
                .unwrap_or(u64::MAX))
            .try_fold(0u64, |sum, size| sum.checked_add(size))
            .ok_or("Credential size overflow")?
    );
    execution
        .processor
        .store()
        .document_scope(|| -> Result<(), String> {
            if execution
                .processor
                .store()
                .get(TABLE, &id)
                .map_err(|e| e.to_string())?
                .is_none()
            {
                let rows = execution
                    .processor
                    .store()
                    .list(TABLE)
                    .map_err(|e| e.to_string())?;
                let active: Vec<_> = rows
                    .iter()
                    .filter(|row| row["state"] != "removed")
                    .collect();
                let used = active
                    .iter()
                    .try_fold(0u64, |sum, row| {
                        sum.checked_add(row["bytes"].as_u64().unwrap_or(u64::MAX))
                    })
                    .ok_or("Credential reservation accounting overflow")?;
                if active.len() >= 4096
                    || row["bytes"].as_u64().unwrap_or(u64::MAX)
                        > (64u64 * 1024 * 1024).saturating_sub(used)
                {
                    return Err("Node credential projection quota exceeded".into());
                }
            }
            execution
                .processor
                .store()
                .put(TABLE, &id, &row)
                .map_err(|e| e.to_string())
        })
        .map_err(|e| e.to_string())??;
    execution
        .credential_deadlines
        .lock()
        .unwrap()
        .insert(id.clone(), authority_until);
    drop(guard);
    let disk_execution = execution.clone();
    let disk_command = command.clone();
    let ownership_workspace = workspace.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        for (path, content) in material.files {
            transfer::authority(&disk_execution, &disk_command)?;
            if tokio::time::Instant::now() >= authority_until {
                return Err("Credential validation expired during installation".into());
            }
            let bytes = STANDARD
                .decode(content)
                .map_err(|_| "Invalid credential encoding")?;
            crate::private_files::write(&node_root, &home.join(path), &bytes)?;
        }
        crate::runs::container_exec::chown_tree_to_worker(&ownership_workspace, &home, uid, gid);
        crate::runs::container_exec::chown_ancestors_to_worker(&ownership_workspace, &home, uid, gid);
        Ok(())
    })
    .await
    .map_err(|_| "Credential installation worker failed")??;
    let _publication = run_gate.lock().await;
    transfer::authority(&execution, &command)?;
    if execution.processor.file_workspace(&command)? != workspace
        || tokio::time::Instant::now() >= authority_until
    {
        return Err("Credential authority expired before publication".into());
    }
    row["state"] = json!("active");
    execution
        .processor
        .store()
        .put(TABLE, &id, &row)
        .map_err(|e| e.to_string())?;
    execution
        .credential_deadlines
        .lock()
        .unwrap()
        .insert(id, authority_until);
    Ok(row)
}
async fn refresh(
    execution: Arc<Execution>,
    connection: Connection,
    command: Command,
    id: &str,
    mut row: Value,
) -> Result<Value, String> {
    let digest = command.digest()?;
    let prior_receipt = execution
        .processor
        .store()
        .get("runner_credential_commands", &command.id)
        .map_err(|e| e.to_string())?;
    if let Some(receipt) = &prior_receipt {
        if receipt["digest"] != digest {
            return Err("Credential command identity has different inputs".into());
        }
        if receipt["state"] == "completed" {
            return Ok(receipt["result"].clone());
        }
    }
    if row["state"] != "active" {
        return Err("Credential projection is not active".into());
    }
    let old: Grant =
        serde_json::from_value(row["grant"].clone()).map_err(|_| "Credential metadata invalid")?;
    matching(&old, &command)?;
    let node_root = root(&execution)?;
    let relative = PathBuf::from(
        row["home_relative"]
            .as_str()
            .ok_or("Credential home missing")?,
    );
    if relative.is_absolute()
        || relative
            .components()
            .any(|p| !matches!(p, std::path::Component::Normal(_)))
    {
        return Err("Credential home is outside node root".into());
    }
    let names: Vec<String> =
        serde_json::from_value(row["files"].clone()).map_err(|_| "Credential file list invalid")?;
    let home = node_root.join(relative);
    let saved = row.clone();
    let material = tokio::task::spawn_blocking(move || -> Result<Material, String> {
        let mut files = BTreeMap::new();
        let mut total = 0usize;
        for name in names {
            crate::runner_remote::transfers::validate_path(&name)?;
            let bytes = crate::private_files::read(&node_root, &home.join(&name), MAX_BYTES)?;
            total = total
                .checked_add(bytes.len())
                .ok_or("Credential size overflow")?;
            if total > MAX_BYTES {
                return Err("Refreshed credential exceeds its bound".into());
            }
            if crate::private_files::read(&node_root, &home.join(&name), MAX_BYTES)? != bytes {
                return Err("Credential changed during refresh snapshot".into());
            }
            files.insert(name, STANDARD.encode(bytes));
        }
        let material = Material {
            kind: saved["kind"]
                .as_str()
                .ok_or("Credential kind missing")?
                .into(),
            files,
            env_files: serde_json::from_value(saved["env_files"].clone())
                .map_err(|_| "Invalid credential bindings")?,
            env_dirs: serde_json::from_value(saved["env_dirs"].clone())
                .map_err(|_| "Invalid credential bindings")?,
        };
        material.validate()?;
        Ok(material)
    })
    .await
    .map_err(|_| "Credential refresh snapshot failed")??;
    transfer::authority(&execution, &command)?;
    let revision = material.revision()?;
    let expected = if let Some(receipt) = &prior_receipt {
        if receipt["candidate_revision"] != revision {
            return Err("Unconfirmed credential refresh has different local bytes; reconcile its original receipt".into());
        }
        receipt["expected_revision"]
            .as_str()
            .ok_or("Credential refresh intent missing")?
            .to_owned()
    } else {
        old.revision.clone()
    };
    execution.processor.store().document_scope(||->Result<(),String>{
        if let Some(prior)=execution.processor.store().get("runner_credential_commands",&command.id).map_err(|e|e.to_string())?{
            if prior["digest"]!=digest||prior["candidate_revision"]!=revision{return Err("Credential refresh identity conflict".into());}
        }else{
            execution.processor.store().put("runner_credential_commands",&command.id,&json!({"id":command.id,"grant_id":id,"digest":digest,"expected_revision":expected,"candidate_revision":revision,"state":"dispatching"})).map_err(|e|e.to_string())?;
        }Ok(())
    }).map_err(|e|e.to_string())??;
    let client = http(connection.ca_pem.as_deref()).map_err(|e| e.to_string())?;
    let url = endpoint(
        &connection.server,
        &format!("/v1/runner/credential-grants/{id}/refresh"),
    )
    .map_err(|e| e.to_string())?;
    let authorization =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&connection.lease).map_err(|e| e.to_string())?);
    let mut accepted = None;
    for _ in 0..3 {
        transfer::authority(&execution, &command)?;
        let requested = tokio::time::Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let response = client
                .post(url.clone())
                .bearer_auth(&authorization)
                .json(&json!({"expected_revision":expected,"material":material}))
                .send()
                .await
                .map_err(|_| "Credential refresh request failed")?;
            if !response.status().is_success() {
                return Err(
                    "Credential refresh refused; inspect revision or authorization".to_owned(),
                );
            }
            serde_json::from_slice::<Grant>(&transfer::bytes(response, 16 * 1024).await?)
                .map_err(|_| "Invalid credential refresh receipt".into())
        })
        .await;
        if let Ok(Ok(grant)) = result {
            accepted = Some((grant, requested));
            break;
        }
    }
    let (grant, validated_at) =
        accepted.ok_or("Credential refresh was not confirmed; reconcile the original revision")?;
    matching(&grant, &command)?;
    if grant.id != id || grant.revision != revision {
        return Err("Credential refresh receipt differs from submitted files".into());
    }
    let gate = execution.processor.gate(&command.run_id).await;
    let _guard = gate.lock().await;
    let result = execution
        .processor
        .store()
        .document_scope(|| -> Result<Value, String> {
            row = execution
                .processor
                .store()
                .get(TABLE, id)
                .map_err(|e| e.to_string())?
                .ok_or("Credential projection receipt missing")?;
            if row["grant"]["revision"] == expected || row["grant"]["revision"] == revision {
                row["grant"] = json!(grant);
                execution
                    .processor
                    .store()
                    .put(TABLE, id, &row)
                    .map_err(|e| e.to_string())?;
            }
            let mut result = row.clone();
            result["grant"] = json!(grant);
            let mut receipt = execution
                .processor
                .store()
                .get("runner_credential_commands", &command.id)
                .map_err(|e| e.to_string())?
                .ok_or("Credential refresh intent missing")?;
            receipt["state"] = json!("completed");
            receipt["result"] = result.clone();
            execution
                .processor
                .store()
                .put("runner_credential_commands", &command.id, &receipt)
                .map_err(|e| e.to_string())?;
            Ok(result)
        })
        .map_err(|e| e.to_string())??;
    if row["state"] == "active"
        && row["grant"]["revision"] == revision
        && transfer::authority(&execution, &command).is_ok()
    {
        execution
            .credential_deadlines
            .lock()
            .unwrap()
            .insert(id.into(), deadline(&grant, validated_at));
    }
    Ok(result)
}
const STATUS_BATCH_SIZE:usize=256;
const STATUS_BATCH_CONCURRENCY:usize=16;
const LEGACY_STATUS_CONCURRENCY:usize=64;
fn legacy_status_origins()->&'static std::sync::Mutex<std::collections::HashMap<String,tokio::time::Instant>>{
    static ORIGINS:std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String,tokio::time::Instant>>>=std::sync::OnceLock::new();
    ORIGINS.get_or_init(Default::default)
}
async fn status_observations(client:&reqwest::Client,server:&str,authorization:&str,ids:Vec<String>,slots:Arc<tokio::sync::Semaphore>)->Result<Vec<(Value,tokio::time::Instant)>,String>{
    let legacy=legacy_status_origins().lock().unwrap().get(server).is_some_and(|at|at.elapsed()<Duration::from_secs(60));
    if !legacy{
        let requested=tokio::time::Instant::now();
        let url=endpoint(server,"/v1/runner/credential-grants/status").map_err(|e|e.to_string())?;
        let response=tokio::time::timeout(Duration::from_secs(5),client.post(url).header("authorization",authorization).json(&json!({"ids":ids})).send()).await.map_err(|_|"Credential status batch timed out".to_owned())?.map_err(|e|e.to_string())?;
        if matches!(response.status().as_u16(),404|405){legacy_status_origins().lock().unwrap().insert(server.into(),tokio::time::Instant::now());}
        else{
            if !response.status().is_success(){return Err(format!("Credential status batch refused (HTTP {})",response.status()));}
            let bytes=tokio::time::timeout(Duration::from_secs(5).saturating_sub(requested.elapsed()),transfer::bytes(response,512*1024)).await.map_err(|_|"Credential status batch body timed out".to_owned())??;
            let value:Value=serde_json::from_slice(&bytes).map_err(|_|"Invalid credential status batch")?;
            return Ok(value["grants"].as_array().ok_or("Credential status batch omitted its grants")?.iter().cloned().map(|value|(value,requested)).collect());
        }
    }
    use futures_util::{stream,StreamExt};
    Ok(stream::iter(ids).map(|id|{let slots=slots.clone();async move{
        let _slot=slots.acquire_owned().await.map_err(|_|"Credential status scheduler closed".to_owned());
        let requested=tokio::time::Instant::now();
        let result=async{
            let _slot=_slot?;
            let url=endpoint(server,&format!("/v1/runner/credential-grants/{id}/status")).map_err(|e|e.to_string())?;
            tokio::time::timeout(Duration::from_secs(5),async{
                let response=client.get(url).header("authorization",authorization).send().await.map_err(|e|e.to_string())?;
                if !response.status().is_success(){return Err(format!("Credential status refused (HTTP {})",response.status()));}
                serde_json::from_slice::<Value>(&transfer::bytes(response,512*1024).await?).map_err(|_|"Invalid credential grant status".into())
            }).await.map_err(|_|"Credential status timed out".to_owned())?
        }.await;
        (match result{Ok(grant)=>json!({"id":id,"grant":grant}),Err(error)=>json!({"id":id,"error":error})},requested)
    }}).buffer_unordered(LEGACY_STATUS_CONCURRENCY).collect().await)
}
pub(super) async fn revalidate(execution: Arc<Execution>, connection: Connection) {
    let Ok(_batch)=execution.credential_revalidation.try_lock() else{return};
    let mut rows=match execution.processor.store().list(TABLE){Ok(rows)=>rows,Err(error)=>{tracing::error!(%error,"Credential renewals could not read their projections");return}};
    rows.retain(|row|row["state"]=="active");
    let now=tokio::time::Instant::now();
    {let deadlines=execution.credential_deadlines.lock().unwrap();
        rows.retain(|row|row["grant"]["id"].as_str().is_some_and(|id|deadlines.get(id).is_none_or(|at|*at<=now+Duration::from_secs(10))));
        rows.sort_by_key(|row|row["grant"]["id"].as_str().and_then(|id|deadlines.get(id).copied()).unwrap_or(now));
    }
    if rows.is_empty(){return;}
    let client=match http(connection.ca_pem.as_deref()){Ok(client)=>client,Err(error)=>{tracing::error!(%error,"Credential renewal transport unavailable");return}};
    let authorization=match serde_json::to_vec(&connection.lease){Ok(value)=>format!("Bearer {}",URL_SAFE_NO_PAD.encode(value)),Err(error)=>{tracing::error!(%error,"Credential renewal lease is invalid");return}};
    let legacy_slots=Arc::new(tokio::sync::Semaphore::new(LEGACY_STATUS_CONCURRENCY));
    let batches:Vec<_>=rows.chunks(STATUS_BATCH_SIZE).map(|rows|rows.to_vec()).collect();
    use futures_util::{StreamExt,stream};
    stream::iter(batches).map(|rows|{
        let(execution,client,authorization,server,legacy_slots)=(&execution,&client,&authorization,&connection.server,legacy_slots.clone());
        async move {
            let ids:Vec<_>=rows.iter().filter_map(|row|row["grant"]["id"].as_str().map(str::to_owned)).collect();
            let observed_at=chrono::Utc::now().to_rfc3339();
            let result=status_observations(client,server,authorization,ids,legacy_slots).await;
            for row in rows {
                let Some(id)=row["grant"]["id"].as_str()else{continue};
                let validation=(||->Result<(Grant,tokio::time::Instant),String>{
                    let command:Command=serde_json::from_value(row["command"].clone()).map_err(|_|"Invalid credential projection command")?;
                    transfer::authority(execution,&command)?;
                    let values=result.as_ref().map_err(Clone::clone)?;
                    let(value,requested)=values.iter().find(|(value,_)|value["id"]==id).ok_or("Credential status batch omitted an identity")?;
                    if let Some(error)=value["error"].as_str(){return Err(error.to_owned());}
                    let grant:Grant=serde_json::from_value(value["grant"].clone()).map_err(|_|"Invalid credential grant status")?;
                    matching(&grant,&command)?;
                    if grant.id!=id || row["grant"]["revision"]!=grant.revision{return Err("Credential grant revision changed".into());}
                    if tokio::time::Instant::now()>=deadline(&grant,*requested){return Err("Credential observation expired before acceptance".into());}
                    Ok((grant,*requested))
                })();
                let saved=execution.processor.store().document_scope(||->Result<(),String>{
                    let Some(mut current)=execution.processor.store().get(TABLE,id).map_err(|e|e.to_string())?else{return Ok(())};
                    if current["state"]!="active" || current["grant"]["revision"]!=row["grant"]["revision"] || current["command"]!=row["command"] {return Ok(())}
                    let command:Command=serde_json::from_value(current["command"].clone()).map_err(|_|"Invalid current credential command")?;
                    transfer::authority(execution,&command)?;
                    current["authority_observation"]=match &validation{Ok(_)=>json!({"state":"verified","checked_at":observed_at}),Err(error)=>json!({"state":"failed","checked_at":observed_at,"error":error})};
                    execution.processor.store().put(TABLE,id,&current).map_err(|e|e.to_string())?;
                    if let Ok((grant,requested))=&validation{execution.credential_deadlines.lock().unwrap().insert(id.into(),deadline(grant,*requested));}
                    Ok(())
                });
                if let Err(error)=validation{tracing::warn!(grant_id=id,%error,"Credential authority was not renewed");}
                if !matches!(saved,Ok(Ok(()))){tracing::error!(?saved,grant_id=id,"Credential renewal observation was not committed");}
            }
        }
    }).buffer_unordered(STATUS_BATCH_CONCURRENCY).collect::<Vec<_>>().await;
}

pub(super) fn authority_deadline(
    execution: &Execution,
    command: &Command,
    mut deadline: tokio::time::Instant,
) -> Result<tokio::time::Instant, String> {
    if !command.action.new_work() {
        return Ok(deadline);
    }
    for row in execution
        .processor
        .store()
        .list(TABLE)
        .map_err(|e| e.to_string())?
    {
        if row["grant"]["scope"]["run_id"] == command.run_id
            && row["grant"]["scope"]["generation"] == command.generation
            && row["state"] != "removed"
        {
            let id = row["grant"]["id"]
                .as_str()
                .ok_or("Invalid credential projection")?;
            let expires = execution
                .credential_deadlines
                .lock()
                .unwrap()
                .get(id)
                .copied()
                .ok_or("Credential has no current authority")?;
            deadline = deadline.min(expires);
        }
    }
    Ok(deadline)
}
pub(super) async fn enforce(execution: &Execution) -> Result<(), String> {
    let expired = execution
        .processor
        .store()
        .document_scope(|| -> Result<Vec<Value>, String> {
            let mut expired = Vec::new();
            for mut row in execution
                .processor
                .store()
                .list(TABLE)
                .map_err(|e| e.to_string())?
            {
                if row["state"] == "removed" {
                    continue;
                }
                let grant: Grant = serde_json::from_value(row["grant"].clone())
                    .map_err(|_| "Invalid credential projection record")?;
                let current = execution
                    .processor
                    .store()
                    .get("runner_remote_bindings", &grant.scope.run_id)
                    .map_err(|e| e.to_string())?;
                let stopped = current.as_ref().is_some_and(|r| r["state"] == "stopped");
                let expires = execution
                    .credential_deadlines
                    .lock()
                    .unwrap()
                    .get(&grant.id)
                    .copied();
                if !stopped
                    && expires.is_some_and(|deadline| tokio::time::Instant::now() < deadline)
                {
                    continue;
                }
                row["state"] = json!("revoked");
                execution
                    .processor
                    .store()
                    .put(TABLE, &grant.id, &row)
                    .map_err(|e| e.to_string())?;
                if let Some(mut binding) = current {
                    if binding["generation"] == grant.scope.generation
                        && binding["state"] != "stopped"
                    {
                        binding["state"] = json!("stopping");
                        binding["reason"] = json!("credential_authority_expired");
                        execution
                            .processor
                            .store()
                            .put("runner_remote_bindings", &grant.scope.run_id, &binding)
                            .map_err(|e| e.to_string())?;
                    }
                }
                expired.push(row);
            }
            Ok(expired)
        })
        .map_err(|e| e.to_string())??;
    use futures_util::{StreamExt, stream};
    let results = stream::iter(expired.into_iter().map(|row| async move {
        let grant: Grant = serde_json::from_value(row["grant"].clone())
            .map_err(|_| "Invalid credential projection record")?;
        execution
            .processor
            .stop_credential_run(&grant.scope.run_id, grant.scope.generation)
            .await?;
        let node_root = root(execution)?;
        let relative = PathBuf::from(
            row["home_relative"]
                .as_str()
                .ok_or("Credential home missing")?,
        );
        if relative.is_absolute()
            || relative
                .components()
                .any(|p| !matches!(p, std::path::Component::Normal(_)))
        {
            return Err("Invalid credential cleanup path".to_owned());
        }
        let path = node_root.join(relative);
        tokio::task::spawn_blocking(move || crate::private_files::remove_tree(&node_root, &path))
            .await
            .map_err(|_| "Credential cleanup worker failed")??;
        execution
            .processor
            .store()
            .document_scope(|| -> Result<(), String> {
                let mut current = execution
                    .processor
                    .store()
                    .get(TABLE, &grant.id)
                    .map_err(|e| e.to_string())?
                    .ok_or("Credential projection receipt missing")?;
                current["state"] = json!("removed");
                execution
                    .processor
                    .store()
                    .put(TABLE, &grant.id, &current)
                    .map_err(|e| e.to_string())
            })
            .map_err(|e| e.to_string())??;
        execution
            .credential_deadlines
            .lock()
            .unwrap()
            .remove(&grant.id);
        Ok(())
    }))
    .buffer_unordered(8)
    .collect::<Vec<Result<(), String>>>()
    .await;
    for result in results {
        result?;
    }
    Ok(())
}
