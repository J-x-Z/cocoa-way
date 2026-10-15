#![cfg(unix)]

use std::{
    collections::HashMap,
    io::{IoSlice, Read, Write},
    mem::MaybeUninit,
    os::{fd::AsFd, unix::net::UnixStream},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use smithay::{
    delegate_compositor, delegate_shm, delegate_xdg_shell,
    reexports::{
        rustix::{
            self,
            net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg},
        },
        wayland_protocols::xdg::shell::server::xdg_wm_base,
        wayland_server::{
            Client, Display,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer::WlBuffer, wl_seat::WlSeat, wl_shm, wl_surface::WlSurface},
        },
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{
            self, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes,
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgPopupSurfaceData, XdgShellHandler,
            XdgShellState,
        },
        shm::{ShmHandler, ShmState, with_buffer_contents},
    },
};

// Isolate dispatch in a killable process: a cycle/lock regression must fail in
// bounded time rather than leave a spinning or deadlocked test thread behind.
fn bounded(test: &str, run: impl FnOnce()) {
    const CHILD: &str = "COCOA_WAY_SMITHAY_PROTOCOL_TEST";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        run();
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "protocol regression failed: {test}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("protocol dispatch exceeded its deadline: {test}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Default)]
struct ClientState {
    compositor: CompositorClientState,
    disconnected: Mutex<Option<DisconnectReason>>,
}

impl ClientData for ClientState {
    fn disconnected(&self, _id: ClientId, reason: DisconnectReason) {
        *self.disconnected.lock().unwrap() = Some(reason);
    }
}

struct State {
    compositor: CompositorState,
    shm: ShmState,
    shell: XdgShellState,
    popup_calls: Vec<PopupSurface>,
    commits: usize,
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        let buffer = compositor::with_states(surface, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let Some(BufferAssignment::NewBuffer(buffer)) = attributes.current().buffer.as_ref()
            else {
                panic!("healthy client did not commit a buffer");
            };
            buffer.clone()
        });
        with_buffer_contents(&buffer, |_, len, metadata| {
            assert_eq!(len, 128);
            assert_eq!(metadata.offset, 64);
            assert_eq!(
                (metadata.width, metadata.height, metadata.stride),
                (4, 4, 16)
            );
        })
        .unwrap();
        self.commits += 1;
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.shell
    }

    fn new_toplevel(&mut self, _surface: ToplevelSurface) {}

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.popup_calls.push(surface);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);

struct Server {
    display: Display<State>,
    state: State,
}

impl Server {
    fn new() -> Self {
        let display = Display::new().unwrap();
        let dh = display.handle();
        Self {
            display,
            state: State {
                compositor: CompositorState::new::<State>(&dh),
                shm: ShmState::new::<State>(&dh, []),
                shell: XdgShellState::new::<State>(&dh),
                popup_calls: Vec::new(),
                commits: 0,
            },
        }
    }

    fn dispatch(&mut self) {
        for _ in 0..2 {
            self.display.dispatch_clients(&mut self.state).unwrap();
            self.display.flush_clients().unwrap();
        }
    }
}

// Raw version-1 requests avoid adding client-side xdg protocol features or
// dependencies. Only registry/sync events need decoding; no real display opens.
struct Peer {
    socket: UnixStream,
    client: Client,
    data: Arc<ClientState>,
    next_id: u32,
    compositor: u32,
    shm: u32,
    shell: u32,
}

fn word(bytes: &[u8]) -> u32 {
    u32::from_ne_bytes(bytes[..4].try_into().unwrap())
}

fn string(value: &str) -> Vec<u8> {
    let mut bytes = ((value.len() + 1) as u32).to_ne_bytes().to_vec();
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    bytes.resize(bytes.len().next_multiple_of(4), 0);
    bytes
}

impl Peer {
    fn new(server: &mut Server) -> Self {
        let (socket, server_socket) = UnixStream::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let data = Arc::new(ClientState::default());
        let client = server
            .display
            .handle()
            .insert_client(server_socket, data.clone())
            .unwrap();
        let mut peer = Self {
            socket,
            client,
            data,
            next_id: 2,
            compositor: 0,
            shm: 0,
            shell: 0,
        };
        let registry = peer.id();
        peer.request(1, 1, &[registry]); // wl_display.get_registry
        let events = peer.roundtrip(server);
        let globals: HashMap<_, _> = events
            .into_iter()
            .filter_map(|(id, opcode, body)| {
                if id != registry || opcode != 0 {
                    return None;
                }
                let length = word(&body[4..]) as usize;
                let name = String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap();
                Some((name, word(&body)))
            })
            .collect();
        peer.compositor = peer.bind(registry, globals["wl_compositor"], "wl_compositor");
        peer.shm = peer.bind(registry, globals["wl_shm"], "wl_shm");
        peer.shell = peer.bind(registry, globals["xdg_wm_base"], "xdg_wm_base");
        peer.roundtrip(server);
        peer
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn frame(object: u32, opcode: u16, body: &[u8]) -> Vec<u8> {
        let mut bytes = object.to_ne_bytes().to_vec();
        bytes.extend_from_slice(&(((body.len() as u32 + 8) << 16) | opcode as u32).to_ne_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    fn request(&mut self, object: u32, opcode: u16, arguments: &[u32]) {
        let body: Vec<_> = arguments
            .iter()
            .flat_map(|word| word.to_ne_bytes())
            .collect();
        self.socket
            .write_all(&Self::frame(object, opcode, &body))
            .unwrap();
    }

    fn bind(&mut self, registry: u32, global: u32, interface: &str) -> u32 {
        let id = self.id();
        let mut body = global.to_ne_bytes().to_vec();
        body.extend(string(interface));
        body.extend(1u32.to_ne_bytes());
        body.extend(id.to_ne_bytes());
        self.socket
            .write_all(&Self::frame(registry, 0, &body))
            .unwrap();
        id
    }

    fn roundtrip(&mut self, server: &mut Server) -> Vec<(u32, u16, Vec<u8>)> {
        let callback = self.id();
        self.request(1, 0, &[callback]); // wl_display.sync
        server.dispatch();
        let mut events = Vec::new();
        loop {
            let mut header = [0; 8];
            self.socket.read_exact(&mut header).unwrap();
            let id = word(&header);
            let opcode = word(&header[4..]) as u16;
            let size = (word(&header[4..]) >> 16) as usize;
            assert!((8..=65535).contains(&size));
            let mut body = vec![0; size - 8];
            self.socket.read_exact(&mut body).unwrap();
            assert!(
                !(id == 1 && opcode == 0),
                "unexpected wl_display.error: {body:?}"
            );
            if id == callback && opcode == 0 {
                return events;
            }
            events.push((id, opcode, body));
        }
    }

    fn surface(&mut self) -> u32 {
        let surface = self.id();
        self.request(self.compositor, 0, &[surface]);
        surface
    }

    fn xdg_surface(&mut self) -> (u32, u32) {
        let surface = self.surface();
        let xdg = self.id();
        self.request(self.shell, 2, &[xdg, surface]);
        (surface, xdg)
    }

    fn positioner(&mut self) -> u32 {
        let positioner = self.id();
        self.request(self.shell, 1, &[positioner]);
        self.request(positioner, 1, &[4, 4]); // set_size
        self.request(positioner, 2, &[0, 0, 4, 4]); // set_anchor_rect
        positioner
    }

    fn popup(&mut self, xdg: u32, parent: u32, positioner: u32) {
        let popup = self.id();
        self.request(xdg, 2, &[popup, parent, positioner]);
    }

    fn pool(&mut self) -> (std::fs::File, u32) {
        let file = tempfile::tempfile().unwrap();
        file.set_len(128).unwrap();
        let pool = self.id();
        let body: Vec<_> = [pool, 64]
            .iter()
            .flat_map(|word| word.to_ne_bytes())
            .collect();
        let bytes = Self::frame(self.shm, 0, &body);
        let fds = [file.as_fd()];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
        let sent = sendmsg(
            &self.socket,
            &[IoSlice::new(&bytes)],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        assert!(sent > 0);
        self.socket.write_all(&bytes[sent..]).unwrap();
        (file, pool)
    }

    fn assert_protocol_error(&self, object: u32, code: u32) {
        let disconnected = self.data.disconnected.lock().unwrap();
        let Some(DisconnectReason::ProtocolError(error)) = disconnected.as_ref() else {
            panic!("client was not disconnected with a protocol error: {disconnected:?}");
        };
        assert_eq!(error.object_id, object);
        assert_eq!(error.code, code);
    }

    fn assert_healthy_submission(&mut self, server: &mut Server) {
        let before = server.state.commits;
        let (_file, pool) = self.pool();
        self.request(pool, 2, &[128]); // legal wl_shm_pool.resize
        let buffer = self.id();
        self.request(
            pool,
            0,
            &[buffer, 64, 4, 4, 16, wl_shm::Format::Argb8888 as u32],
        );
        let surface = self.surface();
        self.request(surface, 1, &[buffer, 0, 0]); // attach
        self.request(surface, 6, &[]); // commit
        self.roundtrip(server);
        assert_eq!(server.state.commits, before + 1);
        assert!(self.data.disconnected.lock().unwrap().is_none());
    }
}

#[test]
fn nonpositive_shm_resize_disconnects_only_the_sender() {
    bounded("nonpositive_shm_resize_disconnects_only_the_sender", || {
        for size in [0i32, -1, i32::MIN] {
            let mut server = Server::new();
            let mut good = Peer::new(&mut server);
            let mut bad = Peer::new(&mut server);
            let (_file, pool) = bad.pool();
            bad.roundtrip(&mut server);
            bad.request(pool, 2, &[size as u32]);
            server.dispatch();
            bad.assert_protocol_error(pool, wl_shm::Error::InvalidFd as u32);
            good.assert_healthy_submission(&mut server);
        }
    });
}

#[test]
fn popup_self_parent_is_rejected_before_registration() {
    bounded("popup_self_parent_is_rejected_before_registration", || {
        for alias in [false, true] {
            let mut server = Server::new();
            let mut good = Peer::new(&mut server);
            let mut bad = Peer::new(&mut server);
            let (surface, xdg) = bad.xdg_surface();
            let parent = if alias {
                let parent = bad.id();
                bad.request(bad.shell, 2, &[parent, surface]);
                parent
            } else {
                xdg
            };
            let positioner = bad.positioner();
            bad.roundtrip(&mut server);
            let target = bad
                .client
                .object_from_protocol_id::<WlSurface>(&server.display.handle(), surface)
                .unwrap();
            bad.popup(xdg, parent, positioner);
            server.dispatch();
            bad.assert_protocol_error(bad.shell, xdg_wm_base::Error::InvalidPopupParent as u32);
            assert!(server.state.popup_calls.is_empty());
            assert!(compositor::get_role(&target).is_none());
            good.assert_healthy_submission(&mut server);
        }
    });
}

fn set_parent(surface: &WlSurface, parent: &WlSurface) {
    compositor::with_states(surface, |states| {
        states
            .data_map
            .get::<XdgPopupSurfaceData>()
            .unwrap()
            .lock()
            .unwrap()
            .parent = Some(parent.clone());
    });
}

#[test]
fn popup_cyclic_ancestry_is_rejected_without_locking_up() {
    bounded(
        "popup_cyclic_ancestry_is_rejected_without_locking_up",
        || {
            for cycle in 0..3 {
                let mut server = Server::new();
                let mut good = Peer::new(&mut server);
                let mut bad = Peer::new(&mut server);
                let positioner = bad.positioner();
                let (_, first) = bad.xdg_surface();
                bad.popup(first, 0, positioner);
                let (_, second) = bad.xdg_surface();
                bad.popup(second, first, positioner);
                let (candidate_surface, candidate) = bad.xdg_surface();
                bad.roundtrip(&mut server);
                assert_eq!(server.state.popup_calls.len(), 2);
                let a = server.state.popup_calls[0].wl_surface().clone();
                let b = server.state.popup_calls[1].wl_surface().clone();
                let target = bad
                    .client
                    .object_from_protocol_id::<WlSurface>(
                        &server.display.handle(),
                        candidate_surface,
                    )
                    .unwrap();
                // Simulate ancestry supplied by an extension/compositor. Cover a
                // cycle through the new popup, an existing self-loop and A <-> B.
                let parent = match cycle {
                    0 => {
                        set_parent(&a, &target);
                        first
                    }
                    1 => {
                        set_parent(&a, &a);
                        first
                    }
                    _ => {
                        set_parent(&a, &b);
                        second
                    }
                };
                bad.popup(candidate, parent, positioner);
                server.dispatch();
                bad.assert_protocol_error(bad.shell, xdg_wm_base::Error::InvalidPopupParent as u32);
                assert_eq!(server.state.popup_calls.len(), 2);
                assert!(compositor::get_role(&target).is_none());
                good.assert_healthy_submission(&mut server);
            }
        },
    );
}

#[test]
fn valid_nested_and_null_popup_parents_still_work() {
    bounded("valid_nested_and_null_popup_parents_still_work", || {
        let mut server = Server::new();
        let mut peer = Peer::new(&mut server);
        let positioner = peer.positioner();
        let (_, root) = peer.xdg_surface();
        let toplevel = peer.id();
        peer.request(root, 1, &[toplevel]);
        let mut parent = root;
        for _ in 0..4 {
            let (_, xdg) = peer.xdg_surface();
            peer.popup(xdg, parent, positioner);
            parent = xdg;
        }
        let (_, unparented) = peer.xdg_surface();
        peer.popup(unparented, 0, positioner);
        peer.roundtrip(&mut server);
        assert_eq!(server.state.popup_calls.len(), 5);
        assert!(server.state.popup_calls[4].get_parent_surface().is_none());
        peer.assert_healthy_submission(&mut server);
    });
}
