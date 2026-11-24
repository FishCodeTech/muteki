use super::*;
use control_mcp::{
    BatchResult, DeleteThreadsParams, Outcome, PatchSettingsParams, RespondInputParams,
    RewindParams, RewindScope, TerminalOp, TerminalParams,
};
use tcode_core::settings::SettingsPatch;
use tcode_protocol::{Command, Principal};

/// The settings an agent may change, as display preferences only. Everything else touches
/// authority (approvals, computer use, browser evaluation, plugins, child approval, grants,
/// hosting, credentials), automation (settlement rules and tiers, resuming, fallbacks, title
/// and review models) or UI state (sidebar, last project), and stays the user's. The match is
/// exhaustive so a new setting is decided here, not allowed by default.
pub(super) fn settings_patch_never_exposed(patch: &SettingsPatch) -> bool {
    match patch {
        SettingsPatch::Language(_)
        | SettingsPatch::ThemeMode(_)
        | SettingsPatch::WordWrapDiffs(_)
        | SettingsPatch::AutoOpenTaskPanel(_)
        | SettingsPatch::LiveCommandPanelDisabled(_)
        | SettingsPatch::SidebarProviderMarks(_)
        | SettingsPatch::InactiveFrameThrottleDisabled(_)
        | SettingsPatch::FallbackReviewAdvisor(_) => false,
        SettingsPatch::SourceControlHost { .. }
        | SettingsPatch::RemoveSourceControlHost { .. }
        | SettingsPatch::SkipDeleteConfirmation(_)
        | SettingsPatch::ProviderUpdateChecksDisabled(_)
        | SettingsPatch::AbortOnModelFallback(_)
        | SettingsPatch::ResumeOnLimitReset(_)
        | SettingsPatch::AutoSettleAfterDays(_)
        | SettingsPatch::AutoSettleOnMerge(_)
        | SettingsPatch::ProjectSettlement { .. }
        | SettingsPatch::ProjectMergeMethod { .. }
        | SettingsPatch::RemoveAgentCreditsOnMerge(_)
        | SettingsPatch::OrchestrateDecisionModels(_)
        | SettingsPatch::OrchestrateChildModels(_)
        | SettingsPatch::OrchestrateChildApproval(_)
        | SettingsPatch::OrchestrateChildWorktrees(_)
        | SettingsPatch::ComputerUseEnabled(_)
        | SettingsPatch::ComputerUseImageMode(_)
        | SettingsPatch::ComputerUseAllowInput(_)
        | SettingsPatch::ComputerUseAllowForegroundFallback(_)
        | SettingsPatch::ComputerUseShowAgentCursor(_)
        | SettingsPatch::BrowserEnabled(_)
        | SettingsPatch::BrowserHomeUrl(_)
        | SettingsPatch::BrowserAllowEvaluate(_)
        | SettingsPatch::PluginManagementEnabled(_)
        | SettingsPatch::PluginManagementProvider { .. }
        | SettingsPatch::ControlEnabled(_)
        | SettingsPatch::ControlDefaultGrant(_)
        | SettingsPatch::ControlProjectGrant { .. }
        | SettingsPatch::SettlementAutomation(_)
        | SettingsPatch::TitleGenerationProvider(_)
        | SettingsPatch::TitleGenerationModel(_)
        | SettingsPatch::TitleGenerationProfileId(_)
        | SettingsPatch::FallbackReviewProvider(_)
        | SettingsPatch::FallbackReviewModel(_)
        | SettingsPatch::FallbackReviewProfileId(_)
        | SettingsPatch::SidebarLayout(_)
        | SettingsPatch::RemoteHostingEnabled(_)
        | SettingsPatch::Traverse(_)
        | SettingsPatch::RemoteHostName(_)
        | SettingsPatch::LastProject(_) => true,
    }
}

/// The context every privileged write runs in: the host's full authority, authored by the
/// agent, never the user.
fn agent_cx(cx: &HostCx) -> HostCx {
    let mut cx = cx.clone();
    cx.principal = Principal::Full;
    cx.origin = Some(MessageOrigin::Agent);
    cx.author = None;
    cx
}

fn refusal(error: tcode_protocol::ProtocolError) -> String {
    format!("refused: {}", error.message)
}

#[derive(Serialize)]
struct TerminalAnswer {
    thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_id: Option<u64>,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

impl AppState {
    pub(super) fn op_respond_input(
        &mut self,
        caller: &Caller,
        params: RespondInputParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let target = params.thread_id;
        let pending = self
            .resident(&target)
            .and_then(|session| session.timeline.pending_user_input.as_ref())
            .map(|pending| pending.request_id.clone());
        let request_id = match (params.question_id, pending) {
            (Some(asked), Some(pending)) if asked == pending => pending,
            (None, Some(pending)) => pending,
            (Some(asked), _) => {
                return Err(format!(
                    "refused: request {asked} is not pending in thread {target}"
                ));
            }
            (None, None) => {
                return Err(format!(
                    "refused: thread {target} has no pending user-input request"
                ));
            }
        };
        let mut cx = agent_cx(cx);
        let command = Command::RespondUserInput {
            session_id: target.clone(),
            request_id: request_id.clone(),
            answers: params.answers.clone(),
        };
        self.validate_command_target(&command).map_err(refusal)?;
        self.respond_user_input(&target, request_id.clone(), params.answers.clone(), &mut cx)
            .map_err(refusal)?;
        self.audit(
            caller,
            &target,
            action::RESPOND_INPUT,
            Some(request_id.clone()),
            &mut cx,
        );
        Ok(serde_json::json!({ "thread_id": target, "request_id": request_id, "answered": true }))
    }

    pub(super) fn op_terminal(
        &mut self,
        caller: &Caller,
        params: TerminalParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let target = params.thread_id;
        let mut cx = agent_cx(cx);
        let answer = match params.op {
            TerminalOp::Open => {
                if self.resident(&target).is_none() {
                    let meta = self
                        .find_meta(&target)
                        .filter(|meta| meta.archived_at.is_none())
                        .ok_or_else(|| format!("refused: thread {target} is archived"))?;
                    self.load_background_session(meta, &mut cx);
                }
                self.validate_command_target(&Command::NewTerminal {
                    session_id: target.clone(),
                })
                .map_err(refusal)?;
                self.new_terminal(&target, &mut cx);
                self.audit(caller, &target, action::TERMINAL_OPEN, None, &mut cx);
                TerminalAnswer {
                    thread_id: target,
                    terminal_id: None,
                    status: "opening",
                    note: Some(
                        "the shell starts in the background; get_thread lists its terminal id",
                    ),
                }
            }
            TerminalOp::Input { terminal_id, text } => {
                self.terminal_in_thread(terminal_id, &target)?;
                self.validate_command_target(&Command::TerminalInput {
                    terminal_id,
                    bytes: text.clone().into_bytes(),
                })
                .map_err(refusal)?;
                self.write_terminal_input(terminal_id, text.into_bytes(), &mut cx);
                self.audit(
                    caller,
                    &target,
                    action::TERMINAL_INPUT,
                    Some(terminal_id.to_string()),
                    &mut cx,
                );
                TerminalAnswer {
                    thread_id: target,
                    terminal_id: Some(terminal_id),
                    status: "written",
                    note: None,
                }
            }
            TerminalOp::Close { terminal_id } => {
                self.terminal_in_thread(terminal_id, &target)?;
                self.validate_command_target(&Command::CloseTerminal {
                    session_id: target.clone(),
                    terminal_id,
                })
                .map_err(refusal)?;
                self.close_terminal(&target, terminal_id, &mut cx);
                self.audit(
                    caller,
                    &target,
                    action::TERMINAL_CLOSE,
                    Some(terminal_id.to_string()),
                    &mut cx,
                );
                TerminalAnswer {
                    thread_id: target,
                    terminal_id: Some(terminal_id),
                    status: "closed",
                    note: None,
                }
            }
        };
        Ok(serde_json::json!(answer))
    }

    /// The grant reached `thread_id`; a terminal id is only as reachable as the thread
    /// whose panel holds it.
    fn terminal_in_thread(&self, terminal_id: u64, thread_id: &str) -> Result<(), String> {
        if self.terminal_owner(terminal_id) == Some(thread_id) {
            Ok(())
        } else {
            Err(format!(
                "refused: terminal {terminal_id} is not one of thread {thread_id}'s terminals"
            ))
        }
    }

    pub(super) fn op_delete_threads(
        &mut self,
        caller: &Caller,
        params: DeleteThreadsParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let remove_worktree = params.remove_worktree.unwrap_or(false);
        let dry_run = params.dry_run.unwrap_or(false);
        let mut cx = agent_cx(cx);
        let mut results = Vec::with_capacity(params.thread_ids.len());
        for target in params.thread_ids {
            let skipped = |reason: String| BatchResult {
                target: target.clone(),
                outcome: Outcome::Skipped,
                reason: Some(reason),
                side_effects: None,
            };
            let meta = match self.batch_target(caller, &target, false) {
                Ok(meta) => meta,
                Err(reason) => {
                    results.push(skipped(reason));
                    continue;
                }
            };
            let ids = descendant_session_ids(&self.sessions, &target);
            if target == caller.thread_id || ids.contains(&caller.thread_id) {
                results.push(skipped("would delete the calling thread itself".into()));
                continue;
            }
            if let Err(error) = self.validate_command_target(&Command::DeleteSession {
                session_id: target.clone(),
                remove_worktree,
            }) {
                results.push(skipped(error.message));
                continue;
            }
            let descendants = ids.iter().filter(|id| **id != target).count();
            let mut side_effects = vec![format!(
                "deletes \"{}\" and {descendants} thread(s) under it, permanently",
                meta.title
            )];
            if meta.worktree.is_some() {
                side_effects.push(if remove_worktree {
                    format!(
                        "removes its worktree {} unless another thread shares it",
                        meta.cwd.display()
                    )
                } else {
                    format!("keeps its worktree {}", meta.cwd.display())
                });
            }
            if dry_run {
                results.push(BatchResult {
                    target,
                    outcome: Outcome::WouldApply,
                    reason: None,
                    side_effects: Some(side_effects),
                });
                continue;
            }
            self.delete_session(&target, remove_worktree, &mut cx);
            self.audit(
                caller,
                &caller.thread_id,
                action::DELETE_THREADS,
                Some(meta.title.clone()),
                &mut cx,
            );
            results.push(BatchResult {
                target,
                outcome: Outcome::Applied,
                reason: None,
                side_effects: Some(side_effects),
            });
        }
        Ok(serde_json::json!({ "results": results }))
    }

    pub(super) fn op_rewind(
        &mut self,
        caller: &Caller,
        params: RewindParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let target = params.thread_id;
        if self
            .find_meta(&target)
            .is_some_and(|meta| !meta.provider.caps().native_rewind)
        {
            return Err(format!(
                "refused: thread {target}'s provider has no native rewind"
            ));
        }
        if self.resident(&target).is_none() {
            return Err(format!(
                "refused: thread {target}'s history is not loaded on the host; send it a message or open it first"
            ));
        }
        let mode = match params.mode {
            RewindScope::Files => RewindMode::Files,
            RewindScope::Conversation => RewindMode::Conversation,
            RewindScope::FilesAndConversation => RewindMode::FilesAndConversation,
        };
        self.validate_command_target(&Command::RewindTurn {
            session_id: target.clone(),
            turn: params.turn,
            mode,
        })
        .map_err(refusal)?;
        let mut cx = agent_cx(cx);
        self.rewind_turn(&target, params.turn, mode, &mut cx);
        self.audit(
            caller,
            &target,
            action::REWIND,
            Some(params.turn.to_string()),
            &mut cx,
        );
        Ok(serde_json::json!({
            "thread_id": target,
            "turn": params.turn,
            "mode": params.mode,
            "status": "requested",
        }))
    }

    pub(super) fn op_patch_settings(
        &mut self,
        caller: &Caller,
        params: PatchSettingsParams,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let name = params
            .patch
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let patch: SettingsPatch = serde_json::from_value(params.patch)
            .map_err(|error| format!("invalid settings patch: {error}"))?;
        self.validate_command_target(&Command::PatchSettings {
            patch: patch.clone(),
        })
        .map_err(refusal)?;
        let mut cx = agent_cx(cx);
        self.patch_settings(patch, &mut cx);
        self.audit(
            caller,
            &caller.thread_id,
            action::PATCH_SETTINGS,
            Some(name.clone()),
            &mut cx,
        );
        Ok(serde_json::json!({ "applied": name }))
    }
}
