//! Security task (CTF / Pentest) run lifecycle, control journal, blackboard state,
//! SSE replay/live stream, uploads, retention, and task domain APIs.

mod continuation;
pub(crate) mod evidence;
mod execution_budget;
mod followup;
mod knowledge;
mod launch_config;
mod planner_plugins;
mod reports;
mod terminal;
mod usage;
mod worker_mcp;
mod worker_poc;
mod worker_restart;
mod worker_result;
pub use worker_restart::TargetRestarter;
mod control_recovery;
mod credential_revocation;
mod idle_ownership;
mod queued_restart;
mod restart_recovery;
pub(crate) use credential_revocation::{
    record as record_credential_projection, revoke as revoke_account_workers,
};
pub(crate) use restart_recovery::{
    announce_remote_reattach, drain_remote_ingress, settle_remote_run,
};
mod runner_stop_recovery;
pub(crate) use runner_stop_recovery::{
    refresh as refresh_control_result, settle as settle_runner_stops,
};
fn recover_worker_results_for(
    state: &AppState,
    module: Option<crate::business::Module>,
) -> Result<(), String> {
    for run in state
        .store
        .list("runs")
        .map_err(|error| error.to_string())?
    {
        if module.is_some_and(|module| crate::business::Module::for_run(&run) != module) {
            continue;
        }
        let Some(id) = run["run_id"].as_str() else {
            continue;
        };
        if !graph::graph_db_path(state, id).is_file()
            || !state.runs.claim_execution_owner(state, id)?
        {
            continue;
        }
        let pending_runner = state
            .store
            .list("runner_commands")
            .map_err(|e| e.to_string())?
            .iter()
            .any(|command| {
                command["run_id"] == id
                    && !command["exit"].is_object()
                    && command["execution_stopped"] != true
            });
        if pending_runner || queued_restart::pending(state, id)? {
            continue;
        }
        worker_result::settle_claims(state, id, "interrupted", "process_restart")?;
        worker_result::recover_projections(state, id)?;
    }
    Ok(())
}
/// The common execution engine restores only the owning business module's Runs.
pub(crate) async fn recover_module(
    state: &Arc<AppState>,
    module: crate::business::Module,
) -> Result<(), String> {
    recover_worker_results_for(state, Some(module))?;
    restart_recovery::mark_for(state, Some(module))?;
    control_journal::recover_module(state, Some(module)).await
}
pub fn start_control_recovery_loop(state: std::sync::Arc<crate::AppState>) {
    if let Err(error) = followup::reconcile(&state) {
        tracing::error!(%error,"Followup restart reconciliation failed");
    }
    if let Err(error) = reports::reconcile(&state) {
        tracing::error!(%error,"Report generation reconciliation failed");
    }
    idle_ownership::spawn(state.clone());
    queued_restart::spawn(state.clone());
    restart_recovery::spawn(state.clone());
    control_recovery::spawn_recoveries(state.clone());
    control_journal::spawn(state);
}
mod board_context;
pub mod competition_api;
pub mod container_exec;
mod control_context;
mod control_contract;
mod control_decisions;
mod control_freeze;
pub mod control_journal;
pub mod coordinator;
#[path = "ctf/planner.rs"]
mod ctf_planner;
pub(crate) mod execution_guard;
pub mod graph;
mod image_capabilities;
pub mod local_pi;
pub mod pentest;
mod planner_artifacts;
mod planner_transport;
pub mod progress;
pub mod readiness;
pub mod retention;
pub mod settings;
pub(crate) mod shared_pools;
pub mod src_api;
pub(crate) mod supervisor_stream;
pub mod titler;
pub mod worker;
pub(crate) use worker::clear_stale_browser_profile_lock_local;
mod worker_cli_checkpoint;

pub fn ctf_step_system_prompt(restart_bound: bool) -> String {
    ctf_planner::pi_native::ctf_step_system(restart_bound)
}

pub use planner_transport::apply_step_tracker_from_events;
pub use terminal::settle_terminal_outcome;

use crate::{AppState, native_chat};
use agent::SessionCommand;
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Multipart, Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use fs2::FileExt;
use futures_util::StreamExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    sync::{broadcast, oneshot},
};

const REPLAY_BATCH_SIZE: usize = 500;

fn upload_limit(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<i64>().ok())
        .map(|value| value.max(1) as usize)
        .unwrap_or(default)
}

pub type ContainerCancelRequest = Arc<Mutex<Option<oneshot::Sender<oneshot::Sender<bool>>>>>;

pub struct ActiveWorker {
    pub solver_id: String,
    pub thread_id: String,
    pub abort: tokio::task::AbortHandle,
    pub engine: String,
    pub intent_id: String,
    pub lane: String,
    pub cancel: Option<ContainerCancelRequest>,
    pub host_cancel: Option<Arc<AtomicBool>>,
}

pub struct WorkerCancelOutcome {
    pub requested: Vec<String>,
    pub failures: Vec<String>,
}

#[derive(Default, Clone, Copy)]
struct ClockCursor {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    offset: u64,
    seq: u64,
    fact_seq: u64,
}

pub struct RunHandle {
    pub bus: broadcast::Sender<Value>,
    pub seq: AtomicU64,
    event_file_lock: Mutex<()>,
    terminal_publication: Mutex<()>,
    progress_lock: Mutex<()>,
    usage_lock: Mutex<()>,
    pub fact_seq: AtomicU64,
    pub artifact_seq: AtomicU64,
    pub paused: AtomicBool,
    pub draining: AtomicBool,
    pub deleting: AtomicBool,
    cleanup_watcher: AtomicBool,
    pub delete_lock: tokio::sync::Mutex<()>,
    pub coordinator_abort: Mutex<Option<tokio::task::AbortHandle>>,
    followup_abort: Mutex<Option<tokio::task::AbortHandle>>,
    coordinator_tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    pub workers: Mutex<HashMap<String, ActiveWorker>>,
    worker_tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    pub facts: Mutex<Vec<Value>>,
    pub dead_ends: Mutex<Vec<Value>>,
    pub directives: Mutex<Vec<Value>>,
    pub control_steer_acks: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    pub container_sessions: Mutex<HashMap<String, (Arc<container_exec::SupervisorLink>, String)>>,
    pub hitl_requests: Mutex<Vec<Value>>,
    pub rejected_flags: Mutex<HashSet<String>>,
    pub(super) settled_false_positives: Mutex<HashSet<String>>,
    pub cost_usage: Mutex<RunCostUsage>,
    context_read_bytes: Mutex<HashMap<String, usize>>,
    clock_cursor: Mutex<ClockCursor>,
    execution_owner: Mutex<Option<fs::File>>,
    pub guard_eval_lock: tokio::sync::Mutex<()>,
    pub guard_notify: tokio::sync::Notify,
}

#[derive(Default)]
pub struct RunCostUsage {
    pub total_usd: f64,
    pub by_solver: HashMap<String, f64>,
}

impl Default for RunHandle {
    fn default() -> Self {
        let (bus, _) = broadcast::channel(1024);
        Self {
            bus,
            seq: AtomicU64::new(0),
            event_file_lock: Mutex::new(()),
            terminal_publication: Mutex::new(()),
            progress_lock: Mutex::new(()),
            usage_lock: Mutex::new(()),
            fact_seq: AtomicU64::new(0),
            artifact_seq: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            deleting: AtomicBool::new(false),
            cleanup_watcher: AtomicBool::new(false),
            delete_lock: tokio::sync::Mutex::new(()),
            coordinator_abort: Mutex::new(None),
            followup_abort: Mutex::new(None),
            coordinator_tasks: Mutex::new(Vec::new()),
            workers: Mutex::new(HashMap::new()),
            worker_tasks: Mutex::new(Vec::new()),
            facts: Mutex::new(Vec::new()),
            dead_ends: Mutex::new(Vec::new()),
            directives: Mutex::new(Vec::new()),
            control_steer_acks: Mutex::new(HashMap::new()),
            container_sessions: Mutex::new(HashMap::new()),
            hitl_requests: Mutex::new(Vec::new()),
            rejected_flags: Mutex::new(HashSet::new()),
            settled_false_positives: Mutex::new(HashSet::new()),
            cost_usage: Mutex::new(RunCostUsage::default()),
            context_read_bytes: Mutex::new(HashMap::new()),
            clock_cursor: Mutex::new(ClockCursor::default()),
            execution_owner: Mutex::new(None),
            guard_eval_lock: tokio::sync::Mutex::new(()),
            guard_notify: tokio::sync::Notify::new(),
        }
    }
}

#[derive(Default)]
pub struct RunRegistry {
    target_restarters: Mutex<HashMap<String, TargetRestarter>>,
    handles: Mutex<HashMap<String, Arc<RunHandle>>>,
}

impl RunRegistry {
    /// The owning runner binds this in process after establishing target ownership.
    pub fn bind_target_restarter(&self, run_id: &str, restarter: TargetRestarter) {
        self.target_restarters
            .lock()
            .unwrap()
            .insert(run_id.into(), restarter);
    }
    pub fn has_target_restarter(&self, run_id: &str) -> bool {
        self.target_restarters.lock().unwrap().contains_key(run_id)
    }

    pub fn active_run_count(&self) -> usize {
        self.active_run_ids().len()
    }

    pub fn active_run_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .handles
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, handle)| {
                !handle.workers.lock().unwrap().is_empty()
                    || handle
                        .coordinator_abort
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|task| !task.is_finished())
                    || handle
                        .worker_tasks
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|task| !task.is_finished())
                    || handle
                        .followup_abort
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|task| !task.is_finished())
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    pub(super) fn has_execution_owner(&self, run_id: &str) -> bool {
        self.handles
            .lock()
            .unwrap()
            .get(run_id)
            .is_some_and(|handle| handle.execution_owner.lock().unwrap().is_some())
    }

    pub(super) fn existing_handle(&self, run_id: &str) -> Option<Arc<RunHandle>> {
        self.handles.lock().unwrap().get(run_id).cloned()
    }

    /// A RunManager owns one Run across its execution generations. The file
    /// lock makes that source ownership visible to another service process.
    pub fn claim_execution_owner(&self, state: &AppState, run_id: &str) -> Result<bool, String> {
        let handle = self.handle(state, run_id);
        let mut owner = handle.execution_owner.lock().unwrap();
        if owner.is_some() {
            return Ok(true);
        }
        let dir = control_dir(state, run_id);
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(dir.join("execution-owner.lock"))
            .map_err(|error| error.to_string())?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                let events_path = session_dir(state, run_id).join("events.jsonl");
                if let Ok(mut events_file) = OpenOptions::new()
                    .read(true)
                    .append(true)
                    .open(&events_path)
                {
                    if events_file.seek(SeekFrom::End(-1)).is_ok() {
                        let mut last = [0_u8; 1];
                        if events_file.read_exact(&mut last).is_ok() && last[0] != b'\n' {
                            events_file
                                .write_all(b"\n")
                                .map_err(|error| error.to_string())?;
                        }
                    }
                }
                let (seq, fact_seq) = read_jsonl_clocks_cached(state, run_id, &handle.clock_cursor);
                let supplemental = state
                    .store
                    .list_for_run_ids("run_followup_events", &HashSet::from([run_id.to_owned()]))
                    .map_err(|e| e.to_string())?
                    .iter()
                    .filter_map(|r| r["seq"].as_u64())
                    .max()
                    .unwrap_or(0);
                handle
                    .seq
                    .fetch_max(seq.max(supplemental), Ordering::SeqCst);
                handle.fact_seq.fetch_max(fact_seq, Ordering::SeqCst);
                let conn = open_control_db(state, run_id)?;
                control_context::recover_prior_epoch(&conn, run_id, state.chat.epoch())?;
                *owner = Some(file);
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn handle(&self, state: &AppState, run_id: &str) -> Arc<RunHandle> {
        let mut map = self.handles.lock().unwrap();
        if let Some(h) = map.get(run_id) {
            return h.clone();
        }
        let handle = Arc::new(RunHandle::default());
        let run_doc = state.store.get("runs", run_id).ok().flatten();
        if run_doc
            .as_ref()
            .is_some_and(|run| matches!(run["control_mode"].as_str(), Some("frozen" | "quiesced")))
        {
            handle.paused.store(true, Ordering::SeqCst);
        }
        let historical = run_doc
            .as_ref()
            .and_then(|run| run["history_event_file"].as_str())
            .is_some();
        let (max_seq, max_fact_seq) = if historical {
            persisted_event_iter(state, run_id)
                .map(event_clocks)
                .unwrap_or_else(|_| read_jsonl_clocks_cached(state, run_id, &handle.clock_cursor))
        } else {
            read_jsonl_clocks_cached(state, run_id, &handle.clock_cursor)
        };
        let supplemental = match state
            .store
            .list_for_run_ids("run_followup_events", &HashSet::from([run_id.to_owned()]))
        {
            Ok(rows) => rows
                .iter()
                .filter_map(|r| r["seq"].as_u64())
                .max()
                .unwrap_or(0),
            Err(error) => {
                tracing::error!(%error,"Followup event clock read failed");
                0
            }
        };
        handle
            .seq
            .store(max_seq.max(supplemental), Ordering::SeqCst);
        handle.fact_seq.store(max_fact_seq, Ordering::SeqCst);
        map.insert(run_id.to_owned(), handle.clone());
        handle
    }

    pub fn register_worker(&self, run_id: &str, worker: ActiveWorker) {
        let mut map = self.handles.lock().unwrap();
        let handle = map
            .entry(run_id.to_owned())
            .or_insert_with(|| Arc::new(RunHandle::default()))
            .clone();
        drop(map);
        handle
            .worker_tasks
            .lock()
            .unwrap()
            .retain(|task| !task.is_finished());
        handle
            .worker_tasks
            .lock()
            .unwrap()
            .push(worker.abort.clone());
        handle
            .workers
            .lock()
            .unwrap()
            .insert(worker.solver_id.clone(), worker);
    }

    pub fn set_worker_lane(&self, run_id: &str, solver_id: &str, lane: &str) {
        let handle = self.handles.lock().unwrap().get(run_id).cloned();
        if let Some(handle) = handle
            && let Some(worker) = handle.workers.lock().unwrap().get_mut(solver_id)
        {
            worker.lane = lane.to_owned();
        }
    }

    pub fn request_worker_cancel(
        &self,
        run_id: &str,
        scope_kind: &str,
        scope_value: &str,
    ) -> WorkerCancelOutcome {
        let handle = self.handles.lock().unwrap().get(run_id).cloned();
        let mut outcome = WorkerCancelOutcome {
            requested: Vec::new(),
            failures: Vec::new(),
        };
        let Some(handle) = handle else {
            return outcome;
        };
        let mut workers = handle.workers.lock().unwrap();
        workers.retain(|_, worker| !worker.abort.is_finished());
        for worker in workers.values_mut() {
            let selected = match scope_kind {
                "global" | "run" | "challenge" => true,
                "worker" | "solver" => worker.solver_id == scope_value,
                "engine" => worker.engine == scope_value,
                "intent" => worker.intent_id == scope_value,
                "lane" => worker.lane == scope_value,
                _ => false,
            };
            if !selected {
                continue;
            }
            let requested = if let Some(cancel) = &worker.cancel {
                if let Some(request) = cancel.lock().unwrap().take() {
                    let (reply, _confirmation) = oneshot::channel();
                    request.send(reply).is_ok()
                } else {
                    false
                }
            } else if let Some(cancel) = &worker.host_cancel {
                cancel.store(true, Ordering::SeqCst);
                true
            } else {
                false
            };
            if requested {
                outcome.requested.push(worker.solver_id.clone());
            } else {
                outcome.failures.push(worker.solver_id.clone());
            }
        }
        outcome.requested.sort();
        outcome.failures.sort();
        outcome
    }

    pub fn unregister_worker(&self, run_id: &str, solver_id: &str) -> Option<ActiveWorker> {
        let map = self.handles.lock().unwrap();
        let h = map.get(run_id)?.clone();
        drop(map);
        h.context_read_bytes.lock().unwrap().remove(solver_id);
        h.workers.lock().unwrap().remove(solver_id)
    }

    pub fn register_coordinator(&self, run_id: &str, abort: tokio::task::AbortHandle) {
        let mut map = self.handles.lock().unwrap();
        let handle = map
            .entry(run_id.to_owned())
            .or_insert_with(|| Arc::new(RunHandle::default()))
            .clone();
        drop(map);
        let mut guard = handle.coordinator_abort.lock().unwrap();
        if let Some(prev) = guard.take() {
            prev.abort();
        }
        handle
            .coordinator_tasks
            .lock()
            .unwrap()
            .retain(|task| !task.is_finished());
        handle.coordinator_tasks.lock().unwrap().push(abort.clone());
        *guard = Some(abort);
    }

    pub fn coordinator_live(&self, run_id: &str) -> bool {
        let handle = self.handles.lock().unwrap().get(run_id).cloned();
        handle.is_some_and(|handle| {
            handle
                .coordinator_abort
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|task| !task.is_finished())
        })
    }

    pub fn active_worker_count(&self, run_id: &str) -> usize {
        let map = self.handles.lock().unwrap();
        let Some(h) = map.get(run_id).cloned() else {
            return 0;
        };
        drop(map);
        let mut workers = h.workers.lock().unwrap();
        workers.retain(|_, w| !w.abort.is_finished());
        let coord_alive = h
            .coordinator_abort
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|a| !a.is_finished());
        workers.len() + usize::from(coord_alive && workers.is_empty())
    }

    pub fn abort_all_workers(&self, run_id: &str) -> Vec<String> {
        let map = self.handles.lock().unwrap();
        let Some(h) = map.get(run_id).cloned() else {
            return Vec::new();
        };
        drop(map);
        if let Some(coord) = h.coordinator_abort.lock().unwrap().take() {
            coord.abort();
        }
        self.abort_workers(run_id)
    }

    pub fn abort_workers(&self, run_id: &str) -> Vec<String> {
        let map = self.handles.lock().unwrap();
        let Some(h) = map.get(run_id).cloned() else {
            return Vec::new();
        };
        drop(map);
        let mut workers = h.workers.lock().unwrap();
        let mut thread_ids = Vec::new();
        for (_, w) in workers.drain() {
            w.abort.abort();
            thread_ids.push(w.thread_id);
        }
        thread_ids
    }

    pub fn remove_run(&self, run_id: &str) -> Vec<String> {
        let thread_ids = self.abort_all_workers(run_id);
        self.handles.lock().unwrap().remove(run_id);
        self.target_restarters.lock().unwrap().remove(run_id);
        thread_ids
    }

    pub async fn settle_for_delete(&self, handle: &Arc<RunHandle>) -> Result<(), String> {
        handle.deleting.store(true, Ordering::SeqCst);
        let mut followups = Vec::new();
        if let Some(task) = handle.followup_abort.lock().unwrap().take() {
            task.abort();
            followups.push(task);
        }
        if !wait_for_tasks(&followups, main_runtime_cancel_timeout()).await {
            return Err("Followup cancellation is not settled".into());
        }
        let coordinators = handle.coordinator_tasks.lock().unwrap().clone();
        if !wait_for_tasks(&coordinators, main_runtime_cancel_timeout()).await {
            for task in &coordinators {
                task.abort();
            }
            if !wait_for_tasks(&coordinators, coordinator_cancel_timeout()).await {
                return Err("Coordinator exit is unconfirmed".to_owned());
            }
        }
        let workers = handle.worker_tasks.lock().unwrap().clone();
        for task in &workers {
            task.abort();
        }
        if !wait_for_tasks(&workers, main_runtime_cancel_timeout()).await {
            return Err("Worker exit is unconfirmed".to_owned());
        }
        Ok(())
    }

    pub fn next_artifact_seq(&self, run_id: &str) -> u64 {
        let mut map = self.handles.lock().unwrap();
        let h = map
            .entry(run_id.to_owned())
            .or_insert_with(|| Arc::new(RunHandle::default()))
            .clone();
        drop(map);
        h.artifact_seq.fetch_add(1, Ordering::SeqCst) + 1
    }
}

fn source_cancel_timeout(name: &str, default: f64) -> Duration {
    let seconds = std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite())
        .unwrap_or(default)
        .max(0.01);
    Duration::from_secs_f64(seconds)
}

fn main_runtime_cancel_timeout() -> Duration {
    source_cancel_timeout("MUTEKI_MAIN_RUNTIME_CANCEL_TIMEOUT", 15.0)
}

fn coordinator_cancel_timeout() -> Duration {
    source_cancel_timeout("MUTEKI_STANDBY_CANCEL_TIMEOUT", 2.0)
}

async fn wait_for_tasks(tasks: &[tokio::task::AbortHandle], timeout: Duration) -> bool {
    tokio::time::timeout(timeout, async {
        while tasks.iter().any(|task| !task.is_finished()) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .is_ok()
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .merge(execution_guard::router())
        .route("/api/runs", post(create_run))
        .route("/api/dispatch/parse", post(dispatch_parse))
        .route("/api/runs/dispatch-parse", post(dispatch_parse))
        .route("/api/runs/{id}", delete(delete_run))
        .route("/api/runs/{id}/start", post(start_run))
        .route("/api/runs/{id}/open", post(open_run))
        .route("/api/runs/{id}/resolve", post(resolve_run))
        .route("/api/runs/{id}/progress", post(request_progress))
        .route("/api/runs/{id}/control", post(control_run))
        .route("/api/runs/{id}/hitl", post(control_run))
        .route(
            "/api/runs/{id}/control/{command_id}",
            get(get_control_command),
        )
        .route(
            "/api/runs/{id}/workers",
            post(spawn_worker_route).delete(kill_worker_route),
        )
        .route(
            "/api/runs/{id}/uploads",
            post(upload_files).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/runs/{id}/followups",
            post(followup::submit).get(followup::list),
        )
        .route("/api/runs/{id}/followups/{job}", delete(followup::cancel))
        .route(
            "/api/runs/{id}/followups/{job}/artifact",
            get(followup::get_artifact),
        )
        .route("/api/runs/{id}/writeup", get(followup::writeup))
        .route(
            "/api/reports",
            get(reports::list_all).post(reports::create_generic),
        )
        .route(
            "/api/reports/settings",
            get(reports::get_settings).put(reports::put_settings),
        )
        .route("/api/reports/candidates", get(reports::candidates))
        .route("/api/reports/import", post(reports::import))
        .route("/api/reports/batch", post(reports::batch))
        .route("/api/reports/{id}/generate", post(reports::generate))
        .route(
            "/api/reports/{id}/generation",
            delete(reports::delete_generation),
        )
        .route(
            "/api/reports/{id}/pending-update",
            post(reports::pending_update),
        )
        .route(
            "/api/reports/{id}",
            get(reports::get_one_generic)
                .put(reports::update_generic)
                .delete(reports::remove_generic),
        )
        .route("/api/report-folders", post(reports::create_folder))
        .route(
            "/api/report-folders/{id}/move-up",
            post(reports::move_folder_up),
        )
        .route(
            "/api/report-folders/{id}/delete-all",
            post(reports::delete_folder_all),
        )
        .route(
            "/api/report-folders/{id}",
            put(reports::update_folder).delete(reports::delete_folder),
        )
        .route(
            "/api/runs/{id}/reports",
            get(reports::list_for_run).post(reports::create),
        )
        .route(
            "/api/runs/{id}/reports/{report}",
            get(reports::get_one)
                .put(reports::update)
                .delete(reports::remove),
        )
        .route("/api/runs/{id}/credentials", get(run_credentials))
        .route("/api/runs/{id}/terminal", get(run_terminal_ws))
        .route("/api/runs/local-pi/catalog", get(local_pi_catalog_route))
        .route(
            "/api/runs/local-pi/settings",
            get(local_pi_get_settings_route).put(local_pi_put_settings_route),
        )
        .route("/api/runs/local-pi/probe", post(local_pi_probe_route))
        .route(
            "/api/runs/local-pi/providers/models",
            post(local_pi_provider_models_route),
        )
        .route(
            "/api/runs/{id}/local-pi/inspector",
            get(local_pi_inspector_route),
        )
        .route("/api/settings/run-retention", get(get_retention))
        .route("/api/settings/run-retention/{mode}", put(put_retention))
        .route("/api/pentest/reports", get(pentest::list_hub))
        .route(
            "/api/runs/{id}/pentest-report/regenerate",
            post(pentest::regenerate),
        )
        .route(
            "/api/runs/{id}/pentest-report.zip",
            get(pentest::get_bundle),
        )
        .route("/api/src/tasks", post(src_api::create_task))
        .merge(competition_api::router())
        .merge(settings::router())
}

pub(crate) async fn local_pi_catalog_route(State(state): State<Arc<AppState>>) -> Response {
    local_pi::build_full_local_pi_catalog(&state)
        .await
        .map(Json)
        .into_response()
}

pub(crate) async fn local_pi_get_settings_route(State(state): State<Arc<AppState>>) -> Response {
    local_pi::load_local_pi_settings(&state)
        .map(|settings| Json(json!({"ok": true, "settings": settings})))
        .into_response()
}

pub(crate) async fn local_pi_put_settings_route(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    match local_pi::save_local_pi_settings(&state, &body) {
        Ok(settings) => Json(json!({
            "ok": true,
            "settings": settings,
        }))
        .into_response(),
        Err(error) => error.into_response(),
    }
}

pub(crate) async fn local_pi_provider_models_route(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    Json(local_pi::fetch_local_pi_provider_models(&state, &body).await).into_response()
}

pub(crate) async fn local_pi_probe_route(
    State(state): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Response {
    let req = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let probe = match local_pi::run_local_pi_compatibility_probe(&state, &req).await {
        Ok(probe) => probe,
        Err(error) => return error.into_response(),
    };
    Json(json!({
        "ok": probe.get("ok").and_then(Value::as_bool).unwrap_or(false),
        "probe": probe,
    }))
    .into_response()
}

async fn local_pi_inspector_route(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Response {
    let Some(run_doc) = state.store.get("runs", &run_id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": format!("Run {run_id} not found")})),
        )
            .into_response();
    };
    let snapshot = local_pi::build_inspector_snapshot(&state, &run_id, &run_doc);
    Json(json!({
        "ok": true,
        "inspector": snapshot,
    }))
    .into_response()
}

pub fn now_secs() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

pub fn encode_run_id(run_id: &str) -> String {
    let mut out = String::with_capacity(run_id.len());
    for b in run_id.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        "run-default".to_owned()
    } else {
        out
    }
}

pub fn session_dir(state: &AppState, run_id: &str) -> PathBuf {
    native_chat::state_dir(state)
        .join("sessions")
        .join(encode_run_id(run_id))
}

pub fn control_dir(state: &AppState, run_id: &str) -> PathBuf {
    native_chat::state_dir(state)
        .join("control")
        .join(encode_run_id(run_id))
}

pub fn is_local_run(state: &AppState, run_id: &str) -> bool {
    if state.runs.active_worker_count(run_id) > 0 {
        return true;
    }
    if session_dir(state, run_id).join("events.jsonl").is_file() {
        return true;
    }
    if let Ok(Some(run)) = state.store.get("runs", run_id) {
        return run.get("history_event_file").is_none_or(Value::is_null);
    }
    false
}

#[derive(Default, Clone, Copy)]
pub(crate) struct PermissiveU64(pub(crate) Option<u64>);

impl<'de> serde::Deserialize<'de> for PermissiveU64 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PermissiveU64;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(PermissiveU64(Some(value)))
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(PermissiveU64(u64::try_from(value).ok()))
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(PermissiveU64(None))
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(PermissiveU64(None))
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(PermissiveU64(None))
            }
            fn visit_string<E: serde::de::Error>(self, _: String) -> Result<Self::Value, E> {
                Ok(PermissiveU64(None))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(PermissiveU64(None))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(PermissiveU64(None))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(PermissiveU64(None))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(PermissiveU64(None))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

struct PayloadFactSeq(Option<u64>);

impl<'de> serde::Deserialize<'de> for PayloadFactSeq {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PayloadFactSeq;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(PayloadFactSeq(None))
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(PayloadFactSeq(None))
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(PayloadFactSeq(None))
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(PayloadFactSeq(None))
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(PayloadFactSeq(None))
            }
            fn visit_string<E: serde::de::Error>(self, _: String) -> Result<Self::Value, E> {
                Ok(PayloadFactSeq(None))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(PayloadFactSeq(None))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(PayloadFactSeq(None))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(PayloadFactSeq(None))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut fact_seq = None;
                while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                    if key == "fact_seq" {
                        fact_seq = map.next_value::<PermissiveU64>()?.0;
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(PayloadFactSeq(fact_seq))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Default, Clone, Copy)]
struct ClockEvent {
    seq: Option<u64>,
    fact_seq: Option<u64>,
}

impl<'de> serde::Deserialize<'de> for ClockEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ClockEvent;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut seq = None;
                let mut fact_seq = None;
                while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                    match key.as_ref() {
                        "seq" => seq = map.next_value::<PermissiveU64>()?.0,
                        "payload" => fact_seq = map.next_value::<PayloadFactSeq>()?.0,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(ClockEvent { seq, fact_seq })
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

pub(crate) fn persisted_events(state: &AppState, run_id: &str) -> Result<Vec<Value>, String> {
    persisted_event_iter(state, run_id).map(Iterator::collect)
}

pub(crate) fn persisted_event_iter(
    state: &AppState,
    run_id: &str,
) -> Result<impl Iterator<Item = Value> + use<>, String> {
    Ok(persisted_event_scan(state, run_id, 0)?.filter_map(|(_, event)| event))
}

pub(crate) fn persisted_event_scan(
    state: &AppState,
    run_id: &str,
    after: u64,
) -> Result<impl Iterator<Item = (u64, Option<Value>)> + use<>, String> {
    let historical = state
        .store
        .get("runs", run_id)
        .map_err(|error| error.to_string())?
        .and_then(|run| run["history_event_file"].as_str().map(str::to_owned))
        .is_some();
    let events = if historical {
        crate::history_api::events(state, run_id).map_err(|error| error.to_string())?
    } else {
        Vec::new()
    };
    let offset = events
        .last()
        .and_then(|event| event["seq"].as_u64())
        .unwrap_or(0);
    let local = open_jsonl_scanner(
        state,
        run_id,
        after,
        if historical { Some(offset) } else { None },
    );
    let stream = events
        .into_iter()
        .map(move |event| {
            let seq = event["seq"].as_u64().unwrap_or(0);
            if seq <= after {
                (seq, None)
            } else {
                (seq, Some(event))
            }
        })
        .chain(local);
    let stored = state
        .store
        .list_for_run_ids("run_followup_events", &HashSet::from([run_id.to_owned()]))
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|e| e["run_id"] == run_id && e["seq"].as_u64().unwrap_or(0) > after)
        .collect::<Vec<_>>();
    let stream: Box<dyn Iterator<Item = (u64, Option<Value>)> + Send> = if stored.is_empty() {
        Box::new(stream)
    } else {
        let mut rows = stream.collect::<std::collections::BTreeMap<_, _>>();
        for event in stored {
            rows.entry(event["seq"].as_u64().unwrap_or(0))
                .or_insert(Some(event));
        }
        Box::new(rows.into_iter())
    };
    Ok(stream)
}

fn read_jsonl_events(state: &AppState, run_id: &str) -> Vec<Value> {
    read_jsonl_event_iter(state, run_id).collect()
}

fn event_clocks(events: impl Iterator<Item = Value>) -> (u64, u64) {
    events.fold((0, 0), |(seq, fact_seq), event| {
        (
            seq.max(event["seq"].as_u64().unwrap_or(0)),
            fact_seq.max(event["payload"]["fact_seq"].as_u64().unwrap_or(0)),
        )
    })
}

fn read_jsonl_clocks_cached(
    state: &AppState,
    run_id: &str,
    cursor_slot: &Mutex<ClockCursor>,
) -> (u64, u64) {
    let path = session_dir(state, run_id).join("events.jsonl");
    let Ok(mut file) = fs::File::open(path) else {
        *cursor_slot.lock().unwrap() = ClockCursor::default();
        return (0, 0);
    };
    let Ok(meta) = file.metadata() else {
        return (0, 0);
    };
    let observed_len = meta.len();
    #[cfg(unix)]
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    let prev = *cursor_slot.lock().unwrap();
    #[cfg(unix)]
    let reusable = prev.dev == dev && prev.ino == ino && prev.offset <= observed_len;
    #[cfg(not(unix))]
    let reusable = prev.offset <= observed_len;
    if reusable && prev.offset == observed_len {
        return (prev.seq, prev.fact_seq);
    }
    let (mut start_offset, mut seq, mut fact_seq) = if reusable {
        (prev.offset, prev.seq, prev.fact_seq)
    } else {
        (0, 0, 0)
    };
    if start_offset > 0 && file.seek(SeekFrom::Start(start_offset)).is_err() {
        if file.seek(SeekFrom::Start(0)).is_err() {
            return (seq, fact_seq);
        }
        start_offset = 0;
        seq = 0;
        fact_seq = 0;
    }
    let mut reader = BufReader::new(file.take(observed_len.saturating_sub(start_offset)));
    let mut line = String::new();
    let mut scanned_offset = start_offset;
    let mut committed_offset = start_offset;
    let mut committed_seq = seq;
    let mut committed_fact_seq = fact_seq;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(bytes_read) => {
                scanned_offset += bytes_read as u64;
                let terminated = line.ends_with('\n');
                if terminated {
                    line.pop();
                    if line.ends_with('\r') {
                        line.pop();
                    }
                }
                if let Ok(ev) = serde_json::from_str::<ClockEvent>(&line) {
                    seq = std::cmp::max(seq + 1, ev.seq.unwrap_or(0));
                    fact_seq = fact_seq.max(ev.fact_seq.unwrap_or(0));
                }
                if terminated {
                    committed_offset = scanned_offset;
                    committed_seq = seq;
                    committed_fact_seq = fact_seq;
                } else {
                    break;
                }
            }
        }
    }
    {
        let mut slot = cursor_slot.lock().unwrap();
        #[cfg(unix)]
        let advance = slot.dev != dev || slot.ino != ino || committed_offset >= slot.offset;
        #[cfg(not(unix))]
        let advance = committed_offset >= slot.offset;
        if advance {
            *slot = ClockCursor {
                #[cfg(unix)]
                dev,
                #[cfg(unix)]
                ino,
                offset: committed_offset,
                seq: committed_seq,
                fact_seq: committed_fact_seq,
            };
        }
    }
    (seq, fact_seq)
}

struct JsonlEventScanner {
    reader: Option<BufReader<std::io::Take<fs::File>>>,
    line: String,
    after: u64,
    reindex: bool,
    seq: u64,
    fact_seq: u64,
    emitted_any: bool,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    scanned_offset: u64,
    committed_offset: u64,
    committed_seq: u64,
    committed_fact_seq: u64,
    cursor_handle: Option<Arc<RunHandle>>,
}

impl Iterator for JsonlEventScanner {
    type Item = (u64, Option<Value>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let reader = self.reader.as_mut()?;
            self.line.clear();
            match reader.read_line(&mut self.line) {
                Ok(0) | Err(_) => {
                    self.reader = None;
                    if let Some(handle) = self.cursor_handle.take() {
                        let mut slot = handle.clock_cursor.lock().unwrap();
                        #[cfg(unix)]
                        let same_or_fresh = (slot.offset == 0 && slot.seq == 0)
                            || (slot.dev == self.dev && slot.ino == self.ino);
                        #[cfg(not(unix))]
                        let same_or_fresh = true;
                        if same_or_fresh && self.committed_offset > slot.offset {
                            *slot = ClockCursor {
                                #[cfg(unix)]
                                dev: self.dev,
                                #[cfg(unix)]
                                ino: self.ino,
                                offset: self.committed_offset,
                                seq: self.committed_seq,
                                fact_seq: self.committed_fact_seq,
                            };
                        }
                    }
                    if !self.emitted_any && self.seq > 0 && !self.reindex {
                        self.emitted_any = true;
                        return Some((self.seq, None));
                    }
                    return None;
                }
                Ok(bytes_read) => {
                    self.scanned_offset += bytes_read as u64;
                    let terminated = self.line.ends_with('\n');
                    if terminated {
                        self.line.pop();
                        if self.line.ends_with('\r') {
                            self.line.pop();
                        }
                    }
                    if self.seq < self.after {
                        let Ok(clock) = serde_json::from_str::<ClockEvent>(&self.line) else {
                            if terminated {
                                self.committed_offset = self.scanned_offset;
                            }
                            continue;
                        };
                        let next_seq = if self.reindex {
                            self.seq + 1
                        } else {
                            std::cmp::max(self.seq + 1, clock.seq.unwrap_or(0))
                        };
                        if next_seq <= self.after {
                            self.seq = next_seq;
                            self.fact_seq = self.fact_seq.max(clock.fact_seq.unwrap_or(0));
                            if terminated {
                                self.committed_offset = self.scanned_offset;
                                self.committed_seq = self.seq;
                                self.committed_fact_seq = self.fact_seq;
                            }
                            self.emitted_any = true;
                            return Some((self.seq, None));
                        }
                    }
                    let Ok(mut event) = serde_json::from_str::<Value>(&self.line) else {
                        if terminated {
                            self.committed_offset = self.scanned_offset;
                        }
                        continue;
                    };
                    if !event.is_object() {
                        if terminated {
                            self.committed_offset = self.scanned_offset;
                        }
                        continue;
                    }
                    self.seq = if self.reindex {
                        self.seq + 1
                    } else {
                        std::cmp::max(
                            self.seq + 1,
                            event.get("seq").and_then(Value::as_u64).unwrap_or(0),
                        )
                    };
                    self.fact_seq = self
                        .fact_seq
                        .max(event["payload"]["fact_seq"].as_u64().unwrap_or(0));
                    event["seq"] = json!(self.seq);
                    if terminated {
                        self.committed_offset = self.scanned_offset;
                        self.committed_seq = self.seq;
                        self.committed_fact_seq = self.fact_seq;
                    }
                    self.emitted_any = true;
                    return Some((self.seq, Some(event)));
                }
            }
        }
    }
}

fn open_jsonl_scanner(
    state: &AppState,
    run_id: &str,
    after: u64,
    reindex_from: Option<u64>,
) -> JsonlEventScanner {
    let path = session_dir(state, run_id).join("events.jsonl");
    let Ok(mut file) = fs::File::open(path) else {
        return JsonlEventScanner {
            reader: None,
            line: String::new(),
            after,
            reindex: reindex_from.is_some(),
            seq: reindex_from.unwrap_or(0),
            fact_seq: 0,
            emitted_any: false,
            #[cfg(unix)]
            dev: 0,
            #[cfg(unix)]
            ino: 0,
            scanned_offset: 0,
            committed_offset: 0,
            committed_seq: 0,
            committed_fact_seq: 0,
            cursor_handle: None,
        };
    };
    let meta = file.metadata().ok();
    let observed_len = meta.as_ref().map(fs::Metadata::len).unwrap_or(0);
    #[cfg(unix)]
    let (dev, ino) = meta
        .as_ref()
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            (m.dev(), m.ino())
        })
        .unwrap_or((0, 0));
    let cursor_handle = if after > 0 && reindex_from.is_none() {
        state.runs.existing_handle(run_id)
    } else {
        None
    };
    let mut start_offset = 0u64;
    let mut seq = reindex_from.unwrap_or(0);
    let mut fact_seq = 0u64;
    if let Some(handle) = cursor_handle.as_ref() {
        let prev = *handle.clock_cursor.lock().unwrap();
        #[cfg(unix)]
        let reusable = prev.offset > 0
            && prev.dev == dev
            && prev.ino == ino
            && prev.offset <= observed_len
            && prev.seq <= after;
        #[cfg(not(unix))]
        let reusable = prev.offset > 0 && prev.offset <= observed_len && prev.seq <= after;
        if reusable && file.seek(SeekFrom::Start(prev.offset)).is_ok() {
            start_offset = prev.offset;
            seq = prev.seq;
            fact_seq = prev.fact_seq;
        } else {
            let _ = file.seek(SeekFrom::Start(0));
        }
    }
    JsonlEventScanner {
        reader: Some(BufReader::new(
            file.take(observed_len.saturating_sub(start_offset)),
        )),
        line: String::new(),
        after,
        reindex: reindex_from.is_some(),
        seq,
        fact_seq,
        emitted_any: false,
        #[cfg(unix)]
        dev,
        #[cfg(unix)]
        ino,
        scanned_offset: start_offset,
        committed_offset: start_offset,
        committed_seq: seq,
        committed_fact_seq: fact_seq,
        cursor_handle,
    }
}

pub(crate) fn read_jsonl_event_scan(
    state: &AppState,
    run_id: &str,
    after: u64,
) -> impl Iterator<Item = (u64, Option<Value>)> + use<> {
    open_jsonl_scanner(state, run_id, after, None)
}

pub(crate) fn read_jsonl_event_iter(
    state: &AppState,
    run_id: &str,
) -> impl Iterator<Item = Value> + use<> {
    open_jsonl_scanner(state, run_id, 0, None).filter_map(|(_, event)| event)
}

pub(crate) async fn last_persisted_activity(
    state: &Arc<AppState>,
    run_id: &str,
) -> Result<f64, String> {
    let state = state.clone();
    let run_id = run_id.to_owned();
    tokio::task::spawn_blocking(move || {
        Ok(persisted_event_iter(&state, &run_id)?
            .filter_map(|event| event["ts"].as_f64())
            .filter(|ts| ts.is_finite() && *ts > 0.)
            .fold(0f64, f64::max))
    })
    .await
    .map_err(|error| error.to_string())?
}

pub fn emit_event(
    state: &AppState,
    run_id: &str,
    solver_id: Option<&str>,
    event_type: &str,
    payload: Value,
) -> Value {
    emit_event_with_metadata(state, run_id, solver_id, event_type, payload, None)
}

pub fn emit_event_with_metadata(
    state: &AppState,
    run_id: &str,
    solver_id: Option<&str>,
    event_type: &str,
    mut payload: Value,
    metadata: Option<&Value>,
) -> Value {
    let Ok(Some(run_doc)) = state.store.get("runs", run_id) else {
        return json!({"event_type":event_type,"run_id":run_id,"persisted":false,"fenced":"run_unavailable"});
    };
    let handle = state.runs.handle(state, run_id);
    if handle.deleting.load(Ordering::SeqCst) {
        return json!({"event_type":event_type,"run_id":run_id,"persisted":false,"fenced":"run_unavailable"});
    }
    if let Some(solver_id) =
        solver_id.filter(|id| id.starts_with("cli-") && *id != local_pi::LOCAL_PI_SOLVER_ID)
    {
        let generation = worker::worker_execution_generation(solver_id);
        if run_doc["execution_generation"]
            .as_u64()
            .is_some_and(|active| generation < active)
        {
            return json!({"event_type":event_type,"run_id":run_id,
                "solver_id":solver_id,"fenced":"stale_execution_generation"});
        }
    }
    if event_type == "cost.update"
        && payload["scope"] != "auxiliary"
        && payload["accounting_source"] != "run_usage_ledger"
        && let (Some(solver), Some(usd)) = (solver_id, payload["usd"].as_f64())
        && usd.is_finite()
        && usd >= 0.0
    {
        let mut cost = handle.cost_usage.lock().unwrap();
        let previous = cost.by_solver.insert(solver.to_owned(), usd).unwrap_or(0.0);
        cost.total_usd += if usd >= previous { usd - previous } else { usd };
    }
    if event_type == "tool.result" {
        if let Err(error) = evidence::attach(state, run_id, solver_id, &mut payload) {
            return json!({"event_type":event_type,"run_id":run_id,"solver_id":solver_id,"persisted":false,"error":error});
        }
    }
    let file_guard = handle.event_file_lock.lock().unwrap();
    let seq = handle.seq.fetch_add(1, Ordering::SeqCst) + 1;
    let ts = now_secs();
    let mut ev = json!({
        "event_type": event_type,
        "run_id": run_id,
        "seq": seq,
        "ts": ts,
        "payload": payload,
    });
    if let Some(metadata) = metadata {
        for key in [
            "producer",
            "actor_id",
            "command_id",
            "causation_id",
            "correlation_id",
            "idempotency_key",
        ] {
            if let Some(value) = metadata.get(key) {
                ev[key] = value.clone();
            }
        }
    }
    if let Some(sid) = solver_id {
        ev["solver_id"] = json!(sid);
    }

    let is_auxiliary_event = event_type.starts_with("followup.")
        || (event_type == "cost.update" && ev["payload"]["scope"] == "auxiliary");
    let sdir = session_dir(state, run_id);
    if !is_auxiliary_event {
        let _ = fs::create_dir_all(&sdir);
    }
    let jsonl_path = sdir.join("events.jsonl");
    let mut persisted = if (!is_auxiliary_event || sdir.is_dir())
        && let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(jsonl_path)
    {
        let line = format!("{ev}\n");
        file.write_all(line.as_bytes()).is_ok()
    } else {
        false
    };
    if !persisted && is_auxiliary_event {
        match state
            .store
            .put("run_followup_events", &format!("{run_id}:{seq}"), &ev)
        {
            Ok(()) => persisted = true,
            Err(error) => tracing::error!(%error,run_id,seq,"Followup event persistence failed"),
        }
    }
    if persisted {
        if let Err(error) = src_api::run_projection::consume(state, run_id, &ev) {
            tracing::warn!(run_id, %error, "SRC Run event projection remains pending");
        }
    } else {
        tracing::error!(run_id, seq, "Run event could not be persisted");
    }
    drop(file_guard);

    if persisted {
        let _ = handle.bus.send(ev.clone());
        state.run_progress.observe(run_id, &ev);
    } else {
        ev["persisted"] = json!(false);
    }
    ev
}

pub fn project_run_summary(state: &AppState, mut run: Value) -> Value {
    let run_id = run["run_id"].as_str().unwrap_or("").to_owned();
    let active_workers = state.runs.active_worker_count(&run_id);
    let local_pi_turn_active = run["kind"].as_str() == Some("local_pi")
        && local_pi::has_live_session(&run_id)
        && run["local_pi_session"]["turn_active"].as_bool() == Some(true);
    let running =
        active_workers > 0 || local_pi_turn_active || run["status"].as_str() == Some("running");
    run["running"] = json!(running);
    if run.get("kind").is_none() || run["kind"].is_null() {
        run["kind"] = json!("swarm");
    }
    if run.get("operator_label").is_none() {
        run["operator_label"] = Value::Null;
    }
    if run.get("operator_label_updated_at").is_none() {
        run["operator_label_updated_at"] = Value::Null;
    }
    if run.get("solved").is_none() {
        run["solved"] = json!(false);
    }
    if run.get("flags").is_none() || !run["flags"].is_array() {
        let flags = if let Some(f) = run["flag"].as_str().filter(|s| !s.is_empty()) {
            vec![json!(f)]
        } else {
            Vec::new()
        };
        run["flags"] = json!(flags);
    }
    if run.get("expected_flags").is_none() {
        run["expected_flags"] = json!(1);
    }
    if run.get("multi_flag").is_none() {
        run["multi_flag"] = json!(false);
    }
    if run.get("platform_confirmation_required").is_none() {
        run["platform_confirmation_required"] = json!(false);
    }
    if run.get("pinned").is_none() {
        run["pinned"] = json!(false);
    }
    if run.get("archived").is_none() {
        run["archived"] = json!(false);
    }
    if run.get("mode").is_none() {
        run["mode"] = json!("ctf");
    }
    if run.get("category").is_none() {
        run["category"] = json!("misc");
    }
    if run.get("updated").is_none() {
        run["updated"] = run["ts"].clone();
    }
    if run["status"].as_str() == Some("finished") && run["solved"] != true {
        let (settled_solved, settled_reason) = terminal::settle_terminal_outcome(
            &run,
            false,
            run["outcome_reason"].as_str().unwrap_or("finished"),
        );
        if settled_solved {
            run["solved"] = json!(true);
            run["status"] = json!("solved");
            run["outcome_reason"] = json!(settled_reason);
        }
    }
    let is_terminal = matches!(
        run["status"].as_str(),
        Some("solved" | "finished" | "failed" | "stopped" | "cancelled" | "completed")
    );
    if is_terminal {
        run["finished"] = json!(true);
    } else if run.get("finished").is_none() {
        run["finished"] = json!(false);
    }
    run
}

pub fn list_runs(state: &AppState, include_archived: bool) -> Result<Vec<Value>, String> {
    let raw = state.store.list("runs").map_err(|e| e.to_string())?;
    let mut runs: Vec<Value> = raw
        .into_iter()
        .filter(|r| {
            let src_bound = r["src_task_id"].as_str().is_some_and(|s| !s.is_empty())
                || r["bound_to_src"] == true;
            if src_bound {
                return false;
            }
            include_archived || r["archived"] != true
        })
        .map(|r| project_run_summary(state, r))
        .collect();

    runs.sort_by(|a, b| {
        let a_run = a["running"].as_bool().unwrap_or(false);
        let b_run = b["running"].as_bool().unwrap_or(false);
        if a_run != b_run {
            return b_run.cmp(&a_run);
        }
        let a_upd = a["updated"]
            .as_f64()
            .or_else(|| a["ts"].as_f64())
            .unwrap_or(0.0);
        let b_upd = b["updated"]
            .as_f64()
            .or_else(|| b["ts"].as_f64())
            .unwrap_or(0.0);
        b_upd
            .partial_cmp(&a_upd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let a_seq = a["created_seq"].as_u64().unwrap_or(0);
                let b_seq = b["created_seq"].as_u64().unwrap_or(0);
                b_seq.cmp(&a_seq)
            })
    });
    Ok(runs)
}

fn next_run_seq(state: &AppState) -> u64 {
    let current = state
        .store
        .get("system", "run_counter")
        .ok()
        .flatten()
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| {
            state
                .store
                .list("runs")
                .unwrap_or_default()
                .iter()
                .filter_map(|r| {
                    r["run_id"]
                        .as_str()
                        .and_then(|s| s.strip_prefix("run-"))
                        .and_then(|n| n.parse::<u64>().ok())
                })
                .max()
                .unwrap_or(0)
        });
    let next = current + 1;
    let _ = state.store.put("system", "run_counter", &json!(next));
    next
}

pub fn ensure_run_record(state: &AppState, run_id: &str) -> Result<Value, String> {
    if let Some(existing) = state.store.get("runs", run_id).map_err(|e| e.to_string())? {
        return Ok(existing);
    }
    let doc = draft_run_record(state, run_id);
    state
        .store
        .put("runs", run_id, &doc)
        .map_err(|e| e.to_string())?;
    ensure_run_directory(state, run_id);
    Ok(doc)
}

fn next_run_order(state: &AppState) -> u64 {
    let max_order = state
        .store
        .list("runs")
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            r["order"].as_u64().or_else(|| {
                r["order"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .map(|v| v.floor() as u64)
            })
        })
        .max()
        .unwrap_or(0);
    max_order.saturating_add(1)
}

pub(crate) fn draft_run_record(state: &AppState, run_id: &str) -> Value {
    let seq = next_run_seq(state);
    let now = now_secs();
    let order = next_run_order(state);
    json!({
        "run_id": run_id,
        "name": run_id,
        "original_name": run_id,
        "custom_name": Value::Null,
        "category": "misc",
        "mode": "ctf",
        "target": "",
        "flag_format": "flag{...}",
        "description": "",
        "status": "draft",
        "running": false,
        "solved": false,
        "flag": Value::Null,
        "flags": [],
        "expected_flags": 1,
        "multi_flag": false,
        "platform_confirmation_required": false,
        "ts": now,
        "updated": now,
        "created_seq": seq,
        "execution_generation": 0,
        "control_generation": 0,
        "pinned": false,
        "pinned_at": Value::Null,
        "archived": false,
        "archived_at": Value::Null,
        "folder_id": Value::Null,
        "order": order,
        "attachments": [],
    })
}

pub(crate) fn ensure_run_directory(state: &AppState, run_id: &str) {
    let _ = fs::create_dir_all(session_dir(state, run_id).join("uploads"));
}

pub(crate) async fn create_run(
    State(state): State<Arc<AppState>>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let module = if body["mode"] == "pentest" {
        crate::business::Module::Pentest
    } else {
        crate::business::Module::Ctf
    };
    if let Err(error) = crate::business::require(&state, module) {
        return error.into_response();
    }
    let seq = next_run_seq(&state);
    let run_id = body["run_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("run-{seq:04}"));

    match ensure_run_record(&state, &run_id) {
        Ok(mut run) => {
            if body["mode"] == "pentest" && run["started"] != true {
                run["mode"] = json!("pentest");
                if let Err(error) = state.store.put("runs", &run_id, &run) {
                    return (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":{"code":"storage.unavailable","message":error.to_string()}}))).into_response();
                }
            }
            let cmd_id = body["command_id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4().simple()));
            let receipt = json!({
                "schema_version": 1,
                "command_id": cmd_id,
                "receipt_id": format!("rcpt-{}", uuid::Uuid::new_v4().simple()),
                "state": "completed",
                "run_id": run_id,
                "aggregate": {"type": "run", "id": run_id},
                "output": {"run_id": run_id},
            });
            Json(json!({"run_id": run_id, "receipt": receipt})).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": err})),
        )
            .into_response(),
    }
}

pub fn prepare_dispatch_contract(challenge: &Value) -> Result<(Value, Value), String> {
    prepare_dispatch_contract_with_authorization(challenge, None)
}

fn prepare_dispatch_contract_with_authorization(
    challenge: &Value,
    authorized_pentest_contract: Option<&Value>,
) -> Result<(Value, Value), String> {
    let obj = challenge
        .as_object()
        .ok_or_else(|| "challenge must be a JSON object".to_owned())?;

    let mode = obj
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("ctf")
        .trim()
        .to_ascii_lowercase();
    if mode != "pentest"
        && obj
            .get("production_guard")
            .is_some_and(|v| v["enabled"] != false)
    {
        return Err("Production guard requires Pentest mode".into());
    }
    if mode != "ctf" && mode != "pentest" {
        return Err(format!("Unsupported challenge mode: {mode}"));
    }

    let description = obj
        .get("description")
        .or_else(|| obj.get("prompt"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();

    let raw_name = obj
        .get("name")
        .or_else(|| obj.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    let attachments: Vec<String> = obj
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            v.as_str()
                .or_else(|| v["path"].as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .collect();

    for att in &attachments {
        if !std::path::Path::new(att).exists() {
            return Err(format!("Attachment path does not exist: {att}"));
        }
    }

    let inferred_name = if !raw_name.is_empty() {
        raw_name.to_owned()
    } else if let Some(first_att) = attachments.first() {
        std::path::Path::new(first_att)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("challenge")
            .to_owned()
    } else if !description.is_empty() {
        description
            .lines()
            .next()
            .unwrap_or("challenge")
            .chars()
            .take(40)
            .collect::<String>()
            .trim()
            .to_owned()
    } else {
        "untitled-challenge".to_owned()
    };

    let raw_cat = obj
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let category = if !raw_cat.is_empty() && raw_cat != "auto" {
        raw_cat
    } else {
        infer_category(&inferred_name, &description, &attachments)
    };

    let wrapper_ff = obj
        .get("flag_format_wrapper")
        .or_else(|| obj.get("flag_format_hint"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let raw_ff = obj
        .get("flag_format")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .or(if !wrapper_ff.is_empty() {
            Some(wrapper_ff)
        } else {
            None
        })
        .unwrap_or("flag{...}")
        .trim();
    let flag_format = match raw_ff {
        "" | "brace" => "flag{...}".to_owned(),
        "token" => "token".to_owned(),
        "custom" => obj
            .get("flag_prefix")
            .or_else(|| obj.get("flag_format_wrapper"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(|p| {
                let p = p.trim();
                if p.contains("...") {
                    p.to_owned()
                } else {
                    format!("{p}...")
                }
            })
            .unwrap_or_else(|| "flag{...}".to_owned()),
        other => other.to_owned(),
    };

    let target = obj
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();

    if description.is_empty() && target.is_empty() && attachments.is_empty() {
        return Err("Provide at least a description, target connection, or attachment".to_owned());
    }

    let expected_flags = obj
        .get("expected_flags")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let multi_flag = obj
        .get("multi_flag")
        .and_then(Value::as_bool)
        .unwrap_or(expected_flags > 1);
    let platform_confirmation_required = obj
        .get("platform_confirmation_required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let allow_operator_input = obj
        .get("allow_operator_input")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let mut connection = Map::new();
    if !target.is_empty() {
        if target.starts_with("http://") || target.starts_with("https://") {
            connection.insert("url".to_owned(), json!(target));
        } else if let Some((h, p)) = target.split_once(':')
            && let Ok(port) = p.trim().parse::<u16>()
        {
            connection.insert("host".to_owned(), json!(h.trim()));
            connection.insert("port".to_owned(), json!(port));
        } else {
            connection.insert("raw".to_owned(), json!(target));
        }
    }

    let mut task_contract = json!({
        "version": 1,
        "mode": mode,
        "title": inferred_name,
        "category": category,
        "connection": connection,
        "completion": {
            "kind": if mode == "pentest" { "goal" } else { "flag" },
            "flag_format": flag_format,
            "expected_flags": expected_flags,
            "multi_flag": multi_flag,
            "platform_confirmation_required": platform_confirmation_required,
            "goal": if mode == "pentest" { description.clone() } else { String::new() },
        },
        "interaction": {
            "allow_operator_input": allow_operator_input,
        },
        "attachments": attachments,
    });

    let pentest_contract = if mode == "pentest" {
        let contract = if let Some(contract) = authorized_pentest_contract {
            let mut contract = contract.clone();
            execution_guard::apply_contract(&mut contract, challenge.get("production_guard"))?;
            contract
        } else {
            pentest::compile_contract(challenge)?
        };
        task_contract["pentest"] = contract.clone();
        Some(contract)
    } else {
        None
    };
    let mut normalized_challenge = json!({
        "name": inferred_name,
        "points":obj.get("points").and_then(Value::as_f64).filter(|value|value.is_finite()).unwrap_or(0.).round_ties_even() as i64,
        "category": category,
        "description": description,
        "flag_format": flag_format,
        "target": target,
        "mode": mode,
        "expected_flags": expected_flags,
        "multi_flag": multi_flag,
        "platform_confirmation_required": platform_confirmation_required,
        "allow_operator_input": allow_operator_input,
        "attachments": attachments,
        "task_contract": task_contract,
    });
    if let Some(contract) = pentest_contract {
        normalized_challenge["pentest_contract"] = contract;
    }
    if let Some(src_task_id) = obj.get("src_task_id").and_then(Value::as_str) {
        normalized_challenge["src_task_id"] = json!(src_task_id);
    }

    Ok((
        json!({"name": inferred_name, "category": category}),
        normalized_challenge,
    ))
}

fn infer_category(name: &str, desc: &str, attachments: &[String]) -> String {
    let hay = format!(
        "{} {} {}",
        name.to_ascii_lowercase(),
        desc.to_ascii_lowercase(),
        attachments.join(" ").to_ascii_lowercase()
    );
    if hay.contains("pwn") || hay.contains("bof") || hay.contains("rop") || hay.contains("libc") {
        "pwn".to_owned()
    } else if hay.contains("rev")
        || hay.contains("reverse")
        || hay.contains("decompile")
        || hay.contains("flag_checker")
        || hay.contains(".py")
    {
        "rev".to_owned()
    } else if hay.contains("crypto")
        || hay.contains("rsa")
        || hay.contains("aes")
        || hay.contains("cipher")
    {
        "crypto".to_owned()
    } else if hay.contains("web")
        || hay.contains("http://")
        || hay.contains("https://")
        || hay.contains("sql")
        || hay.contains("xss")
    {
        "web".to_owned()
    } else if hay.contains("forensics") || hay.contains("pcap") || hay.contains("memory") {
        "forensics".to_owned()
    } else {
        "misc".to_owned()
    }
}

async fn dispatch_parse(Json(body): Json<Value>) -> Response {
    let challenge = match execution_guard::dispatch_challenge(
        &body,
        body.get("challenge").unwrap_or(&body).clone(),
    ) {
        Ok(value) => value,
        Err(detail) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":detail})),
            )
                .into_response();
        }
    };
    match prepare_dispatch_contract(&challenge) {
        Ok((parsed, norm)) => Json(json!({
            "parsed": parsed,
            "task_contract": norm["task_contract"],
        }))
        .into_response(),
        Err(detail) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": detail})),
        )
            .into_response(),
    }
}

fn execution_conflict(code: &str, detail: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({"code": code, "detail": detail})),
    )
        .into_response()
}

fn check_stop_pending(state: &AppState, run: &Value) -> Result<(), Response> {
    if let Some(id) = run["run_id"].as_str()
        && execution_guard::is_revoked(state, id, run)
            .map_err(|error| execution_conflict("guard.state_unavailable", &error))?
    {
        return Err(execution_conflict("guard.revoked", "Run egress is revoked"));
    }
    if runner_stop_recovery::pending(state, run).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": error})),
        )
            .into_response()
    })? {
        return Err(execution_conflict(
            "run.stop_pending",
            "Run is awaiting stop confirmation",
        ));
    }
    Ok(())
}

fn check_new_execution(state: &AppState, run_id: &str, run: &Value) -> Result<(), Response> {
    check_stop_pending(state, run)?;
    if run["running"] == true
        || state.runs.active_worker_count(run_id) > 0
        || local_pi::has_live_session(run_id)
    {
        return Err(execution_conflict(
            "run.already_active",
            "Run is still active",
        ));
    }
    Ok(())
}

fn resume_execution_command(
    state: &Arc<AppState>,
    run_id: &str,
    command_id: &str,
    mut marker: Value,
) -> Response {
    let response = marker["response"].clone();
    let status = StatusCode::from_u16(marker["status"].as_u64().unwrap_or(200) as u16)
        .unwrap_or(StatusCode::OK);
    if status != StatusCode::OK {
        return (status, Json(response)).into_response();
    }
    let generation = marker["generation"].as_u64().unwrap_or(0);
    let Some(run) = state.store.get("runs", run_id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail":"Run no longer exists"})),
        )
            .into_response();
    };
    if run["execution_generation"].as_u64() != Some(generation) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"detail":"Run execution generation changed after this command"})),
        )
            .into_response();
    }
    match state.runs.claim_execution_owner(state, run_id) {
        Ok(true) => {}
        Ok(false) => {
            return execution_conflict(
                "run.resolve.owner_conflict",
                "Run is owned by another service process",
            );
        }
        Err(e) => return followup::error(StatusCode::INTERNAL_SERVER_ERROR, "storage.failed", &e),
    }
    if marker["action"] == "resolve" {
        if let Err(error) = resolve_guidance(state, run_id, &marker) {
            return followup::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "run.resolve.guidance_failed",
                &error,
            );
        }
    }
    let stop_pending = match runner_stop_recovery::pending(state, &run) {
        Ok(pending) => pending,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error})),
            )
                .into_response();
        }
    };
    if stop_pending
        || (marker["kind"] != "idle" && run["running"] != true)
        || state.runs.active_worker_count(run_id) > 0
        || local_pi::has_live_session(run_id)
    {
        if marker["phase"] == "dispatched" {
            return (status, Json(response)).into_response();
        }
        return execution_conflict(
            "run.replay_blocked",
            "Run cannot replay this execution plan in its current state",
        );
    }
    let action = marker["action"].as_str().unwrap_or("");
    let start_type = if action == "resolve" {
        "run.reopened"
    } else {
        "run.started"
    };
    let specs: Vec<worker::WorkerSpec> = match serde_json::from_value(marker["specs"].clone()) {
        Ok(specs) => specs,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"detail":format!("Execution plan unavailable: {error}")})),
            )
                .into_response();
        }
    };
    if execution_guard::protected(&run) || execution_guard::protected(&marker["challenge"]) {
        let saved_cfg = launch_config::settings(state, run_id).ok();
        if let Some(response) = execution_guard::preflight(state, &specs, &[], saved_cfg.as_ref()) {
            return response;
        }
    }
    let mut external_requested = false;
    for spec in &specs {
        let key = worker::external_start_key(command_id, &spec.solver_id);
        match state.store.get("run_execution_commands", &key) {
            Ok(Some(_)) => external_requested = true,
            Ok(None) => {}
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail":error.to_string()})),
                )
                    .into_response();
            }
        }
    }
    let mut anchor_found = false;
    let mut actual_output = false;
    let mut preparing_found = false;
    for event in read_jsonl_event_iter(state, run_id) {
        if event["event_type"] == "run.preparing"
            && event["payload"]["execution_generation"] == generation
        {
            preparing_found = true;
        }
        if !anchor_found {
            if event["event_type"] == start_type
                && event["payload"]["execution_generation"] == generation
            {
                anchor_found = true;
            }
        } else if specs
            .iter()
            .any(|spec| event["solver_id"] == spec.solver_id)
            && matches!(
                event["event_type"].as_str(),
                Some(
                    "text.delta"
                        | "reasoning.delta"
                        | "tool.start"
                        | "tool.result"
                        | "terminal.output"
                        | "cost.update"
                )
            )
        {
            actual_output = true;
            break;
        }
    }
    if marker["kind"] == "idle" && action == "start" && !anchor_found {
        if !preparing_found {
            emit_event(
                state,
                run_id,
                None,
                "run.preparing",
                json!({
                "challenge":marker["challenge"],"name_autogen":marker["name_autogen"],
                "kind":marker["kind"],"driver_kind":marker["kind"],
                "execution_generation":generation,"control_generation":marker["control_generation"]}),
            );
        }
        emit_event(
            state,
            run_id,
            None,
            "run.started",
            json!({
            "challenge":marker["challenge"],"name_autogen":marker["name_autogen"],
            "kind":marker["kind"],"driver_kind":marker["kind"],
            "execution_generation":generation,"control_generation":marker["control_generation"]}),
        );
    }
    if marker["kind"] == "idle" || actual_output {
        marker["phase"] = json!("dispatched");
        let _ = state
            .store
            .put("run_execution_commands", command_id, &marker);
        return (status, Json(response)).into_response();
    }
    if external_requested {
        return (StatusCode::CONFLICT, Json(json!({
            "error":{"code":"run.effect.outcome_unknown",
                "message":"Worker start was requested before the previous process stopped; no external result was recorded"}
        }))).into_response();
    }
    match state.runs.claim_execution_owner(state, run_id) {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"detail":"Run is owned by another service process"})),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error})),
            )
                .into_response();
        }
    }
    let challenge = marker["challenge"].clone();
    if action == "start" {
        if !preparing_found {
            emit_event(
                state,
                run_id,
                None,
                "run.preparing",
                json!({
                "challenge":challenge,"name_autogen":marker["name_autogen"],
                "kind":marker["kind"],"driver_kind":marker["kind"],
                "execution_generation":generation,"control_generation":marker["control_generation"]}),
            );
        }
        if !anchor_found {
            emit_event(
                state,
                run_id,
                None,
                "run.started",
                json!({
                "challenge":challenge,"name_autogen":marker["name_autogen"],
                "kind":marker["kind"],"driver_kind":marker["kind"],
                "execution_generation":generation,"control_generation":marker["control_generation"]}),
            );
        }
        if let Ok(graph) = graph::SharedGraph::open(state, run_id) {
            if let Err(detail) = graph.seed_origin_fact_checked(&challenge) {
                mark_run_finished(
                    state,
                    run_id,
                    None,
                    false,
                    "graph_origin_failed",
                    Some(&detail),
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail":detail})),
                )
                    .into_response();
            }
            if challenge["mode"] == "pentest"
                && !graph
                    .events()
                    .iter()
                    .any(|event| event["kind"] == "pentest_contract")
            {
                if let Err(detail) =
                    pentest::record_contract(state, run_id, &challenge["pentest_contract"])
                {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"detail":detail})),
                    )
                        .into_response();
                }
            }
        }
    } else if !anchor_found {
        emit_event(
            state,
            run_id,
            None,
            "run.reopened",
            json!({
            "reason":"resolve","challenge":challenge,
            "execution_generation":generation,"control_generation":marker["control_generation"]}),
        );
    }
    if marker["kind"].as_str() == Some("local_pi") {
        local_pi::spawn_local_pi_session(
            state.clone(),
            run_id.to_owned(),
            local_pi::LocalPiSpawnOptions {
                execution_generation: generation,
                resume_required: action == "resolve",
                initial_prompt: marker["hint"].as_str().map(str::to_owned),
                triggered_by_command: None,
            },
        );
        marker["phase"] = json!("dispatched");
        let _ = state
            .store
            .put("run_execution_commands", command_id, &marker);
        return (status, Json(response)).into_response();
    }
    if action == "start" && specs.is_empty() {
        marker["phase"] = json!("dispatched");
        let _ = state
            .store
            .put("run_execution_commands", command_id, &marker);
        return (status, Json(response)).into_response();
    }
    if let Err(error) = usage::restore(state, run_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail":error})),
        )
            .into_response();
    }
    if marker["coordinator"].as_bool().unwrap_or(true) {
        coordinator::spawn_coordinator(
            state.clone(),
            run_id.to_owned(),
            specs,
            challenge,
            marker["hint"].as_str().map(str::to_owned),
        );
    } else {
        for (index, spec) in specs.into_iter().enumerate() {
            worker::spawn_local_worker(
                state.clone(),
                run_id.to_owned(),
                spec,
                challenge.clone(),
                marker["hint"].as_str().map(str::to_owned),
                index + 1,
            );
        }
    }
    marker["phase"] = json!("dispatched");
    let _ = state
        .store
        .put("run_execution_commands", command_id, &marker);
    (status, Json(response)).into_response()
}

pub(crate) async fn start_run(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Err(error) = crate::business::require_run(&state, &run_id) {
        return error.into_response();
    }
    let run_handle = state.runs.handle(&state, &run_id);
    let _lifecycle_guard = run_handle.delete_lock.lock().await;
    if run_handle.deleting.load(Ordering::SeqCst) {
        return Json(json!({"ok":false,"detail":"Run deletion is settling"})).into_response();
    }
    {
        let requested_kind = body["kind"].as_str().unwrap_or("swarm").trim();
        if !matches!(requested_kind, "swarm" | "idle" | "local_pi") {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail": format!("Unsupported run kind: {requested_kind}")})),
            )
                .into_response();
        }
        if requested_kind == "local_pi" {
            let settings = match local_pi::load_local_pi_settings(&state) {
                Ok(s) => s,
                Err(error) => return error.into_response(),
            };
            let enabled = settings
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if !enabled {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"code": "local_pi.disabled", "detail": "本地 Pi 单会话已关闭，请在设置中启用"})),
                )
                    .into_response();
            }
        }
    }

    let cmd_id = body["command_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4().simple()));
    if let Ok(Some(prior)) = state.store.get("run_execution_commands", &cmd_id) {
        if prior["run_id"] == run_id && prior["action"] == "start" {
            if (body["production_guard"]["enabled"] == true
                || body["challenge"]["production_guard"]["enabled"] == true)
                && !execution_guard::protected(&prior["challenge"])
            {
                return execution_conflict(
                    "guard.command_mismatch",
                    "Prior execution command has a different protection mode",
                );
            }
            if prior["phase"] == "dispatched" {
                return (
                    StatusCode::from_u16(prior["status"].as_u64().unwrap_or(200) as u16)
                        .unwrap_or(StatusCode::OK),
                    Json(prior["response"].clone()),
                )
                    .into_response();
            }
            return resume_execution_command(&state, &run_id, &cmd_id, prior);
        }
        return (
            StatusCode::CONFLICT,
            Json(json!({"detail":"Execution command_id belongs to another Run action"})),
        )
            .into_response();
    }

    match state.runs.claim_execution_owner(&state, &run_id) {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"detail":"Run is owned by another service process"})),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error})),
            )
                .into_response();
        }
    }

    let mut run_doc = match ensure_run_record(&state, &run_id) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error})),
            )
                .into_response();
        }
    };
    if let Err(response) = check_new_execution(&state, &run_id, &run_doc) {
        return response;
    }

    let raw_challenge = match execution_guard::dispatch_challenge(
        &body,
        body.get("challenge").cloned().unwrap_or_else(|| json!({})),
    ) {
        Ok(value) => value,
        Err(detail) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":detail})),
            )
                .into_response();
        }
    };
    let authorized_pentest_contract = if raw_challenge["src_task_id"].as_str().is_some() {
        if raw_challenge["mode"] != "pentest" {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"detail": "SRC Run 只能按渗透测试授权模式启动"})),
            )
                .into_response();
        }
        match src_api::bound_contract(&state, &run_id, &raw_challenge) {
            Ok(contract) => Some(contract),
            Err(detail) => {
                return (StatusCode::FORBIDDEN, Json(json!({"detail": detail}))).into_response();
            }
        }
    } else if run_doc["src_task_id"].as_str().is_some() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail": "SRC Run 必须通过当前任务授权启动"})),
        )
            .into_response();
    } else {
        None
    };
    if raw_challenge["src_task_id"].as_str().is_none()
        && body["worker_network"].as_str().is_some_and(|network| {
            network.starts_with("container:") || network.starts_with("src-record:")
        })
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail":"Task gateway Worker 网络需要绑定 SRC Task"})),
        )
            .into_response();
    }
    let (_, challenge) = match prepare_dispatch_contract_with_authorization(
        &raw_challenge,
        authorized_pentest_contract.as_ref(),
    ) {
        Ok(res) => res,
        Err(detail) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail": detail})),
            )
                .into_response();
        }
    };
    let guard_requested =
        execution_guard::protected(&challenge) || execution_guard::protected(&run_doc);
    if guard_requested && body["kind"].as_str().is_some_and(|kind| kind != "swarm") {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"code":"guard.preflight.only_muteki_pi_allowed"})),
        )
            .into_response();
    }
    if guard_requested
        && body["worker_profiles"].as_array().is_some_and(|seats| {
            seats.iter().any(|seat| {
                seat["enabled"] != false
                    && seat["engine"]
                        .as_str()
                        .is_some_and(|engine| settings::normalize_engine(engine) != "pi")
            })
        })
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"code":"guard.preflight.only_muteki_pi_allowed"})),
        )
            .into_response();
    }
    let src_environment = if let Some(task_id) = raw_challenge["src_task_id"].as_str() {
        match src_api::prepare_worker_environment(Arc::clone(&state), task_id).await {
            Ok(environment) => Some(environment),
            Err(detail) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"detail":detail})),
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    let body = match launch_config::resolve(&state, &run_id, &run_doc, &body, &challenge, false) {
        Ok(config) => config,
        Err(detail) => {
            let status = if detail.contains("kind is fixed") {
                StatusCode::CONFLICT
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            return (status, Json(json!({"detail":detail}))).into_response();
        }
    };
    let kind = body["kind"].as_str().unwrap_or("swarm").trim();
    if kind == "swarm"
        && matches!(challenge["mode"].as_str(), Some("ctf" | "pentest"))
        && authorized_pentest_contract.is_none()
        && body["_competition_visit_command"].as_str().is_none()
        && !crate::settings_metadata::planner_configured(&state, &body)
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "code": "run.dispatch.planner_required",
                "detail": "缺少有效的推理模型配置，任务未下发。"
            })),
        )
            .into_response();
    }
    let exec_gen = run_doc["execution_generation"].as_u64().unwrap_or(0) + 1;
    let ctrl_gen = run_doc["control_generation"].as_u64().unwrap_or(0);

    let mut preflight_payload = body.clone();
    if preflight_payload.get("cost_budget_usd").is_none() {
        preflight_payload["cost_budget_usd"] = run_doc["cost_budget_usd"].clone();
    }
    for key in ["mode", "src_task_id", "bound_to_src"] {
        if preflight_payload.get(key).is_none() {
            preflight_payload[key] = run_doc[key].clone();
        }
    }
    let (specs, degraded) = if matches!(kind, "idle" | "local_pi") {
        (Vec::new(), Vec::new())
    } else {
        match worker::resolve_worker_specs(&state, &preflight_payload, exec_gen).await {
            Ok(s) => s,
            Err((detail, failures)) => {
                if guard_requested && !failures.is_empty() {
                    if let Some(response) =
                        execution_guard::preflight(&state, &[], &failures, Some(&preflight_payload))
                    {
                        return response;
                    }
                }
                if failures.is_empty() {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({"detail": detail})),
                    )
                        .into_response();
                }
                let mut failed = run_doc.clone();
                failed["execution_generation"] = json!(exec_gen);
                failed["profile_failures"] = json!(failures);
                failed["running"] = json!(false);
                if let Err(error) = terminal::finish_projected(
                    &state,
                    &run_id,
                    exec_gen,
                    None,
                    false,
                    "preflight_failed",
                    Some(&detail),
                    Some((&run_doc, &failed)),
                ) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"detail":error})),
                    )
                        .into_response();
                }
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({
                        "detail": detail,
                        "profile_failures": failures,
                    })),
                )
                    .into_response();
            }
        }
    };

    if guard_requested {
        if let Some(response) =
            execution_guard::preflight(&state, &specs, &degraded, Some(&preflight_payload))
        {
            return response;
        }
    }
    for failure in &degraded {
        emit_event(&state, &run_id, None, "engine_degraded", failure.clone());
    }
    run_doc["profile_failures"] = json!(degraded);
    let now = now_secs();
    let custom_name = run_doc["custom_name"].as_str().map(str::to_owned);
    let name_autogen = titler::should_auto_title(custom_name.as_deref(), &raw_challenge);
    let display_name = custom_name
        .clone()
        .unwrap_or_else(|| challenge["name"].as_str().unwrap_or(&run_id).to_owned());

    run_doc["name"] = json!(display_name);
    run_doc["name_autogen"] = json!(name_autogen);
    run_doc["original_name"] = challenge["name"].clone();
    run_doc["category"] = challenge["category"].clone();
    run_doc["mode"] = challenge["mode"].clone();
    run_doc["target"] = challenge["target"].clone();
    run_doc["flag_format"] = challenge["flag_format"].clone();
    run_doc["description"] = challenge["description"].clone();
    run_doc["expected_flags"] = challenge["expected_flags"].clone();
    run_doc["multi_flag"] = challenge["multi_flag"].clone();
    run_doc["platform_confirmation_required"] = challenge["platform_confirmation_required"].clone();
    run_doc["allow_operator_input"] = challenge["allow_operator_input"].clone();
    run_doc["points"] = challenge["points"].clone();
    run_doc["attachments"] = challenge["attachments"].clone();
    run_doc["task_contract"] = challenge["task_contract"].clone();
    run_doc["pentest_contract"] = challenge["pentest_contract"].clone();
    run_doc["src_task_id"] = challenge["src_task_id"].clone();
    let requested_network = src_environment
        .as_ref()
        .and_then(|environment| environment["worker_network"].as_str())
        .filter(|network| !network.is_empty())
        .or_else(|| {
            body["worker_network"]
                .as_str()
                .filter(|network| !network.is_empty())
        });
    if let Some(network) = requested_network {
        let projection = container_exec::project_worker_network(network);
        if projection.effective.is_none() {
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail":projection.error.unwrap_or_else(||"Worker 网络模式无效".to_owned())}))).into_response();
        }
        run_doc["worker_network"] = json!(network);
    } else {
        run_doc["worker_network"] = Value::Null;
    }
    if let Some(environment) = &src_environment {
        run_doc["src_environment_generation"] = environment["environment_generation"].clone();
        run_doc["src_environment_policy_digest"] = environment["policy_digest"].clone();
        run_doc["src_environment_policy_version"] = environment["policy_version"].clone();
        run_doc["src_environment_gateway"] = environment["gateway_container"].clone();
        run_doc["src_network_enforced"] = environment["network_enforced"].clone();
        run_doc["src_content_enforced"] = environment["content_enforced"].clone();
    }
    run_doc["status"] = json!(if kind == "idle" { "idle" } else { "running" });
    run_doc["running"] = json!(kind != "idle");
    run_doc["finished"] = json!(false);
    run_doc["control_mode"] = json!("active");
    run_doc["paused"] = json!(false);
    run_doc["solved"] = json!(false);
    run_doc["execution_generation"] = json!(exec_gen);
    run_doc["control_generation"] = json!(ctrl_gen);
    run_doc["updated"] = json!(now);
    run_doc["outcome_reason"] = Value::Null;
    run_doc["outcome_detail"] = Value::Null;
    launch_config::persist(&mut run_doc, &body, &challenge);

    let response = json!({"run_id":run_id,"started":true,"kind":kind,
        "receipt":{"schema_version":1,"command_id":cmd_id,"state":"completed","run_id":run_id}});
    let marker = json!({"command_id":cmd_id,"run_id":run_id,"action":"start",
        "generation":exec_gen,"status":200,"response":response,"phase":"prepared",
        "kind":kind,"challenge":challenge,"specs":specs,
        "coordinator":body["coordinator"].as_bool().unwrap_or(true),
        "name_autogen":name_autogen,"control_generation":ctrl_gen});
    if let Err(err) = state.store.put_many(&[
        ("runs", &run_id, &run_doc),
        ("run_execution_commands", &cmd_id, &marker),
    ]) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": err.to_string()})),
        )
            .into_response();
    }

    emit_event(
        &state,
        &run_id,
        if kind == "local_pi" {
            Some(local_pi::LOCAL_PI_SOLVER_ID)
        } else {
            None
        },
        "run.preparing",
        json!({
            "challenge": challenge,
            "name_autogen": name_autogen,
            "kind": kind,
            "driver_kind": kind,
            "execution_generation": exec_gen,
            "control_generation": ctrl_gen,
        }),
    );

    emit_event(
        &state,
        &run_id,
        if kind == "local_pi" {
            Some(local_pi::LOCAL_PI_SOLVER_ID)
        } else {
            None
        },
        "run.started",
        json!({
            "challenge": challenge,
            "name_autogen": name_autogen,
            "kind": kind,
            "driver_kind": kind,
            "execution_generation": exec_gen,
            "control_generation": ctrl_gen,
        }),
    );

    if name_autogen {
        titler::spawn_if_needed(state.clone(), run_id.clone(), challenge.clone());
    }

    if let Ok(g) = graph::SharedGraph::open(&state, &run_id) {
        if let Err(detail) = g.seed_origin_fact_checked(&challenge) {
            mark_run_finished(
                &state,
                &run_id,
                None,
                false,
                "graph_origin_failed",
                Some(&detail),
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":detail})),
            )
                .into_response();
        }
    }

    if challenge["mode"] == "pentest" {
        if let Err(detail) =
            pentest::record_contract(&state, &run_id, &challenge["pentest_contract"])
        {
            let mut failed_marker = marker.clone();
            failed_marker["status"] = json!(500);
            failed_marker["response"] = json!({"detail":detail});
            if let Err(error) = state
                .store
                .put("run_execution_commands", &cmd_id, &failed_marker)
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail":error.to_string()})),
                )
                    .into_response();
            }
            if let Err(error) = terminal::finish(
                &state,
                &run_id,
                exec_gen,
                None,
                false,
                "pentest_contract_unavailable",
                Some(&detail),
            ) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail":error})),
                )
                    .into_response();
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": detail})),
            )
                .into_response();
        }
        if guard_requested {
            if let Err(detail) = execution_guard::init_run(&state, &run_id, exec_gen) {
                if let Err(error) = terminal::finish(
                    &state,
                    &run_id,
                    exec_gen,
                    None,
                    false,
                    "guard_init_failed",
                    Some(&detail),
                ) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"detail":error})),
                    )
                        .into_response();
                }
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail": detail})),
                )
                    .into_response();
            }
        }
    }

    if let Err(error) = usage::restore(&state, &run_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail":error})),
        )
            .into_response();
    }
    state
        .runs
        .handle(&state, &run_id)
        .draining
        .store(false, Ordering::SeqCst);
    if kind == "local_pi" {
        let initial_prompt = body
            .pointer("/local_pi_config/prompt")
            .or_else(|| body.get("prompt"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        local_pi::spawn_local_pi_session(
            state.clone(),
            run_id.clone(),
            local_pi::LocalPiSpawnOptions {
                execution_generation: exec_gen,
                resume_required: false,
                initial_prompt,
                triggered_by_command: None,
            },
        );
    } else if !specs.is_empty() {
        let use_coordinator = body["coordinator"].as_bool().unwrap_or(true);
        if use_coordinator {
            coordinator::spawn_coordinator(
                state.clone(),
                run_id.clone(),
                specs,
                challenge.clone(),
                None,
            );
        } else {
            for (index, spec) in specs.into_iter().enumerate() {
                worker::spawn_local_worker(
                    state.clone(),
                    run_id.clone(),
                    spec,
                    challenge.clone(),
                    None,
                    index + 1,
                );
            }
        }
    }

    let mut dispatched_marker = marker;
    dispatched_marker["phase"] = json!("dispatched");
    let _ = state
        .store
        .put("run_execution_commands", &cmd_id, &dispatched_marker);
    Json(response).into_response()
}

pub fn record_flag(
    state: &AppState,
    run_id: &str,
    solver_id: &str,
    flag: &str,
) -> Result<bool, String> {
    let handle = state.runs.handle(state, run_id);
    if handle.rejected_flags.lock().unwrap().contains(flag) {
        return Err("Flag was marked false by operator".to_owned());
    }

    let mut run_doc = ensure_run_record(state, run_id)?;
    let mut flags: Vec<String> = run_doc["flags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let new_flag = !flags.iter().any(|f| f == flag);
    if new_flag {
        flags.push(flag.to_owned());
    }
    let expected = run_doc["expected_flags"].as_u64().unwrap_or(1);
    let multi_flag = run_doc["multi_flag"] == true;
    let platform_confirmation_required = run_doc["platform_confirmation_required"] == true;
    let solved = run_doc["solved"] == true
        || (!platform_confirmation_required
            && !(multi_flag && expected <= 1)
            && flags.len() >= expected.max(1) as usize);

    run_doc["flag"] = json!(flags.first().cloned().unwrap_or_else(|| flag.to_owned()));
    run_doc["flags"] = json!(flags);
    run_doc["solved"] = json!(solved);
    run_doc["updated"] = json!(now_secs());
    let _ = state.store.put("runs", run_id, &run_doc);

    if new_flag {
        if let Ok(g) = graph::SharedGraph::open(state, run_id) {
            let _ = g.flag_found(solver_id, flag);
        }

        emit_event(
            state,
            run_id,
            Some(solver_id),
            "solvegraph.delta",
            json!({"kind": "flag", "flag": flag}),
        );
        emit_event(
            state,
            run_id,
            Some(solver_id),
            "insight.event",
            json!({"kind": "FlagFound", "flag": flag, "text": format!("Flag found: {flag}")}),
        );
        emit_event(
            state,
            run_id,
            Some(solver_id),
            "blackboard.delta",
            json!({
                "kind": "flag_found",
                "flag": flag,
                "actor": solver_id,
                "source": solver_id,
            }),
        );
    }

    Ok(solved)
}

pub fn record_hitl_request(state: &AppState, run_id: &str, solver_id: &str, need: &str) -> String {
    let handle = state.runs.handle(state, run_id);
    let req_id = format!("hitl-{}", uuid::Uuid::new_v4().simple());
    if let Ok(g) = graph::SharedGraph::open(state, run_id) {
        let _ = g.add_hitl_request(&req_id, solver_id, need);
    }
    handle.hitl_requests.lock().unwrap().push(json!({
        "request_id": req_id,
        "worker": solver_id,
        "need": need,
        "closed": false,
        "ts": now_secs(),
    }));
    if let Ok(Some(mut run_doc)) = state.store.get("runs", run_id) {
        run_doc["awaiting_operator"] = json!(need);
        run_doc["updated"] = json!(now_secs());
        let _ = state.store.put("runs", run_id, &run_doc);
    }
    emit_event(
        state,
        run_id,
        Some(solver_id),
        "hitl.request",
        json!({
            "request_id": req_id,
            "id": req_id,
            "worker": solver_id,
            "need": need,
            "kind": "need_input",
            "need_kind": "external_blocker",
        }),
    );
    req_id
}

pub fn render_blackboard_context(state: &AppState, run_id: &str) -> String {
    let run_doc = state
        .store
        .get("runs", run_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}));
    if let Ok(g) = graph::SharedGraph::open(state, run_id) {
        return g.to_board_markdown(run_id, &run_doc);
    }
    let handle = state.runs.handle(state, run_id);
    let mut md = format!(
        "# Blackboard Snapshot ({run_id})\n\
         - Challenge: {}\n\
         - Category: {}\n\
         - Flag Format: `{}`\n\
         - Solved: {}\n",
        run_doc["name"].as_str().unwrap_or(run_id),
        run_doc["category"].as_str().unwrap_or("misc"),
        run_doc["flag_format"].as_str().unwrap_or("flag{...}"),
        run_doc["solved"].as_bool().unwrap_or(false),
    );
    if let Some(flags) = run_doc["flags"].as_array()
        && !flags.is_empty()
    {
        let list: Vec<&str> = flags.iter().filter_map(Value::as_str).collect();
        md.push_str(&format!("- Flags: {}\n", list.join(", ")));
    }
    let facts = handle.facts.lock().unwrap().clone();
    md.push_str("\n## Verified Facts\n");
    if facts.is_empty() {
        md.push_str("- (none yet)\n");
    } else {
        for f in facts {
            md.push_str(&format!(
                "- [fact#{}] {}\n",
                f["fact_seq"].as_u64().unwrap_or(0),
                f["fact"].as_str().unwrap_or("")
            ));
        }
    }
    let dead_ends = handle.dead_ends.lock().unwrap().clone();
    md.push_str("\n## Dead Ends\n");
    if dead_ends.is_empty() {
        md.push_str("- (none)\n");
    } else {
        for d in dead_ends {
            md.push_str(&format!("- {}\n", d["reason"].as_str().unwrap_or("")));
        }
    }
    let directives = handle.directives.lock().unwrap().clone();
    if !directives.is_empty() {
        md.push_str("\n## Operator Directives\n");
        for dir in directives {
            md.push_str(&format!("- {}\n", dir["text"].as_str().unwrap_or("")));
        }
    }
    md
}

pub fn mark_run_finished(
    state: &AppState,
    run_id: &str,
    solver_id: Option<&str>,
    solved: bool,
    reason: &str,
    detail: Option<&str>,
) {
    let generation = terminal::task_generation(run_id).or_else(|| {
        solver_id
            .filter(|id| id.starts_with("cli-") && *id != local_pi::LOCAL_PI_SOLVER_ID)
            .map(worker::worker_execution_generation)
    });
    let result = generation
        .map(Ok)
        .unwrap_or_else(|| {
            state
                .store
                .get("runs", run_id)
                .map_err(|e| e.to_string())
                .and_then(|run| run.ok_or("Run missing at terminal transition".into()))
                .map(|run| run["execution_generation"].as_u64().unwrap_or(1))
        })
        .and_then(|generation| {
            terminal::finish(state, run_id, generation, solver_id, solved, reason, detail)
        });
    if let Err(error) = result {
        tracing::error!(run_id,%error,"Run terminal transition remains uncommitted");
    }
}

fn run_uses_container(state: &AppState, run_id: &str) -> Result<bool, String> {
    let commands = state
        .store
        .list("run_execution_commands")
        .map_err(|error| error.to_string())?;
    Ok(commands.iter().any(|command| {
        command["run_id"] == run_id
            && command["specs"]
                .as_array()
                .is_some_and(|specs| specs.iter().any(|spec| spec["backend"] == "container"))
    }))
}

fn watch_unsettled_delete(
    state: Arc<AppState>,
    run_id: String,
    handle: Arc<RunHandle>,
    container_expected: bool,
) {
    if handle.cleanup_watcher.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _delete_guard = handle.delete_lock.lock().await;
            let current = state.runs.handles.lock().unwrap().get(&run_id).cloned();
            if !handle.deleting.load(Ordering::SeqCst)
                || !current
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &handle))
            {
                break;
            }
            let tasks_settled = handle
                .coordinator_tasks
                .lock()
                .unwrap()
                .iter()
                .all(tokio::task::AbortHandle::is_finished)
                && handle
                    .worker_tasks
                    .lock()
                    .unwrap()
                    .iter()
                    .all(tokio::task::AbortHandle::is_finished);
            if !tasks_settled {
                continue;
            }
            if state
                .agents
                .close_in_workspaces(&[
                    session_dir(&state, &run_id),
                    graph::graph_dir(&state, &run_id),
                ])
                .await
                .is_err()
            {
                continue;
            }
            if container_exec::teardown_owned_run_containers(&run_id)
                .await
                .is_err()
            {
                continue;
            }
            if container_exec::teardown_recorded_before_delete(&state, &run_id)
                .await
                .is_err()
            {
                continue;
            }
            if container_exec::teardown_orphan_run_container(&state, &run_id, container_expected)
                .await
                .is_err()
            {
                continue;
            }
            handle.deleting.store(false, Ordering::SeqCst);
            break;
        }
        handle.cleanup_watcher.store(false, Ordering::SeqCst);
    });
}

pub(crate) async fn delete_run(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Response {
    delete_run_inner(state, run_id, None).await
}
async fn delete_run_inner(
    state: Arc<AppState>,
    run_id: String,
    expected: Option<(Value, f64)>,
) -> Response {
    let run = match state.store.get("runs", &run_id) {
        Ok(Some(run)) => run,
        Ok(None) => return Json(json!({"ok":false})).into_response(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok":false,"detail":error.to_string()})),
            )
                .into_response();
        }
    };
    let source_records = match state.store.list("src_live_records") {
        Ok(records) => records,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok":false,"detail":error.to_string()})),
            )
                .into_response();
        }
    };
    let task_id = source_records
        .iter()
        .find(|record| {
            record["values"]["kind"] == "run_binding"
                && record["values"]["payload"]["run_id"] == run_id
        })
        .and_then(|record| record["values"]["task_id"].as_str())
        .or_else(|| run["src_task_id"].as_str().filter(|task| !task.is_empty()));
    if let Some(task_id) = task_id {
        let key = format!("run_disposal:{task_id}:{run_id}");
        match state.store.get("src_live_records", &key) {
            Ok(Some(record)) if record["values"]["payload"]["state"] == "approved" => {}
            Ok(_) => return Json(json!({"ok":false})).into_response(),
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok":false,"detail":error.to_string()})),
                )
                    .into_response();
            }
        }
    }
    match state.runs.claim_execution_owner(&state, &run_id) {
        Ok(true) => {}
        Ok(false) => return Json(json!({"ok":false})).into_response(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok":false,"detail":error})),
            )
                .into_response();
        }
    }
    let container_expected = match run_uses_container(&state, &run_id) {
        Ok(expected) => expected,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok":false,"detail":error})),
            )
                .into_response();
        }
    };
    let handle = state.runs.handle(&state, &run_id);
    let _delete_guard = handle.delete_lock.lock().await;
    if let Some((expected, activity)) = expected.as_ref() {
        let current_activity = match last_persisted_activity(&state, &run_id).await {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok":false,"detail":error})),
                )
                    .into_response();
            }
        };
        if current_activity != *activity {
            return (
                StatusCode::CONFLICT,
                Json(json!({"ok":false,"detail":"Run activity changed before retention deletion"})),
            )
                .into_response();
        }
        match state.store.get("runs", &run_id) {
            Ok(Some(current)) if &current == expected => {}
            Ok(_) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"ok":false,"detail":"Run changed before retention deletion"})),
                )
                    .into_response();
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok":false,"detail":error.to_string()})),
                )
                    .into_response();
            }
        }
    }
    if let Err(error) = state.runs.settle_for_delete(&handle).await {
        watch_unsettled_delete(
            state.clone(),
            run_id.clone(),
            handle.clone(),
            container_expected,
        );
        return Json(json!({"ok":false,"detail":error})).into_response();
    }
    if let Err(error) = state
        .agents
        .close_in_workspaces(&[
            session_dir(&state, &run_id),
            graph::graph_dir(&state, &run_id),
        ])
        .await
    {
        watch_unsettled_delete(
            state.clone(),
            run_id.clone(),
            handle.clone(),
            container_expected,
        );
        return Json(json!({"ok":false,"detail":error})).into_response();
    }
    if let Err(error) = container_exec::teardown_owned_run_containers(&run_id).await {
        watch_unsettled_delete(
            state.clone(),
            run_id.clone(),
            handle.clone(),
            container_expected,
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"detail":error})),
        )
            .into_response();
    }
    if let Err(error) = container_exec::teardown_recorded_before_delete(&state, &run_id).await {
        watch_unsettled_delete(
            state.clone(),
            run_id.clone(),
            handle.clone(),
            container_expected,
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"detail":error})),
        )
            .into_response();
    }
    if let Err(error) =
        container_exec::teardown_orphan_run_container(&state, &run_id, container_expected).await
    {
        watch_unsettled_delete(
            state.clone(),
            run_id.clone(),
            handle.clone(),
            container_expected,
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok":false,"detail":error})),
        )
            .into_response();
    }
    let logical = session_dir(&state, &run_id);
    if let Ok(metadata) = logical.symlink_metadata()
        && metadata.file_type().is_symlink()
    {
        let slot = match shared_pools::registered_slot(&state, &run_id) {
            Ok(slot) => slot,
            Err(error) => {
                return (StatusCode::CONFLICT, Json(json!({"detail":error}))).into_response();
            }
        };
        let target = logical.canonicalize().map_err(|error| error.to_string());
        let expected = slot.canonicalize().map_err(|error| error.to_string());
        if target.is_err() || expected.is_err() || target != expected {
            return (
                StatusCode::CONFLICT,
                Json(json!({"detail":"Shared workspace ownership changed before deletion"})),
            )
                .into_response();
        }
        if let Err(error) = fs::remove_dir_all(&slot).and_then(|_| fs::remove_file(&logical)) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error.to_string()})),
            )
                .into_response();
        }
    }
    for dir in [
        session_dir(&state, &run_id),
        control_dir(&state, &run_id),
        graph::graph_dir(&state, &run_id),
    ] {
        if let Err(error) = fs::remove_dir_all(&dir) {
            if error.kind() != std::io::ErrorKind::NotFound {
                handle.deleting.store(false, Ordering::SeqCst);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok":false,"detail":error.to_string()})),
                )
                    .into_response();
            }
        }
    }
    let deleted = (|| -> Result<bool, String> {
        let Some(before) = state
            .store
            .get("runs", &run_id)
            .map_err(|error| error.to_string())?
        else {
            return Ok(false);
        };
        state.store.put_many_if_with_deletes(&[("runs",&run_id,Some(&before))],&[],|conn|{
            let mut stmt=conn.prepare("SELECT collection,id FROM documents WHERE collection IN ('run_followup_jobs','run_followup_artifacts','run_followup_events','run_guidance_deliveries','run_control_recoveries','run_reports') AND json_extract(value,'$.run_id')=?1")?;
            let mut deletes=stmt.query_map(params![run_id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            deletes.push(("runs".to_owned(),run_id.clone()));
            Ok(Some(deletes))
        }).map_err(|error|error.to_string())
    })();
    match deleted {
        Ok(deleted) => {
            if deleted {
                state.runs.remove_run(&run_id);
            } else {
                handle.deleting.store(false, Ordering::SeqCst);
            }
            if deleted && let Err(error) = crate::chat::purge_run_side_chats(&state, &run_id).await
            {
                return (StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok":false,"detail":format!("Run deleted, but its side chats were not removed: {error}")}))).into_response();
            }
            if deleted && let Err(error) = crate::chat::purge_ctf_run_threads(&state, &run_id).await
            {
                return (StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok":false,"detail":format!("Run deleted, but its internal CTF threads were not removed: {error}")}))).into_response();
            }
            Json(json!({"ok":deleted})).into_response()
        }
        Err(error) => {
            handle.deleting.store(false, Ordering::SeqCst);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok":false,"detail":error.to_string()})),
            )
                .into_response()
        }
    }
}

pub(crate) async fn open_run(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Response {
    let dir = session_dir(&state, &run_id);
    let _ = fs::create_dir_all(&dir);
    #[cfg(target_os = "macos")]
    {
        let _ = crate::host_process::std_command("open").arg(&dir).spawn();
    }
    Json(json!({"ok": true, "path": dir.to_string_lossy()})).into_response()
}

fn resolve_guidance(state: &AppState, run_id: &str, marker: &Value) -> Result<(), String> {
    let Some(text) = marker["hint"].as_str().filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let generation = marker["generation"]
        .as_u64()
        .ok_or("Resolve generation missing")?;
    let id = format!("dir-resolve-{generation}");
    let action = marker["request"]["guidance"]["action"]
        .as_str()
        .unwrap_or("hint");
    let graph = graph::SharedGraph::open(state, run_id)?;
    if !graph.directives().iter().any(|d| d["directive_id"] == id) {
        if graph.add_operator_directive(
            &id,
            "operator",
            "global",
            action,
            text,
            true,
            "soft_rebind",
        ) <= 0
        {
            return Err("Resolve directive persistence failed".into());
        }
    }
    let conn = open_control_db(state, run_id)?;
    control_context::create_guidance(
        &conn,
        run_id,
        &id,
        "global",
        "global",
        action,
        &json!({"text":text,"standing":true,"directive_id":id}),
    )?;
    for before in state
        .store
        .list("run_control_recoveries")
        .map_err(|e| e.to_string())?
    {
        if before["run_id"] == run_id
            && before["source_execution_generation"].as_u64() == Some(generation - 1)
            && matches!(before["state"].as_str(), Some("pending" | "processing"))
        {
            let mut row = before.clone();
            row["state"] = json!("settled");
            row["settled_by"] = marker["command_id"].clone();
            let key = before["command_id"]
                .as_str()
                .ok_or("Recovery command id missing")?;
            state
                .store
                .put_many_if(
                    &[("run_control_recoveries", key, Some(&before))],
                    &[("run_control_recoveries", key, &row)],
                )
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn directive_changed(
    state: &AppState,
    run_id: &str,
    id: &str,
    status: &str,
    reason: &str,
) -> Result<(), String> {
    let mut updated = false;
    for _ in 0..16 {
        let mut run = state
            .store
            .get("runs", run_id)
            .map_err(|e| e.to_string())?
            .ok_or("Run not found")?;
        let before = run.clone();
        run["guidance_revision"] = json!(run["guidance_revision"].as_u64().unwrap_or(0) + 1);
        if state
            .store
            .put_many_if(
                &[("runs", run_id, Some(&before))],
                &[("runs", run_id, &run)],
            )
            .map_err(|e| e.to_string())?
        {
            updated = true;
            break;
        }
    }
    if !updated {
        return Err("Guidance revision changed concurrently".into());
    }
    emit_event(
        state,
        run_id,
        Some("followup"),
        "blackboard.delta",
        json!({"kind":"operator_directive_changed","directive_id":id,"status":status,"reason":reason}),
    );
    Ok(())
}

pub(crate) async fn resolve_run(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    if let Err(error) = crate::business::require_run(&state, &run_id) {
        return error.into_response();
    }
    let run_handle = state.runs.handle(&state, &run_id);
    let _lifecycle_guard = run_handle.delete_lock.lock().await;
    if run_handle.deleting.load(Ordering::SeqCst) {
        return Json(json!({"ok":false,"detail":"Run deletion is settling"})).into_response();
    }
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let Some(mut run_doc) = state.store.get("runs", &run_id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": format!("Run {run_id} not found")})),
        )
            .into_response();
    };
    let cmd_id = body["command_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4().simple()));
    if let Ok(Some(prior)) = state.store.get("run_execution_commands", &cmd_id) {
        if prior["run_id"] == run_id && prior["action"] == "resolve" {
            if (body["production_guard"]["enabled"] == true
                || body["challenge"]["production_guard"]["enabled"] == true)
                && !execution_guard::protected(&prior["challenge"])
            {
                return execution_conflict(
                    "guard.command_mismatch",
                    "Prior execution command has a different protection mode",
                );
            }
            if prior.get("request").is_some_and(|r| r != &body) {
                return followup::error(
                    StatusCode::CONFLICT,
                    "run.resolve.command_conflict",
                    "command_id already used with different parameters",
                );
            }
            if prior["phase"] == "dispatched" {
                return (
                    StatusCode::from_u16(prior["status"].as_u64().unwrap_or(200) as u16)
                        .unwrap_or(StatusCode::OK),
                    Json(prior["response"].clone()),
                )
                    .into_response();
            }
            return resume_execution_command(&state, &run_id, &cmd_id, prior);
        }
        return (
            StatusCode::CONFLICT,
            Json(json!({"detail":"Execution command_id belongs to another Run action"})),
        )
            .into_response();
    }
    if body["expected_execution_generation"]
        .as_u64()
        .is_some_and(|value| value != run_doc["execution_generation"].as_u64().unwrap_or(1))
        || body["expected_generation"]
            .as_u64()
            .is_some_and(|value| value != run_doc["control_generation"].as_u64().unwrap_or(0))
    {
        return followup::error(
            StatusCode::CONFLICT,
            "run.resolve.generation_conflict",
            "Run changed before continuation; refresh its state",
        );
    }
    let before = run_doc.clone();
    let original_request = body.clone();
    let bindings = match state.store.list("competition_bindings_live") {
        Ok(v) => v,
        Err(e) => {
            return followup::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage.failed",
                &e.to_string(),
            );
        }
    };
    let leases = match state.store.list("competition_leases_live") {
        Ok(v) => v,
        Err(e) => {
            return followup::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage.failed",
                &e.to_string(),
            );
        }
    };
    if leases.iter().any(|lease| {
        let bound = lease["run_id"] == run_id
            || bindings.iter().any(|b| {
                b["run_id"] == run_id
                    && (b["lease_id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map_or_else(
                            || {
                                b["competition_id"] == lease["competition_id"]
                                    && b["competition_challenge_id"]
                                        == lease["competition_challenge_id"]
                                    && matches!(
                                        lease["state"].as_str(),
                                        Some("active" | "renewing")
                                    )
                            },
                            |id| lease["lease_id"] == id,
                        ))
            });
        bound
            && (lease["state"] == "expired"
                || lease["expires_at"]
                    .as_str()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .is_some_and(|t| t <= chrono::Utc::now())
                || lease["expires_at"]
                    .as_f64()
                    .is_some_and(|t| t <= now_secs()))
    }) {
        return followup::error(
            StatusCode::CONFLICT,
            "run.resolve.lease_expired",
            "靶机租约已过期，请先续租或重新申领",
        );
    }
    match state.runs.claim_execution_owner(&state, &run_id) {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"detail":"Run is owned by another service process"})),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error})),
            )
                .into_response();
        }
    }
    match state.store.list("runner_commands") {
        Ok(commands)
            if commands.iter().any(|row| {
                row["run_id"] == run_id
                    && !row["exit"].is_object()
                    && row["execution_stopped"] != true
            }) =>
        {
            return (StatusCode::CONFLICT,Json(json!({"ok":false,"error":{"code":"run.execution.unconfirmed","message":"Previous Worker execution is still running or unknown; reconcile or confirm its stop before resuming"}}))).into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error.to_string()})),
            )
                .into_response();
        }
        _ => {}
    }
    if let Err(response) = check_new_execution(&state, &run_id, &run_doc) {
        return response;
    }
    if let Err(error) = crate::runner_remote::controller::stop_recorded(&state, &run_id).await {
        return (StatusCode::CONFLICT,Json(json!({"ok":false,"error":{"code":"run.execution.recovery_pending","message":error}}))).into_response();
    }
    let historical_source = if run_doc["historical"] == true && run_doc["mode"] == "pentest" {
        match pentest::historical_graph::prepare(&state, &run_id, &run_doc) {
            Ok(source) => Some(source),
            Err(detail) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail":detail})),
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    let exec_gen = run_doc["execution_generation"].as_u64().unwrap_or(1) + 1;
    let ctrl_gen = run_doc["control_generation"].as_u64().unwrap_or(0);
    let hint = body["guidance"]["text"]
        .as_str()
        .or_else(|| body["description"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let explicit_contract = body["challenge"].as_object().is_some_and(|c| {
        c.keys().any(|k| {
            matches!(
                k.as_str(),
                "name"
                    | "category"
                    | "flag_format"
                    | "target"
                    | "attachments"
                    | "pentest_contract"
                    | "task_contract"
                    | "mode"
                    | "src_task_id"
                    | "expected_flags"
                    | "multi_flag"
            )
        })
    });

    let mut challenge = historical_source
        .as_ref()
        .map(|source| source.challenge.clone())
        .unwrap_or_else(|| {
            json!({
                "name": run_doc["name"],
                "category": run_doc["category"],
                "description": run_doc["description"],
                "flag_format": run_doc["flag_format"],
                "target": run_doc["target"],
                "mode": run_doc["mode"],
                "expected_flags": run_doc["expected_flags"],
                "multi_flag": run_doc["multi_flag"],
                "attachments": run_doc["attachments"],
                "task_contract": run_doc["task_contract"],
                "pentest_contract": run_doc["pentest_contract"],
            })
        });
    if let Some(raw_challenge) = body.get("challenge").filter(|_| explicit_contract) {
        let authorized_pentest_contract = if raw_challenge["src_task_id"].as_str().is_some() {
            if raw_challenge["mode"] != "pentest" {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"detail":"SRC Run 只能按渗透测试授权模式启动"})),
                )
                    .into_response();
            }
            match src_api::bound_contract(&state, &run_id, raw_challenge) {
                Ok(contract) => Some(contract),
                Err(detail) => {
                    return (StatusCode::FORBIDDEN, Json(json!({"detail":detail}))).into_response();
                }
            }
        } else if run_doc["src_task_id"].as_str().is_some() {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"detail":"SRC Run 必须通过当前任务授权启动"})),
            )
                .into_response();
        } else {
            None
        };
        challenge = match prepare_dispatch_contract_with_authorization(
            raw_challenge,
            authorized_pentest_contract.as_ref(),
        ) {
            Ok((_, normalized)) => normalized,
            Err(detail) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"detail":detail})),
                )
                    .into_response();
            }
        };
    }

    let guard_requested = execution_guard::protected(&run_doc)
        || execution_guard::protected(&challenge)
        || body["production_guard"]["enabled"] == true;
    let body = match launch_config::resolve(&state, &run_id, &run_doc, &body, &challenge, true) {
        Ok(config) => config,
        Err(detail) => {
            return (StatusCode::CONFLICT, Json(json!({"detail":detail}))).into_response();
        }
    };
    let resolved_kind = body["kind"].as_str().unwrap_or("swarm").to_owned();
    if resolved_kind == "local_pi" {
        if local_pi::has_live_session(&run_id)
            && run_doc["local_pi_session"]["turn_active"].as_bool() == Some(true)
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({"detail": "Local Pi session turn is currently active"})),
            )
                .into_response();
        }
        if let Err(err) = local_pi::validate_saved_resume(&state, &run_id, &run_doc) {
            local_pi::record_resume_failure(&state, &run_id, &err, Some((&cmd_id, "resolve")));
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "ok": false,
                    "code": "local_pi.resume_failed",
                    "detail": err,
                })),
            )
                .into_response();
        }
    }
    launch_config::persist(&mut run_doc, &body, &challenge);
    let (specs, degraded) = if resolved_kind == "local_pi" {
        (Vec::new(), Vec::new())
    } else {
        let mut preflight_body = body.clone();
        for key in ["mode", "src_task_id", "bound_to_src"] {
            if preflight_body.get(key).is_none() {
                preflight_body[key] = run_doc[key].clone();
            }
        }
        match worker::resolve_worker_specs(&state, &preflight_body, exec_gen).await {
            Ok(s) => s,
            Err((detail, failures)) => {
                if guard_requested && !failures.is_empty() {
                    if let Some(response) =
                        execution_guard::preflight(&state, &[], &failures, Some(&body))
                    {
                        return response;
                    }
                }
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"detail": detail})),
                )
                    .into_response();
            }
        }
    };
    if guard_requested {
        if let Some(response) = execution_guard::preflight(&state, &specs, &degraded, Some(&body)) {
            return response;
        }
    }

    for failure in &degraded {
        emit_event(&state, &run_id, None, "engine_degraded", failure.clone());
    }
    run_doc["profile_failures"] = json!(degraded);
    if let Some(source) = &historical_source {
        if let Err(detail) = pentest::historical_graph::activate(&state, &run_id, source) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":detail})),
            )
                .into_response();
        }
        for key in [
            "name",
            "category",
            "description",
            "flag_format",
            "target",
            "mode",
            "expected_flags",
            "multi_flag",
            "platform_confirmation_required",
            "attachments",
            "task_contract",
            "pentest_contract",
        ] {
            run_doc[key] = challenge[key].clone();
        }
        pentest::historical_graph::mark_active(&mut run_doc, source.graph_seq);
        if let Err(detail) =
            pentest::record_contract(&state, &run_id, &challenge["pentest_contract"])
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":detail})),
            )
                .into_response();
        }
    }

    if let Err(detail) = graph::SharedGraph::open(&state, &run_id).and_then(|graph| {
        graph.reconcile_historical_direct_steps(
            &std::collections::HashSet::new(),
            Some(exec_gen),
            Some(&run_doc),
        )
    }) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail":detail})),
        )
            .into_response();
    }

    run_doc["status"] = json!("running");
    run_doc["running"] = json!(true);
    run_doc["finished"] = json!(false);
    run_doc["control_mode"] = json!("active");
    run_doc["paused"] = json!(false);
    run_doc["solved"] = json!(false);
    run_doc["terminal_at"] = Value::Null;
    run_doc["outcome_reason"] = Value::Null;
    run_doc["outcome_detail"] = Value::Null;
    run_doc["awaiting_operator"] = Value::Null;
    if let Some(obj) = run_doc.as_object_mut() {
        obj.remove("freeze_control");
    }
    if explicit_contract {
        for key in [
            "name",
            "category",
            "description",
            "flag_format",
            "target",
            "mode",
            "expected_flags",
            "multi_flag",
            "platform_confirmation_required",
            "attachments",
            "task_contract",
            "pentest_contract",
            "src_task_id",
        ] {
            run_doc[key] = challenge[key].clone();
        }
    }
    run_doc["execution_generation"] = json!(exec_gen);
    run_doc["updated"] = json!(now_secs());
    if let Err(error) = usage::restore(&state, &run_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail":error})),
        )
            .into_response();
    }
    let run_handle = state.runs.handle(&state, &run_id);
    run_handle.draining.store(false, Ordering::SeqCst);
    run_handle.paused.store(false, Ordering::SeqCst);
    let response = json!({"ok":true,"command_id":cmd_id,"kind":resolved_kind});
    let marker = json!({"command_id":cmd_id,"run_id":run_id,"action":"resolve","request":original_request,
        "generation":exec_gen,"status":200,"response":response,"phase":"prepared",
        "kind":resolved_kind,"challenge":challenge,"specs":specs,
        "coordinator":body["coordinator"].as_bool().unwrap_or(true),"hint":hint,
        "control_generation":ctrl_gen});
    match state.store.put_many_if(
        &[
            ("runs", &run_id, Some(&before)),
            ("run_execution_commands", &cmd_id, None),
        ],
        &[
            ("runs", &run_id, &run_doc),
            ("run_execution_commands", &cmd_id, &marker),
        ],
    ) {
        Ok(true) => {}
        Ok(false) => {
            return followup::error(
                StatusCode::CONFLICT,
                "run.resolve.generation_conflict",
                "Run changed before continuation",
            );
        }
        Err(e) => {
            return followup::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage.failed",
                &e.to_string(),
            );
        }
    }
    if let Err(error) = resolve_guidance(&state, &run_id, &marker) {
        return followup::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "run.resolve.guidance_failed",
            &error,
        );
    }

    emit_event(
        &state,
        &run_id,
        None,
        "run.reopened",
        json!({
            "reason": "resolve",
            "challenge": challenge,
            "execution_generation": exec_gen,
            "control_generation": ctrl_gen,
        }),
    );

    if resolved_kind == "local_pi" {
        if local_pi::dispatch_live_control(
            &run_id,
            local_pi::LocalPiControl::Continue {
                command_id: cmd_id.clone(),
                action: "resolve".to_owned(),
                text: hint.clone(),
            },
        )
        .await
        .is_err()
        {
            local_pi::spawn_local_pi_session(
                state.clone(),
                run_id.clone(),
                local_pi::LocalPiSpawnOptions {
                    execution_generation: exec_gen,
                    resume_required: true,
                    initial_prompt: hint.clone(),
                    triggered_by_command: Some((cmd_id.clone(), "resolve".to_owned())),
                },
            );
        }
    } else {
        let use_coordinator = body["coordinator"].as_bool().unwrap_or(true);
        if use_coordinator {
            coordinator::spawn_coordinator(
                state.clone(),
                run_id.clone(),
                specs,
                challenge.clone(),
                hint.clone(),
            );
        } else {
            for (index, spec) in specs.into_iter().enumerate() {
                worker::spawn_local_worker(
                    state.clone(),
                    run_id.clone(),
                    spec,
                    challenge.clone(),
                    hint.clone(),
                    index + 1,
                );
            }
        }
    }

    let mut dispatched_marker = marker;
    dispatched_marker["phase"] = json!("dispatched");
    let _ = state
        .store
        .put("run_execution_commands", &cmd_id, &dispatched_marker);
    Json(response).into_response()
}

async fn request_progress(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Response {
    let run = match state.store.get("runs", &run_id) {
        Ok(Some(run)) => run,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error.to_string()})),
            )
                .into_response();
        }
    };
    if run["execution_generation"].as_u64().unwrap_or(0) == 0 && run["started"] != true {
        return (
            StatusCode::CONFLICT,
            Json(json!({"detail":"Run has not been started yet"})),
        )
            .into_response();
    }
    let id = run_id.clone();
    match tokio::task::spawn_blocking(move || progress::publish(&state, &id, true)).await {
        Ok(Ok(brief)) => {
            Json(json!({"ok":true,"status":"published","brief_id":brief["brief_id"],"brief":brief}))
                .into_response()
        }
        result => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":{"code":"run.progress.failed","message":format!("{result:?}")}})),
        )
            .into_response(),
    }
}

fn open_control_db(state: &AppState, run_id: &str) -> Result<Connection, String> {
    let dir = control_dir(state, run_id);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let db_path = dir.join("control.db");
    let conn = Connection::open(&db_path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600));
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS control_commands (
            command_id TEXT PRIMARY KEY,
            action TEXT NOT NULL,
            target TEXT NOT NULL,
            status TEXT NOT NULL,
            generation INTEGER NOT NULL,
            payload_json TEXT NOT NULL,
            response_json TEXT NOT NULL,
            created_at REAL NOT NULL
        );",
    )
    .map_err(|e| e.to_string())?;
    control_context::init(&conn)?;
    control_journal::init(&conn)?;
    Ok(conn)
}

pub(crate) fn control_command_recorded(
    state: &AppState,
    run_id: &str,
    command_id: &str,
) -> Result<bool, String> {
    let conn = open_control_db(state, run_id)?;
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM control_commands WHERE command_id=?1",
            params![command_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(count > 0)
}

fn store_control_secret(
    state: &AppState,
    run_id: &str,
    secret_val: &str,
) -> Result<String, String> {
    let secrets_dir = control_dir(state, run_id).join("secrets");
    fs::create_dir_all(&secrets_dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&secrets_dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let secret_id = format!("sec-{}", uuid::Uuid::new_v4().simple());
    let path = secrets_dir.join(format!("{secret_id}.secret"));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|e| e.to_string())?;
    use std::io::Write;
    if let Err(error) = file
        .write_all(secret_val.as_bytes())
        .and_then(|_| file.sync_all())
    {
        let _ = fs::remove_file(&path);
        return Err(error.to_string());
    }
    Ok(secret_id)
}

fn context_delivery_scope(
    conn: &Connection,
    handle: &RunHandle,
    target: &str,
    command_id: &str,
) -> Result<Option<(String, Option<String>)>, String> {
    let worker_id = target
        .strip_prefix("worker:")
        .or_else(|| target.strip_prefix("solver:"));
    let Some(worker_id) = worker_id else {
        return Ok(Some((target.to_owned(), None)));
    };
    if let Some(previous) = control_context::resource_for_command(conn, command_id)? {
        let scope = previous["scope"].as_str().unwrap_or(target).to_owned();
        let continuation = scope
            .strip_prefix("intent:I-control-")
            .map(|_| scope.trim_start_matches("intent:").to_owned());
        return Ok(Some((scope, continuation)));
    }
    if !handle.workers.lock().unwrap().contains_key(worker_id) {
        return Ok(None);
    }
    let intent_id = format!("I-control-{command_id}");
    Ok(Some((format!("intent:{intent_id}"), Some(intent_id))))
}

fn propose_live_context_continuation(
    state: &AppState,
    run_id: &str,
    handle: &RunHandle,
    resource: &Value,
) -> bool {
    let coordinator_live = handle
        .coordinator_abort
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|task| !task.is_finished());
    coordinator_live
        && graph::SharedGraph::open(state, run_id)
            .is_ok_and(|graph| control_context::propose_continuation(&graph, resource))
}

/// Steers every live Worker concurrently; returns `(delivered, live_workers)`.
pub(crate) async fn steer_src_workers(
    state: &Arc<AppState>,
    run_id: &str,
    text: &str,
) -> (usize, usize) {
    let command_id = format!("src-steer-{}", uuid::Uuid::new_v4().simple());
    steer_all_live_workers(state, run_id, &command_id, text).await
}

async fn steer_all_live_workers(
    state: &Arc<AppState>,
    run_id: &str,
    command_id: &str,
    text: &str,
) -> (usize, usize) {
    let generation = state
        .store
        .get("runs", run_id)
        .ok()
        .flatten()
        .and_then(|r| r["execution_generation"].as_u64())
        .unwrap_or(1);
    let ids: Vec<String> = state
        .runs
        .handle(state, run_id)
        .workers
        .lock()
        .unwrap()
        .values()
        .filter(|worker| {
            !worker.abort.is_finished()
                && worker::worker_execution_generation(&worker.solver_id) == generation
                && !matches!(worker.lane.as_str(), "review" | "verifier" | "followup")
        })
        .map(|worker| worker.solver_id.clone())
        .collect();
    let outcomes = futures_util::future::join_all(ids.iter().map(|id| {
        let target = format!("worker:{id}");
        async move {
            let key = json!([run_id, command_id, id]).to_string();
            if state
                .store
                .get("run_guidance_deliveries", &key)
                .ok()
                .flatten()
                .is_some()
            {
                return Some(true);
            }
            let delivered = steer_live_worker(state, run_id, &target, text).await;
            if delivered == Some(true) {
                if let Err(error) = state.store.put(
                    "run_guidance_deliveries",
                    &key,
                    &json!({"run_id":run_id,"command_id":command_id,"worker_id":id}),
                ) {
                    tracing::error!(%error,"Guidance delivery persistence failed");
                    return Some(false);
                }
            }
            delivered
        }
    }))
    .await;
    (
        outcomes
            .iter()
            .filter(|outcome| **outcome == Some(true))
            .count(),
        ids.len(),
    )
}

async fn steer_live_worker(
    state: &Arc<AppState>,
    run_id: &str,
    target: &str,
    text: &str,
) -> Option<bool> {
    let worker_id = target
        .strip_prefix("worker:")
        .or_else(|| target.strip_prefix("solver:"))?;
    let handle = state.runs.handle(state, run_id);
    let worker = handle
        .workers
        .lock()
        .unwrap()
        .get(worker_id)
        .map(|worker| (worker.thread_id.clone(), worker.cancel.is_some()));
    let Some((thread_id, container)) = worker else {
        return None;
    };
    let request_id = format!("control-steer-{}", uuid::Uuid::new_v4().simple());
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        if container {
            let (link, container_worker_id) = loop {
                if let Some(session) = handle
                    .container_sessions
                    .lock()
                    .unwrap()
                    .get(worker_id)
                    .cloned()
                {
                    break session;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            let (reply, ack) = oneshot::channel();
            handle
                .control_steer_acks
                .lock()
                .unwrap()
                .insert(request_id.clone(), reply);
            let command = json!({"id":request_id,"type":"steer","message":text}).to_string();
            if link.write_stdin(&container_worker_id, &command).await != Ok(true) {
                return false;
            }
            return ack.await.unwrap_or(false);
        }
        let session = loop {
            if let Some(session) = state.agents.get(&thread_id).await {
                break session;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let (reply, ack) = oneshot::channel();
        handle
            .control_steer_acks
            .lock()
            .unwrap()
            .insert(request_id.clone(), reply);
        if session
            .send(SessionCommand::Steer {
                request_id: request_id.clone(),
                text: text.to_owned(),
                attachments: Vec::new(),
            })
            .await
            .is_err()
        {
            return false;
        }
        ack.await.unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    handle
        .control_steer_acks
        .lock()
        .unwrap()
        .remove(&request_id);
    Some(outcome)
}

pub(crate) async fn control_run(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    control_run_inner(state, run_id, body, false).await
}

pub(crate) async fn control_run_trusted(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    control_run_inner(state, run_id, body, true).await
}

async fn control_run_inner(
    state: Arc<AppState>,
    run_id: String,
    mut body: Value,
    trusted_actor: bool,
) -> Response {
    if !matches!(
        body["action"].as_str(),
        Some("stop" | "emergency_stop" | "pause" | "freeze")
    ) {
        if let Err(error) = crate::business::require_run(&state, &run_id) {
            return error.into_response();
        }
    }
    if matches!(body["action"].as_str(), Some("ask" | "writeup")) {
        if body["action"] == "writeup"
            && state
                .store
                .get("runs", &run_id)
                .ok()
                .flatten()
                .is_some_and(|r| r["running"] == true)
        {
            return followup::error(
                StatusCode::CONFLICT,
                "run.control.invalid_phase",
                "任务仍在运行中，请等待结束后再生成 Writeup",
            );
        }
        return followup::submit(State(state),Path(run_id),Json(json!({"kind":body["action"],"question":body["text"].as_str().or_else(||body["question"].as_str()).or_else(||body["hint"].as_str()).unwrap_or(""),"request_id":body["command_id"],"context_hint":body["context_hint"],"template_id":body["template_id"],"custom_prompt":body["custom_prompt"],"custom_template":body["custom_template"]}))).await;
    }
    let run_handle = state.runs.handle(&state, &run_id);
    let _lifecycle_guard = run_handle.delete_lock.lock().await;
    if run_handle.deleting.load(Ordering::SeqCst)
        && !matches!(
            body["action"].as_str(),
            Some("stop" | "emergency_stop" | "force_cancel")
        )
    {
        return Json(json!({"ok":false,"detail":"Run deletion is settling"})).into_response();
    }
    match state.store.get("runs", &run_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error":{"code":"run.not_found","message":"Run not found"}})),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":{"code":"storage.failed","message":error.to_string()}})),
            )
                .into_response();
        }
    };
    if let Err((status, error)) = control_contract::normalize(&run_id, &mut body) {
        return (status, Json(error)).into_response();
    }

    let action = body["action"]
        .as_str()
        .unwrap_or("directive")
        .trim()
        .to_owned();
    if matches!(
        action.as_str(),
        "hint" | "focus" | "redirect" | "directive" | "correction"
    ) && state
        .store
        .get("runs", &run_id)
        .ok()
        .flatten()
        .is_some_and(|r| r["running"] != true)
    {
        return followup::error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "run.control.resolve_required",
            "任务已结束，请点击「带着新思路继续」重新开启任务",
        );
    }
    let target = body["scope"]
        .as_str()
        .or_else(|| body["target"].as_str())
        .unwrap_or("global")
        .trim()
        .to_owned();
    let redirect_url = body["url"]
        .as_str()
        .or_else(|| body["target_url"].as_str())
        .unwrap_or("");
    let text = body["text"]
        .as_str()
        .or_else(|| body["message"].as_str())
        .or_else(|| body["hint"].as_str())
        .unwrap_or(redirect_url)
        .trim()
        .to_owned();
    let command_id = body["command_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4().simple()));
    let actor: String = (if trusted_actor {
        body["actor"].as_str()
    } else {
        None
    })
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .unwrap_or("operator")
    .chars()
    .take(256)
    .collect();
    if let Some(fields) = body.as_object_mut() {
        fields.insert("actor".to_owned(), json!(actor.clone()));
    }
    if body
        .get("secret")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"ok":false,"detail":"secret must be a string"})),
        )
            .into_response();
    }
    if matches!(
        action.as_str(),
        "graceful_drain" | "clear_standing" | "reset_guidance"
    ) && !matches!(target.as_str(), "global")
        && target != format!("run:{run_id}")
        && target != format!("challenge:{run_id}")
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":format!("{action} requires a run-wide scope")})),
        )
            .into_response();
    }
    let force_scope = if action == "force_cancel" {
        let selected = if target == "global" {
            ("global", "")
        } else if let Some((kind, value)) = target.split_once(':') {
            (kind, value)
        } else {
            ("", "")
        };
        if !matches!(
            selected.0,
            "global" | "run" | "challenge" | "worker" | "solver" | "engine" | "intent" | "lane"
        ) || (selected.0 == "global" && !selected.1.is_empty())
            || (selected.0 != "global" && selected.1.is_empty())
            || (matches!(selected.0, "run" | "challenge") && selected.1 != run_id)
        {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail":"invalid force_cancel scope"})),
            )
                .into_response();
        }
        let worker_id = body["worker_id"].as_str().unwrap_or("").trim();
        if selected.0 == "global" && !worker_id.is_empty() {
            Some(("worker".to_owned(), worker_id.to_owned()))
        } else {
            Some((selected.0.to_owned(), selected.1.to_owned()))
        }
    } else {
        None
    };

    let conn = match open_control_db(&state, &run_id) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "detail": e})),
            )
                .into_response();
        }
    };

    if let Err(error) = control_journal::recover(&state, &run_id, &conn) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":{"code":"run.control.recovery_failed","message":error}})),
        )
            .into_response();
    }
    let mut run_doc = match state.store.get("runs", &run_id) {
        Ok(Some(doc)) => doc,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error.to_string()})),
            )
                .into_response();
        }
    };
    let revoked = match execution_guard::is_revoked(&state, &run_id, &run_doc) {
        Ok(value) => value,
        Err(error) => return crate::control::storage(error).into_response(),
    };
    if !revoked && !matches!(action.as_str(), "stop" | "complete" | "emergency_stop") {
        match runner_stop_recovery::pending(&state, &run_doc) {
            Ok(false) => {
                if let Err(error) = control_freeze::recover_pending(&state, &run_id).await {
                    tracing::warn!(run_id=%run_id,%error,"Prior freeze recovery remains pending");
                }
                run_doc = match state.store.get("runs", &run_id) {
                    Ok(Some(run)) => run,
                    Ok(None) => return StatusCode::NOT_FOUND.into_response(),
                    Err(error) => return crate::control::storage(error).into_response(),
                };
            }
            Ok(true) => {}
            Err(error) => return crate::control::storage(error).into_response(),
        }
    }
    let before_control = run_doc.clone();

    if let Ok((existing_payload, existing_resp)) = conn.query_row(
        "SELECT payload_json, response_json FROM control_commands WHERE command_id = ?1",
        params![command_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ) && let Ok(parsed) = serde_json::from_str::<Value>(&existing_resp)
    {
        let mut prior = serde_json::from_str::<Value>(&existing_payload).unwrap_or(Value::Null);
        let mut current = body.clone();
        if prior["target"].is_null() {
            prior["target"] = json!(target);
        }
        if current["target"].is_null() {
            current["target"] = json!(target);
        }
        if let Some(secret_id) = prior["secret_id"].as_str() {
            let saved = fs::read(
                control_dir(&state, &run_id)
                    .join("secrets")
                    .join(format!("{secret_id}.secret")),
            );
            if !saved.as_ref().ok().is_some_and(|bytes| {
                current["secret"]
                    .as_str()
                    .is_some_and(|secret| bytes.as_slice() == secret.as_bytes())
            }) {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"detail":"command_id was used with different control content"})),
                )
                    .into_response();
            }
            prior
                .as_object_mut()
                .map(|fields| fields.remove("secret_id"));
        }
        if current["secret"].is_string() && prior["secret"] == "[REDACTED]" {
            current["secret"] = json!("[REDACTED]");
        }
        if prior != current {
            return (
                StatusCode::CONFLICT,
                Json(json!({"detail":"command_id was used with different control content"})),
            )
                .into_response();
        }
        if parsed["status"] == "unknown"
            && matches!(
                parsed["effect"]["kind"].as_str(),
                Some("live_delivery_outcome_unknown" | "context_delivery_outcome_unknown")
            )
            && state.runs.claim_execution_owner(&state, &run_id).ok() == Some(true)
            && !read_jsonl_event_iter(&state, &run_id).any(|event| {
                event["event_type"] == "control.command"
                    && event["payload"]["command_id"] == command_id
                    && event["payload"]["status"] == "unknown"
            })
        {
            emit_event(
                &state,
                &run_id,
                None,
                "control.command",
                json!({
                    "command_id":command_id,"action":action,"target":target,
                    "status":"unknown","generation":parsed["generation"],
                    "code":"recovery_incomplete","reexecuted":false,
                    "effect":{"kind":"delivery_unknown"},"target_ids":[]
                }),
            );
        }
        return Json(parsed).into_response();
    }

    if revoked && !matches!(action.as_str(), "stop" | "emergency_stop") {
        return execution_conflict("guard.revoked", "Run egress is revoked");
    }
    if let Err((status, error)) = control_contract::admit(&run_id, &run_doc, &body, now_secs()) {
        return (status, Json(error)).into_response();
    }
    if action == "spawn_worker"
        && let Err(response) = check_stop_pending(&state, &run_doc)
    {
        return response;
    }

    match state.runs.claim_execution_owner(&state, &run_id) {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({"ok":false,"detail":"Run is owned by another service process"})),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok":false,"detail":error})),
            )
                .into_response();
        }
    }

    let current_gen = run_doc["control_generation"].as_u64().unwrap_or(0);
    let is_lifecycle = matches!(
        action.as_str(),
        "pause"
            | "freeze"
            | "resume"
            | "thaw"
            | "stop"
            | "emergency_stop"
            | "complete"
            | "graceful_drain"
    );
    if let Some(expected) = body["expected_generation"].as_u64()
        && expected != current_gen
    {
        if action == "force_cancel" {
            let detail =
                format!("control_generation conflict: expected {expected}, current {current_gen}");
            let rejected = json!({"ok":false,"command_id":command_id,
                "status":"rejected","code":"generation_conflict",
                "detail":detail,"generation":current_gen});
            let mut rejected_payload = body.clone();
            if rejected_payload["secret"].is_string() {
                rejected_payload["secret"] = json!("[REDACTED]");
            }
            if let Err(error) = conn.execute(
                "INSERT INTO control_commands
                 (command_id, action, target, status, generation, payload_json, response_json, created_at)
                 VALUES (?1, ?2, ?3, 'rejected', ?4, ?5, ?6, ?7)",
                params![&command_id, &action, &target, current_gen as i64,
                    rejected_payload.to_string(), rejected.to_string(), now_secs()],
            ) {
                return (StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail":error.to_string()}))).into_response();
            }
            emit_event(
                &state,
                &run_id,
                None,
                "control.command",
                json!({"command_id":command_id,"action":action,"target":target,
                    "status":"rejected","generation":current_gen,
                    "code":"generation_conflict","target_ids":[]}),
            );
            return (StatusCode::CONFLICT, Json(rejected)).into_response();
        }
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "detail": format!("control_generation conflict: expected {expected}, current {current_gen}"),
                "generation": current_gen,
            })),
        )
            .into_response();
    }

    let mut journal_payload = body.clone();
    if let Some(sec) = body["secret"].as_str().filter(|s| !s.is_empty()) {
        let sec_id = match store_control_secret(&state, &run_id, sec) {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok":false,"detail":error})),
                )
                    .into_response();
            }
        };
        journal_payload["secret"] = json!("[REDACTED]");
        journal_payload["secret_id"] = json!(sec_id);
    }

    let lifecycle_scope =
        target == "global" || target.starts_with("run:") || target.starts_with("challenge:");
    let next_gen =
        if is_lifecycle && (!matches!(action.as_str(), "freeze" | "thaw") || lifecycle_scope) {
            current_gen + 1
        } else {
            current_gen
        };
    run_doc["control_generation"] = json!(next_gen);

    let handle = state.runs.handle(&state, &run_id);
    let mut decision_closed = false;
    let mut lifecycle_state: Option<String> = None;
    let mut effect_observed = true;
    let mut force_cancel_targets = Vec::new();
    let mut force_cancel_failures = Vec::new();
    let mut force_cancel_state = "effect_observed";
    let mut context_effect = Value::Null;
    let mut context_queued = false;

    let live_worker = target
        .strip_prefix("worker:")
        .or_else(|| target.strip_prefix("solver:"))
        .is_some_and(|worker_id| handle.workers.lock().unwrap().contains_key(worker_id));
    let live_context_text = body["context"]["content"]
        .as_str()
        .or_else(|| body["context"].as_str())
        .unwrap_or("");
    let live_stdin_delivery = live_worker
        && if action == "add_context" {
            !live_context_text.is_empty()
                && !live_context_text.starts_with("secret://")
                && body["context"]["kind"] != "secret_ref"
        } else {
            matches!(
                action.as_str(),
                "ask" | "hint" | "focus" | "redirect" | "directive" | "correction"
            ) && !text.is_empty()
                && !text.starts_with("secret://")
        };
    let durable_context = action == "add_context"
        || (matches!(
            action.as_str(),
            "ask" | "hint" | "focus" | "redirect" | "directive" | "correction"
        ) && !text.is_empty());
    {
        let pending_kind = if live_stdin_delivery {
            "live_delivery_outcome_unknown"
        } else if durable_context {
            "context_delivery_outcome_unknown"
        } else {
            "control_effect_outcome_unknown"
        };
        let pending = json!({"ok":false,"command_id":command_id,"status":"unknown",
            "generation":next_gen,"code":"recovery_incomplete","reexecuted":false,
            "effect":{"kind":pending_kind,"target":target}});
        if action == "emergency_stop" {
            let (approvals, unsettled) = match execution_guard::admit_stop(
                &conn,
                &before_control,
                &journal_payload,
                &command_id,
                &pending,
            ) {
                Ok(result) => result,
                Err(error) => return error.into_response(),
            };
            let generation = before_control["execution_generation"].clone();
            emit_event(
                &state,
                &run_id,
                None,
                "guard.egress.state_changed",
                json!({"egress_state":"revoked","execution_generation":generation,"command_id":command_id}),
            );
            for aid in approvals {
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "guard.approval.resolved",
                    json!({"approval_id":aid,"action":"emergency_stop","execution_generation":generation}),
                );
            }
            for call in unsettled {
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "guard.action.settled",
                    json!({"tool_call_id":call,"receipt_id":call,"status":"outcome_unknown","delivery_status":"outcome_unknown","retry_safe":false,"execution_generation":generation}),
                );
            }
            return match execution_guard::stop(
                &state,
                &run_id,
                conn,
                &before_control,
                &command_id,
                next_gen,
            )
            .await
            {
                Ok(response) => Json(response).into_response(),
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"command_id":command_id,"status":"unknown","detail":error})),
                )
                    .into_response(),
            };
        }
        if let Err(error) = conn.execute(
            "INSERT INTO control_commands
             (command_id,action,target,status,generation,payload_json,response_json,created_at)
             VALUES (?1,?2,?3,'unknown',?4,?5,?6,?7)",
            params![
                command_id,
                action,
                target,
                next_gen as i64,
                journal_payload.to_string(),
                pending.to_string(),
                now_secs()
            ],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok":false,"detail":error.to_string()})),
            )
                .into_response();
        }
    }

    if matches!(action.as_str(), "stop" | "complete") {
        let intent = json!({"command_id":command_id,"run_id":run_id,"generation":run_doc["execution_generation"],"control_generation":next_gen,"action":action,"text":text,"state":"pending"});
        if let Err(error) = state.store.put("runner_stop_intents", &command_id, &intent) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error.to_string()})),
            )
                .into_response();
        }
    }

    let authoritative_id = format!("dir-{command_id}");
    let standing = body["standing"].as_bool().unwrap_or(true);
    if matches!(
        action.as_str(),
        "hint" | "focus" | "redirect" | "directive" | "correction"
    ) {
        body["directive_id"] = json!(authoritative_id);
        if run_doc["kind"] != "local_pi" {
            let graph = match graph::SharedGraph::open(&state, &run_id) {
                Ok(g) => g,
                Err(e) => {
                    return followup::error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "run.control.graph_failed",
                        &e,
                    );
                }
            };
            if !graph
                .directives()
                .iter()
                .any(|d| d["directive_id"] == authoritative_id)
                && graph.add_operator_directive(
                    &authoritative_id,
                    &actor,
                    &target,
                    &action,
                    &text,
                    standing,
                    "soft_rebind",
                ) <= 0
            {
                return followup::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "run.control.graph_failed",
                    "Could not persist directive",
                );
            }
        }
        if let Err(e) = control_context::create_guidance(
            &conn,
            &run_id,
            &command_id,
            &target,
            &target,
            &action,
            &body,
        ) {
            return followup::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "run.control.context_failed",
                &e,
            );
        }
    }

    if run_doc["kind"].as_str() == Some("local_pi")
        && matches!(
            action.as_str(),
            "stop"
                | "complete"
                | "interrupt_turn"
                | "interrupt"
                | "pause"
                | "continue"
                | "resume"
                | "send_message"
                | "directive"
                | "hint"
                | "ask"
                | "focus"
                | "redirect"
                | "correction"
        )
    {
        match action.as_str() {
            "stop" | "complete" => {
                if local_pi::dispatch_live_control(
                    &run_id,
                    local_pi::LocalPiControl::Stop {
                        command_id: command_id.clone(),
                    },
                )
                .await
                .is_err()
                {
                    if !run_doc
                        .get("local_pi_session")
                        .is_some_and(Value::is_object)
                    {
                        run_doc["local_pi_session"] = json!({});
                    }
                    run_doc["local_pi_session"]["session_state"] = json!("stopped");
                    run_doc["local_pi_session"]["turn_active"] = json!(false);
                    run_doc["local_pi_session"]["queued_messages"] = json!(0);
                    mark_run_finished(
                        &state,
                        &run_id,
                        Some(local_pi::LOCAL_PI_SOLVER_ID),
                        action == "complete" || run_doc["solved"] == true,
                        if action == "complete" {
                            "goal_met"
                        } else {
                            "operator_stop"
                        },
                        Some(&text),
                    );
                    if let Ok(Some(updated)) = state.store.get("runs", &run_id) {
                        run_doc = updated;
                    }
                    emit_event(
                        &state,
                        &run_id,
                        Some(local_pi::LOCAL_PI_SOLVER_ID),
                        "control.receipt",
                        json!({
                            "command_id": command_id,
                            "action": action,
                            "solver_id": local_pi::LOCAL_PI_SOLVER_ID,
                            "main_thread": true,
                            "status": "applied",
                            "state": "effect_observed",
                            "detail": "本地 Pi 单会话已停止，待处理消息已清空",
                            "ts": now_secs(),
                        }),
                    );
                }
                run_doc["control_mode"] = json!("terminated");
                effect_observed = true;
                context_effect = json!({"kind": "local_pi_stopped"});
            }
            "interrupt_turn" | "interrupt" | "pause" => {
                if local_pi::dispatch_live_control(
                    &run_id,
                    local_pi::LocalPiControl::InterruptTurn {
                        command_id: command_id.clone(),
                        action: action.clone(),
                    },
                )
                .await
                .is_ok()
                {
                    effect_observed = true;
                    context_effect = json!({"kind": "local_pi_interrupt_requested"});
                } else {
                    emit_event(
                        &state,
                        &run_id,
                        Some(local_pi::LOCAL_PI_SOLVER_ID),
                        "control.receipt",
                        json!({
                            "command_id": command_id,
                            "action": action,
                            "solver_id": local_pi::LOCAL_PI_SOLVER_ID,
                            "main_thread": true,
                            "status": "applied",
                            "state": "effect_observed",
                            "detail": "当前无进行中的生成轮次，会话已处于可继续状态",
                            "ts": now_secs(),
                        }),
                    );
                    effect_observed = true;
                    context_effect = json!({"kind": "local_pi_already_idle"});
                }
            }
            "continue" | "resume" => {
                if let Err(err) = local_pi::validate_saved_resume(&state, &run_id, &run_doc) {
                    local_pi::record_resume_failure(
                        &state,
                        &run_id,
                        &err,
                        Some((&command_id, &action)),
                    );
                    let _ = conn.execute(
                        "DELETE FROM control_commands WHERE command_id=?1 AND status='unknown'",
                        [&command_id],
                    );
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({
                            "ok": false,
                            "code": "local_pi.resume_failed",
                            "detail": err,
                        })),
                    )
                        .into_response();
                }
                let opt_text = (!text.is_empty()).then(|| text.clone());
                if local_pi::dispatch_live_control(
                    &run_id,
                    local_pi::LocalPiControl::Continue {
                        command_id: command_id.clone(),
                        action: action.clone(),
                        text: opt_text.clone(),
                    },
                )
                .await
                .is_err()
                {
                    let exec_gen = run_doc["execution_generation"].as_u64().unwrap_or(1);
                    local_pi::spawn_local_pi_session(
                        state.clone(),
                        run_id.clone(),
                        local_pi::LocalPiSpawnOptions {
                            execution_generation: exec_gen,
                            resume_required: true,
                            initial_prompt: opt_text,
                            triggered_by_command: Some((command_id.clone(), action.clone())),
                        },
                    );
                }
                run_doc["status"] = json!("running");
                run_doc["running"] = json!(true);
                run_doc["finished"] = json!(false);
                run_doc["awaiting_operator"] = Value::Null;
                effect_observed = true;
                context_effect = json!({"kind": "local_pi_continued"});
            }
            _ => {
                if local_pi::dispatch_live_control(
                    &run_id,
                    local_pi::LocalPiControl::UserMessage {
                        command_id: command_id.clone(),
                        action: action.clone(),
                        text: text.clone(),
                        steer: body["steer"].as_bool().unwrap_or(false),
                    },
                )
                .await
                .is_ok()
                {
                    effect_observed = true;
                    context_effect = json!({"kind": "local_pi_message_delivered"});
                } else {
                    if let Err(err) = local_pi::validate_saved_resume(&state, &run_id, &run_doc) {
                        local_pi::record_resume_failure(
                            &state,
                            &run_id,
                            &err,
                            Some((&command_id, &action)),
                        );
                        let _ = conn.execute(
                            "DELETE FROM control_commands WHERE command_id=?1 AND status='unknown'",
                            [&command_id],
                        );
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "ok": false,
                                "code": "local_pi.resume_failed",
                                "detail": err,
                            })),
                        )
                            .into_response();
                    }
                    let exec_gen = run_doc["execution_generation"].as_u64().unwrap_or(1);
                    local_pi::spawn_local_pi_session(
                        state.clone(),
                        run_id.clone(),
                        local_pi::LocalPiSpawnOptions {
                            execution_generation: exec_gen,
                            resume_required: true,
                            initial_prompt: Some(text.clone()),
                            triggered_by_command: Some((command_id.clone(), action.clone())),
                        },
                    );
                    run_doc["status"] = json!("running");
                    run_doc["running"] = json!(true);
                    run_doc["finished"] = json!(false);
                    run_doc["awaiting_operator"] = Value::Null;
                    effect_observed = true;
                    context_effect = json!({"kind": "local_pi_session_resumed_with_message"});
                }
            }
        }
    } else {
        match action.as_str() {
        "freeze" | "thaw" => {
            match control_freeze::apply(&state,&run_id,&command_id,&body,&mut run_doc).await {
                Ok(effect)=>{effect_observed=effect["state"]=="effect_observed";lifecycle_state=effect["state"].as_str().map(str::to_owned);context_effect=effect;},
                Err(error)=>return(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":{"code":"run.control.freeze_failed","message":error},"command_id":command_id,"status":"unknown"}))).into_response(),
            }
        }
        "pause" => {
            run_doc["control_mode"]=json!("quiesced");
            handle.paused.store(true, Ordering::SeqCst);
            run_doc["awaiting_operator"] = json!("paused");
            emit_event(
                &state,
                &run_id,
                None,
                "blackboard.delta",
                json!({"kind": "operator_paused"}),
            );
        }
        "resume" => {
            if run_doc["freeze_control"].is_object(){let mut thaw=body.clone();thaw["action"]=json!("thaw");match control_freeze::apply(&state,&run_id,&command_id,&thaw,&mut run_doc).await{Ok(effect)if effect["state"]=="effect_observed"=>{},Ok(effect)=>return(StatusCode::CONFLICT,Json(effect)).into_response(),Err(error)=>return(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"detail":error}))).into_response()}}
            run_doc["control_mode"]=json!("active");
            handle.paused.store(false, Ordering::SeqCst);
            if action == "resume" {
                handle.draining.store(false, Ordering::SeqCst);
            }
            run_doc["awaiting_operator"] = Value::Null;
            emit_event(
                &state,
                &run_id,
                None,
                "blackboard.delta",
                json!({"kind": "operator_resumed"}),
            );
        }
        "stop" | "complete" => {
            let tids = state.runs.abort_all_workers(&run_id);
            for tid in tids {
                state.agents.close(&tid).await;
            }
            let teardown = container_exec::teardown_run_container_recorded(&state, &run_id).await;
            let teardown = if let Err(error) = teardown {
                let execution = run_doc["execution_generation"].as_u64().unwrap_or(0);
                runner_stop_recovery::confirm_queued_stop(&state, &handle, &run_id, execution).await
                    .and_then(|confirmed| confirmed.then_some(()).ok_or(error))
            } else {
                teardown
            };
            match teardown {
                Err(error)=>{
                    let confirmed=state.store.list("runner_controller_runs").ok().is_some_and(|rows|rows.iter().any(|row|row["run_id"]==run_id&&row["generation"]==run_doc["execution_generation"]&&row["state"]=="stopped"));
                    effect_observed=false;lifecycle_state=Some(if confirmed{"partial"}else{"unknown"}.into());
                    context_effect=json!({"kind":if confirmed{"stop_confirmed_artifacts_pending"}else{"stop_unconfirmed"},"process_exit_confirmed":confirmed,"error":error});
                    run_doc["status"]=json!("stopping");run_doc["awaiting_operator"]=json!("stop_confirmation");
                },
                Ok(())=>{
            let tasks=handle.worker_tasks.lock().unwrap().clone();
            if !wait_for_tasks(&tasks,main_runtime_cancel_timeout()).await {
                return (StatusCode::CONFLICT,Json(json!({"code":"run.stop_unconfirmed","detail":"Worker tasks have not stopped"}))).into_response();
            }
            handle.paused.store(false, Ordering::SeqCst);
            handle.draining.store(false, Ordering::SeqCst);
            if let Err(error)=worker_result::settle_claims(&state,&run_id,"canceled","operator_stop") {
                return (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"code":"worker.result_commit_failed","detail":error}))).into_response();
            }
            if let Err(error)=control_freeze::settle_stopped(&state,&run_id,&mut run_doc){context_effect=json!({"kind":"stopped_lease_cleanup_pending","error":error});}
            let stopped_leases = run_doc.clone();
            let solved = action == "complete" || run_doc["solved"] == true;
            let reason = if action == "complete" {
                "goal_met"
            } else {
                "operator_stop"
            };
            if let Err(error)=terminal::finish(&state,&run_id,run_doc["execution_generation"].as_u64().unwrap_or(1),None,solved,reason,Some(&text)){return(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"detail":error,"command_id":command_id,"status":"unknown"}))).into_response();}
            if let Ok(Some(updated)) = state.store.get("runs", &run_id) {
                run_doc = updated;
                run_doc["control_generation"] = json!(next_gen);
                for key in ["freeze_control","budget_suspend_started_at","budget_suspended_seconds","worker_suspended_seconds","stopped_lease_cleanup"]{run_doc[key]=stopped_leases[key].clone();}
                run_doc["control_mode"]=json!("terminated");
            }
                },
            }
        }
        "graceful_drain" => {
            let live = handle.coordinator_abort.lock().unwrap().as_ref()
                .is_some_and(|coordinator| !coordinator.is_finished());
            if live {
                handle.draining.store(true, Ordering::SeqCst);
                run_doc["awaiting_operator"] = json!("draining");
            } else {
                effect_observed = false;
            }
        }
        "force_cancel" => {
            let (scope_kind, scope_value) = force_scope.as_ref().expect("force_cancel scope");
            let outcome = state.runs.request_worker_cancel(&run_id, scope_kind, scope_value);
            force_cancel_targets = outcome.requested;
            force_cancel_failures = outcome.failures;
            force_cancel_state = if force_cancel_targets.is_empty() {
                "unknown"
            } else if force_cancel_failures.is_empty() {
                "effect_observed"
            } else {
                "partial"
            };
            for solver_id in &force_cancel_targets {
                emit_event(&state, &run_id, Some(solver_id), "worker.status",
                    json!({"online":true,"status":"cancel_requested",
                        "reason":"operator_force_cancel","process_exit_confirmed":false}));
            }
        }
        "spawn_worker" | "cancel_worker" => {
            let mut worker_body = body.clone();
            if action == "cancel_worker" && worker_body["worker_id"].is_null()
                && worker_body["solver_id"].is_null()
                && let Some(worker_id) = target.strip_prefix("worker:") {
                worker_body["worker_id"] = json!(worker_id);
            }
            let response = if action == "spawn_worker" {
                spawn_worker_effect(&state, &run_id, &worker_body).await
            } else {
                kill_worker_effect(&state, &run_id, &worker_body).await
            };
            let status = response.status();
            let effect: Value = match axum::body::to_bytes(response.into_body(), usize::MAX).await
                .ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()) {
                Some(value) => value,
                None => return (StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok":false,"detail":"Worker control response is invalid"}))).into_response(),
            };
            if !status.is_success() {
                return (status, Json(effect)).into_response();
            }
            effect_observed = effect["ok"] == true;
            context_effect = json!({"kind":if effect_observed {
                if action == "spawn_worker" {"worker_spawn_requested"} else {"worker_cancel_requested"}
            } else {"worker_control_unavailable"},"detail":effect});
        }
        "mark_false" => {
            let bad_flag = body["flag"]
                .as_str()
                .or_else(|| body["text"].as_str())
                .unwrap_or("")
                .trim()
                .to_owned();
            if !bad_flag.is_empty() {
                let reopened=match graph::SharedGraph::open(&state,&run_id).and_then(|mut graph|graph.reopen_after_false_positive(&actor,&bad_flag,"marked_false")){
                    Ok(reopened)=>reopened,Err(error)=>return(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"detail":error}))).into_response(),
                };
                context_effect=json!({"kind":"flag_invalidated","recovery":"full_coordinator","reopened":reopened,"resolve_scheduled":true});
                let remaining: Vec<String> = run_doc["flags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .filter(|f| f != &bad_flag)
                    .collect();
                run_doc["flag"] = remaining
                    .first()
                    .cloned()
                    .map(|f| json!(f))
                    .unwrap_or(Value::Null);
                run_doc["flags"] = json!(remaining);
                run_doc["solved"] = json!(false);

            }
        }
        "answer_decision" | "submit" | "dismiss" | "dismiss_help" => {
            match control_decisions::apply(&state,&run_id,&command_id,&body,&actor,&mut run_doc).await {
                Ok((observed,effect))=>{effect_observed=observed;decision_closed=observed;context_effect=effect;},
                Err(error)=>return(StatusCode::UNPROCESSABLE_ENTITY,Json(json!({"error":{"code":"run.control.decision_failed","message":error},"command_id":command_id,"status":"unknown"}))).into_response(),
            }
        }
        "add_context" => {
            let context_text = body["context"]["content"].as_str()
                .or_else(|| body["context"].as_str()).unwrap_or("");
            let live_delivery = if !context_text.is_empty()
                && !context_text.starts_with("secret://")
                && body["context"]["kind"] != "secret_ref" {
                steer_live_worker(&state,&run_id,&target,context_text).await
            } else { None };
            if let Some(delivered) = live_delivery {
                effect_observed = delivered;
                context_effect = json!({"kind":if delivered {
                    "live_context_delivered"
                } else { "live_context_unavailable" },"target":target});
            } else {
                match context_delivery_scope(&conn,&handle,&target,&command_id) {
                    Ok(Some((scope,continuation))) => {
                        match control_context::create(&conn, &run_id, &command_id,
                            &target, &scope, &body) {
                            Ok(resource) => {
                                if let Some(intent_id) = continuation {
                                    context_queued = propose_live_context_continuation(
                                        &state,&run_id,&handle,&resource);
                                    effect_observed = false;
                                    context_effect = json!({"kind":if context_queued {
                                            "exact_continuation_queued"
                                        } else { "exact_continuation_unavailable" },
                                        "context_id":resource["context_id"],
                                        "continuation_intent_id":intent_id});
                                } else {
                                    context_effect = json!({"kind":"context_created",
                                        "context_id":resource["context_id"],
                                        "standing":resource["standing"]});
                                }
                                emit_event(&state, &run_id, None, "control.context",
                                    json!({"operation":"created","context_id":resource["context_id"],
                                        "command_id":command_id,"scope":scope,
                                        "standing":resource["standing"]}));
                            }
                            Err(error) => {
                                let _ = conn.execute(
                                    "DELETE FROM control_commands WHERE command_id=?1 AND status='unknown'",
                                    [&command_id],
                                );
                                return (StatusCode::UNPROCESSABLE_ENTITY,
                                    Json(json!({"ok":false,"detail":error}))).into_response();
                            }
                        }
                    }
                    Ok(None) => {
                        effect_observed = false;
                        context_effect = json!({"kind":"context_target_unavailable","target":target});
                    }
                    Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"ok":false,"detail":error}))).into_response(),
                }
            }
        }
        "expire_context" => {
            let context_id = body["context_id"].as_str().unwrap_or("").trim();
            if context_id.is_empty() {
                return (StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"ok":false,"detail":"context_id is required"}))).into_response();
            }
            match control_context::expire(&conn, &run_id, &command_id, context_id, &actor,
                body["reason"].as_str().unwrap_or("operator command")) {
                Ok(changed) => {
                    let graph_expired = match graph::SharedGraph::open(&state, &run_id)
                        .and_then(|graph| graph.expire_directive(context_id, &command_id)) {
                        Ok(value) => value,
                        Err(error) => {
                            effect_observed = false;
                            emit_event(&state, &run_id, None, "control.context",
                                json!({"operation":"context_expire_partial",
                                    "command_id":command_id,"detail":error}));
                            false
                        }
                    };
                    run_doc["guidance_revision"]=json!(run_doc["guidance_revision"].as_u64().unwrap_or(0)+1);
                    emit_event(&state,&run_id,None,"blackboard.delta",json!({"kind":"operator_directive_changed","directive_id":context_id,"status":"expired"}));
                    handle.directives.lock().unwrap().retain(|directive|
                        directive["directive_id"] != context_id);
                    context_effect = json!({"kind":"context_expired","context_id":context_id,
                        "expired_context_count":if changed {1} else {0},
                        "expired_directive":graph_expired});
                    emit_event(&state, &run_id, None, "control.context",
                        json!({"operation":"expired","context_id":context_id,
                            "command_id":command_id,"changed":changed}));
                }
                Err(error) => return (StatusCode::NOT_FOUND,
                    Json(json!({"ok":false,"detail":error}))).into_response(),
            }
        }
        "clear_standing" | "reset_guidance" => {
            let expired = match control_context::clear_standing(&state, &conn, &run_id,
                &command_id, &text, &actor, &action) {
                Ok(value) => value,
                Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok":false,"detail":error}))).into_response(),
            };
            let expired_ids: HashSet<String> = expired.iter()
                .filter_map(|resource| resource["context_id"].as_str().map(str::to_owned))
                .collect();
            let graph_expired = match graph::SharedGraph::open(&state, &run_id)
                .and_then(|graph| {
                    let mut matched = Vec::new();
                    for id in &expired_ids {
                        if graph.expire_directive(id,&command_id)? {
                            matched.push(id.clone());
                        }
                    }
                    if !text.starts_with("secret://") {
                        matched.extend(graph.expire_standing_directives(&text,&command_id)?);
                    }
                    matched.sort();
                    matched.dedup();
                    Ok(matched)
                }) {
                Ok(value) => value,
                Err(error) => {
                    effect_observed = false;
                    emit_event(&state, &run_id, None, "control.context",
                        json!({"operation":"standing_clear_partial",
                            "command_id":command_id,"detail":error,
                            "expired_context_count":expired.len()}));
                    Vec::new()
                }
            };
            run_doc["guidance_revision"]=json!(run_doc["guidance_revision"].as_u64().unwrap_or(0)+1);
            for id in &graph_expired {emit_event(&state,&run_id,None,"blackboard.delta",json!({"kind":"operator_directive_changed","directive_id":id,"status":"expired"}));}
            handle.directives.lock().unwrap().retain(|directive| {
                let id=directive["directive_id"].as_str().unwrap_or("");
                !expired_ids.contains(id)&&!graph_expired.iter().any(|expired|expired==id)
            });
            context_effect = json!({"kind":if effect_observed {
                    "guidance_cleared"
                } else { "guidance_clear_partial" },
                "expired_context_count":expired.len(),
                "expired_directives":graph_expired});
            emit_event(&state, &run_id, None, "control.context",
                json!({"operation":"standing_cleared","command_id":command_id,
                    "expired_context_count":expired.len(),
                    "expired_directives":graph_expired}));
        }
        _ => {
            let guidance_action = matches!(action.as_str(), "ask" | "hint" | "focus"
                | "redirect" | "directive" | "correction");
            let authoritative_id=format!("dir-{command_id}");
            let live_delivery = if guidance_action && !text.is_empty()
                && !text.starts_with("secret://") {
                steer_live_worker(&state,&run_id,&target,&text).await
            } else { None };
            if let Some(delivered) = live_delivery {
                effect_observed = delivered;
                context_effect = json!({"kind":if delivered {
                    "live_guidance_delivered"
                } else { "live_guidance_unavailable" },"target":target});
            }
            // Run-wide guidance also reaches Workers that are already mid-task; the
            // durable context below only informs Workers and planner passes that start later.
            let broadcast = if live_delivery.is_none()
                && matches!(action.as_str(), "hint" | "focus" | "redirect" | "directive" | "correction")
                && !text.is_empty() && !text.starts_with("secret://")
                && (target == "global" || target.starts_with("run:") || target.starts_with("challenge:")) {
                Some(steer_all_live_workers(&state,&run_id,&command_id,&text).await)
            } else { None };
            let mut guidance_unavailable = false;
            let companion = if guidance_action && live_delivery.is_none() {
                match context_delivery_scope(&conn,&handle,&target,&command_id) {
                    Ok(Some((scope,continuation))) => {
                        match control_context::create_guidance(&conn, &run_id, &command_id,
                            &target, &scope, &action, &body) {
                            Ok(resource) => {
                                if let (Some(intent_id),Some(resource)) =
                                    (continuation,resource.as_ref()) {
                                    context_queued = propose_live_context_continuation(
                                        &state,&run_id,&handle,resource);
                                    effect_observed = false;
                                    context_effect = json!({"kind":if context_queued {
                                            "exact_continuation_queued"
                                        } else { "exact_continuation_unavailable" },
                                        "context_id":resource["context_id"],
                                        "continuation_intent_id":intent_id});
                                }
                                resource
                            }
                            Err(error) => {
                                let _ = conn.execute(
                                    "DELETE FROM control_commands WHERE command_id=?1 AND status='unknown'",
                                    [&command_id],
                                );
                                return (StatusCode::UNPROCESSABLE_ENTITY,
                                    Json(json!({"ok":false,"detail":error}))).into_response();
                            }
                        }
                    }
                    Ok(None) => {
                        guidance_unavailable = true;
                        effect_observed = false;
                        context_effect = json!({"kind":"context_target_unavailable","target":target});
                        None
                    }
                    Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"ok":false,"detail":error}))).into_response(),
                }
            } else { None };
            if let Some((delivered,total)) = broadcast
                && context_effect.is_null() {
                context_effect = json!({"kind":"live_guidance_broadcast",
                    "delivered":delivered,"workers":total});
            }
            if !guidance_unavailable {
            let dir_id = companion.as_ref()
                .and_then(|resource| resource["context_id"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| authoritative_id.clone());
            let standing = body["standing"].as_bool().unwrap_or(false);
            let preempt = body["preempt_policy"]
                .as_str()
                .unwrap_or("soft_rebind")
                .to_owned();
            if live_delivery.is_none() || standing {
                handle.directives.lock().unwrap().push(json!({
                    "directive_id": dir_id,
                    "action": action,
                    "text": text,
                    "standing": standing,
                    "ts": now_secs(),
                }));
            }
            if let Some(resource) = companion {
                emit_event(&state, &run_id, None, "control.context",
                    json!({"operation":"created","context_id":resource["context_id"],
                        "command_id":command_id,"scope":target,"standing":standing}));
            }
            emit_event(
                &state,
                &run_id,
                Some("coordinator"),
                "coordinator.guidance",
                json!({
                    "target": target,
                    "guidance": text,
                    "standing": standing,
                    "preemption": preempt,
                }),
            );
            emit_event(
                &state,
                &run_id,
                None,
                "blackboard.delta",
                json!({
                    "kind": "operator_directive_changed",
                    "directive_id": dir_id,
                    "action": action,
                    "text": text,
                    "standing": standing,
                    "status": "applied",
                    "preemption": preempt,
                }),
            );
            }
        }
    }
    }

    run_doc["updated"] = json!(now_secs());

    let observed_paused = handle.paused.load(Ordering::SeqCst);
    emit_event(
        &state,
        &run_id,
        None,
        "hitl.response",
        json!({
            "target": target,
            "action": action,
            "text": text,
            "control_generation": next_gen,
            "observed_paused": observed_paused,
        }),
    );

    let req_id = body["request_id"].as_str();
    let control_effect_state = if let Some(state) = lifecycle_state.as_deref() {
        state
    } else if action == "force_cancel" {
        force_cancel_state
    } else if context_queued {
        "partial"
    } else if effect_observed {
        "effect_observed"
    } else {
        "unknown"
    };
    emit_event(
        &state,
        &run_id,
        None,
        "control.command",
        json!({
            "command_id": command_id,
            "action": action,
            "actor": actor,
            "target": target,
            "status": control_effect_state,
            "generation": next_gen,
            "request_id": req_id,
            "decision_closed": decision_closed,
            "observed_paused": observed_paused,
            "effect": if !context_effect.is_null() {
                context_effect.clone()
            } else if action == "graceful_drain" && effect_observed {
                json!({"kind":"run_quiesced","targets":[]})
            } else if action == "force_cancel" && force_cancel_state == "effect_observed" {
                json!({"kind":"worker_cancel_requested","targets":force_cancel_targets})
            } else { Value::Null },
            "target_ids": if action == "force_cancel" {
                json!(force_cancel_targets)
            } else { Value::Null },
            "cancel_failures": if action == "force_cancel" {
                json!(force_cancel_failures)
            } else { Value::Null },
            "process_exit_confirmed": if action == "force_cancel" {
                json!(false)
            } else { Value::Null },
        }),
    );

    let mut response = json!({
        "ok": if action == "force_cancel" {
            force_cancel_state == "effect_observed"
        } else { effect_observed || context_queued },
        "command_id": command_id,
        "status": if let Some(state)=lifecycle_state.as_deref(){state}else if action == "force_cancel" { force_cancel_state }
            else if context_queued { "partial" }
            else if effect_observed { "applied" } else { "unknown" },
        "generation": next_gen,
        "observed_paused": observed_paused,
    });
    if action == "force_cancel" {
        response["effect"] = json!("worker_cancel_requested");
        response["target_ids"] = json!(force_cancel_targets);
        response["cancel_failures"] = json!(force_cancel_failures);
        response["process_exit_confirmed"] = json!(false);
    }
    if !context_effect.is_null() {
        response["effect"] = context_effect.clone();
    }

    if let Err(error) = control_journal::finish(
        &state,
        &run_id,
        &conn,
        &command_id,
        &before_control,
        &run_doc,
        &response,
    ) {
        return(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":{"code":"run.control.settlement_failed","message":error},"command_id":command_id,"status":"unknown","effect_recorded":control_journal::has_result(&conn,&command_id).unwrap_or(false)}))).into_response();
    }
    if matches!(action.as_str(), "stop" | "complete")
        && let Err(error) = runner_stop_recovery::release_applied(&state, &conn, &command_id)
    {
        tracing::warn!(run_id=%run_id, command_id=%command_id, %error, "Confirmed stop fence is released by recovery");
    }

    if action == "mark_false" && context_effect["resolve_scheduled"] == true {
        let flag = body["flag"]
            .as_str()
            .or_else(|| body["text"].as_str())
            .unwrap_or("")
            .trim();
        handle
            .rejected_flags
            .lock()
            .unwrap()
            .insert(flag.to_owned());
        if let Err(error) =
            control_journal::publish_false_positive(&state, &run_id, &command_id, flag, &actor)
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error,"command_id":command_id,"status":"unknown"})),
            )
                .into_response();
        }
        if let Err(error) = control_recovery::enqueue_recovery(&state, &run_id, &command_id) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":error,"command_id":command_id,"status":"unknown"})),
            )
                .into_response();
        }
    }

    if let Ok(raw) = conn.query_row(
        "SELECT response_json FROM control_commands WHERE command_id=?1",
        [&command_id],
        |row| row.get::<_, String>(0),
    ) {
        if let Ok(saved) = serde_json::from_str::<Value>(&raw) {
            response = saved;
        }
    }
    Json(response).into_response()
}

async fn get_control_command(
    State(state): State<Arc<AppState>>,
    Path((run_id, command_id)): Path<(String, String)>,
) -> Response {
    if let Err(error) = refresh_control_result(&state, &run_id, &command_id).await {
        tracing::warn!(run_id=%run_id,command_id=%command_id,%error,"Control confirmation remains pending");
    }
    let Ok(conn) = open_control_db(&state, &run_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": "Control journal not found"})),
        )
            .into_response();
    };
    match conn.query_row(
        "SELECT response_json FROM control_commands WHERE command_id = ?1",
        params![command_id],
        |row| row.get::<_, String>(0),
    ) {
        Ok(raw) if let Ok(val) = serde_json::from_str::<Value>(&raw) => Json(val).into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": format!("Command {command_id} not found")})),
        )
            .into_response(),
    }
}

pub(crate) async fn spawn_worker_route(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    if let Err(error) = crate::business::require_run(&state, &run_id) {
        return error.into_response();
    }
    let run_handle = state.runs.handle(&state, &run_id);
    let _lifecycle_guard = run_handle.delete_lock.lock().await;
    if run_handle.deleting.load(Ordering::SeqCst) {
        return Json(json!({"ok":false,"detail":"Run deletion is settling"})).into_response();
    }
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    spawn_worker_effect(&state, &run_id, &body).await
}

async fn spawn_worker_effect(state: &Arc<AppState>, run_id: &str, body: &Value) -> Response {
    let Some(run_doc) = state.store.get("runs", &run_id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "detail": "Run not found"})),
        )
            .into_response();
    };
    if let Err(response) = check_stop_pending(state, &run_doc) {
        return response;
    }
    let guard_protected = execution_guard::protected(&run_doc);
    if guard_protected
        && body["engine"]
            .as_str()
            .is_some_and(|eng| settings::normalize_engine(eng) != "pi")
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "code": "guard.preflight.only_muteki_pi_allowed",
                "detail": "Each protected Worker seat must resolve to Pi"
            })),
        )
            .into_response();
    }
    if let Err((status, error)) = control_contract::admit(
        run_id,
        &run_doc,
        &json!({"action":"spawn_worker"}),
        now_secs(),
    ) {
        return (status, Json(error)).into_response();
    }
    let exec_gen = run_doc["execution_generation"].as_u64().unwrap_or(1).max(1);
    let mut payload = json!({});
    if let Ok(config) = launch_config::settings(state, run_id) {
        for key in [
            "worker_backend",
            "worker_node_id",
            "worker_network",
            "offline",
        ] {
            if !config[key].is_null() {
                payload[key] = config[key].clone();
            }
        }
    }
    if let Some(eng) = body["engine"].as_str().filter(|s| !s.trim().is_empty()) {
        payload["engines"] = json!([eng]);
    } else if guard_protected {
        payload["engines"] = json!(["pi"]);
    }
    payload["cost_budget_usd"] = run_doc["cost_budget_usd"].clone();
    payload["mode"] = run_doc["mode"].clone();
    payload["src_task_id"] = run_doc["src_task_id"].clone();
    payload["bound_to_src"] = run_doc["bound_to_src"].clone();
    let (specs, degraded) = match worker::resolve_worker_specs(&state, &payload, exec_gen).await {
        Ok(s) => s,
        Err((detail, failures)) => {
            if guard_protected && !failures.is_empty() {
                if let Some(resp) =
                    execution_guard::preflight(state, &[], &failures, Some(&payload))
                {
                    return resp;
                }
            }
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"ok": false, "detail": detail})),
            )
                .into_response();
        }
    };
    if guard_protected {
        if let Some(resp) = execution_guard::preflight(state, &specs, &degraded, Some(&payload)) {
            return resp;
        }
    }
    for failure in degraded {
        emit_event(&state, &run_id, None, "engine_degraded", failure);
    }
    let Some(mut spec) = specs.into_iter().next() else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"ok": false, "detail": "No eligible Worker"})),
        )
            .into_response();
    };
    let challenge = json!({
        "name": run_doc["name"],
        "category": run_doc["category"],
        "description": run_doc["description"],
        "flag_format": run_doc["flag_format"],
        "target": run_doc["target"],
        "mode": run_doc["mode"],
        "expected_flags": run_doc["expected_flags"],
        "multi_flag": run_doc["multi_flag"],
        "attachments": run_doc["attachments"],
        "task_contract": run_doc["task_contract"],
        "pentest_contract": run_doc["pentest_contract"],
    });
    let mut used: HashSet<String> = state
        .runs
        .handle(state, run_id)
        .workers
        .lock()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    match state.store.list("runner_dispatches") {
        Ok(rows) => used.extend(
            rows.iter()
                .filter(|row| row["run_id"] == run_id)
                .filter_map(|row| row["worker_id"].as_str().map(str::to_owned)),
        ),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "detail": error.to_string()})),
            )
                .into_response();
        }
    }
    let suffix = if exec_gen > 1 {
        format!("-g{exec_gen}")
    } else {
        String::new()
    };
    let mut number = 1;
    while used.contains(&format!("cli-{}#{number}{suffix}", spec.norm_engine)) {
        number += 1;
    }
    spec.solver_id = format!("cli-{}#{number}{suffix}", spec.norm_engine);
    let first_step = match graph::SharedGraph::open(state, run_id) {
        Ok(graph) => {
            graph
                .all_intents()
                .iter()
                .filter_map(|intent| {
                    intent["intent_id"]
                        .as_str()?
                        .strip_prefix("step_")?
                        .split('_')
                        .next()?
                        .parse::<usize>()
                        .ok()
                })
                .max()
                .unwrap_or(0)
                + 1
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "detail": error})),
            )
                .into_response();
        }
    };
    worker::spawn_local_worker(
        state.clone(),
        run_id.to_owned(),
        spec,
        challenge,
        None,
        first_step,
    );
    Json(json!({"ok": true, "spawned": 1})).into_response()
}

pub(crate) async fn kill_worker_route(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let mut body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    if let Some(fields) = body.as_object_mut() {
        fields.insert("actor".to_owned(), json!("operator"));
    }
    kill_worker_effect(&state, &run_id, &body).await
}

async fn kill_worker_effect(state: &Arc<AppState>, run_id: &str, body: &Value) -> Response {
    let solver_id = body["solver_id"]
        .as_str()
        .or_else(|| body["worker_id"].as_str())
        .unwrap_or("")
        .trim();
    if solver_id.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"ok": false, "code":"missing_worker", "detail": "worker_id is required"})),
        )
            .into_response();
    }
    if let Some(w) = state.runs.unregister_worker(&run_id, solver_id) {
        w.abort.abort();
        state.agents.close(&w.thread_id).await;
        emit_event(
            &state,
            &run_id,
            Some(solver_id),
            "worker.status",
            json!({
                "online": false,
                "status": "done",
                "reason": "operator_stop",
                "actor": body["actor"].as_str().unwrap_or("operator"),
            }),
        );
        return Json(json!({"ok": true, "worker_id":solver_id})).into_response();
    }
    Json(json!({"ok": false, "detail": "worker is not active"})).into_response()
}

fn sanitize_upload_name(raw: &str) -> String {
    let base = std::path::Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    if base.is_empty() || base == "." || base == ".." {
        "file".to_owned()
    } else {
        base.to_owned()
    }
}

async fn upload_files(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    mut multipart: Multipart,
) -> Response {
    if let Err(error) = crate::business::require_run(&state, &run_id) {
        return error.into_response();
    }
    let run_handle = state.runs.handle(&state, &run_id);
    let _lifecycle_guard = run_handle.delete_lock.lock().await;
    if run_handle.deleting.load(Ordering::SeqCst) {
        return Json(json!({"ok":false,"detail":"Run deletion is settling"})).into_response();
    }
    if let Err(e) = ensure_run_record(&state, &run_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": e})),
        )
            .into_response();
    }
    let uploads_dir = session_dir(&state, &run_id).join("uploads");
    if let Err(e) = fs::create_dir_all(&uploads_dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": e.to_string()})),
        )
            .into_response();
    }
    let uploads_dir = fs::canonicalize(&uploads_dir).unwrap_or(uploads_dir);
    let max_upload_files = upload_limit("MUTEKI_MAX_UPLOAD_FILES", 20);
    let max_upload_mb = upload_limit("MUTEKI_MAX_UPLOAD_MB", 25);
    let max_upload_file_bytes = max_upload_mb.saturating_mul(1024 * 1024);

    let mut uploaded = Vec::new();
    let mut created_paths: Vec<PathBuf> = Vec::new();
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                for path in &created_paths {
                    let _ = tokio::fs::remove_file(path).await;
                }
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"detail": error.to_string()})),
                )
                    .into_response();
            }
        };
        if field.name() != Some("files") {
            continue;
        }
        if uploaded.len() >= max_upload_files {
            for path in &created_paths {
                let _ = tokio::fs::remove_file(path).await;
            }
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"detail": "too many files"})),
            )
                .into_response();
        }
        let raw_name = field.file_name().unwrap_or("file").to_owned();
        let safe_name = sanitize_upload_name(&raw_name);

        let stem = std::path::Path::new(&safe_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("upload");
        let ext = std::path::Path::new(&safe_name)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("");

        let mut attempt = 0u64;
        let (mut out, final_path, chosen_name) = loop {
            let candidate_name = if attempt == 0 {
                safe_name.clone()
            } else if ext.is_empty() {
                format!("{stem}-{attempt}")
            } else {
                format!("{stem}-{attempt}.{ext}")
            };
            let candidate_path = uploads_dir.join(&candidate_name);
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate_path)
                .await
            {
                Ok(out) => break (out, candidate_path, candidate_name),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => attempt += 1,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"detail": error.to_string()})),
                    )
                        .into_response();
                }
            }
        };
        let mut size = 0usize;
        loop {
            let chunk = match field.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => {
                    drop(out);
                    let _ = tokio::fs::remove_file(&final_path).await;
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"detail": error.to_string()})),
                    )
                        .into_response();
                }
            };
            size = size.saturating_add(chunk.len());
            if size > max_upload_file_bytes {
                drop(out);
                let _ = tokio::fs::remove_file(&final_path).await;
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({"detail": format!("{safe_name} too large")})),
                )
                    .into_response();
            }
            if let Err(error) = out.write_all(&chunk).await {
                drop(out);
                let _ = tokio::fs::remove_file(&final_path).await;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail": error.to_string()})),
                )
                    .into_response();
            }
        }
        if let Err(error) = out.flush().await {
            drop(out);
            let _ = tokio::fs::remove_file(&final_path).await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": error.to_string()})),
            )
                .into_response();
        }

        uploaded.push(json!({
            "name": chosen_name,
            "path": final_path.to_string_lossy(),
            "size": size,
        }));
        created_paths.push(final_path);
    }

    if uploaded.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": "files field is required"})),
        )
            .into_response();
    }

    Json(json!({"files": uploaded})).into_response()
}

async fn run_credentials(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Response {
    if state.store.get("runs", &run_id).ok().flatten().is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": format!("Run {run_id} not found")})),
        )
            .into_response();
    }
    let creds = graph::SharedGraph::open(&state, &run_id)
        .map(|g| g.canonical_credentials())
        .unwrap_or_default();
    Json(json!({"credentials": creds})).into_response()
}

async fn terminal_backfill(
    state: &Arc<AppState>,
    run_id: &str,
    socket: &mut WebSocket,
    after: &mut u64,
) -> Result<(), ()> {
    let (state, id, start_after) = (state.clone(), run_id.to_owned(), *after);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<(u64, Option<String>), ()>>(16);
    tokio::task::spawn_blocking(move || {
        let scan = {
            let handle = state.runs.handle(&state, &id);
            let _guard = handle.event_file_lock.lock().unwrap();
            persisted_event_scan(&state, &id, start_after)
        };
        let Ok(scan) = scan else {
            let _ = tx.blocking_send(Err(()));
            return;
        };
        let mut max_seq = start_after;
        for (seq, maybe_event) in scan {
            if tx.is_closed() {
                return;
            }
            max_seq = max_seq.max(seq);
            if let Some(event) = maybe_event
                && event["event_type"] == "terminal.output"
                && let Some(text) = event["payload"]["text"].as_str()
                && tx.blocking_send(Ok((seq, Some(text.to_owned())))).is_err()
            {
                return;
            }
        }
        let _ = tx.blocking_send(Ok((max_seq, None)));
    });
    while let Some(item) = rx.recv().await {
        let (seq, maybe_text) = item?;
        if let Some(text) = maybe_text {
            socket
                .send(Message::Text(text.into()))
                .await
                .map_err(|_| ())?;
        }
        *after = (*after).max(seq);
    }
    Ok(())
}
async fn run_terminal_ws(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    match state.store.get("runs", &run_id) {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        let handle = state.runs.handle(&state, &run_id);
        let mut rx = handle.bus.subscribe();let mut after=0;
        if terminal_backfill(&state,&run_id,&mut socket,&mut after).await.is_err(){return;}
        loop {
            tokio::select! {
                incoming=socket.recv()=>{if matches!(incoming,None|Some(Err(_))|Some(Ok(Message::Close(_)))){break;}},
                received=rx.recv()=>match received {
                    Ok(event)=>{let seq=event["seq"].as_u64().unwrap_or(0);if seq<=after{continue;}if event["event_type"]=="terminal.output"&&let Some(text)=event["payload"]["text"].as_str(){if socket.send(Message::Text(text.to_owned().into())).await.is_err(){break;}}after=seq;},
                    Err(broadcast::error::RecvError::Lagged(_))=>{if terminal_backfill(&state,&run_id,&mut socket,&mut after).await.is_err(){break;}},
                    Err(broadcast::error::RecvError::Closed)=>break,
                }
            }
        }
    })
}

fn default_retention_policy(mode: &str) -> Value {
    json!({
        "mode": mode,
        "archive_enabled": false,
        "archive_after_days": 30,
        "delete_enabled": false,
        "delete_after_days": 90,
    })
}

pub(crate) async fn get_retention(State(state): State<Arc<AppState>>) -> crate::chat::Api {
    let saved = state
        .store
        .get("settings", "run_retention")
        .map_err(crate::control::storage)?
        .unwrap_or_else(|| json!({}));
    let ctf = saved
        .get("ctf")
        .cloned()
        .unwrap_or_else(|| default_retention_policy("ctf"));
    let pentest = saved
        .get("pentest")
        .cloned()
        .unwrap_or_else(|| default_retention_policy("pentest"));
    Ok(Json(json!({
        "policies": {
            "ctf": ctf,
            "pentest": pentest,
        }
    })))
}

pub(crate) async fn put_retention(
    State(state): State<Arc<AppState>>,
    Path(mode): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, crate::chat::Failure> {
    if mode != "ctf" && mode != "pentest" {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"detail": format!("Unknown retention mode: {mode}")})),
        ));
    }
    let Some(archive_enabled) = body["archive_enabled"].as_bool() else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": "archive_enabled must be a boolean"})),
        ));
    };
    let Some(delete_enabled) = body["delete_enabled"].as_bool() else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": "delete_enabled must be a boolean"})),
        ));
    };
    let Some(archive_days) = body["archive_after_days"].as_u64() else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": "archive_after_days must be an integer"})),
        ));
    };
    let Some(delete_days) = body["delete_after_days"].as_u64() else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": "delete_after_days must be an integer"})),
        ));
    };
    if !(1..=3650).contains(&archive_days) || !(1..=3650).contains(&delete_days) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": "Retention days must be between 1 and 3650"})),
        ));
    }
    if archive_enabled && delete_enabled && delete_days <= archive_days {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(
                json!({"detail": "delete_after_days must be greater than archive_after_days when both are enabled"}),
            ),
        ));
    }

    let policy = json!({
        "mode": mode,
        "archive_enabled": archive_enabled,
        "archive_after_days": archive_days,
        "delete_enabled": delete_enabled,
        "delete_after_days": delete_days,
    });
    let mut saved = state
        .store
        .get("settings", "run_retention")
        .map_err(crate::control::storage)?
        .unwrap_or_else(|| json!({}));
    saved[&mode] = policy.clone();
    state
        .store
        .put("settings", "run_retention", &saved)
        .map_err(crate::control::storage)?;
    Ok(Json(json!({"policy": policy})).into_response())
}

pub fn local_run_detail(state: &AppState, run_id: &str) -> Option<Value> {
    let run = state.store.get("runs", run_id).ok().flatten()?;
    Some(project_run_summary(state, run))
}

pub fn local_run_history(state: &AppState, run_id: &str, before: u64, limit: usize) -> Value {
    let mut page = std::collections::VecDeque::new();
    if let Ok(events) = persisted_event_iter(state, run_id) {
        for event in events.take_while(|event| event["seq"].as_u64().unwrap_or(0) < before) {
            if page.len() == limit + 1 {
                page.pop_front();
            }
            page.push_back(event);
        }
    }
    let more = page.len() > limit;
    let page: Vec<Value> = page.into_iter().rev().take(limit).collect();
    let next = if more {
        page.last().map(|v| v["seq"].clone())
    } else {
        None
    };
    json!({
        "events": page,
        "next_before": next,
        "historical": state.store.get("runs",run_id).ok().flatten().is_some_and(|run|run["history_event_file"].is_string()),
    })
}

fn format_sse_frame(event_name: &str, id: Option<u64>, data: &Value) -> String {
    format!(
        "{}event: {}\ndata: {}\n\n",
        id.map(|n| format!("id: {n}\n")).unwrap_or_default(),
        event_name,
        data
    )
}

#[derive(Default)]
struct IntentLineage {
    from_facts: Vec<i64>,
    depends_on: Vec<String>,
    required_pocs: Vec<String>,
    route_hash: String,
    lane_key: String,
}

struct GraphTailEvent {
    kind: String,
    actor: String,
    ts: f64,
    payload: Value,
}

#[derive(Default)]
struct GraphReplayHydration {
    origin_fact: Option<(i64, f64, String, String)>,
    intent_lineage: HashMap<String, IntentLineage>,
    fact_revisions: Vec<GraphTailEvent>,
    sub_goal_events: Vec<GraphTailEvent>,
}

#[derive(Default)]
struct HydrationSeen {
    origin_fact: bool,
    fact_revisions: HashSet<i64>,
    sub_goal_events: HashSet<(String, String)>,
}

fn open_graph_readonly(path: &std::path::Path) -> Option<Connection> {
    if !path.is_file() {
        return None;
    }
    if let Ok(conn) = crate::history_import::read_db(path) {
        let _ = conn.busy_timeout(Duration::from_secs(2));
        if conn
            .query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))
            .is_ok()
        {
            return Some(conn);
        }
    }
    let mut wal_os = path.as_os_str().to_os_string();
    wal_os.push("-wal");
    if !std::path::Path::new(&wal_os).exists() {
        let abs = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if let Ok(mut file_url) = url::Url::from_file_path(&abs) {
            file_url.set_query(Some("mode=ro&immutable=1"));
            if let Ok(conn) = Connection::open_with_flags(
                file_url.as_str(),
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) {
                let _ = conn.busy_timeout(Duration::from_secs(2));
                if conn
                    .query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))
                    .is_ok()
                {
                    return Some(conn);
                }
            }
        }
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 2000;");
    conn.query_row("SELECT 1 FROM sqlite_master LIMIT 1", [], |_| Ok(()))
        .ok()?;
    Some(conn)
}

fn load_graph_replay_hydration(state: &AppState, run_id: &str) -> Option<GraphReplayHydration> {
    let path = graph::graph_dir(state, run_id).join("shared_graph.db");
    let conn = open_graph_readonly(&path)?;
    let mut out = GraphReplayHydration::default();

    if let Ok(mut stmt) = conn.prepare(
        "SELECT seq, ts, payload FROM events WHERE kind = 'fact_origin' ORDER BY seq LIMIT 1",
    ) {
        if let Ok(Some((seq, ts, raw))) = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .optional()
        {
            if let Ok(payload) = serde_json::from_str::<Value>(&raw) {
                let fact = payload["fact"].as_str().unwrap_or("").to_owned();
                let summary = payload["summary"].as_str().unwrap_or("").to_owned();
                out.origin_fact = Some((seq, ts, fact, summary));
            }
        }
    }

    if let Ok(mut stmt) =
        conn.prepare("SELECT intent_id, fact_seq FROM intent_sources ORDER BY rowid")
    {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) {
            for row in rows.flatten() {
                let entry = out.intent_lineage.entry(row.0).or_default();
                if row.1 > 0 && !entry.from_facts.contains(&row.1) {
                    entry.from_facts.push(row.1);
                }
            }
        }
    }

    if let Ok(mut stmt) = conn
        .prepare("SELECT intent_id, depends_on_intent_id FROM intent_dependencies ORDER BY rowid")
    {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            for row in rows.flatten() {
                if row.1.is_empty() {
                    continue;
                }
                let entry = out.intent_lineage.entry(row.0).or_default();
                if !entry.depends_on.contains(&row.1) {
                    entry.depends_on.push(row.1);
                }
            }
        }
    }

    if let Ok(mut stmt) =
        conn.prepare("SELECT intent_id, required_pocs_json, route_hash, lane_key FROM intents")
    {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        }) {
            for (intent_id, pocs_raw, route_hash, lane_key) in rows.flatten() {
                let pocs = serde_json::from_str::<Vec<String>>(&pocs_raw).unwrap_or_default();
                if pocs.is_empty() && route_hash.is_empty() && lane_key.is_empty() {
                    continue;
                }
                let entry = out.intent_lineage.entry(intent_id).or_default();
                if entry.required_pocs.is_empty() {
                    entry.required_pocs = pocs;
                }
                if entry.route_hash.is_empty() {
                    entry.route_hash = route_hash;
                }
                if entry.lane_key.is_empty() {
                    entry.lane_key = lane_key;
                }
            }
        }
    }

    if let Ok(mut stmt) = conn.prepare(
        "SELECT kind, actor, ts, payload FROM events WHERE kind IN ('fact_superseded', 'fact_rejected') ORDER BY seq",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, String>(3)?,
            ))
        }) {
            for (kind, actor, ts, raw) in rows.flatten() {
                if let Ok(payload) = serde_json::from_str::<Value>(&raw) {
                    out.fact_revisions.push(GraphTailEvent {
                        kind,
                        actor,
                        ts,
                        payload,
                    });
                }
            }
        }
    }

    if let Ok(mut stmt) = conn.prepare(
        "SELECT kind, actor, ts, payload FROM events WHERE kind IN ('sub_goal_added', 'sub_goal_satisfied') ORDER BY seq",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, String>(3)?,
            ))
        }) {
            for (kind, actor, ts, raw) in rows.flatten() {
                if let Ok(payload) = serde_json::from_str::<Value>(&raw) {
                    out.sub_goal_events.push(GraphTailEvent {
                        kind,
                        actor,
                        ts,
                        payload,
                    });
                }
            }
        }
    }

    Some(out)
}

fn enrich_replay_event(
    event: &mut Value,
    hydration: &GraphReplayHydration,
    seen: &mut HydrationSeen,
) {
    if event["event_type"].as_str() != Some("blackboard.delta") {
        return;
    }
    let kind = event["payload"]["kind"].as_str().unwrap_or("").to_owned();
    match kind.as_str() {
        "intent_proposed" => {
            let Some(iid) = event["payload"]["intent_id"].as_str().map(str::to_owned) else {
                return;
            };
            let Some(lineage) = hydration.intent_lineage.get(&iid) else {
                return;
            };
            let Some(payload) = event.get_mut("payload").and_then(Value::as_object_mut) else {
                return;
            };
            if payload
                .get("from_facts")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
                && !lineage.from_facts.is_empty()
            {
                payload.insert("from_facts".to_owned(), json!(lineage.from_facts));
            }
            if payload
                .get("depends_on")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
                && !lineage.depends_on.is_empty()
            {
                payload.insert("depends_on".to_owned(), json!(lineage.depends_on));
            }
            if payload
                .get("required_pocs")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
                && !lineage.required_pocs.is_empty()
            {
                payload.insert("required_pocs".to_owned(), json!(lineage.required_pocs));
            }
            if payload
                .get("route_hash")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
                && !lineage.route_hash.is_empty()
            {
                payload.insert("route_hash".to_owned(), json!(lineage.route_hash));
            }
            if payload
                .get("lane_key")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
                && !lineage.lane_key.is_empty()
            {
                payload.insert("lane_key".to_owned(), json!(lineage.lane_key));
            }
        }
        "fact_origin" => {
            if let Some(root) = event.as_object_mut() {
                root.insert("solver_id".to_owned(), json!("origin"));
            }
            if let Some(payload) = event.get_mut("payload").and_then(Value::as_object_mut) {
                payload.insert("actor".to_owned(), json!("origin"));
                if let Some((origin_seq, _, fact, summary)) = &hydration.origin_fact {
                    if !payload
                        .get("fact_seq")
                        .and_then(Value::as_i64)
                        .is_some_and(|seq| seq > 0)
                    {
                        payload.insert("fact_seq".to_owned(), json!(origin_seq));
                    }
                    if payload
                        .get("fact")
                        .and_then(Value::as_str)
                        .is_none_or(|s| s.trim().is_empty())
                        && !fact.is_empty()
                    {
                        payload.insert("fact".to_owned(), json!(fact));
                    }
                    if payload
                        .get("summary")
                        .and_then(Value::as_str)
                        .is_none_or(|s| s.trim().is_empty())
                        && !summary.is_empty()
                    {
                        payload.insert("summary".to_owned(), json!(summary));
                    }
                }
                payload
                    .entry("verified".to_owned())
                    .or_insert_with(|| json!(true));
                payload
                    .entry("confidence".to_owned())
                    .or_insert_with(|| json!(1.0));
            }
            seen.origin_fact = true;
        }
        "fact_superseded" | "fact_rejected" => {
            if let Some(seq) = event["payload"]["fact_seq"].as_i64() {
                seen.fact_revisions.insert(seq);
            }
        }
        "sub_goal_added" | "sub_goal_satisfied" => {
            if let Some(goal_id) = event["payload"]["goal_id"].as_str() {
                seen.sub_goal_events.insert((kind, goal_id.to_owned()));
            }
        }
        _ => {}
    }
}

fn tail_blackboard_event(run_id: &str, seq: u64, row: &GraphTailEvent) -> Value {
    let mut payload = row.payload.as_object().cloned().unwrap_or_default();
    payload.insert("kind".to_owned(), json!(row.kind));
    if !row.actor.is_empty() {
        payload
            .entry("actor".to_owned())
            .or_insert_with(|| json!(row.actor));
    }
    json!({
        "seq": seq,
        "ts": row.ts,
        "run_id": run_id,
        "event_type": "blackboard.delta",
        "solver_id": if row.actor.is_empty() { Value::Null } else { json!(row.actor) },
        "payload": Value::Object(payload),
    })
}

fn hydration_tail_events(
    run_id: &str,
    seq: u64,
    hydration: &GraphReplayHydration,
    seen: &HydrationSeen,
) -> Vec<Value> {
    let mut out = Vec::new();
    if !seen.origin_fact {
        if let Some((origin_seq, ts, fact, summary)) = &hydration.origin_fact {
            out.push(json!({
                "seq": seq,
                "ts": ts,
                "run_id": run_id,
                "event_type": "blackboard.delta",
                "solver_id": "origin",
                "payload": {
                    "kind": "fact_origin",
                    "fact_seq": origin_seq,
                    "actor": "origin",
                    "fact": fact,
                    "summary": summary,
                    "verified": true,
                    "confidence": 1.0,
                },
            }));
        }
    }
    for row in &hydration.fact_revisions {
        let Some(fact_seq) = row.payload["fact_seq"].as_i64() else {
            continue;
        };
        if seen.fact_revisions.contains(&fact_seq) {
            continue;
        }
        out.push(tail_blackboard_event(run_id, seq, row));
    }
    for row in &hydration.sub_goal_events {
        let Some(goal_id) = row.payload["goal_id"].as_str() else {
            continue;
        };
        if seen
            .sub_goal_events
            .contains(&(row.kind.clone(), goal_id.to_owned()))
        {
            continue;
        }
        out.push(tail_blackboard_event(run_id, seq, row));
    }
    out
}

pub fn stream_local_run_events(
    state: Arc<AppState>,
    run_id: String,
    after: u64,
    batch_replay: bool,
) -> Response {
    let handle = state.runs.handle(&state, &run_id);
    let rx = handle.bus.subscribe();
    let replay_end_seq = Arc::new(AtomicU64::new(0));
    let replay_end_writer = replay_end_seq.clone();
    let live_state = state.clone();
    let live_run_id = run_id.clone();
    let (sender, receiver) = tokio::sync::mpsc::channel::<String>(1);
    tokio::task::spawn_blocking(move || {
        let hydration = load_graph_replay_hydration(&state, &run_id);
        let replay = {
            let _guard = handle.event_file_lock.lock().unwrap();
            persisted_event_scan(&state, &run_id, after).ok()
        };
        let mut seen = HydrationSeen::default();
        let mut did_reset = false;
        let mut current_seq = 0u64;
        let mut batch = Vec::new();
        for (seq, maybe_event) in replay.into_iter().flatten() {
            if sender.is_closed() {
                return;
            }
            current_seq = seq;
            let Some(mut event) = maybe_event else {
                continue;
            };
            if let Some(h) = hydration.as_ref() {
                enrich_replay_event(&mut event, h, &mut seen);
            }
            if batch_replay {
                batch.push(event);
                if batch.len() < REPLAY_BATCH_SIZE {
                    continue;
                }
                let last_seq = batch
                    .last()
                    .and_then(|ev| ev["seq"].as_u64())
                    .unwrap_or(seq);
                if sender
                    .blocking_send(format_sse_frame(
                        "replay.batch",
                        Some(last_seq),
                        &json!({"events": batch, "last_seq": last_seq}),
                    ))
                    .is_err()
                {
                    return;
                }
                batch.clear();
            } else {
                let kind = event["event_type"].as_str().unwrap_or("run.event");
                if sender
                    .blocking_send(format_sse_frame(kind, event["seq"].as_u64(), &event))
                    .is_err()
                {
                    return;
                }
            }
        }
        if after > current_seq {
            did_reset = true;
            seen = HydrationSeen::default();
            if sender
                .blocking_send(format_sse_frame(
                    "replay.reset",
                    None,
                    &json!({"last_seq": current_seq}),
                ))
                .is_err()
            {
                return;
            }
            if current_seq > 0 {
                let reset_replay = {
                    let _guard = handle.event_file_lock.lock().unwrap();
                    persisted_event_scan(&state, &run_id, 0).ok()
                };
                for (seq, maybe_event) in reset_replay
                    .into_iter()
                    .flatten()
                    .take_while(|(seq, _)| *seq <= current_seq)
                {
                    if sender.is_closed() {
                        return;
                    }
                    let Some(mut event) = maybe_event else {
                        continue;
                    };
                    if let Some(h) = hydration.as_ref() {
                        enrich_replay_event(&mut event, h, &mut seen);
                    }
                    if batch_replay {
                        batch.push(event);
                        if batch.len() < REPLAY_BATCH_SIZE {
                            continue;
                        }
                        let last_seq = batch
                            .last()
                            .and_then(|ev| ev["seq"].as_u64())
                            .unwrap_or(seq);
                        if sender
                            .blocking_send(format_sse_frame(
                                "replay.batch",
                                Some(last_seq),
                                &json!({"events": batch, "last_seq": last_seq}),
                            ))
                            .is_err()
                        {
                            return;
                        }
                        batch.clear();
                    } else {
                        let kind = event["event_type"].as_str().unwrap_or("run.event");
                        if sender
                            .blocking_send(format_sse_frame(kind, event["seq"].as_u64(), &event))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
        }
        if (after == 0 || did_reset) && current_seq > 0 {
            if let Some(h) = hydration.as_ref() {
                for event in hydration_tail_events(&run_id, current_seq, h, &seen) {
                    if sender.is_closed() {
                        return;
                    }
                    if batch_replay {
                        batch.push(event);
                        if batch.len() < REPLAY_BATCH_SIZE {
                            continue;
                        }
                        let last_seq = batch
                            .last()
                            .and_then(|ev| ev["seq"].as_u64())
                            .unwrap_or(current_seq);
                        if sender
                            .blocking_send(format_sse_frame(
                                "replay.batch",
                                Some(last_seq),
                                &json!({"events": batch, "last_seq": last_seq}),
                            ))
                            .is_err()
                        {
                            return;
                        }
                        batch.clear();
                    } else {
                        let kind = event["event_type"].as_str().unwrap_or("run.event");
                        if sender
                            .blocking_send(format_sse_frame(kind, event["seq"].as_u64(), &event))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
        }
        if !batch.is_empty() {
            let last_seq = batch
                .last()
                .and_then(|event| event["seq"].as_u64())
                .unwrap_or(current_seq);
            if sender
                .blocking_send(format_sse_frame(
                    "replay.batch",
                    Some(last_seq),
                    &json!({"events": batch, "last_seq": last_seq}),
                ))
                .is_err()
            {
                return;
            }
        }
        replay_end_writer.store(current_seq, Ordering::SeqCst);
        let _ = sender.blocking_send(format_sse_frame(
            "replay.complete",
            None,
            &json!({"last_seq": current_seq}),
        ));
    });
    let init_stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|frame| (Ok::<_, std::convert::Infallible>(frame), receiver))
    });

    let live_stream = futures_util::stream::unfold(
        (
            rx,
            None::<u64>,
            None::<tokio::sync::mpsc::Receiver<(u64, String)>>,
            live_state,
            live_run_id,
            replay_end_seq,
        ),
        |(mut rx, seen_opt, mut catchup_rx, live_state, live_run_id, replay_end_seq)| async move {
            let mut seen_seq = seen_opt.unwrap_or_else(|| replay_end_seq.load(Ordering::SeqCst));
            if let Some(mut crx) = catchup_rx.take() {
                if let Some((seq, frame)) = crx.recv().await {
                    seen_seq = seen_seq.max(seq);
                    return Some((
                        Ok::<_, std::convert::Infallible>(frame),
                        (
                            rx,
                            Some(seen_seq),
                            Some(crx),
                            live_state,
                            live_run_id,
                            replay_end_seq,
                        ),
                    ));
                }
            }
            let mut ping = tokio::time::interval(Duration::from_secs(10));
            let _ = ping.tick().await;
            loop {
                tokio::select! {
                    _ = ping.tick() => {
                        return Some((
                            Ok::<_, std::convert::Infallible>(": muteki-ping\n\n".to_owned()),
                            (rx, Some(seen_seq), None, live_state, live_run_id, replay_end_seq),
                        ));
                    }
                    msg = rx.recv() => {
                        match msg {
                            Ok(ev) => {
                                let seq = ev["seq"].as_u64().unwrap_or(0);
                                if seq <= seen_seq {
                                    continue;
                                }
                                seen_seq = seq;
                                let et = ev["event_type"].as_str().unwrap_or("run.event");
                                let frame = format_sse_frame(et, Some(seq), &ev);
                                return Some((
                                    Ok(frame),
                                    (rx, Some(seen_seq), None, live_state, live_run_id, replay_end_seq),
                                ));
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                let (ctx, mut crx) = tokio::sync::mpsc::channel::<(u64, String)>(1);
                                let c_state = live_state.clone();
                                let c_run_id = live_run_id.clone();
                                let c_after = seen_seq;
                                tokio::task::spawn_blocking(move || {
                                    let scan = {
                                        let handle = c_state.runs.handle(&c_state, &c_run_id);
                                        let _guard = handle.event_file_lock.lock().unwrap();
                                        persisted_event_scan(&c_state, &c_run_id, c_after).ok()
                                    };
                                    for (seq, maybe_ev) in scan.into_iter().flatten() {
                                        if ctx.is_closed() {
                                            return;
                                        }
                                        let Some(ev) = maybe_ev else {
                                            continue;
                                        };
                                        let et = ev["event_type"].as_str().unwrap_or("run.event");
                                        let frame = format_sse_frame(et, Some(seq), &ev);
                                        if ctx.blocking_send((seq, frame)).is_err() {
                                            return;
                                        }
                                    }
                                });
                                if let Some((seq, frame)) = crx.recv().await {
                                    seen_seq = seen_seq.max(seq);
                                    return Some((
                                        Ok(frame),
                                        (
                                            rx,
                                            Some(seen_seq),
                                            Some(crx),
                                            live_state,
                                            live_run_id,
                                            replay_end_seq,
                                        ),
                                    ));
                                }
                                continue;
                            }
                            Err(broadcast::error::RecvError::Closed) => return None,
                        }
                    }
                }
            }
        },
    );

    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(init_stream.chain(live_stream)),
    )
        .into_response()
}

pub fn local_graph_export(
    state: &AppState,
    run_id: &str,
    format: &str,
) -> Result<Response, String> {
    if !matches!(format, "jsonl" | "timeline" | "sqlite") {
        return Err("format must be jsonl, timeline, or sqlite".to_owned());
    }
    let run = state
        .store
        .get("runs", run_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "unknown run".to_owned())?;
    let path = graph::graph_dir(state, run_id).join("shared_graph.db");
    if !path.is_file() {
        return Err("run graph unavailable".to_owned());
    }
    let conn = crate::history_import::read_db(&path)
        .map_err(|error| format!("run graph unreadable: {error}"))?;
    let body = if format == "sqlite" {
        graph::export_sqlite_snapshot(&conn, false)?
    } else {
        graph::export_graph_text(&conn, run_id, &run, format, false)?.into_bytes()
    };
    Ok(graph_export_response(run_id, format, body))
}

pub(crate) fn graph_export_response(run_id: &str, format: &str, body: Vec<u8>) -> Response {
    let content_type = match format {
        "jsonl" => "application/x-ndjson",
        "timeline" => "text/plain; charset=utf-8",
        _ => "application/vnd.sqlite3",
    };
    let suffix = match format {
        "timeline" => "txt",
        "sqlite" => "sqlite",
        _ => "jsonl",
    };
    let filename = graph::export_filename(run_id, suffix);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(content_type),
    );
    headers.insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .expect("sanitized graph export filename"),
    );
    (headers, body).into_response()
}

pub fn unsupported_501(message: &str) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "detail": message,
            "error": {"code": "capability.unsupported", "message": message}
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_state() -> (TempDir, Arc<AppState>) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store
            .put("system", "state_dir", &json!(dir.path()))
            .unwrap();
        (dir, Arc::new(AppState::new(store)))
    }

    #[test]
    fn manual_progress_refresh_keeps_an_ordered_source_range() {
        let (_dir, state) = test_state();
        let run_id = "run-progress-range";
        ensure_run_record(&state, run_id).unwrap();
        emit_event(
            &state,
            run_id,
            None,
            "run.started",
            json!({"challenge":{"mode":"ctf"}}),
        );
        emit_event(
            &state,
            run_id,
            None,
            "run.finished",
            json!({"solved":true,"flags":["flag{x}"],"reason":"goal_met"}),
        );
        let automatic = progress::publish(&state, run_id, false).unwrap();
        let briefs = |n: usize| {
            read_jsonl_events(&state, run_id)
                .into_iter()
                .filter(|event| event["event_type"] == "progress.brief")
                .count()
                == n
        };
        assert!(
            automatic["source_from_seq"].as_u64().unwrap()
                <= automatic["source_to_seq"].as_u64().unwrap()
        );
        assert!(briefs(1));
        let manual = progress::publish(&state, run_id, true).unwrap();
        assert_eq!(manual["brief_id"], automatic["brief_id"]);
        assert!(
            manual["source_from_seq"].as_u64().unwrap()
                <= manual["source_to_seq"].as_u64().unwrap()
        );
        assert!(briefs(1));
        emit_event(
            &state,
            run_id,
            Some("coordinator"),
            "reason.progress",
            json!({"summary":"A later real summary","sections":{"confirmed":[],"active":[],"blocked":[],"next":["look again"]}}),
        );
        let changed = progress::publish(&state, run_id, true).unwrap();
        assert_ne!(changed["brief_id"], automatic["brief_id"]);
        assert!(
            changed["source_from_seq"].as_u64().unwrap()
                <= changed["source_to_seq"].as_u64().unwrap()
        );
        assert!(briefs(2));
        let again = progress::publish(&state, run_id, true).unwrap();
        assert_eq!(again["brief_id"], changed["brief_id"]);
        assert!(briefs(2));
    }

    #[test]
    fn manual_progress_refresh_repairs_a_stored_inverted_source_range() {
        let (_dir, state) = test_state();
        let run_id = "run-progress-inverted";
        ensure_run_record(&state, run_id).unwrap();
        emit_event(
            &state,
            run_id,
            None,
            "run.started",
            json!({"challenge":{"mode":"ctf"}}),
        );
        emit_event(
            &state,
            run_id,
            None,
            "run.finished",
            json!({"solved":true,"flags":["flag{x}"],"reason":"goal_met"}),
        );
        progress::publish(&state, run_id, true).unwrap();
        let path = session_dir(&state, run_id).join("events.jsonl");
        let repaired_lines = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| {
                let mut event: Value = serde_json::from_str(line).unwrap();
                if event["event_type"] == "progress.brief" {
                    let to = event["payload"]["source_to_seq"].as_u64().unwrap();
                    event["payload"]["source_from_seq"] = json!(to + 1);
                }
                event.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&path, repaired_lines).unwrap();
        let briefs = || {
            read_jsonl_events(&state, run_id)
                .into_iter()
                .filter(|event| event["event_type"] == "progress.brief")
                .count()
        };
        let before = briefs();
        let repaired = progress::publish(&state, run_id, true).unwrap();
        assert!(
            repaired["source_from_seq"].as_u64().unwrap()
                <= repaired["source_to_seq"].as_u64().unwrap()
        );
        assert_eq!(briefs(), before);
        let again = progress::publish(&state, run_id, true).unwrap();
        assert_eq!(again["source_from_seq"], repaired["source_from_seq"]);
        assert_eq!(again["source_to_seq"], repaired["source_to_seq"]);
        assert_eq!(briefs(), before);
    }

    #[test]
    fn jsonl_reader_skips_numeric_rows_and_preserves_object_sequence() {
        let (_dir, state) = test_state();
        let run_id = "run-jsonl-object-sequence";
        let path = session_dir(&state, run_id).join("events.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "7\nnull\n[1,2]\n{\"event_type\":\"one\",\"seq\":4}\ntrue\n{\"event_type\":\"two\"}\n",
        )
        .unwrap();
        let events = read_jsonl_events(&state, run_id);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["seq"], 4);
        assert_eq!(events[1]["seq"], 5);
    }

    #[test]
    fn concurrent_worker_events_keep_whole_jsonl_rows_and_sequence() {
        let (_dir, state) = test_state();
        let run_id = "run-concurrent-jsonl";
        ensure_run_record(&state, run_id).unwrap();
        state.runs.handle(&state, run_id);
        let writers: Vec<_> = (0..2)
            .map(|index| {
                let state = state.clone();
                std::thread::spawn(move || {
                    for step in 0..100 {
                        emit_event(
                            &state,
                            run_id,
                            Some(if index == 0 { "cli-pi#1" } else { "cli-pi#2" }),
                            "terminal.output",
                            json!({"step":step,"text":"x".repeat(8192)}),
                        );
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let path = session_dir(&state, run_id).join("events.jsonl");
        let raw = fs::read_to_string(path).unwrap();
        let rows: Vec<Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 200);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row["seq"].as_u64(), Some(index as u64 + 1));
        }
    }

    #[test]
    fn dispatch_contract_normalizes_flag_formats_and_category() {
        let (parsed, norm) = prepare_dispatch_contract(&json!({
            "name": "baby_s_first",
            "category": "auto",
            "description": "Reverse this python flag_checker script",
            "flag_format": "brace",
            "mode": "ctf"
        }))
        .unwrap();
        assert_eq!(parsed["name"], "baby_s_first");
        assert_eq!(parsed["category"], "rev");
        assert_eq!(norm["flag_format"], "flag{...}");
        assert_eq!(
            norm["task_contract"]["completion"]["flag_format"],
            "flag{...}"
        );
    }

    #[test]
    fn blackboard_ingress_drain_records_facts_dead_ends_and_flags() {
        let (_dir, state) = test_state();
        let run_id = "run-0001";
        ensure_run_record(&state, run_id).unwrap();
        let sdir = session_dir(&state, run_id);
        let (_, _, ingress) =
            worker::stage_workspace(&sdir, "cli-claude#1", "claude", &json!({"mode": "ctf"}))
                .unwrap();

        let graph = graph::SharedGraph::open(&state, run_id).unwrap();
        graph.propose_intent("reason", "step_001", "Inspect archive", &json!({}), None);
        assert!(graph.claim_intent("cli-claude#1", "step_001", 600.));
        fs::write(
            ingress.join("request-submit_fact-req1.json"),
            json!({
                "protocol": "muteki-blackboard-v2",
                "request_id": "req1",
                "operation": "submit_fact",
                "worker_id": "cli-claude#1",
                "payload": {
                    "title": "Password check",
                    "content": "Script compares input against csawctf{test_flag}",
                    "evidence_artifact_id": "art-1"
                }
            })
            .to_string(),
        )
        .unwrap();

        fs::write(
            ingress.join("request-submit_flag-req2.json"),
            json!({
                "protocol": "muteki-blackboard-v2",
                "request_id": "req2",
                "operation": "submit_flag",
                "worker_id": "cli-claude#1",
                "payload": {
                    "flag": "csawctf{test_flag}"
                }
            })
            .to_string(),
        )
        .unwrap();

        let (solved, finish) =
            worker::drain_blackboard_ingress(&state, run_id, "cli-claude#1", &ingress, &sdir);
        assert!(solved);
        assert!(finish);

        let res1: Value =
            serde_json::from_str(&fs::read_to_string(ingress.join("result-req1.json")).unwrap())
                .unwrap();
        assert_eq!(res1["ok"], true);
        assert_eq!(
            res1["message"],
            "已记入草稿。核对无误即调 commit-step 定稿收束。"
        );

        let res2: Value =
            serde_json::from_str(&fs::read_to_string(ingress.join("result-req2.json")).unwrap())
                .unwrap();
        assert_eq!(res2["ok"], true);
        assert_eq!(res2["message"], "SUBMITTED");

        let stored = state.store.get("runs", run_id).unwrap().unwrap();
        assert_eq!(stored["solved"], true);
        assert_eq!(stored["flag"], "csawctf{test_flag}");
    }

    #[test]
    fn stale_worker_ingress_cannot_submit_flag_to_reopened_run() {
        let (_dir, state) = test_state();
        let run_id = "run-ingress-generation";
        let mut run = ensure_run_record(&state, run_id).unwrap();
        run["execution_generation"] = json!(2);
        state.store.put("runs", run_id, &run).unwrap();
        let sdir = session_dir(&state, run_id);
        let (_, _, ingress) =
            worker::stage_workspace(&sdir, "cli-pi#1-g2", "pi", &json!({"mode":"ctf"})).unwrap();
        fs::write(
            ingress.join("request-submit_flag-old.json"),
            json!({"protocol":"muteki-blackboard-v2","request_id":"old",
                "operation":"submit_flag","worker_id":"cli-pi#1",
                "execution_generation":1,"flag":"flag{old}"})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            worker::drain_blackboard_ingress(&state, run_id, "cli-pi#1-g2", &ingress, &sdir),
            (false, false)
        );
        assert_eq!(
            state.store.get("runs", run_id).unwrap().unwrap()["flags"],
            json!([])
        );
        let rejected: Value =
            serde_json::from_slice(&fs::read(ingress.join("result-old.json")).unwrap()).unwrap();
        assert_eq!(rejected["code"], "run.execution_generation_stale");
        fs::write(
            ingress.join("request-submit_flag-current.json"),
            json!({"protocol":"muteki-blackboard-v2","request_id":"current",
                "operation":"submit_flag","worker_id":"cli-pi#1-g2",
                "execution_generation":2,"flag":"flag{current}"})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            worker::drain_blackboard_ingress(&state, run_id, "cli-pi#1-g2", &ingress, &sdir),
            (true, true)
        );
        assert_eq!(
            state.store.get("runs", run_id).unwrap().unwrap()["flags"],
            json!(["flag{current}"])
        );
    }

    #[test]
    fn late_worker_events_do_not_enter_a_new_execution_generation() {
        let (_dir, state) = test_state();
        let run_id = "run-late-worker";
        let mut run = ensure_run_record(&state, run_id).unwrap();
        run["execution_generation"] = json!(2);
        state.store.put("runs", run_id, &run).unwrap();
        let old = emit_event(
            &state,
            run_id,
            Some("cli-pi#1"),
            "worker.status",
            json!({"status":"online"}),
        );
        assert_eq!(old["fenced"], "stale_execution_generation");
        let current = emit_event(
            &state,
            run_id,
            Some("cli-pi#1-g2"),
            "worker.status",
            json!({"status":"online"}),
        );
        assert_eq!(current["seq"], 1);
        let stored = read_jsonl_events(&state, run_id);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0]["solver_id"], "cli-pi#1-g2");
    }

    #[tokio::test]
    async fn answering_one_need_input_card_leaves_the_other_open() {
        let (_dir, state) = test_state();
        let run_id = "i27-iso-decisions";
        ensure_run_record(&state, run_id).unwrap();
        let sdir = session_dir(&state, run_id);
        let (_, _, ingress) =
            worker::stage_workspace(&sdir, "cli-pi#1", "pi", &json!({"mode":"ctf"})).unwrap();
        for (name, need) in [("i27a", "隔离决策甲"), ("i27b", "隔离决策乙")] {
            fs::write(
                ingress.join(format!("request-need_input-{name}.json")),
                json!({
                    "protocol": "muteki-blackboard-v2",
                    "request_id": name,
                    "operation": "need_input",
                    "worker_id": "cli-pi#1",
                    "execution_generation": 1,
                    "payload": {"need": need}
                })
                .to_string(),
            )
            .unwrap();
        }
        assert_eq!(
            worker::drain_blackboard_ingress(&state, run_id, "cli-pi#1", &ingress, &sdir),
            (false, false)
        );
        let open = graph::SharedGraph::open(&state, run_id)
            .unwrap()
            .open_decisions()
            .unwrap();
        assert_eq!(open.len(), 2, "{open:?}");
        assert_eq!(open[0]["need"], "隔离决策甲");
        assert_eq!(open[1]["need"], "隔离决策乙");
        let first = open[0]["request_id"].as_str().unwrap().to_owned();
        let second = open[1]["request_id"].as_str().unwrap().to_owned();
        let response = control_run(
            State(state.clone()),
            Path(run_id.to_owned()),
            Json(json!({
                "command_id": "i27-answer-first",
                "action": "answer_decision",
                "target": "global",
                "request_id": first,
                "text": "只回答第一张隔离卡"
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["effect"]["kind"], "decision_answered");
        assert_eq!(body["effect"]["result"]["request_ids"], json!([first]));
        let still = graph::SharedGraph::open(&state, run_id)
            .unwrap()
            .open_decisions()
            .unwrap();
        assert_eq!(still.len(), 1, "{still:?}");
        assert_eq!(still[0]["request_id"], second);
        assert_eq!(still[0]["need"], "隔离决策乙");
        let resolved = read_jsonl_events(&state, run_id)
            .into_iter()
            .find(|event| event["event_type"] == "hitl.resolved")
            .unwrap();
        assert_eq!(resolved["payload"]["request_ids"], json!([first]));
        assert_eq!(resolved["payload"]["action"], "answer_decision");
    }

    #[tokio::test]
    async fn control_journal_enforces_cas_and_redacts_secrets() {
        let (_dir, state) = test_state();
        let run_id = "run-0002";
        ensure_run_record(&state, run_id).unwrap();

        let resp = control_run(
            State(state.clone()),
            Path(run_id.to_owned()),
            Json(json!({
                "command_id": "cmd-1",
                "action": "pause",
                "target": "global",
                "expected_generation": 0,
                "secret": "super-secret-value"
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let stale = control_run(
            State(state.clone()),
            Path(run_id.to_owned()),
            Json(json!({
                "command_id": "cmd-2",
                "action": "resume",
                "target": "global",
                "expected_generation": 0
            })),
        )
        .await;
        assert_eq!(stale.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn shared_graph_deduplicates_facts_extracts_credentials_and_exports_sqlite() {
        let (_dir, state) = test_state();
        let run_id = "run-graph-test";
        ensure_run_record(&state, run_id).unwrap();

        let g = graph::SharedGraph::open(&state, run_id).unwrap();
        let origin_seq = g.seed_origin_fact(&json!({
            "name": "vault-login",
            "category": "web",
            "flag_format": "flag{...}"
        }));
        assert!(origin_seq > 0);

        let (obs1, fact1) = g.add_evidence(
            "cli-claude#1",
            "Admin credentials",
            "Verified login for user=admin with password=S3cretVault!2026",
            Some("art-1"),
            true,
            1.0,
            Some("step_001"),
            None,
            None,
        );
        assert!(obs1 > 0);
        let fact1_seq = fact1.expect("first fact should be admitted");

        let (obs2, fact2) = g.add_evidence(
            "cli-codex#1",
            "Admin credentials duplicate",
            "Verified   login for user=admin   with password=S3cretVault!2026",
            Some("art-2"),
            true,
            1.0,
            Some("step_002"),
            None,
            None,
        );
        assert!(obs2 > obs1);
        assert_eq!(fact2, Some(fact1_seq));

        let creds = g.canonical_credentials();
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0]["entity"], "admin");
        assert_eq!(creds[0]["value"], "S3cretVault!2026");

        let p_seq = g.propose_intent(
            "reason",
            "step_001",
            "Inspect login handler",
            &json!({
                "worker_class": "code",
                "priority": "high",
                "expected_observable": "Auth check logic",
                "stop_condition": "Handler verified",
                "coverage_key": "web:auth_handler"
            }),
            Some(&[origin_seq]),
        );
        assert!(p_seq > 0);
        assert_eq!(g.open_dispatchable_intents().len(), 1);
        assert!(g.claim_intent("cli-claude#1", "step_001", 300.0));
        assert_eq!(g.open_dispatchable_intents().len(), 0);
        let c_seq = g.conclude_intent("cli-claude#1", "step_001", "done", "completed");
        assert!(c_seq > 0);

        let bytes = g.export_sqlite_bytes().unwrap();
        assert!(bytes.starts_with(b"SQLite format 3"));
    }

    #[test]
    fn coordinator_parses_reason_reply_deduplicates_coverage_and_rotates_seats() {
        let (_dir, state) = test_state();
        let run_id = "run-coord-test";
        ensure_run_record(&state, run_id).unwrap();
        let g = graph::SharedGraph::open(&state, run_id).unwrap();

        let raw_reply = r#"```json
        {
          "verdict": "explore",
          "goal_met": false,
          "progress": {
            "summary": "已定位逆向校验函数，准备静态分析与脚本求解并行推进。",
            "confirmed": ["ELF 64-bit binary"],
            "active": ["Static decompilation"],
            "blocked": [],
            "next": ["Extract XOR table", "Write z3 solver"]
          },
          "intents": [
            {
              "id": "I1",
              "from": [1],
              "goal": "Extract XOR key table from rodata",
              "priority": "high",
              "expected_observable": "32-byte key array",
              "stop_condition": "Bytes dumped and verified",
              "coverage_key": "rev:rodata_xor"
            },
            {
              "id": "I2",
              "from": [1],
              "goal": "Duplicate coverage key should be filtered",
              "priority": "normal",
              "expected_observable": "Same table",
              "stop_condition": "Done",
              "coverage_key": "rev:rodata_xor"
            }
          ]
        }
        ```"#;

        let parsed = coordinator::parse_reason_reply(raw_reply, 4);
        assert_eq!(parsed.verdict, "explore");
        assert!(!parsed.goal_met);
        assert_eq!(parsed.intents.len(), 2);

        let dispatched = coordinator::dispatch_ctf_intents(&g, &parsed, "reason");
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0]["intent_id"], "step_001");
        assert_eq!(dispatched[0]["coverage_key"], "rev:rodata_xor");

        let seats = vec![
            worker::WorkerSpec {
                solver_id: "cli-claude#1".to_owned(),
                profile_id: "wp-1".to_owned(),
                profile_label: "Claude".to_owned(),
                raw_engine: "claude-code".to_owned(),
                norm_engine: "claude".to_owned(),
                adapter_id: "claude".to_owned(),
                instance_id: "default".to_owned(),
                model: None,
                effort: None,
                account_id: String::new(),
                effective_credential_id: "cred-1".to_owned(),
                backend: "local".to_owned(),
                profile: Value::Null,
            },
            worker::WorkerSpec {
                solver_id: "cli-codex#1".to_owned(),
                profile_id: "wp-2".to_owned(),
                profile_label: "Codex".to_owned(),
                raw_engine: "codex".to_owned(),
                norm_engine: "codex".to_owned(),
                adapter_id: "codex".to_owned(),
                instance_id: "default".to_owned(),
                model: None,
                effort: None,
                account_id: String::new(),
                effective_credential_id: "cred-2".to_owned(),
                backend: "local".to_owned(),
                profile: Value::Null,
            },
        ];
        let busy = HashSet::new();
        let mut failed = HashSet::new();
        failed.insert("claude".to_owned());
        let used = HashSet::new();
        let mut cursor = 0;
        let chosen =
            coordinator::pick_worker_seat(&seats, &busy, Some(&failed), &used, &mut cursor)
                .unwrap();
        assert_eq!(chosen.solver_id, "cli-codex#1");
    }

    #[test]
    fn replay_hydration_restores_fgs_lineage_on_active_and_cleanly_closed_wal_dbs() {
        let dir = tempfile::tempdir().unwrap();
        let spaced_root = dir.path().join("Application Support");
        fs::create_dir_all(&spaced_root).unwrap();
        let store = crate::store::Store::open(spaced_root.join("muteki.sqlite")).unwrap();
        store
            .put("system", "state_dir", &json!(spaced_root))
            .unwrap();
        let state = Arc::new(AppState::new(store));
        let run_id = "run-hydration-test";
        ensure_run_record(&state, run_id).unwrap();

        let g = graph::SharedGraph::open(&state, run_id).unwrap();
        let origin_seq = g.seed_origin_fact(&json!({
            "name": "vault-login",
            "category": "web",
            "description": "http://example.test/\nSolve this"
        }));
        let (_obs, fact1) = g.add_evidence(
            "cli-pi",
            "Endpoint found",
            "Discovered /download endpoint",
            None,
            true,
            1.0,
            Some("step_001"),
            None,
            None,
        );
        let fact1_seq = fact1.unwrap();
        let seq1 = g.propose_intent(
            "reason",
            "step_001",
            "Recon root page",
            &json!({"worker_class": "code", "priority": "high", "route_hash": "route-a", "lane_key": "lane-a"}),
            Some(&[origin_seq]),
        );
        assert!(seq1 > 0);
        let seq2 = g.propose_intent(
            "reason",
            "step_002",
            "Follow up download",
            &json!({"worker_class": "code", "priority": "high", "depends_on": ["step_001"], "required_pocs": ["poc-1"]}),
            Some(&[fact1_seq]),
        );
        assert!(seq2 > 0);
        let rev_seq = g
            .pentest_decide_supersede_fact("challenger", fact1_seq, "stale hypothesis", None)
            .unwrap();
        assert!(rev_seq > 0);

        let active_hydration = load_graph_replay_hydration(&state, run_id).unwrap();
        assert!(active_hydration.origin_fact.is_some());
        assert_eq!(
            active_hydration.intent_lineage["step_001"].from_facts,
            vec![origin_seq]
        );
        assert_eq!(
            active_hydration.intent_lineage["step_002"].from_facts,
            vec![fact1_seq]
        );
        assert_eq!(
            active_hydration.intent_lineage["step_002"].depends_on,
            vec!["step_001"]
        );
        assert_eq!(
            active_hydration.intent_lineage["step_002"].required_pocs,
            vec!["poc-1"]
        );
        assert_eq!(active_hydration.fact_revisions.len(), 1);

        drop(g);
        let db_path = graph::graph_dir(&state, run_id).join("shared_graph.db");
        let _ = fs::remove_file(db_path.with_file_name("shared_graph.db-wal"));
        let _ = fs::remove_file(db_path.with_file_name("shared_graph.db-shm"));

        let closed_hydration = load_graph_replay_hydration(&state, run_id).unwrap();
        assert_eq!(
            closed_hydration.intent_lineage["step_002"].from_facts,
            vec![fact1_seq]
        );

        let mut seen = HydrationSeen::default();
        let mut origin_ev = json!({
            "seq": 1,
            "ts": 100.0,
            "run_id": run_id,
            "event_type": "blackboard.delta",
            "solver_id": "coordinator",
            "payload": {"kind": "fact_origin", "fact_seq": origin_seq, "actor": "coordinator"},
        });
        enrich_replay_event(&mut origin_ev, &closed_hydration, &mut seen);
        assert_eq!(origin_ev["solver_id"], "origin");
        assert_eq!(origin_ev["payload"]["actor"], "origin");
        assert!(
            origin_ev["payload"]["fact"]
                .as_str()
                .unwrap()
                .contains("http://example.test/")
        );
        assert!(seen.origin_fact);

        let mut prop_ev = json!({
            "seq": 2,
            "ts": 101.0,
            "run_id": run_id,
            "event_type": "blackboard.delta",
            "solver_id": "reason",
            "payload": {"kind": "intent_proposed", "intent_id": "step_002", "goal": "Follow up download"},
        });
        enrich_replay_event(&mut prop_ev, &closed_hydration, &mut seen);
        assert_eq!(prop_ev["payload"]["from_facts"], json!([fact1_seq]));
        assert_eq!(prop_ev["payload"]["depends_on"], json!(["step_001"]));
        assert_eq!(prop_ev["payload"]["required_pocs"], json!(["poc-1"]));

        let tail = hydration_tail_events(run_id, 2, &closed_hydration, &seen);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0]["payload"]["kind"], "fact_superseded");
        assert_eq!(tail[0]["payload"]["fact_seq"], fact1_seq);
        assert_eq!(tail[0]["solver_id"], "challenger");
    }

    #[test]
    fn publish_intent_proposals_preserves_fgs_lineage_fields() {
        let (_dir, state) = test_state();
        let run_id = "run-proposal-fields";
        ensure_run_record(&state, run_id).unwrap();
        let proposed = vec![json!({
            "intent_id": "step_009",
            "goal": "Derive signing rule",
            "worker_class": "code",
            "priority": "high",
            "from_facts": [11, 27],
            "depends_on": ["step_001"],
            "required_pocs": ["poc-1"],
            "route_hash": "route-sign",
            "lane_key": "lane-sign",
            "expected_observable": "Rule recovered",
            "stop_condition": "Tested",
            "coverage_key": "web:sign",
        })];
        coordinator::publish_intent_proposals(&state, run_id, &proposed, "challenger");
        let events = read_jsonl_events(&state, run_id);
        let ev = events
            .iter()
            .find(|item| {
                item["event_type"] == "blackboard.delta"
                    && item["payload"]["kind"] == "intent_proposed"
            })
            .unwrap();
        assert_eq!(ev["solver_id"], "challenger");
        assert_eq!(ev["payload"]["actor"], "challenger");
        assert_eq!(ev["payload"]["from_facts"], json!([11, 27]));
        assert_eq!(ev["payload"]["depends_on"], json!(["step_001"]));
        assert_eq!(ev["payload"]["required_pocs"], json!(["poc-1"]));
        assert_eq!(ev["payload"]["route_hash"], "route-sign");
        assert_eq!(ev["payload"]["lane_key"], "lane-sign");
    }
}
