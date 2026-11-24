//! A single SQLite read transaction owns the views and their replay watermark.
use super::*;

pub fn read_snapshot(path: &std::path::Path, run_id: &str) -> Result<Value, String> {
    let conn = crate::history_import::read_db(path).map_err(|error| error.to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(|error| error.to_string())?;
    conn.execute_batch("BEGIN").map_err(|error| error.to_string())?;
    let mut stmt = conn.prepare("SELECT e.seq,e.kind,e.payload,e.artifact_id,e.verified,e.confidence,
        COALESCE(fs.status,'active'),COALESCE(fr.verdict,'') FROM events e
        LEFT JOIN fact_states fs ON fs.fact_seq=e.seq LEFT JOIN fact_reviews fr ON fr.fact_seq=e.seq ORDER BY e.seq")
        .map_err(|error| error.to_string())?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,
        row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,bool>(4)?,
        row.get::<_,f64>(5)?,row.get::<_,String>(6)?,row.get::<_,String>(7)?)))
        .map_err(|error| error.to_string())?;
    let mut events = Vec::new();
    for row in rows {
        let (seq,kind,raw,artifact,verified,confidence,status,review) = row.map_err(|error| error.to_string())?;
        let payload: Value = serde_json::from_str(&raw).map_err(|error| format!("Invalid graph event {seq}: {error}"))?;
        events.push((seq,kind,payload,artifact,verified,confidence,status,review));
    }
    let cutoff = events.iter().filter(|event| event.1 == "visit_wave_reset").map(|event| event.0).max().unwrap_or(0);
    let watermark = events.last().map(|event|event.0).unwrap_or(0);
    let mut evidence = Vec::new();
    let mut dead_ends = Vec::new();
    let mut flags = Vec::<Value>::new();
    let mut rejected_flags = Vec::<Value>::new();
    let mut findings = Vec::<Value>::new();
    let mut rejected_findings = HashSet::<String>::new();
    for (seq,kind,payload,artifact,verified,confidence,status,review) in events {
        match kind.as_str() {
            EV_FACT_ADDED if verified && matches!(status.as_str(),"active"|"verified"|"revalidated") && !matches!(review.as_str(),"challenged"|"unresolved"|"rejected") => {
                evidence.push(json!({"source":payload["source"].as_str().unwrap_or(""),"fact":payload["fact"].as_str().unwrap_or(""),
                    "artifact_path":null,"artifact_id":artifact,"verified":verified,"source_solver":payload["source_solver"].as_str().unwrap_or(""),
                    "confidence":confidence,"witness":payload["witness"],"verifier":payload["verifier"].as_str().unwrap_or("")}));
            },
            EV_DEAD_END if seq > cutoff => dead_ends.push(json!(payload["reason"].as_str().unwrap_or(""))),
            EV_FLAG_FOUND if !payload["flag"].is_null() && !flags.contains(&payload["flag"]) && !rejected_flags.contains(&payload["flag"]) => flags.push(payload["flag"].clone()),
            EV_FLAG_INVALIDATED if !payload["flag"].is_null() => {
                flags.retain(|flag|flag != &payload["flag"]);
                if !rejected_flags.contains(&payload["flag"]) { rejected_flags.push(payload["flag"].clone()); }
            },
            "finding_found" => {
                let key = crate::runs::pentest::finding_identity(&payload);
                if !rejected_findings.contains(&key) && !findings.iter().any(|row|crate::runs::pentest::finding_identity(row)==key) { findings.push(payload); }
            },
            "finding_invalidated" => {
                let key = payload["finding_key"].as_str().filter(|key|!key.is_empty()).map(str::to_owned)
                    .unwrap_or_else(||crate::runs::pentest::finding_identity(&payload));
                findings.retain(|row|crate::runs::pentest::finding_identity(row)!=key);
                rejected_findings.insert(key);
            },
            _ => {},
        }
    }
    let mut intents_stmt = conn.prepare("SELECT intent_id,goal,route_hash,status,dispatch_state,worker_class,branch_id,coverage_key,
        expected_observable,stop_condition,CAST(priority AS TEXT),worker,result FROM intents
        WHERE worker_class NOT IN ('review','verifier') ORDER BY created_seq").map_err(|error|error.to_string())?;
    let intents = intents_stmt.query_map([],|row| {
        let raw: String = row.get(10)?;
        let priority = raw.parse::<i64>().unwrap_or(match raw.as_str(){"operator"=>100,"high"=>50,"low"=>-10,_=>0});
        Ok(json!({"intent_id":row.get::<_,String>(0)?,"goal":row.get::<_,String>(1)?,"route_hash":row.get::<_,String>(2)?,
            "status":row.get::<_,String>(3)?,"dispatch_state":row.get::<_,String>(4)?,"worker_class":row.get::<_,String>(5)?,
            "branch_id":row.get::<_,String>(6)?,"coverage_key":row.get::<_,String>(7)?,"expected_observable":row.get::<_,String>(8)?,
            "stop_condition":row.get::<_,String>(9)?,"priority":priority,"worker":row.get::<_,Option<String>>(11)?,"result":row.get::<_,Option<String>>(12)?}))
    }).map_err(|error|error.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|error|error.to_string())?;
    Ok(json!({"graph_id":"ctf.shared_graph.v1","watermark":watermark,"state":{
        "challenge_id":run_id,"evidence":evidence,"dead_ends":dead_ends,"flag":flags.first(),"flags":flags,
        "rejected_flags":rejected_flags,"findings":findings,"intents":intents}}))
}
