//! Native CLI checkpoint invocations retain the engine's actual persisted history.
use super::*;
use super::worker::IntentAssignment;
use crate::agent_runtime::RuntimeError;
#[derive(Default)]
pub(super) struct Session { pub id:Option<String>,pub established:bool }
impl Session {
    pub fn invocation(engine:&str,argv:&[String])->Self{Self{id:if engine=="kimi"{argv.windows(2).find(|pair|pair[0]=="--session").map(|pair|pair[1].clone())}else{None},established:false}}
    pub fn observe(&mut self,line:&str){
        let Ok(value)=serde_json::from_str::<Value>(line)else{return};let kind=value["type"].as_str().unwrap_or("");
        let id=if kind=="thread.started"{value["thread_id"].as_str()}else if kind=="session"{value["id"].as_str().or_else(||value["session_id"].as_str())}else{value["session_id"].as_str().or_else(||value["sessionID"].as_str()).or_else(||value["sessionId"].as_str())};
        if let Some(id)=id.filter(|id|!id.is_empty()&&id.len()<=4096){if self.id.as_deref()!=Some(id){self.established=false;}self.id=Some(id.to_owned());}
        let message=value.get("message").unwrap_or(&value);
        let completed=kind=="turn.completed"&&value["payload"]["resultType"].as_str().is_none_or(|result|result=="success")
            ||kind=="result"&&value["is_error"]!=true&&value["subtype"].as_str().is_none_or(|status|status=="success")
            ||kind=="message_end"&&message["role"]=="assistant"&&!matches!(message["stopReason"].as_str(),Some("error"|"aborted"))
            ||kind=="step_finish"&&value["part"]["reason"].as_str().is_some_and(|reason|matches!(reason,"stop"|"tool-calls"))
            ||value["role"]=="assistant"&&value["content"].as_str().is_some_and(|text|!text.is_empty())&&value["tool_calls"].as_array().is_none_or(|calls|calls.is_empty());
        if completed&&self.id.is_some(){self.established=true;}
    }
    pub fn resumable(&self)->Option<&str>{self.id.as_deref().filter(|_|self.established)}
}
pub(super) fn initial(engine:&str,model:Option<&str>,effort:Option<&str>,prompt:&str,env:&HashMap<String,String>,interactive:bool,persistent:bool)->Result<(Vec<String>,Option<String>),RuntimeError>{
    let(mut argv,stdin)=super::container_exec::build_container_cli_invocation(engine,model,effort,prompt,env,interactive)?;
    if persistent{argv.retain(|arg|!matches!(arg.as_str(),"--no-session-persistence"|"--ephemeral"|"--no-session"));if engine=="kimi"{argv.splice(1..1,["--session".into(),uuid::Uuid::new_v4().to_string()]);}}Ok((argv,stdin))
}
pub(super) fn resume(engine:&str,model:Option<&str>,effort:Option<&str>,prompt:&str,env:&HashMap<String,String>,session:&str)->Result<(Vec<String>,Option<String>),RuntimeError>{
    let(mut argv,stdin)=initial(engine,model,effort,prompt,env,false,true)?;
    if engine=="kimi"&&let Some(index)=argv.iter().position(|arg|arg=="--session"){argv.drain(index..index+2);}
    let flag=match engine{"claude"|"cursor"|"grok"|"omp"=>"--resume","kimi"|"opencode"=>"--session","droid"=>"--session-id","codex"=>{
        argv.insert(2,"resume".into());argv.insert(3,session.into());return Ok((argv,stdin));},_=>return Err(RuntimeError::unsupported(format!("No native checkpoint resume for {engine}")))};
    let index=if engine=="droid"{2}else{1};argv.splice(index..index,[flag.into(),session.into()]);Ok((argv,stdin))
}
pub(super) fn prompt(_assignment:&IntentAssignment)->String{"尚未收束:请立即停止新的探测命令。若已有实测增量观察,调用 submit_fact 起草结论后再调 commit_step 定稿;若尚未开展新探测,直接调 commit_step 收束本 Step(不要提交“尚未验证”的占位 Fact)。".to_owned()}
