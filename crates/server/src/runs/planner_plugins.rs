//! A per-invocation, authenticated bridge to the existing managed MCP registry.
use super::*;
use axum::{extract::State,http::{HeaderMap,StatusCode},routing::post};
struct BridgeState{registry:Arc<crate::plugins::Registry>,token:String,mode:String,scope:String}
pub(super) struct Bridge{pub config:Value,task:tokio::task::AbortHandle}
impl Drop for Bridge{fn drop(&mut self){self.task.abort();}}
async fn invoke(State(state):State<Arc<BridgeState>>,headers:HeaderMap,Json(body):Json<Value>)->Response{
    let token=headers.get("authorization").and_then(|value|value.to_str().ok()).and_then(|value|value.strip_prefix("Bearer ")).unwrap_or("");
    if token.len()!=state.token.len() || token.bytes().zip(state.token.bytes()).fold(0,|diff,(left,right)|diff|(left^right))!=0{return StatusCode::FORBIDDEN.into_response();}
    let Some(name)=body["name"].as_str()else{return StatusCode::BAD_REQUEST.into_response()};
    if !body["arguments"].is_object(){return(StatusCode::UNPROCESSABLE_ENTITY,Json(json!({"error":{"code":"worker_mcp.invalid_arguments"}}))).into_response();}
    let work=async{
        let tools=state.registry.prepare_tools("pi",&state.mode,&state.scope).await?;
        let tool=tools.iter().find(|tool|tool["name"]==name).ok_or_else(||crate::plugins::PluginError::new("worker_mcp.tool_unavailable","Tool is not enabled for this planner"))?;
        let validator=jsonschema::draft202012::options().build(&tool["input_schema"]).map_err(|error|crate::plugins::PluginError::new("worker_mcp.schema_invalid",error.to_string()))?;
        validator.validate(&body["arguments"]).map_err(|error|crate::plugins::PluginError::new("worker_mcp.invalid_arguments",error.to_string()))?;
        state.registry.invoke("pi",name,body["arguments"].clone(),&state.mode,&state.scope).await
    };
    match tokio::time::timeout(Duration::from_secs(120),work).await{
        Ok(Ok(result))=>Json(result).into_response(),Ok(Err(error))=>(StatusCode::BAD_GATEWAY,Json(json!({"error":{"code":error.code,"message":error.message}}))).into_response(),Err(_)=>(StatusCode::GATEWAY_TIMEOUT,Json(json!({"error":{"code":"worker_mcp.outcome_unknown","message":"Tool outcome requires reconciliation"}}))).into_response()
    }
}
pub(super) async fn start(state:&AppState,run_id:&str,mode:&str)->Result<Bridge,String>{
    let registry=crate::plugins::registry(state).map_err(|error|error.to_string())?;
    let tools=registry.prepare_tools("pi",mode,run_id).await.map_err(|error|error.to_string())?;
    let errors=registry.runtime_mcp_health("pi",mode,run_id).map_err(|error|error.to_string())?;
    let listener=tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED,0)).await.map_err(|e|e.to_string())?;
    let port=listener.local_addr().map_err(|e|e.to_string())?.port();let token=format!("{}{}",uuid::Uuid::new_v4().simple(),uuid::Uuid::new_v4().simple());
    let config=json!({"url":format!("http://host.docker.internal:{port}/invoke"),"token":token,"tools":tools,"connection_errors":errors});
    let app=Router::new().route("/invoke",post(invoke)).layer(DefaultBodyLimit::max(4*1024*1024)).with_state(Arc::new(BridgeState{registry,token,mode:mode.into(),scope:run_id.into()}));
    let task=tokio::spawn(async move{if let Err(error)=axum::serve(listener,app).await{tracing::error!(%error,"Planner MCP bridge stopped");}}).abort_handle();
    Ok(Bridge{config,task})
}
