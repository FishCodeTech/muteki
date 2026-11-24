//! Outbound authenticated Runner connection with scoped commands and file jobs.
mod transfer;
mod credentials;
use super::identity::{Credential, Lease};
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_rustls::rustls::{
    self,
    pki_types::{CertificateDer, pem::PemObject},
};
use tokio_tungstenite::{
    Connector,
    tungstenite::{
        client::IntoClientRequest,
        protocol::{Message, WebSocketConfig},
    },
};

#[derive(Serialize, Deserialize)]
struct Configuration {
    server: String,
    ca_pem: Option<String>,
    credential: Credential,
    lease: Option<Lease>,
}
fn endpoint(server: &str, path: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(server)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("Runner server must be an HTTPS origin without userinfo, path, query or fragment");
    }
    url.set_path(path);
    Ok(url)
}
fn path(root: &Path) -> PathBuf {
    root.join("runner/remote.json")
}
fn directory(root: &Path) -> anyhow::Result<PathBuf> {
    let directory = root.join("runner");
    std::fs::create_dir_all(&directory)?;
    if std::fs::symlink_metadata(&directory)?
        .file_type()
        .is_symlink()
    {
        bail!("Runner directory cannot be a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}
fn save(root: &Path, config: &Configuration, create: bool) -> anyhow::Result<()> {
    let directory = directory(root)?;
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&serde_json::to_vec(config)?)?;
    file.as_file().sync_all()?;
    if create {
        file.persist_noclobber(path(root))?;
    } else {
        file.persist(path(root))?;
    }
    #[cfg(unix)]
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}
fn load(root: &Path) -> anyhow::Result<Configuration> {
    let metadata = std::fs::symlink_metadata(path(root))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Remote identity must be a regular private file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("Remote identity file must have mode 0600");
        }
    }
    Ok(serde_json::from_slice(&std::fs::read(path(root))?)?)
}
fn http(ca: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut builder = crate::network_proxy::http_client_builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(ca) = ca {
        builder = builder.tls_built_in_root_certs(false);
        for cert in reqwest::Certificate::from_pem_bundle(ca.as_bytes())? {
            builder = builder.add_root_certificate(cert);
        }
    }
    Ok(builder.build()?)
}
fn connector(ca: Option<&str>) -> anyhow::Result<Connector> {
    let mut roots = rustls::RootCertStore::empty();
    match ca {
        Some(ca) => {
            for cert in CertificateDer::pem_slice_iter(ca.as_bytes()) {
                roots.add(cert?)?;
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Connector::Rustls(Arc::new(config)))
}
/// Fetches the listener CA over an unauthenticated channel and accepts it only if it matches the pin
/// copied from the administrator's enrollment command. No secret is sent before the pin is checked.
async fn pinned_authority(server: &str, pin: &str) -> anyhow::Result<String> {
    let pin: String = pin.chars().filter(|c| *c != ':').collect::<String>().to_ascii_lowercase();
    if pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("--ca-sha256 must be the 64-hex-digit SHA-256 of the Runner CA certificate");
    }
    let response = crate::network_proxy::http_client_builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_certs(true)
        .build()?
        .get(endpoint(server, "/v1/runner/ca")?)
        .send()
        .await
        .context("Could not reach the Runner endpoint")?;
    if !response.status().is_success() {
        bail!("Runner endpoint did not provide its CA (HTTP {})", response.status());
    }
    let value: Value = response.json().await?;
    let pem = value["ca_pem"].as_str().context("Runner endpoint returned no CA")?.to_owned();
    let certificates = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()?;
    if certificates.len() != 1 {
        bail!("Runner endpoint must provide exactly one pinned CA certificate");
    }
    let actual = super::tls::fingerprint(&pem).map_err(anyhow::Error::msg)?;
    if actual != pin {
        bail!("Runner CA fingerprint mismatch (got {actual}); the endpoint is not the server that issued this command");
    }
    Ok(pem)
}
/// reqwest already honours proxy variables for HTTPS; the WebSocket follows the same rules.
fn proxy_for(target: &reqwest::Url) -> anyhow::Result<Option<reqwest::Url>> {
    let variable = |names: &[&str]| names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()));
    let host = target.host_str().unwrap_or_default().trim_matches(['[', ']']).to_ascii_lowercase();
    if let Some(list) = variable(&["NO_PROXY", "no_proxy"]) {
        for entry in list.split(',') {
            let entry = entry.trim().trim_start_matches('.').to_ascii_lowercase();
            if entry == "*" || (!entry.is_empty() && (host == entry || host.ends_with(&format!(".{entry}")))) {
                return Ok(None);
            }
        }
    }
    let Some(proxy) = variable(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]) else {
        return Ok(None);
    };
    let proxy = reqwest::Url::parse(&proxy).context("HTTPS_PROXY is not a valid URL")?;
    if proxy.scheme() != "http" {
        bail!("The Runner WebSocket supports only http:// CONNECT proxies");
    }
    Ok(Some(proxy))
}
async fn tunnel(proxy: &reqwest::Url, target: &reqwest::Url) -> anyhow::Result<tokio::net::TcpStream> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let host = proxy.host_str().context("Proxy host missing")?;
    let mut stream = tokio::net::TcpStream::connect((host, proxy.port_or_known_default().unwrap_or(80)))
        .await
        .context("Proxy is unreachable")?;
    let authority = format!("{}:{}", target.host_str().context("Runner host missing")?, target.port_or_known_default().unwrap_or(443));
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if !proxy.username().is_empty() {
        let pair = format!("{}:{}", proxy.username(), proxy.password().unwrap_or(""));
        request.push_str(&format!("Proxy-Authorization: Basic {}\r\n", base64::engine::general_purpose::STANDARD.encode(pair)));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 || stream.read(&mut byte).await? == 0 {
            bail!("Proxy closed the CONNECT tunnel");
        }
        head.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&head);
    if !status.split_whitespace().nth(1).is_some_and(|code| code == "200") {
        bail!("Proxy refused the CONNECT tunnel: {}", status.lines().next().unwrap_or(""));
    }
    Ok(stream)
}
/// Identity summary for diagnostics; never prints secrets.
pub fn status(root: &Path) -> anyhow::Result<Value> {
    let config = load(root)?;
    Ok(json!({
        "server": config.server,
        "node_id": config.credential.node_id,
        "credential_version": config.credential.version,
        "credential_expires_at": config.credential.expires_at,
        "lease_expires_at": config.lease.as_ref().map(|l| l.expires_at),
        "ca": match &config.ca_pem {
            Some(pem) => json!({"pinned":true,"sha256":super::tls::fingerprint(pem).ok()}),
            None => json!({"pinned":false}),
        },
        "proxy": proxy_for(&endpoint(&config.server, "/")?)?.map(|p| format!("{}://{}:{}", p.scheme(), p.host_str().unwrap_or(""), p.port_or_known_default().unwrap_or(80))),
    }))
}
/// Enrollment, credential installation and daemon operation share ownership of the private config.
fn exclusive(root: &Path, busy: &'static str) -> anyhow::Result<std::fs::File> {
    let directory = directory(root)?;
    use fs2::FileExt;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(directory.join("runner.lock"))?;
    lock.try_lock_exclusive().context(busy)?;
    Ok(lock)
}
/// Installs a credential issued by an administrator rotation for this same node identity.
pub fn install_credential(root: &Path, credential_file: &Path) -> anyhow::Result<()> {
    let _lock = exclusive(root, "Stop this Runner before installing a credential")?;
    let mut config = load(root)?;
    let text = std::fs::read_to_string(credential_file)?;
    let mut value: Value = serde_json::from_str(text.trim()).context("Credential must be the JSON shown in Web settings")?;
    let credential: Credential = serde_json::from_value(value.get_mut("credential").map(Value::take).unwrap_or(value))?;
    if credential.node_id != config.credential.node_id {
        bail!("Credential belongs to another node; enroll this Runner again instead");
    }
    if credential.version <= config.credential.version {
        bail!("Credential is not newer than the installed one");
    }
    config.credential = credential;
    config.lease = None;
    save(root, &config, false)
}
pub async fn enroll(
    root: &Path,
    server: &str,
    token_file: &Path,
    ca_file: Option<&Path>,
    ca_sha256: Option<&str>,
    replace: bool,
) -> anyhow::Result<()> {
    let _lock = exclusive(root, "Stop this Runner before enrolling")?;
    let existing = path(root).try_exists()?;
    if existing && !replace {
        bail!("Remote identity already exists; pass --replace to enroll this Runner again (revoke the old node in Web settings)");
    }
    let url = endpoint(server, "/v1/runner/enroll")?;
    let ca_pem = match (ca_file, ca_sha256) {
        (Some(_), Some(_)) => bail!("Use either --ca-file or --ca-sha256"),
        (Some(file), None) => Some(std::fs::read_to_string(file)?),
        (None, Some(pin)) => Some(pinned_authority(server, pin).await?),
        (None, None) => None,
    };
    let token = std::fs::read_to_string(token_file)?;
    let response = http(ca_pem.as_deref())?
        .post(url)
        .json(&json!({"token":token.trim(),"protocol_version":crate::runner::PROTOCOL}))
        .send()
        .await?;
    if !response.status().is_success() {
        bail!(
            "Enrollment refused (HTTP {}); no identity was installed",
            response.status()
        );
    }
    let mut value: Value = response.json().await?;
    if value["protocol_version"] != crate::runner::PROTOCOL {
        bail!("Incompatible Runner protocol");
    }
    let config = Configuration {
        server: server.into(),
        ca_pem,
        credential: serde_json::from_value(value["credential"].take())?,
        lease: None,
    };
    save(root,&config,!existing).context("Enrollment consumed but identity could not be saved; revoke the orphaned identity and enroll again")?;
    Ok(())
}

async fn connected(
    root: &Path,
    config: &mut Configuration,
    execution: &Arc<Execution>,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    if config
        .lease
        .as_ref()
        .is_none_or(|lease| lease.expires_at <= now)
    {
        let response = http(config.ca_pem.as_deref())?
            .post(endpoint(&config.server, "/v1/runner/session")?)
            .json(
                &json!({"credential":config.credential,"protocol_version":crate::runner::PROTOCOL}),
            )
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("Node session refused (HTTP {})", response.status());
        }
        let mut value: Value = response.json().await?;
        if value["protocol_version"] != crate::runner::PROTOCOL {
            bail!("Incompatible Runner protocol");
        }
        config.lease = Some(serde_json::from_value(value["lease"].take())?);
        save(root, config, false)?;
    }
    let mut url = endpoint(&config.server, "/v1/runner/connect")?;
    url.set_scheme("wss")
        .map_err(|_| anyhow::anyhow!("Invalid WSS origin"))?;
    let mut request = url.as_str().into_client_request()?;
    let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&config.lease)?);
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {encoded}").parse()?);
    let websocket_config = WebSocketConfig::default()
        .max_message_size(Some(1024 * 1024))
        .max_frame_size(Some(1024 * 1024));
    let tls = connector(config.ca_pem.as_deref())?;
    let connection = tokio::time::timeout(Duration::from_secs(10), async {
        match proxy_for(&url)? {
            Some(proxy) => {
                let stream = tunnel(&proxy, &url).await?;
                Ok::<_, anyhow::Error>(
                    tokio_tungstenite::client_async_tls_with_config(request, stream, Some(websocket_config), Some(tls)).await,
                )
            }
            None => Ok(tokio_tungstenite::connect_async_tls_with_config(request, Some(websocket_config), false, Some(tls)).await),
        }
    })
    .await??;
    if matches!(&connection, Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status().as_u16()==401)
    {
        config.lease = None;
        save(root, config, false)?;
    }
    let (mut socket, _) = connection?;
    let hello = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await?
        .context("Missing handshake")??;
    let hello: Value = serde_json::from_str(hello.to_text()?)?;
    let lease = config.lease.as_ref().unwrap();
    if hello["type"] != "hello"
        || hello["protocol_version"] != crate::runner::PROTOCOL
        || hello["node_id"] != lease.node_id
        || hello["generation"] != lease.generation
    {
        bail!("Invalid Runner handshake");
    }
    let projects: Vec<String> = serde_json::from_value(hello["projects"].clone())?;
    *execution.connection_scope.lock().unwrap() = Some(ConnectionScope {
        node_id: lease.node_id.clone(),
        generation: lease.generation,
        credential_version: lease.credential_version,
        projects: projects.clone(),
    });
    let lease_seconds = hello["lease_seconds"]
        .as_u64()
        .filter(|v| *v > 0 && *v <= 30)
        .context("Invalid lease duration")?;
    tracing::info!(node_id=%lease.node_id,"Runner authenticated to remote control plane");
    let (results, mut incoming_results) = tokio::sync::mpsc::channel::<Value>(16);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    let mut resource_tick=tokio::time::interval(Duration::from_secs(10));
    let mut response_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::select! {
            biased;
            _=tokio::time::sleep_until(response_deadline)=>bail!("Lease heartbeat response timed out"),
            _=heartbeat.tick()=>{tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Text("heartbeat".into()))).await??;},
            _=resource_tick.tick()=>{
                let execution=execution.clone();let reply=results.clone();
                tokio::spawn(async move{if let Ok(Ok(snapshot))=tokio::time::timeout(Duration::from_secs(8),execution.processor.resource_snapshot()).await{
                    let _=reply.send(json!({"type":"resources","recommendation":snapshot["recommendation"],"host":snapshot["host"],"docker":snapshot["docker"]})).await;
                }});
            },
            incoming=socket.next()=>{
                let message=incoming.context("Control plane disconnected")??;
                let mut value:Value=serde_json::from_str(message.to_text()?)?;
                if value["type"]=="lease" {
                    let lease=config.lease.as_mut().unwrap();
                    if value["generation"]!=lease.generation{bail!("Invalid lease generation");}
                    lease.expires_at=value["expires_at"].as_i64().context("Missing lease expiry")?;
                    let remaining=value["lease_seconds"].as_u64().filter(|v|*v>0&&*v<=lease_seconds).context("Invalid lease renewal duration")?;
                    let deadline=tokio::time::Instant::now()+Duration::from_secs(remaining);
                    *execution.deadline.lock().unwrap()=deadline;
                    let renewal_execution=execution.clone();let renewal_node=lease.node_id.clone();
                    tokio::spawn(async move {
                        if let Err(error)=renewal_execution.processor.renew_runtimes(&renewal_node,deadline).await {
                            tracing::warn!(%error,"Container execution lease renewal failed");
                        }
                    });
                    let credential_connection=transfer::Connection{server:config.server.clone(),ca_pem:config.ca_pem.clone(),lease:config.lease.as_ref().unwrap().clone(),root:root.to_owned()};
                    tokio::spawn(credentials::revalidate(execution.clone(),credential_connection));
                    response_deadline=tokio::time::Instant::now()+Duration::from_secs(10);
                    save(root,config,false)?;
                } else if value["type"]=="command" {
                    let id=value["request_id"].as_str().filter(|id|id.len()<=64).context("Missing request identity")?.to_owned();
                    let command:super::commands::Command=serde_json::from_value(value["command"].take())?;
                    let lease=config.lease.as_ref().unwrap();
                    let mut authority=super::commands::Authority{node_id:lease.node_id.clone(),session_generation:lease.generation,
                        credential_version:lease.credential_version,projects:projects.clone(),valid_until:*execution.deadline.lock().unwrap()};
                    let credential_authority=credentials::authority_deadline(execution,&command,authority.valid_until);
                    let pool=if command.action.new_work(){&execution.work}else{&execution.control};
                    let permit=pool.clone().try_acquire_owned();
                    let reply=results.clone();let execution=execution.clone();
                    let connection=transfer::Connection{server:config.server.clone(),ca_pem:config.ca_pem.clone(),lease:config.lease.as_ref().unwrap().clone(),root:root.to_owned()};
                    tokio::spawn(async move {
                        let result=match credential_authority { Err(error)=>Err(error), Ok(deadline)=>{
                            authority.valid_until=deadline;
                            match permit {Ok(_permit)=>if matches!(command.action,super::commands::Operation::TransferPull{..}|super::commands::Operation::TransferPush{..}|super::commands::Operation::TransferStatus{..}){
                            transfer::dispatch(execution.clone(),connection,command.clone(),authority).await
                        }else if matches!(command.action,super::commands::Operation::CredentialInstall{..}|super::commands::Operation::CredentialRefresh{..}|super::commands::Operation::CredentialStatus{..}){
                            credentials::dispatch(execution.clone(),connection,command.clone(),authority).await
                        }else{execution.processor.execute(&command,&authority,chrono::Utc::now().timestamp()).await},
                            Err(_)=>Err("Node command slots are busy; no action was dispatched".into())}
                        }};
                        let mut response=match result{Ok(result)=>json!({"type":"result","request_id":id,"result":result}),Err(error)=>json!({"type":"result","request_id":id,"error":error})};
                        if response.to_string().len()>1024*1024{response=json!({"type":"result","request_id":id,"error":"Result exceeds frame limit; use a bounded cursor or transfer"});}
                        let _=reply.send(response).await;
                    });
                } else {bail!("Unexpected control-plane message");}
            },
            Some(result)=incoming_results.recv()=>{tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Text(result.to_string().into()))).await??;}
        }
    }
}
#[derive(Clone)]
struct ConnectionScope {
    node_id: String,
    generation: u64,
    credential_version: u64,
    projects: Vec<String>,
}
struct Execution {
    connection_scope: std::sync::Mutex<Option<ConnectionScope>>,
    credential_deadlines:std::sync::Mutex<std::collections::HashMap<String,tokio::time::Instant>>,
    credential_revalidation:tokio::sync::Mutex<()>,
    file_jobs: std::sync::Mutex<std::collections::HashSet<String>>,
    files: Arc<tokio::sync::Semaphore>,
    processor: super::commands::Processor,
    deadline: std::sync::Mutex<tokio::time::Instant>,
    work: Arc<tokio::sync::Semaphore>,
    control: Arc<tokio::sync::Semaphore>,
}
async fn watchdog(execution: Arc<Execution>, node: String) -> anyhow::Result<()> {
    loop {
        if tokio::time::Instant::now() >= *execution.deadline.lock().unwrap() {
            if let Err(error) = execution.processor.expire(&node).await {
                tracing::error!(%error,"Remote lease stop reconciliation failed");
            }
        }
        if let Err(error)=credentials::enforce(&execution).await{tracing::warn!(%error,"Credential stop/cleanup remains unconfirmed");}
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub async fn run(root: &Path) -> anyhow::Result<()> {
    let mut config = load(root)?;
    endpoint(&config.server, "/")?;
    let execution = Arc::new(Execution {
        connection_scope: std::sync::Mutex::new(None),
        credential_deadlines:std::sync::Mutex::new(std::collections::HashMap::new()),
        credential_revalidation:tokio::sync::Mutex::new(()),
        file_jobs: std::sync::Mutex::new(std::collections::HashSet::new()),
        files: Arc::new(tokio::sync::Semaphore::new(2)),
        processor: super::commands::Processor::new(
            crate::store::Store::open(root.join("muteki.sqlite"))?,
            root.join("runner/control.sock"),
        ),
        deadline: std::sync::Mutex::new(tokio::time::Instant::now()),
        work: Arc::new(tokio::sync::Semaphore::new(4)),
        control: Arc::new(tokio::sync::Semaphore::new(8)),
    });
    execution
        .processor
        .expire(&config.credential.node_id)
        .await
        .map_err(anyhow::Error::msg)?;
    transfer::reconcile(execution.processor.store()).map_err(anyhow::Error::msg)?;
    let node = config.credential.node_id.clone();
    tokio::select! {result=watchdog(execution.clone(),node)=>result,result=reconnect(root,&mut config,&execution)=>result}
}
async fn reconnect(
    root: &Path,
    config: &mut Configuration,
    execution: &Arc<Execution>,
) -> anyhow::Result<()> {
    let mut failures = 0u32;
    loop {
        let started = tokio::time::Instant::now();
        if let Err(error) = connected(root, config, execution).await {
            tracing::warn!(error=%error,"Remote Runner connection unavailable; reconnecting");
        }
        failures = if started.elapsed() >= Duration::from_secs(60) {
            1
        } else {
            failures.saturating_add(1).min(5)
        };
        let mut jitter = [0u8; 1];
        getrandom::getrandom(&mut jitter).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        tokio::time::sleep(Duration::from_millis(
            (1u64 << failures) * 1000 + u64::from(jitter[0]) * 4,
        ))
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_never_sent_to_non_https_or_ambiguous_origins() {
        for origin in [
            "http://localhost:8100",
            "https://user:secret@example.com",
            "https://example.com/path",
            "https://example.com?token=x",
            "https://example.com#fragment",
        ] {
            assert!(endpoint(origin, "/v1/runner/enroll").is_err());
        }
        assert_eq!(
            endpoint("https://example.com:8443", "/v1/runner/enroll")
                .unwrap()
                .as_str(),
            "https://example.com:8443/v1/runner/enroll"
        );
    }
}
