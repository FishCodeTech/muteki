//! Successful ordinary conversation turns are real model evidence, tied to
//! the credential revision that actually launched the execution.
use crate::AppState;
use serde_json::{Value,json};

pub(crate) fn success(state: &AppState,runtime: &Value,revision: Option<&str>,turn: &Value,thread_id: &str,model: &str) -> Result<bool,String> {
    if turn["kind"].as_str().is_some_and(|kind|!matches!(kind,"send" | "retry" | "edit_resend" | "resume" | "conversation.turn.send"))
        || turn["operation"].is_string() || turn["runtime_invocation"].is_object() || turn["status"] != "completed" || model.is_empty() { return Ok(false); }
    let credential = runtime["credential_id"].as_str().unwrap_or("");
    if credential.is_empty() { return Ok(false); }
    let account_id = credential.strip_prefix("account:").or_else(||credential.strip_prefix("endpoint:"));
    let account = account_id.map(|id|state.store.get("credential_accounts",id)).transpose().map_err(|error|error.to_string())?.flatten();
    if account_id.is_some() && (revision.is_none() || account.as_ref().and_then(|row|row["revision"].as_str()) != revision) { return Ok(false); }
    let adapter = runtime["adapter_id"].as_str().unwrap_or("openai");
    let instance = runtime["instance_id"].as_str().unwrap_or("default");
    let runtime_key = format!("{adapter}:{instance}");
    let native = crate::native_chat::is_native(runtime);
    let engine = crate::agent_runtime::descriptors::for_adapter(adapter).map(|(engine,_)|engine.engine()).unwrap_or("api");
    let key = format!("{}:{credential}|{runtime_key}",if native{"native"}else{"http"});
    let test_key = format!("{credential}|{runtime_key}");
    for _ in 0..8 {
        let previous = state.store.get("model_catalogs",&key).map_err(|error|error.to_string())?;
        let prior_test = state.store.get("model_test_results",&test_key).map_err(|error|error.to_string())?;
        let mut catalog = previous.clone().unwrap_or_else(||json!({"credential_id":credential,"engine":engine,"environment":"local",
            "runtime_instance":runtime_key,"configured_models":[],"discovered_models":[],"verified_models":[],
            "source":if native{crate::runtime_api::NATIVE_CATALOG_SOURCE}else{"http_openai_models"},"refresh_status":"missing"}));
        let mut verified = catalog["verified_models"].as_array().cloned().unwrap_or_default();
        if !verified.iter().any(|value|value.as_str()==Some(model)) { verified.push(json!(model)); }
        catalog["verified_models"] = json!(verified);catalog["configuration_revision"] = json!(revision);
        let test = json!({"ok":true,"status":"ok","model":model,"engine":engine,"credential_id":credential,"backend":"local",
            "runtime_instance":runtime_key,"tested_at":chrono::Utc::now().timestamp_millis() as f64/1000.0,
            "configuration_revision":revision,"layer":"model","verification_source":"conversation","thread_id":thread_id,"turn_id":turn["turn_id"],
            "detail":"已由真实聊天成功回合验证"});
        let mut conditions=vec![("model_catalogs",key.as_str(),previous.as_ref()),("model_test_results",test_key.as_str(),prior_test.as_ref())];
        if let Some(id)=account_id { conditions.push(("credential_accounts",id,account.as_ref())); }
        if state.store.put_many_if(&conditions,&[("model_catalogs",key.as_str(),&catalog),("model_test_results",test_key.as_str(),&test)]).map_err(|error|error.to_string())? {return Ok(true);}
        if let Some(id)=account_id {
            if state.store.get("credential_accounts",id).map_err(|error|error.to_string())? != account {return Ok(false);}
        }
    }
    Err("Model evidence changed repeatedly during commit".into())
}
