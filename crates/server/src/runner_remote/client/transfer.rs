mod upload;
use super::{Execution, endpoint, http};
use crate::runner_remote::{
    commands::{Authority, Command, Operation},
    identity::Lease,
    transfers::{CHUNK_BYTES, Manifest, Scope, Transfers},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Weak},
    time::Duration,
};

const JOBS: &str = "runner_transfer_jobs";
pub(super) struct Connection {
    pub server: String,
    pub ca_pem: Option<String>,
    pub lease: Lease,
    pub root: PathBuf,
}
struct Active {
    execution: Weak<Execution>,
    id: String,
}
impl Drop for Active {
    fn drop(&mut self) {
        if let Some(execution) = self.execution.upgrade() {
            execution.file_jobs.lock().unwrap().remove(&self.id);
        }
    }
}
fn id_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
pub(super) fn authority(execution: &Execution, command: &Command) -> Result<Authority, String> {
    let scope = execution
        .connection_scope
        .lock()
        .unwrap()
        .clone()
        .ok_or("No authenticated connection")?;
    if scope.node_id != command.node_id
        || scope.generation != command.session_generation
        || scope.credential_version != command.credential_version
    {
        return Err("Transfer belongs to an obsolete node session".into());
    }
    let authority = Authority {
        node_id: scope.node_id,
        session_generation: scope.generation,
        credential_version: scope.credential_version,
        projects: scope.projects,
        valid_until: *execution.deadline.lock().unwrap(),
    };
    execution.processor.file_scope(command, &authority, false)?;
    Ok(authority)
}
fn storage(execution: &Execution, root: &std::path::Path) -> Result<Transfers, String> {
    Transfers::open(
        execution.processor.store().clone(),
        root,
        4 * 1024 * 1024 * 1024,
    )
}
pub(super) fn reconcile(store: &crate::store::Store) -> Result<(), String> {
    for mut row in store.list(JOBS).map_err(|e| e.to_string())? {
        if matches!(
            row["state"].as_str(),
            Some("queued" | "receiving" | "sending")
        ) {
            row["state"] = json!("interrupted");
            row["error"] =
                json!("Runner restarted; reconcile the execution before resuming this transfer");
            store
                .put(
                    JOBS,
                    row["id"].as_str().ok_or("Transfer job identity missing")?,
                    &row,
                )
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
pub(super) async fn dispatch(
    execution: Arc<Execution>,
    connection: Connection,
    command: Command,
    initial: Authority,
) -> Result<Value, String> {
    let (id, pull) = match &command.action {
        Operation::TransferPull { transfer_id } | Operation::TransferPush { transfer_id, .. } => {
            (transfer_id.clone(), true)
        }
        Operation::TransferStatus { transfer_id } => (transfer_id.clone(), false),
        _ => return Err("Not a transfer action".into()),
    };
    if !id_valid(&id) {
        return Err("Invalid transfer identifier".into());
    }
    let gate = execution.processor.gate(&command.run_id).await;
    let _guard = gate.lock().await;
    let scope = execution.processor.file_scope(&command, &initial, true)?;
    let store = execution.processor.store();
    let prior = store.get(JOBS, &id).map_err(|e| e.to_string())?;
    if let Some(row) = &prior {
        if row["scope"] != json!(scope) {
            return Err("Transfer belongs to another execution scope".into());
        }
    }
    if !pull {
        return prior.ok_or("Transfer job is unknown".into());
    }
    let digest = command.digest()?;
    if let Some(row) = &prior {
        if row["digest"] != digest {
            return Err("Transfer job has a different accepted policy".into());
        }
        if row["state"] == "committed" {
            return Ok(row.clone());
        }
    }
    let mut active = execution.file_jobs.lock().unwrap();
    if active.contains(&id) {
        return prior.ok_or("Live transfer has no durable job record".into());
    }
    let direction = if matches!(command.action, Operation::TransferPush { .. }) {
        "output"
    } else {
        "input"
    };
    let row = json!({"direction":direction,"id":id,"scope":scope,"digest":digest,"command":command,"state":"queued","accepted_at":chrono::Utc::now().to_rfc3339()});
    store
        .document_scope(|| -> Result<(), String> {
            if store
                .list(JOBS)
                .map_err(|e| e.to_string())?
                .iter()
                .filter(|r| {
                    matches!(
                        r["state"].as_str(),
                        Some("queued" | "receiving" | "sending")
                    )
                })
                .count()
                >= 256
            {
                return Err("Node transfer queue is full".into());
            }
            store.put(JOBS, &id, &row).map_err(|e| e.to_string())
        })
        .map_err(|e| e.to_string())??;
    active.insert(id.clone());
    drop(active);
    tokio::spawn(async move {
        let _active = Active {
            execution: Arc::downgrade(&execution),
            id: id.clone(),
        };
        let permit = execution.files.clone().acquire_owned().await;
        let result = match permit {
            Ok(_permit) => {
                if matches!(command.action, Operation::TransferPush { .. }) {
                    upload::run(execution.clone(), &connection, &command, &id).await
                } else {
                    download(execution.clone(), &connection, &command, &id).await
                }
            }
            Err(_) => Err("Transfer queue closed".into()),
        };
        let mut finished = match execution.processor.store().get(JOBS, &id) {
            Ok(Some(row)) => row,
            _ => {
                tracing::error!("Transfer job outcome could not be read for settlement");
                return;
            }
        };
        finished["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
        match result {
            Ok(receipt) => {
                finished["state"] = json!("committed");
                finished["receipt"] = receipt;
            }
            Err(error) => {
                finished["state"] = json!("failed");
                finished["error"] = json!(error);
            }
        }
        if let Err(error) = execution.processor.store().put(JOBS, &id, &finished) {
            tracing::error!(%error,"Could not persist transfer job outcome");
        }
    });
    Ok(row)
}
pub(super) async fn bytes(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err("Transfer response exceeds its bound".into());
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if chunk.len() > limit.saturating_sub(data.len()) {
            return Err("Transfer response exceeds its bound".into());
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}
pub(super) async fn get(
    execution: &Execution,
    connection: &Connection,
    command: &Command,
    client: &reqwest::Client,
    path: &str,
) -> Result<reqwest::Response, String> {
    let authorization = format!(
        "Bearer {}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&connection.lease).map_err(|e| e.to_string())?)
    );
    let (path, query) = path
        .split_once('?')
        .map(|(path, query)| (path, Some(query)))
        .unwrap_or((path, None));
    let mut url = endpoint(&connection.server, path).map_err(|e| e.to_string())?;
    url.set_query(query);
    for attempt in 0..5 {
        authority(execution, command)?;
        let response = client
            .get(url.clone())
            .header("authorization", &authorization)
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response)
                if response.status().as_u16() != 429 && !response.status().is_server_error() =>
            {
                return Err(format!(
                    "File source refused access (HTTP {})",
                    response.status()
                ));
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
    }
    Err("File source unavailable; durable transfer progress was retained".into())
}
async fn source_manifest(
    execution: &Execution,
    connection: &Connection,
    command: &Command,
    client: &reqwest::Client,
    id: &str,
    scope: &Scope,
) -> Result<Manifest, String> {
    let response = get(
        execution,
        connection,
        command,
        client,
        &format!("/v1/runner/transfers/{id}"),
    )
    .await?;
    let mut row: Value =
        serde_json::from_slice(&bytes(response, 16 * 1024).await?).map_err(|e| e.to_string())?;
    if row["state"] != "verified" {
        return Err("File source is no longer verified and available".into());
    }
    let manifest: Manifest =
        serde_json::from_value(row["manifest"].take()).map_err(|e| e.to_string())?;
    if manifest.id != id || manifest.scope != *scope {
        return Err("File source manifest does not match the authorized Run".into());
    }
    Ok(manifest)
}
async fn piece(
    execution: &Execution,
    connection: &Connection,
    command: &Command,
    client: &reqwest::Client,
    path: &str,
    offset: u64,
    size: u64,
) -> Result<(Vec<u8>, String), String> {
    for attempt in 0..3 {
        let response = get(execution, connection, command, client, path).await?;
        let hash = response
            .headers()
            .get("x-muteki-sha256")
            .and_then(|h| h.to_str().ok())
            .ok_or("Chunk digest missing")?
            .to_owned();
        let response_offset = response
            .headers()
            .get("x-muteki-offset")
            .and_then(|h| h.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let total = response
            .headers()
            .get("x-muteki-total-bytes")
            .and_then(|h| h.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if response_offset != Some(offset) || total != Some(size) {
            return Err("Chunk cursor or declared size mismatch".into());
        }
        match bytes(response, CHUNK_BYTES).await {
            Ok(data) => return Ok((data, hash)),
            Err(error) if attempt == 2 => return Err(error),
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
    Err("Chunk body could not be received; acknowledged progress was retained".into())
}
async fn disk<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
}
async fn download(
    execution: Arc<Execution>,
    connection: &Connection,
    command: &Command,
    id: &str,
) -> Result<Value, String> {
    let current = authority(&execution, command)?;
    let scope = execution.processor.file_scope(command, &current, false)?;
    let client = http(connection.ca_pem.as_deref()).map_err(|e| e.to_string())?;
    let manifest = source_manifest(&execution, connection, command, &client, id, &scope).await?;
    let storage = {
        let execution = execution.clone();
        let root = connection.root.clone();
        Arc::new(disk(move || storage(&execution, &root)).await?)
    };
    let starting = {
        let storage = storage.clone();
        let manifest = manifest.clone();
        disk(move || storage.begin(&manifest)).await?
    };
    if starting["state"] == "committed" {
        return Ok(starting);
    }
    let mut offset = starting["received"]
        .as_u64()
        .ok_or("Transfer cursor missing")?;
    let mut job = execution
        .processor
        .store()
        .get(JOBS, id)
        .map_err(|e| e.to_string())?
        .ok_or("Job record missing")?;
    job["state"] = json!("receiving");
    job["manifest"] = json!(manifest);
    job["received"] = json!(offset);
    execution
        .processor
        .store()
        .put(JOBS, id, &job)
        .map_err(|e| e.to_string())?;
    while offset < manifest.size {
        let mut url = endpoint(
            &connection.server,
            &format!("/v1/runner/transfers/{id}/chunks"),
        )
        .map_err(|e| e.to_string())?;
        url.query_pairs_mut()
            .append_pair("offset", &offset.to_string())
            .append_pair("length", &CHUNK_BYTES.to_string());
        let path = format!("{}?{}", url.path(), url.query().unwrap());
        let (data, hash) = piece(
            &execution,
            connection,
            command,
            &client,
            &path,
            offset,
            manifest.size,
        )
        .await?;
        authority(&execution, command)?;
        let saved = {
            let storage = storage.clone();
            let scope = scope.clone();
            let id = id.to_owned();
            disk(move || storage.chunk(&id, &scope, offset, &data, &hash)).await?
        };
        offset = saved["received"]
            .as_u64()
            .ok_or("Missing acknowledged cursor")?;
        job["received"] = json!(offset);
        execution
            .processor
            .store()
            .put(JOBS, id, &job)
            .map_err(|e| e.to_string())?;
    }
    if source_manifest(&execution, connection, command, &client, id, &scope).await? != manifest {
        return Err("Source manifest changed before commit".into());
    }
    let gate = execution.processor.gate(&command.run_id).await;
    let workspace = execution.processor.file_workspace(command)?;
    let id = id.to_owned();
    let command = command.clone();
    disk(move || {
        storage.commit_guarded(&id, &scope, &workspace, || {
            let guard = gate.clone().blocking_lock_owned();
            let current = authority(&execution, &command)?;
            execution.processor.file_scope(&command, &current, false)?;
            if execution.processor.file_workspace(&command)? != workspace {
                return Err("Workspace changed before file publication".into());
            }
            Ok(guard)
        })
    })
    .await
}
