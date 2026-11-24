use super::*;
use control_mcp::RequestGrantParams;
use tcode_core::control::ControlGrantRequest;

impl AppState {
    /// Store the request on the caller for the user to answer in the UI; an identical pending
    /// request is returned instead of asked twice.
    pub(super) fn op_request_grant(
        &mut self,
        caller: &Caller,
        params: RequestGrantParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let mut meta = self
            .find_meta(&caller.thread_id)
            .ok_or_else(|| "unknown thread".to_string())?;
        let grant = ControlGrant {
            scope: params.scope,
            tiers: params.tiers.into_iter().collect(),
            categories: params.categories.into_iter().collect(),
        };
        if grant.is_empty() {
            return Err("request_grant needs at least one tier or category".into());
        }
        let reason = params.reason.trim().to_owned();
        if reason.is_empty() {
            return Err("request_grant needs a reason the user can read".into());
        }
        let pending = |request: &ControlGrantRequest| {
            serde_json::json!({
                "status": "pending",
                "request_id": request.id,
                "requested": request.grant,
                "note": "The user answers in Tcode's UI; you receive a [control] message with their answer.",
            })
        };
        if let Some(existing) = meta
            .control_grant_requests
            .iter()
            .find(|request| request.grant == grant)
        {
            return Ok(pending(existing));
        }
        let request = ControlGrantRequest {
            id: uuid::Uuid::new_v4().to_string(),
            grant,
            reason,
            requested_at: now_secs(),
        };
        let response = pending(&request);
        let detail = request.reason.clone();
        meta.control_grant_requests.push(request);
        meta.updated_at = now_secs();
        self.store_meta(meta, cx);
        self.audit(
            caller,
            &caller.thread_id,
            action::REQUEST_GRANT,
            Some(detail),
            cx,
        );
        Ok(response)
    }

    /// The user's answer to a request: approval widens the thread's own grant by what was
    /// asked; either way the thread hears the answer as a `[control]` message.
    pub fn respond_control_grant(
        &mut self,
        session_id: &str,
        request_id: &str,
        approve: bool,
        cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        let unknown = |code: &str, message: &str| ProtocolError {
            code: code.into(),
            message: message.into(),
        };
        let mut meta = self
            .find_meta(session_id)
            .ok_or_else(|| unknown("unknown_session", "This thread is no longer available."))?;
        let index = meta
            .control_grant_requests
            .iter()
            .position(|request| request.id == request_id)
            .ok_or_else(|| {
                unknown(
                    "unknown_request",
                    "This grant request was already answered.",
                )
            })?;
        let request = meta.control_grant_requests.remove(index);
        let text = if approve {
            let mut grant = meta
                .control_grant
                .clone()
                .unwrap_or_else(|| self.caller_grant(&meta).0);
            grant.merge(&request.grant, meta.project_id.as_deref());
            meta.control_grant = Some(grant);
            format!(
                "[control] grant approved: {}. Your grant is now {}. It applies at once to the \
                 tools you already have; tools it adds appear when your provider next starts a \
                 session.",
                request.grant.describe(),
                meta.control_grant.as_ref().expect("set above").describe()
            )
        } else {
            format!(
                "[control] grant declined: {}. Your grant stays {}.",
                request.grant.describe(),
                self.caller_grant(&meta).0.describe()
            )
        };
        meta.updated_at = now_secs();
        self.store_meta(meta, cx);
        self.refresh_control_grants();
        self.deliver_orchestrate_callback_to_parent(session_id, text, cx);
        Ok(())
    }

    /// Set or clear the thread's own grant.
    pub fn set_control_grant(
        &mut self,
        session_id: &str,
        grant: Option<ControlGrant>,
        cx: &mut HostCx,
    ) {
        self.change_meta(session_id, cx, |meta| meta.control_grant = grant);
        self.refresh_control_grants();
    }
}
