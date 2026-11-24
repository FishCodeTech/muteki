//! A verified API transport per window. The bundled UI and its draft storage stay local.
use crate::{native::{self,Notice},policy::{BridgeError,Outcome,ScopeState}};
use serde_json::{Value,json};
use std::{collections::{HashMap,HashSet},path::{Path,PathBuf},sync::{Arc,Mutex,OnceLock},time::Duration};
use axum::{Router,body::{Body,to_bytes},extract::{State,Request,FromRequestParts,WebSocketUpgrade,ws::{WebSocket,Message}},http::{StatusCode,HeaderValue},response::{Response,IntoResponse}};
use futures_util::{StreamExt,SinkExt};
use tokio_tungstenite::tungstenite::{self,client::IntoClientRequest};
struct Connection{scope:ScopeState,transport:String,key:String,stop:Option<tokio::sync::oneshot::Sender<()>>,configuring:bool,message:String,service_version:Value,features:Value}
#[derive(Default)]struct Registry{root:PathBuf,ui:String,local:String,windows:HashMap<String,Connection>,auth:HashMap<String,String>,preferences:Value,connecting:HashSet<String>}
static REGISTRY:OnceLock<Mutex<Registry>>=OnceLock::new();
fn registry()->&'static Mutex<Registry>{REGISTRY.get().expect("desktop connections initialized")}
fn failed(code:&'static str,message:impl Into<String>)->BridgeError{BridgeError::new(code,message)}
fn origin(value:&str)->Result<String,BridgeError>{let url=crate::policy::web_url(value).filter(|url|matches!(url.path(),""|"/")&&url.query().is_none()&&url.fragment().is_none()).ok_or_else(||failed("desktop.connection_origin_invalid","服务地址须为 HTTP/HTTPS 来源，不含路径、凭据或查询参数。"))?;Ok(url.origin().ascii_serialization())}
fn auth_key(scope:&ScopeState)->String{json!([scope.origin,scope.service_id,scope.identity_id]).to_string()}
fn read(path:&Path)->Value{
    use std::io::Read;use std::os::unix::fs::OpenOptionsExt;
    let read=||->Option<Value>{let mut file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path).ok()?;let metadata=file.metadata().ok()?;if !metadata.is_file()||metadata.len()>4*1024*1024{return None}let mut bytes=Vec::new();file.by_ref().take(4*1024*1024+1).read_to_end(&mut bytes).ok()?;if bytes.len()>4*1024*1024{return None}serde_json::from_slice(&bytes).ok()};read().unwrap_or_else(||json!({}))
}
fn persist(registry:&Registry)->Result<(),BridgeError>{
    use std::os::unix::fs::{DirBuilderExt,OpenOptionsExt};use std::io::Write;
    let directory=registry.root.join("desktop");std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&directory).map_err(|error|failed("desktop.connection_store_failed",error.to_string()))?;
    let file=directory.join("connections.json");let temporary=directory.join(format!(".connections-{}.tmp",uuid::Uuid::new_v4().simple()));
    let result=(||->std::io::Result<()>{let mut output=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;output.write_all(json!({"version":1,"windows":registry.preferences,"auth":registry.auth}).to_string().as_bytes())?;output.sync_all()?;std::fs::rename(&temporary,&file)?;std::fs::File::open(&directory)?.sync_all()})();let _=std::fs::remove_file(temporary);result.map_err(|error|failed("desktop.connection_store_failed",format!("连接配置保存失败，原工作台保持可用：{error}")))
}
fn probe(origin:&str)->Result<(Value,Value),BridgeError>{
    let client=reqwest::blocking::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(20)).build().map_err(|error|failed("desktop.connection_failed",error.to_string()))?;
    let document=|path:&str|->Result<Value,BridgeError>{let response=client.get(format!("{origin}{path}")).send().map_err(|error|failed("desktop.connection_failed",error.to_string()))?.error_for_status().map_err(|error|failed("desktop.connection_failed",error.to_string()))?;if response.content_length().is_some_and(|size|size>1024*1024){return Err(failed("desktop.connection_protocol_invalid","服务探测响应过大。"));}use std::io::Read;let mut bytes=Vec::new();response.take(1024*1024+1).read_to_end(&mut bytes).map_err(|error|failed("desktop.connection_failed",error.to_string()))?;if bytes.len()>1024*1024{return Err(failed("desktop.connection_protocol_invalid","服务探测响应过大。"));}serde_json::from_slice(&bytes).map_err(|error|failed("desktop.connection_protocol_invalid",error.to_string()))};
    let health=document("/api/health")?;if !health.is_object()||!(health["app"]=="muteki-rust"||matches!(health["status"].as_str(),Some("ok"|"ready"))||health["status"]=="degraded"&&health["ready"]==true){return Err(failed("desktop.connection_service_invalid","该地址未返回 Muteki 服务身份。"));}
    let auth=document("/api/auth/status")?;if auth["session_protocol"]!=2||auth["service_id"].as_str().is_none_or(str::is_empty)||auth["identity_id"].as_str().is_none_or(str::is_empty)||!auth["auth_required"].is_boolean(){return Err(failed("desktop.connection_protocol_invalid","服务登录协议或身份信息不兼容。"));}Ok((health,auth))
}
pub fn initialize(root:&Path,ui:&str,local:&str)->Result<(),BridgeError>{
    let saved=read(&root.join("desktop/connections.json"));let auth=saved["auth"].as_object().into_iter().flatten().filter_map(|(key,value)|value.as_str().map(|value|(key.clone(),value.into()))).collect();let initial=Registry{root:root.into(),ui:ui.into(),local:local.into(),auth,preferences:saved["windows"].as_object().cloned().map(Value::Object).unwrap_or_else(||json!({})),..Registry::default()};REGISTRY.set(Mutex::new(initial)).map_err(|_|failed("desktop.connection_initialized","连接管理器已初始化。"))?;
    let selected={let registry=registry().lock().unwrap_or_else(|error|error.into_inner());let saved=&registry.preferences["main"];if saved["mode"]=="local"{local.to_owned()}else{saved["origin"].as_str().unwrap_or(local).to_owned()}};
    let original_preferences=registry().lock().unwrap_or_else(|error|error.into_inner()).preferences.clone();
    match connect_inner("main",&selected,false){Ok(_)=>Ok(()),Err(error)=>{connect_inner("main",local,false)?;let mut registry=registry().lock().unwrap_or_else(|error|error.into_inner());registry.preferences=original_preferences;persist(&registry)?;let connection=registry.windows.get_mut("main").unwrap();connection.configuring=true;connection.message=error.message;Ok(())}}
}
pub fn scope(window:&str)->ScopeState{registry().lock().unwrap_or_else(|error|error.into_inner()).windows.get(window).map(|connection|connection.scope.clone()).unwrap_or_default()}
pub fn snapshot(window:&str)->Value{let registry=registry().lock().unwrap_or_else(|error|error.into_inner());let Some(connection)=registry.windows.get(window)else{return json!({"status":"disconnected","configuring":true})};json!({"origin":connection.scope.origin,"transportOrigin":connection.transport,"connectionVersion":connection.scope.connection_version,"scopeVersion":connection.scope.scope_version,"serviceId":connection.scope.service_id,"identityId":connection.scope.identity_id,"status":"connected","configuring":connection.configuring,"message":connection.message,"serviceVersion":connection.service_version,"serviceFeatures":connection.features,"connectionMode":if connection.scope.origin==registry.local{"local"}else{"external"}})}
pub fn transport(window:&str)->Outcome{let registry=registry().lock().unwrap_or_else(|error|error.into_inner());let connection=registry.windows.get(window).ok_or_else(||failed("desktop.connection_unavailable","当前窗口尚未连接服务。"))?;Ok(json!({"origin":connection.transport,"key":connection.key}))}
pub fn token(window:&str)->Option<String>{let registry=registry().lock().unwrap_or_else(|error|error.into_inner());let connection=registry.windows.get(window)?;registry.auth.get(&auth_key(&connection.scope)).cloned()}
pub fn set_token(window:&str,token:Option<&str>)->Outcome{let mut registry=registry().lock().unwrap_or_else(|error|error.into_inner());let connection=registry.windows.get(window).ok_or_else(||failed("desktop.connection_unavailable","当前窗口尚未连接服务。"))?;let key=auth_key(&connection.scope);let previous=registry.auth.get(&key).cloned();if let Some(token)=token.filter(|value|!value.is_empty()){registry.auth.insert(key.clone(),token.into());}else{registry.auth.remove(&key);}if let Err(error)=persist(&registry){if let Some(previous)=previous{registry.auth.insert(key,previous);}else{registry.auth.remove(&key);}return Err(error)}Ok(Value::Null)}
pub fn configure(window:&str,enabled:bool)->Outcome{let mut registry=registry().lock().unwrap_or_else(|error|error.into_inner());let connection=registry.windows.get_mut(window).ok_or_else(||failed("desktop.connection_unavailable","工作台连接尚未就绪。"))?;connection.configuring=enabled;drop(registry);let state=snapshot(window);native::notify(Notice::Connection(window.into()));Ok(state)}
pub fn connect(window:&str,selected:Option<&str>)->Outcome{let selected=selected.map(str::to_owned).unwrap_or_else(||registry().lock().unwrap_or_else(|error|error.into_inner()).local.clone());let result=connect_inner(window,&selected,true);if let Err(error)=&result{if let Some(connection)=registry().lock().unwrap_or_else(|error|error.into_inner()).windows.get_mut(window){connection.configuring=true;connection.message=error.message.clone();}native::notify(Notice::Connection(window.into()));}result}
fn connect_inner(window:&str,selected:&str,flush:bool)->Outcome{
    let selected=origin(selected)?;
    {let mut registry=registry().lock().unwrap_or_else(|error|error.into_inner());if !registry.connecting.insert(window.into()){return Err(failed("desktop.connection_busy","正在连接，请稍候。"));}}
    struct Attempt(String);impl Drop for Attempt{fn drop(&mut self){registry().lock().unwrap_or_else(|error|error.into_inner()).connecting.remove(&self.0);}}let _attempt=Attempt(window.into());
    let(health,auth)=probe(&selected)?;let old_scope=scope(window);let(ui,key)={let registry=registry().lock().unwrap_or_else(|error|error.into_inner());(registry.ui.clone(),format!("{}{}",uuid::Uuid::new_v4().simple(),uuid::Uuid::new_v4().simple()))};
    let authentication_key=json!([selected,auth["service_id"],auth["identity_id"]]).to_string();
    let(transport,stop)=start_proxy(window,&selected,&ui,&key,&authentication_key)?;
    struct Candidate(Option<tokio::sync::oneshot::Sender<()>>);impl Drop for Candidate{fn drop(&mut self){if let Some(stop)=self.0.take(){let _=stop.send(());}}}let mut candidate=Candidate(Some(stop));
    if flush{crate::updates::flush_drafts().map_err(|error|failed("desktop.draft_save_failed",error.message))?;}
    struct Barrier(bool);impl Drop for Barrier{fn drop(&mut self){if self.0{crate::updates::release_barrier();}}}let _barrier=Barrier(flush);
    let commit=||->Outcome{
    let mut registry=registry().lock().unwrap_or_else(|error|error.into_inner());if registry.windows.get(window).map(|connection|&connection.scope)!=if old_scope.service_id.is_empty(){None}else{Some(&old_scope)}{return Err(failed("desktop.connection_cancelled","连接切换期间原工作台已改变。"));}
    let previous_preferences=registry.preferences.clone();registry.preferences[window]=json!({"origin":selected,"mode":if selected==registry.local{"local"}else{"external"}});if let Err(error)=persist(&registry){registry.preferences=previous_preferences;return Err(error)}
    let mut next_scope=old_scope.clone();next_scope.update(&selected,auth["service_id"].as_str().unwrap(),auth["identity_id"].as_str().unwrap());
    let next=Connection{scope:next_scope,transport,key,stop:candidate.0.take(),configuring:false,message:String::new(),service_version:health["version"].clone(),features:health["features"].clone()};let next_auth_key=auth_key(&next.scope);if next.scope.origin==registry.local&&!registry.auth.contains_key(&next_auth_key){if let Some(token)=read(&registry.root.join("desktop/session.json"))["token"].as_str().filter(|token|!token.is_empty()){registry.auth.insert(next_auth_key,token.into());}}
    let previous=registry.windows.insert(window.into(),next);drop(registry);if let Some(mut previous)=previous{if let Some(stop)=previous.stop.take(){let _=stop.send(());}}
    let snapshot=snapshot(window);native::notify(Notice::Connection(window.into()));Ok(snapshot)
    };
    if flush{crate::updates::with_draft_documents(commit).map_err(|error|failed("desktop.draft_save_failed",error))?}else{commit()}
}
pub fn new_window(window:&str,parent:&str)->Outcome{let selected=scope(parent).origin;connect_inner(window,&selected,false)}
pub fn close(window:&str){if let Some(mut connection)=registry().lock().unwrap_or_else(|error|error.into_inner()).windows.remove(window){if let Some(stop)=connection.stop.take(){let _=stop.send(());}}}

#[derive(Clone)]struct Proxy{window:String,upstream:String,ui:String,key:String,auth_key:String,client:reqwest::Client}
fn start_proxy(window:&str,upstream:&str,ui:&str,key:&str,authentication_key:&str)->Result<(String,tokio::sync::oneshot::Sender<()>),BridgeError>{
    let listener=std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0)).map_err(|error|failed("desktop.connection_transport_failed",error.to_string()))?;listener.set_nonblocking(true).map_err(|error|failed("desktop.connection_transport_failed",error.to_string()))?;let address=listener.local_addr().map_err(|error|failed("desktop.connection_transport_failed",error.to_string()))?;
    let(sender,receiver)=tokio::sync::oneshot::channel();let(ready,readiness)=std::sync::mpsc::sync_channel(1);let(window,upstream,ui,key,auth_key)=(window.to_owned(),upstream.to_owned(),ui.to_owned(),key.to_owned(),authentication_key.to_owned());
    std::thread::spawn(move||{
        let runtime=match tokio::runtime::Builder::new_current_thread().enable_all().build(){Ok(runtime)=>runtime,Err(error)=>{let _=ready.send(Err(error.to_string()));return}};
        runtime.block_on(async move{
            let listener=match tokio::net::TcpListener::from_std(listener){Ok(listener)=>listener,Err(error)=>{let _=ready.send(Err(error.to_string()));return}};
            let client=match reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(20)).build(){Ok(client)=>client,Err(error)=>{let _=ready.send(Err(error.to_string()));return}};
            let state=Arc::new(Proxy{window,upstream,ui,key,auth_key,client});let router=Router::new().fallback(proxy).with_state(state);
            if ready.send(Ok::<_,String>(())).is_err(){return}tokio::select!{_ = axum::serve(listener,router)=>{},_ = receiver=>{}}
        });
    });
    match readiness.recv_timeout(Duration::from_secs(5)){Ok(Ok(()))=>Ok((format!("http://{address}"),sender)),Ok(Err(error))=>Err(failed("desktop.connection_transport_failed",error)),Err(error)=>{let _=sender.send(());Err(failed("desktop.connection_transport_failed",format!("窗口传输未就绪：{error}")))}}
}

fn cors(mut response:Response,ui:&str)->Response{if let Ok(origin)=HeaderValue::from_str(ui){response.headers_mut().insert("access-control-allow-origin",origin);}response.headers_mut().insert("vary",HeaderValue::from_static("Origin"));response.headers_mut().insert("access-control-expose-headers",HeaderValue::from_static("*"));response}
async fn proxy(State(state):State<Arc<Proxy>>,request:Request)->Response{
    let(mut parts,body)=request.into_parts();let origin=parts.headers.get("origin").and_then(|value|value.to_str().ok());if origin!=Some(state.ui.as_str()){return StatusCode::FORBIDDEN.into_response()}
    if parts.method==axum::http::Method::OPTIONS{let mut response=StatusCode::NO_CONTENT.into_response();response.headers_mut().insert("access-control-allow-methods",HeaderValue::from_static("GET,HEAD,POST,PUT,PATCH,DELETE,OPTIONS"));if let Some(headers)=parts.headers.get("access-control-request-headers"){response.headers_mut().insert("access-control-allow-headers",headers.clone());}return cors(response,&state.ui)}
    if registry().lock().unwrap_or_else(|error|error.into_inner()).windows.get(&state.window).is_none_or(|connection|connection.key!=state.key){return cors(StatusCode::GONE.into_response(),&state.ui)}
    let raw_query=parts.uri.query().unwrap_or("");let query_key=raw_query.split('&').find_map(|pair|pair.strip_prefix("__muteki_desktop_proxy="));let key=parts.headers.get("x-muteki-desktop-proxy").and_then(|value|value.to_str().ok()).or(query_key);if key!=Some(state.key.as_str())||!parts.uri.path().starts_with("/api/"){return cors(StatusCode::FORBIDDEN.into_response(),&state.ui)}
    let query=raw_query.split('&').filter(|pair|!pair.starts_with("__muteki_desktop_proxy=")).collect::<Vec<_>>().join("&");let target=format!("{}{}{}",state.upstream,parts.uri.path(),if query.is_empty(){String::new()}else{format!("?{query}")});
    if parts.headers.get("upgrade").and_then(|value|value.to_str().ok()).is_some_and(|value|value.eq_ignore_ascii_case("websocket")){
        return match WebSocketUpgrade::from_request_parts(&mut parts,&()).await{Ok(upgrade)=>{let token=registry().lock().unwrap_or_else(|error|error.into_inner()).auth.get(&state.auth_key).cloned();upgrade.on_upgrade(move|socket|websocket(socket,target,token)).into_response()},Err(error)=>error.into_response()}
    }
    let bytes=match to_bytes(body,256*1024*1024).await{Ok(bytes)=>bytes,Err(_)=>return cors(StatusCode::PAYLOAD_TOO_LARGE.into_response(),&state.ui)};let mut outbound=state.client.request(parts.method.clone(),target).body(bytes);
    for(name,value)in &parts.headers{if !matches!(name.as_str(),"host"|"cookie"|"authorization"|"origin"|"referer"|"connection"|"transfer-encoding"|"content-length"|"x-muteki-desktop-proxy")&&!name.as_str().starts_with("sec-"){outbound=outbound.header(name,value);}}
    if let Some(token)=registry().lock().unwrap_or_else(|error|error.into_inner()).auth.get(&state.auth_key).cloned(){outbound=outbound.bearer_auth(token);}let upstream=match outbound.send().await{Ok(response)=>response,Err(error)=>return cors((StatusCode::BAD_GATEWAY,axum::Json(json!({"error":{"code":"desktop.transport_failed","message":error.to_string()}}))).into_response(),&state.ui)};
    let status=upstream.status();let mut headers=upstream.headers().clone();for name in ["set-cookie","connection","transfer-encoding","access-control-allow-origin","access-control-allow-credentials"]{headers.remove(name);}let mut response=Response::new(Body::from_stream(upstream.bytes_stream()));*response.status_mut()=status;*response.headers_mut()=headers;cors(response,&state.ui)
}
async fn websocket(mut local:WebSocket,target:String,token:Option<String>){
    let target=target.replacen("http://","ws://",1).replacen("https://","wss://",1);let mut request=match target.into_client_request(){Ok(request)=>request,Err(_)=>return};if let Some(token)=token{if let Ok(value)=HeaderValue::from_str(&format!("Bearer {token}")){request.headers_mut().insert("authorization",value);}}
    let Ok(Ok((mut remote,_)))=tokio::time::timeout(Duration::from_secs(20),tokio_tungstenite::connect_async(request)).await else{let _=local.close().await;return};
    loop{tokio::select!{
        item=local.recv()=>{let message=match item{Some(Ok(Message::Text(value)))=>tungstenite::Message::Text(value.to_string().into()),Some(Ok(Message::Binary(value)))=>tungstenite::Message::Binary(value),Some(Ok(Message::Ping(value)))=>tungstenite::Message::Ping(value),Some(Ok(Message::Pong(value)))=>tungstenite::Message::Pong(value),Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break};if remote.send(message).await.is_err(){break}},
        item=remote.next()=>{let message=match item{Some(Ok(tungstenite::Message::Text(value)))=>Message::Text(value.to_string().into()),Some(Ok(tungstenite::Message::Binary(value)))=>Message::Binary(value),Some(Ok(tungstenite::Message::Ping(value)))=>Message::Ping(value),Some(Ok(tungstenite::Message::Pong(value)))=>Message::Pong(value),Some(Ok(tungstenite::Message::Close(_)))|None|Some(Err(_))=>break,_=>continue};if local.send(message).await.is_err(){break}},
    }}    let _=remote.close(None).await;let _=local.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_connect_and_local_recover_against_the_isolation_backend() {
        let root = std::env::temp_dir().join(format!("muteki-i52-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).unwrap();
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local=format!("http://{}",listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (ready_tx,ready_rx)=std::sync::mpsc::sync_channel(1);
        let (stop_tx,stop_rx)=tokio::sync::oneshot::channel::<()>();
        let server=std::thread::spawn(move||{
            let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async move{
                let listener=tokio::net::TcpListener::from_std(listener).unwrap();
                let router=Router::new()
                    .route("/api/health",axum::routing::get(||async{axum::Json(json!({"app":"muteki-rust","version":"test"}))}))
                    .route("/api/auth/status",axum::routing::get(||async{axum::Json(json!({"session_protocol":2,"service_id":"synthetic-service","identity_id":"operator","auth_required":false}))}));
                ready_tx.send(()).unwrap();
                axum::serve(listener,router).with_graceful_shutdown(async{let _=stop_rx.await;}).await.unwrap();
            });
        });
        ready_rx.recv().unwrap();
        initialize(&root, "http://127.0.0.1:9", &local).expect("connection.local");
        let started = snapshot("main");
        assert_eq!(started["origin"], local);
        assert_eq!(started["status"], "connected");
        assert_eq!(started["connectionMode"], "local");
        assert!(started["serviceId"].as_str().is_some_and(|id| !id.is_empty()));
        let switched = connect("main", Some(&local)).expect("connection.connect");
        assert_eq!(switched["origin"], local);
        assert_eq!(switched["connectionMode"], "local");
        assert_eq!(switched["configuring"], false);
        assert!(connect("main", Some("http://127.0.0.1:1")).is_err(), "connection.connect");
        let kept = snapshot("main");
        assert_eq!(kept["origin"], local);
        assert_eq!(kept["status"], "connected");
        let recovered = connect("main", None).expect("connection.local");
        assert_eq!(recovered["origin"], local);
        assert_eq!(recovered["connectionMode"], "local");
        assert_eq!(recovered["configuring"], false);
        let configuring = configure("main", true).expect("connection.configure");
        assert_eq!(configuring["configuring"], true);
        assert_eq!(configuring["origin"], local);
        let resumed = configure("main", false).expect("connection.resume");
        assert_eq!(resumed["configuring"], false);
        assert_eq!(resumed["origin"], local);
        close("main");
        stop_tx.send(()).unwrap();
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
