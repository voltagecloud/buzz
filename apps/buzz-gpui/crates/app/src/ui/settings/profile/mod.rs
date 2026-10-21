//! Settings → Profile (§5b.5 item 1): the selected community's profile,
//! or the default profile new communities start from; identity details,
//! the private key and sign-out.

mod private_key;
mod sign_out;

pub use sign_out::{SignOutClosed, SignOutDialog};

use super::avatar::{
    AvatarDismissed, AvatarDone, AvatarEditor, Preview, popover as avatar_popover,
};
use super::{
    Modal, SettingsPage, button, field, form_group, header, inline_header, note, preference_row,
    rows_group, section,
};
use crate::community::{AppState, Community, IdentityState};
use crate::ui::{media, overlay};
use buzz_relay_client::account::{ABOUT_LIMIT, picture_error};
use buzz_relay_client::{ProfileEdit, RelayOrigin};
use buzz_ui::{
    ActiveTheme, Avatar, AvatarSize, ButtonVariant, IconButton, IconButtonSize, IconName, TextArea,
    TextField, Toast, ToastTone, TypeStyled,
};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use private_key::PrivateKey;
use serde::{Deserialize, Serialize};
use std::rc::Rc;

/// Viewer preference holding the default profile (buzz-app's
/// `client.profile`): what joining a community starts from, and what
/// Settings → Profile edits while no community is selected.
const DEFAULT_PROFILE: &str = "profile";

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    name: String,
    picture: String,
    about: String,
}

/// The viewer's default profile; empty when none is saved.
pub fn default_profile(cx: &App) -> ProfileEdit {
    let state = cx.global::<AppState>();
    let IdentityState::Ready(identity) = &state.identity else {
        return ProfileEdit::default();
    };
    let stored: Stored = state
        .store
        .viewer_pref(&identity.pubkey_hex(), DEFAULT_PROFILE)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    ProfileEdit {
        name: stored.name,
        picture: stored.picture,
        about: stored.about,
    }
}

/// Saves `profile` as the default. A picture on `community`'s own media
/// is replaced by the previous default's, since other communities cannot
/// show it (`profileDefault`).
pub fn remember_profile(profile: ProfileEdit, community: Option<&RelayOrigin>, cx: &App) {
    let profile = without_community_media(profile, &default_profile(cx), community);
    let state = cx.global::<AppState>();
    let IdentityState::Ready(identity) = &state.identity else {
        return;
    };
    let json = serde_json::to_string(&Stored {
        name: profile.name,
        picture: profile.picture,
        about: profile.about,
    })
    .expect("profiles serialize");
    if let Err(error) = state
        .store
        .set_viewer_pref(&identity.pubkey_hex(), DEFAULT_PROFILE, &json)
    {
        tracing::warn!(%error, "could not save the default profile");
    }
}

/// buzz-app's `profileDefault`.
fn without_community_media(
    profile: ProfileEdit,
    previous: &ProfileEdit,
    community: Option<&RelayOrigin>,
) -> ProfileEdit {
    let Some(community) = community else {
        return profile;
    };
    let on_community = url::Url::parse(&profile.picture).is_ok_and(|picture| {
        picture.origin().ascii_serialization() == community.http().trim_end_matches('/')
            && picture.path().starts_with("/media/")
    });
    if on_community {
        ProfileEdit {
            picture: previous.picture.clone(),
            ..profile
        }
    } else {
        profile
    }
}

/// The viewer's profile in the selected community, read fresh when the
/// community is chosen (`inspectProfile`).
enum Load {
    Loading,
    /// Loaded; a community without a kind 0 shows the default profile.
    Ready,
    Failed(SharedString),
}

/// buzz-app's `beginProfileSave`: only the latest save, app-wide, updates
/// the default profile.
static PROFILE_SAVES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const NOT_CURRENT: &str =
    "Your profile change is not current. Your edits are retained; save again to retry.";

/// The profile editor's fields and what they were loaded from.
pub(super) struct ProfileForm {
    community: Option<EntityId>,
    load: Load,
    /// Bumped by every load, so a late result for an earlier community
    /// or attempt is dropped.
    attempt: usize,
    _load: Option<Task<()>>,
    /// Why the last identity copy failed; its toast stays until dismissed.
    copy_failed: Option<SharedString>,
    name: Entity<InputState>,
    about: Entity<TextareaState>,
    /// The avatar URL; the avatar editor changes it.
    picture: String,
    /// The open "Edit avatar" popover. Save waits while it is open, as
    /// buzz-app's `onBusyChange` does.
    avatar: Option<(Entity<AvatarEditor>, [Subscription; 2])>,
    /// The saved profile the fields started from.
    baseline: ProfileEdit,
    saving: bool,
    error: Option<SharedString>,
    /// The private key widget; hidden while the sign-out dialog shows its
    /// own.
    private_key: Entity<PrivateKey>,
    _subscriptions: Vec<Subscription>,
}

impl ProfileForm {
    pub(super) fn new(window: &mut Window, cx: &mut Context<SettingsPage>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx));
        let about = cx.new(|cx| TextareaState::new(window, cx));
        let private_key = cx.new(|cx| PrivateKey::new(true, window, cx));
        let changed = |this: &mut SettingsPage, cx: &mut Context<SettingsPage>| {
            this.profile.error = None;
            cx.notify();
        };
        let _subscriptions = vec![
            cx.subscribe(&name, move |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    changed(this, cx);
                }
            }),
            cx.subscribe(&about, move |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    changed(this, cx);
                }
            }),
        ];
        Self {
            community: None,
            load: Load::Ready,
            attempt: 0,
            _load: None,
            copy_failed: None,
            name,
            about,
            picture: String::new(),
            avatar: None,
            baseline: ProfileEdit::default(),
            saving: false,
            error: None,
            private_key,
            _subscriptions,
        }
    }

    /// Leaving Profile hides the key, as unmounting `PrivateKey` does.
    pub(super) fn hide_private_key(&self, cx: &mut App) {
        self.private_key.update(cx, |key, cx| key.hide(cx));
    }

    fn values(&self, cx: &App) -> ProfileEdit {
        ProfileEdit {
            name: self.name.read(cx).value().to_string(),
            picture: self.picture.clone(),
            about: self.about.read(cx).value().to_string(),
        }
    }

    fn dirty(&self, cx: &App) -> bool {
        self.values(cx) != self.baseline
    }

    fn set(&mut self, values: ProfileEdit, window: &mut Window, cx: &mut App) {
        self.name.update(cx, |state, cx| {
            state.set_value(values.name.clone(), window, cx)
        });
        self.about.update(cx, |state, cx| {
            state.set_value(values.about.clone(), window, cx)
        });
        self.picture = values.picture.clone();
        self.avatar = None;
        self.baseline = values;
    }
}

impl SettingsPage {
    /// Follows the selected community: a switch drops any draft and reads
    /// the viewer's profile there afresh; without a community the form
    /// edits the default profile.
    pub(super) fn sync_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let community = AppState::current(cx);
        let id = community.as_ref().map(Entity::entity_id);
        if id == self.profile.community && self.profile.attempt > 0 {
            return;
        }
        self.profile.community = id;
        match community {
            Some(community) => self.load_profile(&community, window, cx),
            None => {
                self.profile.attempt += 1;
                self.profile._load = None;
                self.profile.load = Load::Ready;
                self.profile.error = None;
                let profile = default_profile(cx);
                self.profile.set(profile, window, cx);
            }
        }
    }

    /// Reads the viewer's kind 0 in `community`; also "Retry loading
    /// profile".
    fn load_profile(
        &mut self,
        community: &Entity<Community>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.profile.attempt += 1;
        let attempt = self.profile.attempt;
        self.profile.load = Load::Loading;
        self.profile.error = None;
        let session = community.read(cx).session.clone();
        self.profile._load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = session.own_profile().await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.profile.attempt != attempt {
                    return;
                }
                match result {
                    Ok(loaded) => {
                        let persisted = loaded.unwrap_or_else(|| default_profile(cx));
                        this.profile.load = Load::Ready;
                        this.profile.set(persisted, window, cx);
                    }
                    Err(error) => this.profile.load = Load::Failed(error.into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Copies a public identity value (`copyIdentity`).
    fn copy_identity(&mut self, value: String, label: &'static str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
        // GPUI reports no failed write; a read-back that differs is one.
        let failed = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .is_some_and(|text| text != value);
        if failed {
            self.profile.copy_failed = Some(
                format!(
                    "Couldn’t copy {}. Select it and copy manually.",
                    label.to_lowercase()
                )
                .into(),
            );
            cx.notify();
        } else {
            self.profile.copy_failed = None;
            self.show_toast(format!("{label} copied."), cx);
        }
    }

    fn retry_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(community) = AppState::current(cx) {
            self.load_profile(&community, window, cx);
        }
    }

    fn cancel_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let baseline = self.profile.baseline.clone();
        self.profile.error = None;
        self.profile.set(baseline, window, cx);
        cx.notify();
    }

    fn save_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let edit = self.profile.values(cx);
        let saved = match buzz_relay_client::account::validate(&edit) {
            Ok(saved) => saved,
            Err(error) => {
                self.profile.error = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        use std::sync::atomic::Ordering;
        let generation = PROFILE_SAVES.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(community) = AppState::current(cx) else {
            // The default profile is local; nothing is published.
            remember_profile(saved.clone(), None, cx);
            self.profile.set(saved, window, cx);
            self.show_toast("Profile updated", cx);
            return;
        };
        // Nothing to save over until the profile here has loaded.
        if !matches!(self.profile.load, Load::Ready) || self.profile.saving {
            return;
        }
        self.profile.saving = true;
        self.profile.error = None;
        let origin = community.read(cx).origin.clone();
        let session = community.read(cx).session.clone();
        cx.spawn_in(window, async move |this, cx| {
            // `publish_profile` reads the current kind 0 first, so fields
            // this editor does not own survive; then a confirming read.
            let result = match session.publish_profile(saved.clone()).await {
                Ok(()) => match session.own_profile().await {
                    Ok(Some(confirmed)) if confirmed == saved => Ok(confirmed),
                    Ok(_) => Err(NOT_CURRENT.to_string()),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            // Finishes even if Settings closed meanwhile.
            if result.is_ok() && PROFILE_SAVES.load(Ordering::SeqCst) == generation {
                let profile = saved.clone();
                cx.update(|_, cx| remember_profile(profile, Some(&origin), cx))
                    .ok();
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.profile.saving = false;
                match result {
                    Ok(confirmed) => {
                        if this.profile.community == Some(community.entity_id()) {
                            this.profile.load = Load::Ready;
                            this.profile.set(confirmed, window, cx);
                        }
                        this.show_toast("Profile updated", cx);
                    }
                    Err(error) => this.profile.error = Some(error.into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// "Edit avatar": opens the popover on the current picture.
    fn open_avatar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.profile.saving || self.profile.avatar.is_some() {
            return;
        }
        let community = AppState::current(cx);
        let picture = self.profile.picture.clone();
        let editor = cx.new(|cx| AvatarEditor::new(picture, community, window, cx));
        let subscriptions = [
            cx.subscribe(&editor, |this, _, AvatarDone(picture), cx| {
                this.profile.picture = picture.clone();
                this.profile.error = None;
                this.close_avatar(cx);
            }),
            cx.subscribe(&editor, |this, _, _: &AvatarDismissed, cx| {
                this.close_avatar(cx)
            }),
        ];
        editor.update(cx, |editor, cx| editor.focus(window, cx));
        self.profile.avatar = Some((editor, subscriptions));
        cx.notify();
    }

    fn close_avatar(&mut self, cx: &mut Context<Self>) {
        self.profile.avatar = None;
        cx.notify();
    }

    pub(super) fn profile_section(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = *cx.colors();
        let community = AppState::current(cx);
        let subtitle = if community.is_some() {
            "Update this community’s profile. Other community profiles stay unchanged."
        } else {
            "Used for new communities. Existing community profiles stay unchanged."
        };
        let mut parts = vec![header("Profile", Some(subtitle.into()), cx).into_any_element()];
        let identity = match &cx.global::<AppState>().identity {
            IdentityState::Ready(identity) => Ok(identity.clone()),
            IdentityState::Loading => Err("Opening your local identity…"),
            _ => Err(
                "Your local identity is unavailable. Connect your identity to edit your profile.",
            ),
        };
        let identity = match identity {
            Ok(identity) => identity,
            Err(status) => {
                parts.push(note(status, cx).into_any_element());
                return section(parts);
            }
        };
        let name = community
            .as_ref()
            .map(|community| community.read(cx).name())
            .unwrap_or_default();
        parts.push(match &self.profile.load {
            Load::Ready => self
                .profile_form(community.as_ref(), window, cx)
                .into_any_element(),
            Load::Loading => {
                note(format!("Loading your profile in {name}…"), cx).into_any_element()
            }
            Load::Failed(error) => div()
                .flex()
                .flex_col()
                .items_start()
                .gap(rems(0.5))
                .child(
                    note(
                        format!("Your profile in {name} couldn’t be loaded. {error}"),
                        cx,
                    )
                    .text_color(c.text_danger),
                )
                .child(div().px(super::inset()).child(
                    button("profile-retry", "Retry loading profile").on_click(
                        cx.listener(|this, _, window, cx| this.retry_profile(window, cx)),
                    ),
                ))
                .into_any_element(),
        });

        // Identity details.
        let hex = identity.pubkey_hex();
        let npub = {
            use nostr::nips::nip19::ToBech32;
            identity.public_key().to_bech32().unwrap_or_default()
        };
        let copy_row = |id: &'static str,
                        label: &'static str,
                        what: &'static str,
                        value: String,
                        cx: &mut Context<Self>| {
            let lower = what.to_lowercase();
            let copied = value.clone();
            preference_row(
                None,
                label,
                Some(
                    div()
                        .text_mono()
                        .text_size(px(12.))
                        .text_color(c.text_subtle)
                        .child(value)
                        .into_any_element(),
                ),
                vec![
                    IconButton::new(id, IconName::Copy)
                        .size(IconButtonSize::Sm)
                        .variant(ButtonVariant::Ghost)
                        .tooltip(format!("Copy {lower}"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy_identity(copied.clone(), what, cx)
                        }))
                        .into_any_element(),
                ],
                cx,
            )
            .into_any_element()
        };
        let rows = vec![
            copy_row("copy-hex", "Public key (hex)", "Public key", hex, cx),
            copy_row(
                "copy-npub",
                "Nostr address (npub)",
                "Nostr address",
                npub,
                cx,
            ),
        ];
        // One key widget at a time; the sign-out dialog brings its own.
        // The key and the Sign out row share one `mt-6` wrapper, so they
        // stack with no gap, as in buzz-app.
        let signing_out = matches!(self.modal, Some(Modal::SignOut { .. }));
        let private_key = (!signing_out).then(|| {
            form_group(
                vec![self.profile.private_key.clone().into_any_element()],
                cx,
            )
        });
        let sign_out = rows_group(
            vec![
                preference_row(
                    None,
                    "Sign out of Buzz",
                    // buzz-app adds that Builderlab and hosted community
                    // logins end too; neither exists here.
                    Some("Remove your private key from this device.".into_any_element()),
                    vec![
                        button("sign-out", "Sign out of Buzz")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_sign_out(window, cx)),
                            )
                            .into_any_element(),
                    ],
                    cx,
                )
                .into_any_element(),
            ],
            cx,
        );
        parts.push(
            div()
                .flex()
                .flex_col()
                .child(inline_header(
                    "Identity details",
                    Some(
                        "Your identity is shared across communities. Public keys are safe to share."
                            .into(),
                    ),
                    Vec::new(),
                    cx,
                ))
                .child(rows_group(rows, cx))
                .child(
                    div()
                        .mt(rems(1.5))
                        .flex()
                        .flex_col()
                        .children(private_key)
                        .child(sign_out),
                )
                .into_any_element(),
        );
        // "Identity wasn’t copied": an error toast that stays until
        // dismissed.
        if let Some(failure) = self.profile.copy_failed.clone() {
            parts.push(crate::ui::toasts::place(
                window,
                Toast::new("identity-copy-failed", "Identity wasn’t copied")
                    .description(failure)
                    .tone(ToastTone::Error)
                    .on_dismiss(cx.listener(|this, _, _, cx| {
                        this.profile.copy_failed = None;
                        cx.notify();
                    })),
            ));
        }
        section(parts)
    }

    fn profile_form(
        &mut self,
        community: Option<&Entity<Community>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let c = *cx.colors();
        let form = &self.profile;
        let values = form.values(cx);
        let dirty = form.dirty(cx);
        // An open avatar editor holds Save, as buzz-app's `onBusyChange`.
        let editing = form.avatar.is_some();
        let busy = form.saving || editing;
        let media = community.map(|community| community.read(cx).session.media().clone());
        let name = values.name.trim().to_string();
        let about_len = values.about.encode_utf16().count();
        let about_error = (about_len > ABOUT_LIMIT).then(|| {
            SharedString::from(format!(
                "Shorten the description to {ABOUT_LIMIT} characters before saving."
            ))
        });
        let picture_error =
            picture_error(&values.picture).map(|error| SharedString::from(error.to_string()));

        // The compact AvatarEditor: an 80px avatar, or the draft while the
        // editor is open, with the edit badge over its lower right.
        let draft = form
            .avatar
            .as_ref()
            .map(|(editor, _)| editor.read(cx).preview(cx));
        let artwork = match draft {
            Some(Preview::Emoji { art, color }) => div()
                .size_full()
                .rounded_full()
                .overflow_hidden()
                .bg(color)
                .children(art.map(|art| img(art).size_full()))
                .into_any_element(),
            draft => {
                let picture = match draft {
                    Some(Preview::Picture(picture)) => picture,
                    _ => values.picture.clone(),
                };
                let mut avatar = Avatar::new(if name.is_empty() { "?" } else { &name }.to_string())
                    .size(AvatarSize::Px(80.));
                if !picture.is_empty()
                    && buzz_relay_client::account::picture_error(&picture).is_none()
                {
                    avatar = avatar.image(match &media {
                        Some(media) => media::avatar(&picture, media),
                        None => ImageSource::from(SharedString::from(picture)),
                    });
                }
                avatar.into_any_element()
            }
        };
        let page = cx.entity().downgrade();
        let dismiss: overlay::Dismiss = Rc::new(move |_, cx| {
            let _ = page.update(cx, |this, cx| this.close_avatar(cx));
        });
        let popover = form
            .avatar
            .as_ref()
            .map(|(editor, _)| avatar_popover(window, editor.clone(), dismiss));
        let badge = div()
            .absolute()
            // `.editBadge`: centred at 85.9375% of the frame.
            .left(px(80. * 0.859375 - 16.))
            .top(px(80. * 0.859375 - 16.))
            .child(
                IconButton::new(
                    "avatar-edit",
                    if values.picture.is_empty() {
                        IconName::Plus
                    } else {
                        IconName::PencilSimple
                    },
                )
                .size(IconButtonSize::Sm)
                .variant(ButtonVariant::Prominent)
                .shape(buzz_ui::IconButtonShape::Round)
                .disabled(form.saving)
                .tooltip("Edit avatar")
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.profile.avatar.is_some() {
                        this.close_avatar(cx);
                    } else {
                        this.open_avatar(window, cx);
                    }
                })),
            )
            .children(popover);
        let editor = div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(rems(0.5))
            .child(div().relative().size(rems(5.)).child(artwork).child(badge))
            .children(
                picture_error
                    .map(|error| div().text_body_sm().text_color(c.text_danger).child(error)),
            );
        let preview = div()
            .flex()
            .items_center()
            .gap(rems(1.))
            .child(editor)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .gap(rems(0.25))
                    .child(div().text_label_sm().child(if name.is_empty() {
                        "Your profile".to_string()
                    } else {
                        name.clone()
                    }))
                    .when(!values.about.trim().is_empty(), |this| {
                        this.child(
                            div()
                                .text_body_sm()
                                .text_color(c.text_subtle)
                                .child(values.about.trim().to_string()),
                        )
                    }),
            );
        let actions = dirty.then(|| {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_end()
                .gap(rems(0.75))
                .mt(rems(0.5))
                .child(
                    button("profile-cancel", "Cancel").disabled(busy).on_click(
                        cx.listener(|this, _, window, cx| this.cancel_profile(window, cx)),
                    ),
                )
                .child(
                    button("profile-save", "Save")
                        .prominent()
                        .loading(form.saving)
                        .disabled(editing || buzz_relay_client::account::validate(&values).is_err())
                        .on_click(cx.listener(|this, _, window, cx| this.save_profile(window, cx))),
                )
        });
        form_group(
            vec![
                preview.into_any_element(),
                field(
                    "Display name",
                    TextField::new(&form.name).disabled(form.saving),
                    None,
                    None,
                    cx,
                )
                .into_any_element(),
                field(
                    "Profile description (optional)",
                    TextArea::new(&form.about)
                        .rows(3)
                        .invalid(about_error.is_some())
                        .disabled(form.saving),
                    Some(format!("{about_len} of {ABOUT_LIMIT} characters").into()),
                    about_error,
                    cx,
                )
                .into_any_element(),
            ]
            .into_iter()
            .chain(form.error.clone().map(|error| {
                div()
                    .text_body_sm()
                    .text_color(c.text_danger)
                    .child(error)
                    .into_any_element()
            }))
            .chain(actions.map(IntoElement::into_any_element))
            .collect(),
            cx,
        )
    }

    /// "Sign out of Buzz": the dialog, with its own key widget.
    fn open_sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.profile.hide_private_key(cx);
        let dialog = cx.new(|cx| SignOutDialog::new(window, cx));
        let _subscription = cx.subscribe(&dialog, |this, _, _: &SignOutClosed, cx| {
            this.close_modal(cx)
        });
        self.modal = Some(Modal::SignOut {
            dialog,
            _subscription,
        });
        cx.notify();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    #[test]
    fn community_media_stays_out_of_the_default() {
        let community = RelayOrigin::parse("wss://relay.example.com").unwrap();
        let previous = ProfileEdit {
            picture: "https://cdn.example.org/me.png".into(),
            ..Default::default()
        };
        let edit = |picture: &str| ProfileEdit {
            name: "Alice".into(),
            picture: picture.into(),
            about: String::new(),
        };
        let local = without_community_media(
            edit("https://relay.example.com/media/abc.png"),
            &previous,
            Some(&community),
        );
        assert_eq!(local.picture, previous.picture);
        assert_eq!(local.name, "Alice");
        for kept in [
            "https://elsewhere.example.com/media/abc.png",
            "https://relay.example.com/avatars/abc.png",
            "",
        ] {
            assert_eq!(
                without_community_media(edit(kept), &previous, Some(&community)).picture,
                kept
            );
        }
        let picture = "https://relay.example.com/media/abc.png";
        assert_eq!(
            without_community_media(edit(picture), &previous, None).picture,
            picture
        );
    }
}
