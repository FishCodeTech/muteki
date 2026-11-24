use super::*;
use tcode_core::settings::SettingsPatch;
use tcode_core::thread_sort::ThreadSection;

impl AppState {
    pub(super) fn native_rewind_blocked(&self, active: &ActiveSession) -> bool {
        !active.meta.provider.caps().native_rewind
            || active.turn_in_flight
            || active.delivery_in_flight.is_some()
            || active.background_task_count > 0
            || active.timeline.turn_running
            || !active.queue.is_empty()
            || self.pending_native_rewinds.contains_key(&active.meta.id)
    }

    pub(super) fn checkout_blocked(active: &ActiveSession) -> bool {
        // A provider that dies without closing its timeline turn must not block
        // checkout forever: the promise is no foreground turn.
        active.turn_in_flight
    }

    pub(super) fn conversation_read_only(meta: &SessionMeta) -> bool {
        meta.native_subagent.is_some()
    }

    /// The host's own wakes are not the user's to steer or drop.
    pub(super) fn queued_message_editable(active: &ActiveSession, id: u64) -> bool {
        active.delivery_in_flight != Some(id)
            && !active.queue.iter().any(|message| {
                message.id == id
                    && matches!(
                        message.kind,
                        super::active_session::QueuedMessageKind::PullRequestWake { .. }
                    )
            })
    }

    /// Validate retained conversation writes before the pipe acknowledges ownership.
    pub(crate) fn validate_command_target(
        &self,
        command: &tcode_protocol::Command,
    ) -> Result<(), tcode_protocol::ProtocolError> {
        use tcode_protocol::Command;
        let error = |code: &str, message: &str| tcode_protocol::ProtocolError {
            code: code.into(),
            message: message.into(),
        };
        self.validate_plugin_command(command)?;
        if let Command::PatchSettings { patch } = command {
            if let SettingsPatch::ProjectSettlement { project_id, .. }
            | SettingsPatch::ProjectMergeMethod { project_id, .. }
            | SettingsPatch::ControlProjectGrant { project_id, .. } = patch
                && !self
                    .projects
                    .iter()
                    .any(|project| &project.id == project_id)
            {
                return Err(error(
                    "unknown_project",
                    "This project is no longer available.",
                ));
            }
            self.settings
                .clone()
                .apply(patch.clone())
                .map_err(|message| error("invalid_settings", message))?;
        }
        let host_kind = match command {
            Command::SetHostToken { host, .. } => {
                Some((host, self.settings.source_control.kind(host)))
            }
            Command::PatchSettings {
                patch: SettingsPatch::SourceControlHost { host, kind, .. },
            } => Some((host, *kind)),
            _ => None,
        };
        if let Some((host, kind)) = host_kind {
            // Settings and the token store lowercase and trim; a scheme or slash is not theirs to drop.
            if kind.authority(host).ok() != Some(host.trim().to_ascii_lowercase()) {
                return Err(error("invalid_host", &kind.rule()));
            }
        }
        match command {
            Command::DeleteProfile { profile_id }
                if Settings::is_builtin_profile_id(profile_id) =>
            {
                return Err(error(
                    "builtin_profile",
                    "Built-in provider profiles cannot be deleted.",
                ));
            }
            Command::TerminalInput { terminal_id, .. }
            | Command::ClearTerminal { terminal_id }
            | Command::ResizeTerminal { terminal_id, .. }
                if self.terminal_handle(*terminal_id).is_none() =>
            {
                return Err(error(
                    "unknown_terminal",
                    "This terminal is no longer available.",
                ));
            }
            Command::CancelAgent { session_id }
                if !self
                    .sessions
                    .iter()
                    .any(|meta| &meta.id == session_id && meta.is_dispatched()) =>
            {
                return Err(error(
                    "not_an_agent",
                    "This thread is not an agent dispatched by another thread.",
                ));
            }
            Command::ToggleProjectCollapsed { project_id }
            | Command::MoveSession { project_id, .. }
            | Command::SetProjectIcon { project_id, .. }
            | Command::SetProjectRoot { project_id, .. }
            | Command::DeleteProject { project_id }
                if !self
                    .projects
                    .iter()
                    .any(|project| &project.id == project_id) =>
            {
                return Err(error(
                    "unknown_project",
                    "This project is no longer available.",
                ));
            }
            Command::SetProfileSecret { profile_id, .. }
            | Command::UpdateProfileSettings { profile_id, .. }
            | Command::DeleteProfile { profile_id }
                if self.settings.resolved_profile(profile_id).is_none() =>
            {
                return Err(error(
                    "unknown_profile",
                    "This provider profile is no longer available.",
                ));
            }
            _ => {}
        }
        match command {
            Command::InstallAcpAgent { id } if self.acp_installing.contains(id) => {
                return Err(error(
                    "acp_installing",
                    "This agent is already being installed.",
                ));
            }
            Command::InstallAcpAgent { id }
                if !self.acp_marketplace().iter().any(|agent| &agent.id == id) =>
            {
                return Err(error(
                    "unknown_acp_agent",
                    "This agent is not in the registry.",
                ));
            }
            Command::UpdateAcpAgent { id, .. } | Command::SetActiveAcpAgent { id, .. }
                if !self.settings.acp_agents.contains_key(id) =>
            {
                return Err(error("unknown_acp_agent", "This agent is not installed."));
            }
            Command::SetActiveAcpAgent { session_id, id }
                if self
                    .settings
                    .acp_agents
                    .get(id)
                    .is_some_and(|agent| !agent.offered_for_new_sessions())
                    && self.resident(session_id).is_none_or(|session| {
                        session.meta.acp_agent_id.as_deref() != Some(id.as_str())
                    }) =>
            {
                return Err(error(
                    "native_acp_agent",
                    "This agent duplicates a native provider; use that provider for new sessions.",
                ));
            }
            Command::MergeWorktree { session_id } => {
                return match self.find_meta(session_id) {
                    None => Err(error(
                        "unknown_session",
                        "This thread is no longer available on the host.",
                    )),
                    Some(meta) if meta.worktree.is_none() => Err(error(
                        "no_worktree",
                        "This thread has no worktree to merge.",
                    )),
                    Some(_) => Ok(()),
                };
            }
            _ => {}
        }
        let Some(session_id) = command.session_id() else {
            return Ok(());
        };
        if matches!(
            command,
            Command::SettleSession { .. }
                | Command::UnsettleSession { .. }
                | Command::PinSession { .. }
                | Command::UnpinSession { .. }
                | Command::ReorderPinned { .. }
                | Command::ReorderActive { .. }
        ) && self
            .find_meta(session_id)
            .is_some_and(|meta| meta.archived_at.is_some())
        {
            return Err(error(
                "archived_session",
                "Unarchive this thread before changing its lifecycle.",
            ));
        }
        if let Command::PinSession {
            order_key: Some(order_key),
            ..
        }
        | Command::ReorderPinned { order_key, .. }
        | Command::ReorderActive { order_key, .. } = command
            && order_key.is_empty()
        {
            return Err(error("invalid_order_key", "An order key cannot be empty."));
        }
        // Only a thread in the section has a place in its order, so a reorder
        // raced by an unpin or a settle never pins or reopens the thread again.
        let section = self
            .find_meta(session_id)
            .map(|meta| tcode_core::thread_sort::thread_section(&meta));
        match command {
            Command::ReorderPinned { .. }
                if section.is_some_and(|section| section != ThreadSection::Pinned) =>
            {
                return Err(error("not_pinned", "This thread is not pinned."));
            }
            Command::ReorderActive { .. }
                if section.is_some_and(|section| section != ThreadSection::Active) =>
            {
                return Err(error("not_active", "This thread is not in Active."));
            }
            _ => {}
        }
        if matches!(command, Command::SettleSession { .. }) && self.settle_thread_busy(session_id) {
            return Err(error(
                "thread_busy",
                "Wait for this thread to finish before settling.",
            ));
        }
        // Index mutations operate on stored sessions, without requiring a live provider.
        if matches!(
            command,
            Command::SettleChildren { .. }
                | Command::SettleSession { .. }
                | Command::CancelAgent { .. }
                | Command::UnsettleSession { .. }
                | Command::SetAutoSettle { .. }
                | Command::PinSession { .. }
                | Command::UnpinSession { .. }
                | Command::ReorderPinned { .. }
                | Command::ReorderActive { .. }
                | Command::ArchiveSession { .. }
                | Command::UnarchiveSession { .. }
                | Command::RenameSession { .. }
                | Command::RegenerateSessionTitle { .. }
                | Command::DeleteSession { .. }
                | Command::MarkSessionUnread { .. }
                | Command::MarkSessionRead { .. }
                | Command::ForkThread { .. }
                | Command::MoveSession { .. }
                | Command::WatchPullRequest { .. }
                | Command::SetPullRequestWatchesPaused { .. }
                | Command::RunPullRequestAction { .. }
                | Command::EditPullRequestReviewDraft { .. }
                | Command::SetControlGrant { .. }
                | Command::RespondControlGrant { .. }
                | Command::AddThreadReference { .. }
                | Command::RemoveThreadReference { .. }
                | Command::SetFollowUp { .. }
                | Command::SetParked { .. }
                | Command::DismissSettleSuggestion { .. }
                | Command::RevokeSelfSettle { .. }
        ) {
            return if self.sessions.iter().any(|meta| meta.id == session_id)
                || self.resident(session_id).is_some()
            {
                Ok(())
            } else {
                Err(error(
                    "unknown_session",
                    "This thread is no longer available on the host.",
                ))
            };
        }
        let active = self.resident(session_id).ok_or_else(|| {
            error(
                "unknown_session",
                "This thread is no longer available on the host.",
            )
        })?;
        match command {
            Command::SetDraftWorkspace { .. } if !active.draft => {
                return Err(error("not_a_draft", "Only a draft can change workspace."));
            }
            Command::CheckoutBranch { .. } if Self::checkout_blocked(active) => {
                return Err(error(
                    "turn_running",
                    "Wait for the running turn before changing branches.",
                ));
            }
            Command::CaptureTerminalSelection {
                selection: None, ..
            } => return Err(error("no_selection", "Select terminal text first.")),
            Command::NewTerminal { .. } | Command::SplitTerminal { .. }
                if self.terminal_limit_reached(active) =>
            {
                return Err(error(
                    "terminal_limit",
                    "This thread has reached its terminal limit.",
                ));
            }
            Command::SplitTerminal { .. } if !self.terminal_split_available(active) => {
                return Err(error(
                    "terminal_split_unavailable",
                    "There is no terminal available to split.",
                ));
            }
            Command::RewindTurn { turn, mode, .. }
                if self.native_rewind_blocked(active)
                    || active
                        .timeline
                        .turns
                        .get(*turn)
                        .and_then(|turn| turn.provider_checkpoint_id.as_ref())
                        .is_none()
                    || (*turn == 0 && mode.includes_conversation()) =>
            {
                return Err(error(
                    "rewind_unavailable",
                    "This turn cannot be rewound now.",
                ));
            }
            Command::ActivateTerminal { terminal_id, .. }
            | Command::CloseTerminal { terminal_id, .. }
            | Command::CaptureTerminalSelection { terminal_id, .. }
                if active.terminal_workspace.terminal(*terminal_id).is_none() =>
            {
                return Err(error(
                    "unknown_terminal",
                    "This terminal is no longer available.",
                ));
            }
            Command::RemoveTerminalContext { context_id, .. }
                if !active
                    .terminal_workspace
                    .contexts
                    .iter()
                    .any(|context| context.id == *context_id) =>
            {
                return Err(error(
                    "unknown_terminal_context",
                    "This terminal selection is no longer available.",
                ));
            }
            Command::RemoveReviewComment { index, .. }
                if *index >= self.review_comments(session_id).len() =>
            {
                return Err(error(
                    "unknown_review_comment",
                    "This review comment is no longer available.",
                ));
            }
            Command::RunGitAction { .. } if self.git_busy.contains(session_id) => {
                return Err(error("git_busy", "A Git operation is already running."));
            }
            Command::RespondApproval { request_id, .. }
                if !self
                    .approval_requests(session_id)
                    .iter()
                    .any(|request| &request.id == request_id) =>
            {
                return Err(error(
                    "unknown_approval",
                    "This approval is no longer pending.",
                ));
            }
            Command::RespondUserInput { request_id, .. }
                if active
                    .timeline
                    .pending_user_input
                    .as_ref()
                    .is_none_or(|pending| pending.request_id != *request_id) =>
            {
                return Err(error(
                    "unknown_user_input",
                    "This question is no longer pending.",
                ));
            }
            Command::SteerQueued { id, .. }
            | Command::DropQueued { id, .. }
            | Command::MoveQueued { id, .. }
                if !active.queue.iter().any(|message| message.id == *id)
                    || !Self::queued_message_editable(active, *id)
                    || matches!(command, Command::MoveQueued { before: Some(before), .. }
                        if !active.queue.iter().any(|message| message.id == *before)) =>
            {
                return Err(error(
                    "unknown_queued_message",
                    "This message is no longer editable in the queue.",
                ));
            }
            _ => {}
        }
        if matches!(command, Command::Interrupt { .. })
            && !active.turn_in_flight
            && active.delivery_in_flight.is_none()
            && !active.timeline.turn_running
            && active.background_task_count == 0
        {
            return Err(error(
                "no_running_turn",
                "There is no running turn to interrupt.",
            ));
        }
        if matches!(
            command,
            Command::Interrupt { .. }
                | Command::RespondApproval { .. }
                | Command::RespondUserInput { .. }
        ) {
            match &active.runtime {
                Runtime::Live(commands) if !commands.is_closed() => {}
                _ => {
                    return Err(error(
                        "no_running_turn",
                        "The provider is no longer running.",
                    ));
                }
            }
        }
        if matches!(
            command,
            Command::SendTurn { .. }
                | Command::ScheduleTurn { .. }
                | Command::ConfirmRelayAndSend { .. }
                | Command::OrchestrateTurn { .. }
                | Command::Steer { .. }
        ) {
            if Self::conversation_read_only(&active.meta) {
                return Err(error(
                    "read_only_session",
                    "This thread is a read-only mirror.",
                ));
            }
            if !matches!(command, Command::ConfirmRelayAndSend { .. })
                && self.relay_confirmation(session_id).is_some()
            {
                return Err(error(
                    "relay_confirmation_required",
                    "Confirm the provider change before sending.",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{TestAppContext, TestStore};
    use tcode_protocol::{ClientMessage, ClientPayload, Command, HostMessage};

    /// A host is written to settings by its kind's rule: a Forgejo server keeps its port and
    /// mount, a GitHub host takes neither, and each refusal says the kind's own rule.
    #[test]
    fn a_host_is_refused_by_its_kinds_rule() {
        use tcode_core::pull_request::HostKind;
        let store = TestStore::new("host-validation");
        let state = AppState::new((*store).clone()).unwrap();
        let add = |host: &str, kind| Command::PatchSettings {
            patch: SettingsPatch::SourceControlHost {
                host: host.into(),
                kind,
                enabled: Some(true),
                account: None,
            },
        };
        assert!(
            state
                .validate_command_target(&add("git.acme.test:3000/forge", HostKind::Forgejo))
                .is_ok()
        );
        let github = state
            .validate_command_target(&add("git.acme.test:3000", HostKind::Github))
            .unwrap_err();
        assert_eq!(github.code, "invalid_host");
        assert_eq!(
            github.message,
            "Use a GitHub hostname without a URL or path."
        );
        let gitea = state
            .validate_command_target(&add("https://git.acme.test", HostKind::Gitea))
            .unwrap_err();
        assert_eq!(gitea.code, "invalid_host");
        assert!(
            gitea.message.starts_with("Use a Gitea host name"),
            "{}",
            gitea.message
        );
    }

    #[test]
    fn writes_blocked_by_host_state_receive_rejected_acks() {
        let store = TestStore::new("write-preconditions");
        let mut state = AppState::new((*store).clone()).unwrap();
        let mut context = TestAppContext::default();
        let mut cx = context.host_cx();
        let id = state.start_draft("fixture".into(), std::env::temp_dir(), &mut cx);
        for (index, expected) in [
            "read_only_session",
            "relay_confirmation_required",
            "git_busy",
            "not_a_draft",
            "turn_running",
            "acp_installing",
            "terminal_limit",
        ]
        .into_iter()
        .enumerate()
        {
            let active = state.resident_mut(&id).unwrap();
            active.meta.native_subagent = (index == 0).then(|| "mirror".into());
            if index == 1 {
                active.timeline.apply_at(
                    None,
                    &AgentEvent::ItemCompleted(agent::ThreadItem {
                        id: "history".into(),
                        parent_item_id: None,
                        content: agent::ItemContent::UserMessage {
                            text: "Earlier conversation".into(),
                            context_len: None,
                            attachments: Vec::new(),
                        },
                    }),
                );
            }
            if index == 1 {
                active.timeline.apply_at(
                    None,
                    &AgentEvent::TurnCompleted {
                        turn_id: "earlier-turn".into(),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    },
                );
            }
            active.pending_relay = (index == 1).then_some(PendingRelay {
                from_provider: ProviderKind::ClaudeCode,
                from_model: None,
                from_profile: None,
            });
            let command = match index {
                2 => {
                    state.git_busy.insert(id.clone());
                    Command::RunGitAction {
                        session_id: id.clone(),
                        action: GitAction::Push,
                        message: None,
                        included: None,
                        feature_branch: None,
                    }
                }
                3 => {
                    state.resident_mut(&id).unwrap().draft = false;
                    Command::SetDraftWorkspace {
                        session_id: id.clone(),
                        mode: WorkspaceMode::LocalCheckout,
                    }
                }
                4 => {
                    state.resident_mut(&id).unwrap().turn_in_flight = true;
                    Command::CheckoutBranch {
                        session_id: id.clone(),
                        branch: "main".into(),
                    }
                }
                5 => {
                    state.acp_installing.insert("already-installing".into());
                    Command::InstallAcpAgent {
                        id: "already-installing".into(),
                    }
                }
                6 => {
                    state.pending_terminal_spawns.insert(
                        id.clone(),
                        (0..MAX_TERMINALS_PER_SESSION as u64)
                            .map(|id| (id, TerminalSpawnAction::New))
                            .collect(),
                    );
                    Command::NewTerminal {
                        session_id: id.clone(),
                    }
                }
                _ => Command::SendTurn {
                    session_id: id.clone(),
                    text: "must not vanish".into(),
                    attachment_paths: Vec::new(),
                },
            };
            crate::pipe::handle_client_message(
                &mut state,
                &mut cx,
                ClientMessage {
                    principal: None,
                    id: 42,
                    key: Some(format!("write-{index}")),
                    payload: ClientPayload::Command(command),
                },
                tcode_client::attachment::AttachmentPolicy::LOCAL,
            );
            assert!(
                context
                    .drain_outgoing()
                    .iter()
                    .any(|message| matches!(message,
                        HostMessage::Ack { id: 42, result: Err(error) } if error.code == expected
                    )),
                "expected rejected Ack: {expected}"
            );
            assert!(state.resident(&id).unwrap().queue.is_empty());
        }
    }
    #[test]
    fn checkout_is_admitted_after_provider_death_with_an_unclosed_timeline_turn() {
        use crate::app::test_support::TestClientState;
        let store = TestStore::new("checkout-after-death");
        let mut context = TestAppContext::default();
        let state = context.new_entity(TestClientState::new((*store).clone()));
        let id = state.update(&mut context, |state, _| {
            let meta = SessionMeta::new(ProviderKind::Codex, store.root().to_path_buf(), None);
            let id = meta.id.clone();
            let mut active = ActiveSession::new(meta, false, Vec::new());
            active.timeline.apply_at(
                None,
                &AgentEvent::TurnStarted {
                    turn_id: "unclosed".into(),
                },
            );
            active.turn_in_flight = true;
            active.mark_dead();
            assert!(active.timeline.turn_running);
            state.install_selected(active);
            assert!(!state.session_status_snapshot(&id).unwrap().checkout_blocked);
            id
        });
        state.dispatch_command(
            &mut context,
            43,
            Command::CheckoutBranch {
                session_id: id,
                branch: "main".into(),
            },
        );
        assert!(context.drain_outgoing().iter().any(|message| matches!(
            message,
            HostMessage::Ack {
                id: 43,
                result: Ok(_)
            }
        )));
        // Admission starts a real git child. Drive its completion before the test exits.
        context.run_next();
        assert!(context.drain_outgoing().iter().any(|message| matches!(
            message,
            HostMessage::Event(EventEnvelope {
                event: ServerEvent::Runtime(RuntimeEvent::Error(RuntimeError::External(_))),
                ..
            })
        )));
    }

    #[test]
    fn provider_channel_rejection_is_not_acknowledged_as_success() {
        let store = TestStore::new("provider-write-rejection");
        let mut state = AppState::new((*store).clone()).unwrap();
        let mut context = TestAppContext::default();
        let mut cx = context.host_cx();
        let id = state.start_draft("fixture".into(), std::env::temp_dir(), &mut cx);
        let (commands, _receiver) = smol::channel::bounded(1);
        commands.try_send(SessionCommand::Interrupt).unwrap();
        let active = state.resident_mut(&id).unwrap();
        active.runtime = Runtime::Live(commands);
        active.turn_in_flight = true;
        active.timeline.pending_user_input = Some(tcode_core::session::PendingUserInput {
            request_id: "question".into(),
            questions: Vec::new(),
            delivery: agent::UserInputDelivery::Blocking,
        });
        state.record_approval_event(
            &id,
            &AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                id: "approval".into(),
                parent_item_id: None,
                turn_id: None,
                options: Vec::new(),
                kind: agent::ApprovalKind::ExecCommand {
                    command: "echo fixture".into(),
                    cwd: None,
                    reason: None,
                },
            }),
        );
        for (index, command) in [
            Command::Interrupt {
                session_id: id.clone(),
            },
            Command::RespondApproval {
                session_id: id.clone(),
                request_id: "approval".into(),
                decision: ApprovalDecision::Option("accept".into()),
            },
            Command::RespondUserInput {
                session_id: id.clone(),
                request_id: "question".into(),
                answers: Default::default(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            crate::pipe::handle_client_message(
                &mut state,
                &mut cx,
                ClientMessage {
                    principal: None,
                    id: 42,
                    key: Some(format!("blocked-{index}")),
                    payload: ClientPayload::Command(command),
                },
                tcode_client::attachment::AttachmentPolicy::LOCAL,
            );
            assert!(context.drain_outgoing().iter().any(|message| matches!(message,
                HostMessage::Ack { id: 42, result: Err(error) } if error.code == "provider_unavailable"
            )));
        }
        assert!(
            !state.approval_requests(&id).is_empty(),
            "failed delivery must leave the approval pending"
        );
    }
}
