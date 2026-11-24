//! Typed execution operations owned by the Runner process.
mod recovery;
mod limits;
mod retention;
use crate::{AppState, runs::container_exec::{self, ContainerHandle, WorkerSpec}};
use axum::{Json, Router, extract::{State, Path, Query}, http::StatusCode, routing::{get,post}};
use serde::Deserialize;
use serde_json::{Value,json};
use std::{collections::HashMap,sync::Arc,path::PathBuf};
use tokio::{sync::Mutex,io::{AsyncWriteExt,AsyncBufReadExt,AsyncSeekExt}};

/// Physical exit, known business result and published artifacts are independent.
#[derive(Clone,Copy)]
pub(crate) struct ExecutionFacts{pub stopped:bool,pub outcome_known:bool,pub published:bool}
pub(crate) fn facts(row:&Value)->ExecutionFacts{
    ExecutionFacts{stopped:row["execution_stopped"]==true||row["exit"].is_object(),outcome_known:row["exit"].is_object()&&row["state"]!="outcome_unknown",published:row["outputs_published"]==true}
}

pub struct Execution {
    state: Arc<AppState>,
    resources: crate::runner_resources::Monitor,
    root: PathBuf,
    log_bytes: std::sync::atomic::AtomicU64,
    handles: Mutex<HashMap<String,ContainerHandle>>,
    gates: Mutex<HashMap<String,std::sync::Weak<Mutex<()>>>>,
    start_cleanup: std::sync::Mutex<HashMap<String, FailedStart>>,
}
impl Execution {
    pub fn open(root: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let root=root.canonicalize()?;
        let store=crate::store::Store::open(root.join("muteki.sqlite"))?;
        store.put("system","state_dir",&json!(root))?;
        Ok(Arc::new(Self {state:Arc::new(AppState::new(store)),resources:Default::default(),log_bytes:std::sync::atomic::AtomicU64::new(retention::measure(&root)),root,handles:Mutex::new(HashMap::new()),gates:Mutex::new(HashMap::new()),start_cleanup:std::sync::Mutex::new(HashMap::new())}))
    }
    pub async fn reconcile(&self)->Result<(),String>{
        self.settle_failed_starts().await;
        if let Err(error)=crate::runner_resources::retry_settlements(&self.state.store){tracing::warn!(%error,"Reservation settlements remain pending");}
        recovery::reconcile(self).await
    }
    pub fn spawn_reconciliation(ex:&Arc<Self>){
        let weak=Arc::downgrade(ex);
        tokio::spawn(async move {let mut tick=0u64;loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let Some(ex)=weak.upgrade()else{break};
            if let Err(error)=ex.reconcile().await{tracing::error!(%error,"Runner recovery failed");}
            if tick%retention::gc_ticks()==0{if let Err(error)=retention::purge(&ex,false){tracing::warn!(%error,"Runner event-log retention failed");}}
            tick+=1;
        }});
    }
    async fn gate(&self,id:&str)->Arc<Mutex<()>>{
        let mut gates=self.gates.lock().await;gates.retain(|_,v|v.strong_count()>0);
        if let Some(gate)=gates.get(id).and_then(std::sync::Weak::upgrade){return gate;}
        let gate=Arc::new(Mutex::new(()));gates.insert(id.into(),Arc::downgrade(&gate));gate
    }
    fn generation(&self,id:&str,generation:u64)->Result<Value,String>{
        let row=self.state.store.get("runner_runs",id).map_err(|e|e.to_string())?.ok_or("Run has no execution contract")?;
        if generation==0||row["generation"]!=generation{return Err("Stale Run execution generation".into());}
        Ok(row)
    }
    fn active(&self,id:&str,generation:u64)->Result<(),String>{
        if self.start_cleanup.lock().unwrap().values().any(|pending|pending.run==id&&pending.generation==generation){return Err("Run awaits failed Start compensation; new execution admission is fenced".into());}
        let row=self.generation(id,generation)?;
        if row["state"]!="active"{return Err("Run is not accepting execution commands".into());}Ok(())
    }
    async fn handle(&self,id:&str)->Result<ContainerHandle,String>{
        self.handles.lock().await.get(id).cloned().ok_or_else(||"Run container is not attached to this Runner".into())
    }
    fn events_path(&self,worker:&str)->Result<PathBuf,String>{
        valid_id(worker)?;
        Ok(self.root.join("runner/events").join(format!("{worker}.jsonl")))
    }
}
/// Owns the external Start boundary even if the HTTP caller is cancelled. A
/// failed return never means that the supervisor did not start a process.
#[derive(Clone)]
struct FailedStart { run:String, generation:u64, handle:ContainerHandle, record:Value }
struct StartOwner { execution:Arc<Execution>, command:String, failure:Option<FailedStart> }
impl Drop for StartOwner {
    fn drop(&mut self) {
        let Some(failure)=self.failure.take() else{return};
        self.execution.start_cleanup.lock().unwrap().insert(self.command.clone(),failure);
        let execution=self.execution.clone();
        tokio::spawn(async move { execution.settle_failed_starts().await; });
    }
}
impl Execution {
    async fn settle_failed_starts(&self) {
        let pending:Vec<_>=self.start_cleanup.lock().unwrap().iter().map(|(id,row)|(id.clone(),row.clone())).collect();
        for (command,mut failed) in pending {
            let gate=self.gate(&failed.run).await;let _guard=gate.lock().await;
            let attached=self.handles.lock().await.get(&failed.run).is_some_and(|handle|
                handle.container_id==failed.handle.container_id && handle.owner_token==failed.handle.owner_token
                    && Arc::ptr_eq(&handle.link,&failed.handle.link));
            if !attached && !self.state.store.get("runner_runs",&failed.run).is_ok_and(|row|row.is_some_and(|row|
                row["generation"].as_u64().is_some_and(|generation|generation>failed.generation)
                    || row["generation"]==failed.generation && row["state"]=="stopped")) {
                tracing::error!(command_id=%command,"Failed Start has no confirmed stop evidence; reconciliation remains pending");
                continue;
            }
            if attached {
                if let Err(error)=container_exec::teardown_run_container_checked(&failed.run).await {
                    tracing::error!(%error,command_id=%command,"Failed Start still awaits confirmed execution stop");
                    continue;
                }
                self.handles.lock().await.remove(&failed.run);
            }
            failed.record["state"]=json!("outcome_unknown");
            failed.record["execution_stopped"]=json!(true);
            failed.record["stream_error"]=json!("Start did not complete its durable output/receipt handoff; execution was stopped");
            let saved=self.state.store.document_scope(||->Result<(),String>{
                if let Some(mut contract)=self.state.store.get("runner_runs",&failed.run).map_err(|e|e.to_string())? {
                    if contract["generation"]==failed.generation {contract["state"]=json!("stopped");self.state.store.put("runner_runs",&failed.run,&contract).map_err(|e|e.to_string())?;}
                }
                let mut record=self.state.store.get("runner_commands",&command).map_err(|e|e.to_string())?.unwrap_or_else(||failed.record.clone());
                for key in ["result","state","execution_stopped","stream_error"] {if !failed.record[key].is_null(){record[key]=failed.record[key].clone();}}
                self.state.store.put("runner_commands",&command,&record).map_err(|e|e.to_string())?;
                crate::runner_resources::release_completed(&self.state.store,&command)
            });
            match saved {
                Ok(Ok(()))=>{self.start_cleanup.lock().unwrap().remove(&command);},
                error=>tracing::error!(?error,command_id=%command,"Confirmed failed Start awaits durable settlement"),
            }
        }
    }
}

fn valid_id(id:&str)->Result<(),String>{
    if id.is_empty()||id.len()>160||!id.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'-'|b'_')) {
        return Err("Invalid execution identifier".into());
    } Ok(())
}
type Reply=Result<Json<Value>,(StatusCode,Json<Value>)>;
fn error(e:impl ToString)->(StatusCode,Json<Value>){(StatusCode::CONFLICT,Json(json!({"error":e.to_string()})))}

fn first_generation()->u64{1}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ensure {run_id:String,settings:Value,workspace_relative:Option<PathBuf>,owner_run_id:Option<String>,#[serde(default="first_generation")] generation:u64}
async fn ensure(State(ex):State<Arc<Execution>>,Json(mut input):Json<Ensure>)->Reply{
    valid_id(&input.run_id).map_err(error)?;
    if !input.settings.is_object(){return Err(error("Runtime settings must be an object"));}
    let gate=ex.gate(&input.run_id).await;let _guard=gate.lock().await;
    if input.generation==0{return Err(error("Generation must be positive"));}
    if let Some(prior)=ex.state.store.get("runner_runs",&input.run_id).map_err(error)?{
        let previous=prior["generation"].as_u64().ok_or_else(||error("Invalid saved generation"))?;
        if input.generation<previous || (input.generation==previous && prior["state"]!="active") ||
            (input.generation>previous && prior["state"]!="stopped") {
            return Err(error("Run generation transition requires confirmed stop or recovery"));
        }
        if input.generation==previous && !ex.handles.lock().await.get(&input.run_id).is_some_and(|handle|handle.link.is_alive()){
            return Err(error("Existing Run execution requires reconciliation after Runner restart"));
        }
    }
    if input.run_id.starts_with(crate::runs::container_exec::SHARED_CONTAINER_RUN_ID){return Err(error("Reserved shared runtime identity"));}
    let existing_handle={ex.handles.lock().await.get(&input.run_id).cloned()};
    let image=if let Some(handle)=&existing_handle{handle.image.clone()}else{
        crate::runs::container_exec::inspect_worker_image_status_local().await["image_id"].as_str().filter(|id|id.starts_with("sha256:")).ok_or_else(||error("Worker image identity is unavailable"))?.to_owned()
    };
    input.settings["_runner_image_id"]=json!(image);
    limits::resolve(&mut input.settings,existing_handle.as_ref()).await.map_err(error)?;
    if existing_handle.is_some() {
        if let Some(prior)=ex.state.store.get("runner_runs",&input.run_id).map_err(error)? {
            if prior["resource_limit_sources"].is_object() {
                input.settings["_runner_resource_limit_sources"]=prior["resource_limit_sources"].clone();
            }
        }
    }
    if !input.settings["_runner_trust_domain"].is_string(){
        let run=ex.state.store.get("runs",&input.run_id).map_err(error)?;
        let project=run.as_ref().and_then(|r|r["project_id"].as_str()).unwrap_or("personal");
        let credentials=crate::runner::canonical_digest(input.settings["credentials"].clone()).map_err(error)?;
        input.settings["_runner_trust_domain"]=json!(format!("local:{project}:{credentials}"));
    }
    let expected=crate::runs::session_dir(&ex.state,&input.run_id);
    let workspace=if let Some(relative)=input.workspace_relative {
        if relative.is_absolute()||relative.components().any(|p|matches!(p,std::path::Component::ParentDir)){return Err(error("Invalid workspace path"));}
        let candidate=ex.root.join(relative);
        let probe=ex.root.join("runtime/probes").join(&input.run_id);
        let auxiliary=ex.root.join("runtime/auxiliary").join(&input.run_id);
        let owned=input.owner_run_id.as_deref().filter(|owner|*owner!=input.run_id).map(|owner|crate::runs::session_dir(&ex.state,owner));
        let owned_real=candidate.canonicalize().ok().is_some_and(|real|[Some(&expected),owned.as_ref()].into_iter().flatten()
            .filter_map(|root|root.canonicalize().ok()).any(|root|real.starts_with(root)));
        if !candidate.starts_with(&expected)&&!candidate.starts_with(&probe)&&!(input.run_id.starts_with("btw-")&&candidate.starts_with(&auxiliary))&&!owned.as_ref().is_some_and(|owned|candidate.starts_with(owned))&&!owned_real{return Err(error(format!("Workspace {} does not belong to this Run ({})",candidate.display(),expected.display())));}
        candidate
    } else {expected.clone()};
    if input.settings["worker_container_scope"]!="shared"&&workspace==expected {
        crate::runs::shared_pools::prepare_independent(&ex.state,&input.run_id).map_err(error)?;
    }
    std::fs::create_dir_all(&workspace).map_err(error)?;
    if input.settings["worker_container_scope"]=="shared" && input.owner_run_id.as_deref()==Some(input.run_id.as_str()) {
        crate::runs::shared_pools::prepare(&ex.state,&input.run_id,&input.settings).map_err(error)?;
    }
    let canonical=workspace.canonicalize().map_err(error)?;
    if !canonical.starts_with(&ex.root){return Err(error("Workspace symlink leaves node state root"));}
    if !ex.handles.lock().await.contains_key(&input.run_id) {
    ex.state.store.put("runner_runs",&input.run_id,&json!({"run_id":input.run_id,"generation":input.generation,"state":"preparing"})).map_err(error)?;
    }
    let handle=container_exec::ensure_run_container_owned(&ex.state,&input.run_id,&workspace,&input.settings,input.owner_run_id.as_deref()).await.map_err(error)?;
    let identity=recovery::inspect(&handle.container_id,&ex.root).await.map_err(error)?;
    let container_id=identity["container_id"].as_str().filter(|_|identity["present"]==true)
        .ok_or_else(||error("Created container identity is unconfirmed"))?;
    let mut value=serde_json::to_value(&handle).map_err(error)?;
    value["generation"]=json!(input.generation);
    let browser_reserved=input.settings["_runner_browser_enabled"]==true || ex.state.store.get("runner_runs",&input.run_id).map_err(error)?
        .is_some_and(|prior|prior["generation"]==input.generation && prior["browser_reserved"]==true && prior["state"]!="stopped");
    ex.state.store.put("runner_runs",&input.run_id,&json!({"run_id":input.run_id,"generation":input.generation,"state":"active","container":handle.name,"container_id":container_id,"scope":handle.scope,"container_workspace":handle.container_workspace,"worker_uid":handle.worker_uid,"worker_gid":handle.worker_gid,"workspace_uid":handle.workspace_uid,"workspace_gid":handle.workspace_gid,"owner_run_id":input.owner_run_id,"browser_reserved":browser_reserved,"resource_limits":handle.limits,"resource_limit_sources":input.settings["_runner_resource_limit_sources"],"workspace_relative":canonical.strip_prefix(&ex.root).map_err(error)?})).map_err(error)?;
    ex.handles.lock().await.insert(input.run_id,handle);
    Ok(Json(value))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {command_id:String,run_id:String,spec:WorkerSpec,#[serde(default="first_generation")] generation:u64}
async fn start(State(ex):State<Arc<Execution>>,Json(mut input):Json<Start>)->Reply{
    valid_id(&input.command_id).map_err(error)?;valid_id(&input.run_id).map_err(error)?;
    let gate=ex.gate(&input.run_id).await;let _guard=gate.lock().await;
    ex.active(&input.run_id,input.generation).map_err(error)?;
    let handle=ex.handle(&input.run_id).await.map_err(error)?;
    let cwd=std::path::Path::new(&input.spec.cwd);
    if !cwd.starts_with(&handle.container_workspace)||cwd.components().any(|p|matches!(p,std::path::Component::ParentDir)){
        return Err(error("Worker cwd is outside its Run workspace"));
    }
    if input.spec.argv.is_empty()||input.spec.timeout_sec<=0 {return Err(error("Worker requires argv and a positive timeout"));}
    let digest=crate::runner::canonical_digest(json!({"run_id":input.run_id,"generation":input.generation,"spec":input.spec})).map_err(error)?;
    let mut record=json!({"id":input.command_id,"run_id":input.run_id,"generation":input.generation,
        "solver_id":input.spec.tag,"digest":digest,"state":"dispatching"});
    let replay=ex.state.store.document_scope(||->Result<Option<Value>,String>{
        if let Some(prior)=ex.state.store.get("runner_commands",&input.command_id).map_err(|e|e.to_string())?{
            if prior["digest"]!=digest{return Err("Command ID was already used with different inputs".into());}
            if prior["state"]=="started"{return Ok(Some(prior["result"].clone()));}
            return Err("Prior command outcome requires reconciliation; it will not be replayed".into());
        }
        if let Some(reservation)=input.spec.reservation_id.as_deref(){
            let mut row=ex.state.store.get("runner_dispatches",reservation).map_err(|e|e.to_string())?.ok_or("Resource reservation is missing")?;
            if row["status"]!="running"||row["run_id"]!=input.run_id||row["worker_id"].as_str()!=input.spec.tag.as_deref(){
                return Err("Resource reservation does not authorize this Worker start".into());
            }
            if let Some(prior)=row["runner_command_id"].as_str(){
                if !ex.state.store.get("runner_commands",prior).map_err(|e|e.to_string())?.is_some_and(|r|r["exit"].is_object()){
                    return Err("Previous Worker execution has no confirmed exit".into());
                }
            }
            row["runner_command_id"]=json!(input.command_id);
            ex.state.store.put("runner_dispatches",reservation,&row).map_err(|e|e.to_string())?;
        }else if let Some(solver)=input.spec.tag.as_deref(){
            if ex.state.store.list("runner_dispatches").map_err(|e|e.to_string())?.iter().any(|row|row["run_id"]==input.run_id&&row["worker_id"]==solver){
                return Err("Scheduled Worker requires its exact resource reservation ID".into());
            }
        }
        ex.state.store.put("runner_commands",&input.command_id,&record).map_err(|e|e.to_string())?;Ok(None)
    }).map_err(error)?.map_err(error)?;
    if let Some(value)=replay{return Ok(Json(value));}
    if handle.scope=="shared" {
        input.spec.owner_run_id=Some(input.run_id.clone());input.spec.owner_token=Some(handle.owner_token.clone());input.spec.owner_workspace=Some(handle.container_workspace.clone());
    } else { input.spec.owner_run_id=None;input.spec.owner_token=None;input.spec.owner_workspace=None; }
    let available=fs2::available_space(&ex.root).map_err(error)?;
    if available<256*1024*1024{return Err(error("Runner disk headroom is below 256 MiB; free space before starting work"));}
    tokio::fs::create_dir_all(ex.root.join("runner/events")).await.map_err(error)?;
    let mut start_owner=StartOwner{execution:ex.clone(),command:input.command_id.clone(),failure:Some(FailedStart{
        run:input.run_id.clone(),generation:input.generation,handle:handle.clone(),record:record.clone(),
    })};
    let (worker,mut frames)=handle.link.start_worker(&input.spec).await.map_err(error)?;
    let result=json!({"worker_id":worker,"run_id":input.run_id,"command_id":input.command_id});
    start_owner.failure.as_mut().unwrap().record["result"]=result.clone();
    let path=ex.events_path(&worker).map_err(error)?;
    let mut file=tokio::fs::OpenOptions::new().create_new(true).write(true).open(&path).await.map_err(error)?;
    record["state"]=json!("started");record["result"]=result.clone();
    ex.state.store.put("runner_commands",&input.command_id,&record).map_err(error)?;
    let store=ex.state.store.clone();
    let execution=ex.clone();
    tokio::spawn(async move {
        let mut persisted_bytes=0u64;
        while let Some(frame)=frames.recv().await {
            let terminal=frame.t=="exit";
            let result=async {
                let mut raw=serde_json::to_vec(&frame).map_err(std::io::Error::other)?;raw.push(b'\n');
                if raw.len()>512*1024||persisted_bytes.saturating_add(raw.len() as u64)>128*1024*1024{
                    return Err(std::io::Error::other("Runner event exceeds the 512 KiB frame or 128 MiB per-Worker log budget"));
                }
                if retention::reserve(&execution.log_bytes,raw.len() as u64).is_err(){
                    retention::purge(&execution,true).map_err(std::io::Error::other)?;
                    retention::reserve(&execution.log_bytes,raw.len() as u64)?;
                }
                file.write_all(&raw).await?;file.flush().await?;persisted_bytes+=raw.len() as u64;Ok::<(),std::io::Error>(())
            }.await;
            if let Err(error)=result {
                tracing::error!(%error,"Runner event persistence failed; stopping affected Run");
                let message=error.to_string();
                let mut failure=serde_json::to_vec(&json!({"t":"transport_error","error":message})).unwrap();failure.push(b'\n');
                let _=file.write_all(&failure).await;let _=file.flush().await;
                if let Ok(Some(mut row))=store.get("runner_commands",&input.command_id){row["stream_error"]=json!(message);row["state"]=json!("outcome_unknown");let _=store.put("runner_commands",&input.command_id,&row);}
                match teardown(State(execution.clone()),Path(input.run_id.clone()),Json(Stop{generation:input.generation})).await{
                    Ok(_)=>{if let Ok(Some(mut row))=store.get("runner_commands",&input.command_id){row["execution_stopped"]=json!(true);let _=store.put("runner_commands",&input.command_id,&row);let _=crate::runner_resources::release_completed(&store,&input.command_id);}},
                    Err(stop_error)=>tracing::error!(?stop_error,"Evidence failure stop is unconfirmed"),
                }
                break;
            }
            if terminal||frame.t=="transport_error" {
                let saved=store.document_scope(||->Result<(),String>{
                    let mut current=store.get("runner_commands",&input.command_id).map_err(|e|e.to_string())?.ok_or("Worker command receipt missing")?;
                    if terminal {current["exit"]=json!(frame);current["state"]=json!("started");}
                    else {current["stream_error"]=json!(frame.error);if !current["exit"].is_object(){current["state"]=json!("outcome_unknown");}}
                    store.put("runner_commands",&input.command_id,&current).map_err(|e|e.to_string())?;
                    if terminal||current["execution_stopped"]==true{crate::runner_resources::release_completed(&store,&input.command_id)?;}
                    Ok(())
                });
                if !matches!(saved,Ok(Ok(()))){tracing::error!("Could not persist Worker terminal/transport outcome");}
                if !terminal {
                    match teardown(State(execution.clone()),Path(input.run_id.clone()),Json(Stop{generation:input.generation})).await {
                        Ok(_) => { if let Ok(Some(mut row)) = store.get("runner_commands", &input.command_id) {
                            row["execution_stopped"] = json!(true);
                            if store.put("runner_commands", &input.command_id, &row).is_ok() {
                                let _ = crate::runner_resources::release_completed(&store, &input.command_id);
                            }
                        } },
                        Err(error) => tracing::error!(?error, "Transport failure stop is unconfirmed"),
                    }
                }
                break;
            }
        }
    });
    start_owner.failure=None;
    Ok(Json(result))
}

#[derive(Deserialize)]
struct Cursor {#[serde(default)] after:u64}
async fn events(State(ex):State<Arc<Execution>>,Path(worker):Path<String>,Query(cursor):Query<Cursor>)->Reply{
    let path=ex.events_path(&worker).map_err(error)?;
    let mut file=tokio::fs::File::open(path).await.map_err(error)?;
    file.seek(std::io::SeekFrom::Start(cursor.after)).await.map_err(error)?;
    let mut reader=tokio::io::BufReader::new(file);let mut after=cursor.after;let mut frames=Vec::new();let mut page_bytes=0usize;
    for _ in 0..256 {
        let mut line=String::new();let count=reader.read_line(&mut line).await.map_err(error)?;
        if count==0||!line.ends_with('\n'){break;}
        if count>512*1024{return Err(error("Stored event exceeds the supported frame size"));}
        if !frames.is_empty()&&page_bytes+count>768*1024{break;}
        page_bytes+=count;
        let frame:Value=serde_json::from_str(&line).map_err(error)?;
        if frame["t"]=="exit" && !ex.state.store.list("runner_commands").map_err(error)?.iter().any(|r|
            r["result"]["worker_id"]==worker&&r["exit"].is_object()){break;}
        frames.push(frame);after+=count as u64;
    }
    let observed=ex.state.store.list("runner_commands").map_err(error)?.iter().find(|row|row["result"]["worker_id"]==worker).map(facts);
    let stopped=observed.is_some_and(|facts|facts.stopped);
    let unknown=observed.is_some_and(|facts|facts.stopped&&!facts.outcome_known);
    Ok(Json(json!({"worker_id":worker,"after":after,"frames":frames,"execution_stopped":stopped,"outcome_unknown":unknown})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {run_id:String,worker_id:String,signal:String,#[serde(default="first_generation")] generation:u64}
async fn signal(State(ex):State<Arc<Execution>>,Json(input):Json<Signal>)->Reply{
    control(State(ex),Path(input.run_id),Json(Control{lease_millis:None,lease_expires_at:None,generation:input.generation,op:"Signal".into(),
        worker_id:Some(input.worker_id),signal:Some(input.signal),data:None,owner_run_id:None,owner_token:None,owner_workspace:None})).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stop {#[serde(default="first_generation")] generation:u64}
async fn teardown(State(ex):State<Arc<Execution>>,Path(run):Path<String>,Json(input):Json<Stop>)->Reply{
    valid_id(&run).map_err(error)?;
    let gate=ex.gate(&run).await;let _guard=gate.lock().await;
    let mut contract=ex.generation(&run,input.generation).map_err(error)?;
    if contract["state"]=="stopped"{return Ok(Json(json!({"run_id":run,"exit_confirmed":true})));}
    ex.handle(&run).await.map_err(error)?;
    contract["state"]=json!("stopping");ex.state.store.put("runner_runs",&run,&contract).map_err(error)?;
    container_exec::teardown_run_container_checked(&run).await.map_err(error)?;
    contract["state"]=json!("stopped");ex.state.store.put("runner_runs",&run,&contract).map_err(error)?;
    ex.handles.lock().await.remove(&run);
    Ok(Json(json!({"run_id":run,"exit_confirmed":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {#[serde(default="first_generation")] generation:u64,op:String,lease_millis:Option<u64>,lease_expires_at:Option<i64>,worker_id:Option<String>,signal:Option<String>,data:Option<String>,
    owner_run_id:Option<String>,owner_token:Option<String>,owner_workspace:Option<String>}
async fn control(State(ex):State<Arc<Execution>>,Path(run):Path<String>,Json(input):Json<Control>)->Reply{
    if input.op=="TeardownRun"{
        let _=teardown(State(ex),Path(run),Json(Stop{generation:input.generation})).await?;
        return Ok(Json(json!(container_exec::SupervisorFrame{ok:true,t:"resp".into(),..Default::default()})));
    }
    let gate=ex.gate(&run).await;let _guard=gate.lock().await;
    ex.active(&run,input.generation).map_err(error)?;
    let handle=ex.handle(&run).await.map_err(error)?;
    let _=(&input.owner_run_id,&input.owner_token,&input.owner_workspace);
    if matches!(input.op.as_str(),"WriteStdin"|"Signal") {
        let worker=input.worker_id.as_deref().ok_or_else(||error("Worker ID is required"))?;
        if !ex.state.store.list("runner_commands").map_err(error)?.iter().any(|row|
            row["run_id"]==run&&row["generation"]==input.generation&&row["result"]["worker_id"]==worker){return Err(error("Worker does not belong to Run generation"));}
    }
    let _guard = if input.op == "WriteStdin" { drop(_guard); None } else { Some(_guard) };
    let owner=(handle.scope=="shared").then_some(run.as_str());
    let token=(handle.scope=="shared").then_some(handle.owner_token.as_str());
    let frame=match input.op.as_str(){
        "Health"=>handle.link.health().await,
        "RenewLease"=>handle.link.renew_lease(input.lease_millis.ok_or_else(||error("Missing lease duration"))?,input.lease_expires_at.ok_or_else(||error("Missing lease expiry"))?).await,
        "Signal"=>{
            let signal=input.signal.as_deref().unwrap_or("");
            if !matches!(signal,"STOP"|"CONT"|"TERM"|"KILL"){return Err(error("Invalid signal"));}
            handle.link.signal(input.worker_id.as_deref().unwrap(),signal,owner,token).await
        },
        "WriteStdin"=>handle.link.write_stdin(input.worker_id.as_deref().unwrap(),input.data.as_deref().unwrap_or("")).await
            .map(|ok|container_exec::SupervisorFrame{ok,t:"resp".into(),..Default::default()}),
        "TeardownRun"=>handle.link.teardown(owner,token,(handle.scope=="shared").then_some(handle.container_workspace.as_str())).await,
        _=>return Err(error("Unsupported supervisor operation")),
    }.map_err(error)?;
    Ok(Json(json!(frame)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Orphan {expected:bool,#[serde(default="first_generation")] generation:u64}
async fn orphan_teardown(State(ex):State<Arc<Execution>>,Path(run):Path<String>,Json(input):Json<Orphan>)->Reply{
    valid_id(&run).map_err(error)?;
    let gate=ex.gate(&run).await;let _guard=gate.lock().await;
    let mut contract=ex.state.store.get("runner_runs",&run).map_err(error)?;
    if let Some(row)=contract.as_mut(){
        ex.generation(&run,input.generation).map_err(error)?;
        if row["state"]=="active"&&ex.handles.lock().await.contains_key(&run){return Err(error("Run is still attached; use confirmed teardown"));}
        row["state"]=json!("stopping");ex.state.store.put("runner_runs",&run,row).map_err(error)?;
    }
    if let Some(row)=contract.as_ref(){
        let identity=row["container_id"].as_str().ok_or_else(||error("Creation outcome lacks immutable identity; cannot confirm orphan cleanup"))?;
        let observation=recovery::inspect(identity,&ex.root).await.map_err(error)?;
        if observation["present"]==true {
            if row["scope"]=="shared"&&observation["execution_stopped"]!=true{return Err(error("Shared runtime is still running; cannot stop other Runs during orphan cleanup"));}
            let output=tokio::time::timeout(std::time::Duration::from_secs(20),crate::host_process::command("docker")
                .args(["rm","-f",identity]).kill_on_drop(true).output()).await.map_err(error)?.map_err(error)?;
            if !output.status.success(){return Err(error("Immutable orphan container removal was not confirmed"));}
        }
        drop(_guard);
        ex.reconcile().await.map_err(error)?;
        let current=ex.generation(&run,input.generation).map_err(error)?;
        if current["state"]!="stopped"{return Err(error("Orphan execution remains unconfirmed"));}
        return Ok(Json(json!({"run_id":run,"exit_confirmed":true})));
    }
    container_exec::teardown_orphan_run_container(&ex.state,&run,input.expected).await.map_err(error)?;
    if let Some(row)=contract.as_mut(){row["state"]=json!("stopped");ex.state.store.put("runner_runs",&run,row).map_err(error)?;}
    Ok(Json(json!({"run_id":run,"exit_confirmed":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftCatalog {engine:String,credential_id:String,#[serde(default)] provider:String}
async fn draft_catalog(State(ex):State<Arc<Execution>>,Json(input):Json<DraftCatalog>)->Reply{
    crate::model_settings_api::runner_draft_catalog(&ex.state,&input.credential_id,&input.engine,&input.provider).await
}
async fn recover_browser_profile(State(ex):State<Arc<Execution>>,Path(run):Path<String>)->Reply{
    valid_id(&run).map_err(error)?;
    let handle=ex.handle(&run).await.map_err(error)?;
    let workspace=crate::runs::session_dir(&ex.state,&run);
    tokio::task::spawn_blocking(move||crate::runs::clear_stale_browser_profile_lock_local(&workspace,&handle.name)).await.map_err(error)?.map_err(error)?;
    Ok(Json(json!({"run_id":run,"profile_lock_verified":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SrcEnvironment {task_id:String,operation:String,#[serde(default)] body:Value}
async fn src_environment(State(ex):State<Arc<Execution>>,Json(input):Json<SrcEnvironment>)->Reply{
    valid_id(&input.task_id).map_err(error)?;
    crate::runs::src_api::runner_environment(ex.state.clone(),&input.task_id,&input.operation,&input.body).await
        .map(Json).map_err(|e|(e.status,Json(json!({"error":e.message,"code":e.code}))))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SrcInspect {container_name:String}
async fn src_inspect(Json(input):Json<SrcInspect>)->Reply{
    let name=&input.container_name;
    let suffix=name.strip_prefix("muteki-src-egress-").or_else(||name.strip_prefix("muteki-src-browser-"));
    if !suffix.is_some_and(|s|s.len()==24&&s.bytes().all(|b|b.is_ascii_hexdigit())){return Err(error("Not a managed SRC runtime name"));}
    let output=tokio::time::timeout(std::time::Duration::from_secs(10),crate::host_process::command("docker")
        .args(["inspect",name]).kill_on_drop(true).output()).await.map_err(error)?.map_err(error)?;
    if !output.status.success(){
        if String::from_utf8_lossy(&output.stderr).contains("No such"){return Ok(Json(json!({"container":null})));}
        return Err(error("Managed SRC runtime inspection failed"));
    }
    let rows:Vec<Value>=serde_json::from_slice(&output.stdout).map_err(error)?;
    Ok(Json(json!({"container":rows.first()})))
}
async fn command_receipt(State(ex):State<Arc<Execution>>,Path(id):Path<String>)->Reply{
    valid_id(&id).map_err(error)?;
    ex.state.store.get("runner_commands",&id).map_err(error)?.map(|mut row|{let observed=facts(&row);row["execution_facts"]=json!({"process_exit_confirmed":observed.stopped,"outcome_known":observed.outcome_known,"artifacts_published":observed.published});Json(row)})
        .ok_or_else(||(StatusCode::NOT_FOUND,Json(json!({"error":"Command has not been recorded"}))))
}
async fn run_status(State(ex):State<Arc<Execution>>,Path(id):Path<String>)->Reply{
    valid_id(&id).map_err(error)?;
    let run = ex.state.store.get("runner_runs", &id).map_err(error)?.ok_or_else(|| {
        (StatusCode::NOT_FOUND, Json(json!({
            "code": "runner.run_not_recorded",
            "error": "Unknown Runner Run",
        })))
    })?;
    let commands:Vec<_>=ex.state.store.list("runner_commands").map_err(error)?.into_iter().filter(|r|r["run_id"]==id).collect();
    Ok(Json(json!({"run":run,"commands":commands})))
}
async fn resources(State(ex):State<Arc<Execution>>)->Reply {
    let link=ex.handles.lock().await.values().find(|handle|handle.link.is_alive()).map(|handle|handle.link.clone());
    ex.resources.snapshot(&ex.state,link).await.map(Json).map_err(error)
}
pub fn router(ex:Arc<Execution>)->Router{
    Router::new().route("/v1/resources",get(resources)).route("/v1/containers/ensure",post(ensure))
        .route("/v1/runs/{id}",get(run_status))
        .route("/v1/commands/{id}",get(command_receipt))
        .route("/v1/catalog/draft",post(draft_catalog))
        .route("/v1/src/environment",post(src_environment))
        .route("/v1/src/runtime-inspect",post(src_inspect))
        .route("/v1/containers/{run}/browser-profile/recover",post(recover_browser_profile))
        .route("/v1/containers/{run}/control",post(control))
        .route("/v1/containers/{run}/teardown",post(teardown))
        .route("/v1/containers/{run}/orphan-teardown",post(orphan_teardown))
        .route("/v1/workers/start",post(start)).route("/v1/workers/signal",post(signal))
        .route("/v1/workers/{worker}/events",get(events)).with_state(ex)
}
