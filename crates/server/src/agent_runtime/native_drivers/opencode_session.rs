//! State for the OpenCode 2 stream. All reconciliation and live frames pass through
//! one projection, so history backfill has the same identities as live output.
use super::*;
use std::collections::{HashSet, VecDeque};

struct Child { parent: String, origin: String, call: Option<String>, stopped: bool, running: bool }
struct Form { owner: String, origin: String, raw: Value }
struct Tool { name: String, input: Value, terminal: bool }

pub(super) struct State {
    client: Client, base: String, password: String, root: String, cwd: String,
    access: String, mcp: Option<String>, selected: Option<Value>, tx: Sender<AgentEvent>,
    mode: String, turn: Option<Turn>, last_turn: String, boundary: Option<(String,bool)>,
    delivered: HashSet<String>, steers: HashMap<String,String>, held_terminal: Option<Value>,
    children: HashMap<String,Child>, forms: HashMap<String,Form>, form_order: VecDeque<String>,
    presented: Option<String>, approvals: HashMap<String,(String,String)>, tools: HashMap<String,Tool>,
    texts: HashMap<String,String>, steps: HashSet<String>, commands: HashSet<String>,
    context_window: Option<u64>, interrupted: bool, reconnecting: bool,last_message:Option<String>,pending_turns:VecDeque<SessionCommand>,
}

fn file_inputs(attachments: &[agent::Attachment]) -> Result<Vec<Value>, RuntimeError> {
    attachments.iter().map(|attachment| {
        let uri = attachment.source_path.as_ref().and_then(|path| url::Url::from_file_path(path).ok())
            .ok_or_else(|| error("opencode2.attachment_invalid", "Attachment has no authorized workspace path"))?;
        Ok(json!({"uri":uri.as_str()}))
    }).collect()
}

impl State {
    pub(super) async fn new(client: Client, base: String, password: String, root: String, cwd: String,
        access: String, mcp: Option<String>, selected: Option<Value>, tx: Sender<AgentEvent>) -> Result<Self,RuntimeError> {
        let mut state=Self {client,base,password,root,cwd,access,mcp,selected,tx,mode:"default".into(),
            turn:None,last_turn:String::new(),boundary:None,delivered:HashSet::new(),steers:HashMap::new(),held_terminal:None,
            children:HashMap::new(),forms:HashMap::new(),form_order:VecDeque::new(),presented:None,approvals:HashMap::new(),
            tools:HashMap::new(),texts:HashMap::new(),steps:HashSet::new(),commands:HashSet::new(),context_window:None,interrupted:false,reconnecting:false,last_message:None,pending_turns:VecDeque::new()};
        state.catalog().await?;
        Ok(state)
    }
    async fn call(&self, method:Method,path:&str,body:Option<Value>) -> Result<Value,RuntimeError> {
        request(&self.client,&self.base,&self.password,method,path,body).await
    }
    fn path(&self,sid:&str,suffix:&str) -> String {format!("/api/session/{sid}{suffix}")}
    fn origin(&self,sid:&str)->String {
        self.children.get(sid).map(|child|child.origin.clone()).unwrap_or_else(||self.turn.as_ref().map(|turn|turn.id.clone()).unwrap_or_else(||self.last_turn.clone()))
    }
    fn event(&self,sid:&str,event:AgentEvent) {
        let origin=self.origin(sid);
        emit(&self.tx,if origin.is_empty() {event} else {AgentEvent::Origin{turn_id:origin,agent_id:(sid!=self.root).then(||sid.to_owned()),event:Box::new(event)}});
    }
    fn child_status(&self,sid:&str,status:ItemStatus,output:Option<&str>){
        let Some(child)=self.children.get(sid)else{return};
        let status=match status{ItemStatus::InProgress=>"running",ItemStatus::Completed=>"completed",ItemStatus::Failed=>"failed",ItemStatus::Interrupted|ItemStatus::Declined=>"cancelled"};
        let mut patch=json!({"agent_id":sid,"parent_id":if child.parent==self.root{Value::Null}else{json!(child.parent)},"status":status,"call_id":child.call,"session_ref":sid});
        if let Some(output)=output{patch["result"]=json!(output);}
        self.event(sid,AgentEvent::NativeAgentUpdate{patch});
    }
    fn close_child(&mut self,sid:&str,status:ItemStatus,output:Option<String>){
        let link=self.children.get_mut(sid).map(|child|{child.running=false;(child.parent.clone(),child.call.clone())});
        if let Some((parent,Some(call)))=link{if let Some(tool)=self.tools.get_mut(&call){tool.terminal=true;let item=ThreadItem{id:call,parent_item_id:(parent!=self.root).then(||format!("agent:{parent}")),content:ItemContent::ToolCall{presentation: None,image_reads:Vec::new(),name:tool.name.clone(),input:tool.input.clone(),output:output.clone(),status}};self.event(&parent,AgentEvent::ItemCompleted(item));}}
        self.child_status(sid,status,output.as_deref());
        self.event(sid,AgentEvent::ItemCompleted(ThreadItem{id:format!("agent:{sid}"),parent_item_id:None,content:ItemContent::ToolCall{presentation: None,image_reads:Vec::new(),name:"subagent".into(),input:json!({}),output,status}}));
    }
    fn liveness(&self) {
        emit(&self.tx,AgentEvent::BackgroundTasksChanged{count:self.children.values().filter(|child|child.running && !child.stopped).count()});
    }
    pub(super) async fn restore_configuration(&mut self)->Result<(),RuntimeError>{
        let mut permissions=rules(&self.access,self.mode=="plan");
        if let Some(name)=&self.mcp{permissions.as_array_mut().unwrap().extend([json!({"action":"muteki-*","resource":"*","effect":"deny"}),json!({"action":format!("{name}_*"),"resource":"*","effect":"allow"})]);}
        self.call(Method::PATCH,&self.path(&self.root,""),Some(json!({"permissions":permissions}))).await?;
        self.catalog().await
    }
    async fn catalog(&mut self)->Result<(),RuntimeError> {
        let location=url::form_urlencoded::byte_serialize(self.cwd.as_bytes()).collect::<String>();
        let rows=envelope(self.call(Method::GET,&format!("/api/command?location%5Bdirectory%5D={location}"),None).await?,"command.list")?;
        let rows=rows.as_array().ok_or_else(||error("opencode2.protocol_invalid","command.list is not an array"))?;
        let mut commands=Vec::new();
        self.commands.clear();
        for row in rows {
            if let Some(name)=text(row,"name") {
                self.commands.insert(name.into());
                commands.push(agent::ProviderCommand{name:name.into(),description:text(row,"description").map(str::to_owned),kind:agent::ProviderCommandKind::Command});
            }
        }
        for name in ["mcp","agents","status"] {
            commands.push(agent::ProviderCommand{name:name.into(),description:Some(format!("OpenCode {name}")),kind:agent::ProviderCommandKind::Command});
        }
        emit(&self.tx,AgentEvent::ProviderCommands{commands});
        if let Ok(rows)=self.call(Method::GET,&format!("/api/model?location%5Bdirectory%5D={location}"),None).await.and_then(|v|envelope(v,"model.list")) {
            self.context_window=rows.as_array().into_iter().flatten().find(|row|self.selected.as_ref().is_some_and(|model|row["providerID"]==model["providerID"] && (row["id"]==model["id"] || row["modelID"]==model["id"])))
                .and_then(|row|row["limit"]["input"].as_u64().or_else(||row["limit"]["context"].as_u64())).filter(|n|*n>0);
        }
        Ok(())
    }
    fn begin(&mut self,id:String) {
        self.interrupted=false; self.last_turn=id.clone();
        self.turn=Some(Turn{id:id.clone(),started:false,usage:TokenUsage{context_window:self.context_window,..Default::default()}});
        emit(&self.tx,AgentEvent::TurnStarted{turn_id:id});
    }
    pub(super) fn lost(&mut self,err:RuntimeError){
        self.cancel_pending();let message=err.message.clone();let children=self.children.iter().filter(|(_,child)|child.running&&!child.stopped).map(|(id,_)|id.clone()).collect::<Vec<_>>();
        for sid in children{self.close_child(&sid,ItemStatus::Failed,Some(message.clone()));if let Some(child)=self.children.get_mut(&sid){child.stopped=true;}}
        self.liveness();self.fail(err);
    }
    fn fail(&mut self,err:RuntimeError) {
        emit(&self.tx,AgentEvent::TurnFailed{code:err.code.to_owned(),message:err.message});
        if let Some(turn)=self.turn.take() {emit(&self.tx,AgentEvent::TurnCompleted{turn_id:turn.id,status:TurnStatus::Failed,usage:Some(turn.usage)});}
    }
    async fn switch_mode(&mut self,mode:&str)->Result<(),RuntimeError> {
        if !matches!(mode,"default"|"plan") {return Err(error("opencode2.mode_invalid",mode));}
        if self.mode==mode {return Ok(());}
        self.call(Method::POST,&self.path(&self.root,"/agent"),Some(json!({"agent":if mode=="plan" {"plan"}else{"build"}}))).await?;
        let mut permissions=rules(&self.access,mode=="plan");
        if let Some(name)=&self.mcp {permissions.as_array_mut().unwrap().extend([json!({"action":"muteki-*","resource":"*","effect":"deny"}),json!({"action":format!("{name}_*"),"resource":"*","effect":"allow"})]);}
        self.call(Method::PATCH,&self.path(&self.root,""),Some(json!({"permissions":permissions}))).await?;
        self.mode=mode.into();Ok(())
    }
    async fn submit(&mut self,delivery:u64,prompt:String,options:Option<agent::TurnOptions>,attachments:Vec<agent::Attachment>)->Result<(),RuntimeError> {
        if self.turn.is_some() || self.reconnecting {return Err(error("opencode2.turn_busy","OpenCode execution or reconnect is active"));}
        self.begin(message_id());
        let mode=options.as_ref().and_then(|v|v.interaction_mode.as_deref()).unwrap_or("default");
        self.switch_mode(mode).await?;
        if let Some(model)=&self.selected {self.call(Method::POST,&self.path(&self.root,"/model"),Some(json!({"model":model}))).await?;}
        let command=prompt.strip_prefix('/').map(|s|s.split_once(char::is_whitespace).unwrap_or((s,"")));
        if let Some((name,args))=command.filter(|(name,_)|matches!(*name,"mcp"|"agents"|"status")) {
            if !args.trim().is_empty() || !attachments.is_empty() {return Err(error("opencode2.operation_arguments_invalid","This operation accepts no arguments or attachments"));}
            let location=url::form_urlencoded::byte_serialize(self.cwd.as_bytes()).collect::<String>();
            let path=match name {"mcp"=>format!("/api/mcp?location%5Bdirectory%5D={location}"),"agents"=>format!("/api/agent?location%5Bdirectory%5D={location}"),_=>self.path(&self.root,"")};
            let result=envelope(self.call(Method::GET,&path,None).await?,name)?;
            emit(&self.tx,AgentEvent::TurnAccepted{delivery_id:delivery});
            let turn=self.turn.take().unwrap();
            emit(&self.tx,AgentEvent::ItemCompleted(ThreadItem{id:format!("{}:operation",turn.id),parent_item_id:None,content:ItemContent::AssistantMessage{text:serde_json::to_string_pretty(&result).unwrap_or_else(|_|result.to_string())}}));
            emit(&self.tx,AgentEvent::TurnCompleted{turn_id:turn.id,status:TurnStatus::Completed,usage:None});return Ok(());
        }
        let id=self.turn.as_ref().unwrap().id.clone();
        let (path,body,check)=if let Some((name,args))=command.filter(|(name,_)|self.commands.contains(*name)) {
            if !attachments.is_empty() {
                return Err(error("opencode2.command_attachments_unsupported", "OpenCode commands do not accept file attachments. Send the files in a normal message."));
            }
            let newest=envelope(self.call(Method::GET,&self.path(&self.root,"/message?order=desc&limit=1"),None).await?,"message.list")?;
            self.boundary=Some((newest[0]["id"].as_str().unwrap_or("").into(),true));
            (self.path(&self.root,"/command"),json!({"name":name,"text":args}),false)
        } else {
            self.boundary=Some((id.clone(),false));
            let files=file_inputs(&attachments)?;
            (self.path(&self.root,"/prompt"),json!({"id":id,"text":prompt,"files":files}),true)
        };
        let result=self.call(Method::POST,&path,Some(body)).await?;
        if check && envelope(result,"prompt")?["id"] != id {return Err(error("opencode2.admission_invalid","Input acknowledgement differs from submitted id"));}
        emit(&self.tx,AgentEvent::TurnAccepted{delivery_id:delivery});Ok(())
    }
    pub(super) async fn command(&mut self,command:SessionCommand)->bool {
        let result=match command {
            SessionCommand::SendTurn{delivery_id,text,options,attachments}=>{
                if self.turn.is_some() || self.reconnecting {self.pending_turns.push_back(SessionCommand::SendTurn{delivery_id,text,options,attachments});return true;}
                match self.submit(delivery_id,text,options,attachments).await{Err(err)if err.code=="runtime.delivery_unknown"=>{self.control_failed(&delivery_id.to_string(),"turn",&err);Ok(())},result=>result}
            },
            SessionCommand::Compact=>{
                if self.turn.is_some()||self.reconnecting{emit(&self.tx,AgentEvent::Warning{message:"OpenCode compaction rejected while execution or reconnect is active".into()});return true;}else{
                    let id=message_id();self.begin(id.clone());self.boundary=Some((id.clone(),false));
                    match self.call(Method::POST,&self.path(&self.root,"/compact"),Some(json!({"id":id}))).await.and_then(|v|envelope(v,"compact")) {
                        Ok(v) if v["id"]==id=>Ok(()),Ok(_)=>Err(error("opencode2.admission_invalid","Compaction acknowledgement differs")),Err(e)=>Err(e)
                    }
                }
            }
            SessionCommand::RespondApproval{request_id,decision}=>{
                let owner=self.approvals.get(&request_id).map(|v|v.0.clone());
                if let Some(owner)=owner {
                    let reply=match &decision {ApprovalDecision::Option(id)if id=="always"=>"always",ApprovalDecision::Option(id)if id=="once"=>"once",_=>"reject"};
                    match self.call(Method::POST,&self.path(&owner,&format!("/permission/{request_id}/reply")),Some(json!({"decision":reply}))).await {
                        Ok(_)=>{self.approvals.remove(&request_id);self.event(&owner,AgentEvent::ApprovalResolved{request_id,decision});Ok(())},Err(err)=>{self.control_failed(&request_id,"approval",&err);Ok(())}
                    }
                }else{self.control_failed(&request_id,"approval",&error("opencode2.request_stale","Permission is no longer pending"));Ok(())}
            }
            SessionCommand::RespondUserInput{request_id,answers}=>{
                if let Some(form)=self.forms.get(&request_id) {
                    let owner=form.owner.clone();let origin=form.origin.clone();let mut native=serde_json::Map::new();
                    for field in form.raw["fields"].as_array().into_iter().flatten() {
                        let Some(key)=text(field,"key")else{continue};let Some(value)=answers.get(key)else{continue};
                        let mut values=value.as_array().or_else(||value["values"].as_array()).or_else(||value["answers"].as_array()).map(|values|values.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();
                        if let Some(text)=value.as_str().or_else(||value["text"].as_str()).filter(|text|!text.trim().is_empty()){if !values.iter().any(|value|value==text){values.push(text.to_owned());}}
                        if !values.is_empty(){native.insert(key.into(),if field["type"]=="multiselect"{json!(values)}else{json!(values.join(", "))});}
                    }
                    let path=self.path(&owner,&format!("/form/{request_id}{}",if answers.is_empty(){""}else{"/reply"}));
                    let method=if answers.is_empty(){Method::DELETE}else{Method::POST};
                    match self.call(method,&path,(!answers.is_empty()).then(||json!({"answer":native}))).await {
                        Ok(_)=>{self.forms.remove(&request_id);self.presented=None;emit(&self.tx,AgentEvent::Origin{turn_id:origin,agent_id:(owner!=self.root).then_some(owner),event:Box::new(AgentEvent::UserInputResolved{request_id,answers})});self.present_form();Ok(())},
                        Err(err)=>{self.control_failed(&request_id,"user_input",&err);Ok(())}
                    }
                }else{self.control_failed(&request_id,"user_input",&error("opencode2.request_stale","Form is no longer pending"));Ok(())}
            }
            SessionCommand::Steer{request_id,text,attachments}=>{
                let id=format!("msg_muteki_steer_{}",request_id.replace('-',""));
                let files = match file_inputs(&attachments) {
                    Ok(files) => files,
                    Err(err) => { self.control_failed(&request_id, "steer", &err); return true; }
                };
                self.steers.insert(id.clone(),request_id.clone());
                match self.call(Method::POST,&self.path(&self.root,"/prompt"),Some(json!({"id":id,"text":text,"delivery":"steer","files":files}))).await.and_then(|v|envelope(v,"steer")) {
                    Ok(v)if v["id"]==id=>{emit(&self.tx,AgentEvent::ControlDelivery{request_id,operation:"steer".into(),status:"accepted".into(),error:None});Ok(())},
                    Err(err) if err.code=="runtime.delivery_unknown"=>{self.control_failed(&request_id,"steer",&err);Ok(())},
                    _=>{self.steers.remove(&id);emit(&self.tx,AgentEvent::SteerRejected{request_id});Ok(())}
                }
            }
            SessionCommand::Interrupt=>self.interrupt().await,
            SessionCommand::Shutdown=>{let _=self.interrupt().await;return false;},
            _=>Ok(())
        };
        if let Err(err)=result {self.fail(err);} true
    }
    pub(super) fn next_turn(&mut self)->Option<SessionCommand>{if self.turn.is_none()&&!self.reconnecting{self.pending_turns.pop_front()}else{None}}
    fn cancel_pending(&mut self){for command in self.pending_turns.drain(..){if let SessionCommand::SendTurn{delivery_id,..}=command{emit(&self.tx,AgentEvent::ControlDelivery{request_id:delivery_id.to_string(),operation:"turn".into(),status:"failed".into(),error:Some(json!({"code":"runtime.cancelled_before_delivery","message":"Queued turn was cancelled before native dispatch"}))});}}}
    fn control_failed(&self,id:&str,operation:&str,err:&RuntimeError){emit(&self.tx,AgentEvent::ControlDelivery{request_id:id.into(),operation:operation.into(),status:if err.code=="runtime.delivery_unknown"{"unknown"}else{"failed"}.into(),error:Some(json!({"code":err.code,"message":err.message}))});}
    async fn interrupt(&mut self)->Result<(),RuntimeError>{
        self.cancel_pending();self.interrupted=true;self.held_terminal=None;let mut failures=Vec::new();
        let mut inbox=self.steers.keys().cloned().collect::<Vec<_>>();inbox.sort();
        for id in inbox{match self.call(Method::DELETE,&self.path(&self.root,&format!("/inbox/{id}")),None).await{Ok(_)=>{if let Some(request_id)=self.steers.remove(&id){emit(&self.tx,AgentEvent::SteerRejected{request_id});}},Err(error)=>failures.push(format!("inbox {id}: {}",error.message))}}
        let depth=|sid:&str|{let mut sid=sid.to_owned();let mut seen=HashSet::new();while let Some(child)=self.children.get(&sid){if !seen.insert(sid.clone()){break}sid=child.parent.clone();}seen.len()};
        let mut children=self.children.keys().cloned().collect::<Vec<_>>();children.sort_by_key(|sid|(std::cmp::Reverse(depth(sid)),sid.clone()));
        for sid in children{match self.call(Method::POST,&self.path(&sid,"/interrupt"),Some(json!({"resume":false}))).await{Ok(_)=>{self.close_child(&sid,ItemStatus::Interrupted,None);if let Some(child)=self.children.get_mut(&sid){child.stopped=true;}},Err(error)=>failures.push(format!("session {sid}: {}",error.message))}}
        self.liveness();if let Err(error)=self.call(Method::POST,&self.path(&self.root,"/interrupt"),Some(json!({"resume":false}))).await{failures.push(format!("root: {}",error.message));}
        if failures.is_empty(){Ok(())}else{Err(error("opencode2.interrupt_incomplete",failures.join("; ")))}
    }
    fn present_form(&mut self){
        if self.presented.is_some(){return;}
        while let Some(id)=self.form_order.pop_front(){
            if let Some(form)=self.forms.get(&id){self.presented=Some(id.clone());emit(&self.tx,AgentEvent::Origin{turn_id:form.origin.clone(),agent_id:(form.owner!=self.root).then(||form.owner.clone()),event:Box::new(AgentEvent::UserInputRequested{request_id:id,questions:parse_form(&form.raw),delivery:if form.owner==self.root {UserInputDelivery::Blocking}else{UserInputDelivery::Async}})});break;}
        }
    }
    pub(super) async fn frame(&mut self,event:Value)->bool {
        let kind=event_name(&event).to_owned();
        if kind=="muteki.stream.closed"{self.lost(error("opencode2.sse_closed","Event stream recovery exhausted"));return false;}
        if kind=="muteki.stream.reconnecting"{self.reconnecting=true;emit(&self.tx,AgentEvent::Warning{message:"OpenCode event stream reconnecting; execution retained".into()});return true;}
        if kind=="server.connected" && self.reconnecting {
            match self.reconcile().await {
                Ok(frames)=>{for frame in frames {self.project(frame).await;}self.reconnecting=false;},
                Err(err)=>{self.lost(err);return false;}
            }
            return true;
        }
        self.project(event).await;true
    }
    async fn project(&mut self,event:Value){
        let kind=event_name(&event);let data=&event["data"];
        let sid=text(data,"sessionID").or_else(||text(&data["form"],"sessionID")).unwrap_or("").to_owned();
        if kind=="session.created" && let Some(parent)=text(data,"parentID") && (parent==self.root || self.children.contains_key(parent)) {
            if self.children.contains_key(&sid){return;}
            let origin=self.origin(parent);
            self.children.entry(sid.clone()).or_insert(Child{parent:parent.into(),origin,call:None,stopped:false,running:true});
            self.child_status(&sid,ItemStatus::InProgress,None);
            self.event(&sid,AgentEvent::ItemStarted(ThreadItem{id:format!("agent:{sid}"),parent_item_id:None,content:ItemContent::ToolCall{presentation: None,image_reads:Vec::new(),name:"subagent".into(),input:data.clone(),output:None,status:ItemStatus::InProgress}}));self.liveness();return;
        }
        if sid!=self.root && !self.children.contains_key(&sid){return;}
        if self.children.get(&sid).is_some_and(|child|child.stopped){return;}
        let child=sid!=self.root;
        if !child{if let Some(id)=text(data,"assistantMessageID"){self.last_message=Some(id.to_owned());}}
        match kind {
            "session.inbox.enqueued" if data["item"]["type"]=="synthetic"=>{
                let metadata=&data["item"]["payload"]["metadata"];
                if metadata["source"]=="subagent" && let Some(child_id)=text(metadata,"childID") {
                    if let Some(child)=self.children.get_mut(child_id){child.running=false;}
                    let status=if self.children.get(child_id).is_some_and(|child|child.stopped){ItemStatus::Interrupted}else{match text(metadata,"state"){Some("failed")=>ItemStatus::Failed,Some("cancelled")=>ItemStatus::Interrupted,_=>ItemStatus::Completed}};
                    self.close_child(child_id,status,text(&data["item"]["payload"],"text").map(str::to_owned));self.liveness();
                }
            }
            "session.inbox.delivered"|"session.inbox.cancelled"=>{
                let id=text(data,"inboxID").unwrap_or("").to_owned();
                if kind.ends_with("delivered"){self.delivered.insert(id.clone());}
                if let Some(request_id)=self.steers.remove(&id){emit(&self.tx,if kind.ends_with("delivered"){AgentEvent::SteerAccepted{request_id}}else{AgentEvent::SteerRejected{request_id}});}
                if !child && let Some(active)=&self.turn {
                    if id==active.id && kind.ends_with("cancelled"){self.finish("session.execution.interrupted",data).await;}
                    else if self.steers.is_empty() && self.delivered.contains(&active.id) && let Some(held)=self.held_terminal.take(){self.finish(event_name(&held),&held["data"]).await;}
                }
            }
            "session.execution.started"=>{
                if child{if let Some(child)=self.children.get_mut(&sid){child.running=true;}self.child_status(&sid,ItemStatus::InProgress,None);self.liveness();}
                else if let Some(active)=self.turn.as_mut(){active.started=true;}
                else if !self.interrupted {
                    let origin=self.last_turn.clone();let id=message_id();
                    emit(&self.tx,AgentEvent::ContinuationStarted{origin_turn_id:origin,turn_id:id.clone()});
                    self.boundary=Some((self.last_message.clone().unwrap_or_default(),true));self.delivered.insert(id.clone());
                    self.begin(id);if let Some(turn)=self.turn.as_mut(){turn.started=true;}
                }
            }
            "permission.asked"=>{
                if let Some(id)=text(data,"id") {
                    if self.approvals.contains_key(id){return;}
                    let origin=self.origin(&sid);self.approvals.insert(id.into(),(sid.clone(),origin.clone()));
                    let action=text(data,"action").unwrap_or("tool");
                    let detail=data["resources"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
                    let kind=if action=="shell"{ApprovalKind::ExecCommand{command:detail,cwd:None,reason:None}}else{ApprovalKind::ToolUse{name:action.into(),input:data.clone(),detail}};
                    self.event(&sid,AgentEvent::ApprovalRequested(ApprovalRequest {native_execution: Some(json!({"tool_name":action,"action":action,"arguments":data,"resource_kind":if action=="shell"{"process"}else if matches!(action,"edit"|"write"){"file"}else{"mcp"}})), tool_call_id: data["toolCallID"].as_str().map(str::to_owned), id:id.into(),turn_id:Some(origin),parent_item_id:child.then(||format!("agent:{sid}")),kind,options:vec![ApprovalOption{id:"once".into(),label:"Allow once".into(),kind:ApprovalOptionKind::AllowOnce},ApprovalOption{id:"always".into(),label:"Always allow".into(),kind:ApprovalOptionKind::AllowAlways},ApprovalOption{id:"reject".into(),label:"Reject".into(),kind:ApprovalOptionKind::RejectOnce}]}));
                }
            }
            "permission.replied"=>{
                if let Some(id)=text(data,"requestID").or_else(||text(data,"id")) && self.approvals.remove(id).is_some(){self.event(&sid,AgentEvent::ApprovalResolved{request_id:id.into(),decision:if text(data,"reply")==Some("reject"){ApprovalDecision::Cancel}else{ApprovalDecision::Option("once".into())}});}
            }
            "form.created"=>{
                let form=&data["form"];
                if let Some(id)=text(form,"id") {
                    if self.forms.contains_key(id){return;}
                    let supported=form["fields"].as_array().is_some_and(|fields|!fields.is_empty() && fields.iter().all(|field|matches!(text(field,"type"),Some("string"|"multiselect")) && field["hidden"]!=true && field["when"].is_null()));
                    if !supported {
                        let result=self.call(Method::DELETE,&self.path(&sid,&format!("/form/{id}")),None).await;
                        emit(&self.tx,AgentEvent::Warning{message:format!("OpenCode form {id} has unsupported field types or conditional fields; cancellation {}",if result.is_ok(){"confirmed"}else{"failed"})});return;
                    }
                    self.forms.insert(id.into(),Form{owner:sid.clone(),origin:self.origin(&sid),raw:form.clone()});self.form_order.push_back(id.into());self.present_form();
                }
            }
            "form.replied"|"form.cancelled"=>{
                if let Some(id)=text(data,"requestID").or_else(||text(data,"id")) && self.forms.remove(id).is_some(){
                    if self.presented.as_deref()==Some(id){self.presented=None;self.event(&sid,AgentEvent::UserInputResolved{request_id:id.into(),answers:serde_json::Map::new()});}self.present_form();
                }
            }
            "session.text.delta"|"session.text.ended"|"session.reasoning.delta"|"session.reasoning.ended"=>{
                let reasoning=kind.starts_with("session.reasoning");let id=format!("{sid}:{}:{}:{}",if reasoning{"reasoning"}else{"text"},data["assistantMessageID"].as_str().unwrap_or("assistant"),data["ordinal"].as_u64().unwrap_or(0));
                let old=self.texts.entry(id.clone()).or_default();let ended=kind.ends_with(".ended");
                let next=if ended{text(data,"text").unwrap_or("").into()}else{format!("{}{}",old,text(data,"delta").unwrap_or(""))};
                if *old==next && !ended{return;}*old=next.clone();
                let content=if reasoning{ItemContent::Reasoning{text:next}}else{ItemContent::AssistantMessage{text:next}};
                let item=ThreadItem{id,parent_item_id:child.then(||format!("agent:{sid}")),content};
                self.event(&sid,if ended{AgentEvent::ItemCompleted(item)}else{AgentEvent::ItemUpdated(item)});
            }
            "session.tool.input.started"|"session.tool.called"|"session.tool.progress"|"session.tool.success"|"session.tool.failed"=>{
                if let Some(call)=text(data,"id") {
                    let id=format!("{sid}:{call}");let entry=self.tools.entry(id.clone()).or_insert(Tool{name:text(data,"name").unwrap_or("tool").into(),input:json!({}),terminal:false});
                    if entry.terminal{return;}if let Some(name)=text(data,"name"){entry.name=name.into();}if !data["input"].is_null(){entry.input=data["input"].clone();}
                    let name=entry.name.clone();let input=entry.input.clone();let metadata=&data["metadata"];
                    let terminal=(kind.ends_with(".success")||kind.ends_with(".failed")) && metadata["status"]!="running";
                    entry.terminal=terminal;
                    if let Some(child_id)=text(metadata,"sessionID") {
                        let origin=self.origin(&sid);
                        let child=self.children.entry(child_id.into()).or_insert(Child{parent:sid.clone(),origin,call:None,stopped:false,running:true});child.call=Some(id.clone());
                    }
                    let status=if kind.ends_with(".failed"){ItemStatus::Failed}else if terminal{ItemStatus::Completed}else{ItemStatus::InProgress};
                    let output=data.get("content").or_else(||data.get("error")).map(|v|v.as_str().map(str::to_owned).unwrap_or_else(||v.to_string()));
                    if let Some(child_id)=text(metadata,"sessionID") {
                        if terminal { self.close_child(child_id,status,output.clone()); }
                        else { self.child_status(child_id,ItemStatus::InProgress,None); }
                    }
                    let item=ThreadItem{id,parent_item_id:child.then(||format!("agent:{sid}")),content:ItemContent::ToolCall{presentation: None,image_reads:Vec::new(),name,input,output,status}};
                    self.event(&sid,if terminal{AgentEvent::ItemCompleted(item)}else{AgentEvent::ItemUpdated(item)});self.liveness();
                }
            }
            "session.compaction.started"=>self.event(&sid,AgentEvent::ContextCompacted(Compaction{in_progress:true,trigger:Some(if text(data,"reason")==Some("manual"){"manual"}else{"auto"}.into()),..Default::default()})),
            "session.compaction.completed"|"session.compaction.ended"|"session.compaction.failed"=>self.event(&sid,AgentEvent::ContextCompacted(Compaction{in_progress:false,status:Some(if kind.ends_with("failed"){"failed"}else{"completed"}.into()),summary:text(data,"text").map(str::to_owned),message:text(data,"message").map(str::to_owned),..Default::default()})),
            "session.step.ended"|"session.step.failed"=>{
                let step=format!("{sid}:{}",data["assistantMessageID"].as_str().unwrap_or(""));if !self.steps.insert(step){return;}
                let tokens=&data["tokens"];let input=tokens["input"].as_u64();let output=tokens["output"].as_u64();let cached=tokens["cache"]["read"].as_u64();let write=tokens["cache"]["write"].as_u64();let reasoning=tokens["reasoning"].as_u64();
                let sum=|values:&[Option<u64>]|values.iter().any(Option::is_some).then(||values.iter().flatten().sum::<u64>());
                let usage=TokenUsage{input_tokens:sum(&[input,cached,write]),output_tokens:sum(&[output,reasoning]),cached_input_tokens:cached,cache_write_tokens:write,reasoning_tokens:reasoning,used_tokens:sum(&[input,output,reasoning,cached,write]),context_window:self.context_window,cost_usd:data["cost"].as_f64(),..Default::default()};
                if !child && let Some(active)=self.turn.as_mut(){
                    active.usage.input_tokens=sum(&[active.usage.input_tokens,usage.input_tokens]);active.usage.output_tokens=sum(&[active.usage.output_tokens,usage.output_tokens]);active.usage.cached_input_tokens=sum(&[active.usage.cached_input_tokens,cached]);active.usage.cache_write_tokens=sum(&[active.usage.cache_write_tokens,write]);active.usage.reasoning_tokens=sum(&[active.usage.reasoning_tokens,reasoning]);active.usage.cost_usd=match(active.usage.cost_usd,usage.cost_usd){(None,None)=>None,(a,b)=>Some(a.unwrap_or(0.)+b.unwrap_or(0.))};active.usage.used_tokens=usage.used_tokens;
                }
                self.event(&sid,AgentEvent::UsageDetail{usage:json!({"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"cache_read_tokens":cached,"cache_write_tokens":write,"reasoning_tokens":reasoning,"cost_usd":usage.cost_usd,"input_includes_cache":true}),scope:"message".into(),usage_id:data["assistantMessageID"].as_str().unwrap_or("").into()});
                self.event(&sid,AgentEvent::TokenUsage(usage));
            }
            "session.execution.succeeded"|"session.execution.failed"|"session.execution.interrupted"=>{
                if child {
                    let output=self.texts.iter().filter(|(key,_)|key.starts_with(&format!("{sid}:text:"))).map(|(_,v)|v.as_str()).collect::<Vec<_>>().join("\n");
                    let status=if kind.ends_with("succeeded"){ItemStatus::Completed}else if kind.ends_with("interrupted"){ItemStatus::Interrupted}else{ItemStatus::Failed};
                    self.close_child(&sid,status,Some(output));self.liveness();
                }else if self.turn.as_ref().is_some_and(|t|t.started || self.interrupted) {
                    if data["reconciled"]==true || self.interrupted || (self.steers.is_empty() && self.turn.as_ref().is_some_and(|t|self.delivered.contains(&t.id))) || self.boundary.as_ref().is_some_and(|(_,before)|*before){self.finish(kind,data).await;}else{self.held_terminal=Some(event.clone());}
                }
            }
            "command.updated"=>{if let Err(err)=self.catalog().await{emit(&self.tx,AgentEvent::Warning{message:err.message});}},
            _=>{}
        }
    }
    async fn finish(&mut self,kind:&str,data:&Value){
        let Some(turn)=self.turn.take()else{return};
        let status=if kind.ends_with("succeeded"){TurnStatus::Completed}else if kind.ends_with("interrupted"){TurnStatus::Interrupted}else{TurnStatus::Failed};
        if status==TurnStatus::Failed{emit(&self.tx,AgentEvent::TurnFailed{code:"opencode2.execution_failed".into(),message:text(&data["error"],"message").unwrap_or("OpenCode execution failed").into()});}
        if let Ok(rows)=self.call(Method::GET,&self.path(&self.root,"/message?order=desc&limit=5&type=assistant"),None).await.and_then(|v|envelope(v,"message.list")) && let Some(row)=rows.as_array().and_then(|rows|rows.first()) && let(Some(provider),Some(model))=(text(&row["model"],"providerID"),text(&row["model"],"id")){emit(&self.tx,AgentEvent::ServedModel{model:format!("{provider}/{model}"),reason:None});}
        emit(&self.tx,AgentEvent::TurnCompleted{turn_id:turn.id,status,usage:Some(turn.usage)});
        self.boundary=None;
    }
    async fn reconcile(&mut self)->Result<Vec<Value>,RuntimeError>{
        let active=envelope(self.call(Method::GET,"/api/session/active",None).await?,"session.active")?;
        let active=active.as_object().ok_or_else(||error("opencode2.protocol_invalid","session.active is not an object"))?;
        let mut frames=Vec::new();
        if let Some((boundary,before))=self.boundary.clone(){
            let mut path=self.path(&self.root,"/message?order=desc&limit=50");let mut messages=Vec::new();let mut cursors=HashSet::new();let mut found=boundary.is_empty();
            'pages:loop{
                let response=self.call(Method::GET,&path,None).await?;let data=envelope(response.clone(),"message.list")?;
                let rows=data.as_array().ok_or_else(||error("opencode2.protocol_invalid","message.list is not an array"))?;
                for row in rows {if row["id"]==boundary {found=true;if !before{messages.push(row.clone());}break 'pages;}messages.push(row.clone());}
                let Some(cursor)=text(&response["cursor"],"next").filter(|_|!rows.is_empty())else{break};
                if !cursors.insert(cursor.to_owned()){return Err(error("opencode2.protocol_invalid","Repeated message cursor"));}
                path=self.path(&self.root,&format!("/message?limit=50&cursor={}",url::form_urlencoded::byte_serialize(cursor.as_bytes()).collect::<String>()));
            }
            if found {
                let idle=messages.iter().find(|m|m["type"]=="idle").cloned();
                for message in messages.iter().rev().filter(|m|m["type"]=="assistant"){
                    let mut text_ordinal=0;let mut reasoning_ordinal=0;
                    for part in message["content"].as_array().into_iter().flatten(){
                        let base=json!({"sessionID":self.root,"assistantMessageID":message["id"]});let mut data=base;
                        let kind=match text(part,"type") {
                            Some("text")=>{data["ordinal"]=json!(text_ordinal);text_ordinal+=1;data["text"]=part["text"].clone();"session.text.ended"},
                            Some("reasoning")=>{data["ordinal"]=json!(reasoning_ordinal);reasoning_ordinal+=1;data["text"]=part["text"].clone();"session.reasoning.ended"},
                            Some("tool")=>{
                                data["id"]=part["id"].clone();data["name"]=part["name"].clone();data["input"]=part["state"]["input"].clone();data["content"]=part["state"]["content"].clone();data["metadata"]=part["state"]["metadata"].clone();data["error"]=part["state"]["error"].clone();
                                match text(&part["state"],"status"){Some("completed")=>"session.tool.success",Some("error")=>"session.tool.failed",_=>"session.tool.called"}
                            },_=>continue
                        };frames.push(json!({"type":kind,"data":data}));
                    }
                }
                if !active.contains_key(&self.root){
                    if let Some(turn)=self.turn.as_mut(){turn.started=true;self.delivered.insert(turn.id.clone());}
                    frames.push(json!({"type":match idle.as_ref().and_then(|m|text(m,"outcome")){Some("succeeded")=>"session.execution.succeeded",Some("failed")=>"session.execution.failed",_=>"session.execution.interrupted"},"data":{"sessionID":self.root,"reconciled":true}}));
                }
            }else if !active.contains_key(&self.root) && self.turn.is_some(){if let Some(turn)=self.turn.as_mut(){turn.started=true;}frames.push(json!({"type":"session.execution.interrupted","data":{"sessionID":self.root,"reconciled":true}}));}
        }
        let sessions=std::iter::once(self.root.clone()).chain(self.children.keys().cloned()).collect::<Vec<_>>();
        let mut listed=HashSet::new();
        for sid in sessions {
            if !active.contains_key(&sid){if self.children.get(&sid).is_some_and(|child|child.running&&!child.stopped){self.close_child(&sid,ItemStatus::Interrupted,Some("Background execution ended while disconnected; no terminal report was available".into()));}continue;}
            for (suffix,kind) in [("/permission","permission.asked"),("/form","form.created")] {
                let rows=envelope(self.call(Method::GET,&self.path(&sid,suffix),None).await?,suffix)?;
                let rows=rows.as_array().ok_or_else(||error("opencode2.protocol_invalid","Pending requests are not an array"))?;
                for row in rows {if let Some(id)=text(row,"id"){listed.insert(id.to_owned());}let data=if kind=="form.created"{json!({"sessionID":sid,"form":row})}else{let mut data=row.clone();data["sessionID"]=json!(sid);data};frames.push(json!({"type":kind,"data":data}));}
            }
        }
        let stale=self.approvals.keys().filter(|id|!listed.contains(*id)).cloned().collect::<Vec<_>>();
        for id in stale {if let Some((sid,_))=self.approvals.remove(&id){self.event(&sid,AgentEvent::ApprovalResolved{request_id:id,decision:ApprovalDecision::Cancel});}}
        let stale=self.forms.keys().filter(|id|!listed.contains(*id)).cloned().collect::<Vec<_>>();
        for id in stale{if let Some(form)=self.forms.remove(&id){if self.presented.as_deref()==Some(&id){self.presented=None;self.event(&form.owner,AgentEvent::UserInputResolved{request_id:id,answers:serde_json::Map::new()});}}}
        if !active.contains_key(&self.root){for(_,request_id)in self.steers.drain(){emit(&self.tx,AgentEvent::ControlDelivery{request_id,operation:"steer".into(),status:"consumption_unknown".into(),error:Some(json!({"code":"opencode2.steer_consumption_unknown","message":"Input was admitted before reconnect; consumption was not observed"}))});}}
        self.present_form();self.liveness();Ok(frames)
    }
}

#[cfg(test)]
#[tokio::test]
async fn nested_form_session_keeps_schema_and_cancel_removes_the_card() {
    let (tx, rx) = smol::channel::unbounded();
    let mut state = State {
        client: Client::new(),
        base: "http://127.0.0.1:9".into(),
        password: String::new(),
        root: "ses_root".into(),
        cwd: "/tmp".into(),
        access: "default".into(),
        mcp: None,
        selected: None,
        tx,
        mode: "default".into(),
        turn: None,
        last_turn: String::new(),
        boundary: None,
        delivered: HashSet::new(),
        steers: HashMap::new(),
        held_terminal: None,
        children: HashMap::new(),
        forms: HashMap::new(),
        form_order: VecDeque::new(),
        presented: None,
        approvals: HashMap::new(),
        tools: HashMap::new(),
        texts: HashMap::new(),
        steps: HashSet::new(),
        commands: HashSet::new(),
        context_window: None,
        interrupted: false,
        reconnecting: false,
        last_message: None,
        pending_turns: VecDeque::new(),
    };
    let kept = state.frame(json!({
        "type": "form.created",
        "data": {"form": {
            "id": "form-i04",
            "sessionID": "ses_root",
            "title": "隔离提问",
            "fields": [
                {"key":"color","type":"string","title":"颜色","description":"选一个颜色","required":true,
                    "options":[{"value":"red","label":"Red"},{"value":"blue","label":"Blue"}],
                    "schema":{"type":"string","enum":["red","blue"],"minLength":4}},
                {"key":"tags","type":"multiselect","title":"标签","description":"选标签","required":false,
                    "options":[{"value":"a","label":"A"},{"value":"b","label":"B"}],
                    "schema":{"type":"array","minItems":1,"uniqueItems":true}}
            ]
        }}
    })).await;
    assert!(kept);
    let event = rx.recv().await.expect("form.created presents a card");
    let AgentEvent::Origin { turn_id, agent_id, event: inner } = event.clone() else {
        panic!("nested form.sessionID was not routed, got {event:?}");
    };
    assert_eq!(agent_id, None);
    assert_eq!(turn_id, "");
    let AgentEvent::UserInputRequested { request_id, questions, delivery } = *inner else {
        panic!("nested form did not become a question card");
    };
    assert_eq!(request_id, "form-i04");
    assert_eq!(delivery, UserInputDelivery::Blocking);
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0].contract.as_ref().unwrap()["kind"], "single_select");
    assert_eq!(questions[0].contract.as_ref().unwrap()["required"], true);
    assert_eq!(questions[0].contract.as_ref().unwrap()["requested_schema"]["type"], "string");
    assert_eq!(questions[0].contract.as_ref().unwrap()["requested_schema"]["enum"], json!(["red", "blue"]));
    assert_eq!(questions[0].contract.as_ref().unwrap()["requested_schema"]["minLength"], 4);
    assert_eq!(questions[1].contract.as_ref().unwrap()["kind"], "multi_select");
    assert_eq!(questions[1].contract.as_ref().unwrap()["required"], false);
    assert_eq!(questions[1].contract.as_ref().unwrap()["requested_schema"]["type"], "array");
    assert_eq!(questions[1].contract.as_ref().unwrap()["requested_schema"]["minItems"], 1);

    let declared: Vec<String> = crate::agent_runtime::access::ALL_MODES.iter().map(|mode| (*mode).to_owned()).collect();
    let plan = crate::agent_runtime::access::plan(agent::ProviderKind::OpenCode, "opencode.server", true, &declared, "supervised").unwrap();
    let mut translator = crate::agent_runtime::translate::Translator::new(
        crate::agent_runtime::translate::Identity {
            agent_session_id: "ses_root".into(),
            engine: "opencode".into(),
            adapter_id: "opencode.server".into(),
            instance_id: "default".into(),
            transport: "http".into(),
            cwd: "/tmp".into(),
            generation: 1,
            resumed: false,
        },
        plan,
    );
    let mut doc = json!({"thread":{"thread_id":"t","state":{"status":"running","running_turn_id":"turn-1"}},"turns":[{"turn_id":"turn-1","status":"running"}],"messages":[{"message_id":"u","turn_id":"turn-1","role":"user","text":"hi","stream_seq":1}],"event_seq":1});
    let mut events = Vec::new();
    let mut effects = crate::agent_runtime::translate::Effects::default();
    translator.apply(&mut doc, event, &mut |doc, kind, payload| {
        let seq = crate::chat::stream_head(doc) + 1;
        doc["event_seq"] = json!(seq);
        events.push((kind.to_owned(), payload));
    }, &mut effects);
    let stored = doc["thread"]["state"]["pending_user_input"].clone();
    let (_, read_model) = events.iter().find(|(kind, _)| kind == "core.user_input.requested").expect("read model event");
    assert_eq!(stored["questions"], read_model["questions"]);
    assert_eq!(stored["questions"][0]["kind"], "single_select");
    assert_eq!(stored["questions"][0]["required"], true);
    assert_eq!(stored["questions"][0]["requested_schema"]["type"], "string");
    assert_eq!(stored["questions"][0]["requested_schema"]["enum"], json!(["red", "blue"]));
    assert_eq!(stored["questions"][0]["requested_schema"]["minLength"], 4);
    assert_eq!(stored["questions"][1]["kind"], "multi_select");
    assert_eq!(stored["questions"][1]["required"], false);
    assert_eq!(stored["questions"][1]["requested_schema"]["minItems"], 1);
    assert_eq!(
        crate::agent_runtime::translate::assemble_answers(&stored, "submit", &json!({"color":{"values":["red"]}})),
        Err("conversation.user_input.constraints_invalid")
    );
    assert_eq!(doc["thread"]["state"]["pending_user_input"]["request_id"], "form-i04");
    assert_eq!(doc["thread"]["state"]["pending_user_input"]["status"], "pending");
    translator.apply(
        &mut doc,
        AgentEvent::UserInputResolved { request_id: "form-i04".into(), answers: serde_json::Map::new() },
        &mut |doc, kind, payload| {
            let seq = crate::chat::stream_head(doc) + 1;
            doc["event_seq"] = json!(seq);
            events.push((kind.to_owned(), payload));
        },
        &mut effects,
    );
    assert!(doc["thread"]["state"]["pending_user_input"].is_null());
    assert_eq!(events.last().unwrap().0, "core.user_input.resolved");
    assert_eq!(events.last().unwrap().1["cancelled"], true);
}
