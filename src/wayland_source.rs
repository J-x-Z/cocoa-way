use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use smithay::reexports::wayland_server::backend::{ClientId, Handle};
use smithay::reexports::wayland_server::{DisplayHandle, ListeningSocket};
use winit::event_loop::EventLoopProxy;

#[derive(Default)]
struct OutputInterest {
    clients: HashMap<ClientId, Arc<UnixStream>>,
    blocked: HashSet<ClientId>,
}

/// The readiness thread never dispatches client code; AppKit stays on the main thread.
pub struct WaylandSource {
    control: UnixStream,
    armed: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
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
                loop {
                    match listener.accept() {
                        Ok(Some(stream)) => {
                            log::info!("New client connected");
                            let monitor = match stream.try_clone() {
                                Ok(monitor) => Arc::new(monitor),
                                Err(error) => {
                                    log::warn!("Could not monitor a Wayland client: {error}");
                                    continue;
                                }
                            };
                            match handle.insert_client(
                                stream,
                                Arc::new(crate::state::ClientState {
                                    compositor_state: Default::default(),
                                }),
                            ) {
                                Ok(client) => {
                                    // Backend registration finishes before taking the source lock.
                                    output.lock().unwrap().clients.insert(client.id(), monitor);
                                }
                                Err(error) => {
                                    log::warn!("Could not register a Wayland client: {error}");
                                }
                            }
                        }
                        Ok(None) => return true,
                        Err(error) => {
                            log::error!("Wayland accept failed: {error}");
                            return false;
                        }
                    }
                }
            },
        )?;
        source.backend_handle = Some(backend_handle);
        Ok(source)
    }

    fn watch(
        display: BorrowedFd<'_>,
        listener: Option<BorrowedFd<'_>>,
        wake: impl Fn() -> bool + Send + 'static,
        mut accept: impl FnMut(&Mutex<OutputInterest>) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let display = display.try_clone_to_owned()?;
        let listener = listener.map(|fd| fd.try_clone_to_owned()).transpose()?;
        let (control, mut reader) = UnixStream::pair()?;
        control.set_nonblocking(true)?;
        reader.set_nonblocking(true)?;
        let armed = Arc::new(AtomicBool::new(true));
        let stopping = Arc::new(AtomicBool::new(false));
        let output = Arc::new(Mutex::new(OutputInterest::default()));
        let thread_armed = armed.clone();
        let thread_stopping = stopping.clone();
        let thread_output = output.clone();
        let thread = std::thread::Builder::new()
            .name("wayland-readiness".into())
            .spawn(move || {
                while !thread_stopping.load(Ordering::Acquire) {
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
                    // No timeout: only readiness, interest changes, rearm, or shutdown wake us.
                    let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
                    if result < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        log::error!("Wayland readiness wait failed: {error}");
                        break;
                    }
                    if fds[0].revents != 0 {
                        let mut bytes = [0; 64];
                        while matches!(reader.read(&mut bytes), Ok(count) if count > 0) {}
                    }
                    if thread_stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let mut ready = fds[2].revents != 0;
                    if fds[1].revents != 0 {
                        if !accept(&thread_output) {
                            break;
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
                    if ready {
                        // Level readiness must be acknowledged after dispatch, not spun on.
                        if thread_armed.swap(false, Ordering::AcqRel) && !wake() {
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            control,
            armed,
            stopping,
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
        self.stopping.store(true, Ordering::Release);
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
    use std::sync::{Weak, mpsc};
    use std::time::{Duration, Instant};

    fn watch_display(display: &Display<()>, tx: mpsc::Sender<()>) -> WaylandSource {
        let mut source =
            WaylandSource::watch(display.as_fd(), None, move || tx.send(()).is_ok(), |_| true)
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
        let source =
            WaylandSource::watch(server.as_fd(), None, move || tx.send(()).is_ok(), |_| true)
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
        let (_, server) = UnixStream::pair().unwrap();
        let mut source = WaylandSource::watch(server.as_fd(), None, || true, |_| true).unwrap();
        source.stop();
        assert!(source.thread.is_none());
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
        let source =
            WaylandSource::watch(display.as_fd(), None, move || tx.send(()).is_ok(), |_| true)
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
                true
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
