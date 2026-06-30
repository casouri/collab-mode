//! Utility for Managing cancellation and shutdown [`CancelManager`]
//! is just a cancellation token (used to cancel tasks) plus a task
//! tracker (used to wait on canceled tasks). The main server and the
//! signaling and web channel all use the root cancel manager, the
//! actors spawned by signaling and web channel uses child cancel
//! manager so they can be separatedly cancelled.

use std::future::Future;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};
use tokio_util::task::TaskTracker;

/// Cloneable handle around a scoped [`CancellationToken`] and a
/// process-wide [`TaskTracker`].
///
/// You can create a children of the cancel manager, which carries a
/// child cancellation token, but the tracker stays the same. So
/// only root manager can close_and_wait on the tracker.
#[derive(Clone)]
pub struct CancelManager {
    cancel: CancellationToken,
    tracker: TaskTracker,
    is_root: bool,
}

impl CancelManager {
    /// Construct the root cancel manager.
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            tracker: TaskTracker::new(),
            is_root: true,
        }
    }

    /// Create a child. Cancelling the parent cancels the child;
    /// cancelling the child doesn’t cancel the parent. The tracker is
    /// shared so child can’t close.
    pub fn child(&self) -> Self {
        Self {
            cancel: self.cancel.child_token(),
            tracker: self.tracker.clone(),
            is_root: false,
        }
    }

    /// Cancel this level and all children.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Await cancellation of this scope.
    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancel.cancelled()
    }

    /// Clone a cancellation token out.
    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Spawn a task that is dropped (cancelled) when this cancel
    /// manager is cancelled, and is registered with the shared
    /// tracker.
    pub fn spawn<F>(&self, fut: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let cancel = self.cancel.clone();
        self.tracker.spawn(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    tracing::debug!("cancel_manager: task cancelled");
                }
                _ = fut => {}
            }
        })
    }

    /// Spawn a tracked task that does *not* respond to cancellation.
    pub fn spawn_uncancelled<F>(&self, fut: F) -> JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.tracker.spawn(fut)
    }

    /// Root-only. Cancel, close the shared tracker, and wait up to
    /// `timeout` for every tracked task to finish.
    ///
    /// # Panics
    ///
    /// Panics if called on a non-root manager. Closing the tracker
    /// from a child would shut it down for the parent and every other
    /// child too, which is always a bug.
    pub async fn close_and_wait(
        &self,
        timeout: Duration,
    ) -> Result<(), tokio::time::error::Elapsed> {
        assert!(
            self.is_root,
            "CancelManager::close_and_wait may only be called on the root manager",
        );
        self.cancel.cancel();
        self.tracker.close();
        tokio::time::timeout(timeout, self.tracker.wait()).await
    }
}

impl Default for CancelManager {
    fn default() -> Self {
        Self::new()
    }
}
