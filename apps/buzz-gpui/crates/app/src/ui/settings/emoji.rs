//! Settings → Custom emoji (`CustomEmojiSettings.tsx`, relay.md §5k.3):
//! upload an image, name it, and publish it in the viewer's kind 30030
//! set; list the viewer's own emoji. There is no remove or rename, as in
//! the reference: saving an existing name replaces its image.

use super::{button, field, form_group, header, inline_header, inset, note, section};
use crate::community::{AppState, Community};
use crate::ui::media;
use buzz_relay_client::emoji::suggest_shortcode;
use buzz_relay_client::messages::{media_byte_limit, normalize_shortcode};
use buzz_relay_client::upload::{self, UploadError, UploadFile, sniff_image};
use buzz_relay_client::{CatalogStatus, CustomEmoji};
use buzz_ui::tokens::radius;
use buzz_ui::{
    ActiveTheme, ButtonVariant, Icon, IconName, Tab, TabVariant, Tabs, TextField, TypeStyled,
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::path::Path;
use std::sync::Arc;

const NOT_AN_IMAGE: &str = "Choose an image file for custom emoji.";
const UPLOAD_FAILED: &str = "Failed to upload emoji image.";

/// Emitted after a save, for the page's toast.
pub struct EmojiAdded(pub String);

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Add,
    Mine,
}

/// The uploaded image waiting for a name.
struct Chosen {
    file_name: String,
    preview: Arc<Image>,
    url: String,
}

pub struct CustomEmojiSettings {
    view: Option<View>,
    name: Entity<InputState>,
    chosen: Option<Chosen>,
    uploading: bool,
    saving: bool,
    error: Option<SharedString>,
    /// Upload, which takes focus when Clear, Save or the empty state's
    /// action replace the focused control.
    upload_focus: FocusHandle,
    focus_upload: bool,
    /// The community the form is for; results for another are dropped.
    community: Option<EntityId>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<EmojiAdded> for CustomEmojiSettings {}

impl CustomEmojiSettings {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("party-parrot"));
        let subscriptions = vec![
            cx.subscribe(&name, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.error = None;
                    cx.notify();
                }
            }),
            cx.observe_global::<AppState>(|this, cx| {
                let current = Self::community(cx).map(|c| c.entity_id());
                if current == this.community {
                    return;
                }
                // Another community: start over.
                this.community = current;
                this.chosen = None;
                this.error = None;
                if let Some(community) = Self::community(cx) {
                    community.read(cx).session.ensure_emoji();
                }
                cx.notify();
            }),
        ];
        if let Some(community) = Self::community(cx) {
            community.read(cx).session.ensure_emoji();
        }
        Self {
            view: None,
            name,
            chosen: None,
            uploading: false,
            saving: false,
            error: None,
            upload_focus: cx.focus_handle(),
            focus_upload: false,
            community: Self::community(cx).map(|c| c.entity_id()),
            _subscriptions: subscriptions,
        }
    }

    fn community(cx: &App) -> Option<Entity<Community>> {
        AppState::current(cx)
    }

    fn view(&self) -> View {
        self.view.unwrap_or(View::Add)
    }

    fn choose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(community) = Self::community(cx) else {
            return;
        };
        let session = community.read(cx).session.clone();
        let owner = community.entity_id();
        // Development only: a headless session has no file dialog.
        let dev_file = std::env::var_os("BUZZ_DEV_PICK_FILE").map(std::path::PathBuf::from);
        let paths = dev_file.is_none().then(|| {
            cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Upload image".into()),
            })
        });
        cx.spawn_in(window, async move |this, cx| {
            let paths = match (dev_file, paths) {
                (Some(path), _) => vec![path],
                (None, Some(paths)) => match paths.await {
                    Ok(Ok(Some(paths))) => paths,
                    _ => return,
                },
                (None, None) => return,
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.uploading = true;
                this.error = None;
                cx.notify();
            });
            let file = cx
                .background_executor()
                .spawn(async move { read_image(&path) })
                .await;
            let result = match file {
                Ok(file) => {
                    let (file_name, preview) = (
                        file.name.clone(),
                        Arc::new(Image::from_bytes(
                            image_format(&file.mime),
                            file.bytes.clone(),
                        )),
                    );
                    upload::upload(session.media(), file)
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|blob| {
                            if blob.mime.starts_with("image/") {
                                Ok(Chosen {
                                    file_name,
                                    preview,
                                    url: blob.url,
                                })
                            } else {
                                Err(NOT_AN_IMAGE.to_string())
                            }
                        })
                }
                Err(error) => Err(error),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.uploading = false;
                if this.community != Some(owner) {
                    return cx.notify();
                }
                match result {
                    Ok(chosen) => {
                        if this.name.read(cx).value().trim().is_empty()
                            && let Some(code) = suggest_shortcode(&chosen.file_name)
                        {
                            this.name
                                .update(cx, |state, cx| state.set_value(code, window, cx));
                        }
                        this.chosen = Some(chosen);
                        this.error = None;
                    }
                    Err(error) => {
                        this.error = Some(if error.is_empty() {
                            UPLOAD_FAILED.into()
                        } else {
                            error.into()
                        })
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.chosen = None;
        self.error = None;
        self.focus_upload = true;
        self.name
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(community), Some(chosen)) = (Self::community(cx), self.chosen.as_ref()) else {
            return;
        };
        let Some(code) = normalize_shortcode(self.name.read(cx).value().as_ref()) else {
            return;
        };
        if self.saving {
            return;
        }
        self.saving = true;
        self.error = None;
        let session = community.read(cx).session.clone();
        let owner = community.entity_id();
        let url = chosen.url.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = session.add_emoji(&code, &url).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                if this.community != Some(owner) {
                    return cx.notify();
                }
                match result {
                    Ok(()) => {
                        this.clear(window, cx);
                        cx.emit(EmojiAdded(code));
                    }
                    Err(error) => {
                        this.error = Some(if error.is_empty() {
                            "Failed to add emoji.".into()
                        } else {
                            error.into()
                        })
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn add_view(
        &self,
        mine: &[CustomEmoji],
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let busy = self.uploading || self.saving;
        let upload_label = if self.uploading {
            "Uploading…"
        } else if self.chosen.is_some() {
            "Choose different image"
        } else {
            "Upload image"
        };
        let focused = self.upload_focus.is_focused(window);
        let upload = div()
            .track_focus(&self.upload_focus)
            .rounded(radius::CONTROL)
            .when(focused, |d| {
                d.shadow(vec![
                    BoxShadow::new(px(0.), px(0.), c.border_focus).spread_radius(px(2.)),
                ])
            })
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if !busy && matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    cx.stop_propagation();
                    this.choose(window, cx);
                }
            }))
            .child(
                button("emoji-upload", upload_label)
                    .icon(IconName::CloudUpload)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, window, cx| this.choose(window, cx))),
            );
        let error = self.error.clone().map(|error| {
            div()
                .px(inset())
                .text_body_sm()
                .text_color(c.text_danger)
                .child(error)
        });
        let Some(chosen) = &self.chosen else {
            // `EmptyState` with the upload button as its action.
            return div()
                .flex()
                .flex_col()
                .gap(rems(1.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(rems(0.5))
                        .py(rems(2.))
                        .text_center()
                        .child(Icon::new(IconName::Smiley).size(32.).color(c.text_subtle))
                        .child(div().text_label().child("Upload an image"))
                        .child(
                            div().text_body_sm().text_color(c.text_subtle).child(
                                "Upload a GIF, PNG, JPEG, or WebP. Square images work best.",
                            ),
                        )
                        .child(div().pt(rems(0.5)).child(upload)),
                )
                .children(error)
                .into_any_element();
        };
        let value = self.name.read(cx).value().to_string();
        let code = normalize_shortcode(&value);
        let invalid = !value.trim().is_empty() && code.is_none();
        let replaces = code
            .as_ref()
            .filter(|code| mine.iter().any(|emoji| emoji.shortcode == **code));
        let colon = |c: &buzz_ui::Colors| div().text_color(c.text_subtle).child(":");
        let name_field = field(
            "Give it a name",
            TextField::new(&self.name)
                .invalid(invalid)
                .disabled(self.saving)
                .leading(colon(&c))
                .trailing(colon(&c)),
            Some("Type this name between colons in messages and reactions.".into()),
            invalid.then(|| "Use only letters, numbers, hyphen, or underscore.".into()),
            cx,
        );
        form_group(
            vec![
                inline_header("Selected image", None, Vec::new(), cx)
                    .mb(rems(0.))
                    .px(rems(0.))
                    .into_any_element(),
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.75))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .size(rems(4.))
                            .border_1()
                            .border_color(c.border_standard)
                            .rounded(px(12.))
                            .child(
                                img(chosen.preview.clone())
                                    .size(rems(3.5))
                                    .object_fit(ObjectFit::Contain),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(rems(0.5))
                            .items_start()
                            .child(
                                div()
                                    .w_full()
                                    .truncate()
                                    .text_body_sm()
                                    .child(chosen.file_name.clone()),
                            )
                            .child(upload),
                    )
                    .into_any_element(),
                name_field.into_any_element(),
            ]
            .into_iter()
            .chain(replaces.map(|code| {
                div()
                    .text_body_sm()
                    .text_color(c.text_subtle)
                    .child(format!(
                        "You already have :{code}: — saving will replace its image."
                    ))
                    .into_any_element()
            }))
            .chain(error.map(|error| error.px(rems(0.)).into_any_element()))
            .chain(std::iter::once(
                div()
                    .flex()
                    .justify_end()
                    .gap(rems(0.75))
                    .child(
                        button("emoji-clear", "Clear")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.clear(window, cx))),
                    )
                    .child(
                        button(
                            "emoji-save",
                            if self.saving {
                                "Saving…"
                            } else {
                                "Save emoji"
                            },
                        )
                        .variant(ButtonVariant::Prominent)
                        .disabled(busy || code.is_none())
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    )
                    .into_any_element(),
            ))
            .collect(),
            cx,
        )
        .into_any_element()
    }

    fn mine_view(
        &self,
        status: &CatalogStatus,
        mine: &[CustomEmoji],
        community: &Entity<Community>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        match status {
            CatalogStatus::Idle | CatalogStatus::Loading if mine.is_empty() => {
                return note("Loading…", cx).into_any_element();
            }
            CatalogStatus::Error(error) => {
                let session = community.read(cx).session.clone();
                return div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap(rems(0.75))
                    .child(note(error.clone(), cx))
                    .child(
                        div()
                            .px(inset())
                            .child(button("emoji-retry", "Retry emoji").on_click(
                                move |_, _, _| {
                                    session.refresh_emoji();
                                },
                            )),
                    )
                    .into_any_element();
            }
            _ => {}
        }
        if mine.is_empty() {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .gap(rems(0.5))
                .py(rems(2.))
                .text_center()
                .child(Icon::new(IconName::Smiley).size(32.).color(c.text_subtle))
                .child(div().text_label().child("No emojis yet"))
                .child(
                    div()
                        .text_body_sm()
                        .text_color(c.text_subtle)
                        .child("Add an image to use in messages and reactions."),
                )
                .child(
                    div()
                        .pt(rems(0.5))
                        .child(button("emoji-add", "Add emoji").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.view = Some(View::Add);
                                this.focus_upload = true;
                                cx.notify();
                            },
                        ))),
                )
                .into_any_element();
        }
        let media = community.read(cx).session.media().clone();
        div()
            .flex()
            .flex_col()
            .gap(rems(0.5))
            .py(rems(0.75))
            .px(inset())
            .children(mine.iter().map(|emoji| {
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.5))
                    .text_body()
                    .child(
                        img(media::image(&emoji.url, &media))
                            .size(px(22.))
                            .object_fit(ObjectFit::Contain),
                    )
                    .child(format!(":{}:", emoji.shortcode))
            }))
            .into_any_element()
    }
}

impl Render for CustomEmojiSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_upload) {
            window.focus(&self.upload_focus, cx);
        }
        let mut parts = vec![
            header(
                "Custom emoji",
                Some(
                    "Share custom emoji on this relay. Use :name: in messages and reactions."
                        .into(),
                ),
                cx,
            )
            .into_any_element(),
        ];
        let Some(community) = Self::community(cx) else {
            parts.push(note("Add a community to share custom emoji.", cx).into_any_element());
            return section(parts);
        };
        let snapshot = community.read(cx).snapshot.emoji.clone();
        let mine: Vec<CustomEmoji> = snapshot.mine.to_vec();
        let view = self.view();
        let mine_label = if mine.is_empty() {
            "My emojis".to_string()
        } else {
            format!("My emojis ({})", mine.len())
        };
        parts.push(
            div()
                .flex()
                .flex_col()
                .gap(rems(1.))
                .child(
                    div().px(inset()).child(
                        Tabs::new(TabVariant::Panel)
                            .tab(
                                Tab::new("emoji-tab-add", "Add emoji")
                                    .selected(view == View::Add)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.view = Some(View::Add);
                                        cx.notify();
                                    })),
                            )
                            .tab(
                                Tab::new("emoji-tab-mine", mine_label)
                                    .selected(view == View::Mine)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.view = Some(View::Mine);
                                        cx.notify();
                                    })),
                            ),
                    ),
                )
                .child(match view {
                    View::Add => self.add_view(&mine, window, cx),
                    View::Mine => self.mine_view(&snapshot.status, &mine, &community, cx),
                })
                .into_any_element(),
        );
        section(parts)
    }
}

/// Reads a chosen image: GIF, PNG, JPEG or WebP within the upload limit
/// for its type.
fn read_image(path: &Path) -> Result<UploadFile, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("Couldn’t read that file: {error}"))?;
    let mime = sniff_image(&bytes)
        .filter(|mime| {
            matches!(
                *mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        })
        .ok_or(NOT_AN_IMAGE)?;
    if bytes.len() as u64 > media_byte_limit(mime) {
        return Err(UploadError::Size.to_string());
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "emoji".into());
    Ok(UploadFile {
        name,
        mime: mime.into(),
        bytes,
    })
}

fn image_format(mime: &str) -> ImageFormat {
    match mime {
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        _ => ImageFormat::Png,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn reads_only_web_images() {
        let dir = std::env::temp_dir().join(format!("buzz-emoji-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("Party Parrot.png");
        std::fs::write(
            &png,
            include_bytes!("../../../../../assets/icons/buzz-128.png"),
        )
        .unwrap();
        let file = read_image(&png).unwrap();
        assert_eq!(file.mime, "image/png");
        assert_eq!(
            suggest_shortcode(&file.name).as_deref(),
            Some("party_parrot")
        );
        let text = dir.join("notes.txt");
        std::fs::write(&text, b"hello").unwrap();
        assert_eq!(read_image(&text).unwrap_err(), NOT_AN_IMAGE);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
