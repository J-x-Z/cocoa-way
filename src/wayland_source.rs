use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use smithay::reexports::wayland_server::backend::{ClientId, Handle};
use smithay::reexports::wayland_server::{DisplayHandle, ListeningSocket};
use winit::event_loop::EventLoopProxy;

const MAX_ACCEPTS_PER_PASS: usize = 32;

#[derive(Default)]
struct OutputInterest {
    clients: HashMap<ClientId, Arc<UnixStream>>,
    blocked: HashSet<ClientId>,
}

#[derive(Default)]
struct FailureState {
    recorded: bool,
    message: Option<String>,
}

#[derive(Default)]
struct SourceStatus {
    stopping: AtomicBool,
    failure: Mutex<FailureState>,
    retry: Condvar,
}

impl SourceStatus {
    fn stop(&self) {
        let _guard = self
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.stopping.store(true, Ordering::Release);
        self.retry.notify_all();
    }

    fn record_failure(&self, message: String) -> bool {
        let mut failure = self
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.stopping.load(Ordering::Acquire) || failure.recorded {
            return false;
        }
        failure.recorded = true;
        failure.message = Some(message);
        true
    }

    fn fail(&self, message: String, wake: &impl Fn() -> bool) {
        if self.record_failure(message) {
            // Failure notification is independent of the ordinary readiness arm.
            let _ = wake();
        }
    }

    fn take_failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .message
            .take()
    }

    fn retry_interrupted<T>(
        &self,
        mut operation: impl FnMut() -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return Ok(None);
            }
            match operation() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    let guard = self
                        .failure
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    // Only interrupted operations back off; the idle poll has no timer.
                    let _ = self
                        .retry
                        .wait_timeout_while(guard, Duration::from_millis(1), |_| {
                            !self.stopping.load(Ordering::Acquire)
                        })
                        .unwrap_or_else(|error| error.into_inner());
                }
                result => return result.map(Some),
            }
        }
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    #[test]
    fn failure_is_published_before_one_wake_and_taken_once() {
        let status = SourceStatus::default();
        let wakes = AtomicUsize::new(0);
        let wake = || {
            assert!(status.failure.lock().unwrap().message.is_some());
            wakes.fetch_add(1, Ordering::Relaxed);
            false
        };
        status.fail("first failure".into(), &wake);
        assert_eq!(status.take_failure().as_deref(), Some("first failure"));
        assert!(status.take_failure().is_none());
        status.fail("later failure".into(), &wake);
        assert!(status.take_failure().is_none());
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn normal_stop_suppresses_failure_and_further_operations() {
        let status = SourceStatus::default();
        status.stop();
        status.fail("shutdown error".into(), &|| {
            panic!("stop must not wake for failure")
        });
        assert!(status.take_failure().is_none());
        assert_eq!(
            status
                .retry_interrupted::<()>(|| panic!("stop must not retry IO"))
                .unwrap(),
            None,
        );
    }

    #[test]
    fn interrupted_operations_back_off_then_recover() {
        let status = SourceStatus::default();
        let started = Instant::now();
        let mut attempts = 0;
        let result = status.retry_interrupted(|| {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(42)
            }
        });
        assert_eq!(result.unwrap(), Some(42));
        assert_eq!(attempts, 3);
        assert!(started.elapsed() >= Duration::from_millis(2));
        assert!(status.take_failure().is_none());
    }

    #[test]
    fn repeated_interruptions_do_not_fail_a_valid_source_or_spin() {
        let status = SourceStatus::default();
        let started = Instant::now();
        let mut attempts = 0;
        let result = status
            .retry_interrupted::<()>(|| {
                attempts += 1;
                if attempts <= 16 {
                    Err(io::Error::from(io::ErrorKind::Interrupted))
                } else {
                    Ok(())
                }
            })
            .unwrap();
        assert_eq!(result, Some(()));
        assert_eq!(attempts, 17);
        assert!(started.elapsed() >= Duration::from_millis(16));
        assert!(status.take_failure().is_none());
    }

    #[test]
    fn stop_cancels_interrupted_retries_without_failure() {
        let status = SourceStatus::default();
        let mut attempts = 0;
        assert_eq!(
            status
                .retry_interrupted::<()>(|| {
                    attempts += 1;
                    status.stop();
                    Err(io::Error::from(io::ErrorKind::Interrupted))
                })
                .unwrap(),
            None,
        );
        assert_eq!(attempts, 1);
        assert!(status.take_failure().is_none());
    }
}

/// The readiness thread never dispatches client code; AppKit stays on the main thread.
pub struct WaylandSource {
    control: UnixStream,
    armed: Arc<AtomicBool>,
    status: Arc<SourceStatus>,
    output: Arc<Mutex<OutputInterest>>,
    backend_handle: Option<Handle>,
    thread: Option<JoinHandle<()>>,
}

impl WaylandSource {
    pub fn new(
        display: BorrowedFd<'_>,
        listener: ListeningSocket,
        mut handle: DisplayHandle,
        proxy: EventLoopProxy<()>,
    ) -> io::Result<Self> {
        let listener_fd = listener.as_fd().try_clone_to_owned()?;
        let backend_handle = handle.backend_handle();
        let mut source = Self::watch(
            display,
            Some(listener_fd.as_fd()),
            move || proxy.send_event(()).is_ok(),
            move |output| {
                let Some(stream) = listener.accept()? else {
                    return Ok(false);
                };
                log::info!("New client connected");
                let monitor = Arc::new(stream.try_clone().map_err(|error| {
                    io::Error::other(format!("Could not monitor a Wayland client: {error}"))
                })?);
                let client = handle
                    .insert_client(
                        stream,
                        Arc::new(crate::state::ClientState {
                            compositor_state: Default::default(),
                        }),
                    )
                    .map_err(|error| {
                        io::Error::other(format!("Could not register a Wayland client: {error}"))
                    })?;
                // Backend registration finishes before taking the source lock.
                output.lock().unwrap().clients.insert(client.id(), monitor);
                Ok(true)
            },
        )?;
        source.backend_handle = Some(backend_handle);
        Ok(source)
    }

    // Accept one client per callback; false means the nonblocking listener is drained.
    fn watch(
        display: BorrowedFd<'_>,
        listener: Option<BorrowedFd<'_>>,
        wake: impl Fn() -> bool + Send + 'static,
        accept: impl FnMut(&Mutex<OutputInterest>) -> io::Result<bool> + Send + 'static,
    ) -> io::Result<Self> {
        Self::watch_with_poll(display, listener, wake, accept, |fds| {
            // No timeout: only readiness, interest changes, rearm, or shutdown wake us.
            let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
            if result < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        })
    }

    fn watch_with_poll(
        display: BorrowedFd<'_>,
        listener: Option<BorrowedFd<'_>>,
        wake: impl Fn() -> bool + Send + 'static,
        mut accept: impl FnMut(&Mutex<OutputInterest>) -> io::Result<bool> + Send + 'static,
        mut poll: impl FnMut(&mut [libc::pollfd]) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        let display = display.try_clone_to_owned()?;
        let listener = listener.map(|fd| fd.try_clone_to_owned()).transpose()?;
        let (control, mut reader) = UnixStream::pair()?;
        control.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let armed = Arc::new(AtomicBool::new(true));
        let status = Arc::new(SourceStatus::default());
        let output = Arc::new(Mutex::new(OutputInterest::default()));
        let thread_armed = armed.clone();
        let thread_status = status.clone();
        let thread_output = output.clone();
        let thread = std::thread::Builder::new()
            .name("wayland-readiness".into())
            .spawn(move || {
                'watch: while !thread_status.stopping.load(Ordering::Acquire) {
                    let armed = thread_armed.load(Ordering::Acquire);
                    // Keep each socket alive until poll returns, even if the setter prunes it.
                    let writable: Vec<_> = if armed {
                        let output = thread_output.lock().unwrap();
                        output
                            .blocked
                            .iter()
                            .filter_map(|id| {
                                output
                                    .clients
                                    .get(id)
                                    .map(|stream| (id.clone(), stream.clone()))
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let mut fds = vec![
                        libc::pollfd {
                            fd: reader.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: listener.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                            events: libc::POLLIN,
                            revents: 0,
                        },
                        libc::pollfd {
                            fd: if armed { display.as_raw_fd() } else { -1 },
                            events: libc::POLLIN,
                            revents: 0,
                        },
                    ];
                    fds.extend(writable.iter().map(|(_, stream)| libc::pollfd {
                        fd: stream.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    }));
                    match thread_status.retry_interrupted(|| poll(&mut fds)) {
                        Ok(Some(())) => {}
                        Ok(None) => break,
                        Err(error) => {
                            thread_status
                                .fail(format!("Wayland readiness wait failed: {error}"), &wake);
                            break;
                        }
                    }
                    if thread_status.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0)
                        || fds[..3]
                            .iter()
                            .any(|fd| fd.revents & (libc::POLLERR | libc::POLLHUP) != 0)
                    {
                        thread_status.fail(
                            "Wayland readiness poll reported an invalid or closed source".into(),
                            &wake,
                        );
                        break;
                    }
                    if fds[0].revents != 0 {
                        let mut bytes = [0; 64];
                        while matches!(reader.read(&mut bytes), Ok(count) if count > 0) {}
                    }
                    if thread_status.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let mut ready = fds[2].revents != 0;
                    if fds[1].revents != 0 {
                        // Wake dispatch before a connection flood can keep this drain busy.
                        // Remaining connections keep the listener level-readable; no timer.
                        for _ in 0..MAX_ACCEPTS_PER_PASS {
                            match thread_status.retry_interrupted(|| accept(&thread_output)) {
                                Ok(Some(true)) => {}
                                Ok(Some(false)) => break,
                                Ok(None) => break 'watch,
                                Err(error) => {
                                    thread_status
                                        .fail(format!("Wayland accept failed: {error}"), &wake);
                                    break 'watch;
                                }
                            }
                        }
                        ready = true;
                    }
                    if !writable.is_empty() {
                        let output = thread_output.lock().unwrap();
                        ready |= writable.iter().zip(&fds[3..]).any(|((id, _), fd)| {
                            fd.revents != 0
                                && output.blocked.contains(id)
                                && output.clients.contains_key(id)
                        });
                    }
                    if ready && !thread_status.stopping.load(Ordering::Acquire) {
                        // Level readiness must be acknowledged after dispatch, not spun on.
                        if thread_armed.swap(false, Ordering::AcqRel) && !wake() {
                            // The callback just rejected a wake; do not retry it in a loop.
                            thread_status.record_failure(
                                "Wayland readiness wake callback rejected notification".into(),
                            );
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            control,
            armed,
            status,
            output,
            backend_handle: None,
            thread: Some(thread),
        })
    }

    /// Update after collecting active IDs and flushing clients, outside backend callbacks.
    /// Only clients whose individual flush returned WouldBlock need write readiness.
    /// This changes interest without acknowledging readiness; call rearm separately afterward.
    pub fn set_output_interest(&self, active: &[ClientId], blocked: &[ClientId]) {
        let active: HashSet<_> = active.iter().cloned().collect();
        let mut removed: HashSet<_> = {
            let output = self.output.lock().unwrap();
            output
                .clients
                .keys()
                .filter(|id| !active.contains(*id))
                .cloned()
                .collect()
        };
        if !removed.is_empty()
            && let Some(handle) = &self.backend_handle
        {
            // The caller's snapshot may predate a concurrent accept. Never hold the
            // source mutex while acquiring the backend lock (or inside its callback).
            handle.with_all_clients(|id| {
                removed.remove(&id);
            });
        }
        let blocked = blocked
            .iter()
            .filter(|id| active.contains(*id))
            .cloned()
            .collect();
        let changed = {
            let mut output = self.output.lock().unwrap();
            let previous_count = output.clients.len();
            output.clients.retain(|id, _| !removed.contains(id));
            let changed = previous_count != output.clients.len() || output.blocked != blocked;
            // Keep IDs even if accept has not installed their monitoring socket yet.
            output.blocked = blocked;
            changed
        };
        if changed {
            self.notify();
        }
    }

    pub fn rearm(&self) {
        if !self.armed.swap(true, Ordering::AcqRel) {
            self.notify();
        }
    }

    /// Consume a terminal readiness failure once. Check on wake before dispatch;
    /// on Some(error), log it and exit the event loop. Normal stop creates no failure.
    pub fn take_failure(&self) -> Option<String> {
        self.status.take_failure()
    }

    fn notify(&self) {
        loop {
            match (&self.control).write(&[1]) {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    log::debug!("Wayland readiness notification failed: {error}");
                    break;
                }
            }
        }
    }

    pub fn stop(&mut self) {
        self.status.stop();
        self.notify();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let mut output = self.output.lock().unwrap();
        output.clients.clear();
        output.blocked.clear();
    }
}

impl Drop for WaylandSource {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::wayland_server::Display;
    use smithay::reexports::wayland_server::backend::DisconnectReason;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Weak, mpsc};
    use std::time::{Duration, Instant};

    fn watch_display(display: &Display<()>, tx: mpsc::Sender<()>) -> WaylandSource {
        let mut source = WaylandSource::watch(
            display.as_fd(),
            None,
            move || tx.send(()).is_ok(),
            |_| Ok(false),
        )
        .unwrap();
        source.backend_handle = Some(display.handle().backend_handle());
        source
    }

    fn track_client(
        display: &Display<()>,
        source: &WaylandSource,
        server: &UnixStream,
    ) -> (ClientId, Weak<UnixStream>) {
        let id = display
            .handle()
            .insert_client(server.try_clone().unwrap(), Arc::new(()))
            .unwrap()
            .id();
        let monitor = Arc::new(server.try_clone().unwrap());
        let weak = Arc::downgrade(&monitor);
        source
            .output
            .lock()
            .unwrap()
            .clients
            .insert(id.clone(), monitor);
        (id, weak)
    }

    fn fill_send_buffer(server: &mut UnixStream) {
        server.set_nonblocking(true).unwrap();
        let size = 4096_i32;
        let result = unsafe {
            libc::setsockopt(
                server.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const i32).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            )
        };
        assert_eq!(result, 0, "{}", io::Error::last_os_error());
        let bytes = [0; 4096];
        let mut pending = &bytes[..];
        loop {
            match server.write(pending) {
                Ok(0) => panic!("socket stopped accepting data before WouldBlock"),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && pending.len() > 1 => {
                    pending = &bytes[..1];
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) => panic!("could not fill socket: {error}"),
            }
        }
    }

    fn drain_socket(client: &mut UnixStream) {
        client.set_nonblocking(true).unwrap();
        let mut bytes = [0; 8192];
        loop {
            match client.read(&mut bytes) {
                Ok(0) => panic!("socket closed before its output could be drained"),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) => panic!("could not drain socket: {error}"),
            }
        }
    }

    fn send_sync(client: &mut UnixStream, id: u32) {
        let mut message = Vec::new();
        for word in [1_u32, 12 << 16, id] {
            message.extend_from_slice(&word.to_ne_bytes());
        }
        client.write_all(&message).unwrap();
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "readiness thread did not make progress"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn readiness_wakes_once_until_dispatch_acknowledges_it() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let source = WaylandSource::watch(
            server.as_fd(),
            None,
            move || tx.send(()).is_ok(),
            |_| Ok(false),
        )
        .unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        client.write_all(&[1]).unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        server.read_exact(&mut [0]).unwrap();
        source.rearm();
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        client.write_all(&[2]).unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn stop_interrupts_an_idle_wait() {
        let (_client, server) = UnixStream::pair().unwrap();
        let mut source =
            WaylandSource::watch(server.as_fd(), None, || true, |_| Ok(false)).unwrap();
        source.stop();
        assert!(source.thread.is_none());
        assert!(source.take_failure().is_none());
    }

    #[test]
    fn accept_failure_is_durable_and_wakes_once_even_when_disarmed() {
        let (_client, server) = UnixStream::pair().unwrap();
        let (mut signal, listener) = UnixStream::pair().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = attempts.clone();
        let (tx, rx) = mpsc::channel();
        let mut source = WaylandSource::watch(
            server.as_fd(),
            Some(listener.as_fd()),
            move || tx.send(()).is_ok(),
            move |_| {
                worker_attempts.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::from_raw_os_error(libc::EMFILE))
            },
        )
        .unwrap();
        source.armed.store(false, Ordering::Release);
        signal.write_all(&[1]).unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        wait_until(|| source.thread.as_ref().unwrap().is_finished());
        assert!(
            source
                .take_failure()
                .unwrap()
                .contains("Wayland accept failed")
        );
        assert!(source.take_failure().is_none());
        source.rearm();
        source.stop();
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert!(source.take_failure().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn interrupted_accept_retries_then_preserves_readiness() {
        let (_client, server) = UnixStream::pair().unwrap();
        let (mut signal, mut listener) = UnixStream::pair().unwrap();
        let listener_fd = listener.as_fd().try_clone_to_owned().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = attempts.clone();
        let (tx, rx) = mpsc::channel();
        let mut source = WaylandSource::watch(
            server.as_fd(),
            Some(listener_fd.as_fd()),
            move || tx.send(()).is_ok(),
            move |_| {
                if worker_attempts.fetch_add(1, Ordering::Relaxed) < 2 {
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                listener.read_exact(&mut [0])?;
                Ok(false)
            },
        )
        .unwrap();
        signal.write_all(&[1]).unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
        assert!(source.take_failure().is_none());
        source.stop();
        assert!(source.take_failure().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn accept_quantum_wakes_before_draining_a_backlogged_listener() {
        let (_client, server) = UnixStream::pair().unwrap();
        let (mut signal, mut listener) = UnixStream::pair().unwrap();
        let listener_fd = listener.as_fd().try_clone_to_owned().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = attempts.clone();
        let wake_attempts = attempts.clone();
        let (tx, rx) = mpsc::channel();
        let mut source = WaylandSource::watch(
            server.as_fd(),
            Some(listener_fd.as_fd()),
            move || tx.send(wake_attempts.load(Ordering::Relaxed)).is_ok(),
            move |_| {
                if worker_attempts.fetch_add(1, Ordering::Relaxed) < MAX_ACCEPTS_PER_PASS {
                    return Ok(true);
                }
                listener.read_exact(&mut [0])?;
                Ok(false)
            },
        )
        .unwrap();
        signal.write_all(&[1]).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            MAX_ACCEPTS_PER_PASS,
        );
        wait_until(|| attempts.load(Ordering::Relaxed) == MAX_ACCEPTS_PER_PASS + 1);
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        source.stop();
        assert!(source.take_failure().is_none());
    }

    #[test]
    fn poll_failure_is_recorded_before_one_wake_without_retry() {
        let (_client, server) = UnixStream::pair().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = attempts.clone();
        let (tx, rx) = mpsc::channel();
        let mut source = WaylandSource::watch_with_poll(
            server.as_fd(),
            None,
            move || tx.send(()).is_ok(),
            |_| panic!("poll failure must not attempt accept"),
            move |_| {
                worker_attempts.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::from_raw_os_error(libc::EIO))
            },
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            source
                .take_failure()
                .unwrap()
                .contains("Wayland readiness wait failed")
        );
        assert!(source.take_failure().is_none());
        source.stop();
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn normal_stop_suppresses_a_concurrent_poll_error() {
        let (_client, server) = UnixStream::pair().unwrap();
        let (started, started_rx) = mpsc::channel();
        let (finish, finish_rx) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let mut source = WaylandSource::watch_with_poll(
            server.as_fd(),
            None,
            move || tx.send(()).is_ok(),
            |_| Ok(false),
            move |_| {
                started.send(()).unwrap();
                finish_rx.recv_timeout(Duration::from_secs(1)).unwrap();
                Err(io::Error::from_raw_os_error(libc::EIO))
            },
        )
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        source.status.stop();
        finish.send(()).unwrap();
        source.stop();
        assert!(source.take_failure().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn normal_stop_cancels_a_continuously_accepting_callback() {
        let (_client, server) = UnixStream::pair().unwrap();
        let (mut signal, listener) = UnixStream::pair().unwrap();
        let (started, started_rx) = mpsc::channel();
        let mut announced = false;
        let (tx, rx) = mpsc::channel();
        let mut source = WaylandSource::watch(
            server.as_fd(),
            Some(listener.as_fd()),
            move || tx.send(()).is_ok(),
            move |_| {
                if !announced {
                    announced = true;
                    started.send(()).unwrap();
                }
                std::thread::yield_now();
                Ok(true)
            },
        )
        .unwrap();
        signal.write_all(&[1]).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        source.stop();
        assert!(source.take_failure().is_none());
        // The first bounded quantum may have posted its ordinary readiness wake.
        assert!(rx.try_iter().count() <= 1);
    }

    #[test]
    fn repeated_poll_interruptions_recover_without_losing_readiness() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&[1]).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = attempts.clone();
        let (tx, rx) = mpsc::channel();
        let mut source = WaylandSource::watch_with_poll(
            server.as_fd(),
            None,
            move || tx.send(()).is_ok(),
            |_| panic!("interrupted poll must not attempt accept"),
            move |fds| {
                if worker_attempts.fetch_add(1, Ordering::Relaxed) < 20 {
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
                if result < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(source.take_failure().is_none());
        source.stop();
        assert!(attempts.load(Ordering::Relaxed) >= 21);
        assert!(source.take_failure().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn invalid_poll_descriptor_reports_failure_instead_of_hotlooping() {
        let (_client, server) = UnixStream::pair().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = attempts.clone();
        let (tx, rx) = mpsc::channel();
        let mut source = WaylandSource::watch_with_poll(
            server.as_fd(),
            None,
            move || tx.send(()).is_ok(),
            |_| Ok(false),
            move |fds| {
                worker_attempts.fetch_add(1, Ordering::Relaxed);
                fds[2].revents = libc::POLLNVAL;
                Ok(())
            },
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            source
                .take_failure()
                .unwrap()
                .contains("invalid or closed source")
        );
        source.stop();
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert!(source.take_failure().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn rejected_readiness_wake_records_failure_without_retrying_callback() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let worker_wakes = wakes.clone();
        let mut source = WaylandSource::watch(
            server.as_fd(),
            None,
            move || {
                worker_wakes.fetch_add(1, Ordering::Relaxed);
                false
            },
            |_| Ok(false),
        )
        .unwrap();
        client.write_all(&[1]).unwrap();
        wait_until(|| source.thread.as_ref().unwrap().is_finished());
        assert!(
            source
                .take_failure()
                .unwrap()
                .contains("wake callback rejected")
        );
        source.rearm();
        source.stop();
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(source.take_failure().is_none());
    }

    #[test]
    fn rejected_failure_wake_keeps_diagnostic_without_retrying_callback() {
        let (_client, server) = UnixStream::pair().unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let worker_wakes = wakes.clone();
        let mut source = WaylandSource::watch_with_poll(
            server.as_fd(),
            None,
            move || {
                worker_wakes.fetch_add(1, Ordering::Relaxed);
                false
            },
            |_| Ok(false),
            |_| Err(io::Error::from_raw_os_error(libc::EIO)),
        )
        .unwrap();
        wait_until(|| source.thread.as_ref().unwrap().is_finished());
        assert!(
            source
                .take_failure()
                .unwrap()
                .contains("Wayland readiness wait failed")
        );
        source.rearm();
        source.stop();
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(source.take_failure().is_none());
    }

    #[test]
    fn wayland_backend_fd_wakes_for_real_protocol_requests() {
        let mut display = smithay::reexports::wayland_server::Display::<()>::new().unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        display
            .handle()
            .insert_client(server, Arc::new(()))
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let source = WaylandSource::watch(
            display.as_fd(),
            None,
            move || tx.send(()).is_ok(),
            |_| Ok(false),
        )
        .unwrap();
        // wl_display.sync, new callback ID 2. The backend handles this core request.
        for id in [2_u32, 3] {
            let mut message = Vec::new();
            for word in [1_u32, 12 << 16, id] {
                message.extend_from_slice(&word.to_ne_bytes());
            }
            client.write_all(&message).unwrap();
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
            display.dispatch_clients(&mut ()).unwrap();
            display.flush_clients().unwrap();
            source.rearm();
        }
    }

    #[test]
    fn bounded_dispatch_continuations_do_not_replenish_readiness_before_native_yield() {
        let mut display = Display::<()>::new().unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        display
            .handle()
            .insert_client(server, Arc::new(()))
            .unwrap();
        let (tx, rx) = mpsc::channel();
        let source = watch_display(&display, tx);
        for id in 2..66 {
            send_sync(&mut client, id);
        }
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut dispatched = 0;
        let mut native_turns = 0;
        loop {
            let outcome = display
                .backend()
                .dispatch_all_clients_with_budget(&mut (), 4, Duration::from_secs(1), 2)
                .unwrap();
            assert!(outcome.dispatched <= 4);
            dispatched += outcome.dispatched;
            native_turns += 1;
            assert!(native_turns <= 65);
            // Model returning control to AppKit between bounded turns. Even if
            // the kernel FD remains readable, no proxy wake may replenish here.
            assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            if !outcome.more_pending {
                break;
            }
        }
        assert_eq!(dispatched, 64);
        assert!(native_turns > 1);
        display.flush_clients().unwrap();
        source.rearm(); // Only the native turn's AboutToWait acknowledges readiness.
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        send_sync(&mut client, 66);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn blocked_output_wakes_once_until_dispatch_acknowledges_it() {
        let mut display = Display::<()>::new().unwrap();
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let (_healthy_client, healthy_server) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let source = watch_display(&display, tx);
        let (id, _) = track_client(&display, &source, &server);
        let (healthy_id, _) = track_client(&display, &source, &healthy_server);
        let active = [id.clone(), healthy_id];
        fill_send_buffer(&mut server);

        send_sync(&mut client, 2);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        display.dispatch_clients(&mut ()).unwrap();
        assert_eq!(
            display
                .backend()
                .flush(Some(id.clone()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock,
        );
        source.set_output_interest(&active, std::slice::from_ref(&id));
        source.rearm();
        // The healthy socket is writable, but only the full socket is watched.
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());

        drain_socket(&mut client);
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        source.set_output_interest(&active, std::slice::from_ref(&id));
        send_sync(&mut client, 3);
        // Read and write readiness share the same pending acknowledgment.
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());

        source.rearm();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        display.dispatch_clients(&mut ()).unwrap();
        display.backend().flush(Some(id)).unwrap();
        source.set_output_interest(&active, &[]);
        source.rearm();
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
    }

    #[test]
    fn blocked_interest_survives_late_monitor_registration() {
        let display = Display::<()>::new().unwrap();
        let (_client, server) = UnixStream::pair().unwrap();
        let id = display
            .handle()
            .insert_client(server.try_clone().unwrap(), Arc::new(()))
            .unwrap()
            .id();
        let monitor = Arc::new(server.try_clone().unwrap());
        let (mut signal, mut listener) = UnixStream::pair().unwrap();
        let listener_fd = listener.as_fd().try_clone_to_owned().unwrap();
        let accepted_id = id.clone();
        let (tx, rx) = mpsc::channel();
        let source = WaylandSource::watch(
            display.as_fd(),
            Some(listener_fd.as_fd()),
            move || tx.send(()).is_ok(),
            move |output| {
                listener.read_exact(&mut [0]).unwrap();
                output
                    .lock()
                    .unwrap()
                    .clients
                    .insert(accepted_id.clone(), monitor.clone());
                Ok(false)
            },
        )
        .unwrap();
        source.set_output_interest(std::slice::from_ref(&id), std::slice::from_ref(&id));
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        signal.write_all(&[1]).unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        source.rearm();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        source.set_output_interest(std::slice::from_ref(&id), &[]);
        source.rearm();
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
    }

    #[test]
    fn stale_active_snapshot_does_not_prune_an_accepted_client() {
        let display = Display::<()>::new().unwrap();
        let handle = display.handle().backend_handle();
        let mut active = Vec::new();
        handle.with_all_clients(|id| active.push(id));
        let (_client, server) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let source = watch_display(&display, tx);
        let (id, monitor) = track_client(&display, &source, &server);

        source.set_output_interest(&active, &[]);
        assert!(source.output.lock().unwrap().clients.contains_key(&id));
        assert!(monitor.upgrade().is_some());
        handle.kill_client(id, DisconnectReason::ConnectionClosed);
        source.set_output_interest(&[], &[]);
        assert!(source.output.lock().unwrap().clients.is_empty());
        assert!(monitor.upgrade().is_none());
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
    }

    #[test]
    fn pruning_releases_a_descriptor_held_by_a_poll_snapshot() {
        let display = Display::<()>::new().unwrap();
        let (_client, mut server) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let source = watch_display(&display, tx);
        let (id, monitor) = track_client(&display, &source, &server);
        fill_send_buffer(&mut server);
        source.set_output_interest(std::slice::from_ref(&id), std::slice::from_ref(&id));
        wait_until(|| monitor.strong_count() == 2);

        display
            .handle()
            .backend_handle()
            .kill_client(id, DisconnectReason::ConnectionClosed);
        source.set_output_interest(&[], &[]);
        wait_until(|| monitor.strong_count() == 0);
        let output = source.output.lock().unwrap();
        assert!(output.clients.is_empty());
        assert!(output.blocked.is_empty());
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
    }

    #[test]
    fn canceled_interest_ignores_writability_from_an_old_poll_snapshot() {
        let display = Display::<()>::new().unwrap();
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let source = watch_display(&display, tx);
        let (id, monitor) = track_client(&display, &source, &server);
        fill_send_buffer(&mut server);
        source.set_output_interest(std::slice::from_ref(&id), std::slice::from_ref(&id));
        wait_until(|| monitor.strong_count() == 2);
        {
            // Prevent revalidation until the now-writable socket's interest is canceled.
            let mut output = source.output.lock().unwrap();
            drain_socket(&mut client);
            output.blocked.clear();
            source.notify();
        }
        wait_until(|| monitor.strong_count() == 1);
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        assert!(source.armed.load(Ordering::Acquire));
    }

    #[test]
    fn stop_interrupts_a_blocked_output_wait_and_releases_descriptors() {
        let display = Display::<()>::new().unwrap();
        let (_client, mut server) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let mut source = watch_display(&display, tx);
        let (id, monitor) = track_client(&display, &source, &server);
        fill_send_buffer(&mut server);
        source.set_output_interest(std::slice::from_ref(&id), std::slice::from_ref(&id));
        wait_until(|| monitor.strong_count() == 2);
        source.stop();
        assert!(source.thread.is_none());
        assert!(monitor.upgrade().is_none());
        assert!(source.output.lock().unwrap().blocked.is_empty());
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
    }
}
