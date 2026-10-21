//! Product feedback for the deployment's private inbox (buzz-app's
//! `feedbackUpload` and the outbox's feedback event), for the Feedback
//! plugin's dialog. A send goes through the session's durable outbox, so it
//! survives closing the dialog or the app and is retried as the same event.

use super::capabilities::{Failure, Watch, community, on_tokio};
use super::protocol::INVALID_PARAMS;
use buzz_relay_client::feedback::{self, Category, DIAGNOSTICS_NAME};
use buzz_relay_client::{Attachment, Delivery, EventId, FeedbackState, UploadError, UploadFile};
use gpui_kit::*;
use serde_json::{Value, json};

fn invalid(message: &str) -> Failure {
    (INVALID_PARAMS, message.to_owned())
}

fn state_json(state: &FeedbackState) -> Value {
    json!({
        "hydrated": state.hydrated,
        "available": state.available,
        "error": state.error,
        "pending": state.pending.as_ref().map(|pending| json!({
            "id": pending.id.to_hex(),
            "content": pending.content,
            "delivery": match pending.delivery {
                Delivery::Sending => "sending",
                Delivery::Accepted => "accepted",
                Delivery::Seen => "seen",
                Delivery::Failed => "failed",
                Delivery::Unknown => "unknown",
            },
            "error": pending.error,
        })),
    })
}

fn attachment_json(attachment: &Attachment) -> Value {
    json!({
        "name": attachment.name,
        "url": attachment.url,
        "mime": attachment.mime,
        "size": attachment.size,
        "sha256": attachment.sha256,
    })
}

fn attachment(value: &Value) -> Result<Attachment, Failure> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| invalid("attachments need name, url, mime, size and sha256"))
    };
    Ok(Attachment {
        name: text("name")?,
        url: text("url")?,
        mime: text("mime")?,
        size: value
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid("attachments need name, url, mime, size and sha256"))?,
        sha256: text("sha256")?,
        voice_duration: None,
    })
}

fn category(params: &Value) -> Result<Option<Category>, Failure> {
    match params.get("category").and_then(Value::as_str) {
        None => Ok(None),
        Some(name) => Category::ALL
            .into_iter()
            .find(|category| category.as_str() == name)
            .map(Some)
            .ok_or_else(|| invalid("category is bug, praise or needs-work")),
    }
}

fn event_id(params: &Value) -> Result<EventId, Failure> {
    params
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| EventId::from_hex(id).ok())
        .ok_or_else(|| invalid("id is the pending feedback's id"))
}

/// `feedback/watch`: sends `feedback/changed` with `state`, whether
/// feedback can be sent now and the feedback on its way, whenever it
/// changes.
pub fn watch(
    _params: &Value,
    notify: impl Fn(Value, &mut App) + 'static,
    cx: &mut App,
) -> Result<(Watch, Value), Failure> {
    let session = community(cx)?.read(cx).session.clone();
    let mut receiver = session.feedback();
    let first = state_json(&receiver.borrow());
    let (sender, mut changes) = futures::channel::mpsc::unbounded::<Value>();
    crate::runtime::Runtime::handle(cx).spawn(async move {
        while receiver.changed().await.is_ok() {
            let now = state_json(&receiver.borrow_and_update());
            if sender.unbounded_send(now).is_err() {
                return;
            }
        }
    });
    let forward = cx.spawn(async move |cx| {
        use futures::StreamExt;
        while let Some(state) = changes.next().await {
            cx.update(|cx| notify(json!({ "state": state }), cx));
        }
    });
    Ok((Watch::new((), forward), json!({ "state": first })))
}

/// `feedback/upload`: checks the image at `path` (PNG, JPEG, GIF or
/// WebP, without the editor's private metadata) and uploads it to the
/// deployment's feedback storage.
pub fn upload(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from)
        .ok_or_else(|| invalid("path is required"))?;
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Refuse oversized files before reading them.
        let size = std::fs::metadata(&path).map_or(0, |meta| meta.len());
        if size > 50 * 1024 * 1024 {
            return Err((3, feedback::IMAGE_TOO_LARGE.to_owned()));
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| (3, format!("Couldn’t read that file: {error}")))?;
        let file = feedback::image(&name, bytes).map_err(|error| (3, error))?;
        let mime = file.mime.clone();
        let uploaded = session
            .upload_feedback(file)
            .await
            .and_then(|uploaded| {
                if uploaded.mime == mime {
                    Ok(uploaded)
                } else {
                    Err(UploadError::Invalid)
                }
            })
            .map_err(|error| (3, error.to_string()))?;
        Ok(attachment_json(&uploaded))
    }))
}

/// `feedback/send`: sends `message` with its `category` and
/// `attachments`. `diagnostics` is the diagnostics text to upload first
/// (checked against the event's limits before it is), or the attachment
/// an earlier send uploaded. Answers `{"diagnostics"}`, the diagnostics
/// attachment, for a later send to reuse.
pub fn send(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let category = category(params)?;
    let mut attachments = params
        .get("attachments")
        .and_then(Value::as_array)
        .map(|list| list.iter().map(attachment).collect::<Result<Vec<_>, _>>())
        .transpose()?
        .unwrap_or_default();
    let session = community(cx)?.read(cx).session.clone();
    let mut upload = None;
    let mut reused = None;
    match params.get("diagnostics") {
        Some(Value::String(text)) => {
            // Reserve the largest result before uploading it.
            attachments.push(feedback::diagnostics_placeholder(
                text.len() as u64,
                session.origin(),
            ));
            upload = Some(UploadFile {
                name: DIAGNOSTICS_NAME.into(),
                mime: "text/plain".into(),
                bytes: text.clone().into_bytes(),
            });
        }
        Some(Value::Null) | None => {}
        Some(uploaded) => {
            let uploaded = attachment(uploaded)?;
            reused = Some(uploaded.clone());
            attachments.push(uploaded);
        }
    }
    feedback::event(&message, category, &attachments, session.origin())
        .map_err(|error| (3, error))?;
    Ok(on_tokio(cx, async move {
        let mut diagnostics = reused;
        if let Some(file) = upload {
            let uploaded = session
                .upload_feedback(file)
                .await
                .map_err(|error| (3, error.to_string()))?;
            if !matches!(
                uploaded.mime.as_str(),
                "application/octet-stream" | "text/plain"
            ) {
                return Err((3, UploadError::Invalid.to_string()));
            }
            if let Some(last) = attachments.last_mut() {
                *last = uploaded.clone();
            }
            diagnostics = Some(uploaded);
        }
        session
            .send_feedback(&message, category, &attachments)
            .await
            .map_err(|error| (3, error))?;
        Ok(json!({ "diagnostics": diagnostics.as_ref().map(attachment_json) }))
    }))
}

/// `feedback/retry`: sends the pending feedback again, as the same event.
pub fn retry(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let id = event_id(params)?;
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        session
            .retry_feedback(id)
            .await
            .map(|()| Value::Null)
            .map_err(|error| (3, error))
    }))
}

/// `feedback/dismiss`: forgets the pending feedback on this device. It
/// may already have been delivered; this does not undo that.
pub fn dismiss(params: &Value, cx: &App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let id = event_id(params)?;
    let session = community(cx)?.read(cx).session.clone();
    Ok(on_tokio(cx, async move {
        session
            .dismiss_feedback(id)
            .await
            .map(|()| Value::Null)
            .map_err(|error| {
                if error.is_empty() {
                    (3, "Could not clear feedback.".to_owned())
                } else {
                    (3, error)
                }
            })
    }))
}
