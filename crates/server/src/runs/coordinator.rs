//! Mode routing and shared execution/FGS data structures.
//! CTF and Pentest own independent coordinator state, planning and finalization.
//!

use crate::{
    AppState,
    runs::{
        ctf_planner, emit_event,
        graph::{SharedGraph, canonical_fact_whitespace, normalize_lane_key, normalize_route_hash},
        mark_run_finished, now_secs, pentest, session_dir,
        worker::{self, IntentAssignment, WorkerSpec, spawn_local_worker_with_intent},
    },
};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    sync::{Arc, OnceLock, atomic::Ordering},
    time::{Duration, Instant},
};

#[path = "ctf/coordinator.rs"]
mod ctf;
#[path = "pentest/coordinator.rs"]
mod pentest_driver;

pub(crate) use ctf::dispatch_intents as dispatch_ctf_intents;
pub(crate) use pentest_driver::pentest_planning_trigger;
#[cfg(test)]
use pentest_driver::{pentest_requested_intents,commit_pentest_reason_plan};

pub const BARREN_WORKER_LIMIT: usize = 8;

pub const REASON_SYSTEM: &str = "\
你负责读取完整 Fact-Goal-Step 图并决定下一步，不执行任务。

只返回 JSON：
{\"verdict\":\"explore|complete\",\"goal_met\":false,\"complete_why\":\"\",
\"progress\":{\"summary\":\"\",\"confirmed\":[],\"active\":[],\"blocked\":[],\"next\":[]},\"intents\":[
{\"id\":\"I1\",\"from\":[],\"goal\":\"当前可执行的方向\",\"priority\":\"high|normal|low\",
\"expected_observable\":\"本 Step 可观察到的结果\",\"stop_condition\":\"有界且可检验的停止条件\",
\"coverage_key\":\"范围与假设对应的稳定标识\"}]}

规则：
- Goal 已由图中的 Fact 或平台结果满足时返回 complete。
- 尚未完成时创建{max_intents_instruction}当前可执行、尽量互补的 Step。
- Step 只描述要向目标取的那条信息，不写分步方法；一个 Step 只产出一个可独立交接的结果。
- 同批 Step 没有先后顺序，全部前置必须已经是 Fact。后续动作依赖尚未形成 Fact 的共享入口时，留到下一轮再创建。
- from 可以引用相关 Fact id；冷启动时可以为空。
- 每个 Step 必须写 expected_observable、stop_condition 和 coverage_key。
- progress.summary 使用简体中文概括当前进展和下一步。
- 只输出 JSON。";

#[derive(Debug, Clone)]
pub struct PlannedIntent {
    pub model_id: String,
    pub goal: String,
    pub worker_class: String,
    pub priority: String,
    pub from_facts: Vec<i64>,
    pub depends_on: Vec<String>,
    pub route_hash: String,
    pub branch_id: String,
    pub lane_key: String,
    pub risk_class: String,
    pub resource_key: String,
    pub expected_observable: String,
    pub stop_condition: String,
    pub coverage_key: String,
    pub required_pocs: Vec<String>,
    pub rationale: String,
    pub initial: bool,
}

#[derive(Debug, Clone)]
pub struct ReasonResult {
    pub verdict: String,
    pub goal_met: bool,
    pub complete_why: String,
    pub drift: String,
    pub progress_summary: String,
    pub progress_sections: HashMap<String, Vec<String>>,
    pub intents: Vec<PlannedIntent>,
    pub supersede_intents: Vec<String>,
    pub supersede_why: String,
    pub reprioritize_intents: HashMap<String, String>,
    pub pinned_facts: Vec<i64>,
    pub parse_status: String,
    pub parse_detail: String,
    pub planner_failure: Option<(String, String)>,
}

pub fn build_reason_prompt(graph_yaml: &str, max_intents: isize) -> (String, String) {
    let instruction = if max_intents < 0 {
        "任意数量的".to_owned()
    } else if max_intents == 0 {
        "零个新的".to_owned()
    } else {
        format!("最多 {max_intents} 个")
    };
    let system = REASON_SYSTEM.replace("{max_intents_instruction}", &instruction);
    let user = format!("共享求解图：\n\n{graph_yaml}\n\n只输出规划 JSON。");
    (system, user)
}

fn extract_json_object(raw: &str) -> Result<Value, (String, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err((
            "empty_response".to_owned(),
            "planner response was empty".to_owned(),
        ));
    }
    static RE_FENCE: OnceLock<Regex> = OnceLock::new();
    let re_fence = RE_FENCE.get_or_init(|| Regex::new(r"```(?:json)?").unwrap());
    let cleaned = re_fence.replace_all(trimmed, "");
    let Some(start) = cleaned.find('{') else {
        return Err((
            "json_object_not_found".to_owned(),
            "no JSON object found in planner response".to_owned(),
        ));
    };
    let Some(end) = cleaned.rfind('}') else {
        return Err((
            "json_object_not_found".to_owned(),
            "no closing brace found in planner response".to_owned(),
        ));
    };
    if end < start {
        return Err((
            "json_object_not_found".to_owned(),
            "invalid brace positions in planner response".to_owned(),
        ));
    }
    let slice = &cleaned[start..=end];
    match serde_json::from_str::<Value>(slice) {
        Ok(val) if val.is_object() => Ok(val),
        Ok(_) => Err((
            "json_root_not_object".to_owned(),
            "planner JSON root was not an object".to_owned(),
        )),
        Err(err) => Err(("json_decode_error".to_owned(), err.to_string())),
    }
}

pub fn parse_reason_reply(raw_text: &str, max_intents: isize) -> ReasonResult {
    let (d, mut parse_status, mut parse_detail) = match extract_json_object(raw_text) {
        Ok(val) => (val, "parsed".to_owned(), String::new()),
        Err((st, det)) => (json!({}), st, det),
    };

    let plan_is_valid = d.get("intents").is_some_and(Value::is_array);
    if parse_status == "parsed" && !plan_is_valid {
        parse_status = "schema_invalid".to_owned();
        parse_detail = "planner JSON did not contain the required intents array".to_owned();
    }

    let mut goal_met = d["goal_met"].as_bool().unwrap_or(false);
    let raw_intents = d["intents"].as_array().cloned().unwrap_or_default();
    let take_count = if max_intents < 0 {
        raw_intents.len()
    } else {
        raw_intents.len().min(max_intents as usize)
    };

    let mut intents = Vec::new();
    for (idx, raw) in raw_intents.into_iter().take(take_count).enumerate() {
        let Some(obj) = raw.as_object() else {
            continue;
        };
        let goal = obj
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if goal.is_empty() {
            continue;
        }
        let wc_raw = obj
            .get("worker_class")
            .and_then(Value::as_str)
            .unwrap_or("code")
            .trim();
        let worker_class = match wc_raw {
            "code" | "shell_agent" | "verifier" | "review" => wc_raw.to_owned(),
            _ => "code".to_owned(),
        };
        let prio_raw = obj
            .get("priority")
            .and_then(Value::as_str)
            .unwrap_or("normal")
            .trim()
            .to_ascii_lowercase();
        let priority = match prio_raw.as_str() {
            "high" | "normal" | "low" => prio_raw,
            _ => "normal".to_owned(),
        };
        let from_facts: Vec<i64> = obj
            .get("from")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .filter(|&n| n > 0)
            .collect();
        let depends_on: Vec<String> = obj
            .get("depends_on")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned))
            .collect();
        let route_hash = normalize_route_hash(
            obj.get("route_hash")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        let mut coverage_key = obj
            .get("coverage_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if coverage_key.is_empty() && worker_class != "review" {
            coverage_key = if !route_hash.is_empty() {
                route_hash.clone()
            } else {
                format!("step-cov-{}", idx + 1)
            };
        }
        let expected_observable = obj
            .get("expected_observable")
            .and_then(Value::as_str)
            .unwrap_or("Concrete command output or verified artifact")
            .trim()
            .to_owned();
        let stop_condition = obj
            .get("stop_condition")
            .and_then(Value::as_str)
            .unwrap_or("Stop once the target hypothesis is verified or ruled out")
            .trim()
            .to_owned();

        intents.push(PlannedIntent {
            model_id: obj
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("I{}", idx + 1)),
            goal,
            worker_class,
            priority,
            from_facts,
            depends_on,
            route_hash,
            branch_id: obj
                .get("branch_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned(),
            lane_key: normalize_lane_key(
                obj.get("lane_key").and_then(Value::as_str).unwrap_or(""),
            ),
            risk_class: obj
                .get("risk_class")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned(),
            resource_key: obj
                .get("resource_key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned(),
            expected_observable,
            stop_condition,
            coverage_key,
            required_pocs: Vec::new(),
            rationale: obj
                .get("rationale")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned(),
            initial: obj.get("initial").and_then(Value::as_bool).unwrap_or(false),
        });
    }

    let mut verdict = d["verdict"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(verdict.as_str(), "complete" | "course_correct" | "explore") {
        verdict = if goal_met {
            "complete".to_owned()
        } else {
            "explore".to_owned()
        };
    }
    if verdict == "complete" {
        goal_met = true;
    }

    let complete_why = d["complete_why"].as_str().unwrap_or("").trim().to_owned();
    let drift = d["drift"].as_str().unwrap_or("").trim().to_owned();

    let mut progress_sections: HashMap<String, Vec<String>> = HashMap::new();
    let progress_obj = d.get("progress").and_then(Value::as_object);
    for key in ["confirmed", "active", "blocked", "next"] {
        let mut items = Vec::new();
        if let Some(arr) = progress_obj
            .and_then(|p| p.get(key))
            .and_then(Value::as_array)
        {
            for v in arr.iter().filter_map(Value::as_str) {
                let norm = canonical_fact_whitespace(v);
                if !norm.is_empty() && !items.contains(&norm) {
                    items.push(norm);
                }
            }
        }
        progress_sections.insert(key.to_owned(), items);
    }
    if progress_sections.get("next").is_none_or(Vec::is_empty) {
        progress_sections.insert(
            "next".to_owned(),
            intents.iter().map(|it| it.goal.clone()).collect(),
        );
    }

    let mut progress_summary = progress_obj
        .and_then(|p| p.get("summary"))
        .and_then(Value::as_str)
        .map(canonical_fact_whitespace)
        .unwrap_or_default();
    if progress_summary.is_empty() {
        if !complete_why.is_empty() {
            progress_summary = complete_why.clone();
        } else if !intents.is_empty() {
            let goals: Vec<&str> = intents.iter().map(|i| i.goal.as_str()).collect();
            progress_summary = goals.join("；");
        }
    }

    let supersede_intents: Vec<String> = d["supersede_intents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned))
        .take(24)
        .collect();
    let supersede_why = d["supersede_why"].as_str().unwrap_or("").trim().to_owned();

    let mut reprioritize_intents = HashMap::new();
    if let Some(map) = d["reprioritize_intents"].as_object() {
        for (k, v) in map.iter().take(24) {
            if let Some(p) = v.as_str() {
                let p_low = p.trim().to_ascii_lowercase();
                if matches!(p_low.as_str(), "high" | "normal" | "low") {
                    reprioritize_intents.insert(k.trim().to_owned(), p_low);
                }
            }
        }
    }

    let pinned_facts: Vec<i64> = d["pinned_facts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .filter(|&n| n > 0)
        .collect();

    let planner_failure = if !plan_is_valid {
        Some((
            "invalid_plan".to_owned(),
            "planner reply did not contain the required JSON intents array".to_owned(),
        ))
    } else if intents.is_empty() && verdict != "complete" {
        Some((
            "empty_plan".to_owned(),
            "planner returned no executable intents".to_owned(),
        ))
    } else {
        None
    };

    ReasonResult {
        verdict,
        goal_met,
        complete_why,
        drift,
        progress_summary,
        progress_sections,
        intents,
        supersede_intents,
        supersede_why,
        reprioritize_intents,
        pinned_facts,
        parse_status,
        parse_detail,
        planner_failure,
    }
}

/// Pushes `ReasonResult` intents into `shared_graph.db` with structural deduplication.


/// Selects a healthy idle `WorkerSpec` seat for `intent_id`, preferring engines that have not
/// failed this intent and engines not yet used in the current batch (`heterogeneous dispatch`).
pub fn pick_worker_seat<'a>(
    seats: &'a [WorkerSpec],
    busy_solvers: &HashSet<String>,
    failed_engines_for_intent: Option<&HashSet<String>>,
    used_engines_in_batch: &HashSet<String>,
    cursor: &mut usize,
) -> Option<&'a WorkerSpec> {
    pick_worker_seat_matching(seats, busy_solvers, failed_engines_for_intent,
        used_engines_in_batch, cursor, |_| true)
}

fn pick_worker_seat_matching<'a>(
    seats: &'a [WorkerSpec],
    busy_solvers: &HashSet<String>,
    failed_engines_for_intent: Option<&HashSet<String>>,
    used_engines_in_batch: &HashSet<String>,
    cursor: &mut usize,
    eligible: impl Fn(&WorkerSpec) -> bool,
) -> Option<&'a WorkerSpec> {
    if seats.is_empty() {
        return None;
    }
    let n = seats.len();
    for offset in 0..n {
        let idx = (*cursor + offset) % n;
        let seat = &seats[idx];
        if !eligible(seat) { continue; }
        if busy_solvers.contains(&seat.solver_id) {
            continue;
        }
        if failed_engines_for_intent.is_some_and(|f| f.contains(&seat.norm_engine)) {
            continue;
        }
        if !used_engines_in_batch.contains(&seat.norm_engine) {
            *cursor = (idx + 1) % n;
            return Some(seat);
        }
    }
    for offset in 0..n {
        let idx = (*cursor + offset) % n;
        let seat = &seats[idx];
        if !eligible(seat) { continue; }
        if busy_solvers.contains(&seat.solver_id) {
            continue;
        }
        if failed_engines_for_intent.is_some_and(|f| f.contains(&seat.norm_engine)) {
            continue;
        }
        *cursor = (idx + 1) % n;
        return Some(seat);
    }
    for offset in 0..n {
        let idx = (*cursor + offset) % n;
        let seat = &seats[idx];
        if !eligible(seat) { continue; }
        if !busy_solvers.contains(&seat.solver_id) {
            *cursor = (idx + 1) % n;
            return Some(seat);
        }
    }
    None
}

/// The source CTF/Pentest Decide reacts to new solve results, not to its own
/// proposals, claims, attempt diagnostics, or Worker lifecycle telemetry.




#[cfg(test)]
mod pentest_reason_tests {
    use super::*;

    #[test]
    fn in_flight_pentest_replans_on_graph_result_not_scheduler_noise() {
        let events = vec![
            json!(17),
            json!({"seq":11,"kind":"intent_proposed","actor":"reason"}),
            json!({"seq":12,"kind":"intent_claimed","actor":"worker-a"}),
            json!({"seq":13,"kind":"reason_attempt","actor":"reason"}),
        ];
        assert_eq!(pentest_planning_trigger(&events, 10), None);
        let mut with_fact = events.clone();
        with_fact.push(json!({"seq":14,"kind":"fact_added","actor":"worker-a"}));
        assert_eq!(pentest_planning_trigger(&with_fact, 10), Some("ctf_graph_change"));
        with_fact.push(json!({"seq":15,"kind":"operator_directive","actor":"operator"}));
        assert_eq!(pentest_planning_trigger(&with_fact, 14), Some("operator"));
        assert_eq!(pentest_requested_intents(1, 0), 4);
        assert_eq!(pentest_requested_intents(1, 3), 2);
        assert_eq!(pentest_requested_intents(2, 4), 2);
    }
}

#[cfg(test)]
#[path = "coordinator_host_cancel_tests.rs"]
mod host_cancel_tests;

/// The generation a Worker label was allocated for; labels of generation 1 carry no suffix.
fn label_generation(label:&str)->u64 {
    label.rsplit('-').next().and_then(|tail|tail.strip_prefix('g'))
        .filter(|digits|!digits.is_empty()&&digits.chars().all(|c|c.is_ascii_digit()))
        .and_then(|digits|digits.parse().ok()).unwrap_or(1)
}

/// Workers this generation already dispatched, read back from the graph so a coordinator
/// resumed after a Web restart keeps spending the same `max_total_workers` budget.
pub(crate) fn dispatched_worker_total(graph:&SharedGraph,generation:u64)->usize {
    graph.all_intents().iter().filter_map(|row|row["worker"].as_str())
        .filter(|label|label.starts_with("cli-")&&label_generation(label)==generation.max(1))
        .collect::<HashSet<_>>().len()
}

fn allocate_worker_label(engine:&str,generation:u64,sequences:&mut HashMap<String,usize>,graph:&SharedGraph)->String {
    let sequence=sequences.entry(engine.to_owned()).or_default();
    loop {
        *sequence+=1;
        let base=if *sequence==1{format!("cli-{engine}")}else{format!("cli-{engine}-{sequence}")};
        let label=if generation>1{format!("{base}-g{generation}")}else{base};
        if !graph.all_intents().iter().any(|row|row["worker"]==label){return label;}
    }
}

struct ActiveAssignment {
    seat_id: String,
    solver_id: String,
    norm_engine: String,
    profile_id: String,
    worker_role: String,
    intent_id: String,
    lane_key: String,
    resource_key: String,
    fact_count_at_start: usize,
    abort: tokio::task::AbortHandle,
    cancel: Option<super::ContainerCancelRequest>,
    host_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

pub(crate) fn resolve_reaped_worker_exit_reason(
    state: &AppState,
    run_id: &str,
    graph: &SharedGraph,
    solver_id: &str,
    intent_id: &str,
    exit_slot: Option<&Arc<std::sync::Mutex<Option<String>>>>,
    is_solved: bool,
) -> String {
    if is_solved {
        return "solved".to_owned();
    }
    if let Some(slot) = exit_slot
        && let Some(recorded) = slot.lock().unwrap().clone()
        && !recorded.trim().is_empty()
    {
        return recorded;
    }
    let mut latest = None;
    for event in super::read_jsonl_event_iter(state, run_id) {
        if event["solver_id"].as_str() != Some(solver_id) {
            continue;
        }
        match event["event_type"].as_str() {
            Some("worker.finished" | "worker.status") => {
                if let Some(ex) = event["payload"]["exit_reason"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                {
                    latest = Some(ex.to_owned());
                    continue;
                }
                if let Some(reason) = event["payload"]["reason"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                {
                    latest = Some(
                        match reason {
                            "solved" => "solved",
                            "timeout" => "timeout",
                            "oom" => "oom",
                            "vendor_error" => "vendor_error",
                            "intent_dropped" | "canceled" | "operator_stop" => "canceled",
                            "runtime_failure" | "preflight_failed" | "failed" => "failed",
                            _ => "done",
                        }
                        .to_owned(),
                    );
                }
            }
            Some("worker.error") => {
                let detail = event["payload"]["detail"].as_str();
                let reason = event["payload"]["reason"].as_str().unwrap_or("failed");
                latest = Some(
                    if matches!(reason, "timeout" | "oom" | "vendor_error" | "canceled") {
                        reason.to_owned()
                    } else {
                        worker::classify_worker_exit_reason(false, false, false, false, detail)
                            .to_owned()
                    },
                );
            }
            _ => {}
        }
    }
    if let Some(found) = latest {
        return found;
    }
    if let Some(intent) = graph
        .all_intents()
        .into_iter()
        .find(|row| row["intent_id"].as_str() == Some(intent_id))
        && let Some(existing_res) = intent["result"].as_str().filter(|s| !s.is_empty())
    {
        return existing_res.to_owned();
    }
    "done".to_owned()
}



































pub(super) fn publish_intent_proposals(state: &AppState, run_id: &str, proposed: &[Value], actor: &str) {
    for prop in proposed {
        emit_event(state, run_id, Some(actor), "blackboard.delta", json!({
            "kind":"intent_proposed","actor":actor,"intent_id":prop["intent_id"],"goal":prop["goal"],
            "worker_class":prop["worker_class"],"priority":prop["priority"],
            "from_facts":prop["from_facts"],"depends_on":prop["depends_on"],
            "required_pocs":prop["required_pocs"],"route_hash":prop["route_hash"],
            "lane_key":prop["lane_key"],
            "expected_observable":prop["expected_observable"],
            "stop_condition":prop["stop_condition"],"coverage_key":prop["coverage_key"],
        }));
    }
}

fn publish_reason_progress(
    state: &AppState, run_id: &str, mode: &str, trigger: &str,
    result: &ReasonResult, source_to_seq: u64,
) {
    let section = |key: &str| -> Vec<Value> {
        result.progress_sections.get(key).into_iter().flatten()
            .map(|text| json!({"text":text})).collect()
    };
    emit_event(state, run_id, Some("reason"), "reason.progress", json!({
        "brief_id":format!("brief-{}", uuid::Uuid::new_v4().simple()),
        "kind":"decide","phase":"explore","mode":mode,"trigger":trigger,
        "summary":result.progress_summary,"summary_source":"reason","summary_status":"ready",
        "source_from_seq":1,"source_to_seq":source_to_seq,
        "sections":{"confirmed":section("confirmed"),"active":section("active"),
            "blocked":section("blocked"),"next":section("next")},
    }));
}

async fn cancel_decide_dropped_workers(
    state: &Arc<AppState>,
    run_id: &str,
    active: &mut HashMap<String, ActiveAssignment>,
    dropped: &[String],
) -> Vec<ActiveAssignment> {
    let mut cancelled = Vec::new();
    let solvers: Vec<String> = active.iter()
        .filter(|(_, assignment)| dropped.contains(&assignment.intent_id)
            && !assignment.abort.is_finished())
        .map(|(solver, _)| solver.clone()).collect();
    for solver in solvers {
        let request = active.get(&solver)
            .and_then(|assignment| assignment.cancel.as_ref())
            .and_then(|cancel| cancel.lock().unwrap().take());
        let host_cancel = active.get(&solver).and_then(|assignment| assignment.host_cancel.clone());
        let container = request.is_some();
        let stopped = if let Some(request) = request {
            let (reply, confirmed) = tokio::sync::oneshot::channel();
            request.send(reply).is_ok()
                && matches!(tokio::time::timeout(Duration::from_secs(12), confirmed).await,
                    Ok(Ok(true)))
        } else if let Some(cancel) = host_cancel {
            cancel.store(true, Ordering::SeqCst);
            if let Some(worker) = state.runs.unregister_worker(run_id, &solver) {
                state.agents.close(&worker.thread_id).await;
            }
            true
        } else { false };
        if !stopped { continue; }
        let Some(assignment) = active.remove(&solver) else { continue };
        if container {
            assignment.abort.abort();
            if let Some(worker) = state.runs.unregister_worker(run_id, &solver) {
                state.agents.close(&worker.thread_id).await;
            }
        }
        emit_event(state, run_id, Some("reason"), "blackboard.delta",
            json!({"kind":"intent_drop_requested","intent_id":assignment.intent_id,
                "worker":solver,"reason":"Decide explicitly dropped this Step"}));
        cancelled.push(assignment);
    }
    cancelled
}









pub fn spawn_coordinator(
    state: Arc<AppState>,
    run_id: String,
    seats: Vec<WorkerSpec>,
    challenge: Value,
    reopen_hint: Option<String>,
) -> tokio::task::AbortHandle {
    let state_clone = state.clone();
    let run_id_clone = run_id.clone();
    let generation=state.store.get("runs",&run_id).ok().flatten().and_then(|run|run["execution_generation"].as_u64()).unwrap_or(1);
    let context=(run_id.clone(),generation);
    let join = tokio::spawn(super::terminal::GENERATION.scope(context,async move {
        match challenge["mode"].as_str().unwrap_or("ctf") {
            "ctf" => ctf::run(state_clone, run_id_clone, seats, challenge, reopen_hint).await,
            "pentest" => pentest_driver::run(state_clone, run_id_clone, seats, challenge, reopen_hint).await,
            mode => mark_run_finished(&state_clone,&run_id_clone,None,false,"unsupported_mode",Some(mode)),
        }
    }));
    let abort = join.abort_handle();
    state
        .runs
        .register_coordinator(&run_id, abort.clone());
    abort
}



#[cfg(test)]
mod pentest_reason_live_tests {
    use super::*;
    use crate::{runs::{ensure_run_record, pentest::{compile_contract, record_contract}}, store::Store};

    #[tokio::test]
    #[ignore = "requires MUTEKI_PENTEST_REASON_STATE, source Pi/Ollama account and local DVWA"]
    async fn real_dvwa_pi_reason_commits_graph_before_task_result() {
        let root = std::path::PathBuf::from(std::env::var("MUTEKI_PENTEST_REASON_STATE").unwrap());
        let store = Store::open(root.join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(&root)).unwrap();
        let state = Arc::new(AppState::new(store));
        let target = "http://127.0.0.1:4280/login.php";
        let response = reqwest::get(target).await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let html = response.text().await.unwrap();
        assert!(html.contains("login"));

        let run_id = "reason-dvwa-task-commit";
        let challenge = json!({"mode":"pentest","description":format!(
            "授权测试仅限 {target} 。请验证本机 DVWA 登录页面的真实表单字段和默认凭据，先规划一条取证 Step。")});
        let contract = compile_contract(&challenge).unwrap();
        let mut run = ensure_run_record(&state, run_id).unwrap();
        run["mode"] = json!("pentest");
        run["name"] = json!("DVWA 登录取证");
        run["pentest_contract"] = contract.clone();
        state.store.put("runs", run_id, &run).unwrap();
        record_contract(&state, run_id, &contract).unwrap();
        let graph = SharedGraph::open(&state, run_id).unwrap();
        assert!(graph.seed_origin_fact(&challenge) > 0);
        graph.append_event("observation_added", "dvwa-http", &json!({
            "detail":format!("Live GET {target} returned HTTP 200; login marker present: {}", html.contains("login")),
            "target":target,
        }), None, true, 1.0, None);
        drop(graph);

        let task_state = state.clone();
        let task = tokio::spawn(async move {
            let plan = pentest::plan_steps(task_state.clone(), run_id, 1, None).await?;
            commit_pentest_reason_plan(&task_state, run_id, plan)
        });
        let plan = task.await.unwrap().unwrap_or_else(|failure|panic!("{}: {}",failure.kind,failure.detail));
        assert!(plan.graph_operations_committed && !plan.stale);
        assert!(!plan.proposed_intents.is_empty(), "Pi/Ollama submitted no executable Step");
        let events = SharedGraph::open(&state, run_id).unwrap().events();
        assert!(events.iter().any(|event| event["kind"] == "reason_attempt"));
        assert_eq!(events.iter().filter(|event|event["kind"] == "intent_proposed").count(),
            plan.proposed_intents.len());
        assert!(crate::runs::read_jsonl_events(&state, run_id).iter().all(|event|
            event["payload"]["kind"] != "reason_done"));
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn each_dispatch_has_a_distinct_worker_identity_and_generation() {
        let directory=tempfile::tempdir().unwrap();let graph=SharedGraph::open_at(directory.path().join("graph.db")).unwrap();
        let mut sequences=HashMap::new();
        assert_eq!(allocate_worker_label("pi",1,&mut sequences,&graph),"cli-pi");
        assert_eq!(allocate_worker_label("pi",1,&mut sequences,&graph),"cli-pi-2");
        assert_eq!(allocate_worker_label("omp",1,&mut sequences,&graph),"cli-omp");
        let mut next_generation=HashMap::new();
        assert_eq!(allocate_worker_label("pi",2,&mut next_generation,&graph),"cli-pi-g2");
        graph.propose_intent("reason","step_001","Inspect input",&json!({}),None);assert!(graph.claim_intent("cli-pi","step_001",600.));
        let mut restored=HashMap::new();assert_eq!(allocate_worker_label("pi",1,&mut restored,&graph),"cli-pi-2");
    }
}
