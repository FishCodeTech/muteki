use super::*;
use tcode_core::settlement::{AgentDelivery, AgentExecution};
use tcode_protocol::AgentStatus;

pub(crate) struct DomainDiff {
    subscriptions: HashSet<Topic>,
    index: IndexSnapshot,
    summary_revisions: [u64; 7],
    space_indexes: HashMap<String, IndexSnapshot>,
    settings: Settings,
    providers: Option<ProvidersStatus>,
    git_status: HashMap<String, GitStatusStatus>,
    session_statuses: HashMap<String, SessionStatus>,
    session_plans: HashMap<String, SessionPlan>,
}

impl DomainDiff {
    pub(crate) fn new(state: &AppState) -> Self {
        Self {
            subscriptions: state.subscriptions.clone(),
            index: if state.subscriptions.contains(&Topic::Index) {
                state.index_snapshot()
            } else {
                IndexSnapshot {
                    summary: IndexSummary::default(),
                    sessions: Vec::new(),
                    projects: state.projects.clone(),
                }
            },
            summary_revisions: state.index_summary_revisions(),
            space_indexes: HashMap::new(),
            settings: state.settings_snapshot(),
            providers: state
                .subscriptions
                .contains(&Topic::Providers)
                .then(|| state.providers_status_snapshot()),
            git_status: HashMap::new(),
            session_statuses: state.subscribed_session_status_snapshots(),
            session_plans: state.subscribed_session_plan_snapshots(),
        }
    }

    pub(crate) fn emit_changes(&mut self, state: &mut AppState, cx: &mut HostCx) {
        if self.index.projects != state.projects {
            state.space_archives_revision = None;
        }
        state.refresh_space_archives();
        let settings_changed = self.settings != state.settings;
        let summary_revisions = state.index_summary_revisions();
        let sessions_changed = self.summary_revisions[0] != summary_revisions[0];
        let summary_changed = self.summary_revisions != summary_revisions || settings_changed;
        if state.subscriptions.contains(&Topic::Index) {
            if !self.subscriptions.contains(&Topic::Index) {
                self.index = state.index_snapshot();
            } else {
                if self.index.projects != state.projects
                    || (sessions_changed
                        && !self.index.sessions.iter().eq(state
                            .sessions
                            .iter()
                            .filter(|meta| meta.archived_at.is_none())))
                {
                    let index = state.index_snapshot();
                    for event in index_changes(&self.index, &index) {
                        emit_replacement(Topic::Index, event, cx);
                    }
                    self.index = index;
                } else if summary_changed {
                    let summary = state.build_index_summary();
                    for event in index_summary_changes(&self.index.summary, &summary) {
                        emit_replacement(Topic::Index, event, cx);
                    }
                    self.index.summary = summary;
                } else {
                    #[cfg(debug_assertions)]
                    assert_eq!(
                        self.index.summary,
                        state.build_index_summary(),
                        "index summary inputs changed without a revision"
                    );
                }
            }
        } else {
            if self.subscriptions.contains(&Topic::Index) {
                self.index.sessions = Vec::new();
                self.index.summary = IndexSummary::default();
            }
            if self.index.projects != state.projects {
                self.index.projects.clone_from(&state.projects);
            }
        }

        self.space_indexes.retain(|id, _| {
            state.subscriptions.contains(&Topic::SpaceIndex {
                space_id: id.clone(),
            })
        });
        for (id, projects) in &state.space_scopes {
            let topic = Topic::SpaceIndex {
                space_id: id.clone(),
            };
            if !state.subscriptions.contains(&topic) {
                continue;
            }
            let visible_projects: HashSet<_> = state
                .projects
                .iter()
                .filter(|p| projects.contains(&p.id))
                .map(|p| p.id.as_str())
                .collect();
            if let Some(old) = self.space_indexes.get_mut(id)
                && old
                    .projects
                    .iter()
                    .eq(state.projects.iter().filter(|p| projects.contains(&p.id)))
                && (!sessions_changed
                    || old.sessions.iter().eq(state.sessions.iter().filter(|meta| {
                        meta.archived_at.is_none()
                            && meta
                                .project_id
                                .as_deref()
                                .is_some_and(|id| visible_projects.contains(id))
                    })))
            {
                if summary_changed
                    || old.summary.archived_revision
                        != state
                            .space_archives
                            .get(id)
                            .map_or(0, |archive| archive.revision)
                {
                    let summary = state.space_index_summary(id, &old.projects);
                    for event in index_summary_changes(&old.summary, &summary) {
                        emit_replacement(topic.clone(), event, cx);
                    }
                    old.summary = summary;
                } else {
                    #[cfg(debug_assertions)]
                    assert_eq!(
                        old.summary,
                        state.space_index_summary(id, &old.projects),
                        "scoped index summary inputs changed without a revision"
                    );
                }
                continue;
            }
            let index = state.space_index_snapshot(id, projects);
            if let Some(old) = self.space_indexes.get(id) {
                for event in index_changes(old, &index) {
                    emit_replacement(topic.clone(), event, cx);
                }
            }
            self.space_indexes.insert(id.clone(), index);
        }
        if settings_changed {
            let settings = state.settings_snapshot();
            emit_replacement(
                Topic::Settings,
                settings_change(&self.settings, &settings),
                cx,
            );
            self.settings = settings;
        }

        if state.subscriptions.contains(&Topic::Providers) {
            let providers = state.providers_status_snapshot();
            if self.providers.as_ref() != Some(&providers) {
                if self.providers.is_some() {
                    emit_replacement(
                        Topic::Providers,
                        ServerEvent::ProvidersReplaced(providers.clone()),
                        cx,
                    );
                }
                self.providers = Some(providers);
            }
        } else {
            self.providers = None;
        }

        let git_status: HashMap<_, _> = state
            .subscriptions
            .iter()
            .filter_map(|topic| match topic {
                Topic::GitStatus { session_id } => {
                    Some((session_id.clone(), state.git_status_snapshot(session_id)))
                }
                _ => None,
            })
            .collect();
        for (id, status) in &git_status {
            if self.subscriptions.contains(&Topic::GitStatus {
                session_id: id.clone(),
            }) && self.git_status.get(id) != Some(status)
            {
                emit_replacement(
                    Topic::GitStatus {
                        session_id: id.clone(),
                    },
                    ServerEvent::GitStatusReplaced(status.clone()),
                    cx,
                );
            }
        }
        self.git_status = git_status;

        let session_statuses = state.subscribed_session_status_snapshots();
        let mut changed_ids: Vec<_> = session_statuses
            .iter()
            .filter_map(|(id, status)| {
                (self.subscriptions.contains(&Topic::SessionStatus {
                    session_id: id.clone(),
                }) && self.session_statuses.get(id) != Some(status))
                .then_some(id.as_str())
            })
            .collect();
        changed_ids.sort_unstable();
        for id in changed_ids {
            let status = &session_statuses[id];
            let usage_only = self.session_statuses.get_mut(id).is_some_and(|previous| {
                previous.usage = status.usage;
                previous.context_window = status.context_window;
                previous == status
            });
            let event = if usage_only {
                ServerEvent::SessionUsageChanged {
                    usage: status.usage,
                    context_window: status.context_window,
                }
            } else {
                ServerEvent::SessionStatusReplaced(Box::new(status.clone()))
            };
            emit_replacement(
                Topic::SessionStatus {
                    session_id: id.to_string(),
                },
                event,
                cx,
            );
        }
        self.session_statuses = session_statuses;
        let session_plans = state.subscribed_session_plan_snapshots();
        let mut changed_ids: Vec<_> = session_plans
            .iter()
            .filter_map(|(id, plan)| {
                (self.subscriptions.contains(&Topic::SessionPlan {
                    session_id: id.clone(),
                }) && self.session_plans.get(id) != Some(plan))
                .then_some(id.as_str())
            })
            .collect();
        changed_ids.sort_unstable();
        for id in changed_ids {
            emit_replacement(
                Topic::SessionPlan {
                    session_id: id.to_string(),
                },
                ServerEvent::SessionPlanReplaced(session_plans[id].clone()),
                cx,
            );
        }
        self.session_plans = session_plans;
        self.summary_revisions = summary_revisions;
        self.subscriptions.clone_from(&state.subscriptions);
    }
}

fn index_changes(old: &IndexSnapshot, new: &IndexSnapshot) -> Vec<ServerEvent> {
    let mut events = Vec::new();
    let old_projects: HashMap<_, _> = old.projects.iter().map(|p| (p.id.as_str(), p)).collect();
    let new_projects: HashSet<_> = new.projects.iter().map(|p| p.id.as_str()).collect();
    for project in &new.projects {
        if old_projects.get(project.id.as_str()) != Some(&project) {
            events.push(ServerEvent::IndexUpsertProject(project.clone()));
        }
    }
    let old_sessions: HashMap<_, _> = old.sessions.iter().map(|m| (m.id.as_str(), m)).collect();
    let new_sessions: HashSet<_> = new.sessions.iter().map(|m| m.id.as_str()).collect();
    for meta in &new.sessions {
        if old_sessions.get(meta.id.as_str()) != Some(&meta) {
            events.push(ServerEvent::IndexUpsertSession(meta.clone()));
        }
    }
    for meta in &old.sessions {
        if !new_sessions.contains(meta.id.as_str()) {
            events.push(ServerEvent::IndexRemoveSession {
                session_id: meta.id.clone(),
            });
        }
    }
    for project in &old.projects {
        if !new_projects.contains(project.id.as_str()) {
            events.push(ServerEvent::IndexRemoveProject {
                project_id: project.id.clone(),
            });
        }
    }
    events.extend(index_summary_changes(&old.summary, &new.summary));
    events
}

fn index_summary_changes(old: &IndexSummary, new: &IndexSummary) -> Vec<ServerEvent> {
    let mut events = Vec::new();
    if old != new {
        let IndexSummary {
            activity,
            title_generating,
            archived_counts,
            worktree_shared,
            archived_revision,
        } = new;
        if old.title_generating == *title_generating
            && old.archived_counts == *archived_counts
            && old.worktree_shared == *worktree_shared
            && old.archived_revision == *archived_revision
        {
            events.push(ServerEvent::IndexActivitiesChanged {
                upserts: activity
                    .iter()
                    .filter(|(id, value)| old.activity.get(*id) != Some(*value))
                    .map(|(id, value)| (id.clone(), value.clone()))
                    .collect(),
                removed: old
                    .activity
                    .keys()
                    .filter(|id| !activity.contains_key(*id))
                    .cloned()
                    .collect(),
            });
        } else {
            events.push(ServerEvent::IndexSummaryReplaced(new.clone()));
        }
    }
    events
}

/// Visiting a thread changes only its visit time, so that alone crosses the
/// wire rather than every setting.
fn settings_change(old: &Settings, new: &Settings) -> ServerEvent {
    let only_visits = Settings {
        last_visited: new.last_visited.clone(),
        ..old.clone()
    } == *new
        && old
            .last_visited
            .keys()
            .all(|id| new.last_visited.contains_key(id));
    if !only_visits {
        return ServerEvent::SettingsReplaced(new.clone());
    }
    ServerEvent::LastVisitedChanged(
        new.last_visited
            .iter()
            .filter(|(id, at)| old.last_visited.get(*id) != Some(at))
            .map(|(id, at)| (id.clone(), *at))
            .collect(),
    )
}

fn emit_replacement(topic: Topic, event: ServerEvent, cx: &mut HostCx) {
    cx.emit(HostEvent::Domain(EventEnvelope {
        request_id: None,
        topic,
        event,
    }));
}

// Borrowing the host state confines cached results to one immutable snapshot.
pub(super) struct ActivityProjection<'a> {
    state: &'a AppState,
    children: &'a HashMap<String, Vec<usize>>,
    holding: std::cell::RefCell<HashMap<String, bool>>,
}

impl<'a> ActivityProjection<'a> {
    pub(super) fn new(state: &'a AppState) -> Self {
        Self {
            state,
            children: state.sessions.children(),
            holding: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// Whether a child of this thread holds it: one still runs, or a
    /// dispatched one awaits its lead's settle.
    pub(super) fn children_hold(&self, parent_id: &str) -> bool {
        if let Some(holds) = self.holding.borrow().get(parent_id) {
            return *holds;
        }
        let holds = self.children.get(parent_id).is_some_and(|children| {
            children.iter().any(|index| {
                let child = &self.state.sessions[*index];
                match self.agent_status(child) {
                    Some(status) => status.delivery.holds_parent(),
                    None => {
                        self.state.resident(&child.id).is_some_and(|session| {
                            session.is_unfinished() || session.timeline.turn_running
                        }) || self.children_hold(&child.id)
                    }
                }
            })
        });
        self.holding
            .borrow_mut()
            .insert(parent_id.to_owned(), holds);
        holds
    }

    /// A dispatched orchestrate child's execution and delivery; `None` for
    /// every other thread.
    pub(super) fn agent_status(&self, meta: &SessionMeta) -> Option<AgentStatus> {
        if !meta.is_dispatched() {
            return None;
        }
        let resident = self.state.resident(&meta.id);
        let activity = self.state.thread_activity.get(&meta.id);
        let asking = resident.is_some_and(|session| {
            let (approvals, input) = self.state.open_requests(&meta.id, session);
            !approvals.is_empty() || input.is_some()
        });
        let execution = if asking {
            AgentExecution::Waiting
        } else if resident.is_some_and(|session| {
            session.turn_in_flight
                || session.delivery_in_flight.is_some()
                || !session.queue.is_empty()
                || session.preparing_worktree
                || matches!(session.runtime, Runtime::Starting { .. })
        }) {
            AgentExecution::Working
        } else if resident.is_some_and(|session| session.background_task_count > 0)
            || self.children_hold(&meta.id)
        {
            AgentExecution::Waiting
        } else if meta.cancelled_at.is_some() {
            AgentExecution::Cancelled
        } else if activity.is_some_and(|activity| activity.failed) {
            AgentExecution::Failed
        } else if activity.is_some_and(|activity| activity.interrupted) {
            AgentExecution::Interrupted
        } else {
            AgentExecution::Finished
        };
        Some(AgentStatus {
            execution,
            delivery: AgentDelivery::of(meta, execution),
            report: meta.report(),
            run_started_at: activity.and_then(|activity| activity.last_run_started_at),
            run_completed_at: activity.and_then(|activity| activity.last_run_completed_at),
        })
    }

    pub(super) fn session_activity(
        &self,
        meta: &SessionMeta,
        resident: Option<&ActiveSession>,
    ) -> SessionActivity {
        let (approvals, input) = resident.map_or((&[][..], None), |session| {
            self.state.open_requests(&meta.id, session)
        });
        SessionActivity {
            working: resident.is_some_and(|session| {
                session.preparing_worktree
                    || matches!(session.runtime, Runtime::Starting { .. })
                    || session.turn_in_flight
                    || session.delivery_in_flight.is_some()
                    || (meta.native_subagent.is_some() && session.timeline.turn_running)
                    || session
                        .queue
                        .iter()
                        .any(|message| message.origin == MessageOrigin::Human)
            }),
            turn_running: resident.is_some_and(|session| {
                session.turn_in_flight && session.timeline.turn_retry.is_none()
            }),
            waiting: resident.is_some_and(|session| session.background_task_count > 0)
                || self.children_hold(&meta.id)
                || (!meta.pull_request_watches_paused
                    && tcode_core::pull_request::watched(&meta.pull_requests)
                        .next()
                        .is_some()),
            waiting_for_approval: !approvals.is_empty(),
            waiting_for_input: input.is_some(),
            failed: self
                .state
                .thread_activity
                .get(&meta.id)
                .is_some_and(|activity| activity.failed),
            unread: self.state.session_unread(meta),
            fork: AppState::session_fork_availability(meta, resident),
            agent: self.agent_status(meta),
        }
    }
}

impl AppState {
    fn index_summary_revisions(&self) -> [u64; 7] {
        [
            self.sessions.revision(),
            self.residents.live.revision(),
            self.residents.parked.revision(),
            self.thread_activity.revision(),
            self.title_generating.revision(),
            self.approvals.revision(),
            self.archived_revision,
        ]
    }

    pub fn index_snapshot(&self) -> IndexSnapshot {
        self.index_snapshot_with_summary(self.build_index_summary())
    }

    fn index_snapshot_with_summary(&self, summary: IndexSummary) -> IndexSnapshot {
        IndexSnapshot {
            summary,
            sessions: self
                .sessions
                .iter()
                .filter(|meta| meta.archived_at.is_none())
                .cloned()
                .collect(),
            projects: self.projects.clone(),
        }
    }

    fn build_index_summary(&self) -> IndexSummary {
        let activity = ActivityProjection::new(self);
        let mut summary = IndexSummary {
            title_generating: self.title_generating.clone(),
            archived_revision: self.archived_revision,
            ..IndexSummary::default()
        };
        let sharing = std::cell::OnceCell::new();
        for meta in &self.sessions {
            if meta.archived_at.is_none() {
                let resident = self.resident(&meta.id);
                summary.activity.insert(
                    meta.id.clone(),
                    activity
                        .session_activity(resident.map_or(meta, |session| &session.meta), resident),
                );
                if meta.worktree.is_some()
                    && sharing
                        .get_or_init(|| self.worktree_sharing())
                        .is_shared(meta)
                {
                    summary.worktree_shared.insert(meta.id.clone());
                }
                continue;
            }
            if let Some(project_id) = &meta.project_id {
                *summary
                    .archived_counts
                    .entry(project_id.clone())
                    .or_default() += 1;
            }
        }
        for id in self.residents.ids() {
            let Some(session) = self.resident(id) else {
                continue;
            };
            if session.meta.archived_at.is_none() && !summary.activity.contains_key(id) {
                summary.activity.insert(
                    id.to_owned(),
                    activity.session_activity(&session.meta, Some(session)),
                );
                if session.meta.worktree.is_some()
                    && sharing
                        .get_or_init(|| self.worktree_sharing())
                        .is_shared(&session.meta)
                {
                    summary.worktree_shared.insert(id.to_owned());
                }
            }
        }
        summary
    }

    fn space_projects(&self, ids: &BTreeSet<String>) -> Vec<Project> {
        self.projects
            .iter()
            .filter(|p| ids.contains(&p.id))
            .cloned()
            .collect()
    }

    fn space_index_snapshot(&self, space_id: &str, ids: &BTreeSet<String>) -> IndexSnapshot {
        let projects = self.space_projects(ids);
        let summary = self.space_index_summary(space_id, &projects);
        let ids: HashSet<_> = projects.iter().map(|p| p.id.as_str()).collect();
        let sessions = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.archived_at.is_none()
                    && meta
                        .project_id
                        .as_deref()
                        .is_some_and(|id| ids.contains(id))
            })
            .cloned()
            .collect();
        IndexSnapshot {
            projects,
            sessions,
            summary,
        }
    }

    fn space_index_summary(&self, space_id: &str, projects: &[Project]) -> IndexSummary {
        let activity = ActivityProjection::new(self);
        let ids: HashSet<_> = projects.iter().map(|p| p.id.as_str()).collect();
        let in_scope = |meta: &&SessionMeta| {
            meta.project_id
                .as_deref()
                .is_some_and(|id| ids.contains(id))
        };
        let metas: Vec<_> = self.sessions.iter().filter(in_scope).collect();
        let residents: Vec<_> = self
            .residents
            .ids()
            .filter_map(|id| self.resident(id))
            .filter(|s| in_scope(&&s.meta))
            .collect();
        let sharing = std::cell::OnceCell::new();
        let mut summary = IndexSummary::default();
        for meta in &metas {
            if meta.archived_at.is_some() {
                *summary
                    .archived_counts
                    .entry(meta.project_id.clone().expect("scoped project"))
                    .or_default() += 1;
            }
        }
        for meta in metas
            .iter()
            .copied()
            .chain(residents.iter().map(|s| &s.meta))
        {
            if meta.archived_at.is_some() {
                continue;
            }
            let resident = self.resident(&meta.id);
            summary.activity.insert(
                meta.id.clone(),
                activity.session_activity(resident.map_or(meta, |s| &s.meta), resident),
            );
            if self.title_generating.contains(&meta.id) {
                summary.title_generating.insert(meta.id.clone());
            }
            if meta.worktree.is_some()
                && sharing
                    .get_or_init(|| {
                        WorktreeSharing::new(
                            metas
                                .iter()
                                .copied()
                                .chain(residents.iter().map(|s| &s.meta)),
                        )
                    })
                    .is_shared(meta)
            {
                summary.worktree_shared.insert(meta.id.clone());
            }
        }
        summary.archived_revision = self
            .space_archives
            .get(space_id)
            .map_or(0, |archive| archive.revision);
        summary
    }

    pub(crate) fn refresh_space_archives(&mut self) {
        if self.space_archives_revision == Some(self.archived_revision) {
            return;
        }
        self.space_archives_revision = Some(self.archived_revision);
        for (id, projects) in &self.space_scopes {
            let mut archive = self.space_archived_snapshot(projects);
            if let Some(old) = self.space_archives.get(id) {
                archive.revision = old.revision
                    + u64::from(
                        old.sessions != archive.sessions
                            || old.worktree_shared != archive.worktree_shared,
                    );
            } else {
                archive.revision = self
                    .space_archive_revisions
                    .get(id)
                    .map_or(0, |revision| revision + 1);
            }
            self.space_archives.insert(id.clone(), archive);
        }
    }

    pub(crate) fn scoped_archived_sessions(
        &self,
        principal: &tcode_protocol::Principal,
    ) -> ArchivedSessions {
        match principal {
            tcode_protocol::Principal::Full => self.archived_sessions(),
            tcode_protocol::Principal::Space {
                space_id,
                project_ids,
                ..
            } => self
                .space_archives
                .get(space_id)
                .cloned()
                .unwrap_or_else(|| {
                    self.space_archived_snapshot(&project_ids.iter().cloned().collect())
                }),
        }
    }

    fn space_archived_snapshot(&self, project_ids: &BTreeSet<String>) -> ArchivedSessions {
        let projects = self.space_projects(project_ids);
        let ids: HashSet<_> = projects.iter().map(|p| p.id.as_str()).collect();
        let metas: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.project_id
                    .as_deref()
                    .is_some_and(|id| ids.contains(id))
            })
            .collect();
        let residents = self
            .residents
            .ids()
            .filter_map(|id| self.resident(id))
            .filter(|s| {
                s.meta
                    .project_id
                    .as_deref()
                    .is_some_and(|id| ids.contains(id))
            });
        let sharing = WorktreeSharing::new(metas.iter().copied().chain(residents.map(|s| &s.meta)));
        let mut sessions: Vec<_> = metas
            .into_iter()
            .filter(|m| m.archived_at.is_some())
            .cloned()
            .collect();
        sessions.sort_by(|a, b| {
            b.archived_at
                .cmp(&a.archived_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let worktree_shared = sessions
            .iter()
            .filter(|m| sharing.is_shared(m))
            .map(|m| m.id.clone())
            .collect();
        ArchivedSessions {
            sessions,
            worktree_shared,
            revision: 0,
        }
    }

    fn scoped_provider_choices(&self) -> Vec<tcode_protocol::ScopedProviderChoice> {
        ProviderKind::NATIVE
            .into_iter()
            .chain([ProviderKind::Acp])
            .flat_map(|kind| {
                self.settings
                    .profiles_for_kind(kind)
                    .into_iter()
                    .filter(|profile| profile.settings.enabled)
                    .map(move |profile| {
                        let mut models: Vec<_> = self
                            .models_for(kind)
                            .iter()
                            .filter(|m| !profile.settings.hidden_models.contains(&m.id))
                            .cloned()
                            .collect();
                        for id in &profile.settings.custom_models {
                            if !models.iter().any(|m| &m.id == id)
                                && !profile.settings.hidden_models.contains(id)
                            {
                                models.push(ModelSpec {
                                    id: id.clone(),
                                    display_name: id.clone(),
                                    is_default: false,
                                    options: Vec::new(),
                                });
                            }
                        }
                        tcode_protocol::ScopedProviderChoice {
                            provider: kind,
                            name: self.settings.profile_display_name(&profile.id),
                            profile_id: Some(profile.id),
                            models,
                        }
                    })
            })
            .collect()
    }

    fn scope_snapshot(&self, principal: &tcode_protocol::Principal) -> tcode_protocol::Scope {
        match principal {
            tcode_protocol::Principal::Full => tcode_protocol::Scope::Full,
            tcode_protocol::Principal::Space {
                space_id,
                space_name,
                project_ids,
                ..
            } => tcode_protocol::Scope::Space {
                space_id: space_id.clone(),
                space_name: space_name.clone(),
                projects: self.space_projects(&project_ids.iter().cloned().collect()),
                providers: self.scoped_provider_choices(),
            },
        }
    }

    pub fn archived_sessions(&self) -> ArchivedSessions {
        let sharing = self.worktree_sharing();
        let mut sessions: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| meta.archived_at.is_some())
            .cloned()
            .collect();
        sessions.sort_by_key(|meta| std::cmp::Reverse(meta.archived_at));
        let worktree_shared = sessions
            .iter()
            .filter(|meta| sharing.is_shared(meta))
            .map(|meta| meta.id.clone())
            .collect();
        ArchivedSessions {
            sessions,
            worktree_shared,
            revision: self.archived_revision,
        }
    }

    pub(super) fn archived_sharing_affected(&self, other: &SessionMeta) -> bool {
        self.sessions
            .iter()
            .any(|meta| meta.archived_at.is_some() && meta.shares_worktree_with(other))
    }

    pub(super) fn worktree_sharing(&self) -> WorktreeSharing<'_> {
        WorktreeSharing::new(
            self.sessions
                .iter()
                .chain(self.residents.live.values().map(|session| &session.meta)),
        )
    }

    pub fn settings_snapshot(&self) -> Settings {
        self.settings.clone()
    }

    pub fn providers_status_snapshot(&self) -> ProvidersStatus {
        self.providers.status_snapshot(
            self.acp_marketplace_items(),
            self.acp_registry_loading,
            self.acp_registry_error.clone(),
            self.acp_installing.clone(),
            self.provider_plugin_catalogs(),
        )
    }

    pub fn git_status_snapshot(&self, session_id: &str) -> GitStatusStatus {
        GitStatusStatus {
            status: self.git_status.get(session_id).cloned(),
            busy: self.git_busy.contains(session_id),
        }
    }

    /// Reconcile the deliberate local live-terminal handle registry after one
    /// host mailbox turn. Only opaque `Arc<Terminal>` values cross this path;
    /// layout and context data are emitted in `SessionStatus`.
    pub(crate) fn sync_terminal_handles(&self) {
        self.terminal_registry.replace_from(
            self.residents
                .live
                .values()
                .map(|session| &session.terminal_workspace)
                .chain(
                    self.residents
                        .parked
                        .values()
                        .map(|session| &session.terminal_workspace),
                )
                .chain(self.terminal_workspaces.values()),
        );
    }

    pub(crate) fn subscription_snapshot(
        &mut self,
        subscription: &tcode_protocol::Subscription,
        principal: &tcode_protocol::Principal,
    ) -> Option<EventEnvelope> {
        let topic = &subscription.topic;
        let event = match topic {
            Topic::Scope => ServerEvent::ScopeSnapshot(self.scope_snapshot(principal)),
            Topic::SpaceIndex { space_id } => {
                ServerEvent::IndexSnapshot(self.space_index_snapshot(
                    space_id,
                    self.space_scopes.get(space_id).unwrap_or(&BTreeSet::new()),
                ))
            }
            Topic::Index => ServerEvent::IndexSnapshot(self.index_snapshot()),
            Topic::Settings => ServerEvent::SettingsSnapshot(self.settings_snapshot()),
            Topic::Providers => ServerEvent::ProvidersReplaced(self.providers_status_snapshot()),
            Topic::GitStatus { session_id } => {
                ServerEvent::GitStatusReplaced(self.git_status_snapshot(session_id))
            }
            Topic::SessionStatus { session_id } => ServerEvent::SessionStatusReplaced(Box::new(
                self.session_status_snapshot(session_id)?,
            )),
            Topic::SessionPlan { session_id } => {
                ServerEvent::SessionPlanReplaced(self.session_plan_snapshot(session_id)?)
            }
            // Answered from the session's log by `reply_to_subscription`.
            Topic::SessionEvents { .. } => return None,
            Topic::CommandOutput { .. } => return None,
            Topic::RuntimeEvents => return None,
            Topic::Preview => return None,
            // Retained latest-run status, so a client that subscribes after a
            // fast completion still recovers the outcome. `None` means no run
            // has ever started for this project.
            Topic::ExternalImport { project_id } => ServerEvent::ExternalImportStatusReplaced {
                project_id: project_id.clone(),
                status: self.external_imports.get(project_id).cloned(),
            },
            Topic::Terminal { terminal_id } => ServerEvent::TerminalFrame {
                terminal_id: *terminal_id,
                frame: Box::new(self.terminal_frame(*terminal_id)?),
            },
        };
        let event = match (subscription.known_index.as_ref(), event) {
            (Some(known), ServerEvent::IndexSnapshot(snapshot))
                if tcode_protocol::index_fingerprint(
                    &snapshot.summary,
                    &snapshot.sessions,
                    &snapshot.projects,
                )
                .ok()
                .as_ref()
                    == Some(known) =>
            {
                ServerEvent::IndexUnchanged {
                    fingerprint: known.clone(),
                    archived_revision: snapshot.summary.archived_revision,
                }
            }
            (_, event) => event,
        };
        Some(EventEnvelope {
            request_id: None,
            topic: topic.clone(),
            event,
        })
    }

    /// The requests a session waits on. A provider shut down without a closing
    /// record leaves them open in the timeline; only an in-flight turn waits.
    fn open_requests<'a>(
        &'a self,
        session_id: &str,
        session: &'a ActiveSession,
    ) -> (
        &'a [agent::ApprovalRequest],
        Option<&'a tcode_core::session::PendingUserInput>,
    ) {
        // Not the turn: a background subagent asks between its parent's turns.
        if !matches!(session.runtime, Runtime::Live(_)) {
            return (&[], None);
        }
        (
            self.approval_requests(session_id),
            session.timeline.pending_user_input.as_ref(),
        )
    }

    pub(super) fn agent_status(&self, meta: &SessionMeta) -> Option<AgentStatus> {
        ActivityProjection::new(self).agent_status(meta)
    }

    pub fn session_plan_snapshot(&self, session_id: &str) -> Option<SessionPlan> {
        let timeline = &self.resident(session_id)?.timeline;
        Some(SessionPlan {
            session_id: session_id.to_string(),
            steps: timeline.plan_steps.clone(),
        })
    }

    fn subscribed_session_plan_snapshots(&self) -> HashMap<String, SessionPlan> {
        self.subscriptions
            .iter()
            .filter_map(|topic| match topic {
                Topic::SessionPlan { session_id } => {
                    Some((session_id.clone(), self.session_plan_snapshot(session_id)?))
                }
                _ => None,
            })
            .collect()
    }

    pub fn session_status_snapshot(&self, session_id: &str) -> Option<SessionStatus> {
        self.session_status_with_activity(session_id, &ActivityProjection::new(self))
    }

    /// The active model's option descriptors: pushed over the wire by the
    /// provider, or read from the provider's model catalog.
    pub(super) fn model_option_descriptors(
        &self,
        session: &ActiveSession,
    ) -> Vec<OptionDescriptor> {
        let meta = &session.meta;
        if matches!(
            meta.provider.caps().option_descriptors,
            OptionDescriptors::Wire
        ) {
            return session.provider_options.clone();
        }
        meta.model
            .as_deref()
            .and_then(|model| {
                self.models_for(meta.provider)
                    .iter()
                    .find(|spec| spec.id == model)
            })
            .map(|spec| spec.options.clone())
            .unwrap_or_default()
    }

    /// The window a session's model runs at with no `contextWindow` selection.
    pub(super) fn default_context_window(&self, session_id: &str) -> Option<u64> {
        let session = self.resident(session_id)?;
        agent::context_window::selected(&self.model_option_descriptors(session), &[])
    }

    fn session_status_with_activity(
        &self,
        session_id: &str,
        activity: &ActivityProjection<'_>,
    ) -> Option<SessionStatus> {
        let session = self.resident(session_id)?;
        let meta = &session.meta;
        let mut provider_option_descriptors = self.model_option_descriptors(session);
        if let Some(descriptor) = permission_control(meta.provider) {
            provider_option_descriptors.push(descriptor);
        }
        for descriptor in &session.provider_options {
            let id = match descriptor {
                OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => id,
            };
            provider_option_descriptors.retain(|existing| match existing {
                OptionDescriptor::Select {
                    id: existing_id, ..
                }
                | OptionDescriptor::Boolean {
                    id: existing_id, ..
                } => existing_id != id,
            });
            provider_option_descriptors.push(descriptor.clone());
        }
        let mut provider_option_selections = meta.option_selections.clone();
        if !matches!(session.runtime, Runtime::Idle)
            && let Some(descriptor) = session.permission_descriptor()
        {
            let id = match descriptor {
                OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => id,
            };
            provider_option_selections.retain(|selection| selection.id != id);
            provider_option_selections.extend(
                session
                    .confirmed_option_selections
                    .iter()
                    .filter(|selection| selection.id == id)
                    .cloned(),
            );
        }
        let relay_confirmation = session.pending_relay.as_ref().and_then(|pending| {
            has_meaningful_history(&session.timeline).then(|| {
                (
                    self.provider_label(pending.from_provider, pending.from_profile.as_deref()),
                    self.provider_label(meta.provider, meta.profile_id.as_deref()),
                )
            })
        });
        let terminal_preferences = self.terminal_preferences_for(session);
        let (approvals, user_input) = self.open_requests(session_id, session);
        // The provider reports the window it was launched with; a selection
        // made since then caps the meter until the restart that applies it.
        let selected_window = agent::context_window::selected(
            &provider_option_descriptors,
            &provider_option_selections,
        );
        let context_window = session
            .timeline
            .usage
            .and_then(|usage| usage.context_window)
            .map(|reported| selected_window.map_or(reported, |selected| reported.min(selected)));
        Some(SessionStatus {
            session_id: session_id.to_string(),
            title: meta.title.clone(),
            cwd: meta.cwd.clone(),
            attachments_dir: self.attachments_dir_for(session_id),
            provider: meta.provider,
            requested_model: meta.model.clone(),
            requested_profile_id: meta.profile_id.clone(),
            acp_agent_id: meta.acp_agent_id.clone(),
            project_id: meta.project_id.clone(),
            queued_messages: session
                .queue
                .iter()
                .map(|message| QueuedMessageStatus {
                    delivery_key: message.delivery_key.clone(),
                    id: message.id,
                    editable: Self::queued_message_editable(session, message.id),
                    text: message.text.clone(),
                    fire_at_unix_secs: message.not_before.and_then(|time| {
                        time.duration_since(UNIX_EPOCH)
                            .ok()
                            .map(|duration| duration.as_secs())
                    }),
                })
                .collect(),
            review_comment_drafts: self
                .review_comment_drafts
                .get(session_id)
                .cloned()
                .unwrap_or_default(),
            terminals: session
                .terminal_workspace
                .terminals
                .iter()
                .map(|terminal| TerminalStatus {
                    id: terminal.id,
                    title: terminal.terminal.label(),
                    exited: terminal.terminal.exited(),
                })
                .collect(),
            active_terminal_id: session.terminal_workspace.active_id,
            terminal_splits: session.terminal_workspace.splits.clone(),
            terminal_contexts: session.terminal_workspace.contexts.clone(),
            terminal_open: terminal_preferences.is_some_and(|preferences| preferences.open),
            terminal_height: terminal_preferences
                .map(|preferences| preferences.height.clamp(120., 600.))
                .unwrap_or(240.),
            delivery_in_flight: session.delivery_in_flight,
            activity: activity.session_activity(meta, Some(session)),
            stopping: session.interrupt_requested,
            turn_retry: matches!(session.runtime, Runtime::Live(_))
                .then(|| session.timeline.turn_retry.clone())
                .flatten(),
            goal: session.timeline.goal.clone(),
            native_rewind_blocked: self.native_rewind_blocked(session),
            checkout_blocked: Self::checkout_blocked(session),
            conversation_read_only: Self::conversation_read_only(meta),
            terminal_limit_reached: self.terminal_limit_reached(session),
            terminal_split_available: self.terminal_split_available(session),
            usage: session.timeline.usage,
            context_window,
            // A provider shut down without a closing record leaves its turn
            // running in the timeline; only an in-flight turn is live.
            running_turn: session
                .turn_in_flight
                .then(|| session.timeline.running_turn())
                .flatten(),
            pending_approvals: approvals.to_vec(),
            pending_user_input: user_input.cloned(),
            steering_supported: session.can_steer(),
            provider_option_descriptors,
            provider_option_selections,
            provider_option_requested_selections: meta.option_selections.clone(),
            provider_commands: session.provider_commands.clone(),
            git_branch: session.git_branch.clone(),
            branches: session.branches.clone(),
            draft: session.draft,
            draft_workspace: session.draft_workspace.clone(),
            worktree: meta.worktree.clone(),
            preparing_worktree: session.preparing_worktree,
            relay_confirmation,
            native_rewind_pending: self.pending_native_rewinds.contains_key(session_id),
            // The one-shot value is transferred to the client as a dedicated
            // serialized event; availability is client-replica state after
            // that point, not a host-side consuming read.
            native_rewind_prefill_available: false,
            model_pending_restart: session.model_changed_while_live(),
            options_pending_restart: session.options_changed_while_live(),
            injected_tools: self.injected_tools(session),
            control: self.control_status(meta),
        })
    }

    fn subscribed_session_status_snapshots(&self) -> HashMap<String, SessionStatus> {
        let mut activity = None;
        self.subscriptions
            .iter()
            .filter_map(|topic| {
                let Topic::SessionStatus { session_id } = topic else {
                    return None;
                };
                let activity = activity.get_or_insert_with(|| ActivityProjection::new(self));
                Some((
                    session_id.clone(),
                    self.session_status_with_activity(session_id, activity)?,
                ))
            })
            .collect()
    }

    pub(super) fn upsert_session_in_memory(&mut self, meta: SessionMeta) {
        let existing = self.sessions.iter().find(|existing| existing.id == meta.id);
        let archived_metadata_changed = existing != Some(&meta)
            && (meta.archived_at.is_some()
                || existing.is_some_and(|old| old.archived_at.is_some()));
        let sharing_may_change = existing
            .is_none_or(|old| old.cwd != meta.cwd || old.worktree != meta.worktree)
            && (self.archived_sharing_affected(&meta)
                || existing.is_some_and(|old| self.archived_sharing_affected(old)));
        if archived_metadata_changed || sharing_may_change {
            self.archived_revision += 1;
        }
        match self
            .sessions
            .iter_mut()
            .find(|existing| existing.id == meta.id)
        {
            Some(existing) => *existing = meta,
            None => self.sessions.push(meta),
        }
        self.sessions
            .sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
    }

    /// Enqueue a FIFO barrier used by the application quit hook. The returned
    /// receiver resolves only after every earlier store write has committed,
    /// with the store failure that lost one if any did.
    pub fn store_write_barrier(
        &mut self,
        cx: &mut HostCx,
    ) -> smol::channel::Receiver<Result<(), String>> {
        let (completion, barrier) = smol::channel::bounded(1);
        self.enqueue_store_write(StoreWrite::Flush(completion), cx);
        barrier
    }
}
