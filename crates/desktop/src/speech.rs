//! One real Apple recognizer, owned by the originating window/document and service scope.
use crate::{native::{self,Notice},policy::{BridgeError,Outcome}};
use serde_json::{Value,json};
use std::{ffi::CString,sync::Mutex};
#[derive(Clone)]struct Recording{window:String,document:String,route:String,scope_version:u64,owner:Value}
static ACTIVE:Mutex<Option<Recording>>=Mutex::new(None);
unsafe extern "C"{fn muteki_speech_start(locale:*const std::ffi::c_char,callback:extern "C" fn(*const u8,usize))->i32;fn muteki_speech_finish();fn muteki_speech_cancel();}
extern "C" fn receive(bytes:*const u8,length:usize){
    if bytes.is_null()||length>1024*1024{return}let raw=unsafe{std::slice::from_raw_parts(bytes,length)};
    let mut event:Value=serde_json::from_slice(raw).unwrap_or_else(|_|json!({"type":"error","code":"desktop.speech.invalid_event","message":"原生语音识别返回无效事件。"}));
    let mut active=ACTIVE.lock().unwrap_or_else(|error|error.into_inner());let Some(recording)=active.as_ref().cloned()else{return};
    if !matches!(event["type"].as_str(),Some("requesting"|"listening"|"partial"|"processing"|"result"|"error"|"cancelled")){event=json!({"type":"error","code":"desktop.speech.invalid_event","message":"原生语音识别返回无效事件。"});}
    for field in ["id","scopeKey","connectionVersion","serviceId","identityId"]{event[field]=recording.owner[field].clone();}
    if matches!(event["type"].as_str(),Some("result"|"error"|"cancelled")){active.take();}drop(active);
    native::notify(Notice::Speech(recording.window,recording.document,event));
}
fn matches(recording:&Recording,window:&str,document:&str,id:&str)->bool{recording.window==window&&recording.document==document&&recording.owner["id"]==id}
pub fn start(window:&str,document:&str,route:&str,scope_version:u64,input:&Value)->Outcome{
    let id=input["id"].as_str().unwrap_or("");let locale=input["locale"].as_str().unwrap_or("");
    if !(16..=80).contains(&id.len())||!id.bytes().all(|byte|byte.is_ascii_alphanumeric()||byte==b'-')||input["scopeKey"].as_str().is_none_or(str::is_empty)||!locale.split('-').enumerate().all(|(index,part)|if index==0{(2..=3).contains(&part.len())&&part.bytes().all(|byte|byte.is_ascii_alphabetic())}else{(2..=8).contains(&part.len())&&part.bytes().all(|byte|byte.is_ascii_alphanumeric())}){return Err(BridgeError::new("desktop.speech.invalid_request","语音输入请求无效。"));}
    let previous=ACTIVE.lock().unwrap_or_else(|error|error.into_inner()).clone();if let Some(previous)=previous{if previous.window!=window{return Err(BridgeError::new("desktop.speech.busy","另一个窗口正在使用麦克风，请先结束该录音。"));}unsafe{muteki_speech_cancel();}}
    let language=CString::new(locale).map_err(|_|BridgeError::new("desktop.speech.invalid_request","语音识别语言无效。"))?;
    *ACTIVE.lock().unwrap_or_else(|error|error.into_inner())=Some(Recording{window:window.into(),document:document.into(),route:route.into(),scope_version,owner:input.clone()});
    let code=unsafe{muteki_speech_start(language.as_ptr(),receive)};
    if code!=0{ACTIVE.lock().unwrap_or_else(|error|error.into_inner()).take();return Err(BridgeError::new(match code{1=>"desktop.speech.usage_description_missing",2=>"desktop.speech.busy",_=>"desktop.speech.start_failed"},match code{1=>"请使用包含麦克风与语音识别用途声明的 Muteki 安装包。",2=>"已有原生录音正在进行。",_=>"原生语音识别启动失败。"}));}Ok(json!({"id":id}))
}
pub fn finish(window:&str,document:&str,input:&Value)->Outcome{let id=input["id"].as_str().unwrap_or("");if !ACTIVE.lock().unwrap_or_else(|error|error.into_inner()).as_ref().is_some_and(|recording|matches(recording,window,document,id)){return Err(BridgeError::new("desktop.speech.session_changed","录音会话已结束或改变。"));}unsafe{muteki_speech_finish();}Ok(json!({"id":id}))}
pub fn cancel(window:&str,document:&str,input:&Value)->Outcome{let id=input["id"].as_str().unwrap_or("");let active=ACTIVE.lock().unwrap_or_else(|error|error.into_inner()).as_ref().is_some_and(|recording|recording.window==window&&recording.document==document&&(id.is_empty()||recording.owner["id"]==id));if active{unsafe{muteki_speech_cancel();}}Ok(Value::Null)}
pub fn cancel_window(window:&str){if ACTIVE.lock().unwrap_or_else(|error|error.into_inner()).as_ref().is_some_and(|recording|recording.window==window){unsafe{muteki_speech_cancel();}}}
pub fn context_changed(window:&str,document:&str,route:&str,scope_version:u64){let cancelled=ACTIVE.lock().unwrap_or_else(|error|error.into_inner()).as_ref().is_some_and(|recording|recording.window==window&&(recording.scope_version!=scope_version||recording.document!=document||recording.route!=route));if cancelled{unsafe{muteki_speech_cancel();}}}
