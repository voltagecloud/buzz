//! Settings → Agents → "Saved agent activity" (buzz-app's
//! `ArchiveSettings.tsx`), and the capture that fills the archive while a
//! community is connected.

use super::{Modal, SettingsPage, button, form_group, inline_header, preference_row};
use crate::community::{AppState, Community, IdentityState, data_dir};
use crate::runtime::Runtime;
use crate::ui::overlay::{self, Align, select_menu};
use buzz_identity::Identity;
use buzz_relay_client::archive::{self, Archive, ArchiveSettings, RETENTION_DAYS};
use buzz_ui::{
    ActiveTheme, AlertDialog, Button, ButtonSize, Menu, MenuItem, Select, Surface, Switch,
    TypeStyled,
};
use gpui_kit::*;
use std::rc::Rc;
use std::sync::Arc;

/// The archive file, opened on first use. A failure stays retryable and
/// never stops the app.
#[derive(Default)]
struct ArchiveHost(Option<Arc<Archive>>);

impl Global for ArchiveHost {}

fn open(cx: &mut App) -> Result<Arc<Archive>, String> {
    if let Some(archive) = &cx.default_global::<ArchiveHost>().0 {
        return Ok(archive.clone());
    }
    let archive = Arc::new(Archive::open(archive_path())?);
    cx.global_mut::<ArchiveHost>().0 = Some(archive.clone());
    Ok(archive)
}

fn archive_path() -> std::path::PathBuf {
    data_dir().join("archive").join("events.sqlite3")
}

/// Keeps one archive: another open may have won while `archive` opened.
fn keep(archive: Arc<Archive>, cx: &mut App) -> Arc<Archive> {
    match opened(cx) {
        Some(kept) => kept,
        None => {
            cx.default_global::<ArchiveHost>().0 = Some(archive.clone());
            archive
        }
    }
}

/// The archive file and the settings for one community, as last read.
type Read = Result<(String, ArchiveSettings), String>;

/// The archive settings the card shows. Reading them writes to disk (the
/// archive seeds and prunes), so it runs off the UI thread when the card
/// shows, the community changes, and after a save or clear.
pub(super) struct Loaded {
    community: EntityId,
    /// The last read; `None` until the first finishes.
    read: Option<Read>,
    /// The card showed again since the read; read once more.
    pub(super) stale: bool,
    _task: Task<()>,
}

/// The archive, if it opened; capture opens it when a community connects.
pub fn opened(cx: &App) -> Option<Arc<Archive>> {
    cx.try_global::<ArchiveHost>()?.0.clone()
}

/// The viewer's archive key and identity in `community`.
fn partition(community: &Entity<Community>, cx: &App) -> (String, String) {
    let community = community.read(cx);
    (
        community.session.viewer().to_string(),
        archive::community_key(&community.origin),
    )
}

/// The viewer's archive settings in `community`, and where the archive
/// is (`ArchiveHost.settings`), for plugins.
pub(crate) fn settings_for(
    community: &Entity<Community>,
    cx: &mut App,
) -> Result<(ArchiveSettings, std::path::PathBuf), String> {
    let (viewer, key) = partition(community, cx);
    let archive = open(cx)?;
    Ok((
        archive.settings(&viewer, &key)?,
        archive.path().to_path_buf(),
    ))
}

/// A page of saved records, and every agent with records of its kind.
pub(crate) type SavedPage = (archive::ArchivePage, Vec<String>);

/// One page of the viewer's saved records of `kind` (24200 activity or
/// 44200 metrics) in `community`, newest first, from one `agent` and
/// older than `before` if given (`ArchiveHost.read`), read off the UI
/// thread.
pub(crate) fn read_for(
    community: &Entity<Community>,
    kind: u16,
    agent: Option<String>,
    before: Option<i64>,
    cx: &mut App,
) -> Result<Task<Result<SavedPage, String>>, String> {
    let IdentityState::Ready(identity) = &cx.global::<AppState>().identity else {
        return Err("You are not signed in.".into());
    };
    let identity = identity.clone();
    let (_, key) = partition(community, cx);
    let archive = open(cx)?;
    let read = Runtime::handle(cx).spawn_blocking(move || {
        let page = archive.read(&identity, &key, kind, agent.as_deref(), before)?;
        let agents = archive.agents(&identity.pubkey_hex(), &key, kind)?;
        Ok((page, agents))
    });
    Ok(cx.background_spawn(async move {
        read.await
            .unwrap_or_else(|_| Err("The archive read stopped.".into()))
    }))
}

/// Saves archive settings, as the card does, then captures under them.
pub(crate) fn configure_for(
    community: &Entity<Community>,
    observer: bool,
    metrics: bool,
    observer_days: u32,
    revision: i64,
    cx: &mut App,
) -> Result<ArchiveSettings, String> {
    let (viewer, key) = partition(community, cx);
    let saved = open(cx)?.configure(&viewer, &key, observer, metrics, observer_days, revision)?;
    recapture(community, cx);
    Ok(saved)
}

/// Clears saved records of `kind` (or all), as the card does.
pub(crate) fn clear_for(
    community: &Entity<Community>,
    kind: Option<u16>,
    cx: &mut App,
) -> Result<(), String> {
    let (viewer, key) = partition(community, cx);
    open(cx)?.clear(&viewer, &key, kind)?;
    // Clearing moves the revision; capture follows it.
    recapture(community, cx);
    Ok(())
}

/// [`capture`] again after the settings changed.
fn recapture(community: &Entity<Community>, cx: &mut App) {
    if let IdentityState::Ready(identity) = &cx.global::<AppState>().identity {
        capture(community, identity.clone(), cx);
    }
}

/// The newest [`capture`] of each community, so a slower earlier one
/// never replaces its settings.
#[derive(Default)]
struct Captures(std::collections::HashMap<EntityId, u64>);

impl Global for Captures {}

/// Starts, restarts or stops capturing agent activity for `community`
/// under its current settings. Envelopes are checked (fresh, addressed to
/// the viewer, decrypting) before they are archived. The archive is opened
/// and its settings read off the UI thread: both write to disk, and every
/// community does this at startup.
pub fn capture(community: &Entity<Community>, identity: Arc<Identity>, cx: &mut App) {
    let session = community.read(cx).session.clone();
    let (viewer, key) = partition(community, cx);
    let id = community.entity_id();
    let generation = {
        let captures = &mut cx.default_global::<Captures>().0;
        let generation = captures.get(&id).map_or(0, |g| g + 1);
        captures.insert(id, generation);
        generation
    };
    let existing = opened(cx);
    let read = cx.background_spawn(async move {
        let archive = match existing {
            Some(archive) => archive,
            None => Arc::new(Archive::open(archive_path())?),
        };
        let settings = archive.settings(&viewer, &key)?;
        Ok::<_, String>((archive, settings, viewer, key))
    });
    cx.spawn(async move |cx| {
        let read = read.await;
        cx.update(|cx| {
            if cx.global::<Captures>().0.get(&id) != Some(&generation) {
                return;
            }
            let (archive, settings, viewer, key) = match read {
                Ok(found) => found,
                Err(error) => {
                    tracing::warn!(%error, "agent activity is not archived");
                    session.observe(Vec::new());
                    return;
                }
            };
            // Another open (Settings) may have won; one archive is kept.
            let archive = keep(archive, cx);
            let mut frames = session.observe(settings.kinds());
            if settings.kinds().is_empty() {
                return;
            }
            Runtime::handle(cx).spawn(async move {
                while let Some(event) = frames.recv().await {
                    let (archive, identity, viewer, key) = (
                        archive.clone(),
                        identity.clone(),
                        viewer.clone(),
                        key.clone(),
                    );
                    let revision = settings.revision;
                    let stored = tokio::task::spawn_blocking(move || {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |since| since.as_secs());
                        archive::check_envelope(&identity, &event, now)?;
                        archive.ingest(&viewer, &key, &event, revision)
                    })
                    .await;
                    if let Ok(Err(error)) = stored {
                        tracing::debug!(%error, "an agent activity envelope was not archived");
                    }
                }
            });
        });
    })
    .detach();
}

/// What the archive confirmation dialog is about.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Confirm {
    /// Shorten retention to this many days.
    Retention(u32),
    /// Clear one kind of record.
    Clear(u16),
}

fn days(days: u32) -> String {
    format!("{days} {}", if days == 1 { "day" } else { "days" })
}

impl SettingsPage {
    /// Reads `community`'s archive settings again, showing the last read
    /// for it meanwhile.
    fn archive_load(&mut self, community: &Entity<Community>, cx: &mut Context<Self>) {
        let id = community.entity_id();
        let (viewer, key) = partition(community, cx);
        let existing = opened(cx);
        let read = cx.background_spawn(async move {
            let archive = match existing {
                Some(archive) => archive,
                None => Arc::new(Archive::open(archive_path())?),
            };
            let settings = archive.settings(&viewer, &key)?;
            Ok::<_, String>((archive, settings))
        });
        let task = cx.spawn(async move |this, cx| {
            let read = read.await;
            let _ = this.update(cx, |this, cx| {
                let read = read.map(|(archive, settings)| {
                    let archive = keep(archive, cx);
                    (archive.path().to_string_lossy().into_owned(), settings)
                });
                if let Some(loaded) = &mut this.archive {
                    loaded.read = Some(read);
                }
                cx.notify();
            });
        });
        let previous = self
            .archive
            .take()
            .filter(|loaded| loaded.community == id)
            .and_then(|loaded| loaded.read);
        self.archive = Some(Loaded {
            community: id,
            read: previous,
            stale: false,
            _task: task,
        });
    }

    /// The card (`ArchiveCard`) with its content for the selected community.
    pub(super) fn archive_section(&mut self, window: &Window, cx: &mut Context<Self>) -> Div {
        let c = *cx.colors();
        let community = AppState::current(cx);
        let title = match &community {
            Some(community) => format!("Saved agent activity · {}", community.read(cx).name()),
            None => "Saved agent activity".into(),
        };
        let text = |text: String| {
            div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
                .into_any_element()
        };
        let Some(community) = community else {
            return div()
                .child(inline_header(title, None, Vec::new(), cx))
                .child(form_group(
                    vec![text(
                        "Select a community to manage its saved agent activity.".into(),
                    )],
                    cx,
                ));
        };
        let current = self
            .archive
            .as_ref()
            .filter(|loaded| loaded.community == community.entity_id() && !loaded.stale);
        if current.is_none() {
            self.archive_load(&community, cx);
        }
        let loaded = self.archive.as_ref().and_then(|loaded| loaded.read.clone());
        let mut body = vec![text(
            "Saved in this app’s local SQLite archive on this device. Records are partitioned \
             by account and community. Capture runs while this community is connected, even \
             with Agent Activity disabled."
                .into(),
        )];
        match loaded {
            None => body.push(text("Loading archive settings…".into())),
            Some(Ok((path, settings))) => {
                body.extend(self.archive_controls(&path, &settings, window, cx));
            }
            Some(Err(_)) => {
                body.push(
                    div()
                        .text_body_sm()
                        .text_color(c.text_danger)
                        .child(
                            "Could not read archive settings. No capture preference was changed.",
                        )
                        .into_any_element(),
                );
                body.push(
                    button("archive-reload", "Reload archive settings")
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.archive_error = None;
                            this.archive = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
        }
        if let Some(error) = self.archive_error.clone() {
            body.push(
                div()
                    .text_body_sm()
                    .text_color(c.text_danger)
                    .child(error)
                    .into_any_element(),
            );
        }
        div()
            .child(inline_header(title, None, Vec::new(), cx))
            .child(form_group(body, cx))
    }

    fn archive_controls(
        &mut self,
        path: &str,
        settings: &ArchiveSettings,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let c = *cx.colors();
        let note = |text: String| {
            div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(text)
                .into_any_element()
        };
        let switch = |id: &'static str,
                      title: &'static str,
                      description: &'static str,
                      checked: bool,
                      apply: fn(&mut ArchiveSettings, bool),
                      cx: &mut Context<Self>| {
            let current = settings.clone();
            let page = cx.entity().downgrade();
            preference_row(
                None,
                title,
                Some(description.into_any_element()),
                vec![
                    Switch::new(id, checked)
                        .on_change(move |value, _, cx| {
                            let mut next = current.clone();
                            apply(&mut next, value);
                            let _ = page.update(cx, |this, cx| this.archive_save(next, cx));
                        })
                        .into_any_element(),
                ],
                cx,
            )
            .into_any_element()
        };
        let observer = switch(
            "archive-observer",
            "Save agent activity",
            "Keep encrypted owner-only activity. Turning this off leaves saved history intact.",
            settings.observer,
            |s, v| s.observer = v,
            cx,
        );
        let metrics = switch(
            "archive-metrics",
            "Save agent turn metrics",
            "Keep encrypted token and cost records for 90 days, up to 64 MiB per community \
             (16 MiB per agent). A usage dashboard is not included.",
            settings.metrics,
            |s, v| s.metrics = v,
            cx,
        );
        // "Activity retention": a field select, shorter values confirmed.
        let current = settings.clone();
        let menu = || {
            let items = RETENTION_DAYS.map(|choice| {
                let current = current.clone();
                MenuItem::new(
                    SharedString::from(format!("retention-{choice}")),
                    days(choice),
                )
                .checked(choice == current.observer_days)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.retention_menu = false;
                    if choice < current.observer_days {
                        this.modal = Some(Modal::Archive(Confirm::Retention(choice)));
                    } else if choice != current.observer_days {
                        this.archive_save(
                            ArchiveSettings {
                                observer_days: choice,
                                ..current.clone()
                            },
                            cx,
                        );
                    }
                    cx.notify();
                }))
            });
            Menu::new()
                .child(
                    div()
                        .px(rems(0.75))
                        .py(rems(0.375))
                        .text_caption()
                        .text_color(c.text_subtle)
                        .child("Maximum age"),
                )
                .children(items)
                .into_any_element()
        };
        let page = cx.entity().downgrade();
        let dismiss: overlay::Dismiss = Rc::new(move |_, cx| {
            let _ = page.update(cx, |this, cx| {
                this.retention_menu = false;
                cx.notify();
            });
        });
        let select = Select::new("archive-retention")
            .value(days(settings.observer_days))
            .surface(Surface::Floating)
            .on_click(cx.listener(|this, _, _, cx| {
                this.retention_menu = !this.retention_menu;
                cx.notify();
            }));
        let retention = select_menu(
            select,
            self.retention_menu,
            Align::Start,
            Some(rems(16.)),
            menu,
            dismiss,
            window,
        )
        .w_full();
        let mib = settings.bytes as f64 / 1024. / 1024.;
        vec![
            div()
                .text_body_sm()
                .text_color(c.text_subtle)
                .child(div().child(format!("Archive file: {path}")))
                .child(format!(
                    "This account and community: {mib:.1} MiB of encrypted records. SQLite \
                     indexes and temporary journal files use additional disk space."
                ))
                .into_any_element(),
            observer,
            metrics,
            div()
                .flex()
                .flex_col()
                .gap(rems(0.5))
                .child(div().text_label_sm().child("Activity retention"))
                .child(retention)
                .into_any_element(),
            note(
                "Activity is limited to 512 MiB per community and 128 MiB per agent. Oldest \
                 records may be removed sooner at the size limit. Shortening retention deletes \
                 expired records; turning capture off does not delete them. Budgets apply \
                 separately to each account and community, not to the entire device."
                    .into(),
            ),
            div()
                .flex()
                .gap(rems(0.5))
                .child(
                    Button::new("archive-clear-activity", "Clear activity history")
                        .destructive()
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.modal = Some(Modal::Archive(Confirm::Clear(24200)));
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("archive-clear-metrics", "Clear turn metrics")
                        .destructive()
                        .size(ButtonSize::Sm)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.modal = Some(Modal::Archive(Confirm::Clear(44200)));
                            cx.notify();
                        })),
                )
                .into_any_element(),
        ]
    }

    /// Saves the choices and restarts capture under them.
    fn archive_save(&mut self, next: ArchiveSettings, cx: &mut Context<Self>) {
        let Some(community) = AppState::current(cx) else {
            return;
        };
        let (viewer, key) = partition(&community, cx);
        let saved = open(cx).and_then(|archive| {
            archive.configure(
                &viewer,
                &key,
                next.observer,
                next.metrics,
                next.observer_days,
                next.revision,
            )
        });
        self.archive_error = saved
            .err()
            .map(|_| "Could not save archive settings. Reload settings and retry.".into());
        self.archive_load(&community, cx);
        recapture(&community, cx);
        cx.notify();
    }

    fn archive_clear(&mut self, kind: u16, cx: &mut Context<Self>) {
        let Some(community) = AppState::current(cx) else {
            return;
        };
        let (viewer, key) = partition(&community, cx);
        let cleared = open(cx).and_then(|archive| archive.clear(&viewer, &key, Some(kind)));
        self.archive_error = cleared
            .err()
            .map(|_| "Could not clear the archive. Saved records may remain; retry.".into());
        self.archive_load(&community, cx);
        // Clearing moves the revision; capture follows it.
        recapture(&community, cx);
        self.modal = None;
        cx.notify();
    }

    pub(super) fn archive_dialog(&self, confirm: Confirm, cx: &mut Context<Self>) -> AnyElement {
        let community = AppState::current(cx)
            .map(|community| community.read(cx).name())
            .unwrap_or_default();
        let (title, description, action) = match confirm {
            Confirm::Retention(choice) => (
                "Shorten activity retention?".to_string(),
                format!(
                    "Permanently deletes activity older than {} for this account in \
                     {community}. This cannot be undone.",
                    days(choice)
                ),
                "Shorten retention",
            ),
            Confirm::Clear(kind) => {
                let what = if kind == 24200 {
                    "activity"
                } else {
                    "turn metrics"
                };
                (
                    if kind == 24200 {
                        "Clear activity history?".to_string()
                    } else {
                        "Clear turn metrics?".to_string()
                    },
                    format!(
                        "Permanently removes this account’s saved {what} in {community}. New \
                         records can still be captured."
                    ),
                    "Clear saved records",
                )
            }
        };
        AlertDialog::new(title)
            .description(description)
            .action(
                Button::new("archive-cancel", "Cancel")
                    .surface(Surface::Floating)
                    .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))),
            )
            .action(
                Button::new("archive-confirm", action)
                    .destructive()
                    .on_click(cx.listener(move |this, _, _, cx| match confirm {
                        Confirm::Retention(choice) => {
                            this.modal = None;
                            let Some(community) = AppState::current(cx) else {
                                return;
                            };
                            let (viewer, key) = partition(&community, cx);
                            if let Ok(current) =
                                open(cx).and_then(|archive| archive.settings(&viewer, &key))
                            {
                                this.archive_save(
                                    ArchiveSettings {
                                        observer_days: choice,
                                        ..current
                                    },
                                    cx,
                                );
                            }
                        }
                        Confirm::Clear(kind) => this.archive_clear(kind, cx),
                    })),
            )
            .into_any_element()
    }
}
