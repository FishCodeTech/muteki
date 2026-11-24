//! A dedicated TLS-only listener. Node authentication never uses Web cookies.
use super::identity::{self, Credential, Lease};
use crate::AppState;
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt, stream::FuturesUnordered};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap, future::Future, net::SocketAddr, path::Path, pin::Pin, sync::Arc,
    time::Duration,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    },
    server::TlsStream,
};

type Handshake = Pin<Box<dyn Future<Output = (std::net::IpAddr, Option<(TlsStream<TcpStream>, SocketAddr)>)> + Send>>;
pub struct Listener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    pending: FuturesUnordered<Handshake>,
    pending_by_ip: HashMap<std::net::IpAddr, usize>,
}
impl Listener {
    pub async fn bind(address: SocketAddr, cert: &Path, key: &Path) -> anyhow::Result<Self> {
        let certificates = CertificateDer::pem_file_iter(cert)?.collect::<Result<Vec<_>, _>>()?;
        let key = PrivateKeyDer::from_pem_file(key)?;
        Self::bind_certified(address, certificates, key).await
    }
    pub async fn bind_pem(address: SocketAddr, cert: &str, key: &str) -> anyhow::Result<Self> {
        let certificates = CertificateDer::pem_slice_iter(cert.as_bytes()).collect::<Result<Vec<_>, _>>()?;
        let key = PrivateKeyDer::from_pem_slice(key.as_bytes())?;
        Self::bind_certified(address, certificates, key).await
    }
    async fn bind_certified(
        address: SocketAddr,
        certificates: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> anyhow::Result<Self> {
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certificates, key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            tcp: TcpListener::bind(address).await?,
            acceptor: TlsAcceptor::from(Arc::new(config)),
            pending: FuturesUnordered::new(),
            pending_by_ip: HashMap::new(),
        })
    }
}
impl axum::serve::Listener for Listener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                next=self.pending.next(),if !self.pending.is_empty()=>{
                    if let Some((ip, connection)) = next {
                        if let Some(count) = self.pending_by_ip.get_mut(&ip) {
                            *count -= 1;
                            if *count == 0 { self.pending_by_ip.remove(&ip); }
                        }
                        if let Some(connection) = connection { return connection; }
                    }
                }
                next=self.tcp.accept()=>match next {
                    Ok((stream,address))=>{
                        let ip = address.ip();
                        if self.pending.len() >= 32 || self.pending_by_ip.get(&ip).copied().unwrap_or(0) >= 4 {
                            drop(stream);
                            continue;
                        }
                        *self.pending_by_ip.entry(ip).or_default() += 1;
                        let acceptor=self.acceptor.clone();
                        self.pending.push(Box::pin(async move {
                            let connection = tokio::time::timeout(Duration::from_secs(5),acceptor.accept(stream)).await
                                .ok().and_then(Result::ok).map(|stream|(stream,address));
                            (ip, connection)
                        }));
                    }
                    Err(error)=>{tracing::warn!(%error,"Runner TLS accept failed");tokio::time::sleep(Duration::from_millis(100)).await;}
                }
            }
        }
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

#[derive(Clone)]
struct Control {
    state: Arc<AppState>,
    shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    builtin_ca: bool,
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn denied() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error":"Invalid or expired node authorization"})),
    )
        .into_response()
}
fn private(value: Value) -> Response {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(value),
    )
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enrollment {
    token: String,
    protocol_version: u64,
}
async fn enroll(State(control): State<Control>, Json(input): Json<Enrollment>) -> Response {
    if input.protocol_version != crate::runner::PROTOCOL {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"Runner protocol is incompatible"})),
        )
            .into_response();
    }
    match identity::enroll(&control.state.store, &input.token, now()) {
        Ok(credential) => {
            private(json!({"credential":credential,"protocol_version":crate::runner::PROTOCOL}))
        }
        Err(_) => denied(),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    credential: Credential,
    protocol_version: u64,
}
async fn session(State(control): State<Control>, Json(input): Json<Session>) -> Response {
    if input.protocol_version != crate::runner::PROTOCOL {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"Runner protocol is incompatible"})),
        )
            .into_response();
    }
    match identity::connect(&control.state.store, &input.credential, now()) {
        Ok(lease) => private(json!({"lease":lease,"protocol_version":crate::runner::PROTOCOL})),
        Err(_) => denied(),
    }
}
async fn connect(
    State(control): State<Control>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let lease = (|| -> Option<Lease> {
        let value = headers
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?;
        if value.len() > 2048 {
            return None;
        }
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(value).ok()?).ok()
    })();
    let Some(lease) = lease else {
        return denied();
    };
    let (guard, jobs) = match control.state.remote_runners.attach(&lease) {
        Ok(slot) => slot,
        Err(error) => return (StatusCode::CONFLICT, Json(json!({"error":error}))).into_response(),
    };
    if identity::renew(&control.state.store, &lease, now()).is_err() {
        return denied();
    }
    upgrade
        .max_message_size(1024 * 1024)
        .max_frame_size(1024 * 1024)
        .on_upgrade(move |socket| live(socket, control, lease, guard, jobs))
        .into_response()
}
async fn live(
    mut socket: WebSocket,
    control: Control,
    lease: Lease,
    _guard: super::broker::Guard,
    mut jobs: tokio::sync::mpsc::Receiver<super::broker::Job>,
) {
    let mut shutdown = control.shutdown.clone();
    let Ok(projects) = identity::projects(&control.state.store, &lease, now()) else {
        return;
    };
    let hello = json!({"type":"hello","protocol_version":crate::runner::PROTOCOL,"node_id":lease.node_id,
        "generation":lease.generation,"projects":projects,"lease_seconds":identity::LEASE_SECONDS,"capabilities":["identity","heartbeat","scoped_execution","workspace_pull","artifact_push","supervisor_lease"]});
    if !send(&mut socket, hello).await {
        return;
    }
    let mut pending: HashMap<String, tokio::sync::oneshot::Sender<Result<Value, String>>> =
        HashMap::new();
    let mut last_heartbeat = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _=tokio::time::sleep_until(last_heartbeat+Duration::from_secs(10))=>break,
            _=async { match shutdown.as_mut() { Some(rx)=>{let _=rx.wait_for(|stop|*stop).await;}, None=>std::future::pending::<()>().await } }=>break,
            incoming=socket.recv()=>match incoming {
                Some(Ok(Message::Text(message))) if message.as_str()=="heartbeat"=>{
                    let Ok(expires)=identity::renew(&control.state.store,&lease,now()) else{break;};
                    last_heartbeat=tokio::time::Instant::now();
                    if !send(&mut socket,json!({"type":"lease","expires_at":expires,"lease_seconds":(expires-now()).clamp(0,identity::LEASE_SECONDS),"generation":lease.generation})).await{break;}
                },
                Some(Ok(Message::Text(message)))=>{
                    let Ok(mut response)=serde_json::from_str::<Value>(&message) else{break;};
                    if response["type"]=="resources"{
                        if serde_json::from_value::<crate::runner_resources::Recommendation>(response["recommendation"].clone()).is_err(){break;}
                        response["node_id"]=json!(lease.node_id);response["received_at"]=json!(now());
                        if control.state.store.put("runner_node_resources",&lease.node_id,&response).is_err(){break;}
                        continue;
                    }
                    if response["type"]!="result"{break;}
                    let Some(id)=response["request_id"].as_str() else{break;};
                    let Some(reply)=pending.remove(id) else{break;};
                    let result=if let Some(error)=response["error"].as_str(){Err(error.to_owned())}else{Ok(response["result"].take())};
                    let _=reply.send(result);
                },
                _=>break,
            },
            job=jobs.recv(),if pending.len()<16=>{
                let Some(job)=job else{break;};
                if job.command.expires_at<=now(){let _=job.reply.send(Err("Remote command expired in queue".into()));continue;}
                if let Err(error)=identity::authorize(&control.state.store,&lease,&job.command.project_id,job.command.action.new_work(),now()){
                    let _=job.reply.send(Err(error));continue;
                }
                let id=uuid::Uuid::new_v4().to_string();
                let message=json!({"type":"command","request_id":id,"command":job.command});
                if message.to_string().len()>1024*1024{let _=job.reply.send(Err("Remote command exceeds frame limit".into()));continue;}
                pending.insert(id,job.reply);
                if !send(&mut socket,message).await{break;}
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await;
}
async fn send(socket: &mut WebSocket, value: Value) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            socket.send(Message::Text(value.to_string().into()))
        )
        .await,
        Ok(Ok(()))
    )
}
/// The CA certificate is public; nodes compare its SHA-256 with the pin from the enrollment command.
async fn authority(State(control): State<Control>) -> Response {
    if !control.builtin_ca {
        return (StatusCode::NOT_FOUND, Json(json!({"error":"This listener uses an externally managed certificate"}))).into_response();
    }
    match super::tls::ca_pem(&crate::native_chat::state_dir(&control.state)) {
        Ok(Some(pem)) => private(json!({"ca_pem":pem})),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"error":"This listener does not use the built-in Runner CA"}))).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":error}))).into_response(),
    }
}
pub fn router(state: Arc<AppState>) -> Router {
    router_with(state, None)
}
pub fn router_with(state: Arc<AppState>, shutdown: Option<tokio::sync::watch::Receiver<bool>>) -> Router {
    let control = Control {
        state: state.clone(),
        builtin_ca: shutdown.is_some(),
        shutdown,
    };
    Router::new()
        .route("/v1/runner/ca", get(authority))
        .route("/v1/runner/enroll", post(enroll))
        .route("/v1/runner/session", post(session))
        .route("/v1/runner/connect", get(connect))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(control)
        .merge(super::credentials::node_router(state.clone()))
        .merge(super::files::node_router(state))
}
