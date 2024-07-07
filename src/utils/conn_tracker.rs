use std::future::Future;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;


/// An abstraction struct to pass tracker and cancellation token.
/// Since tokens cannot be uncancelled, this cannot be reopened after closing.
#[derive(Clone)]
pub(crate) struct ConnTracker {
    task_tracker: TaskTracker,
    cancellation_token: CancellationToken,
}

impl ConnTracker {
    pub(crate) fn new() -> Self {
        Self {
            task_tracker: TaskTracker::new(),
            cancellation_token: CancellationToken::new()
        }
    }

    pub(crate) fn child(&self) -> Self {
        Self {
            task_tracker: self.task_tracker.clone(),
            cancellation_token: self.cancellation_token.child_token(),
        }
    }

    pub(crate) fn clone_child_tracker(&self) -> TaskTracker {
        self.task_tracker.clone()
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.cancellation_token.is_cancelled() && self.task_tracker.is_closed()
    }


    pub(crate) fn spawn<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.task_tracker.spawn(task)
    }
}