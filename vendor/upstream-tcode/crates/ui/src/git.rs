//! Git presentation with core-owned semantics and services-owned I/O.

use tcode_core::git::{GitAction, GitHint};

pub fn git_action_label_key(action: GitAction) -> &'static str {
    match action {
        GitAction::Commit => "git.action.commit",
        GitAction::CommitPush => "git.action.commit_push",
        GitAction::Push => "git.action.push",
        GitAction::Pull => "git.action.pull",
        GitAction::PublishBranch => "git.action.publish",
        GitAction::InitializeGit => "git.action.init",
    }
}

pub fn git_hint_key(hint: GitHint) -> &'static str {
    match hint {
        GitHint::InProgress => "git.hint.in_progress",
        GitHint::Detached => "git.hint.detached",
        GitHint::NoCommits => "git.hint.no_commits",
        GitHint::NoRemote => "git.hint.no_remote",
        GitHint::Diverged => "git.hint.diverged",
        GitHint::UpToDate => "git.hint.up_to_date",
        GitHint::NoChanges => "git.hint.no_changes",
        GitHint::NoUpstream => "git.hint.no_upstream",
        GitHint::Behind => "git.hint.behind",
        GitHint::NoAhead => "git.hint.no_ahead",
    }
}
