//! Resolve automatic quotas once, before selecting a physical runtime or pool.
use crate::runs::container_exec::{self, ContainerHandle, WorkerResourceLimits};
use serde_json::{Value, json};

pub(super) async fn resolve(settings: &mut Value, existing: Option<&ContainerHandle>) -> Result<(), String> {
    let base = container_exec::resolve_worker_resource_limits(Some(settings))?;
    let keys = [("worker_memory", &["MUTEKI_WORKER_MEMORY"][..]),
        ("worker_cpus", &["MUTEKI_WORKER_CPUS"][..]),
        ("worker_pids_limit", &["MUTEKI_WORKER_PIDS", "MUTEKI_WORKER_PIDS_LIMIT"][..])];
    let mut sources = serde_json::Map::new();
    let mut automatic = Vec::new();
    for (key, vars) in keys {
        let source = if !settings[key].is_null() { "configured" }
            else if vars.iter().any(|var| std::env::var(var).is_ok_and(|value| !value.trim().is_empty())) { "environment" }
            else { automatic.push(key); "automatic" };
        sources.insert(key.into(), json!(source));
    }
    let shared = settings["worker_container_scope"] == "shared";
    let browser = settings["_runner_browser_enabled"] == true;
    let snapshot = if shared && existing.is_none() && !automatic.is_empty() {
        Some(crate::runner_resources::snapshot_local().await)
    } else { None };
    let mut resolved = base.clone();
    if let Some(existing) = existing {
        for key in ["worker_memory", "worker_cpus", "worker_pids_limit"] {
            if settings[key].is_null() { set_from(&mut resolved, &existing.limits, key); sources.insert(key.into(),json!("existing_runtime")); }
        }
    } else if shared {
        if let Some(snapshot) = snapshot {
            let proposed = shared_defaults(&base, &snapshot)?;
            for key in &automatic { set_from(&mut resolved, &proposed, key); sources.insert((*key).into(),json!("node_shared_default")); }
        }
    } else if browser {
        let mut proposed = base.clone();
        proposed.memory_bytes = proposed.memory_bytes.max(2 * 1024 * 1024 * 1024);
        proposed.memory = format!("{}m", proposed.memory_bytes / (1024 * 1024));
        proposed.pids_limit = proposed.pids_limit.max(512);
        for key in &automatic { set_from(&mut resolved, &proposed, key); sources.insert((*key).into(),json!("browser_run_default")); }
    } else {
        for key in &automatic { sources.insert((*key).into(),json!("image_default")); }
    }
    let mib=1024*1024;
    settings["worker_memory"] = json!(if resolved.memory_bytes % mib == 0 {
        format!("{}m",resolved.memory_bytes/mib)
    } else {format!("{}b",resolved.memory_bytes)});
    settings["worker_cpus"] = json!(resolved.cpus);
    settings["worker_pids_limit"] = json!(resolved.pids_limit);
    settings["_runner_resource_limit_sources"] = json!(sources);
    Ok(())
}
fn set_from(target: &mut WorkerResourceLimits, source: &WorkerResourceLimits, key: &str) {
    match key {
        "worker_memory" => {target.memory=source.memory.clone();target.memory_bytes=source.memory_bytes;},
        "worker_cpus" => target.cpus=source.cpus,
        "worker_pids_limit" => target.pids_limit=source.pids_limit,
        _ => unreachable!(),
    }
}
fn shared_defaults(base: &WorkerResourceLimits, snapshot: &Value) -> Result<WorkerResourceLimits, String> {
    if snapshot["docker"]["available"] != true { return Err("Cannot resolve automatic shared-pool quotas without Docker capacity".into()); }
    let memory = snapshot["docker"]["memory_bytes"].as_u64().filter(|value|*value>0).ok_or("Docker memory capacity unavailable")?;
    let docker_cpus = snapshot["docker"]["cpus"].as_u64().filter(|value|*value>0).ok_or("Docker CPU capacity unavailable")?;
    let host_cpus = snapshot["host"]["cpus"].as_u64().filter(|value|*value>0).ok_or("Host CPU capacity unavailable")?;
    let budget = snapshot["recommendation"]["memory_budget_bytes"].as_u64().ok_or("Node memory budget unavailable")?;
    let workers = snapshot["recommendation"]["max_workers"].as_u64().ok_or("Node Worker estimate unavailable")?;
    let mut result = base.clone();
    let mib = 1024*1024;
    let memory_mib = budget.max(256*mib).min(memory) / mib;
    if memory_mib == 0 { return Err("Docker memory capacity is below automatic pool minimum".into()); }
    result.memory_bytes=memory_mib*mib;result.memory=format!("{memory_mib}m");
    result.cpus=host_cpus.min(docker_cpus) as f64;
    result.pids_limit=workers.saturating_mul(512).clamp(512,65536) as i64;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn explicit_browser_quotas_are_preserved_and_equivalent_sizes_normalize() {
        for scope in ["run","shared"] {
            let mut settings=json!({"worker_container_scope":scope,"_runner_browser_enabled":true,
                "worker_memory":"0.5g","worker_cpus":0.5,"worker_pids_limit":128});
            resolve(&mut settings,None).await.unwrap();
            assert_eq!(settings["worker_memory"],"512m");assert_eq!(settings["worker_cpus"],0.5);assert_eq!(settings["worker_pids_limit"],128);
            assert_eq!(settings["_runner_resource_limit_sources"]["worker_memory"],"configured");
        }
    }
    #[test]
    fn shared_defaults_follow_vm_capacity_without_altering_the_input() {
        let base=WorkerResourceLimits::full_defaults();
        let snapshot=json!({"host":{"cpus":16},"docker":{"available":true,"cpus":4,"memory_bytes":6*1024u64*1024*1024},
            "recommendation":{"memory_budget_bytes":4*1024u64*1024*1024,"max_workers":4}});
        let result=shared_defaults(&base,&snapshot).unwrap();
        assert_eq!(result.memory,"4096m");assert_eq!(result.cpus,4.0);assert_eq!(result.pids_limit,2048);
        assert_eq!(base.memory,"2g");assert_eq!(base.pids_limit,512);
        assert!(shared_defaults(&base,&json!({"docker":{"available":false}})).is_err());
    }
}
