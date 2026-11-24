//! Message-owned visual replies: `muteki-visualize` fences and file markers
//! in assistant text become immutable published snapshots.
//!
//! A snapshot is written once per (message, reference), including a durable
//! failure receipt, so later edits or deletion of a source file never change
//! a reply that was already shown.

use regex::Regex;
use serde_json::{Value, json};
use base64::Engine;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

pub const MAX_SOURCE_BYTES: usize = 1_000_000;
pub const MAX_IMAGE_BYTES: usize = 5_000_000;
pub const MAX_DOCUMENT_BYTES: usize = 10_000_000;

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "ico"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualizationError {
    pub code: String,
    pub message: String,
}

impl VisualizationError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_owned(), message: message.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub path: String,
    pub html: Option<String>,
    pub title: String,
}

/// A spaced marker must end its line, so a sentence that merely mentions the
/// syntax mid-line stays text; it may follow prose on that line.
fn marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)(?:visualize(\{[^\n]*?\})|visualize(\{[^\n]*?\})|(?:^|[^\w\n-])(?:muteki-visualize|visualize)[ \t]+(\{[^\n]*\})[ \t]*$)").unwrap()
    })
}

fn opener() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[ \t]*(`{3,}|~{3,})muteki-visualize[ \t]*\r?$").unwrap())
}

/// Fenced blocks: an opening line of three or more backticks or tildes with
/// the `muteki-visualize` info string, closed by the same fence at the start
/// of a later line.
fn fences(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut found = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some(open) = opener().captures(lines[index]) else {
            index += 1;
            continue;
        };
        let fence = open[1].to_owned();
        let close = (index + 1..lines.len()).find(|&i| {
            let line = lines[i].strip_suffix('\r').unwrap_or(lines[i]);
            line.strip_prefix(fence.as_str()).is_some_and(|rest| rest.chars().all(|c| c == ' ' || c == '\t'))
        });
        match close {
            Some(end) => {
                let body = lines[index + 1..end].join("\n");
                let body = body.strip_suffix('\r').unwrap_or(&body);
                found.push(body.trim().to_owned());
                index = end + 1;
            }
            None => index += 1,
        }
    }
    found
}

/// Every visual reference in an assistant message, fences first.
pub fn references(text: &str) -> Vec<Reference> {
    let mut result: Vec<Reference> = fences(text)
        .into_iter()
        .enumerate()
        .map(|(index, html)| Reference { path: format!("inline:{index}"), html: Some(html), title: String::new() })
        .collect();
    for captures in marker().captures_iter(text) {
        let raw = captures.get(1).or_else(|| captures.get(2)).or_else(||captures.get(3)).map(|m| m.as_str()).unwrap_or("");
        let Ok(Value::Object(value)) = serde_json::from_str::<Value>(raw) else { continue };
        if let Some(path) = value.get("path").and_then(Value::as_str) {
            let title = match value.get("title") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            };
            result.push(Reference { path: path.to_owned(), html: None, title });
        }
    }
    result
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>, VisualizationError> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| unavailable(path, &e))?;
    let mut content = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut content).map_err(|e| unavailable(path, &e))?;
    if content.len() > limit {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        return Err(VisualizationError::new("visualization.too_large", format!("{name} 超出 {limit} 字节，未截断或发布")));
    }
    Ok(content)
}

fn unavailable(path: &Path, error: &std::io::Error) -> VisualizationError {
    VisualizationError::new("visualization.source_unavailable", format!("{error}: '{}'", path.display()))
}

fn image_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let ext = r"\.(?:png|jpe?g|gif|webp|avif|bmp|ico)";
        let quoted = |q: &str| {
            let class = r#"[^\r\n"'`]"#;
            format!(r#"{q}((?:/[^/\r\n"'`]{class}*?|\./{class}+?|\.\./{class}+?|file:///{class}+?){ext}){q}"#)
        };
        let url = r#"url\(\s*((?:/[^/\s"'`()][^\s"'`()]*?|\./[^\s"'`()]+?|\.\./[^\s"'`()]+?)\.(?:png|jpe?g|gif|webp|avif|bmp|ico))\s*\)"#;
        Regex::new(&format!("(?i){}|{}|{}|{url}", quoted("\""), quoted("'"), quoted("`"))).unwrap()
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn image_mime(content: &[u8]) -> Option<&'static str> {
    crate::image_validation::mime(content).ok()
}

fn inline_images(html: &str, root: &Path, base: &Path) -> Result<String, VisualizationError> {
    use base64::Engine;
    let root = std::fs::canonicalize(root).map_err(|e| unavailable(root, &e))?;
    let mut images: std::collections::HashMap<String, String> = Default::default();
    let mut size = html.len();
    let mut out = String::with_capacity(html.len());
    let mut last = 0;
    for captures in image_pattern().captures_iter(html) {
        let Some(reference) = (1..=4).find_map(|i| captures.get(i)) else { continue };
        let text = reference.as_str();
        if !images.contains_key(text) {
            if text.contains('?') || text.contains('#') {
                return Err(VisualizationError::new("visualization.image_path", "本地图片必须使用文件路径"));
            }
            let local = if text.len() >= 7 && text[..7].eq_ignore_ascii_case("file://") { &text[7..] } else { text };
            let candidate = base.join(percent_decode(local));
            let target = std::fs::canonicalize(&candidate).map_err(|e| unavailable(&candidate, &e))?;
            if !target.starts_with(&root) {
                return Err(VisualizationError::new("visualization.image_scope", "图片必须位于当前对话的可视化目录内"));
            }
            let content = read_limited(&target, MAX_IMAGE_BYTES)?;
            let name = target.file_name().unwrap_or_default().to_string_lossy();
            let mime = image_mime(&content)
                .ok_or_else(|| VisualizationError::new("visualization.invalid_image", format!("{name} 不是可验证的图片")))?;
            images.insert(text.to_owned(), format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&content)));
        }
        let encoded = &images[text];
        size = size + encoded.len() - text.len();
        if size > MAX_DOCUMENT_BYTES {
            return Err(VisualizationError::new("visualization.too_large", "内嵌图片后图形超过 10 MB，未截断或发布"));
        }
        out.push_str(&html[last..reference.start()]);
        out.push_str(encoded);
        last = reference.end();
    }
    out.push_str(&html[last..]);
    Ok(out)
}

fn html_title(html: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?is)<title(?:\s[^>]*)?>(.*?)</title\s*>").unwrap());
    let Some(raw) = re.captures(html).map(|c| c[1].to_owned()) else { return String::new() };
    raw.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

/// Build the publishable document for a workspace source file.
fn document(workspace: &Path, identity: &str, message_id: &str, reference: &Reference, assets: &dyn Fn() -> Value) -> Result<Value, VisualizationError> {
    let (html, base) = match &reference.html {
        Some(html) => (html.clone(), workspace.to_path_buf()),
        None => {
            let raw = reference.path.strip_prefix("~/").map_or_else(
                || PathBuf::from(&reference.path),
                |rest| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
            );
            let path = std::fs::canonicalize(&raw).map_err(|e| unavailable(&raw, &e))?;
            let scope = std::fs::canonicalize(workspace).map_err(|e| unavailable(workspace, &e))?;
            if !path.starts_with(&scope) {
                return Err(VisualizationError::new("visualization.file_scope", "图形文件必须位于当前对话的可视化目录内"));
            }
            let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default();
            if ext == "html" {
                let bytes = read_limited(&path, MAX_SOURCE_BYTES)?;
                let html = String::from_utf8(bytes).map_err(|_| {
                    VisualizationError::new("visualization.source_unavailable", "'utf-8' codec can't decode the visualization source")
                })?;
                (html, path.parent().map(Path::to_path_buf).unwrap_or_default())
            } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
                let bytes = read_limited(&path, MAX_IMAGE_BYTES)?;
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let mime = image_mime(&bytes)
                    .ok_or_else(|| VisualizationError::new("visualization.invalid_image", format!("{name} 不是可验证的图片")))?;
                let safe_name = name.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;");
                let html = format!(
                    "<!doctype html><html><head><meta charset=\"utf-8\"><title>{safe_name}</title><style>body{{margin:0;display:flex;align-items:center;justify-content:center;min-height:100vh;background:#fff}}img{{max-width:100%;max-height:100vh;object-fit:contain}}</style></head><body><img src=\"data:{mime};base64,{}\" alt=\"{safe_name}\"></body></html>",
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                );
                return finish_document(html, workspace, workspace, identity, message_id, reference, assets);
            } else {
                return Err(VisualizationError::new("visualization.file_type", "可视化文件必须是 .html 或常见图片格式"));
            }
        }
    };
    if html.len() > MAX_SOURCE_BYTES {
        return Err(VisualizationError::new("visualization.too_large", "图形源码超过 1 MB，未截断或发布"));
    }
    finish_document(html, workspace, &base, identity, message_id, reference, assets)
}

fn finish_document(html: String, workspace: &Path, base: &Path, identity: &str, message_id: &str, reference: &Reference, assets: &dyn Fn() -> Value) -> Result<Value, VisualizationError> {
    let html = inline_images(&html, workspace, base)?;
    let title = if reference.title.is_empty() { html_title(&html) } else { reference.title.clone() };
    let document = json!({
        "html": html, "assets": assets(),
        "title": if title.is_empty() { "交互图".to_owned() } else { title },
        "documentId": identity, "messageId": message_id,
    });
    if serde_json::to_vec(&document).map(|v| v.len()).unwrap_or(usize::MAX) > MAX_DOCUMENT_BYTES * 2 {
        return Err(VisualizationError::new("visualization.too_large", "图形快照超过存储范围，未截断或发布"));
    }
    Ok(document)
}

/// Publish once per message/reference. Concurrent publishers race on a hard
/// link, so the first complete snapshot wins and is never replaced.
pub fn snapshot(root: &Path, workspace: &Path, message_id: &str, reference_index: usize, reference_count: usize, reference: &Reference, assets: &dyn Fn() -> Value) -> std::io::Result<Value> {
    let message_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(message_id);
    let path_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&reference.path);
    let identity = if reference_count <= 1 && reference_index == 0 {
        format!("{message_key}-0")
    } else {
        let readable = format!("{message_key}-{reference_index}-{path_key}");
        if readable.len() + ".json".len() <= 255 {
            readable
        } else {
            super::sha_hex(format!("{message_id}\0{reference_index}\0{}", reference.path).as_bytes())[..32].to_owned()
        }
    };
    let target = root.join(format!("{identity}.json"));
    let stored = |target: &Path| -> std::io::Result<Value> {
        let bytes = std::fs::read(target)?;
        let value:Value=serde_json::from_slice(&bytes).map_err(|error|std::io::Error::new(std::io::ErrorKind::InvalidData,error))?;
        if value["messageId"]!=message_id||value["referencePath"]!=reference.path{return Err(std::io::Error::new(std::io::ErrorKind::InvalidData,"visualization snapshot reference changed"));}Ok(value)
    };
    if target.is_file() {
        return stored(&target);
    }
    let mut value = match document(workspace, &identity, message_id, reference, assets) {
        Ok(document) => document,
        Err(error) => json!({"error": {"code": error.code, "message": error.message}}),
    };
    value["messageId"]=json!(message_id);value["referencePath"]=json!(reference.path);
    std::fs::create_dir_all(root)?;
    let temporary = tempfile::Builder::new().prefix(".publish-").tempfile_in(root)?;
    std::fs::write(temporary.path(), serde_json::to_vec(&value).unwrap_or_default())?;
    match std::fs::hard_link(temporary.path(), &target) {
        Ok(()) => Ok(value),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => stored(&target),
        Err(error) => Err(error),
    }
}

pub fn instructions(workspace: &Path, skill: Option<&Path>) -> String {
    let theme = match skill {
        Some(path) => format!("主题与交互细节读取 visualize Skill：{}。", path.display()),
        None => "当前引擎没有启用 visualize Skill，没有可读取的官方主题规范；直接输出自包含的 HTML，不要查找该 Skill。".to_owned(),
    };
    format!(
        "Muteki 聊天展示接口：需要结构图或交互图时，直接输出 muteki-visualize 围栏，块内放完整 HTML 片段；宿主保存并内联渲染，无需文件写权限或控制工具。不要使用网络 API。{theme}文件交付与本地图片目录为 {}；文件交付时单独一行输出 muteki-visualize {{\"path\":\"绝对路径\"}}，指向目录内的 .html 或图片文件（png、jpg、gif、webp、avif、bmp、ico）。",
        workspace.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_and_markers_are_found_in_message_order_of_kind() {
        let text = "intro\n```muteki-visualize\n<div>a</div>\n```\nmiddle\n````muteki-visualize\n<p>```</p>\n````\nmuteki-visualize {\"path\":\"/tmp/x.html\",\"title\":\"T\"}\nsee visualize{\"path\":\"/tmp/y.html\"} inline\nmuteki-visualize {broken";
        let found = references(text);
        assert_eq!(found.len(), 4);
        assert_eq!(found[0].html.as_deref(), Some("<div>a</div>"));
        assert_eq!(found[0].path, "inline:0");
        assert_eq!(found[1].html.as_deref(), Some("<p>```</p>"), "a shorter fence inside does not close a longer one");
        assert_eq!(found[1].path, "inline:1");
        assert_eq!((found[2].path.as_str(), found[2].title.as_str()), ("/tmp/x.html", "T"));
        assert_eq!(found[3].path, "/tmp/y.html");
    }

    #[test]
    fn an_unclosed_fence_is_ordinary_text() {
        assert!(references("```muteki-visualize\n<div>never closed</div>").is_empty());
    }

    #[test]
    fn a_snapshot_is_published_once_and_survives_source_changes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("chart.html");
        std::fs::write(&file, "<title>Sales &amp; Cost</title><img src=\"./dot.png\">").unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let png = png.into_inner();
        std::fs::write(workspace.join("dot.png"), png).unwrap();
        let reference = Reference { path: file.display().to_string(), html: None, title: String::new() };
        let assets = || json!({"visualize.css": "x"});
        let first = snapshot(&dir.path().join("snap"), &workspace, "m1", 0, 1, &reference, &assets).unwrap();
        assert_eq!(first["title"], "Sales & Cost");
        assert!(first["html"].as_str().unwrap().contains("data:image/png;base64,"));
        assert_eq!(first["assets"]["visualize.css"], "x");
        std::fs::remove_file(&file).unwrap();
        let again = snapshot(&dir.path().join("snap"), &workspace, "m1", 0, 1, &reference, &assets).unwrap();
        assert_eq!(again, first);
    }

    #[test]
    fn files_outside_the_workspace_get_a_durable_failure_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside = dir.path().join("outside.html");
        std::fs::write(&outside, "<p>x</p>").unwrap();
        let reference = Reference { path: outside.display().to_string(), html: None, title: String::new() };
        let result = snapshot(&dir.path().join("snap"), &workspace, "m1", 0, 1, &reference, &|| json!({})).unwrap();
        assert_eq!(result["error"]["code"], "visualization.file_scope");
        let inline = Reference { path: "inline:x".into(), html: Some("<img src='/etc/hosts.png'>".into()), title: String::new() };
        let failed = snapshot(&dir.path().join("snap"), &workspace, "m2", 0, 1, &inline, &|| json!({})).unwrap();
        assert_eq!(failed["error"]["code"], "visualization.source_unavailable");
    }

}
