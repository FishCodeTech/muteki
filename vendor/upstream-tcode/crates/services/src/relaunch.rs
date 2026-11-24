//! Restart-continuity marker for macOS permission grants that need a relaunch.
//!
//! macOS applies some TCC grants (notably Screen Recording) only after the app
//! restarts, and may quit tcode from its own "Quit & Reopen" dialog. Before any
//! Screen Recording permission flow, the app drops a small `relaunch.json`
//! marker into the data dir recording which Settings page to reopen and which
//! session was active. A denied flow clears the marker when the app becomes
//! active again. On the next launch any remaining marker is *taken* (read +
//! deleted) so the app can reopen the session, reopen Settings on the recorded
//! page, and recheck.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A pending restart's continuity state. Written before a grant/relaunch and
/// consumed exactly once at the next startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaunchMarker {
    /// Which Settings page to reopen: `"computer_use"` or `"browser"`.
    pub reopen_settings: String,
    /// The session that was active when the marker was written, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_session: Option<String>,
}

/// The marker file inside the data dir.
fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join("relaunch.json")
}

/// Start a fresh tcode with `args` and return; the caller quits afterwards.
///
/// This is the arg-carrying sibling of `computer_use_mcp::permissions::
/// relaunch_app`, which needs LaunchServices to preserve the `.app` bundle's
/// TCC identity. Remote mode switching needs to pass `--connect <host_id>`
/// instead, so it goes through `open -n --args` (macOS) or the bare executable.
pub fn spawn_with_args(args: &[String]) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let bundle = exe
        .ancestors()
        .find(|path| path.extension().is_some_and(|ext| ext == "app"));
    match bundle {
        Some(app) if cfg!(target_os = "macos") => {
            let mut command = crate::process::command("open");
            command.arg("-n").arg(app);
            if !args.is_empty() {
                command.arg("--args").args(args);
            }
            command.spawn()?;
        }
        _ => {
            crate::process::command(exe).args(args).spawn()?;
        }
    }
    Ok(())
}

/// Persist the marker, overwriting any previous one.
pub fn write(data_dir: &Path, marker: &RelaunchMarker) -> std::io::Result<()> {
    let data = serde_json::to_vec_pretty(marker)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(marker_path(data_dir), data)
}

/// Discard a pending marker. Missing markers are already clear.
pub fn clear(data_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(marker_path(data_dir)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Whether a marker is waiting, without consuming it: the next instance is
/// already starting while the one that wrote it is still closing.
pub fn pending(data_dir: &Path) -> bool {
    marker_path(data_dir).is_file()
}

/// Read the marker and delete it (consume-once). Returns `None` when absent or
/// unparsable; the file is removed either way so a corrupt marker can't wedge
/// every future launch into a relaunch loop.
pub fn take(data_dir: &Path) -> Option<RelaunchMarker> {
    let path = marker_path(data_dir);
    let bytes = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_marker_is_consumed_once_and_cancelled_or_corrupt_markers_are_discarded() {
        let root = std::env::temp_dir().join(format!("tcode-relaunch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(take(&root), None);
        clear(&root).unwrap();

        let marker = RelaunchMarker {
            reopen_settings: "computer_use".into(),
            active_session: Some("sess-42".into()),
        };
        write(&root, &marker).unwrap();
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("relaunch.json")).unwrap()).unwrap();
        assert_eq!(
            persisted,
            serde_json::json!({
                "reopen_settings": "computer_use", "active_session": "sess-42"
            })
        );
        assert_eq!(take(&root), Some(marker.clone()));
        assert_eq!(take(&root), None);

        write(&root, &marker).unwrap();
        clear(&root).unwrap();
        assert_eq!(take(&root), None);
        std::fs::write(root.join("relaunch.json"), b"not json").unwrap();
        assert_eq!(take(&root), None);
        assert!(!root.join("relaunch.json").exists());

        std::fs::write(
            root.join("relaunch.json"),
            br#"{"reopen_settings":"browser"}"#,
        )
        .unwrap();
        assert_eq!(
            take(&root),
            Some(RelaunchMarker {
                reopen_settings: "browser".into(),
                active_session: None,
            })
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
