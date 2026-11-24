//! App-owned Git process and filesystem infrastructure.

use std::path::Path;

use agent::{FileChange, FileChangeKind};
use tcode_core::git::{GitAction, GitStatus, parse_status};
pub use tcode_protocol::{GitDiffResult, GitDiffScope, GitFileText};

const MAX_RAW_DIFF_BYTES: usize = 200 * 1024;
pub const MAX_FILE_TEXT_BYTES: u64 = 512 * 1024;

fn working_tree_diff_args(ignore_whitespace: bool) -> Vec<String> {
    let mut args = vec!["diff".into(), "HEAD".into()];
    if ignore_whitespace {
        args.push("-w".into());
    }
    args.push("--".into());
    args
}

fn merge_base_args(base: &str) -> Vec<String> {
    vec!["merge-base".into(), base.into(), "HEAD".into()]
}

fn branch_diff_args(merge_base: &str, ignore_whitespace: bool) -> Vec<String> {
    let mut args = vec!["diff".into(), format!("{merge_base}...HEAD")];
    if ignore_whitespace {
        args.push("-w".into());
    }
    args.push("--".into());
    args
}

struct ParsedFileChange {
    change: FileChange,
    old_path: Option<String>,
    new_path: Option<String>,
}

fn git_command(cwd: &Path) -> std::process::Command {
    let mut command = crate::process::command("git");
    command.env("GIT_OPTIONAL_LOCKS", "0").current_dir(cwd);
    command
}

fn git_output(cwd: &Path, args: &[String]) -> Result<std::process::Output, String> {
    git_command(cwd)
        .args(args)
        .output()
        .map_err(|error| error.to_string())
}

fn append_capped(raw: &mut Vec<u8>, bytes: &[u8], truncated: &mut bool) {
    let remaining = MAX_RAW_DIFF_BYTES.saturating_sub(raw.len());
    raw.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    *truncated |= bytes.len() > remaining;
}

fn patch_path(value: &str, side_prefix: Option<&str>) -> Option<String> {
    let value = value.trim_end_matches('\t');
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    if value == "/dev/null" {
        return None;
    }
    Some(
        side_prefix
            .and_then(|prefix| value.strip_prefix(prefix))
            .unwrap_or(value)
            .to_string(),
    )
}

fn split_git_patch(raw: &str, cwd: &Path, repo_prefix: &str) -> Vec<ParsedFileChange> {
    let mut sections = Vec::new();
    let mut current = String::new();
    for line in raw.lines() {
        if line.starts_with("diff --git ") && !current.is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        if !current.is_empty() || line.starts_with("diff --git ") {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.is_empty() {
        sections.push(current);
    }
    sections
        .into_iter()
        .filter_map(|patch| {
            let old_path = patch
                .lines()
                .find_map(|line| {
                    line.strip_prefix("rename from ")
                        .and_then(|path| patch_path(path, None))
                })
                .or_else(|| {
                    patch.lines().find_map(|line| {
                        line.strip_prefix("--- ")
                            .and_then(|path| patch_path(path, Some("a/")))
                    })
                });
            let new_path = patch
                .lines()
                .find_map(|line| {
                    line.strip_prefix("rename to ")
                        .and_then(|path| patch_path(path, None))
                })
                .or_else(|| {
                    patch.lines().find_map(|line| {
                        line.strip_prefix("+++ ")
                            .and_then(|path| patch_path(path, Some("b/")))
                    })
                });
            let path = new_path.as_deref().or(old_path.as_deref())?;
            let cwd_path = path.strip_prefix(repo_prefix).unwrap_or(path);
            Some(ParsedFileChange {
                change: FileChange {
                    path: cwd.join(cwd_path).to_string_lossy().to_string(),
                    kind: if old_path.is_none() {
                        FileChangeKind::Create
                    } else if new_path.is_none() {
                        FileChangeKind::Delete
                    } else if patch.lines().any(|line| line.starts_with("rename to ")) {
                        FileChangeKind::Rename
                    } else {
                        FileChangeKind::Modify
                    },
                    diff: Some(patch),
                },
                old_path,
                new_path,
            })
        })
        .collect()
}

fn repo_prefix(cwd: &Path) -> String {
    let args = vec!["rev-parse".into(), "--show-prefix".into()];
    git_output(cwd, &args)
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|prefix| prefix.trim().to_string())
        .unwrap_or_default()
}

fn repo_root_path(path: &str, prefix: &str) -> String {
    if prefix.is_empty() || path.starts_with(prefix) {
        path.to_string()
    } else {
        format!("{prefix}{path}")
    }
}

fn read_disk_text(path: &Path) -> Option<String> {
    String::from_utf8(crate::user_files::read_bounded(path, MAX_FILE_TEXT_BYTES).ok()?).ok()
}

fn read_git_text(cwd: &Path, revision: &str, path: &str) -> Option<String> {
    let args = vec!["show".into(), format!("{revision}:{path}")];
    let output = git_output(cwd, &args).ok()?;
    if !output.status.success() || output.stdout.len() as u64 > MAX_FILE_TEXT_BYTES {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn load_file_texts(
    cwd: &Path,
    scope: GitDiffScope,
    base_revision: Option<&str>,
    prefix: &str,
    parsed: &[ParsedFileChange],
) -> Vec<GitFileText> {
    parsed
        .iter()
        .map(|parsed| {
            let old_path = parsed
                .old_path
                .as_deref()
                .map(|path| repo_root_path(path, prefix));
            let new_path = parsed
                .new_path
                .as_deref()
                .map(|path| repo_root_path(path, prefix));
            match scope {
                GitDiffScope::WorkingTree => GitFileText {
                    old: old_path
                        .as_deref()
                        .and_then(|path| read_git_text(cwd, "HEAD", path)),
                    new: new_path
                        .as_deref()
                        .and_then(|_| read_disk_text(Path::new(&parsed.change.path))),
                },
                GitDiffScope::Branch => GitFileText {
                    old: base_revision.and_then(|revision| {
                        old_path
                            .as_deref()
                            .and_then(|path| read_git_text(cwd, revision, path))
                    }),
                    new: new_path
                        .as_deref()
                        .and_then(|path| read_git_text(cwd, "HEAD", path)),
                },
                GitDiffScope::Unknown => GitFileText::default(),
            }
        })
        .collect()
}

fn git_branches(cwd: &Path) -> (Vec<String>, Option<String>) {
    let args = vec![
        "for-each-ref".into(),
        "--format=%(refname:short)".into(),
        "refs/heads".into(),
    ];
    let mut branches = git_output(cwd, &args)
        .ok()
        .filter(|output| output.status.success())
        .map(|output| parse_branch_list(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default();
    let origin_args = vec![
        "symbolic-ref".into(),
        "--quiet".into(),
        "--short".into(),
        "refs/remotes/origin/HEAD".into(),
    ];
    let origin_default = git_output(cwd, &origin_args)
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(origin) = &origin_default
        && !branches.contains(origin)
    {
        branches.push(origin.clone());
    }
    let default = ["main", "master"]
        .into_iter()
        .find(|candidate| branches.iter().any(|branch| branch == candidate))
        .map(str::to_string)
        .or(origin_default)
        .or_else(|| branches.first().cloned());
    (branches, default)
}

pub fn load_git_diff(
    cwd: &Path,
    scope: GitDiffScope,
    base: Option<&str>,
    ignore_whitespace: bool,
) -> GitDiffResult {
    let (branches, default_base) = git_branches(cwd);
    let mut base_revision = None;
    let args = match scope {
        GitDiffScope::WorkingTree => working_tree_diff_args(ignore_whitespace),
        GitDiffScope::Branch => {
            let base = base.or(default_base.as_deref()).unwrap_or("HEAD");
            let merge_base = match git_output(cwd, &merge_base_args(base)) {
                Ok(output) if output.status.success() => {
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                }
                Ok(output) => {
                    return GitDiffResult {
                        error: Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
                        branches,
                        default_base,
                        ..GitDiffResult::default()
                    };
                }
                Err(error) => {
                    return GitDiffResult {
                        error: Some(error),
                        branches,
                        default_base,
                        ..GitDiffResult::default()
                    };
                }
            };
            let args = branch_diff_args(&merge_base, ignore_whitespace);
            base_revision = Some(merge_base);
            args
        }
        GitDiffScope::Unknown => {
            return GitDiffResult {
                error: Some("unknown git diff scope".into()),
                branches,
                default_base,
                ..GitDiffResult::default()
            };
        }
    };
    let output = match git_output(cwd, &args) {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return GitDiffResult {
                error: Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
                branches,
                default_base,
                ..GitDiffResult::default()
            };
        }
        Err(error) => {
            return GitDiffResult {
                error: Some(error),
                branches,
                default_base,
                ..GitDiffResult::default()
            };
        }
    };
    let mut raw = Vec::new();
    let mut truncated = false;
    append_capped(&mut raw, &output.stdout, &mut truncated);
    if scope == GitDiffScope::WorkingTree && !truncated {
        let untracked_args = vec![
            "ls-files".into(),
            "--others".into(),
            "--exclude-standard".into(),
            "-z".into(),
        ];
        if let Ok(untracked) = git_output(cwd, &untracked_args)
            && untracked.status.success()
        {
            for path in untracked
                .stdout
                .split(|byte| *byte == 0)
                .filter(|p| !p.is_empty())
            {
                let path = String::from_utf8_lossy(path).to_string();
                let args = vec!["diff".into(), "--no-index".into(), "/dev/null".into(), path];
                if let Ok(output) = git_output(cwd, &args)
                    && (output.status.success() || output.status.code() == Some(1))
                {
                    append_capped(&mut raw, &output.stdout, &mut truncated);
                }
                if truncated {
                    break;
                }
            }
        }
    }
    let raw = String::from_utf8_lossy(&raw);
    let prefix = repo_prefix(cwd);
    let parsed = split_git_patch(&raw, cwd, &prefix);
    let texts = load_file_texts(cwd, scope, base_revision.as_deref(), &prefix, &parsed);
    let changes = parsed.into_iter().map(|parsed| parsed.change).collect();
    GitDiffResult {
        changes,
        texts,
        truncated,
        error: None,
        branches,
        default_base,
    }
}

/// Read the branch from a checkout, linked worktree, submodule, or its subdirectory.
pub fn read_git_branch(cwd: &Path) -> Option<String> {
    let git = cwd.ancestors().find_map(|root| {
        let path = root.join(".git");
        if path.is_dir() {
            return Some(path);
        }
        let file = std::fs::read_to_string(path).ok()?;
        let directory = file.trim().strip_prefix("gitdir: ")?;
        Some(root.join(directory))
    })?;
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        (!name.is_empty()).then(|| name.to_string())
    } else if !head.is_empty() {
        Some(head.chars().take(7).collect())
    } else {
        None
    }
}

/// Parse `git for-each-ref` output into a list of branch names (blank lines
/// dropped, whitespace trimmed).
fn parse_branch_list(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// List local git branches for `cwd` (empty when not a repo / git fails).
pub fn list_git_branches(cwd: &Path) -> Vec<String> {
    let output = crate::process::command("git")
        .args(["for-each-ref", "refs/heads", "--format=%(refname:short)"])
        .current_dir(cwd)
        .output();
    match output {
        Ok(out) if out.status.success() => parse_branch_list(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

/// Why a `git checkout` was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckoutError {
    /// The working tree has uncommitted changes.
    Dirty,
    /// git failed (spawn error or non-zero checkout).
    Git(String),
}

/// Check out `branch` in `cwd` iff the working tree is clean.
pub fn checkout_if_clean(cwd: &Path, branch: &str) -> Result<(), CheckoutError> {
    let status = crate::process::command("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .map_err(|e| CheckoutError::Git(format!("git status failed: {e}")))?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(CheckoutError::Git(format!(
            "git status failed: {}",
            stderr.trim()
        )));
    }
    if !status.stdout.is_empty() {
        return Err(CheckoutError::Dirty);
    }
    let checkout = crate::process::command("git")
        .args(["checkout", branch])
        .current_dir(cwd)
        .output()
        .map_err(|e| CheckoutError::Git(format!("git checkout failed: {e}")))?;
    if !checkout.status.success() {
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        return Err(CheckoutError::Git(format!(
            "git checkout failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

pub fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_command(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

pub fn is_git_repo(cwd: &Path) -> bool {
    run_git(cwd, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|out| out.trim() == "true")
}

pub fn read_status(cwd: &Path) -> GitStatus {
    if !is_git_repo(cwd) {
        return GitStatus::default();
    }
    let porcelain = run_git(cwd, &["status", "--porcelain=2", "--branch"]).unwrap_or_default();
    let numstat = read_numstat(cwd);
    let checkout = crate::forge::Checkout::new(cwd);
    let has_origin_remote = checkout.remotes().iter().any(|(name, _)| name == "origin");
    let default_branch = run_git(cwd, &["symbolic-ref", "refs/remotes/origin/HEAD"])
        .ok()
        .map(|value| {
            value
                .trim()
                .trim_start_matches("refs/remotes/origin/")
                .to_string()
        })
        .filter(|value| !value.is_empty());
    let mut status = parse_status(
        &porcelain,
        &numstat,
        default_branch.as_deref(),
        has_origin_remote,
    );
    status.github_repository = crate::github::repository::resolve(&checkout)
        .map(|repository| format!("{}/{}", repository.owner, repository.name));
    status
}

fn read_numstat(cwd: &Path) -> Vec<(String, u32, u32)> {
    let mut out = Vec::new();
    for args in [
        ["diff", "--numstat"].as_slice(),
        ["diff", "--cached", "--numstat"].as_slice(),
    ] {
        if let Ok(text) = run_git(cwd, args) {
            for line in text.lines() {
                let mut columns = line.split('\t');
                let (Some(insertions), Some(deletions), Some(path)) =
                    (columns.next(), columns.next(), columns.next())
                else {
                    continue;
                };
                out.push((
                    path.to_string(),
                    insertions.parse().unwrap_or(0),
                    deletions.parse().unwrap_or(0),
                ));
            }
        }
    }
    out
}

pub fn commit_diff_context(cwd: &Path, included: Option<&[String]>) -> (String, String) {
    let has_head = run_git(cwd, &["rev-parse", "--verify", "HEAD"]).is_ok();
    let pathspec: Vec<&str> = match included {
        Some(paths) if !paths.is_empty() => {
            let mut values = vec!["--"];
            values.extend(paths.iter().map(String::as_str));
            values
        }
        _ => Vec::new(),
    };
    let mut stat_args = vec!["diff", "--stat"];
    let mut patch_args = vec!["diff", "--no-ext-diff", "--patch", "--minimal"];
    if has_head {
        stat_args.push("HEAD");
        patch_args.push("HEAD");
    }
    stat_args.extend_from_slice(&pathspec);
    patch_args.extend_from_slice(&pathspec);
    let mut stat = run_git(cwd, &stat_args).unwrap_or_default();
    if stat.trim().is_empty() {
        stat = run_git(cwd, &["status", "--short"]).unwrap_or_default();
    }
    let patch = run_git(cwd, &patch_args).unwrap_or_default();
    (stat, patch)
}

pub fn run_claude_headless(
    binary: Option<&Path>,
    cwd: &Path,
    prompt: &str,
) -> Result<String, String> {
    let bin = binary
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "claude".to_string());
    let out = crate::process::command(&bin)
        .arg("-p")
        .arg(prompt)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run {bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{bin} -p failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn perform_action(
    cwd: &Path,
    action: GitAction,
    message: Option<&str>,
    included: Option<&[String]>,
    feature_branch: Option<&str>,
    current_branch: Option<&str>,
) -> Result<String, String> {
    match action {
        GitAction::InitializeGit => Ok(run_git(cwd, &["init"])?.trim().to_string()),
        GitAction::Commit | GitAction::CommitPush => {
            if let Some(branch) = feature_branch {
                run_git(cwd, &["checkout", "-b", branch])?;
            }
            stage_for_commit(cwd, included)?;
            let message = message.unwrap_or("").trim();
            if message.is_empty() {
                return Err("empty commit message".to_string());
            }
            let mut transcript = run_git(cwd, &["commit", "-m", message])?.trim().to_string();
            if action == GitAction::CommitPush {
                let push = run_git(cwd, &["push"])?;
                transcript.push('\n');
                transcript.push_str(push.trim());
            }
            Ok(transcript)
        }
        GitAction::Push => Ok(run_git(cwd, &["push"])?.trim().to_string()),
        GitAction::Pull => Ok(run_git(cwd, &["pull", "--ff-only"])?.trim().to_string()),
        GitAction::PublishBranch => {
            let branch = feature_branch
                .or(current_branch)
                .ok_or_else(|| "no current branch to publish".to_string())?;
            Ok(run_git(cwd, &["push", "-u", "origin", branch])?
                .trim()
                .to_string())
        }
    }
}

pub fn stage_for_commit(cwd: &Path, included: Option<&[String]>) -> Result<(), String> {
    match included {
        Some(paths) if !paths.is_empty() => {
            let _ = run_git(cwd, &["reset", "-q"]);
            let mut args = vec!["add", "-A", "--"];
            args.extend(paths.iter().map(String::as_str));
            run_git(cwd, &args).map(|_| ())
        }
        _ => run_git(cwd, &["add", "-A"]).map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn run(root: &Path, args: &[&str]) {
        let output = crate::process::command("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "tcode")
            .env("GIT_AUTHOR_EMAIL", "tcode@localhost")
            .env("GIT_COMMITTER_NAME", "tcode")
            .env("GIT_COMMITTER_EMAIL", "tcode@localhost")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    fn scratch_repo(prefix: &str) -> (PathBuf, PathBuf) {
        let temp = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        let root = temp.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        run(&root, &["-c", "init.templateDir=", "init", "-b", "main"]);
        run(&root, &["config", "commit.gpgSign", "false"]);
        run(
            &root,
            &[
                "config",
                "core.hooksPath",
                temp.join("no-hooks").to_str().unwrap(),
            ],
        );
        run(&root, &["config", "core.autocrlf", "false"]);
        // Production merge-back commits in this repo; CI runners have no
        // global git identity, so pin one at the repo level.
        run(&root, &["config", "user.name", "tcode"]);
        run(&root, &["config", "user.email", "tcode@localhost"]);
        std::fs::write(root.join("tracked.txt"), "initial\n").unwrap();
        run(&root, &["add", "tracked.txt"]);
        run(&root, &["commit", "-m", "initial"]);
        (temp, root)
    }

    #[test]
    fn working_tree_and_branch_diff_round_trip() {
        let (temp, root) = scratch_repo("tcode-diff-scope");
        let git = |args: &[&str]| run(&root, args);
        let base = "main";
        git(&["checkout", "-b", "feature"]);
        std::fs::write(root.join("tracked.txt"), "after\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "new\n").unwrap();

        let working = load_git_diff(&root, GitDiffScope::WorkingTree, None, false);
        assert!(working.error.is_none());
        assert_eq!(working.changes.len(), 2);
        assert_eq!(working.texts.len(), working.changes.len());
        assert!(working.changes.iter().any(|change| {
            change.path.ends_with("untracked.txt") && change.kind == FileChangeKind::Create
        }));
        let tracked_index = working
            .changes
            .iter()
            .position(|change| change.path.ends_with("tracked.txt"))
            .unwrap();
        assert_eq!(
            working.texts[tracked_index].old.as_deref(),
            Some("initial\n")
        );
        assert_eq!(working.texts[tracked_index].new.as_deref(), Some("after\n"));
        let untracked_index = working
            .changes
            .iter()
            .position(|change| change.path.ends_with("untracked.txt"))
            .unwrap();
        assert!(working.texts[untracked_index].old.is_none());
        assert_eq!(working.texts[untracked_index].new.as_deref(), Some("new\n"));
        git(&["add", "."]);
        git(&["commit", "-m", "feature changes"]);
        let branch = load_git_diff(&root, GitDiffScope::Branch, Some(base), false);
        assert!(branch.error.is_none());
        assert_eq!(branch.changes.len(), 2);
        assert_eq!(branch.texts.len(), branch.changes.len());
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn diff_texts_handle_created_and_deleted_files() {
        let (temp, root) = scratch_repo("tcode-diff-file-text-test");
        std::fs::remove_file(root.join("tracked.txt")).unwrap();
        std::fs::write(root.join("created file.txt"), "created\n").unwrap();

        let result = load_git_diff(&root, GitDiffScope::WorkingTree, None, false);

        assert!(result.error.is_none());
        assert_eq!(result.texts.len(), result.changes.len());
        let deleted_index = result
            .changes
            .iter()
            .position(|change| change.path.ends_with("tracked.txt"))
            .unwrap();
        assert_eq!(result.changes[deleted_index].kind, FileChangeKind::Delete);
        assert_eq!(
            result.texts[deleted_index].old.as_deref(),
            Some("initial\n")
        );
        assert!(result.texts[deleted_index].new.is_none());
        let created_index = result
            .changes
            .iter()
            .position(|change| change.path.ends_with("created file.txt"))
            .unwrap();
        assert_eq!(result.changes[created_index].kind, FileChangeKind::Create);
        assert!(result.texts[created_index].old.is_none());
        assert_eq!(
            result.texts[created_index].new.as_deref(),
            Some("created\n")
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn whitespace_only_changes_can_be_ignored() {
        let (temp, root) = scratch_repo("tcode-diff-whitespace-test");
        std::fs::write(root.join("tracked.txt"), "  initial  \n").unwrap();

        let normal = load_git_diff(&root, GitDiffScope::WorkingTree, None, false);
        let ignored = load_git_diff(&root, GitDiffScope::WorkingTree, None, true);

        assert_eq!(normal.changes.len(), 1);
        assert_eq!(normal.texts.len(), normal.changes.len());
        assert!(ignored.changes.is_empty());
        assert_eq!(ignored.texts.len(), ignored.changes.len());

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn rename_uses_the_old_path_for_base_text() {
        let (temp, root) = scratch_repo("tcode-diff-rename-test");
        run(&root, &["mv", "tracked.txt", "renamed.txt"]);

        let result = load_git_diff(&root, GitDiffScope::WorkingTree, None, false);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].kind, FileChangeKind::Rename);
        assert!(result.changes[0].path.ends_with("renamed.txt"));
        assert_eq!(result.texts[0].old.as_deref(), Some("initial\n"));
        assert_eq!(result.texts[0].new.as_deref(), Some("initial\n"));
        assert_eq!(result.texts.len(), result.changes.len());

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn diff_texts_map_subdirectory_cwd_paths_to_repo_root() {
        let (temp, root) = scratch_repo("tcode-diff-subdirectory-test");
        let subdirectory = root.join("subdirectory");
        std::fs::create_dir(&subdirectory).unwrap();
        std::fs::write(subdirectory.join("nested.txt"), "before\n").unwrap();
        run(&root, &["add", "subdirectory/nested.txt"]);
        run(&root, &["commit", "-m", "add nested file"]);
        std::fs::write(subdirectory.join("nested.txt"), "after\n").unwrap();

        let result = load_git_diff(&subdirectory, GitDiffScope::WorkingTree, None, false);

        assert_eq!(result.changes.len(), 1);
        assert_eq!(
            Path::new(&result.changes[0].path),
            subdirectory.join("nested.txt")
        );
        assert_eq!(result.texts[0].old.as_deref(), Some("before\n"));
        assert_eq!(result.texts[0].new.as_deref(), Some("after\n"));
        assert_eq!(result.texts.len(), result.changes.len());

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn checkout_preserves_dirty_worktree_then_switches_once_clean() {
        let (temp, root) = scratch_repo("tcode-checkout-dirty-test");
        run(&root, &["branch", "feature/x"]);
        std::fs::write(root.join("tracked.txt"), "dirty\n").unwrap();

        assert_eq!(
            checkout_if_clean(&root, "feature/x"),
            Err(CheckoutError::Dirty)
        );
        assert_eq!(read_git_branch(&root), Some("main".into()));
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "dirty\n"
        );
        assert_eq!(list_git_branches(&root), ["feature/x", "main"]);
        run(&root, &["checkout", "--", "tracked.txt"]);
        assert_eq!(checkout_if_clean(&root, "feature/x"), Ok(()));
        assert_eq!(read_git_branch(&root), Some("feature/x".into()));

        let commit = crate::process::command("git")
            .args(["rev-parse", "--short=7", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(commit.status.success());
        let expected = String::from_utf8(commit.stdout).unwrap().trim().to_string();
        run(&root, &["checkout", "--detach"]);
        assert_eq!(read_git_branch(&root), Some(expected));
        let uninitialized = temp.join("uninitialized");
        std::fs::create_dir_all(&uninitialized).unwrap();
        assert_eq!(read_git_branch(&uninitialized), None);
        std::fs::create_dir(uninitialized.join(".git")).unwrap();
        assert_eq!(read_git_branch(&uninitialized), None);

        let _ = std::fs::remove_dir_all(temp);
    }
}
