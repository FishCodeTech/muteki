//! Connection identities own short-lived session secrets and conditional response caches.
use super::*;
#[derive(Clone)]pub(super) struct Cached{pub etag:String,pub body:Value}
static RESPONSES:OnceLock<Mutex<HashMap<String,Cached>>>=OnceLock::new();
static SESSIONS:OnceLock<Mutex<HashMap<String,Arc<tokio::sync::Mutex<Option<String>>>>>>=OnceLock::new();
pub(super) fn identity(state:&AppState,connection:&Value)->ApiResult<String>{
    let reference=field(connection,"credential_ref");let secret=if reference.is_empty(){String::new()}else{read_secret(state,reference)?};
    Ok(format!("{:x}",Sha256::digest(json!([native_chat::state_dir(state),connection["connection_id"],connection["canonical_base_url"],connection["account_key"],reference,format!("{:x}",Sha256::digest(secret.as_bytes()))]).to_string().as_bytes())))
}
pub(super) fn response(key:&str)->Option<Cached>{RESPONSES.get_or_init(||Mutex::new(HashMap::new())).lock().unwrap().get(key).cloned()}
pub(super) fn save(key:String,etag:String,body:Value){let mut rows=RESPONSES.get_or_init(||Mutex::new(HashMap::new())).lock().unwrap();if rows.len()>=1024 && !rows.contains_key(&key){rows.clear();}rows.insert(key,Cached{etag,body});}
pub(super) fn session(key:&str)->Arc<tokio::sync::Mutex<Option<String>>>{let mut rows=SESSIONS.get_or_init(||Mutex::new(HashMap::new())).lock().unwrap();if rows.len()>=256 && !rows.contains_key(key){rows.clear();}rows.entry(key.into()).or_insert_with(||Arc::new(tokio::sync::Mutex::new(None))).clone()}
pub(super) async fn invalidate_session(state:&AppState,connection:&Value)->ApiResult<()>{let key=identity(state,connection)?;*session(&key).lock().await=None;Ok(())}
pub(super) fn transport(error:&reqwest::Error)->ApiError{
    ApiError::new(StatusCode::BAD_GATEWAY,if error.is_timeout(){"competition.platform_timeout"}else if error.is_connect(){"competition.platform_transient"}else{"competition.platform_transport"},if error.is_timeout(){"Platform request timed out"}else{"Platform transport failed"})
}
/// Mutating requests whose response was lost may already have taken effect.
pub(super) fn request_transport(error:&reqwest::Error,mutating:bool)->ApiError{
    if mutating&&!error.is_connect(){
        ApiError::new(StatusCode::BAD_GATEWAY,"competition.platform_outcome_unknown","Platform response was not confirmed; reconcile the original submission before retrying")
    }else{transport(error)}
}
pub(super) fn schema_hash(schema:&Value)->String{
    let paths=schema["paths"].as_object().into_iter().flatten().map(|(path,methods)|{let mut methods=methods.as_object().into_iter().flatten().filter(|(method,_)|method.as_str()!="parameters").map(|(method,_)|method.clone()).collect::<Vec<_>>();methods.sort();(path.clone(),methods)}).collect::<std::collections::BTreeMap<_,_>>();
    let mut schemas=schema["components"]["schemas"].as_object().into_iter().flatten().map(|(name,_)|name.clone()).collect::<Vec<_>>();schemas.sort();format!("{:x}",Sha256::digest(json!({"paths":paths,"schemas":schemas}).to_string().as_bytes()))
}
