//! `ChannelBackend`: a minimal apalis 1.0 backend whose task source is a
//! tokio unbounded channel.
//!
//! Why not `MemoryStorage`? Its receiver is boxed inside a `Mutex` and the
//! storage is meant to be owned outright by one worker; we need the *sender*
//! side held by the enqueue-pump service (and by JNI callers on Android)
//! while the worker owns the receiving end. `UnboundedReceiver::poll_recv`
//! maps 1:1 onto the `Backend::poll_next` contract, including `None`
//! meaning "exhausted".

use apalis_core::backend::{Backend, BackendConfig};
use apalis_core::backend::finalize::Ephemeral;
use apalis_core::layers::Identity;
use apalis_core::task::Task;
use apalis_core::task::task_id::RandomId;
use std::task::{Context, Poll};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Error type carried through the backend contract.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("task channel closed")]
    Closed,
}

/// Cloneable sender half. Held by the pump service, not by the worker.
#[derive(Clone)]
pub struct ChannelHandle<Args> {
    sender: UnboundedSender<Task<Args>>,
}

impl<Args> std::fmt::Debug for ChannelHandle<Args> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelHandle").finish()
    }
}

impl<Args> ChannelHandle<Args> {
    /// Queue one fully built task; fails only if the worker is gone.
    pub fn send(&self, task: Task<Args>) -> Result<(), ChannelError> {
        self.sender.send(task).map_err(|_| ChannelError::Closed)
    }

    /// Whether the receiving worker still exists.
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

/// The backend instance owned by the worker.
pub struct ChannelBackend<Args> {
    receiver: UnboundedReceiver<Task<Args>>,
}

impl<Args> std::fmt::Debug for ChannelBackend<Args> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelBackend").finish()
    }
}

impl<Args> ChannelBackend<Args> {
    /// Pair a backend with the handle used to feed it.
    pub fn channel() -> (Self, ChannelHandle<Args>) {
        let (sender, receiver) = unbounded_channel();
        (Self { receiver }, ChannelHandle { sender })
    }
}

impl<Args> Backend for ChannelBackend<Args>
where
    Args: Send + 'static,
{
    type Task = Task<Args>;
    type Error = ChannelError;

    fn poll_ready(
        &mut self,
        _cx: &mut Context<'_>,
        _worker: &apalis_core::worker::context::WorkerContext,
    ) -> Poll<Result<(), Self::Error>> {
        // An unbounded channel never applies backpressure; always ready.
        Poll::Ready(Ok(()))
    }

    fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
        _worker: &apalis_core::worker::context::WorkerContext,
    ) -> Poll<Option<Result<Self::Task, Self::Error>>> {
        match self.receiver.poll_recv(cx) {
            Poll::Ready(Some(task)) => Poll::Ready(Some(Ok(task))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_close(
        &mut self,
        _cx: &mut Context<'_>,
        _worker: &apalis_core::worker::context::WorkerContext,
    ) -> Poll<Result<(), Self::Error>> {
        self.receiver.close();
        Poll::Ready(Ok(()))
    }
}

impl<Args> BackendConfig for ChannelBackend<Args>
where
    Args: Send + 'static,
{
    type Id = RandomId;
    type Args = Args;
    // Must be apalis' own marker: satisfies the FinalizeBackend bound for
    // non-durable backends (the SQLite ledger provides the durability).
    type Kind = Ephemeral;
    type Config = ();
    type Layer = Identity;

    fn config(&self) -> &Self::Config {
        &()
    }

    fn middleware(&mut self, _worker: &mut apalis_core::worker::context::WorkerContext) -> Self::Layer {
        Identity::new()
    }
}
