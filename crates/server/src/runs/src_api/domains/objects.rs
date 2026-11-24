//! Task-scoped business objects and fenced mutation leases.
use super::{Api, Error, now, put_artifact, rec, records, require, save_rec, task, verify_ref};
use crate::AppState;
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

const LEASES: &str = "src_resource_leases";

pub(super) fn participant(s: &AppState, id: &str, context: &str, actor: &Value) -> Api<String> {
    let t = task(s, id)?;
    let thread = actor["thread_id"].as_str().filter(|v| !v.is_empty());
    let run = actor["run_id"].as_str().filter(|v| !v.is_empty());
    if thread.is_some() && run.is_some() {
        return Err(Error::invalid("participant requires one Thread or Run"));
    }
    let source = if let Some(run_id) = run {
        if !records(s, id, "run_binding")?
            .iter()
            .any(|v| v["payload"]["run_id"] == run_id)
            || s.store
                .get("runs", run_id)
                .map_err(crate::runs::src_api::db)?
                .is_none_or(|v| v["src_task_id"] != id)
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.run_not_bound",
                "participant Run is outside this Task",
            ));
        }
        run_id
    } else {
        let expected = t["binding"]["thread_id"].as_str().unwrap_or("");
        if thread.is_some_and(|v| v != expected) {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.thread_not_bound",
                "participant Thread is outside this Task",
            ));
        }
        expected
    };
    let worker = actor["worker_id"].as_str().unwrap_or("");
    if run.is_none() && !worker.is_empty() {
        return Err(Error::invalid("worker participant requires a bound Run"));
    }
    Ok(format!("{id}:{context}:{source}:{worker}"))
}
fn lease_key(id: &str, context: &str, kind: &str, resource: &str) -> String {
    if kind == "src.business_object" {
        format!("{id}:{resource}")
    } else {
        format!("{id}:{context}:{resource}")
    }
}
fn store_lease_key(kind: &str, resource_key: &str) -> String {
    format!("{kind}\0{resource_key}")
}
fn active(lease: &Value) -> bool {
    lease["released_at"].is_null()
        && lease["expires_at"].as_str().is_some_and(|v| {
            chrono::DateTime::parse_from_rfc3339(v)
                .ok()
                .is_some_and(|t| t > chrono::Utc::now())
        })
}
fn lease_input<'a>(body: &'a Value) -> Api<(&'a str, &'a str, &'a str)> {
    let context = require(body, "context_id")?;
    let kind = body["resource_kind"]
        .as_str()
        .unwrap_or("src.business_object");
    let resource = require(body, "resource_id")?;
    if !kind.starts_with("src.") {
        return Err(Error::invalid("invalid SRC lease resource kind"));
    }
    Ok((context, kind, resource))
}
pub(super) fn acquire(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    if task(s, id)?["lifecycle"]["state"] != "active" {
        return Err(Error::conflict("SRC Task is not active"));
    }
    let (context, kind, resource) = lease_input(body)?;
    let owner = participant(s, id, context, &body["actor"])?;
    if body["owner"].as_str().is_some_and(|v| v != owner) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.task_binding_invalid",
            "lease owner does not match bound participant",
        ));
    }
    let key = lease_key(id, context, kind, resource);
    let store_key = store_lease_key(kind, &key);
    let previous = s
        .store
        .get(LEASES, &store_key)
        .map_err(crate::runs::src_api::db)?;
    let version = previous
        .as_ref()
        .and_then(|v| v["version"].as_u64())
        .unwrap_or(0);
    if previous.as_ref().is_some_and(|v| active(&v["values"])&&v["values"]["owner"]!=owner) {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "resource_busy",
            "resource is leased by another participant",
        ));
    }
    let token = previous
        .as_ref()
        .and_then(|v| v["values"]["fencing_token"].as_u64())
        .unwrap_or(0)
        + 1;
    let ttl = body["ttl_seconds"].as_i64().unwrap_or(600).clamp(1, 86400) as u64;
    let at = chrono::Utc::now();
    let value = json!({"schema_version":1,"resource_kind":kind,"resource_key":key,
        "owner":owner,"scope":format!("src:{id}"),"ttl_seconds":ttl,
        "fencing_token":token,"acquired_at":at.to_rfc3339(),"expires_at":(at+chrono::Duration::seconds(ttl as i64)).to_rfc3339(),"released_at":null});
    match s
        .store
        .compare_and_put(LEASES, &store_key, version, value.clone())
        .map_err(crate::runs::src_api::db)?
    {
        Ok(_) => Ok(value),
        Err(_) => Err(Error::new(
            StatusCode::CONFLICT,
            "resource_busy",
            "resource lease changed during acquisition",
        )),
    }
}
pub(super) fn assert_lease(
    s: &AppState,
    id: &str,
    context: &str,
    kind: &str,
    resource: &str,
    owner: &str,
    token: u64,
) -> Api<Value> {
    let key = store_lease_key(kind, &lease_key(id, context, kind, resource));
    let row = s
        .store
        .get(LEASES, &key)
        .map_err(crate::runs::src_api::db)?
        .ok_or_else(|| {
            Error::new(
                StatusCode::CONFLICT,
                "resource_busy",
                "resource lease missing",
            )
        })?;
    let lease = &row["values"];
    if !active(lease)
        || lease["scope"] != format!("src:{id}")
        || lease["owner"] != owner
        || lease["fencing_token"] != token
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "resource_busy",
            "resource lease is expired, released or fenced",
        ));
    }
    Ok(row)
}
pub(super) fn release(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let (context, kind, resource) = lease_input(body)?;
    let owner = participant(s, id, context, &body["actor"])?;
    if body["owner"].as_str().is_some_and(|v| v != owner) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.task_binding_invalid",
            "lease owner does not match bound participant",
        ));
    }
    let token = body["fencing_token"]
        .as_u64()
        .ok_or_else(|| Error::invalid("fencing_token required"))?;
    let row = assert_lease(s, id, context, kind, resource, &owner, token)?;
    let mut value = row["values"].clone();
    value["released_at"] = json!(now());
    match s
        .store
        .compare_and_put(
            LEASES,
            &store_lease_key(kind, &lease_key(id, context, kind, resource)),
            row["version"].as_u64().unwrap_or(0),
            value.clone(),
        )
        .map_err(crate::runs::src_api::db)?
    {
        Ok(_) => Ok(value),
        Err(_) => Err(Error::new(
            StatusCode::CONFLICT,
            "resource_busy",
            "resource lease changed during release",
        )),
    }
}
pub(super) fn register(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let context = require(body, "context_id")?;
    let kind = require(body, "object_type")?;
    let object_id = require(body, "object_id")?;
    let owner = require(body, "owner")?;
    let natural = format!("{context}:{kind}:{object_id}");
    let prior = records(s, id, "test_object")?
        .into_iter()
        .find(|v| v["natural_key"] == natural);
    let revision = prior
        .as_ref()
        .and_then(|v| v["revision"].as_u64())
        .unwrap_or(0);
    if body["expected_revision"]
        .as_u64()
        .is_some_and(|v| v != revision)
    {
        return Err(Error::conflict("test object revision is stale"));
    }
    let value = json!({"context_id":context,"object_type":kind,"object_id":object_id,"owner":owner,
        "tenant":body["tenant"].as_str().unwrap_or(""),"metadata":body["metadata"].as_object().cloned().unwrap_or_default()});
    save_rec(s, id, "test_object", &natural, value, Some(revision))
}
fn state_event(
    s: &AppState,
    id: &str,
    experiment: &str,
    phase: &str,
    state: &Value,
    artifact: &Value,
) -> Api<()> {
    let event_id = format!("sobjectstate-{}", Uuid::new_v4().simple());
    save_rec(
        s,
        id,
        "object_state_event",
        &event_id,
        json!({"experiment_id":experiment,"phase":phase,"state":state,
        "artifact_refs":if artifact.is_null(){vec![]}else{vec![artifact.clone()]},"observed_at":now()}),
        Some(0),
    )?;
    Ok(())
}
pub(super) fn begin(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    if task(s, id)?["lifecycle"]["state"] != "active" {
        return Err(Error::conflict("SRC Task is not active"));
    }
    let context = require(body, "context_id")?;
    let kind = require(body, "object_type")?;
    let object_id = require(body, "object_id")?;
    let owner = require(body, "owner")?;
    let actor = &body["actor"];
    let participant = participant(s, id, context, actor)?;
    let initial = &body["initial_state"];
    if !initial.is_object()
        || !body["expected_change"].is_object()
        || !body["restoration"].is_object()
    {
        return Err(Error::invalid(
            "initial_state, expected_change and restoration must be objects",
        ));
    }
    let mutation = body["mutation"] != false;
    let resource = format!("{kind}:{object_id}");
    let lease = if mutation {
        acquire(
            s,
            id,
            &json!({"context_id":context,"resource_kind":"src.business_object",
        "resource_id":resource,"actor":actor,"ttl_seconds":body["ttl_seconds"]}),
        )?
    } else {
        Value::Null
    };
    let obj = register(s, id, body)?;
    let before = state_artifact(
        s,
        id,
        crate::runs::src_api::source_compact_json(initial).as_bytes(),
        &format!("{object_id}-before.json"),
        "application/json",actor["run_id"].as_str(),
    )?;
    let experiment_id = format!("sexperiment-{}", Uuid::new_v4().simple());
    let value = json!({"context_id":context,
        "object_type":kind,"object_id":object_id,"owner":owner,"tenant":body["tenant"].as_str().unwrap_or(""),
        "participant":participant,"actor":actor,"object_record_id":obj["record_id"],
        "before_artifact":before,"initial_state":initial,"expected_change":body["expected_change"],
        "restoration_plan":body["restoration"],"lease":lease,"after_artifact":null,"after_state":null,
        "restoration":{"status":if mutation{"unknown"}else{"not_applicable"}},
        "metadata":body["metadata"].as_object().cloned().unwrap_or_default()});
    let row = save_rec(
        s,
        id,
        "object_experiment",
        &experiment_id,
        value.clone(),
        Some(0),
    );
    if row.is_err() && mutation {
        let _ = release(
            s,
            id,
            &json!({"context_id":context,"resource_kind":"src.business_object","resource_id":resource,
            "actor":actor,"fencing_token":lease["fencing_token"]}),
        );
    }
    let row = row?;
    state_event(s, id, &experiment_id, "before", initial, &before)?;
    let mut result = row;
    result["fencing_token"] = json!(lease["fencing_token"].as_u64().unwrap_or(0));
    Ok(result)
}
fn experiment(s: &AppState, id: &str, body: &Value) -> Api<(String, Value)> {
    let experiment_id = require(body, "experiment_id")?.to_owned();
    let row = rec(s, id, "object_experiment", &experiment_id)?;
    let lease = &row["payload"]["lease"];
    if !lease.is_null() {
        let value = &row["payload"];
        let token = body["fencing_token"]
            .as_u64()
            .ok_or_else(|| Error::invalid("fencing_token required"))?;
        assert_lease(
            s,
            id,
            require(value, "context_id")?,
            "src.business_object",
            &format!(
                "{}:{}",
                require(value, "object_type")?,
                require(value, "object_id")?
            ),
            require(lease, "owner")?,
            token,
        )?;
    }
    Ok((experiment_id, row))
}
fn evidence(s: &AppState, id: &str, value: &Value) -> Api<Vec<Value>> {
    let mut refs = Vec::new();
    for reference in value.as_array().into_iter().flatten() {
        verify_ref(s, id, reference)?;
        refs.push(reference.clone());
    }
    Ok(refs)
}
pub(super) fn after(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let (experiment_id, row) = experiment(s, id, body)?;
    let state = &body["observed_state"];
    if !state.is_object() {
        return Err(Error::invalid("observed_state must be an object"));
    }
    let refs = evidence(s, id, &body["evidence_refs"])?;
    let artifact = state_artifact(
        s,
        id,
        crate::runs::src_api::source_compact_json(state).as_bytes(),
        &format!("{experiment_id}-after.json"),
        "application/json",row["payload"]["actor"]["run_id"].as_str(),
    )?;
    let mut value = row["payload"].clone();
    value["after_state"] = state.clone();
    value["after_artifact"] = artifact.clone();
    value["after_evidence_refs"] = json!(refs);
    let saved = save_rec(
        s,
        id,
        "object_experiment",
        &experiment_id,
        value,
        row["revision"].as_u64(),
    )?;
    state_event(s, id, &experiment_id, "after", state, &artifact)?;
    Ok(saved)
}
pub(super) fn restore(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let status = require(body, "status")?;
    if !matches!(
        status,
        "restored" | "partially_restored" | "not_restored" | "unknown" | "not_applicable"
    ) {
        return Err(Error::invalid("unknown restoration state"));
    }
    let readback = &body["readback"];
    if !matches!(status, "not_applicable" | "unknown") && !readback.is_object() {
        return Err(Error::invalid("restoration requires a state readback"));
    }
    let residual = body["residual_objects"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let unrestored = body["unrestored_fields"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let reason = body["reason"].as_str().unwrap_or("");
    if matches!(status, "partially_restored" | "not_restored" | "unknown")
        && reason.is_empty()
        && residual.is_empty()
        && unrestored.is_empty()
    {
        return Err(Error::invalid(
            "incomplete restoration requires a reason or residual state",
        ));
    }
    if status == "restored" && (!residual.is_empty() || !unrestored.is_empty()) {
        return Err(Error::conflict(
            "restored cannot retain residual objects or fields",
        ));
    }
    let (experiment_id, row) = experiment(s, id, body)?;
    let refs = evidence(s, id, &body["evidence_refs"])?;
    let artifact = if readback.is_object() {
        state_artifact(
            s,
            id,
            crate::runs::src_api::source_compact_json(readback).as_bytes(),
            &format!("{experiment_id}-restore-readback.json"),
            "application/json",row["payload"]["actor"]["run_id"].as_str(),
        )?
    } else {
        Value::Null
    };
    let mut value = row["payload"].clone();
    value["restoration"] = json!({"status":status,"readback":readback,"readback_artifact":artifact,
        "residual_objects":residual,"unrestored_fields":unrestored,"reason":reason,
        "evidence_refs":refs,"recorded_at":now()});
    let saved = save_rec(
        s,
        id,
        "object_experiment",
        &experiment_id,
        value.clone(),
        row["revision"].as_u64(),
    )?;
    state_event(
        s,
        id,
        &experiment_id,
        "restoration",
        &json!({"status":status,"readback":readback,
        "residual_objects":residual,"unrestored_fields":unrestored}),
        &artifact,
    )?;
    if body["release"] != false && !value["lease"].is_null() {
        release(
            s,
            id,
            &json!({"context_id":value["context_id"],"resource_kind":"src.business_object",
            "resource_id":format!("{}:{}",value["object_type"].as_str().unwrap_or(""),value["object_id"].as_str().unwrap_or("")),
            "actor":value["actor"],"fencing_token":value["lease"]["fencing_token"]}),
        )?;
    }
    Ok(saved)
}
pub(super) fn history(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let experiment_id = require(body, "experiment_id")?;
    let row = rec(s, id, "object_experiment", experiment_id)?;
    let events = records(s, id, "object_state_event")?
        .into_iter()
        .filter(|v| v["payload"]["experiment_id"] == experiment_id)
        .collect::<Vec<_>>();
    Ok(json!({"experiment":row,"events":events}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn business_object_lease_fences_cross_context_mutation() {
        let state = std::sync::Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = crate::runs::src_api::create_task_inner(&state, &json!({
            "title":"Lease test","goal":"Track one business object","idempotency_key":"src-object-lease-test"})).await.unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let a = json!({"context_id":"identity-a","resource_kind":"src.business_object","resource_id":"order:17"});
        let b = json!({"context_id":"identity-b","resource_kind":"src.business_object","resource_id":"order:17"});
        let lease = acquire(&state, id, &a).unwrap();
        assert_eq!(lease["fencing_token"], 1);
        assert_eq!(acquire(&state, id, &b).unwrap_err().code, "resource_busy");
        let mut stale = a.clone();
        stale["fencing_token"] = json!(0);
        assert_eq!(
            release(&state, id, &stale).unwrap_err().code,
            "resource_busy"
        );
        stale["fencing_token"] = json!(1);
        release(&state, id, &stale).unwrap();
        let second = acquire(&state, id, &b).unwrap();
        assert_eq!(second["fencing_token"], 2);
        let mut released = b.clone();
        released["fencing_token"] = json!(2);
        release(&state, id, &released).unwrap();
        let started = begin(
            &state,
            id,
            &json!({"context_id":"identity-b","object_type":"order","object_id":"17",
            "owner":"test-account","initial_state":{"paid":false},"expected_change":{"paid":true},
            "restoration":{"paid":false}}),
        )
        .unwrap();
        assert_eq!(started["fencing_token"], 3);
        let experiment = started["record_id"].as_str().unwrap();
        assert_eq!(after(&state,id,&json!({"experiment_id":experiment,"fencing_token":2,"observed_state":{"paid":true}})).unwrap_err().code,"resource_busy");
        after(
            &state,
            id,
            &json!({"experiment_id":experiment,"fencing_token":3,"observed_state":{"paid":true}}),
        )
        .unwrap();
        let restored = restore(
            &state,
            id,
            &json!({"experiment_id":experiment,"fencing_token":3,"status":"restored",
            "readback":{"paid":false}}),
        )
        .unwrap();
        assert_eq!(restored["payload"]["restoration"]["status"], "restored");
        assert_eq!(
            history(&state, id, &json!({"experiment_id":experiment})).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            release(
                &state,
                id,
                &json!({"context_id":"identity-b","resource_id":"order:17","fencing_token":3})
            )
            .unwrap_err()
            .code,
            "resource_busy"
        );
    }
}

fn state_artifact(s:&AppState,id:&str,bytes:&[u8],name:&str,media:&str,source_run:Option<&str>)->Api<Value>{
    let mut reference=put_artifact(s,id,bytes,name,media)?;
    reference["source_run_id"]=source_run.map(|id|json!(id)).unwrap_or(Value::Null);
    let key=format!("{id}:{}",reference["artifact_id"].as_str().unwrap());let mut saved=s.store.get("src_artifacts",&key).map_err(crate::runs::src_api::db)?.ok_or_else(||Error::conflict("state artifact disappeared"))?;
    saved["source_run_id"]=reference["source_run_id"].clone();s.store.put("src_artifacts",&key,&saved).map_err(crate::runs::src_api::db)?;Ok(reference)
}
