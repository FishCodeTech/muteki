//! Host-check evidence before the graph's atomic WorkerResult boundary.
use super::*;
#[path = "ctf/worker_result.rs"]
mod ctf;
#[path = "pentest/worker_result.rs"]
mod pentest_mode;
pub(super) use pentest_mode::stage_report as stage_pentest_report;
pub(super) fn stage(
    state: &AppState,
    run_id: &str,
    worker: &str,
    intent: Option<&str>,
    request: &str,
    kind: &str,
    payload: &Value,
) -> Result<(), String> {
    let run = state
        .store
        .get("runs", run_id)
        .map_err(|e| e.to_string())?
        .ok_or("Worker Run not found")?;
    let epoch = run
        .get("target_epoch")
        .or_else(|| run.get("execution_generation"))
        .and_then(Value::as_u64)
        .ok_or("Worker epoch missing")?;
    let intent = intent
        .filter(|v| !v.is_empty())
        .ok_or("Worker staging requires its current Step")?;
    let graph = graph::SharedGraph::open(state, run_id)?;
    let mut draft = payload.clone();
    if kind == "observations" {
        if !draft["enables"].is_null() && !draft["enables"].is_string() {
            return Err("enables must describe a confirmed capability as text".into());
        }
        let events = graph.events();
        let cutoff = events
            .iter()
            .filter(|event| event["kind"] == "visit_wave_reset")
            .filter_map(|event| event["seq"].as_i64())
            .max()
            .unwrap_or(0);
        let facts: Vec<_> = events
            .iter()
            .filter(|event| {
                event["kind"] == "fact_added"
                    && event["seq"].as_i64().is_some_and(|seq| seq > cutoff)
            })
            .collect();
        let active: HashSet<i64> = graph
            .active_facts()
            .iter()
            .filter_map(|fact| fact["fact_seq"].as_i64())
            .collect();
        let mut supersedes = Vec::new();
        for token in draft["supersedes"]
            .as_str()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            let index = token
                .strip_prefix("fact_")
                .and_then(|index| index.parse::<usize>().ok())
                .filter(|index| *index > 0);
            let seq=index.and_then(|index|facts.get(index-1)).and_then(|event|event["seq"].as_i64())
                .or_else(||token.parse::<i64>().ok().filter(|seq|facts.iter().any(|event|event["seq"]==*seq)))
                .filter(|seq|active.contains(seq))
                .ok_or_else(||format!("unknown or non-revisable Fact reference in --supersedes: {token} (use fact_XXX from the graph, never fact_origin)"))?;
            if !supersedes.contains(&seq) {
                supersedes.push(seq);
            }
        }
        draft["supersedes"] = json!(supersedes);
    }
    graph.stage_worker_result(worker, intent, epoch, request, kind, &draft)
}
pub(super) fn staged_request(
    state: &AppState,
    run_id: &str,
    worker: &str,
    intent: Option<&str>,
    request: &str,
) -> Result<Option<(String, Value)>, String> {
    let run = state
        .store
        .get("runs", run_id)
        .map_err(|e| e.to_string())?
        .ok_or("Worker Run not found")?;
    let epoch = run
        .get("target_epoch")
        .or_else(|| run.get("execution_generation"))
        .and_then(Value::as_u64)
        .ok_or("Worker epoch missing")?;
    graph::SharedGraph::open(state, run_id)?.staged_request(
        worker,
        intent.ok_or("Worker staging requires its current Step")?,
        epoch,
        request,
    )
}
pub(super) fn commit(
    state: &AppState,
    run_id: &str,
    worker: &str,
    intent: &str,
    checkpoint: &str,
    payload: &Value,
    finish: bool,
    solved: bool,
) -> Result<Value, String> {
    let run = state
        .store
        .get("runs", run_id)
        .map_err(|e| e.to_string())?
        .ok_or("Worker result Run not found")?;
    match run["mode"].as_str() {
        None | Some("ctf") => ctf::commit(
            state, run_id, worker, intent, checkpoint, payload, finish, solved,
        ),
        Some("pentest") => pentest_mode::commit(
            state, run_id, worker, intent, checkpoint, payload, finish, solved,
        ),
        _ => Err("Unsupported Worker result mode".into()),
    }
}
pub(super) fn salvage(
    state: &AppState,
    run_id: &str,
    worker: &str,
    intent: &str,
    status: &str,
    detail: &str,
) -> Result<Value, String> {
    let graph = graph::SharedGraph::open(state, run_id)?;
    let row = graph
        .all_intents()
        .into_iter()
        .find(|row| row["intent_id"] == intent)
        .ok_or("Salvage Step does not exist")?;
    if row["status"] != "claimed" || row["worker"] != worker {
        return Ok(json!({"already_settled":true,"intent_id":intent}));
    }
    let events = graph.events();
    let observations=events.iter().filter(|event|event["actor"]==worker && event["kind"]=="observation_added" && event["payload"]["intent_id"]==intent).map(|event|json!({"seq":event["seq"],"text":event["payload"]["fact"],"provenance":event["payload"]["evidence_provenance"]})).collect::<Vec<_>>();
    let payload = json!({"status":status,"summary":detail,"result_detail":detail,"handoff_missing":true,"observations":[],"dead_ends":[],"pocs":[],"artifacts":[],"retained_observations":observations});
    commit(
        state,
        run_id,
        worker,
        intent,
        "",
        &payload,
        true,
        status == "solved",
    )
}

pub(super) fn settle_claims(
    state: &AppState,
    run_id: &str,
    status: &str,
    detail: &str,
) -> Result<(), String> {
    let graph = graph::SharedGraph::open(state, run_id)?;
    let mut failures = Vec::new();
    for row in graph
        .all_intents()
        .into_iter()
        .filter(|row| row["status"] == "claimed")
    {
        if let (Some(worker), Some(intent)) = (row["worker"].as_str(), row["intent_id"].as_str()) {
            if let Err(error) = salvage(state, run_id, worker, intent, status, detail) {
                failures.push(format!("{worker}/{intent}: {error}"));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn recorded_worker_result_deltas(state: &AppState, run: &str) -> Result<Vec<(Value, Value)>, String> {
    Ok(persisted_event_iter(state, run)?
        .filter(|event| {
            event["event_type"] == "blackboard.delta"
                && !event["payload"]["worker_result_commit_seq"].is_null()
        })
        .map(|event| {
            (
                event["payload"]["worker_result_commit_seq"].clone(),
                event["payload"]["seq"].clone(),
            )
        })
        .collect())
}

fn publish_result_with_records(
    state: &AppState,
    run: &str,
    worker: &str,
    receipt: &Value,
    events: &[Value],
    recorded: &mut Vec<(Value, Value)>,
) -> Result<(), String> {
    for (field, kind) in [
        ("superseded_event_seqs", "fact_superseded"),
        ("fact_seqs", "fact_added"),
        ("dead_end_seqs", "dead_end"),
        ("poc_seqs", "poc_saved"),
    ] {
        for seq in receipt[field].as_array().into_iter().flatten() {
            if recorded
                .iter()
                .any(|(commit_seq, recorded_seq)| *commit_seq == receipt["commit_seq"] && recorded_seq == seq)
            {
                continue;
            }
            let source = events
                .iter()
                .find(|event| event["seq"] == *seq)
                .ok_or("Committed Worker graph event missing")?;
            let mut payload = source["payload"].clone();
            payload["kind"] = json!(kind);
            payload["seq"] = seq.clone();
            payload["worker_result_commit_seq"] = receipt["commit_seq"].clone();
            payload["actor"] = source["actor"].clone();
            payload["verified"] = source["verified"].clone();
            if kind == "fact_added" {
                payload["fact_seq"] = seq.clone();
                if let Some(seq) = seq.as_u64() {
                    state
                        .runs
                        .handle(state, run)
                        .fact_seq
                        .fetch_max(seq, Ordering::SeqCst);
                }
            }
            if emit_event(state, run, Some(worker), "blackboard.delta", payload)["persisted"]
                == false
            {
                return Err("Worker result projection persistence failed".into());
            }
            recorded.push((receipt["commit_seq"].clone(), seq.clone()));
        }
    }
    Ok(())
}

fn publish_result(
    state: &AppState,
    run: &str,
    worker: &str,
    receipt: &Value,
) -> Result<(), String> {
    let events = graph::SharedGraph::open(state, run)?.events();
    let mut recorded = recorded_worker_result_deltas(state, run)?;
    publish_result_with_records(state, run, worker, receipt, &events, &mut recorded)
}
pub(super) fn recover_projections(state: &AppState, run: &str) -> Result<(), String> {
    let events = graph::SharedGraph::open(state, run)?.events();
    let committed: Vec<Value> = events
        .iter()
        .filter(|event| event["kind"] == "worker_result_committed")
        .cloned()
        .collect();
    if committed.is_empty() {
        return Ok(());
    }
    let mut recorded = recorded_worker_result_deltas(state, run)?;
    for event in committed {
        let mut receipt = event["payload"].clone();
        receipt["commit_seq"] = event["seq"].clone();
        publish_result_with_records(
            state,
            run,
            event["actor"]
                .as_str()
                .ok_or("Worker result actor missing")?,
            &receipt,
            &events,
            &mut recorded,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Arc<AppState>, graph::SharedGraph) {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(directory.path().join("muteki.sqlite")).unwrap();
        store
            .put("system", "state_dir", &json!(directory.path()))
            .unwrap();
        let state = Arc::new(AppState::new(store));
        state.store.put("runs","parity",&json!({"run_id":"parity","name":"parity","mode":"ctf","description":"Inspect attached data.","attachments":["input.zip"],"execution_generation":1,"target_epoch":1})).unwrap();
        let graph = graph::SharedGraph::open(&state, "parity").unwrap();
        let origin = graph.seed_origin_fact(&state.store.get("runs", "parity").unwrap().unwrap());
        graph.propose_intent(
            "reason",
            "step_001",
            "Inspect archive.",
            &json!({}),
            Some(&[origin]),
        );
        assert!(graph.claim_intent("worker", "step_001", 600.));
        (directory, state, graph)
    }
    fn draft(state: &AppState, request: &str, title: &str, content: &str) {
        stage(
            state,
            "parity",
            "worker",
            Some("step_001"),
            request,
            "observations",
            &json!({"title":title,"text":content,"claim_verified":true}),
        )
        .unwrap();
    }
    fn normalized(context: &str) -> String {
        context
            .lines()
            .filter(|line| {
                !line.trim_start().starts_with("createdAt:")
                    && !line.trim_start().starts_with("satisfiedAt:")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn ctf_model_context_matches_python_before_and_after_commit() {
        let (_directory, state, graph) = setup();
        let fixture: Value =
            serde_json::from_str(include_str!("fixtures/ctf-context-python.json")).unwrap();
        assert_eq!(
            normalized(&super::super::ctf_planner::worker_context(&state, "parity", "step_001").unwrap()),
            fixture["claimed"]
        );
        draft(
            &state,
            "draft-1",
            "Obsolete draft",
            "Unpublished preliminary text.",
        );
        draft(&state, "draft-2", "Archive contents", "Contains one image.");
        assert_eq!(graph.active_facts().len(), 1);
        let receipt = commit(
            &state,
            "parity",
            "worker",
            "step_001",
            "",
            &json!({"status":"explored"}),
            true,
            false,
        )
        .unwrap();
        assert_eq!(receipt["fact_seqs"].as_array().unwrap().len(), 1);
        assert_eq!(
            normalized(&super::super::ctf_planner::worker_context(&state, "parity", "step_001").unwrap()),
            fixture["committed"]
        );
        let count = graph.events().len();
        assert_eq!(
            commit(
                &state,
                "parity",
                "worker",
                "step_001",
                "",
                &json!({"status":"explored"}),
                true,
                false
            )
            .unwrap()["deduplicated"],
            true
        );
        assert_eq!(graph.events().len(), count);
    }
    #[test]
    fn interrupted_draft_remains_an_unadmitted_observation() {
        let (_directory, state, graph) = setup();
        draft(&state, "draft-1", "Incomplete", "Needs more evidence.");
        salvage(
            &state,
            "parity",
            "worker",
            "step_001",
            "timeout",
            "execution expired",
        )
        .unwrap();
        assert_eq!(graph.active_facts().len(), 1);
        let observations: Vec<_> = graph
            .events()
            .into_iter()
            .filter(|event| event["kind"] == "observation_added" && event["actor"]=="worker")
            .collect();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0]["payload"]["claimed_verified"], false);
        assert_eq!(graph.all_intents()[0]["status"], "done");
    }
    #[test]
    fn revision_is_atomic_and_invalid_reference_does_not_replace_draft() {
        let (_directory, state, graph) = setup();
        let old = graph.append_event(
            "fact_added",
            "worker",
            &json!({"fact":"Old\nPrior observation"}),
            None,
            true,
            1.,
            None,
        );
        stage(&state,"parity","worker",Some("step_001"),"valid","observations",&json!({"title":"Corrected","text":"New evidence","claim_verified":true,"supersedes":"fact_001"})).unwrap();
        assert!(stage(&state,"parity","worker",Some("step_001"),"invalid","observations",&json!({"title":"Bad","text":"Should not be saved","claim_verified":true,"supersedes":"fact_999"})).is_err());
        let receipt = commit(
            &state,
            "parity",
            "worker",
            "step_001",
            "",
            &json!({"status":"explored"}),
            true,
            false,
        )
        .unwrap();
        assert_eq!(receipt["superseded_fact_seqs"], json!([old]));
        assert!(
            !graph
                .active_facts()
                .iter()
                .any(|fact| fact["fact_seq"] == old)
        );
        assert!(
            graph
                .active_facts()
                .iter()
                .any(|fact| fact["fact"] == "Corrected\nNew evidence")
        );
    }
    #[test]
    fn another_worker_reads_the_complete_fact_artifact_only_in_its_run() {
        let (_dir,state,graph)=setup();
        let arts=session_dir(&state,"parity").join("arts");std::fs::create_dir_all(&arts).unwrap();
        let output=format!("{}\nTAIL: exact observed response", "raw evidence ".repeat(300));
        std::fs::write(arts.join("art-1.json"),json!({"artifact_id":"art-1","command":"inspect input.zip","content":output}).to_string()).unwrap();
        emit_event(&state,"parity",Some("worker"),"tool.result",json!({"artifact_id":"art-1","tool":"inspect"}));
        draft(&state,"fact-draft","Observed archive structure","The archive contains an image.");
        let receipt=commit(&state,"parity","worker","step_001","",&json!({"status":"done"}),true,false).unwrap();
        let fact=receipt["fact_seqs"][0].as_i64().unwrap();
        graph.propose_intent("reason","step_002","Inspect the image.",&json!({}),Some(&[fact]));assert!(graph.claim_intent("worker-b","step_002",600.));
        let result=super::super::evidence::read(&state,"parity","worker-b",Some("step_002"),"art-1",true).unwrap();
        assert_eq!(result["content"],output);assert_eq!(result["command"],"inspect input.zip");
        state.store.put("runs","other-run",&json!({"run_id":"other-run","mode":"ctf","target_epoch":1,"execution_generation":1})).unwrap();
        graph::SharedGraph::open(&state,"other-run").unwrap();
        assert!(super::super::evidence::read(&state,"other-run","worker-b",Some("step_002"),"art-1",true).is_err());
    }

}
