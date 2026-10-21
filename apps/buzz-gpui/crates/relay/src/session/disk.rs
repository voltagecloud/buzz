//! The actor's writes to the device store, run off the actor in the
//! order they were queued, so a head saved before a channel was revoked
//! can never land after its removal and bring the channel back.

use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce() + Send>;

/// Queues blocking store writes and runs them in order.
pub(super) struct Disk {
    jobs: mpsc::UnboundedSender<Job>,
}

impl Disk {
    pub(super) fn new(runtime: &tokio::runtime::Handle) -> Self {
        let (jobs, mut queued) = mpsc::unbounded_channel::<Job>();
        runtime.spawn(async move {
            while let Some(job) = queued.recv().await {
                let mut batch = vec![job];
                while let Ok(job) = queued.try_recv() {
                    batch.push(job);
                }
                let _ =
                    tokio::task::spawn_blocking(move || batch.into_iter().for_each(|job| job()))
                        .await;
            }
        });
        Self { jobs }
    }

    /// Runs `job` after every write queued before it.
    pub(super) fn write(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.jobs.send(Box::new(job));
    }

    /// Resolves once every write queued so far has run.
    pub(super) fn flushed(&self) -> oneshot::Receiver<()> {
        let (done, flushed) = oneshot::channel();
        self.write(move || {
            let _ = done.send(());
        });
        flushed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn writes_run_in_order() {
        let disk = Disk::new(&tokio::runtime::Handle::current());
        let log = Arc::new(Mutex::new(Vec::new()));
        for i in 0..100 {
            let log = log.clone();
            disk.write(move || log.lock().unwrap().push(i));
        }
        disk.flushed().await.unwrap();
        assert_eq!(*log.lock().unwrap(), (0..100).collect::<Vec<_>>());
    }
}
