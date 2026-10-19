//! Bounded host-side multiplexed relay for both published sockets and stdio pipes.
//!
//! A single nonblocking pump owns all writes: a stalled local consumer cannot
//! hold a transport mutex or stop the opposite direction. The wire format remains
//! OPEN/DATA/CLOSE with a nine-byte big-endian header. Large incoming DATA frames
//! are streamed, not allocated. Timeouts fail the relay explicitly, never silently
//! drop protocol bytes. Callers must tear down the relay child on any error.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const HEADER: usize = 9;
const OPEN: u8 = 1;
const DATA: u8 = 2;
const CLOSE: u8 = 3;
const MAX_FRAME: usize = 16 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy)]
pub struct Limits {
    pub peers: usize,
    pub peer_bytes: usize,
    pub transport_bytes: usize,
    pub chunks: usize,
    pub deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            peers: 128,
            peer_bytes: 256 * 1024,
            transport_bytes: 256 * 1024,
            chunks: 256,
            deadline: Duration::from_secs(10),
        }
    }
}

struct Chunk {
    bytes: Box<[u8]>,
    offset: usize,
    queued: Instant,
}

struct Queue {
    chunks: VecDeque<Chunk>,
    bytes: usize,
    limit: usize,
    chunk_limit: usize,
}

impl Queue {
    fn new(limit: usize, chunk_limit: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes: 0,
            limit,
            chunk_limit,
        }
    }

    fn room(&self) -> usize {
        if self.chunks.len() >= self.chunk_limit {
            0
        } else {
            self.limit - self.bytes
        }
    }

    fn push(&mut self, bytes: Vec<u8>) {
        assert!(bytes.len() <= self.room());
        if bytes.is_empty() {
            return;
        }
        self.bytes += bytes.len();
        self.chunks.push_back(Chunk {
            // A short read must not retain its original 64 KiB allocation while
            // charging only one byte against the queue budget.
            bytes: bytes.into_boxed_slice(),
            offset: 0,
            queued: Instant::now(),
        });
    }

    fn write(&mut self, writer: &mut impl Write) -> io::Result<()> {
        let Some(chunk) = self.chunks.front_mut() else {
            return Ok(());
        };
        match writer.write(&chunk.bytes[chunk.offset..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(length) => {
                chunk.offset += length;
                if chunk.offset == chunk.bytes.len() {
                    self.bytes -= chunk.bytes.len();
                    self.chunks.pop_front();
                }
            }
            Err(error) if retry(&error) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn expired(&self, deadline: Duration) -> bool {
        self.chunks
            .front()
            .is_some_and(|chunk| chunk.queued.elapsed() >= deadline)
    }

    fn empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

struct Peer {
    socket: UnixStream,
    input: Queue,
    connecting: bool,
    retry_at: Option<Instant>,
    opened: Instant,
    host_eof: bool,
    guest_closed: bool,
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

/// Restores descriptor flags even on errors. The caller must own the descriptors;
/// do not run other readers/writers on their duplicated open-file descriptions.
struct Nonblocking {
    fd: RawFd,
    flags: libc::c_int,
}

impl Nonblocking {
    fn new(fd: RawFd) -> io::Result<Self> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, flags })
    }
}

impl Drop for Nonblocking {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(self.fd, libc::F_SETFL, self.flags);
        }
    }
}

fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

fn poll_timeout(deadline: Option<Instant>) -> libc::c_int {
    deadline.map_or(-1, |deadline| {
        let nanos = deadline
            .saturating_duration_since(Instant::now())
            .as_nanos();
        nanos.div_ceil(1_000_000).min(libc::c_int::MAX as u128) as _
    })
}

fn encoded(kind: u8, id: u32, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER + data.len());
    bytes.push(kind);
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
    bytes.extend_from_slice(data);
    bytes
}

fn connect(path: &str, limits: Limits) -> io::Result<Peer> {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path.is_empty() || path.as_bytes().contains(&0) || path.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid local relay socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (destination, source) in address.sun_path.iter_mut().zip(path.bytes()) {
        *destination = source as _;
    }
    #[cfg(target_os = "macos")]
    {
        address.sun_len = std::mem::size_of::<libc::sockaddr_un>() as _;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = unsafe { UnixStream::from_raw_fd(fd) };
    // Neither this descriptor nor its protocol data may leak into guest commands.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    socket.set_nonblocking(true)?;
    let result = unsafe {
        libc::connect(
            fd,
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as _,
        )
    };
    let mut connecting = result != 0;
    let mut retry_at = None;
    if connecting {
        let error = io::Error::last_os_error();
        if matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        ) || error.raw_os_error() == Some(libc::EAGAIN)
        {
            // Waypipe may still be creating its listening socket. Retry without
            // stopping transport reads or discarding this stream's queued DATA.
            connecting = false;
            retry_at = Some(Instant::now() + Duration::from_millis(50));
        } else if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EINTR)
        ) {
            return Err(error);
        }
    }
    Ok(Peer {
        socket,
        input: Queue::new(limits.peer_bytes, limits.chunks),
        connecting,
        retry_at,
        opened: Instant::now(),
        host_eof: false,
        guest_closed: false,
    })
}

#[derive(Default)]
struct Input {
    header: [u8; HEADER],
    used: usize,
    remaining: Option<usize>,
    started: Option<Instant>,
    rejected_open: bool,
}

impl Input {
    fn kind(&self) -> u8 {
        self.header[0]
    }
    fn id(&self) -> u32 {
        u32::from_be_bytes(self.header[1..5].try_into().unwrap())
    }
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn readable(&self, peers: &BTreeMap<u32, Peer>, output: &Queue) -> bool {
        if self.used < HEADER {
            return true;
        }
        match self.kind() {
            // A full destination still blocks later frames in this byte lane.
            // Preserve framing/FIFO; opposite-direction writes remain independent.
            DATA => peers
                .get(&self.id())
                .is_none_or(|peer| peer.guest_closed || peer.input.room() > 0),
            OPEN => !self.rejected_open || output.room() >= HEADER,
            _ => true,
        }
    }

    // Returns false on clean transport EOF; retains incomplete state on EAGAIN.
    fn read(
        &mut self,
        reader: &mut impl Read,
        peers: &mut BTreeMap<u32, Peer>,
        output: &mut Queue,
        path: &str,
        limits: Limits,
    ) -> io::Result<bool> {
        if self.used < HEADER {
            match reader.read(&mut self.header[self.used..]) {
                Ok(0) if self.used == 0 => return Ok(false),
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(length) => {
                    self.started.get_or_insert_with(Instant::now);
                    self.used += length;
                }
                Err(error) if retry(&error) => return Ok(true),
                Err(error) => return Err(error),
            }
            if self.used < HEADER {
                return Ok(true);
            }
        }
        let kind = self.kind();
        let id = self.id();
        let length = *self.remaining.get_or_insert_with(|| {
            u32::from_be_bytes(self.header[5..9].try_into().unwrap()) as usize
        });
        if length > MAX_FRAME
            || !matches!(kind, OPEN | DATA | CLOSE)
            || (kind != DATA && length != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid container relay frame header",
            ));
        }
        match kind {
            OPEN => {
                if !self.rejected_open {
                    if peers.contains_key(&id) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "duplicate relay stream id",
                        ));
                    }
                    if peers.len() < limits.peers {
                        if let Ok(peer) = connect(path, limits) {
                            peers.insert(id, peer);
                            self.reset();
                            return Ok(true);
                        }
                    }
                    // Only a rejected OPEN needs return-lane capacity. Remember
                    // the decision so a blocked CLOSE does not retry the connect.
                    self.rejected_open = true;
                }
                if output.room() < HEADER {
                    return Ok(true);
                }
                output.push(encoded(CLOSE, id, &[]));
                self.reset();
            }
            CLOSE => {
                if let Some(peer) = peers.get_mut(&id) {
                    peer.guest_closed = true;
                }
                self.reset();
            }
            DATA => {
                if length == 0 {
                    self.reset();
                    return Ok(true);
                }
                let peer = peers.get_mut(&id).filter(|peer| !peer.guest_closed);
                let room = peer.as_ref().map_or(CHUNK, |peer| peer.input.room());
                if room == 0 {
                    return Ok(true);
                }
                let mut bytes = vec![0; length.min(CHUNK).min(room)];
                match reader.read(&mut bytes) {
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(count) => {
                        bytes.truncate(count);
                        if let Some(peer) = peer {
                            peer.input.push(bytes);
                        }
                        self.remaining = Some(length - count);
                        if length == count {
                            self.reset();
                        }
                    }
                    Err(error) if retry(&error) => {}
                    Err(error) => return Err(error),
                }
            }
            _ => unreachable!(),
        }
        Ok(true)
    }
}

pub fn bridge<R: Read + AsRawFd, W: Write + AsRawFd>(
    reader: &mut R,
    writer: &mut W,
    local_socket: &str,
) -> Result<(), String> {
    bridge_with_limits(reader, writer, local_socket, Limits::default())
        .map_err(|error| format!("host container relay failed: {error}"))
}

/// Deadline-bound handshake write for an unbuffered socket or child-stdin pipe.
/// Keep this outside the bridge: it must precede framed relay traffic.
pub fn write_before<W: Write + AsRawFd>(
    writer: &mut W,
    bytes: &[u8],
    timeout: Duration,
) -> io::Result<()> {
    let _flags = Nonblocking::new(writer.as_raw_fd())?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut offset = 0;
    while offset < bytes.len() {
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match writer.write(&bytes[offset..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => offset += count,
            Err(error) if retry(&error) => {
                let mut descriptor = libc::pollfd {
                    fd: writer.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let result =
                    unsafe { libc::poll(&mut descriptor, 1, poll_timeout(Some(deadline))) };
                if result < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                } else if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
                {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn bridge_with_limits<R: Read + AsRawFd, W: Write + AsRawFd>(
    reader: &mut R,
    writer: &mut W,
    local_socket: &str,
    limits: Limits,
) -> io::Result<()> {
    if limits.peers == 0
        || limits.peer_bytes == 0
        || limits.transport_bytes <= HEADER
        || limits.chunks == 0
        || limits.deadline.is_zero()
        || Instant::now().checked_add(limits.deadline).is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid relay limits",
        ));
    }
    let _reader_flags = Nonblocking::new(reader.as_raw_fd())?;
    let _writer_flags = Nonblocking::new(writer.as_raw_fd())?;
    let mut peers = BTreeMap::<u32, Peer>::new();
    let mut output = Queue::new(limits.transport_bytes, limits.chunks);
    let mut input = Input::default();
    let mut transport_eof = false;
    let mut rotation = 0usize;
    loop {
        for peer in peers.values_mut() {
            if peer.retry_at.is_some_and(|time| Instant::now() >= time) {
                let mut next = connect(local_socket, limits)?;
                // Keep the original queue and deadline while replacing only the
                // failed descriptor. Dropping `next` closes the old descriptor.
                std::mem::swap(&mut peer.socket, &mut next.socket);
                peer.connecting = next.connecting;
                peer.retry_at = next.retry_at;
            }
        }
        // A completed control header can have waited for output capacity. It
        // must progress even when no further transport bytes arrive to poll.
        if !transport_eof && input.used == HEADER && input.readable(&peers, &output) {
            input.read(reader, &mut peers, &mut output, local_socket, limits)?;
        }
        if output.expired(limits.deadline)
            || input
                .started
                .is_some_and(|time| time.elapsed() >= limits.deadline)
            || peers.values().any(|peer| {
                peer.input.expired(limits.deadline)
                    || ((peer.connecting || peer.retry_at.is_some())
                        && peer.opened.elapsed() >= limits.deadline)
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "relay channel exceeded its queue/frame deadline",
            ));
        }
        let retired = peers
            .iter()
            .filter_map(|(&id, peer)| {
                (peer.input.empty() && (peer.guest_closed || peer.host_eof)).then_some(id)
            })
            .collect::<Vec<_>>();
        for id in retired {
            if !peers[&id].guest_closed {
                if output.room() < HEADER {
                    break;
                }
                output.push(encoded(CLOSE, id, &[]));
            }
            peers.remove(&id);
        }
        // EOF is not permission to discard already accepted DATA. Drain both
        // write directions (under their existing deadlines) before returning.
        if transport_eof && output.empty() && peers.values().all(|peer| peer.input.empty()) {
            return Ok(());
        }
        let can_read = input.readable(&peers, &output);
        let mut polls = vec![
            libc::pollfd {
                fd: if transport_eof {
                    -1
                } else {
                    reader.as_raw_fd()
                },
                events: if can_read { libc::POLLIN } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: if transport_eof && output.empty() {
                    -1
                } else {
                    writer.as_raw_fd()
                },
                events: if !output.empty() { libc::POLLOUT } else { 0 },
                revents: 0,
            },
        ];
        let mut ids = peers.keys().copied().collect::<Vec<_>>();
        // Rotate first-reader priority when a shared output queue is nearly full.
        if !ids.is_empty() {
            let length = ids.len();
            ids.rotate_left(rotation % length);
            rotation = rotation.wrapping_add(1);
        }
        for id in &ids {
            let peer = &peers[id];
            let mut events = 0;
            if peer.retry_at.is_none() && (peer.connecting || !peer.input.empty()) {
                events |= libc::POLLOUT;
            }
            if !transport_eof
                && !peer.connecting
                && peer.retry_at.is_none()
                && !peer.host_eof
                && !peer.guest_closed
                && output.room() > HEADER
            {
                events |= libc::POLLIN;
            }
            polls.push(libc::pollfd {
                fd: if events == 0 {
                    -1
                } else {
                    peer.socket.as_raw_fd()
                },
                events,
                revents: 0,
            });
        }
        let mut next_deadline = input.started.map(|time| time + limits.deadline);
        let mut consider = |time: Instant| {
            next_deadline = Some(next_deadline.map_or(time, |current| current.min(time)));
        };
        if let Some(chunk) = output.chunks.front() {
            consider(chunk.queued + limits.deadline);
        }
        for peer in peers.values() {
            if let Some(chunk) = peer.input.chunks.front() {
                consider(chunk.queued + limits.deadline);
            }
            if peer.connecting || peer.retry_at.is_some() {
                consider(peer.opened + limits.deadline);
            }
            if let Some(retry_at) = peer.retry_at {
                consider(retry_at);
            }
        }
        // Idle relays sleep in poll indefinitely. Only queued writes, partial
        // frames and connection retries need timed wakeups.
        let timeout = poll_timeout(next_deadline);
        if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, timeout) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if polls.iter().any(|poll| poll.revents & libc::POLLNVAL != 0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if polls[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            if !can_read {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            if !input.read(reader, &mut peers, &mut output, local_socket, limits)? {
                transport_eof = true;
            }
        }
        if (!transport_eof || !output.empty())
            && polls[1].revents & (libc::POLLERR | libc::POLLHUP) != 0
        {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if polls[1].revents & libc::POLLOUT != 0 {
            output.write(writer)?;
        }
        for (index, id) in ids.iter().enumerate() {
            let Some(peer) = peers.get_mut(id) else {
                continue;
            };
            let ready = polls[index + 2].revents;
            if peer.connecting {
                if ready & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
                    if let Some(error) = peer.socket.take_error()? {
                        return Err(error);
                    }
                    peer.connecting = false;
                }
                continue;
            }
            if ready & libc::POLLERR != 0 || (ready & libc::POLLHUP != 0 && !peer.input.empty()) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if ready & libc::POLLOUT != 0 {
                peer.input.write(&mut peer.socket)?;
            }
            if ready & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
                && !transport_eof
                && !peer.host_eof
                && !peer.guest_closed
                && output.room() > HEADER
            {
                let mut bytes = vec![0; CHUNK.min(output.room() - HEADER)];
                match peer.socket.read(&mut bytes) {
                    Ok(0) => peer.host_eof = true,
                    Ok(length) => output.push(encoded(DATA, *id, &bytes[..length])),
                    Err(error) if retry(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_open_does_not_need_output_byte_or_chunk_capacity() {
        for (byte_limit, chunk_limit) in [(HEADER + 1, 4), (1024, 1)] {
            let directory = tempfile::tempdir_in("/tmp").unwrap();
            let path = directory.path().join("p");
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let mut peers = BTreeMap::new();
            let mut output = Queue::new(byte_limit, chunk_limit);
            output.push(encoded(DATA, 99, b"x"));
            assert_eq!(output.room(), 0);
            let mut input = Input::default();
            input
                .read(
                    &mut encoded(OPEN, 1, &[]).as_slice(),
                    &mut peers,
                    &mut output,
                    path.to_str().unwrap(),
                    Limits::default(),
                )
                .unwrap();
            assert!(peers.contains_key(&1));
            assert_eq!(input.used, 0);
            assert_eq!(output.chunks.len(), 1);
        }
    }

    #[test]
    fn rejected_open_waits_for_close_capacity_without_retrying_connect() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let path = directory.path().join("p");
        let mut peers = BTreeMap::new();
        let mut output = Queue::new(1024, 1);
        output.push(encoded(CLOSE, 99, &[]));
        let mut input = Input::default();
        let mut no_more_bytes = io::empty();
        input
            .read(
                &mut encoded(OPEN, 1, &[]).as_slice(),
                &mut peers,
                &mut output,
                "",
                Limits::default(),
            )
            .unwrap();
        assert!(input.rejected_open);
        assert!(!input.readable(&peers, &output));
        output.write(&mut Vec::new()).unwrap();
        assert!(input.readable(&peers, &output));
        // Even if a valid listener is now available, rejection is already decided.
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        input
            .read(
                &mut no_more_bytes,
                &mut peers,
                &mut output,
                path.to_str().unwrap(),
                Limits::default(),
            )
            .unwrap();
        assert!(peers.is_empty());
        let mut bytes = Vec::new();
        output.write(&mut bytes).unwrap();
        assert_eq!(bytes, encoded(CLOSE, 1, &[]));
        assert_eq!(input.used, 0);
    }

    #[test]
    fn idle_poll_has_no_periodic_wakeup() {
        assert_eq!(poll_timeout(None), -1);
        assert_eq!(poll_timeout(Some(Instant::now())), 0);
    }

    #[test]
    fn tiny_reads_charge_retained_allocations_and_chunk_metadata_is_bounded() {
        let mut queue = Queue::new(256 * 1024, 4);
        for _ in 0..4 {
            let mut allocation = Vec::with_capacity(CHUNK);
            allocation.push(1);
            queue.push(allocation);
        }
        assert_eq!(queue.room(), 0);
        assert_eq!(queue.bytes, 4);
        assert_eq!(
            queue
                .chunks
                .iter()
                .map(|chunk| chunk.bytes.len())
                .sum::<usize>(),
            4
        );
        queue.write(&mut Vec::new()).unwrap();
        assert!(queue.room() > 0);
    }

    #[test]
    fn queue_byte_limit_and_partial_write_keep_full_allocation_charged() {
        struct Partial;
        impl Write for Partial {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Ok(1)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut queue = Queue::new(4, 64);
        queue.push(vec![1; 4]);
        queue.write(&mut Partial).unwrap();
        assert_eq!(queue.room(), 0);
        assert_eq!(queue.bytes, 4);
        for _ in 0..3 {
            queue.write(&mut Partial).unwrap();
        }
        assert_eq!(queue.room(), 4);
    }

    #[test]
    fn malformed_or_oversized_frames_are_rejected_before_payload_allocation() {
        for (kind, length) in [(DATA, MAX_FRAME + 1), (OPEN, 1), (CLOSE, 1), (99, 0)] {
            let mut bytes = vec![kind];
            bytes.extend_from_slice(&7u32.to_be_bytes());
            bytes.extend_from_slice(&(length as u32).to_be_bytes());
            let mut input = Input::default();
            let mut output = Queue::new(1024, 4);
            let error = input
                .read(
                    &mut bytes.as_slice(),
                    &mut BTreeMap::new(),
                    &mut output,
                    "/unused",
                    Limits::default(),
                )
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(output.empty());
        }
    }

    #[test]
    fn descriptor_flags_are_restored_after_nonblocking_guard() {
        let (socket, _) = UnixStream::pair().unwrap();
        let fd = socket.as_raw_fd();
        let initial = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        {
            let _guard = Nonblocking::new(fd).unwrap();
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
                0
            );
        }
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFL) }, initial);
    }
}
