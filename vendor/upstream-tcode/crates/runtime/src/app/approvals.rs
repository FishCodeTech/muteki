use super::*;

impl AppState {
    /// Apply the approval-state transition carried by a canonical provider event.
    pub(super) fn record_approval_event(&mut self, session_id: &str, event: &AgentEvent) {
        match event {
            AgentEvent::ApprovalRequested(request) => {
                let requests = self.approvals.entry(session_id.to_string()).or_default();
                if !requests.iter().any(|pending| pending.id == request.id) {
                    requests.push(request.clone());
                }
            }
            AgentEvent::ApprovalResolved { request_id, .. } => {
                self.clear_approval(session_id, request_id);
            }
            AgentEvent::TurnCompleted { .. } => {
                if let Some(requests) = self.approvals.get_mut(session_id) {
                    requests.retain(|request| request.parent_item_id.is_some());
                    if requests.is_empty() {
                        self.approvals.remove(session_id);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn approval_requests(&self, session_id: &str) -> &[agent::ApprovalRequest] {
        self.approvals
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub(super) fn first_approval(&self, session_id: &str) -> Option<&agent::ApprovalRequest> {
        self.approval_requests(session_id).first()
    }

    pub(super) fn clear_approvals(&mut self, session_id: &str) {
        self.approvals.remove(session_id);
    }

    fn clear_approval(&mut self, session_id: &str, request_id: &str) {
        let Some(requests) = self.approvals.get_mut(session_id) else {
            return;
        };
        requests.retain(|request| request.id != request_id);
        if requests.is_empty() {
            self.approvals.remove(session_id);
        }
    }

    /// Send a response to any resident session and clear the same request from
    /// the host authority once the provider command has accepted it.
    pub(super) fn respond_session_approval(
        &mut self,
        session_id: &str,
        request_id: String,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        let commands = match &self
            .resident(session_id)
            .ok_or_else(|| "session is not loaded".to_string())?
            .runtime
        {
            Runtime::Live(commands) => commands.clone(),
            _ => return Err("session is not live".into()),
        };
        commands
            .try_send(SessionCommand::RespondApproval {
                request_id: request_id.clone(),
                decision,
            })
            .map_err(|err| format!("failed to respond to approval: {err}"))?;
        self.clear_approval(session_id, &request_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TestStore;
    use super::*;

    fn request(id: &str) -> agent::ApprovalRequest {
        agent::ApprovalRequest {
            id: id.into(),
            parent_item_id: None,
            turn_id: None,
            kind: agent::ApprovalKind::ExecCommand {
                command: "cargo test".into(),
                cwd: None,
                reason: None,
            },
            options: Vec::new(),
        }
    }

    fn live_session(id: &str, commands: smol::channel::Sender<SessionCommand>) -> ActiveSession {
        let mut meta = SessionMeta::new(ProviderKind::Codex, PathBuf::from("/tmp"), None);
        meta.id = id.into();
        let mut session = ActiveSession::new(meta, false, Vec::new());
        session.runtime = Runtime::Live(commands);
        session
    }

    fn state(name: &str) -> (TestStore, AppState) {
        let store = TestStore::new(name);
        let state = AppState::new((*store).clone()).unwrap();
        (store, state)
    }

    #[test]
    fn approvals_are_session_scoped_deduplicated_and_cleared_only_after_delivery() {
        for parked in [false, true] {
            let (_store, mut state) = state("approval-authority");
            let (commands, receiver) = smol::channel::unbounded();
            let mut child = live_session("child", commands);
            child.meta.parent_session_id = Some("parent".into());
            child.turn_in_flight = true;
            state.sessions.push(child.meta.clone());
            if parked {
                state.residents.parked.insert("child".into(), child);
            } else {
                state.residents.live.insert("child".into(), child);
            }
            let first = request("first");
            for (session, pending) in [("child", &first), ("child", &first), ("parent", &first)] {
                state.record_approval_event(
                    session,
                    &AgentEvent::ApprovalRequested(pending.clone()),
                );
            }
            state.record_approval_event("child", &AgentEvent::ApprovalRequested(request("second")));
            assert_eq!(state.approval_requests("child").len(), 2);
            assert_eq!(state.first_approval("child"), Some(&first));
            let status = state.child_status_json(&state.sessions[0], &Timeline::default());
            assert_eq!(status["approval_request_id"], "first");
            assert_eq!(status["waiting_approval"], "command `cargo test`");
            assert_eq!(
                state
                    .session_status_snapshot("child")
                    .unwrap()
                    .pending_approvals
                    .len(),
                2
            );

            state
                .respond_session_approval("child", "first".into(), ApprovalDecision::Cancel)
                .unwrap();
            assert!(
                matches!(receiver.try_recv(), Ok(SessionCommand::RespondApproval { request_id, decision: ApprovalDecision::Cancel }) if request_id == "first")
            );
            assert_eq!(state.first_approval("child").unwrap().id, "second");
            assert!(!state.approval_requests("parent").is_empty());

            drop(receiver);
            assert!(
                state
                    .respond_session_approval(
                        "child",
                        "second".into(),
                        ApprovalDecision::Option("accept".into())
                    )
                    .is_err()
            );
            assert_eq!(
                state.first_approval("child").unwrap().id,
                "second",
                "failed delivery must remain visible"
            );
            state.record_approval_event(
                "child",
                &AgentEvent::ApprovalResolved {
                    request_id: "second".into(),
                    decision: ApprovalDecision::Option("accept".into()),
                },
            );
            assert!(
                state
                    .session_status_snapshot("child")
                    .unwrap()
                    .pending_approvals
                    .is_empty()
            );
            assert_eq!(
                state.child_status_json(&state.sessions[0], &Timeline::default())["approval_request_id"],
                serde_json::Value::Null
            );
            assert!(!state.approval_requests("parent").is_empty());
            state.record_approval_event(
                "parent",
                &AgentEvent::TurnCompleted {
                    turn_id: "turn".into(),
                    status: TurnStatus::Completed,
                    usage: None,
                },
            );
            assert!(state.approval_requests("parent").is_empty());
        }
    }
}
