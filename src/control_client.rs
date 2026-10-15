use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::control_protocol::{ControlRequest, ControlResponse};

pub(crate) const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub(crate) const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

pub fn default_socket_path() -> PathBuf {
    std::env::var_os("COCOA_WAY_CONTROL_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("cocoa-way")
                .join("control")
                .join("control.sock")
        })
}

pub fn send_request(socket: &Path, request: &ControlRequest) -> Result<ControlResponse, String> {
    send_request_with_timeout(socket, request, REQUEST_TIMEOUT)
}

pub(crate) fn send_request_with_timeout(
    socket: &Path,
    request: &ControlRequest,
    timeout: Duration,
) -> Result<ControlResponse, String> {
    let deadline = Instant::now() + timeout;
    let mut encoded = serde_json::to_vec(request)
        .map_err(|error| format!("failed to encode request: {error}"))?;
    if encoded.len() > MAX_REQUEST_BYTES {
        return Err("control request exceeds the 64 KiB frame limit".into());
    }
    encoded.push(b'\n');
    let mut stream =
        connect_before(socket, deadline.min(Instant::now() + WRITE_TIMEOUT)).map_err(|error| {
            format!(
                "cannot connect to Cocoa-Way at {}: {error}. Start Cocoa-Way first.",
                socket.display()
            )
        })?;
    write_before(
        &mut stream,
        &encoded,
        deadline.min(Instant::now() + WRITE_TIMEOUT),
    )
    .map_err(|error| format!("failed to send request: {error}"))?;

    let response = read_frame_before(&mut stream, MAX_RESPONSE_BYTES, deadline)
        .map_err(|error| format!("failed to read response: {error}"))?;
    serde_json::from_slice(&response).map_err(|error| format!("invalid server response: {error}"))
}

fn timeout_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "control I/O deadline exceeded")
}

fn wait_ready(stream: &UnixStream, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(timeout_error)?;
        let mut pollfd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events,
            revents: 0,
        };
        let milliseconds = remaining
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        let result = unsafe { libc::poll(&mut pollfd, 1, milliseconds) };
        if result > 0 {
            if Instant::now() >= deadline {
                return Err(timeout_error());
            }
            return Ok(());
        }
        if result == 0 {
            return Err(timeout_error());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub(crate) fn connect_before(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Unix socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as u8;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    #[cfg(target_os = "macos")]
    {
        let enabled: libc::c_int = 1;
        if unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&enabled as *const libc::c_int).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    stream.set_nonblocking(true)?;
    let result =
        unsafe { libc::connect(fd, (&address as *const libc::sockaddr_un).cast(), length) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        wait_ready(&stream, libc::POLLOUT, deadline)?;
        if let Some(error) = stream.take_error()? {
            return Err(error);
        }
    }
    if Instant::now() >= deadline {
        return Err(timeout_error());
    }
    Ok(stream)
}

// Nonblocking syscalls plus an absolute deadline bound even continuously trickling peers.
pub(crate) fn read_frame_before(
    stream: &mut UnixStream,
    limit: usize,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    stream.set_nonblocking(true)?;
    let mut frame = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        wait_ready(stream, libc::POLLIN, deadline)?;
        let count = match stream.read(&mut buffer) {
            Ok(0) if frame.is_empty() => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "empty control frame",
                ));
            }
            Ok(0) => return Ok(frame),
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let newline = buffer[..count].iter().position(|byte| *byte == b'\n');
        let end = newline.unwrap_or(count);
        if end > limit.saturating_sub(frame.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control frame exceeds byte limit",
            ));
        }
        frame.extend_from_slice(&buffer[..end]);
        if newline.is_some() {
            return Ok(frame);
        }
    }
}

pub(crate) fn write_before(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    while !bytes.is_empty() {
        wait_ready(stream, libc::POLLOUT, deadline)?;
        match stream.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "closed control socket",
                ));
            }
            Ok(count) => bytes = &bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
