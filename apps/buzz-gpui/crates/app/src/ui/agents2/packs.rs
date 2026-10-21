//! Kenny's agent avatar packs (buzz-app's `avatar-packs.ts`): the 49 PNG
//! posters are bundled as assets and `catalog.json` names them. A pack
//! avatar is saved as its poster's versioned HTTPS URL from the catalog,
//! so other clients can show it; here the bundled poster stands in for it.
//! Animations are not shown (buzz-app streams them from Block's CDN).

use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Deserialize)]
struct Catalog {
    collections: Vec<Pack>,
    assets: Vec<PackAvatar>,
}

/// A collection, such as Fuzzies.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pack {
    pub id: String,
    pub label: String,
}

/// One avatar of a pack.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackAvatar {
    pub id: String,
    pub label: String,
    pub collection_id: String,
    /// The poster's versioned HTTPS URL, saved as the agent's picture.
    pub url: String,
}

impl PackAvatar {
    /// The bundled poster's asset path.
    pub fn asset(&self) -> String {
        format!("avatar-packs/{}.png", self.id)
    }
}

static CATALOG: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!(
        "../../../../../assets/avatar-packs/catalog.json"
    ))
    .expect("the bundled avatar catalog parses")
});

pub fn packs() -> &'static [Pack] {
    &CATALOG.collections
}

pub fn avatars() -> &'static [PackAvatar] {
    &CATALOG.assets
}

/// The pack avatar a picture URL names, if it is one.
pub fn by_url(url: &str) -> Option<&'static PackAvatar> {
    avatars().iter().find(|avatar| avatar.url == url)
}

pub fn by_id(id: &str) -> Option<&'static PackAvatar> {
    avatars().iter().find(|avatar| avatar.id == id)
}

/// A pack's avatars, in catalog order.
pub fn of_pack(pack: &str) -> Vec<&'static PackAvatar> {
    avatars()
        .iter()
        .filter(|avatar| avatar.collection_id == pack)
        .collect()
}

/// A random number below `n`, from a v4 UUID's random bits.
pub fn random_below(n: usize) -> usize {
    (uuid::Uuid::new_v4().as_u128() % n.max(1) as u128) as usize
}

/// A random avatar from every pack (`randomAgentAvatar`).
pub fn random() -> &'static PackAvatar {
    let all = avatars();
    &all[random_below(all.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_bundles_every_poster() {
        assert_eq!(avatars().len(), 49);
        assert_eq!(
            packs().iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["fuzzies", "gloopies", "pollies"]
        );
        for avatar in avatars() {
            assert!(
                std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets"))
                    .join(avatar.asset())
                    .exists(),
                "{} has a poster",
                avatar.id
            );
            assert!(avatar.url.starts_with("https://"));
        }
        let avatar = random();
        assert_eq!(by_url(&avatar.url).map(|a| &a.id), Some(&avatar.id));
    }
}
