//! SRC connector file imports with opaque artifact and batch IDs.
use super::*;

const KEYS: &str = "src_connector_import_keys";
const HEADS: &str = "src_connector_import_heads";
const MODES: &str = "src_connector_import_modes";
pub(super) const APPLIED: &str = "src_connector_import_command_applied";

fn stored_bytes(s: &AppState, task_id: &str, batch: &Value) -> Api<Vec<u8>> {
    let reference = batch["source_refs"]
        .as_array()
        .and_then(|refs| refs.first())
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.artifact_missing",
                "connector source artifact is unavailable",
            )
        })?;
    let saved = verify_ref(s, task_id, reference)?;
    base64::engine::general_purpose::STANDARD
        .decode(saved["data"].as_str().unwrap_or(""))
        .map_err(|_| {
            Error::new(
                StatusCode::CONFLICT,
                "src.artifact_corrupt",
                "connector source artifact is unreadable",
            )
        })
}

fn same_source(
    s: &AppState,
    task_id: &str,
    batch: &Value,
    mode: &Value,
    connector: &str,
    config_version: &Value,
    mapping: &Value,
    data: &[u8],
) -> Api<bool> {
    if batch["connector_id"] != connector
        || batch["config_version"] != *config_version
        || mode["field_mapping"] != *mapping
    {
        return Ok(false);
    }
    Ok(stored_bytes(s, task_id, batch)? == data)
}

fn replay(mut batch: Value) -> Value {
    batch["deduplicated"] = json!(true);
    batch
}

pub(super) fn import(s: &AppState, task_id: &str, body: &Value) -> Api<Value> {
    import_inner(s, task_id, body, None)
}

pub(super) fn import_marked(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
) -> Api<Value> {
    import_inner(s, task_id, body, Some(command_id))
}

fn mark_replay(s: &AppState, command_id: Option<&str>, output: Value) -> Api<Value> {
    if let Some(command_id) = command_id {
        let marker = json!({"command_id":command_id,"output":output});
        s.store
            .put_if_absent(APPLIED, command_id, &marker)
            .map_err(super::db)?;
    }
    Ok(output)
}

fn import_inner(s: &AppState, task_id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    if let Some(command_id) = command_id {
        if let Some(marker) = s.store.get(APPLIED, command_id).map_err(super::db)? {
            return Ok(marker["output"].clone());
        }
    }
    let connector = require(body, "connector_id")?;
    let manifest = connector_manifest(connector)?;
    if !manifest["capabilities"]
        .as_array()
        .is_some_and(|capabilities| capabilities.iter().any(|item| item == "import_file"))
    {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability.unavailable",
            "connector does not support file import",
        ));
    }
    let config = connector_config(
        s,
        task_id,
        &json!({"connector_id":connector,"version":body["config_version"]}),
    )?;
    let encoded = body["content_b64"]
        .as_str()
        .or_else(|| body["data_b64"].as_str())
        .ok_or_else(|| Error::invalid("source file base64 required"))?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Error::invalid("invalid base64 import content"))?;
    let filename = body["filename"]
        .as_str()
        .or_else(|| body["source_metadata"]["filename"].as_str())
        .unwrap_or("source.bin");
    let mapping = &body["field_mapping"];
    let explicit_key = body["idempotency_key"]
        .as_str()
        .filter(|key| !key.is_empty());
    let index_key = explicit_key.map(|key| format!("{task_id}:{key}"));
    loop {
        let head = s.store.get(HEADS, task_id).map_err(super::db)?;
        if let Some(index_key) = index_key.as_deref() {
            if let Some(index) = s.store.get(KEYS, index_key).map_err(super::db)? {
                let batch =
                    rec(s, task_id, "connector_batch", require(&index, "batch_id")?)?["payload"]
                        .clone();
                if !same_source(
                    s,
                    task_id,
                    &batch,
                    &index,
                    connector,
                    &config["version"],
                    mapping,
                    &data,
                )? || batch["acquisition"]["filename"] != filename
                {
                    return Err(Error::conflict(
                        "connector import idempotency key used for different file",
                    ));
                }
                return mark_replay(s, command_id, replay(batch));
            }
        } else {
            for row in records(s, task_id, "connector_batch")? {
                let batch = &row["payload"];
                let Some(batch_id) = batch["batch_id"].as_str() else {
                    continue;
                };
                let mode = s
                    .store
                    .get(MODES, &format!("{task_id}:{batch_id}"))
                    .map_err(super::db)?;
                let Some(mode) = mode.filter(|mode| mode["mode"] == "default") else {
                    continue;
                };
                if same_source(
                    s,
                    task_id,
                    batch,
                    &mode,
                    connector,
                    &config["version"],
                    mapping,
                    &data,
                )? {
                    if batch["acquisition"]["filename"] != filename {
                        return Err(Error::conflict(
                            "connector import idempotency key used for different filename",
                        ));
                    }
                    return mark_replay(s, command_id, replay(batch.clone()));
                }
            }
        }

        let _source_metadata = json!({"filename":filename,"scheme":body["source_metadata"]["scheme"],
            "created_by":body["created_by"].as_str().unwrap_or("local-operator")});
        let parsed = connector_source::parse(&data, connector, filename, mapping)?;
        let batch_id = format!("sconnimport_{}", Uuid::new_v4().simple());
        let artifact_id = format!("sart_{}", Uuid::new_v4().simple());
        let artifact_name = std::path::Path::new(filename)
            .file_name()
            .and_then(|part| part.to_str())
            .unwrap_or("source.bin");
        let artifact = json!({"artifact_id":artifact_id,"version":1,"task_id":task_id,"size":data.len(),
            "name":artifact_name,"media_type":parsed.media_type,
            "data":base64::engine::general_purpose::STANDARD.encode(&data),"created_at":now()});
        let source_ref = json!({"artifact_id":artifact_id,"version":1,"task_id":task_id,"size":data.len(),
            "name":artifact_name,"media_type":parsed.media_type,"original":true,"complete":"complete"});
        let items=parsed.items;
        let errors = parsed.failures;
        let error_count = errors.len();
        let state = if errors.is_empty() {
            "completed"
        } else if items.is_empty() {
            "failed"
        } else {
            "partial"
        };
        let batch = json!({"schema":"muteki.src.connector-batch.v1","batch_id":batch_id,
            "connector_id":connector,"config_version":config["version"],"task_id":task_id,"product_mode":"src",
            "items":items,"source_refs":[source_ref],"errors":errors,"unknown_fields":[],
            "field_mapping":parsed.mapping,
            "acquisition":{"filename":filename,"complete":parsed.complete,"truncated":false},
            "counts":{"processed":items.len(),"new":0,"updated":0,"merged":0,"skipped":0,
                "failed":error_count,"truncated":0},"cost":{"state":"not_applicable"},
            "external_state":state,"deduplicated":false});
        let (record_key, _, record) = stage_rec(
            s,
            task_id,
            "connector_batch",
            &batch_id,
            batch.clone(),
            Some(0),
        )?;
        let record_doc = json!({"version":1,"values":record});
        let artifact_key = format!("{task_id}:{artifact_id}");
        let mode_key = format!("{task_id}:{batch_id}");
        let mode = json!({"mode":if explicit_key.is_some(){"explicit"}else{"default"},
            "batch_id":batch_id,"connector_id":connector,"config_version":config["version"],
            "field_mapping":mapping});
        let next_head = json!({"version":head.as_ref().and_then(|value|value["version"].as_u64()).unwrap_or(0)+1});
        let mut conditions = vec![
            (HEADS, task_id, head.as_ref()),
            (RECORDS, record_key.as_str(), None),
            ("src_artifacts", artifact_key.as_str(), None),
            (MODES, mode_key.as_str(), None),
        ];
        let mut writes = vec![
            (HEADS, task_id, &next_head),
            (RECORDS, record_key.as_str(), &record_doc),
            ("src_artifacts", artifact_key.as_str(), &artifact),
            (MODES, mode_key.as_str(), &mode),
        ];
        let index = index_key.as_ref().map(|_| {
            json!({"batch_id":batch_id,"connector_id":connector,
            "config_version":config["version"],"field_mapping":mapping})
        });
        if let (Some(index_key), Some(index)) = (index_key.as_deref(), index.as_ref()) {
            conditions.push((KEYS, index_key, None));
            writes.push((KEYS, index_key, index));
        }
        let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":batch}));
        if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
            conditions.push((APPLIED, command_id, None));
            writes.push((APPLIED, command_id, marker));
        }
        if s.store
            .put_many_if(&conditions, &writes)
            .map_err(super::db)?
        {
            return Ok(batch);
        }
    }
}
