//! Task browser and Worker gateway HTTP inspection requests.
use super::{
    Api, Error, RECORDS, browser_runtime, db, now, policy, put_artifact, rec, records, require,
    save_rec, stage_rec, task, worker_gateway,
};
use crate::AppState;
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::SystemTime,
};

static SETTLED_TRAFFIC: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, u64)>>> = OnceLock::new();

fn traffic_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

fn settled_traffic_unchanged(path: &Path, stamp: Option<(SystemTime, u64)>) -> bool {
    stamp.is_some_and(|stamp| {
        SETTLED_TRAFFIC
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(path)
            .is_some_and(|saved| *saved == stamp)
    })
}

fn remember_settled_traffic(path: &Path, stamp: Option<(SystemTime, u64)>) {
    if let Some(stamp) = stamp {
        SETTLED_TRAFFIC
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(path.to_owned(), stamp);
    }
}

struct Context {
    root: PathBuf,
    policy: Value,
    channel: &'static str,
}
fn context(s:&AppState,id:&str,request_id:&str)->Api<Context>{
    for(root,channel)in [(worker_gateway::gateway_root(s,id)?,"worker"),(browser_runtime::root(s,id)?.join("gateway"),"browser")]{
        if root.join("inspection_requests").join(format!("{request_id}.json")).is_file(){
            let policy=read_json(&root.join("policy.json")).ok_or_else(||Error::invalid("inspection policy must be a bounded regular JSON file"))?;
            if policy["src_task_id"]!=id&&policy["task_id"]!=id{return Err(Error::conflict("inspection gateway belongs to another Task"));}
            let _ = super::policy(s, id)?;
            return Ok(Context{root,policy,channel});
        }
    }
    Err(Error::new(StatusCode::NOT_FOUND,"src.inspection_request_missing","inspection request unavailable"))
}

fn request_id(body: &Value) -> Api<&str> {
    let id = require(body, "request_id")?;
    if id.len() != 32 || !id.bytes().all(|v| v.is_ascii_hexdigit()) {
        return Err(Error::invalid("invalid inspection request_id"));
    }
    Ok(id)
}
fn read_request(s: &AppState, id: &str, request_id: &str) -> Api<(Value, Context)> {
    let context = context(s, id, request_id)?;
    let root = &context.root;
    let request=read_json(&root.join("inspection_requests").join(format!("{request_id}.json"))).ok_or_else(||Error::invalid("inspection request must be a bounded regular JSON file"))?;
    if request["request_id"] != request_id || !matches!(request["state"].as_str(),Some("pending"|"approved"|"denied")) {
        return Err(Error::conflict("inspection request is no longer pending"));
    }
    let facts = &request["facts"];
    let fact_digest = format!(
        "{:x}",
        Sha256::digest(browser_runtime::canonical_json(facts).as_bytes())
    );
    if request["digest"] != fact_digest {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "inspection request facts digest changed",
        ));
    }
    browser_runtime::target_scope(s, id, require(facts, "url")?)?;
    if request["request_raw_ref"] != format!("traffic/{request_id}.request") {
        return Err(Error::invalid("inspection raw request path invalid"));
    }
    Ok((request, context))
}
fn public_request(s: &AppState, id: &str, request: &Value, context: &Context) -> Api<Value> {
    let request_id = require(request, "request_id")?;
    let path = context
        .root
        .join("traffic")
        .join(format!("{request_id}.request"));
    let bytes = read_bounded(&path,32*1024*1024).ok_or_else(|| {
        Error::new(
            StatusCode::NOT_FOUND,
            "src.inspection_request_missing",
            "inspection raw request unavailable",
        )
    })?;
    let artifact = put_artifact(
        s,
        id,
        &bytes,
        &format!("{request_id}-inspection-request.http"),
        "message/http",
    )?;
    let facts = &request["facts"];
    let headers = facts["headers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let name = v.get(0)?.as_str()?;
            let value = v.get(1)?.as_str()?;
            let sensitive = matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "proxy-authorization" | "cookie" | "cookie2"
            );
            Some(json!([name, if sensitive { "[redacted]" } else { value }]))
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"request_id":request_id,"state":request["state"],"task_id":id,
        "url":facts["url"],"method":facts["method"],"path":facts["path"],"headers":headers,
        "request_raw_ref":artifact,
        "channel":context.channel,
        "matched_rule_refs":request["matched_rule_refs"],
        "reusable_approval":request["reusable_approval"]==true,
        "requested_at":request["requested_at"],
        "approval_mode":if request["reusable_approval"]==true
            {"once_or_task_scope"}else{"once_only"}}),
    )
}
pub(super) fn requests(s:&AppState,id:&str)->Api<Value>{
    sync_task(s,id)?;let mut items=Vec::new();
    for row in records(s,id,"egress_inspection_link")?{let mut value=row["payload"].clone();let rid=require(&value,"request_id")?.to_owned();
        value["approval"]=super::super::execution_runtime::approval(s,id,&rid)?.map(|row|row["payload"].clone()).unwrap_or(Value::Null);
        value["request"]=read_request(s,id,&rid).ok().map(|(request,context)|public_request(s,id,&request,&context)).transpose()?.unwrap_or(Value::Null);value["persistent"]=json!(true);items.push(value);
    }Ok(json!(items))
}

fn grant_path(s:&AppState,id:&str,grant:&Value,grant_id:&str)->Api<PathBuf>{
    let root=if grant["gateway_channel"]=="browser"{browser_runtime::root(s,id)?.join("gateway")}else{worker_gateway::gateway_root(s,id)?};
    Ok(root.join("inspection_grants").join(format!("{grant_id}.json")))
}
fn atomic_decision(path: &Path, value: &Value) -> Api<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(value.to_string().as_bytes())?;
        file.sync_all()?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o640))?;
        fs::rename(&temporary, path)
    })();
    let _ = fs::remove_file(&temporary);
    result.map_err(crate::runs::src_api::db)
}
fn create_grant(
    s: &AppState,
    id: &str,
    request: &Value,
    context: &Context,
    body: &Value,
    actor: &str,
) -> Api<Value> {
    let refs = request["matched_rule_refs"]
        .as_array()
        .filter(|refs| !refs.is_empty())
        .ok_or_else(|| {
            Error::new(
                StatusCode::FORBIDDEN,
                "policy_denied",
                "reusable rule reference is missing",
            )
        })?;
    for reference in refs {
        let expected = reference
            .as_str()
            .ok_or_else(|| Error::invalid("invalid rule reference"))?;
        let matched = context.policy["rules"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|rule| {
                rule["enabled"] != false
                    && rule["mode"] == "enforce"
                    && rule["effect"] == "ask"
                    && rule["reusable_approval"] == true
                    && expected
                        == format!(
                            "{}:v{}",
                            rule["rule_id"].as_str().unwrap_or(""),
                            rule["version"].as_u64().unwrap_or(0)
                        )
            });
        if !matched {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "policy_denied",
                "matched rules do not allow a reusable Task grant",
            ));
        }
    }
    let selected = &body["scope"];
    for key in [
        "resource_selector",
        "identity_constraints",
        "environment_constraints",
        "input_constraints",
        "actions",
    ] {
        if !selected[key].is_null() {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "policy_denied",
                "inspection Task grant must preserve the exact approved request",
            ));
        }
    }
    let expiry = if let Some(raw) = selected["expires_at"].as_str() {
        chrono::DateTime::parse_from_rfc3339(raw)
            .map_err(|_| Error::invalid("invalid grant expires_at"))?
            .with_timezone(&Utc)
    } else {
        let ttl = selected["ttl_seconds"]
            .as_i64()
            .filter(|v| *v > 0)
            .unwrap_or(4 * 3600);
        Utc::now() + Duration::seconds(ttl)
    };
    if expiry <= Utc::now() {
        return Err(Error::invalid("grant expires_at must be in the future"));
    }
    let request_id = require(request, "request_id")?;
    let hash = format!(
        "{:x}",
        Sha256::digest(format!("{id}:{request_id}").as_bytes())
    );
    let grant_id = body["existing_grant_id"].as_str().map(str::to_owned).unwrap_or_else(||format!("sgrant_{}", &hash[..24]));
    let facts = &request["facts"];
    let resource = json!({"url":facts["url"],"path":facts["path"],"method":facts["method"],
        "identity_ref":facts["identity_ref"],"tenant":facts["tenant"]});
    let mut gateway_grant = json!({"grant_id":grant_id,"policy_digest":request["policy_digest"],
        "rule_refs":refs,"resource":resource,"facts_digest":request["digest"],
        "expires_at":expiry.timestamp_millis() as f64/1000.0,"revoked_at":null});
    gateway_grant["digest"] = json!(format!(
        "{:x}",
        Sha256::digest(browser_runtime::canonical_json(&gateway_grant).as_bytes())
    ));
    let current = policy(s, id)?;
    let payload = json!({"grant_id":grant_id,"task_id":id,"rule_refs":refs,
        "resource_selector":{"tool_name":"src.http.transparent_proxy","url":{"target":facts["url"]}},
        "actions":["request"],"identity_constraints":{"identity_ref":facts["identity_ref"]},
        "environment_constraints":{"environment_id":context.root.file_name().and_then(|v|v.to_str()).unwrap_or(""),
            "environment_generation":current["version"]},
        "input_constraints":{"facts_digest":request["digest"]},
        "source_approval_id":request_id,"created_by":actor,"created_at":now(),
        "expires_at":expiry.to_rfc3339(),"revoked_at":null,"version":1,
        "policy_digest":current["digest"],"gateway_policy_digest":request["policy_digest"],
        "inspection_request_digest":request["digest"],"gateway_grant":gateway_grant});
    let value=if let Ok(prior)=rec(s,id,"task_grant",&grant_id){
        if !prior["payload"]["inspection_request_digest"].is_null()&&prior["payload"]["inspection_request_digest"]!=request["digest"]{return Err(Error::conflict("inspection grant belongs to different request facts"));}
        let mut value=prior["payload"].clone();for field in ["gateway_grant","gateway_policy_digest","inspection_request_digest"]{value[field]=payload[field].clone();}value["gateway_channel"]=json!(context.channel);
        if value==prior["payload"]{value}else{save_rec(s,id,"task_grant",&grant_id,value,prior["revision"].as_u64())?["payload"].clone()}
    }else{save_rec(s,id,"task_grant",&grant_id,payload,Some(0))?["payload"].clone()};
    Ok(value)
}
pub(super) fn decide(s:&AppState,id:&str,body:&Value)->Api<Value>{
    let rid=request_id(body)?;sync_task(s,id)?;let approval=super::super::execution_runtime::approval(s,id,rid)?.ok_or_else(||Error::new(StatusCode::NOT_FOUND,"artifact_missing","persistent inspection approval unavailable"))?;
    let _ = task(s, id)?;
    let approved=body["approved"].as_bool().unwrap_or_else(||matches!(body["decision"].as_str(),Some("approve_once"|"approve_task_scope")));if body["approved"].as_bool().is_none()&&!matches!(body["decision"].as_str(),Some("approve_once"|"approve_task_scope"|"deny")){return Err(Error::invalid("approved or decision is required"));}
    let task_scope=body["task_scope"]==true||body["decision"]=="approve_task_scope";
    let result=super::super::decide_approval(s,id,&json!({"approval_id":approval["payload"]["approval_id"],"base_revision":approval["revision"],"decision":if !approved{"deny"}else if task_scope{"approve_task_scope"}else{"approve_once"}}))?;
    sync_task(s,id)?;Ok(result)
}

pub(super) fn revoke_grant(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let selected = body["grant_id"]
        .as_str()
        .or_else(|| body["digest"].as_str())
        .ok_or_else(|| Error::invalid("grant_id or digest is required"))?;
    let row = records(s, id, "task_grant")?
        .into_iter()
        .find(|row| {
            let grant = &row["payload"];
            grant["gateway_grant"].is_object()
                && (grant["grant_id"] == selected
                    || grant["inspection_request_digest"] == selected
                    || grant["gateway_grant"]["digest"] == selected)
        })
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.grant_not_found",
                "persistent inspection grant is unavailable",
            )
        })?;
    let _ = task(s, id)?;
    if body["base_revision"]
        .as_u64()
        .is_some_and(|v| Some(v) != row["revision"].as_u64())
    {
        return Err(Error::conflict("grant revision is stale"));
    }
    let mut grant = row["payload"].clone();
    if grant["revoked_at"].is_null() {
        grant["revoked_at"] = json!(now());
        grant["gateway_grant"]["revoked_at"] = grant["revoked_at"].clone();
        let grant_id = require(&grant, "grant_id")?.to_owned();
        let path=grant_path(s,id,&grant,&grant_id)?;
        atomic_decision(&path, &grant["gateway_grant"])?;
        save_rec(
            s,
            id,
            "task_grant",
            row["record_id"].as_str().unwrap_or(&grant_id),
            grant.clone(),
            row["revision"].as_u64(),
        )?;
    }
    Ok(json!({"grant_id":grant["grant_id"],"revoked":true,"revoked_at":grant["revoked_at"]}))
}
pub(super) fn revoke_grant_marked(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: &str,
    marker_collection: &str,
) -> Api<Value> {
    let grant_id = require(body, "grant_id")?;
    let marker = loop {
        if let Some(marker) = s.store.get(marker_collection, command_id).map_err(db)? {
            break marker;
        }
        let row = rec(s, id, "task_grant", grant_id)?;
        if !row["payload"]["gateway_grant"].is_object() {
            return Err(Error::conflict("inspection grant changed"));
        }
        let revision = row["revision"].as_u64().unwrap_or(0);
        if body["base_revision"].as_u64() != Some(revision) {
            return Err(Error::conflict("grant revision is stale"));
        }
        let mut grant = row["payload"].clone();
        grant["revoked_at"] = json!(now());
        grant["gateway_grant"]["revoked_at"] = grant["revoked_at"].clone();
        let (rid, version, staged) = stage_rec(
            s,
            id,
            "task_grant",
            row["record_id"].as_str().unwrap_or(grant_id),
            grant,
            Some(revision),
        )?;
        let previous = s.store.get(RECORDS, &rid).map_err(db)?;
        let document = json!({"version":version+1,"values":staged});
        let output = json!({"grant_id":grant_id,"revoked":true});
        let marker = json!({"command_id":command_id,"phase":"record_committed","output":output,
            "grant_id":grant_id});
        if s.store
            .put_many_if(
                &[
                    (RECORDS, &rid, previous.as_ref()),
                    (marker_collection, command_id, None),
                ],
                &[
                    (RECORDS, &rid, &document),
                    (marker_collection, command_id, &marker),
                ],
            )
            .map_err(db)?
        {
            break marker;
        }
    };
    if marker["phase"] == "completed" {
        return Ok(marker["output"].clone());
    }
    let saved = rec(s, id, "task_grant", grant_id)?;
    let path=grant_path(s,id,&saved["payload"],grant_id)?;
    atomic_decision(&path, &saved["payload"]["gateway_grant"])?;
    let mut completed = marker.clone();
    completed["phase"] = json!("completed");
    if s.store
        .put_many_if(
            &[(marker_collection, command_id, Some(&marker))],
            &[(marker_collection, command_id, &completed)],
        )
        .map_err(db)?
    {
        Ok(marker["output"].clone())
    } else {
        Ok(s.store
            .get(marker_collection, command_id)
            .map_err(db)?
            .ok_or_else(|| Error::conflict("grant effect marker disappeared"))?["output"]
            .clone())
    }
}
pub(super) fn revoke_incompatible_grants(s: &AppState, id: &str, digest: &Value) -> Api<()> {
    for row in records(s, id, "task_grant")? {
        let mut grant = row["payload"].clone();
        if !grant["gateway_grant"].is_object()
            || !grant["revoked_at"].is_null()
            || grant["policy_digest"] == *digest
        {
            continue;
        }
        grant["revoked_at"] = json!(now());
        grant["gateway_grant"]["revoked_at"] = grant["revoked_at"].clone();
        let grant_id = require(&grant, "grant_id")?.to_owned();
        let path=grant_path(s,id,&grant,&grant_id)?;
        atomic_decision(&path, &grant["gateway_grant"])?;
        save_rec(
            s,
            id,
            "task_grant",
            row["record_id"].as_str().unwrap_or(&grant_id),
            grant,
            row["revision"].as_u64(),
        )?;
    }
    Ok(())
}

fn read_bounded(path:&Path,limit:usize)->Option<Vec<u8>>{
    use std::io::Read;use std::os::unix::fs::OpenOptionsExt;
    for ancestor in path.parent()?.ancestors(){if fs::symlink_metadata(ancestor).ok()?.file_type().is_symlink(){return None}}
    let file=fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path).ok()?;let metadata=file.metadata().ok()?;if !metadata.is_file()||metadata.len()>limit as u64{return None}let mut bytes=Vec::new();file.take(limit as u64+1).read_to_end(&mut bytes).ok()?;(bytes.len()<=limit).then_some(bytes)
}
fn read_json(path:&Path)->Option<Value>{serde_json::from_slice(&read_bounded(path,32*1024*1024)?).ok()}

fn save_link(s:&AppState,id:&str,rid:&str,value:Value)->Api<()>{let prior=rec(s,id,"egress_inspection_link",rid).ok();if prior.as_ref().is_none_or(|row|row["payload"]!=value){save_rec(s,id,"egress_inspection_link",rid,value,Some(prior.as_ref().and_then(|row|row["revision"].as_u64()).unwrap_or(0)))?;}Ok(())}
fn bridge_request(s:&AppState,id:&str,rid:&str,request:&Value,context:&Context)->Api<()> {
    use base64::Engine;
    let raw_path=context.root.join("traffic").join(format!("{rid}.request"));let metadata=fs::symlink_metadata(&raw_path).map_err(db)?;
    if !metadata.is_file()||metadata.file_type().is_symlink()||metadata.len()>32*1024*1024{return Err(Error::invalid("inspection raw request is not a bounded regular file"));}
    let raw=read_bounded(&raw_path,32*1024*1024).ok_or_else(||Error::invalid("inspection raw request is not a bounded regular file"))?;let facts=&request["facts"];let arguments=json!({"url":facts["url"],"method":facts["method"],"headers":facts["headers"],"body_sha256":facts["body_sha256"],"raw_request_b64":base64::engine::general_purpose::STANDARD.encode(raw),"idempotent":false});
    let p=policy(s,id)?;let t=task(s,id)?;let resource_facts=facts.as_object().ok_or_else(||Error::invalid("inspection facts must be an object"))?.iter().map(|(name,value)|(name.clone(),json!({"value":value,"origin":"request","state":"known"}))).collect::<serde_json::Map<_,_>>();
    let execution=json!({"invocation_id":rid,"task_id":id,"thread_id":t["binding"]["thread_id"],"tool_name":"src.http.transparent_proxy","action":"request","arguments_hash":format!("{:x}",Sha256::digest(browser_runtime::canonical_json(&arguments).as_bytes())),"arguments":arguments,"resource_kind":"http","resource_facts":resource_facts,"identity_ref":facts["identity_ref"].as_str().unwrap_or("default"),"environment_id":context.root.file_name().and_then(|value|value.to_str()).unwrap_or(""),"environment_generation":p["version"],"policy_version":p["version"]});
    let timeout=context.policy["inspection_requirements"]["approval_timeout_seconds"].as_f64().unwrap_or(300.0);let remaining=(request["requested_at"].as_f64().unwrap_or(0.0)+timeout-chrono::Utc::now().timestamp_millis() as f64/1000.).ceil() as i64;
    let runtime=super::super::execution_runtime::admit(s,id,&execution,remaining.max(1))?;
    let approval=super::super::execution_runtime::approval(s,id,rid)?;let mut link=json!({"request_id":rid,"invocation_id":rid,"approval_id":runtime["approval_id"],"state":runtime["dispatch_state"],"request_digest":request["digest"],"policy_version":p["version"],"channel":context.channel});
    if runtime["dispatch_state"]=="dispatched"||matches!(runtime["dispatch_state"].as_str(),Some("completed"|"failed"|"outcome_unknown")){return save_link(s,id,rid,link)}
    if remaining<=0{if let Some(row)=approval.as_ref().filter(|row|row["payload"]["status"]=="pending"){let mut value=row["payload"].clone();value["status"]=json!("expired");save_rec(s,id,"execution_approval",require(&value,"approval_id")?,value.clone(),row["revision"].as_u64())?;}}
    if let Some(row)=&approval{if row["payload"]["status"]=="approved"&&row["payload"]["decision"]=="approve_task_scope"{
        let actor=row["payload"]["decided_by"].as_str().unwrap_or("local-operator");let grant=create_grant(s,id,request,context,&json!({"existing_grant_id":row["payload"]["grant_id"]}),actor)?;
        atomic_decision(&context.root.join("inspection_grants").join(format!("{}.json",require(&grant,"grant_id")?)),&grant["gateway_grant"])?;
    }}
    let outcome=if runtime["dispatch_state"]=="denied"{runtime}else{match super::super::execution_runtime::dispatch(s,id,rid){Ok(value)=>value,Err(error)if error.code=="approval_required"=>{link["state"]=json!("pending");return save_link(s,id,rid,link)},Err(error)=>{super::super::execution_runtime::settle(s,id,rid,Value::Null,Some(error.code),false)?;let mut value=runtime.clone();value["dispatch_state"]=json!("denied");value["error_code"]=json!(error.code);value}}};
    let approved=outcome["dispatch_state"]=="dispatched";link["state"]=json!(if approved{"approved"}else{"denied"});
    save_link(s,id,rid,link)?;
    let decision=json!({"request_id":rid,"request_digest":request["digest"],"approved":approved,"code":if approved{json!("approved")}else{outcome["error_code"].clone()},"grant_id":approval.as_ref().map(|row|row["payload"]["grant_id"].clone()),"decided_at":chrono::Utc::now().timestamp_millis() as f64/1000.});
    atomic_decision(&context.root.join("inspection_decisions").join(format!("{rid}.json")),&decision)
}
fn project_inspection_cards(s:&AppState,id:&str,invocations:&[String])->Api<()>{
    if invocations.is_empty(){return Ok(());}
    let project=|s:&AppState|->Api<()>{for invocation in invocations{super::super::execution_runtime::project_chat(s,id,invocation)?;}Ok(())};
    if crate::chat::chat_lock_held(){return project(s);}
    if let Some(handle)=tokio::runtime::Handle::try_current().ok().filter(|handle|handle.runtime_flavor()==tokio::runtime::RuntimeFlavor::MultiThread){
        return tokio::task::block_in_place(||handle.block_on(async{let _guard=s.chat.active.lock().await;project(s)}));
    }
    project(s)
}
fn archive_gateway_traffic_root(
    s: &AppState,
    id: &str,
    root: &Path,
    key_prefix: &str,
    default_source_kind: &str,
    default_actor_ref: &str,
    capture_source: &str,
    skip_if_key_present: Option<&str>,
) -> Api<usize> {
    static ARCHIVED: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, u64)>>> = OnceLock::new();
    let traffic = root.join("traffic");
    if !traffic.is_dir() {
        return Ok(0);
    }
    let current_policy = super::policy(s, id)?;
    let policy_digest = current_policy["digest"].as_str().unwrap_or("").to_owned();
    let mut entries = Vec::new();
    for entry in fs::read_dir(&traffic).map_err(db)? {
        let entry = entry.map_err(db)?;
        let path = entry.path();
        let Some(rid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
            .filter(|rid| rid.len() == 32 && rid.bytes().all(|b| b.is_ascii_hexdigit()))
        else {
            continue;
        };
        let stamp = traffic_stamp(&path);
        let unchanged = stamp.is_some_and(|st| {
            ARCHIVED
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&path)
                .is_some_and(|saved| *saved == st)
        });
        if unchanged {
            continue;
        }
        entries.push((rid.to_owned(), path, stamp));
    }
    if entries.is_empty() {
        return Ok(0);
    }
    let existing_keys: std::collections::HashSet<String> = records(s, id, "http_exchange")?
        .into_iter()
        .filter_map(|row| row["natural_key"].as_str().map(str::to_owned))
        .collect();
    let redact = |headers: &Value| {
        headers
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                let name = v.get(0)?.as_str()?;
                let value = v.get(1)?.as_str()?;
                let sensitive = matches!(
                    name.to_ascii_lowercase().as_str(),
                    "authorization"
                        | "proxy-authorization"
                        | "cookie"
                        | "cookie2"
                        | "set-cookie"
                );
                Some(json!([name, if sensitive { "[redacted]" } else { value }]))
            })
            .collect::<Vec<_>>()
    };
    let mut archived_count = 0usize;
    for (gateway_id, path, stamp) in entries {
        let Some(meta) = read_json(&path) else {
            continue;
        };
        if meta["exchange_id"].as_str() != Some(gateway_id.as_str()) {
            continue;
        }
        let key = format!("{key_prefix}:{gateway_id}");
        let status_code = meta["status_code"].as_u64();
        let context_id = meta["decision"]["identity_ref"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or("default");


        let skip_alt = skip_if_key_present
            .map(|p| format!("{p}:{gateway_id}"))
            .is_some_and(|k| existing_keys.contains(&k));
        if meta["capture_state"] == "complete" && status_code.is_some() {
            if !existing_keys.contains(&key) && !skip_alt {
                let req_rel = format!("traffic/{gateway_id}.request");
                let resp_rel = format!("traffic/{gateway_id}.response");
                let req_path = if meta["request_raw_ref"] == req_rel {
                    Some(root.join(&req_rel))
                } else if traffic.join(format!("{gateway_id}.request")).is_file() {
                    Some(traffic.join(format!("{gateway_id}.request")))
                } else {
                    None
                };
                let resp_path = if meta["response_raw_ref"] == resp_rel {
                    Some(root.join(&resp_rel))
                } else if traffic.join(format!("{gateway_id}.response")).is_file() {
                    Some(traffic.join(format!("{gateway_id}.response")))
                } else {
                    None
                };
                if let (Some(req_file), Some(resp_file)) = (req_path, resp_path) {
                    if let (Some(req_bytes), Some(resp_bytes)) = (
                        read_bounded(&req_file, 32 * 1024 * 1024),
                        read_bounded(&resp_file, 32 * 1024 * 1024),
                    ) {
                        let req_ref = put_artifact(
                            s,
                            id,
                            &req_bytes,
                            &format!("{gateway_id}-request.http"),
                            "message/http",
                        )?;
                        let resp_ref = put_artifact(
                            s,
                            id,
                            &resp_bytes,
                            &format!("{gateway_id}-response.http"),
                            "message/http",
                        )?;
                        let exchange_id = format!("sex_proxy_{gateway_id}");
                        let source_run_id = meta["source_run_id"].as_str().filter(|v| !v.is_empty());
                        let attribution = if source_run_id.is_some() {
                            "unverified_client_header"
                        } else {
                            "unknown_task_scoped_capture"
                        };
                        let value = json!({
                            "exchange_id": exchange_id,
                            "task_id": id,
                            "source_run_id": meta["source_run_id"],
                            "source_kind": default_source_kind,
                            "actor_ref": default_actor_ref,
                            "context_id": context_id,
                            "identity_revision": 0,
                            "browser_generation": 0,
                            "declared_target": meta["declared_target"],
                            "observed_url": meta["observed_url"],
                            "method": meta["method"],
                            "started_at": now(),
                            "finished_at": now(),
                            "request_raw_ref": req_ref,
                            "response_raw_ref": resp_ref,
                            "request_headers": redact(&meta["request_headers"]),
                            "response_headers": redact(&meta["response_headers"]),
                            "status_code": meta["status_code"],
                            "protocol": meta["protocol"],
                            "redirect_parent": null,
                            "replay_of": null,
                            "execution_state": "completed",
                            "capture_state": "complete",
                            "capture_limitations": meta["errors"].as_array().cloned().unwrap_or_default(),
                            "authorization_ref": policy_digest,
                            "decision_ref": null,
                            "metadata": {
                                "gateway_exchange_id": gateway_id,
                                "decision": meta["decision"],
                                "connected_ip": meta["connected_ip"],
                                "capture_source": capture_source,
                                "worker_id": meta["worker_id"],
                                "source_run_attribution": attribution
                            }
                        });
                        save_rec(s, id, "http_exchange", &key, value.clone(), Some(0))?;
                        super::traffic_index::index_exchange(s, id, &value, &req_bytes, Some(&resp_bytes))?;
                        super::advance_exchange_source(s, id, &key)?;
                        archived_count += 1;
                    }
                }
            }
            if let Some(st) = stamp {
                ARCHIVED
                    .get_or_init(Default::default)
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(path, st);
            }
        } else if matches!(meta["capture_state"].as_str(), Some("partial" | "uninspected")) {
            if meta["errors"].as_array().is_some_and(|e| !e.is_empty()) {
                let err_id = format!("segress_err_{gateway_id}");
                let rev = rec(s, id, "egress_proxy_error", &err_id)
                    .ok()
                    .and_then(|r| r["revision"].as_u64());
                let _ = save_rec(
                    s,
                    id,
                    "egress_proxy_error",
                    &err_id,
                    json!({
                        "error_id": err_id,
                        "gateway_exchange_id": gateway_id,
                        "source_run_id": meta["source_run_id"],
                        "worker_id": meta["worker_id"],
                        "declared_target": meta["declared_target"],
                        "error_code": meta["error_code"],
                        "errors": meta["errors"],
                        "recorded_at": now(),
                    }),
                    rev,
                );
            }
            if let Some(st) = stamp {
                ARCHIVED
                    .get_or_init(Default::default)
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(path, st);
            }
        }
    }
    Ok(archived_count)
}

fn archive_worker_proxy_traffic(s: &AppState, id: &str) -> Api<usize> {
    let mut total = 0usize;
    if let Ok(root) = worker_gateway::gateway_root(s, id) {
        total += archive_gateway_traffic_root(
            s,
            id,
            &root,
            "worker_proxy",
            "proxy",
            "worker-egress-gateway",
            "worker_egress_gateway",
            Some("gateway"),
        )?;
    }
    if let Ok(browser_root) = browser_runtime::root(s, id) {
        let gateway_dir = browser_root.join("gateway");
        if gateway_dir.join("traffic").is_dir() {
            total += archive_gateway_traffic_root(
                s,
                id,
                &gateway_dir,
                "gateway",
                "browser",
                "browser-gateway",
                "browser_egress_gateway",
                Some("worker_proxy"),
            )?;
        }
    }
    Ok(total)
}

pub(super) fn sync_task(s:&AppState,id:&str)->Api<Vec<String>>{
    let mut pending=Vec::new();
    for root in [worker_gateway::gateway_root(s,id)?,browser_runtime::root(s,id)?.join("gateway")]{
        let requests=root.join("inspection_requests");if requests.is_dir(){for entry in fs::read_dir(&requests).map_err(db)?{let entry=entry.map_err(db)?;let filename=entry.file_name();let Some(rid)=filename.to_str().and_then(|name|name.strip_suffix(".json")).filter(|rid|rid.len()==32&&rid.bytes().all(|byte|byte.is_ascii_hexdigit()))else{continue};
            if read_json(&root.join("inspection_decisions").join(format!("{rid}.json"))).is_some_and(|decision|decision["approved"]==false){continue}
            match read_request(s,id,rid).and_then(|(request,context)|bridge_request(s,id,rid,&request,&context)){
                Ok(())=>pending.push(rid.to_owned()),
                Err(error)=>{tracing::warn!(task_id=id,request_id=rid,code=error.code,"SRC inspection bridge could not reconcile request");
                    if let Some(row)=super::super::execution_runtime::approval(s,id,rid)?{if row["payload"]["status"]=="pending"{let mut value=row["payload"].clone();value["status"]=json!("superseded");save_rec(s,id,"execution_approval",require(&value,"approval_id")?,value.clone(),row["revision"].as_u64())?;}}
                    if let Ok(receipt)=super::super::execution_runtime::receipt(s,id,rid){if matches!(receipt["dispatch_state"].as_str(),Some("ready"|"waiting_approval"|"dispatched")){super::super::execution_runtime::settle(s,id,rid,Value::Null,Some(error.code),receipt["dispatch_state"]=="dispatched")?;}}
                    if let Some(request)=read_json(&entry.path()){let answer=json!({"request_id":rid,"request_digest":request["digest"],"approved":false,"code":error.code,"decided_at":chrono::Utc::now().timestamp_millis() as f64/1000.});atomic_decision(&root.join("inspection_decisions").join(format!("{rid}.json")),&answer)?;}
                }
            }
        }}
        let traffic=root.join("traffic");if !traffic.is_dir(){continue}for link in records(s,id,"egress_inspection_link")?{let Some(rid)=link["payload"]["request_id"].as_str()else{continue};let path=traffic.join(format!("{rid}.json"));if !path.is_file(){continue}let stamp=traffic_stamp(&path);if settled_traffic_unchanged(&path,stamp){continue}let Some(meta)=read_json(&path)else{continue};if meta["exchange_id"]!=rid{continue}
            let errors=meta["errors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>();let runtime=super::super::execution_runtime::receipt(s,id,rid)?;
            if let Some(row)=super::super::execution_runtime::approval(s,id,rid)?{if row["payload"]["status"]=="pending"&&errors.iter().any(|error|error.starts_with("approval_required")){let mut value=row["payload"].clone();value["status"]=json!("expired");save_rec(s,id,"execution_approval",require(&value,"approval_id")?,value.clone(),row["revision"].as_u64())?;super::super::execution_runtime::dispatch(s,id,rid)?;}}
            if !matches!(runtime["dispatch_state"].as_str(),Some("dispatched"|"outcome_unknown")){if matches!(runtime["dispatch_state"].as_str(),Some("completed"|"failed"|"denied")){remember_settled_traffic(&path,stamp);}continue}
            let settled=if !errors.is_empty(){Some(super::super::execution_runtime::settle(s,id,rid,meta.clone(),Some(&errors.join("; ")),meta["capture_state"]!="complete")?)}else if matches!(meta["capture_state"].as_str(),Some("complete"|"uninspected")){Some(super::super::execution_runtime::settle(s,id,rid,meta.clone(),None,false)?)}else{None};
            if settled.as_ref().is_some_and(|row|matches!(row["dispatch_state"].as_str(),Some("completed"|"failed"|"denied"))){remember_settled_traffic(&path,stamp);}
        }
    }
    archive_worker_proxy_traffic(s, id)?;
    project_inspection_cards(s,id,&pending)?;
    Ok(pending)
}
