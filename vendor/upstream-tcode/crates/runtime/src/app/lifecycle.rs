use super::*;

impl AppState {
    /// Request a provider-owned restore point. No local Git or transcript
    /// operation is performed: the canonical timeline changes only after the
    /// provider confirms the native request.
    pub fn rewind_turn(&mut self, target_id: &str, turn: usize, mode: RewindMode, cx: &mut HostCx) {
        let Some(active) = self.resident(target_id) else {
            return;
        };
        if self.native_rewind_blocked(active) {
            self.report_error(RuntimeError::NativeRewindBlocked, cx);
            return;
        }
        let Some(checkpoint_id) = active
            .timeline
            .turns
            .get(turn)
            .and_then(|turn| turn.provider_checkpoint_id.clone())
        else {
            self.report_error(RuntimeError::NativeRewindBlocked, cx);
            return;
        };
        // Claude Code 2.1.214 cannot persist a conversation rewind before the
        // first assistant anchor. File-only rewind remains valid there.
        if turn == 0 && mode.includes_conversation() {
            self.report_error(RuntimeError::NativeRewindBlocked, cx);
            return;
        }
        let session_id = active.meta.id.clone();
        let live_commands = match &active.runtime {
            Runtime::Live(commands) => Some(commands.clone()),
            Runtime::Idle | Runtime::Starting { .. } => None,
        };
        self.pending_native_rewinds
            .insert(session_id.clone(), (checkpoint_id.clone(), mode));
        if let Some(commands) = live_commands {
            if commands
                .try_send(SessionCommand::Rewind {
                    checkpoint_id,
                    mode,
                })
                .is_err()
            {
                self.pending_native_rewinds.remove(&session_id);
                self.report_error(RuntimeError::ProcessGone, cx);
            }
        } else {
            self.ensure_started(target_id, cx);
        }
    }

    /// The command asked for while the provider was starting; clears it.
    fn take_requested_command(&mut self, session_id: &str) -> Option<SessionCommand> {
        self.resident_mut(session_id)?.requested_command.take()
    }

    /// Spawn the provider process for the active session if it isn't running.
    pub(super) fn ensure_started(&mut self, target_id: &str, cx: &mut HostCx) {
        let Some(session_id) = self
            .resident(target_id)
            .map(|session| session.meta.id.as_str())
            .map(str::to_owned)
        else {
            return;
        };
        self.ensure_session_started(&session_id, cx);
    }

    /// Spawn a provider for either the foreground session or a parked child.
    pub(super) fn ensure_session_started(&mut self, session_id: &str, cx: &mut HostCx) {
        let idle = self
            .residents
            .live
            .get(session_id)
            .filter(|active| active.meta.id == session_id)
            .map(|active| matches!(active.runtime, Runtime::Idle))
            .or_else(|| {
                self.residents
                    .parked
                    .get(session_id)
                    .map(|active| matches!(active.runtime, Runtime::Idle))
            })
            .unwrap_or(false);
        if !idle {
            return;
        }
        self.next_start_generation = self
            .next_start_generation
            .checked_add(1)
            .expect("provider start generation overflow");
        let generation = self.next_start_generation;
        let active = self.resident_mut(session_id).unwrap();
        if active.meta.moved_from.is_some() {
            // The native session belongs to the directory the thread left.
            active.meta.resume_cursor = None;
            active.meta.pending_fork = false;
        }
        active.runtime = Runtime::Starting { generation };
        active.mcp_servers_attached = true;
        active.idle_since = None;
        active.live_model = active.meta.model.clone();
        active.live_cwd = active.meta.cwd.clone();
        active.live_option_selections = active.meta.option_selections.clone();
        active.confirmed_option_selections.clear();

        let meta = active.meta.clone();
        let settings = self.settings.clone();
        let launch_env = self.prepare_session_env(&meta, cx);
        let preview_registration = if meta.provider.caps().mcp_servers {
            self.preview_registration_for(&meta)
        } else {
            None
        };
        let pull_request_registration = if self.pull_request_tools_offered(meta.provider) {
            self.pull_request_registration_for(&meta)
        } else {
            None
        };
        let instructions = pull_request_registration
            .is_some()
            .then(|| self.pull_request_instructions(&meta.id));
        let orchestrate_registration = self.orchestrate_registration_for(&meta);
        let orchestrate_report_registration = self.orchestrate_child_registration_for(&meta);
        let computer_use_registration = if computer_use_attaches(meta.provider, &self.settings) {
            self.computer_use_registration_for(&meta)
        } else {
            None
        };
        let control_registration = self.control_registration_for(&meta);
        let provider_launcher = self.provider_launcher.clone();
        let session_id = meta.id.clone();
        if let Some(cursor) = &meta.resume_cursor {
            log::info!(
                "starting provider {:?} with resume cursor: {}",
                meta.provider,
                cursor.0
            );
        } else {
            log::info!("starting provider {:?} (fresh session)", meta.provider);
        }

        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = async {
                let launch_env = launch_env.await.map_err(agent::AgentError::Spawn)?;
                let mut opts = session_options(
                    &meta,
                    &settings,
                    launch_env,
                    McpServers {
                        preview: preview_registration,
                        orchestrate: orchestrate_registration,
                        orchestrate_report: orchestrate_report_registration,
                        computer_use: computer_use_registration,
                        control: control_registration,
                    },
                );
                opts.mcp_servers.extend(pull_request_registration);
                opts.instructions = instructions;
                provider_launcher.launch(meta.provider, opts).await
            }
            .await;
            host_cx.enqueue(move |state, cx| {
                let matches_active = state.residents.live.get(&session_id).is_some_and(|active| {
                    active.meta.id == session_id && active.is_starting_generation(generation)
                });
                // The session may have been parked (thread switch) while its
                // start was in flight; the attempt then adopts the parked entry.
                let matches_parked = !matches_active
                    && state
                        .resident(&session_id)
                        .is_some_and(|parked| parked.is_starting_generation(generation));
                match result {
                    Ok(handle) => {
                        if !matches_active && !matches_parked {
                            // Superseded by a newer start, or the session is gone.
                            let _ = handle.commands.try_send(SessionCommand::Shutdown);
                            return;
                        }
                        let commands = handle.commands.clone();
                        let events = handle.events.clone();
                        let pump_session = session_id.clone();
                        let pump_commands = handle.commands.clone();
                        let pump_cx = cx.clone();
                        let pump = HostCx::spawn_background(cx, async move {
                            while let Ok(event) = events.recv().await {
                                let event_session = pump_session.clone();
                                let event_commands = pump_commands.clone();
                                pump_cx.enqueue(move |state, cx| {
                                    // A provider detached to idle may still have events
                                    // queued; they must not reach a restarted one.
                                    let current = state.resident(&event_session).is_some_and(
                                        |session| matches!(&session.runtime, Runtime::Live(live) if live.same_channel(&event_commands)),
                                    );
                                    if current {
                                        state.on_event(&event_session, event, cx);
                                    }
                                });
                            }
                            pump_cx.enqueue(move |state, cx| {
                                state.on_event_stream_ended(&pump_session, &pump_commands, cx);
                            });
                        });
                        if let Some(resident) = state.resident_mut(&session_id) {
                            resident.runtime = Runtime::Live(commands.clone());
                            resident._pump = Some(pump);
                            if let Some(descriptor) = resident.permission_descriptor() {
                                let (id, apply) = match descriptor {
                                    OptionDescriptor::Select { id, apply, .. }
                                    | OptionDescriptor::Boolean { id, apply, .. } => (id, apply),
                                };
                                if apply != ApplyTiming::Restart
                                    && let Some(selection) = resident
                                        .meta
                                        .option_selections
                                        .iter()
                                        .find(|selection| selection.id == id)
                                    && Some(selection)
                                        != resident
                                            .live_option_selections
                                            .iter()
                                            .find(|selection| selection.id == id)
                                {
                                    let _ = commands.try_send(SessionCommand::SetOption {
                                        id: selection.id.clone(),
                                        value: selection.value.clone(),
                                    });
                                }
                            }
                        }
                        if matches_active {
                            if let Some((checkpoint_id, mode)) =
                                state.pending_native_rewinds.get(&session_id).cloned()
                            {
                                if commands
                                    .try_send(SessionCommand::Rewind {
                                        checkpoint_id,
                                        mode,
                                    })
                                    .is_err()
                                {
                                    state.pending_native_rewinds.remove(&session_id);
                                    state.report_error(RuntimeError::ProcessGone, cx);
                                }
                            } else if let Some(command) = state.take_requested_command(&session_id) {
                                if commands.try_send(command).is_err() {
                                    state.report_error(RuntimeError::ProcessGone, cx);
                                }
                            } else if state.dispatch_next_queued(&session_id, cx).is_err() {
                                state.report_error(RuntimeError::ProcessGone, cx);
                            }
                        } else {
                            if let Some((checkpoint_id, mode)) =
                                state.pending_native_rewinds.get(&session_id).cloned()
                            {
                                if commands
                                    .try_send(SessionCommand::Rewind {
                                        checkpoint_id,
                                        mode,
                                    })
                                    .is_err()
                                {
                                    state.pending_native_rewinds.remove(&session_id);
                                    state.report_error(RuntimeError::ProcessGone, cx);
                                }
                            } else if let Some(command) = state.take_requested_command(&session_id) {
                                if commands.try_send(command).is_err() {
                                    state.report_error(RuntimeError::ProcessGone, cx);
                                }
                            } else {
                                // Work through the parked queue exactly as a
                                // finished background turn would.
                                state.on_background_turn_completed(&session_id, cx);
                            }
                        }
                    }
                    Err(err) => {
                        if matches_active || matches_parked {
                            // The queue is deliberately KEPT in both cases: it
                            // holds text the user typed but that was never
                            // sent. It stays visible in the queue strip and
                            // flushes on the next successful start; clearing it
                            // would destroy their words along with the process.
                            if let Some(resident) = state.resident_mut(&session_id) {
                                resident.runtime = Runtime::Idle;
                                resident.delivery_in_flight = None;
                                resident.turn_in_flight = false;
                                resident.background_task_count = 0;
                            }
                            state.pending_native_rewinds.remove(&session_id);
                            let error_event = AgentEvent::ProviderStartFailed {
                                error: err.to_string(),
                            };
                            state.record_event(&session_id, &error_event, cx);
                            let is_child = state.sessions.iter().any(|meta| {
                                meta.id == session_id && meta.parent_session_id.is_some()
                            });
                            if is_child {
                                if let Some(child) = state.resident_mut(&session_id) {
                                    child.queue.clear();
                                }
                                state.fail_child_expectation(
                                    &session_id,
                                    &err.to_string(),
                                    cx,
                                );
                            }
                            state.evaluate_thread_settlement(&session_id, cx);
                            let missing_cwd = state
                                .find_meta(&session_id)
                                .filter(|meta| !meta.cwd.is_dir())
                                .map(|meta| (meta.project_id, meta.cwd));
                            let error = match missing_cwd {
                                Some((project_id, cwd)) => RuntimeError::WorkingDirectoryMissing {
                                    session_id: session_id.clone(),
                                    project_id,
                                    cwd,
                                },
                                None => RuntimeError::ProviderStart {
                                    error: err.to_string(),
                                },
                            };
                            state.report_error(error, cx);
                        }
                    }
                }
            });
        });
    }

    /// The provider's event stream ended without a `SessionClosed`. If the
    /// session still owns that exact provider (same command channel, still
    /// Live), the adapter died without announcing it — and the lifecycle flags
    /// (`turn_in_flight`, `delivery_in_flight`, `background_task_count`) have
    /// no other reset path, so the thread would sit at "Working" forever.
    /// Synthesize the close so the ordinary teardown runs no matter which
    /// adapter exit path forgot to emit it. The same-channel check keeps a
    /// stale pump (session already closed, restarted, or shut down mid-event)
    /// from tearing down a successor provider.
    pub(super) fn on_event_stream_ended(
        &mut self,
        session_id: &str,
        pump_commands: &smol::channel::Sender<SessionCommand>,
        cx: &mut HostCx,
    ) {
        let still_owned = self
            .resident(session_id)
            .is_some_and(|session| match &session.runtime {
                Runtime::Live(current) => current.same_channel(pump_commands),
                _ => false,
            });
        if !still_owned {
            return;
        }
        log::warn!(
            "provider event stream for {session_id} ended without SessionClosed; synthesizing close"
        );
        self.on_event(
            session_id,
            AgentEvent::SessionClosed {
                reason: Some("provider event stream ended without a close".into()),
            },
            cx,
        );
    }

    pub(super) fn meta_mut(&mut self, session_id: &str) -> Option<&mut SessionMeta> {
        // Parked sessions keep receiving meta updates (resume cursor, updated_at)
        // — losing the cursor while parked would break the next cold resume.
        self.resident_mut(session_id)
            .map(|session| &mut session.meta)
    }

    pub(super) fn persist_meta(&mut self, meta: &SessionMeta, cx: &mut HostCx) {
        let old_parent = self
            .sessions
            .iter()
            .find(|old| old.id == meta.id)
            .and_then(|old| old.parent_session_id.clone());
        let parent_changed = self
            .sessions
            .iter()
            .find(|old| old.id == meta.id)
            .is_some_and(|old| old.parent_session_id.is_some() != meta.parent_session_id.is_some());
        if parent_changed {
            self.thread_activity.remove(&meta.id);
        }
        self.advance_decision_revision(&meta.id);
        self.enqueue_store_write(
            StoreWrite::UpsertMeta {
                meta: Box::new(meta.clone()),
                initial: false,
            },
            cx,
        );
        self.upsert_session_in_memory(meta.clone());
        let id = meta.id.clone();
        let former_parent =
            old_parent.filter(|parent| Some(parent) != meta.parent_session_id.as_ref());
        cx.enqueue(move |state, cx| {
            state.evaluate_thread_settlement(&id, cx);
            if let Some(parent) = former_parent {
                state.evaluate_thread_settlement(&parent, cx);
            }
        });
    }

    /// Close what this thread's provider leaves open before it goes away: the
    /// persisted turn and requests, and its native subagent mirrors. Execution
    /// children own independent providers and keep running.
    pub(super) fn retire_provider_work(&mut self, id: &str, cx: &mut HostCx) {
        let has_open_work = self.resident(id).is_some_and(|session| {
            !session.draft
                && !matches!(session.runtime, Runtime::Idle)
                && (session.turn_in_flight
                    || session.delivery_in_flight.is_some()
                    || session.background_task_count > 0
                    || session.timeline.turn_running
                    || !session.timeline.pending_approvals.is_empty()
                    || session.timeline.pending_user_input.is_some())
        });
        if has_open_work {
            self.record_event(id, &AgentEvent::SessionClosed { reason: None }, cx);
        }
        self.interrupt_native_subagent_work(id, cx);
        self.clear_approvals(id);
    }

    /// Detach this thread's provider and keep the thread resident. Events the
    /// old provider still has queued are dropped by the pump's channel check.
    pub(super) fn detach_provider_to_idle(&mut self, id: &str, cx: &mut HostCx) {
        self.retire_provider_work(id, cx);
        if let Some(session) = self.resident_mut(id) {
            session.shutdown_to_idle();
        }
    }

    pub(crate) fn shutdown_active(&mut self, target_id: &str, cx: &mut HostCx) {
        self.revoke_computer_use_registration(target_id);
        if let Some(session_id) = self
            .resident(target_id)
            .map(|session| session.meta.id.as_str())
            .map(str::to_string)
        {
            self.clear_approvals(&session_id);
            self.pending_native_rewinds.remove(&session_id);
        }
        if let Some(active) = self.residents.live.remove(target_id) {
            if active.draft && self.archived_sharing_affected(&active.meta) {
                self.archived_revision += 1;
            }
            if let Runtime::Live(commands) = active.runtime {
                let _ = commands.try_send(SessionCommand::Shutdown);
            }
        }
        self.release_stale_session_logs(cx);
    }

    /// Shut down every provider process before the application exits.
    pub fn shutdown_all(&mut self, cx: &mut HostCx) {
        self.stop_diff_pass();
        self.stop_pull_request_workers();
        for id in self
            .mcp
            .computer_use_registrations
            .keys()
            .cloned()
            .collect::<Vec<_>>()
        {
            self.revoke_computer_use_registration(&id);
        }
        for id in self.residents.live.keys().cloned().collect::<Vec<_>>() {
            self.shutdown_active(&id, cx);
        }
        for (_, parked) in self.residents.parked.drain() {
            if let Runtime::Live(commands) = parked.runtime {
                let _ = commands.try_send(SessionCommand::Shutdown);
            }
        }
        // Drop every conversation-owned PTY, including those parked while an
        // idle thread was off screen.
        self.terminal_workspaces.clear();
        self.pending_native_rewinds.clear();
    }

    /// Leave the active session without killing its provider or in-memory work.
    /// Every "switch away" path goes through here; only destructive paths use
    /// `shutdown_active` directly.
    pub(super) fn park_active(&mut self, target_id: &str, cx: &mut HostCx) {
        let Some(mut active) = self.residents.live.remove(target_id) else {
            return;
        };
        if active.draft && self.archived_sharing_affected(&active.meta) {
            self.archived_revision += 1;
        }
        self.park_terminal_workspace(&mut active);
        let native_rewind_pending = self.pending_native_rewinds.contains_key(&active.meta.id);
        let has_work = active.turn_in_flight
            || active.delivery_in_flight.is_some()
            || !active.queue.is_empty()
            || active.background_task_count > 0
            || native_rewind_pending;
        let parkable = active.draft
            || has_work
            || matches!(active.runtime, Runtime::Live(_) | Runtime::Starting { .. })
            || self.event_records.contains_key(target_id);
        if !parkable {
            self.release_stale_session_logs(cx);
            return;
        }
        let session_id = active.meta.id.clone();
        if has_work {
            active.idle_since = None;
        }
        self.residents.park(active);
        self.reschedule_scheduled_wake(cx);
        if !has_work {
            self.mark_resident_idle(&session_id, cx);
        }
    }

    /// Start the idle grace and enforce the parked LRU bound. A live provider
    /// keeps the grace its last turn end started, so parking a thread does not
    /// extend its process's life.
    pub(super) fn mark_resident_idle(&mut self, session_id: &str, cx: &mut HostCx) {
        let rewind_pending = self.pending_native_rewinds.contains_key(session_id);
        let Some(session) = self
            .residents
            .resident_mut(session_id)
            .filter(|session| !rewind_pending && session.fully_idle())
        else {
            return;
        };
        let armed = session
            .idle_since
            .filter(|_| matches!(session.runtime, Runtime::Live(_)));
        let idle_since = armed.unwrap_or_else(Instant::now);
        session.idle_since = Some(idle_since);

        let mut residents: Vec<_> = self
            .residents
            .parked
            .iter()
            .filter(|(id, session)| {
                session.fully_idle() && !self.pending_native_rewinds.contains_key(id.as_str())
            })
            .filter_map(|(id, session)| {
                session.idle_since.map(|idle| {
                    let replay = !session.draft && matches!(session.runtime, Runtime::Idle);
                    let bytes = if replay {
                        self.event_records
                            .get(id)
                            .map_or(0, SessionLog::retained_bytes)
                    } else {
                        0
                    };
                    (id.clone(), idle, replay, bytes)
                })
            })
            .collect();
        residents.sort_unstable_by_key(|(_, idle, _, _)| *idle);
        for (id, _, replay, bytes) in &residents {
            if *replay && *bytes > MAX_IDLE_REPLAY_BYTES {
                self.drop_background(id, cx);
            }
        }
        residents.retain(|(_, _, replay, bytes)| !*replay || *bytes <= MAX_IDLE_REPLAY_BYTES);
        let mut replay_count = residents.iter().filter(|(_, _, replay, _)| *replay).count();
        let mut replay_bytes: usize = residents.iter().map(|(_, _, _, bytes)| *bytes).sum();
        while replay_count > MAX_IDLE_REPLAY_RESIDENTS
            || replay_bytes > MAX_IDLE_REPLAY_BYTES
            || residents.len() > MAX_IDLE_RESIDENTS
        {
            let replay_over =
                replay_count > MAX_IDLE_REPLAY_RESIDENTS || replay_bytes > MAX_IDLE_REPLAY_BYTES;
            let oldest = residents
                .iter()
                .position(|(_, _, replay, _)| !replay_over || *replay)
                .expect("an exceeded replay budget has an idle replay resident");
            let (id, _, replay, bytes) = residents.remove(oldest);
            log::info!("evicting idle resident {id}");
            self.drop_background(&id, cx);
            replay_count -= usize::from(replay);
            replay_bytes -= bytes;
        }

        if armed.is_some() {
            return;
        }
        let session_id = session_id.to_string();
        let deadline = idle_since + self.resident_idle_grace;
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            smol::Timer::at(deadline).await;
            host_cx.enqueue(move |state, cx| state.reap_idle_resident(&session_id, idle_since, cx));
        });
    }

    /// The idle grace timer's expiry: act only if the resident is still in the
    /// idle period the timer was started for. A parked resident is forgotten;
    /// a viewed one keeps its timeline and only loses its provider process.
    pub(super) fn reap_idle_resident(
        &mut self,
        session_id: &str,
        idle_since: Instant,
        cx: &mut HostCx,
    ) {
        let still_idle = !self.pending_native_rewinds.contains_key(session_id)
            && self.residents.resident(session_id).is_some_and(|session| {
                session.idle_since == Some(idle_since) && session.fully_idle()
            });
        if !still_idle {
            return;
        }
        if self.residents.parked.contains_key(session_id) {
            self.drop_background(session_id, cx);
        } else if let Some(viewed) = self.residents.live.get_mut(session_id)
            && matches!(viewed.runtime, Runtime::Live(_))
        {
            log::info!("shutting down idle provider of viewed session {session_id}");
            viewed.shutdown_to_idle();
        }
    }

    /// Shut down and forget a parked session (archive/delete paths).
    pub(super) fn drop_background(&mut self, session_id: &str, cx: &mut HostCx) {
        self.revoke_computer_use_registration(session_id);
        self.clear_approvals(session_id);
        self.pending_native_rewinds.remove(session_id);
        if let Some(parked) = self.residents.evict(session_id)
            && let Runtime::Live(commands) = parked.runtime
        {
            let _ = commands.try_send(SessionCommand::Shutdown);
        }
        self.release_stale_session_logs(cx);
        self.reschedule_scheduled_wake(cx);
    }

    pub(super) fn close_orchestrator_children(&mut self, parent_id: &str, cx: &mut HostCx) {
        // Nested subagent mirrors hang under other mirrors, yet they belong to
        // the one provider process that just closed.
        let descendants = descendant_session_ids(&self.sessions, parent_id);
        let child_ids: Vec<_> = self
            .sessions
            .iter()
            .filter(|meta| {
                meta.parent_session_id.as_deref() == Some(parent_id)
                    || (meta.native_subagent.is_some() && descendants.contains(&meta.id))
            })
            .map(|meta| meta.id.clone())
            .collect();
        for child_id in child_ids {
            self.drop_background(&child_id, cx);
            self.revoke_preview_registration(&child_id);
            self.revoke_pull_request_registration(&child_id);
            self.revoke_orchestrate_child_registration(&child_id);
            self.revoke_control_registration(&child_id);
        }
        self.revoke_preview_registration(parent_id);
        self.revoke_pull_request_registration(parent_id);
        self.revoke_computer_use_registration(parent_id);
        self.revoke_control_registration(parent_id);
        if let Some(registration) = self.mcp.orchestrate_registrations.remove(parent_id)
            && let Some(tokens) = &self.mcp.orchestrate_tokens
        {
            tokens.revoke(&registration.bearer_token);
        }
    }

    pub(super) fn revoke_preview_registration(&mut self, session_id: &str) {
        if let Some(registration) = self.mcp.preview_registrations.remove(session_id)
            && let Some(tokens) = &self.mcp.preview_tokens
        {
            tokens.revoke(&registration.bearer_token);
        }
    }

    pub(super) fn report_error(&mut self, error: RuntimeError, cx: &mut HostCx) {
        log::error!("{error:?}");
        emit_runtime(cx, RuntimeEvent::Error(error));
    }
}
