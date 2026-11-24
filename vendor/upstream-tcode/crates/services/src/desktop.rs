use std::io;
use std::path::Path;

/// Command names the Zed CLI is installed under. Upstream's installer uses
/// `zed`, but on Linux that collides with the ZFS event daemon (also `zed`),
/// so several distributions rename the editor's binary — Zed's own packaging
/// docs suggest `zedit`, `zeditor`, or `zed-cli`. Try each in turn; the first
/// one present on PATH wins.
const ZED_CLI_CANDIDATES: &[&str] = &["zed", "zeditor", "zed-cli", "zedit"];

pub fn open_in_zed(cwd: &Path) -> io::Result<()> {
    open_with_candidates(ZED_CLI_CANDIDATES, cwd)
}

fn open_with_candidates(candidates: &[&str], cwd: &Path) -> io::Result<()> {
    let mut not_found = None;
    for cmd in candidates {
        match crate::process::command(cmd).arg(cwd).spawn() {
            Ok(child) => {
                drop(child);
                return Ok(());
            }
            // A missing binary just means this install uses a different name.
            Err(err) if err.kind() == io::ErrorKind::NotFound => not_found = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(not_found.unwrap_or_else(|| io::Error::from(io::ErrorKind::NotFound)))
}
