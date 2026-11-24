//! Static source analysis. Imported material never grants execution scope.
use super::verify_ref;
use crate::AppState;
use crate::runs::src_api::{Api, Error, rec, require};
use axum::http::StatusCode;
use base64::Engine;
use serde_json::{Value, json};

mod openapi;
pub(super) use openapi::{ingest as ingest_openapi, read as read_openapi};

mod openapi_compare;
pub(super) use openapi_compare::compare as compare_openapi;
pub(super) use openapi_compare::request_parameters;

mod javascript;
pub(super) use javascript::{
    collect as collect_javascript, ingest as ingest_javascript, read as read_javascript,
    unpack_sourcemap,
};

pub(super) fn read_skill(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    let rid = require(p, "record_id")?;
    let row = rec(s, id, "skill_material", rid)?;
    let content = verify_ref(s, id, &row["payload"]["artifact"])?;
    let mut flattened = json!({"record_id":row["record_id"],"kind":row["kind"],
        "revision":row["revision"],"parent_id":row["parent_id"],
        "version":row["payload"]["version"].as_u64().unwrap_or(row["revision"].as_u64().unwrap_or(0))});
    for (key, value) in row["payload"].as_object().into_iter().flatten() {
        flattened[key] = value.clone();
    }
    let mut result = json!({"record":flattened,
        "capability_mappings":row["payload"]["capability_mappings"]});
    if let Some(index) = p.get("fragment_index").filter(|value| !value.is_null()) {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(content["data"].as_str().unwrap_or(""))
            .map_err(|_| {
                Error::new(
                    StatusCode::BAD_REQUEST,
                    "skill_fragment_invalid",
                    "fragment does not resolve to the original source",
                )
            })?;
        let fragments = row["payload"]["selected_fragments"].as_array();
        let index = index.as_i64().ok_or_else(|| {
            Error::new(
                StatusCode::BAD_REQUEST,
                "skill_fragment_invalid",
                "fragment does not resolve to the original source",
            )
        })?;
        let length = fragments.map_or(0, Vec::len) as i64;
        let index = if index < 0 { length + index } else { index };
        let fragment = fragments
            .and_then(|rows| rows.get(index as usize))
            .ok_or_else(|| {
                Error::new(
                    StatusCode::BAD_REQUEST,
                    "skill_fragment_invalid",
                    "fragment does not resolve to the original source",
                )
            })?;
        if !fragment.is_object() {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "skill_fragment_invalid",
                "fragment does not resolve to the original source",
            ));
        }
        let number = |field: &str| -> Option<usize> {
            fragment[field]
                .as_u64()
                .or_else(|| fragment[field].as_str()?.parse().ok())
                .and_then(|value| usize::try_from(value).ok())
        };
        let start = number("byte_start").ok_or_else(|| {
            Error::new(
                StatusCode::BAD_REQUEST,
                "skill_fragment_invalid",
                "fragment does not resolve to the original source",
            )
        })?;
        let end = number("byte_end").ok_or_else(|| {
            Error::new(
                StatusCode::BAD_REQUEST,
                "skill_fragment_invalid",
                "fragment does not resolve to the original source",
            )
        })?;
        if start > end || end > raw.len() {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "skill_fragment_invalid",
                "fragment does not resolve to the original source",
            ));
        }
        let mut selected = fragment.clone();
        selected["content"] = json!({"encoding":"base64",
            "data":base64::engine::general_purpose::STANDARD.encode(&raw[start..end]),
            "size":end-start});
        result["fragment"] = selected;
    } else {
        result["original"] = json!({"encoding":"base64","data":content["data"],
            "size":content["size"]});
    }
    Ok(result)
}
