use agent::{ProviderKind, RewindMode};
use tcode_core::git::GitAction;
use tcode_protocol::{
    GitActionRequest, MergeWorktreeFailure, NoticeSeverity, PluginOperationTarget, RuntimeEffect,
    RuntimeError, RuntimeNotice, RuntimeNotification as RuntimeEvent, RuntimeOperationId,
    RuntimeToast,
};

use crate::toast::ToastKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RuntimeEventSeverity {
    Error,
    Warning,
    Success,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PresentedRuntimeEvent {
    pub severity: RuntimeEventSeverity,
    pub message: String,
}

pub(super) fn apply_runtime_effect(effect: &RuntimeEffect) {
    match effect {
        RuntimeEffect::ApplyLocale { language } => {
            crate::settings::apply_locale(language.as_deref());
        }
        RuntimeEffect::CopyToClipboard { .. } => {
            unreachable!("clipboard effects are applied by the app shell")
        }
    }
}

pub(super) fn present_runtime_event(event: &RuntimeEvent) -> PresentedRuntimeEvent {
    let (severity, message) = match event {
        RuntimeEvent::Error(error) => {
            let message = match error {
                RuntimeError::External(message) | RuntimeError::ProviderMessage(message) => {
                    message.clone()
                }
                RuntimeError::PersistSettings { error } => {
                    crate::tr!("errors.persist_settings", error = error).into_owned()
                }
                RuntimeError::TerminalStart { error } => {
                    crate::tr!("errors.terminal_start", error = error).into_owned()
                }
                RuntimeError::TerminalRestart { error } => {
                    crate::tr!("errors.terminal_restart", error = error).into_owned()
                }
                RuntimeError::PersistProject { error } => {
                    crate::tr!("errors.persist_project", error = error).into_owned()
                }
                RuntimeError::WorktreeRemove { error } => {
                    crate::tr!("errors.worktree_remove", error = error).into_owned()
                }
                RuntimeError::DeleteSession { error } => {
                    crate::tr!("errors.delete_session", error = error).into_owned()
                }
                RuntimeError::DeleteProject { error } => {
                    crate::tr!("errors.delete_project", error = error).into_owned()
                }
                RuntimeError::NativeRewindBlocked => crate::tr!("chat.rewind_blocked").into_owned(),
                RuntimeError::PersistEvent { error } => {
                    crate::tr!("errors.persist_event", error = error).into_owned()
                }
                RuntimeError::WorktreeAdd { error } => {
                    crate::tr!("errors.worktree_add", error = error).into_owned()
                }
                RuntimeError::PersistSession { error } => {
                    crate::tr!("errors.persist_session", error = error).into_owned()
                }
                RuntimeError::TitleGenerationFailed => {
                    crate::tr!("errors.title_generation_failed").into_owned()
                }
                RuntimeError::TitleGenerationEmpty => {
                    crate::tr!("errors.title_generation_empty").into_owned()
                }
                RuntimeError::ProcessGone => crate::tr!("errors.process_gone").into_owned(),
                RuntimeError::SteerUnsupported { agent } => {
                    crate::tr!("composer.steer_unsupported", agent = agent).into_owned()
                }
                RuntimeError::DirtyTree => crate::tr!("notice.dirty_tree").into_owned(),
                RuntimeError::ProviderStart { error } => {
                    crate::tr!("errors.provider_start", error = error).into_owned()
                }
                RuntimeError::WorkingDirectoryMissing { cwd, .. } => crate::tr!(
                    "errors.working_directory_missing",
                    cwd = cwd.to_string_lossy()
                )
                .into_owned(),
                RuntimeError::ProviderClosed {
                    reason: Some(reason),
                } => crate::tr!("errors.provider_closed_reason", reason = reason).into_owned(),
                RuntimeError::ProviderClosed { reason: None } => {
                    crate::tr!("errors.provider_closed").into_owned()
                }
                RuntimeError::PersistSessionIndex { error } => {
                    crate::tr!("errors.persist_session_index", error = error).into_owned()
                }
                _ => format!("Unknown runtime error: {error:?}"),
            };
            (RuntimeEventSeverity::Error, message)
        }
        RuntimeEvent::Notice(notice) => {
            let severity = match notice.severity() {
                NoticeSeverity::Success => RuntimeEventSeverity::Success,
                NoticeSeverity::Warning => RuntimeEventSeverity::Warning,
            };
            let message = match notice {
                RuntimeNotice::ProviderMessage(message) => message.clone(),
                RuntimeNotice::TcodeUpdateAvailable { version } => {
                    crate::tr!("notice.tcode_update_available", version = version).into_owned()
                }
                RuntimeNotice::NativeRewindCompleted { mode } => match mode {
                    RewindMode::Files => crate::tr!("chat.rewind_files_done").into_owned(),
                    RewindMode::Conversation => {
                        crate::tr!("chat.rewind_conversation_done").into_owned()
                    }
                    RewindMode::FilesAndConversation => {
                        crate::tr!("chat.rewind_all_done").into_owned()
                    }
                },
                RuntimeNotice::SwitchedBranch { branch } => {
                    crate::tr!("notice.switched_branch", branch = branch).into_owned()
                }
                RuntimeNotice::WorktreeSeeded {
                    copied_files,
                    skipped,
                    limit_reached,
                } => {
                    let skipped_count = skipped.len();
                    let skipped = skipped.join(", ");
                    if *limit_reached {
                        crate::tr!(
                            "notice.worktree_seeded_limit",
                            copied = copied_files,
                            skipped = skipped
                        )
                        .into_owned()
                    } else {
                        crate::tr!(
                            "notice.worktree_seeded",
                            copied = copied_files,
                            skipped = skipped_count
                        )
                        .into_owned()
                    }
                }
                RuntimeNotice::WorktreeMergedFastForward => {
                    crate::tr!("notice.worktree_merged_fast_forward").into_owned()
                }
                RuntimeNotice::WorktreeMergedCommit => {
                    crate::tr!("notice.worktree_merged_commit").into_owned()
                }
                RuntimeNotice::ReviewRecorded { items, coverage } => {
                    crate::tr!("notice.review_recorded", items = items, coverage = coverage)
                        .into_owned()
                }
                RuntimeNotice::ReferenceBoundaryCrossed {
                    thread_title,
                    title,
                    ..
                } => crate::tr!(
                    "notice.reference_boundary_crossed",
                    thread = thread_title,
                    title = title
                )
                .into_owned(),
                RuntimeNotice::WorktreeMergeFailed { reason, detail } => match reason {
                    MergeWorktreeFailure::Missing => {
                        crate::tr!("notice.worktree_merge_missing").into_owned()
                    }
                    MergeWorktreeFailure::DirtyWorktree => {
                        crate::tr!("notice.worktree_merge_dirty_worktree").into_owned()
                    }
                    MergeWorktreeFailure::DestinationDetached => {
                        crate::tr!("notice.worktree_merge_destination_detached").into_owned()
                    }
                    MergeWorktreeFailure::DirtyDestination => {
                        crate::tr!("notice.worktree_merge_dirty_destination").into_owned()
                    }
                    MergeWorktreeFailure::DivergedConflict => {
                        crate::tr!("notice.worktree_merge_conflict").into_owned()
                    }
                    MergeWorktreeFailure::Git => crate::tr!(
                        "notice.worktree_merge_git_error",
                        error = detail.as_deref().unwrap_or_default()
                    )
                    .into_owned(),
                },
                _ => format!("Unknown runtime notice: {notice:?}"),
            };
            (severity, message)
        }
        RuntimeEvent::Toast(_) => unreachable!("rich toasts use present_runtime_toast"),
        RuntimeEvent::Effect(_) => unreachable!("runtime effects are not presentable"),
    };

    PresentedRuntimeEvent { severity, message }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RuntimeToastDisposition {
    Push,
    Start(RuntimeOperationId),
    Update(RuntimeOperationId),
    Finish(RuntimeOperationId),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PresentedRuntimeToast {
    pub disposition: RuntimeToastDisposition,
    pub kind: ToastKind,
    pub title: String,
    pub detail: Option<String>,
    pub action: Option<RuntimeToastAction>,
    pub progress: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RuntimeToastAction {
    RetryGit(GitActionRequest),
    UpdateProviders(Vec<ProviderKind>),
}

fn git_action_toast_titles(action: GitAction) -> (String, String) {
    match action {
        GitAction::Commit => (
            crate::tr!("git.toast.committing").into_owned(),
            crate::tr!("git.toast.committed").into_owned(),
        ),
        GitAction::CommitPush => (
            crate::tr!("git.toast.committing_pushing").into_owned(),
            crate::tr!("git.toast.committed_pushed").into_owned(),
        ),
        GitAction::Push => (
            crate::tr!("git.toast.pushing").into_owned(),
            crate::tr!("git.toast.pushed").into_owned(),
        ),
        GitAction::Pull => (
            crate::tr!("git.toast.pulling").into_owned(),
            crate::tr!("git.toast.pulled").into_owned(),
        ),
        GitAction::PublishBranch => (
            crate::tr!("git.toast.publishing").into_owned(),
            crate::tr!("git.toast.published").into_owned(),
        ),
        GitAction::InitializeGit => (
            crate::tr!("git.toast.initializing").into_owned(),
            crate::tr!("git.toast.initialized").into_owned(),
        ),
    }
}

/// Started, succeeded and failed titles for a native plugin operation.
fn plugin_operation_toast_titles(target: &PluginOperationTarget) -> [String; 3] {
    use agent::PluginActionKind;
    let (name, keys) = match target {
        PluginOperationTarget::Plugin { action, plugin, .. } => (
            plugin.as_str(),
            match action {
                PluginActionKind::Install => [
                    "providers.plugins.toast.installing",
                    "providers.plugins.toast.installed",
                    "providers.plugins.toast.install_failed",
                ],
                PluginActionKind::Uninstall => [
                    "providers.plugins.toast.uninstalling",
                    "providers.plugins.toast.uninstalled",
                    "providers.plugins.toast.uninstall_failed",
                ],
                PluginActionKind::Enable => [
                    "providers.plugins.toast.enabling",
                    "providers.plugins.toast.enabled",
                    "providers.plugins.toast.enable_failed",
                ],
                PluginActionKind::Disable => [
                    "providers.plugins.toast.disabling",
                    "providers.plugins.toast.disabled",
                    "providers.plugins.toast.disable_failed",
                ],
                PluginActionKind::Update => [
                    "providers.plugins.toast.updating",
                    "providers.plugins.toast.updated",
                    "providers.plugins.toast.update_failed",
                ],
            },
        ),
        PluginOperationTarget::AddMarketplace { source } => (
            source.as_str(),
            [
                "providers.plugins.toast.adding_marketplace",
                "providers.plugins.toast.added_marketplace",
                "providers.plugins.toast.add_marketplace_failed",
            ],
        ),
        PluginOperationTarget::RemoveMarketplace { name } => (
            name.as_str(),
            [
                "providers.plugins.toast.removing_marketplace",
                "providers.plugins.toast.removed_marketplace",
                "providers.plugins.toast.remove_marketplace_failed",
            ],
        ),
    };
    keys.map(|key| crate::tr!(key, name = name).into_owned())
}

pub(super) fn present_runtime_toast(toast: &RuntimeToast) -> PresentedRuntimeToast {
    match toast {
        RuntimeToast::ProviderUpdatesAvailable { updates } => {
            let automatic: Vec<_> = updates
                .iter()
                .filter(|update| update.automatic)
                .map(|update| update.provider)
                .collect();
            let title = if let [update] = updates.as_slice() {
                crate::tr!(
                    "providers.updates_available_one",
                    provider = update.provider.display_name(),
                    version = &update.version
                )
                .into_owned()
            } else {
                crate::tr!("providers.updates_available_many", count = updates.len()).into_owned()
            };
            return PresentedRuntimeToast {
                disposition: RuntimeToastDisposition::Push,
                kind: ToastKind::Info,
                title,
                detail: (updates.len() > 1).then(|| {
                    updates
                        .iter()
                        .map(|update| {
                            format!("{} v{}", update.provider.display_name(), update.version)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
                action: (!automatic.is_empty())
                    .then_some(RuntimeToastAction::UpdateProviders(automatic)),
                progress: None,
            };
        }
        RuntimeToast::ProviderUpdateStarted { operation, run }
        | RuntimeToast::ProviderUpdateProgress { operation, run }
        | RuntimeToast::ProviderUpdateFinished { operation, run } => {
            let finished = matches!(toast, RuntimeToast::ProviderUpdateFinished { .. });
            let failed = run
                .failed
                .iter()
                .map(|provider| provider.display_name())
                .collect::<Vec<_>>()
                .join(", ");
            let title = if finished {
                if run.failed.is_empty() {
                    crate::tr!("providers.updated_count", count = run.completed).into_owned()
                } else {
                    crate::tr!(
                        "providers.update_result",
                        updated = run.completed - run.failed.len(),
                        failed = run.failed.len(),
                        providers = &failed
                    )
                    .into_owned()
                }
            } else {
                crate::tr!(
                    "providers.update_progress",
                    provider = run
                        .current
                        .map(|provider| provider.display_name())
                        .unwrap_or_default(),
                    completed = run.completed,
                    total = run.total
                )
                .into_owned()
            };
            return PresentedRuntimeToast {
                disposition: if finished {
                    RuntimeToastDisposition::Finish(*operation)
                } else if matches!(toast, RuntimeToast::ProviderUpdateStarted { .. }) {
                    RuntimeToastDisposition::Start(*operation)
                } else {
                    RuntimeToastDisposition::Update(*operation)
                },
                kind: if !finished {
                    ToastKind::Loading
                } else if run.failed.is_empty() {
                    ToastKind::Success
                } else if run.failed.len() == run.total {
                    ToastKind::Error
                } else {
                    ToastKind::Warning
                },
                title,
                detail: (!failed.is_empty()).then(|| {
                    crate::tr!("providers.update_failures", providers = failed).into_owned()
                }),
                action: None,
                progress: (!finished)
                    .then_some(100. * run.completed as f32 / run.total.max(1) as f32),
            };
        }
        _ => {}
    }
    let (disposition, kind, title, detail, action) = match toast {
        RuntimeToast::GitBusy => (
            RuntimeToastDisposition::Push,
            ToastKind::Warning,
            crate::tr!("git.toast.busy").into_owned(),
            None,
            None,
        ),
        RuntimeToast::GitStarted { operation, action } => (
            RuntimeToastDisposition::Start(*operation),
            ToastKind::Loading,
            git_action_toast_titles(*action).0,
            None,
            None,
        ),
        RuntimeToast::GitSucceeded { operation, action } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Success,
            git_action_toast_titles(*action).1,
            None,
            None,
        ),
        RuntimeToast::GitFailed {
            operation,
            detail,
            retry,
        } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Error,
            crate::tr!("git.toast.failed").into_owned(),
            Some(detail.clone()),
            Some(RuntimeToastAction::RetryGit(retry.clone())),
        ),
        RuntimeToast::CommitMessageGenerated { message } => (
            RuntimeToastDisposition::Push,
            ToastKind::Info,
            "Generated commit message".to_string(),
            Some(message.clone()),
            None,
        ),
        RuntimeToast::CommitMessageFailed { detail } => (
            RuntimeToastDisposition::Push,
            ToastKind::Error,
            crate::tr!("git.toast.failed").into_owned(),
            Some(detail.clone()),
            None,
        ),
        RuntimeToast::AcpInstallStarted { operation, name } => (
            RuntimeToastDisposition::Start(*operation),
            ToastKind::Loading,
            crate::tr!("providers.acp.installing", name = name).into_owned(),
            None,
            None,
        ),
        RuntimeToast::AcpInstallSucceeded { operation, name } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Success,
            crate::tr!("providers.acp.installed_toast", name = name).into_owned(),
            None,
            None,
        ),
        RuntimeToast::AcpInstallFailed {
            operation,
            name,
            detail,
        } => (
            RuntimeToastDisposition::Finish(*operation),
            ToastKind::Error,
            crate::tr!("providers.acp.install_failed", name = name).into_owned(),
            Some(detail.clone()),
            None,
        ),
        RuntimeToast::PluginOperationStarted {
            operation, target, ..
        } => {
            let [title, _, _] = plugin_operation_toast_titles(target);
            (
                RuntimeToastDisposition::Start(*operation),
                ToastKind::Loading,
                title,
                None,
                None,
            )
        }
        RuntimeToast::PluginOperationSucceeded {
            operation, target, ..
        } => {
            let [_, title, _] = plugin_operation_toast_titles(target);
            (
                RuntimeToastDisposition::Finish(*operation),
                ToastKind::Success,
                title,
                None,
                None,
            )
        }
        RuntimeToast::PluginOperationFailed {
            operation,
            target,
            detail,
            ..
        } => {
            let [_, _, title] = plugin_operation_toast_titles(target);
            (
                RuntimeToastDisposition::Finish(*operation),
                ToastKind::Error,
                title,
                Some(detail.clone()),
                None,
            )
        }
        _ => (
            RuntimeToastDisposition::Push,
            ToastKind::Warning,
            "Unknown runtime notification".to_string(),
            Some(format!("{toast:?}")),
            None,
        ),
    };

    PresentedRuntimeToast {
        disposition,
        kind,
        title,
        detail,
        action,
        progress: None,
    }
}

#[cfg(test)]
mod tests {
    use agent::ProviderKind;

    use super::*;

    #[test]
    fn merged_provider_toast_offers_only_automatic_updates_in_both_locales() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for language in [crate::LANGUAGE_ENGLISH, crate::LANGUAGE_SIMPLIFIED_CHINESE] {
            crate::set_locale(language);
            let mut updates = vec![
                tcode_protocol::ProviderUpdateAvailable {
                    provider: ProviderKind::ClaudeCode,
                    version: "2.1.0".into(),
                    automatic: false,
                },
                tcode_protocol::ProviderUpdateAvailable {
                    provider: ProviderKind::Codex,
                    version: "1.2.0".into(),
                    automatic: false,
                },
            ];
            let manual = present_runtime_toast(&RuntimeToast::ProviderUpdatesAvailable {
                updates: updates.clone(),
            });
            assert!(manual.action.is_none());
            assert_eq!(
                manual.detail.as_deref(),
                Some("Claude Code v2.1.0\nCodex v1.2.0")
            );
            assert_eq!(
                manual.title,
                if language == crate::LANGUAGE_ENGLISH {
                    "Updates available for 2 providers"
                } else {
                    "有 2 个 Provider 可更新"
                }
            );
            updates[1].automatic = true;
            let automatic =
                present_runtime_toast(&RuntimeToast::ProviderUpdatesAvailable { updates });
            assert_eq!(
                automatic.action,
                Some(RuntimeToastAction::UpdateProviders(vec![
                    ProviderKind::Codex
                ]))
            );
        }
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }

    #[test]
    fn locale_effect_is_applied_only_at_ui_boundary() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        apply_runtime_effect(&RuntimeEffect::ApplyLocale {
            language: Some(crate::LANGUAGE_SIMPLIFIED_CHINESE.to_string()),
        });
        let chinese = crate::tr!("chat.new_thread").into_owned();

        apply_runtime_effect(&RuntimeEffect::ApplyLocale {
            language: Some(crate::LANGUAGE_ENGLISH.to_string()),
        });
        let english = crate::tr!("chat.new_thread").into_owned();

        assert_eq!(chinese, "新建对话");
        assert_eq!(english, "New thread");
    }

    #[test]
    fn toast_lifecycle_and_raw_diagnostics_survive_localization() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        let plugin_target = PluginOperationTarget::Plugin {
            action: agent::PluginActionKind::Install,
            scope: agent::PluginScope::User,
            plugin: "gamma@tcode-probe".into(),
        };
        let retry = GitActionRequest {
            session_id: "session-1".into(),
            action: GitAction::CommitPush,
            message: Some("exact message".into()),
            included: Some(vec!["a.rs".into(), "b.rs".into()]),
            feature_branch: Some("feature/exact".into()),
        };
        let toasts = vec![
            RuntimeToast::GitBusy,
            RuntimeToast::GitStarted {
                operation: RuntimeOperationId(1),
                action: GitAction::Commit,
            },
            RuntimeToast::GitSucceeded {
                operation: RuntimeOperationId(1),
                action: GitAction::Commit,
            },
            RuntimeToast::GitFailed {
                operation: RuntimeOperationId(1),
                detail: "git raw\0detail".into(),
                retry: retry.clone(),
            },
            RuntimeToast::CommitMessageGenerated {
                message: "generated raw\0message".into(),
            },
            RuntimeToast::CommitMessageFailed {
                detail: "commit raw\0detail".into(),
            },
            RuntimeToast::AcpInstallStarted {
                operation: RuntimeOperationId(2),
                name: "Agent".into(),
            },
            RuntimeToast::AcpInstallSucceeded {
                operation: RuntimeOperationId(2),
                name: "Agent".into(),
            },
            RuntimeToast::AcpInstallFailed {
                operation: RuntimeOperationId(2),
                name: "Agent".into(),
                detail: "acp raw\0detail".into(),
            },
            RuntimeToast::PluginOperationStarted {
                operation: RuntimeOperationId(3),
                profile_id: "claude".into(),
                target: plugin_target.clone(),
            },
            RuntimeToast::PluginOperationSucceeded {
                operation: RuntimeOperationId(3),
                profile_id: "claude".into(),
                target: plugin_target.clone(),
            },
            RuntimeToast::PluginOperationFailed {
                operation: RuntimeOperationId(3),
                profile_id: "claude".into(),
                target: plugin_target.clone(),
                detail: "plugin raw\0detail".into(),
            },
        ];

        for locale in [crate::LANGUAGE_ENGLISH, crate::LANGUAGE_SIMPLIFIED_CHINESE] {
            crate::set_locale(locale);
            for toast in &toasts {
                let presented = present_runtime_toast(toast);
                match toast {
                    RuntimeToast::GitStarted { operation, .. }
                    | RuntimeToast::AcpInstallStarted { operation, .. }
                    | RuntimeToast::PluginOperationStarted { operation, .. } => {
                        assert_eq!(
                            presented.disposition,
                            RuntimeToastDisposition::Start(*operation)
                        );
                        assert_eq!(presented.kind, ToastKind::Loading);
                    }
                    RuntimeToast::GitSucceeded { operation, .. }
                    | RuntimeToast::AcpInstallSucceeded { operation, .. }
                    | RuntimeToast::PluginOperationSucceeded { operation, .. } => {
                        assert_eq!(
                            presented.disposition,
                            RuntimeToastDisposition::Finish(*operation)
                        );
                        assert_eq!(presented.kind, ToastKind::Success);
                    }
                    RuntimeToast::GitFailed { operation, .. }
                    | RuntimeToast::AcpInstallFailed { operation, .. }
                    | RuntimeToast::PluginOperationFailed { operation, .. } => {
                        assert_eq!(
                            presented.disposition,
                            RuntimeToastDisposition::Finish(*operation)
                        );
                        assert_eq!(presented.kind, ToastKind::Error);
                    }
                    RuntimeToast::GitBusy => {
                        assert_eq!(presented.disposition, RuntimeToastDisposition::Push);
                        assert_eq!(presented.kind, ToastKind::Warning);
                    }
                    RuntimeToast::CommitMessageGenerated { .. } => {
                        assert_eq!(presented.disposition, RuntimeToastDisposition::Push);
                        assert_eq!(presented.kind, ToastKind::Info);
                    }
                    RuntimeToast::CommitMessageFailed { .. } => {
                        assert_eq!(presented.disposition, RuntimeToastDisposition::Push);
                        assert_eq!(presented.kind, ToastKind::Error);
                    }
                    _ => {}
                }
            }

            let title_pairs = [
                (
                    GitAction::Commit,
                    crate::tr!("git.toast.committing").into_owned(),
                    crate::tr!("git.toast.committed").into_owned(),
                ),
                (
                    GitAction::CommitPush,
                    crate::tr!("git.toast.committing_pushing").into_owned(),
                    crate::tr!("git.toast.committed_pushed").into_owned(),
                ),
                (
                    GitAction::Push,
                    crate::tr!("git.toast.pushing").into_owned(),
                    crate::tr!("git.toast.pushed").into_owned(),
                ),
                (
                    GitAction::Pull,
                    crate::tr!("git.toast.pulling").into_owned(),
                    crate::tr!("git.toast.pulled").into_owned(),
                ),
                (
                    GitAction::PublishBranch,
                    crate::tr!("git.toast.publishing").into_owned(),
                    crate::tr!("git.toast.published").into_owned(),
                ),
                (
                    GitAction::InitializeGit,
                    crate::tr!("git.toast.initializing").into_owned(),
                    crate::tr!("git.toast.initialized").into_owned(),
                ),
            ];
            for (index, (action, started, succeeded)) in title_pairs.into_iter().enumerate() {
                let operation = RuntimeOperationId(index as u64 + 10);
                let start = present_runtime_toast(&RuntimeToast::GitStarted { operation, action });
                let success =
                    present_runtime_toast(&RuntimeToast::GitSucceeded { operation, action });
                assert_eq!(start.title, started);
                assert_eq!(success.title, succeeded);
                assert_eq!(start.disposition, RuntimeToastDisposition::Start(operation));
                assert_eq!(
                    success.disposition,
                    RuntimeToastDisposition::Finish(operation)
                );
            }

            let failed = present_runtime_toast(&RuntimeToast::GitFailed {
                operation: RuntimeOperationId(1),
                detail: "git raw\0detail".into(),
                retry: retry.clone(),
            });
            assert_eq!(failed.detail.as_deref(), Some("git raw\0detail"));
            assert_eq!(
                failed.action,
                Some(RuntimeToastAction::RetryGit(retry.clone()))
            );
            assert_eq!(
                present_runtime_toast(&RuntimeToast::CommitMessageGenerated {
                    message: "generated raw\0message".into(),
                })
                .detail
                .as_deref(),
                Some("generated raw\0message")
            );
            assert_eq!(
                present_runtime_toast(&RuntimeToast::CommitMessageFailed {
                    detail: "commit raw\0detail".into(),
                })
                .detail
                .as_deref(),
                Some("commit raw\0detail")
            );
            assert_eq!(
                present_runtime_toast(&RuntimeToast::AcpInstallFailed {
                    operation: RuntimeOperationId(2),
                    name: "Agent".into(),
                    detail: "acp raw\0detail".into(),
                })
                .detail
                .as_deref(),
                Some("acp raw\0detail")
            );
            let plugin_failed = present_runtime_toast(&RuntimeToast::PluginOperationFailed {
                operation: RuntimeOperationId(3),
                profile_id: "claude".into(),
                target: plugin_target.clone(),
                detail: "plugin raw\0detail".into(),
            });
            assert_eq!(plugin_failed.detail.as_deref(), Some("plugin raw\0detail"));
            assert!(plugin_failed.title.contains("gamma@tcode-probe"));

            for (event, severity, raw_message) in [
                (
                    RuntimeEvent::Error(RuntimeError::External("external\0diagnostic".into())),
                    RuntimeEventSeverity::Error,
                    Some("external\0diagnostic"),
                ),
                (
                    RuntimeEvent::Error(RuntimeError::ProviderMessage(
                        "provider-error\0diagnostic".into(),
                    )),
                    RuntimeEventSeverity::Error,
                    Some("provider-error\0diagnostic"),
                ),
                (
                    RuntimeEvent::Error(RuntimeError::ProcessGone),
                    RuntimeEventSeverity::Error,
                    None,
                ),
                (
                    RuntimeEvent::Notice(RuntimeNotice::ProviderMessage(
                        "provider-warning\0diagnostic".into(),
                    )),
                    RuntimeEventSeverity::Warning,
                    Some("provider-warning\0diagnostic"),
                ),
                (
                    RuntimeEvent::Notice(RuntimeNotice::TcodeUpdateAvailable {
                        version: "1.0.0".into(),
                    }),
                    RuntimeEventSeverity::Success,
                    None,
                ),
            ] {
                let presented = present_runtime_event(&event);
                assert_eq!(presented.severity, severity);
                if let Some(raw_message) = raw_message {
                    assert_eq!(presented.message, raw_message);
                }
            }
        }

        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }
}
