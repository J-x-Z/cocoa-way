#[allow(dead_code)]
#[path = "../src/control_client.rs"]
mod control_client;
#[allow(dead_code)]
#[path = "../src/control_protocol.rs"]
mod control_protocol;

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

use control_protocol::{ControlRequest, ControlResponse};

fn request() -> ControlRequest {
    ControlRequest {
        command: "status".into(),
        session: None,
        limit: 10,
    }
}

#[test]
fn stalled_server_times_out_and_a_later_request_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let worker = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        // Wait for the bounded client to close, without retaining an idle test worker.
        assert_eq!(reader.read_line(&mut line).unwrap(), 0);
        let (mut stream, _) = listener.accept().unwrap();
        let response = ControlResponse::success("status", serde_json::json!({"healthy": true}));
        let mut encoded = serde_json::to_vec(&response).unwrap();
        encoded.push(b'\n');
        stream.write_all(&encoded).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
    });
    let started = Instant::now();
    let error =
        control_client::send_request_with_timeout(&path, &request(), Duration::from_millis(80))
            .unwrap_err();
    assert!(error.contains("deadline exceeded"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    let response =
        control_client::send_request_with_timeout(&path, &request(), Duration::from_secs(2))
            .unwrap();
    assert_eq!(response.data["healthy"], true);
    worker.join().unwrap();
}

#[test]
fn oversized_response_is_rejected_before_eof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let worker = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        reader.read_line(&mut String::new()).unwrap();
        let bytes = vec![b'x'; control_client::MAX_RESPONSE_BYTES + 1];
        let _ = reader.get_mut().write_all(&bytes);
        let mut line = String::new();
        assert_eq!(reader.read_line(&mut line).unwrap(), 0);
    });
    let error =
        control_client::send_request_with_timeout(&path, &request(), Duration::from_secs(2))
            .unwrap_err();
    assert!(error.contains("exceeds byte limit"), "{error}");
    worker.join().unwrap();
}

#[test]
fn trickling_bytes_do_not_extend_the_read_deadline() {
    let (mut stream, mut peer) = UnixStream::pair().unwrap();
    let worker = std::thread::spawn(move || {
        for _ in 0..50 {
            if peer.write_all(b" ").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let started = Instant::now();
    let error =
        control_client::read_frame_before(&mut stream, 1024, started + Duration::from_millis(80))
            .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(stream);
    worker.join().unwrap();
}

#[test]
fn nonreading_peer_cannot_block_a_write() {
    let (mut stream, _peer) = UnixStream::pair().unwrap();
    let started = Instant::now();
    let error = control_client::write_before(
        &mut stream,
        &vec![b'x'; 8 * 1024 * 1024],
        started + Duration::from_millis(80),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn frame_boundaries_and_eof_remain_compatible() {
    for input in [b"abcd\nignored".as_slice(), b"abcd".as_slice()] {
        let (mut stream, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(input).unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(
            control_client::read_frame_before(
                &mut stream,
                4,
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap(),
            b"abcd"
        );
    }
}

#[test]
fn oversized_request_fails_before_connecting() {
    let mut request = request();
    request.session = Some("x".repeat(control_client::MAX_REQUEST_BYTES));
    let error = control_client::send_request(std::path::Path::new("/must-not-connect"), &request)
        .unwrap_err();
    assert!(error.contains("frame limit"), "{error}");
}

#[test]
fn a_full_local_accept_queue_cannot_block_connect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let listener = UnixListener::bind(&path).unwrap();
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
    let started = Instant::now();
    let mut connected = Vec::new();
    let mut rejected = false;
    for _ in 0..8 {
        match control_client::connect_before(&path, Instant::now() + Duration::from_millis(50)) {
            Ok(stream) => connected.push(stream),
            Err(_) => {
                rejected = true;
                break;
            }
        }
    }
    assert!(rejected);
    assert!(started.elapsed() < Duration::from_secs(1));
}
