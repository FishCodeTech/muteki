//! A remote snapshot changes immutable revisions and current projections in one commit per challenge.
use super::*;
fn normalized(item:&Value)->Value{
    let mut content=json!({});
    for key in ["name","category","target","flag_format"]{content[key]=json!(field(item,key).trim());}
    for key in ["instance_required","platform_type"]{if let Some(value)=item.get(key){content[key]=value.clone();}}
    content["description"]=json!(field(item,"description").replace("\r\n","\n").replace('\r',"\n").trim_matches('\n'));
    content["points"]=json!(item["points"].as_f64().filter(|value|value.is_finite()).unwrap_or(0.));
    for key in ["hints","prerequisites"]{content[key]=json!(item[key].as_array().into_iter().flatten().map(|value|value.as_str().map(|text|text.trim().to_owned()).unwrap_or_else(||value.to_string())).collect::<Vec<_>>());}
    content["multi_flag"]=json!(item["multi_flag"]==true);content["expected_flags"]=json!(item["expected_flags"].as_u64().unwrap_or(1).max(1));content["artifacts"]=item["artifacts"].as_array().cloned().map(Value::Array).unwrap_or_else(||json!([]));content
}
fn identity(content:&Value)->Value{
    let mut value=normalized(content);value["artifacts"]=json!(content["artifacts"].as_array().into_iter().flatten().map(|artifact|json!({"name":artifact["name"],"size":artifact["size"],"content":artifact.get("sha256").filter(|v|v.as_str().is_some_and(|v|!v.is_empty())).unwrap_or(&artifact["artifact_id"])})).collect::<Vec<_>>());value
}
fn push(report:&mut Value,field:&str,id:&str){report[field].as_array_mut().unwrap().push(json!(id));}
fn pause_text(value:&Value)->Option<String>{value.get("paused_from").and_then(Value::as_str).map(str::trim).filter(|text|!text.is_empty()).map(str::to_owned)}
fn pause_source(item:&Value)->Option<String>{
    pause_text(item).or_else(||item.get("raw").and_then(|raw|pause_text(raw).or_else(||pause_text(&raw["list"])).or_else(||pause_text(&raw["detail"]))))
}
pub(super) async fn save_challenges(state:&Arc<AppState>,connection:&Value,competition_id:&str,command_id:&str,remote:&[Value],snapshot_complete:bool)->ApiResult<Value>{
    let mut identities=std::collections::HashSet::new();for item in remote{let external=field(item,"external_challenge_id");if !external.is_empty()&&!identities.insert(external){return Err(ApiError::invalid("Platform snapshot contains duplicate challenge identities"));}}
    let root=native_chat::state_dir(state).join("control/competition/snapshot-writes");fs::create_dir_all(&root).map_err(ApiError::internal)?;
    let file=OpenOptions::new().create(true).read(true).write(true).open(root.join(format!("{competition_id}.lock"))).map_err(ApiError::internal)?;
    let _lock=tokio::task::spawn_blocking(move||{fs2::FileExt::lock_exclusive(&file).map_err(ApiError::internal)?;Ok::<_,ApiError>(file)}).await.map_err(ApiError::internal)??;
    let mut report=json!({"competition_id":competition_id,"synced_challenges":0,"added":[],"changed":[],"unchanged":[],"removed":[],"locked":[],"unlocked":[],"solved":[],"artifact_failures":[]});
    let mut seen=std::collections::HashSet::new();
    for item in remote{
        let external=field(item,"external_challenge_id");if external.is_empty(){continue;}seen.insert(external.to_owned());
        let prior=scoped(state,CHALLENGES,competition_id)?.into_iter().find(|row|row["external_challenge_id"]==external);
        if prior.as_ref().is_some_and(|row|row["tombstoned"]==true){push(&mut report,"unchanged",field(prior.as_ref().unwrap(),"challenge_id"));continue;}
        let cid=prior.as_ref().map(|row|field(row,"challenge_id").to_owned()).unwrap_or_else(||id("cch"));
        let old_revision=prior.as_ref().and_then(|row|row["current_revision_id"].as_str()).map(|rid|state.store.get(REVISIONS,rid).map_err(ApiError::internal)).transpose()?.flatten();
        let hidden=item["remote_state"]=="hidden";let mut content=normalized(item);
        if !hidden{match artifacts::materialize(state,connection,item,old_revision.as_ref()).await{
            Ok(artifacts)=>content["artifacts"]=json!(artifacts),Err(error)=>{
                report["artifact_failures"].as_array_mut().unwrap().push(json!({"external_challenge_id":external,"challenge_id":cid,"category":error.code,"message":error.message}));
                append_event(state,competition_id,"challenge",&cid,"competition.sync.artifact_failed",command_id,json!({"challenge_id":cid,"code":error.code}))?;continue;
            }
        }}
        let mut applied=false;
        for _ in 0..32{
            let before=state.store.get(CHALLENGES,&cid).map_err(ApiError::internal)?;
            if before.as_ref().is_some_and(|row|row["tombstoned"]==true){push(&mut report,"unchanged",&cid);applied=true;break;}
            let revisions=scoped(state,REVISIONS,competition_id)?.into_iter().filter(|row|row["competition_challenge_id"]==cid).collect::<Vec<_>>();
            let existing=if hidden{None}else{revisions.iter().find(|row|identity(row)==identity(&content))};
            let new_revision=if !hidden && existing.is_none(){let mut revision=content.clone();revision["revision_id"]=json!(id("crev"));revision["competition_id"]=json!(competition_id);revision["competition_challenge_id"]=json!(cid);revision["revision_seq"]=json!(revisions.iter().filter_map(|row|row["revision_seq"].as_u64()).max().unwrap_or(0)+1);revision["created_at"]=json!(now());Some(revision)}else{None};
            let revision_id=if hidden{before.as_ref().map(|row|row["current_revision_id"].clone()).unwrap_or(Value::Null)}else{existing.or(new_revision.as_ref()).unwrap()["revision_id"].clone()};
            let mut next=before.clone().unwrap_or_else(||json!({"challenge_id":cid,"competition_id":competition_id,"external_challenge_id":external,"state":"discovered","paused_from":null,"tombstoned":false,"created_at":now()}));
            for key in ["name","category"]{if !hidden || !field(&content,key).is_empty(){next[key]=content[key].clone();}}
            next["current_revision_id"]=revision_id.clone();next["remote_state"]=item["remote_state"].as_str().map(|v|json!(v)).unwrap_or_else(||json!("open"));
            if let Some(source)=pause_source(item){next["paused_from"]=json!(source);}
            let was_hidden=before.as_ref().is_some_and(|row|row["remote_state"]=="hidden");
            let revision_changed=before.as_ref().is_some_and(|row|row["current_revision_id"]!=revision_id);
            let mut events=Vec::new();
            if before.is_none(){events.push(leases::planned_event(competition_id,"challenge",&cid,"competition.challenge.discovered",command_id,json!({"challenge_id":cid,"external_challenge_id":external,"name":next["name"],"category":next["category"]})));}
            if let Some(revision)=&new_revision{events.push(leases::planned_event(competition_id,"challenge",&cid,"competition.revision.created",command_id,json!({"challenge_id":cid,"revision_id":revision_id,"revision_seq":revision["revision_seq"]})));}
            let remote_changed=before.as_ref().is_none_or(|row|row["remote_state"]!=next["remote_state"]);
            if remote_changed || revision_changed{events.push(leases::planned_event(competition_id,"challenge",&cid,"competition.challenge.state_changed",command_id,json!({"challenge_id":cid,"from":before.as_ref().map(|row|row["remote_state"].clone()),"to":next["remote_state"],"revision_id":revision_id,"previous_revision_id":before.as_ref().map(|row|row["current_revision_id"].clone()),"reason":if hidden{"remote_locked"}else if was_hidden{"remote_unlocked"}else if next["remote_state"]=="solved_remote"{"remote_solved"}else{"revision_changed"}})));}
            if before.as_ref()==Some(&next){push(&mut report,"unchanged",&cid);applied=true;break;}
            next["updated_at"]=json!(now());
            let mut expected=vec![(CHALLENGES,cid.as_str(),before.as_ref())];let mut records=vec![(CHALLENGES,cid.as_str(),&next)];
            if let Some(revision)=&new_revision{let rid=field(revision,"revision_id");expected.push((REVISIONS,rid,None));records.push((REVISIONS,rid,revision));}
            if !put_records_with_events(state,competition_id,&expected,&records,&events)?{continue;}
            if before.is_none(){push(&mut report,"added",&cid);}else if revision_changed{push(&mut report,"changed",&cid);}else{push(&mut report,"unchanged",&cid);}
            if hidden && !was_hidden{push(&mut report,"locked",&cid);}else if was_hidden && !hidden{push(&mut report,"unlocked",&cid);}
            if remote_changed && next["remote_state"]=="solved_remote"{push(&mut report,"solved",&cid);}
            applied=true;break;
        }
        if !applied{return Err(ApiError::new(StatusCode::CONFLICT,"competition.sync.projection_pending","Challenge changed repeatedly while applying the snapshot"));}
        report["synced_challenges"]=json!(report["synced_challenges"].as_u64().unwrap_or(0)+1);
    }
    if snapshot_complete{for candidate in scoped(state,CHALLENGES,competition_id)?{
        let cid=field(&candidate,"challenge_id");let mut retired=false;
        for _ in 0..32{let Some(before)=state.store.get(CHALLENGES,cid).map_err(ApiError::internal)?else{retired=true;break};if before["tombstoned"]==true||seen.contains(field(&before,"external_challenge_id")){retired=true;break;}
            let mut next=before.clone();next["tombstoned"]=json!(true);next["state"]=json!("retired");next["updated_at"]=json!(now());let event=leases::planned_event(competition_id,"challenge",cid,"competition.challenge.state_changed",command_id,json!({"challenge_id":cid,"from":before["state"],"to":"retired","reason":"remote_removed"}));
            if put_records_with_events(state,competition_id,&[(CHALLENGES,cid,Some(&before))],&[(CHALLENGES,cid,&next)],&[event])?{push(&mut report,"removed",cid);retired=true;break;}
        }if !retired{return Err(ApiError::new(StatusCode::CONFLICT,"competition.sync.projection_pending","Challenge changed repeatedly while retiring removed snapshot entries"));}
    }}
    Ok(report)
}
