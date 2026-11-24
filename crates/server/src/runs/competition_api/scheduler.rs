//! Deterministic competition admission and budget projection.
use super::*;

/// Source CompetitionServices has one sequential scheduler loop. Keep that
/// service ownership across Rust processes sharing the same state directory.
pub(super) fn claim_service_owner(state: &AppState) -> std::io::Result<Option<std::fs::File>> {
    let path = native_chat::state_dir(state)
        .join("_competition")
        .join("scheduler-owner.lock");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

fn number(value: &Value) -> f64 {
    value
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(0.0)
}
fn string_set(value: &Value) -> std::collections::HashSet<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
fn since(timestamp: &Value) -> Option<f64> {
    let text = timestamp.as_str()?;
    let parsed = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    Some(
        chrono::Utc::now()
            .signed_duration_since(parsed)
            .num_milliseconds()
            .max(0) as f64
            / 1000.0,
    )
}

fn reopened_run_wallclock(
    run: &Value,
    events: impl Iterator<Item = Value>,
    now: f64,
) -> Option<f64> {
    let mut active_since = run["ts"].as_f64()?;
    let mut active = true;
    let mut total = 0.0;
    let mut reopened = false;
    for event in events {
        let Some(at) = event["ts"].as_f64() else { continue };
        match event["event_type"].as_str() {
            Some("run.finished") if active => {
                total += (at - active_since).max(0.0);
                active = false;
            }
            Some("run.reopened") if !active => {
                active_since = at;
                active = true;
                reopened = true;
            }
            _ => {}
        }
    }
    if !reopened { return None }
    if active {
        let end = if run["running"] == true { now }
            else { run["terminal_at"].as_f64().unwrap_or(now) };
        total += (end - active_since).max(0.0);
    }
    Some(total)
}

pub(super) fn detect_mode_change(state: &AppState, competition_id: &str, mode: &str) -> ApiResult<()> {
    static LAST_MODES: OnceLock<Mutex<HashMap<(PathBuf,String),String>>> = OnceLock::new();
    let previous = LAST_MODES.get_or_init(||Mutex::new(HashMap::new()))
        .lock().map_err(ApiError::internal)?
        .insert((native_chat::state_dir(state),competition_id.to_owned()),mode.to_owned());
    if previous.as_deref() == Some("autonomous") && mode != "autonomous" {
        append_event(state,competition_id,"competition",competition_id,
            "competition.policy.automation_changed","",
            json!({"competition_id":competition_id,"from":"autonomous","to":mode,"downgrade":true}))?;
    }
    Ok(())
}

pub(super) fn settle_budgets(state: &AppState, competition_id: &str, policy: &Value) -> ApiResult<Vec<Value>> {
    let bindings = scoped(state, BINDINGS, competition_id)?;
    let run_ids: std::collections::HashSet<String> = bindings
        .iter()
        .filter_map(|v| v["run_id"].as_str().map(str::to_owned))
        .collect();
    let usage = crate::usage_history::rows_for_runs(&state.store, &run_ids)
        .map_err(ApiError::internal)?;
    let mut tokens = 0.0;
    let mut cost = 0.0;
    let mut unpriced = false;
    for row in usage
        .iter()
        .filter(|v| v["run_id"].as_str().is_some_and(|id| run_ids.contains(id)))
    {
        tokens += number(&row["input_tokens"]) + number(&row["output_tokens"]);
        cost += number(&row["cost"]);
        if row["cost"].is_null()
            && (number(&row["input_tokens"]) + number(&row["output_tokens"])) > 0.0
        {
            unpriced = true;
        }
    }
    let mut wallclock = 0.0;
    for run_id in &run_ids {
        if let Some(run) = state
            .store
            .get("runs", run_id)
            .map_err(ApiError::internal)?
        {
            if run["execution_generation"].as_u64().unwrap_or(1) > 1 {
                let events = super::super::persisted_event_iter(state, run_id)
                    .map_err(ApiError::internal)?;
                if let Some(active) = reopened_run_wallclock(&run, events, super::super::now_secs()) {
                    wallclock += active;
                    continue;
                }
            }
            let start = run["ts"].as_f64().unwrap_or(0.0);
            let end = if run["running"] == true {
                super::super::now_secs()
            } else if let Some(terminal_at) = run["terminal_at"].as_f64() {
                terminal_at
            } else if let Some(cached) = state.store.get("competition_run_terminal_at", run_id)
                .map_err(ApiError::internal)?.and_then(|value| value.as_f64()) {
                cached
            } else {
                let observed = super::super::persisted_event_iter(state, run_id)
                    .map_err(ApiError::internal)?
                    .filter(|event| event["event_type"] == "run.finished")
                    .last()
                    .and_then(|event| event["ts"].as_f64());
                if let Some(terminal_at) = observed {
                    state.store.put("competition_run_terminal_at", run_id, &json!(terminal_at))
                        .map_err(ApiError::internal)?;
                }
                observed.unwrap_or_else(|| run["updated"].as_f64().unwrap_or(start))
            };
            wallclock += (end - start).max(0.0);
        }
    }
    let submissions = scoped(state, SUBMISSIONS, competition_id)?
        .iter()
        .map(|row| row["dispatch_count"].as_u64().unwrap_or(if row["state"] == "queued" {0} else {1}))
        .sum::<u64>() as f64;
    let instances = scoped(state, LEASES, competition_id)?
        .iter()
        .filter(|v| v["state"] == "active")
        .count() as f64;
    let observed = [
        ("tokens", tokens),
        ("cost", cost),
        ("wallclock", wallclock),
        ("submissions", submissions),
        ("instances", instances),
    ];
    let mut budgets = Vec::new();
    for (kind, used) in observed {
        let prior = state
            .store
            .get(BUDGETS, &format!("{competition_id}:{kind}"))
            .map_err(ApiError::internal)?;
        let limit = prior.as_ref().map_or_else(||number(&policy["budget_limits"][kind]),
            |row|number(&row["limit"]));
        if prior.is_none() && limit == 0.0 && used == 0.0 { continue; }
        let was_exhausted = prior.as_ref().is_some_and(|v| {
            number(&v["limit"]) > 0.0 && number(&v["used"]) >= number(&v["limit"])
        });
        let row = json!({"competition_id":competition_id,"kind":kind,"limit":limit,"used":used,"measurement_incomplete":kind == "cost" && unpriced,
            "before":prior.as_ref().map(|row|number(&row["used"])).unwrap_or(0.),"after":used,"delta":used-prior.as_ref().map(|row|number(&row["used"])).unwrap_or(0.),"window":"competition","resets_at":null,"updated_at":now()});
        if prior.as_ref().is_none_or(|v| {
            number(&v["used"]) != used
                || number(&v["limit"]) != limit
                || v["measurement_incomplete"] != row["measurement_incomplete"]
        }) {
            state
                .store
                .put(BUDGETS, &format!("{competition_id}:{kind}"), &row)
                .map_err(ApiError::internal)?;
        }
        if limit > 0.0 && used >= limit && !was_exhausted {
            append_event(
                state,
                competition_id,
                "competition",
                competition_id,
                "competition.scheduler.budget_exhausted",
                "",
                json!({"competition_id":competition_id,"kind":kind,"limit":limit,"used":used,"operation":"dispatch"}),
            )?;
        }
        budgets.push(row);
    }
    Ok(budgets)
}

fn auto_enqueue(state:&AppState,competition_id:&str,policy:&Value)->ApiResult<()>{
    if policy["automation_mode"]=="observe"{return Ok(())}
    let deny=string_set(&policy["category_deny"]);
    for candidate in scoped(state,CHALLENGES,competition_id)?{
        if candidate["tombstoned"]==true||candidate["remote_state"]!="open"||candidate["current_revision_id"].is_null()
            ||!matches!(field(&candidate,"state"),"discovered"|"selected")
            ||deny.contains(field(&candidate,"category")){continue}
        let cid=field(&candidate,"challenge_id");let mut settled=false;
        for _ in 0..32{
            let Some(before)=state.store.get(CHALLENGES,cid).map_err(ApiError::internal)?else{settled=true;break};
            let queue=state.store.get(QUEUE,cid).map_err(ApiError::internal)?;
            if before["tombstoned"]==true||before["remote_state"]!="open"||before["current_revision_id"].is_null()
                ||!matches!(field(&before,"state"),"discovered"|"selected")
                ||deny.contains(field(&before,"category"))
                ||queue.as_ref().is_some_and(|row|row["operator_hold"]==true||matches!(field(row,"state"),"queued"|"held"|"dispatching")){settled=true;break}
            let mut next=before.clone();let mut events=Vec::new();let mut from=field(&before,"state");
            for to in ["selected","queued"]{if from==to{continue}events.push(leases::planned_event(competition_id,"challenge",cid,"competition.challenge.state_changed","",json!({"challenge_id":cid,"from":from,"to":to,"reason":"scheduler.auto_enqueue"})));from=to;}
            next["state"]=json!("queued");next["paused_from"]=Value::Null;next["updated_at"]=json!(now());
            let mut entry=queue.clone().unwrap_or_else(||json!({"competition_id":competition_id,"competition_challenge_id":cid,"priority":0.,"score":0.,"not_before":null,"admission_decision":{},"enqueued_at":now()}));
            entry["state"]=json!("queued");entry["updated_at"]=json!(now());
            events.push(leases::planned_event(competition_id,"challenge",cid,"competition.queue.enqueued","",json!({"challenge_id":cid,"priority":entry["priority"],"reason":"scheduler.auto_enqueue"})));
            if put_records_with_events(state,competition_id,&[(CHALLENGES,cid,Some(&before)),(QUEUE,cid,queue.as_ref())],&[(CHALLENGES,cid,&next),(QUEUE,cid,&entry)],&events)?{settled=true;break}
        }
        if !settled{return Err(ApiError::new(StatusCode::CONFLICT,"competition.queue.changed","Queue changed repeatedly during automatic admission"))}
    }Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopened_run_budget_excludes_the_dormant_interval() {
        let events = [
            json!({"event_type":"run.started","ts":101.0}),
            json!({"event_type":"run.finished","ts":110.0}),
            json!({"event_type":"run.reopened","ts":1000.0}),
            json!({"event_type":"run.finished","ts":1015.0}),
        ];
        let run = json!({"ts":100.0,"terminal_at":1015.0,"running":false});
        assert_eq!(reopened_run_wallclock(&run, events.iter().cloned(), 2000.0), Some(25.0));
        let active = json!({"ts":100.0,"terminal_at":null,"running":true});
        assert_eq!(reopened_run_wallclock(&active, events[..3].iter().cloned(), 1012.0), Some(22.0));
    }

    #[test]
    fn existing_budget_bucket_keeps_its_limit_after_policy_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store.put("system","state_dir",&json!(dir.path())).unwrap();
        let state = AppState::new(store);
        state.store.put(BUDGETS,"comp-budget:submissions",&json!({
            "competition_id":"comp-budget","kind":"submissions","limit":1.0,"used":1.0})).unwrap();
        let mut policy = default_policy("comp-budget");
        policy["budget_limits"]["submissions"] = json!(2.0);
        settle_budgets(&state,"comp-budget",&policy).unwrap();
        assert_eq!(state.store.get(BUDGETS,"comp-budget:submissions")
            .unwrap().unwrap()["limit"],1.0);
    }

    #[test]
    fn tsec_revisit_states_are_dispatchable_without_changing_default_profile() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(dir.path())).unwrap();
        let state = AppState::new(store);
        let competition = json!({"competition_id":"comp-test","scheduler_state":"running","created_at":now()});
        let connection = json!({"connection_id":"pconn-test","status":"active","capabilities":{}});
        let mut policy = default_policy("comp-test");
        let revision = json!({"revision_id":"rev-test","description":"Find the flag","points":100});
        let entry = json!({"competition_challenge_id":"cch-test","state":"queued","enqueued_at":now(),"admission_decision":{"visit_count":1}});
        for challenge_state in ["running", "candidate_found", "submitting"] {
            let challenge = json!({"challenge_id":"cch-test","competition_id":"comp-test","state":challenge_state,"remote_state":"open","current_revision_id":"rev-test","category":"web","name":"Easy Web"});
            assert_eq!(evaluate(&state, &competition, &policy, &connection, &[], &challenge, Some(&revision), &entry, &Value::Null).unwrap()["reason"], "challenge_not_dispatchable");
            policy["policy_profile"] = json!("tsec_eval");
            assert_eq!(evaluate(&state, &competition, &policy, &connection, &[], &challenge, Some(&revision), &entry, &Value::Null).unwrap()["action"], "admit");
            policy["policy_profile"] = json!("default");
        }
    }

    #[test]
    fn observe_tick_leaves_discovered_challenges_out_of_queue() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(dir.path())).unwrap();
        let state = AppState::new(store);
        state.store.put(CONNECTIONS, "pconn-test", &json!({"connection_id":"pconn-test","platform_kind":"ctfd","status":"active"})).unwrap();
        let competition = json!({"competition_id":"comp-test","connection_id":"pconn-test","scheduler_state":"running","created_at":now()});
        state.store.put(COMPETITIONS, "comp-test", &competition).unwrap();
        let mut policy = default_policy("comp-test");
        policy["automation_mode"] = json!("observe");
        state.store.put(CHALLENGES, "cch-test", &json!({"challenge_id":"cch-test","competition_id":"comp-test","external_challenge_id":"7","name":"Easy Web","category":"web","current_revision_id":"rev-test","remote_state":"open","state":"discovered","tombstoned":false})).unwrap();
        state.store.put(REVISIONS, "rev-test", &json!({"revision_id":"rev-test","competition_id":"comp-test","description":"Find the flag","points":100})).unwrap();

        assert!(prepare_tick(&state, &competition, &policy).unwrap().is_empty());
        assert_eq!(state.store.get(CHALLENGES, "cch-test").unwrap().unwrap()["state"], "discovered");
        assert!(state.store.get(QUEUE, "cch-test").unwrap().is_none());
    }

    #[tokio::test]
    async fn terminal_policy_pauses_early_run_and_requeues_visit() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(dir.path())).unwrap();
        let state = Arc::new(AppState::new(store));
        state.store.put(CONNECTIONS, "pconn-test", &json!({"connection_id":"pconn-test","platform_kind":"ctfd","status":"active"})).unwrap();
        let competition = json!({"competition_id":"comp-test","connection_id":"pconn-test","scheduler_state":"running","created_at":now()});
        state.store.put(COMPETITIONS, "comp-test", &competition).unwrap();
        let mut policy = default_policy("comp-test");
        policy["terminal_phase_challenge_ids"] = json!(["final"]);
        state.store.put(CHALLENGES, "blocker", &json!({"challenge_id":"blocker","competition_id":"comp-test","external_challenge_id":"first","remote_state":"open","state":"discovered","tombstoned":false})).unwrap();
        state.store.put(CHALLENGES, "final", &json!({"challenge_id":"final","competition_id":"comp-test","external_challenge_id":"final","remote_state":"open","state":"running","tombstoned":false})).unwrap();
        state.store.put(BINDINGS, "bind-final", &json!({"binding_id":"bind-final","competition_id":"comp-test","competition_challenge_id":"final","run_id":"run-final","state":"running"})).unwrap();
        state.store.put("runs", "run-final", &json!({"run_id":"run-final","running":true,"solved":false,"control_generation":0})).unwrap();
        state.store.put(QUEUE, "final", &json!({"competition_id":"comp-test","competition_challenge_id":"final","state":"done","admission_decision":{"visit_count":1,"candidate_count_at_visit":0,"dry_waves":0}})).unwrap();

        enforce_terminal_phase_gate(&state, &competition, &policy).await.unwrap();
        assert_eq!(state.store.get(BINDINGS, "bind-final").unwrap().unwrap()["state"], "paused");
        assert_eq!(state.store.get(CHALLENGES, "final").unwrap().unwrap()["state"], "running");
        assert_eq!(state.store.get(QUEUE, "final").unwrap().unwrap()["state"], "queued");
        assert_eq!(state.store.get(QUEUE, "final").unwrap().unwrap()["admission_decision"]["dry_waves"], 1);
        assert_eq!(state.store.get("runs", "run-final").unwrap().unwrap()["running"], false);
        spawn_run_watcher(state.clone(), "comp-test".to_owned(), "final".to_owned(),
            "run-final".to_owned(), "bind-final".to_owned(), 0);
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(state.store.get(BINDINGS, "bind-final").unwrap().unwrap()["state"], "paused");
    }

    #[tokio::test]
    async fn deepchain_shrink_stops_excess_hard_binding() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(dir.path())).unwrap();
        let state = Arc::new(AppState::new(store));
        let competition = json!({"competition_id":"comp-test","scheduler_state":"running"});
        state.store.put(COMPETITIONS, "comp-test", &competition).unwrap();
        let mut policy = default_policy("comp-test");
        policy["policy_profile"] = json!("tsec_eval");
        policy["deepchain_slots"] = json!(1);
        for (id, multi, flags) in [("hard",true,1),("easy",false,2),("single",false,1)] {
            state.store.put(CHALLENGES,id,&json!({"challenge_id":id,"competition_id":"comp-test","external_challenge_id":id,"name":format!("{id} challenge"),"category":id,"current_revision_id":format!("rev-{id}"),"remote_state":"open","state":"running","tombstoned":false})).unwrap();
            state.store.put(REVISIONS,&format!("rev-{id}"),&json!({"revision_id":format!("rev-{id}"),"multi_flag":multi,"expected_flags":flags})).unwrap();
            if id == "single" { continue; }
            state.store.put(BINDINGS,&format!("bind-{id}"),&json!({"binding_id":format!("bind-{id}"),"competition_id":"comp-test","competition_challenge_id":id,"run_id":format!("run-{id}"),"state":"running","updated_at":now()})).unwrap();
            state.store.put("runs",&format!("run-{id}"),&json!({"run_id":format!("run-{id}"),"running":true,"solved":false,"control_generation":0})).unwrap();
            state.store.put(QUEUE,id,&json!({"competition_id":"comp-test","competition_challenge_id":id,"state":"done","admission_decision":{"visit_count":1,"candidate_count_at_visit":0}})).unwrap();
        }
        assert_eq!(deepchain_allowance(&state,"comp-test",&policy).unwrap(),1);
        state.store.delete(CHALLENGES,"single").unwrap();
        assert_eq!(deepchain_allowance(&state,"comp-test",&policy).unwrap(),2);
        enforce_deepchain(&state,&competition,&policy).await.unwrap();
        assert_eq!(state.store.get(BINDINGS,"bind-hard").unwrap().unwrap()["state"],"running");
        state.store.put(CHALLENGES,"single",&json!({"challenge_id":"single","competition_id":"comp-test","external_challenge_id":"single","current_revision_id":"rev-single","remote_state":"open","state":"discovered","tombstoned":false})).unwrap();
        enforce_deepchain(&state,&competition,&policy).await.unwrap();
        assert_eq!(state.store.get(BINDINGS,"bind-hard").unwrap().unwrap()["state"],"paused");
        assert_eq!(state.store.get(BINDINGS,"bind-easy").unwrap().unwrap()["state"],"running");
        assert_eq!(state.store.get("runs","run-hard").unwrap().unwrap()["running"],false);
        assert_eq!(state.store.get(QUEUE,"hard").unwrap().unwrap()["state"],"queued");
    }

    #[test]
    fn fill_idle_revisit_waits_for_first_visits_and_real_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(dir.path())).unwrap();
        let state = AppState::new(store);
        let competition = json!({"competition_id":"comp-test","created_at":now()});
        let connection = json!({"status":"active","capabilities":{}});
        let mut policy = default_policy("comp-test");
        policy["fill_idle_revisits"] = json!(true);
        policy["visit_floor_s"] = json!(3600.0);
        policy["max_concurrent_runs"] = json!(2);
        let challenge = json!({"challenge_id":"visited","competition_id":"comp-test","external_challenge_id":"visited","state":"queued","remote_state":"open","tombstoned":false,"category":"web"});
        let revision = json!({"description":"Find the flag","points":100});
        let entry = json!({"competition_challenge_id":"visited","state":"queued","enqueued_at":now(),"priority":0,"admission_decision":{"visit_count":1,"last_visit_at":now()}});
        state.store.put(CHALLENGES,"visited",&challenge).unwrap();
        state.store.put(QUEUE,"visited",&entry).unwrap();
        let first = evaluate(&state,&competition,&policy,&connection,&[],&challenge,Some(&revision),&entry,&Value::Null).unwrap();
        assert_eq!(first["action"],"admit");
        assert!(first["rules"].as_array().unwrap().iter().any(|rule|
            rule["rule"] == "visit_floor" && rule["passed"] == true && rule["reason"] == "idle_slot_revisit"));
        state.store.put(CHALLENGES,"fresh",&json!({"challenge_id":"fresh","competition_id":"comp-test","external_challenge_id":"fresh","state":"discovered","remote_state":"open","tombstoned":false})).unwrap();
        let blocked = evaluate(&state,&competition,&policy,&connection,&[],&challenge,Some(&revision),&entry,&Value::Null).unwrap();
        assert_eq!(blocked["reason"],"visit_floor");
        state.store.put(BINDINGS,"bind-fresh",&json!({"binding_id":"bind-fresh","competition_id":"comp-test","competition_challenge_id":"fresh","state":"planned"})).unwrap();
        let available = evaluate(&state,&competition,&policy,&connection,&[],&challenge,Some(&revision),&entry,&Value::Null).unwrap();
        assert_eq!(available["action"],"admit");
        state.store.put(BINDINGS,"bind-other",&json!({"binding_id":"bind-other","competition_id":"comp-test","competition_challenge_id":"other","state":"running"})).unwrap();
        let full = evaluate(&state,&competition,&policy,&connection,&[],&challenge,Some(&revision),&entry,&Value::Null).unwrap();
        assert_eq!(full["reason"],"visit_floor");
    }

    #[test]
    fn wrong_verdict_after_visit_bypasses_floor_but_older_wrong_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(dir.path())).unwrap();
        let state = AppState::new(store);
        let competition = json!({"competition_id":"comp-test","scheduler_state":"running","created_at":now()});
        let connection = json!({"status":"active","capabilities":{}});
        let mut policy = default_policy("comp-test");
        policy["visit_floor_s"] = json!(3600.0);
        policy["dry_defer_waves"] = json!(2);
        let challenge = json!({"challenge_id":"cch-test","competition_id":"comp-test",
            "external_challenge_id":"test","name":"Test","category":"web",
            "state":"queued","remote_state":"open","tombstoned":false});
        let revision = json!({"description":"Find the flag","points":100});
        let visit = (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
        let mut entry = json!({"competition_challenge_id":"cch-test","state":"queued",
            "enqueued_at":visit,"admission_decision":{"last_visit_at":visit,"visit_count":1,"dry_waves":0}});
        let submission_id = "sub-test";
        let older_wrong = (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339();
        state.store.put(SUBMISSIONS, submission_id, &json!({"competition_id":"comp-test",
            "competition_challenge_id":"cch-test","state":"wrong","updated_at":older_wrong})).unwrap();
        let old = evaluate(&state,&competition,&policy,&connection,&[],&challenge,
            Some(&revision),&entry,&Value::Null).unwrap();
        assert_eq!(old["action"], "defer");
        assert_eq!(old["reason"], "visit_floor");

        let newer_wrong = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        state.store.put(SUBMISSIONS, submission_id, &json!({"competition_id":"comp-test",
            "competition_challenge_id":"cch-test","state":"wrong","updated_at":newer_wrong})).unwrap();
        let fresh = evaluate(&state,&competition,&policy,&connection,&[],&challenge,
            Some(&revision),&entry,&Value::Null).unwrap();
        assert_eq!(fresh["action"], "admit");
        assert!(!fresh["rules"].as_array().unwrap().iter().any(|rule| rule["rule"] == "visit_floor"));

        state.store.delete(SUBMISSIONS, submission_id).unwrap();
        entry["admission_decision"]["dry_waves"] = json!(2);
        let blackhole = evaluate(&state,&competition,&policy,&connection,&[],&challenge,
            Some(&revision),&entry,&Value::Null).unwrap();
        assert_eq!(blackhole["action"], "defer");
        assert_eq!(blackhole["reason"], "visit_floor");
        assert!(blackhole["rules"].as_array().unwrap().iter().any(|rule|
            rule["rule"] == "blackhole_defer" && rule["passed"] == false));
    }

    #[test]
    fn budget_exhaustion_holds_auto_enqueued_challenge() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store
            .put("system", "state_dir", &json!(dir.path()))
            .unwrap();
        let state = AppState::new(store);
        state
            .store
            .put(
                CONNECTIONS,
                "pconn-test",
                &json!({"connection_id":"pconn-test","platform_kind":"ctfd","status":"active"}),
            )
            .unwrap();
        let competition = json!({"competition_id":"comp-test","connection_id":"pconn-test","scheduler_state":"running","created_at":now()});
        state
            .store
            .put(COMPETITIONS, "comp-test", &competition)
            .unwrap();
        let mut policy = default_policy("comp-test");
        policy["budget_limits"] = json!({"tokens":5.0});
        state.store.put(POLICIES, "comp-test", &policy).unwrap();
        state.store.put(CHALLENGES,"cch-test",&json!({"challenge_id":"cch-test","competition_id":"comp-test","external_challenge_id":"7","name":"Easy Web","category":"web","current_revision_id":"rev-test","remote_state":"open","state":"discovered","tombstoned":false})).unwrap();
        state.store.put(REVISIONS,"rev-test",&json!({"revision_id":"rev-test","competition_id":"comp-test","description":"Find the flag","points":100,"hints":[]})).unwrap();
        state.store.put(BINDINGS,"bind-prior",&json!({"binding_id":"bind-prior","competition_id":"comp-test","run_id":"run-prior","state":"finished"})).unwrap();
        state.store.put("usage","usage-prior",&json!({"id":"usage-prior","run_id":"run-prior","input_tokens":4,"output_tokens":3,"cost":0.1})).unwrap();
        let ranked = prepare_tick(&state, &competition, &policy).unwrap();
        assert!(ranked.is_empty());
        let entry = state.store.get(QUEUE, "cch-test").unwrap().unwrap();
        assert_eq!(
            entry["admission_decision"]["reason"],
            "budget_exhausted:tokens"
        );
        assert_eq!(
            state
                .store
                .get(BUDGETS, "comp-test:tokens")
                .unwrap()
                .unwrap()["used"],
            7.0
        );
    }

    #[test]
    fn tsec_profile_sorts_easy_before_hard() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store
            .put("system", "state_dir", &json!(dir.path()))
            .unwrap();
        let state = AppState::new(store);
        state
            .store
            .put(
                CONNECTIONS,
                "pconn-test",
                &json!({"connection_id":"pconn-test","platform_kind":"ctfd","status":"active"}),
            )
            .unwrap();
        let competition = json!({"competition_id":"comp-test","connection_id":"pconn-test","scheduler_state":"running","created_at":now()});
        state
            .store
            .put(COMPETITIONS, "comp-test", &competition)
            .unwrap();
        let mut policy = default_policy("comp-test");
        policy["policy_profile"] = json!("tsec_eval");
        for (id, name, points) in [("hard", "Hard Web", 1000), ("easy", "Easy Web", 100)] {
            state.store.put(CHALLENGES,id,&json!({"challenge_id":id,"competition_id":"comp-test","external_challenge_id":id,"name":name,"category":"web","current_revision_id":format!("rev-{id}"),"remote_state":"open","state":"discovered","tombstoned":false})).unwrap();
            state.store.put(REVISIONS,&format!("rev-{id}"),&json!({"revision_id":format!("rev-{id}"),"competition_id":"comp-test","description":"Find the flag","points":points,"hints":[]})).unwrap();
        }
        let ranked = prepare_tick(&state, &competition, &policy).unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0]["competition_challenge_id"], "easy");
        let mut selected = ranked[0].clone();
        let challenge = state.store.get(CHALLENGES, "easy").unwrap().unwrap();
        mark_admitted(
            &state,
            "comp-test",
            &mut selected,
            "run-easy",
            "bind-easy",
            &policy,
            &challenge,
        )
        .unwrap();
        assert_eq!(selected["admission_decision"]["persistent_execution"], true);
        prepare_tick(&state, &competition, &policy).unwrap();
        let retained = state.store.get(QUEUE, "easy").unwrap().unwrap();
        assert_eq!(retained["admission_decision"]["visit_count"], 1);
    }
}

fn difficulty(challenge: &Value) -> u8 {
    let text = format!(
        "{} {}",
        field(challenge, "category"),
        field(challenge, "name")
    )
    .to_ascii_lowercase();
    if text.contains("easy") {
        0
    } else if text.contains("hard") {
        2
    } else {
        1
    }
}
fn visit_band(entry: &Value, policy: &Value) -> u8 {
    let visits = visit::field(&entry,"visit_count")
        .as_u64()
        .unwrap_or(0);
    if visits == 0 {
        return 0;
    }
    let dry = visit::field(&entry,"dry_waves")
        .as_u64()
        .unwrap_or(0);
    let cap = policy["dry_defer_waves"].as_u64().unwrap_or(0);
    if cap > 0 && dry >= cap {
        return 3;
    }
    let floor = number(&policy["visit_floor_s"]);
    if floor > 0.0
        && since(&visit::field(&entry,"last_visit_at")).is_some_and(|age| age >= floor)
    {
        1
    } else {
        2
    }
}

fn terminal_blockers(
    state: &AppState,
    competition_id: &str,
    policy: &Value,
    challenge: &Value,
) -> ApiResult<Vec<String>> {
    let terminal = string_set(&policy["terminal_phase_challenge_ids"]);
    if !terminal.contains(field(challenge, "external_challenge_id")) {
        return Ok(Vec::new());
    }
    let mut blockers = Vec::new();
    for other in scoped(state, CHALLENGES, competition_id)? {
        if terminal.contains(field(&other, "external_challenge_id"))
            || other["tombstoned"] == true
            || matches!(field(&other, "remote_state"), "closed" | "solved_remote" | "hidden")
            || matches!(field(&other, "state"), "solved" | "skipped" | "retired" | "exhausted")
        {
            continue;
        }
        if policy["terminal_phase_fill_idle"] == true
            && state.store.get(QUEUE, field(&other, "challenge_id"))
                .map_err(ApiError::internal)?
                .is_some_and(|entry| visit::field(&entry,"visit_count").as_u64().unwrap_or(0) > 0)
        {
            continue;
        }
        blockers.push(field(&other, "external_challenge_id").to_owned());
    }
    blockers.sort();
    Ok(blockers)
}

fn binding_occupies(binding: &Value) -> bool {
    matches!(field(binding, "state"), "planned" | "creating" | "starting" | "running" | "resolving")
}

fn lease_occupies(lease: &Value) -> bool {
    matches!(field(lease, "state"), "requested" | "provisioning" | "active" | "renewing" | "releasing")
}

fn challenge_open(challenge: &Value) -> bool {
    challenge["tombstoned"] != true
        && challenge["remote_state"] == "open"
        && !matches!(field(challenge, "state"), "solved" | "retired" | "skipped" | "exhausted")
}

fn multi_flag(revision: Option<&Value>) -> bool {
    revision.is_some_and(|revision| revision["multi_flag"] == true
        || revision["expected_flags"].as_u64().unwrap_or(1) > 1)
}

fn occupying_multi_flag(
    state: &AppState,
    competition_id: &str,
    policy: &Value,
) -> ApiResult<Vec<(Value, Value)>> {
    let persistent = string_set(&policy["persistent_challenge_ids"]);
    let mut found = Vec::new();
    for binding in scoped(state, BINDINGS, competition_id)? {
        if !binding_occupies(&binding) { continue; }
        let Some(challenge) = state.store.get(CHALLENGES, field(&binding, "competition_challenge_id"))
            .map_err(ApiError::internal)? else { continue; };
        if persistent.contains(field(&challenge, "external_challenge_id")) { continue; }
        let revision = state.store.get(REVISIONS, field(&challenge, "current_revision_id"))
            .map_err(ApiError::internal)?;
        if multi_flag(revision.as_ref()) { found.push((binding, challenge)); }
    }
    Ok(found)
}

fn deepchain_allowance(state: &AppState, competition_id: &str, policy: &Value) -> ApiResult<u64> {
    let slots = policy["deepchain_slots"].as_u64().unwrap_or(0);
    if slots == 0 { return Ok(0); }
    for challenge in scoped(state, CHALLENGES, competition_id)? {
        if !challenge_open(&challenge) { continue; }
        let revision = state.store.get(REVISIONS, field(&challenge, "current_revision_id"))
            .map_err(ApiError::internal)?;
        if !multi_flag(revision.as_ref()) { return Ok(slots); }
    }
    Ok(slots.max(2))
}

pub(super) fn deepchain_hold(
    state: &AppState,
    competition_id: &str,
    policy: &Value,
    challenge: &Value,
    revision: &Value,
) -> ApiResult<bool> {
    if policy["policy_profile"] != "tsec_eval" || !multi_flag(Some(revision))
        || string_set(&policy["persistent_challenge_ids"])
            .contains(field(challenge, "external_challenge_id")) {
        return Ok(false);
    }
    let allowed = deepchain_allowance(state, competition_id, policy)?;
    Ok(allowed > 0 && occupying_multi_flag(state, competition_id, policy)?.len() as u64 >= allowed)
}

fn requeue_visit(state: &AppState, competition_id: &str, challenge_id: &str) -> ApiResult<()> {
    let Some(mut entry) = state.store.get(QUEUE, challenge_id).map_err(ApiError::internal)? else {
        return Ok(());
    };
    if entry["state"] != "done" { return Ok(()); }
    let current = scoped(state, CANDIDATES, competition_id)?.iter()
        .filter(|candidate| candidate["competition_challenge_id"] == challenge_id).count() as u64;
    let previous=entry.clone();
    visit::finish(&mut entry,None,current)?;
    entry["state"] = json!("queued");
    entry["updated_at"] = json!(now());
    save_queue_decision(state,challenge_id,&previous,&entry)?;
    Ok(())
}

pub(super) async fn enforce_terminal_phase_gate(
    state: &Arc<AppState>,
    competition: &Value,
    policy: &Value,
) -> ApiResult<()> {
    let competition_id = field(competition, "competition_id");
    let terminal = string_set(&policy["terminal_phase_challenge_ids"]);
    if terminal.is_empty() {
        return Ok(());
    }
    for challenge in scoped(state, CHALLENGES, competition_id)? {
        if !terminal.contains(field(&challenge, "external_challenge_id"))
            || terminal_blockers(state, competition_id, policy, &challenge)?.is_empty()
        {
            continue;
        }
        let challenge_id = field(&challenge, "challenge_id");
        let active_binding = scoped(state, BINDINGS, competition_id)?
            .into_iter()
            .find(|binding| binding["competition_challenge_id"] == challenge_id && binding["state"] == "running");
        if let Some(binding) = active_binding {
            binding_command(state, competition_id, "run_binding.pause", "",
                &json!({"challenge_id":challenge_id,"binding_id":binding["binding_id"]})).await?;
        }
        if let Some(lease) = scoped(state, LEASES, competition_id)?
            .into_iter()
            .find(|lease| lease["competition_challenge_id"] == challenge_id && lease["state"] == "active")
        {
            instance_command(state, competition_id, "instance.stop", "",
                &json!({"lease_id":lease["lease_id"]})).await?;
        }
        requeue_visit(state, competition_id, challenge_id)?;
    }
    Ok(())
}

pub(super) async fn enforce_deepchain(
    state: &Arc<AppState>,
    competition: &Value,
    policy: &Value,
) -> ApiResult<()> {
    if policy["policy_profile"] != "tsec_eval" { return Ok(()); }
    let competition_id = field(competition, "competition_id");
    let allowed = deepchain_allowance(state, competition_id, policy)?;
    if allowed == 0 { return Ok(()); }
    let occupying = occupying_multi_flag(state, competition_id, policy)?;
    let extras = occupying.len().saturating_sub(allowed as usize);
    if extras == 0 { return Ok(()); }
    let candidates = scoped(state, CANDIDATES, competition_id)?;
    let mut ranked = occupying.into_iter().map(|(binding, challenge)| {
        let candidate_count = candidates.iter()
            .filter(|candidate| candidate["competition_challenge_id"] == challenge["challenge_id"])
            .count();
        let near = if matches!(field(&challenge, "state"), "candidate_found" | "submitting")
            || candidate_count > 0 { 0_u8 } else { 1_u8 };
        let key = (near, std::cmp::Reverse(difficulty(&challenge)), candidate_count,
            field(&binding, "updated_at").to_owned());
        (key, binding, challenge)
    }).collect::<Vec<_>>();
    ranked.sort_by(|a, b| a.0.cmp(&b.0));
    let mut failures = Vec::new();
    for (_, binding, challenge) in ranked.into_iter().take(extras) {
        if binding["state"] != "running" { continue; }
        let challenge_id = field(&challenge, "challenge_id");
        if let Err(error) = binding_command(state, competition_id, "run_binding.pause", "",
            &json!({"challenge_id":challenge_id,"binding_id":binding["binding_id"]})).await {
            failures.push(format!("{challenge_id}:{}", error.code));
            continue;
        }
        if let Some(lease) = scoped(state, LEASES, competition_id)?.into_iter()
            .find(|lease| lease["competition_challenge_id"] == challenge_id && lease_occupies(lease)) {
            if let Err(error) = instance_command(state, competition_id, "instance.stop", "",
                &json!({"lease_id":lease["lease_id"]})).await {
                failures.push(format!("{challenge_id}:{}", error.code));
                continue;
            }
        }
        requeue_visit(state, competition_id, challenge_id)?;
    }
    if failures.is_empty() { Ok(()) } else {
        Err(ApiError::new(StatusCode::CONFLICT, "competition.scheduler.deepchain_failed", failures.join("; ")))
    }
}

fn fill_idle_revisit(
    state: &AppState,
    competition_id: &str,
    policy: &Value,
    challenge: &Value,
) -> ApiResult<bool> {
    if policy["fill_idle_revisits"] != true
        || string_set(&policy["persistent_challenge_ids"])
            .contains(field(challenge, "external_challenge_id")) {
        return Ok(false);
    }
    let bindings = scoped(state, BINDINGS, competition_id)?;
    let active_runs = bindings.iter().filter(|binding| binding_occupies(binding)).count() as u64;
    let run_limit = policy["max_concurrent_runs"].as_u64().unwrap_or(0);
    if run_limit > 0 && active_runs >= run_limit { return Ok(false); }
    let active_instances = scoped(state, LEASES, competition_id)?.iter()
        .filter(|lease| lease_occupies(lease)).count() as u64;
    let instance_limit = policy["max_instances"].as_u64().unwrap_or(0);
    if instance_limit > 0 && active_instances >= instance_limit { return Ok(false); }
    let terminal = string_set(&policy["terminal_phase_challenge_ids"]);
    for other in scoped(state, CHALLENGES, competition_id)? {
        if terminal.contains(field(&other, "external_challenge_id")) || !challenge_open(&other) {
            continue;
        }
        let id = field(&other, "challenge_id");
        if bindings.iter().any(|binding| binding["competition_challenge_id"] == id
            && binding_occupies(binding)) { continue; }
        let visited = state.store.get(QUEUE, id).map_err(ApiError::internal)?
            .is_some_and(|entry| visit::field(&entry,"visit_count").as_u64().unwrap_or(0) > 0);
        if !visited { return Ok(false); }
    }
    Ok(true)
}

fn gate(rules: &mut Vec<Value>, reason: &mut String, rule: &str, passed: bool, failed_reason: &str, detail: Value) {
    if !passed && reason.is_empty() {
        *reason = failed_reason.to_owned();
    }
    rules.push(json!({"rule":rule,"passed":passed,"reason":if passed && failed_reason != "idle_slot_revisit" { "" } else { failed_reason },"detail":detail}));
}

fn evaluate(
    state: &AppState,
    competition: &Value,
    policy: &Value,
    connection: &Value,
    budget_rows: &[Value],
    challenge: &Value,
    revision: Option<&Value>,
    entry: &Value,
    advice: &Value,
) -> ApiResult<Value> {
    let competition_id = field(competition, "competition_id");
    let cid = field(challenge, "challenge_id");
    let mode = field(policy, "automation_mode");
    let mut rules = Vec::new();
    let mut reason = String::new();
    gate(&mut rules, &mut reason,
        "automation_mode",
        mode != "observe",
        "automation_mode_observe",
        json!({"mode":mode}),
    );
    gate(&mut rules, &mut reason,
        "challenge_state",
        matches!(field(challenge, "state"), "queued" | "provisioning")
            || policy["policy_profile"] == "tsec_eval"
                && matches!(field(challenge, "state"), "running" | "candidate_found" | "submitting"),
        "challenge_not_dispatchable",
        json!({"state":challenge["state"]}),
    );
    gate(&mut rules, &mut reason,
        "tombstone",
        challenge["tombstoned"] != true,
        "tombstoned",
        json!({}),
    );
    gate(&mut rules, &mut reason,
        "remote_state",
        challenge["remote_state"] == "open",
        if challenge["remote_state"] == "solved_remote" {
            "remote_solved"
        } else {
            "remote_locked"
        },
        json!({"remote_state":challenge["remote_state"]}),
    );
    gate(&mut rules, &mut reason,
        "operator_hold",
        entry["operator_hold"] != true,
        "operator_hold",
        json!({}),
    );
    let deny = string_set(&policy["category_deny"]);
    let category = field(challenge, "category");
    gate(&mut rules, &mut reason,
        "category_policy",
        category.is_empty() || !deny.contains(category),
        "category_denied",
        json!({"category":category}),
    );
    let blockers = terminal_blockers(state, competition_id, policy, challenge)?;
    gate(&mut rules, &mut reason,
        "terminal_phase",
        blockers.is_empty(),
        "terminal_phase_wait",
        json!({"remaining":blockers.len(),"blockers":blockers.iter().take(10).collect::<Vec<_>>() }),
    );
    gate(&mut rules, &mut reason,
        "connection_auth",
        connection["status"] != "auth_required",
        "auth_required",
        json!({}),
    );
    let automation_off = matches!(
        field(&connection["capabilities"], "automation"),
        "off" | "disabled" | "readonly"
    );
    gate(&mut rules, &mut reason,
        "platform_automation",
        !automation_off,
        "platform_policy",
        json!({}),
    );
    for budget in budget_rows {
        let limit = number(&budget["limit"]);
        let used = number(&budget["used"]);
        let kind = field(budget, "kind");
        if limit > 0.0 {
            if budget["measurement_incomplete"] == true {
                gate(&mut rules, &mut reason,
                    &format!("budget:{kind}"),
                    false,
                    &format!("budget_unmetered:{kind}"),
                    json!({"limit":limit,"used":used}),
                );
                continue;
            }
            gate(&mut rules, &mut reason,
                &format!("budget:{kind}"),
                used < limit,
                &format!("budget_exhausted:{kind}"),
                json!({"limit":limit,"used":used}),
            );
        }
    }
    gate(&mut rules, &mut reason, "revision", revision.is_some(), "no_revision", json!({}));
    let has_content = revision.is_some_and(|v| {
        !field(v, "description").is_empty()
            || !field(v, "target").is_empty()
            || v["artifacts"]
                .as_array()
                .is_some_and(|files| !files.is_empty())
    });
    gate(&mut rules, &mut reason,
        "challenge_content",
        has_content,
        "missing_description_or_target",
        json!({}),
    );
    let persistent = string_set(&policy["persistent_challenge_ids"])
        .contains(field(challenge, "external_challenge_id"));
    let total_budget = number(&policy["total_budget_s"]);
    if !persistent && total_budget > 0.0 {
        let elapsed = since(&competition["created_at"]).unwrap_or(0.0);
        gate(&mut rules, &mut reason,
            "total_budget",
            elapsed < total_budget,
            "total_budget_exhausted",
            json!({"elapsed":elapsed,"limit":total_budget}),
        );
    }
    let floor = number(&policy["visit_floor_s"]);
    let visit_age = since(&visit::field(&entry,"last_visit_at"));
    let wrong_submissions: Vec<_> = scoped(state, SUBMISSIONS, competition_id)?
        .into_iter().filter(|submission| submission["competition_challenge_id"] == cid && submission["state"] == "wrong")
        .collect();
    let last_wrong = wrong_submissions.iter()
        .filter_map(|submission| submission["updated_at"].as_str())
        .filter_map(|text| chrono::DateTime::parse_from_rfc3339(text).ok()).max();
    let last_visit = visit::field(&entry,"last_visit_at").as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok());
    let wrong_after_visit = last_wrong.is_some_and(|wrong| last_visit.is_none_or(|visit| wrong > visit));
    if reason.is_empty() && !persistent && floor > 0.0 && visit_age.is_some() {
        let fill_idle = fill_idle_revisit(state, competition_id, policy, challenge)?;
        if fill_idle {
            gate(&mut rules, &mut reason, "visit_floor", true, "idle_slot_revisit", json!({"age":visit_age,"floor":floor}));
        } else if !wrong_after_visit {
            let age = visit_age.unwrap_or(0.0);
            let overdue_mult = number(&policy["overdue_mult"]);
            let overdue_mult = if overdue_mult > 0.0 {overdue_mult} else {1.25};
            if age < floor && age < floor * overdue_mult {
                let dry_waves = visit::field(&entry,"dry_waves").as_u64().unwrap_or(0);
                let dry_cap = policy["dry_defer_waves"].as_u64().unwrap_or(0);
                if dry_cap > 0 && dry_waves >= dry_cap {
                    rules.push(json!({"rule":"blackhole_defer","passed":false,"reason":"blackhole_defer","detail":{"dry_waves":dry_waves}}));
                    reason = "visit_floor".to_owned();
                } else {
                    gate(&mut rules, &mut reason, "visit_floor", false, "visit_floor", json!({"age":age,"floor":floor}));
                }
            }
        }
    }
    let backoff_due = entry["not_before"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|at| at > chrono::Utc::now());
    if backoff_due && (reason.is_empty() || reason == "visit_floor") {
        gate(&mut rules, &mut reason,
            "failure_backoff",
            false,
            "failure_backoff",
            json!({"not_before":entry["not_before"]}),
        );
        reason = "failure_backoff".to_owned();
    }
    let points = revision.map(|v| number(&v["points"])).unwrap_or(0.0);
    let hints = revision
        .and_then(|v| v["hints"].as_array())
        .map_or(0.0, |a| a.len() as f64 * 5.0);
    let waiting = since(&entry["enqueued_at"]).unwrap_or(0.0) / 60.0;
    let candidates = scoped(state, CANDIDATES, competition_id)?
        .iter()
        .filter(|v| v["competition_challenge_id"] == cid)
        .count();
    let wrong = wrong_submissions.len();
    let failed = scoped(state, BINDINGS, competition_id)?.iter()
        .filter(|binding| binding["competition_challenge_id"] == cid && binding["state"] == "failed")
        .count();
    let identifiers: std::collections::HashSet<&str> = [field(challenge, "external_challenge_id"), field(challenge, "name")]
        .into_iter().filter(|value| !value.is_empty()).collect();
    let mut blocking = 0.0;
    if revision.is_some() {
        for other in scoped(state, CHALLENGES, competition_id)? {
            if other["challenge_id"] == cid || !challenge_open(&other) { continue; }
            let Some(revision_id) = other["current_revision_id"].as_str() else { continue; };
            let Some(other_revision) = state.store.get(REVISIONS, revision_id).map_err(ApiError::internal)? else { continue; };
            if other_revision["prerequisites"].as_array().is_some_and(|items| items.iter()
                .filter_map(Value::as_str).any(|key| identifiers.contains(key))) {
                blocking += 200.0;
            }
        }
    }
    let mut active_categories = std::collections::HashSet::new();
    for binding in scoped(state, BINDINGS, competition_id)?.into_iter().filter(binding_occupies) {
        if let Some(active_challenge) = state.store.get(CHALLENGES, field(&binding, "competition_challenge_id"))
            .map_err(ApiError::internal)? {
            active_categories.insert(field(&active_challenge, "category").to_owned());
        }
    }
    let endgame = competition["ends_at"].as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .is_some_and(|end| end.signed_duration_since(chrono::Utc::now()).num_seconds() <= 3600);
    let mut components = json!({"operator_priority":if number(&entry["priority"]) > 0.0 {1000000.0} else {0.0},
        "blocking_dependents":blocking,"endgame_urgency":if endgame {25.0} else {0.0},
        "points":points,"hints":hints,"healthy_category":0.0,
        "waiting_minutes":waiting,"category_coverage":if active_categories.contains(category) {0.0} else {30.0},
        "recent_failure_penalty":-50.0*failed as f64,
        "existing_candidate":if candidates > 0 {40.0} else {0.0},
        "wrong_submission_penalty":-20.0*wrong as f64});
    if policy["policy_profile"] == "tsec_eval" {
        components["difficulty_priority"] = json!(90.0 - 45.0 * difficulty(challenge) as f64);
        components["near_solve"] = json!(if candidates > 0 || matches!(field(challenge, "state"), "candidate_found" | "submitting") {500.0} else {0.0});
        components["visit_band"] = json!(visit_band(entry,policy));
    }
    let score = components
        .as_object()
        .map(|o| o.values().filter_map(Value::as_f64).sum::<f64>())
        .unwrap_or(0.0);
    let action = if mode == "observe" {
        "observe"
    } else if reason == "visit_floor" || reason == "failure_backoff" {
        "defer"
    } else if reason.is_empty() {
        "admit"
    } else {
        "hold"
    };
    let reason = if action == "admit" {
        "admitted".to_owned()
    } else if action == "observe" {
        "automation_mode_observe".to_owned()
    } else {
        reason
    };
    let related: Vec<Value> = advice["recommendations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| field(v, "challenge_id").is_empty() || field(v, "challenge_id") == cid)
        .cloned()
        .collect();
    let advisor = json!({"consulted":advice["status"] != "disabled","status":advice["status"],"recommendations":related});
    let bindings=scoped(state,BINDINGS,competition_id)?;let leases=scoped(state,LEASES,competition_id)?;
    let inputs=json!({"challenge":{"challenge_id":cid,"external_challenge_id":challenge["external_challenge_id"],"state":challenge["state"],"remote_state":challenge["remote_state"],"unlocked":challenge["remote_state"]=="open","category":category,"points":points,"revision_id":challenge["current_revision_id"],"hints":revision.map(|row|row["hints"].clone()),"prerequisites":revision.map(|row|row["prerequisites"].clone())},
        "competition":{"competition_id":competition_id,"scheduler_state":competition["scheduler_state"],"ends_at":competition["ends_at"],"automation_mode":mode},
        "queue":{"priority":entry["priority"],"state":entry["state"],"enqueued_at":entry["enqueued_at"],"not_before":entry["not_before"],"waiting_seconds":since(&entry["enqueued_at"])},
        "capacity":{"active_runs":bindings.iter().filter(|row|matches!(field(row,"state"),"planned"|"starting"|"running"|"resolving")).count(),"active_instances":leases.iter().filter(|row|lease_occupies(row)).count(),"worker_total":bindings.iter().filter_map(|row|row["run_id"].as_str()).map(|run|state.runs.active_worker_count(run)).sum::<usize>()},
        "policy":policy,"connection":{"connection_id":connection["connection_id"],"platform_kind":connection["platform_kind"],"status":connection["status"],"capabilities":connection["capabilities"]},"signals":components,"budget_changes":budget_rows,"decided_at":now()});
    Ok(json!({"decision_id":id("adm"),"competition_id":competition_id,"challenge_id":cid,"action":action,"reason":reason,"score":score,"score_components":components,"rules":rules,"inputs":inputs,"budget_changes":budget_rows,"advisor":advisor,"decided_at":now()}))

}

#[cfg(test)]
pub(super) fn prepare_tick(
    state: &AppState,
    competition: &Value,
    policy: &Value,
) -> ApiResult<Vec<Value>> {
    let competition_id = field(competition, "competition_id");
    let budgets = settle_budgets(state, competition_id, policy)?;
    let advice = advisor::latest(state, competition_id);
    prepare_tick_with_advice(state, competition, policy, &budgets, &advice)
}

pub(super) fn prepare_tick_with_advice(
    state: &AppState,
    competition: &Value,
    policy: &Value,
    budgets: &[Value],
    advice: &Value,
) -> ApiResult<Vec<Value>> {
    let competition_id = field(competition, "competition_id");
    let connection = live_connection(state, field(competition, "connection_id"))?;
    if policy["automation_mode"] != "observe" {
        auto_enqueue(state, competition_id, policy)?;
    }
    let challenges = scoped(state, CHALLENGES, competition_id)?;
    let mut ranked = Vec::new();
    for mut entry in scoped(state, QUEUE, competition_id)? {
        if !matches!(field(&entry, "state"), "queued" | "held") {
            continue;
        }
        let cid = field(&entry, "competition_challenge_id").to_owned();
        let Some(challenge) = challenges.iter().find(|v| v["challenge_id"] == cid) else {
            continue;
        };
        let revision = challenge["current_revision_id"]
            .as_str()
            .and_then(|rid| state.store.get(REVISIONS, rid).ok().flatten());
        let mut decision = evaluate(
            state,
            competition,
            policy,
            &connection,
            budgets,
            challenge,
            revision.as_ref(),
            &entry,
            advice,
        )?;
        let original=entry.clone();
        visit::project(&mut entry)?;
        let previous=entry["admission_decision"].clone();
        let mut projected=entry.clone();
        projected["admission_decision"]=decision;
        visit::project(&mut projected)?;
        decision=projected["admission_decision"].clone();
        let changed =
            previous["action"] != decision["action"] || previous["reason"] != decision["reason"];
        let due = since(&previous["decided_at"]).is_none_or(|age| age >= 60.0);
        if changed || due {
            entry["score"] = decision["score"].clone();
            entry["admission_decision"] = decision.clone();
            entry["updated_at"] = json!(now());
            let events=if changed{vec![leases::planned_event(competition_id,"competition",competition_id,"competition.scheduler.admission_decision","",decision.clone())]}else{Vec::new()};
            if !put_records_with_events(state,competition_id,&[(QUEUE,cid.as_str(),Some(&original))],&[(QUEUE,cid.as_str(),&entry)],&events)?{
                return Err(ApiError::new(StatusCode::CONFLICT,"competition.queue.projection_pending","Queue changed while evaluating admission; retry current visit"));
            }
        }
        if decision["action"] == "admit" {
            ranked.push((entry, decision, challenge.clone()));
        }
    }
    let order_list: Vec<String> = policy["challenge_order"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let tsec = policy["policy_profile"] == "tsec_eval";
    ranked.sort_by(|a, b| {
        let pri = number(&b.0["priority"]).total_cmp(&number(&a.0["priority"]));
        if pri != std::cmp::Ordering::Equal {
            return pri;
        }
        if !order_list.is_empty() {
            let rank = |v: &Value| {
                order_list
                    .iter()
                    .position(|id| id == field(v, "external_challenge_id"))
                    .unwrap_or(usize::MAX)
            };
            let cmp = rank(&a.2).cmp(&rank(&b.2));
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
        } else if tsec {
            let band_a = a.1["score_components"]["visit_band"].as_u64().unwrap_or(2);
            let band_b = b.1["score_components"]["visit_band"].as_u64().unwrap_or(2);
            let cmp = band_a.cmp(&band_b);
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
            let cmp = difficulty(&a.2).cmp(&difficulty(&b.2));
            if cmp != std::cmp::Ordering::Equal {
                return cmp;
            }
            let near_a = number(&a.1["score_components"]["near_solve"]);
            let near_b = number(&b.1["score_components"]["near_solve"]);
            let cmp = near_b.total_cmp(&near_a);
            if cmp != std::cmp::Ordering::Equal {return cmp;}
            let cmp=number(&b.1["score_components"]["points"]).total_cmp(&number(&a.1["score_components"]["points"]));if cmp!=std::cmp::Ordering::Equal{return cmp;}
        }
        let cmp = number(&b.1["score"]).total_cmp(&number(&a.1["score"]));
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        let cmp = field(&a.0, "enqueued_at").cmp(field(&b.0, "enqueued_at"));
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        field(&a.0, "competition_challenge_id").cmp(field(&b.0, "competition_challenge_id"))
    });
    Ok(ranked.into_iter().map(|(entry, _, _)| entry).collect())
}

pub(super) fn admitted_entry(state:&AppState,competition_id:&str,entry:&Value,run_id:&str,binding_id:&str,policy:&Value,challenge:&Value)->ApiResult<Value>{
    let generation=state.store.get("runs",run_id).map_err(ApiError::internal)?.and_then(|run|run["execution_generation"].as_u64()).unwrap_or(0);
    if generation==0{return Err(ApiError::new(StatusCode::CONFLICT,"competition.visit.execution_missing","A durable execution generation is required before admission"));}
    let mut entry=entry.clone();
    let cid=field(&entry,"competition_challenge_id").to_owned();
    let count=visit::field(&entry,"visit_count").as_u64().unwrap_or(0);
    let persistent=string_set(&policy["persistent_challenge_ids"]).contains(field(challenge,"external_challenge_id")) || policy["policy_profile"]=="tsec_eval";
    let timebox=policy["round_timeboxes_s"].as_array().and_then(|boxes|boxes.get(count as usize).or_else(||boxes.last())).and_then(Value::as_u64)
        .unwrap_or_else(||policy["per_challenge_seconds"].as_f64().filter(|n|*n>0.).map(|n|n as u64).unwrap_or(600));
    let candidates=scoped(state,CANDIDATES,competition_id)?.iter().filter(|row|row["competition_challenge_id"]==cid).count() as u64;
    visit::admit(&mut entry,visit::Execution{run_id:run_id.into(),generation},binding_id,candidates,persistent,timebox)?;
    entry["admission_decision"]["binding_id"]=json!(binding_id);
    entry["admission_decision"]["run_id"]=json!(run_id);
    entry["updated_at"] = json!(now());
    Ok(entry)
}
#[cfg(test)]
pub(super) fn mark_admitted(state:&AppState,competition_id:&str,entry:&mut Value,run_id:&str,binding_id:&str,policy:&Value,challenge:&Value)->ApiResult<()>{
    let cid=field(entry,"competition_challenge_id").to_owned();
    for _ in 0..32{
        let before=state.store.get(QUEUE,&cid).map_err(ApiError::internal)?.ok_or_else(||ApiError::missing("scheduler queue entry"))?;
        let next=admitted_entry(state,competition_id,&before,run_id,binding_id,policy,challenge)?;if next==before{*entry=next;return Ok(());}
        let event=leases::planned_event(competition_id,"competition",competition_id,"competition.scheduler.admission_decision","",next["admission_decision"].clone());
        if put_records_with_events(state,competition_id,&[(QUEUE,&cid,Some(&before))],&[(QUEUE,&cid,&next)],&[event])?{*entry=next;return Ok(());}
    }Err(ApiError::new(StatusCode::CONFLICT,"competition.scheduler.admission_pending","Run started; scheduler admission projection remains pending"))
}

pub(super) fn category(raw:&str)->&'static str{
    match raw.trim().to_ascii_lowercase().as_str(){"web"|"web3"=>"web","pwn"|"pwnable"|"binary"|"exploit"|"exploitation"=>"pwn","reverse"|"rev"|"re"|"reversing"=>"reverse","crypto"|"cryptography"=>"crypto","forensics"|"stego"|"steganography"|"forensic"|"network"=>"forensics",_=>"misc"}
}
pub(super) fn dispatch_parameters(policy:&Value,challenge:&Value,entry:&Value,connection:&Value)->Value{
    let persistent=policy["policy_profile"]=="tsec_eval" || string_set(&policy["persistent_challenge_ids"]).contains(field(challenge,"external_challenge_id"));
    let visits=visit::field(&entry,"visit_count").as_u64().unwrap_or(0) as usize;
    let timebox=if persistent{0.}else{policy["round_timeboxes_s"].as_array().and_then(|boxes|boxes.get(visits).or_else(||boxes.last())).and_then(Value::as_f64).or_else(||policy["per_challenge_seconds"].as_f64().filter(|n|*n>0.)).unwrap_or(600.)};
    let mut body=json!({"kind":"swarm","visit_timebox_s":timebox,"wall_clock_budget":timebox,"keepalive_max":if persistent{0}else{policy["keepalive_max"].as_u64().unwrap_or(0)}});
    if connection["platform_kind"]=="tsecbench"{body["allow_operator_input"]=json!(false);}body
}
pub(super) fn capacity_hold(state:&AppState,competition_id:&str,entry:&Value,reason:&str)->ApiResult<()>{
    let cid=field(entry,"competition_challenge_id");let mut next=entry.clone();next["admission_decision"]["action"]=json!("hold");next["admission_decision"]["reason"]=json!(reason);next["admission_decision"]["decided_at"]=json!(now());next["updated_at"]=json!(now());
    let event=leases::planned_event(competition_id,"competition",competition_id,"competition.scheduler.admission_decision","",next["admission_decision"].clone());
    if !put_records_with_events(state,competition_id,&[(QUEUE,cid,Some(entry))],&[(QUEUE,cid,&next)],&[event])?{return Err(ApiError::new(StatusCode::CONFLICT,"competition.queue.projection_pending","Queue changed while recording capacity hold"));}Ok(())
}
