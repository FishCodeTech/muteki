//! Measured physical-container usage. Shared totals are never divided into invented Run usage.
use super::*;
use std::collections::BTreeMap;
use tokio::sync::Mutex;

#[derive(Default)]
pub(crate) struct Monitor {
    sample: Mutex<Sampler>,
}
#[derive(Default)]
struct Sampler {
    cached: Option<(tokio::time::Instant, Value)>,
    smoothing: Smoothing,
}
#[derive(Default)]
struct Smoothing {
    current: Option<u32>,
    pending: Option<(u32, tokio::time::Instant)>,
}
impl Smoothing {
    fn choose(&mut self, target: u32, now: tokio::time::Instant, urgent: bool) -> u32 {
        let Some(current) = self.current else { self.current=Some(target); return target; };
        if current == target { self.pending=None; return current; }
        if urgent && target < current { self.current=Some(target); self.pending=None; return target; }
        let since = match self.pending {
            Some((pending, since)) if pending == target => since,
            _ => { self.pending=Some((target, now)); now },
        };
        let delay = Duration::from_secs(if target < current {30} else {60});
        if now.duration_since(since) >= delay { self.current=Some(target); self.pending=None; target } else { current }
    }
}
impl Monitor {
    pub(crate) async fn snapshot(&self, state: &AppState, link: Option<Arc<crate::runs::container_exec::SupervisorLink>>) -> Result<Value, String> {
        let mut snapshot = {
            let mut cached = self.sample.lock().await;
            if cached.cached.as_ref().is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(5)) {
                let (mut base, raw, health) = tokio::join!(super::snapshot_local(), super::output("docker", &[
                    "stats", "--no-stream", "--no-trunc", "--format", "{{json .}}"]), async {
                    if let Some(link)=link {
                        tokio::time::timeout(Duration::from_secs(2),link.health()).await.ok().and_then(Result::ok)
                    } else { None }
                });
                base["usage"] = match raw {
                    Some(raw) => match parse(&raw) {
                        Ok(value) => value,
                        Err(error) => json!({"available":false,"error":error}),
                    },
                    None => json!({"available":false,"error":"Docker usage collection failed or timed out"}),
                };
                let mut recommendation: Recommendation = serde_json::from_value(base["recommendation"].clone()).map_err(|e|e.to_string())?;
                if let Some(health) = health.filter(|health| health.memory_sampled && health.host_memory_total_bytes>0 &&
                    health.host_memory_available_bytes<=health.host_memory_total_bytes) {
                    let active = state.store.list(DISPATCHES).map_err(|e|e.to_string())?.iter()
                        .filter(|row|matches!(row["status"].as_str(),Some("running"|"detached"))).count() as u64;
                    let reserve = (health.host_memory_total_bytes/4).max(1024*1024*1024).min(health.host_memory_total_bytes);
                    let headroom = health.host_memory_available_bytes.saturating_sub(reserve);
                    let target = active.saturating_add(headroom / recommendation.estimated_worker_bytes.max(1))
                        .min(u64::from(recommendation.max_workers)) as u32;
                    let smoothed = cached.smoothing.choose(target,tokio::time::Instant::now(),health.host_memory_available_bytes<=reserve)
                        .min(recommendation.max_workers);
                    recommendation.memory_admission_ceiling=Some(target);
                    recommendation.max_workers=smoothed;
                    recommendation.max_runs=recommendation.max_runs.min(smoothed);
                    recommendation.source="docker_host_mem_available_with_worker_estimate".into();
                    base["memory_pressure"]=json!({"available":true,"source":"supervisor_proc_meminfo",
                        "host_total_bytes":health.host_memory_total_bytes,"host_available_bytes":health.host_memory_available_bytes,
                        "reserve_bytes":reserve,"admission_headroom_bytes":headroom,"target_max_workers":target,
                        "cooldown_pending":smoothed!=target,"decrease_delay_seconds":30,"increase_delay_seconds":60});
                } else {
                    cached.smoothing=Default::default();
                    base["memory_pressure"]=json!({"available":false,"reason":"No current supervisor memory sample; using total-capacity estimate"});
                }
                base["recommendation"]=json!(recommendation);
                base["limitations"]=json!(["Worker/browser memory costs remain initial estimates; workload calibration and host-native memory pressure are not yet incorporated"]);
                cached.cached = Some((tokio::time::Instant::now(), base));
            }
            cached.cached.as_ref().unwrap().1.clone()
        };
        let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for row in state.store.list("runner_runs").map_err(|e| e.to_string())? {
            if row["state"] == "stopped" { continue; }
            if let Some(id) = row["container_id"].as_str() {
                groups.entry(id.to_owned()).or_default().push(row);
            }
        }
        let measurements = snapshot["usage"]["containers"].as_array().cloned().unwrap_or_default();
        let owned: Vec<Value> = groups.into_iter().map(|(id, runs)| {
            let sample = measurements.iter().find(|row| row["container_id"] == id);
            let shared = runs.iter().any(|row| row["scope"] == "shared");
            json!({"container_id":id,"attribution":if shared{"shared_container"}else{"container"},
                "runs":runs.iter().map(|row|json!({"run_id":row["run_id"],"generation":row["generation"],"state":row["state"]})).collect::<Vec<_>>(),
                "configured_limits":runs.first().map(|row|row["resource_limits"].clone()),
                "limit_sources":runs.first().map(|row|row["resource_limit_sources"].clone()),
                "measurement":sample,"measurement_available":sample.is_some_and(|row|row["available"]==true)})
        }).collect();
        if let Some(usage) = snapshot["usage"].as_object_mut() { usage.remove("containers"); }
        snapshot["physical_runtimes"] = json!(owned);
        let dispatches=state.store.list(DISPATCHES).map_err(|e|e.to_string())?;
        let browsers=components::inventory(&state.store,&dispatches)?.browsers;
        snapshot["components"]=json!({"browser_runs":browsers.len(),"estimated_browser_bytes":components::BROWSER_BYTES,
            "reserved_browser_bytes":(browsers.len() as u64).saturating_mul(components::BROWSER_BYTES),"estimated":true});
        Ok(snapshot)
    }
}

fn bytes(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let split = value.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(value.len());
    let number: f64 = value[..split].parse().map_err(|_| "Invalid Docker memory measurement")?;
    let scale = match value[split..].trim() {
        "B" => 1f64, "kB" | "KB" => 1e3, "MB" => 1e6, "GB" => 1e9, "TB" => 1e12,
        "KiB" => 1024f64, "MiB" => 1024f64.powi(2), "GiB" => 1024f64.powi(3), "TiB" => 1024f64.powi(4),
        _ => return Err("Unknown Docker memory unit".into()),
    };
    let value = number * scale;
    if !value.is_finite() || value < 0.0 || value >= u64::MAX as f64 { return Err("Docker memory measurement out of range".into()); }
    Ok(value.ceil() as u64)
}
fn parse(raw: &str) -> Result<Value, String> {
    let mut rows = Vec::new();
    let mut memory = 0u64;
    let mut tasks = 0u64;
    let mut complete = true;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let row: Value = serde_json::from_str(line).map_err(|_| "Invalid Docker statistics response")?;
        let id = row["ID"].as_str().filter(|value|value.len()==64 && value.bytes().all(|byte|byte.is_ascii_hexdigit()))
            .ok_or("Docker statistics lacks immutable container identity")?;
        let (used, limit) = row["MemUsage"].as_str().and_then(|value|value.split_once('/')).ok_or("Docker memory statistics unavailable")?;
        let used = bytes(used)?; let limit = bytes(limit)?;
        let pids = row["PIDs"].as_str().and_then(|value|value.parse::<u64>().ok());
        let cpu = row["CPUPerc"].as_str().and_then(|value|value.strip_suffix('%')).and_then(|value|value.parse::<f64>().ok())
            .filter(|value|value.is_finite() && *value>=0.0);
        let available = limit > 0;
        complete &= available && pids.is_some() && cpu.is_some();
        if available { memory = memory.checked_add(used).ok_or("Docker usage total overflow")?; }
        tasks = tasks.checked_add(pids.unwrap_or(0)).ok_or("Docker task total overflow")?;
        rows.push(json!({"container_id":id,"available":available,"memory_working_set_bytes":available.then_some(used),
            "memory_limit_bytes":available.then_some(limit),"processes_and_threads":pids,"cpu_percent":cpu}));
    }
    Ok(json!({"available":true,"complete":complete,"sampled_at":chrono::Utc::now().to_rfc3339(),
        "source":"docker_stats","memory_semantics":"Linux Docker CLI usage excludes inactive file cache; this is not VM free memory",
        "measured_container_memory_bytes":memory,"measured_processes_and_threads":tasks,"containers":rows}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recommendation_cooldown_and_urgent_pressure_preserve_explicit_overrides() {
        let now=tokio::time::Instant::now();let mut smoothing=Smoothing::default();
        assert_eq!(smoothing.choose(4,now,false),4);
        assert_eq!(smoothing.choose(3,now+Duration::from_secs(1),false),4);
        assert_eq!(smoothing.choose(4,now+Duration::from_secs(10),false),4);
        assert_eq!(smoothing.choose(3,now+Duration::from_secs(20),false),4);
        assert_eq!(smoothing.choose(3,now+Duration::from_secs(50),false),3);
        assert_eq!(smoothing.choose(0,now+Duration::from_secs(51),true),0);
        assert_eq!(smoothing.choose(4,now+Duration::from_secs(52),false),0);
        assert_eq!(smoothing.choose(4,now+Duration::from_secs(111),false),0);
        assert_eq!(smoothing.choose(4,now+Duration::from_secs(112),false),4);
        let mut recommendation=recommend(4,6*1024*1024*1024,"test");recommendation.memory_admission_ceiling=Some(0);
        assert_eq!(effective(&Policy::Recommended,&recommendation),Some((0,0)));
        assert_eq!(effective(&Policy::Unlimited,&recommendation),None);
        assert_eq!(effective(&Policy::Manual{max_workers:50,max_runs:10},&recommendation),Some((50,10)));
    }
    #[test]
    fn docker_units_and_missing_samples_do_not_invent_zero_usage() {
        assert_eq!(bytes("1.5 GiB").unwrap(),1610612736);
        assert_eq!(bytes("0B").unwrap(),0);
        assert!(bytes("NaNGiB").is_err());assert!(bytes("12watts").is_err());
        let first=json!({"ID":"a".repeat(64),"MemUsage":"1.5MiB / 2GiB","PIDs":"17","CPUPerc":"125.5%"});
        let second=json!({"ID":"b".repeat(64),"MemUsage":"0B / 0B","PIDs":"0","CPUPerc":"0.00%"});
        let result=parse(&format!("{first}\n{second}\n")).unwrap();
        assert_eq!(result["measured_container_memory_bytes"],1572864);
        assert_eq!(result["complete"],false);
        assert_eq!(result["containers"][0]["cpu_percent"],125.5);
        assert!(result["containers"][1]["memory_working_set_bytes"].is_null());
        assert!(parse("invalid").is_err());
    }
}
