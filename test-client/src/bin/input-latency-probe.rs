//! Native, receive-only Wayland input/commit instrumentation. No input injection,
//! key-repeat synthesis, presentation-time inference, or cross-clock subtraction.
//!
//! JSONL API (schema 1): --log-stderr (recommended), or --log-stdout explicitly.
//! guest_monotonic_ns and all *_ns fields are absolute CLOCK_MONOTONIC nanoseconds.
//! input_received is timestamped at backend event delivery, not kernel/hardware
//! arrival; input_handled is timestamped after applying that event to app state.
//! forwarded_event_time_ms_opaque is exactly wl_keyboard/wl_pointer's time u32,
//! not converted, unwrapped, or assumed to share CLOCK_MONOTONIC's epoch.
//! event_sequence/key_order/button_order count actual delivered protocol events.
//! Focus snapshots and compositor repeat_info never manufacture key events.
//! Key/button receipts include numeric evdev_key/evdev_button, raw_state (0
//! release, 1 press, other values left untouched), protocol_serial and opaque
//! forwarded time. Inapplicable source fields are null, not synthetic zeros.
//! Non-key/button protocol events count in event_sequence, not key/button_order.
//! order_status diagnoses only the observed focus-scoped protocol stream:
//! unmatched releases/duplicate presses alone do not establish transport loss,
//! reordered hardware input, or cross-focus ordering. Logging perturbs timing.
//!
//! SHM marker API v1: the first 40 pixels of row 0 encode 120 payload bytes.
//! For each three bytes a,b,c, the native-endian ARGB8888 word is
//! 0xff000000 | (a << 16) | (b << 8) | c. Decode RGB, not raw memory byte order.
//! Every marker pixel is opaque; the rest of the window is also opaque ARGB8888.
//! Payload integers are little-endian, with the following byte offsets:
//!   0: magic ASCII CWIP [4]; 4: version u16=1; 6: payload length u16=120;
//!   8: flags u32 (bit0 new cause, bit1 animation, bit2 opaque time present,
//!      bit3 multiple visual events coalesced; other bits zero);
//!  12: cause kind u32 (0 none, 1 key, 2 button, 3 motion, 4 enter, 5 leave,
//!      6 axis); 16: frame_sequence u64; 24: cause_event_sequence u64;
//!  32: forwarded event time u32; 36: evdev key/button code (axis enum for axis)
//!      u32; 40: raw key/button state u32; 44: protocol serial u32;
//!  48: guest receipt ns u64; 56: guest handler-completed ns u64;
//!  64: pre-commit CLOCK_MONOTONIC ns u64;
//!  72: visual events applied since previous commit u64;
//!  80: first applied event sequence u64; 88: last applied sequence u64;
//!  96: buffer width u32; 100: height u32; 104: wl_shm format u32=0 (ARGB8888);
//! 108: guest PID u32; 112: seat registry-global name u32; 116: reserved u32=0.
//! Absent cause fields are zero. A cause is the LAST visual event applied since
//! the preceding commit, NOT necessarily the sole cause; inspect flags/range.
//! Animation/configure-only frames have NO cause, even if old input remains
//! visible. Join only marked causes to receipts using run/tag, seat, kind,
//! sequence and the actual forwarded (time, serial, code, state) tuple.
//! pre-commit ns is sampled before writing the marker and queuing commit;
//! commit_queued_ns is sampled after wl_surface.commit is queued. Neither is
//! a compositor-receipt, composition, presentation, or input-to-photon time.
//! frame_callback carries the compositor's opaque callback argument separately.
//! buffer_release and frame_callback both identify the actual frame_sequence.

use std::{
    collections::{HashMap, HashSet},
    env,
    error::Error,
    ffi::CString,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{
        fs::{FileExt, OpenOptionsExt},
        io::{AsRawFd, OwnedFd, RawFd},
        net::UnixStream,
    },
    path::PathBuf,
    sync::{mpsc, Arc},
    time::Duration,
};

use wayland_client::{
    backend::{
        protocol::{AllowNull, Argument, ArgumentType, Interface, Message, MessageDesc},
        smallvec::SmallVec,
        Backend, ObjectData, ObjectId, WaylandError,
    },
    protocol::{__interfaces::*, wl_keyboard, wl_pointer, wl_registry, wl_seat},
    Connection, Proxy, WEnum,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
type Args = SmallVec<[Argument<ObjectId, RawFd>; 4]>;
const APP_ID: &str = "cocoa-way.input-latency-probe";
const MARKER_BYTES: usize = 120;
const MARKER_PIXELS: usize = MARKER_BYTES / 3;
const MAX_DIMENSION: u32 = 2048;

#[repr(C)]
struct Timespec {
    seconds: std::os::raw::c_long,
    nanoseconds: std::os::raw::c_long,
}

#[repr(C)]
struct PollFd {
    fd: RawFd,
    events: i16,
    revents: i16,
}

#[cfg(target_os = "linux")]
type Nfds = std::os::raw::c_ulong;
#[cfg(not(target_os = "linux"))]
type Nfds = std::os::raw::c_uint;
#[cfg(target_os = "linux")]
const CLOCK_MONOTONIC: i32 = 1;
#[cfg(target_os = "macos")]
const CLOCK_MONOTONIC: i32 = 6;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("input-latency-probe supports Linux guests and macOS only");

extern "C" {
    fn clock_gettime(clock: i32, time: *mut Timespec) -> i32;
    fn poll(fds: *mut PollFd, count: Nfds, timeout_ms: i32) -> i32;
}

fn monotonic_ns() -> u64 {
    let mut time = Timespec {
        seconds: 0,
        nanoseconds: 0,
    };
    // POSIX writes exactly one initialized timespec; no process-relative epoch.
    let result = unsafe { clock_gettime(CLOCK_MONOTONIC, &mut time) };
    assert_eq!(
        result,
        0,
        "CLOCK_MONOTONIC failed: {}",
        io::Error::last_os_error()
    );
    assert!(time.seconds >= 0 && (0..1_000_000_000).contains(&time.nanoseconds));
    (time.seconds as u64) * 1_000_000_000 + time.nanoseconds as u64
}

// Minimal xdg-shell v1 metadata, using the existing pure-Rust backend. Only the
// listed operations are exposed; unused constructor slots retain their opcode.
const fn desc(
    name: &'static str,
    signature: &'static [ArgumentType],
    child: Option<&'static Interface>,
    objects: &'static [&'static Interface],
    destructor: bool,
) -> MessageDesc {
    MessageDesc {
        name,
        signature,
        since: 1,
        is_destructor: destructor,
        child_interface: child,
        arg_interfaces: objects,
    }
}

use ArgumentType::{Array, Int, NewId, Object, Str, Uint};
const NO: AllowNull = AllowNull::No;
const YES: AllowNull = AllowNull::Yes;
static WM_BASE: Interface = Interface {
    name: "xdg_wm_base",
    version: 1,
    requests: &[
        desc("destroy", &[], None, &[], true),
        desc("create_positioner", &[NewId], None, &[], false),
        desc(
            "get_xdg_surface",
            &[NewId, Object(NO)],
            Some(&XDG_SURFACE),
            &[&WL_SURFACE_INTERFACE],
            false,
        ),
        desc("pong", &[Uint], None, &[], false),
    ],
    events: &[desc("ping", &[Uint], None, &[], false)],
    c_ptr: None,
};
static XDG_SURFACE: Interface = Interface {
    name: "xdg_surface",
    version: 1,
    requests: &[
        desc("destroy", &[], None, &[], true),
        desc("get_toplevel", &[NewId], Some(&TOPLEVEL), &[], false),
        desc(
            "get_popup",
            &[NewId, Object(YES), Object(NO)],
            None,
            &[
                &XDG_SURFACE,
                &wayland_client::backend::protocol::ANONYMOUS_INTERFACE,
            ],
            false,
        ),
        desc(
            "set_window_geometry",
            &[Int, Int, Int, Int],
            None,
            &[],
            false,
        ),
        desc("ack_configure", &[Uint], None, &[], false),
    ],
    events: &[desc("configure", &[Uint], None, &[], false)],
    c_ptr: None,
};
static TOPLEVEL: Interface = Interface {
    name: "xdg_toplevel",
    version: 1,
    requests: &[
        desc("destroy", &[], None, &[], true),
        desc("set_parent", &[Object(YES)], None, &[&TOPLEVEL], false),
        desc("set_title", &[Str(NO)], None, &[], false),
        desc("set_app_id", &[Str(NO)], None, &[], false),
        desc(
            "show_window_menu",
            &[Object(NO), Uint, Int, Int],
            None,
            &[&WL_SEAT_INTERFACE],
            false,
        ),
        desc(
            "move",
            &[Object(NO), Uint],
            None,
            &[&WL_SEAT_INTERFACE],
            false,
        ),
        desc(
            "resize",
            &[Object(NO), Uint, Uint],
            None,
            &[&WL_SEAT_INTERFACE],
            false,
        ),
        desc("set_max_size", &[Int, Int], None, &[], false),
        desc("set_min_size", &[Int, Int], None, &[], false),
    ],
    events: &[
        desc("configure", &[Int, Int, Array], None, &[], false),
        desc("close", &[], None, &[], false),
    ],
    c_ptr: None,
};

#[derive(Clone, Copy)]
enum Output {
    Off,
    Stderr,
    Stdout,
}

struct Options {
    output: Output,
    continuous: bool,
    duration: Option<Duration>,
    socket: Option<PathBuf>,
    tag: String,
    buffers: usize,
    width: u32,
    height: u32,
}

fn options() -> Result<Option<Options>> {
    let mut result = Options {
        output: Output::Off,
        continuous: false,
        duration: None,
        socket: None,
        tag: String::new(),
        buffers: 3,
        width: 640,
        height: 360,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("input-latency-probe [--static | --continuous-animation] [--duration SECONDS]\n\
                    [--socket PATH] [--buffers 2|3] [--width 64..2048] [--height 64..2048]\n\
                    [--log-stderr | --log-stdout] [--tag TEXT]\n\n\
                    Default: static, 640x360, three buffers, no logs, no time limit.\n\
                    Focus only this window and type/move/click; close its window to stop.\n\
                    Without --socket, use WAYLAND_SOCKET or WAYLAND_DISPLAY/XDG_RUNTIME_DIR.\n\
                    --log-stderr is recommended under waypipe; stdout is untouched otherwise.\n\
                    Absolute guest CLOCK_MONOTONIC ns; forwarded time arguments are opaque.\n\
                    No repeat synthesis, input injection, or input-to-photon measurement.\n\
                    SHM marker: CWIP v1, 120 little-endian payload bytes in row-zero RGB\n\
                    of 40 opaque ARGB8888 pixels. See source module docs for offsets/API.");
                return Ok(None);
            }
            "--static" => result.continuous = false,
            "--continuous-animation" | "--animate" => result.continuous = true,
            "--log-stderr" | "--log-stdout" => {
                if !matches!(result.output, Output::Off) {
                    return Err("choose exactly one log destination".into());
                }
                result.output = if arg == "--log-stderr" {
                    Output::Stderr
                } else {
                    Output::Stdout
                };
            }
            "--duration" => {
                let seconds: f64 = args.next().ok_or("--duration needs seconds")?.parse()?;
                if !seconds.is_finite() || seconds <= 0.0 {
                    return Err("--duration must be finite and positive".into());
                }
                result.duration = Some(Duration::try_from_secs_f64(seconds)?);
            }
            "--socket" => result.socket = Some(args.next().ok_or("--socket needs a path")?.into()),
            "--tag" => result.tag = args.next().ok_or("--tag needs text")?,
            "--buffers" => result.buffers = args.next().ok_or("--buffers needs 2 or 3")?.parse()?,
            "--width" => result.width = args.next().ok_or("--width needs pixels")?.parse()?,
            "--height" => result.height = args.next().ok_or("--height needs pixels")?.parse()?,
            _ => return Err(format!("unknown option {arg}; use --help").into()),
        }
    }
    if !(2..=3).contains(&result.buffers) {
        return Err("--buffers must be 2 or 3".into());
    }
    if !(64..=MAX_DIMENSION).contains(&result.width)
        || !(64..=MAX_DIMENSION).contains(&result.height)
    {
        return Err("width and height must be in 64..2048".into());
    }
    Ok(Some(result))
}

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            c if c < '\u{20}' => output.push_str(&format!("\\u{:04x}", c as u32)),
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

#[derive(Clone, Copy)]
enum Kind {
    Registry,
    Sync,
    Ignore,
    Seat(u32),
    Keyboard(u32),
    Pointer(u32),
    WmBase,
    XdgSurface,
    Toplevel,
    Buffer(usize),
    Frame(u64),
}

struct Received {
    kind: Kind,
    ns: u64,
    message: Message<ObjectId, OwnedFd>,
}

struct ReceiveData {
    kind: Kind,
    sender: mpsc::Sender<Received>,
}

impl ObjectData for ReceiveData {
    fn event(
        self: Arc<Self>,
        _: &Backend,
        message: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData>> {
        let ns = monotonic_ns();
        let _ = self.sender.send(Received {
            kind: self.kind,
            ns,
            message,
        });
        None
    }

    fn destroyed(&self, _: ObjectId) {}
}

#[derive(Default)]
struct Seat {
    object: Option<ObjectId>,
    version: u32,
    keyboard: Option<ObjectId>,
    pointer: Option<ObjectId>,
    keys: HashSet<u32>,
    buttons: HashSet<u32>,
}

#[derive(Default)]
struct Buffer {
    object: Option<ObjectId>,
    file: Option<File>,
    width: u32,
    height: u32,
    busy_frame: Option<u64>,
}

#[derive(Clone, Copy, Default)]
struct Cause {
    kind: u32,
    sequence: u64,
    time: Option<u32>,
    code: u32,
    state: u32,
    serial: u32,
    seat: u32,
    receipt_ns: u64,
    handler_ns: u64,
}

struct App {
    connection: Connection,
    sender: mpsc::Sender<Received>,
    options: Options,
    registry: ObjectId,
    compositor: Option<ObjectId>,
    shm: Option<ObjectId>,
    wm: Option<ObjectId>,
    surface: Option<ObjectId>,
    xdg: Option<ObjectId>,
    toplevel: Option<ObjectId>,
    seats: HashMap<u32, Seat>,
    buffers: Vec<Buffer>,
    next_buffer_slot: usize,
    scratch: Vec<u8>,
    width: u32,
    height: u32,
    configured: bool,
    dirty: bool,
    callback_pending: Option<u64>,
    running: bool,
    event_sequence: u64,
    key_order: u64,
    button_order: u64,
    key_presses: u64,
    key_releases: u64,
    button_presses: u64,
    button_releases: u64,
    duplicate_presses: u64,
    unmatched_releases: u64,
    frame_sequence: u64,
    cause: Option<Cause>,
    applied_count: u64,
    first_applied: u64,
    visual_seed: u32,
    pointer_position: Option<(f64, f64)>,
}

impl App {
    fn data(&self, kind: Kind) -> Arc<dyn ObjectData> {
        Arc::new(ReceiveData {
            kind,
            sender: self.sender.clone(),
        })
    }

    fn request(
        &self,
        object: &ObjectId,
        opcode: u16,
        args: Args,
        kind: Option<Kind>,
    ) -> Result<ObjectId> {
        Ok(self.connection.backend().send_request(
            Message {
                sender_id: object.clone(),
                opcode,
                args,
            },
            kind.map(|kind| self.data(kind)),
            None,
        )?)
    }

    fn bind(
        &self,
        name: u32,
        interface: &'static Interface,
        version: u32,
        kind: Kind,
    ) -> Result<ObjectId> {
        Ok(self.connection.backend().send_request(
            Message {
                sender_id: self.registry.clone(),
                opcode: 0,
                args: [
                    Argument::Uint(name),
                    Argument::Str(Some(Box::new(CString::new(interface.name)?))),
                    Argument::Uint(version),
                    Argument::NewId(ObjectId::null()),
                ]
                .into_iter()
                .collect(),
            },
            Some(self.data(kind)),
            Some((interface, version)),
        )?)
    }

    fn record(&self, record: &str, ns: u64, fields: fmt::Arguments<'_>) -> Result<()> {
        if matches!(self.options.output, Output::Off) {
            return Ok(());
        }
        let line = format!(
            "{{\"probe\":\"{APP_ID}\",\"schema\":1,\"tag\":{},\"pid\":{},\"record\":\"{record}\",\"guest_monotonic_ns\":{ns}{fields}}}",
            json_string(&self.options.tag), std::process::id(),
        );
        match self.options.output {
            Output::Stderr => writeln!(io::stderr().lock(), "{line}")?,
            Output::Stdout => writeln!(io::stdout().lock(), "{line}")?,
            Output::Off => {}
        }
        Ok(())
    }

    fn init_window(&mut self) -> Result<()> {
        let compositor = self
            .compositor
            .as_ref()
            .ok_or("compositor lacks wl_compositor")?;
        self.shm.as_ref().ok_or("compositor lacks wl_shm")?;
        let wm = self
            .wm
            .as_ref()
            .ok_or("compositor lacks stable xdg_wm_base")?;
        let surface = self.request(
            compositor,
            0,
            [Argument::NewId(ObjectId::null())].into_iter().collect(),
            Some(Kind::Ignore),
        )?;
        let xdg = self.request(
            wm,
            2,
            [
                Argument::NewId(ObjectId::null()),
                Argument::Object(surface.clone()),
            ]
            .into_iter()
            .collect(),
            Some(Kind::XdgSurface),
        )?;
        let toplevel = self.request(
            &xdg,
            1,
            [Argument::NewId(ObjectId::null())].into_iter().collect(),
            Some(Kind::Toplevel),
        )?;
        let mode = if self.options.continuous {
            "animated"
        } else {
            "static"
        };
        for (opcode, text) in [
            (2, format!("Input Latency Probe ({mode})")),
            (3, APP_ID.to_owned()),
        ] {
            self.request(
                &toplevel,
                opcode,
                [Argument::Str(Some(Box::new(CString::new(text)?)))]
                    .into_iter()
                    .collect(),
                None,
            )?;
        }
        for (opcode, size) in [(7, MAX_DIMENSION as i32), (8, 64)] {
            self.request(
                &toplevel,
                opcode,
                [Argument::Int(size), Argument::Int(size)]
                    .into_iter()
                    .collect(),
                None,
            )?;
        }
        self.request(&surface, 6, Args::new(), None)?;
        self.surface = Some(surface);
        self.xdg = Some(xdg);
        self.toplevel = Some(toplevel);
        Ok(())
    }

    fn capabilities(&mut self, seat_name: u32, capabilities: wl_seat::Capability) -> Result<()> {
        let seat = self.seats.get_mut(&seat_name).ok_or("unknown seat")?;
        let object = seat.object.as_ref().ok_or("seat missing object")?.clone();
        let version = seat.version;
        let has_keyboard = capabilities.contains(wl_seat::Capability::Keyboard);
        let has_pointer = capabilities.contains(wl_seat::Capability::Pointer);
        if !has_keyboard {
            seat.keys.clear();
            if let Some(id) = (version >= 3).then(|| seat.keyboard.take()).flatten() {
                self.request(&id, 0, Args::new(), None)?;
            }
        } else if seat.keyboard.is_none() {
            let id = self.request(
                &object,
                1,
                [Argument::NewId(ObjectId::null())].into_iter().collect(),
                Some(Kind::Keyboard(seat_name)),
            )?;
            self.seats.get_mut(&seat_name).unwrap().keyboard = Some(id);
        }
        let seat = self.seats.get_mut(&seat_name).unwrap();
        if !has_pointer {
            seat.buttons.clear();
            if let Some(id) = (version >= 3).then(|| seat.pointer.take()).flatten() {
                self.request(&id, 1, Args::new(), None)?;
            }
        } else if seat.pointer.is_none() {
            let id = self.request(
                &object,
                0,
                [Argument::NewId(ObjectId::null())].into_iter().collect(),
                Some(Kind::Pointer(seat_name)),
            )?;
            self.seats.get_mut(&seat_name).unwrap().pointer = Some(id);
        }
        Ok(())
    }

    fn input(&mut self, received: Received) -> Result<()> {
        let handler_start_ns = monotonic_ns();
        let source_object_id = received.message.sender_id.protocol_id();
        self.event_sequence += 1;
        let sequence = self.event_sequence;
        let mut cause = Cause {
            sequence,
            receipt_ns: received.ns,
            ..Cause::default()
        };
        let mut changed = false;
        let mut order_status = "not_key_or_button";
        let mut protocol_serial = None;
        let mut position = None;
        let mut axis_kind = None;
        let mut axis_value = None;
        let (seat_name, event_name, detail) = match received.kind {
            Kind::Keyboard(seat_name) => {
                cause.seat = seat_name;
                let (_, event) =
                    wl_keyboard::WlKeyboard::parse_event(&self.connection, received.message)?;
                let detail = if matches!(self.options.output, Output::Off) {
                    String::new()
                } else {
                    format!("{event:?}")
                };
                let seat = self
                    .seats
                    .get_mut(&seat_name)
                    .ok_or("keyboard for unknown seat")?;
                let name = match event {
                    wl_keyboard::Event::Key {
                        serial,
                        time,
                        key,
                        state,
                    } => {
                        protocol_serial = Some(serial);
                        cause.kind = 1;
                        cause.serial = serial;
                        cause.time = Some(time);
                        cause.code = key;
                        cause.state = enum_u32(state);
                        self.key_order += 1;
                        order_status = transition(
                            &mut seat.keys,
                            key,
                            cause.state,
                            &mut self.key_presses,
                            &mut self.key_releases,
                        );
                        self.visual_seed = key.wrapping_mul(0x9e37_79b9)
                            ^ (self.key_order as u32).rotate_left(11)
                            ^ cause.state;
                        changed = true;
                        "key"
                    }
                    wl_keyboard::Event::Enter { serial, keys, .. } => {
                        protocol_serial = Some(serial);
                        seat.keys = keys
                            .chunks_exact(4)
                            .map(|bytes| u32::from_ne_bytes(bytes.try_into().unwrap()))
                            .collect();
                        "keyboard_enter"
                    }
                    wl_keyboard::Event::Leave { serial, .. } => {
                        protocol_serial = Some(serial);
                        seat.keys.clear();
                        "keyboard_leave"
                    }
                    wl_keyboard::Event::Keymap { fd, .. } => {
                        drop(fd);
                        "keyboard_keymap"
                    }
                    wl_keyboard::Event::Modifiers { serial, .. } => {
                        protocol_serial = Some(serial);
                        "keyboard_modifiers"
                    }
                    wl_keyboard::Event::RepeatInfo { .. } => "keyboard_repeat_info",
                    _ => "keyboard_other",
                };
                (seat_name, name, detail)
            }
            Kind::Pointer(seat_name) => {
                cause.seat = seat_name;
                let (_, event) =
                    wl_pointer::WlPointer::parse_event(&self.connection, received.message)?;
                let detail = if matches!(self.options.output, Output::Off) {
                    String::new()
                } else {
                    format!("{event:?}")
                };
                let name = match event {
                    wl_pointer::Event::Button {
                        serial,
                        time,
                        button,
                        state,
                    } => {
                        protocol_serial = Some(serial);
                        cause.kind = 2;
                        cause.serial = serial;
                        cause.time = Some(time);
                        cause.code = button;
                        cause.state = enum_u32(state);
                        self.button_order += 1;
                        let seat = self
                            .seats
                            .get_mut(&seat_name)
                            .ok_or("pointer for unknown seat")?;
                        order_status = transition(
                            &mut seat.buttons,
                            button,
                            cause.state,
                            &mut self.button_presses,
                            &mut self.button_releases,
                        );
                        self.visual_seed = button.wrapping_mul(0x85eb_ca6b)
                            ^ (self.button_order as u32).rotate_left(7)
                            ^ cause.state;
                        changed = true;
                        "button"
                    }
                    wl_pointer::Event::Motion {
                        time,
                        surface_x,
                        surface_y,
                    } => {
                        position = Some((surface_x, surface_y));
                        cause.kind = 3;
                        cause.time = Some(time);
                        self.pointer_position = Some((surface_x, surface_y));
                        changed = true;
                        "pointer_motion"
                    }
                    wl_pointer::Event::Enter {
                        serial,
                        surface_x,
                        surface_y,
                        ..
                    } => {
                        protocol_serial = Some(serial);
                        position = Some((surface_x, surface_y));
                        cause.kind = 4;
                        cause.serial = serial;
                        self.pointer_position = Some((surface_x, surface_y));
                        self.seats
                            .get_mut(&seat_name)
                            .ok_or("pointer for unknown seat")?
                            .buttons
                            .clear();
                        changed = true;
                        "pointer_enter"
                    }
                    wl_pointer::Event::Leave { serial, .. } => {
                        protocol_serial = Some(serial);
                        cause.kind = 5;
                        cause.serial = serial;
                        self.pointer_position = None;
                        self.seats
                            .get_mut(&seat_name)
                            .ok_or("pointer for unknown seat")?
                            .buttons
                            .clear();
                        changed = true;
                        "pointer_leave"
                    }
                    wl_pointer::Event::Axis { time, axis, value } => {
                        cause.kind = 6;
                        cause.time = Some(time);
                        cause.code = enum_u32(axis);
                        axis_kind = Some(cause.code);
                        axis_value = Some(value);
                        self.visual_seed =
                            self.visual_seed.rotate_left(3) ^ (value.to_bits() as u32);
                        changed = true;
                        "pointer_axis"
                    }
                    wl_pointer::Event::AxisStop { time, axis } => {
                        cause.time = Some(time);
                        axis_kind = Some(enum_u32(axis));
                        "pointer_axis_stop"
                    }
                    wl_pointer::Event::Frame => "pointer_frame",
                    wl_pointer::Event::AxisSource { .. } => "pointer_axis_source",
                    wl_pointer::Event::AxisDiscrete { .. } => "pointer_axis_discrete",
                    _ => "pointer_other",
                };
                (seat_name, name, detail)
            }
            _ => return Err("non-input event passed to input handler".into()),
        };
        if order_status == "duplicate_press" {
            self.duplicate_presses += 1;
        }
        if order_status == "unmatched_release" {
            self.unmatched_releases += 1;
        }
        if changed {
            if self.applied_count == 0 {
                self.first_applied = sequence;
            }
            self.applied_count += 1;
            self.dirty = true;
        }
        cause.handler_ns = monotonic_ns();
        if changed {
            self.cause = Some(cause);
        }
        if matches!(self.options.output, Output::Off) {
            return Ok(());
        }
        let key_or_button = cause.kind == 1 || cause.kind == 2;
        let fields = format!(
            ",\"event\":\"{event_name}\",\"event_sequence\":{sequence},\"seat_global\":{seat_name},\"source_object_id\":{source_object_id},\"forwarded_event_time_ms_opaque\":{},\"protocol_serial\":{},\"evdev_code\":{},\"evdev_key\":{},\"evdev_button\":{},\"raw_state\":{},\"surface_x\":{},\"surface_y\":{},\"axis\":{},\"axis_value\":{},\"key_order\":{},\"button_order\":{},\"key_presses\":{},\"key_releases\":{},\"button_presses\":{},\"button_releases\":{},\"order_status\":\"{order_status}\",\"visual_update\":{changed},\"detail\":{}",
            json_number(cause.time), json_number(protocol_serial),
            json_number(key_or_button.then_some(cause.code)),
            json_number((cause.kind == 1).then_some(cause.code)),
            json_number((cause.kind == 2).then_some(cause.code)),
            json_number(key_or_button.then_some(cause.state)),
            json_number(position.map(|(x, _)| x)), json_number(position.map(|(_, y)| y)),
            json_number(axis_kind), json_number(axis_value), self.key_order, self.button_order,
            self.key_presses, self.key_releases, self.button_presses, self.button_releases, json_string(&detail),
        );
        self.record("input_received", received.ns, format_args!("{fields}"))?;
        self.record(
            "input_handled",
            cause.handler_ns,
            format_args!(
                "{fields},\"handler_start_ns\":{handler_start_ns},\"receipt_ns\":{}",
                received.ns
            ),
        )?;
        Ok(())
    }

    fn handle(&mut self, received: Received) -> Result<()> {
        match received.kind {
            Kind::Keyboard(_) | Kind::Pointer(_) => return self.input(received),
            Kind::Registry => {
                let (_, event) =
                    wl_registry::WlRegistry::parse_event(&self.connection, received.message)?;
                match event {
                    wl_registry::Event::Global {
                        name,
                        interface,
                        version,
                    } => match interface.as_str() {
                        "wl_compositor" if self.compositor.is_none() => {
                            self.compositor = Some(self.bind(
                                name,
                                &WL_COMPOSITOR_INTERFACE,
                                version.min(4),
                                Kind::Ignore,
                            )?)
                        }
                        "wl_shm" if self.shm.is_none() => {
                            self.shm = Some(self.bind(name, &WL_SHM_INTERFACE, 1, Kind::Ignore)?)
                        }
                        "xdg_wm_base" if self.wm.is_none() => {
                            self.wm = Some(self.bind(name, &WM_BASE, 1, Kind::WmBase)?)
                        }
                        "wl_seat" if version >= 1 => {
                            let id = self.bind(
                                name,
                                &WL_SEAT_INTERFACE,
                                version.min(7),
                                Kind::Seat(name),
                            )?;
                            self.seats.insert(
                                name,
                                Seat {
                                    object: Some(id),
                                    version: version.min(7),
                                    ..Seat::default()
                                },
                            );
                        }
                        _ => {}
                    },
                    wl_registry::Event::GlobalRemove { name } => {
                        // Keep delivered-event state until disconnect; a removed global
                        // does not by itself invalidate already bound protocol objects.
                        self.record(
                            "global_removed",
                            received.ns,
                            format_args!(",\"global_name\":{name}"),
                        )?;
                    }
                    _ => {}
                }
            }
            Kind::Sync => self.init_window()?,
            Kind::Seat(name) => {
                let (_, event) = wl_seat::WlSeat::parse_event(&self.connection, received.message)?;
                if let wl_seat::Event::Capabilities {
                    capabilities: WEnum::Value(capabilities),
                } = event
                {
                    self.capabilities(name, capabilities)?;
                }
            }
            Kind::WmBase => {
                let serial = uint(&received.message, 0)?;
                self.request(
                    &received.message.sender_id,
                    3,
                    [Argument::Uint(serial)].into_iter().collect(),
                    None,
                )?;
            }
            Kind::XdgSurface => {
                let serial = uint(&received.message, 0)?;
                self.request(
                    &received.message.sender_id,
                    4,
                    [Argument::Uint(serial)].into_iter().collect(),
                    None,
                )?;
                self.configured = true;
                self.dirty = true;
                self.record(
                    "configure",
                    received.ns,
                    format_args!(
                        ",\"protocol_serial\":{serial},\"width\":{},\"height\":{}",
                        self.width, self.height
                    ),
                )?;
            }
            Kind::Toplevel => match received.message.opcode {
                0 => {
                    let width = int(&received.message, 0)?;
                    let height = int(&received.message, 1)?;
                    self.record(
                        "toplevel_configure",
                        received.ns,
                        format_args!(",\"requested_width\":{width},\"requested_height\":{height}"),
                    )?;
                    if width > 0 {
                        self.width = (width as u32).clamp(64, MAX_DIMENSION);
                    }
                    if height > 0 {
                        self.height = (height as u32).clamp(64, MAX_DIMENSION);
                    }
                }
                1 => self.running = false,
                _ => {}
            },
            Kind::Buffer(index) => {
                let buffer = &mut self.buffers[index];
                if buffer.object.as_ref() != Some(&received.message.sender_id) {
                    return Err("release from an unexpected buffer generation".into());
                }
                let frame = buffer
                    .busy_frame
                    .take()
                    .ok_or("release from a non-busy buffer")?;
                self.record(
                    "buffer_release",
                    received.ns,
                    format_args!(",\"frame_sequence\":{frame},\"buffer_slot\":{index}"),
                )?;
            }
            Kind::Frame(frame) => {
                let callback_time = uint(&received.message, 0)?;
                if self.callback_pending != Some(frame) {
                    return Err("unexpected frame callback sequence".into());
                }
                self.callback_pending = None;
                self.record(
                    "frame_callback",
                    received.ns,
                    format_args!(
                        ",\"frame_sequence\":{frame},\"callback_time_ms_opaque\":{callback_time}"
                    ),
                )?;
            }
            Kind::Ignore => {}
        }
        Ok(())
    }

    fn prepare_buffer(&mut self, index: usize) -> Result<()> {
        let buffer = &self.buffers[index];
        if buffer.object.is_some() && buffer.width == self.width && buffer.height == self.height {
            return Ok(());
        }
        if buffer.busy_frame.is_some() {
            return Err("attempt to rewrite a busy buffer".into());
        }
        if let Some(id) = buffer.object.as_ref() {
            self.request(id, 0, Args::new(), None)?;
        }
        self.buffers[index] = Buffer::default();
        let length = self.width as usize * self.height as usize * 4;
        let file = shm_file(length as u64)?;
        let shm = self.shm.as_ref().ok_or("missing wl_shm")?;
        let pool = self.request(
            shm,
            0,
            [
                Argument::NewId(ObjectId::null()),
                Argument::Fd(file.as_raw_fd()),
                Argument::Int(length as i32),
            ]
            .into_iter()
            .collect(),
            Some(Kind::Ignore),
        )?;
        let object = self.request(
            &pool,
            0,
            [
                Argument::NewId(ObjectId::null()),
                Argument::Int(0),
                Argument::Int(self.width as i32),
                Argument::Int(self.height as i32),
                Argument::Int((self.width * 4) as i32),
                Argument::Uint(0),
            ]
            .into_iter()
            .collect(),
            Some(Kind::Buffer(index)),
        )?;
        self.request(&pool, 1, Args::new(), None)?;
        self.buffers[index] = Buffer {
            object: Some(object),
            file: Some(file),
            width: self.width,
            height: self.height,
            busy_frame: None,
        };
        Ok(())
    }

    fn render(&mut self) -> Result<()> {
        if !self.running
            || !self.configured
            || self.callback_pending.is_some()
            || (!self.dirty && !self.options.continuous)
        {
            return Ok(());
        }
        let Some(index) = (0..self.buffers.len())
            .map(|offset| (self.next_buffer_slot + offset) % self.buffers.len())
            .find(|index| self.buffers[*index].busy_frame.is_none())
        else {
            return Ok(());
        };
        if self.frame_sequence == 0 {
            for slot in 0..self.buffers.len() {
                self.prepare_buffer(slot)?;
            }
        }
        self.prepare_buffer(index)?;
        let frame = self.frame_sequence + 1;
        let render_start_ns = monotonic_ns();
        let color = 0xff000000 | (self.visual_seed & 0x00ffffff);
        let marker_x = (self.visual_seed as usize % (self.width as usize - 32)) + 16;
        let marker_y =
            ((self.visual_seed.rotate_left(13) as usize) % (self.height as usize - 32)) + 16;
        let stripe = (frame as usize * 3) % self.width as usize;
        self.scratch
            .resize(self.width as usize * self.height as usize * 4, 0);
        for (index, pixel) in self.scratch.chunks_exact_mut(4).enumerate() {
            let x = index % self.width as usize;
            let y = index / self.width as usize;
            let key_marker = x.abs_diff(marker_x) < 12 && y.abs_diff(marker_y) < 12;
            let crosshair = self.pointer_position.is_some_and(|(px, py)| {
                ((x as f64 - px).abs() < 2.0 && (y as f64 - py).abs() < 12.0)
                    || ((y as f64 - py).abs() < 2.0 && (x as f64 - px).abs() < 12.0)
            });
            let animation = self.options.continuous && x.abs_diff(stripe) < 3;
            let value: u32 = if y > 0 && (key_marker || crosshair || animation) {
                0xffffffff
            } else {
                color
            };
            pixel.copy_from_slice(&value.to_ne_bytes());
        }
        let buffer = &self.buffers[index];
        let file = buffer.file.as_ref().ok_or("buffer missing SHM file")?;
        file.write_all_at(&self.scratch, 0)?;
        let surface = self.surface.as_ref().ok_or("missing surface")?;
        let xdg = self.xdg.as_ref().ok_or("missing xdg surface")?;
        self.request(
            xdg,
            3,
            [
                Argument::Int(0),
                Argument::Int(0),
                Argument::Int(self.width as i32),
                Argument::Int(self.height as i32),
            ]
            .into_iter()
            .collect(),
            None,
        )?;
        self.request(
            surface,
            1,
            [
                Argument::Object(buffer.object.as_ref().unwrap().clone()),
                Argument::Int(0),
                Argument::Int(0),
            ]
            .into_iter()
            .collect(),
            None,
        )?;
        self.request(
            surface,
            2,
            [
                Argument::Int(0),
                Argument::Int(0),
                Argument::Int(self.width as i32),
                Argument::Int(self.height as i32),
            ]
            .into_iter()
            .collect(),
            None,
        )?;
        self.request(
            surface,
            3,
            [Argument::NewId(ObjectId::null())].into_iter().collect(),
            Some(Kind::Frame(frame)),
        )?;
        let pre_commit_ns = monotonic_ns();
        let marker = self.marker(frame, pre_commit_ns);
        file.write_all_at(&marker, 0)?;
        self.request(surface, 6, Args::new(), None)?;
        let commit_queued_ns = monotonic_ns();
        self.buffers[index].busy_frame = Some(frame);
        self.next_buffer_slot = (index + 1) % self.buffers.len();
        self.callback_pending = Some(frame);
        self.frame_sequence = frame;
        let cause = self.cause.unwrap_or_default();
        self.record("frame_commit", pre_commit_ns, format_args!(
            ",\"frame_sequence\":{frame},\"buffer_slot\":{index},\"render_start_ns\":{render_start_ns},\"pre_commit_ns\":{pre_commit_ns},\"commit_queued_ns\":{commit_queued_ns},\"has_new_input_cause\":{},\"cause_kind\":{},\"cause_event_sequence\":{},\"cause_forwarded_event_time_ms_opaque\":{},\"cause_evdev_code\":{},\"cause_raw_state\":{},\"cause_protocol_serial\":{},\"cause_seat_global\":{},\"cause_receipt_ns\":{},\"cause_handler_ns\":{},\"applied_visual_events\":{},\"first_applied_event_sequence\":{},\"last_applied_event_sequence\":{},\"width\":{},\"height\":{},\"continuous_animation\":{}",
            self.cause.is_some(), cause.kind, cause.sequence,
            cause.time.map_or_else(|| "null".to_owned(), |time| time.to_string()),
            cause.code, cause.state, cause.serial, cause.seat, cause.receipt_ns, cause.handler_ns,
            self.applied_count, self.first_applied, cause.sequence, self.width, self.height, self.options.continuous,
        ))?;
        self.cause = None;
        self.applied_count = 0;
        self.first_applied = 0;
        self.dirty = false;
        Ok(())
    }

    fn marker(&self, frame: u64, pre_commit_ns: u64) -> [u8; MARKER_PIXELS * 4] {
        let cause = self.cause.unwrap_or_default();
        let flags: u32 = u32::from(self.cause.is_some())
            | (u32::from(self.options.continuous) << 1)
            | (u32::from(cause.time.is_some()) << 2)
            | (u32::from(self.applied_count > 1) << 3);
        let mut payload = [0u8; MARKER_BYTES];
        payload[0..4].copy_from_slice(b"CWIP");
        payload[4..6].copy_from_slice(&1u16.to_le_bytes());
        payload[6..8].copy_from_slice(&(MARKER_BYTES as u16).to_le_bytes());
        for (offset, value) in [
            (8, flags),
            (12, cause.kind),
            (32, cause.time.unwrap_or(0)),
            (36, cause.code),
            (40, cause.state),
            (44, cause.serial),
            (96, self.width),
            (100, self.height),
            (104, 0),
            (108, std::process::id()),
            (112, cause.seat),
        ] {
            payload[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (offset, value) in [
            (16, frame),
            (24, cause.sequence),
            (48, cause.receipt_ns),
            (56, cause.handler_ns),
            (64, pre_commit_ns),
            (72, self.applied_count),
            (80, self.first_applied),
            (88, cause.sequence),
        ] {
            payload[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        let mut pixels = [0u8; MARKER_PIXELS * 4];
        for (bytes, pixel) in payload.chunks_exact(3).zip(pixels.chunks_exact_mut(4)) {
            let word = 0xff000000u32
                | ((bytes[0] as u32) << 16)
                | ((bytes[1] as u32) << 8)
                | bytes[2] as u32;
            pixel.copy_from_slice(&word.to_ne_bytes());
        }
        pixels
    }
}

fn enum_u32<T: Into<u32>>(value: WEnum<T>) -> u32 {
    match value {
        WEnum::Value(value) => value.into(),
        WEnum::Unknown(value) => value,
    }
}

fn json_number<T: fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "null".to_owned(), |value| value.to_string())
}

fn transition(
    down: &mut HashSet<u32>,
    code: u32,
    state: u32,
    presses: &mut u64,
    releases: &mut u64,
) -> &'static str {
    match state {
        1 => {
            *presses += 1;
            if down.insert(code) {
                "press"
            } else {
                "duplicate_press"
            }
        }
        0 => {
            *releases += 1;
            if down.remove(&code) {
                "release"
            } else {
                "unmatched_release"
            }
        }
        _ => "unknown_state",
    }
}

fn uint(message: &Message<ObjectId, OwnedFd>, index: usize) -> Result<u32> {
    match message.args.get(index) {
        Some(Argument::Uint(value)) => Ok(*value),
        _ => Err("unexpected uint event argument".into()),
    }
}

fn int(message: &Message<ObjectId, OwnedFd>, index: usize) -> Result<i32> {
    match message.args.get(index) {
        Some(Argument::Int(value)) => Ok(*value),
        _ => Err("unexpected int event argument".into()),
    }
}

fn shm_file(length: u64) -> Result<File> {
    // Use a regular unlinked file rather than mmap/memfd crates. Positional writes
    // update the compositor's shared mapping; busy buffers are never written.
    let directory = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    for attempt in 0..32 {
        let path = directory.join(format!(
            ".cw-input-probe-{}-{}-{attempt}",
            std::process::id(),
            monotonic_ns()
        ));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                fs::remove_file(&path)?;
                file.set_len(length)?;
                return Ok(file);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("could not allocate a unique SHM backing file".into())
}

fn run(options: Options) -> Result<()> {
    let start_ns = monotonic_ns();
    let deadline = options
        .duration
        .map(|duration| start_ns.saturating_add(duration.as_nanos().min(u64::MAX as u128) as u64));
    let connection = if let Some(path) = options.socket.as_ref() {
        Connection::from_socket(UnixStream::connect(path)?)?
    } else {
        Connection::connect_to_env()?
    };
    let (sender, receiver) = mpsc::channel();
    let registry = connection.backend().send_request(
        Message {
            sender_id: connection.backend().display_id(),
            opcode: 1,
            args: [Argument::NewId(ObjectId::null())].into_iter().collect(),
        },
        Some(Arc::new(ReceiveData {
            kind: Kind::Registry,
            sender: sender.clone(),
        })),
        None,
    )?;
    let mut app = App {
        width: options.width,
        height: options.height,
        buffers: (0..options.buffers).map(|_| Buffer::default()).collect(),
        next_buffer_slot: 0,
        connection,
        sender,
        options,
        registry,
        compositor: None,
        shm: None,
        wm: None,
        surface: None,
        xdg: None,
        toplevel: None,
        seats: HashMap::new(),
        scratch: Vec::new(),
        configured: false,
        dirty: true,
        callback_pending: None,
        running: true,
        event_sequence: 0,
        key_order: 0,
        button_order: 0,
        key_presses: 0,
        key_releases: 0,
        button_presses: 0,
        button_releases: 0,
        duplicate_presses: 0,
        unmatched_releases: 0,
        frame_sequence: 0,
        cause: None,
        applied_count: 0,
        first_applied: 0,
        visual_seed: 0x204860,
        pointer_position: None,
    };
    app.record("start", start_ns, format_args!(
        ",\"clock\":\"CLOCK_MONOTONIC\",\"clock_id\":{CLOCK_MONOTONIC},\"clock_units\":\"ns\",\"clock_epoch\":\"guest_system_monotonic\",\"receipt_point\":\"backend_event_delivery\",\"marker_version\":1,\"marker_payload_bytes\":{MARKER_BYTES},\"marker_pixels\":{MARKER_PIXELS},\"buffers\":{},\"continuous_animation\":{},\"input_injection\":false,\"repeat_synthesis\":false",
        app.options.buffers, app.options.continuous,
    ))?;
    app.request(
        &app.connection.backend().display_id(),
        0,
        [Argument::NewId(ObjectId::null())].into_iter().collect(),
        Some(Kind::Sync),
    )?;
    while app.running {
        if deadline.is_some_and(|deadline| monotonic_ns() >= deadline) {
            break;
        }
        app.connection.backend().dispatch_inner_queue()?;
        while let Ok(received) = receiver.try_recv() {
            app.handle(received)?;
            if !app.running || deadline.is_some_and(|deadline| monotonic_ns() >= deadline) {
                break;
            }
        }
        if !app.running || deadline.is_some_and(|deadline| monotonic_ns() >= deadline) {
            break;
        }
        app.render()?;
        let Some(guard) = app.connection.prepare_read() else {
            continue;
        };
        let want_write = match app.connection.flush() {
            Ok(()) => false,
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => true,
            Err(error) => return Err(error.into()),
        };
        let now = monotonic_ns();
        let timeout = deadline.map_or(-1, |deadline| {
            deadline
                .saturating_sub(now)
                .div_ceil(1_000_000)
                .min(i32::MAX as u64) as i32
        });
        let mut fd = PollFd {
            fd: guard.connection_fd().as_raw_fd(),
            events: 1 | if want_write { 4 } else { 0 },
            revents: 0,
        };
        // One live pollfd with the platform's nfds_t ABI. The prepared read is
        // cancelled by dropping guard on timeout, EINTR, or write-only readiness.
        let ready = unsafe { poll(&mut fd, 1, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if ready > 0 && fd.revents & (1 | 8 | 16 | 32) != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    app.record("stop", monotonic_ns(), format_args!(
        ",\"reason\":\"{}\",\"observed_input_events\":{},\"key_events\":{},\"button_events\":{},\"key_presses\":{},\"key_releases\":{},\"button_presses\":{},\"button_releases\":{},\"duplicate_presses\":{},\"unmatched_releases\":{},\"frames_committed\":{},\"uncommitted_visual_events\":{},\"busy_buffers\":{},\"pending_callback_frame_sequence\":{}",
        if app.running { "duration" } else { "window_close" }, app.event_sequence,
        app.key_order, app.button_order, app.key_presses, app.key_releases,
        app.button_presses, app.button_releases, app.duplicate_presses, app.unmatched_releases,
        app.frame_sequence, app.applied_count, app.buffers.iter().filter(|buffer| buffer.busy_frame.is_some()).count(),
        app.callback_pending.map_or_else(|| "null".to_owned(), |frame| frame.to_string()),
    ))?;
    // Disconnect only this client. Do not wait for callbacks of an occluded window.
    Ok(())
}

fn main() {
    let result = options().and_then(|options| match options {
        Some(options) => run(options),
        None => Ok(()),
    });
    if let Err(error) = result {
        eprintln!(
            "{{\"probe\":\"{APP_ID}\",\"schema\":1,\"record\":\"error\",\"message\":{}}}",
            json_string(&error.to_string())
        );
        std::process::exit(1);
    }
}
