//! A persisted UTC deadline is capped by saved remaining time and a monotonic lease.
use super::*;
struct Lease{at:std::time::Instant,remaining:f64,suspended:f64}
static LEASES:std::sync::OnceLock<Mutex<HashMap<String,Lease>>>=std::sync::OnceLock::new();
fn seconds(value:&Value,name:&str)->Result<f64,String>{let seconds=value.as_f64().filter(|v|v.is_finite()&&*v>=0.).ok_or_else(||format!("{name} must be finite nonnegative seconds"))?;let duration=Duration::try_from_secs_f64(seconds).map_err(|_|format!("{name} exceeds the duration range"))?;if std::time::Instant::now().checked_add(duration).is_none(){return Err(format!("{name} exceeds the clock range"));}Ok(seconds)}
fn end_after(duration_seconds:f64)->Result<chrono::DateTime<chrono::Utc>,String>{
    let seconds=seconds(&json!(duration_seconds),"execution lease")?;let millis=(now_secs()+seconds)*1000.;if !millis.is_finite()||millis>i64::MAX as f64{return Err("Execution deadline overflow".into());}chrono::DateTime::from_timestamp_millis(millis as i64).ok_or_else(||"Execution deadline overflow".into())
}
fn deadline(record:&Value)->Result<f64,String>{let text=record["deadline_utc"].as_str().ok_or("Execution deadline is missing")?;chrono::DateTime::parse_from_rfc3339(text).map(|value|value.timestamp_millis() as f64/1000.).map_err(|error|error.to_string())}
pub(super) fn prepare(run:&Value,config:&Value)->Result<Value,String>{
    let existing=run.get("execution_budget").filter(|value|value.is_object());
    let requested=config.get("execution_budget").filter(|value|value.is_object());
    if let Some(record)=existing.or(requested){
        if let(Some(existing),Some(requested))=(existing,requested){if existing!=requested{return Err("Continuation must preserve its original execution budget".into());}}
        if record["version"]!=1{return Err("Unsupported execution budget version".into());}deadline(record)?;
        for field in ["remaining_seconds","conclude_seconds","finalization_seconds"]{seconds(&record[field],field)?;}
        let mut lease = record.clone();
        if existing.is_some() && run["running"] == false
            && matches!(run["status"].as_str(), Some("finished" | "solved")) {
            let remaining = seconds(&record["remaining_seconds"], "remaining_seconds")?;
            lease["deadline_utc"] = json!(end_after(remaining)?.to_rfc3339());
            lease["suspended_seconds_at_start"] = json!(super::control_freeze::suspended_seconds(run));
            lease["saved_at"] = json!(now_secs());
        }
        return Ok(lease);
    }
    let total=seconds(config.get("wall_clock_budget").unwrap_or(&json!(0.)),"wall_clock_budget")?;
    if total<=0.{return Ok(Value::Null);}
    let finalize=seconds(config["stage_policy"]["coordinator"].get("finalization_seconds").unwrap_or(&json!(120.)),"finalization_seconds")?.min(total/3.).max(0.);
    let conclude=seconds(config["stage_policy"]["coordinator"].get("conclude_seconds").unwrap_or(&json!(120.)),"conclude_seconds")?.min((total-finalize)/3.).max(0.);
    let end=end_after(total)?;
    Ok(json!({"version":1,"deadline_utc":end.to_rfc3339(),"remaining_seconds":total,"conclude_seconds":conclude,"finalization_seconds":finalize,"suspended_seconds_at_start":super::control_freeze::suspended_seconds(run)}))
}
pub(super) fn remaining(state:&AppState,run_id:&str,phase:&str)->Result<Option<f64>,String>{
    let run=state.store.get("runs",run_id).map_err(|e|e.to_string())?.ok_or("Execution Run not found")?;
    let record=&run["execution_budget"];if record.is_null(){return Ok(None);}
    for field in ["remaining_seconds","conclude_seconds","finalization_seconds"]{seconds(&record[field],field)?;}
    let end=deadline(record)?;let suspended=super::control_freeze::suspended_seconds(&run);let initial_suspended=record["suspended_seconds_at_start"].as_f64().unwrap_or(0.);
    let key=format!("{}:{run_id}:{}",state.chat.epoch(),record["deadline_utc"]);
    let mut leases=LEASES.get_or_init(||Mutex::new(HashMap::new())).lock().unwrap();
    let lease=leases.entry(key).or_insert_with(||Lease{at:std::time::Instant::now(),remaining:record["remaining_seconds"].as_f64().unwrap_or(0.).min((end-now_secs()+(suspended-initial_suspended).max(0.)).max(0.)),suspended});
    let remaining=(lease.remaining-lease.at.elapsed().as_secs_f64()+(suspended-lease.suspended).max(0.)).max(0.);
    let reserve=match phase{"execute"=>record["conclude_seconds"].as_f64().unwrap_or(0.)+record["finalization_seconds"].as_f64().unwrap_or(0.),"conclude"=>record["finalization_seconds"].as_f64().unwrap_or(0.),_=>0.};
    Ok(Some((remaining-reserve).max(0.)))
}
pub(super) fn limit(state:&AppState,run_id:&str,phase:&str,maximum:Duration)->Result<Duration,String>{
    match remaining(state,run_id,phase)?{Some(seconds)if seconds<=0.=>Err("execution_budget_exhausted".into()),Some(seconds)=>Ok(maximum.min(Duration::try_from_secs_f64(seconds).map_err(|_|"Execution budget duration overflow")?)),None=>Ok(maximum)}
}
pub(super) fn checkpoint(state:&AppState,run_id:&str)->Result<(),String>{
    let Some(remaining)=remaining(state,run_id,"finalize")?else{return Ok(())};
    for _ in 0..16{let before=state.store.get("runs",run_id).map_err(|e|e.to_string())?.ok_or("Execution Run missing")?;
        let mut after=before.clone();after["execution_budget"]["remaining_seconds"]=json!(remaining.min(before["execution_budget"]["remaining_seconds"].as_f64().unwrap_or(remaining)));after["execution_budget"]["saved_at"]=json!(now_secs());
        if state.store.put_many_if(&[("runs",run_id,Some(&before))],&[("runs",run_id,&after)]).map_err(|e|e.to_string())?{return Ok(());}
    }Err("Execution budget checkpoint conflicted repeatedly".into())
}

pub(super) struct WorkerLease{key:String,record:Value,at:std::time::Instant,suspended:f64,last_save:std::time::Instant}
impl WorkerLease{
    pub fn start(state:&AppState,run_id:&str,worker:&str,intent:&str,profile:&Value,challenge:&Value)->Result<Self,String>{
        let run=state.store.get("runs",run_id).map_err(|e|e.to_string())?.ok_or("Worker Run missing")?;
        let epoch=run.get("target_epoch").unwrap_or(&run["execution_generation"]);
        let key=json!([run_id,worker,intent,epoch]).to_string();let suspended=super::control_freeze::worker_budget(state,run_id,worker).0;
        let before=state.store.get("run_worker_execution_budgets",&key).map_err(|e|e.to_string())?;
        let mut record=if let Some(record)=before{record}else{
            let execute=super::worker::effective_execute_timeout_seconds(profile,challenge) as f64;
            let conclude=seconds(profile.get("conclude_timeout").unwrap_or(&json!(120.)),"Worker conclude_timeout")?;let finalize: f64=5.;
            let total=remaining(state,run_id,"conclude")?.map(|left|left.min(execute+conclude+finalize)).unwrap_or(execute+conclude+finalize);
            let finalize=finalize.min(total);let conclude=conclude.min((total-finalize).max(0.));
            let end=end_after(total)?;
            let record=json!({"version":1,"run_id":run_id,"worker_id":worker,"intent_id":intent,"target_epoch":epoch,"deadline_utc":end.to_rfc3339(),"remaining_seconds":total,"conclude_seconds":conclude,"finalization_seconds":finalize,"suspended_at_start":suspended});
            if !state.store.put_many_if(&[("run_worker_execution_budgets",&key,None)],&[("run_worker_execution_budgets",&key,&record)]).map_err(|e|e.to_string())?{return Err("Worker execution lease already exists; retry admission".into());}record
        };
        if record["version"]!=1||record["run_id"]!=run_id||record["worker_id"]!=worker||record["intent_id"]!=intent||record["target_epoch"]!=*epoch{return Err("Worker execution lease binding is invalid".into());}
        for field in ["conclude_seconds","finalization_seconds"]{seconds(&record[field],field)?;}
        let left=seconds(&record["remaining_seconds"],"Worker remaining seconds")?.min((deadline(&record)?-now_secs()+(suspended-record["suspended_at_start"].as_f64().unwrap_or(0.)).max(0.)).max(0.));
        record["remaining_seconds"]=json!(left);
        Ok(Self{key,record,at:std::time::Instant::now(),suspended,last_save:std::time::Instant::now()})
    }
    pub fn remaining(&self,state:&AppState,run_id:&str,worker:&str,phase:&str)->Result<f64,String>{
        let suspended=super::control_freeze::worker_budget(state,run_id,worker).0;
        let own=(self.record["remaining_seconds"].as_f64().unwrap_or(0.)-self.at.elapsed().as_secs_f64()+(suspended-self.suspended).max(0.)).max(0.);
        let reserve=match phase{"execute"=>self.record["conclude_seconds"].as_f64().unwrap_or(0.)+self.record["finalization_seconds"].as_f64().unwrap_or(0.),"conclude"=>self.record["finalization_seconds"].as_f64().unwrap_or(0.),_=>0.};
        let own=(own-reserve).max(0.);Ok(remaining(state,run_id,phase)?.map(|parent|parent.min(own)).unwrap_or(own))
    }
    pub fn checkpoint(&mut self,state:&AppState,run_id:&str,worker:&str)->Result<(),String>{
        if self.last_save.elapsed()<Duration::from_secs(1){return Ok(());}
        let left=self.remaining(state,run_id,worker,"finalize")?;
        let before=state.store.get("run_worker_execution_budgets",&self.key).map_err(|e|e.to_string())?.ok_or("Worker lease disappeared")?;
        let mut after=before.clone();after["remaining_seconds"]=json!(left.min(before["remaining_seconds"].as_f64().unwrap_or(left)));after["saved_at"]=json!(now_secs());
        if !state.store.put_many_if(&[("run_worker_execution_budgets",&self.key,Some(&before))],&[("run_worker_execution_budgets",&self.key,&after)]).map_err(|e|e.to_string())?{return Err("Worker lease changed during checkpoint".into());}
        self.last_save=std::time::Instant::now();Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_run_reopens_with_saved_allowance_and_a_fresh_deadline() {
        let old = json!({"version":1,"deadline_utc":"2020-01-01T00:00:00Z",
            "remaining_seconds":585.0,"conclude_seconds":120.0,
            "finalization_seconds":120.0,"suspended_seconds_at_start":0.0});
        let ended = json!({"status":"finished","running":false,"execution_budget":old});
        let config = json!({"execution_budget":old});
        let prepared = prepare(&ended, &config).unwrap();
        assert_eq!(prepared["remaining_seconds"], 585.0);
        assert!(deadline(&prepared).unwrap() > now_secs() + 580.0);
        let active = json!({"status":"running","running":true,"execution_budget":old});
        assert_eq!(prepare(&active, &config).unwrap()["deadline_utc"], old["deadline_utc"]);
    }
}
