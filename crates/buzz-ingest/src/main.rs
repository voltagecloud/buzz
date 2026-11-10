//! Verified integration webhooks; the relay remains the only durable store.

mod config;
mod http;
mod normalize;
mod plan;
mod relay;
#[cfg(test)]
mod tests;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::var("BUZZ_INGEST_CONFIG")
        .map_err(|_| anyhow::anyhow!("BUZZ_INGEST_CONFIG is required"))?;
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(256 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 256 * 1024 {
        anyhow::bail!("configuration exceeds 256 KiB");
    }
    let config: config::Config = serde_json::from_slice(&bytes)?;
    config.validate()?;
    let secrets = config::Secrets::from_env(&config)?;
    let raw = std::env::var("BUZZ_INGEST_PRIVATE_KEY")
        .map_err(|_| anyhow::anyhow!("BUZZ_INGEST_PRIVATE_KEY is required"))?;
    let keys =
        nostr::Keys::parse(&raw).map_err(|_| anyhow::anyhow!("invalid BUZZ_INGEST_PRIVATE_KEY"))?;
    let relay = relay::Relay::new(&config.relay_url, keys)?;
    let addr: std::net::SocketAddr = std::env::var("BUZZ_INGEST_BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:5010".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let stop = tokio_util::sync::CancellationToken::new();
    let shutdown = stop.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, http::router(config, secrets, relay))
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
    });
    tokio::select! {
        result=&mut server=>result??,
        result=shutdown_signal()=>{
            result?;
            stop.cancel();
            tokio::time::timeout(std::time::Duration::from_secs(5),server).await???;
        }
    }
    Ok(())
}

/// Kubernetes sends SIGTERM; local development also supports Ctrl-C.
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result=tokio::signal::ctrl_c()=>result,
            received=terminate.recv()=>received.ok_or_else(||std::io::Error::other("SIGTERM stream closed")),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
