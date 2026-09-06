use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::network::{HostProxyBridge, has_proxy_environment};
use crate::runtime_paths::{build_child_path, resolve_command_path, shell_single_quote};

#[derive(Debug, Clone, Deserialize)]
pub struct ContainerSession {
    pub name: String,
    pub image: String,
    #[serde(default = "default_runtime")]
    pub runtime: String,
    pub display: Option<String>,
    pub presentation: Option<String>,
    pub profile: Option<String>,
    pub app: Option<String>,
    pub command: Option<String>,
    pub socket: Option<String>,
    pub container_socket: Option<String>,
    pub waypipe_path: Option<String>,
    pub waypipe_compress: Option<String>,
    pub waypipe_threads: Option<usize>,
    #[serde(default = "default_audio")]
    pub audio: bool,
    #[serde(default)]
    pub runtime_args: Vec<String>,
    #[serde(default)]
    pub mounts: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    session: Vec<ContainerSession>,
}

pub struct SessionLoadReport {
    pub sessions: Vec<ContainerSession>,
    pub error: Option<String>,
}

#[derive(Clone, Copy)]
enum ContainerRuntime {
    Apple,
    Docker,
    OrbStack,
}

#[derive(Debug)]
pub struct LaunchReport {
    pub container_child: Option<Child>,
    pub waypipe_child: Child,
    pub runtime: String,
    pub container_name: String,
    pub host_socket: String,
    pub container_socket: String,
    pub audio_host_socket: Option<String>,
    pub command: String,
}

#[derive(Debug)]
pub struct CheckReport {
    pub runtime: String,
    pub image: String,
    pub command: String,
    pub waypipe: String,
    pub runtime_binary: String,
    pub container_name: String,
    pub running: bool,
}

#[derive(Debug)]
pub enum LaunchError {
    MissingCommand {
        command: String,
        hint: String,
    },
    InvalidSocketPath {
        host_socket: String,
    },
    CreateSocketDir {
        dir: String,
        source: std::io::Error,
    },
    StartContainer {
        session: String,
        source: std::io::Error,
    },
    RuntimeNotReady {
        runtime: String,
        hint: String,
    },
    ImageUnavailable {
        runtime: String,
        image: String,
        hint: String,
    },
    ImageNotGuiReady {
        runtime: String,
        image: String,
        hint: String,
    },
    ContainerAlreadyRunning {
        runtime: String,
        container_name: String,
        hint: String,
    },
    UnsupportedDisplay {
        display: String,
        hint: String,
    },
    SocketTimeout {
        host_socket: String,
    },
    StartWaypipe {
        source: std::io::Error,
    },
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaunchError::MissingCommand { command, hint } => {
                write!(f, "Missing command '{}'. {}", command, hint)
            }
            LaunchError::InvalidSocketPath { host_socket } => {
                write!(
                    f,
                    "Invalid host socket path '{}'. Keep the base path below {} bytes so waypipe can create its derived client sockets on macOS.",
                    host_socket,
                    UNIX_SOCKET_PATH_BYTES - WAYPIPE_CLIENT_SUFFIX_RESERVE
                )
            }
            LaunchError::CreateSocketDir { dir, source } => {
                write!(f, "Failed to create socket directory '{}': {}", dir, source)
            }
            LaunchError::StartContainer { session, source } => {
                write!(
                    f,
                    "Failed to start container session '{}': {}",
                    session, source
                )
            }
            LaunchError::RuntimeNotReady { runtime, hint } => {
                write!(f, "{} is not ready. {}", runtime, hint)
            }
            LaunchError::ImageUnavailable {
                runtime,
                image,
                hint,
            } => {
                write!(
                    f,
                    "{} image '{}' is not available locally. {}",
                    runtime, image, hint
                )
            }
            LaunchError::ImageNotGuiReady {
                runtime,
                image,
                hint,
            } => {
                write!(
                    f,
                    "{} image '{}' is not GUI-ready. {}",
                    runtime, image, hint
                )
            }
            LaunchError::ContainerAlreadyRunning {
                runtime,
                container_name,
                hint,
            } => {
                write!(
                    f,
                    "{} session container '{}' is already running. {}",
                    runtime, container_name, hint
                )
            }
            LaunchError::UnsupportedDisplay { display, hint } => {
                write!(f, "Display '{}' is not available. {}", display, hint)
            }
            LaunchError::SocketTimeout { host_socket } => {
                write!(
                    f,
                    "Timed out waiting for container socket at '{}'",
                    host_socket
                )
            }
            LaunchError::StartWaypipe { source } => {
                write!(f, "Failed to start waypipe client: {}", source)
            }
        }
    }
}

impl LaunchError {
    pub fn is_apple_container_transport_blocked(&self) -> bool {
        matches!(
            self,
            LaunchError::RuntimeNotReady { runtime, .. }
                if runtime.contains("Apple Container GUI transport")
        )
    }

    pub fn is_container_already_running(&self) -> bool {
        matches!(self, LaunchError::ContainerAlreadyRunning { .. })
    }

    pub fn is_unsupported_display(&self) -> bool {
        matches!(self, LaunchError::UnsupportedDisplay { .. })
    }
}

const RELAY_ENV: &str = "COCOA_WAY_CONTAINER_RELAY";
const RELAY_IMAGE_ENV: &str = "COCOA_WAY_RELAY_IMAGE";
const RELAY_NAME_ENV: &str = "COCOA_WAY_RELAY_CONTAINER_NAME";
const RELAY_ENV_LIST_ENV: &str = "COCOA_WAY_RELAY_ENV";
const RELAY_MOUNTS_ENV: &str = "COCOA_WAY_RELAY_MOUNTS";
const RELAY_RUNTIME_ARGS_ENV: &str = "COCOA_WAY_RELAY_RUNTIME_ARGS";
const RELAY_TRANSPORT_ENV: &str = "COCOA_WAY_APPLE_TRANSPORT";
const RELAY_AUDIO_ENV: &str = "COCOA_WAY_RELAY_AUDIO";
const RELAY_AUDIO_HOST_ENV: &str = "COCOA_WAY_RELAY_AUDIO_HOST";
const GUEST_AUDIO_SOCKET_ENV: &str = "COCOA_WAY_AUDIO_SOCKET";

pub fn should_run_container_relay() -> bool {
    std::env::var_os(RELAY_ENV).is_some()
        || (std::env::var_os(RELAY_IMAGE_ENV).is_some()
            && std::env::args().skip(1).any(|arg| arg == "-R"))
}

pub fn run_container_relay_from_env() -> i32 {
    match run_container_relay(std::env::args().skip(1).collect()) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("cocoa-way container relay failed: {}", error);
            1
        }
    }
}

fn run_container_relay(args: Vec<String>) -> Result<(), String> {
    let relay = parse_waypipe_ssh_relay_args(&args)?;
    let image =
        std::env::var(RELAY_IMAGE_ENV).map_err(|_| format!("missing {}", RELAY_IMAGE_ENV))?;
    let container_name =
        std::env::var(RELAY_NAME_ENV).unwrap_or_else(|_| "cocoa-way-apple-relay".into());
    let child_path = build_child_path();
    let container = resolve_command_path("container", None, "container", &child_path)
        .ok_or_else(|| "missing command `container`".to_string())?;

    let preference = std::env::var(RELAY_TRANSPORT_ENV)
        .unwrap_or_else(|_| "auto".into())
        .to_ascii_lowercase();
    let publish_supported = apple_publish_socket_supported(&container, &child_path);
    let mut environment = relay_env_list(RELAY_ENV_LIST_ENV);
    let proxy_bridge = if has_proxy_environment(&environment) {
        None
    } else {
        match HostProxyBridge::start(&container, &child_path) {
            Ok(Some(bridge)) => {
                environment.extend(bridge.container_environment());
                eprintln!(
                    "cocoa-way network: forwarding the host loopback proxy to Apple Container"
                );
                Some(bridge)
            }
            Ok(None) => None,
            Err(error) => {
                eprintln!(
                    "cocoa-way network: host proxy bridge unavailable: {}",
                    error
                );
                None
            }
        }
    };
    if preference != "stdio" && publish_supported {
        match run_publish_socket_relay(
            &relay,
            &container,
            &child_path,
            &image,
            &container_name,
            &environment,
        ) {
            Ok(()) => return Ok(()),
            Err(PublishRelayError::Active(error)) => return Err(error),
            Err(PublishRelayError::Startup(error)) => {
                eprintln!(
                    "cocoa-way transport v2 startup failed: {}; falling back to stdio relay",
                    error
                );
                cleanup_named_runtime_container(
                    ContainerRuntime::Apple,
                    &container,
                    &child_path,
                    &container_name,
                );
            }
        }
    } else if preference != "stdio" {
        eprintln!("cocoa-way transport v2 is unavailable; falling back to stdio relay");
    }

    let result = run_stdio_relay(
        &relay,
        &container,
        &child_path,
        &image,
        &container_name,
        &environment,
    );
    drop(proxy_bridge);
    result
}

fn run_stdio_relay(
    relay: &WaypipeRelayArgs,
    container: &Path,
    child_path: &str,
    image: &str,
    container_name: &str,
    environment: &[String],
) -> Result<(), String> {
    eprintln!("cocoa-way container relay: using stdio compatibility transport");

    let mut cmd = Command::new(container);
    cmd.env("PATH", &child_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .arg("run")
        .arg("--rm")
        .arg("-i")
        .arg("--name")
        .arg(container_name)
        .arg("--env")
        .arg(format!("XDG_RUNTIME_DIR={}", default_guest_runtime_dir()));
    for env in environment {
        if !env.starts_with("XDG_RUNTIME_DIR=") {
            cmd.arg("--env").arg(env);
        }
    }
    for mount in relay_env_list(RELAY_MOUNTS_ENV) {
        cmd.arg("--mount").arg(mount);
    }
    for arg in relay_env_list(RELAY_RUNTIME_ARGS_ENV) {
        cmd.arg(arg);
    }
    cmd.arg(image)
        .arg("perl")
        .arg("-e")
        .arg(guest_relay_perl())
        .arg(&relay.remote_socket)
        .args(&relay.remote_command);

    let mut child = cmd.spawn().map_err(|error| error.to_string())?;
    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| "container relay stdin unavailable".to_string())?;
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| "container relay stdout unavailable".to_string())?;

    let child_stdin = Arc::new(Mutex::new(child_stdin));
    let relay_result = bridge_relay_frames(
        &mut child_stdout,
        Arc::clone(&child_stdin),
        &relay.local_socket,
    );
    let status = child.wait().map_err(|error| error.to_string())?;
    relay_result?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("container relay exited with {}", status))
    }
}

enum PublishRelayError {
    Startup(String),
    Active(String),
}

fn run_publish_socket_relay(
    relay: &WaypipeRelayArgs,
    container: &Path,
    child_path: &str,
    image: &str,
    container_name: &str,
    environment: &[String],
) -> Result<(), PublishRelayError> {
    let host_transport =
        relay_transport_host_socket(&relay.local_socket).map_err(PublishRelayError::Startup)?;
    let guest_transport = format!("/tmp/cocoa-way/transport-v2-{}.sock", std::process::id());
    let audio_enabled = std::env::var_os(RELAY_AUDIO_ENV).is_some();
    let host_audio = if audio_enabled {
        match std::env::var(RELAY_AUDIO_HOST_ENV) {
            Ok(path) if !path.trim().is_empty() => Some(path),
            _ => Some(
                relay_audio_host_socket(&relay.local_socket).map_err(PublishRelayError::Startup)?,
            ),
        }
    } else {
        None
    };
    let guest_audio = format!("/tmp/cocoa-way/audio-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&host_transport);
    if let Some(host_audio) = host_audio.as_deref() {
        let _ = std::fs::remove_file(host_audio);
    }

    let mut cmd = Command::new(container);
    cmd.env("PATH", child_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .arg("run")
        .arg("--rm")
        .arg("--name")
        .arg(container_name)
        .arg("--publish-socket")
        .arg(format!("{}:{}", host_transport, guest_transport))
        .arg("--env")
        .arg(format!("XDG_RUNTIME_DIR={}", default_guest_runtime_dir()));
    if let Some(host_audio) = host_audio.as_deref() {
        cmd.arg("--publish-socket")
            .arg(format!("{}:{}", host_audio, guest_audio))
            .arg("--env")
            .arg(format!("{}={}", GUEST_AUDIO_SOCKET_ENV, guest_audio));
    }
    for env in environment {
        if !env.starts_with("XDG_RUNTIME_DIR=") {
            cmd.arg("--env").arg(env);
        }
    }
    for mount in relay_env_list(RELAY_MOUNTS_ENV) {
        cmd.arg("--mount").arg(mount);
    }
    for arg in relay_env_list(RELAY_RUNTIME_ARGS_ENV) {
        cmd.arg(arg);
    }
    cmd.arg(image)
        .arg("perl")
        .arg("-e")
        .arg(guest_publish_socket_relay_perl())
        .arg(&relay.remote_socket)
        .arg(&guest_transport)
        .args(&relay.remote_command);

    let mut child = cmd
        .spawn()
        .map_err(|error| PublishRelayError::Startup(error.to_string()))?;
    let child_stdout = child.stdout.take().ok_or_else(|| {
        PublishRelayError::Startup("container transport control channel unavailable".into())
    })?;
    if let Err(error) =
        wait_for_publish_relay_ready(child_stdout, &mut child, Duration::from_secs(10))
    {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&host_transport);
        if let Some(host_audio) = host_audio.as_deref() {
            let _ = std::fs::remove_file(host_audio);
        }
        return Err(PublishRelayError::Startup(error));
    }
    let mut transport =
        match connect_published_socket(&host_transport, &mut child, Duration::from_secs(10)) {
            Ok(socket) => socket,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&host_transport);
                if let Some(host_audio) = host_audio.as_deref() {
                    let _ = std::fs::remove_file(host_audio);
                }
                return Err(PublishRelayError::Startup(error));
            }
        };
    transport
        .write_all(RELAY_V2_PREAMBLE)
        .and_then(|_| transport.flush())
        .map_err(|error| {
            PublishRelayError::Startup(format!("transport handshake failed: {}", error))
        })?;
    eprintln!("cocoa-way container relay: using publish-socket transport v2");
    let writer = transport
        .try_clone()
        .map_err(|error| PublishRelayError::Active(error.to_string()))?;
    let relay_result = bridge_relay_frames(
        &mut transport,
        Arc::new(Mutex::new(writer)),
        &relay.local_socket,
    );
    let _ = transport.shutdown(std::net::Shutdown::Both);
    let status = child
        .wait()
        .map_err(|error| PublishRelayError::Active(error.to_string()))?;
    let _ = std::fs::remove_file(&host_transport);
    if let Some(host_audio) = host_audio.as_deref() {
        let _ = std::fs::remove_file(host_audio);
    }
    relay_result.map_err(PublishRelayError::Active)?;
    if status.success() {
        Ok(())
    } else {
        Err(PublishRelayError::Active(format!(
            "publish-socket relay exited with {}",
            status
        )))
    }
}

fn bridge_relay_frames<R, W>(
    reader: &mut R,
    writer: Arc<Mutex<W>>,
    local_socket: &str,
) -> Result<(), String>
where
    R: Read,
    W: Write + Send + 'static,
{
    let mut streams = HashMap::<u32, UnixStream>::new();
    while let Some(frame) = read_relay_frame(reader)? {
        match frame.kind {
            RELAY_FRAME_OPEN => {
                let socket = match connect_unix_socket(local_socket, Duration::from_secs(8)) {
                    Ok(socket) => socket,
                    Err(error) => {
                        eprintln!("cocoa-way container relay stream {}: {}", frame.id, error);
                        let _ = write_relay_frame_locked(&writer, RELAY_FRAME_CLOSE, frame.id, &[]);
                        continue;
                    }
                };
                let mut reader = socket.try_clone().map_err(|error| error.to_string())?;
                let relay_input = Arc::clone(&writer);
                let stream_id = frame.id;
                thread::spawn(move || {
                    let mut buffer = [0u8; 64 * 1024];
                    loop {
                        match reader.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(length) => {
                                if write_relay_frame_locked(
                                    &relay_input,
                                    RELAY_FRAME_DATA,
                                    stream_id,
                                    &buffer[..length],
                                )
                                .is_err()
                                {
                                    return;
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                                continue;
                            }
                            Err(_) => break,
                        }
                    }
                    let _ =
                        write_relay_frame_locked(&relay_input, RELAY_FRAME_CLOSE, stream_id, &[]);
                });
                streams.insert(frame.id, socket);
            }
            RELAY_FRAME_DATA => {
                if let Some(stream) = streams.get_mut(&frame.id) {
                    if stream.write_all(&frame.payload).is_err() {
                        streams.remove(&frame.id);
                        let _ = write_relay_frame_locked(&writer, RELAY_FRAME_CLOSE, frame.id, &[]);
                    }
                }
            }
            RELAY_FRAME_CLOSE => {
                if let Some(stream) = streams.remove(&frame.id) {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
            _ => return Err(format!("unknown container relay frame type {}", frame.kind)),
        }
    }
    streams.clear();
    Ok(())
}

pub fn apple_publish_socket_supported(container: &Path, child_path: &str) -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        Command::new(container)
            .env("PATH", child_path)
            .args(["run", "--help"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).contains("--publish-socket"))
            .unwrap_or(false)
    })
}

fn relay_transport_host_socket(local_socket: &str) -> Result<String, String> {
    relay_aux_host_socket(local_socket, "v2")
}

fn relay_audio_host_socket(local_socket: &str) -> Result<String, String> {
    relay_aux_host_socket(local_socket, "audio")
}

fn relay_aux_host_socket(local_socket: &str, kind: &str) -> Result<String, String> {
    // The relay runs in a child Cocoa-Way process, so a path derived from
    // std::process::id() differs from the one later computed by the main
    // process. Anchor auxiliary sockets beside the already-stable session
    // socket instead.
    let parent = Path::new(local_socket)
        .parent()
        .ok_or_else(|| format!("session socket `{}` has no parent directory", local_socket))?;
    create_private_socket_dir(&parent)
        .map_err(|error| format!("failed to create transport directory: {}", error))?;
    let socket = parent.join(format!(
        "{}-{:016x}.sock",
        kind,
        stable_name_hash(local_socket)
    ));
    let socket = socket.display().to_string();
    if socket.as_bytes().len() >= UNIX_SOCKET_PATH_BYTES {
        return Err(format!("auxiliary socket path is too long: `{}`", socket));
    }
    Ok(socket)
}

fn connect_published_socket(
    path: &str,
    child: &mut Child,
    timeout: Duration,
) -> Result<UnixStream, String> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    while Instant::now() < deadline {
        match UnixStream::connect(path) {
            Ok(socket) => return Ok(socket),
            Err(error) => last_error = Some(error),
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!(
                "container exited before transport socket became ready ({})",
                status
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "timed out connecting to published transport socket `{}`: {}",
        path,
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "socket was not created".into())
    ))
}

fn wait_for_publish_relay_ready(
    stdout: impl Read + Send + 'static,
    child: &mut Child,
    timeout: Duration,
) -> Result<(), String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut ready_sent = false;
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) if line == RELAY_V2_READY_LINE => {
                    ready_sent = true;
                    let _ = sender.send(Ok(()));
                }
                Ok(line) => eprintln!("[container stdout] {}", line),
                Err(error) => {
                    if !ready_sent {
                        let _ = sender.send(Err(format!(
                            "failed to read container transport readiness: {}",
                            error
                        )));
                    }
                    break;
                }
            }
        }
        if !ready_sent {
            let _ = sender.send(Err(
                "container exited before its publish-socket listener became ready".into(),
            ));
        }
    });

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("container transport control channel closed".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!(
                "container exited before its publish-socket listener became ready ({})",
                status
            ));
        }
    }
    Err("timed out waiting for the container publish-socket listener".into())
}

const RELAY_FRAME_OPEN: u8 = 1;
const RELAY_FRAME_DATA: u8 = 2;
const RELAY_FRAME_CLOSE: u8 = 3;
const RELAY_FRAME_HEADER_LEN: usize = 9;
const RELAY_FRAME_MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const RELAY_V2_PREAMBLE: &[u8] = b"CWV2\x01";
const RELAY_V2_READY_LINE: &str = "COCOA_WAY_TRANSPORT_V2_READY";

struct RelayFrame {
    kind: u8,
    id: u32,
    payload: Vec<u8>,
}

fn read_relay_frame(reader: &mut impl Read) -> Result<Option<RelayFrame>, String> {
    let mut header = [0u8; RELAY_FRAME_HEADER_LEN];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(format!("failed to read container relay frame: {}", error)),
    }
    let id = u32::from_be_bytes(header[1..5].try_into().unwrap());
    let length = u32::from_be_bytes(header[5..9].try_into().unwrap()) as usize;
    if length > RELAY_FRAME_MAX_PAYLOAD {
        return Err(format!(
            "container relay frame is too large: {} bytes",
            length
        ));
    }
    let mut payload = vec![0u8; length];
    reader
        .read_exact(&mut payload)
        .map_err(|error| format!("failed to read container relay payload: {}", error))?;
    Ok(Some(RelayFrame {
        kind: header[0],
        id,
        payload,
    }))
}

fn write_relay_frame_locked<W: Write>(
    writer: &Arc<Mutex<W>>,
    kind: u8,
    id: u32,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| std::io::Error::other("container relay writer lock poisoned"))?;
    write_relay_frame(&mut *writer, kind, id, payload)
}

fn write_relay_frame(
    writer: &mut impl Write,
    kind: u8,
    id: u32,
    payload: &[u8],
) -> std::io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::other("container relay payload is too large"))?;
    let mut header = [0u8; RELAY_FRAME_HEADER_LEN];
    header[0] = kind;
    header[1..5].copy_from_slice(&id.to_be_bytes());
    header[5..9].copy_from_slice(&length.to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()
}

struct WaypipeRelayArgs {
    remote_socket: String,
    local_socket: String,
    remote_command: Vec<String>,
}

fn parse_waypipe_ssh_relay_args(args: &[String]) -> Result<WaypipeRelayArgs, String> {
    let mut remote_socket = None;
    let mut local_socket = None;
    let mut remote_command = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-R" if i + 1 < args.len() => {
                let Some((remote, local)) = args[i + 1].split_once(':') else {
                    return Err(format!("invalid -R socket mapping `{}`", args[i + 1]));
                };
                remote_socket = Some(remote.to_string());
                local_socket = Some(local.to_string());
                i += 2;
            }
            "--" => {
                remote_command = Some(args[i + 1..].to_vec());
                break;
            }
            _ => i += 1,
        }
    }

    let remote_socket = remote_socket.ok_or_else(|| "missing ssh -R remote socket".to_string())?;
    let local_socket = local_socket.ok_or_else(|| "missing ssh -R local socket".to_string())?;
    let remote_command = remote_command.ok_or_else(|| "missing remote command".to_string())?;
    if remote_command.is_empty() {
        return Err("empty remote command".into());
    }
    Ok(WaypipeRelayArgs {
        remote_socket,
        local_socket,
        remote_command,
    })
}

fn relay_env_list(name: &str) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn connect_unix_socket(path: &str, timeout: Duration) -> Result<UnixStream, String> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    while Instant::now() < deadline {
        match UnixStream::connect(path) {
            Ok(socket) => return Ok(socket),
            Err(error) => {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(format!(
        "failed to connect local waypipe socket `{}`: {}",
        path,
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "timed out".into())
    ))
}

fn guest_relay_perl() -> &'static str {
    r#"
use strict;
use warnings;
use IO::Socket::UNIX;
use IO::Select;
use File::Path qw(make_path);
use IO::Handle;
use POSIX qw(WNOHANG);
my $sock_path = shift @ARGV;
my $dir = $sock_path;
$dir =~ s{/[^/]+$}{};
make_path($dir) if length($dir) && !-d $dir;
make_path($ENV{XDG_RUNTIME_DIR}) if exists $ENV{XDG_RUNTIME_DIR} && length($ENV{XDG_RUNTIME_DIR}) && !-d $ENV{XDG_RUNTIME_DIR};
unlink $sock_path;
my $server = IO::Socket::UNIX->new(Type => SOCK_STREAM, Local => $sock_path, Listen => 128) or die "listen $sock_path: $!\n";
my $pid = fork();
die "fork: $!\n" unless defined $pid;
if ($pid == 0) {
  exec @ARGV;
  die "exec @ARGV: $!\n";
}
binmode STDIN;
binmode STDOUT;
my $sel = IO::Select->new(\*STDIN, $server);
my %peers;
my %peer_ids;
my $next_id = 1;
my $input = '';

sub write_all {
  my ($fh, $data) = @_;
  my $offset = 0;
  while ($offset < length($data)) {
    my $written = syswrite($fh, $data, length($data) - $offset, $offset);
    return 0 unless defined $written && $written > 0;
    $offset += $written;
  }
  return 1;
}

sub send_frame {
  my ($kind, $id, $data) = @_;
  $data = '' unless defined $data;
  return write_all(\*STDOUT, pack('CNN', $kind, $id, length($data)) . $data);
}

sub close_peer {
  my ($id, $notify) = @_;
  my $peer = delete $peers{$id};
  return unless $peer;
  delete $peer_ids{fileno($peer)};
  $sel->remove($peer);
  close $peer;
  send_frame(3, $id, '') if $notify;
}

while (1) {
  last if waitpid($pid, WNOHANG) == $pid;
  my @ready = $sel->can_read(0.25);
  for my $fh (@ready) {
    if ($fh == $server) {
      my $peer = $server->accept();
      next unless $peer;
      binmode $peer;
      my $id = $next_id++;
      $peers{$id} = $peer;
      $peer_ids{fileno($peer)} = $id;
      $sel->add($peer);
      send_frame(1, $id, '') or exit 1;
      next;
    }

    if ($fh == \*STDIN) {
      my $chunk = '';
      my $n = sysread(STDIN, $chunk, 65536);
      exit 0 unless defined $n && $n > 0;
      $input .= $chunk;
      while (length($input) >= 9) {
        my ($kind, $id, $length) = unpack('CNN', substr($input, 0, 9));
        die "relay frame too large\n" if $length > 16 * 1024 * 1024;
        last if length($input) < 9 + $length;
        substr($input, 0, 9, '');
        my $data = substr($input, 0, $length, '');
        if ($kind == 2 && exists $peers{$id}) {
          close_peer($id, 1) unless write_all($peers{$id}, $data);
        } elsif ($kind == 3) {
          close_peer($id, 0);
        }
      }
      next;
    }

    my $id = $peer_ids{fileno($fh)};
    my $data = '';
    my $n = sysread($fh, $data, 65536);
    if (!defined $n || $n == 0) {
      close_peer($id, 1);
    } else {
      send_frame(2, $id, $data) or exit 1;
    }
  }
}
for my $id (keys %peers) {
  close_peer($id, 1);
}
waitpid($pid, 0);
"#
}

fn guest_publish_socket_relay_perl() -> &'static str {
    r#"
use strict;
use warnings;
use IO::Socket::UNIX;
use IO::Select;
use File::Path qw(make_path);
use POSIX qw(WNOHANG);
my $sock_path = shift @ARGV;
my $transport_path = shift @ARGV;
for my $path ($sock_path, $transport_path) {
  my $dir = $path;
  $dir =~ s{/[^/]+$}{};
  make_path($dir) if length($dir) && !-d $dir;
  unlink $path;
}
make_path($ENV{XDG_RUNTIME_DIR}) if exists $ENV{XDG_RUNTIME_DIR} && length($ENV{XDG_RUNTIME_DIR}) && !-d $ENV{XDG_RUNTIME_DIR};
my $server = IO::Socket::UNIX->new(Type => SOCK_STREAM, Local => $sock_path, Listen => 128) or die "listen $sock_path: $!\n";
my $transport_server = IO::Socket::UNIX->new(Type => SOCK_STREAM, Local => $transport_path, Listen => 1) or die "listen $transport_path: $!\n";
my $audio_pid;
if (exists $ENV{COCOA_WAY_AUDIO_SOCKET} && length($ENV{COCOA_WAY_AUDIO_SOCKET}) && -x "/usr/local/bin/cocoa-way-audio-relay") {
  my $ready = "/tmp/cocoa-way-audio.ready";
  unlink $ready;
  $audio_pid = fork();
  die "audio fork: $!\n" unless defined $audio_pid;
  if ($audio_pid == 0) {
    $ENV{COCOA_WAY_AUDIO_READY} = $ready;
    exec "/usr/local/bin/cocoa-way-audio-relay";
    die "exec cocoa-way-audio-relay: $!\n";
  }
  my $attempt = 0;
  while ((!-e $ready || !-S $ENV{COCOA_WAY_AUDIO_SOCKET}) && $attempt < 200) {
    $attempt++;
    select undef, undef, undef, 0.05;
  }
  die "audio relay did not become ready\n" unless -e $ready && -S $ENV{COCOA_WAY_AUDIO_SOCKET};
}
STDOUT->autoflush(1);
print STDOUT "COCOA_WAY_TRANSPORT_V2_READY\n";
my $pid = fork();
die "fork: $!\n" unless defined $pid;
if ($pid == 0) {
  exec @ARGV;
  die "exec @ARGV: $!\n";
}
my $transport = $transport_server->accept() or die "accept transport: $!\n";
binmode $transport;
my $hello = '';
while (length($hello) < 5) {
  my $n = sysread($transport, $hello, 5 - length($hello), length($hello));
  die "transport handshake ended early\n" unless defined $n && $n > 0;
}
die "unsupported transport handshake\n" unless $hello eq "CWV2\x01";
print STDERR "cocoa-way transport v2: host channel connected\n";
my $sel = IO::Select->new($transport, $server);
my %peers;
my %peer_ids;
my $next_id = 1;
my $input = '';
my $child_exited = 0;

sub write_all {
  my ($fh, $data) = @_;
  my $offset = 0;
  while ($offset < length($data)) {
    my $written = syswrite($fh, $data, length($data) - $offset, $offset);
    return 0 unless defined $written && $written > 0;
    $offset += $written;
  }
  return 1;
}

sub send_frame {
  my ($kind, $id, $data) = @_;
  $data = '' unless defined $data;
  return write_all($transport, pack('CNN', $kind, $id, length($data)) . $data);
}

sub close_peer {
  my ($id, $notify) = @_;
  my $peer = delete $peers{$id};
  return unless $peer;
  delete $peer_ids{fileno($peer)};
  $sel->remove($peer);
  close $peer;
  send_frame(3, $id, '') if $notify;
}

RELAY: while (1) {
  if (waitpid($pid, WNOHANG) == $pid) {
    $child_exited = 1;
    last RELAY;
  }
  my @ready = $sel->can_read(0.25);
  for my $fh (@ready) {
    if ($fh == $server) {
      my $peer = $server->accept();
      next unless $peer;
      binmode $peer;
      my $id = $next_id++;
      $peers{$id} = $peer;
      $peer_ids{fileno($peer)} = $id;
      $sel->add($peer);
      print STDERR "cocoa-way transport v2: opened stream $id\n";
      last RELAY unless send_frame(1, $id, '');
      next;
    }

    if ($fh == $transport) {
      my $chunk = '';
      my $n = sysread($transport, $chunk, 65536);
      last RELAY unless defined $n && $n > 0;
      $input .= $chunk;
      while (length($input) >= 9) {
        my ($kind, $id, $length) = unpack('CNN', substr($input, 0, 9));
        die "relay frame too large\n" if $length > 16 * 1024 * 1024;
        last if length($input) < 9 + $length;
        substr($input, 0, 9, '');
        my $data = substr($input, 0, $length, '');
        if ($kind == 2 && exists $peers{$id}) {
          close_peer($id, 1) unless write_all($peers{$id}, $data);
        } elsif ($kind == 3) {
          close_peer($id, 0);
        }
      }
      next;
    }

    my $id = $peer_ids{fileno($fh)};
    my $data = '';
    my $n = sysread($fh, $data, 65536);
    if (!defined $n || $n == 0) {
      close_peer($id, 1);
    } else {
      last RELAY unless send_frame(2, $id, $data);
    }
  }
}
for my $id (keys %peers) {
  close_peer($id, 0);
}
unless ($child_exited) {
  kill 'TERM', $pid;
  waitpid($pid, 0);
}
if (defined $audio_pid) {
  kill 'TERM', $audio_pid;
  waitpid($audio_pid, 0);
}
unlink $sock_path;
unlink $transport_path;
"#
}

fn default_runtime() -> String {
    "container".into()
}

fn default_audio() -> bool {
    true
}

impl ContainerSession {
    pub fn presentation_mode(&self) -> crate::presentation::PresentationMode {
        crate::presentation::PresentationMode::parse(self.presentation.as_deref())
    }
}

pub fn config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home)
        .join(".config/cocoa-way")
        .join("container-sessions.toml")
}

pub fn load_sessions() -> Vec<ContainerSession> {
    load_sessions_report().sessions
}

pub fn load_sessions_report() -> SessionLoadReport {
    let path = config_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let example = r#"# cocoa-way container sessions
# Container Mode owns local Linux GUI sessions. Classic SSH/OrbStack entries
# remain in ~/.config/cocoa-way/connections.toml.

# [[session]]
# name = "Niri Desktop"
# runtime = "container"
# image = "localhost/cocoa-way-niri:latest"
# display = "auto"
# presentation = "desktop"
# profile = "niri"
# command = "niri"
# waypipe_compress = "none"
# audio = true

# [[session]]
# name = "Single App Diagnostic"
# runtime = "container"
# image = "localhost/cocoa-way-smoke:latest"
# profile = "single-app"
# app = "foot"
"#;
            let _ = std::fs::write(&path, example);
            log::info!("Created example container-sessions.toml at {:?}", path);
            return SessionLoadReport {
                sessions: vec![],
                error: None,
            };
        }
    };

    match toml::from_str::<Config>(&content) {
        Ok(cfg) => SessionLoadReport {
            sessions: cfg.session,
            error: None,
        },
        Err(e) => {
            log::warn!("Failed to parse container-sessions.toml: {}", e);
            SessionLoadReport {
                sessions: vec![],
                error: Some(format!("Failed to parse {}: {}", path.display(), e)),
            }
        }
    }
}

pub fn append_session(session: &ContainerSession) -> std::io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut content = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        "# cocoa-way container sessions\n\
         # Container Mode owns local Linux GUI sessions.\n"
            .into()
    });
    if !content.ends_with('\n') {
        content.push('\n');
    }
    if !content.ends_with("\n\n") {
        content.push('\n');
    }
    content.push_str(&session_to_toml(session));

    std::fs::write(path, content)
}

pub fn replace_session(index: usize, session: &ContainerSession) -> std::io::Result<()> {
    let path = config_path();
    let content = std::fs::read_to_string(&path)?;
    let ranges = session_ranges(&content);

    let Some((start, end)) = ranges.get(index).copied() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "session index not found",
        ));
    };

    let mut updated = String::new();
    updated.push_str(&content[..start]);
    if !updated.ends_with('\n') && !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(&session_to_toml(session));
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(content[end..].trim_start_matches('\n'));
    std::fs::write(path, updated)
}

pub fn remove_session(index: usize) -> std::io::Result<()> {
    let path = config_path();
    let content = std::fs::read_to_string(&path)?;
    let ranges = session_ranges(&content);

    let Some((start, end)) = ranges.get(index).copied() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "session index not found",
        ));
    };

    let mut updated = String::new();
    updated.push_str(&content[..start]);
    updated.push_str(content[end..].trim_start_matches('\n'));
    std::fs::write(path, updated)
}

fn session_ranges(content: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut current_start: Option<usize> = None;

    for (offset, line) in line_offsets(&content) {
        if line.trim() == "[[session]]" {
            if let Some(start) = current_start.replace(offset) {
                ranges.push((start, offset));
            }
        }
    }
    if let Some(start) = current_start {
        ranges.push((start, content.len()));
    }
    ranges
}

fn line_offsets(content: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    content.lines().map(move |line| {
        let current = offset;
        offset += line.len() + 1;
        (current, line)
    })
}

pub(crate) fn session_to_toml(session: &ContainerSession) -> String {
    let mut content = String::new();
    content.push_str("[[session]]\n");
    content.push_str(&format!("name = \"{}\"\n", toml_escape(&session.name)));
    content.push_str(&format!(
        "runtime = \"{}\"\n",
        toml_escape(&session.runtime)
    ));
    content.push_str(&format!("image = \"{}\"\n", toml_escape(&session.image)));
    if let Some(display) = session.display.as_deref().filter(|value| !value.is_empty()) {
        content.push_str(&format!("display = \"{}\"\n", toml_escape(display)));
    }
    if let Some(presentation) = session
        .presentation
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        content.push_str(&format!(
            "presentation = \"{}\"\n",
            toml_escape(presentation)
        ));
    }
    if let Some(profile) = session.profile.as_deref().filter(|value| !value.is_empty()) {
        content.push_str(&format!("profile = \"{}\"\n", toml_escape(profile)));
    }
    if let Some(command) = session.command.as_deref().filter(|value| !value.is_empty()) {
        content.push_str(&format!("command = \"{}\"\n", toml_escape(command)));
    }
    if let Some(app) = session.app.as_deref().filter(|value| !value.is_empty()) {
        content.push_str(&format!("app = \"{}\"\n", toml_escape(app)));
    }
    if let Some(socket) = session.socket.as_deref().filter(|value| !value.is_empty()) {
        content.push_str(&format!("socket = \"{}\"\n", toml_escape(socket)));
    }
    if let Some(socket) = session
        .container_socket
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        content.push_str(&format!("container_socket = \"{}\"\n", toml_escape(socket)));
    }
    if let Some(path) = session
        .waypipe_path
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        content.push_str(&format!("waypipe_path = \"{}\"\n", toml_escape(path)));
    }
    if let Some(compress) = session
        .waypipe_compress
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        content.push_str(&format!(
            "waypipe_compress = \"{}\"\n",
            toml_escape(compress)
        ));
    }
    if let Some(threads) = session.waypipe_threads {
        content.push_str(&format!("waypipe_threads = {}\n", threads));
    }
    content.push_str(&format!("audio = {}\n", session.audio));
    if !session.runtime_args.is_empty() {
        content.push_str(&format!(
            "runtime_args = [{}]\n",
            toml_string_list(&session.runtime_args)
        ));
    }
    if !session.mounts.is_empty() {
        content.push_str(&format!(
            "mounts = [{}]\n",
            toml_string_list(&session.mounts)
        ));
    }
    if !session.env.is_empty() {
        content.push_str(&format!("env = [{}]\n", toml_string_list(&session.env)));
    }
    content
}

pub fn check_session(session: &ContainerSession) -> Result<CheckReport, LaunchError> {
    let child_path = build_child_path();
    let waypipe = resolve_command_path(
        "waypipe",
        session.waypipe_path.as_deref(),
        "waypipe",
        &child_path,
    )
    .ok_or_else(|| LaunchError::MissingCommand {
        command: "waypipe".into(),
        hint: "Install waypipe or set waypipe_path in container-sessions.toml.".into(),
    })?;
    let runtime_binary_name = runtime_binary_name(&session.runtime);
    let runtime_kind = normalize_container_runtime(&session.runtime);
    let runtime_binary =
        resolve_command_path(runtime_binary_name, None, runtime_binary_name, &child_path)
            .ok_or_else(|| LaunchError::MissingCommand {
                command: runtime_binary_name.into(),
                hint: format!(
                    "Install {} or choose another runtime for this session.",
                    runtime_label(&session.runtime)
                ),
            })?;
    preflight_session(session, runtime_kind, &runtime_binary, &child_path)?;
    let container_name = container_name(session);
    let running =
        runtime_container_is_running(runtime_kind, &runtime_binary, &child_path, &container_name);
    Ok(CheckReport {
        runtime: runtime_label(&session.runtime).into(),
        image: session.image.clone(),
        command: session_command(session),
        waypipe: waypipe.display().to_string(),
        runtime_binary: runtime_binary.display().to_string(),
        container_name,
        running,
    })
}

pub fn launch_checked_session(
    session: &ContainerSession,
    runtime_dir: &str,
    display: &str,
    check: CheckReport,
    mut progress: impl FnMut(crate::application_model::LaunchStep, &str),
) -> Result<LaunchReport, LaunchError> {
    use crate::application_model::LaunchStep;

    let child_path = build_child_path();
    let waypipe = std::path::PathBuf::from(&check.waypipe);
    let runtime_binary_name = runtime_binary_name(&session.runtime);
    let runtime_kind = normalize_container_runtime(&session.runtime);
    let runtime_binary =
        resolve_command_path(runtime_binary_name, None, runtime_binary_name, &child_path)
            .ok_or_else(|| LaunchError::MissingCommand {
                command: runtime_binary_name.into(),
                hint: format!(
                    "Install {} or choose another runtime for this session.",
                    runtime_label(&session.runtime)
                ),
            })?;
    let host_socket = session
        .socket
        .clone()
        .unwrap_or_else(|| default_container_socket(runtime_dir, &session.name));
    let container_socket = session
        .container_socket
        .clone()
        .unwrap_or_else(|| default_container_socket_path(&host_socket, runtime_kind));
    let container_name = container_name(session);
    if runtime_container_is_running(runtime_kind, &runtime_binary, &child_path, &container_name) {
        if matches!(runtime_kind, ContainerRuntime::Apple) {
            progress(
                LaunchStep::CreateContainer,
                "Removing an untracked Cocoa-Way Apple Container instance left by an earlier run",
            );
            cleanup_named_runtime_container(
                runtime_kind,
                &runtime_binary,
                &child_path,
                &container_name,
            );
        }
        if runtime_container_is_running(runtime_kind, &runtime_binary, &child_path, &container_name)
        {
            return Err(LaunchError::ContainerAlreadyRunning {
                runtime: runtime_label(&session.runtime).into(),
                container_name,
                hint: "Stop the existing container from Container Mode before launching this profile again, or choose a different profile name.".into(),
            });
        }
    }

    let command = session_command(session);
    let server_command = build_server_command(
        &container_socket,
        &command,
        &waypipe_tuning_args(session, runtime_kind),
    );
    let mut cmd = Command::new(&runtime_binary);
    cmd.env("PATH", &child_path);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    match runtime_kind {
        ContainerRuntime::Apple => {
            prepare_host_socket(&host_socket)?;
        }
        ContainerRuntime::Docker | ContainerRuntime::OrbStack => {
            prepare_host_socket(&host_socket)?;
            let socket_parent =
                Path::new(&host_socket)
                    .parent()
                    .ok_or_else(|| LaunchError::InvalidSocketPath {
                        host_socket: host_socket.clone(),
                    })?;
            cmd.arg("run").arg("--rm");
            cmd.arg("--name").arg(&container_name);
            if !has_env_key(&session.env, "XDG_RUNTIME_DIR") {
                cmd.arg("--env")
                    .arg(format!("XDG_RUNTIME_DIR={}", default_guest_runtime_dir()));
            }
            for env in &session.env {
                cmd.arg("--env").arg(env);
            }
            for mount in &session.mounts {
                cmd.arg("--mount").arg(mount);
            }
            for arg in &session.runtime_args {
                cmd.arg(arg);
            }
            cmd.arg("-v")
                .arg(format!(
                    "{}:{}",
                    socket_parent.display(),
                    socket_parent.display()
                ))
                .arg(&session.image)
                .args(["sh", "-lc", &server_command]);
        }
    }

    let audio_host_socket = if matches!(runtime_kind, ContainerRuntime::Apple) && session.audio {
        Some(
            relay_audio_host_socket(&host_socket).map_err(|_| LaunchError::InvalidSocketPath {
                host_socket: host_socket.clone(),
            })?,
        )
    } else {
        None
    };

    let (container_child, waypipe_child) = match runtime_kind {
        ContainerRuntime::Apple => {
            cleanup_named_runtime_container(
                runtime_kind,
                &runtime_binary,
                &child_path,
                &container_name,
            );
            progress(
                LaunchStep::StartWaypipe,
                "Starting the native socket transport and Waypipe client",
            );
            let waypipe_child = spawn_apple_container_waypipe_ssh(
                &waypipe,
                &child_path,
                runtime_dir,
                display,
                session,
                &host_socket,
                &container_socket,
                &container_name,
                &command,
                audio_host_socket.as_deref(),
            )?;
            progress(
                LaunchStep::StartCommand,
                "The application command was handed to Apple Container",
            );
            (None, waypipe_child)
        }
        ContainerRuntime::Docker | ContainerRuntime::OrbStack => {
            progress(
                LaunchStep::StartWaypipe,
                "Starting the Waypipe client for the selected display",
            );
            let mut waypipe_child = spawn_waypipe_client(
                &waypipe,
                &child_path,
                runtime_dir,
                display,
                &host_socket,
                &waypipe_tuning_args(session, runtime_kind),
            )?;
            if !wait_for_socket(&host_socket, Duration::from_secs(8)) {
                let _ = waypipe_child.kill();
                let _ = waypipe_child.wait();
                return Err(LaunchError::SocketTimeout { host_socket });
            }
            let container_child = match cmd.spawn() {
                Ok(child) => child,
                Err(source) => {
                    let _ = waypipe_child.kill();
                    let _ = waypipe_child.wait();
                    return Err(LaunchError::StartContainer {
                        session: session.name.clone(),
                        source,
                    });
                }
            };
            progress(
                LaunchStep::StartCommand,
                "The application command was started inside the container",
            );
            (Some(container_child), waypipe_child)
        }
    };
    Ok(LaunchReport {
        container_child,
        waypipe_child,
        runtime: runtime_label(&session.runtime).into(),
        container_name,
        host_socket,
        container_socket,
        audio_host_socket,
        command,
    })
}

pub fn container_name(session: &ContainerSession) -> String {
    format!("cocoa-way-{}", slugify(&session.name))
}

pub fn terminal_command(session: &ContainerSession) -> String {
    let runtime = runtime_binary_name(&session.runtime);
    let name = shell_single_quote(&container_name(session));
    match normalize_container_runtime(&session.runtime) {
        ContainerRuntime::Apple => format!(
            "{} exec -it {} sh -lc {}",
            runtime,
            name,
            shell_single_quote("exec ${SHELL:-/bin/sh}")
        ),
        ContainerRuntime::Docker | ContainerRuntime::OrbStack => format!(
            "{} exec -it {} sh -lc {}",
            runtime,
            name,
            shell_single_quote("exec ${SHELL:-/bin/sh}")
        ),
    }
}

pub fn cleanup_named_session(session: &ContainerSession) -> Result<(), String> {
    let runtime_kind = normalize_container_runtime(&session.runtime);
    if matches!(runtime_kind, ContainerRuntime::OrbStack) {
        return Ok(());
    }

    let child_path = build_child_path();
    let binary_name = runtime_binary_name(&session.runtime);
    let binary = resolve_command_path(binary_name, None, binary_name, &child_path)
        .ok_or_else(|| format!("Missing command `{}`.", binary_name))?;
    let name = container_name(session);
    cleanup_named_runtime_container(runtime_kind, &binary, &child_path, &name);
    Ok(())
}

fn cleanup_named_runtime_container(
    runtime_kind: ContainerRuntime,
    binary: &Path,
    child_path: &str,
    name: &str,
) {
    let args: Vec<&str> = match runtime_kind {
        ContainerRuntime::Apple => vec!["delete", "--force", name],
        ContainerRuntime::Docker => vec!["rm", "-f", name],
        ContainerRuntime::OrbStack => return,
    };
    let mut command = Command::new(binary);
    command.env("PATH", child_path).args(args);
    match command_output_with_timeout(command, Duration::from_secs(6)) {
        Ok(output) if output.status.success() => {}
        Ok(output) if runtime_output_is_not_found(&output.stderr) => {}
        Ok(output) if runtime_output_is_not_found(&output.stdout) => {}
        Ok(output) => {
            let message = runtime_output_message(&output.stderr)
                .or_else(|| runtime_output_message(&output.stdout))
                .unwrap_or_else(|| "runtime cleanup failed".into());
            log::warn!("Failed to cleanup stale container '{}': {}", name, message);
        }
        Err(RuntimeCommandError::TimedOut) => {
            log::warn!(
                "Timed out cleaning up stale container '{}'; runtime may still be wedged",
                name
            );
        }
        Err(RuntimeCommandError::Io(error)) => {
            log::warn!("Failed to cleanup stale container '{}': {}", name, error);
        }
    }
}

enum RuntimeCommandError {
    Io(std::io::Error),
    TimedOut,
}

fn command_output_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> Result<Output, RuntimeCommandError> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(RuntimeCommandError::Io)?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().map_err(RuntimeCommandError::Io)? {
            Some(status) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut pipe) = child.stdout.take() {
                    pipe.read_to_end(&mut stdout)
                        .map_err(RuntimeCommandError::Io)?;
                }
                if let Some(mut pipe) = child.stderr.take() {
                    pipe.read_to_end(&mut stderr)
                        .map_err(RuntimeCommandError::Io)?;
                }
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RuntimeCommandError::TimedOut);
            }
            None => thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn runtime_output_message(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn runtime_output_is_not_found(output: &[u8]) -> bool {
    let text = String::from_utf8_lossy(output).to_lowercase();
    text.contains("not found") || text.contains("no such container")
}

fn runtime_container_is_running(
    runtime_kind: ContainerRuntime,
    binary: &Path,
    child_path: &str,
    name: &str,
) -> bool {
    match runtime_kind {
        ContainerRuntime::Apple => Command::new(binary)
            .env("PATH", child_path)
            .args(["list", "--all"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| apple_container_list_has_running(&output.stdout, name))
            .unwrap_or(false),
        ContainerRuntime::Docker => Command::new(binary)
            .env("PATH", child_path)
            .args(["inspect", "-f", "{{.State.Running}}", name])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.trim() == "true")
            })
            .unwrap_or(false),
        ContainerRuntime::OrbStack => false,
    }
}

fn apple_container_list_has_running(output: &[u8], name: &str) -> bool {
    String::from_utf8_lossy(output).lines().any(|line| {
        let mut columns = line.split_whitespace();
        matches!(columns.next(), Some(id) if id == name)
            && columns.any(|column| column == "running")
    })
}

fn toml_string_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("\"{}\"", toml_escape(value)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn toml_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect()
}

fn normalize_container_runtime(runtime: &str) -> ContainerRuntime {
    match runtime {
        "docker" => ContainerRuntime::Docker,
        "orb" | "orbstack" => ContainerRuntime::OrbStack,
        _ => ContainerRuntime::Apple,
    }
}

fn preflight_session(
    session: &ContainerSession,
    runtime: ContainerRuntime,
    runtime_binary: &Path,
    child_path: &str,
) -> Result<(), LaunchError> {
    validate_display_slot(session)?;
    validate_presentation_command(session)?;
    match runtime {
        ContainerRuntime::Apple => {
            preflight_apple_container(runtime_binary, child_path)?;
            inspect_image(
                runtime_binary,
                child_path,
                &["image", "inspect", &session.image],
                "Apple Container",
                &session.image,
                "Run `container image pull <image>` or build/tag the image before launching.",
            )?;
            verify_gui_tools_in_image(
                session,
                runtime_binary,
                child_path,
                runtime,
                "Apple Container",
                &session.image,
            )?;
            Ok(())
        }
        ContainerRuntime::Docker => {
            inspect_image(
                runtime_binary,
                child_path,
                &["image", "inspect", &session.image],
                "Docker",
                &session.image,
                "Run `docker pull <image>` or build/tag the image before launching.",
            )?;
            verify_gui_tools_in_image(
                session,
                runtime_binary,
                child_path,
                runtime,
                "Docker",
                &session.image,
            )
        }
        ContainerRuntime::OrbStack => Ok(()),
    }
}

fn validate_presentation_command(session: &ContainerSession) -> Result<(), LaunchError> {
    if !session.presentation_mode().is_rootless() {
        return Ok(());
    }

    let Some(command) = session_command_binary(session) else {
        return Ok(());
    };
    let binary = Path::new(&command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&command)
        .to_ascii_lowercase();
    match binary.as_str() {
        "xterm" => {
            return Err(LaunchError::UnsupportedDisplay {
                display: "rootless".into(),
                hint: "xterm is an X11 application, but Cocoa-Way rootless currently accepts native Wayland applications. Use foot for validation, or use desktop mode until Xwayland integration is available.".into(),
            });
        }
        "niri" | "hyprland" => {
            return Err(LaunchError::UnsupportedDisplay {
                display: "rootless".into(),
                hint: format!(
                    "{} is a desktop compositor. Select desktop presentation for compositor sessions; rootless presentation is for individual native Wayland applications.",
                    command
                ),
            });
        }
        _ => {}
    }

    Ok(())
}

fn validate_display_slot(session: &ContainerSession) -> Result<(), LaunchError> {
    let display = session
        .display
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("auto");
    if display.chars().any(char::is_control) {
        return Err(LaunchError::UnsupportedDisplay {
            display: display.into(),
            hint: "Display names cannot contain control characters.".into(),
        });
    }
    Ok(())
}

fn preflight_apple_container(runtime_binary: &Path, child_path: &str) -> Result<(), LaunchError> {
    let output = Command::new(runtime_binary)
        .env("PATH", child_path)
        .args(["system", "status"])
        .output()
        .map_err(|source| LaunchError::StartContainer {
            session: "Apple Container status check".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(LaunchError::RuntimeNotReady {
            runtime: "Apple Container".into(),
            hint: "Run `container system start` and try again.".into(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.lines().any(is_apple_container_running_status) {
        return Err(LaunchError::RuntimeNotReady {
            runtime: "Apple Container".into(),
            hint: "Run `container system start` and wait until `container system status` reports running.".into(),
        });
    }
    Ok(())
}

fn is_apple_container_running_status(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    matches!(parts.next(), Some("status")) && matches!(parts.next(), Some("running"))
}

fn inspect_image(
    runtime_binary: &Path,
    child_path: &str,
    args: &[&str],
    runtime: &str,
    image: &str,
    hint: &str,
) -> Result<(), LaunchError> {
    match Command::new(runtime_binary)
        .env("PATH", child_path)
        .args(args)
        .output()
    {
        Ok(output) if output.status.success() => Ok(()),
        Ok(_) => Err(LaunchError::ImageUnavailable {
            runtime: runtime.into(),
            image: image.into(),
            hint: hint.into(),
        }),
        Err(source) => Err(LaunchError::StartContainer {
            session: format!("{} image check", runtime),
            source,
        }),
    }
}

fn verify_gui_tools_in_image(
    session: &ContainerSession,
    runtime_binary: &Path,
    child_path: &str,
    runtime: ContainerRuntime,
    runtime_label: &str,
    image: &str,
) -> Result<(), LaunchError> {
    let run_probes = |probes: &[String]| {
        let mut cmd = Command::new(runtime_binary);
        cmd.env("PATH", child_path);
        cmd.arg("run").arg("--rm");
        for env in &session.env {
            cmd.arg("--env").arg(env);
        }
        for mount in &session.mounts {
            cmd.arg("--mount").arg(mount);
        }
        for arg in &session.runtime_args {
            cmd.arg(arg);
        }
        let script = probes.join(" && ");
        cmd.args([image, "sh", "-lc", &script]);
        cmd.output()
    };

    let mut required_probes = vec!["command -v waypipe >/dev/null".to_string()];
    if matches!(runtime, ContainerRuntime::Apple) {
        required_probes.push("command -v perl >/dev/null".into());
    }
    if let Some(binary) = session_command_binary(session) {
        required_probes.push(format!(
            "command -v {} >/dev/null",
            shell_single_quote(&binary)
        ));
    }

    match run_probes(&required_probes) {
        Ok(output) if output.status.success() => {}
        Ok(_) => {
            return Err(LaunchError::ImageNotGuiReady {
            runtime: runtime_label.into(),
            image: image.into(),
            hint: "Install waypipe, Apple relay dependencies, and the configured GUI command in the image, then rebuild/tag it for Cocoa-Way.".into(),
            });
        }
        Err(source) => {
            return Err(LaunchError::StartContainer {
                session: format!("{} GUI readiness check", runtime_label),
                source,
            });
        }
    }

    if matches!(runtime, ContainerRuntime::Apple) && session.audio {
        let audio_probes = [
            "command -v cocoa-way-audio-relay >/dev/null".into(),
            "command -v pipewire >/dev/null".into(),
            "command -v pipewire-pulse >/dev/null".into(),
            "command -v wireplumber >/dev/null".into(),
            "command -v pactl >/dev/null".into(),
            "command -v parec >/dev/null".into(),
        ];
        match run_probes(&audio_probes) {
            Ok(output) if output.status.success() => {}
            Ok(_) => log::warn!(
                "Apple Container image '{}' has no complete Cocoa-Way audio stack; GUI launch will continue without audio",
                image
            ),
            Err(source) => log::warn!(
                "Could not inspect optional audio tools in Apple Container image '{}': {}",
                image,
                source
            ),
        }
    }

    Ok(())
}

fn runtime_binary_name(runtime: &str) -> &str {
    match runtime {
        "docker" => "docker",
        "orb" | "orbstack" => "orb",
        _ => "container",
    }
}

fn runtime_label(runtime: &str) -> &str {
    match runtime {
        "docker" => "Docker",
        "orb" | "orbstack" => "OrbStack",
        _ => "Apple Container",
    }
}

pub fn is_apple_container_session(session: &ContainerSession) -> bool {
    matches!(
        normalize_container_runtime(&session.runtime),
        ContainerRuntime::Apple
    )
}

fn session_command(session: &ContainerSession) -> String {
    if let Some(command) = session.command.as_deref() {
        return command.into();
    }

    match session.profile.as_deref() {
        Some("niri") => "niri".into(),
        Some("shell") => "sh".into(),
        _ => session
            .app
            .clone()
            .unwrap_or_else(|| "weston-terminal".into()),
    }
}

fn session_command_binary(session: &ContainerSession) -> Option<String> {
    let command = session_command(session);
    let first = command.split_whitespace().next()?.trim_matches(['"', '\'']);
    if first.is_empty() || first.contains('=') {
        return None;
    }
    match first {
        "exec" | "env" | "sh" | "bash" => None,
        other => Some(other.into()),
    }
}

const UNIX_SOCKET_PATH_BYTES: usize = 103;
const WAYPIPE_CLIENT_SUFFIX_RESERVE: usize = 28;

fn short_host_socket_root() -> std::path::PathBuf {
    let uid = unsafe { libc::geteuid() };
    std::path::PathBuf::from(format!("/tmp/cw-{}-{}", uid, std::process::id()))
}

fn stable_name_hash(value: &str) -> u64 {
    value
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

fn create_private_socket_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn default_container_socket(_runtime_dir: &str, name: &str) -> String {
    let slug = slugify(name);
    let short_slug = slug.chars().take(24).collect::<String>();
    short_host_socket_root()
        .join(format!(
            "{}-{:08x}.sock",
            short_slug,
            stable_name_hash(name) as u32
        ))
        .to_string_lossy()
        .into_owned()
}

fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if slug.chars().last() != Some('-') {
            slug.push('-');
        }
    }
    while slug.starts_with('-') {
        slug.remove(0);
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("container");
    }
    slug
}

fn default_container_socket_path(host_socket: &str, runtime: ContainerRuntime) -> String {
    match runtime {
        ContainerRuntime::Apple => "/tmp/cocoa-way/waypipe.sock".into(),
        ContainerRuntime::Docker | ContainerRuntime::OrbStack => host_socket.into(),
    }
}

fn default_guest_runtime_dir() -> &'static str {
    "/tmp/cocoa-way-runtime"
}

fn waypipe_tuning_args(session: &ContainerSession, runtime: ContainerRuntime) -> Vec<String> {
    let mut args = Vec::new();
    let compress = session
        .waypipe_compress
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            if matches!(runtime, ContainerRuntime::Apple) {
                Some("none")
            } else {
                None
            }
        });
    if let Some(compress) = compress {
        args.push("--compress".into());
        args.push(compress.into());
    }
    if let Some(threads) = session.waypipe_threads {
        args.push("--threads".into());
        args.push(threads.to_string());
    }
    args
}

fn has_env_key(env: &[String], key: &str) -> bool {
    env.iter().any(|value| {
        value
            .split_once('=')
            .map(|(name, _)| name == key)
            .unwrap_or(false)
    })
}

fn prepare_host_socket(host_socket: &str) -> Result<(), LaunchError> {
    if host_socket.as_bytes().len() + WAYPIPE_CLIENT_SUFFIX_RESERVE > UNIX_SOCKET_PATH_BYTES {
        return Err(LaunchError::InvalidSocketPath {
            host_socket: host_socket.into(),
        });
    }
    let parent = Path::new(host_socket)
        .parent()
        .ok_or_else(|| LaunchError::InvalidSocketPath {
            host_socket: host_socket.into(),
        })?;
    create_private_socket_dir(parent).map_err(|source| LaunchError::CreateSocketDir {
        dir: parent.display().to_string(),
        source,
    })?;
    let _ = std::fs::remove_file(host_socket);
    Ok(())
}

fn build_server_command(container_socket: &str, command: &str, waypipe_args: &[String]) -> String {
    let container_socket = shell_single_quote(container_socket);
    let command = shell_single_quote(&format!("sleep 0.35; exec {}", command));
    let runtime_dir = shell_single_quote(default_guest_runtime_dir());
    let waypipe_args = shell_args(waypipe_args);
    format!(
        "export XDG_RUNTIME_DIR=${{XDG_RUNTIME_DIR:-{runtime_dir}}} && mkdir -p \"$XDG_RUNTIME_DIR\" $(dirname {socket}) && exec waypipe{waypipe_args} --socket {socket} server sh -lc {command}",
        runtime_dir = runtime_dir,
        waypipe_args = waypipe_args,
        socket = container_socket,
        command = command,
    )
}

fn shell_args(args: &[String]) -> String {
    if args.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            args.iter()
                .map(|arg| shell_single_quote(arg))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
}

fn wait_for_socket(host_socket: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if Path::new(host_socket).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn spawn_apple_container_waypipe_ssh(
    waypipe: &Path,
    child_path: &str,
    runtime_dir: &str,
    display: &str,
    session: &ContainerSession,
    host_socket: &str,
    container_socket: &str,
    container_name: &str,
    command: &str,
    audio_host_socket: Option<&str>,
) -> Result<Child, LaunchError> {
    let helper = std::env::current_exe().map_err(|source| LaunchError::StartWaypipe { source })?;
    let helper = helper.to_string_lossy().to_string();
    let remote_command = apple_remote_gui_command(command, uses_nested_wayland_compositor(session));
    let mut cmd = Command::new(waypipe);
    cmd.env("PATH", child_path)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("WAYLAND_DISPLAY", display)
        .env(RELAY_ENV, "1")
        .env(RELAY_IMAGE_ENV, &session.image)
        .env(RELAY_NAME_ENV, container_name)
        .env(RELAY_ENV_LIST_ENV, session.env.join("\n"))
        .env(RELAY_MOUNTS_ENV, session.mounts.join("\n"))
        .env(RELAY_RUNTIME_ARGS_ENV, session.runtime_args.join("\n"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if session.audio {
        cmd.env(RELAY_AUDIO_ENV, "1");
        if let Some(audio_host_socket) = audio_host_socket {
            cmd.env(RELAY_AUDIO_HOST_ENV, audio_host_socket);
        }
    }
    cmd.args(waypipe_tuning_args(session, ContainerRuntime::Apple));
    cmd.args([
        "--socket",
        host_socket,
        "--remote-socket",
        container_socket,
        "--ssh-bin",
        &helper,
        "--remote-bin",
        "waypipe",
        "ssh",
        "apple-container",
        "sh",
        "-lc",
        &remote_command,
    ]);
    cmd.spawn()
        .map_err(|source| LaunchError::StartWaypipe { source })
}

fn uses_nested_wayland_compositor(session: &ContainerSession) -> bool {
    let profile = session
        .profile
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        profile.as_str(),
        "niri" | "hyprland" | "sway" | "wayfire" | "weston"
    ) {
        return true;
    }

    session_command_binary(session)
        .and_then(|command| {
            Path::new(&command)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_ascii_lowercase)
        })
        .is_some_and(|command| {
            matches!(
                command.as_str(),
                "niri" | "hyprland" | "sway" | "wayfire" | "weston"
            )
        })
}

fn apple_remote_gui_command(command: &str, bridge_nested_clipboard: bool) -> String {
    let launch = if bridge_nested_clipboard {
        format!(
            "if command -v wl-paste >/dev/null 2>&1 && command -v wl-copy >/dev/null 2>&1 && command -v timeout >/dev/null 2>&1; then helper=$(mktemp \"${{XDG_RUNTIME_DIR:-/tmp}}/cocoa-way-clipboard-relay.XXXXXX\") && printf '%s' {} > \"$helper\" && chmod 700 \"$helper\" && exec \"$helper\" sh -lc {}; exit 1; else printf '%s\\n' 'cocoa-way: nested clipboard needs wl-clipboard and coreutils timeout' >&2; exec {}; fi",
            shell_single_quote(include_str!("../examples/container-images/cocoa-way-clipboard-relay")),
            shell_single_quote(command),
            command,
        )
    } else {
        format!("exec {command}")
    };
    format!(
        "sleep 0.35; if [ -n \"${{COCOA_WAY_AUDIO_SOCKET:-}}\" ] && [ -e /tmp/cocoa-way-audio.ready ]; then export PULSE_SERVER=\"unix:${{XDG_RUNTIME_DIR:-/tmp/cocoa-way}}/pulse/native\" PULSE_SINK=cocoa_way; fi; export MOZ_ENABLE_WAYLAND=${{MOZ_ENABLE_WAYLAND:-1}}; {launch}"
    )
}

fn spawn_waypipe_client(
    waypipe: &Path,
    child_path: &str,
    runtime_dir: &str,
    display: &str,
    socket: &str,
    waypipe_args: &[String],
) -> Result<Child, LaunchError> {
    let mut cmd = Command::new(waypipe);
    cmd.env("PATH", child_path)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("WAYLAND_DISPLAY", display)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.args(waypipe_args);
    cmd.args(["--socket", socket, "client"])
        .spawn()
        .map_err(|source| LaunchError::StartWaypipe { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_keeps_container_names_stable() {
        assert_eq!(slugify("Niri Desktop"), "niri-desktop");
        assert_eq!(slugify("  Cocoa Way / GUI  "), "cocoa-way-gui");
        assert_eq!(slugify("!!!"), "container");
    }

    #[test]
    fn toml_escape_handles_special_characters() {
        assert_eq!(toml_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(toml_escape("a\nb\tc"), r#"a\nb\tc"#);
    }

    #[test]
    fn apple_container_status_requires_running_value() {
        assert!(is_apple_container_running_status(
            "status             running"
        ));
        assert!(!is_apple_container_running_status(
            "status             stopped"
        ));
        assert!(!is_apple_container_running_status(
            "apiserver.status   running"
        ));
    }

    #[test]
    fn apple_container_list_detects_running_session() {
        let output = br#"ID                      IMAGE                            OS     ARCH   STATE    IP                CPUS  MEMORY   STARTED
cocoa-way-niri-desktop  localhost/cocoa-way-niri:latest  linux  arm64  running  192.168.64.66/24  4     1024 MB  2026-06-25T01:33:46Z
old-session             localhost/example:latest         linux  arm64  stopped  -                 4     1024 MB  2026-06-25T01:33:46Z
"#;
        assert!(apple_container_list_has_running(
            output,
            "cocoa-way-niri-desktop"
        ));
        assert!(!apple_container_list_has_running(output, "old-session"));
        assert!(!apple_container_list_has_running(output, "missing"));
    }

    #[test]
    fn already_running_error_is_non_fatal_lifecycle_state() {
        let error = LaunchError::ContainerAlreadyRunning {
            runtime: "Apple Container".into(),
            container_name: "cocoa-way-existing".into(),
            hint: "Stop it first.".into(),
        };
        assert!(error.is_container_already_running());
        assert_eq!(
            error.to_string(),
            "Apple Container session container 'cocoa-way-existing' is already running. Stop it first."
        );
    }

    #[test]
    fn relay_parser_extracts_waypipe_ssh_reverse_socket() {
        let args = vec![
            "-R".into(),
            "/tmp/remote.sock:/tmp/local.sock".into(),
            "apple-container".into(),
            "--".into(),
            "waypipe".into(),
            "--socket".into(),
            "/tmp/remote.sock".into(),
            "server".into(),
            "foot".into(),
        ];
        let relay = parse_waypipe_ssh_relay_args(&args).unwrap();
        assert_eq!(relay.remote_socket, "/tmp/remote.sock");
        assert_eq!(relay.local_socket, "/tmp/local.sock");
        assert_eq!(relay.remote_command, args[4..]);
    }

    #[test]
    fn relay_frame_round_trip_preserves_stream_and_payload() {
        let mut encoded = Vec::new();
        write_relay_frame(&mut encoded, RELAY_FRAME_DATA, 42, b"clipboard").unwrap();

        let frame = read_relay_frame(&mut encoded.as_slice()).unwrap().unwrap();
        assert_eq!(frame.kind, RELAY_FRAME_DATA);
        assert_eq!(frame.id, 42);
        assert_eq!(frame.payload, b"clipboard");
    }

    #[test]
    fn guest_relay_accepts_more_than_one_forwarded_connection() {
        let relay = guest_relay_perl();
        assert!(relay.contains("Listen => 128"));
        assert!(relay.contains("send_frame(1, $id, '')"));
        assert!(relay.contains("my %peers"));
    }

    #[test]
    fn publish_socket_relay_separates_transport_from_process_stdio() {
        let relay = guest_publish_socket_relay_perl();
        assert!(relay.contains("my $transport_path = shift @ARGV"));
        assert!(relay.contains("Listen => 128"));
        assert!(relay.contains("Listen => 1"));
        assert!(relay.contains(RELAY_V2_READY_LINE));
        assert!(relay.contains("sysread($transport"));
        assert!(!relay.contains("binmode STDIN"));
    }

    #[test]
    fn publish_socket_relay_prepares_audio_before_transport_ready() {
        let relay = guest_publish_socket_relay_perl();
        let audio = relay
            .find("exec \"/usr/local/bin/cocoa-way-audio-relay\"")
            .unwrap();
        let ready = relay.find(RELAY_V2_READY_LINE).unwrap();
        assert!(audio < ready);
        assert!(relay.contains("audio relay did not become ready"));
    }

    #[test]
    fn transport_socket_lives_next_to_the_session_socket() {
        let socket = relay_transport_host_socket("/tmp/cocoa-way/session.sock").unwrap();
        assert!(socket.starts_with("/tmp/cocoa-way/v2-"));
        assert!(socket.ends_with(".sock"));
    }

    #[test]
    fn audio_socket_is_short_and_separate_from_transport() {
        let source = "/tmp/cocoa-way/session.sock";
        let transport = relay_transport_host_socket(source).unwrap();
        let audio = relay_audio_host_socket(source).unwrap();
        assert!(audio.starts_with("/tmp/cocoa-way/audio-"));
        assert!(audio.ends_with(".sock"));
        assert_ne!(audio, transport);
        assert!(audio.as_bytes().len() < UNIX_SOCKET_PATH_BYTES);
    }

    #[test]
    fn default_socket_leaves_room_for_waypipe_client_suffix_on_macos() {
        let runtime_dir = "/var/folders/mk/xtx0gv_n1hj8xqby99zzmfsh0000gn/T/cocoa-way";
        let socket = default_container_socket(
            runtime_dir,
            "A deliberately long Cocoa-Way session name used for regression coverage",
        );
        assert!(socket.starts_with("/tmp/cw-"));
        assert!(
            socket.as_bytes().len() + WAYPIPE_CLIENT_SUFFIX_RESERVE <= UNIX_SOCKET_PATH_BYTES,
            "{} is too long for a derived waypipe client socket",
            socket
        );
    }

    #[test]
    fn different_session_names_get_different_short_sockets() {
        let first = default_container_socket("/ignored", "Niri Desktop");
        let second = default_container_socket("/ignored", "Niri Desktop Copy");
        assert_ne!(first, second);
    }

    #[test]
    fn relay_parser_rejects_missing_reverse_socket() {
        let args = vec!["apple-container".into(), "--".into(), "true".into()];
        assert!(parse_waypipe_ssh_relay_args(&args).is_err());
    }

    #[test]
    fn apple_gui_command_uses_audio_environment_and_optional_clipboard_relay() {
        let command = apple_remote_gui_command("niri --session", true);
        assert!(command.contains("COCOA_WAY_AUDIO_SOCKET"));
        assert!(command.contains("cocoa-way-audio.ready"));
        assert!(command.contains("PULSE_SINK=cocoa_way"));
        assert!(command.contains("MOZ_ENABLE_WAYLAND"));
        assert!(command.contains("command -v wl-paste"));
        assert!(command.contains("command -v timeout"));
        assert!(command.contains("exec \"$helper\" sh -lc 'niri --session'"));
        assert!(command.contains("exec niri --session; fi"));
        assert!(command.contains("image/png"));
    }

    #[test]
    fn direct_apps_do_not_mistake_waypipe_proxy_sockets_for_nested_compositors() {
        let command = apple_remote_gui_command("firefox", false);
        assert!(command.contains("exec firefox"));
        assert!(!command.contains("cocoa-way-clipboard-relay"));
    }

    #[test]
    fn server_command_quotes_socket_and_gui_command() {
        let command = build_server_command(
            "/tmp/cocoa way/gui.sock",
            "sh -lc 'echo hi'",
            &["--compress".into(), "none".into()],
        );
        assert!(
            command
                .contains("waypipe '--compress' 'none' --socket '/tmp/cocoa way/gui.sock' server")
        );
        assert!(command.contains("XDG_RUNTIME_DIR"));
        assert!(command.contains("sleep 0.35; exec sh -lc '\\''echo hi'\\'''"));
    }

    #[test]
    fn apple_container_defaults_to_uncompressed_local_waypipe() {
        let session = ContainerSession {
            name: "Niri Desktop".into(),
            image: "localhost/cocoa-way-niri:latest".into(),
            runtime: "container".into(),
            display: Some("auto".into()),
            presentation: None,
            profile: Some("niri".into()),
            app: None,
            command: Some("niri".into()),
            socket: None,
            container_socket: None,
            waypipe_path: None,
            waypipe_compress: None,
            waypipe_threads: None,
            audio: false,
            runtime_args: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
        };

        assert_eq!(
            waypipe_tuning_args(&session, ContainerRuntime::Apple),
            vec!["--compress".to_string(), "none".to_string()]
        );
        assert!(waypipe_tuning_args(&session, ContainerRuntime::Docker).is_empty());
    }

    #[test]
    fn named_display_is_accepted_for_dedicated_worker() {
        let auto_session = ContainerSession {
            name: "Niri Desktop".into(),
            image: "localhost/cocoa-way-niri:latest".into(),
            runtime: "container".into(),
            display: Some("auto".into()),
            presentation: None,
            profile: Some("niri".into()),
            app: None,
            command: None,
            socket: None,
            container_socket: None,
            waypipe_path: None,
            waypipe_compress: None,
            waypipe_threads: None,
            audio: false,
            runtime_args: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
        };
        assert!(validate_display_slot(&auto_session).is_ok());

        let external_session = ContainerSession {
            name: "Niri Desktop".into(),
            image: "localhost/cocoa-way-niri:latest".into(),
            runtime: "container".into(),
            display: Some("external".into()),
            presentation: None,
            profile: Some("niri".into()),
            app: None,
            command: None,
            socket: None,
            container_socket: None,
            waypipe_path: None,
            waypipe_compress: None,
            waypipe_threads: None,
            audio: false,
            runtime_args: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
        };
        assert!(validate_display_slot(&external_session).is_ok());
    }

    #[test]
    fn terminal_command_targets_named_container() {
        let session = ContainerSession {
            name: "Niri Desktop".into(),
            image: "localhost/cocoa-way-niri:latest".into(),
            runtime: "container".into(),
            display: Some("default".into()),
            presentation: None,
            profile: Some("niri".into()),
            app: None,
            command: None,
            socket: None,
            container_socket: None,
            waypipe_path: None,
            waypipe_compress: None,
            waypipe_threads: None,
            audio: false,
            runtime_args: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
        };
        assert_eq!(container_name(&session), "cocoa-way-niri-desktop");
        assert!(terminal_command(&session).contains("container exec -it 'cocoa-way-niri-desktop'"));
    }

    #[test]
    fn docker_terminal_command_uses_docker_exec() {
        let session = ContainerSession {
            name: "Smoke App".into(),
            image: "localhost/cocoa-way-smoke:latest".into(),
            runtime: "docker".into(),
            display: Some("default".into()),
            presentation: None,
            profile: Some("single-app".into()),
            app: None,
            command: Some("foot".into()),
            socket: None,
            container_socket: None,
            waypipe_path: None,
            waypipe_compress: None,
            waypipe_threads: None,
            audio: false,
            runtime_args: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
        };
        assert!(terminal_command(&session).contains("docker exec -it 'cocoa-way-smoke-app'"));
    }

    #[test]
    fn missing_presentation_keeps_the_desktop_behavior() {
        let session = ContainerSession {
            name: "Legacy Desktop".into(),
            image: "example:latest".into(),
            runtime: "container".into(),
            display: Some("auto".into()),
            presentation: None,
            profile: None,
            app: None,
            command: Some("true".into()),
            socket: None,
            container_socket: None,
            waypipe_path: None,
            waypipe_compress: None,
            waypipe_threads: None,
            audio: false,
            runtime_args: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
        };

        assert_eq!(
            session.presentation_mode(),
            crate::presentation::PresentationMode::Desktop
        );
    }

    #[test]
    fn session_toml_preserves_rootless_presentation() {
        let session = ContainerSession {
            name: "Rootless App".into(),
            image: "example:latest".into(),
            runtime: "container".into(),
            display: Some("auto".into()),
            presentation: Some("rootless".into()),
            profile: Some("single-app".into()),
            app: None,
            command: Some("foot".into()),
            socket: None,
            container_socket: None,
            waypipe_path: None,
            waypipe_compress: None,
            waypipe_threads: None,
            audio: true,
            runtime_args: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
        };

        let serialized = session_to_toml(&session);
        assert!(serialized.contains("presentation = \"rootless\""));
        assert!(serialized.contains("audio = true"));
        let reparsed: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.session[0].presentation.as_deref(),
            Some("rootless")
        );
        assert!(reparsed.session[0].audio);
    }

    #[test]
    fn omitted_audio_defaults_to_enabled_and_explicit_off_is_preserved() {
        let enabled: Config =
            toml::from_str("[[session]]\nname = \"Default Audio\"\nimage = \"example:latest\"\n")
                .unwrap();
        assert!(enabled.session[0].audio);

        let disabled: Config = toml::from_str(
            "[[session]]\nname = \"Muted\"\nimage = \"example:latest\"\naudio = false\n",
        )
        .unwrap();
        assert!(!disabled.session[0].audio);
        assert!(session_to_toml(&disabled.session[0]).contains("audio = false"));
    }

    #[test]
    fn rootless_rejects_xterm_without_xwayland() {
        let session = ContainerSession {
            name: "Rootless Xterm".into(),
            image: "example:latest".into(),
            runtime: "container".into(),
            display: Some("auto".into()),
            presentation: Some("rootless".into()),
            profile: Some("single-app".into()),
            app: None,
            command: Some("/usr/bin/xterm".into()),
            socket: None,
            container_socket: None,
            waypipe_path: None,
            waypipe_compress: None,
            waypipe_threads: None,
            audio: false,
            runtime_args: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
        };

        assert!(matches!(
            validate_presentation_command(&session),
            Err(LaunchError::UnsupportedDisplay { .. })
        ));
    }

    #[test]
    fn rootless_rejects_nested_desktop_compositors() {
        let session = ContainerSession {
            name: "Rootless Niri".into(),
            image: "example:latest".into(),
            runtime: "container".into(),
            display: Some("auto".into()),
            presentation: Some("rootless".into()),
            profile: Some("niri".into()),
            app: None,
            command: Some("niri".into()),
            socket: None,
            container_socket: None,
            waypipe_path: None,
            waypipe_compress: None,
            waypipe_threads: None,
            audio: false,
            runtime_args: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
        };

        assert!(matches!(
            validate_presentation_command(&session),
            Err(LaunchError::UnsupportedDisplay { .. })
        ));
    }
}
