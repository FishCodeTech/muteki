//! CTF Pi Decide: a committed Pi tool draft is rechecked against the live graph
//! before its Step and Goal operations are written.

use super::{
    coordinator::{PlannedIntent, ReasonResult, dispatch_ctf_intents},
    emit_event,
    graph::SharedGraph,
    session_dir,
};
use crate::AppState;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};
use walkdir::WalkDir;

#[path = "planner_runtime.rs"]
pub(crate) mod pi_native;

#[path = "context.rs"]
mod context;

pub(crate) struct CtfStepPlan {
    pub reason: ReasonResult,
    pub watermark: i64,
    pub proposed_intents: Vec<Value>,
    pub dropped_intent_ids: Vec<String>,
    pub reprioritized_intent_ids: Vec<String>,
    pub intent_decisions: Vec<Value>,
    pub reason_attempt: Value,
    pub goal_evidence_facts: Vec<i64>,
    pub yield_visit: bool,
    pub context_update: Value,
}

pub(crate) fn planning_trigger(events: &[Value], after: i64) -> Option<&'static str> {
    if after < 0 {
        return Some("ctf_initial");
    }
    for event in events {
        if event["payload"]["status"]=="consumed" {continue;}
        if event["seq"].as_i64().unwrap_or(0) <= after || event["actor"] == "reason" {
            continue;
        }
        let Some(kind) = event["kind"].as_str() else {
            continue;
        };
        if kind == "branch_split"
            && event["payload"]["branches"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|branch| {
                    branch["from_facts"]
                        .as_array()
                        .is_some_and(|facts| !facts.is_empty())
                })
        {
            return Some("ctf_graph_change");
        }
        if matches!(
            kind,
            "fact_added"
                | "intent_concluded"
                | "dead_end"
                | "finding_found"
                | "flag_found"
                | "flag_invalidated"
                | "platform_verdict"
                | "operator_directive"
                | "operator_directive_status"
                | "branch_facts_bound"
        ) {
            return Some(if kind.starts_with("operator_") {
                "operator"
            } else {
                "ctf_graph_change"
            });
        }
    }
    None
}

fn internal_workspace_name(name: &str) -> bool {
    matches!(
        name,
        "manifest.json"
            | "runtime-diagnostics.json"
            | "00_step_history_review.md"
            | "graph.yaml"
            | "graph.jsonl"
            | "events.jsonl"
            | "AGENTS.md"
            | "CLAUDE.md"
    ) || [".lock", ".db", ".db-wal", ".db-shm"]
        .iter()
        .any(|suffix| name.to_ascii_lowercase().ends_with(suffix))
        || name.starts_with("graph_wave_") && name.ends_with(".yaml")
}

fn in_flight_workspace_name(name: &str, prefixes: &[String]) -> bool {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|part| part.to_str())
        .unwrap_or(name)
        .to_ascii_lowercase();
    let normalized = stem.replace(['_', '-'], "");
    prefixes.iter().any(|prefix| {
        if stem == *prefix
            || stem.starts_with(&format!("{prefix}_"))
            || stem.starts_with(&format!("{prefix}-"))
        {
            return true;
        }
        let compact = prefix.replace(['_', '-'], "");
        compact.len() <= normalized.len()
            && normalized.starts_with(&compact)
            && normalized[compact.len()..]
                .chars()
                .next()
                .is_none_or(|next| !next.is_ascii_digit())
    })
}

fn workspace_file_time(path: &Path) -> u128 {
    fs::metadata(path)
        .ok()
        .filter(|meta| meta.is_file())
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos())
}

fn include_workspace_file(
    items: &mut HashMap<String, u128>,
    path: &Path,
    display: String,
    prefixes: &[String],
) {
    if Path::new(&display)
        .components()
        .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| {
                internal_workspace_name(name) || in_flight_workspace_name(name, prefixes)
            })
    {
        return;
    }
    items.insert(display, workspace_file_time(path));
}

fn decide_workspace_files_block(graph: &SharedGraph, challenge: &Value, root: &Path) -> String {
    let mut items = HashMap::<String, u128>::new();
    let mut prefixes = Vec::new();
    for step in graph.all_intents() {
        if step["status"] != "claimed"
            || !matches!(step["dispatch_state"].as_str(), None | Some("active"))
        {
            continue;
        }
        let Some(id) = step["intent_id"].as_str() else {
            continue;
        };
        let id = id.to_ascii_lowercase();
        prefixes.push(id.clone());
        prefixes.push(id.replace(['_', '-'], ""));
    }
    let mut attachment_names = HashSet::new();
    for item in challenge["attachments"].as_array().into_iter().flatten() {
        let Some(raw) = item.as_str().or_else(|| item["path"].as_str()) else {
            continue;
        };
        let attachment = PathBuf::from(raw);
        let Some(name) = attachment.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.is_empty() || internal_workspace_name(name) {
            continue;
        }
        attachment_names.insert(name.to_owned());
        let landed = root.join(name);
        if landed.is_file() {
            include_workspace_file(&mut items, &landed, name.to_owned(), &prefixes);
        } else if attachment.is_file() {
            include_workspace_file(&mut items, &attachment, name.to_owned(), &prefixes);
        }
    }

    if root.is_dir() {
        let shared = root.join("shared");
        let live = shared.join("live");
        for artifact_root in [&shared, &live] {
            let Ok(children) = fs::read_dir(artifact_root) else {
                continue;
            };
            for child in children.flatten() {
                let path = child.path();
                let name = child.file_name().to_string_lossy().to_string();
                if name.starts_with('.')
                    || artifact_root == &shared
                        && matches!(
                            name.as_str(),
                            "live" | "objects" | "links" | "index.jsonl" | "shared"
                        )
                    || name == "shared" && path.is_symlink()
                    || in_flight_workspace_name(&name, &prefixes)
                {
                    continue;
                }
                if path.is_file() {
                    include_workspace_file(&mut items, &path, format!("shared/{name}"), &prefixes);
                } else if path.is_dir() {
                    let files: Vec<PathBuf> = WalkDir::new(&path)
                        .min_depth(1)
                        .into_iter()
                        .flatten()
                        .map(|entry| entry.path().to_owned())
                        .filter(|file| file.is_file())
                        .filter(|file| {
                            file.strip_prefix(artifact_root).is_ok_and(|relative| {
                                !relative
                                    .components()
                                    .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
                            })
                        })
                        .filter(|file| {
                            file.file_name()
                                .and_then(|part| part.to_str())
                                .is_some_and(|name| {
                                    !(name == "shared" && file.is_symlink())
                                        && !internal_workspace_name(name)
                                        && !in_flight_workspace_name(name, &prefixes)
                                })
                        })
                        .collect();
                    if files.len() > 8 {
                        let latest = files
                            .iter()
                            .map(|file| workspace_file_time(file))
                            .max()
                            .unwrap_or(0);
                        items.insert(
                            format!("shared/{name}/ (目录, {} 个文件)", files.len()),
                            latest,
                        );
                    } else {
                        for file in files {
                            if let Ok(relative) = file.strip_prefix(artifact_root) {
                                include_workspace_file(
                                    &mut items,
                                    &file,
                                    format!("shared/{}", relative.to_string_lossy()),
                                    &prefixes,
                                );
                            }
                        }
                    }
                }
            }
        }
        if let Ok(children) = fs::read_dir(root) {
            for child in children.flatten() {
                let path = child.path();
                let name = child.file_name().to_string_lossy().to_string();
                if path.is_file()
                    && (attachment_names.contains(&name) || !internal_workspace_name(&name))
                    && !internal_workspace_name(&name)
                {
                    include_workspace_file(&mut items, &path, name, &prefixes);
                }
            }
        }
    }
    if items.is_empty() {
        return String::new();
    }
    let mut ordered: Vec<_> = items.into_iter().collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut lines = vec!["【工作目录里的文件】(随项目附带的物料,或已结案 step 留下的产物;你没有读取工具,要用到内容就开一个 step 交给 execute 读。注意:仍在 open 状态的 step 可能正在后台写入文件,不要因看到相关文件名就 drop_step 取消仍在运行的 open step)".to_owned()];
    lines.extend(ordered.iter().take(30).map(|(name, _)| format!("- {name}")));
    if ordered.len() > 30 {
        lines.push(format!("- …还有 {} 项(略)", ordered.len() - 30));
    }
    lines.join("\n")
}

pub(super) fn graph_views(
    graph: &SharedGraph,
    _challenge: &Value,
    run: &Value,
    max_intents: usize,
) -> Result<(String, Value), String> {
    let events = graph.events();
    let cutoff = events
        .iter()
        .filter(|event| event["kind"] == "visit_wave_reset")
        .filter_map(|event| event["seq"].as_i64())
        .max()
        .unwrap_or(0);
    let active: Vec<_> = graph
        .active_facts()
        .into_iter()
        .filter(|fact| {
            fact["fact_seq"].as_i64().is_some_and(|seq| seq > cutoff)
                || events
                    .iter()
                    .any(|event| event["seq"] == fact["fact_seq"] && event["kind"] == "fact_origin")
        })
        .collect();
    let active_seqs: HashSet<i64> = active
        .iter()
        .filter_map(|fact| fact["fact_seq"].as_i64())
        .collect();
    let mut all_ids: HashMap<i64, String> = HashMap::new();
    let mut fact_id_map = serde_json::Map::new();
    let mut fact_index = 0;
    for event in &events {
        let Some(seq) = event["seq"].as_i64() else {
            continue;
        };
        let id = match event["kind"].as_str() {
            Some("fact_origin") => "fact_origin".to_owned(),
            Some("fact_added") if seq > cutoff => {
                fact_index += 1;
                format!("fact_{fact_index:03}")
            }
            _ => continue,
        };
        all_ids.insert(seq, id.clone());
        if active_seqs.contains(&seq) {
            fact_id_map.insert(id, json!(seq));
        }
    }
    let products = graph.intent_products()?;
    let intents: Vec<_> = graph
        .all_intents()
        .into_iter()
        .filter(|row| row["created_seq"].as_i64().is_some_and(|seq| seq > cutoff))
        .collect();
    let mut producers = HashMap::<i64, String>::new();
    for row in &intents {
        let id = row["intent_id"].as_str().unwrap_or("");
        for seq in products.get(id).into_iter().flatten() {
            producers.insert(*seq, id.to_owned());
        }
    }
    let mut facts = Vec::new();
    let mut preview_facts = Vec::new();
    let mut fact_to_step = serde_json::Map::new();
    for fact in &active {
        let seq = fact["fact_seq"].as_i64().unwrap_or(0);
        let Some(id) = all_ids.get(&seq) else {
            continue;
        };
        let event = events.iter().find(|event| event["seq"] == seq);
        let payload = event.map(|event| &event["payload"]).unwrap_or(&Value::Null);
        let text = if id == "fact_origin" {
            context::live_origin(run)
        } else {
            fact["fact"].as_str().unwrap_or("").to_owned()
        };
        let (title, content) = text.split_once('\n').unwrap_or((&text, ""));
        let producer = producers.get(&seq).map(String::as_str).unwrap_or("");
        let mut refs: Vec<Value> = payload["evidence_provenance"]["artifact_refs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["artifact_id"].as_str())
            .map(|id| json!({"id":id}))
            .collect();
        let source_observation = payload["obs_seq"].as_i64().filter(|seq| *seq > 0);
        if let Some(obs_seq) = source_observation
            && let Some(observation) = events
                .iter()
                .find(|event| event["seq"] == obs_seq && event["kind"] == "observation_added")
        {
            if let Some(id) = observation["payload"]["artifact_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                && !refs.iter().any(|item| item["id"] == id)
            {
                refs.push(json!({"id":id}));
            }
        }
        if !producer.is_empty() {
            fact_to_step.insert(id.clone(), json!(producer));
        }
        facts.push(json!({"id":id,"title":title,"content":content,
            "createdAt":event.map(|event|context::timestamp(&event["ts"])).unwrap_or_default(),
            "producedBy":if producer.is_empty() { Value::Null } else { json!(producer) },
            "sourceObservations":source_observation.map(|seq|vec![format!("observation_{seq}")]).unwrap_or_default(),
            "sourceArtifacts":refs}));
        preview_facts.push(json!({"id":id,"seq":seq,"title":title.trim()}));
    }
    let mut steps = Vec::new();
    let mut open_steps = Vec::new();
    let mut done_steps = Vec::new();
    for row in intents {
        let id = row["intent_id"].as_str().unwrap_or("");
        let from: Vec<Value> = row["from_facts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .filter_map(|seq| all_ids.get(&seq))
            .map(|id| json!(id))
            .collect();
        let to: Vec<Value> = products
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|seq| all_ids.get(seq))
            .map(|id| json!(id))
            .collect();
        let dropped = row["result"] == "superseded"
            || matches!(row["dispatch_state"].as_str(), Some("retired" | "closed"))
                && row["status"] != "done";
        let state = if dropped {
            "dropped"
        } else if row["status"] == "done" {
            "done"
        } else {
            "open"
        };
        let running = state == "open" && row["status"] == "claimed";
        let interrupted = row["result"].as_str().is_some_and(|r| {
            matches!(r, "cancelled" | "interrupted" | "dropped" | "timed_out" | "failed")
        }) && state != "open";
        let completed = state == "done" && !interrupted;
        let mut step = json!({"id":id,"action":row["goal"],"from":from,"actor":"agent",
            "priority":row["priority"],"state":state,"to":to,
            "completed":completed,"interrupted":interrupted,
            "expectedObservable":row["expected_observable"],
            "stopCondition":row["stop_condition"],"coverageKey":row["coverage_key"],
            "result":row["result"],"reason":events.iter().find(|event|event["seq"]==row["concluded_seq"]).map(|event|event["payload"]["reason"].clone()),
            "createdAt":events.iter().find(|event|event["seq"]==row["created_seq"]).map(|event|context::timestamp(&event["ts"])).unwrap_or_default(),
            "requires":row["required_pocs"],"requiresCapabilities":row["requires_capabilities"]});
        if row["worker"].as_str().is_some_and(|s| !s.trim().is_empty()) {
            step["worker"] = row["worker"].clone();
        }
        steps.push(step);
        let preview = json!({"id":id,"action":row["goal"],"from":from,
            "priority":row["priority"],"state":state,"to":to,
            "running":running,"worker_id":row["worker"],"has_live_context":running});
        if state == "open" {
            open_steps.push(preview);
        } else {
            done_steps.push(preview);
        }
    }
    let mut goals = vec![json!({"id":"goal_final","kind":"final",
        "criterion":"解决这道 CTF 题目，并提交全部 FLAG，多 FLAG 题目要找到全部 FLAG 并有效提交",
        "createdAt":facts.first().map(|fact|fact["createdAt"].clone())})];
    goals.extend(graph.sub_goals().into_iter().map(|goal| {
        json!({
            "id":goal["goal_id"],"kind":"sub","criterion":goal["criterion"],
            "state":goal["state"],"satisfiedBy":goal["evidence_fact_seqs"].as_array().into_iter().flatten().filter_map(Value::as_i64).filter_map(|seq|all_ids.get(&seq)).collect::<Vec<_>>(),
            "satisfiedReason":goal["reason"],"satisfiedAt":goal["satisfied_at"],"createdAt":goal["created_at"]
        })
    }));
    let resources = graph.pocs();
    let resource_ids: Vec<String> = resources
        .iter()
        .filter(|resource| {
            matches!(
                resource["status"].as_str(),
                Some("available" | "directional" | "wip")
            )
        })
        .filter_map(|resource| resource["poc_id"].as_str().map(str::to_owned))
        .collect();
    let dead_ends: Vec<Value> = graph
        .dead_ends()
        .into_iter()
        .map(|row| {
            json!({
        "id":format!("dead_end_{}",row["seq"].as_i64().unwrap_or(0)),
        "tested":row["tested_scope"],"observed":row["observed_result"],
        "reason":row["reason"],"step":row["intent_id"]})
        })
        .collect();
    let observations: Vec<Value> = events
        .iter()
        .filter(|event| event["kind"] == "observation_added")
        .map(|event| {
            json!({
                "id":format!("observation_{}",event["seq"].as_i64().unwrap_or(0)),
                "text":event["payload"]["detail"],
                "step":event["payload"]["intent_id"],
                "artifact":event["payload"]["artifact_id"],
            })
        })
        .collect();
    let flags = run["flags"].as_array().cloned().unwrap_or_default();
    let source = json!({"facts":facts,"goals":goals,"steps":steps,
        "observations":observations,"deadEnds":dead_ends,"resources":resources,
        "flagProgress":{"captured":flags,"count":flags.len(),
            "expected":run["expected_flags"],"multiFlag":run["multi_flag"],
            "platformConfirmationRequired":run["platform_confirmation_required"],
            "complete":run["solved"]},
        "operatorNotes":graph.directives().into_iter()
            .filter(|row|row["status"] == "applied")
            .filter_map(|row|row["text"].as_str().filter(|text|!text.starts_with("secret://"))
                .map(|text|json!({"id":row["directive_id"],"action":row["action"],"text":text}))).collect::<Vec<_>>(),
        "requestedIntents":max_intents});
    let yaml = context::render(&source);
    let mut snapshot = json!({"schema":"muteki.ctf-decide-graph.v1",
        "max_intents":if max_intents==usize::MAX {-1i64} else {max_intents as i64},"graph_seq":graph.semantic_graph_watermark(),
        "fact_id_map":fact_id_map,"facts":preview_facts,
        "goals":goals,"reserved_goal_ids":graph.sub_goals().iter()
            .filter_map(|goal|goal["goal_id"].as_str()).chain(std::iter::once("goal_final"))
            .collect::<Vec<_>>(),
        "resource_ids":resource_ids,"open_steps":open_steps,
        "done_steps":done_steps,"fact_to_step":fact_to_step,"cancelled":[]});
    crate::runs::board_context::attach(&mut snapshot, &source, graph)?;
    Ok((yaml, snapshot))
}

fn save_text(state: &AppState, run_id: &str, kind: &str, text: &str) -> Result<String, String> {
    if text.is_empty() {
        return Ok(String::new());
    }
    let id = format!("reason-{kind}-{}", uuid::Uuid::new_v4().simple());
    let dir = session_dir(state, run_id).join("reason-responses");
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    let path = dir.join(format!("{id}.txt"));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|error| error.to_string())?;
    Ok(id)
}

fn fact_seq(value: &Value, known: &HashMap<String, i64>) -> Option<i64> {
    if let Some(seq) = value.as_i64() {
        return known.values().any(|item| *item == seq).then_some(seq);
    }
    let id = value.as_str()?;
    known.get(id).copied().or_else(|| {
        id.parse::<i64>()
            .ok()
            .filter(|seq| known.values().any(|item| item == seq))
    })
}

fn fact_sources(value: &Value, known: &HashMap<String, i64>) -> Option<Vec<i64>> {
    let mut ids = Vec::new();
    for item in value.as_array()? {
        let seq = fact_seq(item, known)?;
        if !ids.contains(&seq) {
            ids.push(seq);
        }
    }
    Some(ids)
}

fn empty_reason() -> ReasonResult {
    ReasonResult {
        verdict: "explore".to_owned(),
        goal_met: false,
        complete_why: String::new(),
        drift: String::new(),
        progress_summary: String::new(),
        progress_sections: HashMap::new(),
        intents: Vec::new(),
        supersede_intents: Vec::new(),
        supersede_why: String::new(),
        reprioritize_intents: HashMap::new(),
        pinned_facts: Vec::new(),
        parse_status: "parsed".to_owned(),
        parse_detail: String::new(),
        planner_failure: None,
    }
}

pub(crate) async fn plan_steps(
    state: Arc<AppState>,
    run_id: &str,
    challenge: &Value,
    max_intents: usize,
    retry: Option<&Value>,
) -> Result<CtfStepPlan, String> {
    plan_steps_with_role(state, run_id, challenge, max_intents, retry, false).await
}

pub(crate) async fn plan_challenger(
    state: Arc<AppState>,
    run_id: &str,
    challenge: &Value,
    max_intents: usize,
) -> Result<CtfStepPlan, String> {
    plan_steps_with_role(state, run_id, challenge, max_intents, None, true).await
}

async fn plan_steps_with_role(
    state: Arc<AppState>,
    run_id: &str,
    challenge: &Value,
    max_intents: usize,
    retry: Option<&Value>,
    challenger: bool,
) -> Result<CtfStepPlan, String> {
    let actor = if challenger { "challenger" } else { "reason" };
    let graph = SharedGraph::open(&state, run_id)?;
    let watermark = graph.semantic_graph_watermark();
    let run = state
        .store
        .get("runs", run_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Run 不存在".to_owned())?;
    let (full_source, mut snapshot) =
        graph.with_immediate_tx(|| graph_views(&graph, challenge, &run, max_intents))?;
    super::worker::persist_graph_context(&state, run_id, &full_source)?;
    let files=decide_workspace_files_block(&graph, challenge, &session_dir(&state,run_id));
    snapshot["context_objects"].as_array_mut().ok_or("context objects missing")?.push(json!({"id":"workspace_files","type":"workspace","object":files}));
    crate::runs::board_context::attach_result(&state,run_id,&mut snapshot)?;
    let sidecar=crate::runs::board_context::load(&state,run_id)?;
    snapshot["context_objects"].as_array_mut().unwrap().push(json!({"id":"previous_result","type":"receipt","object":sidecar["previous_result"]}));
    let (mut source, delivered_through) = crate::runs::board_context::decide(&state, run_id, &snapshot)?;
    if !challenger {
        if let Some(note) = retry.filter(|note| note["retry_index"].as_u64().is_some_and(|n| n > 0))
        {
            source.push_str(&format!(
            "\n\n## Planner retry\nretry index: {}\nprevious failure: {}\nThe run goal is still incomplete. Propose executable Steps that avoid the rejected ownership, stage, method, and coverage combinations below.\n- no detailed rejection",
            note["retry_index"].as_u64().unwrap_or(1),
            note["previous_failure"].as_str().unwrap_or("empty_plan")
            ));
        }
        source.push_str("\n\n请判断下一步,操作记入草稿后 commit。");
    }
    if source.len()>crate::runs::board_context::PACKAGE_BYTES {return Err("context.decide_package_budget_exceeded".into());}
    drop(graph);
    let input_id = save_text(
        &state,
        run_id,
        if challenger {
            "challenger-input"
        } else {
            "input"
        },
        &source,
    )?;
    emit_event(
        &state,
        run_id,
        Some(actor),
        "blackboard.delta",
        json!({
        "kind":"context_manifest","role":actor,"worker_id":actor,
        "input_artifact_id":input_id,"total_chars":source.chars().count(),
        "full_board_included":false,"payload_bytes":source.len(),"estimated_tokens":source.len().div_ceil(4)}),
    );
    let selection = pi_native::selected_for_ctf(&state, run_id)?
        .ok_or_else(|| "CTF Pi planner account/model is not configured".to_owned())?;
    snapshot["available_capabilities"] = json!(super::worker::available_run_capabilities(&state, run_id, "ctf"));
    let reply = if challenger {
        pi_native::run_ctf_challenger(
            state.clone(),
            run_id,
            &source,
            &snapshot,
            max_intents,
            selection,
        )
        .await
    } else {
        pi_native::run_ctf(
            state.clone(),
            run_id,
            &source,
            &snapshot,
            max_intents,
            selection,
        )
        .await
    }
    .map_err(|failure| format!("{}: {}", failure.error_type, failure.detail))?;
    let available_caps = super::worker::available_run_capabilities(&state, run_id, "ctf");
    let response_id = save_text(
        &state,
        run_id,
        if challenger {
            "challenger-response"
        } else {
            "response"
        },
        &reply.raw_response,
    )?;
    let draft_id = save_text(
        &state,
        run_id,
        if challenger {
            "challenger-draft"
        } else {
            "draft"
        },
        &reply.draft.to_string(),
    )?;
    let graph = SharedGraph::open(&state, run_id)?;
    let current: HashMap<String, i64> =
        graph_views(&graph, challenge, &run, max_intents)?.1["fact_id_map"]
            .as_object()
            .into_iter()
            .flat_map(|map| map.iter())
            .filter_map(|(id, seq)| Some((id.clone(), seq.as_i64()?)))
            .collect();
    let origin = current.get("fact_origin").copied().unwrap_or(0);
    let resources: HashSet<String> = graph
        .pocs()
        .iter()
        .filter(|row| {
            matches!(
                row["status"].as_str(),
                Some("available" | "directional" | "wip")
            )
        })
        .filter_map(|row| row["poc_id"].as_str().map(str::to_owned))
        .collect();
    let open_ids: HashSet<String> = graph
        .all_intents()
        .iter()
        .filter(|row| {
            row["dispatch_state"] == "active"
                && matches!(row["status"].as_str(), Some("open" | "claimed"))
        })
        .filter_map(|row| row["intent_id"].as_str().map(str::to_owned))
        .collect();
    let mut reason = empty_reason();
    let mut receipts = Vec::new();
    let mut revised = Vec::new();
    let mut retired = HashSet::new();
    let mut subgoals = Vec::new();
    let mut known_goals: HashSet<String> = graph
        .sub_goals()
        .iter()
        .filter_map(|goal| goal["goal_id"].as_str().map(str::to_owned))
        .collect();
    known_goals.insert("goal_final".to_owned());
    let mut goal_evidence_facts = Vec::new();
    let mut yield_visit = false;
    let operations = reply.draft["operations"]
        .as_array()
        .ok_or_else(|| "Pi Decide committed draft has no operations".to_owned())?;
    for (index, operation) in operations.iter().enumerate() {
        let name = operation["op"].as_str().unwrap_or("");
        let mut outcome = "accepted";
        let mut reason_code = name.to_owned();
        if challenger && matches!(name, "drop_step" | "change_step_priority" | "add_sub_goal") {
            receipts.push(json!({"raw_index":index,"op":name,"outcome":"dropped",
                "stage":"parse","reason_code":"challenger_plan_forbidden"}));
            continue;
        }
        match name {
            "archive_directive" => {
                let id=operation["directive_id"].as_str().unwrap_or("");
                if operation["reason"].as_str().is_none_or(|r|r.trim().is_empty()) || !graph.directives().iter().any(|d|d["directive_id"]==id&&d["status"]=="applied") {outcome="dropped";reason_code="invalid_archive_directive".into();}
            }

            "open_step" => {
                let action = operation["action"].as_str().unwrap_or("").trim();
                let from = fact_sources(&operation["from"], &current);
                let required: Option<Vec<String>> = if operation.get("requires").is_none() {
                    Some(Vec::new())
                } else {
                    operation["requires"].as_array().and_then(|items| {
                        items
                            .iter()
                            .map(|item| item.as_str().map(str::to_owned))
                            .collect()
                    })
                };
                let req_caps: Option<Vec<String>> = match operation.get("requires_capabilities") {
                    None | Some(Value::Null) => Some(Vec::new()),
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(|item| {
                            item.as_str()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned)
                        })
                        .collect(),
                    Some(_) => None,
                };
                if action.is_empty()
                    || from.is_none()
                    || required
                        .as_ref()
                        .is_some_and(|items| items.iter().any(|id| !resources.contains(id)))
                    || required.is_none()
                    || req_caps.is_none()
                    || reason.intents.len() >= max_intents
                {
                    outcome = "dropped";
                    reason_code = "invalid_open_step".to_owned();
                } else if super::worker::validate_requested_capabilities(
                    req_caps.as_ref().unwrap(),
                    &available_caps,
                )
                .is_err()
                {
                    outcome = "dropped";
                    reason_code = "unknown_capability".to_owned();
                } else {
                    let priority = operation["priority"].as_str().unwrap_or("normal");
                    let priority = if matches!(priority, "high" | "normal" | "low") {
                        priority
                    } else {
                        "normal"
                    };
                    reason.intents.push(PlannedIntent {
                        model_id: format!("D{}", reason.intents.len() + 1),
                        goal: action.to_owned(),
                        worker_class: "code".to_owned(),
                        priority: priority.to_owned(),
                        from_facts: from.unwrap_or_default(),
                        depends_on: Vec::new(),
                        route_hash: String::new(),
                        branch_id: String::new(),
                        lane_key: String::new(),
                        risk_class: String::new(),
                        resource_key: String::new(),
                        expected_observable: operation["expected_observable"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                        stop_condition: operation["stop_condition"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                        coverage_key: operation["coverage_key"].as_str().unwrap_or("").to_owned(),
                        required_pocs: required.unwrap_or_default(),
                        rationale: String::new(),
                        initial: false,
                    });
                }
            }
            "drop_step" => {
                let id = operation["intent_id"]
                    .as_str()
                    .or_else(|| operation["step_id"].as_str())
                    .unwrap_or("");
                if !open_ids.contains(id) || reason.supersede_intents.iter().any(|item| item == id)
                {
                    outcome = "dropped";
                    reason_code = "unknown_step".to_owned();
                } else {
                    reason.supersede_intents.push(id.to_owned());
                    let why = operation["why"]
                        .as_str()
                        .or_else(|| operation["reason"].as_str())
                        .unwrap_or("");
                    if !why.is_empty() {
                        reason.supersede_why.push_str(&format!("{id}: {why}; "));
                    }
                }
            }
            "change_step_priority" => {
                let id = operation["intent_id"].as_str().unwrap_or("");
                let priority = operation["priority"].as_str().unwrap_or("");
                if !open_ids.contains(id) || !matches!(priority, "high" | "normal" | "low") {
                    outcome = "dropped";
                    reason_code = "invalid_priority".to_owned();
                } else {
                    reason
                        .reprioritize_intents
                        .insert(id.to_owned(), priority.to_owned());
                }
            }
            "supersede_fact" => {
                let id = operation["fact_id"].as_str().unwrap_or("");
                let why = operation["reason"].as_str().unwrap_or("").trim();
                let known_seq = |seq: i64| current.values().any(|known| *known == seq);
                let seq = operation["fact_seq"]
                    .as_i64()
                    .filter(|seq| known_seq(*seq))
                    .or_else(|| current.get(id).copied())
                    .unwrap_or(0);
                let by_id = operation["by_fact_id"].as_str().unwrap_or("").trim();
                let by = operation["by_fact_seq"]
                    .as_i64()
                    .filter(|seq| known_seq(*seq))
                    .or_else(|| {
                        (by_id != "fact_origin")
                            .then(|| current.get(by_id).copied())
                            .flatten()
                    });
                let requested_by = !by_id.is_empty()
                    || operation["by_fact_seq"].as_i64().is_some_and(|seq| seq > 0);
                if seq <= 0
                    || seq == origin
                    || why.is_empty()
                    || retired.contains(&seq)
                    || by == Some(seq)
                    || (requested_by && by.is_none())
                {
                    outcome = "dropped";
                    reason_code = "invalid_supersede_fact".to_owned();
                } else {
                    retired.insert(seq);
                    revised.push((seq, why.to_owned(), by));
                }
            }
            "add_sub_goal" => {
                let criterion = operation["criterion"].as_str().unwrap_or("").trim();
                let mut number = known_goals.len();
                let id = loop {
                    let id = format!("goal_{number:03}");
                    if !known_goals.contains(&id) {
                        break id;
                    }
                    number += 1;
                };
                if criterion.is_empty()
                    || operation["goal_id"]
                        .as_str()
                        .is_some_and(|requested| requested != id)
                {
                    outcome = "dropped";
                    reason_code = "invalid_sub_goal".to_owned();
                } else {
                    known_goals.insert(id.clone());
                    subgoals.push(json!({"op":name,"goal_id":id,"criterion":criterion}));
                }
            }
            "satisfy_goal" => {
                let id = operation["goal_id"].as_str().unwrap_or("goal_final");
                let why = operation["reason"]
                    .as_str()
                    .or_else(|| operation["why"].as_str())
                    .unwrap_or("")
                    .trim();
                let from = if operation.get("from").is_none() {
                    Some(Vec::new())
                } else {
                    fact_sources(&operation["from"], &current)
                };
                if !known_goals.contains(id)
                    || why.is_empty()
                    || from.is_none()
                    || (id != "goal_final" && from.as_ref().is_some_and(Vec::is_empty))
                {
                    outcome = "dropped";
                    reason_code = "invalid_goal_evidence".to_owned();
                } else if id == "goal_final" {
                    reason.goal_met = true;
                    reason.verdict = "complete".to_owned();
                    reason.complete_why = why.to_owned();
                    goal_evidence_facts = from.unwrap_or_default();
                } else {
                    subgoals.push(json!({"op":name,"goal_id":id,"reason":why,
                    "evidence_fact_seqs":from.unwrap_or_default()}));
                }
            }
            "commit" => {
                reason.progress_summary = operation["summary"]
                    .as_str()
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                yield_visit = operation["yield_visit"] == true;
            }
            _ => {
                outcome = "dropped";
                reason_code = "unknown_op".to_owned();
            }
        }
        receipts.push(json!({"raw_index":index,"op":name,"outcome":outcome,
            "stage":"parse","reason_code":reason_code}));
    }
    for receipt in &mut receipts {
        if receipt["outcome"] != "accepted"
            || !matches!(receipt["op"].as_str(), Some("open_step" | "satisfy_goal"))
        {
            continue;
        }
        let index = receipt["raw_index"].as_u64().unwrap_or(u64::MAX) as usize;
        let Some(operation) = operations.get(index) else {
            continue;
        };
        if fact_sources(&operation["from"], &current)
            .is_some_and(|from| from.iter().any(|seq| retired.contains(seq)))
        {
            receipt["outcome"] = json!("dropped");
            receipt["reason_code"] = json!("superseded_fact_in_draft");
        }
    }
    reason
        .intents
        .retain(|intent| intent.from_facts.iter().all(|seq| !retired.contains(seq)));
    subgoals.retain(|operation| {
        operation["op"] != "satisfy_goal"
            || operation["evidence_fact_seqs"]
                .as_array()
                .is_some_and(|facts| {
                    facts
                        .iter()
                        .filter_map(Value::as_i64)
                        .all(|seq| !retired.contains(&seq))
                })
    });
    if goal_evidence_facts.iter().any(|seq| retired.contains(seq)) {
        reason.goal_met = false;
        reason.verdict = "explore".to_owned();
        reason.complete_why.clear();
        goal_evidence_facts.clear();
    }
    if reason.progress_summary.is_empty() {
        reason.progress_summary = reason
            .intents
            .iter()
            .map(|intent| intent.goal.clone())
            .collect::<Vec<_>>()
            .join("；");
    }
    reason.progress_sections.insert(
        "next".to_owned(),
        reason
            .intents
            .iter()
            .map(|intent| intent.goal.clone())
            .collect(),
    );
    let attempt = json!({"attempt_index":1,"response_status":"received",
        "input_artifact_id":input_id,"raw_response_artifact_id":response_id,
        "native_pi_draft_artifact_id":draft_id,"native_pi_exit_code":reply.exit_code,
        "native_pi_tool_names":reply.tool_names,"raw_response_chars":reply.raw_response.chars().count(),
        "parse_status":"parsed","parsed_intent_count":reason.intents.len()});
    if graph.append_event(
        "reason_attempt",
        actor,
        &json!({"diagnostics":attempt,"intent_decisions":receipts}),
        None,
        true,
        1.0,
        None,
    ) <= 0
    {
        return Err("CTF Reason 回执无法持久化".to_owned());
    }
    let mut graph = graph;
    for (seq, why, by) in &revised {
        let revision_seq = graph.pentest_decide_supersede_fact(actor, *seq, why, *by)?;
        if revision_seq > 0 {
            emit_event(
                &state,
                run_id,
                Some(actor),
                "blackboard.delta",
                json!({"kind":"fact_superseded","actor":actor,"fact_seq":*seq,
                    "by_fact_seq":*by,"reason":why,"revision_seq":revision_seq}),
            );
        }
    }
    for operation in &subgoals {
        if operation["op"] == "add_sub_goal" {
            let criterion = operation["criterion"].as_str().unwrap_or("");
            let created_id = graph.pentest_decide_add_sub_goal(
                actor,
                criterion,
                operation["goal_id"].as_str().unwrap_or(""),
            )?;
            emit_event(
                &state,
                run_id,
                Some(actor),
                "blackboard.delta",
                json!({
                    "kind": "sub_goal_added",
                    "goal_id": created_id,
                    "criterion": criterion,
                    "actor": actor,
                }),
            );
        } else {
            let goal_id = operation["goal_id"].as_str().unwrap_or("");
            let reason_text = operation["reason"].as_str().unwrap_or("");
            let evidence: Vec<i64> = operation["evidence_fact_seqs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_i64)
                .collect();
            let seq = graph.pentest_decide_satisfy_sub_goal(
                actor,
                goal_id,
                reason_text,
                &evidence,
            )?;
            if seq > 0 {
                emit_event(
                    &state,
                    run_id,
                    Some(actor),
                    "blackboard.delta",
                    json!({
                        "kind": "sub_goal_satisfied",
                        "goal_id": goal_id,
                        "reason": reason_text,
                        "evidence_fact_seqs": evidence,
                        "actor": actor,
                    }),
                );
            }
        }
    }
    let dropped =
        graph.pentest_decide_drop_steps(actor, &reason.supersede_intents, &reason.supersede_why)?;
    let reprioritized =
        graph.pentest_decide_reprioritize_steps(actor, &reason.reprioritize_intents)?;
    for receipt in &mut receipts {
        if receipt["op"] == "archive_directive" && receipt["outcome"] == "accepted" {
            let Some(op) = receipt["raw_index"]
                .as_u64()
                .and_then(|i| operations.get(i as usize))
            else {
                receipt["outcome"] = json!("dropped");
                receipt["reason_code"] = json!("invalid_archive_directive");
                continue;
            };
            let (Some(id), Some(archive_reason)) =
                (op["directive_id"].as_str(), op["reason"].as_str())
            else {
                receipt["outcome"] = json!("dropped");
                receipt["reason_code"] = json!("invalid_archive_directive");
                continue;
            };
            if let Err(err) = graph.archive_directive(&state, run_id, id, archive_reason, actor) {
                receipt["outcome"] = json!("dropped");
                receipt["reason_code"] = json!("archive_directive_rejected");
                receipt["detail"] = json!(err);
            }
        }
    }
    let proposed = dispatch_ctf_intents(&graph, &reason, actor);
    let watermark = graph.semantic_graph_watermark().max(watermark);
    Ok(CtfStepPlan {
        reason,
        watermark,
        proposed_intents: proposed,
        dropped_intent_ids: dropped,
        reprioritized_intent_ids: reprioritized,
        intent_decisions: receipts,
        reason_attempt: attempt,
        goal_evidence_facts,
        yield_visit,
        context_update: if challenger {Value::Null} else {crate::runs::board_context::pending(delivered_through, &reply.draft["working_notes"])?},
    })
}

/// The model context is required. Never substitute a partial in-memory board.
pub(super) fn worker_context(state: &AppState, run_id: &str, intent_id: &str) -> Result<String, String> {
    let run = state
        .store
        .get("runs", run_id)
        .map_err(|error| error.to_string())?
        .ok_or("Run does not exist")?;
    let graph = SharedGraph::open(state, run_id)?;
    let (full, snapshot) = graph.with_immediate_tx(|| graph_views(&graph, &run, &run, 0))?;
    super::worker::persist_graph_context(state, run_id, &full)?;
    crate::runs::board_context::worker(&snapshot, intent_id)
}
