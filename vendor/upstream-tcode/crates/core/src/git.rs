//! Git quick-actions for the local, single-SCM workflow. This excludes PR/MR
//! operations and publish-to-provider wizards.
//!
//! This core owns status parsing, the adaptive quick-action state machine,
//! path-spec selection, slug generation, and prompt builders. Process-backed
//! execution lives in `crates/services/src/git.rs`.

use std::collections::HashSet;

use agent::FileChange;

/// Collapse structured file-change operations into one change per path.
///
/// Providers without a canonical turn diff use this as a partial summary.
/// Concatenating fragments preserves every reported operation, but overlapping
/// edits can make the resulting line statistics non-net.
pub fn merge_file_changes_by_path<'a>(
    changes: impl IntoIterator<Item = &'a FileChange>,
) -> Vec<FileChange> {
    let mut merged: Vec<FileChange> = Vec::new();
    for change in changes {
        if let Some(existing) = merged.iter_mut().find(|item| item.path == change.path) {
            existing.kind = change.kind;
            if let Some(fragment) = &change.diff {
                let diff = existing.diff.get_or_insert_with(String::new);
                if !diff.is_empty() && !diff.ends_with('\n') {
                    diff.push('\n');
                }
                diff.push_str(fragment);
            }
        } else {
            merged.push(change.clone());
        }
    }
    merged
}

/// One changed file in the working tree (staged and/or unstaged), with its
/// combined line delta (0/0 for untracked files with no numstat).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitFileEntry {
    pub path: String,
    pub insertions: u32,
    pub deletions: u32,
}

/// A snapshot of the repository state needed by the adaptive quick-action
/// button.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct GitStatus {
    /// `cwd` is inside a git working tree.
    pub is_repo: bool,
    /// `HEAD` resolves to a commit (false for a fresh `git init` with no commit).
    pub has_commits: bool,
    /// Detached HEAD (no current branch).
    pub detached: bool,
    /// Current branch name (`None` when detached / no branch).
    pub branch: Option<String>,
    /// The branch is the repo's default ref (`origin/HEAD`, else main/master).
    pub is_default_branch: bool,
    /// The working tree has staged or unstaged changes (or untracked files).
    pub has_working_tree_changes: bool,
    /// An `origin` remote is configured.
    pub has_origin_remote: bool,
    /// The current branch has a configured upstream.
    pub has_upstream: bool,
    /// Commits ahead of upstream.
    pub ahead: u32,
    /// Commits behind upstream.
    pub behind: u32,
    /// Changed files (for the commit dialog list).
    pub changed_files: Vec<GitFileEntry>,
    /// The GitHub repository a bare pull request number refers to, resolved as gh does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_repository: Option<String>,
}

impl GitStatus {
    /// The branch diverged from its upstream (both ahead and behind) — a
    /// fast-forward pull is impossible, so Pull is offered disabled.
    pub fn diverged(&self) -> bool {
        self.ahead > 0 && self.behind > 0
    }
}

/// An executable git operation behind the quick-action button / dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GitAction {
    /// Open the commit dialog, then `git commit` the selected files.
    Commit,
    /// Open the commit dialog, then commit and `git push`.
    CommitPush,
    /// `git push` existing local commits.
    Push,
    /// `git pull --ff-only`.
    Pull,
    /// `git push -u origin <branch>` (set upstream for a branch that has none).
    PublishBranch,
    /// `git init` in a non-repo cwd.
    InitializeGit,
}

impl GitAction {
    /// Whether triggering this action opens the commit dialog first.
    pub fn opens_commit_dialog(self) -> bool {
        matches!(self, GitAction::Commit | GitAction::CommitPush)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHint {
    InProgress,
    Detached,
    NoCommits,
    NoRemote,
    Diverged,
    UpToDate,
    NoChanges,
    NoUpstream,
    Behind,
    NoAhead,
}

/// The resolved state of the adaptive quick-action button. `action` is `None`
/// when the button is a disabled status hint; `label` remains the semantic
/// action whose label the disabled button displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickAction {
    pub action: Option<GitAction>,
    pub label: GitAction,
    pub hint: Option<GitHint>,
    pub disabled: bool,
}

impl QuickAction {
    fn run(action: GitAction) -> Self {
        Self {
            action: Some(action),
            label: action,
            hint: None,
            disabled: false,
        }
    }

    fn hint(label: GitAction, hint: GitHint) -> Self {
        Self {
            action: None,
            label,
            hint: Some(hint),
            disabled: true,
        }
    }
}

/// Resolve the primary quick-action for `status`. `is_busy` is true while an
/// action is already running (the button is disabled with an in-progress hint).
///
/// PR/MR and publish-repository workflows are out of scope; a branch without
/// an upstream instead resolves to `PublishBranch` (push `-u`).
pub fn quick_action(status: &GitStatus, is_busy: bool) -> QuickAction {
    if is_busy {
        return QuickAction::hint(GitAction::Commit, GitHint::InProgress);
    }
    if !status.is_repo {
        return QuickAction::run(GitAction::InitializeGit);
    }
    if status.detached {
        return QuickAction::hint(GitAction::Commit, GitHint::Detached);
    }
    let dirty = status.has_working_tree_changes;
    // A fresh repo with no commit yet: the only thing to do is the first commit.
    if !status.has_commits {
        if dirty {
            return QuickAction::run(GitAction::Commit);
        }
        return QuickAction::hint(GitAction::Commit, GitHint::NoCommits);
    }
    if dirty {
        if status.has_upstream {
            return QuickAction::run(GitAction::CommitPush);
        }
        return QuickAction::run(GitAction::Commit);
    }
    // Clean working tree.
    if !status.has_upstream {
        if status.has_origin_remote && status.branch.is_some() {
            return QuickAction::run(GitAction::PublishBranch);
        }
        return QuickAction::hint(GitAction::Commit, GitHint::NoRemote);
    }
    if status.diverged() {
        return QuickAction::hint(GitAction::Pull, GitHint::Diverged);
    }
    if status.behind > 0 {
        return QuickAction::run(GitAction::Pull);
    }
    if status.ahead > 0 {
        return QuickAction::run(GitAction::Push);
    }
    QuickAction::hint(GitAction::Commit, GitHint::UpToDate)
}

/// One entry of the quick-action dropdown (the applicable subset for `status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub action: GitAction,
    pub disabled: bool,
    pub hint: Option<GitHint>,
}

/// Build the quick-action dropdown items for `status`. Offers the actions that
/// make sense for the repository shape and gives disabled actions a reason.
pub fn menu_items(status: &GitStatus, is_busy: bool) -> Vec<MenuItem> {
    if !status.is_repo {
        return vec![MenuItem {
            action: GitAction::InitializeGit,
            disabled: is_busy,
            hint: None,
        }];
    }

    let mut items = Vec::new();
    let dirty = status.has_working_tree_changes;
    let can_commit = !is_busy && dirty && !status.detached;
    items.push(MenuItem {
        action: GitAction::Commit,
        disabled: !can_commit,
        hint: (!can_commit).then(|| commit_disabled_hint(status)),
    });

    // Push: needs commits ahead of a configured upstream, clean tree.
    let can_push = !is_busy
        && status.has_upstream
        && status.ahead > 0
        && status.behind == 0
        && !status.detached;
    items.push(MenuItem {
        action: GitAction::Push,
        disabled: !can_push,
        hint: (!can_push).then(|| push_disabled_hint(status)),
    });

    // Pull: behind a configured upstream, fast-forwardable.
    let can_pull = !is_busy && status.has_upstream && status.behind > 0 && !status.diverged();
    if status.has_upstream {
        items.push(MenuItem {
            action: GitAction::Pull,
            disabled: !can_pull,
            hint: (!can_pull).then(|| pull_disabled_hint(status)),
        });
    }

    // Publish branch: no upstream yet but an origin remote exists.
    if !status.has_upstream && status.has_origin_remote {
        let can_publish = !is_busy && status.branch.is_some();
        items.push(MenuItem {
            action: GitAction::PublishBranch,
            disabled: !can_publish,
            hint: (!can_publish).then_some(GitHint::Detached),
        });
    }

    items
}

fn commit_disabled_hint(status: &GitStatus) -> GitHint {
    if status.detached {
        GitHint::Detached
    } else {
        GitHint::NoChanges
    }
}

fn push_disabled_hint(status: &GitStatus) -> GitHint {
    if status.detached {
        GitHint::Detached
    } else if !status.has_upstream {
        GitHint::NoUpstream
    } else if status.diverged() {
        GitHint::Diverged
    } else if status.behind > 0 {
        GitHint::Behind
    } else {
        GitHint::NoAhead
    }
}

fn pull_disabled_hint(status: &GitStatus) -> GitHint {
    if status.diverged() {
        GitHint::Diverged
    } else {
        GitHint::UpToDate
    }
}

/// The path-spec to stage for a commit given the changed files and the set of
/// user-*excluded* (unchecked) paths.
///
/// Returns `None` when nothing is excluded (stage everything: `git add -A`),
/// otherwise `Some(included)` — the checked subset, staged explicitly so
/// unchecked files are left out of the commit.
pub fn included_paths(all: &[GitFileEntry], excluded: &HashSet<String>) -> Option<Vec<String>> {
    if excluded.is_empty() {
        return None;
    }
    Some(
        all.iter()
            .filter(|f| !excluded.contains(&f.path))
            .map(|f| f.path.clone())
            .collect(),
    )
}

/// Sanitize an arbitrary string into a lowercase git ref fragment: strip
/// quotes, collapse separators, and cap at 48 chars. Falls back to `"update"`
/// when empty.
pub fn sanitize_branch_fragment(raw: &str) -> String {
    let is_valid =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '/' | '_' | '-');
    let is_edge_trim = |c: char| matches!(c, '.' | '/' | '_' | '-' | ' ' | '\t' | '\n' | '\r');

    // Trim, lowercase, drop quotes, then trim separator-ish edges.
    let lowered: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '`'))
        .collect();
    let normalized = lowered.trim_matches(is_edge_trim);

    // Replace runs of invalid chars with a single '-'.
    let mut out = String::with_capacity(normalized.len());
    let mut in_invalid_run = false;
    for ch in normalized.chars() {
        if is_valid(ch) {
            out.push(ch);
            in_invalid_run = false;
        } else if !in_invalid_run {
            out.push('-');
            in_invalid_run = true;
        }
    }

    // Collapse runs of '/' and of '-'; underscores are preserved.
    let mut collapsed = String::with_capacity(out.len());
    let mut prev: Option<char> = None;
    for ch in out.chars() {
        if matches!(ch, '/' | '-') && prev == Some(ch) {
            continue;
        }
        collapsed.push(ch);
        prev = Some(ch);
    }

    let trimmed = collapsed.trim_matches(|c| matches!(c, '.' | '/' | '_' | '-'));
    let capped: String = trimmed.chars().take(48).collect();
    let capped = capped.trim_end_matches(['.', '/', '_', '-']);
    if capped.is_empty() {
        "update".to_string()
    } else {
        capped.to_string()
    }
}

/// The feature-branch name for the default-branch safeguard "create a feature
/// branch" path: `tcode/<slug>` derived from the commit subject.
pub fn feature_branch_name(subject: &str) -> String {
    format!("tcode/{}", sanitize_branch_fragment(subject))
}

/// Maximum UTF-8 bytes of patch content included in a commit-message prompt.
const COMMIT_PATCH_MAX: usize = 8_000;

/// Build the plain-text prompt for the headless commit-message generation
/// (`claude -p`). Includes the diff stat and a truncated patch.
pub fn build_commit_prompt(stat: &str, patch: &str) -> String {
    let patch = if patch.len() > COMMIT_PATCH_MAX {
        let mut cut = COMMIT_PATCH_MAX;
        while !patch.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n… (patch truncated)", &patch[..cut])
    } else {
        patch.to_string()
    };
    format!(
        "You write concise git commit messages.\n\
         Write a single conventional commit message for the staged changes below.\n\
         Rules:\n\
         - subject line must be imperative mood, at most 72 characters, no trailing period\n\
         - optionally add a short body after a blank line\n\
         - output ONLY the commit message text — no explanation, no code fences\n\
         \n\
         Diff stat:\n{stat}\n\
         \n\
         Patch:\n{patch}\n",
    )
}

/// Sanitize a model-produced commit message: strip surrounding code fences,
/// trim, and cap the subject line at 72 chars. Empty output falls back to the
/// caller (which re-generates), so an empty string is returned as-is.
pub fn sanitize_commit_message(raw: &str) -> String {
    let mut text = raw.trim();
    // Strip a leading/trailing ``` fence block if present.
    if let Some(rest) = text.strip_prefix("```") {
        // Drop the fence's info line.
        let rest = rest.split_once('\n').map(|x| x.1).unwrap_or("");
        text = rest.trim_end().strip_suffix("```").unwrap_or(rest).trim();
    }
    let mut lines = text.lines();
    let Some(subject_raw) = lines.next() else {
        return String::new();
    };
    let subject: String = subject_raw
        .trim()
        .trim_end_matches('.')
        .chars()
        .take(72)
        .collect();
    let subject = subject.trim().to_string();
    let body: Vec<&str> = lines.collect();
    let body = body.join("\n");
    let body = body.trim();
    if subject.is_empty() {
        return String::new();
    }
    if body.is_empty() {
        subject
    } else {
        format!("{subject}\n\n{body}")
    }
}

/// Parse `git status --porcelain=2 --branch` output plus the numstat map into
/// the local part of a [`GitStatus`]. `default_branch` is the short name from
/// `origin/HEAD` (or `None`); `has_origin_remote` from `git remote`.
///
/// This parser assumes a repository and sets `is_repo` to true.
pub fn parse_status(
    porcelain: &str,
    numstat: &[(String, u32, u32)],
    default_branch: Option<&str>,
    has_origin_remote: bool,
) -> GitStatus {
    let mut branch: Option<String> = None;
    let mut detached = false;
    let mut has_commits = true;
    let mut has_upstream = false;
    let mut ahead = 0u32;
    let mut behind = 0u32;
    let mut has_working_tree_changes = false;
    let mut paths: Vec<String> = Vec::new();

    for line in porcelain.lines() {
        if let Some(value) = line.strip_prefix("# branch.oid ") {
            if value.trim() == "(initial)" {
                has_commits = false;
            }
        } else if let Some(value) = line.strip_prefix("# branch.head ") {
            let value = value.trim();
            if value == "(detached)" {
                detached = true;
                branch = None;
            } else {
                branch = Some(value.to_string());
            }
        } else if let Some(value) = line.strip_prefix("# branch.upstream ") {
            has_upstream = !value.trim().is_empty();
        } else if let Some(value) = line.strip_prefix("# branch.ab ") {
            let (a, b) = parse_branch_ab(value.trim());
            ahead = a;
            behind = b;
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            has_working_tree_changes = true;
            if let Some(path) = parse_porcelain_path(line)
                && !paths.contains(&path)
            {
                paths.push(path);
            }
        }
    }

    let numstat_map: std::collections::HashMap<&str, (u32, u32)> = numstat
        .iter()
        .map(|(p, i, d)| (p.as_str(), (*i, *d)))
        .collect();
    paths.sort();
    let changed_files = paths
        .into_iter()
        .map(|path| {
            let (insertions, deletions) = numstat_map.get(path.as_str()).copied().unwrap_or((0, 0));
            GitFileEntry {
                path,
                insertions,
                deletions,
            }
        })
        .collect();

    let is_default_branch = match (&branch, default_branch) {
        (Some(b), Some(d)) => b == d,
        (Some(b), None) => b == "main" || b == "master",
        _ => false,
    };

    GitStatus {
        is_repo: true,
        has_commits,
        detached,
        branch,
        is_default_branch,
        has_working_tree_changes,
        has_origin_remote,
        has_upstream,
        ahead,
        behind,
        changed_files,
        github_repository: None,
    }
}

/// Parse `+A -B` from a `# branch.ab` value.
fn parse_branch_ab(value: &str) -> (u32, u32) {
    let mut ahead = 0;
    let mut behind = 0;
    for token in value.split_whitespace() {
        if let Some(rest) = token.strip_prefix('+') {
            ahead = rest.parse().unwrap_or(0);
        } else if let Some(rest) = token.strip_prefix('-') {
            behind = rest.parse().unwrap_or(0);
        }
    }
    (ahead, behind)
}

/// Extract the file path from a porcelain=2 changed-entry line.
fn parse_porcelain_path(line: &str) -> Option<String> {
    let mut chars = line.chars();
    let kind = chars.next()?;
    match kind {
        // Untracked / ignored: "? path" or "! path".
        '?' | '!' => Some(line[2..].trim().to_string()),
        // Ordinary "1 ..." (8 metadata fields then path) or rename "2 ..."
        // (9 fields then "path\torig").
        '1' | '2' => {
            let skip = if kind == '1' { 8 } else { 9 };
            let rest = line.splitn(skip + 1, ' ').nth(skip)?;
            // Rename entries encode "path\torig"; take the new path.
            let path = rest.split('\t').next().unwrap_or(rest);
            Some(path.trim().to_string())
        }
        // Unmerged "u ..." (10 fields then path).
        'u' => {
            let rest = line.splitn(11, ' ').nth(10)?;
            Some(rest.trim().to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirty_upstream() -> GitStatus {
        GitStatus {
            is_repo: true,
            has_commits: true,
            has_working_tree_changes: true,
            has_upstream: true,
            has_origin_remote: true,
            branch: Some("feature/x".into()),
            ..Default::default()
        }
    }

    #[test]
    fn quick_action_selects_the_next_available_repository_operation() {
        let clean = GitStatus {
            has_working_tree_changes: false,
            ..dirty_upstream()
        };
        for (label, status, action, hint) in [
            (
                "uninitialized",
                GitStatus::default(),
                GitAction::InitializeGit,
                None,
            ),
            (
                "dirty tracked",
                dirty_upstream(),
                GitAction::CommitPush,
                None,
            ),
            (
                "dirty untracked",
                GitStatus {
                    has_upstream: false,
                    ..dirty_upstream()
                },
                GitAction::Commit,
                None,
            ),
            (
                "ahead",
                GitStatus {
                    ahead: 2,
                    ..clean.clone()
                },
                GitAction::Push,
                None,
            ),
            (
                "behind",
                GitStatus {
                    behind: 3,
                    ..clean.clone()
                },
                GitAction::Pull,
                None,
            ),
            (
                "diverged",
                GitStatus {
                    ahead: 1,
                    behind: 1,
                    ..clean.clone()
                },
                GitAction::Pull,
                Some(GitHint::Diverged),
            ),
            (
                "publish",
                GitStatus {
                    has_upstream: false,
                    ..clean.clone()
                },
                GitAction::PublishBranch,
                None,
            ),
            (
                "no remote",
                GitStatus {
                    has_upstream: false,
                    has_origin_remote: false,
                    ..clean.clone()
                },
                GitAction::Commit,
                Some(GitHint::NoRemote),
            ),
            (
                "detached",
                GitStatus {
                    detached: true,
                    ..dirty_upstream()
                },
                GitAction::Commit,
                Some(GitHint::Detached),
            ),
            (
                "initial dirty",
                GitStatus {
                    has_commits: false,
                    ..dirty_upstream()
                },
                GitAction::Commit,
                None,
            ),
            (
                "initial clean",
                GitStatus {
                    has_commits: false,
                    ..clean.clone()
                },
                GitAction::Commit,
                Some(GitHint::NoCommits),
            ),
            (
                "up to date",
                clean,
                GitAction::Commit,
                Some(GitHint::UpToDate),
            ),
        ] {
            assert_eq!(
                quick_action(&status, false),
                QuickAction {
                    action: hint.is_none().then_some(action),
                    label: action,
                    hint,
                    disabled: hint.is_some(),
                },
                "{label}"
            );
            assert_eq!(
                quick_action(&status, true),
                QuickAction {
                    action: None,
                    label: GitAction::Commit,
                    hint: Some(GitHint::InProgress),
                    disabled: true,
                },
                "{label}: busy takes precedence"
            );
        }
    }

    #[test]
    fn repository_menu_offers_applicable_actions_and_explains_unavailable_ones() {
        let repo = GitStatus {
            is_repo: true,
            has_commits: true,
            has_origin_remote: true,
            branch: Some("main".into()),
            ..Default::default()
        };
        for (label, status, expected) in [
            (
                "not a repo",
                GitStatus::default(),
                vec![(GitAction::InitializeGit, false, None)],
            ),
            (
                "unpublished",
                repo.clone(),
                vec![
                    (GitAction::Commit, true, Some(GitHint::NoChanges)),
                    (GitAction::Push, true, Some(GitHint::NoUpstream)),
                    (GitAction::PublishBranch, false, None),
                ],
            ),
            (
                "dirty and ahead",
                GitStatus {
                    has_working_tree_changes: true,
                    has_upstream: true,
                    ahead: 2,
                    ..repo.clone()
                },
                vec![
                    (GitAction::Commit, false, None),
                    (GitAction::Push, false, None),
                    (GitAction::Pull, true, Some(GitHint::UpToDate)),
                ],
            ),
            (
                "behind",
                GitStatus {
                    has_upstream: true,
                    behind: 2,
                    ..repo.clone()
                },
                vec![
                    (GitAction::Commit, true, Some(GitHint::NoChanges)),
                    (GitAction::Push, true, Some(GitHint::Behind)),
                    (GitAction::Pull, false, None),
                ],
            ),
            (
                "diverged",
                GitStatus {
                    has_upstream: true,
                    ahead: 1,
                    behind: 2,
                    ..repo.clone()
                },
                vec![
                    (GitAction::Commit, true, Some(GitHint::NoChanges)),
                    (GitAction::Push, true, Some(GitHint::Diverged)),
                    (GitAction::Pull, true, Some(GitHint::Diverged)),
                ],
            ),
            (
                "detached",
                GitStatus {
                    detached: true,
                    branch: None,
                    ..repo
                },
                vec![
                    (GitAction::Commit, true, Some(GitHint::Detached)),
                    (GitAction::Push, true, Some(GitHint::Detached)),
                    (GitAction::PublishBranch, true, Some(GitHint::Detached)),
                ],
            ),
        ] {
            let items = menu_items(&status, false);
            assert_eq!(
                items
                    .iter()
                    .map(|i| (i.action, i.disabled, i.hint))
                    .collect::<Vec<_>>(),
                expected,
                "{label}"
            );
            let busy = menu_items(&status, true);
            assert!(busy.iter().all(|i| i.disabled), "{label}");
            assert_eq!(
                busy.iter().map(|i| i.action).collect::<Vec<_>>(),
                items.iter().map(|i| i.action).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn commit_selection_distinguishes_all_some_and_no_files() {
        let files = ["a.rs", "b.rs", "c.rs"].map(|path| GitFileEntry {
            path: path.into(),
            insertions: 0,
            deletions: 0,
        });
        for (excluded, expected) in [
            (vec![], None),
            (vec!["b.rs"], Some(vec!["a.rs".into(), "c.rs".into()])),
            (vec!["a.rs", "b.rs", "c.rs"], Some(vec![])),
        ] {
            assert_eq!(
                included_paths(&files, &excluded.into_iter().map(String::from).collect()),
                expected
            );
        }
    }

    #[test]
    fn slug_generation() {
        assert_eq!(
            sanitize_branch_fragment("Add: Feature!! Foo"),
            "add-feature-foo"
        );
        // Underscores are preserved; separator edges are trimmed.
        assert_eq!(
            sanitize_branch_fragment("  --Weird__Name--  "),
            "weird__name"
        );
        assert_eq!(sanitize_branch_fragment("feat/thing bar"), "feat/thing-bar");
        assert_eq!(sanitize_branch_fragment("feat//a///b"), "feat/a/b");
        assert_eq!(sanitize_branch_fragment("***"), "update");
        assert_eq!(
            feature_branch_name("Fix the parser"),
            "tcode/fix-the-parser"
        );
        assert_eq!(
            feature_branch_name("feat: add toast system!"),
            "tcode/feat-add-toast-system"
        );
    }

    #[test]
    fn commit_message_sanitizer() {
        assert_eq!(
            sanitize_commit_message("feat: do the thing."),
            "feat: do the thing"
        );
        assert_eq!(
            sanitize_commit_message("```\nfix: bug\n\nbody line\n```"),
            "fix: bug\n\nbody line"
        );
        let long = "x".repeat(100);
        assert_eq!(sanitize_commit_message(&long).len(), 72);
        assert_eq!(sanitize_commit_message("   "), "");
    }

    #[test]
    fn porcelain_status_preserves_branch_state_and_changed_paths() {
        let porcelain = "# branch.oid abc123\n# branch.head feature/x\n# branch.upstream origin/feature/x\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 abc def src/app.rs\n? new file.txt\n? new file.txt\n";
        let status = parse_status(
            porcelain,
            &[("src/app.rs".into(), 5, 3)],
            Some("main"),
            true,
        );
        assert_eq!(
            status,
            GitStatus {
                is_repo: true,
                has_commits: true,
                detached: false,
                branch: Some("feature/x".into()),
                is_default_branch: false,
                has_working_tree_changes: true,
                has_origin_remote: true,
                has_upstream: true,
                ahead: 2,
                behind: 1,
                changed_files: vec![
                    GitFileEntry {
                        path: "new file.txt".into(),
                        insertions: 0,
                        deletions: 0
                    },
                    GitFileEntry {
                        path: "src/app.rs".into(),
                        insertions: 5,
                        deletions: 3
                    },
                ],
                github_repository: None,
            }
        );
        for (head, oid, default, commits, detached, is_default) in [
            ("main", "(initial)", None, false, false, true),
            ("master", "abc", None, true, false, true),
            ("trunk", "abc", Some("trunk"), true, false, true),
            ("main", "abc", Some("trunk"), true, false, false),
            ("(detached)", "abc", None, true, true, false),
        ] {
            let porcelain = format!("# branch.oid {oid}\n# branch.head {head}\n");
            let status = parse_status(&porcelain, &[], default, false);
            assert_eq!(
                status,
                GitStatus {
                    is_repo: true,
                    has_commits: commits,
                    detached,
                    branch: (!detached).then(|| head.into()),
                    is_default_branch: is_default,
                    ..Default::default()
                },
                "{head} against {default:?}"
            );
        }
    }
}
