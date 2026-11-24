//! The upstream numeric terminal interface over Muteki's live PTYs.
use crate::{
    AppState, chat, control,
    control_contract::{ReadTerminalParams, TerminalOp, TerminalParams},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, OnceLock};
fn error(message: impl ToString) -> Value {
    json!({"code":"control.terminal_failed","message":message.to_string()})
}
fn terminal_id(s: &AppState, thread: &str, session: &str) -> Result<u64, Value> {
    static IDS: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = IDS.get_or_init(|| Mutex::new(())).lock().map_err(error)?;
    for row in s.store.list("control_terminals").map_err(error)? {
        if row["session_id"] == session {
            if let Some(id) = row["id"].as_u64() {
                return Ok(id);
            }
        }
    }
    let next = s
        .store
        .get("control_terminals", "next")
        .map_err(error)?
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
        + 1;
    s.store
        .put("control_terminals", "next", &json!(next))
        .map_err(error)?;
    s.store
        .put(
            "control_terminals",
            &next.to_string(),
            &json!({"id":next,"thread_id":thread,"session_id":session}),
        )
        .map_err(error)?;
    Ok(next)
}
pub fn list(s: &AppState, thread: &str) -> Result<Vec<Value>, Value> {
    s.terminals
        .list(thread)
        .iter()
        .map(|session| {
            let public = session.public();
            let id = terminal_id(s, thread, public["session_id"].as_str().unwrap_or(""))?;
            Ok(json!({"id":id,"label":public["name"]}))
        })
        .collect()
}
fn get(
    s: &AppState,
    caller: &str,
    id: u64,
) -> Result<(String, Arc<crate::terminal::TerminalSession>), Value> {
    let row = s
        .store
        .get("control_terminals", &id.to_string())
        .map_err(error)?
        .ok_or_else(|| error(format!("unknown terminal: {id}")))?;
    let owner = row["thread_id"]
        .as_str()
        .ok_or_else(|| error("Terminal has no owner"))?;
    let (grant, _) = control::effective(s, caller).map_err(|(_, b)| b.0["error"].clone())?;
    control::check_target(s, caller, owner, &grant)?;
    let session = s
        .terminals
        .get(owner, row["session_id"].as_str().unwrap_or(""))
        .ok_or_else(|| error(format!("terminal {id} is not running")))?;
    Ok((owner.to_owned(), session))
}
pub fn act(s: &Arc<AppState>, caller: &str, p: TerminalParams) -> Result<Value, Value> {
    let target = control::resolve_thread_id(caller, &p.thread_id);
    let (grant, _) = control::effective(s, caller).map_err(|(_, b)| b.0["error"].clone())?;
    control::check_target(s, caller, target, &grant)?;
    let result = match p.op {
        TerminalOp::Open => {
            let doc = chat::load(s, target).map_err(|(_, b)| b.0["error"].clone())?;
            let cwd = crate::native_chat::session_cwd(s, &doc, target).map_err(error)?;
            let session = s
                .terminals
                .create_control(
                    target,
                    doc["thread"]["workspace_id"].as_str().unwrap_or(""),
                    &cwd.to_string_lossy(),
                )
                .map_err(|(_, b)| b.0["error"].clone())?;
            let public = session.public();
            let id = terminal_id(s, target, public["session_id"].as_str().unwrap_or(""))?;
            json!({"thread_id":target,"terminal_id":id,"status":"opened"})
        }
        TerminalOp::Input { terminal_id, text } => {
            let (owner, session) = get(s, caller, terminal_id)?;
            if owner != target {
                return Err(error("terminal belongs to another thread"));
            }
            session
                .write_input_str(&text)
                .map_err(|error| json!({"code":error.code,"message":error.message}))?;
            json!({"thread_id":target,"terminal_id":terminal_id,"status":"input_sent"})
        }
        TerminalOp::Close { terminal_id } => {
            let (owner, session) = get(s, caller, terminal_id)?;
            if owner != target {
                return Err(error("terminal belongs to another thread"));
            }
            session.close_session("closed by control");
            json!({"thread_id":target,"terminal_id":terminal_id,"status":"closed"})
        }
    };
    control::audit(s, caller, target, "terminal", result.clone());
    Ok(result)
}
pub fn read(s: &Arc<AppState>, caller: &str, p: ReadTerminalParams) -> Result<Value, Value> {
    let (owner, session) = get(s, caller, p.terminal_id)?;
    let (text, exited, exit_code) = session
        .control_screen()
        .ok_or_else(|| error("terminal is closed"))?;
    let mut omitted = vec!["scrollback above the visible screen is not included".to_owned()];
    let shown = if text.len() > 65536 {
        let start = text.ceil_char_boundary(text.len() - 65536);
        omitted.push("the screen was cut to its last 65536 bytes".into());
        &text[start..]
    } else {
        &text
    };
    let (clean, redacted) = crate::control_index::redact(shown);
    let doc = chat::load(s, &owner).map_err(|(_, b)| b.0["error"].clone())?;
    Ok(
        json!({"terminal_id":p.terminal_id,"thread_id":owner,"title":doc["thread"]["title"],"exited":exited,"exit_code":exit_code,"text":crate::control_index::fence(&format!("terminal {}",p.terminal_id),&clean),"redacted":redacted,"omitted":omitted}),
    )
}
