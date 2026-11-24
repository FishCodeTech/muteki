//! Project bound Run events into the SRC Task record stream.
use super::*;
use base64::Engine;
use sha2::{Digest, Sha256};

const EXTRACTOR: &str = "src.run-projection.v1";
const APPLIED: &str = "src_run_projection_applied";

fn mapped(token: &str) -> Option<&'static str> {
    match token {
        "dead_end" | "deadend" => Some("dead_end"),
        "observation" => Some("observation"),
        "fact" => Some("fact"),
        "step" | "intent" => Some("step"),
        "tool" | "tool_call" | "command" => Some("tool_call"),
        "report" | "report_version" => Some("report_version"),
        "finding_reviewed" => Some("review_event"),
        "error" | "failed" => Some("run_error"),
        "directive" | "operator" => Some("operator_directive"),
        "artifact" => Some("artifact_event"),
        _ => None,
    }
}

fn kind(raw: &str) -> &'static str {
    let value = raw.trim().to_ascii_lowercase();
    if let Some(kind) = mapped(&value) { return kind; }
    let normalized = value.replace('-', "_");
    for segment in normalized.split('.').filter(|part| !part.is_empty()).rev() {
        if let Some(kind) = mapped(segment) { return kind; }
        let tokens: Vec<_> = segment.split('_').filter(|part| !part.is_empty()).collect();
        if tokens.contains(&"dead") && tokens.contains(&"end") { return "dead_end"; }
        for token in tokens {
            if let Some(kind) = mapped(token) { return kind; }
        }
    }
    "run_event"
}

fn source_event(run_id: &str, event: &Value) -> Value {
    let occurred_at = event["occurred_at"].as_str().map(str::to_owned)
        .or_else(|| event["ts"].as_f64().and_then(|seconds| {
            chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
                (seconds * 1_000_000.0).round() as i64).map(|time| {
                let precision = if time.timestamp_subsec_micros() == 0 {
                    chrono::SecondsFormat::Secs
                } else {chrono::SecondsFormat::Micros};
                time.to_rfc3339_opts(precision,true)
            })
        })).unwrap_or_else(now);
    json!({"schema_version":1,"run_id":run_id,"seq":event["seq"],
        "event_type":event["event_type"],"occurred_at":occurred_at,
        "payload":event["payload"].as_object().cloned().map(Value::Object)
            .unwrap_or_else(||json!({}))})
}

fn project(s: &AppState, task_id: &str, run_id: &str, event: &Value) -> Api<bool> {
    let seq = event["seq"].as_u64()
        .ok_or_else(|| Error::invalid("Run event seq is required"))?;
    let data = source_event(run_id,event);
    let event_type = data["event_type"].as_str().unwrap_or("");
    let payload_kind = data["payload"]["kind"].as_str().filter(|value| !value.is_empty());
    let record_kind = kind(payload_kind.unwrap_or(event_type));
    let source_event_id = format!("{EXTRACTOR}:{run_id}:{seq}:{:x}",
        Sha256::digest(source_compact_json(&data).as_bytes()));
    let marker_key = json!([task_id,"projection",source_event_id]).to_string();
    if s.store.get(APPLIED,&marker_key).map_err(db)?.is_some() { return Ok(false); }
    let record_id = format!("{run_id}:{seq}:{EXTRACTOR}");
    let record_key = rec_key(task_id,record_kind,&record_id);
    loop {
        if s.store.get(APPLIED,&marker_key).map_err(db)?.is_some() { return Ok(false); }
        let binding = records(s,task_id,"run_binding")?.into_iter()
            .find(|row| row["payload"]["run_id"] == run_id && row["payload"]["destroyed_at"].is_null())
            .ok_or_else(|| Error::new(StatusCode::NOT_FOUND,"src.run_not_bound",
                "Run 不属于该 SRC Task"))?;
        let binding_generation = binding["generation"].as_u64().unwrap_or(0);
        let payload_generation = binding["payload"]["execution_generation"]
            .as_u64()
            .or_else(|| binding["payload"]["generation"].as_u64())
            .unwrap_or(0);
        let live_generation = s
            .store
            .get("runs", run_id)
            .map_err(db)?
            .and_then(|run| run["execution_generation"].as_u64())
            .unwrap_or(0);
        let generation = data["payload"]["execution_generation"]
            .as_u64()
            .unwrap_or(binding_generation);
        if generation < binding_generation
            || generation < payload_generation
            || generation < live_generation
        {
            return Err(Error::new(StatusCode::CONFLICT,"authorization_stale",
                "event belongs to an old Run generation"));
        }
        if let Some(existing) = s.store.get(RECORDS,&record_key).map_err(db)? {
            if existing["values"]["payload"]["source"] != data {
                return Err(Error::new(StatusCode::CONFLICT,"version_conflict",
                    "projected run event sequence already exists with different content"));
            }
            advance_source(s,task_id,run_id,generation,seq,EXTRACTOR,"ready","")?;
            s.store.put_if_absent(APPLIED,&marker_key,&json!({"record_id":record_id}))
                .map_err(db)?;
            return Ok(false);
        }
        let current_watermark = s.store.get(SOURCE_WATERMARKS,&source_key(task_id,run_id)).map_err(db)?;
        if current_watermark.as_ref().is_some_and(|row|row["generation"].as_u64().unwrap_or(0)>generation) {
            return Err(Error::new(StatusCode::CONFLICT,"authorization_stale",
                "source generation is stale"));
        }
        if current_watermark.as_ref().is_some_and(|row|row["next_seq"].as_u64().unwrap_or(0)>seq) {
            return Err(Error::new(StatusCode::CONFLICT,"version_conflict",
                "source watermark cannot move backwards"));
        }
        let raw = data.to_string().into_bytes();
        let artifact_id = format!("sart_{}",uuid::Uuid::new_v4().simple());
        let name = format!("{run_id}-{seq}.json");
        let artifact_key = format!("{task_id}:{artifact_id}");
        let artifact = json!({"artifact_id":artifact_id,"version":1,"task_id":task_id,
            "size":raw.len(),"name":name,"media_type":"application/json",
            "original":true,"complete":"complete","source_run_id":run_id,"source_event_seq":seq});
        let stored_artifact = json!({"artifact_id":artifact_id,"version":1,"task_id":task_id,
            "size":raw.len(),"name":name,"media_type":"application/json",
            "data":base64::engine::general_purpose::STANDARD.encode(raw),"created_at":now(),
            "source_run_id":run_id,"source_event_seq":seq});
        let payload = json!({"source":data,"artifact":artifact,"event_type":event_type,
            "source_run_id":run_id,"source_event_seq":seq,"extractor_version":EXTRACTOR,
            "observed_at":data["occurred_at"]});
        let (key,_,mut record) = stage_rec(s,task_id,record_kind,&record_id,payload,Some(0))?;
        record["record_id"] = json!(record_id);
        record["parent_id"] = json!(run_id);
        record["natural_key"] = json!(format!("{run_id}:{seq}:{record_kind}"));
        record["generation"] = json!(generation);
        let record_doc = json!({"version":1,"values":record});
        let watermark = json!({"task_id":task_id,"source_id":run_id,"generation":generation,
            "next_seq":seq,"extractor_version":EXTRACTOR,"state":"ready","error":"",
            "updated_at":now()});
        let marker = json!({"record_id":record_id,"kind":record_kind,"artifact":artifact});
        let watermark_key = source_key(task_id,run_id);
        let mut expected = vec![(RECORDS,key.as_str(),None),
            ("src_artifacts",artifact_key.as_str(),None),
            (SOURCE_WATERMARKS,watermark_key.as_str(),current_watermark.as_ref()),
            (APPLIED,marker_key.as_str(),None)];
        let mut writes = vec![(RECORDS,key.as_str(),&record_doc),
            ("src_artifacts",artifact_key.as_str(),&stored_artifact),
            (SOURCE_WATERMARKS,watermark_key.as_str(),&watermark),
            (APPLIED,marker_key.as_str(),&marker)];
        let is_candidate_milestone = matches!(record_kind, "report_version" | "review_event")
            || matches!(
                data["payload"]["kind"].as_str().unwrap_or(""),
                "finding_found" | "finding_reviewed"
            )
            || matches!(event_type, "finding_found" | "finding_reviewed");
        let candidate_entry = if is_candidate_milestone {
            let cand_id = format!("cand_{run_id}_{seq}");
            let ep = &data["payload"];
            let cand_payload = json!({
                "candidate_id": cand_id,
                "status": "pending_review",
                "auto_promoted": false,
                "run_id": run_id,
                "seq": seq,
                "record_kind": record_kind,
                "source_record_id": record_id,
                "artifact_refs": ep.get("artifact_refs").or_else(|| ep.get("artifact_ids")).cloned().unwrap_or_else(|| json!([])),
                "exchange_ids": ep.get("exchange_ids").cloned().unwrap_or_else(|| json!([])),
                "summary": ep["summary"].as_str().or_else(|| ep["title"].as_str()).or_else(|| ep["message"].as_str()).unwrap_or(""),
                "payload": ep,
                "created_at": data["occurred_at"],
            });
            let (ckey, _, mut crec) = stage_rec(s, task_id, "worker_finding_candidate", &cand_id, cand_payload, Some(0))?;
            crec["record_id"] = json!(cand_id);
            crec["parent_id"] = json!(run_id);
            crec["natural_key"] = json!(cand_id);
            crec["generation"] = json!(generation);
            Some((cand_id, ckey, json!({"version": 1, "values": crec})))
        } else {
            None
        };
        if let Some((_, ckey, cdoc)) = candidate_entry.as_ref() {
            expected.push((RECORDS, ckey.as_str(), None));
            writes.push((RECORDS, ckey.as_str(), cdoc));
        }
        let binding_update = if generation > binding_generation
            || binding["payload"]["execution_generation"].as_u64() != Some(generation)
        {
            let lookup = binding["natural_key"].as_str().filter(|value| !value.is_empty())
                .unwrap_or_else(||binding["record_id"].as_str().unwrap_or(""));
            let mut next_payload = binding["payload"].clone();
            next_payload["execution_generation"] = json!(generation);
            let (key,version,mut updated) = stage_rec(s,task_id,"run_binding",lookup,
                next_payload,binding["revision"].as_u64())?;
            updated["generation"] = json!(generation);
            let before = s.store.get(RECORDS,&key).map_err(db)?;
            Some((key,before,json!({"version":version+1,"values":updated})))
        } else {None};
        if let Some((key,before,doc)) = binding_update.as_ref() {
            expected.push((RECORDS,key.as_str(),before.as_ref()));
            writes.push((RECORDS,key.as_str(),doc));
        }
        if s.store.put_many_if(&expected,&writes).map_err(db)? {
            domains::enqueue_run_milestone(
                s,
                task_id,
                run_id,
                seq,
                record_kind,
                &record_id,
                &data,
                candidate_entry.as_ref().map(|(cid, _, _)| cid.as_str()),
            );
            return Ok(true);
        }
    }
}

pub(in crate::runs) fn consume(s: &AppState, run_id: &str, event: &Value) -> Result<(),String> {
    let Some(task_id) = s.store.get("runs",run_id).map_err(|error|error.to_string())?
        .and_then(|run|run["src_task_id"].as_str().map(str::to_owned))
        .filter(|value|!value.is_empty()) else {return Ok(());};
    let after = s.store.get(SOURCE_WATERMARKS,&source_key(&task_id,run_id))
        .map_err(|error|error.to_string())?
        .and_then(|row|row["next_seq"].as_u64()).unwrap_or(0);
    let result = if event["seq"].as_u64().is_some_and(|seq|seq>after+1) {
        recover_run(s,&task_id,run_id).map(|_|())
    } else {
        project(s,&task_id,run_id,event).map(|_|())
    };
    if let Err(error) = &result {
        mark_failed(s,&task_id,run_id,event["seq"].as_u64().unwrap_or(0),error);
    }
    result.map_err(|error|format!("{}: {}",error.code,error.message))
}

fn mark_failed(s: &AppState, task_id: &str, run_id: &str, failed_seq: u64, error: &Error) {
    let key = source_key(task_id,run_id);
    for _ in 0..3 {
        let Ok(current) = s.store.get(SOURCE_WATERMARKS,&key) else {return;};
        let next_seq = current.as_ref().and_then(|row|row["next_seq"].as_u64()).unwrap_or(0);
        if next_seq >= failed_seq {return;}
        let generation = current.as_ref().and_then(|row|row["generation"].as_u64()).unwrap_or(0);
        let row = json!({"task_id":task_id,"source_id":run_id,"generation":generation,
            "next_seq":next_seq,"extractor_version":EXTRACTOR,"state":"failed",
            "error":format!("{}: {}",error.code,error.message),"updated_at":now()});
        match s.store.put_many_if(&[(SOURCE_WATERMARKS,&key,current.as_ref())],
            &[(SOURCE_WATERMARKS,&key,&row)]) {
            Ok(true) => return,
            Ok(false) => continue,
            Err(_) => return,
        }
    }
}

fn sync_binding_live_generation(s: &AppState, task_id: &str, run_id: &str) -> Api<()> {
    let Some(live_generation) = s
        .store
        .get("runs", run_id)
        .map_err(db)?
        .and_then(|run| run["execution_generation"].as_u64())
    else {
        return Ok(());
    };
    let Some(binding) = records(s, task_id, "run_binding")?
        .into_iter()
        .find(|row| row["payload"]["run_id"] == run_id && row["payload"]["destroyed_at"].is_null())
    else {
        return Ok(());
    };
    let binding_gen = binding["generation"].as_u64().unwrap_or(0);
    let payload_gen = binding["payload"]["execution_generation"]
        .as_u64()
        .or_else(|| binding["payload"]["generation"].as_u64())
        .unwrap_or(0);
    if live_generation > binding_gen || live_generation > payload_gen {
        let lookup = binding["natural_key"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| binding["record_id"].as_str().unwrap_or(""));
        let mut next_payload = binding["payload"].clone();
        next_payload["execution_generation"] = json!(live_generation);
        let (key, version, mut updated) = stage_rec(
            s,
            task_id,
            "run_binding",
            lookup,
            next_payload,
            binding["revision"].as_u64(),
        )?;
        updated["generation"] = json!(live_generation.max(binding_gen));
        let before = s.store.get(RECORDS, &key).map_err(db)?;
        let _ = s.store.put_many_if(
            &[(RECORDS, key.as_str(), before.as_ref())],
            &[(RECORDS, key.as_str(), &json!({"version": version + 1, "values": updated}))],
        ).map_err(db)?;
    }
    Ok(())
}

fn recover_run(s: &AppState, task_id: &str, run_id: &str) -> Api<usize> {
    sync_binding_live_generation(s, task_id, run_id)?;
    let after = s.store.get(SOURCE_WATERMARKS,&source_key(task_id,run_id)).map_err(db)?
        .and_then(|row|row["next_seq"].as_u64()).unwrap_or(0);
    let mut projected = 0;
    for (_, maybe_event) in runs::read_jsonl_event_scan(s, run_id, after) {
        let Some(event) = maybe_event else {
            continue;
        };
        match project(s,task_id,run_id,&event) {
            Ok(applied) => projected += usize::from(applied),
            Err(error) => {
                mark_failed(s,task_id,run_id,event["seq"].as_u64().unwrap_or(0),&error);
                return Err(error);
            }
        }
    }
    Ok(projected)
}

pub(super) fn recover_task(s: &AppState, task_id: &str) -> Api<usize> {
    let mut projected = 0;
    for binding in records(s,task_id,"run_binding")? {
        if !binding["payload"]["destroyed_at"].is_null() {continue;}
        let Some(run_id) = binding["payload"]["run_id"].as_str() else {continue;};
        projected += recover_run(s,task_id,run_id)?;
    }
    Ok(projected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_event_source_digest_matches_python_contract() {
        let event = json!({"run_id":"run-source","seq":7,"event_type":"pentest.fact",
            "ts":1730000000.123,"payload":{"execution_generation":2,"fact":"fixture"},
            "solver_id":"cli-pi#1"});
        let source = source_event("run-source",&event);
        assert_eq!(source["occurred_at"],"2024-10-27T03:33:20.123000Z");
        assert!(source.get("solver_id").is_none());
        assert_eq!(format!("{:x}",Sha256::digest(source_compact_json(&source).as_bytes())),
            "59e89843eab250f3526451fb3e3e659b5418ec6862d7813aa8aa267dc44cd83f");
    }

    async fn fixture() -> (tempfile::TempDir, Arc<AppState>, String, String) {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(directory.path().join("muteki.sqlite")).unwrap();
        store.put("system","state_dir",&json!(directory.path().to_string_lossy())).unwrap();
        let state = Arc::new(AppState::new(store));
        let created = create_task_inner(&state,&json!({"title":"Run projection",
            "goal":"Review local run","authorization":{"targets":["http://127.0.0.1:8080/app"]},
            "idempotency_key":"run-projection-fixture"})).await.unwrap();
        let task_id = created["output"]["task_id"].as_str().unwrap().to_owned();
        let ensured = ensure_run(&state,&task_id,&json!({
            "target_url":"http://127.0.0.1:8080/app","question":"check"})).unwrap();
        let run_id = ensured["binding"]["run_id"].as_str().unwrap().to_owned();
        (directory,state,task_id,run_id)
    }

    #[tokio::test]
    async fn bound_run_events_project_once_and_recover_after_restart() {
        let (directory,state,task_id,run_id) = fixture().await;
        let first = runs::emit_event(&state,&run_id,None,"pentest.observation",
            json!({"kind":"observation","execution_generation":1,"text":"first"}));
        let observation_records = records(&state,&task_id,"observation").unwrap();
        assert_eq!(observation_records.len(),1);
        assert_eq!(observation_records[0]["payload"]["source"]["schema_version"],1);
        assert_eq!(observation_records[0]["payload"]["source"]["run_id"],run_id);
        assert_eq!(observation_records[0]["payload"]["source"]["seq"],first["seq"]);
        assert!(observation_records[0]["payload"]["source"]["occurred_at"].is_string());
        assert!(observation_records[0]["payload"]["source"].get("solver_id").is_none());
        assert_eq!(observation_records[0]["generation"],1);
        assert_eq!(source_watermarks(&state,&task_id).unwrap()[0]["next_seq"],1);
        assert_eq!(state.store.list("src_artifacts").unwrap().len(),2);
        assert_eq!(project(&state,&task_id,&run_id,&first).unwrap(),false);
        let mut changed = first.clone();
        changed["payload"]["text"] = json!("different");
        assert_eq!(project(&state,&task_id,&run_id,&changed).unwrap_err().code,"version_conflict");
        let stale = json!({"seq":2,"event_type":"pentest.fact",
            "occurred_at":"2026-11-02T00:00:00Z",
            "payload":{"kind":"fact","execution_generation":0}});
        assert_eq!(project(&state,&task_id,&run_id,&stale).unwrap_err().code,"authorization_stale");

        let recovered = json!({"seq":2,"run_id":run_id,"event_type":"pentest.report",
            "occurred_at":"2026-11-02T00:00:00Z",
            "payload":{"execution_generation":1,"version":1}});
        let path = runs::session_dir(&state,&run_id).join("events.jsonl");
        use std::io::Write;
        writeln!(std::fs::OpenOptions::new().append(true).open(path).unwrap(),"{recovered}").unwrap();
        drop(state);
        let reopened = Arc::new(AppState::new(crate::store::Store::open(
            directory.path().join("muteki.sqlite")).unwrap()));
        assert_eq!(recover_task(&reopened,&task_id).unwrap(),1);
        assert_eq!(records(&reopened,&task_id,"report_version").unwrap().len(),1);
        assert_eq!(source_watermarks(&reopened,&task_id).unwrap()[0]["next_seq"],2);
        assert_eq!(recover_task(&reopened,&task_id).unwrap(),0);
    }

    #[tokio::test]
    async fn projection_marker_failure_rolls_back_artifact_record_and_watermark() {
        let (directory,state,task_id,run_id) = fixture().await;
        let event = json!({"seq":1,"run_id":run_id,"event_type":"pentest.fact",
            "occurred_at":"2026-11-02T00:00:00Z",
            "payload":{"execution_generation":1,"fact":"local fixture"}});
        let db = rusqlite::Connection::open(directory.path().join("muteki.sqlite")).unwrap();
        db.execute_batch("CREATE TRIGGER fail_src_projection_marker BEFORE INSERT ON documents
            WHEN NEW.collection='src_run_projection_applied'
            BEGIN SELECT RAISE(ABORT,'projection marker failed'); END;").unwrap();
        assert!(project(&state,&task_id,&run_id,&event).is_err());
        assert!(records(&state,&task_id,"fact").unwrap().is_empty());
        assert_eq!(state.store.list("src_artifacts").unwrap().len(),1);
        assert!(source_watermarks(&state,&task_id).unwrap().is_empty());
        use std::io::Write;
        let path = runs::session_dir(&state,&run_id).join("events.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        writeln!(std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap(),
            "{event}").unwrap();
        assert!(recover_task(&state,&task_id).is_err());
        assert_eq!(source_watermarks(&state,&task_id).unwrap()[0]["state"],"failed");
        assert!(records(&state,&task_id,"fact").unwrap().is_empty());
        db.execute_batch("DROP TRIGGER fail_src_projection_marker;").unwrap();
        assert_eq!(recover_task(&state,&task_id).unwrap(),1);
        assert_eq!(records(&state,&task_id,"fact").unwrap().len(),1);
        assert_eq!(state.store.list("src_artifacts").unwrap().len(),2);
        assert_eq!(source_watermarks(&state,&task_id).unwrap()[0]["next_seq"],1);
        assert_eq!(source_watermarks(&state,&task_id).unwrap()[0]["state"],"ready");
    }

    #[tokio::test]
    async fn later_live_event_recovers_failed_predecessor_before_advancing() {
        let (directory,state,task_id,run_id) = fixture().await;
        let db = rusqlite::Connection::open(directory.path().join("muteki.sqlite")).unwrap();
        db.execute_batch("CREATE TRIGGER fail_first_projection BEFORE INSERT ON documents
            WHEN NEW.collection='src_run_projection_applied'
            BEGIN SELECT RAISE(ABORT,'first projection failed'); END;").unwrap();
        runs::emit_event(&state,&run_id,None,"pentest.observation",
            json!({"kind":"observation","execution_generation":1,"text":"first"}));
        assert!(records(&state,&task_id,"observation").unwrap().is_empty());
        assert_eq!(source_watermarks(&state,&task_id).unwrap()[0]["state"],"failed");
        db.execute_batch("DROP TRIGGER fail_first_projection;").unwrap();
        runs::emit_event(&state,&run_id,None,"pentest.observation",
            json!({"kind":"observation","execution_generation":1,"text":"second"}));
        assert_eq!(records(&state,&task_id,"observation").unwrap().len(),2);
        assert_eq!(source_watermarks(&state,&task_id).unwrap()[0]["next_seq"],2);
        assert_eq!(source_watermarks(&state,&task_id).unwrap()[0]["state"],"ready");
    }
}
