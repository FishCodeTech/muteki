use super::*;
use tcode_core::control::WatchEvent;
use tcode_core::thread_sort::ThreadSection;

/// What a settle did, or would do, beyond marking the thread settled. Unsettle restores none
/// of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettleEffects {
    pub provider_detached: bool,
    pub pull_request_watches_ended: usize,
    /// Agent and server messages dropped from the queue; human ones stay.
    pub queued_dropped: usize,
    /// Open terminals inspected; the ones at an idle shell are closed.
    pub terminals_checked: usize,
    pub unpinned: bool,
    /// Pending non-blocking input answered with empty answers.
    pub input_answered: bool,
}

/// Sessions viewed by clients and sessions retained for in-flight work or re-adoption.
#[derive(Default)]
pub struct ResidentSessions {
    pub live: Revisioned<HashMap<String, ActiveSession>>,
    pub(super) parked: Revisioned<HashMap<String, ActiveSession>>,
}
impl ResidentSessions {
    pub(crate) fn resident(&self, id: &str) -> Option<&ActiveSession> {
        self.live.get(id).or_else(|| self.parked.get(id))
    }
    pub(super) fn resident_mut(&mut self, id: &str) -> Option<&mut ActiveSession> {
        self.live.get_mut(id).or_else(|| self.parked.get_mut(id))
    }
    pub(super) fn park(&mut self, session: ActiveSession) {
        self.parked.insert(session.meta.id.clone(), session);
    }
    pub(super) fn adopt(&mut self, id: &str) -> Option<ActiveSession> {
        self.parked.remove(id)
    }
    pub(super) fn evict(&mut self, id: &str) -> Option<ActiveSession> {
        self.parked.remove(id)
    }
    pub(super) fn ids(&self) -> impl Iterator<Item = &str> {
        self.live
            .keys()
            .chain(self.parked.keys())
            .map(String::as_str)
    }
}

impl AppState {
    /// Advanced by every record and metadata write for the thread, so a
    /// decision read before an await can tell it is still current.
    pub(super) fn advance_decision_revision(&mut self, id: &str) {
        let revision = self.decision_revisions.entry(id.to_owned()).or_default();
        *revision = revision
            .checked_add(1)
            .expect("thread decision revision overflow");
    }

    pub(super) fn decision_revision(&self, id: &str) -> u64 {
        self.decision_revisions.get(id).copied().unwrap_or(0)
    }

    /// Advanced when a child's completion in flight stops being the one to
    /// deliver: new work was admitted for it, or it was cancelled or archived.
    pub(crate) fn subscribe(
        &mut self,
        subscription: &tcode_protocol::Subscription,
        cx: &mut HostCx,
    ) {
        if self.subscriptions.insert(subscription.topic.clone()) {
            match &subscription.topic {
                Topic::SessionStatus { session_id }
                | Topic::SessionPlan { session_id }
                | Topic::SessionEvents { session_id }
                | Topic::GitStatus { session_id } => self.select_session(session_id, cx),
                // No projection ran while nobody was attached, so the first
                // subscriber rebuilds the frame. A later one reads that same
                // retained frame and continues from the shared delta sequence.
                Topic::Terminal { terminal_id } => {
                    self.refresh_terminal_projection(*terminal_id);
                }
                Topic::CommandOutput { .. } => self.open_command_output(&subscription.topic),
                _ => {}
            }
        }
    }

    pub(crate) fn unsubscribe(
        &mut self,
        subscription: &tcode_protocol::Subscription,
        cx: &mut HostCx,
    ) {
        self.subscriptions.remove(&subscription.topic);
        if let Topic::CommandOutput { .. } = &subscription.topic {
            self.close_command_output(&subscription.topic);
        }
        if let Topic::SpaceIndex { space_id } = &subscription.topic {
            self.space_scopes.remove(space_id);
            if let Some(archive) = self.space_archives.remove(space_id) {
                self.space_archive_revisions
                    .insert(space_id.clone(), archive.revision);
            }
        }
        let session_id = match &subscription.topic {
            Topic::SessionStatus { session_id }
            | Topic::SessionPlan { session_id }
            | Topic::SessionEvents { session_id }
            | Topic::GitStatus { session_id } => session_id,
            _ => return,
        };
        if !self.subscriptions.iter().any(|topic| match topic {
            Topic::SessionStatus { session_id: id }
            | Topic::SessionPlan { session_id: id }
            | Topic::SessionEvents { session_id: id }
            | Topic::GitStatus { session_id: id } => id == session_id,
            _ => false,
        }) {
            self.park_active(session_id, cx);
        }
    }

    /// Assemble the provider-bound message at the runtime boundary. Unreadable
    /// attachment files are skipped, matching the composer's previous behavior.
    pub(super) fn assemble_user_message(
        &self,
        target_id: &str,
        text: String,
        attachment_paths: Vec<PathBuf>,
    ) -> (String, Vec<Attachment>) {
        let terminal_contexts = self
            .resident(target_id)
            .map(|active| active.terminal_workspace.contexts.as_slice())
            .unwrap_or_default();
        let text = append_terminal_contexts_to_prompt(&text, terminal_contexts);
        let text = append_review_comments_to_prompt(&text, self.review_comments(target_id));
        (text, read_attachments(attachment_paths))
    }

    pub(super) fn clear_consumed_draft_context(&mut self, target_id: &str, cx: &mut HostCx) {
        self.clear_terminal_contexts(target_id);
        self.clear_review_comments(target_id, cx);
    }

    /// Cycle the sidebar PROJECTS ordering and persist it.
    pub fn cycle_project_sort(&mut self, cx: &mut HostCx) {
        let mut settings = self.settings.clone();
        settings.project_sort = settings.project_sort.next();
        self.update_settings(settings, cx);
    }

    /// Save a normalized user override, or return to the project config.
    pub fn set_project_icon(
        &mut self,
        project_id: &str,
        png: Option<Vec<u8>>,
        cx: &mut HostCx,
    ) -> std::io::Result<()> {
        let project = self
            .projects
            .iter_mut()
            .find(|p| p.id == project_id)
            .ok_or_else(|| std::io::Error::other("unknown project"))?;
        let path = if let Some(png) = png {
            let path = self
                .store
                .root()
                .join("project-icons")
                .join(format!("{}.png", uuid::Uuid::new_v4()));
            tcode_services::project_icons::save_override(&path, &png)?;
            Some(path)
        } else {
            None
        };
        let refresh_default = path.is_none() && project.icon_path.is_none();
        project.icon_path = path;
        let project = project.clone();
        if refresh_default {
            // Reset also refreshes edited defaults when the persisted selection is unchanged.
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id: None,
                topic: Topic::Index,
                event: ServerEvent::IndexUpsertProject(project.clone()),
            }));
        }
        self.enqueue_store_write(StoreWrite::UpsertProject(project), cx);
        Ok(())
    }

    /// Create a project rooted at `root`, or return the existing id when one
    /// already covers it.
    ///
    /// The root is validated here, against this host's filesystem and path
    /// rules: a client cannot decide whether `C:\src` or `/srv/src` is absolute,
    /// and only the host can see whether the directory exists.
    pub fn create_project(
        &mut self,
        root: PathBuf,
        cx: &mut HostCx,
    ) -> Result<String, ProtocolError> {
        if let Some(existing) = self.projects.iter().find(|p| p.root == root) {
            return Ok(existing.id.clone());
        }
        if !root.is_absolute() {
            return Err(ProtocolError {
                code: "invalid_project_root".into(),
                message: format!("{} is not an absolute path on this host", root.display()),
            });
        }
        if !root.is_dir() {
            return Err(ProtocolError {
                code: "invalid_project_root".into(),
                message: format!("{} is not a directory on this host", root.display()),
            });
        }
        let quick_chat = self.user_directories.scratch_root().ok().as_ref() == Some(&root);
        let mut project = Project::from_root(root);
        project.quick_chat = quick_chat;
        let id = project.id.clone();
        self.enqueue_store_write(StoreWrite::UpsertProject(project.clone()), cx);
        self.projects.push(project);
        Ok(id)
    }

    /// Point `project_id` at `root`; see [`Command::SetProjectRoot`]. Every
    /// resident thread of the project is stopped first so no process keeps
    /// working in the old directory; each resumes from its cursor when next
    /// opened.
    pub fn set_project_root(
        &mut self,
        project_id: &str,
        root: PathBuf,
        move_files: bool,
        cx: &mut HostCx,
    ) -> HostTask<Result<(), ProtocolError>> {
        let outcome = self.prepare_project_root_change(project_id, &root, move_files, cx);
        let host = cx.clone();
        let project_id = project_id.to_string();
        cx.spawn_background(async move {
            let (old_root, worktrees) = outcome?;
            let destination = root.clone();
            let changed = host
                .unblock(move || {
                    if move_files {
                        tcode_services::fs_tree::move_tree(&old_root, &destination)?;
                    } else if !destination.is_dir() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("{} is not a directory on this host", destination.display()),
                        ));
                    }
                    if let Err(error) = tcode_services::worktree::repair(&destination, &worktrees) {
                        log::warn!(
                            "could not repair the worktrees of {}: {error}",
                            destination.display()
                        );
                    }
                    Ok(())
                })
                .await;
            changed.map_err(project_root_error)?;
            host.enqueue_and_wait(move |app, cx| app.apply_project_root(&project_id, root, cx))
                .await
                .map_err(|_| ProtocolError {
                    code: "transport_closed".into(),
                    message: "Host stopped before the project root changed.".into(),
                })?
        })
    }

    /// Validate a root change and stop the project's resident threads.
    /// Returns the old root and the worktree paths to repair.
    fn prepare_project_root_change(
        &mut self,
        project_id: &str,
        root: &Path,
        move_files: bool,
        cx: &mut HostCx,
    ) -> Result<(PathBuf, Vec<PathBuf>), ProtocolError> {
        let project = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .ok_or_else(|| ProtocolError {
                code: "unknown_project".into(),
                message: "This project is no longer available.".into(),
            })?;
        if !root.is_absolute() {
            return Err(ProtocolError {
                code: "invalid_project_root".into(),
                message: format!("{} is not an absolute path on this host", root.display()),
            });
        }
        let old_root = project.root.clone();
        if move_files && root.starts_with(&old_root) {
            return Err(ProtocolError {
                code: "invalid_project_root".into(),
                message: format!(
                    "cannot move {} into itself at {}",
                    old_root.display(),
                    root.display()
                ),
            });
        }
        let in_project = |meta: &SessionMeta| {
            meta.project_id.as_deref() == Some(project_id) || meta.cwd.starts_with(&old_root)
        };
        let resident_ids: Vec<String> = self
            .residents
            .live
            .values()
            .chain(self.residents.parked.values())
            .filter(|active| in_project(&active.meta))
            .map(|active| active.meta.id.clone())
            .collect();
        if resident_ids
            .iter()
            .any(|id| self.resident(id).is_some_and(ActiveSession::has_work))
        {
            return Err(ProtocolError {
                code: "project_busy".into(),
                message: "A thread of this project is still running; wait for it to finish.".into(),
            });
        }
        for id in &resident_ids {
            if self.residents.live.contains_key(id) {
                self.shutdown_active(id, cx);
            } else if let Some(parked) = self.residents.evict(id)
                && let Runtime::Live(commands) = parked.runtime
            {
                let _ = commands.try_send(SessionCommand::Shutdown);
            }
        }
        let worktrees = self
            .sessions
            .iter()
            .filter(|meta| meta.worktree.is_some() && in_project(meta))
            .map(|meta| meta.cwd.clone())
            .collect();
        Ok((old_root, worktrees))
    }

    /// Record `root` as the project's root once the directory is in place,
    /// carrying its threads along or into the project already there.
    fn apply_project_root(
        &mut self,
        project_id: &str,
        root: PathBuf,
        cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .cloned()
        else {
            return Err(ProtocolError {
                code: "unknown_project".into(),
                message: "This project is no longer available.".into(),
            });
        };
        let old_root = project.root.clone();
        let absorbing = self
            .projects
            .iter()
            .find(|other| other.root == root && other.id != project_id)
            .map(|other| other.id.clone());
        let metas: Vec<SessionMeta> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.project_id.as_deref() == Some(project_id) || meta.cwd.starts_with(&old_root)
            })
            .cloned()
            .collect();
        for mut meta in metas {
            if let Some(cwd) = rebase_path(&meta.cwd, &old_root, &root) {
                meta.cwd = cwd;
            }
            if let Some(worktree) = &mut meta.worktree
                && let Some(path) = rebase_path(&worktree.root_project_path, &old_root, &root)
            {
                worktree.root_project_path = path;
            }
            if let Some(target) = &absorbing
                && meta.project_id.as_deref() == Some(project_id)
            {
                meta.project_id = Some(target.clone());
            }
            if let Some(resident) = self.resident_mut(&meta.id) {
                resident.meta.cwd = meta.cwd.clone();
                resident.meta.worktree = meta.worktree.clone();
                resident.meta.project_id = meta.project_id.clone();
            }
            self.persist_meta(&meta, cx);
        }
        match absorbing {
            Some(target_id) => {
                let target = self
                    .projects
                    .iter_mut()
                    .find(|other| other.id == target_id)
                    .expect("absorbing project was just found");
                for (key, value) in &project.permission_defaults {
                    target
                        .permission_defaults
                        .entry(key.clone())
                        .or_insert_with(|| value.clone());
                }
                if target.icon_path.is_none() {
                    target.icon_path = project.icon_path.clone();
                }
                let target = target.clone();
                self.enqueue_store_write(StoreWrite::UpsertProject(target), cx);
                let draft_destination =
                    ConversationDestination::ProjectDraft(project_id.to_string());
                self.terminal_workspaces.remove(&draft_destination);
                if self
                    .terminal_preferences
                    .remove(&draft_destination.preference_key())
                    .is_some()
                {
                    self.write_terminal_preferences(cx);
                }
                self.enqueue_store_write(StoreWrite::RemoveProject(project_id.to_string()), cx);
                self.settings
                    .collapsed_projects
                    .retain(|id| id != project_id);
                self.persist_settings(cx);
                self.projects.retain(|other| other.id != project_id);
                self.replace_external_import_status(project_id, None, cx);
            }
            None => {
                let project = self
                    .projects
                    .iter_mut()
                    .find(|other| other.id == project_id)
                    .expect("project was just found");
                if project.name == project_name_from_root(&old_root) {
                    project.name = project_name_from_root(&root);
                }
                project.quick_chat =
                    self.user_directories.scratch_root().ok().as_ref() == Some(&root);
                project.root = root;
                let project = project.clone();
                self.enqueue_store_write(StoreWrite::UpsertProject(project), cx);
            }
        }
        Ok(())
    }

    /// Move a thread into `project_id`, its conversation continuing in `cwd` (the project
    /// root when absent). A provider's native session belongs to the directory it ran in —
    /// Claude Code keeps its sessions per directory and cannot resume one from another — so
    /// the move clears the resume cursor and the next turn starts a fresh provider session
    /// there, handed the conversation so far by the relay owner
    /// ([`Self::hand_off_moved_conversation`]). The Tcode transcript stays as it is.
    pub fn move_session(
        &mut self,
        session_id: &str,
        project_id: &str,
        cwd: Option<PathBuf>,
        cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        self.move_session_by(session_id, project_id, cwd, false, cx)
    }

    /// [`Self::move_session`]; `by_own_turn` when the thread's own running turn asked for the
    /// move, which keeps that turn: its provider leaves the old directory when it ends.
    pub(super) fn move_session_by(
        &mut self,
        session_id: &str,
        project_id: &str,
        cwd: Option<PathBuf>,
        by_own_turn: bool,
        cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        let error = |code: &str, message: String| ProtocolError {
            code: code.into(),
            message,
        };
        let resident_meta = self
            .resident(session_id)
            .map(|session| session.meta.clone());
        let Some(mut meta) = resident_meta.or_else(|| self.find_meta(session_id)) else {
            return Err(error(
                "unknown_session",
                "This thread is no longer available on the host.".into(),
            ));
        };
        let Some(project) = self.projects.iter().find(|p| p.id == project_id).cloned() else {
            return Err(error(
                "unknown_project",
                "This project is no longer available.".into(),
            ));
        };
        if meta.archived_at.is_some() {
            return Err(error(
                "archived_session",
                "Unarchive this thread before moving it.".into(),
            ));
        }
        if self
            .resident(session_id)
            .is_some_and(|session| session.draft)
        {
            return Err(error(
                "draft_session",
                "An unsent draft is not a thread yet; start it in the other project instead."
                    .into(),
            ));
        }
        if Self::conversation_read_only(&meta) {
            return Err(error(
                "read_only_session",
                "A native subagent's mirror cannot be moved.".into(),
            ));
        }
        if meta.worktree.is_some() {
            return Err(error(
                "worktree_session",
                "This thread runs in its own worktree; move the worktree's branch instead.".into(),
            ));
        }
        let cwd = match cwd {
            None => project.root.clone(),
            Some(cwd) if cwd.is_relative() => project.root.join(cwd),
            Some(cwd) => cwd,
        };
        match directory_within(&project.root, &cwd) {
            None => {
                return Err(error(
                    "invalid_cwd",
                    format!("{} is not a directory on this host.", cwd.display()),
                ));
            }
            Some(false) => {
                return Err(error(
                    "cwd_outside_project",
                    format!(
                        "{} is not inside {}.",
                        cwd.display(),
                        project.root.display()
                    ),
                ));
            }
            Some(true) => {}
        }
        if meta.project_id.as_deref() == Some(project_id) && meta.cwd == cwd {
            return Ok(());
        }
        if !by_own_turn && self.settle_thread_busy(session_id) {
            return Err(error(
                "thread_busy",
                "This thread is busy; wait for it to finish before moving it.".into(),
            ));
        }

        let old_cwd = std::mem::replace(&mut meta.cwd, cwd);
        meta.moved_from.get_or_insert(old_cwd);
        meta.project_id = Some(project.id.clone());
        meta.resume_cursor = None;
        meta.pending_fork = false;
        meta.updated_at = now_secs();
        if !by_own_turn {
            self.detach_provider_to_idle(session_id, cx);
        }
        if let Some(resident) = self.resident_mut(session_id) {
            resident.meta = meta.clone();
            resident.git_branch = None;
            resident.branches.clear();
        }
        self.persist_meta(&meta, cx);
        self.refresh_control_grants();
        if self.resident(session_id).is_some() {
            self.refresh_session_git_branch(meta.id.clone(), meta.cwd.clone(), cx);
            self.refresh_git_status(session_id, cx);
        }
        Ok(())
    }

    pub fn create_new_project(
        &self,
        name: String,
        cx: &HostCx,
    ) -> HostTask<Result<String, ProtocolError>> {
        let directories = self.user_directories.clone();
        let task = cx.unblock(move || directories.new_project_root(&name));
        let host = cx.clone();
        cx.spawn_background(async move {
            let root = task.await.map_err(project_directory_error)?;
            host.enqueue_and_wait(move |app, cx| app.create_project(root, cx))
                .await
                .map_err(|_| ProtocolError {
                    code: "transport_closed".into(),
                    message: "Host stopped before creating the project.".into(),
                })?
        })
    }

    pub fn start_scratch_draft(&self, cx: &HostCx) -> HostTask<Result<String, ProtocolError>> {
        let directories = self.user_directories.clone();
        let task = cx.unblock(move || directories.scratch_directory());
        let host = cx.clone();
        cx.spawn_background(async move {
            let (root, cwd) = task.await.map_err(project_directory_error)?;
            host.enqueue_and_wait(move |app, cx| {
                let project_id = app.create_project(root, cx)?;
                Ok(app.start_draft(project_id, cwd, cx))
            })
            .await
            .map_err(|_| ProtocolError {
                code: "transport_closed".into(),
                message: "Host stopped before creating the project.".into(),
            })?
        })
    }

    /// Scan supported external-agent histories without exposing the import
    /// service or application stores to callers.
    pub fn scan_external_history(&self, executor: &HostCx) -> HostTask<Vec<RecentDir>> {
        let exclude: Vec<_> = self
            .projects
            .iter()
            .map(|project| project.root.clone())
            .collect();
        let sessions = self.sessions.clone();
        executor.unblock(move || {
            let known = existing_external_ids(&sessions);
            let mut recent = scan_recent_dirs(&ExternalRoots::detect(), &exclude);
            for dir in &mut recent {
                dir.threads
                    .retain(|thread| !known.contains(&thread.external_id));
            }
            recent.retain(|dir| !dir.threads.is_empty());
            recent
        })
    }

    /// Import selected external threads in the background, publishing progress
    /// as replicated host state on [`Topic::ExternalImport`]. Returns `false`
    /// for an unknown project; a second concurrent run is rejected outright.
    ///
    /// Completion is runtime-owned: the importer's last update finalizes the
    /// index here regardless of whether any client is still subscribed.
    pub fn start_external_import(
        &mut self,
        project_id: &str,
        threads: Vec<ExternalThread>,
        cx: &mut HostCx,
    ) -> Result<bool, ProtocolError> {
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .cloned()
        else {
            return Ok(false);
        };
        if let Some(status) = self.external_imports.get(project_id)
            && matches!(status.state, ExternalImportState::Progress { .. })
        {
            return Err(ProtocolError {
                code: "import_in_progress".into(),
                message: format!("an import is already running for project {project_id}"),
            });
        }
        let run_id = self.next_import_run_id;
        self.next_import_run_id += 1;
        let total = threads.len();
        let tool = threads
            .first()
            .map(|thread| thread.source.display_name().to_string())
            .unwrap_or_default();
        self.replace_external_import_status(
            project_id,
            Some(ExternalImportStatus {
                run_id,
                state: ExternalImportState::Progress {
                    done: 0,
                    total,
                    tool,
                },
            }),
            cx,
        );

        let store = self.store.clone();
        let metas = self.sessions.clone();
        let id = project_id.to_string();
        let updates = cx.clone();
        cx.unblock(move || {
            let mut imported = 0;
            let mut skipped = 0;
            let mut existing = existing_external_ids(&metas);
            for (index, thread) in threads.into_iter().enumerate() {
                let tool = thread.source.display_name().to_string();
                match import_thread(&store, &project, &thread, &mut existing) {
                    ImportOutcome::Imported => imported += 1,
                    ImportOutcome::SkippedDuplicate
                    | ImportOutcome::SkippedEmpty
                    | ImportOutcome::Failed(_) => skipped += 1,
                }
                let (id, done) = (id.clone(), index + 1);
                updates.enqueue(move |state, cx| {
                    state.advance_external_import(
                        &id,
                        run_id,
                        ExternalImportState::Progress { done, total, tool },
                        cx,
                    );
                });
            }
            updates.enqueue(move |state, cx| {
                state.complete_external_import(&id, run_id, imported, skipped, cx);
            });
        })
        .detach();
        Ok(true)
    }

    /// Publish a status the importer produced, ignoring updates from a run that
    /// a newer one has already superseded.
    fn advance_external_import(
        &mut self,
        project_id: &str,
        run_id: u64,
        state: ExternalImportState,
        cx: &mut HostCx,
    ) {
        if self
            .external_imports
            .get(project_id)
            .map(|status| status.run_id)
            != Some(run_id)
        {
            return;
        }
        self.replace_external_import_status(
            project_id,
            Some(ExternalImportStatus { run_id, state }),
            cx,
        );
    }

    /// Finalize a finished run. The index is reloaded in this mailbox turn, so
    /// its replacement reaches clients before the `Finished` status published
    /// by the follow-up turn — a subscriber never sees `Finished` with a stale
    /// session list.
    fn complete_external_import(
        &mut self,
        project_id: &str,
        run_id: u64,
        imported: usize,
        skipped: usize,
        cx: &mut HostCx,
    ) {
        if self
            .external_imports
            .get(project_id)
            .map(|status| status.run_id)
            != Some(run_id)
        {
            return;
        }
        self.finish_external_import(project_id, cx);
        let project_id = project_id.to_string();
        cx.enqueue(move |state, cx| {
            state.advance_external_import(
                &project_id,
                run_id,
                ExternalImportState::Finished { imported, skipped },
                cx,
            );
        });
    }

    pub(crate) fn replace_external_import_status(
        &mut self,
        project_id: &str,
        status: Option<ExternalImportStatus>,
        cx: &mut HostCx,
    ) {
        match &status {
            Some(status) => {
                self.external_imports
                    .insert(project_id.to_string(), status.clone());
            }
            None => {
                self.external_imports.remove(project_id);
            }
        }
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic: Topic::ExternalImport {
                project_id: project_id.to_string(),
            },
            event: ServerEvent::ExternalImportStatusReplaced {
                project_id: project_id.to_string(),
                status,
            },
        }));
    }

    /// Search this host's own stored sessions in index order, on the blocking
    /// executor.
    pub fn search_session_content(
        &self,
        query: String,
        limit: u32,
        executor: &HostCx,
    ) -> HostTask<Vec<SessionSearchHit>> {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX).min(50);
        let sessions = self
            .sessions
            .iter()
            .filter(|meta| match &executor.principal {
                tcode_protocol::Principal::Full => true,
                tcode_protocol::Principal::Space { project_ids, .. } => meta
                    .project_id
                    .as_ref()
                    .is_some_and(|id| project_ids.contains(id)),
            })
            .cloned()
            .collect::<Vec<_>>();
        let search = self.session_search.clone();
        executor.unblock(move || search.search(&sessions, &query, limit))
    }

    /// Bring every thread's search entries up to date in the background, so
    /// the first search after an upgrade reads text instead of folding every
    /// log.
    pub(crate) fn index_session_search(&self, cx: &mut HostCx) {
        let search = self.session_search.clone();
        let ids: Vec<_> = self.sessions.iter().map(|meta| meta.id.clone()).collect();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            host_cx.unblock(move || search.index(&ids)).await;
        });
    }

    /// List one replicated session cwd on the background executor.
    pub fn list_workspace_at(
        &self,
        cwd: Option<PathBuf>,
        executor: &HostCx,
    ) -> HostTask<Vec<PathEntry>> {
        executor.unblock(move || cwd.map(|cwd| list_workspace(&cwd)).unwrap_or_default())
    }

    /// Reload sessions written by the external-history importer and expand its
    /// project group.
    fn finish_external_import(&mut self, project_id: &str, cx: &mut HostCx) {
        match self.store.load_index() {
            Ok(sessions) => {
                let archived = self.archived_sessions();
                self.sessions = sessions.into();
                let missing: Vec<_> = self
                    .sessions
                    .iter()
                    .filter(|meta| !self.thread_activity.contains_key(&meta.id))
                    .map(|meta| meta.id.clone())
                    .collect();
                for id in missing {
                    self.queue_activity_load(&id, cx);
                }
                if self.archived_sessions() != archived {
                    self.archived_revision += 1;
                }
            }
            Err(error) => self.report_error(
                RuntimeError::External(format!("could not reload the imported threads: {error}")),
                cx,
            ),
        }
        if self
            .settings
            .collapsed_projects
            .iter()
            .any(|id| id == project_id)
        {
            let mut settings = self.settings.clone();
            settings.collapsed_projects.retain(|id| id != project_id);
            self.update_settings(settings, cx);
        }
    }

    /// Flush pending appends, then render a thread into transferable bytes on
    /// the host's blocking-I/O executor. Nothing is written: the requesting
    /// client owns the destination, which may not be on this machine at all.
    pub fn render_thread_export(
        &mut self,
        session_id: &str,
        format: ThreadExportFormat,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, ProtocolError>> {
        let Some(meta) = self.find_meta(session_id) else {
            let session_id = session_id.to_owned();
            return cx.spawn_background(async move {
                Err(ProtocolError {
                    code: "unknown_session".into(),
                    message: format!("unknown session {session_id}"),
                })
            });
        };
        let barrier = self.store_write_barrier(cx);
        let store = self.store.clone();
        let host_cx = cx.clone();
        cx.spawn_background(async move {
            barrier
                .recv()
                .await
                .map_err(|error| ProtocolError {
                    code: "store_barrier_closed".into(),
                    message: format!("session-store flush failed: {error}"),
                })?
                .map_err(|message| ProtocolError {
                    code: "store_flush_failed".into(),
                    message,
                })?;
            let suggested_name = export::export_file_name(&meta.title, format);
            let bytes = host_cx
                .unblock(move || export::render_thread(&store, &meta, format))
                .await
                .map_err(|error| ProtocolError {
                    code: "export_failed".into(),
                    message: error.to_string(),
                })?;
            if bytes.len() > tcode_protocol::MAX_THREAD_EXPORT_BYTES {
                return Err(ProtocolError {
                    code: "export_too_large".into(),
                    message: format!(
                        "the rendered export is {} bytes, over the {} byte transfer limit",
                        bytes.len(),
                        tcode_protocol::MAX_THREAD_EXPORT_BYTES
                    ),
                });
            }
            Ok(QueryResponse::ThreadExport {
                bytes,
                suggested_name,
                mime: export::export_mime(format).to_owned(),
            })
        })
    }

    /// Merge a clean dedicated-worktree branch into its clean original checkout
    /// without blocking the host owner thread.
    /// The returned channel carries the notice the clients are shown.
    pub fn merge_worktree(
        &mut self,
        session_id: &str,
        cx: &mut HostCx,
    ) -> smol::channel::Receiver<RuntimeNotice> {
        let (done, outcome) = smol::channel::bounded(1);
        let Some(meta) = self.find_meta(session_id) else {
            return outcome;
        };
        let Some(worktree) = meta.worktree else {
            return outcome;
        };
        let destination = worktree.root_project_path;
        let worktree_path = meta.cwd;
        let branch = worktree.branch;
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || merge_back(&destination, &worktree_path, &branch))
                .await;
            host_cx.enqueue(move |_state, cx| {
                let notice = match result {
                    Ok(MergeBackOutcome::FastForward) => RuntimeNotice::WorktreeMergedFastForward,
                    Ok(MergeBackOutcome::MergeCommit) => RuntimeNotice::WorktreeMergedCommit,
                    Err(error) => {
                        let (reason, detail) = match error {
                            MergeBackError::WorktreeMissing => {
                                (MergeWorktreeFailure::Missing, None)
                            }
                            MergeBackError::DirtyWorktree => {
                                (MergeWorktreeFailure::DirtyWorktree, None)
                            }
                            MergeBackError::DestinationDetached => {
                                (MergeWorktreeFailure::DestinationDetached, None)
                            }
                            MergeBackError::DirtyDestination => {
                                (MergeWorktreeFailure::DirtyDestination, None)
                            }
                            MergeBackError::DivergedConflict => {
                                (MergeWorktreeFailure::DivergedConflict, None)
                            }
                            MergeBackError::Git(detail) => {
                                (MergeWorktreeFailure::Git, Some(detail))
                            }
                        };
                        RuntimeNotice::WorktreeMergeFailed { reason, detail }
                    }
                };
                emit_runtime(cx, RuntimeEvent::Notice(notice.clone()));
                let _ = done.try_send(notice);
            });
        });
        outcome
    }

    /// Toggle a project's collapsed state (persisted in settings).
    pub fn toggle_project_collapsed(&mut self, project_id: &str, cx: &mut HostCx) {
        let mut settings = self.settings.clone();
        if let Some(pos) = settings
            .collapsed_projects
            .iter()
            .position(|id| id == project_id)
        {
            settings.collapsed_projects.remove(pos);
        } else {
            settings.collapsed_projects.push(project_id.to_string());
        }
        self.update_settings(settings, cx);
    }

    pub(crate) fn resident(&self, id: &str) -> Option<&ActiveSession> {
        self.residents.resident(id)
    }

    pub(super) fn resident_mut(&mut self, id: &str) -> Option<&mut ActiveSession> {
        self.residents.resident_mut(id)
    }

    pub(super) fn find_meta(&self, id: &str) -> Option<SessionMeta> {
        self.sessions
            .iter()
            .find(|meta| meta.id == id)
            .cloned()
            .or_else(|| self.resident(id).map(|session| session.meta.clone()))
    }

    /// Directory where one session's image attachments are persisted.
    pub(crate) fn attachments_dir_for(&self, session_id: &str) -> PathBuf {
        user_files::attachment_dir(self.store.root(), session_id)
    }

    /// Whether a client may save an attachment into `dir` with extension
    /// `ext`: only a directory below this host's attachments root, named
    /// without traversal, and only a plain extension for the generated name.
    pub(crate) fn accepts_attachment(&self, dir: &Path, ext: &str) -> bool {
        dir.starts_with(user_files::attachments_root(self.store.root()))
            && !dir
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
            && tcode_core::attachments::is_safe_extension(ext)
    }

    /// Persist attachment bytes to a previously captured active-session target.
    /// Callers run this blocking helper on the background executor.
    pub fn save_attachment_to_dir(dir: &Path, bytes: &[u8], ext: &str) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.{ext}", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    pub fn update_settings(&mut self, settings: Settings, cx: &mut HostCx) {
        let settlement_changed = self.settings.auto_settle_after_days
            != settings.auto_settle_after_days
            || self.settings.auto_settle_on_merge != settings.auto_settle_on_merge
            || self.settings.project_settlement_overrides != settings.project_settlement_overrides
            || self.settings.settlement_automation != settings.settlement_automation;
        self.enqueue_settings(&settings, cx);
        if ProviderKind::NATIVE
            .iter()
            .any(|&provider| self.settings.provider(provider) != settings.provider(provider))
        {
            self.providers.invalidate_versions();
        }
        let language = settings.language.clone();
        let changed: HashSet<_> = self
            .providers
            .provider_usage
            .keys()
            .chain(self.providers.usage_checking.iter())
            .chain(self.providers.provider_snapshots.keys())
            .filter(|id| self.settings.resolved_profile(id) != settings.resolved_profile(id))
            .cloned()
            .collect();
        for id in changed {
            self.providers.invalidate_profile_reads(&id);
        }
        let hosts_changed = self.settings.source_control.hosts != settings.source_control.hosts;
        let control_changed = self.settings.control != settings.control;
        self.settings = settings;
        if control_changed {
            self.refresh_control_grants();
        }
        self.forge
            .configure(self.settings.source_control.hosts.clone());
        if hosts_changed {
            self.name_pull_request_hosts();
            self.refresh_host_credentials(cx);
        }
        if settlement_changed {
            self.evaluate_all_settlement(cx);
        }
        self.forget_disabled_plugin_catalogs(cx);
        self.providers.provider_secret_names =
            provider_secret_names(&self.settings, &self.settings_store);
        // Keep the live computer-use MCP config in step with the persisted
        // settings on every change (the server outlives any one snapshot).
        computer_use_mcp::config::set(self.settings.computer_use.clone());
        emit_runtime(
            cx,
            RuntimeEvent::Effect(RuntimeEffect::ApplyLocale { language }),
        );
    }

    pub fn patch_settings(&mut self, patch: tcode_protocol::SettingsPatch, cx: &mut HostCx) {
        let mut settings = self.settings.clone();
        // Command validation already refused a patch `apply` would refuse.
        if settings.apply(patch).is_ok() {
            self.update_settings(settings, cx);
        }
    }

    /// Persist a restart-continuity marker naming the Settings page to reopen and
    /// the session that is active now. Written before a Screen Recording request
    /// or an explicit relaunch, so an externally-initiated quit reopens cleanly.
    pub fn write_relaunch_marker(&self, target_id: &str, reopen_settings: &str) {
        let marker = tcode_services::relaunch::RelaunchMarker {
            reopen_settings: reopen_settings.to_string(),
            active_session: self
                .resident(target_id)
                .map(|session| session.meta.id.as_str())
                .map(str::to_string),
        };
        if let Err(err) = tcode_services::relaunch::write(self.store.root(), &marker) {
            log::warn!("failed to write relaunch marker: {err}");
        }
    }

    pub fn clear_relaunch_marker(&self) {
        if let Err(err) = tcode_services::relaunch::clear(self.store.root()) {
            log::warn!("failed to clear relaunch marker: {err}");
        }
    }

    /// Apply a marker taken at launch: reopen the recorded session and open
    /// Settings on the recorded page. The page reruns a permission recheck as it
    /// mounts, so the user immediately sees the post-restart status. No-op when
    /// there is no marker (the normal launch path).
    pub fn apply_pending_relaunch(&mut self) -> (Option<String>, Option<String>) {
        let Some(marker) = self.pending_relaunch.take() else {
            return (None, None);
        };
        let session_id = marker
            .active_session
            .filter(|id| self.sessions.iter().any(|meta| meta.id == *id));
        (Some(marker.reopen_settings), session_id)
    }

    pub(super) fn settle_thread_busy(&self, id: &str) -> bool {
        self.resident(id).is_some_and(|session| {
            session.preparing_worktree
                || matches!(session.runtime, Runtime::Starting { .. })
                || session.turn_in_flight
                || session.delivery_in_flight.is_some()
                || session.timeline.turn_running
                || !session.timeline.pending_approvals.is_empty()
                || session
                    .timeline
                    .pending_user_input
                    .as_ref()
                    .is_some_and(|input| input.delivery.is_blocking())
                || session
                    .queue
                    .iter()
                    .any(|message| message.origin == MessageOrigin::Human)
        })
    }

    pub fn settle_session(&mut self, id: &str, cx: &mut HostCx) -> Option<SettleEffects> {
        if self.settle_thread_busy(id) {
            return None;
        }
        self.settle_session_at(id, now_secs(), cx)
    }

    /// What settling the thread now would do beyond marking it settled.
    pub(super) fn settle_effects(&self, id: &str) -> SettleEffects {
        let meta = self.find_meta(id);
        let resident = self.resident(id);
        SettleEffects {
            provider_detached: resident
                .is_some_and(|session| !matches!(session.runtime, Runtime::Idle)),
            pull_request_watches_ended: meta.as_ref().map_or(0, |meta| {
                meta.pull_requests
                    .iter()
                    .filter(|link| link.watch.is_some())
                    .count()
            }),
            queued_dropped: resident.map_or(0, |session| {
                session
                    .queue
                    .iter()
                    .filter(|message| message.origin != MessageOrigin::Human)
                    .count()
            }),
            terminals_checked: resident
                .map(|session| &session.terminal_workspace)
                .or_else(|| {
                    self.terminal_workspaces
                        .get(&ConversationDestination::Thread(id.to_owned()))
                })
                .map_or(0, |workspace| workspace.terminals.len()),
            unpinned: meta.as_ref().is_some_and(|meta| meta.pinned_at.is_some()),
            input_answered: resident
                .and_then(|session| session.timeline.pending_user_input.as_ref())
                .is_some_and(|input| !input.delivery.is_blocking()),
        }
    }

    /// Settle this thread only: detach its provider, cancel its automatic
    /// queue and close its idle terminals. Children are separate threads.
    pub(super) fn settle_session_at(
        &mut self,
        id: &str,
        timestamp: u64,
        cx: &mut HostCx,
    ) -> Option<SettleEffects> {
        let mut meta = self
            .find_meta(id)
            .filter(|meta| meta.archived_at.is_none())?;
        let effects = self.settle_effects(id);
        if !meta.is_settled() {
            meta.settled_at = Some(timestamp);
        }
        if let Some(input) = self
            .resident(id)
            .and_then(|session| session.timeline.pending_user_input.as_ref())
            .filter(|input| !input.delivery.is_blocking())
        {
            let request_id = input.request_id.clone();
            self.on_event(
                id,
                AgentEvent::UserInputResolved {
                    request_id,
                    answers: Default::default(),
                },
                cx,
            );
        }
        meta.settled_override = Some(SettledOverride::Settled);
        meta.unsettled_at = None;
        self.end_pull_request_watches(&mut meta);
        meta.pinned_at = None;
        meta.pin_order = None;
        meta.active_order = None;
        self.detach_provider_to_idle(id, cx);
        if let Some(session) = self.resident_mut(id) {
            session
                .queue
                .retain(|message| message.origin == MessageOrigin::Human);
            session.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
        self.reschedule_scheduled_wake(cx);
        self.close_settled_idle_terminals(id, cx);
        self.fire_control_watches(id, WatchEvent::Settled, cx);
        Some(effects)
    }

    pub fn unsettle_session(&mut self, id: &str, cx: &mut HostCx) {
        let Some(mut meta) = self.find_meta(id).filter(|meta| meta.archived_at.is_none()) else {
            return;
        };
        if meta.settled_override == Some(SettledOverride::Active) {
            return;
        }
        meta.settled_override = Some(SettledOverride::Active);
        meta.settled_at = None;
        meta.unsettled_at = Some(now_secs());
        if let Some(session) = self.resident_mut(id) {
            session.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }

    /// Pinning promotes a settled thread to Active without stamping
    /// `unsettled_at`. A thread already pinned keeps its time and key, so a
    /// raced duplicate never moves a thread the user placed.
    pub fn pin_session(&mut self, id: &str, order_key: Option<String>, cx: &mut HostCx) {
        let Some(mut meta) = self.find_meta(id).filter(|meta| meta.archived_at.is_none()) else {
            return;
        };
        let promotes = meta.is_settled();
        if meta.pinned_at.is_some() && !promotes {
            return;
        }
        if meta.pinned_at.is_none() {
            meta.pinned_at = Some(now_secs());
            meta.pin_order = order_key;
        }
        if promotes {
            meta.settled_override = Some(SettledOverride::Active);
            meta.settled_at = None;
        }
        if let Some(session) = self.resident_mut(id) {
            session.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }

    /// Unpinning clears the key too: pinning again is a fresh slot unless
    /// the client supplies the old one. The thread stays in Active.
    pub fn unpin_session(&mut self, id: &str, cx: &mut HostCx) {
        let Some(mut meta) = self.find_meta(id).filter(|meta| meta.archived_at.is_none()) else {
            return;
        };
        if meta.pinned_at.is_none() {
            return;
        }
        meta.pinned_at = None;
        meta.pin_order = None;
        if let Some(session) = self.resident_mut(id) {
            session.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }

    pub fn reorder_session(
        &mut self,
        id: &str,
        section: ThreadSection,
        order_key: String,
        cx: &mut HostCx,
    ) {
        let Some(mut meta) = self.find_meta(id).filter(|meta| meta.archived_at.is_none()) else {
            return;
        };
        let slot = match section {
            ThreadSection::Pinned => &mut meta.pin_order,
            ThreadSection::Active => &mut meta.active_order,
            ThreadSection::Settled => return,
        };
        if slot.as_deref() == Some(order_key.as_str()) {
            return;
        }
        *slot = Some(order_key);
        if let Some(session) = self.resident_mut(id) {
            session.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }

    pub fn set_auto_settle(&mut self, id: &str, enabled: bool, cx: &mut HostCx) {
        let Some(mut meta) = self.find_meta(id).filter(|meta| meta.archived_at.is_none()) else {
            return;
        };
        if enabled == meta.auto_settle_disabled_at.is_none() {
            return;
        }
        meta.auto_settle_disabled_at = (!enabled).then(now_secs);
        if let Some(session) = self.resident_mut(id) {
            session.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
        self.evaluate_thread_settlement(id, cx);
    }

    /// A message accepted for this thread: reopen it and make any completion
    /// already on its way from it stale. Its queued work blocks settlement
    /// until the provider records it. The user speaking withdraws a self-settle
    /// grant: the thread may only settle itself again if asked anew.
    pub(super) fn reactivate_session(&mut self, id: &str, cx: &mut HostCx) {
        let Some(mut meta) = self.find_meta(id).filter(|meta| meta.archived_at.is_none()) else {
            return;
        };
        let withdraws_self_settle = meta.self_settle.is_some()
            && cx.origin.unwrap_or(MessageOrigin::Human) == MessageOrigin::Human;
        if !withdraws_self_settle
            && meta.settled_override.is_none()
            && meta.settled_at.is_none()
            && meta.cancelled_at.is_none()
        {
            return;
        }
        if withdraws_self_settle {
            meta.self_settle = None;
        }
        if meta.is_settled() {
            meta.unsettled_at = Some(now_secs());
        }
        meta.settled_override = None;
        meta.settled_at = None;
        meta.cancelled_at = None;
        if let Some(session) = self.resident_mut(id) {
            session.meta = meta.clone();
        }
        self.persist_meta(&meta, cx);
    }

    /// Archive a thread (reversible; it vanishes from the sidebar). Blocked while
    /// its turn is running (returns without changing anything so the caller's
    /// tooltip stands). The active thread is closed back to the empty state.
    pub fn archive_session(&mut self, session_id: &str, cx: &mut HostCx) {
        if self.turn_running_for(session_id)
            || self
                .sessions
                .iter()
                .find(|meta| meta.id == session_id)
                .is_none_or(|meta| meta.archived_at.is_some())
        {
            return;
        }
        self.archive_session_ids(&[session_id.to_owned()], now_secs(), cx);
    }

    /// Restore an archived thread (Settings → Archived Threads → Unarchive).
    /// A thread that was read before archiving stays read.
    pub fn unarchive_session(&mut self, session_id: &str, cx: &mut HostCx) {
        let Some(archived_at) = self
            .sessions
            .iter()
            .find(|meta| meta.id == session_id)
            .and_then(|meta| meta.archived_at)
        else {
            return;
        };
        let now = now_secs();
        let mut visited_changed = false;
        let ids = descendant_session_ids(&self.sessions, session_id);
        for id in ids {
            let Some(mut meta) = self
                .sessions
                .iter()
                .find(|meta| meta.id == id && meta.archived_at == Some(archived_at))
                .cloned()
            else {
                continue;
            };
            if let Some(visited) = self.settings.last_visited.get_mut(&id)
                && *visited >= meta.updated_at
            {
                *visited = now;
                visited_changed = true;
            }
            meta.archived_at = None;
            meta.updated_at = now;
            self.persist_meta(&meta, cx);
        }
        if visited_changed {
            self.persist_settings(cx);
        }
    }

    pub(super) fn archive_session_ids(
        &mut self,
        ids: &[String],
        archived_at: u64,
        cx: &mut HostCx,
    ) {
        let ids: HashSet<String> = ids
            .iter()
            .flat_map(|id| descendant_session_ids(&self.sessions, id))
            .collect();

        for id in &ids {
            self.retire_provider_work(id, cx);
            self.shutdown_active(id, cx);
            // An archived conversation must not leave an off-screen PTY running.
            self.terminal_workspaces
                .remove(&ConversationDestination::Thread(id.to_string()));
            self.drop_background(id, cx);
            self.revoke_preview_registration(id);
            self.revoke_pull_request_registration(id);
            self.revoke_orchestrate_child_registration(id);
            self.revoke_control_registration(id);
        }
        for id in &ids {
            self.close_orchestrator_children(id, cx);
        }
        let changed: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| ids.contains(meta.id.as_str()))
            .map(|meta| {
                let mut meta = meta.clone();
                meta.archived_at = Some(archived_at);
                // To its lead, an agent archived before settle is cancelled:
                // restoring it later must not hold the lead again.
                if meta.is_dispatched() && !meta.is_settled() && meta.cancelled_at.is_none() {
                    meta.cancelled_at = Some(archived_at);
                }
                meta
            })
            .collect();
        for mut meta in changed {
            self.end_pull_request_watches(&mut meta);
            self.persist_meta(&meta, cx);
        }
    }

    /// Rename a thread (context-menu inline edit). Empty titles are rejected.
    pub fn rename_session(&mut self, session_id: &str, title: &str, cx: &mut HostCx) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        if let Some(session) = self.resident_mut(session_id) {
            session.meta.title = title.to_string();
        }
        if let Some(mut meta) = self.sessions.iter().find(|m| m.id == session_id).cloned() {
            meta.title = title.to_string();
            meta.updated_at = now_secs();
            let meta = meta.clone();
            self.persist_meta(&meta, cx);
        }
    }

    pub(super) fn session_fork_availability(
        meta: &SessionMeta,
        resident: Option<&ActiveSession>,
    ) -> ForkAvailability {
        if !meta.provider.caps().supports_fork {
            ForkAvailability::Unsupported
        } else if meta.resume_cursor.is_none() {
            ForkAvailability::Empty
        } else if resident.is_some_and(|session| session.turn_in_flight) {
            ForkAvailability::Running
        } else {
            ForkAvailability::Available
        }
    }

    /// Duplicate a stored transcript and arrange for its next provider start to
    /// fork the source's native session. The fork stays idle until its first
    /// user turn, exactly like a cold-opened stored thread.
    pub fn fork_thread(&mut self, id: &str, cx: &mut HostCx) -> Option<String> {
        let resident = self.resident(id);
        let source = resident
            .map(|session| &session.meta)
            .or_else(|| self.sessions.iter().find(|meta| meta.id == id))?;
        if let Some(error) = fork_refusal(Self::session_fork_availability(source, resident)) {
            self.report_error(RuntimeError::External(error.into()), cx);
            return None;
        }
        let source = source.clone();

        let mut fork = SessionMeta::new(source.provider, source.cwd.clone(), source.model.clone());
        fork.title = format!("{} (fork)", source.title);
        fork.option_selections = source.option_selections.clone();
        fork.project_id = source.project_id.clone();
        fork.acp_agent_id = source.acp_agent_id.clone();
        fork.profile_id = source.profile_id.clone();
        fork.resume_cursor = source.resume_cursor.clone();
        // The fork carries the source's context, so it may read what the source could.
        fork.references = source.references.clone();
        fork.pending_fork = true;
        // `worktree` deliberately stays absent: it is an ownership/cleanup
        // marker. The cwd may be shared, but the fork must not own the source's
        // generated worktree or offer to delete it.

        let fork_id = fork.id.clone();
        self.upsert_session_in_memory(fork.clone());
        let (completion, completed) = smol::channel::bounded(1);
        self.enqueue_store_write(
            StoreWrite::Fork {
                src: source.id,
                meta: Box::new(fork.clone()),
                completion,
            },
            cx,
        );
        self.queue_activity_load(&fork_id, cx);
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = completed
                .recv()
                .await
                .unwrap_or_else(|_| Err("session store writer stopped".into()));
            host_cx.enqueue(move |state, cx| match result {
                Ok(()) => {
                    state.select_session(&fork.id, cx);
                    state.reply_to_subscription(
                        None,
                        tcode_protocol::Subscription {
                            known_index: None,
                            topic: Topic::SessionEvents {
                                session_id: fork.id.clone(),
                            },
                            after: None,
                        },
                        cx,
                    );
                }
                Err(error) => {
                    state.report_error(RuntimeError::PersistSession { error }, cx);
                }
            });
        });
        Some(fork_id)
    }

    /// Permanently delete a thread and every thread under it (the tree
    /// archive acts on): stop their providers, close their terminals, delete
    /// their metas and event logs in one transaction, and (when
    /// `remove_worktree`) remove each git worktree they own that no remaining
    /// thread works in. A worktree they own that is not removed is recorded as
    /// kept, so the startup sweep leaves it.
    pub fn delete_session(&mut self, session_id: &str, remove_worktree: bool, cx: &mut HostCx) {
        let mut ids = descendant_session_ids(&self.sessions, session_id);
        if ids.is_empty() {
            ids.push(session_id.to_string());
        }
        self.delete_session_ids(&ids, remove_worktree, cx);
    }

    fn delete_session_ids(&mut self, ids: &[String], remove_worktree: bool, cx: &mut HostCx) {
        if ids.is_empty() {
            return;
        }
        let deleted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let metas: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| deleted.contains(meta.id.as_str()))
            .cloned()
            .collect();
        let mut terminal_preferences_changed = false;
        for id in ids {
            self.clear_approvals(id);
            if self.residents.live.contains_key(id) {
                // shutdown_active drops the ActiveSession (and its terminal PTY).
                self.shutdown_active(id, cx);
            }
            // Deleting a thread that is working in the background kills it for real.
            self.drop_background(id, cx);
            self.terminal_workspaces
                .remove(&ConversationDestination::Thread(id.clone()));
            terminal_preferences_changed |= self.terminal_preferences.remove(id).is_some();
            self.revoke_orchestrate_child_registration(id);
            self.close_orchestrator_children(id, cx);
            self.settings.last_visited.remove(id);
        }
        if terminal_preferences_changed {
            self.write_terminal_preferences(cx);
        }
        if metas
            .iter()
            .any(|meta| meta.archived_at.is_some() || self.archived_sharing_affected(meta))
        {
            self.archived_revision += 1;
        }
        self.sessions
            .retain(|meta| !deleted.contains(meta.id.as_str()));
        self.thread_activity
            .retain(|id, _| !deleted.contains(id.as_str()));
        self.settlement_deadlines
            .retain(|id, _| !deleted.contains(id.as_str()));
        self.rearm_settlement_timer(cx);
        self.activity_pending
            .retain(|id| !deleted.contains(id.as_str()));
        self.activity_priority
            .retain(|id| !deleted.contains(id.as_str()));
        self.activity_queued
            .retain(|id| !deleted.contains(id.as_str()));
        self.activity_deferred
            .retain(|id| !deleted.contains(id.as_str()));
        self.decision_revisions
            .retain(|id, _| !deleted.contains(id.as_str()));
        let mut kept_worktrees = Vec::new();
        let mut worktree_removals = Vec::new();
        let sharing = self.worktree_sharing();
        for meta in &metas {
            let Some(worktree) = &meta.worktree else {
                continue;
            };
            if !remove_worktree {
                kept_worktrees.push(meta.cwd.clone());
            } else if sharing.is_shared(meta) {
                log::info!(
                    "keeping worktree {} in use by another thread",
                    meta.cwd.display()
                );
            } else {
                worktree_removals.push((
                    meta.id.clone(),
                    worktree.root_project_path.clone(),
                    meta.cwd.clone(),
                ));
            }
        }
        self.enqueue_store_write(
            StoreWrite::RemoveSessions {
                ids: ids.to_vec(),
                kept_worktrees,
            },
            cx,
        );
        let parents: HashSet<_> = metas
            .iter()
            .filter_map(|meta| meta.parent_session_id.as_deref())
            .filter(|id| !deleted.contains(*id))
            .collect();
        for parent in parents {
            self.evaluate_thread_settlement(parent, cx);
        }
        // Persist the pruned last-visited map (ignore save errors — cosmetic).
        self.persist_settings(cx);
        for (deleted_id, root, cwd) in worktree_removals {
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let result = host_cx
                    .unblock(move || remove_git_worktree(&root, &cwd))
                    .await;
                host_cx.enqueue(move |state, cx| {
                    if let Err(err) = result
                        && !state.sessions.iter().any(|meta| meta.id == deleted_id)
                        && !state.residents.live.contains_key(&deleted_id)
                        && !state.residents.parked.contains_key(&deleted_id)
                    {
                        state.report_error(
                            RuntimeError::WorktreeRemove {
                                error: err.to_string(),
                            },
                            cx,
                        );
                    }
                });
            });
        }
    }

    /// Permanently remove a project and all of its threads from tcode. Project
    /// files and worktrees on disk are left in place.
    pub fn delete_project(&mut self, project_id: &str, cx: &mut HostCx) {
        let session_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|meta| meta.project_id.as_deref() == Some(project_id))
            .map(|meta| meta.id.clone())
            .collect();
        let drafts: Vec<_> = self
            .residents
            .live
            .values()
            .filter(|s| s.draft && s.meta.project_id.as_deref() == Some(project_id))
            .map(|s| s.meta.id.clone())
            .collect();
        for id in drafts {
            self.shutdown_active(&id, cx);
        }
        let draft_destination = ConversationDestination::ProjectDraft(project_id.to_string());
        self.terminal_workspaces.remove(&draft_destination);
        if self
            .terminal_preferences
            .remove(&draft_destination.preference_key())
            .is_some()
        {
            self.write_terminal_preferences(cx);
        }
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        for session_id in session_ids {
            if seen.contains(&session_id) {
                continue;
            }
            for id in descendant_session_ids(&self.sessions, &session_id) {
                if seen.insert(id.clone()) {
                    ids.push(id);
                }
            }
        }
        self.delete_session_ids(&ids, false, cx);
        self.enqueue_store_write(StoreWrite::RemoveProject(project_id.to_string()), cx);
        self.settings
            .collapsed_projects
            .retain(|id| id != project_id);
        self.persist_settings(cx);
        self.projects.retain(|project| project.id != project_id);
        self.replace_external_import_status(project_id, None, cx);
    }

    /// Whether `session_id` owns live or queued work.
    pub(crate) fn turn_running_for(&self, session_id: &str) -> bool {
        self.resident(session_id)
            .is_some_and(ActiveSession::has_work)
    }

    /// Advance a thread's last-visited watermark to the `updated_at` a client
    /// has shown. A late acknowledgement never rewinds a newer one.
    pub fn mark_session_read(&mut self, session_id: &str, through: u64, cx: &mut HostCx) {
        if self
            .settings
            .last_visited
            .get(session_id)
            .is_some_and(|&visited| visited >= through)
        {
            return;
        }
        self.settings
            .last_visited
            .insert(session_id.to_string(), through);
        self.persist_settings(cx);
    }

    /// Mark a thread unread (context menu): set its last-visited just below its
    /// update time so the dot reappears.
    pub fn mark_session_unread(&mut self, session_id: &str, cx: &mut HostCx) {
        let updated = self
            .sessions
            .iter()
            .find(|m| m.id == session_id)
            .map(|m| m.updated_at)
            .unwrap_or(0);
        self.settings
            .last_visited
            .insert(session_id.to_string(), updated.saturating_sub(1));
        self.persist_settings(cx);
    }

    /// Whether a thread shows an unread dot: it has been visited before, its
    /// update time is newer than that visit, it is not the active thread, and
    /// it is not settled.
    pub(crate) fn session_unread(&self, meta: &SessionMeta) -> bool {
        !meta.is_settled()
            && !self.residents.live.contains_key(&meta.id)
            && self
                .settings
                .last_visited
                .get(&meta.id)
                .is_some_and(|&visited| meta.updated_at > visited)
    }

    /// Remove app-owned worktrees that no thread in the store works in and the
    /// user did not keep.
    pub(crate) fn recover_orphaned_worktrees(&self, cx: &mut HostCx) {
        let store = self.store.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let summary = host_cx.unblock(move || cleanup_orphans(&store)).await;
            if !summary.removed.is_empty() || !summary.skipped.is_empty() {
                log::info!(
                    "worktree orphan recovery removed {}, left {}",
                    summary.removed.len(),
                    summary.skipped.len()
                );
            }
        });
    }

    /// Choose the draft's workspace mode (checkout-row picker). No-op unless the
    /// active thread is an unstarted draft.
    pub fn set_draft_workspace(&mut self, target_id: &str, mode: WorkspaceMode, _cx: &mut HostCx) {
        if let Some(active) = self.resident_mut(target_id).filter(|a| a.draft) {
            active.draft_workspace = mode;
        }
    }

    /// Kick off background worktree creation for a draft's first send, then send
    /// the queued text once it is ready. Sets the "Preparing worktree…" state.
    pub(super) fn begin_worktree_prep(
        &mut self,
        target_id: &str,
        text: String,
        attachments: Vec<Attachment>,
        _base: String,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        active.preparing_worktree = true;
        let session_id = active.meta.id.clone();
        let session_id_for_task = session_id.clone();
        let root = active.meta.cwd.clone();

        let root_for_task = root.clone();
        let data_dir = self.store.root().clone();
        let target_id = target_id.to_string();
        let delivery_key = cx.delivery_key.clone();
        let author = cx.author.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || provision(&root_for_task, &session_id_for_task, &data_dir))
                .await;
            host_cx.enqueue(move |state, cx| {
                if state
                    .resident(&target_id)
                    .is_some_and(|active| state.archived_sharing_affected(&active.meta))
                {
                    state.archived_revision += 1;
                }
                let Some(active) = state
                    .resident_mut(&target_id)
                    .filter(|a| a.meta.id == session_id && a.draft)
                else {
                    return;
                };
                active.preparing_worktree = false;
                match result {
                    Ok(created) => {
                        active.meta.cwd = created.path.clone();
                        active.meta.worktree = Some(WorktreeInfo {
                            root_project_path: root,
                            base: created.base,
                            branch: created.branch.clone(),
                        });
                        active.draft_workspace = WorkspaceMode::LocalCheckout;
                        active.git_branch = Some(created.branch);
                        if created.seed_summary.manifest_found {
                            emit_runtime(
                                cx,
                                RuntimeEvent::Notice(RuntimeNotice::WorktreeSeeded {
                                    copied_files: created.seed_summary.copied_files,
                                    skipped: created.seed_summary.skipped,
                                    limit_reached: created.seed_summary.limit_reached,
                                }),
                            );
                        }
                        // Now that the worktree exists, run the deferred send.
                        let previous_key = std::mem::replace(&mut cx.delivery_key, delivery_key);
                        let previous_author = std::mem::replace(&mut cx.author, author);
                        state.send_turn_assembled(&target_id, text, attachments, cx);
                        cx.delivery_key = previous_key;
                        cx.author = previous_author;
                    }
                    Err(err) => {
                        active.draft_workspace = WorkspaceMode::LocalCheckout;
                        state.report_error(
                            RuntimeError::WorktreeAdd {
                                error: err.to_string(),
                            },
                            cx,
                        );
                    }
                }
            });
        });
    }

    /// Build a draft for `cwd` under `project_id` without persisting it or
    /// starting a provider (see `commit_draft`).
    pub(super) fn build_draft_session(
        project_id: String,
        cwd: PathBuf,
        provider: ProviderKind,
        model: Option<String>,
        acp_agent_id: Option<String>,
        provider_commands: Vec<ProviderCommand>,
    ) -> ActiveSession {
        let mut meta = SessionMeta::new(provider, cwd, model);
        meta.project_id = Some(project_id);
        meta.acp_agent_id = acp_agent_id;
        ActiveSession::new(meta, true, provider_commands)
    }

    /// The provider + model a new draft should start with: the most recently
    /// updated, non-archived session in this project. Only reasoning effort is
    /// inherited from its model options. Projects without active history fall
    /// back to the most recently updated non-archived global session (or the
    /// Claude default), without inheriting model options.
    pub(super) fn draft_defaults(
        &self,
        project_id: &str,
    ) -> (
        ProviderKind,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<OptionSelection>,
    ) {
        if let Some(meta) = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.archived_at.is_none() && meta.project_id.as_deref() == Some(project_id)
            })
            .max_by_key(|meta| meta.updated_at)
        {
            let reasoning_effort = meta
                .option_selections
                .iter()
                .find(|selection| selection.id == "reasoningEffort")
                .cloned();
            return (
                meta.provider,
                meta.model.clone(),
                meta.acp_agent_id.clone(),
                // Inherit the profile too, so "new thread" keeps talking to the
                // same third-party endpoint instead of falling back to the
                // built-in provider (which would reject the profile's model).
                meta.profile_id.clone(),
                reasoning_effort,
            );
        }

        match self
            .sessions
            .iter()
            .filter(|meta| meta.archived_at.is_none())
            .max_by_key(|meta| meta.updated_at)
        {
            Some(meta) => (
                meta.provider,
                meta.model.clone(),
                meta.acp_agent_id.clone(),
                meta.profile_id.clone(),
                None,
            ),
            None => (ProviderKind::ClaudeCode, None, None, None, None),
        }
    }

    /// The permission a new conversation with `provider` starts at: the
    /// project's configured value, otherwise the provider's recommended one.
    pub(super) fn initial_permission_selection(
        &self,
        project_id: Option<&str>,
        provider: ProviderKind,
    ) -> Option<OptionSelection> {
        let descriptor = permission_control(provider)?;
        let configured = project_id
            .and_then(|id| self.projects.iter().find(|project| project.id == id))
            .and_then(|project| {
                project
                    .permission_defaults
                    .get(tcode_core::settings::provider_key(provider))
            });
        match descriptor {
            OptionDescriptor::Select {
                id, recommended, ..
            } => Some(OptionSelection {
                id,
                value: serde_json::Value::String(configured.cloned().or(recommended)?),
            }),
            OptionDescriptor::Boolean {
                id, recommended, ..
            } => Some(OptionSelection {
                id,
                value: serde_json::Value::Bool(
                    configured
                        .and_then(|configured| configured.parse().ok())
                        .or(recommended)?,
                ),
            }),
        }
    }

    /// The reasoning effort last used with exactly this (provider, model),
    /// from the most recently updated session that ran it (archived included:
    /// memory outlives the thread). Model switches restore this instead of
    /// resetting to the model's default. Keyed per model, so the remembered
    /// value is always one the model accepts.
    pub(super) fn remembered_effort(
        &self,
        provider: ProviderKind,
        model: Option<&str>,
    ) -> Option<OptionSelection> {
        self.sessions
            .iter()
            .filter(|meta| meta.provider == provider && meta.model.as_deref() == model)
            .max_by_key(|meta| meta.updated_at)
            .and_then(|meta| {
                meta.option_selections
                    .iter()
                    .find(|selection| selection.id == "reasoningEffort")
                    .cloned()
            })
    }

    /// Switch the main area into a draft for `project_id` (rooted at `cwd`): an
    /// empty timeline with a focused, functional composer. The session is
    /// created lazily on the first send (see `send_turn`/`commit_draft`).
    pub fn start_draft(&mut self, project_id: String, cwd: PathBuf, cx: &mut HostCx) -> String {
        let device_id = match &cx.principal {
            tcode_protocol::Principal::Full => None,
            tcode_protocol::Principal::Space { device_id, .. } => Some(device_id.clone()),
        };
        let standing = self
            .residents
            .live
            .values()
            .chain(self.residents.parked.values())
            .find(|active| {
                active.draft
                    && active.draft_device_id == device_id
                    && active.meta.project_id.as_deref() == Some(project_id.as_str())
                    && active.meta.cwd == cwd
            })
            .map(|active| active.meta.id.clone());
        if let Some(session_id) = standing {
            if let Some(mut parked) = self.residents.adopt(&session_id) {
                self.restore_terminal_workspace(&mut parked);
                if self.archived_sharing_affected(&parked.meta) {
                    self.archived_revision += 1;
                }
                self.residents.live.insert(session_id.clone(), parked);
            }
            self.refresh_git_status(&session_id, cx);
            return session_id;
        }
        let (provider, model, acp_agent_id, profile_id, reasoning_effort) =
            self.draft_defaults(&project_id);
        let provider_commands =
            self.cached_provider_commands(provider, profile_id.as_deref(), acp_agent_id.as_deref());
        let mut draft = Self::build_draft_session(
            project_id,
            cwd,
            provider,
            model,
            acp_agent_id,
            provider_commands,
        );
        draft.draft_device_id = device_id;
        draft.meta.profile_id = profile_id;
        draft.meta.option_selections = reasoning_effort.into_iter().collect();
        if let Some(selection) =
            self.initial_permission_selection(draft.meta.project_id.as_deref(), provider)
        {
            draft.meta.option_selections.push(selection);
        }
        let terminal_preferences = self.terminal_preferences_for(&draft);
        let restored_terminal = self.restore_terminal_workspace(&mut draft);
        let session_id = draft.meta.id.clone();
        self.thread_activity
            .insert(session_id.clone(), Default::default());
        if self.archived_sharing_affected(&draft.meta) {
            self.archived_revision += 1;
        }
        self.residents.live.insert(session_id.clone(), draft);
        if let Some(active) = self.resident(&session_id) {
            self.refresh_session_git_branch(active.meta.id.clone(), active.meta.cwd.clone(), cx);
        }
        if !restored_terminal {
            self.reopen_persisted_terminals(&session_id, terminal_preferences, cx);
        }
        self.refresh_git_status(&session_id, cx);
        session_id
    }

    /// Persist the active draft as a real session.
    /// The session id is preserved, so its already-recorded events line up.
    pub(super) fn commit_draft(&mut self, target_id: &str, cx: &mut HostCx) {
        let preference_migration = self.resident(target_id).and_then(|active| {
            active.draft.then(|| {
                (
                    conversation_destination(active).preference_key(),
                    active.meta.id.clone(),
                )
            })
        });
        if let Some(active) = self.resident_mut(target_id)
            && active.draft
        {
            active.draft = false;
            let meta = active.meta.clone();
            self.emit_domain(
                Topic::SessionEvents {
                    session_id: meta.id.clone(),
                },
                ServerEvent::SessionSnapshot {
                    total: 0,
                    total_turns: 0,
                    truncated: false,
                    from: 0,
                    end: 0,
                    records: Vec::new(),
                },
                cx,
            );
            self.enqueue_store_write(
                StoreWrite::UpsertMeta {
                    meta: Box::new(meta.clone()),
                    initial: true,
                },
                cx,
            );
            self.upsert_session_in_memory(meta);
        }
        if let Some((draft_key, session_key)) = preference_migration
            && let Some(preferences) = self.terminal_preferences.remove(&draft_key)
        {
            self.terminal_preferences.insert(session_key, preferences);
            self.write_terminal_preferences(cx);
        }
    }

    pub(super) fn schedule_timeline_load(
        &mut self,
        session_id: String,
        target: TimelineLoadTarget,
        cx: &mut HostCx,
    ) {
        let generation = {
            let generation = self
                .timeline_load_generations
                .entry(session_id.clone())
                .or_default();
            *generation += 1;
            *generation
        };
        let intended = match target {
            TimelineLoadTarget::Active { .. } => self.resident(&session_id),
            TimelineLoadTarget::Background => self.residents.parked.get(&session_id),
        };
        let Some(cwd) = intended.map(|session| session.meta.cwd.clone()) else {
            return;
        };
        if matches!(target, TimelineLoadTarget::Active { .. }) {
            self.refresh_session_git_branch(session_id.clone(), cwd, cx);
        }
        let load = TimelineLoad { generation, target };
        // A cached log is the whole conversation, including appends whose
        // writes are still queued, so the timeline derives from it and never
        // from the store, which lags the writes still queued.
        match self.event_records.get(&session_id) {
            Some(log) => {
                let cursor = log.end();
                let fold = log.fold().clone();
                self.fold_timeline(session_id, load, cursor, fold, cx);
            }
            None => {
                self.hydrate_log(&session_id, Some(load), cx);
            }
        }
    }

    /// Mark `fold`, the cached log's fold up to row `cursor`, idle as `load`
    /// asks, then make it the session's unless a later load superseded `load`
    /// or the session left the residency it was loaded for.
    pub(super) fn fold_timeline(
        &mut self,
        session_id: String,
        load: TimelineLoad,
        cursor: u64,
        mut fold: Timeline,
        cx: &mut HostCx,
    ) {
        let mark_idle = load.mark_idle();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let timeline = host_cx
                .unblock(move || {
                    if mark_idle {
                        fold.mark_idle();
                    }
                    fold
                })
                .await;
            host_cx.enqueue(move |state, cx| {
                let generation_matches = state.timeline_load_generations.get(&session_id).copied()
                    == Some(load.generation);
                let target_matches = match load.target {
                    TimelineLoadTarget::Active { .. } => {
                        state.residents.live.contains_key(&session_id)
                    }
                    TimelineLoadTarget::Background => {
                        state.residents.parked.contains_key(&session_id)
                    }
                };
                if !generation_matches || !target_matches {
                    return;
                }
                let mut timeline = timeline;
                // A resident session's log stays cached; records it accepted
                // after `cursor` continue it.
                if let Some(log) = state.event_records.get(&session_id) {
                    for record in log.records_from(cursor) {
                        timeline.apply_at(record.ts, &record.event);
                    }
                }
                if let Some(session) = state.resident_mut(&session_id) {
                    session.timeline = timeline;
                }
                state.repair_orphaned_mirror_turn(&session_id, cx);
            });
        });
    }

    /// Adopt a subscribed session, including an uncommitted parked draft. Stored
    /// sessions replay their event log; providers start lazily on the next send.
    pub fn select_session(&mut self, session_id: &str, cx: &mut HostCx) {
        if self.residents.live.contains_key(session_id) {
            return;
        }
        let Some(meta) = self.find_meta(session_id) else {
            return;
        };

        // A parked session is re-adopted, not replayed cold: its process, pump
        // and queue come back as they were, and the timeline is rebuilt from the
        // cached log — which stayed current while parked, because `record_event`
        // routes by session id.
        if let Some(mut parked) = self.residents.adopt(session_id) {
            log::info!(
                "re-adopting parked session {} (turn in flight: {}, queued: {})",
                session_id,
                parked.turn_in_flight,
                parked.queue.len()
            );
            let terminal_preferences = self.terminal_preferences_for(&parked);
            let restored_terminal = self.restore_terminal_workspace(&mut parked);
            let mark_idle = !parked.draft
                && matches!(parked.runtime, Runtime::Idle)
                && !parked.turn_in_flight
                && parked.delivery_in_flight.is_none()
                && parked.queue.is_empty()
                && parked.background_task_count == 0
                && !self.pending_native_rewinds.contains_key(session_id);
            let needs_restart = matches!(parked.runtime, Runtime::Idle) && !parked.queue.is_empty();
            if parked.draft && self.archived_sharing_affected(&parked.meta) {
                self.archived_revision += 1;
            }
            self.residents.live.insert(session_id.to_string(), parked);
            self.schedule_timeline_load(
                session_id.to_string(),
                TimelineLoadTarget::Active { mark_idle },
                cx,
            );
            if !restored_terminal {
                self.reopen_persisted_terminals(session_id, terminal_preferences, cx);
            }
            // Anything still queued that can go now, goes now.
            if self.dispatch_next_queued(session_id, cx).is_err() {
                self.report_error(RuntimeError::ProcessGone, cx);
            }
            if needs_restart {
                // Parked with a dead provider (its start failed while parked):
                // the queue survived, so try again now that someone is looking.
                self.ensure_started(session_id, cx);
            }
            self.refresh_git_status(session_id, cx);
            self.reschedule_scheduled_wake(cx);
            return;
        }

        log::info!(
            "opening session {} (resume cursor: {})",
            meta.id,
            meta.resume_cursor.is_some()
        );
        let session_id = meta.id.clone();
        let provider_commands = self.cached_provider_commands_for(&meta);
        let mut active = ActiveSession::new(meta, false, provider_commands);
        let terminal_preferences = self.terminal_preferences_for(&active);
        let restored_terminal = self.restore_terminal_workspace(&mut active);
        self.residents.live.insert(session_id.clone(), active);
        self.schedule_timeline_load(
            session_id.clone(),
            TimelineLoadTarget::Active { mark_idle: true },
            cx,
        );
        if !restored_terminal {
            self.reopen_persisted_terminals(&session_id, terminal_preferences, cx);
        }
        self.refresh_git_status(&session_id, cx);
    }
}

fn project_root_error(error: std::io::Error) -> ProtocolError {
    ProtocolError {
        code: match error.kind() {
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::AlreadyExists => {
                "invalid_project_root"
            }
            _ => "move_project_failed",
        }
        .into(),
        message: error.to_string(),
    }
}

fn project_directory_error(error: std::io::Error) -> ProtocolError {
    ProtocolError {
        code: if error.kind() == std::io::ErrorKind::InvalidInput {
            "invalid_project_root"
        } else {
            "create_project_failed"
        }
        .into(),
        message: error.to_string(),
    }
}

/// Images at `paths` as message attachments; an unreadable path is left out.
pub(super) fn read_attachments(paths: Vec<PathBuf>) -> Vec<Attachment> {
    paths
        .into_iter()
        .filter_map(|path| {
            let bytes = fs::read(&path).ok()?;
            Some(Attachment {
                media_type: mime_from_path(&path),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                source_path: Some(path.to_string_lossy().into_owned()),
            })
        })
        .collect()
}

/// Why a thread cannot be forked right now, if it cannot.
pub(super) fn fork_refusal(availability: ForkAvailability) -> Option<&'static str> {
    match availability {
        ForkAvailability::Unsupported => Some("This provider does not support conversation forks."),
        ForkAvailability::Empty => Some("This conversation is empty and cannot be forked."),
        ForkAvailability::Running => {
            Some("Wait for the running turn to finish before forking this conversation.")
        }
        ForkAvailability::Available => None,
    }
}

/// Whether `dir` is a directory inside `root` on this host, links resolved; `None` when it
/// is not a directory there.
pub(super) fn directory_within(root: &Path, dir: &Path) -> Option<bool> {
    let dir = fs::canonicalize(dir).ok().filter(|dir| dir.is_dir())?;
    let root = fs::canonicalize(root).ok()?;
    Some(dir.starts_with(root))
}
