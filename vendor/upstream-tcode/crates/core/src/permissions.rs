//! Computer-use permission facts, reported by the machine running the agent.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    Accessibility,
    ScreenRecording,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionStatus {
    pub accessibility: bool,
    pub screen_recording: bool,
}

impl PermissionStatus {
    pub fn granted(&self, kind: PermissionKind) -> bool {
        match kind {
            PermissionKind::Accessibility => self.accessibility,
            PermissionKind::ScreenRecording => self.screen_recording,
        }
    }

    pub fn all_granted(&self) -> bool {
        self.accessibility && self.screen_recording
    }
}

/// Platform semantics are supplied by the host, never inferred by a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", content = "status", rename_all = "snake_case")]
pub enum ComputerUsePermissions {
    MacOs(PermissionStatus),
    NotRequired,
    Unsupported,
}
