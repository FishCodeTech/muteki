use super::*;
use tcode_protocol::{ProviderUpdateAvailable, ProviderUpdateRun};

#[derive(Default)]
struct VersionCheckRound {
    pending: usize,
    updates: Vec<ProviderUpdateAvailable>,
}

struct UpdateRun {
    operation: RuntimeOperationId,
    status: ProviderUpdateRun,
    pending: std::collections::VecDeque<ProviderKind>,
}

#[derive(Default)]
struct ModelRefresh {
    configuration: ProviderSettings,
    profile_revision: u64,
    network: bool,
    generation: u64,
    loading: bool,
}

struct SecretWrite {
    completion: smol::channel::Receiver<Result<(), String>>,
    result: Option<Result<(), String>>,
}

type SecretWrites = HashMap<String, HashMap<String, Arc<smol::lock::Mutex<SecretWrite>>>>;

pub struct ProviderCatalog {
    pub model_catalogs: HashMap<ProviderKind, Vec<ModelSpec>>,
    model_refreshes: HashMap<ProviderKind, ModelRefresh>,
    pub provider_versions: HashMap<ProviderKind, ProviderVersionState>,
    version_rounds: HashMap<u64, VersionCheckRound>,
    next_version_round: u64,
    update_run: Option<UpdateRun>,
    pub tcode_update: TcodeUpdateState,
    pub provider_snapshots: HashMap<String, ProviderSnapshot>,
    /// Latest account usage per profile id (Codex / Claude Code).
    pub provider_usage: HashMap<String, tcode_core::usage::ProviderUsage>,
    /// Profile ids with a usage fetch in flight.
    pub usage_checking: HashSet<String>,
    profile_revisions: HashMap<String, u64>,
    secret_writes: SecretWrites,
    pub(super) provider_secret_names: HashMap<String, HashSet<String>>,
}

impl ProviderCatalog {
    pub(super) fn new(
        model_catalogs: HashMap<ProviderKind, Vec<ModelSpec>>,
        provider_secret_names: HashMap<String, HashSet<String>>,
    ) -> Self {
        Self {
            model_catalogs,
            model_refreshes: HashMap::new(),
            provider_versions: HashMap::new(),
            version_rounds: HashMap::new(),
            next_version_round: 0,
            update_run: None,
            tcode_update: TcodeUpdateState::default(),
            provider_snapshots: HashMap::new(),
            provider_usage: HashMap::new(),
            usage_checking: HashSet::new(),
            profile_revisions: HashMap::new(),
            secret_writes: HashMap::new(),
            provider_secret_names,
        }
    }

    fn begin_model_refresh(
        &mut self,
        provider: ProviderKind,
        configuration: ProviderSettings,
        profile_revision: u64,
        network: bool,
    ) -> Option<u64> {
        let refresh = self.model_refreshes.entry(provider).or_default();
        if refresh.loading
            && refresh.configuration == configuration
            && refresh.profile_revision == profile_revision
            && refresh.network == network
        {
            return None;
        }
        *refresh = ModelRefresh {
            configuration,
            profile_revision,
            network,
            generation: refresh.generation + 1,
            loading: true,
        };
        Some(refresh.generation)
    }

    fn finish_model_refresh(&mut self, provider: ProviderKind, generation: u64) -> bool {
        let Some(refresh) = self.model_refreshes.get_mut(&provider) else {
            return false;
        };
        if refresh.generation != generation {
            return false;
        }
        refresh.loading = false;
        true
    }

    pub(super) fn invalidate_profile_reads(&mut self, id: &str) {
        self.provider_snapshots.remove(id);
        self.provider_usage.remove(id);
        self.usage_checking.remove(id);
        *self.profile_revisions.entry(id.to_owned()).or_default() += 1;
    }

    pub(super) fn invalidate_versions(&mut self) {
        for status in self.provider_versions.values_mut() {
            *status = ProviderVersionState {
                revision: status.revision + 1,
                updating: status.updating,
                ..Default::default()
            };
        }
    }

    fn complete_version_check(
        &mut self,
        round: u64,
        provider: ProviderKind,
        revision: u64,
        installation: Option<Installation>,
        assessment: provider_updates::Assessment,
    ) -> Option<RuntimeToast> {
        let status = self.provider_versions.entry(provider).or_default();
        let update = if status.revision == revision {
            let already = status.update_available;
            status.checking = false;
            status.installation = installation;
            status.installed = assessment.current;
            status.latest = assessment.latest;
            status.update_available = assessment.update_available;
            if status.update_available
                && !already
                && let Some(version) = &status.latest
            {
                Some(ProviderUpdateAvailable {
                    provider,
                    version: version.clone(),
                    automatic: status
                        .installation
                        .as_ref()
                        .and_then(|installation| installation.update.as_ref())
                        .is_some_and(|command| !command.requires_terminal),
                })
            } else {
                None
            }
        } else {
            None
        };
        if let Some(round_state) = self.version_rounds.get_mut(&round) {
            if let Some(update) = update {
                round_state.updates.push(update);
            }
            round_state.pending -= 1;
            if round_state.pending == 0 {
                let mut round = self.version_rounds.remove(&round).unwrap();
                round.updates.sort_by_key(|update| {
                    ProviderKind::NATIVE
                        .iter()
                        .position(|provider| *provider == update.provider)
                });
                return (!round.updates.is_empty()).then_some(
                    RuntimeToast::ProviderUpdatesAvailable {
                        updates: round.updates,
                    },
                );
            }
        }
        None
    }

    fn complete_usage(
        &mut self,
        id: String,
        revision: u64,
        usage: Option<tcode_core::usage::ProviderUsage>,
    ) {
        if self.profile_revisions.get(&id).copied().unwrap_or(0) != revision {
            return;
        }
        self.usage_checking.remove(&id);
        let Some(mut usage) = usage else {
            return;
        };
        // A failed fetch keeps the last successful windows on screen, dated
        // by that fetch, so a transient CLI error does not blank the numbers.
        if usage.error.is_some()
            && usage.windows.is_empty()
            && let Some(previous) = self.provider_usage.get(&id)
            && !previous.windows.is_empty()
        {
            usage.fetched_at = previous.fetched_at;
            usage.windows = previous.windows.clone();
            if usage.plan.is_none() {
                usage.plan = previous.plan.clone();
            }
        }
        self.provider_usage.insert(id, usage);
    }

    pub(super) fn status_snapshot(
        &self,
        acp_marketplace_items: Vec<AcpMarketplaceItem>,
        acp_registry_loading: bool,
        acp_registry_error: Option<String>,
        acp_installing: HashSet<String>,
        plugins: Vec<tcode_protocol::ProviderPluginCatalog>,
    ) -> ProvidersStatus {
        ProvidersStatus {
            model_catalogs: self.model_catalogs.clone(),
            models_loading: self
                .model_refreshes
                .iter()
                .map(|(provider, refresh)| (*provider, refresh.loading))
                .collect(),
            provider_versions: self
                .provider_versions
                .iter()
                .map(|(&provider, status)| {
                    let command = status
                        .installation
                        .as_ref()
                        .and_then(|installation| installation.update.as_ref());
                    (
                        provider,
                        ProtocolProviderVersionStatus {
                            installed: status.installed.clone(),
                            latest: status.latest.clone(),
                            update_available: status.update_available,
                            checking: status.checking,
                            updating: status.updating,
                            update_command: command.map(|command| command.display()),
                            update_command_summary: command.map(|command| command.summary()),
                            update_requires_terminal: command
                                .is_some_and(|command| command.requires_terminal),
                        },
                    )
                })
                .collect(),
            update_run: self.update_run.as_ref().map(|run| run.status.clone()),
            tcode_update: TcodeUpdateStatus {
                current: self.tcode_update.current.clone(),
                latest: self.tcode_update.latest.clone(),
                release_url: self.tcode_update.release_url.clone(),
                update_available: self.tcode_update.update_available,
                checking: self.tcode_update.checking,
            },
            provider_snapshots: self.provider_snapshots.clone(),
            provider_usage: self.provider_usage.clone(),
            usage_checking: self.usage_checking.clone(),
            acp_marketplace_items,
            acp_registry_loading,
            acp_registry_error,
            acp_installing,
            providers_checked_at: self
                .provider_snapshots
                .values()
                .filter_map(|snapshot| snapshot.checked_at)
                .max(),
            providers_checking: self
                .provider_snapshots
                .values()
                .any(|snapshot| snapshot.checking)
                || self
                    .provider_versions
                    .values()
                    .any(|status| status.checking)
                || self.tcode_update.checking,
            secret_names: self.provider_secret_names.clone(),
            plugins,
        }
    }
}

impl AppState {
    /// Kick off a background refresh of every provider's model catalog (called
    /// at app start and after a binary-path change). Results update
    /// `model_catalogs` and are persisted so the next launch is instant.
    pub fn refresh_model_catalogs(&mut self, cx: &mut HostCx) {
        for provider in ProviderKind::NATIVE {
            let profile_id = Settings::builtin_profile_id(provider).to_string();
            let configuration = self.settings.provider(provider);
            let profile_revision = self
                .providers
                .profile_revisions
                .get(&profile_id)
                .copied()
                .unwrap_or(0);
            let network = self.provider_update_checks_enabled();
            let Some(generation) = self.providers.begin_model_refresh(
                provider,
                configuration.clone(),
                profile_revision,
                network,
            ) else {
                continue;
            };
            let binary = configuration.binary_path.clone();
            let launch_env = self.prepare_profile_env(&profile_id, cx);
            let store = self.store.clone();
            // Claude's manifest cache lives next to the persisted catalogs and
            // phones home only under the same switch as CLI update checks.
            let refresh = CatalogRefresh {
                cache_dir: Some(store.root().clone()),
                network,
            };
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let result = match launch_env.await {
                    Ok(launch_env) => list_models(provider, binary, launch_env, refresh)
                        .await
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error),
                };
                host_cx.enqueue(move |state, _cx| {
                    if !state.providers.finish_model_refresh(provider, generation) {
                        return;
                    }
                    if state.settings.provider(provider) != configuration
                        || state
                            .providers
                            .profile_revisions
                            .get(&profile_id)
                            .copied()
                            .unwrap_or(0)
                            != profile_revision
                        || state.provider_update_checks_enabled() != network
                    {
                        return;
                    }
                    match result {
                        Ok(models) if !models.is_empty() => {
                            if let Err(err) = store.save_models(provider, &models) {
                                log::warn!("failed to persist {provider:?} model catalog: {err}");
                            }
                            state.providers.model_catalogs.insert(provider, models);
                        }
                        Ok(_) => log::info!("{provider:?} returned an empty model catalog"),
                        Err(err) => log::warn!("failed to list {provider:?} models: {err}"),
                    }
                });
            });
        }
    }

    /// Whether the on-launch provider version check is enabled (default on).
    pub fn provider_update_checks_enabled(&self) -> bool {
        !self.settings.provider_update_checks_disabled
    }

    /// Resolve the binary path for a built-in provider profile.
    pub(super) fn resolve_provider_binary(&self, provider: ProviderKind) -> Option<PathBuf> {
        self.resolve_profile_binary(Settings::builtin_profile_id(provider))
    }

    /// Resolve the binary path for a profile: its settings override, else a
    /// PATH lookup of the protocol's bare command name.
    pub(super) fn resolve_profile_binary(&self, profile_id: &str) -> Option<PathBuf> {
        let profile = self.settings.resolved_profile(profile_id)?;
        profile
            .settings
            .binary_path
            .or_else(|| agent::find_on_path(&default_program(profile.kind)))
    }

    /// Re-run everything that depends on *how* a provider's CLI is launched
    /// (binary path, home, environment): its model catalog and its status probe.
    pub fn reload_provider(&mut self, cx: &mut HostCx) {
        self.refresh_model_catalogs(cx);
        self.refresh_provider_status(cx);
        if self.provider_update_checks_enabled() {
            self.check_provider_versions(cx);
        }
    }

    // A *profile* is a named configuration on top of a protocol `ProviderKind`.
    // The built-in native-provider cards are profiles too, with the stable ids
    // of `Settings::builtin_profile_id`.
    // The model catalog and update-check version stay keyed by kind; status
    // probes and card config (env, binary, home, accent, custom/hidden models)
    // are profile-specific, as are secrets.

    /// Every selectable native profile, grouped by kind. ACP is handled
    /// separately through the installed-agent list.
    pub(super) fn all_profiles(&self) -> Vec<ResolvedProfile> {
        ProviderKind::NATIVE
            .iter()
            .flat_map(|kind| self.settings.profiles_for_kind(*kind))
            .collect()
    }

    /// Apply a serializable edit to one profile's card settings, routing built-in
    /// ids to their `providers` card and user ids to the `profiles` map.
    pub fn update_profile_settings(
        &mut self,
        id: &str,
        patch: ProfileSettingsPatch,
        cx: &mut HostCx,
    ) {
        let mut settings = self.settings.clone();
        let target = if let Some(kind) = Settings::builtin_kind_from_id(id) {
            settings.provider_mut(kind)
        } else if let Some(profile) = settings.profiles.get_mut(id) {
            &mut profile.settings
        } else {
            return;
        };
        match patch {
            ProfileSettingsPatch::SetEnabled { enabled } => target.enabled = enabled,
            ProfileSettingsPatch::ReplaceConfiguration(configuration) => {
                target.display_name = configuration.display_name;
                target.accent_color = configuration.accent_color;
                target.env = configuration.env;
                target.binary_path = configuration.binary_path;
                target.home_path = configuration.home_path;
                target.launch_args = configuration.launch_args;
                target.pi = configuration.pi;
                target.custom_models = configuration.custom_models;
                target.hidden_models = configuration.hidden_models;
            }
        }
        self.update_settings(settings, cx);
    }

    /// Store (or clear) one sensitive env value for a profile in `secrets.json`.
    pub fn set_profile_secret(
        &mut self,
        id: &str,
        name: &str,
        value: Option<&str>,
        cx: &mut HostCx,
    ) {
        self.providers.invalidate_profile_reads(id);
        if Settings::builtin_kind_from_id(id).is_some() {
            self.providers.invalidate_versions();
        }
        let (completion, pending) = smol::channel::bounded(1);
        self.providers
            .secret_writes
            .entry(id.to_owned())
            .or_default()
            .insert(
                name.to_owned(),
                Arc::new(smol::lock::Mutex::new(SecretWrite {
                    completion: pending,
                    result: None,
                })),
            );
        self.enqueue_store_write(
            StoreWrite::SetProfileSecret {
                profile_id: id.to_string(),
                key: name.to_string(),
                value: value.map(str::to_string),
                completion,
            },
            cx,
        );
        let names = self
            .providers
            .provider_secret_names
            .entry(id.to_string())
            .or_default();
        if value.is_some() {
            names.insert(name.to_string());
        } else {
            names.remove(name);
        }
    }

    /// Create a first-class *third-party* Claude Code profile from the Add-agent
    /// dialog: a named endpoint (Kimi preset or a custom Anthropic-compatible
    /// URL). Wires the three env vars, registers the model as a custom slug so it
    /// shows in the picker, gives the profile its own isolated `HOME` (seeded so
    /// `claude` runs non-interactively and never touches the official
    /// `~/.claude`), and stores the API key in `secrets.json`. Returns the id.
    pub fn create_third_party_profile(
        &mut self,
        name: &str,
        base_url: &str,
        model: Option<&str>,
        api_key: &str,
        cx: &mut HostCx,
    ) -> String {
        let name = name.trim();
        let name = if name.is_empty() { "Third-party" } else { name };
        let id = self.settings.allocate_profile_id(name);

        // Each third-party Claude profile gets an isolated HOME so its auth /
        // config never collides with the official Claude login. Seed onboarding
        // so the CLI starts straight into API-key mode.
        let home = self.store.root().join("profile-homes").join(&id);

        let mut env = vec![EnvVar {
            name: "ANTHROPIC_BASE_URL".into(),
            value: base_url.trim().to_string(),
            sensitive: false,
        }];
        if let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) {
            env.push(EnvVar {
                name: "ANTHROPIC_MODEL".into(),
                value: model.to_string(),
                sensitive: false,
            });
        }
        env.push(EnvVar {
            name: "ANTHROPIC_API_KEY".into(),
            value: String::new(),
            sensitive: true,
        });
        let custom_models = model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| vec![m.to_string()])
            .unwrap_or_default();

        let profile = ProviderProfile {
            kind: ProviderKind::ClaudeCode,
            settings: ProviderSettings {
                display_name: Some(name.to_string()),
                env,
                custom_models,
                home_path: Some(home.clone()),
                ..ProviderSettings::default()
            },
        };
        let result_id = id.clone();
        let api_key = api_key.trim().to_string();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            host_cx
                .unblock(move || {
                    let _ = std::fs::create_dir_all(&home);
                    let _ = std::fs::write(
                        home.join(".claude.json"),
                        r#"{"hasCompletedOnboarding":true,"bypassPermissionsModeAccepted":true}"#,
                    );
                })
                .await;
            host_cx.enqueue(move |state, cx| {
                if state.settings.profiles.contains_key(&id) {
                    return;
                }
                let mut settings = state.settings.clone();
                settings.profiles.insert(id.clone(), profile);
                state.update_settings(settings, cx);
                state.set_profile_secret(&id, "ANTHROPIC_API_KEY", Some(&api_key), cx);
            });
        });
        result_id
    }

    /// Delete a user profile: remove its card, its secrets, and detach any
    /// sessions still pointing at it (they fall back to the built-in profile).
    /// Built-in ids are ignored.
    pub fn delete_profile(&mut self, id: &str, cx: &mut HostCx) {
        if Settings::is_builtin_profile_id(id) {
            return;
        }
        let mut settings = self.settings.clone();
        if settings.profiles.remove(id).is_none() {
            return;
        }
        self.enqueue_store_write(StoreWrite::ClearProfileSecrets(id.to_string()), cx);
        self.providers.provider_secret_names.remove(id);
        self.providers.secret_writes.remove(id);
        self.update_settings(settings, cx);
    }

    /// Probe every provider profile: is the CLI there, what version, and who is signed
    /// in? Runs the same `--version` call the version check uses, plus the
    /// provider's own auth surface where one is unambiguous (`claude auth
    /// status --json`; Codex's `auth.json`). Multi-provider CLIs report an
    /// indeterminate auth state until their model/session requests run.
    pub fn refresh_provider_status(&mut self, cx: &mut HostCx) {
        for profile in self.all_profiles() {
            let profile_id = profile.id.clone();
            let provider = profile.kind;
            let revision = self
                .providers
                .profile_revisions
                .get(&profile_id)
                .copied()
                .unwrap_or(0);
            let snapshot = self
                .providers
                .provider_snapshots
                .entry(profile_id.clone())
                .or_default();
            if snapshot.checking {
                continue;
            }
            snapshot.checking = true;
            let binary = self.resolve_profile_binary(&profile_id);
            let launch_env = self.prepare_profile_env(&profile_id, cx);
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let snapshot = match launch_env.await {
                    Ok(launch_env) => Ok(probe_provider(provider, binary, launch_env).await),
                    Err(error) => Err(error),
                };
                log::info!("probe {provider:?} profile {profile_id} -> {snapshot:?}");
                host_cx.enqueue(move |state, cx| {
                    if state
                        .providers
                        .profile_revisions
                        .get(&profile_id)
                        .copied()
                        .unwrap_or(0)
                        != revision
                        || state.settings.resolved_profile(&profile_id).as_ref() != Some(&profile)
                    {
                        return;
                    }
                    match snapshot {
                        Ok(snapshot) => {
                            state
                                .providers
                                .provider_snapshots
                                .insert(profile_id, snapshot);
                        }
                        Err(error) => {
                            if let Some(snapshot) =
                                state.providers.provider_snapshots.get_mut(&profile_id)
                            {
                                snapshot.checking = false;
                            }
                            state.report_error(RuntimeError::PersistSettings { error }, cx);
                        }
                    }
                });
            });
        }
    }

    /// Fetch account usage / rate-limit windows for every usage-capable profile.
    pub fn refresh_provider_usage(&mut self, cx: &mut HostCx) {
        self.refresh_provider_usage_inner(cx, false);
    }

    /// Refresh usage unless the profile already has a result from the last two
    /// minutes. Called after turns to avoid repeatedly polling provider CLIs.
    pub fn refresh_provider_usage_if_stale(&mut self, cx: &mut HostCx) {
        self.refresh_provider_usage_inner(cx, true);
    }

    fn refresh_provider_usage_inner(&mut self, cx: &mut HostCx, stale_only: bool) {
        let now = now_secs();
        for profile in self.all_profiles() {
            let supported = profile.supports_account_usage();
            let profile_id = profile.id.clone();
            let provider = profile.kind;
            if !supported
                || self
                    .providers
                    .provider_snapshots
                    .get(&profile_id)
                    .is_some_and(|snapshot| !snapshot.installed)
                || self.providers.usage_checking.contains(&profile_id)
                || stale_only
                    && self
                        .providers
                        .provider_usage
                        .get(&profile_id)
                        .is_some_and(|usage| now.saturating_sub(usage.fetched_at) < 120)
            {
                continue;
            }
            self.providers.usage_checking.insert(profile_id.clone());
            let revision = self
                .providers
                .profile_revisions
                .get(&profile_id)
                .copied()
                .unwrap_or(0);
            let binary = self.resolve_profile_binary(&profile_id);
            let launch_env = self.prepare_profile_env(&profile_id, cx);
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let launch_env = match launch_env.await {
                    Ok(env) => env,
                    Err(error) => {
                        host_cx.enqueue(move |state, cx| {
                            state.providers.complete_usage(profile_id, revision, None);
                            state.report_error(RuntimeError::PersistSettings { error }, cx);
                        });
                        return;
                    }
                };
                let usage = tcode_services::provider_usage::fetch_provider_usage(
                    provider, binary, launch_env,
                )
                .await;
                host_cx.enqueue(move |state, _cx| {
                    if state.settings.resolved_profile(&profile_id).as_ref() == Some(&profile) {
                        state.providers.complete_usage(profile_id, revision, usage);
                    }
                });
            });
        }
    }

    /// Check every provider and the running tcode build in the background,
    /// emitting one merged toast for newly available CLI updates per round.
    pub fn check_provider_versions(&mut self, cx: &mut HostCx) {
        let round = self.providers.next_version_round;
        self.providers.next_version_round += 1;
        self.providers
            .version_rounds
            .insert(round, VersionCheckRound::default());
        for provider in ProviderKind::NATIVE {
            let binary = self.resolve_provider_binary(provider);
            let status = self
                .providers
                .provider_versions
                .entry(provider)
                .or_default();
            if status.checking || status.updating {
                continue;
            }
            status.checking = true;
            self.providers
                .version_rounds
                .get_mut(&round)
                .unwrap()
                .pending += 1;
            let revision = status.revision;
            let program = binary
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| default_program(provider));
            let launch_env = self.prepare_profile_env(Settings::builtin_profile_id(provider), cx);
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let env = match launch_env.await {
                    Ok(env) => env.pairs(provider),
                    Err(error) => {
                        host_cx.enqueue(move |state, cx| {
                            let assessment = provider_updates::check(ProviderCheckInput {
                                installed_output: None,
                                latest_output: None,
                            });
                            if let Some(toast) = state
                                .providers
                                .complete_version_check(round, provider, revision, None, assessment)
                            {
                                emit_runtime(cx, RuntimeEvent::Toast(toast));
                            }
                            state.report_error(RuntimeError::PersistSettings { error }, cx);
                        });
                        return;
                    }
                };
                let installed = run_capture_env(&program, &["--version"], &env).await;
                let installation = match binary.as_deref() {
                    Some(binary) => {
                        Some(provider_updates::resolve_installation(provider, binary, &env).await)
                    }
                    None => None,
                };
                let latest = match installation.as_ref() {
                    Some(installation) => {
                        let installation = installation.clone();
                        host_cx
                            .unblock(move || provider_updates::latest_version(&installation))
                            .await
                    }
                    None => None,
                };
                let assessment = provider_updates::check(ProviderCheckInput {
                    installed_output: installed.as_deref(),
                    latest_output: latest.as_deref(),
                });
                host_cx.enqueue(move |state, cx| {
                    if let Some(toast) = state.providers.complete_version_check(
                        round,
                        provider,
                        revision,
                        installation,
                        assessment,
                    ) {
                        emit_runtime(cx, RuntimeEvent::Toast(toast));
                    }
                });
            });
        }

        if self.providers.version_rounds[&round].pending == 0 {
            self.providers.version_rounds.remove(&round);
        }
        if self.providers.tcode_update.checking {
            return;
        }
        self.providers.tcode_update.checking = true;
        let current = self.providers.tcode_update.current.clone();
        let releases = self.releases.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let fetched = host_cx.unblock(move || releases.latest_json()).await;
            let fetched = match &fetched {
                Ok(bytes) => Ok(bytes.as_slice()),
                Err(error) => Err(*error),
            };
            let assessment = app_releases::check(&current, fetched);
            host_cx.enqueue(move |state, cx| {
                let already = state.providers.tcode_update.update_available;
                let status = &mut state.providers.tcode_update;
                status.checking = false;
                status.latest = assessment.latest;
                status.release_url = assessment.release_url;
                status.update_available = assessment.update_available;
                if status.update_available
                    && !already
                    && let Some(version) = &status.latest
                {
                    emit_runtime(
                        cx,
                        RuntimeEvent::Notice(RuntimeNotice::TcodeUpdateAvailable {
                            version: version.clone(),
                        }),
                    );
                }
            });
        });
    }

    pub fn update_providers(&mut self, providers: Vec<ProviderKind>, cx: &mut HostCx) {
        if self.providers.update_run.is_some() {
            return;
        }
        let mut seen = HashSet::new();
        let pending: std::collections::VecDeque<_> = providers
            .into_iter()
            .filter(|provider| seen.insert(*provider))
            .collect();
        if pending.is_empty() {
            return;
        }
        let operation = self.next_operation_id();
        let status = ProviderUpdateRun {
            total: pending.len(),
            completed: 0,
            current: pending.front().copied(),
            failed: Vec::new(),
        };
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::ProviderUpdateStarted {
                operation,
                run: status.clone(),
            }),
        );
        self.providers.update_run = Some(UpdateRun {
            operation,
            status,
            pending,
        });
        self.start_next_provider_update(cx);
    }

    fn complete_provider_update(&mut self, provider: ProviderKind, ok: bool, cx: &mut HostCx) {
        if let Some(status) = self.providers.provider_versions.get_mut(&provider) {
            status.updating = false;
            if ok {
                status.update_available = false;
            }
        }
        let Some(run) = self.providers.update_run.as_mut() else {
            return;
        };
        run.status.completed += 1;
        if !ok {
            run.status.failed.push(provider);
        }
        self.start_next_provider_update(cx);
    }

    fn start_next_provider_update(&mut self, cx: &mut HostCx) {
        let Some(run) = self.providers.update_run.as_mut() else {
            return;
        };
        let Some(provider) = run.pending.pop_front() else {
            let mut run = self.providers.update_run.take().unwrap();
            run.status.current = None;
            emit_runtime(
                cx,
                RuntimeEvent::Toast(RuntimeToast::ProviderUpdateFinished {
                    operation: run.operation,
                    run: run.status,
                }),
            );
            self.refresh_provider_status(cx);
            self.check_provider_versions(cx);
            return;
        };
        run.status.current = Some(provider);
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::ProviderUpdateProgress {
                operation: run.operation,
                run: run.status.clone(),
            }),
        );
        let installation = self
            .providers
            .provider_versions
            .get(&provider)
            .and_then(|s| s.installation.clone());
        let Some(installation) = installation.filter(|installation| {
            installation
                .update
                .as_ref()
                .is_some_and(|command| !command.requires_terminal)
        }) else {
            self.complete_provider_update(provider, false, cx);
            return;
        };
        let Some(binary) = self.resolve_provider_binary(provider) else {
            self.complete_provider_update(provider, false, cx);
            return;
        };
        let status = self
            .providers
            .provider_versions
            .entry(provider)
            .or_default();
        status.updating = true;
        status.checking = false;
        status.revision += 1;
        let revision = status.revision;

        let settings = self.settings.clone();
        let settings_store = self.settings_store.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let env = host_cx
                .unblock(move || {
                    let profile_id = Settings::builtin_profile_id(provider);
                    let secrets = settings_store.profile_secrets(profile_id);
                    launch_env_for_profile(&settings, profile_id, secrets).pairs(provider)
                })
                .await;
            let current = provider_updates::resolve_installation(provider, &binary, &env).await;
            host_cx.enqueue(move |state, cx| {
                if state
                    .providers
                    .provider_versions
                    .get(&provider)
                    .map(|s| s.revision)
                    != Some(revision)
                    || state.resolve_provider_binary(provider).as_ref() != Some(&binary)
                    || current != installation
                {
                    if let Some(status) = state.providers.provider_versions.get_mut(&provider) {
                        status.updating = false;
                        status.installation = None;
                    }
                    state.complete_provider_update(provider, false, cx);
                    return;
                }
                let command = current.update.expect("validated automatic update plan");
                let host_cx = cx.clone();
                HostCx::spawn_detached(cx, async move {
                    let ok = command.run().await;
                    host_cx.enqueue(move |state, cx| {
                        state.complete_provider_update(provider, ok, cx);
                    });
                });
            });
        });
    }

    /// The installation a session's command menus come from: a native
    /// profile's home, or one ACP agent.
    pub(super) fn commands_cache_key(
        &self,
        provider: ProviderKind,
        profile_id: Option<&str>,
        acp_agent_id: Option<&str>,
    ) -> Option<CommandsCacheKey> {
        let home = match provider {
            ProviderKind::Acp => None,
            ProviderKind::Codex
            | ProviderKind::ClaudeCode
            | ProviderKind::Pi
            | ProviderKind::OpenCode
            | ProviderKind::Cursor
            | ProviderKind::Grok => self
                .settings
                .resolved_profile(
                    profile_id.unwrap_or_else(|| Settings::builtin_profile_id(provider)),
                )
                .and_then(|profile| profile.settings.home_path),
        };
        CommandsCacheKey::new(provider, home, acp_agent_id)
    }

    pub(super) fn cached_provider_commands(
        &self,
        provider: ProviderKind,
        profile_id: Option<&str>,
        acp_agent_id: Option<&str>,
    ) -> Vec<ProviderCommand> {
        self.commands_cache_key(provider, profile_id, acp_agent_id)
            .map(|key| self.store.load_commands(&key))
            .unwrap_or_default()
    }

    pub(super) fn cached_provider_commands_for(&self, meta: &SessionMeta) -> Vec<ProviderCommand> {
        self.cached_provider_commands(
            meta.provider,
            meta.profile_id.as_deref(),
            meta.acp_agent_id.as_deref(),
        )
    }

    pub(super) fn prepare_session_env(
        &self,
        meta: &SessionMeta,
        cx: &HostCx,
    ) -> impl std::future::Future<Output = Result<LaunchEnv, String>> + use<> {
        let native = (meta.provider != ProviderKind::Acp).then(|| {
            self.prepare_profile_env(
                meta.profile_id
                    .as_deref()
                    .unwrap_or_else(|| Settings::builtin_profile_id(meta.provider)),
                cx,
            )
        });
        let acp_env = (meta.provider == ProviderKind::Acp).then(|| {
            meta.acp_agent_id
                .as_deref()
                .and_then(|id| self.settings.acp_agent(id))
                .map(|agent| agent.env.clone())
                .unwrap_or_default()
        });
        async move {
            match native {
                Some(env) => env.await,
                None => Ok(LaunchEnv {
                    env: acp_env.unwrap_or_default(),
                    home: None,
                }),
            }
        }
    }

    // Only credential writes gate a launch; unrelated log batches must not
    // turn an ordinary refresh into a database flush barrier.
    pub(super) fn prepare_profile_env(
        &self,
        profile_id: &str,
        cx: &HostCx,
    ) -> impl std::future::Future<Output = Result<LaunchEnv, String>> + use<> {
        let profile_id = profile_id.to_owned();
        let settings = self.settings.clone();
        let store = self.settings_store.clone();
        let host_cx = cx.clone();
        let sensitive: HashSet<_> = settings
            .resolved_profile(&profile_id)
            .into_iter()
            .flat_map(|profile| profile.settings.env)
            .filter(|var| var.sensitive && !var.name.trim().is_empty())
            .map(|var| var.name)
            .collect();
        let writes: Vec<_> = self
            .providers
            .secret_writes
            .get(&profile_id)
            .into_iter()
            .flat_map(|writes| writes.iter())
            .filter(|(name, _)| sensitive.contains(*name))
            .map(|(_, write)| write.clone())
            .collect();
        async move {
            for write in writes {
                let mut write = write.lock().await;
                if write.result.is_none() {
                    write.result = Some(write.completion.recv().await.unwrap_or_else(|_| {
                        Err(
                            "the session store writer stopped before saving provider credentials"
                                .to_owned(),
                        )
                    }));
                }
                write
                    .result
                    .as_ref()
                    .expect("completed secret write")
                    .clone()?;
            }
            if sensitive.is_empty() {
                return Ok(launch_env_for_profile(
                    &settings,
                    &profile_id,
                    Default::default(),
                ));
            }
            host_cx
                .unblock(move || {
                    let secrets = store
                        .try_profile_secrets(&profile_id)
                        .map_err(|error| error.to_string())?;
                    Ok(launch_env_for_profile(&settings, &profile_id, secrets))
                })
                .await
        }
    }

    /// The cached model catalog for `provider` (empty when never fetched).
    pub(crate) fn models_for(&self, provider: ProviderKind) -> &[ModelSpec] {
        self.providers
            .model_catalogs
            .get(&provider)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn set_sidebar_collapsed(&mut self, collapsed: bool, cx: &mut HostCx) {
        // Persist so the choice survives a restart (save errors are cosmetic).
        self.settings.sidebar_collapsed = collapsed;
        self.persist_settings(cx);
    }
}

/// The reasoning-effort selection value, if any.
pub(super) fn effort_selection(selections: &[OptionSelection]) -> Option<String> {
    selections
        .iter()
        .find(|s| s.id == "reasoningEffort")
        .and_then(|s| s.value.as_str().map(str::to_string))
}

/// Selections sorted by id for order-independent comparison, optionally dropping
/// the reasoning-effort entry (which, for per-turn providers, never forces a
/// restart).
pub(super) fn normalized_selections(
    selections: &[OptionSelection],
    ignore_effort: bool,
) -> Vec<(String, serde_json::Value)> {
    let mut out: Vec<(String, serde_json::Value)> = selections
        .iter()
        .filter(|s| !(ignore_effort && s.id == "reasoningEffort"))
        .map(|s| (s.id.clone(), s.value.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub(super) fn launch_env_for_profile(
    settings: &Settings,
    profile_id: &str,
    secrets: std::collections::BTreeMap<String, String>,
) -> LaunchEnv {
    let Some(profile) = settings.resolved_profile(profile_id) else {
        return LaunchEnv::default();
    };
    let profile_settings = profile.settings;
    let env = profile_settings
        .env
        .iter()
        .filter(|var| !var.name.trim().is_empty())
        .filter_map(|var| {
            let value = if var.sensitive {
                // Sensitive rows keep their value only in secrets.json; a row
                // whose secret was never saved contributes nothing.
                secrets.get(&var.name).cloned()?
            } else {
                var.value.clone()
            };
            Some((var.name.trim().to_string(), value))
        })
        .collect();
    LaunchEnv {
        env,
        home: profile_settings.home_path.clone(),
    }
}

pub(super) fn provider_secret_names(
    settings: &Settings,
    settings_store: &SettingsStore,
) -> HashMap<String, HashSet<String>> {
    ProviderKind::NATIVE
        .into_iter()
        .flat_map(|kind| settings.profiles_for_kind(kind))
        .map(|profile| {
            let id = profile.id;
            let names = launch_env_for_profile(settings, &id, settings_store.profile_secrets(&id))
                .env
                .into_iter()
                .map(|(name, _)| name)
                .collect();
            (id, names)
        })
        .collect()
}

/// Whether tcode's computer-use server is offered to a session of `provider`.
/// A harness with its own computer use keeps it; tcode's is not added beside it.
pub(super) fn computer_use_attaches(provider: ProviderKind, settings: &Settings) -> bool {
    settings.computer_use.enabled && !provider.caps().native_computer_use
}

pub(super) fn session_provider_settings(
    meta: &SessionMeta,
    settings: &Settings,
) -> tcode_core::settings::ProviderSettings {
    meta.profile_id
        .as_deref()
        .and_then(|id| settings.resolved_profile(id))
        .map(|profile| profile.settings)
        .unwrap_or_else(|| settings.provider(meta.provider))
}

/// The in-process MCP servers a session was registered with; each attaches only where the
/// session's provider and settings take it.
#[derive(Default)]
pub(super) struct McpServers {
    pub(super) preview: Option<agent::McpRegistration>,
    pub(super) orchestrate: Option<agent::McpRegistration>,
    pub(super) orchestrate_report: Option<agent::McpRegistration>,
    pub(super) computer_use: Option<agent::McpRegistration>,
    pub(super) control: Option<agent::McpRegistration>,
}

pub(super) fn session_options(
    meta: &SessionMeta,
    settings: &Settings,
    launch_env: LaunchEnv,
    servers: McpServers,
) -> SessionOptions {
    // A session's binary / launch-args come from its selected profile (built-in
    // or user-created), so a third-party profile can point at its own CLI while
    // sharing the protocol adapter. Falls back to the kind's built-in card.
    let provider_settings = session_provider_settings(meta, settings);
    // For an ACP session, which agent to launch (and how) comes from the
    // installed-agent list, keyed by the id the session was created with.
    let acp_agent: Option<InstalledAgent> = meta
        .acp_agent_id
        .as_deref()
        .and_then(|id| settings.acp_agent(id))
        .cloned();
    SessionOptions {
        cwd: meta.cwd.clone(),
        model: meta.model.clone(),
        resume: meta.resume_cursor.clone(),
        fork: meta.pending_fork,
        binary_path: provider_settings.binary_path.clone(),
        option_selections: meta.option_selections.clone(),
        mcp_servers: [
            meta.provider
                .caps()
                .mcp_servers
                .then_some(servers.preview)
                .flatten(),
            meta.orchestrate_enabled
                .then_some(servers.orchestrate)
                .flatten(),
            meta.parent_session_id
                .is_some()
                .then_some(servers.orchestrate_report)
                .flatten(),
            computer_use_attaches(meta.provider, settings)
                .then_some(servers.computer_use)
                .flatten(),
            meta.provider
                .caps()
                .mcp_servers
                .then_some(servers.control)
                .flatten(),
        ]
        .into_iter()
        .flatten()
        .collect(),
        launch_env,
        // Native providers that expose "Launch arguments" use their profile;
        // an ACP agent carries its own from the installed-agent card.
        extra_args: if meta.provider.caps().launch_args {
            match meta.provider {
                ProviderKind::ClaudeCode
                | ProviderKind::OpenCode
                | ProviderKind::Cursor
                | ProviderKind::Grok => provider_settings.extra_args(),
                ProviderKind::Pi => {
                    let mut extra_args = provider_settings.extra_args();
                    if provider_settings.pi.trust_project_extensions {
                        extra_args.push("--approve".into());
                    }
                    extra_args
                }
                ProviderKind::Acp => acp_agent
                    .as_ref()
                    .map(|agent| agent.extra_args())
                    .unwrap_or_default(),
                ProviderKind::Codex => Vec::new(),
            }
        } else {
            Vec::new()
        },
        acp: acp_agent.map(|agent| agent::AcpAgent {
            id: agent.id.clone(),
            name: agent.name.clone(),
            launch: agent.launch.clone(),
        }),
        instructions: None,
    }
}

#[cfg(test)]
mod provider_lifecycle_tests {
    use super::*;

    #[test]
    fn version_round_merges_new_updates_and_finishes_after_stale_completion() {
        let mut catalog = ProviderCatalog::new(HashMap::new(), HashMap::new());
        catalog.version_rounds.insert(
            0,
            VersionCheckRound {
                pending: 3,
                updates: Vec::new(),
            },
        );
        let assessment = provider_updates::check(ProviderCheckInput {
            installed_output: Some("1.0.0"),
            latest_output: Some("2.0.0"),
        });
        assert_eq!(
            catalog.complete_version_check(0, ProviderKind::Codex, 0, None, assessment.clone()),
            None
        );
        assert_eq!(
            catalog.complete_version_check(
                0,
                ProviderKind::ClaudeCode,
                0,
                None,
                assessment.clone()
            ),
            None
        );
        let toast =
            catalog.complete_version_check(0, ProviderKind::Pi, 1, None, assessment.clone());
        assert_eq!(
            toast,
            Some(RuntimeToast::ProviderUpdatesAvailable {
                updates: vec![
                    ProviderUpdateAvailable {
                        provider: ProviderKind::ClaudeCode,
                        version: "2.0.0".into(),
                        automatic: false
                    },
                    ProviderUpdateAvailable {
                        provider: ProviderKind::Codex,
                        version: "2.0.0".into(),
                        automatic: false
                    },
                ]
            })
        );
        catalog.version_rounds.insert(
            1,
            VersionCheckRound {
                pending: 1,
                updates: Vec::new(),
            },
        );
        assert_eq!(
            catalog.complete_version_check(1, ProviderKind::Codex, 0, None, assessment),
            None,
            "unchanged availability must not notify again"
        );
    }

    #[test]
    fn changing_binary_discards_old_update_results_without_finishing_the_new_check() {
        use crate::app::test_support::*;
        let cx = &mut TestAppContext::default();
        let test_store = TestStore::new("tcode-update-installation-race");
        let state = cx.new_entity(TestClientState::new((*test_store).clone()));
        state.update(cx, |state, cx| {
            let provider = ProviderKind::ClaudeCode;
            state.providers.provider_versions.insert(
                provider,
                ProviderVersionState {
                    installed: Some("1.0.0".into()),
                    latest: Some("2.0.0".into()),
                    update_available: true,
                    checking: true,
                    updating: true,
                    ..Default::default()
                },
            );
            let previous = state.providers.provider_versions[&provider].revision;
            let mut settings = state.settings.clone();
            settings.provider_mut(provider).binary_path = Some(PathBuf::from("/new/claude"));
            state.update_settings(settings, cx);
            let status = state
                .providers
                .provider_versions
                .get_mut(&provider)
                .unwrap();
            assert!(!status.update_available);
            assert!(status.installed.is_none());
            assert!(status.installation.is_none());
            assert!(
                status.updating,
                "configuration edits must not allow duplicate updates"
            );
            status.checking = true;

            state.providers.version_rounds.insert(
                0,
                VersionCheckRound {
                    pending: 1,
                    updates: Vec::new(),
                },
            );
            state.providers.version_rounds.insert(
                1,
                VersionCheckRound {
                    pending: 1,
                    updates: Vec::new(),
                },
            );
            let result = provider_updates::check(ProviderCheckInput {
                installed_output: Some("3.0.0"),
                latest_output: Some("3.0.1"),
            });
            assert_eq!(
                state
                    .providers
                    .complete_version_check(0, provider, previous, None, result.clone()),
                None
            );
            let status = &state.providers.provider_versions[&provider];
            assert!(status.checking);
            assert!(!status.update_available);
            assert!(status.latest.is_none());
            let revision = status.revision;
            state
                .providers
                .complete_version_check(1, provider, revision, None, result);
            let status = &state.providers.provider_versions[&provider];
            assert!(!status.checking);
            assert_eq!(status.installed.as_deref(), Some("3.0.0"));
            assert!(status.update_available);

            // Secret values live outside Settings, so changing a manager's
            // environment must independently invalidate the cached plan.
            state.set_profile_secret("claude", "MISE_DATA_DIR", Some("/new/mise"), cx);
            let result = provider_updates::check(ProviderCheckInput {
                installed_output: Some("3.0.0"),
                latest_output: Some("3.0.2"),
            });
            state.providers.version_rounds.insert(
                2,
                VersionCheckRound {
                    pending: 1,
                    updates: Vec::new(),
                },
            );
            assert_eq!(
                state
                    .providers
                    .complete_version_check(2, provider, revision, None, result),
                None
            );
            assert!(!state.providers.provider_versions[&provider].update_available);
            assert!(
                state.providers.provider_versions[&provider]
                    .latest
                    .is_none()
            );
        });
        cx.run_until_parked();
    }

    #[test]
    fn configuration_invalidation_rejects_old_probe_without_finishing_new_probe() {
        let mut catalog = ProviderCatalog::new(HashMap::new(), HashMap::new());
        let error = tcode_core::usage::ProviderUsage {
            error: Some("temporarily unreachable".into()),
            ..Default::default()
        };
        catalog.usage_checking.insert("claude".into());
        catalog.complete_usage("claude".into(), 0, Some(error.clone()));
        assert_eq!(
            catalog.provider_usage["claude"], error,
            "supported failures stay visible"
        );
        // Exercise secret replacement through its production entry point while
        // a response with the previous revision is still outstanding.
        use crate::app::test_support::*;
        let cx = &mut TestAppContext::default();
        let test_store = TestStore::new("tcode-usage-secret-race");
        let state = cx.new_entity(TestClientState::new((*test_store).clone()));
        state.update(cx, |state, cx| {
            std::mem::swap(&mut state.providers, &mut catalog);
            state.set_profile_secret(
                "claude",
                "CLAUDE_CODE_OAUTH_TOKEN",
                Some("fixture-replacement"),
                cx,
            );
            std::mem::swap(&mut state.providers, &mut catalog);
        });
        assert!(!catalog.provider_usage.contains_key("claude"));
        catalog.usage_checking.insert("claude".into());
        catalog.complete_usage("claude".into(), 0, Some(error));
        assert!(catalog.usage_checking.contains("claude"));
        assert!(!catalog.provider_usage.contains_key("claude"));
        let windows = tcode_core::usage::ProviderUsage {
            windows: vec![tcode_core::usage::UsageWindow {
                kind: tcode_core::usage::UsageWindowKind::FiveHour,
                scope: None,
                used_percent: 42.,
                resets_at: Some(1800000000),
            }],
            ..Default::default()
        };
        catalog.complete_usage("claude".into(), 1, Some(windows.clone()));
        assert_eq!(catalog.provider_usage["claude"], windows);
        assert!(!catalog.usage_checking.contains("claude"));
    }

    #[test]
    fn failed_usage_fetch_keeps_the_last_windows_and_their_time() {
        let mut catalog = ProviderCatalog::new(HashMap::new(), HashMap::new());
        let windows = tcode_core::usage::ProviderUsage {
            fetched_at: 1_700_000_000,
            plan: Some("max".into()),
            windows: vec![tcode_core::usage::UsageWindow {
                kind: tcode_core::usage::UsageWindowKind::FiveHour,
                scope: None,
                used_percent: 42.,
                resets_at: Some(1_800_000_000),
            }],
            error: None,
        };
        catalog.complete_usage("claude".into(), 0, Some(windows.clone()));
        catalog.complete_usage(
            "claude".into(),
            0,
            Some(tcode_core::usage::ProviderUsage {
                fetched_at: 1_700_000_500,
                error: Some("temporarily unreachable".into()),
                ..Default::default()
            }),
        );
        assert_eq!(
            catalog.provider_usage["claude"],
            tcode_core::usage::ProviderUsage {
                error: Some("temporarily unreachable".into()),
                ..windows.clone()
            }
        );
        catalog.complete_usage(
            "claude".into(),
            0,
            Some(tcode_core::usage::ProviderUsage {
                fetched_at: 1_700_001_000,
                ..windows.clone()
            }),
        );
        assert_eq!(catalog.provider_usage["claude"].error, None);
        assert_eq!(catalog.provider_usage["claude"].fetched_at, 1_700_001_000);
    }
}
