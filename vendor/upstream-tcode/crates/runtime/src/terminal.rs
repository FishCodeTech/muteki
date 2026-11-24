//! Live terminal workspace state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub use tcode_core::ui::TerminalSplitDirection;
pub use tcode_protocol::{
    TerminalContextStatus as TerminalContext, TerminalSplitStatus as TerminalSplit,
};

mod command_output;
mod projection;
pub(crate) use command_output::render as render_command_output;
pub(crate) use projection::{FRAME_INTERVAL, TerminalProjection, TerminalUpdate};

/// `TerminalDrawer` is a shared UI entity that swaps between conversations.
/// Globally unique tab ids prevent its geometry, selection, bell, and event
/// caches from aliasing two conversations whose first local tab would both be
/// `1`.
static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

pub struct TerminalEntry {
    pub id: u64,
    pub terminal: Arc<term::Terminal>,
}

pub struct TerminalWorkspace {
    pub terminals: Vec<TerminalEntry>,
    pub active_id: Option<u64>,
    pub splits: Vec<TerminalSplit>,
    pub contexts: Vec<TerminalContext>,
    next_context_id: u64,
}

/// Host-private index from terminal id to its live PTY, so command dispatch and
/// the grid projection can reach a terminal without walking every workspace.
/// No client ever sees this: the grid itself is replicated over the pipe.
#[derive(Clone, Default)]
pub(crate) struct TerminalRegistry {
    handles: Arc<RwLock<HashMap<u64, Arc<term::Terminal>>>>,
}

impl TerminalRegistry {
    pub(crate) fn replace_from<'a>(
        &self,
        workspaces: impl IntoIterator<Item = &'a TerminalWorkspace>,
    ) {
        let mut handles = self.handles.write().unwrap();
        handles.clear();
        for workspace in workspaces {
            for entry in &workspace.terminals {
                handles.insert(entry.id, entry.terminal.clone());
            }
        }
    }

    pub(crate) fn terminal(&self, id: u64) -> Option<Arc<term::Terminal>> {
        self.handles.read().unwrap().get(&id).cloned()
    }
}

impl Default for TerminalWorkspace {
    fn default() -> Self {
        Self {
            terminals: Vec::new(),
            active_id: None,
            splits: Vec::new(),
            contexts: Vec::new(),
            next_context_id: 1,
        }
    }
}

impl TerminalWorkspace {
    pub fn active(&self) -> Option<&TerminalEntry> {
        let id = self.active_id?;
        self.terminals.iter().find(|entry| entry.id == id)
    }

    pub fn terminal(&self, id: u64) -> Option<&TerminalEntry> {
        self.terminals.iter().find(|entry| entry.id == id)
    }

    /// Add and activate a terminal with a process-wide unique tab id.
    pub fn push(&mut self, terminal: term::Terminal) -> u64 {
        let id = NEXT_TERMINAL_ID.fetch_add(1, Ordering::Relaxed);
        self.terminals.push(TerminalEntry {
            id,
            terminal: Arc::new(terminal),
        });
        self.active_id = Some(id);
        id
    }

    pub fn split_for(&self, terminal_id: u64) -> Option<TerminalSplit> {
        self.splits
            .iter()
            .copied()
            .find(|split| split.first == terminal_id || split.second == terminal_id)
    }

    pub fn add_context(&mut self, label: String, selection: term::SelectedText) {
        let id = self.next_context_id;
        self.next_context_id += 1;
        self.contexts.push(TerminalContext {
            id,
            terminal_label: label,
            line_start: selection.line_start,
            line_end: selection.line_end,
            text: selection.text,
        });
    }
}
