//! Workspace file/folder listing for the `@`-mention popover.
//!
//! Prefers `git ls-files` (cached + untracked, gitignore-respected) when the
//! session cwd is a git repo; otherwise falls back to a bounded recursive walk
//! that skips common build and dependency directories.

use std::collections::BTreeSet;
use std::path::Path;
pub use tcode_protocol::PathEntry;

/// Entry cap for both listing paths; depth cap for the filesystem fallback.
const MAX_ENTRIES: usize = 8000;
const MAX_DEPTH: usize = 8;
/// Directories the fallback walk never descends into.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
    ".cache",
];

/// List workspace files and folders under `cwd`. Blocking — call via
/// `smol::unblock`. Empty when `cwd` is unreadable.
pub fn list_workspace(cwd: &Path) -> Vec<PathEntry> {
    if let Some(entries) = list_from_git(cwd) {
        return entries;
    }
    list_from_walk(cwd)
}

/// `git ls-files --cached --others --exclude-standard`, then synthesize the set
/// of parent directories as folder entries. Returns `None` when not a repo or
/// git is unavailable.
fn list_from_git(cwd: &Path) -> Option<Vec<PathEntry>> {
    let output = crate::process::command("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(entries_from_relpaths(&text))
}

/// Parse newline-separated relative file paths into file + derived-folder entries.
fn entries_from_relpaths(text: &str) -> Vec<PathEntry> {
    let mut files: Vec<PathEntry> = Vec::new();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for line in text.lines() {
        let rel = line.trim();
        if rel.is_empty() {
            continue;
        }
        // Accumulate every ancestor directory of this file.
        let mut acc = String::new();
        let parts: Vec<&str> = rel.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            dirs.insert(acc.clone());
        }
        files.push(PathEntry::from_rel(rel.to_string(), false));
        if files.len() + dirs.len() >= MAX_ENTRIES {
            break;
        }
    }
    let mut entries: Vec<PathEntry> = dirs
        .into_iter()
        .map(|d| PathEntry::from_rel(d, true))
        .collect();
    entries.extend(files);
    entries
}

/// Bounded recursive walk fallback (non-git workspaces).
fn list_from_walk(cwd: &Path) -> Vec<PathEntry> {
    let mut entries = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(cwd.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH || entries.len() >= MAX_ENTRIES {
            continue;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(cwd) else {
                continue;
            };
            let rel_path = rel.to_string_lossy().replace('\\', "/");
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push(PathEntry::from_rel(rel_path, is_dir));
            if is_dir {
                stack.push((path, depth + 1));
            }
            if entries.len() >= MAX_ENTRIES {
                break;
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_listing_respects_gitignore_and_falls_back_to_filtered_filesystem_walk() {
        let root = std::env::temp_dir().join(format!("tcode-workspace-{}", uuid::Uuid::new_v4()));
        for directory in ["src/ui", "node_modules", "target"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        for file in [
            "src/main.rs",
            "src/ui/composer.rs",
            "README.md",
            "node_modules/dependency.js",
            "target/output",
        ] {
            std::fs::write(root.join(file), "fixture").unwrap();
        }
        for git in [false, true] {
            if git {
                let output = crate::process::command("git")
                    .args(["-c", "init.templateDir=", "init", "-b", "main"])
                    .current_dir(&root)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                std::fs::write(
                    root.join(".gitignore"),
                    "node_modules/\ntarget/\n.gitignore\n",
                )
                .unwrap();
            }
            let mut entries = list_workspace(&root);
            entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| (entry.rel_path.as_str(), entry.is_dir))
                    .collect::<Vec<_>>(),
                [
                    ("README.md", false),
                    ("src", true),
                    ("src/main.rs", false),
                    ("src/ui", true),
                    ("src/ui/composer.rs", false),
                ],
                "git={git}"
            );
            let composer = entries
                .iter()
                .find(|entry| entry.rel_path == "src/ui/composer.rs")
                .unwrap();
            assert_eq!(composer.basename, "composer.rs");
            assert_eq!(composer.parent, "src/ui");
        }
        std::fs::remove_dir_all(&root).unwrap();
        assert!(list_workspace(&root).is_empty());
    }
}
