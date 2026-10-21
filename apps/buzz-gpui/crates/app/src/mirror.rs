//! Bridges tokio `watch` channels into GPUI entities.

use crate::runtime::Runtime;
use futures::StreamExt;
use gpui_kit::{Context, Task};
use tokio::sync::watch;

/// Calls `apply` on the entity with every new value of `receiver` (the
/// current value first), notifying observers. Stops when the entity or the
/// sender goes away.
pub fn mirror<T, E>(
    mut receiver: watch::Receiver<T>,
    cx: &mut Context<E>,
    apply: impl Fn(&mut E, T) + 'static,
) -> Task<()>
where
    T: Clone + Send + Sync + 'static,
    E: 'static,
{
    let (sender, mut values) = futures::channel::mpsc::unbounded();
    let _ = sender.unbounded_send(receiver.borrow_and_update().clone());
    Runtime::handle(cx).spawn(async move {
        while receiver.changed().await.is_ok() {
            let value = receiver.borrow_and_update().clone();
            if sender.unbounded_send(value).is_err() {
                return;
            }
        }
    });
    cx.spawn(async move |this, cx| {
        while let Some(mut value) = values.next().await {
            // Only the newest pending value matters.
            while let Ok(newer) = values.try_recv() {
                value = newer;
            }
            let updated = this.update(cx, |this, cx| {
                apply(this, value);
                cx.notify();
            });
            if updated.is_err() {
                return;
            }
        }
    })
}
