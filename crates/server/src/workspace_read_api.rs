//! Read-only file surfaces for a conversation's registered workspace.
use crate::AppState;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;

const MAX_PREVIEW: usize = 512_000;
const RECORDED_EVENT_SCAN: usize = 512;
const MAX_SEARCH_FILE: usize = 2_000_000;
const IGNORED: &[&str] = &[
    ".git",
    ".next",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
];
const SEARCH_IGNORED: &[&str] = &[
    ".git",
    ".next",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
    "coverage",
    ".turbo",
    ".cache",
];
type Error = (StatusCode, Json<Value>);
type Api = Result<Json<Value>, Error>;

fn typed(status: StatusCode, code: &str, message: &str) -> Error {
    (
        status,
        Json(json!({"detail":message,"error":{"code":code,"message":message}})),
    )
}
fn fail(status: StatusCode, message: &str) -> Error {
    typed(status, "conversation.workspace.surface_failed", message)
}
fn io_error(_: std::io::Error) -> Error {
    fail(
        StatusCode::BAD_REQUEST,
        "Workspace path is unavailable or unreadable",
    )
}
pub(crate) fn bound_root(s: &AppState, thread: &str) -> Result<PathBuf, Error> {
    let read = |collection: &str, key: &str| {
        s.store.get(collection, key).map_err(|_| {
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not read workspace metadata",
            )
        })
    };
    let thread = read("conversation_threads", thread)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Thread not found"))?;
    let wid = thread["thread"]["workspace_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "This conversation has no workspace"))?;
    let workspace = read("workspaces", wid)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Workspace not found"))?;
    let raw = workspace["root_path"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Workspace root is unavailable"))?;
    canonical_workspace_root(raw)
}
pub(crate) fn canonical_workspace_root(raw: &str) -> Result<PathBuf, Error> {
    let expanded = if raw == "~" || raw.starts_with("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|p| p.join(raw.strip_prefix("~/").unwrap_or("")))
            .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "Workspace root is unavailable"))?
    } else {
        PathBuf::from(raw)
    };
    let root = expanded.canonicalize().map_err(io_error)?;
    if !root.is_dir() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "Workspace root is unavailable",
        ));
    }
    Ok(root)
}
fn resolve(root: &FsPath, raw: &str) -> Result<PathBuf, Error> {
    let input = FsPath::new(raw);
    if input
        .components()
        .any(|p| matches!(p, Component::ParentDir))
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "Path is outside this workspace",
        ));
    }
    let path = root.join(input).canonicalize().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            fail(StatusCode::NOT_FOUND, "File does not exist or was deleted")
        } else {
            io_error(e)
        }
    })?;
    if !path.starts_with(root) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "Path is outside this workspace",
        ));
    }
    Ok(path)
}
pub fn recorded_read(s: &AppState, thread: &str, requested: &str, target: &FsPath) -> bool {
    let Some(name) = FsPath::new(requested)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    let canonical_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(name);
    let Ok(events) = s
        .store
        .chat_events_mentioning(thread, name, canonical_name, RECORDED_EVENT_SCAN)
    else {
        return false;
    };
    events
        .iter()
        .flat_map(crate::tool_resources::referenced_paths)
        .any(|path| {
            FsPath::new(&path)
                .canonicalize()
                .is_ok_and(|read| read == target)
        })
}
fn readable(s: &AppState, thread: &str, root: &FsPath, raw: &str) -> Result<PathBuf, Error> {
    if !FsPath::new(raw).is_absolute() {
        return resolve(root, raw);
    }
    let path = FsPath::new(raw).canonicalize().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            fail(StatusCode::NOT_FOUND, "File does not exist or was deleted")
        } else {
            io_error(e)
        }
    })?;
    if path.starts_with(root) || recorded_read(s, thread, raw, &path) {
        return Ok(path);
    }
    Err(typed(
        StatusCode::FORBIDDEN,
        "conversation.workspace.path_not_recorded",
        "This file is outside the workspace and this conversation has no tool read of it",
    ))
}
fn relative(root: &FsPath, path: &FsPath) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
async fn blocking<F>(f: F) -> Api
where
    F: FnOnce() -> Api + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "Workspace read failed"))?
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/threads/{id}/git", get(thread_git_status))
        .route("/api/threads/{id}/workspace/files", get(directory))
        .route("/api/threads/{id}/workspace/file", get(file))
        .route("/api/threads/{id}/workspace/file/raw", get(raw))
        .route(
            "/api/threads/{id}/workspace/search/files",
            get(search_files),
        )
        .route(
            "/api/threads/{id}/workspace/search/content",
            get(search_content),
        )
        .route("/api/threads/{id}/workspace/diff", get(diff))
}
#[derive(Default, Deserialize)]
struct FileQuery {
    #[serde(default)]
    path: String,
    line: Option<i64>,
    #[serde(default)]
    download: bool,
}
async fn directory(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Api {
    let root = bound_root(&s, &id)?;
    blocking(move || {
        let dir = resolve(&root, &q.path)?;
        if !dir.is_dir() { return Err(fail(StatusCode::BAD_REQUEST, "Target is not a directory")); }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&dir).map_err(io_error)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(meta) = fs::symlink_metadata(entry.path()) else { continue };
            if IGNORED.contains(&name.as_str()) || meta.is_symlink() || !(meta.is_dir() || meta.is_file()) { continue; }
            entries.push(json!({"name":name,"path":relative(&root,&entry.path()),"kind":if meta.is_dir(){"directory"}else{"file"},"size":if meta.is_dir(){Value::Null}else{json!(meta.len())}}));
        }
        entries.sort_by_key(|v| (v["kind"] != "directory", v["name"].as_str().unwrap_or("").to_lowercase()));
        Ok(Json(json!({"path":relative(&root,&dir),"entries":entries})))
    }).await
}
fn extension(path: &FsPath) -> String {
    path.extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
}
fn media_type(path: &FsPath) -> &'static str {
    match extension(path).as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "html" | "htm" => "text/html",
        "xml" => "application/xml",
        "md" | "markdown" | "mdx" => "text/markdown",
        "json" => "application/json",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "ts" | "tsx" => "text/typescript",
        "py" => "text/x-python",
        "txt" | "log" => "text/plain",
        "yaml" | "yml" => "text/yaml",
        _ => "application/octet-stream",
    }
}
fn preview_kind(path: &FsPath, raw: &[u8], size: u64) -> &'static str {
    if size > MAX_PREVIEW as u64 {
        return "too_large";
    }
    let ext = extension(path);
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg" => return "image",
        "pdf" => return "pdf",
        "html" | "htm" => return "html",
        "md" | "markdown" | "mdx" => return "markdown",
        _ => (),
    }
    let sample = &raw[..raw.len().min(8192)];
    if sample.contains(&0) {
        return "binary";
    }
    match ext.as_str() {
        "txt" | "log" => "text",
        "py" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "json" | "css" | "rs" | "go"
        | "java" | "kt" | "c" | "h" | "cpp" | "hpp" | "cs" | "rb" | "php" | "swift" | "sh"
        | "bash" | "zsh" | "yml" | "yaml" | "toml" | "xml" | "sql" | "r" | "lua" | "vue"
        | "svelte" | "diff" | "patch" | "ini" | "cfg" | "env" => "code",
        _ if std::str::from_utf8(sample).is_ok() => "text",
        _ => "binary",
    }
}
fn read_limited(path: &FsPath, limit: usize) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(io_error)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    Ok(bytes)
}
async fn file(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Api {
    let root = bound_root(&s, &id)?;
    blocking(move || {
        let path = readable(&s, &id, &root, &q.path)?;
        let meta = fs::metadata(&path).map_err(io_error)?;
        if !meta.is_file() { return Err(fail(StatusCode::NOT_FOUND, "File does not exist or was deleted")); }
        let mut size = meta.len();
        let raw = if size <= MAX_PREVIEW as u64 { read_limited(&path, MAX_PREVIEW)? } else { Vec::new() };
        if raw.len() > MAX_PREVIEW { size = raw.len() as u64; }
        let kind = preview_kind(&path, &raw, size);
        let content = if matches!(kind, "text" | "code" | "markdown" | "html") {
            Some(String::from_utf8_lossy(&raw).into_owned())
        } else { None };
        let message = match kind {
            "too_large" => Some("File exceeds the 500 KB preview limit. Download the original file to view it."),
            "binary" => Some("Inline preview is unavailable for this file type. Download the original file."),
            "html" => Some("HTML previews render in a sandboxed iframe."), _ => None,
        };
        Ok(Json(json!({"path":relative(&root,&path),"in_workspace":path.starts_with(&root),"size":size,"media_type":media_type(&path),"preview_kind":kind,"max_preview_bytes":MAX_PREVIEW,"truncated":false,"downloadable":true,"line":q.line.filter(|n|*n>0),"content":content,"message":message})))
    }).await
}
async fn raw(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Result<Response, Error> {
    let root = bound_root(&s, &id)?;
    let path = readable(&s, &id, &root, &q.path)?;
    let file = tokio::fs::File::open(&path).await.map_err(io_error)?;
    if !file.metadata().await.map_err(io_error)?.is_file() {
        return Err(fail(
            StatusCode::NOT_FOUND,
            "File does not exist or was deleted",
        ));
    }
    let original = media_type(&path);
    let unsafe_inline = matches!(original, "text/html" | "image/svg+xml" | "application/xml");
    let media = if unsafe_inline {
        "application/octet-stream"
    } else {
        original
    };
    let disposition = if q.download || unsafe_inline {
        "attachment"
    } else {
        "inline"
    };
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    let fallback: String = filename
        .chars()
        .take(180)
        .map(|c| {
            if c.is_ascii_alphanumeric() || " ._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = filename
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut bytes = vec![0; 64 * 1024];
        let n = file.read(&mut bytes).await?;
        if n == 0 {
            Ok::<_, std::io::Error>(None)
        } else {
            bytes.truncate(n);
            Ok(Some((bytes, file)))
        }
    });
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(media));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "{disposition}; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
        ))
        .map_err(|_| fail(StatusCode::BAD_REQUEST, "Invalid filename"))?,
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    limit: Option<usize>,
}
async fn search_files(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Api {
    search(s, id, q, false).await
}
async fn search_content(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Api {
    let root = bound_root(&s,&id)?;
    let query = q.q.trim();
    if query.is_empty() { return Ok(Json(json!({"results":[],"truncated":false,"query":query}))); }
    let limit = q.limit.unwrap_or(30).clamp(1,100);
    let mut command = crate::host_process::command("rg");
    command.current_dir(&root).args(["--json","--line-number","--color=never","--max-count=3","--smart-case"])
        .arg(format!("--max-filesize={MAX_SEARCH_FILE}"));
    for ignored in SEARCH_IGNORED { command.args(["--glob",&format!("!{ignored}/**")]); }
    command.args(["-e",query,"--","."]).kill_on_drop(true);
    let output = match tokio::time::timeout(Duration::from_secs(20),command.output()).await {
        Err(_) => return Err(fail(StatusCode::GATEWAY_TIMEOUT,"内容搜索超时（20 秒）")),
        Ok(Err(error)) if error.kind()==std::io::ErrorKind::NotFound => return search(s,id,q,true).await,
        Ok(Err(_)) => return Err(fail(StatusCode::SERVICE_UNAVAILABLE,"搜索工具不可用")),
        Ok(Ok(output)) => output,
    };
    if !matches!(output.status.code(),Some(0 | 1)) {
        return Err(fail(StatusCode::BAD_REQUEST,&format!("内容搜索失败：{}",String::from_utf8_lossy(&output.stderr).trim())));
    }
    let mut results = Vec::new();let mut truncated = false;
    for line in output.stdout.split(|byte|*byte==b'\n') {
        let Ok(row) = serde_json::from_slice::<Value>(line) else { continue };
        if row["type"] != "match" {continue;}
        let decode = |value: &Value| -> String {
            if let Some(text)=value["text"].as_str() {return text.to_owned();}
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(value["bytes"].as_str().unwrap_or(""))
                .map(|bytes|String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default()
        };
        let path = root.join(decode(&row["data"]["path"]));
        if path.is_symlink() {continue;}
        let Ok(canonical)=path.canonicalize() else {continue};
        if !canonical.starts_with(&root) {continue;}
        if results.len()>=limit {truncated=true;break;}
        results.push(json!({"path":relative(&root,&canonical),"line":row["data"]["line_number"],
            "snippet":decode(&row["data"]["lines"]).trim_end_matches(['\r','\n']).chars().take(200).collect::<String>()}));
    }
    Ok(Json(json!({"results":results,"truncated":truncated,"query":query})))
}
async fn search(s: Arc<AppState>, id: String, q: SearchQuery, content: bool) -> Api {
    let root = bound_root(&s, &id)?;
    blocking(move || {
        let query = q.q.trim();
        let needle = query.to_lowercase();
        let pattern = if content && !query.is_empty() {
            Some(regex::RegexBuilder::new(query)
                .case_insensitive(!query.chars().any(char::is_uppercase))
                .build()
                .map_err(|_| fail(StatusCode::BAD_REQUEST, "Invalid content search regular expression"))?)
        } else { None };
        let limit = q.limit.unwrap_or(if content {30} else {60}).clamp(1,100);
        let mut results = Vec::new();
        let mut truncated = false;
        if !query.is_empty() {
            let started = Instant::now();
            let mut dirs = vec![root.clone()];
            'walk: while let Some(dir) = dirs.pop() {
                if started.elapsed() > Duration::from_secs(20) { return Err(fail(StatusCode::GATEWAY_TIMEOUT,"内容搜索超时（20 秒）")); }
                let Ok(canonical) = dir.canonicalize() else { continue };
                if !canonical.starts_with(&root) { continue; }
                let Ok(iter) = fs::read_dir(&canonical) else { continue };
                let mut entries: Vec<_> = iter.flatten().collect();
                entries.sort_by_key(|e| e.file_name());
                for entry in entries {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let Ok(meta) = fs::symlink_metadata(&path) else { continue };
                    if meta.is_symlink() || SEARCH_IGNORED.contains(&name.as_str()) { continue; }
                    if meta.is_dir() { dirs.push(path.clone()); }
                    if !(meta.is_dir() || meta.is_file()) { continue; }
                    let rel = relative(&root, &path);
                    if content {
                        if !meta.is_file() || meta.len() > MAX_SEARCH_FILE as u64 { continue; }
                        let Ok(bytes) = read_limited(&path, MAX_SEARCH_FILE) else { continue };
                        if bytes.len() > MAX_SEARCH_FILE || bytes.contains(&0) { continue; }
                        let text = String::from_utf8_lossy(&bytes);
                        let mut hits = 0;
                        for (index, line) in text.lines().enumerate() {
                            if !pattern.as_ref().is_some_and(|re| re.is_match(line)) { continue; }
                            results.push(json!({"path":rel,"line":index+1,"snippet":line.chars().take(200).collect::<String>()}));
                            hits += 1;
                            if results.len() >= limit { truncated = true; break 'walk; }
                            if hits >= 3 { break; }
                        }
                    } else if rel.to_lowercase().contains(&needle) {
                        results.push(json!({"name":name,"path":rel,"kind":if meta.is_dir(){"directory"}else{"file"},"size":if meta.is_dir(){Value::Null}else{json!(meta.len())}}));
                        if results.len() >= limit { truncated = true; break 'walk; }
                    }
                }
            }
        }
        if !content {
            results.sort_by_key(|v| {
                let name = v["name"].as_str().unwrap_or("").to_lowercase();
                if name == needle {0} else if name.starts_with(&needle) {1} else {2}
            });
        }
        let mut result = json!({"results":results,"truncated":truncated,"query":query});
        if !content { let mut ignored = SEARCH_IGNORED.to_vec(); ignored.sort(); result["ignored_dirs"] = json!(ignored); }
        Ok(Json(result))
    }).await
}
#[derive(Default, Deserialize)]
struct DiffQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    staging: String,
    #[serde(default)]
    baseline: String,
    #[serde(default)]
    base: String,
}

const MAX_DIFF_OUTPUT: usize = 8_000_000;

async fn git_read(root: &FsPath, args: &[&str]) -> Result<Option<String>, Error> {
    use std::process::Stdio;
    let mut child = crate::host_process::command("git")
        .args([
            "--no-pager",
            "--literal-pathspecs",
            "-c",
            "core.quotePath=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "diff.submodule=short",
            "-C",
        ])
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env_remove("GIT_EXTERNAL_DIFF")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| fail(StatusCode::BAD_REQUEST, "Git is unavailable"))?;
    let mut stdout = child
        .stdout
        .take()
        .expect("piped git output")
        .take(MAX_DIFF_OUTPUT as u64 + 1);
    let read = async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map_err(io_error)?;
        if bytes.len() > MAX_DIFF_OUTPUT {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "Diff exceeds the 8 MB preview limit; select a file",
            ));
        }
        let status = child.wait().await.map_err(io_error)?;
        Ok(status
            .success()
            .then(|| String::from_utf8_lossy(&bytes).into_owned()))
    };
    tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .map_err(|_| fail(StatusCode::BAD_REQUEST, "Git read timed out (30 seconds)"))?
}

fn patch_counts(patch: &str) -> (usize, usize) {
    let additions = patch
        .lines()
        .filter(|row| row.starts_with('+') && !row.starts_with("+++"))
        .count();
    let deletions = patch
        .lines()
        .filter(|row| row.starts_with('-') && !row.starts_with("---"))
        .count();
    (additions, deletions)
}

fn git_path(value: &str) -> String {
    if !value.starts_with('"') {
        return value.to_owned();
    }
    let bytes = value.trim_matches('"').as_bytes();
    let mut decoded = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1;
            if (b'0'..=b'7').contains(&bytes[i]) {
                let mut n = 0u16;
                let mut count = 0;
                while i < bytes.len() && count < 3 && (b'0'..=b'7').contains(&bytes[i]) {
                    n = n * 8 + (bytes[i] - b'0') as u16;
                    i += 1;
                    count += 1;
                }
                decoded.push(n as u8);
                continue;
            }
            decoded.push(match bytes[i] {
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                other => other,
            });
        } else {
            decoded.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn patch_files(patch: &str, staging: &str) -> Vec<Value> {
    let mut chunks = Vec::<Vec<&str>>::new();
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            chunks.push(Vec::new());
        }
        if let Some(chunk) = chunks.last_mut() {
            chunk.push(line);
        }
    }
    chunks
        .into_iter()
        .map(|chunk| {
            let header = chunk[0].strip_prefix("diff --git ").unwrap_or("");
            let (old, new) = if let Some((a, b)) = header.split_once(" b/") {
                (a.trim_start_matches("a/").to_owned(), b.to_owned())
            } else if let Some((a, b)) = header.split_once(" \"b/") {
                (
                    git_path(a).trim_start_matches("a/").to_owned(),
                    git_path(&format!("\"b/{b}"))
                        .trim_start_matches("b/")
                        .to_owned(),
                )
            } else {
                (String::new(), "unknown".to_owned())
            };
            let mut path = new;
            let mut old_path = None;
            let mut status = "M";
            let mut binary = false;
            for line in &chunk[1..] {
                if line.starts_with("new file mode") {
                    status = "A";
                }
                if line.starts_with("deleted file mode") {
                    status = "D";
                }
                if let Some(v) = line.strip_prefix("--- ") {
                    let v = git_path(v.trim_end_matches('\t'));
                    if v != "/dev/null" && status == "D" {
                        path = v.trim_start_matches("a/").to_owned();
                    }
                }
                if let Some(v) = line.strip_prefix("+++ ") {
                    let v = git_path(v.trim_end_matches('\t'));
                    if v != "/dev/null" {
                        path = v.trim_start_matches("b/").to_owned();
                    }
                }
                if let Some(v) = line.strip_prefix("rename from ") {
                    old_path = Some(git_path(v));
                    status = "R";
                }
                if let Some(v) = line.strip_prefix("rename to ") {
                    path = git_path(v);
                    status = "R";
                }
                binary |= line.starts_with("Binary files ") || line.starts_with("GIT binary patch");
            }
            if status == "R" && old_path.is_none() && old != path {
                old_path = Some(old);
            }
            let (additions, deletions) = patch_counts(&chunk.join("\n"));
            json!({"path":path,"old_path":old_path,"status":status,"staging":staging,
            "binary":binary,"additions":additions,"deletions":deletions})
        })
        .collect()
}

fn diff_fail(code: &str, message: &str) -> Error {
    typed(StatusCode::BAD_REQUEST, code, message)
}

async fn default_base_branch(root: &FsPath, current: Option<&str>) -> Result<Option<String>, Error> {
    if let Some(head) = git_read(
        root,
        &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"],
    )
    .await?
    {
        let head = head.trim();
        if !head.is_empty() && Some(head) != current {
            return Ok(Some(head.to_owned()));
        }
    }
    for name in ["main", "master", "trunk", "develop"] {
        if Some(name) == current {
            continue;
        }
        let refname = format!("refs/heads/{name}");
        if git_read(root, &["rev-parse", "--verify", "--quiet", &refname])
            .await?
            .is_some()
        {
            return Ok(Some(name.to_owned()));
        }
    }
    Ok(None)
}

async fn branch_diff(root: &FsPath, requested: &str, path: Option<&str>) -> Api {
    if git_read(root, &["rev-parse", "--show-toplevel"])
        .await?
        .is_none()
    {
        return Ok(Json(json!({"is_repo":false,"root_path":root,"baseline":"branch",
            "base":null,"bases":[],"files":[],"sections":{},"patch":"","additions":0,"deletions":0,
            "truncated":false})));
    }
    let head = git_read(root, &["rev-parse", "--short", "HEAD"])
        .await?
        .map(|s| s.trim().to_owned())
        .ok_or_else(|| diff_fail("workspace.diff.no_commits", "This repository has no commits yet"))?;
    let branch = git_read(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await?
        .map(|s| s.trim().to_owned());
    let listed = git_read(
        root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .await?
    .ok_or_else(|| diff_fail("workspace.diff.branches_unreadable", "Could not read Git branches"))?;
    let bases: Vec<String> = listed
        .lines()
        .map(str::trim)
        .filter(|name| {
            !name.is_empty() && !name.ends_with("/HEAD") && Some(*name) != branch.as_deref()
        })
        .map(str::to_owned)
        .collect();
    let base = if requested.is_empty() {
        default_base_branch(root, branch.as_deref())
            .await?
            .ok_or_else(|| {
                diff_fail(
                    "workspace.diff.base_unavailable",
                    "No base branch could be determined; choose one",
                )
            })?
    } else {
        requested.to_owned()
    };
    if base.starts_with('-') || !bases.iter().any(|name| name == &base) {
        return Err(diff_fail(
            "workspace.diff.base_unknown",
            "The base branch does not exist in this repository",
        ));
    }
    let merge_base = git_read(root, &["merge-base", &base, "HEAD"])
        .await?
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            diff_fail(
                "workspace.diff.no_merge_base",
                "The base branch shares no history with HEAD",
            )
        })?;
    let range = format!("{merge_base}...HEAD");
    let patch = git_read(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--find-renames",
            "--relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            &range,
            "--",
            path.unwrap_or("."),
        ],
    )
    .await?
    .ok_or_else(|| diff_fail("workspace.diff.read_failed", "Could not read Git diff"))?
    .trim()
    .to_owned();
    let files = patch_files(&patch, "artifact");
    let additions: usize = files
        .iter()
        .map(|f| f["additions"].as_u64().unwrap_or(0) as usize)
        .sum();
    let deletions: usize = files
        .iter()
        .map(|f| f["deletions"].as_u64().unwrap_or(0) as usize)
        .sum();
    Ok(Json(json!({"is_repo":true,"root_path":root,"baseline":"branch","base":base,
        "bases":bases,"merge_base":merge_base.get(..7).unwrap_or(&merge_base),
        "head_sha":head,"current_branch":branch,"detached_head":branch.is_none(),
        "detached_sha":if branch.is_none(){Some(&head)}else{None},
        "captured_at":chrono::Utc::now().to_rfc3339(),"dirty":false,"files":files,"sections":{},
        "selected_path":path,"selected_staging":null,"patch":patch,
        "additions":additions,"deletions":deletions,"truncated":patch.contains("/* truncated: file exceeds")})))
}

async fn diff(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<DiffQuery>,
) -> Api {
    let root = bound_root(&s, &id)?;
    let staging = q.staging.trim().to_lowercase();
    if !staging.is_empty() && !["staged", "unstaged", "untracked"].contains(&staging.as_str()) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "staging must be staged, unstaged, or untracked",
        ));
    }
    let raw_path = q.path.trim();
    let path = if raw_path.is_empty() {
        None
    } else {
        let input = FsPath::new(raw_path);
        let input = if input.is_absolute() {
            input
                .strip_prefix(&root)
                .map_err(|_| fail(StatusCode::BAD_REQUEST, "Path is outside this workspace"))?
        } else {
            input
        };
        if input.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "Path is outside this workspace",
            ));
        }
        if root.join(input).exists() {
            resolve(&root, &input.to_string_lossy())?;
        }
        Some(input.to_string_lossy().into_owned())
    };
    match q.baseline.trim() {
        "" | "worktree" => {}
        "branch" => return branch_diff(&root, q.base.trim(), path.as_deref()).await,
        _ => {
            return Err(diff_fail(
                "workspace.diff.baseline_invalid",
                "baseline must be worktree or branch",
            ));
        }
    }
    let is_repo = git_read(&root, &["rev-parse", "--show-toplevel"])
        .await?
        .is_some();
    let mut result = json!({"is_repo":is_repo,"root_path":root,"baseline":"worktree",
        "head_sha":null,"current_branch":null,"detached_head":false,"detached_sha":null,
        "captured_at":chrono::Utc::now().to_rfc3339(),"dirty":false,"files":[],"sections":{},
        "selected_path":path,"selected_staging":if staging.is_empty(){None}else{Some(&staging)},
        "patch":"","additions":0,"deletions":0});
    let mut all_files = Vec::new();
    let mut labeled = Vec::new();
    let mut total_add = 0;
    let mut total_del = 0;
    let mut truncated = false;
    if is_repo {
        let head = git_read(&root, &["rev-parse", "--short", "HEAD"])
            .await?
            .map(|s| s.trim().to_owned());
        let branch = git_read(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await?
            .map(|s| s.trim().to_owned());
        result["head_sha"] = json!(head);
        result["current_branch"] = json!(branch);
        result["detached_head"] = json!(branch.is_none() && head.is_some());
        if branch.is_none() {
            result["detached_sha"] = json!(head);
        }
        let status = git_read(
            &root,
            &["status", "--porcelain=v1", "-z", "-uall", "--", "."],
        )
        .await?
        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "Could not read Git status"))?;
        result["dirty"] = json!(!status.is_empty());
    }
    for section in ["staged", "unstaged", "untracked"] {
        let mut patch = String::new();
        let mut files = Vec::new();
        if is_repo && (staging.is_empty() || staging == section) {
            if section == "untracked" {
                let scope = path.as_deref().unwrap_or(".");
                let names = git_read(
                    &root,
                    &[
                        "ls-files",
                        "--others",
                        "--exclude-standard",
                        "-z",
                        "--",
                        scope,
                    ],
                )
                .await?
                .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "Could not read untracked files"))?;
                for name in names.split('\0').filter(|n| !n.is_empty()) {
                    let Ok(target) = resolve(&root, name) else {
                        continue;
                    };
                    if !target.is_file() {
                        continue;
                    }
                    let raw = read_limited(&target, MAX_PREVIEW)?;
                    let binary = raw[..raw.len().min(8192)].contains(&0);
                    truncated |= !binary && raw.len() > MAX_PREVIEW;
                    let body = if binary {
                        format!("Binary files /dev/null and b/{name} differ")
                    } else if raw.len() > MAX_PREVIEW {
                        format!(
                            "--- /dev/null\n+++ b/{name}\n@@ -0,0 +1 @@\n+/* truncated: file exceeds {MAX_PREVIEW} bytes */"
                        )
                    } else if raw.is_empty() {
                        String::new()
                    } else {
                        let text = String::from_utf8_lossy(&raw);
                        let count = text.split_inclusive('\n').count();
                        let mut body =
                            format!("--- /dev/null\n+++ b/{name}\n@@ -0,0 +1,{count} @@\n");
                        for line in text.split_inclusive('\n') {
                            body.push('+');
                            body.push_str(line);
                        }
                        if !text.ends_with('\n') {
                            body.push_str("\n\\ No newline at end of file\n");
                        }
                        body
                    };
                    let item_patch =
                        format!("diff --git a/{name} b/{name}\nnew file mode 100644\n{body}");
                    let a = if binary || raw.len() > MAX_PREVIEW {
                        0
                    } else {
                        String::from_utf8_lossy(&raw).split_inclusive('\n').count()
                    };
                    let d = 0;
                    files.push(json!({"path":name,"old_path":null,"status":"A","staging":section,"binary":binary,"additions":a,"deletions":d}));
                    if !patch.is_empty() {
                        patch.push('\n');
                    }
                    patch.push_str(&item_patch);
                    if patch.len() > MAX_DIFF_OUTPUT {
                        return Err(fail(
                            StatusCode::BAD_REQUEST,
                            "Diff exceeds the 8 MB preview limit; select a file",
                        ));
                    }
                }
            } else {
                let mut args = vec![
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--find-renames",
                    "--relative",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                ];
                if section == "staged" {
                    args.push("--cached");
                }
                args.extend(["--", path.as_deref().unwrap_or(".")]);
                patch = git_read(&root, &args)
                    .await?
                    .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "Could not read Git diff"))?
                    .trim()
                    .to_owned();
                files = patch_files(&patch, section);
            }
        }
        let a: usize = files
            .iter()
            .map(|f| f["additions"].as_u64().unwrap_or(0) as usize)
            .sum();
        let d: usize = files
            .iter()
            .map(|f| f["deletions"].as_u64().unwrap_or(0) as usize)
            .sum();
        total_add += a;
        total_del += d;
        all_files.extend(files);
        if !patch.is_empty() {
            labeled.push(format!("# {section}\n{patch}"));
        }
        result["sections"][section] = json!({"patch":patch,"additions":a,"deletions":d});
    }
    result["patch"] = if path.is_some() && !staging.is_empty() {
        result["sections"][&staging]["patch"].clone()
    } else {
        json!(labeled.join("\n\n"))
    };
    result["files"] = json!(all_files);
    result["additions"] = json!(total_add);
    result["deletions"] = json!(total_del);
    result["truncated"] = json!(truncated);
    Ok(Json(result))
}

pub(crate) async fn inspect_git_status(root: &FsPath) -> Result<Value, Error> {
    let mut status = json!({"is_repo":false,"root_path":root,"current_branch":null,
        "detached_head":false,"detached_sha":null,"branches":[],"dirty":false,
        "changed_file_count":0,"git_common_dir":null,"remotes":[],"upstream":null,
        "ahead":null,"behind":null,"head_sha":null,"head_subject":null,"read_only":false,"pull_requests_supported":true});
    if git_read(root, &["rev-parse", "--show-toplevel"])
        .await?
        .is_none()
    {
        return Ok(status);
    }
    status["is_repo"] = json!(true);
    let branch = git_read(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await?
        .map(|s| s.trim().to_owned());
    status["current_branch"] = json!(branch);
    let head = git_read(
        root,
        &["log", "--no-show-signature", "-1", "--format=%H%x00%s"],
    )
    .await?;
    if let Some(head) = head {
        let (sha, subject) = head
            .trim_end_matches('\n')
            .split_once('\0')
            .unwrap_or(("", ""));
        if !sha.is_empty() {
            status["head_sha"] = json!(sha);
            status["head_subject"] = json!(subject);
            if branch.is_none() {
                status["detached_head"] = json!(true);
                status["detached_sha"] = json!(
                    git_read(root, &["rev-parse", "--short", "HEAD"])
                        .await?
                        .map(|s| s.trim().to_owned())
                );
            }
        }
    }
    let listed = git_read(
        root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/heads",
        ],
    )
    .await?
    .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "Could not read local Git branches"))?;
    status["branches"] = json!(listed.lines().filter(|s| !s.is_empty()).collect::<Vec<_>>());
    let changed = git_read(
        root,
        &["status", "--porcelain=v1", "-z", "-uall", "--", "."],
    )
    .await?
    .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "Could not read Git status"))?;
    let mut records = changed.split('\0').filter(|s| !s.is_empty());
    let mut changed_count = 0;
    while let Some(record) = records.next() {
        changed_count += 1;
        if record
            .as_bytes()
            .iter()
            .take(2)
            .any(|b| matches!(*b, b'R' | b'C'))
        {
            records.next();
        }
    }
    status["dirty"] = json!(changed_count > 0);
    status["changed_file_count"] = json!(changed_count);
    status["git_common_dir"] = json!(
        git_read(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"]
        )
        .await?
        .map(|s| s.trim().to_owned())
    );
    let remotes = git_read(root, &["remote"]).await?.ok_or_else(|| {
        fail(
            StatusCode::BAD_REQUEST,
            "Could not read local Git remote names",
        )
    })?;
    status["remotes"] = json!(
        remotes
            .lines()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    );
    let upstream = git_read(
        root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .await?
    .map(|s| s.trim().to_owned());
    if upstream.is_some() {
        if let Some(counts) = git_read(
            root,
            &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
        )
        .await?
        {
            let parts = counts.split_whitespace().collect::<Vec<_>>();
            if parts.len() == 2 {
                status["behind"] = json!(parts[0].parse::<u64>().ok());
                status["ahead"] = json!(parts[1].parse::<u64>().ok());
            }
        }
    }
    status["upstream"] = json!(upstream);
    Ok(status)
}

fn metadata_error(_: rusqlite::Error) -> Error {
    fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not read workspace metadata",
    )
}
pub(crate) fn status_occupancy(
    s: &AppState,
    root: &FsPath,
    exclude: &str,
    result: &mut Value,
) -> Result<(), Error> {
    let workspaces = s.store.list("workspaces").map_err(metadata_error)?;
    let mut occupants = Vec::new();
    let mut branch_switch_blockers = Vec::new();
    for doc in s
        .store
        .list("conversation_threads")
        .map_err(metadata_error)?
    {
        let thread = &doc["thread"];
        let Some(id) = thread["thread_id"].as_str() else {
            continue;
        };
        if thread["state"]["status"] == "archived" {
            continue;
        }
        let Some(wid) = thread["workspace_id"].as_str() else {
            continue;
        };
        let Some(workspace) = workspaces.iter().find(|w| w["workspace_id"] == wid) else {
            continue;
        };
        let Some(raw) = workspace["root_path"].as_str() else {
            continue;
        };
        if canonical_workspace_root(raw).is_ok_and(|p| p == root) {
            if id != exclude {
                occupants.push(id.to_owned());
            }
            if doc["runtime_epoch"].as_str() == Some(s.chat.epoch())
                && thread["state"]["status"] == "running"
            {
                branch_switch_blockers.push(json!({
                    "thread_id": id,
                    "title": thread["title"].as_str().unwrap_or(id),
                }));
            }
        }
    }
    result["occupied"] = json!(!occupants.is_empty());
    result["occupant_count"] = json!(occupants.len());
    result["branch_switch_blocked"] = json!(!branch_switch_blockers.is_empty());
    result["branch_switch_blockers"] = json!(branch_switch_blockers);
    result["occupant_thread_ids"] = json!(occupants);
    Ok(())
}

async fn thread_git_status(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Api {
    let root = bound_root(&s, &id)?;
    let thread = s
        .store
        .get("conversation_threads", &id)
        .map_err(metadata_error)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Thread not found"))?;
    let wid = thread["thread"]["workspace_id"].as_str().unwrap_or("");
    let workspace = s
        .store
        .get("workspaces", wid)
        .map_err(metadata_error)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Workspace not found"))?;
    let mut result = inspect_git_status(&root).await?;
    result["thread_id"] = json!(id);
    result["workspace_id"] = json!(wid);
    result["mode"] = json!(workspace["settings"]["mode"].as_str().unwrap_or(""));
    result["configured_branch"] = workspace["settings"]["branch"].clone();
    result["workspace_kind"] = workspace["kind"].clone();
    status_occupancy(&s, &root, &id, &mut result)?;
    Ok(Json(result))
}

pub(crate) async fn project_git_status(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Api {
    let root = project_bound_root(&s, &id)?;
    let mut result = inspect_git_status(&root).await?;
    result["project_id"] = json!(id);
    status_occupancy(&s, &root, "", &mut result)?;
    Ok(Json(result))
}

pub(crate) fn project_bound_root(s: &AppState, id: &str) -> Result<PathBuf, Error> {
    if s.store
        .get("projects", id)
        .map_err(metadata_error)?
        .is_none()
    {
        return Err(fail(StatusCode::NOT_FOUND, "Project not found"));
    }
    let mut workspaces = s.store.list("workspaces").map_err(metadata_error)?;
    workspaces.retain(|w| {
        w["project_id"] == id
            && !matches!(
                w["settings"]["mode"].as_str(),
                Some("new_worktree" | "existing_worktree")
            )
    });
    workspaces.sort_by_key(|w| {
        (
            w["kind"] != "local",
            w["created_at"].as_str().unwrap_or("").to_owned(),
        )
    });
    let workspace = workspaces
        .first()
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Project has no bound workspace"))?;
    let raw = workspace["root_path"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Workspace root is unavailable"))?;
    canonical_workspace_root(raw)
}
