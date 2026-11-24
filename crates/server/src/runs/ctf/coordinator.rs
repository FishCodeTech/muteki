//! Independent CTF orchestration and finalization.
use super::*;
use crate::runs as run;

struct PendingReason {
    state: Arc<AppState>,
    run_id: String,
    trigger: &'static str,
    watermark: i64,
    requested_intents: usize,
    wave: usize,
    task: Option<tokio::task::JoinHandle<Result<ctf_planner::CtfStepPlan, String>>>,
}

struct PendingChallenger {
    task: Option<tokio::task::JoinHandle<Result<ctf_planner::CtfStepPlan, String>>>,
}

impl PendingChallenger {
    fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
    }
}

impl Drop for PendingChallenger {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn cancel_ctf_challenger(pending: &mut Option<PendingChallenger>) {
    let Some(mut current) = pending.take() else {
        return;
    };
    if let Some(task) = current.task.take() {
        task.abort();
        let _ = task.await;
    }
}

impl PendingReason {
    fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
    }
}

impl Drop for PendingReason {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            emit_event(
                &self.state,
                &self.run_id,
                Some("reason"),
                "blackboard.delta",
                json!({"kind":"reason_cancelled","trigger":self.trigger,
                    "watermark":self.watermark,"requested_intents":self.requested_intents,
                    "reason":"coordinator_shutdown"}),
            );
        }
    }
}

async fn cancel_ctf_reason(pending: &mut Option<PendingReason>, reason: &str) {
    let Some(mut current) = pending.take() else {
        return;
    };
    let Some(task) = current.task.take() else {
        return;
    };
    task.abort();
    let _ = task.await;
    emit_event(
        &current.state,
        &current.run_id,
        Some("reason"),
        "blackboard.delta",
        json!({"kind":"reason_cancelled","trigger":current.trigger,
            "watermark":current.watermark,"requested_intents":current.requested_intents,
            "reason":reason}),
    );
}

pub(crate) fn dispatch_intents(
    graph: &SharedGraph,
    result: &ReasonResult,
    actor: &str,
) -> Vec<Value> {
    let existing_cov = graph.coverage_keys();
    let mut batch_cov = HashSet::new();
    let normalize_step = |goal: &str, from: &[i64]| {
        let mut sources = from.to_vec();
        sources.sort_unstable();
        sources.dedup();
        (
            canonical_fact_whitespace(goal).to_ascii_lowercase(),
            sources,
        )
    };
    let existing_ctf_steps: HashSet<_> = {
        graph
            .all_intents()
            .into_iter()
            .filter(|row| {
                row["dispatch_state"] != "retired"
                    && matches!(row["status"].as_str(), Some("open" | "claimed" | "done"))
            })
            .map(|row| {
                normalize_step(
                    row["goal"].as_str().unwrap_or(""),
                    &row["from_facts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_i64)
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    };
    let mut batch_ctf_steps = HashSet::new();
    let mut next_step = graph.prior_intent_count() + 1;
    let mut proposed = Vec::new();

    for it in &result.intents {
        let ctf_step = normalize_step(&it.goal, &it.from_facts);
        if existing_ctf_steps.contains(&ctf_step) || !batch_ctf_steps.insert(ctf_step) {
            continue;
        }
        let cov_norm = canonical_fact_whitespace(&it.coverage_key).to_ascii_lowercase();
        if !cov_norm.is_empty()
            && (existing_cov.contains(&cov_norm) || batch_cov.contains(&cov_norm))
        {
            continue;
        }
        let intent_id = {
            let id = format!("step_{next_step:03}");
            next_step += 1;
            id
        };

        let payload = json!({
            "worker_class": it.worker_class,
            "priority": it.priority,
            "requested_priority": it.priority,
            "priority_reason": "planner",
            "route_hash": it.route_hash,
            "branch_id": it.branch_id,
            "lane_key": it.lane_key,
            "risk_class": it.risk_class,
            "resource_key": it.resource_key,
            "expected_observable": it.expected_observable,
            "stop_condition": it.stop_condition,
            "coverage_key": it.coverage_key,
            "required_pocs": it.required_pocs,
            "depends_on": it.depends_on,
            "rationale": it.rationale,
            "initial": it.initial,
        });

        let seq = graph.propose_intent(
            actor,
            &intent_id,
            &it.goal,
            &payload,
            if it.from_facts.is_empty() {
                None
            } else {
                Some(&it.from_facts)
            },
        );
        if seq <= 0 {
            continue;
        }
        if !cov_norm.is_empty() {
            batch_cov.insert(cov_norm);
        }
        proposed.push(json!({
            "intent_id": intent_id,
            "created_seq": seq,
            "goal": it.goal,
            "worker_class": it.worker_class,
            "priority": it.priority,
            "from_facts": it.from_facts,
            "depends_on": it.depends_on,
            "required_pocs": it.required_pocs,
            "expected_observable": it.expected_observable,
            "stop_condition": it.stop_condition,
            "coverage_key": it.coverage_key,
            "route_hash": it.route_hash,
            "lane_key": it.lane_key,
            "risk_class": it.risk_class,
            "resource_key": it.resource_key,
        }));
    }

    proposed
}

pub(super) async fn run(
    state: Arc<AppState>,
    run_id: String,
    mut seats: Vec<WorkerSpec>,
    challenge: Value,
    reopen_hint: Option<String>,
) {
    let worker_settings = match run::launch_config::settings(&state, &run_id) {
        Ok(settings) => settings,
        Err(error) => {
            mark_run_finished(
                &state,
                &run_id,
                None,
                false,
                "configuration_unavailable",
                Some(&error),
            );
            return;
        }
    };
    let unlimited_workers = worker_settings["max_workers"].as_u64() == Some(0);
    let mut template_profiles = HashSet::new();
    let seat_templates: Vec<_> = seats
        .iter()
        .filter(|seat| template_profiles.insert(seat.profile_id.clone()))
        .cloned()
        .collect();
    let mut counted_profiles = HashSet::new();
    let ordinary_capacity: usize = if unlimited_workers {
        usize::MAX
    } else {
        seats
            .iter()
            .filter(|seat| counted_profiles.insert(seat.profile_id.clone()))
            .filter(|seat| worker::profile_allows_role(&seat.profile, "explore", &worker_settings))
            .map(|seat| {
                worker::profile_role_capacity(
                    &seat.profile,
                    "explore",
                    &worker_settings,
                    seats.len(),
                )
            })
            .sum::<usize>()
            .min(seats.len())
    };
    let Ok(graph) = SharedGraph::open(&state, &run_id) else {
        mark_run_finished(
            &state,
            &run_id,
            None,
            false,
            "runtime_failure",
            Some("无法初始化 shared_graph.db"),
        );
        return;
    };

    let origin_seq = match graph.seed_origin_fact_checked(&challenge) {
        Ok(seq) => seq,
        Err(error) => {
            mark_run_finished(
                &state,
                &run_id,
                None,
                false,
                "graph_origin_failed",
                Some(&error),
            );
            return;
        }
    };
    if origin_seq > 0 {
        let description = challenge["description"].as_str().unwrap_or("");
        let text = format!("题目信息:\n{description}");
        let summary = text.lines().next().unwrap_or("");
        emit_event(
            &state,
            &run_id,
            Some("origin"),
            "blackboard.delta",
            json!({
                "kind": "fact_origin",
                "fact_seq": origin_seq,
                "actor": "origin",
                "fact": text,
                "summary": summary,
                "verified": true,
                "confidence": 1.0,
            }),
        );
    }

    // Reconcile a ContextResource committed before a prior runtime stopped
    // but before its exact continuation reached the shared graph.
    if let Ok(resources) = run::control_context::active_continuations(&state, &run_id) {
        for resource in resources {
            run::control_context::propose_continuation(&graph, &resource);
        }
    }

    let generation = state
        .store
        .get("runs", &run_id)
        .ok()
        .flatten()
        .and_then(|run| run["execution_generation"].as_u64())
        .unwrap_or(0);
    let prior_events = run::read_jsonl_events(&state, &run_id);
    let anchor = prior_events.iter().rposition(|event| {
        matches!(
            event["event_type"].as_str(),
            Some("run.started" | "run.reopened")
        ) && event["payload"]["execution_generation"] == generation
    });
    let coordinator_started = prior_events[anchor.map_or(0, |index| index + 1)..]
        .iter()
        .any(|event| {
            event["event_type"] == "blackboard.delta"
                && event["payload"]["kind"] == "coordinator_started"
        });

    if !coordinator_started {
        emit_event(
            &state,
            &run_id,
            Some("coordinator"),
            "blackboard.delta",
            json!({
                "kind": "coordinator_started",
                "seats": seats.len(),
                "engines": seats.iter().map(|s| s.raw_engine.clone()).collect::<Vec<_>>(),
            }),
        );
    }

    let handle = state.runs.handle(&state, &run_id);
    let _ = graph.reconcile_historical_direct_steps(&HashSet::new(), None, None);
    let mut last_budget_checkpoint = Instant::now();
    let mut active_assignments: HashMap<String, ActiveAssignment> = HashMap::new();
    let mut exit_reasons: HashMap<String, Arc<std::sync::Mutex<Option<String>>>> = HashMap::new();
    let mut spawned_total: usize = super::super::coordinator::dispatched_worker_total(&graph, generation);
    let failed_engines: HashMap<String, HashSet<String>> = HashMap::new();
    let mut seat_cursor: usize = 0;
    let mut worker_label_seq: HashMap<String, usize> = HashMap::new();
    let mut last_decided_wm: i64 = -1;
    let mut wave: usize = 0;

    let mut pending_ctf_reason: Option<PendingReason> = None;
    let mut pending_ctf_challenger: Option<PendingChallenger> = None;
    let mut challenger_last_progress_count = 0usize;
    let mut challenger_baseline_done = 0usize;
    let mut last_challenger_done_count = 0usize;
    let mut ctf_planner_failures: u32 = 0;
    let mut ctf_retry_at: Option<Instant> = None;
    let mut ctf_yielding = false;

    let mut tick = tokio::time::interval(Duration::from_millis(200));

    loop {
        tick.tick().await;

        if handle.deleting.load(Ordering::SeqCst) {
            cancel_ctf_reason(&mut pending_ctf_reason, "run_deleted").await;
            cancel_ctf_challenger(&mut pending_ctf_challenger).await;

            if let Err(error) = run::worker_result::settle_claims(
                &state,
                &run_id,
                "canceled",
                "coordinator_finalizing",
            ) {
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "worker.error",
                    json!({"reason":"worker_result_salvage_failed","detail":error}),
                );
            } else {
                graph.release_claims_for_finalize();
            }
            break;
        }

        let run_doc = state
            .store
            .get("runs", &run_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| json!({}));
        let solved_now = run_doc["solved"].as_bool().unwrap_or(false)
            || run_doc["status"].as_str() == Some("solved");
        if solved_now {
            cancel_ctf_reason(&mut pending_ctf_reason, "run_solved").await;
            cancel_ctf_challenger(&mut pending_ctf_challenger).await;

            for (_, assign) in active_assignments.drain() {
                if let Err(error) = run::worker_result::salvage(
                    &state,
                    &run_id,
                    &assign.solver_id,
                    &assign.intent_id,
                    "solved",
                    "flag_found",
                ) {
                    emit_event(
                        &state,
                        &run_id,
                        Some(&assign.solver_id),
                        "worker.error",
                        json!({"reason":"worker_result_salvage_failed","detail":error}),
                    );
                }
                emit_event(
                    &state,
                    &run_id,
                    Some(&assign.solver_id),
                    "blackboard.delta",
                    json!({
                        "kind": "intent_concluded",
                        "intent_id": assign.intent_id,
                        "worker": assign.solver_id,
                        "result": "solved",
                    }),
                );
            }
            if let Err(error) = run::worker_result::settle_claims(
                &state,
                &run_id,
                "canceled",
                "coordinator_finalizing",
            ) {
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "worker.error",
                    json!({"reason":"worker_result_salvage_failed","detail":error}),
                );
            } else {
                graph.release_claims_for_finalize();
            }
            drop(graph);
            finalize_coordinator(&state, &run_id, true, "solved", None).await;
            return;
        }
        if run_doc["status"].as_str() == Some("finished")
            || !run_doc["running"].as_bool().unwrap_or(false)
        {
            cancel_ctf_reason(&mut pending_ctf_reason, "run_stopped").await;
            cancel_ctf_challenger(&mut pending_ctf_challenger).await;

            if let Err(error) = run::worker_result::settle_claims(
                &state,
                &run_id,
                "canceled",
                "coordinator_finalizing",
            ) {
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "worker.error",
                    json!({"reason":"worker_result_salvage_failed","detail":error}),
                );
            } else {
                graph.release_claims_for_finalize();
            }
            drop(graph);
            break;
        }
        if last_budget_checkpoint.elapsed() >= Duration::from_secs(1) {
            if let Err(error) = run::execution_budget::checkpoint(&state, &run_id) {
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "worker.error",
                    json!({"reason":"budget_checkpoint_failed","detail":error}),
                );
                handle.draining.store(true, Ordering::SeqCst);
            }
            last_budget_checkpoint = Instant::now();
        }
        let (execution_left, conclude_left) = match (
            run::execution_budget::remaining(&state, &run_id, "execute"),
            run::execution_budget::remaining(&state, &run_id, "conclude"),
        ) {
            (Ok(execute), Ok(conclude)) => (execute, conclude),
            (execute, conclude) => {
                let error = execute
                    .err()
                    .or_else(|| conclude.err())
                    .unwrap_or_else(|| "Execution budget unavailable".into());
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "worker.error",
                    json!({"reason":"budget_read_failed","detail":error}),
                );
                cancel_ctf_reason(&mut pending_ctf_reason, "budget_read_failed").await;
                cancel_ctf_challenger(&mut pending_ctf_challenger).await;
                if let Err(failure) = run::worker_result::settle_claims(
                    &state,
                    &run_id,
                    "interrupted",
                    "budget_read_failed",
                ) {
                    emit_event(
                        &state,
                        &run_id,
                        None,
                        "worker.error",
                        json!({"reason":"worker_result_salvage_failed","detail":failure}),
                    );
                }
                drop(graph);
                finalize_coordinator(&state, &run_id, false, "runtime_failure", None).await;
                return;
            }
        };
        let cost_budget = run_doc["cost_budget_usd"].as_f64().unwrap_or(0.0);
        if execution_left.is_some_and(|seconds| seconds <= 0.)
            && !active_assignments.is_empty()
            && conclude_left.is_some_and(|left| left > 0.)
        {
            handle.draining.store(true, Ordering::SeqCst);
        } else if execution_left.is_some_and(|seconds| seconds <= 0.) {
            cancel_ctf_reason(&mut pending_ctf_reason, "wall_budget_exhausted").await;
            cancel_ctf_challenger(&mut pending_ctf_challenger).await;

            if let Err(error) = run::worker_result::settle_claims(
                &state,
                &run_id,
                "canceled",
                "coordinator_finalizing",
            ) {
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "worker.error",
                    json!({"reason":"worker_result_salvage_failed","detail":error}),
                );
            } else {
                graph.release_claims_for_finalize();
            }
            drop(graph);
            finalize_coordinator(&state, &run_id, false, "budget_exhausted", None).await;
            return;
        }
        if cost_budget > 0.0 && handle.cost_usage.lock().unwrap().total_usd >= cost_budget {
            cancel_ctf_reason(&mut pending_ctf_reason, "cost_budget_exhausted").await;
            cancel_ctf_challenger(&mut pending_ctf_challenger).await;

            if let Err(error) = run::worker_result::settle_claims(
                &state,
                &run_id,
                "canceled",
                "coordinator_finalizing",
            ) {
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "worker.error",
                    json!({"reason":"worker_result_salvage_failed","detail":error}),
                );
            } else {
                graph.release_claims_for_finalize();
            }
            drop(graph);
            finalize_coordinator(&state, &run_id, false, "budget_exhausted", None).await;
            return;
        }

        {
            let challenge_budget = worker_settings["review"]["max_challenges_per_cycle"]
                .as_u64()
                .or_else(|| {
                    worker_settings["stage_policy"]["coordinator"]["review"]
                        ["max_challenges_per_cycle"]
                        .as_u64()
                })
                .unwrap_or(8) as usize;
            let decisions = graph.drain_review_proposals(challenge_budget);
            if !decisions.is_empty() {
                let graph_events = graph.events();
                for decision in decisions {
                    if decision["decision"] == "accepted"
                        && let Some(seq) = decision["applied_seq"].as_i64()
                        && let Some(applied) = graph_events.iter().find(|event| event["seq"] == seq)
                    {
                        emit_event(
                            &state,
                            &run_id,
                            Some("coordinator"),
                            "blackboard.delta",
                            json!({
                                "kind": applied["kind"],
                                "seq": seq,
                                "proposal_seq": decision["proposal_seq"],
                                "payload": applied["payload"],
                            }),
                        );
                    }
                    emit_event(
                        &state,
                        &run_id,
                        Some("coordinator"),
                        "blackboard.delta",
                        json!({
                            "kind": "review_proposal_decision",
                            "proposal_seq": decision["proposal_seq"],
                            "marker": decision["marker"],
                            "decision": decision["decision"],
                            "reason": decision["reason"],
                            "applied_seq": decision["applied_seq"],
                        }),
                    );
                }
            }
        }

        let finished_solvers: Vec<String> = active_assignments
            .iter()
            .filter(|(_, a)| a.abort.is_finished())
            .map(|(k, _)| k.clone())
            .collect();

        for sid in finished_solvers {
            if let Some(assign) = active_assignments.remove(&sid) {
                if !assign.lane_key.is_empty() {
                    graph.release_lane(&assign.lane_key, &assign.solver_id);
                }
                if !assign.resource_key.is_empty() {
                    graph.release_resource_lock(
                        &assign.resource_key,
                        &assign.solver_id,
                        "worker_finished",
                    );
                }
                if graph.all_intents().iter().any(|intent| {
                    intent["intent_id"] == assign.intent_id
                        && intent["dispatch_state"] == "closed"
                        && intent["result"] == "superseded"
                }) {
                    continue;
                }

                let latest_doc = state
                    .store
                    .get("runs", &run_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| json!({}));
                let is_solved = latest_doc["solved"].as_bool().unwrap_or(false);
                let exit_slot = exit_reasons.remove(&assign.solver_id);
                let result_str = resolve_reaped_worker_exit_reason(
                    &state,
                    &run_id,
                    &graph,
                    &assign.solver_id,
                    &assign.intent_id,
                    exit_slot.as_ref(),
                    is_solved,
                );
                let wm_before_conclude = graph.semantic_graph_watermark();
                if let Err(error) = run::worker_result::salvage(
                    &state,
                    &run_id,
                    &assign.solver_id,
                    &assign.intent_id,
                    &result_str,
                    "worker_exit",
                ) {
                    emit_event(
                        &state,
                        &run_id,
                        Some(&assign.solver_id),
                        "worker.error",
                        json!({"reason":"worker_result_salvage_failed","detail":error}),
                    );
                    continue;
                }
                if assign.worker_role == "verifier" {
                    for ev in graph.events() {
                        if ev["seq"].as_i64().unwrap_or(0) > wm_before_conclude
                            && matches!(
                                ev["kind"].as_str(),
                                Some("fact_revalidated" | "fact_superseded" | "fact_reviewed")
                            )
                        {
                            emit_event(
                                &state,
                                &run_id,
                                Some(&assign.solver_id),
                                "blackboard.delta",
                                json!({
                                    "kind": ev["kind"],
                                    "seq": ev["seq"],
                                    "payload": ev["payload"],
                                }),
                            );
                        }
                    }
                }

                emit_event(
                    &state,
                    &run_id,
                    Some(&assign.solver_id),
                    "blackboard.delta",
                    json!({
                        "kind": "intent_concluded",
                        "intent_id": assign.intent_id,
                        "worker": assign.solver_id,
                        "result": result_str,
                    }),
                );

                if is_solved {
                    cancel_ctf_reason(&mut pending_ctf_reason, "run_solved").await;

                    if let Err(error) = run::worker_result::settle_claims(
                        &state,
                        &run_id,
                        "canceled",
                        "coordinator_finalizing",
                    ) {
                        emit_event(
                            &state,
                            &run_id,
                            None,
                            "worker.error",
                            json!({"reason":"worker_result_salvage_failed","detail":error}),
                        );
                    } else {
                        graph.release_claims_for_finalize();
                    }
                    drop(graph);
                    finalize_coordinator(&state, &run_id, true, "solved", Some(&assign.solver_id))
                        .await;
                    return;
                }
            }
        }

        if handle.draining.load(Ordering::SeqCst) {
            if active_assignments.is_empty() {
                emit_event(
                    &state,
                    &run_id,
                    Some("coordinator"),
                    "blackboard.delta",
                    json!({"kind":"operator_drain_complete"}),
                );
                if let Err(error) = run::worker_result::settle_claims(
                    &state,
                    &run_id,
                    "canceled",
                    "coordinator_finalizing",
                ) {
                    emit_event(
                        &state,
                        &run_id,
                        None,
                        "worker.error",
                        json!({"reason":"worker_result_salvage_failed","detail":error}),
                    );
                } else {
                    graph.release_claims_for_finalize();
                }
                drop(graph);
                let solved = state
                    .store
                    .get("runs", &run_id)
                    .ok()
                    .flatten()
                    .is_some_and(|run| run["solved"] == true);
                finalize_coordinator(
                    &state,
                    &run_id,
                    solved,
                    if solved {
                        "solved"
                    } else {
                        "coordinator: no verified flag"
                    },
                    None,
                )
                .await;
                return;
            }
            continue;
        }

        if handle.paused.load(Ordering::SeqCst) {
            continue;
        }
        if cost_budget > 0.0 {
            match run::usage::cost_coverage(&state, &run_id) {
                Ok(coverage) if coverage["known"] != true => {
                    if run_doc["awaiting_operator"] != "cost_usage_unknown" {
                        let mut updated = run_doc.clone();
                        updated["awaiting_operator"] = json!("cost_usage_unknown");
                        updated["cost_coverage"] = coverage.clone();
                        if state
                            .store
                            .put_many_if(
                                &[("runs", &run_id, Some(&run_doc))],
                                &[("runs", &run_id, &updated)],
                            )
                            .ok()
                            == Some(true)
                        {
                            emit_event(
                                &state,
                                &run_id,
                                None,
                                "blackboard.delta",
                                json!({"kind":"cost_coverage_hold","reason":"Invocation cost is unknown; new model calls await confirmed usage","coverage":coverage}),
                            );
                        }
                    }
                    continue;
                }
                Err(error) => {
                    emit_event(
                        &state,
                        &run_id,
                        None,
                        "worker.error",
                        json!({"reason":"cost_coverage_unavailable","detail":error}),
                    );
                    continue;
                }
                Ok(coverage) => {
                    if run_doc["awaiting_operator"] == "cost_usage_unknown" {
                        let mut updated = run_doc.clone();
                        updated["awaiting_operator"] = Value::Null;
                        updated["cost_coverage"] = coverage;
                        if state
                            .store
                            .put_many_if(
                                &[("runs", &run_id, Some(&run_doc))],
                                &[("runs", &run_id, &updated)],
                            )
                            .ok()
                            != Some(true)
                        {
                            continue;
                        }
                        emit_event(
                            &state,
                            &run_id,
                            None,
                            "blackboard.delta",
                            json!({"kind":"cost_coverage_restored"}),
                        );
                    }
                }
            }
        }

        let worker_budget = run_doc["max_total_workers"].as_u64().unwrap_or(0) as usize;
        if worker_budget > 0 && spawned_total >= worker_budget && active_assignments.is_empty() {
            if let Err(error) = run::worker_result::settle_claims(
                &state,
                &run_id,
                "canceled",
                "coordinator_finalizing",
            ) {
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "worker.error",
                    json!({"reason":"worker_result_salvage_failed","detail":error}),
                );
            } else {
                graph.release_claims_for_finalize();
            }
            drop(graph);
            finalize_coordinator(&state, &run_id, false, "budget_exhausted", None).await;
            return;
        }

        let current_wm = graph.semantic_graph_watermark();
        let open_intents = graph.open_dispatchable_intents();

        let ctf_trigger = { ctf_planner::planning_trigger(&graph.events(), last_decided_wm) };

        let needs_plan = pending_ctf_reason.is_none()
            && (ctf_trigger.is_some()
                || ctf_retry_at.is_some_and(|deadline| Instant::now() >= deadline));

        {
            if pending_ctf_challenger
                .as_ref()
                .is_some_and(PendingChallenger::is_finished)
            {
                let mut pending = pending_ctf_challenger
                    .take()
                    .expect("finished Challenger task");
                let result = pending.task.take().expect("Challenger join handle").await;
                match result {
                    Ok(Ok(plan)) => {
                        publish_intent_proposals(
                            &state,
                            &run_id,
                            &plan.proposed_intents,
                            "challenger",
                        );
                        emit_event(
                            &state,
                            &run_id,
                            Some("challenger"),
                            "blackboard.delta",
                            json!({
                            "kind":"challenger_finished","mode":"ctf",
                            "proposed":plan.proposed_intents.len()}),
                        );
                        if !plan.proposed_intents.is_empty() {
                            emit_event(
                                &state,
                                &run_id,
                                Some("challenger"),
                                "blackboard.delta",
                                json!({
                                "kind":"challenger_committed","proposed":plan.proposed_intents.len()}),
                            );
                        }
                    }
                    Ok(Err(detail)) => {
                        emit_event(
                            &state,
                            &run_id,
                            Some("challenger"),
                            "blackboard.delta",
                            json!({"kind":"challenger_failed","mode":"ctf","error":detail}),
                        );
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => {
                        emit_event(
                            &state,
                            &run_id,
                            Some("challenger"),
                            "blackboard.delta",
                            json!({"kind":"challenger_failed","mode":"ctf","error":error.to_string()}),
                        );
                    }
                }
            }
            if pending_ctf_reason
                .as_ref()
                .is_some_and(PendingReason::is_finished)
            {
                let mut pending = pending_ctf_reason.take().expect("finished CTF Reason task");
                let outcome = pending.task.take().expect("CTF Reason join handle").await;
                let result =
                    outcome.unwrap_or_else(|error| Err(format!("CTF Reason 任务中断：{error}")));
                match result {
                    Ok(plan) => {
                        if let Err(error)=run::board_context::save(&state,&run_id,&plan.context_update,&json!({"decisions":plan.intent_decisions,"proposed":plan.proposed_intents})) {
                            emit_event(&state,&run_id,Some("reason"),"context.save_failed",json!({"error":error,"cursor_advanced":false}));
                        }
                        let newer_input =
                            ctf_planner::planning_trigger(&graph.events(), pending.watermark)
                                .is_some();
                        last_decided_wm = if newer_input {
                            pending.watermark
                        } else {
                            plan.watermark
                        };
                        let proposed = &plan.proposed_intents;
                        let dropped = &plan.dropped_intent_ids;
                        let reprioritized = &plan.reprioritized_intent_ids;
                        publish_intent_proposals(&state, &run_id, proposed, "reason");
                        emit_event(
                            &state,
                            &run_id,
                            Some("reason"),
                            "blackboard.delta",
                            json!({
                            "kind":"reason_done","trigger":pending.trigger,
                            "watermark":pending.watermark,"requested_intents":pending.requested_intents,
                            "wave":pending.wave,"proposed":proposed.len(),"status":"ok",
                            "verdict":plan.reason.verdict,"progress_summary":plan.reason.progress_summary,
                            "progress_sections":plan.reason.progress_sections,
                            "superseded_intent_ids":dropped,"reprioritized_intent_ids":reprioritized,
                            "intent_decisions":plan.intent_decisions,
                            "reason_attempts":[plan.reason_attempt]}),
                        );
                        for cancelled in cancel_decide_dropped_workers(
                            &state,
                            &run_id,
                            &mut active_assignments,
                            dropped,
                        )
                        .await
                        {
                            if !cancelled.lane_key.is_empty() {
                                graph.release_lane(&cancelled.lane_key, &cancelled.solver_id);
                            }
                            if !cancelled.resource_key.is_empty() {
                                graph.release_resource_lock(
                                    &cancelled.resource_key,
                                    &cancelled.solver_id,
                                    "intent_dropped",
                                );
                            }
                        }
                        publish_reason_progress(
                            &state,
                            &run_id,
                            "ctf",
                            pending.trigger,
                            &plan.reason,
                            handle.seq.load(Ordering::SeqCst),
                        );
                        let current_run = state
                            .store
                            .get("runs", &run_id)
                            .ok()
                            .flatten()
                            .unwrap_or(Value::Null);
                        let platform_pending = current_run["platform_confirmation_required"]
                            == true
                            && current_run["solved"] != true
                            && current_run["flags"]
                                .as_array()
                                .is_some_and(|flags| !flags.is_empty());
                        if plan.reason.goal_met {
                            if current_run["solved"] == true {
                                if let Err(error) = run::worker_result::settle_claims(
                                    &state,
                                    &run_id,
                                    "canceled",
                                    "coordinator_finalizing",
                                ) {
                                    emit_event(
                                        &state,
                                        &run_id,
                                        None,
                                        "worker.error",
                                        json!({"reason":"worker_result_salvage_failed","detail":error}),
                                    );
                                } else {
                                    graph.release_claims_for_finalize();
                                }
                                drop(graph);
                                finalize_coordinator(&state, &run_id, true, "goal_met", None).await;
                                return;
                            }
                            emit_event(
                                &state,
                                &run_id,
                                Some("reason"),
                                "blackboard.delta",
                                json!({
                                "kind":if platform_pending {"goal_waiting_platform"} else {"goal_complete_rejected"},
                                "reason":if platform_pending {"platform_confirmation_pending"} else {"flags_incomplete"},
                                "fact_seqs":plan.goal_evidence_facts}),
                            );
                        }
                        let work_remains = !proposed.is_empty()
                            || !graph.open_dispatchable_intents().is_empty()
                            || !active_assignments.is_empty();
                        if platform_pending && !work_remains {
                            ctf_yielding = true;
                            ctf_planner_failures = 0;
                            ctf_retry_at = None;
                            emit_event(
                                &state,
                                &run_id,
                                Some("reason"),
                                "blackboard.delta",
                                json!({
                                "kind":"platform_confirmation_pending","flags":current_run["flags"]}),
                            );
                        } else if plan.yield_visit && !work_remains {
                            ctf_yielding = true;
                            ctf_planner_failures = 0;
                            ctf_retry_at = None;
                            emit_event(
                                &state,
                                &run_id,
                                Some("reason"),
                                "blackboard.delta",
                                json!({
                                "kind":"reason_yield_visit","summary":plan.reason.progress_summary}),
                            );
                        } else if work_remains || newer_input {
                            ctf_yielding = false;
                            ctf_planner_failures = 0;
                            ctf_retry_at = None;
                        } else {
                            ctf_yielding = true;
                            ctf_planner_failures = 0;
                            ctf_retry_at = None;
                        }
                    }
                    Err(detail) => {
                        last_decided_wm = graph.semantic_graph_watermark();
                        ctf_planner_failures += 1;
                        emit_event(
                            &state,
                            &run_id,
                            Some("reason"),
                            "blackboard.delta",
                            json!({
                            "kind":"reason_done","trigger":pending.trigger,
                            "watermark":pending.watermark,"proposed":0,"status":"failed",
                            "planner_failure":"planner_unavailable","planner_failure_detail":detail}),
                        );
                        if ctf_planner_failures >= 3 {
                            emit_event(
                                &state,
                                &run_id,
                                Some("reason"),
                                "blackboard.delta",
                                json!({
                                "kind":"planner_unavailable","attempts":ctf_planner_failures,
                                "detail":detail}),
                            );
                            if let Err(error) = run::worker_result::settle_claims(
                                &state,
                                &run_id,
                                "canceled",
                                "coordinator_finalizing",
                            ) {
                                emit_event(
                                    &state,
                                    &run_id,
                                    None,
                                    "worker.error",
                                    json!({"reason":"worker_result_salvage_failed","detail":error}),
                                );
                            } else {
                                graph.release_claims_for_finalize();
                            }
                            drop(graph);
                            finalize_coordinator(&state, &run_id, false, "runtime_failure", None)
                                .await;
                            return;
                        }
                        ctf_retry_at = Some(
                            Instant::now()
                                + Duration::from_secs(
                                    (2u64.pow(ctf_planner_failures.min(5))).min(30),
                                ),
                        );
                    }
                }
            }
            if needs_plan && pending_ctf_reason.is_none() {
                if ctf_trigger.is_some() {
                    ctf_planner_failures = 0;
                    ctf_retry_at = None;
                    ctf_yielding = false;
                }
                wave += 1;
                let trigger = ctf_trigger.unwrap_or(if ctf_retry_at.is_some() {
                    "retry"
                } else {
                    "quiescence"
                });
                let queued = open_intents
                    .iter()
                    .filter(|intent| {
                        matches!(
                            intent["worker_class"].as_str(),
                            Some("code" | "shell_agent")
                        )
                    })
                    .count();
                let requested = if unlimited_workers {
                    usize::MAX
                } else {
                    (ordinary_capacity.max(4).saturating_sub(queued)).max(2)
                };
                emit_event(
                    &state,
                    &run_id,
                    Some("reason"),
                    "blackboard.delta",
                    json!({
                    "kind":"reason_start","trigger":trigger,"watermark":current_wm,
                    "retry_index":ctf_planner_failures,"wave":wave,
                    "requested_intents":requested,"queued_ordinary_intents":queued,
                    "active_workers":active_assignments.len()}),
                );
                if pending_ctf_challenger.is_none() {
                    let done_count = graph
                        .all_intents()
                        .iter()
                        .filter(|intent| {
                            intent["status"] == "done" && intent["dispatch_state"] != "retired"
                        })
                        .count();
                    let flags: HashSet<String> = run_doc["flags"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|flag| flag.as_str().map(str::to_owned))
                        .collect();
                    let progress_count = if run_doc["platform_confirmation_required"] == true {
                        let confirmed: HashSet<String> = graph
                            .platform_verdicts()
                            .iter()
                            .filter(|receipt| {
                                matches!(receipt["status"].as_str(), Some("accepted" | "duplicate"))
                            })
                            .filter_map(|receipt| receipt["flag"].as_str().map(str::to_owned))
                            .collect();
                        flags.intersection(&confirmed).count()
                    } else {
                        flags.len()
                    };
                    if progress_count > challenger_last_progress_count {
                        challenger_last_progress_count = progress_count;
                        challenger_baseline_done = done_count;
                        last_challenger_done_count = done_count;
                    }
                    if done_count.saturating_sub(challenger_baseline_done) >= 6
                        && done_count.saturating_sub(last_challenger_done_count) >= 6
                    {
                        emit_event(
                            &state,
                            &run_id,
                            Some("challenger"),
                            "blackboard.delta",
                            json!({
                            "kind":"challenger_started","mode":"ctf"}),
                        );
                        let challenger_state = state.clone();
                        let challenger_run = run_id.clone();
                        let challenger_challenge = challenge.clone();
                        let task = tokio::spawn(async move {
                            ctf_planner::plan_challenger(
                                challenger_state,
                                &challenger_run,
                                &challenger_challenge,
                                requested.max(1),
                            )
                            .await
                        });
                        pending_ctf_challenger = Some(PendingChallenger { task: Some(task) });
                        last_challenger_done_count = done_count;
                    }
                }
                let retry = (ctf_planner_failures > 0).then(|| {
                    json!({
                    "retry_index":ctf_planner_failures+1,
                    "previous_failure":"planner_unavailable"})
                });
                let task_state = state.clone();
                let task_run = run_id.clone();
                let task_challenge = challenge.clone();
                let task = tokio::spawn(async move {
                    ctf_planner::plan_steps(
                        task_state,
                        &task_run,
                        &task_challenge,
                        requested,
                        retry.as_ref(),
                    )
                    .await
                });
                pending_ctf_reason = Some(PendingReason {
                    state: state.clone(),
                    run_id: run_id.clone(),
                    trigger,
                    watermark: current_wm,
                    requested_intents: requested,
                    wave,
                    task: Some(task),
                });
            }
        }

        // Python CTF uses Decide/Challenger, never automatic Review workers.

        let open_now = graph.open_dispatchable_intents();
        if unlimited_workers && !seat_templates.is_empty() {
            let needed = active_assignments.len() + open_now.len() + usize::from(false);
            while seats.len() < needed {
                let mut seat = seat_templates[seats.len() % seat_templates.len()].clone();
                let number = seats
                    .iter()
                    .filter(|existing| existing.norm_engine == seat.norm_engine)
                    .count()
                    + 1;
                seat.solver_id = if generation > 1 {
                    format!("cli-{}#{number}-g{generation}", seat.norm_engine)
                } else {
                    format!("cli-{}#{number}", seat.norm_engine)
                };
                seats.push(seat);
            }
        }
        let mut busy_solvers: HashSet<String> = active_assignments
            .values()
            .map(|assignment| assignment.seat_id.clone())
            .collect();

        let mut used_engines_in_batch: HashSet<String> = active_assignments
            .values()
            .map(|a| a.norm_engine.clone())
            .collect();

        for intent_row in open_now {
            if worker_budget > 0 && spawned_total >= worker_budget {
                break;
            }
            if busy_solvers.len() >= seats.len() {
                break;
            }
            let iid = intent_row["intent_id"].as_str().unwrap_or("").to_owned();
            if iid.is_empty() {
                continue;
            }
            let role = worker::role_for_worker_class(
                intent_row["worker_class"].as_str().unwrap_or("code"),
            );
            let active_role_count = active_assignments
                .values()
                .filter(|assignment| assignment.worker_role == role)
                .count()
                + usize::from(false);
            let global_role_capacity = match role {
                "review" | "verifier" => {
                    worker::global_role_capacity(role, &worker_settings, seats.len())
                }
                _ => ordinary_capacity.max(1),
            };
            if active_role_count >= global_role_capacity {
                continue;
            }
            let lane_key = intent_row["lane_key"].as_str().unwrap_or("").to_owned();
            let risk_class = intent_row["risk_class"]
                .as_str()
                .unwrap_or("read")
                .to_owned();
            let resource_key = intent_row["resource_key"].as_str().unwrap_or("").to_owned();

            let failed_for_iid = failed_engines.get(&iid);
            let Some(seat) = pick_worker_seat_matching(
                &seats,
                &busy_solvers,
                failed_for_iid,
                &used_engines_in_batch,
                &mut seat_cursor,
                |seat| {
                    worker::profile_allows_role(&seat.profile, role, &worker_settings)
                        && active_assignments
                            .values()
                            .filter(|assignment| {
                                assignment.profile_id == seat.profile_id
                                    && assignment.worker_role == role
                            })
                            .count()
                            < worker::profile_role_capacity(
                                &seat.profile,
                                role,
                                &worker_settings,
                                seats.len(),
                            )
                },
            ) else {
                continue;
            };

            let seat_id = seat.solver_id.clone();
            let mut dispatch_spec = seat.clone();
            dispatch_spec.solver_id =
                allocate_worker_label(&seat.norm_engine, generation, &mut worker_label_seq, &graph);
            let seat = &dispatch_spec;

            if !lane_key.is_empty()
                && !graph.lock_lane(&lane_key, &risk_class, &seat.solver_id, &iid, 600.0)
            {
                continue;
            }
            if !resource_key.is_empty()
                && !graph.request_resource_lock(&resource_key, &seat.solver_id, &iid, 600.0)
            {
                if !lane_key.is_empty() {
                    graph.release_lane(&lane_key, &seat.solver_id);
                }
                continue;
            }

            if !graph.claim_intent(&seat.solver_id, &iid, 600.0) {
                if !lane_key.is_empty() {
                    graph.release_lane(&lane_key, &seat.solver_id);
                }
                if !resource_key.is_empty() {
                    graph.release_resource_lock(&resource_key, &seat.solver_id, "claim_failed");
                }
                continue;
            }

            let assignment = IntentAssignment {
                intent_id: iid.clone(),
                goal: intent_row["goal"].as_str().unwrap_or("").to_owned(),
                expected_observable: intent_row["expected_observable"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
                stop_condition: intent_row["stop_condition"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
                coverage_key: intent_row["coverage_key"].as_str().unwrap_or("").to_owned(),
                reopen_hint: reopen_hint.clone(),
                worker_class: intent_row["worker_class"]
                    .as_str()
                    .unwrap_or("code")
                    .to_owned(),
            };

            let fact_count_at_start = graph.active_facts().len();
            let spawned = spawn_local_worker_with_intent(
                state.clone(),
                run_id.clone(),
                seat.clone(),
                challenge.clone(),
                assignment,
                false,
            );
            state
                .runs
                .set_worker_lane(&run_id, &seat.solver_id, &lane_key);

            busy_solvers.insert(seat_id.clone());
            used_engines_in_batch.insert(seat.norm_engine.clone());
            exit_reasons.insert(seat.solver_id.clone(), spawned.exit_reason);
            active_assignments.insert(
                seat.solver_id.clone(),
                ActiveAssignment {
                    seat_id,
                    solver_id: seat.solver_id.clone(),
                    norm_engine: seat.norm_engine.clone(),
                    profile_id: seat.profile_id.clone(),
                    worker_role: role.to_owned(),
                    intent_id: iid,
                    lane_key,
                    resource_key,
                    fact_count_at_start,
                    abort: spawned.abort,
                    cancel: spawned.cancel,
                    host_cancel: spawned.host_cancel,
                },
            );
            spawned_total += 1;
        }

        if active_assignments.is_empty() && graph.open_dispatchable_intents().is_empty() {
            if pending_ctf_reason.is_some()
                || pending_ctf_challenger.is_some()
                || ctf_retry_at.is_some()
                || ctf_planner::planning_trigger(&graph.events(), last_decided_wm).is_some()
            {
                continue;
            }

            let final_doc = state
                .store
                .get("runs", &run_id)
                .ok()
                .flatten()
                .unwrap_or_else(|| json!({}));
            let is_solved = final_doc["solved"].as_bool().unwrap_or(false);
            if ctf_yielding {
                emit_event(
                    &state,
                    &run_id,
                    Some("reason"),
                    "blackboard.delta",
                    json!({
                    "kind":"reason_no_progress","watermark":graph.semantic_graph_watermark(),
                    "detail":"Decide returned no actionable Step and no new evidence arrived"}),
                );
            }
            let reason = if is_solved {
                "solved"
            } else {
                "coordinator: no verified flag"
            };
            if let Err(error) = run::worker_result::settle_claims(
                &state,
                &run_id,
                "canceled",
                "coordinator_finalizing",
            ) {
                emit_event(
                    &state,
                    &run_id,
                    None,
                    "worker.error",
                    json!({"reason":"worker_result_salvage_failed","detail":error}),
                );
            } else {
                graph.release_claims_for_finalize();
            }
            drop(graph);
            finalize_coordinator(&state, &run_id, is_solved, reason, None).await;
            return;
        }
    }
}

async fn finalize_coordinator(
    state: &Arc<AppState>,
    run_id: &str,
    solved: bool,
    reason: &str,
    winner_solver: Option<&str>,
) {
    let handle = state.runs.handle(state, run_id);
    if handle.draining.swap(false, Ordering::SeqCst) {
        emit_event(
            state,
            run_id,
            Some("coordinator"),
            "blackboard.delta",
            json!({"kind":"operator_drain_completed",
                "reason":"coordinator epoch ended after draining in-flight work"}),
        );
    }
    let tids = state.runs.abort_workers(run_id);
    for tid in tids {
        state.agents.close(&tid).await;
    }
    match run::worker_result::settle_claims(state, run_id, "interrupted", "coordinator_finalizing")
    {
        Ok(()) => {
            if let Ok(graph) = run::graph::SharedGraph::open(state, run_id) {
                graph.release_claims_for_finalize();
            }
        }
        Err(error) => {
            emit_event(
                state,
                run_id,
                None,
                "worker.error",
                json!({"reason":"worker_result_salvage_failed","detail":error}),
            );
        }
    }
    crate::runs::container_exec::teardown_run_container(run_id).await;

    let run_doc = state
        .store
        .get("runs", run_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}));
    let is_solved = solved || run_doc["solved"].as_bool().unwrap_or(false);
    let terminal_reason = reason.to_owned();

    if is_solved {
        let sdir = session_dir(state, run_id);
        let _ = fs::create_dir_all(&sdir);
        let winner_identity = run::read_jsonl_events(state, run_id)
            .into_iter()
            .rev()
            .find(|event| {
                event["solver_id"].as_str() == winner_solver
                    && (event["payload"]["engine"].is_string()
                        || event["payload"]["profile_id"].is_string())
            })
            .unwrap_or(Value::Null);
        if let Some(worker) = winner_solver {
            let key = json!([run_id, worker]).to_string();
            if let Ok(Some(continuation)) = state.store.get("run_worker_continuations", &key) {
                if continuation["established"] == true {
                    if let Err(error) =
                        state
                            .store
                            .put("run_winner_continuations", run_id, &continuation)
                    {
                        tracing::warn!(run_id,%error,"Winner continuation persistence failed");
                    }
                }
            }
        }
        let winner_json = json!({
            "run_id": run_id,
            "winner": winner_solver.unwrap_or("coordinator"),
            "engine":winner_identity["payload"]["engine"],"profile_id":winner_identity["payload"]["profile_id"],
            "flag": run_doc["flag"],
            "flags": run_doc["flags"],
            "ts": now_secs(),
        });
        let _ = fs::write(sdir.join("winner.json"), winner_json.to_string());
    }

    if is_solved {
        match run::knowledge::save(state, run_id, winner_solver) {
            Ok(path) => {
                emit_event(
                    state,
                    run_id,
                    Some("coordinator"),
                    "knowledge.template.saved",
                    json!({"path":path,"source_run_id":run_id}),
                );
            }
            Err(error) => {
                tracing::warn!(run_id,%error,"Solved Run template could not be persisted")
            }
        }
    }
    mark_run_finished(
        state,
        run_id,
        winner_solver,
        is_solved,
        &terminal_reason,
        None,
    );
}
