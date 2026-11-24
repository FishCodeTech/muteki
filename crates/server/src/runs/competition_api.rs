//! Live competition and platform operations. Imported records stay read-only.

pub(crate) mod advisor;
mod archive_commands;
mod artifacts;
mod binding_outbox;
mod challenge_commands;
mod connection_refs;
mod connection_outbox;
mod control_commands;
mod extension_adapter;
mod gate_recovery;
mod gateway;
mod leases;
mod messages;
mod projections;
mod scheduler;
mod visit;
mod sync_projection;
mod platform_http;
use sync_projection::save_challenges;
mod submission_outbox;
mod submission_commands;
mod submission_retry;
mod manual_override;
mod effect_control;
mod versioned_routes;
pub(crate) use gateway::{
    canonical_command_type, dispatch_command_envelope, query_envelope, supports_command, supports_query,
};

pub(crate) fn is_binding_command(kind: &str) -> bool {
    binding_outbox::is_binding_command(kind)
}

use crate::{AppState, native_chat};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, patch, post, put},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{sync::broadcast, time::MissedTickBehavior};

const CONNECTIONS: &str = "platform_connections";
const COMPETITIONS: &str = "competitions";
const POLICIES: &str = "competition_policies_live";
const CHALLENGES: &str = "competition_challenges_live";
const REVISIONS: &str = "competition_revisions_live";
const QUEUE: &str = "competition_queue_live";
const BINDINGS: &str = "competition_bindings_live";
const LEASES: &str = "competition_leases_live";
const CANDIDATES: &str = "competition_candidates_live";
const SUBMISSIONS: &str = "competition_submissions_live";
const EVENTS: &str = "competition_events_live";
const SECRETS: &str = "platform_secret_metadata";
const CREDENTIAL_CLEANUPS: &str = "competition_direct_credential_cleanups";
const COMMANDS: &str = "platform_commands";
const VERSIONS: &str = "platform_aggregate_versions";
const BROWSER: &str = "platform_browser_sessions";
const BUDGETS: &str = "competition_resource_budgets_live";
const SYNC_CURSORS: &str = "competition_sync_cursors_live";

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}
impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "competition.invalid",
            message,
        )
    }
    fn missing(kind: &'static str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "competition.not_found",
            format!("{kind} not found"),
        )
    }
    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "Competition storage failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "competition.storage",
            "Competition storage failed",
        )
    }
    fn unsupported(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::NOT_IMPLEMENTED,
            "capability.unsupported",
            message,
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error":{"code":self.code,"message":self.message},"detail":self.message})),
        )
            .into_response()
    }
}
type ApiResult<T> = Result<T, ApiError>;
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}
fn field<'a>(v: &'a Value, name: &str) -> &'a str {
    v[name].as_str().unwrap_or("").trim()
}
fn default_policy(competition_id: &str) -> Value {
    json!({"competition_id":competition_id,"automation_mode":"assisted","max_concurrent_runs":2,"max_instances":1,
        "submission_cooldown_seconds":30.0,"category_allow":[],"category_deny":[],"budget_limits":{},
        "policy_profile":"default","round_timeboxes_s":[],"visit_floor_s":0.0,"fill_idle_revisits":false,
        "terminal_phase_fill_idle":false,"overdue_mult":1.25,"working_set":0,"keepalive_max":0,
        "keepalive_tail_s":0.0,"total_budget_s":0.0,"dry_defer_waves":0,"deepchain_slots":0,
        "per_challenge_seconds":0.0,"stuck_waves_cap":0,"challenge_order":[],
        "terminal_phase_challenge_ids":[],"persistent_challenge_ids":[],"updated_at":now()})
}
fn policy_for_connection(competition_id: &str, connection: &Value) -> Value {
    let mut policy = default_policy(competition_id);
    let hints = connection["capabilities"]["detail"]["policy_hints"].as_object();
    let profile = hints.and_then(|values|values.get("policy_profile")).and_then(Value::as_str)
        .unwrap_or("").trim();
    let profile = if profile.is_empty()
        && !matches!(field(connection,"platform_kind"),"ctfd" | "rctf" | "gzctf" | "generic_browser") {
        hints.and_then(|values|values.get("suggested_policy_profile"))
            .and_then(Value::as_str).filter(|value|!value.trim().is_empty())
            .unwrap_or("tsec_eval")
    } else {profile};
    if profile == "tsec_eval" {
        if let Some(defaults) = tsec_eval_defaults().as_object() {
            for (key,value) in defaults {policy[key] = value.clone();}
        }
    }
    if let Some(hints) = hints {
        for (key,value) in hints {
            if key != "policy_profile" && (policy.get(key).is_some()
                || tsec_eval_defaults().get(key).is_some()) {
                policy[key] = value.clone();
            }
        }
    }
    policy
}
fn remote_snapshot_content(remote: &[Value]) -> Value {
    let mut stable: Vec<Value> = remote.iter().map(|v|json!({"id":v["external_challenge_id"],"name":v["name"],
        "category":v["category"],"remote_state":v["remote_state"],"points":v["points"],"description":v["description"],
        "target":v["target"],"flag_format":v["flag_format"],"multi_flag":v["multi_flag"],"expected_flags":v["expected_flags"],
        "hints":v["hints"],"artifacts":v["artifacts"],"instance_required":v["instance_required"],"platform_type":v["platform_type"]})).collect();
    stable.sort_by(|a, b| field(a, "id").cmp(field(b, "id")));
    json!(stable)
}
struct RemoteFetch {
    challenges: Vec<Value>,
    cursor: Option<String>,
    snapshot_complete: bool,
    platform_status: Option<Value>,
    competition_clock: Option<Value>,
    notices: Vec<Value>,
    extension_id: String,
}
fn tsec_eval_defaults() -> Value {
    json!({"policy_profile":"tsec_eval","automation_mode":"autonomous","max_concurrent_runs":3,"max_instances":3,
        "submission_cooldown_seconds":1.0,"round_timeboxes_s":[],"visit_floor_s":0.0,
        "fill_idle_revisits":false,"terminal_phase_fill_idle":false,"overdue_mult":1.25,
        "working_set":3,"keepalive_max":0,"keepalive_tail_s":1800.0,"total_budget_s":21300.0,
        "dry_defer_waves":4,"deepchain_slots":1,"per_challenge_seconds":4000.0,"stuck_waves_cap":2})
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/platform-connections", post(versioned_routes::create_connection))
        .route(
            "/api/platform-connections/{id}",
            patch(versioned_routes::patch_connection).delete(versioned_routes::delete_connection),
        )
        .route(
            "/api/platform-connections/{id}/probe",
            post(versioned_routes::probe_connection),
        )
        .route(
            "/api/platform-connections/{id}/secret",
            put(versioned_routes::put_connection_secret).delete(versioned_routes::delete_connection_secret),
        )
        .route(
            "/api/platform-connections/{id}/browser-session",
            get(get_browser_session)
                .put(versioned_routes::put_browser_session)
                .delete(versioned_routes::delete_browser_session),
        )
        .route("/api/competitions", post(versioned_routes::create_competition))
        .route(
            "/api/competitions/{id}",
            patch(versioned_routes::patch_competition).delete(versioned_routes::delete_competition),
        )
        .route("/api/competitions/{id}/commands", post(versioned_routes::competition_command))
        .route("/api/competitions/{id}/messages", post(versioned_routes::competition_message))
        .merge(advisor::router())
}
pub(crate) fn is_lease_effect(state: &AppState, effect_id: &str) -> Result<bool, String> {
    leases::is_effect(state, effect_id).map_err(|error| error.message)
}
pub(crate) fn is_connection_effect(state: &AppState, effect_id: &str) -> Result<bool, String> {
    connection_outbox::is_effect(state,effect_id).map_err(|error|error.message)
}
pub(crate) fn is_submission_or_binding_effect(state: &AppState, effect_id: &str) -> Result<bool, String> {
    effect_control::is_effect(state,effect_id).map_err(|error|error.message)
}
pub(crate) async fn control_submission_or_binding_effect(state: &Arc<AppState>, envelope: &Value) -> Response {
    match effect_control::control(state,envelope).await {
        Ok(receipt) => Json(json!({"receipt":receipt})).into_response(),
        Err(error) => error.into_response(),
    }
}
pub(crate) async fn control_connection_effect(state: &Arc<AppState>, envelope: &Value) -> Response {
    match connection_outbox::control_effect(state,envelope).await {
        Ok(receipt) => Json(json!({"receipt":receipt})).into_response(),
        Err(error) => error.into_response(),
    }
}
pub(crate) async fn control_lease_effect(state: &Arc<AppState>, envelope: &Value) -> Response {
    match leases::control_effect(state, envelope).await {
        Ok(receipt) => Json(json!({"receipt":receipt})).into_response(),
        Err(error) => error.into_response(),
    }
}
pub async fn initialize(state:&Arc<AppState>)->Result<(),String>{
    projections::register(state).map_err(|error|error.message)?;
    connection_refs::reconcile(state).map_err(|error|error.message)?;
    connection_refs::reconcile_create(state).map_err(|error|error.message)?;
    connection_refs::reconcile_browser_import(state).await.map_err(|error|error.message)?;
    connection_refs::reconcile_browser_session(state).await.map_err(|error|error.message)?;
    connection_refs::reconcile_credential(state).map_err(|error|error.message)?;
    connection_refs::reconcile_direct_cleanup(state).await.map_err(|error|error.message)?;
    connection_refs::reconcile_unregistered(state).map_err(|error|error.message)?;
    archive_commands::reconcile(state).map_err(|error|error.message)?;
    challenge_commands::reconcile(state).map_err(|error|error.message)?;
    control_commands::reconcile(state).map_err(|error|error.message)?;
    submission_commands::reconcile(state).map_err(|error|error.message)?;
    submission_retry::reconcile(state).map_err(|error|error.message)?;
    manual_override::reconcile(state).map_err(|error|error.message)?;
    effect_control::reconcile(state).await.map_err(|error|error.message)?;
    connection_outbox::reconcile_missing(state).map_err(|error|error.message)?;
    leases::reconcile_startup(state).await.map_err(|error|error.message)?;
    binding_outbox::reconcile_startup(state).await.map_err(|error|error.message)?;
    match reconcile_orphan_bindings(state).await {
        Err(error) if error.code=="competition.binding.projection_pending"=>{
            tracing::warn!(%error.message,"Some Competition bindings still await stop/projection evidence; independent bindings remain available");
        },
        Err(error)=>return Err(error.message),
        Ok(())=>{},
    }
    submission_outbox::reconcile_startup(state).await.map_err(|error|error.message)?;
    gate_recovery::reconcile(state).map_err(|error|error.message)?;
    Ok(())
}

pub fn start_loops(state: Arc<AppState>) {
    let projection_state = state.clone();
    crate::business::spawn(state.clone(),crate::business::Module::Ctf,"ctf.projections",async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if projection_state.admission.draining() || crate::business::require_descriptor(&projection_state,crate::business::Module::Ctf,"builtin.competition").is_err(){continue;}
            if let Err(error) = projections::consume(&projection_state) {
                tracing::warn!(code=%error.code,"Competition projection consumption failed");
            }
        }
    });

    let connection_state = state.clone();
    crate::business::spawn(state.clone(),crate::business::Module::Ctf,"ctf.connections",async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if connection_state.admission.draining() || crate::business::require_descriptor(&connection_state,crate::business::Module::Ctf,"builtin.competition").is_err(){continue;}
            if let Err(error) = connection_outbox::maintenance(&connection_state).await {
                tracing::warn!(code=%error.code,"Competition connection outbox delivery failed");
            }
        }
    });
    let binding_state = state.clone();
    crate::business::spawn(state.clone(),crate::business::Module::Ctf,"ctf.bindings",async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if binding_state.admission.draining() || crate::business::require_descriptor(&binding_state,crate::business::Module::Ctf,"builtin.competition").is_err(){continue;}
            if let Err(error) = binding_outbox::maintenance(&binding_state).await {
                tracing::warn!(code=%error.code,"Competition binding outbox delivery failed");
            }
        }
    });
    let submission_state = state.clone();
    crate::business::spawn(state.clone(),crate::business::Module::Ctf,"ctf.submissions",async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if submission_state.admission.draining() || crate::business::require_descriptor(&submission_state,crate::business::Module::Ctf,"builtin.competition").is_err(){continue;}
            let competitions = match submission_state.store.list(COMPETITIONS) {
                Ok(records) => records,
                Err(error) => {
                    tracing::error!(%error,"Competition submission loop could not load records");
                    continue;
                }
            };
            for competition in competitions {
                if competition["historical"] == true || competition["archived"] == true {continue;}
                let competition_id = field(&competition,"competition_id");
                if let Err(error) = submission_outbox::maintenance(&submission_state,competition_id).await {
                    tracing::warn!(competition_id,code=%error.code,"Competition submission pump failed");
                }
            }
        }
    });
    let maintenance_state = state.clone();
    crate::business::spawn(state.clone(),crate::business::Module::Ctf,"ctf.maintenance",async move {
        let state = maintenance_state;
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.tick().await;
        loop {
            interval.tick().await;
            if state.admission.draining() || crate::business::require_descriptor(&state,crate::business::Module::Ctf,"builtin.competition").is_err(){continue;}
            if let Err(error) = reconcile_orphan_bindings(&state).await {
                tracing::warn!(code=%error.code,"Competition orphan Run reconciliation failed");
            }
            if let Err(error) = gate_recovery::reconcile(&state) {
                tracing::warn!(code=%error.code,"Competition Gate recovery failed");
            }
            if let Err(error) = effect_control::reconcile(&state).await {
                tracing::warn!(code=%error.code,"Competition effect control recovery failed");
            }
            let competitions = match state.store.list(COMPETITIONS) {
                Ok(records) => records,
                Err(error) => {
                    tracing::error!(%error,"Competition loop could not load records");
                    continue;
                }
            };
            for competition in competitions {
                if competition["historical"] == true || competition["archived"] == true {
                    continue;
                }
                let competition_id = field(&competition, "competition_id").to_owned();
                if let Err(error) = leases::maintenance(&state, &competition_id).await {
                    tracing::warn!(competition_id,code=%error.code,"Competition lease maintenance failed");
                }
                let age = competition["last_sync_attempt_at"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|at| chrono::Utc::now().signed_duration_since(at).num_seconds())
                    .unwrap_or(i64::MAX);
                if age >= 60 {
                    if let Err(error) = sync_live_competition(&state, &competition).await {
                        tracing::warn!(competition_id,%error.message,"Competition sync failed");
                    }
                }
            }
        }
    });
    crate::business::spawn(state.clone(),crate::business::Module::Ctf,"ctf.scheduler",async move {
        let mut scheduler_owner = None;
        loop {
            if state.admission.draining() || crate::business::require_descriptor(&state,crate::business::Module::Ctf,"builtin.competition").is_err(){tokio::time::sleep(Duration::from_millis(100)).await;continue;}
            let competitions = match state.store.list(COMPETITIONS) {
                Ok(records) => records,
                Err(error) => {
                    tracing::error!(%error,"Competition scheduler could not load records");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            for competition in competitions {
                if competition["historical"] == true || competition["archived"] == true
                    || competition["scheduler_state"] != "running" { continue; }
                if scheduler_owner.is_none() {
                    match scheduler::claim_service_owner(&state) {
                        Ok(owner) => scheduler_owner = owner,
                        Err(error) => tracing::warn!(%error,"Competition scheduler ownership failed"),
                    }
                }
                if scheduler_owner.is_some() {
                    let competition_id = field(&competition, "competition_id");
                    if let Err(error) = dispatch_queued_with_advisor(state.clone(), competition_id).await {
                        tracing::warn!(competition_id,%error.message,"Competition tick failed");
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}
async fn reconcile_orphan_bindings(state:&Arc<AppState>)->ApiResult<()>{
    let mut errors=Vec::new();
    for candidate in state.store.list(BINDINGS).map_err(ApiError::internal)?{
        if let Err(error)=reconcile_orphan_binding(state,&candidate).await{
            tracing::warn!(binding_id=field(&candidate,"binding_id"),code=error.code,%error.message,"Orphan binding recovery remains pending");errors.push(error.message);
        }
    }
    if errors.is_empty(){Ok(())}else{Err(ApiError::new(StatusCode::CONFLICT,"competition.binding.projection_pending",errors.join("; ")))}
}
async fn reconcile_orphan_binding(state:&Arc<AppState>,candidate:&Value)->ApiResult<()>{
    let binding_id = field(&candidate, "binding_id").to_owned();
    let _binding_lock = submission_outbox::submission_lock(state,
        &format!("binding-{binding_id}")).await?;
    let Some(binding) = state.store.get(BINDINGS, &binding_id)
        .map_err(ApiError::internal)? else { return Ok(()); };
    if binding["state"] != "running" {
        return Ok(());
    }
    let prior_binding = binding.clone();
    let run_id = field(&binding, "run_id").to_owned();
    if run_id.is_empty() || state.runs.active_run_ids().iter().any(|active|active==&run_id) {
        return Ok(());
    }
    let handle=state.runs.handle(state,&run_id);
    let Ok(_lifecycle)=handle.delete_lock.try_lock() else{return Ok(())};
    if state.runs.active_run_ids().iter().any(|active|active==&run_id){return Ok(());}
    if !state.runs.claim_execution_owner(state, &run_id).map_err(ApiError::internal)?
        || state.runs.active_worker_count(&run_id) > 0 {
        return Ok(());
    }
    let competition_id = field(&binding, "competition_id").to_owned();
    let challenge_id = field(&binding, "competition_challenge_id").to_owned();
    let Some(owner_run)=state.store.get("runs",&run_id).map_err(ApiError::internal)? else{return Ok(())};
    if prior_binding["execution_generation"].as_u64().is_some_and(|generation|generation>0&&owner_run["execution_generation"].as_u64()!=Some(generation)){
        tracing::warn!(run_id,binding_id,"Stale orphan binding cannot stop a newer Run execution");return Ok(());
    }
    super::container_exec::teardown_run_container_recorded(state,&run_id).await.map_err(|message|ApiError::new(StatusCode::CONFLICT,"competition.binding.stop_unconfirmed",message))?;
    super::mark_run_finished(
        state,
        &run_id,
        None,
        false,
        "process_restart",
        Some("Competition worker did not survive service restart"),
    );
    state.store.document_scope(||->ApiResult<()>{
        let Some(run)=state.store.get("runs",&run_id).map_err(ApiError::internal)? else{return Ok(())};
        let current=state.store.get(BINDINGS,&binding_id).map_err(ApiError::internal)?;
        if current.as_ref()!=Some(&prior_binding){return Ok(());}
        if run["running"]==true{return Err(ApiError::new(StatusCode::CONFLICT,"competition.binding.terminal_pending","Run terminal commit is pending"));}
        let generation=run["execution_generation"].as_u64().unwrap_or(0);
        if prior_binding["execution_generation"].as_u64().is_some_and(|prior|prior>0&&prior!=generation){return Err(ApiError::new(StatusCode::CONFLICT,"competition.binding.execution_stale","Orphan binding no longer owns this Run generation"));}
        let challenge=state.store.get(CHALLENGES,&challenge_id).map_err(ApiError::internal)?;
        let queue=state.store.get(QUEUE,&challenge_id).map_err(ApiError::internal)?;
        let execution=visit::Execution{run_id:run_id.clone(),generation};
        let competing=scoped(state,BINDINGS,&competition_id)?.iter().any(|other|
            other["competition_challenge_id"]==challenge_id&&other["binding_id"]!=binding_id
                &&matches!(field(other,"state"),"planned"|"creating"|"starting"|"running"|"resolving"|"local_finished"|"remote_pending"|"paused"));
        let owns_projection=match &queue{
            Some(row) if !row["visit"].is_object()&&!field(&row["admission_decision"],"binding_id").is_empty()&&row["admission_decision"]["binding_id"]!=binding_id=>false,
            Some(row)=>visit::matches_execution(row,&execution)?.unwrap_or(!competing),
            None=>!competing,
        };
        let mut next_run=run.clone();next_run["status"]=json!("interrupted");
        let mut next_binding=binding.clone();next_binding["state"]=json!("paused");next_binding["execution_generation"]=json!(generation);next_binding["updated_at"]=json!(now());
        let mut next_challenge=if owns_projection{challenge.clone()}else{None};
        if let Some(row)=next_challenge.as_mut(){if !matches!(field(row,"state"),"candidate_found"|"submitting"|"solved"|"skipped"|"exhausted"|"failed"|"retired"){row["state"]=json!("paused");row["updated_at"]=json!(now());}}
        let mut next_queue=if owns_projection{queue.clone()}else{None};
        if let Some(row)=next_queue.as_mut(){
            let count=scoped(state,CANDIDATES,&competition_id)?.iter().filter(|candidate|candidate["competition_challenge_id"]==challenge_id).count() as u64;
            visit::finish(row,Some(&visit::Execution{run_id:run_id.clone(),generation}),count)?;
            if matches!(field(row,"state"),"running"|"dispatching"|"dispatched"){row["state"]=json!("done");row["updated_at"]=json!(now());}
        }
        let mut expected=vec![("runs",run_id.as_str(),Some(&run)),(BINDINGS,binding_id.as_str(),Some(&prior_binding))];
        let mut records=vec![("runs",run_id.as_str(),&next_run),(BINDINGS,binding_id.as_str(),&next_binding)];
        if let(Some(before),Some(next))=(&challenge,&next_challenge){expected.push((CHALLENGES,challenge_id.as_str(),Some(before)));records.push((CHALLENGES,challenge_id.as_str(),next));}
        if let(Some(before),Some(next))=(&queue,&next_queue){expected.push((QUEUE,challenge_id.as_str(),Some(before)));records.push((QUEUE,challenge_id.as_str(),next));}
        let event=leases::planned_event(&competition_id,"run_binding",&binding_id,"competition.run_binding.state_changed","",json!({"challenge_id":challenge_id,"run_id":run_id,"execution_generation":generation,"from":"running","to":"paused","reason":"process_restart"}));
        if !put_records_with_events(state,&competition_id,&expected,&records,&[event])?{return Err(ApiError::new(StatusCode::CONFLICT,"competition.binding.projection_pending","Orphan binding projection changed; retry"));}
        Ok(())
    }).map_err(ApiError::internal)??;
    Ok(())
}

fn save_queue_decision(state:&AppState,id:&str,before:&Value,next:&Value)->ApiResult<()>{if !state.store.put_many_if(&[(QUEUE,id,Some(before))],&[(QUEUE,id,next)]).map_err(ApiError::internal)?{return Err(ApiError::new(StatusCode::CONFLICT,"competition.queue.projection_pending","Queue changed while projecting visit; retry against current state"));}Ok(())}
fn merge_sync_metadata(state:&AppState,original:&Value,updated:&Value)->ApiResult<()>{
    let id=field(original,"competition_id");
    for _ in 0..32{
        let current=state.store.get(COMPETITIONS,id).map_err(ApiError::internal)?.ok_or_else(||ApiError::new(StatusCode::CONFLICT,"competition.sync.target_changed","Competition was removed during synchronization"))?;
        if ["connection_id","external_competition_id"].iter().any(|key|current[*key]!=original[*key]){return Err(ApiError::new(StatusCode::CONFLICT,"competition.sync.target_changed","Competition connection changed during synchronization"))}
        let mut next=current.clone();for key in ["last_sync_attempt_at","last_synced_at","last_sync_error","updated_at","platform_status","competition_clock","starts_at","ends_at"]{if original.get(key)!=updated.get(key){if let Some(value)=updated.get(key){next[key]=value.clone();}}}
        if state.store.put_many_if(&[(COMPETITIONS,id,Some(&current))],&[(COMPETITIONS,id,&next)]).map_err(ApiError::internal)?{return Ok(())}
    }Err(ApiError::new(StatusCode::CONFLICT,"competition.sync.concurrent_update","Competition changed repeatedly during synchronization"))
}
async fn sync_live_competition(state: &Arc<AppState>, competition: &Value) -> ApiResult<()> {
    let competition_id = field(competition, "competition_id");
    let mut updated = competition.clone();
    updated["last_sync_attempt_at"] = json!(now());
    merge_sync_metadata(state,competition,&updated)?;
    let connection = live_connection(state, field(competition, "connection_id"))?;
    let remote = match fetch_remote_challenges(
        state,
        &connection,
        field(competition, "external_competition_id"),
    )
    .await
    {
        Ok(remote) => remote,
        Err(error) => {
            updated["last_sync_error"] = json!(error.message);
            merge_sync_metadata(state,competition,&updated)?;
            if matches!(
                error.code,
                "competition.auth_required" | "competition.credential_expired"
            ) {
                let connection_id = field(&connection, "connection_id");
                let mut blocked = connection.clone();
                blocked["status"] = json!("auth_required");
                blocked["last_error"] = json!(error.message);
                blocked["updated_at"] = json!(now());
                state.store.put_many_if(&[(CONNECTIONS,connection_id,Some(&connection))],&[(CONNECTIONS,connection_id,&blocked)]).map_err(ApiError::internal)?;
                append_event(
                    state,
                    competition_id,
                    "competition",
                    competition_id,
                    "competition.scheduler.dispatch_paused",
                    "",
                    json!({"competition_id":competition_id,"reason":"auth_required","operation":"dispatch"}),
                )?;
            }
            return Err(error);
        }
    };
    let (platform_status_changed, _notices) =
        extension_adapter::apply_remote_sync_metadata(state, &connection, &mut updated, &remote, "")?;
    let prior_cursor = state
        .store
        .get(SYNC_CURSORS, competition_id)
        .map_err(ApiError::internal)?;
    let snapshot = if !remote.snapshot_complete && remote.challenges.is_empty() {
        prior_cursor.as_ref().map(|v|v["snapshot"].clone()).unwrap_or(Value::Null)
    } else { remote_snapshot_content(&remote.challenges) };
    let builtin = matches!(field(&connection,"platform_kind"),"ctfd" | "rctf" | "gzctf" | "generic_browser");
    let changed = prior_cursor.as_ref().is_none_or(|v| v["snapshot"] != snapshot)
        && (remote.snapshot_complete || !remote.challenges.is_empty());
    let count = if remote.snapshot_complete || !remote.challenges.is_empty() {
        match save_challenges(state, &connection, competition_id, "", &remote.challenges, remote.snapshot_complete).await {
            Ok(count) => count,
            Err(error) => {
                updated["last_sync_error"] = json!(error.message);
                merge_sync_metadata(state,competition,&updated)?;
                return Err(error);
            }
        }
    } else {
        json!({"synced_challenges":remote.challenges.len(),"artifact_failures":[]})
    };
    let cursor = remote.cursor.or_else(||if builtin { Some(if !changed {
        prior_cursor.as_ref().and_then(|v|v["cursor"].as_str()).unwrap_or("").to_owned()
    } else { id("sync") }) } else { None })
        .or_else(||prior_cursor.as_ref().and_then(|v|v["cursor"].as_str()).map(str::to_owned)).unwrap_or_default();
    if !state.store.put_many_if(&[(SYNC_CURSORS,competition_id,prior_cursor.as_ref())],&[(SYNC_CURSORS,competition_id,&json!({"competition_id":competition_id,"kind":"challenges","cursor":cursor,"snapshot":snapshot,"updated_at":now()}))]).map_err(ApiError::internal)?{return Err(ApiError::new(StatusCode::CONFLICT,"competition.sync.concurrent_update","A newer synchronization cursor is already committed"))}
    updated["last_synced_at"] = json!(now());
    let artifact_failures=count["artifact_failures"].as_array().map_or(0,Vec::len);
    updated["last_sync_error"] = if artifact_failures>0{json!(format!("{artifact_failures} challenge attachment downloads failed"))}else{json!("")};
    updated["updated_at"] = json!(now());
    merge_sync_metadata(state,competition,&updated)?;
    if changed || platform_status_changed {
        append_event(
            state,
            competition_id,
            "competition",
            competition_id,
            "competition.sync.applied",
            "",
            json!({
                "competition_id": competition_id,
                "synced_challenges": count["synced_challenges"],"sync_report":count,
                "platform_status_changed": platform_status_changed,
            }),
        )?;
    }
    Ok(())
}

fn validated_segment(value: &str) -> ApiResult<&str> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::invalid("Invalid secret reference segment"));
    }
    Ok(value)
}
fn secret_ref(connection_id: &str, key: &str) -> String {
    format!("secret://platform/{connection_id}/{key}")
}
fn parse_secret_ref(reference: &str) -> ApiResult<(&str, &str)> {
    let path = reference
        .strip_prefix("secret://platform/")
        .ok_or_else(|| ApiError::invalid("Invalid platform secret reference"))?;
    let (connection_id, key) = path
        .split_once('/')
        .ok_or_else(|| ApiError::invalid("Invalid platform secret reference"))?;
    validated_segment(connection_id)?;
    validated_segment(key)?;
    Ok((connection_id, key))
}
fn secure_dir(path: &FsPath) -> ApiResult<()> {
    fs::create_dir_all(path).map_err(ApiError::internal)?;
    if fs::symlink_metadata(path)
        .map_err(ApiError::internal)?
        .file_type()
        .is_symlink()
    {
        return Err(ApiError::invalid("Secret directory must not be a symlink"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(ApiError::internal)?;
    }
    Ok(())
}
fn secret_path(state: &AppState, connection_id: &str, key: &str) -> ApiResult<PathBuf> {
    validated_segment(connection_id)?;
    validated_segment(key)?;
    let root = native_chat::state_dir(state).join("control/competition/secrets");
    secure_dir(&root)?;
    let dir = root.join(connection_id);
    secure_dir(&dir)?;
    Ok(dir.join(format!("{key}.secret")))
}
fn write_secret(
    state: &AppState,
    connection_id: &str,
    key: &str,
    value: &str,
    expires_at: &str,
    rotation: u64,
) -> ApiResult<Value> {
    if value.is_empty() {
        return Err(ApiError::invalid("Credential must not be empty"));
    }
    let path = secret_path(state, connection_id, key)?;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(ApiError::internal)?;
    if let Err(error) = file
        .write_all(value.as_bytes())
        .and_then(|_| file.sync_all())
    {
        let _ = fs::remove_file(&temp);
        return Err(ApiError::internal(error));
    }
    fs::rename(&temp, &path).map_err(ApiError::internal)?;
    let reference = secret_ref(connection_id, key);
    let created_at = state
        .store
        .get(SECRETS, &reference)
        .map_err(ApiError::internal)?
        .and_then(|v| v["created_at"].as_str().map(str::to_owned))
        .unwrap_or_else(now);
    let metadata = json!({"reference":reference,"connection_id":connection_id,"key":key,"created_at":created_at,"updated_at":now(),"expires_at":expires_at,"rotation":rotation,"status":"active"});
    state
        .store
        .put(SECRETS, &reference, &metadata)
        .map_err(ApiError::internal)?;
    Ok(metadata)
}
fn read_secret(state: &AppState, reference: &str) -> ApiResult<String> {
    let (connection_id, key) = parse_secret_ref(reference)?;
    let record = state
        .store
        .get(SECRETS, reference)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("credential"))?;
    if record["status"] != "active"
        || record["connection_id"] != connection_id
        || record["key"] != key
    {
        return Err(ApiError::missing("credential"));
    }
    if record["expires_at"]
        .as_str()
        .filter(|s| !s.is_empty())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|at| at < chrono::Utc::now())
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "competition.credential_expired",
            "Platform credential has expired",
        ));
    }
    let path = secret_path(state, connection_id, key)?;
    let metadata = fs::symlink_metadata(&path).map_err(|_| ApiError::missing("credential"))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ApiError::invalid("Credential file is not regular"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ApiError::invalid(
                "Credential file permissions are too broad",
            ));
        }
    }
    fs::read_to_string(path).map_err(ApiError::internal)
}
pub(crate) fn resolve_platform_secret(state: &AppState, reference: &str) -> Result<String, String> {
    read_secret(state, reference).map_err(|error| error.message)
}
fn remove_secret(state: &AppState, reference: &str) -> ApiResult<()> {
    let (connection_id, key) = parse_secret_ref(reference)?;
    let path = secret_path(state, connection_id, key)?;
    if path.exists() {
        fs::remove_file(path).map_err(ApiError::internal)?;
    }
    state
        .store
        .delete(SECRETS, reference)
        .map_err(ApiError::internal)?;
    Ok(())
}

async fn notify_secret_change(state: &Arc<AppState>, old_reference: &str, new_reference: &str) {
    if let Err(error) = crate::extensions::handle_secret_change(state, old_reference, new_reference).await {
        tracing::warn!(%error, "Extension secret change notification failed");
    }
}
fn public_connection(mut record: Value) -> Value {
    record["has_credential"] = json!(!field(&record, "credential_ref").is_empty());
    if let Some(object) = record.as_object_mut() {
        object.remove("credential_ref");
    }
    record
}
fn query_connection_view(mut record: Value) -> Value {
    record["has_credential"] = json!(!field(&record, "credential_ref").is_empty());
    record
}
fn query_lease_view(mut record: Value) -> Value {
    record["has_credential"] = json!(!field(&record, "credential_ref").is_empty());
    record
}
fn live_connection(state: &AppState, connection_id: &str) -> ApiResult<Value> {
    state
        .store
        .get(CONNECTIONS, connection_id)
        .map_err(ApiError::internal)?
        .filter(|v| v["historical"] != true && v["archived"] != true)
        .ok_or_else(|| ApiError::missing("connection"))
}
fn live_competition(state: &AppState, competition_id: &str) -> ApiResult<Value> {
    state
        .store
        .get(COMPETITIONS, competition_id)
        .map_err(ApiError::internal)?
        .filter(|v| v["historical"] != true)
        .ok_or_else(|| ApiError::missing("competition"))
}
fn command_id(body: &Value) -> String {
    field(body, "command_id").to_owned()
}
fn prior_receipt(state: &AppState, command_id: &str) -> ApiResult<Option<Value>> {
    if command_id.is_empty() {
        return Ok(None);
    }
    Ok(state
        .store
        .get(COMMANDS, command_id)
        .map_err(ApiError::internal)?
        .map(|v| {
            let mut receipt = v["response"]["receipt"].clone();
            receipt["deduplicated"] = json!(true);
            receipt
        }))
}
fn planned_key_receipt(state: &AppState, admission: &Value) -> ApiResult<Option<Value>> {
    let key_id = field(admission,"key_id");
    if key_id.is_empty() {return Ok(None);}
    let Some(keyed) = state.store.get("platform_idempotency_keys",key_id).map_err(ApiError::internal)?
        else {return Ok(None)};
    if keyed["semantic_fingerprint"] != admission["semantic_fingerprint"] {
        return Err(ApiError::new(StatusCode::CONFLICT,"command.idempotency_conflict",
            "idempotency_key was used with different content"));
    }
    let prior_id = field(&keyed,"command_id");
    let prior = prior_receipt(state,prior_id)?.ok_or_else(||ApiError::new(
        StatusCode::CONFLICT,"command.outcome_unknown",
        "Prior command outcome must be inspected before retry"))?;
    Ok(Some(prior))
}
pub(crate) fn settle_planned_result(state: &AppState, command_id: &str) {
    let Ok(Some(command)) = state.store.get(COMMANDS,command_id) else {return};
    if command["response"]["receipt"]["state"] != "accepted" {return;}
    let kind = field(&command["request"],"command_type");
    if matches!(kind,"connection.create" | "competition.connection.create") {
        if let Err(error) = connection_refs::recover_create(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Connection create local effect recovery pending");
        }
        return;
    }
    if matches!(kind,"connection.test" | "competition.sync") {
        if let Err(error) = connection_outbox::recover_missing(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Competition connection outbox recovery remains pending");
        }
        return;
    }
    if kind == "connection.unregister" {
        if let Err(error) = connection_refs::finish_unregistered(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Connection archive receipt recovery pending");
        }
        return;
    }
    if kind == "competition.unregister" {
        if let Err(error) = archive_commands::finish(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Competition archive receipt recovery pending");
        }
        return;
    }
    if matches!(kind,"competition.submission.submit" | "platform_submission.approve") {
        if let Err(error) = submission_commands::recover(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Competition submission receipt recovery pending");
        }
        return;
    }
    if kind == "platform_submission.retry" {
        if let Err(error) = submission_retry::recover(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Competition submission retry recovery pending");
        }
        return;
    }
    if kind == "competition.submission.manual_override" {
        if let Err(error) = manual_override::recover(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Competition manual override recovery pending");
        }
        return;
    }
    if binding_outbox::is_binding_command(kind) {
        if let Err(error) = binding_outbox::recover_missing(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"RunBinding local outbox recovery pending");
        }
        return;
    }
    if matches!(kind,"effect.retry" | "effect.cancel") {
        if let Err(error) = effect_control::settle(state,command_id) {
            tracing::warn!(command_id,code=%error.code,"Competition effect control receipt recovery pending");
        }
        return;
    }
    let result = match kind {
        "challenge.select" | "challenge.queue" | "competition.challenge.queue" | "challenge.skip"
            => Some(("competition_challenge_effect_results",true)),
        "policy.update" | "competition.policy.update" | "scheduler.start"
            | "scheduler.pause" | "scheduler.resume"
            => Some(("competition_control_effect_results",false)),
        _ => None,
    };
    let Some((collection,challenge)) = result else {return};
    if !state.store.get(collection,command_id).is_ok_and(|result|result.is_some()) {return;}
    let settled = if challenge {challenge_commands::finish(state,command_id)}
        else {control_commands::finish(state,command_id)};
    if let Err(error) = settled {
        tracing::warn!(command_id,code=%error.code,"Competition result settlement remains pending");
    }
}
fn completed_receipt(
    command_id: &str,
    aggregate_type: &str,
    aggregate_id: &str,
    output: Value,
) -> Value {
    let cmd_id = if command_id.is_empty() {
        id("cmd")
    } else {
        command_id.to_owned()
    };
    json!({"schema_version":1,"command_id":cmd_id,"receipt_id":id("rcpt"),"state":"completed","run_id":null,
        "aggregate":{"type":aggregate_type,"id":aggregate_id},"event_cursor":null,"deduplicated":false,
        "submitted_at":now(),"completed_at":now(),"next":[],"output":output,"effect_ids":[],"error":null})
}
fn save_receipt(
    state: &AppState,
    command_id: &str,
    kind: &str,
    aggregate_type: &str,
    aggregate_id: &str,
    output: Value,
) -> ApiResult<Value> {
    let receipt = completed_receipt(command_id, aggregate_type, aggregate_id, output);
    let cmd_id = field(&receipt,"command_id");
    state.store.put(COMMANDS, &cmd_id, &json!({"request":{"command_type":kind,"aggregate_id":aggregate_id},"response":{"receipt":receipt}})).map_err(ApiError::internal)?;
    Ok(receipt)
}
fn canonical_url(raw: &str) -> ApiResult<String> {
    let input = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    let mut url =
        reqwest::Url::parse(&input).map_err(|_| ApiError::invalid("Invalid platform URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError::invalid(
            "Platform URL must be an http(s) origin without credentials or query",
        ));
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn validate_site_profile(value: &Value) -> ApiResult<Value> {
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::invalid("site_profile must be an object"))?;
    let mut profile = serde_json::Map::new();
    for (key, value) in object {
        if !matches!(
            key.as_str(),
            "login_path"
                | "probe_path"
                | "challenges_path"
                | "challenges_data_key"
                | "challenge_detail_path"
                | "challenge_detail_data_key"
                | "submit_path"
                | "flag_field"
                | "challenge_field"
                | "csrf_path"
                | "csrf_pattern"
                | "csrf_header"
                | "verdict_correct"
                | "verdict_incorrect"
        ) {
            return Err(ApiError::invalid(format!(
                "Unsupported site_profile field {key}"
            )));
        }
        if matches!(key.as_str(), "verdict_correct" | "verdict_incorrect") {
            if !value.as_array().is_some_and(|a| {
                a.len() <= 20 && a.iter().all(|v| v.as_str().is_some_and(|s| s.len() <= 80))
            }) {
                return Err(ApiError::invalid(format!(
                    "site_profile.{key} must be a short string array"
                )));
            }
        } else if !value.as_str().is_some_and(|s| s.len() <= 2048) {
            return Err(ApiError::invalid(format!(
                "site_profile.{key} must be a string"
            )));
        }
        if key.ends_with("_path")
            && !value.as_str().is_some_and(|s| {
                s.is_empty()
                    || (s.starts_with('/')
                        && !s.starts_with("//")
                        && !s.contains("..")
                        && !s.contains('#'))
            })
        {
            return Err(ApiError::invalid(format!(
                "site_profile.{key} must be a same-origin path"
            )));
        }
        profile.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(profile))
}
fn profile_path<'a>(connection: &'a Value, key: &str, default: &'a str) -> ApiResult<&'a str> {
    let path = connection["site_profile"][key].as_str().unwrap_or(default);
    if path.is_empty()
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("..")
        || path.contains('#')
    {
        return Err(ApiError::invalid(format!(
            "site_profile.{key} is not a same-origin path"
        )));
    }
    Ok(path)
}
fn browser_cookie_header(
    state: &AppState,
    connection: &Value,
    request_url: &str,
) -> ApiResult<String> {
    let connection_id = field(connection, "connection_id");
    let status = browser_status(state, connection_id)?;
    if status["present"] != true || status["expired"] == true {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "competition.auth_required",
            "A valid browser session is required",
        ));
    }
    let record = state
        .store
        .get(BROWSER, connection_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::missing("browser session"))?;
    let storage: Value = serde_json::from_str(&read_secret(state, field(&record, "reference"))?)
        .map_err(ApiError::internal)?;
    let url = reqwest::Url::parse(request_url).map_err(ApiError::internal)?;
    let base =
        reqwest::Url::parse(field(connection, "canonical_base_url")).map_err(ApiError::internal)?;
    if url.origin() != base.origin() {
        return Err(ApiError::invalid(
            "Browser request must stay on its configured origin",
        ));
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let cookies = storage["cookies"]
        .as_array()
        .ok_or_else(|| ApiError::invalid("Invalid browser storage state"))?;
    let header = cookies
        .iter()
        .filter_map(|cookie| {
            let domain = field(cookie, "domain")
                .trim_start_matches('.')
                .to_ascii_lowercase();
            let valid_domain =
                !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")));
            let secure = cookie["secure"] == true;
            let expires = cookie["expires"].as_f64().unwrap_or(-1.0);
            let valid_expiry = expires < 0.0 || expires > super::now_secs();
            let cookie_path = cookie["path"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("/");
            let valid_path = url.path().starts_with(cookie_path);
            let name = field(cookie, "name");
            let value = cookie["value"].as_str().unwrap_or("");
            (valid_domain
                && (!secure || url.scheme() == "https")
                && valid_expiry
                && valid_path
                && !name.is_empty()
                && !name.contains([';', '\r', '\n'])
                && !value.contains([';', '\r', '\n']))
            .then(|| format!("{name}={value}"))
        })
        .collect::<Vec<_>>()
        .join("; ");
    if header.is_empty() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "competition.auth_required",
            "No matching browser cookies are available for this origin",
        ));
    }
    Ok(header)
}

#[cfg(test)]
async fn create_connection(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let cmd = command_id(&body);
    if let Some(receipt) = prior_receipt(&state, &cmd)? {
        return Ok(Json(json!({"receipt":receipt})));
    }
    let kind = field(&body, "platform_kind");
    let kind = if kind.is_empty() {
        field(&body, "platform")
    } else {
        kind
    };
    if !matches!(kind, "ctfd" | "rctf" | "gzctf" | "generic_browser")
        && !extension_adapter::supported(&state, kind)?
    {
        return Err(ApiError::unsupported(format!(
            "Platform adapter {kind:?} is unavailable"
        )));
    }
    let base = canonical_url(field(&body, "base_url"))?;
    let account = field(&body, "account_key");
    if account.is_empty() && !matches!(kind, "gzctf" | "generic_browser") {
        return Err(ApiError::invalid("account_key is required"));
    }
    let supplied_credential = field(&body, "credential");
    let supplied_reference = field(&body, "credential_ref");
    if !supplied_credential.is_empty() && !supplied_reference.is_empty() {
        return Err(ApiError::invalid(
            "Supply credential or credential_ref, not both",
        ));
    }
    let credential = if supplied_credential.is_empty() && !supplied_reference.is_empty() {
        read_secret(&state, supplied_reference)?
    } else {
        supplied_credential.to_owned()
    };
    let site_profile = if let Some(value) = body.get("site_profile") {
        Some(validate_site_profile(value)?)
    } else {
        None
    };
    if let Some(mut existing) = state
        .store
        .list(CONNECTIONS)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|v| {
            v["historical"] != true
                && v["platform_kind"] == kind
                && v["canonical_base_url"] == base
                && v["account_key"] == account
        })
    {
        let connection_id = field(&existing, "connection_id").to_owned();
        let mut changed_refs = None;
        if !credential.is_empty() {
            let old_ref = field(&existing, "credential_ref").to_owned();
            let rotation = if old_ref.is_empty() {
                1
            } else {
                state
                    .store
                    .get(SECRETS, &old_ref)
                    .map_err(ApiError::internal)?
                    .and_then(|v| v["rotation"].as_u64())
                    .unwrap_or(0)
                    + 1
            };
            let new_ref = write_secret(&state, &connection_id, "token", &credential, "", rotation)?
                ["reference"]
                .clone();
            if !old_ref.is_empty() && old_ref != new_ref.as_str().unwrap_or("") {
                remove_secret(&state, &old_ref)?;
            }
            changed_refs = Some((old_ref,new_ref.as_str().unwrap_or("").to_owned()));
            existing["credential_ref"] = new_ref;
            existing["status"] = json!("active");
            existing["last_error"] = json!("");
        }
        if let Some(profile) = site_profile {
            existing["site_profile"] = profile;
            existing["capabilities"] = json!({});
        }
        existing["archived"] = json!(false);
        existing["updated_at"] = json!(now());
        state
            .store
            .put(CONNECTIONS, &connection_id, &existing)
            .map_err(ApiError::internal)?;
        let receipt = save_receipt(
            &state,
            &cmd,
            "connection.create",
            "connection",
            &connection_id,
            json!({"connection_id":connection_id,"created":false}),
        )?;
        if let Some((old_ref,new_ref)) = changed_refs {
            notify_secret_change(&state,&old_ref,&new_ref).await;
        }
        return Ok(Json(json!({"receipt":receipt})));
    }
    let connection_id = id("pconn");
    let site_profile = site_profile.unwrap_or_else(|| json!({}));
    let reference = if !credential.is_empty() {
        write_secret(&state, &connection_id, "token", &credential, "", 1)?["reference"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    } else {
        String::new()
    };
    let record = json!({"connection_id":connection_id,"platform_kind":kind,"canonical_base_url":base,"account_key":account,
        "credential_ref":reference,"site_profile":site_profile,"capabilities":{},"status":if reference.is_empty() { "auth_required" } else { "active" },
        "last_error":"","archived":false,"created_at":now(),"updated_at":now()});
    state
        .store
        .put(CONNECTIONS, &connection_id, &record)
        .map_err(ApiError::internal)?;
    let receipt = save_receipt(
        &state,
        &cmd,
        "connection.create",
        "connection",
        &connection_id,
        json!({"connection_id":connection_id,"created":true}),
    )?;
    if !reference.is_empty() {
        notify_secret_change(&state,"",&reference).await;
    }
    Ok(Json(json!({"receipt":receipt})))
}

async fn patch_connection(
    State(state): State<Arc<AppState>>,
    Path(connection_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let cmd = command_id(&body);
    if let Some(receipt) = prior_receipt(&state, &cmd)? {
        return Ok(Json(json!({"receipt":receipt})));
    }
    let admission = &body["_planned_admission"];
    let command_id = if cmd.is_empty() { id("cmd") } else { cmd };
    loop {
        if let Some(receipt) = prior_receipt(&state, &command_id)? {
            return Ok(Json(json!({"receipt":receipt})));
        }
        if let Some(receipt) = planned_key_receipt(&state, admission)? {
            return Ok(Json(json!({"receipt":receipt})));
        }
        let previous = live_connection(&state, &connection_id)?;
        let mut connection = previous.clone();
        let mut changed = false;
        if let Some(raw) = body["base_url"].as_str() {
            connection["canonical_base_url"] = json!(canonical_url(raw)?);
            changed = true;
        }
        if let Some(account) = body["account_key"].as_str() {
            if account.trim().is_empty()
                && !matches!(field(&connection, "platform_kind"), "gzctf" | "generic_browser") {
                return Err(ApiError::invalid("account_key must not be empty"));
            }
            connection["account_key"] = json!(account.trim());
            changed = true;
        }
        if let Some(profile) = body.get("site_profile") {
            connection["site_profile"] = validate_site_profile(profile)?;
            changed = true;
        }
        if !changed {
            return Err(ApiError::invalid("No supported connection fields were provided"));
        }
        let version_key = format!("connection\0{connection_id}");
        let version = state.store.get(VERSIONS, &version_key).map_err(ApiError::internal)?;
        if body["expected_version"].as_u64().is_some_and(|expected| {
            version.as_ref().and_then(|row| row["version"].as_u64()).unwrap_or(0) != expected
        }) {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.version_conflict",
                "expected_version does not match connection stream"));
        }
        connection["capabilities"] = json!({});
        connection["updated_at"] = json!(now());
        let receipt = completed_receipt(&command_id,"connection",&connection_id,
            json!({"connection_id":connection_id}));
        let mut stored = json!({"request":{"command_type":"connection.update",
            "aggregate_id":connection_id},"response":{"receipt":receipt}});
        let claim = admission.get("claim").filter(|value| value.is_object());
        if let Some(claim) = claim {
            stored["request_fingerprint"] = claim["request_fingerprint"].clone();
            stored["status"] = json!(StatusCode::OK.as_u16());
        }
        let mut expected = vec![(CONNECTIONS,connection_id.as_str(),Some(&previous)),
            (COMMANDS,command_id.as_str(),None),(VERSIONS,version_key.as_str(),version.as_ref())];
        let mut records = vec![(CONNECTIONS,connection_id.as_str(),&connection),
            (COMMANDS,command_id.as_str(),&stored)];
        if let Some(claim) = claim {
            expected.push(("platform_command_claims",command_id.as_str(),None));
            records.push(("platform_command_claims",command_id.as_str(),claim));
            let key_id = field(admission,"key_id");
            if !key_id.is_empty() {
                expected.push(("platform_idempotency_keys",key_id,None));
                records.push(("platform_idempotency_keys",key_id,&admission["keyed"]));
            }
        }
        if state.store.put_many_if(&expected,&records).map_err(ApiError::internal)? {
            return Ok(Json(json!({"receipt":receipt})));
        }
    }
}

async fn put_connection_secret(
    State(state): State<Arc<AppState>>,
    Path(connection_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let cmd = command_id(&body);
    if let Some(receipt) = prior_receipt(&state, &cmd)? {
        return Ok(Json(json!({"receipt":receipt})));
    }
    let command_id = if cmd.is_empty() {id("cmd")} else {cmd};
    let connection = live_connection(&state, &connection_id)?;
    let credential = field(&body, "credential");
    let expires_at = field(&body, "expires_at");
    if !expires_at.is_empty() {
        chrono::DateTime::parse_from_rfc3339(expires_at)
            .map_err(|_| ApiError::invalid("expires_at must be RFC3339"))?;
    }
    let old_ref = field(&connection, "credential_ref").to_owned();
    let mode = field(&body, "mode");
    let mode = if mode.is_empty() {"rotate"} else {mode};
    if !matches!(mode, "update" | "rotate") {
        return Err(ApiError::invalid("mode must be update or rotate"));
    }
    let old_meta = if old_ref.is_empty() {
        None
    } else {
        state
            .store
            .get(SECRETS, &old_ref)
            .map_err(ApiError::internal)?
    };
    if !old_ref.is_empty() && old_meta.is_none() {
        return Err(ApiError::missing("credential"));
    }
    let rotation = old_meta.as_ref().and_then(|v|v["rotation"].as_u64())
        .unwrap_or(0)+1;
    let supplied_key = field(&body,"key");
    let base_key = if mode == "update" && old_meta.is_some() {
        field(old_meta.as_ref().unwrap(),"key")
    } else if supplied_key.is_empty() {"token"} else {supplied_key};
    let key = if old_meta.is_some() {
        format!("{}-r{}",base_key.split("-r").next().unwrap_or(base_key),
            uuid::Uuid::new_v4().simple())
    } else {base_key.to_owned()};
    let mut metadata = write_secret(
        &state,
        &connection_id,
        &key,
        credential,
        expires_at,
        rotation,
    )?;
    if let Some(old) = old_meta.as_ref() {
        metadata["created_at"] = old["created_at"].clone();
        state.store.put(SECRETS,field(&metadata,"reference"),&metadata)
            .map_err(ApiError::internal)?;
    }
    let reference = field(&metadata,"reference").to_owned();
    let cleanup = if !old_ref.is_empty() && old_ref != reference {
        let intent = json!({"command_id":command_id,"kind":"set",
            "reference":old_ref,"new_reference":reference,"created_at":now()});
        state.store.put(CREDENTIAL_CLEANUPS,&command_id,&intent)
            .map_err(ApiError::internal)?;
        true
    } else {false};
    let envelope = json!({"command_type":"connection.credential.set",
        "command_id":command_id,"idempotency_key":body["idempotency_key"],
        "aggregate_type":"connection","aggregate_id":connection_id,
        "payload":{"connection_id":connection_id,"credential_ref":reference}});
    let response = crate::operations_api::dispatch_local_competition_command(state.clone(),envelope).await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(),usize::MAX).await
        .map_err(ApiError::internal)?;
    let result: Value = serde_json::from_slice(&bytes).map_err(ApiError::internal)?;
    if !status.is_success() {
        let code = field(&result["error"],"code");
        if code != "command.outcome_unknown" && old_ref != reference {
            let _ = remove_secret(&state,&reference);
            if cleanup {let _ = state.store.delete(CREDENTIAL_CLEANUPS,&command_id);}
        }
        let error_code = match code {
            "command.idempotency_conflict" => "command.idempotency_conflict",
            "command.outcome_unknown" => "command.outcome_unknown",
            "command.version_conflict" => "command.version_conflict",
            _ => "competition.connection.credential_set_failed",
        };
        return Err(ApiError::new(status,error_code,field(&result["error"],"message")));
    }
    let completed = result["receipt"]["state"] == "completed";
    if completed {
        if !old_ref.is_empty() && old_ref != reference {
            remove_secret(&state,&old_ref)?;
        }
        notify_secret_change(&state,&old_ref,&reference).await;
        if cleanup {state.store.delete(CREDENTIAL_CLEANUPS,&command_id)
            .map_err(ApiError::internal)?;}
    } else if result["receipt"]["state"] == "failed" && old_ref != reference {
        let _ = remove_secret(&state,&reference);
        if cleanup {let _ = state.store.delete(CREDENTIAL_CLEANUPS,&command_id);}
    }
    Ok(Json(json!({"receipt":result["receipt"],
        "secret":if completed {metadata} else {Value::Null},
        "previous_reference_invalidated":if completed && old_ref != reference {
            old_ref.as_str()} else {""},"change_effect":{}})))
}

fn browser_status(state: &AppState, connection_id: &str) -> ApiResult<Value> {
    let record = state
        .store
        .get(BROWSER, connection_id)
        .map_err(ApiError::internal)?;
    Ok(browser_status_record(record.as_ref()))
}
fn browser_expiry(body:&Value)->ApiResult<String>{
    match body.get("expires_at"){
        None|Some(Value::Null)=>Ok(String::new()),Some(Value::String(text))if text.trim().is_empty()=>Ok(String::new()),
        Some(Value::String(text))=>chrono::DateTime::parse_from_rfc3339(text).map(|at|at.with_timezone(&chrono::Utc).to_rfc3339()).map_err(|_|ApiError::invalid("expires_at must include a valid RFC3339 timezone")),
        _=>Err(ApiError::invalid("expires_at must be RFC3339 text"))
    }
}

fn browser_status_record(record: Option<&Value>) -> Value {
    let Some(record) = record else {
        return json!({"present":false,"expired":false});
    };
    let expired=match record.get("expires_at"){
        None|Some(Value::Null)=>false,Some(Value::String(text))if text.is_empty()=>false,
        Some(Value::String(text))=>chrono::DateTime::parse_from_rfc3339(text).map(|at|at<=chrono::Utc::now()).unwrap_or(true),_=>true
    };
    let present = record["reference"].as_str().is_some_and(|s| !s.is_empty());
    json!({"present":present,"expired":expired,"imported_at":record["imported_at"],"expires_at":record["expires_at"],"cookies":record["cookies"],"origins":record["origins"]})
}
async fn get_browser_session(
    State(state): State<Arc<AppState>>,
    Path(connection_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let connection = live_connection(&state, &connection_id)?;
    if connection["platform_kind"] != "generic_browser" {
        return Err(ApiError::invalid(
            "Browser session requires generic_browser connection",
        ));
    }
    Ok(Json(
        json!({"connection_id":connection_id,
            "browser_session":browser_status(&state, &connection_id)?}),
    ))
}
#[cfg(test)]
async fn put_browser_session(
    State(state): State<Arc<AppState>>,
    Path(connection_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let cmd = command_id(&body);
    if let Some(receipt) = prior_receipt(&state, &cmd)? {
        return Ok(Json(json!({"receipt":receipt})));
    }
    let mut connection = live_connection(&state, &connection_id)?;
    if connection["platform_kind"] != "generic_browser" {
        return Err(ApiError::invalid(
            "Browser session requires generic_browser connection",
        ));
    }
    let expires_at = browser_expiry(&body)?;
    let previous = state
        .store
        .get(BROWSER, &connection_id)
        .map_err(ApiError::internal)?;
    let old_reference = previous.as_ref().map(|row|field(row,"reference").to_owned()).unwrap_or_default();
    let record = if body["renew"] == true {
        let mut record = previous.ok_or_else(|| ApiError::missing("browser session"))?;
        let reference = field(&record, "reference").to_owned();
        let mut metadata = state
            .store
            .get(SECRETS, &reference)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::missing("browser session secret"))?;
        metadata["expires_at"] = json!(expires_at);
        metadata["updated_at"] = json!(now());
        state
            .store
            .put(SECRETS, &reference, &metadata)
            .map_err(ApiError::internal)?;
        record["expires_at"] = json!(expires_at);
        record
    } else {
        let mut storage = body
            .get("storage_state")
            .cloned()
            .ok_or_else(|| ApiError::invalid("storage_state is required"))?;
        if storage.is_object() {
            if storage.get("cookies").is_none() { storage["cookies"] = json!([]); }
            if storage.get("origins").is_none() { storage["origins"] = json!([]); }
        }
        if !storage.is_object() || !storage["cookies"].is_array() || !storage["origins"].is_array()
        {
            return Err(ApiError::invalid(
                "storage_state requires cookies and origins arrays",
            ));
        }
        let serialized = serde_json::to_string(&storage).map_err(ApiError::internal)?;
        let reference = write_secret(
            &state,
            &connection_id,
            "browser-state",
            &serialized,
            &expires_at,
            1,
        )?["reference"]
            .clone();
        json!({"reference":reference,"imported_at":now(),"expires_at":expires_at,"cookies":storage["cookies"].as_array().map_or(0,Vec::len),"origins":storage["origins"].as_array().map_or(0,Vec::len)})
    };
    state
        .store
        .put(BROWSER, &connection_id, &record)
        .map_err(ApiError::internal)?;
    connection["status"] = json!("active");
    connection["updated_at"] = json!(now());
    state
        .store
        .put(CONNECTIONS, &connection_id, &connection)
        .map_err(ApiError::internal)?;
    let receipt = save_receipt(
        &state,
        &cmd,
        if body["renew"] == true {
            "connection.browser_session.renew"
        } else {
            "connection.browser_session.import"
        },
        "connection",
        &connection_id,
        json!({"connection_id":connection_id,"browser_session":browser_status(&state, &connection_id)?}),
    )?;
    notify_secret_change(&state,&old_reference,field(&record,"reference")).await;
    Ok(Json(json!({"receipt":receipt})))
}
fn http_client()->ApiResult<reqwest::Client>{
    crate::network_proxy::http_client_builder().timeout(Duration::from_secs(20)).redirect(reqwest::redirect::Policy::none()).build().map_err(ApiError::internal)
}
async fn gzctf_session_cookie(state:&AppState,connection:&Value)->ApiResult<String>{
    let key=platform_http::identity(state,connection)?;let slot=platform_http::session(&key);let mut cache=slot.lock().await;
    if let Some(cookie)=cache.as_ref(){return Ok(cookie.clone());}
    let cookie=gzctf_login_cookie(state,connection).await?;*cache=Some(cookie.clone());Ok(cookie)
}
async fn gzctf_login_cookie(state: &AppState, connection: &Value) -> ApiResult<String> {
    let reference = field(connection, "credential_ref");
    if reference.is_empty() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "competition.auth_required",
            "GZCTF credential is required",
        ));
    }
    let account = field(connection, "account_key");
    if account.is_empty() {
        let cookie = read_secret(state, reference)?;
        if cookie.contains(['\r', '\n', ';']) {
            return Err(ApiError::invalid("Invalid session cookie characters"));
        }
        return Ok(cookie);
    }
    let password = read_secret(state, reference)?;
    let url = format!(
        "{}/api/account/login",
        field(connection, "canonical_base_url").trim_end_matches('/')
    );
    let response = http_client()?
        .post(url)
        .json(&json!({"userName":account,"password":password}))
        .send()
        .await
        .map_err(|error|platform_http::transport(&error))?;
    if response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::FORBIDDEN {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "competition.auth_required",
            "GZCTF login was rejected",
        ));
    }
    if !response.status().is_success() {
        let code = if response.status() == StatusCode::TOO_MANY_REQUESTS {
            "competition.platform_rate_limited"
        } else if response.status().is_server_error() {
            "competition.platform_transient"
        } else { "competition.platform_error" };
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            code,
            format!("GZCTF login returned HTTP {}", response.status().as_u16()),
        ));
    }
    let token = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .filter_map(|v| v.trim().strip_prefix("GZCTF_Token="))
        .find(|v| !v.is_empty() && !v.contains(['\r', '\n', ';']))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "competition.platform_response",
                "GZCTF login did not establish a session",
            )
        })?;
    Ok(token.to_owned())
}
async fn platform_request_header(
    state: &AppState,
    connection: &Value,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
    extra_header: Option<(&str, &str)>,
) -> ApiResult<(StatusCode, Value, Option<f64>)> {
    if !path.starts_with('/') || path.starts_with("//") || path.contains("..") || path.contains('#')
    {
        return Err(ApiError::invalid(
            "Platform request path must stay on the configured origin",
        ));
    }
    let base = field(connection, "canonical_base_url");
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let retry_body = body.clone();
    let mutating=!matches!(method,reqwest::Method::GET|reqwest::Method::HEAD|reqwest::Method::OPTIONS);
    let cache_key=if method==reqwest::Method::GET && connection["platform_kind"]=="gzctf"{Some(format!("{}:{url}",platform_http::identity(state,connection)?))}else{None};
    let cached=cache_key.as_deref().and_then(platform_http::response);
    let mut request = http_client()?.request(method.clone(), &url);
    if let Some(cached)=cached.as_ref().filter(|cached|!cached.etag.is_empty()){request=request.header(reqwest::header::IF_NONE_MATCH,&cached.etag);}
    match field(connection, "platform_kind") {
        "ctfd" | "rctf" => {
            let reference = field(connection, "credential_ref");
            if reference.is_empty() {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "competition.auth_required",
                    "Platform credential is required",
                ));
            }
            let token = read_secret(state, reference)?;
            if token.contains(['\r', '\n']) {
                return Err(ApiError::invalid("Invalid credential characters"));
            }
            let scheme = if field(connection, "platform_kind") == "ctfd" {
                "Token"
            } else {
                "Bearer"
            };
            request = request.header(reqwest::header::AUTHORIZATION, format!("{scheme} {token}"));
            if field(connection, "platform_kind") == "ctfd" {
                request = request.header(reqwest::header::CONTENT_TYPE, "application/json");
            }
        }
        "gzctf" => {
            let token = gzctf_session_cookie(state, connection).await?;
            request = request.header(reqwest::header::COOKIE, format!("GZCTF_Token={token}"));
        }
        "generic_browser" => {
            request = request.header(
                reqwest::header::COOKIE,
                browser_cookie_header(state, connection, &url)?,
            );
        }
        _ => {
            return Err(ApiError::unsupported(
                "HTTP adapter is unavailable for this platform",
            ));
        }
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    if let Some((name, value)) = extra_header {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| ApiError::invalid("Invalid browser CSRF header name"))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| ApiError::invalid("Invalid browser CSRF header value"))?;
        request = request.header(name, value);
    }
    let mut response=request.send().await.map_err(|error|platform_http::request_transport(&error,mutating))?;
    if response.status() == StatusCode::UNAUTHORIZED
        && field(connection, "platform_kind") == "gzctf"
        && !field(connection, "account_key").is_empty()
    {
        platform_http::invalidate_session(state,connection).await?;
        let token = gzctf_session_cookie(state, connection).await?;
        let mut retry = http_client()?
            .request(method, &url)
            .header(reqwest::header::COOKIE, format!("GZCTF_Token={token}"));
        if let Some(cached)=cached.as_ref().filter(|cached|!cached.etag.is_empty()){retry=retry.header(reqwest::header::IF_NONE_MATCH,&cached.etag);}
        if let Some(body) = retry_body {
            retry = retry.json(&body);
        }
        if let Some((name, value)) = extra_header {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ApiError::invalid("Invalid browser CSRF header name"))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| ApiError::invalid("Invalid browser CSRF header value"))?;
            retry = retry.header(name, value);
        }
        response=retry.send().await.map_err(|error|platform_http::request_transport(&error,mutating))?;
    }
    let retry_after_seconds = response.headers().get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after_seconds);
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if status==StatusCode::NOT_MODIFIED{return cached.map(|cached|(StatusCode::OK,cached.body,retry_after_seconds)).ok_or_else(||ApiError::new(StatusCode::BAD_GATEWAY,"competition.platform_response","Platform returned 304 without a matching cached response"));}
    let etag=response.headers().get(reqwest::header::ETAG).and_then(|value|value.to_str().ok()).map(str::to_owned);
    let mut bytes=Vec::new();while let Some(chunk)=response.chunk().await.map_err(|error|platform_http::request_transport(&error,mutating))?{if bytes.len().saturating_add(chunk.len())>8*1024*1024{return Err(ApiError::new(StatusCode::BAD_GATEWAY,"competition.platform_response","Platform response is too large"));}bytes.extend_from_slice(&chunk);}
    let value=if bytes.is_empty(){Value::Null}else{match serde_json::from_slice::<Value>(&bytes){Ok(value)=>value,Err(_)if !status.is_success()=>Value::Null,Err(_)=>return Err(ApiError::new(StatusCode::BAD_GATEWAY,"competition.platform_response","Platform returned a non-JSON response"))}};
    if status.is_success(){if let Some(key)=cache_key{platform_http::save(key,etag.unwrap_or_default(),value.clone());}}

    Ok((status, value, retry_after_seconds))
}
fn parse_retry_after_seconds(value: &str) -> Option<f64> {
    if let Ok(seconds) = value.trim().parse::<f64>() {
        return seconds.is_finite().then_some(seconds.max(0.0));
    }
    chrono::DateTime::parse_from_rfc2822(value.trim()).ok().map(|date| {
        (date.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_milliseconds().max(0) as f64 / 1000.0
    })
}
async fn platform_request(
    state: &AppState,
    connection: &Value,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> ApiResult<(StatusCode, Value)> {
    platform_request_header(state, connection, method, path, body, None)
        .await.map(|(status, value, _)| (status, value))
}
async fn browser_csrf_token(
    state: &AppState,
    connection: &Value,
) -> ApiResult<Option<(String, String)>> {
    let profile = &connection["site_profile"];
    let csrf_path = profile["csrf_path"].as_str().unwrap_or("");
    let pattern = profile["csrf_pattern"].as_str().unwrap_or("");
    if csrf_path.is_empty() && pattern.is_empty() {
        return Ok(None);
    }
    if csrf_path.is_empty() || pattern.is_empty() {
        return Err(ApiError::invalid(
            "site_profile requires both csrf_path and csrf_pattern",
        ));
    }
    let path = profile_path(connection, "csrf_path", "")?;
    let url = format!(
        "{}{}",
        field(connection, "canonical_base_url").trim_end_matches('/'),
        path
    );
    let response = http_client()?
        .get(&url)
        .header(
            reqwest::header::COOKIE,
            browser_cookie_header(state, connection, &url)?,
        )
        .send()
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "competition.platform_transport",
                "Browser CSRF page could not be loaded",
            )
        })?;
    if !response.status().is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "competition.platform_error",
            format!(
                "Browser CSRF page returned HTTP {}",
                response.status().as_u16()
            ),
        ));
    }
    let text = response.text().await.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "competition.platform_response",
            "Browser CSRF page could not be read",
        )
    })?;
    if text.len() > 1024 * 1024 {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "competition.platform_response",
            "Browser CSRF page is too large",
        ));
    }
    let expression = regex::Regex::new(pattern)
        .map_err(|_| ApiError::invalid("Invalid browser CSRF pattern"))?;
    let token = expression
        .captures(&text)
        .and_then(|m| m.get(1))
        .map(|m| m.as_str().to_owned())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "competition.browser_shape_changed",
                "Browser CSRF token was not found",
            )
        })?;
    let header = profile["csrf_header"]
        .as_str()
        .unwrap_or("CSRF-Token")
        .to_owned();
    Ok(Some((header, token)))
}
fn checked_platform_response(status:StatusCode,body:Value,kind:&str)->ApiResult<Value>{
    if kind=="rctf"{
        let tag=field(&body,"kind");if !tag.is_empty() && !tag.starts_with("good"){
            let(code,status)=match tag{"badToken"=>("competition.auth_required",StatusCode::UNAUTHORIZED),"badPerms"=>("competition.platform_permission",StatusCode::FORBIDDEN),"badChallenge"|"badNotFound"=>("competition.platform_not_found",StatusCode::NOT_FOUND),"badRateLimit"=>("competition.platform_rate_limited",StatusCode::TOO_MANY_REQUESTS),"badNotStarted"|"badEnded"=>("competition.platform_gate",StatusCode::CONFLICT),_=>("competition.platform_error",StatusCode::BAD_GATEWAY)};
            return Err(ApiError::new(status,code,format!("rCTF returned {tag}")));
        }
    }
    if !status.is_success(){
        let code=match status{StatusCode::UNAUTHORIZED=>"competition.auth_required",StatusCode::FORBIDDEN if kind=="generic_browser"=>"competition.auth_required",StatusCode::FORBIDDEN=>"competition.platform_permission",StatusCode::NOT_FOUND=>"competition.platform_not_found",StatusCode::TOO_MANY_REQUESTS=>"competition.platform_rate_limited",_ if kind=="gzctf" && field(&body,"title")=="SubmissionLimitExceeded"=>"competition.platform_permission",_ if status.is_server_error()=>"competition.platform_transient",_=>"competition.platform_error"};
        return Err(ApiError::new(status,code,format!("Platform HTTP {}{}",status.as_u16(),if kind=="gzctf"{format!(": {}",field(&body,"title"))}else{String::new()})));
    }
    match kind{
        "ctfd"=>{if body["success"]!=true{return Err(ApiError::new(StatusCode::BAD_GATEWAY,"competition.platform_response","CTFd reported an invalid or unsuccessful envelope"));}Ok(body["data"].clone())},
        "rctf"=>{if !field(&body,"kind").starts_with("good"){return Err(ApiError::new(StatusCode::BAD_GATEWAY,"competition.platform_response","rCTF response envelope is missing its success kind"));}Ok(body["data"].clone())},_=>Ok(body)
    }
}

async fn platform_get(state: &AppState, connection: &Value, path: &str) -> ApiResult<Value> {
    let (status, body) =
        platform_request(state, connection, reqwest::Method::GET, path, None).await?;
    checked_platform_response(status, body, field(connection, "platform_kind"))
}
#[cfg(test)]
async fn probe_connection(
    State(state): State<Arc<AppState>>,
    Path(connection_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let cmd = command_id(&body);
    if let Some(receipt) = prior_receipt(&state, &cmd)? {
        return Ok(Json(json!({"receipt":receipt})));
    }
    let capabilities = probe_connection_effect(&state, &connection_id).await?;
    let receipt = save_receipt(
        &state,
        &cmd,
        "connection.test",
        "connection",
        &connection_id,
        json!({"connection_id":connection_id,"capabilities":capabilities}),
    )?;
    Ok(Json(json!({"receipt":receipt})))
}

async fn probe_connection_effect(state: &Arc<AppState>, connection_id: &str) -> ApiResult<Value> {
    let mut connection = live_connection(&state, &connection_id)?;
    let kind = field(&connection, "platform_kind").to_owned();
    let capabilities = match kind.as_str() {
        "ctfd" => {
            let list = platform_get(&state, &connection, "/api/v1/challenges").await?;
            if !list.is_array() {
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "competition.platform_response",
                    "CTFd challenge list is invalid",
                ));
            }
            let scope=match platform_get(&state,&connection,"/api/v1/configs").await{Ok(_)=>"admin",Err(error)if error.code=="competition.platform_permission"=>"user",Err(error)=>return Err(error)};
            json!({"platform_kind":kind,"auth_methods":["token_header"],"auth_detail":{"token_scope":scope},"sync_modes":["full"],"submit":true,"attachments":true,"hints":true,"prerequisites":true,"dynamic_instances":false,"evidence":{"visible_challenges":list.as_array().map_or(0,Vec::len)}})
        }
        "rctf" => {
            platform_get(&state, &connection, "/api/v1/users/me").await?;
            let (version, list) = match platform_get(&state, &connection, "/api/v2/challs").await {
                Ok(v) => ("v2", v),
                Err(error) if error.code=="competition.platform_not_found" => (
                    "v1",
                    platform_get(&state, &connection, "/api/v1/challs").await?,
                ),Err(error)=>return Err(error),
            };
            if !list.is_array() {
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "competition.platform_response",
                    "rCTF challenge list is invalid",
                ));
            }
            json!({"platform_kind":kind,"auth_methods":["bearer"],"sync_modes":["full"],"submit":true,"attachments":true,"dynamic_instances":version=="v2","evidence":{"api_version":version,"visible_challenges":list.as_array().map_or(0,Vec::len)}})
        }
        "gzctf" => {
            let schema = platform_get(&state, &connection, "/openapi/v1.json").await?;
            let paths = schema["paths"].as_object().ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "competition.platform_response",
                    "GZCTF OpenAPI schema is invalid",
                )
            })?;
            let required=["/api/game","/api/game/{id}","/api/game/{id}/details","/api/game/{id}/challenges/{challengeid}","/api/game/{id}/challenges/{challengeid}/status/{submitid}","/api/game/{id}/container/{challengeid}","/api/game/{id}/container/{challengeid}/extend","/api/account/login"];
            let missing=required.iter().filter(|required|!paths.keys().any(|path|path.eq_ignore_ascii_case(required))).collect::<Vec<_>>();
            let digest=platform_http::schema_hash(&schema);let previous=field(&connection["capabilities"]["evidence"],"openapi_schema_hash");
            json!({"platform_kind":kind,"auth_methods":["cookie_session"],"sync_modes":["full","etag"],"submit":true,"submit_async":true,"attachments":true,"dynamic_instances":paths.keys().any(|path|path.eq_ignore_ascii_case("/api/game/{id}/container/{challengeid}")),"instance_renewable":true,"instance_stoppable":true,"evidence":{"openapi_paths":paths.len(),"platform_version":schema["info"]["version"],"openapi_schema_hash":digest,"previous_schema_hash":previous,"schema_changed":!previous.is_empty() && previous!=digest,"openapi_required_missing":missing}})

        }
        "generic_browser" => {
            let status = browser_status(&state, &connection_id)?;
            if status["expired"] == true {
                let _ = purge_expired_browser_session(&state, &connection_id);
            }
            if status["present"] != true || status["expired"] == true {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "competition.auth_required",
                    "A valid browser session is required",
                ));
            }
            let probe_path = profile_path(&connection, "probe_path", "/")?;
            platform_get(&state, &connection, probe_path).await?;
            let challenges_path = connection["site_profile"]["challenges_path"]
                .as_str()
                .unwrap_or("");
            let visible = if challenges_path.is_empty() {
                None
            } else {
                let path = profile_path(&connection, "challenges_path", "")?;
                let data = platform_get(&state, &connection, path).await?;
                Some(extract_profile_items(&connection, &data)?.len())
            };
            json!({"platform_kind":kind,"auth_methods":["browser_session"],"sync_modes":if visible.is_some() { json!(["full"]) } else { json!([]) },
                "submit":connection["site_profile"]["submit_path"].as_str().is_some_and(|s|!s.is_empty()),"dynamic_instances":false,
                "browser_session":status,"evidence":{"visible_challenges":visible,"site_profile":connection["site_profile"]}})
        }
        _ => extension_adapter::probe(&state, &connection).await?,
    };
    connection["capabilities"] = capabilities.clone();
    connection["status"] = json!("active");
    connection["last_error"] = json!("");
    connection["updated_at"] = json!(now());
    state
        .store
        .put(CONNECTIONS, &connection_id, &connection)
        .map_err(ApiError::internal)?;
    Ok(capabilities)
}

fn scoped(state: &AppState, collection: &str, competition_id: &str) -> ApiResult<Vec<Value>> {
    if collection == EVENTS {
        return state.store.list_competition_events(competition_id).map_err(ApiError::internal);
    }
    Ok(state
        .store
        .list(collection)
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|v| v["competition_id"] == competition_id)
        .collect())
}
fn ordered_scoped(state: &AppState, collection: &str, competition_id: &str) -> ApiResult<Vec<Value>> {
    let mut rows = state.store.list_inserted(collection).map_err(ApiError::internal)?
        .into_iter().filter(|row| row["competition_id"] == competition_id).collect::<Vec<_>>();
    rows.sort_by(|a,b| field(a,"created_at").cmp(field(b,"created_at")));
    Ok(rows)
}
fn pending_competition_receipts(state: &AppState) -> ApiResult<usize> {
    Ok(state.store.list(COMMANDS).map_err(ApiError::internal)?.iter().filter(|row| {
        gateway::supports_command(field(&row["request"],"command_type"))
            && matches!(field(&row["response"]["receipt"],"state"), "accepted" | "running" | "waiting")
    }).count())
}
fn pending_competition_outbox(state: &AppState) -> ApiResult<usize> {
    const COLLECTIONS: [&str; 4] = [
        "competition_connection_outbox_live", "competition_binding_outbox_live",
        "competition_lease_outbox_live", "competition_submission_outbox_live",
    ];
    let pending = |row: &Value| matches!(field(row,"status"), "pending" | "processing" | "failed" | "paused");
    let mut count = 0;
    for collection in COLLECTIONS {
        count += state.store.list(collection).map_err(ApiError::internal)?.iter().filter(|row| pending(row)).count();
    }
    let effects = state.store.list("platform_effect_receipts").map_err(ApiError::internal)?;
    let domains: HashMap<&str,&str> = effects.iter().filter_map(|effect| {
        Some((effect["effect_id"].as_str()?, effect["domain"].as_str()?))
    }).collect();
    count += state.store.list("platform_outbox_live").map_err(ApiError::internal)?.iter().filter(|row| {
        let destination = field(row,"destination");
        let domain = row["effect_id"].as_str().and_then(|id| domains.get(id).copied())
            .unwrap_or(if matches!(destination,"competition.live" | "run.gateway") {"competition"} else {"platform"});
        domain == "competition" && pending(row)
    }).count();
    Ok(count)
}
fn event_bus(competition_id: &str) -> broadcast::Sender<Value> {
    static BUSES: OnceLock<Mutex<HashMap<String, broadcast::Sender<Value>>>> = OnceLock::new();
    let buses = BUSES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut buses = buses.lock().unwrap();
    buses
        .entry(competition_id.to_owned())
        .or_insert_with(|| broadcast::channel(1024).0)
        .clone()
}
pub(crate) fn event_stream_version(
    state: &AppState,
    aggregate_type: &str,
    aggregate_id: &str,
) -> Result<u64, String> {
    state.store.list(EVENTS).map_err(|error| error.to_string()).map(|events| {
        events.into_iter().filter(|event| {
            event["aggregate_type"] == aggregate_type && event["aggregate_id"] == aggregate_id
        }).count() as u64
    })
}
fn append_event(
    state: &AppState,
    competition_id: &str,
    aggregate_type: &str,
    aggregate_id: &str,
    event_type: &str,
    command_id: &str,
    payload: Value,
) -> ApiResult<Value> {
    let event = json!({"event_type":event_type,"occurred_at":now(),"competition_id":competition_id,
        "aggregate_type":aggregate_type,"aggregate_id":aggregate_id,"payload":payload,"command_id":command_id});
    let event = crate::operations_api::persist_competition_event(
        state, aggregate_type, aggregate_id, EVENTS, competition_id, event,
    ).map_err(ApiError::internal)?;
    let _ = event_bus(competition_id).send(event.clone());
    Ok(event)
}
fn put_records_with_events_returning(
    state: &AppState,
    competition_id: &str,
    expected: &[(&str, &str, Option<&Value>)],
    records: &[(&str, &str, &Value)],
    events: &[Value],
) -> ApiResult<Option<Vec<Value>>> {
    let floors = events
        .iter()
        .map(|event| {
            let aggregate_type = field(event, "aggregate_type");
            let aggregate_id = field(event, "aggregate_id");
            crate::operations_api::current_aggregate_version(state, aggregate_type, aggregate_id)
                .map(|version| (event.clone(), version))
                .map_err(ApiError::internal)
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let committed = state
        .store
        .put_many_if_with_competition_events(
            expected,
            records,
            "platform_aggregate_versions",
            EVENTS,
            competition_id,
            &floors,
        )
        .map_err(ApiError::internal)?;
    let Some(committed) = committed else {
        return Ok(None);
    };
    for event in &committed {
        let _ = event_bus(competition_id).send(event.clone());
    }
    Ok(Some(committed))
}
fn put_records_with_events(
    state: &AppState,
    competition_id: &str,
    expected: &[(&str, &str, Option<&Value>)],
    records: &[(&str, &str, &Value)],
    events: &[Value],
) -> ApiResult<bool> {
    Ok(put_records_with_events_returning(state, competition_id, expected, records, events)?.is_some())
}
pub(crate) fn live_connections(state: &AppState) -> Result<Vec<Value>, String> {
    state
        .store
        .list(CONNECTIONS)
        .map_err(|e| e.to_string())
        .map(|rows| {
            rows.into_iter()
                .filter(|v| v["historical"] != true && v["archived"] != true)
                .map(public_connection)
                .collect()
        })
}
pub(crate) fn live_platform_secrets(state: &AppState) -> Result<Vec<Value>, String> {
    let references: std::collections::HashSet<String> = state
        .store
        .list(CONNECTIONS)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|v| {
            v["credential_ref"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .collect();
    Ok(state
        .store
        .list(SECRETS)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|v| {
            v["reference"]
                .as_str()
                .is_some_and(|s| references.contains(s))
        })
        .collect())
}
pub(crate) fn live_metrics(state: &AppState) -> Result<Value, String> {
    let events = state.store.list(EVENTS).map_err(|e| e.to_string())?;
    let submissions = state.store.list(SUBMISSIONS).map_err(|e| e.to_string())?;
    let commands = state.store.list(COMMANDS).map_err(|e| e.to_string())?;
    let outbox = state.store.list("competition_lease_outbox_live").map_err(|e| e.to_string())?;
    let connections = state.store.list(CONNECTIONS).map_err(|e| e.to_string())?;
    let competitions = state.store.list(COMPETITIONS).map_err(|e| e.to_string())?;
    let browsers = state.store.list(BROWSER).map_err(|e| e.to_string())?;
    let mut by_state = serde_json::Map::new();
    for submission in submissions {
        let key = field(&submission, "state").to_owned();
        let count = by_state.get(&key).and_then(Value::as_u64).unwrap_or(0) + 1;
        by_state.insert(key, json!(count));
    }
    let pending = commands
        .iter()
        .filter(|v| {
            matches!(
                v["response"]["receipt"]["state"].as_str(),
                Some("accepted" | "running" | "waiting")
            )
        })
        .count();
    let mut outbox_states = serde_json::Map::new();
    for row in &outbox {
        let key = field(row, "status").to_owned();
        let count = outbox_states.get(&key).and_then(Value::as_u64).unwrap_or(0) + 1;
        outbox_states.insert(key, json!(count));
    }
    let pending_outbox = outbox.iter().filter(|row| {
        matches!(field(row, "status"), "pending" | "processing" | "failed" | "paused")
    }).count();
    let oldest_pending_seconds = outbox.iter()
        .filter(|row| field(row, "status") != "delivered")
        .filter_map(|row| chrono::DateTime::parse_from_rfc3339(field(row, "created_at")).ok())
        .map(|created| (chrono::Utc::now() - created.with_timezone(&chrono::Utc)).num_milliseconds().max(0) as f64 / 1000.0)
        .fold(0.0_f64, f64::max);
    Ok(
        json!({"event_watermark":events.iter().filter_map(|v|v["global_seq"].as_u64()).max().unwrap_or(0),
        "event_count":events.len(),"pending_receipts":pending,"pending_outbox":pending_outbox,
        "outbox":{"by_state":outbox_states,"oldest_pending_seconds":(oldest_pending_seconds * 1000.0).round() / 1000.0},
        "submissions":by_state,
        "sync_cursors":state.store.list(SYNC_CURSORS).map_err(|e|e.to_string())?.len(),
        "sync_errors":competitions.iter().filter(|v|!field(v,"last_sync_error").is_empty()).count(),
        "connections":connections.iter().filter(|v|v["historical"] != true && v["archived"] != true).count(),
        "competitions":competitions.iter().filter(|v|v["historical"] != true && v["archived"] != true).count(),
        "browser_sessions":browsers.len()}),
    )
}
pub(crate) fn expired_browser_sessions(state: &AppState) -> Result<Vec<Value>, String> {
    let records = state.store.list(BROWSER).map_err(|e| e.to_string())?;
    Ok(records.into_iter().filter_map(|record| {
        let expires_at = record["expires_at"].as_str()?;
        let expiry = chrono::DateTime::parse_from_rfc3339(expires_at).ok()?;
        if expiry >= chrono::Utc::now() { return None; }
        let reference = record["reference"].as_str()?;
        let (connection_id,_) = parse_secret_ref(reference).ok()?;
        Some(json!({"candidate_id":format!("browser-session:{connection_id}"),"kind":"expired_browser_session","target":connection_id,"expired_at":expires_at}))
    }).collect())
}
pub(crate) fn purge_expired_browser_session(
    state: &AppState,
    connection_id: &str,
) -> Result<bool, String> {
    let status = browser_status(state, connection_id).map_err(|e| e.message)?;
    if status["present"] != true || status["expired"] != true {
        return Ok(false);
    }
    let record = state
        .store
        .get(BROWSER, connection_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Browser session disappeared".to_owned())?;
    remove_secret(state, field(&record, "reference")).map_err(|e| e.message)?;
    state
        .store
        .delete(BROWSER, connection_id)
        .map_err(|e| e.to_string())?;
    if let Some(mut connection) = state
        .store
        .get(CONNECTIONS, connection_id)
        .map_err(|e| e.to_string())?
    {
        connection["status"] = json!("auth_required");
        connection["updated_at"] = json!(now());
        state
            .store
            .put(CONNECTIONS, connection_id, &connection)
            .map_err(|e| e.to_string())?;
    }
    Ok(true)
}
pub(crate) fn live_snapshot(
    state: &AppState,
    competition_id: &str,
) -> Result<Option<Value>, String> {
    let Some(competition) = state.store.get(COMPETITIONS, competition_id).map_err(|e|e.to_string())? else {
        return Ok(None);
    };
    let connection = field(&competition, "connection_id");
    let connection = state.store.get(CONNECTIONS, connection).map_err(|e|e.to_string())?
        .map(query_connection_view);
    let policy = state
        .store
        .get(POLICIES, competition_id)
        .map_err(|e| e.to_string())?;
    let challenges = ordered_scoped(state, CHALLENGES, competition_id).map_err(|e| e.message)?;
    let revisions = ordered_scoped(state, REVISIONS, competition_id).map_err(|e| e.message)?;
    let queue = ordered_scoped(state, QUEUE, competition_id).map_err(|e| e.message)?;
    let bindings = ordered_scoped(state, BINDINGS, competition_id).map_err(|e| e.message)?;
    let leases = ordered_scoped(state, LEASES, competition_id).map_err(|e| e.message)?;
    let candidates = ordered_scoped(state, CANDIDATES, competition_id).map_err(|e| e.message)?;
    let submissions = ordered_scoped(state, SUBMISSIONS, competition_id).map_err(|e| e.message)?;
    let budgets = ordered_scoped(state, BUDGETS, competition_id).map_err(|e| e.message)?;
    let event_watermark = state.store.list(EVENTS).map_err(|e|e.to_string())?
        .iter().filter_map(|v| v["global_seq"].as_u64())
        .max()
        .unwrap_or(0);
    let pending_receipts = pending_competition_receipts(state).map_err(|e|e.message)?;
    let pending_outbox = pending_competition_outbox(state).map_err(|e|e.message)?;
    let schema_version_db = projections::schema_version(state).map_err(|e|e.message)?;
    let projection_watermarks = projections::watermarks(state).map_err(|e|e.message)?;
    let challenge_views: Vec<Value> = challenges.into_iter().map(|challenge| {
        let cid = field(&challenge, "challenge_id");
        let revision = revisions.iter().find(|v| v["revision_id"] == challenge["current_revision_id"]);
        let binding = bindings.iter().find(|v| v["competition_challenge_id"] == cid
            && matches!(field(v,"state"),"planned" | "creating" | "starting" | "running" | "active"
                | "local_finished" | "remote_pending" | "rejected" | "resolving" | "paused"));
        let lease = leases.iter().find(|v|v["competition_challenge_id"] == cid
            && matches!(field(v,"state"),"requested" | "provisioning" | "active" | "renewing" | "releasing"));
        let entry = queue.iter().find(|v| v["competition_challenge_id"] == cid);
        json!({"challenge":challenge,"current_revision":revision,"active_binding":binding,"run_id":binding.map(|v|v["run_id"].clone()),
            "active_lease":lease.cloned().map(query_lease_view),"queue_entry":entry,"candidates":candidates.iter().filter(|v|v["competition_challenge_id"] == cid).cloned().map(safe_candidate).collect::<Vec<_>>(),
            "submissions":submissions.iter().filter(|v|v["competition_challenge_id"] == cid).cloned().map(safe_submission).collect::<Vec<_>>()})
    }).collect();
    Ok(Some(
        json!({"generated_at":now(),"schema_version_db":schema_version_db,"connection":connection,"competition":competition,"policy":policy,
        "challenges":challenge_views,"queue":queue,"budgets":budgets,"event_watermark":event_watermark,"projection_watermarks":projection_watermarks,"pending_receipts":pending_receipts,"pending_outbox":pending_outbox}),
    ))
}
pub(crate) fn live_history(
    state: &AppState,
    competition_id: &str,
    before: u64,
    limit: usize,
) -> Result<Option<Value>, String> {
    if live_snapshot(state, competition_id)?.is_none() {
        return Ok(None);
    }
    let mut events = scoped(state, EVENTS, competition_id).map_err(|e| e.message)?;
    events.retain(|v| v["seq"].as_u64().is_some_and(|n| n < before));
    events.sort_by_key(|v| std::cmp::Reverse(v["seq"].as_u64().unwrap_or(0)));
    let more = events.len() > limit;
    events.truncate(limit);
    let next_before = if more {
        events.last().map(|v| v["seq"].clone())
    } else {
        None
    };
    Ok(Some(json!({"events":events,"next_before":next_before})))
}
pub(crate) fn live_challenges(
    state: &AppState,
    competition_id: &str,
) -> Result<Option<Value>, String> {
    Ok(live_snapshot(state, competition_id)?.map(|v| v["challenges"].clone()))
}
pub(crate) fn live_submissions(
    state: &AppState,
    competition_id: &str,
) -> Result<Option<Value>, String> {
    if live_snapshot(state, competition_id)?.is_none() {
        return Ok(None);
    }
    Ok(Some(
        json!({"competition_id":competition_id,"submissions":scoped(state, SUBMISSIONS, competition_id).map_err(|e| e.message)?.into_iter().map(safe_submission).collect::<Vec<_>>(),
        "candidates":scoped(state, CANDIDATES, competition_id).map_err(|e| e.message)?.into_iter().map(safe_candidate).collect::<Vec<_>>()}),
    ))
}
pub(crate) fn live_leases(state: &AppState, competition_id: &str) -> Result<Option<Value>, String> {
    if live_snapshot(state, competition_id)?.is_none() {
        return Ok(None);
    }
    Ok(Some(json!(
        scoped(state, LEASES, competition_id).map_err(|e| e.message)?
    )))
}
fn snapshot_content_fingerprint(snapshot: &Value) -> Value {
    let mut copy = snapshot.clone();
    if let Some(obj) = copy.as_object_mut() {
        obj.remove("generated_at");
    }
    copy
}
fn validate_sse_cursor(
    after: Option<&str>,
    last_event_id: Option<&str>,
) -> ApiResult<u64> {
    let candidate = last_event_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| after.map(str::trim).filter(|s| !s.is_empty()));
    let Some(raw) = candidate else {
        return Ok(0);
    };
    let parsed = raw.parse::<i64>().map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.events.cursor_invalid",
            "after / Last-Event-ID must be an integer seq",
        )
    })?;
    Ok(parsed.max(0) as u64)
}
pub(crate) async fn live_events_response_with_cursor(
    state: Arc<AppState>,
    competition_id: String,
    after: Option<&str>,
    last_event_id: Option<&str>,
) -> Option<Response> {
    let snapshot = live_snapshot(&state, &competition_id).ok()??;
    if let Err(error) = validate_sse_cursor(after, last_event_id) {
        return Some(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "schema_version": 1,
                        "code": error.code,
                        "message": error.message,
                        "category": "validation",
                        "recovery_hint": "使用上一条已确认事件的整数 seq 重新连接",
                        "detail": {}
                    },
                    "detail": error.message
                })),
            )
                .into_response(),
        );
    }
    Some(build_coalesced_sse_response(state, competition_id, snapshot))
}
fn build_coalesced_sse_response(
    state: Arc<AppState>,
    competition_id: String,
    snapshot: Value,
) -> Response {
    let initial_content = snapshot_content_fingerprint(&snapshot);
    let initial = Event::default()
        .event("snapshot")
        .id(snapshot["event_watermark"].to_string())
        .data(json!({"watermark_seq":snapshot["event_watermark"],"snapshot":snapshot}).to_string());
    let coalesced = futures_util::stream::unfold(
        (
            state,
            competition_id,
            initial_content,
        ),
        |(state, cid, mut last_content)| async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(latest) = live_snapshot(&state, &cid).ok().flatten() else {
                    return None;
                };
                let content = snapshot_content_fingerprint(&latest);
                if content == last_content {
                    continue;
                }
                last_content = content;
                let seq = latest["event_watermark"].to_string();
                let event = Event::default().event("snapshot").id(seq).data(
                    json!({"watermark_seq":latest["event_watermark"],"snapshot":latest})
                        .to_string(),
                );
                return Some((
                    Ok::<_, std::convert::Infallible>(event),
                    (state, cid, last_content),
                ));
            }
        },
    );
    let stream =
        futures_util::stream::once(async { Ok::<_, std::convert::Infallible>(initial) })
            .chain(coalesced);
    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default().interval(Duration::from_secs(10)))
        .into_response()
}

fn external_segment(value: &str) -> ApiResult<&str> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::invalid("Invalid external identifier"));
    }
    Ok(value)
}
fn remote_challenge(
    external_id: String,
    name: String,
    category: String,
    remote_state: &str,
    points: Value,
    description: String,
    hints: Value,
    artifacts: Value,
    raw: Value,
) -> Value {
    json!({"external_challenge_id":external_id,"name":name,"category":category,"remote_state":remote_state,
        "points":points,"description":description,"hints":hints,"artifacts":artifacts,"raw":raw})
}
fn extract_profile_items(connection: &Value, body: &Value) -> ApiResult<Vec<Value>> {
    let key = connection["site_profile"]["challenges_data_key"]
        .as_str()
        .unwrap_or("data");
    let items = if key.is_empty() { body } else { &body[key] };
    items
        .as_array()
        .map(|a| a.iter().filter(|v| v.is_object()).cloned().collect())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "competition.browser_shape_changed",
                "Browser API challenge response does not match the site profile",
            )
        })
}
async fn fetch_remote_challenges(
    state: &Arc<AppState>,
    connection: &Value,
    external_competition_id: &str,
) -> ApiResult<RemoteFetch> {
    let kind = field(connection, "platform_kind");
    if !matches!(kind, "ctfd" | "rctf" | "gzctf" | "generic_browser") {
        return extension_adapter::sync(state, connection, external_competition_id).await;
    }
    let mut result = Vec::new();
    match kind {
        "ctfd" => {
            let list = platform_get(state, connection, "/api/v1/challenges").await?;
            let items = list.as_array().ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "competition.platform_response",
                    "CTFd challenge list is invalid",
                )
            })?;
            for item in items {
                let external_id = item["id"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| item["id"].as_i64().map(|n| n.to_string()))
                    .unwrap_or_default();
                if external_segment(&external_id).is_err() {
                    continue;
                }
                if item["type"] == "hidden" {
                    result.push(remote_challenge(
                        external_id,
                        field(item, "name").to_owned(),
                        field(item, "category").to_owned(),
                        "hidden",
                        json!(0),
                        String::new(),
                        json!([]),
                        json!([]),
                        item.clone(),
                    ));
                    continue;
                }
                let detail = platform_get(
                    state,
                    connection,
                    &format!("/api/v1/challenges/{external_id}"),
                )
                .await?;
                let hints = detail["hints"]
                    .as_array()
                    .map(|a| a.iter().map(|h| h["content"].clone()).collect::<Vec<_>>())
                    .unwrap_or_default();
                let artifacts = detail["files"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(|f| json!({"name":f.split('?').next().unwrap_or("").rsplit('/').next().unwrap_or("")})).collect::<Vec<_>>()).unwrap_or_default();
                let solved = item["solved_by_me"] == true || detail["solved_by_me"] == true;
                result.push(remote_challenge(
                    external_id,
                    field(item, "name").to_owned(),
                    field(item, "category").to_owned(),
                    if solved { "solved_remote" } else { "open" },
                    detail
                        .get("value")
                        .cloned()
                        .unwrap_or_else(|| item["value"].clone()),
                    field(&detail, "description").to_owned(),
                    json!(hints),
                    json!(artifacts),
                    json!({"list":item,"detail":detail}),
                ));
            }
        }
        "rctf" => {
            let list = match platform_get(state, connection, "/api/v2/challs").await {
                Ok(v) => v,
                Err(error) if error.code=="competition.platform_not_found"=>platform_get(state,connection,"/api/v1/challs").await?,Err(error)=>return Err(error),
            };
            let items = list.as_array().ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "competition.platform_response",
                    "rCTF challenge list is invalid",
                )
            })?;
            for item in items {
                let external_id = field(item, "id").to_owned();
                if external_segment(&external_id).is_err() {
                    continue;
                }
                let points = if item["points"].is_object() {
                    item["points"]["max"].clone()
                } else {
                    item["points"].clone()
                };
                let artifacts = item["files"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter(|f| f.is_object())
                            .map(|f| json!({"name":f["name"],"size":f["size"]}))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let mut challenge = remote_challenge(
                    external_id,
                    field(item, "name").to_owned(),
                    field(item, "category").to_owned(),
                    "open",
                    points,
                    field(item, "description").to_owned(),
                    json!([]),
                    json!(artifacts),
                    item.clone(),
                );
                if item.get("instancerLifetime").is_some() {
                    challenge["instance_required"] = json!(item["instancerLifetime"].is_number());
                }
                result.push(challenge);
            }
        }
        "gzctf" => {
            let game = external_segment(external_competition_id)?;
            let detail =
                platform_get(state, connection, &format!("/api/game/{game}/details")).await?;
            let groups = detail
                .get("challenges")
                .or_else(|| detail.get("Challenges"))
                .cloned()
                .unwrap_or(Value::Null);
            let mut entries = Vec::new();
            if let Some(groups) = groups.as_object() {
                for (category, group) in groups {
                    if let Some(items) = group.as_array() {
                        for item in items {
                            entries.push((category.clone(), item.clone()));
                        }
                    }
                }
            } else if let Some(items) = groups.as_array() {
                for item in items {
                    entries.push((field(item, "category").to_owned(), item.clone()));
                }
            }
            for (category, item) in entries {
                let challenge_id = item["id"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| item["id"].as_i64().map(|n| n.to_string()))
                    .unwrap_or_default();
                if external_segment(&challenge_id).is_err() {
                    continue;
                }
                let full = platform_get(
                    state,
                    connection,
                    &format!("/api/game/{game}/challenges/{challenge_id}"),
                )
                .await?;
                let external_id = format!("{game}/{challenge_id}");
                let hints = full["hints"]
                    .as_array()
                    .cloned()
                    .or_else(|| item["hints"].as_array().cloned())
                    .unwrap_or_default();
                let mut artifacts = full["files"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|f| f["url"].as_str().map(|url| json!({
                                "name":f["name"].as_str().filter(|s|!s.is_empty()).unwrap_or_else(||url.split('?').next().unwrap_or("").rsplit('/').next().unwrap_or("")),
                                "size":f["size"]})))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                for key in ["attachment", "context"] {
                    if let Some(url) = full[key]["url"].as_str() {
                        let name = full[key]["name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| {
                                url.split('?')
                                    .next()
                                    .unwrap_or("")
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or("")
                            });
                        artifacts.push(json!({"name":name,"size":full[key].get("size").or_else(||full[key].get("fileSize"))}));
                    }
                }
                let mut challenge = remote_challenge(
                    external_id,
                    field(&item, "title").to_owned(),
                    category,
                    if full["accepted"] == true || item["accepted"] == true {
                        "solved_remote"
                    } else {
                        "open"
                    },
                    full.get("score")
                        .cloned()
                        .unwrap_or_else(|| item["score"].clone()),
                    field(&full, "content").to_owned(),
                    json!(hints),
                    json!(artifacts),
                    json!({"list":item,"detail":full}),
                );
                let challenge_type = full.get("type").or_else(|| item.get("type")).unwrap_or(&Value::Null);
                let instance_required = match challenge_type.as_str() {
                    Some("StaticAttachment" | "DynamicAttachment") => false,
                    Some("StaticContainer" | "DynamicContainer") => true,
                    _ => match challenge_type.as_u64() {
                        Some(0 | 2) => false,
                        Some(1 | 3) => true,
                        _ => return Err(ApiError::new(StatusCode::BAD_GATEWAY,
                            "competition.platform_response", "GZCTF challenge type is missing or unsupported")),
                    },
                };
                challenge["instance_required"] = json!(instance_required);
                challenge["platform_type"] = challenge_type.clone();
                result.push(challenge);
            }
        }
        "generic_browser" => {
            let path = profile_path(connection, "challenges_path", "")?;
            let body = platform_get(state, connection, path).await?;
            let items = extract_profile_items(connection, &body)?;
            for mut item in items {
                let external_id = item["id"]
                    .as_str()
                    .or_else(|| item["key"].as_str())
                    .or_else(|| item["slug"].as_str())
                    .map(str::to_owned)
                    .or_else(|| item["id"].as_i64().map(|n| n.to_string()))
                    .unwrap_or_default();
                if external_segment(&external_id).is_err() {
                    continue;
                }
                if connection["site_profile"]["challenge_detail_path"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                {
                    let template = profile_path(connection, "challenge_detail_path", "")?;
                    let detail_path = template.replace("{challenge_key}", &external_id);
                    let detail_body = platform_get(state, connection, &detail_path).await?;
                    let key = connection["site_profile"]["challenge_detail_data_key"]
                        .as_str()
                        .unwrap_or("data");
                    let detail = if key.is_empty() {
                        &detail_body
                    } else {
                        &detail_body[key]
                    };
                    let values = detail.as_object().ok_or_else(|| {
                        ApiError::new(
                            StatusCode::BAD_GATEWAY,
                            "competition.browser_shape_changed",
                            "Browser API detail response does not match the site profile",
                        )
                    })?;
                    if let Some(item_obj) = item.as_object_mut() {
                        for (key, value) in values {
                            item_obj.insert(key.clone(), value.clone());
                        }
                    }
                }
                let raw_state = item["state"]
                    .as_str()
                    .or_else(|| item["status"].as_str())
                    .unwrap_or("open")
                    .to_ascii_lowercase();
                let remote_state = if matches!(raw_state.as_str(), "hidden" | "locked" | "disabled")
                {
                    "hidden"
                } else if matches!(raw_state.as_str(), "solved" | "solved_remote" | "accepted") {
                    "solved_remote"
                } else {
                    "open"
                };
                let points = item
                    .get("value")
                    .or_else(|| item.get("points"))
                    .or_else(|| item.get("score"))
                    .cloned()
                    .unwrap_or_else(|| json!(0));
                let artifacts = item["files"].as_array().or_else(||item["attachments"].as_array()).map(|a|a.iter().filter_map(|raw| {
                    if let Some(url) = raw.as_str() { Some(json!({"name":url.split('?').next().unwrap_or("").rsplit('/').next().unwrap_or("")})) }
                    else if raw.is_object() { Some(json!({"name":raw["name"],"size":raw["size"]})) } else { None }
                }).collect::<Vec<_>>()).unwrap_or_default();
                let hints = item["hints"].as_array().cloned().unwrap_or_default();
                let mut challenge = remote_challenge(
                    external_id.clone(),
                    item["name"]
                        .as_str()
                        .or_else(|| item["title"].as_str())
                        .unwrap_or(&external_id)
                        .to_owned(),
                    field(&item, "category").to_owned(),
                    remote_state,
                    points,
                    item["description"]
                        .as_str()
                        .or_else(|| item["content"].as_str())
                        .unwrap_or("")
                        .to_owned(),
                    json!(hints),
                    json!(artifacts),
                    item.clone(),
                );
                challenge["target"] = item["target"].clone();
                challenge["multi_flag"] = item["multi_flag"].clone();
                challenge["expected_flags"] = item["expected_flags"].clone();
                challenge["flag_format"] = item
                    .get("flag_format")
                    .cloned()
                    .or_else(|| item.get("flagFormat").cloned())
                    .unwrap_or(Value::Null);
                result.push(challenge);
            }
        }
        _ => return Err(ApiError::unsupported("Platform adapter is unavailable")),
    }
    Ok(RemoteFetch {
        challenges: result,
        cursor: None,
        snapshot_complete: true,
        platform_status: None,
        competition_clock: None,
        notices: Vec::new(),
        extension_id: String::new(),
    })
}

#[cfg(test)]
async fn create_competition(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let cmd = command_id(&body);
    if let Some(receipt) = prior_receipt(&state, &cmd)? {
        return Ok(Json(json!({"receipt":receipt})));
    }
    let output = sync_competition_effect(&state, &body, &cmd, false).await?;
    let competition_id = field(&output,"competition_id").to_owned();
    let receipt = save_receipt(&state,&cmd,"competition.sync","competition",&competition_id,output)?;
    Ok(Json(json!({"receipt":receipt})))
}

fn applied_sync_output(state: &AppState, competition_id: &str, cmd: &str) -> ApiResult<Option<Value>> {
    Ok(state.store.list(EVENTS).map_err(ApiError::internal)?
        .into_iter().find(|event|event["event_type"] == "competition.sync.applied"
            && event["aggregate_id"] == competition_id && event["command_id"] == cmd)
        .map(|applied| {
            let mut out = json!({
                "competition_id": competition_id,
                "synced_challenges": applied["payload"]["synced_challenges"],
            });
            if let Some(changed) = applied["payload"].get("platform_status_changed") {
                out["platform_status_changed"] = changed.clone();
            }
            out
        }))
}

async fn sync_competition_effect(
    state: &Arc<AppState>, body: &Value, cmd: &str, planned: bool,
) -> ApiResult<Value> {
    let connection_id = field(&body, "connection_id");
    let external_id = field(&body, "external_competition_id");
    if connection_id.is_empty() || external_id.is_empty() {
        return Err(ApiError::invalid(
            "connection_id and external_competition_id are required",
        ));
    }
    let connection = live_connection(&state, connection_id)?;
    if connection["archived"] == true {
        return Err(ApiError::invalid("Connection has been removed"));
    }
    let old = state
        .store
        .list(COMPETITIONS)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|v| {
            v["historical"] != true
                && v["connection_id"] == connection_id
                && v["external_competition_id"] == external_id
        });
    let competition_id = old
        .as_ref()
        .map(|v| field(v, "competition_id").to_owned())
        .unwrap_or_else(|| id("comp"));
    if planned && let Some(output) = applied_sync_output(state,&competition_id,cmd)? {
        return Ok(output);
    }
    let remote = fetch_remote_challenges(&state, &connection, external_id).await?;
    let title = field(&body, "title");
    let description = field(&body, "description");
    let prior_status = old
        .as_ref()
        .and_then(|v| v.get("platform_status"))
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut record = json!({"competition_id":competition_id,"connection_id":connection_id,"external_competition_id":external_id,
        "title":if title.is_empty() { old.as_ref().and_then(|v|v["title"].as_str()).filter(|s|!s.is_empty()).unwrap_or(if planned {""} else {external_id}) } else { title },
        "description":if description.is_empty() { old.as_ref().and_then(|v|v["description"].as_str()).unwrap_or("") } else { description },
        "starts_at":body.get("starts_at").cloned().filter(|v|!v.is_null()).or_else(||old.as_ref().map(|v|v["starts_at"].clone())).unwrap_or(Value::Null),
        "ends_at":body.get("ends_at").cloned().filter(|v|!v.is_null()).or_else(||old.as_ref().map(|v|v["ends_at"].clone())).unwrap_or(Value::Null),
        "platform_status":prior_status,"scheduler_state":old.as_ref().map(|v|v["scheduler_state"].clone()).unwrap_or_else(||json!("stopped")),
        "tombstoned":false,"archived":false,"last_sync_attempt_at":now(),"last_synced_at":now(),"last_sync_error":"",
        "created_at":old.as_ref().map(|v|v["created_at"].clone()).unwrap_or_else(||json!(now())),"updated_at":now()});
    if let Some(clock) = old.as_ref().and_then(|v| v.get("competition_clock")).filter(|v| v.is_object()) {
        record["competition_clock"] = clock.clone();
    }
    let (platform_status_changed, notices) =
        extension_adapter::apply_remote_sync_metadata(state, &connection, &mut record, &remote, cmd)?;
    if !state.store.put_many_if(&[(COMPETITIONS,&competition_id,old.as_ref())],&[(COMPETITIONS,&competition_id,&record)]).map_err(ApiError::internal)?{return Err(ApiError::new(StatusCode::CONFLICT,"competition.sync.concurrent_update","Competition changed during synchronization"));}
    if state
        .store
        .get(POLICIES, &competition_id)
        .map_err(ApiError::internal)?
        .is_none()
    {
        let policy = policy_for_connection(&competition_id,&connection);
        state
            .store
            .put(POLICIES, &competition_id, &policy)
            .map_err(ApiError::internal)?;
    }
    if old.is_none() && !planned {
        append_event(
            &state,
            &competition_id,
            "competition",
            &competition_id,
            "competition.registered",
            &cmd,
            json!({"competition_id":competition_id,"connection_id":connection_id,"external_competition_id":external_id,"title":record["title"]}),
        )?;
    }
    if !planned {
        append_event(
            &state,
            &competition_id,
            "competition",
            &competition_id,
            "competition.sync.requested",
            &cmd,
            json!({"competition_id":competition_id,"connection_id":connection_id}),
        )?;
    }
    let count = save_challenges(&state, &connection, &competition_id, &cmd, &remote.challenges, remote.snapshot_complete).await?;
    let snapshot = remote_snapshot_content(&remote.challenges);
    let builtin = matches!(field(&connection,"platform_kind"),"ctfd" | "rctf" | "gzctf" | "generic_browser");
    let previous_cursor = state.store.get(SYNC_CURSORS,&competition_id).map_err(ApiError::internal)?;
    let cursor = remote.cursor.unwrap_or_else(||if builtin {
        if previous_cursor.as_ref().is_some_and(|v|v["snapshot"] == snapshot) {
            previous_cursor.as_ref().and_then(|v|v["cursor"].as_str()).unwrap_or("").to_owned()
        } else { id("sync") }
    } else { String::new() });
    if !state.store.put_many_if(&[(SYNC_CURSORS,&competition_id,previous_cursor.as_ref())],&[(SYNC_CURSORS,&competition_id,&json!({"competition_id":competition_id,"kind":"challenges","cursor":cursor,"snapshot":snapshot,"updated_at":now()}))]).map_err(ApiError::internal)?{return Err(ApiError::new(StatusCode::CONFLICT,"competition.sync.concurrent_update","A newer synchronization cursor is already committed"));}
    append_event(
        &state,
        &competition_id,
        "competition",
        &competition_id,
        if planned {"competition.sync.applied"} else {"competition.sync.completed"},
        &cmd,
        json!({
            "competition_id": competition_id,
            "synced_challenges": count["synced_challenges"],"sync_report":count,
            "platform_status_changed": platform_status_changed,
        }),
    )?;
    Ok(json!({
        "competition_id": competition_id,
        "synced_challenges": count["synced_challenges"],"sync_report":count,
        "platform_status_changed": platform_status_changed,
        "notices": notices.len(),
    }))
}

async fn patch_competition(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let cmd = command_id(&body);
    if let Some(receipt) = prior_receipt(&state, &cmd)? {
        return Ok(Json(json!({"receipt":receipt})));
    }
    let admission = &body["_planned_admission"];
    let command_id = if cmd.is_empty() { id("cmd") } else { cmd };
    loop {
        if let Some(receipt) = prior_receipt(&state,&command_id)? {
            return Ok(Json(json!({"receipt":receipt})));
        }
        if let Some(receipt) = planned_key_receipt(&state,admission)? {
            return Ok(Json(json!({"receipt":receipt})));
        }
        let previous = live_competition(&state,&competition_id)?;
        let mut competition = previous.clone();
        let mut changed = false;
        for key in ["title", "description", "starts_at", "ends_at"] {
            if let Some(value) = body.get(key) {
                if matches!(key, "starts_at" | "ends_at") && !value.is_null() {
                    let time = value.as_str()
                        .ok_or_else(|| ApiError::invalid(format!("{key} must be RFC3339")))?;
                    chrono::DateTime::parse_from_rfc3339(time)
                        .map_err(|_| ApiError::invalid(format!("{key} must be RFC3339")))?;
                }
                competition[key] = value.clone();
                changed = true;
            }
        }
        if !changed {
            return Err(ApiError::invalid("No supported competition fields were provided"));
        }
        let version_key = format!("competition\0{competition_id}");
        let version = state.store.get(VERSIONS,&version_key).map_err(ApiError::internal)?;
        if body["expected_version"].as_u64().is_some_and(|expected| {
            version.as_ref().and_then(|row|row["version"].as_u64()).unwrap_or(0) != expected
        }) {
            return Err(ApiError::new(StatusCode::CONFLICT,"command.version_conflict",
                "expected_version does not match competition stream"));
        }
        competition["updated_at"] = json!(now());
        let receipt = completed_receipt(&command_id,"competition",&competition_id,
            json!({"competition_id":competition_id}));
        let mut stored = json!({"request":{"command_type":"competition.update",
            "aggregate_id":competition_id},"response":{"receipt":receipt}});
        let claim = admission.get("claim").filter(|value| value.is_object());
        if let Some(claim) = claim {
            stored["request_fingerprint"] = claim["request_fingerprint"].clone();
            stored["status"] = json!(StatusCode::OK.as_u16());
        }
        let event = leases::planned_event(&competition_id,"competition",&competition_id,
            "competition.updated",&command_id,json!({"competition_id":competition_id}));
        let mut expected = vec![(COMPETITIONS,competition_id.as_str(),Some(&previous)),
            (COMMANDS,command_id.as_str(),None),(VERSIONS,version_key.as_str(),version.as_ref())];
        let mut records = vec![(COMPETITIONS,competition_id.as_str(),&competition),
            (COMMANDS,command_id.as_str(),&stored)];
        if let Some(claim) = claim {
            expected.push(("platform_command_claims",command_id.as_str(),None));
            records.push(("platform_command_claims",command_id.as_str(),claim));
            let key_id = field(admission,"key_id");
            if !key_id.is_empty() {
                expected.push(("platform_idempotency_keys",key_id,None));
                records.push(("platform_idempotency_keys",key_id,&admission["keyed"]));
            }
        }
        if put_records_with_events(&state,&competition_id,&expected,&records,&[event])? {
            return Ok(Json(json!({"receipt":receipt})));
        }
    }
}

async fn competition_command(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<String>,
    Json(mut body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let raw_kind = if field(&body, "command_type").is_empty() {
        field(&body, "kind")
    } else {
        field(&body, "command_type")
    }
    .to_owned();
    let kind = gateway::canonical_command_type(&raw_kind).to_owned();
    body["command_type"] = json!(kind);
    if (matches!(
        kind.as_str(),
        "challenge.select" | "challenge.queue" | "competition.challenge.queue" | "challenge.skip"
    ) || binding_outbox::is_binding_command(&kind))
        && body["payload"].is_object()
        && body["payload"].get("competition_id").is_none()
    {
        body["payload"]["competition_id"] = json!(competition_id);
    }
    let receipt = execute_command(&state, &competition_id, &body).await?;
    Ok(Json(json!({"receipt":receipt})))
}

fn build_message_response_json(message_receipt: &Value, command_receipt: Option<Value>) -> Value {
    let mut out = json!({
        "message_receipt": message_receipt,
        "command_receipt": command_receipt,
        "receipt": message_receipt,
    });
    if !message_receipt["error"].is_null() {
        out["error"] = message_receipt["error"].clone();
    }
    out
}

fn persist_attached_failed_receipt(
    state: &AppState,
    attached_cmd_id: &str,
    attached_idem_key: &str,
    attached_kind: &str,
    aggregate_type: &str,
    aggregate_id: &str,
    payload: &Value,
    expected_version: Option<u64>,
    status: StatusCode,
    error_json: &Value,
) -> ApiResult<Value> {
    let default_category = match status {
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::FORBIDDEN => "permission",
        StatusCode::CONFLICT => "state",
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "validation",
        _ => "internal",
    };
    let code = field(error_json, "code")
        .to_owned()
        .if_empty("competition.command_failed");
    let message = field(error_json, "message")
        .to_owned()
        .if_empty("attached competition command failed");
    let category = field(error_json, "category")
        .to_owned()
        .if_empty(default_category);
    let receipt_state = if code == "command.version_conflict" {
        "conflict"
    } else {
        "failed"
    };
    let timestamp = now();
    let error_obj = json!({
        "schema_version": 1,
        "code": code,
        "message": message,
        "category": category,
        "recovery_hint": field(error_json, "recovery_hint"),
        "correlation_id": attached_cmd_id,
        "retryable": false,
        "detail": error_json.get("detail").cloned().unwrap_or_else(|| json!({})),
    });
    let receipt = json!({
        "schema_version": 1,
        "command_id": attached_cmd_id,
        "receipt_id": id("rcpt"),
        "state": receipt_state,
        "run_id": null,
        "aggregate": {"type": aggregate_type, "id": aggregate_id},
        "event_cursor": null,
        "deduplicated": false,
        "submitted_at": timestamp,
        "completed_at": timestamp,
        "next": [],
        "output": {},
        "effect_ids": [],
        "error": error_obj,
    });
    let request = json!({
        "command_type": attached_kind,
        "aggregate_type": aggregate_type,
        "aggregate_id": aggregate_id,
        "payload": payload,
    });
    let envelope = json!({
        "command_type": attached_kind,
        "command_id": attached_cmd_id,
        "idempotency_key": attached_idem_key,
        "expected_version": expected_version,
        "aggregate_type": aggregate_type,
        "aggregate_id": aggregate_id,
        "payload": payload,
    });
    let (req_fp, semantic_fp, key_id) =
        crate::operations_api::competition_command_identity(&envelope);
    if let Some(existing) = state.store.get(COMMANDS, attached_cmd_id).map_err(ApiError::internal)? {
        if existing["request_fingerprint"] != req_fp {
            return Err(ApiError::new(StatusCode::CONFLICT,
                "command.idempotency_conflict", "command_id was used with different content"));
        }
        let mut prior = existing["response"]["receipt"].clone();
        prior["deduplicated"] = json!(true);
        return Ok(prior);
    }
    let stored = json!({
        "request": request,
        "request_fingerprint": req_fp,
        "status": crate::operations_api::planned_receipt_status(&receipt).as_u16(),
        "response": {"receipt": receipt},
    });
    let existing_claim = state
        .store
        .get("platform_command_claims", attached_cmd_id)
        .map_err(ApiError::internal)?;
    let mut claim_record = existing_claim.clone().unwrap_or_else(|| {
        json!({
            "command_id": attached_cmd_id,
            "request_fingerprint": req_fp,
            "created_at": timestamp,
        })
    });
    claim_record["state"] = json!("failed");
    claim_record["updated_at"] = json!(timestamp);
    let mut expected = vec![
        (COMMANDS, attached_cmd_id, None),
        (
            "platform_command_claims",
            attached_cmd_id,
            existing_claim.as_ref(),
        ),
    ];
    let mut records = vec![
        (COMMANDS, attached_cmd_id, &stored),
        ("platform_command_claims", attached_cmd_id, &claim_record),
    ];
    let existing_key = if key_id.is_empty() {
        None
    } else {
        state
            .store
            .get("platform_idempotency_keys", &key_id)
            .map_err(ApiError::internal)?
    };
    if let Some(prior_key) = &existing_key {
        if prior_key["semantic_fingerprint"] != semantic_fp {
            return Err(ApiError::new(StatusCode::CONFLICT,
                "command.idempotency_conflict", "idempotency_key was used with different content"));
        }
        let prior_id = field(prior_key, "command_id");
        let prior = state.store.get(COMMANDS, prior_id).map_err(ApiError::internal)?
            .ok_or_else(||ApiError::new(StatusCode::CONFLICT,
                "command.outcome_unknown", "Prior command outcome must be inspected before retry"))?;
        let mut receipt = prior["response"]["receipt"].clone();
        receipt["deduplicated"] = json!(true);
        return Ok(receipt);
    }
    let keyed_record = json!({
        "command_id": attached_cmd_id,
        "idempotency_key": attached_idem_key,
        "semantic_fingerprint": semantic_fp,
        "created_at": timestamp,
    });
    if !key_id.is_empty() {
        expected.push(("platform_idempotency_keys", key_id.as_str(), None));
        records.push(("platform_idempotency_keys", key_id.as_str(), &keyed_record));
    }
    let committed = state
        .store
        .put_many_if(&expected, &records)
        .map_err(ApiError::internal)?;
    if !committed {
        if let Some(prior) = state.store.get(COMMANDS, attached_cmd_id).map_err(ApiError::internal)? {
            if prior["request_fingerprint"] != req_fp {
                return Err(ApiError::new(StatusCode::CONFLICT,
                    "command.idempotency_conflict", "command_id was used with different content"));
            }
            let mut receipt = prior["response"]["receipt"].clone();
            receipt["deduplicated"] = json!(true);
            return Ok(receipt);
        }
        if !key_id.is_empty() {
            if let Some(prior_key) = state.store.get("platform_idempotency_keys", &key_id).map_err(ApiError::internal)? {
                if prior_key["semantic_fingerprint"] != semantic_fp {
                    return Err(ApiError::new(StatusCode::CONFLICT,
                        "command.idempotency_conflict", "idempotency_key was used with different content"));
                }
                let prior_id = field(&prior_key, "command_id");
                if let Some(prior) = state.store.get(COMMANDS, prior_id).map_err(ApiError::internal)? {
                    let mut receipt = prior["response"]["receipt"].clone();
                    receipt["deduplicated"] = json!(true);
                    return Ok(receipt);
                }
            }
        }
        return Err(ApiError::new(StatusCode::CONFLICT, "command.outcome_unknown",
            "Prior command outcome must be inspected before retry"));
    }
    Ok(receipt)
}

pub(super) async fn competition_message_response(
    state: Arc<AppState>,
    competition_id: String,
    body: Value,
) -> Response {
    if let Some(attached_val) = body.get("command")
        && !attached_val.is_null()
        && !attached_val.is_object()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "schema_version": 1,
                    "code": "competition.invalid_command",
                    "source_code": "competition.message.command_invalid",
                    "message": "command must be an object",
                    "category": "validation",
                    "detail": {}
                },
                "detail": "command must be an object"
            })),
        )
            .into_response();
    }

    let mut attached_kind = String::new();
    let mut attached_payload = json!({});
    let mut attached_agg_type = "competition".to_owned();
    let mut attached_agg_id = competition_id.clone();
    let mut attached_expected_version = Value::Null;
    let mut attached_semantic = Value::Null;

    if let Some(cmd_obj) = body.get("command").filter(|v| v.is_object()) {
        if cmd_obj
            .get("payload")
            .is_some_and(|p| !p.is_null() && !p.is_object())
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "schema_version": 1,
                        "code": "competition.invalid_command",
                        "message": "command.payload must be an object",
                        "category": "validation",
                        "detail": {}
                    },
                    "detail": "command.payload must be an object"
                })),
            )
                .into_response();
        }
        let raw_kind = if field(cmd_obj, "command_type").is_empty() {
            field(cmd_obj, "kind")
        } else {
            field(cmd_obj, "command_type")
        };
        let canonical = gateway::canonical_command_type(raw_kind);
        if canonical.is_empty() || !gateway::supports_command(canonical) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "schema_version": 1,
                        "code": "competition.invalid_command",
                        "source_code": "competition.command.unsupported",
                        "message": format!("unsupported competition command_type {raw_kind:?}"),
                        "category": "validation",
                        "detail": {}
                    },
                    "detail": format!("unsupported competition command_type {raw_kind:?}")
                })),
            )
                .into_response();
        }
        attached_kind = canonical.to_owned();
        attached_payload = cmd_obj
            .get("payload")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        if attached_payload.is_object() && attached_payload.get("competition_id").is_none() {
            attached_payload["competition_id"] = json!(competition_id);
        }
        let default_agg = if binding_outbox::is_binding_command(&attached_kind)
            && !field(&attached_payload, "binding_id").is_empty()
        {
            "run_binding"
        } else {
            "competition"
        };
        attached_agg_type = field(cmd_obj, "aggregate_type")
            .to_owned()
            .if_empty(default_agg);
        let default_agg_id = if attached_agg_type == "run_binding"
            && !field(&attached_payload, "binding_id").is_empty()
        {
            field(&attached_payload, "binding_id")
        } else {
            competition_id.as_str()
        };
        attached_agg_id = field(cmd_obj, "aggregate_id")
            .to_owned()
            .if_empty(default_agg_id);
        attached_expected_version = cmd_obj
            .get("expected_version")
            .cloned()
            .unwrap_or(Value::Null);
        attached_semantic = json!({
            "command_type": attached_kind,
            "aggregate_type": attached_agg_type,
            "aggregate_id": attached_agg_id,
            "payload": attached_payload,
            "expected_version": attached_expected_version,
        });
    }

    let text = field(&body, "text").trim().to_owned();
    let author = field(&body, "author").to_owned().if_empty("local-user");
    let requested_cmd_id = field(&body, "command_id").to_owned();
    let msg_idem_key = field(&body, "idempotency_key").to_owned();
    let explicit_linked_id = field(&body, "linked_command_id").to_owned();
    let semantic_fp = messages::semantic_fingerprint_for(
        &competition_id,
        &author,
        &text,
        &explicit_linked_id,
        &attached_semantic,
    );
    let probe_request = json!({
        "command_type": "competition.message",
        "aggregate_type": "competition",
        "aggregate_id": competition_id,
        "payload": {
            "competition_id": competition_id,
            "author": author,
            "text": text,
        },
    });

    let prior_record = {
        let _lock = crate::operations_api::command_dispatch_lock().lock().await;
        let by_id = if !requested_cmd_id.is_empty() {
            match state.store.get(COMMANDS, &requested_cmd_id) {
                Ok(v) => v,
                Err(error) => return ApiError::internal(error).into_response(),
            }
        } else {
            None
        };
        if let Some(prior) = by_id {
            Some(prior)
        } else if !msg_idem_key.is_empty() {
            match messages::find_prior_by_idempotency_key(&state, &msg_idem_key, &Value::Null) {
                Ok(v) => v,
                Err(error) => return error.into_response(),
            }
        } else {
            None
        }
    };

    if let Some(prior) = prior_record {
        if !messages::same_message_semantics(&prior, &probe_request, &semantic_fp) {
            return ApiError::new(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "competition.message idempotency conflict",
            )
            .into_response();
        }
        let prior_id = field(&prior["response"]["receipt"], "command_id");
        let mut message_receipt = match messages::ensure_receipt_cursor(&state, prior_id, &prior) {
            Ok(receipt) => receipt,
            Err(error) => return error.into_response(),
        };
        message_receipt["deduplicated"] = json!(true);
        let linked_id = field(&prior["plan"], "linked_command_id")
            .to_owned()
            .if_empty(field(&prior["request"]["payload"], "command_id"));
        let command_receipt = if !linked_id.is_empty() {
            state
                .store
                .get(COMMANDS, &linked_id)
                .ok()
                .flatten()
                .map(|row| {
                    let mut rcpt = row["response"]["receipt"].clone();
                    rcpt["deduplicated"] = json!(true);
                    rcpt
                })
                .or_else(|| {
                    prior["plan"]
                        .get("command_receipt")
                        .filter(|v| v.is_object())
                        .cloned()
                })
        } else {
            None
        };
        let status = crate::operations_api::planned_receipt_status(&message_receipt);
        return (
            status,
            Json(build_message_response_json(&message_receipt, command_receipt)),
        )
            .into_response();
    }

    let msg_cmd_id = if requested_cmd_id.is_empty() {
        id("cmd")
    } else {
        requested_cmd_id
    };

    let mut command_receipt: Option<Value> = None;
    if let Some(cmd_obj) = body.get("command").filter(|v| v.is_object())
        && live_competition(&state, &competition_id).is_ok()
    {
        let attached_cmd_id = field(cmd_obj, "command_id")
            .to_owned()
            .if_empty(&format!("{msg_cmd_id}:linked"));
        let attached_idem_key = field(cmd_obj, "idempotency_key")
            .to_owned()
            .if_empty(&if msg_idem_key.is_empty() {
                String::new()
            } else {
                format!("{msg_idem_key}:linked")
            });
        let attached_envelope = json!({
            "command_type": attached_kind,
            "command_id": attached_cmd_id,
            "idempotency_key": attached_idem_key,
            "expected_version": attached_expected_version,
            "aggregate_type": attached_agg_type,
            "aggregate_id": attached_agg_id,
            "payload": attached_payload,
        });
        let attached_resp = crate::operations_api::dispatch_local_competition_command(
            state.clone(),
            attached_envelope,
        )
        .await;
        let attached_status = attached_resp.status();
        let attached_bytes = match axum::body::to_bytes(attached_resp.into_body(), usize::MAX).await
        {
            Ok(bytes) => bytes,
            Err(error) => return ApiError::internal(error).into_response(),
        };
        let attached_json: Value = serde_json::from_slice(&attached_bytes).unwrap_or(Value::Null);
        if attached_json["receipt"].is_object() {
            command_receipt = Some(attached_json["receipt"].clone());
        } else if attached_json["error"].is_object() {
            let err_code = field(&attached_json["error"], "code");
            if matches!(
                err_code,
                "command.idempotency_conflict" | "command.outcome_unknown"
            ) {
                return (attached_status, Json(attached_json)).into_response();
            }
            match persist_attached_failed_receipt(
                &state,
                &attached_cmd_id,
                &attached_idem_key,
                &attached_kind,
                &attached_agg_type,
                &attached_agg_id,
                &attached_payload,
                attached_expected_version.as_u64(),
                attached_status,
                &attached_json["error"],
            ) {
                Ok(failed_receipt) => command_receipt = Some(failed_receipt),
                Err(error) => return error.into_response(),
            }
        }
    }

    let linked_id = command_receipt
        .as_ref()
        .map(|r| field(r, "command_id").to_owned())
        .unwrap_or_else(|| explicit_linked_id.clone());
    let mut message_payload = json!({
        "competition_id": competition_id,
        "text": text,
        "author": author,
        "_message_plan": {
            "idempotency_key": msg_idem_key,
            "message_semantic_fingerprint": semantic_fp,
            "linked_command_id": linked_id,
            "attached_command": attached_semantic,
            "command_receipt": command_receipt,
        }
    });
    if !linked_id.is_empty() {
        message_payload["command_id"] = json!(linked_id);
    }
    if let Some(receipt) = &command_receipt {
        message_payload["receipt_state"] = receipt["state"].clone();
    }

    let message_envelope = json!({
        "command_type": "competition.message",
        "command_id": msg_cmd_id,
        "idempotency_key": msg_idem_key,
        "expected_version": body.get("expected_version").cloned().unwrap_or(Value::Null),
        "aggregate_type": "competition",
        "aggregate_id": competition_id,
        "payload": message_payload,
    });
    let msg_resp =
        crate::operations_api::dispatch_local_competition_command(state.clone(), message_envelope)
            .await;
    let msg_status = msg_resp.status();
    let msg_bytes = match axum::body::to_bytes(msg_resp.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    let msg_json: Value = serde_json::from_slice(&msg_bytes).unwrap_or(Value::Null);
    if msg_json["receipt"].is_object() {
        let message_receipt = msg_json["receipt"].clone();
        let status = crate::operations_api::planned_receipt_status(&message_receipt);
        (
            status,
            Json(build_message_response_json(&message_receipt, command_receipt)),
        )
            .into_response()
    } else {
        (msg_status, Json(msg_json)).into_response()
    }
}

#[allow(dead_code)]
async fn competition_message(
    State(state): State<Arc<AppState>>,
    Path(competition_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let response = competition_message_response(state, competition_id, body).await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(ApiError::internal)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(ApiError::internal)?;
    if !status.is_success() && value["message_receipt"].is_null() {
        let code = match field(&value["error"], "code") {
            "command.idempotency_conflict" => "command.idempotency_conflict",
            "command.version_conflict" => "command.version_conflict",
            "competition.invalid_command" => "competition.invalid_command",
            "competition.competition.not_found" => "competition.competition.not_found",
            _ => "competition.invalid",
        };
        return Err(ApiError::new(
            status,
            code,
            field(&value["error"], "message"),
        ));
    }
    Ok(Json(value))
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}
impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_owned()
        } else {
            self
        }
    }
}

async fn execute_command(
    state: &Arc<AppState>,
    competition_id: &str,
    body: &Value,
) -> ApiResult<Value> {
    let cmd = command_id(body);
    let raw_kind = if field(body, "command_type").is_empty() {
        field(body, "kind")
    } else {
        field(body, "command_type")
    };
    let kind = gateway::canonical_command_type(raw_kind);
    if matches!(kind,"instance.ensure" | "instance.stop") && !cmd.is_empty()
        && let Some(previous) = state.store.get(COMMANDS,&cmd).map_err(ApiError::internal)?
        && previous["request"]["competition_id"].as_str().is_some() {
        let request = json!({"command_type":kind,"competition_id":competition_id,
            "payload":body.get("payload").filter(|value|value.is_object()).cloned().unwrap_or_else(||json!({}))});
        if previous["request"] != request {
            return Err(ApiError::new(StatusCode::CONFLICT,"competition.command.idempotency_conflict",
                "Command ID was used with different input"));
        }
    }
    let planned_command = binding_outbox::is_binding_command(kind)
        || matches!(
            kind,
            "challenge.select"
                | "challenge.queue"
                | "competition.challenge.queue"
                | "challenge.skip"
                | "policy.update"
                | "competition.policy.update"
                | "scheduler.start"
                | "scheduler.pause"
                | "scheduler.resume"
                | "competition.submission.submit"
                | "platform_submission.approve"
                | "platform_submission.retry"
                | "competition.submission.manual_override"
                | "instance.ensure"
                | "instance.stop"
                | "competition.message"
                | "competition.projection.rebuild"
                | "competition.projection.rebuild_all"
        );
    if !planned_command && let Some(receipt) = prior_receipt(state, &cmd)? {
        return Ok(receipt);
    }
    if matches!(kind,"competition.submission.submit" | "platform_submission.approve") {
        let payload = body.get("payload").filter(|v| v.is_object()).cloned()
            .unwrap_or_else(||json!({}));
        return submission_commands::execute(state,competition_id,kind,&cmd,&payload,
            body["expected_version"].as_u64(),
            body["cursor_scope"].as_str().unwrap_or("operator:local-user"),
            &body["_planned_admission"],field(body,"actor_id"));
    }
    if kind == "platform_submission.retry" {
        let payload = body.get("payload").filter(|v| v.is_object()).cloned()
            .unwrap_or_else(||json!({}));
        return submission_retry::execute(state,competition_id,&cmd,&payload,
            body["expected_version"].as_u64(),
            body["cursor_scope"].as_str().unwrap_or("operator:local-user"),
            &body["_planned_admission"]);
    }
    if kind == "competition.submission.manual_override" {
        let payload = body.get("payload").filter(|v|v.is_object()).cloned()
            .unwrap_or_else(||json!({}));
        return manual_override::execute(state,competition_id,&cmd,&payload,
            body["expected_version"].as_u64(),
            body["cursor_scope"].as_str().unwrap_or("operator:local-user"),
            &body["_planned_admission"],field(body,"actor_id"));
    }
    if kind == "competition.projection.rebuild" {
        let payload = body.get("payload").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
        let name = field(&payload, "projection_name").to_owned().if_empty(field(&payload, "name"));
        let replayed = projections::rebuild(state, &name)?;
        let wm = projections::watermark(state, &name)?;
        return save_receipt(
            state,
            &cmd,
            "competition.projection.rebuild",
            "competition",
            &name,
            json!({"projection_name": name, "replayed": replayed, "watermark": wm}),
        );
    }
    if kind == "competition.projection.rebuild_all" {
        let replayed = projections::rebuild_all(state)?;
        let wms = projections::watermarks(state)?;
        return save_receipt(
            state,
            &cmd,
            "competition.projection.rebuild_all",
            "competition",
            competition_id,
            json!({"replayed": replayed, "watermarks": wms}),
        );
    }
    if kind == "competition.message" {
        let mut payload = body.get("payload").filter(|v| v.is_object()).cloned().unwrap_or_else(|| json!({}));
        payload["competition_id"] = json!(competition_id);
        return messages::execute(
            state,
            competition_id,
            &cmd,
            field(body, "idempotency_key"),
            body["expected_version"].as_u64(),
            body["cursor_scope"].as_str().unwrap_or("operator:local-user"),
            &body["_planned_admission"],
            &payload,
        );
    }
    let competition = live_competition(state, competition_id)?;
    if !planned_command && competition["archived"] == true {
        return Err(ApiError::invalid("Competition has been removed"));
    }
    let mut payload = body
        .get("payload")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    if payload.is_object() {
        if body.get("expected_version").is_some() && payload.get("expected_version").is_none() {
            payload["expected_version"] = body["expected_version"].clone();
        }
        if body.get("idempotency_key").is_some() && payload.get("idempotency_key").is_none() {
            payload["idempotency_key"] = body["idempotency_key"].clone();
        }
        if body.get("cursor_scope").is_some() && payload.get("cursor_scope").is_none() {
            payload["cursor_scope"] = body["cursor_scope"].clone();
        }
    }
    match kind {
        "policy.update" | "competition.policy.update"
        | "scheduler.start" | "scheduler.pause" | "scheduler.resume" =>
            control_commands::execute(state,competition_id,kind,&cmd,&payload,
                body["expected_version"].as_u64(),
                body["cursor_scope"].as_str().unwrap_or("operator:local-user"),
                &body["_planned_admission"]),
        "challenge.select"
        | "challenge.queue"
        | "competition.challenge.queue"
        | "challenge.skip" => challenge_commands::execute(state,competition_id,kind,&cmd,
            &payload,body["expected_version"].as_u64(),
            body["cursor_scope"].as_str().unwrap_or("operator:local-user"),
            &body["_planned_admission"]),
        "competition.submission.submit" => {
            submit_command(state, competition_id, kind, &cmd, &payload).await
        }
        "platform_submission.approve" | "platform_submission.retry" => {
            retry_or_approve(state, competition_id, kind, &cmd, &payload).await
        }
        "instance.ensure" | "instance.stop" => {
            leases::command(state, competition_id, kind, &cmd, &payload,
                &body["_planned_admission"]).await
        }
        _ if binding_outbox::is_binding_command(kind) => {
            binding_outbox::command(
                state,
                Some(competition_id),
                kind,
                &cmd,
                &payload,
                &body["_planned_admission"],
            )
            .await
        }
        _ => Err(ApiError::invalid(format!(
            "Unknown competition command {kind:?}"
        ))),
    }
}

async fn binding_command(state:&Arc<AppState>,competition_id:&str,kind:&str,command_id:&str,payload:&Value)->ApiResult<Value>{
    binding_outbox::internal(state,competition_id,kind,command_id,payload).await
}

async fn instance_command(
    state: &Arc<AppState>,
    competition_id: &str,
    kind: &str,
    command_id: &str,
    payload: &Value,
) -> ApiResult<Value> {
    leases::command(state, competition_id, kind, command_id, payload, &Value::Null).await
}

#[cfg(test)]
async fn dispatch_queued(state: Arc<AppState>, competition_id: &str) -> ApiResult<()> {
    dispatch_queued_inner(state, competition_id, false).await
}

async fn dispatch_queued_with_advisor(state: Arc<AppState>, competition_id: &str) -> ApiResult<()> {
    dispatch_queued_inner(state, competition_id, true).await
}

async fn dispatch_queued_inner(state: Arc<AppState>, competition_id: &str, consult_advisor: bool) -> ApiResult<()> {
    static DISPATCH_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _dispatch = DISPATCH_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let competition = live_competition(&state, competition_id)?;
    let policy = state
        .store
        .get(POLICIES, competition_id)
        .map_err(ApiError::internal)?
        .unwrap_or(Value::Null);
    scheduler::detect_mode_change(&state, competition_id,field(&policy,"automation_mode"))?;
    if competition["scheduler_state"] != "running" {
        return Ok(());
    }
    if competition["ends_at"].as_str()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .is_some_and(|end| end <= chrono::Utc::now()) {
        return Ok(());
    }
    let budgets = scheduler::settle_budgets(&state, competition_id, &policy)?;
    let advice = if consult_advisor {
        advisor::consult(&state, &competition).await
    } else {
        json!({"status":"disabled","recommendations":[]})
    };
    if policy["automation_mode"] != "observe" {
        scheduler::enforce_terminal_phase_gate(&state, &competition, &policy).await?;
        scheduler::enforce_deepchain(&state, &competition, &policy).await?;
    }
    let connection = live_connection(&state, field(&competition, "connection_id"))?;
    let configured = policy["max_concurrent_runs"].as_i64().unwrap_or(2);
    let max = if configured > 0 { configured as usize } else { usize::MAX };
    let active = scoped(&state, BINDINGS, competition_id)?
        .iter()
        .filter(|v| {
            v["state"] == "running"
                && v["run_id"]
                    .as_str()
                    .and_then(|run_id| state.store.get("runs", run_id).ok().flatten())
                    .is_some_and(|run| run["running"] == true)
        })
        .count();
    let queue = scheduler::prepare_tick_with_advice(&state, &competition, &policy, &budgets, &advice)?;
    if active>=max{for entry in &queue{scheduler::capacity_hold(&state,competition_id,entry,"run_capacity")?;}return Ok(());}
    let mut slots = max - active;
    for mut entry in queue {
        let queue_before=entry.clone();
        if slots==0{scheduler::capacity_hold(&state,competition_id,&entry,"run_capacity")?;continue;}
        let cid = field(&entry, "competition_challenge_id").to_owned();
        let Some(challenge) = state
            .store
            .get(CHALLENGES, &cid)
            .map_err(ApiError::internal)?
        else {
            continue;
        };
        if challenge["tombstoned"] == true || challenge["remote_state"] != "open" {
            continue;
        }
        let category = field(&challenge, "category");
        if policy["category_deny"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == category))
            || policy["category_allow"]
                .as_array()
                .is_some_and(|a| !a.is_empty() && !a.iter().any(|v| v == category))
        {
            continue;
        }
        let Some(revision_id) = challenge["current_revision_id"].as_str() else {
            continue;
        };
        let Some(revision) = state
            .store
            .get(REVISIONS, revision_id)
            .map_err(ApiError::internal)?
        else {
            continue;
        };
        let existing_binding = scoped(&state, BINDINGS, competition_id)?
            .into_iter()
            .filter(|binding| binding["competition_challenge_id"] == cid)
            .filter(|binding| matches!(field(binding, "state"),
                "planned" | "creating" | "starting" | "running" | "resolving" |
                "local_finished" | "remote_pending" | "rejected" | "paused")
                || policy["policy_profile"] == "tsec_eval" && binding["state"] == "finished")
            .max_by(|a, b| field(a, "updated_at").cmp(field(b, "updated_at"))
                .then_with(|| field(a, "binding_id").cmp(field(b, "binding_id"))));
        if let Some(binding) = &existing_binding {
            let reason = if binding["revision_id"] != revision_id {
                "binding_revision_conflict"
            } else if matches!(field(binding, "state"), "paused" | "finished") {
                ""
            } else {
                "already_running"
            };
            if !reason.is_empty() {
                entry["admission_decision"]["action"] = json!("hold");
                entry["admission_decision"]["reason"] = json!(reason);
                entry["updated_at"] = json!(now());
                save_queue_decision(&state,&cid,&queue_before,&entry)?;
                continue;
            }
            let run_id = field(binding, "run_id");
            let run = state.store.get("runs", run_id).map_err(ApiError::internal)?;
            if run_id.is_empty() || run.as_ref().is_none_or(|run| run["running"] == true)
                || state.runs.active_worker_count(run_id) > 0 {
                entry["admission_decision"]["action"] = json!("hold");
                entry["admission_decision"]["reason"] = json!("binding_execution_busy");
                entry["updated_at"] = json!(now());
                save_queue_decision(&state,&cid,&queue_before,&entry)?;
                continue;
            }
        }
        let attachment_paths = match artifacts::local_paths(&state, &revision) {
            Ok(paths) => paths,
            Err(error) => {
                entry["state"] = json!("held");
                entry["admission_decision"]["action"] = json!("hold");
                entry["admission_decision"]["reason"] = json!("artifact_unavailable");
                entry["admission_decision"]["code"] = json!(error.code);
                save_queue_decision(&state,&cid,&queue_before,&entry)?;
                continue;
            }
        };
        if scheduler::deepchain_hold(&state, competition_id, &policy, &challenge, &revision)? {
            entry["admission_decision"]["action"] = json!("hold");
            entry["admission_decision"]["reason"] = json!("deepchain_slots");
            save_queue_decision(&state,&cid,&queue_before,&entry)?;
            continue;
        }
        let working_set = policy["working_set"].as_u64().unwrap_or(0);
        if working_set > 0
            && visit::field(&entry,"visit_count")
                .as_u64()
                .unwrap_or(0)
                > 0
            && (max - slots) as u64 >= working_set
        {
            entry["admission_decision"]["action"] = json!("hold");
            entry["admission_decision"]["reason"] = json!("working_set");
            save_queue_decision(&state,&cid,&queue_before,&entry)?;
            continue;
        }
        let mut description = field(&revision, "description").to_owned();
        if let Some(hints) = revision["hints"]
            .as_array()
            .filter(|hints| !hints.is_empty())
        {
            let rendered = hints
                .iter()
                .filter_map(Value::as_str)
                .map(|hint| format!("- {hint}"))
                .collect::<Vec<_>>()
                .join("\n");
            if !rendered.is_empty() {
                description = format!("{description}\n\nHints:\n{rendered}")
                    .trim()
                    .to_owned();
            }
        }
        let mut target = field(&revision, "target").to_owned();
        if description.is_empty() && target.is_empty() && attachment_paths.is_empty() {
            entry["state"] = json!("held");
            entry["admission_decision"]["action"] = json!("hold");
            entry["admission_decision"]["reason"] = json!("missing_description_or_target");
            save_queue_decision(&state,&cid,&queue_before,&entry)?;
            continue;
        }
        let worker_config =
            crate::settings_metadata::configuration(&state).unwrap_or_else(|_| json!({}));
        let worker_limit = worker_config["max_total_workers"].as_u64().unwrap_or(0);
        if worker_limit > 0 {
            let active_workers: u64 = state
                .store
                .list("runs")
                .map_err(ApiError::internal)?
                .iter()
                .filter(|run| run["running"] == true && run["historical"] != true)
                .map(|run| state.runs.active_worker_count(field(run, "run_id")) as u64)
                .sum();
            let required = 1;
            if active_workers + required > worker_limit {
                entry["admission_decision"]["action"] = json!("hold");
                entry["admission_decision"]["reason"] = json!("worker_capacity");
                save_queue_decision(&state,&cid,&queue_before,&entry)?;
                continue;
            }
        }
        let run_id = existing_binding.as_ref()
            .map(|binding| field(binding, "run_id").to_owned())
            .unwrap_or_else(|| id("run"));
        let staged_attachments = match artifacts::stage_for_run(&state, &revision, &run_id) {
            Ok(paths) => paths,
            Err(error) => {
                entry["state"] = json!("held");
                entry["admission_decision"]["action"] = json!("hold");
                entry["admission_decision"]["reason"] = json!("artifact_unavailable");
                entry["admission_decision"]["code"] = json!(error.code);
                save_queue_decision(&state,&cid,&queue_before,&entry)?;
                continue;
            }
        };
        let mut lease_id = Value::Null;
        let mut new_lease = false;
        if target.is_empty()
            && revision["instance_required"] != false
            && (connection["capabilities"]["dynamic_instances"] == true
                || connection["capabilities"]["instances"] == true)
        {
            let leases = scoped(&state, LEASES, competition_id)?;
            if let Some(lease) = leases.iter()
                .find(|v| v["competition_challenge_id"] == cid && v["state"] == "active") {
                if lease["reconcile_degraded"] == true {
                    entry["admission_decision"]["action"] = json!("hold");
                    entry["admission_decision"]["reason"] = json!("instance_reconcile_degraded");
                    entry["updated_at"] = json!(now());
                    save_queue_decision(&state,&cid,&queue_before,&entry)?;
                    continue;
                }
                target = field(&lease, "address").to_owned();
                lease_id = lease["lease_id"].clone();
            } else if leases.iter().any(|v| v["competition_challenge_id"] == cid
                && matches!(field(v, "state"), "requested" | "provisioning" | "renewing" | "releasing")) {
                entry["admission_decision"]["action"] = json!("hold");
                entry["admission_decision"]["reason"] = json!("instance_lease_pending");
                entry["updated_at"] = json!(now());
                save_queue_decision(&state,&cid,&queue_before,&entry)?;
                continue;
            } else {
                match instance_command(
                    &state,
                    competition_id,
                    "instance.ensure",
                    "",
                    &json!({"challenge_id":cid}),
                )
                .await
                {
                    Ok(receipt) => {
                        if matches!(field(&receipt,"state"),"accepted" | "waiting" | "running") {
                            entry["admission_decision"]["action"] = json!("hold");
                            entry["admission_decision"]["reason"] = json!("instance_lease_pending");
                            entry["updated_at"] = json!(now());
                            save_queue_decision(&state,&cid,&queue_before,&entry)?;
                            continue;
                        }
                        target = field(&receipt["output"], "address").to_owned();
                        lease_id = receipt["output"]["lease_id"].clone();
                        new_lease = true;
                    }
                    Err(error) => {
                        entry["admission_decision"]["action"] = json!("hold");
                        entry["admission_decision"]["reason"] = json!("instance_provision_failed");
                        entry["not_before"] =
                            json!((chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339());
                        save_queue_decision(&state,&cid,&queue_before,&entry)?;
                        tracing::warn!(challenge_id=%cid,code=%error.code,"Competition instance provisioning failed");
                        continue;
                    }
                }
            }
            if target.is_empty() {
                entry["admission_decision"]["action"] = json!("hold");
                entry["admission_decision"]["reason"] = json!("instance_address_missing");
                save_queue_decision(&state,&cid,&queue_before,&entry)?;
                continue;
            }
        }
        let mut challenge_spec = json!({"mode":"ctf","id":revision["revision_id"],"name":revision["name"],"category":scheduler::category(field(&revision,"category")),"points":revision["points"].as_f64().unwrap_or(0.).round_ties_even() as i64,
            "description":description,"target":target,"flag_format":revision["flag_format"],"expected_flags":revision["expected_flags"],
            "multi_flag":revision["multi_flag"],"attachments":staged_attachments,"platform_confirmation_required":true});
        let mut start_body=scheduler::dispatch_parameters(&policy,&challenge,&entry,&connection);
        if start_body["allow_operator_input"]==false{challenge_spec["allow_operator_input"]=json!(false);}
        start_body["challenge"]=challenge_spec.clone();
        let planned_binding=if existing_binding.is_none(){match binding_outbox::ensure_scheduler_start(&state,competition_id,&challenge,&revision,&run_id,&lease_id,&entry,&start_body).await{Ok(binding)=>Some(binding),Err(error)if error.code=="competition.binding.concurrent_update"=>{scheduler::capacity_hold(&state,competition_id,&entry,"binding_admission_changed")?;continue;},Err(error)=>return Err(error)}}else{None};
        if let Some(binding)=&planned_binding{if binding["state"]!="running"{slots=slots.saturating_sub(1);continue;}}
        let (dispatch_status, dispatch_error) = if let Some(binding) = &existing_binding {
            let result = binding_command(
                &state,
                competition_id,
                "run_binding.resolve",
                "",
                &{let mut request=start_body.clone();request["challenge_id"]=json!(cid);request["binding_id"]=binding["binding_id"].clone();request},
            )
            .await;
            match result {
                Ok(_) => (StatusCode::OK, None),
                Err(error) => (error.status, Some(error.code)),
            }
        } else {
            (StatusCode::OK,None)

        };
        if !dispatch_status.is_success() {
            if new_lease {
                if let Some(lease_id) = lease_id.as_str() {
                    let _ = instance_command(
                        &state,
                        competition_id,
                        "instance.stop",
                        "",
                        &json!({"lease_id":lease_id}),
                    )
                    .await;
                }
            }
            entry["state"] = json!("held");
            entry["admission_decision"]["action"] = json!("hold");
            entry["admission_decision"]["reason"] = json!("run_preflight_failed");
            entry["admission_decision"]["http_status"] = json!(dispatch_status.as_u16());
            if let Some(code) = dispatch_error {
                entry["admission_decision"]["code"] = json!(code);
            }
            entry["not_before"] =
                json!((chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339());
            save_queue_decision(&state,&cid,&queue_before,&entry)?;
            append_event(
                &state,
                competition_id,
                "challenge",
                &cid,
                "competition.scheduler.dispatch_paused",
                "",
                json!({"challenge_id":cid,"reason":"run_preflight_failed"}),
            )?;
            continue;
        }
        if let Some(binding)=&existing_binding{let current=state.store.get(BINDINGS,field(binding,"binding_id")).map_err(ApiError::internal)?;if current.as_ref().is_none_or(|binding|binding["state"]!="running"){slots=slots.saturating_sub(1);continue;}}
        if let Some(binding)=existing_binding{
            let binding_id=field(&binding,"binding_id");let mut attached=false;
            for _ in 0..32{let Some(before)=state.store.get(BINDINGS,binding_id).map_err(ApiError::internal)?else{break};if before["state"]!="running"{break;}let mut next=before.clone();next["lease_id"]=lease_id.clone();next["updated_at"]=json!(now());if state.store.put_many_if(&[(BINDINGS,binding_id,Some(&before))],&[(BINDINGS,binding_id,&next)]).map_err(ApiError::internal)?{attached=true;break;}}
            if !attached{scheduler::capacity_hold(&state,competition_id,&entry,"binding_changed_after_start")?;}
        }
        slots -= 1;
    }
    Ok(())
}
fn run_flag_event(event: &Value) -> Option<&str> {
    match field(event,"event_type") {
        "flag.accepted" => event["payload"]["flag"].as_str(),
        "blackboard.delta" if event["payload"]["kind"] == "flag_found" =>
            event["payload"]["flag"].as_str(),
        _ => None,
    }
}

fn run_flag_generation(event: &Value, fallback: u64) -> u64 {
    event["payload"]["execution_generation"].as_u64().filter(|value|*value > 0)
        .or_else(||event["execution_generation"].as_u64().filter(|value|*value > 0))
        .or_else(||field(event,"solver_id").rsplit_once("-g")
            .and_then(|(_,value)|value.parse::<u64>().ok()).filter(|value|*value > 0))
        .unwrap_or(fallback)
}

fn run_flag_actor(event: &Value) -> &str {
    event["payload"]["actor"].as_str().filter(|value|!value.is_empty())
        .or_else(||event["solver_id"].as_str()).unwrap_or("")
}

/// Commit local visit projections together; a watcher from an older execution cannot
/// harvest a newer visit or publish a partial binding/challenge/queue transition.
fn harvest_run_visit(state:&AppState,competition_id:&str,challenge_id:&str,run_id:&str,binding_id:&str,generation:u64,solved:bool,revisit:bool)->ApiResult<()>{
    state.store.document_scope(||->ApiResult<()>{
        let run=state.store.get("runs",run_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("run"))?;
        let binding=state.store.get(BINDINGS,binding_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("binding"))?;
        if run["running"]==true || run["execution_generation"].as_u64()!=Some(generation) || binding["execution_generation"].as_u64()!=Some(generation){return Err(ApiError::new(StatusCode::CONFLICT,"competition.visit.execution_stale","Terminal visit no longer owns this execution"));}
        let challenge=state.store.get(CHALLENGES,challenge_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("challenge"))?;
        let queue=state.store.get(QUEUE,challenge_id).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("queue entry"))?;
        let execution=visit::Execution{run_id:run_id.into(),generation};
        if !queue["visit"].is_object()&&!field(&queue["admission_decision"],"binding_id").is_empty()&&queue["admission_decision"]["binding_id"]!=binding_id{return Ok(());}
        let visit_owner=visit::matches_execution(&queue,&execution)?;
        if visit_owner==Some(false){return Ok(());}
        if visit_owner.is_none()&&scoped(state,BINDINGS,competition_id)?.iter().any(|other|other["competition_challenge_id"]==challenge_id&&other["binding_id"]!=binding_id&&matches!(field(other,"state"),"planned"|"creating"|"starting"|"running"|"resolving"|"paused")){return Err(ApiError::new(StatusCode::CONFLICT,"competition.visit.owner_unconfirmed","Legacy visit has competing bindings; preserve current challenge projection"));}
        let candidates=scoped(state,CANDIDATES,competition_id)?;
        let count=candidates.iter().filter(|candidate|candidate["competition_challenge_id"]==challenge_id).count() as u64;
        let mut next_queue=queue.clone();
        if !visit::finish(&mut next_queue,Some(&visit::Execution{run_id:run_id.into(),generation}),count)?{return Ok(());}
        if matches!(field(&next_queue,"state"),"running"|"dispatching"|"dispatched"){next_queue["state"]=json!(if revisit{"queued"}else{"done"});}
        next_queue["updated_at"]=json!(now());
        let mut next_binding=binding.clone();let target=if solved{"solved"}else if revisit{"paused"}else{"finished"};
        next_binding["state"]=json!(target);next_binding["updated_at"]=json!(now());
        let mut next_challenge=challenge.clone();
        let awaiting=candidates.iter().any(|candidate|candidate["competition_challenge_id"]==challenge_id && candidate["source_run_id"]==run_id && candidate["gate_verdict"]=="confirmed" && matches!(field(candidate,"state"),"candidate"|"awaiting_approval"|"approved"));
        if challenge["state"]!="solved" {
            if solved||awaiting{next_challenge["state"]=json!("candidate_found");}
            else if revisit{if !matches!(field(&challenge,"state"),"running"|"candidate_found"|"submitting"){next_challenge["state"]=json!("queued");}}
            else{next_challenge["state"]=json!("paused");}
        }
        next_challenge["updated_at"]=json!(now());
        let mut events=vec![leases::planned_event(competition_id,"run_binding",binding_id,"competition.run_binding.state_changed","",json!({"challenge_id":challenge_id,"run_id":run_id,"execution_generation":generation,"from":binding["state"],"to":target}))];
        if revisit{events.push(leases::planned_event(competition_id,"challenge",challenge_id,"competition.queue.enqueued","",json!({"challenge_id":challenge_id,"reason":"visit_finished"})));}
        if !put_records_with_events(state,competition_id,&[("runs",run_id,Some(&run)),(BINDINGS,binding_id,Some(&binding)),(CHALLENGES,challenge_id,Some(&challenge)),(QUEUE,challenge_id,Some(&queue))],&[(BINDINGS,binding_id,&next_binding),(CHALLENGES,challenge_id,&next_challenge),(QUEUE,challenge_id,&next_queue)],&events)?{
            return Err(ApiError::new(StatusCode::CONFLICT,"competition.visit.projection_pending","Terminal visit projection changed; retry"));
        }Ok(())
    }).map_err(ApiError::internal)?
}

fn spawn_run_watcher(
    state: Arc<AppState>,
    competition_id: String,
    challenge_id: String,
    run_id: String,
    binding_id: String,
    execution_generation: u64,
) {
    tokio::spawn(async move {
        let mut last_flag_event_seq = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let Ok(Some(run)) = state.store.get("runs", &run_id) else {
                break;
            };
            if run["execution_generation"].as_u64().unwrap_or(0) != execution_generation
                || state.store.get(BINDINGS, &binding_id).ok().flatten()
                    .is_none_or(|binding| binding["execution_generation"].as_u64().unwrap_or(0) != execution_generation)
            {
                break;
            }
            let scan_state = state.clone();
            let scan_run_id = run_id.clone();
            let scan_comp_id = competition_id.clone();
            let scan_chal_id = challenge_id.clone();
            let scan_bind_id = binding_id.clone();
            let scan_after = last_flag_event_seq;
            if let Ok(next_seq) = tokio::task::spawn_blocking(move || {
                let mut cur_seq = scan_after;
                for (seq, maybe_event) in
                    super::read_jsonl_event_scan(&scan_state, &scan_run_id, scan_after)
                {
                    if seq <= cur_seq {
                        continue;
                    }
                    let Some(event) = maybe_event else {
                        cur_seq = seq;
                        continue;
                    };
                    let Some(answer) = run_flag_event(&event) else {
                        cur_seq = seq;
                        continue;
                    };
                    let worker_id = run_flag_actor(&event);
                    let source_generation = run_flag_generation(&event, execution_generation);
                    if source_generation != execution_generation {
                        cur_seq = seq;
                        continue;
                    }
                    match submission_commands::candidate_exists(
                        &scan_state,
                        &scan_comp_id,
                        &scan_chal_id,
                        1,
                        answer,
                    ) {
                        Ok(true) => {
                            cur_seq = seq;
                            continue;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(
                                run_id = scan_run_id,
                                code = %error.code,
                                "Competition candidate lookup pending"
                            );
                            break;
                        }
                    }
                    let command_id = format!(
                        "competition-run-flag:{scan_bind_id}:{execution_generation}:{seq}"
                    );
                    let payload = json!({
                        "challenge_id": scan_chal_id,
                        "answer": answer,
                        "answer_slot": 1,
                        "source_run_id": scan_run_id,
                        "source_kind": "run_event",
                        "witness": format!("run_event:{scan_run_id}:{seq}"),
                        "execution_generation": execution_generation,
                        "worker_id": worker_id
                    });
                    match submission_commands::execute(
                        &scan_state,
                        &scan_comp_id,
                        "competition.submission.submit",
                        &command_id,
                        &payload,
                        None,
                        "system:competition",
                        &Value::Null,
                        "system",
                    ) {
                        Ok(_) => cur_seq = seq,
                        Err(error) => {
                            tracing::warn!(
                                run_id = scan_run_id,
                                code = %error.code,
                                "Competition running Flag candidate projection pending"
                            );
                            break;
                        }
                    }
                }
                cur_seq
            })
            .await
            {
                last_flag_event_seq = next_seq;
            }
            if run["running"] == true {
                if let Ok(Some(entry)) = state.store.get(QUEUE, &challenge_id) {
                    if visit::field(&entry,"persistent_execution") != true {
                        let timebox = visit::field(&entry,"visit_timebox_s")
                            .as_u64()
                            .unwrap_or(0);
                        let elapsed = visit::field(&entry,"last_visit_at")
                            .as_str()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map(|at| {
                                chrono::Utc::now()
                                    .signed_duration_since(at)
                                    .num_seconds()
                                    .max(0) as u64
                            })
                            .unwrap_or(0);
                        if timebox > 0 && elapsed >= timebox {
                            let response = super::control_run(State(state.clone()),Path(run_id.clone()),Json(json!({"action":"stop","text":"visit_timebox","command_id":id("cmd")}))).await;
                            if response.status().is_success() {
                                let _ = append_event(
                                    &state,
                                    &competition_id,
                                    "run_binding",
                                    &binding_id,
                                    "competition.scheduler.dispatch_paused",
                                    "",
                                    json!({"challenge_id":challenge_id,"run_id":run_id,"reason":"visit_timebox"}),
                                );
                            }
                        }
                    }
                }
                continue;
            }
            if run["outcome_detail"] == "competition_binding_pause"
                || state.store.get(BINDINGS, &binding_id).ok().flatten()
                    .is_some_and(|binding| binding["state"] == "paused") {
                break;
            }
            let solved = run["solved"] == true;
            let policy = state.store.get(POLICIES, &competition_id).ok().flatten();
            let revisit = !solved
                && policy
                    .as_ref()
                    .is_some_and(|p| p["policy_profile"] == "tsec_eval")
                && state
                    .store
                    .get(CHALLENGES, &challenge_id)
                    .ok()
                    .flatten()
                    .is_some_and(|challenge| {
                        challenge["remote_state"] == "open"
                            && challenge["tombstoned"] != true
                            && !policy
                                .as_ref()
                                .and_then(|p| p["persistent_challenge_ids"].as_array())
                                .is_some_and(|ids| {
                                    ids.iter()
                                        .any(|id| id == field(&challenge, "external_challenge_id"))
                                })
                    });
            if let Err(error)=harvest_run_visit(&state,&competition_id,&challenge_id,&run_id,&binding_id,execution_generation,solved,revisit){
                tracing::warn!(run_id,code=error.code,"Competition terminal visit projection pending");continue;
            }
            if let Ok(Some(binding)) = state.store.get(BINDINGS, &binding_id) {
                if let Some(lease_id) = binding["lease_id"].as_str().filter(|s| !s.is_empty()) {
                    if let Ok(Some(lease)) = state.store.get(LEASES, lease_id) {
                        if lease["state"] == "active" {
                            if let Err(error) = instance_command(
                                &state,
                                &competition_id,
                                "instance.stop",
                                "",
                                &json!({"lease_id":lease_id}),
                            )
                            .await
                            {
                                let _ = append_event(
                                    &state,
                                    &competition_id,
                                    "instance_lease",
                                    lease_id,
                                    "competition.instance.reconcile_degraded",
                                    "",
                                    json!({"lease_id":lease_id,"challenge_id":challenge_id,"reason":error.code}),
                                );
                            }
                        }
                    }
                }
            }
            break;
        }
    });
}

fn safe_candidate(mut candidate: Value) -> Value {
    if let Some(object) = candidate.as_object_mut() {
        object.remove("answer_ref");
        object.remove("value");
    }
    candidate
}
fn safe_submission(mut submission: Value) -> Value {
    if let Some(object) = submission.as_object_mut() {
        object.remove("answer_ref");
    }
    submission
}
#[derive(Clone,Copy,serde::Serialize,serde::Deserialize)]
#[serde(rename_all="snake_case")]
enum SubmissionCertainty { NotDispatched, Rejected, Confirmed, Unknown }
impl SubmissionCertainty {
    fn retry_safe(self)->bool{matches!(self,Self::NotDispatched|Self::Rejected)}
}
struct SubmitOutcome {
    certainty: SubmissionCertainty,
    state: String,
    remote_receipt: String,
    retry_after_seconds: Option<f64>,
}
fn submit_outcome(state: &str, remote_receipt: impl Into<String>, retry_after_seconds: Option<f64>) -> SubmitOutcome {
    let certainty=match state {
        "rate_limited"|"auth_required"=>SubmissionCertainty::Rejected,
        "correct"|"wrong"|"partial"|"duplicate_or_solved"=>SubmissionCertainty::Confirmed,
        _=>SubmissionCertainty::Unknown,
    };
    SubmitOutcome { certainty,state: state.to_owned(), remote_receipt: remote_receipt.into(), retry_after_seconds }
}
fn ctfd_retry_seconds(message: &str) -> Option<f64> {
    let lower = message.to_ascii_lowercase();
    let remainder = lower.split_once("try again in ")?.1.trim_start();
    let digits = remainder.chars().take_while(char::is_ascii_digit).collect::<String>();
    if !remainder[digits.len()..].trim_start().starts_with("second") { return None; }
    digits.parse::<f64>().ok()
}
async fn platform_submit(
    state: &Arc<AppState>,
    connection: &Value,
    challenge: &Value,
    answer: &str,
    submission_id:&str,
) -> ApiResult<SubmitOutcome> {
    let external = field(challenge, "external_challenge_id");
    let kind = field(connection, "platform_kind");
    if !matches!(kind, "ctfd" | "rctf" | "gzctf" | "generic_browser") {
        let (state_name, receipt) = extension_adapter::submit(state, connection, challenge, answer,submission_id).await?;
        return Ok(submit_outcome(&state_name, receipt, None));
    }
    let (status, body, retry_after) = match kind {
        "ctfd" => {
            let challenge_id = external
                .parse::<u64>()
                .map(|n| json!(n))
                .unwrap_or_else(|_| json!(external));
            platform_request_header(
                state,
                connection,
                reqwest::Method::POST,
                "/api/v1/challenges/attempt",
                Some(json!({"challenge_id":challenge_id,"submission":answer})),
                None,
            )
            .await?
        }
        "rctf" => {
            external_segment(external)?;
            platform_request_header(
                state,
                connection,
                reqwest::Method::POST,
                &format!("/api/v1/challs/{external}/submit"),
                Some(json!({"flag":answer})),
                None,
            )
            .await?
        }
        "gzctf" => {
            let (game, cid) = external
                .split_once('/')
                .ok_or_else(|| ApiError::invalid("Invalid GZCTF challenge identity"))?;
            external_segment(game)?;
            external_segment(cid)?;
            platform_request_header(
                state,
                connection,
                reqwest::Method::POST,
                &format!("/api/Game/{game}/Challenges/{cid}"),
                Some(json!({"flag":answer})),
                None,
            )
            .await?
        }
        "generic_browser" => {
            external_segment(external)?;
            let template = profile_path(connection, "submit_path", "")?;
            let path = template.replace("{challenge_key}", external);
            let flag_field = connection["site_profile"]["flag_field"]
                .as_str()
                .unwrap_or("flag");
            let challenge_field = connection["site_profile"]["challenge_field"]
                .as_str()
                .unwrap_or("");
            let mut payload = serde_json::Map::new();
            payload.insert(flag_field.to_owned(), json!(answer));
            if !challenge_field.is_empty() {
                payload.insert(challenge_field.to_owned(), json!(external));
            }
            let header = browser_csrf_token(state, connection).await?;
            platform_request_header(
                state,
                connection,
                reqwest::Method::POST,
                &path,
                Some(Value::Object(payload)),
                header
                    .as_ref()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            )
            .await?
        }
        _ => return Err(ApiError::unsupported("This platform cannot submit answers")),
    };
    match kind {
        "ctfd" => {
            let verdict = field(&body["data"], "status");
            let message = field(&body["data"], "message");
            let state = match verdict {
                "correct" => "correct",
                "incorrect" => "wrong",
                "partial" => "partial",
                "already_solved" => "duplicate_or_solved",
                "ratelimited" => "rate_limited",
                "authentication_required" => "auth_required",
                _ => "failed",
            };
            if state == "rate_limited" || (status == StatusCode::TOO_MANY_REQUESTS && state != "auth_required") {
                return Ok(submit_outcome("rate_limited", "", ctfd_retry_seconds(message).or(retry_after)));
            }
            if state == "auth_required" || status == StatusCode::UNAUTHORIZED {
                return Ok(submit_outcome("auth_required", "", None));
            }
            if status.is_server_error() {
                return Ok(submit_outcome("unknown", body.to_string(), None));
            }
            if !status.is_success() && !matches!(state, "wrong" | "rate_limited" | "auth_required")
            {
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "competition.platform_error",
                    format!("CTFd returned HTTP {}", status.as_u16()),
                ));
            }
            Ok(submit_outcome(state, verdict, None))
        }
        "rctf" => {
            let verdict = field(&body, "kind");
            let state = match verdict {
                "goodFlag" => "correct",
                "badFlag" => "wrong",
                "badAlreadySolvedChallenge" => "duplicate_or_solved",
                "badRateLimit" => "rate_limited",
                "badToken" => "auth_required",
                "badNotStarted" | "badEnded" => "transient_failure",
                _ => "unknown",
            };
            if state == "rate_limited" || (status == StatusCode::TOO_MANY_REQUESTS && state != "auth_required") {
                let precise = body["data"]["timeLeft"].as_f64().filter(|v| v.is_finite()).map(|v| (v / 1000.0).max(0.0));
                return Ok(submit_outcome("rate_limited", "", precise.or(retry_after)));
            }
            if state == "auth_required" || (status == StatusCode::UNAUTHORIZED && verdict.is_empty()) {
                return Ok(submit_outcome("auth_required", "", None));
            }
            if status.is_server_error() {
                return Ok(submit_outcome("unknown", body.to_string(), None));
            }
            let mut outcome=submit_outcome(state, verdict, None);
            if matches!(verdict,"badNotStarted"|"badEnded"){outcome.certainty=SubmissionCertainty::Rejected;}
            Ok(outcome)
        }
        "gzctf" => {
            if status == StatusCode::TOO_MANY_REQUESTS { return Ok(submit_outcome("rate_limited", "", retry_after)); }
            if status == StatusCode::UNAUTHORIZED { return Ok(submit_outcome("auth_required", "", None)); }
            if status.is_server_error() { return Ok(submit_outcome("unknown", body.to_string(), None)); }
            if !status.is_success() {
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "competition.platform_error",
                    format!("GZCTF returned HTTP {}", status.as_u16()),
                ));
            }
            let submit_id = body.as_u64().map(|id| id.to_string())
                .or_else(|| body.as_str().filter(|id| !id.is_empty()).map(str::to_owned))
                .or_else(|| body["id"].as_u64().map(|id| id.to_string()))
                .or_else(|| body["id"].as_str().filter(|id| !id.is_empty()).map(str::to_owned))
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        "competition.platform_response",
                        "GZCTF did not return a submission id",
                    )
                })?;
            Ok(submit_outcome("pending", submit_id, None))
        }
        "generic_browser" => {
            if matches!(status,StatusCode::UNAUTHORIZED|StatusCode::FORBIDDEN){return Ok(submit_outcome("auth_required","",None));}
            if status == StatusCode::TOO_MANY_REQUESTS {
                return Ok(submit_outcome("rate_limited", "", retry_after));
            }
            if !status.is_success() {
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "competition.platform_error",
                    format!("Browser API returned HTTP {}", status.as_u16()),
                ));
            }
            let blob = body.to_string().to_ascii_lowercase();
            let incorrect = connection["site_profile"]["verdict_incorrect"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![json!("incorrect"), json!("wrong"), json!("invalid")]);
            let correct = connection["site_profile"]["verdict_correct"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![json!("correct"), json!("success"), json!("accepted")]);
            let verdict = if incorrect.iter().any(|v| {
                v.as_str()
                    .is_some_and(|s| blob.contains(&s.to_ascii_lowercase()))
            }) {
                "wrong"
            } else if correct.iter().any(|v| {
                v.as_str()
                    .is_some_and(|s| blob.contains(&s.to_ascii_lowercase()))
            }) {
                "correct"
            } else {
                "unknown"
            };
            let remote_id = body["submission_id"]
                .as_str()
                .or_else(|| body["id"].as_str())
                .unwrap_or("")
                .to_owned();
            Ok(submit_outcome(verdict, remote_id, None))
        }
        _ => unreachable!(),
    }
}

async fn submit_existing_candidate(
    state: &Arc<AppState>,
    competition_id: &str,
    kind: &str,
    command_id: &str,
    candidate: Value,
) -> ApiResult<Value> {
    let candidate_id = field(&candidate, "candidate_id").to_owned();
    let challenge_id = field(&candidate, "competition_challenge_id").to_owned();
    let challenge = state
        .store
        .get(CHALLENGES, &challenge_id)
        .map_err(ApiError::internal)?
        .filter(|v| v["competition_id"] == competition_id)
        .ok_or_else(|| ApiError::missing("challenge"))?;
    if challenge["remote_state"] == "solved_remote" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "competition.submission.already_solved",
            "Challenge is already solved on the platform",
        ));
    }
    let competition = live_competition(state, competition_id)?;
    let connection_id = field(&competition, "connection_id");
    let connection = live_connection(state, connection_id)?;
    let previous = scoped(state, SUBMISSIONS, competition_id)?;
    let attempts = previous
        .iter()
        .filter(|v| v["candidate_id"] == candidate_id)
        .count() as u64;
    submission_outbox::stage(state,competition_id,kind,command_id,
        &candidate,&challenge,&connection,attempts+1)
}

async fn submit_command(
    state: &Arc<AppState>,
    competition_id: &str,
    kind: &str,
    command_id: &str,
    payload: &Value,
) -> ApiResult<Value> {
    let challenge_id = field(payload, "challenge_id");
    let answer = payload["answer"].as_str().unwrap_or("");
    if answer.trim().is_empty() {
        return Err(ApiError::invalid("Answer is required"));
    }
    let candidate = register_candidate(
        state,
        competition_id,
        challenge_id,
        answer,
        field(payload, "source_run_id"),
        payload["answer_slot"].as_u64().unwrap_or(1),
        "operator_submitted",
        "approved",
        command_id,
    )?;
    let receipt = submit_existing_candidate(state, competition_id, kind, command_id, candidate).await?;
    Ok(receipt)
}

fn register_candidate(
    state: &AppState,
    competition_id: &str,
    challenge_id: &str,
    answer: &str,
    source_run_id: &str,
    answer_slot: u64,
    gate_verdict: &str,
    status: &str,
    command_id: &str,
) -> ApiResult<Value> {
    let answer_slot = answer_slot.max(1);
    let digest = format!("{:x}", Sha256::digest(answer.as_bytes()));
    let lock_key = format!("{:x}",Sha256::digest(
        format!("{competition_id}\0{challenge_id}\0{answer_slot}\0{digest}").as_bytes()));
    let lock_path = native_chat::state_dir(state).join("_platform").join("candidate-locks")
        .join(format!("{lock_key}.lock"));
    fs::create_dir_all(lock_path.parent().unwrap()).map_err(ApiError::internal)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(lock_path).map_err(ApiError::internal)?;
    fs2::FileExt::lock_exclusive(&lock).map_err(ApiError::internal)?;
    let challenge = state.store.get(CHALLENGES,challenge_id)
        .map_err(ApiError::internal)?
        .filter(|row| row["competition_id"] == competition_id)
        .ok_or_else(|| ApiError::missing("challenge"))?;
    if challenge["state"] == "solved" {
        return Err(ApiError::new(StatusCode::CONFLICT,"competition.submission.already_solved",
            "Challenge is already solved"));
    }
    if let Some(existing) = scoped(state,CANDIDATES,competition_id)?.into_iter().find(|candidate| {
        candidate["competition_challenge_id"] == challenge_id
            && candidate["answer_slot"] == answer_slot
            && candidate["digest"] == digest
    }) { return Ok(existing); }
    let competition = live_competition(state, competition_id)?;
    let connection_id = field(&competition, "connection_id");
    let candidate_id = id("cand");
    let key = format!("candidate-{}", uuid::Uuid::new_v4().simple());
    let secret = write_secret(state, connection_id, &key, answer, "", 1)?;
    let source_execution_generation = if source_run_id.is_empty() { 0 } else {
        scoped(state, BINDINGS, competition_id)?.into_iter()
            .filter(|binding| binding["competition_challenge_id"] == challenge_id
                && binding["run_id"] == source_run_id)
            .filter_map(|binding| binding["execution_generation"].as_u64())
            .max().unwrap_or(0)
    };
    let candidate = json!({"candidate_id":candidate_id,"competition_id":competition_id,"competition_challenge_id":challenge_id,
        "answer_slot":answer_slot,"digest":digest,"source_run_id":if source_run_id.is_empty() { Value::Null } else { json!(source_run_id) },
        "source_ref":if source_run_id.is_empty() { "operator" } else { "run_event" },"gate_verdict":gate_verdict,
        "source_execution_generation":source_execution_generation,
        "state":status,"answer_ref":secret["reference"],"created_at":now(),"updated_at":now()});
    let event = leases::planned_event(competition_id,"candidate",&candidate_id,
        "competition.submission.candidate_registered",command_id,
        json!({"candidate_id":candidate_id,"challenge_id":challenge_id,"digest":digest,
            "source_run_id":candidate["source_run_id"],"execution_generation":source_execution_generation}));
    let mut expected = vec![(CANDIDATES,candidate_id.as_str(),None)];
    let mut records = vec![(CANDIDATES,candidate_id.as_str(),&candidate)];
    let mut events = vec![event];
    let mut next_challenge = challenge.clone();
    if !source_run_id.is_empty() && challenge["state"] == "running" {
        next_challenge["state"] = json!("candidate_found");
        next_challenge["updated_at"] = json!(now());
        expected.push((CHALLENGES,challenge_id,Some(&challenge)));
        records.push((CHALLENGES,challenge_id,&next_challenge));
        events.push(leases::planned_event(competition_id,"challenge",challenge_id,
            "competition.challenge.state_changed",command_id,
            json!({"challenge_id":challenge_id,"from":"running","to":"candidate_found"})));
    }
    if !put_records_with_events(state,competition_id,&expected,&records,&events)? {
        return Err(ApiError::new(StatusCode::CONFLICT,"competition.submission.concurrent_update",
            "Candidate or challenge changed concurrently"));
    }
    Ok(candidate)
}

async fn retry_or_approve(
    state: &Arc<AppState>,
    competition_id: &str,
    kind: &str,
    command_id: &str,
    payload: &Value,
) -> ApiResult<Value> {
    let candidate = if kind == "platform_submission.approve" {
        let candidate_id = field(payload, "candidate_id");
        let candidate = state
            .store
            .get(CANDIDATES, candidate_id)
            .map_err(ApiError::internal)?
            .filter(|v| v["competition_id"] == competition_id)
            .ok_or_else(|| ApiError::missing("candidate"))?;
        if !matches!(field(&candidate, "state"), "awaiting_approval" | "approved") {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "competition.submission.illegal_approval",
                "Candidate is not awaiting approval",
            ));
        }
        candidate
    } else {
        let submission_id = field(payload, "submission_id");
        let submission = state
            .store
            .get(SUBMISSIONS, submission_id)
            .map_err(ApiError::internal)?
            .filter(|v| v["competition_id"] == competition_id)
            .ok_or_else(|| ApiError::missing("submission"))?;
        if !matches!(
            field(&submission, "state"),
            "failed" | "rate_limited" | "pending" | "auth_required"
        ) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "competition.submission.illegal_retry",
                "Submission cannot be retried from its current state",
            ));
        }
        let candidate_id = field(&submission, "candidate_id");
        state
            .store
            .get(CANDIDATES, candidate_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::missing("candidate"))?
    };
    submit_existing_candidate(state, competition_id, kind, command_id, candidate).await
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

    #[tokio::test]
    async fn planned_connection_update_commits_receipt_claim_and_row_together() {
        let (dir,state) = test_state();
        let connection_id = "connection-update-atomic";
        let previous = json!({"connection_id":connection_id,"platform_kind":"ctfd",
            "canonical_base_url":"http://example.test","account_key":"before",
            "historical":false,"archived":false,"capabilities":{"old":true}});
        state.store.put(CONNECTIONS,connection_id,&previous).unwrap();
        let admission = json!({"claim":{"command_id":"cmd-connection-update",
            "request_fingerprint":"fingerprint","state":"accepted"},
            "key_id":"key-connection-update","keyed":{"command_id":"cmd-connection-update",
            "semantic_fingerprint":"semantic"},"semantic_fingerprint":"semantic"});
        let body = json!({"command_id":"cmd-connection-update","account_key":"after",
            "_planned_admission":admission});
        let db = rusqlite::Connection::open(dir.path().join("muteki.sqlite")).unwrap();
        db.execute_batch("CREATE TRIGGER fail_connection_update BEFORE INSERT ON documents
            WHEN NEW.collection='platform_commands' AND NEW.id='cmd-connection-update'
            BEGIN SELECT RAISE(ABORT,'receipt insert failed'); END;").unwrap();
        assert!(patch_connection(State(state.clone()),Path(connection_id.to_owned()),Json(body.clone())).await.is_err());
        assert_eq!(state.store.get(CONNECTIONS,connection_id).unwrap(),Some(previous));
        assert!(state.store.get("platform_command_claims","cmd-connection-update").unwrap().is_none());
        assert!(state.store.get("platform_idempotency_keys","key-connection-update").unwrap().is_none());
        db.execute_batch("DROP TRIGGER fail_connection_update;").unwrap();
        let receipt = patch_connection(State(state.clone()),Path(connection_id.to_owned()),Json(body.clone()))
            .await.unwrap().0["receipt"].clone();
        assert_eq!(receipt["state"],"completed");
        assert_eq!(state.store.get(CONNECTIONS,connection_id).unwrap().unwrap()["account_key"],"after");
        assert!(state.store.get("platform_command_claims","cmd-connection-update").unwrap().is_some());
        assert!(state.store.get("platform_idempotency_keys","key-connection-update").unwrap().is_some());
        assert_eq!(patch_connection(State(state.clone()),Path(connection_id.to_owned()),Json(body))
            .await.unwrap().0["receipt"]["receipt_id"],receipt["receipt_id"]);
    }

    #[tokio::test]
    async fn planned_competition_update_rolls_back_on_event_failure() {
        let (dir,state) = test_state();
        let competition_id = "competition-update-atomic";
        let previous = json!({"competition_id":competition_id,"title":"Before",
            "historical":false,"archived":false});
        state.store.put(COMPETITIONS,competition_id,&previous).unwrap();
        let body = json!({"command_id":"cmd-competition-update","title":"After",
            "_planned_admission":{"claim":{"command_id":"cmd-competition-update",
                "request_fingerprint":"fingerprint","state":"accepted"},
                "key_id":"key-competition-update",
                "keyed":{"command_id":"cmd-competition-update","semantic_fingerprint":"semantic"},
                "semantic_fingerprint":"semantic"}});
        let db = rusqlite::Connection::open(dir.path().join("muteki.sqlite")).unwrap();
        db.execute_batch("CREATE TRIGGER fail_competition_update BEFORE INSERT ON documents
            WHEN NEW.collection='competition_events_live'
            BEGIN SELECT RAISE(ABORT,'event insert failed'); END;").unwrap();
        assert!(patch_competition(State(state.clone()),Path(competition_id.to_owned()),Json(body.clone())).await.is_err());
        assert_eq!(state.store.get(COMPETITIONS,competition_id).unwrap(),Some(previous));
        assert!(state.store.get(COMMANDS,"cmd-competition-update").unwrap().is_none());
        assert!(state.store.get("platform_command_claims","cmd-competition-update").unwrap().is_none());
        assert!(state.store.get("platform_idempotency_keys","key-competition-update").unwrap().is_none());
        db.execute_batch("DROP TRIGGER fail_competition_update;").unwrap();
        let receipt = patch_competition(State(state.clone()),Path(competition_id.to_owned()),Json(body.clone()))
            .await.unwrap().0["receipt"].clone();
        assert_eq!(receipt["state"],"completed");
        assert_eq!(state.store.get(COMPETITIONS,competition_id).unwrap().unwrap()["title"],"After");
        assert_eq!(scoped(&state,EVENTS,competition_id).unwrap().len(),1);
        assert_eq!(patch_competition(State(state.clone()),Path(competition_id.to_owned()),Json(body))
            .await.unwrap().0["receipt"]["receipt_id"],receipt["receipt_id"]);
    }

    #[tokio::test]
    async fn competition_updates_with_same_version_fence_across_sqlite_connections() {
        let (dir,state) = test_state();
        let second = Arc::new(AppState::new(crate::store::Store::open(
            dir.path().join("muteki.sqlite")).unwrap()));
        let competition_id = "competition-version-fence";
        state.store.put(COMPETITIONS,competition_id,&json!({"competition_id":competition_id,
            "title":"Before","historical":false,"archived":false})).unwrap();
        let first = patch_competition(State(state.clone()),Path(competition_id.to_owned()),
            Json(json!({"command_id":"version-first","title":"First","expected_version":0})));
        let second = patch_competition(State(second),Path(competition_id.to_owned()),
            Json(json!({"command_id":"version-second","title":"Second","expected_version":0})));
        let (first,second) = tokio::join!(first,second);
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()),1);
        assert_eq!(usize::from(first.is_err()) + usize::from(second.is_err()),1);
        assert_eq!(scoped(&state,EVENTS,competition_id).unwrap().len(),1);
        assert_eq!(state.store.list(COMMANDS).unwrap().iter().filter(|row|
            row["request"]["command_type"]=="competition.update").count(),1);
    }

    #[test]
    fn gate_recovery_restores_a_truncated_flag_event_once() {
        let (dir,state) = test_state();
        let run_id = "run-gate-tail";
        let flag = "flag{graph-backed}";
        state.store.put(CONNECTIONS,"connection-gate",&json!({
            "connection_id":"connection-gate","platform_kind":"ctfd",
            "status":"active"})).unwrap();
        state.store.put(COMPETITIONS,"competition-gate",&json!({
            "competition_id":"competition-gate","connection_id":"connection-gate",
            "archived":false,"historical":false})).unwrap();
        state.store.put(POLICIES,"competition-gate",&default_policy("competition-gate")).unwrap();
        state.store.put(CHALLENGES,"challenge-gate",&json!({
            "challenge_id":"challenge-gate","competition_id":"competition-gate",
            "state":"running","tombstoned":false})).unwrap();
        state.store.put(BINDINGS,"binding-gate",&json!({
            "binding_id":"binding-gate","competition_id":"competition-gate",
            "competition_challenge_id":"challenge-gate","run_id":run_id,
            "execution_generation":1,"state":"running"})).unwrap();
        state.store.put("runs",run_id,&json!({"run_id":run_id,"execution_generation":1,
            "running":false,"flags":[flag],"flag":flag,"solved":true})).unwrap();
        super::super::graph::SharedGraph::open(&state,run_id).unwrap().flag_found("cli-pi#1",flag);
        let events_path = super::super::session_dir(&state,run_id).join("events.jsonl");
        fs::create_dir_all(events_path.parent().unwrap()).unwrap();
        fs::write(&events_path,b"{\"event_type\":\"blackboard.delta\"").unwrap();
        gate_recovery::reconcile(&state).unwrap();
        let candidates = scoped(&state,CANDIDATES,"competition-gate").unwrap();
        assert_eq!(candidates.len(),1);
        assert_eq!(candidates[0]["state"],"awaiting_approval");
        assert_eq!(candidates[0]["source_ref"],"run_event");
        assert_eq!(state.store.get(CHALLENGES,"challenge-gate").unwrap().unwrap()["state"],"candidate_found");
        let valid = super::super::read_jsonl_events(&state,run_id);
        assert_eq!(valid.iter().filter(|event| event["event_type"] == "blackboard.delta"
            && event["payload"]["kind"] == "flag_found").count(),1);
        assert_eq!(valid.last().unwrap()["payload"]["recovered"],true);
        let candidate_id = field(&candidates[0],"candidate_id").to_owned();
        drop(state);
        let second = Arc::new(AppState::new(crate::store::Store::open(
            dir.path().join("muteki.sqlite")).unwrap()));
        gate_recovery::reconcile(&second).unwrap();
        let after = scoped(&second,CANDIDATES,"competition-gate").unwrap();
        assert_eq!(after.len(),1);
        assert_eq!(field(&after[0],"candidate_id"),candidate_id);
        assert_eq!(super::super::read_jsonl_events(&second,run_id).iter()
            .filter(|event| event["event_type"] == "blackboard.delta"
                && event["payload"]["kind"] == "flag_found").count(),1);
    }

    #[test]
    fn gate_recovery_ignores_invalidated_and_prior_generation_flags() {
        let (_dir,state) = test_state();
        state.store.put(COMPETITIONS,"competition-gate-stale",&json!({
            "competition_id":"competition-gate-stale","archived":false,"historical":false})).unwrap();
        state.store.put(CHALLENGES,"challenge-gate-stale",&json!({
            "challenge_id":"challenge-gate-stale","competition_id":"competition-gate-stale",
            "state":"running"})).unwrap();
        state.store.put(BINDINGS,"binding-gate-stale",&json!({
            "binding_id":"binding-gate-stale","competition_id":"competition-gate-stale",
            "competition_challenge_id":"challenge-gate-stale","run_id":"run-gate-stale",
            "execution_generation":2,"state":"running"})).unwrap();
        state.store.put("runs","run-gate-stale",&json!({
            "run_id":"run-gate-stale","execution_generation":2,
            "flags":["flag{invalidated}","flag{old-generation}"]})).unwrap();
        let graph = super::super::graph::SharedGraph::open(&state,"run-gate-stale").unwrap();
        graph.flag_found("cli-pi#1","flag{invalidated}");
        graph.invalidate_flag("competition","flag{invalidated}","remote wrong");
        graph.flag_found("cli-pi#1","flag{old-generation}");
        gate_recovery::reconcile(&state).unwrap();
        assert!(scoped(&state,CANDIDATES,"competition-gate-stale").unwrap().is_empty());
        assert!(super::super::read_jsonl_events(&state,"run-gate-stale").is_empty());
    }

    #[test]
    fn autonomous_candidate_waits_for_source_admission_conditions() {
        let (_dir,state) = test_state();
        let competition_id = "competition-admission";
        let challenge_id = "challenge-admission";
        state.store.put(CONNECTIONS,"connection-admission",&json!({
            "connection_id":"connection-admission","platform_kind":"ctfd",
            "status":"active","capabilities":{"automation_allowed":false}})).unwrap();
        state.store.put(COMPETITIONS,competition_id,&json!({
            "competition_id":competition_id,"connection_id":"connection-admission"})).unwrap();
        let mut policy = default_policy(competition_id);
        policy["automation_mode"] = json!("autonomous");
        policy["submission_cooldown_seconds"] = json!(0.0);
        state.store.put(POLICIES,competition_id,&policy).unwrap();
        state.store.put(CHALLENGES,challenge_id,&json!({
            "challenge_id":challenge_id,"competition_id":competition_id,
            "state":"running","remote_state":"open"})).unwrap();
        let candidate = register_candidate(&state,competition_id,challenge_id,"flag{admission}",
            "run-admission",1,"model-submitted:test","candidate","").unwrap();
        assert!(!submission_outbox::admit_autonomous_candidate(&state,competition_id,&candidate).unwrap());
        let mut connection = state.store.get(CONNECTIONS,"connection-admission").unwrap().unwrap();
        state.store.delete(CONNECTIONS,"connection-admission").unwrap();
        assert!(!submission_outbox::admit_autonomous_candidate(&state,competition_id,&candidate).unwrap());
        state.store.put(CONNECTIONS,"connection-admission",&connection).unwrap();
        connection["capabilities"]["automation_allowed"] = json!(true);
        connection["capabilities"]["submission_cooldown_until"] = json!((chrono::Utc::now()
            + chrono::Duration::hours(1)).to_rfc3339());
        state.store.put(CONNECTIONS,"connection-admission",&connection).unwrap();
        assert!(!submission_outbox::admit_autonomous_candidate(&state,competition_id,&candidate).unwrap());
        connection["capabilities"]["submission_cooldown_until"] = Value::Null;
        state.store.put(CONNECTIONS,"connection-admission",&connection).unwrap();
        state.store.put(BUDGETS,&format!("{competition_id}:submissions"),
            &json!({"competition_id":competition_id,"kind":"submissions","limit":1.0,"used":1})).unwrap();
        assert!(!submission_outbox::admit_autonomous_candidate(&state,competition_id,&candidate).unwrap());
        assert!(scoped(&state,SUBMISSIONS,competition_id).unwrap().is_empty());
        assert!(state.store.list("competition_submission_outbox_live").unwrap().is_empty());
        policy["budget_limits"]["submissions"] = json!(2.0);
        state.store.put(POLICIES,competition_id,&policy).unwrap();
        assert!(!submission_outbox::admit_autonomous_candidate(&state,competition_id,&candidate).unwrap());
        let mut budget = state.store.get(BUDGETS,&format!("{competition_id}:submissions")).unwrap().unwrap();
        budget["limit"] = json!(2.0);
        state.store.put(BUDGETS,&format!("{competition_id}:submissions"),&budget).unwrap();
        assert!(submission_outbox::admit_autonomous_candidate(&state,competition_id,&candidate).unwrap());
        assert!(!submission_outbox::admit_autonomous_candidate(&state,competition_id,&candidate).unwrap());
        assert_eq!(scoped(&state,SUBMISSIONS,competition_id).unwrap().len(),1);
        assert_eq!(state.store.list("competition_submission_outbox_live").unwrap().len(),1);
    }

    #[test]
    fn autonomous_submission_admission_is_atomic() {
        let (_dir, state) = test_state();
        let competition_id = "competition-atomic-admission";
        let candidate_id = "candidate-atomic-admission";
        let challenge_id = "challenge-atomic-admission";
        let command_id = "competition-gate:candidate-atomic-admission";
        let candidate = json!({"candidate_id":candidate_id,"competition_id":competition_id,
            "competition_challenge_id":challenge_id,"state":"candidate","answer_slot":1,
            "digest":"answer-digest","answer_ref":"secret-ref"});
        let challenge = json!({"challenge_id":challenge_id,"competition_id":competition_id,
            "state":"candidate_found"});
        let connection = json!({"platform_kind":"ctfd"});
        state.store.put(CANDIDATES,candidate_id,&candidate).unwrap();
        state.store.put(CHALLENGES,challenge_id,&challenge).unwrap();
        let mut changed = candidate.clone();
        changed["state"] = json!("rejected");
        state.store.put(CANDIDATES,candidate_id,&changed).unwrap();

        let error = submission_outbox::stage(&state,competition_id,
            "competition.submission.submit",command_id,&candidate,&challenge,&connection,1)
            .unwrap_err();
        assert_eq!(error.code,"competition.submission.concurrent_update");
        assert!(state.store.get(COMMANDS,command_id).unwrap().is_none());
        assert!(scoped(&state,EVENTS,competition_id).unwrap().is_empty());
        assert!(scoped(&state,SUBMISSIONS,competition_id).unwrap().is_empty());
        assert!(state.store.list("competition_submission_outbox_live").unwrap().is_empty());

        state.store.put(CANDIDATES,candidate_id,&candidate).unwrap();
        let receipt = submission_outbox::stage(&state,competition_id,
            "competition.submission.submit",command_id,&candidate,&challenge,&connection,1)
            .unwrap();
        assert_eq!(receipt["state"],"waiting");
        let submission_id = field(&receipt["output"],"submission_id");
        let outbox_id = receipt["output"]["outbox_ids"][0].as_str().unwrap();
        let effect_id = receipt["effect_ids"][0].as_str().unwrap();
        assert_eq!(state.store.get(SUBMISSIONS,submission_id).unwrap().unwrap()["state"],"queued");
        assert_eq!(state.store.get("competition_submission_outbox_live",outbox_id)
            .unwrap().unwrap()["status"],"pending");
        assert_eq!(state.store.get("platform_effect_receipts",effect_id)
            .unwrap().unwrap()["state"],"waiting");
        assert_eq!(scoped(&state,EVENTS,competition_id).unwrap().len(),2);
        let replay = submission_outbox::stage(&state,competition_id,
            "competition.submission.submit",command_id,&candidate,&challenge,&connection,1)
            .unwrap();
        assert_eq!(replay["deduplicated"],true);
        assert_eq!(replay["receipt_id"],receipt["receipt_id"]);
        assert_eq!(scoped(&state,EVENTS,competition_id).unwrap().len(),2);
    }

    #[test]
    fn verdict_record_and_event_share_one_sqlite_commit() {
        let (_dir, state) = test_state();
        let before = json!({"state":"submitting"});
        let after = json!({"state":"correct"});
        state.store.put(SUBMISSIONS,"submission-atomic",&before).unwrap();
        let event = leases::planned_event("competition-atomic","platform_submission",
            "submission-atomic","competition.platform_submission.state_changed","",
            json!({"submission_id":"submission-atomic","from":"submitting","to":"correct"}));
        assert!(!put_records_with_events(&state,"competition-atomic",
            &[(SUBMISSIONS,"submission-atomic",Some(&json!({"state":"queued"})))],
            &[(SUBMISSIONS,"submission-atomic",&after)],&[event.clone()]).unwrap());
        assert_eq!(state.store.get(SUBMISSIONS,"submission-atomic").unwrap(),Some(before.clone()));
        assert!(scoped(&state,EVENTS,"competition-atomic").unwrap().is_empty());
        assert!(put_records_with_events(&state,"competition-atomic",
            &[(SUBMISSIONS,"submission-atomic",Some(&before))],
            &[(SUBMISSIONS,"submission-atomic",&after)],&[event]).unwrap());
        assert_eq!(state.store.get(SUBMISSIONS,"submission-atomic").unwrap(),Some(after));
        let saved = scoped(&state,EVENTS,"competition-atomic").unwrap();
        assert_eq!(saved.len(),1);
        assert_eq!(saved[0]["payload"]["to"],"correct");
    }

    #[tokio::test]
    async fn two_reconcilers_restore_one_missing_verdict_event() {
        let (dir, state) = test_state();
        let second = Arc::new(AppState::new(crate::store::Store::open(
            dir.path().join("muteki.sqlite")).unwrap()));
        state.store.put(SUBMISSIONS,"submission-recovery",&json!({
            "submission_id":"submission-recovery","competition_id":"competition-recovery",
            "competition_challenge_id":"challenge-recovery","candidate_id":"candidate-recovery",
            "state":"wrong","digest":"digest-only"})).unwrap();
        let (left,right) = tokio::join!(
            submission_outbox::reconcile_verdict_events(&state,"competition-recovery"),
            submission_outbox::reconcile_verdict_events(&second,"competition-recovery"),
        );
        left.unwrap();
        right.unwrap();
        let saved = scoped(&state,EVENTS,"competition-recovery").unwrap();
        assert_eq!(saved.len(),1);
        assert_eq!(saved[0]["payload"]["to"],"wrong");
        assert_eq!(saved[0]["payload"]["recovered"],true);
    }

    #[tokio::test]
    async fn gzctf_submit_returns_remote_id_before_status_poll() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let polls = Arc::new(AtomicUsize::new(0));
        let counted = polls.clone();
        let app = Router::new()
            .route("/api/Game/game/Challenges/chal",post(|| async { Json(json!("42")) }))
            .route("/api/Game/game/Challenges/chal/Status/42",get(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1,Ordering::SeqCst);
                    (StatusCode::SERVICE_UNAVAILABLE,Json(json!({"error":"poll unavailable"})))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}",listener.local_addr().unwrap());
        let server = tokio::spawn(axum::serve(listener,app).into_future());
        let (_dir,state) = test_state();
        let Json(created) = create_connection(State(state.clone()),Json(json!({
            "platform_kind":"gzctf","base_url":base_url,"credential":"synthetic-cookie",
            "command_id":"gzctf-pending-connection"}))).await.unwrap();
        let connection_id = created["receipt"]["output"]["connection_id"].as_str().unwrap();
        let connection = live_connection(&state,connection_id).unwrap();
        let outcome = platform_submit(&state,&connection,
            &json!({"external_challenge_id":"game/chal"}),"flag{synthetic}","synthetic-submission").await.unwrap();
        assert_eq!(outcome.state,"pending");
        assert_eq!(outcome.remote_receipt,"42");
        assert_eq!(polls.load(Ordering::SeqCst),0);
        server.abort();
    }

    #[tokio::test]
    async fn correct_verdict_recovery_aligns_multiflag_projection_once() {
        let (dir,state) = test_state();
        let second = Arc::new(AppState::new(crate::store::Store::open(
            dir.path().join("muteki.sqlite")).unwrap()));
        state.store.put(REVISIONS,"revision-recover",&json!({
            "revision_id":"revision-recover","expected_flags":2})).unwrap();
        state.store.put(CHALLENGES,"challenge-recover",&json!({
            "challenge_id":"challenge-recover","competition_id":"competition-recover",
            "current_revision_id":"revision-recover","state":"submitting","remote_state":"open"})).unwrap();
        for (sid,cid,digest) in [("sub-one","candidate-one","digest-one"),
            ("sub-two","candidate-two","digest-two")] {
            state.store.put(SUBMISSIONS,sid,&json!({"submission_id":sid,
                "competition_id":"competition-recover","competition_challenge_id":"challenge-recover",
                "candidate_id":cid,"state":"correct","digest":digest,
                "remote_receipt":"correct"})).unwrap();
            state.store.put(CANDIDATES,cid,&json!({"candidate_id":cid,
                "competition_id":"competition-recover","state":"approved"})).unwrap();
        }
        state.store.put("competition_submission_outbox_live","outbox-two",&json!({
            "outbox_id":"outbox-two","competition_id":"competition-recover",
            "submission_id":"sub-two","command_id":"command-two","effect_id":"effect-two",
            "status":"processing","attempts":1})).unwrap();
        state.store.put("platform_effect_receipts","effect-two",&json!({
            "effect_id":"effect-two","state":"running","output":{},"completed_at":null})).unwrap();
        state.store.put(COMMANDS,"command-two",&json!({"response":{"receipt":{
            "state":"running","completed_at":null,
            "output":{"submission_id":"sub-two","state":"unknown"}}}})).unwrap();
        submission_outbox::reconcile_startup(&state).await.unwrap();
        assert_eq!(state.store.get(SUBMISSIONS,"sub-two").unwrap().unwrap()["state"],"correct");
        assert_eq!(state.store.get("competition_submission_outbox_live","outbox-two")
            .unwrap().unwrap()["status"],"delivered");
        let (left,right) = tokio::join!(
            submission_outbox::maintenance(&state,"competition-recover"),
            submission_outbox::maintenance(&second,"competition-recover"));
        left.unwrap();right.unwrap();
        assert_eq!(state.store.get(CHALLENGES,"challenge-recover").unwrap().unwrap()["state"],"solved");
        assert_eq!(state.store.get(CHALLENGES,"challenge-recover").unwrap().unwrap()["remote_state"],"solved_remote");
        assert_eq!(state.store.get(CANDIDATES,"candidate-one").unwrap().unwrap()["state"],"submitted");
        assert_eq!(state.store.get(CANDIDATES,"candidate-two").unwrap().unwrap()["state"],"submitted");
        assert_eq!(state.store.get("platform_effect_receipts","effect-two").unwrap().unwrap()["output"]["state"],"correct");
        assert_eq!(state.store.get(COMMANDS,"command-two").unwrap().unwrap()["response"]["receipt"]["output"]["state"],"correct");
        let events = scoped(&state,EVENTS,"competition-recover").unwrap();
        assert_eq!(events.iter().filter(|event| event["event_type"] == "competition.challenge.state_changed"
            && event["payload"]["to"] == "solved").count(),1);
        let latest_verdict = events.iter()
            .filter(|event| event["event_type"] == "competition.platform_submission.state_changed"
                && event["payload"]["to"] == "correct")
            .filter_map(|event| event["seq"].as_u64()).max().unwrap();
        let solved = events.iter().find(|event| event["event_type"] == "competition.challenge.state_changed"
            && event["payload"]["to"] == "solved").unwrap();
        assert!(latest_verdict < solved["seq"].as_u64().unwrap());
    }

    #[tokio::test]
    async fn solved_binding_does_not_stop_a_newer_run_generation() {
        let (_dir,state) = test_state();
        state.store.put("runs","run-stale-correct",&json!({"run_id":"run-stale-correct",
            "execution_generation":2,"running":true,"status":"running"})).unwrap();
        state.store.put(BINDINGS,"binding-stale-correct",&json!({
            "binding_id":"binding-stale-correct","competition_id":"competition-stale-correct",
            "competition_challenge_id":"challenge-stale-correct","run_id":"run-stale-correct",
            "execution_generation":1,"state":"running"})).unwrap();
        submission_outbox::settle_solved_binding(&state,"competition-stale-correct",
            "challenge-stale-correct").await.unwrap();
        assert_eq!(state.store.get("runs","run-stale-correct").unwrap().unwrap()["running"],true);
        let binding = state.store.get(BINDINGS,"binding-stale-correct").unwrap().unwrap();
        assert_eq!(binding["state"],"running");
        assert!(field(&binding,"gateway_stop_error").contains("stale run binding"));
    }

    #[tokio::test]
    async fn prior_execution_watcher_cannot_finish_reopened_binding() {
        let (_dir, state) = test_state();
        state.store.put("runs", "run-generation", &json!({
            "run_id":"run-generation","running":false,"execution_generation":1,
            "outcome_detail":"competition_binding_pause"})).unwrap();
        state.store.put(BINDINGS, "binding-generation", &json!({
            "binding_id":"binding-generation","competition_id":"competition-generation",
            "competition_challenge_id":"challenge-generation","run_id":"run-generation",
            "execution_generation":1,"state":"running"})).unwrap();
        state.store.put(CHALLENGES, "challenge-generation", &json!({
            "challenge_id":"challenge-generation","competition_id":"competition-generation",
            "state":"running"})).unwrap();
        spawn_run_watcher(state.clone(),"competition-generation".into(),
            "challenge-generation".into(),"run-generation".into(),
            "binding-generation".into(),1);
        let mut run = state.store.get("runs","run-generation").unwrap().unwrap();
        run["execution_generation"] = json!(2);
        run["running"] = json!(true);
        state.store.put("runs","run-generation",&run).unwrap();
        let mut binding = state.store.get(BINDINGS,"binding-generation").unwrap().unwrap();
        binding["execution_generation"] = json!(2);
        state.store.put(BINDINGS,"binding-generation",&binding).unwrap();
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(state.store.get(BINDINGS,"binding-generation").unwrap().unwrap()["state"],"running");
        assert_eq!(state.store.get(CHALLENGES,"challenge-generation").unwrap().unwrap()["state"],"running");
    }

    #[tokio::test]
    async fn tsec_dispatch_reuses_active_binding_and_run_identity() {
        let (_dir, state) = test_state();
        state.store.put(CONNECTIONS, "pconn-test", &json!({"connection_id":"pconn-test","platform_kind":"ctfd","status":"active","capabilities":{}})).unwrap();
        state.store.put(COMPETITIONS, "comp-test", &json!({"competition_id":"comp-test","connection_id":"pconn-test","scheduler_state":"running","created_at":now()})).unwrap();
        let mut policy = default_policy("comp-test");
        policy["policy_profile"] = json!("tsec_eval");
        state.store.put(POLICIES, "comp-test", &policy).unwrap();
        state.store.put(REVISIONS, "rev-test", &json!({"revision_id":"rev-test","competition_id":"comp-test","name":"Easy Web","category":"web","description":"Find the flag","points":100})).unwrap();
        state.store.put(CHALLENGES, "cch-test", &json!({"challenge_id":"cch-test","competition_id":"comp-test","external_challenge_id":"7","name":"Easy Web","category":"web","current_revision_id":"rev-test","remote_state":"open","state":"candidate_found","tombstoned":false})).unwrap();
        state.store.put(QUEUE, "cch-test", &json!({"competition_id":"comp-test","competition_challenge_id":"cch-test","state":"queued","priority":0.0,"enqueued_at":now(),"admission_decision":{"visit_count":1}})).unwrap();
        state.store.put("runs", "run-stable", &json!({"run_id":"run-stable","running":true,"execution_generation":1})).unwrap();
        state.store.put(BINDINGS, "bind-stable", &json!({"binding_id":"bind-stable","competition_id":"comp-test","competition_challenge_id":"cch-test","revision_id":"rev-test","run_id":"run-stable","execution_generation":1,"state":"running","updated_at":now()})).unwrap();

        dispatch_queued(state.clone(), "comp-test").await.unwrap();
        assert_eq!(state.store.get(QUEUE, "cch-test").unwrap().unwrap()["admission_decision"]["reason"], "already_running");
        assert_eq!(state.store.list(BINDINGS).unwrap().len(), 1);
        assert_eq!(state.store.list("runs").unwrap().len(), 1);
        assert_eq!(state.store.get(CHALLENGES, "cch-test").unwrap().unwrap()["state"], "candidate_found");

        let mut run = state.store.get("runs", "run-stable").unwrap().unwrap();
        run["running"] = json!(false);
        state.store.put("runs", "run-stable", &run).unwrap();
        let mut binding = state.store.get(BINDINGS, "bind-stable").unwrap().unwrap();
        binding["state"] = json!("paused");
        state.store.put(BINDINGS, "bind-stable", &binding).unwrap();
        dispatch_queued(state.clone(), "comp-test").await.unwrap();
        let queue = state.store.get(QUEUE, "cch-test").unwrap().unwrap();
        assert_eq!(queue["admission_decision"]["reason"], "run_preflight_failed");
        assert_eq!(state.store.list(BINDINGS).unwrap().len(), 1);
        assert_eq!(state.store.list("runs").unwrap().len(), 1);
        assert_eq!(state.store.get(BINDINGS, "bind-stable").unwrap().unwrap()["state"], "paused");
        assert_eq!(state.store.get("runs", "run-stable").unwrap().unwrap()["execution_generation"], 1);
        let failed = state.store.list(COMMANDS).unwrap().into_iter()
            .find(|row| row["request"]["command_type"] == "run_binding.resolve").unwrap();
        assert_eq!(failed["response"]["receipt"]["state"], "failed");
        assert_eq!(failed["response"]["receipt"]["output"]["state"], "paused");
    }

    #[test]
    fn secret_files_are_private_and_metadata_never_contains_values() {
        let (_dir, state) = test_state();
        let metadata =
            write_secret(&state, "pconn-test", "token", "top-secret-value", "", 1).unwrap();
        let reference = metadata["reference"].as_str().unwrap();
        assert_eq!(read_secret(&state, reference).unwrap(), "top-secret-value");
        assert!(!metadata.to_string().contains("top-secret-value"));
        let path = secret_path(&state, "pconn-test", "token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        remove_secret(&state, reference).unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn connection_create_updates_existing_credential_and_copies_secret_reference() {
        let (_dir, state) = test_state();
        let base = json!({"platform_kind":"ctfd","base_url":"https://example.invalid","account_key":"fixture"});
        let mut first = base.clone();
        first["credential"] = json!("first-token");
        first["command_id"] = json!("create-first");
        let created = create_connection(State(state.clone()), Json(first))
            .await
            .unwrap()
            .0;
        let connection_id = created["receipt"]["output"]["connection_id"]
            .as_str()
            .unwrap();
        state.store.put(crate::capabilities::GRANTS,"synthetic-grant",&json!({
            "grant_id":"synthetic-grant","revoked_at":null})).unwrap();
        state.store.put(crate::agent_runtime::ledger::COLLECTION,"synthetic-thread",&json!({
            "thread_id":"synthetic-thread","status":"running","epoch":state.chat.epoch()})).unwrap();
        let mut second = base.clone();
        second["credential"] = json!("second-token");
        second["command_id"] = json!("create-second");
        let repeated = create_connection(State(state.clone()), Json(second))
            .await
            .unwrap()
            .0;
        assert_eq!(repeated["receipt"]["output"]["created"], false);
        assert!(state.store.get(crate::capabilities::GRANTS,"synthetic-grant").unwrap().unwrap()["revoked_at"].is_string());
        assert_eq!(state.store.get(crate::agent_runtime::ledger::COLLECTION,"synthetic-thread").unwrap().unwrap()["status"],"ended");
        let connection = live_connection(&state, connection_id).unwrap();
        let resolved = crate::agent_runtime::credentials::env_refs_with_platform(&state,
            &json!({"SYNTHETIC_TOKEN":connection["credential_ref"]})).unwrap();
        assert_eq!(resolved[0].0,"SYNTHETIC_TOKEN");
        assert_eq!(resolved[0].1,"second-token");
        assert_eq!(
            read_secret(&state, field(&connection, "credential_ref")).unwrap(),
            "second-token"
        );
        let source = write_secret(&state, "pconn-source", "token", "copied-token", "", 1).unwrap();
        let mut third = base;
        third["account_key"] = json!("second-account");
        third["credential_ref"] = source["reference"].clone();
        third["command_id"] = json!("create-third");
        let copied = create_connection(State(state.clone()), Json(third))
            .await
            .unwrap()
            .0;
        let copy_id = copied["receipt"]["output"]["connection_id"]
            .as_str()
            .unwrap();
        let copy = live_connection(&state, copy_id).unwrap();
        assert_ne!(copy["credential_ref"], source["reference"]);
        assert_eq!(
            read_secret(&state, field(&copy, "credential_ref")).unwrap(),
            "copied-token"
        );
        assert!(!copy.to_string().contains("copied-token"));
    }

    #[tokio::test]
    async fn policy_profile_and_downgrade_emit_safe_events() {
        let (_dir, state) = test_state();
        state
            .store
            .put(
                CONNECTIONS,
                "pconn-test",
                &json!({"connection_id":"pconn-test","status":"active"}),
            )
            .unwrap();
        state.store.put(COMPETITIONS,"comp-test",&json!({"competition_id":"comp-test","connection_id":"pconn-test","scheduler_state":"stopped"})).unwrap();
        let receipt = execute_command(&state,"comp-test",&json!({"command_type":"policy.update","command_id":"policy-profile","payload":{"competition_id":"comp-test","policy_profile":"tsec_eval"}})).await.unwrap();
        assert_eq!(receipt["state"],"completed");
        assert_eq!(receipt["output"],json!({}));
        assert_eq!(state.store.get(POLICIES,"comp-test").unwrap().unwrap()["max_concurrent_runs"],3);
        scheduler::detect_mode_change(&state,"comp-test","autonomous").unwrap();
        execute_command(&state,"comp-test",&json!({"command_type":"policy.update","command_id":"policy-downgrade","payload":{"competition_id":"comp-test","automation_mode":"assisted"}})).await.unwrap();
        assert!(!scoped(&state,EVENTS,"comp-test").unwrap().iter()
            .any(|event|event["event_type"] == "competition.policy.automation_changed"));
        scheduler::detect_mode_change(&state,"comp-test","assisted").unwrap();
        execute_command(&state,"comp-test",&json!({"command_type":"policy.update","command_id":"policy-source-ranges",
            "payload":{"competition_id":"comp-test","max_concurrent_runs":0,"max_instances":80,"round_timeboxes_s":[90000],"budget_limits":{"custom":1.5}}})).await.unwrap();
        let policy = state.store.get(POLICIES,"comp-test").unwrap().unwrap();
        assert_eq!(policy["max_concurrent_runs"],0);
        assert_eq!(policy["max_instances"],80);
        assert_eq!(policy["round_timeboxes_s"],json!([90000]));
        let long_id = "policy-".to_owned() + &"x".repeat(180);
        let long_receipt = execute_command(&state,"comp-test",&json!({"command_type":"policy.update",
            "command_id":long_id,"payload":{"competition_id":"comp-test","max_concurrent_runs":0}})).await.unwrap();
        assert_eq!(long_receipt["command_id"],long_id);
        assert!(
            scoped(&state, EVENTS, "comp-test")
                .unwrap()
                .iter()
                .any(|v| v["event_type"] == "competition.policy.automation_changed")
        );
    }

    #[tokio::test]
    async fn ctfd_sync_and_submission_keep_answer_out_of_projections() {
        let app = Router::new()
            .route("/api/v1/challenges",get(|| async { Json(json!({"success":true,"data":[{"id":7,"name":"Hello","category":"web","value":100}]})) }))
            .route("/api/v1/challenges/7",get(|| async { Json(json!({"success":true,"data":{"id":7,"description":"Find the flag","value":100,"hints":[],"files":["/files/sample.bin?token=short"]}})) }))
            .route("/files/sample.bin",get(|| async { ([("content-type","application/octet-stream")],b"artifact-bytes".to_vec()) }))
            .route("/api/v1/challenges/attempt",post(|| async { Json(json!({"success":true,"data":{"status":"correct"}})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let (_dir, state) = test_state();
        let Json(created) = create_connection(State(state.clone()),Json(json!({"platform_kind":"ctfd","base_url":base_url,"account_key":"test","credential":"token","command_id":"create-1"}))).await.unwrap();
        let connection_id = created["receipt"]["output"]["connection_id"]
            .as_str()
            .unwrap();
        let Json(created) = create_competition(State(state.clone()),Json(json!({"connection_id":connection_id,"external_competition_id":"default","title":"Test","command_id":"sync-1"}))).await.unwrap();
        let competition_id = created["receipt"]["output"]["competition_id"]
            .as_str()
            .unwrap();
        let snapshot = live_snapshot(&state, competition_id).unwrap().unwrap();
        assert_eq!(snapshot["challenges"].as_array().unwrap().len(), 1);
        let revision = &snapshot["challenges"][0]["current_revision"];
        let artifact_id = revision["artifacts"][0]["artifact_id"].as_str().unwrap();
        assert!(artifact_id.starts_with("cart-"));
        assert!(revision["artifacts"][0].get("digest").is_none());
        assert_eq!(revision["artifacts"][0]["name"], "sample.bin");
        let paths = artifacts::local_paths(&state, revision).unwrap();
        assert_eq!(fs::read(&paths[0]).unwrap(), b"artifact-bytes");
        let duplicate = artifacts::store_bytes(&state,b"artifact-bytes","copy.bin","application/octet-stream").unwrap();
        assert_ne!(duplicate["artifact_id"],artifact_id);
        let staged = artifacts::stage_for_run(&state, revision, "run-staging-test").unwrap();
        assert!(staged[0].ends_with("/sample.bin"));
        assert_eq!(fs::read(&staged[0]).unwrap(), b"artifact-bytes");
        let before = state.store.list(EVENTS).unwrap().len();
        let competition = live_competition(&state, competition_id).unwrap();
        sync_live_competition(&state, &competition).await.unwrap();
        assert_eq!(state.store.list(EVENTS).unwrap().len(), before);
        assert!(
            state
                .store
                .get(SYNC_CURSORS, competition_id)
                .unwrap()
                .is_some()
        );
        let challenge_id = snapshot["challenges"][0]["challenge"]["challenge_id"]
            .as_str()
            .unwrap();
        let receipt = submit_command(
            &state,
            competition_id,
            "competition.submission.submit",
            "submit-1",
            &json!({"challenge_id":challenge_id,"answer":"flag{private}"}),
        )
        .await
        .unwrap();
        assert_eq!(receipt["state"], "waiting");
        submission_outbox::maintenance(&state, competition_id).await.unwrap();
        assert_eq!(state.store.get(COMMANDS,"submit-1").unwrap().unwrap()["response"]["receipt"]["output"]["state"], "correct");
        let snapshot = live_snapshot(&state, competition_id).unwrap().unwrap();
        assert!(!snapshot.to_string().contains("flag{private}"));
        assert!(!snapshot.to_string().contains("answer_ref"));
        assert!(
            !json!(state.store.list(CANDIDATES).unwrap())
                .to_string()
                .contains("flag{private}")
        );
        server.abort();
    }

    #[tokio::test]
    async fn browser_profile_uses_imported_same_origin_cookies() {
        use axum::http::HeaderMap;
        let app = Router::new()
            .route("/",get(|headers: HeaderMap| async move {
                if headers.get("cookie").and_then(|v|v.to_str().ok()) != Some("sid=abc") { return (StatusCode::UNAUTHORIZED,Json(json!({"error":"auth"}))); }
                (StatusCode::OK,Json(json!({"ok":true})))
            }))
            .route("/api/challenges",get(|| async {Json(json!({"data":[{"id":"web-1","name":"Web One","category":"web","description":"Inspect this challenge","target":"https://example.invalid"}]}))}))
            .route("/api/submit/web-1",post(|| async {Json(json!({"result":"correct"}))}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let (_dir, state) = test_state();
        let Json(created) = create_connection(State(state.clone()),Json(json!({"platform_kind":"generic_browser","base_url":base_url,"account_key":"test",
            "site_profile":{"probe_path":"/","challenges_path":"/api/challenges","submit_path":"/api/submit/{challenge_key}"},"command_id":"browser-create"}))).await.unwrap();
        let connection_id = created["receipt"]["output"]["connection_id"]
            .as_str()
            .unwrap();
        let _ = put_browser_session(State(state.clone()),Path(connection_id.to_owned()),Json(json!({"storage_state":{"cookies":[{"name":"sid","value":"abc","domain":"127.0.0.1","path":"/","expires":-1,"secure":false}],"origins":[]},"command_id":"browser-import"}))).await.unwrap();
        let _ = probe_connection(
            State(state.clone()),
            Path(connection_id.to_owned()),
            Json(json!({"command_id":"browser-probe"})),
        )
        .await
        .unwrap();
        let Json(created) = create_competition(State(state.clone()),Json(json!({"connection_id":connection_id,"external_competition_id":"default","command_id":"browser-sync"}))).await.unwrap();
        let competition_id = created["receipt"]["output"]["competition_id"]
            .as_str()
            .unwrap();
        let snapshot = live_snapshot(&state, competition_id).unwrap().unwrap();
        assert_eq!(snapshot["challenges"].as_array().unwrap().len(), 1);
        let challenge_id = snapshot["challenges"][0]["challenge"]["challenge_id"]
            .as_str()
            .unwrap();
        let receipt = submit_command(
            &state,
            competition_id,
            "competition.submission.submit",
            "browser-submit",
            &json!({"challenge_id":challenge_id,"answer":"flag{private}"}),
        )
        .await
        .unwrap();
        assert_eq!(receipt["state"], "waiting");
        submission_outbox::maintenance(&state, competition_id).await.unwrap();
        assert_eq!(state.store.get(COMMANDS,"browser-submit").unwrap().unwrap()["response"]["receipt"]["output"]["state"], "correct");
        assert!(
            !live_snapshot(&state, competition_id)
                .unwrap()
                .unwrap()
                .to_string()
                .contains("flag{private}")
        );
        let expired = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        let _ = put_browser_session(
            State(state.clone()),
            Path(connection_id.to_owned()),
            Json(json!({"renew":true,"expires_at":expired,"command_id":"browser-expire"})),
        )
        .await
        .unwrap();
        assert_eq!(
            browser_status(&state, connection_id).unwrap()["expired"],
            true
        );
        let renewed = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
        let _ = put_browser_session(
            State(state.clone()),
            Path(connection_id.to_owned()),
            Json(json!({"renew":true,"expires_at":renewed,"command_id":"browser-renew"})),
        )
        .await
        .unwrap();
        let _ = probe_connection(
            State(state.clone()),
            Path(connection_id.to_owned()),
            Json(json!({"command_id":"browser-reprobe"})),
        )
        .await
        .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn versioned_browser_session_rejects_non_text_expiry() {
        let (_dir, state) = test_state();
        let Json(created) = create_connection(State(state.clone()), Json(json!({"platform_kind":"generic_browser","base_url":"http://127.0.0.1:9","command_id":"i99-create"}))).await.unwrap();
        let connection_id = created["receipt"]["output"]["connection_id"].as_str().unwrap();
        let storage = json!({"cookies":[],"origins":[]});
        let rejected = versioned_routes::put_browser_session(State(state.clone()), Path(connection_id.to_owned()), Json(json!({"storage_state":storage,"expires_at":123,"command_id":"i99-number"}))).await;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(rejected.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let message = body["error"]["message"].as_str().or_else(|| body["receipt"]["error"]["message"].as_str()).unwrap_or("");
        assert!(message.contains("RFC3339 text"), "{body}");
        assert_eq!(browser_status(&state, connection_id).unwrap()["present"], false);
        let timeless = versioned_routes::put_browser_session(State(state.clone()), Path(connection_id.to_owned()), Json(json!({"storage_state":storage,"expires_at":"2026-11-05T00:00:00","command_id":"i99-notz"}))).await;
        assert_eq!(timeless.status(), StatusCode::BAD_REQUEST);
        let future = chrono::Utc::now() + chrono::Duration::days(1);
        let expiry = future.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap()).to_rfc3339();
        let accepted = versioned_routes::put_browser_session(State(state.clone()), Path(connection_id.to_owned()), Json(json!({"storage_state":storage,"expires_at":expiry,"command_id":"i99-ok"}))).await;
        assert!(accepted.status().is_success(), "{}", accepted.status());
        let status = browser_status(&state, connection_id).unwrap();
        assert_eq!(status["expired"], false);
        assert_eq!(status["expires_at"], future.to_rfc3339());
        let renewed_bad = versioned_routes::put_browser_session(State(state.clone()), Path(connection_id.to_owned()), Json(json!({"renew":true,"expires_at":123,"command_id":"i99-renew-number"}))).await;
        assert_eq!(renewed_bad.status(), StatusCode::BAD_REQUEST);
        assert_eq!(browser_status(&state, connection_id).unwrap()["expires_at"], future.to_rfc3339());
        let illegal = browser_status_record(Some(&json!({"reference":"secret://platform/example","expires_at":"illegal-old-expiry","imported_at":"2026-11-03T00:00:00+00:00","cookies":1,"origins":0})));
        assert_eq!(illegal["expired"], true);
    }

    #[tokio::test]
    async fn rctf_instance_lifecycle_updates_live_lease() {
        let app = Router::new()
            .route("/api/v1/users/me",get(|| async {Json(json!({"kind":"goodUser","data":{"id":"u"}}))}))
            .route("/api/v2/challs",get(|| async {Json(json!({"kind":"goodChallengesV2","data":[{"id":"one","name":"One","category":"web","description":"Find flag"}]}))}))
            .route("/api/v2/integrations/challs/one/instance",put(|Json(body): Json<Value>| async move {
                    assert_eq!(body, json!({}));
                    Json(json!({"kind":"goodInstance","data":{"endpoints":[{"kind":"http","host":"instance.test","port":1234}]}}))
                })
                .patch(|Json(body): Json<Value>| async move {
                    assert_eq!(body, json!({}));
                    Json(json!({"kind":"goodInstance","data":{"endpoints":[{"kind":"http","host":"instance.test","port":1235}]}}))
                })
                .delete(|| async {Json(json!({"kind":"goodInstance","data":null}))}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let (_dir, state) = test_state();
        let Json(created) = create_connection(State(state.clone()),Json(json!({"platform_kind":"rctf","base_url":base_url,"account_key":"test","credential":"token","command_id":"rctf-create"}))).await.unwrap();
        let connection_id = created["receipt"]["output"]["connection_id"]
            .as_str()
            .unwrap();
        let _ = probe_connection(
            State(state.clone()),
            Path(connection_id.to_owned()),
            Json(json!({"command_id":"rctf-probe"})),
        )
        .await
        .unwrap();
        let Json(created) = create_competition(State(state.clone()),Json(json!({"connection_id":connection_id,"external_competition_id":"default","command_id":"rctf-sync"}))).await.unwrap();
        let competition_id = created["receipt"]["output"]["competition_id"]
            .as_str()
            .unwrap();
        let snapshot = live_snapshot(&state, competition_id).unwrap().unwrap();
        let challenge_id = snapshot["challenges"][0]["challenge"]["challenge_id"]
            .as_str()
            .unwrap();
        let acquired = instance_command(
            &state,
            competition_id,
            "instance.ensure",
            "lease-1",
            &json!({"challenge_id":challenge_id}),
        )
        .await
        .unwrap();
        let lease_id = acquired["output"]["lease_id"].as_str().unwrap();
        assert_eq!(acquired["state"],"waiting");
        let ensure_effect_id = acquired["effect_ids"][0].as_str().unwrap();
        assert_eq!(state.store.get("platform_effect_receipts",ensure_effect_id).unwrap().unwrap()["state"],"waiting");
        assert_eq!(state.store.get(LEASES,lease_id).unwrap().unwrap()["state"],"requested");
        leases::maintenance(&state,competition_id).await.unwrap();
        assert_eq!(state.store.get(COMMANDS,"lease-1").unwrap().unwrap()["response"]["receipt"]["state"],"completed");
        assert_eq!(state.store.get("platform_effect_receipts",ensure_effect_id).unwrap().unwrap()["state"],"completed");
        assert_eq!(state.store.get(LEASES,lease_id).unwrap().unwrap()["address"],"http://instance.test:1234");
        let duplicate = instance_command(
            &state,
            competition_id,
            "instance.ensure",
            "lease-2",
            &json!({"challenge_id":challenge_id}),
        )
        .await
        .unwrap_err();
        assert_eq!(duplicate.code, "competition.lease.active_exists");
        let mut lease = state.store.get(LEASES,lease_id).unwrap().unwrap();
        let original_token = lease["fencing_token"].clone();
        let original_generation = lease["generation"].clone();
        lease["renew_deadline_at"] = json!((chrono::Utc::now()-chrono::Duration::seconds(1)).to_rfc3339());
        state.store.put(LEASES,lease_id,&lease).unwrap();
        leases::maintenance(&state,competition_id).await.unwrap();
        let renewed = state.store.get(LEASES,lease_id).unwrap().unwrap();
        assert_eq!(renewed["address"], "http://instance.test:1235");
        assert_eq!(renewed["fencing_token"],original_token);
        assert_eq!(renewed["generation"],original_generation);
        assert_eq!(
            live_snapshot(&state, competition_id).unwrap().unwrap()["challenges"][0]["active_lease"]
                ["lease_id"],
            lease_id
        );
        let stop = instance_command(
            &state,
            competition_id,
            "instance.stop",
            "lease-3",
            &json!({"lease_id":lease_id}),
        )
        .await
        .unwrap();
        assert_eq!(stop["state"],"waiting");
        let stop_effect_id = stop["effect_ids"][0].as_str().unwrap();
        assert_eq!(state.store.get("platform_effect_receipts",stop_effect_id).unwrap().unwrap()["state"],"waiting");
        leases::maintenance(&state,competition_id).await.unwrap();
        assert_eq!(state.store.get(COMMANDS,"lease-3").unwrap().unwrap()["response"]["receipt"]["state"],"completed");
        assert_eq!(state.store.get("platform_effect_receipts",stop_effect_id).unwrap().unwrap()["state"],"completed");
        assert!(live_snapshot(&state,competition_id).unwrap().unwrap()["challenges"][0]["active_lease"].is_null());
        server.abort();
    }

    #[tokio::test]
    async fn gzctf_password_login_provides_cookie_without_persisting_password() {
        use axum::http::{HeaderMap, header};
        let app = Router::new()
            .route("/api/account/login",post(|Json(body): Json<Value>| async move {
                if body["userName"] == "alice" && body["password"] == "private-password" {
                    (StatusCode::OK,[(header::SET_COOKIE,"GZCTF_Token=session; Path=/; HttpOnly")],Json(json!({"ok":true})))
                } else {
                    (StatusCode::UNAUTHORIZED,[(header::SET_COOKIE,"")],Json(json!({"ok":false})))
                }
            }))
            .route("/openapi/v1.json",get(|headers: HeaderMap| async move {
                if headers.get(header::COOKIE).and_then(|v|v.to_str().ok()) != Some("GZCTF_Token=session") {
                    return (StatusCode::UNAUTHORIZED,Json(json!({"error":"auth"})));
                }
                (StatusCode::OK,Json(json!({"paths":{"/api/game":{},"/api/game/{id}/details":{},"/api/game/{id}/challenges/{challengeid}":{}}})))
            }))
            .route("/api/game/42/details",get(|| async {Json(json!({"challenges":{"web":[{"id":7,"title":"Test","score":100}]}}))}))
            .route("/api/game/42/challenges/7",get(|| async {Json(json!({"content":"Find the flag","score":100,"type":"StaticAttachment"}))}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let (_dir, state) = test_state();
        let Json(created) = create_connection(State(state.clone()),Json(json!({"platform_kind":"gzctf","base_url":base_url,"account_key":"alice","credential":"private-password","command_id":"gz-create"}))).await.unwrap();
        let connection_id = created["receipt"]["output"]["connection_id"]
            .as_str()
            .unwrap();
        let _ = probe_connection(
            State(state.clone()),
            Path(connection_id.to_owned()),
            Json(json!({"command_id":"gz-probe"})),
        )
        .await
        .unwrap();
        let Json(created) = create_competition(State(state.clone()),Json(json!({"connection_id":connection_id,"external_competition_id":"42","command_id":"gz-sync"}))).await.unwrap();
        let competition_id = created["receipt"]["output"]["competition_id"]
            .as_str()
            .unwrap();
        assert_eq!(
            live_snapshot(&state, competition_id).unwrap().unwrap()["challenges"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            !json!(state.store.list(CONNECTIONS).unwrap())
                .to_string()
                .contains("private-password")
        );
        server.abort();
    }

    #[tokio::test]
    async fn sync_keeps_a_pause_source_when_the_challenge_is_hidden_or_tombstoned() {
        let (_dir, state) = test_state();
        let connection = json!({"connection_id":"conn-pause","platform_kind":"ctfd"});
        let open = json!({
            "external_challenge_id":"1",
            "name":"PauseKeep",
            "category":"misc",
            "remote_state":"open",
            "points":10,
            "description":"pause source text",
            "hints":[],
            "artifacts":[],
            "raw":{"list":{"id":1,"state":"paused","paused_from":"queued"},
                "detail":{"paused_from":"queued","description":"pause source text"}}
        });
        let hidden = json!({
            "external_challenge_id":"1",
            "name":"PauseKeep",
            "category":"misc",
            "remote_state":"hidden",
            "points":0,
            "description":"",
            "hints":[],
            "artifacts":[],
            "raw":{"id":1,"type":"hidden","state":"hidden"}
        });
        let added = save_challenges(&state, &connection, "comp-pause", "cmd-pause-1", &[open], true).await.unwrap();
        assert_eq!(added["added"].as_array().unwrap().len(), 1);
        let stored = state.store.list(CHALLENGES).unwrap().into_iter()
            .find(|row| row["competition_id"] == "comp-pause").unwrap();
        let challenge_id = field(&stored, "challenge_id").to_owned();
        let revision_id = field(&stored, "current_revision_id").to_owned();
        assert_eq!(stored["paused_from"], "queued");
        assert_eq!(state.store.get(REVISIONS, &revision_id).unwrap().unwrap()["description"], "pause source text");

        let locked = save_challenges(&state, &connection, "comp-pause", "cmd-pause-2", &[hidden.clone()], true).await.unwrap();
        assert_eq!(locked["locked"][0], challenge_id);
        let stored = state.store.get(CHALLENGES, &challenge_id).unwrap().unwrap();
        assert_eq!(stored["paused_from"], "queued");
        assert_eq!(stored["remote_state"], "hidden");
        assert_eq!(stored["current_revision_id"], revision_id);
        assert_eq!(stored["tombstoned"], false);

        let removed = save_challenges(&state, &connection, "comp-pause", "cmd-pause-3", &[], true).await.unwrap();
        assert_eq!(removed["removed"][0], challenge_id);
        let stored = state.store.get(CHALLENGES, &challenge_id).unwrap().unwrap();
        assert_eq!(stored["paused_from"], "queued");
        assert_eq!(stored["tombstoned"], true);
        assert_eq!(stored["state"], "retired");
        assert_eq!(stored["current_revision_id"], revision_id);

        let returned = save_challenges(&state, &connection, "comp-pause", "cmd-pause-4", &[hidden], true).await.unwrap();
        assert_eq!(returned["unchanged"][0], challenge_id);
        assert!(returned["added"].as_array().unwrap().is_empty());
        let stored = state.store.get(CHALLENGES, &challenge_id).unwrap().unwrap();
        assert_eq!(stored["paused_from"], "queued");
        assert_eq!(stored["tombstoned"], true);
        assert_eq!(stored["state"], "retired");
        assert_eq!(state.store.get(REVISIONS, &revision_id).unwrap().unwrap()["description"], "pause source text");
    }
}
