//! Workbench references resolve only inside the conversation's bound Task.
use super::*;
use base64::Engine;

pub(crate) fn resolve(s:&AppState,thread_id:&str,kind:&str,ref_id:&str)->Result<Value,String>{
    resolve_for_thread(s,thread_id,kind,ref_id).map_err(|error|format!("{}: {}",error.code,error.message))
}
pub(crate) fn resolve_for_thread(s:&AppState,thread_id:&str,kind:&str,ref_id:&str)->Api<Value>{
    let caller=resolve_authorized_src_caller(s,thread_id)?.ok_or_else(||Error::new(StatusCode::FORBIDDEN,"task_binding_invalid","thread has no SRC Task"))?;
    let task_id=caller.task_id.as_str();
    let record_kind=match kind{"source"|"passage"=>"asset","clue"=>"finding_claim","task"=>"workbench_todo","report"=>"frozen_report","evidence"=>"http_exchange","import"=>"import_batch","run"=>"run","change"=>"monitor_change","feedback"=>"platform_feedback","plan"=>"research_plan",_=>return Err(Error::new(StatusCode::BAD_REQUEST,"workbench_ref_invalid","unsupported SRC workbench reference"))};
    let(name,body,description)=if kind=="clue"{
        check_delegated_ref_access(&caller,kind,ref_id)?;
        let claims=records(s,task_id,"finding_claim")?.into_iter().filter(|row|row["payload"]["finding_id"]==ref_id).map(|row|row["payload"].clone()).collect::<Vec<_>>();
        let meta=records(s,task_id,"workbench_finding")?.into_iter().filter(|row|row["payload"]["finding_id"]==ref_id).max_by_key(|row|row["payload"]["version"].as_u64().unwrap_or(0)).map(|row|row["payload"].clone()).unwrap_or(json!({}));
        if claims.is_empty()&&meta.as_object().is_none_or(|row|row.is_empty()){return Err(Error::new(StatusCode::NOT_FOUND,"workbench_ref_stale","clue is unavailable in this Task"));}
        let stale=records(s,task_id,"finding_stale")?.into_iter().filter(|row|row["payload"]["finding_id"]==ref_id&&row["payload"]["cleared"]!=true).map(|row|row["payload"].clone()).collect::<Vec<_>>();
        let name=meta["title"].as_str().filter(|text|!text.is_empty()).or_else(||claims.first().and_then(|row|row["text"].as_str())).unwrap_or(ref_id).to_owned();
        let description=format!("SRC clue · {} 条主张",claims.len());
        (name,json!({"finding_id":ref_id,"meta":meta,"claims":claims,"stale_markers":stale}),description)
    }else if kind=="source"||kind=="passage"{
        let (base_asset_id, obs_index, attrs_only) = if let Some((prefix, idx)) = ref_id.rsplit_once("-obs-") {
            (prefix, idx.parse::<usize>().ok(), false)
        } else if let Some(prefix) = ref_id.strip_suffix("-attrs") {
            (prefix, None, true)
        } else {
            (ref_id, None, false)
        };
        if check_delegated_ref_access(&caller, kind, ref_id).is_err() {
            check_delegated_ref_access(&caller, kind, base_asset_id)?;
        }
        let payload=rec(s,task_id,"asset",base_asset_id)?["payload"].clone();
        let asset_name=["title","display_name","canonical_key"].iter().find_map(|field|payload["current_profile"][*field].as_str().or_else(||payload[*field].as_str()).filter(|text|!text.is_empty())).unwrap_or(base_asset_id).to_owned();
        if let Some(one_based)=obs_index {
            let mut obs: Vec<Value> = payload["observations"].as_array().cloned().unwrap_or_default();
            if obs.is_empty() {
                obs = records(s, task_id, "asset_observation")?.into_iter()
                    .filter(|row| row["payload"]["asset_id"] == base_asset_id)
                    .map(|row| row["payload"].clone())
                    .collect();
            }
            let selected = one_based.checked_sub(1).and_then(|i| obs.get(i)).cloned().ok_or_else(|| Error::new(StatusCode::NOT_FOUND, "workbench_ref_stale", "asset observation slice is unavailable"))?;
            (format!("{asset_name} · obs #{one_based}"), json!({"asset_id":base_asset_id,"observation_index":one_based,"observation":selected}), format!("SRC passage · asset_observation"))
        } else if attrs_only {
            (format!("{asset_name} · attrs"), json!({"asset_id":base_asset_id,"canonical_key":payload["canonical_key"],"type":payload["type"],"identity":payload["identity"],"current_profile":payload["current_profile"]}), format!("SRC passage · asset_profile"))
        } else {
            (asset_name, payload, format!("SRC {kind} · asset"))
        }
    }else if kind=="report"{
        check_delegated_ref_access(&caller,kind,ref_id)?;
        let payload = match rec(s, task_id, "frozen_report", ref_id) {
            Ok(row) => row["payload"].clone(),
            Err(err) if err.status == StatusCode::NOT_FOUND => rec(s, task_id, "workbench_report_draft", ref_id)?["payload"].clone(),
            Err(err) => return Err(err),
        };
        let name=["title","report_id","draft_id"].iter().find_map(|field|payload["document"][*field].as_str().or_else(||payload[*field].as_str()).filter(|text|!text.is_empty())).unwrap_or(ref_id).to_owned();
        (name, payload, "SRC report".to_owned())
    }else if kind=="evidence"{
        check_delegated_ref_access(&caller,kind,ref_id)?;
        let (resolved_kind, payload) = match rec(s, task_id, "http_exchange", ref_id) {
            Ok(row) => ("http_exchange", row["payload"].clone()),
            Err(err) if err.status == StatusCode::NOT_FOUND => match rec(s, task_id, "import_exchange", ref_id) {
                Ok(row) => ("import_exchange", row["payload"].clone()),
                Err(err2) if err2.status == StatusCode::NOT_FOUND => (
                    "record_revision",
                    rec(s, task_id, "record_revision", ref_id)?["payload"].clone(),
                ),
                Err(err2) => return Err(err2),
            },
            Err(err) => return Err(err),
        };
        let name=["title","display_name","url","canonical_key","method"].iter().find_map(|field|payload[*field].as_str().filter(|text|!text.is_empty())).unwrap_or(ref_id).to_owned();
        (name, payload, format!("SRC evidence · {resolved_kind}"))
    }else if kind=="feedback"{
        check_delegated_ref_access(&caller,kind,ref_id)?;
        let mut payload=rec(s,task_id,"platform_feedback",ref_id)?["payload"].clone();
        if !payload["text_ref"].is_null() {
            if let Ok(artifact)=domains::query(s,task_id,"src.artifact.read",&json!({"ref_data":payload["text_ref"]})) {
                if let Some(encoded)=artifact["data"].as_str()
                    && let Ok(bytes)=base64::engine::general_purpose::STANDARD.decode(encoded)
                {
                    payload["text"]=json!(String::from_utf8_lossy(&bytes).into_owned());
                }
            }
        }
        let name=["title","display_name","source","platform_reference","text"].iter().find_map(|field|payload[*field].as_str().filter(|text|!text.is_empty())).unwrap_or(ref_id).to_owned();
        (name, payload, "SRC feedback · platform_feedback".to_owned())
    }else{
        check_delegated_ref_access(&caller,kind,ref_id)?;
        let payload=rec(s,task_id,record_kind,ref_id)?["payload"].clone();
        let name=["title","display_name","canonical_key","question","text"].iter().find_map(|field|payload[*field].as_str().filter(|text|!text.is_empty())).unwrap_or(ref_id).to_owned();
        (name,payload,format!("SRC {kind} · {record_kind}"))
    };
    Ok(json!({"name":name,"description":description,"text":serde_json::to_string_pretty(&body).map_err(db)?,"status":"fresh"}))
}
