//! Bounded clipboard workers. Outgoing transfers have no waiting queue; host
//! image conversion keeps only one active job and the latest pending snapshot.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostImageToken {
    pub generation: u64,
    pub change_count: isize,
}

impl HostImageToken {
    pub fn is_current(self, generation: u64, cached_count: isize, live_count: isize) -> bool {
        self.generation == generation
            && self.change_count == cached_count
            && self.change_count == live_count
    }
}

pub type HostImageResult = Result<Option<Arc<[u8]>>, String>;

pub struct HostImageCompletion {
    pub token: HostImageToken,
    pub result: HostImageResult,
}

#[derive(Default)]
pub struct HostClipboardContents {
    pub text: Option<Arc<str>>,
    pub image_png: Option<Arc<[u8]>>,
}

impl HostClipboardContents {
    /// Delayed images augment, never replace, the text from the same snapshot.
    pub fn merge_image(
        &mut self,
        completion: HostImageCompletion,
        generation: u64,
        cached_count: isize,
        live_count: isize,
    ) -> Result<bool, String> {
        if !completion
            .token
            .is_current(generation, cached_count, live_count)
        {
            return Ok(false);
        }
        let image = completion.result?;
        let changed = self.image_png != image;
        self.image_png = image;
        Ok(changed)
    }
}

struct HostImageJob {
    token: HostImageToken,
    tiff: Box<[u8]>,
}

type ImageWaker = Arc<dyn Fn() + Send + Sync>;
type ImageTask = Box<dyn FnOnce() -> HostImageCompletion + Send>;

struct HostImageChannels {
    jobs: std::sync::mpsc::SyncSender<(ImageTask, Option<ImageWaker>)>,
    results: std::sync::mpsc::Receiver<HostImageCompletion>,
}

#[derive(Default)]
pub struct HostImageWorker {
    pending: Option<HostImageJob>,
    in_flight: Option<HostImageToken>,
    worker: Option<HostImageChannels>,
    wake: Option<ImageWaker>,
}

impl HostImageWorker {
    /// Install before the first snapshot. The callback must not panic or block.
    pub fn set_waker(&mut self, wake: ImageWaker) {
        self.wake = Some(wake);
    }

    pub fn replace_pending(&mut self, image: Option<(HostImageToken, Box<[u8]>)>) {
        self.pending = image.map(|(token, tiff)| HostImageJob { token, tiff });
    }

    pub fn has_work(&self) -> bool {
        self.pending.is_some() || self.in_flight.is_some()
    }

    pub fn pending_token(&self) -> Option<HostImageToken> {
        self.pending.as_ref().map(|job| job.token)
    }

    pub fn start_pending(
        &mut self,
        convert: impl FnOnce(Box<[u8]>) -> HostImageResult + Send + 'static,
    ) -> io::Result<()> {
        self.start_pending_with(convert, |task| {
            std::thread::Builder::new()
                .name("clipboard-image".into())
                .spawn(task)
                .map(|_| ())
        })
    }

    fn start_pending_with(
        &mut self,
        convert: impl FnOnce(Box<[u8]>) -> HostImageResult + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.in_flight.is_some() {
            return Ok(());
        }
        let Some(job) = self.pending.take() else {
            return Ok(());
        };
        if self.worker.is_none() {
            let (jobs, requests) =
                std::sync::mpsc::sync_channel::<(ImageTask, Option<ImageWaker>)>(1);
            let (completed, results) = std::sync::mpsc::sync_channel(1);
            spawn(Box::new(move || {
                // One persistent worker, sleeping on a channel, not a periodic loop.
                while let Ok((task, wake)) = requests.recv() {
                    if completed.send(task()).is_err() {
                        break;
                    }
                    if let Some(wake) = wake {
                        wake();
                    }
                }
            }))?;
            self.worker = Some(HostImageChannels { jobs, results });
        }
        let token = job.token;
        let task: ImageTask = Box::new(move || {
            // No AppKit/pasteboard object or borrowed provider bytes cross threads.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| convert(job.tiff)))
                    .unwrap_or_else(|_| Err("macOS clipboard image worker panicked".into()));
            HostImageCompletion { token, result }
        });
        if self
            .worker
            .as_ref()
            .unwrap()
            .jobs
            .try_send((task, self.wake.clone()))
            .is_err()
        {
            self.worker = None;
            return Err(io::Error::other("macOS clipboard image worker unavailable"));
        }
        self.in_flight = Some(token);
        Ok(())
    }

    /// Receiving a completion frees the single conversion slot without joining.
    pub fn poll(&mut self) -> Option<HostImageCompletion> {
        let token = self.in_flight?;
        let completion = match self.worker.as_ref()?.results.try_recv() {
            Ok(completion) => completion,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.worker = None;
                HostImageCompletion {
                    token,
                    result: Err("macOS clipboard image worker exited without a result".into()),
                }
            }
        };
        self.in_flight = None;
        Some(completion)
    }
}

/// Image receives can arrive immediately after the paste key, before conversion.
/// These own only bounded FDs, not extra workers or copies of image data.
#[derive(Default)]
pub struct PendingHostImageRequests {
    token: Option<HostImageToken>,
    fds: Vec<std::os::fd::OwnedFd>,
}

impl PendingHostImageRequests {
    pub fn reset(&mut self, token: Option<HostImageToken>) {
        self.fds.clear();
        self.token = token;
    }

    pub fn token(&self) -> Option<HostImageToken> {
        self.token
    }

    pub fn retain(&mut self, token: HostImageToken, fd: std::os::fd::OwnedFd) -> io::Result<()> {
        if self.token != Some(token) {
            return Err(io::Error::other(
                "clipboard image snapshot is no longer pending",
            ));
        }
        if self.fds.len() >= MAX_TRANSFERS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "pending clipboard image FD limit reached",
            ));
        }
        self.fds.push(fd);
        Ok(())
    }

    pub fn take(&mut self, token: HostImageToken) -> Vec<std::os::fd::OwnedFd> {
        if self.token != Some(token) {
            return Vec::new();
        }
        self.token = None;
        std::mem::take(&mut self.fds)
    }
}

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

    fn image_token(generation: u64, change_count: isize) -> HostImageToken {
        HostImageToken {
            generation,
            change_count,
        }
    }

    fn image_completion(token: HostImageToken, bytes: &[u8]) -> HostImageCompletion {
        HostImageCompletion {
            token,
            result: Ok(Some(Arc::from(bytes))),
        }
    }

    #[test]
    fn image_completion_requires_generation_cached_and_live_counts() {
        let token = image_token(7, 23);
        for (generation, cached, live) in [(8, 23, 23), (7, 24, 23), (7, 23, 24)] {
            let mut contents = HostClipboardContents {
                text: Some(Arc::from("new selection")),
                image_png: Some(Arc::from(&b"new png"[..])),
            };
            assert!(
                !contents
                    .merge_image(
                        image_completion(token, b"old png"),
                        generation,
                        cached,
                        live
                    )
                    .unwrap()
            );
            assert_eq!(contents.text.as_deref(), Some("new selection"));
            assert_eq!(contents.image_png.as_deref(), Some(&b"new png"[..]));
        }
        // A guest selection invalidates conversion even if it never writes the
        // host pasteboard (including unsupported MIME and cleared selections).
        assert!(!token.is_current(8, 23, 23));
    }

    #[test]
    fn converted_image_merges_with_text_and_errors_do_not_erase_contents() {
        let token = image_token(7, 23);
        let text: Arc<str> = Arc::from("synchronous text");
        let mut contents = HostClipboardContents {
            text: Some(text.clone()),
            image_png: None,
        };
        assert!(
            contents
                .merge_image(image_completion(token, b"png"), 7, 23, 23)
                .unwrap()
        );
        assert!(Arc::ptr_eq(contents.text.as_ref().unwrap(), &text));
        assert_eq!(contents.image_png.as_deref(), Some(&b"png"[..]));
        assert!(
            !contents
                .merge_image(image_completion(token, b"png"), 7, 23, 23)
                .unwrap()
        );
        let failure = HostImageCompletion {
            token,
            result: Err("injected decode failure".into()),
        };
        assert!(
            contents
                .merge_image(failure, 7, 23, 23)
                .unwrap_err()
                .contains("decode")
        );
        assert_eq!(contents.image_png.as_deref(), Some(&b"png"[..]));
        let stale_failure = HostImageCompletion {
            token,
            result: Err("obsolete".into()),
        };
        assert!(!contents.merge_image(stale_failure, 8, 23, 23).unwrap());
    }

    fn image_test_worker() -> (HostImageWorker, std::sync::mpsc::Receiver<()>) {
        let (wake, wakes) = std::sync::mpsc::channel();
        let mut worker = HostImageWorker::default();
        worker.set_waker(Arc::new(move || {
            wake.send(()).unwrap();
        }));
        (worker, wakes)
    }

    #[test]
    fn image_worker_is_single_flight_coalesces_and_wakes_after_completion() {
        let (mut worker, wakes) = image_test_worker();
        let (release, gate) = std::sync::mpsc::channel();
        let first = image_token(1, 10);
        let latest = image_token(3, 12);
        worker.replace_pending(Some((first, Box::from(&b"first"[..]))));
        worker
            .start_pending(move |bytes| {
                gate.recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
                Ok(Some(Arc::from(bytes)))
            })
            .unwrap();
        assert!(worker.has_work());
        assert!(worker.poll().is_none());
        worker.replace_pending(Some((image_token(2, 11), Box::from(&b"obsolete"[..]))));
        worker.replace_pending(Some((latest, Box::from(&b"latest"[..]))));
        worker
            .start_pending(|_| panic!("cannot start a concurrent conversion"))
            .unwrap();
        assert_eq!(worker.pending_token(), Some(latest));
        release.send(()).unwrap();
        wakes
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let completed = worker.poll().unwrap();
        assert_eq!(completed.token, first);
        assert_eq!(completed.result.unwrap().as_deref(), Some(&b"first"[..]));
        assert!(worker.poll().is_none());
        worker
            .start_pending(|bytes| Ok(Some(Arc::from(bytes))))
            .unwrap();
        wakes
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let completed = worker.poll().unwrap();
        assert_eq!(completed.token, latest);
        assert_eq!(completed.result.unwrap().as_deref(), Some(&b"latest"[..]));
        assert!(!worker.has_work());
    }

    #[test]
    fn image_worker_survives_conversion_errors_and_panics() {
        let (mut worker, wakes) = image_test_worker();
        for panic in [false, true] {
            worker.replace_pending(Some((image_token(1, 1), Box::default())));
            worker
                .start_pending(move |_| {
                    if panic {
                        panic!("injected conversion panic");
                    }
                    Err("injected conversion error".into())
                })
                .unwrap();
            wakes
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert!(
                worker
                    .poll()
                    .unwrap()
                    .result
                    .unwrap_err()
                    .contains(if panic { "panicked" } else { "error" })
            );
            assert!(!worker.has_work());
        }
        worker.replace_pending(Some((image_token(2, 2), Box::default())));
        worker.start_pending(|_| Ok(None)).unwrap();
        wakes
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(worker.poll().unwrap().result.unwrap().is_none());
    }

    #[test]
    fn image_worker_reports_spawn_failure_and_disconnected_completion() {
        let token = image_token(1, 1);
        let mut worker = HostImageWorker::default();
        worker.replace_pending(Some((token, Box::default())));
        let error = worker
            .start_pending_with(
                |_| Ok(None),
                |_| Err(io::Error::other("injected spawn failure")),
            )
            .unwrap_err();
        assert!(error.to_string().contains("spawn failure"));
        assert!(!worker.has_work());
        let (jobs, _requests) = std::sync::mpsc::sync_channel(1);
        let (sender, results) = std::sync::mpsc::sync_channel(1);
        drop(sender);
        worker.worker = Some(HostImageChannels { jobs, results });
        worker.in_flight = Some(token);
        let completion = worker.poll().unwrap();
        assert_eq!(completion.token, token);
        assert!(completion.result.unwrap_err().contains("without a result"));
        assert!(worker.worker.is_none());
        assert!(!worker.has_work());
    }

    #[test]
    fn pending_image_receivers_are_bounded_and_closed_on_replacement() {
        let token = image_token(1, 10);
        let mut pending = PendingHostImageRequests::default();
        pending.reset(Some(token));
        let mut readers = Vec::new();
        for _ in 0..MAX_TRANSFERS {
            let (reader, writer) = UnixStream::pair().unwrap();
            reader.set_nonblocking(true).unwrap();
            pending.retain(token, writer.into()).unwrap();
            readers.push(reader);
        }
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        assert_eq!(
            pending.retain(token, writer.into()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        assert!(pending.take(image_token(0, 9)).is_empty());
        assert_eq!(pending.fds.len(), MAX_TRANSFERS);
        pending.reset(Some(image_token(2, 11)));
        for mut reader in readers {
            assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        }
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        assert!(pending.retain(token, writer.into()).is_err());
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn immediate_paste_receivers_get_the_completed_snapshot_or_eof_on_failure() {
        use std::io::Write;
        let token = image_token(1, 10);
        let mut pending = PendingHostImageRequests::default();
        for success in [true, false] {
            pending.reset(Some(token));
            let (mut reader, writer) = UnixStream::pair().unwrap();
            pending.retain(token, writer.into()).unwrap();
            let mut contents = HostClipboardContents::default();
            let completion = if success {
                image_completion(token, b"png")
            } else {
                HostImageCompletion {
                    token,
                    result: Err("decode failed".into()),
                }
            };
            let fds = pending.take(token);
            let accepted = contents.merge_image(completion, 1, 10, 10).unwrap_or(false);
            for fd in fds {
                if accepted {
                    let mut writer = std::fs::File::from(fd);
                    writer
                        .write_all(contents.image_png.as_ref().unwrap())
                        .unwrap();
                }
            }
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, if success { &b"png"[..] } else { &[][..] });
            assert!(pending.token().is_none());
        }
    }

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
