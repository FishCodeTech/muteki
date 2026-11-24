use super::*;
use crate::{
    runs::{ensure_run_record, prepare_dispatch_contract},
    store::Store,
};
use std::path::PathBuf;

fn pi_pids() -> HashSet<u32> {
    let output = crate::host_process::std_command("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let parent = fields.next()?.parse::<u32>().ok()?;
            let command = fields.next()?;
            (parent == std::process::id() && command != "ps").then_some(pid)
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires local Pi, source pi-ollama account, and running DVWA"]
async fn real_host_pi_drop_keeps_sibling_running() {
    let source = PathBuf::from(
        std::env::var("MUTEKI_PENTEST_HOST_PI_ACCOUNT")
            .expect("set MUTEKI_PENTEST_HOST_PI_ACCOUNT to source pi-ollama account directory"),
    );
    let target = std::env::var("MUTEKI_PENTEST_HOST_DVWA")
        .unwrap_or_else(|_| "http://127.0.0.1:4280/".to_owned());
    let temp = tempfile::tempdir().unwrap();
    let copied = temp.path().join("source/state/_secrets/accounts/pi-ollama");
    fs::create_dir_all(&copied).unwrap();
    for name in [
        "API_KEY",
        "BASE_URL",
        "ENGINE",
        "MODEL",
        "MODEL_CAPABILITIES.json",
        "PROVIDER",
        "REVISION",
    ] {
        let file = source.join(name);
        if file.is_file() {
            fs::copy(file, copied.join(name)).unwrap();
        }
    }
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let store = Store::open(state_dir.join("muteki.sqlite")).unwrap();
    store.put("system", "state_dir", &json!(state_dir)).unwrap();
    crate::import::run(&temp.path().join("source"), &state_dir, &store).unwrap();
    let state = Arc::new(AppState::new(store));
    let run_id = "host-pi-drop-test";
    let (_, challenge) = prepare_dispatch_contract(&json!({
        "mode":"pentest", "name":"DVWA host worker drop", "target":target,
        "description":format!("Assess only {target} and verify its login behavior."),
    }))
    .unwrap();
    let mut doc = ensure_run_record(&state, run_id).unwrap();
    doc["mode"] = json!("pentest");
    doc["status"] = json!("running");
    doc["running"] = json!(true);
    doc["pentest_contract"] = challenge["pentest_contract"].clone();
    doc["task_contract"] = challenge["task_contract"].clone();
    doc["execution_generation"] = json!(1);
    state.store.put("runs", run_id, &doc).unwrap();
    let mut graph = SharedGraph::open(&state, run_id).unwrap();
    graph.seed_origin_fact(&challenge);
    for (id, worker) in [("step-drop", "cli-pi#1"), ("step-keep", "cli-pi#2")] {
        assert!(graph.propose_intent("reason", id, id, &json!({}), None) > 0);
        assert!(graph.claim_intent(worker, id, 600.0));
    }

    let spec = |id: &str| WorkerSpec {
        solver_id: id.to_owned(),
        profile_id: id.to_owned(),
        profile_label: id.to_owned(),
        raw_engine: "pi".to_owned(),
        norm_engine: "pi".to_owned(),
        adapter_id: "pi.rpc".to_owned(),
        instance_id: "default".to_owned(),
        model: Some("glm-5.3-flash:cloud".to_owned()),
        effort: None,
        account_id: "pi-ollama".to_owned(),
        effective_credential_id: "account:pi-ollama".to_owned(),
        backend: "local".to_owned(),
        profile: Value::Null,
    };
    let intent = |id: &str, goal: &str| IntentAssignment {
        intent_id: id.to_owned(),
        goal: goal.to_owned(),
        expected_observable: "HTTP evidence from the authorized DVWA page".to_owned(),
        stop_condition: "Record a bounded observation and stop".to_owned(),
        coverage_key: id.to_owned(),
        reopen_hint: None,
        worker_class: "code".to_owned(),
    };
    let before = pi_pids();
    let first = spawn_local_worker_with_intent(
        state.clone(),
        run_id.to_owned(),
        spec("cli-pi#1"),
        challenge.clone(),
        intent(
            "step-drop",
            "Inspect the DVWA login page and its form behavior",
        ),
        false,
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while state.agents.open_threads().await.len() < 1 {
        if first.abort.is_finished() || Instant::now() >= deadline {
            let events = super::super::read_jsonl_events(&state, run_id);
            let summary: Vec<Value> = events
                .iter()
                .map(|event| {
                    json!({
                        "type":event["event_type"], "reason":event["payload"]["reason"],
                        "detail":event["payload"]["detail"]
                    })
                })
                .collect();
            panic!("first host Pi session did not start: {summary:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let first_pids = pi_pids()
        .difference(&before)
        .copied()
        .collect::<HashSet<_>>();
    assert_eq!(first_pids.len(), 1, "could not identify first Pi CLI PID");
    let second = spawn_local_worker_with_intent(
        state.clone(),
        run_id.to_owned(),
        spec("cli-pi#2"),
        challenge.clone(),
        intent(
            "step-keep",
            "Inspect the DVWA landing page and record its HTTP response",
        ),
        false,
    );
    let mut active = HashMap::from([
        (
            "cli-pi#1".to_owned(),
            ActiveAssignment {
                seat_id: "cli-pi#1".to_owned(),
                solver_id: "cli-pi#1".to_owned(),
                norm_engine: "pi".to_owned(),
                profile_id: "cli-pi#1".to_owned(),
                worker_role: "explore".to_owned(),
                intent_id: "step-drop".to_owned(),
                lane_key: String::new(),
                resource_key: String::new(),
                fact_count_at_start: 0,
                abort: first.abort,
                cancel: first.cancel,
                host_cancel: first.host_cancel,
            },
        ),
        (
            "cli-pi#2".to_owned(),
            ActiveAssignment {
                seat_id: "cli-pi#2".to_owned(),
                solver_id: "cli-pi#2".to_owned(),
                norm_engine: "pi".to_owned(),
                profile_id: "cli-pi#2".to_owned(),
                worker_role: "explore".to_owned(),
                intent_id: "step-keep".to_owned(),
                lane_key: String::new(),
                resource_key: String::new(),
                fact_count_at_start: 0,
                abort: second.abort,
                cancel: second.cancel,
                host_cancel: second.host_cancel,
            },
        ),
    ]);
    let deadline = Instant::now() + Duration::from_secs(30);
    while state.agents.open_threads().await.len() < 2 {
        assert!(
            Instant::now() < deadline,
            "second host Pi session did not start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let second_pids = pi_pids()
        .difference(&before)
        .copied()
        .collect::<HashSet<_>>();
    assert_eq!(second_pids.len(), 2, "could not identify both Pi CLI PIDs");
    let sibling_pid = *second_pids.difference(&first_pids).next().unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!active["cli-pi#1"].abort.is_finished());
    assert!(!active["cli-pi#2"].abort.is_finished());

    let dropped = graph
        .pentest_decide_drop_steps(
            "reason",
            &["step-drop".to_owned()],
            "controlled replacement of the first Step",
        )
        .unwrap();
    assert_eq!(dropped, vec!["step-drop"]);
    let cancelled = cancel_decide_dropped_workers(&state, run_id, &mut active, &dropped).await;
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].solver_id, "cli-pi#1");
    let deadline = Instant::now() + Duration::from_secs(5);
    while first_pids.iter().any(|pid| pi_pids().contains(pid)) {
        assert!(Instant::now() < deadline, "dropped Pi CLI remained alive");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cancelled[0].abort.is_finished() {
        assert!(
            Instant::now() < deadline,
            "dropped host Worker task remained active"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        pi_pids().contains(&sibling_pid),
        "sibling Pi CLI exited with dropped Step"
    );
    assert!(!active["cli-pi#2"].abort.is_finished());
    let events = super::super::read_jsonl_events(&state, run_id);
    assert!(
        events
            .iter()
            .any(|event| event["payload"]["kind"] == "intent_drop_requested"
                && event["payload"]["intent_id"] == "step-drop")
    );
    let steps = graph.all_intents();
    assert!(steps.iter().any(|step| step["intent_id"] == "step-drop"
        && step["status"] == "done"
        && step["result"] == "superseded"));
    assert!(
        steps
            .iter()
            .any(|step| step["intent_id"] == "step-keep" && step["status"] == "claimed")
    );

    let gate = Arc::new(super::super::worker::HostStartGate {
        solver_id: "cli-pi#3".to_owned(),
        arrived: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    assert!(
        super::super::worker::HOST_START_GATE
            .set(gate.clone())
            .is_ok()
    );
    assert!(graph.propose_intent("reason", "step-prestart", "prestart", &json!({}), None) > 0);
    assert!(graph.claim_intent("cli-pi#3", "step-prestart", 600.0));
    let arrived = gate.arrived.notified();
    let third = spawn_local_worker_with_intent(
        state.clone(),
        run_id.to_owned(),
        spec("cli-pi#3"),
        challenge,
        intent(
            "step-prestart",
            "Inspect the DVWA login page after a new Step begins",
        ),
        false,
    );
    active.insert(
        "cli-pi#3".to_owned(),
        ActiveAssignment {
            seat_id: "cli-pi#3".to_owned(),
                solver_id: "cli-pi#3".to_owned(),
            norm_engine: "pi".to_owned(),
            profile_id: "cli-pi#3".to_owned(),
            worker_role: "explore".to_owned(),
            intent_id: "step-prestart".to_owned(),
            lane_key: String::new(),
            resource_key: String::new(),
            fact_count_at_start: 0,
            abort: third.abort,
            cancel: third.cancel,
            host_cancel: third.host_cancel,
        },
    );
    tokio::time::timeout(Duration::from_secs(10), arrived)
        .await
        .unwrap();
    assert_eq!(
        pi_pids().difference(&before).count(),
        1,
        "prestart Worker launched Pi before release"
    );
    let prestart = graph
        .pentest_decide_drop_steps(
            "reason",
            &["step-prestart".to_owned()],
            "cancelled before Pi session startup",
        )
        .unwrap();
    assert_eq!(prestart, vec!["step-prestart"]);
    let cancelled_prestart =
        cancel_decide_dropped_workers(&state, run_id, &mut active, &prestart).await;
    assert_eq!(cancelled_prestart.len(), 1);
    gate.release.notify_one();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cancelled_prestart[0].abort.is_finished() {
        assert!(
            Instant::now() < deadline,
            "prestart Worker did not settle after cancellation"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        pi_pids().difference(&before).count(),
        1,
        "cancelled prestart Worker launched a late Pi session"
    );
    let events = super::super::read_jsonl_events(&state, run_id);
    let drop_ids: Vec<&str> = events
        .iter()
        .filter(|event| event["payload"]["kind"] == "intent_drop_requested")
        .filter_map(|event| event["payload"]["intent_id"].as_str())
        .collect();
    assert_eq!(drop_ids, vec!["step-drop", "step-prestart"]);
    assert!(!events.iter().any(|event| event["solver_id"] == "cli-pi#3"
        && matches!(
            event["event_type"].as_str(),
            Some("text.delta" | "tool.start" | "tool.result")
        )));
    assert!(
        !graph
            .events()
            .iter()
            .any(|event| event["kind"] == "fact_added" && event["actor"] == "cli-pi#3")
    );
    assert!(pi_pids().contains(&sibling_pid));

    for assignment in active.values() {
        assignment.abort.abort();
    }
    state.agents.close_all().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !pi_pids().contains(&sibling_pid),
        "cleanup left sibling Pi CLI running"
    );
}
