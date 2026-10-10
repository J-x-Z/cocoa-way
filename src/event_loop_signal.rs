//! An mpsc sender that wakes the event loop after queueing a message.

use std::sync::Arc;
use std::sync::mpsc::{self, SendError};

use winit::event_loop::EventLoopProxy;

/// Clones share the message queue and waker without requiring `T: Clone`.
pub struct Sender<T> {
    sender: mpsc::Sender<T>,
    wake: Arc<dyn Fn() + Send + Sync>,
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
        }
    }

    /// Queues the message before waking the loop on each successful send.
    ///
    /// A disconnected receiver returns the original message in `SendError<T>`
    /// without waking. Success reports channel delivery, not proxy availability.
    pub fn send(&self, message: T) -> Result<(), SendError<T>> {
        self.sender.send(message)?;
        (self.wake)();
        Ok(())
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            wake: Arc::clone(&self.wake),
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
    fn every_successful_send_wakes_once() {
        let (sender, receiver) = mpsc::channel();
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        let sender = Sender::with_waker(sender, move || {
            wake_count.fetch_add(1, Ordering::SeqCst);
        });

        for message in 1..=3 {
            sender.send(message).unwrap();
            assert_eq!(wakes.load(Ordering::SeqCst), message);
            assert_eq!(receiver.try_recv(), Ok(message));
        }
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
