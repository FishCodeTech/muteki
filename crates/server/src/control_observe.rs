//! Read-only `Observe` tools for `muteki-control`: cross-thread `@` references,
//! Git status & commit log, GitHub PR status/checks/comments, terminal screen
//! buffer inspection, review-candidate triage, and non-sensitive settings view.

use crate::{
    AppState, chat, control, control_content, github_pr,
    workspace_read_api::{bound_root, inspect_git_status, project_bound_root, status_occupancy},
};
use serde_json::{Value, json};
use std::sync::Arc;

/// `muteki_reference_thread`: record a persistent cross-thread `@` reference with boundary notices.
pub fn reference_thread(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let raw_target = args["target_thread_id"]
        .as_str()
        .or_else(|| args["thread_id"].as_str())
        .map(str::trim)
        .unwrap_or("");
    let target = control::resolve_thread_id(caller, raw_target);
    if target.is_empty() || target == caller {
        return Err(json!({
            "code": "control.reference.invalid_target",
            "category": "validation",
            "message": "target_thread_id must name another existing thread"
        }));
    }
    let reason = args["reason"].as_str().unwrap_or("");
    let anchors: Vec<String> = args["anchors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let record = control::attach_reference(s, caller, target, reason, anchors, caller)
        .map_err(|(_, axum::Json(b))| b["error"].clone())?;
    Ok(json!({
        "reference": record,
        "ui_chip": format!("@{}", record.target_title),
        "privacy_notice": record.boundary_notice,
    }))
}

/// `muteki_get_git`: inspect branch, porcelain file changes, ahead/behind, occupancy, and recent commits.
pub async fn get_git(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let recent_limit = args["recent_commits"]
        .as_u64()
        .unwrap_or(8)
        .clamp(1, 30);
    let project_id = args["project_id"]
        .as_str()
        .map(str::trim)
        .filter(|p| !p.is_empty());

    let (root, scope_label, exclude_tid) = if let Some(pid) = project_id {
        let root = project_bound_root(s, pid).map_err(|(_, axum::Json(b))| b["error"].clone())?;
        (root, json!({"project_id": pid}), "")
    } else {
        let tid = control::resolve_thread_id(
            caller,
            args["thread_id"].as_str().unwrap_or(control::OWN_THREAD),
        );
        let root = bound_root(s, tid).map_err(|(_, axum::Json(b))| b["error"].clone())?;
        (root, json!({"thread_id": tid}), tid)
    };

    let mut status = inspect_git_status(&root)
        .await
        .map_err(|(_, axum::Json(b))| b["error"].clone())?;
    status_occupancy(s, &root, exclude_tid, &mut status)
        .map_err(|(_, axum::Json(b))| b["error"].clone())?;
    status["target"] = scope_label;

    if status["is_repo"] == true {
        if let Ok(porcelain) = crate::git_process::run(
            &root,
            &["status", "--porcelain=v1", "-uall", "--", "."],
            20,
            "conversation.git.error",
            false,
        )
        .await
            && porcelain.status.success()
        {
            let mut staged = Vec::<String>::new();
            let mut unstaged = Vec::<String>::new();
            let mut untracked = Vec::<String>::new();
            for line in porcelain.stdout.lines().take(300) {
                if line.len() < 4 {
                    continue;
                }
                let bytes = line.as_bytes();
                let x = bytes[0] as char;
                let y = bytes[1] as char;
                let path = line[3..].trim().to_owned();
                if x == '?' && y == '?' {
                    untracked.push(path);
                } else {
                    if x != ' ' && x != '?' {
                        staged.push(format!("{x} {path}"));
                    }
                    if y != ' ' && y != '?' {
                        unstaged.push(format!("{y} {path}"));
                    }
                }
            }
            status["files"] = json!({
                "staged": staged,
                "unstaged": unstaged,
                "untracked": untracked,
            });
        }

        let count_arg = format!("-{recent_limit}");
        if let Ok(log_out) = crate::git_process::run(
            &root,
            &[
                "log",
                "--no-show-signature",
                &count_arg,
                "--format=%H%x1f%h%x1f%an%x1f%aI%x1f%s",
            ],
            20,
            "conversation.git.error",
            false,
        )
        .await
            && log_out.status.success()
        {
            let commits: Vec<Value> = log_out
                .stdout
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.split('\x1f').collect();
                    if parts.len() >= 5 {
                        Some(json!({
                            "sha": parts[0],
                            "short_sha": parts[1],
                            "author": parts[2],
                            "authored_at": parts[3],
                            "subject": parts[4],
                        }))
                    } else {
                        None
                    }
                })
                .collect();
            status["recent_commits"] = json!(commits);
        }
    }

    Ok(status)
}

/// `muteki_get_pull_request`: inspect GitHub PR status, CI check runs, comments, and diff stats.
pub async fn get_pull_request(
    s: &Arc<AppState>,
    caller: &str,
    args: &Value,
) -> Result<Value, Value> {
    let tid = control::resolve_thread_id(
        caller,
        args["thread_id"].as_str().unwrap_or(control::OWN_THREAD),
    );
    let number = args["number"].as_u64();
    let include_comments = args["include_comments"].as_bool().unwrap_or(true);
    let include_checks = args["include_checks"].as_bool().unwrap_or(true);
    github_pr::inspect_thread_prs(s, tid, number, include_comments, include_checks)
        .await
        .map_err(|(_, axum::Json(b))| b["error"].clone())
}

/// `muteki_read_terminal`: read redacted PTY history buffer for a thread's embedded terminal(s).
pub fn read_terminal(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let tid = control::resolve_thread_id(
        caller,
        args["thread_id"].as_str().unwrap_or(control::OWN_THREAD),
    );
    let max_bytes = args["max_bytes"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or(16_384)
        .clamp(512, 65_536);
    let session_id = args["session_id"]
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty());

    let sessions = s.terminals.list(tid);
    let session_list: Vec<Value> = sessions.iter().map(|sess| sess.public()).collect();
    if sessions.is_empty() {
        return Ok(json!({
            "thread_id": tid,
            "sessions": [],
            "selected_session": null,
            "quoted_output": null,
        }));
    }

    let chosen = if let Some(sid) = session_id {
        s.terminals.get(tid, sid).ok_or_else(|| {
            json!({
                "code": "control.terminal.not_found",
                "category": "not_found",
                "message": format!("terminal session {sid} does not exist on thread {tid}")
            })
        })?
    } else {
        sessions
            .iter()
            .find(|sess| sess.is_running())
            .or_else(|| sessions.last())
            .cloned()
            .unwrap()
    };

    let meta = chosen.public();
    let sid = meta["session_id"].as_str().unwrap_or("terminal");
    let (raw_text, truncated) = chosen.read_history_text(max_bytes);
    let (fenced, redactions) =
        control_content::fence_thread_data(tid, &format!("terminal:{sid}"), &raw_text);

    Ok(json!({
        "thread_id": tid,
        "sessions": session_list,
        "selected_session": meta,
        "history_truncated": truncated,
        "redacted_secret_count": redactions,
        "quoted_output": fenced,
    }))
}

/// `muteki_review_candidates`: find unsettled threads that finished running, await input/approval,
/// were flagged `follow_up`, or went stale.
pub fn review_candidates(s: &Arc<AppState>, caller: &str, args: &Value) -> Result<Value, Value> {
    let stale_hours = args["stale_hours"].as_u64().unwrap_or(24).clamp(1, 720) as i64;
    let limit = args["limit"].as_u64().unwrap_or(30).clamp(1, 100) as usize;
    let project_filter = args["project_id"]
        .as_str()
        .map(str::trim)
        .filter(|p| !p.is_empty());

    let (grant, _) =
        control::effective(s, caller).map_err(|(_, axum::Json(b))| b["error"].clone())?;
    let caller_project = control::thread_meta(s, caller)
        .map(|(_, p)| p)
        .unwrap_or_default();
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(stale_hours);

    let ancestors = control::ancestor_restrictions(s, caller)?;
    let mut candidates = Vec::<Value>::new();
    for doc in s.store.list(chat::COLLECTION).unwrap_or_default() {
        let thread = &doc["thread"];
        if thread["mode"] != "conversation"
            || thread["archived"] == true
            || chat::is_ctf_thread(thread)
            || chat::is_side_chat(thread)
        {
            continue;
        }
        let Some(tid) = thread["thread_id"].as_str() else {
            continue;
        };
        if tid == caller || ancestors.contains(tid) {
            continue;
        }
        let pid = thread["project_id"].as_str().unwrap_or("");
        if let Some(fp) = project_filter
            && pid != fp
        {
            continue;
        }
        if !crate::chat_subagents::is_descendant_of(s, tid, caller)
            && !grant.scope.reaches(&caller_project, pid)
        {
            continue;
        }

        let ctrl_doc = control::thread_doc(s, tid).unwrap_or(Value::Null);
        let lifecycle = control::read_lifecycle(&ctrl_doc);
        if lifecycle.settled {
            continue;
        }
        if let Some(snooze) = &lifecycle.snoozed_until
            && let Ok(ts) = chrono::DateTime::parse_from_rfc3339(snooze)
            && ts.with_timezone(&chrono::Utc) > chrono::Utc::now()
        {
            continue;
        }

        let status = thread["state"]["status"].as_str().unwrap_or("idle");
        let has_pending_approval = thread["state"]["pending_approvals"]
            .as_object()
            .is_some_and(|m| m.values().any(|v| v["status"] == "pending"));
        let has_pending_input = thread["state"]["pending_user_input"]["status"] == "pending";
        let turn_count = doc["turns"].as_array().map_or(0, Vec::len);
        let updated_at = thread["updated_at"].as_str().unwrap_or("");
        let is_stale = chrono::DateTime::parse_from_rfc3339(updated_at)
            .map(|dt| dt.with_timezone(&chrono::Utc) < cutoff)
            .unwrap_or(false);

        let mut reasons = Vec::<&'static str>::new();
        if has_pending_approval {
            reasons.push("needs_approval");
        }
        if has_pending_input {
            reasons.push("needs_input");
        }
        if lifecycle.follow_up {
            reasons.push("follow_up");
        }
        if status == "idle" && turn_count > 0 && ctrl_doc["review"].is_null() {
            reasons.push("completed_unsettled");
        }
        if is_stale && turn_count > 0 {
            reasons.push("stale");
        }
        if reasons.is_empty() {
            continue;
        }

        candidates.push(json!({
            "thread_id": tid,
            "title": thread["title"],
            "project_id": pid,
            "status": status,
            "turn_count": turn_count,
            "updated_at": updated_at,
            "reasons": reasons,
            "lifecycle": lifecycle,
            "review": ctrl_doc["review"],
            "is_subagent": thread["state"]["lineage"]["origin"] == "app_owned",
        }));
    }

    candidates.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));
    let total = candidates.len();
    candidates.truncate(limit);
    Ok(json!({
        "candidates": candidates,
        "total": total,
        "returned": candidates.len(),
    }))
}

/// `muteki_get_settings`: read non-sensitive `muteki-control` settings and caller grant.
pub fn get_settings_tool(s: &Arc<AppState>, caller: &str) -> Result<Value, Value> {
    let cfg = control::settings(s).map_err(|(_, axum::Json(b))| b["error"].clone())?;
    let resolved = cfg.orchestration_for_mode(control::thread_orchestration_mode(s, caller));
    let (caller_grant, caller_source) =
        control::effective(s, caller).map_err(|(_, axum::Json(b))| b["error"].clone())?;
    Ok(json!({
        "caller_thread_id": caller,
        "workbench_mode": resolved.mode,
        "orchestration_source": resolved.source,
        "effective_grant": caller_grant,
        "grant_source": caller_source,
        "subagent_policy": cfg.subagent_policy,
        "orchestration": resolved.profile,
        "orchestration_default": cfg.orchestration,
        "orchestration_overrides": cfg.orchestration_overrides,
    }))
}
