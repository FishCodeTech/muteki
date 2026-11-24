//! Metadata jobs reserve a revision in the same conversation write that
//! requests them. Applying a title checks that revision and the original turn.
use crate::{AppState,chat,native_chat};
use axum::{Json,extract::{Path,State}};
use serde_json::{Value,json};
use std::{collections::HashMap,sync::{Arc,Mutex}};
enum Job { Generate(String,String,u64),Cancel(String) }
pub struct Service {sender:tokio::sync::mpsc::UnboundedSender<Job>,receiver:Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Job>>>}
impl Default for Service {
    fn default()->Self {let(sender,receiver)=tokio::sync::mpsc::unbounded_channel();Self{sender,receiver:Mutex::new(Some(receiver))}}
}

pub(crate) fn observe(state:&AppState,thread_id:&str,doc:&mut Value,event:&str,payload:&Value) {
    if doc["thread"]["mode"]!="conversation"{return;}
    if event=="core.thread.renamed" {
        doc["thread"]["metadata_revision"]=json!(doc["thread"]["metadata_revision"].as_u64().unwrap_or(0)+1);
        let _=state.metadata.sender.send(Job::Cancel(thread_id.into()));return;
    }
    let turn=payload["turn_id"].as_str().unwrap_or("");
    if matches!(event,"core.turn.failed"|"core.turn.interrupted"|"core.turn.retried"|"core.turn.edit_resent") {
        if doc["thread"]["metadata_turn_id"]==turn || payload["superseded_turn_ids"].as_array().into_iter().flatten()
            .any(|id|id==&doc["thread"]["metadata_turn_id"]) || doc["thread"]["metadata_turn_id"]==payload["retry_of_turn_id"] {
            doc["thread"]["metadata_revision"]=json!(doc["thread"]["metadata_revision"].as_u64().unwrap_or(0)+1);
            let _=state.metadata.sender.send(Job::Cancel(thread_id.into()));
        }
        return;
    }
    if event!="core.turn.started" || turn.is_empty() || doc["thread"]["title_source"]!="fallback" || doc["thread"]["metadata_turn_id"]==turn{return;}
    let first=doc["turns"].as_array().into_iter().flatten().find(|row|row["status"]!="superseded" && !row["operation"].is_string()
        && !row["runtime_invocation"].is_object() && (row["text"].as_str().is_some_and(|text|!text.trim().is_empty())
            || row["attachments"].as_array().is_some_and(|files|!files.is_empty())));
    if first.is_none_or(|row|row["turn_id"]!=turn){return;}
    let revision=doc["thread"]["metadata_revision"].as_u64().unwrap_or(0)+1;
    doc["thread"]["metadata_revision"]=json!(revision);doc["thread"]["metadata_turn_id"]=json!(turn);
    let _=state.metadata.sender.send(Job::Generate(thread_id.into(),turn.into(),revision));
}
fn eligible(doc:&Value,turn_id:&str,revision:u64)->bool {
    doc["thread"]["title_source"]=="fallback" && doc["thread"]["metadata_turn_id"]==turn_id
        && doc["thread"]["metadata_revision"]==revision && doc["thread"]["archived"]!=true
        && doc["turns"].as_array().into_iter().flatten().any(|row|row["turn_id"]==turn_id
            && !matches!(row["status"].as_str(),Some("superseded"|"interrupted"|"cancelled"|"failed")))
}

pub fn spawn(state:Arc<AppState>) {
    let Some(mut receiver)=state.metadata.receiver.lock().unwrap().take() else{return};
    tokio::spawn(async move {
        let mut tasks=HashMap::<String,tokio::task::AbortHandle>::new();
        while let Some(job)=receiver.recv().await {
            tasks.retain(|_,task|!task.is_finished());
            let (thread_id,request)=match job {Job::Generate(id,turn,revision)=>(id,Some((turn,revision))),Job::Cancel(id)=>(id,None)};
            if let Some(previous)=tasks.remove(&thread_id){previous.abort();}
            if let Some((turn,revision))=request {
                let state=state.clone();let id=thread_id.clone();
                let task=tokio::spawn(async move { update(state,id,turn,revision).await; });
                tasks.insert(thread_id,task.abort_handle());
            }
        }
        for task in tasks.into_values(){task.abort();}
    });
}

struct TitleFailure {code:&'static str,message:String}
impl TitleFailure {
    fn new(code:&'static str,message:impl Into<String>)->Self{Self{code,message:message.into()}}
}
const RUNTIME_UNSUPPORTED:&str="conversation.title.runtime_unsupported";

/// The model chosen in settings for titles; `None` means the thread's own runtime.
fn configured_runtime(state:&AppState)->Result<Option<Value>,TitleFailure> {
    let ui=state.store.get("settings","ui").map_err(|error|TitleFailure::new("conversation.title.storage_failed",error.to_string()))?;
    if let Some(model)=ui.as_ref().map(|doc|&doc["values"]["titleModel"]).filter(|value|value.is_object()) {
        let text=|key:&str|model[key].as_str().map(str::trim).unwrap_or("");
        let (credential,model_id,runtime_key)=(text("credentialId"),text("modelId"),text("runtimeKey"));
        if !credential.is_empty() || !model_id.is_empty() {
            if credential.is_empty()||model_id.is_empty(){return Err(TitleFailure::new("conversation.title.model_invalid","标题模型设置缺少凭据或模型。"));}
            let engine=if let Some(engine)=credential.strip_prefix("system:"){engine.to_owned()}else{
                let id=credential.strip_prefix("account:").unwrap_or(credential);
                let account=crate::agent_runtime::credentials::account(&state.store,id)
                    .ok_or_else(||TitleFailure::new("conversation.title.credential_missing","标题模型使用的凭据已不存在，请重新选择。"))?;
                let worker=account["worker_engine"].as_str().unwrap_or("");
                (if worker.is_empty(){account["engine"].as_str().unwrap_or("")}else{worker}).to_owned()
            };
            let key=if runtime_key.is_empty(){crate::runtime_api::default_runtime_key(&engine)}else{Some(runtime_key.to_owned())}
                .ok_or_else(||TitleFailure::new("conversation.title.engine_unknown",format!("标题模型使用的引擎 {engine} 不可用。")))?;
            return Ok(Some(crate::native_credentials::probe_runtime(credential,&key,model_id,"")));
        }
    }
    let Some(profile) = crate::runs::titler::configured_profile(state, None) else {
        return Ok(None);
    };
    crate::runs::titler::native_runtime_from_profile(state, &profile)
        .map_err(|detail| TitleFailure::new("conversation.title.model_invalid", detail))
}

/// One title request; usage is recorded against the thread whether or not the reply parses.
async fn request_title(state:&Arc<AppState>,thread_id:&str,turn_id:&str,runtime:&Value,request:&Value)->Result<String,TitleFailure> {
    let prompt=format!("Generate a concise conversation title in the user's language. Return only a JSON object with a title string, at most 60 characters. Do not use tools. Describe this opening user request:\n{request}");
    let reply:Result<Value,String>=if runtime["_titler_profile"].is_object() {
        crate::runs::titler::prompt_profile_http(state, &runtime["_titler_profile"], &prompt, 1024)
            .await
            .map(|(text, usage, model)| json!({"reply": text, "usage": usage, "model": model}))
    } else if let Some(reply)=chat::extension_prompt_model(state,runtime,&prompt).await{reply}else if native_chat::is_native(runtime) {
        native_chat::prompt_model(state,runtime,&prompt).await.map_err(|error|error.message)
    } else {return Err(TitleFailure::new(RUNTIME_UNSUPPORTED,"当前运行时不支持生成标题。"))};
    let reply=reply.map_err(|error|TitleFailure::new("conversation.title.model_failed",error))?;
    if reply["usage"].is_object(){
        let id=format!("title-{}",uuid::Uuid::new_v4());
        let mut record=json!({"id":id,"at":chrono::Utc::now().timestamp_millis() as f64/1000.0,"engine":runtime["adapter_id"],
            "model":reply["model"],"credential_id":runtime["credential_id"],"thread_id":thread_id,"turn_id":turn_id,
            "workspace_kind":"conversation","actor_kind":"auxiliary","role":"titler"});
        crate::usage_history::capture(&mut record,&reply["usage"]);
        if state.store.put("usage",&id,&record).is_err(){tracing::warn!("Could not save title usage");}
    }
    let title = reply["reply"]
        .as_str()
        .and_then(crate::runs::titler::parse_title_response)
        .ok_or_else(|| TitleFailure::new("conversation.title.reply_invalid", "模型没有返回可解析的标题。"))?;
    Ok(title.chars().take(60).collect())
}

async fn update(state:Arc<AppState>,thread_id:String,turn_id:String,revision:u64) {
    let (runtime,request)={
        let _guard=state.chat.active.lock().await;
        let Ok(doc)=chat::load(&state,&thread_id) else{return};
        if !eligible(&doc,&turn_id,revision){return;}
        let Some(turn)=doc["turns"].as_array().into_iter().flatten().find(|row|row["turn_id"]==turn_id) else{return};
        let attachments:Vec<Value>=turn["attachments"].as_array().into_iter().flatten().filter_map(Value::as_str)
            .filter_map(|id|crate::files::attachment_metadata(&doc,id)).map(|meta|json!({"name":meta["name"],"media_type":meta["media_type"],"size":meta["size"]})).collect();
        (turn["runtime_snapshot"].clone(),json!({"request":turn["text"],"attachments":attachments,"selected_context":turn["capability_refs"]}))
    };
    let runtime=match configured_runtime(&state) {
        Ok(Some(configured))=>configured,
        Ok(None)=>runtime,
        Err(failure)=>{tracing::warn!(thread_id,turn_id,code=failure.code,error=%failure.message,"Conversation title model is not usable");return;}
    };
    for delay in [0,2,4] {
        if delay>0{tokio::time::sleep(std::time::Duration::from_secs(delay)).await;}
        {let _guard=state.chat.active.lock().await;let Ok(doc)=chat::load(&state,&thread_id) else{return};if !eligible(&doc,&turn_id,revision){return;}}
        let title=match request_title(&state,&thread_id,&turn_id,&runtime,&request).await {
            Ok(title)=>title,
            Err(failure) if failure.code==RUNTIME_UNSUPPORTED=>return,
            Err(failure)=>{tracing::warn!(thread_id,turn_id,code=failure.code,error=%failure.message,"Conversation title generation failed");continue;}
        };
        let _guard=state.chat.active.lock().await;
        let Ok(mut doc)=chat::load(&state,&thread_id) else{return};if !eligible(&doc,&turn_id,revision){return;}
        doc["thread"]["title"]=json!(title);doc["thread"]["title_source"]=json!("generated");
        doc["thread"]["metadata_revision"]=json!(revision+1);
        chat::emit(&state,&thread_id,&mut doc,"core.thread.metadata.updated",json!({"thread_id":thread_id,"turn_id":turn_id,
            "title":title,"title_source":"generated","metadata_revision":revision+1}));
        if chat::save(&state,&thread_id,&mut doc).is_err(){tracing::warn!("Could not save generated conversation title");}
        return;
    }
}

fn title_failure(status:axum::http::StatusCode,failure:TitleFailure)->chat::Failure{chat::fail_code(status,failure.code,&failure.message)}

/// User-requested rename by model: replaces whatever the title is now, from the thread's first request.
async fn regenerate(State(state):State<Arc<AppState>>,Path(thread_id):Path<String>)->chat::Api {
    use axum::http::StatusCode;
    let (runtime,request,turn_id,revision)={
        let _guard=state.chat.active.lock().await;
        let doc=chat::load(&state,&thread_id)?;
        if doc["thread"]["mode"]!="conversation"{return Err(chat::fail_code(StatusCode::CONFLICT,"conversation.title.unsupported_thread","只有对话线程可以重新生成标题。"));}
        let first=doc["turns"].as_array().into_iter().flatten().find(|row|row["status"]!="superseded" && !row["operation"].is_string()
            && !row["runtime_invocation"].is_object() && (row["text"].as_str().is_some_and(|text|!text.trim().is_empty())
                || row["attachments"].as_array().is_some_and(|files|!files.is_empty())))
            .ok_or_else(||chat::fail_code(StatusCode::CONFLICT,"conversation.title.no_request","线程还没有可用于生成标题的首条消息。"))?;
        let attachments:Vec<Value>=first["attachments"].as_array().into_iter().flatten().filter_map(Value::as_str)
            .filter_map(|id|crate::files::attachment_metadata(&doc,id)).map(|meta|json!({"name":meta["name"],"media_type":meta["media_type"],"size":meta["size"]})).collect();
        (first["runtime_snapshot"].clone(),json!({"request":first["text"],"attachments":attachments,"selected_context":first["capability_refs"]}),
            first["turn_id"].as_str().unwrap_or("").to_owned(),doc["thread"]["metadata_revision"].as_u64().unwrap_or(0))
    };
    let runtime=configured_runtime(&state).map_err(|failure|title_failure(StatusCode::CONFLICT,failure))?.unwrap_or(runtime);
    let title=request_title(&state,&thread_id,&turn_id,&runtime,&request).await.map_err(|failure|{
        let status=if failure.code==RUNTIME_UNSUPPORTED{StatusCode::CONFLICT}else{StatusCode::BAD_GATEWAY};
        title_failure(status,failure)
    })?;
    let _guard=state.chat.active.lock().await;
    let mut doc=chat::load(&state,&thread_id)?;
    if doc["thread"]["metadata_revision"].as_u64().unwrap_or(0)!=revision {
        return Err(chat::fail_code(StatusCode::CONFLICT,"conversation.title.superseded","标题在生成期间被修改，请重试。"));
    }
    let revision=revision+1;
    doc["thread"]["title"]=json!(title);doc["thread"]["title_source"]=json!("generated");doc["thread"]["metadata_revision"]=json!(revision);
    chat::emit(&state,&thread_id,&mut doc,"core.thread.metadata.updated",json!({"thread_id":thread_id,"turn_id":turn_id,
        "title":title,"title_source":"generated","metadata_revision":revision}));
    chat::save(&state,&thread_id,&mut doc)?;
    Ok(Json(json!({"title":title})))
}

pub fn router()->axum::Router<Arc<AppState>> {
    axum::Router::new().route("/api/threads/{id}/title/regenerate",axum::routing::post(regenerate))
}
