//! Provider-agnostic agent session layer.
//!
//! Each native provider module spawns its CLI as a child process,
//! speaks the provider-native transport, and normalizes everything
//! into the canonical [`AgentEvent`] stream. The UI only ever sees this module's
//! types; nothing provider-shaped leaks past this crate except [`ResumeCursor`],
//! which is intentionally opaque.

#[cfg(feature = "process")]
pub mod acp;
#[cfg(feature = "process")]
mod acp_session;
#[cfg(feature = "process")]
mod actor;
#[cfg(feature = "process")]
pub mod claude;
mod claude_context;
#[cfg(feature = "process")]
mod claude_goal;
mod claude_manifest;
#[cfg(feature = "process")]
mod claude_plugins;
pub mod context_window;
mod image_reads;
pub use image_reads::tool_result_images;
#[cfg(not(feature = "process"))]
pub mod claude {
    pub use crate::claude_context::*;
}
#[cfg(feature = "process")]
pub mod codex;
#[cfg(feature = "process")]
pub mod cursor;
#[cfg(feature = "process")]
pub mod grok;
#[cfg(feature = "process")]
#[cfg(feature = "process")]
pub mod pi;
#[cfg(feature = "process")]
mod process;
#[cfg(feature="process")]
pub use process::{
    configure_host_environment, configure_supervisor as configure_process_supervisor,
    supervised_command,
};
#[cfg(feature = "process")]
pub use process::keep_own_stdio_from_children;
#[cfg(feature = "process")]
mod subagent_tail;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(feature = "process")]
use smol::channel::{Receiver, Sender};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Codex,
    ClaudeCode,
    Pi,
    OpenCode,
    Cursor,
    Grok,
    /// Any agent speaking the Agent Client Protocol. Which one is carried
    /// separately ([`SessionOptions::acp`]) so this stays `Copy`: the native
    /// providers keep their own clients, and ACP covers the rest of the
    /// ecosystem.
    Acp,
}

/// Work a harness runs in the background of a thread, as it names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: BackgroundKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub started_reported: bool,
}

/// What a piece of background work is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundKind {
    Command,
    Agent,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub supports_steering: bool,
    pub supports_fork: bool,
    pub native_rewind: bool,
    /// The harness compacts its context on request ([`SessionCommand::Compact`]).
    pub manual_compaction: bool,
    /// How the harness's standing goal is driven ([`SessionCommand::SetGoal`]),
    /// when it has one.
    pub goals: Option<GoalControl>,
    pub per_turn_effort: bool,
    pub options_apply_live: bool,
    pub live_option_push: LiveOptionPush,
    /// Whether this adapter can attach tcode's HTTP MCP servers to a session.
    /// Preview attachment rides the same capability as every other MCP server.
    pub mcp_servers: bool,
    /// The harness drives the desktop itself, so tcode's computer-use MCP
    /// server is not attached to its sessions.
    pub native_computer_use: bool,
    pub launch_args: bool,
    pub option_descriptors: OptionDescriptors,
    pub home_path: bool,
    pub trust_project_extensions: bool,
    pub plugin_management: PluginManagement,
    /// Where [`SessionOptions::instructions`] reach the model.
    pub instruction_channel: InstructionChannel,
}

/// What a harness lets a client do with its standing goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalControl {
    /// The goal can be paused and resumed without clearing it.
    pub pause: bool,
    /// A change applies out of band, even while a turn runs. Otherwise the
    /// change is a turn of its own, as Claude Code's `/goal` slash command is.
    pub live: bool,
}

/// The provider's own channel for instructions a host gives the model alongside the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionChannel {
    /// Appended to the system prompt once per process, on creation and resume alike.
    SessionAppend,
    /// Developer context Codex re-sends when it changes; restored after a compaction.
    DeveloperContext,
    /// The system field of every prompt.
    PerPromptSystem,
    /// A content block of its own after the user's text on every prompt that is not a native
    /// `/command`, while the session attached Tcode's MCP servers.
    PerPromptUserBlock,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveOptionPush {
    All,
    Only(&'static [&'static str]),
    None,
}

impl LiveOptionPush {
    pub fn supports(self, option_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(option_ids) => option_ids.contains(&option_id),
            Self::None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionDescriptors {
    Wire,
    Catalog,
}

impl ProviderKind {
    /// The natively maintained providers, in the order provider lists present
    /// them.
    pub const NATIVE: [ProviderKind; 6] = [
        ProviderKind::ClaudeCode,
        ProviderKind::Codex,
        ProviderKind::Pi,
        ProviderKind::OpenCode,
        ProviderKind::Cursor,
        ProviderKind::Grok,
    ];

    /// Provider behavior consumed by runtime and UI policy.
    ///
    /// This is deliberately one exhaustive table so adding a provider forces a
    /// review of every capability fact in one place.
    pub fn caps(&self) -> Caps {
        match self {
            ProviderKind::ClaudeCode => Caps {
                supports_steering: true,
                supports_fork: true,
                native_rewind: true,
                manual_compaction: true,
                goals: Some(GoalControl {
                    pause: false,
                    live: false,
                }),
                per_turn_effort: false,
                options_apply_live: false,
                live_option_push: LiveOptionPush::None,
                mcp_servers: true,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Catalog,
                home_path: true,
                trust_project_extensions: false,
                plugin_management: PluginManagement {
                    actions: &[
                        PluginActionKind::Install,
                        PluginActionKind::Uninstall,
                        PluginActionKind::Enable,
                        PluginActionKind::Disable,
                        PluginActionKind::Update,
                    ],
                    marketplaces: true,
                },
                instruction_channel: InstructionChannel::SessionAppend,
            },
            ProviderKind::Codex => Caps {
                supports_steering: true,
                supports_fork: true,
                native_rewind: false,
                manual_compaction: true,
                goals: Some(GoalControl {
                    pause: true,
                    live: true,
                }),
                per_turn_effort: true,
                options_apply_live: false,
                live_option_push: LiveOptionPush::None,
                mcp_servers: true,
                // `codex features list` reports `computer_use stable` (0.159.3).
                native_computer_use: true,
                launch_args: false,
                option_descriptors: OptionDescriptors::Catalog,
                home_path: true,
                trust_project_extensions: false,
                plugin_management: PluginManagement {
                    actions: &[
                        PluginActionKind::Install,
                        PluginActionKind::Uninstall,
                        PluginActionKind::Enable,
                        PluginActionKind::Disable,
                    ],
                    marketplaces: true,
                },
                instruction_channel: InstructionChannel::DeveloperContext,
            },
            ProviderKind::Acp => Caps {
                supports_steering: false,
                supports_fork: false,
                native_rewind: false,
                manual_compaction: false,
                goals: None,
                per_turn_effort: false,
                options_apply_live: true,
                live_option_push: LiveOptionPush::All,
                // ACP support is negotiated per agent; capable agents receive
                // every tcode HTTP MCP registration in session/new or load.
                mcp_servers: true,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Wire,
                home_path: true,
                trust_project_extensions: false,
                plugin_management: PluginManagement::NONE,
                instruction_channel: InstructionChannel::PerPromptUserBlock,
            },
            ProviderKind::Pi => Caps {
                supports_steering: true,
                supports_fork: false,
                native_rewind: false,
                manual_compaction: true,
                goals: None,
                per_turn_effort: false,
                options_apply_live: false,
                live_option_push: LiveOptionPush::Only(&["reasoningEffort"]),
                mcp_servers: false,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Catalog,
                home_path: true,
                trust_project_extensions: true,
                plugin_management: PluginManagement::NONE,
                instruction_channel: InstructionChannel::None,
            },
            ProviderKind::OpenCode => Caps {
                supports_steering: false,
                supports_fork: false,
                native_rewind: false,
                manual_compaction: false,
                goals: None,
                per_turn_effort: true,
                options_apply_live: false,
                live_option_push: LiveOptionPush::None,
                mcp_servers: true,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Catalog,
                home_path: false,
                trust_project_extensions: false,
                plugin_management: PluginManagement::NONE,
                instruction_channel: InstructionChannel::PerPromptSystem,
            },
            ProviderKind::Cursor => Caps {
                supports_steering: false,
                supports_fork: false,
                native_rewind: false,
                manual_compaction: false,
                goals: None,
                per_turn_effort: false,
                options_apply_live: true,
                live_option_push: LiveOptionPush::All,
                mcp_servers: true,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Wire,
                home_path: true,
                trust_project_extensions: false,
                plugin_management: PluginManagement::NONE,
                instruction_channel: InstructionChannel::PerPromptUserBlock,
            },
            ProviderKind::Grok => Caps {
                supports_steering: true,
                supports_fork: true,
                native_rewind: false,
                manual_compaction: false,
                goals: None,
                per_turn_effort: false,
                options_apply_live: true,
                live_option_push: LiveOptionPush::All,
                mcp_servers: true,
                native_computer_use: false,
                launch_args: true,
                option_descriptors: OptionDescriptors::Wire,
                home_path: true,
                trust_project_extensions: false,
                plugin_management: PluginManagement {
                    actions: &[
                        PluginActionKind::Install,
                        PluginActionKind::Uninstall,
                        PluginActionKind::Update,
                    ],
                    marketplaces: true,
                },
                instruction_channel: InstructionChannel::PerPromptUserBlock,
            },
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            ProviderKind::Codex => "Codex",
            ProviderKind::ClaudeCode => "Claude Code",
            ProviderKind::Pi => "pi",
            ProviderKind::OpenCode => "OpenCode",
            ProviderKind::Cursor => "Cursor",
            ProviderKind::Grok => "Grok",
            ProviderKind::Acp => "ACP agent",
        }
    }
}

/// Which native plugin operations an adapter drives. Whether one is offered
/// for a given entry is computed per catalog ([`ProviderPluginEntry::actions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginManagement {
    pub actions: &'static [PluginActionKind],
    pub marketplaces: bool,
}

impl PluginManagement {
    pub const NONE: Self = Self {
        actions: &[],
        marketplaces: false,
    };

    pub fn supports(&self, action: PluginActionKind) -> bool {
        self.actions.contains(&action)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginActionKind {
    Install,
    Uninstall,
    Enable,
    Disable,
    Update,
}

/// A native settings scope a plugin is installed or enabled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginScope {
    User,
    Project,
    Local,
    Managed,
    Session,
}

/// A yes/no fact the native CLI may not report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tri {
    Yes,
    No,
    #[default]
    Unknown,
}

impl From<bool> for Tri {
    fn from(value: bool) -> Self {
        if value { Tri::Yes } else { Tri::No }
    }
}

/// One native operation the host computed as available for an entry in the
/// catalog's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginAction {
    Install { scope: PluginScope },
    Uninstall { scope: PluginScope },
    Enable { scope: PluginScope },
    Disable { scope: PluginScope },
    Update { scope: PluginScope },
}

impl PluginAction {
    pub fn kind(self) -> PluginActionKind {
        match self {
            Self::Install { .. } => PluginActionKind::Install,
            Self::Uninstall { .. } => PluginActionKind::Uninstall,
            Self::Enable { .. } => PluginActionKind::Enable,
            Self::Disable { .. } => PluginActionKind::Disable,
            Self::Update { .. } => PluginActionKind::Update,
        }
    }

    pub fn scope(self) -> PluginScope {
        match self {
            Self::Install { scope }
            | Self::Uninstall { scope }
            | Self::Enable { scope }
            | Self::Disable { scope }
            | Self::Update { scope } => scope,
        }
    }
}

/// A marketplace-level native operation available in the catalog's context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MarketplaceAction {
    Add,
    /// Removing the marketplace also uninstalls these installed plugins in
    /// every scope, so the host asks before running it.
    Remove {
        marketplace: String,
        uninstalls: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSourceKind {
    Official,
    ThirdParty,
    LocalPath,
    Git,
    Npm,
    Command,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSource {
    #[serde(default)]
    pub marketplace: Option<String>,
    pub kind: PluginSourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstallation {
    pub scope: PluginScope,
    /// Where the installed copy lives on the host.
    #[serde(default)]
    pub location: Option<PathBuf>,
    #[serde(default)]
    pub version: Option<String>,
    /// The scope's own enablement where the native CLI reports it; the
    /// effective value is [`ProviderPluginEntry::enabled`].
    #[serde(default)]
    pub scope_enabled: Option<bool>,
}

/// Components a plugin declares, by name. A `None` field was not enumerated,
/// which is not the same as declaring none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredComponents {
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    #[serde(default)]
    pub agents: Option<Vec<String>>,
    /// Hook event names.
    #[serde(default)]
    pub hooks: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_servers: Option<Vec<String>>,
    /// Hook event names the native CLI reports as not yet trusted (Codex
    /// skips such hooks until they are reviewed natively). `None` when the
    /// provider has no hook-trust state.
    #[serde(default)]
    pub untrusted_hooks: Option<Vec<String>>,
    #[serde(default)]
    pub lsp_servers: Option<Vec<String>>,
    #[serde(default)]
    pub apps: Option<Vec<String>>,
}

/// One plugin in a provider's native catalog: installed in any scope, offered
/// by a marketplace, or both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPluginEntry {
    /// The native id the CLI accepts, `name@marketplace`.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub source: PluginSource,
    #[serde(default)]
    pub installations: Vec<PluginInstallation>,
    /// Effective enablement for the catalog's context directory.
    #[serde(default)]
    pub enabled: Tri,
    #[serde(default)]
    pub declared: Option<DeclaredComponents>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub actions: Vec<PluginAction>,
    /// Read-only `(key, native text)` facts, keyed by stable identifiers the
    /// client localizes.
    #[serde(default)]
    pub diagnostics: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPluginMarketplace {
    pub name: String,
    /// The repository, URL or path the marketplace was added from.
    pub source: String,
    pub kind: PluginSourceKind,
    #[serde(default)]
    pub location: Option<PathBuf>,
}

/// A provider's native plugin catalog as seen from one directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginListing {
    pub entries: Vec<ProviderPluginEntry>,
    pub marketplaces: Vec<ProviderPluginMarketplace>,
    pub marketplace_actions: Vec<MarketplaceAction>,
    /// Native errors for parts of the catalog that could not be read, such
    /// as a marketplace that failed to load.
    pub errors: Vec<String>,
}

/// Where and as whom a plugin-management command runs: the profile's CLI and
/// environment, and the directory whose project scopes it addresses.
#[derive(Debug, Clone)]
pub struct PluginContext {
    pub binary_path: Option<PathBuf>,
    pub launch_env: LaunchEnv,
    pub cwd: PathBuf,
    /// False when `cwd` is only a neutral directory, so project and local
    /// scopes are not offered.
    pub project: bool,
}

/// One native plugin mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginOp {
    Install {
        id: String,
        scope: PluginScope,
        /// The `sha256` of a command the user accepted from an earlier
        /// [`PluginOpOutcome::AcceptCommand`].
        accept_command: Option<String>,
    },
    Uninstall {
        id: String,
        scope: PluginScope,
    },
    SetEnabled {
        id: String,
        scope: PluginScope,
        enabled: bool,
    },
    Update {
        id: String,
        scope: PluginScope,
        accept_command: Option<String>,
    },
    AddMarketplace {
        source: String,
    },
    RemoveMarketplace {
        name: String,
    },
}

/// What a native plugin mutation reported. Native failures are errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginOpOutcome {
    Done {
        diagnostics: Vec<(String, String)>,
    },
    /// The CLI refused to run a marketplace-declared command until a person
    /// accepts exactly this command.
    AcceptCommand(CommandAcceptance),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAcceptance {
    pub command: String,
    pub sha256: String,
    #[serde(default)]
    pub mode: Option<String>,
    /// The CLI's own explanation.
    pub native_text: String,
}

/// List a provider's native plugins and marketplaces as seen from `context`.
#[cfg(feature = "process")]
pub async fn list_plugins(
    provider: ProviderKind,
    context: &PluginContext,
) -> Result<PluginListing, AgentError> {
    match provider {
        ProviderKind::ClaudeCode => claude_plugins::list(context).await,
        ProviderKind::Codex => codex::plugins::list(context).await,
        ProviderKind::Grok => grok::plugins::list(context).await,
        ProviderKind::Pi | ProviderKind::OpenCode | ProviderKind::Cursor | ProviderKind::Acp => {
            Err(no_plugin_management(provider))
        }
    }
}

/// Run one native plugin mutation in `context`.
#[cfg(feature = "process")]
pub async fn run_plugin_op(
    provider: ProviderKind,
    context: &PluginContext,
    op: &PluginOp,
) -> Result<PluginOpOutcome, AgentError> {
    match provider {
        ProviderKind::ClaudeCode => claude_plugins::run(context, op).await,
        ProviderKind::Codex => codex::plugins::run(context, op).await,
        ProviderKind::Grok => grok::plugins::run(context, op).await,
        ProviderKind::Pi | ProviderKind::OpenCode | ProviderKind::Cursor | ProviderKind::Acp => {
            Err(no_plugin_management(provider))
        }
    }
}

/// A native failure in the CLI's own words.
#[cfg(feature = "process")]
fn native_message(error: &AgentError) -> String {
    match error {
        AgentError::Provider(message) => message.clone(),
        other => other.to_string(),
    }
}

#[cfg(feature = "process")]
fn no_plugin_management(provider: ProviderKind) -> AgentError {
    AgentError::Provider(format!(
        "{} has no plugin management",
        provider.display_name()
    ))
}

/// Registry agents never offered for new sessions because they duplicate a
/// native integration.
pub const HIDDEN_ACP_AGENT_IDS: [&str; 6] = [
    "claude-acp",
    "codex-acp",
    "pi-acp",
    "opencode",
    "cursor",
    "grok-build",
];

/// One ACP agent a session can run: its registry identity plus how to launch it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpAgent {
    /// Registry id (`"gemini"`, `"cursor"`, …), or a user-chosen id for a custom agent.
    pub id: String,
    pub name: String,
    pub launch: AcpLaunch,
}

/// How to start an ACP agent process (ACP is JSON-RPC over the child's stdio).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcpLaunch {
    /// Registry `npx` distribution: `npm exec --yes -- <package> <args…>`.
    Npx {
        package: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// Registry `binary` distribution, already downloaded and extracted.
    Binary {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// A user-defined command (the escape hatch for agents not in the registry).
    Custom {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
}

/// Provider-shaped opaque state needed to resume a session later
/// (Codex: `{"thread_id": ...}`; Claude/pi/OpenCode use their native ids).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeCursor(pub serde_json::Value);

impl ResumeCursor {
    #[cfg(feature = "process")]
    pub(crate) fn str_field(&self, keys: &[&str]) -> Option<&str> {
        keys.iter()
            .find_map(|key| self.0.get(*key).and_then(serde_json::Value::as_str))
    }
}

#[cfg(feature = "process")]
pub(crate) fn processed_tokens(usage: TokenUsage) -> u64 {
    usage.used_tokens.unwrap_or_else(|| {
        usage
            .input_tokens
            .unwrap_or(0)
            .saturating_add(usage.output_tokens.unwrap_or(0))
            .saturating_add(usage.cached_input_tokens.unwrap_or(0))
    })
}

#[cfg(feature = "process")]
pub(crate) fn start_mapped_turn(
    prefix: &str,
    counter: &mut u64,
    current: &mut Option<String>,
    usage: &mut Option<TokenUsage>,
    failed: &mut bool,
) -> Vec<AgentEvent> {
    if current.is_some() {
        return Vec::new();
    }
    *counter += 1;
    let turn_id = format!("{prefix}-turn-{counter}");
    *current = Some(turn_id.clone());
    *usage = None;
    *failed = false;
    vec![AgentEvent::TurnStarted { turn_id }]
}

#[cfg(feature = "process")]
pub(crate) fn complete_mapped_turn(
    current: &mut Option<String>,
    interrupted: &mut bool,
    failed: bool,
    usage: Option<TokenUsage>,
) -> Vec<AgentEvent> {
    let Some(turn_id) = current.take() else {
        return Vec::new();
    };
    let status = if *interrupted {
        TurnStatus::Interrupted
    } else if failed {
        TurnStatus::Failed
    } else {
        TurnStatus::Completed
    };
    *interrupted = false;
    vec![AgentEvent::TurnCompleted {
        turn_id,
        status,
        usage,
    }]
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    /// Provider-native model id; `None` = provider default.
    pub model: Option<String>,
    pub resume: Option<ResumeCursor>,
    /// Fork the resumed provider session instead of continuing it in place.
    /// This is meaningful only when `resume` is present.
    pub fork: bool,
    /// Override for the CLI binary; `None` = resolve from PATH.
    pub binary_path: Option<PathBuf>,
    /// Chosen values for the provider's [`OptionDescriptor`]s (permission,
    /// reasoning effort,
    /// context window, service tier, fast mode, thinking, …). Each provider
    /// reads the ids it understands and ignores the rest.
    pub option_selections: Vec<OptionSelection>,
    /// Enabled tcode HTTP MCP servers to register with this session.
    pub mcp_servers: Vec<McpRegistration>,
    /// Per-provider environment (Settings → Providers): extra variables merged
    /// into the child's environment, plus the home-directory override. See
    /// [`LaunchEnv`].
    pub launch_env: LaunchEnv,
    /// Extra CLI arguments appended at spawn for providers whose settings card
    /// exposes "Launch arguments".
    pub extra_args: Vec<String>,
    /// Which ACP agent to launch. Required when `provider == ProviderKind::Acp`,
    /// ignored (and `None`) for native providers.
    pub acp: Option<AcpAgent>,
    /// Tcode's instructions for the model, delivered through the provider's
    /// [`Caps::instruction_channel`]; never part of a user turn's text.
    pub instructions: Option<String>,
}

/// The per-provider environment configured in Settings → Providers, applied to
/// every child process we spawn for that provider (sessions, `list_models`, and
/// version/auth probes alike).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchEnv {
    /// Extra `KEY=VALUE` pairs merged into the child's environment. Later
    /// entries win, and these override anything inherited from the parent.
    pub env: Vec<(String, String)>,
    /// Home-directory override. Provider-specific: Claude gets `HOME`, Codex
    /// gets `CODEX_HOME`, pi gets `PI_CODING_AGENT_DIR`, Cursor gets
    /// `CURSOR_CONFIG_DIR` and `CURSOR_DATA_DIR` and Grok gets `GROK_HOME`.
    /// OpenCode has no supported single-directory override, so its value is
    /// ignored. Cursor reads its login from `$XDG_CONFIG_HOME/cursor` or
    /// `~/.config/cursor` whatever these say; moving either would also move
    /// the configuration of every tool the agent runs (`gh`, git), so Cursor
    /// profiles share one login.
    pub home: Option<PathBuf>,
}

impl LaunchEnv {
    /// The complete list of variables to set on a child of `provider`: the
    /// configured pairs, plus the provider's home variable when an override is
    /// set (appended last, so it wins over an equivalent hand-written pair).
    pub fn pairs(&self, provider: ProviderKind) -> Vec<(String, String)> {
        let mut pairs = self.env.clone();
        if let Some(home) = &self.home {
            let keys: &[&str] = match provider {
                ProviderKind::ClaudeCode => &["HOME"],
                ProviderKind::Codex => &["CODEX_HOME"],
                ProviderKind::Pi => &["PI_CODING_AGENT_DIR"],
                ProviderKind::Cursor => &["CURSOR_CONFIG_DIR", "CURSOR_DATA_DIR"],
                ProviderKind::Grok => &["GROK_HOME"],
                ProviderKind::OpenCode => &[],
                // ACP agents carry their own env in the launch recipe; there is
                // no protocol-level home concept to override.
                ProviderKind::Acp => &[],
            };
            for key in keys {
                pairs.push(((*key).to_string(), home.to_string_lossy().into_owned()));
            }
        }
        pairs
    }
}

/// Connection details for the tcode preview MCP server (a streamable-HTTP
/// endpoint on loopback, guarded by a bearer token). Provider-agnostic; each
/// provider maps it onto its native MCP-server config shape.
#[derive(Debug, Clone)]
pub struct McpRegistration {
    /// MCP server name (and tool namespace).
    pub name: String,
    /// Streamable-HTTP endpoint, e.g. `http://127.0.0.1:53211/mcp`.
    pub url: String,
    /// Bearer token presented on every request (`Authorization: Bearer <token>`).
    pub bearer_token: String,
}

impl McpRegistration {
    pub const SERVER_NAME_PREVIEW: &'static str = "tcode_preview";
    pub const SERVER_NAME_ORCHESTRATE: &'static str = "tcode_orchestrate";
    /// The child-thread half of orchestration: a single `report_result` tool.
    pub const SERVER_NAME_ORCHESTRATE_REPORT: &'static str = "tcode_report";
    pub const SERVER_NAME_COMPUTER_USE: &'static str = "tcode_computer_use";
    pub const SERVER_NAME_PULL_REQUESTS: &'static str = "tcode_pull_requests";
    pub const SERVER_NAME_CONTROL: &'static str = "tcode_control";

    /// Codex `-c` override value: an inline TOML table for a streamable-HTTP MCP
    /// server. Codex rejects a literal `bearer_token` for HTTP, so the token
    /// rides in `http_headers.Authorization` instead (verified against
    /// codex `config/src/mcp_types.rs`). Returns the full `key=value` argument.
    pub fn codex_config_override(&self) -> String {
        format!(
            "mcp_servers.{name}={{url=\"{url}\",http_headers={{Authorization=\"Bearer {token}\"}}}}",
            name = self.name,
            url = self.url,
            token = self.bearer_token,
        )
    }
}

/// Claude Code `--mcp-config` JSON containing every supplied registration.
pub fn claude_mcp_config_json<'a>(
    registrations: impl IntoIterator<Item = &'a McpRegistration>,
) -> String {
    let servers: serde_json::Map<String, serde_json::Value> = registrations
        .into_iter()
        .map(|registration| {
            (
                registration.name.clone(),
                serde_json::json!({
                    "type": "http",
                    "url": registration.url,
                    "headers": {
                        "Authorization": format!("Bearer {}", registration.bearer_token),
                    }
                }),
            )
        })
        .collect();
    serde_json::json!({ "mcpServers": servers }).to_string()
}

/// One model a provider offers, with its selectable options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: String, // provider-native id sent on the wire
    pub display_name: String,
    pub is_default: bool,
    pub options: Vec<OptionDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OptionDescriptor {
    Select {
        id: String,
        label: String,
        options: Vec<SelectOption>,
        default_value: Option<String>,
        #[serde(default)]
        role: OptionRole,
        #[serde(default)]
        apply: ApplyTiming,
        #[serde(default)]
        recommended: Option<String>,
        #[serde(default)]
        permissive: Option<String>,
    },
    Boolean {
        id: String,
        label: String,
        default_value: bool,
        #[serde(default)]
        role: OptionRole,
        #[serde(default)]
        apply: ApplyTiming,
        #[serde(default)]
        recommended: Option<bool>,
        #[serde(default)]
        permissive: Option<bool>,
    },
}

/// Where the UI places the control; carries no permission semantics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionRole {
    #[default]
    Model,
    Permission,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyTiming {
    #[default]
    Live,
    NextTurn,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
    /// Present when this value cannot be selected; explains why.
    #[serde(default)]
    pub unavailable: Option<String>,
}

/// Pre-session controls; a session's ProviderOptions is authoritative.
pub fn permission_control(provider: ProviderKind) -> Option<OptionDescriptor> {
    let (id, apply, default, recommended, permissive, values) = match provider {
        ProviderKind::ClaudeCode => (
            "permissionMode",
            ApplyTiming::Live,
            "default",
            "auto",
            "bypassPermissions",
            &[
                (
                    "default",
                    "Manual",
                    "Reads and built-in read-only commands run without asking; edits, other commands and network access ask you.",
                ),
                (
                    "acceptEdits",
                    "Accept edits",
                    "Reads, file edits and common filesystem commands run without asking; other commands ask.",
                ),
                (
                    "auto",
                    "Auto",
                    "A background safety classifier reviews each action; safe ones run, risky ones are blocked or ask. Unavailable when the account, model or organization does not support it.",
                ),
                (
                    "dontAsk",
                    "Don't ask",
                    "Only pre-approved tools run; anything that would ask is denied instead.",
                ),
                (
                    "bypassPermissions",
                    "Bypass permissions",
                    "Skips permission prompts. Deny/ask rules and protected paths still apply.",
                ),
            ] as &[(&str, &str, &str)],
        ),
        ProviderKind::Codex => (
            "permissions",
            ApplyTiming::NextTurn,
            "ask",
            "auto_review",
            "full_access",
            &[
                (
                    "ask",
                    "Ask for approval",
                    "Reads, edits and commands run inside the workspace sandbox; access to the internet or files outside the workspace asks you.",
                ),
                (
                    "auto_review",
                    "Approve for me",
                    "Same workspace sandbox; requests that would ask are reviewed by Codex's auto-reviewer and only actions it finds risky ask you.",
                ),
                (
                    "read_only",
                    "Read Only",
                    "Reads run in a read-only sandbox; editing files or network access asks you.",
                ),
                ("full_access", "Full Access", "No sandbox and no prompts."),
            ] as &[(&str, &str, &str)],
        ),
        ProviderKind::Grok => (
            "permissionMode",
            ApplyTiming::Restart,
            "default",
            "auto",
            "bypassPermissions",
            &[
                (
                    "default",
                    "Ask",
                    "Reads run without asking; edits and commands ask you.",
                ),
                (
                    "acceptEdits",
                    "Accept edits",
                    "Edits run without asking; commands still ask.",
                ),
                (
                    "auto",
                    "Auto",
                    "A safety classifier approves routine actions; flagged ones ask you.",
                ),
                (
                    "bypassPermissions",
                    "Always approve",
                    "Every tool call runs; deny rules still apply.",
                ),
                (
                    "dontAsk",
                    "Don't ask",
                    "Only pre-approved tools run; anything that would ask is denied.",
                ),
            ] as &[(&str, &str, &str)],
        ),
        // Cursor exposes approvalMode only in user config, with no CLI override; --force honors unrestricted.
        ProviderKind::Cursor => (
            "approvalMode",
            ApplyTiming::Restart,
            "unrestricted",
            "unrestricted",
            "unrestricted",
            &[(
                "unrestricted",
                "Run everything",
                "Every tool call runs without asking",
            )] as &[(&str, &str, &str)],
        ),
        ProviderKind::OpenCode => (
            "permission_mode",
            ApplyTiming::Live,
            "normal",
            "auto",
            "auto",
            &[
                (
                    "normal",
                    "Normal",
                    "Follows your OpenCode permission rules; actions marked ask wait for you.",
                ),
                (
                    "auto",
                    "Auto-approve",
                    "Answers every ask with once, as OpenCode's own clients do; explicit deny rules still deny.",
                ),
            ] as &[(&str, &str, &str)],
        ),
        ProviderKind::Pi | ProviderKind::Acp => return None,
    };
    Some(OptionDescriptor::Select {
        id: id.into(),
        label: "Permissions".into(),
        options: values
            .iter()
            .map(|(value, label, description)| SelectOption {
                value: (*value).into(),
                label: (*label).into(),
                description: Some((*description).into()),
                unavailable: None,
            })
            .collect(),
        default_value: Some(default.into()),
        role: OptionRole::Permission,
        apply,
        recommended: Some(recommended.into()),
        permissive: Some(permissive.into()),
    })
}

/// The ACP session option that carries the agent's mode, which ACP agents use for their
/// approval modes.
pub const ACP_MODE_OPTION_ID: &str = "acp:mode";

/// Whether option `id` of `provider` decides what runs without the user's approval: the
/// provider's permission control, or an ACP session mode, which ACP agents use for their
/// approval modes alongside anything else.
pub fn option_governs_approvals(provider: ProviderKind, id: &str) -> bool {
    id == ACP_MODE_OPTION_ID
        || permission_control(provider).is_some_and(|descriptor| match descriptor {
            OptionDescriptor::Select { id: own, .. }
            | OptionDescriptor::Boolean { id: own, .. } => own == id,
        })
}

pub fn permission_notice(provider: ProviderKind) -> Option<&'static str> {
    match provider {
        ProviderKind::Pi => Some("No permission control"),
        _ => None,
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn permission_controls_publish_native_choices_and_decode_old_model_options() {
        for (provider, id, apply, values, default, recommended, permissive) in [
            (
                ProviderKind::ClaudeCode,
                "permissionMode",
                ApplyTiming::Live,
                vec![
                    "default",
                    "acceptEdits",
                    "auto",
                    "dontAsk",
                    "bypassPermissions",
                ],
                "default",
                "auto",
                "bypassPermissions",
            ),
            (
                ProviderKind::Codex,
                "permissions",
                ApplyTiming::NextTurn,
                vec!["ask", "auto_review", "read_only", "full_access"],
                "ask",
                "auto_review",
                "full_access",
            ),
            (
                ProviderKind::Grok,
                "permissionMode",
                ApplyTiming::Restart,
                vec![
                    "default",
                    "acceptEdits",
                    "auto",
                    "bypassPermissions",
                    "dontAsk",
                ],
                "default",
                "auto",
                "bypassPermissions",
            ),
            (
                ProviderKind::OpenCode,
                "permission_mode",
                ApplyTiming::Live,
                vec!["normal", "auto"],
                "normal",
                "auto",
                "auto",
            ),
            (
                ProviderKind::Cursor,
                "approvalMode",
                ApplyTiming::Restart,
                vec!["unrestricted"],
                "unrestricted",
                "unrestricted",
                "unrestricted",
            ),
        ] {
            let OptionDescriptor::Select {
                id: actual_id,
                options,
                default_value,
                role,
                apply: actual_apply,
                recommended: actual_recommended,
                permissive: actual_permissive,
                ..
            } = permission_control(provider).unwrap()
            else {
                panic!("native select control")
            };
            assert_eq!(actual_id, id);
            assert_eq!(role, OptionRole::Permission);
            assert_eq!(actual_apply, apply);
            assert_eq!(
                options
                    .iter()
                    .map(|option| option.value.as_str())
                    .collect::<Vec<_>>(),
                values
            );
            assert!(options.iter().all(|option| {
                option
                    .description
                    .as_ref()
                    .is_some_and(|description| !description.is_empty())
                    && option.unavailable.is_none()
            }));
            assert_eq!(default_value.as_deref(), Some(default));
            assert_eq!(actual_recommended.as_deref(), Some(recommended));
            assert_eq!(actual_permissive.as_deref(), Some(permissive));
        }
        assert_eq!(permission_control(ProviderKind::Pi), None);
        assert_eq!(permission_control(ProviderKind::Acp), None);
        let old: OptionDescriptor = serde_json::from_str(r#"{"kind":"select","id":"reasoningEffort","label":"Reasoning","options":[{"value":"high","label":"High","description":null}],"default_value":"high"}"#).unwrap();
        assert!(
            matches!(old, OptionDescriptor::Select { role:OptionRole::Model, apply:ApplyTiming::Live, recommended:None, permissive:None, options, .. } if options[0].unavailable.is_none())
        );
    }
}

/// Resolve a provider binary to an absolute path before spawning.
///
/// Every provider spawn sets `current_dir(cwd)`, and a bare program name plus a
/// working-directory change makes PATH resolution unreliable (it fails outright
/// when PATH holds unexpanded entries such as `~/.dotnet/tools`). Resolving the
/// binary ourselves against the parent's PATH keeps the lookup deterministic.
fn effective_path_os() -> Option<std::ffi::OsString> {
    #[cfg(feature = "process")]
    {
        process::host_path()
    }
    #[cfg(not(feature = "process"))]
    {
        std::env::var_os("PATH")
    }
}

#[cfg(feature = "process")]
pub(crate) fn resolve_binary(
    binary_path: Option<&std::path::Path>,
    default_name: &str,
) -> Result<PathBuf, AgentError> {
    if let Some(path) = binary_path {
        return Ok(path.to_path_buf());
    }
    // An explicit path component means "use this as given" (no PATH search).
    // Both separators are checked: Windows accepts `/` in paths too.
    if default_name.contains(['/', '\\']) {
        return Ok(PathBuf::from(default_name));
    }
    if effective_path_os().is_none() {
        return Err(AgentError::Spawn(format!(
            "`{default_name}` not found: PATH is unset"
        )));
    }
    find_on_path(default_name).ok_or_else(|| {
        AgentError::Spawn(format!(
            "`{default_name}` not found on PATH (set its path in Settings → Providers)"
        ))
    })
}

/// Locate `name` on the parent's `PATH`, returning its absolute path.
///
/// On Windows this is `PATHEXT`-aware: the provider CLIs (and npm/pnpm/bun) are
/// installed as `claude.cmd` / `codex.exe` shims and *never* exist under their
/// bare name, so a plain `dir.join(name)` probe finds nothing. Each PATH entry
/// is tried with every `PATHEXT` extension, in `PATHEXT` order, before the bare
/// name. Elsewhere it is the classic "first executable file on PATH" search.
///
/// Shared with the app crate (`crate::process`), which routes every bare-name
/// spawn (npm, pnpm, bun, git, …) through it on Windows.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = effective_path_os()?;
    find_in_dirs(
        std::env::split_paths(&path_var),
        name,
        &path_extensions(),
        is_executable,
    )
}

/// The executable extensions to try for a bare name: `PATHEXT` on Windows
/// (falling back to its documented default), and nothing anywhere else.
fn path_extensions() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .map(str::to_string)
        .collect()
}

/// The file names to probe for `name` in one PATH directory, in priority order.
///
/// A name that already carries an extension (`claude.cmd`, `codex.exe`) is used
/// as given. A bare name gets each `pathext` entry appended, with the
/// extensionless name tried last (so a Unix-style extensionless binary dropped
/// on a Windows PATH still resolves). With an empty `pathext` (every non-Windows
/// target) this is just `[name]`.
fn candidate_names(name: &str, pathext: &[String]) -> Vec<String> {
    if std::path::Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let mut names: Vec<String> = pathext.iter().map(|ext| format!("{name}{ext}")).collect();
    names.push(name.to_string());
    names
}

/// The PATH walk itself, parameterized on the directories, the extension list
/// and the "is this executable" predicate so both platform branches are testable
/// from any host (see `resolve_binary_tests`).
fn find_in_dirs(
    dirs: impl IntoIterator<Item = PathBuf>,
    name: &str,
    pathext: &[String],
    is_exec: impl Fn(&std::path::Path) -> bool,
) -> Option<PathBuf> {
    let candidates = candidate_names(name, pathext);
    for dir in dirs {
        // Skip unexpanded/relative entries — they are meaningless to us and
        // would resolve against the child's cwd.
        if !dir.is_absolute() {
            continue;
        }
        for candidate in &candidates {
            let path = dir.join(candidate);
            if is_exec(&path) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Windows has no exec bit: a regular file whose name matched a `PATHEXT`
/// candidate *is* the executable.
#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// A chosen option value, persisted per session as a select string or boolean.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionSelection {
    pub id: String,
    pub value: serde_json::Value,
}

#[cfg(feature = "process")]
fn selection_str(selections: &[OptionSelection], id: &str) -> Option<String> {
    selections
        .iter()
        .find(|selection| selection.id == id)
        .and_then(|selection| selection.value.as_str().map(str::to_owned))
}

#[cfg(feature = "process")]
fn selection_bool(selections: &[OptionSelection], id: &str) -> Option<bool> {
    selections
        .iter()
        .find(|selection| selection.id == id)
        .and_then(|selection| selection.value.as_bool())
}

/// Whether a [`AgentEvent::UserInputRequested`] holds the agent until it is
/// answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserInputDelivery {
    /// The agent is blocked until a matching
    /// [`SessionCommand::RespondUserInput`] arrives.
    #[default]
    Blocking,
    /// The agent keeps working on the turn and reads the answer when it is
    /// delivered (Codex `request_user_input_async`). The request stays open
    /// until answered or the turn ends; the composer stays usable meanwhile.
    Async,
}

impl UserInputDelivery {
    pub fn is_blocking(&self) -> bool {
        matches!(self, Self::Blocking)
    }
}

/// A structured question the agent asks the user (Claude `AskUserQuestion`,
/// Codex `item/tool/requestUserInput`). Rendered as a multiple-choice (or
/// free-text) prompt; answers ride back through [`SessionCommand::RespondUserInput`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserInputQuestion {
    /// Original field schema, required state and option values retained for host rendering and validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<Value>,
    /// The answer key. Claude and Grok: the complete question text (both index
    /// answers by question text). Codex and Cursor: the native question id.
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<UserInputOption>,
    pub multi_select: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserInputOption {
    pub label: String,
    pub description: String,
}

/// An inline image or a verified workspace file delivered beside the turn text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// MIME type, e.g. `image/png`.
    pub media_type: String,
    /// Standard-base64 of the raw bytes (no `data:...;base64,` prefix).
    pub data_base64: String,
    /// A staged file path. With no inline data, deliver the file by reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

impl Attachment {
    pub fn workspace_file(&self) -> Option<&str> {
        self.source_path.as_deref().filter(|_| self.data_base64.is_empty())
    }
}

/// A provider-native command or skill surfaced to the composer's `/` and `$`
/// menus. Each native provider contributes what its command/skill discovery
/// surface exposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCommand {
    pub name: String,
    pub description: Option<String>,
    pub kind: ProviderCommandKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCommandKind {
    /// A `/`-command, sent as `/name …` at the start of the turn text (Claude
    /// `slash_commands`, which include its skills).
    Command,
    /// A `$`-skill, sent as a `$name` mention in the turn text (Codex skills).
    /// A skill the provider also lists as a command is sent as that command,
    /// the only form Claude Code expands.
    Skill,
}

/// Per-turn overrides layered on top of the session's persisted options.
/// Codex and OpenCode apply effort/variant per turn; Claude confirms its
/// permission mode before submitting each turn. Pi uses session-level options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnOptions {
    pub effort: Option<String>,
    /// Native read-only planning mode, applied before the provider sees this
    /// turn. Adapters that cannot confirm the switch must fail the turn.
    pub interaction_mode: Option<String>,
}

/// Where a static model catalog may refresh its data from. Claude's catalog is
/// the t3code model manifest (see `claude_manifest`); the other providers
/// discover their models from the CLI and ignore this.
#[derive(Debug, Clone, Default)]
pub struct CatalogRefresh {
    /// Directory holding the last good remote manifest (the tcode data dir).
    pub cache_dir: Option<PathBuf>,
    /// Whether a network fetch is allowed (the provider update-check setting).
    pub network: bool,
}

/// List the provider's models (spawn, query, teardown). `launch_env` carries the
/// provider's configured environment/home so the catalog reflects the same CLI
/// (and account) a session would actually run against.
#[cfg(feature = "process")]
pub async fn list_models(
    provider: ProviderKind,
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
    refresh: CatalogRefresh,
) -> Result<Vec<ModelSpec>, AgentError> {
    match provider {
        ProviderKind::Codex => codex::list_models(binary_path, launch_env).await,
        ProviderKind::ClaudeCode => claude::list_models(binary_path, launch_env, refresh).await,
        ProviderKind::Pi => pi::list_models(binary_path, launch_env).await,
        ProviderKind::OpenCode => Err(AgentError::Protocol("OpenCode 2 model catalog is owned by the Muteki native driver".into())),
        ProviderKind::Cursor => cursor::list_models(binary_path, launch_env).await,
        ProviderKind::Grok => grok::list_models(binary_path, launch_env).await,
        // ACP agents advertise their models over the wire at session start
        // (`AgentEvent::ProviderOptions`), so there is no catalog to pre-fetch.
        ProviderKind::Acp => Ok(Vec::new()),
    }
}

#[derive(Debug)]
pub enum AgentError {
    Spawn(String),
    Protocol(String),
    Provider(String),
    Io(std::io::Error),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(message) => {
                write!(formatter, "failed to spawn provider process: {message}")
            }
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Provider(message) => write!(formatter, "provider reported error: {message}"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AgentError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Commands the UI sends into a live session's actor loop.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    SendTurn {
        /// Runtime queue id echoed by [`AgentEvent::TurnAccepted`] after the
        /// adapter has crossed its provider-specific submission boundary.
        delivery_id: u64,
        text: String,
        /// Per-turn overrides (Codex/OpenCode apply effort per turn; Claude/pi
        /// use session options). `None` = use the persisted options.
        options: Option<TurnOptions>,
        /// Image attachments carried alongside the text (empty for text-only
        /// turns). Each provider maps these onto its native content blocks.
        attachments: Vec<Attachment>,
    },
    RetryTurn {
        turn_id: String,
    },
    Interrupt,
    RespondApproval {
        request_id: String,
        decision: ApprovalDecision,
    },
    /// Answer a pending user-input request (Claude `AskUserQuestion`, Codex
    /// `item/tool/requestUserInput` and `request_user_input_async`). Each value
    /// is a string (single-select / free text) or an array of strings
    /// (multi-select), keyed by the matching [`UserInputQuestion::id`].
    RespondUserInput {
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
    },
    /// Inject a message into the turn that is ALREADY running, so the model
    /// picks it up at its next opportunity to accept input (typically the next
    /// tool call). Distinct from queueing, which is an app-level concept: a
    /// queued message is held and sent as an ordinary [`Self::SendTurn`] once
    /// the current turn completes.
    ///
    /// Only providers whose [`ProviderKind::caps`] reports steering accept
    /// this; the others log and ignore it (the UI must not offer it there).
    Steer {
        /// Stable id correlating the persisted request with provider acceptance.
        request_id: String,
        text: String,
        attachments: Vec<Attachment>,
    },
    /// Set one of the agent's self-described options (see
    /// [`AgentEvent::ProviderOptions`]). An agent speaking ACP gets
    /// `session/set_mode` or `session/set_config_option` by the descriptor's
    /// origin; the other providers ignore ids they do not know.
    SetOption {
        id: String,
        value: serde_json::Value,
    },
    /// Ask the provider to restore one of its own message checkpoints. The
    /// provider remains the sole owner of both the checkpoint and the restore
    /// semantics; adapters without a native implementation reject this.
    Rewind {
        checkpoint_id: String,
        mode: RewindMode,
    },
    PreviewRewind {
        checkpoint_id: String,
        mode: RewindMode,
    },
    UndoRewind {
        checkpoint_id: String,
        undo_token: String,
    },
    StopBackgroundTask {
        task_id: Option<String>,
    },
    /// Ask the provider to compact its context now, the way its own `/compact`
    /// does (Codex `thread/compact/start`, pi `compact`, Claude Code's
    /// `/compact` slash command). The provider reports progress as
    /// [`AgentEvent::ContextCompacted`]; adapters without a native operation
    /// warn and ignore it, and the UI offers it only where
    /// [`Caps::manual_compaction`] is set.
    Compact,
    /// Set, replace or change the status of the harness's standing goal the
    /// way its own `/goal` does (Codex `thread/goal/set`, Claude Code's `/goal`
    /// slash command). `None` keeps the current value. The provider reports
    /// the result as [`AgentEvent::GoalUpdated`]; the UI offers it only where
    /// [`Caps::goals`] is set.
    SetGoal {
        objective: Option<String>,
        status: Option<GoalStatus>,
    },
    /// Clear the standing goal (Codex `thread/goal/clear`, `/goal clear`).
    ClearGoal,
    Shutdown,
}

/// A live provider session: send commands in, read canonical events out.
/// Dropping both channels (or sending `Shutdown`) tears the child process down.
#[cfg(feature = "process")]
pub struct SessionHandle {
    pub provider: ProviderKind,
    pub commands: Sender<SessionCommand>,
    pub events: Receiver<AgentEvent>,
}

#[cfg(feature = "process")]
async fn spawn_session<F, Fut>(
    provider: ProviderKind,
    opts: SessionOptions,
    actor: F,
    exited_message: &'static str,
) -> Result<SessionHandle, AgentError>
where
    F: FnOnce(
        SessionOptions,
        Receiver<SessionCommand>,
        Sender<AgentEvent>,
        Sender<Result<(), AgentError>>,
    ) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (commands, command_rx) = smol::channel::unbounded();
    let (event_tx, events) = smol::channel::unbounded();
    let (ready_tx, ready) = smol::channel::bounded(1);
    smol::spawn(actor(opts, command_rx, event_tx, ready_tx)).detach();
    ready
        .recv()
        .await
        .map_err(|_| AgentError::Protocol(exited_message.into()))??;
    Ok(SessionHandle {
        provider,
        commands,
        events,
    })
}

/// Start a new (or resumed) session with the given provider.
#[cfg(feature = "process")]
pub async fn start_session(
    provider: ProviderKind,
    opts: SessionOptions,
) -> Result<SessionHandle, AgentError> {
    // Spawning into a missing directory fails with the OS's "not found" (or
    // Windows' "invalid directory"), which every provider then reports as its
    // binary being missing. Worktrees are routinely removed under old threads.
    if !opts.cwd.is_dir() {
        return Err(AgentError::Spawn(format!(
            "working directory `{}` no longer exists",
            opts.cwd.display()
        )));
    }
    match provider {
        ProviderKind::Codex => codex::start(opts).await,
        ProviderKind::ClaudeCode => claude::start(opts).await,
        ProviderKind::Pi => pi::start(opts).await,
        ProviderKind::OpenCode => Err(AgentError::Protocol("OpenCode 2 sessions are owned by the Muteki native driver".into())),
        ProviderKind::Cursor => cursor::start(opts).await,
        ProviderKind::Grok => grok::start(opts).await,
        ProviderKind::Acp => acp::start(opts).await,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanResolution {
    /// Accepted in this thread.
    Implemented,
    /// Accepted in a fresh thread.
    HandedOff { session_id: String },
    /// Explicitly dismissed by the user.
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierCategory {
    Cyber,
    Bio,
    Other(String),
}

impl ClassifierCategory {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "cyber" => Self::Cyber,
            "bio" => Self::Bio,
            other => Self::Other(other.to_string()),
        }
    }
}

/// A failed native turn can be continued without adding user input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRetry {
    pub turn_id: String,
    /// Unix seconds; None leaves only the manual retry available.
    pub retry_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    ProcessStarted { pid: u32 },
    /// A native delegated agent patch, retaining the provider's stable identity.
    NativeAgentUpdate { patch: serde_json::Value },
    TurnRetry {
        retry: Option<TurnRetry>,
    },
    /// A tcode-level handoff between providers. This is never emitted by an
    /// adapter; the runtime persists it before the first relayed user turn.
    ProviderRelay {
        from_provider: ProviderKind,
        from_model: Option<String>,
        to_provider: ProviderKind,
        to_model: Option<String>,
        /// The handoff as sent to `to_provider`: the transcript of the
        /// conversation so far wrapped around the user's message. Records from
        /// before it was kept carry `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        /// Set when the handoff follows the thread's move to another directory: the
        /// directory the native session it replaces belonged to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        moved_from: Option<PathBuf>,
    },
    /// History-only: retained to decode stored plan decisions; never emitted.
    PlanResolved {
        item_id: String,
        resolution: PlanResolution,
    },
    /// The agent's self-described options (ACP `modes` / `models` /
    /// `configOptions`), pushed at session start and on every change. Reuses
    /// [`OptionDescriptor`] so the composer's existing picker renders them
    /// verbatim; answer with [`SessionCommand::SetOption`].
    ProviderOptions {
        descriptors: Vec<OptionDescriptor>,
        selections: Vec<OptionSelection>,
    },
    /// Emitted once the provider session is ready. `resume` must round-trip
    /// through `SessionOptions::resume` to continue this thread later.
    SessionStarted {
        provider_session_id: String,
        resume: ResumeCursor,
        model: Option<String>,
    },
    /// The model that actually served the response. Providers may reroute away
    /// from the model selected when the session started.
    ServedModel {
        model: String,
        reason: Option<String>,
    },
    ModelFallbackDetected {
        expected: String,
        actual: String,
        category: Option<ClassifierCategory>,
        /// Claude's `refused_user_message_uuid` — the native rewind checkpoint for the refused turn.
        checkpoint_id: Option<String>,
        parent_tool_use_id: Option<String>,
    },
    TurnBlocked {
        category: Option<ClassifierCategory>,
        /// The selected model that refused (NOT "<synthetic>").
        model: Option<String>,
        /// Human-readable detail from Claude's error text.
        detail: String,
    },
    /// A provider control operation failed before the turn's prompt was sent.
    /// `code` is stable so hosts can distinguish it from model execution.
    TurnFailed {
        code: String,
        message: String,
    },
    TurnStarted {
        turn_id: String,
    },
    /// Transient delivery acknowledgement for [`SessionCommand::SendTurn`].
    /// The runtime consumes this before persistence: it is control-plane state,
    /// not part of the canonical conversation history.
    TurnAccepted {
        delivery_id: u64,
    },
    /// The agent took none of [`SessionOptions::mcp_servers`] (an ACP agent without HTTP MCP),
    /// so neither the tools nor [`SessionOptions::instructions`] reach the model. Transient, like
    /// [`Self::TurnAccepted`].
    McpServersNotAttached,
    /// A delivery response from the provider transport. Accepted does not mean consumed.
    RuntimeOperationResult { request_id:String, result:Option<Value>, error:Option<String> },
    RuntimeCapabilities { capabilities: Value },
    ControlDelivery { request_id: String, operation: String, status: String, error: Option<Value> },
    /// Detailed usage preserving provider cache-write and reasoning fields.
    UsageDetail { usage: Value, scope: String, usage_id: String },
    /// A provider woke itself from a completed background report; no prompt is submitted.
    ContinuationStarted { origin_turn_id: String, turn_id: String },
    /// Provider background work which keeps the session alive after its foreground turn.
    BackgroundTasksChanged {
        count: usize,
    },
    BackgroundTaskList {
        tasks: Vec<BackgroundTask>,
    },
    /// Pins asynchronous provider events to their original native execution.
    Origin {
        turn_id: String,
        agent_id: Option<String>,
        event: Box<AgentEvent>,
    },
    /// The provider's current net file changes for one turn. Unlike the
    /// per-tool [`ItemContent::FileChange`] stream, this is a replacement
    /// snapshot: later events with the same `turn_id` supersede earlier ones.
    /// Providers that supply the exact net result of their tracked mutations
    /// mark it [`ChangeCompleteness::Exact`]; adapters built from structured
    /// edit fragments use [`ChangeCompleteness::Partial`].
    TurnChangesUpdated {
        turn_id: String,
        changes: Vec<FileChange>,
        completeness: ChangeCompleteness,
    },
    /// A provider-owned restore point attached to one user turn. The id is
    /// opaque and must only be returned to the provider that emitted it.
    TurnCheckpoint {
        turn_id: String,
        checkpoint_id: String,
    },
    /// The provider successfully applied a native restore operation. A
    /// conversation restore also returns the selected prompt for composer
    /// prefill, matching the provider's own edit-after-rewind behavior.
    RewindCompleted {
        checkpoint_id: String,
        mode: RewindMode,
        prefill: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prefill_attachments: Vec<String>,
        #[serde(default)]
        later_history: LaterHistory,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        files: Option<RewindFiles>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        incomplete: Option<RewindIncomplete>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        undo_token: Option<String>,
    },
    /// A native restore request was rejected without changing tcode's local
    /// conversation state.
    RewindFailed {
        checkpoint_id: String,
        mode: RewindMode,
        failure: RewindFailure,
    },
    RewindPreviewed {
        checkpoint_id: String,
        mode: RewindMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        files: Option<RewindFiles>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    RewindUndone {
        checkpoint_id: String,
    },
    RewindUndoFailed {
        checkpoint_id: String,
        failure: RewindFailure,
    },
    UnreadableRecord {
        line: String,
    },
    TurnCompleted {
        turn_id: String,
        status: TurnStatus,
        usage: Option<TokenUsage>,
    },
    ItemStarted(ThreadItem),
    ItemUpdated(ThreadItem),
    ItemCompleted(ThreadItem),
    /// A runtime-synthesized steering request, persisted before provider delivery.
    SteerRequested {
        request_id: String,
        text: String,
        /// Local paths of the image attachments steered with the text (see
        /// [`ItemContent::UserMessage::attachments`]).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    /// Emitted only after the provider has consumed the correlated steer into
    /// its model context (or provided the strongest available consumption
    /// signal when its protocol has no explicit acknowledgement). Codex echoes
    /// the drained input as a `userMessage` item before its next model request;
    /// the `turn/steer` reply alone only means the input was enqueued. Claude
    /// uses its next `status: requesting` checkpoint; a steer written
    /// microseconds before that status can still miss the request, but this is
    /// the best available signal without CLI protocol support.
    SteerAccepted {
        request_id: String,
    },
    /// The provider could not fold the correlated steer into a running turn:
    /// the turn had already ended by the time the request reached it. The
    /// text was never seen by the model, so the runtime re-sends it as an
    /// ordinary turn.
    SteerRejected {
        request_id: String,
    },
    /// Streaming text growth for an in-progress item. The item may not have
    /// been announced via `ItemStarted` yet (providers differ); the UI creates
    /// the item lazily on first delta.
    Delta {
        item_id: String,
        kind: DeltaKind,
        text: String,
    },
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved {
        request_id: String,
        decision: ApprovalDecision,
    },
    /// The agent is asking the user one or more structured questions.
    /// `delivery` says whether it waits for the matching
    /// [`SessionCommand::RespondUserInput`] or keeps working meanwhile.
    UserInputRequested {
        request_id: String,
        questions: Vec<UserInputQuestion>,
        #[serde(default, skip_serializing_if = "UserInputDelivery::is_blocking")]
        delivery: UserInputDelivery,
    },
    /// A pending user-input request has been settled (answered, or cancelled on
    /// teardown — in which case `answers` is empty).
    UserInputResolved {
        request_id: String,
        answers: serde_json::Map<String, serde_json::Value>,
    },
    TokenUsage(TokenUsage),
    /// Provider-native commands / skills discovered for this session. The
    /// composer's `/` and `$` menus consume these. Session metadata — not folded
    /// into the timeline / persisted to the JSONL log.
    ProviderCommands {
        commands: Vec<ProviderCommand>,
    },
    /// The provider compacted its context window (Claude `system/compact_boundary`;
    /// Codex `contextCompaction` item). Rendered as a "Context compacted" work-log row.
    ContextCompacted(Compaction),
    /// A tcode-level context-window change. This is never emitted by an adapter;
    /// the runtime persists it after the user message that selected the window.
    ContextWindowChanged {
        window: u64,
    },
    /// Another thread's model acted on this thread through `tcode_control`. This is
    /// never emitted by an adapter; the runtime records it in the target thread.
    ControlAction(ControlAction),
    /// Structured plan / task list for the sidebar (Codex `turn/plan/updated`,
    /// Claude `TodoWrite`). Replaces the current turn's plan wholesale.
    PlanUpdated {
        turn_id: Option<String>,
        explanation: Option<String>,
        steps: Vec<PlanStep>,
    },
    /// The harness's standing goal changed (Codex `thread/goal/updated` and
    /// `thread/goal/cleared`, Claude Code's `goal_status` transcript records).
    /// Replaces the session's goal wholesale; `None` means there is none.
    GoalUpdated {
        goal: Option<Goal>,
    },
    /// History-only: retained to decode stored proposed-plan deltas; never emitted.
    ProposedPlanDelta {
        item_id: String,
        text: String,
    },
    /// History-only: retained to decode stored proposed plans; never emitted.
    ProposedPlan {
        item_id: String,
        markdown: String,
    },
    Warning {
        message: String,
    },
    /// A runtime-synthesized fatal provider startup failure persisted for replay.
    #[rustfmt::skip]
    ProviderStartFailed { error: String },
    Error {
        message: String,
        fatal: bool,
    },
    /// Emitted after the [`AgentEvent::Error`] of a turn Claude Code rejected
    /// for an exhausted usage window. `resets_at` is unix seconds.
    UsageLimitReached {
        resets_at: u64,
    },
    SessionClosed {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

/// An objective the harness keeps working toward across turns until it is
/// met, paused or cleared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_used_secs: Option<u64>,
    /// How many times the harness has checked the objective against the work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<u32>,
    /// The latest check's verdict, when the harness explains it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    /// The harness judged it cannot make progress (Codex `blocked`, a Claude
    /// Code check that found the objective impossible).
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Failed,
    Interrupted,
}

pub use rewind_protocol::{LaterHistory, RewindMode, RewindFile, RewindFiles, RewindFailure,
    RewindFailureReason, RewindIncomplete};

/// How strongly a provider can attribute a turn's reported file changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeCompleteness {
    /// The provider supplied the exact turn-scoped net result of the file
    /// mutations it tracks, rather than inferring it from the ambient working
    /// tree. Uninstrumented command writes are outside that claim.
    Exact,
    /// The listed paths came from structured edit operations, but arbitrary
    /// commands or external writers may have changed additional files.
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    AssistantText,
    ReasoningText,
    CommandOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    Interrupted,
    Declined,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadItem {
    /// Provider-scoped stable id; deltas and later lifecycle events reference it.
    pub id: String,
    /// Spawn item that owns this child activity, when the item came from a subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_item_id: Option<String>,
    pub content: ItemContent,
}

/// Protocol-supplied presentation, kept separate from a tool's callable name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolPresentation {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub output_unavailable: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemContent {
    UserMessage {
        text: String,
        /// Byte length of an injected context prefix folded into `text` — the
        /// orchestrate guidance + configuration that `/orchestrate` composes
        /// ahead of the user's own words. The provider still receives the whole
        /// `text`; the UI uses this offset to render the prefix as a collapsed
        /// disclosure row and keep the bubble to `text[context_len..]`. Absent on
        /// ordinary messages and on every log written before this field existed,
        /// which therefore render as a plain bubble exactly as before.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_len: Option<usize>,
        /// Local paths of the image attachments sent with this message, so the
        /// timeline (live and replayed) can render them. Empty on text-only
        /// messages and on logs written before this field existed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
    },
    AssistantMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    CommandExecution {
        command: String,
        output: String,
        exit_code: Option<i32>,
        status: ItemStatus,
    },
    FileChange {
        changes: Vec<FileChange>,
        status: ItemStatus,
    },
    ToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        presentation: Option<ToolPresentation>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        image_reads: Vec<Attachment>,
        name: String,
        input: serde_json::Value,
        output: Option<String>,
        status: ItemStatus,
    },
    Subagent {
        agent_type: String,
        description: String,
        status: ItemStatus,
        /// Final one-line summary when finished.
        summary: Option<String>,
        /// Model the provider ran the subagent on, once known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Reasoning effort the subagent ran with, once known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },

    ImageRead {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<Attachment>,
    },
    WebSearch {
        query: String,
    },
    /// Anything canonicalization doesn't model yet; `summary` is displayable.
    Other {
        provider_kind: String,
        summary: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    /// Unified diff for this file when the provider supplies one.
    pub diff: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Create,
    Modify,
    Delete,
    Rename,
}

/// Split a provider-supplied, multi-file Git-style unified diff into canonical
/// file changes. This parses only file boundaries and operation kinds; each
/// returned change retains its complete diff section for rendering.
pub fn file_changes_from_unified_diff(diff: &str) -> Result<Vec<FileChange>, String> {
    if diff.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut sections = Vec::new();
    let mut current = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") && !current.is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        if !current.is_empty() || line.starts_with("diff --git ") {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.is_empty() {
        sections.push(current);
    }
    if sections.is_empty() {
        return Err("turn diff contains no `diff --git` file sections".into());
    }

    sections
        .into_iter()
        .map(|section| {
            let mut old_path = None;
            let mut new_path = None;
            for line in section.lines() {
                if let Some(path) = line.strip_prefix("rename from ") {
                    old_path = Some(path.to_string());
                } else if let Some(path) = line.strip_prefix("rename to ") {
                    new_path = Some(path.to_string());
                } else if let Some(path) = line.strip_prefix("--- a/") {
                    old_path.get_or_insert_with(|| path.to_string());
                } else if let Some(path) = line.strip_prefix("+++ b/") {
                    new_path.get_or_insert_with(|| path.to_string());
                }
            }
            if old_path.is_none() || new_path.is_none() {
                let header = section
                    .lines()
                    .next()
                    .and_then(|line| line.strip_prefix("diff --git a/"))
                    .ok_or_else(|| "turn diff section has no valid header".to_string())?;
                if let Some((old, new)) = header.rsplit_once(" b/") {
                    old_path.get_or_insert_with(|| old.to_string());
                    new_path.get_or_insert_with(|| new.to_string());
                }
            }

            let creates = section
                .lines()
                .any(|line| line.starts_with("new file mode ") || line == "--- /dev/null");
            let deletes = section
                .lines()
                .any(|line| line.starts_with("deleted file mode ") || line == "+++ /dev/null");
            let kind = if creates {
                FileChangeKind::Create
            } else if deletes {
                FileChangeKind::Delete
            } else if old_path != new_path {
                FileChangeKind::Rename
            } else {
                FileChangeKind::Modify
            };
            let path = match kind {
                FileChangeKind::Delete => old_path,
                _ => new_path.or(old_path),
            }
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "turn diff section has no file path".to_string())?;

            Ok(FileChange {
                path,
                kind,
                diff: Some(section),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Trusted native dispatch inputs, retained before presentation normalization.
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub native_execution: Option<Value>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    pub tool_call_id: Option<String>,
    pub id: String,
    pub turn_id: Option<String>,
    /// Spawn item of the subagent that asked. A background subagent outlives
    /// the turn that spawned it, so its request stays open past that turn's end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_item_id: Option<String>,
    pub kind: ApprovalKind,
    /// The provider's native choices for this request, in provider order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ApprovalOption>,
}

/// One choice offered by a provider's permission request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalOption {
    /// Opaque id echoed back in [`ApprovalDecision::Option`].
    pub id: String,
    pub label: String,
    pub kind: ApprovalOptionKind,
}

/// Native scope and disposition, for presentation and orchestration routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalKind {
    ExecCommand {
        command: String,
        cwd: Option<String>,
        reason: Option<String>,
    },
    /// A read-only file/search operation (Claude's `file_read_approval` family:
    /// Read/View/Grep/Glob/…/WebSearch). `detail` is the pre-rendered summary.
    FileRead { detail: String },
    FileChange {
        changes: Vec<FileChange>,
        reason: Option<String>,
    },
    /// Dynamic fallback for any tool that doesn't classify as command / file
    /// read / file change (agent, mcp, image, …). `detail` is the pre-rendered
    /// summary.
    ToolUse {
        name: String,
        input: serde_json::Value,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Codex aborts (`cancel`); Claude denies with "User cancelled tool execution";
    /// ACP reports cancelled; OpenCode replies reject.
    Cancel,
    /// Pick one of the provider's own [`ApprovalOption`]s.
    Option(String),
}

impl<'de> Deserialize<'de> for ApprovalDecision {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum StoredDecision {
            Cancel,
            Option(String),
            Approve,
            ApproveForSession,
            Deny,
        }
        Ok(match StoredDecision::deserialize(deserializer)? {
            StoredDecision::Cancel => Self::Cancel,
            StoredDecision::Option(id) => Self::Option(id),
            // These ids are history-only and never occur in live request options.
            StoredDecision::Approve => Self::Option("legacy:approve".into()),
            StoredDecision::ApproveForSession => Self::Option("legacy:approve_for_session".into()),
            StoredDecision::Deny => Self::Option("legacy:deny".into()),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextFreshness {
    /// Older logs lack provenance; their counts may be turn aggregates.
    #[default]
    Unknown,
    Current,
    LastKnown,
    Compacting,
    AwaitingObservation,
}

/// One `tcode_control` write, as the target thread records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlAction {
    pub actor_thread_id: String,
    pub actor_title: String,
    /// A semantic key from `tcode_core::control::action`, which clients put in words.
    pub action: String,
    /// Data only (a title, an id, a name, a count, someone's own words), never a sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Compaction {
    pub in_progress: bool,
    /// Terminal outcome when the provider reports more than completion.
    pub status: Option<String>,
    pub trigger: Option<String>,
    pub pre_tokens: Option<u64>,
    /// Provider-reported compaction output size; not a new request observation.
    pub post_tokens: Option<u64>,
    pub dropped_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
    pub summary: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenUsage {
    pub freshness: ContextFreshness,
    /// Main-loop traffic for the completed turn, separate from occupancy.
    pub turn_processed_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Total context currently in use, if the provider reports it.
    pub used_tokens: Option<u64>,
    pub context_window: Option<u64>,
    /// Cumulative tokens processed over the session's lifetime, if known (Codex
    /// `thread/tokenUsage` running total; timeline accumulation of Claude turns).
    /// Shown as "Total processed" in the context-meter popover.
    pub total_processed_tokens: Option<u64>,
    /// Provider-reported cost for this turn/message, in US dollars.
    pub cost_usd: Option<f64>,
    /// Provider-reported turn duration, in milliseconds.
    pub duration_ms: Option<u64>,
}

impl TokenUsage {
    #[cfg(feature = "process")]
    fn merge(&mut self, usage: Self) {
        self.freshness = usage.freshness;
        self.input_tokens = add_token_counts(self.input_tokens, usage.input_tokens);
        self.cached_input_tokens =
            add_token_counts(self.cached_input_tokens, usage.cached_input_tokens);
        self.cache_write_tokens = add_token_counts(self.cache_write_tokens, usage.cache_write_tokens);
        self.reasoning_tokens = add_token_counts(self.reasoning_tokens, usage.reasoning_tokens);
        self.turn_processed_tokens = add_token_counts(self.turn_processed_tokens, usage.turn_processed_tokens);
        self.output_tokens = add_token_counts(self.output_tokens, usage.output_tokens);
        self.used_tokens = usage.used_tokens.or(self.used_tokens);
        self.context_window = match (self.context_window, usage.context_window) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (left, right) => left.or(right),
        };
        self.total_processed_tokens = usage.total_processed_tokens;
        self.cost_usd = match (self.cost_usd, usage.cost_usd) {
            (None, None) => None,
            (left, right) => Some(left.unwrap_or(0.0) + right.unwrap_or(0.0)),
        };
        self.duration_ms = match (self.duration_ms, usage.duration_ms) {
            (Some(left), Some(right)) => Some(left.saturating_add(right)),
            (left, right) => left.or(right),
        };
    }
}

#[cfg(feature = "process")]
fn add_token_counts(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

#[cfg(feature = "process")]
fn json_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_f64().map(|number| number.max(0.0) as u64))
    })
}

#[cfg(test)]
mod launch_env_tests {
    use super::*;

    #[test]
    fn home_override_is_provider_specific_and_wins_over_configured_pairs() {
        for (provider, variable) in [
            (ProviderKind::ClaudeCode, Some("HOME")),
            (ProviderKind::Codex, Some("CODEX_HOME")),
            (ProviderKind::Pi, Some("PI_CODING_AGENT_DIR")),
            (ProviderKind::OpenCode, None),
            (ProviderKind::Acp, None),
        ] {
            let configured = vec![("FOO".to_owned(), "bar".to_owned())];
            let mut env = LaunchEnv {
                env: configured.clone(),
                home: None,
            };
            assert_eq!(env.pairs(provider), configured);
            assert!(LaunchEnv::default().pairs(provider).is_empty());
            if let Some(variable) = variable {
                env.env.push((variable.into(), "previous".into()));
            }
            let mut expected = env.env.clone();
            env.home = Some(PathBuf::from("/tmp/provider-profile"));
            if let Some(variable) = variable {
                expected.push((variable.into(), "/tmp/provider-profile".into()));
            }
            assert_eq!(env.pairs(provider), expected, "{provider:?}");
        }
    }
}

#[cfg(all(test, feature = "process"))]
mod resolve_binary_tests {
    use super::*;

    /// The documented Windows default, used when `PATHEXT` is unset.
    fn windows_pathext() -> Vec<String> {
        [".COM", ".EXE", ".BAT", ".CMD"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// Windows' "is executable": a regular file (no exec bit exists there).
    #[cfg(windows)]
    fn is_file(path: &std::path::Path) -> bool {
        path.is_file()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tcode-resolve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn explicit_paths_bypass_the_environment_path_search() {
        for path in [
            "/opt/custom/claude",
            r"C:\tools\claude.cmd",
            "C:/tools/claude.cmd",
            "relative/claude",
        ] {
            let explicit = std::path::Path::new(path);
            assert_eq!(
                resolve_binary(Some(explicit), "unrelated-name").unwrap(),
                explicit
            );
            assert_eq!(resolve_binary(None, path).unwrap(), explicit);
        }
    }

    #[test]
    fn missing_binary_reports_a_helpful_error() {
        let err = resolve_binary(None, "tcode-definitely-not-a-real-binary").unwrap_err();
        assert!(matches!(err, AgentError::Spawn(msg) if msg.contains("not found on PATH")));
    }

    #[cfg(unix)]
    #[test]
    fn unix_path_search_requires_executable_files_and_absolute_directories() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("unix");
        let bin = dir.join("foo");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("bar"), "not executable").unwrap();
        std::fs::set_permissions(dir.join("bar"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let suffixed = dir.join("shim.exe");
        std::fs::write(&suffixed, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&suffixed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let extensions = path_extensions();
        assert_eq!(
            find_in_dirs([dir.clone()], "foo", &extensions, is_executable),
            Some(bin)
        );
        assert_eq!(
            find_in_dirs([dir.clone()], "bar", &extensions, is_executable),
            None
        );
        assert_eq!(
            find_in_dirs([dir.clone()], "shim", &extensions, is_executable),
            None
        );
        assert_eq!(
            find_in_dirs([dir.clone()], "shim.exe", &extensions, is_executable),
            Some(suffixed)
        );
        assert_eq!(
            find_in_dirs([dir.clone()], ".", &extensions, is_executable),
            None
        );
        assert_eq!(
            find_in_dirs([PathBuf::from("relative")], "foo", &extensions, |_| panic!(
                "relative PATH entries must not be probed"
            )),
            None
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The resolved file name, lowercased: the extension comes from `PATHEXT`
    /// (conventionally uppercase: `.EXE`), while the file on disk is usually
    /// lowercase. Both Windows and macOS resolve that case-insensitively, so the
    /// assertions below compare names case-insensitively too.
    #[cfg(windows)]
    fn resolved_name(path: Option<PathBuf>) -> Option<String> {
        Some(path?.file_name()?.to_string_lossy().to_lowercase())
    }

    /// The real Windows shape: `claude` only ever exists as `claude.cmd`, so the
    /// bare join the old resolver did found nothing.
    // Relies on the Windows filesystem being case-insensitive: PATHEXT is
    // upper-case while npm writes `claude.cmd`. On a case-sensitive FS the
    // join cannot match, so this is a Windows-only behavior test. The
    // case-independent ordering/fallback logic is covered below on every OS.
    #[cfg(windows)]
    #[test]
    fn windows_resolves_a_cmd_shim_for_a_bare_name() {
        let dir = temp_dir("win-cmd");
        std::fs::write(dir.join("claude.cmd"), "@echo off\n").unwrap();

        let found = find_in_dirs([dir], "claude", &windows_pathext(), is_file);
        assert_eq!(resolved_name(found), Some("claude.cmd".to_string()));
    }

    /// PATHEXT order decides: `.EXE` beats `.CMD` when both exist.
    #[cfg(windows)]
    #[test]
    fn windows_honors_pathext_order_and_falls_back_to_the_bare_name() {
        let dir = temp_dir("win-order");
        std::fs::write(dir.join("codex.cmd"), "").unwrap();
        std::fs::write(dir.join("codex.exe"), "").unwrap();
        let found = find_in_dirs([dir], "codex", &windows_pathext(), is_file);
        assert_eq!(resolved_name(found), Some("codex.exe".to_string()));

        // An extensionless file is still found — last, after every PATHEXT try.
        let bare = temp_dir("win-bare");
        std::fs::write(bare.join("codex"), "").unwrap();
        let found = find_in_dirs([bare], "codex", &windows_pathext(), is_file);
        assert_eq!(resolved_name(found), Some("codex".to_string()));
    }

    /// A name that already has an extension is used verbatim (no PATHEXT loop):
    /// `claude.cmd` must not be probed as `claude.cmd.EXE`.
    #[test]
    fn windows_uses_an_explicit_extension_as_given() {
        assert_eq!(
            candidate_names("claude.cmd", &windows_pathext()),
            vec!["claude.cmd".to_string()]
        );
        assert_eq!(
            candidate_names("npm", &windows_pathext()),
            vec![
                "npm.COM".to_string(),
                "npm.EXE".to_string(),
                "npm.BAT".to_string(),
                "npm.CMD".to_string(),
                "npm".to_string(),
            ]
        );
    }
}

#[cfg(test)]
mod turn_diff_tests {
    use super::*;

    #[test]
    fn splits_multi_file_turn_diff_and_classifies_operations() {
        let diff = concat!(
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
            "diff --git a/new.txt b/new.txt\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/new.txt\n",
            "@@ -0,0 +1 @@\n",
            "+hello\n",
            "diff --git a/old.txt b/old.txt\n",
            "deleted file mode 100644\n",
            "--- a/old.txt\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-gone\n",
            "diff --git a/before.rs b/after.rs\n",
            "similarity index 100%\n",
            "rename from before.rs\n",
            "rename to after.rs\n",
        );

        let changes = file_changes_from_unified_diff(diff).unwrap();
        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0].path, "src/lib.rs");
        assert_eq!(changes[0].kind, FileChangeKind::Modify);
        assert_eq!(changes[1].path, "new.txt");
        assert_eq!(changes[1].kind, FileChangeKind::Create);
        assert_eq!(changes[2].path, "old.txt");
        assert_eq!(changes[2].kind, FileChangeKind::Delete);
        assert_eq!(changes[3].path, "after.rs");
        assert_eq!(changes[3].kind, FileChangeKind::Rename);
        assert!(changes[0].diff.as_deref().unwrap().contains("-old\n+new"));
        assert!(!changes[0].diff.as_deref().unwrap().contains("new.txt"));
    }

    #[test]
    fn empty_diff_is_an_exact_empty_snapshot_but_malformed_diff_is_rejected() {
        assert!(file_changes_from_unified_diff("\n").unwrap().is_empty());
        assert!(file_changes_from_unified_diff("--- a/file\n+++ b/file\n").is_err());
    }
}

#[cfg(test)]
mod thread_item_serde_tests {
    use super::*;

    #[test]
    fn cursor_and_grok_use_their_wire_names() {
        for (kind, wire) in [
            (ProviderKind::Cursor, r#""cursor""#),
            (ProviderKind::Grok, r#""grok""#),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ProviderKind>(wire).unwrap(), kind);
        }
    }

    #[test]
    fn provider_failure_wire_tag_and_legacy_error_remain_compatible() {
        let event = AgentEvent::ProviderStartFailed {
            error: "spawn failed".into(),
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"provider_start_failed","error":"spawn failed"}"#
        );
        let legacy = r#"{"type":"error","message":"boom","fatal":true}"#;
        assert!(
            matches!(serde_json::from_str::<AgentEvent>(legacy).unwrap(),
            AgentEvent::Error { message, fatal: true } if message == "boom")
        );
    }

    #[test]
    fn user_input_delivery_is_async_only_when_recorded() {
        let legacy = r#"{"type":"user_input_requested","request_id":"q","questions":[]}"#;
        assert!(matches!(
            serde_json::from_str::<AgentEvent>(legacy).unwrap(),
            AgentEvent::UserInputRequested {
                delivery: UserInputDelivery::Blocking,
                ..
            }
        ));
        let event = AgentEvent::UserInputRequested {
            request_id: "q".into(),
            questions: Vec::new(),
            delivery: UserInputDelivery::Async,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"user_input_requested","request_id":"q","questions":[],"delivery":"async"}"#
        );
    }

    #[test]
    fn stored_items_preserve_parent_links_and_read_legacy_records() {
        let legacy = r#"{"type":"item_completed","id":"old","content":{"kind":"user_message","text":"hello"}}"#;
        let decoded: AgentEvent = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            decoded,
            AgentEvent::ItemCompleted(ThreadItem {
                parent_item_id: None,
                ..
            })
        ));
        let parented = r#"{"type":"item_completed","id":"child","parent_item_id":"spawn","content":{"kind":"assistant_message","text":"working"}}"#;
        let decoded: AgentEvent = serde_json::from_str(parented).unwrap();
        assert!(matches!(
            &decoded,
            AgentEvent::ItemCompleted(ThreadItem { parent_item_id: Some(parent), .. })
                if parent == "spawn"
        ));
        assert_eq!(
            serde_json::to_value(decoded).unwrap()["parent_item_id"],
            "spawn"
        );
    }
}

#[cfg(test)]
mod usage_compatibility_tests {
    use super::*;

    #[test]
    fn compaction_and_usage_read_old_literal_records_without_inventing_provenance() {
        let event: AgentEvent = serde_json::from_str(r#"{"type":"context_compacted"}"#).unwrap();
        assert!(
            matches!(event, AgentEvent::ContextCompacted(c) if !c.in_progress && c.pre_tokens.is_none())
        );
        let usage: TokenUsage = serde_json::from_str(
            r#"{"used_tokens":4100000,"context_window":1000000,"total_processed_tokens":4100000}"#,
        )
        .unwrap();
        assert_eq!(usage.freshness, ContextFreshness::Unknown);
        assert_eq!(usage.total_processed_tokens, Some(4100000));
        // Existing readers use a unit variant with this tag. Extra fields are
        // additive and do not introduce an unknown protocol event tag.
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum LegacyEvent {
            ContextCompacted,
        }
        let _: LegacyEvent = serde_json::from_str(r#"{"type":"context_compacted","in_progress":false,"trigger":"manual","pre_tokens":500}"#).unwrap();
        let event = AgentEvent::ContextCompacted(Compaction {
            trigger: Some("manual".into()),
            pre_tokens: Some(500),
            ..Default::default()
        });
        let wire = serde_json::to_value(event).unwrap();
        assert_eq!(wire["type"], "context_compacted");
        assert_eq!(wire["pre_tokens"], 500);
    }
}

/// Parse a `MAJOR.MINOR.PATCH` triple from `--version` output such as
/// `"2.1.206 (Claude Code)"` or a bare manifest version string.
pub(crate) fn parse_semver(text: &str) -> Option<(u32, u32, u32)> {
    let token = text.split_whitespace().find(|token| token.contains('.'))?;
    let mut parts = token.trim_start_matches('v').split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts
            .next()
            .and_then(|part| part.split(['-', '+']).next())?
            .parse()
            .ok()?,
    ))
}

/// Pi/OMP native operations share a wire contract, separate from prompt text.
pub fn native_rpc_operation(engine:&str,name:&str,arguments:&str)->Result<(&'static str,Value),String>{
    let mut params=serde_json::json!({});
    let method=match name{
        "compact"=>{if !arguments.is_empty(){params["customInstructions"]=Value::String(arguments.into());}"compact"},
        "session"|"stats"=>"get_session_stats","context"=>"get_state","tree" if engine=="pi"=>"get_tree",
        "autocompact"|"autoretry"=>{if arguments.is_empty()&&name=="autocompact"{"get_state"}else{params["enabled"]=Value::Bool(match arguments.to_ascii_lowercase().as_str(){"on"=>true,"off"=>false,_=>return Err(format!("/{name} requires on or off"))});if name=="autocompact"{"set_auto_compaction"}else{"set_auto_retry"}}},
        _=>return Err(format!("Unsupported native operation /{name}"))
    };
    if !arguments.is_empty()&&!matches!(name,"compact"|"autocompact"|"autoretry"){return Err(format!("/{name} accepts no arguments"));}
    Ok((method,params))
}
pub fn native_rpc_result(method:&str,mut result:Value)->Value{
    if let Some(object)=result.as_object_mut(){object.remove("preserveData");}
    let context=match method{
        "get_session_stats"=>serde_json::json!({"used":result["contextUsage"]["tokens"],"limit":result["contextUsage"]["contextWindow"]}),
        "get_state"=>serde_json::json!({"limit":result["model"]["contextWindow"]}),
        "compact"=>serde_json::json!({"used":result["estimatedTokensAfter"]}),_=>Value::Null};
    let compact=if method=="compact"{serde_json::json!({"tokens_before":result["tokensBefore"],"tokens_after":result["estimatedTokensAfter"],"summary":result["shortSummary"]})}else{Value::Null};
    serde_json::json!({"status":"completed","result":result,"context":context,"compaction":compact})
}

/// Preserve a provider question's field contract before adapting its presentation.
pub fn native_question_contract(question:&Value)->Option<Value>{
    let mut wire_values=serde_json::Map::new();
    let choices=question["options"].as_array().map(|values|values.iter().filter_map(|option|{
        let value=option.get("value").or_else(||option.get("label")).or_else(||option.get("id")).or_else(||option.as_str().map(|_|option))?;
        let label=option["label"].as_str().or_else(||value.as_str()).map(str::to_owned).unwrap_or_else(||value.to_string());
        let key=value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string());wire_values.insert(key.clone(),value.clone());
        Some(serde_json::json!({"value":key,"label":label,"description":option["description"],"recommended":option["recommended"]==true}))
    }).collect::<Vec<_>>()).unwrap_or_default();
    let multiple=question["multiSelect"].as_bool().or_else(||question["multi_select"].as_bool()).unwrap_or(false);
    let declared=question["kind"].as_str().or_else(||question["type"].as_str()).unwrap_or("");
    let kind=if multiple||declared=="multi_select"{"multi_select"}else if !choices.is_empty(){"single_select"}else{match declared{"boolean"=>"boolean","integer"|"number"=>"number",_=>"free_text"}};
    let mut schema=question.get("requested_schema").or_else(||question.get("schema")).filter(|value|value.is_object()).cloned().unwrap_or(serde_json::json!({}));
    for key in ["type","enum","oneOf","minimum","maximum","exclusiveMinimum","exclusiveMaximum","multipleOf","minLength","maxLength","pattern","format","items","minItems","maxItems","uniqueItems"]{if let Some(value)=question.get(key){schema[key]=value.clone();}}
    if schema["type"].as_str().is_some_and(|kind|!matches!(kind,"string"|"boolean"|"integer"|"number"|"array"|"object")){schema.as_object_mut()?.remove("type");}
    if schema.as_object().is_some_and(|value|value.is_empty()){schema=Value::Null;}
    Some(serde_json::json!({"kind":kind,"required":question["required"].as_bool().unwrap_or(true),"allow_free_text":question["allow_free_text"].as_bool().or_else(||question["allowFreeformInput"].as_bool()).or_else(||question["custom"].as_bool()).unwrap_or(true),"options":choices,"wire_values":wire_values,"requested_schema":schema,"typed_answers":question["typed_answers"]==true}))
}

impl AgentEvent {
    /// Root-only consumers must not let a background child's terminal end the caller.
    pub fn root_event(&self)->Option<&Self>{match self{Self::Origin{agent_id:None,event,..}=>event.root_event(),Self::Origin{agent_id:Some(_),..}=>None,event=>Some(event)}}
    pub fn into_root_event(self)->Option<Self>{match self{Self::Origin{agent_id:None,event,..}=>event.into_root_event(),Self::Origin{agent_id:Some(_),..}=>None,event=>Some(event)}}
}
