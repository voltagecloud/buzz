//! Community administration through the session (`bundled/moderation`):
//! the relay-signed roster, owner and admin member commands, and invites.
//! Each is one request run on the session's runtime; the relay decides.

use super::Session;
use super::util::{now, post_event, sign};
use crate::moderation::{self, Change, Invite, Member};
use crate::reader::ReadOptions;

impl Session {
    /// The relay's signing key, once it has identified itself.
    fn authority(&self) -> Result<String, String> {
        self.snapshot
            .borrow()
            .relay
            .as_ref()
            .map(|relay| relay.authority.clone())
            .ok_or_else(|| "Community authority unavailable".to_owned())
    }

    /// One fresh read of the community roster (`readRoster`), verified
    /// against the relay key this session already holds. `None` when the
    /// community publishes no member list.
    pub async fn community_roster(&self) -> Result<Option<Vec<Member>>, String> {
        let author = self.authority()?;
        let reader = self.direct.reader.clone();
        self.run(async move {
            let events = reader
                .read(
                    vec![moderation::roster_filter(&author)],
                    ReadOptions {
                        fresh: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| error.message)?;
            moderation::roster(&events, &author)
        })
        .await
    }

    /// Adds a member, changes a role or removes someone (NIP-43 9030-9032),
    /// signed by the viewer. `Ok` only on a receipt for this event.
    pub async fn change_community_member(&self, change: Change) -> Result<(), String> {
        let http = self.direct.http.clone();
        let identity = self.direct.identity.clone();
        self.run(async move {
            let template = moderation::member_command(&change, now())?;
            let event = sign(&identity, template)?;
            moderation::member_outcome(post_event(&http, &event).await, &event.id)
        })
        .await
    }

    /// Mints an invite link (`POST /api/invites`): it expires after
    /// `ttl_secs` and allows `max_uses` claims, or any number.
    pub async fn mint_invite(
        &self,
        ttl_secs: u64,
        max_uses: Option<u32>,
    ) -> Result<Invite, String> {
        let http = self.direct.http.clone();
        self.run(async move {
            let body = moderation::invite_request(ttl_secs, max_uses)?;
            moderation::invite_outcome(http.post("/api/invites", body).await)
        })
        .await
    }
}
