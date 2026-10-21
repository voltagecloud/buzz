//! Settings → Updates (buzz-app's `UpdateSettings`): "Software updates",
//! the version and the update status in an `EmptyState` card, then the
//! automatic-check switch and the new release's notes. The work happens
//! in `crate::updates`.

use super::{
    SettingsPage, button, empty_card, header, inline_header, preference_row, rows_group, section,
};
use crate::updates::{self, InstallKind, Status, Updater};
use buzz_ui::{ActiveTheme, IconName, Switch, TypeStyled, tokens::radius};
use gpui_kit::base::text::{TextView, TextViewStyle};
use gpui_kit::*;

/// What the card says and offers for the updater's state.
#[derive(Debug, PartialEq)]
struct Card {
    message: String,
    error: bool,
    action: Option<(&'static str, Click)>,
    loading: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Click {
    Check,
    Install,
    Restart,
    Download,
    Release,
}

fn card(status: &Status, kind: InstallKind, version: Option<String>, percent: Option<u32>) -> Card {
    let version = version.unwrap_or_default();
    let card = |message: String, action: Option<(&'static str, Click)>| Card {
        message,
        error: false,
        action,
        loading: false,
    };
    match status {
        Status::Idle => card(
            "Check if a new version is available.".into(),
            Some(("Check for updates", Click::Check)),
        ),
        Status::Checking => Card {
            loading: true,
            ..card(
                "Checking for updates…".into(),
                Some(("Check for updates", Click::Check)),
            )
        },
        Status::UpToDate => card(
            "You're on the latest version.".into(),
            Some(("Check again", Click::Check)),
        ),
        Status::Available => match kind {
            InstallKind::Browser => card(
                format!("Buzz {version} is available. Download it to update."),
                Some(("Download", Click::Download)),
            ),
            InstallKind::Dev | InstallKind::Deb => card(
                format!(
                    "Buzz {version} is available. Automatic updates aren't available on this \
                     build. Download the latest release manually."
                ),
                Some(("View release", Click::Release)),
            ),
        },
        Status::Downloading => Card {
            loading: true,
            ..card(
                match percent {
                    Some(percent) => format!("Downloading update… {percent}%"),
                    None => "Downloading update…".into(),
                },
                Some(("Update now", Click::Install)),
            )
        },
        Status::Ready => card(
            format!("Buzz {version} is ready to install. Installing asks for your password."),
            Some(("Update now", Click::Install)),
        ),
        Status::Installing => Card {
            loading: true,
            ..card(
                "Installing update…".into(),
                Some(("Update now", Click::Install)),
            )
        },
        Status::Installed => card(
            format!("Buzz {version} is installed. Restart Buzz to use it."),
            Some(("Restart Buzz", Click::Restart)),
        ),
        Status::Error(message) => Card {
            error: true,
            ..card(
                format!("Update failed: {message}"),
                Some(("Retry", Click::Check)),
            )
        },
    }
}

fn click(action: Click, cx: &mut App) {
    match action {
        Click::Check => updates::check(true, cx),
        Click::Install => updates::install(cx),
        Click::Restart => updates::restart(cx),
        Click::Download => updates::open_download(cx),
        Click::Release => updates::open_release(cx),
    }
}

impl SettingsPage {
    pub(super) fn updates_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // Opening the section checks, once; the toast yields to it
        // (`showInline`).
        if *Updater::get(cx).status() == Status::Idle {
            updates::check(true, cx);
        }
        crate::ui::toasts::dismiss(updates::TOAST, cx);
        let c = *cx.colors();
        let updater = Updater::get(cx);
        let state = card(
            updater.status(),
            updater.kind(),
            updater.version().map(|version| version.to_string()),
            updater.fraction().map(|fraction| (fraction * 100.) as u32),
        );
        let offered = updater.release().is_some();
        let status = div()
            .text_body_sm()
            .text_color(if state.error {
                c.text_danger
            } else {
                c.text_subtle
            })
            .child(state.message);
        let card = empty_card(
            if offered {
                IconName::Download
            } else {
                IconName::ArrowsClockwise
            },
            if offered {
                c.text_standard
            } else {
                c.text_subtle
            },
            format!("Version {}", crate::updates::VERSION),
            status,
            state.action.map(|(label, action)| {
                button("check-updates", label)
                    .loading(state.loading)
                    .on_click(move |_, _, cx| click(action, cx))
                    .into_any_element()
            }),
            cx,
        );
        let mut children = vec![
            header("Software updates", None, cx).into_any_element(),
            card.into_any_element(),
            automatic(cx),
        ];
        children.extend(notes(cx));
        section(children)
    }
}

/// The automatic-check switch.
fn automatic(cx: &App) -> AnyElement {
    let preferences = Updater::get(cx).preferences();
    rows_group(
        vec![
            preference_row(
                None,
                "Check for updates automatically",
                Some(
                    "Buzz looks for a new release on GitHub when it starts and every 6 hours."
                        .into_any_element(),
                ),
                vec![
                    Switch::new("updates-automatic", preferences.automatic)
                        .on_change(move |automatic, window, cx| {
                            updates::set_preferences(updates::Preferences { automatic }, cx);
                            window.refresh();
                        })
                        .into_any_element(),
                ],
                cx,
            )
            .into_any_element(),
        ],
        cx,
    )
    .into_any_element()
}

/// The new release's notes, as GitHub has them.
fn notes(cx: &App) -> Option<AnyElement> {
    let c = cx.colors();
    let release = Updater::get(cx).release()?;
    let version = release.version()?;
    let published = release
        .published_at
        .as_deref()
        .and_then(|date| chrono::DateTime::parse_from_rfc3339(date).ok())
        .map(|date| {
            let local: chrono::DateTime<chrono::Local> = date.into();
            SharedString::from(format!("Released {}", local.format("%B %-d, %Y")))
        });
    let body = release
        .body
        .as_deref()
        .map(notes_markdown)
        .filter(|body| !body.trim().is_empty())
        .unwrap_or_else(|| "No release notes.".into());
    let style = TextViewStyle::default()
        .with_foreground(c.text_standard)
        .with_muted_foreground(c.text_subtle)
        .with_link(c.text_link)
        .with_border(c.border_standard)
        .with_code_background(c.affordance_subtle)
        .with_selection(c.text_link.opacity(0.25));
    let view_on_github = button("update-release-page", "View on GitHub")
        .icon_end(IconName::ArrowSquareOut)
        .on_click(|_, _, cx| updates::open_release(cx))
        .into_any_element();
    Some(
        div()
            .child(inline_header(
                format!("What's new in {version}"),
                published,
                vec![view_on_github],
                cx,
            ))
            .child(
                div()
                    .p(rems(1.))
                    .border_1()
                    .border_color(c.border_standard)
                    .rounded(radius::CONTAINER)
                    .text_body_sm()
                    .child(TextView::markdown("update-notes", body).style(style)),
            )
            .into_any_element(),
    )
}

/// GitHub's notes with their headings as bold lines: the card sits under
/// its own header, and Markdown headings would outsize it.
fn notes_markdown(body: &str) -> String {
    body.replace("\r\n", "\n")
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let level = trimmed.chars().take_while(|c| *c == '#').count();
            match (1..=6).contains(&level) && trimmed[level..].starts_with(' ') {
                true => format!("**{}**", trimmed[level..].trim()),
                false => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn v() -> Option<String> {
        Some("0.1.1".into())
    }

    #[test]
    fn notes_headings_become_bold_lines() {
        assert_eq!(
            notes_markdown("## What's Changed\r\n* A by @b\r\n\r\n#3 is fixed"),
            "**What's Changed**\n* A by @b\n\n#3 is fixed"
        );
    }

    #[test]
    fn copy_follows_the_reference() {
        let idle = card(&Status::Idle, InstallKind::Deb, None, None);
        assert_eq!(idle.message, "Check if a new version is available.");
        assert_eq!(idle.action, Some(("Check for updates", Click::Check)));
        let latest = card(&Status::UpToDate, InstallKind::Deb, None, None);
        assert_eq!(latest.message, "You're on the latest version.");
        assert_eq!(latest.action.unwrap().0, "Check again");
        let failed = card(
            &Status::Error("offline".into()),
            InstallKind::Deb,
            v(),
            None,
        );
        assert_eq!(failed.message, "Update failed: offline");
        assert!(failed.error);
        assert_eq!(failed.action.unwrap().0, "Retry");
    }

    #[test]
    fn each_install_kind_offers_its_own_step() {
        let dev = card(&Status::Available, InstallKind::Dev, v(), None);
        assert!(
            dev.message
                .contains("Automatic updates aren't available on this build.")
        );
        assert_eq!(dev.action.unwrap().1, Click::Release);
        let browser = card(&Status::Available, InstallKind::Browser, v(), None);
        assert_eq!(browser.action.unwrap().1, Click::Download);
        let ready = card(&Status::Ready, InstallKind::Deb, v(), None);
        assert_eq!(ready.action, Some(("Update now", Click::Install)));
        let downloading = card(&Status::Downloading, InstallKind::Deb, v(), Some(42));
        assert_eq!(downloading.message, "Downloading update… 42%");
        assert!(downloading.loading);
        let installed = card(&Status::Installed, InstallKind::Deb, v(), None);
        assert_eq!(installed.action, Some(("Restart Buzz", Click::Restart)));
    }
}
