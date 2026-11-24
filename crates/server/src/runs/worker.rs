//! Local single-worker execution and `muteki-blackboard-v2` file-IPC ingress drain.

use crate::{
    AppState,
    agent_runtime::{StartRequest, access, binaries, credentials, descriptors},
    native_chat,
    runs::{
        ActiveWorker, ContainerCancelRequest, emit_event, mark_run_finished, now_secs, record_flag,
        record_hitl_request, session_dir,
        settings::{cli_engine_id, normalize_engine, resolve_effective_credential},
    },
    runtime_api,
};
use agent::{AgentEvent, DeltaKind, ItemContent, SessionCommand, TurnOptions, TurnStatus};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

#[path = "worker_clock.rs"]
mod clock;
#[path = "ctf/worker.rs"]
mod ctf;
#[path = "pentest/worker.rs"]
mod pentest_mode;
use clock::WorkerClock;

pub const SKILL_MD: &str = include_str!("SKILL.md");
pub const BLACKBOARD_PY: &str = include_str!("blackboard.py");

const ROLE_CONTRACT_MARKER: &str = "<!-- muteki-role-contract -->";

const RESTART_TARGET_SECTION: &str = "If tool evidence confirms the remote target container has crashed or become\ncompletely unreachable after previously responding (never for HTTP 4xx/5xx\nresponses, which mean the service is alive), request a platform rebuild:\n\n```bash\npython3 \"$MUTEKI_BLACKBOARD_SCRIPT\" restart-target --confirmed-fault \\\n  --evidence '<observed fault and checks>'\n```";

const CAMPAIGN_FINISH_SENTENCE: &str = " When `MUTEKI_WORKER_EXECUTION_MODE=campaign`, the receipt\nhas `finish_worker=false`: the Fact is saved as a checkpoint and you continue the\nsame session toward all platform-confirmed flags.";

pub fn render_blackboard_skill(mode: &str, restart_bound: bool, campaign: bool) -> String {
    let norm = SKILL_MD.replace("\r\n", "\n");
    let split_before = |input: &str, marker: &str| -> (String, String) {
        match input.find(marker) {
            Some(idx) => (input[..idx].to_owned(), input[idx..].to_owned()),
            None => (input.to_owned(), String::new()),
        }
    };
    let (common_head, rest) = split_before(&norm, "\nIn CTF mode,");
    let (ctf_enables, rest) = split_before(&rest, "\nThe draft is not published");
    let (common_commit, rest) = split_before(&rest, "\nIn normal Step execution,");
    let (step_exec_block, rest) = split_before(&rest, "\nRecord a bounded direction");
    let (common_tools, rest) = split_before(&rest, "\nCTF Workers submit a value");
    let (ctf_flag_basic, rest) = split_before(
        &rest,
        "\nWhen `MUTEKI_WORKER_EXECUTION_MODE=campaign`, the first receipt",
    );
    let (ctf_campaign_verdict, rest) = split_before(&rest, "\nPut ordinary reusable files");
    let (common_shared, pentest_block) = split_before(&rest, "\nPentest Workers use the same");

    let is_ctf = mode == "ctf";
    let is_pentest = mode == "pentest";
    let show_campaign = is_ctf && campaign;
    let show_restart = is_ctf && restart_bound;

    let mut sections: Vec<String> = Vec::new();
    let push_section = |out: &mut Vec<String>, text: &str| {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
        }
    };

    push_section(&mut sections, &common_head);
    if is_ctf {
        push_section(&mut sections, &ctf_enables);
    }
    push_section(&mut sections, &common_commit);
    let step_exec_clean = if show_campaign {
        step_exec_block
    } else {
        step_exec_block.replace(CAMPAIGN_FINISH_SENTENCE, "")
    };
    push_section(&mut sections, &step_exec_clean);
    push_section(&mut sections, &common_tools);
    if is_ctf {
        push_section(&mut sections, &ctf_flag_basic);
        if show_campaign {
            push_section(&mut sections, &ctf_campaign_verdict);
        }
        if show_restart {
            push_section(&mut sections, RESTART_TARGET_SECTION);
        }
    }
    push_section(&mut sections, &common_shared);
    if is_pentest {
        push_section(&mut sections, &pentest_block);
    }

    let mut rendered = sections.join("\n\n");
    rendered.push('\n');
    rendered
}

pub(crate) fn available_run_capabilities(
    state: &AppState,
    run_id: &str,
    mode: &str,
) -> Vec<String> {
    let mut caps: HashSet<String> = [
        "blackboard",
        "submit-fact",
        "commit-step",
        "mark-deadend",
        "request-input",
        "save-poc",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();

    if mode == "ctf" {
        caps.insert("submit-flag".to_owned());
        if state.runs.has_target_restarter(run_id) {
            caps.insert("restart_target".to_owned());
            caps.insert("restart-target".to_owned());
        }
    } else if mode == "pentest" {
        for cap in [
            "recent-evidence",
            "read-artifact",
            "submit-report",
            "review",
        ] {
            caps.insert(cap.to_owned());
        }
        if crate::capabilities::enabled(state, "skills", "agent-browser") {
            caps.insert("agent-browser".to_owned());
            caps.insert("browser".to_owned());
        }
    }

    if let Ok(registry) = crate::plugins::registry(state) {
        for engine in ["pi", "claude", "codex", "omp"] {
            if let Ok(records) = registry.enabled(engine, mode) {
                for record in records {
                    if let Some(id) = record["id"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        caps.insert(id.to_owned());
                    }
                    if let Some(name) = record["name"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        caps.insert(name.to_owned());
                    }
                    if let Some(mcp) = record["mcp"].as_object() {
                        for server in mcp.keys() {
                            let s = server.trim();
                            if !s.is_empty() {
                                caps.insert(s.to_owned());
                            }
                        }
                    }
                    for skill in record["skills"].as_array().into_iter().flatten() {
                        if let Some(name) = skill["name"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            caps.insert(name.to_owned());
                        }
                        if let Some(id) = skill["id"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            caps.insert(id.to_owned());
                        }
                    }
                }
            }
            for tool in registry.cached_tool_names(engine, mode, run_id) {
                let t = tool.trim();
                if !t.is_empty() {
                    caps.insert(t.to_owned());
                }
            }
        }
    }

    let mut list: Vec<String> = caps.into_iter().collect();
    list.sort();
    list
}

pub fn validate_requested_capabilities(
    requested: &[String],
    available: &[String],
) -> Result<(), String> {
    if requested.is_empty() {
        return Ok(());
    }
    let allowed: HashSet<&str> = available
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let missing: Vec<&str> = requested
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !allowed.contains(*s))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("请求的未开放或未配置能力: {}", missing.join(", ")))
    }
}

pub(crate) fn stage_swarm_worker_plugins(
    state: &AppState,
    workdir: &Path,
    solver_id: &str,
    engine: &str,
    mode: &str,
) -> Result<crate::plugins::StagedWorkerComponents, String> {
    let registry = crate::plugins::registry(state).map_err(|e| e.to_string())?;
    let safe_solver = solver_id.replace(['/', '\\'], "_");
    let stage_root = workdir
        .join(".muteki-agent-state/worker-plugins")
        .join(&safe_solver);
    let staged = registry
        .stage_worker_assets(&stage_root, engine, mode)
        .map_err(|e| e.to_string())?;
    if engine != "pi" && !staged.skills.is_empty() {
        #[cfg(unix)]
        for (idx, skill_md) in staged.skills.iter().enumerate() {
            let Some(skill_dir) = skill_md.parent() else {
                continue;
            };
            let Ok(rel_from_workdir) = skill_dir.strip_prefix(workdir) else {
                continue;
            };
            let link_name = format!("plugin-{safe_solver}-{idx}");
            for rel_root in [".claude/skills", ".codex/skills", ".agents/skills"] {
                let link = workdir.join(rel_root).join(&link_name);
                if let Some(parent) = link.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if link.symlink_metadata().is_err() {
                    let _ = std::os::unix::fs::symlink(
                        Path::new("../..").join(rel_from_workdir),
                        &link,
                    );
                }
            }
        }
    }
    Ok(staged)
}

async fn start_container_worker(
    state: &Arc<AppState>,
    run_id: &str,
    worker: &str,
    link: &Arc<super::container_exec::SupervisorLink>,
    spec: &super::container_exec::WorkerSpec,
    budget: &super::execution_budget::WorkerLease,
) -> Result<(String, super::supervisor_stream::Stream), String> {
    let handle = state.runs.handle(state, run_id);
    loop {
        let transition = handle.delete_lock.lock().await;
        let run = state
            .store
            .get("runs", run_id)
            .map_err(|error| error.to_string())?
            .ok_or("Worker Run disappeared before launch")?;
        if handle.deleting.load(Ordering::SeqCst)
            || matches!(
                run["status"].as_str(),
                Some("finished" | "solved" | "stopped" | "failed" | "cancelled" | "archived")
            )
        {
            return Err("Worker Run ended before launch".into());
        }
        let frozen = run["freeze_control"].is_object()
            && (run["freeze_control"]["run_wide"] == true
                || run["freeze_control"]["workers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|row| row["worker_id"] == worker));
        if frozen {
            drop(transition);
            tokio::time::sleep(Duration::from_millis(120)).await;
            continue;
        }
        let left = budget.remaining(state, run_id, worker, "finalize")?;
        if left <= 0. {
            return Err("Worker execution budget expired before launch".into());
        }
        let mut spec = spec.clone();
        spec.timeout_sec = left.ceil() as i64;
        let (wid, stream) = link.start_worker(&spec).await?;
        handle
            .container_sessions
            .lock()
            .unwrap()
            .insert(worker.into(), (link.clone(), wid.clone()));
        return Ok((wid, stream));
    }
}

async fn resource_snapshot_or_cancel(
    placement: Option<(&AppState, &str)>,
    cancel_rx: &mut Option<oneshot::Receiver<oneshot::Sender<bool>>>,
    host_cancel: &AtomicBool,
) -> Option<Value> {
    tokio::select! {
        biased;
        request=async {match cancel_rx {Some(rx)=>rx.await,None=>std::future::pending().await}}, if cancel_rx.is_some()=>{
            if let Ok(receipt)=request {let _=receipt.send(true);}
            None
        },
        _=async {loop {if host_cancel.load(Ordering::SeqCst){break;} tokio::time::sleep(Duration::from_millis(100)).await;}}=>None,
        value=async {match placement{Some((state,node))=>crate::runner_resources::snapshot_node(state,node).await,None=>crate::runner_resources::snapshot().await}}=>Some(value),
    }
}

/// Queue this Worker on its execution node's durable reservation list.
/// Returns `None` after recording the exit reason when the Worker was canceled
/// or admission failed, so callers only need to return.
#[allow(clippy::too_many_arguments)]
async fn acquire_node_reservation(
    state: &Arc<AppState>,
    run_id: &str,
    solver_id: &str,
    container: bool,
    browser: bool,
    cancel_rx: &mut Option<oneshot::Receiver<oneshot::Sender<bool>>>,
    host_cancel: &AtomicBool,
    exit_reason_slot: &Mutex<Option<String>>,
) -> Option<crate::runner_resources::Reservation> {
    let workload = crate::runner_resources::Workload {
        generation: worker_execution_generation(solver_id),
        browser,
    };
    let settings = super::launch_config::settings(state, run_id).unwrap_or(Value::Null);
    let node = if container {
        crate::runner_remote::controller::selected(&settings).unwrap_or("local")
    } else {
        "local"
    };
    let fail = |reason: &str, detail: Value| {
        *exit_reason_slot.lock().unwrap() = Some("failed".into());
        emit_event(
            state,
            run_id,
            Some(solver_id),
            "worker.error",
            json!({"reason":reason,"detail":detail}),
        );
        state.runs.unregister_worker(run_id, solver_id);
    };
    let canceled = || {
        *exit_reason_slot.lock().unwrap() = Some("canceled".into());
        state.runs.unregister_worker(run_id, solver_id);
    };
    let reservation = match crate::runner_resources::Reservation::enqueue_on_node(
        state, run_id, solver_id, workload, node,
    ) {
        Ok(value) => value,
        Err(error) => {
            fail("resource_queue_failed", json!(error));
            return None;
        }
    };
    let Some(resources) =
        resource_snapshot_or_cancel(Some((state, node)), cancel_rx, host_cancel).await
    else {
        canceled();
        return None;
    };
    let mut recommendation: crate::runner_resources::Recommendation =
        serde_json::from_value(resources["recommendation"].clone())
            .expect("resource snapshot schema");
    let mut announced = false;
    let mut recommendation_at = tokio::time::Instant::now();
    loop {
        if host_cancel.load(Ordering::SeqCst) {
            canceled();
            return None;
        }
        if recommendation_at.elapsed() >= Duration::from_secs(10) {
            let Some(refreshed) =
                resource_snapshot_or_cancel(Some((state, node)), cancel_rx, host_cancel).await
            else {
                canceled();
                return None;
            };
            match serde_json::from_value(refreshed["recommendation"].clone()) {
                Ok(value) => {
                    recommendation = value;
                    recommendation_at = tokio::time::Instant::now();
                }
                Err(error) => {
                    fail("resource_sample_invalid", json!(error.to_string()));
                    return None;
                }
            }
        }
        match reservation.try_admit(state, &recommendation) {
            Ok(true) => return Some(reservation),
            Ok(false) => {}
            Err(error) => {
                fail("resource_admission_failed", json!(error));
                return None;
            }
        }
        if !announced {
            emit_event(
                state,
                run_id,
                Some(solver_id),
                "worker.queued",
                json!({"reason":"node_concurrency_limit"}),
            );
            announced = true;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            request = async { match &mut *cancel_rx { Some(rx) => rx.await, None => std::future::pending().await } }, if cancel_rx.is_some() => {
                if let Ok(receipt) = request { let _ = receipt.send(true); }
                canceled();
                return None;
            }
        }
    }
}

#[cfg(test)]
#[tokio::test]
async fn queued_resource_sampling_prioritizes_cancellation() {
    let (request_tx, request_rx) = oneshot::channel();
    let (ack_tx, ack_rx) = oneshot::channel();
    request_tx.send(ack_tx).unwrap();
    let mut request = Some(request_rx);
    assert!(
        resource_snapshot_or_cancel(None, &mut request, &AtomicBool::new(false))
            .await
            .is_none()
    );
    assert!(ack_rx.await.unwrap());
    assert!(
        resource_snapshot_or_cancel(None, &mut None, &AtomicBool::new(true))
            .await
            .is_none()
    );
}

pub(crate) fn worker_execution_generation(solver_id: &str) -> u64 {
    solver_id
        .rsplit_once("-g")
        .and_then(|(_, generation)| generation.parse::<u64>().ok())
        .filter(|generation| *generation > 0)
        .unwrap_or(1)
}

pub(super) fn external_start_key(command_id: &str, solver_id: &str) -> String {
    format!("{command_id}:external-start:{solver_id}")
}

fn record_external_start_request(
    state: &AppState,
    run_id: &str,
    solver_id: &str,
) -> Result<(), String> {
    let generation = worker_execution_generation(solver_id);
    let marker = state
        .store
        .list("run_execution_commands")
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|row| {
            row["run_id"] == run_id
                && row["generation"] == generation
                && matches!(row["action"].as_str(), Some("start" | "resolve"))
                && row["specs"]
                    .as_array()
                    .is_some_and(|specs| specs.iter().any(|spec| spec["solver_id"] == solver_id))
        });
    let Some(marker) = marker else {
        return Ok(());
    };
    let Some(command_id) = marker["command_id"].as_str() else {
        return Ok(());
    };
    state
        .store
        .put(
            "run_execution_commands",
            &external_start_key(command_id, solver_id),
            &json!({"command_id":command_id,"run_id":run_id,"generation":generation,
            "solver_id":solver_id,"action":"external_start_requested"}),
        )
        .map_err(|error| error.to_string())
}

fn emit_start_event_once(
    state: &AppState,
    run_id: &str,
    solver_id: Option<&str>,
    prior: &[Value],
    event_type: &str,
    payload: Value,
) {
    let repeated = prior.iter().any(|event| {
        event["event_type"] == event_type
            && event["solver_id"].as_str() == solver_id
            && event["payload"]["intent_id"] == payload["intent_id"]
            && event["payload"]["kind"] == payload["kind"]
            && event["payload"]["phase"] == payload["phase"]
    });
    if !repeated {
        emit_event(state, run_id, solver_id, event_type, payload);
    }
}

// Carried forward from the final legacy Python Worker prompt.
#[path = "ctf/prompts.rs"]
mod ctf_prompts;
#[path = "pentest/prompts.rs"]
mod pentest_prompts;
pub const CTF_WORKER_SYSTEM_PROMPT: &str = ctf_prompts::SYSTEM;

pub const REVIEW_WORKER_SYSTEM_PROMPT: &str =
    "You are an autonomous agent. Use the available tools to complete the assigned task.";

pub(in crate::runs) fn redact_operator_secrets(value: &str, secrets: &[String]) -> String {
    let mut redacted = value.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            redacted = redacted.replace(secret, "[已隐去]");
        }
    }
    redacted
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerSpec {
    pub solver_id: String,
    pub profile_id: String,
    pub profile_label: String,
    pub raw_engine: String,
    pub norm_engine: String,
    pub adapter_id: String,
    pub instance_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub account_id: String,
    pub effective_credential_id: String,
    pub backend: String,
    #[serde(default)]
    pub profile: Value,
}

pub(crate) fn role_for_worker_class(worker_class: &str) -> &'static str {
    match worker_class {
        "review" => "review",
        "verifier" => "verifier",
        _ => "explore",
    }
}

fn implicit_role_fallback_enabled(profile: &Value, role: &str, settings: &Value) -> bool {
    if profile["allow_implicit_role_fallback"] == true
        || settings["allow_implicit_role_fallback"] == true
        || settings[role]["allow_implicit_role_fallback"] == true
    {
        return true;
    }
    match role {
        "review" => settings["review"]["allow_review_fallback"] == true,
        "verifier" => settings["verifier"]["allow_verifier_fallback"] == true,
        _ => false,
    }
}

pub(crate) fn profile_allows_role(profile: &Value, role: &str, settings: &Value) -> bool {
    let roles_opt = profile["roles"].as_array().filter(|r| !r.is_empty());
    let marked_implicit = profile["implicit_roles"]
        .as_array()
        .is_some_and(|roles| roles.iter().any(|value| value == role));

    let Some(roles) = roles_opt else {
        // Empty or omitted `roles` on profile: ordinary `explore` is allowed by default;
        // `review` and `verifier` are implicit and require fallback authorization.
        return if matches!(role, "review" | "verifier") {
            implicit_role_fallback_enabled(profile, role, settings)
        } else {
            true
        };
    };

    let explicit_match = if role == "explore" {
        roles.iter().any(|value| {
            matches!(
                value.as_str(),
                Some("explore" | "bootstrap" | "solve" | "scout" | "code" | "shell_agent")
            )
        })
    } else {
        roles.iter().any(|value| value == role)
    };

    if explicit_match && !marked_implicit {
        return true;
    }
    if matches!(role, "review" | "verifier") {
        return implicit_role_fallback_enabled(profile, role, settings);
    }
    profile["allow_implicit_role_fallback"] == true
        || settings["allow_implicit_role_fallback"] == true
}

pub(crate) fn global_role_capacity(role: &str, settings: &Value, max_workers: usize) -> usize {
    let cap = max_workers.max(1);
    match role {
        "review" => match settings["review"]["max_concurrent"].as_u64() {
            Some(0) | None => cap,
            Some(n) => (n as usize).min(cap),
        },
        "verifier" => match settings["verifier"]["max_concurrent"].as_u64() {
            Some(0) | None => cap,
            Some(n) => (n as usize).min(cap),
        },
        _ => cap,
    }
}

pub(crate) fn profile_role_capacity(
    profile: &Value,
    role: &str,
    settings: &Value,
    max_workers: usize,
) -> usize {
    let cap = max_workers.max(1);
    if profile["unlimited_workers"] == true {
        return cap;
    }
    match role {
        "review" => {
            let prof_val = profile["max_review_running"]
                .as_u64()
                .or_else(|| profile["capacity"]["max_review_running"].as_u64());
            match prof_val {
                Some(n) if n > 0 => (n as usize).min(cap),
                _ => global_role_capacity("review", settings, cap),
            }
        }
        "verifier" => {
            let prof_val = profile["max_verifier_running"]
                .as_u64()
                .or_else(|| profile["capacity"]["max_verifier_running"].as_u64());
            match prof_val {
                Some(n) if n > 0 => (n as usize).min(cap),
                _ => global_role_capacity("verifier", settings, cap),
            }
        }
        _ => {
            let value = profile["max_running"]
                .as_u64()
                .or_else(|| profile["capacity"]["max_running"].as_u64())
                .or_else(|| profile["concurrency"].as_u64())
                .unwrap_or(1);
            if value == 0 {
                cap
            } else {
                (value as usize).min(cap)
            }
        }
    }
}

pub(crate) fn effective_max_continuations(profile: &Value) -> u64 {
    profile["max_continuations"]
        .as_u64()
        .or_else(|| {
            profile["max_continuations"]
                .as_i64()
                .filter(|&v| v >= 0)
                .map(|v| v as u64)
        })
        .unwrap_or(0)
}

pub(crate) fn continuation_boundary_reached(continuations: u64, max_continuations: u64) -> bool {
    max_continuations > 0 && continuations >= max_continuations
}

pub(crate) fn effective_soft_reminder_seconds(profile: &Value) -> u64 {
    profile["soft_reminder_seconds"]
        .as_u64()
        .or_else(|| {
            profile["soft_reminder_seconds"]
                .as_f64()
                .filter(|&v| v >= 0.0)
                .map(|v| v as u64)
        })
        .unwrap_or(0)
}

pub(crate) fn effective_execute_timeout_seconds(profile: &Value, challenge: &Value) -> u64 {
    profile["execute_timeout"]
        .as_u64()
        .or_else(|| profile["timeout_seconds"].as_u64())
        .or_else(|| challenge["execute_timeout"].as_u64())
        .filter(|&v| v > 0)
        .unwrap_or(2400)
}

pub(crate) fn first_execution_slice_seconds(
    soft_reminder_seconds: u64,
    remaining_execute_timeout: u64,
) -> Option<u64> {
    if soft_reminder_seconds == 0 || remaining_execute_timeout == 0 {
        return None;
    }
    Some(soft_reminder_seconds.min(remaining_execute_timeout))
}

pub(crate) fn soft_reminder_prompt(
    assignment: &IntentAssignment,
    campaign_mode: bool,
    finalize_on_exit: bool,
    _elapsed_secs: u64,
    _remaining_secs: u64,
) -> String {
    if campaign_mode {
        campaign_continuation_prompt(assignment, finalize_on_exit)
    } else {
        "本轮探索时间已超建议上限,请尽快汇总已确认的实测事实并调用 submit_fact + commit_step 收束本 Step(若无新探测结果可直接调 commit_step)。".to_owned()
    }
}

pub(crate) fn classify_worker_exit_reason(
    solved: bool,
    canceled: bool,
    timed_out: bool,
    oom: bool,
    error: Option<&str>,
) -> &'static str {
    if solved {
        return "solved";
    }
    if oom {
        return "oom";
    }
    if timed_out {
        return "timeout";
    }
    if canceled {
        return "canceled";
    }
    if let Some(err) = error.map(str::trim).filter(|s| !s.is_empty()) {
        let lower = err.to_ascii_lowercase();
        if lower.contains("oom") || lower.contains("out of memory") || lower.contains("rc=137") {
            return "oom";
        }
        if lower.contains("timed out") || lower.contains("timeout") || lower.contains("超时") {
            return "timeout";
        }
        if lower.contains("cancel") || lower.contains("dropped") || lower.contains("aborted") {
            return "canceled";
        }
        if [
            "rate limit",
            "rate_limit",
            "429",
            "quota",
            "insufficient_quota",
            "billing",
            "unauthorized",
            "401",
            "403",
            "invalid_api_key",
            "api key",
            "provider",
            "vendor",
            "overloaded",
            "502",
            "503",
            "504",
            "upstream",
            "model_not_found",
            "authentication",
        ]
        .iter()
        .any(|kw| lower.contains(kw))
        {
            return "vendor_error";
        }
        return "failed";
    }
    "done"
}

fn campaign_continuation_prompt(assignment: &IntentAssignment, finalize_on_exit: bool) -> String {
    if finalize_on_exit {
        "Continue this authorized CTF challenge in the same Agent session and workspace. \
         A completed model turn does not complete the challenge. Read current platform verdicts \
         with the Blackboard context command. Continue from real tool evidence until the platform \
         confirms every required flag or the Run stops."
            .to_owned()
    } else {
        format!(
            "Continue only the assigned Step in this same Agent session and workspace. \
            Goal: {}. Expected observable: {}. Stop condition: {}. Use real tool results. \
            Submit a Fact and commit the Step when confirmed, or record a dead end and commit \
            the Step when ruled out.",
            assignment.goal, assignment.expected_observable, assignment.stop_condition
        )
    }
}

#[derive(Debug, Clone)]
pub struct IntentAssignment {
    pub intent_id: String,
    pub goal: String,
    pub expected_observable: String,
    pub stop_condition: String,
    pub coverage_key: String,
    pub reopen_hint: Option<String>,
    pub worker_class: String,
}

/// Resolves a worker spec from the start payload (`worker_profiles` / `engines`)
/// or saved worker settings (`settings/workers`).
pub async fn resolve_worker_specs(
    state: &AppState,
    payload: &Value,
    generation: u64,
) -> Result<(Vec<WorkerSpec>, Vec<Value>), (String, Vec<Value>)> {
    let saved_cfg = crate::settings_metadata::configuration(state).unwrap_or_else(|_| json!({}));
    let backend = payload["worker_backend"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| saved_cfg["worker_backend"].as_str())
        .unwrap_or("container")
        .trim()
        .to_owned();
    let node = payload["worker_node_id"]
        .as_str()
        .or_else(|| saved_cfg["worker_node_id"].as_str())
        .unwrap_or("local");
    let run_mode = if payload["bound_to_src"] == true
        || payload["src_task_id"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty())
        || payload["mode"] == "src"
    {
        "src"
    } else if payload["mode"] == "pentest" || payload["challenge"]["mode"] == "pentest" {
        "pentest"
    } else {
        "ctf"
    };
    let container_img_status = if backend == "container" && node == "local" {
        Some(super::container_exec::inspect_worker_image_status().await)
    } else {
        None
    };

    let mut raw_profiles: Vec<Value> = payload["worker_profiles"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p["enabled"].as_bool().unwrap_or(true))
        .collect();

    if raw_profiles.is_empty()
        && let Some(saved) = saved_cfg["worker_profiles"].as_array()
    {
        raw_profiles = saved
            .iter()
            .filter(|p| p["enabled"].as_bool().unwrap_or(true))
            .cloned()
            .collect();
    }

    let category = payload["challenge"]["category"].as_str().unwrap_or("");
    let roster = payload
        .get("engines")
        .or_else(|| saved_cfg["overrides"][category].get("engines"))
        .or_else(|| saved_cfg.get("engines"));
    if let Some(roster) = roster {
        let Some(refs) = roster.as_array() else {
            return Err(("engines must be an array".into(), Vec::new()));
        };
        if !raw_profiles.is_empty() {
            let mut selected = Vec::new();
            let mut seen = HashSet::new();
            for reference in refs {
                let Some(reference) = reference.as_str() else {
                    return Err(("engines entries must be strings".into(), Vec::new()));
                };
                let exact = raw_profiles
                    .iter()
                    .find(|p| p["id"] == reference || p["name"] == reference);
                let matches = if let Some(profile) = exact {
                    vec![profile]
                } else {
                    raw_profiles
                        .iter()
                        .filter(|p| {
                            normalize_engine(p["engine"].as_str().unwrap_or(""))
                                == normalize_engine(reference)
                        })
                        .collect()
                };
                if matches.is_empty() {
                    return Err((
                        format!("Unknown Worker profile in task roster: {reference}"),
                        Vec::new(),
                    ));
                }
                for profile in matches {
                    let identity = profile["id"]
                        .as_str()
                        .or_else(|| profile["name"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    if seen.insert(identity) {
                        selected.push(profile.clone());
                    }
                }
            }
            raw_profiles = selected;
            if raw_profiles.is_empty() {
                return Err(("Task Worker roster is empty".into(), Vec::new()));
            }
        }
    }
    if raw_profiles.is_empty() {
        let engines_list: Vec<String> = payload["engines"]
            .as_array()
            .or_else(|| saved_cfg["engines"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| {
                v.as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .collect();
        for (idx, eng) in engines_list.into_iter().enumerate() {
            raw_profiles.push(json!({
                "id": format!("wp-{}", idx + 1),
                "label": eng,
                "engine": cli_engine_id(&eng),
                "model": "",
                "account_id": "",
                "enabled": true,
            }));
        }
    }

    if raw_profiles.is_empty() {
        return Err((
            "No Worker engines are configured. Add and enable a Worker before starting a run."
                .to_owned(),
            Vec::new(),
        ));
    }
    raw_profiles.sort_by(|left, right| {
        left["priority"]
            .as_u64()
            .unwrap_or(100)
            .cmp(&right["priority"].as_u64().unwrap_or(100))
            .then_with(|| {
                left["id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(right["id"].as_str().unwrap_or(""))
            })
    });

    let max_seats = if payload["coordinator"] == false {
        1
    } else {
        match payload["max_workers"]
            .as_u64()
            .or_else(|| saved_cfg["max_workers"].as_u64())
        {
            Some(0) => raw_profiles.len().max(1),
            Some(limit) => limit as usize,
            None => raw_profiles.len().max(1),
        }
    };

    let mut profile_specs: Vec<Vec<WorkerSpec>> = Vec::new();
    let mut failures = Vec::new();
    let mut engine_counts: HashMap<String, usize> = HashMap::new();

    for profile in raw_profiles.iter() {
        let pid = profile["id"].as_str().unwrap_or("wp-1").trim().to_owned();
        let raw_eng = profile["engine"].as_str().unwrap_or("").trim().to_owned();
        let norm_eng = normalize_engine(&raw_eng);
        let label = profile["label"]
            .as_str()
            .or_else(|| profile["name"].as_str())
            .unwrap_or(&raw_eng)
            .trim()
            .to_owned();
        let model_raw = profile["model"].as_str().unwrap_or("").trim().to_owned();
        let model = if model_raw.is_empty() || model_raw == "default" {
            None
        } else {
            Some(model_raw.clone())
        };
        let effort = profile["reasoning_effort"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "default" && *s != "inherit")
            .map(str::to_owned);
        let account_id = profile["account_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                profile["credential_account"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| {
                profile["credential_id"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or("")
            .trim()
            .to_owned();

        let Some(desc) = descriptors::find(&norm_eng) else {
            failures.push(json!({
                "profile_id": pid,
                "engine": raw_eng,
                "model": model_raw,
                "backend": backend,
                "stage": "preflight",
                "layer": "engine",
                "code": "unsupported_engine",
                "detail": format!("未知引擎: {raw_eng}"),
            }));
            continue;
        };

        let Some(runtime_key) = runtime_api::default_runtime_key(&norm_eng) else {
            failures.push(json!({
                "profile_id": pid,
                "engine": raw_eng,
                "model": model_raw,
                "backend": backend,
                "stage": "preflight",
                "layer": "runtime",
                "code": "runtime_missing",
                "detail": format!("{norm_eng} 无可用运行时适配器"),
            }));
            continue;
        };

        let instance = match state.store.get(runtime_api::INSTANCES, &runtime_key) {
            Ok(instance) => instance.unwrap_or(Value::Null),
            Err(error) => {
                failures.push(json!({"profile_id":pid,"engine":raw_eng,"backend":backend,
                    "stage":"preflight","layer":"runtime","code":"storage.failed","detail":error.to_string()}));
                continue;
            }
        };
        let explicit_binary = instance["binary_path"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(std::path::Path::new);

        if payload["offline"] == true
            && let Err(err) = super::container_exec::check_web_tools_withholding(&norm_eng)
        {
            failures.push(json!({
                "profile_id": pid,
                "engine": raw_eng,
                "model": model_raw,
                "backend": backend,
                "stage": "preflight",
                "layer": "engine",
                "code": err.code,
                "detail": err.message,
            }));
            continue;
        }

        if let Some(img_status) = &container_img_status {
            if img_status["daemon"]["ok"].as_bool() != Some(true) {
                failures.push(json!({
                    "profile_id": pid,
                    "engine": raw_eng,
                    "model": model_raw,
                    "backend": backend,
                    "stage": "preflight",
                    "layer": "container",
                    "code": "docker_daemon_unavailable",
                    "detail": format!(
                        "Docker daemon 未就绪: {}",
                        img_status["daemon"]["detail"].as_str().unwrap_or("")
                    ),
                }));
                continue;
            }
            if img_status["pulled"]["ok"].as_bool() != Some(true) {
                failures.push(json!({
                    "profile_id": pid,
                    "engine": raw_eng,
                    "model": model_raw,
                    "backend": backend,
                    "stage": "preflight",
                    "layer": "container",
                    "code": "worker_image_missing",
                    "detail": format!(
                        "Worker 镜像未就绪: {}",
                        img_status["pulled"]["detail"].as_str().unwrap_or("")
                    ),
                }));
                continue;
            }
        } else if backend != "container"
            && let Err(err) = binaries::resolve(desc, explicit_binary).await
        {
            failures.push(json!({
                "profile_id": pid,
                "engine": raw_eng,
                "model": model_raw,
                "backend": backend,
                "stage": "preflight",
                "layer": "binary",
                "code": "cli_missing",
                "detail": err.message,
            }));
            continue;
        }

        if let Err(err) = crate::agent_runtime::version_policy::verify_worker_engine(
            state,
            &norm_eng,
            &backend,
            node,
            Some(run_mode),
            explicit_binary,
            container_img_status.as_ref(),
        )
        .await
        {
            failures.push(json!({
                "profile_id": pid,
                "engine": raw_eng,
                "model": model_raw,
                "backend": backend,
                "stage": "preflight",
                "layer": "version",
                "code": err.code,
                "detail": err.message,
            }));
            continue;
        }

        let (_, effective_cred) = resolve_effective_credential(state, &norm_eng, &account_id).await;
        if effective_cred.is_empty() {
            failures.push(json!({
                "profile_id": pid,
                "engine": raw_eng,
                "model": model_raw,
                "backend": backend,
                "stage": "preflight",
                "layer": "credential",
                "code": "credential_missing",
                "detail": format!("{norm_eng} 缺少可用凭据或本机 CLI 登录"),
            }));
            continue;
        }

        let credential_check = if backend == "container" {
            super::container_exec::validate_container_credential(
                state,
                &norm_eng,
                &effective_cred,
                model.as_deref(),
            )
            .await
        } else {
            credentials::resolve(
                &state.store,
                &native_chat::state_dir(state),
                desc,
                &effective_cred,
                model.as_deref(),
            )
            .await
            .map(|_| ())
        };
        if let Err(err) = credential_check {
            failures.push(json!({
                "profile_id": pid,
                "engine": raw_eng,
                "model": model_raw,
                "backend": backend,
                "stage": "preflight",
                "layer": "credential",
                "code": err.code,
                "detail": err.message,
            }));
            continue;
        }

        if payload["cost_budget_usd"].as_f64().unwrap_or(0.0) > 0.0 {
            let quote = crate::pricing_read::resolve(
                &state.store,
                &[json!({"engine":norm_eng,"model":model_raw,"credential_id":effective_cred})],
            );
            if !quote.is_ok_and(|quote| !quote["items"][0]["rates"].is_null()) {
                failures.push(json!({"profile_id":pid,"engine":raw_eng,"model":model_raw,"backend":backend,
                    "stage":"preflight","layer":"pricing","code":"pricing_unavailable","detail":"所选 Worker 模型缺少精确美元单价；配置价格或移除美元预算"}));
                continue;
            }
        }
        let network = payload["worker_network"]
            .as_str()
            .or_else(|| saved_cfg["worker_network"].as_str())
            .unwrap_or("host");
        let mut probe_settings = saved_cfg.clone();
        probe_settings["worker_backend"] = json!(backend);
        probe_settings["worker_node_id"] = json!(node);
        probe_settings["worker_network"] = json!(network);
        probe_settings["mode"] = json!(run_mode);
        if !(backend == "container" && node != "local")
            && let Err(error) = super::readiness::check(
                state,
                profile,
                &backend,
                network,
                &effective_cred,
                &runtime_key,
                &probe_settings,
            )
            .await
        {
            failures.push(json!({"profile_id":pid,"engine":raw_eng,"model":model_raw,"backend":backend,
                "network":if backend == "container"{network}else{""},"stage":"preflight","layer":"model","code":error.code,"detail":error.message}));
            continue;
        }

        let adapter_id = runtime_key
            .split(':')
            .next()
            .unwrap_or(&runtime_key)
            .to_owned();
        let instance_id = runtime_key
            .split(':')
            .nth(1)
            .unwrap_or("default")
            .to_owned();

        let ordinary_capacity = if profile_allows_role(profile, "explore", &saved_cfg) {
            profile_role_capacity(profile, "explore", &saved_cfg, max_seats)
        } else {
            0
        };
        let review_capacity = if profile_allows_role(profile, "review", &saved_cfg) {
            profile_role_capacity(profile, "review", &saved_cfg, max_seats)
        } else {
            0
        };
        let verifier_capacity = if profile_allows_role(profile, "verifier", &saved_cfg) {
            profile_role_capacity(profile, "verifier", &saved_cfg, max_seats)
        } else {
            0
        };
        let seat_capacity = ordinary_capacity
            .saturating_add(review_capacity)
            .saturating_add(verifier_capacity)
            .min(max_seats);

        let mut bucket = Vec::with_capacity(seat_capacity);
        for _ in 0..seat_capacity {
            bucket.push(WorkerSpec {
                solver_id: String::new(),
                profile_id: pid.clone(),
                profile_label: label.clone(),
                raw_engine: cli_engine_id(&norm_eng),
                norm_engine: norm_eng.clone(),
                adapter_id: adapter_id.clone(),
                instance_id: instance_id.clone(),
                model: model.clone(),
                effort: effort.clone(),
                account_id: account_id.clone(),
                effective_credential_id: effective_cred.clone(),
                backend: backend.clone(),
                profile: profile.clone(),
            });
        }
        if !bucket.is_empty() {
            profile_specs.push(bucket);
        }
    }

    let mut valid_specs = Vec::new();
    while valid_specs.len() < max_seats {
        let mut added = false;
        for bucket in &mut profile_specs {
            if let Some(spec) = bucket.pop() {
                valid_specs.push(spec);
                added = true;
            }
            if valid_specs.len() == max_seats {
                break;
            }
        }
        if !added {
            break;
        }
    }

    if valid_specs.is_empty() {
        let msg = failures
            .first()
            .and_then(|f| f["detail"].as_str())
            .unwrap_or("所有已配置的 Worker 预检均未通过")
            .to_owned();
        return Err((msg, failures));
    }

    let seat_count = max_seats.min(valid_specs.len());
    let mut specs = Vec::with_capacity(seat_count);
    for idx in 0..seat_count {
        let mut base = valid_specs[idx % valid_specs.len()].clone();
        let count = engine_counts.entry(base.norm_engine.clone()).or_insert(0);
        *count += 1;
        let base_solver = format!("cli-{}#{}", base.norm_engine, *count);
        base.solver_id = if generation > 1 {
            format!("{base_solver}-g{generation}")
        } else {
            base_solver
        };
        specs.push(base);
    }

    Ok((specs, failures))
}

pub fn stage_workspace(
    session_dir: &Path,
    solver_id: &str,
    norm_engine: &str,
    challenge: &Value,
) -> std::io::Result<(PathBuf, PathBuf, PathBuf)> {
    match challenge["mode"].as_str() {
        None | Some("ctf") => ctf::stage_workspace(session_dir, solver_id, norm_engine, challenge),
        Some("pentest") => {
            pentest_mode::stage_workspace(session_dir, solver_id, norm_engine, challenge)
        }
        _ => Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "Unknown task mode",
        )),
    }
}

pub fn build_worker_prompt(
    challenge: &Value,
    blackboard_script: &Path,
    reopen_hint: Option<&str>,
) -> String {
    build_worker_prompt_for_intent(challenge, blackboard_script, None, None, reopen_hint)
}

pub fn build_worker_prompt_for_intent(
    challenge: &Value,
    blackboard_script: &Path,
    assignment: Option<&IntentAssignment>,
    board_snapshot: Option<&str>,
    reopen_hint: Option<&str>,
) -> String {
    match challenge["mode"].as_str() {
        None | Some("ctf") => ctf::build_worker_prompt_for_intent(
            challenge,
            blackboard_script,
            assignment,
            board_snapshot,
            reopen_hint,
        ),
        Some("pentest") => pentest_mode::build_worker_prompt_for_intent(
            challenge,
            blackboard_script,
            assignment,
            board_snapshot,
            reopen_hint,
        ),
        _ => unreachable!("task mode must be admitted before prompt assembly"),
    }
}

pub(super) fn persist_graph_context(
    state: &AppState,
    run_id: &str,
    text: &str,
) -> Result<(), String> {
    let shared = session_dir(state, run_id).join("shared");
    let live = shared.join("live");
    let directory = if live.is_dir() { live } else { shared };
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    write_atomic_bytes(&directory.join("graph.yaml"), text.as_bytes()).map_err(|e| e.to_string())
}

pub(super) fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("Artifact parent missing"))?;
    std::fs::create_dir_all(parent)?;
    write_atomic_bytes_no_create(path, bytes)
}

pub(super) fn write_atomic_bytes_no_create(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("Artifact parent missing"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(in crate::runs) fn write_atomic_json(path: &Path, value: &Value) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(value)?)?;
    std::fs::rename(&tmp, path)
}

fn write_worker_assignment_record(
    session_dir: &Path,
    solver_id: &str,
    role: &str,
    execution_mode: &str,
    intent_id: &str,
    generation: u64,
) {
    let dir = session_dir.join(".muteki-agent-state/worker_assignments");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = write_atomic_json(
            &dir.join(format!("{solver_id}.json")),
            &json!({
                "worker_id": solver_id,
                "role": role,
                "execution_mode": execution_mode,
                "intent_id": intent_id,
                "generation": generation,
            }),
        );
    }
}

fn resolve_server_worker_context(
    state: &AppState,
    run_id: &str,
    solver_id: &str,
    session_dir: &Path,
    req: &Value,
) -> (String, bool, Option<String>) {
    let normalize_role = |r: &str| -> String {
        match r.trim().to_ascii_lowercase().as_str() {
            "review" | "reviewer" => "review".to_owned(),
            "verifier" | "verify" => "verifier".to_owned(),
            _ => "explore".to_owned(),
        }
    };

    let mut server_role: Option<String> = None;
    let mut campaign_mode = false;
    let mut active_intent_id: Option<String> = None;

    if let Ok(graph) = super::graph::SharedGraph::open(state, run_id) {
        if let Some(claimed) = graph.all_intents().into_iter().find(|intent| {
            intent["worker"].as_str() == Some(solver_id)
                && intent["status"] == "claimed"
                && intent["dispatch_state"] == "active"
        }) {
            let wc = claimed["worker_class"].as_str().unwrap_or("code");
            server_role = Some(role_for_worker_class(wc).to_owned());
            active_intent_id = claimed["intent_id"].as_str().map(str::to_owned);
        }
    }

    let assign_path = session_dir
        .join(".muteki-agent-state/worker_assignments")
        .join(format!("{solver_id}.json"));
    if let Ok(raw) = std::fs::read_to_string(&assign_path)
        && let Ok(doc) = serde_json::from_str::<Value>(&raw)
    {
        if server_role.is_none()
            && let Some(r) = doc["role"].as_str()
        {
            server_role = Some(normalize_role(r));
        }
        if doc["execution_mode"].as_str() == Some("campaign") {
            campaign_mode = true;
        }
        if active_intent_id.is_none() {
            active_intent_id = doc["intent_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
        }
    }

    let env_path = session_dir.join(".muteki-skills/muteki-blackboard/env.json");
    if let Ok(raw) = std::fs::read_to_string(&env_path)
        && let Ok(env_doc) = serde_json::from_str::<Value>(&raw)
    {
        if server_role.is_none()
            && let Some(r) = env_doc["MUTEKI_BLACKBOARD_ROLE"].as_str()
        {
            let norm = normalize_role(r);
            if norm != "explore" || req.get("role").is_none() {
                server_role = Some(norm);
            }
        }
        if env_doc["MUTEKI_WORKER_EXECUTION_MODE"].as_str() == Some("campaign") {
            campaign_mode = true;
        }
    }

    if req["execution_mode"].as_str() == Some("campaign")
        || req["payload"]["campaign_mode"].as_bool() == Some(true)
    {
        campaign_mode = true;
    }

    let role = server_role.unwrap_or_else(|| {
        req["role"]
            .as_str()
            .or_else(|| req["payload"]["role"].as_str())
            .map(normalize_role)
            .unwrap_or_else(|| "explore".to_owned())
    });

    (role, campaign_mode, active_intent_id)
}

pub fn drain_blackboard_ingress(
    state: &AppState,
    run_id: &str,
    solver_id: &str,
    ingress_dir: &Path,
    session_dir: &Path,
) -> (bool, bool) {
    match state.store.get("runs", run_id) {
        Ok(Some(run)) => match run["mode"].as_str() {
            None | Some("ctf") => {
                ctf::drain_blackboard_ingress(state, run_id, solver_id, ingress_dir, session_dir)
            }
            Some("pentest") => pentest_mode::drain_blackboard_ingress(
                state,
                run_id,
                solver_id,
                ingress_dir,
                session_dir,
            ),
            _ => {
                emit_event(
                    state,
                    run_id,
                    Some(solver_id),
                    "worker.error",
                    json!({"reason":"unsupported_mode"}),
                );
                (false, true)
            }
        },
        result => {
            emit_event(
                state,
                run_id,
                Some(solver_id),
                "worker.error",
                json!({"reason":"run_unavailable","detail":format!("{result:?}")}),
            );
            (false, true)
        }
    }
}

fn store_artifact(
    session_dir: &Path,
    seq: u64,
    command: &str,
    output: &str,
) -> std::io::Result<String> {
    let arts_dir = session_dir.join("arts");
    std::fs::create_dir_all(&arts_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&arts_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut candidate = seq.max(1);
    loop {
        let art_id = format!("art-{candidate}");
        if std::fs::read_dir(&arts_dir)?.flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{art_id}."))
        }) {
            candidate = candidate
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("artifact id space exhausted"))?;
            continue;
        }
        let path = arts_dir.join(format!("{art_id}.json"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                let doc = json!({
                    "artifact_id": art_id,
                    "seq": candidate,
                    "command": command,
                    "content": output,
                    "ts": now_secs(),
                });
                if let Err(error) = file
                    .write_all(doc.to_string().as_bytes())
                    .and_then(|_| file.sync_all())
                {
                    let _ = std::fs::remove_file(&path);
                    return Err(error);
                }
                return Ok(art_id);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                candidate = candidate
                    .checked_add(1)
                    .ok_or_else(|| std::io::Error::other("artifact id space exhausted"))?;
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn store_poc_image(
    session_dir: &Path,
    solver_id: &str,
    raw_path: &str,
    _name: &str,
    seq: u64,
) -> Result<Value, String> {
    let extension = Path::new(raw_path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        _ => return Ok(Value::Null),
    };
    let base = session_dir.canonicalize().map_err(|e| e.to_string())?;
    let given = Path::new(raw_path);
    let shared_root = Path::new(super::container_exec::SHARED_CONTAINER_WORKSPACE).join(
        session_dir
            .file_name()
            .ok_or("Run workspace identity missing")?,
    );
    let source = if let Ok(relative) = given.strip_prefix(&shared_root) {
        base.join(relative)
    } else if let Ok(relative) = given.strip_prefix(super::container_exec::CONTAINER_WORKSPACE) {
        base.join(relative)
    } else if given.is_absolute() {
        given.to_path_buf()
    } else {
        base.join("workers").join(solver_id).join(given)
    };
    let source = source
        .canonicalize()
        .map_err(|_| "图片 PoC 文件不存在".to_owned())?;
    if !source.starts_with(&base) || !source.is_file() {
        return Err("图片 PoC 不在本次运行工作区".to_owned());
    }
    let relative = source.strip_prefix(&base).map_err(|e| e.to_string())?;
    let bytes = super::evidence::bounded_file(&base, relative, 64 * 1024 * 1024)?;
    if crate::image_validation::mime(&bytes)? != mime {
        return Err("图片 PoC 内容与格式不符".into());
    }
    let sidecar = relative.with_file_name(format!(
        "{}.muteki-browser.json",
        source.file_name().unwrap().to_string_lossy()
    ));
    let browser_capture = super::evidence::bounded_file(&base, &sidecar, 1024 * 1024)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|capture| {
            capture["schema"] == "muteki-browser-screenshot-v1" && capture["worker_id"] == solver_id
        });
    let root = session_dir.join("arts");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let mut candidate = seq.max(1);
    loop {
        let artifact_id = format!("art-{candidate}");
        if std::fs::read_dir(&root)
            .map_err(|e| e.to_string())?
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{artifact_id}."))
            })
        {
            candidate = candidate
                .checked_add(1)
                .ok_or_else(|| "证据工件 ID 耗尽".to_owned())?;
            continue;
        }
        let filename = format!("{artifact_id}.{extension}");
        let path = root.join(&filename);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(error.to_string());
                }
                return Ok(json!({"artifact_id":artifact_id,"filename":filename,
                    "size":bytes.len(),"sha256":({use sha2::{Digest,Sha256};format!("{:x}",Sha256::digest(&bytes))}),"mime_type":mime,
                    "browser_capture":browser_capture}));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                candidate = candidate
                    .checked_add(1)
                    .ok_or_else(|| "证据工件 ID 耗尽".to_owned())?;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn stage_run_browser(session_dir: &Path, run_id: &str) -> Result<(), String> {
    let root = session_dir.join(".muteki-browser");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).map_err(|error| error.to_string())?;
    let stage_lock = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("stage.lock"))
        .map_err(|error| error.to_string())?;
    fs2::FileExt::lock_exclusive(&stage_lock).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(root.join("home")).map_err(|error| error.to_string())?;
    let config_path = root.join("run.json");
    let browser_namespace = uuid::Uuid::new_v4().simple().to_string();
    let config = json!({"run_id":run_id,
        "browser_namespace":&browser_namespace[..12]});
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_path)
    {
        Ok(mut file) => file
            .write_all(config.to_string().as_bytes())
            .map_err(|error| error.to_string())?,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.to_string()),
    }
    #[cfg(unix)]
    {
        let shim = bin.join("agent-browser");
        if shim.symlink_metadata().is_err() {
            std::os::unix::fs::symlink("/opt/muteki/browser_coord.py", &shim)
                .map_err(|error| error.to_string())?;
        }
    }
    let skill_dir = session_dir.join(".muteki-skills/agent-browser");
    std::fs::create_dir_all(&skill_dir).map_err(|error| error.to_string())?;
    let skill_path = skill_dir.join("SKILL.md");
    if !skill_path.exists() {
        std::fs::write(
            &skill_path,
            include_str!("../../../../docker/worker-common/agent-browser.SKILL.md"),
        )
        .map_err(|error| error.to_string())?;
    }
    for prefix in [
        ".agents/skills",
        ".claude/skills",
        ".codex/skills",
        ".cursor/skills",
        ".pi/skills",
        ".omp/skills",
        ".kimi/skills",
        ".grok/skills",
        ".opencode/skills",
    ] {
        let parent = session_dir.join(prefix);
        std::fs::create_dir_all(&parent).map_err(|error| error.to_string())?;
        let link = parent.join("agent-browser");
        #[cfg(unix)]
        if link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink("../../.muteki-skills/agent-browser", &link)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

async fn clear_stale_browser_profile_lock(
    run_id: &str,
    session_dir: &Path,
    container_name: &str,
) -> Result<(), String> {
    if let Some(socket) = crate::runner::configured_socket() {
        crate::runner::exchange(
            &socket,
            &format!("/v1/containers/{run_id}/browser-profile/recover"),
            Some(json!({})),
        )
        .await?;
        return Ok(());
    }
    let root = session_dir.to_path_buf();
    let name = container_name.to_owned();
    tokio::task::spawn_blocking(move || clear_stale_browser_profile_lock_local(&root, &name))
        .await
        .map_err(|e| e.to_string())?
}

pub(crate) fn clear_stale_browser_profile_lock_local(
    session_dir: &Path,
    container_name: &str,
) -> Result<(), String> {
    let browser_dir = session_dir.join(".muteki-browser");
    let stage_lock = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(browser_dir.join("stage.lock"))
        .map_err(|error| error.to_string())?;
    fs2::FileExt::lock_exclusive(&stage_lock).map_err(|error| error.to_string())?;
    let profile = session_dir.join(".muteki-browser-profile");
    let lock = profile.join("SingletonLock");
    if !lock.is_symlink() {
        return Ok(());
    }
    let holder = std::fs::read_link(&lock).map_err(|error| error.to_string())?;
    let holder = holder.to_string_lossy();
    let (holder_host, holder_pid) = holder
        .rsplit_once('-')
        .ok_or("浏览器 Profile 锁的持有者无法核验")?;
    if holder_host.len() != 12
        || !holder_host.bytes().all(|byte| byte.is_ascii_hexdigit())
        || holder_pid.is_empty()
        || !holder_pid.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("浏览器 Profile 锁的持有者无法核验".to_owned());
    }
    let current = crate::host_process::std_command("docker")
        .args([
            "inspect",
            container_name,
            "--format",
            "{{.Config.Hostname}}",
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !current.status.success() {
        return Err("无法核验当前 Run 容器的浏览器所有权".to_owned());
    }
    let current_host = String::from_utf8_lossy(&current.stdout);
    if current_host.trim() == holder_host {
        return Ok(());
    }
    let listed = crate::host_process::std_command("docker")
        .args(["ps", "-a", "--no-trunc", "--format", "{{.ID}} {{.State}}"])
        .output()
        .map_err(|error| error.to_string())?;
    if !listed.status.success() {
        return Err("无法核验上一个浏览器容器是否仍在运行".to_owned());
    }
    let holders: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(id, _)| id.starts_with(holder_host))
        .map(|(_, state)| state.to_owned())
        .collect();
    if holders.len() > 1
        || holders
            .iter()
            .any(|state| !matches!(state.as_str(), "exited" | "dead" | "created"))
    {
        return Err("上一个浏览器容器仍在运行，不能解锁共享 Profile".to_owned());
    }
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let candidate = profile.join(name);
        if candidate.is_symlink() {
            std::fs::remove_file(candidate).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub fn spawn_local_worker(
    state: Arc<AppState>,
    run_id: String,
    spec: WorkerSpec,
    challenge: Value,
    reopen_hint: Option<String>,
    step_index: usize,
) -> tokio::task::AbortHandle {
    match challenge["mode"].as_str() {
        None | Some("ctf") => {
            ctf::spawn_local_worker(state, run_id, spec, challenge, reopen_hint, step_index)
        }
        Some("pentest") => pentest_mode::spawn_local_worker(
            state,
            run_id,
            spec,
            challenge,
            reopen_hint,
            step_index,
        ),
        _ => unreachable!("task mode must be admitted before Worker dispatch"),
    }
}

pub struct SpawnedWorker {
    pub abort: tokio::task::AbortHandle,
    pub cancel: Option<ContainerCancelRequest>,
    pub host_cancel: Option<Arc<AtomicBool>>,
    pub exit_reason: Arc<Mutex<Option<String>>>,
}

#[cfg(test)]
pub(super) struct HostStartGate {
    pub solver_id: String,
    pub arrived: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

#[cfg(test)]
pub(super) static HOST_START_GATE: std::sync::OnceLock<Arc<HostStartGate>> =
    std::sync::OnceLock::new();

pub fn spawn_local_worker_with_intent(
    state: Arc<AppState>,
    run_id: String,
    mut spec: WorkerSpec,
    challenge: Value,
    assignment: IntentAssignment,
    finalize_on_exit: bool,
) -> SpawnedWorker {
    super::usage::freeze_price(&state, &mut spec);
    let solver_id = spec.solver_id.clone();
    let container = spec.backend == "container";
    let (cancel_tx, cancel_rx) = if container {
        let (tx, rx) = oneshot::channel();
        (Some(Arc::new(Mutex::new(Some(tx)))), Some(rx))
    } else {
        (None, None)
    };
    let host_cancel = Arc::new(AtomicBool::new(false));
    let host_cancel_for_task = host_cancel.clone();
    let exit_reason = Arc::new(Mutex::new(None));
    let exit_reason_for_task = exit_reason.clone();
    let exit_reason_for_completion = exit_reason.clone();
    let engine = spec.norm_engine.clone();
    let intent_id = assignment.intent_id.clone();
    let thread_id = format!("run:{run_id}:{solver_id}:{}", uuid::Uuid::new_v4().simple());
    let state_for_task = state.clone();
    let run_id_for_task = run_id.clone();
    let thread_for_task = thread_id.clone();
    let completion_state = state.clone();
    let completion_run_id = run_id.clone();
    let completion_solver_id = solver_id.clone();
    let completion_intent_id = assignment.intent_id.clone();

    let join = tokio::spawn(async move {
        match challenge["mode"].as_str() {
            None | Some("ctf") => {
                ctf::execute_worker_task(
                    state_for_task,
                    run_id_for_task,
                    spec,
                    thread_for_task,
                    challenge,
                    assignment,
                    finalize_on_exit,
                    cancel_rx,
                    host_cancel_for_task,
                    exit_reason_for_task,
                )
                .await
            }
            Some("pentest") => {
                pentest_mode::execute_worker_task(
                    state_for_task,
                    run_id_for_task,
                    spec,
                    thread_for_task,
                    challenge,
                    assignment,
                    finalize_on_exit,
                    cancel_rx,
                    host_cancel_for_task,
                    exit_reason_for_task,
                )
                .await
            }
            _ => unreachable!("task mode must be admitted before Worker execution"),
        }
        if finalize_on_exit
            && let Ok(Some(run)) = completion_state.store.get("runs", &completion_run_id)
            && let Ok(graph) =
                super::graph::SharedGraph::open(&completion_state, &completion_run_id)
            && graph.all_intents().iter().any(|intent| {
                intent["intent_id"].as_str() == Some(completion_intent_id.as_str())
                    && intent["worker"].as_str() == Some(completion_solver_id.as_str())
                    && intent["status"] == "claimed"
            })
        {
            let recorded_exit = exit_reason_for_completion.lock().unwrap().clone();
            let reason = run["outcome_reason"].as_str().unwrap_or("worker_finished");
            if matches!(reason, "operator_stop" | "intent_dropped")
                || recorded_exit.as_deref() == Some("canceled")
            {
                if let Err(error) = super::worker_result::salvage(
                    &completion_state,
                    &completion_run_id,
                    &completion_solver_id,
                    &completion_intent_id,
                    "canceled",
                    reason,
                ) {
                    emit_event(
                        &completion_state,
                        &completion_run_id,
                        Some(&completion_solver_id),
                        "worker.error",
                        json!({"reason":"worker_result_salvage_failed","detail":error}),
                    );
                }
            } else if matches!(run["status"].as_str(), Some("finished" | "solved")) {
                let result = if run["solved"] == true {
                    "solved"
                } else if let Some(ref ex) = recorded_exit {
                    ex.as_str()
                } else if matches!(reason, "runtime_failure" | "preflight_failed") {
                    "failed"
                } else {
                    "done"
                };
                if let Err(error) = super::worker_result::salvage(
                    &completion_state,
                    &completion_run_id,
                    &completion_solver_id,
                    &completion_intent_id,
                    result,
                    reason,
                ) {
                    emit_event(
                        &completion_state,
                        &completion_run_id,
                        Some(&completion_solver_id),
                        "worker.error",
                        json!({"reason":"worker_result_salvage_failed","detail":error}),
                    );
                }
            }
        }
    });
    let abort = join.abort_handle();
    state.runs.register_worker(
        &run_id,
        ActiveWorker {
            solver_id: solver_id.clone(),
            thread_id: thread_id.clone(),
            abort: abort.clone(),
            engine,
            intent_id,
            lane: String::new(),
            cancel: cancel_tx.clone(),
            host_cancel: (!container).then_some(host_cancel.clone()),
        },
    );
    SpawnedWorker {
        abort,
        cancel: cancel_tx,
        host_cancel: (!container).then_some(host_cancel),
        exit_reason,
    }
}

fn summarize_tool_item(content: &ItemContent) -> Option<(String, Option<String>)> {
    match content {
        ItemContent::CommandExecution {
            command, output, ..
        } => Some((command.clone(), Some(output.clone()))),
        ItemContent::FileChange { changes, .. } => {
            let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
            Some((format!("file_change: {}", paths.join(", ")), None))
        }
        ItemContent::ToolCall {
            name,
            input,
            output,
            ..
        } => {
            let arg_preview = input
                .as_object()
                .and_then(|o| o.iter().next())
                .map(|(k, v)| format!("{k}={v}"))
                .unwrap_or_default();
            let title = if arg_preview.is_empty() {
                name.clone()
            } else {
                format!("{name}({arg_preview})")
            };
            Some((title, output.clone()))
        }
        _ => None,
    }
}
