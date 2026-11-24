//! Node-local resource policy. A recommendation is an estimate, never a hidden cap.
mod metrics;
mod components;
pub(crate) use metrics::Monitor;
use crate::AppState;
use axum::{Json, Router, extract::{Path, State}, http::StatusCode, routing::get};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Policy {
    Recommended,
    Manual { max_workers: u32, max_runs: u32 },
    Unlimited,
}
impl Default for Policy { fn default() -> Self { Self::Recommended } }
impl Policy {
    pub fn validate(&self) -> Result<(), String> {
        if let Self::Manual { max_workers, max_runs } = self {
            if *max_workers == 0 || *max_runs == 0 {
                return Err("Manual concurrency limits must be positive; use unlimited to disable".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recommendation {
    pub max_workers: u32,
    pub max_runs: u32,
    /// Fresh pressure can restrict admission while the displayed recommendation
    /// waits for its smoothing window. Manual/unlimited policies ignore this.
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub memory_admission_ceiling: Option<u32>,
    pub memory_budget_bytes: u64,
    pub estimated_worker_bytes: u64,
    pub source: String,
    pub estimated: bool,
}

/// Conservative starting estimate, exposed as such until node workload measurements exist.
pub fn recommend(cpus: u32, memory: u64, source: &str) -> Recommendation {
    const GIB: u64 = 1024 * 1024 * 1024;
    let reserve = (memory / 4).max(GIB).min(memory);
    let budget = memory.saturating_sub(reserve);
    let workers = (budget / GIB).min(u64::from(cpus.max(1))).min(u64::from(u32::MAX)) as u32;
    Recommendation { max_workers: workers.max(1), max_runs: workers.max(1), memory_admission_ceiling: None,
        memory_budget_bytes: budget, estimated_worker_bytes: GIB, source: source.into(), estimated: true }
}

pub fn effective(policy: &Policy, recommendation: &Recommendation) -> Option<(u32, u32)> {
    match policy {
        Policy::Unlimited => None,
        Policy::Manual { max_workers, max_runs } => Some((*max_workers, *max_runs)),
        Policy::Recommended => {
            let workers=recommendation.memory_admission_ceiling.unwrap_or(recommendation.max_workers).min(recommendation.max_workers);
            Some((workers, recommendation.max_runs.min(workers)))
        },
    }
}

fn queue_store(state:&AppState)->&crate::store::Store{state.admission_store.as_ref().unwrap_or(&state.store)}

pub fn load(state: &AppState) -> Result<Policy, String> {
    match queue_store(state).get("runner_resource_policies", "local").map_err(|e| e.to_string())? {
        Some(value) => {
            let policy: Policy = serde_json::from_value(value).map_err(|e| e.to_string())?;
            policy.validate()?;
            Ok(policy)
        }
        None => Ok(Policy::default()),
    }
}

const DISPATCHES: &str = "runner_dispatches";
/// Durable reservation. An unresolved reservation after a crash must be reconciled,
/// not blindly discarded while its container may still be running.
pub struct Reservation {
    store: crate::store::Store,
    id: String,
}
fn settle_reservation(store:&crate::store::Store,row:&mut Value)->Result<(),String>{
    let pending=if let Some(command)=row["runner_command_id"].as_str(){
        store.get("runner_commands",command).map_err(|e|e.to_string())?.is_none_or(|r|!crate::runner_execution::facts(&r).stopped)
    }else{false};
    row["status"]=json!(if pending{"detached"}else{"released"});
    row["settled_at"]=json!(chrono::Utc::now().to_rfc3339());
    store.put(DISPATCHES,row["id"].as_str().ok_or("Reservation ID missing")?,row).map_err(|e|e.to_string())
}
fn pending_settlements()->&'static std::sync::Mutex<std::collections::HashMap<String,crate::store::Store>> {
    static PENDING:std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String,crate::store::Store>>>=std::sync::OnceLock::new();
    PENDING.get_or_init(Default::default)
}
fn settle_id(store:&crate::store::Store,id:&str)->Result<(),String>{
    store.document_scope(||->Result<(),String>{
        if let Some(mut row)=store.get(DISPATCHES,id).map_err(|e|e.to_string())?{settle_reservation(store,&mut row)?;}Ok(())
    }).map_err(|e|e.to_string())?
}
/// Keep failed Drop settlements owned until the durable reservation reflects
/// release/detachment. A database outage must not leak capacity for this epoch.
pub(crate) fn retry_settlements(_store:&crate::store::Store)->Result<(),String>{
    let pending:Vec<_>=pending_settlements().lock().unwrap().iter().map(|(id,store)|(id.clone(),store.clone())).collect();
    let mut errors=Vec::new();
    for(id,store)in pending{match settle_id(&store,&id){Ok(())=>{pending_settlements().lock().unwrap().remove(&id);},Err(error)=>errors.push(format!("{id}: {error}"))}}
    if errors.is_empty(){Ok(())}else{Err(errors.join("; "))}
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Err(error)=settle_id(&self.store,&self.id){
            tracing::error!(%error,reservation_id=%self.id,"Node reservation settlement is pending");
            pending_settlements().lock().unwrap().insert(self.id.clone(),self.store.clone());
            if let Ok(runtime)=tokio::runtime::Handle::try_current(){let store=self.store.clone();let id=self.id.clone();runtime.spawn(async move{
                while pending_settlements().lock().unwrap().contains_key(&id){
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if let Err(error)=retry_settlements(&store){tracing::warn!(%error,"Reservation settlement retry remains pending");}
                }
            });}
        }
    }
}

pub(crate) fn release_completed(store:&crate::store::Store,command:&str)->Result<(),String>{
    store.document_scope(||->Result<(),String>{
        let stopped=store.get("runner_commands",command).map_err(|e|e.to_string())?.is_some_and(|r|r["execution_stopped"]==true);
        for mut row in store.list(DISPATCHES).map_err(|e|e.to_string())?{
            if row["runner_command_id"]==command&&(row["status"]=="detached"||(stopped&&row["status"]=="running")){settle_reservation(store,&mut row)?;}
        }Ok(())
    }).map_err(|e|e.to_string())?
}

pub(crate) fn reconcile_restart(state:&AppState)->Result<(),String>{
    state.store.document_scope(||->Result<(),String>{
        for mut row in state.store.list(DISPATCHES).map_err(|e|e.to_string())?{
            if row["host_epoch"]==state.chat.epoch()||matches!(row["status"].as_str(),Some("released"|"interrupted")){continue;}
            if row["runner_command_id"].is_string(){settle_reservation(&state.store,&mut row)?;}
            else {row["status"]=json!("interrupted");row["reason"]=json!("web_restarted_before_confirmed_dispatch");
                state.store.put(DISPATCHES,row["id"].as_str().ok_or("Reservation ID missing")?,&row).map_err(|e|e.to_string())?;}
        }Ok(())
    }).map_err(|e|e.to_string())?
}

pub(crate) fn release_unadmitted_run(state: &AppState, run: &str, generation: u64) -> Result<bool, String> {
    let store = queue_store(state);
    store.document_scope(|| -> Result<bool, String> {
        let mut rows: Vec<_> = store.list(DISPATCHES).map_err(|error| error.to_string())?
            .into_iter().filter(|row| row["run_id"] == run && row["generation"] == generation).collect();
        if rows.iter().any(|row| {
            !matches!(row["status"].as_str(), Some("queued" | "interrupted" | "released"))
                || !row["admitted_at"].is_null() || !row["admission_sequence"].is_null()
                || !row["runner_command_id"].is_null()
        }) {
            return Ok(false);
        }
        for row in rows.iter_mut().filter(|row| row["status"] != "released") {
            row["reason"] = json!("operator_stop_before_admission");
            settle_reservation(store, row)?;
        }
        Ok(true)
    }).map_err(|error| error.to_string())?
}

#[derive(Clone, Copy)]
pub struct Workload {
    pub generation: u64,
    pub browser: bool,
}
impl Reservation {
    pub fn id(&self)->&str{&self.id}
    pub fn enqueue(state: &AppState, run_id: &str, worker_id: &str) -> Result<Self, String> {
        Self::enqueue_with_workload(state,run_id,worker_id,Workload{generation:1,browser:false})
    }
    pub fn enqueue_with_workload(state:&AppState,run_id:&str,worker_id:&str,workload:Workload)->Result<Self,String>{Self::enqueue_on_node(state,run_id,worker_id,workload,"local")}
    pub fn enqueue_on_node(state:&AppState,run_id:&str,worker_id:&str,workload:Workload,node:&str)->Result<Self,String>{
        if workload.generation==0 {return Err("Resource reservation generation must be positive".into());}
        let id = uuid::Uuid::new_v4().to_string();
        let store=queue_store(state);
        store.put(DISPATCHES, &id, &json!({"id":id,"node_id":node,
            "run_id":run_id,"worker_id":worker_id,"generation":workload.generation,"browser":workload.browser,"status":"queued","priority":0,"host_epoch":state.chat.epoch(),
            "queued_at":chrono::Utc::now().to_rfc3339()})).map_err(|e| e.to_string())?;
        let dispatches = store.list(DISPATCHES).map_err(|e|e.to_string())?;
        if let Some(prior)=dispatches.iter().find(|row|row["run_id"]==run_id&&row["worker_id"]==worker_id&&row["generation"]==workload.generation&&row["status"]=="interrupted"&&row["reason"]=="web_restarted_before_confirmed_dispatch"
            && !dispatches.iter().any(|later| later["restored_from"] == row["id"])){
            let mut restored=store.get(DISPATCHES,&id).map_err(|e|e.to_string())?.ok_or("New reservation missing")?;
            restored["priority"]=prior["priority"].clone();restored["queued_at"]=prior["queued_at"].clone();restored["restored_from"]=prior["id"].clone();
            store.put(DISPATCHES,&id,&restored).map_err(|e|e.to_string())?;
        }
        Ok(Self { store: store.clone(), id })
    }

    pub fn try_admit(&self, state: &AppState, recommendation: &Recommendation) -> Result<bool, String> {
        if let Err(error)=retry_settlements(&self.store){tracing::warn!(%error,"Reservation settlements remain pending");}
        self.store.document_scope(|| -> Result<bool,String> {
            let current=self.store.get(DISPATCHES,&self.id).map_err(|e|e.to_string())?.ok_or("Dispatch reservation missing")?;
            let node=current["node_id"].as_str().unwrap_or("local");
            let policy=load_node(state,node)?;
            if node!="local"&&(!state.remote_runners.online(node)||queue_store(state).get("runner_node_resources",node).map_err(|e|e.to_string())?.is_none_or(|row|chrono::Utc::now().timestamp()-row["received_at"].as_i64().unwrap_or(0)>25)){
                return components::waiting(&self.store,&current,json!({"reason":"node_offline_or_resources_stale"}));
            }
            let rows:Vec<_> = self.store.list(DISPATCHES).map_err(|e| e.to_string())?.into_iter().filter(|row|row["node_id"].as_str().unwrap_or("local")==node).collect();
            let current=&current;
            if current["status"] == "running" { return Ok(true); }
            if current["status"] != "queued" { return Err("Dispatch reservation is no longer queued".into()); }
            let run = current["run_id"].clone();
            if let Some((workers, runs)) = effective(&policy, recommendation) {
                let active: Vec<_> = rows.iter().filter(|row| matches!(row["status"].as_str(),Some("running"|"detached"))).collect();
                let inventory=components::inventory_node(&self.store,&rows,node)?;
                let active_runs=inventory.runs;
                if active.len() >= workers as usize ||
                    (!active_runs.contains(run.as_str().unwrap_or("")) && active_runs.len() >= runs as usize) {
                    return components::waiting(&self.store,current,json!({"reason":"node_concurrency_limit"}));
                }
                let owners=(policy==Policy::Recommended).then_some(inventory.browsers);
                let required=|candidate:&Value| owners.as_ref().map(|owners|components::required_memory(owners,candidate,active.len(),recommendation.estimated_worker_bytes));
                if let Some(bytes)=required(current).filter(|bytes|*bytes>recommendation.memory_budget_bytes) {
                    return components::waiting(&self.store,current,json!({"reason":"component_memory_estimate", "required_bytes":bytes,
                        "budget_bytes":recommendation.memory_budget_bytes,"estimated_browser_bytes":components::BROWSER_BYTES}));
                }
                let eligible: Vec<_> = rows.iter().filter(|row| row["status"] == "queued" &&
                    (active_runs.len() < runs as usize || active_runs.contains(row["run_id"].as_str().unwrap_or(""))) &&
                    required(row).is_none_or(|bytes|bytes<=recommendation.memory_budget_bytes)).collect();
                if next_dispatch(&eligible, &rows, chrono::Utc::now().timestamp())
                    .is_some_and(|row| row["id"] != self.id) {
                    return components::waiting(&self.store,current,json!({"reason":"fair_queue_turn"}));
                }
            }
            let mut row = self.store.get(DISPATCHES, &self.id).map_err(|e| e.to_string())?.ok_or("Dispatch disappeared")?;
            let sequence = self.store.get("runner_scheduler", "local").map_err(|e| e.to_string())?
                .and_then(|row| row["sequence"].as_u64()).unwrap_or(0).checked_add(1).ok_or("Scheduler sequence exhausted")?;
            self.store.put("runner_scheduler", "local", &json!({"sequence":sequence})).map_err(|e| e.to_string())?;
            row.as_object_mut().unwrap().remove("waiting");
            row["status"] = json!("running");
            row["admission_sequence"] = json!(sequence);
            row["admitted_at"] = json!(chrono::Utc::now().to_rfc3339());
            self.store.put(DISPATCHES, &self.id, &row).map_err(|e| e.to_string())?;
            Ok(true)
        }).map_err(|e| e.to_string())?
    }
}

/// Queue one container execution (planner call, catalog or model probe) behind the same node
/// concurrency and memory budget as ordinary Workers. The caller keeps the returned
/// reservation until the process exits; `container` is the Runner identity the start is checked against.
pub async fn admit_wait(state:&AppState,container:&str,worker:&str,node:&str,wait:Duration,mut queued:impl FnMut())->Result<Reservation,String>{
    let reservation=Reservation::enqueue_on_node(state,container,worker,Workload{generation:1,browser:false},node)?;
    let deadline=tokio::time::Instant::now()+wait;
    let mut recommendation:Option<(Recommendation,tokio::time::Instant)>=None;
    let mut announced=false;
    loop{
        if recommendation.as_ref().is_none_or(|(_,at)|at.elapsed()>=Duration::from_secs(10)){
            let snapshot=tokio::time::timeout_at(deadline,snapshot_node(state,node)).await.map_err(|_|"Timed out waiting for node capacity".to_owned())?;
            recommendation=Some((serde_json::from_value(snapshot["recommendation"].clone()).map_err(|e|format!("Resource sample is invalid: {e}"))?,tokio::time::Instant::now()));
        }
        if reservation.try_admit(state,&recommendation.as_ref().unwrap().0)?{return Ok(reservation);}
        if !announced{announced=true;queued();}
        if tokio::time::Instant::now()>=deadline{return Err("Timed out waiting for node capacity".into());}
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Priority ages by one point per 30 seconds of waiting. Among equal effective
/// priorities, prefer the Run least recently admitted, then FIFO within a Run.
fn next_dispatch<'a>(eligible: &[&'a Value], rows: &[Value], now: i64) -> Option<&'a Value> {
    let mut last = std::collections::HashMap::<&str, u64>::new();
    for row in rows {
        if let (Some(run), Some(sequence)) = (row["run_id"].as_str(), row["admission_sequence"].as_u64()) {
            last.entry(run).and_modify(|value| *value = (*value).max(sequence)).or_insert(sequence);
        }
    }
    let score = |row: &Value| {
        let queued = row["queued_at"].as_str().and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|date| date.timestamp()).unwrap_or(now);
        row["priority"].as_i64().unwrap_or(0).clamp(-20, 20) + now.saturating_sub(queued).max(0) / 30
    };
    eligible.iter().copied().min_by(|a, b| score(b).cmp(&score(a))
        .then_with(|| last.get(a["run_id"].as_str().unwrap_or("")).copied().unwrap_or(0)
            .cmp(&last.get(b["run_id"].as_str().unwrap_or("")).copied().unwrap_or(0)))
        .then_with(|| a["queued_at"].as_str().cmp(&b["queued_at"].as_str()))
        .then_with(|| a["id"].as_str().cmp(&b["id"].as_str())))
}

async fn output(program: &str, args: &[&str]) -> Option<String> {
    let result = tokio::time::timeout(Duration::from_secs(5),
        crate::host_process::command(program).args(args).kill_on_drop(true).output()).await.ok()?.ok()?;
    result.status.success().then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

pub async fn snapshot() -> Value {
    if let Some(socket) = crate::runner::configured_socket() {
        return match crate::runner::request(&socket,"/v1/resources").await {
            Ok(value) => value,
            Err(error) => json!({"node_id":"local","error":error,"recommendation":recommend(1,2*1024*1024*1024,"runner_unavailable_estimate")}),
        };
    }
    snapshot_local().await
}

pub(crate) async fn snapshot_local() -> Value {
    let host_cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    #[cfg(target_os = "macos")]
    let host_memory = output("sysctl", &["-n", "hw.memsize"]).await.and_then(|s| s.parse::<u64>().ok());
    #[cfg(not(target_os = "macos"))]
    let host_memory = tokio::fs::read_to_string("/proc/meminfo").await.ok().and_then(|s|
        s.lines().find_map(|line| line.strip_prefix("MemTotal:").and_then(|rest|
            rest.split_whitespace().next()?.parse::<u64>().ok()?.checked_mul(1024))));
    let docker = output("docker", &["info", "--format", "{{json .}}"])
        .await.and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let docker_memory = docker.as_ref().and_then(|d| d["MemTotal"].as_u64()).filter(|v| *v > 0);
    let docker_cpus = docker.as_ref().and_then(|d| d["NCPU"].as_u64()).and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
    let memory = match (host_memory, docker_memory) { (Some(a),Some(b)) => Some(a.min(b)), (a,b) => a.or(b) };
    let cpus = docker_cpus.map(|n| n.min(host_cpus)).unwrap_or(host_cpus);
    let source = if docker_memory.is_some() { "host_and_docker_limits" } else { "host_limits" };
    let recommendation = recommend(cpus, memory.unwrap_or(2 * 1024 * 1024 * 1024),
        if memory.is_some() { source } else { "fallback_memory_estimate" });
    json!({"node_id":"local", "sampled_at":chrono::Utc::now().to_rfc3339(),
        "host":{"cpus":host_cpus,"memory_bytes":host_memory},
        "docker":{"available":docker.is_some(),"cpus":docker_cpus,"memory_bytes":docker_memory},
        "recommendation":recommendation,
        "limitations":["Initial estimate; workload/browser measurements and current memory pressure are not yet incorporated"]})
}

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;
fn failure(error: impl ToString) -> (StatusCode, Json<Value>) {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":error.to_string()})))
}
pub(crate) async fn status(State(state): State<Arc<AppState>>) -> Reply {
    let policy = load(&state).map_err(failure)?;
    let mut value = snapshot().await;
    let recommendation: Recommendation = serde_json::from_value(value["recommendation"].clone()).map_err(failure)?;
    value["effective_limits"] = match effective(&policy, &recommendation) {
        Some((workers,runs)) => json!({"max_workers":workers,"max_runs":runs}), None => Value::Null,
    };
    value["policy"] = serde_json::to_value(policy).map_err(failure)?;
    value["dispatches"] = json!(state.store.list(DISPATCHES).map_err(failure)?
        .into_iter().filter(|row| row["status"] != "released").collect::<Vec<_>>());
    Ok(Json(value))
}
pub(crate) async fn update(State(state): State<Arc<AppState>>, Json(policy): Json<Policy>) -> Reply {
    policy.validate().map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error":e}))))?;
    state.store.put("runner_resource_policies", "local", &serde_json::to_value(&policy).map_err(failure)?).map_err(failure)?;
    Ok(Json(json!({"node_id":"local","policy":policy})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DispatchPriority { priority: i64 }
pub(crate) async fn update_priority(State(state): State<Arc<AppState>>, Path(id): Path<String>, Json(input): Json<DispatchPriority>) -> Reply {
    if !(-20..=20).contains(&input.priority) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error":"Priority must be between -20 and 20"}))));
    }
    let result = state.store.document_scope(|| -> Result<Value, (StatusCode, Json<Value>)> {
        let mut row = state.store.get(DISPATCHES, &id).map_err(failure)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error":"Dispatch not found"}))))?;
        if row["status"] != "queued" {
            return Err((StatusCode::CONFLICT, Json(json!({"error":"Only queued dispatches can change priority"}))));
        }
        row["priority"] = json!(input.priority);
        state.store.put(DISPATCHES, &id, &row).map_err(failure)?;
        Ok(row)
    }).map_err(failure)??;
    Ok(Json(result))
}


fn load_node(state:&AppState,node:&str)->Result<Policy,String>{
    if node=="local"{return load(state);}
    match queue_store(state).get("runner_resource_policies",node).map_err(|e|e.to_string())?{
        Some(value)=>{let policy:Policy=serde_json::from_value(value).map_err(|e|e.to_string())?;policy.validate()?;Ok(policy)},
        None=>load(state),
    }
}
pub async fn snapshot_node(state:&AppState,node:&str)->Value{
    if node=="local"{return snapshot().await;}
    queue_store(state).get("runner_node_resources",node).ok().flatten().unwrap_or_else(||json!({"node_id":node,"pending":true,
        "recommendation":recommend(1,2*1024*1024*1024,"remote_metrics_pending_no_admission")}))
}
pub(crate) async fn node_status(State(state):State<Arc<AppState>>,Path(node):Path<String>)->Reply{
    let mut value=snapshot_node(&state,&node).await;
    let policy=load_node(&state,&node).map_err(failure)?;
    value["policy"]=json!(policy);value["connected"]=json!(state.remote_runners.online(&node));
    value["dispatches"]=json!(state.store.list(DISPATCHES).map_err(failure)?.into_iter().filter(|row|row["node_id"]==node&&row["status"]!="released").collect::<Vec<_>>());
    Ok(Json(value))
}
pub(crate) async fn update_node(State(state):State<Arc<AppState>>,Path(node):Path<String>,Json(policy):Json<Policy>)->Reply{
    if state.store.get("runner_nodes",&node).map_err(failure)?.is_none(){return Err((StatusCode::NOT_FOUND,Json(json!({"error":"Node not found"}))));}
    policy.validate().map_err(failure)?;
    state.store.put("runner_resource_policies",&node,&json!(policy)).map_err(failure)?;
    Ok(Json(json!({"node_id":node,"policy":policy})))
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/runner/resources", get(status).put(update))
        .route("/api/runner/dispatches/{id}/priority", axum::routing::put(update_priority))
        .route("/api/runner/nodes/{id}/resources",get(node_status).put(update_node))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_budget_is_per_generation_and_does_not_block_a_fitting_nonbrowser() {
        let dir=tempfile::tempdir().unwrap();let state=AppState::new(crate::store::Store::open(dir.path().join("node.sqlite")).unwrap());
        let mut recommendation=recommend(8,6*1024*1024*1024,"test");
        recommendation.memory_budget_bytes=3*1024*1024*1024+components::BROWSER_BYTES;
        let browser=Workload{generation:1,browser:true};
        let a1=Reservation::enqueue_with_workload(&state,"a","a1",browser).unwrap();assert!(a1.try_admit(&state,&recommendation).unwrap());
        let a2=Reservation::enqueue_with_workload(&state,"a","a2",browser).unwrap();assert!(a2.try_admit(&state,&recommendation).unwrap());
        let b=Reservation::enqueue_with_workload(&state,"b","b1",browser).unwrap();assert!(!b.try_admit(&state,&recommendation).unwrap());
        assert_eq!(state.store.get(DISPATCHES,&b.id).unwrap().unwrap()["waiting"]["reason"],"component_memory_estimate");
        let c=Reservation::enqueue(&state,"c","c1").unwrap();assert!(c.try_admit(&state,&recommendation).unwrap());
        state.store.put("runner_runs","a",&json!({"run_id":"a","generation":1,"state":"active","browser_reserved":true})).unwrap();
        drop(a1);drop(a2);
        let inventory=components::inventory(&state.store,&state.store.list(DISPATCHES).unwrap()).unwrap();
        assert_eq!(inventory.browsers.len(),1);assert!(inventory.runs.contains("a"));
        state.store.put("runner_runs","a",&json!({"run_id":"a","generation":1,"state":"stopped","browser_reserved":true})).unwrap();
        assert!(components::inventory(&state.store,&state.store.list(DISPATCHES).unwrap()).unwrap().browsers.is_empty());
    }
    #[test]
    fn idle_runtime_keeps_run_slot_but_manual_and_unlimited_bypass_memory_estimates() {
        let dir=tempfile::tempdir().unwrap();let state=AppState::new(crate::store::Store::open(dir.path().join("node.sqlite")).unwrap());
        let mut recommendation=recommend(4,6*1024*1024*1024,"test");recommendation.memory_budget_bytes=1024*1024*1024;
        let browser=Workload{generation:1,browser:true};
        let a=Reservation::enqueue_with_workload(&state,"a","a1",browser).unwrap();assert!(!a.try_admit(&state,&recommendation).unwrap());
        state.store.put("runner_resource_policies","local",&json!({"mode":"manual","max_workers":2,"max_runs":1})).unwrap();
        assert!(a.try_admit(&state,&recommendation).unwrap());
        state.store.put("runner_runs","a",&json!({"run_id":"a","generation":1,"state":"active","browser_reserved":true})).unwrap();drop(a);
        let b=Reservation::enqueue_with_workload(&state,"b","b1",browser).unwrap();assert!(!b.try_admit(&state,&recommendation).unwrap());
        let a2=Reservation::enqueue_with_workload(&state,"a","a2",browser).unwrap();assert!(a2.try_admit(&state,&recommendation).unwrap());
        state.store.put("runner_resource_policies","local",&json!({"mode":"unlimited"})).unwrap();assert!(b.try_admit(&state,&recommendation).unwrap());
    }
    #[test]
    fn ineligible_new_run_does_not_block_existing_run_workers() {
        let dir=tempfile::tempdir().unwrap();
        let state=AppState::new(crate::store::Store::open(dir.path().join("node.sqlite")).unwrap());
        state.store.put("runner_resource_policies","local",&json!({"mode":"manual","max_workers":2,"max_runs":1})).unwrap();
        let recommendation=recommend(4,6*1024*1024*1024,"test");
        let a=Reservation::enqueue(&state,"a","a1").unwrap();
        assert!(a.try_admit(&state,&recommendation).unwrap());
        let b=Reservation::enqueue(&state,"b","b1").unwrap();
        let a2=Reservation::enqueue(&state,"a","a2").unwrap();
        assert!(!b.try_admit(&state,&recommendation).unwrap());
        assert!(a2.try_admit(&state,&recommendation).unwrap());
        drop(a); drop(a2);
        assert!(b.try_admit(&state,&recommendation).unwrap());
    }
    #[test]
    fn admission_rotates_runs_and_ages_priority_without_a_hidden_unlimited_cap() {
        let dir=tempfile::tempdir().unwrap();
        let state=AppState::new(crate::store::Store::open(dir.path().join("node.sqlite")).unwrap());
        state.store.put("runner_resource_policies","local",&json!({"mode":"manual","max_workers":1,"max_runs":1})).unwrap();
        let recommendation=recommend(4,6*1024*1024*1024,"test");
        let a=Reservation::enqueue(&state,"a","a1").unwrap();
        assert!(a.try_admit(&state,&recommendation).unwrap());
        let a2=Reservation::enqueue(&state,"a","a2").unwrap();
        let b=Reservation::enqueue(&state,"b","b1").unwrap();
        let now=chrono::Utc::now().to_rfc3339();
        for id in [&a2.id,&b.id] {let mut row=state.store.get(DISPATCHES,id).unwrap().unwrap();row["queued_at"]=json!(now);state.store.put(DISPATCHES,id,&row).unwrap();}
        drop(a);
        assert!(!a2.try_admit(&state,&recommendation).unwrap());
        assert!(b.try_admit(&state,&recommendation).unwrap());
        state.store.put("runner_resource_policies","local",&json!({"mode":"unlimited"})).unwrap();
        assert!(a2.try_admit(&state,&recommendation).unwrap());
        let now=chrono::Utc::now().timestamp();
        let old=json!({"id":"old","run_id":"old","priority":-20,"queued_at":chrono::DateTime::from_timestamp(now-1230,0).unwrap().to_rfc3339()});
        let recent=json!({"id":"recent","run_id":"recent","priority":20,"queued_at":chrono::DateTime::from_timestamp(now,0).unwrap().to_rfc3339()});
        assert_eq!(next_dispatch(&[&recent,&old],&[],now).unwrap()["id"],"old");
    }
    #[tokio::test]
    async fn queued_priority_is_durable_and_cannot_mutate_an_admitted_dispatch() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("node.sqlite");
        let state=Arc::new(AppState::new(crate::store::Store::open(&path).unwrap()));
        let first=Reservation::enqueue(&state,"a","a1").unwrap();
        let second=Reservation::enqueue(&state,"b","b1").unwrap();
        let _ = update_priority(State(state.clone()),Path(second.id.clone()),Json(DispatchPriority{priority:10})).await.unwrap();
        let other=AppState::new(crate::store::Store::open(&path).unwrap());
        assert_eq!(other.store.get(DISPATCHES,&second.id).unwrap().unwrap()["priority"],10);
        let recommendation=recommend(1,2*1024*1024*1024,"test");
        assert!(!first.try_admit(&state,&recommendation).unwrap());
        assert!(second.try_admit(&state,&recommendation).unwrap());
        let result=update_priority(State(state.clone()),Path(second.id.clone()),Json(DispatchPriority{priority:0})).await;
        assert_eq!(result.unwrap_err().0,StatusCode::CONFLICT);
    }
    #[test]
    fn confirmed_container_stop_releases_capacity_without_fabricating_worker_success(){
        let root=tempfile::tempdir().unwrap();let state=AppState::new(crate::store::Store::open(root.path().join("node.sqlite")).unwrap());
        state.store.put("runner_commands","lost",&json!({"state":"outcome_unknown","execution_stopped":true})).unwrap();
        state.store.put(DISPATCHES,"ticket",&json!({"id":"ticket","status":"running","runner_command_id":"lost"})).unwrap();
        release_completed(&state.store,"lost").unwrap();
        assert_eq!(state.store.get(DISPATCHES,"ticket").unwrap().unwrap()["status"],"released");
        assert!(state.store.get("runner_commands","lost").unwrap().unwrap()["exit"].is_null());
    }
    #[test]
    fn detached_execution_holds_capacity_until_matching_exit_receipt() {
        let dir=tempfile::tempdir().unwrap();
        let state=AppState::new(crate::store::Store::open(dir.path().join("node.sqlite")).unwrap());
        state.store.put("runner_resource_policies","local",&json!({"mode":"manual","max_workers":1,"max_runs":1})).unwrap();
        let recommendation=recommend(4,6*1024*1024*1024,"test");
        let first=Reservation::enqueue(&state,"a","worker-a").unwrap();
        assert!(first.try_admit(&state,&recommendation).unwrap());
        let mut row=state.store.get(DISPATCHES,&first.id).unwrap().unwrap();
        row["runner_command_id"]=json!("command-a");
        state.store.put(DISPATCHES,&first.id,&row).unwrap();
        state.store.put("runner_commands","command-a",&json!({"state":"started"})).unwrap();
        drop(first);
        assert_eq!(state.store.get(DISPATCHES,row["id"].as_str().unwrap()).unwrap().unwrap()["status"],"detached");
        let second=Reservation::enqueue(&state,"b","worker-b").unwrap();
        assert!(!second.try_admit(&state,&recommendation).unwrap());
        state.store.put("runner_commands","command-a",&json!({"state":"started","exit":{"t":"exit","rc":0}})).unwrap();
        release_completed(&state.store,"command-a").unwrap();
        assert!(second.try_admit(&state,&recommendation).unwrap());
    }
    #[test]
    fn restart_interruption_does_not_turn_queued_work_into_execution() {
        let dir=tempfile::tempdir().unwrap();
        let store=crate::store::Store::open(dir.path().join("node.sqlite")).unwrap();
        store.put(DISPATCHES,"queued-before-crash",&json!({"id":"queued-before-crash","status":"queued","host_epoch":"old"})).unwrap();
        let state=AppState::new(store);
        reconcile_restart(&state).unwrap();
        assert_eq!(state.store.get(DISPATCHES,"queued-before-crash").unwrap().unwrap()["status"],"interrupted");
        assert!(state.store.list("runner_commands").unwrap().is_empty());
    }
    #[test]
    fn queued_work_is_released_when_limits_are_disabled_and_policy_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resources.sqlite");
        let state = AppState::new(crate::store::Store::open(&path).unwrap());
        state.store.put("runner_resource_policies", "local", &json!({"mode":"manual","max_workers":1,"max_runs":1})).unwrap();
        let r = recommend(4, 6 * 1024 * 1024 * 1024, "test");
        let a = Reservation::enqueue(&state, "a", "worker-a").unwrap();
        let b = Reservation::enqueue(&state, "b", "worker-b").unwrap();
        assert!(a.try_admit(&state, &r).unwrap());
        assert!(!b.try_admit(&state, &r).unwrap());
        state.store.put("runner_resource_policies", "local", &json!({"mode":"unlimited"})).unwrap();
        assert!(b.try_admit(&state, &r).unwrap());
        drop(b);
        drop(a);
        drop(state);
        let reopened = AppState::new(crate::store::Store::open(&path).unwrap());
        assert_eq!(load(&reopened).unwrap(), Policy::Unlimited);
        assert!(reopened.store.list(DISPATCHES).unwrap().iter().all(|r| r["status"] == "released"));
    }
    #[test]
    fn overrides_are_not_clamped_to_recommendation() {
        let r = recommend(4, 6 * 1024 * 1024 * 1024, "docker");
        assert_eq!(r.max_workers, 4);
        assert_eq!(effective(&Policy::Unlimited, &r), None);
        assert_eq!(effective(&Policy::Manual { max_workers: 50, max_runs: 20 }, &r), Some((50,20)));
        assert!(Policy::Manual { max_workers: 0, max_runs: 1 }.validate().is_err());
    }
    #[tokio::test]
    #[ignore = "requires local Docker"]
    async fn detects_actual_docker_limit() {
        let value = snapshot().await;
        assert_eq!(value["docker"]["available"], true);
        let total = value["docker"]["memory_bytes"].as_u64().unwrap();
        assert!(value["recommendation"]["memory_budget_bytes"].as_u64().unwrap() < total);
    }
}
