//! GIF search through the relay's KLIPY proxy (`gifs.ts`,
//! `gif-capability.ts`, relay.md §1.4).
//!
//! The relay holds the provider's API key and advertises the route in
//! NIP-11; the app never sees a key. A chosen GIF is sent as a Markdown
//! image of its URL, never uploaded.

use serde_json::{Value, json};

/// The only search route the native app follows.
pub const SEARCH_PATH: &str = "/gifs/search";

pub const UNAVAILABLE: &str = "GIF search is unavailable";
const FAILED: &str = "GIF search failed";

/// `relayKlipySearchPath` narrowed to the native route: the relay must
/// list `buzz-gif` and point `gif.search` of provider `klipy` at
/// [`SEARCH_PATH`].
pub fn search_path(info: &Value) -> Option<&'static str> {
    let advertised = info["supported_extensions"]
        .as_array()
        .is_some_and(|list| list.iter().any(|value| value == "buzz-gif"));
    (advertised && info["gif"]["provider"] == "klipy" && info["gif"]["search"] == SEARCH_PATH)
        .then_some(SEARCH_PATH)
}

/// The search request body; an empty query asks for trending GIFs.
pub fn search_body(customer_id: &str, locale: &str, query: &str) -> String {
    json!({
        "customer_id": customer_id,
        "locale": locale,
        "query": query.trim(),
    })
    .to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub url: String,
    pub width: f64,
    pub height: f64,
    pub size: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Gif {
    pub id: u64,
    pub slug: String,
    pub title: String,
    /// What the message links to.
    pub original: Asset,
    /// The smaller picture the picker shows.
    pub preview: Asset,
}

/// `completeAsset`: positive finite dimensions and size, and an HTTPS URL.
fn asset(value: &Value) -> Option<Asset> {
    let number = |key: &str| value[key].as_f64().filter(|n| n.is_finite() && *n > 0.);
    let url = value["url"].as_str()?;
    let parsed = url::Url::parse(url).ok()?;
    (parsed.scheme() == "https").then_some(())?;
    Some(Asset {
        url: url.to_owned(),
        width: number("width")?,
        height: number("height")?,
        size: number("size")?,
    })
}

/// `normalizeKlipyGifs` over a search response.
pub fn parse(response: &Value) -> Result<Vec<Gif>, String> {
    if response["result"] == false {
        return Err(FAILED.into());
    }
    let items = response["data"]["data"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut gifs = Vec::new();
    for item in &items {
        let file = &item["file"];
        let slug = item["slug"].as_str().filter(|slug| !slug.is_empty());
        let (Some(slug), true) = (slug, item["type"] == "gif" && file.is_object()) else {
            continue;
        };
        let original = ["md", "hd", "sm", "xs"]
            .iter()
            .find_map(|size| asset(&file[size]["gif"]));
        let Some(original) = original else {
            continue;
        };
        let preview = [("sm", "webp"), ("sm", "gif"), ("xs", "webp"), ("xs", "gif")]
            .iter()
            .find_map(|(size, format)| asset(&file[size][format]))
            .unwrap_or_else(|| original.clone());
        let title = item["title"].as_str().map(str::trim).unwrap_or_default();
        gifs.push(Gif {
            id: item["id"].as_u64().unwrap_or(gifs.len() as u64),
            slug: slug.to_owned(),
            title: if title.is_empty() { "GIF" } else { title }.to_owned(),
            original,
            preview,
        });
    }
    Ok(gifs)
}

/// `gifMarkdown`: brackets out of the title, parentheses escaped in the
/// URL.
pub fn markdown(gif: &Gif) -> String {
    let title = gif
        .title
        .replace(['[', ']'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let url = gif.original.url.replace('(', "%28").replace(')', "%29");
    format!(
        "![{}]({url})",
        if title.is_empty() { "GIF" } else { &title }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(url: &str) -> Value {
        json!({"url": url, "width": 220, "height": 124, "size": 40000})
    }

    #[test]
    fn accepts_only_the_advertised_klipy_route() {
        let info = json!({
            "supported_extensions": ["nip-er", "buzz-gif"],
            "gif": {"provider": "klipy", "search": "/gifs/search"},
        });
        assert_eq!(search_path(&info), Some(SEARCH_PATH));
        let mut other = info.clone();
        other["gif"]["search"] = json!("/api/v2/gifs/search");
        assert_eq!(search_path(&other), None);
        let mut tenor = info.clone();
        tenor["gif"]["provider"] = json!("tenor");
        assert_eq!(search_path(&tenor), None);
        assert_eq!(search_path(&json!({"gif": info["gif"]})), None);
    }

    #[test]
    fn picks_original_and_preview_assets_like_the_reference() {
        let response = json!({"result": true, "data": {"data": [
            {"id": 7, "slug": "wave", "title": " Hi [there] ", "type": "gif", "file": {
                "hd": {"gif": asset("https://k.test/hd.gif")},
                "md": {"gif": {"url": "https://k.test/md.gif", "width": 0, "height": 1, "size": 1}},
                "sm": {"gif": asset("https://k.test/sm.gif"), "webp": asset("http://k.test/sm.webp")},
                "xs": {"webp": asset("https://k.test/xs.webp")},
            }},
            {"slug": "clip", "type": "clip", "file": {"md": {"gif": asset("https://k.test/c.gif")}}},
            {"slug": "", "type": "gif", "file": {"md": {"gif": asset("https://k.test/e.gif")}}},
            {"slug": "plain", "type": "gif", "file": {"xs": {"gif": asset("https://k.test/p(1).gif")}}},
        ]}});
        let gifs = parse(&response).unwrap();
        assert_eq!(gifs.len(), 2);
        // md is incomplete, so hd; the http webp is skipped.
        assert_eq!(gifs[0].original.url, "https://k.test/hd.gif");
        assert_eq!(gifs[0].preview.url, "https://k.test/sm.gif");
        assert_eq!(gifs[0].title, "Hi [there]");
        assert_eq!(markdown(&gifs[0]), "![Hi there](https://k.test/hd.gif)");
        assert_eq!(gifs[1].title, "GIF");
        assert_eq!(gifs[1].id, 1);
        assert_eq!(gifs[1].preview, gifs[1].original);
        assert_eq!(markdown(&gifs[1]), "![GIF](https://k.test/p%281%29.gif)");
        assert_eq!(parse(&json!({"result": false})), Err(FAILED.into()));
    }

    #[test]
    fn sends_a_trimmed_query() {
        let body: Value = serde_json::from_str(&search_body("c-1", "en-US", "  cats ")).unwrap();
        assert_eq!(
            body,
            json!({"customer_id": "c-1", "locale": "en-US", "query": "cats"})
        );
    }
}
