//! Tracks resource-producing workers through rollback, including native-loop shutdown.
use std::sync::{Arc, LazyLock, Mutex};

#[derive(Default)]
struct State {
    closing: bool,
    active: usize,
}

#[derive(Clone, Default)]
pub struct Producers(Arc<Mutex<State>>);

pub static GLOBAL: LazyLock<Producers> = LazyLock::new(Producers::default);

pub struct Lease(Producers);

impl Producers {
    pub fn begin(&self) -> Option<Lease> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.closing {
            return None;
        }
        state.active += 1;
        Some(Lease(self.clone()))
    }

    pub fn close(&self) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closing = true;
    }

    /// The supplied operation must be nonblocking. This serializes the decision
    /// to admit a resource-bearing reply with close(), without holding this lock
    /// while waiting for event-queue capacity.
    pub fn admit<T, R>(&self, message: T, send: impl FnOnce(T) -> R) -> Result<R, T> {
        let state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.closing {
            Err(message)
        } else {
            Ok(send(message))
        }
    }

    pub fn closing(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closing
    }

    pub fn active(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.0.0.lock().unwrap_or_else(|error| error.into_inner());
        state.active -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_rejects_new_work_but_tracks_existing_work_through_rollback() {
        let group = Producers::default();
        let first = group.begin().unwrap();
        let second = group.begin().unwrap();
        group.close();
        assert!(group.closing());
        assert!(group.begin().is_none());
        drop(first);
        assert_eq!(group.active(), 1);
        drop(second);
        assert_eq!(group.active(), 0);
    }

    #[test]
    fn admission_and_shutdown_are_atomic() {
        let group = Producers::default();
        let barrier = Arc::new(std::sync::Barrier::new(17));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let group = group.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    group.begin()
                })
            })
            .collect();
        barrier.wait();
        group.close();
        let leases: Vec<_> = threads
            .into_iter()
            .filter_map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(group.active(), leases.len());
        assert!(group.begin().is_none());
        drop(leases);
        assert_eq!(group.active(), 0);
    }

    #[test]
    fn closing_returns_ownership_without_invoking_the_message_sender() {
        let group = Producers::default();
        assert_eq!(group.admit(7, |message| message + 1), Ok(8));
        group.close();
        assert_eq!(
            group.admit(9, |_| panic!("must not admit after close")),
            Err(9)
        );
    }
}
