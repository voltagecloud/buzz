//! The "Add link" / "Edit link" dialog (`ComposerLinkDialog.tsx`,
//! `composer-link.ts`, `EditableInput.tsx` `editLink`). The composer edits
//! Markdown, so a link is `[text](url)` source whose markers it hides.

use super::live_markdown::Pair;
use buzz_ui::{ActiveTheme, Button, ButtonVariant, Dialog, Surface, TextField, TypeStyled};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::ops::Range;

/// What the dialog edits: a link already in the draft, a bare URL, or the
/// selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// The source the saved link replaces.
    pub range: Range<usize>,
    pub text: String,
    pub href: String,
    pub existing: bool,
}

/// The link at the selection (`editLink`): a Markdown link the selection
/// is within, else a bare URL, else the selected text as a new link.
pub fn target(
    text: &str,
    selection: Range<usize>,
    links: &[Pair],
    bare: &[Range<usize>],
) -> Target {
    if let Some(pair) = links
        .iter()
        .find(|pair| pair.open.start <= selection.start && selection.end <= pair.close.end)
    {
        let destination = &text[pair.close.clone()];
        let href = destination
            .strip_prefix("](")
            .and_then(|rest| rest.strip_suffix(')'))
            .unwrap_or_default()
            .trim();
        let href = href
            .strip_prefix('<')
            .and_then(|rest| rest.strip_suffix('>'))
            .unwrap_or(href);
        return Target {
            range: pair.open.start..pair.close.end,
            text: unescape(&text[pair.open.end..pair.close.start]),
            href: href.to_string(),
            existing: true,
        };
    }
    if let Some(url) = bare
        .iter()
        .find(|url| url.start <= selection.start && selection.end <= url.end)
    {
        return Target {
            range: url.clone(),
            text: text[url.clone()].to_string(),
            href: text[url.clone()].to_string(),
            existing: true,
        };
    }
    Target {
        range: selection.clone(),
        text: text[selection].to_string(),
        href: String::new(),
        existing: false,
    }
}

/// `composerLinkUrl`: a `buzz://` link, or a credential-free HTTP(S) URL.
pub fn link_url(value: &str) -> Option<String> {
    let href = value.trim();
    if buzz_relay_client::parse_buzz_link(href).is_some() {
        return Some(href.to_string());
    }
    let url = url::Url::parse(href).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.into())
}

/// The checks of `save`, with their wording.
pub fn validate(text: &str, url: &str) -> Result<String, &'static str> {
    let href = link_url(url).ok_or("Use an HTTP(S) URL or a valid buzz:// link.")?;
    if text.contains(['\r', '\n']) {
        return Err("Link text must stay on one line.");
    }
    Ok(href)
}

/// `[text](href)`; empty text shows the URL.
pub fn markdown(text: &str, href: &str) -> String {
    let label = if text.trim().is_empty() { href } else { text };
    let mut escaped = String::with_capacity(label.len());
    for c in label.chars() {
        if matches!(c, '\\' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    if href.contains([' ', '(', ')', '<', '>']) {
        format!("[{escaped}](<{href}>)")
    } else {
        format!("[{escaped}]({href})")
    }
}

fn unescape(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut escaped = false;
    for c in label.chars() {
        if c == '\\' && !escaped {
            escaped = true;
            continue;
        }
        out.push(c);
        escaped = false;
    }
    out
}

pub enum LinkEvent {
    Save { text: String, href: String },
    Remove,
    Close,
}

pub struct LinkDialog {
    pub target: Target,
    text: Entity<InputState>,
    url: Entity<InputState>,
    pub error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<LinkEvent> for LinkDialog {}

impl LinkDialog {
    pub fn new(target: Target, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = cx.new(|cx| InputState::new(window, cx).default_value(target.text.clone()));
        let url = cx.new(|cx| InputState::new(window, cx).default_value(target.href.clone()));
        // The URL when there is text to link, else the text first.
        if target.text.is_empty() {
            text.update(cx, |state, cx| state.focus(window, cx));
        } else {
            url.update(cx, |state, cx| state.focus(window, cx));
        }
        let subscriptions = vec![
            cx.subscribe_in(
                &url,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        this.error = None;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.save(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(&text, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.save(window, cx);
                }
            }),
        ];
        Self {
            target,
            text,
            url,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn save(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let (text, url) = (
            self.text.read(cx).value().to_string(),
            self.url.read(cx).value().to_string(),
        );
        if url.trim().is_empty() {
            return;
        }
        match validate(&text, &url) {
            Ok(href) => cx.emit(LinkEvent::Save { text, href }),
            Err(error) => {
                self.error = Some(error.into());
                cx.notify();
            }
        }
    }
}

impl Render for LinkDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = *cx.colors();
        let empty = self.url.read(cx).value().trim().is_empty();
        let field = |label: &'static str, control: AnyElement, error: Option<SharedString>| {
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .text_label_sm()
                        .text_color(c.text_standard)
                        .child(label),
                )
                .child(control)
                .children(
                    error.map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
                )
        };
        let dialog = Dialog::new(
            "composer-link",
            if self.target.existing {
                "Edit link"
            } else {
                "Add link"
            },
        )
        .on_close(cx.listener(|_, _, _, cx| cx.emit(LinkEvent::Close)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(16.))
                .child(field(
                    "Text",
                    TextField::new(&self.text)
                        .surface(Surface::Floating)
                        .into_any_element(),
                    None,
                ))
                .child(field(
                    "URL",
                    TextField::new(&self.url)
                        .surface(Surface::Floating)
                        .invalid(self.error.is_some())
                        .into_any_element(),
                    self.error.clone(),
                )),
        )
        .when(self.target.existing, |dialog| {
            dialog.footer(
                Button::new("remove-link", "Remove link")
                    .surface(Surface::Floating)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(LinkEvent::Remove))),
            )
        })
        .footer(
            Button::new("cancel-link", "Cancel")
                .surface(Surface::Floating)
                .on_click(cx.listener(|_, _, _, cx| cx.emit(LinkEvent::Close))),
        )
        .footer(
            Button::new("save-link", "Save")
                .variant(ButtonVariant::Prominent)
                .disabled(empty)
                .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
        );
        div()
            .key_context("ComposerLink")
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    cx.emit(LinkEvent::Close);
                }
            }))
            .child(dialog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn validates_like_the_reference() {
        assert_eq!(
            validate("docs", "https://buzz.dev/a"),
            Ok("https://buzz.dev/a".into())
        );
        assert_eq!(
            validate("docs", "javascript:alert(1)"),
            Err("Use an HTTP(S) URL or a valid buzz:// link.")
        );
        assert_eq!(
            validate("docs", "https://user:pw@x.y"),
            Err("Use an HTTP(S) URL or a valid buzz:// link.")
        );
        assert_eq!(
            validate("two\nlines", "https://x.y"),
            Err("Link text must stay on one line.")
        );
        assert_eq!(markdown("a [b]", "https://x.y"), "[a \\[b\\]](https://x.y)");
        assert_eq!(
            markdown("", "https://x.y/(a)"),
            "[https://x.y/(a)](<https://x.y/(a)>)"
        );
    }

    #[test]
    fn finds_the_link_at_the_selection() {
        let text = "see [the \\[docs\\]](https://a.b/c) now";
        let pair = Pair {
            open: 4..5,
            close: 17..33,
            kind: super::super::live_markdown::LINK,
        };
        assert_eq!(
            target(text, 8..8, std::slice::from_ref(&pair), &[]),
            Target {
                range: 4..33,
                text: "the [docs]".into(),
                href: "https://a.b/c".into(),
                existing: true,
            }
        );
        let bare = "go https://x.y/z";
        let url = 3..16;
        assert_eq!(
            target(bare, 6..6, &[], std::slice::from_ref(&url)).href,
            "https://x.y/z"
        );
        let plain = target("pick this", 5..9, &[], &[]);
        assert_eq!((plain.text.as_str(), plain.existing), ("this", false));
    }
}
