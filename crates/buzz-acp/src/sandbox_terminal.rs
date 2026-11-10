//! Local terminal process boundary. Relay authorization is applied by the
//! harness before this worker is started. The worker uses local IPC only.

#[cfg(target_os = "linux")]
mod process_tree;
#[cfg(target_os = "linux")]
mod worker;

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
