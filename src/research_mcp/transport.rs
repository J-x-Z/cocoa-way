use super::Server;

pub const MAX_FRAME: usize = 64 * 1024;
const MAX_RESPONSE: usize = 128 * 1024;

#[derive(Default)]
pub struct Frames {
    bytes: Vec<u8>,
    pub eof: bool,
}

impl Frames {
    pub fn remaining(&self) -> usize {
        (MAX_FRAME + 1).saturating_sub(self.bytes.len())
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        if bytes.len() > self.remaining() {
            return Err("stdin_frame_limit_exceeded");
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    pub fn ready(&self) -> bool {
        self.bytes.contains(&b'\n') || self.eof || self.remaining() == 0
    }

    pub fn next(&mut self) -> Result<Option<Vec<u8>>, &'static str> {
        let length = match self.bytes.iter().position(|byte| *byte == b'\n') {
            Some(index) => index,
            None if self.bytes.len() > MAX_FRAME => return Err("stdin_frame_limit_exceeded"),
            None if self.eof && !self.bytes.is_empty() => self.bytes.len(),
            None => return Ok(None),
        };
        if length > MAX_FRAME {
            return Err("stdin_frame_limit_exceeded");
        }
        let frame = self.bytes[..length].to_vec();
        let consumed = (length + 1).min(self.bytes.len());
        self.bytes.drain(..consumed);
        Ok(Some(frame))
    }
}

#[cfg(unix)]
pub use unix::{set_nonblocking, shutdown_requested};

#[cfg(unix)]
pub fn serve(server: Server) -> Result<(), &'static str> {
    unix::serve(server)
}

#[cfg(not(unix))]
pub fn serve(mut server: Server) -> Result<(), &'static str> {
    use std::io::{Read, Write};
    // No child can launch on this platform, so blocking stdio has no TTL work.
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut frames = Frames::default();
    let mut byte = [0u8];
    loop {
        match input.read(&mut byte).map_err(|_| "stdin_read_failed")? {
            0 => frames.eof = true,
            _ => frames.push(&byte)?,
        }
        if let Some(frame) = frames.next()? {
            if let Some(response) = server.handle(&frame) {
                let encoded =
                    serde_json::to_vec(&response).map_err(|_| "response_encoding_failed")?;
                if encoded.len() > MAX_RESPONSE {
                    return Err("response_limit_exceeded");
                }
                output
                    .write_all(&encoded)
                    .and_then(|_| output.write_all(b"\n"))
                    .and_then(|_| output.flush())
                    .map_err(|_| "stdout_write_failed")?;
            }
        }
        if frames.eof {
            return Ok(());
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::{Frames, MAX_RESPONSE, Server};

    static SHUTDOWN: AtomicBool = AtomicBool::new(false);

    extern "C" fn signal_handler(_: libc::c_int) {
        SHUTDOWN.store(true, Ordering::Relaxed);
    }

    pub fn shutdown_requested() -> bool {
        SHUTDOWN.load(Ordering::Relaxed)
    }

    struct Signals(Vec<(libc::c_int, libc::sigaction)>);

    impl Signals {
        fn install() -> io::Result<Self> {
            SHUTDOWN.store(false, Ordering::Relaxed);
            let mut saved = Self(Vec::new());
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGCHLD] {
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
                // Retain waitable leaders even if our parent ignored SIGCHLD.
                action.sa_sigaction = if signal == libc::SIGCHLD {
                    libc::SIG_DFL
                } else {
                    signal_handler as *const () as usize
                };
                unsafe {
                    libc::sigemptyset(&mut action.sa_mask);
                }
                if unsafe { libc::sigaction(signal, &action, &mut previous) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                saved.0.push((signal, previous));
            }
            Ok(saved)
        }
    }

    impl Drop for Signals {
        fn drop(&mut self) {
            for (signal, previous) in &self.0 {
                unsafe {
                    libc::sigaction(*signal, previous, std::ptr::null_mut());
                }
            }
        }
    }

    pub fn set_nonblocking(fd: libc::c_int) -> io::Result<libc::c_int> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(flags)
    }

    struct StdioFlags(Vec<(libc::c_int, libc::c_int)>);

    impl StdioFlags {
        fn install() -> io::Result<Self> {
            let mut saved = Self(Vec::new());
            for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
                saved.0.push((fd, set_nonblocking(fd)?));
            }
            Ok(saved)
        }
    }

    impl Drop for StdioFlags {
        fn drop(&mut self) {
            for (fd, flags) in &self.0 {
                unsafe {
                    libc::fcntl(*fd, libc::F_SETFL, *flags);
                }
            }
        }
    }

    pub fn serve(server: Server) -> Result<(), &'static str> {
        let _signals = Signals::install().map_err(|_| "signal_setup_failed")?;
        let _flags = StdioFlags::install().map_err(|_| "stdio_setup_failed")?;
        // Drop children before restoring handlers/stdio, including on errors.
        let mut server = server;
        let mut frames = Frames::default();
        let mut pending = Vec::new();
        let mut written = 0;
        let mut eof_deadline = None;
        // One bounded loop, no reader/output/timer threads. Nonblocking stdout
        // ensures a client that stops reading cannot prevent child TTL cleanup.
        loop {
            server.children.tick();
            if shutdown_requested() || eof_deadline.is_some_and(|end| Instant::now() >= end) {
                return Ok(());
            }
            if pending.is_empty() {
                if let Some(frame) = frames.next()? {
                    if let Some(response) = server.handle(&frame) {
                        pending = serde_json::to_vec(&response)
                            .map_err(|_| "response_encoding_failed")?;
                        if pending.len() >= MAX_RESPONSE {
                            return Err("response_limit_exceeded");
                        }
                        pending.push(b'\n');
                        written = 0;
                    }
                } else if frames.eof && !server.children.has_active() {
                    return Ok(());
                }
            }
            let mut fds = [
                libc::pollfd {
                    fd: if frames.eof { -1 } else { libc::STDIN_FILENO },
                    events: if frames.remaining() > 0 {
                        libc::POLLIN
                    } else {
                        0
                    },
                    revents: 0,
                },
                libc::pollfd {
                    fd: libc::STDOUT_FILENO,
                    events: if pending.is_empty() { 0 } else { libc::POLLOUT },
                    revents: 0,
                },
            ];
            let timeout = if pending.is_empty() && frames.ready() && !frames.eof {
                0
            } else if !server.children.has_active() && eof_deadline.is_none() {
                // With no children/deadline, idle entirely in poll: no ticks,
                // timer thread, or periodic wakeups just for an idle MCP client.
                -1
            } else {
                50
            };
            let polled =
                unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if polled < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err("stdio_poll_failed");
            }
            if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
                return Err("stdio_closed");
            }
            if fds[1].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                return Ok(());
            }
            if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let mut bytes = [0u8; 4096];
                let capacity = frames.remaining().min(bytes.len());
                if capacity == 0 {
                    return Err("stdin_buffer_limit_exceeded");
                }
                let count =
                    unsafe { libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), capacity) };
                if count == 0 {
                    frames.eof = true;
                    server.closing = true;
                    server.children.shutdown();
                    eof_deadline = Some(
                        Instant::now()
                            + Duration::from_millis(super::super::policy::GUI_STOP_GRACE_MS + 500),
                    );
                } else if count > 0 {
                    frames.push(&bytes[..count as usize])?;
                } else if !matches!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    return Err("stdin_read_failed");
                }
            }
            if !pending.is_empty() && fds[1].revents & libc::POLLOUT != 0 {
                let bytes = &pending[written..];
                let count =
                    unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
                if count > 0 {
                    written += count as usize;
                    if written == pending.len() {
                        pending.clear();
                    }
                } else if count == 0
                    || !matches!(
                        io::Error::last_os_error().kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    )
                {
                    return Err("stdout_write_failed");
                }
            }
        }
    }
}
