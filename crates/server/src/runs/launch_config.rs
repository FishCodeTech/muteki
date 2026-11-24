//! Resolve once per execution; isolation is immutable across Run generations.
use super::*;

pub(super) fn settings(state:&AppState,run_id:&str)->Result<Value,String>{
    let run=state.store.get("runs",run_id).map_err(|e|e.to_string())?;
    if let Some(config)=run.as_ref().and_then(|run|run.get("effective_configuration")).filter(|v|v.is_object()){return Ok(config.clone());}
    crate::settings_metadata::configuration(state).map_err(|e|format!("Worker configuration unavailable: {e:?}"))
}
fn previous_isolation(state:&AppState,run_id:&str,run:&Value)->Result<Option<Value>,String>{
    if let Some(saved)=run.get("worker_runtime").filter(|v|v.is_object()){return Ok(Some(saved.clone()));}
    let mut backends=HashSet::new();let mut scopes=HashSet::new();
    for marker in state.store.list("run_execution_commands").map_err(|e|e.to_string())?.into_iter().filter(|row|row["run_id"]==run_id) {
        for spec in marker["specs"].as_array().into_iter().flatten(){if let Some(backend)=spec["backend"].as_str(){backends.insert(backend.to_owned());}}
    }
    for event in persisted_event_iter(state,run_id)? {
        let payload=&event["payload"];
        if let Some(backend)=payload["backend"].as_str().filter(|v|matches!(*v,"local"|"container")){backends.insert(backend.to_owned());}
        if let Some(scope)=payload["container_scope"].as_str(){scopes.insert(scope.to_owned());}
    }
    if backends.len()>1 || scopes.len()>1{return Err("Historical Run has conflicting Worker isolation records".into());}
    if let Some(backend)=backends.into_iter().next(){let scope=if backend=="local"{"run".to_owned()}else{scopes.into_iter().next().ok_or("Historical container Run has no recorded container scope; restore its original launch policy before continuing")?};return Ok(Some(json!({"mode":run["mode"],"worker_backend":backend,"worker_container_scope":scope})));}
    if run["execution_generation"].as_u64().unwrap_or(0)>0 || run["historical"]==true{return Err("Historical Run isolation evidence is missing; restore its original launch policy before continuing".into());}
    Ok(None)
}

pub(super) fn resolve(state:&AppState,run_id:&str,run:&Value,request:&Value,challenge:&Value,continuing:bool)->Result<Value,String>{
    if !request.is_object(){return Err("Run launch request must be an object".into());}
    let mut config=if continuing{settings(state,run_id)?}else{crate::settings_metadata::configuration(state).map_err(|e|format!("Worker configuration unavailable: {e:?}"))?};
    let category=challenge["category"].as_str().unwrap_or("");
    if let Some(engines)=config["overrides"][category].get("engines"){config["engines"]=engines.clone();}
    for(key,value)in request.as_object().unwrap(){config[key]=value.clone();}
    crate::settings_metadata::sanitize_worker_config(&mut config);
    config["challenge"]=challenge.clone();
    let mode=challenge["mode"].as_str().unwrap_or("ctf");
    let prior=previous_isolation(state,run_id,run)?;
    let has_executed=run["execution_generation"].as_u64().unwrap_or(0)>0 || run["historical"]==true || continuing;
    let kind = if has_executed {
        let old_kind = run
            .get("kind")
            .and_then(Value::as_str)
            .or_else(|| prior.as_ref().and_then(|p| p.get("kind").and_then(Value::as_str)))
            .filter(|v| !v.is_empty())
            .unwrap_or("swarm");
        if let Some(req_kind) = request.get("kind").and_then(Value::as_str).filter(|v| !v.is_empty()) {
            if req_kind != old_kind {
                return Err(format!("Run kind is fixed as `{old_kind}` by its original execution; cannot switch to `{req_kind}`"));
            }
        }
        old_kind.to_owned()
    } else {
        let req_kind = request
            .get("kind")
            .and_then(Value::as_str)
            .or_else(|| run.get("kind").and_then(Value::as_str))
            .filter(|v| !v.is_empty())
            .unwrap_or("swarm");
        if !matches!(req_kind, "swarm" | "local_pi") {
            return Err("kind must be swarm or local_pi".into());
        }
        req_kind.to_owned()
    };
    if kind == "local_pi" && mode != "ctf" {
        return Err("local_pi execution option is only available in CTF mode".into());
    }
    config["kind"] = json!(kind);
    let template_id = request.get("dispatch_template_id").and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty());
    if let Some(template_id) = template_id && !continuing {
        if kind != "swarm" {
            return Err("dispatch_template_id only applies to swarm runs".into());
        }
        crate::settings_metadata::apply_dispatch_template(state, &mut config, template_id)?;
    }
    if let Some(object) = config.as_object_mut() {
        object.remove("dispatch_templates");
        object.remove("dispatch_template_id");
    }
    if kind == "local_pi" {
        if request.get("worker_backend").and_then(Value::as_str).is_none() {
            config["worker_backend"] = json!("local");
        }
        config["coordinator"] = json!(false);
        config["max_workers"] = json!(1);
        let saved_defaults = super::local_pi::load_local_pi_settings(state)
            .map_err(|failure| super::local_pi::failure_text(&failure))?;
        let mut lp_cfg = serde_json::Map::new();
        if let Some(m) = saved_defaults.get("default_model").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            lp_cfg.insert("model".to_owned(), json!(m));
        }
        if let Some(a) = saved_defaults.get("default_account_id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            lp_cfg.insert("account_id".to_owned(), json!(a));
        }
        if let Some(ep) = saved_defaults.get("default_endpoint").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            lp_cfg.insert("endpoint".to_owned(), json!(ep));
        }
        if let Some(mode) = saved_defaults.get("default_access_mode").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            lp_cfg.insert("access_mode".to_owned(), json!(mode));
        }
        if let Some(sk) = saved_defaults.get("default_skills").filter(|v| v.is_array()) {
            lp_cfg.insert("skills".to_owned(), sk.clone());
        }
        if let Some(off) = saved_defaults.get("offline_env").and_then(Value::as_bool) {
            lp_cfg.insert("offline_env".to_owned(), json!(off));
        }
        if let Some(existing) = run.get("local_pi_config").and_then(Value::as_object) {
            for (k, v) in existing {
                lp_cfg.insert(k.clone(), v.clone());
            }
        }
        if let Some(req_lp) = request.get("local_pi_config").and_then(Value::as_object) {
            for (k, v) in req_lp {
                lp_cfg.insert(k.clone(), v.clone());
            }
        }
        for key in ["model", "account_id", "endpoint", "access_mode", "prompt", "skills"] {
            if let Some(v) = request.get(key).filter(|v| !v.is_null()) {
                lp_cfg.insert(key.to_owned(), v.clone());
            }
        }
        if !lp_cfg.contains_key("skills") {
            lp_cfg.insert("skills".to_owned(), json!(super::local_pi::default_skill_ids()));
        }
        config["local_pi_config"] = Value::Object(lp_cfg);
    }
    if let Some(prior)=prior.as_ref(){
        let old_node=run["effective_configuration"]["worker_node_id"].as_str().unwrap_or("local");
        if request["worker_node_id"].as_str().is_some_and(|node|node!=old_node){return Err("Run execution node is fixed; create a new Run to change placement".into());}
        config["worker_node_id"]=json!(old_node);
        let old_mode=prior["mode"].as_str().or_else(||run["mode"].as_str()).unwrap_or("ctf");
        if old_mode!=mode{return Err("Run mode is fixed by its original execution".into());}
        for key in ["worker_backend","worker_container_scope"]{
            if let Some(value)=request.get(key).filter(|v|!v.is_null() && v.as_str()!=Some("")) {
                if value!=&prior[key]{return Err(format!("Run {key} is fixed by its original execution"));}
            }
            config[key]=prior[key].clone();
        }
        if request.get("offline").is_none()
            && let Some(saved_offline) = run["effective_configuration"]["offline"].as_bool()
        {
            config["offline"] = json!(saved_offline);
        }
    }
    let backend=config["worker_backend"].as_str().filter(|v|!v.is_empty()).map(str::to_owned)
        .or_else(||std::env::var("MUTEKI_WORKER_BACKEND").ok()).unwrap_or_else(||"container".into());
    let backend=if backend=="container_dockerexec"{"container"}else{&backend};
    if !matches!(backend,"local"|"container"){return Err("worker_backend must be local or container".into());}
    if std::env::var("MUTEKI_CONTAINER_MODE").as_deref()==Ok("single") && backend!="local"{return Err("Single-container deployment requires worker_backend=local".into());}
    config["worker_backend"]=json!(backend);
    if backend!="container" && crate::runner_remote::controller::selected(&config).is_some(){return Err("Remote nodes require the container Worker backend".into());}
    if !matches!(config["offline"],Value::Null|Value::Bool(_)){return Err("offline must be a boolean".into());}
    if mode=="pentest"{config["offline"]=json!(false);}
    let scope=if backend=="local"{"run"}else{config["worker_container_scope"].as_str().unwrap_or("run")}.to_owned();
    if !matches!(scope.as_str(),"run"|"shared"){return Err("worker_container_scope must be run or shared".into());}
    config["worker_container_scope"]=json!(scope);
    if !matches!(config["worker_privilege"].as_str().unwrap_or("root"),"root"|"default"|"elevated"){return Err("worker_privilege must be root".into());}
    // Legacy privilege selections no longer lower container Agent permissions.
    config["worker_privilege"]=json!("root");
    if !config["stage_policy"].is_object(){config["stage_policy"]=json!({});}
    if let Some(sp)=config["stage_policy"].as_object_mut(){sp.remove("race");}
    for key in ["coordinator","budgets"]{if !config["stage_policy"][key].is_object(){config["stage_policy"][key]=json!({});}}
    if let Some(coord)=config["stage_policy"]["coordinator"].as_object_mut(){coord.remove("dispatch_mode");}
    for(key,section)in [("wall_clock_budget","coordinator"),("max_total_workers","budgets"),("cost_budget_usd","budgets")] {
        let explicit=request.get(key).or_else(||if key=="wall_clock_budget"{request.get("visit_timebox_s")}else{None});
        let value=explicit.or_else(||config["stage_policy"][section].get(key).filter(|v|v.as_f64().is_some_and(|n|n>0.))).unwrap_or(&config[key]).clone();
        if !value.is_null() && !value.as_f64().is_some_and(|n|n.is_finite() && n>=0.){return Err(format!("{key} must be a finite nonnegative number"));}
        if key=="max_total_workers" && !value.is_null() && value.as_u64().is_none(){return Err("max_total_workers must be a nonnegative integer".into());}
        config[key]=if value.is_null(){json!(0)}else{value};
        let resolved=config[key].clone();config["stage_policy"][section][key]=resolved;
    }
    config["cold_start"]=json!(!continuing && request["cold_start"]!=false);
    if mode=="ctf"{config["max_workers"]=json!(config["max_workers"].as_u64().unwrap_or(3).min(3).max(1));}
    if !config["max_workers"].as_u64().is_some_and(|n|n>0){return Err("max_workers must be a positive integer".into());}
    container_exec::resolve_worker_resource_limits(Some(&config))?;
    if request.get("execution_budget").is_none(){config.as_object_mut().unwrap().remove("execution_budget");}
    let mut budget_run=run.clone();
    if let Some(parent)=request["_competition_visit_command"].as_str(){
        let command=state.store.get("platform_commands",parent).map_err(|e|e.to_string())?.ok_or("Competition visit admission is missing")?;
        if command["request"]["command_type"]!="run_binding.resolve" || command["plan"]["run_id"]!=run_id || command["plan"]["child_run_command_id"]!=request["command_id"]{return Err("Competition visit budget differs from its admitted binding command".into());}
        budget_run["execution_budget"]=Value::Null;config.as_object_mut().unwrap().remove("execution_budget");
    }
    config["execution_budget"]=super::execution_budget::prepare(&budget_run,&config)?;
    Ok(config)
}
pub(super) fn persist(run:&mut Value,config:&Value,challenge:&Value){
    run["effective_configuration"]=config.clone();
    run["execution_budget"]=config["execution_budget"].clone();
    let kind = config.get("kind").and_then(Value::as_str).unwrap_or("swarm");
    run["kind"]=json!(kind);
    if kind == "local_pi" {
        run["coordinator"]=json!(false);
        if let Some(lp_cfg) = config.get("local_pi_config") {
            run["local_pi_config"]=lp_cfg.clone();
        }
    }
    run["worker_runtime"]=json!({"version":1,"mode":challenge["mode"],"kind":kind,"worker_backend":config["worker_backend"],"worker_node_id":config["worker_node_id"],"worker_container_scope":config["worker_container_scope"]});
    for key in ["wall_clock_budget","max_total_workers","cost_budget_usd","worker_backend","worker_container_scope","visit_timebox_s","keepalive_max"]{run[key]=config[key].clone();}
}
