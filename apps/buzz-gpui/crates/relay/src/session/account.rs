//! The viewer's own writes outside chat: profile, status and leaving the
//! community. Each is one signed request with its receipt
//! checked, run on the session's runtime so any executor can await it.

use super::util::{self, now, post_event, sign};
use super::{Message, Session};
use crate::account::{self, LeaveOutcome, ProfileEdit};
use crate::emoji;
use crate::event::{Verified, Verifier};
use crate::http::RelayHttp;
use crate::outbox::{PublishError, accept_publish};
use crate::reader::{ReadOptions, Reader};
use crate::user_status::{self, StatusInput, UserStatus};
use buzz_identity::Identity;
use nostr::prelude::EventId;
use std::sync::Arc;
use std::time::Duration;

const STATUS_UNAVAILABLE: &str = "Status updates are unavailable in this community.";
/// A status save gives up after this long (`user-status.ts`).
const STATUS_TIMEOUT: Duration = Duration::from_secs(12);
const PROFILE_TIMEOUT: Duration = Duration::from_secs(30);
/// A profile read gives up after this long (`inspectProfile`).
const PROFILE_READ_TIMEOUT: Duration = Duration::from_secs(12);
/// Adding an emoji gives up after this long (`emoji-directory.ts`).
const EMOJI_TIMEOUT: Duration = Duration::from_secs(12);

pub(super) struct Direct {
    pub http: RelayHttp,
    pub reader: Reader,
    pub identity: Arc<Identity>,
    /// One status save at a time.
    pub status: tokio::sync::Mutex<()>,
    /// Emoji additions run one after another, so none is lost.
    pub emoji: tokio::sync::Mutex<()>,
    /// One channel lifecycle action at a time.
    pub lifecycle: tokio::sync::Mutex<()>,
}

/// Puts the viewer's previous profile back when dropped while armed
/// (`previous` is `Some`).
struct RevertProfile {
    commands: tokio::sync::mpsc::UnboundedSender<Message>,
    viewer: String,
    failed: EventId,
    previous: Option<Option<Verified>>,
}

impl Drop for RevertProfile {
    fn drop(&mut self) {
        let Some(previous) = self.previous.take() else {
            return;
        };
        let (viewer, failed) = (std::mem::take(&mut self.viewer), self.failed);
        let _ = self.commands.send(Message::With(Box::new(move |actor| {
            if actor.profiles.revert(&viewer, &failed, previous) {
                actor.publish();
            }
        })));
    }
}

impl Session {
    /// Publishes the viewer's profile (§5f.5): reads the current kind 0
    /// fresh so fields this app does not edit survive, signs the new one,
    /// shows it at once, and posts it. A refused or unconfirmed save puts
    /// the previous profile back.
    pub async fn publish_profile(&self, edit: ProfileEdit) -> Result<(), String> {
        let direct = (
            self.direct.reader.clone(),
            self.direct.http.clone(),
            self.direct.identity.clone(),
        );
        let commands = self.commands.clone();
        let viewer = self.viewer.clone();
        let work = async move {
            let (reader, http, identity) = direct;
            account::validate(&edit).map_err(|error| error.to_string())?;
            let events = reader
                .read(
                    vec![account::own_profile_filter(&viewer)],
                    ReadOptions {
                        fresh: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| format!("Your profile couldn’t be loaded. {}", error.message))?;
            let existing = account::newest_profile(&events, &viewer).cloned();
            let template = account::profile_template(
                &edit,
                existing.as_ref().map(|event| event.content.as_str()),
                existing.as_ref().map(Verified::created_at_secs),
                now(),
            )
            .map_err(|error| error.to_string())?;
            let event = sign(&identity, template)?;
            let verified = Verifier::default()
                .verify(event.clone())
                .map_err(|error| error.to_string())?;
            // Shown at once; put back unless the relay takes it, also when
            // a timeout drops this future.
            let mut revert = RevertProfile {
                commands: commands.clone(),
                viewer: viewer.clone(),
                failed: event.id,
                previous: Some(existing),
            };
            let _ = commands.send(Message::With(Box::new(move |actor| {
                if actor.profiles.accept(&[verified]) {
                    actor.publish();
                }
            })));
            match accept_publish(post_event(&http, &event).await, &event.id) {
                Ok(_) => {
                    revert.previous = None;
                    Ok(())
                }
                Err(error) => {
                    drop(revert);
                    Err(match error {
                        PublishError::Rejected(message) => message,
                        PublishError::Uncertain(message) => {
                            format!("{message}. Your edits are kept; save again to retry.")
                        }
                    })
                }
            }
        };
        self.run(async move {
            tokio::time::timeout(PROFILE_TIMEOUT, work)
                .await
                .unwrap_or_else(|_| Err("Saving your profile timed out. Try again.".into()))
        })
        .await
    }

    /// Reads the viewer's own profile again, fresh and in the background,
    /// so an avatar hydrated from disk or cut short by roster setup is
    /// current (`ProfileButton.tsx`). Failures are ignored.
    pub fn refresh_own_profile(&self) {
        let reader = self.direct.reader.clone();
        let commands = self.commands.clone();
        let viewer = self.viewer.clone();
        self.runtime.spawn(async move {
            let read = reader
                .read(
                    vec![account::own_profile_filter(&viewer)],
                    ReadOptions {
                        fresh: true,
                        ..ReadOptions::background()
                    },
                )
                .await;
            if let Ok(events) = read {
                let _ = commands.send(Message::With(Box::new(move |actor| {
                    if actor.profiles.accept(&events) {
                        actor.publish();
                    }
                })));
            }
        });
    }

    /// The viewer's profile here, read fresh (`inspectProfile`). `None`
    /// when the viewer has never published one in this community.
    pub async fn own_profile(&self) -> Result<Option<ProfileEdit>, String> {
        let reader = self.direct.reader.clone();
        let viewer = self.viewer.clone();
        self.run(async move {
            let read = reader.read(
                vec![account::own_profile_filter(&viewer)],
                ReadOptions {
                    fresh: true,
                    ..Default::default()
                },
            );
            let events = tokio::time::timeout(PROFILE_READ_TIMEOUT, read)
                .await
                .map_err(|_| "The request timed out.".to_string())?
                .map_err(|error| error.message)?;
            Ok(account::newest_profile(&events, &viewer)
                .map(|event| account::profile_fields(&event.content)))
        })
        .await
    }

    /// The viewer's current status, read fresh. `None` when unset, cleared
    /// or expired.
    pub async fn own_status(&self) -> Result<Option<UserStatus>, String> {
        self.user_status(&self.viewer).await
    }

    /// Saves the viewer's status (§5j.1); empty text and emoji clear it.
    /// Reads the current status first so the new event replaces it.
    pub async fn set_status(&self, input: StatusInput) -> Result<(), String> {
        if self.commands.is_closed() {
            return Err(STATUS_UNAVAILABLE.into());
        }
        let Ok(_guard) = self.direct.status.try_lock() else {
            return Err("A status update is already in progress.".into());
        };
        let reader = self.direct.reader.clone();
        let http = self.direct.http.clone();
        let identity = self.direct.identity.clone();
        let viewer = self.viewer.clone();
        let commands = self.commands.clone();
        let work = async move {
            let events = reader
                .read(
                    vec![user_status::filter(&viewer)],
                    ReadOptions {
                        fresh: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| error.message)?;
            let own: Vec<Verified> = events
                .iter()
                .filter(|event| event.pubkey_hex() == viewer)
                .cloned()
                .collect();
            let previous = user_status::latest_created_at(&own, &viewer);
            let _ = commands.send(Message::With(Box::new(move |actor| {
                actor.statuses_observe(&own);
            })));
            let template = user_status::template(&input, previous, now())
                .map_err(|error| error.to_string())?;
            // Only a signer that may sign statuses can save one.
            let event = sign(&identity, template).map_err(|_| STATUS_UNAVAILABLE.to_string())?;
            accept_publish(post_event(&http, &event).await, &event.id)
                .map_err(|error| error.to_string())?;
            // Shown in the session's statuses at once, as live traffic
            // would.
            if let Ok(saved) = Verifier::default().verify(event) {
                let _ = commands.send(Message::With(Box::new(move |actor| {
                    actor.status_saved(saved);
                })));
            }
            Ok(())
        };
        self.run(async move {
            tokio::time::timeout(STATUS_TIMEOUT, work)
                .await
                .unwrap_or_else(|_| Err("Saving your status timed out. Try again.".into()))
        })
        .await
    }

    /// Adds `code` to the viewer's custom emoji set, or gives it a new
    /// image (§5k.3): reads the set fresh, keeps every other entry, and
    /// publishes the replacement. The catalog shows it once accepted.
    pub async fn add_emoji(&self, code: &str, url: &str) -> Result<(), String> {
        let reader = self.direct.reader.clone();
        let http = self.direct.http.clone();
        let identity = self.direct.identity.clone();
        let commands = self.commands.clone();
        let viewer = self.viewer.clone();
        let (code, url) = (code.to_owned(), url.to_owned());
        // The name (and image link) are checked before anything is read.
        emoji::add_template(None, &code, &url, 0)?;
        let lock = self.direct.emoji.lock().await;
        let work = async move {
            let events = reader
                .read(
                    vec![emoji::own_set_filter(&viewer)],
                    ReadOptions {
                        fresh: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| error.message)?;
            let fresh: Vec<Verified> = emoji::own_set(&events, &viewer)
                .into_iter()
                .cloned()
                .collect();
            // A capped or refused catalog must not hide the latest own
            // set from replacement: the newer of it and the fresh read.
            let (reply, cached) = tokio::sync::oneshot::channel();
            let _ = commands.send(Message::With(Box::new(move |actor| {
                if actor.emoji.accept(&fresh) {
                    actor.publish();
                }
                let _ = reply.send(actor.emoji.own());
            })));
            let cached = cached.await.ok().flatten();
            let fresh = emoji::own_set(&events, &viewer).cloned();
            let current = match (cached, fresh) {
                (Some(cached), Some(fresh)) => Some(if crate::event::is_newer(&fresh, &cached) {
                    fresh
                } else {
                    cached
                }),
                (cached, fresh) => cached.or(fresh),
            };
            let template = emoji::add_template(current.as_deref(), &code, &url, now())?;
            if !emoji::valid_set_template(&template, now()) {
                return Err("Refusing to sign an invalid emoji set".to_owned());
            }
            let event = sign(&identity, template)?;
            let verified = Verifier::default()
                .verify(event.clone())
                .map_err(|error| error.to_string())?;
            util::publish(&http, &event).await?;
            let _ = commands.send(Message::With(Box::new(move |actor| {
                if actor.emoji.accept(&[verified]) {
                    actor.publish();
                }
            })));
            Ok::<(), String>(())
        };
        let result = self
            .run(async move {
                match tokio::time::timeout(EMOJI_TIMEOUT, work).await {
                    Ok(Ok(())) => Ok(()),
                    // The reference shows one message for every failure.
                    Ok(Err(error)) => {
                        tracing::debug!(%error, "adding emoji failed");
                        Err("Failed to add emoji.".into())
                    }
                    Err(_) => Err("Timed out while adding emoji.".into()),
                }
            })
            .await;
        drop(lock);
        result
    }

    /// Asks the relay to end the viewer's membership (NIP-43, §1.8).
    pub async fn leave(&self) -> Result<LeaveOutcome, String> {
        let http = self.direct.http.clone();
        let identity = self.direct.identity.clone();
        self.run(async move {
            let event = sign(&identity, account::leave_template(now()))?;
            account::leave_outcome(post_event(&http, &event).await, &event.id)
        })
        .await
    }

    /// Stops the session for good: pending sends end and nothing writes
    /// local storage afterwards, so it can be purged.
    pub async fn dispose(&self) {
        self.workflows.dispose();
        self.homes.clear();
        let (reply, done) = tokio::sync::oneshot::channel();
        if self.commands.send(Message::Dispose(reply)).is_ok() {
            let _ = done.await;
        }
    }
}
