//! Pinning and arranging threads. Rows of the desktop threads view drag within
//! and between the Pinned, Active and Settled sections; the phone's Arrange
//! threads page drags by handle; Move up / Move down write the same keys
//! without a drag. Rows of the project view do none of this.
//!
//! gpui-base has no sortable-list primitive (its dock drag is specific to tab
//! groups), so the drag composes GPUI's `on_drag`, `on_drag_move` and
//! `on_drop`, and GPUI does not cancel a drag on Escape by itself.

use super::*;
use gpui::{DragMoveEvent, KeystrokeEvent, MouseButton, Pixels, WeakEntity, deferred};
use tcode_core::thread_sort::{order_key_between, plan_reorder, thread_section};

/// A `semantic_chip`'s height: one `xs` line and its 1pt insets.
const BOUNDARY_LABEL_HEIGHT: f32 = 18.;
const EMPTY_TARGET_HEIGHT: f32 = 36.;
const ARRANGE_HANDLE_SIZE: f32 = 44.;

/// The drag payload; the sidebar keeps the drag's state in [`ThreadDrag`].
pub(super) struct DraggedThread {
    session_id: String,
    from: ThreadSection,
    scope: Option<String>,
    title: SharedString,
    can_settle: bool,
    compact: bool,
}

pub(super) struct ThreadDrag {
    pub(super) session_id: String,
    from: ThreadSection,
    /// The project the drag stays inside; `None` is every project.
    scope: Option<String>,
    /// Where the thread lands in Pinned or Active, as an index among that
    /// section's other rows; `None` leaves it where it was.
    gap: Option<(ThreadSection, usize)>,
    /// The other rows of each section inside `scope` as the drag began,
    /// which gap indexes count.
    others: ThreadSections<String>,
    over: Option<ThreadSection>,
    can_settle: bool,
    width: Pixels,
    auto_scroll: gpui_base::AutoScroll,
    _escape: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DragAction {
    Pin,
    Unpin,
    Settle,
    Unsettle,
}

impl DragAction {
    pub(super) fn icon(self) -> IconName {
        match self {
            Self::Pin => IconName::Pin,
            Self::Unpin => IconName::PinOff,
            Self::Settle => IconName::CircleCheck,
            Self::Unsettle => IconName::Undo2,
        }
    }

    pub(super) fn label(self) -> Cow<'static, str> {
        match self {
            Self::Pin => crate::tr!("sidebar.pin"),
            Self::Unpin => crate::tr!("sidebar.unpin"),
            Self::Settle => crate::tr!("sidebar.settle"),
            Self::Unsettle => crate::tr!("sidebar.unsettle"),
        }
    }
}

/// What a row, a section boundary or the Settled shelf is to a drag over it.
#[derive(Clone)]
pub(super) enum DropZone {
    Row {
        id: String,
        section: ThreadSection,
    },
    /// The start of Pinned or Active: its label, or its empty target.
    Start(ThreadSection),
    Settled,
}

impl ThreadDrag {
    fn action(&self) -> Option<DragAction> {
        use ThreadSection::*;
        match (self.from, self.over?) {
            (Settled, Settled) => None,
            (_, Settled) => self.can_settle.then_some(DragAction::Settle),
            (Pinned, Pinned) | (Active, Active) => None,
            (_, Pinned) => Some(DragAction::Pin),
            (Pinned, Active) => Some(DragAction::Unpin),
            (Settled, Active) => Some(DragAction::Unsettle),
        }
    }

    /// The sections as they would be after the drop, when the drag is over
    /// a list showing `scope`.
    pub(super) fn preview<'a>(
        &self,
        scope: Option<&str>,
        mut sections: ThreadSections<&'a SessionMeta>,
    ) -> ThreadSections<&'a SessionMeta> {
        let Some((section, index)) = self.gap.filter(|_| self.scope.as_deref() == scope) else {
            return sections;
        };
        let from = sections.section_mut(self.from);
        let Some(position) = from.iter().position(|meta| meta.id == self.session_id) else {
            return sections;
        };
        let meta = from.remove(position);
        let to = sections.section_mut(section);
        to.insert(index.min(to.len()), meta);
        sections
    }

    /// The section a list showing `scope` marks as the drop target.
    pub(super) fn target(&self, scope: Option<&str>) -> Option<ThreadSection> {
        self.over.filter(|over| {
            self.scope.as_deref() == scope && (*over != ThreadSection::Settled || self.can_settle)
        })
    }
}

/// The lifted row that follows the pointer.
pub(super) struct ThreadDragView {
    sidebar: WeakEntity<SessionsSidebar>,
    title: SharedString,
    compact: bool,
}

impl Render for ThreadDragView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (action, width) = self
            .sidebar
            .upgrade()
            .and_then(|sidebar| {
                let drag = sidebar.read(cx).drag.as_ref()?;
                Some((drag.action(), drag.width))
            })
            .unwrap_or((None, px(240.)));
        if self.compact {
            // The lifted `list_row` a finger carries: its height and inset,
            // floated at the overlay radius.
            return v_flex()
                .w(width)
                .min_h(px(crate::material::LIST_ROW_MIN_HEIGHT))
                .justify_center()
                .px(px(COMPACT_PAGE_INSET))
                .py(cx.theme().tokens.spacing.sm)
                .rounded(crate::material::radius_overlay(cx))
                .bg(cx.theme().list_active)
                .shadow_lg()
                .child(
                    div()
                        .text_body(true, cx)
                        .line_clamp(2)
                        .text_color(cx.theme().foreground)
                        .child(self.title.clone()),
                )
                .when_some(action, |card, action| {
                    card.child(
                        div()
                            .text_secondary(true, cx)
                            .text_color(cx.theme().primary)
                            .child(action.label()),
                    )
                })
                .into_any_element();
        }
        crate::material::selectable_row_two_line(h_flex(), true, false, cx)
            .w(width)
            .shadow_lg()
            .text_color(cx.theme().sidebar_foreground)
            .child(sidebar_label(self.title.clone()).text_body(false, cx))
            .when_some(action, |row, action| {
                row.child(
                    h_flex()
                        .id("thread-drag-badge")
                        .role(Role::Status)
                        .aria_label(action.label())
                        // A bordered status badge with a leading icon:
                        // `semantic_chip` takes neither.
                        .flex_none()
                        .h(px(20.))
                        .px(px(6.))
                        .gap_1()
                        .items_center()
                        .rounded(crate::material::radius_chip(cx))
                        .border_1()
                        .border_color(cx.theme().primary.opacity(0.4))
                        .bg(cx.theme().primary.opacity(0.1))
                        .text_token(&cx.theme().tokens.typography.xs)
                        .font_medium()
                        .text_color(cx.theme().primary)
                        .child(Icon::new(action.icon()))
                        .child(action.label()),
                )
            })
            .into_any_element()
    }
}

impl SessionsSidebar {
    /// Whether rows are listed by project, where they are neither dragged
    /// nor moved: the project view orders them by activity alone.
    fn in_project_view(&self, cx: &App) -> bool {
        self.store.read(cx).sidebar_layout() == SidebarLayout::Grouped
            && self.window_state.read(cx).destination() != Destination::ArrangeThreads
    }

    /// The project a list shows a thread under: the filter in the desktop's
    /// flat list, every project otherwise.
    pub(super) fn list_scope(&self, cx: &App) -> Option<String> {
        if self.window_state.read(cx).destination() == Destination::ArrangeThreads {
            self.arrange_scope.clone()
        } else if self.compact(cx) {
            None
        } else {
            self.project_filter.clone()
        }
    }

    /// Every listed thread by section, in display order, and whether each is
    /// inside `scope`.
    fn scoped_sections(
        &self,
        scope: Option<&str>,
        cx: &App,
    ) -> ThreadSections<(SessionMeta, bool)> {
        let sessions = self.store.read(cx).flat_sessions();
        let sections = partition_threads(&sessions);
        let scoped = |rows: &Vec<&SessionMeta>| {
            rows.iter()
                .map(|meta| {
                    let inside = scope.is_none_or(|id| meta.project_id.as_deref() == Some(id));
                    ((*meta).clone(), inside)
                })
                .collect()
        };
        ThreadSections {
            pinned: scoped(&sections.pinned),
            active: scoped(&sections.active),
            settled: scoped(&sections.settled),
        }
    }

    /// The payload a row starts a drag with, or `None` where rows do not drag:
    /// in member scope and while the row is being renamed.
    pub(super) fn dragged_thread(
        &self,
        meta: &SessionMeta,
        state: &ThreadRowState,
        cx: &App,
    ) -> Option<DraggedThread> {
        let store = self.store.read(cx);
        let draggable =
            store.scope().is_full() && state.renaming.is_none() && !self.in_project_view(cx);
        draggable.then(|| DraggedThread {
            session_id: meta.id.clone(),
            from: thread_section(meta),
            scope: self.list_scope(cx),
            title: meta.title.clone().into(),
            can_settle: !store.turn_running_for(&meta.id)
                && !store.pending_approval_for(&meta.id)
                && !store.pending_user_input_for(&meta.id),
            compact: self.compact(cx),
        })
    }

    /// Make `element` start a drag of `dragged`.
    pub(super) fn drag_source<E: gpui::StatefulInteractiveElement>(
        element: E,
        dragged: DraggedThread,
        cx: &Context<Self>,
    ) -> E {
        let sidebar = cx.entity().downgrade();
        element.on_drag(dragged, move |dragged, _, _, cx| {
            Self::begin_drag(sidebar.clone(), dragged, cx)
        })
    }

    fn begin_drag(
        sidebar: WeakEntity<Self>,
        dragged: &DraggedThread,
        cx: &mut App,
    ) -> Entity<ThreadDragView> {
        let escape = {
            let sidebar = sidebar.clone();
            cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
                if event.keystroke.key == "escape"
                    && sidebar
                        .update(cx, |this, cx| this.cancel_drag(window, cx))
                        .unwrap_or(false)
                {
                    cx.stop_propagation();
                }
            })
        };
        let _ = sidebar.update(cx, |this, cx| {
            let sections = this.scoped_sections(dragged.scope.as_deref(), cx);
            let gap = (dragged.from != ThreadSection::Settled)
                .then(|| {
                    sections
                        .section(dragged.from)
                        .iter()
                        .filter(|(_, inside)| *inside)
                        .position(|(meta, _)| meta.id == dragged.session_id)
                        .map(|index| (dragged.from, index))
                })
                .flatten();
            let others = |section| {
                sections
                    .section(section)
                    .iter()
                    .filter(|(meta, inside)| *inside && meta.id != dragged.session_id)
                    .map(|(meta, _)| meta.id.clone())
                    .collect()
            };
            this.drag = Some(ThreadDrag {
                session_id: dragged.session_id.clone(),
                from: dragged.from,
                scope: dragged.scope.clone(),
                gap,
                others: ThreadSections {
                    pinned: others(ThreadSection::Pinned),
                    active: others(ThreadSection::Active),
                    settled: vec![],
                },
                over: Some(dragged.from),
                can_settle: dragged.can_settle,
                width: px(240.),
                auto_scroll: Default::default(),
                _escape: escape,
            });
            this.compact_model_dirty = true;
            cx.notify();
        });
        crate::haptics::threshold();
        cx.new(|_| ThreadDragView {
            sidebar,
            title: dragged.title.clone(),
            compact: dragged.compact,
        })
    }

    /// Escape: the row returns to its place.
    fn cancel_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.drag.take().is_none() {
            return false;
        }
        cx.stop_active_drag(window);
        cx.notify();
        true
    }

    /// A drag released outside every drop target ends without a drop.
    pub(super) fn clear_finished_drag(&mut self, cx: &App) {
        if self.drag.is_some() && !cx.has_active_drag() {
            self.drag = None;
        }
    }

    /// Track the pointer over `element`, the unanimated slot of `zone`, so a
    /// row sliding through the pointer never moves the gap back.
    pub(super) fn drop_zone(
        &self,
        element: gpui::Div,
        zone: DropZone,
        scope: Option<String>,
        inset: f32,
        cx: &Context<Self>,
    ) -> gpui::Div {
        element.on_drag_move::<DraggedThread>(cx.listener(
            move |this, event: &DragMoveEvent<DraggedThread>, _, cx| {
                if event.bounds.contains(&event.event.position) {
                    let lower = event.event.position.y > event.bounds.center().y;
                    let width = event.bounds.size.width - px(inset * 2.);
                    this.drag_over(&zone, scope.as_deref(), lower, width, cx);
                }
            },
        ))
    }

    fn drag_over(
        &mut self,
        zone: &DropZone,
        scope: Option<&str>,
        lower: bool,
        width: Pixels,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        let (mut gap, mut over) = (drag.gap, drag.over);
        if drag.scope.as_deref() != scope {
            over = None;
        } else {
            match zone {
                DropZone::Settled
                | DropZone::Row {
                    section: ThreadSection::Settled,
                    ..
                } => over = Some(ThreadSection::Settled),
                DropZone::Start(section) => {
                    gap = Some((*section, 0));
                    over = Some(*section);
                }
                DropZone::Row { id, .. } if *id == drag.session_id => {
                    over = Some(gap.map_or(drag.from, |(section, _)| section));
                }
                DropZone::Row { id, section } => {
                    let index = drag
                        .others
                        .section(*section)
                        .iter()
                        .position(|other| other == id);
                    if let Some(index) = index {
                        gap = Some((*section, index + usize::from(lower)));
                        over = Some(*section);
                    }
                }
            }
        }
        let drag = self.drag.as_mut().expect("checked above");
        let row = matches!(zone, DropZone::Row { .. });
        if (drag.gap, drag.over) != (gap, over) {
            crate::haptics::selection();
        }
        if (drag.gap, drag.over) != (gap, over) || (row && drag.width != width) {
            drag.gap = gap;
            drag.over = over;
            if row {
                drag.width = width;
            }
            self.compact_model_dirty = true;
            cx.notify();
        }
    }

    /// Scroll the list under a drag held near its top or bottom edge.
    pub(super) fn drag_auto_scroll(
        &mut self,
        event: &DragMoveEvent<DraggedThread>,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        let position = event.event.position;
        let delta = (event.bounds.left() <= position.x && position.x <= event.bounds.right())
            .then(|| gpui_base::AutoScroll::compute_delta(position.y, event.bounds))
            .flatten();
        drag.auto_scroll.set(delta, cx, |delta, this, cx| {
            if let Some(list) = this.drag_list(cx) {
                list.scroll_by(delta);
                cx.notify();
            }
        });
    }

    /// The list a drag in progress scrolls: the Arrange threads page's, or
    /// else the threads view's.
    fn drag_list(&self, cx: &App) -> Option<ListState> {
        self.drag.as_ref()?;
        Some(
            if self.window_state.read(cx).destination() == Destination::ArrangeThreads {
                self.arrange_list_state.clone()
            } else {
                self.flat_list_state.clone()
            },
        )
    }

    pub(super) fn drop_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        cx.notify();
        match (drag.over, drag.gap) {
            (Some(ThreadSection::Settled), _) => {
                if drag.action() == Some(DragAction::Settle) {
                    self.settle_thread(&drag.session_id, window, cx);
                }
            }
            (Some(over), Some((section, index))) if over == section => self.place_thread(
                &drag.session_id,
                section,
                index,
                drag.scope.as_deref(),
                window,
                cx,
            ),
            _ => {}
        }
    }

    /// Put a thread at `index` among the other rows of `to` that `scope`
    /// shows, pinning, unpinning or un-settling it on the way. Only the moved
    /// thread's key is written unless its neighbours need fresh keys; rows
    /// outside `scope` keep theirs.
    fn place_thread(
        &mut self,
        id: &str,
        to: ThreadSection,
        index: usize,
        scope: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(meta) = self.store.read(cx).thread_meta(id).cloned() else {
            return;
        };
        let from = thread_section(&meta);
        let key = |meta: &SessionMeta| match to {
            ThreadSection::Pinned => meta.pin_order.clone(),
            _ => meta.active_order.clone(),
        };
        let sections = self.scoped_sections(scope, cx);
        let rows = sections.section(to);
        if from == to
            && rows
                .iter()
                .filter(|(_, inside)| *inside)
                .position(|(meta, _)| meta.id == id)
                == Some(index)
        {
            return;
        }
        let others: Vec<_> = rows.iter().filter(|(meta, _)| meta.id != id).collect();
        let mut ordered: Vec<(String, Option<String>)> = others
            .iter()
            .filter(|(_, inside)| *inside)
            .map(|(meta, _)| (meta.id.clone(), key(meta)))
            .collect();
        ordered.insert(index.min(ordered.len()), (id.to_owned(), key(&meta)));
        let hidden: Vec<String> = others
            .iter()
            .filter(|(_, inside)| !*inside)
            .filter_map(|(meta, _)| key(meta))
            .collect();
        let ordered: Vec<_> = ordered
            .iter()
            .map(|(id, key)| (id.as_str(), key.as_deref()))
            .collect();
        let hidden: Vec<_> = hidden.iter().map(String::as_str).collect();
        let reorder = |session_id: String, order_key: String| match to {
            ThreadSection::Pinned => Command::ReorderPinned {
                session_id,
                order_key,
            },
            _ => Command::ReorderActive {
                session_id,
                order_key,
            },
        };
        let mut moved_key = None;
        let mut neighbours = vec![];
        for (session_id, order_key) in plan_reorder(&ordered, &hidden, id) {
            if session_id == id {
                moved_key = Some(order_key);
            } else {
                neighbours.push(reorder(session_id, order_key));
            }
        }
        let session_id = id.to_owned();
        let (mut commands, undo) = match (from, to) {
            (ThreadSection::Pinned, ThreadSection::Pinned)
            | (ThreadSection::Active, ThreadSection::Active) => (
                moved_key
                    .map(|key| reorder(session_id.clone(), key))
                    .into_iter()
                    .collect(),
                None,
            ),
            (_, ThreadSection::Pinned) => (
                vec![Command::PinSession {
                    session_id,
                    order_key: moved_key,
                }],
                None,
            ),
            (from, _) => {
                let first = if from == ThreadSection::Pinned {
                    Command::UnpinSession {
                        session_id: session_id.clone(),
                    }
                } else {
                    Command::UnsettleSession {
                        session_id: session_id.clone(),
                    }
                };
                let undo = (from == ThreadSection::Pinned).then(|| {
                    (
                        UndoKind::Unpin,
                        vec![Command::PinSession {
                            session_id: session_id.clone(),
                            order_key: meta.pin_order.clone(),
                        }],
                    )
                });
                (
                    std::iter::once(first)
                        .chain(moved_key.map(|key| reorder(session_id, key)))
                        .collect(),
                    undo,
                )
            }
        };
        commands.extend(neighbours);
        if !commands.is_empty() {
            self.perform_lifecycle(commands, undo, window, cx);
        }
    }

    /// Whether a thread can move up and down within its section, or `None`
    /// for a settled thread and in the project view.
    pub(super) fn move_bounds(&self, id: &str, cx: &App) -> Option<(bool, bool)> {
        let meta = self.store.read(cx).thread_meta(id).cloned()?;
        let section = thread_section(&meta);
        if section == ThreadSection::Settled || self.in_project_view(cx) {
            return None;
        }
        let scope = self.list_scope(cx);
        let sections = self.scoped_sections(scope.as_deref(), cx);
        let rows: Vec<_> = sections
            .section(section)
            .iter()
            .filter(|(_, inside)| *inside)
            .collect();
        let index = rows.iter().position(|(other, _)| other.id == id)?;
        Some((index > 0, index + 1 < rows.len()))
    }

    pub(super) fn on_move(
        &mut self,
        action: &ThreadMove,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(meta) = self.store.read(cx).thread_meta(&action.0).cloned() else {
            return;
        };
        let section = thread_section(&meta);
        if section == ThreadSection::Settled {
            return;
        }
        let scope = self.list_scope(cx);
        let sections = self.scoped_sections(scope.as_deref(), cx);
        let Some(index) = sections
            .section(section)
            .iter()
            .filter(|(_, inside)| *inside)
            .position(|(other, _)| other.id == meta.id)
        else {
            return;
        };
        let Some(index) = (if action.1 {
            index.checked_add(1)
        } else {
            index.checked_sub(1)
        }) else {
            return;
        };
        self.place_thread(&meta.id, section, index, scope.as_deref(), window, cx);
    }

    /// A pin from the menu leads the pinned rows.
    pub(super) fn on_pin(
        &mut self,
        action: &ThreadPin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let first = self
            .store
            .read(cx)
            .flat_sessions()
            .into_iter()
            .filter(|meta| thread_section(meta) == ThreadSection::Pinned)
            .filter_map(|meta| meta.pin_order)
            .min();
        if self.store.read(cx).sidebar_layout() == SidebarLayout::Flat {
            self.reveal_pinned = Some(action.0.clone());
        }
        self.perform_lifecycle(
            vec![Command::PinSession {
                session_id: action.0.clone(),
                order_key: order_key_between(None, first.as_deref()),
            }],
            None,
            window,
            cx,
        );
    }

    pub(super) fn on_unpin(
        &mut self,
        action: &ThreadUnpin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.unpin_thread(&action.0, window, cx);
    }

    pub(super) fn unpin_thread(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meta) = self.store.read(cx).thread_meta(id).cloned() else {
            return;
        };
        self.perform_lifecycle(
            vec![Command::UnpinSession {
                session_id: id.to_owned(),
            }],
            Some((
                UndoKind::Unpin,
                vec![Command::PinSession {
                    session_id: id.to_owned(),
                    order_key: meta.pin_order,
                }],
            )),
            window,
            cx,
        );
    }

    /// The Arrange threads page shows the opening thread's project in
    /// Grouped and every thread in Flat.
    pub(super) fn on_arrange(
        &mut self,
        action: &ThreadArrange,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.arrange_scope = (self.store.read(cx).sidebar_layout() == SidebarLayout::Grouped)
            .then(|| self.store.read(cx).thread_meta(&action.0).cloned())
            .flatten()
            .and_then(|meta| meta.project_id);
        self.arrange_settled_expanded = false;
        self.window_state
            .update(cx, |state, cx| state.go(Destination::ArrangeThreads, cx));
    }

    /// While a drag is in progress, the label at the start of Pinned or
    /// Active, overlaying the rows so they never move, or the target that
    /// stands in for an empty section. Nothing at rest.
    pub(super) fn render_drag_boundary(
        &self,
        section: ThreadSection,
        empty: bool,
        scope: Option<String>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(drag) = self.drag.as_ref().filter(|drag| drag.scope == scope) else {
            return div().into_any_element();
        };
        let targeted = drag.target(scope.as_deref()) == Some(section);
        let color = if targeted {
            cx.theme().primary
        } else {
            cx.theme().sidebar_foreground
        };
        let label = match section {
            ThreadSection::Pinned => crate::tr!("sidebar.pinned"),
            _ => crate::tr!("sidebar.active"),
        };
        if empty {
            let target = div().w_full().py_1().child(
                h_flex()
                    .h(px(EMPTY_TARGET_HEIGHT - 8.))
                    .px_2()
                    .items_center()
                    .rounded(crate::material::radius_chip(cx))
                    .border_1()
                    .border_dashed()
                    .border_color(cx.theme().primary)
                    .text_caption(cx)
                    .text_color(color)
                    .child(label),
            );
            return self
                .drop_zone(target, DropZone::Start(section), scope, 0., cx)
                .into_any_element();
        }
        let rule = if targeted {
            cx.theme().primary.opacity(0.5)
        } else {
            cx.theme().border
        };
        // Deferred so the label paints after the rows on both sides of the
        // seam, centered on the seam rather than on a row's title line. The
        // sidebar background is transparent, so the hairline stops at the pill
        // instead of running under it.
        div()
            .relative()
            .w_full()
            .h_0()
            .child(deferred(
                h_flex()
                    .absolute()
                    .top(px(-BOUNDARY_LABEL_HEIGHT / 2.))
                    .left_0()
                    .right_0()
                    .h(px(BOUNDARY_LABEL_HEIGHT))
                    .gap_1()
                    .items_center()
                    .child(div().flex_none().w_2().h(px(1.)).bg(rule))
                    .child(crate::material::semantic_chip(
                        label,
                        cx.theme().muted,
                        if targeted {
                            cx.theme().primary
                        } else {
                            cx.theme().muted_foreground
                        },
                        cx,
                    ))
                    .child(div().flex_1().h(px(1.)).bg(rule)),
            ))
            .into_any_element()
    }

    /// The height of the target [`Self::render_drag_boundary`] shows for an
    /// empty section, for row offsets; a label takes none.
    pub(super) fn drag_boundary_height(&self, empty: bool, scope: Option<&str>) -> f32 {
        let dragging = self
            .drag
            .as_ref()
            .is_some_and(|drag| drag.scope.as_deref() == scope);
        if dragging && empty {
            EMPTY_TARGET_HEIGHT
        } else {
            0.
        }
    }

    /// The slot a dragged row leaves while it follows the pointer.
    pub(super) fn render_drag_gap(height: f32) -> gpui::AnyElement {
        div().w_full().h(px(height)).into_any_element()
    }

    pub(crate) fn render_arrange(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        self.clear_finished_drag(cx);
        let scope = self.arrange_scope.clone();
        let sessions = self.store.read(cx).flat_sessions();
        let sections = partition_threads(sessions.iter().filter(|meta| {
            scope
                .as_deref()
                .is_none_or(|id| meta.project_id.as_deref() == Some(id))
        }));
        let sections = match &self.drag {
            Some(drag) => drag.preview(scope.as_deref(), sections),
            None => sections,
        };
        let mut rows: Vec<ArrangeRow> = vec![ArrangeRow::Caption(
            ThreadSection::Pinned,
            sections.pinned.len(),
        )];
        if sections.pinned.is_empty() {
            rows.push(ArrangeRow::PinnedEmpty);
        }
        rows.extend(
            sections
                .pinned
                .iter()
                .map(|meta| ArrangeRow::Thread(Box::new((*meta).clone()))),
        );
        rows.push(ArrangeRow::Caption(
            ThreadSection::Active,
            sections.active.len(),
        ));
        rows.extend(
            sections
                .active
                .iter()
                .map(|meta| ArrangeRow::Thread(Box::new((*meta).clone()))),
        );
        if !sections.settled.is_empty() {
            rows.push(ArrangeRow::Settled(sections.settled.len()));
            if self.arrange_settled_expanded {
                rows.extend(
                    sections
                        .settled
                        .iter()
                        .map(|meta| ArrangeRow::Thread(Box::new((*meta).clone()))),
                );
            }
        }
        let keys: Vec<String> = rows.iter().map(ArrangeRow::key).collect();
        if keys != self.arrange_row_keys {
            replace_list_rows(
                &self.arrange_list_state,
                |index| self.arrange_row_keys.get(index).cloned(),
                keys.iter().cloned(),
            );
            self.arrange_row_keys = keys;
        }
        let projects: HashMap<String, String> = self
            .store
            .read(cx)
            .projects()
            .into_iter()
            .map(|project| (project.id, project.name))
            .collect();
        let rows = Rc::new(rows);
        v_flex()
            .size_full()
            .child(
                div()
                    .debug_selector(|| "arrange-title".into())
                    .flex_none()
                    .px(px(COMPACT_PAGE_INSET))
                    .pt(cx.theme().tokens.spacing.sm)
                    .text_token(&cx.theme().tokens.typography.xl)
                    .font_semibold()
                    .child(crate::tr!("sidebar.arrange_title")),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(COMPACT_PAGE_INSET))
                    .py(cx.theme().tokens.spacing.sm)
                    .text_secondary(true, cx)
                    .child(crate::tr!("sidebar.arrange_hint")),
            )
            .child(
                div()
                    .id("arrange-thread-list")
                    .flex_1()
                    .min_h_0()
                    .on_drop(cx.listener(|this, _: &DraggedThread, window, cx| {
                        this.drop_thread(window, cx)
                    }))
                    .on_drag_move(cx.listener(
                        |this, event: &DragMoveEvent<DraggedThread>, _, cx| {
                            this.drag_auto_scroll(event, cx)
                        },
                    ))
                    .child(
                        list(
                            self.arrange_list_state.clone(),
                            cx.processor(move |this, index: usize, _, cx| {
                                this.render_arrange_row(&rows[index], &projects, cx)
                            }),
                        )
                        .size_full(),
                    ),
            )
            .into_any_element()
    }

    fn render_arrange_row(
        &self,
        row: &ArrangeRow,
        projects: &HashMap<String, String>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let scope = self.arrange_scope.clone();
        let target = self
            .drag
            .as_ref()
            .and_then(|drag| drag.target(scope.as_deref()));
        let caption_color = |section| {
            if target == Some(section) {
                cx.theme().primary
            } else {
                cx.theme().muted_foreground
            }
        };
        match row {
            ArrangeRow::Caption(section, count) => {
                let label = match section {
                    ThreadSection::Pinned => crate::tr!("sidebar.pinned_count", count = count),
                    _ => crate::tr!("sidebar.active_count", count = count),
                };
                let caption = crate::material::list_caption(label.into_owned().into(), true, cx)
                    .text_color(caption_color(*section));
                self.drop_zone(caption, DropZone::Start(*section), scope, 0., cx)
                    .into_any_element()
            }
            ArrangeRow::PinnedEmpty => self
                .drop_zone(
                    crate::material::empty_row(crate::tr!("sidebar.arrange_pinned_empty"), cx),
                    DropZone::Start(ThreadSection::Pinned),
                    scope,
                    0.,
                    cx,
                )
                .into_any_element(),
            ArrangeRow::Settled(count) => {
                let expanded = self.arrange_settled_expanded;
                // The compact sidebar's settled disclosure, at a touch
                // section bar's height and inset.
                let header = crate::material::accessible_clickable(
                    h_flex(),
                    "arrange-settled",
                    Role::Button,
                    crate::tr!("sidebar.settled"),
                    cx,
                )
                .aria_expanded(expanded)
                .debug_selector(|| "arrange-settled".into())
                .w_full()
                .h(px(bar_height(Bar::Section, true)))
                .gap(cx.theme().tokens.spacing.sm)
                .px(px(COMPACT_PAGE_INSET))
                .items_center()
                .text_caption(cx)
                .text_color(caption_color(ThreadSection::Settled))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.arrange_settled_expanded = !this.arrange_settled_expanded;
                    cx.notify();
                }))
                .child(
                    collapse_chevron(!expanded, cx)
                        .text_color(caption_color(ThreadSection::Settled)),
                )
                .child(crate::tr!("sidebar.settled_count", count = count));
                self.drop_zone(
                    div().w_full().child(header),
                    DropZone::Settled,
                    scope,
                    0.,
                    cx,
                )
                .into_any_element()
            }
            ArrangeRow::Thread(meta) => {
                let zone = DropZone::Row {
                    id: meta.id.clone(),
                    section: thread_section(meta),
                };
                let dragged = self
                    .drag
                    .as_ref()
                    .is_some_and(|drag| drag.session_id == meta.id);
                let element = if dragged {
                    div()
                        .w_full()
                        .child(Self::render_drag_gap(crate::material::LIST_ROW_MIN_HEIGHT))
                } else {
                    div()
                        .w_full()
                        .child(self.render_arrange_thread(meta, projects, cx))
                };
                self.drop_zone(element, zone, scope, 0., cx)
                    .into_any_element()
            }
        }
    }

    fn render_arrange_thread(
        &self,
        meta: &SessionMeta,
        projects: &HashMap<String, String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let project = meta
            .project_id
            .as_ref()
            .and_then(|id| projects.get(id))
            .cloned();
        let state = self.thread_row_state(
            meta,
            &HashMap::new(),
            format!("arrange-thread-{}", meta.id),
            cx,
        );
        let handle = crate::material::accessible_clickable(
            div(),
            SharedString::from(format!("arrange-handle-{}", meta.id)),
            Role::Button,
            crate::tr!("sidebar.arrange_handle", title = meta.title.clone()),
            cx,
        )
        .debug_selector({
            let id = meta.id.clone();
            move || format!("arrange-handle-{id}")
        })
        .flex_none()
        .size(px(ARRANGE_HANDLE_SIZE))
        .flex()
        .items_center()
        .justify_center()
        .cursor_grab()
        // The handle alone starts a drag, so a press must not reach the row.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            Icon::new(IconName::GripVertical)
                .size(px(20.))
                .text_color(cx.theme().muted_foreground),
        );
        let handle = match self.dragged_thread(meta, &state, cx) {
            Some(dragged) => Self::drag_source(handle, dragged, cx),
            None => handle,
        };
        crate::material::list_row(
            SharedString::from(format!("arrange-row-{}", meta.id)),
            meta.title.clone().into(),
            cx,
        )
        .debug_selector({
            let id = meta.id.clone();
            move || format!("arrange-row-{id}")
        })
        // The 44pt grip must reach the page edge; `list_row`'s trailing inset
        // would push it in.
        .pr(cx.theme().tokens.spacing.xs)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(cx.theme().tokens.spacing.xxs)
                .child(
                    div()
                        .text_body(true, cx)
                        .line_clamp(2)
                        .text_color(if meta.is_settled() {
                            cx.theme().muted_foreground
                        } else {
                            cx.theme().foreground
                        })
                        .child(meta.title.clone()),
                )
                .when_some(project, |column, project| {
                    column.child(
                        div()
                            .text_secondary(true, cx)
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(project)),
                    )
                }),
        )
        .child(handle)
    }
}

enum ArrangeRow {
    Caption(ThreadSection, usize),
    PinnedEmpty,
    Thread(Box<SessionMeta>),
    Settled(usize),
}

impl ArrangeRow {
    fn key(&self) -> String {
        match self {
            Self::Caption(section, _) => format!("caption-{section:?}"),
            Self::PinnedEmpty => "pinned-empty".into(),
            Self::Thread(meta) => format!("thread-{}", meta.id),
            Self::Settled(_) => "settled".into(),
        }
    }
}
