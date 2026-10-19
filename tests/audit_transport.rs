#[path = "../src/apple_container_transport.rs"]
mod transport;

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

fn frame(writer: &mut impl Write, kind: u8, id: u32, data: &[u8]) {
    writer.write_all(&[kind]).unwrap();
    writer.write_all(&id.to_be_bytes()).unwrap();
    writer
        .write_all(&(data.len() as u32).to_be_bytes())
        .unwrap();
    writer.write_all(data).unwrap();
}

fn receive(reader: &mut impl Read) -> (u8, u32, Vec<u8>) {
    let mut header = [0; 9];
    reader.read_exact(&mut header).unwrap();
    let mut data = vec![0; u32::from_be_bytes(header[5..].try_into().unwrap()) as usize];
    reader.read_exact(&mut data).unwrap();
    (
        header[0],
        u32::from_be_bytes(header[1..5].try_into().unwrap()),
        data,
    )
}

fn limits() -> transport::Limits {
    transport::Limits {
        peer_bytes: 32 * 1024,
        transport_bytes: 32 * 1024,
        deadline: Duration::from_secs(3),
        ..Default::default()
    }
}

fn pair() -> (UnixStream, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    for socket in [&a, &b] {
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
    }
    (a, b)
}

#[test]
fn blocked_local_write_keeps_reverse_direction_live_and_preserves_large_frame() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let path = directory.path().join("p");
    let listener = UnixListener::bind(&path).unwrap();
    let (mut host, mut guest) = pair();
    let path = path.to_str().unwrap().to_string();
    let pump = std::thread::spawn(move || {
        let mut writer = host.try_clone().unwrap();
        transport::bridge_with_limits(&mut host, &mut writer, &path, limits())
    });
    frame(&mut guest, 1, 7, &[]);
    let (mut local, _) = listener.accept().unwrap();
    local
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut sender = guest.try_clone().unwrap();
    let payload = (0..2 * 1024 * 1024)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let expected = payload.clone();
    let upload = std::thread::spawn(move || frame(&mut sender, 2, 7, &payload));
    std::thread::sleep(Duration::from_millis(50));
    local.write_all(b"independent input").unwrap();
    assert_eq!(receive(&mut guest), (2, 7, b"independent input".to_vec()));
    let mut bytes = vec![0; expected.len()];
    local.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, expected);
    upload.join().unwrap();
    // CLOSE drains preceding local writes before shutting down the stream.
    frame(&mut guest, 3, 7, &[]);
    assert_eq!(local.read(&mut [0]).unwrap(), 0);
    guest.shutdown(std::net::Shutdown::Both).unwrap();
    assert!(pump.join().unwrap().is_ok());
}

#[test]
fn blocked_transport_write_does_not_stop_inbound_delivery() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let path = directory.path().join("p");
    let listener = UnixListener::bind(&path).unwrap();
    let (mut host, mut guest) = pair();
    let path = path.to_str().unwrap().to_string();
    let pump = std::thread::spawn(move || {
        let mut writer = host.try_clone().unwrap();
        transport::bridge_with_limits(&mut host, &mut writer, &path, limits())
    });
    frame(&mut guest, 1, 1, &[]);
    let (mut local, _) = listener.accept().unwrap();
    local
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut producer = local.try_clone().unwrap();
    let download = std::thread::spawn(move || {
        let _ = producer.write_all(&vec![42; 2 * 1024 * 1024]);
    });
    std::thread::sleep(Duration::from_millis(50));
    frame(&mut guest, 2, 1, b"key/button/control bytes");
    let mut input = vec![0; 24];
    local.read_exact(&mut input).unwrap();
    assert_eq!(input, b"key/button/control bytes");
    guest.shutdown(std::net::Shutdown::Both).unwrap();
    let _ = pump.join().unwrap();
    download.join().unwrap();
}

#[test]
fn open_and_following_data_progress_while_unrelated_output_is_full() {
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};

    // Inject EAGAIN after readiness, which a nonblocking writer must tolerate.
    // One output chunk fills admission deterministically, without socket-size guesses.
    struct GatedWriter {
        socket: UnixStream,
        stalled: Arc<AtomicBool>,
        attempted: mpsc::SyncSender<()>,
    }
    impl AsRawFd for GatedWriter {
        fn as_raw_fd(&self) -> RawFd {
            self.socket.as_raw_fd()
        }
    }
    impl Write for GatedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.stalled.load(Ordering::Acquire) {
                let _ = self.attempted.try_send(());
                Err(std::io::ErrorKind::WouldBlock.into())
            } else {
                self.socket.write(bytes)
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn accept_before(listener: &UnixListener) -> UnixStream {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match listener.accept() {
                Ok((socket, _)) => {
                    // BSD accept inherits O_NONBLOCK from the listener. A read
                    // timeout does not clear it: these are blocking fixture readers.
                    socket.set_nonblocking(false).unwrap();
                    let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };
                    assert!(flags >= 0);
                    assert_eq!(flags & libc::O_NONBLOCK, 0);
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    return socket;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "OPEN waited for unrelated output"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
    }

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let path = directory.path().join("p");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (mut host, mut guest) = pair();
    let stalled = Arc::new(AtomicBool::new(true));
    let (attempted, observed) = mpsc::sync_channel(1);
    let mut writer = GatedWriter {
        socket: host.try_clone().unwrap(),
        stalled: stalled.clone(),
        attempted,
    };
    let path = path.to_str().unwrap().to_string();
    let pump = std::thread::spawn(move || {
        transport::bridge_with_limits(
            &mut host,
            &mut writer,
            &path,
            transport::Limits {
                chunks: 1,
                ..limits()
            },
        )
    });
    frame(&mut guest, 1, 1, &[]);
    let mut a = accept_before(&listener);
    a.write_all(b"bulk").unwrap();
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    frame(&mut guest, 1, 2, &[]);
    frame(&mut guest, 2, 1, b"existing");
    frame(&mut guest, 2, 2, b"new");
    let mut b = accept_before(&listener);
    let mut existing = [0; 8];
    a.read_exact(&mut existing).unwrap();
    assert_eq!(&existing, b"existing");
    let mut new = [0; 3];
    b.read_exact(&mut new).unwrap();
    assert_eq!(&new, b"new");
    stalled.store(false, Ordering::Release);
    assert_eq!(receive(&mut guest), (2, 1, b"bulk".to_vec()));
    drop(guest);
    assert!(pump.join().unwrap().is_ok());
}

#[test]
fn residual_same_lane_hol_preserves_fifo_while_reverse_direction_progresses() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let path = directory.path().join("p");
    let listener = UnixListener::bind(&path).unwrap();
    let (mut host, mut guest) = pair();
    let path = path.to_str().unwrap().to_string();
    let pump = std::thread::spawn(move || {
        let mut writer = host.try_clone().unwrap();
        transport::bridge_with_limits(&mut host, &mut writer, &path, limits())
    });
    frame(&mut guest, 1, 1, &[]);
    frame(&mut guest, 1, 2, &[]);
    let (mut a, _) = listener.accept().unwrap();
    let (mut b, _) = listener.accept().unwrap();
    a.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    b.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut sender = guest.try_clone().unwrap();
    let upload = std::thread::spawn(move || {
        frame(&mut sender, 2, 1, &vec![42; 8 * 1024 * 1024]);
        frame(&mut sender, 3, 1, &[]);
        frame(&mut sender, 2, 2, b"after bulk");
    });
    std::thread::sleep(Duration::from_millis(50));
    b.write_all(b"reverse").unwrap();
    assert_eq!(receive(&mut guest), (2, 2, b"reverse".to_vec()));
    // This is a residual of shared framing, not something to "fix" by dropping
    // or reordering stream A's bytes. Every wait is bounded for regression use.
    let error = b.read(&mut [0; 1]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    let mut payload = Vec::new();
    a.read_to_end(&mut payload).unwrap();
    assert_eq!(payload, vec![42; 8 * 1024 * 1024]);
    b.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut marker = [0; 10];
    b.read_exact(&mut marker).unwrap();
    assert_eq!(&marker, b"after bulk");
    upload.join().unwrap();
    drop(guest);
    assert!(pump.join().unwrap().is_ok());
}

#[test]
fn saturated_channel_fails_with_a_deadline_not_an_unbounded_writer() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let path = directory.path().join("p");
    let listener = UnixListener::bind(&path).unwrap();
    let (mut host, mut guest) = pair();
    let path = path.to_str().unwrap().to_string();
    let started = Instant::now();
    let pump = std::thread::spawn(move || {
        let mut writer = host.try_clone().unwrap();
        let limits = transport::Limits {
            deadline: Duration::from_millis(150),
            ..limits()
        };
        transport::bridge_with_limits(&mut host, &mut writer, &path, limits)
    });
    frame(&mut guest, 1, 1, &[]);
    let (mut local, _) = listener.accept().unwrap();
    let producer = std::thread::spawn(move || {
        let _ = local.write_all(&vec![0; 2 * 1024 * 1024]);
    });
    assert_eq!(
        pump.join().unwrap().unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    producer.join().unwrap();
}

#[test]
fn stdio_pipe_transport_retains_wire_compatibility() {
    use std::os::fd::FromRawFd;
    fn pipe() -> (std::fs::File, std::fs::File) {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe {
            (
                std::fs::File::from_raw_fd(fds[0]),
                std::fs::File::from_raw_fd(fds[1]),
            )
        }
    }
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let path = directory.path().join("p");
    let listener = UnixListener::bind(&path).unwrap();
    let (mut input, mut guest_writer) = pipe();
    let (mut guest_reader, mut output) = pipe();
    let path = path.to_str().unwrap().to_string();
    let pump = std::thread::spawn(move || transport::bridge(&mut input, &mut output, &path));
    frame(&mut guest_writer, 1, 9, &[]);
    let (mut local, _) = listener.accept().unwrap();
    local.write_all(b"stdio").unwrap();
    assert_eq!(receive(&mut guest_reader), (2, 9, b"stdio".to_vec()));
    drop(guest_writer);
    assert!(pump.join().unwrap().is_ok());
}

#[test]
fn transport_eof_drains_all_preceding_data_before_closing_local_stream() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let path = directory.path().join("p");
    let listener = UnixListener::bind(&path).unwrap();
    let (mut host, mut guest) = pair();
    let path = path.to_str().unwrap().to_string();
    let pump = std::thread::spawn(move || {
        let mut writer = host.try_clone().unwrap();
        transport::bridge_with_limits(&mut host, &mut writer, &path, limits())
    });
    frame(&mut guest, 1, 1, &[]);
    let (mut local, _) = listener.accept().unwrap();
    local
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let upload = std::thread::spawn(move || {
        frame(&mut guest, 2, 1, &vec![42; 2 * 1024 * 1024]);
        // No CLOSE frame: clean transport EOF must still drain accepted bytes.
    });
    let mut data = Vec::new();
    local.read_to_end(&mut data).unwrap();
    assert_eq!(data, vec![42; 2 * 1024 * 1024]);
    upload.join().unwrap();
    assert!(pump.join().unwrap().is_ok());
}

#[test]
fn peer_limit_explicitly_closes_new_stream_without_losing_existing_stream() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let path = directory.path().join("p");
    let listener = UnixListener::bind(&path).unwrap();
    let (mut host, mut guest) = pair();
    let path = path.to_str().unwrap().to_string();
    let pump = std::thread::spawn(move || {
        let mut writer = host.try_clone().unwrap();
        transport::bridge_with_limits(
            &mut host,
            &mut writer,
            &path,
            transport::Limits {
                peers: 1,
                ..limits()
            },
        )
    });
    frame(&mut guest, 1, 1, &[]);
    let (mut local, _) = listener.accept().unwrap();
    local
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    frame(&mut guest, 1, 2, &[]);
    assert_eq!(receive(&mut guest), (3, 2, Vec::new()));
    frame(&mut guest, 2, 1, b"retained");
    frame(&mut guest, 3, 1, &[]);
    let mut data = Vec::new();
    local.read_to_end(&mut data).unwrap();
    assert_eq!(data, b"retained");
    drop(guest);
    assert!(pump.join().unwrap().is_ok());
}

#[test]
fn handshake_write_has_a_deadline_and_preserves_descriptor_flags() {
    use std::os::fd::AsRawFd;
    let (mut writer, _stalled_reader) = pair();
    let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
    let started = Instant::now();
    let error = transport::write_before(
        &mut writer,
        &vec![0; 2 * 1024 * 1024],
        Duration::from_millis(100),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) },
        flags
    );
}

#[test]
fn delayed_waypipe_listener_preserves_early_open_and_data() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let path = directory.path().join("p");
    let (mut host, mut guest) = pair();
    let relay_path = path.to_str().unwrap().to_string();
    let pump = std::thread::spawn(move || {
        let mut writer = host.try_clone().unwrap();
        transport::bridge_with_limits(&mut host, &mut writer, &relay_path, limits())
    });
    frame(&mut guest, 1, 1, &[]);
    frame(&mut guest, 2, 1, b"early input");
    std::thread::sleep(Duration::from_millis(80));
    let listener = UnixListener::bind(&path).unwrap();
    let (mut local, _) = listener.accept().unwrap();
    local
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut input = [0; 11];
    local.read_exact(&mut input).unwrap();
    assert_eq!(&input, b"early input");
    drop(guest);
    assert!(pump.join().unwrap().is_ok());
}

#[test]
fn host_eof_churn_reclaims_streams_without_waiting_for_guest_close() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let path = directory.path().join("p");
    let listener = UnixListener::bind(&path).unwrap();
    let (mut host, mut guest) = pair();
    let path = path.to_str().unwrap().to_string();
    let pump = std::thread::spawn(move || {
        let mut writer = host.try_clone().unwrap();
        transport::bridge_with_limits(
            &mut host,
            &mut writer,
            &path,
            transport::Limits {
                peers: 1,
                ..limits()
            },
        )
    });
    for id in 1..=256 {
        frame(&mut guest, 1, id, &[]);
        let (local, _) = listener.accept().unwrap();
        drop(local);
        assert_eq!(receive(&mut guest), (3, id, Vec::new()));
    }
    drop(guest);
    assert!(pump.join().unwrap().is_ok());
}
