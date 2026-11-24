//! Ordinary manual Git actions on registered project and thread workspaces.
use crate::{
    AppState,
    git_process::{Failure, WRITES, failure, run},
    workspace_read_api::{bound_root, inspect_git_status, project_bound_root, status_occupancy},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
};
type Api = Result<Json<Value>, Failure>;
const GIT_ERROR: &str = "conversation.git.error";

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/projects/{id}/git/checkout", post(project_checkout))
        .route("/api/threads/{id}/git/checkout", post(thread_checkout))
        .route("/api/threads/{id}/git/commit", post(thread_commit))
        .route("/api/threads/{id}/git/push", post(thread_push))
        .route("/api/threads/{id}/git/pull", post(thread_pull))
        .route("/api/threads/{id}/git/init", post(thread_init))
        .route("/api/threads/{id}/git/merge-worktree", post(thread_merge_worktree))
}
#[derive(Deserialize)]
struct CheckoutBody {
    branch: String,
    #[serde(default)]
    create: bool,
}
#[derive(Deserialize)]
struct CommitBody {
    #[serde(default)]
    message: String,
    paths: Option<Vec<String>>,
    #[serde(default)]
    amend: bool,
}
#[derive(Default, Deserialize)]
pub(crate) struct PushBody {
    set_upstream: Option<bool>,
}
#[derive(Default, Deserialize)]
struct MergeWorktreeBody {
    #[serde(default)]
    squash: bool,
}
#[derive(Default, Deserialize)]
struct PullBody {
    #[serde(default)]
    rebase: bool,
}

pub(crate) fn validate_branch(raw: &str) -> Result<&str, Failure> {
    let name = raw.trim();
    if name.is_empty()
        || name.len() > 200
        || name.starts_with('-')
        || name.ends_with(".lock")
        || name.contains("..")
        || name
            .chars()
            .any(|c| c.is_control() || " \\~^:?*[".contains(c))
    {
        return Err(failure(
            "conversation.git.invalid_branch",
            "Invalid branch name",
        ));
    }
    Ok(name)
}
async fn repo(root: &FsPath) -> Result<PathBuf, Failure> {
    let probe = run(
        root,
        &["rev-parse", "--show-toplevel"],
        30,
        GIT_ERROR,
        false,
    )
    .await?;
    if !probe.status.success() || probe.stdout.trim().is_empty() {
        return Err(failure(
            "conversation.git.not_repo",
            "Workspace is not a Git repository",
        ));
    }
    PathBuf::from(probe.stdout.trim())
        .canonicalize()
        .map_err(|_| failure("conversation.git.not_repo", "Git root is unavailable"))
}
pub(crate) async fn branch(root: &FsPath) -> Result<String, Failure> {
    let probe = run(
        root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        30,
        GIT_ERROR,
        false,
    )
    .await?;
    if !probe.status.success() || probe.stdout.trim().is_empty() {
        return Err(failure(
            "conversation.git.detached_head",
            "Switch to a branch before performing this action",
        ));
    }
    Ok(probe.stdout.trim().to_owned())
}
fn occupancy(s: &AppState, root: &FsPath, exclude: &str, maximum: u64) -> Result<(), Failure> {
    let mut result = json!({});
    status_occupancy(s, root, exclude, &mut result)?;
    if result["occupant_count"].as_u64().unwrap_or(0) > maximum {
        return Err(failure(
            "conversation.git.root_occupied",
            "Other active conversations use this workspace; create a separate worktree",
        ));
    }
    Ok(())
}
pub(crate) fn thread_root(s: &AppState, id: &str) -> Result<PathBuf, Failure> {
    let root = bound_root(s, id)?;
    occupancy(s, &root, id, 0)?;
    Ok(root)
}
async fn status(s: &AppState, root: &FsPath, id: &str) -> Result<Value, Failure> {
    let mut value = inspect_git_status(root).await?;
    value["thread_id"] = json!(id);
    let thread = s
        .store
        .get("conversation_threads", id)
        .map_err(|_| failure(GIT_ERROR, "Could not read workspace metadata"))?;
    value["workspace_id"] = thread
        .as_ref()
        .map(|t| t["thread"]["workspace_id"].clone())
        .unwrap_or(Value::Null);
    status_occupancy(s, root, id, &mut value)?;
    Ok(value)
}
async fn checkout(root: &FsPath, body: &CheckoutBody) -> Result<(), Failure> {
    let name = validate_branch(&body.branch)?;
    let root = repo(root).await?;
    let format = run(
        &root,
        &["check-ref-format", "--branch", name],
        30,
        "conversation.git.invalid_branch",
        false,
    )
    .await?;
    if !format.status.success() {
        return Err(failure(
            "conversation.git.invalid_branch",
            "Invalid branch name",
        ));
    }
    if body.create {
        run(
            &root,
            &["switch", "-c", name],
            30,
            "conversation.git.checkout_failed",
            true,
        )
        .await?;
    } else {
        let reference = format!("refs/heads/{name}");
        let exists = run(
            &root,
            &["show-ref", "--verify", "--quiet", &reference],
            30,
            GIT_ERROR,
            false,
        )
        .await?;
        if !exists.status.success() {
            return Err(failure(
                "conversation.git.checkout_failed",
                "Local branch does not exist",
            ));
        }
        run(
            &root,
            &["switch", name],
            30,
            "conversation.git.checkout_failed",
            true,
        )
        .await?;
    }
    Ok(())
}
fn ensure_checkout_idle(s: &AppState, root: &FsPath) -> Result<(), Failure> {
    let mut status = json!({});
    status_occupancy(s, root, "", &mut status)?;
    if status["branch_switch_blocked"] == true {
        let mut error = failure(
            "conversation.git.root_occupied",
            "A running conversation uses this checkout; wait for it to finish or create a separate worktree",
        );
        error.1.0["error"]["detail"] = json!({"branch_switch_blockers": status["branch_switch_blockers"]});
        return Err(error);
    }
    Ok(())
}
struct CheckoutReservation<'a> {
    state: &'a AppState,
    root: PathBuf,
}
impl Drop for CheckoutReservation<'_> {
    fn drop(&mut self) { self.state.chat.checkout_roots.lock().remove(&self.root); }
}
async fn reserve_checkout<'a>(state: &'a AppState, root: &FsPath) -> Result<CheckoutReservation<'a>, Failure> {
    let _guard = state.chat.active.lock().await;
    ensure_checkout_idle(state, root)?;
    state.chat.checkout_roots.lock().insert(root.to_owned());
    Ok(CheckoutReservation { state, root: root.to_owned() })
}
async fn project_checkout(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<CheckoutBody>,
) -> Api {
    let _guard = WRITES.lock().await;
    let root = project_bound_root(&s, &id)?;
    let _reservation = reserve_checkout(&s, &root).await?;
    checkout(&root, &body).await?;
    let mut result = inspect_git_status(&root).await?;
    result["project_id"] = json!(id);
    status_occupancy(&s, &root, "", &mut result)?;
    Ok(Json(result))
}
async fn thread_checkout(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<CheckoutBody>,
) -> Api {
    let _guard = WRITES.lock().await;
    let root = bound_root(&s, &id)?;
    let _reservation = reserve_checkout(&s, &root).await?;
    checkout(&root, &body).await?;
    Ok(Json(status(&s, &root, &id).await?))
}
pub(crate) fn selected_paths(root: &FsPath, paths: &[String]) -> Result<Vec<String>, Failure> {
    if paths.len() > 2000 {
        return Err(failure(
            "conversation.git.invalid_path",
            "Too many selected paths",
        ));
    }
    paths
        .iter()
        .map(|raw| {
            let raw = raw.trim();
            if raw.is_empty() || raw.contains('\0') {
                return Err(failure(
                    "conversation.git.invalid_path",
                    "Path must not be empty",
                ));
            }
            let mut normalized = PathBuf::new();
            for component in root.join(raw).components() {
                match component {
                    Component::ParentDir => {
                        normalized.pop();
                    }
                    Component::CurDir => (),
                    other => normalized.push(other.as_os_str()),
                }
            }
            let relative = normalized.strip_prefix(root).map_err(|_| {
                failure(
                    "conversation.git.path_outside",
                    "Path is outside the workspace",
                )
            })?;
            if relative.as_os_str().is_empty() {
                return Err(failure(
                    "conversation.git.invalid_path",
                    "Select a file or directory inside the workspace",
                ));
            }
            Ok(relative.to_string_lossy().into_owned())
        })
        .collect()
}
async fn commit(root: &FsPath, body: CommitBody) -> Result<Value, Failure> {
    let message = body.message.trim();
    if message.is_empty() || message.chars().count() > 20_000 || message.contains('\0') {
        return Err(failure(
            "conversation.git.empty_message",
            "Provide a commit message of up to 20,000 characters",
        ));
    }
    let root = repo(root).await?;
    let branch = branch(&root).await?;
    let paths = selected_paths(&root, body.paths.as_deref().unwrap_or(&[]))?;
    let mut stage = vec!["add", "-A"];
    if !paths.is_empty() {
        stage.push("--");
        stage.extend(paths.iter().map(String::as_str));
    }
    run(&root, &stage, 30, "conversation.git.stage_failed", true).await?;
    if !body.amend {
        let mut diff = vec!["diff", "--cached", "--quiet"];
        if !paths.is_empty() {
            diff.push("--");
            diff.extend(paths.iter().map(String::as_str));
        }
        let staged = run(&root, &diff, 30, "conversation.git.stage_failed", false).await?;
        if staged.status.success() {
            return Err(failure(
                "conversation.git.nothing_to_commit",
                "There are no changes to commit",
            ));
        }
        if staged.status.code() != Some(1) {
            return Err(failure(
                "conversation.git.stage_failed",
                &staged.combined_output(),
            ));
        }
    }
    let mut args = vec!["commit", "--quiet", "-m", message];
    if body.amend {
        args.push("--amend");
    }
    if !paths.is_empty() {
        args.extend(["--only", "--"]);
        args.extend(paths.iter().map(String::as_str));
    }
    run(&root, &args, 120, "conversation.git.commit_failed", true).await?;
    let head = run(
        &root,
        &["log", "--no-show-signature", "-1", "--format=%H%x00%s"],
        30,
        GIT_ERROR,
        true,
    )
    .await?;
    let (sha, summary) = head
        .stdout
        .trim_end_matches('\n')
        .split_once('\0')
        .unwrap_or(("", ""));
    Ok(
        json!({"sha":sha,"short_sha":sha.chars().take(7).collect::<String>(),"summary":summary,"branch":branch,"amend":body.amend}),
    )
}
async fn thread_commit(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<CommitBody>,
) -> Api {
    let _guard = WRITES.lock().await;
    let root = thread_root(&s, &id)?;
    let mut result = commit(&root, body).await?;
    result["git"] = status(&s, &root, &id).await?;
    Ok(Json(result))
}
async fn thread_init(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Api {
    let _guard = WRITES.lock().await;
    let root = bound_root(&s, &id)?;
    let probe = run(&root, &["rev-parse", "--show-toplevel"], 30, GIT_ERROR, false).await?;
    if probe.status.success() && !probe.stdout.trim().is_empty() {
        return Err(failure(
            "conversation.git.already_repo",
            "Workspace is already inside a Git repository",
        ));
    }
    run(&root, &["init", "--quiet"], 30, "conversation.git.init_failed", true).await?;
    crate::control::audit(&s, "user", &id, "git", json!({"action":"init"}));
    Ok(Json(status(&s, &root, &id).await?))
}
async fn tree_is_clean(root: &FsPath) -> Result<bool, Failure> {
    let out = run(root, &["status", "--porcelain"], 30, GIT_ERROR, true).await?;
    Ok(out.stdout.trim().is_empty())
}
fn merge_back_failure(error: &tcode_control_services::worktree::MergeBackError) -> Failure {
    use tcode_control_services::worktree::MergeBackError as E;
    let (code, message) = match error {
        E::WorktreeMissing => ("conversation.git.worktree_missing", "The worktree no longer exists".to_owned()),
        E::DirtyWorktree => ("conversation.git.worktree_dirty", "Commit the changes in this worktree before merging".to_owned()),
        E::DestinationDetached => ("conversation.git.destination_detached", "The project directory is not on a branch; check one out first".to_owned()),
        E::DirtyDestination => ("conversation.git.destination_dirty", "The project directory has uncommitted changes".to_owned()),
        E::DivergedConflict => ("conversation.git.merge_conflict", "The branches diverged and cannot merge cleanly; the merge was aborted".to_owned()),
        E::Git(detail) => ("conversation.git.merge_failed", detail.clone()),
    };
    failure(code, &message)
}
async fn squash_merge_back(destination: &FsPath, worktree: &FsPath, branch: &str) -> Result<(), Failure> {
    if !worktree.is_dir() {
        return Err(merge_back_failure(&tcode_control_services::worktree::MergeBackError::WorktreeMissing));
    }
    if !tree_is_clean(worktree).await? {
        return Err(merge_back_failure(&tcode_control_services::worktree::MergeBackError::DirtyWorktree));
    }
    let head = run(destination, &["symbolic-ref", "--quiet", "--short", "HEAD"], 30, GIT_ERROR, false).await?;
    if !head.status.success() {
        return Err(merge_back_failure(&tcode_control_services::worktree::MergeBackError::DestinationDetached));
    }
    if !tree_is_clean(destination).await? {
        return Err(merge_back_failure(&tcode_control_services::worktree::MergeBackError::DirtyDestination));
    }
    let merged = run(destination, &["merge", "--squash", "--", branch], 120, "conversation.git.merge_failed", false).await?;
    if merged.status.success() {
        return Ok(());
    }
    let unmerged = run(destination, &["diff", "--name-only", "--diff-filter=U"], 30, GIT_ERROR, false).await?;
    let conflicted = !unmerged.stdout.trim().is_empty();
    run(destination, &["reset", "--merge"], 30, "conversation.git.merge_failed", true).await?;
    if conflicted {
        return Err(merge_back_failure(&tcode_control_services::worktree::MergeBackError::DivergedConflict));
    }
    Err(failure("conversation.git.merge_failed", &merged.combined_output()))
}
async fn thread_merge_worktree(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<MergeWorktreeBody>>,
) -> Api {
    let squash = body.map(|b| b.0.squash).unwrap_or(false);
    let _guard = WRITES.lock().await;
    let doc = crate::chat::load(&s, &id)?;
    if doc["thread"]["state"]["lineage"]["isolation"] != "worktree" {
        return Err(failure(
            "conversation.git.not_worktree",
            "This thread has no dedicated worktree",
        ));
    }
    let worktree = crate::native_chat::session_cwd(&s, &doc, &id)
        .map_err(|e| failure("conversation.git.not_worktree", &e.to_string()))?;
    let workspace = s
        .store
        .get("workspaces", doc["thread"]["workspace_id"].as_str().unwrap_or(""))
        .map_err(|_| failure(GIT_ERROR, "Could not read workspace metadata"))?
        .ok_or_else(|| failure("conversation.git.worktree_missing", "The worktree workspace is missing"))?;
    let branch = workspace["settings"]["branch"]
        .as_str()
        .ok_or_else(|| failure("conversation.git.not_worktree", "The worktree branch is unknown"))?
        .to_owned();
    let project = doc["thread"]["project_id"].as_str().unwrap_or("");
    let destination = project_bound_root(&s, project)?;
    let _reservation = reserve_checkout(&s, &destination).await?;
    crate::control_git::repository_boundary(&destination, &destination)
        .map_err(|e| failure("conversation.git.not_repo", &e))?;
    let outcome = if squash {
        squash_merge_back(&destination, &worktree, &branch).await?;
        "squashed"
    } else {
        let (dest, tree, name) = (destination.clone(), worktree.clone(), branch.clone());
        let merged = tokio::task::spawn_blocking(move || {
            tcode_control_services::worktree::merge_back(&dest, &tree, &name)
        })
        .await
        .map_err(|_| failure("conversation.git.merge_failed", "The merge task failed"))?
        .map_err(|e| merge_back_failure(&e))?;
        match merged {
            tcode_control_services::worktree::MergeBackOutcome::FastForward => "fast_forward",
            tcode_control_services::worktree::MergeBackOutcome::MergeCommit => "merged",
        }
    };
    crate::control::audit(
        &s,
        "user",
        &id,
        "git",
        json!({"action":"merge_worktree","squash":squash,"outcome":outcome}),
    );
    let message = match outcome {
        "fast_forward" => "Merged the worktree branch into the project directory by fast-forward",
        "merged" => "Merged the worktree branch into the project directory with a merge commit",
        _ => "Squashed the worktree branch into the project directory; the changes are staged and not yet committed",
    };
    Ok(Json(json!({
        "outcome": outcome,
        "message": message,
        "squash": squash,
        "source_branch": branch,
        "git": status(&s, &bound_root(&s, &id)?, &id).await?,
    })))
}
async fn upstream(root: &FsPath) -> Result<String, Failure> {
    let output = run(
        root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        30,
        GIT_ERROR,
        false,
    )
    .await?;
    Ok(if output.status.success() {
        output.stdout.trim().to_owned()
    } else {
        String::new()
    })
}
pub(crate) async fn push(root: &FsPath, body: PushBody) -> Result<Value, Failure> {
    let root = repo(root).await?;
    let current = branch(&root).await?;
    let name = validate_branch(&current)?;
    let upstream = upstream(&root).await?;
    if upstream.is_empty() && body.set_upstream == Some(false) {
        return Err(failure(
            "conversation.git.no_upstream",
            "Set an upstream before pushing this branch",
        ));
    }
    let set_upstream = body.set_upstream == Some(true) || upstream.is_empty();
    let remote = if !upstream.is_empty() {
        upstream.split('/').next().unwrap_or("").to_owned()
    } else {
        let list = run(&root, &["remote"], 30, GIT_ERROR, true).await?;
        let names = list
            .stdout
            .lines()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        if names.contains(&"origin") {
            "origin".to_owned()
        } else {
            names
                .first()
                .ok_or_else(|| {
                    failure(
                        "conversation.git.no_remote",
                        "Repository has no configured remote",
                    )
                })?
                .to_string()
        }
    };
    let reference = format!("refs/heads/{name}:refs/heads/{name}");
    let mut args = vec!["push", "--porcelain"];
    if set_upstream {
        args.extend(["--set-upstream", "--", &remote, &reference]);
    }
    let output = run(&root, &args, 120, "conversation.git.push_failed", true).await?;
    Ok(
        json!({"branch":name,"remote":remote,"set_upstream":set_upstream,"output":output.combined_output()}),
    )
}
async fn thread_push(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<PushBody>>,
) -> Api {
    let _guard = WRITES.lock().await;
    let root = thread_root(&s, &id)?;
    let mut result = push(&root, body.map(|b| b.0).unwrap_or_default()).await?;
    let git = status(&s, &root, &id).await?;
    for key in ["ahead", "behind", "upstream"] {
        result[key] = git[key].clone();
    }
    result["git"] = git;
    Ok(Json(result))
}
async fn pull(root: &FsPath, body: PullBody) -> Result<Value, Failure> {
    let root = repo(root).await?;
    let branch = branch(&root).await?;
    let upstream = upstream(&root).await?;
    if upstream.is_empty() {
        return Err(failure(
            "conversation.git.no_upstream",
            "Branch has no upstream to pull",
        ));
    }
    let output = run(
        &root,
        &["pull", if body.rebase { "--rebase" } else { "--ff-only" }],
        120,
        "conversation.git.pull_failed",
        false,
    )
    .await?;
    if !output.status.success() {
        let mut conflict = false;
        for marker in ["rebase-merge", "rebase-apply", "MERGE_HEAD"] {
            let probe = run(
                &root,
                &["rev-parse", "--git-path", marker],
                30,
                GIT_ERROR,
                false,
            )
            .await?;
            conflict |= probe.status.success() && root.join(probe.stdout.trim()).exists();
        }
        return Err(failure(
            if conflict {
                "conversation.git.pull_conflict"
            } else {
                "conversation.git.pull_failed"
            },
            &output.combined_output(),
        ));
    }
    Ok(
        json!({"branch":branch,"upstream":upstream,"rebase":body.rebase,"output":output.combined_output()}),
    )
}
async fn thread_pull(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<PullBody>>,
) -> Api {
    let _guard = WRITES.lock().await;
    let root = thread_root(&s, &id)?;
    let mut result = pull(&root, body.map(|b| b.0).unwrap_or_default()).await?;
    let git = status(&s, &root, &id).await?;
    for key in ["ahead", "behind"] {
        result[key] = git[key].clone();
    }
    result["git"] = git;
    Ok(Json(result))
}

pub(crate) async fn thread_checkout_op(
    s: &Arc<AppState>,
    id: &str,
    branch: String,
    create: bool,
) -> Result<Value, Failure> {
    let Json(value) = thread_checkout(State(s.clone()), Path(id.to_owned()), Json(CheckoutBody { branch, create })).await?;
    Ok(value)
}

pub(crate) async fn thread_commit_op(
    s: &Arc<AppState>,
    id: &str,
    message: String,
    paths: Option<Vec<String>>,
    amend: bool,
) -> Result<Value, Failure> {
    let Json(value) = thread_commit(State(s.clone()), Path(id.to_owned()), Json(CommitBody { message, paths, amend })).await?;
    Ok(value)
}

pub(crate) async fn thread_push_op(
    s: &Arc<AppState>,
    id: &str,
    set_upstream: Option<bool>,
) -> Result<Value, Failure> {
    let Json(value) = thread_push(State(s.clone()), Path(id.to_owned()), Some(Json(PushBody { set_upstream }))).await?;
    Ok(value)
}

pub(crate) async fn thread_pull_op(
    s: &Arc<AppState>,
    id: &str,
    rebase: bool,
) -> Result<Value, Failure> {
    let Json(value) = thread_pull(State(s.clone()), Path(id.to_owned()), Some(Json(PullBody { rebase }))).await?;
    Ok(value)
}

pub(crate) async fn thread_stage_op(
    s: &Arc<AppState>,
    id: &str,
    paths: &[String],
    unstage: bool,
) -> Result<Value, Failure> {
    let _guard = WRITES.lock().await;
    let root = thread_root(s, id)?;
    let repo_root = repo(&root).await?;
    let clean_paths = selected_paths(&repo_root, paths)?;
    if unstage {
        let mut args = vec!["restore", "--staged"];
        if clean_paths.is_empty() {
            args.push(".");
        } else {
            args.push("--");
            args.extend(clean_paths.iter().map(String::as_str));
        }
        run(&repo_root, &args, 30, "conversation.git.unstage_failed", true).await?;
    } else {
        let mut args = vec!["add", "-A"];
        if !clean_paths.is_empty() {
            args.push("--");
            args.extend(clean_paths.iter().map(String::as_str));
        }
        run(&repo_root, &args, 30, "conversation.git.stage_failed", true).await?;
    }
    status(s, &root, id).await
}

pub(crate) async fn thread_merge_worktree_op(
    s: &Arc<AppState>,
    child_thread_id: &str,
    target_thread_id: &str,
    squash: bool,
) -> Result<Value, Failure> {
    let _guard = WRITES.lock().await;
    let child_root = bound_root(s, child_thread_id)?;
    let child_repo = repo(&child_root).await?;
    let child_branch = branch(&child_repo).await?;
    let target_root = thread_root(s, target_thread_id)?;
    let target_repo = repo(&target_root).await?;
    let target_branch = branch(&target_repo).await?;
    if child_branch == target_branch {
        return Err(failure(
            "conversation.git.merge_same_branch",
            "Source worktree and target thread are on the same branch",
        ));
    }
    let args: Vec<&str> = if squash {
        vec!["merge", "--squash", "--", &child_branch]
    } else {
        vec!["merge", "--no-edit", "--", &child_branch]
    };
    let out = run(&target_repo, &args, 120, "conversation.git.merge_failed", false).await?;
    if !out.status.success() {
        return Err(failure("conversation.git.merge_conflict", &out.combined_output()));
    }
    let git = status(s, &target_root, target_thread_id).await?;
    Ok(json!({
        "source_thread_id": child_thread_id,
        "source_branch": child_branch,
        "target_thread_id": target_thread_id,
        "target_branch": target_branch,
        "squash": squash,
        "output": out.combined_output(),
        "git": git,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-b", "main"], 30, GIT_ERROR, true)
            .await
            .unwrap();
        run(
            dir.path(),
            &["config", "user.name", "Git workflow test"],
            30,
            GIT_ERROR,
            true,
        )
        .await
        .unwrap();
        run(
            dir.path(),
            &["config", "user.email", "git-test@example.invalid"],
            30,
            GIT_ERROR,
            true,
        )
        .await
        .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "initial\n").unwrap();
        commit(
            dir.path(),
            CommitBody {
                message: "initial".into(),
                paths: None,
                amend: false,
            },
        )
        .await
        .unwrap();
        dir
    }
    fn code(error: Failure) -> String {
        error.1.0["error"]["code"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn selected_commit_is_literal_and_preserves_other_staged_changes() {
        let dir = fixture().await;
        let root = dir.path();
        std::fs::write(root.join("tracked.txt"), "staged separately\n").unwrap();
        run(root, &["add", "tracked.txt"], 30, GIT_ERROR, true)
            .await
            .unwrap();
        std::fs::write(root.join("[literal].txt"), "selected\n").unwrap();
        std::fs::write(root.join("l.txt"), "must remain untracked\n").unwrap();
        let result = commit(
            root,
            CommitBody {
                message: "selected".into(),
                paths: Some(vec!["[literal].txt".into()]),
                amend: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(result["summary"], "selected");
        let names = run(
            root,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
            30,
            GIT_ERROR,
            true,
        )
        .await
        .unwrap();
        assert_eq!(names.stdout.trim(), "[literal].txt");
        let staged = run(
            root,
            &["diff", "--cached", "--name-only"],
            30,
            GIT_ERROR,
            true,
        )
        .await
        .unwrap();
        assert_eq!(staged.stdout.trim(), "tracked.txt");
        assert_eq!(
            code(
                commit(
                    root,
                    CommitBody {
                        message: "no changes".into(),
                        paths: Some(vec!["[literal].txt".into()]),
                        amend: false
                    }
                )
                .await
                .unwrap_err()
            ),
            "conversation.git.nothing_to_commit"
        );
        commit(
            root,
            CommitBody {
                message: "renamed summary".into(),
                paths: Some(vec!["[literal].txt".into()]),
                amend: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            code(
                commit(
                    root,
                    CommitBody {
                        message: "outside".into(),
                        paths: Some(vec!["../outside".into()]),
                        amend: false
                    }
                )
                .await
                .unwrap_err()
            ),
            "conversation.git.path_outside"
        );
    }

    #[tokio::test]
    async fn checkout_and_local_remote_push_pull_follow_manual_contract() {
        let dir = fixture().await;
        let remote = tempfile::tempdir().unwrap();
        run(remote.path(), &["init", "--bare"], 30, GIT_ERROR, true)
            .await
            .unwrap();
        let remote_path = remote.path().to_str().unwrap();
        run(
            dir.path(),
            &["remote", "add", "origin", remote_path],
            30,
            GIT_ERROR,
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            code(
                push(
                    dir.path(),
                    PushBody {
                        set_upstream: Some(false)
                    }
                )
                .await
                .unwrap_err()
            ),
            "conversation.git.no_upstream"
        );
        assert_eq!(
            code(pull(dir.path(), PullBody::default()).await.unwrap_err()),
            "conversation.git.no_upstream"
        );
        let pushed = push(dir.path(), PushBody::default()).await.unwrap();
        assert_eq!(pushed["set_upstream"], true);
        assert_eq!(upstream(dir.path()).await.unwrap(), "origin/main");
        assert_eq!(
            push(dir.path(), PushBody::default()).await.unwrap()["set_upstream"],
            false
        );
        pull(dir.path(), PullBody::default()).await.unwrap();
        pull(dir.path(), PullBody { rebase: true }).await.unwrap();
        checkout(
            dir.path(),
            &CheckoutBody {
                branch: "feature/manual".into(),
                create: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(branch(dir.path()).await.unwrap(), "feature/manual");
        checkout(
            dir.path(),
            &CheckoutBody {
                branch: "main".into(),
                create: false,
            },
        )
        .await
        .unwrap();
        assert!(
            checkout(
                dir.path(),
                &CheckoutBody {
                    branch: "--detach".into(),
                    create: false
                }
            )
            .await
            .is_err()
        );
        run(
            dir.path(),
            &["switch", "--detach", "HEAD"],
            30,
            GIT_ERROR,
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            code(
                commit(
                    dir.path(),
                    CommitBody {
                        message: "detached".into(),
                        paths: None,
                        amend: false
                    }
                )
                .await
                .unwrap_err()
            ),
            "conversation.git.detached_head"
        );
    }
}
