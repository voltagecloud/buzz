//! Local terminal process boundary. Relay authorization is applied by the
//! harness before this worker is started. The worker uses local IPC only.

#[cfg(target_os = "linux")]
mod authority;
#[cfg(target_os = "linux")]
mod client;
#[cfg(target_os = "linux")]
mod journal;
#[cfg(target_os = "linux")]
mod process_tree;
#[cfg(target_os = "linux")]
mod server;
#[cfg(target_os = "linux")]
mod snapshot_worker;
#[cfg(target_os = "linux")]
mod worker;

/// Start terminal service only for a Linux owner-gated, immutable channel thread.
/// Unsupported transports and scopes never subscribe to terminal requests.
pub(crate) fn spawn(
    config: &crate::config::Config,
    rest: crate::relay::RestClient,
    shutdown: tokio::sync::watch::Receiver<()>,
) -> anyhow::Result<Option<tokio::task::JoinHandle<anyhow::Result<()>>>> {
    #[cfg(target_os = "linux")]
    {
        Ok(server::Config::from_harness(config)?
            .map(|config| tokio::spawn(server::serve(config, rest, shutdown))))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (config, rest, shutdown);
        Ok(None)
    }
}

/// Only loss of process containment requires stopping the whole harness.
pub(crate) fn containment_failure(error: &anyhow::Error) -> bool {
    #[cfg(target_os = "linux")]
    {
        error.is::<client::ContainmentError>()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = error;
        false
    }
}

/// Shutdown cannot abandon a worker while the harness checkpoints its workspace.
pub(crate) async fn stop(
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
) -> anyhow::Result<()> {
    let Some(mut task) = task else { return Ok(()) };
    // The maximum admission (15s), confirmed worker cleanup (15s), final audit
    // attempt (5s), and socket close (1s) fit inside this absolute bound.
    match tokio::time::timeout(std::time::Duration::from_secs(40), &mut task).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) if !containment_failure(&error) => {
            tracing::warn!("terminal service stopped with its audit journal retained");
            Ok(())
        }
        Ok(Ok(Err(error))) => Err(error),
        Ok(Err(_)) => anyhow::bail!("terminal service exited without confirming containment"),
        Err(_) => {
            task.abort();
            anyhow::bail!("terminal service shutdown could not confirm containment")
        }
    }
}

/// Dispatch the isolated worker before ordinary harness configuration or Tokio
/// threads exist. Exit 2 means containment could not be confirmed: the caller
/// must terminate the harness instead of leaving an uncontained sandbox alive.
pub(crate) fn worker_entry() -> i32 {
    #[cfg(target_os = "linux")]
    {
        worker::entry()
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("sandbox terminals require Linux process containment");
        2
    }
}

/// Run a contained snapshot producer before the harness runtime or model pool.
/// The parent supplies its fixed helper; no relay event selects executable paths.
pub(crate) fn snapshot_worker_entry() -> i32 {
    #[cfg(target_os = "linux")]
    {
        snapshot_worker::entry()
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("sandbox snapshots require Linux process containment");
        2
    }
}
