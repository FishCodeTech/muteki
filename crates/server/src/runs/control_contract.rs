//! The direct and unified control routes share this admission contract.
use axum::http::StatusCode;
use serde_json::{Value, json};

pub(super) type Error = (StatusCode, Value);
fn reject(code: &str, message: impl Into<String>) -> Error {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({"ok":false,"error":{"code":format!("run.control.{code}"),"category":"validation","message":message.into()}}),
    )
}
const ACTIONS: &[&str] = &[
    "ask",
    "hint",
    "focus",
    "redirect",
    "directive",
    "correction",
    "pause",
    "freeze",
    "resume",
    "thaw",
    "stop",
    "emergency_stop",
    "complete",
    "graceful_drain",
    "force_cancel",
    "dismiss",
    "dismiss_help",
    "clear_standing",
    "reset_guidance",
    "mark_false",
    "submit",
    "writeup",
    "answer_decision",
    "add_context",
    "expire_context",
    "spawn_worker",
    "cancel_worker",
    "interrupt_turn",
    "interrupt",
    "continue",
    "send_message",
];
const RUN_ACTIONS: &[&str] = &[
    "pause",
    "resume",
    "stop",
    "emergency_stop",
    "complete",
    "graceful_drain",
    "clear_standing",
    "reset_guidance",
    "mark_false",
    "interrupt_turn",
    "interrupt",
    "continue",
];
const TERMINATED: &[&str] = &[
    "ask",
    "writeup",
    "hint",
    "focus",
    "redirect",
    "mark_false",
    "answer_decision",
    "dismiss",
    "dismiss_help",
    "clear_standing",
    "reset_guidance",
    "expire_context",
    "stop",
    "emergency_stop",
    "complete",
    "force_cancel",
    "continue",
];
fn nonempty(value: &Value) -> bool {
    value.as_str().is_some_and(|v| !v.trim().is_empty())
}

pub(super) fn normalize(run_id: &str, body: &mut Value) -> Result<(), Error> {
    if !body.is_object() {
        return Err(reject("invalid_payload", "Control body must be an object"));
    }
    if let Some(value) = body.get("run_id") {
        if value.as_str() != Some(run_id) {
            return Err(reject(
                "run_mismatch",
                "Control run_id differs from the route",
            ));
        }
    }
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("directive")
        .trim()
        .to_lowercase();
    if !ACTIONS.contains(&action.as_str()) {
        return Err(reject(
            "action_invalid",
            format!("Unknown control action {action}"),
        ));
    }
    if action == "emergency_stop" && body["expected_generation"].as_u64().is_none() {
        return Err(reject(
            "generation_invalid",
            "Emergency stop requires expected_generation",
        ));
    }
    body["action"] = json!(action);
    let raw = body
        .get("scope")
        .or_else(|| body.get("target"))
        .cloned()
        .unwrap_or_else(|| json!("global"));
    let (kind, value) = if raw.is_object() {
        if raw
            .as_object()
            .unwrap()
            .keys()
            .any(|key| !matches!(key.as_str(), "kind" | "value"))
        {
            return Err(reject("invalid_scope", "Unknown control scope field"));
        }
        (
            raw["kind"].as_str().unwrap_or("global").to_owned(),
            raw["value"].as_str().unwrap_or("").to_owned(),
        )
    } else if let Some(raw) = raw.as_str() {
        if raw.trim() == "global" {
            ("global".into(), String::new())
        } else {
            let (kind, value) = raw
                .split_once(':')
                .ok_or_else(|| reject("invalid_scope", "Control scope must include its kind"))?;
            (kind.trim().to_lowercase(), value.trim().to_owned())
        }
    } else {
        return Err(reject(
            "invalid_scope",
            "Control scope must be text or an object",
        ));
    };
    let kind = if kind == "solver" { "worker" } else { &kind };
    if !matches!(
        kind,
        "global" | "run" | "challenge" | "worker" | "intent" | "engine" | "lane"
    ) || (kind == "global" && !value.is_empty())
        || (kind != "global" && value.is_empty())
    {
        return Err(reject(
            "invalid_scope",
            "Invalid control scope kind or value",
        ));
    }
    if RUN_ACTIONS.contains(&action.as_str()) && !matches!(kind, "global" | "run" | "challenge") {
        return Err(reject(
            "invalid_scope",
            format!("{action} requires a Run scope"),
        ));
    }
    let scope = if kind == "global" {
        "global".into()
    } else {
        format!("{kind}:{value}")
    };
    body["scope"] = json!(scope);
    body["target"] = json!(scope);
    if let Some(id) = body.get("command_id") {
        if !id.is_string()
            || id
                .as_str()
                .is_none_or(|v| !(3..=128).contains(&v.trim().len()))
        {
            return Err(reject(
                "invalid_command_id",
                "command_id must contain 3–128 characters",
            ));
        }
        body["command_id"] = json!(id.as_str().unwrap().trim());
    }
    for key in [
        "text",
        "message",
        "hint",
        "url",
        "target_url",
        "request_id",
        "worker_id",
        "solver_id",
        "reason",
        "context_id",
        "flag",
    ] {
        if body
            .get(key)
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
            return Err(reject("invalid_payload", format!("{key} must be a string")));
        }
    }
    if body
        .get("expected_generation")
        .is_some_and(|v| !v.is_null() && v.as_u64().is_none())
    {
        return Err(reject(
            "generation_invalid",
            "expected_generation must be a nonnegative integer",
        ));
    }
    if body
        .get("deadline_at")
        .is_some_and(|v| !v.is_null() && v.as_f64().is_none_or(|n| !n.is_finite()))
    {
        return Err(reject(
            "deadline_invalid",
            "deadline_at must be a finite timestamp",
        ));
    }
    if matches!(
        action.as_str(),
        "hint" | "focus" | "redirect" | "directive" | "correction" | "submit" | "send_message"
    ) && !nonempty(&body["text"])
        && !nonempty(&body["message"])
        && !nonempty(&body["hint"])
        && !(action == "redirect" && (nonempty(&body["url"]) || nonempty(&body["target_url"])))
    {
        return Err(reject("missing_content", format!("{action} requires text")));
    }
    if action == "answer_decision" && !nonempty(&body["request_id"]) {
        return Err(reject(
            "missing_request_id",
            "answer_decision requires request_id",
        ));
    }
    if action == "add_context" && !body["context"].is_object() && !nonempty(&body["context"]) {
        return Err(reject("missing_context", "add_context requires context"));
    }
    if action == "expire_context" && !nonempty(&body["context_id"]) {
        return Err(reject(
            "missing_context",
            "expire_context requires context_id",
        ));
    }
    if action == "cancel_worker" && kind != "worker" && !nonempty(&body["worker_id"]) {
        return Err(reject(
            "missing_worker",
            "cancel_worker requires a Worker scope or worker_id",
        ));
    }
    if serde_json::to_vec(body)
        .map_err(|_| reject("invalid_payload", "Invalid control JSON"))?
        .len()
        > 64 * 1024
    {
        return Err(reject(
            "payload_too_large",
            "Control payload exceeds 64 KiB",
        ));
    }
    Ok(())
}
pub(super) fn admit(run_id: &str, run: &Value, body: &Value, at: f64) -> Result<(), Error> {
    let scope = body["scope"].as_str().unwrap_or("global");
    if let Some((kind, value)) = scope.split_once(':') {
        let challenge = run["challenge_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(run_id);
        if (kind == "run" && value != run_id) || (kind == "challenge" && value != challenge) {
            return Err(reject(
                "scope_mismatch",
                "Control scope belongs to another Run or challenge",
            ));
        }
    }
    if body["deadline_at"]
        .as_f64()
        .is_some_and(|deadline| deadline <= at)
    {
        return Err(reject("deadline_expired", "Control deadline has expired"));
    }
    let action = body["action"].as_str().unwrap_or("");
    if action == "emergency_stop" && !super::execution_guard::protected(run) {
        return Err(reject(
            "guard_disabled",
            "Emergency stop requires a protected Pentest Run",
        ));
    }
    let terminated = matches!(
        run["status"].as_str(),
        Some("completed" | "failed" | "stopped" | "cancelled" | "finished" | "solved")
    ) || run["finished"] == true
        || run["control_mode"] == "terminated";
    let is_local_pi = run["kind"].as_str() == Some("local_pi");
    if action == "ask" && !terminated && !is_local_pi {
        return Err(reject(
            "ask_live_unsupported",
            "Use a side chat to ask about a running Run",
        ));
    }
    if (matches!(
        run["status"].as_str(),
        Some("completed" | "failed" | "stopped" | "cancelled" | "finished" | "solved")
    ) || run["finished"].as_bool() == Some(true)
        || run["control_mode"] == "terminated")
        && !TERMINATED.contains(&action)
        && !(is_local_pi
            && matches!(
                action,
                "continue"
                    | "resume"
                    | "send_message"
                    | "directive"
                    | "interrupt_turn"
                    | "interrupt"
            ))
    {
        return Err(reject(
            "run_terminated",
            "Finished Run rejects this live control action",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_ask_is_rejected_before_control_mutation() {
        let run = json!({"status":"running","running":true});
        let ask = json!({"action":"ask","scope":"global"});
        assert_eq!(
            admit("run-1", &run, &ask, 0.).unwrap_err().1["error"]["code"],
            "run.control.ask_live_unsupported"
        );
        let finished = json!({"status":"stopped","running":false});
        assert!(admit("run-1", &finished, &ask, 0.).is_ok());
    }
}
