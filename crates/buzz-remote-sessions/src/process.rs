/// Spawn `cmd`, retrying only while its executable is briefly busy.
///
/// A concurrent fork can inherit a staging writer until that child execs,
/// even after this thread closes it. Retry only that failed exec: the command
/// never started. Persistent writers still fail after 100 ms.
pub fn spawn_with_busy_retry(
    cmd: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    for _ in 0..10 {
        match cmd.spawn() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => return result,
        }
    }
    cmd.spawn()
}
