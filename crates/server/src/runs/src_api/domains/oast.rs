//! Local OAST callback receiver with Task-scoped, immutable evidence.
use super::{
    Api, Error, credential_store, in_policy, now, policy, put_artifact, rec, records, require,
    save_rec, task,
};
use crate::AppState;
use axum::http::{HeaderMap, StatusCode};
use ring::hmac;
use serde_json::{Value, json};
use uuid::Uuid;
#[path="oast_receiver.rs"]mod receiver;

fn hmac_secret_key(s: &AppState, id: &str) -> Api<hmac::Key> {
    let reference = credential_store::managed_reference(id, "src_oast_hmac_key")?;
    let secret = if credential_store::version(s, id, &reference).is_ok() {
        credential_store::resolve(s, id, &reference, None)?
    } else {
        let generated = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        credential_store::put(
            s,
            id,
            &json!({
                "alias": "src_oast_hmac_key",
                "value": generated,
                "kind": "oast_hmac",
                "label": "SRC OAST HMAC correlation key",
            }),
        )?;
        credential_store::resolve(s, id, &reference, None)?
    };
    Ok(hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes()))
}

fn sign_token(s: &AppState, id: &str, interaction_id: &str, token: &str) -> Api<String> {
    let key = hmac_secret_key(s, id)?;
    let msg = format!("{id}:{interaction_id}:{token}");
    let tag = hmac::sign(&key, msg.as_bytes());
    Ok(hex_encode(tag.as_ref()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(raw: &str) -> Option<Vec<u8>> {
    let raw = raw.trim();
    if raw.len() % 2 != 0 || raw.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    for pair in raw.as_bytes().chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)? as u8;
        let lo = (pair[1] as char).to_digit(16)? as u8;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn interaction(s: &AppState, id: &str, interaction_id: &str) -> Api<Value> {
    rec(s, id, "oast_interaction", interaction_id)
}
fn valid_exchange_target(s: &AppState, id: &str, exchange_id: &str) -> Api<String> {
    let exchange = rec(s, id, "http_exchange", exchange_id)?["payload"].clone();
    let target = require(&exchange, "observed_url")?;
    let t = task(s, id)?;
    let p = policy(s, id)?;
    if t["lifecycle"]["state"] != "active"
        || !in_policy(&p, target)
        || !t["input"]["authorization"]["targets"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|v| {
                    v.as_str().is_some_and(|allowed| {
                        crate::runs::src_api::target_matches(target, allowed)
                    })
                })
            })
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "OAST trigger is outside current Task scope",
        ));
    }
    Ok(target.to_owned())
}

fn valid_trigger(s: &AppState, id: &str, body: &Value) -> Api<(String, Vec<String>)> {
    let t = task(s, id)?;
    if t["lifecycle"]["state"] != "active" {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "OAST Task is not active",
        ));
    }
    let mut exchange_ids = Vec::new();
    if let Some(arr) = body["trigger_exchange_ids"].as_array() {
        for item in arr {
            let eid = item
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Error::invalid("trigger_exchange_ids must contain non-empty strings"))?;
            exchange_ids.push(eid.to_owned());
        }
    } else if let Some(eid) = body["trigger_exchange_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        exchange_ids.push(eid.to_owned());
    } else {
        return Err(Error::invalid(
            "trigger_exchange_id or trigger_exchange_ids is required",
        ));
    }
    let mut first_target = String::new();
    for eid in &exchange_ids {
        let target = valid_exchange_target(s, id, eid)?;
        if first_target.is_empty() {
            first_target = target;
        }
    }
    Ok((first_target, exchange_ids))
}

pub(super) async fn allocate(state:std::sync::Arc<AppState>,id:&str,body:&Value)->Api<Value>{
    let s=state.as_ref();
    let (target, trigger_ids) = valid_trigger(s, id, body)?;
    let protocols: Vec<String> = if let Some(arr) = body.get("protocols") {
        let Some(items) = arr.as_array().filter(|a| !a.is_empty()) else {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "validation_error",
                "OAST protocols must contain http, dns, tcp or udp",
            ));
        };
        let mut parsed = Vec::with_capacity(items.len());
        for item in items {
            let Some(proto) = item.as_str() else {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "validation_error",
                    "OAST protocols must contain http, dns, tcp or udp",
                ));
            };
            if !matches!(proto, "http" | "dns" | "tcp" | "udp") {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "validation_error",
                    "OAST protocols must contain http, dns, tcp or udp",
                ));
            }
            parsed.push(proto.to_owned());
        }
        parsed
    } else {
        return Err(Error::invalid("protocols is required"));
    };
    let service_instance = body["service_instance"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("local");
    if !matches!(service_instance, "local" | "webhook" | "external") {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability_unavailable",
            format!("OAST provider {service_instance:?} is not configured"),
        ));
    }
    let external_base = body["external_base_url"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|raw| {
            let parsed = reqwest::Url::parse(raw)
                .map_err(|_| Error::invalid("external_base_url must be a valid URL"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(Error::invalid("external_base_url must be http or https"));
            }
            Ok(raw.trim_end_matches('/').to_owned())
        })
        .transpose()?;
    if service_instance != "local" && external_base.is_none() {
        return Err(Error::invalid(
            "external_base_url is required when service_instance is webhook or external",
        ));
    }
    let retention = body["retention_seconds"]
        .as_u64()
        .unwrap_or(30*86400);
    let expires_at = if let Some(raw) = body["expires_at"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let parsed = chrono::DateTime::parse_from_rfc3339(raw)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .map_err(|_| {
                Error::new(
                    StatusCode::BAD_REQUEST,
                    "validation_error",
                    "OAST expires_at must be an ISO timestamp",
                )
            })?;
        parsed.to_rfc3339()
    } else {
        return Err(Error::invalid("expires_at is required"));
    };
    let idem_key = body["idempotency_key"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let current_policy = policy(s, id)?;
    let default_auth_ref = format!(
        "{}:v{}",
        current_policy["policy_id"].as_str().unwrap_or("src-policy"),
        current_policy["version"].as_u64().unwrap_or(0)
    );
    let auth_ref = body["authorization_ref"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(&default_auth_ref);
    let request=json!({"task_id":id,"run_id":body["run_id"].as_str().unwrap_or(""),"step_id":body["step_id"].as_str().unwrap_or(""),"trigger_exchange_ids":trigger_ids,"protocols":protocols,"expires_at":expires_at,"service_instance":service_instance,"retention_seconds":retention,"authorization_ref":auth_ref});
    if let Some(key) = idem_key {
        if let Some(prior) = records(s, id, "oast_interaction")?
            .into_iter()
            .find(|row| row["payload"]["idempotency_key"].as_str() == Some(key))
        {
            let mut saved=prior["payload"]["request"].clone();if !saved.is_object(){saved=prior["payload"].clone();saved.as_object_mut().unwrap().retain(|key,_|request.get(key).is_some());}
            if saved!=request{return Err(Error::conflict("OAST idempotency key was reused with different allocation input"));}return Ok(prior["payload"].clone());
        }
    }
    let token = Uuid::new_v4().simple().to_string();
    let interaction_id = format!("soast-{}", Uuid::new_v4().simple());
    if chrono::DateTime::parse_from_rfc3339(&expires_at).map_err(crate::runs::src_api::db)?<=chrono::Utc::now(){return Err(Error::invalid("OAST expiration must be in the future"));}
    let callback_url = if let Some(ext) = external_base.as_ref() {
        format!("{ext}/{token}")
    } else {
        let receiver=receiver::start(state.clone(),"127.0.0.1",0).await?;
        format!("{}/{}",require(&receiver,"base_url")?,token)
    };
    let hmac_sig = sign_token(s, id, &interaction_id, &token)?;
    let source_health = if service_instance == "local" {
        json!({"state": "local_receiver_ready", "external_reachability": "unverified"})
    } else {
        json!({"state": "external_webhook_configured", "external_reachability": "operator_managed"})
    };
    let completeness = if service_instance == "local" {
        "local_receiver_only"
    } else {
        "external_oast_webhook"
    };
    let primary_trigger = body
        .get("trigger_exchange_id")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| {
            trigger_ids
                .first()
                .map(|id| json!(id))
                .unwrap_or(Value::Null)
        });
    let value = json!({
        "request":request,
        "interaction_id": interaction_id,
        "task_id": id,
        "run_id": body["run_id"].as_str().unwrap_or(""),
        "step_id": body["step_id"].as_str().unwrap_or(""),
        "token": token,
        "callback_url": callback_url,
        "trigger_exchange_id": primary_trigger,
        "trigger_exchange_ids": trigger_ids,
        "target_url": target,
        "authorization_ref": auth_ref,
        "service_instance": service_instance,
        "expires_at": expires_at,
        "created_at": now(),
        "retention_seconds": retention,
        "state": "active",
        "poll_cursor": 0,
        "last_successful_poll_at": null,
        "source_health": source_health,
        "protocols": protocols,
        "completeness": completeness,
        "hmac_signature": hmac_sig,
        "idempotency_key": idem_key,
    });
    if let Some(key)=idem_key{
        let allocation_key=json!([id,key]).to_string();let key_record=json!({"request":request,"interaction_id":interaction_id});
        let(storage,version,row)=super::stage_rec(s,id,"oast_interaction",&interaction_id,value.clone(),Some(0))?;let document=json!({"version":version+1,"values":row});
        if !s.store.put_many_if(&[("src_oast_allocation_keys",&allocation_key,None),(super::RECORDS,&storage,None)],&[("src_oast_allocation_keys",&allocation_key,&key_record),(super::RECORDS,&storage,&document)]).map_err(crate::runs::src_api::db)?{
            let prior=s.store.get("src_oast_allocation_keys",&allocation_key).map_err(crate::runs::src_api::db)?.ok_or_else(||Error::conflict("OAST allocation changed during admission"))?;
            if prior["request"]!=request{return Err(Error::conflict("OAST idempotency key was reused"));}
            return interaction(s,id,require(&prior,"interaction_id")?).map(|row|row["payload"].clone());
        }
    }else{save_rec(s,id,"oast_interaction",&interaction_id,value.clone(),Some(0))?;}

    Ok(value)
}

pub(super) async fn recover_local_receiver(state:std::sync::Arc<AppState>)->Api<()> {
    let s=state.as_ref();
    let active=s.store.list(crate::runs::src_api::RECORDS).map_err(crate::runs::src_api::db)?
        .into_iter().map(|stored|stored["values"].clone())
        .filter(|row|row["kind"]=="oast_interaction"&&row["payload"]["state"]=="active"&&row["payload"]["service_instance"]=="local")
        .filter(|row|row["payload"]["expires_at"].as_str().and_then(|value|chrono::DateTime::parse_from_rfc3339(value).ok()).is_some_and(|expiry|expiry>chrono::Utc::now()))
        .collect::<Vec<_>>();
    if active.is_empty(){return Ok(());}
    let saved=s.store.get("system","oast_receiver_url").map_err(crate::runs::src_api::db)?
        .and_then(|value|value.as_str().map(str::to_owned))
        .or_else(||active.first().and_then(|row|row["payload"]["callback_url"].as_str().map(str::to_owned)))
        .unwrap_or_default();
    let endpoint=url::Url::parse(&saved).ok().filter(|url|url.scheme()=="http")
        .and_then(|url|Some((url.host_str()?.parse::<std::net::IpAddr>().ok()?,url.port()?,url.origin().ascii_serialization())));
    let (base_url,recovery_error)=if let Some((host,port,base_url))=endpoint {
        match receiver::start(state.clone(),&host.to_string(),port).await {
            Ok(_) => (base_url,None),
            Err(error) => (base_url,Some(error.message)),
        }
    }else{(String::new(),Some("Persisted OAST receiver address is invalid".to_owned()))};
    for row in active {
        let Some(task_id)=row["task_id"].as_str() else{continue};
        let Some(interaction_id)=row["payload"]["interaction_id"].as_str() else{continue};
        let mut payload=row["payload"].clone();
        let same_endpoint=payload["callback_url"].as_str().is_some_and(|url|url.starts_with(&format!("{base_url}/")));
        payload["receiver_recovery"]=json!({
            "state":if !same_endpoint{"stale_endpoint"}else if recovery_error.is_some(){"bind_failed"}else{"ready"},
            "checked_at":now(),"detail":recovery_error.clone(),
        });
        if let Err(error)=save_rec(s,task_id,"oast_interaction",interaction_id,payload,row["revision"].as_u64()){
            tracing::warn!(task_id,interaction_id,code=error.code,"OAST receiver recovery state could not be saved");
        }
    }
    if let Some(error)=recovery_error {return Err(Error::new(StatusCode::SERVICE_UNAVAILABLE,"capability_unavailable",error));}
    Ok(())
}

pub(super) fn expire(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let interaction_id = require(body, "interaction_id")?;
    let row = interaction(s, id, interaction_id)?;
    let delete_token = body["delete_token"].as_bool().unwrap_or(false);
    let mut value = row["payload"].clone();
    value["state"] = json!(if delete_token { "deleted" } else { "expired" });
    value["expired_at"] = json!(now());
    save_rec(
        s,
        id,
        "oast_interaction",
        interaction_id,
        value.clone(),
        row["revision"].as_u64(),
    )?;
    Ok(value)
}
pub(super) fn health(s:&AppState,id:&str,body:&Value)->Api<Value>{
    let row=interaction(s,id,require(body,"interaction_id")?)?;
    let payload=&row["payload"];
    if payload["service_instance"]=="local"{
        return Ok(json!({
            "interaction_id":payload["interaction_id"],
            "service_instance":"local",
            "state":payload["state"],
            "source_health":payload["source_health"],
            "callback_url":payload["callback_url"],
            "completeness":payload["completeness"],
            "receiver_recovery":payload.get("receiver_recovery").cloned().unwrap_or(Value::Null),
        }));
    }
    Err(Error::new(StatusCode::SERVICE_UNAVAILABLE,"capability_unavailable","OAST health requires a registered provider"))
}
pub(super) async fn local_start(s:std::sync::Arc<AppState>,id:&str,body:&Value)->Api<Value>{
    task(&s,id)?;let host=body["host"].as_str().unwrap_or("127.0.0.1");
    let port=body.get("port").map(|value|value.as_u64().filter(|value|*value<=65535).map(|value|value as u16).ok_or_else(||Error::invalid("OAST port must be an integer between 0 and 65535"))).transpose()?.unwrap_or(0);
    receiver::start(s,host,port).await
}
pub(super) fn events(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let interaction_id = require(body, "interaction_id")?;
    interaction(s, id, interaction_id)?;
    let after = if let Some(value) = body.get("after_seq").or_else(|| body.get("after_rowid")) {
        value.as_u64().ok_or_else(|| Error::new(StatusCode::BAD_REQUEST,
            "query.arguments_invalid", "OAST event position must be a nonnegative integer"))?
    } else if let Some(cursor) = body.get("cursor") {
        cursor.as_str().and_then(|value| value.parse::<u64>().ok()).ok_or_else(||
            Error::new(StatusCode::BAD_REQUEST, "query.arguments_invalid", "OAST event cursor must be an unsigned integer"))?
    } else { 0 };
    let limit = body["limit"].as_u64().unwrap_or(100).clamp(1, 1000) as usize;
    let all = records(s, id, "oast_event")?
        .into_iter()
        .map(|v| v["payload"].clone())
        .filter(|v| v["interaction_id"] == interaction_id && v["seq"].as_u64().unwrap_or(0) > after)
        .collect::<Vec<_>>();
    let items = all.iter().take(limit).cloned().collect::<Vec<_>>();
    let next_seq = if all.len() > limit {
        items.last().and_then(|v| v["seq"].as_u64())
    } else {
        None
    };
    let next = next_seq.map(|v| v.to_string());
    let watermark = crate::runs::src_api::source_watermarks(s, id)?
        .into_iter()
        .find(|row| row["source_id"] == format!("oast:{interaction_id}"))
        .unwrap_or_else(|| json!({"source_id": format!("oast:{interaction_id}"), "next_seq": 0, "state": "ready"}));
    Ok(json!({
        "items": items,
        "events": items,
        "total": all.len(),
        "next_cursor": next,
        "next_rowid": next_seq,
        "complete": next.is_none(),
        "watermark": watermark,
        "source_health": {"state": "local_receiver_ready", "external_reachability": "unverified"}
    }))
}
pub(super) fn poll(s:&AppState,id:&str,body:&Value)->Api<Value>{
    let row=interaction(s,id,require(body,"interaction_id")?)?;
    if row["payload"]["service_instance"]=="local"{
        let mut page=events(s,id,body)?;
        if let Some(obj)=page.as_object_mut(){
            obj.insert("service_instance".into(),json!("local"));
            obj.insert("poll_cursor".into(),row["payload"]["poll_cursor"].clone());
        }
        return Ok(page);
    }
    Err(Error::new(StatusCode::SERVICE_UNAVAILABLE,"capability_unavailable","OAST interaction has no polling provider; read callback events"))
}

pub(crate) fn callback(
    s: &AppState,
    token: &str,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Api<()> {
    callback_raw(s,token,method,path,headers,body,"unknown","derived_http_body")
}
fn callback_raw(s:&AppState,token:&str,method:&str,path:&str,headers:&HeaderMap,body:&[u8],source_address:&str,representation:&str)->Api<()>{
    if token.len() != 32 || !token.bytes().all(|v| v.is_ascii_hexdigit()) {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "src.oast_token_unmatched",
            "OAST token unavailable",
        ));
    }
    if body.len() > 32 * 1024 * 1024+65536 {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "src.oast_capture_too_large",
            "OAST callback exceeds 32 MiB",
        ));
    }
    let rows = s
        .store
        .list(crate::runs::src_api::RECORDS)
        .map_err(crate::runs::src_api::db)?;
    let matches = rows
        .into_iter()
        .map(|v| v["values"].clone())
        .filter(|v| v["kind"] == "oast_interaction" && v["payload"]["token"] == token)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "src.oast_token_unmatched",
            "OAST token unavailable",
        ));
    }
    let record = &matches[0];
    let id = require(record, "task_id")?;
    let interaction = &record["payload"];
    let expired = interaction["state"] != "active"
        || interaction["expires_at"]
            .as_str()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .is_some_and(|expires| expires < chrono::Utc::now())
;
    let event_id = format!("soast_event_{}", Uuid::new_v4().simple());
    let mut artifact = put_artifact(
        s,
        id,
        body,
        &format!("{event_id}.bin"),
        "application/octet-stream",
    )?;
    artifact["original"]=json!(representation=="raw_wire");
    let safe_headers = headers
        .iter()
        .map(|(name, value)| {
            let redact = matches!(
                name.as_str(),
                "authorization" | "cookie" | "proxy-authorization"
            );
            (
                name.as_str().to_owned(),
                if redact {
                    "[redacted]".to_owned()
                } else {
                    value.to_str().unwrap_or("[binary]").to_owned()
                },
            )
        })
        .collect::<Vec<_>>();
    for _ in 0..64{
        let current=rec(s,id,"oast_interaction",require(interaction,"interaction_id")?)?;let mut next=current["payload"].clone();
        let observed=records(s,id,"oast_event")?.iter().filter(|row|row["payload"]["interaction_id"]==interaction["interaction_id"]).filter_map(|row|row["payload"]["seq"].as_u64()).max().unwrap_or(0);
        let seq=next["next_seq"].as_u64().unwrap_or(0).max(observed)+1;next["next_seq"]=json!(seq);
        let value=json!({"event_id":event_id,"interaction_id":interaction["interaction_id"],"task_id":id,"seq":seq,"protocol":"http","method":method,"path":path,"headers":safe_headers,"raw_ref":artifact,"source_address":source_address,"request_path":path,"representation":representation,"received_at":now(),"late":expired,"health_probe":false,"capture_state":if representation=="raw_wire"{"complete"}else{"partial"},"capture_limitations":if representation=="raw_wire"{json!([])}else{json!(["application_body_only","source_address_unavailable"])}});
        match super::atomic_records(s,id,&[("oast_event",&event_id,value,Some(0)),("oast_interaction",require(interaction,"interaction_id")?,next,current["revision"].as_u64())]){Ok(true)=>return Ok(()),Ok(false)=>continue,Err(error)if error.status==StatusCode::CONFLICT=>continue,Err(error)=>return Err(error)}
    }return Err(Error::conflict("OAST callback sequence is busy; retry callback delivery"));

}

pub(super) fn token_mint(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let t = task(s, id)?;
    if t["lifecycle"]["state"] != "active" {
        return Err(Error::conflict("SRC Task is not active"));
    }
    let token = Uuid::new_v4().simple().to_string();
    let interaction_id = format!("soast-{}", Uuid::new_v4().simple());
    let hmac_signature = sign_token(s, id, &interaction_id, &token)?;
    let ttl = body["ttl_seconds"].as_i64().unwrap_or(86400).clamp(60, 30 * 86400);
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(ttl)).to_rfc3339();
    let external_base = body["external_base_url"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|raw| {
            let parsed = reqwest::Url::parse(raw)
                .map_err(|_| Error::invalid("external_base_url must be a valid URL"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(Error::invalid("external_base_url must use http or https"));
            }
            Ok(raw.trim_end_matches('/').to_owned())
        })
        .transpose()?;
    let callback_url = external_base
        .as_ref()
        .map(|base| format!("{base}/{token}"))
        .unwrap_or_else(|| format!("muteki-oast://{id}/{token}"));
    let doc = json!({
        "interaction_id": interaction_id,
        "task_id": id,
        "token": token,
        "hmac_signature": hmac_signature,
        "callback_url": callback_url,
        "dns_label": format!("{token}.oast.local"),
        "service_instance": if external_base.is_some() { "external" } else { "correlated_token" },
        "state": "active",
        "expires_at": expires_at,
        "created_at": now(),
        "next_seq": 0,
        "correlation": {
            "exchange_id": body["exchange_id"],
            "draft_id": body["draft_id"],
            "finding_id": body["finding_id"],
            "run_id": body["run_id"],
            "note": body["note"].as_str().unwrap_or(""),
        },
        "protocols": body["protocols"].as_array().cloned().unwrap_or_else(|| vec![json!("http"), json!("dns")]),
    });
    save_rec(s, id, "oast_interaction", &interaction_id, doc.clone(), Some(0))?;
    Ok(doc)
}

pub(super) fn callback_ingest(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let token = require(body, "token")?.trim();
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::invalid("token must be 32 hex characters"));
    }
    let matched = records(s, id, "oast_interaction")?
        .into_iter()
        .find(|row| row["payload"]["token"] == token)
        .ok_or_else(|| Error::new(StatusCode::NOT_FOUND, "src.oast_token_unmatched", "OAST token not found in task"))?;
    let interaction_id = require(&matched["payload"], "interaction_id")?.to_owned();
    let sig_verified = if let Some(provided) = body["signature"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
        let key = hmac_secret_key(s, id)?;
        let msg = format!("{id}:{interaction_id}:{token}");
        let raw_sig = hex_decode(provided).ok_or_else(|| Error::new(StatusCode::FORBIDDEN, "oast_signature_invalid", "OAST HMAC signature malformed"))?;
        hmac::verify(&key, msg.as_bytes(), &raw_sig)
            .map_err(|_| Error::new(StatusCode::FORBIDDEN, "oast_signature_invalid", "OAST HMAC signature verification failed"))?;
        true
    } else if body["verify_signature"].as_bool() == Some(true) {
        return Err(Error::invalid("signature is required when verify_signature is true"));
    } else {
        false
    };

    let raw_bytes = if let Some(b64) = body["raw_body_b64"].as_str().filter(|v| !v.is_empty()) {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| Error::invalid("raw_body_b64 must be valid base64"))?
    } else {
        body["body"].as_str().unwrap_or("").as_bytes().to_vec()
    };
    let mut header_map = HeaderMap::new();
    if let Some(obj) = body["headers"].as_object() {
        for (k, v) in obj {
            if let (Ok(name), Some(val_str)) = (
                axum::http::HeaderName::from_bytes(k.as_bytes()),
                v.as_str(),
            ) {
                if let Ok(hv) = axum::http::HeaderValue::from_str(val_str) {
                    header_map.insert(name, hv);
                }
            }
        }
    }
    let source_address = body["source_address"].as_str().unwrap_or("external_webhook");
    let method = body["method"].as_str().unwrap_or("POST");
    let path = body["path"].as_str().unwrap_or("/webhook");
    callback_raw(s, token, method, path, &header_map, &raw_bytes, source_address, "external_webhook")?;

    let all_events = records(s, id, "oast_event")?;
    let latest_event = all_events
        .into_iter()
        .filter(|row| row["payload"]["interaction_id"] == interaction_id)
        .max_by_key(|row| row["payload"]["seq"].as_u64().unwrap_or(0));
    Ok(json!({
        "ingested": true,
        "interaction_id": interaction_id,
        "token": token,
        "hmac_verified": sig_verified,
        "correlation": matched["payload"]["correlation"],
        "event": latest_event.map(|row| row["payload"].clone()).unwrap_or(Value::Null),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[tokio::test]
    async fn local_callback_is_bound_to_authorized_exchange_and_records_late_events() {
        let state = std::sync::Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        state.store.put("system", "port", &json!(8135)).unwrap();
        let target = "http://127.0.0.1:4280/login.php";
        let created = crate::runs::src_api::create_task_inner(
            &state,
            &json!({"title":"OAST","goal":"Check callback",
            "authorization":{"targets":[target]},"idempotency_key":"oast-local-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let exchange=super::super::record_exchange(&state,id,&json!({"request_raw_b64":base64::engine::general_purpose::STANDARD.encode(b"GET /login.php HTTP/1.1\r\nHost: 127.0.0.1:4280\r\n\r\n"),
            "observed_url":target})).unwrap();
        let p = policy(&state, id).unwrap();
        let auth = format!("{}:v1", p["policy_id"].as_str().unwrap());
        let interaction = allocate(
            state.clone(),
            id,
            &json!({"trigger_exchange_id":exchange["exchange_id"],"authorization_ref":auth,
                "protocols":["http"],"expires_at":"2099-01-01T00:00:00Z"}),
        )
        .await.unwrap();
        let token = interaction["token"].as_str().unwrap();
        let iid = interaction["interaction_id"].as_str().unwrap();
        let healthy = health(&state, id, &json!({"interaction_id":iid})).unwrap();
        assert_eq!(healthy["service_instance"], "local");
        assert_eq!(healthy["source_health"]["state"], "local_receiver_ready");
        assert_eq!(poll(&state, id, &json!({"interaction_id":iid})).unwrap()["items"].as_array().unwrap().len(), 0);
        let mut headers = HeaderMap::new();
        headers.insert("cookie", "secret=hidden".parse().unwrap());
        callback(&state, token, "POST", "/callback", &headers, b"signal").unwrap();
        let page = events(&state, id, &json!({"interaction_id":iid})).unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["headers"][0][1], "[redacted]");
        assert!(page["items"][0]["raw_ref"]["artifact_id"]
            .as_str().is_some_and(|v| v.starts_with("sart_")));
        assert!(page["items"][0]["raw_ref"].get("sha512").is_none());
        expire(&state, id, &json!({"interaction_id":iid})).unwrap();
        callback(&state, token, "GET", "/late", &HeaderMap::new(), b"").unwrap();
        assert_eq!(
            events(&state, id, &json!({"interaction_id":iid,"after_seq":1})).unwrap()["items"][0]["late"],
            true
        );
    }
}
