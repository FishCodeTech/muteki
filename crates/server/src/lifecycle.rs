//! Activation owns recovery. A maintenance candidate has no background executors.
use crate::AppState;
use serde_json::{Value,json};
use std::{collections::HashSet,sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}}};

pub struct Service {
    pub(crate) activation:tokio::sync::Mutex<()>,
    ready:AtomicBool,
    loops_started:AtomicBool,
    completed:Mutex<HashSet<&'static str>>,
    status:Mutex<Value>,
}
impl Default for Service {
    fn default()->Self{Self{activation:tokio::sync::Mutex::new(()),ready:AtomicBool::new(false),loops_started:AtomicBool::new(false),completed:Mutex::new(HashSet::new()),status:Mutex::new(json!({"state":"waiting_activation","ready":false,"steps":[]}))}}
}
impl Service {
    pub fn ready(&self)->bool{self.ready.load(Ordering::SeqCst)}
    pub fn snapshot(&self)->Value{self.status.lock().unwrap().clone()}
    pub fn fail(&self,message:String){
        self.ready.store(false,Ordering::SeqCst);
        let mut status=self.status.lock().unwrap();status["ready"]=json!(false);status["state"]=json!("failed");status["error"]=json!(message);
    }
    fn step(&self,name:&'static str,result:Result<(),String>)->Result<(),String>{
        let mut status=self.status.lock().unwrap();
        let steps=status["steps"].as_array_mut().unwrap();
        steps.retain(|step|step["name"]!=name);
        steps.push(json!({"name":name,"core":true,"state":if result.is_ok(){"completed"}else{"failed"},"detail":result.as_ref().err(),"completed_at":chrono::Utc::now().to_rfc3339()}));
        if result.is_ok(){self.completed.lock().unwrap().insert(name);}
        result
    }
    fn done(&self,name:&str)->bool{self.completed.lock().unwrap().contains(name)}
}

pub async fn activate(state:Arc<AppState>)->Result<(),String>{
    let _guard=state.lifecycle.activation.lock().await;
    if state.lifecycle.ready(){state.admission.set_draining(false);return Ok(());}
    state.admission.set_draining(true);
    {
        let mut status=state.lifecycle.status.lock().unwrap();
        status["state"]=json!("initializing");status["ready"]=json!(false);
        status["epoch"]=json!(state.chat.epoch());status["started_at"]=json!(chrono::Utc::now().to_rfc3339());
        status.as_object_mut().unwrap().remove("error");
    }
    let result=initialize(&state).await;
    if let Err(error)=result {
        state.lifecycle.fail(error.clone());
        let _=state.store.put("system","startup_recovery",&state.lifecycle.snapshot());
        return Err(error);
    }
    {
        let mut status=state.lifecycle.status.lock().unwrap();
        status["ready"]=json!(true);status["state"]=json!("ready");status["completed_at"]=json!(chrono::Utc::now().to_rfc3339());
    }
    if let Err(error)=state.store.put("system","startup_recovery",&state.lifecycle.snapshot()) {
        state.lifecycle.fail(error.to_string());return Err(error.to_string());
    }
    if !state.lifecycle.loops_started.swap(true,Ordering::SeqCst) {
        state.computer.spawn_reaper().await;
        crate::runtime_api::spawn_refresh_loop(state.clone());
        crate::model_settings_api::spawn_catalog_refresh_loop(state.clone());
        crate::chat_metadata::spawn(state.clone());
        crate::quota_resume::spawn_loop(state.clone());
        crate::operations_api::spawn_effect_loop(state.clone());
        crate::extensions::start_background(state.clone());
        crate::runs::retention::spawn_loop(state.clone());
        crate::runs::start_control_recovery_loop(state.clone());
        crate::runner_remote::controller::spawn_recovery(state.clone());
        crate::runs::progress::spawn(state.clone());
        tokio::spawn(crate::control_tools::watch_loop(state.clone()));
    }
    crate::business::activate(state.clone());
    state.lifecycle.ready.store(true,Ordering::SeqCst);
    state.admission.set_draining(false);
    Ok(())
}
async fn initialize(state:&Arc<AppState>)->Result<(),String>{
    macro_rules! step {($name:literal,$work:expr)=>{if !state.lifecycle.done($name){
        state.lifecycle.step($name,$work)?;
        state.store.put("system","startup_recovery",&state.lifecycle.snapshot()).map_err(|error|error.to_string())?;
    }}}
    let root=state.store.get("system","state_dir").map_err(|error|error.to_string())?
        .and_then(|v|v.as_str().map(std::path::PathBuf::from)).ok_or("State directory is missing")?;
    step!("native_processes",crate::runtime_guardian::reap(&root).map(|_|()).and_then(|_|crate::runtime_guardian::configure(&root)).map_err(|error|error.to_string()));
    step!("credential_materials",crate::account_material::recover(&state.store,&root).map(|_|()).map_err(|error|error.to_string()));
    step!("runner_resources",crate::runner_resources::reconcile_restart(state));
    step!("worktree_removals",crate::workspaces::recover_removals(state).await);
    step!("extensions",crate::extensions::initialize(state).await);
    Ok(())
}
