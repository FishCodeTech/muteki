//! Continue a conversation once its provider quota resets.
//!
//! An engine that rejects a turn for an exhausted usage window leaves a
//! `quota_hold` on the thread. `conversation.thread.quota_resume` turns that
//! hold into a `quota_resume` schedule (or cancels it), and a service loop
//! resumes due schedules through the normal `conversation.thread.resume`
//! command, so it runs with no window open and with the usual state checks.

use crate::{
    AppState,
    chat::{self, Failure, fail_code},
};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub const POLL: Duration = Duration::from_secs(30);
/// Resume a little after the reported reset so the provider has rolled over.
const GRACE_SECONDS: f64 = 20.0;
const MAX_AHEAD_SECONDS: f64 = 8.0 * 86400.0;
const MAX_PAST_SECONDS: f64 = 86400.0;
const PREFERENCES: (&str, &str) = ("settings", "conversation_preferences");

pub fn now() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

fn last_turn(v: &Value) -> Option<&Value> {
    v["turns"].as_array()?.iter().filter(|t| t["status"] != "superseded").last()
}

fn archived(v: &Value) -> bool {
    v["thread"]["archived"] == true || v["thread"]["state"]["status"] == "archived"
}

pub(crate) fn apply(s: &AppState, tid: &str, v: &mut Value, payload: &Value, at: f64) -> Result<Value, Failure> {
    let Some(enabled) = payload["enabled"].as_bool() else {
        return Err(fail_code(StatusCode::BAD_REQUEST, "conversation.quota_resume.enabled_required", "payload.enabled 必须是布尔值"));
    };
    let schedule = if enabled {
        if archived(v) {
            return Err(fail_code(StatusCode::CONFLICT, "conversation.thread.archived", "已归档的对话不能自动继续"));
        }
        let last = last_turn(v).filter(|t| t["status"] == "failed").ok_or_else(|| {
            fail_code(StatusCode::CONFLICT, "conversation.quota_resume.not_failed", "只有因额度失败的最新 Turn 可以安排自动继续")
        })?;
        let turn = last["turn_id"].as_str().unwrap_or("").to_owned();
        if payload["turn_id"].as_str().is_some_and(|requested| !requested.is_empty() && requested != turn) {
            return Err(fail_code(StatusCode::CONFLICT, "conversation.quota_resume.turn_changed", "最新 Turn 已变化，请刷新后重试"));
        }
        let resets_at = payload["resets_at"]
            .as_f64()
            .filter(|r| r.is_finite() && (at - MAX_PAST_SECONDS..=at + MAX_AHEAD_SECONDS).contains(r))
            .ok_or_else(|| {
                fail_code(
                    StatusCode::BAD_REQUEST,
                    "conversation.quota_resume.resets_at_invalid",
                    "resets_at 必须是过去 1 天到未来 8 天内的 Unix 秒级时间戳",
                )
            })?;
        json!({
            "turn_id": turn, "resets_at": resets_at, "resume_at": resets_at + GRACE_SECONDS,
            "kind": payload["kind"].as_str().unwrap_or(""), "scheduled_at": at,
        })
    } else {
        Value::Null
    };
    let state = &mut v["thread"]["state"];
    state["quota_resume"] = schedule.clone();
    if state["quota_hold"].is_object() {
        state["quota_hold"]["handled"] = json!(true);
    }
    let reason = payload["reason"]
        .as_str()
        .filter(|r| !r.is_empty())
        .unwrap_or(if enabled { "scheduled" } else { "cancelled" })
        .to_owned();
    chat::emit(s, tid, v, "core.thread.quota_resume_set", json!({"thread_id": tid, "schedule": schedule, "reason": reason}));
    Ok(json!({"thread_id": tid, "schedule": schedule}))
}

/// A new turn or archiving settles whatever the quota left pending.
pub(crate) fn clear(v: &mut Value) {
    if let Some(state) = v["thread"]["state"].as_object_mut() {
        state.remove("quota_hold");
        state.remove("quota_resume");
    }
}

pub fn auto_resume_enabled(s: &AppState) -> rusqlite::Result<bool> {
    Ok(preferences(s)?["auto_resume_on_quota_reset"] == true)
}

pub fn preferences(s: &AppState) -> rusqlite::Result<Value> {
    let stored = s.store.get(PREFERENCES.0, PREFERENCES.1)?.unwrap_or_default();
    Ok(json!({
        "version": stored["version"].as_u64().unwrap_or(0),
        "auto_resume_on_quota_reset": stored["auto_resume_on_quota_reset"] == true,
    }))
}

/// Store the preference; `None` when `expected` is not the current version.
pub fn write_preferences(s: &AppState, enabled: bool, expected: Option<u64>) -> rusqlite::Result<Option<Value>> {
    let current = preferences(s)?;
    let version = current["version"].as_u64().unwrap_or(0);
    if expected.is_some_and(|expected| expected != version) {
        return Ok(None);
    }
    let next = json!({"version": version + 1, "auto_resume_on_quota_reset": enabled});
    s.store.put(PREFERENCES.0, PREFERENCES.1, &next)?;
    Ok(Some(next))
}

fn error_code(failure: &Failure) -> String {
    failure.1 .0["error"]["code"].as_str().unwrap_or("unknown").to_owned()
}

async fn dispatch(s: &Arc<AppState>, tid: &str, body: Value) -> Result<Value, Failure> {
    chat::command(State(s.clone()), Path(tid.to_owned()), Json(body)).await.map(|Json(v)| v)
}

/// One pass: schedule unhandled holds when the preference is on, then resume
/// every due schedule. Returns what each thread did, for logs and tests.
pub async fn run_pass(s: &Arc<AppState>, at: f64) -> Vec<Value> {
    let ids = match s.store.quota_threads() {
        Ok(ids) => ids,
        Err(error) => {
            tracing::error!(%error, "Could not list quota holds");
            return Vec::new();
        }
    };
    let auto = match auto_resume_enabled(s) {
        Ok(enabled) => enabled,
        Err(error) => {
            return vec![json!({"action": "read_preferences", "error": error.to_string()})];
        }
    };
    let mut outcomes = Vec::new();
    for tid in ids {
        let Ok(Some(v)) = s.store.get(chat::COLLECTION, &tid) else { continue };
        if archived(&v) || v["thread"]["state"]["status"] == "running" {
            continue;
        }
        let state = &v["thread"]["state"];
        let hold = &state["quota_hold"];
        let last = last_turn(&v).cloned().unwrap_or_default();
        if auto
            && state["quota_resume"].is_null()
            && hold.is_object()
            && hold["handled"] != true
            && last["status"] == "failed"
            && hold["turn_id"] == last["turn_id"]
        {
            let turn = hold["turn_id"].as_str().unwrap_or("");
            let result = dispatch(s, &tid, json!({
                "command_id": format!("quota-auto:{turn}"),
                "command_type": "conversation.thread.quota_resume",
                "payload": {"enabled": true, "turn_id": turn, "resets_at": hold["resets_at"], "kind": hold["kind"], "reason": "auto_pref"},
            }))
            .await;
            let mut outcome = json!({"thread_id": tid, "turn_id": turn, "action": "schedule"});
            if let Err(failure) = &result {
                outcome["error"] = json!(error_code(failure));
                tracing::info!(thread = %tid, code = %error_code(failure), "Quota auto-schedule rejected");
            }
            outcomes.push(outcome);
        }
        let Ok(Some(v)) = s.store.get(chat::COLLECTION, &tid) else { continue };
        let schedule = &v["thread"]["state"]["quota_resume"];
        if !schedule.is_object() || schedule["resume_at"].as_f64().unwrap_or(f64::MAX) > at {
            continue;
        }
        let turn = schedule["turn_id"].as_str().unwrap_or("").to_owned();
        let result = dispatch(s, &tid, json!({
            "command_id": format!("quota-resume:{turn}"),
            "command_type": "conversation.thread.resume",
            "payload": {},
        }))
        .await;
        let mut outcome = json!({"thread_id": tid, "turn_id": turn, "action": "resume"});
        if let Err(failure) = &result {
            let code = error_code(failure);
            outcome["error"] = json!(code);
            let _ = dispatch(s, &tid, json!({
                "command_id": format!("quota-resume-clear:{turn}"),
                "command_type": "conversation.thread.quota_resume",
                "payload": {"enabled": false, "reason": format!("resume_rejected:{code}")},
            }))
            .await;
        }
        outcomes.push(outcome);
    }
    outcomes
}

/// The hold an exhausted account implies: the latest reset among the windows
/// it has fully used, as the original Codex adapter derived it.
pub(crate) fn exhausted_hold(windows: &[Value], turn: &str) -> Option<Value> {
    let latest = windows
        .iter()
        .filter(|w| w["used_percent"].as_f64().is_some_and(|used| used >= 100.0))
        .filter_map(|w| w["reset_at"].as_f64().map(|reset| (reset, w["kind"].as_str().unwrap_or(""))))
        .max_by(|a, b| a.0.total_cmp(&b.0))?;
    Some(json!({"turn_id": turn, "resets_at": latest.0, "kind": latest.1, "handled": false}))
}

/// Record `hold` when its turn is still the thread's latest, failed turn.
fn place_hold(s: &AppState, tid: &str, v: &mut Value, hold: Value) -> bool {
    let current = last_turn(v).is_some_and(|t| t["status"] == "failed" && t["turn_id"] == hold["turn_id"]);
    if !current || archived(v) || v["thread"]["state"]["quota_hold"].is_object() {
        return false;
    }
    let warning = json!({"kind": "rate_limit", "message": "已达到用量上限", "turn_id": hold["turn_id"],
        "rate_limit": {"limited": true, "resets_at": hold["resets_at"], "kind": hold["kind"]}});
    v["thread"]["state"]["quota_hold"] = hold;
    chat::emit(s, tid, v, "core.runtime.warning", warning);
    true
}

/// Codex app-server ends a turn the account cannot pay for as a plain failure:
/// the vendored adapter keeps neither `codexErrorInfo` nor rate-limit pushes.
/// The account's own windows, read right after, say whether quota was the cause.
pub(crate) async fn hold_from_limits(s: Arc<AppState>, tid: String, turn: String, credential_id: String) {
    let Ok(limits) = s.limits.read(&s, true, Some(&credential_id)).await else { return };
    let hold = limits["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["status"] == "ok" && row["stale"] != true)
        .find_map(|row| exhausted_hold(row["windows"].as_array().map(Vec::as_slice).unwrap_or(&[]), &turn));
    let Some(hold) = hold else { return };
    let _guard = s.chat.active.lock().await;
    let Ok(mut v) = chat::load(&s, &tid) else { return };
    if place_hold(&s, &tid, &mut v, hold)
        && let Err(error) = chat::save(&s, &tid, &mut v)
    {
        tracing::error!(?error, thread = %tid, "Could not record a quota hold");
    }
}

pub fn spawn_loop(s: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            if s.admission.draining() || crate::business::require(&s,crate::business::Module::Chat).is_err(){tokio::time::sleep(std::time::Duration::from_millis(250)).await;continue;}
            for outcome in run_pass(&s, now()).await {
                tracing::info!(%outcome, "Quota resume pass");
            }
            tokio::time::sleep(POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(s: &AppState, tid: &str, runtime: Value, turn_status: &str, hold: Value) {
        let doc = json!({
            "thread": {"thread_id": tid, "title": "t", "state": {"status": "idle", "quota_hold": hold}},
            "runtime": runtime, "runtime_epoch": s.chat.epoch(),
            "turns": [{"turn_id": "t1", "seq": 1, "kind": "send", "status": turn_status, "text": "hi"}],
            "messages": [], "event_seq": 0,
        });
        s.store.put(chat::COLLECTION, tid, &doc).unwrap();
    }

    fn state() -> Arc<AppState> {
        Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()))
    }

    async fn issue(s: &Arc<AppState>, tid: &str, id: &str, payload: Value) -> Result<Value, Failure> {
        dispatch(s, tid, json!({"command_id": id, "command_type": "conversation.thread.quota_resume", "payload": payload})).await
    }

    #[test]
    fn exhausted_windows_place_a_hold_on_the_latest_failed_turn_only() {
        let windows = vec![
            json!({"id": "primary", "kind": "session", "used_percent": 100.0, "reset_at": 1_800.0}),
            json!({"id": "secondary", "kind": "weekly", "used_percent": 100.0, "reset_at": 9_000.0}),
        ];
        let hold = exhausted_hold(&windows, "t1").unwrap();
        assert_eq!((hold["resets_at"].as_f64(), hold["kind"].as_str()), (Some(9_000.0), Some("weekly")));
        assert!(exhausted_hold(&[json!({"kind": "weekly", "used_percent": 99.0, "reset_at": 9_000.0})], "t1").is_none());

        let s = state();
        thread(&s, "q", json!({}), "completed", Value::Null);
        let mut v = s.store.get(chat::COLLECTION, "q").unwrap().unwrap();
        assert!(!place_hold(&s, "q", &mut v, hold.clone()), "a completed turn was not refused for quota");
        thread(&s, "q", json!({}), "failed", Value::Null);
        let mut v = s.store.get(chat::COLLECTION, "q").unwrap().unwrap();
        assert!(!place_hold(&s, "q", &mut v, exhausted_hold(&windows, "older").unwrap()));
        assert!(place_hold(&s, "q", &mut v, hold));
        assert_eq!(v["thread"]["state"]["quota_hold"]["resets_at"], 9_000.0);
    }

    #[tokio::test]
    async fn schedules_require_a_failed_latest_turn_and_a_plausible_reset() {
        let s = state();
        let at = now();
        thread(&s, "a", json!({}), "completed", Value::Null);
        let err = issue(&s, "a", "c1", json!({"enabled": true, "resets_at": at + 60.0})).await.unwrap_err();
        assert_eq!(error_code(&err), "conversation.quota_resume.not_failed");

        thread(&s, "b", json!({}), "failed", json!({"turn_id": "t1", "resets_at": at + 60.0, "kind": "usage_limit", "handled": false}));
        let err = issue(&s, "b", "c2", json!({"enabled": true, "resets_at": at + 30.0 * 86400.0})).await.unwrap_err();
        assert_eq!(error_code(&err), "conversation.quota_resume.resets_at_invalid");
        let err = issue(&s, "b", "c3", json!({"enabled": true, "turn_id": "other", "resets_at": at})).await.unwrap_err();
        assert_eq!(error_code(&err), "conversation.quota_resume.turn_changed");
        assert_eq!(error_code(&issue(&s, "b", "c4", json!({})).await.unwrap_err()), "conversation.quota_resume.enabled_required");

        issue(&s, "b", "c5", json!({"enabled": true, "turn_id": "t1", "resets_at": at + 60.0, "kind": "usage_limit"})).await.unwrap();
        let v = s.store.get(chat::COLLECTION, "b").unwrap().unwrap();
        let schedule = &v["thread"]["state"]["quota_resume"];
        assert_eq!(schedule["turn_id"], "t1");
        assert_eq!(schedule["resume_at"].as_f64().unwrap(), at + 60.0 + GRACE_SECONDS);
        assert_eq!(v["thread"]["state"]["quota_hold"]["handled"], true);
        let events = s.store.chat_events("b", None, 0, i64::MAX as u64, 16).unwrap();
        assert!(events.iter().any(|e| e["event_type"] == "core.thread.quota_resume_set"));

        issue(&s, "b", "c6", json!({"enabled": false})).await.unwrap();
        let v = s.store.get(chat::COLLECTION, "b").unwrap().unwrap();
        assert!(v["thread"]["state"]["quota_resume"].is_null());
    }

    #[tokio::test]
    async fn the_pass_schedules_holds_only_when_the_preference_is_on_and_clears_rejected_resumes() {
        let s = state();
        let at = now();
        thread(&s, "x", json!({"credential_id": "account:missing", "model": "m"}), "failed",
            json!({"turn_id": "t1", "resets_at": at - 120.0, "kind": "usage_limit", "handled": false}));
        assert!(run_pass(&s, at).await.is_empty(), "the preference is off by default");

        write_preferences(&s, true, Some(0)).unwrap().unwrap();
        assert!(write_preferences(&s, false, Some(0)).unwrap().is_none(), "a stale version is refused");
        let outcomes = run_pass(&s, at).await;
        assert_eq!(outcomes[0]["action"], "schedule");
        assert!(outcomes[0].get("error").is_none());
        assert_eq!(outcomes[1]["action"], "resume");
        assert!(outcomes[1]["error"].is_string());
        let v = s.store.get(chat::COLLECTION, "x").unwrap().unwrap();
        assert!(v["thread"]["state"]["quota_resume"].is_null());
        assert_eq!(v["thread"]["state"]["quota_hold"]["handled"], true);
        assert!(run_pass(&s, at).await.is_empty(), "a handled hold is not scheduled again");
    }

    #[tokio::test]
    async fn a_due_schedule_resumes_the_thread_and_settles_the_hold() {
        let s = state();
        s.store
            .put("credential_accounts", "fixture", &json!({"id": "fixture", "secret": "synthetic", "base_url": "http://127.0.0.1:9/v1", "models": ["m"], "default_model": "m"}))
            .unwrap();
        let at = now();
        thread(&s, "r", json!({"credential_id": "account:fixture", "model": "m"}), "failed",
            json!({"turn_id": "t1", "resets_at": at - 60.0, "kind": "usage_limit", "handled": false}));
        issue(&s, "r", "c1", json!({"enabled": true, "turn_id": "t1", "resets_at": at - 60.0})).await.unwrap();
        let outcomes = run_pass(&s, at).await;
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].get("error").is_none(), "{outcomes:?}");
        let v = s.store.get(chat::COLLECTION, "r").unwrap().unwrap();
        assert_eq!(v["turns"].as_array().unwrap().last().unwrap()["kind"], "resume");
        assert!(v["thread"]["state"].get("quota_resume").is_none_or(Value::is_null));
        assert!(v["thread"]["state"].get("quota_hold").is_none_or(Value::is_null));
    }
}
