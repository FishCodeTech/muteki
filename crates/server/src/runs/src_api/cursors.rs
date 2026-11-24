//! Source-compatible, Task-scoped query cursors.
use super::{Api, AppState, Error, db};
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn invalid(message: &str) -> Error {
    Error::new(StatusCode::BAD_REQUEST, "query.cursor_invalid", message)
}

fn key(state: &AppState) -> Api<Vec<u8>> {
    crate::operations_api::platform_cursor_key(state).map_err(db)
}

fn scope_digest(scope: &str) -> String {
    if scope.trim().is_empty() { return String::new(); }
    format!("{:x}", Sha256::digest(scope.trim().as_bytes()))[..16].to_owned()
}

pub(in crate::runs::src_api) fn signed(
    state: &AppState, aggregate_type: &str, aggregate_id: &str, seq: i64, scope: &str,
) -> Api<String> {
    let payload = json!({"aid":aggregate_id,"at":aggregate_type,"scp":scope_digest(scope),"seq":seq}).to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(&key(state)?).map_err(db)?;
    mac.update(payload.as_bytes());
    let signature = mac.finalize().into_bytes();
    Ok(format!("v1.{}.{}", URL_SAFE_NO_PAD.encode(payload), URL_SAFE_NO_PAD.encode(&signature[..24])))
}

pub(in crate::runs::src_api) fn after(
    state: &AppState, cursor: &str, aggregate_type: &str, aggregate_id: &str, _scope: &str,
) -> Api<i64> {
    if cursor.is_empty() { return Ok(0); }
    let parts: Vec<_> = cursor.split('.').collect();
    if parts.len() != 3 || parts[0] != "v1" { return Err(invalid("malformed event cursor")); }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).map_err(|_| invalid("malformed event cursor"))?;
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).map_err(|_| invalid("malformed event cursor"))?;
    if signature.len() != 24 { return Err(invalid("event cursor signature mismatch")); }
    let mut mac = Hmac::<Sha256>::new_from_slice(&key(state)?).map_err(db)?;
    mac.update(&payload);
    let expected = mac.finalize().into_bytes();
    if signature.iter().zip(expected.iter()).fold(0u8, |bits, (left, right)| bits | (left ^ right)) != 0 {
        return Err(invalid("event cursor signature mismatch"));
    }
    let data: Value = serde_json::from_slice(&payload).map_err(|_| invalid("malformed event cursor"))?;
    if data["at"] != aggregate_type || data["aid"] != aggregate_id {
        return Err(invalid("event cursor is bound to another aggregate"));
    }
    data["seq"].as_i64().filter(|seq| *seq >= 0).ok_or_else(|| invalid("malformed event cursor payload"))
}
