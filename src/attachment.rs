use std::io::Cursor;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageFormat, ImageReader};
use serde_json::{Value, json};

use crate::{Result, ToolError, fail};

pub(crate) const INVALID: &str = "invalid-attachments";
pub(crate) const VISION_REQUIRED: &str = "attachment-vision-required";

const MAX_FILES: usize = 3;
const MAX_TEXT: usize = 4096;
const MAX_IMAGE: usize = 128 * 1024;

pub(crate) fn validate(value: Option<&Value>) -> Result<Vec<Value>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let files = value
        .as_array()
        .filter(|files| files.len() <= MAX_FILES)
        .ok_or_else(|| ToolError(INVALID.into()))?;
    let mut validated = Vec::with_capacity(files.len());
    let mut text_bytes = 0;
    for file in files {
        let name = file["name"]
            .as_str()
            .filter(|name| !name.is_empty() && name.len() <= 255)
            .ok_or_else(|| ToolError(INVALID.into()))?;
        match (file["text"].as_str(), file["jpeg"].as_str()) {
            (Some(text), None) if !text.contains('\0') => {
                text_bytes += text.len();
                if text_bytes > MAX_TEXT {
                    return fail(INVALID);
                }
                validated.push(json!({"name": name, "text": text}));
            }
            (None, Some(encoded)) if encoded.len() <= MAX_IMAGE => {
                if !STANDARD.decode(encoded).is_ok_and(|bytes| {
                    ImageReader::with_format(Cursor::new(bytes), ImageFormat::Jpeg)
                        .into_dimensions()
                        .is_ok_and(|(w, h)| (1..=768).contains(&w) && (1..=768).contains(&h))
                }) {
                    return fail(INVALID);
                }
                validated.push(json!({"name": name, "jpeg": encoded}));
            }
            _ => return fail(INVALID),
        }
    }
    Ok(validated)
}

pub(crate) fn has_images(files: &[Value]) -> bool {
    files.iter().any(|file| file["jpeg"].is_string())
}

pub(crate) fn names(message: &Value) -> Vec<&str> {
    message["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| file["name"].as_str())
        .collect()
}

// The provider sees only chat-completions fields. Images are sent on the active turn.
pub(crate) fn provider_message(message: &mut Value, active: bool) {
    let Some(files) = message
        .as_object_mut()
        .and_then(|m| m.remove("attachments"))
    else {
        return;
    };
    let mut text = message["content"].as_str().unwrap_or_default().to_owned();
    let mut images = Vec::new();
    for file in files.as_array().into_iter().flatten() {
        let name = file["name"].as_str().unwrap_or_default();
        if let Some(content) = file["text"].as_str() {
            text.push_str(&format!("\n\nAttached document {name:?}:\n{content}"));
        } else if let Some(encoded) = file["jpeg"].as_str() {
            text.push_str(&format!("\n\nAttached image: {name:?}"));
            if active {
                images.push(json!({"type": "image_url", "image_url": {
                    "url": format!("data:image/jpeg;base64,{encoded}")
                }}));
            }
        }
    }
    message["content"] = if images.is_empty() {
        json!(text)
    } else {
        images.insert(0, json!({"type": "text", "text": text}));
        json!(images)
    };
}
