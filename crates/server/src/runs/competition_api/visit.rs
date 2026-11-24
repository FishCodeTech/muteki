//! One durable visit record owns counters; admission_decision is a compatibility projection.
use super::*;
#[derive(Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
pub(super) struct Execution { pub run_id:String, pub generation:u64 }
#[derive(Clone,Copy,Default,serde::Serialize,serde::Deserialize)]
#[serde(rename_all="snake_case")]
enum Phase { #[default] Unvisited, Running, Settled }
#[derive(Default,serde::Serialize,serde::Deserialize)]
#[serde(default)]
struct Visit {
    phase:Phase,
    execution:Option<Execution>,
    binding_id:String,
    visit_count:u64,
    last_visit_at:Option<String>,
    dry_waves:u64,
    visit_timebox_s:u64,
    candidate_count_at_visit:u64,
    persistent_execution:bool,
    harvested_at:Option<String>,
}
const FIELDS:&[&str]=&["visit_count","last_visit_at","dry_waves","visit_timebox_s","candidate_count_at_visit","persistent_execution","harvested_at"];
fn read(entry:&Value)->ApiResult<Visit>{
    if entry["visit"].is_object(){return serde_json::from_value(entry["visit"].clone()).map_err(ApiError::internal);}
    let legacy=&entry["admission_decision"];
    let mut visit=Visit{visit_count:legacy["visit_count"].as_u64().unwrap_or(0),
        last_visit_at:legacy["last_visit_at"].as_str().filter(|text|!text.is_empty()).map(str::to_owned),
        dry_waves:legacy["dry_waves"].as_u64().unwrap_or(0),visit_timebox_s:legacy["visit_timebox_s"].as_u64().unwrap_or(0),
        candidate_count_at_visit:legacy["candidate_count_at_visit"].as_u64().unwrap_or(0),persistent_execution:legacy["persistent_execution"]==true,
        harvested_at:legacy["harvested_at"].as_str().filter(|text|!text.is_empty()).map(str::to_owned),..Visit::default()};
    if let (Some(run),Some(generation))=(entry["last_admitted_execution"][0].as_str(),entry["last_admitted_execution"][1].as_u64()){
        visit.execution=Some(Execution{run_id:run.into(),generation});
    }
    visit.phase=if visit.visit_count==0{Phase::Unvisited}else if visit.harvested_at.is_some(){Phase::Settled}else{Phase::Running};
    Ok(visit)
}
fn write(entry:&mut Value,visit:Visit)->ApiResult<()>{
    let value=serde_json::to_value(&visit).map_err(ApiError::internal)?;
    if !entry["admission_decision"].is_object(){entry["admission_decision"]=json!({});}
    for key in FIELDS{entry["admission_decision"][*key]=value[*key].clone();}
    entry["last_admitted_execution"]=visit.execution.map(|execution|json!([execution.run_id,execution.generation])).unwrap_or(Value::Null);
    entry["visit"]=value;
    Ok(())
}
pub(super) fn field<'a>(entry:&'a Value,key:&str)->&'a Value{
    if entry["visit"].is_object(){&entry["visit"][key]}else{&entry["admission_decision"][key]}
}
pub(super) fn project(entry:&mut Value)->ApiResult<()>{let visit=read(entry)?;write(entry,visit)}
pub(super) fn admit(entry:&mut Value,execution:Execution,binding:&str,candidates:u64,persistent:bool,timebox:u64)->ApiResult<bool>{
    let mut visit=read(entry)?;
    if visit.execution.as_ref()==Some(&execution){write(entry,visit)?;return Ok(false);}
    visit.phase=Phase::Running;visit.execution=Some(execution);visit.binding_id=binding.into();
    visit.visit_count=visit.visit_count.checked_add(1).ok_or_else(||ApiError::internal("Visit counter exhausted"))?;
    visit.last_visit_at=Some(now());visit.candidate_count_at_visit=candidates;
    visit.persistent_execution=persistent;visit.visit_timebox_s=if persistent{0}else{timebox};visit.harvested_at=None;
    write(entry,visit)?;Ok(true)
}
/// None means an old row contains no execution identity; the caller must then
/// prove that no competing binding owns the challenge before projecting it.
pub(super) fn matches_execution(entry:&Value,expected:&Execution)->ApiResult<Option<bool>>{
    let visit=read(entry)?;
    if let Some(execution)=visit.execution{return Ok(Some(execution==*expected));}
    if entry["visit"].is_object(){return Ok(Some(false));}
    if let Some(run)=entry["admission_decision"]["run_id"].as_str().filter(|run|!run.is_empty()){return Ok(Some(run==expected.run_id));}
    Ok(None)
}
pub(super) fn finish(entry:&mut Value,expected:Option<&Execution>,candidates:u64)->ApiResult<bool>{
    let legacy=!entry["visit"].is_object();
    let mut visit=read(entry)?;
    if legacy&&visit.execution.is_none(){if let Some(expected)=expected{visit.execution=Some(expected.clone());}}
    if expected.is_some()&&visit.execution.as_ref()!=expected{return Ok(false);}
    if visit.harvested_at.is_some(){write(entry,visit)?;return Ok(false);}
    visit.dry_waves=if candidates>visit.candidate_count_at_visit{0}else{visit.dry_waves.checked_add(1).ok_or_else(||ApiError::internal("Visit counter exhausted"))?};
    visit.harvested_at=Some(now());visit.phase=Phase::Settled;write(entry,visit)?;Ok(true)
}
