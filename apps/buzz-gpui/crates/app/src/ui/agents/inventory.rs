//! Community origins of saved agents.

/// A community's HTTP origin (`https://host`), as buzz-app's `relayOrigin`.
pub fn origin(url: &str) -> String {
    buzz_relay_client::RelayOrigin::parse(url)
        .map(|origin| origin.http())
        .unwrap_or_default()
}
