//! CTF result admission and commit boundary.
use super::*;

pub(crate) fn commit(
    state: &AppState,
    run_id: &str,
    worker: &str,
    intent: &str,
    checkpoint: &str,
    payload: &Value,
    finish: bool,
    solved: bool,
) -> Result<Value, String> {
    let run = state
        .store
        .get("runs", run_id)
        .map_err(|e| e.to_string())?
        .ok_or("Worker result Run not found")?;
    let epoch = run
        .get("target_epoch")
        .or_else(|| run.get("execution_generation"))
        .and_then(Value::as_u64)
        .ok_or("Worker target epoch missing")?;
    let mut normalized = payload.clone();
    if !normalized.is_object() {
        return Err("WorkerResult must be an object".into());
    }
    for field in ["observations", "dead_ends", "pocs", "artifacts"] {
        if normalized[field].is_null() {
            normalized[field] = json!([]);
        }
        if !normalized[field].is_array() {
            return Err(format!("WorkerResult {field} must be an array"));
        }
    }
    normalized.as_object_mut().unwrap().remove("request_digest");
    let request_digest=crate::runner::canonical_digest(json!([normalized,finish,solved]))?;
    normalized["request_digest"]=json!(request_digest);
    let graph = graph::SharedGraph::open(state, run_id)?;
    graph.migrate_worker_result_request(run_id,worker,intent,epoch,checkpoint,&normalized,&request_digest,finish,solved)?;
    if let Some(receipt) = graph.worker_result_receipt_for_request(run_id, worker, intent, epoch, checkpoint, &request_digest)? {
        publish_result(state, run_id, worker, &receipt)?;
        return Ok(receipt);
    }
    let staged = graph.staged_worker_result(worker, intent, epoch)?;
    for (kind, mut value) in staged.clone() {
        if kind == "observations" {
            if normalized["handoff_missing"] == true {
                value["claim_verified"] = json!(false);
            } else {
                normalized["supersedes"] = value["supersedes"].clone();
            }
            if let (Some(title), Some(content)) = (value["title"].as_str(), value["text"].as_str())
            {
                normalized["result_detail"] = json!(title);
                value["text"] = json!(format!("{title}\n{content}"));
            }
        }
        if kind == "vulnerability_report" || kind == "need_input" {
            normalized[&kind] = value;
        } else if let Some(rows) = normalized.get_mut(&kind).and_then(Value::as_array_mut) {
            rows.push(value);
        } else {
            return Err("Unrecognized staged Worker result".into());
        }
    }
    if normalized["status"] == "done" {
        let has_fact = normalized["observations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["claim_verified"] == true);
        let dead = normalized["dead_ends"].as_array().unwrap().clone();
        normalized["status"] = json!(if solved {
            "solved"
        } else if !has_fact && !dead.is_empty() {
            "dead_end"
        } else {
            "explored"
        });
        if !has_fact {
            normalized["result_detail"] = json!(if let Some(last) = dead.last() {
                format!(
                    "Worker explicitly ruled out: {}",
                    last["reason"].as_str().unwrap_or("")
                )
            } else {
                "Step completed without a confirmed new Fact".to_owned()
            });
        }
    }
    let mut image_refs = Vec::new();
    for poc in normalized["pocs"].as_array_mut().unwrap() {
        if !poc.is_object() {
            return Err("Worker PoC must be an object".into());
        }
        let status = poc["status"].as_str().unwrap_or("available");
        if !matches!(
            status,
            "available" | "wip" | "directional" | "spent" | "quarantined"
        ) {
            poc["status"] = json!("available");
        }
        if !poc["image_ref"].is_null() {
            let trusted = staged.iter().any(|(kind, row)| {
                kind == "pocs"
                    && row["poc_id"] == poc["poc_id"]
                    && row["image_ref"] == poc["image_ref"]
            }) || graph.events().iter().any(|event| {
                event["kind"] == "poc_saved"
                    && event["actor"] == worker
                    && event["payload"]["intent_id"] == intent
                    && event["payload"]["target_epoch"] == epoch
                    && event["payload"]["poc_id"] == poc["poc_id"]
                    && event["payload"]["image_ref"] == poc["image_ref"]
            });
            if !trusted {
                return Err(
                    "Image PoC has no host staging receipt for this Worker and Step".into(),
                );
            }
            evidence::image(state, run_id, &poc["image_ref"])?;
            poc["artifact_id"] = poc["image_ref"]["artifact_id"].clone();
            let mut reference = poc["image_ref"].clone();
            reference["kind"] = json!("image");
            reference["worker_id"] = json!(worker);
            reference["command"] = poc["entry_command"].clone();
            image_refs.push(reference);
        }
    }
    for poc in graph.pocs().into_iter().filter(|poc| {
        poc["actor"] == worker
            && poc["intent_id"] == intent
            && poc["target_epoch"] == epoch
            && matches!(
                poc["status"].as_str(),
                Some("available" | "wip" | "directional")
            )
            && poc["image_ref"].is_object()
    }) {
        evidence::image(state, run_id, &poc["image_ref"])?;
        let mut reference = poc["image_ref"].clone();
        reference["kind"] = json!("image");
        reference["worker_id"] = json!(worker);
        reference["command"] = poc["entry_command"].clone();
        if !image_refs.contains(&reference) {
            image_refs.push(reference);
        }
    }
    let events: Vec<Value> = persisted_event_iter(state, run_id)?
        .filter(|event| {
            event["event_type"] == "tool.result"
                && event["solver_id"] == worker
                && event["payload"]["intent_id"] == intent
                && event["payload"]["target_epoch"] == epoch
        })
        .collect();
    for observation in normalized["observations"].as_array_mut().unwrap() {
        if !observation.is_object() {
            return Err("Worker observation must be an object".into());
        }
        observation["mode"] = run["mode"].clone();
        let id = observation["provenance"]["artifact_id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let event = events.iter().rev().find(|event| {
            event["event_type"] == "tool.result"
                && event["solver_id"] == worker
                && event["payload"]["intent_id"] == intent
                && event["payload"]["target_epoch"] == epoch
                && event["payload"]["artifact_id"] == id
        });
        let mut provenance =
            json!({"worker_id":worker,"intent_id":intent,"target_epoch":epoch,"mode":run["mode"]});
        {
            let refs: Vec<Value> = events
                .iter()
                .filter(|event| {
                    event["event_type"] == "tool.result"
                        && event["solver_id"] == worker
                        && event["payload"]["intent_id"] == intent
                        && event["payload"]["target_epoch"] == epoch
                })
                .map(|event| event["payload"]["artifact_ref"].clone())
                .collect();
            if refs.is_empty()
                && observation["claim_verified"] == true
                && observation["enables"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
            {
                return Err(
                    "enabling evidence needs a persisted tool result from this Step".into(),
                );
            }
            if !refs.is_empty() {
                provenance["artifact_refs"] = json!(refs);
                if let Some(enables) = observation["enables"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                {
                    provenance["enables"] = json!(enables);
                }
            }
        }
        if let Some(event) = event {
            evidence::verify(state, run_id, &event["payload"]["artifact_ref"])?;
            provenance["artifact_id"] = json!(id);
            let mut refs = vec![event["payload"]["artifact_ref"].clone()];
            refs.extend(image_refs.clone());
            provenance["artifact_refs"] = json!(refs);
            provenance["artifact_sha256"] = event["payload"]["artifact_ref"]["sha256"].clone();
            provenance["tool_event_seq"] = event["seq"].clone();
            provenance["tool_event_id"] = json!(format!("{run_id}:{}", event["seq"]));
            provenance["observed_at"] = event["ts"].clone();
            provenance["promoted_at"] = json!(now_secs());
            provenance["tool_event_ts"] = event["ts"].clone();
            provenance["tool_call_id"] = event["payload"]["tool_call_id"].clone();
        }

        observation["provenance"] = provenance;
    }
    {}
    let mut references = Vec::new();
    for event in events.iter().filter(|event| {
        event["event_type"] == "tool.result"
            && event["solver_id"] == worker
            && event["payload"]["intent_id"] == intent
            && event["payload"]["target_epoch"] == epoch
    }) {
        let reference = event["payload"]["artifact_ref"].clone();
        evidence::verify(state, run_id, &reference)?;
        references.push(reference);
    }
    for artifact in normalized["artifacts"].as_array().unwrap() {
        let id = artifact
            .as_str()
            .ok_or("WorkerResult artifact identity must be text")?;
        if !references
            .iter()
            .any(|reference| reference["artifact_id"] == id)
        {
            return Err("WorkerResult artifact is not attributed to its Step and epoch".into());
        }
    }
    for poc in normalized["pocs"].as_array().unwrap() {
        worker_poc::verify(&session_dir(state, run_id), poc)?;
    }
    normalized["artifact_refs"] = json!(references);
    let receipt = graph.with_immediate_tx(|| {
        if let Some(receipt) =
            graph.worker_result_receipt_for_request(run_id, worker, intent, epoch, checkpoint, &request_digest)?
        {
            return Ok(receipt);
        }
        if graph.staged_worker_result(worker, intent, epoch)? != staged {
            return Err(
                "Worker staging changed during evidence validation; retry settlement".into(),
            );
        }
        graph.commit_worker_result(
            state,
            run_id,
            worker,
            intent,
            epoch,
            checkpoint,
            &normalized,
            finish,
            solved,
        )
    })?;
    publish_result(state, run_id, worker, &receipt)?;
    Ok(receipt)
}
