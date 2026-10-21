//! Identity and `buzz://` link targets inside message bodies
//! (`profiles/target.ts`, `navigation/buzz-links.ts`).

use nostr::nips::nip19::FromBech32;
use nostr::prelude::{EventId, PublicKey};
use url::Url;

/// The key behind a `nostr:npub1…` profile link. Other NIP-19 forms are not
/// identity links in message bodies.
pub fn profile_key(target: &str) -> Option<PublicKey> {
    let npub = target.strip_prefix("nostr:")?;
    let valid = npub.strip_prefix("npub1").is_some_and(|data| {
        !data.is_empty()
            && data
                .bytes()
                .all(|b| b"023456789acdefghjklmnpqrstuvwxyz".contains(&b))
    });
    if !valid {
        return None;
    }
    PublicKey::from_bech32(npub).ok()
}

/// A `buzz://channel/<id>[/<message>]` or
/// `buzz://message?channel=<id>&id=<message>[&thread=<root>]` link. Relative
/// to the community it is read in; never an access grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuzzLink {
    pub channel: String,
    pub message: Option<EventId>,
    pub thread: Option<EventId>,
}

pub fn parse_buzz_link(href: &str) -> Option<BuzzLink> {
    if href.len() > 32_768 {
        return None;
    }
    let url = Url::parse(href).ok()?;
    if url.scheme() != "buzz"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let channel_ok = |id: &str| {
        let mut bytes = id.bytes();
        bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
            && id.len() <= 256
            && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    let event = |id: &str| {
        (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| EventId::from_hex(&id.to_ascii_lowercase()).ok())
            .flatten()
    };
    match url.host_str()? {
        "channel" => {
            let path = url.path().strip_prefix('/')?;
            let parts: Vec<&str> = path.split('/').collect();
            let channel = *parts.first()?;
            if parts.len() > 2 || !channel_ok(channel) {
                return None;
            }
            let message = match parts.get(1) {
                Some(id) => Some(event(id)?),
                None => None,
            };
            Some(BuzzLink {
                channel: channel.to_string(),
                message,
                thread: None,
            })
        }
        "message" if matches!(url.path(), "" | "/") => {
            let all = |key: &str| {
                url.query_pairs()
                    .filter(|(name, _)| name == key)
                    .map(|(_, value)| value.into_owned())
                    .collect::<Vec<_>>()
            };
            let (channel, id, thread) = (all("channel"), all("id"), all("thread"));
            if channel.len() != 1 || id.len() != 1 || thread.len() > 1 {
                return None;
            }
            if !channel_ok(&channel[0]) {
                return None;
            }
            Some(BuzzLink {
                channel: channel[0].clone(),
                message: Some(event(&id[0])?),
                thread: match thread.first() {
                    Some(root) => Some(event(root)?),
                    None => None,
                },
            })
        }
        _ => None,
    }
}

/// The link "Copy link" puts on the clipboard.
pub fn message_link(channel: &str, id: &EventId) -> String {
    let mut url = Url::parse("buzz://message").expect("static URL");
    url.query_pairs_mut()
        .append_pair("channel", channel)
        .append_pair("id", &id.to_hex());
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::nips::nip19::ToBech32;

    #[test]
    fn profile_links_take_npubs_only() {
        let key = buzz_identity::Identity::ephemeral().public_key();
        let npub = key.to_bech32().unwrap();
        assert_eq!(profile_key(&format!("nostr:{npub}")), Some(key));
        assert_eq!(profile_key(&npub), None);
        assert_eq!(profile_key(&format!("nostr:{}", npub.to_uppercase())), None);
        assert_eq!(profile_key("nostr:npub1qqqq"), None);
    }

    #[test]
    fn message_links_round_trip() {
        let id = EventId::from_byte_array([7; 32]);
        let link = message_link("0e9d217a-2d7e", &id);
        assert_eq!(
            link,
            format!("buzz://message?channel=0e9d217a-2d7e&id={}", id.to_hex())
        );
        assert_eq!(
            parse_buzz_link(&link),
            Some(BuzzLink {
                channel: "0e9d217a-2d7e".into(),
                message: Some(id),
                thread: None,
            })
        );
        assert_eq!(
            parse_buzz_link("buzz://channel/general"),
            Some(BuzzLink {
                channel: "general".into(),
                message: None,
                thread: None,
            })
        );
        assert_eq!(parse_buzz_link("buzz://channel/-bad"), None);
        assert_eq!(parse_buzz_link("buzz://message?channel=a"), None);
        assert_eq!(parse_buzz_link("https://channel/general"), None);
    }
}
