//! Task-to-Task immutable asset publication snapshots.
use super::{
    Api, Error, rec, records, require, verify_ref,
};
use crate::AppState;
use axum::http::StatusCode;
use serde_json::Value;

fn same_artifact(a: &Value, b: &Value) -> bool {
    a["artifact_id"].as_str().is_some_and(|id| b["artifact_id"] == id)
}

pub(super) fn visible(s: &AppState, id: &str) -> Api<Vec<Value>> {
    let mut out = Vec::new();
    for reference in records(s, id, "asset_reference")? {
        let reference = &reference["payload"];
        if reference["revoked_at"].is_string() {
            continue;
        }
        let Some(owner) = reference["source_task_id"].as_str() else {
            continue;
        };
        let Some(publication_id) = reference["publication_id"].as_str() else {
            continue;
        };
        let Ok(row) = rec(s, owner, "asset_publication", publication_id) else {
            continue;
        };
        let publication = &row["payload"];
        if publication["revoked_at"].is_string()
            || publication["target_task_id"] != id
            || publication["task_id"] != owner
        {
            continue;
        }
        out.push(publication.clone());
    }
    Ok(out)
}

pub(super) fn readable_ref(s: &AppState, id: &str, reference: &Value) -> Api<Value> {
    let owner = require(reference, "task_id")?;
    if owner == id {
        return verify_ref(s, id, reference);
    }
    let allowed = visible(s, id)?.into_iter().any(|publication| {
        if publication["task_id"] != owner {
            return false;
        }
        let explicit = publication["artifact_refs"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|v| v["task_id"] == owner && same_artifact(v, reference));
        explicit
            || publication["assets"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|asset| {
                    asset["observations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|observation| {
                            observation["artifact_refs"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(|v| v["task_id"] == owner && same_artifact(v, reference))
                        })
                })
    });
    if !allowed {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.task_binding_invalid",
            "artifact is outside the readable publication scope",
        ));
    }
    verify_ref(s, owner, reference)
}
