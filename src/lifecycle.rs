//! Bounded cleanup retaining ownership across panics, retries and UI shutdown.
//! Native objects must not enter a job. Destruction and abandoned queue draining
//! happen on the background reaper, never on AppKit.

use crate::container_sessions::ProfileId;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

type Outcome = Result<(), String>;
type Job = Arc<Mutex<Box<dyn Work>>>;

trait Work: Send {
    fn cleanup(&mut self) -> Outcome;
    fn complete(&self, outcome: Outcome);
}

struct Owned<T> {
    resources: T,
    cleanup: fn(&mut T) -> Outcome,
    complete: fn(&T, Outcome),
}

impl<T: Send> Work for Owned<T> {
    fn cleanup(&mut self) -> Outcome {
        (self.cleanup)(&mut self.resources)
    }

    fn complete(&self, outcome: Outcome) {
        (self.complete)(&self.resources, outcome);
    }
}

fn notify_completion(job: &Job, outcome: Outcome) {
    let job = job.lock().unwrap_or_else(|e| e.into_inner());
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.complete(outcome))).is_err() {
        log::error!("Lifecycle completion callback panicked; resource ownership remains protected");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Session(ProfileId, String),
    Display(String),
    Abandoned,
}

struct Entry {
    key: Key,
    job: Job,
    failures: u32,
    last_error: Option<String>,
    ready_at: Instant,
}

struct Running {
    entry: Entry,
    receiver: mpsc::Receiver<Outcome>,
}

struct Disposing {
    key: Key,
    receiver: mpsc::Receiver<()>,
}

struct QueueState {
    pending: VecDeque<Entry>,
    running: Vec<Running>,
    disposing: Vec<Disposing>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

/// Pending and failed jobs own live resources. They retain their leases until
/// cleanup succeeds and its completion is consumed. Failure is not drain.
pub struct CleanupQueue {
    state: Option<QueueState>,
}

impl CleanupQueue {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            state: Some(QueueState {
                pending: VecDeque::new(),
                running: Vec::new(),
                disposing: Vec::new(),
                wake: Arc::new(wake),
            }),
        }
    }

    /// Put every resource and lease in `resources`, not in a callback. Cleanup
    /// must be retry-safe. Completion runs on the polling/UI thread and borrows
    /// the bundle, so it cannot release ownership while reporting failure.
    pub fn enqueue<T: Send + 'static>(
        &mut self,
        key: Key,
        resources: T,
        cleanup: fn(&mut T) -> Outcome,
        complete: fn(&T, Outcome),
    ) {
        let state = self.state.as_mut().unwrap();
        state.pending.push_back(Entry {
            key,
            job: Arc::new(Mutex::new(Box::new(Owned {
                resources,
                cleanup,
                complete,
            }))),
            failures: 0,
            last_error: None,
            ready_at: Instant::now(),
        });
        state.pump();
    }

    pub fn session_busy(&self, id: ProfileId) -> bool {
        self.keys()
            .any(|key| matches!(key, Key::Session(candidate, _) if *candidate == id))
    }

    pub fn display_busy(&self, slot: &str) -> bool {
        self.keys().any(|key| match key {
            Key::Session(_, display) | Key::Display(display) => display == slot,
            Key::Abandoned => false,
        })
    }

    fn keys(&self) -> impl Iterator<Item = &Key> {
        let state = self.state.as_ref().unwrap();
        state
            .pending
            .iter()
            .map(|entry| &entry.key)
            .chain(state.running.iter().map(|running| &running.entry.key))
            .chain(state.disposing.iter().map(|disposing| &disposing.key))
    }

    pub fn is_empty(&self) -> bool {
        self.state.as_ref().unwrap().is_empty()
    }

    /// Include this in the native loop's next wakeup, including when no windows
    /// remain: failed cleanup and thread creation need timer-driven retries.
    pub fn next_wakeup(&self) -> Option<Instant> {
        self.state.as_ref().unwrap().next_wakeup()
    }

    pub fn poll(&mut self) {
        self.state.as_mut().unwrap().poll(true);
    }

    /// Only after input admission stops. False means unresolved cleanup; Drop
    /// transfers remaining ownership to the reaper. The parent must separately
    /// drain outstanding launch producers before treating shutdown as complete.
    pub fn finish(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.poll();
            if self.is_empty() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(5)));
        }
    }
}

impl Drop for CleanupQueue {
    fn drop(&mut self) {
        if let Some(mut state) = self.state.take()
            && !state.is_empty()
        {
            let reaper = Arc::clone(reaper());
            state.wake = Arc::new(move || reaper.ready.notify_one());
            retire(Retired::Queue(state));
        }
    }
}

impl QueueState {
    fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.running.is_empty() && self.disposing.is_empty()
    }

    fn next_wakeup(&self) -> Option<Instant> {
        // Full concurrency relies on worker completion notifications.
        (self.running.len() < 2)
            .then(|| self.pending.iter().map(|entry| entry.ready_at).min())
            .flatten()
    }

    fn retry(&mut self, mut entry: Entry, error: String, notify: bool) {
        log::error!(
            "Lifecycle cleanup {:?} failed; retaining ownership: {error}",
            entry.key
        );
        if notify && entry.last_error.as_ref() != Some(&error) {
            notify_completion(&entry.job, Err(error.clone()));
        }
        entry.last_error = Some(error);
        entry.failures = entry.failures.saturating_add(1);
        let backoff =
            Duration::from_millis(100).saturating_mul(1 << entry.failures.saturating_sub(1).min(8));
        entry.ready_at = Instant::now() + backoff;
        self.pending.push_back(entry);
    }

    fn poll(&mut self, notify: bool) {
        self.disposing
            .retain(|disposing| match disposing.receiver.try_recv() {
                Ok(()) => false,
                Err(mpsc::TryRecvError::Empty) => true,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // A lost disposal acknowledgement is not proof of resource release.
                    log::error!(
                        "Lifecycle disposal {:?} disconnected; keeping identity busy",
                        disposing.key
                    );
                    true
                }
            });
        let mut index = 0;
        while index < self.running.len() {
            let outcome = match self.running[index].receiver.try_recv() {
                Ok(outcome) => outcome,
                Err(mpsc::TryRecvError::Empty) => {
                    index += 1;
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => Err("cleanup worker disconnected".into()),
            };
            let Running { entry, .. } = self.running.swap_remove(index);
            match outcome {
                Ok(()) => {
                    if notify {
                        notify_completion(&entry.job, Ok(()));
                        let (sender, receiver) = mpsc::sync_channel(1);
                        self.disposing.push(Disposing {
                            key: entry.key,
                            receiver,
                        });
                        retire(Retired::Job {
                            job: entry.job,
                            done: sender,
                            wake: Arc::clone(&self.wake),
                        });
                    } else {
                        drop(entry.job);
                    }
                }
                Err(error) => self.retry(entry, error, notify),
            }
        }
        self.pump();
    }

    fn pump(&mut self) {
        while self.running.len() < 2 {
            let now = Instant::now();
            let Some(position) = self.pending.iter().position(|entry| entry.ready_at <= now) else {
                break;
            };
            let entry = self.pending.remove(position).unwrap();
            let owned = Arc::clone(&entry.job);
            let (sender, receiver) = mpsc::sync_channel(1);
            let wake = Arc::clone(&self.wake);
            let spawn = std::thread::Builder::new()
                .name("cocoa-way-cleanup".into())
                .spawn(move || {
                    // Both the owning envelope and guard are outside catch_unwind.
                    // Unwinding the borrowed operation cannot destroy its resources.
                    let mut job = owned.lock().unwrap_or_else(|e| e.into_inner());
                    let outcome =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.cleanup()))
                            .unwrap_or_else(|_| Err("cleanup worker panicked".into()));
                    drop(job);
                    drop(owned);
                    let _ = sender.send(outcome);
                    wake();
                });
            match spawn {
                Ok(_) => self.running.push(Running { entry, receiver }),
                Err(error) => {
                    self.retry(
                        entry,
                        format!("could not start cleanup worker: {error}"),
                        false,
                    );
                    break;
                }
            }
        }
    }
}

enum Retired {
    Job {
        job: Job,
        done: mpsc::SyncSender<()>,
        wake: Arc<dyn Fn() + Send + Sync>,
    },
    Queue(QueueState),
}

#[derive(Default)]
struct Reaper {
    inbox: Mutex<Vec<Retired>>,
    ready: Condvar,
    started: AtomicBool,
}

fn reaper() -> &'static Arc<Reaper> {
    static REAPER: OnceLock<Arc<Reaper>> = OnceLock::new();
    REAPER.get_or_init(|| Arc::new(Reaper::default()))
}

fn retire(item: Retired) {
    let reaper = reaper();
    reaper
        .inbox
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(item);
    reaper.ready.notify_one();
    if !reaper.started.swap(true, Ordering::AcqRel) {
        let owned = Arc::clone(reaper);
        if let Err(error) = std::thread::Builder::new()
            .name("cocoa-way-reaper".into())
            .spawn(move || reap(owned))
        {
            // The process-lifetime inbox still owns everything. In particular,
            // never fall back to joining an audio worker on the caller thread.
            reaper.started.store(false, Ordering::Release);
            log::error!("Could not start lifecycle reaper; resources remain reserved: {error}");
        }
    }
}

fn reap(reaper: Arc<Reaper>) {
    let mut queues = Vec::<QueueState>::new();
    loop {
        let retired = std::mem::take(&mut *reaper.inbox.lock().unwrap_or_else(|e| e.into_inner()));
        for item in retired {
            match item {
                Retired::Job { job, done, wake } => {
                    drop(job);
                    let _ = done.send(());
                    wake();
                }
                Retired::Queue(queue) => queues.push(queue),
            }
        }
        queues.retain_mut(|queue| {
            queue.poll(false);
            !queue.is_empty()
        });
        let mut inbox = reaper.inbox.lock().unwrap_or_else(|e| e.into_inner());
        if !inbox.is_empty() {
            continue;
        }
        // Workers started before handoff may still hold the old UI waker.
        // Failed-only queues sleep until backoff expires, not at a fixed rate.
        let delay = if queues
            .iter()
            .any(|queue| !queue.running.is_empty() || !queue.disposing.is_empty())
        {
            Some(Duration::from_millis(50))
        } else {
            queues
                .iter()
                .filter_map(QueueState::next_wakeup)
                .min()
                .map(|due| due.saturating_duration_since(Instant::now()))
        };
        inbox = match delay {
            Some(delay) => {
                reaper
                    .ready
                    .wait_timeout(inbox, delay)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
            None => reaper.ready.wait(inbox).unwrap_or_else(|e| e.into_inner()),
        };
        drop(inbox);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for background cleanup"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn enqueue_is_nonblocking_and_concurrency_is_bounded() {
        struct Resource {
            active: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
        }
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut queue = CleanupQueue::new(|| {});
        let ids = (0..10)
            .map(|_| ProfileId::new().unwrap())
            .collect::<Vec<_>>();
        let started = Instant::now();
        for (index, id) in ids.iter().copied().enumerate() {
            queue.enqueue(
                Key::Session(id, format!("test-{index}")),
                Resource {
                    active: Arc::clone(&active),
                    peak: Arc::clone(&peak),
                },
                |resource| {
                    let count = resource.active.fetch_add(1, Ordering::SeqCst) + 1;
                    resource.peak.fetch_max(count, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    resource.active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                },
                |_, _| {},
            );
        }
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(queue.session_busy(ids[9]));
        assert!(queue.display_busy("test-9"));
        assert!(queue.finish(Duration::from_secs(3)));
        assert!(peak.load(Ordering::SeqCst) <= 2);
    }

    struct Lease(Arc<AtomicUsize>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn completion_keeps_the_lease_until_main_thread_consumes_it() {
        let released = Arc::new(AtomicUsize::new(0));
        let mut queue = CleanupQueue::new(|| {});
        queue.enqueue(
            Key::Abandoned,
            Lease(Arc::clone(&released)),
            |_| Ok(()),
            |lease, result| {
                assert!(result.is_ok());
                assert_eq!(lease.0.load(Ordering::SeqCst), 0);
            },
        );
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(released.load(Ordering::SeqCst), 0);
        assert!(queue.finish(Duration::from_secs(1)));
        wait_until(|| released.load(Ordering::SeqCst) == 1);
    }

    #[test]
    fn panic_retains_lease_and_busy_key_through_retry_and_completion() {
        struct Resource {
            lease: Lease,
            attempts: Arc<AtomicUsize>,
            outcomes: Arc<Mutex<Vec<bool>>>,
        }
        let released = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::new(AtomicUsize::new(0));
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let id = ProfileId::new().unwrap();
        let mut queue = CleanupQueue::new(|| {});
        queue.enqueue(
            Key::Session(id, "panic-slot".into()),
            Resource {
                lease: Lease(Arc::clone(&released)),
                attempts: Arc::clone(&attempts),
                outcomes: Arc::clone(&outcomes),
            },
            |resource| {
                if resource.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("injected cleanup panic");
                }
                Ok(())
            },
            |resource, result| {
                assert_eq!(resource.lease.0.load(Ordering::SeqCst), 0);
                resource.outcomes.lock().unwrap().push(result.is_ok());
            },
        );
        wait_until(|| attempts.load(Ordering::SeqCst) == 1);
        wait_until(|| {
            queue.poll();
            !outcomes.lock().unwrap().is_empty()
        });
        assert!(queue.session_busy(id));
        assert!(queue.display_busy("panic-slot"));
        assert_eq!(released.load(Ordering::SeqCst), 0);
        assert!(queue.next_wakeup().is_some());
        assert!(queue.finish(Duration::from_secs(2)));
        assert_eq!(*outcomes.lock().unwrap(), [false, true]);
        wait_until(|| released.load(Ordering::SeqCst) == 1);
    }

    #[test]
    fn failed_cleanup_is_not_finished_and_keeps_ownership_after_queue_drop() {
        struct Resource {
            _lease: Lease,
            allowed: Arc<AtomicBool>,
            attempts: Arc<AtomicUsize>,
        }
        let released = Arc::new(AtomicUsize::new(0));
        let allowed = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut queue = CleanupQueue::new(|| {});
        queue.enqueue(
            Key::Display("failed-slot".into()),
            Resource {
                _lease: Lease(Arc::clone(&released)),
                allowed: Arc::clone(&allowed),
                attempts: Arc::clone(&attempts),
            },
            |resource| {
                resource.attempts.fetch_add(1, Ordering::SeqCst);
                if resource.allowed.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err("runtime unavailable".into())
                }
            },
            |_, _| {},
        );
        assert!(!queue.finish(Duration::from_millis(20)));
        assert!(queue.display_busy("failed-slot"));
        drop(queue);
        wait_until(|| attempts.load(Ordering::SeqCst) >= 2);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        allowed.store(true, Ordering::SeqCst);
        wait_until(|| released.load(Ordering::SeqCst) == 1);
    }

    #[test]
    fn deadline_and_queue_drop_never_join_pending_resource_on_ui_thread() {
        struct BlockingDrop {
            started: mpsc::Sender<std::thread::ThreadId>,
            release: mpsc::Receiver<()>,
        }
        impl Drop for BlockingDrop {
            fn drop(&mut self) {
                let _ = self.started.send(std::thread::current().id());
                let _ = self.release.recv();
            }
        }
        let mut queue = CleanupQueue::new(|| {});
        let (release1, wait1) = mpsc::channel();
        let (release2, wait2) = mpsc::channel();
        for wait in [wait1, wait2] {
            queue.enqueue(
                Key::Abandoned,
                wait,
                |wait| {
                    let _ = wait.recv();
                    Ok(())
                },
                |_, _| {},
            );
        }
        let (started, observe) = mpsc::channel();
        let (release_drop, wait_drop) = mpsc::channel();
        queue.enqueue(
            Key::Abandoned,
            BlockingDrop {
                started,
                release: wait_drop,
            },
            |_| Ok(()),
            |_, _| {},
        );
        let started = Instant::now();
        assert!(!queue.finish(Duration::from_millis(10)));
        drop(queue);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(observe.try_recv().is_err());
        release1.send(()).unwrap();
        release2.send(()).unwrap();
        let destructor_thread = observe.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_ne!(destructor_thread, std::thread::current().id());
        release_drop.send(()).unwrap();
    }

    #[test]
    fn successful_disposal_is_nonblocking_and_busy_until_background_release() {
        struct Resource {
            _lease: Lease,
            completed: Arc<AtomicBool>,
            dropping: mpsc::Sender<std::thread::ThreadId>,
            release: mpsc::Receiver<()>,
        }
        impl Drop for Resource {
            fn drop(&mut self) {
                let _ = self.dropping.send(std::thread::current().id());
                let _ = self.release.recv();
            }
        }
        let released = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicBool::new(false));
        let (dropping, observe) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let id = ProfileId::new().unwrap();
        let mut queue = CleanupQueue::new(|| {});
        queue.enqueue(
            Key::Session(id, "dispose-slot".into()),
            Resource {
                _lease: Lease(Arc::clone(&released)),
                completed: Arc::clone(&completed),
                dropping,
                release: wait,
            },
            |_| Ok(()),
            |resource, _| {
                resource.completed.store(true, Ordering::SeqCst);
            },
        );
        wait_until(|| {
            queue.poll();
            completed.load(Ordering::SeqCst)
        });
        assert_ne!(
            observe.recv_timeout(Duration::from_secs(3)).unwrap(),
            std::thread::current().id()
        );
        assert!(queue.session_busy(id));
        assert_eq!(released.load(Ordering::SeqCst), 0);
        assert!(!queue.finish(Duration::from_millis(10)));
        release.send(()).unwrap();
        assert!(queue.finish(Duration::from_secs(1)));
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }
}
