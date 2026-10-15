//! Nonblocking admission for outgoing clipboard workers. There is no waiting queue:
//! rejected work is dropped immediately, including its payload and receiver FD.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const MAX_TRANSFERS: usize = 8;
const MAX_TRANSFER_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 128 * 1024 * 1024;

fn try_add(counter: &AtomicUsize, amount: usize, limit: usize) -> bool {
    let mut used = counter.load(Ordering::Relaxed);
    loop {
        let Some(total) = used.checked_add(amount).filter(|total| *total <= limit) else {
            return false;
        };
        match counter.compare_exchange_weak(used, total, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(current) => used = current,
        }
    }
}

pub struct TransferBudget {
    active: AtomicUsize,
    bytes: AtomicUsize,
}

impl Default for TransferBudget {
    fn default() -> Self {
        Self {
            active: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
        }
    }
}

impl TransferBudget {
    pub fn spawn(
        self: &Arc<Self>,
        bytes: usize,
        worker: impl FnOnce() + Send + 'static,
    ) -> io::Result<()> {
        self.spawn_with(bytes, worker, |task| {
            std::thread::Builder::new()
                .name("clipboard-send".into())
                .spawn(task)
                .map(|_| ())
        })
    }

    fn reserve(self: &Arc<Self>, bytes: usize) -> io::Result<TransferPermit> {
        if bytes > MAX_TRANSFER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "clipboard payload exceeds the 64 MiB transfer limit",
            ));
        }
        if !try_add(&self.active, 1, MAX_TRANSFERS) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "clipboard worker limit reached",
            ));
        }
        if !try_add(&self.bytes, bytes, MAX_TOTAL_BYTES) {
            self.active.fetch_sub(1, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "clipboard byte budget exhausted",
            ));
        }
        Ok(TransferPermit {
            budget: Arc::clone(self),
            bytes,
        })
    }

    fn spawn_with(
        self: &Arc<Self>,
        bytes: usize,
        worker: impl FnOnce() + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    ) -> io::Result<()> {
        let permit = self.reserve(bytes)?;
        spawn(Box::new(move || {
            // Keep the reservation through I/O and release it on unwind, too.
            let _permit = permit;
            worker();
        }))
    }
}

struct TransferPermit {
    budget: Arc<TransferBudget>,
    bytes: usize,
}

impl Drop for TransferPermit {
    fn drop(&mut self) {
        self.budget.bytes.fetch_sub(self.bytes, Ordering::Relaxed);
        self.budget.active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    fn usage(budget: &TransferBudget) -> (usize, usize) {
        (
            budget.active.load(Ordering::Relaxed),
            budget.bytes.load(Ordering::Relaxed),
        )
    }

    #[test]
    fn stalled_transfers_are_bounded_and_rejection_closes_the_fd() {
        let budget = Arc::new(TransferBudget::default());
        let permits: Vec<_> = (0..MAX_TRANSFERS)
            .map(|_| budget.reserve(1).unwrap())
            .collect();
        let (mut receiver, sender) = UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        assert_eq!(
            budget.spawn(1, move || drop(sender)).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(receiver.read(&mut [0]).unwrap(), 0);
        assert_eq!(usage(&budget), (MAX_TRANSFERS, MAX_TRANSFERS));
        drop(permits);
        assert_eq!(usage(&budget), (0, 0));
        assert!(budget.reserve(1).is_ok());
    }

    #[test]
    fn repeated_requests_cannot_exceed_live_worker_and_fd_limits() {
        let budget = Arc::new(TransferBudget::default());
        let (completed, completions) = std::sync::mpsc::channel();
        let mut releases = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..MAX_TRANSFERS {
            let (release, gate) = std::sync::mpsc::channel();
            let (receiver, sender) = UnixStream::pair().unwrap();
            receiver.set_nonblocking(true).unwrap();
            let completed = completed.clone();
            budget
                .spawn_with(
                    1,
                    move || {
                        let _ = gate.recv_timeout(std::time::Duration::from_secs(5));
                        drop(sender);
                    },
                    move |task| {
                        std::thread::Builder::new()
                            .spawn(move || {
                                task();
                                let _ = completed.send(());
                            })
                            .map(|_| ())
                    },
                )
                .unwrap();
            releases.push(release);
            receivers.push(receiver);
        }
        for _ in 0..1000 {
            assert!(budget.spawn(1, || panic!("rejected worker ran")).is_err());
        }
        assert_eq!(usage(&budget), (MAX_TRANSFERS, MAX_TRANSFERS));
        for release in releases {
            release.send(()).unwrap();
        }
        for _ in 0..MAX_TRANSFERS {
            completions
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
        }
        assert_eq!(usage(&budget), (0, 0));
        for mut receiver in receivers {
            assert_eq!(receiver.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn byte_limits_are_inclusive_and_failed_reservations_roll_back() {
        let budget = Arc::new(TransferBudget::default());
        let first = budget.reserve(MAX_TRANSFER_BYTES).unwrap();
        let second = budget.reserve(MAX_TRANSFER_BYTES).unwrap();
        assert!(budget.reserve(1).is_err());
        assert_eq!(usage(&budget), (2, MAX_TOTAL_BYTES));
        drop(first);
        assert!(budget.reserve(MAX_TRANSFER_BYTES).is_ok());
        drop(second);
        assert!(budget.reserve(MAX_TRANSFER_BYTES + 1).is_err());
        assert!(budget.reserve(usize::MAX).is_err());
        assert_eq!(usage(&budget), (0, 0));
        let counter = AtomicUsize::new(usize::MAX);
        assert!(!try_add(&counter, 1, usize::MAX));
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
    }

    #[test]
    fn failed_worker_creation_releases_budget_payload_and_fd() {
        let budget = Arc::new(TransferBudget::default());
        let payload = Arc::new([0u8; 8]);
        let worker_payload = Arc::clone(&payload);
        let (mut receiver, sender) = UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let error = budget
            .spawn_with(
                payload.len(),
                move || drop((sender, worker_payload)),
                |task| {
                    drop(task);
                    Err(io::Error::other("injected worker creation failure"))
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("injected"));
        assert_eq!(usage(&budget), (0, 0));
        assert_eq!(Arc::strong_count(&payload), 1);
        assert_eq!(receiver.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn completed_or_panicking_work_releases_the_reservation() {
        let budget = Arc::new(TransferBudget::default());
        budget
            .spawn_with(
                7,
                || {},
                |task| {
                    task();
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(usage(&budget), (0, 0));
        let result = std::panic::catch_unwind(|| {
            let _ = budget.spawn_with(
                7,
                || panic!("injected worker panic"),
                |task| {
                    task();
                    Ok(())
                },
            );
        });
        assert!(result.is_err());
        assert_eq!(usage(&budget), (0, 0));
    }

    #[test]
    fn worker_completion_restores_capacity_without_joining_on_dispatch() {
        let budget = Arc::new(TransferBudget::default());
        let (finished, receiver) = std::sync::mpsc::channel();
        let worker_budget = Arc::clone(&budget);
        budget
            .spawn_with(
                1,
                || {},
                move |task| {
                    std::thread::Builder::new()
                        .spawn(move || {
                            task();
                            finished.send(usage(&worker_budget)).unwrap();
                        })
                        .map(|_| ())
                },
            )
            .unwrap();
        assert_eq!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            (0, 0)
        );
    }
}
