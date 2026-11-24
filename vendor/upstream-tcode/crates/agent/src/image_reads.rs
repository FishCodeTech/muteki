use serde_json::Value;

use crate::Attachment;

pub fn tool_result_images(name: &str, content: &Value) -> Vec<Attachment> {
    if is_read_tool(name) {
        content_images(content)
    } else {
        Vec::new()
    }
}
// Tool results are model inputs, but generated images are not read operations.
// Only recognize explicit read/capture tools; an image block alone is insufficient.
pub(crate) fn is_read_tool(name: &str) -> bool {
    let name = name.rsplit('/').next().unwrap_or(name);
    let name = name.rsplit("__").next().unwrap_or(name);
    let name = name.rsplit('.').next().unwrap_or(name);
    matches!(
        name.to_ascii_lowercase().as_str(),
        "read"
            | "read_file"
            | "read_image"
            | "view_image"
            | "image_view"
            | "viewimage"
            | "screenshot"
            | "take_screenshot"
            | "capture_screenshot"
            | "browser_take_screenshot"
            | "preview_screenshot"
            | "read_media_file"
            | "observe_ui"
            | "inspect_ui"
            | "expand_ui"
            | "search_ui"
            | "act_ui"
            | "wait_for"
    )
}

pub(crate) fn content_images(content: &Value) -> Vec<Attachment> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            match block.get("type").and_then(Value::as_str) {
                Some("inputImage") => return data_url(block.get("imageUrl")?.as_str()?),
                Some("input_image") => return data_url(block.get("image_url")?.as_str()?),
                Some("image") => {}
                _ => return None,
            }
            let source = block.get("source").unwrap_or(block);
            let media_type = source
                .get("mimeType")
                .or_else(|| source.get("media_type"))?
                .as_str()?;
            let data = source.get("data")?.as_str()?;
            if !media_type.starts_with("image/") || data.is_empty() {
                return None;
            }
            Some(Attachment {
                media_type: media_type.into(),
                data_base64: data.into(),
                source_path: None,
            })
        })
        .collect()
}

pub(crate) fn data_url(url: &str) -> Option<Attachment> {
    let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    (mime.starts_with("image/") && !data.is_empty()).then(|| Attachment {
        media_type: mime.into(),
        data_base64: data.into(),
        source_path: None,
    })
}
