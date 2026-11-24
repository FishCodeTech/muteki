//! The Agents view: a lead thread's dispatched agents and the provider-native
//! subagents of its transcript, in the desktop right panel and the phone sheet.
//! It shows what the host reports; delivery is never decided here.

use std::time::Duration;

use agent::{ItemContent, ItemStatus, ProviderKind};
use gpui::{
    Action, AnyElement, App, Context, Entity, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{InteractiveElementExt as _, h_flex, v_flex};
use serde::Deserialize;
use tcode_core::notice::HostNotice;
use tcode_core::session::EntryContent;
use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};
use tcode_core::ui::RightTab;

use crate::icon::{Icon, IconName};
use crate::material;
use crate::overlay::{DialogButtons, OverlayExt as _};
use crate::sizing::Sizable as _;
use crate::store::{TopicKind, WorkspaceStore, observe_store_topics};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariant, ButtonVariants as _};
use crate::widgets::menu::DropdownMenu as _;
use crate::widgets::spinner::Spinner;
use crate::widgets::tooltip::Tooltip;
use gpui_base::PopoverState;

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_agents, no_json)]
struct AgentOpen(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_agents, no_json)]
struct AgentStop(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_agents, no_json)]
struct AgentSettle(String);
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = tcode_agents, no_json)]
struct AgentCancel(String);

#[derive(Clone)]
struct AgentRow {
    key: String,
    title: String,
    provider: Option<ProviderKind>,
    /// "provider · model".
    detail: String,
    /// Absent for a settled or archived agent the host reports no activity
    /// for.
    execution: Option<AgentExecution>,
    /// Absent for a provider-native subagent.
    delivery: Option<AgentDelivery>,
    /// Whether the child's `report_result` reached its lead.
    report: AgentReport,
    archived: bool,
    preview: Option<String>,
    /// The thread Open selects: the dispatched agent, or a native mirror.
    open: Option<String>,
    /// The dispatched agent that Stop, Settle and Cancel act on.
    agent: Option<String>,
    /// The provider that manages a native subagent.
    managed_by: Option<&'static str>,
    /// A native subagent's type, which its provider names it by.
    agent_type: Option<String>,
    /// When the latest run started and, once it has, ended (unix ms).
    started_at: Option<u64>,
    ended_at: Option<u64>,
}

impl AgentRow {
    fn elapsed(&self) -> Option<String> {
        let started = self.started_at?;
        // A start no later than the last end belongs to a finished run: a
        // running agent whose next run has not started yet has no elapsed time.
        let run_ended = self.ended_at.filter(|ended| *ended >= started);
        let ended = match (
            self.execution.is_some_and(AgentExecution::running),
            run_ended,
        ) {
            (true, None) => tcode_core::project::now_secs() * 1000,
            (false, Some(ended)) => ended,
            _ => return None,
        };
        Some(crate::chat::format_duration(
            ended.saturating_sub(started) / 1000,
        ))
    }
}

/// A lead thread's agents in the Agents view's groups, newest first.
#[derive(Default)]
pub(crate) struct Agents {
    needs_settle: Vec<AgentRow>,
    running: Vec<AgentRow>,
    native: Vec<AgentRow>,
    done: Vec<AgentRow>,
}

impl Agents {
    /// The thread's agents as the host reports them. Results are left out:
    /// only the panel itself shows them, see [`Self::with_results`].
    pub(crate) fn of(store: &WorkspaceStore, parent_id: &str) -> Self {
        let live = store.sidebar_sessions();
        let parent = live.iter().find(|meta| meta.id == parent_id);
        let mut native = Vec::new();
        store.with_active_timeline(|timeline| {
            for entry in &timeline.entries {
                let EntryContent::Item(ItemContent::Subagent {
                    agent_type,
                    description,
                    status,
                    summary,
                    model,
                    ..
                }) = &entry.content
                else {
                    continue;
                };
                let mirror = live.iter().find(|meta| {
                    meta.parent_session_id.as_deref() == Some(parent_id)
                        && meta.native_subagent.as_deref() == Some(entry.id.as_str())
                });
                let provider = parent.map(|parent| parent.provider);
                let execution = match status {
                    ItemStatus::InProgress => AgentExecution::Working,
                    ItemStatus::Completed => AgentExecution::Finished,
                    ItemStatus::Interrupted => AgentExecution::Interrupted,
                    ItemStatus::Failed | ItemStatus::Declined => AgentExecution::Failed,
                };
                native.push(AgentRow {
                    key: entry.id.clone(),
                    title: Some(compact(description))
                        .filter(|title| !title.is_empty())
                        .unwrap_or_else(|| agent_type.clone()),
                    provider,
                    detail: detail(provider, model.as_deref()),
                    execution: Some(execution),
                    delivery: None,
                    report: AgentReport::None,
                    archived: false,
                    preview: summary.as_deref().map(compact).filter(|s| !s.is_empty()),
                    open: mirror.map(|meta| meta.id.clone()),
                    agent: None,
                    managed_by: provider.map(|provider| provider.display_name()),
                    agent_type: Some(agent_type.clone()),
                    // A native subagent's end is not reported, only its start.
                    started_at: entry.ts.filter(|_| execution.running()),
                    ended_at: None,
                });
            }
        });
        native.reverse();

        let mut dispatched: Vec<_> = live
            .iter()
            .chain(store.archived_sessions())
            .filter(|meta| {
                meta.is_dispatched() && meta.parent_session_id.as_deref() == Some(parent_id)
            })
            .collect();
        dispatched.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));
        let mut agents = Self {
            native,
            ..Self::default()
        };
        for meta in dispatched {
            let archived = meta.archived_at.is_some();
            // An archived agent has no reported activity; only its
            // settlement is known.
            let status = (!archived).then(|| store.agent_status(&meta.id)).flatten();
            let (execution, delivery) = match status {
                Some(status) => (Some(status.execution), status.delivery),
                // Nor does a settled one; an unsettled agent is running
                // until the host reports it.
                None => {
                    let delivery = AgentDelivery::of(meta, AgentExecution::Working);
                    let running = matches!(delivery, AgentDelivery::Running);
                    (running.then_some(AgentExecution::Working), delivery)
                }
            };
            let row = AgentRow {
                key: meta.id.clone(),
                title: meta.title.clone(),
                provider: Some(meta.provider),
                detail: detail(Some(meta.provider), meta.model.as_deref()),
                execution,
                delivery: Some(delivery),
                report: status.map_or_else(|| meta.report(), |status| status.report),
                archived,
                preview: None,
                open: Some(meta.id.clone()),
                agent: Some(meta.id.clone()),
                managed_by: None,
                agent_type: None,
                started_at: status.and_then(|status| status.run_started_at),
                ended_at: status.and_then(|status| status.run_completed_at),
            };
            match delivery {
                AgentDelivery::AwaitingSettle => agents.needs_settle.push(row),
                AgentDelivery::Running => agents.running.push(row),
                AgentDelivery::Settled | AgentDelivery::NotDelivered => agents.done.push(row),
            }
        }
        agents
    }

    /// Add each dispatched agent's result: the report its latest callback
    /// delivered to the lead, as the child wrote it.
    fn with_results(mut self, store: &WorkspaceStore) -> Self {
        let mut results = std::collections::HashMap::new();
        store.with_active_timeline(|timeline| {
            for entry in &timeline.entries {
                let Some(HostNotice::Orchestrate(callbacks)) =
                    crate::chat::model::user_notice(&entry.content)
                else {
                    continue;
                };
                for callback in callbacks {
                    let report = compact(callback.report().unwrap_or_default());
                    if !report.is_empty() {
                        results.insert(callback.child_id, report);
                    }
                }
            }
        });
        for row in self.needs_settle.iter_mut().chain(&mut self.done) {
            row.preview = results.remove(&row.key);
        }
        self
    }

    fn any_running(&self) -> bool {
        !self.running.is_empty()
            || self
                .native
                .iter()
                .any(|row| row.execution.is_some_and(AgentExecution::running))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.needs_settle.is_empty()
            && self.running.is_empty()
            && self.native.is_empty()
            && self.done.is_empty()
    }

    pub(crate) fn count(&self) -> usize {
        self.needs_settle.len() + self.running.len() + self.native.len() + self.done.len()
    }

    /// Dispatched agents still running and finished ones awaiting settle.
    pub(crate) fn outstanding(&self) -> usize {
        self.running.len() + self.needs_settle.len()
    }

    /// The cue on the panel toggle and the phone pill: running work first,
    /// then results awaiting settle.
    pub(crate) fn cue(&self, cx: &App) -> Option<Hsla> {
        if self.any_running() {
            Some(cx.theme().primary)
        } else if !self.needs_settle.is_empty() {
            Some(cx.theme().warning)
        } else {
            None
        }
    }

    fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        match self.running.len() {
            0 => {}
            1 => parts.push(crate::tr!("agents.summary_running_one").into_owned()),
            count => parts.push(crate::tr!("agents.summary_running", count = count).into_owned()),
        }
        match self.needs_settle.len() {
            0 => {}
            1 => parts.push(crate::tr!("agents.summary_unsettled_one").into_owned()),
            count => parts.push(crate::tr!("agents.summary_unsettled", count = count).into_owned()),
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

fn compact(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn detail(provider: Option<ProviderKind>, model: Option<&str>) -> String {
    [provider.map(|provider| provider.display_name()), model]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Open a thread reached from its lead. An archived agent is reached the way
/// Settings → Archived Threads reaches every archived thread: unarchived.
pub(crate) fn open_agent(store: &Entity<WorkspaceStore>, id: String, cx: &mut App) {
    store.update(cx, |store, _| {
        if store
            .archived_sessions()
            .iter()
            .any(|meta| meta.id == id && meta.archived_at.is_some())
        {
            store.unarchive_session(id.clone());
        }
        store.select_session(id);
    });
}

#[derive(Clone, Copy)]
enum BadgeStyle {
    Plain,
    Outline,
    Warning,
    Danger,
}

fn execution_color(execution: AgentExecution, cx: &App) -> Hsla {
    match execution {
        AgentExecution::Working => cx.theme().primary,
        AgentExecution::Waiting => cx.theme().warning,
        AgentExecution::Finished => cx.theme().success,
        AgentExecution::Failed => cx.theme().danger,
        AgentExecution::Cancelled | AgentExecution::Interrupted => cx.theme().muted_foreground,
    }
}

fn execution_label(execution: AgentExecution) -> SharedString {
    crate::tr!(match execution {
        AgentExecution::Working => "agents.state_working",
        AgentExecution::Waiting => "agents.state_waiting",
        AgentExecution::Finished => "agents.state_finished",
        AgentExecution::Failed => "agents.state_failed",
        AgentExecution::Cancelled => "agents.state_cancelled",
        AgentExecution::Interrupted => "agents.state_interrupted",
    })
    .into_owned()
    .into()
}

pub struct AgentsPanel {
    store: Entity<WorkspaceStore>,
    /// Whether the panel is on screen and so holds the store's archived
    /// threads, from which archived agents come.
    shown: bool,
    done_expanded: bool,
    /// The phone sheet this panel fills, dismissed when an agent opens.
    sheet: Option<WeakEntity<PopoverState>>,
    vscroll: ScrollHandle,
    /// Whether the last frame showed a running agent, whose elapsed time
    /// the tick advances.
    live: bool,
    tick: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AgentsPanel {
    /// The panel of the desktop right panel's Agents tab.
    pub fn new(store: Entity<WorkspaceStore>, cx: &mut Context<Self>) -> Self {
        let mut panel = Self::unshown(store.clone(), cx);
        panel
            ._subscriptions
            .push(cx.observe(&store, |this, store, cx| {
                let panel = store.read(cx).panel_state();
                this.set_shown(
                    panel.right_panel_open && panel.right_tab == RightTab::Agents,
                    cx,
                );
            }));
        let state = store.read(cx).panel_state();
        panel.set_shown(
            state.right_panel_open && state.right_tab == RightTab::Agents,
            cx,
        );
        panel
    }

    /// The panel of the phone's Agents sheet, shown through [`Self::set_shown`].
    pub(crate) fn for_sheet(store: Entity<WorkspaceStore>, cx: &mut Context<Self>) -> Self {
        Self::unshown(store, cx)
    }

    fn unshown(store: Entity<WorkspaceStore>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            observe_store_topics(
                &store,
                &[
                    TopicKind::ActiveSession,
                    TopicKind::SessionEvents,
                    TopicKind::Index,
                ],
                cx,
            ),
            cx.on_release(|this, cx| {
                if this.shown {
                    this.store
                        .update(cx, |store, _| store.release_archived_sessions());
                }
            }),
        ];
        Self {
            store,
            shown: false,
            done_expanded: false,
            sheet: None,
            vscroll: ScrollHandle::new(),
            live: false,
            tick: None,
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if shown == self.shown {
            return;
        }
        self.shown = shown;
        self.store.update(cx, |store, cx| {
            if shown {
                store.hold_archived_sessions(cx);
            } else {
                store.release_archived_sessions();
            }
        });
        self.tick = shown.then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let ticked = this.update(cx, |this, cx| {
                        if this.live {
                            cx.notify();
                        }
                    });
                    if ticked.is_err() {
                        break;
                    }
                }
            })
        });
    }

    pub(crate) fn in_sheet(&mut self, sheet: WeakEntity<PopoverState>) {
        self.sheet = Some(sheet);
    }

    fn on_open(&mut self, action: &AgentOpen, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sheet) = self.sheet.as_ref().and_then(WeakEntity::upgrade) {
            sheet.update(cx, |sheet, cx| sheet.dismiss(window, cx));
        }
        open_agent(&self.store, action.0.clone(), cx);
    }

    fn on_stop(&mut self, action: &AgentStop, _: &mut Window, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, _| store.interrupt_session(action.0.clone()));
    }

    fn on_settle(&mut self, action: &AgentSettle, _: &mut Window, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, _| store.settle_session(action.0.clone()));
    }

    fn on_cancel(&mut self, action: &AgentCancel, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let id = action.0.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            let id = id.clone();
            alert
                .title(crate::tr!("agents.cancel_title"))
                .description(crate::tr!("agents.cancel_description"))
                .button_props(
                    DialogButtons::default()
                        .ok_text(crate::tr!("agents.cancel"))
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text(crate::tr!("agents.cancel_keep"))
                        .show_cancel(true),
                )
                .on_ok(move |_, _, cx| {
                    store.update(cx, |store, _| store.cancel_agent(id.clone()));
                    true
                })
        });
    }

    /// The actions a row offers, with their icons and the action each sends.
    fn row_actions(row: &AgentRow) -> Vec<(IconName, SharedString, Box<dyn Action>)> {
        let mut actions: Vec<(IconName, SharedString, Box<dyn Action>)> = Vec::new();
        let Some(agent) = row.agent.clone().filter(|_| !row.archived) else {
            return actions;
        };
        match row.delivery {
            Some(AgentDelivery::Running) => {
                actions.push((
                    IconName::Square,
                    crate::tr!("agents.stop").into_owned().into(),
                    Box::new(AgentStop(agent.clone())),
                ));
                actions.push((
                    IconName::CircleX,
                    crate::tr!("agents.cancel").into_owned().into(),
                    Box::new(AgentCancel(agent)),
                ));
            }
            Some(AgentDelivery::AwaitingSettle) => {
                actions.push((
                    IconName::CircleCheck,
                    crate::tr!("agents.settle").into_owned().into(),
                    Box::new(AgentSettle(agent.clone())),
                ));
                actions.push((
                    IconName::CircleX,
                    crate::tr!("agents.cancel").into_owned().into(),
                    Box::new(AgentCancel(agent)),
                ));
            }
            _ => {}
        }
        actions
    }

    fn badge(label: SharedString, style: BadgeStyle, cx: &App) -> gpui::Div {
        let theme = cx.theme();
        let none = gpui::transparent_black();
        let (bg, fg) = match style {
            BadgeStyle::Plain | BadgeStyle::Outline => (none, theme.muted_foreground),
            BadgeStyle::Warning => (theme.warning.opacity(0.12), theme.warning),
            BadgeStyle::Danger => (theme.danger.opacity(0.12), theme.danger),
        };
        material::semantic_chip(label, bg, fg, cx)
            .flex()
            .gap_1()
            .items_center()
            .when(matches!(style, BadgeStyle::Outline), |badge| {
                badge.border_1().border_color(theme.border)
            })
    }

    fn delivery_badges(row: &AgentRow, cx: &App) -> Vec<AnyElement> {
        let label = |key: &str| SharedString::from(crate::tr!(key).into_owned());
        let mut badges = Vec::new();
        match row.delivery {
            Some(AgentDelivery::AwaitingSettle) => badges.push(
                Self::badge(label("agents.delivery_awaiting"), BadgeStyle::Warning, cx)
                    .into_any_element(),
            ),
            Some(AgentDelivery::Settled) => badges.push(
                Self::badge(label("agents.delivery_settled"), BadgeStyle::Plain, cx)
                    .child(Icon::new(IconName::Check))
                    .into_any_element(),
            ),
            Some(AgentDelivery::NotDelivered) => badges.push(
                Self::badge(
                    label("agents.delivery_not_delivered"),
                    BadgeStyle::Plain,
                    cx,
                )
                .into_any_element(),
            ),
            Some(AgentDelivery::Running) | None => {}
        }
        match row.report {
            AgentReport::Reported => badges.push(
                Self::badge(label("agents.report_reported"), BadgeStyle::Plain, cx)
                    .child(Icon::new(IconName::Check))
                    .into_any_element(),
            ),
            AgentReport::Unreported => badges.push(
                Self::badge(label("agents.report_unreported"), BadgeStyle::Warning, cx)
                    .into_any_element(),
            ),
            AgentReport::Failed => badges.push(
                Self::badge(label("agents.report_failed"), BadgeStyle::Danger, cx)
                    .into_any_element(),
            ),
            AgentReport::None | AgentReport::Expected => {}
        }
        if row.archived {
            badges.push(
                Self::badge(label("agents.archived"), BadgeStyle::Outline, cx).into_any_element(),
            );
        }
        badges
    }

    fn status_mark(row: &AgentRow, cx: &App) -> AnyElement {
        match row.execution {
            Some(AgentExecution::Working) => {
                Spinner::new().color(cx.theme().primary).into_any_element()
            }
            execution => crate::material::status_dot(
                execution.map_or(cx.theme().muted_foreground, |execution| {
                    execution_color(execution, cx)
                }),
            )
            .into_any_element(),
        }
    }

    fn render_row(&self, row: &AgentRow, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let group = SharedString::from(format!("agent-row-{}", row.key));
        let title = if row.title.trim().is_empty() {
            crate::tr!("agents.untitled").into_owned()
        } else {
            row.title.clone()
        };
        let actions = Self::row_actions(row);
        // Hover reveals the actions over the elapsed time; a touch screen has
        // no hover, so there they stand in its place.
        let coarse = crate::window_seam::is_mobile(cx);
        let inline_actions = !compact && !actions.is_empty();
        let elapsed = row.elapsed().filter(|_| !(inline_actions && coarse));
        let trailing = (inline_actions || elapsed.is_some()).then(|| {
            let cluster_width =
                actions.len() as f32 * 20. + actions.len().saturating_sub(1) as f32 * 2.;
            div()
                .relative()
                .flex_none()
                .h(px(20.))
                .when(inline_actions, |slot| slot.min_w(px(cluster_width)))
                .when_some(elapsed, |slot, elapsed| {
                    slot.child(
                        h_flex()
                            .h_full()
                            .justify_end()
                            .items_center()
                            .whitespace_nowrap()
                            .text_secondary(compact, cx)
                            .when(inline_actions, |time| {
                                time.group_hover(group.clone(), |time| time.invisible())
                            })
                            .child(elapsed),
                    )
                })
                .when(inline_actions, |slot| {
                    slot.child(
                        h_flex()
                            .absolute()
                            .right_0()
                            .top_0()
                            .gap_0p5()
                            // Opacity, not visibility, keeps the buttons tab
                            // stops so keyboard focus can reveal them.
                            .when(!coarse, |cluster| {
                                cluster
                                    .opacity(0.)
                                    .group_hover(group.clone(), |cluster| cluster.opacity(1.))
                            })
                            .children(actions.iter().enumerate().map(
                                |(index, (icon, label, action))| {
                                    let action = action.boxed_clone();
                                    Button::new(SharedString::from(format!(
                                        "agent-action-{}-{index}",
                                        row.key
                                    )))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(icon.clone()).text_color(muted))
                                    .aria_label(label.clone())
                                    .tooltip(label.clone())
                                    .focus_visible(|button| button.opacity(1.))
                                    .on_click(
                                        move |_, window, cx| {
                                            cx.stop_propagation();
                                            window.dispatch_action(action.boxed_clone(), cx);
                                        },
                                    )
                                },
                            )),
                    )
                })
        });
        let line_one = h_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .items_center()
            .text_body(compact, cx)
            .child(Self::status_mark(row, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(title.clone())),
            )
            .children(Self::delivery_badges(row, cx))
            .children(trailing);
        let line_two = h_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .items_center()
            .text_secondary(compact, cx)
            .when_some(row.provider, |line, provider| {
                line.child(
                    match provider {
                        ProviderKind::Acp => Icon::empty().path("icons/box.svg"),
                        kind => crate::provider_card::provider_glyph(kind),
                    }
                    .flex_none(),
                )
            })
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(row.detail.clone())),
            )
            .when_some(row.execution, |line, execution| {
                line.child(div().flex_none().child("·")).child(
                    div()
                        .flex_none()
                        .text_color(execution_color(execution, cx))
                        .child(execution_label(execution)),
                )
            });
        let content = v_flex()
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .child(line_one)
            .child(line_two)
            .when_some(row.preview.clone(), |content, preview| {
                content.child(
                    div()
                        .w_full()
                        .text_secondary(false, cx)
                        .line_clamp(2)
                        .overflow_hidden()
                        .text_color(if row.execution == Some(AgentExecution::Failed) {
                            cx.theme().danger
                        } else {
                            muted
                        })
                        .child(crate::widgets::fade_text(preview)),
                )
            });
        let label = SharedString::from(title);
        let open = row.open.clone();
        let readonly = row.managed_by.map(|provider| {
            let note = crate::tr!("agents.readonly", provider = provider).into_owned();
            match &row.agent_type {
                Some(agent_type) => format!("{agent_type}\n{note}"),
                None => note,
            }
        });
        let base = material::panel_row(
            SharedString::from(format!("agent-{}", row.key)),
            label,
            compact,
            cx,
        )
        .child(content)
        .when(compact && !actions.is_empty(), |row_el| {
            let actions: Vec<_> = row
                .open
                .iter()
                .map(|id| {
                    (
                        SharedString::from(crate::tr!("agents.open").into_owned()),
                        Box::new(AgentOpen(id.clone())) as Box<dyn Action>,
                    )
                })
                .chain(
                    actions
                        .iter()
                        .map(|(_, label, action)| (label.clone(), action.boxed_clone())),
                )
                .collect();
            row_el.child(
                material::toolbar_icon_button(
                    SharedString::from(format!("agent-more-{}", row.key)),
                    IconName::Ellipsis,
                    crate::tr!("mobile.more_actions"),
                    true,
                )
                .dropdown_menu(move |menu, _, _| {
                    actions.iter().fold(menu, |menu, (label, action)| {
                        menu.menu(label.clone(), action.boxed_clone())
                    })
                }),
            )
        });
        base.group(group)
            .debug_selector({
                let key = row.key.clone();
                move || format!("agent-row-{key}")
            })
            .when_some(open, |row_el, id| {
                row_el.cursor_pointer().on_click(move |_, window, cx| {
                    window.dispatch_action(Box::new(AgentOpen(id.clone())), cx);
                })
            })
            .when_some(readonly, |row_el, text| {
                row_el.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
            })
            .into_any_element()
    }

    fn render_empty(cx: &App) -> AnyElement {
        material::empty_state(
            Icon::new(IconName::Users),
            crate::tr!("agents.empty_title"),
            crate::tr!("agents.empty_desc"),
            cx,
        )
        .into_any_element()
    }
}

impl Render for AgentsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        let agents = {
            let store = self.store.read(cx);
            store
                .active_session_id()
                .map(|id| Agents::of(store, &id).with_results(store))
                .unwrap_or_default()
        };
        self.live = agents.any_running();
        let root = v_flex()
            .id("agents-panel")
            .size_full()
            .min_w_0()
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_stop))
            .on_action(cx.listener(Self::on_settle))
            .on_action(cx.listener(Self::on_cancel));
        if agents.is_empty() {
            return root.child(Self::render_empty(cx));
        }
        let inset = px(if compact {
            material::COMPACT_PAGE_INSET
        } else {
            material::CARD_INSET
        });
        let mut column = v_flex()
            .w_full()
            .min_w_0()
            .px(if compact {
                px(0.)
            } else {
                cx.theme().tokens.spacing.sm
            })
            .pb_3()
            .gap_0p5();
        let parent_id = self.store.read(cx).active_session_id();
        column = column.child(
            div()
                .px(inset)
                .py_2()
                .debug_selector(|| "agents-settle-all".into())
                .child(
                    Button::new("agents-settle-all")
                        .w_full()
                        .outline()
                        .small()
                        .label(crate::tr!("agents.settle_all"))
                        .touch(compact)
                        .disabled(agents.needs_settle.is_empty())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(id) = parent_id.clone() {
                                this.store.update(cx, |store, _| store.settle_children(id));
                            }
                        })),
                ),
        );
        if let Some(summary) = agents.summary() {
            column = column.child(
                div()
                    .px(inset)
                    .py_2()
                    .text_secondary(compact, cx)
                    .debug_selector(|| "agents-summary".into())
                    .child(summary),
            );
        }
        for row in agents.needs_settle.iter().chain(&agents.running) {
            column = column.child(self.render_row(row, compact, cx));
        }
        if !agents.native.is_empty() {
            column = column.child(material::list_caption(
                crate::tr!("agents.provider_subagents").into_owned().into(),
                compact,
                cx,
            ));
            for row in &agents.native {
                column = column.child(self.render_row(row, compact, cx));
            }
            if let Some(provider) = agents.native.first().and_then(|row| row.managed_by) {
                column = column.child(
                    div()
                        .px(inset)
                        .py_1()
                        .text_secondary(compact, cx)
                        .child(crate::tr!("agents.readonly", provider = provider)),
                );
            }
        }
        if !agents.done.is_empty() {
            let expanded = self.done_expanded;
            let label: SharedString = crate::tr!("agents.done", count = agents.done.len())
                .into_owned()
                .into();
            column = column.child(
                gpui_base::Button::new("agents-done")
                    .accessibility_label(label.clone())
                    .aria_expanded(expanded)
                    .w_full()
                    .h(px(material::bar_height(material::Bar::Section, compact)))
                    .px(inset)
                    .flex()
                    .items_center()
                    .rounded(material::radius_card(cx))
                    .when(!crate::window_seam::is_mobile(cx), |header| {
                        header.hover(|header| header.bg(cx.theme().list_hover))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.done_expanded = !this.done_expanded;
                        cx.notify();
                    }))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .items_center()
                            .text_caption(cx)
                            .child(Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            }))
                            .child(label),
                    ),
            );
            if expanded {
                for row in &agents.done {
                    column = column.child(self.render_row(row, compact, cx));
                }
            }
        }
        root.child(crate::scroll::page_viewport(
            "agents-scroll-bounce",
            &self.vscroll,
            div()
                .id("agents-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .lock_scroll_axis()
                .track_scroll(&self.vscroll)
                .child(column),
        ))
    }
}

#[cfg(test)]
mod tests {
    use agent::ProviderKind;
    use gpui::{AppContext as _, TestAppContext};
    use tcode_core::project::SessionMeta;
    use tcode_protocol::{EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic};

    use super::Agents;
    use crate::store::{WorkspaceAttachment, WorkspaceStore};

    /// A settled child is not resident, so the host reports no activity for
    /// it: it is done and the badge leaves it out, while an unsettled child
    /// the host has not reported yet is running.
    #[gpui::test]
    fn a_settled_child_without_reported_activity_is_done(cx: &mut TestAppContext) {
        let (to_host, _requests) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        let thread = |id: &str, parent: Option<&str>| {
            let mut meta = SessionMeta::new(ProviderKind::Codex, "/project".into(), None);
            meta.id = id.into();
            meta.parent_session_id = parent.map(str::to_string);
            meta
        };
        let mut settled = thread("settled", Some("lead"));
        settled.settled_at = Some(1);
        let index = tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(IndexSnapshot {
                projects: Vec::new(),
                sessions: vec![
                    thread("lead", None),
                    settled,
                    thread("unsettled", Some("lead")),
                ],
                summary: Default::default(),
            }),
        }))
        .unwrap();
        crate::store::tests::seed_full_scope(&store, &incoming, vec![index], cx);
        let mut pump = std::pin::pin!(link.pump());
        let mut task_cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(pump.as_mut(), &mut task_cx).is_pending());
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));

        store.read_with(cx, |store, _| {
            let agents = Agents::of(store, "lead");
            let keys = |rows: &[super::AgentRow]| {
                rows.iter().map(|row| row.key.clone()).collect::<Vec<_>>()
            };
            assert_eq!(keys(&agents.done), ["settled"]);
            assert_eq!(keys(&agents.running), ["unsettled"]);
            assert_eq!(agents.outstanding(), 1);
        });
        link.close();
    }

    #[gpui::test]
    fn settle_all_button_sends_the_lead_command(cx: &mut TestAppContext) {
        use gpui::{px, size};
        use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};
        use tcode_protocol::{
            AgentStatus, ClientPayload, Command, SessionActivity, decode_client_line,
        };
        cx.update(crate::theme::init);
        let (to_host, outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        let thread = |id: &str, parent: Option<&str>| {
            let mut meta = SessionMeta::new(ProviderKind::Codex, "/project".into(), None);
            meta.id = id.into();
            meta.title = id.into();
            meta.parent_session_id = parent.map(str::to_owned);
            meta
        };
        let mut index = IndexSnapshot {
            sessions: vec![thread("lead", None), thread("finished", Some("lead"))],
            projects: vec![],
            summary: Default::default(),
        };
        index.summary.activity.insert(
            "finished".into(),
            SessionActivity {
                agent: Some(AgentStatus {
                    execution: AgentExecution::Finished,
                    delivery: AgentDelivery::AwaitingSettle,
                    report: AgentReport::Reported,
                    run_started_at: None,
                    run_completed_at: None,
                }),
                working: false,
                turn_running: false,
                waiting: false,
                waiting_for_approval: false,
                waiting_for_input: false,
                failed: false,
                unread: false,
                fork: tcode_protocol::ForkAvailability::Available,
            },
        );
        let index = tcode_protocol::encode_line(&HostMessage::Event(EventEnvelope {
            request_id: None,
            topic: Topic::Index,
            event: ServerEvent::IndexSnapshot(index),
        }))
        .unwrap();
        crate::store::tests::seed_full_scope(&store, &incoming, vec![index], cx);
        let mut driver = crate::store::tests::LocalHostDriver::from_link(link.clone());
        driver.wait_until(cx, &store, "child status", |cx| {
            store.read_with(cx, |store, _| store.agent_status("finished").is_some())
        });
        store.update(cx, |store, _| store.select_session("lead".into()));
        let (_, cx) = cx.add_window_view(|window, cx| {
            let panel = cx.new(|cx| super::AgentsPanel::for_sheet(store.clone(), cx));
            gpui_base::Root::new(panel, window, cx)
        });
        cx.simulate_resize(size(px(393.), px(852.)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        let button = cx
            .debug_bounds("agents-settle-all")
            .expect("settle-all button is drawn");
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        let mut commands = Vec::new();
        driver.wait_until(cx, &store, "settle-all command", |_| {
            while let Ok(line) = outgoing.try_recv() {
                if let ClientPayload::Command(command) = decode_client_line(&line).unwrap().payload
                {
                    commands.push(command);
                }
            }
            commands
                .iter()
                .any(|command| matches!(command, Command::SettleChildren { .. }))
        });
        assert_eq!(
            commands
                .into_iter()
                .filter(|command| matches!(command, Command::SettleChildren { .. }))
                .collect::<Vec<_>>(),
            vec![Command::SettleChildren {
                session_id: "lead".into()
            }]
        );
        link.close();
    }
}
