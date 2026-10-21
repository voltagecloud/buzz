//! The pairing socket: route discovery through the community's NIP-11
//! document, then a dedicated WebSocket that subscribes to the session's
//! key and answers NIP-42 challenges with the ephemeral key.

use buzz_pairing::{PairingSession, session::KIND_PAIRING};
use futures::{SinkExt, StreamExt};
use nostr_pairing::{Event, JsonUtil};
use std::time::Duration;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use url::Url;

pub(super) type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

// Choose the Rustls provider per connection, rather than relying on
// process-global feature inference, as buzz-app does. This app's other
// clients use aws-lc-rs, so pairing does too.
pub(super) fn tls_connector() -> Result<tokio_tungstenite::Connector, String> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| "Couldn’t initialize the secure pairing connection.")?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
        config,
    )))
}

pub(super) fn community(input: &str) -> Result<Url, String> {
    let mut url = Url::parse(input).map_err(|_| "Enter a secure community URL.")?;
    if input.len() > 2048
        || !matches!(url.scheme(), "https" | "wss")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(
            "Enter a https:// or wss:// community URL without a path or account details.".into(),
        );
    }
    url.set_scheme("https")
        .map_err(|_| "Enter a secure community URL.")?;
    Ok(url)
}

fn secure_pairing_url(input: &str) -> Result<Url, String> {
    let url =
        Url::parse(input).map_err(|_| "The community returned an unsupported pairing address.")?;
    if url.scheme() != "wss"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("The community returned an unsupported pairing address.".into());
    }
    Ok(url)
}

fn advertised(origin: &Url, document: &serde_json::Value) -> Result<Url, String> {
    if let Some(value) = document.get("pairing_relay_url") {
        return secure_pairing_url(
            value
                .as_str()
                .ok_or("The community returned an unsupported pairing address.")?,
        );
    }
    let mut url = origin.clone();
    url.set_scheme("wss")
        .map_err(|_| "Unsupported community address.")?;
    if document
        .get("supported_nips")
        .and_then(|v| v.as_array())
        .is_some_and(|nips| nips.iter().any(|n| n.as_u64() == Some(43)))
    {
        url.set_path("/pair");
    }
    Ok(url)
}

pub(super) async fn discover(origin: &Url) -> Result<Url, String> {
    #[cfg(test)]
    if let Some(relay) = super::end_to_end_tests::pairing_relay(origin) {
        return Ok(relay);
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|_| "Couldn’t connect to this community.")?;
    let mut response = client
        .get(origin.clone())
        .header("Accept", "application/nostr+json")
        .send()
        .await
        .map_err(|_| "Couldn’t reach this community. Check your connection and try again.")?;
    if !response.status().is_success() {
        return Err("This community couldn’t provide its pairing details. Try again.".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Couldn’t read community pairing details.")?
    {
        if bytes.len() + chunk.len() > 65536 {
            return Err("Community pairing details are too large.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let json =
        serde_json::from_slice(&bytes).map_err(|_| "Couldn’t read community pairing details.")?;
    advertised(origin, &json)
}

pub(super) async fn connect(relay: &Url, limit: Duration) -> Result<Socket, String> {
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(256 * 1024))
        .max_frame_size(Some(256 * 1024));
    tokio::time::timeout(
        limit,
        tokio_tungstenite::connect_async_tls_with_config(
            relay.as_str(),
            Some(config),
            false,
            Some(tls_connector()?),
        ),
    )
    .await
    .map_err(|_| "Pairing connection timed out. Check your connection and try again.")?
    .map(|(socket, _)| socket)
    .map_err(|_| "Couldn’t connect for pairing. Check your connection and try again.".into())
}

pub(super) async fn send(socket: &mut Socket, event: &Event) -> Result<(), String> {
    socket
        .send(Message::Text(
            format!("[\"EVENT\",{}]", event.as_json()).into(),
        ))
        .await
        .map_err(|_| "The pairing connection closed. Try again.".into())
}

// Authentication remains live after EOSE: some relays challenge the first
// publication.
#[derive(Default)]
pub(super) struct Authentication {
    id: Option<String>,
    challenges: usize,
    pub unacknowledged: Vec<Event>,
}

impl Authentication {
    pub async fn handle(
        &mut self,
        socket: &mut Socket,
        session: &PairingSession,
        relay: &Url,
        message: &serde_json::Value,
    ) -> Result<bool, String> {
        match message[0].as_str() {
            Some("AUTH") => {
                self.challenges += 1;
                if self.challenges > 3 {
                    return Err("The relay repeated its account challenge. Try again.".into());
                }
                let challenge = message[1]
                    .as_str()
                    .ok_or("Couldn’t authenticate pairing.")?;
                let event = session
                    .sign_event(nostr_pairing::EventBuilder::auth(
                        challenge,
                        nostr_pairing::RelayUrl::parse(relay.as_str())
                            .map_err(|_| "Unsupported pairing address.")?,
                    ))
                    .map_err(|_| "Couldn’t authenticate pairing.")?;
                self.id = Some(event.id.to_hex());
                socket
                    .send(Message::Text(
                        format!("[\"AUTH\",{}]", event.as_json()).into(),
                    ))
                    .await
                    .map_err(|_| "Couldn’t authenticate pairing.")?;
                Ok(true)
            }
            Some("OK")
                if self
                    .id
                    .as_deref()
                    .is_some_and(|id| Some(id) == message[1].as_str()) =>
            {
                if message[2].as_bool() != Some(true) {
                    return Err("This community didn’t accept the pairing connection.".into());
                }
                self.id = None;
                request(socket, session).await?;
                // Replay only signed events awaiting receipts. Peer event
                // IDs deduplicate them.
                for event in &self.unacknowledged {
                    send(socket, event).await?;
                }
                Ok(true)
            }
            Some("OK") => {
                if let Some(index) = self
                    .unacknowledged
                    .iter()
                    .position(|e| Some(e.id.to_hex().as_str()) == message[1].as_str())
                {
                    if message[2].as_bool() == Some(true) {
                        self.unacknowledged.remove(index);
                    } else if !message[3]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("auth-required:")
                    {
                        return Err("The community couldn’t deliver pairing. Try again.".into());
                    }
                }
                Ok(true)
            }
            Some("CLOSED") if message[1] == "pair" => {
                if !message[2]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("auth-required:")
                {
                    return Err("The community closed pairing. Try again.".into());
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

// Subscribe immediately, preserving offers arriving before the readiness
// marker.
pub(super) async fn subscribe(
    socket: &mut Socket,
    session: &PairingSession,
    relay: &Url,
) -> Result<(Vec<Event>, Authentication), String> {
    tokio::time::timeout(Duration::from_secs(15), async {
        request(socket, session).await?;
        let mut pending = Vec::new();
        let mut auth = Authentication::default();
        loop {
            let message = next(socket).await?;
            if auth.handle(socket, session, relay, &message).await? {
                continue;
            }
            match message[0].as_str() {
                Some("EOSE") if message[1] == "pair" => return Ok((pending, auth)),
                Some("EVENT") if message[1] == "pair" => {
                    if pending.len() >= 32 {
                        return Err("Too much pairing traffic. Try again.".into());
                    }
                    if let Some(event) = event(&message) {
                        pending.push(event);
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| "Pairing took too long to connect. Try again.".to_string())?
}

async fn request(socket: &mut Socket, session: &PairingSession) -> Result<(), String> {
    let request = serde_json::json!([
        "REQ",
        "pair",
        {"kinds": [KIND_PAIRING], "#p": [session.pubkey().to_hex()]}
    ]);
    socket
        .send(Message::Text(request.to_string().into()))
        .await
        .map_err(|_| "Couldn’t subscribe to pairing.".into())
}

pub(super) async fn next(socket: &mut Socket) -> Result<serde_json::Value, String> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
                    && value.is_array()
                {
                    return Ok(value);
                }
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                return Err("The pairing connection closed. Try again.".into());
            }
            Some(Ok(_)) => {}
        }
    }
}

pub(super) fn event(value: &serde_json::Value) -> Option<Event> {
    if value[0] != "EVENT" || value[1] != "pair" {
        return None;
    }
    serde_json::from_value(value[2].clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_tls_is_explicit_even_with_multiple_app_providers() {
        assert!(matches!(
            tls_connector().unwrap(),
            tokio_tungstenite::Connector::Rustls(_)
        ));
    }

    #[test]
    fn destinations_stay_secure_and_credentials_are_rejected() {
        for value in [
            "http://relay.test",
            "ws://127.0.0.1:53000",
            "https://user:secret@relay.test",
            "https://relay.test/api",
            "https://relay.test?secret=x",
        ] {
            assert!(community(value).is_err());
        }
        let origin = community("wss://relay.test").unwrap();
        assert_eq!(origin.as_str(), "https://relay.test/");
        assert_eq!(
            advertised(&origin, &serde_json::json!({"supported_nips":[43]}))
                .unwrap()
                .as_str(),
            "wss://relay.test/pair"
        );
        assert_eq!(
            advertised(&origin, &serde_json::json!({}))
                .unwrap()
                .as_str(),
            "wss://relay.test/"
        );
        assert!(
            advertised(
                &origin,
                &serde_json::json!({"pairing_relay_url":"ws://relay.test"})
            )
            .is_err()
        );
        assert_eq!(
            advertised(
                &origin,
                &serde_json::json!({"pairing_relay_url":"wss://pair.test/pair"})
            )
            .unwrap()
            .as_str(),
            "wss://pair.test/pair"
        );
    }
}
