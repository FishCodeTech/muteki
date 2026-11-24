use super::*;

impl AppState {
    /// Select a provider-owned `model` (None = provider default) for the active
    /// session and persist it. On an unsent draft the model picker also selects
    /// its provider; an established session remains bound to its provider.
    /// Takes effect on the next provider (re)start; if a provider is currently
    /// live, the next `send_turn` restarts it (see `send_turn`).
    pub fn set_active_model(
        &mut self,
        target_id: &str,
        provider: ProviderKind,
        model: Option<String>,
        // Which provider profile the picked row belongs to (`None` = the built-in
        // profile for `provider`). Rebinding an established session's profile is
        // a backend change and goes through the relay confirmation, exactly like
        // a provider change.
        profile_id: Option<String>,
        cx: &mut HostCx,
    ) {
        let profile_id = profile_id.filter(|id| !Settings::is_builtin_profile_id(id));
        let remembered_effort = self.remembered_effort(provider, model.as_deref());
        let project_id = self
            .resident(target_id)
            .and_then(|active| active.meta.project_id.clone());
        let permission_default = self.initial_permission_selection(project_id.as_deref(), provider);
        let provider_commands =
            self.cached_provider_commands(provider, profile_id.as_deref(), None);
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        // In a draft the model picker is also the provider picker. The selected
        // row carries its provider explicitly: model ids are provider-defined
        // and custom ids cannot be classified safely from their spelling.
        if active.draft {
            if active.meta.provider == provider
                && active.meta.model == model
                && active.meta.profile_id == profile_id
            {
                return;
            }
            let same_provider = active.meta.provider == provider;
            let permission = if same_provider {
                permission_control(provider).and_then(|descriptor| {
                    let id = match descriptor {
                        OptionDescriptor::Select { id, .. }
                        | OptionDescriptor::Boolean { id, .. } => id,
                    };
                    active
                        .meta
                        .option_selections
                        .iter()
                        .find(|selection| selection.id == id)
                        .cloned()
                })
            } else {
                None
            };
            active.meta.provider = provider;
            active.meta.acp_agent_id = None;
            active.meta.profile_id = profile_id;
            active.meta.model = model;
            active.meta.option_selections.clear();
            active.meta.option_selections.extend(remembered_effort);
            active
                .meta
                .option_selections
                .extend(permission.or(permission_default));
            if !same_provider {
                active.provider_options.clear();
                active.confirmed_option_selections.clear();
            }
            active.provider_commands = provider_commands;
            return;
        }
        // Established sessions can preview a different provider — or a
        // different profile of the same provider, which is a different backend
        // with its own isolated home — but the provider-native cursor is
        // retained until the user confirms a relay.
        if active.meta.provider != provider || active.meta.profile_id != profile_id {
            let source = active.pending_relay.clone().unwrap_or(PendingRelay {
                from_provider: active.meta.provider,
                from_model: active.meta.model.clone(),
                from_profile: active.meta.profile_id.clone(),
            });
            let mut abandoned = false;
            if active.pending_relay.is_some()
                && source.from_provider == provider
                && source.from_profile == profile_id
            {
                active.pending_relay = None;
            } else if has_meaningful_history(&active.timeline) {
                active.pending_relay = Some(source);
            } else {
                active.resume_cursor_for_fresh_provider();
                abandoned = true;
            }
            active.meta.provider = provider;
            active.meta.acp_agent_id = None;
            active.meta.profile_id = profile_id;
            active.meta.model = model;
            active.meta.option_selections.clear();
            active.meta.option_selections.extend(remembered_effort);
            active.meta.option_selections.extend(permission_default);
            active.provider_commands = provider_commands;
            active.provider_options.clear();
            active.confirmed_option_selections.clear();
            if active.pending_relay.is_some() {
                return;
            }
            if abandoned {
                self.close_abandoned_turn(target_id, cx);
            }
            self.preview_draft_or_persist_active(target_id, cx);
            return;
        }
        if active.meta.model == model {
            return;
        }
        let permission_id = permission_control(provider).map(|descriptor| match descriptor {
            OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => id,
        });
        active.meta.model = model;
        active
            .meta
            .option_selections
            .retain(|selection| Some(selection.id.as_str()) == permission_id.as_deref());
        active.meta.option_selections.extend(remembered_effort);
        if active.pending_relay.is_some() {
            return;
        }
        self.preview_draft_or_persist_active(target_id, cx);
    }

    /// Set (or clear) the persisted value of one option descriptor for the
    /// active session. `value` is a string (select) or bool (boolean); passing
    /// `None` removes the selection so it resolves back to its default. Takes
    /// effect per the restart machinery (see `send_turn`).
    pub fn set_active_option(
        &mut self,
        target_id: &str,
        id: &str,
        value: Option<serde_json::Value>,
        cx: &mut HostCx,
    ) {
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        let permission = active.permission_descriptor();
        let value = value.or_else(|| {
            permission.as_ref().and_then(|descriptor| match descriptor {
                OptionDescriptor::Select {
                    id: option_id,
                    default_value,
                    ..
                } if option_id == id => default_value
                    .as_ref()
                    .map(|value| serde_json::Value::String(value.clone())),
                OptionDescriptor::Boolean {
                    id: option_id,
                    default_value,
                    ..
                } if option_id == id => Some(serde_json::Value::Bool(*default_value)),
                _ => None,
            })
        });
        active.meta.option_selections.retain(|s| s.id != id);
        if let Some(value) = value {
            active.meta.option_selections.push(OptionSelection {
                id: id.to_string(),
                value,
            });
        }
        let permission_push = permission
            .as_ref()
            .is_some_and(|descriptor| match descriptor {
                OptionDescriptor::Select {
                    id: option_id,
                    apply,
                    ..
                }
                | OptionDescriptor::Boolean {
                    id: option_id,
                    apply,
                    ..
                } => option_id == id && *apply != ApplyTiming::Restart,
            });
        let permission_restart = permission
            .as_ref()
            .is_some_and(|descriptor| match descriptor {
                OptionDescriptor::Select {
                    id: option_id,
                    apply,
                    ..
                }
                | OptionDescriptor::Boolean {
                    id: option_id,
                    apply,
                    ..
                } => option_id == id && *apply == ApplyTiming::Restart,
            });
        if (permission_push
            || (!permission_restart && active.meta.provider.caps().live_option_push.supports(id)))
            && let Runtime::Live(commands) = &active.runtime
            && let Some(selection) = active.meta.option_selections.iter().find(|s| s.id == id)
        {
            let _ = commands.try_send(SessionCommand::SetOption {
                id: selection.id.clone(),
                value: selection.value.clone(),
            });
            if !permission_push {
                active
                    .live_option_selections
                    .retain(|selection| selection.id != id);
                active.live_option_selections.push(selection.clone());
            }
        }
        self.preview_draft_or_persist_active(target_id, cx);
    }

    /// Load the local branches for the active session's cwd in the background
    /// (called when the checkout-row popover opens).
    pub fn load_branches(&mut self, target_id: &str, cx: &mut HostCx) {
        let Some(active) = self.resident(target_id) else {
            return;
        };
        let cwd = active.meta.cwd.clone();
        let session_id = active.meta.id.clone();
        let target_id = target_id.to_string();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let branches = host_cx.unblock(move || list_git_branches(&cwd)).await;
            host_cx.enqueue(move |state, _cx| {
                if let Some(active) = state.resident_mut(&target_id)
                    && active.meta.id == session_id
                {
                    active.branches = branches;
                }
            });
        });
    }

    /// Check out `branch` in the active session's cwd, if the working tree is
    /// clean. Runs git off the main thread; reports success/failure as an
    /// `RuntimeEvent` the chat view turns into a notification, and on the
    /// returned channel.
    pub fn checkout_branch(
        &mut self,
        target_id: &str,
        branch: String,
        cx: &mut HostCx,
    ) -> smol::channel::Receiver<Result<(), String>> {
        let (done, outcome) = smol::channel::bounded(1);
        let Some(active) = self.resident(target_id) else {
            let _ = done.try_send(Err("the thread is not open".into()));
            return outcome;
        };
        if Self::checkout_blocked(active) {
            let _ = done.try_send(Err("busy: a turn is running in this thread".into()));
            return outcome;
        }
        let cwd = active.meta.cwd.clone();
        let session_id = active.meta.id.clone();
        let branch_for_task = branch.clone();
        let target_id = target_id.to_string();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || checkout_if_clean(&cwd, &branch_for_task))
                .await;
            host_cx.enqueue(move |state, cx| {
                let checked_out = match result {
                    Ok(()) => {
                        if let Some(cwd) = state
                            .resident(&target_id)
                            .filter(|active| active.meta.id == session_id)
                            .map(|active| active.meta.cwd.clone())
                        {
                            state.refresh_session_git_branch(session_id.clone(), cwd, cx);
                        }
                        emit_runtime(
                            cx,
                            RuntimeEvent::Notice(RuntimeNotice::SwitchedBranch { branch }),
                        );
                        Ok(())
                    }
                    Err(CheckoutError::Dirty) => {
                        emit_runtime(cx, RuntimeEvent::Error(RuntimeError::DirtyTree));
                        Err("the working tree has uncommitted changes".into())
                    }
                    Err(CheckoutError::Git(message)) => {
                        emit_runtime(
                            cx,
                            RuntimeEvent::Error(RuntimeError::External(message.clone())),
                        );
                        Err(message)
                    }
                };
                let _ = done.try_send(checked_out);
            });
        });
        outcome
    }

    /// Toggle a model id in the persisted favorites list.
    pub fn toggle_favorite_model(&mut self, model: &str, cx: &mut HostCx) {
        let mut settings = self.settings.clone();
        if let Some(pos) = settings.favorite_models.iter().position(|m| m == model) {
            settings.favorite_models.remove(pos);
        } else {
            settings.favorite_models.push(model.to_string());
        }
        self.update_settings(settings, cx);
    }
}
