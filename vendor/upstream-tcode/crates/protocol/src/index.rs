use base64::Engine as _;
use sha2::{Digest, Sha256};
use tcode_core::project::{Project, SessionMeta};

use crate::IndexSummary;

/// Hash the applied replica, independently of map/set iteration and session
/// insertion order. Project order is part of the sidebar projection.
/// The archive invalidation token is carried separately by IndexUnchanged:
/// dropping a space's last subscription can advance it without changing rows.
/// Called only when restoring an index subscription, never on live updates.
pub fn index_fingerprint(
    summary: &IndexSummary,
    sessions: &[SessionMeta],
    projects: &[Project],
) -> Result<String, serde_json::Error> {
    let mut sessions: Vec<_> = sessions.iter().collect();
    sessions.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    let mut value = serde_json::to_value((summary, sessions, projects))?;
    for field in ["title_generating", "worktree_shared"] {
        if let Some(values) = value[0][field].as_array_mut() {
            values.sort_unstable_by(|a, b| a.as_str().cmp(&b.as_str()));
        }
    }
    if let Some(summary) = value[0].as_object_mut() {
        summary.remove("archived_revision");
    }
    value.sort_all_objects();
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(serde_json::to_vec(&value)?)))
}
