//! One persisted event projection feeds automatic and requested progress cards.
use super::*;
use serde::{Serialize,Deserialize};
use std::collections::BTreeMap;

pub struct Service{sender:tokio::sync::mpsc::UnboundedSender<(String,bool)>,receiver:Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<(String,bool)>>>}
impl Default for Service{fn default()->Self{let(sender,receiver)=tokio::sync::mpsc::unbounded_channel();Self{sender,receiver:Mutex::new(Some(receiver))}}}
impl Service{
    pub fn observe(&self,id:&str,event:&Value){
        let kind=event["event_type"].as_str().unwrap_or("");if !matches!(kind,"run.preparing"|"run.started"|"run.reopened"|"run.finished"|"reason.progress"|"reason.intent"|"blackboard.delta"|"hitl.request"|"hitl.resolved"|"stalled"|"worker.error"){return;}
        let urgent=matches!(kind,"run.finished"|"hitl.request"|"stalled")||matches!(event["payload"]["kind"].as_str(),Some("planner_unavailable"|"flag_found"|"finding_found"|"goal_complete"|"all_flags_found"));
        let _=self.sender.send((id.into(),urgent));
    }
}
#[derive(Default,Serialize,Deserialize)]#[serde(default)]
struct Projection{
    scanned_seq:u64,cursor:u64,published_seq:u64,last_published_at:f64,source_hash:String,last_brief:Value,phase:String,mode:String,
    summary:String,summary_source:String,error:String,sections:BTreeMap<String,Vec<Value>>,
    fact_texts:serde_json::Map<String,Value>,confirmed:serde_json::Map<String,Value>,candidates:serde_json::Map<String,Value>,intents:serde_json::Map<String,Value>,blocked:Vec<Value>,urgent:String,
}
fn put_recent(rows:&mut serde_json::Map<String,Value>,key:String,row:Value,limit:usize){rows.shift_remove(&key);rows.insert(key,row);while rows.len()>limit{let Some(key)=rows.keys().next().cloned()else{break};rows.shift_remove(&key);}}
fn item(text:&str,kind:&str,id:impl ToString)->Value{json!({"text":text.chars().take(1600).collect::<String>(),"ref":{"kind":kind,"id":id.to_string()}})}
fn section_rows(value:&Value)->Vec<Value>{value.as_array().into_iter().flatten().filter_map(|row|row.as_str().or_else(||row["text"].as_str())).map(|text|item(text,"summary","")).collect()}
fn phase(raw:&str)->&str{match raw{"race"|"race_scout"=>"racing","reason"|"explore"|"dispatch"=>"running","collect"=>"collecting","complete"=>"finished",other=>other}}
impl Projection{
    fn blocked(&mut self,row:Value){if row["text"].as_str().is_some_and(|s|!s.is_empty())&&!self.blocked.contains(&row){self.blocked.push(row);if self.blocked.len()>16{self.blocked.remove(0);}}}
    fn observe(&mut self,event:&Value){
        let seq=event["seq"].as_u64().unwrap_or(0);if seq<=self.scanned_seq{return;}self.scanned_seq=seq;
        let previous=json!([self.phase,self.mode,self.summary,self.summary_source,self.error,self.sections,self.confirmed,self.candidates,self.intents,self.blocked,self.urgent]);
        let kind=event["event_type"].as_str().unwrap_or("");let p=&event["payload"];
        if kind=="progress.brief"{if let Some(hash)=p["source_hash"].as_str(){self.source_hash=hash.into();self.published_seq=p["source_to_seq"].as_u64().unwrap_or(0);self.last_brief=p.clone();}return;}
        match kind {
            "run.preparing"|"run.started"|"run.reopened"=>{self.phase="running".into();if let Some(mode)=p["challenge"]["mode"].as_str(){self.mode=mode.into();}},
            "reason.intent"=>{for raw in p["intents"].as_array().into_iter().flatten(){let id=raw["id"].as_str().or_else(||raw["intent_id"].as_str()).map(str::to_owned).unwrap_or_else(||seq.to_string());let mut row=item(raw["goal"].as_str().unwrap_or(""),"intent",&id);row["status"]=json!("proposed");put_recent(&mut self.intents,id,row,32);}if p["goal_met"]==true{self.phase="goal_met".into();self.urgent="milestone".into();}},
            "reason.progress"=>{self.summary=p["summary"].as_str().unwrap_or("").trim().into();self.summary_source=if self.summary.is_empty(){""}else{"reason"}.into();self.error=p["summary_error"].as_str().unwrap_or("").into();for key in ["confirmed","active","blocked","next"]{self.sections.insert(key.into(),section_rows(&p["sections"][key]));}},
            "hitl.request"=>{self.phase="paused".into();self.urgent="blocker".into();self.blocked(item(p["need"].as_str().or_else(||p["text"].as_str()).unwrap_or(""),"decision",p["request_id"].as_str().unwrap_or("")));},
            "hitl.resolved"=>{let ids=p["request_ids"].as_array().cloned().unwrap_or_default();self.blocked.retain(|row|row["ref"]["kind"]!="decision"||!ids.contains(&row["ref"]["id"]));if !self.blocked.iter().any(|row|row["ref"]["kind"]=="decision"){self.phase="running".into();}},
            "stalled"|"worker.error"=>{self.blocked(item(p["reason"].as_str().or_else(||p["detail"].as_str()).unwrap_or(kind),"event",seq));if kind=="stalled"{self.urgent="stalled".into();}},
            "blackboard.delta"=>self.blackboard(p,seq),
            "run.finished"=>{
                self.phase=if p["solved"]==true{if self.mode=="pentest"{"goal_met"}else{"solved"}}else if p["reason"]=="runtime_failure"{"failed"}else{"finished"}.into();
                self.urgent="final".into();let flags=p["flags"].as_array().cloned().unwrap_or_else(||p.get("flag").filter(|v|v.is_string()).cloned().into_iter().collect());
                for flag in &flags{if let Some(flag)=flag.as_str(){put_recent(&mut self.confirmed,format!("flag:{flag}"),item(flag,"flag",flag),16);}}
                self.sections.insert("confirmed".into(),self.confirmed.values().rev().take(3).cloned().collect());self.sections.insert("active".into(),Vec::new());self.sections.insert("next".into(),Vec::new());self.sections.insert("blocked".into(),self.blocked.iter().rev().take(3).cloned().collect());
                if self.phase=="failed"{self.summary.clear();self.summary_source.clear();self.error=p["detail"].as_str().unwrap_or("本轮执行失败，尚无可用总结").into();}
                else{self.error.clear();self.summary_source="terminal".into();self.summary=if p["solved"]==true{if self.mode=="pentest"{"已达到任务目标，本轮执行完成。".into()}else{format!("本轮执行完成，已收集 {} 个 Flag。",flags.len())}}else if self.mode=="pentest"{"本轮执行已结束，未达到任务目标。".into()}else{format!("本轮执行已结束，已收集 {} 个 Flag。{}",flags.len(),if p["platform_confirmation_required"]==true{"需比赛平台确认后判定解题。"}else{""})};}
            },_=>{}
        }
        if previous!=json!([self.phase,self.mode,self.summary,self.summary_source,self.error,self.sections,self.confirmed,self.candidates,self.intents,self.blocked,self.urgent]){self.cursor=seq;}
    }

    fn blackboard(&mut self,p:&Value,seq:u64){
        let kind=p["kind"].as_str().unwrap_or("");let intent=p["intent_id"].as_str().unwrap_or("");let fact=p["fact_seq"].as_u64().unwrap_or(seq).to_string();
        match kind {
            "reason_done"=>{let summary=p["progress_summary"].as_str().unwrap_or("").trim();if summary.is_empty(){self.error=p["error"].as_str().or_else(||p["planner_failure_detail"].as_str()).unwrap_or("Reason 未返回可用的进展总结").into();}else{self.summary=summary.into();self.summary_source="reason".into();self.error.clear();for key in ["confirmed","active","blocked","next"]{self.sections.insert(key.into(),section_rows(&p["progress_sections"][key]));}}},
            "phase_transition"=>self.phase=phase(p["to"].as_str().unwrap_or("running")).into(),
            "race_started"|"race_runtime_window_started"=>self.phase="racing".into(),
            "race_concluded"|"operator_resumed"|"operator_thawed"=>self.phase="running".into(),
            "intent_proposed"|"intent_claimed"=>{let status=if kind=="intent_claimed"{"claimed"}else{"proposed"};let mut row=self.intents.get(intent).cloned().unwrap_or_else(||item(p["goal"].as_str().unwrap_or(intent),"intent",intent));row["status"]=json!(status);put_recent(&mut self.intents,intent.into(),row,32);},
            "intent_concluded"|"intent_state_changed"|"intent_reopened"=>{let closed=kind=="intent_concluded"||matches!(p["dispatch_state"].as_str(),Some("closed"|"retired"));if closed||kind=="intent_reopened"{for id in intent.split(',').map(str::trim){if let Some(mut row)=self.intents.get(id).cloned(){row["status"]=json!(if kind=="intent_reopened"{"proposed"}else{"done"});put_recent(&mut self.intents,id.into(),row,32);}}}},
            "fact_added"=>{let row=item(p["fact"].as_str().or_else(||p["summary"].as_str()).unwrap_or(""),"fact",&fact);put_recent(&mut self.fact_texts,fact.clone(),row.clone(),512);if p["verified"]==true{put_recent(&mut self.confirmed,fact,row,16);}else{put_recent(&mut self.candidates,fact,row,32);}},
            "fact_promoted"|"fact_revalidated"=>{let row=self.candidates.shift_remove(&fact).or_else(||self.fact_texts.get(&fact).cloned()).unwrap_or_else(||item(p["fact"].as_str().unwrap_or(""),"fact",&fact));put_recent(&mut self.confirmed,fact,row,16);},
            "fact_rejected"|"fact_challenged"|"fact_superseded"=>{let old=self.candidates.shift_remove(&fact).or_else(||self.confirmed.shift_remove(&fact));let detail=p["reason"].as_str().or_else(||old.as_ref().and_then(|row|row["text"].as_str())).unwrap_or("Fact requires review");self.blocked(item(detail,"fact",fact));},
            "dead_end"=>self.blocked(item(p["reason"].as_str().unwrap_or(""),"dead_end",seq)),
            "awaiting_operator"|"need_input"|"operator_paused"=>{self.phase="paused".into();self.urgent="blocker".into();self.blocked(item(p["reason"].as_str().or_else(||p["need"].as_str()).unwrap_or(""),"event",seq));},
            "planner_unavailable"=>{self.phase="failed".into();self.error=p["detail"].as_str().unwrap_or("Planner unavailable").into();self.urgent="blocker".into();self.blocked(item(&self.error,"event",seq));},
            "flag_found"=>{if let Some(flag)=p["flag"].as_str(){put_recent(&mut self.confirmed,format!("flag:{flag}"),item(flag,"flag",flag),16);}self.phase="collecting".into();self.urgent="milestone".into();},
            "finding_found"=>{put_recent(&mut self.confirmed,format!("finding:{seq}"),item(p["summary"].as_str().or_else(||p["finding_class"].as_str()).unwrap_or(""),"finding",seq),16);self.urgent="milestone".into();},
            "goal_complete"=>{self.phase="goal_met".into();self.urgent="milestone".into();},"all_flags_found"=>{self.phase="solved".into();self.urgent="milestone".into();},_=>{}
        }
    }
}
pub(super) fn publish(state:&AppState,id:&str,requested:bool)->Result<Value,String>{
    let handle=state.runs.handle(state,id);let _guard=handle.progress_lock.lock().unwrap();
    let run=state.store.get("runs",id).map_err(|e|e.to_string())?.ok_or("Run not found")?;
    let old=state.store.get("run_progress",id).map_err(|e|e.to_string())?;
    let mut projection:Projection=old.clone().map(serde_json::from_value).transpose().map_err(|e|e.to_string())?.unwrap_or_default();
    if projection.mode.is_empty(){projection.mode=run["mode"].as_str().unwrap_or("ctf").into();}if projection.phase.is_empty(){projection.phase="running".into();}
    for event in persisted_event_iter(state,id)?{projection.observe(&event);}
    for key in ["confirmed","active","blocked","next"]{projection.sections.entry(key.into()).or_default();}
    let terminal=matches!(projection.phase.as_str(),"solved"|"goal_met"|"finished"|"failed");
    let kind=if !projection.urgent.is_empty(){projection.urgent.as_str()}else if requested{"requested"}else{"periodic"};
    let hashed=if kind=="requested"{projection.last_brief["kind"].as_str().filter(|value|!value.is_empty()).unwrap_or("periodic")}else{kind};
    let status=if !projection.error.is_empty(){"failed"}else if projection.summary.is_empty(){"pending"}else{"ready"};
    let material=json!({"kind":hashed,"summary":projection.summary,"summary_source":projection.summary_source,"summary_status":status,"summary_error":projection.error,"sections":projection.sections});
    use sha2::Digest;let hash=format!("{:x}",sha2::Sha256::digest(material.to_string().as_bytes()));
    let mut output=projection.last_brief.clone();
    if hash!=projection.source_hash && (requested||status!="pending"||matches!(kind,"final"|"blocker"|"stalled")){
        let source_to=projection.cursor;let mut source_from=projection.published_seq.saturating_add(1);
        if source_from>source_to{
            match (projection.last_brief["source_from_seq"].as_u64(),projection.last_brief["source_to_seq"].as_u64()){
                (Some(from),Some(to)) if from<=to => source_from=from,
                _ => source_from=source_to,
            }
        }
        output=material;output["brief_id"]=json!(format!("pb-{}-{}-{}",run["execution_generation"].as_u64().unwrap_or(0),projection.cursor,&hash[..10]));output["phase"]=json!(projection.phase);output["mode"]=json!(projection.mode);output["trigger"]=json!(if requested{"operator"}else{"automatic"});output["source_hash"]=json!(hash);output["source_from_seq"]=json!(source_from);output["source_to_seq"]=json!(source_to);
        output["counts"]=json!({"confirmed":projection.confirmed.len(),"active":if terminal{0}else{projection.intents.values().filter(|row|row["status"]=="claimed").count()+projection.candidates.len()},"blocked":projection.blocked.len(),"next":if terminal{0}else{projection.intents.values().filter(|row|row["status"]=="proposed").count()}});
        let event=emit_event(state,id,Some("coordinator"),"progress.brief",output.clone());if event["persisted"]==false{return Err("Progress event persistence failed".into());}
        projection.source_hash=hash;projection.published_seq=projection.cursor;projection.last_published_at=now_secs();projection.last_brief=output.clone();
    }
    if let (Some(from),Some(to))=(output["source_from_seq"].as_u64(),output["source_to_seq"].as_u64()) {
        if from>to { output["source_from_seq"]=json!(to); projection.last_brief["source_from_seq"]=json!(to); }
    }
    projection.urgent.clear();let saved=serde_json::to_value(projection).map_err(|e|e.to_string())?;
    if !state.store.put_many_if(&[("run_progress",id,old.as_ref())],&[("run_progress",id,&saved)]).map_err(|e|e.to_string())?{return Err("Progress projection changed concurrently".into());}Ok(output)
}
pub fn spawn(state:Arc<AppState>){let Some(mut receiver)=state.run_progress.receiver.lock().unwrap().take()else{return};tokio::spawn(async move{
    let mut due=HashMap::<String,tokio::time::Instant>::new();
    let mut dirty=HashMap::<String,tokio::time::Instant>::new();
    loop{let next=due.values().copied().min();tokio::select!{
        message=receiver.recv()=>{let Some((id,urgent))=message else{break};let now=tokio::time::Instant::now();let first=*dirty.entry(id.clone()).or_insert(now);let last=state.store.get("run_progress",&id).ok().flatten().and_then(|row|row["last_published_at"].as_f64()).unwrap_or(0.);let interval=Duration::from_secs_f64((last+45.-now_secs()).clamp(0.,45.));let at=if urgent{now}else{(now+Duration::from_secs(12)).max(now+interval).min(first+Duration::from_secs(120))};due.entry(id).and_modify(|old|*old=(*old).min(at)).or_insert(at);},
        _=async{if let Some(at)=next{tokio::time::sleep_until(at).await}else{std::future::pending().await}}=>{let now=tokio::time::Instant::now();let ids=due.iter().filter(|(_,at)|**at<=now).map(|(id,_)|id.clone()).collect::<Vec<_>>();for id in ids{due.remove(&id);dirty.remove(&id);let s=state.clone();let result=tokio::task::spawn_blocking(move||publish(&s,&id,false)).await;if let Ok(Err(error))=result{tracing::error!(%error,"Progress publication failed");}}}
    }}
});}
