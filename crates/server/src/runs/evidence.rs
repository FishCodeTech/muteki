//! Object ids identify artifacts; independent bytes/digests prove their integrity.
use super::*;
use std::path::Path;
use sha2::{Digest,Sha256};

pub(crate) fn artifact(state:&AppState,run_id:&str,id:&str)->Result<(Value,Value),String>{
    if id.is_empty() || !id.bytes().all(|byte|byte.is_ascii_alphanumeric() || matches!(byte,b'-'|b'_')){return Err("Invalid evidence artifact identity".into());}
    let bytes=bounded_file(&session_dir(state,run_id),&Path::new("arts").join(format!("{id}.json")),64*1024*1024)?;
    let value:Value=serde_json::from_slice(&bytes).map_err(|e|e.to_string())?;
    if value["artifact_id"]!=id{return Err("Evidence artifact identity differs from its file".into());}
    Ok((value,json!({"artifact_id":id,"size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes)),"integrity_version":1})))
}
pub(crate) fn verify(state:&AppState,run_id:&str,reference:&Value)->Result<Value,String>{
    let id=reference["artifact_id"].as_str().ok_or("Evidence artifact identity missing")?;let(value,actual)=artifact(state,run_id,id)?;
    if reference["sha256"].as_str().is_none_or(|v|v.len()!=64) || reference["sha256"]!=actual["sha256"] || reference["size"].as_u64()!=actual["size"].as_u64(){return Err("Evidence artifact is missing, changed, or has no recorded integrity proof".into());}
    Ok(value)
}
pub(super) fn attach(state:&AppState,run_id:&str,worker:Option<&str>,payload:&mut Value)->Result<(),String>{
    let id=payload["artifact_id"].as_str().ok_or("Tool event artifact missing")?;
    let(_,reference)=artifact(state,run_id,id)?;
    let run=state.store.get("runs",run_id).map_err(|e|e.to_string())?.ok_or("Tool event Run missing")?;
    let intent=worker.and_then(|worker|state.runs.handle(state,run_id).workers.lock().unwrap().get(worker).map(|worker|worker.intent_id.clone()).or_else(||graph::SharedGraph::open(state,run_id).ok()?.all_intents().into_iter().find(|row|row["worker"]==worker&&row["status"]=="claimed").and_then(|row|row["intent_id"].as_str().map(str::to_owned))));
    payload["artifact_ref"]=reference;payload["worker_id"]=json!(worker);payload["intent_id"]=json!(intent);
    payload["target_epoch"]=run.get("target_epoch").cloned().unwrap_or_else(||run["execution_generation"].clone());
    Ok(())
}
pub(super) fn recent(state:&AppState,run_id:&str,worker:&str,intent:Option<&str>,limit:usize)->Result<Value,String>{
    let run=state.store.get("runs",run_id).map_err(|e|e.to_string())?.ok_or("Run not found")?;let epoch=run.get("target_epoch").unwrap_or(&run["execution_generation"]);
    let cap=limit.clamp(1,100);
    let mut window=std::collections::VecDeque::with_capacity(cap);
    for event in persisted_event_iter(state,run_id)?.filter(|event|event["event_type"]=="tool.result" && event["solver_id"]==worker && event["payload"]["intent_id"].as_str()==intent && event["payload"]["target_epoch"]==*epoch) {
        if window.len()==cap{window.pop_front();}
        window.push_back(event);
    }
    let mut records=Vec::with_capacity(window.len());
    for event in window.into_iter().rev() {
        let reference=&event["payload"]["artifact_ref"];verify(state,run_id,reference)?;
        records.push(json!({"artifact_id":reference["artifact_id"],"bytes":reference["size"],"sha256":reference["sha256"],"tool_event_id":format!("{run_id}:{}",event["seq"]),"intent_id":intent,"target_epoch":epoch}));
    }records.reverse();Ok(json!(records))
}
pub(super) fn read(state:&AppState,run_id:&str,worker:&str,intent:Option<&str>,id:&str,include_command:bool)->Result<Value,String>{
    let run=state.store.get("runs",run_id).map_err(|e|e.to_string())?.ok_or("Run not found")?;let epoch=run.get("target_epoch").unwrap_or(&run["execution_generation"]);
    let own=persisted_event_iter(state,run_id)?.filter(|event|event["event_type"]=="tool.result" && event["solver_id"]==worker && event["payload"]["intent_id"].as_str()==intent && event["payload"]["artifact_id"]==id && event["payload"]["target_epoch"]==*epoch).last();
    let reference=own.map(|event|event["payload"]["artifact_ref"].clone()).or_else(||{
        let graph=graph::SharedGraph::open(state,run_id).ok()?;
        let active=graph.active_facts().iter().filter_map(|fact|fact["fact_seq"].as_i64()).collect::<HashSet<_>>();
        graph.events().into_iter().filter(|fact|fact["kind"]=="fact_added" && fact["verified"]==true && fact["seq"].as_i64().is_some_and(|seq|active.contains(&seq))).flat_map(|fact|fact["payload"]["evidence_provenance"]["artifact_refs"].as_array().cloned().unwrap_or_default()).find(|reference|reference["artifact_id"]==id)
    }).ok_or("Artifact is not referenced by this challenge or current Worker Step")?;
    let artifact=verify(state,run_id,&reference)?;let content=artifact["content"].as_str().ok_or("Artifact is not text")?;
    if content.contains('\0'){return Err("Artifact contains binary NUL bytes".into());}
    let mut result=json!({"ok":true,"content":content,"message":content});if include_command{result["command"]=artifact["command"].clone();}Ok(result)
}

/// Open each directory beneath the trusted root without following links, then
/// bound the read on the same descriptor used for the regular-file check.
pub(super) fn bounded_file(root:&Path,relative:&Path,limit:u64)->Result<Vec<u8>,String>{
    use std::path::Component;
    let root=root.canonicalize().map_err(|e|e.to_string())?;
    let parts=relative.components().map(|part|match part{Component::Normal(name)=>Ok(name.to_os_string()),_=>Err("Evidence path must stay beneath its root".to_owned())}).collect::<Result<Vec<_>,_>>()?;
    if parts.is_empty(){return Err("Evidence filename missing".into());}
    #[cfg(unix)]
    let file={
        use std::os::{fd::{AsRawFd,FromRawFd},unix::{ffi::OsStrExt,fs::OpenOptionsExt}};
        let mut options=OpenOptions::new();options.read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW);
        let mut directory=options.open(&root).map_err(|e|e.to_string())?;
        for (index,part) in parts.iter().enumerate(){
            let name=std::ffi::CString::new(part.as_os_str().as_bytes()).map_err(|e|e.to_string())?;
            let flags=libc::O_RDONLY|libc::O_NOFOLLOW|libc::O_CLOEXEC|if index+1<parts.len(){libc::O_DIRECTORY}else{libc::O_NONBLOCK};
            let fd=unsafe{libc::openat(directory.as_raw_fd(),name.as_ptr(),flags)};
            if fd<0{return Err(std::io::Error::last_os_error().to_string());}
            let next=unsafe{std::fs::File::from_raw_fd(fd)};
            directory=next;
        }
        directory
    };
    #[cfg(not(unix))]
    let file={
        let mut path=root.to_path_buf();
        for part in &parts{path.push(part);if fs::symlink_metadata(&path).map_err(|e|e.to_string())?.file_type().is_symlink(){return Err("Evidence links are not supported".into());}}
        std::fs::File::open(path).map_err(|e|e.to_string())?
    };
    let meta=file.metadata().map_err(|e|e.to_string())?;
    if !meta.is_file()||meta.len()>limit{return Err("Evidence is not a bounded regular file".into());}
    let mut bytes=Vec::new();file.take(limit+1).read_to_end(&mut bytes).map_err(|e|e.to_string())?;
    if bytes.len() as u64!=meta.len()||bytes.len() as u64>limit{return Err("Evidence changed during reading".into());}
    Ok(bytes)
}
pub(super) fn image(state:&AppState,run_id:&str,reference:&Value)->Result<Vec<u8>,String>{
    let id=reference["artifact_id"].as_str().ok_or("Image identity missing")?;
    let filename=reference["filename"].as_str().ok_or("Image filename missing")?;
    if !id.strip_prefix("art-").is_some_and(|suffix|!suffix.is_empty()&&suffix.bytes().all(|b|b.is_ascii_digit()))||Path::new(filename).components().count()!=1||!filename.starts_with(&format!("{id}.")){return Err("Image identity differs from its filename".into());}
    let bytes=bounded_file(&session_dir(state,run_id),&Path::new("arts").join(filename),64*1024*1024)?;
    if reference["size"].as_u64()!=Some(bytes.len() as u64)||reference["sha256"]!=format!("{:x}",Sha256::digest(&bytes)){return Err("Image evidence differs from its recorded size or digest".into());}
    if reference["mime_type"]!=crate::image_validation::mime(&bytes)?{return Err("Image evidence format differs from its decoded content".into());}
    Ok(bytes)
}
