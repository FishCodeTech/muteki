//! Task-owned browser containers behind a path-checking Docker egress gateway.
use super::{
    Api, Error, in_policy, now, objects, policy, put_artifact, rec, records, require, save_rec,
    task, verify_ref,
};
use crate::AppState;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const GATEWAY_IMAGE: &str = "muteki-src-egress:latest";
const BROWSER_IMAGE: &str = "muteki-src-browser:latest";
fn token(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..24].to_owned()
}
fn names(id: &str) -> (String, String) {
    let hash = token(id);
    (
        format!("muteki-src-egress-{hash}"),
        format!("muteki-src-browser-{hash}"),
    )
}
pub(super) fn root(s: &AppState, id: &str) -> Api<PathBuf> {
    task(s, id)?;
    let state = s
        .store
        .get("system", "state_dir")
        .map_err(crate::runs::src_api::db)?
        .and_then(|v| v.as_str().map(PathBuf::from))
        .ok_or_else(|| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "capability.unavailable",
                "managed state_dir unavailable",
            )
        })?;
    let managed = state.join("src-browser");
    if managed
        .symlink_metadata()
        .is_ok_and(|v| v.file_type().is_symlink())
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.browser_root_invalid",
            "Task browser root is a symlink",
        ));
    }
    let path = managed.join(token(id));
    if path
        .symlink_metadata()
        .is_ok_and(|v| v.file_type().is_symlink())
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.browser_root_invalid",
            "Task browser Task directory is a symlink",
        ));
    }
    std::fs::create_dir_all(&path).map_err(crate::runs::src_api::db)?;
    if !path
        .canonicalize()
        .map_err(crate::runs::src_api::db)?
        .starts_with(managed.canonicalize().map_err(crate::runs::src_api::db)?)
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.browser_root_invalid",
            "Task browser directory escapes managed state",
        ));
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .map_err(crate::runs::src_api::db)?;
    Ok(path)
}
fn directory(path: &Path, mode: u32) -> Api<()> {
    if path
        .symlink_metadata()
        .is_ok_and(|v| v.file_type().is_symlink())
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.browser_root_invalid",
            "managed browser directory is a symlink",
        ));
    }
    std::fs::create_dir_all(path).map_err(crate::runs::src_api::db)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(crate::runs::src_api::db)?;
    Ok(())
}
pub(super) async fn docker(args: &[String], seconds: u64) -> Api<String> {
    let mut command = crate::host_process::command("docker");
    command.args(args).kill_on_drop(true);
    let output = timeout(Duration::from_secs(seconds), command.output())
        .await
        .map_err(|_| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "capability.unavailable",
                "Docker command timed out",
            )
        })?
        .map_err(|e| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "capability.unavailable",
                format!("Docker unavailable: {e}"),
            )
        })?;
    if !output.status.success() {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability.unavailable",
            format!(
                "Docker command failed: {}",
                String::from_utf8_lossy(if output.stderr.is_empty() {
                    &output.stdout
                } else {
                    &output.stderr
                })
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
pub(super) async fn inspect(name: &str) -> Api<Option<Value>> {
    if let Some(socket)=crate::runner::configured_socket(){
        let value=crate::runner::exchange(&socket,"/v1/src/runtime-inspect",Some(json!({"container_name":name}))).await
            .map_err(|message|Error::new(StatusCode::SERVICE_UNAVAILABLE,"runner.inspection_failed",message))?;
        return Ok((!value["container"].is_null()).then(||value["container"].clone()));
    }
    let mut command = crate::host_process::command("docker");
    command.args(["inspect", name]).kill_on_drop(true);
    let output = timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "capability.unavailable",
                "Docker inspect timed out",
            )
        })?
        .map_err(|e| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "capability.unavailable",
                format!("Docker unavailable: {e}"),
            )
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let values: Value = serde_json::from_slice(&output.stdout).map_err(crate::runs::src_api::db)?;
    Ok(values.as_array().and_then(|v| v.first()).cloned())
}
pub(super) async fn image(tag: &str, context: Option<PathBuf>) -> Api<()> {
    let args = vec![
        "image".into(),
        "inspect".into(),
        "--format".into(),
        "{{ index .Config.Labels \"muteki.src.source_digest\" }}".into(),
        tag.into(),
    ];
    if docker(&args,10).await.is_ok_and(|label|label==crate::src_resources::image_digest(tag)) {
        return Ok(());
    }
    let context = context.ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability.unavailable",
            format!("browser image {tag} unavailable"),
        )
    })?;
    docker(
        &[
            "build".into(),
            "--label".into(),format!("muteki.src.source_digest={}",crate::src_resources::image_digest(tag)),
            "-t".into(),
            tag.into(),
            context.to_string_lossy().into_owned(),
        ],
        900,
    )
    .await?;
    let label=docker(&args,10).await?;
    if label!=crate::src_resources::image_digest(tag){return Err(Error::conflict("SRC image source identity changed during build"));}
    Ok(())
}
fn latest_image_newer(tag:&str,container:&Value)->bool{container["Config"]["Labels"]["muteki.src.source_digest"]!=crate::src_resources::image_digest(tag)}
pub(super) fn browser_policy(s: &AppState, id: &str) -> Api<Value> {
    let t = task(s, id)?;
    let p = policy(s, id)?;
    if t["lifecycle"]["state"] != "active" {
        return Err(Error::conflict("SRC Task is not active"));
    }
    let mut targets = Vec::new();
    for task_target in t["input"]["authorization"]["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        for policy_target in p["authorized_targets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let selected = if crate::runs::src_api::target_matches(task_target, policy_target) {
                Some(task_target)
            } else if crate::runs::src_api::target_matches(policy_target, task_target) {
                Some(policy_target)
            } else {
                None
            };
            if let Some(selected) = selected {
                if !targets.iter().any(|v| v == selected) {
                    targets.push(selected.to_owned());
                }
            }
        }
    }
    if targets.is_empty() {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "Task browser has no intersecting authorized target",
        ));
    }
    let mut effective=p.clone();effective["authorized_targets"]=json!(targets);effective["environment_generation"]=p["version"].clone();effective["src_task_id"]=json!(id);effective["src_policy_digest"]=p["digest"].clone();effective["gateway_source_digest"]=json!(crate::src_resources::digest("src-egress"));effective["upstream_proxies"]=super::proxypool::policy_projection(s,id)?;Ok(effective)
}
pub(super) fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        canonical_json(&map[key])
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(rows) => format!(
            "[{}]",
            rows.iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => value.to_string(),
    }
}
fn policy_digest(value: &Value) -> String {
    format!("{:x}", Sha256::digest(canonical_json(value).as_bytes()))
}
fn gateway_root(root: &Path) -> Api<PathBuf> {
    let path = root.join("gateway");
    directory(&path, 0o2770)?;
    for sub in [
        "traffic",
        "inspection_requests",
        "inspection_decisions",
        "inspection_grants",
    ] {
        directory(&path.join(sub), 0o2770)?;
    }
    Ok(path)
}
fn atomic_policy(path: &Path, value: &Value) -> Api<()> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, value.to_string()).map_err(crate::runs::src_api::db)?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o640))
        .map_err(crate::runs::src_api::db)?;
    std::fs::rename(&temporary, path).map_err(crate::runs::src_api::db)
}
async fn ensure_gateway(s: &AppState, id: &str, task_root: &Path) -> Api<(String, Value, bool)> {
    let (name, _) = names(id);
    let content = browser_policy(s, id)?;
    let expected = policy_digest(&content);
    let gateway_root = gateway_root(task_root)?;
    let context=crate::src_resources::context(s,"src-egress").map_err(crate::runs::src_api::db)?;
    image(GATEWAY_IMAGE, Some(context)).await?;
    let mut existing = inspect(&name).await?;
    if let Some(current) = &existing {
        if current["Config"]["Labels"]["muteki.src.task_id"] != id
            || current["Config"]["Labels"]["muteki.src.gateway_schema"] != "rust1"
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "src.gateway_stale",
                "Task gateway identity changed",
            ));
        }
    }
    if existing
        .as_ref()
        .is_some_and(|current| latest_image_newer(GATEWAY_IMAGE, current))
    {
        super::gateway_replacement_ready(s,id)?;
        let (_, browser) = names(id);
        if let Some(current) = inspect(&browser).await? {
            if current["Config"]["Labels"]["muteki.src.browser_task"] != id {
                return Err(Error::conflict("Task browser identity changed"));
            }
            docker(&["rm".into(), "-f".into(), browser], 20).await?;
        }
        docker(&["rm".into(), "-f".into(), name.clone()], 20).await?;
        existing = None;
    }
    let recreated = existing.is_none();
    if let Some(current) = &existing {
        if current["State"]["Running"] == true {
            if let Ok(bytes) = std::fs::read(gateway_root.join("health.json")) {
                if let Ok(health) = serde_json::from_slice::<Value>(&bytes) {
                    let started = current["State"]["StartedAt"]
                        .as_str()
                        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                        .map(|v| v.timestamp() as f64)
                        .unwrap_or(f64::MAX);
                    let verified = health["verified_at"].as_f64().unwrap_or(0.0);
                    if verified >= started
                        && health["state"] == "ready"
                        && health["digest"] == expected
                        && health["source_digest"]==crate::src_resources::digest("src-egress")
                        && health["generation"] == content["environment_generation"]
                        && health["network_enforced"] == (content["network_mode"]=="enforced")
                        && health["inspection"]["state"] == "ready"
                        && (content["inspection_requirements"]["mode"]!="enforce"||health["inspection"]["enforce_ready"]==true)
                    {
                        return Ok((name, health, false));
                    }
                }
            }
        }
    }
    atomic_policy(&gateway_root.join("policy.json"), &content)?;
    let _ = std::fs::remove_file(gateway_root.join("health.json"));
    let _ = std::fs::remove_file(gateway_root.join("proxy-health.json"));
    if let Some(current) = existing {
        if current["State"]["Running"] == true {
            docker(
                &["kill".into(), "--signal".into(), "HUP".into(), name.clone()],
                10,
            )
            .await?;
        } else {
            docker(&["start".into(), name.clone()], 20).await?;
        }
    } else {
        let gid = unsafe { libc::getgid() };
        let args = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.clone(),
            "--cap-add".into(),
            "NET_ADMIN".into(),
            "--group-add".into(),
            gid.to_string(),
            "--env".into(),
            format!("MUTEKI_SRC_GID={gid}"),
            "--add-host".into(),
            "host.docker.internal:host-gateway".into(),
            "--label".into(),
            format!("muteki.src.task_id={id}"),
            "--label".into(),
            "muteki.src.gateway_schema=rust1".into(),
            "--label".into(),format!("muteki.src.source_digest={}",crate::src_resources::digest("src-egress")),
            "-v".into(),
            format!("{}:/run/src", gateway_root.display()),
            GATEWAY_IMAGE.into(),
        ];
        docker(&args, 30).await?;
    }
    for _ in 0..150 {
        if let Ok(bytes) = std::fs::read(gateway_root.join("health.json")) {
            if let Ok(health) = serde_json::from_slice::<Value>(&bytes) {
                if health["state"] == "failed" {
                    return Err(Error::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "capability.unavailable",
                        format!("Task gateway failed: {}", health["error"]),
                    ));
                }
                if health["state"] == "ready"
                    && health["digest"] == expected
                    && health["source_digest"]==crate::src_resources::digest("src-egress")
                        && health["generation"] == content["environment_generation"]
                    && health["network_enforced"] == (content["network_mode"]=="enforced")
                    && health["inspection"]["state"] == "ready"
                    && (content["inspection_requirements"]["mode"]!="enforce"||health["inspection"]["enforce_ready"]==true)
                {
                    return Ok((name, health, recreated));
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err(Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "capability.unavailable",
        "Task browser gateway health did not verify",
    ))
}
async fn ensure_browser_container(
    s: &AppState,
    id: &str,
    task_root: &Path,
    gateway: &str,
) -> Api<(String, bool)> {
    let (_, name) = names(id);
    let context=crate::src_resources::context(s,"src-browser").map_err(crate::runs::src_api::db)?;
    image(BROWSER_IMAGE, Some(context)).await?;
    let gateway_doc = inspect(gateway).await?.ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability.unavailable",
            "Task gateway container disappeared",
        )
    })?;
    let gateway_id = require(&gateway_doc, "Id")?;
    if let Some(current) = inspect(&name).await? {
        let network = current["HostConfig"]["NetworkMode"].as_str().unwrap_or("");
        if current["Config"]["Labels"]["muteki.src.browser_task"] != id {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "src.browser_stale",
                "Task browser container identity changed",
            ));
        }
        let network_changed = ![
            format!("container:{gateway}"),
            format!("container:{gateway_id}"),
        ]
        .iter()
        .any(|v| v == network);
        if latest_image_newer(BROWSER_IMAGE, &current) || network_changed {
            super::gateway_replacement_ready(s,id)?;
            docker(&["rm".into(), "-f".into(), name.clone()], 20).await?;
        } else {
            if current["State"]["Running"] != true {
                docker(&["start".into(), name.clone()], 20).await?;
            }
            return Ok((name, false));
        }
    }
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let args = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        name.clone(),
        "--user".into(),
        format!("{uid}:{gid}"),
        "--network".into(),
        format!("container:{gateway}"),
        "--label".into(),
        format!("muteki.src.browser_task={id}"),
        "--label".into(),format!("muteki.src.source_digest={}",crate::src_resources::digest("src-browser")),
        "--label".into(),
        format!("muteki.src.browser_gateway={gateway}"),
        "-v".into(),
        format!("{0}:{0}", task_root.display()),
        BROWSER_IMAGE.into(),
    ];
    docker(&args, 30).await?;
    Ok((name, true))
}
fn service_row(s: &AppState, id: &str) -> Api<Option<Value>> {
    Ok(records(s, id, "browser_service")?
        .into_iter()
        .find(|v| v["natural_key"] == "task"))
}
fn context_row(s: &AppState, id: &str, context: &str) -> Api<Option<Value>> {
    Ok(records(s, id, "browser_context")?
        .into_iter()
        .find(|v| v["natural_key"] == context))
}
pub(super) fn recover(s:&AppState,id:&str)->Api<Value>{
    let Some(row)=service_row(s,id)?else{return Ok(json!({"state":"not_created"}));};
    let epoch=s.chat.epoch();if row["payload"]["control_epoch"]==epoch{return Ok(row["payload"].clone());}
    let generation=row["payload"]["browser_generation"].as_u64().unwrap_or(row["generation"].as_u64().unwrap_or(0)).checked_add(1).ok_or_else(||Error::conflict("browser generation overflow"))?;
    let task_root=root(s,id)?;let mut service=row["payload"].clone();service["browser_generation"]=json!(generation);service["control_epoch"]=json!(epoch);service["recovered_at"]=json!(now());service["verification_required"]=json!(true);
    let mut updates=vec![("browser_service".to_owned(),"task".to_owned(),service.clone(),row["revision"].as_u64())];let mut contexts=Vec::new();
    for row in records(s,id,"browser_context")?{
        let context=require(&row["payload"],"context_id")?;let profile=task_root.join("profiles").join(token(&format!("{id}:{context}")));
        let readable=std::fs::read_dir(&profile).is_ok();let mut value=row["payload"].clone();
        value["browser_generation"]=json!(generation);value["recovery"]=json!(if readable{"session_recreated"}else{"profile_unavailable"});value["session_storage_restored"]=json!(false);value["verification_required"]=json!(true);value["updated_at"]=json!(now());
        if !readable{value["state"]=json!("profile_unavailable");}
        updates.push(("browser_context".into(),context.to_owned(),value.clone(),row["revision"].as_u64()));contexts.push(value);
    }
    let borrowed=updates.iter().map(|(kind,key,value,revision)|(kind.as_str(),key.as_str(),value.clone(),*revision)).collect::<Vec<_>>();
    if !super::atomic_records(s,id,&borrowed)?{return Err(Error::conflict("browser changed during control-plane recovery"));}
    Ok(json!({"browser_generation":generation,"recovery":"session_recreated","session_storage_restored":false,"contexts":contexts,"service":service}))
}
pub(super) async fn ensure(s: Arc<AppState>, id: &str) -> Api<Value> {
    let _activation=crate::src_resources::activation(&s,id).await.map_err(|message|Error::conflict(message))?;
    let task_root = root(&s, id)?;
    let (gateway, health, gateway_recreated) = ensure_gateway(&s, id, &task_root).await?;
    let (browser, browser_recreated) = ensure_browser_container(&s, id, &task_root, &gateway).await?;
    let recreated = gateway_recreated || browser_recreated;
    let prior = service_row(&s, id)?;
    let generation = prior
        .as_ref()
        .and_then(|v| v["payload"]["browser_generation"].as_u64())
        .unwrap_or(0)
        + u64::from(
            prior
                .as_ref()
                .is_none_or(|v| v["payload"]["state"] != "ready")
                || recreated,
        );
    let generation = generation.max(1);
    let p = policy(&s, id)?;
    let value = json!({"owner":"task","state":"ready","driver":"agent-browser-docker",
        "gateway_container":gateway,"browser_container":browser,"gateway_policy_digest":health["digest"],
        "src_policy_digest":p["digest"],"src_policy_version":p["version"],
        "browser_generation":generation,"control_epoch":s.chat.epoch(),"verification_required":true,"network_enforced_for_this_browser":health["network_enforced"],
        "inspection_enforced_for_this_browser":health["inspection"]["enforce_ready"],"global_network_enforced":false,
        "supported_actions":["current_task_policy"],"coverage_limitations":["worker_egress_not_covered","websocket_upgrade_not_supported"],
        "updated_at":now()});
    if let Some(row) = prior {
        if !recreated
            && row["payload"]["src_policy_digest"] == p["digest"]
            && row["payload"]["state"] == "ready"
        {
            return Ok(row["payload"].clone());
        }
        save_rec(
            &s,
            id,
            "browser_service",
            "task",
            value.clone(),
            row["revision"].as_u64(),
        )?;
    } else {
        save_rec(&s, id, "browser_service", "task", value.clone(), Some(0))?;
    }
    Ok(value)
}
async fn command(s: &AppState, id: &str, context: &str, args: &[&str]) -> Api<Value> {
    let task_root = root(s, id)?;
    let (_, container) = names(id);
    let context_token = token(&format!("{id}:{context}"));
    let home = task_root.join("home").join(&context_token);
    let profile = task_root.join("profiles").join(&context_token);
    directory(&home, 0o700)?;
    directory(&profile, 0o700)?;
    let session = format!("s-{}-{}", &token(id)[..12], &context_token[..8]);
    let env = [
        ("HOME", home.to_string_lossy().into_owned()),
        (
            "XDG_CONFIG_HOME",
            home.join(".config").to_string_lossy().into_owned(),
        ),
        (
            "XDG_CACHE_HOME",
            home.join(".cache").to_string_lossy().into_owned(),
        ),
        (
            "XDG_DATA_HOME",
            home.join(".local/share").to_string_lossy().into_owned(),
        ),
        ("AGENT_BROWSER_SESSION", session.clone()),
        ("AGENT_BROWSER_NAMESPACE", session),
        (
            "AGENT_BROWSER_PROFILE",
            profile.to_string_lossy().into_owned(),
        ),
        (
            "AGENT_BROWSER_SOCKET_DIR",
            format!("/tmp/sb-{}", &context_token[..12]),
        ),
        ("AGENT_BROWSER_EXECUTABLE_PATH", "/usr/bin/chromium".into()),
    ];
    let mut argv = vec!["exec".to_owned()];
    for (key, value) in env {
        argv.push("-e".into());
        argv.push(format!("{key}={value}"));
    }
    argv.push(container);
    argv.push("agent-browser".into());
    argv.extend(args.iter().map(|v| (*v).to_owned()));
    argv.push("--json".into());
    let output = docker(&argv, 60).await?;
    let value: Value = serde_json::from_str(&output).map_err(|_| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability.unavailable",
            "Task browser returned invalid JSON",
        )
    })?;
    if value["success"] != true {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability.unavailable",
            format!("Task browser command failed: {}", value["error"]),
        ));
    }
    Ok(value["data"].clone())
}
pub(super) async fn ensure_context(s: Arc<AppState>, id: &str, context: &str) -> Api<Value> {
    if context.is_empty(){return Err(Error::invalid("Task browser context_id is required"));}
    let service = ensure(s.clone(), id).await?;
    let prior = context_row(&s, id, context)?;
    if prior.as_ref().is_some_and(|v| {
        v["payload"]["state"] == "ready"
            && v["payload"]["gateway_policy_digest"] == service["gateway_policy_digest"]
            && v["payload"]["browser_generation"] == service["browser_generation"]
    }) {
        return Ok(prior.unwrap()["payload"].clone());
    }
    let tabs = command(&s, id, context, &["tab", "list"]).await?;
    let value = json!({"context_id":context,"state":"ready","browser_generation":service["browser_generation"],
        "gateway_policy_digest":service["gateway_policy_digest"],"driver":{"kind":"agent-browser-docker",
        "container":service["browser_container"],"tab_count":tabs["tabs"].as_array().map(Vec::len).unwrap_or(0)},
        "recovery":if prior.is_some(){"session_recreated"}else{"new"},"verification_required":true,
        "session_storage_restored":false,"last_verified_page":null,"updated_at":now()});
    save_rec(
        &s,
        id,
        "browser_context",
        context,
        value.clone(),
        Some(
            prior
                .as_ref()
                .and_then(|v| v["revision"].as_u64())
                .unwrap_or(0),
        ),
    )?;
    Ok(value)
}
pub(super) fn target_scope(s: &AppState, id: &str, url: &str) -> Api<()> {
    crate::runs::src_api::parse_url(url)?;
    let t = task(s, id)?;
    let p = policy(s, id)?;
    if !in_policy(&p, url)
        || !t["input"]["authorization"]["targets"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|v| {
                    v.as_str()
                        .is_some_and(|allowed| crate::runs::src_api::target_matches(url, allowed))
                })
            })
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "browser URL is outside current Task scope",
        ));
    }
    Ok(())
}
fn gateway_events(root: &Path, since: SystemTime) -> Api<Vec<Value>> {
    let mut rows = Vec::new();
    let path = root.join("gateway").join("traffic");
    for entry in std::fs::read_dir(path).map_err(crate::runs::src_api::db)? {
        let entry = entry.map_err(crate::runs::src_api::db)?;
        if entry.path().extension().is_none_or(|v| v != "json") {
            continue;
        }
        if entry
            .metadata()
            .and_then(|v| v.modified())
            .is_ok_and(|v| v < since)
        {
            continue;
        }
        if let Ok(bytes) = std::fs::read(entry.path()) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                rows.push(value);
            }
        }
    }
    Ok(rows)
}
fn record_gateway_events(
    s: &AppState,
    id: &str,
    task_root: &Path,
    context: &str,
    generation: u64,
    digest: &str,
    events: &[Value],
) -> Api<Vec<String>> {
    let mut accepted = Vec::new();
    for event in events {
        let gateway_id = event["exchange_id"].as_str().unwrap_or("");
        if gateway_id.len() != 32 || !gateway_id.bytes().all(|v| v.is_ascii_hexdigit()) {
            continue;
        }
        let key = format!("gateway:{gateway_id}");
        if event["status_code"].as_u64().is_none() || event["capture_state"] != "complete" {
            if records(s, id, "browser_gateway_attempt")?
                .iter()
                .all(|v| v["natural_key"] != key)
            {
                let value = json!({"gateway_exchange_id":gateway_id,"context_id":context,
                    "observed_url":event["observed_url"],"method":event["method"],
                    "errors":event["errors"],"capture_state":event["capture_state"],
                    "gateway_policy_digest":digest,"recorded_at":now()});
                save_rec(s, id, "browser_gateway_attempt", &key, value, Some(0))?;
            }
            continue;
        }
        if let Some(prior) = records(s, id, "http_exchange")?
            .into_iter()
            .find(|v| v["natural_key"] == key)
        {
            if let Some(exchange_id) = prior["payload"]["exchange_id"].as_str() {
                accepted.push(exchange_id.to_owned());
            }
            continue;
        }
        let request_name = format!("traffic/{gateway_id}.request");
        let response_name = format!("traffic/{gateway_id}.response");
        if event["request_raw_ref"] != request_name || event["response_raw_ref"] != response_name {
            continue;
        }
        let root = task_root.join("gateway");
        let request = std::fs::read(root.join(&request_name)).map_err(crate::runs::src_api::db)?;
        let response =
            std::fs::read(root.join(&response_name)).map_err(crate::runs::src_api::db)?;
        let request_ref = put_artifact(
            s,
            id,
            &request,
            &format!("{gateway_id}-request.http"),
            "message/http",
        )?;
        let response_ref = put_artifact(
            s,
            id,
            &response,
            &format!("{gateway_id}-response.http"),
            "message/http",
        )?;
        let exchange_id = format!("sex_browser_{gateway_id}");
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
        let value = json!({"exchange_id":exchange_id,"task_id":id,"source_run_id":null,"source_kind":"browser",
            "actor_ref":"task-browser-gateway","context_id":context,"identity_revision":0,
            "browser_generation":generation,"declared_target":event["declared_target"],
            "observed_url":event["observed_url"],"method":event["method"],"started_at":now(),"finished_at":now(),
            "request_raw_ref":request_ref,"response_raw_ref":response_ref,
            "request_headers":redact(&event["request_headers"]),"response_headers":redact(&event["response_headers"]),
            "status_code":event["status_code"],"protocol":event["protocol"],
            "redirect_parent":null,"replay_of":null,"execution_state":"completed","capture_state":"complete",
            "capture_limitations":[],"authorization_ref":digest,"decision_ref":null,
            "metadata":{"gateway_exchange_id":gateway_id,"decision":event["decision"],"capture_source":"task_browser_gateway"}});
        save_rec(s, id, "http_exchange", &key, value.clone(), Some(0))?;
        super::traffic_index::index_exchange(s, id, &value, &request, Some(&response))?;
        super::advance_exchange_source(s, id, &key)?;
        accepted.push(exchange_id);
    }
    Ok(accepted)
}
fn trusted_actor(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let binding = task(s, id)?["binding"].clone();
    let mut actor = body["actor"].as_object().cloned().unwrap_or_default();
    let run_id = actor
        .get("run_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    actor.insert(
        "id".into(),
        json!(binding["principal_id"].as_str().unwrap_or("local-user")),
    );
    actor.insert("kind".into(), json!("operator"));
    actor.insert(
        "thread_id".into(),
        json!(if run_id.is_empty() {
            binding["thread_id"].as_str().unwrap_or("")
        } else {
            ""
        }),
    );
    actor.insert("run_id".into(), json!(run_id));
    Ok(Value::Object(actor))
}
pub(super) async fn open_tab(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    let context = body["context_id"].as_str().unwrap_or("default");
    let actor = trusted_actor(&s, id, body)?;
    let owner = objects::participant(&s, id, context, &actor)?;
    let current = ensure_context(s.clone(), id, context).await?;
    if actor["browser_generation"]
        .as_u64()
        .is_some_and(|v| current["browser_generation"] != v)
    {
        return Err(Error::conflict("browser generation changed"));
    }
    let target = body["url"].as_str().filter(|v| !v.is_empty());
    let mut request_refs = Vec::new();
    if let Some(url) = target {
        target_scope(&s, id, url)?;
    }
    let shared = body["shared"] == true;
    let label = if shared {
        "muteki-shared".to_owned()
    } else {
        format!("muteki-{}", &token(&owner)[..20])
    };
    let lease = if shared {
        objects::acquire(
            &s,
            id,
            &json!({"context_id":context,"resource_kind":"src.browser_tab",
            "resource_id":label,"actor":actor,"ttl_seconds":body["ttl_seconds"]}),
        )?
    } else {
        Value::Null
    };
    let opened: Api<Value> = async {
        let tabs = command(&s, id, context, &["tab", "list"]).await?;
        let exists = tabs["tabs"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|tab| tab["label"] == label));
        let mut actual = if exists {
            command(&s, id, context, &["tab", &label]).await?
        } else {
            command(&s, id, context, &["tab", "new", "--label", &label]).await?
        };
        if let Some(url) = target {
            let started = SystemTime::now();
            actual = command(&s, id, context, &["open", url]).await?;
            target_scope(&s, id, actual["url"].as_str().unwrap_or(""))?;
            let events = gateway_events(&root(&s, id)?, started)?;
            let allowed = events.iter().any(|v| {
                v["observed_url"].as_str().is_some_and(|seen| {
                    crate::runs::src_api::target_matches(seen, url)
                        && crate::runs::src_api::target_matches(url, seen)
                }) && v["status_code"].as_u64().is_some()
                    && v["errors"].as_array().is_none_or(Vec::is_empty)
            });
            if !allowed {
                return Err(Error::new(
                    StatusCode::BAD_GATEWAY,
                    "src.browser_capture_unavailable",
                    "Task gateway did not confirm the browser navigation",
                ));
            }
            request_refs = record_gateway_events(
                &s,
                id,
                &root(&s, id)?,
                context,
                current["browser_generation"].as_u64().unwrap_or(0),
                current["gateway_policy_digest"].as_str().unwrap_or(""),
                &events,
            )?;
        }
        Ok(actual)
    }
    .await;
    let actual = match opened {
        Ok(actual) => actual,
        Err(error) => {
            if shared {
                let _ = objects::release(
                    &s,
                    id,
                    &json!({"context_id":context,"resource_kind":"src.browser_tab",
                    "resource_id":label,"actor":actor,"fencing_token":lease["fencing_token"]}),
                );
            }
            return Err(error);
        }
    };
    let prior = records(&s, id, "browser_tab")?
        .into_iter()
        .find(|row| row["payload"]["context_id"] == context && row["payload"]["label"] == label);
    let tab_id = actual["tabId"]
        .as_str()
        .or_else(|| actual["targetId"].as_str())
        .or_else(|| {
            prior
                .as_ref()
                .and_then(|row| row["payload"]["driver_tab_id"].as_str())
        })
        .unwrap_or("");
    let id_value = prior
        .as_ref()
        .and_then(|row| row["payload"]["tab_id"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("sbrowsertab-{}", Uuid::new_v4().simple()));
    let natural_key = prior
        .as_ref()
        .and_then(|row| row["natural_key"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{context}:{label}"));
    let created_at = prior
        .as_ref()
        .and_then(|row| row["payload"]["created_at"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(now);
    let previous_refs = prior
        .as_ref()
        .map(|row| row["payload"]["request_refs"].clone());
    let previous_url = prior.as_ref().map(|row| row["payload"]["url"].clone());
    let previous_capture = prior
        .as_ref()
        .map(|row| row["payload"]["capture_state"].clone());
    let value = json!({"tab_id":id_value,"driver_tab_id":tab_id,"label":label,"context_id":context,
        "shared":shared,"owner":owner,
        "browser_generation":current["browser_generation"],
        "url":if actual["url"].is_null(){previous_url.unwrap_or(Value::Null)}else{actual["url"].clone()},
        "state":"open","lease":lease,
        "request_refs":if target.is_some(){json!(request_refs)}else{previous_refs.unwrap_or_else(||json!([]))},
        "gateway_policy_digest":current["gateway_policy_digest"],
        "created_at":created_at,
        "capture_state":if target.is_some(){json!("gateway_verified")}else{previous_capture.unwrap_or_else(||json!("blank"))}});
    save_rec(
        &s,
        id,
        "browser_tab",
        &natural_key,
        value.clone(),
        Some(
            prior
                .as_ref()
                .and_then(|row| row["revision"].as_u64())
                .unwrap_or(0),
        ),
    )?;
    Ok(value)
}
fn owned_tab(s: &AppState, id: &str, body: &Value, actor: &Value) -> Api<Value> {
    let tab_id = require(body, "tab_id")?;
    let row = rec(s, id, "browser_tab", tab_id)?;
    let tab = &row["payload"];
    if tab["state"] != "open" {
        return Err(Error::conflict("browser tab is not open"));
    }
    let context = require(tab, "context_id")?;
    if body["context_id"].as_str().is_some_and(|v| v != context) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.task_binding_invalid",
            "browser context differs from tab",
        ));
    }
    if actor["browser_generation"]
        .as_u64()
        .is_some_and(|v| tab["browser_generation"] != v)
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "authorization_stale",
            "browser generation is stale",
        ));
    }
    let owner = objects::participant(s, id, context, actor)?;
    let shared = tab["shared"] == true || (tab["shared"].is_null() && tab["lease"].is_object());
    if shared {
        if tab["lease"]["owner"] != owner {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.task_binding_invalid",
                "browser tab belongs to another participant",
            ));
        }
        let token = body["fencing_token"]
            .as_u64()
            .ok_or_else(|| Error::invalid("shared browser tab fencing_token required"))?;
        objects::assert_lease(
            s,
            id,
            context,
            "src.browser_tab",
            require(tab, "label")?,
            &owner,
            token,
        )?;
    } else if tab["owner"] != owner {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.task_binding_invalid",
            "browser tab belongs to another participant",
        ));
    }
    Ok(row)
}
pub(super) fn release_tab(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let actor = trusted_actor(s, id, body)?;
    let row = owned_tab(s, id, body, &actor)?;
    let mut tab = row["payload"].clone();
    if tab["shared"] != true && !(tab["shared"].is_null() && tab["lease"].is_object()) {
        return Err(Error::invalid(
            "private browser tab has no shared lease to release",
        ));
    }
    let context = require(&tab, "context_id")?;
    let label = require(&tab, "label")?;
    let lease = objects::release(
        s,
        id,
        &json!({"context_id":context,"resource_kind":"src.browser_tab",
        "resource_id":label,"actor":actor,"fencing_token":body["fencing_token"]}),
    )?;
    tab["lease"] = lease;
    tab["released_at"] = json!(now());
    save_rec(
        s,
        id,
        "browser_tab",
        row["natural_key"]
            .as_str()
            .unwrap_or(require(&tab, "tab_id")?),
        tab.clone(),
        row["revision"].as_u64(),
    )?;
    Ok(tab)
}
pub(super) async fn screenshot(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    let actor = trusted_actor(&s, id, body)?;
    let row = owned_tab(&s, id, body, &actor)?;
    let tab = &row["payload"];
    let context = require(tab, "context_id")?;
    let current = ensure_context(s.clone(), id, context).await?;
    if tab["browser_generation"] != current["browser_generation"]
        || tab["gateway_policy_digest"] != current["gateway_policy_digest"]
    {
        return Err(Error::conflict("browser tab gateway or generation changed"));
    }
    let label = require(tab, "label")?;
    command(&s, id, context, &["tab", label]).await?;
    let page = command(&s, id, context, &["get", "url"]).await?;
    let url = require(&page, "url")?;
    target_scope(&s, id, url)?;
    let task_root = root(&s, id)?;
    let capture_root = task_root.join("captures");
    directory(&capture_root, 0o700)?;
    let file = capture_root.join(format!("{}.png", Uuid::new_v4().simple()));
    let path = file.to_string_lossy().into_owned();
    command(&s, id, context, &["screenshot", &path]).await?;
    let bytes = std::fs::read(&file).map_err(crate::runs::src_api::db)?;
    let _ = std::fs::remove_file(&file);
    if crate::image_validation::mime(&bytes).map_err(|message|Error::new(StatusCode::UNPROCESSABLE_ENTITY,"src.browser_image_invalid",message))?!="image/png"{return Err(Error::invalid("browser screenshot must be PNG"));}
    let (network_ids, capture_errors) =
        match command(&s, id, context, &["network", "requests"]).await {
            Ok(network) => {
                let mut ids = Vec::new();
                for request in network["requests"].as_array().into_iter().flatten() {
                    if let Some(request_id) = request["requestId"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                    {
                        if !ids.iter().any(|seen| seen == request_id) {
                            ids.push(request_id.to_owned());
                        }
                    }
                }
                (ids, Vec::new())
            }
            Err(_) => (Vec::new(), vec!["network_requests_unavailable"]),
        };
    let request_refs = body["request_refs"]
        .as_array()
        .filter(|refs| !refs.is_empty())
        .cloned()
        .unwrap_or_else(|| network_ids.iter().map(|value| json!(value)).collect());
    let mut limitations = vec![
        "subresources_outside_scope_may_be_blocked",
        "worker_egress_not_covered",
    ];
    limitations.extend(capture_errors.iter().copied());
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    let mut artifact = put_artifact(&s, id, &bytes, "browser-screenshot.png", "image/png")?;
    artifact["complete"]=json!("partial");artifact["source_run_id"]=actor["run_id"].clone();
    let key=format!("{id}:{}",require(&artifact,"artifact_id")?);let mut stored=s.store.get("src_artifacts",&key).map_err(crate::runs::src_api::db)?.ok_or_else(||Error::conflict("screenshot artifact disappeared"))?;stored["complete"]=artifact["complete"].clone();stored["source_run_id"]=actor["run_id"].clone();s.store.put("src_artifacts",&key,&stored).map_err(crate::runs::src_api::db)?;
    let screenshot_id = format!("sscreenshot-{}", Uuid::new_v4().simple());
    let value = json!({"screenshot_id":screenshot_id,"artifact":artifact,
        "width":width,"height":height,"url":url,"captured_at":now(),"context_id":context,
        "browser_generation":current["browser_generation"],"tab_id":tab["tab_id"],
        "actor":actor,"identity_revision":actor["identity_revision"].as_u64().unwrap_or(0),
        "action_ref":body["action_ref"],"request_refs":request_refs,
        "capture_errors":capture_errors,
        "inspection":{"mode":policy(&s,id)?["inspection_requirements"]["mode"],"coverage":"task_gateway_netns_proxy","capture_state":"partial",
            "uninspected":true,"reasons":limitations,
            "policy_digest":current["gateway_policy_digest"]},"review_status":"unreviewed"});
    let screenshot_record = save_rec(
        &s,
        id,
        "browser_screenshot",
        &screenshot_id,
        value.clone(),
        Some(0),
    )?;
    let event_id = format!("sbrowserpage-{}", Uuid::new_v4().simple());
    let observed_at = now();
    let page_event = save_rec(
        &s,
        id,
        "browser_page_event",
        &event_id,
        json!({"context_id":context,"actor":actor,"url":url,
        "action":body["action_ref"].as_str().filter(|action| !action.is_empty()).unwrap_or("screenshot"),
        "frame_origin":"","dom_artifact":null,"console_events":[],"network_events":network_ids,
        "session_state":"observed","browser_generation":current["browser_generation"],
        "evidence_refs":[screenshot_record],"observed_at":observed_at}),
        Some(0),
    )?;
    let context_record = context_row(&s, id, context)?
        .ok_or_else(|| Error::conflict("Task browser context changed"))?;
    let mut context_value = context_record["payload"].clone();
    context_value["last_verified_page"] = json!({"url":url,"event_id":page_event["record_id"],
        "observed_at":observed_at,"session_state":"observed"});
    context_value["verification_required"] = json!(false);
    save_rec(
        &s,
        id,
        "browser_context",
        context,
        context_value,
        context_record["revision"].as_u64(),
    )?;
    Ok(value)
}
pub(super) fn image_read(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let screenshot_id = body["screenshot_id"]
        .as_str()
        .or_else(|| body["read_id"].as_str())
        .ok_or_else(|| Error::invalid("screenshot_id required"))?;
    let reviewer = require(body, "reviewer")?;
    let screenshot = rec(s, id, "browser_screenshot", screenshot_id)?["payload"].clone();
    let bytes = verify_ref(s, id, &screenshot["artifact"])?;
    let delivered = body["confirm_delivery"].as_bool().unwrap_or(true);
    let read_id = format!("simageread-{}", Uuid::new_v4().simple());
    let read = save_rec(
        s,
        id,
        "image_review_read",
        &read_id,
        json!({"screenshot_id":screenshot_id,"artifact_id":bytes["artifact_id"],
            "reviewer":reviewer,"status":if delivered{"delivered"}else{"prepared"},
            "media_type":screenshot["artifact"]["media_type"],"size":bytes["size"],
            "delivered_at":if delivered{json!(now())}else{Value::Null}}),
        Some(0),
    )?;
    Ok(json!({"read_receipt":read,"image_bytes":bytes,
        "media_type":screenshot["artifact"]["media_type"],
        "artifact_id":bytes["artifact_id"],"width":screenshot["width"],"height":screenshot["height"]}))
}
pub(super) fn review_image(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let screenshot_id = body["screenshot_id"]
        .as_str()
        .or_else(|| body["read_id"].as_str())
        .ok_or_else(|| Error::invalid("screenshot_id required"))?;
    let row = rec(s, id, "browser_screenshot", screenshot_id)?;
    let mut screenshot = row["payload"].clone();
    let bytes = verify_ref(s, id, &screenshot["artifact"])?;
    let reviewer = require(body, "reviewer")?;
    let read_id = body["review_read_id"].as_str().unwrap_or("");
    let matching_read = records(s, id, "image_review_read")?
        .into_iter()
        .find(|read| read["record_id"] == read_id || read["natural_key"] == read_id);
    let valid_read = matching_read.is_some_and(|read| {
        let payload = &read["payload"];
        payload["reviewer"] == reviewer
            && payload["screenshot_id"] == screenshot_id
            && payload["artifact_id"] == bytes["artifact_id"]
            && payload["status"] == "delivered"
    });
    let judgment = body["judgment"].clone();
    let has_judgment = !judgment.is_null()
        && !judgment.as_str().is_some_and(str::is_empty)
        && !judgment.as_array().is_some_and(Vec::is_empty)
        && !judgment.as_object().is_some_and(serde_json::Map::is_empty);
    let reviewed = valid_read && has_judgment;
    let review_id = format!("simagereview-{}", Uuid::new_v4().simple());
    let review = save_rec(
        s,
        id,
        "image_review",
        &review_id,
        json!({"screenshot_id":screenshot_id,"artifact_id":bytes["artifact_id"],
            "reviewer":reviewer,"status":if reviewed{"reviewed"}else{"unavailable"},
            "judgment":judgment,"review_read_id":if read_id.is_empty(){Value::Null}else{json!(read_id)},
            "error":if reviewed{""}else{"reviewer must read the original image and provide a judgment"},
            "reviewed_at":now()}),
        Some(0),
    )?;
    screenshot["review_status"] = json!(if reviewed { "reviewed" } else { "unreviewed" });
    screenshot["last_image_review"] = review.clone();
    save_rec(
        s,
        id,
        "browser_screenshot",
        screenshot_id,
        screenshot,
        row["revision"].as_u64(),
    )?;
    Ok(review)
}
pub(super) fn service(s: &AppState, id: &str) -> Api<Value> {
    service_row(s, id)?
        .map(|v| v["payload"].clone())
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.browser_unavailable",
                "Task browser has not been ensured",
            )
        })
}
pub(super) fn verified_service(s: &AppState, id: &str) -> Api<Option<Value>> {
    let Some(row) = service_row(s, id)? else {
        return Ok(None);
    };
    let mut value = row["payload"].clone();
    if value["state"] != "ready" {
        return Ok(Some(value));
    }
    let current_policy = policy(s, id)?;
    let gateway_name = value["gateway_container"].as_str().unwrap_or("");
    let browser_name = value["browser_container"].as_str().unwrap_or("");
    let read_inspect = |name: &str| -> Option<Value> {
        if let Some(result)=crate::runner::inspect_src_sync(name){return result.ok().flatten();}
        let output = crate::host_process::std_command("docker")
            .args(["inspect", name])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        serde_json::from_slice::<Value>(&output.stdout)
            .ok()?
            .as_array()?
            .first()
            .cloned()
    };
    let live = (|| -> Option<bool> {
        let gateway = read_inspect(gateway_name)?;
        let browser = read_inspect(browser_name)?;
        let health: Value = serde_json::from_slice(
            &std::fs::read(root(s, id).ok()?.join("gateway/health.json")).ok()?,
        )
        .ok()?;
        let started = gateway["State"]["StartedAt"]
            .as_str()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|v| v.timestamp() as f64)?;
        Some(
            gateway["State"]["Running"] == true
                && browser["State"]["Running"] == true
                && !latest_image_newer(GATEWAY_IMAGE, &gateway)
                && !latest_image_newer(BROWSER_IMAGE, &browser)
                && gateway["Config"]["Labels"]["muteki.src.task_id"] == id
                && gateway["Config"]["Labels"]["muteki.src.gateway_schema"] == "rust1"
                && browser["Config"]["Labels"]["muteki.src.browser_task"] == id
                && value["src_policy_digest"] == current_policy["digest"]
                && value["gateway_policy_digest"] == health["digest"]
                && health["verified_at"]
                    .as_f64()
                    .is_some_and(|v| v + 0.5 >= started)
                && health["state"] == "ready"
                && health["source_digest"]==crate::src_resources::digest("src-egress")
                && health["generation"]==current_policy["version"]
                && health["network_enforced"]==(current_policy["network_mode"]=="enforced")
                && health["inspection"]["state"] == "ready"
                && (current_policy["inspection_requirements"]["mode"]!="enforce"||health["inspection"]["enforce_ready"]==true),
        )
    })()
    .unwrap_or(false);
    if !live {
        value["state"] = json!("stale");
    }
    Ok(Some(value))
}
pub(super) fn context(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let context = require(body, "context_id")?;
    context_row(s, id, context)?
        .map(|v| v["payload"].clone())
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.browser_context_unavailable",
                "Task browser context unavailable",
            )
        })
}
pub(super) fn generation(s:&AppState,id:&str)->Api<Value>{Ok(service(s,id)?["browser_generation"].clone())}
pub(super) fn execution_record(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let context_id = body["context_id"]
        .as_str()
        .ok_or_else(|| Error::invalid("context_id is required"))?;
    let kind = body["kind"]
        .as_str()
        .ok_or_else(|| Error::invalid("kind is required"))?;
    if !matches!(
        kind,
        "reflection" | "console" | "injected_evaluate" | "page_execution" | "oast_callback"
    ) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "policy_denied",
            "unknown browser execution signal kind",
        ));
    }
    let frame_origin = body["frame_origin"]
        .as_str()
        .ok_or_else(|| Error::invalid("frame_origin is required"))?;
    let trigger = body["trigger"]
        .as_object()
        .ok_or_else(|| Error::invalid("trigger is required"))?;
    let evidence_refs = body["evidence_refs"]
        .as_array()
        .ok_or_else(|| Error::invalid("evidence_refs is required"))?;
    let verified_evidence = !evidence_refs.is_empty()
        && evidence_refs.iter().all(|reference| {
            reference.as_str().is_some_and(|value| !value.is_empty())
                || reference.as_object().is_some_and(|value| !value.is_empty())
        });
    let injected = body["injected"].as_bool().unwrap_or(false);
    let normal_user_path = body["normal_user_path"].as_bool().unwrap_or(false);
    let target_executed =
        kind == "page_execution" && normal_user_path && !injected && verified_evidence;
    let record_id = format!("sbrowserexecution-{}", Uuid::new_v4().simple());
    save_rec(
        s,
        id,
        "browser_execution",
        &record_id,
        json!({"context_id":context_id,"kind":kind,"frame_origin":frame_origin,
            "trigger":trigger,"evidence_refs":evidence_refs,"injected":injected,
            "normal_user_path":normal_user_path,"csp":body["csp"].as_str().unwrap_or(""),
            "sandbox":body["sandbox"].as_str().unwrap_or(""),
            "target_executed":target_executed,
            "conclusion":if target_executed{"target_page_executed"}else{"not_proof_of_target_execution"}}),
        Some(0),
    )
}
pub(super) fn run_finished(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let run_id = require(body, "run_id")?;
    if !records(s, id, "run_binding")?
        .iter()
        .any(|v| v["payload"]["run_id"] == run_id)
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.run_not_bound",
            "browser Run is outside this Task",
        ));
    }
    let event_id = format!("sbrowser_run_{}", Uuid::new_v4().simple());
    let value = json!({"event_id":event_id,"run_id":run_id,"task_id":id,"state":"finished",
        "browser_service_owner":"task","at":now()});
    save_rec(
        s,
        id,
        "browser_run_event",
        &event_id,
        value.clone(),
        Some(0),
    )?;
    let context=require(body,"context_id")?;let context=context_row(s,id,context)?.ok_or_else(||Error::conflict("browser context is unavailable"))?;
    Ok(json!({"task_browser_closed":false,"run_id":run_id,"browser_generation":context["payload"]["browser_generation"]}))
}
pub(super) async fn close_task(s: Arc<AppState>, id: &str) -> Api<Value> {
    let (gateway, browser) = names(id);
    if inspect(&browser)
        .await?
        .is_some_and(|v| v["State"]["Running"] == true)
    {
        docker(&["stop".into(), browser], 30).await?;
    }
    if inspect(&gateway)
        .await?
        .is_some_and(|v| v["State"]["Running"] == true)
    {
        docker(&["stop".into(), gateway], 30).await?;
    }
    let row = service_row(&s, id)?.ok_or_else(|| {
        Error::new(
            StatusCode::NOT_FOUND,
            "src.browser_unavailable",
            "Task browser has not been ensured",
        )
    })?;
    let mut value = row["payload"].clone();
    value["state"] = json!("closed");
    value["closed_at"] = json!(now());
    value["browser_generation"] = json!(value["browser_generation"].as_u64().unwrap_or(0) + 1);
    save_rec(
        &s,
        id,
        "browser_service",
        "task",
        value.clone(),
        row["revision"].as_u64(),
    )?;
    Ok(value)
}
pub(super) async fn quiesce(s: Arc<AppState>, id: &str) -> Api<()> {
    let row = service_row(&s, id)?;
    let (gateway, browser) = names(id);
    for name in [browser, gateway] {
        if inspect(&name)
            .await?
            .is_some_and(|v| v["State"]["Running"] == true)
        {
            docker(&["stop".into(), name], 30).await?;
        }
    }
    if let Some(row) = row {
        let mut value = row["payload"].clone();
        value["state"] = json!("paused_policy_change");
        value["browser_generation"] = json!(value["browser_generation"].as_u64().unwrap_or(0) + 1);
        value["updated_at"] = json!(now());
        save_rec(
            &s,
            id,
            "browser_service",
            "task",
            value,
            row["revision"].as_u64(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn browser_policy_uses_narrower_task_policy_intersection() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = crate::runs::src_api::create_task_inner(
            &state,
            &json!({"title":"Browser scope",
            "goal":"Read a target","authorization":{"targets":["https://example.com/app"]},
            "idempotency_key":"browser-policy-intersection"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        crate::runs::src_api::update_policy(
            &state,
            id,
            &json!({"base_version":1,
            "authorized_targets":["https://example.com/app/sub"],
            "excluded_targets":["https://example.com/app/sub/private"]}),
        )
        .unwrap();
        let p = browser_policy(&state, id).unwrap();
        assert_eq!(
            p["authorized_targets"],
            json!(["https://example.com/app/sub"])
        );
        assert_eq!(
            p["excluded_targets"],
            json!(["https://example.com/app/sub/private"])
        );
        assert_eq!(p["defaults"]["effect"], "allow");
        assert_eq!(p["inspection_requirements"]["mode"], "off");
        assert!(p["rules"].as_array().unwrap().is_empty());
        assert_eq!(canonical_json(&json!({"b":2,"a":1})), r#"{"a":1,"b":2}"#);
    }
}
