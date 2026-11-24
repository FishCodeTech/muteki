//! Local Runner service. Its private socket is a process boundary, not a public API.
use axum::{Json, Router, routing::get};
use serde_json::{Value, json};
use std::{path::{Path, PathBuf}, time::Duration};

/// Command identities must not depend on HashMap iteration or JSON key order.
/// The prefix distinguishes new receipts from pre-canonicalization records.
pub(crate) fn canonical_digest(mut value:Value)->Result<String,String>{
    use sha2::{Digest,Sha256};
    value.sort_all_objects();
    let bytes=serde_json::to_vec(&value).map_err(|e|e.to_string())?;
    Ok(format!("canonical-v1:{:x}",Sha256::digest(bytes)))
}

pub const PROTOCOL: u64 = 1;
static LOCAL_SOCKET: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
struct LocalOwner { pid:u32, instance_id:String }
static LOCAL_OWNER:tokio::sync::Mutex<Option<LocalOwner>>=tokio::sync::Mutex::const_new(None);
static INSTANCE_ID:std::sync::LazyLock<String>=std::sync::LazyLock::new(||uuid::Uuid::new_v4().to_string());
/// Set once the owning app asked the local Runner to stop, so supervision does not restart it.
static LOCAL_STOPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SHUTDOWN: std::sync::LazyLock<tokio::sync::Notify> = std::sync::LazyLock::new(tokio::sync::Notify::new);

pub fn configured_socket() -> Option<PathBuf> {
    if std::env::args().nth(1).as_deref() == Some("runner") { return None; }
    std::env::var_os("MUTEKI_RUNNER_SOCKET").filter(|s| !s.is_empty()).map(PathBuf::from)
        .or_else(|| LOCAL_SOCKET.get().cloned())
}

/// Stops only the Runner this Web process spawned and still owns. Attached and
/// explicitly configured Runners can be shared and are never stopped here.
pub async fn stop_local()->Result<bool,String>{
    if std::env::var_os("MUTEKI_RUNNER_SOCKET").is_some_and(|s|!s.is_empty()){return Ok(false);}
    let mut owner=LOCAL_OWNER.lock().await;
    let Some(socket)=LOCAL_SOCKET.get().cloned() else {return Ok(false)};
    LOCAL_STOPPED.store(true,std::sync::atomic::Ordering::SeqCst);
    let Some(expected)=owner.as_ref() else{return Ok(false)};
    let health=match request(&socket,"/v1/health").await{
        Ok(health)=>health,
        Err(error)=>{LOCAL_STOPPED.store(false,std::sync::atomic::Ordering::SeqCst);return Err(error)}
    };
    if health["pid"].as_u64()!=Some(u64::from(expected.pid))||health["instance_id"]!=expected.instance_id{
        *owner=None;
        return Ok(false);
    }
    match exchange(&socket,"/v1/shutdown",Some(json!({"expected_pid":expected.pid,"expected_instance_id":expected.instance_id}))).await {
        Ok(_)=>{*owner=None;Ok(true)},
        Err(error)=>{LOCAL_STOPPED.store(false,std::sync::atomic::Ordering::SeqCst);Err(error)}
    }
}

fn remember_local(socket:PathBuf)->Result<(),String>{
    if let Some(current)=LOCAL_SOCKET.get(){return if current==&socket{Ok(())}else{Err("Local Runner root changed".into())};}
    LOCAL_SOCKET.set(socket.clone()).or_else(|_|if LOCAL_SOCKET.get()==Some(&socket){Ok(())}else{Err(socket)})
        .map_err(|_|"Local Runner root changed".into())
}

/// Select the process boundary before probing it. An unavailable Runner must
/// remain an explicit dependency error, never turn into in-process execution.
pub fn configure_local(root:&Path)->Result<(),String>{
    if std::env::var_os("MUTEKI_RUNNER_SOCKET").is_some_and(|socket|!socket.is_empty()){return Ok(());}
    remember_local(root.join("runner/control.sock"))
}

pub fn supervise_local(root:PathBuf){
    if std::env::var_os("MUTEKI_RUNNER_SOCKET").is_some_and(|socket|!socket.is_empty()){return;}
    tokio::spawn(async move {loop {
        if LOCAL_STOPPED.load(std::sync::atomic::Ordering::SeqCst){return;}
        let socket=root.join("runner/control.sock");
        if matches!(tokio::time::timeout(Duration::from_secs(2),request(&socket,"/v1/health")).await,Ok(Ok(value)) if value["protocol_version"]==PROTOCOL&&value["service"]=="muteki-runner"){
            tokio::time::sleep(Duration::from_secs(2)).await;continue;
        }
        if let Err(error)=ensure_local(&root).await{
            tracing::error!(%error,"Local Runner recovery is pending");
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }});
}

/// Start once, then reattach across Web restarts. The Runner outlives its Web parent.
pub async fn ensure_local(root:&Path)->Result<(),String>{
    let explicit=std::env::var_os("MUTEKI_RUNNER_SOCKET").filter(|s|!s.is_empty());
    let mut owner=LOCAL_OWNER.lock().await;
    if explicit.is_none()&&LOCAL_STOPPED.load(std::sync::atomic::Ordering::SeqCst){return Err("Local Runner supervision was stopped by its owning app".into());}
    let socket=explicit.as_ref().map(PathBuf::from).unwrap_or_else(||root.join("runner/control.sock"));
    let probe=async {
        let value=tokio::time::timeout(Duration::from_secs(2),request(&socket,"/v1/health")).await
            .map_err(|_|"Runner health timed out".to_owned())??;
        if value["protocol_version"]!=PROTOCOL||value["service"]!="muteki-runner"{return Err("Runner protocol is incompatible".to_owned());}
        Ok(())
    };
    if probe.await.is_ok(){
        if explicit.is_none(){remember_local(socket)?;}
        return Ok(());
    }
    if explicit.is_some(){return Err("Configured Runner is unavailable or incompatible".into());}
    let directory=root.join("runner");std::fs::create_dir_all(&directory).map_err(|e|e.to_string())?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::symlink_metadata(&directory).map_err(|e|e.to_string())?.file_type().is_symlink(){return Err("Runner directory cannot be a symlink".into());}
        std::fs::set_permissions(&directory,std::fs::Permissions::from_mode(0o700)).map_err(|e|e.to_string())?;
    }
    let log=std::fs::OpenOptions::new().create(true).append(true).open(directory.join("runner.log")).map_err(|e|e.to_string())?;
    let mut child=crate::host_process::command(std::env::current_exe().map_err(|e|e.to_string())?)
        .args(["runner","--state-dir"]).arg(root).env_remove("MUTEKI_RUNNER_SOCKET")
        .stdin(std::process::Stdio::null()).stdout(log.try_clone().map_err(|e|e.to_string())?).stderr(log)
        .kill_on_drop(false).spawn().map_err(|e|e.to_string())?;
    let child_pid=child.id().ok_or_else(||"Local Runner child has no process identity".to_owned())?;
    tokio::spawn(async move {if let Err(error)=child.wait().await{tracing::error!(%error,"Runner child wait failed");}});
    let deadline=tokio::time::Instant::now()+Duration::from_secs(20);
    loop {
        if let Ok(Ok(value))=tokio::time::timeout(Duration::from_secs(2),request(&socket,"/v1/health")).await {
            if value["protocol_version"]!=PROTOCOL||value["service"]!="muteki-runner"{return Err("Runner protocol is incompatible".into());}
            remember_local(socket)?;
            if value["pid"].as_u64()==Some(u64::from(child_pid)){
                let instance_id=value["instance_id"].as_str().filter(|id|!id.is_empty()).ok_or_else(||"Local Runner has no instance identity".to_owned())?;
                *owner=Some(LocalOwner{pid:child_pid,instance_id:instance_id.to_owned()});
            }else{*owner=None;}
            return Ok(());
        }
        if tokio::time::Instant::now()>=deadline{return Err("Runner did not become ready; inspect runner/runner.log".into());}
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(unix)]
async fn exchange_response(socket: &Path, path: &str, body: Option<Value>) -> Result<(reqwest::StatusCode, Value), String> {
    let client = crate::network_proxy::direct_client_builder().unix_socket(socket)
        .timeout(Duration::from_secs(120)).build().map_err(|e| e.to_string())?;
    if path != "/v1/health" {
        let health: Value = client.get("http://localhost/v1/health").send().await
            .map_err(|e| e.to_string())?.error_for_status().map_err(|e| e.to_string())?
            .json().await.map_err(|e| e.to_string())?;
        if health["protocol_version"] != PROTOCOL || health["service"] != "muteki-runner" {
            return Err("Runner protocol is incompatible".into());
        }
    }
    let url=format!("http://localhost{path}");
    let req=match body {Some(body)=>client.post(url).json(&body),None=>client.get(url)};
    let response=req.send().await.map_err(|e| e.to_string())?;
    let status=response.status();
    let body=response.text().await.map_err(|e|e.to_string())?;
    let value:Value=serde_json::from_str(&body).map_err(|_|format!("Runner returned non-JSON response (HTTP {status}): {body}"))?;
    Ok((status, value))
}

#[cfg(not(unix))]
async fn exchange_response(_: &Path, _: &str, _: Option<Value>) -> Result<(reqwest::StatusCode, Value), String> {
    Err("Local Runner IPC is not supported on this platform yet".into())
}

pub async fn exchange(socket: &Path, path: &str, body: Option<Value>) -> Result<Value, String> {
    let (status, value) = exchange_response(socket, path, body).await?;
    checked_response(status, value)
}

fn checked_response(status: reqwest::StatusCode, value: Value) -> Result<Value, String> {
    if !status.is_success() {
        return Err(value["error"].as_str().unwrap_or("Runner request failed").into());
    }
    Ok(value)
}

pub(crate) async fn run_status_if_recorded(socket: &Path, run_id: &str) -> Result<Option<Value>, String> {
    let (status, value) = exchange_response(socket, &format!("/v1/runs/{run_id}"), None).await?;
    if status == reqwest::StatusCode::NOT_FOUND && value["code"] == "runner.run_not_recorded" {
        return Ok(None);
    }
    checked_response(status, value).map(Some)
}

pub async fn request(socket: &Path,path:&str)->Result<Value,String>{exchange(socket,path,None).await}

pub async fn push_shell_environment() {
    #[cfg(unix)]
    {
        let socket = LOCAL_SOCKET
            .get()
            .cloned()
            .or_else(|| std::env::var_os("MUTEKI_RUNNER_SOCKET").filter(|s| !s.is_empty()).map(PathBuf::from));
        let Some(socket) = socket.filter(|path| path.exists()) else {
            return;
        };
        let payload = crate::shell_env::export_payload();
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            exchange_response(&socket, "/v1/shell-environment", Some(payload)),
        )
        .await;
    }
}

/// Bridge existing synchronous verification callers without nesting a Tokio runtime.
pub(crate) fn inspect_src_sync(name:&str)->Option<Result<Option<Value>,String>>{
    let socket=configured_socket()?;let name=name.to_owned();
    Some(std::thread::spawn(move||{
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e|e.to_string())?;
        let value=runtime.block_on(exchange(&socket,"/v1/src/runtime-inspect",Some(json!({"container_name":name}))))?;
        Ok((!value["container"].is_null()).then(||value["container"].clone()))
    }).join().unwrap_or_else(|_|Err("Runner inspect bridge failed".into())))
}

async fn health() -> Json<Value> {
    Json(json!({"protocol_version":PROTOCOL,"service":"muteki-runner",
        "version":env!("CARGO_PKG_VERSION"),"pid":std::process::id(),"instance_id":INSTANCE_ID.as_str(),
        "capabilities":["node_resources","worker_image_inspection","container_lifecycle","worker_execution","event_replay"]}))
}
/// Graceful stop requested by the owning app. Long-lived streams from the Web
/// process can hold graceful shutdown open, so exit regardless after a grace period.
async fn shutdown(Json(body):Json<Value>) -> Result<Json<Value>,(axum::http::StatusCode,Json<Value>)> {
    if body.get("expected_pid").is_some_and(|pid|pid.as_u64()!=Some(u64::from(std::process::id())))
        ||body.get("expected_instance_id").is_some_and(|id|id.as_str()!=Some(INSTANCE_ID.as_str())){
        return Err((axum::http::StatusCode::CONFLICT,Json(json!({"error":{"code":"runner.owner_changed","message":"Runner instance changed; this process does not own its replacement"}}))));
    }
    tracing::info!("Runner shutdown requested by the owning app");
    SHUTDOWN.notify_one();
    tokio::spawn(async {tokio::time::sleep(Duration::from_secs(10)).await;tracing::warn!("Runner graceful shutdown timed out; exiting");std::process::exit(0);});
    Ok(Json(json!({"stopping":true,"pid":std::process::id()})))
}
async fn image() -> Json<Value> { Json(crate::runs::container_exec::inspect_worker_image_status_local().await) }
async fn pull_image()->Json<Value>{let (ok,result)=crate::runs::container_exec::pull_or_verify_worker_image().await;Json(json!({"ok":ok,"result":result}))}
async fn engine_health()->Json<Value>{Json(crate::native_credentials::container_health_local().await)}
async fn codex_catalog()->Result<Json<Value>,(axum::http::StatusCode,Json<Value>)>{
    crate::runs::container_exec::run_container_codex_bundled_catalog().await.map(|rows|Json(json!(rows)))
        .map_err(|e|(axum::http::StatusCode::CONFLICT,Json(json!({"error":e.to_string()}))))
}
async fn update_shell_environment(Json(body): Json<Value>) -> Json<Value> {
    let changed = body
        .get("env")
        .and_then(Value::as_object)
        .is_some_and(|env_obj| {
            let changed = crate::shell_env::apply_external_env(env_obj, "server_push", None);
            crate::host_process::sync_runtime_caches();
            changed
        });
    Json(json!({"ok": true, "changed": changed}))
}
fn router() -> Router {
    Router::new().route("/v1/health",get(health)).route("/v1/shutdown",axum::routing::post(shutdown))
        .route("/v1/worker-image",get(image))
        .route("/v1/worker-image/pull",axum::routing::post(pull_image))
        .route("/v1/engines/health",get(engine_health)).route("/v1/catalog/codex-bundled",get(codex_catalog))
        .route("/v1/shell-environment", axum::routing::post(update_shell_environment))
}

#[cfg(unix)]
pub async fn serve(root: &Path, stop: impl std::future::Future<Output=()> + Send + 'static) -> anyhow::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt,PermissionsExt,FileTypeExt};
    use fs2::FileExt;
    let directory = root.join("runner");
    std::fs::create_dir_all(&directory)?;
    if std::fs::symlink_metadata(&directory)?.file_type().is_symlink() {
        anyhow::bail!("Runner state directory must not be a symlink");
    }
    std::fs::set_permissions(&directory,std::fs::Permissions::from_mode(0o700))?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW).open(directory.join("runner.lock"))?;
    lock.try_lock_exclusive().map_err(|e| anyhow::anyhow!("Runner already owns this state directory: {e}"))?;
    let socket = directory.join("control.sock");
    if let Ok(metadata) = std::fs::symlink_metadata(&socket) {
        if !metadata.file_type().is_socket() { anyhow::bail!("Runner control path is not a socket"); }
    }
    match std::fs::remove_file(&socket) { Ok(()) => {}, Err(e) if e.kind()==std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) }
    let listener = tokio::net::UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket,std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket=%socket.display(),"Runner listening");
    crate::shell_env::init_from_cache(root);
    crate::host_process::sync_runtime_caches();
    crate::host_process::spawn_background_refresh(None, root.to_path_buf(), Duration::ZERO);
    let execution=crate::runner_execution::Execution::open(root)?;
    execution.reconcile().await.map_err(anyhow::Error::msg)?;
    crate::runner_execution::Execution::spawn_reconciliation(&execution);
    let stop=async move {tokio::select!{_=stop=>{},_=SHUTDOWN.notified()=>{}}};
    let result = axum::serve(listener,router().merge(crate::runner_execution::router(execution))).with_graceful_shutdown(stop).await;
    let cleanup = std::fs::remove_file(&socket);
    result?;
    cleanup?;
    Ok(())
}

#[cfg(not(unix))]
pub async fn serve(_: &Path, _: impl std::future::Future<Output=()> + Send + 'static) -> anyhow::Result<()> {
    anyhow::bail!("Local Runner IPC is not supported on this platform yet")
}

pub async fn entry() -> anyhow::Result<()> {
    use clap::Parser;
    if std::env::args().nth(2).as_deref()==Some("enroll") {
        #[derive(Parser)]
        struct Enrollment {#[arg(long,default_value=".state")]state_dir:PathBuf,#[arg(long)]server:String,
            #[arg(long)]token_file:PathBuf,#[arg(long)]ca_file:Option<PathBuf>,
            /// SHA-256 of the Runner CA shown in the Web enrollment command.
            #[arg(long)]ca_sha256:Option<String>,
            /// Replace an existing identity (for example after the old node was revoked).
            #[arg(long)]replace:bool}
        let options=Enrollment::parse_from(std::env::args().take(1).chain(std::env::args().skip(3)));
        crate::runner_remote::client::enroll(&options.state_dir,&options.server,&options.token_file,options.ca_file.as_deref(),options.ca_sha256.as_deref(),options.replace).await?;
        println!("Remote Runner identity saved privately. Start it with: muteki-server runner --remote --state-dir {}",options.state_dir.display());return Ok(());
    }
    if std::env::args().nth(2).as_deref()==Some("install-credential") {
        #[derive(Parser)]
        struct Install {#[arg(long,default_value=".state")]state_dir:PathBuf,
            /// JSON credential shown once after rotating this node in Web settings.
            #[arg(long)]credential_file:PathBuf}
        let options=Install::parse_from(std::env::args().take(1).chain(std::env::args().skip(3)));
        crate::runner_remote::client::install_credential(&options.state_dir,&options.credential_file)?;
        println!("Rotated credential installed. Start the Runner again with: muteki-server runner --remote --state-dir {}",options.state_dir.display());return Ok(());
    }
    if std::env::args().nth(2).as_deref()==Some("status") {
        #[derive(Parser)]
        struct Status {#[arg(long,default_value=".state")]state_dir:PathBuf}
        let options=Status::parse_from(std::env::args().take(1).chain(std::env::args().skip(3)));
        println!("{}",serde_json::to_string_pretty(&crate::runner_remote::client::status(&options.state_dir)?)?);return Ok(());
    }
    #[derive(Parser)]
    struct Options { #[arg(long, default_value=".state")] state_dir: PathBuf, #[arg(long)]remote:bool }
    let options = Options::parse_from(std::env::args().take(1).chain(std::env::args().skip(2)));
    if options.remote {
        let socket=options.state_dir.join("runner/control.sock");
        let connect=async {
            loop {if let Ok(value)=request(&socket,"/v1/health").await {if value["pid"]==std::process::id(){break;}}
                tokio::time::sleep(Duration::from_millis(100)).await;}
            crate::runner_remote::client::run(&options.state_dir).await
        };
        tokio::select! {result=serve(&options.state_dir,async {let _=tokio::signal::ctrl_c().await;})=>result,result=connect=>result}
    }else {serve(&options.state_dir,async { let _=tokio::signal::ctrl_c().await; }).await}
}

#[cfg(all(test,unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn private_ipc_reports_version_and_rejects_second_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        let socket = root.join("runner/control.sock");
        let (tx,rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move { serve(&root,async { let _=rx.await; }).await });
        let value = tokio::time::timeout(Duration::from_secs(5),async {
            loop { if let Ok(value)=request(&socket,"/v1/health").await {break value;}
                tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await.unwrap();
        assert_eq!(value["protocol_version"],PROTOCOL);
        assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,0o600);
        assert!(serve(dir.path(),std::future::pending()).await.is_err());
        assert!(request(&socket,"/v1/arbitrary-docker-command").await.is_err());
        tx.send(()).unwrap();
        handle.await.unwrap().unwrap();
        assert!(!socket.exists());
    }
}

#[cfg(test)]
mod canonical_receipt_tests {
    #[test]
    fn object_order_is_not_command_identity_but_values_are(){
        let a:serde_json::Value=serde_json::from_str(r#"{"spec":{"env":{"Z":"z","A":"a"},"argv":["one","two"]},"generation":1}"#).unwrap();
        let b:serde_json::Value=serde_json::from_str(r#"{"generation":1,"spec":{"argv":["one","two"],"env":{"A":"a","Z":"z"}}}"#).unwrap();
        assert_eq!(super::canonical_digest(a).unwrap(),super::canonical_digest(b.clone()).unwrap());
        let mut changed:serde_json::Value=b;changed["spec"]["env"]["A"]=serde_json::json!("different");
        let changed=super::canonical_digest(changed).unwrap();
        assert!(changed.starts_with("canonical-v1:"));
        let original=serde_json::from_str(r#"{"generation":1,"spec":{"argv":["one","two"],"env":{"A":"a","Z":"z"}}}"#).unwrap();
        assert_ne!(changed,super::canonical_digest(original).unwrap());
    }
}
