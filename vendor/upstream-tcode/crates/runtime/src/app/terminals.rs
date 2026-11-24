use super::*;
use crate::terminal::{TerminalProjection, TerminalUpdate};
use term::TermEvent;

impl AppState {
    pub(crate) fn reap_terminal_projections(&mut self) {
        self.terminal_projections
            .retain(|id, _| self.terminal_registry.terminal(*id).is_some());
    }

    pub(crate) fn terminal_handle(&self, terminal_id: u64) -> Option<Arc<term::Terminal>> {
        self.terminal_registry.terminal(terminal_id)
    }

    fn terminal_subscribed(&self, terminal_id: u64) -> bool {
        self.subscriptions
            .contains(&Topic::Terminal { terminal_id })
    }

    fn emit_terminal(&mut self, terminal_id: u64, event: ServerEvent, cx: &mut HostCx) {
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic: Topic::Terminal { terminal_id },
            event,
        }));
    }

    /// The frame a subscriber receives on attach.
    ///
    /// Rebuilt only for the first subscriber: while anyone is attached the
    /// retained frame is advanced by the same deltas they receive, so a second
    /// client must read that shared state rather than start a new sequence.
    pub(crate) fn refresh_terminal_projection(&mut self, terminal_id: u64) {
        let Some(terminal) = self.terminal_handle(terminal_id) else {
            return;
        };
        let projection = self
            .terminal_projections
            .entry(terminal_id)
            .or_insert_with(TerminalProjection::new);
        projection.reset(&terminal);
    }

    pub(crate) fn terminal_frame(&self, terminal_id: u64) -> Option<tcode_protocol::TerminalFrame> {
        Some(self.terminal_projections.get(&terminal_id)?.frame.clone())
    }

    /// Record a terminal event and schedule the next projection.
    ///
    /// Bell and OSC 52 rides the delta so every attached client reacts, exactly
    /// as they did when each client re-parsed the byte stream itself.
    pub(crate) fn on_terminal_events(
        &mut self,
        terminal_id: u64,
        bell: bool,
        clipboard: Option<tcode_protocol::terminal::TerminalClipboard>,
        cx: &mut HostCx,
    ) {
        let Some(projection) = self.terminal_projections.get_mut(&terminal_id) else {
            return;
        };
        projection.bell |= bell;
        if clipboard.is_some() {
            projection.clipboard = clipboard;
        }
        self.schedule_terminal_projection(terminal_id, cx);
    }

    pub(crate) fn write_terminal_input(
        &mut self,
        terminal_id: u64,
        bytes: Vec<u8>,
        cx: &mut HostCx,
    ) {
        if let Some(terminal) = self.terminal_handle(terminal_id) {
            terminal.write_input(bytes);
        }
        self.schedule_terminal_projection(terminal_id, cx);
    }

    /// Project immediately when the last one is a frame old, otherwise coalesce
    /// into a single wakeup at the frame boundary.
    pub(crate) fn schedule_terminal_projection(&mut self, terminal_id: u64, cx: &mut HostCx) {
        let Some(projection) = self.terminal_projections.get_mut(&terminal_id) else {
            return;
        };
        if projection.scheduled {
            return;
        }
        let elapsed = projection.last_projected.elapsed();
        if elapsed >= crate::terminal::FRAME_INTERVAL {
            self.project_terminal(terminal_id, cx);
            return;
        }
        projection.scheduled = true;
        let delay = crate::terminal::FRAME_INTERVAL - elapsed;
        let tick = cx.clone();
        cx.spawn_detached(async move {
            smol::Timer::after(delay).await;
            tick.enqueue(move |state, cx| {
                if let Some(projection) = state.terminal_projections.get_mut(&terminal_id) {
                    projection.scheduled = false;
                }
                state.project_terminal(terminal_id, cx);
            });
        });
    }

    fn project_terminal(&mut self, terminal_id: u64, cx: &mut HostCx) {
        let Some(terminal) = self.terminal_handle(terminal_id) else {
            return;
        };
        // Without a subscriber nothing is snapshotted, so rio keeps
        // accumulating damage and take-once image buffers for the next attach.
        if !self.terminal_subscribed(terminal_id) {
            return;
        }
        let Some(projection) = self.terminal_projections.get_mut(&terminal_id) else {
            return;
        };
        projection.last_projected = std::time::Instant::now();
        let update = if projection.styles_exhausted() {
            Some(TerminalUpdate::Frame(projection.reset(&terminal)))
        } else {
            projection.update(&terminal)
        };
        // A burst that outran the scrollback budget owes the client its
        // history, and the terminal may now be idle with no wakeup left to give.
        let owes_history = projection.owes_history();
        match update {
            Some(TerminalUpdate::Frame(frame)) => self.emit_terminal(
                terminal_id,
                ServerEvent::TerminalFrame {
                    terminal_id,
                    frame: Box::new(frame),
                },
                cx,
            ),
            Some(TerminalUpdate::Delta(delta)) => self.emit_terminal(
                terminal_id,
                ServerEvent::TerminalDelta {
                    terminal_id,
                    delta: Box::new(delta),
                },
                cx,
            ),
            None => {}
        }
        if owes_history {
            self.schedule_terminal_projection(terminal_id, cx);
        }
    }

    pub(crate) fn resize_terminal(
        &mut self,
        terminal_id: u64,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
        cx: &mut HostCx,
    ) {
        let Some(terminal) = self.terminal_handle(terminal_id) else {
            return;
        };
        terminal.resize_with_cell_size(
            usize::from(cols.clamp(2, 1000)),
            usize::from(rows.clamp(2, 1000)),
            u32::from(cell_width.max(1)),
            u32::from(cell_height.max(1)),
        );
        self.schedule_terminal_projection(terminal_id, cx);
    }

    pub(crate) fn clear_terminal(&mut self, terminal_id: u64, cx: &mut HostCx) {
        if let Some(terminal) = self.terminal_handle(terminal_id) {
            terminal.clear();
        }
        self.schedule_terminal_projection(terminal_id, cx);
    }

    pub(super) fn restore_terminal_workspace(&mut self, active: &mut ActiveSession) -> bool {
        let destination = conversation_destination(active);
        let Some(workspace) = self.terminal_workspaces.remove(&destination) else {
            return false;
        };
        active.terminal_workspace = workspace;
        true
    }

    pub(super) fn park_terminal_workspace(&mut self, active: &mut ActiveSession) {
        let destination = conversation_destination(active);
        let workspace = std::mem::take(&mut active.terminal_workspace);
        self.terminal_workspaces.insert(destination, workspace);
    }

    pub(super) fn terminal_preferences_for(
        &self,
        active: &ActiveSession,
    ) -> Option<TerminalPreferences> {
        self.terminal_preferences
            .get(&conversation_destination(active).preference_key())
            .copied()
    }

    pub(super) fn write_terminal_preferences(&mut self, cx: &mut HostCx) {
        match serde_json::to_vec_pretty(&self.terminal_preferences) {
            Ok(bytes) => self.enqueue_store_write(StoreWrite::WriteTerminalUi(bytes), cx),
            Err(error) => log::warn!("failed to encode terminal UI state: {error}"),
        }
    }

    pub(super) fn terminal_prefs_mut(
        &mut self,
        key: String,
        count: usize,
    ) -> &mut TerminalPreferences {
        self.terminal_preferences
            .entry(key)
            .or_insert(TerminalPreferences {
                open: false,
                height: 240.,
                count,
            })
    }

    pub(super) fn reopen_persisted_terminals(
        &mut self,
        target_id: &str,
        preferences: Option<TerminalPreferences>,
        cx: &mut HostCx,
    ) {
        if !preferences.is_some_and(|preferences| preferences.open) {
            return;
        }
        self.open_terminal_panel(target_id, cx);
        let count = preferences
            .map(|preferences| preferences.count.clamp(1, MAX_TERMINALS_PER_SESSION))
            .unwrap_or(1);
        for _ in 1..count {
            self.new_terminal(target_id, cx);
        }
    }

    pub(super) fn persist_terminal_resource_count(&mut self, target_id: &str, cx: &mut HostCx) {
        if let Some(active) = self.resident(target_id) {
            let key = conversation_destination(active).preference_key();
            let count = active.terminal_workspace.terminals.len();
            self.terminal_prefs_mut(key, count).count = count;
        }
        self.write_terminal_preferences(cx);
    }

    pub fn set_terminal_height(&mut self, target_id: &str, height: f32, cx: &mut HostCx) {
        if let Some((key, count)) = self.resident(target_id).map(|active| {
            (
                conversation_destination(active).preference_key(),
                active.terminal_workspace.terminals.len(),
            )
        }) {
            self.terminal_prefs_mut(key, count).height = height;
            self.write_terminal_preferences(cx);
        }
    }

    pub(crate) fn terminal_panel_open(&self, target_id: &str) -> bool {
        self.resident(target_id)
            .and_then(|active| self.terminal_preferences_for(active))
            .is_some_and(|preferences| preferences.open)
    }

    pub fn toggle_terminal_panel(&mut self, target_id: &str, cx: &mut HostCx) {
        if self.terminal_panel_open(target_id) {
            self.close_terminal_panel(target_id, cx);
        } else {
            self.open_terminal_panel(target_id, cx);
        }
    }

    pub(super) fn schedule_terminal_spawn(
        &mut self,
        session_id: String,
        cwd: PathBuf,
        action: TerminalSpawnAction,
        cx: &mut HostCx,
    ) {
        self.next_terminal_spawn_id = self
            .next_terminal_spawn_id
            .checked_add(1)
            .expect("terminal spawn id overflow");
        let spawn_id = self.next_terminal_spawn_id;
        self.pending_terminal_spawns
            .entry(session_id.clone())
            .or_default()
            .insert(spawn_id, action);

        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx.unblock(move || term::Terminal::spawn(cwd)).await;
            host_cx.enqueue(move |state, cx| {
                let pending = state
                    .pending_terminal_spawns
                    .get_mut(&session_id)
                    .and_then(|spawns| spawns.remove(&spawn_id));
                if state
                    .pending_terminal_spawns
                    .get(&session_id)
                    .is_some_and(HashMap::is_empty)
                {
                    state.pending_terminal_spawns.remove(&session_id);
                }
                let Some(action) = pending else {
                    return;
                };
                let active_matches = state
                    .resident(&session_id)
                    .is_some_and(|active| active.meta.id == session_id);
                if !active_matches {
                    return;
                }

                let terminal = match result {
                    Ok(terminal) => terminal,
                    Err(error) => {
                        let runtime_error = match action {
                            TerminalSpawnAction::Restart { .. } => RuntimeError::TerminalRestart {
                                error: error.to_string(),
                            },
                            _ => RuntimeError::TerminalStart {
                                error: error.to_string(),
                            },
                        };
                        state.report_error(runtime_error, cx);
                        return;
                    }
                };

                let workspace = &mut state.resident_mut(&session_id).unwrap().terminal_workspace;
                let terminal_id = match action {
                    TerminalSpawnAction::Open | TerminalSpawnAction::New => {
                        (workspace.terminals.len() < MAX_TERMINALS_PER_SESSION)
                            .then(|| workspace.push(terminal))
                    }
                    TerminalSpawnAction::Restart { terminal_id } => {
                        if let Some(entry) = workspace
                            .terminals
                            .iter_mut()
                            .find(|entry| Some(entry.id) == terminal_id)
                        {
                            entry.terminal = terminal.into();
                            Some(entry.id)
                        } else if terminal_id.is_none() && workspace.terminals.is_empty() {
                            Some(workspace.push(terminal))
                        } else {
                            None
                        }
                    }
                    TerminalSpawnAction::Split { first, direction } => {
                        if workspace.terminals.len() < MAX_TERMINALS_PER_SESSION
                            && workspace.terminal(first).is_some()
                            && workspace.split_for(first).is_none()
                        {
                            let second = workspace.push(terminal);
                            workspace.splits.push(TerminalSplit {
                                first,
                                second,
                                direction,
                            });
                            Some(second)
                        } else {
                            None
                        }
                    }
                };
                let Some(terminal_id) = terminal_id else {
                    return;
                };
                state.sync_terminal_handles();
                // A restart reuses the tab id, so the previous grid must not
                // survive into the new PTY's projection.
                state
                    .terminal_projections
                    .insert(terminal_id, TerminalProjection::new());
                if state.terminal_subscribed(terminal_id) {
                    state.refresh_terminal_projection(terminal_id);
                    if let Some(frame) = state.terminal_frame(terminal_id) {
                        state.emit_terminal(
                            terminal_id,
                            ServerEvent::TerminalFrame {
                                terminal_id,
                                frame: Box::new(frame),
                            },
                            cx,
                        );
                    }
                }
                let events = state
                    .terminal_handle(terminal_id)
                    .map(|terminal| terminal.events());
                if let Some(events) = events {
                    let event_cx = cx.clone();
                    cx.spawn_detached(async move {
                        // Drain everything already queued into one wakeup: a
                        // flood produces far more emulator events than frames.
                        while let Ok(first) = events.recv().await {
                            let (mut bell, mut clipboard) = (false, None);
                            let mut note = |event| match event {
                                TermEvent::Bell => bell = true,
                                TermEvent::ClipboardStore { kind, text } => {
                                    clipboard = Some(tcode_protocol::terminal::TerminalClipboard {
                                        selection: kind
                                            == term::rio_vt::clipboard::ClipboardType::Selection,
                                        text,
                                    });
                                }
                                TermEvent::Wakeup | TermEvent::Exited => {}
                            };
                            note(first);
                            while let Ok(event) = events.try_recv() {
                                note(event);
                            }
                            event_cx.enqueue(move |state, cx| {
                                state.on_terminal_events(terminal_id, bell, clipboard, cx);
                            });
                        }
                    });
                }
                state.persist_terminal_resource_count(&session_id, cx);
            });
        });
    }

    pub(super) fn cancel_pending_terminal_spawns(&mut self, session_id: &str) {
        self.pending_terminal_spawns.remove(session_id);
    }

    pub(crate) fn open_terminal_panel(&mut self, target_id: &str, cx: &mut HostCx) {
        let Some((session_id, cwd, destination, count, terminals_empty)) =
            self.resident(target_id).map(|active| {
                (
                    active.meta.id.clone(),
                    active.meta.cwd.clone(),
                    conversation_destination(active),
                    active.terminal_workspace.terminals.len(),
                    active.terminal_workspace.terminals.is_empty(),
                )
            })
        else {
            return;
        };
        let key = destination.preference_key();
        self.terminal_prefs_mut(key, count).open = true;
        self.write_terminal_preferences(cx);
        if terminals_empty {
            let already_pending =
                self.pending_terminal_spawns
                    .get(&session_id)
                    .is_some_and(|spawns| {
                        spawns
                            .values()
                            .any(|action| matches!(action, TerminalSpawnAction::Open))
                    });
            if !already_pending {
                self.schedule_terminal_spawn(session_id, cwd, TerminalSpawnAction::Open, cx);
            }
        }
    }

    pub fn close_terminal_panel(&mut self, target_id: &str, cx: &mut HostCx) {
        let session_id = self
            .resident(target_id)
            .map(|active| active.meta.id.clone());
        if let Some(session_id) = session_id.as_deref() {
            self.cancel_pending_terminal_spawns(session_id);
        }
        if let Some(active) = self.resident(target_id) {
            let key = conversation_destination(active).preference_key();
            let count = active.terminal_workspace.terminals.len();
            self.terminal_prefs_mut(key, count).open = false;
            self.write_terminal_preferences(cx);
        }
    }

    pub fn restart_terminal(&mut self, target_id: &str, cx: &mut HostCx) {
        let Some(active) = self.resident(target_id) else {
            return;
        };
        let session_id = active.meta.id.clone();
        let cwd = active.meta.cwd.clone();
        let terminal_id = active.terminal_workspace.active_id;
        if let Some(spawns) = self.pending_terminal_spawns.get_mut(&session_id) {
            spawns.retain(|_, action| !matches!(action, TerminalSpawnAction::Restart { .. }));
        }
        self.schedule_terminal_spawn(
            session_id,
            cwd,
            TerminalSpawnAction::Restart { terminal_id },
            cx,
        );
    }

    pub(super) fn terminal_limit_reached(&self, active: &ActiveSession) -> bool {
        let pending = self
            .pending_terminal_spawns
            .get(&active.meta.id)
            .map_or(0, HashMap::len);
        active.terminal_workspace.terminals.len() + pending >= MAX_TERMINALS_PER_SESSION
    }

    pub(super) fn terminal_split_available(&self, active: &ActiveSession) -> bool {
        !self.terminal_limit_reached(active)
            && active.terminal_workspace.active_id.is_some_and(|first| {
                active.terminal_workspace.split_for(first).is_none()
                    && !self.pending_terminal_spawns.get(&active.meta.id).is_some_and(|spawns| {
                        spawns.values().any(|action| {
                            matches!(action, TerminalSpawnAction::Split { first: pending, .. } if *pending == first)
                        })
                    })
            })
    }

    pub fn new_terminal(&mut self, target_id: &str, cx: &mut HostCx) {
        let Some(active) = self.resident(target_id) else {
            return;
        };
        if self.terminal_limit_reached(active) {
            return;
        }
        let cwd = self.spawn_cwd(target_id);
        self.schedule_terminal_spawn(
            self.resident(target_id)
                .map(|active| active.meta.id.clone())
                .unwrap_or_default(),
            cwd,
            TerminalSpawnAction::New,
            cx,
        );
    }

    /// A new tab or split follows the active terminal's foreground directory,
    /// falling back to the session's own cwd. This used to be a thread-local
    /// override set by the desktop window, which only worked in-process.
    fn spawn_cwd(&self, target_id: &str) -> PathBuf {
        let Some(active) = self.resident(target_id) else {
            return PathBuf::new();
        };
        active
            .terminal_workspace
            .active()
            .map(|entry| entry.terminal.working_directory())
            .filter(|cwd| cwd.is_dir())
            .unwrap_or_else(|| active.meta.cwd.clone())
    }

    pub fn activate_terminal(&mut self, target_id: &str, terminal_id: u64, _cx: &mut HostCx) {
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        if active.terminal_workspace.terminal(terminal_id).is_some() {
            active.terminal_workspace.active_id = Some(terminal_id);
        }
    }

    pub fn close_terminal(&mut self, target_id: &str, terminal_id: u64, cx: &mut HostCx) {
        let session_id = self
            .resident(target_id)
            .map(|active| active.meta.id.clone());
        if let Some(session_id) = session_id.as_deref() {
            self.cancel_pending_terminal_spawns(session_id);
        }
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        let workspace = &mut active.terminal_workspace;
        workspace.terminals.retain(|entry| entry.id != terminal_id);
        workspace
            .splits
            .retain(|split| split.first != terminal_id && split.second != terminal_id);
        if workspace.active_id == Some(terminal_id) {
            workspace.active_id = workspace.terminals.last().map(|entry| entry.id);
        }
        let empty = workspace.terminals.is_empty();
        self.persist_terminal_resource_count(target_id, cx);
        if empty {
            self.close_terminal_panel(target_id, cx);
        }
    }

    pub fn split_terminal(
        &mut self,
        target_id: &str,
        direction: TerminalSplitDirection,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.resident(target_id) else {
            return;
        };
        let workspace = &active.terminal_workspace;
        let Some(first) = workspace.active_id else {
            return;
        };
        if !self.terminal_split_available(active) {
            return;
        }
        let session_id = active.meta.id.clone();
        let cwd = self.spawn_cwd(target_id);
        self.schedule_terminal_spawn(
            session_id,
            cwd,
            TerminalSpawnAction::Split { first, direction },
            cx,
        );
    }

    pub fn capture_terminal_selection(
        &mut self,
        target_id: &str,
        terminal_id: u64,
        selection: Option<tcode_protocol::TerminalSelection>,
        _cx: &mut HostCx,
    ) {
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        let Some(entry) = active.terminal_workspace.terminal(terminal_id) else {
            return;
        };
        let label = entry.terminal.label();
        // Selection is client state: the host grid has no viewport to select in.
        if let Some(selection) = selection {
            active.terminal_workspace.add_context(
                label,
                term::SelectedText {
                    line_start: selection.line_start,
                    line_end: selection.line_end,
                    text: selection.text,
                },
            );
        }
    }

    pub fn remove_terminal_context(&mut self, target_id: &str, context_id: u64, _cx: &mut HostCx) {
        if let Some(active) = self.resident_mut(target_id) {
            active
                .terminal_workspace
                .contexts
                .retain(|context| context.id != context_id);
        }
    }

    pub(crate) fn review_comments(&self, target_id: &str) -> &[ReviewComment] {
        let Some(id) = self
            .resident(target_id)
            .map(|active| active.meta.id.as_str())
        else {
            return &[];
        };
        self.review_comment_drafts
            .get(id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn add_review_comment(
        &mut self,
        target_id: &str,
        comment: ReviewComment,
        _cx: &mut HostCx,
    ) {
        if let Some(id) = self
            .resident(target_id)
            .map(|active| active.meta.id.clone())
        {
            self.review_comment_drafts
                .entry(id.clone())
                .or_default()
                .push(comment);
        }
    }

    pub fn remove_review_comment(&mut self, target_id: &str, index: usize, _cx: &mut HostCx) {
        let Some(id) = self
            .resident(target_id)
            .map(|active| active.meta.id.clone())
        else {
            return;
        };
        if let Some(comments) = self.review_comment_drafts.get_mut(&id)
            && index < comments.len()
        {
            comments.remove(index);
        }
    }

    pub(super) fn clear_review_comments(&mut self, target_id: &str, _cx: &mut HostCx) {
        if let Some(id) = self
            .resident(target_id)
            .map(|active| active.meta.id.clone())
        {
            self.review_comment_drafts.remove(&id);
        }
    }

    /// Drop the attached terminal contexts once a message consuming them is sent.
    pub(super) fn clear_terminal_contexts(&mut self, target_id: &str) {
        if let Some(active) = self.resident_mut(target_id) {
            active.terminal_workspace.contexts.clear();
        }
    }
}

impl AppState {
    /// Close the thread's terminals that fresh process inspection proves are
    /// at an idle shell, keeping their output. A terminal used since the
    /// settle, one that cannot be inspected, and a thread re-engaged
    /// meanwhile all keep their shells.
    pub(super) fn close_settled_idle_terminals(&mut self, id: &str, cx: &mut HostCx) {
        let terminals: Vec<_> = self
            .resident(id)
            .map(|session| &session.terminal_workspace)
            .or_else(|| {
                self.terminal_workspaces
                    .get(&ConversationDestination::Thread(id.to_owned()))
            })
            .into_iter()
            .flat_map(|workspace| &workspace.terminals)
            .map(|entry| {
                let terminal = entry.terminal.clone();
                (entry.id, terminal.activity_mark(), terminal)
            })
            .collect();
        if terminals.is_empty() {
            return;
        }
        let revision = self.decision_revision(id);
        let id = id.to_owned();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let idle = host_cx
                .unblock(move || {
                    terminals
                        .into_iter()
                        .filter(|(_, _, terminal)| terminal.idle_shell() == Some(true))
                        .collect::<Vec<_>>()
                })
                .await;
            host_cx.enqueue(move |state, _cx| {
                if state.decision_revision(&id) != revision
                    || !state.find_meta(&id).is_some_and(|meta| meta.is_settled())
                {
                    return;
                }
                for (terminal_id, mark, terminal) in idle {
                    if terminal.activity_mark() == mark
                        && state
                            .terminal_handle(terminal_id)
                            .is_some_and(|current| Arc::ptr_eq(&current, &terminal))
                    {
                        // The tab and its emulator stay: an exited shell's
                        // output remains readable.
                        terminal.kill();
                    }
                }
            });
        });
    }
}
