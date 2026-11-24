//! Run admission accepts evidence from a real CLI request under the exact
//! profile, credential revision, executable and network identity.
use crate::{AppState,agent_runtime::{RuntimeError,binaries,credentials,descriptors},native_chat,runtime_api};
use serde_json::{Value,json};
use sha2::{Digest,Sha256};
use std::{collections::{BTreeMap,HashMap},sync::{Arc,Mutex,OnceLock},time::{Duration,Instant}};
use tokio::io::{AsyncReadExt,AsyncWriteExt};

struct Entry { at: Instant, result: Value }
static SUCCESSES: OnceLock<Mutex<HashMap<String,Entry>>> = OnceLock::new();
static LOCKS: OnceLock<Mutex<HashMap<String,Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
fn failure(code: &'static str,message: impl Into<String>) -> RuntimeError { RuntimeError::new(code,message) }
struct ProcessGroup(Option<u32>);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)] if let Some(pid) = self.0 { unsafe { libc::kill(-(pid as i32),libc::SIGKILL); } }
    }
}

pub(super) async fn local_probe(state: &AppState,engine: &str,credential: &str,model: &str,effort: &str,instance: &Value) -> Result<Value,RuntimeError> {
    let descriptor = descriptors::find(engine).ok_or_else(||failure("engine.unknown","Unknown Worker engine"))?;
    let selected = instance["binary_path"].as_str().filter(|value|!value.is_empty()).map(std::path::Path::new);
    let binary = binaries::resolve(descriptor,selected).await?;
    let resolved = credentials::resolve(&state.store,&native_chat::state_dir(state),descriptor,credential,Some(model)).await?;
    let mut env: HashMap<String,String> = resolved.env.into_iter().collect();
    for (key,value) in credentials::env_refs_with_platform(state,&instance["env_refs"])? { env.insert(key,value); }
    for (key,value) in instance["env"].as_object().into_iter().flatten().filter_map(|(key,value)|Some((key,value.as_str()?))) { env.insert(key.clone(),value.into()); }
    if !env.contains_key("PATH")
        && let Ok(path_env) = binaries::launch_path(&binary.path)
    {
        env.insert("PATH".to_owned(), path_env);
    }
    let prompt = "Reply with the single word OK and nothing else.";
    let (mut argv, input) = if engine == "kimi" {
        let model = model.trim();
        let mut argv = vec!["kimi".into(), "--output-format".into(), "stream-json".into(),
            "--prompt".into(), prompt.into()];
        let env_model = env.get("KIMI_MODEL_NAME").cloned()
            .or_else(|| std::env::var("KIMI_MODEL_NAME").ok());
        let uses_env_model = env_model.as_deref().is_some_and(|name| {
            name == model || model.strip_prefix("kimi-code/") == Some(name)
        });
        if !model.is_empty() && !uses_env_model {
            argv.extend(["--model".into(), model.into()]);
        }
        (argv, None)
    } else {
        super::container_exec::build_container_cli_invocation(engine, Some(model), Some(effort), prompt, &env, false)?
    };
    argv[0] = binary.path.to_string_lossy().into_owned();
    if engine == "pi" { argv.push("--no-tools".into()); }
    let scratch = tempfile::Builder::new().prefix("muteki-cli-readiness-").tempdir()
        .map_err(|error|failure("runtime.io_error",error.to_string()))?;
    let mut command = tokio::process::Command::from(agent::supervised_command(&argv[0]));
    command.args(&argv[1..]).current_dir(scratch.path()).envs(&env).kill_on_drop(true)
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    #[cfg(unix)] command.process_group(0);
    let mut child = command.spawn().map_err(|error|failure("runtime.spawn_failed",error.to_string()))?;
    let _group = ProcessGroup(child.id());
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(input) = input { stdin.write_all(input.as_bytes()).await.map_err(|error|failure("runtime.io_error",error.to_string()))?; }
        stdin.shutdown().await.map_err(|error|failure("runtime.io_error",error.to_string()))?;
    }
    let mut stdout = child.stdout.take().ok_or_else(||failure("runtime.io_error","CLI stdout unavailable"))?.take(2*1024*1024+1);
    let mut stderr = child.stderr.take().ok_or_else(||failure("runtime.io_error","CLI stderr unavailable"))?.take(2*1024*1024+1);
    let output = async {
        let mut out = Vec::new(); let mut err = Vec::new();
        let (status,_,_) = tokio::try_join!(child.wait(),stdout.read_to_end(&mut out),stderr.read_to_end(&mut err))?;
        Ok::<_,std::io::Error>((status,out,err))
    };
    let (status,out,err) = tokio::time::timeout(Duration::from_secs(45),output).await
        .map_err(|_|failure("runtime.probe_timeout","Worker CLI model request timed out"))?
        .map_err(|error|failure("runtime.io_error",error.to_string()))?;
    if out.len()>2*1024*1024 { return Err(failure("runtime.probe_output_limit","Worker CLI exceeded its output limit")); }
    let stderr_tail = {
        let text = String::from_utf8_lossy(&err);
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).rev().take(4).collect();
        lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
    };
    if !status.success() {
        let code_str = status.code().map_or("signal".into(),|code|code.to_string());
        let msg = if stderr_tail.is_empty() {
            format!("Worker CLI exited with {code_str}")
        } else {
            format!("Worker CLI exited with {code_str}: {stderr_tail}")
        };
        return Err(failure("runtime.probe_failed", msg));
    }
    let mut reply = String::new();
    let mut reported_model = None;
    let mut model_identity_source = None;
    for line in String::from_utf8_lossy(&out).lines() {
        if let Some(event) = super::container_exec::parse_container_cli_json_line(line) {
            if let Some(ev_err) = event.error_message {
                let msg = if stderr_tail.is_empty() { ev_err } else { format!("{ev_err} ({stderr_tail})") };
                return Err(failure("runtime.probe_failed", msg));
            }
            if event.reported_model.is_some() {
                reported_model = event.reported_model;
                model_identity_source = event.model_identity_source;
            }
            for text in event.assistant_texts { reply.push_str(&text); }
        }
    }
    if reply.trim().is_empty() {
        let msg = if stderr_tail.is_empty() {
            "Worker CLI returned no assistant text".to_owned()
        } else {
            format!("Worker CLI returned no assistant text ({stderr_tail})")
        };
        return Err(failure("runtime.probe_empty", msg));
    }
    let mut result = super::container_exec::cli_prompt_result(Some(model),reported_model.as_deref(),model_identity_source,&reply)?;
    result["ok"] = json!(true);
    result["backend"] = json!("local");
    result["execution_transport"] = json!("cli");
    Ok(result)
}

async fn identity(state: &AppState,profile: &Value,backend: &str,network: &str,credential: &str,runtime: &str,instance: &Value) -> Result<Option<String>,RuntimeError> {
    let Some(id) = credential.strip_prefix("account:") else { return Ok(None) };
    let account = state.store.get("credential_accounts",id).map_err(|error|failure("storage.failed",error.to_string()))?;
    let revision = account.as_ref().and_then(|row|row["revision"].as_str()).unwrap_or("");
    if revision.is_empty() { return Ok(None); }
    let executable = if backend == "container" {
        let image = super::container_exec::inspect_worker_image_status().await;
        if image["image_id"].is_null() && image["image"]["id"].is_null() { return Ok(None); }
        image
    } else {
        let engine = super::settings::normalize_engine(profile["engine"].as_str().unwrap_or(""));
        let descriptor = descriptors::find(&engine).ok_or_else(||failure("engine.unknown","Unknown Worker engine"))?;
        let binary = binaries::resolve(descriptor,instance["binary_path"].as_str().filter(|value|!value.is_empty()).map(std::path::Path::new)).await?;
        let metadata = std::fs::metadata(&binary.path).map_err(|error|failure("runtime.io_error",error.to_string()))?;
        let helpers: BTreeMap<String,Value> = ["node","python3"].into_iter().map(|name| {
            let resolved = crate::shell_env::resolve_program(std::ffi::OsStr::new(name));
            let path = crate::shell_env::is_executable_file(&resolved)
                .then(|| resolved.canonicalize().ok())
                .flatten();
            let identity = path.and_then(|path|std::fs::metadata(&path).ok().map(|metadata|json!({"path":path,"size":metadata.len(),
                "modified":metadata.modified().ok().and_then(|time|time.duration_since(std::time::UNIX_EPOCH).ok()).map(|time|time.as_nanos().to_string())})));
            (name.to_owned(),identity.unwrap_or(Value::Null))
        }).collect();
        json!({"path":binary.path.canonicalize().unwrap_or(binary.path),"size":metadata.len(),"modified":metadata.modified().ok().and_then(|time|time.duration_since(std::time::UNIX_EPOCH).ok()).map(|time|time.as_nanos().to_string()),"helpers":helpers})
    };
    let mut environment: BTreeMap<String,String> = std::env::vars_os().map(|(key,value)|(key.to_string_lossy().into_owned(),value.to_string_lossy().into_owned())).collect();
    for (key, value) in crate::host_process::host_environment() {
        environment.insert(key, value);
    }
    let material = json!({"contract":"worker-cli-model-v3","profile":profile,"credential_id":credential,"revision":revision,
        "state_dir":native_chat::state_dir(state),"backend":backend,"network":network,"runtime_key":runtime,"instance":instance,"executable":executable,"environment":environment});
    Ok(Some(format!("{:x}",Sha256::digest(serde_json::to_vec(&material).map_err(|error|failure("runtime.io_error",error.to_string()))?))))
}

pub async fn check(state: &AppState,profile: &Value,backend: &str,network: &str,credential: &str,runtime: &str,settings: &Value) -> Result<Value,RuntimeError> {
    let ttl = std::env::var("MUTEKI_WORKER_READINESS_CACHE_TTL_SECONDS").ok().map(|value|value.parse::<f64>()).transpose()
        .map_err(|_|failure("worker.readiness.ttl_invalid","Readiness cache TTL must be finite and nonnegative"))?.unwrap_or(0.0);
    if !ttl.is_finite() || ttl < 0.0 { return Err(failure("worker.readiness.ttl_invalid","Readiness cache TTL must be finite and nonnegative")); }
    let instance = state.store.get(runtime_api::INSTANCES,runtime).map_err(|error|failure("storage.failed",error.to_string()))?.unwrap_or(Value::Null);
    let key = identity(state,profile,backend,network,credential,runtime,&instance).await?;
    let lock = key.as_ref().filter(|_|ttl>0.0).map(|key| {
        let mut locks = LOCKS.get_or_init(||Mutex::new(HashMap::new())).lock().unwrap();
        locks.retain(|_,lock|Arc::strong_count(lock)>1);
        locks.entry(key.clone()).or_insert_with(||Arc::new(tokio::sync::Mutex::new(()))).clone()
    });
    let _guard = match lock.as_ref() {Some(lock)=>Some(lock.lock().await),None=>None};
    if ttl>0.0 && let Some(key) = &key {
        let mut cache = SUCCESSES.get_or_init(||Mutex::new(HashMap::new())).lock().unwrap();
        cache.retain(|_,entry|entry.at.elapsed().as_secs_f64()<ttl);
        if let Some(entry) = cache.get(key) { let mut result=entry.result.clone(); result["cached"]=json!(true); return Ok(result); }
    }
    let engine = super::settings::normalize_engine(profile["engine"].as_str().unwrap_or(""));
    let explicit_bin = instance["binary_path"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(std::path::Path::new);
    crate::agent_runtime::version_policy::verify_worker_engine(
        state,
        &engine,
        backend,
        settings["worker_node_id"].as_str().unwrap_or("local"),
        settings["mode"].as_str(),
        explicit_bin,
        None,
    )
    .await?;
    let model = profile["model"].as_str().unwrap_or("");
    let effort = profile["reasoning_effort"].as_str().unwrap_or("default");
    if model.is_empty() { return Err(failure("model.required","Select an explicit Worker model before starting a Run")); }
    let result = if backend == "container" {
        super::container_exec::run_container_model_probe(state,&engine,credential,Some(model),Some(effort),settings).await?
    } else { local_probe(state,&engine,credential,model,effort,&instance).await? };
    let current_instance = state.store.get(runtime_api::INSTANCES,runtime).map_err(|error|failure("storage.failed",error.to_string()))?.unwrap_or(Value::Null);
    if current_instance != instance || identity(state,profile,backend,network,credential,runtime,&current_instance).await? != key {
        return Err(failure("worker.readiness.identity_changed","Worker configuration changed during readiness probe"));
    }
    if ttl>0.0 && let Some(key) = key { SUCCESSES.get_or_init(||Mutex::new(HashMap::new())).lock().unwrap().insert(key,Entry{at:Instant::now(),result:result.clone()}); }
    Ok(result)
}
