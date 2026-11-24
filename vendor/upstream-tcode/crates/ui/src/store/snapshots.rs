use std::path::PathBuf;

use tcode_core::{
    project::WorktreeInfo,
    session::{PendingUserInput, Timeline},
    settings::Settings,
    ui::{RightTab, WorkspaceMode},
};
use tcode_protocol::{ProvidersStatus, QueuedMessageStatus, SessionStatus, TerminalContextStatus};

use crate::conversation_ui::ConversationUiState;

#[derive(Clone)]
pub struct ComposerActiveModel {
    pub provider: agent::ProviderKind,
    pub model: Option<String>,
    pub acp_agent_id: Option<String>,
    pub profile_id: Option<String>,
}

#[derive(Clone)]
pub struct ComposerCheckoutState {
    pub branch: String,
    pub branches: Vec<String>,
    pub checkout_blocked: bool,
    pub is_draft: bool,
    pub worktree_base: Option<String>,
    pub worktree: Option<WorktreeInfo>,
}

#[derive(Clone)]
pub struct ComposerQueue {
    pub messages: Vec<QueuedMessageStatus>,
    pub can_steer: bool,
    pub agent: &'static str,
}

/// The complete replica-derived state consumed by composer views in one frame.
#[derive(Clone)]
pub struct ComposerState {
    pub has_active_session: bool,
    pub terminal_contexts: Vec<TerminalContextStatus>,
    pub relay_confirmation: Option<(String, String)>,
    pub active_cwd: Option<PathBuf>,
    pub provider_commands: Vec<agent::ProviderCommand>,
    pub attachments_dir: Option<PathBuf>,
    pub pending_user_input: Option<PendingUserInput>,
    pub active_model: Option<ComposerActiveModel>,
    pub model_pending_restart: bool,
    pub active_model_spec: Option<agent::ModelSpec>,
    pub active_option_descriptors: Vec<agent::OptionDescriptor>,
    pub active_option_selections: Vec<agent::OptionSelection>,
    pub requested_option_selections: Vec<agent::OptionSelection>,
    pub options_pending_restart: bool,

    pub token_usage: Option<agent::TokenUsage>,
    pub goal: Option<agent::Goal>,
    /// Account rate-limit windows for the profile driving this session.
    pub usage: Option<tcode_core::usage::ProviderUsage>,
    pub provider: Option<agent::ProviderKind>,
    pub queue: Option<ComposerQueue>,
    pub steering_supported: bool,
    pub preparing_worktree: bool,
    pub checkout: Option<ComposerCheckoutState>,
    pub turn_running: bool,
    pub stopping: bool,
    pub context_window: Option<u64>,
    pub native_rewind_blocked: bool,
    pub checkout_blocked: bool,
    pub conversation_read_only: bool,
    pub terminal_limit_reached: bool,
    pub terminal_split_available: bool,
    pub pending_approval: Option<agent::ApprovalRequest>,
    pub pending_approval_count: usize,
}

pub(crate) fn composer_state(
    status: Option<&SessionStatus>,
    _timeline: Option<&Timeline>,
    settings: &Settings,
    providers: &ProvidersStatus,
) -> ComposerState {
    let provider = status.map(|status| status.provider);
    let active_model_spec = status.and_then(|status| {
        let model = status.requested_model.as_deref()?;
        providers
            .model_catalogs
            .get(&status.provider)?
            .iter()
            .find(|spec| spec.id == model)
            .cloned()
    });
    let checkout = status.and_then(|status| {
        let branch = status.git_branch.clone().or_else(|| {
            status
                .worktree
                .as_ref()
                .map(|worktree| worktree.branch.clone())
        })?;
        Some(ComposerCheckoutState {
            branch,
            branches: status.branches.clone(),
            checkout_blocked: status.checkout_blocked,
            is_draft: status.draft,
            worktree_base: match &status.draft_workspace {
                WorkspaceMode::NewWorktree { base } => Some(base.clone()),
                _ => None,
            },
            worktree: status.worktree.clone(),
        })
    });
    let token_usage = status.and_then(|status| {
        status.usage.map(|mut usage| {
            usage.context_window = status.context_window;
            usage
        })
    });

    // The session names its profile explicitly only when it is not on the
    // provider's built-in one; both resolve into the same usage map.
    let usage = status.and_then(|status| {
        let profile_id = status
            .requested_profile_id
            .clone()
            .unwrap_or_else(|| Settings::builtin_profile_id(status.provider).to_owned());
        settings
            .resolved_profile(&profile_id)
            .filter(|profile| profile.supports_account_usage())?;
        providers.provider_usage.get(&profile_id).cloned()
    });

    ComposerState {
        has_active_session: status.is_some(),
        terminal_contexts: status
            .map(|status| status.terminal_contexts.clone())
            .unwrap_or_default(),
        relay_confirmation: status.and_then(|status| status.relay_confirmation.clone()),
        active_cwd: status.map(|status| status.cwd.clone()),
        provider_commands: status
            .map(|status| status.provider_commands.clone())
            .unwrap_or_default(),
        attachments_dir: status.map(|status| status.attachments_dir.clone()),
        pending_user_input: status.and_then(|status| status.pending_user_input.clone()),
        active_model: status.map(|status| ComposerActiveModel {
            provider: status.provider,
            model: status.requested_model.clone(),
            acp_agent_id: status.acp_agent_id.clone(),
            profile_id: status.requested_profile_id.clone(),
        }),
        model_pending_restart: status.is_some_and(|status| status.model_pending_restart),
        active_model_spec,
        active_option_descriptors: status
            .map(|status| status.provider_option_descriptors.clone())
            .unwrap_or_default(),
        active_option_selections: status
            .map(|status| status.provider_option_selections.clone())
            .unwrap_or_default(),
        requested_option_selections: status
            .map(|status| status.provider_option_requested_selections.clone())
            .unwrap_or_default(),
        options_pending_restart: status.is_some_and(|status| status.options_pending_restart),
        token_usage,
        goal: status.and_then(|status| status.goal.clone()),
        usage,
        provider,
        queue: status.map(|status| ComposerQueue {
            messages: status.queued_messages.clone(),
            can_steer: status.steering_supported,
            agent: status.provider.display_name(),
        }),
        steering_supported: status.is_some_and(|status| status.steering_supported),
        preparing_worktree: status.is_some_and(|status| status.preparing_worktree),
        checkout,
        turn_running: status.is_some_and(|status| status.activity.turn_running),
        stopping: status.is_some_and(|status| status.stopping),
        context_window: status.and_then(|status| status.context_window),
        native_rewind_blocked: status.is_none_or(|status| status.native_rewind_blocked),
        checkout_blocked: status.is_none_or(|status| status.checkout_blocked),
        conversation_read_only: status.is_some_and(|status| status.conversation_read_only),
        terminal_limit_reached: status.is_some_and(|status| status.terminal_limit_reached),
        terminal_split_available: status.is_some_and(|status| status.terminal_split_available),
        pending_approval: status.and_then(|status| status.pending_approvals.first().cloned()),
        pending_approval_count: status.map_or(0, |status| status.pending_approvals.len()),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PanelState {
    pub right_panel_open: bool,
    pub right_tab: RightTab,
    pub right_panel_expanded: bool,
    pub terminal_open: bool,
    pub terminal_height: f32,
}

pub(crate) fn panel_state(
    ui: Option<&ConversationUiState>,
    _timeline: Option<&Timeline>,
) -> PanelState {
    PanelState {
        right_panel_open: ui.is_some_and(|ui| ui.right_panel_open),
        right_tab: ui.map_or_else(RightTab::default, |ui| ui.right_tab),
        right_panel_expanded: ui.is_some_and(|ui| ui.right_panel_expanded),
        terminal_open: ui.is_some_and(|ui| ui.terminal_open),
        terminal_height: ui.map_or(240., |ui| ui.terminal_height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_status() -> SessionStatus {
        SessionStatus {
            session_id: "session-1".into(),
            title: "Test".into(),
            cwd: PathBuf::from("/workspace"),
            attachments_dir: PathBuf::from("/attachments"),
            provider: agent::ProviderKind::Codex,
            requested_model: Some("gpt-test".into()),
            requested_profile_id: None,
            acp_agent_id: None,
            project_id: Some("project-1".into()),
            queued_messages: Vec::new(),
            review_comment_drafts: Vec::new(),
            terminals: Vec::new(),
            active_terminal_id: None,
            terminal_splits: Vec::new(),
            terminal_contexts: Vec::new(),
            terminal_open: false,
            terminal_height: 240.,
            delivery_in_flight: None,
            activity: tcode_protocol::SessionActivity {
                working: false,
                turn_running: false,
                waiting: false,
                waiting_for_approval: false,
                waiting_for_input: false,
                failed: false,
                unread: false,
                fork: tcode_protocol::ForkAvailability::Available,
                agent: None,
            },
            stopping: false,
            turn_retry: None,
            goal: None,
            native_rewind_blocked: false,
            checkout_blocked: false,
            conversation_read_only: false,
            terminal_limit_reached: false,
            terminal_split_available: false,
            usage: None,
            context_window: None,
            running_turn: None,
            pending_approvals: Vec::new(),
            pending_user_input: None,
            steering_supported: true,
            provider_option_descriptors: Vec::new(),
            provider_option_selections: Vec::new(),
            provider_option_requested_selections: Vec::new(),
            provider_commands: Vec::new(),
            git_branch: Some("main".into()),
            branches: vec!["main".into()],
            draft: false,
            draft_workspace: WorkspaceMode::LocalCheckout,
            worktree: None,
            preparing_worktree: false,
            relay_confirmation: None,
            native_rewind_pending: false,
            native_rewind_prefill_available: false,
            model_pending_restart: false,
            options_pending_restart: false,
            injected_tools: None,
            control: None,
        }
    }

    #[test]
    fn account_usage_eligibility_does_not_hide_session_context_or_supported_errors() {
        let mut settings: Settings = serde_json::from_str(r#"{"profiles":{"custom":{"kind":"claude_code","env":[{"name":"ANTHROPIC_BASE_URL","value":"https://api.example.com/anthropic"}]}}}"#).unwrap();
        let mut status = session_status();
        status.provider = agent::ProviderKind::ClaudeCode;
        status.requested_profile_id = Some("custom".into());
        let mut timeline = Timeline::default();
        timeline.apply_at(
            None,
            &agent::AgentEvent::TokenUsage(agent::TokenUsage {
                freshness: agent::ContextFreshness::Current,
                used_tokens: Some(1234),
                ..Default::default()
            }),
        );
        let mut providers = ProvidersStatus::default();
        providers.provider_usage.insert(
            "custom".into(),
            tcode_core::usage::ProviderUsage {
                error: Some("temporarily unreachable".into()),
                ..Default::default()
            },
        );
        status.usage = timeline.usage;
        let custom = composer_state(Some(&status), Some(&timeline), &settings, &providers);
        assert_eq!(custom.token_usage.unwrap().used_tokens, Some(1234));
        assert!(custom.usage.is_none());
        settings
            .profiles
            .get_mut("custom")
            .unwrap()
            .settings
            .env
            .clear();
        let native = composer_state(Some(&status), Some(&timeline), &settings, &providers);
        assert_eq!(
            native.usage.unwrap().error.as_deref(),
            Some("temporarily unreachable")
        );
        assert_eq!(native.token_usage.unwrap().used_tokens, Some(1234));
    }

    #[test]
    #[cfg(unix)]
    fn claude_usage_replays_adapter_timeline_and_composer_across_resume() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "tcode-usage-replay-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let binary = root.join("claude-fixture");
        std::fs::write(&binary, "#!/bin/sh\ncase \"$*\" in *--version*) echo '2.1.200'; exit;; esac\nIFS= read -r request\nif [ \"$TCODE_USAGE_INTERRUPTED\" = 1 ]; then IFS= read -r request; fi\ncat \"$TCODE_USAGE_FIXTURE\"\nwhile IFS= read -r request; do :; done\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut timeline = Timeline::default();
        let mut replay = Timeline::default();
        let mut status = session_status();
        status.provider = agent::ProviderKind::ClaudeCode;
        status.requested_model = Some("claude-opus-5".into());
        status.provider_option_selections = vec![agent::OptionSelection {
            id: "contextWindow".into(),
            value: serde_json::json!(300000),
        }];
        let settings = Settings::default();
        let providers = ProvidersStatus::default();
        let mut recorded_events = Vec::new();
        for (index, fixture) in [
            include_str!("../../../agent/tests/fixtures/claude/usage_scope.jsonl"),
            include_str!("../../../agent/tests/fixtures/claude/usage_resume.jsonl"),
            include_str!("../../../agent/tests/fixtures/claude/usage_recorded.jsonl"),
        ]
        .into_iter()
        .enumerate()
        {
            let path = root.join(format!("fixture-{index}.jsonl"));
            std::fs::write(&path, fixture).unwrap();
            smol::block_on(async {
                let handle = agent::claude::start(agent::SessionOptions {
                    cwd: root.clone(),
                    model: Some("claude-opus-5".into()),
                    resume: (index > 0).then(|| {
                        agent::ResumeCursor(serde_json::json!({"session_id":"fixture-session"}))
                    }),
                    fork: false,
                    binary_path: Some(binary.clone()),
                    option_selections: vec![],
                    mcp_servers: vec![],
                    launch_env: agent::LaunchEnv {
                        home: Some(root.clone()),
                        env: vec![
                            (
                                "TCODE_USAGE_FIXTURE".into(),
                                path.to_string_lossy().into_owned(),
                            ),
                            (
                                "TCODE_USAGE_INTERRUPTED".into(),
                                if index == 1 { "1" } else { "0" }.into(),
                            ),
                        ],
                    },
                    extra_args: vec![],
                    acp: None,
                    instructions: None,
                })
                .await
                .unwrap();
                handle
                    .commands
                    .send(agent::SessionCommand::SendTurn {
                        delivery_id: 1,
                        text: "fixture".into(),
                        options: None,
                        attachments: vec![],
                    })
                    .await
                    .unwrap();
                if index == 1 {
                    handle
                        .commands
                        .send(agent::SessionCommand::Interrupt)
                        .await
                        .unwrap();
                }
                loop {
                    let event =
                        smol::future::race(async { handle.events.recv().await.unwrap() }, async {
                            smol::Timer::after(std::time::Duration::from_secs(10)).await;
                            panic!("fixture adapter timed out")
                        })
                        .await;
                    timeline.apply_at(Some(1 + recorded_events.len() as u64), &event);
                    status.usage = timeline.usage;
                    status.context_window = timeline.usage.and_then(|usage| usage.context_window);
                    let snapshot =
                        composer_state(Some(&status), Some(&timeline), &settings, &providers);
                    if let agent::AgentEvent::ContextCompacted(c) = &event {
                        let u = snapshot.token_usage.unwrap();
                        if c.in_progress {
                            assert_eq!(u.freshness, agent::ContextFreshness::Compacting);
                        } else {
                            assert_eq!(u.used_tokens, None);
                            assert_eq!(c.pre_tokens, Some(500260));
                            assert_eq!(c.trigger.as_deref(), Some("manual"));
                            assert_eq!(crate::context_meter::used_tokens(&u), None);
                        }
                    }
                    if let agent::AgentEvent::TokenUsage(u) = &event {
                        assert_ne!(
                            u.used_tokens,
                            Some(900000),
                            "subagent context must not enter main usage"
                        );
                        if u.output_tokens == Some(20) {
                            assert_eq!(u.used_tokens, Some(400150));
                        }
                    }
                    let completed = matches!(event, agent::AgentEvent::TurnCompleted { .. });
                    recorded_events.push(event);
                    if completed {
                        break;
                    }
                }
                handle
                    .commands
                    .send(agent::SessionCommand::Shutdown)
                    .await
                    .unwrap();
            });
            status.usage = timeline.usage;
            status.context_window = timeline.usage.and_then(|usage| usage.context_window);
            let snapshot = composer_state(Some(&status), Some(&timeline), &settings, &providers);
            let usage = snapshot.token_usage.unwrap();
            assert_eq!(usage.used_tokens, Some([500260, 60, 20764][index]));
            assert_eq!(
                usage.total_processed_tokens,
                Some([4001300, 4001365, 4063449][index])
            );
            if index == 1 {
                assert_eq!(
                    timeline.last_turn_status,
                    Some(agent::TurnStatus::Interrupted)
                );
            }
            // A repeated persisted completion is idempotent, including a cancelled result.
            timeline.apply_at(Some(99), recorded_events.last().unwrap());
            assert_eq!(
                timeline.usage.unwrap().total_processed_tokens,
                usage.total_processed_tokens
            );
        }
        for (index, event) in recorded_events.iter().enumerate() {
            replay.apply_at(Some(1 + index as u64), event);
        }
        assert_eq!(replay.usage, timeline.usage);
        let untimed = Timeline::fold_events(recorded_events.clone());
        assert_eq!(untimed.usage, timeline.usage);
        let old: agent::AgentEvent = serde_json::from_str(r#"{"type":"token_usage","used_tokens":4100000,"input_tokens":1000000,"context_window":1000000,"total_processed_tokens":4100000}"#).unwrap();
        replay.apply_at(None, &old);
        status.usage = replay.usage;
        status.context_window = replay.usage.and_then(|usage| usage.context_window);
        let old = composer_state(Some(&status), Some(&replay), &settings, &providers)
            .token_usage
            .unwrap();
        assert_eq!(old.freshness, agent::ContextFreshness::Unknown);
        assert_eq!(
            crate::context_meter::used_tokens(&old),
            None,
            "legacy aggregate has no occupancy provenance"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
