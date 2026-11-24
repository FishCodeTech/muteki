//! Domain records, events and receipts share one compare-and-swap transaction.
use super::*;
use std::collections::BTreeMap;

pub(crate) const EVENTS: &str = "platform_domain_events";

pub(crate) fn event(
    envelope: &Value,
    aggregate_type: &str,
    aggregate_id: &str,
    event_type: &str,
    producer: &str,
    payload: Value,
) -> Value {
    let command_id = envelope["command_id"].as_str().unwrap_or("");
    json!({"schema_version":1,"event_id":new_id("evt"),"aggregate_type":aggregate_type,
        "aggregate_id":aggregate_id,"event_type":event_type,"occurred_at":now(),"producer":producer,
        "actor_id":envelope["actor"]["id"].as_str().unwrap_or("local-user"),
        "command_id":if command_id.is_empty(){Value::Null}else{json!(command_id)},
        "causation_id":if command_id.is_empty(){Value::Null}else{json!(command_id)},
        "correlation_id":envelope["payload"]["correlation_id"].as_str().unwrap_or(command_id),
        "idempotency_key":envelope["idempotency_key"],"payload":payload})
}

pub(crate) fn commit(
    state: &AppState,
    expected: &[(&str, &str, Option<&Value>)],
    records: &[(&str, &str, &Value)],
    events: &[Value],
) -> Result<Option<Vec<Value>>, Failure> {
    commit_with_deletes(state,expected,records,events,&[])
}

pub(crate) fn commit_with_deletes(
    state: &AppState,
    expected: &[(&str, &str, Option<&Value>)],
    records: &[(&str, &str, &Value)],
    events: &[Value],
    deletes: &[(String,String)],
) -> Result<Option<Vec<Value>>,Failure> {
    commit_checked(state,expected,records,events,deletes,|_|Ok(true))
}

pub(crate) fn commit_checked<F>(
    state: &AppState,
    expected: &[(&str, &str, Option<&Value>)],
    records: &[(&str, &str, &Value)],
    events: &[Value],
    deletes: &[(String,String)],
    check: F,
) -> Result<Option<Vec<Value>>, Failure>
where F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<bool> {
    let mut versions = BTreeMap::<String, (Option<Value>, Value)>::new();
    let mut staged = Vec::with_capacity(events.len());
    for original in events {
        let kind = original["aggregate_type"].as_str().ok_or_else(|| storage("Missing event aggregate type"))?;
        let id = original["aggregate_id"].as_str().ok_or_else(|| storage("Missing event aggregate id"))?;
        let key = format!("{kind}\0{id}");
        if !versions.contains_key(&key) {
            let previous = state.store.get(AGGREGATE_VERSIONS, &key).map_err(storage)?;
            let floor = aggregate_version(state, kind, id)?;
            versions.insert(key.clone(), (previous, json!({"aggregate_type":kind,"aggregate_id":id,"version":floor})));
        }
        let (_, version) = versions.get_mut(&key).unwrap();
        let seq = version["version"].as_u64().unwrap_or(0).checked_add(1)
            .ok_or_else(|| storage("Domain event sequence exhausted"))?;
        if original["stream_seq"].as_u64().is_some_and(|expected| expected != seq) { return Ok(None); }
        version["version"] = json!(seq);
        version["updated_at"] = original["occurred_at"].clone();
        let mut event = original.clone();
        event["stream_seq"] = json!(seq);
        staged.push(event);
    }
    let mut conditions = expected.to_vec();
    let mut writes = records.to_vec();
    for (key, (previous, version)) in &versions {
        conditions.push((AGGREGATE_VERSIONS, key.as_str(), previous.as_ref()));
        writes.push((AGGREGATE_VERSIONS, key.as_str(), version));
    }
    for event in &staged {
        let id = event["event_id"].as_str().ok_or_else(|| storage("Missing event identity"))?;
        conditions.push((EVENTS, id, None));
        writes.push((EVENTS, id, event));
    }
    if state.store.put_many_if_with_deletes(&conditions, &writes, |transaction| {
        Ok(check(transaction)?.then(||deletes.to_vec()))
    }).map_err(storage)? {
        Ok(Some(staged))
    } else {
        Ok(None)
    }
}
