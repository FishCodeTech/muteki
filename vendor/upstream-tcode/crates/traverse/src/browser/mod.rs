//! The browser listener: the static bundle, password login and `/ws` into
//! the same [`HostMux`] native devices reach over Traverse. It is plain HTTP
//! for direct access on this machine or its LAN; a bind beyond loopback
//! needs a password first.
//!
//! Wire contract with `crates/web`: `GET /auth/state` →
//! `{"mode":"password","configured":bool}`; `POST /auth/setup` and
//! `POST /auth/login` take JSON `{password, device_id?, device_name,
//! platform?}` and login answers `{host_id, host_name, token}`; `/ws` opens
//! with `{"type":"hello","protocol_version":5,"token":…,"device":{name,
//! platform?}}` and is answered by `hello_ok` or `hello_rejected` with a
//! `reason` of `token` or `protocol`. Only the current protocol version is
//! accepted; the bundle and the listener ship together.
mod auth;
mod original;

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use tcode_protocol::{HostedDevice, HostingAction, HostingState, PathInfo, ProtocolError};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Message, Role, WebSocketConfig};

use crate::http::{Request, content_type, read_request, response, response_with_body_mode};
use crate::mux::HostMux;
use crate::runtime::{block_on, runtime};
use crate::wire::DeviceClaim;
use auth::{AuthStore, DeviceDetails};

pub type StaticBundle = &'static [(&'static str, &'static [u8])];

/// Answers a browser's hosting query for native devices; the Traverse host
/// owns that state, so the listener is only handed a way to reach it. The
/// listener adds the devices it holds tokens for itself.
pub type HostingHandler =
    Arc<dyn Fn(HostingAction) -> Result<HostingState, ProtocolError> + Send + Sync>;

pub struct BrowserConfig {
    pub listen: SocketAddr,
    pub host_name: String,
    pub data_dir: PathBuf,
    pub static_bundle: Option<StaticBundle>,
    pub hosting: Option<HostingHandler>,
}

/// Connections admitted at once, counting those still reading their request.
const MAX_CONNECTIONS: usize = 256;
/// How long an unauthenticated peer has to send its request or its hello.
const UNAUTHENTICATED_DEADLINE: Duration = Duration::from_secs(5);
/// Idle time before the listener pings a browser; two unanswered pings end
/// the socket. Browsers answer pings below the page's JavaScript.
const PING_INTERVAL: Duration = Duration::from_secs(10);
/// Hello frames are small; anything larger is not a hello.
const MAX_HELLO_BYTES: usize = 64 * 1024;

/// The device fields a browser sends with `/auth/login`; setup carries
/// none. `device_id` and `platform` are optional.
#[derive(Deserialize)]
struct LoginClaim {
    #[serde(default)]
    device_name: String,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    platform: Option<String>,
}

impl LoginClaim {
    fn claim(&self) -> DeviceClaim {
        DeviceClaim {
            name: self.device_name.clone(),
            platform: self.platform.clone(),
        }
    }

    fn is_valid(&self) -> bool {
        self.device_id
            .as_deref()
            .is_none_or(tcode_client::host::valid_device_id)
            && self.claim().is_valid()
    }

    fn details(&self) -> DeviceDetails {
        let (name, platform) = self.claim().normalized();
        DeviceDetails { name, platform }
    }
}

struct Shared {
    policy: tcode_client::attachment::AttachmentPolicy,
    mux: HostMux,
    auth: Mutex<AuthStore>,
    static_bundle: Option<StaticBundle>,
    hosting: Option<HostingHandler>,
    /// Closed to end every connection.
    shutdown: async_channel::Receiver<()>,
    connections: AtomicUsize,
    /// Sockets open per browser device, for the hosting list.
    live: Mutex<HashMap<uuid::Uuid, usize>>,
}

pub struct BrowserServer {
    local_addr: SocketAddr,
    shared: Arc<Shared>,
    shutdown: async_channel::Sender<()>,
    accept: Option<tokio::task::JoinHandle<()>>,
}

impl BrowserServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn password_configured(&self) -> bool {
        self.shared.auth.lock().unwrap().password_configured()
    }

    /// Stop accepting and close every browser socket.
    pub fn shutdown(mut self) {
        self.shutdown.close();
        if let Some(accept) = self.accept.take() {
            let _ = block_on(accept);
        }
    }
}

impl Drop for BrowserServer {
    fn drop(&mut self) {
        self.shutdown.close();
        if let Some(accept) = self.accept.take() {
            accept.abort();
        }
    }
}

/// A bind beyond loopback is refused until a password exists in
/// `data_dir`, so a LAN never sees the first-open setup page. [`serve`]
/// enforces it; a host may ask earlier, before starting anything else.
pub fn check_bind(listen: SocketAddr, data_dir: &Path) -> io::Result<()> {
    if listen.ip().is_loopback() || AuthStore::password_configured_at(data_dir)? {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "refusing to listen on {listen} without a password; set one with --password or set-password, or keep the browser listener on loopback"
        ),
    ))
}

/// Bind and serve; see [`check_bind`].
pub fn serve(mux: HostMux, config: BrowserConfig) -> io::Result<BrowserServer> {
    check_bind(config.listen, &config.data_dir)?;
    let auth = AuthStore::open(&config.data_dir, &config.host_name)?;
    let listener = std::net::TcpListener::bind(config.listen)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    let (shutdown, shutdown_rx) = async_channel::bounded::<()>(1);
    let shared = Arc::new(Shared {
        policy: tcode_client::attachment::AttachmentPolicy::network(),
        mux,
        auth: Mutex::new(auth),
        static_bundle: config.static_bundle,
        hosting: config.hosting,
        shutdown: shutdown_rx.clone(),
        connections: AtomicUsize::new(0),
        live: Mutex::new(HashMap::new()),
    });
    let accept_shared = shared.clone();
    let accept = runtime().spawn(async move {
        let listener = match TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                log::error!("browser listener initialization failed: {error}");
                return;
            }
        };
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = shutdown_rx.recv() => break,
            };
            match accepted {
                Ok((stream, peer)) => {
                    let shared = accept_shared.clone();
                    tokio::spawn(async move {
                        log::debug!("browser connection from {peer}");
                        if let Err(error) = admit(stream, shared).await {
                            log::debug!("browser connection ended: {error}");
                        }
                    });
                }
                Err(error) => log::warn!("browser accept failed: {error}"),
            }
        }
    });
    Ok(BrowserServer {
        local_addr,
        shared,
        shutdown,
        accept: Some(accept),
    })
}

/// Change a stopped headless host's password without changing its identity.
pub fn set_password(data_dir: &Path, password: &str, revoke_tokens: bool) -> io::Result<()> {
    let name = std::fs::read(data_dir.join("remote.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["host_name"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "Tcode".into());
    AuthStore::open(data_dir, &name)?.set_password(password, revoke_tokens)
}

async fn admit(stream: tokio::net::TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    struct Permit(Arc<Shared>);
    impl Drop for Permit {
        fn drop(&mut self) {
            self.0.connections.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let previous = shared.connections.fetch_add(1, Ordering::Relaxed);
    let _permit = Permit(shared.clone());
    if previous >= MAX_CONNECTIONS || shared.shutdown.is_closed() {
        return Err(io::Error::other("server unavailable"));
    }
    let _ = stream.set_nodelay(true);
    handle_connection(BufReader::new(stream), shared).await
}

async fn handle_connection<S>(mut stream: BufReader<S>, shared: Arc<Shared>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let request = tokio::select! {
        request = tokio::time::timeout(UNAUTHENTICATED_DEADLINE, read_request(&mut stream)) => {
            request.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "HTTP request timed out")))
        }
        _ = shared.shutdown.recv() => return Ok(()),
    };
    let request = match request {
        Ok(request) => request,
        Err(error) => {
            let _ = response(
                &mut stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                b"bad request",
            )
            .await;
            return Err(error);
        }
    };
    if request.method == "GET" && request.path == "/ws" && is_websocket_upgrade(&request) {
        // The WS loop sends its own close frame on shutdown; bound teardown
        // as well when it is blocked writing to an unresponsive peer.
        return tokio::select! {
            result = websocket(stream, request, shared.clone()) => result,
            _ = async {
                let _ = shared.shutdown.recv().await;
                tokio::time::sleep(Duration::from_secs(1)).await;
            } => Ok(()),
        };
    }
    tokio::select! {
        result = dispatch(&mut stream, request, &shared) => result,
        _ = shared.shutdown.recv() => Ok(()),
    }
}

async fn dispatch<S>(stream: &mut S, request: Request, shared: &Arc<Shared>) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/original") => original::serve(stream, request, shared).await,
        ("GET", "/auth/state") => {
            let configured = shared.auth.lock().unwrap().password_configured();
            json_response(
                stream,
                "200 OK",
                &serde_json::json!({"mode": "password", "configured": configured}),
            )
            .await
        }
        ("POST", "/auth/setup" | "/auth/login") => {
            password_auth(stream, request, shared.clone()).await
        }
        ("GET" | "HEAD", path) => {
            serve_static(stream, path, shared, request.method == "HEAD").await
        }
        _ => {
            response(
                stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"not found",
            )
            .await
        }
    }
}

// Password hashing runs off the runtime's workers; the auth lock serializes
// setup and login.
async fn password_auth<S>(stream: &mut S, request: Request, shared: Arc<Shared>) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    #[derive(Deserialize)]
    struct PasswordRequest {
        password: String,
        #[serde(flatten)]
        device: LoginClaim,
    }
    // Requiring JSON prevents a cross-origin HTML form from claiming first setup.
    if !request.headers.get("content-type").is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    }) {
        return json_response(
            stream,
            "415 Unsupported Media Type",
            &serde_json::json!({"error":"application/json required"}),
        )
        .await;
    }
    let setup = request.path == "/auth/setup";
    let Ok(request) = serde_json::from_slice::<PasswordRequest>(&request.body) else {
        return json_response(
            stream,
            "400 Bad Request",
            &serde_json::json!({"error":"malformed request"}),
        )
        .await;
    };
    if request.password.len() > 1024 || (!setup && !request.device.is_valid()) {
        return json_response(
            stream,
            "400 Bad Request",
            &serde_json::json!({"error":"malformed request"}),
        )
        .await;
    }
    let (status, value) = tokio::task::spawn_blocking(move || -> io::Result<_> {
        let mut auth = shared.auth.lock().unwrap();
        if setup {
            if auth.password_configured() {
                return Ok(("409 Conflict", serde_json::json!({"error":"already configured"})));
            }
            if request.password.chars().count() < 8 {
                return Ok(("400 Bad Request", serde_json::json!({"error":"password requires at least 8 characters"})));
            }
            auth.set_password(&request.password, false)?;
            Ok(("200 OK", serde_json::json!({"configured":true})))
        } else if auth.verify_password(&request.password) {
            let details = request.device.details();
            let token = auth.issue_token(request.device.device_id, details)?;
            Ok(("200 OK", serde_json::json!({"host_id":auth.host_id,"host_name":auth.host_name,"token":token})))
        } else {
            Ok(("403 Forbidden", serde_json::json!({"error":"invalid password or temporarily locked"})))
        }
    })
    .await
    .map_err(|_| io::Error::other("password worker exited"))??;
    json_response(stream, status, &value).await
}

async fn json_response<S, T>(stream: &mut S, status: &str, value: &T) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    response(stream, status, "application/json", &body).await
}

async fn serve_static<S>(
    stream: &mut S,
    request_path: &str,
    shared: &Shared,
    head_only: bool,
) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let lookup = if request_path == "/" {
        "/index.html"
    } else {
        request_path
    };
    let found = shared.static_bundle.and_then(|bundle| {
        bundle
            .iter()
            .find(|(path, _)| *path == lookup)
            .map(|(_, bytes)| *bytes)
    });
    match found {
        Some(bytes) => {
            response_with_body_mode(stream, "200 OK", content_type(lookup), bytes, head_only).await
        }
        None => {
            response_with_body_mode(
                stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"not found",
                head_only,
            )
            .await
        }
    }
}

fn is_websocket_upgrade(request: &Request) -> bool {
    request
        .headers
        .get("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        && request.headers.contains_key("sec-websocket-key")
}

#[derive(Deserialize)]
struct Hello {
    #[serde(rename = "type")]
    kind: String,
    token: String,
    device: DeviceClaim,
}

/// A connected browser's slot in the hosting list, held while its socket is.
struct Live {
    shared: Arc<Shared>,
    device: uuid::Uuid,
}

impl Live {
    fn new(shared: Arc<Shared>, device: uuid::Uuid) -> Self {
        *shared.live.lock().unwrap().entry(device).or_default() += 1;
        Self { shared, device }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let mut live = self.shared.live.lock().unwrap();
        if let Some(count) = live.get_mut(&self.device) {
            *count -= 1;
            if *count == 0 {
                live.remove(&self.device);
            }
        }
    }
}

async fn websocket<S>(mut stream: S, request: Request, shared: Arc<Shared>) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    use tokio::io::AsyncWriteExt as _;
    let key = request
        .headers
        .get("sec-websocket-key")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing websocket key"))?;
    let accept = derive_accept_key(key.as_bytes());
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(handshake.as_bytes()).await?;
    stream.flush().await?;
    // Apply the HTTP input bound while assembling frames too, before an
    // unauthenticated peer can consume tungstenite's 64 MiB default.
    let config = WebSocketConfig::default()
        .read_buffer_size(MAX_HELLO_BYTES)
        .max_message_size(Some(crate::http::MAX_BODY_BYTES))
        .max_frame_size(Some(crate::http::MAX_BODY_BYTES));
    let mut websocket = WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
    let first = tokio::time::timeout(UNAUTHENTICATED_DEADLINE, receive_hello(&mut websocket))
        .await
        .ok()
        .flatten();
    // An older bundle is told so before its token is judged.
    if let Some(received) = first
        .as_ref()
        .and_then(|first| first["protocol_version"].as_u64())
        && received != u64::from(tcode_protocol::PROTOCOL_VERSION)
    {
        let rejected = serde_json::json!({
            "type": "hello_rejected",
            "reason": "protocol",
            "expected": tcode_protocol::PROTOCOL_VERSION,
            "received": received
        });
        let _ = websocket
            .send(Message::Text(rejected.to_string().into()))
            .await;
        let _ = websocket.close(None).await;
        return Ok(());
    }
    let hello = first
        .and_then(|first| serde_json::from_value::<Hello>(first).ok())
        .filter(|hello| hello.kind == "hello");
    let device = hello.as_ref().and_then(|hello| {
        shared
            .auth
            .lock()
            .unwrap()
            .device_id_for_token(&hello.token)
    });
    let (Some(hello), Some(device)) = (hello, device) else {
        let rejected = serde_json::json!({"type": "hello_rejected", "reason": "token"});
        let _ = websocket
            .send(Message::Text(rejected.to_string().into()))
            .await;
        let _ = websocket.close(None).await;
        return Ok(());
    };
    let token = hello.token;
    // The hosting list shows what the device calls itself now, not what it
    // said when it logged in. The token already validated, so a failed write
    // is not a reason to refuse the connection.
    if hello.device.is_valid() {
        let (name, platform) = hello.device.normalized();
        if let Err(error) = shared
            .auth
            .lock()
            .unwrap()
            .refresh_device(&token, DeviceDetails { name, platform })
        {
            log::warn!("could not record the connecting device's details: {error}");
        }
    }
    let hello_ok = {
        let auth = shared.auth.lock().unwrap();
        serde_json::json!({
            "type": "hello_ok",
            "host_id": auth.host_id,
            "host_name": auth.host_name,
            "protocol_version": tcode_protocol::PROTOCOL_VERSION,
        })
    };
    websocket
        .send(Message::Text(hello_ok.to_string().into()))
        .await
        .map_err(io::Error::other)?;
    let _live = Live::new(shared.clone(), device);
    let connection = shared
        .mux
        .attach(tcode_protocol::Principal::Full, shared.policy);
    // The token scopes retained-command keys across socket lifetimes, as the
    // device id does on Traverse, so no two devices can share a dedup cache.
    let scope = auth::hex_hash(token.as_bytes());
    let relay = async {
        let mut unanswered_pings = 0_u8;
        loop {
            enum Input {
                WebSocket(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
                Host(Result<String, async_channel::RecvError>),
                Ping,
                Shutdown,
            }
            let input = tokio::select! {
                message = websocket.next() => Input::WebSocket(message),
                line = connection.from_host.recv() => Input::Host(line),
                _ = tokio::time::sleep(PING_INTERVAL) => Input::Ping,
                _ = shared.shutdown.recv() => Input::Shutdown,
            };
            // Active streams may never reach the idle keepalive tick. Recheck
            // before forwarding in either direction, including hosting polls.
            if !shared.auth.lock().unwrap().token_is_valid(&token) {
                let _ = websocket.close(None).await;
                break;
            }
            match input {
                Input::WebSocket(Some(Ok(Message::Text(line)))) => {
                    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&line) else {
                        let _ = websocket.close(None).await;
                        break;
                    };
                    if let Ok(tcode_protocol::ClientMessage {
                        principal: _,
                        id,
                        key: _,
                        payload:
                            tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Hosting {
                                action,
                            }),
                    }) = serde_json::from_value(value.clone())
                    {
                        let reply = tcode_protocol::HostMessage::QueryResult {
                            id,
                            result: shared
                                .hosting(action)
                                .map(tcode_protocol::QueryResponse::Hosting),
                        };
                        websocket
                            .send(Message::Text(
                                serde_json::to_string(&reply)
                                    .map_err(io::Error::other)?
                                    .into(),
                            ))
                            .await
                            .map_err(io::Error::other)?;
                        continue;
                    }
                    if let Some(key) = value.get("key").and_then(serde_json::Value::as_str) {
                        if !crate::host::valid_key(key) {
                            let _ = websocket.close(None).await;
                            break;
                        }
                        value["key"] = format!("{scope}:{key}").into();
                    }
                    let mut line = value.to_string();
                    line.push('\n');
                    if connection.to_host.send(line).await.is_err() {
                        break;
                    }
                }
                Input::WebSocket(Some(Ok(Message::Ping(payload)))) => {
                    websocket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(io::Error::other)?;
                }
                Input::WebSocket(Some(Ok(Message::Pong(_)))) => unanswered_pings = 0,
                Input::WebSocket(Some(Ok(Message::Close(_))) | Some(Err(_)) | None) => break,
                Input::WebSocket(Some(Ok(_))) => {}
                Input::Host(Ok(line)) => websocket
                    .send(Message::Text(line.trim_end().to_owned().into()))
                    .await
                    .map_err(io::Error::other)?,
                Input::Host(Err(_)) => break,
                Input::Ping => {
                    if unanswered_pings >= 2 {
                        let _ = websocket.close(None).await;
                        break;
                    }
                    unanswered_pings += 1;
                    websocket
                        .send(Message::Ping(Vec::new().into()))
                        .await
                        .map_err(io::Error::other)?;
                }
                Input::Shutdown => {
                    let _ = websocket.close(None).await;
                    break;
                }
            }
        }
        Ok(())
    };
    let result = tokio::select! {
        result = relay => result,
        _ = connection.to_host.closed() => Ok(()),
    };
    connection.to_host.close();
    result
}

async fn receive_hello<S>(websocket: &mut WebSocketStream<S>) -> Option<serde_json::Value>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Some(Ok(Message::Text(first))) = websocket.next().await else {
        return None;
    };
    if first.len() > MAX_HELLO_BYTES {
        return None;
    }
    serde_json::from_str(&first).ok()
}

impl Shared {
    /// Native devices from the Traverse host, then the browsers this
    /// listener holds tokens for. Revoking a browser device is the
    /// listener's to do; every other action is the host's.
    fn hosting(&self, action: HostingAction) -> Result<HostingState, ProtocolError> {
        let action = match action {
            HostingAction::RevokeDevice(id) => match id.parse::<uuid::Uuid>() {
                Ok(device) => {
                    let revoked = self.auth.lock().unwrap().revoke(device);
                    match revoked {
                        Ok(true) => HostingAction::State,
                        Ok(false) => HostingAction::RevokeDevice(id),
                        Err(error) => {
                            log::error!("could not persist the revocation: {error}");
                            return Err(crate::host::revoke_error(error));
                        }
                    }
                }
                Err(_) => HostingAction::RevokeDevice(id),
            },
            action => action,
        };
        let mut state = match &self.hosting {
            Some(hosting) => hosting(action)?,
            None => {
                let auth = self.auth.lock().unwrap();
                HostingState {
                    created_space_id: None,
                    spaces: Vec::new(),
                    enabled: false,
                    expires_in_secs: 0,
                    host_id: auth.host_id.to_string(),
                    host_name: auth.host_name.clone(),
                    invite: None,
                    devices: Vec::new(),
                    traverse: Vec::new(),
                    group: None,
                }
            }
        };
        let auth = self.auth.lock().unwrap();
        let live = self.live.lock().unwrap();
        state
            .devices
            .extend(auth.devices.iter().map(|device| HostedDevice {
                access: tcode_protocol::DeviceAccess::Full,
                id: device.id.to_string(),
                name: device.name.clone(),
                created_unix: device.created_unix,
                platform: device.platform.clone(),
                // A browser reaches the listener at the machine's own address.
                path: live.contains_key(&device.id).then_some(PathInfo {
                    direct: true,
                    relay: None,
                    lan: true,
                    probing_direct: false,
                }),
            }));
        Ok(state)
    }
}
