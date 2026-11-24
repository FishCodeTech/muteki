//! Inbound connector and ingestion events: durable data, never an implicit Run.
use super::{Api, Error, ingest, now, put_artifact, rec, records, require, save_rec};
use crate::AppState;
use axum::http::StatusCode;
use base64::Engine;
use serde_json::{Value, json};

fn event_digest(value: &Value) -> String {
    crate::runs::src_api::digest(value)
}
fn existing(
    s: &AppState,
    id: &str,
    kind: &str,
    event_id: &str,
    digest: &str,
) -> Api<Option<Value>> {
    let prior = records(s, id, kind)?
        .into_iter()
        .find(|v| v["natural_key"] == event_id);
    if prior
        .as_ref()
        .is_some_and(|v| v["payload"]["event_digest"] != digest)
    {
        return Err(Error::conflict(
            "external event ID already refers to different data",
        ));
    }
    Ok(prior.map(|v| v["payload"].clone()))
}
pub(super) fn ingestion_event(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let event = body.get("event").filter(|v| v.is_object()).unwrap_or(body);
    let event_id = event["event_id"]
        .as_str()
        .or_else(|| event["id"].as_str())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            Error::new(
                StatusCode::BAD_REQUEST,
                "external_event_invalid",
                "external event id is required",
            )
        })?;
    let digest = event_digest(event);
    if let Some(prior) = existing(s, id, "external_event", event_id, &digest)? {
        return Ok(
            json!({"event_id":event_id,"artifact":prior["artifact"],"batch":prior["batch"],
            "executed":false,"run_created":false,"deduplicated":true}),
        );
    }
    let raw = serde_json::to_vec(event).map_err(crate::runs::src_api::db)?;
    let artifact = put_artifact(
        s,
        id,
        &raw,
        &format!("external-event-{event_id}.json"),
        "application/json",
    )?;
    let batch = if let Some(assets) = event["assets"].as_array().filter(|v| !v.is_empty()) {
        let content =
            serde_json::to_vec(&json!({"items":assets})).map_err(crate::runs::src_api::db)?;
        ingest(
            s,
            id,
            &json!({"source_kind":"external_event",
            "source_id":event["source_id"].as_str().unwrap_or(event_id),
            "content_b64":base64::engine::general_purpose::STANDARD.encode(content),
            "source_metadata":{"filename":format!("external-event-{event_id}-assets.json"),"event":event},
            "observed_at":event["observed_at"]}),
        )?
    } else {
        Value::Null
    };
    let record = json!({"event_id":event_id,"task_id":id,"event_digest":digest,
        "source_version":event["source_version"],"target_refs":event["target_refs"].as_array().cloned().unwrap_or_default(),
        "artifact":artifact,"received_at":now(),"payload":event,"batch":batch});
    save_rec(s, id, "external_event", event_id, record.clone(), Some(0))?;
    let _ = super::coordination::enqueue_wakeup(
        s,
        id,
        &format!("external_event:{event_id}"),
        "external_event",
        &format!("收到外部事件 `{event_id}`"),
        event["summary"]
            .as_str()
            .or_else(|| event["title"].as_str())
            .unwrap_or("外部资产或情报事件入库"),
        json!({
            "event_id": event_id,
            "source_id": event["source_id"],
            "source_version": event["source_version"],
            "target_refs": event["target_refs"],
            "artifact": artifact
        }),
    );
    Ok(
        json!({"event_id":event_id,"artifact":artifact,"batch":batch,
        "executed":false,"run_created":false}),
    )
}
pub(super) fn connector_event(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let source_id = require(body, "source_id")?;
    let event_id = require(body, "event_id")?;
    let source_version = require(body, "source_version")?;
    let payload = body["payload"]
        .as_object()
        .map(|_| body["payload"].clone())
        .ok_or_else(|| Error::invalid("external connector payload must be an object"))?;
    let target_ref = body["target_ref"].as_str().unwrap_or("");
    let request = json!({"source_id":source_id,"event_id":event_id,"source_version":source_version,"target_ref":target_ref});
    let digest = event_digest(&request);
    if let Some(mut prior) = existing(s, id, "connector_batch", event_id, &digest)? {
        prior["deduplicated"] = json!(true);
        return Ok(prior);
    }
    let raw = serde_json::to_vec(&payload).map_err(crate::runs::src_api::db)?;
    let source_ref = put_artifact(
        s,
        id,
        &raw,
        &format!("{source_id}-{event_id}.json"),
        "application/json",
    )?;
    let batch_id = format!("sbatch_ext_{}", &event_digest(&request)[..24]);
    let mut value = json!({"schema":"muteki.src.connector-batch.v1","batch_id":batch_id,"connector_id":"external_event",
        "config_version":0,"task_id":id,"event_id":event_id,"event_digest":digest,
        "items":[{"input_key":format!("{source_id}:{event_id}"),"mapped":{"type":"external_observation",
            "target_ref":target_ref,"authorization_expanded":false},"raw":payload}],
        "source_refs":[source_ref],"acquisition":{"source_id":source_id,"event_id":event_id,
            "source_version":source_version,"complete":true,"truncated":false},
        "cost":{"state":"not_applicable"},"external_state":"completed","status":"completed",
        "outcome":"completed","reconciliation_state":"not_applicable","created_at":now()});
    if let Ok(ingested) = super::ingestion::ingest_connector_batch_observations(s, id, &value, None) {
        value["observation_counts"] = ingested["counts"].clone();
        value["observations_ingested"] = json!(true);
    }
    save_rec(s, id, "connector_batch", event_id, value.clone(), Some(0))?;
    let _ = super::coordination::enqueue_wakeup(
        s,
        id,
        &format!("connector_event:{source_id}:{event_id}"),
        "external_event",
        &format!("连接器 `{source_id}` 产生外部事件 `{event_id}`"),
        payload["summary"]
            .as_str()
            .or_else(|| payload["title"].as_str())
            .unwrap_or("连接器外部观测入库"),
        json!({
            "event_id": event_id,
            "source_id": source_id,
            "source_version": source_version,
            "target_refs": if target_ref.is_empty() { json!([]) } else { json!([target_ref]) },
            "batch_id": batch_id
        }),
    );
    Ok(value)
}
pub(super) fn reconcile_ingestion(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let batch_id = require(body, "batch_id")?;
    let receipt = body["receipt"]
        .as_object()
        .map(|_| body["receipt"].clone())
        .ok_or_else(|| Error::invalid("external receipt must be an object"))?;
    let row = rec(s, id, "import_batch", batch_id)
        .or_else(|_| rec(s, id, "connector_batch", batch_id))?;
    let kind = row["kind"].as_str().unwrap_or("import_batch").to_owned();
    let mut batch = row["payload"].clone();
    let mut cost = receipt["cost"]
        .as_object()
        .map(|_| receipt["cost"].clone())
        .unwrap_or_else(|| json!({}));
    cost["state"] = json!(if cost["state"] == "unknown" {
        "unknown"
    } else {
        "confirmed"
    });
    batch["upstream_receipt"] = receipt;
    batch["cost"] = cost.clone();
    if kind == "connector_batch" {
        batch["reconciliation_state"] = json!(if cost["state"] == "confirmed" {
            "reconciled"
        } else {
            "unreconciled"
        });
        if batch["observations_ingested"] != true
            && let Ok(ingested) = super::ingestion::ingest_connector_batch_observations(s, id, &batch, None)
        {
            batch["observation_counts"] = ingested["counts"].clone();
            batch["observations_ingested"] = json!(true);
        }
    } else {
        batch["status"] = json!(if cost["state"] == "confirmed" {
            "reconciled"
        } else {
            "external_unknown"
        });
    }
    save_rec(
        s,
        id,
        &kind,
        row["natural_key"].as_str().filter(|v| !v.is_empty()).unwrap_or(batch_id),
        batch.clone(),
        row["revision"].as_u64(),
    )?;
    Ok(batch)
}
