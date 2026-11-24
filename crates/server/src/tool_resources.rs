use agent::{AgentEvent, ItemContent, ItemStatus, ThreadItem};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;

use crate::AppState;

#[derive(Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Reference {
    Workspace {
        name: String,
        path: String,
        line: Option<u64>,
    },
    ImagePath {
        name: String,
        path: String,
    },
    Artifact {
        name: String,
        artifact_id: String,
        media_type: String,
    },
    Recorded {
        name: String,
        media_type: String,
        turn_id: String,
        stream_seq: u64,
        index: usize,
    },
}

struct File {
    name: String,
    media_type: String,
    data: String,
}

#[derive(Default)]
pub(crate) struct ResultFiles {
    refs: Vec<Reference>,
    files: Vec<File>,
}

pub(crate) struct SavedFiles {
    pub resources: Value,
    pub artifacts: Vec<Value>,
    pub invalid: Vec<base64::DecodeError>,
}

impl SavedFiles {
    pub(crate) fn warning(&self, turn_id: &Value, tool_call_id: &str) -> Option<Value> {
        if self.invalid.is_empty() {
            return None;
        }
        let details = self.invalid.iter().map(ToString::to_string).collect::<Vec<_>>();
        Some(json!({
            "kind": "warning",
            "code": "runtime.tool_file_invalid_base64",
            "turn_id": turn_id,
            "tool_call_id": tool_call_id,
            "message": format!("无法预览 {} 个工具文件：工具返回的 Base64 数据无效。", details.len()),
            "details": details,
        }))
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn file_path(uri: &str) -> Option<String> {
    let url = reqwest::Url::parse(uri).ok()?;
    url.to_file_path().ok()?.into_os_string().into_string().ok()
}

fn resource_name(media_type: &str, index: usize) -> String {
    let ext = match media_type {
        "image/jpeg" => "jpg",
        "image/svg+xml" => "svg",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        mime => mime.split_once('/').map_or("bin", |(_, ext)| ext),
    };
    format!(
        "{}.{}",
        if index == 0 {
            "output".into()
        } else {
            format!("output-{}", index + 1)
        },
        ext
    )
}

fn read_tool(name: &str, kind: &str) -> bool {
    let name = name.rsplit(['/', '.']).next().unwrap_or(name);
    let name = name.rsplit("__").next().unwrap_or(name);
    kind == "read"
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "read"
                | "readfile"
                | "read_file"
                | "read_media_file"
                | "read_image"
                | "view_file"
                | "client_view_file"
                | "view_image"
                | "image_read"
                | "viewimage"
        )
}

fn positive_line(input: &Value) -> Option<u64> {
    ["line", "start_line", "StartLine", "startLine", "offset"]
        .into_iter()
        .find_map(|key| input[key].as_u64().filter(|line| *line > 0))
}

fn read_path(input: &Value) -> Option<&str> {
    ["path", "file_path", "filePath", "absolute_path", "Path"]
        .into_iter()
        .find_map(|key| input[key].as_str().filter(|path| !path.is_empty()))
}

fn capture_path(path: &str) -> bool {
    let path = Path::new(path);
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "browser-captures")
        && path.extension().is_some_and(|ext| ext == "png")
        && path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| uuid::Uuid::parse_str(name).is_ok())
}

impl ResultFiles {
    fn push(&mut self, reference: Reference) {
        if !self.refs.contains(&reference) {
            self.refs.push(reference);
        }
    }

    fn path(&mut self, path: &str, line: Option<u64>) {
        let name = file_name(path).to_owned();
        self.push(if capture_path(path) {
            Reference::ImagePath {
                name,
                path: path.into(),
            }
        } else {
            Reference::Workspace {
                name,
                path: path.into(),
                line,
            }
        });
    }

    fn file(&mut self, media_type: &str, data: &str, name: Option<&str>) {
        if data.is_empty() || self.files.iter().any(|file| file.data == data) {
            return;
        }
        self.files.push(File {
            name: name
                .map(str::to_owned)
                .unwrap_or_else(|| resource_name(media_type, self.files.len())),
            media_type: media_type.into(),
            data: data.into(),
        });
    }

    fn inputs(&mut self, name: &str, kind: &str, input: &Value, locations: &[Value]) {
        if read_tool(name, kind) {
            for location in locations {
                if let Some(path) = location["path"].as_str() {
                    self.path(
                        path,
                        location["line"].as_u64().or_else(|| positive_line(input)),
                    );
                }
            }
            if self.refs.is_empty()
                && let Some(path) = read_path(input)
            {
                self.path(path, positive_line(input));
            }
        }
    }

    fn image_path(&mut self, path: &str) {
        self.push(Reference::ImagePath {
            name: file_name(path).to_owned(),
            path: path.into(),
        });
    }

    fn data_url(&mut self, url: &str) -> bool {
        let Some((mime, data)) = url
            .strip_prefix("data:")
            .and_then(|url| url.split_once(";base64,"))
        else {
            return false;
        };
        self.file(mime, data, None);
        true
    }

    fn output(&mut self, value: &mut Value) -> bool {
        if let Some(blocks) = value.as_array_mut() {
            return blocks
                .iter_mut()
                .fold(false, |changed, block| self.output(block) || changed);
        }
        let mut changed = match value["type"].as_str() {
            Some("image") => {
                let source = value.get("source").unwrap_or(value);
                let mime = source["mimeType"]
                    .as_str()
                    .or(source["mediaType"].as_str())
                    .or(source["media_type"].as_str())
                    .or(source["mime_type"].as_str());
                if let (Some(mime), Some(data)) = (mime, source["data"].as_str()) {
                    self.file(mime, data, None);
                    *value = json!({"type":"image","mime_type":mime});
                    true
                } else {
                    if let Some(path) = value["path"].as_str()
                        && value["artifact_id"]
                            .as_str()
                            .is_some_and(|id| id.starts_with("cimg-"))
                    {
                        self.image_path(path);
                    }
                    false
                }
            }
            Some("inputImage" | "input_image") => {
                let url = value["imageUrl"].as_str().or(value["image_url"].as_str());
                if url.is_some_and(|url| self.data_url(url)) {
                    *value = json!({"type":"image"});
                    true
                } else {
                    false
                }
            }
            Some("file") => {
                let url = value["url"].as_str().or(value["uri"].as_str());
                if url.is_some_and(|url| self.data_url(url)) {
                    *value = json!({"type":"file"});
                    true
                } else {
                    if let Some(path) = url.and_then(file_path) {
                        self.path(&path, None);
                    }
                    false
                }
            }
            Some("resource_link") => {
                if let Some(path) = value["uri"].as_str().and_then(file_path) {
                    self.path(&path, None);
                }
                false
            }
            Some("resource") => {
                let resource = &value["resource"];
                let path = resource["uri"].as_str().and_then(file_path);
                let name = resource["name"].as_str().or(path.as_deref().map(file_name));
                let media = resource["mimeType"]
                    .as_str()
                    .unwrap_or("application/octet-stream");
                if let Some(blob) = resource["blob"].as_str() {
                    self.file(media, blob, name);
                    value["resource"].as_object_mut().unwrap().remove("blob");
                    true
                } else if let Some(text) = resource["text"].as_str() {
                    self.file(
                        resource["mimeType"].as_str().unwrap_or("text/plain"),
                        &STANDARD.encode(text),
                        name,
                    );
                    false
                } else {
                    false
                }
            }
            Some("text") => {
                if let Some(mut receipt) = value["text"]
                    .as_str()
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    && receipt["invocation_id"].is_string()
                    && receipt["result"].is_object()
                    && self.output(&mut receipt)
                {
                    value["text"] = json!(output_text(&receipt));
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if let Some(path) = value["path"].as_str()
            && capture_path(path)
            && value["capture_id"].as_str()
                == Path::new(path).file_stem().and_then(|name| name.to_str())
        {
            self.path(path, None);
        }
        for key in ["content", "result"] {
            if let Some(inner) = value.get_mut(key) {
                if let Some(text) = inner.as_str() {
                    if let Some(clean) = self.text_output(text) {
                        *inner = json!(clean);
                        changed = true;
                    }
                } else {
                    changed |= self.output(inner);
                }
            }
        }
        changed
    }

    fn text_output(&mut self, output: &str) -> Option<String> {
        if let Ok(mut value) = serde_json::from_str::<Value>(output) {
            return self.output(&mut value).then(|| output_text(&value));
        }
        if output.starts_with(r#"{"type":"image","source":{"type":"base64","#) {
            let notice = output
                .split_once("\n\n[... ")
                .and_then(|(_, rest)| rest.lines().next())
                .and_then(|line| line.strip_suffix(']'));
            return Some(match notice {
                Some(notice) => format!("Droid 返回的图片内容已截断（{notice}）"),
                None => "无法预览 Droid 返回的图片：图片数据不完整。".into(),
            });
        }
        None
    }

    fn name_files(&mut self) {
        if self.files.len() == 1
            && let Some(Reference::Workspace { name, .. } | Reference::ImagePath { name, .. }) =
                self.refs.first()
        {
            let file = &mut self.files[0];
            file.name = match file.media_type.as_str() {
                "image/jpeg"
                    if Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("png")) =>
                {
                    Path::new(name)
                        .with_extension("jpg")
                        .to_string_lossy()
                        .into_owned()
                }
                _ => name.clone(),
            };
        }
    }

    pub(crate) fn save(self, s: &AppState) -> Result<SavedFiles, String> {
        let mut refs = self.refs;
        let mut artifacts = Vec::new();
        let mut invalid = Vec::new();
        for file in self.files {
            let bytes = match file.bytes() {
                Ok(bytes) => bytes,
                Err(error) => {
                    invalid.push(error);
                    continue;
                }
            };
            let meta = crate::files::store_agent_artifact(
                s,
                &file.name,
                "conversation.tool_file",
                &file.media_type,
                &bytes,
            )?;
            refs.insert(
                artifacts.len(),
                Reference::Artifact {
                    name: file.name,
                    media_type: file.media_type,
                    artifact_id: meta["artifact_id"].as_str().unwrap().into(),
                },
            );
            artifacts.push(meta);
        }
        Ok(SavedFiles { resources: json!(refs), artifacts, invalid })
    }

    fn recorded(&self, turn: &str, seq: u64) -> Value {
        let mut refs = self.refs.clone();
        refs.splice(
            ..0,
            self.files
                .iter()
                .enumerate()
                .map(|(index, file)| Reference::Recorded {
                    turn_id: turn.into(),
                    stream_seq: seq,
                    index,
                    name: file.name.clone(),
                    media_type: file.media_type.clone(),
                }),
        );
        json!(refs)
    }
}

impl File {
    fn bytes(&self) -> Result<Vec<u8>, base64::DecodeError> {
        STANDARD.decode(&self.data)
    }
}

fn output_text(value: &Value) -> String {
    if matches!(value["type"].as_str(), Some("image" | "file")) {
        return String::new();
    }
    if let Some(blocks) = value.as_array() {
        return blocks
            .iter()
            .filter_map(|block| match block["type"].as_str() {
                Some("image" | "file") => None,
                Some("text") => block["text"].as_str().map(str::to_owned),
                _ => Some(block.to_string()),
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    value.to_string()
}

pub(crate) fn native_item(event: &mut AgentEvent) -> Option<&mut ThreadItem> {
    match event {
        AgentEvent::Origin { event, .. } => native_item(event),
        AgentEvent::ItemCompleted(item) | AgentEvent::ItemUpdated(item) => Some(item),
        _ => None,
    }
}

pub(crate) fn native(event: &mut AgentEvent) -> ResultFiles {
    let mut result = ResultFiles::default();
    let Some(item) = native_item(event) else {
        return result;
    };
    match &mut item.content {
        ItemContent::ToolCall {
            name,
            presentation,
            input,
            output,
            status,
            image_reads,
        } if *status != ItemStatus::InProgress => {
            if *status == ItemStatus::Completed {
                if let Some(meta) = presentation.as_ref() {
                    result.inputs(name, &meta.kind, input, &meta.locations);
                } else {
                    result.inputs(name, "", input, &[]);
                }
            }
            if let Some(text) = output {
                if let Some(clean) = result.text_output(text) {
                    *text = clean;
                }
            }
            for image in std::mem::take(image_reads) {
                result.file(&image.media_type, &image.data_base64, None);
            }
        }
        ItemContent::ImageRead { path, image } => {
            result.path(path, None);
            if let Some(image) = image.take() {
                result.file(&image.media_type, &image.data_base64, None);
            }
        }
        _ => {}
    }
    result.name_files();
    result
}

pub(crate) fn tool_payload(payload: &mut Value) -> ResultFiles {
    let mut result = ResultFiles::default();
    if payload["status"] == "completed" || payload["status"].is_null() {
        result.inputs(
            payload["name"].as_str().unwrap_or_default(),
            payload["kind"].as_str().unwrap_or_default(),
            &payload["input"],
            &[],
        );
    }
    if let Some(output) = payload.get_mut("output") {
        if let Some(text) = output.as_str() {
            if let Some(clean) = result.text_output(text) {
                *output = json!(clean);
            }
        } else {
            result.output(output);
        }
    }
    result.name_files();
    result
}

pub(crate) fn append_artifacts(doc: &mut Value, turn: &Value, artifacts: Vec<Value>) {
    for mut meta in artifacts {
        meta["turn_id"] = turn.clone();
        if !doc["artifacts"].is_array() {
            doc["artifacts"] = json!([]);
        }
        doc["artifacts"].as_array_mut().unwrap().push(meta);
    }
}

pub(crate) fn legacy_projection(event: &mut Value, turn: &str, seq: u64) {
    if event["event_type"] == "core.tool.completed" && event["payload"]["resources"].is_null() {
        let payload = &mut event["payload"];
        let result = tool_payload(payload);
        payload["resources"] = result.recorded(turn, seq);
    }
}

pub(crate) fn native_tool_payload(
    event: &mut AgentEvent,
    turn: &str,
    resources: Value,
) -> Option<Value> {
    let mut agent_id = None;
    let mut inner: &AgentEvent = event;
    while let AgentEvent::Origin {
        agent_id: owner,
        event,
        ..
    } = inner
    {
        agent_id = agent_id.or(owner.clone());
        inner = event;
    }
    let item = native_item(event)?;
    let (name, kind, input, output, status) = match &item.content {
        ItemContent::ToolCall {
            name,
            presentation,
            input,
            output,
            status,
            ..
        } => {
            let kind = presentation
                .as_ref()
                .map_or("other", |meta| meta.kind.as_str());
            let status = match status {
                ItemStatus::InProgress => return None,
                ItemStatus::Completed => "completed",
                ItemStatus::Failed => "failed",
                ItemStatus::Declined => "declined",
                ItemStatus::Interrupted => "cancelled",
            };
            (name.as_str(), kind, input.clone(), json!(output), status)
        }
        ItemContent::ImageRead { path, .. } => (
            "image_read",
            "read",
            json!({"path":path}),
            Value::Null,
            "completed",
        ),
        _ => return None,
    };
    Some(
        json!({"tool_call_id":item.id,"name":name,"kind":kind,"input":input,"output":output,"status":status,
        "agent_id": agent_id, "parent_tool_use_id":item.parent_item_id,"resources":resources,"turn_id":turn}),
    )
}

pub(crate) fn legacy_native(event: &Value, turn: &str, seq: u64, mirror: bool) -> Option<Value> {
    let source = &event["payload"];
    if !source["resources"].is_null() {
        return None;
    }
    let mut native_event: AgentEvent = serde_json::from_value(source["event"].clone()).ok()?;
    let content = &native_item(&mut native_event)?.content;
    let has_image = match content {
        ItemContent::ToolCall { image_reads, .. } => !image_reads.is_empty(),
        ItemContent::ImageRead { image, .. } => image.is_some(),
        _ => false,
    };
    if !mirror && !has_image {
        return None;
    }
    let result = native(&mut native_event);
    if result.files.is_empty() && result.refs.is_empty() {
        return None;
    }
    native_tool_payload(&mut native_event, turn, result.recorded(turn, seq))
}

pub(crate) fn referenced_paths(event: &Value) -> Vec<String> {
    let source = &event["payload"];
    if !source["resources"].is_null() {
        return source["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|reference| {
                matches!(reference["kind"].as_str(), Some("workspace" | "image_path"))
                    .then(|| reference["path"].as_str())
                    .flatten()
                    .map(str::to_owned)
            })
            .collect();
    }
    let result = match event["event_type"].as_str() {
        Some("core.tool.completed") => tool_payload(&mut source.clone()),
        Some("core.agent.event") => {
            let Ok(mut native_event) = serde_json::from_value(source["event"].clone()) else {
                return Vec::new();
            };
            native(&mut native_event)
        }
        _ => return Vec::new(),
    };
    result
        .refs
        .iter()
        .filter_map(|reference| match reference {
            Reference::Workspace { path, .. } | Reference::ImagePath { path, .. } => {
                Some(path.clone())
            }
            _ => None,
        })
        .collect()
}

pub(crate) fn recorded(event: &Value, index: usize) -> Result<(Value, Vec<u8>), String> {
    let source = &event["payload"];
    let result = match event["event_type"].as_str() {
        Some("core.tool.completed") => tool_payload(&mut source.clone()),
        Some("core.agent.event") => {
            let mut event = serde_json::from_value(source["event"].clone())
                .map_err(|error| error.to_string())?;
            native(&mut event)
        }
        _ => return Err("Recorded file not found".into()),
    };
    let file = result.files.get(index).ok_or("Recorded file not found")?;
    Ok((
        json!({"name":file.name,"media_type":file.media_type}),
        file.bytes().map_err(|error| error.to_string())?,
    ))
}
