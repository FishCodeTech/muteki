//! Git operations use the pinned upstream implementation and its refusal rules.
use crate::{
    AppState, chat, control,
    control_contract::{GetGitParams, GitOp, GitParams, GitWhat},
};
use serde_json::{Value, json};
use std::{sync::Arc, path::{Path, PathBuf}};
use tcode_control_services::git;
fn error(message: impl ToString) -> Value {
    json!({"code":"control.git_failed","message":message.to_string()})
}
fn result(id: &str, outcome: &str, reason: Option<String>, output: Option<Value>) -> Value {
    json!({"thread_id":id,"outcome":outcome,"reason":reason,"output":output})
}
fn check(
    s: &AppState,
    caller: &str,
    target: &str,
    remote: bool,
    mutation: bool,
) -> Result<Value, Value> {
    let (grant, _) = control::effective(s, caller).map_err(|(_, b)| b.0["error"].clone())?;
    control::check_target(s, caller, target, &grant)?;
    if remote && !grant.categories.contains(&control::Category::GitRemote) {
        return Err(error("git_remote permission is required"));
    }
    let doc = chat::load(s, target).map_err(|(_, b)| b.0["error"].clone())?;
    if mutation && doc["thread"]["state"]["lineage"]["native_subagent"].is_string() {
        return Err(error("provider-native mirror threads are read-only"));
    }
    Ok(doc)
}
pub(crate) fn workspace_root(s: &AppState, doc: &Value) -> Result<PathBuf, Value> {
    let id = doc["thread"]["workspace_id"].as_str().ok_or_else(||error("thread has no workspace"))?;
    let workspace = s.store.get("workspaces", id).map_err(error)?.ok_or_else(||error("workspace is missing"))?;
    let root = workspace["root_path"].as_str().ok_or_else(||error("workspace has no root"))?;
    std::fs::canonicalize(root).map_err(error)
}
pub(crate) fn repository_boundary(cwd: &Path, bound: &Path) -> Result<PathBuf, String> {
    let cwd = cwd.canonicalize().map_err(|error|error.to_string())?;
    let bound = bound.canonicalize().map_err(|error|error.to_string())?;
    if !cwd.starts_with(&bound) { return Err("Git directory is outside the registered workspace".into()); }
    let output = crate::host_process::std_command("git").arg("-C").arg(&cwd).args(["rev-parse","--show-toplevel"]).output().map_err(|error|error.to_string())?;
    if !output.status.success() { return Err(String::from_utf8_lossy(&output.stderr).into_owned()); }
    let repository = PathBuf::from(String::from_utf8(output.stdout).map_err(|error|error.to_string())?.trim()).canonicalize().map_err(|error|error.to_string())?;
    if repository != bound {
        return Err("Git controls require the checkout root to be the registered workspace; this workspace is a subdirectory of another repository".into());
    }
    Ok(repository)
}
pub async fn read(s: &Arc<AppState>, caller: &str, p: GetGitParams) -> Result<Value, Value> {
    let (target, cwd, bound) = if let Some(project) = &p.project_id {
        let (grant, _) = control::effective(s, caller).map_err(|(_, b)| b.0["error"].clone())?;
        let own =
            chat::load(s, caller).map_err(|(_, b)| b.0["error"].clone())?["thread"]["project_id"]
                .as_str()
                .unwrap_or("")
                .to_owned();
        if !grant.scope.reaches(&own, project) {
            return Err(error("project is outside the caller's scope"));
        }
        let root = crate::workspace_read_api::project_bound_root(s, project).map_err(|(_, b)|b.0["error"].clone())?;
        (None, root.clone(), root)
    } else {
        let id = control::resolve_thread_id(caller, p.thread_id.as_deref().unwrap_or(caller));
        let doc = check(s, caller, id, false, false)?;
        (
            Some(id.to_owned()),
            crate::native_chat::session_cwd(s, &doc, id).map_err(error)?,
            workspace_root(s, &doc)?,
        )
    };
    let project = p.project_id;
    tokio::task::spawn_blocking(move || {
        repository_boundary(&cwd, &bound).map_err(error)?;
        let mut answer =
            json!({"thread_id":target,"project_id":project,"cwd":cwd,"redacted":0,"omitted":[]});
        match p.what {
            GitWhat::Status => match git::read_status(&cwd) {
                Ok(status) if status.is_repo => {
                    answer["status"] = serde_json::to_value(status).map_err(error)?
                }
                Ok(_) => answer["error"] = json!("not a git repository"),
                Err(message) => answer["error"] = json!(message),
            },
            GitWhat::Branches => match git::list_git_branches(&cwd) {
                Ok(branches) => {
                    answer["branches"] = json!(branches);
                    answer["current_branch"] = json!(git::read_git_branch(&cwd));
                }
                Err(message) => answer["error"] = json!(message),
            },
            GitWhat::Diff => {
                let diff = git::load_git_diff(&cwd, git::GitDiffScope::WorkingTree, None, false);
                answer["error"] = json!(diff.error);
                let mut omitted = Vec::new();
                if diff.truncated {
                    omitted.push("git's diff was cut at the host's diff limit".to_owned());
                }
                let mut text = String::new();
                let mut left = 0;
                for patch in diff
                    .changes
                    .iter()
                    .filter_map(|change| change.diff.as_ref())
                {
                    if text.len() + patch.len() > 65536 {
                        left += 1;
                        continue;
                    }
                    text.push_str(patch);
                    if !patch.ends_with('\n') {
                        text.push('\n');
                    }
                }
                if left > 0 {
                    omitted.push(format!(
                        "the diffs of {left} files did not fit the response budget"
                    ));
                }
                let (text, count) = crate::control_index::redact(&text);
                answer["diff"] = json!(text);
                answer["redacted"] = json!(count);
                answer["omitted"] = json!(omitted);
                answer["files"] = json!(
                    diff.changes
                        .into_iter()
                        .map(|change| json!({"path":change.path,"kind":change.kind}))
                        .collect::<Vec<_>>()
                );
            }
        }
        Ok(answer)
    })
    .await
    .map_err(error)?
}
pub async fn act(s: &Arc<AppState>, caller: &str, p: GitParams) -> Result<Value, Value> {
    let target = control::resolve_thread_id(caller, &p.thread_id).to_owned();
    let doc = check(s, caller, &target, p.op.remote(), true)?;
    if target != caller
        && doc["thread"]["state"]["status"] == "running"
        && matches!(
            p.op,
            GitOp::Commit { .. } | GitOp::CommitPush { .. } | GitOp::CheckoutBranch { .. }
        )
    {
        return Ok(result(
            &target,
            "skipped",
            Some("busy: a turn is running in this thread".into()),
            None,
        ));
    }
    let cwd = crate::native_chat::session_cwd(s, &doc, &target).map_err(error)?;
    let bound = workspace_root(s, &doc)?;
    let merge = if matches!(p.op, GitOp::MergeWorktree) {
        if doc["thread"]["state"]["lineage"]["isolation"] != "worktree" {
            return Ok(result(
                &target,
                "skipped",
                Some("thread has no dedicated worktree".into()),
                None,
            ));
        }
        let workspace = s
            .store
            .get(
                "workspaces",
                doc["thread"]["workspace_id"].as_str().unwrap_or(""),
            )
            .map_err(error)?
            .ok_or_else(|| error("worktree is missing"))?;
        let branch = workspace["settings"]["branch"]
            .as_str()
            .ok_or_else(|| error("worktree branch is missing"))?
            .to_owned();
        let project = doc["thread"]["project_id"].as_str().unwrap_or("");
        let destination = crate::workspace_read_api::project_bound_root(s, project)
            .map_err(|(_, b)| b.0["error"].clone())?;
        Some((destination, branch))
    } else {
        None
    };
    control::audit(
        s,
        caller,
        &target,
        "git",
        serde_json::to_value(&p.op).map_err(error)?,
    );
    let outcome = tokio::task::spawn_blocking(move || {
        use tcode_control_core::git::GitAction;
        let repository = repository_boundary(&cwd, &bound)?;
        match p.op {
            GitOp::CheckoutBranch { branch } => git::checkout_if_clean(&cwd, &branch)
                .map(|_| None)
                .map_err(|error| format!("{error:?}")),
            GitOp::MergeWorktree => {
                let (destination, branch) = merge.unwrap();
                repository_boundary(&destination, &destination)?;
                tcode_control_services::worktree::merge_back(&destination, &cwd, &branch)
                    .map(|outcome| Some(json!(format!("{outcome:?}"))))
                    .map_err(|error| error.to_string())
            }
            op => {
                let (action, message, included) = match op {
                    GitOp::Commit { message, included } => {
                        (GitAction::Commit, Some(message), included)
                    }
                    GitOp::CommitPush { message, included } => {
                        (GitAction::CommitPush, Some(message), included)
                    }
                    GitOp::Push => (GitAction::Push, None, None),
                    GitOp::Pull => (GitAction::Pull, None, None),
                    GitOp::PublishBranch => (GitAction::PublishBranch, None, None),
                    _ => unreachable!(),
                };
                let included = included.map(|paths|crate::git_api::selected_paths(&repository, &paths).map_err(|(_,body)|body.to_string())).transpose()?;
                let branch = git::read_git_branch(&cwd);
                git::perform_action(
                    &repository,
                    action,
                    message.as_deref(),
                    included.as_deref(),
                    None,
                    branch.as_deref(),
                )
                .map(|text| Some(json!(text)))
            }
        }
    })
    .await
    .map_err(error)?;
    Ok(match outcome {
        Ok(output) => result(&target, "applied", None, output),
        Err(message) => result(&target, "failed", Some(message), None),
    })
}
