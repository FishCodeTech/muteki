//! Invocation/turn identities settle numerical observations without fixed prices.
use super::*;
use agent::AgentEvent;
fn number(value:&Value)->Option<f64>{value.as_f64().filter(|value|value.is_finite() && *value>=0.)}
const COUNTERS:[&str;5]=["input_tokens","output_tokens","cache_read_tokens","cache_write_tokens","reasoning_tokens"];
fn auxiliary_role(role:&str)->bool{matches!(role,"btw"|"respond"|"standby"|"ask"|"writeup"|"followup"|"report"|"report_review"|"titler")}
fn main_invocation(row:&Value)->bool{!row["actor_kind"].as_str().is_some_and(|kind|kind=="auxiliary")&&!row["role"].as_str().is_some_and(auxiliary_role)}
fn quote(state:&AppState,spec:&worker::WorkerSpec)->Value{
    if let Some(snapshot)=spec.profile.get("admitted_price_snapshot"){return snapshot.clone();}
    let mut identity=json!({"engine":spec.norm_engine,"model":spec.model.as_deref().unwrap_or("default"),"credential_id":spec.effective_credential_id});
    for field in ["provider_id","speed"]{if let Some(value)=spec.profile.get(field).filter(|v|v.as_str().is_some_and(|v|!v.is_empty())){identity[field]=value.clone();}}
    crate::pricing_read::resolve(&state.store,&[identity]).ok().map(|rows|rows["items"][0].clone()).unwrap_or(Value::Null)
}
pub(super) fn freeze_price(state:&AppState,spec:&mut worker::WorkerSpec){let snapshot=quote(state,spec);if !spec.profile.is_object(){spec.profile=json!({});}spec.profile["admitted_price_snapshot"]=snapshot;}
fn estimated(usage:&Value,rates:&Value,engine:&str)->Option<f64>{
    let input=number(&usage["input_tokens"])?;let output=number(&usage["output_tokens"])?;let read=number(&usage["cache_read_tokens"]).unwrap_or(0.);let write=number(&usage["cache_write_tokens"]).unwrap_or(0.);
    let regular=if matches!(engine,"claude"|"pi"|"omp"|"opencode"|"droid") && usage["input_includes_cache"]!=true{input}else{(input-read-write).max(0.)};
    let mut total=0.;for(field,tokens)in [("input",regular),("output",output),("cache_read",read),("cache_write",write)]{if tokens>0.{total+=tokens*number(&rates[field])?/1e6;}}
    total.is_finite().then_some(total)
}
fn totals(record:&Value)->Value{
    let slots=record["observations"].as_object();let mut result=json!({});let mut cost=0.;let mut known_cost=false;let mut unknown_cost=false;
    let rows=slots.into_iter().flatten().filter(|(_,row)|{
        if row["scope"]=="session_cumulative"{return false;}
        if row["scope"]=="turn_snapshot"{return !slots.into_iter().flatten().any(|(_,other)|other["turn_id"]==row["turn_id"] && other["usage_epoch"]==row["usage_epoch"] && other["scope"]=="message");}true
    }).map(|(_,row)|row).collect::<Vec<_>>();
    let counter = |field:&str,exclusive_cache:bool| -> Option<f64> {
        let mut epochs=HashMap::<String,f64>::new();
        let mut turns=HashMap::<(String,String),(f64,f64)>::new();
        for(_,row)in slots.into_iter().flatten(){
            if exclusive_cache && row["usage"]["input_includes_cache"]==true{continue;}
            let Some(value)=number(&row["usage"][field])else{continue};
            let epoch=row["usage_epoch"].as_str().unwrap_or("session").to_owned();
            if row["scope"]=="session_cumulative"{epochs.entry(epoch).and_modify(|old|*old=old.max(value)).or_insert(value);}
            else{
                let entry=turns.entry((epoch,row["turn_id"].as_str().unwrap_or("").into())).or_default();
                if row["scope"]=="message"{entry.1+=value;}else{entry.0=entry.0.max(value);}
            }
        }
        let values=epochs.values().copied().chain(turns.iter().filter(|((epoch,_),_)|!epochs.contains_key(epoch)).map(|(_,value)|value.0.max(value.1))).collect::<Vec<_>>();
        (!values.is_empty()).then(||values.iter().sum())
    };
    for field in COUNTERS{result[field]=counter(field,false).map_or(Value::Null,|value|json!(value));}
    let mut invoices=HashMap::<String,f64>::new();
    for (_,row) in slots.into_iter().flatten().filter(|(_,row)|row["scope"]=="session_cumulative"){
        if let Some(amount)=number(&row["amount"]){let epoch=row["usage_epoch"].as_str().unwrap_or("session").to_owned();invoices.entry(epoch).and_modify(|old|*old=old.max(amount)).or_insert(amount);}
    }
    for row in &rows{let epoch=row["usage_epoch"].as_str().unwrap_or("session");if invoices.contains_key(epoch){continue}if row["usage_complete"]==false{unknown_cost=true;}if let Some(value)=number(&row["amount"]){cost+=value;known_cost=true;}else{unknown_cost=true;}}
    for amount in invoices.values(){cost+=amount;known_cost=true;}
    if matches!(record["engine"].as_str(),Some("claude"|"pi"|"omp"|"opencode"|"droid")){
        let extra=counter("cache_read_tokens",true).unwrap_or(0.)+counter("cache_write_tokens",true).unwrap_or(0.);
        if let Some(input)=number(&result["input_tokens"]){result["input_tokens"]=json!(input+extra);}
    }
    result["usd"]=if known_cost{json!(cost)}else{Value::Null};result["cost_status"]=json!(if unknown_cost{"partial"}else if known_cost{"known"}else{"unknown"});result
}
pub(super) fn record(state:&AppState,run_id:&str,invocation:&str,spec:&worker::WorkerSpec,role:&str,turn:&str,scope:&str,identity:&str,observed:&Value)->Result<Value,String>{
    let handle=state.runs.handle(state,run_id);let _guard=handle.usage_lock.lock().unwrap();
    let key=format!("{run_id}:{invocation}");
    for _ in 0..16{
        let before=state.store.get("run_usage_invocations",&key).map_err(|e|e.to_string())?;
        let run=state.store.get("runs",run_id).map_err(|error|error.to_string())?.unwrap_or(Value::Null);
        let generation=run["execution_generation"].as_u64().unwrap_or(1);
        let mut record=before.clone().unwrap_or_else(||json!({"id":key,"run_id":run_id,"generation":generation,"invocation_id":invocation,"worker_id":spec.solver_id,"engine":spec.norm_engine,"model":spec.model,"credential_id":spec.effective_credential_id,"role":role,"price_snapshot":quote(state,spec),"observations":{},"created_at":now_secs()}));
        record["actor_kind"]=json!(if auxiliary_role(role){"auxiliary"}else{"solve"});
        record["billing_scope"]=record["actor_kind"].clone();
        if record["generation"].is_null(){record["generation"]=json!(generation);}
        if before.is_none(){
            record["workspace_kind"]=run["mode"].clone();
            if let Some(binding)=state.store.list("competition_bindings_live").map_err(|error|error.to_string())?.into_iter().find(|binding|binding["run_id"]==run_id){
                record["competition_id"]=binding["competition_id"].clone();record["challenge_id"]=binding["competition_challenge_id"].clone();
            }
        }
        let id=json!([observed["usage_epoch"].as_str().unwrap_or("session"),scope,identity]).to_string();let mut usage=record["observations"][&id]["usage"].as_object().cloned().unwrap_or_default();
        for field in COUNTERS {let value=observed.get(field).or_else(||if field=="cache_read_tokens"{observed.get("cached_input_tokens")}else{None});if let Some(value)=value.filter(|v|number(v).is_some()){let prior=usage.get(field).and_then(number).unwrap_or(0.);usage.insert(field.into(),json!(prior.max(number(value).unwrap())));}}
        if let Some(value)=observed.get("input_includes_cache"){usage.insert("input_includes_cache".into(),value.clone());}
        for field in ["reported_cost","cost_usd"]{if let Some(amount)=number(&observed[field]){let prior=usage.get("reported_cost").and_then(number).unwrap_or(0.);usage.insert("reported_cost".into(),json!(prior.max(amount)));}}
        let usage=json!(usage);let native=if observed["cost_kind"]=="estimated"{None}else{number(&usage["reported_cost"])};
        let supplied_estimate=if observed["cost_kind"]=="estimated"{number(&usage["reported_cost"]).filter(|amount|*amount>0.)}else{None};
        let configured_estimate=estimated(&usage,&record["price_snapshot"]["rates"],&spec.norm_engine);
        let estimate=configured_estimate.or(supplied_estimate);
        record["observations"][&id]=json!({"scope":scope,"usage_id":identity,"turn_id":turn,"usage_epoch":observed["usage_epoch"].as_str().unwrap_or("session"),"usage_complete":observed.get("usage_complete").cloned().unwrap_or_else(||record["observations"][&id].get("usage_complete").cloned().unwrap_or(json!(true))),"usage":usage,"amount":native.or(estimate),"cost_source":if native.is_some(){"native_charge"}else if configured_estimate.is_some(){"configured_price"}else if supplied_estimate.is_some(){"provider_estimate"}else{"unknown"},"updated_at":now_secs()});
        let total=totals(&record);record["totals"]=total.clone();record["updated_at"]=json!(now_secs());
        let mut history=json!({"id":key,"run_id":run_id,"worker_id":spec.solver_id,"invocation_id":invocation,"source":"run_native","engine":spec.norm_engine,"model":spec.model,"phase":role,"credential_id":spec.effective_credential_id,"reported_cost":if record["observations"].as_object().is_some_and(|rows|rows.values().any(|row|row["cost_source"]=="native_charge")){total["usd"].clone()}else{Value::Null},"estimated_cost":total["usd"],"cost_status":total["cost_status"],"price_source":record["price_snapshot"],"recorded_at":chrono::Utc::now().to_rfc3339()});
        history["source"]=json!("runtime");history["generation"]=record["generation"].clone();history["at"]=json!(now_secs());history["role"]=json!(role);history["measurement_scope"]=json!("invocation_cumulative");history["token_schema_version"]=json!(3);
        history["actor_kind"]=record["actor_kind"].clone();history["billing_scope"]=record["billing_scope"].clone();for field in ["workspace_kind","competition_id","challenge_id"]{history[field]=record[field].clone();}
        history["price_snapshot"]=record["price_snapshot"].clone();history["price_source"]=json!(if native.is_some(){"provider_reported"}else if configured_estimate.is_some(){"model_price"}else if estimate.is_some(){"driver_estimate"}else{"unknown"});
        for field in COUNTERS{history[field]=total[field].clone();}
        if state.store.put_many_if(&[("run_usage_invocations",&key,before.as_ref())],&[("run_usage_invocations",&key,&record),("usage_history",&key,&history)]).map_err(|e|e.to_string())?{

            let all=state.store.list("run_usage_invocations").map_err(|e|e.to_string())?.into_iter().filter(|row|row["run_id"]==run_id&&main_invocation(row)).collect::<Vec<_>>();
            let known=all.iter().filter_map(|row|number(&row["totals"]["usd"])).collect::<Vec<_>>();let amount=(!known.is_empty()).then(||known.iter().sum::<f64>());
            let partial=all.iter().any(|row|row["totals"]["cost_status"]!="known");let mut aggregate=json!({"usd":amount,"cost_status":if partial{"partial"}else{"known"},"invocations":all.len()});
            for field in COUNTERS{let values=all.iter().filter_map(|row|number(&row["totals"][field])).collect::<Vec<_>>();aggregate[field]=if values.is_empty(){Value::Null}else{json!(values.iter().sum::<f64>())};}
            let handle=state.runs.handle(state,run_id);if let Some(amount)=amount{handle.cost_usage.lock().unwrap().total_usd=amount;}
            let mut payload=total;payload["execution_generation"]=record["generation"].clone();payload["invocation_id"]=json!(invocation);payload["measurement_scope"]=json!("invocation_cumulative");payload["run_total"]=aggregate;payload["accounting_source"]=json!("run_usage_ledger");payload["scope"]=json!(if auxiliary_role(role){"auxiliary"}else{"solve"});
            let auxiliary=state.store.list("run_usage_invocations").map_err(|e|e.to_string())?.into_iter().filter(|row|row["run_id"]==run_id&&!main_invocation(row)).collect::<Vec<_>>();
            let mut aux=json!({"usd":auxiliary.iter().filter_map(|r|number(&r["totals"]["usd"])).sum::<f64>(),"invocations":auxiliary.len(),"cost_status":if auxiliary.iter().all(|r|r["totals"]["cost_status"]=="known"){"known"}else{"partial"}});
            for field in COUNTERS{aux[field]=json!(auxiliary.iter().filter_map(|r|number(&r["totals"][field])).sum::<f64>());}
            payload["auxiliary_total"]=aux;
            if role != "btw" {
                emit_event(state,run_id,Some(&spec.solver_id),"cost.update",payload.clone());
            }
            return Ok(payload);
        }
    }Err("Usage ledger changed repeatedly".into())
}
pub(super) struct Observer{invocation:String,turn:String,turn_index:u64,role:String,ended:bool,container_epoch:u64,message_index:u64,native_session:Option<String>,native_epoch:u64}
impl Observer{
    pub fn new(invocation:&str,role:&str)->Self{Self{invocation:invocation.into(),turn:"turn-1".into(),turn_index:1,role:role.into(),ended:false,container_epoch:0,message_index:0,native_session:None,native_epoch:0}}
    pub fn checkpoint(&mut self){self.ended=true;self.container_epoch+=1;}
    fn native_record(&self,state:&AppState,run_id:&str,spec:&worker::WorkerSpec,scope:&str,identity:&str,usage:&Value)->Result<(),String>{
        let mut usage=usage.clone();usage["usage_epoch"]=json!(format!("native-{}",self.native_epoch));
        record(state,run_id,&self.invocation,spec,&self.role,&self.turn,scope,identity,&usage).map(|_|())
    }
    pub fn observe(&mut self,state:&AppState,run_id:&str,spec:&worker::WorkerSpec,event:&AgentEvent)->Result<(),String>{
        match event{
            AgentEvent::Origin{agent_id:None,turn_id,event,..}=>{let previous=std::mem::replace(&mut self.turn,turn_id.clone());let result=self.observe(state,run_id,spec,event);self.turn=previous;result},
            AgentEvent::SessionStarted{provider_session_id,..}=>{
                if self.native_session.as_ref().is_some_and(|id|id!=provider_session_id){self.native_epoch+=1;}
                self.native_session=Some(provider_session_id.clone());Ok(())
            },
            AgentEvent::TurnStarted{turn_id}=>{self.turn=turn_id.clone();self.ended=false;self.native_record(state,run_id,spec,"turn_snapshot",&self.turn,&json!({}))},
            AgentEvent::TokenUsage(_) if spec.norm_engine=="droid"=>Ok(()),
            AgentEvent::TurnCompleted{usage:Some(_),..} if spec.norm_engine=="droid"=>Ok(()),
            AgentEvent::TokenUsage(value)=>{let mut usage=crate::agent_runtime::translate::usage_json(value);if spec.norm_engine=="claude"{usage["reported_cost"]=Value::Null;}self.native_record(state,run_id,spec,"turn_snapshot",&self.turn,&usage)},
            AgentEvent::UsageDetail{scope,..} if scope=="context_window"=>Ok(()),
            AgentEvent::UsageDetail{usage,scope,usage_id}=>self.native_record(state,run_id,spec,if scope=="session_cumulative"{"session_cumulative"}else if scope=="message"{"message"}else{"turn_snapshot"},if usage_id.is_empty(){&self.turn}else{usage_id},usage),
            AgentEvent::TurnCompleted{usage:None,..}=>self.native_record(state,run_id,spec,"turn_snapshot",&self.turn,&json!({})),
            AgentEvent::TurnCompleted{usage:Some(value),..}=>{let mut usage=crate::agent_runtime::translate::usage_json(value);if spec.norm_engine=="claude"{usage["reported_cost"]=Value::Null;}self.native_record(state,run_id,spec,"turn_snapshot",&self.turn,&usage)},_=>Ok(())
        }
    }
    pub fn container(&mut self,state:&AppState,run_id:&str,spec:&worker::WorkerSpec,line:&str)->Result<(),String>{
        let Ok(raw)=serde_json::from_str::<Value>(line)else{return Ok(())};
        let ty=raw["type"].as_str().unwrap_or("");
        let epoch=format!("cli-{}",self.container_epoch);
        if ty=="muteki.native_usage"{return record(state,run_id,&self.invocation,spec,&self.role,&self.turn,"message",&raw["usage_id"].to_string(),&{let mut usage=raw["usage"].clone();usage["usage_epoch"]=json!(epoch);usage}).map(|_|());}
        if matches!(ty,"turn.started"|"agent_start"){
            if self.ended{self.turn_index+=1;}
            self.turn=raw["turn_id"].as_str().map(str::to_owned).unwrap_or_else(||format!("turn-{}",self.turn_index));self.ended=false;self.message_index=0;
            record(state,run_id,&self.invocation,spec,&self.role,&self.turn,"turn_snapshot",&self.turn,&json!({"usage_epoch":epoch}))?;
        }
        let mut messages=Vec::new();
        if matches!(ty,"assistant"|"message"|"message_end"){let message=raw.get("message").unwrap_or(&raw);if message["role"].as_str().is_none_or(|role|role=="assistant"){messages.push(message);}}
        if ty=="agent_end"{messages.extend(raw["messages"].as_array().into_iter().flatten().filter(|message|message["role"]=="assistant"));}
        for (message_position,message) in messages.into_iter().enumerate(){
            if ty!="agent_end"{self.message_index+=1;}
            if let Some(usage)=message.get("usage").filter(|value|value.is_object()){
                let id=message["id"].as_str().or_else(||message["responseId"].as_str()).map(str::to_owned).or_else(||message.get("timestamp").filter(|v|!v.is_null()).map(Value::to_string)).unwrap_or_else(||format!("{}:message-{}",self.turn,if ty=="agent_end"{message_position as u64+1}else{self.message_index}));
                let mut observed=normalize_usage(usage,&spec.norm_engine);observed["usage_epoch"]=json!(epoch);
                record(state,run_id,&self.invocation,spec,&self.role,&self.turn,"message",&id,&observed)?;
            }
        }
        if matches!(ty,"turn.completed"|"result"){
            let mut observed=normalize_usage(&raw["usage"],&spec.norm_engine);
            observed["usage_epoch"]=json!(epoch);
            record(state,run_id,&self.invocation,spec,&self.role,&self.turn,"turn_snapshot",&self.turn,&observed)?;
            if let Some(cost)=raw.get("total_cost_usd").or_else(||raw.get("cost_usd")).filter(|value|number(value).is_some()){
                let invoice=json!({"reported_cost":cost,"cost_kind":if spec.norm_engine=="claude"{"estimated"}else{"reported"},"usage_epoch":epoch});
                record(state,run_id,&self.invocation,spec,&self.role,&self.turn,"session_cumulative",&format!("{epoch}:cost"),&invoice)?;
            }
        }
        if matches!(ty,"turn.completed"|"agent_end"|"result"){self.ended=true;}

        Ok(())
    }
}

fn normalize_usage(raw:&Value,engine:&str)->Value{
    fn pick(raw:&Value,keys:&[&str])->Value{keys.iter().find_map(|key|raw.get(*key).filter(|value|value.is_number()).cloned()).unwrap_or(Value::Null)}
    json!({"input_tokens":pick(raw,&["input_tokens","input","inputTokens","prompt_tokens"]),"output_tokens":pick(raw,&["output_tokens","output","outputTokens","completion_tokens"]),"cache_read_tokens":pick(raw,&["cached_input_tokens","cache_read_input_tokens","cacheRead","cacheReadTokens"]),"cache_write_tokens":pick(raw,&["cache_creation_input_tokens","cacheWrite","cacheCreationTokens"]),"reasoning_tokens":pick(raw,&["reasoning_tokens","thinkingTokens"]),"reported_cost":raw["cost"].get("total").or_else(||raw.get("cost_usd")).or_else(||raw.get("total_cost_usd")),"input_includes_cache":!matches!(engine,"claude"|"pi"|"omp"|"opencode"|"droid"),"cost_kind":if matches!(engine,"claude"|"pi"|"omp"){"estimated"}else{"reported"}})
}

pub(super) fn cost_coverage(state:&AppState,run_id:&str)->Result<Value,String>{
    let rows=state.store.list("run_usage_invocations").map_err(|e|e.to_string())?.into_iter().filter(|row|row["run_id"]==run_id&&main_invocation(row)).collect::<Vec<_>>();
    Ok(json!({"known":rows.iter().all(|row|row["totals"]["cost_status"]=="known"),"unknown_invocations":rows.iter().filter(|row|row["totals"]["cost_status"]!="known").map(|row|row["invocation_id"].clone()).collect::<Vec<_>>()}))
}

pub(super) fn restore(state:&AppState,run_id:&str)->Result<(),String>{
    let records=state.store.list("run_usage_invocations").map_err(|e|e.to_string())?.into_iter().filter(|row|row["run_id"]==run_id&&main_invocation(row)).collect::<Vec<_>>();
    let mut total=records.iter().filter_map(|row|number(&row["totals"]["usd"])).sum::<f64>();
    if records.is_empty(){let mut latest=HashMap::new();for event in persisted_event_iter(state,run_id)?{if event["event_type"]=="cost.update"&&event["payload"]["scope"]!="auxiliary"{if let(Some(worker),Some(amount))=(event["solver_id"].as_str(),number(&event["payload"]["usd"])){latest.insert(worker.to_owned(),amount);}}}total=latest.values().sum();}
    state.runs.handle(state,run_id).cost_usage.lock().unwrap().total_usd=total;Ok(())
}

#[cfg(test)]
mod auxiliary_accounting_tests {
    use super::*;
    #[test]
    fn auxiliary_records_do_not_enter_main_total() {
        assert!(!main_invocation(&json!({"role":"btw"})));
        assert!(!main_invocation(&json!({"role":"respond","actor_kind":"auxiliary"})));
        assert!(main_invocation(&json!({"role":"explore","actor_kind":"worker"})));
    }
}
