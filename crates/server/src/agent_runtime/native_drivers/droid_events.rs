use super::*;

pub(super) const STEER_MESSAGE_PREFIX: &str = "muteki-steer-";
#[derive(Default)]pub(super) struct Projection {tools:HashMap<String,(String,Value,String,bool)>,origins:HashMap<String,String>,pub(super) latest_usage:Option<Value>,pub(super) baseline:Option<Value>}
impl Projection {
    fn usage_event(&self,raw:&Value,turn:&str,complete:bool)->AgentEvent{
        let consistent=self.baseline.as_ref().is_some_and(|before|["inputTokens","outputTokens"].iter().all(|key|match(raw[*key].as_u64(),before[*key].as_u64()){(Some(current),Some(previous))=>current>=previous,_=>false}));
        let mut usage=json!({"usage_complete":complete&&consistent,"input_includes_cache":false});
        for (source,target)in [("inputTokens","input_tokens"),("outputTokens","output_tokens"),("cacheReadTokens","cache_read_tokens"),("cacheCreationTokens","cache_write_tokens"),("thinkingTokens","reasoning_tokens")]{
            if let(Some(current),Some(before))=(raw[source].as_u64(),self.baseline.as_ref().and_then(|value|value[source].as_u64())){usage[target]=json!(current.saturating_sub(before));}
        }
        AgentEvent::UsageDetail{scope:"turn_snapshot".into(),usage_id:turn.into(),usage}
    }
    pub(super) fn settle_incomplete(&mut self,turn:&str)->AgentEvent {
        let event=self.latest_usage.as_ref().map(|usage|self.usage_event(usage,turn,false))
            .unwrap_or_else(||AgentEvent::UsageDetail{scope:"turn_snapshot".into(),usage_id:turn.into(),usage:json!({"usage_complete":false})});
        self.baseline=self.latest_usage.clone();
        event
    }
    pub(super) fn apply(&mut self,payload:&Value,turn:&str)->Vec<AgentEvent>{
        let kind=payload["type"].as_str().unwrap_or("");let mut events=Vec::new();
        match kind {
            "tool_call"|"create_message"=>{
                let message = &payload["message"];
                if kind == "create_message" && message["role"] == "user"
                    && let Some(request_id) = message["id"].as_str()
                        .and_then(|id| id.strip_prefix(STEER_MESSAGE_PREFIX))
                {
                    events.push(AgentEvent::SteerAccepted { request_id: request_id.into() });
                }
                let calls=if kind=="tool_call"{vec![&payload["toolUse"]]}else{message["content"].as_array().into_iter().flatten().filter(|b|b["type"]=="tool_use").collect()};
                for call in calls {
                    let Some(id)=call["id"].as_str()else{continue};if self.tools.contains_key(id){continue;}
                    let name=call["name"].as_str().unwrap_or("tool").to_owned();let input=call["input"].clone();
                    if !turn.is_empty(){self.origins.insert(id.into(),turn.into());}
                    self.tools.insert(id.into(),(name.clone(),input.clone(),String::new(),false));
                    events.push(AgentEvent::ItemStarted(ThreadItem{id:id.into(),parent_item_id:None,content:ItemContent::ToolCall{presentation: None,image_reads:Vec::new(),name,input,output:None,status:ItemStatus::InProgress}}));
                }
            }
            "queued_messages_discarded" => {
                for id in payload["drainedRequestIds"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    if let Some(request_id) = id.strip_prefix(STEER_MESSAGE_PREFIX) {
                        events.push(AgentEvent::SteerRejected { request_id: request_id.into() });
                    }
                }
            }
            "tool_progress_update"|"tool_result"=>{
                let Some(id)=payload["toolUseId"].as_str()else{return events};
                let entry=self.tools.entry(id.into()).or_insert_with(||(payload["toolName"].as_str().unwrap_or("tool").into(),json!({}),String::new(),false));
                if entry.3{return events;}
                let completed=kind=="tool_result";
                let output=if completed{payload["content"].as_str().map(str::to_owned).unwrap_or_else(||payload["content"].to_string())}else{
                    let update=&payload["update"];update["text"].as_str().or_else(||update["status"].as_str()).or_else(||update["details"].as_str()).unwrap_or("").to_owned()
                };
                if completed{entry.2=output;}else{entry.2.push_str(&output);}entry.3=completed;
                let item=ThreadItem{id:id.into(),parent_item_id:None,content:ItemContent::ToolCall{presentation: None,image_reads:Vec::new(),name:entry.0.clone(),input:entry.1.clone(),output:Some(entry.2.clone()),status:if completed{if payload["isError"]==true{ItemStatus::Failed}else{ItemStatus::Completed}}else{ItemStatus::InProgress}}};
                events.push(if completed{AgentEvent::ItemCompleted(item)}else{AgentEvent::ItemUpdated(item)});
            }
            "settings_updated"=>{if let Some(model)=payload["settings"]["modelId"].as_str(){events.push(AgentEvent::ServedModel{model:model.into(),reason:None});}},
            "droid_working_state_changed"=>{if payload["newState"]=="compacting_conversation"{events.push(AgentEvent::ContextCompacted(Compaction{in_progress:true,trigger:Some("auto".into()),..Default::default()}));}},
            "session_token_usage_changed"=>{
                let usage=payload.get("inclusiveTokenUsage").or_else(||payload.get("tokenUsage")).cloned().unwrap_or(Value::Null);self.latest_usage=Some(usage.clone());
                if turn.is_empty(){if usage.is_object(){self.baseline=Some(usage);}return events;}
                events.push(self.usage_event(&usage,turn,false));
            }
            "agent_turn_completed"=>{
                if let Some(usage)=payload.get("cumulativeTokenUsage").filter(|value|value.is_object()){events.push(self.usage_event(usage,turn,true));self.latest_usage=Some(usage.clone());self.baseline=Some(usage.clone());}
                else{events.push(AgentEvent::UsageDetail{scope:"turn_snapshot".into(),usage_id:turn.into(),usage:json!({"usage_complete":false})});self.baseline=self.latest_usage.clone();}
            }
            "permission_resolved"=>{if let Some(id)=payload["requestId"].as_str(){events.push(AgentEvent::ApprovalResolved{request_id:id.into(),decision:if payload["selectedOption"]=="proceed_once"{ApprovalDecision::Option("proceed_once".into())}else{ApprovalDecision::Cancel}});}},
            _=>{}
        }
        events.into_iter().map(|event|{
            let id=match &event{AgentEvent::ItemStarted(item)|AgentEvent::ItemUpdated(item)|AgentEvent::ItemCompleted(item)=>Some(item.id.as_str()),_=>None};
            if let Some(origin)=id.and_then(|id|self.origins.get(id)){AgentEvent::Origin{turn_id:origin.clone(),agent_id:None,event:Box::new(event)}}else{event}
        }).collect()
    }
}

pub(super) fn permission(payload:&Value)->(ApprovalKind,Value){
    let mut fields=json!({"approval_kind":"tool","title":"Droid permission","paths":[],"files":[],"unified_diff":null,"command":null});
    let mut commands=Vec::new();let mut paths=Vec::new();let mut files=Vec::new();let mut diffs=Vec::new();
    for row in payload["toolUses"].as_array().into_iter().flatten(){let details=&row["details"];let kind=details["type"].as_str().or_else(||row["confirmationType"].as_str()).unwrap_or("");
        match kind{
            "exec"=>{fields["approval_kind"]=json!("command_execution");fields["title"]=json!("Run command");if let Some(command)=details["fullCommand"].as_str().or_else(||details["command"].as_str()){commands.push(command.to_owned());}},
            "edit"|"create"|"apply_patch"=>{if commands.is_empty(){fields["approval_kind"]=json!("file_change");fields["title"]=json!(match kind{"create"=>"Create file","apply_patch"=>"Apply patch",_=>"Edit file"});}
                let rows=details["files"].as_array().cloned().unwrap_or_default();
                if rows.is_empty(){if let Some(path)=details["filePath"].as_str(){paths.push(path.to_owned());files.push(json!({"path":path,"change":if kind=="create"{"add"}else{"modify"}}));}}else{for row in rows{if let Some(path)=row["filePath"].as_str().or_else(||row["file_path"].as_str()){paths.push(path.to_owned());files.push(json!({"path":path,"change":match row["operation"].as_str(){Some("create")=>"add",Some("delete")=>"delete",_=>"modify"}}));}}}
                if let Some(diff)=details["patchContent"].as_str().or_else(||details["diff"].as_str()){diffs.push(diff.to_owned());}},
            "exit_spec_mode"=>{fields["approval_kind"]=json!("plan_exit");fields["title"]=json!(details["title"].as_str().unwrap_or("Exit spec mode"));fields["plan_markdown"]=details["plan"].clone();},
            "mcp_tool"=>{fields["approval_kind"]=json!("mcp_tool_call");fields["title"]=json!(details["toolName"].as_str().unwrap_or("MCP tool"));},_=>{}
        }
    }
    let tool=&payload["toolUses"][0]["toolUse"];fields["tool_call_id"]=tool["id"].clone();fields["tool_name"]=tool["name"].clone();if matches!(fields["approval_kind"].as_str(),Some("tool"|"mcp_tool_call")){fields["input"]=tool["input"].clone();}
    if !commands.is_empty(){fields["approval_kind"]=json!("command_execution");fields["title"]=json!("Run command");fields["command"]=json!(commands.join("\n"));}if !diffs.is_empty(){fields["unified_diff"]=json!(diffs.join("\n"));}paths.sort();paths.dedup();fields["paths"]=json!(paths);fields["files"]=json!(files);
    let kind=match fields["approval_kind"].as_str(){Some("command_execution")=>ApprovalKind::ExecCommand{command:fields["command"].as_str().unwrap_or("").into(),cwd:None,reason:None},Some("file_change")=>ApprovalKind::FileChange{changes:files.iter().filter_map(|file|Some(agent::FileChange{path:file["path"].as_str()?.into(),kind:match file["change"].as_str(){Some("add")=>agent::FileChangeKind::Create,Some("delete")=>agent::FileChangeKind::Delete,_=>agent::FileChangeKind::Modify},diff:fields["unified_diff"].as_str().map(str::to_owned)})).collect(),reason:None},_=>ApprovalKind::ToolUse{name:fields["title"].as_str().unwrap_or("Droid permission").into(),input:fields.clone(),detail:fields["title"].as_str().unwrap_or("").into()}};(kind,fields)
}
