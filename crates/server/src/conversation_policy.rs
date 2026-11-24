//! The conversation engine depends on this contract, not a security workbench.
use crate::{
    AppState,
    agent_runtime::{RuntimeError, Session, StartRequest},
};
use agent::{AgentEvent, ApprovalDecision};
use axum::http::StatusCode;
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

pub struct Error {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}
/// Run-backed conversations supply their own launch environment and event sink.
pub trait RunBridge: Send + Sync {
    fn check(&self, state: &AppState, thread: &str) -> Result<(), RuntimeError>;
    fn prepare(
        &self,
        state: &AppState,
        document: &Value,
        request: &mut StartRequest,
    ) -> Result<bool, RuntimeError>;
    fn observe(&self, state: &Arc<AppState>, thread: &str, binding: &Value, event: &AgentEvent);
}
pub trait Policy: Send + Sync {
    fn binding(&self, state: &AppState, thread: &str) -> Result<Option<String>, RuntimeError>;
    fn assert_access(
        &self,
        state: &AppState,
        thread: &str,
        adapter: &str,
        mode: &str,
    ) -> Result<bool, RuntimeError>;
    fn install<'a>(
        &self,
        state: &'a Arc<AppState>,
        thread: &'a str,
        request: &'a mut StartRequest,
    ) -> BoxFuture<'a, Result<(), RuntimeError>>;
    fn observe(
        &self,
        state: &AppState,
        thread: &str,
        document: &mut Value,
        session: &Session,
        event: &AgentEvent,
    ) -> Result<(), Error>;
    fn decide(
        &self,
        state: &AppState,
        document: &Value,
        approval: &Value,
        choice: &ApprovalDecision,
    ) -> Result<(), Error>;
    fn prepare_delivery(
        &self,
        state: &AppState,
        thread: &str,
        request: &str,
        choice: &ApprovalDecision,
    ) -> Result<(), RuntimeError>;
    fn delivery_failed(&self, state: &AppState, thread: &str, request: &str) -> Result<(), Error>;
    fn managed_approval(&self, document: &Value, payload: &Value) -> bool;
    fn decide_managed(
        &self,
        state: &AppState,
        thread: &str,
        document: &mut Value,
        payload: &Value,
    ) -> Result<Value, Error>;
}
