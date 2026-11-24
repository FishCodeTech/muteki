//! Persisted application settings domain data.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use agent::{ModelSpec, OptionDescriptor, ProviderKind};
use serde::{Deserialize, Serialize};

use crate::acp::InstalledAcpAgent;
pub use crate::provider_colors::{PROVIDER_COLOR_PALETTE, builtin_provider_color};
use crate::pull_request::HostKind;
use orchestrate_fleet::Role;
use orchestrate_legacy::LegacyOrchestrateModel;

mod orchestrate_fleet;
mod orchestrate_legacy;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceControlSettings {
    #[serde(default)]
    pub hosts: BTreeMap<String, HostSettings>,
    /// Host-authored discovery; never persisted in settings.json.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub status: BTreeMap<String, HostStatus>,
}

impl SourceControlSettings {
    /// The kind a host is read as: the one settings give it, configured or found.
    pub fn kind(&self, host: &str) -> HostKind {
        HostKind::of(
            self.hosts
                .get(host)
                .map(|choice| choice.kind)
                .or_else(|| self.status.get(host).map(|status| status.kind)),
            host,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSettings {
    pub kind: HostKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The chosen account when the host's CLI has several.
    #[serde(default)]
    pub account: Option<String>,
}

impl HostSettings {
    pub fn new(kind: HostKind) -> Self {
        Self {
            kind,
            enabled: true,
            account: None,
        }
    }
}

/// `github.hosts` as settings.json held it before hosts had kinds; read once and moved into
/// `source_control.hosts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyGitHubSettings {
    #[serde(default)]
    pub hosts: BTreeMap<String, LegacyGitHubHost>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyGitHubHost {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub account: Option<String>,
}

/// Where the token Tcode uses for a host comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialSource {
    Saved,
    /// A saved token sent with the account's email, as the host's API tokens are.
    SavedBasic {
        email: String,
    },
    /// A saved token sent alone, as the host's access tokens are.
    SavedBearer,
    /// The environment variable's name.
    Env {
        name: String,
    },
    /// The CLI whose stored login supplies it.
    Cli {
        tool: String,
    },
}

/// What keeps a host from being read, when something does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostProblem {
    /// Nothing resolved: no token saved or in the environment, and these CLIs were not found.
    NoCredential { tools_missing: Vec<String> },
    /// The CLI exists and has no login for the host; `command` signs it in.
    NotSignedIn {
        tool: String,
        command: Option<String>,
    },
    /// The token in use is a saved one the host has not yet answered a request with, so
    /// nothing has shown that it works.
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    pub kind: HostKind,
    /// Only the user's settings name it: no CLI login, environment variable or default does.
    pub added: bool,
    pub token_set: bool,
    /// The source in use now; `None` when nothing resolved.
    pub source: Option<CredentialSource>,
    /// The CLI's accounts for the host, for choosing one.
    pub accounts: Vec<String>,
    pub env_overrides_account: bool,
    /// The host's resolution order: the first that works is used.
    pub order: Vec<CredentialSource>,
    pub problem: Option<HostProblem>,
    /// Where the user creates the token the host takes. A host whose token may instead be an
    /// access token sent alone explains those at `access_token_url`: they are created on the
    /// item they are scoped to, not on one page.
    pub token_url: Option<String>,
    pub access_token_url: Option<String>,
    /// The scopes a token needs, as the host's token page names them.
    pub token_scopes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    Dark,
    #[default]
    System,
}

/// How the sidebar's PROJECTS groups are ordered. Cycled by the sort button
/// next to the "PROJECTS" header and persisted in settings.json.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSort {
    /// Newest session activity first (default; the original behavior).
    #[default]
    RecentActivity,
    /// Project name, case-insensitive A-Z.
    NameAsc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarLayout {
    /// One flat list of threads sorted by attention then recency (default).
    #[default]
    Flat,
    /// Threads grouped under project headers (the legacy layout).
    Grouped,
}

impl ProjectSort {
    /// The next mode in the cycle (RecentActivity → NameAsc → RecentActivity).
    pub fn next(self) -> Self {
        match self {
            ProjectSort::RecentActivity => ProjectSort::NameAsc,
            ProjectSort::NameAsc => ProjectSort::RecentActivity,
        }
    }
}

/// The stable settings-file key for a provider: its entry in `settings.json`'s
/// `providers` map. Profile-keyed data such as `secrets.json` uses
/// [`Settings::builtin_profile_id`].
pub fn provider_key(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "codex",
        ProviderKind::ClaudeCode => "claude",
        ProviderKind::Pi => "pi",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Cursor => "cursor",
        ProviderKind::Grok => "grok",
        // ACP agents are not one provider but many: their per-agent settings
        // live in `Settings::acp_agents`, keyed by registry id. This bucket only
        // ever holds the shared fallbacks (it is never written by the ACP card).
        ProviderKind::Acp => "acp",
    }
}

/// The provider color key of an ACP agent: agents share `ProviderKind::Acp`,
/// so the registry id is what tells them apart.
pub fn acp_color_key(agent_id: &str) -> String {
    format!("acp:{agent_id}")
}

/// The provider's short display name used for card titles and picker labels.
pub fn provider_label(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Codex => "Codex",
        ProviderKind::ClaudeCode => "Claude",
        ProviderKind::Pi => "pi",
        ProviderKind::OpenCode => "OpenCode",
        ProviderKind::Cursor => "Cursor",
        ProviderKind::Grok => "Grok",
        ProviderKind::Acp => "ACP",
    }
}

/// One `KEY=VALUE` pair passed into a provider's child processes.
///
/// Sensitive rows never store their value here: it lives in `secrets.json`
/// (0600) and is never handed back to the UI, which renders the "Stored secret"
/// placeholder instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    /// Plaintext value for non-sensitive rows; always empty when `sensitive`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value: String,
    #[serde(default)]
    pub sensitive: bool,
}

/// Per-provider configuration (Settings → Providers card).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSettings {
    /// Whether the provider may be used for new sessions.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional label shown in the provider list (falls back to the driver name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// `#rrggbb` accent tinting the provider glyph in picker rails / model lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
    /// Environment variables merged into every child process for this provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    /// Override for the CLI binary (`None` = resolve from PATH).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<PathBuf>,
    /// The provider's home variable ([`agent::LaunchEnv::home`]). OpenCode
    /// ignores this field because it has no single-home override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_path: Option<PathBuf>,
    /// Native-provider CLI arguments appended on session start (ignored for Codex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_args: Option<String>,
    #[serde(flatten)]
    pub pi: PiProviderSettings,
    /// Model slugs added by hand in the Models section.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_models: Vec<String>,
    /// Model ids hidden from the composer's model picker.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_models: Vec<String>,
}

/// Serializable edits the UI can make to one provider profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileSettingsPatch {
    SetEnabled { enabled: bool },
    ReplaceConfiguration(Box<ProfileConfigurationPatch>),
}

/// The provider-dialog configuration payload of
/// [`ProfileSettingsPatch::ReplaceConfiguration`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileConfigurationPatch {
    pub display_name: Option<String>,
    pub accent_color: Option<String>,
    pub env: Vec<EnvVar>,
    pub binary_path: Option<PathBuf>,
    pub home_path: Option<PathBuf>,
    pub launch_args: Option<String>,
    #[serde(flatten)]
    pub pi: PiProviderSettings,
    pub custom_models: Vec<String>,
    pub hidden_models: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiProviderSettings {
    /// Whether pi should trust and load the project's local `.pi` configuration.
    #[serde(default, rename = "pi_trust_project_extensions")]
    pub trust_project_extensions: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            display_name: None,
            accent_color: None,
            env: Vec::new(),
            binary_path: None,
            home_path: None,
            launch_args: None,
            pi: PiProviderSettings::default(),
            custom_models: Vec::new(),
            hidden_models: Vec::new(),
        }
    }
}

impl ProviderSettings {
    /// The card's `accent_color` as `0xRRGGBB`, `None` when unset or not a
    /// six-digit hex string.
    pub fn accent_rgb(&self) -> Option<u32> {
        let hex = self.accent_color.as_deref()?.trim().trim_start_matches('#');
        (hex.len() == 6 && hex.chars().all(|ch| ch.is_ascii_hexdigit()))
            .then(|| u32::from_str_radix(hex, 16).ok())
            .flatten()
    }

    /// A native provider's `Launch arguments` field, split on whitespace.
    pub fn extra_args(&self) -> Vec<String> {
        self.launch_args
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

/// A user-created provider profile (Settings → Providers "+ New profile").
///
/// A profile pairs a *protocol* ([`ProviderKind`] — which native CLI/adapter
/// spawns it) with a full [`ProviderSettings`] card. Several profiles may share
/// one protocol, which is how a session can talk to the official Anthropic API
/// *and* a third-party Anthropic-compatible endpoint at the same time: both are
/// `ProviderKind::ClaudeCode`, each with its own `ANTHROPIC_BASE_URL` /
/// `ANTHROPIC_API_KEY` / `ANTHROPIC_MODEL` env and its own isolated home.
///
/// The built-in profiles are not stored here — they
/// remain in [`Settings::providers`] under their [`provider_key`]. Only extra,
/// user-created profiles live in [`Settings::profiles`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// The protocol this profile drives. Determines which native adapter
    /// spawns it and how its native protocol is normalized.
    pub kind: ProviderKind,
    /// The card configuration (env, binary, home, models, display name, …).
    /// Flattened so a profile's JSON is a superset of a provider card's.
    #[serde(flatten)]
    pub settings: ProviderSettings,
}

/// A profile resolved to the two things the launch path needs: which protocol
/// to speak, and the effective card settings to spawn with. Produced by
/// [`Settings::resolved_profile`] for both built-in and user profiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    /// Stable profile id: a built-in [`provider_key`] or a user-chosen slug.
    pub id: String,
    /// The protocol this profile drives.
    pub kind: ProviderKind,
    /// The effective card settings.
    pub settings: ProviderSettings,
}

impl ResolvedProfile {
    /// Whether this endpoint/auth configuration can query native account limits.
    pub fn supports_account_usage(&self) -> bool {
        let (endpoint_key, native_endpoint, credentials, backends): (&str, &str, &[&str], &[&str]) =
            match self.kind {
                ProviderKind::ClaudeCode => (
                    "ANTHROPIC_BASE_URL",
                    "https://api.anthropic.com",
                    &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
                    &[
                        "CLAUDE_CODE_USE_BEDROCK",
                        "CLAUDE_CODE_USE_VERTEX",
                        "CLAUDE_CODE_USE_FOUNDRY",
                    ],
                ),
                ProviderKind::Codex => (
                    "OPENAI_BASE_URL",
                    "https://api.openai.com/v1",
                    &["OPENAI_API_KEY", "CODEX_API_KEY"],
                    &[],
                ),
                _ => return false,
            };
        // Secret presence is enough to classify API-key auth; never inspect or
        // replicate secret values. Missing native sign-in remains a probe error.
        !self.settings.env.iter().enumerate().any(|(index, env)| {
            // LaunchEnv uses the last occurrence of a repeated environment key.
            if self.settings.env[index + 1..]
                .iter()
                .any(|later| later.name == env.name)
            {
                return false;
            }
            let configured = env.sensitive || !env.value.trim().is_empty();
            configured
                && (credentials.contains(&env.name.as_str())
                    || (backends.contains(&env.name.as_str())
                        && env.value != "0"
                        && env.value != "false")
                    || (env.name == endpoint_key
                        && (env.sensitive
                            || env.value.trim().trim_end_matches('/').to_ascii_lowercase()
                                != native_endpoint)))
        })
    }
}

/// One configured model, unique by provider and model ID within its role.
/// Reasoning effort is selected per tool call from the provider's capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrateChildModel {
    pub provider: ProviderKind,
    pub model: String,
    /// Which provider profile (endpoint config) the dispatch launches against;
    /// `None` = the kind's built-in profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// Controls availability as a collaboration peer or executor in this list.
    /// Disabled entries retain their configuration and are omitted from the fleet.
    /// This never controls whether the model can run as the main decision model.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Dispatch with the provider's fast mode (Claude `fastMode`, Codex `fast`
    /// service tier). Ignored by providers without one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    /// The user's own guidance; empty uses the bundled guidance, see [`Self::guidance`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The bundled fleet profile an untouched row follows when the bundle
    /// moves it to another model. Maintained by [`OrchestrateSettings`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled: Option<String>,
}

impl OrchestrateChildModel {
    /// Bundled guidance for this model in the collaboration or execution list.
    pub fn bundled_guidance(&self, collaboration: bool) -> Option<&'static str> {
        orchestrate_fleet::bundled()
            .profile_for(Role::of(collaboration), self.provider, &self.model)
            .map(|profile| profile.guidance.as_str())
    }

    /// The guidance the model is given for this row.
    pub fn guidance(&self, collaboration: bool) -> &str {
        if self.description.trim().is_empty() {
            self.bundled_guidance(collaboration).unwrap_or_default()
        } else {
            &self.description
        }
    }
}

/// Live catalogs are authoritative. Bundled fallbacks cover startup before discovery.
/// Collaboration is deliberately capped at medium/high, independent of execution.
pub fn orchestrate_efforts(
    provider: ProviderKind,
    model: &str,
    catalog: &[ModelSpec],
    collaboration: bool,
) -> Vec<String> {
    let mut efforts = if let Some(spec) = catalog.iter().find(|spec| spec.id == model) {
        spec.options
            .iter()
            .find_map(|option| match option {
                OptionDescriptor::Select { id, options, .. } if id == "reasoningEffort" => Some(
                    options
                        .iter()
                        .map(|choice| choice.value.clone())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    } else {
        orchestrate_fleet::bundled()
            .effort_fallback(provider, model)
            .to_vec()
    };
    if collaboration {
        efforts.retain(|effort| matches!(effort.as_str(), "medium" | "high"));
    }
    efforts
}

/// Who answers the permission requests and questions raised by dispatched
/// child threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChildApprovalMode {
    Orchestrator,
    AlwaysAllow,
    Manual,
    #[default]
    #[serde(other)]
    Auto,
}

/// Settings for tcode's built-in orchestration layer.
///
/// Decision profiles support peer consultation; execution profiles receive tasks.
/// Any session may invoke the workflow. The bundled fleet is
/// `assets/orchestrate/fleet.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "OrchestrateSettingsData")]
pub struct OrchestrateSettings {
    /// Collaboration invite list, not an allow list for the main decision model.
    pub decision_models: Vec<OrchestrateChildModel>,
    pub child_models: Vec<OrchestrateChildModel>,
    #[serde(default)]
    pub child_approval: ChildApprovalMode,
    /// Give dispatched children dedicated Git worktrees when their resolved cwd
    /// is itself a repository root. Dispatch-level `worktree` overrides this.
    #[serde(default)]
    pub child_worktrees: bool,
}

impl Default for OrchestrateSettings {
    fn default() -> Self {
        let fleet = orchestrate_fleet::bundled();
        let rows = |role| {
            fleet
                .defaults(role)
                .map(|profile| OrchestrateChildModel {
                    provider: profile.provider,
                    model: profile.model.clone(),
                    profile_id: None,
                    enabled: true,
                    fast: false,
                    description: String::new(),
                    bundled: Some(profile.id.clone()),
                })
                .collect()
        };
        Self {
            decision_models: rows(Role::Collaboration),
            child_models: rows(Role::Execution),
            child_approval: ChildApprovalMode::default(),
            child_worktrees: false,
        }
    }
}

/// Ignore retired lead identities and consume old fixed efforts only for migration.
#[derive(Deserialize, Default)]
#[serde(default)]
struct OrchestrateSettingsData {
    decision_models: Option<Vec<LegacyOrchestrateModel>>,
    child_models: Vec<LegacyOrchestrateModel>,
    child_approval: ChildApprovalMode,
    child_worktrees: bool,
}

impl From<OrchestrateSettingsData> for OrchestrateSettings {
    fn from(data: OrchestrateSettingsData) -> Self {
        let (decision_models, child_models) = orchestrate_legacy::migrate(
            data.decision_models,
            data.child_models,
            Self::default().decision_models,
        );
        let mut settings = Self {
            decision_models,
            child_models,
            child_approval: data.child_approval,
            child_worktrees: data.child_worktrees,
        };
        settings.normalize_models(true);
        settings
    }
}

impl OrchestrateSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// A model occurs once per role; loading combines distinct notes within
    /// each list, patches keep the first row without mutating it. Rows store
    /// bundled guidance as an empty description, and an untouched row on the
    /// built-in endpoint tracks its bundled profile, whose model it takes on load.
    fn normalize_models(&mut self, loading: bool) {
        let fleet = orchestrate_fleet::bundled();
        for (role, models) in [
            (Role::Collaboration, &mut self.decision_models),
            (Role::Execution, &mut self.child_models),
        ] {
            let mut unique: Vec<OrchestrateChildModel> = Vec::new();
            for mut entry in std::mem::take(models) {
                entry.model = entry.model.trim().to_string();
                let tracked = entry.bundled.take();
                let description = entry.description.trim();
                if description.is_empty()
                    || fleet
                        .profile_for(role, entry.provider, &entry.model)
                        .is_some_and(|profile| profile.guidance == description)
                {
                    entry.description.clear();
                }
                if loading
                    && entry.description.is_empty()
                    && entry.profile_id.is_none()
                    && let Some(profile) = tracked
                        .and_then(|id| fleet.profile(&id))
                        .filter(|profile| profile.role == role)
                {
                    entry.provider = profile.provider;
                    entry.model = profile.model.clone();
                }
                if let Some(existing) = unique.iter_mut().find(|existing| {
                    existing.provider == entry.provider && existing.model == entry.model
                }) {
                    if loading
                        && !entry.description.is_empty()
                        && !existing.description.contains(&entry.description)
                    {
                        if !existing.description.is_empty() {
                            existing.description.push_str("\n\n");
                        }
                        existing.description.push_str(&entry.description);
                    }
                } else {
                    unique.push(entry);
                }
            }
            for entry in &mut unique {
                entry.bundled = if entry.description.is_empty() && entry.profile_id.is_none() {
                    fleet
                        .profile_for(role, entry.provider, &entry.model)
                        .map(|profile| profile.id.clone())
                } else {
                    None
                };
            }
            *models = unique;
        }
    }
}

/// Provider and model used for the isolated, background request that names a
/// thread, initially or on request. Reasoning effort is intentionally fixed to
/// `low` by the runtime: title generation is a small, latency-sensitive task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleGenerationSettings {
    #[serde(default = "default_title_provider")]
    pub provider: ProviderKind,
    #[serde(default = "default_title_model")]
    pub model: String,
    /// Which provider profile (endpoint config) the dispatch launches against;
    /// `None` = the kind's built-in profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

pub const DEFAULT_TITLE_MODEL: &str = "gpt-5.6-luna";

fn default_title_provider() -> ProviderKind {
    ProviderKind::Codex
}

fn default_title_model() -> String {
    DEFAULT_TITLE_MODEL.to_string()
}

impl Default for TitleGenerationSettings {
    fn default() -> Self {
        Self {
            provider: default_title_provider(),
            model: default_title_model(),
            profile_id: None,
        }
    }
}

impl TitleGenerationSettings {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackReviewSettings {
    #[serde(default = "default_title_provider")]
    pub provider: ProviderKind,
    #[serde(default = "default_title_model")]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

impl Default for FallbackReviewSettings {
    fn default() -> Self {
        Self {
            provider: default_title_provider(),
            model: default_title_model(),
            profile_id: None,
        }
    }
}

impl FallbackReviewSettings {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// When a computer-use observation carries a screenshot alongside the folded
/// accessibility outline. Mirrors `computer_use_mcp::config::ImageMode`; kept in
/// core so settings stay GPUI/backend-free and the app maps one to the other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageMode {
    /// Screenshot only when the outline looks too sparse to act on (default).
    #[default]
    Auto,
    /// Always attach a screenshot.
    Always,
    /// Never attach a screenshot (outline only).
    Never,
}

/// Global configuration for desktop computer-use tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputerUseSettings {
    /// Whether newly spawned provider sessions receive the computer-use MCP server.
    #[serde(default)]
    pub enabled: bool,
    /// When observations include a screenshot. Absent in legacy files → `auto`.
    #[serde(default)]
    pub image_mode: ImageMode,
    /// When false the tools are observe-only (`act_ui` rejects every action).
    /// Defaults to TRUE and tolerates an absent field in legacy files.
    #[serde(default = "default_true")]
    pub allow_input: bool,
    /// Permit an opt-in foreground HID retry for keyboard actions when
    /// background PID delivery cannot be initialized.
    #[serde(default)]
    pub allow_foreground_fallback: bool,
    /// Show the agent cursor overlay.
    #[serde(default = "default_true")]
    pub show_agent_cursor: bool,
}

impl Default for ComputerUseSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            image_mode: ImageMode::default(),
            allow_input: true,
            allow_foreground_fallback: false,
            show_agent_cursor: true,
        }
    }
}

/// Settings for the embedded preview browser (Settings → Browser) and the
/// `tcode_preview` MCP server it backs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSettings {
    /// Whether the embedded browser and its preview MCP tools are available.
    /// Defaults to TRUE; absent in legacy files → enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Initial page opened when the preview panel is shown without a target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_url: Option<String>,
    /// Whether the `preview_evaluate` MCP tool may run JavaScript. Defaults to
    /// TRUE; absent in legacy files → allowed.
    #[serde(default = "default_true")]
    pub allow_evaluate: bool,
}

/// Which providers' native plugins Settings → Plugins manages. Off, Tcode
/// never runs that CLI's plugin commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManagementSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-provider switches keyed by [`provider_key`]; an absent key takes
    /// [`Self::provider_default`]. Keyed by string rather than
    /// [`ProviderKind`] so a provider this build does not know survives a
    /// load and save instead of failing the whole file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, bool>,
}

impl Default for PluginManagementSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            providers: BTreeMap::new(),
        }
    }
}

impl PluginManagementSettings {
    pub fn provider_default(kind: ProviderKind) -> bool {
        kind == ProviderKind::Codex
    }

    /// The provider's own switch, regardless of the master switch.
    pub fn provider_switch(&self, kind: ProviderKind) -> bool {
        self.providers
            .get(provider_key(kind))
            .copied()
            .unwrap_or_else(|| Self::provider_default(kind))
    }

    /// Whether Tcode may touch this provider's plugin state at all.
    pub fn provider_enabled(&self, kind: ProviderKind) -> bool {
        self.enabled && self.provider_switch(kind)
    }

    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// A field-scoped mutation of persisted application settings.
///
/// Keeping nested settings mutations field-scoped prevents a writer holding a
/// stale snapshot from replacing unrelated fields changed by another writer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum SettingsPatch {
    /// Adds the host with `kind` when the settings do not list it yet; the kind of a listed host
    /// never changes.
    SourceControlHost {
        host: String,
        kind: HostKind,
        enabled: Option<bool>,
        account: Option<Option<String>>,
    },
    RemoveSourceControlHost {
        host: String,
    },
    Language(Option<String>),
    ThemeMode(ThemeMode),
    WordWrapDiffs(bool),
    SkipDeleteConfirmation(bool),
    AutoOpenTaskPanel(bool),
    LiveCommandPanelDisabled(bool),
    SidebarProviderMarks(bool),
    ProviderUpdateChecksDisabled(bool),
    InactiveFrameThrottleDisabled(bool),
    AbortOnModelFallback(bool),
    ResumeOnLimitReset(bool),
    FallbackReviewAdvisor(bool),
    AutoSettleAfterDays(Option<f64>),
    AutoSettleOnMerge(bool),
    ProjectSettlement {
        project_id: String,
        value: Option<ProjectSettlementSettings>,
    },
    ProjectMergeMethod {
        project_id: String,
        method: crate::pull_request::PullRequestMergeMethod,
    },
    RemoveAgentCreditsOnMerge(bool),
    OrchestrateDecisionModels(Vec<OrchestrateChildModel>),
    OrchestrateChildModels(Vec<OrchestrateChildModel>),
    OrchestrateChildApproval(ChildApprovalMode),
    OrchestrateChildWorktrees(bool),
    ComputerUseEnabled(bool),
    ComputerUseImageMode(ImageMode),
    ComputerUseAllowInput(bool),
    ComputerUseAllowForegroundFallback(bool),
    ComputerUseShowAgentCursor(bool),
    BrowserEnabled(bool),
    BrowserHomeUrl(Option<String>),
    BrowserAllowEvaluate(bool),
    PluginManagementEnabled(bool),
    PluginManagementProvider {
        provider: ProviderKind,
        enabled: bool,
    },
    ControlEnabled(bool),
    ControlDefaultGrant(crate::control::ControlGrant),
    ControlProjectGrant {
        project_id: String,
        value: Option<crate::control::ControlGrant>,
    },
    SettlementAutomation(crate::control::SettlementAutomation),
    TitleGenerationProvider(ProviderKind),
    TitleGenerationModel(String),
    TitleGenerationProfileId(Option<String>),
    FallbackReviewProvider(ProviderKind),
    FallbackReviewModel(String),
    FallbackReviewProfileId(Option<String>),
    SidebarLayout(SidebarLayout),
    RemoteHostingEnabled(bool),
    Traverse(TraverseSetting),
    RemoteHostName(Option<String>),
    LastProject(Option<String>),
}

/// The Traverse instances this machine publishes to while hosting: relay
/// fallback and wide-area discovery for devices off the LAN. The official
/// service is always the first source and is only ever disabled, never
/// removed; with every source disabled the machine uses no relay and no
/// wide-area discovery, and devices reach it on the LAN (DNS-SD) or at an
/// address the user types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "TraverseSettingFile")]
pub struct TraverseSetting {
    pub sources: Vec<TraverseSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseSource {
    #[serde(flatten)]
    pub instance: TraverseInstance,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraverseInstance {
    Official,
    /// A self-hosted instance, by the base URL its manifest is served from.
    Custom {
        url: String,
    },
}

/// Every shape a settings file has held: the source list, or the single
/// choice written before a machine could publish to several instances.
#[derive(Deserialize)]
#[serde(untagged)]
enum TraverseSettingFile {
    Sources { sources: Vec<TraverseSource> },
    Mode(TraverseMode),
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum TraverseMode {
    Official,
    Custom { url: String },
    Off,
}

impl From<TraverseSettingFile> for TraverseSetting {
    fn from(file: TraverseSettingFile) -> Self {
        let official = |enabled| TraverseSource {
            instance: TraverseInstance::Official,
            enabled,
        };
        let sources = match file {
            TraverseSettingFile::Sources { sources } => sources,
            TraverseSettingFile::Mode(TraverseMode::Official) => vec![official(true)],
            TraverseSettingFile::Mode(TraverseMode::Off) => vec![official(false)],
            // The old choice was either-or: a self-hosted instance replaced
            // the official one.
            TraverseSettingFile::Mode(TraverseMode::Custom { url }) => vec![
                official(false),
                TraverseSource {
                    instance: TraverseInstance::Custom { url },
                    enabled: true,
                },
            ],
        };
        Self::new(sources)
    }
}

impl Default for TraverseSetting {
    fn default() -> Self {
        Self {
            sources: vec![TraverseSource {
                instance: TraverseInstance::Official,
                enabled: true,
            }],
        }
    }
}

impl TraverseSetting {
    /// `sources` with the official service first exactly once, keeping the
    /// first official entry's switch, and each self-hosted URL once. A list
    /// that names no official entry leaves it off, as a self-hosted choice
    /// always has.
    pub fn new(sources: Vec<TraverseSource>) -> Self {
        let official = sources
            .iter()
            .find(|source| source.instance == TraverseInstance::Official)
            .is_some_and(|source| source.enabled);
        let mut normalized = vec![TraverseSource {
            instance: TraverseInstance::Official,
            enabled: official,
        }];
        for source in sources {
            if !normalized
                .iter()
                .any(|kept| kept.instance == source.instance)
            {
                normalized.push(source);
            }
        }
        Self {
            sources: normalized,
        }
    }

    pub fn enabled(&self) -> impl Iterator<Item = &TraverseInstance> {
        self.sources
            .iter()
            .filter(|source| source.enabled)
            .map(|source| &source.instance)
    }

    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            home_url: None,
            allow_evaluate: true,
        }
    }
}

impl BrowserSettings {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

// `Eq` is intentionally absent: `acp_agents` holds `AcpLaunch`, which the
// agent crate derives only `PartialEq` for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub source_control: SourceControlSettings,
    /// Legacy input: moved into `source_control` on load; never written back.
    #[serde(default, skip_serializing)]
    pub github: Option<LegacyGitHubSettings>,
    /// None follows the operating-system language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Per-provider cards (Settings → Providers), keyed by [`provider_key`].
    /// These are the built-in native-provider profiles.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderSettings>,
    /// User-created provider profiles, keyed by a stable slug id. Each carries
    /// its own [`ProviderKind`], so multiple profiles can drive the same
    /// protocol (e.g. official Claude + a third-party endpoint). Built-in
    /// profiles are *not* here — they live in `providers`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, ProviderProfile>,
    /// Legacy (pre-`providers`) binary overrides. Read once and migrated into
    /// `providers` on load; never written back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_binary: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_binary: Option<PathBuf>,
    #[serde(default)]
    pub theme_mode: ThemeMode,
    /// Whether the sidebar is collapsed to its icon strip. Persisted so the
    /// choice survives a restart (absent in legacy files → expanded).
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Default soft-wrap for long lines in the diff panel. Tolerantly added:
    /// absent in legacy settings.json files (defaults to off).
    #[serde(default)]
    pub word_wrap_diffs: bool,
    /// When true, the inline archive/delete action skips its confirm dialog.
    /// Stored inverted so legacy files (field absent → false) keep the confirm
    /// dialog on by default. Surfaced as the "Delete confirmation" toggle.
    #[serde(default)]
    pub skip_delete_confirmation: bool,
    /// When true, the right-side plan/task panel opens automatically the first
    /// time steps appear in a turn (unless the user closed it during that turn).
    /// Absent in legacy files (defaults to off).
    #[serde(default)]
    pub auto_open_task_panel: bool,
    /// Whether the live command panel is DISABLED. Stored inverted so absent
    /// legacy settings keep the feature enabled.
    #[serde(default)]
    pub live_command_panel_disabled: bool,
    /// Whether sidebar thread rows show their provider's mark. Off by
    /// default and absent in legacy files.
    #[serde(default)]
    pub sidebar_provider_marks: bool,
    /// Whether the on-launch provider version check is DISABLED. Stored inverted
    /// so it remains enabled for legacy settings files that lack the field.
    #[serde(default)]
    pub provider_update_checks_disabled: bool,
    /// Whether inactive-window frame throttling is DISABLED. Stored inverted so
    /// the throttle defaults to on even for legacy settings files that lack the field.
    #[serde(default)]
    pub inactive_frame_throttle_disabled: bool,
    #[serde(default = "default_true")]
    pub abort_on_model_fallback: bool,
    #[serde(default = "default_true")]
    pub resume_on_limit_reset: bool,
    #[serde(default)]
    pub fallback_review_advisor: bool,
    #[serde(default = "default_auto_settle_after_days")]
    pub auto_settle_after_days: Option<f64>,
    #[serde(default)]
    pub auto_settle_on_merge: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_settlement_overrides: BTreeMap<String, ProjectSettlementSettings>,
    /// The merge method a project's pull requests are merged with unless another is chosen,
    /// set from the merge confirmation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_merge_methods: BTreeMap<String, crate::pull_request::PullRequestMergeMethod>,
    /// Whether a merge leaves agents' credit lines out of GitHub's merge message by default.
    #[serde(default)]
    pub remove_agent_credits_on_merge: bool,
    /// Built-in orchestration identities and child-model routing table.
    #[serde(default, skip_serializing_if = "OrchestrateSettings::is_default")]
    pub orchestrate: OrchestrateSettings,
    /// Global desktop computer-use feature settings. Absent in legacy files,
    /// where the feature remains disabled by default.
    #[serde(default)]
    pub computer_use: ComputerUseSettings,
    /// Embedded preview browser settings. Skipped when default (like
    /// `orchestrate`), so legacy files stay clean and load with the defaults.
    #[serde(default, skip_serializing_if = "BrowserSettings::is_default")]
    pub browser: BrowserSettings,
    #[serde(default, skip_serializing_if = "PluginManagementSettings::is_default")]
    pub plugins: PluginManagementSettings,
    /// What a thread's model may do in Tcode through `tcode_control`.
    #[serde(
        default,
        skip_serializing_if = "crate::control::ControlSettings::is_default"
    )]
    pub control: crate::control::ControlSettings,
    #[serde(
        default,
        skip_serializing_if = "crate::control::SettlementAutomation::is_default"
    )]
    pub settlement_automation: crate::control::SettlementAutomation,
    /// Provider/model used to generate a concise title for new threads.
    #[serde(default, skip_serializing_if = "TitleGenerationSettings::is_default")]
    pub title_generation: TitleGenerationSettings,
    #[serde(default, skip_serializing_if = "FallbackReviewSettings::is_default")]
    pub fallback_review: FallbackReviewSettings,
    /// Ids of project groups the user has collapsed in the sidebar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed_projects: Vec<String>,
    /// Model ids the user has starred in the model picker (favorites float to
    /// the top and are shown first under the star filter).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub favorite_models: Vec<String>,
    /// Sidebar PROJECTS ordering (cycled by the sort button).
    #[serde(default)]
    pub project_sort: ProjectSort,
    /// Sidebar thread layout (flat by default; grouped keeps the legacy view).
    #[serde(default)]
    pub sidebar_layout: SidebarLayout,
    /// Whether this desktop app also serves the remote protocol to other tcode
    /// clients. Absent in legacy files → hosting off.
    #[serde(default)]
    pub remote_hosting_enabled: bool,
    /// Traverse instances used while hosting. Absent in legacy files →
    /// official.
    #[serde(default, skip_serializing_if = "TraverseSetting::is_default")]
    pub traverse: TraverseSetting,
    /// Name this host advertises while pairing and on the discovery beacon.
    /// None uses the machine name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_host_name: Option<String>,
    /// Per-session last-visited time (unix secs), keyed by session id. A session
    /// whose `updated_at` exceeds its last-visited time (and isn't active) shows
    /// an unread dot. A client that has loaded a thread advances it to the
    /// `updated_at` it showed; "Mark unread" sets it just below.
    /// UI state; absent in legacy files.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub last_visited: HashMap<String, u64>,
    /// Project the user last navigated to or started a thread in. A workspace
    /// with no conversation open (launch, or the thread on screen going away)
    /// opens this project's new-thread draft. Set from user navigation only, so
    /// background activity cannot move it. UI state; absent in legacy files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_project_id: Option<String>,
    /// ACP agents the user installed from the marketplace (or defined by hand),
    /// keyed by registry id. Each carries its resolved launch recipe, so a
    /// session can start without consulting the registry again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub acp_agents: BTreeMap<String, InstalledAcpAgent>,
    /// Keys this build does not know about, preserved verbatim on save.
    ///
    /// Without this, an older build (or any build predating a field) would drop
    /// the unknown key on load and silently destroy it on the next save — one
    /// downgrade, and your installed ACP agents or provider config are gone.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub unknown: serde_json::Map<String, serde_json::Value>,
}

const fn default_auto_settle_after_days() -> Option<f64> {
    Some(3.0)
}

/// Auto-settle after 1 to 90 days, or never.
fn auto_settle_days(days: Option<f64>) -> Result<Option<f64>, &'static str> {
    match days {
        Some(days) if !(1.0..=90.0).contains(&days) => {
            Err("Auto-settle days must be between 1 and 90.")
        }
        days => Ok(days),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectSettlementSettings {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_days_override"
    )]
    pub auto_settle_after_days: Option<Option<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_settle_on_merge: Option<bool>,
}

fn deserialize_days_override<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<f64>>, D::Error> {
    Option::<f64>::deserialize(deserializer).map(Some)
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source_control: SourceControlSettings::default(),
            github: None,
            language: None,
            providers: BTreeMap::new(),
            profiles: BTreeMap::new(),
            codex_binary: None,
            claude_binary: None,
            theme_mode: ThemeMode::default(),
            sidebar_collapsed: false,
            word_wrap_diffs: false,
            skip_delete_confirmation: false,
            auto_open_task_panel: false,
            live_command_panel_disabled: false,
            sidebar_provider_marks: false,
            provider_update_checks_disabled: false,
            inactive_frame_throttle_disabled: false,
            abort_on_model_fallback: true,
            resume_on_limit_reset: true,
            fallback_review_advisor: false,
            auto_settle_after_days: default_auto_settle_after_days(),
            auto_settle_on_merge: false,
            project_settlement_overrides: BTreeMap::new(),
            project_merge_methods: BTreeMap::new(),
            remove_agent_credits_on_merge: false,
            orchestrate: OrchestrateSettings::default(),
            computer_use: ComputerUseSettings::default(),
            browser: BrowserSettings::default(),
            plugins: PluginManagementSettings::default(),
            control: crate::control::ControlSettings::default(),
            settlement_automation: crate::control::SettlementAutomation::default(),
            title_generation: TitleGenerationSettings::default(),
            fallback_review: FallbackReviewSettings::default(),
            collapsed_projects: Vec::new(),
            favorite_models: Vec::new(),
            project_sort: ProjectSort::default(),
            sidebar_layout: SidebarLayout::default(),
            remote_hosting_enabled: false,
            traverse: TraverseSetting::default(),
            remote_host_name: None,
            last_visited: HashMap::new(),
            last_project_id: None,
            acp_agents: BTreeMap::new(),
            unknown: serde_json::Map::new(),
        }
    }
}

impl Settings {
    /// Apply one field-scoped mutation without replacing sibling fields. A
    /// value outside its field's range is refused and changes nothing.
    pub fn apply(&mut self, patch: SettingsPatch) -> Result<(), &'static str> {
        match patch {
            SettingsPatch::SourceControlHost {
                host,
                kind,
                enabled,
                account,
            } => {
                let entry = self
                    .source_control
                    .hosts
                    .entry(host.trim().to_ascii_lowercase())
                    .or_insert_with(|| HostSettings::new(kind));
                if entry.kind != kind {
                    return Err("A host's kind does not change.");
                }
                if let Some(enabled) = enabled {
                    entry.enabled = enabled;
                }
                if let Some(account) = account {
                    entry.account = account.filter(|value| !value.trim().is_empty());
                }
            }
            SettingsPatch::RemoveSourceControlHost { host } => {
                self.source_control
                    .hosts
                    .remove(&host.trim().to_ascii_lowercase());
            }
            SettingsPatch::Language(value) => self.language = value,
            SettingsPatch::ThemeMode(value) => self.theme_mode = value,
            SettingsPatch::WordWrapDiffs(value) => self.word_wrap_diffs = value,
            SettingsPatch::SkipDeleteConfirmation(value) => {
                self.skip_delete_confirmation = value;
            }
            SettingsPatch::AutoOpenTaskPanel(value) => self.auto_open_task_panel = value,
            SettingsPatch::LiveCommandPanelDisabled(value) => {
                self.live_command_panel_disabled = value;
            }
            SettingsPatch::SidebarProviderMarks(value) => {
                self.sidebar_provider_marks = value;
            }
            SettingsPatch::ProviderUpdateChecksDisabled(value) => {
                self.provider_update_checks_disabled = value;
            }
            SettingsPatch::InactiveFrameThrottleDisabled(value) => {
                self.inactive_frame_throttle_disabled = value;
            }
            SettingsPatch::AbortOnModelFallback(value) => {
                self.abort_on_model_fallback = value;
            }
            SettingsPatch::ResumeOnLimitReset(value) => {
                self.resume_on_limit_reset = value;
            }
            SettingsPatch::FallbackReviewAdvisor(value) => {
                self.fallback_review_advisor = value;
            }
            SettingsPatch::AutoSettleAfterDays(value) => {
                self.auto_settle_after_days = auto_settle_days(value)?;
            }
            SettingsPatch::AutoSettleOnMerge(value) => self.auto_settle_on_merge = value,
            SettingsPatch::ProjectSettlement { project_id, value } => {
                if let Some(mut value) = value {
                    if let Some(days) = value.auto_settle_after_days {
                        value.auto_settle_after_days = Some(auto_settle_days(days)?);
                    }
                    self.project_settlement_overrides.insert(project_id, value);
                } else {
                    self.project_settlement_overrides.remove(&project_id);
                }
            }
            SettingsPatch::ProjectMergeMethod { project_id, method } => {
                self.project_merge_methods.insert(project_id, method);
            }
            SettingsPatch::RemoveAgentCreditsOnMerge(value) => {
                self.remove_agent_credits_on_merge = value;
            }
            SettingsPatch::OrchestrateDecisionModels(value) => {
                self.orchestrate.decision_models = value;
                self.orchestrate.normalize_models(false);
            }
            SettingsPatch::OrchestrateChildModels(value) => {
                self.orchestrate.child_models = value;
                self.orchestrate.normalize_models(false);
            }
            SettingsPatch::OrchestrateChildApproval(value) => {
                self.orchestrate.child_approval = value;
            }
            SettingsPatch::OrchestrateChildWorktrees(value) => {
                self.orchestrate.child_worktrees = value;
            }
            SettingsPatch::ComputerUseEnabled(value) => self.computer_use.enabled = value,
            SettingsPatch::ComputerUseImageMode(value) => self.computer_use.image_mode = value,
            SettingsPatch::ComputerUseAllowInput(value) => self.computer_use.allow_input = value,
            SettingsPatch::ComputerUseAllowForegroundFallback(value) => {
                self.computer_use.allow_foreground_fallback = value
            }
            SettingsPatch::ComputerUseShowAgentCursor(value) => {
                self.computer_use.show_agent_cursor = value
            }
            SettingsPatch::BrowserEnabled(value) => self.browser.enabled = value,
            SettingsPatch::BrowserHomeUrl(value) => self.browser.home_url = value,
            SettingsPatch::BrowserAllowEvaluate(value) => self.browser.allow_evaluate = value,
            SettingsPatch::PluginManagementEnabled(value) => self.plugins.enabled = value,
            SettingsPatch::PluginManagementProvider { provider, enabled } => {
                self.plugins
                    .providers
                    .insert(provider_key(provider).to_string(), enabled);
            }
            SettingsPatch::ControlEnabled(value) => self.control.enabled = value,
            SettingsPatch::ControlDefaultGrant(value) => self.control.default_grant = value,
            SettingsPatch::ControlProjectGrant { project_id, value } => match value {
                Some(value) => {
                    self.control.project_overrides.insert(project_id, value);
                }
                None => {
                    self.control.project_overrides.remove(&project_id);
                }
            },
            SettingsPatch::SettlementAutomation(value) => self.settlement_automation = value,
            SettingsPatch::TitleGenerationProvider(value) => {
                self.title_generation.provider = value;
            }
            SettingsPatch::TitleGenerationModel(value) => self.title_generation.model = value,
            SettingsPatch::TitleGenerationProfileId(value) => {
                self.title_generation.profile_id = value;
            }
            SettingsPatch::FallbackReviewProvider(value) => {
                self.fallback_review.provider = value;
            }
            SettingsPatch::FallbackReviewModel(value) => self.fallback_review.model = value,
            SettingsPatch::FallbackReviewProfileId(value) => {
                self.fallback_review.profile_id = value;
            }
            SettingsPatch::SidebarLayout(value) => self.sidebar_layout = value,
            SettingsPatch::RemoteHostingEnabled(value) => self.remote_hosting_enabled = value,
            SettingsPatch::Traverse(value) => self.traverse = TraverseSetting::new(value.sources),
            SettingsPatch::RemoteHostName(value) => self.remote_host_name = value,
            SettingsPatch::LastProject(value) => self.last_project_id = value,
        }
        Ok(())
    }
}

impl Settings {
    /// This provider's card settings (defaults when never configured).
    pub fn provider(&self, provider: ProviderKind) -> ProviderSettings {
        self.providers
            .get(provider_key(provider))
            .cloned()
            .unwrap_or_default()
    }

    /// Mutable access, inserting defaults on first write.
    pub fn provider_mut(&mut self, provider: ProviderKind) -> &mut ProviderSettings {
        self.providers
            .entry(provider_key(provider).to_string())
            .or_default()
    }

    /// The built-in profile id for a native protocol. This is the id a session
    /// carries when it uses the default, non-custom configuration for its kind.
    ///
    /// Built-in ids resolve before user profiles, whose ids are slugs of ASCII
    /// alphanumerics and hyphens. Providers added after user profiles existed
    /// therefore take a `native:` id no slug can produce, so a user profile
    /// that already has the provider's name keeps resolving to itself.
    pub fn builtin_profile_id(kind: ProviderKind) -> &'static str {
        match kind {
            ProviderKind::Cursor => "native:cursor",
            ProviderKind::Grok => "native:grok",
            ProviderKind::ClaudeCode
            | ProviderKind::Codex
            | ProviderKind::Pi
            | ProviderKind::OpenCode
            | ProviderKind::Acp => provider_key(kind),
        }
    }

    /// Whether `id` names a built-in provider profile.
    pub fn is_builtin_profile_id(id: &str) -> bool {
        Self::builtin_kind_from_id(id).is_some()
    }

    /// The protocol kind of a built-in profile id, if it is one. Used to route a
    /// mutation of a built-in profile back to its `providers` card.
    pub fn builtin_kind_from_id(id: &str) -> Option<ProviderKind> {
        ProviderKind::NATIVE
            .into_iter()
            .chain([ProviderKind::Acp])
            .find(|kind| Self::builtin_profile_id(*kind) == id)
    }

    /// Resolve a profile id to its protocol kind and effective card settings.
    /// Built-in ids resolve to the matching `providers` card; anything else to
    /// a user-created `profiles` entry. `None` for an unknown id.
    pub fn resolved_profile(&self, id: &str) -> Option<ResolvedProfile> {
        if let Some(kind) = Self::builtin_kind_from_id(id) {
            return Some(ResolvedProfile {
                id: id.to_string(),
                kind,
                settings: self.provider(kind),
            });
        }
        self.profiles.get(id).map(|profile| ResolvedProfile {
            id: id.to_string(),
            kind: profile.kind,
            settings: profile.settings.clone(),
        })
    }

    /// Every selectable profile that drives `kind`: the built-in first, then any
    /// user profiles of that kind in id order. This is what the provider/model
    /// picker iterates.
    pub fn profiles_for_kind(&self, kind: ProviderKind) -> Vec<ResolvedProfile> {
        let mut out = vec![ResolvedProfile {
            id: Self::builtin_profile_id(kind).to_string(),
            kind,
            settings: self.provider(kind),
        }];
        for (id, profile) in &self.profiles {
            if profile.kind == kind {
                out.push(ResolvedProfile {
                    id: id.clone(),
                    kind,
                    settings: profile.settings.clone(),
                });
            }
        }
        out
    }

    /// A profile's card title: its display-name override, else — for built-ins —
    /// the driver label, else the id. Used by the sidebar / picker / status row.
    pub fn profile_display_name(&self, id: &str) -> String {
        let Some(profile) = self.resolved_profile(id) else {
            return id.to_string();
        };
        if let Some(name) = profile
            .settings
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            return name.to_string();
        }
        if Self::is_builtin_profile_id(id) {
            provider_label(profile.kind).to_string()
        } else {
            id.to_string()
        }
    }

    /// Turn a human name into a stable, unique profile id (slug). Never collides
    /// with a built-in id or an existing profile id.
    pub fn allocate_profile_id(&self, name: &str) -> String {
        let base: String = name
            .trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let base = base.trim_matches('-');
        let base = if base.is_empty() { "profile" } else { base };
        let taken =
            |id: &str, s: &Settings| Self::is_builtin_profile_id(id) || s.profiles.contains_key(id);
        if !taken(base, self) {
            return base.to_string();
        }
        let mut n = 2;
        loop {
            let candidate = format!("{base}-{n}");
            if !taken(&candidate, self) {
                return candidate;
            }
            n += 1;
        }
    }

    /// The `0xRRGGBB` color for a provider color key (see
    /// [`SessionMeta::provider_color_key`](crate::project::SessionMeta::provider_color_key)).
    /// A card's own accent wins when set; otherwise built-in profiles use their
    /// brand color and user profiles and ACP agents take a palette slot that is
    /// distinct from every other configured custom provider while the palette
    /// has room. ACP keys are not profile ids, so they never carry an accent.
    pub fn provider_color(&self, key: &str) -> u32 {
        if let Some(accent) = self
            .resolved_profile(key)
            .and_then(|profile| profile.settings.accent_rgb())
        {
            return accent;
        }
        if let Some(color) = Self::builtin_kind_from_id(key).and_then(builtin_provider_color) {
            return color;
        }
        let acp_keys: Vec<String> = self.acp_agents.keys().map(|id| acp_color_key(id)).collect();
        let mut known: Vec<&str> = self
            .profiles
            .keys()
            .map(String::as_str)
            .chain(acp_keys.iter().map(String::as_str))
            .collect();
        known.sort_unstable();
        crate::provider_colors::palette_color(key, &known)
    }

    /// One installed ACP agent, by registry id.
    pub fn acp_agent(&self, id: &str) -> Option<&InstalledAcpAgent> {
        self.acp_agents.get(id)
    }

    /// Every installed ACP agent, in registry-id order (the marketplace and the
    /// provider rail both render them in this order).
    pub fn installed_acp_agents(&self) -> Vec<&InstalledAcpAgent> {
        self.acp_agents.values().collect()
    }

    /// Fold the pre-`providers` binary overrides into the map (once, on load)
    /// and drop the port of the retired HTTP listener, which no build reads.
    pub fn migrate_legacy(&mut self) {
        self.unknown.remove("remote_port");
        for (host, legacy) in self.github.take().unwrap_or_default().hosts {
            self.source_control
                .hosts
                .entry(host.trim().to_ascii_lowercase())
                .or_insert(HostSettings {
                    kind: HostKind::Github,
                    enabled: legacy.enabled,
                    account: legacy.account,
                });
        }
        for (provider, legacy) in [
            (ProviderKind::Codex, self.codex_binary.take()),
            (ProviderKind::ClaudeCode, self.claude_binary.take()),
        ] {
            if let Some(path) = legacy {
                let entry = self.provider_mut(provider);
                if entry.binary_path.is_none() {
                    entry.binary_path = Some(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_approval_settings_preserve_old_orchestrator_and_default_to_auto() {
        for (json, expected) in [
            (r#"{}"#, ChildApprovalMode::Auto),
            (
                r#"{"child_approval":"orchestrator"}"#,
                ChildApprovalMode::Orchestrator,
            ),
            (r#"{"child_approval":"auto"}"#, ChildApprovalMode::Auto),
            (
                r#"{"child_approval":"always_allow"}"#,
                ChildApprovalMode::AlwaysAllow,
            ),
            (r#"{"child_approval":"manual"}"#, ChildApprovalMode::Manual),
            (
                r#"{"child_approval":"future_value"}"#,
                ChildApprovalMode::Auto,
            ),
        ] {
            let settings: OrchestrateSettings = serde_json::from_str(json).unwrap();
            assert_eq!(settings.child_approval, expected, "{json}");
        }
        assert_eq!(
            OrchestrateSettings::default().child_approval,
            ChildApprovalMode::Auto
        );
        assert_eq!(
            serde_json::to_value(ChildApprovalMode::Auto).unwrap(),
            "auto"
        );
    }

    #[test]
    fn older_settings_preserve_access_policy_and_accept_partial_feature_blocks() {
        let legacy: Settings = serde_json::from_str(r#"{"theme_mode":"system"}"#).unwrap();
        assert_eq!(legacy.auto_settle_after_days, Some(3.0));
        assert!(!legacy.auto_settle_on_merge);
        assert!(!legacy.sidebar_provider_marks);
        assert!(!legacy.sidebar_collapsed);
        assert!(!legacy.remote_hosting_enabled);
        assert_eq!(legacy.remote_host_name, None);
        assert!(legacy.profiles.is_empty());
        assert_eq!(legacy.title_generation.profile_id, None);
        assert!(!legacy.computer_use.enabled);
        assert!(legacy.browser.enabled);
        assert!(legacy.browser.allow_evaluate);
        assert_eq!(legacy.browser.home_url, None);
        for settings in [&legacy, &Settings::default()] {
            assert!(settings.plugins.provider_enabled(ProviderKind::Codex));
            for kind in [
                ProviderKind::ClaudeCode,
                ProviderKind::Pi,
                ProviderKind::OpenCode,
                ProviderKind::Cursor,
                ProviderKind::Grok,
                ProviderKind::Acp,
            ] {
                assert!(!settings.plugins.provider_enabled(kind), "{kind:?}");
            }
        }

        let partial: Settings = serde_json::from_str(
            r#"{
            "computer_use":{"enabled":true},
            "browser":{"enabled":false},
            "orchestrate":{},
            "plugins":{"providers":{"claude":true,"kiro":true}},
            "title_generation":{"provider":"codex","model":"m"}
        }"#,
        )
        .unwrap();
        assert!(partial.computer_use.enabled);
        assert_eq!(partial.computer_use.image_mode, ImageMode::Auto);
        assert!(partial.computer_use.allow_input);
        assert!(!partial.computer_use.allow_foreground_fallback);
        assert!(partial.computer_use.show_agent_cursor);
        assert!(!partial.browser.enabled);
        assert!(partial.browser.allow_evaluate);
        assert!(!partial.orchestrate.child_worktrees);
        assert_eq!(partial.title_generation.profile_id, None);
        assert!(partial.plugins.provider_enabled(ProviderKind::ClaudeCode));
        assert!(partial.plugins.provider_enabled(ProviderKind::Codex));
        let master_off: Settings =
            serde_json::from_str(r#"{"plugins":{"enabled":false,"providers":{"claude":true}}}"#)
                .unwrap();
        assert!(!master_off.plugins.provider_enabled(ProviderKind::Codex));
        assert!(
            !master_off
                .plugins
                .provider_enabled(ProviderKind::ClaudeCode)
        );

        let child: OrchestrateChildModel =
            serde_json::from_str(r#"{"provider":"codex","model":"m","enabled":true}"#).unwrap();
        assert_eq!(child.profile_id, None);
        assert!(!child.fast);
    }

    #[test]
    fn provider_color_maps_builtins_to_brand_and_custom_providers_to_distinct_palette_slots() {
        let settings = Settings::default();
        assert_eq!(settings.provider_color("claude"), 0xD97757);
        assert_eq!(settings.provider_color("codex"), 0x8B5CF6);
        assert_eq!(settings.provider_color("pi"), 0x4D9ABF);
        assert_eq!(settings.provider_color("opencode"), 0x22A06B);

        for (profiles, agents, expected) in [
            (
                vec!["work-claude".to_string()],
                vec!["gemini"],
                Some(vec![0x6B7FD7, 0x14B8A6]),
            ),
            (
                ["work-claude", "proxy-codex", "lab"]
                    .map(String::from)
                    .to_vec(),
                vec!["gemini", "goose"],
                None,
            ),
            (
                (0..13).map(|i| format!("profile-{i:02}")).collect(),
                vec![],
                None,
            ),
        ] {
            let mut settings = Settings::default();
            for id in &profiles {
                settings.profiles.insert(
                    id.clone(),
                    ProviderProfile {
                        kind: ProviderKind::ClaudeCode,
                        settings: ProviderSettings::default(),
                    },
                );
            }
            for id in &agents {
                settings.acp_agents.insert(
                    (*id).into(),
                    InstalledAcpAgent {
                        id: (*id).into(),
                        name: (*id).into(),
                        version: String::new(),
                        icon: None,
                        launch: agent::AcpLaunch::Npx {
                            package: (*id).into(),
                            args: Vec::new(),
                            env: Vec::new(),
                        },
                        enabled: true,
                        env: Vec::new(),
                        launch_args: None,
                    },
                );
            }
            let keys: Vec<_> = profiles
                .into_iter()
                .chain(agents.iter().map(|id| format!("acp:{id}")))
                .collect();
            let colors: Vec<u32> = keys
                .iter()
                .map(|key| settings.provider_color(key))
                .collect();
            if let Some(expected) = expected {
                assert_eq!(colors, expected);
            }
            let mut first_palette: Vec<_> = colors
                .iter()
                .take(PROVIDER_COLOR_PALETTE.len())
                .copied()
                .collect();
            first_palette.sort_unstable();
            first_palette.dedup();
            assert_eq!(
                first_palette.len(),
                keys.len().min(PROVIDER_COLOR_PALETTE.len()),
                "{colors:06X?}"
            );
            for color in &colors {
                assert!(PROVIDER_COLOR_PALETTE.contains(color));
                assert!(![0xD97757, 0x8B5CF6, 0x4D9ABF, 0x22A06B].contains(color));
            }
            assert_eq!(
                colors,
                keys.iter()
                    .map(|key| settings.provider_color(key))
                    .collect::<Vec<_>>()
            );
            assert_eq!(settings.provider_color("deleted-profile"), 0xF59E0B);
        }
    }

    #[test]
    fn card_accent_wins_over_brand_and_palette_when_valid() {
        let mut settings = Settings::default();
        settings.profiles.insert(
            "work-claude".into(),
            ProviderProfile {
                kind: ProviderKind::ClaudeCode,
                settings: ProviderSettings {
                    accent_color: Some("#2563eb".into()),
                    ..ProviderSettings::default()
                },
            },
        );
        settings.profiles.insert(
            "broken".into(),
            ProviderProfile {
                kind: ProviderKind::Codex,
                settings: ProviderSettings {
                    accent_color: Some("blue".into()),
                    ..ProviderSettings::default()
                },
            },
        );
        settings.provider_mut(ProviderKind::Codex).accent_color = Some("00FF00".into());
        assert_eq!(settings.provider_color("work-claude"), 0x2563EB);
        assert_eq!(settings.provider_color("codex"), 0x00FF00);
        assert!(PROVIDER_COLOR_PALETTE.contains(&settings.provider_color("broken")));
        assert_eq!(settings.provider_color("claude"), 0xD97757);
    }

    #[test]
    fn old_pi_provider_settings_field_names_remain_serde_compatible() {
        let legacy: ProviderSettings = serde_json::from_str("{}").unwrap();
        assert!(!legacy.pi.trust_project_extensions);

        let old_json = r#"{
            "pi_trust_project_extensions": true
        }"#;
        let settings: ProviderSettings = serde_json::from_str(old_json).unwrap();
        assert!(settings.pi.trust_project_extensions);
        let serialized = serde_json::to_value(&settings).unwrap();
        assert_eq!(serialized["pi_trust_project_extensions"], true);
        assert!(serialized.get("pi").is_none());

        let legacy_patch: ProfileConfigurationPatch = serde_json::from_str(
            r#"{
                "display_name": null,
                "accent_color": null,
                "env": [],
                "binary_path": null,
                "home_path": null,
                "launch_args": null,
                "custom_models": [],
                "hidden_models": []
            }"#,
        )
        .unwrap();
        assert!(!legacy_patch.pi.trust_project_extensions);
    }

    fn persisted(name: &str) -> String {
        let texts: HashMap<String, String> = serde_json::from_str(include_str!(
            "../tests/fixtures/orchestrate/persisted_bundled_texts.json"
        ))
        .unwrap();
        texts[name].clone()
    }

    fn orchestrate(value: serde_json::Value) -> OrchestrateSettings {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn orchestrate_defaults_and_legacy_migration() {
        let defaults = OrchestrateSettings::default();
        assert_eq!(
            defaults
                .decision_models
                .iter()
                .map(|entry| entry.model.as_str())
                .collect::<Vec<_>>(),
            ["gpt-6-astra", "claude-fable-5-1"]
        );
        assert_eq!(defaults.child_models.len(), 2);
        assert_eq!(defaults.child_models[0].model, "gpt-6.1-sol");
        assert_eq!(defaults.child_models[0].provider, ProviderKind::Codex);
        assert!(!defaults.child_models[0].fast);
        assert!(
            defaults.child_models[0]
                .guidance(false)
                .contains("Start at medium")
        );
        let legacy: Settings = serde_json::from_str(r#"{"theme_mode":"system"}"#).unwrap();
        assert_eq!(legacy.orchestrate, defaults);
        let migrated = orchestrate(serde_json::json!({
            "child_models": [
                {"provider": "codex", "model": "gpt-5.6-sol", "enabled": true, "fast": false,
                 "description": persisted("sol_5_6_execution")},
                {"provider": "claude_code", "model": "claude-opus-5-5", "enabled": true, "fast": false,
                 "description": persisted("opus_5_5_execution")},
                {"provider": "codex", "model": "gpt-6-astra", "enabled": true, "fast": false,
                 "description": persisted("astra_collaboration")},
                {"provider": "claude_code", "model": "claude-fable-5-1", "enabled": false, "fast": false,
                 "description": "Custom consultation guidance"}
            ],
            "generic_identity": "old self-concept",
            "model_identities": [{"provider":"codex","model":"gpt-5.6-sol","identity":"old identity"}]
        }));
        assert_eq!(migrated.child_models, defaults.child_models);
        assert_eq!(
            migrated.decision_models,
            [
                defaults.decision_models[0].clone(),
                OrchestrateChildModel {
                    enabled: false,
                    description: "Custom consultation guidance".into(),
                    bundled: None,
                    ..defaults.decision_models[1].clone()
                }
            ]
        );
        let json = serde_json::to_string(&migrated).unwrap();
        assert!(!json.contains("identity"));
        assert_eq!(
            serde_json::from_str::<OrchestrateSettings>(&json).unwrap(),
            migrated
        );
        let empty: OrchestrateSettings =
            serde_json::from_str(r#"{"decision_models":[],"child_models":[]}"#).unwrap();
        let legacy_empty: OrchestrateSettings =
            serde_json::from_str(r#"{"generic_identity":"old instructions"}"#).unwrap();
        assert!(
            legacy_empty.child_models.is_empty(),
            "omitted legacy execution list stays empty"
        );
        assert!(empty.decision_models.is_empty());
        assert!(empty.child_models.is_empty());
        assert_eq!(
            serde_json::from_str::<OrchestrateSettings>(&serde_json::to_string(&empty).unwrap())
                .unwrap(),
            empty
        );
    }

    #[test]
    fn orchestrate_rows_store_bundled_guidance_by_reference() {
        let bundled_copies = serde_json::json!({
            "decision_models": [
                {"provider": "codex", "model": "gpt-6-astra", "enabled": true,
                 "description": persisted("astra_collaboration")},
                {"provider": "claude_code", "model": "claude-fable-5-1", "enabled": true,
                 "description": persisted("fable_5_1_collaboration")}
            ],
            "child_models": [
                {"provider": "codex", "model": "gpt-6.1-sol", "enabled": true,
                 "description": persisted("sol_6_1_execution")},
                {"provider": "claude_code", "model": "claude-opus-5-5", "enabled": true,
                 "description": persisted("opus_5_5_execution")}
            ]
        });
        let mut settings = Settings {
            orchestrate: orchestrate(bundled_copies),
            ..Settings::default()
        };
        assert!(settings.orchestrate.is_default());
        assert!(serde_json::to_value(&settings).unwrap()["orchestrate"].is_null());

        let mut children = settings.orchestrate.child_models.clone();
        children[1].enabled = false;
        children[1].description = format!("{}\n", children[1].guidance(false));
        children.push(OrchestrateChildModel {
            provider: ProviderKind::Codex,
            model: "gpt-6-astra".into(),
            profile_id: None,
            enabled: true,
            fast: false,
            description: String::new(),
            bundled: None,
        });
        settings
            .apply(SettingsPatch::OrchestrateChildModels(children))
            .unwrap();
        let mut decisions = settings.orchestrate.decision_models.clone();
        decisions[0].description = "Mine.".into();
        settings
            .apply(SettingsPatch::OrchestrateDecisionModels(decisions))
            .unwrap();
        let saved = serde_json::to_value(&settings.orchestrate).unwrap();
        assert_eq!(
            saved["decision_models"],
            serde_json::json!([
                {"provider": "codex", "model": "gpt-6-astra", "enabled": true, "description": "Mine."},
                {"provider": "claude_code", "model": "claude-fable-5-1", "enabled": true, "bundled": "fable"}
            ])
        );
        assert_eq!(
            saved["child_models"],
            serde_json::json!([
                {"provider": "codex", "model": "gpt-6.1-sol", "enabled": true, "bundled": "sol"},
                {"provider": "claude_code", "model": "claude-opus-5-5", "enabled": false, "bundled": "opus"},
                {"provider": "codex", "model": "gpt-6-astra", "enabled": true, "bundled": "astra-executor"}
            ])
        );
        assert_eq!(
            settings.orchestrate.child_models[2].guidance(false),
            persisted("astra_execution")
        );
        assert_eq!(orchestrate(saved), settings.orchestrate);
    }

    #[test]
    fn orchestrate_tracking_rows_follow_their_bundled_profile() {
        let loaded = orchestrate(serde_json::json!({
            "decision_models": [],
            "child_models": [
                {"provider": "codex", "model": "gpt-6-sol", "enabled": false, "fast": true, "bundled": "sol"},
                {"provider": "codex", "model": "gpt-9-sol", "bundled": "retired"}
            ]
        }));
        assert_eq!(
            loaded.child_models,
            [
                OrchestrateChildModel {
                    enabled: false,
                    fast: true,
                    ..OrchestrateSettings::default().child_models[0].clone()
                },
                OrchestrateChildModel {
                    provider: ProviderKind::Codex,
                    model: "gpt-9-sol".into(),
                    profile_id: None,
                    enabled: true,
                    fast: false,
                    description: String::new(),
                    bundled: None,
                }
            ]
        );
    }

    #[test]
    fn orchestrate_recognizes_every_persisted_bundled_text() {
        let defaults = OrchestrateSettings::default();
        let sol = || vec![defaults.child_models[0].clone()];
        let opus = || vec![defaults.child_models[1].clone()];
        for (model, text, expected) in [
            ("gpt-5.6-sol", "sol_5_6_execution", sol()),
            ("gpt-6-sol", "sol_6_execution", sol()),
            ("gpt-6.1-sol", "sol_6_1_execution", sol()),
            ("gpt-6-astra", "astra_low_execution", sol()),
            ("gpt-6-astra", "astra_execution", sol()),
            ("claude-opus-5", "opus_5_execution", opus()),
            ("claude-opus-5", "opus_across_providers_execution", opus()),
            ("claude-opus-5", "opus_5_5_execution", opus()),
            ("claude-opus-5-5", "opus_across_providers_execution", opus()),
            ("claude-opus-5-5", "opus_5_5_execution", opus()),
        ] {
            let provider = if model.starts_with("gpt") {
                "codex"
            } else {
                "claude_code"
            };
            let loaded = orchestrate(serde_json::json!({
                "decision_models": [],
                "child_models": [{"provider": provider, "model": model, "description": persisted(text)}]
            }));
            assert_eq!(loaded.child_models, expected, "{model}: {text}");
        }
        for (provider, model, text, index) in [
            ("codex", "gpt-6-astra", "astra_collaboration", 0),
            (
                "claude_code",
                "claude-fable-5-1",
                "fable_5_1_collaboration",
                1,
            ),
        ] {
            let loaded = orchestrate(serde_json::json!({
                "decision_models": [{"provider": provider, "model": model, "description": persisted(text)}],
                "child_models": []
            }));
            assert_eq!(
                loaded.decision_models,
                [defaults.decision_models[index].clone()],
                "{text}"
            );
        }
        // (collaboration, index of the bundled default row it becomes)
        for (provider, model, text, expected) in [
            ("codex", "gpt-5.6-sol", "tier_gpt_medium", Some((false, 0))),
            ("codex", "gpt-5.6-sol", "tier_gpt_default", Some((false, 0))),
            ("codex", "gpt-5.6-sol", "tier_gpt_max", Some((false, 0))),
            (
                "codex",
                "gpt-5.6-sol",
                "tier_top_judgment",
                Some((false, 0)),
            ),
            (
                "claude_code",
                "claude-opus-4-8",
                "tier_opus",
                Some((false, 1)),
            ),
            ("codex", "gpt-6-astra", "tier_astra", Some((true, 0))),
            (
                "claude_code",
                "claude-fable-5",
                "tier_fable",
                Some((true, 1)),
            ),
            ("claude_code", "claude-sonnet-5", "tier_sonnet", None),
        ] {
            let loaded = orchestrate(serde_json::json!({
                "child_models": [{"provider": provider, "model": model, "effort": "high",
                                  "enabled": false, "description": persisted(text)}]
            }));
            let mut decisions = defaults.decision_models.clone();
            let mut children = Vec::new();
            match expected {
                Some((true, index)) => decisions[index].enabled = false,
                Some((false, index)) => children.push(OrchestrateChildModel {
                    enabled: false,
                    ..defaults.child_models[index].clone()
                }),
                None => {}
            }
            assert_eq!(
                (loaded.decision_models, loaded.child_models),
                (decisions, children),
                "{text}"
            );
        }
    }

    #[test]
    fn orchestrate_keeps_customised_opus_5_rows() {
        let old_json = r#"{
            "decision_models": [],
            "child_models": [
                {"provider": "claude_code", "model": "claude-opus-5", "profile_id": "corp", "description": "Execution model for agentic coding, cross-file implementation, refactoring, debugging, and review across providers, including user-facing behavior and API or UI details. Use medium for clear bounded work, high for substantial implementation, and xhigh or max when difficult reasoning justifies the extra work; low can suit small mechanical tasks. Match verification to the changed behavior and avoid repetitive self-checking. Report evidence and unresolved limitations concisely."},
                {"provider": "claude_code", "model": "claude-opus-5", "description": "Reviewer only."}
            ]
        }"#;
        let migrated: OrchestrateSettings = serde_json::from_str(old_json).unwrap();
        assert_eq!(
            migrated.child_models.len(),
            1,
            "same model merges into one row"
        );
        let opus = &migrated.child_models[0];
        assert_eq!(opus.model, "claude-opus-5");
        assert_eq!(opus.profile_id.as_deref(), Some("corp"));
        assert!(opus.description.contains("Reviewer only."));

        // An untouched Opus 5 row merges into the Opus 5.5 row the user already added.
        let migrated = orchestrate(serde_json::json!({
            "decision_models": [],
            "child_models": [
                {"provider": "claude_code", "model": "claude-opus-5-5", "description": "Mine."},
                {"provider": "claude_code", "model": "claude-opus-5", "description": persisted("opus_5_execution")}
            ]
        }));
        let models: Vec<_> = migrated
            .child_models
            .iter()
            .map(|entry| (entry.model.as_str(), entry.description.as_str()))
            .collect();
        assert_eq!(models, [("claude-opus-5-5", "Mine.")]);
    }

    #[test]
    fn orchestrate_moves_untouched_codex_executors_to_sol_6_1() {
        let sol = OrchestrateSettings::default().child_models[0].clone();
        for (model, text, endpoint_guidance) in [
            ("gpt-5.6-sol", "sol_5_6_execution", "sol_5_6_execution"),
            ("gpt-6-astra", "astra_low_execution", "astra_execution"),
            ("gpt-6-astra", "astra_execution", "astra_execution"),
            ("gpt-6-sol", "sol_6_execution", "sol_6_execution"),
        ] {
            let text = persisted(text);
            for (enabled, fast) in [(true, false), (false, false), (true, true)] {
                let migrated = orchestrate(serde_json::json!({
                    "decision_models": [],
                    "child_models": [{
                        "provider": "codex", "model": model, "description": text,
                        "enabled": enabled, "fast": fast
                    }]
                }));
                assert_eq!(
                    migrated.child_models,
                    [OrchestrateChildModel {
                        enabled,
                        fast,
                        ..sol.clone()
                    }],
                    "{model}: enabled={enabled}, fast={fast}"
                );
            }
            let kept = orchestrate(serde_json::json!({
                "decision_models": [],
                "child_models": [{"provider": "codex", "model": model, "description": "custom"}]
            }));
            assert_eq!(
                (
                    kept.child_models[0].model.as_str(),
                    kept.child_models[0].description.as_str()
                ),
                (model, "custom")
            );
            let endpoint = orchestrate(serde_json::json!({
                "decision_models": [],
                "child_models": [{
                    "provider": "codex", "model": model, "description": text,
                    "profile_id": "custom"
                }]
            }));
            let row = &endpoint.child_models[0];
            assert_eq!(
                (
                    row.model.as_str(),
                    row.profile_id.as_deref(),
                    row.guidance(false)
                ),
                (model, Some("custom"), persisted(endpoint_guidance).as_str()),
                "{model}: an endpoint profile keeps its model"
            );
            let migrated = orchestrate(serde_json::json!({
                "decision_models": [],
                "child_models": [{
                    "provider": "codex", "model": model, "description": text
                }, {
                    "provider": "codex", "model": "gpt-6.1-sol", "profile_id": "custom-codex",
                    "enabled": false, "fast": true, "description": "User execution guidance"
                }]
            }));
            assert_eq!(
                migrated.child_models,
                [OrchestrateChildModel {
                    provider: ProviderKind::Codex,
                    model: "gpt-6.1-sol".into(),
                    profile_id: Some("custom-codex".into()),
                    enabled: false,
                    fast: true,
                    description: "User execution guidance".into(),
                    bundled: None,
                }],
                "{model}: existing destination stays unchanged"
            );
        }
    }

    #[test]
    fn orchestrate_merges_legacy_tiers_and_upgrades_bundled_models() {
        let settings = orchestrate(serde_json::json!({
            "child_models": [
                {"provider":"codex", "model":"gpt-5.6-sol", "default_effort":"medium", "description":"Routine work", "enabled":false, "profile_id":"custom", "fast":true},
                {"provider":"codex", "model":"gpt-5.6-sol", "effort":"max", "description":"Difficult bugs"},
                {"provider":"claude_code", "model":"claude-sonnet-5", "effort":"high", "description":persisted("tier_sonnet")},
                {"provider":"claude_code", "model":"claude-opus-4-8", "effort":"high", "description":persisted("tier_opus")},
                {"provider":"claude_code", "model":"claude-fable-5", "effort":"high", "description":persisted("tier_fable")}
            ]
        }));
        let defaults = OrchestrateSettings::default();
        assert_eq!(settings.child_models.len(), 2);
        let sol = &settings.child_models[0];
        assert!(sol.description.contains("medium effort: Routine work"));
        assert!(sol.description.contains("max effort: Difficult bugs"));
        assert!(!sol.enabled);
        assert!(sol.fast);
        assert_eq!(sol.profile_id.as_deref(), Some("custom"));
        assert_eq!(settings.child_models[1], defaults.child_models[1]);
        assert_eq!(settings.decision_models, defaults.decision_models);
        let serialized = serde_json::to_string(&settings).unwrap();
        assert!(!serialized.contains("\"effort\""));
        assert_eq!(
            serde_json::from_str::<OrchestrateSettings>(&serialized).unwrap(),
            settings
        );
    }

    #[test]
    fn orchestrate_capabilities_use_catalog_and_cap_collaboration() {
        let catalog = vec![ModelSpec {
            id: "custom".into(),
            display_name: "Custom".into(),
            is_default: false,
            options: vec![OptionDescriptor::Select {
                role: Default::default(),
                apply: Default::default(),
                recommended: None,
                permissive: None,
                id: "reasoningEffort".into(),
                label: "Effort".into(),
                default_value: None,
                options: ["medium", "high", "deep"]
                    .into_iter()
                    .map(|value| agent::SelectOption {
                        unavailable: None,
                        value: value.into(),
                        label: value.into(),
                        description: None,
                    })
                    .collect(),
            }],
        }];
        assert_eq!(
            orchestrate_efforts(ProviderKind::Codex, "custom", &catalog, false),
            ["medium", "high", "deep"]
        );
        assert_eq!(
            orchestrate_efforts(ProviderKind::Codex, "custom", &catalog, true),
            ["medium", "high"]
        );
        assert!(orchestrate_efforts(ProviderKind::Codex, "unknown", &catalog, false).is_empty());
        for entry in OrchestrateSettings::default().decision_models {
            assert_eq!(
                orchestrate_efforts(entry.provider, &entry.model, &[], true),
                ["medium", "high"]
            );
        }
    }

    #[test]
    fn orchestrate_settings_patches_deduplicate_within_each_role() {
        let mut settings = Settings::default();
        let peer = settings.orchestrate.decision_models[0].clone();
        // The same model serves both roles with role-specific guidance.
        let executor = OrchestrateChildModel {
            description: "Executor notes".into(),
            bundled: None,
            ..peer.clone()
        };
        settings.orchestrate.child_models[0] = executor.clone();

        let mut duplicate = executor.clone();
        duplicate.profile_id = Some("another-endpoint".into());
        duplicate.description = "must not overwrite".into();
        let mut children = settings.orchestrate.child_models.clone();
        children.push(duplicate);
        settings
            .apply(SettingsPatch::OrchestrateChildModels(children))
            .unwrap();
        assert_eq!(settings.orchestrate.child_models.len(), 2);
        assert_eq!(settings.orchestrate.child_models[0], executor);

        let mut duplicate = peer.clone();
        duplicate.profile_id = Some("another-endpoint".into());
        duplicate.description = "must not overwrite".into();
        let mut decisions = settings.orchestrate.decision_models.clone();
        decisions.push(duplicate);
        settings
            .apply(SettingsPatch::OrchestrateDecisionModels(decisions))
            .unwrap();
        assert_eq!(settings.orchestrate.decision_models.len(), 2);
        assert_eq!(settings.orchestrate.decision_models[0], peer);
        assert_eq!(settings.orchestrate.child_models[0], executor);
    }

    #[test]
    fn resolves_builtin_and_user_profiles() {
        let mut settings = Settings::default();
        // Give the built-in Claude card a base URL and a display name.
        settings.provider_mut(ProviderKind::ClaudeCode).display_name = Some("Claude".into());

        // A user profile driving the same protocol (third-party Claude).
        let id = settings.allocate_profile_id("Klaude Kode");
        assert_eq!(id, "klaude-kode");
        settings.profiles.insert(
            id.clone(),
            ProviderProfile {
                kind: ProviderKind::ClaudeCode,
                settings: ProviderSettings {
                    display_name: Some("Klaude Kode".into()),
                    env: vec![EnvVar {
                        name: "ANTHROPIC_BASE_URL".into(),
                        value: "https://api.kimi.com/coding/".into(),
                        sensitive: false,
                    }],
                    ..ProviderSettings::default()
                },
            },
        );

        // Built-in id resolves to the provider card.
        let builtin = settings.resolved_profile("claude").unwrap();
        assert_eq!(builtin.kind, ProviderKind::ClaudeCode);
        assert!(Settings::is_builtin_profile_id("claude"));

        // User id resolves to its profile, tagged with the shared protocol.
        let custom = settings.resolved_profile(&id).unwrap();
        assert_eq!(custom.kind, ProviderKind::ClaudeCode);
        assert_eq!(custom.settings.env[0].value, "https://api.kimi.com/coding/");
        assert!(!Settings::is_builtin_profile_id(&id));

        // Both Claude profiles are offered for the kind, built-in first.
        let claude_profiles = settings.profiles_for_kind(ProviderKind::ClaudeCode);
        assert_eq!(claude_profiles.len(), 2);
        assert_eq!(claude_profiles[0].id, "claude");
        assert_eq!(claude_profiles[1].id, id);
        // Every other native provider still resolves to exactly its built-in.
        assert_eq!(settings.profiles_for_kind(ProviderKind::Codex).len(), 1);
        assert_eq!(settings.profiles_for_kind(ProviderKind::Pi).len(), 1);
        assert_eq!(settings.profiles_for_kind(ProviderKind::OpenCode).len(), 1);
        assert_eq!(
            settings.resolved_profile("pi").unwrap().kind,
            ProviderKind::Pi
        );
        assert_eq!(
            settings.resolved_profile("opencode").unwrap().kind,
            ProviderKind::OpenCode
        );

        // Display names: built-in falls back to label; user shows its name.
        assert_eq!(settings.profile_display_name("claude"), "Claude");
        assert_eq!(settings.profile_display_name(&id), "Klaude Kode");
        assert_eq!(settings.resolved_profile("nope"), None);

        // A second profile of the same name gets a distinct id.
        assert_eq!(settings.allocate_profile_id("Klaude Kode"), "klaude-kode-2");
    }

    #[test]
    fn a_user_profile_named_after_a_new_native_provider_keeps_its_identity() {
        // Saved before Cursor was native: a Claude profile the user named "Cursor".
        let settings: Settings = serde_json::from_str(
            r##"{"profiles":{"cursor":{"kind":"claude_code","display_name":"Cursor","accent_color":"#123456"}}}"##,
        )
        .unwrap();
        assert_eq!(
            settings.resolved_profile("cursor").unwrap().kind,
            ProviderKind::ClaudeCode
        );
        let [builtin] = settings
            .profiles_for_kind(ProviderKind::Cursor)
            .try_into()
            .unwrap();
        assert_eq!(builtin.kind, ProviderKind::Cursor);
        assert_eq!(
            settings.resolved_profile(&builtin.id).unwrap().kind,
            ProviderKind::Cursor
        );
        assert!(!Settings::is_builtin_profile_id(
            &settings.allocate_profile_id(&builtin.id)
        ));
        let thread = crate::project::SessionMeta::new(ProviderKind::Cursor, "/x".into(), None);
        assert_ne!(
            settings.provider_color(&thread.provider_color_key()),
            0x123456
        );
    }

    /// A build that predates a field must not destroy it: unknown keys survive a
    /// load → save round trip. (We hit this for real: an older binary dropped
    /// `acp_agents` and the next save wiped the installed agents.)
    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let json = r#"{
            "theme_mode": "dark",
            "a_future_field": {"nested": [1, 2, 3]},
            "diff_view_mode": "line",
            "another": "value"
        }"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.theme_mode, ThemeMode::Dark);

        let written = serde_json::to_string(&settings).unwrap();
        let back: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            back.get("a_future_field"),
            Some(&serde_json::json!({"nested": [1, 2, 3]})),
            "an unknown field was dropped on save"
        );
        assert_eq!(back.get("another"), Some(&serde_json::json!("value")));
        assert_eq!(back.get("diff_view_mode"), Some(&serde_json::json!("line")));
    }

    /// A settings file written by the HTTP-listener builds names a port and no
    /// Traverse mode. It must load as hosting on the official service with the
    /// name kept, and the retired port must not be written back.
    #[test]
    fn hosting_settings_from_the_http_listener_builds_migrate_to_official_traverse() {
        let mut legacy: Settings = serde_json::from_str(
            r#"{"remote_hosting_enabled":true,"remote_port":47421,"remote_host_name":"Studio"}"#,
        )
        .unwrap();
        legacy.migrate_legacy();
        assert!(legacy.remote_hosting_enabled);
        assert_eq!(
            serde_json::to_value(&legacy.traverse).unwrap(),
            serde_json::json!({"sources": [{"kind": "official", "enabled": true}]})
        );
        assert_eq!(legacy.remote_host_name.as_deref(), Some("Studio"));
        let written: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&legacy).unwrap()).unwrap();
        assert_eq!(written.get("remote_port"), None);
        assert_eq!(written.get("traverse"), None, "the default is not written");
    }

    /// Every Traverse setting a settings file has held loads as the source
    /// list it meant: the single choice of earlier builds keeps its
    /// either-or meaning, and a hand-edited list gets the official source
    /// first exactly once. What is written back is the list.
    #[test]
    fn traverse_settings_load_as_a_source_list_with_the_official_source_first() {
        let official = |enabled| serde_json::json!({"kind": "official", "enabled": enabled});
        let custom = |url: &str, enabled| serde_json::json!({"kind": "custom", "url": url, "enabled": enabled});
        for (file, expected) in [
            (r#"{}"#, vec![official(true)]),
            (r#"{"traverse":{"mode":"official"}}"#, vec![official(true)]),
            (r#"{"traverse":{"mode":"off"}}"#, vec![official(false)]),
            (
                r#"{"traverse":{"mode":"custom","url":"https://traverse.example/"}}"#,
                vec![official(false), custom("https://traverse.example/", true)],
            ),
            (
                r#"{"traverse":{"sources":[{"kind":"official","enabled":true},{"kind":"custom","url":"https://a.example/","enabled":true},{"kind":"custom","url":"https://b.example/","enabled":false}]}}"#,
                vec![
                    official(true),
                    custom("https://a.example/", true),
                    custom("https://b.example/", false),
                ],
            ),
            (
                r#"{"traverse":{"sources":[{"kind":"custom","url":"https://a.example/","enabled":true},{"kind":"official","enabled":true},{"kind":"official","enabled":false},{"kind":"custom","url":"https://a.example/","enabled":false}]}}"#,
                vec![official(true), custom("https://a.example/", true)],
            ),
            (
                r#"{"traverse":{"sources":[{"kind":"custom","url":"https://a.example/","enabled":true}]}}"#,
                vec![official(false), custom("https://a.example/", true)],
            ),
        ] {
            let settings: Settings = serde_json::from_str(file).unwrap();
            assert_eq!(
                serde_json::to_value(&settings.traverse).unwrap(),
                serde_json::json!({ "sources": expected }),
                "{file}"
            );
        }

        let mut patched = Settings::default();
        let off: TraverseSetting = serde_json::from_str(r#"{"mode":"off"}"#).unwrap();
        patched.apply(SettingsPatch::Traverse(off)).unwrap();
        assert_eq!(
            serde_json::to_value(&patched).unwrap().get("traverse"),
            Some(&serde_json::json!({"sources": [official(false)]}))
        );
        assert_eq!(patched.traverse.enabled().count(), 0);
    }
}

#[cfg(test)]
mod account_usage_tests {
    use super::*;

    #[test]
    fn resolved_custom_endpoint_is_not_a_native_account() {
        let settings: Settings = serde_json::from_str(r#"{"profiles":{"custom":{"kind":"claude_code","display_name":"Kimi","env":[{"name":"ANTHROPIC_BASE_URL","value":"https://api.example.com/anthropic"},{"name":"ANTHROPIC_API_KEY","sensitive":true}]}}}"#).unwrap();
        let mut custom = settings.resolved_profile("custom").unwrap();
        assert!(
            !custom.supports_account_usage(),
            "custom protocol compatibility does not imply native subscription support"
        );
        custom.settings.display_name = Some("Claude".into());
        assert!(!custom.supports_account_usage());
        custom.settings.env.clear();
        assert!(
            custom.supports_account_usage(),
            "a custom profile using native account auth remains eligible, even signed out"
        );
    }
}
