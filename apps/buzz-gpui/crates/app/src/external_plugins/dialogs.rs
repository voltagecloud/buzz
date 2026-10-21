//! Dialogs a plugin opens itself (`ui/openDialog`), such as the form a
//! message action shows once its menu closes (buzz-app's message action
//! `component`). One shows at a time, over the workspace.

use super::capabilities::Failure;
use super::protocol::INVALID_PARAMS;
use super::{ExternalPlugins, Status};
use crate::ui::overlay;
use gpui_kit::*;
use serde_json::{Value, json};
use std::rc::Rc;

/// The open dialog: its plugin and view.
#[derive(Default)]
struct OpenDialog(Option<(String, AnyView)>);

impl Global for OpenDialog {}

/// A question a plugin asked (`ui/confirm`), and where its answer goes.
struct Question {
    title: SharedString,
    description: Option<SharedString>,
    confirm: SharedString,
    cancel: SharedString,
    destructive: bool,
    answer: futures::channel::oneshot::Sender<bool>,
}

#[derive(Default)]
struct Asking(Option<Question>);

impl Global for Asking {}

fn answer(confirmed: bool, cx: &mut App) {
    if let Some(question) = cx.default_global::<Asking>().0.take() {
        let _ = question.answer.send(confirmed);
    }
    cx.refresh_windows();
}

/// `ui/confirm`: asks the user to confirm, as buzz-app's `AlertDialog`,
/// and answers `true` or `false`. A second question replaces the first,
/// which answers `false`.
pub fn confirm(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    let text = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| SharedString::from(text.chars().take(300).collect::<String>()))
    };
    let title = text("title").ok_or((INVALID_PARAMS, "title is required".to_owned()))?;
    let (sender, receiver) = futures::channel::oneshot::channel();
    answer(false, cx);
    cx.default_global::<Asking>().0 = Some(Question {
        title,
        description: text("description"),
        confirm: text("confirm").unwrap_or_else(|| "Confirm".into()),
        cancel: text("cancel").unwrap_or_else(|| "Cancel".into()),
        destructive: params.get("destructive").and_then(Value::as_bool) == Some(true),
        answer: sender,
    });
    cx.refresh_windows();
    Ok(cx.spawn(async move |_| Ok(Value::Bool(receiver.await.unwrap_or(false)))))
}

/// `ui/pickFiles`: the files the user chooses in the system's dialog, as
/// paths (none when they cancel). `multiple` allows several. In
/// development, `BUZZ_DEV_PICK_FILE` stands in for the dialog.
pub fn pick_files(params: &Value, cx: &mut App) -> Result<Task<Result<Value, Failure>>, Failure> {
    if let Some(path) = std::env::var_os("BUZZ_DEV_PICK_FILE") {
        let path = std::path::PathBuf::from(path)
            .to_string_lossy()
            .into_owned();
        return Ok(Task::ready(Ok(json!([path]))));
    }
    let prompt = params
        .get("prompt")
        .and_then(Value::as_str)
        .map(|prompt| SharedString::from(prompt.chars().take(64).collect::<String>()));
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: params.get("multiple").and_then(Value::as_bool) == Some(true),
        prompt,
    });
    Ok(cx.spawn(async move |_| match paths.await {
        Ok(Ok(Some(paths))) => Ok(Value::Array(
            paths
                .into_iter()
                .map(|path| Value::String(path.to_string_lossy().into_owned()))
                .collect(),
        )),
        Ok(Ok(None)) | Err(_) => Ok(json!([])),
        Ok(Err(error)) => Err((3, format!("The file dialog failed: {error}"))),
    }))
}

/// The open plugin dialog over the workspace, if any.
pub fn dialog(window: &Window, cx: &App) -> Option<AnyElement> {
    if let Some(question) = cx
        .try_global::<Asking>()
        .and_then(|asking| asking.0.as_ref())
    {
        use buzz_ui::{AlertDialog, Button, Surface};
        let mut confirm = Button::new("plugin-confirm", question.confirm.clone())
            .on_click(|_, _, cx| answer(true, cx));
        if question.destructive {
            confirm = confirm.destructive();
        } else {
            confirm = confirm.variant(buzz_ui::ButtonVariant::Prominent);
        }
        let mut alert = AlertDialog::new(question.title.clone())
            .action(
                Button::new("plugin-confirm-cancel", question.cancel.clone())
                    .surface(Surface::Floating)
                    .on_click(|_, _, cx| answer(false, cx)),
            )
            .action(confirm);
        if let Some(description) = &question.description {
            alert = alert.description(description.clone());
        }
        let dismiss: overlay::Dismiss = Rc::new(|_, cx| answer(false, cx));
        return Some(overlay::modal(window, alert, Some(dismiss)).into_any_element());
    }
    let (plugin, view) = cx.try_global::<OpenDialog>()?.0.clone()?;
    // A plugin switched off takes its dialog with it.
    if ExternalPlugins::status(&plugin, cx) == Status::Off {
        return None;
    }
    Some(overlay::modal(window, view, None).into_any_element())
}

fn close(cx: &mut App) {
    cx.default_global::<OpenDialog>().0 = None;
    cx.refresh_windows();
}

/// `ui/openDialog`: a dialog titled `title` whose body is a view of kind
/// `dialog` with `contribution` set to `id` and the given `context`.
/// Replaces a dialog the plugin already has open.
pub fn open(plugin: &str, params: &Value, cx: &mut App) -> Result<Value, Failure> {
    let text = |key: &str| params.get(key).and_then(Value::as_str);
    let title = text("title")
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .ok_or((INVALID_PARAMS, "title is required".to_owned()))?;
    let id = text("id").unwrap_or("dialog");
    if !super::protocol::valid_contribution_id(id) {
        return Err((INVALID_PARAMS, "id is not a contribution id".to_owned()));
    }
    let context = params.get("context").cloned().unwrap_or(Value::Null);
    if context.to_string().len() > 16_384 {
        return Err((INVALID_PARAMS, "context is over 16 KiB".to_owned()));
    }
    if cx
        .try_global::<OpenDialog>()
        .and_then(|open| open.0.as_ref())
        .is_some_and(|(owner, _)| owner != plugin)
    {
        return Err((1, "Another dialog is open.".to_owned()));
    }
    use super::view::DialogSize;
    let size = match text("size") {
        None | Some("default") => DialogSize::Default,
        Some("wide") => DialogSize::Wide,
        Some("expanded") => DialogSize::Expanded,
        Some(_) => {
            return Err((
                INVALID_PARAMS,
                "size is default, wide or expanded".to_owned(),
            ));
        }
    };
    let title: SharedString = title.chars().take(64).collect::<String>().into();
    let (view, id) = super::view::ExternalView::framed(
        plugin,
        id,
        title,
        size,
        Rc::new(|_, cx| close(cx)),
        json!({ "context": context }),
        cx,
    );
    cx.default_global::<OpenDialog>().0 = Some((plugin.to_owned(), view));
    cx.refresh_windows();
    Ok(json!({ "view": id }))
}
