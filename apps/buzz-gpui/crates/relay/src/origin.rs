//! A community is identified by its relay origin. The same origin is reached
//! over WebSocket for live traffic and HTTPS for finite reads.

use std::fmt;
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelayOrigin {
    /// `wss://host[:port]` (or `ws://` for local development).
    websocket: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct OriginError(&'static str);

impl RelayOrigin {
    /// A community is a secure origin (`relayOrigin`): `wss://` or
    /// `https://`, without credentials, a path, a query or a fragment,
    /// which are refused rather than dropped so a typo never names another
    /// relay. The one exception is a relay on this machine (a loopback
    /// host), which may use `ws://` or `http://` for development.
    pub fn parse(input: &str) -> Result<Self, OriginError> {
        const SHAPE: &str =
            "Enter a wss:// or https:// relay URL without credentials, a path, query, or fragment.";
        let value = input.trim();
        let (scheme, rest) = value.split_once("://").ok_or(OriginError(SHAPE))?;
        let scheme = scheme.to_ascii_lowercase();
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if value.len() > 2048
            || !matches!(scheme.as_str(), "wss" | "https" | "ws" | "http")
            || authority.is_empty()
            || authority
                .chars()
                .any(|c| matches!(c, '/' | '?' | '#' | '\\' | '@') || c.is_whitespace())
        {
            return Err(OriginError(SHAPE));
        }
        let url = Url::parse(value).map_err(|_| {
            OriginError("Enter a valid relay URL, such as wss://relay.example.com.")
        })?;
        let host = url.host_str().ok_or(OriginError(
            "Enter a valid relay URL, such as wss://relay.example.com.",
        ))?;
        // DNS treats the final dot as the same host; storage must too.
        let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        let secure = matches!(scheme.as_str(), "wss" | "https");
        if !secure && !loopback(&host) {
            return Err(OriginError(SHAPE));
        }
        let scheme = if secure { "wss" } else { "ws" };
        // `port()` leaves out the scheme's default port.
        let websocket = match url.port() {
            Some(port) => format!("{scheme}://{host}:{port}"),
            None => format!("{scheme}://{host}"),
        };
        Ok(Self { websocket })
    }

    pub fn websocket(&self) -> &str {
        &self.websocket
    }

    /// The HTTP origin, without a trailing slash.
    pub fn http(&self) -> String {
        match self.websocket.strip_prefix("wss://") {
            Some(rest) => format!("https://{rest}"),
            None => format!("http://{}", &self.websocket["ws://".len()..]),
        }
    }

    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.http())
    }
}

/// A host on this machine: `localhost` or a loopback address.
fn loopback(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

impl fmt::Display for RelayOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.websocket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_and_wss_name_the_same_origin() {
        let a = RelayOrigin::parse("https://Relay.Example.com/").unwrap();
        let b = RelayOrigin::parse("wss://relay.example.com").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.http(), "https://relay.example.com");
        assert_eq!(a.endpoint("/query"), "https://relay.example.com/query");
        let local = RelayOrigin::parse("ws://127.0.0.1:53000").unwrap();
        assert_eq!(local.http(), "http://127.0.0.1:53000");
    }

    #[test]
    fn refuses_anything_but_an_origin() {
        for bad in [
            "relay.example.com",
            "ftp://relay.example.com",
            "wss://relay.example.com/path",
            "wss://relay.example.com?x=1",
            "wss://relay.example.com#top",
            "wss://user:pw@relay.example.com",
            "wss://",
        ] {
            assert!(RelayOrigin::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn plain_origins_are_for_this_machine_only() {
        assert!(RelayOrigin::parse("ws://relay.example.com").is_err());
        assert!(RelayOrigin::parse("http://10.0.0.5:53000").is_err());
        for local in [
            "ws://127.0.0.1:53000",
            "http://localhost:4000",
            "ws://[::1]:9",
        ] {
            assert!(RelayOrigin::parse(local).is_ok(), "{local}");
        }
    }

    #[test]
    fn normalizes_case_trailing_dots_and_default_ports() {
        assert_eq!(
            RelayOrigin::parse("WSS://Relay.Example.com.:443/").unwrap(),
            RelayOrigin::parse("https://relay.example.com").unwrap()
        );
        assert_eq!(
            RelayOrigin::parse("wss://relay.example.com:8443")
                .unwrap()
                .websocket(),
            "wss://relay.example.com:8443"
        );
    }
}
