use super::*;

use agent::{
    MarketplaceAction, PluginAction, PluginActionKind, PluginContext, PluginListing, PluginOp,
    PluginOpOutcome, PluginScope, list_plugins, run_plugin_op,
};
use tcode_protocol::{
    Command, PluginCatalogState, PluginChallenge, PluginChallengeKind, PluginOperationTarget,
    PluginStaleReason, ProviderPluginCatalog,
};

/// One profile's last native plugin listing. Nothing here is persisted: the
/// CLI's own files are the truth and are listed again on demand.
pub(super) struct PluginCatalog {
    context_cwd: Option<PathBuf>,
    launch: PluginLaunch,
    listing: PluginListing,
    state: PluginCatalogState,
    loading: bool,
    /// Bumped when the context or the profile's CLI/home changes; listings
    /// and challenges from an older generation are discarded.
    generation: u64,
    /// Only the newest listing request may land.
    list_request: u64,
    /// The one mutation of this profile in flight or waiting on its user.
    operation: Option<PluginOperation>,
    /// Native facts a finished operation reported, shown on its entry until
    /// the entry is changed again or the context changes.
    notes: HashMap<String, Vec<(String, String)>>,
}

/// The native installation a profile's plugin commands address.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginLaunch {
    provider: ProviderKind,
    binary_path: Option<PathBuf>,
    home: Option<PathBuf>,
}

struct PluginOperation {
    id: RuntimeOperationId,
    subject: String,
    target: PluginOperationTarget,
    op: PluginOp,
    cwd: Option<PathBuf>,
    generation: u64,
    started: bool,
    challenge: Option<PluginChallenge>,
}

fn error(code: &str, message: &str) -> ProtocolError {
    ProtocolError {
        code: code.into(),
        message: message.into(),
    }
}

fn management_disabled() -> ProtocolError {
    error(
        "plugin_management_disabled",
        "Plugin management for this provider is turned off in Settings → Plugins.",
    )
}

fn native_message(error: AgentError) -> String {
    match error {
        AgentError::Provider(message) => message,
        other => other.to_string(),
    }
}

impl AppState {
    fn plugin_launch(&self, profile_id: &str) -> Option<PluginLaunch> {
        let profile = self.settings.resolved_profile(profile_id)?;
        Some(PluginLaunch {
            provider: profile.kind,
            binary_path: profile.settings.binary_path,
            home: profile.settings.home_path,
        })
    }

    pub fn refresh_provider_plugins(
        &mut self,
        profile_id: &str,
        cwd: Option<PathBuf>,
        cx: &mut HostCx,
    ) {
        let Some(launch) = self
            .plugin_launch(profile_id)
            .filter(|launch| self.settings.plugins.provider_enabled(launch.provider))
        else {
            return;
        };
        let catalog = self
            .plugin_catalogs
            .entry(profile_id.to_string())
            .or_insert_with(|| PluginCatalog {
                context_cwd: cwd.clone(),
                launch: launch.clone(),
                listing: PluginListing::default(),
                state: PluginCatalogState::Stale {
                    reason: PluginStaleReason::NotLoaded,
                },
                loading: false,
                generation: 0,
                list_request: 0,
                operation: None,
                notes: HashMap::new(),
            });
        if catalog.context_cwd != cwd || catalog.launch != launch {
            catalog.context_cwd = cwd;
            catalog.launch = launch;
            catalog.generation += 1;
            catalog.notes.clear();
            if matches!(catalog.state, PluginCatalogState::Fresh) {
                catalog.state = PluginCatalogState::Stale {
                    reason: PluginStaleReason::ContextChanged,
                };
            }
            if let Some(operation) = catalog
                .operation
                .take_if(|operation| operation.challenge.is_some())
            {
                Self::abandon_challenge(profile_id, operation, cx);
            }
        }
        self.list_plugins(profile_id, cx);
    }

    /// A challenge that can no longer be answered ends its operation with
    /// the CLI's own refusal, which is what happened natively.
    fn abandon_challenge(profile_id: &str, operation: PluginOperation, cx: &mut HostCx) {
        if !operation.started {
            return;
        }
        let detail = operation
            .challenge
            .and_then(|challenge| challenge.native_text)
            .unwrap_or_default();
        emit_runtime(
            cx,
            RuntimeEvent::Toast(RuntimeToast::PluginOperationFailed {
                operation: operation.id,
                profile_id: profile_id.to_string(),
                target: operation.target,
                detail,
            }),
        );
    }

    /// A provider switched off keeps nothing a client could act on: its
    /// listing is forgotten, a listing in flight is ignored and a pending
    /// challenge is abandoned. A change already running finishes and
    /// reports; a command challenge it then raises is no longer asked.
    pub(super) fn forget_disabled_plugin_catalogs(&mut self, cx: &mut HostCx) {
        let plugins = &self.settings.plugins;
        for (profile_id, catalog) in self
            .plugin_catalogs
            .iter_mut()
            .filter(|(_, catalog)| !plugins.provider_enabled(catalog.launch.provider))
        {
            if let Some(operation) = catalog
                .operation
                .take_if(|operation| operation.challenge.is_some())
            {
                Self::abandon_challenge(profile_id, operation, cx);
            }
            catalog.listing = PluginListing::default();
            catalog.state = PluginCatalogState::Stale {
                reason: PluginStaleReason::NotLoaded,
            };
            catalog.loading = false;
            catalog.list_request += 1;
            catalog.generation += 1;
            catalog.notes.clear();
        }
    }

    fn list_plugins(&mut self, profile_id: &str, cx: &mut HostCx) {
        let settings = &self.settings;
        let Some(catalog) = self
            .plugin_catalogs
            .get_mut(profile_id)
            .filter(|catalog| settings.plugins.provider_enabled(catalog.launch.provider))
        else {
            return;
        };
        catalog.list_request += 1;
        catalog.loading = true;
        let request = catalog.list_request;
        let provider = catalog.launch.provider;
        let cwd = catalog.context_cwd.clone();
        let context = self.plugin_context(profile_id, cwd, cx);
        let profile_id = profile_id.to_string();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = match context.await {
                Ok(context) => list_plugins(provider, &context).await,
                Err(error) => Err(error),
            };
            host_cx.enqueue(move |state, _cx| {
                let Some(catalog) = state.plugin_catalogs.get_mut(&profile_id) else {
                    return;
                };
                if catalog.list_request != request {
                    return;
                }
                catalog.loading = false;
                match result {
                    Ok(listing) => {
                        catalog.listing = listing;
                        catalog.state = PluginCatalogState::Fresh;
                    }
                    Err(error) => {
                        catalog.state = PluginCatalogState::Error {
                            message: native_message(error),
                        }
                    }
                }
            });
        });
    }

    /// Check a plugin command against the catalog it was offered from, so a
    /// client acting on an old snapshot cannot run what the host no longer
    /// offers.
    pub(super) fn validate_plugin_command(&self, command: &Command) -> Result<(), ProtocolError> {
        let (profile_id, cwd) = match command {
            Command::RefreshProviderPlugins { profile_id, .. } => (profile_id, None),
            Command::InstallProviderPlugin {
                profile_id, cwd, ..
            }
            | Command::UninstallProviderPlugin {
                profile_id, cwd, ..
            }
            | Command::SetProviderPluginEnabled {
                profile_id, cwd, ..
            }
            | Command::UpdateProviderPlugin {
                profile_id, cwd, ..
            }
            | Command::AddProviderMarketplace {
                profile_id, cwd, ..
            }
            | Command::RemoveProviderMarketplace {
                profile_id, cwd, ..
            } => (profile_id, Some(cwd)),
            Command::ResolvePluginChallenge { op_id, .. } => {
                let Some(catalog) = self.plugin_catalogs.values().find(|catalog| {
                    catalog
                        .operation
                        .as_ref()
                        .and_then(|operation| operation.challenge.as_ref())
                        .is_some_and(|challenge| challenge.op_id == *op_id)
                }) else {
                    return Err(error(
                        "unknown_plugin_challenge",
                        "This confirmation is no longer pending.",
                    ));
                };
                return if self
                    .settings
                    .plugins
                    .provider_enabled(catalog.launch.provider)
                {
                    Ok(())
                } else {
                    Err(management_disabled())
                };
            }
            _ => return Ok(()),
        };
        let Some(launch) = self.plugin_launch(profile_id) else {
            return Err(error(
                "unknown_profile",
                "This provider profile is no longer available.",
            ));
        };
        let management = launch.provider.caps().plugin_management;
        if management.actions.is_empty() && !management.marketplaces {
            return Err(error(
                "plugin_management_unsupported",
                "This provider's plugins cannot be managed from Tcode.",
            ));
        }
        // Listing a provider that is switched off does nothing.
        let Some(cwd) = cwd else {
            return Ok(());
        };
        if !self.settings.plugins.provider_enabled(launch.provider) {
            return Err(management_disabled());
        }
        let catalog = self
            .plugin_catalogs
            .get(profile_id)
            .filter(|catalog| catalog.context_cwd == *cwd && catalog.launch == launch)
            .ok_or_else(|| {
                error(
                    "plugin_catalog_stale",
                    "List this provider's plugins again before changing them.",
                )
            })?;
        if catalog.operation.is_some() {
            return Err(error(
                "plugin_operation_in_flight",
                "Another plugin change for this provider is still running.",
            ));
        }
        let offered = |entry_id: &str, action: PluginAction| {
            management.supports(action.kind())
                && catalog
                    .listing
                    .entries
                    .iter()
                    .any(|entry| entry.id == entry_id && entry.actions.contains(&action))
        };
        let available = match command {
            Command::InstallProviderPlugin {
                entry_id, scope, ..
            } => offered(entry_id, PluginAction::Install { scope: *scope }),
            Command::UninstallProviderPlugin {
                entry_id, scope, ..
            } => offered(entry_id, PluginAction::Uninstall { scope: *scope }),
            Command::SetProviderPluginEnabled {
                entry_id,
                scope,
                enabled: true,
                ..
            } => offered(entry_id, PluginAction::Enable { scope: *scope }),
            Command::SetProviderPluginEnabled {
                entry_id,
                scope,
                enabled: false,
                ..
            } => offered(entry_id, PluginAction::Disable { scope: *scope }),
            Command::UpdateProviderPlugin {
                entry_id, scope, ..
            } => offered(entry_id, PluginAction::Update { scope: *scope }),
            Command::AddProviderMarketplace { source, .. } => {
                management.marketplaces
                    && !source.trim().is_empty()
                    && catalog
                        .listing
                        .marketplace_actions
                        .contains(&MarketplaceAction::Add)
            }
            Command::RemoveProviderMarketplace { name, .. } => {
                management.marketplaces && removal(&catalog.listing, name).is_some()
            }
            _ => true,
        };
        if available {
            Ok(())
        } else {
            Err(error(
                "plugin_action_unavailable",
                "This plugin action is not available here.",
            ))
        }
    }

    pub fn start_plugin_operation(
        &mut self,
        profile_id: &str,
        cwd: Option<PathBuf>,
        op: PluginOp,
        cx: &mut HostCx,
    ) {
        let (subject, target) = match &op {
            PluginOp::Install { id, scope, .. } => {
                plugin_target(id, *scope, PluginActionKind::Install)
            }
            PluginOp::Uninstall { id, scope } => {
                plugin_target(id, *scope, PluginActionKind::Uninstall)
            }
            PluginOp::SetEnabled { id, scope, enabled } => plugin_target(
                id,
                *scope,
                if *enabled {
                    PluginActionKind::Enable
                } else {
                    PluginActionKind::Disable
                },
            ),
            PluginOp::Update { id, scope, .. } => {
                plugin_target(id, *scope, PluginActionKind::Update)
            }
            PluginOp::AddMarketplace { source } => (
                source.clone(),
                PluginOperationTarget::AddMarketplace {
                    source: source.clone(),
                },
            ),
            PluginOp::RemoveMarketplace { name } => (
                name.clone(),
                PluginOperationTarget::RemoveMarketplace { name: name.clone() },
            ),
        };
        let id = self.next_operation_id();
        // Native removal has no confirmation of its own, so ask first.
        let challenge = match &op {
            PluginOp::RemoveMarketplace { name } => {
                let uninstalls = self
                    .plugin_catalogs
                    .get(profile_id)
                    .and_then(|catalog| removal(&catalog.listing, name))
                    .unwrap_or_default();
                Some(PluginChallenge {
                    op_id: self.next_operation_id(),
                    profile_id: profile_id.to_string(),
                    entry_id: name.clone(),
                    kind: PluginChallengeKind::ConfirmDestructive { uninstalls },
                    native_text: None,
                })
            }
            _ => None,
        };
        let Some(catalog) = self.plugin_catalogs.get_mut(profile_id) else {
            return;
        };
        let wait = challenge.is_some();
        catalog.operation = Some(PluginOperation {
            id,
            subject,
            target,
            op,
            cwd,
            generation: catalog.generation,
            started: false,
            challenge,
        });
        if !wait {
            self.execute_plugin_operation(profile_id, cx);
        }
    }

    fn execute_plugin_operation(&mut self, profile_id: &str, cx: &mut HostCx) {
        let Some(catalog) = self.plugin_catalogs.get_mut(profile_id) else {
            return;
        };
        let provider = catalog.launch.provider;
        let Some(operation) = catalog.operation.as_mut() else {
            return;
        };
        if !operation.started {
            operation.started = true;
            emit_runtime(
                cx,
                RuntimeEvent::Toast(RuntimeToast::PluginOperationStarted {
                    operation: operation.id,
                    profile_id: profile_id.to_string(),
                    target: operation.target.clone(),
                }),
            );
        }
        let id = operation.id;
        let op = operation.op.clone();
        let cwd = operation.cwd.clone();
        let context = self.plugin_context(profile_id, cwd, cx);
        let profile_id = profile_id.to_string();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let result = match context.await {
                Ok(context) => run_plugin_op(provider, &context, &op).await,
                Err(error) => Err(error),
            };
            host_cx.enqueue(move |state, cx| {
                state.finish_plugin_operation(&profile_id, id, result, cx);
            });
        });
    }

    fn finish_plugin_operation(
        &mut self,
        profile_id: &str,
        id: RuntimeOperationId,
        result: Result<PluginOpOutcome, AgentError>,
        cx: &mut HostCx,
    ) {
        let challenge_id = self.next_operation_id();
        let Some(catalog) = self.plugin_catalogs.get_mut(profile_id) else {
            return;
        };
        let Some(mut operation) = catalog
            .operation
            .take_if(|operation| operation.id == id && operation.challenge.is_none())
        else {
            return;
        };
        let target = operation.target.clone();
        let toast = match result {
            Ok(PluginOpOutcome::AcceptCommand(acceptance))
                if operation.generation == catalog.generation =>
            {
                operation.challenge = Some(PluginChallenge {
                    op_id: challenge_id,
                    profile_id: profile_id.to_string(),
                    entry_id: operation.subject.clone(),
                    kind: PluginChallengeKind::AcceptCommand {
                        command: acceptance.command,
                        sha256: acceptance.sha256,
                        mode: acceptance.mode,
                    },
                    native_text: Some(acceptance.native_text),
                });
                catalog.operation = Some(operation);
                return;
            }
            Ok(PluginOpOutcome::AcceptCommand(acceptance)) => RuntimeToast::PluginOperationFailed {
                operation: id,
                profile_id: profile_id.to_string(),
                target,
                detail: acceptance.native_text,
            },
            Ok(PluginOpOutcome::Done { diagnostics }) => {
                if diagnostics.is_empty() {
                    catalog.notes.remove(&operation.subject);
                } else {
                    catalog.notes.insert(operation.subject.clone(), diagnostics);
                }
                RuntimeToast::PluginOperationSucceeded {
                    operation: id,
                    profile_id: profile_id.to_string(),
                    target,
                }
            }
            Err(error) => {
                catalog.notes.remove(&operation.subject);
                RuntimeToast::PluginOperationFailed {
                    operation: id,
                    profile_id: profile_id.to_string(),
                    target,
                    detail: native_message(error),
                }
            }
        };
        emit_runtime(cx, RuntimeEvent::Toast(toast));
        self.after_plugin_change(profile_id, cx);
    }

    /// A finished native change: menus cached for the same home no longer
    /// describe it, and every catalog listing that home is out of date.
    fn after_plugin_change(&mut self, profile_id: &str, cx: &mut HostCx) {
        let Some(launch) = self
            .plugin_catalogs
            .get(profile_id)
            .map(|catalog| catalog.launch.clone())
        else {
            return;
        };
        self.enqueue_store_write(
            StoreWrite::InvalidateCommands(CommandsCacheKey::Native {
                provider: launch.provider,
                home: launch.home.clone(),
            }),
            cx,
        );
        let affected: Vec<String> = self
            .plugin_catalogs
            .iter_mut()
            .filter(|(_, catalog)| {
                catalog.launch.provider == launch.provider && catalog.launch.home == launch.home
            })
            .map(|(id, catalog)| {
                if !matches!(catalog.state, PluginCatalogState::Error { .. }) {
                    catalog.state = PluginCatalogState::Stale {
                        reason: PluginStaleReason::Changed,
                    };
                }
                id.clone()
            })
            .collect();
        for id in affected {
            self.list_plugins(&id, cx);
        }
    }

    /// Answer a pending challenge. Acceptance applies only to the command or
    /// removal this challenge showed; the hash never comes from the client.
    pub fn resolve_plugin_challenge(
        &mut self,
        op_id: RuntimeOperationId,
        accept: bool,
        cx: &mut HostCx,
    ) -> Result<(), ProtocolError> {
        let unknown = || {
            error(
                "unknown_plugin_challenge",
                "This confirmation is no longer pending.",
            )
        };
        let profile_id = self
            .plugin_catalogs
            .iter()
            .find(|(_, catalog)| {
                catalog
                    .operation
                    .as_ref()
                    .and_then(|operation| operation.challenge.as_ref())
                    .is_some_and(|challenge| challenge.op_id == op_id)
            })
            .map(|(id, _)| id.clone())
            .ok_or_else(unknown)?;
        let current = self.plugin_launch(&profile_id);
        let next_challenge_id = self.next_operation_id();
        let catalog = self
            .plugin_catalogs
            .get_mut(&profile_id)
            .ok_or_else(unknown)?;
        let mut operation = catalog.operation.take().ok_or_else(unknown)?;
        if operation.generation != catalog.generation || current.as_ref() != Some(&catalog.launch) {
            Self::abandon_challenge(&profile_id, operation, cx);
            return Err(unknown());
        }
        if !accept {
            Self::abandon_challenge(&profile_id, operation, cx);
            return Ok(());
        }
        let challenge = operation.challenge.take().ok_or_else(unknown)?;
        match challenge.kind {
            PluginChallengeKind::AcceptCommand { sha256, .. } => match &mut operation.op {
                PluginOp::Install { accept_command, .. }
                | PluginOp::Update { accept_command, .. } => *accept_command = Some(sha256),
                _ => return Err(unknown()),
            },
            PluginChallengeKind::ConfirmDestructive { uninstalls } => {
                let current = removal(&catalog.listing, &operation.subject).unwrap_or_default();
                if current != uninstalls {
                    operation.challenge = Some(PluginChallenge {
                        op_id: next_challenge_id,
                        kind: PluginChallengeKind::ConfirmDestructive {
                            uninstalls: current,
                        },
                        ..challenge
                    });
                    catalog.operation = Some(operation);
                    return Err(error(
                        "plugin_challenge_changed",
                        "What this removes has changed; confirm it again.",
                    ));
                }
            }
        }
        catalog.operation = Some(operation);
        self.execute_plugin_operation(&profile_id, cx);
        Ok(())
    }

    pub(crate) fn provider_plugin_catalogs(&self) -> Vec<ProviderPluginCatalog> {
        let mut catalogs: Vec<ProviderPluginCatalog> = self
            .plugin_catalogs
            .iter()
            .filter(|(profile_id, _)| {
                self.settings
                    .resolved_profile(profile_id)
                    .is_some_and(|profile| self.settings.plugins.provider_enabled(profile.kind))
            })
            .map(|(profile_id, catalog)| ProviderPluginCatalog {
                profile_id: profile_id.clone(),
                context_cwd: catalog.context_cwd.clone(),
                marketplaces: catalog.listing.marketplaces.clone(),
                marketplace_actions: catalog.listing.marketplace_actions.clone(),
                errors: catalog.listing.errors.clone(),
                entries: catalog
                    .listing
                    .entries
                    .iter()
                    .map(|entry| {
                        let mut entry = entry.clone();
                        if let Some(notes) = catalog.notes.get(&entry.id) {
                            entry.diagnostics.extend(notes.iter().cloned());
                        }
                        entry
                    })
                    .collect(),
                state: if self.plugin_launch(profile_id).as_ref() == Some(&catalog.launch) {
                    catalog.state.clone()
                } else {
                    PluginCatalogState::Stale {
                        reason: PluginStaleReason::ContextChanged,
                    }
                },
                loading: catalog.loading,
                pending: catalog
                    .operation
                    .iter()
                    .map(|operation| operation.subject.clone())
                    .collect(),
                challenges: catalog
                    .operation
                    .iter()
                    .filter_map(|operation| operation.challenge.clone())
                    .collect(),
            })
            .collect();
        catalogs.sort_by(|a, b| a.profile_id.cmp(&b.profile_id));
        catalogs
    }
}

fn plugin_target(
    id: &str,
    scope: PluginScope,
    action: PluginActionKind,
) -> (String, PluginOperationTarget) {
    (
        id.to_string(),
        PluginOperationTarget::Plugin {
            action,
            scope,
            plugin: id.to_string(),
        },
    )
}

/// What removing `name` uninstalls, when the listing offers its removal.
fn removal(listing: &PluginListing, name: &str) -> Option<Vec<String>> {
    listing
        .marketplace_actions
        .iter()
        .find_map(|action| match action {
            MarketplaceAction::Remove {
                marketplace,
                uninstalls,
            } if marketplace == name => Some(uninstalls.clone()),
            _ => None,
        })
}

impl AppState {
    /// Resolve the native plugin installation without session arguments or MCP servers.
    /// Without a project, run from the provider home instead of a project directory.
    fn plugin_context(
        &self,
        profile_id: &str,
        cwd: Option<PathBuf>,
        cx: &HostCx,
    ) -> impl std::future::Future<Output = Result<PluginContext, AgentError>> + use<> {
        let binary_path = self
            .settings
            .resolved_profile(profile_id)
            .and_then(|profile| profile.settings.binary_path);
        let launch_env = self.prepare_profile_env(profile_id, cx);
        async move {
            let launch_env = launch_env.await.map_err(AgentError::Provider)?;
            let project = cwd.is_some();
            let cwd = cwd
                .or_else(|| launch_env.home.clone())
                .or_else(std::env::home_dir)
                .unwrap_or_else(std::env::temp_dir);
            Ok(PluginContext {
                binary_path,
                launch_env,
                cwd,
                project,
            })
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::app::test_support::*;
    use tcode_protocol::{CommandResponse, HostMessage, SettingsPatch};

    const GAMMA_SHA: &str = "b9c02c85b17261fa1fce010664bb16cbe866f14c7deeaf6cb6c37c2736bbe269";

    /// A `claude` that answers the recorded probe outputs and logs each
    /// invocation as `<cwd> <argv…>`.
    struct FakeClaude {
        dir: PathBuf,
        binary: PathBuf,
        log: PathBuf,
        home: PathBuf,
        project: PathBuf,
    }

    impl FakeClaude {
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let dir =
                std::env::temp_dir().join(format!("tcode-fake-claude-{}", uuid::Uuid::new_v4()));
            let home = dir.join("home");
            let project = dir.join("project");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&project).unwrap();
            let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../agent/tests/fixtures/plugins/claude");
            let log = dir.join("argv.log");
            let binary = dir.join("claude");
            fs::write(
                &binary,
                format!(
                    "#!/bin/sh\n\
                     F='{fixtures}'\n\
                     printf '%s %s\\n' \"$PWD\" \"$*\" >> '{log}'\n\
                     case \"$*\" in\n\
                     'plugin list --json --available') cat \"$F/plugin-list-available-after-install.json\" ;;\n\
                     'plugin marketplace list --json') cat \"$F/marketplace-list.json\" ;;\n\
                     'plugin details '*) cat \"$F/details.txt\" ;;\n\
                     *'--accept-command {GAMMA_SHA}') cat \"$F/install-command-source-accepted.json\" ;;\n\
                     'plugin install gamma@tcode-probe '*) cat \"$F/install-command-source-challenge.txt\"; exit 1 ;;\n\
                     'plugin disable alpha@tcode-probe --scope user --json') cat \"$F/disable-scope-user.json\" ;;\n\
                     *) echo \"unexpected: $*\" >&2; exit 2 ;;\n\
                     esac\n",
                    fixtures = fixtures.display(),
                    log = log.display(),
                ),
            )
            .unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                dir,
                binary,
                log,
                home,
                project,
            }
        }

        fn calls(&self, verb: &str) -> Vec<String> {
            fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains(verb))
                .map(str::to_string)
                .collect()
        }
    }

    impl Drop for FakeClaude {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// A host whose Claude profile runs `fake`, with the default switches.
    fn unmanaged_host(fake: &FakeClaude, store: &TestStore) -> TestClientState {
        let mut state = TestClientState::new((**store).clone());
        let claude = state.settings.provider_mut(ProviderKind::ClaudeCode);
        claude.binary_path = Some(fake.binary.clone());
        claude.home_path = Some(fake.home.clone());
        state
    }

    fn host_with(fake: &FakeClaude, store: &TestStore) -> TestClientState {
        let mut state = unmanaged_host(fake, store);
        state.settings.apply(claude_switch(true)).unwrap();
        state
    }

    fn claude_switch(enabled: bool) -> SettingsPatch {
        SettingsPatch::PluginManagementProvider {
            provider: ProviderKind::ClaudeCode,
            enabled,
        }
    }

    fn catalog(state: &TestClientState) -> Option<ProviderPluginCatalog> {
        state
            .provider_plugin_catalogs()
            .into_iter()
            .find(|catalog| catalog.profile_id == "claude")
    }

    fn settled(state: &TestClientState) -> bool {
        catalog(state).is_some_and(|catalog| {
            !catalog.loading
                && catalog.pending.is_empty()
                && matches!(catalog.state, PluginCatalogState::Fresh)
        })
    }

    fn challenged(state: &TestClientState) -> bool {
        catalog(state).is_some_and(|catalog| !catalog.challenges.is_empty())
    }

    fn ack(cx: &mut TestAppContext, id: u64) -> Result<CommandResponse, ProtocolError> {
        cx.drain_outgoing()
            .into_iter()
            .find_map(|message| match message {
                HostMessage::Ack { id: acked, result } if acked == id => Some(result),
                _ => None,
            })
            .expect("command acknowledged")
    }

    #[test]
    fn accepting_a_command_challenge_reruns_with_its_hash_and_a_stale_one_is_refused() {
        let fake = FakeClaude::new();
        let store = TestStore::new("tcode-plugin-challenge");
        let cx = &mut TestAppContext::default();
        let state = cx.new_entity(host_with(&fake, &store));
        let project = Some(fake.project.clone());
        let refresh = |cwd: Option<PathBuf>| Command::RefreshProviderPlugins {
            profile_id: "claude".into(),
            cwd,
        };
        let install = Command::InstallProviderPlugin {
            profile_id: "claude".into(),
            entry_id: "gamma@tcode-probe".into(),
            scope: PluginScope::User,
            cwd: project.clone(),
        };

        state.dispatch_command(cx, 1, refresh(project.clone()));
        cx.run_until(settled);
        state.dispatch_command(cx, 2, install.clone());
        cx.run_until(challenged);
        let challenge = state.read(|state| catalog(state).unwrap().challenges.remove(0));
        assert_eq!(
            challenge.kind,
            PluginChallengeKind::AcceptCommand {
                command: "echo /tmp/tcode-probe/gamma-src".into(),
                sha256: GAMMA_SHA.into(),
                mode: Some("copy".into()),
            }
        );

        // Listing from another directory abandons the challenge it showed.
        state.dispatch_command(cx, 3, refresh(None));
        cx.run_until(settled);
        state.dispatch_command(
            cx,
            4,
            Command::ResolvePluginChallenge {
                op_id: challenge.op_id,
                accept: true,
            },
        );
        cx.run_until_parked();
        assert_eq!(ack(cx, 4).unwrap_err().code, "unknown_plugin_challenge");
        assert_eq!(fake.calls("plugin install").len(), 1);

        state.dispatch_command(cx, 5, refresh(project.clone()));
        cx.run_until(settled);
        state.dispatch_command(cx, 6, install);
        cx.run_until(challenged);
        let challenge = state.read(|state| catalog(state).unwrap().challenges.remove(0));
        state.dispatch_command(
            cx,
            7,
            Command::ResolvePluginChallenge {
                op_id: challenge.op_id,
                accept: true,
            },
        );
        cx.run_until(|state| settled(state) && fake.calls("--accept-command").len() == 1);
        assert_eq!(ack(cx, 7).unwrap(), CommandResponse::Unit);
        let installs = fake.calls("plugin install");
        let accepted = installs.last().unwrap();
        assert!(
            accepted.ends_with(&format!(
                "plugin install gamma@tcode-probe --scope user --json --accept-command {GAMMA_SHA}"
            )),
            "{installs:?}"
        );
        let project = fs::canonicalize(&fake.project).unwrap();
        assert!(
            accepted.starts_with(&format!("{} ", project.display())),
            "{accepted}"
        );
    }

    /// Off, Tcode never runs the provider's CLI and refuses its plugin
    /// commands; switching it off abandons what was waiting on the person.
    #[test]
    fn a_switched_off_provider_is_never_run_and_its_commands_are_refused() {
        let fake = FakeClaude::new();
        let store = TestStore::new("tcode-plugin-switch");
        let cx = &mut TestAppContext::default();
        let state = cx.new_entity(unmanaged_host(&fake, &store));
        let project = Some(fake.project.clone());
        let refresh = Command::RefreshProviderPlugins {
            profile_id: "claude".into(),
            cwd: project.clone(),
        };
        let install = Command::InstallProviderPlugin {
            profile_id: "claude".into(),
            entry_id: "gamma@tcode-probe".into(),
            scope: PluginScope::User,
            cwd: project.clone(),
        };

        state.dispatch_command(cx, 1, refresh.clone());
        cx.run_until_parked();
        assert_eq!(ack(cx, 1).unwrap(), CommandResponse::Unit);
        state.dispatch_command(cx, 2, install.clone());
        cx.run_until_parked();
        assert_eq!(ack(cx, 2).unwrap_err().code, "plugin_management_disabled");
        assert!(state.read(|state| catalog(state).is_none()));
        assert_eq!(fake.calls(""), Vec::<String>::new());

        state.dispatch_command(
            cx,
            3,
            Command::PatchSettings {
                patch: claude_switch(true),
            },
        );
        state.dispatch_command(cx, 4, refresh);
        cx.run_until(settled);
        assert!(!fake.calls("plugin list").is_empty());
        state.dispatch_command(cx, 5, install);
        cx.run_until(challenged);
        let challenge = state.read(|state| catalog(state).unwrap().challenges.remove(0));

        state.dispatch_command(
            cx,
            6,
            Command::PatchSettings {
                patch: claude_switch(false),
            },
        );
        cx.run_until_parked();
        assert!(state.read(|state| catalog(state).is_none()));
        let calls = fake.calls("");
        state.dispatch_command(
            cx,
            7,
            Command::ResolvePluginChallenge {
                op_id: challenge.op_id,
                accept: true,
            },
        );
        cx.run_until_parked();
        assert_eq!(ack(cx, 7).unwrap_err().code, "unknown_plugin_challenge");
        assert_eq!(fake.calls(""), calls);
    }

    #[test]
    fn a_plugin_change_invalidates_only_the_commands_cached_for_its_home() {
        let fake = FakeClaude::new();
        let store = TestStore::new("tcode-plugin-cache");
        let commands = vec![ProviderCommand {
            name: "alpha:greet".into(),
            description: None,
            kind: agent::ProviderCommandKind::Skill,
        }];
        let key = |home: Option<PathBuf>| CommandsCacheKey::Native {
            provider: ProviderKind::ClaudeCode,
            home,
        };
        let other_home = key(Some(fake.dir.join("other-home")));
        for home in [key(Some(fake.home.clone())), key(None), other_home.clone()] {
            store.save_commands(&home, &commands).unwrap();
        }
        let cx = &mut TestAppContext::default();
        let state = cx.new_entity(host_with(&fake, &store));
        assert_eq!(
            state.read(|state| state.cached_provider_commands(
                ProviderKind::ClaudeCode,
                None,
                None
            )),
            commands
        );

        state.dispatch_command(
            cx,
            1,
            Command::RefreshProviderPlugins {
                profile_id: "claude".into(),
                cwd: None,
            },
        );
        cx.run_until(settled);
        state.dispatch_command(
            cx,
            2,
            Command::SetProviderPluginEnabled {
                profile_id: "claude".into(),
                entry_id: "alpha@tcode-probe".into(),
                scope: PluginScope::User,
                enabled: false,
                cwd: None,
            },
        );
        cx.run_until(|state| settled(state) && fake.calls("plugin disable").len() == 1);

        assert!(
            state
                .read(|state| state.cached_provider_commands(ProviderKind::ClaudeCode, None, None))
                .is_empty()
        );
        assert_eq!(store.load_commands(&key(None)), commands);
        assert_eq!(store.load_commands(&other_home), commands);
    }
}
