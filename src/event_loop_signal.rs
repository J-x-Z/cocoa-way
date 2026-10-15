//! An mpsc sender that wakes the event loop after queueing a message.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use winit::event_loop::EventLoopProxy;

/// Worker admission includes all pending messages, not only that worker's clones.
pub const MAX_PENDING_WORKER_MESSAGES: usize = 256;

#[derive(Debug, PartialEq, Eq)]
pub enum SendTimeoutError<T> {
    Timeout(T),
    Disconnected(T),
}

/// Clones share the message queue and waker without requiring `T: Clone`.
pub struct Sender<T> {
    sender: mpsc::Sender<T>,
    wake: Arc<dyn Fn() + Send + Sync>,
    pending: Arc<PendingWake>,
    ui_thread: Option<ThreadId>,
}

/// Owns receiver shutdown notification. Consumption still requires one
/// `Sender::message_received` call per message, as with the legacy raw receiver.
pub struct Receiver<T> {
    receiver: mpsc::Receiver<T>,
    pending: Arc<PendingWake>,
}

pub fn channel<T>(proxy: EventLoopProxy<()>) -> (Sender<T>, Receiver<T>) {
    channel_with_waker(move || {
        let _ = proxy.send_event(());
    })
}

/// Construct on the UI thread: worker `send`s backpressure at 256 pending
/// messages, while UI sends remain lossless and nonblocking. The UI exemption
/// means this is a worker-flood bound, not a hard cap on UI-generated bursts.
pub fn channel_with_waker<T>(wake: impl Fn() + Send + Sync + 'static) -> (Sender<T>, Receiver<T>) {
    let (raw, receiver) = mpsc::channel();
    let mut sender = Sender::with_waker(raw, wake);
    sender.ui_thread = Some(thread::current().id());
    let receiver = Receiver {
        receiver,
        pending: sender.pending.clone(),
    };
    (sender, receiver)
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, mpsc::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let _guard = self
            .pending
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.pending.disconnected.store(true, Ordering::SeqCst);
        self.pending.received.notify_all();
    }
}

#[derive(Default)]
struct PendingWake {
    notification: AtomicBool,
    messages: AtomicUsize,
    admission: Mutex<()>,
    received: Condvar,
    disconnected: AtomicBool,
}

impl<T> Sender<T> {
    /// Wraps an existing channel sender with the event loop's wake proxy.
    pub fn new(sender: mpsc::Sender<T>, proxy: EventLoopProxy<()>) -> Self {
        Self::with_waker(sender, move || {
            // A queued message cannot be recovered if the loop has already closed.
            let _ = proxy.send_event(());
        })
    }

    /// Injects a thread-safe waker, shared by all clones of this sender.
    ///
    /// The callback must actively wake the consumer and should not panic.
    pub fn with_waker(sender: mpsc::Sender<T>, wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            sender,
            wake: Arc::new(wake),
            pending: Arc::new(PendingWake::default()),
            ui_thread: None,
        }
    }

    /// Lossless, nonblocking UI/event-loop lane. On a managed `channel`, calls
    /// from background threads wait for bounded admission or receiver shutdown.
    /// Legacy raw-channel constructors cannot observe shutdown while full and
    /// retain the unbounded behavior; migrate them to `channel` for worker bounds.
    /// Coalesces wake notifications until the native loop finishes its turn.
    ///
    /// A disconnected receiver returns the original message in `SendError<T>`
    /// without waking. Success reports channel delivery, not proxy availability.
    pub fn send(&self, message: T) -> Result<(), SendError<T>> {
        if self
            .ui_thread
            .is_some_and(|id| id != thread::current().id())
        {
            return self
                .send_admitted(message, None)
                .map_err(|error| match error {
                    SendTimeoutError::Disconnected(message) => SendError(message),
                    SendTimeoutError::Timeout(_) => unreachable!("no deadline supplied"),
                });
        }
        self.pending.messages.fetch_add(1, Ordering::SeqCst);
        self.send_reserved(message)
    }

    /// Bounded admission for background control/lifecycle producers ONLY.
    ///
    /// The event-loop consumer must keep calling `message_received`. On timeout
    /// the original message is returned: callers must retry or fail the operation
    /// explicitly, never silently discard a lifecycle/key/button message. This
    /// waits for queue capacity and must not be called on the UI thread. Receiver
    /// shutdown while full is detected no later than the admission deadline.
    pub fn send_timeout(&self, message: T, timeout: Duration) -> Result<(), SendTimeoutError<T>> {
        self.send_admitted(message, Some(timeout))
    }

    /// Nonblocking bounded admission. Use for lossy bulk diagnostics, or retain
    /// and explicitly handle the original message on `Full` for reliable work.
    pub fn try_send(&self, message: T) -> Result<(), mpsc::TrySendError<T>> {
        self.send_timeout(message, Duration::ZERO)
            .map_err(|error| match error {
                SendTimeoutError::Timeout(message) => mpsc::TrySendError::Full(message),
                SendTimeoutError::Disconnected(message) => {
                    mpsc::TrySendError::Disconnected(message)
                }
            })
    }

    /// Wait for an admission hint without consuming or owning a message. Resource
    /// producers recheck their shutdown gate atomically before trying to send.
    pub fn wait_for_capacity(&self, timeout: Duration) {
        let guard = self
            .pending
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = self
            .pending
            .received
            .wait_timeout_while(guard, timeout, |_| {
                !self.pending.disconnected.load(Ordering::SeqCst)
                    && self.pending.messages.load(Ordering::SeqCst) >= MAX_PENDING_WORKER_MESSAGES
            })
            .unwrap_or_else(|error| error.into_inner());
    }

    fn send_admitted(
        &self,
        message: T,
        timeout: Option<Duration>,
    ) -> Result<(), SendTimeoutError<T>> {
        let started = Instant::now();
        let mut guard = self
            .pending
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        loop {
            if self.pending.disconnected.load(Ordering::SeqCst) {
                return Err(SendTimeoutError::Disconnected(message));
            }
            // A deadline-aware caller must not enqueue after waking late merely
            // because capacity became available in the meantime. ZERO retains
            // the immediate-attempt semantics required by try_send.
            if timeout.is_some_and(|timeout| !timeout.is_zero() && started.elapsed() >= timeout) {
                return Err(SendTimeoutError::Timeout(message));
            }
            if self
                .pending
                .messages
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    (count < MAX_PENDING_WORKER_MESSAGES).then_some(count + 1)
                })
                .is_ok()
            {
                drop(guard);
                return self
                    .send_reserved(message)
                    .map_err(|error| SendTimeoutError::Disconnected(error.0));
            }
            guard = if let Some(timeout) = timeout {
                let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                    return Err(SendTimeoutError::Timeout(message));
                };
                self.pending
                    .received
                    .wait_timeout(guard, remaining)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            } else {
                self.pending
                    .received
                    .wait(guard)
                    .unwrap_or_else(|e| e.into_inner())
            };
        }
    }

    fn send_reserved(&self, message: T) -> Result<(), SendError<T>> {
        if let Err(error) = self.sender.send(message) {
            self.message_received();
            return Err(error);
        }
        if !self.pending.notification.swap(true, Ordering::SeqCst) {
            (self.wake)();
        }
        Ok(())
    }

    /// Call once for each message removed from the paired receiver.
    pub fn message_received(&self) {
        let _guard = self
            .pending
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.pending
            .messages
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            })
            .expect("message_received called without a pending message");
        self.pending.received.notify_one();
    }

    /// Call only at AboutToWait, after Winit has drained proxy notifications.
    /// Remaining work needs a native-turn deadline, NOT another immediate proxy event.
    pub fn finish_turn(&self) -> bool {
        self.pending.notification.store(false, Ordering::SeqCst);
        self.pending.messages.load(Ordering::SeqCst) != 0
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            wake: Arc::clone(&self.wake),
            pending: Arc::clone(&self.pending),
            ui_thread: self.ui_thread,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, PartialEq, Eq)]
    struct Message(&'static str);

    #[test]
    fn full_and_disconnected_try_sends_never_consume_unbalanced_credits() {
        let (sender, receiver) = channel_with_waker(|| {});
        for value in 0..MAX_PENDING_WORKER_MESSAGES {
            assert!(sender.try_send(value).is_ok());
        }
        // A coalescible cleanup-ready wake is intentionally dropped on Full.
        for _ in 0..1000 {
            assert_eq!(sender.try_send(999), Err(mpsc::TrySendError::Full(999)));
        }
        assert_eq!(
            sender.pending.messages.load(Ordering::SeqCst),
            MAX_PENDING_WORKER_MESSAGES
        );
        for expected in 0..MAX_PENDING_WORKER_MESSAGES {
            assert_eq!(receiver.try_recv(), Ok(expected));
            sender.message_received();
        }
        assert!(!sender.finish_turn());
        drop(receiver);
        for _ in 0..1000 {
            assert_eq!(
                sender.try_send(999),
                Err(mpsc::TrySendError::Disconnected(999))
            );
        }
        assert_eq!(sender.pending.messages.load(Ordering::SeqCst), 0);
        assert!(!sender.finish_turn());
    }

    #[test]
    fn managed_channel_backpressures_workers_and_shutdown_returns_control_message() {
        let (sender, receiver) = channel_with_waker(|| {});
        for value in 0..MAX_PENDING_WORKER_MESSAGES {
            sender.send(value).unwrap();
        }
        let worker = sender.clone();
        let (done, result) = mpsc::channel();
        let joined = thread::spawn(move || {
            done.send(worker.send(999)).unwrap();
        });
        assert!(result.recv_timeout(Duration::from_millis(10)).is_err());
        sender.send(888).unwrap(); // The saturated queue must not block the UI.
        drop(receiver);
        assert_eq!(
            result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err()
                .0,
            999
        );
        joined.join().unwrap();
    }

    #[test]
    fn managed_channel_worker_flood_is_lossless_and_bounded() {
        let (sender, receiver) = channel_with_waker(|| {});
        let worker = sender.clone();
        let joined = thread::spawn(move || {
            for value in 0..1024 {
                worker.send(value).unwrap();
            }
        });
        for expected in 0..1024 {
            let value = loop {
                match receiver.try_recv() {
                    Ok(value) => break value,
                    Err(mpsc::TryRecvError::Empty) => thread::yield_now(),
                    Err(error) => panic!("{error}"),
                }
            };
            assert_eq!(value, expected);
            assert!(sender.pending.messages.load(Ordering::SeqCst) <= MAX_PENDING_WORKER_MESSAGES);
            sender.message_received();
        }
        joined.join().unwrap();
        assert!(!sender.finish_turn());
    }

    #[test]
    fn worker_admission_is_bounded_and_returns_ownership_on_timeout() {
        let (raw, receiver) = mpsc::channel();
        let sender = Sender::with_waker(raw, || {});
        for value in 0..MAX_PENDING_WORKER_MESSAGES {
            sender.send_timeout(value, Duration::ZERO).unwrap();
        }
        assert_eq!(
            sender.send_timeout(999, Duration::from_millis(5)),
            Err(SendTimeoutError::Timeout(999))
        );
        for value in 0..MAX_PENDING_WORKER_MESSAGES {
            assert_eq!(receiver.try_recv(), Ok(value));
            sender.message_received();
        }
        assert!(!sender.finish_turn());
    }

    #[test]
    fn expired_admission_does_not_enqueue_when_capacity_is_available_late() {
        let (sender, receiver) = channel_with_waker(|| {});
        let guard = sender.pending.admission.lock().unwrap();
        let worker = sender.clone();
        let (started, ready) = mpsc::channel();
        let joined = thread::spawn(move || {
            started.send(()).unwrap();
            worker.send_timeout(123, Duration::from_millis(10))
        });
        ready.recv().unwrap();
        thread::sleep(Duration::from_millis(40));
        drop(guard);
        assert_eq!(joined.join().unwrap(), Err(SendTimeoutError::Timeout(123)));
        assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert!(!sender.finish_turn());
    }

    #[test]
    fn consuming_a_message_unblocks_workers_but_ui_never_waits_for_capacity() {
        let (raw, receiver) = mpsc::channel();
        let sender = Sender::with_waker(raw, || {});
        for value in 0..MAX_PENDING_WORKER_MESSAGES {
            sender.send(value).unwrap();
        }
        let worker = sender.clone();
        let joined = std::thread::spawn(move || worker.send_timeout(999, Duration::from_secs(2)));
        sender.send(888).unwrap();
        assert_eq!(receiver.try_recv(), Ok(0));
        sender.message_received();
        assert_eq!(receiver.try_recv(), Ok(1));
        sender.message_received();
        joined.join().unwrap().unwrap();
        let tail = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(&tail[tail.len() - 2..], &[888, 999]);
        for _ in tail {
            sender.message_received();
        }
        assert!(!sender.finish_turn());
    }

    #[test]
    fn disconnected_worker_send_returns_the_message_and_releases_admission() {
        let (raw, receiver) = mpsc::channel();
        let sender = Sender::with_waker(raw, || {});
        drop(receiver);
        for _ in 0..MAX_PENDING_WORKER_MESSAGES * 2 {
            assert_eq!(
                sender.send_timeout(Message("owned"), Duration::ZERO),
                Err(SendTimeoutError::Disconnected(Message("owned")))
            );
        }
        assert!(!sender.finish_turn());
    }

    #[test]
    fn message_is_queued_before_wake() {
        let (sender, receiver) = mpsc::channel();
        let receiver = Mutex::new(receiver);
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let sender = Sender::with_waker(sender, move || {
            assert_eq!(receiver.lock().unwrap().try_recv(), Ok(42));
            wake_count.fetch_add(1, Ordering::SeqCst);
        });

        sender.send(42).unwrap();

        assert_eq!(wakes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn output_flood_wakes_once_per_native_turn() {
        let (sender, receiver) = mpsc::channel();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let sender = Sender::with_waker(sender, move || {
            wake_count.fetch_add(1, Ordering::SeqCst);
        });

        for message in 1..=3 {
            sender.send(message).unwrap();
            assert_eq!(wakes.load(Ordering::SeqCst), 1);
            assert_eq!(receiver.try_recv(), Ok(message));
            sender.message_received();
        }
        assert!(!sender.finish_turn());
        sender.send(4).unwrap();
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        assert!(sender.finish_turn());
        assert_eq!(receiver.try_recv(), Ok(4));
        sender.message_received();
        assert!(!sender.finish_turn());
    }

    #[test]
    fn bounded_drain_defers_remaining_work_without_feeding_the_proxy_drain() {
        let (raw, receiver) = mpsc::channel();
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let sender = Sender::with_waker(raw, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        for message in 0..1000 {
            sender.send(message).unwrap();
        }
        for message in 0..64 {
            assert_eq!(receiver.try_recv(), Ok(message));
            sender.message_received();
        }
        assert!(sender.finish_turn());
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        sender.send(1000).unwrap();
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        for message in 64..=1000 {
            assert_eq!(receiver.try_recv(), Ok(message));
            sender.message_received();
        }
        assert!(!sender.finish_turn());
    }

    #[test]
    fn closed_channel_returns_original_message_without_waking() {
        let (sender, receiver) = mpsc::channel();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let sender = Sender::with_waker(sender, move || {
            wake_count.fetch_add(1, Ordering::SeqCst);
        });
        drop(receiver);

        let error: SendError<Message> = sender.send(Message("undelivered")).unwrap_err();

        assert_eq!(error.0, Message("undelivered"));
        assert_eq!(wakes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn clone_can_send_non_clone_messages_from_another_thread() {
        let (sender, receiver) = mpsc::channel();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let sender = Sender::with_waker(sender, move || {
            wake_count.fetch_add(1, Ordering::SeqCst);
        });
        let clone = sender.clone();
        drop(sender);

        std::thread::spawn(move || clone.send(Message("from clone")).unwrap())
            .join()
            .unwrap();

        assert_eq!(receiver.try_recv(), Ok(Message("from clone")));
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }
}
