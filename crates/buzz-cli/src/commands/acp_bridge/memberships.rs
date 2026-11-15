//! Bounded, short-lived owner membership results shared by history operations.
use super::*;
use std::sync::Mutex;
use uuid::Uuid;

#[derive(Default)]
pub(in super::super) struct Memberships(Mutex<HashMap<Uuid, (Instant, bool)>>);

impl Memberships {
    pub(super) fn cached(&self, channel: Uuid) -> Option<bool> {
        self.0
            .lock()
            .ok()?
            .get(&channel)
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
            .map(|(_, member)| *member)
    }

    pub(super) async fn read<R: BridgeRelay>(
        &self,
        relay: &R,
        scope: &Scope,
        channel: Uuid,
    ) -> Result<bool, CliError> {
        let identity = relay.relay_identity().await?;
        let events = relay.query(json!({"kinds":[39002], "authors":[identity.to_hex()], "#d":[channel.to_string()], "limit":1, "consistency":"strong"})).await?;
        if events.len() > 1 {
            return Err(problem("ambiguous channel membership"));
        }
        let member = events.first().is_some_and(|roster| {
            buzz_sdk::gateway::channel_member(roster, identity, channel, scope.owner)
        });
        let mut cache = self
            .0
            .lock()
            .map_err(|_| problem("membership cache unavailable"))?;
        cache.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(30));
        if cache.len() >= 64 {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(channel, _)| *channel)
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(channel, (Instant::now(), member));
        Ok(member)
    }
}
