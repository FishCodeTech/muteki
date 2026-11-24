//! Command palette with action, thread-title and message-content search.
//!
//! Rendered by [`crate::AppShell`] as a full-window overlay only while
//! [`crate::WindowState::palette_open`] is set. Sources:
//! - Threads: fuzzy match over session titles (enter opens the thread).
//! - Messages: debounced full-text search over persisted conversation events.
//! - Actions: "New thread…" per project, "Open settings", "Toggle theme",
//!   "Toggle diff panel".
//!
//! Title and action search use [`fuzzy_score`]; message search runs through the host.

use std::rc::Rc;
use std::time::Duration;

use crate::icon::{Icon, IconName};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::input::{Input, InputEvent, InputState};
use agent::ProviderKind;
use gpui::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, ListAlignment, ListState, ParentElement as _, Render, Role,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, list,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use tcode_protocol::{SessionSearchHit, ThreadExportFormat};

use crate::provider_card::provider_glyph;
use crate::settings::ThemeMode;
use crate::settings_page::apply_theme;
use crate::store::{StoreChange, TopicKind, WorkspaceStore};
use crate::time::{humanize_ago, now_secs};
use crate::window_state::WindowState;

/// How far below the window's top the desktop palette hangs. See the host's
/// note in [`CommandPalette`]'s render on why it does not use `overlay::Dialog`.
const DESKTOP_TOP_OFFSET: f32 = 96.;

/// Score `text` against a fuzzy `query` (case-insensitive subsequence match).
/// Returns `None` when `query` is not a subsequence of `text`; a higher score
/// is a better match (consecutive and earlier hits score more). An empty query
/// matches everything with score 0.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut qi = 0usize;
    let mut score = 0i32;
    let mut last: Option<usize> = None;
    for (ti, tc) in t.iter().enumerate() {
        if qi < q.len() && *tc == q[qi] {
            match last {
                Some(prev) if ti == prev + 1 => score += 10, // consecutive
                Some(_) => score += 1,
                None => score += 5 - (ti.min(5) as i32), // earlier first hit
            }
            last = Some(ti);
            qi += 1;
        }
    }
    (qi == q.len()).then_some(score)
}

/// A concrete action a palette row triggers.
#[derive(Clone)]
enum Action {
    ChangeProjectIcon(tcode_core::project::Project),
    NewThread {
        cwd: std::path::PathBuf,
        project_id: String,
    },
    QuickChat,
    OpenSettings,
    ToggleTheme,
    ToggleDiff,
    ToggleTerminal,
    OpenPreview,
    CheckUpdates,
    MergeWorktree {
        session_id: String,
    },
    ExportThread {
        session_id: String,
        cwd: std::path::PathBuf,
        format: ThreadExportFormat,
    },
    OpenThread {
        session_id: String,
        turn: Option<usize>,
    },
}

/// One rendered palette row.
#[derive(Clone)]
struct Item {
    icon: IconName,
    label: String,
    /// Optional muted subtitle (e.g. a thread's project).
    subtitle: Option<String>,
    /// Thread rows carry their provider glyph + last-activity time (right side).
    provider: Option<ProviderKind>,
    updated_at: Option<u64>,
    action: Action,
}

struct Group {
    label: String,
    items: Vec<Item>,
}

enum Row {
    Caption(String),
    /// Index into [`Results::items`].
    Item(usize),
}

/// The grouped results flattened into list rows. Scroll frames reuse it; the
/// query, content hits and store index rebuild it.
struct Results {
    rows: Vec<Row>,
    items: Vec<Item>,
}

pub struct CommandPalette {
    store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    query: Entity<InputState>,
    focus_handle: FocusHandle,
    selected: usize,
    pub(crate) outside_dismissal: crate::overlay::OutsideDismissal,
    content_hits: Vec<SessionSearchHit>,
    search_generation: u64,
    results: Option<Rc<Results>>,
    list_state: ListState,
    _search_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl CommandPalette {
    pub fn new(
        store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx));

        let subscriptions = vec![
            cx.subscribe(&store, |this, _, change: &StoreChange, cx| {
                if matches!(
                    change.topic,
                    TopicKind::Index | TopicKind::Settings | TopicKind::ActiveSession
                ) {
                    this.invalidate_results(false, cx);
                }
            }),
            cx.subscribe_in(
                &query,
                window,
                |this, _query, event, window, cx| match event {
                    InputEvent::Change => {
                        this.selected = 0;
                        this.schedule_content_search(cx);
                        this.invalidate_results(true, cx);
                    }
                    InputEvent::PressEnter { .. } => {
                        this.activate_selected(window, cx);
                    }
                    _ => {}
                },
            ),
        ];

        Self {
            store,
            window_state,
            query,
            focus_handle: cx.focus_handle(),
            selected: 0,
            outside_dismissal: crate::overlay::OutsideDismissal::default(),
            content_hits: Vec::new(),
            search_generation: 0,
            results: None,
            list_state: ListState::new(0, ListAlignment::Top, px(120.)),
            _search_task: None,
            _subscriptions: subscriptions,
        }
    }

    /// Reset the palette as it opens. With `focus_query` the search input
    /// takes focus; without it the overlay does, so Escape still closes the
    /// palette but no software keyboard is raised.
    pub fn open(&mut self, focus_query: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |state, cx| {
            state.set_value(String::new(), window, cx);
            if focus_query {
                state.focus(window, cx);
            }
        });
        if !focus_query {
            self.focus_handle.focus(window, cx);
        }
        self.selected = 0;
        self.content_hits.clear();
        self.invalidate_results(true, cx);
    }

    #[cfg(test)]
    pub(crate) fn query_focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }

    /// Debounce, then ask the host for content hits. The index, cache and
    /// session metadata are host-owned; this keeps only the debounce and the
    /// generation guard that discards an answer a newer query superseded.
    fn schedule_content_search(&mut self, cx: &mut Context<Self>) {
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.content_hits.clear();

        let raw = self.query.read(cx).value().to_string();
        let query = raw.trim();
        if query.is_empty() || query.starts_with('>') {
            self._search_task = None;
            return;
        }
        let query = query.to_string();
        let store = self.store.clone();
        self._search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let hits = store
                .update(cx, |store, cx| store.search_session_content(query, 50, cx))
                .await;
            let _ = this.update(cx, |palette, cx| {
                palette.apply_content_hits(generation, hits, cx);
            });
        }));
    }

    /// Adopt an answer only while it still belongs to the current query. The
    /// host may answer an earlier keystroke after a later one.
    fn apply_content_hits(
        &mut self,
        generation: u64,
        hits: Vec<SessionSearchHit>,
        cx: &mut Context<Self>,
    ) {
        if self.search_generation != generation {
            return;
        }
        self.content_hits = hits;
        self.invalidate_results(false, cx);
    }

    /// A new query starts the list at the top; a store or content-hit update
    /// keeps the rows the user scrolled to.
    fn invalidate_results(&mut self, scroll_to_top: bool, cx: &mut Context<Self>) {
        self.results = None;
        if scroll_to_top {
            self.list_state.scroll_to(gpui::ListOffset::default());
        }
        cx.notify();
    }

    fn results(&mut self, cx: &Context<Self>) -> Rc<Results> {
        if let Some(results) = &self.results {
            return results.clone();
        }
        let mut rows = Vec::new();
        let mut items = Vec::new();
        for group in self.groups(cx) {
            rows.push(Row::Caption(group.label));
            for item in group.items {
                rows.push(Row::Item(items.len()));
                items.push(item);
            }
        }
        let top = self.list_state.logical_scroll_top();
        self.list_state
            .splice(0..self.list_state.item_count(), rows.len());
        if top.item_ix < rows.len() {
            self.list_state.scroll_to(top);
        }
        let results = Rc::new(Results { rows, items });
        self.results = Some(results.clone());
        results
    }

    fn close(&self, cx: &mut Context<Self>) {
        self.window_state
            .update(cx, |state, cx| state.close_palette(cx));
    }

    /// Build the grouped result list for the current query. A leading `>`
    /// restricts results to Actions.
    fn groups(&self, cx: &Context<Self>) -> Vec<Group> {
        let raw = self.query.read(cx).value().to_string();
        let actions_only = raw.trim_start().starts_with('>');
        // Strip the `>` prefix so the remainder still fuzzy-matches action labels.
        let query = if actions_only {
            raw.trim_start()
                .trim_start_matches('>')
                .trim_start()
                .to_string()
        } else {
            raw
        };
        let store = self.store.read(cx);

        let mut actions: Vec<(i32, Item)> = Vec::new();
        let mut push_action = |label: String, icon: IconName, action: Action| {
            if let Some(score) = fuzzy_score(&query, &label) {
                actions.push((
                    score,
                    Item {
                        icon,
                        label,
                        subtitle: None,
                        provider: None,
                        updated_at: None,
                        action,
                    },
                ));
            }
        };
        if store.scope().is_full() {
            push_action(
                crate::tr!("sidebar.quick_chat").into_owned(),
                IconName::Plus,
                Action::QuickChat,
            );
        }
        for project in store.projects() {
            push_action(
                crate::tr!("palette.new_thread", project = project.name.clone()).into_owned(),
                IconName::Plus,
                Action::NewThread {
                    cwd: project.root.clone(),
                    project_id: project.id.clone(),
                },
            );
            if store.scope().is_full() {
                push_action(
                    crate::tr!("project_icon.action", project = project.name.clone()).into_owned(),
                    IconName::Folder,
                    Action::ChangeProjectIcon(project),
                );
            }
        }
        push_action(
            crate::tr!("palette.open_settings").into_owned(),
            IconName::Settings,
            Action::OpenSettings,
        );
        push_action(
            crate::tr!("palette.toggle_theme").into_owned(),
            IconName::Moon,
            Action::ToggleTheme,
        );
        push_action(
            crate::tr!("palette.toggle_diff").into_owned(),
            IconName::PanelRight,
            Action::ToggleDiff,
        );
        push_action(
            crate::tr!("palette.toggle_terminal").into_owned(),
            IconName::SquareTerminal,
            Action::ToggleTerminal,
        );
        if store.offers_preview() {
            push_action(
                crate::tr!("palette.open_preview").into_owned(),
                IconName::Globe,
                Action::OpenPreview,
            );
        }
        if store.scope().is_full() {
            push_action(
                crate::tr!("palette.check_updates").into_owned(),
                IconName::Inbox,
                Action::CheckUpdates,
            );
        }
        if let Some(session_id) = store.active_session_id()
            && let Some(meta) = store
                .sidebar_sessions()
                .into_iter()
                .find(|meta| meta.id == session_id)
        {
            if meta.worktree.is_some() {
                push_action(
                    crate::tr!("palette.merge_worktree").into_owned(),
                    IconName::Inbox,
                    Action::MergeWorktree {
                        session_id: meta.id.clone(),
                    },
                );
            }
            for (label, format) in [
                (
                    crate::tr!("palette.export_jsonl").into_owned(),
                    ThreadExportFormat::Jsonl,
                ),
                (
                    crate::tr!("palette.export_markdown").into_owned(),
                    ThreadExportFormat::Markdown,
                ),
            ] {
                push_action(
                    label,
                    IconName::Inbox,
                    Action::ExportThread {
                        session_id: meta.id.clone(),
                        cwd: meta.cwd.clone(),
                        format,
                    },
                );
            }
        }
        actions.sort_by_key(|b| std::cmp::Reverse(b.0));

        let mut groups = Vec::new();
        if !actions.is_empty() {
            groups.push(Group {
                label: crate::tr!("palette.actions").into_owned(),
                items: actions.into_iter().map(|(_, i)| i).collect(),
            });
        }

        // Threads (fuzzy over titles) — suppressed in `>`-actions-only mode.
        if !actions_only {
            let mut threads: Vec<(i32, Item)> = Vec::new();
            for group in store.grouped_sessions() {
                for meta in &group.sessions {
                    if let Some(score) = fuzzy_score(&query, &meta.title) {
                        threads.push((
                            score,
                            Item {
                                icon: IconName::SquareTerminal,
                                label: meta.title.clone(),
                                subtitle: Some(crate::project_icon::label(&group.project)),
                                provider: Some(meta.provider),
                                updated_at: Some(meta.updated_at),
                                action: Action::OpenThread {
                                    session_id: meta.id.clone(),
                                    turn: None,
                                },
                            },
                        ));
                    }
                }
            }
            threads.sort_by_key(|b| std::cmp::Reverse(b.0));
            if !threads.is_empty() {
                groups.push(Group {
                    label: crate::tr!("palette.threads").into_owned(),
                    items: threads.into_iter().map(|(_, i)| i).collect(),
                });
            }

            if !query.trim().is_empty() && !self.content_hits.is_empty() {
                let sessions = store.sidebar_sessions();
                let messages = self
                    .content_hits
                    .iter()
                    .map(|hit| {
                        let meta = sessions.iter().find(|meta| meta.id == hit.session_id);
                        Item {
                            icon: IconName::Search,
                            label: hit.session_title.clone(),
                            subtitle: Some(hit.snippet.clone()),
                            provider: meta.map(|meta| meta.provider),
                            updated_at: meta.map(|meta| meta.updated_at),
                            action: Action::OpenThread {
                                session_id: hit.session_id.clone(),
                                turn: Some(hit.turn),
                            },
                        }
                    })
                    .collect();
                groups.push(Group {
                    label: crate::tr!("palette.messages").into_owned(),
                    items: messages,
                });
            }
        }
        groups
    }

    fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let results = self.results(cx);
        if let Some(item) = results.items.get(self.selected) {
            self.activate(item.action.clone(), window, cx);
        }
    }

    fn activate(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Action::ChangeProjectIcon(project) => {
                self.close(cx);
                crate::project_icon::open(self.store.clone(), project, window, cx);
            }
            Action::NewThread { cwd, project_id } => {
                self.close(cx);
                self.store.update(cx, |store, cx| {
                    store.start_draft(project_id, cwd, cx);
                });
                // Draft selection arrives asynchronously and can reuse an
                // already selected draft. Navigation is a separate intent.
                self.window_state
                    .update(cx, |state, cx| state.open_thread(cx));
            }
            Action::QuickChat => {
                self.close(cx);
                self.store
                    .update(cx, |store, cx| store.start_scratch_draft(cx));
                self.window_state
                    .update(cx, |state, cx| state.open_thread(cx));
            }
            Action::OpenSettings => {
                // open_settings also clears palette_open.
                self.window_state
                    .update(cx, |state, cx| state.open_settings(cx));
            }
            Action::ToggleTheme => {
                let next = if cx.theme().mode.is_dark() {
                    ThemeMode::Light
                } else {
                    ThemeMode::Dark
                };
                self.store.update(cx, |store, _cx| {
                    store.set_client_theme(Some(next));
                });
                apply_theme(next, window, cx);
                self.close(cx);
            }
            Action::ToggleDiff => {
                self.store
                    .update(cx, |store, cx| store.toggle_diff_panel(cx));
                self.close(cx);
            }
            Action::ToggleTerminal => {
                self.store
                    .update(cx, |store, cx| store.toggle_terminal_panel(cx));
                self.close(cx);
            }
            Action::OpenPreview => {
                self.store
                    .update(cx, |store, cx| store.open_preview_panel(cx));
                self.close(cx);
            }
            Action::CheckUpdates => {
                self.store
                    .update(cx, |store, _cx| store.check_provider_versions());
                self.close(cx);
            }
            Action::MergeWorktree { session_id } => {
                self.store.update(cx, |store, _cx| {
                    store.merge_worktree(session_id);
                });
                self.close(cx);
            }
            Action::ExportThread {
                session_id,
                cwd,
                format,
            } => {
                self.close(cx);
                crate::thread_export::prompt_thread_export(
                    self.store.clone(),
                    session_id,
                    cwd,
                    format,
                    window,
                    cx,
                );
            }
            Action::OpenThread { session_id, turn } => {
                self.store.update(cx, |store, cx| {
                    if let Some(turn) = turn {
                        store.select_session_at_turn(session_id, turn);
                        cx.notify();
                    } else {
                        store.select_session(session_id);
                    }
                });
                self.close(cx);
                self.window_state
                    .update(cx, |state, cx| state.open_thread(cx));
            }
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let results = self.results(cx);
        let total = results.items.len();
        let selected = match ev.keystroke.key.as_str() {
            "down" if total > 0 => (self.selected + 1).min(total - 1),
            "down" => self.selected,
            "up" => self.selected.saturating_sub(1),
            _ => return,
        };
        cx.stop_propagation();
        self.selected = selected;
        if let Some(row) = results
            .rows
            .iter()
            .position(|row| matches!(row, Row::Item(index) if *index == selected))
        {
            self.list_state.scroll_to_reveal_item(row);
        }
        cx.notify();
    }
}

impl CommandPalette {
    fn render_row(
        &self,
        row: &Row,
        items: &[Item],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let index = match row {
            Row::Caption(label) => {
                return div()
                    .px_2()
                    .pt_1()
                    .pb_1()
                    .text_caption(cx)
                    .child(label.clone())
                    .into_any_element();
            }
            Row::Item(index) => *index,
        };
        let item = &items[index];
        let is_sel = index == self.selected;
        let action = item.action.clone();
        // Every result takes the two-line row, so the list's uniform height
        // hint holds whether or not a result has a subtitle.
        let row = crate::material::selectable_row_two_line(
            h_flex().id(("palette-row", index)),
            is_sel,
            compact,
            cx,
        );
        div()
            .pb_1()
            .child(
                row.debug_selector({
                    let action = action.clone();
                    move || match &action {
                        Action::NewThread { project_id, .. } => {
                            format!("palette-new-thread-{project_id}")
                        }
                        _ => format!("palette-row-{index}"),
                    }
                })
                .role(Role::ListBoxOption)
                .aria_label(item.label.clone())
                .aria_selected(is_sel)
                .when(is_sel, |row| row.aria_active_descendant())
                .flex_none()
                .w_full()
                .cursor_pointer()
                .child(match &item.action {
                    Action::ChangeProjectIcon(project) => {
                        crate::project_icon::artwork(project, 16.).into_any_element()
                    }
                    Action::NewThread { project_id, .. } => self
                        .store
                        .read(cx)
                        .project(project_id)
                        .map(|project| {
                            crate::project_icon::artwork(project, 16.).into_any_element()
                        })
                        .unwrap_or_else(|| {
                            Icon::new(item.icon.clone())
                                .text_color(muted)
                                .into_any_element()
                        }),
                    _ => Icon::new(item.icon.clone())
                        .text_color(muted)
                        .into_any_element(),
                })
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_body(compact, cx)
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(item.label.clone())),
                        )
                        .when_some(item.subtitle.clone(), |this, sub| {
                            this.child(
                                div()
                                    .text_secondary(compact, cx)
                                    .overflow_hidden()
                                    .child(crate::widgets::fade_text(sub)),
                            )
                        }),
                )
                .when_some(item.provider, |this, provider| {
                    this.child(
                        h_flex()
                            .flex_none()
                            .gap_1p5()
                            .items_center()
                            .text_secondary(compact, cx)
                            .when_some(item.updated_at, |this, at| {
                                let ago = now_secs().saturating_sub(at);
                                this.child(div().child(humanize_ago(ago)))
                            })
                            .child(provider_glyph(provider)),
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.activate(action.clone(), window, cx);
                })),
            )
            .into_any_element()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = self.window_state.read(cx).compact;
        let open = self.window_state.read(cx).palette_open;
        let presence = gpui_base::motion::Presence::new("palette-presence", open)
            .transition(gpui_base::motion::Transition::new(crate::edge_back::SETTLE))
            .sample(window, cx);
        if !open && (!compact || !presence.should_render() || presence.progress == 0.) {
            return div().into_any_element();
        }
        let results = self.results(cx);
        let total = results.items.len();
        if total > 0 && self.selected >= total {
            self.selected = total - 1;
        }
        let muted = cx.theme().muted_foreground;
        let viewport = window.viewport_size();
        let insets = crate::window_seam::content_insets(window);
        // A sheet stops a window strip short of the top; the desktop card hangs
        // `DESKTOP_TOP_OFFSET` down and keeps a section gap clear below.
        let reserve = if compact {
            px(crate::window_caption::CAPTION_STRIP_HEIGHT)
        } else {
            px(DESKTOP_TOP_OFFSET) + crate::material::section_gap(cx)
        };
        let available = (viewport.height - insets.top - insets.bottom - reserve).max(px(0.));
        let height = px(crate::overlay::DIALOG_LIST_HEIGHT).min(available);
        let dismissal = self.outside_dismissal.clone();

        let body = if total == 0 {
            crate::material::empty_row(crate::tr!("palette.no_matches"), cx)
                .flex_1()
                .min_h_0()
                .into_any_element()
        } else {
            crate::scroll::page_viewport(
                "palette-list-bounce",
                &self.list_state,
                div()
                    .id("palette-list")
                    .debug_selector(|| "palette-list".into())
                    .role(Role::ListBox)
                    .aria_label(crate::tr!("palette.results"))
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .px(if compact {
                        px(crate::material::COMPACT_PAGE_INSET)
                    } else {
                        cx.theme().tokens.spacing.sm
                    })
                    .py_2()
                    .child(
                        list(
                            self.list_state.clone(),
                            cx.processor(move |this, ix: usize, _, cx| {
                                this.render_row(&results.rows[ix], &results.items, compact, cx)
                            }),
                        )
                        .size_full(),
                    )
                    // material::selectable_row_two_line's height, which it
                    // does not expose, and the row's bottom gap.
                    .child(crate::scroll::list_height_hint(
                        &self.list_state,
                        px(crate::material::selectable_row_two_line_height(compact))
                            + cx.theme().tokens.spacing.xs,
                    )),
            )
            .into_any_element()
        };

        let surface = v_flex().h(height).overflow_hidden();
        let card = if compact {
            crate::material::sheet_contour(surface.w(viewport.width), cx)
        } else {
            crate::material::overlay_contour(
                surface
                    .w(crate::sizing::fit_viewport(640., viewport.width))
                    .rounded(crate::material::radius_overlay(cx)),
                cx,
            )
        }
        .child(
            crate::material::bar("palette-header", crate::material::Bar::Toolbar, compact, cx)
                .items_center()
                .child(Icon::new(IconName::Search).text_color(muted))
                .child(
                    div().flex_1().child(
                        Input::new(&self.query)
                            .placeholder(crate::tr!("palette.placeholder"))
                            .appearance(false),
                    ),
                ),
        )
        .child(body)
        .when(!compact, |card| {
            card.child(
                crate::material::bar("palette-footer", crate::material::Bar::Section, false, cx)
                    .items_center()
                    .gap(cx.theme().tokens.spacing.md)
                    .text_secondary(false, cx)
                    .child(crate::tr!("palette.navigate"))
                    .child(crate::tr!("palette.select"))
                    .child(crate::tr!("palette.close")),
            )
        });

        let card = card
            .id("palette-card")
            .debug_selector(|| "palette-card".into())
            .occlude()
            // The palette's own focus sits on the card, under the dialog
            // host, so the list keys reach it from the host and from the
            // query input alike.
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            // Pressing the card must not move query focus or arm text selection.
            .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            });
        let palette = cx.entity();
        // overlay::Dialog centres a form at h/10 and pads it; the palette owns
        // its list viewport and anchors to the keyboard-safe edge, so it hosts
        // itself on gpui-base's Sheet and Dialog.
        if compact {
            let close = palette.clone();
            let binding = crate::overlay::bind_sheet(
                "command-palette".into(),
                self.focus_handle.clone(),
                open,
                move |_, cx| close.update(cx, |palette, cx| palette.close(cx)),
                window,
                cx,
            );
            let drag = binding.read(cx);
            let progress = crate::edge_back::settle(
                "palette-edge-offset",
                drag.progress,
                drag.dragging,
                window,
                cx,
            );
            let progress = if open {
                progress
            } else {
                progress.max(1. - presence.progress)
            };
            return gpui_base::Sheet::new(cx)
                .focus_handle(self.focus_handle.clone())
                .request_close(move |_, cx| {
                    dismissal.dismissed();
                    palette.update(cx, |palette, cx| palette.close(cx));
                })
                .overlay(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(crate::material::scrim(1. - progress, cx)),
                )
                .surface(
                    div()
                        .absolute()
                        .left_0()
                        .bottom(
                            insets.bottom
                                - crate::edge_back::BackProgress::offset(progress, height),
                        )
                        .child(card),
                )
                .into_any_element();
        }
        gpui_base::Dialog::new(cx)
            // gpui-base's backdrop parent dismisses but does not block; the scrim stops wheels without occluding it.
            .backdrop(
                div()
                    .absolute()
                    .size_full()
                    .bg(crate::material::scrim(1., cx))
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation()),
            )
            .popup(card)
            // Enter belongs to the query input, which runs the selected item.
            .on_ok(|_, _, _| false)
            .request_close(move |_, _, cx| {
                dismissal.dismissed();
                palette.update(cx, |this, cx| this.close(cx));
            })
            .flex_col()
            .items_center()
            .when(compact, |host| host.justify_end().pb(insets.bottom))
            .when(!compact, |host| {
                host.justify_start().pt(insets.top + px(DESKTOP_TOP_OFFSET))
            })
            .into_any_element()
    }
}

impl Focusable for CommandPalette {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use tcode_runtime::pipe::{HostServices, spawn_host};
    use tcode_services::store::SessionStore;

    struct PaletteHarness {
        palette: Entity<CommandPalette>,
        window_state: Entity<WindowState>,
        page_scroll: gpui::ScrollHandle,
    }

    impl PaletteHarness {
        fn new(store: Entity<WorkspaceStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
            let window_state = cx.new(|_| WindowState::new(false));
            Self {
                palette: cx.new(|cx| CommandPalette::new(store, window_state.clone(), window, cx)),
                window_state,
                page_scroll: gpui::ScrollHandle::new(),
            }
        }
    }

    impl Render for PaletteHarness {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let open = self.window_state.read(cx).palette_open;
            div()
                .relative()
                .size_full()
                .child(
                    div()
                        .id("page")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.page_scroll)
                        .child(div().h(px(3000.)).w_full().flex_none()),
                )
                .when(open, |root| root.child(self.palette.clone()))
        }
    }

    /// Draws a full frame. A frame that reuses any view keeps every debug
    /// bound of the previous one, so a row scrolled away would still be found.
    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            _ = window.draw(cx);
        });
    }

    fn dispatch_palette_key(
        palette: &Entity<CommandPalette>,
        cx: &mut VisualTestContext,
        key: &str,
    ) {
        let event = KeyDownEvent {
            keystroke: gpui::Keystroke::parse(key).expect("valid palette key"),
            is_held: false,
            prefer_character_input: false,
        };
        cx.update(|window, cx| {
            palette.update(cx, |palette, cx| {
                palette.on_key_down(&event, window, cx);
            });
        });
    }

    #[test]
    fn fuzzy_search_matches_subsequences_and_prefers_early_contiguous_hits() {
        for (query, text, matches) in [
            ("", "", true),
            ("", "anything", true),
            ("x", "", false),
            ("xyz", "New thread", false),
            ("ttt", "cat", false),
            ("nt", "New thread", true),
            ("NEW", "new thread", true),
            ("é文", "Éditer 文档", true),
            ("文é", "Éditer 文档", false),
        ] {
            assert_eq!(
                fuzzy_score(query, text).is_some(),
                matches,
                "{query:?} in {text:?}"
            );
        }
        let contiguous = fuzzy_score("set", "settings").unwrap();
        let scattered = fuzzy_score("set", "some effort table").unwrap();
        assert!(contiguous > scattered, "{contiguous} !> {scattered}");
        assert!(contiguous > fuzzy_score("set", "reset").unwrap());
    }

    #[gpui::test]
    fn empty_query_lists_commands_and_arrow_keys_keep_query_focus(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("palette-keyboard-test");
        let store = SessionStore::open_at(root.clone()).expect("open test store");
        let host = spawn_host(store, HostServices::default()).expect("spawn test host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let workspace_store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &workspace_store, "workspace baseline", |cx| {
            workspace_store.read_with(cx, |store, _| store.baseline_ready())
        });
        let palette_store = workspace_store.clone();
        let (harness, cx) = cx.add_window_view(move |window, cx| {
            PaletteHarness::new(palette_store.clone(), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        let palette = cx.update(|_, cx| harness.read(cx).palette.clone());
        cx.update(|window, cx| {
            let query = palette.read(cx).query.clone();
            query.read(cx).focus_handle(cx).focus(window, cx);
        });

        cx.update(|_, cx| {
            palette.update(cx, |palette, cx| {
                assert!(palette.query.read(cx).value().is_empty());
                let items = &palette.results(cx).items;
                assert!(
                    items
                        .iter()
                        .any(|item| matches!(item.action, Action::OpenSettings))
                );
                assert!(
                    items
                        .iter()
                        .any(|item| matches!(item.action, Action::ToggleTheme))
                );
                assert!(
                    items
                        .iter()
                        .any(|item| matches!(item.action, Action::ToggleTerminal))
                );
            });
        });

        dispatch_palette_key(&palette, cx, "down");
        dispatch_palette_key(&palette, cx, "down");
        cx.update(|window, cx| {
            let palette = palette.read(cx);
            assert_eq!(palette.selected, 2);
            assert!(palette.query.read(cx).focus_handle(cx).is_focused(window));
        });

        dispatch_palette_key(&palette, cx, "up");
        dispatch_palette_key(&palette, cx, "up");
        dispatch_palette_key(&palette, cx, "up");
        cx.update(|_, cx| assert_eq!(palette.read(cx).selected, 0));

        for _ in 0..10 {
            dispatch_palette_key(&palette, cx, "down");
        }
        cx.update(|_, cx| {
            let total = palette.update(cx, |palette, cx| palette.results(cx).items.len());
            assert_eq!(palette.read(cx).selected, total - 1);
        });

        drop(palette);
        driver.shutdown(cx, &workspace_store, &host);
        drop(workspace_store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn switching_language_relocalizes_the_query_placeholder(cx: &mut TestAppContext) {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        crate::settings::apply_locale(Some(crate::LANGUAGE_SIMPLIFIED_CHINESE));
        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        let (harness, cx) =
            cx.add_window_view(move |window, cx| PaletteHarness::new(store, window, cx));
        let (palette, window_state) = harness.read_with(cx, |harness, _| {
            (harness.palette.clone(), harness.window_state.clone())
        });
        window_state.update(cx, |state, cx| {
            state.palette_open = true;
            cx.notify();
        });
        crate::settings::assert_follows_language_switch(cx, "palette.placeholder", |cx| {
            palette.read_with(cx, |palette, cx| {
                let query = palette.query.read(cx);
                query.presentation().placeholder().to_string()
            })
        });
    }

    #[gpui::test]
    fn escape_from_the_query_and_a_backdrop_press_close_the_palette(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("palette-dismiss-test");
        let store = SessionStore::open_at(root.clone()).expect("open test store");
        let host = spawn_host(store, HostServices::default()).expect("spawn test host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let workspace_store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &workspace_store, "workspace baseline", |cx| {
            workspace_store.read_with(cx, |store, _| store.baseline_ready())
        });
        let palette_store = workspace_store.clone();
        let (harness, cx) = cx.add_window_view(move |window, cx| {
            PaletteHarness::new(palette_store.clone(), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        let (palette, window_state) = cx.update(|_, cx| {
            let harness = harness.read(cx);
            (harness.palette.clone(), harness.window_state.clone())
        });
        let open = |cx: &mut VisualTestContext, focus_query: bool| {
            cx.update(|window, cx| {
                window_state.update(cx, |state, cx| state.open_palette(cx));
                palette.update(cx, |palette, cx| palette.open(focus_query, window, cx));
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            assert!(cx.debug_bounds("palette-card").is_some());
        };
        let is_open = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            window_state.read_with(cx, |state, _| state.palette_open)
        };

        open(cx, true);
        cx.update(|window, cx| {
            let query = palette.read(cx).query.read(cx).focus_handle(cx);
            assert!(query.is_focused(window), "the query takes focus");
        });
        cx.simulate_keystrokes("escape");
        assert!(!is_open(cx), "Escape reaches the dialog through the query");
        assert!(cx.debug_bounds("palette-card").is_none());

        let page_scroll = harness.read_with(cx, |harness, _| harness.page_scroll.clone());
        let wheel = gpui::ScrollWheelEvent {
            position: gpui::point(px(10.), px(10.)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-60.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        };
        cx.simulate_event(wheel.clone());
        draw(cx);
        assert!(
            page_scroll.offset().y < px(0.),
            "the page scrolls without a modal"
        );
        let before = page_scroll.offset();

        open(cx, false);
        cx.simulate_event(wheel);
        draw(cx);
        assert_eq!(
            page_scroll.offset(),
            before,
            "the modal scrim blocks the wheel"
        );
        assert!(is_open(cx), "the wheel leaves the palette open");
        cx.simulate_click(gpui::point(px(10.), px(10.)), gpui::Modifiers::default());
        assert!(!is_open(cx), "a backdrop press closes the palette");

        drop(palette);
        driver.shutdown(cx, &workspace_store, &host);
        drop(workspace_store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    fn phone_sheet_lays_out_only_the_visible_threads(cx: &mut TestAppContext) {
        use tcode_core::project::{Project, SessionMeta};
        use tcode_protocol::{
            EventEnvelope, HostMessage, IndexSnapshot, ServerEvent, Topic, encode_line,
        };

        struct Sheet {
            palette: Entity<CommandPalette>,
        }
        impl Render for Sheet {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().size_full().child(self.palette.clone())
            }
        }

        cx.update(crate::theme::init);
        let (to_host, _outgoing) = async_channel::unbounded();
        let (incoming, from_host) = async_channel::unbounded();
        let project = Project::from_root(std::path::PathBuf::from("/project"));
        let sessions = (0..300)
            .map(|index| {
                let mut meta = SessionMeta::new(
                    ProviderKind::Codex,
                    std::path::PathBuf::from("/project"),
                    None,
                );
                meta.id = format!("thread-{index}");
                meta.title = format!("thread-{index}");
                meta.project_id = Some(project.id.clone());
                meta.updated_at = 1_000 - index;
                meta
            })
            .collect();
        for (topic, event) in [
            (
                Topic::Settings,
                ServerEvent::SettingsSnapshot(Default::default()),
            ),
            (
                Topic::Index,
                ServerEvent::IndexSnapshot(IndexSnapshot {
                    summary: Default::default(),
                    sessions,
                    projects: vec![project],
                }),
            ),
        ] {
            incoming
                .try_send(
                    encode_line(&HostMessage::Event(EventEnvelope {
                        request_id: None,
                        topic,
                        event,
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        let deferred = std::iter::from_fn(|| from_host.try_recv().ok()).collect();
        let link = tcode_client::HostLink::new(to_host, from_host);
        let pump_link = link.clone();
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            pump_link
                .pump_with_timer(|| executor.timer(Duration::from_millis(25)))
                .await;
        });
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link,
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &incoming, deferred, cx);
        store.update(cx, |store, _| store.select_session("thread-0".into()));
        let window_state = cx.new(|_| WindowState::new(false).with_compact(true));
        window_state.update(cx, |state, cx| state.open_palette(cx));
        let (sheet, cx) = cx.add_window_view(|window, cx| Sheet {
            palette: cx.new(|cx| CommandPalette::new(store.clone(), window_state, window, cx)),
        });
        cx.run_until_parked();
        store.update(cx, |store, cx| store.drain_host_events_for_test(cx));
        cx.simulate_resize(gpui::size(px(393.), px(852.)));
        let palette = sheet.read_with(cx, |sheet, _| sheet.palette.clone());
        cx.update(|window, cx| palette.update(cx, |palette, cx| palette.open(false, window, cx)));
        cx.run_until_parked();
        draw(cx);
        assert!(cx.debug_bounds("palette-row-5").is_some());
        assert!(
            cx.debug_bounds("palette-row-250").is_none(),
            "a phone sheet must not lay out all 300 thread rows"
        );

        let position = cx.debug_bounds("palette-row-5").unwrap().center();
        for _ in 0..30 {
            cx.simulate_event(gpui::ScrollWheelEvent {
                position,
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-1000.))),
                touch_phase: gpui::TouchPhase::Moved,
                ..Default::default()
            });
            draw(cx);
        }
        let last = palette.update(cx, |palette, cx| palette.results(cx).items.len() - 1);
        assert!(
            cx.debug_bounds(format!("palette-row-{last}").leak())
                .is_some()
        );
        assert!(cx.debug_bounds("palette-row-5").is_none());

        for _ in 0..5 {
            dispatch_palette_key(&palette, cx, "down");
        }
        draw(cx);
        assert!(
            cx.debug_bounds("palette-row-5").is_some(),
            "the keyboard selection scrolls into view"
        );
    }

    #[gpui::test]
    fn a_host_answer_for_a_superseded_query_is_discarded(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = crate::store::tests::scratch_root("palette-search-test");
        let store = SessionStore::open_at(root.clone()).expect("open test store");
        let host = spawn_host(store, HostServices::default()).expect("spawn test host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let workspace_store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &workspace_store, "workspace baseline", |cx| {
            workspace_store.read_with(cx, |store, _| store.baseline_ready())
        });
        let palette_store = workspace_store.clone();
        let (harness, cx) = cx.add_window_view(move |window, cx| {
            PaletteHarness::new(palette_store.clone(), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        let palette = cx.update(|_, cx| harness.read(cx).palette.clone());

        let hit = |snippet: &str| SessionSearchHit {
            session_id: "session-1".into(),
            session_title: "Thread".into(),
            entry_id: "entry-1".into(),
            turn: 0,
            snippet: snippet.into(),
        };
        let stale = cx.update(|window, cx| {
            palette.update(cx, |palette, cx| {
                palette.query.update(cx, |state, cx| {
                    state.set_value("first".to_string(), window, cx)
                });
                palette.schedule_content_search(cx);
                let stale = palette.search_generation;
                palette.query.update(cx, |state, cx| {
                    state.set_value("second".to_string(), window, cx)
                });
                palette.schedule_content_search(cx);
                stale
            })
        });
        cx.update(|_, cx| {
            palette.update(cx, |palette, cx| {
                palette.apply_content_hits(stale, vec![hit("answer for `first`")], cx);
                assert!(
                    palette.content_hits.is_empty(),
                    "an answer for a superseded query must not be shown"
                );
                let current = palette.search_generation;
                palette.apply_content_hits(current, vec![hit("answer for `second`")], cx);
                assert_eq!(palette.content_hits.len(), 1);
                assert_eq!(palette.content_hits[0].snippet, "answer for `second`");
            });
        });

        drop(palette);
        driver.shutdown(cx, &workspace_store, &host);
        drop(workspace_store);
        let _ = std::fs::remove_dir_all(root);
    }
}
