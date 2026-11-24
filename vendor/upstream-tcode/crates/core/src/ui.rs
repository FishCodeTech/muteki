//! Pure UI/host boundary state shared by the runtime and protocol.

use serde::{Deserialize, Serialize};

pub const MAX_TERMINALS_PER_SESSION: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalSplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceMode {
    #[default]
    LocalCheckout,
    NewWorktree {
        base: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RightTab {
    #[default]
    Diff,
    Plan,
    Agents,
    Preview,
    PullRequests,
}

/// Stable identity for client-owned state attached to a conversation surface.
///
/// Stored threads follow their session id. An unsent draft follows its project
/// because reopening that project's New thread surface creates a fresh
/// transient session id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ConversationDestination {
    Thread(String),
    ProjectDraft(String),
}

impl ConversationDestination {
    /// Backward-compatible key for persisted per-conversation preferences.
    pub fn preference_key(&self) -> String {
        match self {
            Self::Thread(id) => id.clone(),
            Self::ProjectDraft(id) => format!("draft:{id}"),
        }
    }
}
