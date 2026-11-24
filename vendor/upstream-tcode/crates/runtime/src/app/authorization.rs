use std::path::Component;

use super::*;
use tcode_protocol::{ClientPayload, Command, Principal, Query};

fn refusal(kind: &str) -> ProtocolError {
    ProtocolError::out_of_scope(format!("{kind} is outside this space"))
}

impl AppState {
    pub(crate) fn observe_principal(&mut self, principal: &Principal) {
        if let Principal::Space {
            space_id,
            project_ids,
            policy_revision,
            ..
        } = principal
        {
            let revision = self
                .space_policy_revisions
                .entry(space_id.clone())
                .or_default();
            if *policy_revision < *revision {
                return;
            }
            *revision = *policy_revision;
            let projects = project_ids.iter().cloned().collect();
            if self.space_scopes.get(space_id) != Some(&projects) {
                self.space_scopes.insert(space_id.clone(), projects);
                self.space_archives_revision = None;
            }
            self.refresh_space_archives();
        }
    }

    pub(crate) fn session_in_scope(&self, id: &str, principal: &Principal) -> bool {
        match principal {
            Principal::Full => true,
            Principal::Space { project_ids, .. } => self.find_meta(id).is_some_and(|meta| {
                meta.project_id
                    .as_ref()
                    .is_some_and(|id| project_ids.contains(id))
            }),
        }
    }

    fn scoped_metas<'a>(
        &'a self,
        project_ids: &'a [String],
    ) -> impl Iterator<Item = &'a SessionMeta> {
        self.sessions
            .iter()
            .chain(self.residents.live.values().map(|session| &session.meta))
            .chain(self.residents.parked.values().map(|session| &session.meta))
            .filter(|meta| {
                meta.project_id
                    .as_ref()
                    .is_some_and(|id| project_ids.contains(id))
            })
    }

    pub(super) fn terminal_owner(&self, terminal_id: u64) -> Option<&str> {
        self.residents
            .ids()
            .find(|id| {
                self.resident(id).is_some_and(|session| {
                    session.terminal_workspace.terminal(terminal_id).is_some()
                })
            })
            .or_else(|| {
                self.terminal_workspaces
                    .iter()
                    .find_map(|(destination, workspace)| {
                        workspace.terminal(terminal_id)?;
                        match destination {
                            ConversationDestination::Thread(id) => Some(id.as_str()),
                            ConversationDestination::ProjectDraft(_) => None,
                        }
                    })
            })
    }

    pub(crate) fn authorize(
        &self,
        principal: &Principal,
        payload: &ClientPayload,
        cx: &HostCx,
    ) -> Result<(), ProtocolError> {
        let Principal::Space {
            project_ids,
            space_id,
            ..
        } = principal
        else {
            return Ok(());
        };
        let session = |id: &str| {
            self.session_in_scope(id, principal)
                .then_some(())
                .ok_or_else(|| refusal("session"))
        };
        let terminal = |id| {
            self.terminal_owner(id)
                .filter(|owner| self.session_in_scope(owner, principal))
                .ok_or_else(|| refusal("terminal"))
        };
        match payload {
            ClientPayload::Unsubscribe(_) => Ok(()),
            ClientPayload::Subscribe(subscription) => match &subscription.topic {
                Topic::Scope => Ok(()),
                Topic::SpaceIndex { space_id: id } if id == space_id => Ok(()),
                Topic::SessionEvents { session_id }
                | Topic::SessionStatus { session_id }
                | Topic::SessionPlan { session_id }
                | Topic::GitStatus { session_id }
                | Topic::CommandOutput { session_id, .. } => session(session_id),
                Topic::Terminal { terminal_id } => terminal(*terminal_id).map(|_| ()),
                Topic::Index
                | Topic::SpaceIndex { .. }
                | Topic::Settings
                | Topic::Providers
                | Topic::RuntimeEvents
                | Topic::Preview
                | Topic::ExternalImport { .. } => Err(refusal("subscription")),
            },
            ClientPayload::Command(command) => match command {
                Command::StartDraft { project_id, cwd } => {
                    let project = self
                        .projects
                        .iter()
                        .find(|p| &p.id == project_id && project_ids.contains(&p.id))
                        .ok_or_else(|| refusal("project"))?;
                    if cwd == &project.root
                        || self.scoped_metas(project_ids).any(|meta| {
                            meta.project_id.as_ref() == Some(project_id)
                                && meta.worktree.is_some()
                                && &meta.cwd == cwd
                        })
                    {
                        Ok(())
                    } else {
                        Err(refusal("workspace"))
                    }
                }
                Command::ClearRelaunchMarker => Ok(()),
                Command::TerminalInput { terminal_id, .. }
                | Command::ResizeTerminal { terminal_id, .. }
                | Command::ClearTerminal { terminal_id } => terminal(*terminal_id).map(|_| ()),
                Command::ActivateTerminal {
                    session_id,
                    terminal_id,
                }
                | Command::CloseTerminal {
                    session_id,
                    terminal_id,
                }
                | Command::CaptureTerminalSelection {
                    session_id,
                    terminal_id,
                    ..
                } => {
                    session(session_id)?;
                    if terminal(*terminal_id)? == session_id {
                        Ok(())
                    } else {
                        Err(refusal("terminal"))
                    }
                }
                Command::RespondApproval {
                    session_id,
                    request_id,
                    ..
                } => {
                    session(session_id)?;
                    self.approval_requests(session_id)
                        .iter()
                        .any(|request| &request.id == request_id)
                        .then_some(())
                        .ok_or_else(|| refusal("approval request"))
                }
                Command::RespondUserInput {
                    session_id,
                    request_id,
                    ..
                } => {
                    session(session_id)?;
                    self.resident(session_id)
                        .and_then(|s| s.timeline.pending_user_input.as_ref())
                        .is_some_and(|request| &request.request_id == request_id)
                        .then_some(())
                        .ok_or_else(|| refusal("input request"))
                }
                Command::SteerQueued { session_id, id }
                | Command::DropQueued { session_id, id } => {
                    session(session_id)?;
                    self.resident(session_id)
                        .is_some_and(|s| s.queue.iter().any(|message| message.id == *id))
                        .then_some(())
                        .ok_or_else(|| refusal("queued message"))
                }
                Command::MoveQueued {
                    session_id,
                    id,
                    before,
                } => {
                    session(session_id)?;
                    self.resident(session_id)
                        .is_some_and(|s| {
                            [Some(*id), *before]
                                .into_iter()
                                .flatten()
                                .all(|id| s.queue.iter().any(|message| message.id == id))
                        })
                        .then_some(())
                        .ok_or_else(|| refusal("queued message"))
                }
                Command::RemoveTerminalContext {
                    session_id,
                    context_id,
                } => {
                    session(session_id)?;
                    self.resident(session_id)
                        .is_some_and(|s| {
                            s.terminal_workspace
                                .contexts
                                .iter()
                                .any(|c| c.id == *context_id)
                        })
                        .then_some(())
                        .ok_or_else(|| refusal("terminal context"))
                }
                Command::SendTurn {
                    session_id,
                    attachment_paths,
                    ..
                }
                | Command::ScheduleTurn {
                    session_id,
                    attachment_paths,
                    ..
                }
                | Command::ConfirmRelayAndSend {
                    session_id,
                    attachment_paths,
                    ..
                }
                | Command::OrchestrateTurn {
                    session_id,
                    attachment_paths,
                    ..
                }
                | Command::Steer {
                    session_id,
                    attachment_paths,
                    ..
                } => {
                    session(session_id)?;
                    attachment_paths
                        .iter()
                        .try_for_each(|path| self.authorize_attachment(path, project_ids, cx))
                }
                Command::LinkPullRequest { session_id, .. }
                | Command::UnlinkPullRequest { session_id, .. }
                | Command::SetPullRequestFilesViewed { session_id, .. }
                | Command::RefreshPullRequest { session_id, .. }
                | Command::RunPullRequestAction { session_id, .. }
                | Command::EditPullRequestReviewDraft { session_id, .. }
                | Command::WatchPullRequest { session_id, .. }
                | Command::SetPullRequestWatchesPaused { session_id, .. }
                | Command::RetryTurn { session_id, .. }
                | Command::Interrupt { session_id }
                | Command::CompactContext { session_id }
                | Command::SetGoal { session_id, .. }
                | Command::ClearGoal { session_id }
                | Command::SetActiveModel { session_id, .. }
                | Command::SetActiveOption { session_id, .. }
                | Command::SetActiveAcpAgent { session_id, .. }
                | Command::RenameSession { session_id, .. }
                | Command::RegenerateSessionTitle { session_id }
                | Command::ArchiveSession { session_id }
                | Command::UnarchiveSession { session_id }
                | Command::SettleChildren { session_id }
                | Command::SettleSession { session_id }
                | Command::RemoveThreadReference { session_id, .. }
                | Command::SetFollowUp { session_id, .. }
                | Command::SetParked { session_id, .. }
                | Command::DismissSettleSuggestion { session_id }
                | Command::RevokeSelfSettle { session_id }
                | Command::CancelAgent { session_id }
                | Command::UnsettleSession { session_id }
                | Command::SetAutoSettle { session_id, .. }
                | Command::PinSession { session_id, .. }
                | Command::UnpinSession { session_id }
                | Command::ReorderPinned { session_id, .. }
                | Command::ReorderActive { session_id, .. }
                | Command::RewindTurn { session_id, .. }
                | Command::AddReviewComment { session_id, .. }
                | Command::RemoveReviewComment { session_id, .. }
                | Command::SetTerminalHeight { session_id, .. }
                | Command::ToggleTerminalPanel { session_id }
                | Command::CloseTerminalPanel { session_id }
                | Command::RestartTerminal { session_id }
                | Command::NewTerminal { session_id }
                | Command::SplitTerminal { session_id, .. }
                | Command::RunGitAction { session_id, .. }
                | Command::LoadBranches { session_id }
                | Command::CheckoutBranch { session_id, .. }
                | Command::MergeWorktree { session_id }
                | Command::SetDraftWorkspace { session_id, .. }
                | Command::MarkSessionRead { session_id, .. }
                | Command::MarkSessionUnread { session_id }
                | Command::WriteRelaunchMarker { session_id, .. } => session(session_id),
                Command::ForkThread { id } => session(id),
                // A space may move its threads only between its own projects.
                Command::MoveSession {
                    session_id,
                    project_id,
                    ..
                } => {
                    session(session_id)?;
                    project_ids
                        .contains(project_id)
                        .then_some(())
                        .ok_or_else(|| refusal("project"))
                }
                // A reference lets the thread's model read the referenced thread, so a space
                // may only point at threads it can read itself.
                Command::AddThreadReference {
                    session_id,
                    thread_id,
                } => {
                    session(session_id)?;
                    session(thread_id)
                }
                Command::DeleteSession { .. }
                | Command::DeleteProject { .. }
                | Command::CreateProject { .. }
                | Command::CreateNewProject { .. }
                | Command::StartScratchDraft
                | Command::SetProjectIcon { .. }
                | Command::SetProjectRoot { .. }
                | Command::StartExternalImport { .. }
                | Command::OpenLatestSession
                | Command::ShutdownAllAndFlush
                | Command::ApplyPendingRelaunch
                | Command::PatchSettings { .. }
                | Command::SetControlGrant { .. }
                | Command::RespondControlGrant { .. }
                | Command::ResetSettings
                | Command::SetSidebarCollapsed { .. }
                | Command::CycleProjectSort
                | Command::ToggleFavoriteModel { .. }
                | Command::ToggleProjectCollapsed { .. }
                | Command::PreviewReply { .. }
                | Command::ReloadProvider
                | Command::SetHostToken { .. }
                | Command::RefreshHostCredentials
                | Command::SetProfileSecret { .. }
                | Command::UpdateProfileSettings { .. }
                | Command::CreateThirdPartyProfile { .. }
                | Command::DeleteProfile { .. }
                | Command::RefreshProviderStatus
                | Command::RefreshProviderUsage
                | Command::CheckProviderVersions
                | Command::UpdateProviders { .. }
                | Command::RefreshAcpRegistry
                | Command::InstallAcpAgent { .. }
                | Command::RemoveAcpAgent { .. }
                | Command::AddCustomAcpAgent { .. }
                | Command::UpdateAcpAgent { .. }
                | Command::RefreshProviderPlugins { .. }
                | Command::InstallProviderPlugin { .. }
                | Command::UninstallProviderPlugin { .. }
                | Command::SetProviderPluginEnabled { .. }
                | Command::UpdateProviderPlugin { .. }
                | Command::AddProviderMarketplace { .. }
                | Command::RemoveProviderMarketplace { .. }
                | Command::ResolvePluginChallenge { .. } => Err(refusal("command")),
            },
            ClientPayload::Query(query) => match query {
                Query::Ping | Query::SearchSessionContent { .. } | Query::ArchivedSessions => {
                    Ok(())
                }
                Query::SessionHistoryPage { session_id, .. }
                | Query::ListActiveWorkspace { session_id }
                | Query::GenerateCommitMessage { session_id, .. }
                | Query::RenderThreadExport { session_id, .. }
                | Query::ReadItemOutput { session_id, .. }
                | Query::ThreadAnchorTurn { session_id, .. }
                | Query::ReadItemImage { session_id, .. }
                | Query::ReadItemImagePreview { session_id, .. }
                | Query::PullRequest { session_id, .. } => session(session_id),
                Query::ReadProjectIcon { project_id, .. } => self
                    .projects
                    .iter()
                    .any(|p| &p.id == project_id && project_ids.contains(&p.id))
                    .then_some(())
                    .ok_or_else(|| refusal("project")),
                Query::LoadGitDiff { cwd, .. } => {
                    self.authorize_path(cwd, project_ids, PathAccess::Workspace, cx)
                }
                Query::ReadFileBytes { path, .. }
                | Query::ReadImagePreview { path, .. }
                | Query::ReadFilePreview { path }
                | Query::ReadIconImage { path } => {
                    self.authorize_path(path, project_ids, PathAccess::Descendant, cx)
                }
                Query::RemoveUserFile { path } => {
                    self.authorize_path(path, project_ids, PathAccess::AttachmentDescendant, cx)
                }
                Query::BrowseIconImages { directory } => {
                    self.authorize_path(directory, project_ids, PathAccess::Descendant, cx)
                }
                Query::SaveAttachment { dir, .. } => {
                    self.authorize_path(dir, project_ids, PathAccess::Attachment, cx)
                }
                Query::Hosting { .. }
                | Query::ComputerUsePermissions
                | Query::ControlActivity { .. }
                | Query::ReviewRecords { .. }
                | Query::ScanExternalHistory => Err(refusal("query")),
            },
        }
    }

    /// Whether `path` may be sent as an attachment by someone confined to `projects`: a file
    /// inside the attachments folder of one of their threads, never an arbitrary file.
    pub(super) fn authorize_attachment(
        &self,
        path: &Path,
        projects: &[String],
        cx: &HostCx,
    ) -> Result<(), ProtocolError> {
        self.authorize_path(path, projects, PathAccess::AttachmentDescendant, cx)
    }

    fn authorize_path(
        &self,
        path: &Path,
        projects: &[String],
        access: PathAccess,
        cx: &HostCx,
    ) -> Result<(), ProtocolError> {
        let mut roots: Vec<PathBuf> =
            if matches!(access, PathAccess::Descendant | PathAccess::Workspace) {
                self.projects
                    .iter()
                    .filter(|p| projects.contains(&p.id))
                    .map(|p| p.root.clone())
                    .collect()
            } else {
                Vec::new()
            };
        for meta in self.scoped_metas(projects) {
            if matches!(access, PathAccess::Descendant | PathAccess::Workspace) {
                roots.push(meta.cwd.clone());
            }
            if !matches!(access, PathAccess::Workspace) {
                roots.push(self.attachments_dir_for(&meta.id));
            }
        }
        let path = path.to_path_buf();
        let allowed = smol::block_on(cx.unblock(move || {
            let canonical = if matches!(access, PathAccess::Attachment) {
                canonical_destination(&path)
            } else {
                fs::canonicalize(&path).ok()
            };
            canonical.is_some_and(|path| {
                roots.into_iter().any(|root| {
                    let root = if matches!(access, PathAccess::Attachment) {
                        canonical_destination(&root)
                    } else {
                        fs::canonicalize(root).ok()
                    };
                    root.is_some_and(|root| match access {
                        PathAccess::Descendant => path.starts_with(root),
                        PathAccess::AttachmentDescendant => path != root && path.starts_with(root),
                        PathAccess::Workspace | PathAccess::Attachment => path == root,
                    })
                })
            })
        }));
        allowed.then_some(()).ok_or_else(|| refusal("path"))
    }
}

#[derive(Clone, Copy)]
enum PathAccess {
    Descendant,
    Workspace,
    Attachment,
    AttachmentDescendant,
}

fn canonical_destination(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        if let Ok(mut canonical) = fs::canonicalize(ancestor) {
            for name in missing.into_iter().rev() {
                canonical.push(name);
            }
            return Some(canonical);
        }
        missing.push(ancestor.file_name()?.to_os_string());
        ancestor = ancestor.parent()?;
    }
}
