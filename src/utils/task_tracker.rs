use std::fmt::{Display, Formatter, write};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::atomic::Ordering::SeqCst;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;
use tokio::select;
use tokio::sync::mpsc::{Receiver, Sender, channel, WeakSender, UnboundedSender, UnboundedReceiver, unbounded_channel, WeakUnboundedSender};
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};
use crate::utils::task_tracker::TrackerInner::Parent;

#[derive(Copy, Clone)]
enum TrackingSignal {
    TaskStarted,
    TaskStopped
}

impl Display for TrackingSignal {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TrackingSignal::TaskStarted => write!(f, "start"),
            TrackingSignal::TaskStopped => write!(f, "stop"),
        }
    }
}

#[derive(Clone)]
enum TrackerInner {
    Parent(WeakTrackingSignalSender),
    Child(TrackingSignalSender)
}

#[derive(Clone)]
enum TrackingSignalSender {
    Bounded(Sender<TrackingSignal>),
    Unbounded(UnboundedSender<TrackingSignal>)
}

impl TrackingSignalSender {
    async fn send(&self, tracking_signal: TrackingSignal) -> Result<(), TrackingSignal> {
        if let Err(e) = match self {
            Self::Bounded(s) => s.send(tracking_signal).await,
            Self::Unbounded(s) => s.send(tracking_signal)
        } {
            warn!("couldn't send {} signal because the buffer is closed!", e.0);
            return Err(e.0)
        };
        Ok(())
    }

    fn try_send(&self, tracking_signal: TrackingSignal) -> Result<(), TrackingSignal> {
        if let Err(signal) = match self {
            Self::Bounded(s) => {
                s.try_send(tracking_signal).map_err(|e| {
                    match e {
                        TrySendError::Full(s) => {
                            warn!("couldn't send {} signal because the buffer is full!", s);
                            s
                        }
                        TrySendError::Closed(s) => s
                    }
                })
            }
            Self::Unbounded(s) => s.send(tracking_signal).map_err(|e| e.0),
        } {
            warn!("couldn't send {} signal because the buffer is closed!", signal);
            return Err(signal)
        };
        Ok(())
    }

    fn downgrade(&self) -> WeakTrackingSignalSender {
        match self {
            Self::Bounded(s) => WeakTrackingSignalSender::Bounded(s.downgrade()),
            Self::Unbounded(s) => WeakTrackingSignalSender::Unbounded(s.downgrade())
        }
    }
}

#[derive(Clone)]
enum WeakTrackingSignalSender {
    Bounded(WeakSender<TrackingSignal>),
    Unbounded(WeakUnboundedSender<TrackingSignal>)
}

impl WeakTrackingSignalSender {
    fn upgrade(&self) -> Option<TrackingSignalSender> {
        match self {
            Self::Bounded(s) => s.upgrade().map(|ss| {
                TrackingSignalSender::Bounded(ss)
            }),
            Self::Unbounded(s) => s.upgrade().map(|ss| {
                TrackingSignalSender::Unbounded(ss)
            })
        }
    }
}

enum TrackingSignalReceiver {
    Bounded(Receiver<TrackingSignal>),
    Unbounded(UnboundedReceiver<TrackingSignal>)
}

impl TrackingSignalReceiver {
    async fn recv(&mut self) -> Option<TrackingSignal> {
        match self {
            Self::Bounded(r) => r.recv().await,
            Self::Unbounded(r) => r.recv().await
        }
    }

    fn try_recv(&mut self) -> Result<TrackingSignal, tokio::sync::mpsc::error::TryRecvError> {
        match self {
            Self::Bounded(r) => r.try_recv(),
            Self::Unbounded(r) => r.try_recv()
        }
    }
}

#[derive(Copy, Clone)]
pub(crate) struct TrackerSettings {
    max_conn_time: Option<Duration>,
    max_conns: Option<usize>,
}

impl Default for TrackerSettings {
    fn default() -> Self {
        Self {
            max_conn_time: None,
            max_conns: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct TaskTracker {
    inner: TrackerInner,
    settings: TrackerSettings,
    cancellation_token: CancellationToken,
    local_task: bool,
    task_counter: Arc<AtomicUsize>,
}

pub(crate) struct TaskWaiter {
    settings: TrackerSettings,
    cancellation_token: CancellationToken,
    signal_receiver: TrackingSignalReceiver,
    signal_sender: TrackingSignalSender, // Should not be used but keeping it so it doesn't close
    task_counter: Arc<AtomicUsize>,
}

impl TaskWaiter {
    pub(crate) fn new(buffer: Option<usize>) -> Self {
        let (signal_sender, signal_receiver) = tracking_signal_channel(buffer);
        let task_waiter = Self {
            settings: Default::default(),
            cancellation_token: Default::default(),
            signal_receiver,
            signal_sender: signal_sender.clone(),
            task_counter: Arc::new(Default::default()),
        };
        task_waiter
    }

    pub(crate) fn create_tracker(&self) -> TaskTracker {
        TaskTracker {
            inner: Parent(self.signal_sender.downgrade()),
            settings: self.settings,
            cancellation_token: self.cancellation_token.child_token(),
            local_task: false,
            task_counter: self.task_counter.clone(),
        }
    }

    pub(crate) fn create_local_tracker(&self) -> TaskTracker {
        let mut task_tracker = self.create_tracker();
        task_tracker.local_task = true;
        task_tracker
    }
}

impl Drop for TaskTracker {
    fn drop(&mut self) {
        if let TrackerInner::Child(sender) = &self.inner {
            // Subtract before dropping
            self.task_counter.fetch_sub(1, SeqCst);
            if let Err(ts) = sender.try_send(TrackingSignal::TaskStopped) {
                warn!("failed to send {} signal whilst dropping the tracker", ts);
            }
            self.cancellation_token.cancel();
            warn!("dropped")
        }
    }
}

impl TaskTracker {
    pub(crate) fn new(buffer: Option<usize>) -> (Self, TaskWaiter) {
        let (signal_sender, signal_receiver) = tracking_signal_channel(buffer);
        let task_tracker = Self {
            inner: TrackerInner::Parent(signal_sender.downgrade()),
            settings: Default::default(),
            cancellation_token: CancellationToken::new(),
            local_task: false,
            task_counter: Arc::new(Default::default()),
        };
        let task_waiter = TaskWaiter {
            settings: task_tracker.settings,
            cancellation_token: Default::default(),
            signal_receiver,
            signal_sender: signal_sender.clone(),
            task_counter: Arc::new(Default::default()),
        };
        (task_tracker, task_waiter)
    }

    pub(crate) fn new_local(buffer: Option<usize>) -> (Self, TaskWaiter) {
        let (mut task_tracker, task_waiter) = Self::new(buffer);
        task_tracker.local_task = false;
        (task_tracker, task_waiter)
    }

    pub(crate) fn spawn<Fut, T>(&self, future: Fut) -> Option<JoinHandle<Option<T>>>
        where
            Fut: Future<Output = T> + Send + 'static,
            T: Send + 'static
    {
        self.spawn_with_tracker(|_tracker| async move  {
            future.await
        })
    }

    pub(crate) fn spawn_with_counter<Fut, T>(&self, future: Fut, counter: Arc<AtomicUsize>) -> Option<JoinHandle<Option<T>>>
    where
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static
    {
        self.spawn_with_tracker(|_tracker| async move  {
            counter.fetch_add(1, SeqCst);
            let res = future.await;
            counter.fetch_sub(1, SeqCst);
            res
        })
    }

    pub(crate) fn spawn_with_tracker<F, Fut, T>(&self, future_with_tracker: F) -> Option<JoinHandle<Option<T>>>
        where
            F: FnOnce(TaskTracker) -> Fut,
            F: Send + 'static,
            Fut: Future<Output = T> + Send + 'static,
            T: Send + 'static
    {
        let Some(tracker) = self.child() else {
            warn!("failed to create a new tracker child");
            return None;
        };
        let future = async move {
            let out = select! {
                biased;
                _ = cancel_handler(tracker.cancellation_token.clone(), tracker.task_counter.clone(), tracker.settings) => {
                    None
                },
                out = future_with_tracker(tracker) => Some(out)
            };
            out
        };
        let handle = if self.local_task {
            tokio::task::spawn_local(future)
        } else {
            tokio::task::spawn(future)
        };
        Some(handle)
    }

    pub(crate) fn current_task_count(&self) -> usize {
        self.task_counter.load(SeqCst)
    }

    fn child(&self) -> Option<Self> {
        self.task_counter.fetch_add(1, SeqCst);
        let inner_sender = match &self.inner {
            TrackerInner::Parent(s) =>  match s.upgrade() {
                None => {
                    return None
                }
                Some(s) => s
            }
            TrackerInner::Child(s) => s.clone()
        };
        Some(Self {
            inner: TrackerInner::Child(inner_sender),
            settings: self.settings,
            cancellation_token: self.cancellation_token.child_token(),
            local_task: self.local_task,
            task_counter: self.task_counter.clone(),
        })
    }
}

async fn cancel_handler(cancellation_token: CancellationToken, counter: Arc<AtomicUsize>, settings: TrackerSettings) {
    if let Some(max_conn_time) = settings.max_conn_time {
        select! {
            biased;
            _ = cancellation_token.cancelled() => {},
        }
    } else {
        cancellation_token.cancelled().await;
    }
}

fn tracking_signal_channel(buffer: Option<usize>) -> (TrackingSignalSender, TrackingSignalReceiver) {
    match buffer {
        Some(buffer) => {
            let (sender, receiver) = channel(buffer);
            (TrackingSignalSender::Bounded(sender), TrackingSignalReceiver::Bounded(receiver))
        }
        None => {
            let (sender, receiver) = unbounded_channel();
            (TrackingSignalSender::Unbounded(sender), TrackingSignalReceiver::Unbounded(receiver))
        }
    }
}