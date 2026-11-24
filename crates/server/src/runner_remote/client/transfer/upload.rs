use super::*;

async fn request(
    execution: &Execution,
    command: &Command,
    request: reqwest::RequestBuilder,
) -> Result<Value, String> {
    for attempt in 0..5 {
        authority(execution, command)?;
        let reply = request
            .try_clone()
            .ok_or("Artifact request is not repeatable")?
            .send()
            .await;
        match reply {
            Ok(reply) if reply.status().is_success() => {
                if let Ok(bytes) = bytes(reply, 16 * 1024).await {
                    if let Ok(value) = serde_json::from_slice(&bytes) {
                        return Ok(value);
                    }
                }
            }
            Ok(reply) if reply.status().as_u16() != 429 && !reply.status().is_server_error() => {
                return Err(format!("Artifact upload refused (HTTP {})", reply.status()));
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(200 * (1 << attempt))).await;
    }
    Err("Artifact upload unavailable; snapshot and acknowledged cursor were retained".into())
}

pub(super) async fn run(
    execution: Arc<Execution>,
    connection: &Connection,
    command: &Command,
    id: &str,
) -> Result<Value, String> {
    let Operation::TransferPush {
        path, max_bytes, ..
    } = &command.action
    else {
        return Err("Missing artifact request".into());
    };
    let current = authority(&execution, command)?;
    let scope = execution.processor.file_scope(command, &current, false)?;
    let workspace = execution.processor.file_workspace(command)?;
    let storage = {
        let execution = execution.clone();
        let root = connection.root.clone();
        Arc::new(disk(move || storage(&execution, &root)).await?)
    };
    let manifest = {
        let storage = storage.clone();
        let execution = execution.clone();
        let command = command.clone();
        let id = id.to_owned();
        let scope = scope.clone();
        let path = path.clone();
        let max_bytes = *max_bytes;
        disk(move || {
            storage.snapshot(&id, &scope, &workspace, &path, max_bytes, || {
                authority(&execution, &command)?;
                if execution.processor.file_workspace(&command)? != workspace {
                    return Err("Artifact workspace changed".into());
                }
                Ok(())
            })
        })
        .await?
    };
    let client = http(connection.ca_pem.as_deref()).map_err(|e| e.to_string())?;
    let authorization = format!(
        "Bearer {}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&connection.lease).map_err(|e| e.to_string())?)
    );
    let begin = endpoint(&connection.server, "/v1/runner/artifacts").map_err(|e| e.to_string())?;
    let row = request(
        &execution,
        command,
        client
            .post(begin)
            .header("authorization", &authorization)
            .json(&manifest),
    )
    .await?;
    if row["manifest"] != json!(manifest) {
        return Err("Controller artifact manifest conflicts".into());
    }
    let mut offset = row["received"]
        .as_u64()
        .filter(|n| *n <= manifest.size)
        .ok_or("Invalid artifact cursor")?;
    let mut job = execution
        .processor
        .store()
        .get(JOBS, id)
        .map_err(|e| e.to_string())?
        .ok_or("Artifact job missing")?;
    job["state"] = json!("sending");
    job["manifest"] = json!(manifest);
    job["received"] = json!(offset);
    execution
        .processor
        .store()
        .put(JOBS, id, &job)
        .map_err(|e| e.to_string())?;
    while offset < manifest.size {
        authority(&execution, command)?;
        let data = {
            let storage = storage.clone();
            let scope = scope.clone();
            let id = id.to_owned();
            disk(move || storage.read_chunk(&id, &scope, offset, CHUNK_BYTES)).await?
        };
        let length = data.len() as u64;
        if length == 0 {
            return Err("Empty artifact chunk before EOF".into());
        }
        let hash = crate::runner_remote::transfers::digest(&data);
        let mut url = endpoint(
            &connection.server,
            &format!("/v1/runner/artifacts/{id}/chunks"),
        )
        .map_err(|e| e.to_string())?;
        url.query_pairs_mut()
            .append_pair("offset", &offset.to_string());
        let saved = request(
            &execution,
            command,
            client
                .put(url)
                .header("authorization", &authorization)
                .header("x-muteki-sha256", hash)
                .body(data),
        )
        .await?;
        let received = saved["received"]
            .as_u64()
            .ok_or("Missing artifact receipt cursor")?;
        if received != offset + length || saved["manifest"] != json!(manifest) {
            return Err("Artifact acknowledgement does not match uploaded bytes".into());
        }
        offset = received;
        job["received"] = json!(offset);
        execution
            .processor
            .store()
            .put(JOBS, id, &job)
            .map_err(|e| e.to_string())?;
    }
    let seal = endpoint(
        &connection.server,
        &format!("/v1/runner/artifacts/{id}/seal"),
    )
    .map_err(|e| e.to_string())?;
    let receipt = request(
        &execution,
        command,
        client.post(seal).header("authorization", &authorization),
    )
    .await?;
    if receipt["manifest"] != json!(manifest) || receipt["state"] != "verified" {
        return Err("Controller did not verify the complete artifact".into());
    }
    Ok(receipt)
}
