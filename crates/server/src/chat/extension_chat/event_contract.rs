//! The extension AgentEvent boundary mirrors the source platform payload contract.
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

fn redact(input: &str) -> String {
    static RULES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| [
        (r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}","$1 [REDACTED]"),
        (r"\b(sk|rk|pk)-[A-Za-z0-9_-]{8,}","$1-[REDACTED]"),
        (r"\bcrsr_[A-Za-z0-9_-]{8,}","[REDACTED]"),
        (r"\b(gh[pousr]_|github_pat_|xox[abposr]-|AKIA)[A-Za-z0-9_-]{8,}","$1[REDACTED]"),
        (r"://[^/\s:@]+:[^/\s@]+@","://[REDACTED]@"),
        (r#"(?i)([?&](?:access_token|refresh_token|id_token|token|api[_-]?key|key|secret|password|passwd|auth|signature|sig|code)=)[^&\s#"']+"#,"$1[REDACTED]"),
        (r#"(?i)((?:authorization|x-api-key|api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|password)\s*[:=]\s*)[^\s"',}]+"#,"$1[REDACTED]"),
    ].into_iter().map(|(pattern,replacement)| (regex::Regex::new(pattern).unwrap(),replacement)).collect());
    rules
        .iter()
        .fold(input.to_owned(), |text, (regex, replacement)| {
            regex.replace_all(&text, *replacement).into_owned()
        })
}

const RESERVED: &str = "agent_session_id command_id correlation_id event_id native_type occurred_at runtime runtime_turn_id schema_version seq stream_seq thread_id turn_id usage_native_type";
const TOP: &str = "schema_version event_type agent_session_id external_session_id run_id execution_generation turn_id seq occurred_at native_type payload";

fn fields(kind: &str) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        "session.started" | "session.resumed" => ("transport adapter_id instance_id cwd model", ""),
        "session.closed" => ("reason resume_available", ""),
        "turn.started" => ("kind", ""),
        "turn.completed" => ("stop_reason duration_ms", ""),
        "turn.failed" => ("error", "error"),
        "message.delta" => ("text role phase message_id replace", "text"),
        "message.completed" => ("text role phase message_id", "text"),
        "reasoning.summary" => ("text channel partial item_id duration_ms", "text channel"),
        "tool.started" | "tool.progress" | "tool.completed" => (
            "tool_call_id name kind title input output chunk status error exit_code duration_ms parent_tool_call_id agent_id model message_id",
            "tool_call_id",
        ),
        "approval.requested" => (
            "approval_id agent_id approval_kind title reason tool_call_id tool_name command cwd path paths unified_diff files input options scopes expires_at plan_markdown",
            "approval_id",
        ),
        "approval.resolved" => (
            "approval_id agent_id decision scope option_id note automatic",
            "approval_id",
        ),
        "user_input.requested" => (
            "request_id user_input_kind agent_id title message questions requested_schema options response_actions tool_call_id expires_at",
            "request_id",
        ),
        "user_input.resolved" => ("request_id outcome answers", "request_id"),
        "artifact.created" => ("name kind media_type path content_base64 tool_call_id", "name"),
        "workspace.changed" => ("unified_diff files", ""),
        "usage.updated" => (
            "scope usage_id input_tokens output_tokens cached_input_tokens cache_write_tokens reasoning_tokens total_tokens context_window context_used_tokens cost_usd cost_kind step_count llm_duration_ms",
            "scope",
        ),
        "compaction.updated" => (
            "status trigger confirmed tokens_before tokens_after summary message",
            "status",
        ),
        "plan.updated" => ("tasks title explanation phase patch revision", ""),
        "agent.updated" => ("agents patch unsupported unsupported_reason", ""),
        "runtime.capabilities_updated" => ("revision reason capabilities", ""),
        "runtime.warning" => ("kind message code rate_limit", "kind message"),
        "runtime.error" => ("error", "error"),
        "runtime.exited" => ("classification exit_code error", "classification"),
        _ => return None,
    })
}

fn has(words: &str, key: &str) -> bool {
    words.split_ascii_whitespace().any(|word| word == key)
}
fn problem(kind: &str, detail: &str) -> String {
    format!("external_agent.contract.invalid_payload: {kind}: {detail}")
}
fn check_fields(
    kind: &str,
    map: &Map<String, Value>,
    allowed: &str,
    required: &str,
) -> Result<(), String> {
    for key in map.keys() {
        if !has(allowed, key) {
            return Err(problem(kind, &format!("unknown field {key}")));
        }
    }
    for key in required.split_ascii_whitespace() {
        if map.get(key).is_none_or(|value| value.is_null()) {
            return Err(problem(kind, &format!("missing {key}")));
        }
    }
    Ok(())
}

pub(super) fn validate(event: &Value) -> Result<Value, String> {
    let object = event
        .as_object()
        .ok_or_else(|| problem("AgentEvent", "event must be an object"))?;
    check_fields("AgentEvent", object, TOP, "event_type")?;
    let mut normalized = event.clone();
    if normalized.get("payload").is_none() {
        normalized["payload"] = json!({});
    }
    if normalized.get("agent_session_id").is_some() && !normalized["agent_session_id"].is_string() {
        return Err(problem("AgentEvent", "agent_session_id must be text"));
    }
    for key in ["external_session_id", "run_id", "turn_id", "native_type"] {
        optional_text("AgentEvent", event, key)?;
    }
    if event
        .get("occurred_at")
        .is_some_and(|value| !value.is_null() && !value.is_string() && !value.is_number())
    {
        return Err(problem("AgentEvent", "occurred_at must be a datetime"));
    }
    for key in ["schema_version", "execution_generation", "seq"] {
        integer("AgentEvent", event, key, false)?;
        if key != "execution_generation" && event.get(key).is_some_and(Value::is_null) {
            return Err(problem("AgentEvent", &format!("{key} must be an integer")));
        }
    }
    let kind = event["event_type"]
        .as_str()
        .ok_or_else(|| problem("AgentEvent", "event_type must be a string"))?;
    let (allowed, required) = fields(kind).ok_or_else(|| problem(kind, "unknown event type"))?;
    let payload = normalized["payload"]
        .as_object()
        .ok_or_else(|| problem(kind, "payload must be an object"))?;
    for key in payload.keys() {
        if has(RESERVED, key) {
            return Err(format!(
                "external_agent.contract.reserved_key: {kind}: {key}"
            ));
        }
    }
    let fields = format!("native {allowed}");
    check_fields(kind, payload, &fields, required)?;
    if payload
        .get("native")
        .is_some_and(|value| !value.is_object())
    {
        return Err(problem(kind, "native must be an object"));
    }
    validate_shape(kind, &mut normalized["payload"])?;
    Ok(normalized)
}

fn required_text(kind: &str, payload: &Value, key: &str) -> Result<(), String> {
    if payload[key].as_str().is_none_or(str::is_empty) {
        Err(problem(kind, &format!("{key} must be nonempty text")))
    } else {
        Ok(())
    }
}
fn optional_text(kind: &str, payload: &Value, key: &str) -> Result<(), String> {
    if payload
        .get(key)
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        Err(problem(kind, &format!("{key} must be text")))
    } else {
        Ok(())
    }
}
fn one_of(kind: &str, payload: &Value, key: &str, choices: &str) -> Result<(), String> {
    if payload[key]
        .as_str()
        .is_some_and(|value| has(choices, value))
    {
        Ok(())
    } else {
        Err(problem(kind, &format!("{key} must be one of {choices}")))
    }
}
fn nested(
    kind: &str,
    payload: &Value,
    key: &str,
    allowed: &str,
    required: &str,
) -> Result<(), String> {
    let Some(value) = payload.get(key).filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let map = value
        .as_object()
        .ok_or_else(|| problem(kind, &format!("{key} must be an object")))?;
    check_fields(kind, map, allowed, required)
}
fn list(kind: &str, payload: &Value, key: &str) -> Result<(), String> {
    if payload.get(key).is_some_and(|value| !value.is_array()) {
        Err(problem(kind, &format!("{key} must be an array")))
    } else {
        Ok(())
    }
}
fn number(kind: &str, payload: &Value, key: &str, ge_zero: bool) -> Result<(), String> {
    if let Some(value) = payload.get(key).filter(|value| !value.is_null()) {
        if value
            .as_f64()
            .is_none_or(|number| !number.is_finite() || ge_zero && number < 0.0)
        {
            return Err(problem(kind, &format!("{key} must be a valid number")));
        }
    }
    Ok(())
}
fn integer(kind: &str, payload: &Value, key: &str, ge_zero: bool) -> Result<(), String> {
    if let Some(value) = payload.get(key).filter(|value| !value.is_null()) {
        if value.as_i64().is_none_or(|number| ge_zero && number < 0) {
            return Err(problem(kind, &format!("{key} must be an integer")));
        }
    }
    Ok(())
}
fn validate_shape(kind: &str, payload: &mut Value) -> Result<(), String> {
    for key in [
        "transport",
        "adapter_id",
        "instance_id",
        "cwd",
        "model",
        "reason",
        "kind",
        "stop_reason",
        "text",
        "role",
        "phase",
        "message_id",
        "channel",
        "item_id",
        "tool_call_id",
        "name",
        "title",
        "chunk",
        "status",
        "parent_tool_call_id",
        "agent_id",
        "approval_id",
        "approval_kind",
        "tool_name",
        "command",
        "path",
        "unified_diff",
        "expires_at",
        "plan_markdown",
        "decision",
        "scope",
        "option_id",
        "note",
        "request_id",
        "user_input_kind",
        "message",
        "outcome",
        "media_type",
        "content_base64",
        "usage_id",
        "cost_kind",
        "trigger",
        "summary",
        "explanation",
        "unsupported_reason",
        "code",
        "classification",
    ] {
        if !matches!(key, "kind" | "status" | "name" | "error") {
            optional_text(kind, payload, key)?;
        }
    }
    for key in [
        "resume_available",
        "replace",
        "partial",
        "automatic",
        "confirmed",
        "patch",
        "unsupported",
    ] {
        if payload.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(problem(kind, &format!("{key} must be boolean")));
        }
    }
    for key in ["duration_ms", "exit_code"] {
        integer(kind, payload, key, false)?;
    }
    integer(kind, payload, "revision", kind == "plan.updated")?;
    for key in [
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
        "total_tokens",
        "context_window",
        "context_used_tokens",
        "tokens_before",
        "tokens_after",
        "step_count",
        "llm_duration_ms",
    ] {
        integer(kind, payload, key, true)?;
    }
    number(kind, payload, "cost_usd", true)?;
    for key in [
        "files",
        "paths",
        "options",
        "scopes",
        "questions",
        "response_actions",
        "tasks",
        "agents",
    ] {
        list(kind, payload, key)?;
    }
    match kind {
        "turn.started" => optional_text(kind, payload, "kind")?,
        "message.delta" => {
            one_of(
                kind,
                &json!({"role":payload["role"].as_str().unwrap_or("assistant")}),
                "role",
                "assistant user",
            )?;
            if payload["replace"] == true
                && payload["message_id"].as_str().is_none_or(str::is_empty)
            {
                return Err(problem(kind, "replace requires message_id"));
            }
        }
        "message.completed" => one_of(
            kind,
            &json!({"role":payload["role"].as_str().unwrap_or("assistant")}),
            "role",
            "assistant user",
        )?,
        "reasoning.summary" => {
            required_text(kind, payload, "text")?;
            one_of(kind, payload, "channel", "summary thinking")?;
        }
        "tool.started" | "tool.progress" | "tool.completed" => {
            required_text(kind, payload, "tool_call_id")?;
            for key in ["name", "kind", "error"] {
                optional_text(kind, payload, key)?;
            }
            if !payload["status"].is_null() {
                one_of(
                    kind,
                    payload,
                    "status",
                    "pending running completed failed cancelled",
                )?;
            }
        }
        "approval.requested" | "approval.resolved" => {
            required_text(kind, payload, "approval_id")?;
            if kind == "approval.requested" && !payload["approval_kind"].is_null() {
                one_of(
                    kind,
                    payload,
                    "approval_kind",
                    "command_execution file_change permissions mcp_tool_call tool plan_exit",
                )?;
            }
            if kind == "approval.resolved" && !payload["decision"].is_null() {
                one_of(kind, payload, "decision", "allow deny cancelled expired")?;
            }
        }
        "user_input.requested" | "user_input.resolved" => {
            required_text(kind, payload, "request_id")?;
            if kind == "user_input.resolved" && !payload["outcome"].is_null() {
                one_of(
                    kind,
                    payload,
                    "outcome",
                    "answered cancelled declined timeout expired",
                )?;
            }
        }
        "artifact.created" => {
            required_text(kind, payload, "name")?;
            optional_text(kind, payload, "kind")?;
            if payload["path"].is_null() == payload["content_base64"].is_null() {
                return Err(problem(
                    kind,
                    "artifact needs exactly one of path or content_base64",
                ));
            }
        }
        "usage.updated" => {
            one_of(
                kind,
                payload,
                "scope",
                "message turn invocation session_cumulative context_only",
            )?;
            if matches!(payload["scope"].as_str(), Some("message" | "invocation")) {
                required_text(kind, payload, "usage_id")?;
            }
            if !payload["cost_kind"].is_null() {
                one_of(kind, payload, "cost_kind", "reported estimated")?;
            }
        }
        "compaction.updated" => {
            one_of(
                kind,
                payload,
                "status",
                "started completed failed interrupted",
            )?;
            if !payload["trigger"].is_null() {
                one_of(kind, payload, "trigger", "manual auto")?;
            }
        }
        "runtime.warning" => {
            one_of(
                kind,
                payload,
                "kind",
                "rate_limit notice degraded config protocol process",
            )?;
            required_text(kind, payload, "message")?;
            if (payload["kind"] == "rate_limit") != !payload["rate_limit"].is_null() {
                return Err(problem(
                    kind,
                    "rate_limit is required exactly for rate_limit warnings",
                ));
            }
        }
        "runtime.exited" => required_text(kind, payload, "classification")?,
        _ => {}
    }
    validate_nested(kind, payload)?;
    Ok(())
}

fn item_map<'a>(kind: &str, key: &str, item: &'a Value) -> Result<&'a Map<String, Value>, String> {
    item.as_object()
        .ok_or_else(|| problem(kind, &format!("{key} item must be an object")))
}
fn check_files(kind: &str, payload: &Value) -> Result<(), String> {
    for file in payload["files"].as_array().into_iter().flatten() {
        let map = item_map(kind, "files", file)?;
        check_fields(kind, map, "path change old_path unified_diff", "path")?;
        required_text(kind, file, "path")?;
        for key in ["old_path", "unified_diff"] {
            optional_text(kind, file, key)?;
        }
        if !file["change"].is_null() {
            one_of(kind, file, "change", "add modify delete rename")?;
        }
    }
    Ok(())
}
fn check_failure(kind: &str, error: &mut Value) -> Result<(), String> {
    let map = error
        .as_object()
        .ok_or_else(|| problem(kind, "error must be an object"))?;
    check_fields(
        kind,
        map,
        "code category reason engine message detail retryable native_code delivery_unknown",
        "category reason engine message",
    )?;
    one_of(
        kind,
        error,
        "category",
        "usage_limit auth transport permission validation provider runtime_exited timeout cancelled unsupported unknown",
    )?;
    for key in ["reason", "engine", "message"] {
        required_text(kind, error, key)?;
    }
    for key in ["code", "detail", "native_code"] {
        optional_text(kind, error, key)?;
    }
    for key in ["retryable", "delivery_unknown"] {
        if error.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(problem(kind, &format!("error.{key} must be boolean")));
        }
    }
    let expected = format!("external_agent.{}", error["category"].as_str().unwrap());
    if error["code"]
        .as_str()
        .is_some_and(|code| !code.is_empty() && code != expected)
    {
        return Err(problem(kind, "error.code disagrees with category"));
    }
    error["code"] = json!(expected);
    for key in ["message", "detail"] {
        if let Some(value) = error[key].as_str() {
            error[key] = json!(redact(value));
        }
    }
    Ok(())
}
fn validate_nested(kind: &str, payload: &mut Value) -> Result<(), String> {
    for key in ["paths", "scopes", "response_actions"] {
        for item in payload[key].as_array().into_iter().flatten() {
            if !item.is_string() {
                return Err(problem(kind, &format!("{key} must contain strings")));
            }
        }
    }
    for key in ["requested_schema", "answers", "capabilities"] {
        if payload
            .get(key)
            .is_some_and(|value| !value.is_null() && !value.is_object())
        {
            return Err(problem(kind, &format!("{key} must be an object")));
        }
    }
    check_files(kind, payload)?;
    for option in payload["options"].as_array().into_iter().flatten() {
        if kind == "approval.requested" {
            let map = item_map(kind, "options", option)?;
            check_fields(kind, map, "option_id label kind", "option_id")?;
            required_text(kind, option, "option_id")?;
            for key in ["label", "kind"] {
                optional_text(kind, option, key)?;
            }
        } else if !option.is_object() {
            return Err(problem(kind, "options item must be an object"));
        }
    }
    for question in payload["questions"].as_array().into_iter().flatten() {
        if !question.is_object() {
            return Err(problem(kind, "questions item must be an object"));
        }
    }
    for task in payload["tasks"].as_array().into_iter().flatten() {
        let map = item_map(kind, "tasks", task)?;
        check_fields(
            kind,
            map,
            "task_id title status blocked_reason",
            "task_id title",
        )?;
        required_text(kind, task, "task_id")?;
        required_text(kind, task, "title")?;
        if !task["status"].is_null() {
            one_of(
                kind,
                task,
                "status",
                "pending in_progress completed blocked cancelled",
            )?;
        }
        optional_text(kind, task, "blocked_reason")?;
    }
    for node in payload["agents"].as_array().into_iter().flatten() {
        let map = item_map(kind, "agents", node)?;
        check_fields(
            kind,
            map,
            "agent_id parent_id title nickname role model message_id call_id session_ref status request result error activity tool_uses total_tokens duration_ms",
            "agent_id",
        )?;
        required_text(kind, node, "agent_id")?;
        for key in [
            "parent_id",
            "title",
            "nickname",
            "role",
            "model",
            "message_id",
            "call_id",
            "session_ref",
            "request",
            "result",
            "error",
            "activity",
        ] {
            optional_text(kind, node, key)?;
        }
        if !node["status"].is_null() {
            one_of(
                kind,
                node,
                "status",
                "pending running completed failed cancelled",
            )?;
        }
        for key in ["tool_uses", "total_tokens", "duration_ms"] {
            integer(kind, node, key, false)?;
        }
    }
    if kind == "agent.updated"
        && payload["agents"].as_array().is_none_or(Vec::is_empty)
        && payload["unsupported"] != true
    {
        return Err(problem(
            kind,
            "agent update needs a node or unsupported=true",
        ));
    }
    if kind == "runtime.warning" && !payload["rate_limit"].is_null() {
        nested(
            kind,
            payload,
            "rate_limit",
            "limited warning resets_at kind utilization",
            "",
        )?;
        for key in ["limited", "warning"] {
            if payload["rate_limit"]
                .get(key)
                .is_some_and(|value| !value.is_boolean())
            {
                return Err(problem(kind, &format!("rate_limit.{key} must be boolean")));
            }
        }
        for key in ["resets_at", "utilization"] {
            number(kind, &payload["rate_limit"], key, false)?;
        }
        optional_text(kind, &payload["rate_limit"], "kind")?;
    }
    if matches!(kind, "turn.failed" | "runtime.error" | "runtime.exited")
        && !payload["error"].is_null()
    {
        check_failure(kind, &mut payload["error"])?;
    }
    if kind == "runtime.warning" {
        payload["message"] = json!(redact(payload["message"].as_str().unwrap_or("")));
    }
    if kind == "compaction.updated" && payload["message"].is_string() {
        payload["message"] = json!(redact(payload["message"].as_str().unwrap()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_contract_preserves_replace_and_redacts_typed_failure() {
        let replaced = validate(&json!({"event_type":"message.delta","payload":{
            "text":"complete text","message_id":"m-1","replace":true}}))
        .unwrap();
        assert_eq!(replaced["payload"]["replace"], true);
        let failure = validate(&json!({"event_type":"turn.failed","payload":{"error":{
            "category":"provider","reason":"bad_response","engine":"fixture",
            "message":"Bearer abcdefghijklmnop failed","detail":"api_key=topsecret123456"}}}))
        .unwrap();
        assert_eq!(
            failure["payload"]["error"]["code"],
            "external_agent.provider"
        );
        assert!(!failure.to_string().contains("abcdefghijklmnop"));
        assert!(!failure.to_string().contains("topsecret123456"));
    }

    #[test]
    fn agent_event_contract_rejects_reserved_and_unknown_payload_fields() {
        assert!(
            validate(&json!({"event_type":"message.delta","payload":{
            "text":"hi","turn_id":"forged"}}))
            .unwrap_err()
            .contains("reserved_key")
        );
        assert!(
            validate(&json!({"event_type":"approval.requested","payload":{
            "approval_id":"a-1","tool":"wrong"}}))
            .unwrap_err()
            .contains("unknown field")
        );
    }
}
