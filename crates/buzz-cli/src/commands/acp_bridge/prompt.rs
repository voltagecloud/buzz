//! Bounded ACP image input and reusable uploaded attachment metadata.

use base64::{engine::general_purpose::STANDARD, Engine};
use nostr::Event;
use serde_json::Value;

pub(super) const MAX_IMAGE_BYTES: usize = 1024 * 1024;
pub(super) const MAX_IMAGES: usize = 2;

#[derive(Clone, Default)]
pub(super) struct Prompt {
    pub text: String,
    pub images: Vec<Image>,
    pub media: Vec<Vec<String>>,
}

#[derive(Clone)]
pub(super) struct Image {
    pub bytes: Vec<u8>,
    pub mime: String,
}

impl Prompt {
    pub fn from_event(event: &Event) -> Self {
        Self {
            text: event.content.clone(),
            images: Vec::new(),
            media: event
                .tags
                .iter()
                .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("imeta"))
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
        }
    }
}

pub(super) fn parse(prompt: Option<&Value>) -> Result<Prompt, String> {
    let blocks = prompt
        .and_then(Value::as_array)
        .ok_or_else(|| "prompt must be an array of content blocks".to_owned())?;
    let mut result = Prompt::default();
    let mut parts = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_owned());
                }
            }
            Some("resource_link") => {
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or_default();
                let name = block.get("name").and_then(Value::as_str).unwrap_or(uri);
                parts.push(format!("[{name}]({uri})"));
            }
            Some("image") => {
                if result.images.len() >= MAX_IMAGES {
                    return Err(format!(
                        "at most {MAX_IMAGES} images are allowed per prompt"
                    ));
                }
                let mime = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !["image/png", "image/jpeg", "image/gif", "image/webp"].contains(&mime) {
                    return Err("image type must be PNG, JPEG, GIF, or WebP".into());
                }
                let data = block
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or("image requires base64 data")?;
                if data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
                    return Err("image exceeds 1 MiB".into());
                }
                let bytes = STANDARD
                    .decode(data)
                    .map_err(|_| "invalid image base64 data")?;
                if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
                    return Err("image must contain 1 byte to 1 MiB".into());
                }
                if infer::get(&bytes).map(|kind| kind.mime_type()) != Some(mime) {
                    return Err("image MIME type does not match its bytes".into());
                }
                result.images.push(Image {
                    bytes,
                    mime: mime.to_owned(),
                });
            }
            other => {
                return Err(format!(
                    "unsupported prompt content type: {}",
                    other.unwrap_or("missing")
                ))
            }
        }
    }
    result.text = parts.join("\n\n");
    if result.text.trim().is_empty() && result.images.is_empty() {
        return Err("prompt has no text or images".into());
    }
    Ok(result)
}
