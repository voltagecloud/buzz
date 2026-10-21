//! Settings → Plugins (§5b.5 item 6, `Settings.tsx` plugins section): one
//! row per bundled plugin with an enable switch; Channels is required.
//! Below them, the plugins the user added from folders
//! ([`crate::external_plugins`]), each with the command it runs.

use super::plugin_import::PluginImport;
use super::{Modal, SettingsPage, button, header, note, preference_row, rows_group, section};
use crate::external_plugins::{ExternalPlugins, Status, release};
use crate::plugins::{CHANNELS, Plugins, Unavailable};
use crate::ui::modal::Dismissed;
use crate::ui::toasts::{self, Notice};
use buzz_ui::{ActiveTheme, Icon, IconName, Switch, TypeStyled};
use buzz_ui::{AlertDialog, Button, Surface};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::path::PathBuf;

impl SettingsPage {
    pub(super) fn plugins_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let c = *cx.colors();
        let yours = your_plugins(cx);
        let plugins = cx.global::<Plugins>();
        let busy = plugins.busy();
        let mut children = vec![
            header("Plugins", None, cx).into_any_element(),
            yours,
            div()
                .pt(rems(1.5))
                .px(super::inset())
                .text_heading()
                .child("Built in")
                .into_any_element(),
        ];
        match plugins.unavailable().cloned() {
            Some(Unavailable::Recovery(reason)) => children.push(recovery(
                reason,
                busy,
                plugins.error().map(str::to_owned),
                cx,
            )),
            Some(Unavailable::Storage(_)) => children.push(
                note(
                    "Plugin settings are unavailable. Profile and Appearance still work.",
                    cx,
                )
                .into_any_element(),
            ),
            None => {
                let rows = plugins
                    .rows()
                    .iter()
                    .map(|plugin| {
                        let icon = Some(
                            Icon::new(IconName::SquaresFour)
                                .size(20.)
                                .into_any_element(),
                        );
                        let row = if plugin.id == CHANNELS {
                            preference_row(
                                icon,
                                plugin.name.clone(),
                                None,
                                vec![
                                    div()
                                        .text_body_sm()
                                        .text_color(c.text_subtle)
                                        .child("Required")
                                        .into_any_element(),
                                ],
                                cx,
                            )
                        } else {
                            let id = plugin.id.clone();
                            let enabled = plugin.enabled;
                            // One of Buzz's own that runs as a program
                            // says when it could not.
                            let stopped = match ExternalPlugins::bundled_status(&plugin.id, cx) {
                                Some(Status::Failed(reason)) => Some(
                                    div()
                                        .text_body_sm()
                                        .text_color(c.text_danger)
                                        .child(format!(
                                            "Stopped: {}",
                                            reason.lines().next().unwrap_or_default()
                                        ))
                                        .into_any_element(),
                                ),
                                _ => None,
                            };
                            preference_row(
                                icon,
                                plugin.name.clone(),
                                stopped,
                                vec![
                                    Switch::new(
                                        SharedString::from(format!("plugin-{}", plugin.id)),
                                        enabled,
                                    )
                                    .disabled(busy)
                                    .on_change(move |on, _, cx| Plugins::change(&id, on, cx))
                                    .into_any_element(),
                                ],
                                cx,
                            )
                        };
                        div().py(rems(0.25)).child(row).into_any_element()
                    })
                    .collect();
                children.push(rows_group(rows, cx).into_any_element());
            }
        }
        section(children)
    }

    /// Opens Add from Git, reading `source` at once when updating.
    pub(super) fn import_plugin(
        &mut self,
        source: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| PluginImport::new(source, window, cx));
        let subscription = cx.subscribe(&dialog, |this, _, _: &Dismissed, cx| this.close_modal(cx));
        self.modal = Some(Modal::PluginImport {
            dialog,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn confirm_sign(&mut self, path: PathBuf, name: String, cx: &mut Context<Self>) {
        self.modal = Some(Modal::SignPlugin {
            path,
            name,
            busy: false,
            error: None,
        });
        cx.notify();
    }

    fn sign_plugin(&mut self, cx: &mut Context<Self>) {
        let Some(Modal::SignPlugin {
            path, busy, error, ..
        }) = &mut self.modal
        else {
            return;
        };
        if *busy {
            return;
        }
        *busy = true;
        *error = None;
        let path = path.clone();
        let task = ExternalPlugins::sign(&path, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(publisher) => {
                    this.close_modal(cx);
                    this.show_toast(format!("Signed as {}", release::short_npub(&publisher)), cx);
                }
                Err(reason) => {
                    if let Some(Modal::SignPlugin { busy, error, .. }) = &mut this.modal {
                        *busy = false;
                        *error = Some(reason.into());
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn sign_dialog(
        &self,
        name: &str,
        busy: bool,
        error: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        AlertDialog::new(format!("Sign {name} as you?"))
            .description(
                "This writes plugin.artifact.json and plugin.signature.json to its folder. \
                 Anyone who adds this folder sees that you published it, and later versions \
                 must be signed by you too. Signing does not make a plugin safe.",
            )
            .children(
                error.map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            )
            .action(
                Button::new("plugin-sign-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))),
            )
            .action(
                Button::new("plugin-sign-confirm", "Sign")
                    .prominent()
                    .loading(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.sign_plugin(cx))),
            )
            .into_any_element()
    }

    /// Asks for a plugin folder and adds it, off.
    fn add_plugin_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add plugin".into()),
        });
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            cx.update(|_, cx| {
                if let Err(reason) = ExternalPlugins::add(folder, cx) {
                    toasts::show(
                        Notice::error("plugin-add", "Couldn’t add the plugin").description(reason),
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }
}

/// The plugins added from folders.
fn your_plugins(cx: &mut Context<SettingsPage>) -> AnyElement {
    let c = *cx.colors();
    let safe_mode = cx.global::<ExternalPlugins>().safe_mode;
    let rows: Vec<AnyElement> = cx
        .global::<ExternalPlugins>()
        .plugins()
        .iter()
        .filter(|plugin| !plugin.bundled)
        .enumerate()
        .map(|(index, plugin)| {
            let path = plugin.path.clone();
            let (status, failed) = match (&plugin.manifest, &plugin.status) {
                (Err(reason), _) => (reason.clone(), true),
                (Ok(_), Status::Off) => ("Off".to_owned(), false),
                (Ok(_), Status::Paused) => ("Paused for this launch".to_owned(), false),
                (Ok(_), Status::Starting) => ("Starting…".to_owned(), false),
                (Ok(_), Status::Running) => ("Running".to_owned(), false),
                (Ok(_), Status::Failed(reason)) => (
                    format!("Stopped: {}", reason.lines().next().unwrap_or_default()),
                    true,
                ),
            };
            let mut description = div().flex().flex_col().gap(rems(0.125)).child(
                div()
                    .when(failed, |status| status.text_color(c.text_danger))
                    .child(status),
            );
            if let Ok(manifest) = &plugin.manifest {
                if let Some(text) = &manifest.description {
                    description = description.child(text.clone());
                }
                description = description.child(match &plugin.source {
                    Some(source) => format!(
                        "Runs {} · from {}{} at {}",
                        manifest.command_line(),
                        source.repository,
                        if source.reference.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", source.reference)
                        },
                        source.commit.chars().take(7).collect::<String>()
                    ),
                    None => format!(
                        "Runs {} in {}",
                        manifest.command_line(),
                        plugin.path.display()
                    ),
                });
                if !manifest.permissions.is_empty() {
                    let asks = manifest
                        .permissions
                        .iter()
                        .map(|permission| permission.describe())
                        .collect::<Vec<_>>()
                        .join(", ");
                    description = description.child(format!("Can: {asks}"));
                }
                description = description.child(match &plugin.publisher {
                    Some(publisher) => format!("Signed by {}", release::short_npub(publisher)),
                    None => "Unsigned".to_owned(),
                });
            }
            // An installed copy updates from its repository; a folder
            // reloads and can be signed for sharing.
            let first = match &plugin.source {
                Some(source) => {
                    let source = (source.repository.clone(), source.reference.clone());
                    button(
                        SharedString::from(format!("plugin-update-{index}")),
                        "Update",
                    )
                    .ghost()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.import_plugin(Some(source.clone()), window, cx)
                    }))
                }
                None => {
                    let path = path.clone();
                    button(
                        SharedString::from(format!("plugin-reload-{index}")),
                        "Reload",
                    )
                    .ghost()
                    .on_click(move |_, _, cx| ExternalPlugins::reload(&path, cx))
                }
            };
            let sign = (plugin.source.is_none() && plugin.manifest.is_ok()).then(|| {
                let path = path.clone();
                let name = plugin.name();
                button(SharedString::from(format!("plugin-sign-{index}")), "Sign")
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm_sign(path.clone(), name.clone(), cx)
                    }))
            });
            let remove = {
                let path = path.clone();
                button(
                    SharedString::from(format!("plugin-remove-{index}")),
                    "Remove",
                )
                .ghost()
                .on_click(move |_, _, cx| ExternalPlugins::remove(&path, cx))
            };
            let switch = Switch::new(
                SharedString::from(format!("plugin-external-{index}")),
                plugin.enabled,
            )
            .disabled(plugin.manifest.is_err() && !plugin.enabled)
            .on_change(move |on, _, cx| ExternalPlugins::set_enabled(&path, on, cx));
            div()
                .py(rems(0.25))
                .child(preference_row(
                    Some(Icon::new(IconName::Plug).size(20.).into_any_element()),
                    plugin.name(),
                    Some(description.into_any_element()),
                    std::iter::once(first.into_any_element())
                        .chain(sign.map(IntoElement::into_any_element))
                        .chain([remove.into_any_element(), switch.into_any_element()])
                        .collect(),
                    cx,
                ))
                .into_any_element()
        })
        .collect();
    let empty = rows.is_empty();
    div()
        .flex()
        .flex_col()
        .gap(rems(0.75))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(rems(1.))
                .pl(super::inset())
                .child(div().text_heading().child("Your plugins"))
                .child(
                    div()
                        .flex()
                        .gap(rems(0.5))
                        .child(
                            button("plugin-add-git", "Add from Git…")
                                .icon(IconName::GitBranch)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.import_plugin(None, window, cx)
                                })),
                        )
                        .child(
                            button("plugin-add", "Add plugin folder…")
                                .icon(IconName::FolderSimple)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_plugin_folder(window, cx)
                                })),
                        ),
                ),
        )
        .when(safe_mode, |list| {
            list.child(note(
                "Safe mode: the plugins you added are paused for this launch. Restart Buzz \
                 without BUZZODZ_SAFE_MODE=1 to run them.",
                cx,
            ))
        })
        .child(note(
            "A plugin is a program on this computer. It runs with your full access and is not \
             sandboxed, so turn on only plugins you trust. Saving a file in its folder restarts it.",
            cx,
        ))
        .when(!empty, |list| list.child(rows_group(rows, cx)))
        .into_any_element()
}

/// `RecoveryScreen`: the saved settings could not be read.
fn recovery(reason: String, busy: bool, error: Option<String>, cx: &App) -> AnyElement {
    let c = cx.colors();
    div()
        .flex()
        .flex_col()
        .gap(rems(1.))
        .p(rems(2.))
        .border_1()
        .border_color(c.border_standard)
        .rounded(rems(1.5))
        .child(div().text_heading().child("Couldn’t open Buzz"))
        .child(
            div()
                .text_body()
                .child("Buzz couldn’t read its plugin configuration."),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(rems(0.25))
                .child(div().text_label_sm().child("Error details"))
                .child(div().text_body_sm().text_color(c.text_subtle).child(reason)),
        )
        .child(
            div().child(
                button("plugins-retry", "Try again")
                    .disabled(busy)
                    .on_click(|_, _, cx| Plugins::retry(cx)),
            ),
        )
        .child(div().text_body().child(
            "Resetting saves a backup and restores bundled defaults. Installed plugin files \
             remain on disk and can be reinstalled.",
        ))
        .child(
            div().child(
                button("plugins-reset", "Back up & reset settings")
                    .destructive()
                    .disabled(busy)
                    .on_click(|_, _, cx| Plugins::recover(cx)),
            ),
        )
        .children(error.map(|text| div().text_body_sm().text_color(c.text_danger).child(text)))
        .into_any_element()
}
