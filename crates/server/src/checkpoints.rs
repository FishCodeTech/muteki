//! Turn-level workspace checkpoints owned by Muteki.
//!
//! Before a turn runs, the workspace (tracked and untracked files, honouring
//! `.gitignore`) is written as a git tree and pinned under
//! `refs/muteki/checkpoints/<thread>/<turn>`. Restoring brings files inside the
//! workspace scope back to that tree: changed or deleted files are checked out
//! again and files created since are removed. The repository index, HEAD and
//! branches are never touched, and ignored files stay outside both snapshots.
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const REF_PREFIX: &str = "refs/muteki/checkpoints";
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointError {
    pub code: &'static str,
    pub message: String,
}

impl CheckpointError {
    fn new(message: impl Into<String>) -> Self {
        Self::coded("conversation.checkpoint.failed", message)
    }

    fn coded(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

struct Scope {
    top: PathBuf,
    git_dir: PathBuf,
    pathspec: String,
}

struct Output {
    ok: bool,
    stdout: String,
}

async fn git(
    top: &Path,
    args: &[&str],
    index: Option<&Path>,
    stdin: Option<String>,
    check: bool,
) -> Result<Output, CheckpointError> {
    let mut command = crate::host_process::command("git");
    command
        .args(["--no-pager", "--literal-pathspecs", "-c", "core.quotePath=false", "-C"])
        .arg(top)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let name = args.first().copied().unwrap_or("git");
    let mut child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => CheckpointError::new("本机未找到 git 可执行文件"),
        _ => CheckpointError::new(format!("无法启动 git：{error}")),
    })?;
    if let Some(input) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        pipe.write_all(input.as_bytes())
            .await
            .map_err(|error| CheckpointError::new(format!("git {name} 输入失败：{error}")))?;
    }
    let output = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| CheckpointError::new(format!("git {name} 超时")))?
        .map_err(|error| CheckpointError::new(format!("git {name} 失败：{error}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if check && !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = Some(stderr.trim())
            .filter(|text| !text.is_empty())
            .or(Some(stdout.trim()).filter(|text| !text.is_empty()))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("exit {}", output.status.code().unwrap_or(-1)));
        return Err(CheckpointError::new(format!("git {name} 失败：{}", crate::git_process::redact(&detail))));
    }
    Ok(Output { ok: output.status.success(), stdout })
}

async fn scope(root: &str) -> Result<Option<Scope>, CheckpointError> {
    let root = root.trim();
    if root.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(root);
    let resolved = tokio::fs::canonicalize(&path)
        .await
        .map_err(|_| CheckpointError::new(format!("工作区目录不存在：{root}")))?;
    let found = git(&resolved, &["rev-parse", "--show-toplevel"], None, None, false).await?;
    if !found.ok {
        return Ok(None);
    }
    let top = tokio::fs::canonicalize(found.stdout.trim())
        .await
        .map_err(|error| CheckpointError::new(format!("无法解析 Git 仓库根目录：{error}")))?;
    let Ok(relative) = resolved.strip_prefix(&top) else {
        return Ok(None);
    };
    let git_dir = PathBuf::from(
        git(&top, &["rev-parse", "--absolute-git-dir"], None, None, true)
            .await?
            .stdout
            .trim(),
    );
    let pathspec = if relative.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        relative.to_string_lossy().replace('\\', "/")
    };
    Ok(Some(Scope { top, git_dir, pathspec }))
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

pub fn checkpoint_ref(thread_id: &str, turn_id: &str) -> Result<String, CheckpointError> {
    for value in [thread_id, turn_id] {
        if !safe_id(value) {
            return Err(CheckpointError::coded(
                "conversation.checkpoint.invalid_id",
                format!("检查点标识无效：{value:?}"),
            ));
        }
    }
    Ok(format!("{REF_PREFIX}/{thread_id}/{turn_id}"))
}

/// A throwaway index next to the real one, removed when dropped.
struct TempIndex(PathBuf);

impl TempIndex {
    async fn new(scope: &Scope, seed_from_index: bool) -> Result<Self, CheckpointError> {
        let temp = TempIndex(scope.git_dir.join(format!("muteki-checkpoint-{}.index", uuid::Uuid::new_v4().simple())));
        let real = scope.git_dir.join("index");
        if seed_from_index && tokio::fs::try_exists(&real).await.unwrap_or(false) {
            tokio::fs::copy(&real, &temp.0)
                .await
                .map_err(|error| CheckpointError::new(format!("无法准备检查点索引：{error}")))?;
        }
        Ok(temp)
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn snapshot_tree(scope: &Scope) -> Result<String, CheckpointError> {
    let index = TempIndex::new(scope, true).await?;
    git(&scope.top, &["add", "-A", "--", &scope.pathspec], Some(&index.0), None, true).await?;
    Ok(git(&scope.top, &["write-tree"], Some(&index.0), None, true)
        .await?
        .stdout
        .trim()
        .to_owned())
}

async fn resolve(scope: &Scope, reference: &str) -> Result<Option<String>, CheckpointError> {
    let spec = format!("{reference}^{{tree}}");
    let found = git(&scope.top, &["rev-parse", "--verify", "--quiet", &spec], None, None, false).await?;
    let tree = found.stdout.trim();
    Ok((found.ok && !tree.is_empty()).then(|| tree.to_owned()))
}

async fn changes(scope: &Scope, checkpoint: &str) -> Result<Vec<Value>, CheckpointError> {
    let current = snapshot_tree(scope).await?;
    let output = git(
        &scope.top,
        &["diff", "--no-renames", "--name-status", "-z", checkpoint, &current, "--", &scope.pathspec],
        None,
        None,
        true,
    )
    .await?
    .stdout;
    let parts: Vec<&str> = output.split('\0').filter(|part| !part.is_empty()).collect();
    Ok(parts
        .chunks_exact(2)
        .map(|pair| {
            let status = &pair[0][..1];
            let action = if status == "A" { "delete" } else { "restore" };
            json!({"path":pair[1],"status":status,"action":action})
        })
        .collect())
}

/// Pin the current workspace state for `turn_id`. `None` outside a git repo.
pub async fn capture(root: &str, thread_id: &str, turn_id: &str) -> Result<Option<String>, CheckpointError> {
    let reference = checkpoint_ref(thread_id, turn_id)?;
    let Some(scope) = scope(root).await? else { return Ok(None) };
    let tree = snapshot_tree(&scope).await?;
    git(&scope.top, &["update-ref", &reference, &tree], None, None, true).await?;
    Ok(Some(tree))
}

/// What restoring `turn_id`'s checkpoint would change, for previews.
pub async fn status(root: &str, thread_id: &str, turn_id: &str) -> Result<Value, CheckpointError> {
    let reference = checkpoint_ref(thread_id, turn_id)?;
    let Some(scope) = scope(root).await? else {
        return Ok(json!({"available":false,"reason":"not_git","changes":[]}));
    };
    let Some(checkpoint) = resolve(&scope, &reference).await? else {
        return Ok(json!({"available":false,"reason":"missing","changes":[]}));
    };
    Ok(json!({"available":true,"reason":"","changes":changes(&scope, &checkpoint).await?}))
}

/// The preview entry for a turn, folding probe failures into the row.
pub async fn preview(root: &str, thread_id: &str, turn_id: &str) -> Value {
    status(root, thread_id, turn_id)
        .await
        .unwrap_or_else(|error| json!({"available":false,"reason":"error","error":error.message,"changes":[]}))
}

fn prune_empty_dirs(path: &Path, stop: &Path) {
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == stop || !dir.starts_with(stop) || std::fs::remove_dir(dir).is_err() {
            return;
        }
        parent = dir.parent();
    }
}

pub async fn restore(root: &str, thread_id: &str, turn_id: &str) -> Result<Value, CheckpointError> {
    let reference = checkpoint_ref(thread_id, turn_id)?;
    let Some(scope) = scope(root).await? else {
        return Err(CheckpointError::coded("conversation.checkpoint.not_git", "工作区不是 Git 仓库，无法回退文件"));
    };
    let Some(checkpoint) = resolve(&scope, &reference).await? else {
        return Err(CheckpointError::coded(
            "conversation.checkpoint.missing",
            "这一轮没有文件检查点（早于检查点功能，或开始时未能保存），只能保留文件",
        ));
    };
    let rows = changes(&scope, &checkpoint).await?;
    let paths = |action: &str| -> Vec<String> {
        rows.iter()
            .filter(|row| row["action"] == action)
            .filter_map(|row| row["path"].as_str().map(str::to_owned))
            .collect()
    };
    let deleted = paths("delete");
    let restored = paths("restore");
    for relative in &deleted {
        let target = scope.top.join(relative);
        let removable = std::fs::symlink_metadata(&target).is_ok_and(|meta| meta.file_type().is_symlink() || meta.is_file());
        if removable {
            std::fs::remove_file(&target)
                .map_err(|error| CheckpointError::new(format!("无法删除 {relative}：{error}")))?;
            prune_empty_dirs(&target, &scope.top);
        }
    }
    if !restored.is_empty() {
        let index = TempIndex::new(&scope, false).await?;
        git(&scope.top, &["read-tree", &checkpoint], Some(&index.0), None, true).await?;
        let list = restored.iter().map(|path| format!("{path}\0")).collect::<String>();
        git(&scope.top, &["checkout-index", "-f", "-z", "--stdin"], Some(&index.0), Some(list), true).await?;
    }
    Ok(json!({"restored":restored,"deleted":deleted}))
}

pub async fn dirty_files(root: &str) -> Result<(bool, Vec<String>), CheckpointError> {
    let Some(scope) = scope(root).await? else { return Ok((false, Vec::new())) };
    let output = git(
        &scope.top,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--", &scope.pathspec],
        None,
        None,
        true,
    )
    .await?
    .stdout;
    let mut files = Vec::new();
    let mut entries = output.split('\0').filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() > 3 {
            files.push(entry[3..].to_owned());
        }
        if matches!(entry.as_bytes().first(), Some(b'R' | b'C')) {
            entries.next();
        }
    }
    Ok((!files.is_empty(), files))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sh(dir: &Path, args: &[&str]) {
        let status = crate::host_process::command("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-C"])
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[tokio::test]
    async fn a_checkpoint_restores_changed_deleted_and_created_files() {
        let dir = std::env::temp_dir().join(format!("muteki-checkpoint-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        sh(&dir, &["init", "-q"]).await;
        std::fs::write(dir.join("src/keep.txt"), "one").unwrap();
        std::fs::write(dir.join("gone.txt"), "here").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        sh(&dir, &["add", "-A"]).await;
        sh(&dir, &["commit", "-qm", "init"]).await;
        std::fs::write(dir.join("untracked.txt"), "draft").unwrap();
        let root = dir.to_string_lossy().into_owned();

        assert!(capture(&root, "thread-1", "turn-1").await.unwrap().is_some());
        assert_eq!(status(&root, "thread-1", "turn-1").await.unwrap()["changes"], json!([]));
        assert_eq!(status(&root, "thread-1", "turn-2").await.unwrap()["reason"], "missing");

        std::fs::write(dir.join("src/keep.txt"), "two").unwrap();
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        std::fs::create_dir_all(dir.join("new/deep")).unwrap();
        std::fs::write(dir.join("new/deep/made.txt"), "x").unwrap();
        std::fs::write(dir.join("ignored.txt"), "local").unwrap();
        let preview = status(&root, "thread-1", "turn-1").await.unwrap();
        let mut actions: Vec<String> = preview["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| format!("{}:{}", row["action"].as_str().unwrap(), row["path"].as_str().unwrap()))
            .collect();
        actions.sort();
        assert_eq!(actions, ["delete:new/deep/made.txt", "restore:gone.txt", "restore:src/keep.txt"]);
        let (dirty, files) = dirty_files(&root).await.unwrap();
        assert!(dirty && files.contains(&"new/deep/made.txt".to_owned()));

        let result = restore(&root, "thread-1", "turn-1").await.unwrap();
        assert_eq!(result["deleted"], json!(["new/deep/made.txt"]));
        assert_eq!(std::fs::read_to_string(dir.join("src/keep.txt")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(dir.join("gone.txt")).unwrap(), "here");
        assert_eq!(std::fs::read_to_string(dir.join("untracked.txt")).unwrap(), "draft");
        assert_eq!(std::fs::read_to_string(dir.join("ignored.txt")).unwrap(), "local");
        assert!(!dir.join("new").exists());
        let staged = git(&dir, &["diff", "--cached", "--name-only"], None, None, true).await.unwrap();
        assert_eq!(staged.stdout.trim(), "");
        assert!(std::fs::read_dir(dir.join(".git")).unwrap().all(|entry| !entry.unwrap().file_name().to_string_lossy().starts_with("muteki-checkpoint-")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn outside_git_there_is_nothing_to_checkpoint() {
        let dir = std::env::temp_dir().join(format!("muteki-plain-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_string_lossy().into_owned();
        assert_eq!(capture(&root, "t", "u").await.unwrap(), None);
        assert_eq!(status(&root, "t", "u").await.unwrap()["reason"], "not_git");
        assert_eq!(restore(&root, "t", "u").await.unwrap_err().code, "conversation.checkpoint.not_git");
        assert_eq!(checkpoint_ref("t", "../x").unwrap_err().code, "conversation.checkpoint.invalid_id");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
