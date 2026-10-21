//! Keeps the owner's agent list (kind 30177) in each bound community in
//! line with the agents here, and shows the agents it lists that run on
//! another of the owner's devices (buzz-app's `readListings`, `list`,
//! `listedAgents`).

use super::super::listings::{self, Elsewhere, Listing, ListingRead};
use super::{Agents2, http_origin, selected_origin};
use crate::runtime::Runtime;
use buzz_relay_client::RelayHttp;
use gpui_kit::{App, Context};
use std::collections::{HashMap, HashSet};

/// The listing state of the bound communities, by origin. A binding
/// replaced or let go drops its community's state, so nothing written
/// for an old session lands in a new one.
#[derive(Default)]
pub(super) struct Listed {
    /// The viewer's records in each community, as last read and written
    /// since.
    rows: HashMap<String, Vec<Listing>>,
    /// By origin while its records are being read: the listings written
    /// or removed meanwhile, which the read must not undo.
    edits: HashMap<String, HashMap<String, Option<Listing>>>,
    /// By origin, the read under way, by its own number: a read let go
    /// (the connection dropped, the cache was cleared) lands nowhere.
    reading: HashMap<String, u64>,
    /// The number the next read takes.
    next_read: u64,
    /// By origin, why the last read failed.
    failed: HashMap<String, String>,
    /// Agents whose record is being written, so a sync writes each once.
    writing: HashSet<String>,
    /// Origins whose read no longer stands, as the live connection
    /// dropped or the cache was cleared (`agentLibrary.clear()`): their
    /// choices wait for the read again.
    stale: HashSet<String>,
    /// By origin, the session generation the last read was made in.
    read_in: HashMap<String, u64>,
}

impl Listed {
    pub(super) fn forget(&mut self, origin: &str) {
        self.rows.remove(origin);
        self.edits.remove(origin);
        self.reading.remove(origin);
        self.failed.remove(origin);
        self.stale.remove(origin);
        self.read_in.remove(origin);
    }

    /// The community's read no longer stands, and one under way is let
    /// go (`agentLibrary.clear()` aborts it).
    pub(super) fn set_stale(&mut self, origin: &str) {
        self.stale.insert(origin.to_owned());
        self.reading.remove(origin);
    }

    /// The read no longer stands since the session's cache was cleared
    /// in `session`: marked once, so the read that follows is left to
    /// finish.
    pub(super) fn set_cleared(&mut self, origin: &str, session: u64) {
        self.set_stale(origin);
        self.read_in.insert(origin.to_owned(), session);
    }

    /// Whether the last read was made before the session's cache was
    /// cleared.
    pub(super) fn read_before(&self, origin: &str, session: u64) -> bool {
        self.read_in
            .get(origin)
            .is_some_and(|read| *read != session)
    }

    /// The community's listings: as read, or until the first read
    /// finishes, those written since.
    pub(super) fn rows_of(&self, origin: &str) -> Vec<Listing> {
        self.rows.get(origin).cloned().unwrap_or_else(|| {
            self.edits
                .get(origin)
                .map(|edits| edits.values().flatten().cloned().collect())
                .unwrap_or_default()
        })
    }

    /// Replaces or removes the agent's listing, and keeps the change until
    /// the community's records are read, first or again.
    pub(super) fn set_row(&mut self, origin: &str, pubkey: &str, row: Option<Listing>) {
        if !self.rows.contains_key(origin)
            || self.reading.contains_key(origin)
            || self.edits.contains_key(origin)
        {
            self.edits
                .entry(origin.to_owned())
                .or_default()
                .insert(pubkey.to_owned(), row.clone());
        }
        if let Some(rows) = self.rows.get_mut(origin) {
            rows.retain(|entry| entry.pubkey != pubkey);
            rows.extend(row);
        }
    }
}

impl Agents2 {
    /// Reads each bound community's records once, then lists each agent
    /// here that they lack. A failed read is asked for again on the next
    /// sync.
    pub(super) fn sync_listings(&mut self, cx: &mut Context<Self>) {
        let origins: Vec<String> = self
            .bound
            .iter()
            .filter(|(_, bound)| bound.binding.is_some())
            .map(|(origin, _)| origin.clone())
            .collect();
        for origin in origins {
            if self.listed.rows.contains_key(&origin) {
                self.list(&origin, cx);
            } else {
                self.read_listings(&origin, cx);
            }
        }
    }

    /// Reads the community's records again, even once read (buzz-app's
    /// `library.refresh`); one under way answers for it.
    pub fn refresh_listings(&mut self, origin: &str, cx: &mut Context<Self>) {
        self.read_listings(origin, cx);
    }

    fn read_listings(&mut self, origin: &str, cx: &mut Context<Self>) {
        let Some(binding) = self.bound.get(origin).and_then(|b| b.binding.as_ref()) else {
            return;
        };
        // One under way answers for it.
        if self.listed.reading.contains_key(origin) {
            return;
        }
        self.listed.next_read += 1;
        let read_number = self.listed.next_read;
        self.listed.reading.insert(origin.to_owned(), read_number);
        // Watchers see the read begin (`library.refresh`'s `loading`).
        cx.notify();
        let http = RelayHttp::new(binding.session.origin().clone(), binding.identity.clone());
        let viewer = binding.viewer.clone();
        let session = binding.session.generation();
        let read = Runtime::handle(cx).spawn(async move { listings::read(&http, &viewer).await });
        let origin = origin.to_owned();
        cx.spawn(async move |this, cx| {
            let read = read
                .await
                .unwrap_or_else(|_| Err("The agent list read was cancelled".into()));
            let _ = this.update(cx, |this, cx| {
                if this.listed.reading.get(&origin) != Some(&read_number) {
                    return;
                }
                this.listed.reading.remove(&origin);
                this.listed.stale.remove(&origin);
                this.listed.read_in.insert(origin.clone(), session);
                let read = match read {
                    Ok(read) => read,
                    // Its edits stay for the next read, which a sync asks
                    // for.
                    Err(error) => {
                        tracing::warn!(%error, "The agent list could not be read");
                        // Once read, the rows already hold them.
                        if this.listed.rows.contains_key(&origin) {
                            this.listed.edits.remove(&origin);
                        }
                        this.listed.failed.insert(origin.clone(), error);
                        cx.notify();
                        return;
                    }
                };
                this.listed.failed.remove(&origin);
                let edits = this.listed.edits.remove(&origin).unwrap_or_default();
                // A record being written keeps the row it replaces, which
                // its write updates once it lands.
                let writing = &this.listed.writing;
                let previous = this.listed.rows.remove(&origin).unwrap_or_default();
                let kept: Vec<Listing> = previous
                    .into_iter()
                    .filter(|row| writing.contains(&row.pubkey))
                    .collect();
                let mut rows: Vec<Listing> = read
                    .into_iter()
                    .filter(|row| {
                        !edits.contains_key(&row.pubkey)
                            && !kept.iter().any(|kept| kept.pubkey == row.pubkey)
                    })
                    .collect();
                rows.extend(
                    kept.into_iter()
                        .filter(|row| !edits.contains_key(&row.pubkey)),
                );
                rows.extend(edits.into_values().flatten());
                this.listed.rows.insert(origin.clone(), rows);
                cx.notify();
                this.list(&origin, cx);
            });
        })
        .detach();
    }

    /// Lists each agent here in the community whose record is missing or
    /// shows another name, picture or type. Waits for the first read, so
    /// it never replaces a record it has not seen.
    fn list(&mut self, origin: &str, cx: &mut Context<Self>) {
        let Some(rows) = self.listed.rows.get(origin) else {
            return;
        };
        let Some(binding) = self.bound.get(origin).and_then(|b| b.binding.as_ref()) else {
            return;
        };
        let mut writes = Vec::new();
        for saved in self.saved() {
            let identity = saved.summary;
            if identity.deleted
                || identity.owner != binding.viewer
                || http_origin(&identity.relay).as_deref() != Some(origin)
                || self.listed.writing.contains(&identity.pubkey)
            {
                continue;
            }
            let picture = listings::record_picture(identity.picture.as_deref());
            let row = rows.iter().find(|row| row.pubkey == identity.pubkey);
            if row.is_some_and(|row| {
                row.name == identity.name
                    && row.picture == picture
                    && row.runtime.as_deref() == Some(identity.agent_type.as_str())
            }) {
                continue;
            }
            writes.push((
                Listing {
                    pubkey: identity.pubkey,
                    name: identity.name,
                    picture,
                    runtime: Some(identity.agent_type.clone()),
                    prior: row.and_then(|row| row.prior.clone()),
                },
                identity.agent_type,
            ));
        }
        let generation = binding.generation;
        let (session, owner) = (binding.session.clone(), binding.identity.clone());
        for (entry, runtime) in writes {
            self.listed.writing.insert(entry.pubkey.clone());
            let http = RelayHttp::new(session.origin().clone(), owner.clone());
            let owner = owner.clone();
            let write = {
                let entry = entry.clone();
                Runtime::handle(cx)
                    .spawn(async move { listings::publish(&http, &owner, &entry, &runtime).await })
            };
            let origin = origin.to_owned();
            cx.spawn(async move |this, cx| {
                let written = write
                    .await
                    .unwrap_or_else(|_| Err("The agent list was not updated. Retry.".into()));
                let _ = this.update(cx, |this, cx| {
                    this.listed.writing.remove(&entry.pubkey);
                    let current = this
                        .bound
                        .get(&origin)
                        .and_then(|b| b.binding.as_ref())
                        .is_some_and(|binding| binding.generation == generation);
                    match written {
                        Ok(prior) if current => {
                            let pubkey = entry.pubkey.clone();
                            let row = Listing {
                                prior: Some(prior),
                                ..entry
                            };
                            this.listed.set_row(&origin, &pubkey, Some(row));
                            cx.notify();
                            // Lists a rename or new picture made while this
                            // write was under way.
                            this.list(&origin, cx);
                        }
                        Ok(_) => {}
                        Err(error) if current => {
                            tracing::warn!(agent = %entry.name, %error, "Agent was not listed");
                        }
                        Err(_) => {}
                    }
                });
            })
            .detach();
        }
    }

    /// Takes a deleted agent out of every community's listings.
    pub(super) fn unlist(&mut self, pubkey: &str) {
        let origins: Vec<String> = self.listed.rows.keys().cloned().collect();
        for origin in origins {
            self.listed.set_row(&origin, pubkey, None);
        }
    }

    /// The selected community's agents that are not here: ones on the
    /// viewer's other devices. Never run here, and not deleted from here.
    pub fn elsewhere(&self, cx: &App) -> Vec<Elsewhere> {
        let Some(origin) = selected_origin(cx) else {
            return Vec::new();
        };
        self.elsewhere_in(&origin)
    }

    /// The agents your list names at `origin` (its `http` form) that this
    /// device does not run, any runtime or none (`readRelayLibrary`), as
    /// `(pubkey, name)`.
    pub fn listed_elsewhere(&self, origin: &str) -> Vec<(String, String)> {
        let here: HashSet<String> = self.saved().map(|saved| saved.summary.pubkey).collect();
        self.listed
            .rows_of(origin)
            .into_iter()
            .filter(|row| !row.name.is_empty() && !here.contains(&row.pubkey))
            .map(|row| (row.pubkey, row.name))
            .collect()
    }

    /// How far the agent list read at `origin` got: one under way shows
    /// as loading, even over an earlier one (`library.refresh`).
    pub fn listing_state(&self, origin: &str) -> ListingRead {
        if self.listed.reading.contains_key(origin) {
            ListingRead::Loading
        } else if self.listed.stale.contains(origin) {
            ListingRead::Idle
        } else if let Some(error) = self.listed.failed.get(origin) {
            ListingRead::Error(error.clone())
        } else if self.listed.rows.contains_key(origin) {
            ListingRead::Ready
        } else if self
            .bound
            .get(origin)
            .is_some_and(|bound| bound.binding.is_some())
        {
            ListingRead::Idle
        } else {
            ListingRead::Unavailable
        }
    }

    /// [`Self::elsewhere`] at `origin`.
    fn elsewhere_in(&self, origin: &str) -> Vec<Elsewhere> {
        let origin = origin.to_owned();
        let Some(binding) = self.bound.get(&origin).and_then(|b| b.binding.as_ref()) else {
            return Vec::new();
        };
        let here: HashSet<String> = self.saved().map(|saved| saved.summary.pubkey).collect();
        self.listed
            .rows_of(&origin)
            .into_iter()
            .filter(|row| !row.name.is_empty() && !here.contains(&row.pubkey))
            .filter_map(|row| {
                Some(Elsewhere {
                    agent_type: row.runtime?,
                    pubkey: row.pubkey,
                    name: row.name,
                    picture: row.picture,
                    relay: binding.destination.clone(),
                    owner: binding.viewer.clone(),
                })
            })
            .collect()
    }
}
