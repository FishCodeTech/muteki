//! Persisted Agent Client Protocol configuration.

use agent::AcpLaunch;
use serde::{Deserialize, Serialize};

/// An installed agent, as persisted in settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstalledAcpAgent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub icon: Option<String>,
    /// The resolved recipe: exactly what we will spawn.
    pub launch: AcpLaunch,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Extra environment for this agent's process (Settings → Providers).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Extra CLI arguments appended at launch.
    #[serde(default)]
    pub launch_args: Option<String>,
}

/// Serializable edits the UI can make to an installed ACP agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcpAgentPatch {
    SetEnabled {
        enabled: bool,
    },
    SetLaunchOptions {
        env: Vec<(String, String)>,
        launch_args: Option<String>,
    },
}

fn default_true() -> bool {
    true
}

impl InstalledAcpAgent {
    /// Whether new sessions may use this agent. An agent that duplicates a
    /// native integration stays installed, so its existing sessions resume and
    /// it can be removed, but new sessions use the native provider.
    pub fn offered_for_new_sessions(&self) -> bool {
        !agent::HIDDEN_ACP_AGENT_IDS.contains(&self.id.as_str())
    }

    /// The whitespace-split launch arguments (mirrors Claude's "Launch arguments").
    pub fn extra_args(&self) -> Vec<String> {
        self.launch_args
            .as_deref()
            .map(|args| args.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }
}
