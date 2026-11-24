//! Task-extension host: source verification, immutable versions, process isolation,
//! JSON-RPC execution, and declarative UI contributions.

mod api;
mod capabilities;
mod manifest;
mod runtime;
mod source;

pub use runtime::Host;

use crate::AppState;
use axum::{
    Json, Router,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::sync::Arc;

pub(crate) type Result<T> = std::result::Result<T, ExtError>;

#[derive(Debug)]
pub(crate) struct ExtError {
    status: StatusCode,
    code: String,
    category: &'static str,
    message: String,
}
impl ExtError {
    fn validation(code: &str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: code.to_owned(),
            category: "validation",
            message: message.into(),
        }
    }
    fn runtime(code: &str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: code.to_owned(),
            category: "runtime",
            message: message.into(),
        }
    }
    fn not_found(id: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "extension.not_found".to_owned(),
            category: "not_found",
            message: format!("extension not installed: {id}"),
        }
    }
    fn state(code: &str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: code.to_owned(),
            category: "state",
            message: message.into(),
        }
    }
    fn response(self) -> Response {
        failure(self.status, &self.code, self.category, self.message)
    }
}

fn failure(status: StatusCode, code: &str, category: &str, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": {"code": code, "category": category, "message": message.into(), "retryable": false}})))
        .into_response()
}

pub fn router() -> Router<Arc<AppState>> {
    api::router().merge(capabilities::router())
}

pub async fn initialize(state:&Arc<AppState>)->std::result::Result<(),String>{
    api::initialize(state).await.map_err(|error|error.message)
}
pub fn start_background(state:Arc<AppState>){api::start_monitor(state);}

pub(crate) async fn handle_secret_change(state: &Arc<AppState>, old_reference: &str,
    new_reference: &str) -> std::result::Result<serde_json::Value, String> {
    api::handle_secret_change(state, old_reference, new_reference).await
}

pub(crate) use capabilities::{dispatch_command, dispatch_query, module_records};
pub(crate) use api::{supports_lifecycle_command, dispatch_lifecycle_command, dispatch_lifecycle_query};

pub(crate) use capabilities::{invoke_capability, ready_capabilities};


#[cfg(test)]
mod tests;

pub(crate) use api::preview;
