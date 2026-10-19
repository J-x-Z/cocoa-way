//! Real Unix socket tests for the vendored Rust backend, independent of AppState/rendering.

use std::{
    io::{ErrorKind, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use smithay::reexports::wayland_server::{
    Display,
    backend::{
        ClientData, ClientId, DisconnectReason, GlobalHandler, GlobalId, Handle, ObjectData,
        ObjectId,
        protocol::{Argument, ArgumentType, Interface, Message, MessageDesc},
    },
};

const LONG_TURN: Duration = Duration::from_secs(60);
const SLOW_HANDLER: Duration = Duration::from_millis(200);
const SHORT_TURN: Duration = Duration::from_millis(100);

static PROBE: Interface = Interface {
    name: "fairness_probe",
    version: 1,
    requests: &[
        MessageDesc {
            name: "record",
            signature: &[ArgumentType::Uint],
            since: 1,
            is_destructor: false,
            child_interface: None,
            arg_interfaces: &[],
        },
        MessageDesc {
            name: "destroy",
            signature: &[],
            since: 1,
            is_destructor: true,
            child_interface: None,
            arg_interfaces: &[],
        },
        MessageDesc {
            name: "receive_fd",
            signature: &[ArgumentType::Fd],
            since: 1,
            is_destructor: false,
            child_interface: None,
            arg_interfaces: &[],
        },
        MessageDesc {
            name: "create",
            signature: &[ArgumentType::NewId],
            since: 1,
            is_destructor: false,
            child_interface: Some(&PROBE),
            arg_interfaces: &[],
        },
    ],
    events: &[],
    c_ptr: None,
};

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Record(u32, u32),
    DestroyRequest(u32),
    Destroyed(u32),
    Created(u32, u32),
}

struct Refill {
    stream: UnixStream,
    object: u32,
    next: u32,
    remaining: usize,
}

#[derive(Default)]
struct State {
    events: Vec<Event>,
    binds: Vec<u32>,
    refill: Option<Refill>,
    quiet_on_first_flood: Option<(UnixStream, u32)>,
    kill_flood_on_quiet: Option<ClientId>,
    sleep_once: bool,
    sleep_on_bind_once: bool,
    received_fds: usize,
    fail_creation: bool,
}

struct Probe(u32);

impl ObjectData<State> for Probe {
    fn request(
        self: Arc<Self>,
        handle: &Handle,
        state: &mut State,
        client: ClientId,
        message: Message<ObjectId, std::os::fd::OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<State>>> {
        if message.opcode == 1 {
            state.events.push(Event::DestroyRequest(self.0));
            return None;
        }
        if message.opcode == 2 {
            let [Argument::Fd(ref fd)] = message.args[..] else {
                panic!("unexpected FD request");
            };
            // SAFETY: fd is still owned by message and live throughout this call.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
            state.received_fds += 1;
            return None;
        }
        if message.opcode == 3 {
            let [Argument::NewId(ref object)] = message.args[..] else {
                panic!("unexpected create request");
            };
            state
                .events
                .push(Event::Created(self.0, object.protocol_id()));
            if state.fail_creation {
                handle.kill_client(client, DisconnectReason::ConnectionClosed);
                return None;
            }
            return Some(Arc::new(Probe(self.0 + 10)));
        }
        let [Argument::Uint(value)] = message.args[..] else {
            panic!("unexpected probe request");
        };
        state.events.push(Event::Record(self.0, value));
        if self.0 == 2 {
            if let Some(client) = state.kill_flood_on_quiet.take() {
                handle.kill_client(client, DisconnectReason::ConnectionClosed);
            }
        }
        if self.0 == 1 {
            if let Some((mut stream, object)) = state.quiet_on_first_flood.take() {
                stream.write_all(&request(object, 0, &[99])).unwrap();
            }
            if let Some(refill) = state.refill.as_mut() {
                // Refill through the actual peer socket during every flood handler,
                // keeping the FD readable without a timing-sensitive producer thread.
                if refill.remaining != 0 {
                    refill
                        .stream
                        .write_all(&request(refill.object, 0, &[refill.next]))
                        .unwrap();
                    refill.next += 1;
                    refill.remaining -= 1;
                }
            }
        }
        if std::mem::take(&mut state.sleep_once) {
            std::thread::sleep(SLOW_HANDLER);
        }
        None
    }

    fn destroyed(
        self: Arc<Self>,
        _handle: &Handle,
        state: &mut State,
        _client: ClientId,
        _object: ObjectId,
    ) {
        state.events.push(Event::Destroyed(self.0));
    }
}

#[derive(Default)]
struct Global {
    sleep_on_visibility_once: AtomicBool,
}

impl GlobalHandler<State> for Global {
    fn can_view(&self, _: ClientId, _: &Arc<dyn ClientData>, _: GlobalId) -> bool {
        if self.sleep_on_visibility_once.swap(false, Ordering::SeqCst) {
            std::thread::sleep(SLOW_HANDLER);
        }
        true
    }

    fn bind(
        self: Arc<Self>,
        _: &Handle,
        state: &mut State,
        _: ClientId,
        _: GlobalId,
        object: ObjectId,
    ) -> Arc<dyn ObjectData<State>> {
        state.binds.push(object.protocol_id());
        if std::mem::take(&mut state.sleep_on_bind_once) {
            std::thread::sleep(SLOW_HANDLER);
        }
        Arc::new(Probe(3))
    }
}

#[derive(Default)]
struct Disconnects(AtomicUsize);

impl ClientData for Disconnects {
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Peer {
    stream: UnixStream,
    client: ClientId,
    object: ObjectId,
    disconnects: Arc<Disconnects>,
}

struct Server {
    display: Display<State>,
    state: State,
}

impl Server {
    fn new() -> Self {
        Self {
            display: Display::new().unwrap(),
            state: State::default(),
        }
    }

    fn insert(&mut self, server: UnixStream, stream: UnixStream, tag: u32) -> Peer {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut handle = self.display.backend().handle();
        let disconnects = Arc::new(Disconnects::default());
        let client = handle.insert_client(server, disconnects.clone()).unwrap();
        // This raw peer knows the server-created ID out of band; no client library
        // or compositor protocol handlers participate in these scheduler tests.
        let object = handle
            .create_object(client.clone(), &PROBE, 1, Arc::new(Probe(tag)))
            .unwrap();
        Peer {
            stream,
            client,
            object,
            disconnects,
        }
    }

    fn peer(&mut self, tag: u32) -> Peer {
        let (server, stream) = UnixStream::pair().unwrap();
        self.insert(server, stream, tag)
    }

    fn timed_turn(&mut self, count: usize, duration: Duration, quantum: usize) -> (usize, bool) {
        // Return type is inferred through wayland-server's existing Backend reexport.
        let outcome = self
            .display
            .backend()
            .dispatch_all_clients_with_budget(&mut self.state, count, duration, quantum)
            .unwrap();
        assert!(outcome.dispatched <= count);
        (outcome.dispatched, outcome.more_pending)
    }

    fn turn(&mut self, count: usize, quantum: usize) -> (usize, bool) {
        self.timed_turn(count, LONG_TURN, quantum)
    }

    fn finish(&mut self) -> usize {
        let mut total = 0;
        for _ in 0..512 {
            let (count, more_pending) = self.turn(8, 2);
            total += count;
            if !more_pending {
                assert_eq!(
                    self.turn(8, 2),
                    (0, false),
                    "idle must not request another wake"
                );
                return total;
            }
        }
        panic!("continuations did not settle");
    }
}

fn request(object: u32, opcode: u16, args: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + 4 * args.len());
    bytes.extend_from_slice(&object.to_ne_bytes());
    bytes.extend_from_slice(
        &(((8 + 4 * args.len()) as u32) << 16 | u32::from(opcode)).to_ne_bytes(),
    );
    for arg in args {
        bytes.extend_from_slice(&arg.to_ne_bytes());
    }
    bytes
}

fn records(object: &ObjectId, values: impl IntoIterator<Item = u32>) -> Vec<u8> {
    values
        .into_iter()
        .flat_map(|value| request(object.protocol_id(), 0, &[value]))
        .collect()
}

fn bind(registry: u32, name: u32, object: u32) -> Vec<u8> {
    let mut args = vec![name, (PROBE.name.len() + 1) as u32];
    let mut name_bytes = PROBE.name.as_bytes().to_vec();
    name_bytes.push(0);
    while name_bytes.len() % 4 != 0 {
        name_bytes.push(0);
    }
    args.extend(
        name_bytes
            .chunks_exact(4)
            .map(|word| u32::from_ne_bytes(word.try_into().unwrap())),
    );
    args.extend([1, object]);
    request(registry, 0, &args)
}

fn send_fd(stream: &UnixStream, bytes: &[u8], fd: &impl AsRawFd) {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    // usize storage supplies cmsghdr alignment on Darwin and Linux.
    let mut control = [0usize; 8];
    // SAFETY: a zeroed msghdr is a valid starting point; all pointers and lengths
    // below refer to live buffers through sendmsg, which copies the FD.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        let space = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as usize;
        assert!(space <= std::mem::size_of_val(&control));
        message.msg_controllen = space as _;
        let header = libc::CMSG_FIRSTHDR(&message);
        assert!(!header.is_null());
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;
        std::ptr::write_unaligned(
            libc::CMSG_DATA(header).cast::<libc::c_int>(),
            fd.as_raw_fd(),
        );
        let sent = libc::sendmsg(stream.as_raw_fd(), &message, 0);
        assert_eq!(
            sent,
            bytes.len() as isize,
            "{}",
            std::io::Error::last_os_error()
        );
    }
}

fn peek_byte(stream: &UnixStream) -> std::io::Result<usize> {
    let mut byte = 0u8;
    // SAFETY: byte is writable, stream is live, and MSG_PEEK does not consume input.
    let count = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if count == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}

fn advertised_name(server: &mut Server, peer: &mut Peer) -> u32 {
    server.display.flush_clients().unwrap();
    let mut header = [0; 8];
    peer.stream.read_exact(&mut header).unwrap();
    assert_eq!(u32::from_ne_bytes(header[..4].try_into().unwrap()), 2);
    let word = u32::from_ne_bytes(header[4..].try_into().unwrap());
    assert_eq!(word & 0xffff, 0, "expected wl_registry.global");
    let mut args = vec![0; (word >> 16) as usize - 8];
    peer.stream.read_exact(&mut args).unwrap();
    u32::from_ne_bytes(args[..4].try_into().unwrap())
}

#[test]
fn continuously_readable_flood_does_not_starve_quiet_client() {
    let mut server = Server::new();
    let mut flood = server.peer(1);
    let mut quiet = server.peer(2);
    flood
        .stream
        .write_all(&records(&flood.object, 0..8))
        .unwrap();
    quiet
        .stream
        .write_all(&records(&quiet.object, [99]))
        .unwrap();
    server.state.refill = Some(Refill {
        stream: flood.stream.try_clone().unwrap(),
        object: flood.object.protocol_id(),
        next: 8,
        remaining: 128,
    });

    assert_eq!(server.turn(6, 2), (6, true));
    let quiet_at = server
        .state
        .events
        .iter()
        .position(|event| *event == Event::Record(2, 99))
        .unwrap();
    assert!(
        quiet_at <= 2,
        "quiet request waited beyond one flood quantum"
    );
    server.state.refill = None;
    server.finish();
    let flood_values: Vec<_> = server
        .state
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Record(1, value) => Some(*value),
            _ => None,
        })
        .collect();
    assert_eq!(
        flood_values,
        (0..flood_values.len() as u32).collect::<Vec<_>>()
    );
}

#[test]
fn newly_ready_quiet_client_is_admitted_across_one_request_continuations() {
    let mut server = Server::new();
    let mut flood = server.peer(1);
    let quiet = server.peer(2);
    flood
        .stream
        .write_all(&records(&flood.object, 0..8))
        .unwrap();
    server.state.quiet_on_first_flood = Some((
        quiet.stream.try_clone().unwrap(),
        quiet.object.protocol_id(),
    ));
    server.state.refill = Some(Refill {
        stream: flood.stream.try_clone().unwrap(),
        object: flood.object.protocol_id(),
        next: 8,
        remaining: 128,
    });
    for _ in 0..3 {
        assert_eq!(server.turn(1, 1), (1, true));
    }
    assert!(server.state.events.contains(&Event::Record(2, 99)));
    server.state.refill = None;
    server.finish();
}

#[test]
fn readiness_batch_capacity_does_not_hide_quiet_clients_behind_a_flood() {
    let mut server = Server::new();
    let mut flood = server.peer(1);
    flood
        .stream
        .write_all(&records(&flood.object, 0..8))
        .unwrap();
    server.state.refill = Some(Refill {
        stream: flood.stream.try_clone().unwrap(),
        object: flood.object.protocol_id(),
        next: 8,
        remaining: 128,
    });
    let mut quiet: Vec<_> = (2..42).map(|tag| server.peer(tag)).collect();
    for peer in &mut quiet {
        peer.stream.write_all(&records(&peer.object, [99])).unwrap();
    }
    assert_eq!(server.turn(64, 1), (64, true));
    for tag in 2..42 {
        assert!(
            server.state.events.contains(&Event::Record(tag, 99)),
            "quiet client {tag} starved"
        );
    }
    server.state.refill = None;
    server.finish();
}

#[test]
fn buffered_suffix_continues_even_after_the_socket_is_drained() {
    let mut server = Server::new();
    let (socket, stream) = UnixStream::pair().unwrap();
    let probe = socket.try_clone().unwrap();
    let mut peer = server.insert(socket, stream, 1);
    peer.stream
        .write_all(&records(&peer.object, 0..12))
        .unwrap();
    assert_eq!(server.turn(1, 1), (1, true));

    assert_eq!(peek_byte(&probe).unwrap_err().kind(), ErrorKind::WouldBlock);
    drop(probe);

    assert_eq!(
        server.finish(),
        11,
        "no additional write/readable FD may be required"
    );
    assert_eq!(
        server.state.events,
        (0..12)
            .map(|value| Event::Record(1, value))
            .collect::<Vec<_>>()
    );
}

#[test]
fn display_registry_and_bind_requests_all_consume_the_count_budget() {
    let mut server = Server::new();
    server
        .display
        .backend()
        .handle()
        .create_global(&PROBE, 1, Arc::new(Global::default()));
    let mut peer = server.peer(1);
    peer.stream.write_all(&request(1, 1, &[2])).unwrap(); // wl_display.get_registry
    assert_eq!(server.turn(1, usize::MAX), (1, true));
    let name = advertised_name(&mut server, &mut peer);
    let mut bytes = bind(2, name, 3);
    bytes.extend(request(3, 0, &[7]));
    bytes.extend(bind(2, name, 4));
    bytes.extend(request(4, 0, &[8]));
    bytes.extend(request(1, 0, &[5])); // wl_display.sync
    bytes.extend(records(&peer.object, [9]));
    peer.stream.write_all(&bytes).unwrap();

    for step in 0..6 {
        assert_eq!(server.turn(1, usize::MAX), (1, true));
        assert_eq!(server.state.binds.len(), if step < 2 { 1 } else { 2 });
        assert_eq!(server.state.events.len(), [0, 1, 1, 2, 2, 3][step]);
    }
    assert_eq!(server.finish(), 0);
    assert_eq!(
        server.state.events,
        [
            Event::Record(3, 7),
            Event::Record(3, 8),
            Event::Record(1, 9)
        ]
    );
}

#[test]
fn duration_is_checked_between_requests_without_preempting_a_slow_handler() {
    let mut server = Server::new();
    let mut peer = server.peer(1);
    peer.stream
        .write_all(&records(&peer.object, [1, 2]))
        .unwrap();
    server.state.sleep_once = true;
    assert_eq!(server.timed_turn(100, SHORT_TURN, 100), (1, true));
    assert_eq!(server.state.events, [Event::Record(1, 1)]);
    assert_eq!(server.finish(), 1);
}

#[test]
fn duration_is_checked_inside_display_and_registry_drains() {
    let mut server = Server::new();
    server.display.backend().handle().create_global(
        &PROBE,
        1,
        Arc::new(Global {
            sleep_on_visibility_once: AtomicBool::new(true),
        }),
    );
    let mut peer = server.peer(1);
    let mut bytes = request(1, 1, &[2]);
    bytes.extend(request(1, 0, &[3]));
    peer.stream.write_all(&bytes).unwrap();
    assert_eq!(server.timed_turn(100, SHORT_TURN, 100), (1, true));
    let name = advertised_name(&mut server, &mut peer);
    assert_eq!(server.finish(), 1);

    let mut bytes = bind(2, name, 4);
    bytes.extend(bind(2, name, 5));
    peer.stream.write_all(&bytes).unwrap();
    server.state.sleep_on_bind_once = true;
    assert_eq!(server.timed_turn(100, SHORT_TURN, 100), (1, true));
    assert_eq!(server.state.binds, [4]);
    assert_eq!(server.finish(), 1);
    assert_eq!(server.state.binds, [4, 5]);
}

#[test]
fn ordering_and_destructors_survive_request_boundaries() {
    let mut server = Server::new();
    let mut peer = server.peer(1);
    let other = server
        .display
        .backend()
        .handle()
        .create_object(peer.client.clone(), &PROBE, 1, Arc::new(Probe(2)))
        .unwrap();
    let mut bytes = records(&peer.object, [10, 11]);
    bytes.extend(request(peer.object.protocol_id(), 1, &[]));
    bytes.extend(records(&other, [12]));
    bytes.extend(request(other.protocol_id(), 1, &[]));
    peer.stream.write_all(&bytes).unwrap();
    for _ in 0..5 {
        assert_eq!(server.turn(1, 1), (1, true));
    }
    assert_eq!(server.finish(), 0);
    assert_eq!(
        server.state.events,
        [
            Event::Record(1, 10),
            Event::Record(1, 11),
            Event::DestroyRequest(1),
            Event::Destroyed(1),
            Event::Record(2, 12),
            Event::DestroyRequest(2),
            Event::Destroyed(2),
        ]
    );
    assert!(
        server
            .display
            .backend()
            .handle()
            .object_info(peer.object.clone())
            .is_err()
    );
    assert!(
        server
            .display
            .backend()
            .handle()
            .object_info(other)
            .is_err()
    );
    drop(peer.stream);
    server.finish();
    assert_eq!(peer.disconnects.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        server.state.events.len(),
        7,
        "destroyed objects must not be destroyed twice"
    );
}

#[test]
fn created_object_data_is_installed_before_yielding_to_its_first_request() {
    let mut server = Server::new();
    let mut peer = server.peer(1);
    let mut bytes = request(peer.object.protocol_id(), 3, &[2]);
    bytes.extend(request(2, 0, &[7]));
    bytes.extend(request(2, 1, &[]));
    peer.stream.write_all(&bytes).unwrap();
    assert_eq!(server.turn(1, 1), (1, true));
    assert_eq!(server.state.events, [Event::Created(1, 2)]);
    assert_eq!(server.turn(1, 1), (1, true));
    assert_eq!(server.state.events.last(), Some(&Event::Record(11, 7)));
    assert_eq!(server.finish(), 1);
    assert_eq!(
        server.state.events,
        [
            Event::Created(1, 2),
            Event::Record(11, 7),
            Event::DestroyRequest(11),
            Event::Destroyed(11),
        ]
    );
}

#[test]
fn failed_new_object_handler_disconnects_without_dispatching_the_suffix() {
    let mut server = Server::new();
    let mut peer = server.peer(1);
    server.state.fail_creation = true;
    let mut bytes = request(peer.object.protocol_id(), 3, &[2]);
    bytes.extend(request(2, 0, &[7]));
    peer.stream.write_all(&bytes).unwrap();
    let (count, _) = server.turn(1, 1);
    assert_eq!(count, 1);
    assert_eq!(server.finish(), 0);
    assert_eq!(peer.disconnects.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        server.state.events,
        [Event::Created(1, 2), Event::Destroyed(1)]
    );
}

#[test]
fn killed_client_with_a_retained_socket_duplicate_cannot_keep_polling_alive() {
    let mut server = Server::new();
    let (socket, stream) = UnixStream::pair().unwrap();
    let duplicate = socket.try_clone().unwrap();
    let mut peer = server.insert(socket, stream, 1);
    server.state.fail_creation = true;
    peer.stream
        .write_all(&request(peer.object.protocol_id(), 3, &[2]))
        .unwrap();
    assert_eq!(server.turn(1, 1).0, 1);
    assert_eq!(peer.disconnects.0.load(Ordering::SeqCst), 1);
    // The duplicate keeps the kernel socket live even though the backend has
    // retired its own FD. More incoming data must not resurrect its poll entry.
    peer.stream.write_all(&records(&peer.object, [99])).unwrap();
    assert_eq!(server.finish(), 0);
    assert_eq!(
        server.state.events,
        [Event::Created(1, 2), Event::Destroyed(1)]
    );
    drop(duplicate);
}

#[test]
fn rights_in_buffered_suffix_remain_owned_until_dispatch_or_disconnect() {
    for disconnect in [false, true] {
        let mut server = Server::new();
        let peer = server.peer(1);
        let (resource, observer) = UnixStream::pair().unwrap();
        let mut bytes = records(&peer.object, [1]);
        bytes.extend(request(peer.object.protocol_id(), 2, &[]));
        send_fd(&peer.stream, &bytes, &resource);
        drop(resource);
        assert_eq!(server.turn(1, 1), (1, true));
        assert_eq!(server.state.received_fds, 0);
        assert_eq!(
            peek_byte(&observer).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        if disconnect {
            server
                .display
                .backend()
                .handle()
                .kill_client(peer.client.clone(), DisconnectReason::ConnectionClosed);
            assert_eq!(server.finish(), 0);
            assert_eq!(peer.disconnects.0.load(Ordering::SeqCst), 1);
            assert_eq!(server.state.received_fds, 0);
        } else {
            assert_eq!(server.finish(), 1);
            assert_eq!(server.state.received_fds, 1);
        }
        assert_eq!(
            peek_byte(&observer).unwrap(),
            0,
            "received FD leaked after dispatch/cleanup"
        );
    }
}

#[test]
fn disconnect_drains_buffered_requests_then_retires_the_pending_client() {
    let mut server = Server::new();
    let mut peer = server.peer(1);
    peer.stream
        .write_all(&records(&peer.object, [1, 2, 3]))
        .unwrap();
    assert_eq!(server.turn(1, 1), (1, true));
    drop(peer.stream);
    assert_eq!(server.finish(), 2);
    assert_eq!(peer.disconnects.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        server.state.events,
        [
            Event::Record(1, 1),
            Event::Record(1, 2),
            Event::Record(1, 3),
            Event::Destroyed(1),
        ]
    );
    let mut replacement = server.peer(2);
    assert_ne!(peer.client, replacement.client);
    replacement
        .stream
        .write_all(&records(&replacement.object, [4]))
        .unwrap();
    assert_eq!(server.finish(), 1);
    assert_eq!(server.state.events.last(), Some(&Event::Record(2, 4)));
}

#[test]
fn zero_budgets_preserve_requests_and_idle_needs_no_continuation() {
    let mut server = Server::new();
    let mut peer = server.peer(1);
    assert_eq!(server.turn(0, 2), (0, false));
    assert_eq!(server.timed_turn(5, Duration::ZERO, 2), (0, false));
    assert_eq!(server.turn(5, 0), (0, false));
    peer.stream
        .write_all(&records(&peer.object, [1, 2, 3]))
        .unwrap();
    assert_eq!(server.turn(0, 2), (0, true));
    assert_eq!(server.timed_turn(5, Duration::ZERO, 2), (0, true));
    assert_eq!(server.turn(5, 0), (0, true));
    assert!(server.state.events.is_empty());
    assert_eq!(server.turn(1, 1), (1, true));
    assert_eq!(server.turn(0, 2), (0, true));
    assert_eq!(server.finish(), 2);
}

#[test]
fn malformed_suffix_counts_prior_requests_and_disconnects_once() {
    let mut server = Server::new();
    let mut peer = server.peer(1);
    let mut bytes = records(&peer.object, [1]);
    bytes.extend(request(0x00de_adde, 0, &[0])); // No such object.
    bytes.extend(records(&peer.object, [2]));
    peer.stream.write_all(&bytes).unwrap();
    assert_eq!(server.turn(8, 8), (1, false));
    assert_eq!(server.finish(), 0);
    assert_eq!(peer.disconnects.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        server.state.events,
        [Event::Record(1, 1), Event::Destroyed(1)]
    );
}

#[test]
fn stale_queued_client_cannot_alias_a_reused_client_slot() {
    let mut server = Server::new();
    let mut flood = server.peer(1);
    flood
        .stream
        .write_all(&records(&flood.object, 0..6))
        .unwrap();
    assert_eq!(server.turn(1, 1), (1, true));
    let mut quiet = server.peer(2);
    quiet
        .stream
        .write_all(&records(&quiet.object, [99]))
        .unwrap();
    server.state.kill_flood_on_quiet = Some(flood.client.clone());
    // Flood is already at the queue head. Quiet kills it while the next flood
    // quantum is queued; cleanup frees its slot without consuming that queue entry.
    assert_eq!(server.turn(2, 1), (2, true));
    assert_eq!(flood.disconnects.0.load(Ordering::SeqCst), 1);
    let mut replacement = server.peer(3);
    assert_ne!(flood.client, replacement.client);
    replacement
        .stream
        .write_all(&records(&replacement.object, [42]))
        .unwrap();
    assert_eq!(server.finish(), 1);
    assert_eq!(
        server.state.events,
        [
            Event::Record(1, 0),
            Event::Record(1, 1),
            Event::Record(2, 99),
            Event::Destroyed(1),
            Event::Record(3, 42),
        ]
    );
}

#[test]
fn legacy_dispatch_methods_drain_the_retained_userspace_suffix() {
    for single_client in [false, true] {
        let mut server = Server::new();
        let mut peer = server.peer(1);
        peer.stream.write_all(&records(&peer.object, 0..5)).unwrap();
        assert_eq!(server.turn(1, 1), (1, true));
        let count = if single_client {
            server
                .display
                .backend()
                .dispatch_single_client(&mut server.state, peer.client.clone())
                .unwrap()
        } else {
            server
                .display
                .backend()
                .dispatch_all_clients(&mut server.state)
                .unwrap()
        };
        assert_eq!(count, 4);
        if single_client {
            let error = server
                .display
                .backend()
                .dispatch_single_client(&mut server.state, peer.client.clone())
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::WouldBlock);
        } else {
            assert_eq!(
                server
                    .display
                    .backend()
                    .dispatch_all_clients(&mut server.state)
                    .unwrap(),
                0
            );
        }
        assert_eq!(server.turn(8, 2), (0, false));
        assert_eq!(
            server.state.events,
            (0..5)
                .map(|value| Event::Record(1, value))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn vendored_backend_manifest_and_lockfile_keep_the_exact_upstream_version() {
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml")).unwrap();
    assert_eq!(
        manifest["patch"]["crates-io"]["wayland-backend"]["path"].as_str(),
        Some("vendor/wayland-backend"),
    );
    let vendored: toml::Value =
        toml::from_str(include_str!("../vendor/wayland-backend/Cargo.toml")).unwrap();
    assert_eq!(
        vendored["package"]["name"].as_str(),
        Some("wayland-backend")
    );
    assert_eq!(vendored["package"]["version"].as_str(), Some("0.3.11"));
    assert_eq!(vendored["package"]["license"].as_str(), Some("MIT"));
    let lockfile: toml::Value = toml::from_str(include_str!("../Cargo.lock")).unwrap();
    let packages: Vec<_> = lockfile["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|package| package["name"].as_str() == Some("wayland-backend"))
        .collect();
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0]["version"].as_str(), Some("0.3.11"));
    assert!(packages[0].get("source").is_none());
    assert!(packages[0].get("checksum").is_none());
    assert!(
        include_str!("../vendor/wayland-backend/PATCHES")
            .contains("673a33c33048a5ade91a6b139580fa174e19fb0d23f396dca9fa15f2e1e49b35")
    );
    assert!(
        include_str!("../vendor/wayland-backend/LICENSE.txt")
            .contains("Permission is hereby granted, free of charge")
    );
}
