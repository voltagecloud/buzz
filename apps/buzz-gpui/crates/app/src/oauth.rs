//! Browser sign-in through a loopback OAuth callback (buzz-app's
//! `oauth_callback_begin` / `wait` / `cancel` commands).
//!
//! buzz-desktop owns the listener: a 127.0.0.1 socket on an ephemeral port,
//! the injected `redirect_uri`, a 256-bit single-use `state`, the
//! "Sign-in callback" page with strict headers and the ten-minute timeout.
//! This module gives it the app's runtime and browser. Features that sign
//! in with a provider (`begin`, then `wait`, `cancel` on abort) call it;
//! they own the code exchange and the credentials. In buzz-app those are
//! Builderlab and Hosted communities, which this port skips, so nothing
//! calls it yet.
#![allow(dead_code)]

use crate::runtime::Runtime;
use buzz_desktop::oauth_callback::OAuthCallbackHost;
pub use buzz_desktop::oauth_callback::{OAuthAttempt, OAuthCallback, OAuthRequest};
use gpui_kit::{App, AsyncApp, Global};
use std::sync::Arc;

/// The single pending attempt. Callers serialize their own begin, wait and
/// cancel sequence, as in buzz-app.
#[derive(Clone, Default)]
pub struct SignIn(Arc<OAuthCallbackHost>);

impl Global for SignIn {}

impl SignIn {
    /// Binds the listener and opens the authorization URL in the browser.
    /// Returns the attempt and the exact `redirect_uri` to reuse in the
    /// token exchange.
    pub fn begin(request: OAuthRequest, cx: &mut App) -> Result<OAuthAttempt, String> {
        let host = cx.default_global::<Self>().0.clone();
        let runtime = Runtime::handle(cx);
        let _runtime = runtime.enter();
        host.begin(request, |url| {
            cx.open_url(url);
            Ok(())
        })
    }

    /// Waits for the attempt's callback; each result is taken once. Provider
    /// errors come back as parameters; a timeout or cancellation fails.
    pub async fn wait(id: String, cx: &mut AsyncApp) -> Result<OAuthCallback, String> {
        let (host, runtime) =
            cx.update(|cx| (cx.default_global::<Self>().0.clone(), Runtime::handle(cx)));
        runtime
            .spawn(async move { host.wait(&id).await })
            .await
            .unwrap_or_else(|_| Err("Sign-in canceled.".into()))
    }

    /// Ends the attempt if it is still current. Safe to repeat.
    pub fn cancel(id: &str, cx: &mut App) -> Result<(), String> {
        cx.default_global::<Self>().0.cancel(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn get(url: &str) -> String {
        let url = url::Url::parse(url).unwrap();
        let authority = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
        let mut stream = tokio::net::TcpStream::connect(&authority).await.unwrap();
        let target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    /// The flow the app runs: the browser gets the provider URL with the
    /// loopback `redirect_uri` and a state, and the provider's redirect
    /// back with that state completes the attempt.
    #[tokio::test]
    async fn loopback_callback_round_trip() {
        let host = OAuthCallbackHost::default();
        let mut opened = String::new();
        let attempt = host
            .begin(
                OAuthRequest {
                    authorization_url: "https://provider.example/authorize?client_id=buzz".into(),
                    callback_path: "/oauth2redirect/test".into(),
                    use_state: None,
                },
                |url| {
                    opened = url.to_string();
                    Ok(())
                },
            )
            .unwrap();
        assert!(attempt.callback_url.starts_with("http://127.0.0.1:"));
        assert!(attempt.callback_url.ends_with("/oauth2redirect/test"));
        let opened = url::Url::parse(&opened).unwrap();
        let pairs: Vec<_> = opened.query_pairs().into_owned().collect();
        assert!(pairs.contains(&("redirect_uri".into(), attempt.callback_url.clone())));
        let state = pairs
            .iter()
            .find(|(name, _)| name == "state")
            .map(|(_, value)| value.clone())
            .unwrap();
        // 256 random bits, URL-safe base64 without padding.
        assert_eq!(state.len(), 43);

        // A forged callback without the state leaves the attempt pending.
        let forged = get(&format!("{}?code=x", attempt.callback_url)).await;
        assert!(forged.starts_with("HTTP/1.1 400"));

        let response = get(&format!("{}?code=abc&state={state}", attempt.callback_url)).await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("<title>Sign-in callback</title>"));
        assert!(response.contains("Content-Security-Policy: default-src 'none'"));
        assert!(response.contains("Cache-Control: no-store"));
        let callback = host.wait(&attempt.id).await.unwrap();
        assert!(
            callback
                .parameters
                .contains(&("code".to_string(), "abc".to_string()))
        );
        // The result is taken once.
        assert!(host.wait(&attempt.id).await.is_err());
    }

    #[tokio::test]
    async fn cancel_ends_only_the_current_attempt() {
        let host = OAuthCallbackHost::default();
        let request = || OAuthRequest {
            authorization_url: "https://provider.example/authorize".into(),
            callback_path: "/cb".into(),
            use_state: None,
        };
        let first = host.begin(request(), |_| Ok(())).unwrap();
        assert!(host.begin(request(), |_| Ok(())).is_err());
        host.cancel(&first.id).unwrap();
        let second = host.begin(request(), |_| Ok(())).unwrap();
        // A stale id does not cancel the newer attempt.
        host.cancel(&first.id).unwrap();
        assert!(host.begin(request(), |_| Ok(())).is_err());
        host.cancel(&second.id).unwrap();
        // A browser that cannot open cleans up.
        assert!(host.begin(request(), |_| Err("no browser".into())).is_err());
        assert!(host.begin(request(), |_| Ok(())).is_ok());
    }
}
