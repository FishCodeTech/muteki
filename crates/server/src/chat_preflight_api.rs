//! Read-only confirmation previews for retry, edit-resend, fork and native
//! rewind. Reads workspace status and turn checkpoints; never mutates the
//! conversation, the files or a provider session.
use crate::AppState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

type Api = Result<Json<Value>, (StatusCode, Json<Value>)>;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/api/threads/{id}/impact-preview", get(impact_preview))
}

fn fail(status: StatusCode, code: &str, message: &str) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
}

async fn impact_preview(
    State(s): State<Arc<AppState>>,
    Path(tid): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Api {
    let mode = query
        .get("mode")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("retry");
    if !matches!(mode, "retry" | "edit_resend" | "fork" | "native_rewind") {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.impact.preview_failed",
            "Unknown impact mode",
        ));
    }
    let file_mode = query.get("file_mode").map(|s| s.trim()).filter(|s| !s.is_empty()).unwrap_or("keep_files");
    if !matches!(file_mode, "keep_files" | "restore_files" | "sync_files") {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.turn.invalid_file_mode",
            "file_mode 只能是 keep_files、restore_files 或 sync_files",
        ));
    }
    let turn_id = query
        .get("turn_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            fail(
                StatusCode::BAD_REQUEST,
                "conversation.turn.id_required",
                "turn_id is required",
            )
        })?;
    let doc = s
        .store
        .get("conversation_threads", &tid)
        .map_err(|_| {
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "conversation.history.unavailable",
                "Could not read conversation history",
            )
        })?
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "conversation.thread.not_found",
                "Thread not found",
            )
        })?;
    let turns: Vec<_> = doc["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, turn)| turn["status"] != "superseded")
        .collect();
    let target_index = turns
        .iter()
        .position(|(_, turn)| turn["turn_id"] == turn_id)
        .ok_or_else(|| {
            fail(
                StatusCode::NOT_FOUND,
                "conversation.turn.not_found",
                "Turn not found in current branch",
            )
        })?;
    let (stored_index, target) = turns[target_index];
    let target_seq = target["seq"].as_u64().unwrap_or(stored_index as u64 + 1);
    let superseded = if mode == "fork" {
        &turns[..0]
    } else {
        &turns[target_index..]
    };
    let superseded_ids: Vec<_> = superseded
        .iter()
        .map(|(_, t)| t["turn_id"].clone())
        .collect();
    let superseded_seqs: Vec<_> = superseded
        .iter()
        .map(|(index, t)| t["seq"].as_u64().unwrap_or(*index as u64 + 1))
        .collect();
    let affected_ids: HashSet<_> = superseded_ids.iter().filter_map(Value::as_str).collect();
    let messages: Vec<_> = doc["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            m["turn_id"]
                .as_str()
                .is_some_and(|id| affected_ids.contains(id))
        })
        .collect();
    let mut seen = HashSet::new();
    let mut attachments = Vec::new();
    for record in superseded
        .iter()
        .map(|(_, t)| *t)
        .chain(messages.iter().copied())
    {
        for sha in record["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !seen.insert(sha) {
                continue;
            }
            let name = doc["artifacts"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| a["artifact_id"] == sha || a["sha256"] == sha)
                .and_then(|a| a["name"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| sha.chars().take(12).collect());
            let artifact_id = doc["artifacts"].as_array().into_iter().flatten()
                .find(|a| a["artifact_id"] == sha)
                .map(|a| a["artifact_id"].clone()).unwrap_or(Value::Null);
            let needs_reselect = artifact_id.is_null();
            attachments.push(json!({"artifact_id":artifact_id,
                "name":name,"needs_reselect":needs_reselect}));
        }
    }
    let original_text = target["text"].as_str().unwrap_or("");
    let edited_text = query
        .get("text")
        .map(String::as_str)
        .filter(|s| !s.is_empty());
    let root = crate::chat::workspace_root(&s, &doc);
    let (dirty, files) = if root.is_empty() {
        (false, json!("unknown"))
    } else {
        match crate::checkpoints::dirty_files(&root).await {
            Ok((dirty, files)) if dirty && files.is_empty() => (true, json!("unknown")),
            Ok((dirty, files)) => (dirty, json!(files)),
            Err(_) => (false, json!("unknown")),
        }
    };
    let checkpoint = if matches!(mode, "retry" | "edit_resend") {
        Some(crate::checkpoints::preview(&root, &tid, turn_id).await)
    } else {
        None
    };
    if crate::native_chat::is_native(&doc["runtime"]) {
        let mut preview = native_preview(&s, &doc, mode, file_mode, target);
        for (key, value) in [
            ("thread_id", json!(tid)), ("target_turn_id", json!(turn_id)), ("target_seq", json!(target_seq)),
            ("target_text_preview", json!(edited_text.unwrap_or(original_text).chars().take(240).collect::<String>())),
            ("edited", json!(edited_text.is_some_and(|text| !text.trim().is_empty() && text != original_text))),
            ("superseded_turn_ids", json!(superseded_ids)), ("superseded_seqs", json!(superseded_seqs)),
            ("superseded_message_count", json!(messages.len())), ("attachments_affected", json!(attachments)),
        ] {
            preview[key] = value;
        }
        preview["workspace"]["dirty"] = json!(dirty);
        preview["workspace"]["files_touched_since_target"] = files;
        preview["workspace"]["root_path"] = json!(root);
        if let Some(checkpoint) = checkpoint {
            preview["checkpoint"] = checkpoint;
        }
        return Ok(Json(preview));
    }
    let policy = if mode == "fork" {
        "fork_shares_workspace"
    } else {
        "keep_files"
    };
    let rewind_reason = "HTTP 文本对话不支持 Provider 原生回退；可使用 Fork、重试或编辑重发。";
    let honest_label = match mode {
        "fork" => {
            "Fork 复制当前分支的聊天文本和已上传附件，源会话保持原状；工作区关联保持一致，文件保持原状。"
        }
        "native_rewind" => rewind_reason,
        _ => "重新构建聊天文本上下文，原分支保留为已替代记录；工作区文件保持原状。",
    };
    let mut preview = json!({
        "thread_id":tid,"mode":mode,"target_turn_id":turn_id,"target_seq":target_seq,
        "target_text_preview":edited_text.unwrap_or(original_text).chars().take(240).collect::<String>(),
        "edited":edited_text.is_some_and(|text| !text.trim().is_empty() && text != original_text),
        "superseded_turn_ids":superseded_ids,"superseded_seqs":superseded_seqs,
        "superseded_message_count":messages.len(),"attachments_affected":attachments,
        "workspace":{"policy":policy,"dirty":dirty,"files_touched_since_target":files,"root_path":root,"shares_workspace":mode == "fork"},
        "external_side_effects":"cannot_undo",
        "provider":{"rewind_level":"unsupported","invocable":false,"reason":rewind_reason,
            "alternative":"可使用 Fork、重试或编辑重发，保留工作区文件。","source":"static"},
        "guarantees":{"conversation":if mode == "fork" {"source_unchanged"} else if mode == "native_rewind" {"unsupported"} else {"supersede_target_and_later"},
            "files":"keep_files","provider_session":if mode == "native_rewind" {"unsupported"} else {"muteki_text_reconstruct"},"honest_label":honest_label}
    });
    if let Some(checkpoint) = checkpoint {
        preview["checkpoint"] = checkpoint;
    }
    Ok(Json(preview))
}

/// Mode, workspace policy, provider rewind and guarantees for an engine
/// session thread, read from the same capability matrix the turn controls use.
fn native_preview(s: &AppState, doc: &Value, mode: &str, file_mode: &str, target: &Value) -> Value {
    let mut rewind = crate::native_chat::connection(s.chat.epoch(), doc)
        .map(|(_, rewind)| rewind)
        .unwrap_or_else(|| json!({"rewind_level":"unknown","invocable":false,"reason":"原生回退 / rewind 尚未确认","source":"static"}));
    if mode == "native_rewind"
        && rewind["invocable"] == true
        && target["agent_session_id"] != doc["thread"]["state"]["agent_session_id"]
    {
        rewind["rewind_level"] = json!("limited");
        rewind["reason"] = json!("目标属于较早的引擎会话；回退将新建会话并传入保留的聊天历史，文件保持原状");
    }
    let level = rewind["rewind_level"].as_str().unwrap_or("unknown").to_owned();
    let invocable = rewind["invocable"] == true && matches!(level.as_str(), "supported" | "limited") && rewind["stale"] != true;
    let reason = rewind["reason"].as_str().unwrap_or("").to_owned();
    let alternative = rewind["alternative"]
        .as_str()
        .filter(|text| !text.is_empty())
        .unwrap_or("可改用 Fork（保留源历史）或 Retry（保留文件，仅重建对话文本）");
    let label = match mode {
        "native_rewind" if file_mode == "sync_files" => "sync_files",
        "fork" => "fork_shares_workspace",
        _ => "keep_files",
    };
    let policy = if mode == "native_rewind" { file_mode } else { label };
    let provider_session = match mode {
        "native_rewind" if level == "limited" => "muteki_text_reconstruct",
        "native_rewind" => "native_rewind",
        "retry" | "edit_resend" => "muteki_text_reconstruct",
        _ => "new_thread_session",
    };
    let honest = match mode {
        "retry" | "edit_resend" => "Muteki 仅重建对话文本历史，不等于 Provider 原生 rewind，也不等于文件回滚".to_owned(),
        "fork" => "Fork 保留源 Thread；当前与源共享工作区".to_owned(),
        _ if !reason.is_empty() => reason.clone(),
        _ => "同步回退引擎与聊天历史，保留文件和外部操作".to_owned(),
    };
    json!({
        "mode": mode,
        "workspace": {"policy":policy,"shares_workspace":mode == "fork"},
        "external_side_effects": "cannot_undo",
        "provider": {"rewind_level":level,"reason":reason,"alternative":alternative,"invocable":invocable,"source":rewind["source"]},
        "guarantees": {
            "conversation": if mode == "fork" { "source_unchanged" } else { "supersede_target_and_later" },
            "files": label, "provider_session": provider_session, "honest_label": honest,
        },
    })
}
