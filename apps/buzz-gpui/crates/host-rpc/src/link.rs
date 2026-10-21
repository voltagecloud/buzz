//! What both ends of a connection do alike once it is up: send requests,
//! write queued frames, and read frames from a newer peer.

use crate::frame::{self, Frame, Unknown};
use crate::{Protocol, RpcError};
use bytes::Bytes;
use futures::SinkExt;
use futures::stream::SplitSink;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};

/// Requests sent and not yet answered, by id.
pub(crate) type Pending<R> = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<R, RpcError>>>>>;

/// Queues `frame`, the request `id`, and waits up to `timeout` for its
/// answer. Fails as soon as the connection closes, since nothing can
/// answer it then.
pub(crate) async fn request<P: Protocol, R>(
    outgoing: &mpsc::Sender<Frame<P>>,
    pending: &Pending<R>,
    id: u64,
    frame: Frame<P>,
    timeout: Duration,
) -> Result<R, RpcError> {
    let (sender, receiver) = oneshot::channel();
    pending.lock().await.insert(id, sender);
    if outgoing.send(frame).await.is_err() {
        pending.lock().await.remove(&id);
        return Err(RpcError::Disconnected);
    }
    let failure = tokio::select! {
        biased;
        answer = tokio::time::timeout(timeout, receiver) => match answer {
            Ok(Ok(result)) => return result,
            Ok(Err(_)) => RpcError::Disconnected,
            Err(_) => RpcError::TimedOut,
        },
        () = outgoing.closed() => RpcError::Disconnected,
    };
    pending.lock().await.remove(&id);
    Err(failure)
}

/// Writes queued frames until the queue closes or the connection fails.
/// A frame too large to send fails only itself, not the connection:
/// `pending` holds this side's requests, to fail one of those.
pub(crate) async fn write<P: Protocol, R>(
    mut sink: SplitSink<frame::Wire, Bytes>,
    mut queue: mpsc::Receiver<Frame<P>>,
    pending: Pending<R>,
) -> io::Result<()> {
    while let Some(frame) = queue.recv().await {
        let bytes = match frame::encode(&frame) {
            Ok(bytes) if bytes.len() <= frame::MAX_FRAME => bytes,
            _ => match unsendable(frame, &pending).await {
                Some(frame) => frame::encode(&frame)?,
                None => continue,
            },
        };
        sink.send(bytes).await?;
    }
    Ok(())
}

/// What to send instead of a frame that cannot be sent: an error for an
/// answer, and a resync for an event. A request fails where it waits.
async fn unsendable<P: Protocol, R>(frame: Frame<P>, pending: &Pending<R>) -> Option<Frame<P>> {
    let error = || RpcError::Failed("The message is too large to send".into());
    match frame {
        Frame::ClientResponse { id, .. } => Some(Frame::ClientResponse {
            id,
            body: Err(error()),
        }),
        Frame::HostResponse { id, .. } => Some(Frame::HostResponse {
            id,
            body: Err(error()),
        }),
        Frame::ClientRequest { id, .. } | Frame::HostRequest { id, .. } => {
            if let Some(sender) = pending.lock().await.remove(&id) {
                let _ = sender.send(Err(error()));
            }
            None
        }
        Frame::Event { seq, .. } => {
            tracing::warn!(seq, "event too large to send");
            Some(Frame::Resync)
        }
        _ => None,
    }
}

/// Decodes a frame. One from a newer peer that does not decode is answered
/// with `unsupported(id)` if it is a request, and skipped if it is an
/// event; both give `None`.
pub(crate) async fn read<P: Protocol>(
    bytes: &[u8],
    outgoing: &mpsc::Sender<Frame<P>>,
    unsupported: fn(u64) -> Frame<P>,
) -> io::Result<Option<Frame<P>>> {
    let error = match frame::decode::<Frame<P>>(bytes) {
        Ok(frame) => return Ok(Some(frame)),
        Err(error) => error,
    };
    match frame::peek(bytes) {
        Some(Unknown::Request(id)) => match outgoing.send(unsupported(id)).await {
            Ok(()) => Ok(None),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the connection closed",
            )),
        },
        Some(Unknown::Event) => Ok(None),
        None => Err(error),
    }
}
