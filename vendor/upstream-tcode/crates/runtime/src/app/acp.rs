use super::*;

impl AppState {
    /// Load the registry index (cache first, network when stale). Cheap enough
    /// to call every time the Providers page opens.
    pub fn refresh_acp_registry(&mut self, cx: &mut HostCx) {
        if self.acp_registry_loading {
            return;
        }
        self.acp_registry_loading = true;
        let data_dir = self.store.root().clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let cache_dir = data_dir.clone();
            let cached_registry = host_cx.unblock(move || cached(&cache_dir)).await;
            host_cx.enqueue(move |state, _cx| {
                if state.acp_registry_loading && state.acp_registry.is_none() {
                    state.acp_registry = cached_registry;
                }
            });
            let result = host_cx.unblock(move || load(&data_dir)).await;
            host_cx.enqueue(move |state, _cx| {
                state.acp_registry_loading = false;
                match result {
                    Ok(registry) => {
                        state.acp_registry = Some(registry);
                        state.acp_registry_error = None;
                    }
                    Err(err) => {
                        log::warn!("ACP registry unavailable: {err}");
                        state.acp_registry_error = Some(err.to_string());
                    }
                }
            });
        });
    }

    /// The marketplace list: every registry agent except the hidden adapters
    /// over our own native CLIs.
    pub(crate) fn acp_marketplace(&self) -> Vec<RegistryAgent> {
        self.acp_registry
            .as_ref()
            .map(|registry| visible_agents(registry).into_iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Runtime-owned marketplace views in the registry's visible ordering.
    pub(crate) fn acp_marketplace_items(&self) -> Vec<AcpMarketplaceItem> {
        let platform = platform_key();
        self.acp_registry
            .as_ref()
            .map(|registry| {
                visible_agents(registry)
                    .into_iter()
                    .map(|agent| AcpMarketplaceItem {
                        id: agent.id.clone(),
                        name: agent.name.clone(),
                        version: agent.version.clone(),
                        description: agent.description.clone(),
                        installed: self.settings.acp_agents.contains_key(&agent.id),
                        installing: self.acp_installing.contains(&agent.id),
                        supported: resolve_recipe(agent, &platform).is_some(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Download + install one registry agent, with a progress toast.
    pub fn install_acp_agent(&mut self, id: String, cx: &mut HostCx) {
        let Some(agent) = self
            .acp_marketplace()
            .into_iter()
            .find(|agent| agent.id == id)
        else {
            return;
        };
        if !self.acp_installing.insert(id.clone()) {
            return;
        }
        let operation = self.next_operation_id();
        let data_dir = self.store.root().clone();
        let name = agent.name.clone();
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::AcpInstallStarted {
                operation,
                name: name.clone(),
            }),
        );
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = host_cx
                .unblock(move || install(&agent, &data_dir, |_done, _total| {}))
                .await;
            host_cx.enqueue(move |state, cx| {
                state.acp_installing.remove(&id);
                match result {
                    Ok(installed) => {
                        state.settings.acp_agents.insert(id.clone(), installed);
                        state.persist_settings(cx);
                        emit_runtime(
                            cx,
                            RuntimeEvent::Toast(RuntimeToast::AcpInstallSucceeded {
                                operation,
                                name,
                            }),
                        );
                    }
                    Err(err) => emit_runtime(
                        cx,
                        RuntimeEvent::Toast(RuntimeToast::AcpInstallFailed {
                            operation,
                            name,
                            detail: err.to_string(),
                        }),
                    ),
                }
            });
        });
    }

    /// Remove an installed ACP agent (its files and its settings entry).
    pub fn remove_acp_agent(&mut self, id: &str, cx: &mut HostCx) {
        self.settings.acp_agents.remove(id);
        self.persist_settings(cx);
        let data_dir = self.store.root().clone();
        let id = id.to_string();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            host_cx
                .unblock(move || {
                    if let Err(err) = uninstall(&data_dir, &id) {
                        log::warn!("could not remove ACP agent {id}: {err}");
                    }
                })
                .await;
        });
    }

    /// Register a user-defined ACP agent (the escape hatch for anything not in
    /// the registry): an arbitrary command that speaks ACP over its stdio.
    pub fn add_custom_acp_agent(
        &mut self,
        name: String,
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cx: &mut HostCx,
    ) {
        let name = name.trim().to_string();
        let command = command.trim().to_string();
        if name.is_empty() || command.is_empty() {
            return;
        }
        let id = custom_acp_id(&name);
        self.settings.acp_agents.insert(
            id.clone(),
            InstalledAgent {
                id,
                name,
                version: String::new(),
                icon: None,
                launch: agent::AcpLaunch::Custom { command, args, env },
                enabled: true,
                env: Vec::new(),
                launch_args: None,
            },
        );
        self.persist_settings(cx);
    }

    /// Update one installed ACP agent in place (enable switch, env rows, args).
    pub fn update_acp_agent(&mut self, id: &str, patch: AcpAgentPatch, cx: &mut HostCx) {
        if let Some(agent) = self.settings.acp_agents.get_mut(id) {
            match patch {
                AcpAgentPatch::SetEnabled { enabled } => agent.enabled = enabled,
                AcpAgentPatch::SetLaunchOptions { env, launch_args } => {
                    agent.env = env;
                    agent.launch_args = launch_args;
                }
            }
            self.persist_settings(cx);
        }
    }

    pub(super) fn preview_draft_or_persist_active(&mut self, target_id: &str, cx: &mut HostCx) {
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        if active.draft {
            return;
        }
        active.meta.updated_at = now_secs();
        let meta = active.meta.clone();
        self.persist_meta(&meta, cx);
    }

    /// Point the active draft at an installed ACP agent (the model picker's
    /// provider rail). ACP agents have no model catalog: the agent publishes its
    /// models over the wire once the session starts.
    pub fn set_active_acp_agent(&mut self, target_id: &str, id: &str, cx: &mut HostCx) {
        let provider_commands = self.cached_provider_commands(ProviderKind::Acp, None, Some(id));
        let Some(active) = self.resident_mut(target_id) else {
            return;
        };
        if matches!(active.meta.provider, ProviderKind::Acp)
            && active.meta.acp_agent_id.as_deref() == Some(id)
        {
            return;
        }
        if !active.draft && !matches!(active.meta.provider, ProviderKind::Acp) {
            let source = active.pending_relay.clone().unwrap_or(PendingRelay {
                from_provider: active.meta.provider,
                from_model: active.meta.model.clone(),
                from_profile: active.meta.profile_id.clone(),
            });
            if active.pending_relay.is_some() && matches!(source.from_provider, ProviderKind::Acp) {
                active.pending_relay = None;
            } else if has_meaningful_history(&active.timeline) {
                active.pending_relay = Some(source);
            } else {
                active.resume_cursor_for_fresh_provider();
            }
        }
        active.meta.provider = ProviderKind::Acp;
        active.meta.acp_agent_id = Some(id.to_string());
        active.meta.model = None;
        active.meta.option_selections.clear();
        active.provider_options.clear();
        active.provider_commands = provider_commands;
        if active.pending_relay.is_some() {
            return;
        }
        self.preview_draft_or_persist_active(target_id, cx);
    }

    /// Reset the *preferences* to defaults. Anything the user had to configure
    /// or install survives: provider profiles and their credentials, the ACP
    /// agents on disk, and the legacy binary overrides — wiping those would cost
    /// re-authentication and re-downloads, which is never what "restore
    /// defaults" means. Sidebar/UI state (collapsed projects, favorites, sort,
    /// layout, last-visited) is preserved for the same reason, and `unknown`
    /// must survive so a newer build's keys are not destroyed by an older one.
    /// The theme is reset; the caller re-applies it to the window.
    pub fn reset_settings(&mut self, cx: &mut HostCx) {
        let settings = Settings {
            providers: self.settings.providers.clone(),
            profiles: self.settings.profiles.clone(),
            codex_binary: self.settings.codex_binary.clone(),
            claude_binary: self.settings.claude_binary.clone(),
            acp_agents: self.settings.acp_agents.clone(),
            sidebar_collapsed: self.settings.sidebar_collapsed,
            collapsed_projects: self.settings.collapsed_projects.clone(),
            favorite_models: self.settings.favorite_models.clone(),
            project_sort: self.settings.project_sort,
            sidebar_layout: self.settings.sidebar_layout,
            last_visited: self.settings.last_visited.clone(),
            unknown: self.settings.unknown.clone(),
            ..Settings::default()
        };
        self.update_settings(settings, cx);
    }
}

/// A stable settings key for a user-defined ACP agent, derived from its name.
pub(super) fn custom_acp_id(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("custom:{}", slug.trim_matches('-'))
}
