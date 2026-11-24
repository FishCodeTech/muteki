//! Task-bound live WebSocket and SSE sessions. The source implementation uses
//! direct protocol clients; this adapter likewise reports network_enforced=false.
//! Every connect is checked against the current Task, policy and approval, and
//! all captured frames are stored as Task artifacts.
use super::{
    Api, Error, now, policy, put_artifact, rec, records, require, save_rec, atomic_records, task, verify_ref,
};
use crate::AppState;
use axum::http::StatusCode;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
struct SseState{response:reqwest::Response,buffer:Vec<u8>}
enum Protocol{
    Ws{send:Mutex<futures_util::stream::SplitSink<Ws,Message>>,receive:Mutex<futures_util::stream::SplitStream<Ws>>},
    Sse(Mutex<SseState>),
}
struct Connection{protocol:Protocol,cancel:tokio::sync::watch::Sender<bool>}
impl Connection{
    fn websocket(stream:Ws)->Self{let(send,receive)=stream.split();Self{protocol:Protocol::Ws{send:Mutex::new(send),receive:Mutex::new(receive)},cancel:tokio::sync::watch::channel(false).0}}
    fn sse(response:reqwest::Response)->Self{Self{protocol:Protocol::Sse(Mutex::new(SseState{response,buffer:Vec::new()})),cancel:tokio::sync::watch::channel(false).0}}
}
fn live_key(task:&str,stream:&str)->String{json!([task,stream]).to_string()}
struct OpeningEntry { task:String, token:String, cancel:tokio::sync::watch::Sender<bool> }
fn openings()->&'static std::sync::Mutex<HashMap<String,OpeningEntry>> {
    static OPENINGS:OnceLock<std::sync::Mutex<HashMap<String,OpeningEntry>>>=OnceLock::new();
    OPENINGS.get_or_init(||std::sync::Mutex::new(HashMap::new()))
}
/// Own a dispatched handshake until its connection and receipt are published together.
/// Cancellation, caller drop and artifact persistence errors retain an unknown outcome.
struct Opening { state:Arc<AppState>, task:String, stream:String, token:String, cancel:tokio::sync::watch::Receiver<bool>, armed:bool }
impl Opening {
    fn new(state:Arc<AppState>,task:&str,stream:&str)->Self {
        let(cancel,receive)=tokio::sync::watch::channel(false);let token=Uuid::new_v4().to_string();
        openings().lock().unwrap_or_else(|error|error.into_inner()).insert(live_key(task,stream),OpeningEntry{task:task.into(),token:token.clone(),cancel});
        Self{state,task:task.into(),stream:stream.into(),token,cancel:receive,armed:true}
    }
}
impl Drop for Opening {
    fn drop(&mut self){
        let mut entries=openings().lock().unwrap_or_else(|error|error.into_inner());
        let key=live_key(&self.task,&self.stream);
        if entries.get(&key).is_some_and(|entry|entry.token==self.token){entries.remove(&key);}
        drop(entries);
        if self.armed {
            let result=self.state.store.document_scope(||->Api<()>{
                if let Some(row)=intent_row(&self.state,&self.task,&self.stream)? {
                    let mut intent=row["payload"].clone();
                    if intent["state"]=="dispatched" {
                        intent["state"]=json!("outcome_unknown");intent["error_code"]=json!("stream_handshake_interrupted");
                        store_intent(&self.state,&self.task,&self.stream,intent)?;
                        execution_receipt(&self.state,&self.task,&self.stream,"outcome_unknown",&Value::Null)?;
                    }
                }Ok(())
            });
            if !matches!(result,Ok(Ok(()))){tracing::error!(task_id=%self.task,stream_id=%self.stream,"Could not preserve interrupted stream handshake");}
        }
    }
}
fn authorization_request(intent:&Value)->Value{
    let body=&intent["body"];
    json!({"invocation_id":intent["stream_id"],"task_id":intent["task_id"],"thread_id":intent["thread_id"],
        "tool_name":format!("src_{}_connect",intent["protocol"].as_str().unwrap_or("")),"action":"connect","resource_kind":"http",
        "arguments":{"url":intent["url"],"method":"GET","protocol":intent["protocol"],"headers":body["headers"]},
        "identity_ref":body["identity_ref"].as_str().unwrap_or("default"),"identity_revision":intent["identity_revision"],
        "environment_id":"src-stream-direct","environment_generation":intent["policy_version"],"policy_version":intent["policy_version"],"arguments_hash":intent["request_digest"]})
}
fn current_authority(s:&AppState,id:&str,intent:&Value)->Api<Value>{
    let p=super::super::execution_runtime::current_authority(s,id,intent["thread_id"].as_str(),&intent["policy_version"],&intent["policy_digest"])?;
    let body=&intent["body"];
    scope(s,id,require(intent,"url")?,require(intent,"protocol")?,body["authorization_ref"].as_str().unwrap_or(""))?;
    let identity=body["identity_ref"].as_str().unwrap_or("default");
    let revision=super::super::execution_runtime::gateway_identity_revision(s,id,identity)?;
    if intent["identity_revision"].as_u64().unwrap_or(0)!=revision {
        return Err(Error::new(StatusCode::CONFLICT,"authorization_stale","stream identity changed before publication"));
    }
    Ok(p)
}

fn live()->&'static Mutex<HashMap<String,Arc<Connection>>>{static LIVE:OnceLock<Mutex<HashMap<String,Arc<Connection>>>>=OnceLock::new();LIVE.get_or_init(||Mutex::new(HashMap::new()))}
fn open_timeout(body:&Value)->Api<Duration>{let value=match body.get("open_timeout").or_else(||body.get("connect_timeout")).or_else(||body.get("timeout")){None=>20.,Some(value)=>value.as_f64().ok_or_else(||Error::invalid("stream open_timeout must be numeric"))?};if !value.is_finite()||value<=0.{return Err(Error::invalid("stream open_timeout must be positive finite seconds"));}Duration::try_from_secs_f64(value).map_err(|_|Error::invalid("stream timeout is out of range"))}

fn digest(v: &Value) -> String {
    crate::runs::src_api::digest(v)
}
fn stable(prefix: &str, text: &str) -> String {
    format!("{prefix}{}", &digest(&json!(text))[..24])
}
fn raw_ref(s: &AppState, id: &str, bytes: &[u8], name: &str, media_type: &str) -> Api<Value> {
    put_artifact(s, id, bytes, name, media_type)
}
fn scope(s: &AppState, id: &str, url: &str, protocol: &str, _authorization_ref: &str) -> Api<Value> {
    let parsed = crate::runs::src_api::parse_url(url)?;
    if (protocol == "websocket" && !matches!(parsed.scheme(), "ws" | "wss"))
        || (protocol == "sse" && !matches!(parsed.scheme(), "http" | "https"))
    {
        return Err(Error::invalid("stream URL scheme does not match protocol"));
    }
    let t = task(s, id)?;
    if t["lifecycle"]["state"] != "active" {
        return Err(Error::conflict("SRC Task is not active"));
    }
    let p = policy(s, id)?;
    let allowed = t["input"]["authorization"]["targets"]
        .as_array()
        .is_some_and(|rows| {
            rows.iter().any(|v| {
                v.as_str()
                    .is_some_and(|a| crate::runs::src_api::target_matches(url, a))
            })
        });
    if !allowed || !crate::runs::src_api::in_policy(&p, url) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "stream target outside current Task scope",
        ));
    }
    Ok(p)
}
fn intent_row(s: &AppState, id: &str, stream_id: &str) -> Api<Option<Value>> {
    Ok(records(s, id, "protocol_stream_intent")?
        .into_iter()
        .find(|v| v["natural_key"] == stream_id))
}
fn store_intent(s: &AppState, id: &str, stream_id: &str, value: Value) -> Api<Value> {
    let revision = intent_row(s, id, stream_id)?
        .and_then(|v| v["revision"].as_u64())
        .unwrap_or(0);
    Ok(save_rec(
        s,
        id,
        "protocol_stream_intent",
        stream_id,
        value,
        Some(revision),
    )?["payload"]
        .clone())
}
fn execution_receipt(
    s: &AppState,
    id: &str,
    stream_id: &str,
    state: &str,
    approval: &Value,
) -> Api<()> {
    let previous = records(s, id, "execution_receipt")?
        .into_iter()
        .find(|v| v["natural_key"] == stream_id);
    let value = json!({"invocation_id":stream_id,"task_id":id,"dispatch_state":state,
        "outcome_known":state=="completed","approval":approval,"updated_at":now(),
        "external_receipt_ref":if state=="completed"{json!(stream_id)}else{Value::Null}});
    save_rec(
        s,
        id,
        "execution_receipt",
        stream_id,
        value,
        Some(previous.and_then(|v| v["revision"].as_u64()).unwrap_or(0)),
    )?;
    Ok(())
}
fn approval_for(s: &AppState, id: &str, stream_id: &str) -> Api<Option<Value>> {
    Ok(records(s, id, "execution_approval")?
        .into_iter()
        .find(|v| v["payload"]["invocation_id"] == stream_id)
        .map(|v| v["payload"].clone()))
}
fn reserve(s: &AppState, id: &str, protocol: &str, body: &Value, p: &Value) -> Api<Value> {
    let url = require(body, "url")?;
    let identity = body["identity_ref"].as_str().unwrap_or("default");
    let key = body["idempotency_key"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("");
    let stream_id = body["stream_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if key.is_empty() {
                format!(
                    "s{}_{:x}",
                    if protocol == "sse" { "se" } else { "ws" },
                    Uuid::new_v4().simple()
                )
            } else {
                stable(
                    if protocol == "sse" { "sse_" } else { "sws_" },
                    &format!("{id}:{protocol}:{key}"),
                )
            }
        });
    let request_digest = digest(
        &json!({"url":url,"protocol":protocol,"identity_ref":identity,
        "identity_revision":body["identity_revision"],"authorization_ref":body["authorization_ref"],
        "headers":body["headers"],"subprotocols":body["subprotocols"],"replay_of":body["replay_of"],"open_timeout":open_timeout(body)?.as_secs_f64()}),
    );
    if let Some(row) = intent_row(s, id, &stream_id)? {
        let mut value = row["payload"].clone();
        if value["request_digest"] != request_digest {
            return Err(Error::conflict(
                "stream idempotency key used for another request",
            ));
        }
        if matches!(value["state"].as_str(),Some("ready"|"waiting_approval"))&&(value["identity_revision"].is_null()||value["thread_id"].is_null()){
            let t=task(s,id)?;
            let revision=super::super::execution_runtime::gateway_identity_revision(s,id,identity)?;
            if body["identity_revision"].as_u64().filter(|value|*value>0).is_some_and(|expected|expected!=revision){return Err(Error::new(StatusCode::CONFLICT,"authorization_stale","legacy stream identity changed"));}
            value["thread_id"]=t["binding"]["thread_id"].clone();value["identity_revision"]=json!(revision);
            store_intent(s,id,&stream_id,value.clone())?;
        }
        return Ok(value);
    }
    let previous = body["replay_of"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(|v| rec(s, id, "protocol_stream", v))
        .transpose()?;
    if previous
        .as_ref()
        .is_some_and(|v| v["payload"]["protocol"] != protocol)
    {
        return Err(Error::invalid("replay stream protocol changed"));
    }
    let root = previous
        .as_ref()
        .and_then(|v| v["payload"]["root_stream_id"].as_str())
        .unwrap_or(&stream_id)
        .to_owned();
    let index = previous
        .as_ref()
        .and_then(|v| v["payload"]["connection_index"].as_u64())
        .unwrap_or(0)
        + 1;
    let t=task(s,id)?;
    let identity_revision=super::super::execution_runtime::gateway_identity_revision(s,id,identity)?;
    if body["identity_revision"].as_u64().filter(|revision|*revision!=0).is_some_and(|revision|revision!=identity_revision){return Err(Error::new(StatusCode::CONFLICT,"authorization_stale","stream identity revision is stale"));}
    let request=json!({"invocation_id":stream_id,"task_id":id,"tool_name":format!("src_{protocol}_connect"),"action":"connect","resource_kind":"http","arguments":{"url":url,"method":"GET","protocol":protocol,"headers":body["headers"]},"identity_ref":identity,"identity_revision":identity_revision,"environment_id":"src-stream-direct","environment_generation":p["version"],"arguments_hash":request_digest});
    let mut evaluated=crate::runs::src_api::execution_policy::evaluate(p,&request)?;crate::runs::src_api::execution_policy::apply_grant(s,id,p,&request,&mut evaluated)?;
    let decision=evaluated["effect"].as_str().unwrap_or("deny");let matched=&evaluated["matched_rule_refs"];
    if decision == "deny" {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "policy_denied",
            "stream connect denied by policy",
        ));
    }
    let mut approval = Value::Null;
    if decision == "ask" {
        let approval_id = stable("sapproval_", &format!("{id}:{stream_id}"));
        let t = task(s, id)?;
        approval = json!({"approval_id":approval_id,"invocation_id":stream_id,"input_digest":request_digest,
            "request_snapshot":{"invocation_id":stream_id,"task_id":id,"thread_id":t["binding"]["thread_id"],
                "run_id":body["run_id"],"worker_id":null,"tool_name":format!("src_{protocol}_connect"),"action":"connect",
                "arguments":{"url":url,"protocol":protocol,"stream_id":stream_id},"arguments_hash":request_digest,
                "resource_kind":"http","identity_ref":identity,"identity_revision":identity_revision,
                "environment_id":"src-stream-direct","environment_generation":p["version"],"policy_version":p["version"]},
            "matched_rule_refs":matched,"policy_version":p["version"],"status":"pending",
            "requested_at":now(),"expires_at":(chrono::Utc::now()+chrono::Duration::minutes(15)).to_rfc3339(),
            "decided_by":"","decided_at":null,"decision":null,"grant_id":null,"consumed_at":null,"revision":1});
        save_rec(
            s,
            id,
            "execution_approval",
            &approval_id,
            approval.clone(),
            Some(0),
        )?;
    }
    let mut stored_body = body.clone();
    stored_body["stream_id"] = json!(stream_id);
    let value = json!({"stream_id":stream_id,"task_id":id,"protocol":protocol,"url":url,"body":stored_body,
        "root_stream_id":root,"connection_index":index,"request_digest":request_digest,
        "policy_version":p["version"],"policy_digest":p["digest"],"thread_id":t["binding"]["thread_id"],"identity_revision":identity_revision,"state":if decision=="ask"{"waiting_approval"}else{"ready"},
        "approval_id":approval["approval_id"],"created_at":now()});
    store_intent(s, id, &stream_id, value.clone())?;
    execution_receipt(
        s,
        id,
        &stream_id,
        if decision == "ask" {
            "waiting_approval"
        } else {
            "ready"
        },
        &approval,
    )?;
    Ok(value)
}
fn headers(body: &Value) -> Api<HeaderMap> {
    let mut out = HeaderMap::new();
    for (key, value) in body["headers"].as_object().into_iter().flatten() {
        let name = HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| Error::invalid("stream header name invalid"))?;
        let value = HeaderValue::from_str(
            value
                .as_str()
                .ok_or_else(|| Error::invalid("stream header value invalid"))?,
        )
        .map_err(|_| Error::invalid("stream header value invalid"))?;
        out.insert(name, value);
    }
    Ok(out)
}
async fn open_ws(url: &str, body: &Value) -> Api<(Connection, Value, Value)> {
    let mut request = url
        .into_client_request()
        .map_err(|_| Error::invalid("WebSocket request invalid"))?;
    for (key, value) in headers(body)? {
        if let Some(key) = key {
            request.headers_mut().insert(key, value);
        }
    }
    if let Some(protocols) = body["subprotocols"].as_array() {
        let joined = protocols
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| Error::invalid("subprotocol must be text"))
            })
            .collect::<Api<Vec<_>>>()?
            .join(", ");
        if !joined.is_empty() {
            request.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_str(&joined)
                    .map_err(|_| Error::invalid("subprotocol invalid"))?,
            );
        }
    }
    let request_view = json!({"method":"GET","url":url,"headers":request.headers().iter().map(|(k,v)|json!([k.as_str(),v.to_str().unwrap_or("[binary]")])).collect::<Vec<_>>(),"representation":"derived-handshake-fields"});
    let (stream, response) = tokio::time::timeout(open_timeout(body)?, connect_async(request))
        .await
        .map_err(|_| {
            Error::new(
                StatusCode::GATEWAY_TIMEOUT,
                "src.stream_timeout",
                "WebSocket handshake timed out",
            )
        })?
        .map_err(|e| {
            Error::new(
                StatusCode::BAD_GATEWAY,
                "src.stream_connect_failed",
                e.to_string(),
            )
        })?;
    let response_view = json!({"status_code":response.status().as_u16(),"headers":response.headers().iter().map(|(k,v)|json!([k.as_str(),v.to_str().unwrap_or("[binary]")])).collect::<Vec<_>>(),"representation":"derived-handshake-fields"});
    Ok((Connection::websocket(stream), request_view, response_view))
}
async fn open_sse(url: &str, body: &Value, last_event_id: &str) -> Api<(Connection, Value, Value)> {
    let mut headers = headers(body)?;
    headers.insert("Accept", HeaderValue::from_static("text/event-stream"));
    if !last_event_id.is_empty() {
        headers.insert(
            "Last-Event-ID",
            HeaderValue::from_str(last_event_id)
                .map_err(|_| Error::invalid("Last-Event-ID invalid"))?,
        );
    }
    let client = crate::network_proxy::direct_client_builder()
        .redirect(Policy::none())
        .connect_timeout(open_timeout(body)?)
        .build()
        .map_err(crate::runs::src_api::db)?;
    let request = client
        .get(url)
        .headers(headers.clone())
        .build()
        .map_err(crate::runs::src_api::db)?;
    let request_view = json!({"method":"GET","url":url,"headers":headers.iter().map(|(k,v)|json!([k.as_str(),v.to_str().unwrap_or("[binary]")])).collect::<Vec<_>>(),"representation":"derived-request-fields"});
    let response = tokio::time::timeout(open_timeout(body)?, client.execute(request))
        .await
        .map_err(|_| {
            Error::new(
                StatusCode::GATEWAY_TIMEOUT,
                "src.stream_timeout",
                "SSE handshake timed out",
            )
        })?
        .map_err(|e| {
            Error::new(
                StatusCode::BAD_GATEWAY,
                "src.stream_connect_failed",
                e.to_string(),
            )
        })?;
    if !response.status().is_success() {
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "src.sse_http_error",
            format!("SSE endpoint returned HTTP {}", response.status()),
        ));
    }
    if !response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"))
    {
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "src.sse_content_type_invalid",
            "SSE endpoint did not return text/event-stream",
        ));
    }
    let response_view = json!({"status_code":response.status().as_u16(),"headers":response.headers().iter().map(|(k,v)|json!([k.as_str(),v.to_str().unwrap_or("[binary]")])).collect::<Vec<_>>(),"http_version":format!("{:?}",response.version()),"representation":"derived-response-fields"});
    Ok((
        Connection::sse(response),
        request_view,
        response_view,
    ))
}
pub(super) async fn connect(
    s: Arc<AppState>,
    id: &str,
    protocol: &str,
    body: &Value,
) -> Api<Value> {
    let url = require(body, "url")?;
    let p = policy(&s, id)?;
    let default_auth_ref = format!(
        "{}:v{}",
        p["policy_id"].as_str().unwrap_or("src-policy"),
        p["version"].as_u64().unwrap_or(0)
    );
    let authorization_ref = body["authorization_ref"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(&default_auth_ref)
        .to_owned();
    let p = scope(&s, id, url, protocol, &authorization_ref)?;
    let intent=s.store.document_scope(||reserve(&s,id,protocol,body,&p)).map_err(crate::runs::src_api::db)??;
    let stream_id = require(&intent, "stream_id")?;
    let fence=connect_fence(id,stream_id);let _fence=fence.lock().await;
    if let Some(row) = records(&s, id, "protocol_stream")?
        .into_iter()
        .find(|v| v["natural_key"] == stream_id)
    {
        return Ok(row["payload"].clone());
    }
    if intent["state"] == "dispatched" || intent["state"] == "outcome_unknown" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "external_outcome_unknown",
            "stream handshake outcome must be reconciled before retry",
        ));
    }
    let mut opening=Opening::new(s.clone(),id,stream_id);
    loop {
        current_authority(&s,id,&intent)?;
        let approval=approval_for(&s,id,stream_id)?;
        let Some(approval)=approval.filter(|approval|approval["status"]=="pending")else{break};
        if approval["expires_at"].as_str().and_then(|value|chrono::DateTime::parse_from_rfc3339(value).ok()).is_none_or(|value|value<=chrono::Utc::now()){
            return Err(Error::new(StatusCode::REQUEST_TIMEOUT,"approval_expired","stream connection approval expired"));
        }
        tokio::select!{
            _=opening.cancel.changed()=>return Err(Error::new(StatusCode::CONFLICT,"authorization_stale","stream authorization was revoked")),
            _=tokio::time::sleep(Duration::from_millis(250))=>{}
        }
    }
    let approval=s.store.document_scope(||->Api<Value>{
        let p=current_authority(&s,id,&intent)?;
        let row=intent_row(&s,id,stream_id)?.ok_or_else(||Error::conflict("stream intent disappeared"))?;
        let mut dispatched=row["payload"].clone();
        if !matches!(dispatched["state"].as_str(),Some("ready"|"waiting_approval")){
            return Err(Error::conflict("stream invocation is no longer available for dispatch"));
        }
        let approval=if dispatched["state"]=="waiting_approval"{
            super::super::execution_runtime::consume_approval(&s,id,require(&dispatched,"approval_id")?,&p)?
        }else{
            let request=authorization_request(&dispatched);
            let mut decision=super::super::execution_policy::evaluate(&p,&request)?;
            super::super::execution_policy::apply_grant(&s,id,&p,&request,&mut decision)?;
            if decision["effect"]!="allow"||decision["status"]=="unresolved"{return Err(Error::new(StatusCode::FORBIDDEN,"authorization_stale","stream grant changed before dispatch"));}
            Value::Null
        };
        dispatched["state"]=json!("dispatched");
        store_intent(&s,id,stream_id,dispatched)?;
        execution_receipt(&s,id,stream_id,"dispatched",&approval)?;
        Ok(approval)
    }).map_err(crate::runs::src_api::db)??;
    if *opening.cancel.borrow(){return Err(Error::new(StatusCode::CONFLICT,"authorization_stale","stream authorization was revoked"));}
    let last_event_id = body["replay_of"]
        .as_str()
        .filter(|v| !v.is_empty())
        .and_then(|v| rec(&s, id, "protocol_stream", v).ok())
        .and_then(|v| v["payload"]["last_event_id"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let opened = tokio::select!{
        biased;
        _=opening.cancel.changed()=>Err(Error::new(StatusCode::CONFLICT,"authorization_stale","stream authorization was revoked during handshake")),
        result=async{if protocol=="websocket"{open_ws(url,body).await}else{open_sse(url,body,&last_event_id).await}}=>result,
    };
    let (connection, request_view, response_view) = match opened {
        Ok(v) => v,
        Err(error) => {
            let mut state = intent_row(&s, id, stream_id)?.unwrap()["payload"].clone();
            state["state"] = json!("outcome_unknown");
            s.store.document_scope(||->Api<()>{
            store_intent(&s, id, stream_id, state)?;
            execution_receipt(
                &s,
                id,
                stream_id,
                "outcome_unknown",
                &approval,
            )?;
                Ok(())
            }).map_err(crate::runs::src_api::db)??;
            return Err(error);
        }
    };
    let request_ref = raw_ref(
        &s,
        id,
        serde_json::to_vec(&request_view)
            .map_err(crate::runs::src_api::db)?
            .as_slice(),
        &format!("{stream_id}-handshake-request.json"),
        "application/json",
    )?;
    let response_ref = raw_ref(
        &s,
        id,
        serde_json::to_vec(&response_view)
            .map_err(crate::runs::src_api::db)?
            .as_slice(),
        &format!("{stream_id}-handshake-response.json"),
        "application/json",
    )?;
    let value = json!({"stream_id":stream_id,"root_stream_id":intent["root_stream_id"],
        "connection_index":intent["connection_index"],"protocol":protocol,"url":url,"task_id":id,
        "run_id":body["run_id"],"step_id":body["step_id"],"identity_ref":body["identity_ref"].as_str().unwrap_or("default"),
        "identity_revision":intent["identity_revision"],"authorization_ref":authorization_ref,
        "state":"connected","capture_state":"complete","network_enforced":false,"opened_at":now(),"closed_at":null,
        "next_seq":0,"request_ref":request_ref,"response_ref":response_ref,"replay_of":body["replay_of"],
        "last_event_id":last_event_id,"subprotocol":response_view["headers"].as_array().into_iter().flatten().find(|row|row[0].as_str().is_some_and(|value|value.eq_ignore_ascii_case("sec-websocket-protocol"))).map(|row|row[1].clone()),"content_encoding":response_view["headers"].as_array().into_iter().flatten().find(|row|row[0].as_str().is_some_and(|value|value.eq_ignore_ascii_case("content-encoding"))).map(|row|row[1].clone()),"gaps":[],"frames":[]});
    let mut connections=live().lock().await;
    s.store.document_scope(||->Api<()>{
    current_authority(&s,id,&intent)?;
    let pending=intent_row(&s,id,stream_id)?.ok_or_else(||Error::conflict("stream intent disappeared"))?;
    if *opening.cancel.borrow()||pending["payload"]["state"]!="dispatched"{return Err(Error::new(StatusCode::CONFLICT,"authorization_stale","stream handshake was superseded"));}
    save_rec(&s, id, "protocol_stream", stream_id, value.clone(), Some(0))?;
    let mut settled = intent_row(&s, id, stream_id)?.unwrap()["payload"].clone();
    settled["state"] = json!("completed");
    store_intent(&s, id, stream_id, settled)?;
    execution_receipt(
        &s,
        id,
        stream_id,
        "completed",
        &approval,
    )?;
        Ok(())
    }).map_err(crate::runs::src_api::db)??;
    connections.insert(live_key(id,stream_id),Arc::new(connection));
    opening.armed=false;
    Ok(value)
}
pub(super) async fn after_approval(
    s: Arc<AppState>,
    id: &str,
    approval: &Value,
) -> Result<(), String> {
    let tool = approval["request_snapshot"]["tool_name"]
        .as_str()
        .unwrap_or("");
    if !matches!(tool, "src_websocket_connect" | "src_sse_connect") {
        return Ok(());
    }
    let stream_id = approval["request_snapshot"]["arguments"]["stream_id"]
        .as_str()
        .unwrap_or("");
    if stream_id.is_empty() {
        return Err("stream approval has no stream_id".into());
    }
    if approval["status"] == "denied" {
        let row = intent_row(&s, id, stream_id)
            .map_err(|e| e.message)?
            .ok_or("stream intent missing")?;
        let mut value = row["payload"].clone();
        value["state"] = json!("denied");
        store_intent(&s, id, stream_id, value).map_err(|e| e.message)?;
        execution_receipt(&s, id, stream_id, "denied", approval).map_err(|e| e.message)?;
        return Ok(());
    }
    let row = intent_row(&s, id, stream_id)
        .map_err(|e| e.message)?
        .ok_or("stream intent missing")?;
    let protocol = row["payload"]["protocol"]
        .as_str()
        .ok_or("stream protocol missing")?;
    connect(s, id, protocol, &row["payload"]["body"])
        .await
        .map(|_| ())
        .map_err(|e| e.message)
}

fn append_frame(s:&AppState,id:&str,stream_id:&str,template:&Value)->Api<Value>{
    for _ in 0..32{
        let row=rec(s,id,"protocol_stream",stream_id)?;let mut stream=row["payload"].clone();let seq=stream["next_seq"].as_u64().unwrap_or(0)+1;
        let mut value=template.clone();value["stream_id"]=json!(stream_id);value["root_stream_id"]=stream["root_stream_id"].clone();value["connection_index"]=stream["connection_index"].clone();value["seq"]=json!(seq);
        stream["next_seq"]=json!(seq);if value["has_id"]==true{stream["last_event_id"]=value["id"].clone();}
        if !stream["frames"].is_array(){stream["frames"]=json!([]);}stream["frames"].as_array_mut().unwrap().push(value.clone());
        let key=format!("{stream_id}:{seq}");
        match atomic_records(s,id,&[("protocol_stream_event",&key,value.clone(),Some(0)),("protocol_stream",stream_id,stream,row["revision"].as_u64())]){
            Ok(true)=>return Ok(value),Ok(false)=>continue,Err(error)if error.status==StatusCode::CONFLICT=>continue,Err(error)=>return Err(error)
        }
    }Err(Error::conflict("stream sequence changed repeatedly; captured payload remains preserved"))
}
fn event(s:&AppState,id:&str,stream_id:&str,direction:&str,payload:&[u8],binary:bool,state:&str,error:&str)->Api<Value>{
    let reference=raw_ref(s,id,payload,&format!("{stream_id}-{}",Uuid::new_v4().simple()),if binary{"application/octet-stream"}else{"text/plain; charset=utf-8"})?;
    append_frame(s,id,stream_id,&json!({"direction":direction,"opcode":if binary{2}else{1},"payload_ref":reference,"payload_size":payload.len(),"state":state,"error":error,"captured_at":now(),"server_acknowledged":false,"business_effect":"unverified","payload":if binary{Value::Null}else{json!(String::from_utf8_lossy(payload).to_string())}}))
}

fn closed(s:&AppState,id:&str,stream_id:&str,reason:&str,abnormal:bool)->Api<Value>{closed_code(s,id,stream_id,reason,if abnormal{1006}else{1000},abnormal)}
fn closed_code(s: &AppState, id: &str, stream_id: &str, reason: &str,code:u16, abnormal: bool) -> Api<Value> {
    let row = rec(s, id, "protocol_stream", stream_id)?;
    let mut value = row["payload"].clone();
    if value["state"] == "closed" {
        return Ok(value);
    }
    value["state"] = json!("closed");
    value["closed_at"] = json!(now());
    value["close"] = json!({"code":code,"reason":reason});
    if abnormal {
        value["capture_state"] = json!("partial");
        value["gaps"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind":"abnormal_disconnect","code":code,"reason":reason}));
    }
    save_rec(
        s,
        id,
        "protocol_stream",
        stream_id,
        value.clone(),
        row["revision"].as_u64(),
    )?;
    Ok(value)
}
fn cancelled(s:&AppState,id:&str,stream:&str)->Api<Value>{
    let value=rec(s,id,"protocol_stream",stream)?["payload"].clone();
    if value["state"]=="connected"{return Err(Error::new(StatusCode::CONFLICT,"stream_closed","stream has been retired; its durable closure is still pending"));}
    Ok(value)
}
pub(super) async fn send_ws(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    let stream_id = require(body, "stream_id")?;
    let record = rec(&s, id, "protocol_stream", stream_id)?;
    if record["payload"]["protocol"] != "websocket" || record["payload"]["state"] != "connected" {
        return Err(Error::conflict("WebSocket is not connected"));
    }
    let holder =
        live().lock().await.get(&live_key(id,stream_id)).cloned().ok_or_else(|| {
            Error::conflict("WebSocket connection did not survive process restart")
        })?;
    let binary = body["payload_b64"].is_string();
    let bytes = if binary {
        base64::engine::general_purpose::STANDARD
            .decode(body["payload_b64"].as_str().ok_or_else(||Error::invalid("payload_b64 must be text"))?)
            .map_err(|_| Error::invalid("payload_b64 invalid"))?
    } else {
        body["payload"].as_str().ok_or_else(||Error::invalid("payload must be text"))?.as_bytes().to_vec()
    };
    let message = if binary {
        Message::Binary(bytes.clone().into())
    } else {
        Message::Text(String::from_utf8_lossy(&bytes).to_string().into())
    };
    let Protocol::Ws{send,..}=&holder.protocol else{return Err(Error::conflict("stream protocol changed"));};
    let mut cancel=holder.cancel.subscribe();
    let mut sender=send.lock().await;
    let intent=intent_row(&s,id,stream_id)?.ok_or_else(||Error::conflict("stream intent disappeared"))?;
    s.store.document_scope(||current_authority(&s,id,&intent["payload"])).map_err(crate::runs::src_api::db)??;
    if *cancel.borrow(){return Err(Error::new(StatusCode::CONFLICT,"authorization_stale","stream has been revoked"));}
    let result=tokio::select!{
        biased;
        _=cancel.changed()=>return event(&s,id,stream_id,"outbound",&bytes,binary,"outcome_unknown","authorization_revoked_during_send"),
        result=sender.send(message)=>result,
    };
    let error=result.as_ref().err().map(ToString::to_string).unwrap_or_default();
    event(&s,id,stream_id,"outbound",&bytes,binary,if result.is_ok(){"sent"}else{"outcome_unknown"},&error)

}
pub(super) async fn receive_ws(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    let stream_id = require(body, "stream_id")?;
    rec(&s, id, "protocol_stream", stream_id)?;
    let holder = live()
        .lock()
        .await
        .get(&live_key(id,stream_id))
        .cloned()
        .ok_or_else(|| Error::conflict("WebSocket connection is not open"))?;
    let Protocol::Ws{receive,..}=&holder.protocol else{return Err(Error::conflict("stream protocol changed"));};
    let mut cancel=holder.cancel.subscribe();if *cancel.borrow(){return cancelled(&s,id,stream_id);}let mut ws=receive.lock().await;if *cancel.borrow(){return cancelled(&s,id,stream_id);}

    loop {
        let frame=tokio::select!{frame=ws.next()=>frame,_=cancel.changed()=>return cancelled(&s,id,stream_id)};
        match frame {
            Some(Ok(Message::Text(value))) => {
                return event(
                    &s,
                    id,
                    stream_id,
                    "inbound",
                    value.as_bytes(),
                    false,
                    "received","",
                );
            }
            Some(Ok(Message::Binary(value))) => {
                return event(&s, id, stream_id, "inbound", &value, true, "received","");
            }
            Some(Ok(Message::Close(frame))) => {
                live().lock().await.remove(&live_key(id,stream_id));
                return closed_code(&s,id,stream_id,frame.as_ref().map(|v|v.reason.as_ref()).unwrap_or("closed"),frame.as_ref().map(|v|u16::from(v.code)).unwrap_or(1005),frame.as_ref().is_none_or(|v|u16::from(v.code)!=1000));
            }
            Some(Ok(_)) => continue,
            Some(Err(error)) => {
                live().lock().await.remove(&live_key(id,stream_id));
                return closed(&s, id, stream_id, &error.to_string(), true);
            }
            None => {
                live().lock().await.remove(&live_key(id,stream_id));
                return closed(&s, id, stream_id, "eof_without_close_frame", true);
            }
        }
    }
}
pub(super) async fn close(s: Arc<AppState>, id: &str, body: &Value, protocol: &str) -> Api<Value> {
    let stream_id = require(body, "stream_id")?;
    let row = rec(&s, id, "protocol_stream", stream_id)?;
    if row["payload"]["protocol"] != protocol {
        return Err(Error::invalid("stream protocol mismatch"));
    }
    let code=body.get("code").map(|value|value.as_u64().filter(|value|*value<=u16::MAX as u64).map(|value|value as u16).ok_or_else(||Error::invalid("WebSocket close code must be an integer"))).transpose()?.unwrap_or(1000);
    let reason=body["reason"].as_str().unwrap_or("");
    if protocol=="websocket" && (!matches!(code,1000|1001|1002|1003|1007|1008|1009|1010|1011|1012|1013|1014|3000..=4999) || reason.len()>123){return Err(Error::invalid("Invalid WebSocket close code or reason"));}
    let holder=live().lock().await.remove(&live_key(id,stream_id));
    let mut error=None;
    if let Some(holder)=&holder{if let Protocol::Ws{send,..}=&holder.protocol{
        let frame=tokio_tungstenite::tungstenite::protocol::CloseFrame{code:code.into(),reason:reason.into()};
        match tokio::time::timeout(open_timeout(body)?,async{send.lock().await.send(Message::Close(Some(frame))).await}).await{Ok(Ok(()))=>{},Ok(Err(failure))=>error=Some(failure.to_string()),Err(_)=>error=Some("WebSocket close delivery timed out".into())}
    }}
    let value=closed_code(&s,id,stream_id,error.as_deref().unwrap_or(reason),if error.is_some(){1006}else{code},error.is_some() || code!=1000)?;
    if let Some(holder)=holder{let _=holder.cancel.send_replace(true);}Ok(value)

}
fn sse_frame(buffer:&mut Vec<u8>)->Option<Vec<u8>>{
    let mut start=0;let mut index=0;
    while index<buffer.len(){let size=match buffer[index]{b'\r' if index+1==buffer.len()=>return None,b'\r' if buffer.get(index+1)==Some(&b'\n')=>2,b'\r'|b'\n'=>1,_=>{index+=1;continue;}};let empty=index==start;index+=size;if empty{return Some(buffer.drain(..index).collect());}start=index;}None
}
pub(super) async fn read_sse(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    let stream_id = require(body, "stream_id")?;
    let row = rec(&s, id, "protocol_stream", stream_id)?;
    if row["payload"]["protocol"] != "sse" || row["payload"]["state"] != "connected" {
        return Err(Error::conflict("SSE connection is not open"));
    }
    let holder = live()
        .lock()
        .await
        .get(&live_key(id,stream_id))
        .cloned()
        .ok_or_else(|| Error::conflict("SSE connection did not survive process restart"))?;
    let Protocol::Sse(connection)=&holder.protocol else{return Err(Error::conflict("stream protocol changed"));};
    let mut cancel=holder.cancel.subscribe();if *cancel.borrow(){return cancelled(&s,id,stream_id);}let mut connection=connection.lock().await;if *cancel.borrow(){return cancelled(&s,id,stream_id);}
    let SseState{response,buffer}=&mut *connection;
    let raw = loop {
        if let Some(frame) = sse_frame(buffer) {
            break frame;
        }
        let chunk_result=tokio::select!{chunk=response.chunk()=>chunk,_=cancel.changed()=>return cancelled(&s,id,stream_id)};
        let chunk = match chunk_result {
            Ok(value) => value,
            Err(error) => {
                if !buffer.is_empty() {
                    raw_ref(
                        &s,
                        id,
                        buffer,
                        &format!("{stream_id}-partial.sse"),
                        "text/event-stream",
                    )?;
                }
                live().lock().await.remove(&live_key(id,stream_id));
                return closed(&s, id, stream_id, &error.to_string(), true);
            }
        };
        match chunk {
            Some(bytes) => buffer.extend_from_slice(&bytes),
            None => {
                let partial = !buffer.is_empty();
                if partial {
                    raw_ref(
                        &s,
                        id,
                        buffer,
                        &format!("{stream_id}-unterminated.sse"),
                        "text/event-stream",
                    )?;
                }
                live().lock().await.remove(&live_key(id,stream_id));
                return closed(
                    &s,
                    id,
                    stream_id,
                    if partial { "unterminated_event" } else { "eof" },
                    partial,
                );
            }
        }
        if buffer.len() > 8 * 1024 * 1024 {
            live().lock().await.remove(&live_key(id,stream_id));
            return closed(&s, id, stream_id, "sse_frame_limit", true);
        }
    };
    drop(connection);
    let current = rec(&s, id, "protocol_stream", stream_id)?;
    let stream = current["payload"].clone();
    let seq = stream["next_seq"].as_u64().unwrap_or(0) + 1;
    let reference = raw_ref(
        &s,
        id,
        &raw,
        &format!("{stream_id}-{seq}.sse"),
        "text/event-stream; charset=utf-8",
    )?;
    let parsed=parse_sse(&raw);let event_name=parsed["event"].as_str().unwrap_or("message");let event_id=parsed["id"].as_str().unwrap_or("");let retry=&parsed["retry"];
    let duplicate = if event_id.is_empty() {
        Value::Null
    } else {
        records(&s, id, "protocol_stream_event")?
            .into_iter()
            .find(|v| {
                v["payload"]["root_stream_id"] == stream["root_stream_id"]
                    && v["payload"]["id"] == event_id
            })
            .map(|v| {
                json!(format!(
                    "{}:{}",
                    v["payload"]["stream_id"].as_str().unwrap_or(""),
                    v["payload"]["seq"].as_u64().unwrap_or(0)
                ))
            })
            .unwrap_or(Value::Null)
    };
    let value = json!({"stream_id":stream_id,"root_stream_id":stream["root_stream_id"],"connection_index":stream["connection_index"],
        "seq":seq,"direction":"inbound","state":"received","payload_ref":reference,
        "event":event_name,"id":event_id,"retry":retry,
        "payload":parsed["data"],"has_id":parsed["has_id"],"comments":parsed["comments"],"parse_errors":parsed["parse_errors"],"data_sha256":parsed["data_sha256"],"captured_at":now(),"delivery_state":if duplicate.is_null(){"received"}else{"replayed"},
        "duplicate_of":duplicate,"server_acknowledged":false,"business_effect":"unverified"});
    append_frame(&s,id,stream_id,&value)

}
pub(super) fn acknowledge(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let stream_id = require(body, "stream_id")?;
    rec(s, id, "protocol_stream", stream_id)?;
    let seq = body["event_seq"]
        .as_u64()
        .ok_or_else(|| Error::invalid("event_seq required"))?;
    let key = format!("{stream_id}:{seq}");
    let row = rec(s, id, "protocol_stream_event", &key)?;
    let refs = body["evidence_refs"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::invalid("acknowledgement requires response evidence refs"))?;
    for reference in refs {
        if reference.is_object() {
            verify_ref(s, id, reference)?;
        }
    }
    let acknowledgement_id=stable("sack_",&format!("{id}:{key}"));
    if let Ok(prior)=rec(s,id,"protocol_ack",&acknowledgement_id){if prior["payload"]["evidence_refs"]==json!(refs){return Ok(prior["payload"].clone());}return Err(Error::conflict("Acknowledgement identity was reused with other evidence"));}
    let mut event=row["payload"].clone();event["server_acknowledged"]=json!(true);event["ack_evidence_refs"]=json!(refs);
    let acknowledgement=json!({"acknowledgement_id":acknowledgement_id,"stream_id":stream_id,"event_seq":seq,"evidence_refs":refs,"recorded_at":now()});
    if !atomic_records(s,id,&[("protocol_stream_event",&key,event,row["revision"].as_u64()),("protocol_ack",&acknowledgement_id,acknowledgement.clone(),Some(0))])?{return Err(Error::conflict("Stream acknowledgement changed during commit"));}Ok(acknowledgement)

}
pub(super) fn export(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let root = require(body, "root_stream_id").or_else(|_| require(body, "stream_id"))?;
    rec(s, id, "protocol_stream", root)?;
    let mut streams = records(s, id, "protocol_stream")?
        .into_iter()
        .filter(|v| v["payload"]["root_stream_id"] == root)
        .map(|v| v["payload"].clone())
        .collect::<Vec<_>>();
    let mut events = records(s, id, "protocol_stream_event")?
        .into_iter()
        .filter(|v| v["payload"]["root_stream_id"] == root)
        .map(|v| v["payload"].clone())
        .collect::<Vec<_>>();
    streams.sort_by_key(|row|row["connection_index"].as_u64().unwrap_or(0));events.sort_by_key(|row|(row["connection_index"].as_u64().unwrap_or(0),row["seq"].as_u64().unwrap_or(0)));
    let gaps = streams
        .iter()
        .flat_map(|v| {
            v["gaps"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|gap| json!({"stream_id":v["stream_id"],"gap":gap}))
        })
        .collect::<Vec<_>>();
    let complete = streams.iter().all(|v| v["capture_state"] == "complete");
    Ok(
        json!({"schema":"muteki.src.protocol-stream.v1","root_stream_id":root,
        "root":rec(s,id,"protocol_stream",root)?["payload"],"connections":streams,"events":events,
        "gaps":gaps,"complete":complete,"network_enforced":false,"artifact_identity":"artifact_id"}),
    )
}
pub(super) async fn quiesce(s: Arc<AppState>, id: &str, reason: &str) -> Api<()> {
    for entry in openings().lock().unwrap_or_else(|error|error.into_inner()).values().filter(|entry|entry.task==id){entry.cancel.send_replace(true);}
    {
        let mut connections=live().lock().await;
        let keys=connections.keys().filter(|key|serde_json::from_str::<Value>(key).is_ok_and(|parts|parts[0]==id)).cloned().collect::<Vec<_>>();
        for key in keys{if let Some(holder)=connections.remove(&key){holder.cancel.send_replace(true);}}
    }
    for row in records(&s,id,"protocol_stream_intent")?{
        let mut intent=row["payload"].clone();
        if matches!(intent["state"].as_str(),Some("ready"|"waiting_approval"|"dispatched")){
            let state=if intent["state"]=="dispatched"{"outcome_unknown"}else{"denied"};
            intent["state"]=json!(state);intent["error_code"]=json!(reason);
            let stream=require(&intent,"stream_id")?.to_owned();
            s.store.document_scope(||->Api<()>{store_intent(&s,id,&stream,intent)?;execution_receipt(&s,id,&stream,state,&Value::Null)}).map_err(crate::runs::src_api::db)??;
        }
    }
    for row in records(&s, id, "protocol_stream")? {
        if row["payload"]["state"] != "connected" {
            continue;
        }
        let stream_id = row["payload"]["stream_id"].as_str().unwrap_or("");
        let holder=live().lock().await.remove(&live_key(id,stream_id));
        closed(&s,id,stream_id,reason,reason=="process_restart")?;
        if let Some(holder)=holder{let _=holder.cancel.send_replace(true);}
    }
    Ok(())
}
pub(super) async fn recover(s: Arc<AppState>, id: &str) -> Api<()> {
    for row in records(&s,id,"protocol_stream_intent")?{
        if row["payload"]["state"]=="dispatched"{
            let mut intent=row["payload"].clone();let stream=require(&intent,"stream_id")?.to_owned();
            intent["state"]=json!("outcome_unknown");intent["error_code"]=json!("process_restart");
            s.store.document_scope(||->Api<()>{store_intent(&s,id,&stream,intent)?;execution_receipt(&s,id,&stream,"outcome_unknown",&Value::Null)}).map_err(crate::runs::src_api::db)??;
        }
    }
    for row in records(&s, id, "protocol_stream")? {
        if row["payload"]["state"] != "connected" {
            continue;
        }
        let stream_id = row["payload"]["stream_id"].as_str().unwrap_or("");
        if !live().lock().await.contains_key(&live_key(id,stream_id)) {
            closed(&s, id, stream_id, "process_restart", true)?;
        }
    }
    Ok(())
}

fn connect_fence(task:&str,stream:&str)->Arc<Mutex<()>>{
    static FENCES:OnceLock<std::sync::Mutex<HashMap<String,std::sync::Weak<Mutex<()>>>>>=OnceLock::new();let mut rows=FENCES.get_or_init(||std::sync::Mutex::new(HashMap::new())).lock().unwrap_or_else(|error|error.into_inner());rows.retain(|_,value|value.strong_count()>0);let key=live_key(task,stream);if let Some(fence)=rows.get(&key).and_then(std::sync::Weak::upgrade){return fence;}let fence=Arc::new(Mutex::new(()));rows.insert(key,Arc::downgrade(&fence));fence
}
fn parse_sse(raw:&[u8])->Value{
    use sha2::{Digest,Sha256};let raw=raw.strip_prefix(&[0xef,0xbb,0xbf]).unwrap_or(raw);let mut data=Vec::new();let mut event="message".to_owned();let mut id=String::new();let mut has_id=false;let mut retry=Value::Null;let mut comments=Vec::new();let mut errors=Vec::new();
    let mut lines=Vec::new();let mut start=0;let mut at=0;while at<raw.len(){if matches!(raw[at],b'\r'|b'\n'){lines.push(&raw[start..at]);if raw[at]==b'\r'&&raw.get(at+1)==Some(&b'\n'){at+=1;}at+=1;start=at;}else{at+=1;}}if start<raw.len(){lines.push(&raw[start..]);}
    for line in lines{if line.is_empty()||line.starts_with(b":"){comments.push(String::from_utf8_lossy(line).into_owned());continue;}
        let(name,value)=if let Some(at)=line.iter().position(|byte|*byte==b':'){(&line[..at],line[at+1..].strip_prefix(b" ").unwrap_or(&line[at+1..]))}else{(line,&[][..])};
        match name{b"data"=>data.push(value.to_vec()),b"event"=>event=String::from_utf8_lossy(value).into_owned(),b"id"=>{if value.contains(&0){errors.push("id_contains_null");}else{match std::str::from_utf8(value){Ok(value)=>{id=value.to_owned();has_id=true;},Err(_)=>errors.push("id_invalid_utf8")}}},b"retry"=>match std::str::from_utf8(value).ok().and_then(|value|value.parse::<i64>().ok()){Some(value)if value>=0=>retry=json!(value),Some(_)=>errors.push("retry_negative"),None=>errors.push("retry_not_integer")},_=>{}}
    }
    let joined=data.join(&b'\n');json!({"event":event,"id":id,"has_id":has_id,"retry":retry,"data":String::from_utf8_lossy(&joined),"data_sha256":format!("{:x}",Sha256::digest(&joined)),"comments":comments,"parse_errors":errors})
}
