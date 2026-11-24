//! Task policy gateway for SRC Worker runs.
use super::{Api, Error, browser_runtime, policy, task};
use crate::AppState;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::sleep;

const IMAGE: &str = "muteki-src-egress:latest";

fn unavailable(message: impl Into<String>) -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "capability.unavailable",
        message,
    )
}
fn name(id: &str) -> String {
    let hash = format!("{:x}", Sha256::digest(format!("{id}:worker").as_bytes()));
    format!("muteki-src-egress-{}", &hash[..24])
}
pub(super) fn gateway_root(s: &AppState, id: &str) -> Api<PathBuf> {
    let state = s
        .store
        .get("system", "state_dir")
        .map_err(crate::runs::src_api::db)?
        .and_then(|v| v.as_str().map(PathBuf::from))
        .ok_or_else(|| unavailable("managed state_dir unavailable"))?;
    let root = state.join("src-worker-egress").join(name(id));
    std::fs::create_dir_all(&root).map_err(crate::runs::src_api::db)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o2770))
        .map_err(crate::runs::src_api::db)?;
    for sub in [
        "traffic",
        "inspection_requests",
        "inspection_decisions",
        "inspection_grants",
    ] {
        let path = root.join(sub);
        std::fs::create_dir_all(&path).map_err(crate::runs::src_api::db)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o2770))
            .map_err(crate::runs::src_api::db)?;
    }
    Ok(root)
}
pub(super) fn policy_data(s: &AppState, id: &str, current: &Value) -> Api<Value> {
    let task = task(s, id)?;
    if task["lifecycle"]["state"] != "active" {
        return Err(Error::conflict("SRC Task is not active"));
    }
    let network = current["network_mode"].as_str().unwrap_or("declared");
    let inspection = current["inspection_requirements"]["mode"]
        .as_str()
        .unwrap_or("off");
    if !matches!(network, "declared" | "enforced")
        || !matches!(inspection, "off" | "record" | "enforce")
    {
        return Err(Error::invalid("invalid SRC network or inspection mode"));
    }
    if network == "declared" && inspection == "enforce" {
        return Err(unavailable(
            "content enforce requires the enforced Worker network",
        ));
    }
    let task_targets = task["input"]["authorization"]["targets"]
        .as_array()
        .ok_or_else(|| Error::invalid("Task authorized targets are missing"))?;
    for target in current["authorized_targets"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let selected = target
            .as_str()
            .ok_or_else(|| Error::invalid("invalid policy target"))?;
        if !task_targets
            .iter()
            .filter_map(Value::as_str)
            .any(|allowed| crate::runs::src_api::target_matches(selected, allowed))
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "authorization_unresolved",
                "policy target exceeds Task authorization",
            ));
        }
    }
    let exceptions = current["environment_requirements"]["service_exceptions"].as_array();
    if network == "enforced" && exceptions.is_none_or(Vec::is_empty) {
        return Err(unavailable(
            "enforced Scope requires explicit control/model provider service exceptions",
        ));
    }
    for exception in exceptions.into_iter().flatten() {
        if exception["host"].as_str().is_none_or(str::is_empty)
            || exception["port"]
                .as_u64()
                .is_none_or(|v| v == 0 || v > 65535)
        {
            return Err(unavailable("invalid infrastructure service exception"));
        }
    }
    let mut data = current.clone();
    data.as_object_mut()
        .ok_or_else(|| Error::invalid("invalid policy"))?
        .remove("activation");
    data["environment_generation"] = current["version"].clone();
    data["src_policy_digest"]=current["digest"].clone();
    data["gateway_source_digest"]=json!(crate::src_resources::digest("src-egress"));
    data["upstream_proxies"]=super::proxypool::policy_projection(s,id)?;
    Ok(data)
}
fn digest(value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(browser_runtime::canonical_json(value).as_bytes())
    )
}
fn read_health(root: &Path) -> Option<Value> {
    std::fs::read(root.join("health.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}
fn verified(
    current: &Value,
    health: &Value,
    id: &str,
    expected: &str,
    generation: &Value,
    inspection: &str,
    mode: &str,
) -> bool {
    let started = current["State"]["StartedAt"]
        .as_str()
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .map(|v| v.timestamp() as f64)
        .unwrap_or(f64::MAX);
    current["State"]["Running"] == true
        && current["Config"]["Labels"]["muteki.src.task_id"] == id
        && current["Config"]["Labels"]["muteki.src.gateway_schema"] == "rust1"
        && current["Config"]["Labels"]["muteki.src.source_digest"]==crate::src_resources::digest("src-egress")
        && health["source_digest"]==crate::src_resources::digest("src-egress")
        && current["HostConfig"]["CapAdd"]
            .as_array()
            .is_some_and(|v| v.iter().any(|item| item == "NET_ADMIN"))
        && health["state"] == "ready"
        && health["verified_at"]
            .as_f64()
            .is_some_and(|v| v + 0.5 >= started)
        && health["digest"] == expected
        && health["generation"] == *generation
        && health["network_enforced"] == (mode=="enforced")
        && health["inspection"]["state"] == "ready"
        && (inspection != "enforce" || health["inspection"]["enforce_ready"] == true)
}
fn result(id: &str, current: &Value, health: &Value) -> Value {
    let mode = current["network_mode"].as_str().unwrap_or("declared");
    let inspection = current["inspection_requirements"]["mode"]
        .as_str()
        .unwrap_or("off");
    let gateway = name(id);
    json!({"worker_network":if mode=="enforced"{json!(format!("container:{gateway}"))}
        else if inspection=="record"{json!(format!("src-record:{gateway}"))}else{Value::Null},
        "gateway_container":gateway,"policy_digest":current["digest"],
        "policy_version":current["version"],"environment_generation":current["version"],
        "network_enforced":mode=="enforced","content_enforced":inspection=="enforce",
        "inspection_recording":inspection=="record","state":"ready","health":health})
}
fn declared(current: &Value) -> Value {
    json!({"worker_network":null,"gateway_container":null,"policy_digest":current["digest"],
        "policy_version":current["version"],"environment_generation":current["version"],
        "network_enforced":false,"content_enforced":false,"inspection_recording":false,
        "state":"declared","health":{}})
}
fn inspect_sync(name: &str) -> Option<Value> {
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
}
fn latest_is_newer(container:&Value)->bool{container["Config"]["Labels"]["muteki.src.source_digest"]!=crate::src_resources::digest("src-egress")}
pub(super) fn status(s: &AppState, id: &str) -> Api<Value> {
    let current = policy(s, id)?;
    let data = policy_data(s, id, &current)?;
    let mode = current["network_mode"].as_str().unwrap_or("declared");
    let inspection = current["inspection_requirements"]["mode"]
        .as_str()
        .unwrap_or("off");
    if mode == "declared" && inspection == "off" {
        return Ok(declared(&current));
    }
    let gateway = name(id);
    let Some(container) = inspect_sync(&gateway) else {
        return Ok(
            json!({"state":"unavailable","network_enforced":false,"content_enforced":false}),
        );
    };
    if latest_is_newer(&container) {
        return Ok(
            json!({"state":"latest_image_pending_activation","network_enforced":false,"content_enforced":false}),
        );
    }
    let Some(health) = read_health(&gateway_root(s, id)?) else {
        return Ok(json!({"state":"unverified","network_enforced":false,"content_enforced":false}));
    };
    if verified(
        &container,
        &health,
        id,
        &digest(&data),
        &current["version"],
        inspection,
        mode,
    ) {
        Ok(result(id, &current, &health))
    } else {
        Ok(
            json!({"state":"configuration_mismatch","network_enforced":false,"content_enforced":false,"health":health}),
        )
    }
}
pub(super) async fn ensure(s: &AppState, id: &str) -> Api<Value> {
    let _activation=crate::src_resources::activation(s,id).await.map_err(|message|Error::conflict(message))?;
    let current = policy(s, id)?;
    let data = policy_data(s, id, &current)?;
    let mode = current["network_mode"].as_str().unwrap_or("declared");
    let inspection = current["inspection_requirements"]["mode"]
        .as_str()
        .unwrap_or("off");
    if mode == "declared" && inspection == "off" {
        return Ok(declared(&current));
    }
    if let Ok(ready) = status(s, id) {
        if ready["state"] == "ready" {
            return Ok(ready);
        }
    }
    let gateway = name(id);
    let root = gateway_root(s, id)?;
    let expected = digest(&data);
    let context=crate::src_resources::context(s,"src-egress").map_err(unavailable)?;
    browser_runtime::image(IMAGE, Some(context)).await?;
    let mut existing = browser_runtime::inspect(&gateway).await?;
    if let Some(container) = &existing {
        if container["Config"]["Labels"]["muteki.src.task_id"] != id
            || container["Config"]["Labels"]["muteki.src.gateway_schema"] != "rust1"
        {
            return Err(Error::conflict("Task Worker gateway identity changed"));
        }
    }
    if existing.as_ref().is_some_and(latest_is_newer) {
        super::gateway_replacement_ready(s,id)?;
        browser_runtime::docker(&["rm".into(), "-f".into(), gateway.clone()], 20).await?;
        existing = None;
    }
    let temporary = root.join("policy.tmp");
    std::fs::write(&temporary, data.to_string()).map_err(crate::runs::src_api::db)?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o640))
        .map_err(crate::runs::src_api::db)?;
    std::fs::rename(&temporary, root.join("policy.json")).map_err(crate::runs::src_api::db)?;
    let mut public_policy = data.clone();
    public_policy.as_object_mut().unwrap().remove("upstream_proxies");
    let worker_policy = root.join("worker-policy.tmp");
    std::fs::write(&worker_policy, public_policy.to_string()).map_err(crate::runs::src_api::db)?;
    std::fs::rename(worker_policy, root.join("worker-policy.json")).map_err(crate::runs::src_api::db)?;
    let _ = std::fs::remove_file(root.join("health.json"));
    let _ = std::fs::remove_file(root.join("proxy-health.json"));
    if let Some(container) = existing {
        if container["State"]["Running"] == true {
            browser_runtime::docker(
                &[
                    "kill".into(),
                    "--signal".into(),
                    "HUP".into(),
                    gateway.clone(),
                ],
                10,
            )
            .await?;
        } else {
            browser_runtime::docker(&["start".into(), gateway.clone()], 20).await?;
        }
    } else {
        let gid = unsafe { libc::getgid() };
        browser_runtime::docker(
            &[
                "run".into(),
                "-d".into(),
                "--name".into(),
                gateway.clone(),
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
                "--label".into(),
                format!("muteki.src.policy_digest={expected}"),
                "-v".into(),
                format!("{}:/run/src", root.display()),
                IMAGE.into(),
            ],
            30,
        )
        .await?;
    }
    for _ in 0..150 {
        if let Some(health) = read_health(&root) {
            if health["state"] == "failed" {
                return Err(unavailable(format!(
                    "Task Worker gateway failed: {}",
                    health["error"]
                )));
            }
            if let Some(container) = browser_runtime::inspect(&gateway).await? {
                if verified(
                    &container,
                    &health,
                    id,
                    &expected,
                    &current["version"],
                    inspection,
                    mode,
                ) {
                    return Ok(result(id, &current, &health));
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err(unavailable("Task Worker gateway health did not verify"))
}
