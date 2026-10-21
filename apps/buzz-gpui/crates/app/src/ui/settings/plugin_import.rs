//! Settings → Plugins → Add from Git (`PluginImport.tsx`): enter a
//! repository, see the plugins in it with the command each runs, what it
//! may do and who signed it, then install one. Updating an installed
//! plugin opens the same preview for its repository.

use super::form;
use crate::external_plugins::{ExternalPlugins, Plan, git, release};
use crate::ui::modal::Dismissed;
use crate::ui::toasts::{self, Notice};
use buzz_ui::{
    ActiveTheme, Button, Dialog, Icon, IconName, Spinner, Surface, TextField, ToastTone, TypeStyled,
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use std::sync::Arc;
use std::time::Duration;

enum Phase {
    Enter,
    Fetching,
    Preview(Arc<git::Fetched>),
}

pub struct PluginImport {
    repository: Entity<InputState>,
    reference: Entity<InputState>,
    phase: Phase,
    /// The folder being installed.
    installing: Option<String>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<Dismissed> for PluginImport {}

impl PluginImport {
    /// Empty, or for the repository an installed plugin came from, which
    /// it reads at once.
    pub fn new(
        source: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository = cx.new(|cx| {
            InputState::new(window, cx).placeholder("owner/repository, https://… or git@…")
        });
        let reference = cx.new(|cx| InputState::new(window, cx).placeholder("Default branch"));
        fn submit(
            this: &mut PluginImport,
            _: Entity<InputState>,
            event: &InputEvent,
            cx: &mut Context<PluginImport>,
        ) {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.fetch(cx);
            }
        }
        let subscriptions = vec![
            cx.subscribe(&repository, submit),
            cx.subscribe(&reference, submit),
        ];
        let fetch = source.is_some();
        if let Some((url, branch)) = source {
            repository.update(cx, |state, cx| state.set_value(url, window, cx));
            reference.update(cx, |state, cx| state.set_value(branch, window, cx));
        }
        let mut this = Self {
            repository,
            reference,
            phase: Phase::Enter,
            installing: None,
            error: None,
            _subscriptions: subscriptions,
        };
        if fetch {
            this.fetch(cx);
        }
        this
    }

    fn fetch(&mut self, cx: &mut Context<Self>) {
        if matches!(self.phase, Phase::Fetching) {
            return;
        }
        let repository = self.repository.read(cx).value().trim().to_owned();
        if repository.is_empty() {
            return;
        }
        let reference = self.reference.read(cx).value().trim().to_owned();
        // A repository the selected community hosts is read with a NIP-98
        // token signed for exactly that repository, as buzz-app does.
        let signed = crate::community::AppState::current(cx).and_then(|community| {
            let community = community.read(cx);
            let url = buzz_relay_client::projects::community_git_repository(
                &community.origin.http(),
                &repository,
            )?;
            let token = community.session.projects().git_authorization(&url).ok()?;
            Some((url, token))
        });
        let (repository, authorization) = match signed {
            Some((url, token)) => (url, Some(token)),
            None => (repository, None),
        };
        self.phase = Phase::Fetching;
        self.error = None;
        cx.notify();
        let task = cx
            .background_executor()
            .spawn(async move { git::fetch(&repository, &reference, authorization) });
        cx.spawn(async move |this, cx| {
            let fetched = task.await;
            let _ = this.update(cx, |this, cx| {
                match fetched {
                    Ok(fetched) => this.phase = Phase::Preview(Arc::new(fetched)),
                    Err(error) => {
                        this.phase = Phase::Enter;
                        this.error = Some(error.into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn install(&mut self, folder: String, cx: &mut Context<Self>) {
        let Phase::Preview(fetched) = &self.phase else {
            return;
        };
        if self.installing.is_some() {
            return;
        }
        let update = fetched
            .candidate(&folder)
            .and_then(|candidate| ExternalPlugins::install_plan(candidate, cx).ok())
            .is_some_and(|plan| plan != Plan::Add);
        self.installing = Some(folder.clone());
        self.error = None;
        cx.notify();
        let task = ExternalPlugins::install(fetched.clone(), folder, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.installing = None;
                match result {
                    Ok(name) => {
                        let mut notice = Notice::error(
                            "plugin-installed",
                            if update {
                                format!("Updated {name}")
                            } else {
                                format!("Added {name}")
                            },
                        );
                        notice.tone = ToastTone::Success;
                        notice.timeout = Some(Duration::from_secs(6));
                        if !update {
                            notice = notice.description(
                                "It is off. Turn it on in Settings → Plugins when you are ready.",
                            );
                        }
                        toasts::show(notice, cx);
                        cx.emit(Dismissed);
                    }
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn candidate(
        &self,
        candidate: &git::Candidate,
        commit: &str,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let line = |text: String| div().text_body_sm().text_color(c.text_subtle).child(text);
        let mut card = div()
            .flex()
            .flex_col()
            .gap(rems(0.25))
            .p(rems(1.))
            .border_1()
            .border_color(c.border_standard)
            .rounded(rems(0.75));
        let manifest = match &candidate.manifest {
            Ok(manifest) => manifest,
            Err(error) => {
                return card
                    .child(div().text_label().child(if candidate.folder.is_empty() {
                        "(repository root)".to_owned()
                    } else {
                        candidate.folder.clone()
                    }))
                    .child(
                        div()
                            .text_body_sm()
                            .text_color(c.text_danger)
                            .child(error.clone()),
                    )
                    .into_any_element();
            }
        };
        card = card
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rems(0.5))
                    .child(Icon::new(IconName::Plug).size(16.))
                    .child(div().text_label().child(manifest.name.clone()))
                    .child(
                        div()
                            .text_caption()
                            .text_color(c.text_subtle)
                            .child(manifest.id.clone()),
                    ),
            )
            .children(manifest.description.clone().map(line))
            .child(line(format!(
                "In {}",
                if candidate.folder.is_empty() {
                    "the repository root"
                } else {
                    &candidate.folder
                }
            )))
            .child(line(format!(
                "Runs {} with your full access",
                manifest.command_line()
            )));
        if !manifest.permissions.is_empty() {
            card = card.child(line(format!(
                "Can: {}",
                manifest
                    .permissions
                    .iter()
                    .map(|permission| permission.describe())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        card = card.child(match &candidate.signed {
            Ok(Some(publisher)) => line(format!("Signed by {}", release::short_npub(publisher))),
            Ok(None) => line("Unsigned".to_owned()),
            Err(error) => div()
                .text_body_sm()
                .text_color(c.text_danger)
                .child(format!("Signature check failed: {error}")),
        });
        let plan = ExternalPlugins::install_plan(candidate, cx);
        let same = matches!(&plan, Ok(Plan::Update { from }) if from == commit);
        if let Ok(Plan::Update { from }) = &plan {
            card = card.child(line(if same {
                "This is the version installed.".to_owned()
            } else {
                format!(
                    "Replaces the copy installed from {}. It keeps its switch, and restarts now \
                     if it is on.",
                    from.chars().take(7).collect::<String>()
                )
            }));
        }
        if let Err(error) = &plan {
            card = card.child(
                div()
                    .text_body_sm()
                    .text_color(c.text_danger)
                    .child(error.clone()),
            );
        }
        let folder = candidate.folder.clone();
        let installing = self.installing.as_deref() == Some(folder.as_str());
        card.child(
            div().flex().pt(rems(0.5)).child(
                Button::new(
                    SharedString::from(format!("plugin-install-{folder}")),
                    match plan {
                        Ok(Plan::Update { .. }) if same => "Reinstall",
                        Ok(Plan::Update { .. }) => "Update",
                        _ => "Install",
                    },
                )
                .prominent()
                .surface(Surface::Floating)
                .loading(installing)
                .disabled(plan.is_err() || busy)
                .on_click(cx.listener(move |this, _, _, cx| this.install(folder.clone(), cx))),
            ),
        )
        .into_any_element()
    }
}

impl Render for PluginImport {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let error = self
            .error
            .clone()
            .map(|error| div().text_body_sm().text_color(c.text_danger).child(error));
        let dialog = Dialog::new("plugin-import", "Add a plugin from Git")
            .on_close(cx.listener(|_, _, _, cx| cx.emit(Dismissed)));
        match &self.phase {
            Phase::Enter | Phase::Fetching => {
                let fetching = matches!(self.phase, Phase::Fetching);
                let field = |label: &'static str, description: &'static str, input| {
                    form::field(
                        label,
                        Some(description.into()),
                        None,
                        TextField::new(input).surface(Surface::Floating),
                        form::FieldStyle::default(),
                        cx,
                    )
                };
                dialog
                    .description(
                        "Buzz reads the repository and lists its plugins. Nothing in it runs \
                         until you install a plugin and turn it on.",
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(rems(1.))
                            .child(field(
                                "Repository",
                                "HTTPS or SSH. GitHub repositories can be owner/repository.",
                                &self.repository,
                            ))
                            .child(field("Branch or tag", "Optional.", &self.reference))
                            .children(fetching.then(|| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(rems(0.5))
                                    .text_body_sm()
                                    .text_color(c.text_subtle)
                                    .child(Spinner::new().size(14.))
                                    .child("Reading the repository…")
                            }))
                            .children(error),
                    )
                    .footer(
                        Button::new("plugin-import-cancel", "Cancel")
                            .surface(Surface::Floating)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(Dismissed))),
                    )
                    .footer(
                        Button::new("plugin-import-fetch", "Find plugins")
                            .prominent()
                            .loading(fetching)
                            .on_click(cx.listener(|this, _, _, cx| this.fetch(cx))),
                    )
                    .into_any_element()
            }
            Phase::Preview(fetched) => {
                let fetched = fetched.clone();
                let busy = self.installing.is_some();
                let cards: Vec<AnyElement> = fetched
                    .candidates
                    .iter()
                    .map(|candidate| self.candidate(candidate, &fetched.commit, busy, cx))
                    .collect();
                let at = if fetched.reference.is_empty() {
                    String::new()
                } else {
                    format!(" {}", fetched.reference)
                };
                dialog
                    .description(format!(
                        "{}{at} at {}. A plugin runs with your full access and is not \
                         sandboxed; install only plugins you trust.",
                        fetched.repository,
                        fetched.commit.chars().take(7).collect::<String>(),
                    ))
                    .child(
                        div()
                            .id("plugin-import-candidates")
                            .flex()
                            .flex_col()
                            .gap(rems(0.75))
                            .max_h(rems(28.))
                            .overflow_y_scroll()
                            .children(cards)
                            .children(error),
                    )
                    .footer(
                        Button::new("plugin-import-back", "Back")
                            .surface(Surface::Floating)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.phase = Phase::Enter;
                                this.error = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }
        }
    }
}
