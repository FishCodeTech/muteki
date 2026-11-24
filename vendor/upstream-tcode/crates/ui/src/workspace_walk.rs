//! Workspace-entry filtering for the `@`-mention popover.

use std::path::Path;

use tcode_protocol::PathEntry;

/// Shorten a workspace path for display.
///
/// Both `path` and `cwd` belong to the *host*, so this is pure `std::path`
/// string work: canonicalizing here would resolve a remote host's path against
/// the client's own filesystem and produce a path that exists on neither.
pub fn relativize_to_workspace(path: &str, cwd: &Path) -> String {
    Path::new(path)
        .strip_prefix(cwd)
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

/// A ranked filter over workspace entries (case-insensitive), capped at `limit`.
/// Basename matches rank above path-only matches; a basename prefix match ranks
/// first. Empty query returns the first `limit` entries.
pub fn filter_entries<'a>(
    entries: &'a [PathEntry],
    query: &str,
    limit: usize,
) -> Vec<&'a PathEntry> {
    if query.is_empty() {
        return entries.iter().take(limit).collect();
    }
    let q = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &PathEntry)> = Vec::new();
    for entry in entries {
        let base = entry.basename.to_lowercase();
        let path = entry.rel_path.to_lowercase();
        let rank = if base.starts_with(&q) {
            0
        } else if base.contains(&q) {
            1
        } else if path.contains(&q) {
            2
        } else {
            continue;
        };
        scored.push((rank, entry.rel_path.len(), entry));
    }
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then(a.2.rel_path.cmp(&b.2.rel_path))
    });
    scored.into_iter().take(limit).map(|(_, _, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_results_rank_basename_before_path_and_apply_the_limit_after_sorting() {
        let entries = [
            "src/compo/other.rs",
            "z/composer.rs",
            "docs/decompose.md",
            "a/composer.rs",
            "src/ui/composer_trigger.rs",
            "unrelated.rs",
        ]
        .map(|path| PathEntry {
            rel_path: path.into(),
            basename: path.rsplit('/').next().unwrap().into(),
            parent: String::new(),
            is_dir: false,
        });
        let paths = |query, limit| {
            filter_entries(&entries, query, limit)
                .into_iter()
                .map(|entry| entry.rel_path.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            paths("COMPO", 10),
            [
                "a/composer.rs",
                "z/composer.rs",
                "src/ui/composer_trigger.rs",
                "docs/decompose.md",
                "src/compo/other.rs",
            ]
        );
        assert_eq!(paths("compo", 2), ["a/composer.rs", "z/composer.rs"]);
        assert_eq!(paths("", 2), ["src/compo/other.rs", "z/composer.rs"]);
        assert!(paths("compo", 0).is_empty());
        assert!(paths("missing", 10).is_empty());
    }
}
