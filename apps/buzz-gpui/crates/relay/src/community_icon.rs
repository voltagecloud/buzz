//! A community's rail icon from its relay's NIP-11 `icon`
//! (`communities/community-icon.ts`): untrusted relay metadata, kept out of
//! executable URL schemes and huge data URLs.

use base64::Engine;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use regex::Regex;
use std::sync::LazyLock;
use url::Url;

const SVG_PREFIX: &str = "data:image/svg+xml,";
/// The largest inline image, in UTF-16 units as JavaScript measures it.
const MAX_INLINE: usize = 98_304;
const MAX_URL: usize = 2048;

/// `encodeURIComponent` keeps these.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// The fixed template of Buzz emoji avatars.
static EMOJI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^<svg xmlns="http://www\.w3\.org/2000/svg" width="512" height="512" viewBox="0 0 512 512"><rect width="512" height="512" rx="(112|256)" fill="(#[0-9a-fA-F]{6})"/><text x="50%" y="56%" dominant-baseline="middle" text-anchor="middle" font-size="258">((?:[^<>&]|&(?:amp|lt|gt);)+)</text></svg>$"#,
    )
    .expect("valid pattern")
});

static RASTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^data:image/(?:png|jpeg|gif|webp);base64,[a-z0-9+/]+={0,2}$")
        .expect("valid pattern")
});

fn js_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// JavaScript's `\s`.
fn js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// `communityIcon`: an HTTPS URL without credentials, a Buzz emoji avatar
/// rebuilt from its template, or a small inline PNG, JPEG, GIF or WebP.
pub fn community_icon(value: &str) -> Option<String> {
    if js_len(value) <= MAX_URL
        && value
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
        && !value.chars().any(js_space)
        && let Ok(url) = Url::parse(value)
        && url.scheme() == "https"
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
    {
        return Some(value.to_string());
    }
    if js_len(value) <= MAX_INLINE
        && let Some(encoded) = value.strip_prefix(SVG_PREFIX)
    {
        let svg = percent_decode_str(encoded).decode_utf8().ok()?;
        let found = EMOJI.captures(&svg)?;
        let (radius, color, text) = (&found[1], &found[2], &found[3]);
        // `{1,128}` repetitions, each a UTF-16 unit or an entity.
        let units: usize = text
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .encode_utf16()
            .count();
        if units > 128 {
            return None;
        }
        let rebuilt = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 512 512"><rect width="512" height="512" rx="{radius}" fill="{color}"/><text x="50%" y="56%" dominant-baseline="middle" text-anchor="middle" font-size="258">{text}</text></svg>"#
        );
        return Some(format!(
            "{SVG_PREFIX}{}",
            utf8_percent_encode(&rebuilt, COMPONENT)
        ));
    }
    (js_len(value) <= MAX_INLINE && RASTER.is_match(value)).then(|| value.to_string())
}

/// The bytes of an inline icon from [`community_icon`], to render without
/// a fetch. `None` for an HTTPS icon.
pub fn inline_bytes(icon: &str) -> Option<Vec<u8>> {
    if let Some(encoded) = icon.strip_prefix(SVG_PREFIX) {
        return Some(percent_decode_str(encoded).collect());
    }
    let (_, data) = icon.strip_prefix("data:")?.split_once(";base64,")?;
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn emoji(text: &str) -> String {
        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 512 512"><rect width="512" height="512" rx="112" fill="#aabbcc"/><text x="50%" y="56%" dominant-baseline="middle" text-anchor="middle" font-size="258">{text}</text></svg>"##
        );
        format!("{SVG_PREFIX}{}", utf8_percent_encode(&svg, COMPONENT))
    }

    #[test]
    fn https_icons_without_credentials_pass() {
        let icon = "https://relay.example/icon.png?size=64";
        assert_eq!(community_icon(icon).as_deref(), Some(icon));
        assert!(community_icon("HTTPS://relay.example/a.png").is_some());
        assert_eq!(community_icon("http://relay.example/a.png"), None);
        assert_eq!(community_icon("https://user:pw@relay.example/a.png"), None);
        assert_eq!(community_icon("https://relay.example/a b.png"), None);
        assert_eq!(community_icon("javascript:alert(1)"), None);
        let long = format!("https://relay.example/{}", "a".repeat(2048));
        assert_eq!(community_icon(&long), None);
    }

    #[test]
    fn emoji_avatars_are_rebuilt_from_the_template() {
        let icon = emoji("🐝");
        assert_eq!(community_icon(&icon).as_deref(), Some(icon.as_str()));
        assert!(community_icon(&emoji("a &amp; b")).is_some());
        assert_eq!(community_icon(&emoji("<script>")), None);
        assert_eq!(community_icon(&emoji(&"a".repeat(129))), None);
        let script = format!(
            "{SVG_PREFIX}{}",
            utf8_percent_encode("<svg><script>alert(1)</script></svg>", COMPONENT)
        );
        assert_eq!(community_icon(&script), None);
        assert!(inline_bytes(&icon).unwrap().starts_with(b"<svg"));
    }

    #[test]
    fn small_raster_data_urls_pass() {
        let icon = "data:image/png;base64,iVBORw0KGgo=";
        assert_eq!(community_icon(icon).as_deref(), Some(icon));
        assert_eq!(inline_bytes(icon).unwrap()[..4], [0x89, b'P', b'N', b'G']);
        assert_eq!(community_icon("data:image/bmp;base64,AAAA"), None);
        assert_eq!(community_icon("data:text/html;base64,AAAA"), None);
        let big = format!("data:image/png;base64,{}", "A".repeat(MAX_INLINE));
        assert_eq!(community_icon(&big), None);
    }
}
