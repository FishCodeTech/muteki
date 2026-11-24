//! Provider extensions are decoded before the common ACP schema drops metadata.
use super::*;
use std::collections::HashSet;
use std::io::{BufRead, Read, Seek, SeekFrom};

#[derive(Default)]
pub(super) struct VendorState {
    pub(crate) nodes: HashMap<String, Value>,
    notices: Vec<AgentEvent>,
    delegations: VecDeque<(String, Option<String>)>,
    child_text: HashMap<String, (String, String)>,
    root_message: Option<String>,
    context: Option<Value>,
    kimi_home: Option<PathBuf>,
    kimi_usage: Option<Result<(PathBuf, u64), String>>,
    terminal_session_grants: HashSet<String>,
    terminal_turn_grants: HashSet<String>,
}

fn terminal(status: &str) -> bool { matches!(status, "completed"|"failed"|"cancelled") }
fn omp_status(value: &Value) -> Option<&'static str> {
    match value.as_str()? { "pending"=>Some("pending"), "started"|"running"=>Some("running"), "completed"=>Some("completed"), "failed"=>Some("failed"), "aborted"|"cancelled"=>Some("cancelled"), _=>None }
}
fn text(value: &Value) -> String { value.as_str().map(str::to_owned).unwrap_or_else(||if value.is_null(){String::new()}else{value.to_string()}) }

impl State {
    pub(crate) fn vendor_configure(&mut self, opts: &SessionOptions) {
        let env = &opts.launch_env.env;
        let get = |name:&str|env.iter().rev().find(|(key,_)|key==name).map(|(_,value)|value.clone()).or_else(||std::env::var(name).ok());
        self.vendor.kimi_home = get("KIMI_CODE_HOME").map(PathBuf::from).or_else(||get("HOME").map(|home|PathBuf::from(home).join(".kimi-code")));
    }

    fn patch_native_agent(&mut self, mut patch: Value) -> Option<AgentEvent> {
        let id = patch["agent_id"].as_str()?.to_owned();
        let turn=self.turn.clone();
        let node = self.vendor.nodes.entry(id).or_insert_with(||json!({"native_turn_id":turn}));
        let previous = node["status"].as_str().unwrap_or("");
        let next = patch["status"].as_str().unwrap_or("");
        if (terminal(previous) && matches!(next,"pending"|"running")) || (previous=="running" && next=="pending") || (previous=="failed" && next=="completed") { patch.as_object_mut()?.remove("status"); }
        for (key,value) in patch.as_object()? { node[key]=value.clone(); }
        let event = AgentEvent::NativeAgentUpdate{patch:node.clone()};
        Some(if let Some(turn)=node["native_turn_id"].as_str(){AgentEvent::Origin{turn_id:turn.into(),agent_id:None,event:Box::new(event)}}else{event})
    }

    pub(crate) fn vendor_update(&mut self, family: &str, params: &Value) -> Vec<AgentEvent> {
        if self.loading { return Vec::new(); }
        let mut update = params["update"].clone();
        let kind = update["sessionUpdate"].as_str().unwrap_or("").to_owned();
        let meta = update["_meta"].clone();
        let owner = meta["cognition.ai/subagent_context"]["parentAgentId"].as_str().filter(|id|!id.is_empty()&&*id!="root").map(str::to_owned);
        let mut events = Vec::new();
        if family=="devin" {
            if kind=="tool_call" {
                let name=meta["cognition.ai/inferenceToolName"].as_str().unwrap_or("");
                if !name.is_empty() && matches!(update["title"].as_str(),None|Some(""|"Tool")) {update["title"]=json!(name);}
                if name=="run_subagent" && let Some(call)=update["toolCallId"].as_str(){self.vendor.delegations.push_back((call.into(),None));}
            }
            let started=&meta["cognition.ai/subagent_started"];
            if let Some(id)=started["agentId"].as_str() {
                let call=self.vendor.delegations.iter_mut().find(|(_,agent)|agent.is_none()).map(|(call,agent)|{*agent=Some(id.into());call.clone()});
                if let Some(event)=self.patch_native_agent(json!({"agent_id":id,"session_ref":id,"title":started["title"],"request":started["task"],"role":started["profile"],"model":started["model"],"call_id":call,"status":"running"})){events.push(event);}
            }
            let completed=&meta["cognition.ai/subagent_completed"];
            if let Some(id)=completed["agentId"].as_str() {
                let success=completed["success"]==true;
                let mut patch=json!({"agent_id":id,"status":if success{"completed"}else{"failed"}});
                patch[if success{"result"}else{"error"}]=completed["summary"].clone();
                if let Some(event)=self.patch_native_agent(patch){events.push(event);}
            }
            if kind=="usage_update" {
                if owner.is_some(){return events;}
                let mut snapshot=update.clone();snapshot.as_object_mut().map(|o|o.remove("_meta"));
                if self.vendor.context.as_ref()==Some(&snapshot){return events;}
                self.vendor.context=Some(snapshot);
            }
            if matches!(kind.as_str(),"agent_message_chunk"|"agent_thought_chunk") {
                let message=meta["cognition.ai/streamingMessageId"].as_str().map(str::to_owned);
                if let Some(owner)=owner.as_ref() {
                    let key=message.unwrap_or_else(||"unlabelled".into());
                    let chunk=update["content"]["text"].as_str().unwrap_or("");
                    let stream=self.vendor.child_text.entry(owner.clone()).or_default();
                    if stream.0!=key {stream.0=key;stream.1.clear();}stream.1.push_str(chunk);
                    let activity=stream.1.clone();
                    if let Some(event)=self.patch_native_agent(json!({"agent_id":owner,"activity":activity})){events.push(event);}
                    return events;
                }
                if let Some(message)=message {
                    if self.vendor.root_message.as_ref()!=Some(&message){events.extend(self.flush_text());self.vendor.root_message=Some(message);}
                }
            }
        }
        if family=="omp" && matches!(kind.as_str(),"tool_call"|"tool_call_update") {
            let call=update["toolCallId"].as_str().unwrap_or("").to_owned();
            let input=&update["rawInput"];
            let tasks:Vec<&Value>=input["tasks"].as_array().map(|v|v.iter().collect()).unwrap_or_else(||if input["task"].is_string(){vec![input]}else{Vec::new()});
            for task in tasks {if let Some(id)=task["name"].as_str(){if let Some(event)=self.patch_native_agent(json!({"agent_id":id,"call_id":call,"title":id,"request":task["task"],"role":task["agent"],"status":"pending"})){events.push(event);}}}
            let details=&update["rawOutput"]["details"];
            let task_details=details.get("projectAgentsDir").is_some()||details.get("totalDurationMs").is_some();
            if task_details {
                for progress in details["progress"].as_array().into_iter().flatten() {
                    let Some(id)=progress["id"].as_str()else{continue};
                    let model=progress.get("resolvedModel").or_else(||progress.get("modelOverride")).cloned().unwrap_or(Value::Null);
                    let mut patch=json!({"agent_id":id,"call_id":call,"title":progress["description"],"request":progress.get("assignment").or_else(||progress.get("task")),"role":progress["agent"],"model":if model.is_array(){model[0].clone()}else{model},"activity":progress.get("lastIntent").or_else(||progress.get("currentTool")),"tool_uses":progress["toolCount"],"total_tokens":progress["tokens"],"duration_ms":progress["durationMs"]});
                    if let Some(status)=omp_status(&progress["status"]){patch["status"]=json!(status);}
                    if let Some(error)=progress["retryFailure"]["errorMessage"].as_str(){patch["error"]=json!(error);}
                    if let Some(last)=progress["extractedToolData"]["yield"].as_array().and_then(|rows|rows.last()){patch["result"]=json!(text(last.get("data").unwrap_or(last)));}
                    patch.as_object_mut().map(|o|o.retain(|_,v|!v.is_null()));
                    if let Some(event)=self.patch_native_agent(patch){events.push(event);}
                }
                for result in details["results"].as_array().into_iter().flatten() {
                    let Some(id)=result["id"].as_str()else{continue};
                    let status=if result["aborted"]==true{"cancelled"}else if result["error"].as_str().is_some_and(|s|!s.is_empty())||result["exitCode"].as_i64().is_some_and(|n|n!=0){"failed"}else{"completed"};
                    let mut patch=json!({"agent_id":id,"call_id":call,"status":status,"title":result["description"],"request":result.get("assignment").or_else(||result.get("task")),"role":result["agent"],"model":result["resolvedModel"],"duration_ms":result["durationMs"],"total_tokens":result["tokens"]});
                    patch[if status=="failed"{"error"}else{"result"}]=result.get(if status=="failed" && result["error"].as_str().is_some_and(|s|!s.is_empty()){"error"}else{"output"}).cloned().unwrap_or(Value::Null);
                    patch.as_object_mut().map(|o|o.retain(|_,v|!v.is_null()));
                    if let Some(event)=self.patch_native_agent(patch){events.push(event);}
                }
            }
            if matches!(update["status"].as_str(),Some("completed"|"failed")) {
                for job in details["jobs"].as_array().into_iter().flatten().filter(|job|job["type"]=="task") {
                    let (Some(id),Some(status))=(job["id"].as_str(),omp_status(&job["status"]))else{continue};if !terminal(status){continue;}
                    let mut patch=json!({"agent_id":id});let node=self.vendor.nodes.get(id);
                    if !node.is_some_and(|n|terminal(n["status"].as_str().unwrap_or(""))){patch["status"]=json!(status);}
                    for(key,value)in [(if status=="completed"{"result"}else{"error"},job["resultText"].clone()),("duration_ms",job["durationMs"].clone()),("model",job["resolvedModel"].clone())]{if !value.is_null()&&node.is_none_or(|n|n[key].is_null()){patch[key]=value;}}
                    if let Some(event)=self.patch_native_agent(patch){events.push(event);}
                }
            }
        }
        if kind=="usage_update" && let Some(amount)=update["cost"]["amount"].as_f64().filter(|n|n.is_finite()&&*n>=0.0) && update["cost"]["currency"].as_str().is_some_and(|v|v.eq_ignore_ascii_case("USD")) {
            events.push(AgentEvent::UsageDetail{usage:json!({"cost_usd":amount,"cost_kind":if family=="omp"{"estimated"}else{"reported"},"native":{"cost":update["cost"]}}),scope:"session_cumulative".into(),usage_id:format!("{family}.acp.session_cost")});
        }
        if let Ok(update)=serde_json::from_value::<acp::SessionUpdate>(update){
            let mapped=self.apply_update(update);
            events.extend(mapped.into_iter().map(|event|if let Some(owner)=&owner{AgentEvent::Origin{turn_id:self.vendor.nodes.get(owner).and_then(|n|n["native_turn_id"].as_str()).or(self.turn()).unwrap_or("").into(),agent_id:Some(owner.clone()),event:Box::new(event)}}else{event}));
        }
        if !self.vendor.nodes.is_empty(){events.push(AgentEvent::BackgroundTasksChanged{count:self.vendor.nodes.values().filter(|node|!terminal(node["status"].as_str().unwrap_or(""))).count()});}
        events
    }

    pub(crate) fn kimi_usage_checkpoint(&mut self) {
        self.vendor.terminal_turn_grants.clear();
        let result=(||->Result<(PathBuf,u64),String>{
            let home=self.vendor.kimi_home.as_ref().ok_or("Kimi home unavailable")?;
            let index=std::fs::File::open(home.join("session_index.jsonl")).map_err(|e|e.to_string())?;
            let session:&str=self.session_id.as_ref().ok_or("Kimi session unavailable")?.0.as_ref();
            let cwd=self.cwd.canonicalize().map_err(|e|e.to_string())?;
            let mut found=None;
            for line in std::io::BufReader::new(index.take(32*1024*1024)).lines(){let row:Value=serde_json::from_str(&line.map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;if row["sessionId"]==session && row["workDir"].as_str().and_then(|p|Path::new(p).canonicalize().ok()).as_ref()==Some(&cwd){if let Some(dir)=row["sessionDir"].as_str(){found=Some(PathBuf::from(dir).join("agents/main/wire.jsonl"));}}}
            let path=found.ok_or("Kimi session absent from native index")?;let size=match std::fs::metadata(&path){Ok(m)=>m.len(),Err(e)if e.kind()==io::ErrorKind::NotFound=>0,Err(e)=>return Err(e.to_string())};Ok((path,size))
        })();self.vendor.kimi_usage=Some(result);
    }
    pub(crate) fn kimi_usage_finish(&mut self)->Option<TokenUsage>{
        let result=(||->Result<TokenUsage,String>{
            let(path,offset)=self.vendor.kimi_usage.take().ok_or("Kimi usage checkpoint missing")??;
            let mut file=std::fs::File::open(path).map_err(|e|e.to_string())?;let size=file.metadata().map_err(|e|e.to_string())?.len();if size<offset||size-offset>64*1024*1024{return Err("Kimi usage append outside bounds".into());}file.seek(SeekFrom::Start(offset)).map_err(|e|e.to_string())?;
            let mut totals=[0u64;4];let mut records=0;
            for line in std::io::BufReader::new(file.take(size-offset)).lines(){let row:Value=serde_json::from_str(&line.map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;if row["type"]!="usage.record"||row["agentId"]!="main"{continue;}for(i,key)in ["inputOther","output","inputCacheRead","inputCacheCreation"].iter().enumerate(){totals[i]=totals[i].checked_add(row["usage"][key].as_u64().ok_or("Invalid Kimi consumption counter")?).ok_or("Kimi consumption overflow")?;}records+=1;}
            if records==0{return Err("Kimi did not persist turn consumption counters".into());}
            Ok(TokenUsage{input_tokens:Some(totals[0].saturating_add(totals[2]).saturating_add(totals[3])),output_tokens:Some(totals[1]),cached_input_tokens:Some(totals[2]),cache_write_tokens:Some(totals[3]),turn_processed_tokens:Some(totals.iter().fold(0u64,|sum,value|sum.saturating_add(*value))),..self.usage.unwrap_or_default()})
        })();match result{Ok(usage)=>Some(usage),Err(error)=>{self.vendor.notices.push(AgentEvent::Warning{message:format!("kimi.usage_unavailable: {error}")});None}}
    }
}

impl State {
    pub(super) fn remember_terminal_grant(&mut self,args:&acp::RequestPermissionRequest,outcome:&acp::RequestPermissionOutcome) {
        if !self.shell_terminals || args.tool_call.fields.kind!=Some(acp::ToolKind::Execute){return;}
        let acp::RequestPermissionOutcome::Selected(selected)=outcome else{return};
        let Some(option)=args.options.iter().find(|option|option.option_id==selected.option_id)else{return};
        let command=args.tool_call.fields.raw_input.as_ref().and_then(|v|v["command"].as_str()).unwrap_or_else(||args.tool_call.fields.title.as_deref().unwrap_or("")).trim().to_owned();
        if command.is_empty(){return;}
        match option.kind{acp::PermissionOptionKind::AllowAlways=>{self.vendor.terminal_session_grants.insert(command);},acp::PermissionOptionKind::AllowOnce=>{self.vendor.terminal_turn_grants.insert(command);},_=>{}}
    }
    pub(super) fn terminal_granted(&self,command:&str)->bool {
        self.vendor.terminal_session_grants.contains(command)||self.vendor.terminal_turn_grants.contains(command)
    }
    pub(super) fn take_vendor_events(&mut self)->Vec<AgentEvent>{std::mem::take(&mut self.vendor.notices)}
    pub(super) fn vendor_turn_started(&mut self) { self.vendor.terminal_turn_grants.clear(); }
}
