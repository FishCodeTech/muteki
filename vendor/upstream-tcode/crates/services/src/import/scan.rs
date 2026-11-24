use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    ExternalRoots, ExternalThread, RecentDir, SourceTool, collect_files, file_modified_ms,
    timestamp_ms,
};

#[derive(Debug)]
struct DesktopMeta {
    title: Option<String>,
    last_active_ms: Option<u64>,
}

pub fn scan_recent_dirs(roots: &ExternalRoots, exclude_roots: &[PathBuf]) -> Vec<RecentDir> {
    let desktop = desktop_metadata(&roots.claude_desktop_meta);
    let mut found = Vec::<(PathBuf, ExternalThread)>::new();

    for path in claude_session_files(&roots.claude_projects) {
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let mut head = None;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            // Subagent/workflow transcripts share the session's cwd but are not
            // conversations of their own.
            if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                head = None;
                break;
            }
            let Some(cwd) = value.get("cwd").and_then(Value::as_str) else {
                continue;
            };
            let id = value
                .get("sessionId")
                .and_then(Value::as_str)
                .or_else(|| path.file_stem().and_then(|name| name.to_str()))
                .unwrap_or_default()
                .to_string();
            let entrypoint = value
                .get("entrypoint")
                .and_then(Value::as_str)
                .map(str::to_string);
            head = Some((
                PathBuf::from(cwd),
                id,
                entrypoint,
                value.get("timestamp").and_then(timestamp_ms),
            ));
            break;
        }
        let Some((cwd, id, entrypoint, head_ts)) = head else {
            continue;
        };
        let joined = desktop.get(&id);
        let source = if joined.is_some() {
            SourceTool::ClaudeDesktop
        } else if entrypoint.as_deref() == Some("sdk-ts") {
            SourceTool::T3Code
        } else if entrypoint.as_deref() == Some("claude-desktop") {
            SourceTool::ClaudeDesktop
        } else {
            SourceTool::ClaudeCode
        };
        found.push((
            cwd,
            ExternalThread {
                source,
                file: path.clone(),
                external_id: format!("claude:{id}"),
                title_hint: joined.and_then(|meta| meta.title.clone()),
                last_active_ms: joined
                    .and_then(|meta| meta.last_active_ms)
                    .unwrap_or_else(|| file_modified_ms(&path).max(head_ts.unwrap_or(0))),
            },
        ));
    }

    for root in &roots.codex_session_roots {
        for path in collect_files(root, &|path| {
            path.extension().is_some_and(|ext| ext == "jsonl")
        }) {
            let Ok(file) = File::open(&path) else {
                continue;
            };
            let Some(Ok(line)) = BufReader::new(file).lines().next() else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("session_meta") {
                continue;
            }
            let payload = value.get("payload").unwrap_or(&Value::Null);
            let Some(id) = payload.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(cwd) = payload.get("cwd").and_then(Value::as_str) else {
                continue;
            };
            let originator = payload.get("originator").and_then(Value::as_str);
            if originator == Some("tcode") {
                continue;
            }
            found.push((
                PathBuf::from(cwd),
                ExternalThread {
                    source: if originator == Some("Codex Desktop") {
                        SourceTool::CodexDesktop
                    } else {
                        SourceTool::CodexCli
                    },
                    file: path.clone(),
                    external_id: format!("codex:{id}"),
                    title_hint: None,
                    last_active_ms: file_modified_ms(&path)
                        .max(value.get("timestamp").and_then(timestamp_ms).unwrap_or(0)),
                },
            ));
        }
    }

    let mut groups = HashMap::<PathBuf, Vec<ExternalThread>>::new();
    for (cwd, thread) in found {
        groups.entry(cwd).or_default().push(thread);
    }
    // Filesystem checks run once per directory, not per thread: each one can
    // block on a slow or unreachable mount.
    let excluded: Vec<_> = exclude_roots
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect();
    let mut dirs: Vec<_> = groups
        .into_iter()
        .filter(|(cwd, _)| {
            cwd.is_dir() && {
                let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
                !excluded.contains(&cwd)
            }
        })
        .map(|(path, mut threads)| {
            threads.sort_by_key(|thread| std::cmp::Reverse(thread.last_active_ms));
            RecentDir {
                last_active_ms: threads.first().map_or(0, |thread| thread.last_active_ms),
                path,
                threads,
            }
        })
        .collect();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.last_active_ms));
    dirs
}

/// Session transcripts live directly under `projects/<slug>/` — one `.jsonl`
/// per session. Deeper trees (`<session-id>/subagents/**`, `workflows/**`) hold
/// sidechain material, so recursing would surface non-threads.
fn claude_session_files(projects_root: &Path) -> Vec<PathBuf> {
    let Ok(slugs) = std::fs::read_dir(projects_root) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for slug in slugs.flatten() {
        let dir = slug.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    files
}

fn desktop_metadata(root: &Path) -> HashMap<String, DesktopMeta> {
    let mut metadata = HashMap::new();
    for path in collect_files(root, &|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("local_") && name.ends_with(".json"))
    }) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(id) = value.get("cliSessionId").and_then(Value::as_str) else {
            continue;
        };
        metadata.insert(
            id.to_string(),
            DesktopMeta {
                title: value
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                last_active_ms: value.get("lastActivityAt").and_then(Value::as_u64),
            },
        );
    }
    metadata
}
