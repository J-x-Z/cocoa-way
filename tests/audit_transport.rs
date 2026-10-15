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
