//! Thread-routing gateway check for agent actors (no internal DB lease or RBAC checks).
use super::*;
use axum::http::{HeaderMap, header};

fn denied(code: &str, message: &str) -> Failure {
    fail(StatusCode::FORBIDDEN, code, message)
}
pub(super) fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    gateway_candidate: bool,
    envelope: &Value,
    _mode: &str,
    _operation: &str,
    _aggregate_type: &str,
    _aggregate_id: &str,
) -> Result<String, Failure> {
    let actor = envelope.get("actor");
    if actor.is_none() && !gateway_candidate {
        return Ok("operator:local-user".to_owned());
    }
    let Some(actor) = actor.filter(|actor| actor.is_object()) else {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "actor must be an object",
        ));
    };
    let kind = actor["kind"].as_str().unwrap_or("");
    if kind == "operator" && !gateway_candidate {
        return Ok("operator:local-user".to_owned());
    }
    if !matches!(kind, "agent" | "worker" | "extension") {
        return Err(denied(
            "platform.actor.unsupported",
            "Only a verified agent binding may use this actor kind",
        ));
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            fail(
                StatusCode::UNAUTHORIZED,
                "capability.token_required",
                "A live gateway bearer token is required",
            )
        })?;
    let owner = state
        .capability_gateway
        .owner_for_token(token)
        .ok_or_else(|| {
            fail(
                StatusCode::UNAUTHORIZED,
                "capability.token_invalid",
                "Gateway token is unknown or retired",
            )
        })?;
    Ok(format!("binding:{owner}:live"))
}
