//! What can become of a pull request: the primary action, the ⋯ menu's lifecycle group, the
//! confirmations, the merge dialog and the words each answer gets. The host reads the pull
//! request fresh before a merge or a branch update and answers every write; nothing here
//! retries one.

use std::rc::Rc;

use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use serde::Deserialize;
use tcode_core::pull_request::{
    ChecksState, Mergeability, PullRequestKey, PullRequestMergeMethod, PullRequestStackRoute,
    PullRequestState, ThreadPullRequestLink, stack_route,
};
use tcode_protocol::{
    Command, PullRequestAction, PullRequestActionResult, PullRequestActionState,
    PullRequestMergeState, PullRequestRead, PullRequestReadResponse, PullRequestRejection,
};

use super::compose::{answer_of, rejection_reason};
use crate::{
    icon::{Icon, IconName},
    material,
    overlay::{DialogButtons, Notification, OverlayExt as _},
    sizing::Sizable as _,
    store::WorkspaceStore,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{
        button::{Button, ButtonVariant, ButtonVariants as _},
        checkbox::Checkbox,
        choice::{Choice, ChoiceGroup},
        menu::{OpenUrl, PopupMenu},
        spinner::Spinner,
    },
    window_state::{Destination, WindowState},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(super) enum Lifecycle {
    Ready,
    Draft,
    Close,
    Reopen,
    Revert,
    UpdateBranch,
    UpdateRebase,
    Merge,
    EnableAutoMerge,
    DisableAutoMerge,
    AskConflicts,
    AskChecks,
    MergeStack,
    RebaseStack,
}

/// A lifecycle item of a pull request's menu or primary action.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = tcode_pull_requests, no_json)]
pub(super) struct RunLifecycle {
    pub(super) key: PullRequestKey,
    pub(super) kind: Lifecycle,
}

/// The method this client merges a pull request with, from the menu's checks.
#[derive(Action, Clone, PartialEq, Deserialize)]
#[action(namespace = tcode_pull_requests, no_json)]
pub(super) struct ChooseMergeMethod {
    pub(super) key: PullRequestKey,
    pub(super) method: PullRequestMergeMethod,
}

/// The one action a pull request's header offers first, or a status in its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Primary {
    ResolveConflicts,
    Ready,
    FixChecks,
    AutoMergeOn(PullRequestMergeMethod),
    Queued(Option<u32>),
    /// The stack's merge through this layer, disabled while a layer below blocks it.
    MergeStack,
    /// The stack's running or unconfirmed write, in place of a merge.
    StackOperation,
    UpdateBranch,
    EnableAutoMerge,
    Merge {
        method: PullRequestMergeMethod,
        queue: bool,
    },
}

/// What a pull request may meet, as the thread's link and the host's action state say.
#[derive(Clone)]
pub(super) struct Offer {
    pub(super) key: PullRequestKey,
    pub(super) url: String,
    pub(super) state: PullRequestState,
    pub(super) draft: bool,
    pub(super) conflicting: bool,
    pub(super) checks_failing: bool,
    pub(super) checks_pending: bool,
    pub(super) route: PullRequestStackRoute,
    /// The host's action state; a list row has none until one of its items is chosen.
    pub(super) action: Option<PullRequestActionState>,
    /// The method a merge would use.
    pub(super) method: Option<PullRequestMergeMethod>,
    /// The native stack it is a layer of.
    pub(super) stack: Option<super::stack::StackOffer>,
    /// A write is in flight, or one went unanswered and GitHub has not been read since.
    pub(super) busy: bool,
}

impl Offer {
    /// `None` until the link or its stack says what state the pull request is in. The host's
    /// action state, read fresh, speaks over the link's snapshot where it has a word.
    pub(super) fn new(
        key: &PullRequestKey,
        links: &[ThreadPullRequestLink],
        operations: &[tcode_core::pull_request::PullRequestStackOperation],
        action: Option<PullRequestActionState>,
        chosen: Option<PullRequestMergeMethod>,
        project_default: Option<PullRequestMergeMethod>,
        busy: bool,
    ) -> Option<Self> {
        let link = links.iter().find(|link| link.key == *key && link.visible());
        let snapshot = link.and_then(|link| link.snapshot.as_ref());
        let (route, layer) = stack_route(links, key);
        let state = snapshot
            .map(|snapshot| snapshot.state)
            .or(layer.map(|layer| layer.state))?;
        let url = link
            .map(|link| link.url.clone())
            .filter(|url| !url.is_empty())
            .or(layer.map(|layer| layer.url.clone()))
            .unwrap_or_default();
        let method = action
            .as_ref()
            .and_then(|action| merge_method(&action.merge_methods, chosen, project_default));
        let conflicting = match &action {
            Some(action) if action.merge_state != PullRequestMergeState::Unknown => {
                action.merge_state == PullRequestMergeState::Dirty
            }
            _ => snapshot.is_some_and(|s| s.mergeability == Mergeability::Conflicting),
        };
        let checks = |checks: ChecksState| snapshot.is_some_and(|s| s.checks_state == Some(checks));
        Some(Self {
            key: key.clone(),
            url,
            state,
            draft: snapshot.is_some_and(|snapshot| snapshot.is_draft),
            conflicting,
            checks_failing: match &action {
                Some(action) => !action.failing_checks.is_empty(),
                None => checks(ChecksState::Failing),
            },
            checks_pending: match &action {
                Some(action) => action.pending_checks > 0,
                None => checks(ChecksState::Pending),
            },
            route,
            action,
            method,
            stack: super::stack::StackOffer::new(key, links, operations),
            busy,
        })
    }

    /// The rank: conflicts, then ready for review, then failing checks, then what is already
    /// under way, then the merge itself. A stack whose state is unknown offers no merge.
    pub(super) fn primary(&self) -> Option<Primary> {
        let action = self.action.as_ref()?;
        if self.state != PullRequestState::Open {
            return None;
        }
        if self.conflicting {
            return Some(Primary::ResolveConflicts);
        }
        if self.draft {
            return (action.can_update && action.capabilities.draft).then_some(Primary::Ready);
        }
        if self.checks_failing {
            return Some(Primary::FixChecks);
        }
        if let Some(method) = action.auto_merge {
            return Some(Primary::AutoMergeOn(method));
        }
        if action.queued {
            return Some(Primary::Queued(action.queue_position));
        }
        match self.route {
            PullRequestStackRoute::Layer { .. } => {
                let stack = self.stack.as_ref()?;
                if stack.operation_here().is_some() {
                    return Some(Primary::StackOperation);
                }
                return match stack.merge(Some(action)) {
                    super::stack::Avail::Enabled => Some(Primary::MergeStack),
                    super::stack::Avail::Disabled(_) if !stack.map().blockers().is_empty() => {
                        Some(Primary::MergeStack)
                    }
                    _ => None,
                };
            }
            PullRequestStackRoute::Unknown => return None,
            PullRequestStackRoute::Single => {}
        }
        if action.merge_state == PullRequestMergeState::Behind
            && action.can_update_branch
            && action.capabilities.update_branch
        {
            return Some(Primary::UpdateBranch);
        }
        if !action.can_merge {
            return None;
        }
        if self.checks_pending
            && action.auto_merge_allowed
            && action.capabilities.auto_merge
            && !action.merge_queue
        {
            return Some(Primary::EnableAutoMerge);
        }
        let mergeable = action.merge_queue
            || matches!(
                action.merge_state,
                PullRequestMergeState::Clean
                    | PullRequestMergeState::Unstable
                    | PullRequestMergeState::HasHooks
            );
        Some(Primary::Merge {
            method: self.method.filter(|_| mergeable)?,
            queue: action.merge_queue,
        })
    }

    fn item(&self, kind: Lifecycle) -> Box<dyn Action> {
        Box::new(RunLifecycle {
            key: self.key.clone(),
            kind,
        })
    }

    /// The menu's lifecycle group, added once to the row menu every pull request menu builds.
    /// Its writes wait while one is in flight or unanswered.
    pub(super) fn menu(&self, menu: PopupMenu) -> PopupMenu {
        let action = self.action.as_ref();
        let may = |allowed: fn(&PullRequestActionState) -> bool| action.is_none_or(allowed);
        let primary = self.primary();
        let open = self.state == PullRequestState::Open;
        let number = self.key.number.to_string();
        // A layer the stack's write is moving waits for it to end, as for a write of its own.
        let free = !self.busy
            && self
                .stack
                .as_ref()
                .is_none_or(|stack| stack.operation_here().is_none());
        let write = |menu: PopupMenu, label: &str, icon: IconName, kind: Lifecycle| {
            menu.menu_with_icon_and_enable(
                crate::tr!(label).into_owned(),
                icon,
                self.item(kind),
                free,
            )
        };
        let mut menu = menu.separator();
        if let (PullRequestStackRoute::Layer { .. }, Some(stack)) = (self.route, &self.stack) {
            menu = self.stack_group(stack, menu);
        }
        if open && may(|action| action.can_update && action.capabilities.draft) {
            if !self.draft {
                menu = write(
                    menu,
                    "pull_requests.actions.draft",
                    IconName::GitPullRequestDraft,
                    Lifecycle::Draft,
                );
            } else if primary != Some(Primary::Ready) {
                menu = write(
                    menu,
                    "pull_requests.actions.ready",
                    IconName::GitPullRequest,
                    Lifecycle::Ready,
                );
            }
        }
        if open
            && self.route == PullRequestStackRoute::Single
            && !self.conflicting
            && let Some(action) = action
            && action.behind_by.is_some_and(|behind| behind > 0)
            && action.can_update_branch
            && action.capabilities.update_branch
        {
            if merges_base(Some(action)) {
                menu = write(
                    menu,
                    "pull_requests.actions.update_branch",
                    IconName::ArrowUpDown,
                    Lifecycle::UpdateBranch,
                );
            }
            menu = write(
                menu,
                "pull_requests.actions.update_rebase_menu",
                IconName::ArrowUpDown,
                Lifecycle::UpdateRebase,
            );
        }
        if open && !self.draft && may(|action| action.can_merge) {
            match self.route {
                PullRequestStackRoute::Single => {
                    if !self.conflicting
                        && !matches!(primary, Some(Primary::Merge { .. }))
                        && action.is_none_or(|action| !action.merge_methods.is_empty())
                    {
                        // GitHub would refuse it until reviews or checks allow.
                        let blocked = action.is_some_and(|action| {
                            action.merge_state == PullRequestMergeState::Blocked
                        });
                        menu = menu.menu_with_icon_and_enable(
                            crate::tr!("pull_requests.actions.merge_now").into_owned(),
                            IconName::GitMerge,
                            self.item(Lifecycle::Merge),
                            !self.busy && !blocked,
                        );
                        if blocked {
                            menu = menu
                                .label(crate::tr!("pull_requests.actions.blocked").into_owned());
                        }
                    }
                    if let Some(action) = action {
                        if action.auto_merge.is_some() {
                            menu = write(
                                menu,
                                "pull_requests.actions.disable_auto_merge",
                                IconName::GitMerge,
                                Lifecycle::DisableAutoMerge,
                            );
                        } else if action.auto_merge_allowed
                            && action.capabilities.auto_merge
                            && !action.merge_queue
                            && primary != Some(Primary::EnableAutoMerge)
                        {
                            menu = write(
                                menu,
                                "pull_requests.actions.enable_auto_merge_menu",
                                IconName::GitMerge,
                                Lifecycle::EnableAutoMerge,
                            );
                        }
                        if action.merge_methods.len() >= 2 {
                            menu = menu.label(
                                crate::tr!("pull_requests.actions.merge_method").into_owned(),
                            );
                            for method in &action.merge_methods {
                                menu = menu.menu_with_check(
                                    method_label(*method),
                                    self.method == Some(*method),
                                    Box::new(ChooseMergeMethod {
                                        key: self.key.clone(),
                                        method: *method,
                                    }),
                                );
                            }
                        }
                    }
                }
                PullRequestStackRoute::Layer { .. } => {}
                PullRequestStackRoute::Unknown => {
                    menu = menu
                        .menu_with_icon_and_enable(
                            crate::tr!("pull_requests.actions.merge_now").into_owned(),
                            IconName::GitMerge,
                            self.item(Lifecycle::Merge),
                            false,
                        )
                        .label(
                            crate::tr!("pull_requests.actions.stack_checking", number = number)
                                .into_owned(),
                        );
                }
            }
        }
        menu = menu.separator();
        match self.state {
            PullRequestState::Open if may(|action| action.can_update) => write(
                menu,
                "pull_requests.actions.close_menu",
                IconName::GitPullRequestClosed,
                Lifecycle::Close,
            ),
            PullRequestState::Closed
                if may(|action| action.can_update && action.capabilities.reopen) =>
            {
                write(
                    menu,
                    "pull_requests.actions.reopen",
                    IconName::RotateCcw,
                    Lifecycle::Reopen,
                )
            }
            PullRequestState::Merged
                if may(|action| action.can_merge && action.capabilities.revert) =>
            {
                write(
                    menu,
                    "pull_requests.actions.revert_menu",
                    IconName::Undo2,
                    Lifecycle::Revert,
                )
            }
            _ => menu,
        }
    }
}

impl Offer {
    /// A native stack's group: Merge stack and Rebase stack as the stack allows them, then the
    /// method the stack merge uses.
    fn stack_group(&self, stack: &super::stack::StackOffer, mut menu: PopupMenu) -> PopupMenu {
        use super::stack::Avail;
        let action = self.action.as_ref();
        let items = [
            (
                super::stack::merge_count_label(stack),
                IconName::GitMerge,
                stack.merge(action),
                Lifecycle::MergeStack,
            ),
            (
                super::stack::rebase_menu_label(),
                IconName::RefreshCw,
                stack.rebase(action),
                Lifecycle::RebaseStack,
            ),
        ];
        for (label, icon, avail, kind) in items {
            match avail {
                Avail::Hidden => {}
                Avail::Enabled => {
                    menu = menu.menu_with_icon_and_enable(label, icon, self.item(kind), !self.busy)
                }
                Avail::Disabled(reason) => {
                    menu = menu
                        .menu_with_icon_and_enable(label, icon, self.item(kind), false)
                        .label(reason)
                }
            }
        }
        if let Some(action) = action
            && action.merge_methods.len() >= 2
            && stack.merge(Some(action)) != Avail::Hidden
        {
            menu = menu.label(crate::tr!("pull_requests.actions.merge_method").into_owned());
            for method in &action.merge_methods {
                menu = menu.menu_with_check(
                    method_label(*method),
                    self.method == Some(*method),
                    Box::new(ChooseMergeMethod {
                        key: self.key.clone(),
                        method: *method,
                    }),
                );
            }
        }
        menu
    }
}

/// The client's choice, then the project's default, then the first the repository enables.
pub(super) fn merge_method(
    enabled: &[PullRequestMergeMethod],
    chosen: Option<PullRequestMergeMethod>,
    project_default: Option<PullRequestMergeMethod>,
) -> Option<PullRequestMergeMethod> {
    [chosen, project_default]
        .into_iter()
        .flatten()
        .find(|method| enabled.contains(method))
        .or_else(|| enabled.first().copied())
}

pub(super) fn method_label(method: PullRequestMergeMethod) -> String {
    crate::tr!(match method {
        PullRequestMergeMethod::Merge => "pull_requests.merge.method_merge",
        PullRequestMergeMethod::Squash => "pull_requests.merge.method_squash",
        PullRequestMergeMethod::Rebase => "pull_requests.merge.method_rebase",
    })
    .into_owned()
}

pub(super) fn segment_label(method: PullRequestMergeMethod) -> String {
    crate::tr!(match method {
        PullRequestMergeMethod::Merge => "pull_requests.merge.segment_merge",
        PullRequestMergeMethod::Squash => "pull_requests.merge.segment_squash",
        PullRequestMergeMethod::Rebase => "pull_requests.merge.segment_rebase",
    })
    .into_owned()
}

fn short(head: &str) -> String {
    head.chars().take(7).collect()
}

type Answered = Rc<dyn Fn(&PullRequestActionResult, &mut Window, &mut App)>;

/// Where a lifecycle action goes and who hears its answer.
#[derive(Clone)]
pub(super) struct Target {
    pub(super) store: Entity<WorkspaceStore>,
    pub(super) window_state: Entity<WindowState>,
    pub(super) session: String,
    pub(super) offer: Offer,
    /// The pull request's host as a person reads it.
    pub(super) host_name: String,
    pub(super) title: String,
    pub(super) head_branch: String,
    pub(super) base_branch: String,
    /// The thread's project, by id and name.
    project: Option<(String, String)>,
    project_default: Option<PullRequestMergeMethod>,
    /// Told when the write is sent.
    pub(super) started: Rc<dyn Fn(&mut App)>,
    /// Told every answer, after its toast.
    pub(super) done: Answered,
}

impl Target {
    /// The pull request as the thread's links show it.
    pub(super) fn new(
        store: &Entity<WorkspaceStore>,
        window_state: &Entity<WindowState>,
        session: &str,
        key: &PullRequestKey,
        action: Option<PullRequestActionState>,
        chosen: Option<PullRequestMergeMethod>,
        cx: &App,
    ) -> Option<Self> {
        let workspace = store.read(cx);
        let links = workspace.pull_requests(session);
        let project = workspace
            .thread_meta(session)
            .and_then(|meta| meta.project_id.clone())
            .and_then(|id| {
                workspace
                    .projects()
                    .into_iter()
                    .find(|project| project.id == id)
                    .map(|project| (project.id, project.name))
            });
        let project_default = project
            .as_ref()
            .and_then(|(id, _)| workspace.settings().project_merge_methods.get(id).copied());
        let operations = workspace
            .thread_meta(session)
            .map_or(&[][..], |meta| meta.pull_request_operations.as_slice());
        let offer = Offer::new(
            key,
            links,
            operations,
            action,
            chosen,
            project_default,
            false,
        )?;
        let snapshot = links
            .iter()
            .find(|link| link.key == *key)
            .and_then(|link| link.snapshot.clone());
        Some(Self {
            host_name: super::host_name(workspace, &key.host),
            store: store.clone(),
            window_state: window_state.clone(),
            session: session.to_owned(),
            offer,
            title: snapshot
                .as_ref()
                .map(|s| s.title.clone())
                .unwrap_or_default(),
            head_branch: snapshot
                .as_ref()
                .map(|s| s.head_branch.clone())
                .unwrap_or_default(),
            base_branch: snapshot.map(|s| s.base_branch).unwrap_or_default(),
            project,
            project_default,
            started: Rc::new(|_| {}),
            done: Rc::new(|_, _, _| {}),
        })
    }

    fn number(&self) -> String {
        self.offer.key.number.to_string()
    }

    /// Whether the thread's composer may be given a message: not read-only on this device, and
    /// neither settled nor archived.
    fn takes_messages(&self, cx: &App) -> bool {
        let workspace = self.store.read(cx);
        workspace
            .session_status()
            .is_some_and(|status| !status.conversation_read_only)
            && workspace
                .thread_meta(&self.session)
                .is_some_and(|meta| meta.archived_at.is_none() && !meta.is_settled())
    }

    /// Writes at once, confirms first, opens the merge dialog, or fills the composer.
    pub(super) fn run(self, kind: Lifecycle, window: &mut Window, cx: &mut App) {
        let number = self.number();
        match kind {
            Lifecycle::Ready => self.send(PullRequestAction::ReadyForReview, kind, window, cx),
            Lifecycle::Draft => self.send(PullRequestAction::ConvertToDraft, kind, window, cx),
            Lifecycle::Reopen => self.send(PullRequestAction::Reopen, kind, window, cx),
            Lifecycle::DisableAutoMerge => {
                self.send(PullRequestAction::DisableAutoMerge, kind, window, cx)
            }
            Lifecycle::UpdateBranch => {
                let Some(head) = self.offer.action.as_ref().map(|a| a.head.clone()) else {
                    return;
                };
                self.send(
                    PullRequestAction::UpdateBranch {
                        head,
                        rebase: false,
                    },
                    kind,
                    window,
                    cx,
                )
            }
            Lifecycle::Close => match &self.offer.action {
                Some(action) => {
                    let reopen = Some(action.capabilities.reopen);
                    self.confirm_close(reopen, kind, window, cx)
                }
                // A menu opened before the action state was read: read it, so the confirmation
                // never promises a reopen the host cannot do.
                None => {
                    let task = self.store.update(cx, |store, cx| {
                        store.read_pull_request(
                            self.session.clone(),
                            self.offer.key.clone(),
                            PullRequestRead::ActionState,
                            cx,
                        )
                    });
                    window
                        .spawn(cx, async move |cx| {
                            let reopen = match task.await {
                                Ok((PullRequestReadResponse::ActionState(state), _)) => {
                                    Some(state.capabilities.reopen)
                                }
                                _ => None,
                            };
                            let _ = cx
                                .update(|window, cx| self.confirm_close(reopen, kind, window, cx));
                        })
                        .detach();
                }
            },
            Lifecycle::Revert => self.confirm(
                crate::tr!(
                    "pull_requests.actions.revert_title",
                    number = number.clone()
                ),
                crate::tr!("pull_requests.actions.revert_desc", number = number),
                crate::tr!("pull_requests.actions.revert_confirm"),
                ButtonVariant::Primary,
                PullRequestAction::Revert,
                kind,
                window,
                cx,
            ),
            Lifecycle::UpdateRebase => {
                let Some(head) = self.offer.action.as_ref().map(|a| a.head.clone()) else {
                    return;
                };
                let title = crate::tr!(
                    "pull_requests.actions.rebase_title",
                    head = self.head_branch.clone(),
                    base = self.base_branch.clone()
                );
                let description = crate::tr!(
                    "pull_requests.actions.rebase_desc",
                    head = self.head_branch.clone(),
                    host_name = self.host_name.clone()
                );
                self.confirm(
                    title,
                    description,
                    crate::tr!("pull_requests.actions.update_rebase"),
                    ButtonVariant::Primary,
                    PullRequestAction::UpdateBranch { head, rebase: true },
                    kind,
                    window,
                    cx,
                )
            }
            Lifecycle::Merge => open_merge_dialog(self, false, window, cx),
            Lifecycle::EnableAutoMerge => open_merge_dialog(self, true, window, cx),
            Lifecycle::AskConflicts | Lifecycle::AskChecks => self.ask(kind, cx),
            Lifecycle::MergeStack => super::stack::open_merge_dialog(self, window, cx),
            Lifecycle::RebaseStack => super::stack::open_rebase_dialog(self, window, cx),
        }
    }

    /// Puts a message for the agent into this thread's composer; the user reads, edits and sends
    /// it, or not.
    fn ask(self, kind: Lifecycle, cx: &mut App) {
        let number = self.number();
        let text = if kind == Lifecycle::AskConflicts {
            crate::tr!(
                "pull_requests.actions.ask_conflicts",
                number = number,
                head = self.head_branch.clone(),
                base = self.base_branch.clone()
            )
            .into_owned()
        } else {
            let failing = self
                .offer
                .action
                .as_ref()
                .map(|action| action.failing_checks.clone())
                .unwrap_or_default();
            if failing.is_empty() {
                crate::tr!("pull_requests.actions.ask_checks_unnamed", number = number).into_owned()
            } else {
                let mut names = failing
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                if failing.len() > 5 {
                    names.push(' ');
                    names.push_str(&crate::tr!(
                        "pull_requests.actions.and_more",
                        count = (failing.len() - 5).to_string()
                    ));
                }
                crate::tr!(
                    "pull_requests.actions.ask_checks",
                    number = number,
                    names = names
                )
                .into_owned()
            }
        };
        let session = self.session.clone();
        self.store
            .update(cx, |store, cx| store.append_to_composer(session, text, cx));
        // On a phone the composer is on the thread page under this one.
        if self.window_state.read(cx).destination() == Destination::PullRequest {
            self.window_state.update(cx, |state, cx| state.back(cx));
        }
    }

    /// The close confirmation, whose consequence depends on whether the host can `reopen` what
    /// it closes; `None` when that could not be read.
    fn confirm_close(
        self,
        reopen: Option<bool>,
        kind: Lifecycle,
        window: &mut Window,
        cx: &mut App,
    ) {
        let number = self.number();
        let description = match reopen {
            Some(true) => {
                crate::tr!("pull_requests.actions.close_desc", number = number.clone())
            }
            Some(false) => crate::tr!(
                "pull_requests.actions.close_desc_final",
                number = number.clone(),
                host_name = self.host_name.clone()
            ),
            None => crate::tr!(
                "pull_requests.actions.close_desc_unknown",
                number = number.clone(),
                host_name = self.host_name.clone()
            ),
        };
        self.confirm(
            crate::tr!("pull_requests.actions.close_title", number = number),
            description,
            crate::tr!("pull_requests.actions.close_confirm"),
            ButtonVariant::Danger,
            PullRequestAction::Close,
            kind,
            window,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn confirm(
        self,
        title: std::borrow::Cow<'static, str>,
        description: std::borrow::Cow<'static, str>,
        confirm: std::borrow::Cow<'static, str>,
        variant: ButtonVariant,
        action: PullRequestAction,
        kind: Lifecycle,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (title, description, confirm) = (
            title.into_owned(),
            description.into_owned(),
            confirm.into_owned(),
        );
        window.open_alert_dialog(cx, move |alert, _, _| {
            let target = self.clone();
            let action = action.clone();
            alert
                .title(title.clone())
                .description(description.clone())
                .button_props(
                    DialogButtons::default()
                        .ok_variant(variant)
                        .ok_text(confirm.clone())
                        .cancel_text(crate::tr!("pull_requests.actions.cancel"))
                        .show_cancel(true),
                )
                .on_ok(move |_, window, cx| {
                    target.clone().send(action.clone(), kind, window, cx);
                    true
                })
        });
    }

    /// One write, its toast, then whoever asked.
    fn send(self, action: PullRequestAction, kind: Lifecycle, window: &mut Window, cx: &mut App) {
        let task = self.command(action, cx);
        window
            .spawn(cx, async move |cx| {
                let result = task.await;
                let _ = cx.update(|window, cx| self.answered(kind, &result, window, cx));
            })
            .detach();
    }

    fn command(
        &self,
        action: PullRequestAction,
        cx: &mut App,
    ) -> gpui::Task<PullRequestActionResult> {
        (self.started)(cx);
        let task = self.store.update(cx, |store, cx| {
            store.command(
                Command::RunPullRequestAction {
                    session_id: self.session.clone(),
                    key: self.offer.key.clone(),
                    action,
                },
                cx,
            )
        });
        cx.spawn(async move |_| match answer_of(task.await) {
            Ok(result) => result,
            Err(error) => PullRequestActionResult::Rejected(PullRequestRejection::Refused {
                messages: vec![super::detail::reason(&error)],
            }),
        })
    }

    fn answered(
        &self,
        kind: Lifecycle,
        result: &PullRequestActionResult,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(note) = self.toast(kind, result) {
            let key = &self.offer.key;
            window.push_notification(
                note.id1::<super::compose::PullRequestWrite>(SharedString::from(format!(
                    "{}/{}#{}",
                    key.host, key.repository, key.number
                ))),
                cx,
            );
        }
        (self.done)(result, window, cx);
    }

    /// The words of an answer; a success goes by itself, anything else stays until it is read.
    fn toast(&self, kind: Lifecycle, result: &PullRequestActionResult) -> Option<Notification> {
        let number = self.number();
        let (head, base) = (self.head_branch.clone(), self.base_branch.clone());
        let merging = matches!(kind, Lifecycle::Merge | Lifecycle::EnableAutoMerge);
        let updating = matches!(kind, Lifecycle::UpdateBranch | Lifecycle::UpdateRebase);
        let open_on_host = |note: Notification| {
            let url = self.offer.url.clone();
            let host_name = self.host_name.clone();
            note.action(move |_, _, _| {
                let url = url.clone();
                Button::new("pr-result-open")
                    .ghost()
                    .xsmall()
                    .label(crate::tr!(
                        "pull_requests.open_on_host",
                        host_name = &host_name
                    ))
                    .on_click(move |_, _, cx| cx.open_url(&url))
            })
        };
        let note = match result {
            PullRequestActionResult::Applied => match kind {
                Lifecycle::Ready => Notification::success(
                    crate::tr!("pull_requests.result.ready", number = number).into_owned(),
                ),
                Lifecycle::Draft => Notification::success(
                    crate::tr!("pull_requests.result.draft", number = number).into_owned(),
                ),
                Lifecycle::Close => Notification::info(
                    crate::tr!("pull_requests.result.closed", number = number).into_owned(),
                ),
                Lifecycle::Reopen => Notification::success(
                    crate::tr!("pull_requests.result.reopened", number = number).into_owned(),
                ),
                Lifecycle::DisableAutoMerge => Notification::info(
                    crate::tr!("pull_requests.result.auto_merge_off", number = number).into_owned(),
                ),
                Lifecycle::UpdateBranch | Lifecycle::UpdateRebase => Notification::success(
                    crate::tr!("pull_requests.result.updated_body").into_owned(),
                )
                .title(crate::tr!(
                    "pull_requests.result.updated",
                    head = head,
                    base = base
                )),
                _ => Notification::success(
                    crate::tr!("pull_requests.result.merged", number = number).into_owned(),
                ),
            },
            PullRequestActionResult::UpToDate => Notification::info(
                crate::tr!("pull_requests.result.up_to_date", head = head, base = base)
                    .into_owned(),
            ),
            PullRequestActionResult::Queued { .. } => {
                Notification::info(crate::tr!("pull_requests.result.queued_body").into_owned())
                    .title(crate::tr!("pull_requests.result.queued", number = number))
            }
            PullRequestActionResult::AutoMergeEnabled { method } => Notification::info(
                crate::tr!(
                    "pull_requests.result.auto_merge_on_body",
                    method = method_label(*method),
                    host_name = self.host_name.clone()
                )
                .into_owned(),
            )
            .title(crate::tr!(
                "pull_requests.result.auto_merge_on",
                number = number
            )),
            PullRequestActionResult::Opened { number: new, url } => {
                let url = url.clone();
                let host_name = self.host_name.clone();
                Notification::success(
                    crate::tr!(
                        "pull_requests.result.reverted",
                        new = new.to_string(),
                        number = number
                    )
                    .into_owned(),
                )
                .action(move |_, _, _| {
                    let url = url.clone();
                    Button::new("pr-result-open")
                        .ghost()
                        .xsmall()
                        .label(crate::tr!(
                            "pull_requests.open_on_host",
                            host_name = &host_name
                        ))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                })
            }
            PullRequestActionResult::Rejected(PullRequestRejection::StaleHead { head: actual })
                if merging =>
            {
                Notification::error(
                    crate::tr!(
                        "pull_requests.result.stale_body",
                        expected = self
                            .offer
                            .action
                            .as_ref()
                            .map(|action| short(&action.head))
                            .unwrap_or_default(),
                        actual = short(actual)
                    )
                    .into_owned(),
                )
                .title(crate::tr!(
                    "pull_requests.result.not_merged_stale",
                    number = number
                ))
            }
            PullRequestActionResult::Rejected(rejection) => {
                let reason = rejection_reason(rejection, &self.host_name);
                if merging {
                    return Some(Notification::error(reason).title(crate::tr!(
                        "pull_requests.result.merge_failed",
                        number = number
                    )));
                }
                Notification::error(if updating {
                    crate::tr!(
                        "pull_requests.result.update_failed",
                        head = head,
                        reason = reason
                    )
                    .into_owned()
                } else {
                    crate::tr!(
                        "pull_requests.result.action_failed",
                        verb = crate::tr!(match kind {
                            Lifecycle::Ready => "pull_requests.actions.verb_ready",
                            Lifecycle::Draft => "pull_requests.actions.verb_draft",
                            Lifecycle::Close => "pull_requests.actions.verb_close",
                            Lifecycle::Reopen => "pull_requests.actions.verb_reopen",
                            Lifecycle::Revert => "pull_requests.actions.verb_revert",
                            _ => "pull_requests.actions.verb_disable_auto_merge",
                        })
                        .into_owned(),
                        number = number,
                        reason = reason
                    )
                    .into_owned()
                })
            }
            PullRequestActionResult::Uncertain => {
                let message = crate::tr!(
                    "pull_requests.result.uncertain_body",
                    message = crate::tr!("pull_requests.result.connection_lost").into_owned(),
                    host_name = self.host_name.clone()
                )
                .into_owned();
                let title = if merging {
                    crate::tr!("pull_requests.result.uncertain_merge", number = number).into_owned()
                } else {
                    crate::tr!(
                        "pull_requests.result.uncertain",
                        action = crate::tr!(match kind {
                            Lifecycle::Ready => "pull_requests.result.action_ready",
                            Lifecycle::Draft => "pull_requests.result.action_draft",
                            Lifecycle::Close => "pull_requests.result.action_close",
                            Lifecycle::Reopen => "pull_requests.result.action_reopen",
                            Lifecycle::Revert => "pull_requests.result.action_revert",
                            Lifecycle::UpdateBranch | Lifecycle::UpdateRebase => {
                                "pull_requests.result.action_update"
                            }
                            _ => "pull_requests.result.action_auto_merge",
                        })
                        .into_owned(),
                        number = number
                    )
                    .into_owned()
                };
                open_on_host(Notification::warning(message).title(title))
            }
            // A host that rebases in the background has only started it.
            PullRequestActionResult::RebaseStarted if updating => Notification::success(
                crate::tr!(
                    "pull_requests.result.rebase_started_body",
                    host_name = self.host_name.clone()
                )
                .into_owned(),
            )
            .title(crate::tr!(
                "pull_requests.result.rebase_started",
                head = head
            )),
            // A stack write's answers are the stack's own words.
            PullRequestActionResult::Partial { .. }
            | PullRequestActionResult::Pending { .. }
            | PullRequestActionResult::MergeUnconfirmed { .. }
            | PullRequestActionResult::RebaseStarted
            | PullRequestActionResult::Rebased { .. }
            | PullRequestActionResult::RebaseStopped { .. } => return None,
        };
        let settled = matches!(
            result,
            PullRequestActionResult::Applied
                | PullRequestActionResult::UpToDate
                | PullRequestActionResult::Queued { .. }
                | PullRequestActionResult::AutoMergeEnabled { .. }
                | PullRequestActionResult::Opened { .. }
                | PullRequestActionResult::RebaseStarted
        );
        Some(note.autohide(settled))
    }
}

/// The merge confirmation: what GitHub holds now, the method, and what the merge would meet.
struct MergeDialog {
    target: Target,
    auto: bool,
    state: Option<Result<PullRequestActionState, String>>,
    method: Option<PullRequestMergeMethod>,
    make_default: bool,
    remove_credits: bool,
    sending: bool,
}

impl MergeDialog {
    /// The merge reads the pull request fresh; the dialog shows what it read.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.state = None;
        let task = self.target.store.update(cx, |store, cx| {
            store.read_pull_request(
                self.target.session.clone(),
                self.target.offer.key.clone(),
                PullRequestRead::ActionState,
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.state = Some(match result {
                    Ok((PullRequestReadResponse::ActionState(state), _)) => {
                        this.method = merge_method(
                            &state.merge_methods,
                            this.target.offer.method,
                            this.target.project_default,
                        );
                        Ok(state)
                    }
                    Ok(_) => Err(String::new()),
                    Err(error) => Err(super::detail::reason(&error)),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(Ok(state)), Some(method)) = (&self.state, self.method) else {
            return;
        };
        if self.sending {
            return;
        }
        self.sending = true;
        cx.notify();
        if self.make_default
            && let Some((project, _)) = self.target.project.clone()
        {
            self.target.store.update(cx, |store, _| {
                store.set_project_merge_method(project, method)
            });
        }
        let action = PullRequestAction::Merge {
            head: state.head.clone(),
            method,
            auto: self.auto,
            remove_credits: self.credits_shown() && self.remove_credits,
        };
        let mut target = self.target.clone();
        target.offer.action = Some(state.clone());
        let kind = if self.auto {
            Lifecycle::EnableAutoMerge
        } else {
            Lifecycle::Merge
        };
        let task = target.command(action, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.sending = false;
                window.close_dialog(cx);
                target.answered(kind, &result, window, cx);
            });
        })
        .detach();
    }

    /// GitHub writes a merge queue's message itself, and a rebase keeps each commit's.
    /// Only a host that takes the message Tcode sends can have credits left out of it.
    fn credits_shown(&self) -> bool {
        matches!(&self.state, Some(Ok(state))
            if !state.merge_queue && state.capabilities.merge_message)
            && self.method != Some(PullRequestMergeMethod::Rebase)
    }

    fn confirm_label(&self) -> String {
        let state = self.state.as_ref().and_then(|state| state.as_ref().ok());
        if self.sending {
            return crate::tr!(if self.auto {
                "pull_requests.merge.enabling"
            } else {
                "pull_requests.merge.merging"
            })
            .into_owned();
        }
        if state.is_some_and(|state| state.merge_queue) {
            return crate::tr!("pull_requests.merge.add_to_queue").into_owned();
        }
        if self.auto {
            return crate::tr!("pull_requests.actions.enable_auto_merge").into_owned();
        }
        self.method.map(method_label).unwrap_or_default()
    }

    fn fact(&self, label: &str, value: impl IntoElement, cx: &App) -> AnyElement {
        let compact = self.target.window_state.read(cx).compact;
        h_flex()
            .gap_2()
            .items_start()
            .child(
                // One label column so the values align; no owner carries a label-column width.
                div()
                    .w(px(104.))
                    .flex_none()
                    .text_secondary(compact, cx)
                    .child(crate::tr!(label).into_owned()),
            )
            .child(div().flex_1().min_w_0().text_body(compact, cx).child(value))
            .into_any_element()
    }
}

impl Render for MergeDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let target = &self.target;
        let key = &target.offer.key;
        let number = key.number.to_string();
        let mono = cx.theme().mono_font_family.clone();
        let muted = cx.theme().muted_foreground;
        let state = self.state.as_ref().and_then(|state| state.as_ref().ok());
        let head = state.map(|state| state.head.clone());
        let facts =
            v_flex()
                .gap_1p5()
                .child(self.fact("pull_requests.merge.host", key.host.clone(), cx))
                .child(
                    self.fact(
                        "pull_requests.merge.repository",
                        div()
                            .font_family(mono.clone())
                            .child(key.repository.clone()),
                        cx,
                    ),
                )
                .child(
                    self.fact(
                        "pull_requests.merge.pull_request",
                        div()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(format!(
                                "#{number} {}",
                                target.title
                            ))),
                        cx,
                    ),
                )
                .child(self.fact(
                    "pull_requests.merge.into",
                    div().overflow_hidden().font_family(mono.clone()).child(
                        crate::widgets::fade_text(format!(
                            "{} ← {}",
                            target.base_branch, target.head_branch
                        )),
                    ),
                    cx,
                ))
                .child(
                    self.fact(
                        "pull_requests.merge.head",
                        match &head {
                            Some(head) => div()
                                .font_family(mono.clone())
                                .child(short(head))
                                .into_any_element(),
                            None => Spinner::new().into_any_element(),
                        },
                        cx,
                    ),
                );
        let view = cx.entity();
        let compact = target.window_state.read(cx).compact;
        let method = match (state, self.method) {
            (Some(state), Some(chosen)) if state.merge_methods.len() >= 2 => {
                let segments = state.merge_methods.iter().map(|method| {
                    let method = *method;
                    let view = view.clone();
                    Choice::new(
                        SharedString::from(format!("pr-merge-method-{method:?}")),
                        segment_label(method),
                    )
                    .selected(chosen == method)
                    .on_select(move |_, cx| {
                        view.update(cx, |dialog, cx| {
                            dialog.method = Some(method);
                            cx.notify();
                        })
                    })
                });
                ChoiceGroup::new("pr-merge-method")
                    .compact(compact)
                    .aria_label(crate::tr!("pull_requests.merge.method"))
                    .w_full()
                    .choices(segments)
                    .into_any_element()
            }
            (Some(_), Some(chosen)) => div()
                .text_body(compact, cx)
                .child(method_label(chosen))
                .into_any_element(),
            (Some(_), None) => div().into_any_element(),
            (None, _) => Spinner::new().into_any_element(),
        };
        let default_box = self
            .method
            .filter(|method| state.is_some() && Some(*method) != target.project_default)
            .zip(target.project.clone())
            .map(|(method, (_, project))| {
                let view = view.clone();
                Checkbox::new("pr-merge-default")
                    .label(
                        crate::tr!(
                            "pull_requests.merge.make_default",
                            method = segment_label(method),
                            project = project
                        )
                        .into_owned(),
                    )
                    .checked(self.make_default)
                    .disabled(self.sending)
                    .on_click(move |checked, _, cx| {
                        let checked = *checked;
                        view.update(cx, |dialog, cx| {
                            dialog.make_default = checked;
                            cx.notify();
                        })
                    })
            });
        let credits =
            self.credits_shown().then(|| {
                let view = view.clone();
                v_flex()
                    .gap_0p5()
                    .child(
                        Checkbox::new("pr-merge-credits")
                            .label(crate::tr!("pull_requests.merge.remove_credits").into_owned())
                            .checked(self.remove_credits)
                            .disabled(self.sending)
                            .on_click(move |checked, _, cx| {
                                let checked = *checked;
                                view.update(cx, |dialog, cx| {
                                    dialog.remove_credits = checked;
                                    cx.notify();
                                })
                            }),
                    )
                    .child(div().text_secondary(compact, cx).child(
                        crate::tr!("pull_requests.merge.remove_credits_caption").into_owned(),
                    ))
            });
        let note = |icon: IconName, color, text: String| {
            h_flex()
                .gap_1p5()
                .items_start()
                .text_secondary(compact, cx)
                .text_color(cx.theme().foreground)
                .child(
                    // Centres the font-sized icon on the first line's taller line box.
                    div()
                        .pt(cx.theme().tokens.spacing.xxs)
                        .child(Icon::new(icon).text_color(color)),
                )
                .child(div().flex_1().min_w_0().child(text))
        };
        let mut notes = Vec::new();
        if let Some(state) = state {
            if state.merge_queue {
                notes.push(note(
                    IconName::ListOrdered,
                    cx.theme().info,
                    crate::tr!("pull_requests.merge.queue_note", number = number.clone())
                        .into_owned(),
                ));
            } else if self.auto
                && let Some(method) = self.method
            {
                notes.push(note(
                    IconName::GitMerge,
                    cx.theme().info,
                    crate::tr!(
                        "pull_requests.merge.auto_note",
                        number = number.clone(),
                        method = method_label(method),
                        host_name = self.target.host_name.clone()
                    )
                    .into_owned(),
                ));
            }
            let failing = state.failing_checks.len();
            if failing > 0 {
                notes.push(note(
                    IconName::CircleX,
                    cx.theme().danger,
                    if failing == 1 {
                        crate::tr!("pull_requests.merge.checks_failing_one").into_owned()
                    } else {
                        crate::tr!(
                            "pull_requests.merge.checks_failing",
                            failed = failing.to_string()
                        )
                        .into_owned()
                    },
                ));
            }
            if state.pending_checks > 0 {
                notes.push(note(
                    IconName::CircleDashed,
                    cx.theme().warning,
                    if state.pending_checks == 1 {
                        crate::tr!("pull_requests.merge.checks_running_one").into_owned()
                    } else {
                        crate::tr!(
                            "pull_requests.merge.checks_running",
                            running = state.pending_checks.to_string()
                        )
                        .into_owned()
                    },
                ));
            }
            if state.merge_state == PullRequestMergeState::Blocked {
                notes.push(note(
                    IconName::MessageSquareWarning,
                    cx.theme().danger,
                    crate::tr!("pull_requests.actions.blocked").into_owned(),
                ));
            }
            if let Some(behind) = state.behind_by.filter(|behind| *behind > 0) {
                notes.push(note(
                    IconName::ArrowUpDown,
                    muted,
                    crate::tr!(
                        "pull_requests.merge.behind",
                        count = behind.to_string(),
                        base = target.base_branch.clone()
                    )
                    .into_owned(),
                ));
            }
        }
        let failure = match &self.state {
            Some(Err(reason)) => Some(
                v_flex()
                    .gap_1()
                    .text_secondary(compact, cx)
                    .text_color(cx.theme().danger)
                    .child(
                        crate::tr!("pull_requests.merge.load_failed", number = number).into_owned(),
                    )
                    .when(!reason.is_empty(), |failure| failure.child(reason.clone()))
                    .child(
                        Button::new("pr-merge-retry")
                            .outline()
                            .xsmall()
                            .label(crate::tr!("pull_requests.detail.retry"))
                            .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                    ),
            ),
            _ => None,
        };
        let ready = state.is_some() && self.method.is_some();
        v_flex()
            .gap_3()
            .child(facts)
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_caption(cx)
                            .child(crate::tr!("pull_requests.merge.method").into_owned()),
                    )
                    .child(method)
                    .children(default_box),
            )
            .children(credits)
            .when(!notes.is_empty(), |content| {
                content.child(v_flex().gap_1().children(notes))
            })
            .children(failure)
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("pr-merge-cancel")
                            .outline()
                            .small()
                            .disabled(self.sending)
                            .label(crate::tr!("pull_requests.actions.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("pr-merge-confirm")
                            .primary()
                            .small()
                            .loading(self.sending)
                            .disabled(!ready || self.sending)
                            .label(self.confirm_label())
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}

fn open_merge_dialog(target: Target, auto: bool, window: &mut Window, cx: &mut App) {
    let remove_credits = target
        .store
        .read(cx)
        .settings()
        .remove_agent_credits_on_merge;
    let number = target.number();
    let dialog = cx.new(|cx| {
        let mut dialog = MergeDialog {
            target,
            auto,
            state: None,
            method: None,
            make_default: false,
            remove_credits,
            sending: false,
        };
        dialog.load(cx);
        dialog
    });
    let title = crate::tr!(
        if auto {
            "pull_requests.merge.title_auto"
        } else {
            "pull_requests.merge.title"
        },
        number = number
    )
    .into_owned();
    window.open_dialog(cx, move |base, _, cx| {
        let sending = dialog.read(cx).sending;
        base.title(title.clone())
            .keyboard(!sending)
            .close_button(!sending)
            .overlay_closable(!sending)
            .footer(crate::overlay::DialogActions::new())
            .content({
                let dialog = dialog.clone();
                move |content, _, _| content.child(dialog.clone())
            })
    });
}

/// The header's primary action or status, `None` where the rank offers none.
pub(super) fn primary_element(
    target: &Target,
    primary: Primary,
    compact: bool,
    cx: &App,
) -> AnyElement {
    let busy = target.offer.busy;
    let number = target.number();
    let key = target.offer.key.clone();
    let url = target.offer.url.clone();
    let chip = |icon: IconName, color, label: String, tooltip: Option<String>| {
        let id = SharedString::from(format!("pr-primary-chip-{}", key.number));
        let chip = h_flex()
            .id(id)
            .flex_none()
            .gap_1()
            .items_center()
            .px(cx.theme().tokens.spacing.sm)
            .py(cx.theme().tokens.spacing.xxs)
            .rounded(material::radius_chip(cx))
            .bg(gpui::Hsla::opacity(&color, 0.1))
            .text_secondary(compact, cx)
            .text_color(color)
            .child(Icon::new(icon))
            .child(label);
        match tooltip {
            Some(tooltip) => chip
                .tooltip(move |window, cx| {
                    crate::widgets::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
                })
                .into_any_element(),
            None => chip.into_any_element(),
        }
    };
    let run = |kind: Lifecycle| {
        let target = target.clone();
        move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
            target.clone().run(kind, window, cx)
        }
    };
    let button = |icon: IconName, label: String| {
        let accessible = crate::tr!(
            "pull_requests.actions.primary_label",
            label = label.clone(),
            number = number.clone()
        )
        .into_owned();
        let button = Button::new("pr-primary-action")
            .primary()
            .icon(icon)
            .label(label)
            .tooltip(accessible)
            .loading(busy)
            .disabled(busy);
        if compact {
            button.w_full()
        } else {
            button.small()
        }
    };
    use gpui::{InteractiveElement as _, StatefulInteractiveElement as _};
    match primary {
        Primary::ResolveConflicts | Primary::FixChecks => {
            let terms = super::host_terms(target.store.read(cx), &key.host);
            let host_name = target.host_name.clone();
            // The host's own page for what blocks the merge, where it has one. Without a
            // conflicts page the pull request itself is where they are resolved; without a
            // checks page there is nothing to open.
            let (icon, label, ask, page) = if primary == Primary::ResolveConflicts {
                (
                    IconName::GitMergeConflict,
                    crate::tr!("pull_requests.actions.resolve_conflicts").into_owned(),
                    Lifecycle::AskConflicts,
                    Some(match terms.conflicts_page {
                        Some(page) => (
                            crate::tr!(
                                "pull_requests.actions.resolve_on_host",
                                host_name = host_name
                            )
                            .into_owned(),
                            format!("{url}{page}"),
                        ),
                        None => (
                            crate::tr!("pull_requests.open_on_host", host_name = host_name)
                                .into_owned(),
                            url.clone(),
                        ),
                    }),
                )
            } else {
                (
                    IconName::CircleX,
                    crate::tr!("pull_requests.actions.fix_checks").into_owned(),
                    Lifecycle::AskChecks,
                    terms.checks_page.map(|page| {
                        (
                            crate::tr!(
                                "pull_requests.actions.checks_on_host",
                                host_name = host_name
                            )
                            .into_owned(),
                            format!("{url}{page}"),
                        )
                    }),
                )
            };
            let can_ask = target.takes_messages(cx);
            use crate::widgets::menu::DropdownMenu as _;
            button(icon, label)
                .dropdown_menu(move |menu, _, _| {
                    menu.menu_with_enable(
                        crate::tr!(if can_ask {
                            "pull_requests.actions.ask_agent"
                        } else {
                            "pull_requests.actions.thread_unavailable"
                        })
                        .into_owned(),
                        Box::new(RunLifecycle {
                            key: key.clone(),
                            kind: ask,
                        }),
                        can_ask,
                    )
                    .when_some(page.clone(), |menu, (label, url)| {
                        menu.menu(label, Box::new(OpenUrl(url)))
                    })
                })
                .into_any_element()
        }
        Primary::Ready => button(
            IconName::GitPullRequest,
            crate::tr!("pull_requests.actions.ready").into_owned(),
        )
        .on_click(run(Lifecycle::Ready))
        .into_any_element(),
        Primary::AutoMergeOn(method) => chip(
            IconName::GitMerge,
            cx.theme().info,
            crate::tr!(
                "pull_requests.actions.auto_merge_on",
                method = method_label(method)
            )
            .into_owned(),
            Some(
                crate::tr!(
                    "pull_requests.actions.auto_merge_tooltip",
                    host_name = target.host_name.clone()
                )
                .into_owned(),
            ),
        ),
        Primary::Queued(position) => chip(
            IconName::ListOrdered,
            cx.theme().info,
            match position {
                Some(position) => crate::tr!(
                    "pull_requests.actions.queued_position",
                    position = position.to_string()
                )
                .into_owned(),
                None => crate::tr!("pull_requests.actions.queued").into_owned(),
            },
            None,
        ),
        Primary::MergeStack => {
            let Some(stack) = target.offer.stack.as_ref() else {
                return div().into_any_element();
            };
            let (enabled, tooltip) = match stack.merge(target.offer.action.as_ref()) {
                super::stack::Avail::Disabled(reason) => (false, reason),
                _ => (true, super::stack::merge_tooltip(stack)),
            };
            let button = Button::new("pr-primary-action")
                .primary()
                .icon(IconName::GitMerge)
                .label(super::stack::merge_primary_label())
                .aria_label(super::stack::merge_primary_accessible(
                    target.offer.key.number,
                ))
                .tooltip(tooltip)
                .loading(busy)
                .disabled(busy || !enabled)
                .on_click(run(Lifecycle::MergeStack));
            if compact {
                button.w_full().into_any_element()
            } else {
                button.small().into_any_element()
            }
        }
        Primary::StackOperation => {
            let Some(operation) = target
                .offer
                .stack
                .as_ref()
                .and_then(|stack| stack.operation.as_ref())
            else {
                return div().into_any_element();
            };
            let links = target
                .store
                .read(cx)
                .pull_requests(&target.session)
                .to_vec();
            super::stack::operation_chip(Some(target.clone()), operation, &links, compact, cx)
        }
        // A host that only rebases offers its rebase, which asks first.
        Primary::UpdateBranch if !merges_base(target.offer.action.as_ref()) => button(
            IconName::ArrowUpDown,
            crate::tr!("pull_requests.actions.update_rebase_menu").into_owned(),
        )
        .tooltip(
            crate::tr!(
                "pull_requests.actions.rebase_tooltip",
                base = target.base_branch.clone(),
                head = target.head_branch.clone(),
                host_name = target.host_name.clone()
            )
            .into_owned(),
        )
        .on_click(run(Lifecycle::UpdateRebase))
        .into_any_element(),
        Primary::UpdateBranch => button(
            IconName::ArrowUpDown,
            crate::tr!("pull_requests.actions.update_branch").into_owned(),
        )
        .tooltip(
            crate::tr!(
                "pull_requests.actions.update_tooltip",
                base = target.base_branch.clone(),
                head = target.head_branch.clone(),
                host_name = target.host_name.clone()
            )
            .into_owned(),
        )
        .on_click(run(Lifecycle::UpdateBranch))
        .into_any_element(),
        Primary::EnableAutoMerge => button(
            IconName::GitMerge,
            crate::tr!("pull_requests.actions.enable_auto_merge").into_owned(),
        )
        .on_click(run(Lifecycle::EnableAutoMerge))
        .into_any_element(),
        Primary::Merge { method, queue } => button(
            IconName::GitMerge,
            if queue {
                crate::tr!("pull_requests.merge.merge_when_ready").into_owned()
            } else {
                method_label(method)
            },
        )
        .on_click(run(Lifecycle::Merge))
        .into_any_element(),
    }
}

/// Whether the host brings the base in by a merge commit as well as by a rebase; an unread
/// state is taken as yes, as before hosts said.
pub(super) fn merges_base(action: Option<&PullRequestActionState>) -> bool {
    action.is_none_or(|action| action.capabilities.update_merge)
}
